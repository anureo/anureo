# anureo ACP 子代理契约设计

> **状态**：Draft（2026-08-22 可执行开发版，待前后端评审）
> **范围**：把 anureo `agent` tool invocation 暴露为可观测、可加载、可取消的 ACP child session，并与 anureo Desk 建立稳定绑定
> **相关代码**：`agent/agent-core/src/tools/agent/`、`agent/tool/tool-core/src/context.rs`、`apps/acp/src/stream_bridge.rs`、`apps/acp/src/session.rs`、`apps/acp/src/session_repository.rs`、`apps/acp/src/agent.rs`
> **交叉参考**：[子代理交互差距审计](./subagent-interaction-gap.md)、[Session List 重设计](./session-list-redesign.md)、[Session List 规范](../acp-spec/extensions/37-session-list.md)、[Session 生命周期](../acp-spec/02-session-lifecycle.md)、[Session Update](../acp-spec/05-session-update.md)

---

## 1. 目标与非目标

### 1.1 目标

1. 每次 `agent` tool invocation 都有唯一、稳定、可追踪的 identity。
2. invocation 启动前创建一个 durable ACP child session，并写入 canonical SessionIndex。
3. 父 tool call 通过版本化 metadata 直接引用 child session，不依赖标题、时间窗或 output 文本猜测。
4. child session 支持标准 `session/load` 与 `session/cancel`。
5. 同步、后台、失败和取消路径使用同一生命周期，并恰好产生一个 terminal state。
6. 新旧 anureo/Desk 组合可渐进兼容，fallback 不掩盖真实协议错误。

### 1.2 非目标

- 本契约不实现 Agent Profile `primary/subagent/all` mode。
- 本契约不把 Goal runner 或 Multi-Run coordinator 改造成真实服务端编排。
- 本契约不定义运行中 extend / resume 旧 invocation。
- 本契约不改变 SessionIndex 的删除语义；默认不级联删除后代。
- 本契约不把 fork 等同于历史接管；message boundary/history copy 另行设计。

## 2. 设计决策

| 维度 | 决定 | 原因 |
| --- | --- | --- |
| anureo tool name | 保持 `agent` | 不为适配 UI 改写后端 canonical name |
| 前端识别 | `agent`、`task` 均视为 subagent tool | 兼容 anureo 与 OpenCode |
| 身份生成 | invocation 入口一次生成完整 `SubagentIdentity` | 消除 runner 二次拼 ID 和碰撞 |
| durable membership | child 写入 `acp_sessions` / SessionIndex | 首个 checkpoint 前也必须可见 |
| 父子字段 | wire/canonical storage 使用 `parent_session_id` / `parentSessionId` | 与 37 号规范一致 |
| Desk 内部字段 | adapter 映射为 `parentID` | 兼容现有 session tree 领域模型 |
| metadata 命名空间 | `_meta["anureo.dev"].subagent` | 避免占用 ACP 标准字段并支持版本演进 |
| lifecycle bridge | agent-core trait，`apps/acp` 实现 | 保持 agent-core 不依赖 ACP |
| 取消入口 | 标准 `session/cancel(child)` | 不新增重复的 UI 私有 cancel RPC |
| 状态权威源 | SessionIndex durable metadata / session load 为恢复权威源，event 用于低延迟 | global/tool update 可能丢失 |
| 删除 | 删除目标，不默认递归删除 descendants | 遵循当前 SessionIndex 契约 |

## 3. 身份契约

### 3.1 Identity graph

```text
parent_session_id
  + parent_tool_call_id
      -> invocation_id
          -> child_session_id
          -> child_thread_id
```

建议的 agent-core 类型：

```rust
pub struct SubagentIdentity {
    pub invocation_id: String,
    pub parent_session_id: String,
    pub parent_thread_id: String,
    pub parent_tool_call_id: String,
    pub child_session_id: String,
    pub child_thread_id: String,
    pub depth: u32,
}
```

### 3.2 必须满足的不变量

1. `invocation_id`、`child_session_id`、`child_thread_id` 在一次 server 生命周期内及 durable store 中均不碰撞。
2. 三个 ID 在 invocation 入口生成一次，之后只传递、不重算。
3. 同一父会话、同一 agent、同一 depth 的并发或重复调用也必须得到不同 ID。
4. `parent_tool_call_id` 必须来自当前 tool execution context，不能靠 stream 序号反查。
5. `depth = parent_depth + 1`；超过 profile/config 最大深度时，在创建 child session 前失败。
6. `parent_session_id` 使用 ACP session ID，`parent_thread_id` 仅用于 checkpoint；二者不得混用。

具体 ID 字符串格式属于实现细节；wire consumer 只能把它们当 opaque string。

## 4. Lifecycle bridge

agent-core 定义对象安全、异步的 lifecycle interface；ACP 之外的 CLI runtime 可提供 no-op 或本地实现。

```rust
#[async_trait]
pub trait SubagentLifecycleSink: Send + Sync {
    async fn created(
        &self,
        identity: &SubagentIdentity,
        spec: &SubagentSpec,
    ) -> Result<(), SubagentLifecycleError>;

    async fn running(&self, identity: &SubagentIdentity) -> Result<(), SubagentLifecycleError>;

    async fn terminal(
        &self,
        identity: &SubagentIdentity,
        outcome: &SubagentOutcome,
    ) -> Result<(), SubagentLifecycleError>;
}
```

准确签名可按现有 crate 依赖调整，但必须保持以下语义：

- `created` 在 spawn runner 前执行；失败则 invocation 不启动。
- `running` 只允许在 `created` 后发生，重复调用幂等。
- `terminal` 对 completed/failed/cancelled 恰好成功收敛一次；重复终态调用不得产生第二个 terminal event。
- durable repository commit 先于 event publish；publish 失败不回滚已提交状态。
- lifecycle sink 不接收 prompt、model output 或 secret 等不必要数据。

## 5. Child session 注册契约

### 5.1 创建顺序

```text
validate depth/profile/input
  -> allocate SubagentIdentity
  -> SessionStore create child entry
  -> SessionRepository canonical create(parent_session_id=parent)
  -> publish session.created
  -> register invocation cancel handle
  -> spawn child runner
  -> mark running
```

任何 spawn 前失败都必须返回父 tool error；如果 durable child 已创建，则写入 failed terminal state，不能留下永久 running 记录。

### 5.2 继承字段

child session 创建时继承：

- owner principal；
- cwd，或 agent worktree 决策产生的 cwd；
- 父 session 的模型/agent 配置，再应用本次 agent profile override；
- MCP servers 与必要的 session config；
- cancellation parent relationship；
- `parent_session_id = parent ACP session ID`。

不得复制父 session 的 message history 到 child checkpoint。child history 只包含为该 agent 构造的输入和其后执行记录。

### 5.3 SessionIndex mutation

所有 membership、parent、lifecycle、activity 和 delete 写入必须复用 `SessionRepository` canonical mutation，不得新增 SQL 旁路。owner/revision/indexVersion、snapshot/event、tombstone 和 ancestor tree activity 行为完全遵循 37 号规范与 `session-list-redesign.md`。

删除 child 只删除该 target；删除 parent 后仍存活的 child 可按 SessionIndex 规则成为 effective root。若未来需要级联，必须另立协议版本和 UI 确认语义。

### 5.4 Durable status projection

SessionIndex 的 protocol-owned `lifecycle` 只能是 `idle | closed`，不能写入 `running/completed/failed/cancelled`。子代理具体状态持久化在 child record 的 Desk-owned metadata：

```json
{
  "anureo": {
    "subagent": {
      "version": 1,
      "invocationId": "inv_opaque",
      "parentToolCallId": "call_parent_tool",
      "agent": "explore",
      "depth": 1,
      "status": "running",
      "stats": null
    }
  }
}
```

创建/运行期间 session lifecycle 为 `idle`；进入任何 terminal outcome 后，先在同一 canonical mutation 语义中保存具体 subagent status/stats，再将 session lifecycle 收敛为 `closed`。如果当前 repository API 不能原子更新两者，应新增统一 mutation，而不是从 handler 连续调用两个相互独立的写入入口。

## 6. Tool call metadata 契约

### 6.1 Envelope

初始 tool call 与后续相关 tool call update 均使用同一 envelope：

```json
{
  "toolName": "agent",
  "anureo.dev": {
    "subagent": {
      "version": 1,
      "invocationId": "inv_opaque",
      "sessionId": "sess_child",
      "parentSessionId": "sess_parent",
      "parentToolCallId": "call_parent_tool",
      "agent": "explore",
      "model": "provider/model",
      "depth": 1,
      "status": "running",
      "stats": null
    }
  }
}
```

terminal update 示例：

```json
{
  "anureo.dev": {
    "subagent": {
      "version": 1,
      "invocationId": "inv_opaque",
      "sessionId": "sess_child",
      "parentSessionId": "sess_parent",
      "parentToolCallId": "call_parent_tool",
      "agent": "explore",
      "model": "provider/model",
      "depth": 1,
      "status": "completed",
      "stats": {
        "turnCount": 4,
        "totalTokens": 8120,
        "toolCallsCount": 7
      }
    }
  }
}
```

### 6.2 字段规则

| 字段 | 必填 | 规则 |
| --- | --- | --- |
| `version` | 是 | 当前固定为 `1`；未知 major/version 时保留 raw metadata、降级普通 tool UI |
| `invocationId` | 是 | opaque、单次 invocation 唯一 |
| `sessionId` | 是 | 可传给 `session/load` / `session/cancel` 的 ACP child session ID |
| `parentSessionId` | 是 | 必须等于承载 tool call 的 session |
| `parentToolCallId` | 是 | 必须等于当前 tool call ID |
| `agent` | 是 | profile/agent 名称，用于 label；不得作为身份键 |
| `model` | 否 | 可展示；未知或继承未解析时为 null/省略 |
| `depth` | 是 | 非负安全整数，child 通常从 1 开始 |
| `status` | 是 | `created/running/completed/failed/cancelled` |
| `stats` | terminal 时 | completed 必须提供；failed/cancelled 可提供已消耗统计 |

Desk 必须校验 `sessionId`、`parentSessionId` 和 `parentToolCallId` 的关联；关联不一致时不得打开目标 session，并记录不含敏感内容的结构化诊断。

### 6.3 Output fallback

新 anureo 不以 `<task id=...>` 文本作为事实源。Desk 可在兼容旧 OpenCode/anureo 时继续解析 output，但优先级固定为：

1. 版本化 `_meta`；
2. legacy part metadata；
3. legacy `<task>` output；
4. 时间窗 fallback。

只要存在有效版本化 `_meta`，后三级不得覆盖它。metadata 存在但校验失败属于协议错误，不应静默退回时间窗猜测。

## 7. 生命周期与取消

### 7.1 状态机

```text
created -> running -> completed
                   -> failed
                   -> cancelled
created ---------> failed
created ---------> cancelled
```

terminal state 不可逆。父 tool call、registry entry、child SessionIndex durable metadata 三处允许短暂传播延迟，但最终必须收敛到同一 subagent status；SessionIndex protocol lifecycle 与该状态正交，只表达 `idle | closed`。

### 7.2 标准取消路径

Desk 对运行中的 child 调用标准 `session/cancel(child_session_id)`：

1. ACP SessionStore 定位 child entry；
2. entry 取得 invocation cancel handle 或共享 registry key；
3. 触发 child cancellation token；
4. runner 退出后 lifecycle sink 在 child durable metadata 写入 `cancelled`，并把 session lifecycle 置为 `closed`；
5. 父 tool call 收到 terminal update。

重复 cancel 为幂等。对 completed/failed/cancelled child 的 cancel 返回成功或明确 no-op，不得改变终态。

父 session cancel 默认级联取消其仍运行的直接/间接 descendants；实现必须使用 visited set，且每个 child 仍走正常 terminal closure。若产品决定不级联，需要在实现前修改本节并补 UI 提示。

### 7.3 后台完成

后台 invocation 可以在父 prompt 已结束后完成。ACP 必须保留 `parent_session_id + parent_tool_call_id` 关联，并向仍连接的客户端发布对应 tool call terminal update。若客户端离线，重连后以 child SessionIndex durable metadata/session load 为权威恢复；不能依赖内存 notification 作为唯一事实源。

## 8. Desk 适配契约

Desk 需要在 ACP adapter 层完成以下一次性转换，组件层不再自行猜字段：

1. `AcpToolCallRecord` 保存完整 `_meta`。
2. native `tool_call` / `tool_call_update` 与 legacy adapter 都把 metadata 投影到 `ToolPartInput.metadata`。
3. `isSubagentTool` 同时识别 `agent` 和 `task`。
4. anureo tool input 的 `agent` 映射为 UI 的 agent/subagent type label。
5. SessionIndex `parentSessionId` 映射为内部 session `parentID`。
6. 有效 versioned metadata 直接生成 Open/Cancel action；没有时才启用 legacy fallback。
7. 未知 metadata version 降级为普通 tool card，同时保留 raw data 供后续版本 adapter 使用。

只读策略保持产品设置控制：默认 child session 只读；允许 prompting 时也不得复用已经 terminal 的 invocation，发送新 prompt 的语义应是普通 child session 对话或 fork，而不是隐式恢复 agent tool。

## 9. 实现落点

| 仓库/文件 | 改动 |
| --- | --- |
| `agent/tool/tool-core/src/context.rs` | 增加 `tool_call_id`、lifecycle/control context，保证 depth/ACP session 传播 |
| `agent/agent-core/src/tools/agent/mod.rs` | 生成 `SubagentIdentity`，统一生命周期入口 |
| `agent/agent-core/src/tools/agent/runner.rs` | 消费 identity、继承 cancellation、移除 ID 拼接与 `eprintln!` |
| `agent/agent-core/src/tools/agent/registry.rs` | 支持共享注入及 session/invocation 索引，保留 terminal stats |
| `agent/agent-core/src/agent/react/build/checkpointer.rs` | 不再把 depth/ACP session 固定为 `None` |
| `apps/acp/src/agent.rs` / `session.rs` | 实现 lifecycle sink、child entry 与 cancel handle |
| `apps/acp/src/session_repository.rs` | 复用 canonical create/lifecycle mutation，写 parent relation |
| `apps/acp/src/stream_bridge.rs` | 初始/update frame 投影 versioned metadata |
| Desk `acp-session-store.ts` | 保存并合并完整 metadata |
| Desk `acp-legacy-adapter.ts` / `type-mapping.ts` | metadata 与 `parentSessionId -> parentID` 统一适配 |
| Desk `ToolPart.tsx` | 识别 `agent`，使用显式 session ID，展示 cancel/stats |

文件名会随重构调整；上述职责边界与数据流是冻结契约。

## 10. 测试计划

### 10.1 anureo 单元与集成测试

| 测试 | 验证点 |
| --- | --- |
| identity uniqueness | 并发同名/depth invocation 的全部 ID 唯一 |
| context propagation | depth、ACP session、tool call、cancellation 到达 child runner |
| lifecycle ordering | create 在 spawn 前；每条路径恰好一个 terminal |
| repository projection | child 带正确 owner/parent/revision/indexVersion；subagent status 位于 metadata，lifecycle 仅为 idle/closed |
| stream metadata | start/running/terminal frame 字段完整且 identity 一致 |
| cancel | child cancel 精确命中 invocation，重复 cancel 幂等 |
| background | 父 prompt 结束后仍发布 terminal update |
| delete | 非级联删除与 ancestor tree activity 遵循 SessionIndex |

### 10.2 Desk 单元与 E2E

| 测试 | 验证点 |
| --- | --- |
| anureo `agent` recognition | 进入 subagent card，而非 generic tool card |
| metadata preservation | native/legacy 两条 adapter 不丢 `_meta` |
| parent mapping | `parentSessionId` 进入内部 `parentID`，tree/fallback/read-only 一致 |
| explicit binding | 有 metadata 时不调用时间窗 fallback |
| invalid binding | parent/tool mismatch 不打开错误 session，不静默 fallback |
| cancel/stats | running 显示 cancel，terminal 显示统计且不可再取消 |
| real ACP BDD | delegate explore → metadata frame → child list/load → cancel/reload |

建议门禁：

```powershell
cargo nextest run -p agent
cargo nextest run -p anureo-acp
cargo clippy --workspace --all-targets -- -D warnings
bun --cwd C:\Users\heycj\dev\anureo-feat-dev\packages\ui test
bun --cwd C:\Users\heycj\dev\anureo-feat-dev\packages\ui run type-check
npm --prefix e2e run test:bdd:dev
```

## 11. 兼容与发布

- 新 Desk 同时支持 `agent` 和 `task`，但不改写 wire tool name。
- 新 Desk 只在方法/metadata 确实缺失时使用 legacy fallback；解析错误、身份不一致和权限错误不得触发 fallback。
- 新 anureo 可以保留 `_meta.toolName`，新增 namespace 不破坏不了解该字段的 ACP client。
- metadata `version` 变更时先扩展 reader，再升级 writer；删除 v1 reader 需要最低 Desk 版本与实际调用量证据。
- 发布签收必须留存真实 anureo + Desk WebSocket frame、SessionIndex child record、load/cancel 结果和重连恢复证据，不能只依赖同层单元测试。

## 12. 完成定义

只有以下条件全部满足，才能把本文件状态改为“已实现”：

1. `agent` invocation 与 child session/thread 一一对应，压力测试无碰撞。
2. child 在 SessionIndex 中 durable 可见，parent relation 正确。
3. 父 tool metadata 能直接、唯一定位 child，Desk 不走时间窗 fallback。
4. child history 可 load，运行中 child 可用标准 session cancel。
5. completed/failed/cancelled/background 均恰好一个终态，统计可见。
6. ACP、agent-core、Desk unit/type-check 和真实 ACP BDD 全部通过。
7. 文档记录实际提交、兼容矩阵、未完成项与发布证据。

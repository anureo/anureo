# anureo Desk 子代理交互 ↔ anureo 后端差距审计

> **状态**：Audit（2026-08-22 源码复核；可据此排期）
> **范围**：anureo `agent-core` / `apps/acp` 与 anureo 前端 `anureo-feat-dev/packages/ui` 的子代理交互链路
> **相关代码**：`agent/agent-core/src/tools/agent/`、`agent/tool/tool-core/src/context.rs`、`apps/acp/src/stream_bridge.rs`、`apps/acp/src/session_repository.rs`、`apps/acp/src/agent.rs`；前端 `packages/ui/src/components/chat/message/parts/ToolPart.tsx`、`packages/ui/src/lib/acp/acp-session-store.ts`、`packages/ui/src/lib/acp/type-mapping.ts`
> **交叉参考**：[ACP 子代理契约](./acp-subagent-contract.md)、[Session List 重设计](./session-list-redesign.md)、[Session List 规范](../acp-spec/extensions/37-session-list.md)

---

## 1. 审计结论

anureo 已有可运行的 in-process 子代理引擎，anureo Desk 也已有 OpenCode `task` 工具的子任务卡片与只读子会话 UI，但两端之间缺少一等 ACP 契约。当前链路不能稳定回答以下问题：

1. 父会话中的哪个 tool call 创建了哪个子代理 invocation；
2. invocation 对应哪个可加载、可取消的 ACP child session；
3. 子代理当前状态、层级和完成统计如何通过标准事件持续更新；
4. Desk 应如何在不依赖时间窗猜测的情况下打开正确子会话。

因此，第一阶段目标不是继续完善卡片样式，而是建立“父 tool call ↔ invocation ↔ child session”的稳定身份和生命周期。Goal、Multi-Run、Agent Profile mode 与 fork 接管属于独立能力，不应作为子代理 MVP 的前置条件。

## 2. 当前能力矩阵

| 能力 | anureo 当前实现 | Desk 当前实现 | 结论 |
| --- | --- | --- | --- |
| 子代理执行 | `agent` tool 可同步/后台运行，存在 registry、深度限制与统计结构 | 可渲染普通 tool call | 引擎存在，但未形成 ACP child session |
| 子代理工具识别 | wire tool name 为 `agent` | 专用 UI 只识别规范化后严格等于 `task` | **名称不兼容** |
| tool call metadata | 初始 tool call 仅含 `_meta.toolName`；update 未携带子代理 metadata | store/legacy adapter 会丢弃大部分 `_meta` | **两端都需修改** |
| child session | agent-core 创建独立 runner/checkpoint thread | UI 可打开有 `parentID` 的子会话 | **ACP SessionStore/SessionIndex 未注册子会话** |
| 父子关系 | SessionIndex 已支持 `parent_session_id` | 新 ACP projection 使用 `parentSessionId`，旧 UI 多处使用 `parentID` | **边界适配未统一** |
| 唯一身份 | invocation ID 含 sequence | checkpoint thread ID 不含 sequence | **重复/并发调用可能碰撞** |
| depth / ACP context | `ToolCallContext` 有字段 | 不适用 | build 路径将 `depth`、`acp_session_id` 丢为 `None` |
| 取消与状态 | `AsyncAgentRegistry` 有 cancel/get/stats | 子任务卡片无稳定 child session 控制面 | registry 不是 ACP server 共享事实源 |
| Agent Profile mode | profile schema 无 `mode/subagent` | adapter 检查 `subagent` 字段 | **旧文档“已对齐”结论不成立** |
| Goal | CRUD/status 外壳存在；runner 返回 `Unavailable` | 有 API 封装，缺完整消费 | 不能视为可运行能力 |
| Multi-Run | production 使用 `LocalCoordinator + NoopPublisher`，未创建真实 session | 前端自行创建 session/worktree 并编排 | 是单独迁移项目 |
| fork 接管 | fork 新建 session 并复制配置/MCP | action 忽略 message boundary | 未复制指定边界历史，不能称为“接管” |

## 3. 关键差距与源码证据

### 3.1 GAP-01（P0）：`agent` 与 `task` 工具名不匹配

- anureo 的 canonical tool name 定义为 `agent`（`agent/tool/tool-core/src/tool_name.rs`）。
- Desk 的 `TaskToolSummary` 仅在 `normalizedTool === "task"` 时启用（`ToolPart.tsx`）。
- `normalizeToolName` 只做大小写和命名空间归一化，不会把 `agent` 映射为 `task`。

**影响**：即使 anureo 后端补齐 session metadata，当前 `agent` 调用仍不会进入专用子代理 UI。

**决定**：Desk 增加统一的 `isSubagentTool` 判定，迁移期同时接受 `agent` 与 `task`；wire 不把 anureo tool 重命名为 `task`。

### 3.2 GAP-02（P0）：缺少稳定的 invocation / session / thread 身份

`agent` invocation ID 使用 `sub-{parent_thread}-{agent}-{depth}-{seq}`，而 runner 的 checkpoint thread 使用 `sub-{parent_thread}-{agent}-{depth}`。sequence 只存在于前者，重复或并发调用同一 agent/depth 时可能复用 checkpoint thread。

**影响**：历史、取消、统计和 UI 卡片无法可靠一一对应。

**决定**：一次调用只生成一个 `SubagentIdentity`，其中 `invocation_id`、`child_session_id`、`child_thread_id` 均唯一并贯穿全部层；不得在 runner 内重新拼接 ID。

### 3.3 GAP-03（P0）：agent-core 未向 ACP 注册 child session

agent-core 只创建子 runner/checkpoint thread，没有调用 `apps/acp` 的 `SessionStore` 或 `SessionRepository`。虽然 `SessionStore::create_with_owner` 和 canonical SessionIndex mutation 已具备显式 ID/thread 与 `parent_session_id` 能力，但当前没有生命周期桥接点。

**影响**：`session/list` 看不到子会话，`session/load`/`session/cancel` 没有可寻址 child session，Desk 只能使用时间窗 fallback 猜测。

**决定**：在 agent-core 定义不依赖 ACP 的 `SubagentLifecycleSink`；由 `apps/acp` 实现并注入。创建子 runner 前先注册 child session，失败则不启动子代理。

### 3.4 GAP-04（P0）：metadata 未端到端保留

后端 `StreamUpdate::ToolCallUpdated` 仅携带 status/output/raw_output，`stream_bridge.rs` 未在 update 中投影子代理 metadata。前端 `AcpToolCallRecord`、legacy adapter 与 `ToolPartInput` 又没有完整 metadata 通道。

**影响**：Desk 的 `taskSessionId` 一级解析为空，只能退化到 output 文本或时间窗猜测。

**决定**：通过 ACP `_meta["anureo.dev"].subagent` 发送版本化 metadata；前端 store 和两条 adapter 路径必须原样保留，再投影为 UI metadata。

### 3.5 GAP-05（P0）：`parentSessionId` 与 `parentID` 的领域模型漂移

SessionIndex wire 使用 `parentSessionId`，但 Desk 现有 session tree、fallback 和只读判断大量读取 `parentID`。新测试只覆盖新字段并不能证明旧 UI 路径已正确消费。

**决定**：协议边界继续使用 `parentSessionId`；ACP adapter 在唯一入口映射为 Desk 内部 canonical `parentID`。不得让组件自行兼容两个字段。

### 3.6 GAP-06（P1）：depth、ACP session 与取消上下文传播中断

- `build_runnable_config` 将 `depth` 和 `acp_session_id` 固定为 `None`。
- `RunOptions.acp_session_id` 没有完整进入 `ReactBuildConfig`。
- 子代理 build 未继承 cancellation。
- registry 在 build tool source 时按 runner 新建，ACP server 无法按 child session 全局寻址。

**决定**：修复 context 传播，并将共享 registry 或 lifecycle control handle 注入 ACP `SessionEntry`。用户取消 child session 时应取消对应 invocation；父会话取消是否级联由契约显式规定。

### 3.7 GAP-07（P1）：完成统计与后台终态没有稳定推送

`AgentCompletionStats` 已有 `turn_count`、`total_tokens`、`tool_calls_count`，但没有稳定进入 ACP tool update。后台任务完成后，父 tool call 也缺少可重连恢复的终态更新契约。

**决定**：同步和后台路径共用同一 terminal metadata；父 session 的对应 tool call 接收 terminal `tool_call_update`，child SessionIndex 的 durable metadata 同步记录具体终态，session `lifecycle` 只按 37 号规范从 `idle` 收敛为 `closed`。事件丢失后以 child session/load 和 SessionIndex 为权威恢复源。

## 4. 过时结论修正

以下内容不得继续作为开发前提：

- Agent Profile CRUD 不等于已有 `subagent` mode；当前前后端 schema 未对齐。
- Goal handler 存在不等于 Goal 可执行；当前 runner 明确返回 `Unavailable`。
- Multi-Run RPC 存在不等于服务端编排已落地；production coordinator 没有创建真实 ACP sessions。
- extension response 中出现 `notification` 字段不等于 server 已发布 push notification。
- fork 创建新 session 不等于按指定 message boundary 复制历史并“接管”。
- SessionIndex 当前删除语义是删除目标、重算祖先并允许后代提升为 effective root；不得恢复旧草案的默认级联删除。
- “anureo 只改后端、Desk 无需修改”不成立；工具识别、metadata 保留和 parent 字段适配都需要 Desk 改动。

### 4.1 审计验证记录

2026-08-22 在当前两仓工作树上执行了以下针对性回归：

```powershell
# anureo-feat-dev/packages/ui
bun test --isolate src/components/chat/message/parts/__tests__/resolveFallbackTaskSessionId.test.js src/components/chat/message/parts/ToolPart.test.ts src/components/session/sidebar/sessionTree.test.ts src/lib/acp/acp-session-actions.create.test.ts src/lib/acp/acp-event-source.test.ts

# anureo
cargo nextest run -p anureo-acp stream_bridge
```

Desk 结果为 31 passed / 0 failed；anureo stream bridge 结果为 5 passed。它们证明现有 OpenCode-style `task`/`parentID` fallback 和 token usage metadata 回归没有破坏，但**没有**覆盖 anureo `agent` tool、versioned subagent metadata、ACP child session 注册或 `parentSessionId -> parentID` 端到端适配。因此这些通过结果不能作为子代理契约已落地的证据。

## 5. 可执行开发方案

### Phase 0：冻结契约并先写失败测试

目标：让两端围绕同一 identity graph 开发。

```text
parent ACP session
  -> parent toolCallId
  -> subagent invocationId
  -> child ACP sessionId
  -> child checkpoint threadId
```

交付：

- 以 [ACP 子代理契约](./acp-subagent-contract.md) §3～§7 为 wire source of truth。
- anureo 单元测试覆盖唯一 ID、depth/ACP context 传播、lifecycle 调用顺序。
- Desk 单元测试使用真实 anureo tool name `agent`、`parentSessionId` 和 `_meta` envelope。
- ACP wire 测试先断言当前实现缺失 child session / metadata，再进入实现阶段。

### Phase 1：修复 agent-core 身份与上下文

改动：

- 新增 `SubagentIdentity`，在一次 invocation 的入口生成全部 ID。
- runner 直接消费 identity，不再拼 thread ID。
- 将 `tool_call_id`、`depth`、`acp_session_id`、cancellation 与 lifecycle sink 贯穿 `ToolCallContext` / `ReactBuildConfig` / runner。
- 将 registry 改为可注入共享实例，补 child session / invocation / owner / depth 索引字段。
- 将 `eprintln!` 诊断替换为 `tracing`。

完成条件：相同父会话并发启动同名 agent 时，session/thread/registry 均不碰撞；嵌套 depth 正确递增。

### Phase 2：在 ACP 注册一等 child session

改动：

- agent-core 定义 `SubagentLifecycleSink`，`apps/acp` 提供实现。
- spawn 前使用 `SessionStore::create_with_owner` 创建 child entry。
- 使用 `SessionRepository` canonical mutation 写入 `parent_session_id`，复制父 session 的 cwd、配置、模型与 MCP 设置。
- commit 后发布 `session.created`；启动失败则写入明确终态，不留下“running”幽灵会话。
- 建立 child session ↔ invocation cancel handle 映射。

完成条件：Desk 仅通过 SessionIndex 即可看到 child；`session/load(child)` 可读取其历史；`session/cancel(child)` 可停止对应 invocation。

### Phase 3：metadata 与 Desk adapter/UI 对齐

改动：

- anureo 在初始和后续 tool call frame 中发送版本化 subagent metadata。
- Desk `AcpToolCallRecord`、native adapter、legacy adapter 和 `ToolPartInput` 保留 metadata。
- 增加 `isSubagentTool("agent" | "task")`；把 anureo input `agent` 映射为 UI agent label/type。
- ACP session adapter 将 `parentSessionId` 映射为内部 `parentID`。
- 有显式 metadata 时禁用时间窗猜测；fallback 仅服务旧 server。

完成条件：父 tool card 能稳定打开唯一 child session，不读取 output 文本、不依赖 3s/8s 时间窗。

### Phase 4：取消、统计与后台完成

改动：

- 用标准 child `session/cancel` 作为 UI 控制入口，ACP bridge 转发至共享 registry/cancel handle。
- 将完成统计写入 terminal metadata，并在卡片展示 turns/tokens/tool calls。
- 后台完成、失败或取消后向父 tool call 推送 terminal update；SessionIndex metadata 写入具体 outcome，session lifecycle 收敛为 `closed`。

完成条件：同步/后台/失败/取消四条路径均恰好产生一个 terminal state，重连后能从持久化状态恢复。

### Phase 5：嵌套 UX 与 fork 接管

前四阶段稳定后再实施：

- 使用 SessionIndex `parent_session_id` 构造任意深度树并显示 depth。
- 定义 fork 的 message boundary 和 history copy 语义，再增加“Fork 继续对话”。
- Profile mode、Goal、Multi-Run 分别立项，不与本契约捆绑发布。

## 6. 建议提交顺序

1. `fix(agent-core): unify subagent identity and context propagation`
2. `feat(acp): register subagents as child sessions`
3. `feat(acp): publish versioned subagent tool metadata`
4. `feat(desk): adapt anureo agent tools and child session metadata`
5. `feat(subagent): add cancellation stats and background completion`
6. `feat(desk): render nested subagents and repair fork takeover`
7. `docs(subagent): record implementation status and release evidence`

每个提交必须可独立测试；前后端跨仓提交在描述中互相记录依赖 commit。

## 7. 验收矩阵

| 场景 | 必须验证的信号 |
| --- | --- |
| 同步 explore | 父 `agent` tool metadata、SessionIndex child、可 load history、terminal stats |
| 两个并发同名 agent | invocation/session/thread ID 全部唯一，卡片不串线 |
| 嵌套子代理 | depth 递增，parent chain 正确，无 checkpoint 复用 |
| 子会话取消 | `session/cancel` 停止正确 invocation；父 tool 和 child durable metadata 为 cancelled，session lifecycle 为 closed |
| 后台完成 | prompt 返回后仍能收到父 tool terminal update，重连后状态可恢复 |
| 旧 anureo 兼容 | 无 metadata 时才启用 fallback；新 anureo 不走时间窗猜测 |
| 删除父/子 | 遵循 SessionIndex 非级联语义，祖先 tree activity 正确重算 |
| fork | 未实现 message boundary 前不显示“接管成功”语义 |

建议验证命令：

```powershell
cargo nextest run -p agent
cargo nextest run -p anureo-acp
cargo clippy --workspace --all-targets -- -D warnings
bun --cwd C:\Users\heycj\dev\anureo-feat-dev\packages\ui test
bun --cwd C:\Users\heycj\dev\anureo-feat-dev\packages\ui run type-check
npm --prefix e2e run test:bdd:dev
```

## 8. 发布边界

MVP 发布必须同时具备：唯一身份、ACP child session、metadata、Desk adapter、load/cancel 和同步终态。缺少任一项时仍属于实验性 in-process 子代理，不应对外宣称“anureo Desk 已支持一等子代理会话”。Goal、Multi-Run、Profile mode、嵌套树与 fork 接管不阻塞 MVP，但必须在 UI 中避免呈现未实现能力。

# 子代理契约实现详细方案（2026-09-18）

> **状态**: Plan（详细实现版）。对[ACP 子代理契约](./acp-subagent-contract.md)做全量（Phase 1→4）实现细化；**契约语义零修改**，本文只回答"怎么落"。全部落点经当日源码复核，行号为 2026-09-18 快照。
> **文档关系**: 契约（wire 冻结标准）→ [差距审计](./subagent-interaction-gap.md)（Phase 划分/验收）→ [Phase 1 当晚清单](./subagent-phase1-code-plan.md)（今晚执行的提交切分，其内容已被本文 §2 吸收扩展）→ [codex 子代理架构详析](../analysis/codex-subagent-architecture-20260918.md)（设计借鉴来源，文中以「codex 借鉴」标注）。MVP 之上的 codex 吸收分档见[演进方案](./subagent-evolution-plan.md)。
> **跨仓边界**: Desk 侧改动（`anureo-feat-dev/packages/ui`）在本文只列交付清单与验收，不改其内部设计（以契约 §8 为准）。

---

## 0. 当前状态基线（2026-09-18 复核）

| 契约依赖点 | 现状 | 证据 |
| --- | --- | --- |
| `ToolCallContext` | 有 `depth`/`acp_session_id`/`thread_id`/`run_cancellation`，**无 `tool_call_id`** | `agent/tool/tool-core/src/context.rs:7-15` |
| depth 传播 | `ReactBuildConfig` 无 `depth` 字段；`build_runnable_config` 硬编码 `depth: None, acp_session_id: None` → max-depth 守卫（`tools/agent/mod.rs:237`）是死代码 | `react/build/checkpointer.rs:56-57` |
| invocation ID | `prepare_agent_id` 拼 `sub-{thread}-{agent}-{depth}-{seq}`；runner 另拼无 seq 的 thread ID（碰撞风险） | `tools/agent/mod.rs:70-74`、`runner.rs:119-121` |
| registry | `AsyncAgentRegistry` 在 build 路径内新建（每 runner 一个），ACP 无法全局寻址 | `react/build/tool_source.rs:354` |
| lifecycle 桥 | 全仓无 `SubagentLifecycleSink`/`SubagentIdentity` 符号 | grep 零命中 |
| SessionStore | `create_with_owner` 支持显式 session_id + thread_id + owner（幂等：同 id 返回已有 entry） | `apps/acp/src/session.rs:394` |
| SessionRepository | `insert_index_record`（:514）已支持 `parent_session_id`、owner 版本、ancestor/descendant activity；`set_metadata_json`（:1057）与 `set_lifecycle`（:1359）是**独立入口，非原子** | `apps/acp/src/session_repository.rs` |
| stream 桥 | `StreamUpdate::ToolCallStarted/Updated` 不携带子代理 metadata；`create_tool_call` 已构建 `_meta.toolName`（Meta 通道现成） | `apps/acp/src/stream_bridge.rs:144-165,1300-1332` |
| ACP 取消 | `session/cancel` → `SessionStore::set_cancelled` → `cancel_current_generation`（turn 级），无 invocation 级路由 | `apps/acp/src/session.rs:435`、`lib.rs:147` |

## 1. 目标态架构

```
模型发起 agent tool call（父会话 turn 内）
  │ act_executor 发 StreamEvent::ToolCall{call_id, "agent"}          ← 父 tool call 帧
  ▼
AgentTool::call（agent-core）
  ├─ 生成 SubagentIdentity（一次，含 7 字段）                          ← Phase 1
  ├─ sink.created(identity, spec) ──► AcpSubagentRuntime（apps/acp）  ← Phase 2
  │     ├─ SessionStore::create_with_owner(child_session_id, 继承 cwd/owner, child_thread_id)
  │     ├─ SessionRepository::insert_index_record(parent_session_id=父)
  │     ├─ registry 建索引 child_session_id ↔ invocation_id（cancel 寻址）
  │     └─ 失败 ⇒ terminal(failed)，不留 running 幽灵
  ├─ emit StreamEvent::SubagentStarted{call_id, identity}             ← Phase 3 首帧绑定
  ├─ spawn 子 runner（child_thread_id / depth+1 / 继承 cancellation）
  │     └─ 子会话事件走子 checkpoint thread，不进父流
  ├─ registry.complete/fail/cancel（终态守卫：恰一次）
  │     └─ sink.terminal(outcome) ──► 统一 mutation：metadata(status/stats) + lifecycle=closed
  └─ 同步路径 ToolEnd 返回父流；后台路径由 Runtime 经会话级 notifier 推 terminal update
                                                        ← Phase 4
stream_bridge（apps/acp）
  ├─ SubagentStarted ⇒ ToolCallUpdated{meta: _meta["anureo.dev"].subagent}
  ├─ 后续 update 从共享 registry 查状态/stats 合并进 meta
  └─ Desk（跨仓）：isSubagentTool("agent"|"task")、显式 sessionId 打开/取消、禁时间窗猜测
```

三条关键数据流（验收场景对应契约 §10）：

1. **同步**：spawn → 子跑完 → ToolEnd（父流）+ terminal update（meta 带 stats）。
2. **后台**：父 turn 结束（ToolEnd 已返回"后台已启动"）→ 子完成后 Runtime 用**会话级**（非 turn 级）notifier 推 terminal update；客户端离线则靠 SessionIndex durable metadata 恢复（权威源）。
3. **取消**：Desk `session/cancel(child)` → SessionStore 标记 + registry.cancel(invocation) → 子 runner StopReason::Cancelled → sink.terminal(cancelled) → durable + 父 tool call 终态。

### 1.1 架构分层（控制面视角）

现状定性：**工具即控制面、任务即生命周期、内存表即全部状态**——`AgentTool` 同时扮演身份生成器、生命周期管理器、配置构建器、执行器与注册表，外部系统（ACP/Desk/CLI）无法够着子代理。目标态四层：

```
工具层   AgentTool：参数解析 + 事件发射（薄壳）
控制面   SubagentRuntime：identity 类型、共享 registry、lifecycle sink、execution limiter
执行面   tokio task + ReAct loop（不变）
持久面   ACP SessionStore/SessionIndex + checkpoint（复用已有基建，不另造）
```

### 1.2 六条架构原则

| # | 原则 | 现状问题 | 目标 | 落点 |
| --- | --- | --- | --- | --- |
| 1 | 控制面抽出 | AgentTool 自包含五职责，外部够不着 | 工具薄壳化；控制面可注入（sink + 共享 registry 是雏形） | §2.5/2.6、§3.3 |
| 2 | 运行时态与权威态分离 | registry 是唯一状态源，进程死全蒸发 | 双账本：registry 管活任务/取消句柄；SessionIndex 管权威（parent 链、终态）；**事件可丢、状态不能丢** | §3.1-3.2 |
| 3 | 事件归属随身份走 | 子事件全量灌父流（runner.rs:152-160） | 子事件写 child checkpoint thread；父流只见 `_meta` 聚合帧 | §3.4、§4.2 |
| 4 | 通信显式三原语 | 唯一信道=工具返回值（函数调用模型） | 结果注入（E1-1）/ 继续对话（E1-2）/ 运行中 steer（E2-2），前两者零新协议 | [演进方案](./subagent-evolution-plan.md) E1/E2 |
| 5 | 配置继承三类字段 | clone+overlay 隐式语义，冻结快照可携带过期运行时态 | 显式契约：继承 / 覆盖 / 必须从活运行时取（§3.6） | §3.6、演进 E2-4 |
| 6 | 治理三层 + 取消三作用域 | 仅终态表容量一层；取消只有 abort | 总量/并发/驻留三层（§2 守卫 + A2 + E2-1）；invocation/turn 级联/session 三作用域（§5），abort→优雅 Cancelled | §5、演进 A2/E2-1 |

六条共同点：不是加功能，而是**把挤在一处的职责拆到已有基建上**——codex 的结构（ThreadManager/graph-store/信箱）是它自己基建下的解，anureo 的正确解是同一问题域映射到 SessionIndex/checkpoint/stream 上（即演进方案「吸收思想、不搬实现」原则的由来）。

## 2. Phase 1 — agent-core 身份与上下文（细化 7 项 + 工具层修缮）

### 2.1 `SubagentIdentity`（新文件 `agent/agent-core/src/tools/agent/identity.rs`）

```rust
/// 契约 §3.1 的完整身份图。invocation 入口生成一次，之后只传递、不重算。
#[derive(Debug, Clone)]
pub(crate) struct SubagentIdentity {
    pub invocation_id: String,       // 工具面 ID；沿用现有 seq 格式（wire 兼容）
    pub parent_session_id: String,   // ACP session（= ctx.acp_session_id）
    pub parent_thread_id: String,    // 父 checkpoint thread（= ctx.thread_id）
    pub parent_tool_call_id: String, // 本次 tool call 的 call_id（新增传播，见 2.2）
    pub child_session_id: String,    // UUID v4（与 ACP SessionId 格式一致，session.rs:371）
    pub child_thread_id: String,     // 现格式 + seq（消除并发碰撞）
    pub depth: u32,                  // = ctx.depth + 1
}

impl SubagentIdentity {
    pub(crate) fn generate(ctx: &ToolCallContext, agent_name: &str) -> Self;
}
```

不变量对齐契约 §3.2：三条 ID 并发/重复唯一（seq + UUID）；`parent_tool_call_id` 来自执行上下文而非流序号；超限在 identity 生成**之前**失败（现有 `mod.rs:237` 守卫修复后前置）。

### 2.2 `tool_call_id` 传播（契约 §9 第一行）

- `ToolCallContext` 增加 `pub tool_call_id: Option<String>`（context.rs）。
- `act_executor.rs`（构造 ctx 处，~:247）：从当前 `ToolCall` 执行槽填入 `call_id`（此处天然非空——比 `stream_bridge.rs:412` 的 ToolOutput 空回退更上游、更可靠）。
- CLI/其他 runtime 不设则 None：identity 生成退化为 `format!("call-{}", uuid)` 并 tracing warn 一次（不阻塞）。

### 2.3 depth 传播链修复

1. `ReactBuildConfig` + `pub depth: Option<u32>`（Default None，clone 继承语义同 `acp_session_id`）。
2. `checkpointer.rs:56` → `depth: config.depth, acp_session_id: config.acp_session_id.clone()`。
3. `runner.rs`：`sub_config.depth = Some(child_depth)`（child_depth 现仅用于显示，`runner.rs:151`）。
4. 效果：嵌套 agent 的 `ctx.depth` 递增，`mod.rs` max-depth 守卫复活（**这是行为修复**，嵌套层数将从"无限"变为受 `max_sub_agent_depth` 约束——验收需覆盖新拒绝路径的错误文案）。

### 2.4 cancellation 继承（审计 GAP-06 遗留项）

> **状态（2026-09-18 晚更新）**：✅ **已实现**。`build_react_runner` 的第二参数即 `cancellation: Option<RunCancellation>`（runners.rs:120），`runner.rs` 调用处已传入 `ctx.run_cancellation.clone()`——父 turn 取消即取消运行中的子代理（同步路径，契约 §7.2 运行中部分）。后台路径独立 token + registry 级联（§5.2）覆盖。

`ToolCallContext.run_cancellation` 已存在但子 build 未接。`runner.rs` 构建 `RunOptions` 时：

- 同步路径：直接传 `ctx.run_cancellation.clone()`（父取消 → 子随动，满足契约 §7.2 级联的运行中部分）。
- 后台路径：新建独立 token，但在 registry 登记 `parent_cancel_handle`（Phase 4 级联用）。

### 2.5 `SubagentLifecycleSink`（新文件 `tools/agent/lifecycle.rs`）

```rust
#[async_trait]
pub trait SubagentLifecycleSink: Send + Sync {
    async fn created(&self, identity: &SubagentIdentity, spec: &SubagentSpec) -> Result<(), SubagentLifecycleError>;
    async fn running(&self, identity: &SubagentIdentity) -> Result<(), SubagentLifecycleError>;
    async fn terminal(&self, identity: &SubagentIdentity, outcome: &SubagentOutcome) -> Result<(), SubagentLifecycleError>;
}
pub enum SubagentOutcome { Completed { stats: AgentCompletionStats }, Failed { error: String }, Cancelled }
pub struct NoopSubagentLifecycleSink; // 缺省实现（CLI/测试）
```

- **sink 内嵌 `AsyncAgentRegistry`**（构造注入 `Option<Arc<dyn Sink>>`）：`registry.rs` 的 `complete`（:252）/`fail`（:274）/`cancel` 已有"终态不可覆盖"守卫——"恰好一个 terminal"收敛在此，`AgentCancelTool` 走同一 registry 免费获得取消终态（codex 借鉴：不变量收敛点唯一化）。
- `created` 在 `tokio::spawn` 前调用，失败即 invocation 不启动（契约 §4）。
- sink 不接收 prompt/输出/secret。

### 2.6 registry 注入化与索引

- `ReactBuildConfig` + `pub async_agent_registry: Option<AsyncAgentRegistry>`；`tool_source.rs:354` 改 `config.async_agent_registry.clone().unwrap_or_default()`（默认行为不变）。
- registry entry 增索引字段：`invocation_id → { child_session_id, agent, depth, parent_session_id, status, stats, cancel_handle }`（为 Phase 2 寻址与 Phase 4 级联/统计备好）。

### 2.7 诊断

`runner.rs:184` `eprintln!` → `tracing::info!(target: "subagent_display", ...)`。

### 2.8 工具层修缮（T 档，随 Commit 1b）

工具族四件套（`agent`/`agent_get`/`agent_cancel`/`thread_get`）的接口质量修缮，全部小改、独立可测：

| # | 项 | 现状 | 改法 |
| --- | --- | --- | --- |
| T1 | agent_get 列表瘦身 + 紧凑序列化 | 无参列表用 `to_string_pretty` 输出全部条目**含 result 全文**（agent_get.rs:72-76）——50 条终态 × 数 KB result = 上下文炸弹 | 列表只回摘要（id/agent/status/duration_ms），result 仅单查返回；`to_string_pretty` → `to_string` |
| T2 | `estimated_paths` 幽灵参数 | worktree.rs:37-45 解析使用，但 input_schema 未声明（mod.rs:163-204） | 补进 schema（string[]，可选） |
| T3 | cancel 幂等返回 | 对已终态代理返回 Err → 模型视角"操作失败"（agent_cancel.rs:62-64） | tool 层把"已终态"分支转 `Ok(当前状态 JSON)`（契约 §7.2 幂等语义前移，registry 不动） |
| T4 | 错误文案可执行化 | depth 超限 / resolve 失败 / not found 均无指引 | depth → "Solve the task yourself"式；resolve 失败内联可用 profile 名；not found 区分"已被清理(>50 终态驱逐)/不存在" |
| T5 | sync 返回附 stats | `mod.rs:267` 以 `_` 丢弃 `AgentCompletionStats` | 返回尾部附 `{turns, tokens, tool_calls}` 一行 |
| T6 | worktree 元信息回传 | isolation=worktree 时父模型不知道 path/branch/是否保留（worktree.rs:140-166） | 返回附 `{worktree: {path, branch, preserved}}`（仅 worktree 模式） |
| T7 | 并行用法引导 | description 未提同回合多发 agent 调用 | 补一句用法说明 |
| T8 | timeout 语义 + max_runtime | timeout 在 background=true 时静默失效；后台代理无总时限 | schema 补说明 + `max_runtime` 参数（超时终态 Failed("deadline exceeded")）——载体 [演进方案 A2](./subagent-evolution-plan.md) |
| T9 | agent_get 阻塞等待 | 纯轮询，每查一次一轮 round-trip | `wait: bool` + `wait_timeout`（服务端等终态，超时返回当前状态）——载体 [演进方案 E1-5](./subagent-evolution-plan.md) |
| T10 | 工具族整理 | agent_get/thread_get 双"列出"入口无互相指路；tool_source.rs:375 死注释（"ListAgentsTool is not available in this build"，本仓即 anureo，语境错乱） | 描述互引 + agent_get 加 `status` 过滤；清死注释 |

（嵌套代理"叔叔不可见"不单列——Phase 1/2 共享 registry + parent 链落地后 `thread_get` 免费获得全树视图，即 codex `list_agents` 的对应物。）

**Phase 1 验收**：并发 100 同名 invocation 全 ID 唯一；嵌套 depth=1,2 递增、depth=3 被拒；fake sink 断言 created→running→terminal 恰一次；注入共享 registry 时 tool_source 复用；工具面：agent_get 列表无 result 字段、cancel 幂等、错误文案含可用名；`cargo nextest run -p agent` + clippy 零警告。

## 3. Phase 2 — ACP 一等 child session

### 3.1 `AcpSubagentRuntime`（apps/acp 新组件，实现 sink）

构造依赖：`Arc<SessionStore>`、`Arc<SessionRepository>`、`Arc<AsyncAgentRegistry>`、父会话 `SessionEntry`（owner/cwd/mcp/model 弱引用快照）。

```rust
impl SubagentLifecycleSink for AcpSubagentRuntime {
    async fn created(&self, id: &SubagentIdentity, spec: &SubagentSpec) -> Result<(), Error> {
        // 顺序 = 契约 §5.1，任何一步失败：若 durable 已建 ⇒ 先写 failed terminal 再返回错
        // 1) SessionStore::create_with_owner(child_session_id, parent.cwd.clone(), child_thread_id, parent.owner_principal)   // session.rs:394，幂等
        // 2) repository.insert_index_record(child_session_id, child_thread_id, owner, cwd, Some(parent_session_id))          // :514 canonical（owner 版本/ancestor/descendant activity 全走这里）
        // 3) repository.set_metadata_json(child, envelope(status="created"))                                                  // :1057
        // 4) registry.bind(child_session_id ↔ invocation_id)（cancel 寻址 + Phase 3 查表）
    }
    async fn terminal(&self, id, outcome) -> Result<(), Error> {
        // 统一 mutation（见 3.2）：metadata(status+stats) 与 lifecycle=closed 同事务
    }
}
```

### 3.2 统一 terminal mutation（新增 `SessionRepository::finish_subagent`）

契约 §5.4 明确要求：subagent status/stats 与 session lifecycle 收敛必须同一 canonical mutation。现状 `set_metadata_json` 与 `set_lifecycle` 各开连接、非原子。新增：

```rust
/// 单事务：set metadata(anureo.subagent) + set lifecycle='closed' + closed_at + record_activity(ancestors)
pub fn finish_subagent(&self, session_id: &str, subagent_meta_json: &str) -> rusqlite::Result<Vec<SessionIndexRecord>>;
```

（模式照抄 `set_archived`/`delete_all_indexed` 的事务+版本+ancestor 重算结构，session_repository.rs:1286/:1406。）

### 3.3 注入点（agent.rs prompt 路径）

`RunOptions` 增两个字段（`apps/acp/src/agent.rs:1518` 构造处填入）：

```rust
pub async_agent_registry: Option<AsyncAgentRegistry>,     // server 级单例（每 ACP server 一个 Arc）
pub subagent_sink: Option<Arc<dyn SubagentLifecycleSink>>, // AcpSubagentRuntime（引用父 SessionEntry 快照）
```

→ 贯穿 `build_react_runner` → `ReactBuildConfig`（接 Phase 1 接缝）。server 级 registry 实例放 ACP server 状态（与 SessionStore 同级），保证跨 prompt 寻址。

### 3.4 session/load(child) 与 session/list

- load：现有路径 `SessionStore::create_with_id(session_id, cwd, stored_thread_id)`（session.rs:385）+ checkpoint 按 thread_id 回放——child 历史=child checkpoint thread，天然可读。验收：Desk 从 SessionIndex 打开 child 并看到子执行记录。
- list：`insert_index_record` 已写 canonical 索引 + activity，37 号规范投影免费获得（含 `parentSessionId`）。

### 3.5 失败路径（契约 §5.1"不留幽灵"）

spawn 前失败（created 返回 Err）→ AgentTool 直接 tool error，无 durable 记录；created 成功后失败（runner panic/立即退出）→ registry.fail → sink.terminal(Failed) → `finish_subagent` 落 failed + closed。

**Phase 2 验收**：`session/list` 见 child（parent 正确）；`session/load(child)` 读到历史；created 失败不启动子代理；durable child 失败后无 running 残留；`cargo nextest run -p anureo-acp`。

### 3.6 配置继承三类字段契约（原则 5 的具体化）

`AcpSubagentRuntime` 快照父配置（契约 §5.2）与 `build_config_from_profile` 的 overlay 语义按三类字段约束：

| 类别 | 字段 | 规则 |
| --- | --- | --- |
| 继承 | provider、`acp_session_id`、`trace_thread_id`、skills/工具面基线 | parent clone 语义不变 |
| 覆盖 | model/tier/temperature（profile + 调用参数）、working_folder、builtin filter、role + AGENTS.md 拼接 | 现有 overlay 逻辑保持 |
| **活运行时强制回填** | cwd、审批/权限面（E2-4 收窄的基线）、`mcp_servers` 现值 | **禁止从冻结快照继承**——`created()` 时从活 `SessionEntry` 取（codex §5.2「运行时态回填」同构：角色换得掉模型，换不掉运行时边界） |

验收：单测断言三类字段来源，重点第三类——mock 过期快照 cwd，断言子配置取活值。

## 4. Phase 3 — 版本化 metadata 投影

### 4.1 绑定时序与方案选型

问题：初始 `ToolCall` 帧由 act_executor 在 `AgentTool::call` **之前**发出，彼时 identity 尚不存在；若初始帧无 meta，新 Desk 可能对 `toolName=agent` 触发旧 fallback（时间窗猜测）——违反契约 §6.3 优先级。

| 方案 | 做法 | 评估 |
| --- | --- | --- |
| A. 桥接层查表 | stream_bridge 投影时查共享 registry（call_id→meta） | 初始帧仍无 meta（identity 未建）；且桥需要 registry 句柄穿透 |
| **B. 显式绑定事件（选）** | agent-core 新增 `StreamEvent::SubagentStarted { call_id, identity_summary }`，在 identity 生成后立刻 emit；桥转成 `ToolCallUpdated{status:"running", meta}` | 首条带 meta 的 update 在 call 后毫秒级到达；契约允许（envelope 同构，初始帧 pending 不含 meta）；核心改动一个枚举变体 |

选 B；后续状态帧（ToolOutput/ToolEnd 转的 update）由桥从 registry 合并 meta（此时 identity 已绑定，查表安全）。

### 4.2 落点

1. `foundation/stream-event/src/types/stream_event.rs`：+ `SubagentStarted { call_id, invocation_id, child_session_id, parent_session_id, parent_tool_call_id, agent, model, depth }`（扁平字段，桥侧组 envelope）。
2. `tools/agent/mod.rs::call`：identity 生成后 `emit(StreamEvent::SubagentStarted{..})`（经 ctx 的 stream 通道；CLI runtime 打印一行摘要）。
3. `stream_bridge.rs`：`StreamUpdate::ToolCallUpdated` + `meta: Option<Meta>`；`SubagentStarted` → `ToolCallUpdated{status:"running", meta: envelope}`；ToolEnd 分支查 registry 合并 terminal envelope（status + stats）。
4. `create_tool_call`（:1300）：Meta 已有 `toolName`，新增 `anureo.dev` 命名空间仅在有 subagent 数据时插入（不动无子代理的普通工具帧）。
5. envelope 字段逐一对齐契约 §6.1/§6.2（version=1；model 可 null；stats terminal 必填 completed）。

### 4.3 Desk 侧交付清单（跨仓，契约 §8）

`acp-session-store.ts`（保留/合并完整 `_meta`）、`acp-legacy-adapter.ts` + `type-mapping.ts`（投影 + `parentSessionId→parentID` 唯一入口）、`ToolPart.tsx`（`isSubagentTool("agent"|"task")`、显式 sessionId Open/Cancel、stats 展示、未知 version 降级）。验证命令见 §6。

**Phase 3 验收**：父 tool card 帧序 `pending → running(meta) → … → terminal(meta+stats)`；有 versioned meta 时 Desk 不读 output 文本/不走时间窗；`parentToolCallId` 与当前 tool call 不一致时 Desk 拒开并记结构化诊断。

## 5. Phase 4 — 取消、统计与后台终态

取消按三个作用域拆分（原则 6）：**invocation**（§5.1，单个子代理）、**父 turn 级联**（§5.2，visited set 防环）、**session**（Desk 入口，经 §5.1 路由）。现有 `abort()` 粗暴终止（registry.rs:227）全面替换为优雅 `StopReason::Cancelled` 路径，避免子 checkpoint 半途截断。

### 5.1 invocation 级取消（契约 §7.2）

`session/cancel(child)` handler 扩展（现仅 `set_cancelled`，session.rs:435）：

```
set_cancelled(child)                      // 现有 turn 级
→ registry.cancel_by_session(child_session_id)   // Phase 2 索引；内部走既有 cancel 终态守卫
→ 子 runner StopReason::Cancelled
→ sink.terminal(Cancelled) → finish_subagent(cancelled + closed)
→ 父 tool call terminal update（见 5.3）
```

幂等：重复 cancel 命中终态守卫 → 返回成功 no-op；对已 terminal 的 child cancel 不改终态。

### 5.2 父级联取消（契约 §7.2 末段）

父 session cancel → registry 按 `parent_session_id` 索引 BFS（**visited set 防环**）→ 逐 child 走 5.1 的正常 terminal closure（非简单 abort）。

### 5.3 后台终态推送（契约 §7.3）

后台 invocation 完成时父 turn 已结束，turn 级流通道不可用。`AcpSubagentRuntime` 持**会话级** notifier（父 SessionEntry 的出站通道，与 turn 生命周期解耦）：terminal 时若父会话仍连接 → 直接发 `session/update`（tool_call_update + envelope）；离线 → 不补发内存事件，重连后以 SessionIndex durable metadata / session/load 为权威（契约 §1 目标 6：fallback 不掩盖协议错误，事件丢失不丢状态）。

### 5.4 统计

`AgentCompletionStats{turn_count, total_tokens, tool_calls_count}`（registry 现有）→ terminal envelope `stats`（§4.2 第 3 步合并）；failed/cancelled 附已消耗统计（契约 §6.2）。

**Phase 4 验收**：同步/后台/失败/取消四路径恰一个 terminal；重复 cancel 幂等；父取消级联 visited-set 正确；重连后状态从 durable 恢复；真实 ACP BDD（delegate explore → cancel → reload）绿。

## 6. 测试矩阵（合并契约 §10 与审计 §7）

| # | 测试 | 层 | Phase | 命令 |
| --- | --- | --- | --- | --- |
| 1 | identity 并发唯一（100 同名） | agent-core 单测 | 1 | `cargo nextest run -p agent identity` |
| 2 | depth 传播 + 超限拒绝 + tool_call_id 非空 | agent-core 单测 | 1 | 同上 `depth` |
| 3 | lifecycle 顺序/恰一次 terminal/cancel 路径 | agent-core 单测（fake sink） | 1 | 同上 `lifecycle` |
| 4 | registry 注入复用 + 索引一致性 | agent-core 单测 | 1 | 同上 `registry` |
| 5 | child 创建顺序/失败不留幽灵/finish_subagent 原子 | anureo-acp 集成 | 2 | `cargo nextest run -p anureo-acp subagent` |
| 6 | list/load child（含 parent relation、owner/revision/indexVersion） | anureo-acp 集成 | 2 | 同上 |
| 7 | 帧序 pending→running(meta)→terminal(meta+stats)；字段完整性 | wire 测试（先失败后实现） | 3 | `cargo nextest run -p anureo-acp stream_bridge` |
| 8 | cancel 精确命中/幂等/级联 visited-set/后台推送 | anureo-acp 集成 | 4 | 同 5 |
| 9 | Desk metadata 保留/parent 映射/禁 fallback/降级 | Desk 单测（跨仓） | 3 | `bun --cwd ..\anureo-feat-dev\packages\ui test` |
| 10 | 真实 ACP BDD：explore 委派→meta 帧→child 打开→cancel→reload | e2e | 4 | `npm --prefix e2e run test:bdd:dev` |
| 11 | 工具面：agent_get 列表无 result/紧凑序列化、cancel 幂等、错误文案含可用名、stats 附带 | agent-core 单测 | 1（提交 1b） | `cargo nextest run -p agent tools` |

全量门禁：`cargo clippy --workspace --all-targets -- -D warnings` + Desk `run type-check`。

## 7. 提交序列（每提交独立可测）

| # | 提交 | 内容 | 验证 |
| --- | --- | --- | --- |
| 1 | `feat(agent-core): add SubagentIdentity and unify invocation/thread IDs` | §2.1/2.2 | 测试 1、2 |
| 1b | `fix(tools): slim agent tool family interface` | §2.8 T1-T7、T10（T8→A2，T9→E1-5） | 测试 11 |
| 2 | `fix(agent-core): propagate depth/tool_call_id through RunnableConfig` | §2.3 | 测试 2 |
| 3 | `feat(agent-core): introduce SubagentLifecycleSink (exactly-once terminal)` | §2.5/2.4 | 测试 3 |
| 4 | `chore(agent-core): injectable registry + tracing diagnostics` | §2.6/2.7 | 测试 4 |
| 5 | `feat(acp): register subagents as durable child sessions` | §3.1-3.5 | 测试 5、6 |
| 6 | `feat(acp): publish versioned subagent tool metadata` | §4.1-4.2 | 测试 7 |
| 7 | `feat(desk): adapt anureo agent tools and child session metadata` | §4.3（跨仓） | 测试 9 |
| 8 | `feat(subagent): cancellation stats and background completion` | §5 | 测试 8、10 |
| 9 | `docs(subagent): record implementation status and release evidence` | 契约 §12.7 | 签收材料 |

提交 1-4 即[当晚清单](./subagent-phase1-code-plan.md)的细化版；跨仓提交（5-8）在描述中互相记录依赖 commit（审计 §6）。

## 8. 风险与对策

| 风险 | 影响 | 对策 |
| --- | --- | --- |
| 初始帧无 meta 的窗口 | 新 Desk 对 `agent` 帧短暂 pending | 方案 B 绑定事件毫秒级补帧；Desk 对 `agent`/`task` 且无 meta 时进 pending 态而非立即 fallback（旧 server 靠 terminal 后 fallback 兜底） |
| `resolve_tool_call_id` 空 call_id（stream_bridge.rs:412） | terminal 帧合不上 meta | identity 的 `parent_tool_call_id` 源自 ctx（非流），envelope 自带；合并键用 envelope 的 `parentToolCallId` |
| metadata/lifecycle 双写非原子 | 崩溃窗口产生矛盾状态 | `finish_subagent` 单事务（§3.2），禁止 handler 直连两个独立入口 |
| 共享 registry 无限增长 | server 长驻内存 | 终态条目保留至父 session 删除（session deleted 时清对应 invocation 条目）；上限 + LRU 兜底（codex V2Residency 的轻量版，驱逐/重载见[演进方案 E2-1](./subagent-evolution-plan.md)） |
| 旧 Desk × 新 anureo | 旧 UI 忽略 `anureo.dev` 命名空间 | envelope 只增不改 `_meta.toolName`；契约 §11 兼容矩阵成立 |
| 新 Desk × 旧 anureo | 永无 versioned meta | 契约 §6.3 优先级链：legacy part metadata → `<task>` output → 时间窗 |
| depth 守卫复活是行为变更 | 存量超深嵌套用法被拒 | 错误文案给出"Solve the task yourself"式指引（codex 同款）；`max_sub_agent_depth` 可配 |
| 子会话 MCP/模型继承 | child 缺配置导致子代理行为漂移 | `AcpSubagentRuntime` 快照父 SessionEntry 的 session_config/mcp_servers 传给子 build（契约 §5.2），单测锁定 |

## 9. 里程碑

| 里程碑 | 内容 | 出口条件 |
| --- | --- | --- |
| M1（当晚） | ✅ **已完成（2026-09-18）**：提交 1、1b、2-4（Phase 1 全部 + 工具面修缮），commits `6036840d`/`4c817f16`/`cc046b63`/`7b82a19c`/`d5ee7286` | 测试 1-4、11 绿（393/393）+ clippy 零警告 |
| M2 | ✅ **已完成（2026-09-18）**：提交 5-6 合并为 `4938b03f`（后端 child session + metadata），测试 5、6、7 绿（747+4 e2e） | Desk 经 SessionIndex 打开 child 的验收依赖跨仓 M3 |
| M3 | 提交 7（Desk 适配，跨仓） | 测试 9 绿；显式绑定禁时间窗 |
| M4 | 提交 8（取消/统计/后台）+ 9（签收） | ✅ **后端部分完成（2026-09-18）**：`4938b03f`（测试 8 绿）；跨仓 e2e 与签收材料待 Desk 侧后补 |

MVP 发布边界严格遵循审计 §8：缺任一项只称"实验性 in-process 子代理"。

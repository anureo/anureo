# codex 代码执行过程分析（2026-09-06）

> **状态**: 快照分析。追踪 codex-rs 中一条用户输入从 app-server 进入到事件回流客户端、以及 shell 工具调用的完整执行路径。行号为阅读时快照（`core/src` 尤其易漂移），以「文件 + 函数名」为可靠锚点。
> **阅读基础**: `core/src/tasks/{mod,regular,lifecycle}.rs`、`core/src/session/turn.rs`（run_turn / try_run_sampling_request / drain_in_flight）、`core/src/session/handlers.rs`（submission_loop）、`core/src/exec.rs`、`core/src/tools/` 目录、`ext/goal/src/`。
> **交叉参考**: [codex 架构分析](./codex-architecture-20260906.md)、[Goal 功能改进方案](../goal/goal-improvement-plan.md)

---

## 0. 全链路总览

```
客户端（IDE/Web/TUI）
   │ JSON-RPC over stdio/UDS
   ▼
app-server: message_processor ──► request_processors/turn_processor
   ▼
ThreadManager（core/src/thread_manager.rs）
   │ 定位/恢复/创建 Thread（rollout 重放 + state 恢复）
   ▼
Session 的 Op 队列（channel）
   ▼
submission_loop（core/src/session/handlers.rs:515）── Op::TurnInput
   ▼
turn_input::handle ──► Session::start_task（tasks/mod.rs）
   │  abort 旧任务 → 取 steering 积压 → 发 turn 生命周期事件 → tokio::spawn
   ▼
RegularTask::run（tasks/regular.rs）── 循环直到无积压输入
   ▼
run_turn（session/turn.rs:153）── hook 排空 → 预压缩 → 采样循环
   ▼
try_run_sampling_request（turn.rs ~2156）── client_session.stream() SSE
   │  ├─ 文本/推理增量 ──► EventMsg::AgentMessageDelta…（直发客户端）
   │  ├─ FunctionCall 完成 ──► 工具 future 进 in_flight（FuturesOrdered）
   │  └─ Completed(token_usage) ──► 记账 + RawResponseCompleted
   ▼
工具执行（tools/orchestrator → router → registry）
   ├─ shell：exec.rs build_exec_request → 审批判定 → 沙箱 → spawn → 输出截断
   ├─ MCP：mcp_tool_call.rs
   └─ 审批等待：ExecApprovalRequest 事件 → 客户端决定 → Op::ExecApproval 回流
   ▼
FunctionCallOutput ──► record_conversation_items ──► rollout JSONL 持久化
   ▼
needs_follow_up=true ──► 下一轮采样（带工具输出）……直至模型只发 assistant 消息
   ▼
on_task_finished ──► TurnComplete 事件 ──► flush rollout
   ▼
emit_thread_idle_lifecycle_if_idle ──► 扩展（goal 空闲续跑在此插入）
```

## 1. 阶段一：请求进入与线程定位（app-server → ThreadManager）

1. **message_processor**（`app-server/src/message_processor.rs`）解析 JSON-RPC，按方法路由到 `request_processors/` 下对应 processor（turn 请求 → `turn_processor.rs`）。
2. processor 通过 **ThreadManager**（`core/src/thread_manager.rs`）拿到目标线程：
   - 新线程：构建 `NewThread`，初始化 Session 运行时；
   - 已有线程：从 **rollout JSONL 重放**（`core/src/session/rollout_reconstruction.rs`）重建对话历史，叠加 **state**（SQLite：thread metadata、goal 等）恢复线程级状态；
   - fork：复制 rollout 快照（goal 扩展在 fork 前 flush 记账，见 `ext/goal/src/api.rs`）。
3. 一个 Thread 对应一个 **Session**（`core/src/session/session.rs`），宿主持有的是它的 **Op 发送端**——此后一切交互都是消息，没有直接方法调用栈进入 core。

**要点**：宿主 → core 的边界是 channel，天然支持宿主与 core 生命周期解耦（daemon 化、多连接共享线程）。

## 2. 阶段二：Op 队列与 submission_loop

`Op`（`codex-protocol` 定义）覆盖宿主全部操作。`submission_loop`（handlers.rs:515）是 Session 的串行入口：

```rust
match sub.op {
    Op::Interrupt => interrupt(&sess).await,
    Op::TurnInput { request, mode, reply } => turn_input::handle(&sess, *request, mode, sub.id).await,
    Op::RecoverTurn { thread_settings, reply } => turn_input::handle_recovery(...),
    Op::ExecApproval { id, turn_id, decision } => exec_approval(...),   // 工具审批回流
    Op::PatchApproval { id, decision } => patch_approval(...),          // apply_patch 审批回流
    Op::UserInputAnswer / Op::RequestPermissionsResponse / Op::DynamicToolResponse => …,
    Op::RefreshMcpServers / Op::ReloadUserConfig / Op::Compact / Op::ThreadSettings => …,
    Op::InterAgentCommunication { communication } => …,                 // 多 agent 信箱
    Op::Shutdown => …,
}
```

**要点**：

- 审批（ExecApproval/PatchApproval）也是 Op——工具执行中挂起等人时，客户端的决定从同一条队列回流，core 内不需要额外的回调通道；
- `Op::Interrupt` 与普通输入分流：输入走 `input_queue`（steering），interrupt 走硬取消。

## 3. 阶段三：turn 任务的生成（start_task）

`turn_input::handle` 最终调 `Session::start_task`（`core/src/tasks/mod.rs`）：

1. `abort_all_tasks(TurnAbortReason::Replaced)`——同线程同时只有一个活动任务（新任务顶掉旧任务）；
2. 从 `input_queue.get_pending_input()` 取**积压的 steering 输入**（turn 进行中用户又发的消息），合并进本轮；
3. 记录 turn 起点（`turn_timing_state.mark_turn_started`）与本 turn 起始 token usage（预算记账基线，goal 扩展消费）；
4. `emit_turn_start_lifecycle`——遍历 `TurnLifecycleContributor`（扩展钩子，goal 在此 mark_turn_goal_active）；
5. `tokio::spawn` 运行任务，产物装入 `RunningTask{ done: Arc<Notify>, handle: AbortOnDropHandle, cancellation_token, … }` 存入 `ActiveTurn`；
6. 任务天然带 `info_span!("turn", thread.id, turn.id, model, token_usage.*)`——**trace span 层级即执行层级**（submission dispatch → turn → session_task.run → run_turn → stream_request / receiving_stream / handle_responses），token 用量在 span 上可观测。

**要点**：turn 生命周期由 spawn 点统一收尾（`on_task_finished`），任务实现只管 `run`；abort 与正常完成走同一出口，事件顺序有保证。

## 4. 阶段四：RegularTask 与 run_turn 主循环

`RegularTask::run`（tasks/regular.rs）：

```rust
loop {
    let last_agent_message = run_turn(sess, ctx, next_input, prewarmed.take(), token).await?;
    if !sess.input_queue.has_pending_input(&sess.active_turn).await {
        return Ok(last_agent_message);   // 没有积压输入，turn 链结束
    }
    next_input = Vec::new();             // 有 steering 积压 → 立刻再跑一轮
}
```

`run_turn`（turn.rs:153）内部：

1. `drain_async_hook_results(before_user_prompt=true)`——上一轮异步 hook 的结果先落进历史；
2. 取预热 client session（无则新建，`ModelClientSession`）；
3. `run_pre_sampling_compact`——超阈值先压缩上下文（压缩失败按错误类别分流：TurnAborted / ToolCollision / 其他）；
4. 进入**采样循环**：反复调 `try_run_sampling_request` 直到模型不再要工具（或出错/取消）。

## 5. 阶段五：一次采样请求（SSE 流处理）

`try_run_sampling_request`（turn.rs ~2156）是执行过程的心脏：

```rust
let mut stream = client_session.stream(prompt, &model_info, …)   // Responses API SSE
    .or_cancel(&cancellation_token).await??;
let mut in_flight: FuturesOrdered<BoxFuture<'static, CodexResult<ResponseInputItem>>> = …;
loop {
    match stream.next().or_cancel(&cancellation_token).await {
        ResponseEvent::OutputItemAdded(item) => { /* 增量流式转发给客户端；绑定工具参数 diff consumer */ }
        ResponseEvent::OutputItemDone(item) => {
            let out = handle_output_item_done(ctx, item, …).await;
            if let Some(tool_future) = out.tool_future {
                in_flight.push_back(tool_future);      // 工具并行开跑
            }
            needs_follow_up |= out.needs_follow_up;
        }
        ResponseEvent::OutputTextDelta / ReasoningSummaryDelta / ToolCallInputDelta => {
            /* 解析后 send_event：AgentMessageContentDelta 等 */
        }
        ResponseEvent::Completed { response_id, token_usage, end_turn } => {
            sess.record_token_usage_info(&turn_context, token_usage).await;  // 预算记账
            if let Some(false) = end_turn { needs_follow_up = true; }
            break Ok(SamplingRequestResult { needs_follow_up, last_agent_message });
        }
        ResponseEvent::RateLimits(s) => sess.record_rate_limits_info(s).await,
        ResponseEvent::ModelsEtag(etag) => models_manager.refresh_if_new_etag(etag, …).await,
        /* ServerModel / SafetyBuffering / TurnModerationMetadata … */
    }
}
```

**关键设计**：

- **`in_flight: FuturesOrdered`**：一次响应里的多个工具调用**并行执行、结果按序回流**——顺序确定性给了模型，吞吐并行给了执行器；
- **流式直通**：OutputItemAdded/各种 Delta 边收边转发客户端，首字延迟不等人；
- **工具参数增量预览**：`ToolCallInputDelta` 经 `ToolArgumentDiffConsumer` 实时把「工具正在改什么」以 diff 事件发给 UI；
- 流在 `response.completed` 前断开视为错误（`Stream("stream closed before response.completed")`）；取消统一经 `or_cancel` 短路为 `CodexErr::TurnAborted`。

采样循环收尾：`drain_in_flight`（turn.rs ~2098）排空工具结果并 `record_conversation_items` 逐项入史（同时写 rollout）；`needs_follow_up` 为真则带着工具输出发起下一轮采样。

## 6. 阶段六：工具执行（FunctionCall → 输出）

`handle_output_item_done` 识别 `ResponseItem::FunctionCall` 后经 **ToolCallRuntime**（`core/src/tools/`）执行：

| 层 | 文件 | 职责 |
|---|---|---|
| 编排 | `tools/orchestrator.rs`、`tools/parallel.rs` | 一次响应内工具的执行计划与并发策略 |
| 路由 | `tools/router.rs` | 按工具名/namespace 分发（内置 vs MCP vs 动态工具） |
| 注册 | `tools/registry.rs` | 工具表与 spec（含 `tool_dispatch_trace.rs` 分发追踪） |
| 审批 | `tools/approvals.rs`、`tools/network_approval.rs` | 结合 `AskForApproval` 策略与命令风险判定是否需要人批 |
| 沙箱 | `tools/sandboxing.rs` → `sandboxing` crate → 平台 crate | 沙箱模式（read-only / workspace-write / danger-full-access）到平台机制（Seatbelt/landlock/…）的映射 |

### 6.1 shell 工具的执行细节（`core/src/exec.rs`）

1. `process_exec_tool_call`（exec.rs:295）：入口；
2. `build_exec_request`（exec.rs:319）：组装 `ExecParams{ command, cwd, env, timeout_ms, … }`，并按 sandbox policy **重写命令**（包一层沙箱启动，如 macOS Seatbelt profile / landlock / 代理网络）；
3. 审批判定：策略说不确定 → 发 `ExecApprovalRequest` 事件给客户端并挂起；客户端决定以 `Op::ExecApproval{id, turn_id, decision}` 从 submission_loop 回流（handlers.rs:174）——**等待人批期间 turn 不阻塞其他 Op**；
4. `execute_exec_request`（exec.rs:430）：spawn 子进程（cancellation token 一并注册），按 `ExecCapturePolicy` 收集 stdout/stderr，`ExecExpiration`（超时/取消）裁决终止；输出经**截断策略**（unified_exec 的 head-tail buffer：保头保尾，中间折叠）防止上下文爆炸；
5. 产出 `FunctionCallOutput`（exit code + 截断后输出）→ `record_conversation_items` 入史 + rollout 持久化 → 随下一轮采样回给模型。

### 6.2 其他工具

- **MCP 工具**：`core/src/mcp_tool_call.rs` + `session/mcp.rs`（server 生命周期、prewarm）；工具 schema 经 `tools` crate 的 spec 体系适配成 Responses API 形状。
- **apply_patch / web_search / image_generation** 等内置工具：同 registry 路由，各有审批语义（patch 走 `Op::PatchApproval`）。

## 7. 阶段七：事件回传与持久化

- **事件**：core 一切可见变化都是 `EventMsg`（`sess.send_event`）→ 宿主订阅。app-server 侧 `bespoke_event_handling.rs` 把 core 事件定制映射为 `ServerNotification` 信封发给客户端（TUI/CLI 则直接消费 Event）。
- **持久化**：对话项经 `record_conversation_items(..., PersistContext)` 写 **rollout JSONL**（追加式，带 ordinal）；turn 结束时 `flush_rollout`（失败发 Warning 事件并继续重试）。rollout 与 state 是两条线：前者可重放对话，后者存线程级结构化状态（goal、metadata、queued items）。
- **记账**：`record_token_usage_info` 累计 TokenUsage（input/cached/reasoning 细分），turn 级与线程级预算检查在此时点（goal 扩展的消费点之一）。

## 8. 阶段八：turn 收尾、空闲与 goal 的插入点

1. 任务返回后 spawn 点统一 `on_task_finished`：发 TurnComplete（携带最终 agent 消息/TokenCount），flush rollout；
2. `emit_turn_stop_lifecycle` / `emit_thread_idle_lifecycle_if_idle(cause)`（tasks/lifecycle.rs）——**没有活动 turn 且无积压输入时触发 thread idle**；
3. goal 扩展（`ext/goal/src/runtime.rs`）在 `on_thread_idle` 里 `continue_if_idle`：goal 仍 active 且线程空闲 → 作为 `TurnInputContributor` 注入 continuation prompt（模板含剩余 token/预算）→ 线程自动续跑下一 turn。外部 `set_thread_goal` 通过 `goal_state_permit` 与这条 idle 续跑互斥，防止「刚要续跑、目标被人改了」的竞态；
4. token/时间记账在 turn start（基线）、tool finish（progress snapshot + 信号量串行落账）、turn stop（对账）三处闭合（`ext/goal/src/accounting.rs`）。

## 9. 中断与 steering 的两条路径

| | steering（软） | interrupt（硬） |
|---|---|---|
| 入口 | turn 进行中的新输入 → `input_queue` | `Op::Interrupt` → `interrupt()` → `abort_all_tasks` |
| 时机 | 当前采样循环自然收尾后，RegularTask 检测 `has_pending_input` 立即续跑 | 立刻：cancellation_token 级联取消（SSE 读取 `or_cancel`、工具子进程、审批等待） |
| 历史 | 新输入正常入史 | 被中断轮以 `interrupted_turn_history_marker` 标注入史（ContextualUser 或 Developer 角色，模型能看见「上一轮被打断」） |
| turn 元数据 | 继承 parent/root turn id | `TurnAbortReason`（Replaced/Interrupted/…）区分原因 |

多 agent 下的「信箱」（`Op::InterAgentCommunication`）复用同一条队列，触发 turn 的邮件用 `MailboxParentProvenance` 归属父/根 turn。

## 10. 端到端示例（时序）

```
T0  客户端: thread/turn 请求 "跑一下测试并修复失败项"
T1  turn_processor → ThreadManager → Session Op::TurnInput → start_task
    ├─ TurnLifecycleContributor.on_turn_start（goal 记账基线）
    └─ TurnStarted 事件 → 客户端
T2  run_turn → 采样#1: stream() SSE
    ├─ AgentMessageDelta…（流式转发）
    ├─ OutputItemDone(FunctionCall shell "cargo test")
    │    ├─ 审批判定：策略放行（workspace-write 沙箱内）
    │    ├─ build_exec_request → 沙箱包装 → spawn
    │    ├─ ExecCommandStarted/OutputDelta 事件
    │    └─ FunctionCallOutput（exit 1 + head-tail 截断输出）入史/rollout
    └─ Completed(token_usage) → 记账 → needs_follow_up=true
T3  drain_in_flight → 采样#2（带测试输出）→ 模型发 apply_patch → PatchApprovalRequest
    └─ 客户端 Op::PatchApproval(decision=Accepted) → 工具执行 → FunctionCallOutput
T4  采样#3: 模型只发 assistant 消息 → turn 完成
    ├─ on_task_finished: TurnComplete + TokenCount
    ├─ flush_rollout（JSONL 落盘）
    └─ thread idle → goal.on_thread_idle（若 active 注入续跑 prompt）
```

## 11. 与 anureo 对应路径对照

| codex 环节 | anureo 对应 | 差异要点 |
|---|---|---|
| app-server message/turn processor | apps/server HTTP/WS + apps/acp agent.rs 的 prompt 处理 | 同构；anureo 无 Op 队列，ACP prompt 直接驱动 runtime |
| Session Op 队列 + submission_loop | 无（调用直入） | 消息化带来的审批回流/中断统一性是可借鉴项 |
| start_task / SessionTask | agent-core 的 agent 运行时（单任务） | codex 任务类型化（Regular/Compact/Review/UserShell） |
| run_turn 采样循环 + FuturesOrdered | agent-core ReAct 循环 | 并行工具有序回流、参数 diff 流式预览可借鉴 |
| tools 编排 + 审批 + 沙箱 | agent/tool + ACP requestPermission | 审批回流经同一条 Op 队列；输出 head-tail 截断可借鉴 |
| rollout JSONL + state | foundation/checkpoint-sqlite-store | 事件可重放 vs 快照 |
| thread idle → goal 续跑 | apps/acp goal_runner 独立循环 | 见 [goal 方案](../goal/goal-improvement-plan.md)（保持 server-owned 架构，只借记账/审计/steering） |
| trace span 层级 | tracing 已有 | 「span 即执行层级 + usage 字段」的做法可直接参考 |

## 12. 本文档的可靠性边界

- 已核实（读到源码）：submission_loop 的 Op 分支、start_task 流程、RegularTask 循环、run_turn 的采样循环骨架、SSE 事件分支、in_flight/drain、exec.rs 函数签名链、goal 扩展 idle 续跑与记账。
- 结构性推断（依据目录/依赖/调用点，未逐行验证）：app-server processor 到 ThreadManager 的确切调用序列、沙箱平台包装的具体注入方式、`tools/parallel.rs` 的并发策略细节、rollout 压缩时机。引用时请以函数名/文件为锚点复核。

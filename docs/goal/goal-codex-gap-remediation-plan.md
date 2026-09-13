# Goal 子系统 blocked 语义加固与 Codex 差距修复 — 开发方案

> 状态：**实施完成（阶段 A/B/C 已落地；D1–D3 决策项待产品评审，D4 长期项未排期）**
> 创建：2026-09-12
> 实施记录：阶段 A（A1–A4）与阶段 B（B1–B3 + `/goal budget` + `_session/goal` edit/editBudget）已完成并通过测试（goal 63/63；anureo-acp+agent 全量无 FAIL；clippy 零警告），逐项记录见 [goal-codex-alignment-todo.md](./goal-codex-alignment-todo.md) 进度表 P10/P11。阶段 C（C1 续跑元数据 / C2 revision 迁移 / C3 plan 豁免）未开工；D1–D3 决策项未评审。
> 背景：P0–P8 落地后对 `../codex`（`codex-rs/ext/goal`）做的一轮全量代码对拍，发现 blocked 相关 6 处真实偏差与 9 项未实现/偏差。本文档是这些差距的分级开发方案。
> 前置阅读：[goal-codex-alignment.md](./goal-codex-alignment.md)（§6 语义基线，已定稿）；[codex-acp-compatibility-audit.md](./codex-acp-compatibility-audit.md)；[goal-codex-alignment-todo.md](./goal-codex-alignment-todo.md)（P0–P9 进度）
> 原则：**增量修订既有定稿，不推翻 §6 基线**；修订清单见 §9。上游对拍基线：本仓库工作副本 `C:\Users\heycj\dev\codex`（`codex-rs/ext/goal` 全目录）。

---

## 1. 摘要

对拍结论：goal 子系统的核心语义（6 态状态机、blocked 仅自 active 进入、turn 不可恢复错误 → blocked、quota → usage_limited、预算优先不被覆盖）**与 Codex 对齐，无致命 bug**；但存在两类待办：

1. **修复类（阶段 A，P10）**：4 个小改动——turn 错误类型化分类（消除字符串猜测导致的误 blocked）、`/goal set|resume` 命令路径不触发续跑（入口行为不一致）、abort 路径误注入 budget wrap-up turn、blocked 提示词护栏缺失。
2. **对齐/功能类（阶段 B/C，P11–P12）**：`stop_active_goal_for_turn` 化（state lock + turn 绑定 + 即时通知）、budget_limited 的 resume 与覆盖规则、**exec 三连败 → ExecutionUnavailable blocked**（blocked 的第二个触发源，完全缺失）、续跑 turn 元数据（turn_trigger 等价物）、objective_updated steering 激活（当前是死代码）、plan mode 记账豁免。
3. **决策/长期（阶段 D）**：load_session 自动续跑、子代理 token 记账、fork goal 迁移、agent-core 中途注入通道。

预估：阶段 A 1.5–2 天，阶段 B 4–5 天，阶段 C 3–4 天，阶段 D 验证与决策 0.5–1 天（不含 D4 实施）。

## 2. 对拍基准与差距总表

| 编号 | 差距 | 严重度 | loom 现状 | codex 基准 | 阶段 |
|---|---|---|---|---|---|
| G1 | turn 错误分类靠字符串猜测，rate-limit 类错误会被误置 blocked | **高** | `apps/acp/src/agent.rs:1645-1666`（`contains("quota")/\|\|contains("429")`） | `ext/goal/src/extension.rs:361-385` 类型化 `CodexErrorInfo::UsageLimitExceeded` | A1 |
| G2 | `on_turn_error` 无 state lock、无 turn→goal 绑定校验、无即时事件 | 中 | `agent/goal/src/runtime.rs:111-128` | `ext/goal/src/runtime.rs:266-373`（permit + `current_active_goal_id_for_turn` + `thread_goal_updated`） | B1 |
| G3 | budget_limited 不可被 usage_limited 覆盖、budget_limited 不可 resume、无 budget-only 编辑 | 中 | `agent/goal/src/types.rs:47-56`、store `mark_usage_limited` 仅 active | `ext/goal/src/runtime.rs:331-337` `can_stop`；`api.rs:144-290` 外部 set(status=Active) 可复活任意状态 | B2 |
| G4 | blocked 提示词护栏缺失四条（连续 3 goal turn 口径、强制置位、resume 重置、反滥用） | **高**（服务端不计数，prompt 是唯一防线） | `agent/goal/src/tools.rs:330-347` 一句话 | `ext/goal/src/spec.rs` update_goal 描述四段 | A4 |
| G5 | `/goal set` / `/goal resume` 命令路径不触发 idle 续跑（`_session/goal` 路径会） | **高**（goal 可能永不启动） | `apps/acp/src/goal_runtime.rs:83-148`；早退 `apps/acp/src/agent.rs:1314-1361` | `ext/goal/src/runtime.rs:230`（`apply_external_goal_set` → `continue_if_idle`） | A2 |
| G6 | abort 路径也注入 budget wrap-up turn（用户停止后又冒自动 turn） | 中 | `agent/goal/src/runtime.rs:100-106` | `ext/goal/src/extension.rs:332-359`（abort 只记账，不新起 turn） | A3 |
| G7 | exec 连续失败 → ExecutionUnavailable blocked 完全缺失 | **高**（坏环境无限续跑烧 token） | 无 per-tool 结果记账 | `ext/goal/src/accounting.rs:102-152` + `extension.rs` on_turn_stop `execution_failure_goal` | B3 |
| G8 | mid-turn steering 缺失；`steering::objective_updated` 是死代码 | 中 | `agent/goal/src/steering.rs:113-114` 无调用方 | `ext/goal/src/runtime.rs:226-229` `inject_active_turn_steering` | C2 / D4 |
| G9 | 续跑 turn 无 turn_trigger / iterationCount / lastContinuationReason 元数据 | 中 | continuation 是普通用户 prompt | `ext/goal/src/runtime.rs:456` `turn_trigger:"goal"` | C1 |
| G10 | plan mode turn 不豁免 goal 记账 | 低 | `agent/goal/src/runtime.rs:81-87` 不看模式 | `ext/goal/src/extension.rs:247-250`（Plan → `clear_current_turn_goal`） | C3 |
| G11 | 子代理/workflow token 不计入根 goal 预算 | 低（先验证） | `apps/acp/src/agent.rs:1483-1486` 注释明确不装配 | `ext/goal/src/extension.rs:407-409` `record_descendant_token_usage` | D2 |
| G12 | 重启后（session/load）goal loop 停摆至下一次用户 prompt | 低（需产品决策） | `apps/acp/src/agent.rs:2049-2066` 只重发快照 | codex `restore_after_resume`（`runtime.rs:375-397`）也只恢复记账、不主动 continue——非明确偏差 | D1 |
| G13 | fork 不携带 goal（codex fork 复制 goal + flush 在途记账） | 低（已有盲审决策） | `apps/acp/src/agent.rs:1111-1123` | `ext/goal/src/api.rs:114-129` | D3 |
| G14 | 外部 set 组合面窄：无 objective-only / budget-only 编辑（改预算必须换 goal_id 清零计量） | 中 | `GoalService` 只有整体 set/pause/resume/clear/edit(objective) | `GoalSetRequest`（objective/status/token_budget 任意组合 + `max_goal_token_budget` clamp） | B2/C2 |
| G15 | analytics 事件归因（created/resumed/terminal/status_changed with turn attribution） | 低 | metrics 计数器 + otel | `ext/goal/src/analytics.rs` | backlog（不排期） |

保留差异（有意为之，不在本方案范围）：`update_goal` 的 `reason` / `completion_budget_report` 入参（loom 扩展，codex 无）；objective 文件化（P7）；blocked 的 `status_reason` 落库（loom 扩展）。

---

## 3. 阶段 A：修复（P10）——小改动，可立即开工

### A1（G1）turn 错误类型化分类

**现状与根因。** `apps/acp/src/agent.rs:1645-1666` 对 `run_agent_from_config` 的错误用 `lower.contains("quota") || msg.contains("429")` 判定 quota 路径，否则走 `on_turn_error` → goal 被置 blocked。任何不含这两个字面量的限流/额度错误（"rate limit exceeded"、代理改写文案、新 provider 措辞）都会误 blocked。goal-codex-alignment-todo.md P0 自己已记录此问题（foundation/llm 已有 `ErrorKind` 未被利用）。

**事实链（类型信息完整可达）。**
- `run_agent_from_config` 返回 `Result<RunCompletion, RunError>`（`agent/agent-core/src/run/runner.rs:121-126`）；
- `RunError::Run(RunnerError::Llm(ProviderError))`（`agent/agent-core/src/runner_error.rs:18`）；
- `ProviderError.kind: model_spec_core::error::ErrorKind`（`foundation/model-spec-core/src/error/kind.rs:9-34`），其中 `QuotaExhausted`（重试无效）/ `RateLimited`（可退避重试）/ `Billing` 已由各 provider 解析器映射（`foundation/llm/src/error/provider/*`）。

**设计。**

1. `agent/goal/src/types.rs`（或新 `error_class.rs`）新增分类枚举，goal crate 不依赖 model-spec-core（保持现有依赖边界），由 host 侧映射：

```rust
/// 终止性 turn 错误的 goal 语义分类（host 从 RunError 归一后传入）。
pub enum TurnErrorClass {
    /// 配额/额度耗尽：active → usage_limited（系统置位）。
    UsageLimited,
    /// 其余不可恢复错误：active → blocked。
    TurnError,
}
```

2. `apps/acp/src/agent.rs` 新增纯函数（便于单测）：

```rust
fn classify_run_error(e: &RunError) -> TurnErrorClass {
    if let RunError::Run(RunnerError::Llm(pe)) = e {
        match pe.kind {
            ErrorKind::QuotaExhausted | ErrorKind::Billing => TurnErrorClass::UsageLimited,
            _ => TurnErrorClass::TurnError,   // RateLimited 视为重试耗尽 → TurnError，
                                              // 与 codex「retries exhausted → block」一致
        }
    } else {
        TurnErrorClass::TurnError
    }
}
```

3. 替换 `agent.rs:1645-1666` 的 `Err(e)` 分支：`match classify_run_error(&e)` → `UsageLimited` 调 `on_provider_quota_exhausted_with_usage(&msg, Some(totals))`，`TurnError` 调 `on_turn_error(&msg, Some(totals))`。删除字符串判断；`tracing::warn!` 附带 `kind = ?pe.kind` 便于观测。
4. 兜底：若发现 `RunnerError` 其他变体也携带额度语义（如 `Remote`），在 classify 内补充，不回退到字符串匹配。

**边界。** `RateLimited` 归 `TurnError`：react runner 已对可重试错误做过退避，能浮出到 turn 级即为重试耗尽，此时 blocked（阻止续跑循环）与 codex 行为一致。`QuotaExhausted`/`Billing` 重试无效且用户可操作 → usage_limited（可 resume）。

**测试。**
- 单测 `classify_run_error`：构造各 `ErrorKind` 的 `ProviderError` 断言分类；
- 集成（goal e2e harness，参照现有 quota 用例）：伪 LLM 返回额度错误 → 断言 usage_limited；普通错误 → 断言 blocked 且 `status_reason` 归因正确；
- 回归：`turn_error_marks_goal_blocked` 系列测试改造为类型化注入。

**DoD。** agent.rs 中不再存在 quota/429 字符串匹配；额度类错误一律 usage_limited；metrics `record_usage_limited` / `record_blocked` 归因正确。

**预估。** 0.5 天（含测试）。

### A2（G5）`/goal set|resume` 命令路径触发续跑

**现状与根因。** `/goal` 命令在 `prompt()` 内提前 return（`agent.rs:1361`），位于尾部 goal 钩子（`agent.rs:1617-1690`）之前，因此命令 turn 结束后 `continue_if_idle` spawn 不会执行；`run_goal_subcommand`（`apps/acp/src/goal_runtime.rs:83-148`）的 Set 只做 `note_goal_armed`（+ 替换时 defer），Resume 只做 `on_goal_status_changed`，均不 kick。而 `_session/goal` 扩展路径 set/resume 都会 kick（`extensions/goal.rs:522-525`、`:676-688`）。结果：空闲会话里 `/goal set` 武装 goal、`/goal resume` 恢复后，goal 永远不启动（直到用户下次发消息触发尾部钩子）。

**设计。** 在 `run_goal_subcommand` 内对齐 `_session/goal` 路径：

```rust
GoalSubcommand::Set { .. } => {
    ...
    runtime.note_goal_armed(&outcome.goal.goal_id).await;
    if outcome.replaced_existing {
        // 保持 §6.6 快照替换 deferral：推迟到下一次 turn 边界
        runtime.defer_continuation()...
    } else {
        // fresh set：goal 已武装且无 deferral，立即尝试续跑
        let cont = runtime.clone();
        tokio::spawn(async move {
            if let Err(e) = cont.continue_if_idle().await { tracing::warn!(...) }
        });
    }
    ...
}
GoalSubcommand::Resume => {
    ...
    let _ = runtime.on_goal_status_changed(goal::GoalStatus::Active).await;
    // 对齐 extensions/goal.rs after_resume：resume 后自动续跑
    tokio::spawn(continue_if_idle)...
}
```

说明：
- fresh set 时 `on_turn_start`（本命令 turn 开始时）已清除旧 deferral，新 goal 无 deferral 写入，spawn 的 `continue_if_idle` 可直接启动；幂等由 `AcpTurnDriver::start_turn_if_idle` 的 `has_active_prompt` 检查 + `prompt()` 内 `begin_prompt` busy gate 双重保证。
- 替换（replaced_existing）保持 deferral 语义不动——用户刚改写目标，推迟到下一边界是 §6.6 定稿行为，codex 无此机制但 loom 有意收紧。
- 不采用「命令路径也走尾部钩子」的大改（早退结构承担了命令不需要 LLM 的语义），只在子命令处理点补 kick。

**测试。** 集成（embedded runtime）：空闲会话 `/goal set ...` → 断言续跑 prompt 已提交（`has_active_prompt` 或收到 continuation 文本）；`/goal set` 替换 → 断言不立即续跑、deferral 生效；`/goal resume` → 续跑启动。三入口（`/goal`、`_session/goal`、模型 `create_goal`）行为一致性断言。

**DoD。** 任一入口武装/恢复 goal 后，空闲会话都能在无需用户再次输入的情况下启动 goal turn。

**预估。** 0.5 天。

### A3（G6）abort 路径不再注入 budget wrap-up turn

**现状与根因。** `agent/goal/src/runtime.rs:100-106`：`on_turn_abort` → `stop_abnormal`（ActiveOrStopped 补账，可能把 goal 翻成 budget_limited）→ `inject_budget_steering_if_flipped` → 经二道门自动起一个收尾 turn。用户刚取消，却又出现自动 turn。codex `on_turn_abort`（`extension.rs:332-359`）只记账（ClearActive）+ 移除 TurnStartOptions，**绝不新起 turn**。

**设计。** `on_turn_abort` 删除 `inject_budget_steering_if_flipped` 调用，仅保留 `stop_abnormal`（返回类型改为 `Result<(), GoalStoreError>`，或保留返回注入文本但调用方忽略——推荐前者，签名收紧）。预算翻转照常落库；turn 尾部 `publish_neutral_goal_meta`（`agent.rs:1668-1683`）已会把 limited 快照推给 FE；`continue_if_idle` 因 budget_limited 非 active 自然拦截，无后续 turn。budget steering 的去重标记（`budget_limit_reported_goal_id`）在 abort 场景**不消费**——若用户之后 resume（B2 后 budget_limited 可 resume），首次跨预算仍会正常注入一次 wrap-up。

**测试。** 改造现有 abort 集成测试；新增：abort 时用量恰好跨预算 → 断言无新 turn、状态 budget_limited、快照已发布。

**DoD。** abort 后 0 个自动 turn；预算状态正确。

**预估。** 0.25 天。

### A4（G4）blocked 提示词护栏对齐 codex spec

**现状与根因。** 两侧都决定**不做服务端计数**（一致），blocked 三轮规则完全靠 prompt 约束。loom `agent/goal/src/tools.rs` update_goal 的 blocked 描述只有一句："roughly three consecutive attempts (each time reporting blocked and re-attempting without new information)"——把 codex 的「同一 blocking 条件跨 goal turn 复现」曲解成了「模型报告 blocked 后重试」，且丢失三条护栏。codex `ext/goal/src/spec.rs` 原文四段：

1. *"Set to `blocked` only when the same blocking condition has recurred for at least three consecutive goal turns (counting the original/user-triggered turn and any automatic continuations), the agent is at an impasse, and no alternative approach exists."*
2. *"Once the blocked threshold is satisfied, do not keep reporting that you are still blocked while leaving the goal active; set `status` to `blocked`."*
3. *"After a previously blocked goal is resumed, the resumed run starts a fresh blocked audit: do not immediately re-report blocked based on turns before the resume."*
4. *"Do not use `blocked` merely because the work is hard, slow, uncertain, incomplete, or would benefit from user clarification."*

**设计。** 逐段搬运（保留英文、术语与 codex 一致：`goal turns` / `automatic continuations` / `fresh blocked audit`），替换 tools.rs update_goal 描述中的 blocked 段落；`reason` 参数描述保留 loom 扩展（"what is blocking progress"）。同时核对 `create_goal` 描述中关于 blocked 的指引（codex create_goal 亦引用三-turn 口径），保持两处一致。

**测试。** tool spec 快照测试（断言关键短语：`three consecutive goal turns`、`fresh blocked audit`、`do not use blocked merely`）；行为属 prompt 治理，无确定性单测——在 e2e 手测清单中加一条（长 goal 场景观测 blocked 误报率，靠 metrics `record_blocked` 频率监控，见 §10 风险）。

**DoD。** 描述文本与 codex spec.rs 语义等价（四条护栏齐备）。

**预估。** 0.25 天。

---

## 4. 阶段 B：blocked 语义对齐（P11）

### B1（G2）`on_turn_error` 的 stop_active_goal_for_turn 化

**现状与根因。** `agent/goal/src/runtime.rs:111-128`：`store.read`（读「当前」goal）→ `mark_blocked(goal.goal_id)`。缺陷：(a) 未持 `goal_state_lock`，与用户 set/clear/pause 及 `continue_if_idle` 存在窗口（目前仅靠 goal_id CAS 兜底替换/清除场景）；(b) 无 turn→goal 绑定校验——报错的 turn 若并非在追当前 goal（如 goal 刚被换新、旧 turn 报错），会误 block 新 goal；(c) 系统 blocked 无即时通知（只有 turn 尾部快照）。codex `stop_active_goal_for_turn`（`runtime.rs:266-373`）三点齐备：permit 贯穿记账+状态写入+事件、`current_active_goal_id_for_turn` 绑定、`thread_goal_updated` 事件。

**设计。**

1. **turn 令牌与绑定**。`GoalRuntimeHandle` 增加：

```rust
turn_binding: tokio::sync::Mutex<Option<TurnBinding>>,   // 进程内，随 handle 生命周期
struct TurnBinding { turn_token: String, goal_id: String, account: bool }
```

- `on_turn_start` 扩展为 `on_turn_start_for(turn_token: &str)`（旧 `on_turn_start` 兼容封装，内部生成 token）：清 deferral、起墙钟，并在 goal 为 active 时写入绑定（`account = true`；plan mode 见 C3 时置 false）。
- `on_goal_replaced` / `on_goal_status_changed(非 Active)` / `on_turn_finish` / `on_turn_abort` / `on_turn_error` 结束时清绑定。
- host 侧 turn_token 取 `cancellation.generation()` 的字符串形式（prompt 已有，天然唯一且可追溯）。

2. **持锁路径**。`on_turn_error` 重写：

```rust
pub async fn on_turn_error(&self, reason: &str, totals: Option<TokenTotals>) -> Result<Option<String>, GoalStoreError> {
    // 锁序：state_lock → progress_permit（全 runtime 统一，见下）
    let permit = self.state_lock.acquire().await;      // 拿不到：warn 后按现状降级（尽力而为）
    let outcome = self.accounting.stop_abnormal(totals).await?;
    let bound = self.turn_binding.lock().await.take();  // 本 turn 绑定的 goal
    if let Some(binding) = bound.filter(|b| b.account) {
        if self.store.mark_blocked(&self.thread_id, &binding.goal_id, reason).await.is_ok() {
            metrics::global().record_blocked(&self.thread_id);
            self.notify_status_change().await;          // 见 (3)
        }
    }
    drop(permit);
    self.inject_budget_steering_if_flipped(outcome).await   // 保留：finish 前最后一段越界仍注入
}
```

- 绑定校验语义：goal 在 turn 开始后被用户替换 → 旧 turn 报错不再 block 新 goal（对齐 codex：替换后 `apply_external_goal_set` 会把新 goal 绑到当前 turn，但那是「用户显式接管」；loom 用 deferral + 新 goal_id，旧 turn 的 blocked 归因到旧 goal_id 上 CAS 失败自然丢弃，等价）。
- **锁序声明**（写入 alignment §6.4 修订）：`goal_state_lock` → `progress_accounting_lock`，单向。现有代码已隐式满足（`continue_if_idle` 持 state 后调 driver 不碰 progress；`service.pause` 持 state 后 `flush_wall_clock` 拿 progress），本项将其显式化并在两处加注释。

3. **即时状态通知**（codex `thread_goal_updated` 等价物，可选增强）。`GoalRuntimeHandle::new` 增加 `status_notifier: Option<Arc<dyn Fn(Goal) + Send + Sync>>`，host 装配时注入「`publish_neutral_goal_meta` + `_anureo.dev/goal/updated` 广播」组合回调（复用 `extensions/goal.rs:451-465` 的 broadcast 与 `publish_neutral`）。`mark_blocked` / `mark_usage_limited` / budget 翻转成功路径调用之。收益：FE 不再等 turn 尾部快照。注意回调内不得再触碰 store（防重入）；广播走 `tokio::spawn` 已是异步。

**测试。**
- 单测：绑定错位（turn A 无 goal → error 不 block 随后 set 的 goal）；替换竞态（error racing set：旧 goal_id CAS 失败，新 goal 不 blocked）；
- 并发：`on_turn_error` 与 `service.set`/`pause` 交错，断言终态唯一且合法；
- 通知：blocked 后断言 `_meta.goal` 快照与广播时序（在 turn 结束前）。

**DoD。** on_turn_error 仅影响绑定 goal；全程持 state lock（超时降级有日志）；系统置位即时广播。

**预估。** 2 天。

### B2（G3）状态覆盖与 resume 规则对齐

**现状。** ① store `mark_usage_limited` 仅 `WHERE status='active'`：补账先把 goal 翻成 budget_limited 后，quota 判定无法再覆盖成 usage_limited（codex `can_stop` 允许 BudgetLimited → UsageLimited，`runtime.rs:331-337`）。② `user_resumable = paused|blocked|usage_limited`（`types.rs:47-56`）：budget_limited 不可 resume，只能整体重 set（goal_id 变、tokens_used 清零）——用户「提预算继续」做不到。③ 无 budget-only / objective-only 的外部更新面（G14）。

**设计。**

1. `store.mark_usage_limited` 允许集改 `('active','budget_limited')`（`mark_blocked` 维持仅 `active`，与 codex 一致：blocked 不得覆盖 budget_limited）。
2. `types.rs`：`user_resumable` 增加 `BudgetLimited`；`store.resume` 允许集同步（`'paused','blocked','usage_limited','budget_limited'`）。resume 保留 tokens_used / budget / goal_id。
3. **budget-only 编辑**（对齐 codex `GoalSetRequest.token_budget`，保 goal_id）：`GoalService::update_budget(thread_id, budget: Option<i64>)`——非终态可用（复用 `edit` 的终态拒绝语义），store 增加 `update_budget` SQL（CAS goal_id，校验正数与上限，参照 create 路径的 `MAX_GOAL_TOKEN_BUDGET` 逻辑）。入口：
   - `_session/goal` 新 action `editBudget`（`NEUTRAL_GOAL_ACTIONS` + 附录 C 能力广播同步 + `map_service_error` 分支）；
   - `/goal budget <n>` 子命令（`agent::commands::GoalSubcommand::Budget`，parse + receipt）。
4. **推荐流程文档化**（重要，防呆）：budget_limited → `update_budget(更大值)` → `resume`；直接 resume 而不提预算会立即再次触顶（tokens_used 未清）。`_session/goal` 的 `resume` 响应里若 goal 为 budget_limited 且预算未变，附提示字段 `needsBudgetIncrease: true`（中立快照扩展字段，FE 可选消费）。
5. resume from budget_limited 后的续跑：由 A2 的 kick 承担（`/goal resume` 与 `_session/goal resume` 均已覆盖）。

**测试。** store 状态转移矩阵补齐（budget_limited → usage_limited / → active；blocked 不可被 usage_limited 覆盖）；service `update_budget`（终态拒绝、CAS）；e2e：预算触顶 → 提额 → resume → 续跑恢复。

**DoD。** 达到 codex `can_stop` 与外部 set 的等价能力；「提额继续」全链路可用。

**预估。** 1.5 天。

### B3（G7）exec 三连败 → ExecutionUnavailable blocked

**语义（codex `accounting.rs:102-152`）。** 每 turn 记录两个布尔：`failed_execution`（默认命名空间 `exec` 工具失败）、`successful_tool`（任一工具成功）。turn 结束时若：goal 绑定 && 无成功工具 && 有失败 exec → 同一 goal 连续计数 +1（任一成功工具清零；goal 更换重置）；**≥3 → blocked**，reason 形如 `execution unavailable after 3 consecutive failed execution turns`。目的：shell/环境坏掉时阻止续跑循环无限烧 token——这是 blocked 的第二个系统触发源，当前完全缺失。

**落点：不进 agent-core，纯 host 侧消费流事件。** `StreamEvent::ToolEnd { name, is_error, .. }` 与 `ToolError`（`foundation/stream-event/src/types/stream_event.rs:146-154, 174-177`）已经流经 `prompt()` 的 `on_event` 闭包（与 `capture_turn_usage` 同点位，`agent.rs:1565-1577`）。

**设计。**

1. `agent/goal/src/tool_accounting.rs` 新模块（per-thread，挂 `GoalRuntimeHandle`）：

```rust
struct TurnToolStats { failed_execution: bool, successful_tool: bool }
struct ToolAccounting {
    current: TurnToolStats,
    execution_failure_goal_id: Option<String>,
    consecutive_execution_failure_turns: u8,
}
impl ToolAccounting {
    fn record_tool_outcome(&mut self, tool: &str, failed: bool);
    fn begin_turn(&mut self);
    /// turn 结束：返回 Some(goal_id) 表示连续三连败达标（绑定 goal 由调用方注入）
    fn execution_failure_goal(&mut self, bound_goal_id: Option<&str>) -> Option<String>;
}
```

   规则对齐 codex：`record_tool_outcome` 中成功任意工具 → `successful_tool = true` 且清零连败计数；`failed && tool == "bash"` → `failed_execution = true`（**工具名映射**：codex `exec` ↔ loom `tool_basic::bash` 的注册名，落地时以 `create_acp_tools` 注册表实际名称为准，若是 `shell` 则用 `shell`；把名字做成 `tool_accounting::EXEC_TOOL_NAMES: &[&str]` 常量，容错双名）。
2. runtime 集成：
   - `on_turn_start_for` → `tool_accounting.begin_turn()`；
   - 新增 `GoalRuntimeHandle::record_tool_outcome(name, failed)`（锁内速记，不碰 store，零 IO——高频路径安全）；
   - `on_turn_finish` 末尾：`if let Some(gid) = self.tool_accounting.execution_failure_goal(bound_goal_id)` → 持 state lock → `mark_blocked(thread, &gid, "execution unavailable after 3 consecutive failed execution turns")` → metrics + B1 的即时通知。**注意顺序**：在 `inject_budget_steering_if_flipped` 与 `continue_if_idle` spawn 之前判定，blocked 后续跑自然拦截。
   - 错误 turn（`on_turn_error`）不判定（codex 在 turn_stop 判定，turn_error 路径已有自己的 blocked 语义，避免双重置位覆盖 reason）。
3. host 集成：`on_event` 闭包内，与 `capture_turn_usage` 并列：

```rust
if let Some(goal_rt) = goal_rt_for_event.clone() {
    match ev.event {
        StreamEvent::ToolEnd { name, is_error, .. } =>
            goal_rt.record_tool_outcome(&name, is_error),
        StreamEvent::ToolError { .. } =>
            goal_rt.record_tool_outcome("", true),   // name 未知时按失败 exec 计？
        _ => {}
    }
}
```

   `ToolError` 无工具名——**不计入 exec 判定**（保守：codex 只认 default-namespace exec 的 handler 级失败），仅 `ToolEnd{is_error:true, name∈EXEC_TOOL_NAMES}` 计入。子代理（namespace 事件）本阶段不计入，与 D2 一并处理。
4. 阈值与 reason 常量放 `tool_accounting`（`MAX_CONSECUTIVE_EXECUTION_FAILURES = 3`，与 codex 一致）。

**测试。**
- goal crate 单测：三连败触发（无成功工具）、任一成功工具清零、goal 更换重置、非 exec 失败不计入；
- 集成：伪 bash 工具连续 3 turn 失败（每 turn 无其他成功工具）→ 断言 blocked + reason + 续跑停止；第 2 turn 出现成功工具 → 不 blocked；
- 回归：正常混合工具调用（read 成功 + bash 失败）不触发。

**DoD。** 坏 shell 场景 goal 在 3 个 goal turn 后自动 blocked；无误伤。

**预估。** 1.5 天。

---

## 5. 阶段 C：续跑链路元数据与 steering（P12）

### C1（G9）续跑 turn 元数据（turn_trigger 等价物）

**现状。** continuation 经 `AcpTurnDriver::start_turn_if_idle`（`apps/acp/src/goal_runtime.rs:30-66`）以普通用户 prompt 提交：transcript 反复出现 "Continue working toward..." 用户消息，FE 无法区分 goal turn 与用户 turn；`iterationCount` / `lastContinuationReason` 缺失（alignment TODO P8+ 已列）。codex 用 `turn_trigger: Some("goal")` + TurnStartOptions 链。

**设计。** ACP PromptRequest 无元数据字段，走 `_anureo.dev` 通知 + 服务端 marker：

1. **iteration 计数**：`thread_goals` 增列 `iteration_count INTEGER NOT NULL DEFAULT 0`（迁移见 §7）。`on_turn_finish` 时若本 turn 绑定为 goal turn（B1 绑定 + marker 命中）则 +1；`create_goal` / 用户 set / replace 重置 0。
2. **marker 通道**：`AcpTurnDriver::start_turn_if_idle` 在 spawn prompt 前向 `sessions` 写 `ContinuationMarker { session_id, goal_id, reason }`（进程内 map，`SessionManager` 已有类似 per-session 状态位）；`prompt()` 取 busy gate 后 **take** marker（一次性），随第一批 session update 之前发出 `_anureo.dev/goal/continuation` 通知：

```json
{ "sessionId": "...", "goalId": "...", "iteration": 3, "reason": "active-goal" | "budget-limit" | "objective-updated" }
```

   FE 据此把紧随的 turn 标记为 goal 驱动（等价 codex `turn_trigger`），`reason` 即 `lastContinuationReason`。通知易失（断线丢）可接受——重连以 `_meta.goal` 快照收敛，iteration 字段并入快照（§7）。
3. reason 来源：`continue_if_idle`（active-goal）、`inject_budget_steering_if_flipped`（budget-limit）、C2 的 objective-updated（budget-limit 与 objective-updated 共用 wrap-up turn 时取后者优先）。

**测试。** 集成：连续续跑 3 轮 → 通知 iteration 递增 1/2/3；预算收尾 turn reason=budget-limit；marker 被并发用户 prompt 抢占时丢弃（take 语义）不发通知。

**DoD。** FE 可区分 goal turn；iterationCount / lastContinuationReason 数据可用（TODO P8+ 对应项关闭；FE 消费另行立项）。

**预估。** 1.5 天。

### C2（G8）objective_updated steering 激活（下一边界注入）

**现状。** `steering::objective_updated`（`agent/goal/src/steering.rs:113-114`）无任何生产调用方（死代码）。codex 中用户 mid-turn 编辑 objective → `inject_active_turn_steering` 即时注入当前 turn；loom 无 mid-turn 通道（R1 定稿降级），当前行为是「edit 落库，下一续跑 prompt 的 continuation 模板自然带新 objective」——模型**不会被告知 objective 变了**，长 turn 内继续按旧目标工作且切换无显式信号。

**设计（R1 框架内的最小闭环；mid-turn 注入见 D4）。**

1. `thread_goals` 增列 `objective_revision INTEGER NOT NULL DEFAULT 0`：`edit_objective` / `replace` / `create` 时维护（edit +1；create/replace 归 0）。
2. runtime 记 `last_seen_revision`（`GoalRuntimeHandle` 内存，`on_turn_start_for` 读取当前值时更新）。用户 edit mid-turn → revision 变化，turn 结束时 `continue_if_idle` 渲染前比较：`db_revision > last_seen_revision` → 渲染 `steering::objective_updated`（死代码转正）替代 continuation 模板，并同步 last_seen。效果：下一个 goal turn 的 prompt 明确说「objective 已被用户更新为 X」，而非默默换文案。
3. `_session/goal` 增加 `edit` action（objective 编辑，当前只有 `/goal edit` 命令入口；`NEUTRAL_GOAL_ACTIONS` + 能力广播同步）。编辑在 turn 运行中允许（非终态即可，与命令路径一致），B1 绑定保持（goal_id 不变，CAS 不受影响）。
4. C1 的 reason 联动：此场景 wrap-up/continuation turn 的 reason = `objective-updated`。

**测试。** 单测（revision 语义：edit +1 / replace 归 0）；集成：turn 运行中 edit → turn 结束后下一 goal turn prompt 含 objective_updated 文本；未 edit → 常规 continuation；终态 edit 拒绝（既有用例回归）。

**DoD。** mid-turn edit 后模型在下一 goal turn 得到显式变更提示；`objective_updated` 不再是死代码。

**预估。** 1 天。

### C3（G10）plan mode 记账豁免

**现状。** ACP 有 plan 模式（`/plan` 命令、`current_mode_update`，见 `apps/acp/src/extensions/command.rs:653-656`）；`on_turn_start`（`runtime.rs:81-87`）不看协作模式，plan turn 的用量照常计入 goal。codex：`start_turn(collaboration_mode)` 中 Plan → `account_tokens = false` + `clear_current_turn_goal`（`extension.rs:247-250`）。

**设计。** `prompt()` 解析当前 mode（`entry.session_config.mode`，plan 判定以现有 mode id 为准）传入 `on_turn_start_for(turn_token, plan: bool)`；plan=true 时：清 token/墙钟基线（本 turn 不记账）、B1 绑定写 `account = false`（turn error 不 block、exec 三连败不累计）、iteration 不递增。**goal 工具可见性不变**（codex 的 tools_visible 不受 mode 影响，仅影响记账）。turn 结束后绑定清除，下一非 plan turn 恢复正常。

**测试。** 集成：plan mode 下发消息 → goal tokens_used 不变；切回普通模式续跑 → 恢复记账；plan turn 报错 → goal 不 blocked。

**DoD。** plan turn 零 goal 记账、零 goal 副作用。

**预估。** 0.5 天。

---

## 6. 阶段 D：产品决策与长期项

### D1（G12）重启后 goal loop 恢复

**考证修正**：codex `on_thread_resume` → `restore_after_resume`（`runtime.rs:375-397`）**只恢复记账绑定，不主动 continue**；续跑由 `on_thread_idle`（turn 结束转换）驱动。故 loom `load_session` 不 kick 并非明确语义偏差，但「重启后 goal 停摆至下一次用户输入」的产品体验问题真实存在。

**选项**：a) 维持现状并文档化（本节即为文档）；b) `load_session` 后 spawn `continue_if_idle`（激进：重开应用即自动烧 token）；c) **推荐**——load 时重发快照（已有）+ FE 在 goal 卡片提供「恢复运行」按钮（B2 后 resume 已能 kick，零服务端新增）。选 b 需产品显式确认。

### D2（G11）子代理 token 记账

**先验证再设计**：`capture_turn_usage`（`agent.rs:2808-2818`）对闭包收到的**所有** llm usage 事件累计——若 tool-workflow 子 runner 通过 `any_stream_event_sender`（namespace 前缀）回传 Usage 事件，子代理 token 可能已被计入 goal（差距比注释声称的小）。验证手段：跑一个含 workflow 的 goal，对比 `tokens_used` 与主 turn usage。若确实未计入：host 侧在 workflow 工具结果处把子 runner usage 并入 `usage_acc`（对齐 codex `record_descendant_token_usage` 的净效果，不动 goal crate）。验证 0.5 天；若需实施另立方案。

### D3（G13）fork goal 迁移

现状 fork 不携带 goal（盲审 C2 决策，`agent.rs:1111-1123` 注释）；codex fork 复制 goal 且先 flush 在途记账（`api.rs:114-129`）。默认维持 loom 决策（fork = 全新上下文，goal 属于源会话的持续承诺）；若产品要「带着 goal 分叉」，需补 fork 时 goal 行复制 + `flush_thread_goal_progress` 等价物（B1 的 stop_abnormal 可复用）。列为决策项。

### D4（G8 完全体）agent-core mid-turn steering 通道

codex 的 `inject_active_turn_steering` 能把 budget_limit / objective_updated 注入**正在运行**的 turn。loom 等价物需要 agent-core react loop 支持：`RunParams` 增加 `steering_rx: Option<mpsc::Receiver<SteeringItem>>`，think 节点每次 LLM 调用前 drain 队列，作为 context/user 片段附加（不入 checkpoint 或入——需对齐 codex ResponseItem 语义另查）。涉及 checkpoint 兼容与上下文管理，**C2 落地后再评估必要性**（若边界注入已满足体验，不立项）。

---

## 7. 数据与协议变更汇总

**tasks.db 迁移**（沿 task-core 既有迁移机制，`thread_goals` 表）：

| 列 | 类型 | 默认 | 引入 | 维护点 |
|---|---|---|---|---|
| `objective_revision` | INTEGER | 0 | C2 | edit +1；create/replace 归 0 |
| `iteration_count` | INTEGER | 0 | C1 | goal turn 结束 +1；create/replace 归 0 |

**`_anureo.dev` 协议**：
- 新通知 `goal/continuation`（C1）：`{sessionId, goalId, iteration, reason}`；reason ∈ `active-goal|budget-limit|objective-updated`；
- `_session/goal` action 新增：`edit`（objective，C2）、`editBudget`（B2）——`NEUTRAL_GOAL_ACTIONS` 与附录 C.1 能力广播同步更新；
- `resume` 响应可选字段 `needsBudgetIncrease`（B2）。

**中立快照（`_meta.goal`）**：新增 `iterationCount`、`objectiveRevision`；核对 `statusReason` 已暴露（`status_reason` 列已有，落地时验证 `neutral_goal_snapshot` 映射）。

**状态机变更**：`budget_limited` 转为用户可 resume；`usage_limited` 可自 `budget_limited` 覆盖；`blocked` 仍仅自 `active`。

## 8. 测试计划总览

| 层 | 内容 |
|---|---|
| goal crate 单测 | classify（A1，host 侧纯函数）；状态转移矩阵扩展（B2）；ToolAccounting 全规则（B3）；revision/iteration 语义（C1/C2）；绑定与锁竞态（B1） |
| 集成（embedded runtime，沿用 P3 harness） | 三入口续跑一致性（A2）；abort 零自动 turn（A3）；exec 三连败 blocked（B3）；budget_limited → 提额 → resume → 续跑（B2）；plan turn 零记账（C3）；goal/continuation 通知时序与 iteration（C1）；mid-turn edit → 下一 turn objective_updated（C2） |
| e2e（`e2e/`，AGENT_DEMO_MODE 按需） | quota 错误 → usage_limited 快照（A1 回归）；伪工具失败三连 → blocked 快照（B3）；resume 流 UI 可达（B2，FE 部分另行） |
| 观测 | metrics：`record_blocked` / `record_usage_limited` 频率对比（A4 上线前后误报率）；otel span 附 `error_kind`（A1） |

## 9. 对定稿文档的修订清单（实施时同步提交）

[goal-codex-alignment.md](./goal-codex-alignment.md)：

| 节 | 修订 |
|---|---|
| §6.1 状态机 | `budget_limited` 改为「软终态、用户可 resume（需先提额）」；补充 `usage_limited` 可覆盖 `budget_limited`（can_stop 对齐） |
| §6.4 锁 | 显式声明锁序 `goal_state_lock → progress_accounting_lock`；`on_turn_error` 持锁窗口 |
| §6.5 steering | `objective_updated` 激活条件：下一 goal turn 边界注入（revision 驱动）；R1 降级条款追加「abort 不注入 wrap-up」 |
| §6.6 续跑 | `/goal set|resume` 与 `_session/goal` 三入口统一 kick；continuation marker/reason 机制 |
| 新增 §6.10（暂定编号） | exec 三连败 → ExecutionUnavailable blocked 规则（B3）；plan mode 豁免（C3） |

[goal-codex-alignment-todo.md](./goal-codex-alignment-todo.md)：追加 P10（阶段 A）、P11（阶段 B）、P12（阶段 C）条目并勾选管理；P0 已记录的「字符串判 quota」问题由 A1 关闭。

[../acp-spec/extensions/14-goal-scheduled-task.md](../acp-spec/extensions/14-goal-scheduled-task.md)：`_session/goal` action 集与 `goal/continuation` 通知入规范（附录 C 能力版本递增）。

## 10. 风险与开放问题

1. **A4 prompt 变更的行为风险**：护栏收紧可能让模型更晚/更早置 blocked——上线后观测 `record_blocked` 频率与人工抽检 status_reason；必要时微调措辞（纯 prompt 迭代，无代码风险）。
2. **B1 state lock 引入 turn-error 等待**：现有持锁窗口都短（set/clear 的读→写、continue_if_idle 的读→渲染→spawn 即返）；2s 超时降级路径保留并打日志。锁序若未来被打破（progress → state 反向）会死锁——靠注释 + 文档声明约束，无编译期保证（开放：可考虑 runtime 内统一 `acquire_both()` 辅助函数）。
3. **B2 直接 resume 未提额**：立即再次触顶——靠 `needsBudgetIncrease` 提示 + 文档推荐流程缓解；是否在 resume 时自动拒绝（budget 未变且已超）留待评审（倾向不拒绝，保持 resume 语义纯粹）。
4. **B3 误伤**：模型换路径后前两轮失败残留计数——codex 同款规则（任一成功工具清零）已缓解；blocked 可 resume，风险可控。
5. **C1 marker 与并发用户 prompt 竞态**：take 语义保证一次性；用户 prompt 抢先时 marker 丢弃、iteration 不递增（该 turn 不是 goal turn）——符合语义。
6. **开放问题**：① `ToolError`（无工具名）是否应计入 exec 失败（本方案保守不计）；② 子代理 namespace 事件的 exec 失败/usage 是否并入（随 D2）；③ D1 选 b 的产品确认；④ D4 是否立项取决于 C2 效果。

## 11. 实施顺序与工作量

| 顺序 | 项 | 依赖 | 预估 |
|---|---|---|---|
| 1 | A1 类型化错误分类 | 无 | 0.5d |
| 2 | A4 blocked prompt 护栏 | 无（可与 A1 并行） | 0.25d |
| 3 | A2 `/goal` 路径续跑 | 无 | 0.5d |
| 4 | A3 abort 不注入 | 无 | 0.25d |
| 5 | B1 stop 化（绑定 + 锁 + 通知） | 建议 A1 先行（错误路径收敛） | 2d |
| 6 | B2 状态覆盖与 resume + editBudget | 无硬依赖 | 1.5d |
| 7 | B3 exec 三连败 | B1 的绑定机制 | 1.5d |
| 8 | C2 objective_updated 激活（含 revision 迁移） | 无硬依赖 | 1d |
| 9 | C1 续跑元数据（含 iteration 迁移 + 通知） | C2 的 reason 联动 | 1.5d |
| 10 | C3 plan mode 豁免 | B1 绑定 | 0.5d |
| 11 | D1/D2/D3 决策与验证 | — | 0.5–1d |

阶段 A 合计 ≈ 1.5d；阶段 B ≈ 5d；阶段 C ≈ 3d。A/B/C 全量 ≈ 9.5d（不含 D4 与 FE 消费侧）。

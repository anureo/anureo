//! [`GoalRuntimeHandle`]：goal 运行时编排（alignment §5 runtime.rs / §6.5/§6.6）。
//!
//! 钩子由宿主（apps/acp / apps/cli）在 **turn 边界**调用（R1 降级：mid-turn
//! 注入通道不存在，见 alignment 附录 B.6）：
//!
//! | 钩子 | 行为 |
//! |---|---|
//! | `on_turn_start` | 清 deferral + 墙钟起表（仅 active） |
//! | `on_turn_finish` | `ActiveOnly` 补账 + flush 墙钟 + budget 一次性 steering（越界时经二道门注入收尾 turn） |
//! | `on_turn_abort` | `ActiveOrStopped` 补账（不改状态） |
//! | `on_turn_error` | 补账 + active → `blocked` |
//! | `on_session_idle` | `continue_if_idle` 全流程（§6.6） |
//!
//! 可见性检查（goal 是否对该 session 可见）由宿主负责——只对 goal 可见的
//! session 装配本 handle 即可。sub-agent / workflow 不装配（工具不可见、不计账）。

use std::sync::Arc;

use async_trait::async_trait;

use crate::accounting::{GoalAccounting, TokenTotals};
use crate::metrics;
use crate::service::{GoalService, GoalServiceError, GoalStateLock};
use crate::steering;
use crate::store::{GoalStore, GoalStoreError};
use crate::types::{AccountingMode, Goal, GoalStatus};

const GOAL_TOOLS_UNKNOWN: u8 = 0;
const GOAL_TOOLS_VISIBLE: u8 = 1;
const GOAL_TOOLS_MISSING: u8 = 2;

/// 宿主提供的 turn 驱动。`start_turn_if_idle` 是**幂等二道门**（§6.6）：
/// 仅当宿主确认该 session 当前无运行中 turn 时才真正启动；返回是否启动。
#[async_trait]
pub trait TurnDriver: Send + Sync {
    async fn start_turn_if_idle(&self, thread_id: &str, message: &str) -> Result<bool, String>;

    /// C1（G9）：带元数据的 goal turn 启动（codex `turn_trigger:"goal"`
    /// 等价物）。默认退化为 `start_turn_if_idle`——测试 mock 与未升级宿主
    /// 无需感知元数据；宿主 override 时应向客户端发出
    /// `_anureo.dev/goal/continuation` 通知后再启动。
    async fn start_goal_turn(
        &self,
        thread_id: &str,
        message: &str,
        meta: GoalTurnMeta,
    ) -> Result<bool, String> {
        let _ = meta;
        self.start_turn_if_idle(thread_id, message).await
    }
}

/// C1：goal 驱动 turn 的元数据。
#[derive(Debug, Clone)]
pub struct GoalTurnMeta {
    pub goal_id: String,
    pub iteration: i64,
    pub reason: GoalTurnReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalTurnReason {
    /// 常规 idle 续跑。
    ActiveGoal,
    /// 预算触顶后的收尾 turn（R1 二道门）。
    BudgetLimit,
    /// 目标被用户 mid-turn 修改（objective_updated steering，C2）。
    ObjectiveUpdated,
}

pub struct GoalRuntimeHandle {
    thread_id: String,
    store: GoalStore,
    service: GoalService,
    accounting: Arc<GoalAccounting>,
    state_lock: GoalStateLock,
    driver: Arc<dyn TurnDriver>,
    /// B1：turn→goal 绑定（codex `current_active_goal_id_for_turn` 等价物）。
    /// `on_turn_start_for` 写入；turn 结束（finish/abort/error）消费/清除。
    /// 替换/暂停/清除后 CAS 失败即丢弃——错误 turn 不得误伤新 goal。
    turn_binding: tokio::sync::Mutex<Option<TurnBinding>>,
    /// Runtime-owned 状态变化（blocked/limited/continuation iteration）后的
    /// 即时快照通知（codex `thread_goal_updated` 等价物）。回调内不得触碰
    /// store（防重入），广播由宿主自行 spawn；未装配时退化为 turn 尾部快照。
    status_notifier: std::sync::Mutex<Option<StatusNotifier>>,
    /// B3：per-tool 结果记账（exec 三连败 → ExecutionUnavailable blocked）。
    tool_accounting: crate::tool_accounting::ToolAccounting,
    /// C2：本 runtime 已消费的 objective_revision（plan turn 起点同步）。
    /// 用户 mid-turn edit → DB revision 前进 → 下一续跑边界渲染
    /// objective_updated steering。
    last_seen_revision: std::sync::atomic::AtomicI64,
    /// 主 session 的最终 React 配置是否仍包含三个 model-facing goal tools。
    ///
    /// 初始为 unknown，允许首次 continuation 作为能力探测；一旦宿主确认工具
    /// 缺失，idle continuation fail-closed，避免 active goal 在无法调用
    /// `update_goal` 的情况下无限续跑。后续用户 turn 重新构建出完整工具集后
    /// 可恢复为 visible。
    goal_tools_visibility: std::sync::atomic::AtomicU8,
}

/// 系统置位后的即时通知回调（B1）。
pub type StatusNotifier = Arc<dyn Fn(Goal) + Send + Sync>;

#[derive(Debug, Clone)]
struct TurnBinding {
    #[allow(dead_code)] // 标识性字段：用于日志与未来跨 turn 匹配
    turn_token: String,
    goal_id: String,
    /// plan mode 等豁免场景为 false：不记账、不 block、不计失败连击（C3）。
    account: bool,
}

impl GoalRuntimeHandle {
    pub fn new(
        store: GoalStore,
        thread_id: impl Into<String>,
        driver: Arc<dyn TurnDriver>,
    ) -> Arc<Self> {
        let thread_id = thread_id.into();
        let accounting = GoalAccounting::new_unarmed(store.clone(), &thread_id);
        let state_lock = GoalStateLock::new();
        let service = GoalService::with_state_lock(store.clone(), state_lock.clone());
        Arc::new(Self {
            thread_id,
            store,
            service,
            accounting,
            state_lock,
            driver,
            turn_binding: tokio::sync::Mutex::new(None),
            status_notifier: std::sync::Mutex::new(None),
            tool_accounting: crate::tool_accounting::ToolAccounting::new(),
            last_seen_revision: std::sync::atomic::AtomicI64::new(0),
            goal_tools_visibility: std::sync::atomic::AtomicU8::new(GOAL_TOOLS_UNKNOWN),
        })
    }

    /// 由宿主在最终 React 配置构建后回报 Goal tools 是否完整可见。
    pub fn record_goal_tools_visibility(&self, visible: bool) {
        self.goal_tools_visibility.store(
            if visible {
                GOAL_TOOLS_VISIBLE
            } else {
                GOAL_TOOLS_MISSING
            },
            std::sync::atomic::Ordering::SeqCst,
        );
    }

    pub fn goal_tools_visible(&self) -> Option<bool> {
        match self
            .goal_tools_visibility
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            GOAL_TOOLS_VISIBLE => Some(true),
            GOAL_TOOLS_MISSING => Some(false),
            _ => None,
        }
    }

    /// B1：装配系统置位即时通知（host 在创建 handle 后调用一次）。
    pub fn set_status_notifier(&self, notifier: StatusNotifier) {
        *self
            .status_notifier
            .lock()
            .expect("status notifier mutex poisoned") = Some(notifier);
    }

    async fn notify_status_change(&self, goal: Goal) {
        let cb = self
            .status_notifier
            .lock()
            .expect("status notifier mutex poisoned")
            .clone();
        if let Some(cb) = cb {
            cb(goal);
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    pub fn accounting(&self) -> &Arc<GoalAccounting> {
        &self.accounting
    }

    pub fn service(&self) -> &GoalService {
        &self.service
    }

    pub fn state_lock(&self) -> &GoalStateLock {
        &self.state_lock
    }

    /// on_turn_start：清 deferral（§6.6）+ 墙钟起表（仅 active）+ B3 本 turn
    /// 工具统计重置。
    pub async fn on_turn_start(&self) -> Result<(), GoalStoreError> {
        self.store
            .clear_continuation_deferral(&self.thread_id)
            .await?;
        self.accounting.start_wall_clock_if_active().await?;
        self.tool_accounting.begin_turn();
        Ok(())
    }

    /// on_turn_start 的 B1 扩展：绑定本 turn 正在追逐的 active goal。
    /// `turn_token` 仅为标识/日志用途（每 prompt 唯一即可）。
    pub async fn on_turn_start_for(&self, turn_token: &str) -> Result<(), GoalStoreError> {
        self.on_turn_start().await?;
        if let Ok(Some(goal)) = self.store.read(&self.thread_id).await {
            if goal.status == GoalStatus::Active {
                self.last_seen_revision
                    .store(goal.objective_revision, std::sync::atomic::Ordering::SeqCst);
                *self.turn_binding.lock().await = Some(TurnBinding {
                    turn_token: turn_token.to_string(),
                    goal_id: goal.goal_id,
                    account: true,
                });
            }
        }
        Ok(())
    }

    /// 流事件热路径（B3）：host 在 `on_event` 闭包中转发 `ToolEnd` 结果。
    /// 同步、零 IO，不得阻塞。
    pub fn record_tool_outcome(&self, tool: &str, failed: bool) {
        self.tool_accounting.record_tool_outcome(tool, failed);
    }

    /// 读取并清除本 turn 绑定（turn 结束路径共用）。
    async fn take_turn_binding(&self) -> Option<TurnBinding> {
        self.turn_binding.lock().await.take()
    }

    /// on_turn_finish（正常结束）：补账 + 预算越界时经二道门注入一次收尾 turn。
    /// 返回 `Some(text)` 仅当本次触发了 budget steering 注入。
    pub async fn on_turn_finish(
        &self,
        totals: Option<TokenTotals>,
    ) -> Result<Option<String>, GoalStoreError> {
        let bound = self.take_turn_binding().await;
        if matches!(bound.as_ref().map(|b| b.account), Some(false)) {
            // C3：豁免 turn——零记账、零置位、不迭代。
            return Ok(None);
        }
        let outcome = self.accounting.finish_turn(totals).await?;
        // B3：exec 三连败判定（在 budget 注入之前——blocked 后续跑自然拦截；
        // 若 finish 补账先把 goal 翻成 budget_limited，则 mark_blocked 的
        // active CAS 失败，预算优先，与覆盖规则一致）。
        if let Some(binding) = bound.filter(|b| b.account) {
            if let Some(goal_id) = self
                .tool_accounting
                .execution_failure_goal(Some(&binding.goal_id))
            {
                let permit = self.state_lock.acquire().await;
                let blocked = self
                    .store
                    .mark_blocked(
                        &self.thread_id,
                        &goal_id,
                        "execution unavailable after 3 consecutive failed execution turns",
                    )
                    .await;
                drop(permit);
                if let Ok(goal) = blocked {
                    metrics::global().record_blocked(&self.thread_id);
                    self.notify_status_change(goal).await;
                    return Ok(None);
                }
            }
        } else {
            // 无绑定/豁免 turn：不归属、不累计
            let _ = self.tool_accounting.execution_failure_goal(None);
        }
        self.inject_budget_steering_if_flipped(outcome).await
    }

    /// on_turn_abort（用户取消）：补账，不改状态，**不注入任何新 turn**。
    ///
    /// A3（gap-remediation G6）：对齐 codex `on_turn_abort`（ext/goal
    /// extension.rs）——abort 只做 `ActiveOrStopped` 补账（含 budget 越界
    /// 翻转落库），绝不经二道门起收尾 turn：用户已显式停止，预算状态由
    /// turn 尾部的中立快照发布，下一次自然交互时继续生效。
    pub async fn on_turn_abort(&self, totals: Option<TokenTotals>) -> Result<(), GoalStoreError> {
        let bound = self.take_turn_binding().await;
        if matches!(bound.as_ref().map(|b| b.account), Some(false)) {
            return Ok(()); // C3：豁免 turn 不补账
        }
        self.accounting.stop_abnormal(totals).await.map(|_| ())?;
        Ok(())
    }

    /// on_turn_error：补账 + active → `blocked`（§6.1）。
    ///
    /// B1（gap-remediation G2）：持 `goal_state_lock` 贯穿补账与置位（锁序
    /// state_lock → progress_accounting_lock，全 runtime 单向）；只 block
    /// **本 turn 绑定**的 goal（`on_turn_start_for` 写入；无绑定时退化为读
    /// 当前 goal，兼容 turn 中途 create_goal 场景——codex
    /// `mark_current_turn_goal_active` 同语义）。绑定存在但 CAS 失败（goal
    /// 被替换/暂停/清除）时**不得回退**——这正是防误伤新 goal 的保护。
    pub async fn on_turn_error(
        &self,
        reason: &str,
        totals: Option<TokenTotals>,
    ) -> Result<Option<String>, GoalStoreError> {
        let bound = self.take_turn_binding().await;
        if matches!(bound.as_ref().map(|b| b.account), Some(false)) {
            // C3：豁免 turn 报错不落任何状态（plan 轮与 goal 无关）。
            return Ok(None);
        }
        let permit = self.state_lock.acquire().await;
        let outcome = self.accounting.stop_abnormal(totals).await?;
        let mut blocked_goal: Option<Goal> = None;
        match bound {
            Some(binding) if binding.account => {
                if let Ok(goal) = self
                    .store
                    .mark_blocked(&self.thread_id, &binding.goal_id, reason)
                    .await
                {
                    metrics::global().record_blocked(&self.thread_id);
                    blocked_goal = Some(goal);
                }
            }
            // account=false（plan 等豁免 turn）：报错也不 block（C3 语义）。
            Some(_) => {}
            _ => {
                // 无绑定：仅当当前 goal 仍为 active 时才置位（CAS 双保险），
                // 兼容 turn 中途 create_goal 场景。
                if let Ok(Some(goal)) = self.store.read(&self.thread_id).await {
                    if goal.status == GoalStatus::Active {
                        if let Ok(updated) = self
                            .store
                            .mark_blocked(&self.thread_id, &goal.goal_id, reason)
                            .await
                        {
                            metrics::global().record_blocked(&self.thread_id);
                            blocked_goal = Some(updated);
                        }
                    }
                }
            }
        }
        drop(permit);
        if let Some(goal) = blocked_goal {
            self.notify_status_change(goal).await;
        }
        self.inject_budget_steering_if_flipped(outcome).await
    }

    /// provider 配额耗尽（alignment 附录 B.5 `QuotaExhausted`）：
    /// active → `usage_limited`（系统置位，非 runner 失败）。
    pub async fn on_provider_quota_exhausted(&self, reason: &str) -> Result<(), GoalStoreError> {
        self.on_provider_quota_exhausted_with_usage(reason, None)
            .await
    }

    /// provider 配额错误的完整 turn-end 路径：先按异常模式补记当前 turn，
    /// 再把仍为 active 的 goal 置为 usage_limited。
    pub async fn on_provider_quota_exhausted_with_usage(
        &self,
        reason: &str,
        totals: Option<TokenTotals>,
    ) -> Result<(), GoalStoreError> {
        let permit = self.state_lock.acquire().await;
        let _ = self.accounting.stop_abnormal(totals).await?;
        let mut limited: Option<Goal> = None;
        if let Ok(Some(goal)) = self.store.read(&self.thread_id).await {
            if let Ok(updated) = self
                .store
                .mark_usage_limited(&self.thread_id, &goal.goal_id, reason)
                .await
            {
                metrics::global().record_usage_limited(&self.thread_id);
                limited = Some(updated);
            }
        }
        drop(permit);
        if let Some(goal) = limited {
            self.notify_status_change(goal).await;
        }
        Ok(())
    }

    /// 模型通过 update_goal 改为 complete/blocked 前，补记工具调用前已经产生的
    /// usage 并 flush 墙钟。否则状态先离开 active 后，turn-end 的 ActiveOnly
    /// 门控会有意拒绝这段最后用量。
    pub async fn before_model_status_update(
        &self,
        totals: Option<TokenTotals>,
    ) -> Result<(), GoalStoreError> {
        if let Some(totals) = totals {
            let _ = self.accounting.record_usage(totals).await?;
        }
        let _ = self
            .accounting
            .flush_wall_clock(AccountingMode::ActiveOrStopped)
            .await?;
        Ok(())
    }

    /// on_session_idle → `continue_if_idle` 全流程（§6.6）。
    /// 返回是否启动了续跑 turn。
    pub async fn continue_if_idle(&self) -> Result<bool, GoalStoreError> {
        // 与 Codex `tools_visible()` 的 fail-closed 语义对齐：若本 session
        // 最近一次最终配置已确认缺少 Goal tools，模型无法终止 active goal，
        // 因此绝不能继续自动启动新 turn。unknown 仅允许首次探测 turn。
        if self.goal_tools_visible() == Some(false) {
            tracing::error!(
                thread_id = %self.thread_id,
                "skipping goal continuation because model-facing goal tools are unavailable"
            );
            return Ok(false);
        }
        // 1. goal_state_lock（与用户 set/clear 窗口互斥）
        let Some(_permit) = self.state_lock.acquire().await else {
            return Ok(false);
        };
        // 2. 有 deferral → 本轮不续跑
        if self
            .store
            .has_continuation_deferral(&self.thread_id)
            .await?
        {
            metrics::global().record_continuation_deferred(&self.thread_id);
            return Ok(false);
        }
        // 3. 读表：无 goal / 非 active → 不续跑（budget_limited 等终态在此拦截）
        let Some(goal) = self.store.read(&self.thread_id).await? else {
            return Ok(false);
        };
        if goal.status != GoalStatus::Active {
            return Ok(false);
        }
        // 4. 渲染 continuation → 幂等二道门（文件化 goal 先还原全文）
        let goal = self.store.resolve_objective(goal).await;
        // C2：mid-turn 用户 edit（DB revision > 本 turn 已见 revision）→
        // 渲染 objective_updated steering 显式告知模型目标已变更（死代码转正）。
        let seen = self
            .last_seen_revision
            .load(std::sync::atomic::Ordering::SeqCst);
        let objective_changed = goal.objective_revision > seen;
        let (text, reason) = if objective_changed {
            (
                steering::objective_updated(&goal),
                GoalTurnReason::ObjectiveUpdated,
            )
        } else {
            (
                steering::continuation(&goal, None),
                GoalTurnReason::ActiveGoal,
            )
        };
        // C1：goal 驱动 turn 迭代 +1（codex turn_trigger 等价物）。
        let iteration = goal.iteration_count + 1;
        let started = self
            .driver
            .start_goal_turn(
                &self.thread_id,
                &text,
                GoalTurnMeta {
                    goal_id: goal.goal_id.clone(),
                    iteration,
                    reason,
                },
            )
            .await
            .unwrap_or(false);
        if started {
            metrics::global().record_continuation_started(&self.thread_id);
            if let Ok(mut updated) = self
                .store
                .set_iteration(&self.thread_id, &goal.goal_id, iteration)
                .await
            {
                // `goal` 已还原过文件化 objective；保留全文，避免即时快照
                // 暴露内部 @file 标记。
                updated.objective = goal.objective.clone();
                self.notify_status_change(updated).await;
            }
            if objective_changed {
                self.last_seen_revision
                    .store(goal.objective_revision, std::sync::atomic::Ordering::SeqCst);
            }
        }
        Ok(started)
    }

    /// fork / 外部 mutation 保护：写入 deferral（§6.6）。下次 `on_turn_start`
    /// 或 `continue_if_idle` 视其存在而跳过一次续跑。
    pub async fn defer_continuation(&self) -> Result<bool, GoalStoreError> {
        self.store.defer_continuation(&self.thread_id).await
    }

    /// 保护窗结束（session fork 完成 / 保护性 mutation 收尾）：清除 deferral
    /// 并返回是否确有 deferral 被清（宿主可据此立刻 [`Self::continue_if_idle`] 恢复）。
    pub async fn clear_deferral(&self) -> Result<bool, GoalStoreError> {
        self.store
            .clear_continuation_deferral(&self.thread_id)
            .await
    }

    /// 用户 set/clear 之后由宿主调用：基线重置。
    /// `goal_id = None` 表示 goal 已被 clear。
    pub async fn on_goal_replaced(&self, goal_id: Option<&str>, totals: TokenTotals) {
        match goal_id {
            Some(gid) => self.accounting.reset_baselines(gid, totals).await,
            None => self.accounting.clear_baselines().await,
        }
    }

    /// goal 被武装（create_goal / 用户 set）后调用：基线重置到该 goal 的
    /// 武装点（沿用最近已知累计用量，arm 之前的历史用量不计入新 goal）。
    pub async fn note_goal_armed(&self, goal_id: &str) {
        let totals = self.accounting.last_totals().await;
        self.accounting.reset_baselines(goal_id, totals).await;
    }

    /// 模型在 turn 中途创建 goal 时，以工具调用前的实时 usage 作为武装点，
    /// 防止把创建 goal 之前的 token 计入新目标。
    pub async fn note_goal_armed_at(&self, goal_id: &str, totals: TokenTotals) {
        self.accounting.reset_baselines(goal_id, totals).await;
    }

    /// 用户 pause / resume 之后由宿主调用：flush（pause）或重启（resume）墙钟。
    pub async fn on_goal_status_changed(
        &self,
        new_status: GoalStatus,
    ) -> Result<(), GoalStoreError> {
        if new_status == GoalStatus::Active {
            self.accounting.start_wall_clock_if_active().await?;
        } else {
            // 离开 active：绑定随之失效（pause 后错误 turn 不得 block）。
            self.take_turn_binding().await;
            // 补记已计时的 active 段并清基线
            self.accounting
                .flush_wall_clock(AccountingMode::ActiveOrStopped)
                .await?;
        }
        Ok(())
    }

    /// C3：豁免 turn（plan 等协作模式）——绑定 `account=false`：本 turn 零
    /// 记账、报错不 block、不计 exec 连击、不迭代；goal 工具可见性不变
    /// （对齐 codex `start_turn(collaboration_mode)` 的 Plan 分支）。
    pub async fn on_turn_start_exempt(&self, turn_token: &str) -> Result<(), GoalStoreError> {
        self.on_turn_start().await?;
        if let Ok(Some(goal)) = self.store.read(&self.thread_id).await {
            if goal.status == GoalStatus::Active {
                self.last_seen_revision
                    .store(goal.objective_revision, std::sync::atomic::Ordering::SeqCst);
                *self.turn_binding.lock().await = Some(TurnBinding {
                    turn_token: turn_token.to_string(),
                    goal_id: goal.goal_id,
                    account: false,
                });
            }
        }
        Ok(())
    }

    async fn inject_budget_steering_if_flipped(
        &self,
        outcome: crate::types::AccountingOutcome,
    ) -> Result<Option<String>, GoalStoreError> {
        let Some(text) = self
            .accounting
            .take_budget_steering_if_flipped(&outcome)
            .await?
        else {
            return Ok(None);
        };
        metrics::global().record_budget_limited(&self.thread_id);
        // B1：budget 翻转同样即时通知（快照字段与 DB 一致）。
        let goal_now = self.store.read(&self.thread_id).await.ok().flatten();
        if let Some(goal) = &goal_now {
            self.notify_status_change(goal.clone()).await;
        }
        // R1 降级：不打断当前 turn（已结束），经二道门注入一次收尾 turn；
        // C1：wrap-up 同为 goal 驱动 turn（reason=budget-limit，迭代 +1）。
        if let Some(goal) = &goal_now {
            let iteration = goal.iteration_count + 1;
            let started = self
                .driver
                .start_goal_turn(
                    &self.thread_id,
                    &text,
                    GoalTurnMeta {
                        goal_id: goal.goal_id.clone(),
                        iteration,
                        reason: GoalTurnReason::BudgetLimit,
                    },
                )
                .await
                .unwrap_or(false);
            if started {
                let _ = self
                    .store
                    .set_iteration(&self.thread_id, &goal.goal_id, iteration)
                    .await;
            }
        } else {
            let _ = self.driver.start_turn_if_idle(&self.thread_id, &text).await;
        }
        Ok(Some(text))
    }
}

// 让 service 错误在 runtime 层可被宿主统一处理
impl From<GoalServiceError> for GoalStoreError {
    fn from(e: GoalServiceError) -> Self {
        match e {
            GoalServiceError::Store(inner) => inner,
            GoalServiceError::LockTimeout => GoalStoreError::LockTimeout,
            other => GoalStoreError::CorruptStatus(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::CreateGoalRequest;
    use tokio::sync::Mutex;

    struct MockDriver {
        running: Mutex<bool>,
        allow: Mutex<bool>,
        started: Mutex<Vec<String>>,
        /// 持续阻塞 start_turn（用于锁窗口测试）
        gate: Mutex<bool>,
    }

    impl MockDriver {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                running: Mutex::new(false),
                allow: Mutex::new(true),
                started: Mutex::new(Vec::new()),
                gate: Mutex::new(false),
            })
        }
    }

    #[async_trait]
    impl TurnDriver for MockDriver {
        async fn start_turn_if_idle(&self, thread_id: &str, message: &str) -> Result<bool, String> {
            let gate = *self.gate.lock().await;
            if gate {
                // 模拟宿主二道门耗时（不启动），仅供锁窗口测试观察顺序
                return Ok(false);
            }
            let mut running = self.running.lock().await;
            if !*self.allow.lock().await || *running {
                return Ok(false);
            }
            *running = true;
            self.started
                .lock()
                .await
                .push(format!("{thread_id}|{message}"));
            Ok(true)
        }
    }

    async fn setup(
        budget: Option<i64>,
    ) -> (
        tempfile::TempDir,
        Arc<GoalRuntimeHandle>,
        Arc<MockDriver>,
        GoalStore,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .expect("open TaskDb");
        let store = GoalStore::from_task_db(&db);
        let driver = MockDriver::new();
        let handle = GoalRuntimeHandle::new(store.clone(), "t1", driver.clone());
        // 始终创建 goal（budget 参数仅控制预算）
        let g = handle
            .service()
            .set("t1", "objective", budget)
            .await
            .expect("set");
        handle
            .on_goal_replaced(Some(&g.goal_id), TokenTotals::default())
            .await;
        (dir, handle, driver, store)
    }

    /// §6.6：多 idle 事件只启动一次续跑 turn（幂等二道门）。
    #[tokio::test]
    async fn idle_continuation_idempotent() {
        let (_d, handle, driver, _store) = setup(None).await;

        let a = handle.continue_if_idle().await.expect("idle1");
        let b = handle.continue_if_idle().await.expect("idle2");
        assert!(a);
        assert!(!b, "运行中不得重复启动");
        let started = driver.started.lock().await;
        assert_eq!(started.len(), 1);
        assert!(started[0].starts_with("t1|Continue working"));
    }

    #[tokio::test]
    async fn idle_continuation_fails_closed_when_goal_tools_are_missing() {
        let (_d, handle, driver, _store) = setup(None).await;

        handle.record_goal_tools_visibility(false);
        assert!(!handle.continue_if_idle().await.expect("missing tools"));
        assert!(driver.started.lock().await.is_empty());

        handle.record_goal_tools_visibility(true);
        assert!(handle.continue_if_idle().await.expect("tools restored"));
        assert_eq!(driver.started.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn continuation_publishes_incremented_iteration_snapshot() {
        let (_d, handle, _driver, _store) = setup(None).await;
        let iterations = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed = iterations.clone();
        handle.set_status_notifier(Arc::new(move |goal| {
            observed
                .lock()
                .expect("iteration observer poisoned")
                .push(goal.iteration_count);
        }));

        assert!(handle.continue_if_idle().await.expect("continue"));
        assert_eq!(
            iterations
                .lock()
                .expect("iteration observer poisoned")
                .as_slice(),
            &[1]
        );
    }

    /// §6.6：paused / 无 goal / 有 deferral → 不续跑。
    #[tokio::test]
    async fn idle_skips_paused_cleared_deferred() {
        let (_d, handle, driver, _store) = setup(None).await;

        handle.service().pause("t1").await.expect("pause");
        assert!(!handle.continue_if_idle().await.expect("idle paused"));

        handle.service().resume("t1").await.expect("resume");
        handle.defer_continuation().await.expect("defer");
        assert!(!handle.continue_if_idle().await.expect("idle deferred"));
        // on_turn_start 清 deferral 后恢复
        handle.on_turn_start().await.expect("turn start");
        assert!(handle.continue_if_idle().await.expect("idle after start"));

        handle.service().clear("t1").await.expect("clear");
        handle.on_goal_replaced(None, TokenTotals::default()).await;
        assert!(!handle.continue_if_idle().await.expect("idle cleared"));
        // 仅第 4 步（on_turn_start 清 deferral 后）启动过一次续跑
        let started = driver.started.lock().await;
        assert_eq!(started.len(), 1);
        assert!(started[0].contains("Continue working"));
    }

    /// §5：goal_state_lock 使 continue_if_idle 与用户 set 互斥。
    #[tokio::test]
    async fn state_lock_serializes_idle_and_user_set() {
        let (_d, handle, _driver, _store) = setup(None).await;

        // 持锁模拟 continue_if_idle 的读→start 窗口
        let permit = handle.state_lock().acquire().await.expect("acquire");
        let svc = handle.service().clone();
        let setter = tokio::spawn(async move { svc.set("t1", "during window", None).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!setter.is_finished(), "持锁期间用户 set 必须等待");

        drop(permit);
        let g = tokio::time::timeout(std::time::Duration::from_secs(2), setter)
            .await
            .expect("set should complete after release")
            .expect("join")
            .expect("set ok");
        assert_eq!(g.objective, "during window");
    }

    /// §6.1：turn 不可恢复错误 → blocked + ActiveOrStopped 补账。
    #[tokio::test]
    async fn turn_error_marks_blocked_and_accounts() {
        let (_d, handle, _driver, store) = setup(Some(100_000)).await;

        handle.on_turn_start().await.expect("start");
        let steering = handle
            .on_turn_error(
                "boom",
                Some(TokenTotals {
                    input_tokens: 500,
                    output_tokens: 0,
                    cached_tokens: 0,
                }),
            )
            .await
            .expect("error hook");
        assert_eq!(steering, None);
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.status, GoalStatus::Blocked);
        assert_eq!(g.tokens_used, 500);
        assert_eq!(g.status_reason.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn consecutive_turns_account_each_prompt_from_zero() {
        let (_d, handle, _driver, store) = setup(Some(100_000)).await;

        handle.on_turn_start().await.expect("turn 1 start");
        handle
            .on_turn_finish(Some(TokenTotals {
                input_tokens: 100,
                output_tokens: 0,
                cached_tokens: 0,
            }))
            .await
            .expect("turn 1 finish");

        handle.on_turn_start().await.expect("turn 2 start");
        handle
            .on_turn_finish(Some(TokenTotals {
                input_tokens: 80,
                output_tokens: 0,
                cached_tokens: 0,
            }))
            .await
            .expect("turn 2 finish");

        let goal = store.read("t1").await.expect("read").expect("goal");
        assert_eq!(goal.tokens_used, 180, "每个 ACP prompt 的 usage 都必须计入");
    }

    #[tokio::test]
    async fn quota_error_accounts_before_marking_limited() {
        let (_d, handle, _driver, store) = setup(Some(100_000)).await;
        handle.on_turn_start().await.expect("start");
        handle
            .on_provider_quota_exhausted_with_usage(
                "quota exhausted",
                Some(TokenTotals {
                    input_tokens: 75,
                    output_tokens: 25,
                    cached_tokens: 0,
                }),
            )
            .await
            .expect("quota hook");

        let goal = store.read("t1").await.expect("read").expect("goal");
        assert_eq!(goal.tokens_used, 100);
        assert_eq!(goal.status, GoalStatus::UsageLimited);
    }

    /// §6.5：预算越界四断言——DB 翻转 / 收尾 turn 注入一次 / 后续 idle 不续跑。
    #[tokio::test]
    async fn budget_overrun_injects_wrapup_once() {
        let (_d, handle, driver, store) = setup(Some(100)).await;

        handle.on_turn_start().await.expect("start");
        let s1 = handle
            .on_turn_finish(Some(TokenTotals {
                input_tokens: 150,
                output_tokens: 0,
                cached_tokens: 0,
            }))
            .await
            .expect("finish");
        assert!(s1.is_some(), "首次翻转必须注入收尾 steering");
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.status, GoalStatus::BudgetLimited);

        // 后续补账不再注入
        let s2 = handle
            .on_turn_finish(Some(TokenTotals {
                input_tokens: 160,
                output_tokens: 0,
                cached_tokens: 0,
            }))
            .await
            .expect("finish2");
        assert_eq!(s2, None, "budget_limit_reported_goal_id 去重失效");

        // 终态下 idle 不续跑
        assert!(!handle.continue_if_idle().await.expect("idle"));
        let started = driver.started.lock().await;
        assert_eq!(started.len(), 1);
        assert!(started[0].contains("budget_limited"));
    }

    /// A3（gap-remediation G6）：abort 跨预算——只补账落库 budget_limited，
    /// **不注入** wrap-up turn（对齐 codex on_turn_abort）。
    #[tokio::test]
    async fn abort_crossing_budget_limits_but_starts_no_turn() {
        let (_d, handle, driver, store) = setup(Some(100)).await;

        handle.on_turn_start().await.expect("start");
        handle
            .on_turn_abort(Some(TokenTotals {
                input_tokens: 150,
                output_tokens: 0,
                cached_tokens: 0,
            }))
            .await
            .expect("abort");

        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.status, GoalStatus::BudgetLimited, "越界仍须落库");
        assert_eq!(g.tokens_used, 150);
        assert!(
            driver.started.lock().await.is_empty(),
            "abort 后不得自动起任何 turn"
        );
        // 预算去重标记不得被 abort 消费：resume（B2 后 budget_limited 亦可）
        // 后首次越界仍能正常注入一次。
        let s = handle
            .on_turn_finish(Some(TokenTotals {
                input_tokens: 160,
                output_tokens: 0,
                cached_tokens: 0,
            }))
            .await
            .expect("finish");
        assert!(
            s.is_some(),
            "后续 finish 的首次越界仍须注入一次收尾 steering"
        );
    }

    /// §6.5：quota 耗尽 → usage_limited（系统置位）。
    #[tokio::test]
    async fn quota_exhausted_marks_usage_limited() {
        let (_d, handle, _driver, store) = setup(None).await;
        handle
            .on_provider_quota_exhausted("provider 429")
            .await
            .expect("quota");
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.status, GoalStatus::UsageLimited);
        assert_eq!(g.status_reason.as_deref(), Some("provider 429"));
        // usage_limited 可由用户恢复
        assert!(handle.service().resume("t1").await.is_ok());
    }

    /// B2（G3）：budget_limited → usage_limited 可覆盖；budget_limited 可
    /// resume；update_budget 保 goal_id/tokens_used 且 complete 拒绝。
    #[tokio::test]
    async fn b2_budget_limited_coverage_resume_and_update_budget() {
        let (_d, handle, _driver, store) = setup(Some(100)).await;
        let goal_id = store.read("t1").await.unwrap().unwrap().goal_id;

        // 1) 预算触顶 → budget_limited，随后 quota → usage_limited（覆盖）
        handle
            .on_turn_finish(Some(TokenTotals {
                input_tokens: 150,
                output_tokens: 0,
                cached_tokens: 0,
            }))
            .await
            .unwrap();
        assert_eq!(
            store.read("t1").await.unwrap().unwrap().status,
            GoalStatus::BudgetLimited
        );
        handle
            .on_provider_quota_exhausted_with_usage("quota exhausted", None)
            .await
            .unwrap();
        assert_eq!(
            store.read("t1").await.unwrap().unwrap().status,
            GoalStatus::UsageLimited
        );

        // 2) usage_limited → resume（B2：原有语义回归）
        handle.service().resume("t1").await.unwrap();
        // 基线停在 150：本次 totals=200 → delta=50 → 累计 200 ≥ 100 → 再翻转
        handle
            .on_turn_finish(Some(TokenTotals {
                input_tokens: 200,
                output_tokens: 0,
                cached_tokens: 0,
            }))
            .await
            .unwrap();
        let g = store.read("t1").await.unwrap().unwrap();
        assert_eq!(g.status, GoalStatus::BudgetLimited);

        // update_budget：budget_limited 下可提额，保 goal_id/tokens_used
        let updated = handle.service().update_budget("t1", 10_000).await.unwrap();
        assert_eq!(updated.goal_id, goal_id, "goal_id 不得变化");
        assert_eq!(updated.status, GoalStatus::BudgetLimited, "提额不改状态");
        assert_eq!(updated.tokens_used, 200);
        assert_eq!(updated.token_budget, Some(10_000));

        handle.service().resume("t1").await.unwrap();
        assert_eq!(
            store.read("t1").await.unwrap().unwrap().status,
            GoalStatus::Active
        );

        // complete 拒绝提额；非法预算拒绝
        store.mark_complete("t1", &goal_id).await.unwrap();
        assert!(handle.service().update_budget("t1", 999).await.is_err());
        assert!(handle.service().update_budget("t1", 0).await.is_err());
    }

    /// C1（G9）：goal 驱动 turn 迭代计数（每次续跑 +1）。
    #[tokio::test]
    async fn c1_continuation_increments_iteration_count() {
        let (_d, handle, driver, store) = setup(None).await;
        handle.on_turn_start_for("tt-c1").await.expect("start");
        handle.on_turn_finish(None).await.expect("finish");
        handle.continue_if_idle().await.expect("continue");
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.iteration_count, 1);
        assert!(driver.started.lock().await[0].contains("Continue working"));
    }

    /// C2（G8）：mid-turn edit → 下一续跑渲染 objective_updated steering
    /// （死代码转正）；revision 消费后回到常规 continuation。
    #[tokio::test]
    async fn c2_midturn_edit_renders_objective_updated_steering() {
        let (_d, handle, driver, store) = setup(None).await;
        handle.on_turn_start_for("tt-c2").await.expect("start");
        // 用户在 turn 运行中改写目标
        handle
            .service()
            .edit("t1", "revised objective")
            .await
            .expect("edit");
        handle.on_turn_finish(None).await.expect("finish");
        handle.continue_if_idle().await.expect("continue");
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.objective_revision, 1);
        assert_eq!(g.iteration_count, 1);
        let msg = driver.started.lock().await[0].clone();
        assert!(msg.contains("revised objective"), "应渲染新目标: {msg}");
        assert!(
            !msg.contains("Continue working toward"),
            "不应是常规 continuation: {msg}"
        );
        // revision 已消费：重置二道门后再续跑 → 回到常规 continuation
        *driver.running.lock().await = false;
        handle.continue_if_idle().await.expect("continue2");
        let msg2 = driver.started.lock().await[1].clone();
        assert!(msg2.contains("Continue working toward"));
        assert_eq!(store.read("t1").await.unwrap().unwrap().iteration_count, 2);
    }

    /// C3（G10）：豁免 turn（plan）报错不 block goal、用量不落账。
    #[tokio::test]
    async fn c3_exempt_turn_errors_do_not_block_goal() {
        let (_d, handle, _driver, store) = setup(None).await;
        handle.on_turn_start_exempt("tt-c3").await.expect("start");
        handle.on_turn_error("boom", None).await.expect("error");
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(
            g.status,
            GoalStatus::Active,
            "plan turn 报错不得 block goal"
        );
        assert_eq!(g.tokens_used, 0, "豁免 turn 用量不落账");
    }

    /// B1（G2）：turn 绑定存在但 goal 已被替换——错误 turn 不得误伤新 goal。
    #[tokio::test]
    async fn turn_error_does_not_block_replacement_goal() {
        let (_d, handle, _driver, store) = setup(None).await;

        handle.on_turn_start_for("tt-1").await.expect("start");
        // turn 运行中用户替换 goal（新 goal_id）
        let replaced = handle
            .service()
            .set("t1", "replaced objective", None)
            .await
            .expect("replace set");
        handle
            .on_goal_replaced(Some(&replaced.goal_id), TokenTotals::default())
            .await;

        handle
            .on_turn_error("boom", None)
            .await
            .expect("error hook");
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.goal_id, replaced.goal_id);
        assert_eq!(
            g.status,
            GoalStatus::Active,
            "新 goal 不得被旧 turn 的错误 block"
        );
        assert_eq!(g.objective, "replaced objective");
    }

    /// B1：turn 绑定后用户 pause——错误 turn 不得把 paused 改成 blocked。
    #[tokio::test]
    async fn turn_error_does_not_override_paused_status() {
        let (_d, handle, _driver, store) = setup(None).await;

        handle.on_turn_start_for("tt-2").await.expect("start");
        handle.service().pause("t1").await.expect("pause");
        // pause 走 on_goal_status_changed 已清绑定；此处显式走 error 路径
        handle
            .on_turn_error("boom", None)
            .await
            .expect("error hook");
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.status, GoalStatus::Paused);
    }

    /// B1：系统置位 blocked 后即时触发 status_notifier（快照不再等 turn 尾）。
    #[tokio::test]
    async fn status_notifier_fires_on_system_blocked() {
        let (_d, handle, _driver, store) = setup(None).await;
        let notified: std::sync::Arc<std::sync::Mutex<Vec<Goal>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = notified.clone();
        handle.set_status_notifier(std::sync::Arc::new(move |goal| {
            sink.lock().expect("sink").push(goal);
        }));

        handle.on_turn_start_for("tt-3").await.expect("start");
        handle
            .on_turn_error("boom", None)
            .await
            .expect("error hook");

        let count_and_status = {
            let got = notified.lock().expect("sink");
            (got.len(), got.first().map(|g| g.status))
        };
        assert_eq!(count_and_status, (1, Some(GoalStatus::Blocked)));
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.status, GoalStatus::Blocked);
    }

    /// B3（G7）：连续 3 个「失败 exec 且无成功工具」的 goal turn →
    /// on_turn_finish 置 ExecutionUnavailable blocked。
    #[tokio::test]
    async fn three_consecutive_failed_exec_turns_block_goal() {
        let (_d, handle, _driver, store) = setup(None).await;
        for i in 0..3 {
            handle
                .on_turn_start_for(&format!("tt-{i}"))
                .await
                .expect("start");
            handle.record_tool_outcome("bash", true);
            handle.on_turn_finish(None).await.expect("finish");
            let g = store.read("t1").await.expect("read").expect("exists");
            let expected = if i < 2 {
                GoalStatus::Active
            } else {
                GoalStatus::Blocked
            };
            assert_eq!(g.status, expected, "turn {i}");
        }
        let g = store.read("t1").await.expect("read").expect("exists");
        assert!(g
            .status_reason
            .as_deref()
            .unwrap_or("")
            .contains("execution"));
    }

    /// B3：任一成功工具清零连击——混合成功不误伤。
    #[tokio::test]
    async fn successful_tool_resets_execution_failure_streak() {
        let (_d, handle, _driver, store) = setup(None).await;
        for i in 0..2 {
            handle
                .on_turn_start_for(&format!("tt-a{i}"))
                .await
                .expect("start");
            handle.record_tool_outcome("bash", true);
            handle.on_turn_finish(None).await.expect("finish");
        }
        // 成功 turn：read 成功 + bash 失败 → 豁免且清零
        handle.on_turn_start_for("tt-b").await.expect("start");
        handle.record_tool_outcome("read", false);
        handle.record_tool_outcome("bash", true);
        handle.on_turn_finish(None).await.expect("finish");
        // 重新计 2 个仍不触发
        for i in 0..2 {
            handle
                .on_turn_start_for(&format!("tt-c{i}"))
                .await
                .expect("start");
            handle.record_tool_outcome("bash", true);
            handle.on_turn_finish(None).await.expect("finish");
            let g = store.read("t1").await.expect("read").expect("exists");
            assert_eq!(g.status, GoalStatus::Active);
        }
    }

    /// P1 create 的前置终态检查路径（模型/系统 create 不能覆盖 unfinished）。
    #[tokio::test]
    async fn model_create_cannot_overwrite_unfinished() {
        let (_d, handle, _driver, store) = setup(None).await;
        let err = store
            .create(
                &CreateGoalRequest {
                    thread_id: "t1".into(),
                    objective: "second".into(),
                    token_budget: None,
                    verify_command: None,
                },
                "g-model",
            )
            .await
            .expect_err("must reject");
        assert!(matches!(err, GoalStoreError::ExistingUnfinishedGoal(_)));
        let _ = handle;
    }
}

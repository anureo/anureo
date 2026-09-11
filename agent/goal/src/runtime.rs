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
use crate::types::{AccountingMode, GoalStatus};

/// 宿主提供的 turn 驱动。`start_turn_if_idle` 是**幂等二道门**（§6.6）：
/// 仅当宿主确认该 session 当前无运行中 turn 时才真正启动；返回是否启动。
#[async_trait]
pub trait TurnDriver: Send + Sync {
    async fn start_turn_if_idle(&self, thread_id: &str, message: &str) -> Result<bool, String>;
}

pub struct GoalRuntimeHandle {
    thread_id: String,
    store: GoalStore,
    service: GoalService,
    accounting: Arc<GoalAccounting>,
    state_lock: GoalStateLock,
    driver: Arc<dyn TurnDriver>,
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
        })
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

    /// on_turn_start：清 deferral（§6.6）+ 墙钟起表（仅 active）。
    pub async fn on_turn_start(&self) -> Result<(), GoalStoreError> {
        self.store
            .clear_continuation_deferral(&self.thread_id)
            .await?;
        self.accounting.start_wall_clock_if_active().await?;
        Ok(())
    }

    /// on_turn_finish（正常结束）：补账 + 预算越界时经二道门注入一次收尾 turn。
    /// 返回 `Some(text)` 仅当本次触发了 budget steering 注入。
    pub async fn on_turn_finish(
        &self,
        totals: Option<TokenTotals>,
    ) -> Result<Option<String>, GoalStoreError> {
        let outcome = self.accounting.finish_turn(totals).await?;
        self.inject_budget_steering_if_flipped(outcome).await
    }

    /// on_turn_abort（用户取消）：补账，不改状态。
    pub async fn on_turn_abort(
        &self,
        totals: Option<TokenTotals>,
    ) -> Result<Option<String>, GoalStoreError> {
        let outcome = self.accounting.stop_abnormal(totals).await?;
        self.inject_budget_steering_if_flipped(outcome).await
    }

    /// on_turn_error：补账 + active → `blocked`（§6.1；无 goal / 非 active 时
    /// blocked 置位 no-op）。若补账先翻转了 budget_limited，则 blocked 不再
    /// 覆盖（store 层 `WHERE status='active'` 保证）。
    pub async fn on_turn_error(
        &self,
        reason: &str,
        totals: Option<TokenTotals>,
    ) -> Result<Option<String>, GoalStoreError> {
        let outcome = self.accounting.stop_abnormal(totals).await?;
        if let Ok(Some(goal)) = self.store.read(&self.thread_id).await {
            if self
                .store
                .mark_blocked(&self.thread_id, &goal.goal_id, reason)
                .await
                .is_ok()
            {
                metrics::global().record_blocked(&self.thread_id);
            }
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
        let _ = self.accounting.stop_abnormal(totals).await?;
        if let Ok(Some(goal)) = self.store.read(&self.thread_id).await {
            if self
                .store
                .mark_usage_limited(&self.thread_id, &goal.goal_id, reason)
                .await
                .is_ok()
            {
                metrics::global().record_usage_limited(&self.thread_id);
            }
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
        let text = steering::continuation(&goal, None);
        let started = self
            .driver
            .start_turn_if_idle(&self.thread_id, &text)
            .await
            .unwrap_or(false);
        if started {
            metrics::global().record_continuation_started(&self.thread_id);
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
            self.accounting.start_wall_clock_if_active().await
        } else {
            // 补记已计时的 active 段并清基线
            self.accounting
                .flush_wall_clock(AccountingMode::ActiveOrStopped)
                .await?;
            Ok(())
        }
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
        // R1 降级：不打断当前 turn（已结束），经二道门注入一次收尾 turn
        let _ = self.driver.start_turn_if_idle(&self.thread_id, &text).await;
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

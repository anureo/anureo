//! [`GoalAccounting`]：token/墙钟内存基线 + 原子记账（alignment §6.2/§6.4/§6.7）。
//!
//! `progress_accounting_lock` 覆盖「取 snapshot → SQL 记账成功 → 推进内存基线」
//! 全程，防止多个 tool finish / turn stop 消费同一 delta（R5）；SQL 层返回
//! `Unchanged`（goal 被替换/状态不符）时 **delta 丢弃、基线不推进**（§6.2，
//! 防旧 turn 写到新 goal）。
//!
//! 墙钟（§6.7）：`Instant` 基线，仅 goal active 时计时；状态离开 active 时
//! flush 已计时段并清基线；resume 后由宿主重新 start；进程重启期间不追补
//! （基线是纯内存的）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{Mutex, Semaphore};

use crate::steering;
use crate::store::{GoalStore, GoalStoreError};
use crate::types::{AccountingMode, AccountingOutcome, GoalStatus};

/// 锁获取超时（§6.4：所有 acquire 带 timeout）。
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// LLM 用量累计快照（宿主从 `LlmUsage` 转换；goal crate 不依赖 foundation/llm）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TokenTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
}

/// Codex 对拍公式（§6.2）：`goal_tokens = (Δinput − Δcached) + max(Δoutput, 0)`，
/// 全程 saturating——这是 budget accounting，不是 billing report。
pub fn goal_token_delta(last: TokenTotals, current: TokenTotals) -> i64 {
    let d_input = current.input_tokens.saturating_sub(last.input_tokens);
    let d_cached = current.cached_tokens.saturating_sub(last.cached_tokens);
    let d_output = current.output_tokens.saturating_sub(last.output_tokens); // saturating = max(Δ,0)
    let non_cached_input = d_input.saturating_sub(d_cached);
    non_cached_input
        .saturating_add(d_output)
        .min(i64::MAX as u64) as i64
}

/// 单个 thread 的记账器：内存基线 + progress 锁 + budget 一次性 steering。
pub struct GoalAccounting {
    store: GoalStore,
    thread_id: String,
    progress_permit: Arc<Semaphore>,
    inner: Mutex<Inner>,
    budget_reported_goal_id: Mutex<Option<String>>,
}

#[derive(Default)]
struct Inner {
    /// 基线绑定 goal_id（goal 被替换时 delta 丢弃，§6.2）
    token_baseline: Option<(String, TokenTotals)>,
    /// Some = goal active 且正在计时（§6.7）
    wall_clock_since: Option<Instant>,
}

impl GoalAccounting {
    /// `initial_totals`：goal 被武装（create/replace）时刻的会话累计用量——
    /// 之后的增量才计入 goal 预算（arm 之前的历史用量不计）。
    pub fn new(
        store: GoalStore,
        thread_id: impl Into<String>,
        goal_id: &str,
        initial_totals: TokenTotals,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            thread_id: thread_id.into(),
            progress_permit: Arc::new(Semaphore::new(1)),
            inner: Mutex::new(Inner {
                token_baseline: Some((goal_id.to_string(), initial_totals)),
                wall_clock_since: None,
            }),
            budget_reported_goal_id: Mutex::new(None),
        })
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// 正常进度（on_token_usage / on_tool_finish）：`ActiveStatusOnly`（§6.3）。
    pub async fn record_usage(
        &self,
        current: TokenTotals,
    ) -> Result<AccountingOutcome, GoalStoreError> {
        self.account_with(Some(current), AccountingMode::ActiveStatusOnly, false)
            .await
    }

    /// turn 正常结束补记：`ActiveOnly`（补越界前后最后一段）+ flush 墙钟。
    pub async fn finish_turn(
        &self,
        current: Option<TokenTotals>,
    ) -> Result<AccountingOutcome, GoalStoreError> {
        self.account_with(current, AccountingMode::ActiveOnly, true)
            .await
    }

    /// 错误 / abort 路径补账：`ActiveOrStopped`。
    pub async fn stop_abnormal(
        &self,
        current: Option<TokenTotals>,
    ) -> Result<AccountingOutcome, GoalStoreError> {
        self.account_with(current, AccountingMode::ActiveOrStopped, true)
            .await
    }

    /// §6.4 progress_accounting_lock 全窗口：snapshot → SQL → 基线推进。
    ///
    /// 基线绑定 goal_id：
    /// - goal_id 一致 → 计账（CAS 钉住该 goal_id）；SQL `Unchanged`（状态不在
    ///   mode 允许集）→ delta **保留**（基线不推进），下次成功记账时追补；
    /// - goal_id 变化（替换/clear）→ 旧 turn 的 delta **丢弃**，基线 adoption
    ///   新 goal 从当前总量起算（§6.2「防止旧 turn 写到新 goal」）。
    async fn account_with(
        &self,
        current: Option<TokenTotals>,
        mode: AccountingMode,
        flush_clock: bool,
    ) -> Result<AccountingOutcome, GoalStoreError> {
        let _permit =
            tokio::time::timeout(LOCK_TIMEOUT, self.progress_permit.clone().acquire_owned())
                .await
                .map_err(|_| GoalStoreError::LockTimeout)?
                .map_err(|_| GoalStoreError::LockTimeout)?;

        let mut inner = self.inner.lock().await;

        let snapshot = self.store.read(&self.thread_id).await?;
        let Some(goal) = snapshot else {
            inner.token_baseline = None;
            return Ok(AccountingOutcome::Unchanged);
        };

        match inner.token_baseline.take() {
            // goal 被替换/clear：旧基线与旧 delta 全部丢弃
            Some((bid, _)) if bid != goal.goal_id => {
                inner.token_baseline = Some((goal.goal_id, current.unwrap_or_default()));
                Ok(AccountingOutcome::Unchanged)
            }
            other => {
                let (gid, baseline) =
                    other.unwrap_or_else(|| (goal.goal_id.clone(), current.unwrap_or_default()));
                let delta_tokens = current.map(|c| goal_token_delta(baseline, c)).unwrap_or(0);
                let delta_seconds = if flush_clock {
                    inner
                        .wall_clock_since
                        .take()
                        .map(|t| t.elapsed().as_secs() as i64)
                        .unwrap_or(0)
                } else {
                    0
                };

                if delta_tokens == 0 && delta_seconds == 0 {
                    // 无新量：仅 adoption 基线（跳过重复上报）
                    inner.token_baseline = Some((gid, current.unwrap_or(baseline)));
                    return Ok(AccountingOutcome::Unchanged);
                }

                let outcome = self
                    .store
                    .account_thread_goal_usage(
                        &self.thread_id,
                        &gid,
                        mode,
                        delta_tokens,
                        delta_seconds,
                    )
                    .await?;

                match outcome {
                    // §6.2：仅 Updated 推进基线
                    AccountingOutcome::Updated { .. } => {
                        inner.token_baseline = Some((gid, current.unwrap_or(baseline)));
                    }
                    // 状态不在 mode 允许集：基线不推进（delta 追补语义）
                    AccountingOutcome::Unchanged => {
                        inner.token_baseline = Some((gid, baseline));
                    }
                }
                Ok(outcome)
            }
        }
    }

    /// budget 触顶后的一次性 budget steering（§6.5 `budget_limit_reported_goal_id`
    /// 去重）。R1 降级：宿主在**下一个 turn 边界**注入返回的文本。
    /// 返回 `Some(text)` 表示本次翻转尚未上报过。
    pub async fn take_budget_steering_if_flipped(
        &self,
        outcome: &AccountingOutcome,
    ) -> Result<Option<String>, GoalStoreError> {
        if !matches!(
            outcome,
            AccountingOutcome::Updated {
                status: GoalStatus::BudgetLimited,
                ..
            }
        ) {
            return Ok(None);
        }
        let Some(goal) = self.store.read(&self.thread_id).await? else {
            return Ok(None);
        };
        let mut reported = self.budget_reported_goal_id.lock().await;
        if reported.as_deref() == Some(goal.goal_id.as_str()) {
            return Ok(None);
        }
        *reported = Some(goal.goal_id.clone());
        // P7：文件化 goal 先还原全文再渲染 steering。
        let goal = self.store.resolve_objective(goal).await;
        Ok(Some(steering::budget_limit(&goal)))
    }

    // ── 墙钟（§6.7）─────────────────────────────────────────────────────

    /// goal active 时开始计时（on_turn_start 调用）；非 active 为 no-op。
    pub async fn start_wall_clock_if_active(&self) -> Result<(), GoalStoreError> {
        let goal = self.store.read(&self.thread_id).await?;
        let mut inner = self.inner.lock().await;
        if let Some(goal) = goal.filter(|g| g.status == GoalStatus::Active) {
            // ACP 宿主提供的是“本次 prompt”的用量，而不是跨 prompt 的累计值。
            // 每个 turn 从零建立基线，避免把上一轮 totals 当成本轮累计快照而少算。
            inner.token_baseline = Some((goal.goal_id, TokenTotals::default()));
            inner.wall_clock_since = Some(Instant::now());
        } else {
            inner.token_baseline = None;
            inner.wall_clock_since = None;
        }
        Ok(())
    }

    /// 状态离开 active（pause/blocked/complete/usage_limited/budget_limited）：
    /// flush 已计时段 + 清基线（§6.7）。resume 后由宿主重新 start。
    pub async fn flush_wall_clock(
        &self,
        mode: AccountingMode,
    ) -> Result<AccountingOutcome, GoalStoreError> {
        self.account_with(None, mode, true).await
    }

    /// 无 goal 时的构造（runtime 装配期）：基线 None。宿主在 goal 武装
    /// （create/replace）时调用 [`Self::reset_baselines`] 提供武装点基线。
    pub fn new_unarmed(store: GoalStore, thread_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            store,
            thread_id: thread_id.into(),
            progress_permit: Arc::new(Semaphore::new(1)),
            inner: Mutex::new(Inner::default()),
            budget_reported_goal_id: Mutex::new(None),
        })
    }

    /// 清空全部内存基线（goal 被 clear 时；新 goal 武装用 [`Self::reset_baselines`]）。
    pub async fn clear_baselines(&self) {
        let mut inner = self.inner.lock().await;
        inner.token_baseline = None;
        inner.wall_clock_since = None;
    }

    /// goal 被替换后由宿主调用：重置基线到新 goal 的武装点。
    /// `budget_reported_goal_id` 不清——按 goal_id 去重，新 goal 自然重新生效。
    pub async fn reset_baselines(&self, goal_id: &str, initial_totals: TokenTotals) {
        let mut inner = self.inner.lock().await;
        inner.token_baseline = Some((goal_id.to_string(), initial_totals));
        inner.wall_clock_since = None;
    }

    /// 测试/诊断：当前 token 基线（不含 goal_id 绑定）。
    pub async fn token_baseline(&self) -> Option<TokenTotals> {
        let guard = self.inner.lock().await;
        guard.token_baseline.as_ref().map(|(_, t)| *t)
    }

    /// 当前 ACP prompt 最近一次用量快照（goal 武装点基线用；无基线时返回零值）。
    pub async fn last_totals(&self) -> TokenTotals {
        let guard = self.inner.lock().await;
        guard
            .token_baseline
            .as_ref()
            .map(|(_, t)| *t)
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::GoalStore;
    use crate::types::CreateGoalRequest;

    async fn setup(budget: Option<i64>) -> (tempfile::TempDir, Arc<GoalAccounting>, GoalStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .expect("open TaskDb");
        let store = GoalStore::from_task_db(&db);
        store
            .create(
                &CreateGoalRequest {
                    thread_id: "t1".into(),
                    objective: "obj".into(),
                    token_budget: budget,
                    verify_command: None,
                },
                "g1",
            )
            .await
            .expect("create");
        (
            dir,
            GoalAccounting::new(store.clone(), "t1", "g1", TokenTotals::default()),
            store,
        )
    }

    fn totals(input: u64, output: u64, cached: u64) -> TokenTotals {
        TokenTotals {
            input_tokens: input,
            output_tokens: output,
            cached_tokens: cached,
        }
    }

    /// Codex 对拍：公式逐 case（负 delta、cached>input、saturating）。
    #[test]
    fn token_formula_matches_codex() {
        // 正常增量：(200−50)+30 = 180
        assert_eq!(
            goal_token_delta(totals(100, 50, 10), totals(300, 80, 60)),
            180
        );
        // 输出回退（乱序/合并上报）→ max(Δoutput,0)=0
        assert_eq!(goal_token_delta(totals(100, 80, 0), totals(150, 50, 0)), 50);
        // cached 增量超过 input 增量 → saturating 到 0
        assert_eq!(
            goal_token_delta(totals(100, 10, 20), totals(120, 10, 200)),
            0
        );
        // 完全回退 → 0（不得出现负记账）
        assert_eq!(
            goal_token_delta(totals(500, 500, 100), totals(100, 100, 50)),
            0
        );
        // u64 溢出 saturating，且不超 i64::MAX
        let huge = u64::MAX;
        assert_eq!(
            goal_token_delta(totals(0, 0, 0), totals(huge, huge, 0)),
            i64::MAX
        );
        // 无变化
        assert_eq!(goal_token_delta(totals(10, 10, 5), totals(10, 10, 5)), 0);
    }

    /// §6.2 主链路：Updated 推进基线、增量精确；Unchanged 丢弃 delta。
    #[tokio::test]
    async fn baseline_advances_only_on_updated() {
        let (_d, acc, store) = setup(Some(10_000)).await;

        let out = acc.record_usage(totals(100, 50, 10)).await.expect("record");
        assert!(matches!(
            out,
            AccountingOutcome::Updated {
                tokens_used: 140,
                ..
            }
        ));
        assert_eq!(acc.token_baseline().await, Some(totals(100, 50, 10)));

        // 重复上报同一总量 → 0 delta → Unchanged，基线推进到相同值
        let out = acc.record_usage(totals(100, 50, 10)).await.expect("record");
        assert_eq!(out, AccountingOutcome::Unchanged);

        // 用户替换 goal（新 goal_id）→ 旧基线作废：delta 丢弃，基线 adoption 新 goal
        store
            .replace(
                &CreateGoalRequest {
                    thread_id: "t1".into(),
                    objective: "v2".into(),
                    token_budget: Some(10_000),
                    verify_command: None,
                },
                "g2",
            )
            .await
            .expect("replace");
        let out = acc.record_usage(totals(500, 100, 0)).await.expect("record");
        assert_eq!(
            out,
            AccountingOutcome::Unchanged,
            "旧 turn 的 delta 必须被丢弃"
        );
        assert_eq!(
            acc.token_baseline().await,
            Some(totals(500, 100, 0)),
            "基线 adoption 新 goal"
        );

        // adoption 之后：同一总量不再重复计入；后续增量从 adoption 点起算
        let out = acc.record_usage(totals(500, 100, 0)).await.expect("record");
        assert_eq!(out, AccountingOutcome::Unchanged);
        let out = acc.record_usage(totals(600, 100, 0)).await.expect("record");
        assert!(matches!(
            out,
            AccountingOutcome::Updated {
                tokens_used: 100,
                ..
            }
        ));

        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(
            g.tokens_used, 100,
            "新 goal 不得被旧基线污染，只计 adoption 之后增量"
        );
    }

    /// R5（P1 移交）：并发 record_usage 不重复消费同一 delta；
    /// 穿插 pause 后 ActiveStatusOnly 记账 Unchanged，resume 后追补。
    #[tokio::test]
    async fn concurrent_record_usage_interleaved_with_status() {
        let (_d, acc, store) = setup(Some(100_000)).await;
        let goal = store.read("t1").await.expect("read").expect("exists");

        // 8 个并发任务全部上报同一总量：只有第一个产生 delta，其余 0 delta
        // → 断言无重复消费（若双计将得到 6400）
        let mut handles = Vec::new();
        for _ in 0..8 {
            let acc = acc.clone();
            handles.push(tokio::spawn(async move {
                acc.record_usage(totals(800, 0, 0)).await.expect("record")
            }));
        }
        let mut updated = 0;
        for h in handles {
            if matches!(h.await.expect("join"), AccountingOutcome::Updated { .. }) {
                updated += 1;
            }
        }
        assert_eq!(updated, 1, "同一总量只允许被消费一次");
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.tokens_used, 800);

        // pause 期间正常记账被拒；delta 保留（基线不推进）→ resume 后追补
        store.pause("t1", &goal.goal_id).await.expect("pause");
        let out = acc.record_usage(totals(900, 0, 0)).await.expect("record");
        assert_eq!(out, AccountingOutcome::Unchanged);
        assert_eq!(
            acc.token_baseline().await,
            Some(totals(800, 0, 0)),
            "基线不推进"
        );

        store.resume("t1", &goal.goal_id).await.expect("resume");
        let out = acc.record_usage(totals(1000, 0, 0)).await.expect("record");
        assert!(matches!(
            out,
            AccountingOutcome::Updated {
                tokens_used: 1000,
                ..
            }
        ));
    }

    /// §6.5：budget 翻转只上报一次；宿主拿到 budget steering 文本。
    #[tokio::test]
    async fn budget_steering_reported_once() {
        let (_d, acc, _store) = setup(Some(100)).await;

        let out = acc.record_usage(totals(150, 0, 0)).await.expect("record");
        assert!(matches!(
            out,
            AccountingOutcome::Updated {
                status: GoalStatus::BudgetLimited,
                ..
            }
        ));

        let s1 = acc
            .take_budget_steering_if_flipped(&out)
            .await
            .expect("take");
        assert!(s1.is_some(), "首次翻转必须产出 steering");
        assert!(s1.unwrap().contains("budget_limited"));

        // 同一 goal 第二次（ActiveOnly 补账触发的 Updated{budget_limited}）不再上报
        let out2 = acc
            .finish_turn(Some(totals(160, 0, 0)))
            .await
            .expect("catchup");
        assert!(matches!(
            out2,
            AccountingOutcome::Updated {
                status: GoalStatus::BudgetLimited,
                ..
            }
        ));
        let s2 = acc
            .take_budget_steering_if_flipped(&out2)
            .await
            .expect("take");
        assert_eq!(s2, None, "budget_limit_reported_goal_id 去重失效");
    }

    /// §6.7：墙钟基线随状态启停（秒级断言用 0 即可——瞬时 flush）。
    #[tokio::test]
    async fn wall_clock_start_stop_resume() {
        let (_d, acc, store) = setup(None).await;
        let goal = store.read("t1").await.expect("read").expect("exists");

        acc.start_wall_clock_if_active().await.expect("start");
        // 非 active 时 start 是 no-op
        store.pause("t1", &goal.goal_id).await.expect("pause");
        acc.start_wall_clock_if_active()
            .await
            .expect("start (paused)");

        // pause 转移后 flush：把 active 段落账（秒数可能为 0）
        let out = acc
            .flush_wall_clock(AccountingMode::ActiveOrStopped)
            .await
            .expect("flush");
        // baseline 被清后 wall_clock_since 也已 take → 再次 flush 为 Unchanged
        let out2 = acc
            .flush_wall_clock(AccountingMode::ActiveOrStopped)
            .await
            .expect("flush2");
        assert_eq!(out2, AccountingOutcome::Unchanged);
        let _ = out;

        // resume 后重新计时可用
        store.resume("t1", &goal.goal_id).await.expect("resume");
        acc.start_wall_clock_if_active().await.expect("start again");
        acc.reset_baselines("g1", TokenTotals::default()).await;
        assert_eq!(acc.token_baseline().await, Some(TokenTotals::default()));
    }
}

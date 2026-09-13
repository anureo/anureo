//! [`GoalStore`]：`thread_goals` CRUD + 原子记账（alignment §7.1 / §6.2 / §6.3）。
//!
//! 并发语义：
//! - CAS：全部写路径 `WHERE thread_id = ? AND goal_id = ?`；
//! - 记账另加 `AND status IN (<mode 允许集>)`（§6.3）；
//! - 0 行命中一律 `AccountingOutcome::Unchanged` / `NotFoundOrDisallowed`——
//!   调用方（Phase 2 accounting）对 `Unchanged` **丢弃 delta、不推进内存基线**。
//!
//! 单连接池（TaskDb `max_connections(1)`）+ 语句级原子性即满足 §6.2 的
//! 「read-modify-write 不落地」要求；交叉写路径（fork/替换）走显式事务。

use std::path::{Path, PathBuf};

use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::types::{AccountingMode, AccountingOutcome, CreateGoalRequest, Goal, GoalStatus};

pub const SELECT_GOAL_COLUMNS: &str = concat!(
    "thread_id, goal_id, objective, status, token_budget, tokens_used, ",
    "time_used_seconds, verify_command, status_reason, created_at_ms, updated_at_ms, ",
    "objective_revision, iteration_count"
);

#[derive(Debug, thiserror::Error)]
pub enum GoalStoreError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("validation: {0}")]
    Validation(#[from] crate::types::GoalValidationError),
    #[error("thread {0} already has an unfinished goal (model/system create cannot overwrite)")]
    ExistingUnfinishedGoal(String),
    #[error("no goal on thread {0}, or transition not allowed from current status")]
    NotFoundOrDisallowed(String),
    #[error("corrupt status in db: {0}")]
    CorruptStatus(String),
    #[error("internal lock acquisition timed out")]
    LockTimeout,
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn goal_from_row(row: &sqlx::sqlite::SqliteRow) -> sqlx::Result<Goal> {
    let status_str: String = row.try_get("status")?;
    let status = GoalStatus::parse(&status_str).ok_or_else(|| sqlx::Error::ColumnDecode {
        index: "status".to_string(),
        source: format!("unknown goal status: {status_str}").into(),
    })?;
    let objective: String = row.try_get("objective")?;
    Ok(Goal {
        thread_id: row.try_get("thread_id")?,
        goal_id: row.try_get("goal_id")?,
        objective_file: crate::objective_file::is_file_backed(&objective),
        objective,
        status,
        token_budget: row.try_get("token_budget")?,
        tokens_used: row.try_get("tokens_used")?,
        time_used_seconds: row.try_get("time_used_seconds")?,
        verify_command: row.try_get("verify_command")?,
        status_reason: row.try_get("status_reason")?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
        objective_revision: row.try_get("objective_revision")?,
        iteration_count: row.try_get("iteration_count")?,
    })
}

/// goal 存储：`thread_goals` + `thread_goal_continuation_deferrals` 的唯一读写方。
#[derive(Clone)]
pub struct GoalStore {
    pool: SqlitePool,
    /// objective 文件化目录（P7；`<home>/goals`）。`None` 时文件化 no-op
    /// （测试/嵌入式：DB 全文仍是唯一事实源）。
    goals_dir: Option<PathBuf>,
}

impl GoalStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            goals_dir: None,
        }
    }

    /// 复用 TaskDb 连接池（同库多池会有写锁竞争，必须共享）。
    /// 仅标准布局 `<home>/tasks/tasks.db` 推导 `goals_dir = <home>/goals`；
    /// 其他布局（tests 直接放 tempdir、嵌入式）→ `None`（文件化 no-op，
    /// 超长 objective 落回 DB 内联校验拒绝）。
    pub fn from_task_db(db: &task_core::TaskDb) -> Self {
        let goals_dir = db
            .path()
            .parent()
            .filter(|p| p.file_name() == Some(std::ffi::OsStr::new("tasks")))
            .and_then(Path::parent)
            .map(|home| home.join("goals"));
        Self {
            pool: db.pool().clone(),
            goals_dir,
        }
    }

    /// 显式覆盖 objective 文件化目录（P7）。
    pub fn with_goals_dir(mut self, dir: PathBuf) -> Self {
        self.goals_dir = Some(dir);
        self
    }

    /// objective 文件化目录（P7；未配置为 `None`）。
    pub fn goals_dir(&self) -> Option<&Path> {
        self.goals_dir.as_deref()
    }

    /// 文件化 goal 的文本还原：`@file:` 标记 → 读 `<goals_dir>/<name>` 全文。
    /// 非标记 / 无 goals_dir / 读文件失败时原样返回（降级，只记日志）。
    pub async fn resolve_objective(&self, mut goal: crate::types::Goal) -> crate::types::Goal {
        if !goal.objective_file {
            return goal;
        }
        let Some(dir) = &self.goals_dir else {
            return goal;
        };
        let Some(name) = crate::objective_file::marker_file_name(&goal.objective) else {
            return goal;
        };
        match tokio::fs::read_to_string(dir.join(name)).await {
            Ok(text) => goal.objective = text,
            Err(e) => {
                tracing::warn!(
                    thread_id = %goal.thread_id,
                    file = %name,
                    error = %e,
                    "objective file read failed; marker text leaked to consumer"
                );
            }
        }
        goal
    }

    // ── create / replace ────────────────────────────────────────────────

    /// 模型/系统 create（§6.8）：只能在无 goal 或旧 goal 已 complete 时创建。
    /// blocked / limited 都仍需用户处理，模型不得借 create 绕过控制面。
    pub async fn create(
        &self,
        req: &CreateGoalRequest,
        goal_id: &str,
    ) -> Result<Goal, GoalStoreError> {
        req.validate()?;
        let ts = now_ms();
        let sql = format!(
            "INSERT INTO thread_goals \
                 (thread_id, goal_id, objective, status, token_budget, tokens_used, \
                  time_used_seconds, verify_command, status_reason, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, 'active', ?4, 0, 0, ?5, NULL, ?6, ?6) \
             ON CONFLICT(thread_id) DO UPDATE SET \
                 goal_id = excluded.goal_id, objective = excluded.objective, \
                 status = excluded.status, token_budget = excluded.token_budget, \
                 tokens_used = 0, time_used_seconds = 0, \
                 verify_command = excluded.verify_command, status_reason = NULL, \
                 created_at_ms = excluded.created_at_ms, updated_at_ms = excluded.updated_at_ms, \
                 objective_revision = 0, iteration_count = 0 \
             WHERE thread_goals.status = 'complete' \
             RETURNING {SELECT_GOAL_COLUMNS}"
        );
        let row = sqlx::query(&sql)
            .bind(&req.thread_id)
            .bind(goal_id)
            .bind(req.trimmed_objective())
            .bind(req.token_budget)
            .bind(req.verify_command.clone())
            .bind(ts)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| goal_from_row(&row))
            .transpose()?
            .ok_or_else(|| GoalStoreError::ExistingUnfinishedGoal(req.thread_id.clone()))
    }

    /// 用户 set 替换入口（盲审 A3）：无「旧 goal 须终态」前置检查，允许对
    /// active/paused/blocked 等任何状态直接覆盖（§6.1「外部 set」）。
    pub async fn replace(
        &self,
        req: &CreateGoalRequest,
        goal_id: &str,
    ) -> Result<Goal, GoalStoreError> {
        req.validate()?;
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM thread_goals WHERE thread_id = ?1")
            .bind(&req.thread_id)
            .execute(&mut *tx)
            .await?;
        let goal = insert_new_tx(&mut tx, req, goal_id).await?;
        tx.commit().await?;
        Ok(goal)
    }

    // ── read ────────────────────────────────────────────────────────────

    pub async fn read(&self, thread_id: &str) -> Result<Option<Goal>, GoalStoreError> {
        let sql = format!("SELECT {SELECT_GOAL_COLUMNS} FROM thread_goals WHERE thread_id = ?1");
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| goal_from_row(&r)).transpose()?)
    }

    /// 全表投影（P5b `_anureo.dev/goal/list` 用）：`(thread_id, goal)` 列表，
    /// created_at 降序（与旧 goals.json 列表排序一致）。
    pub async fn list_all(&self) -> Result<Vec<(String, Goal)>, GoalStoreError> {
        let sql =
            format!("SELECT {SELECT_GOAL_COLUMNS} FROM thread_goals ORDER BY created_at_ms DESC");
        let rows = sqlx::query(&sql).fetch_all(&self.pool).await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let thread_id: String = row.try_get("thread_id")?;
            out.push((thread_id, goal_from_row(&row)?));
        }
        Ok(out)
    }

    /// 按 goal_id 反查（P5b `_anureo.dev/goal/get|pause|resume|cancel` 以旧
    /// goal-id 为键）：返回 `(thread_id, goal)`；goal_id 全局唯一。
    pub async fn find_by_goal_id(
        &self,
        goal_id: &str,
    ) -> Result<Option<(String, Goal)>, GoalStoreError> {
        let sql =
            format!("SELECT {SELECT_GOAL_COLUMNS} FROM thread_goals WHERE goal_id = ?1 LIMIT 1");
        let row = sqlx::query(&sql)
            .bind(goal_id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => {
                let thread_id: String = row.try_get("thread_id")?;
                Ok(Some((thread_id, goal_from_row(&row)?)))
            }
            None => Ok(None),
        }
    }

    /// P6 迁移专用旁路：一次性写入完整行（含任意 status / tokens_used /
    /// status_reason），不做状态机检查。目标 thread 已有行时先删（幂等重跑
    /// 由调用方 read-first 保证；这里的 DELETE 只是防御）。正常路径请用
    /// [`Self::create`] / [`Self::replace`]。
    // 参数即 thread_goals 行字段的一一对应，收拢成 struct 只会徒增一层。
    #[allow(clippy::too_many_arguments)]
    pub async fn insert_migrated(
        &self,
        thread_id: &str,
        goal_id: &str,
        objective: &str,
        token_budget: Option<i64>,
        tokens_used: i64,
        time_used_seconds: i64,
        verify_command: Option<&str>,
        status: GoalStatus,
        status_reason: Option<&str>,
    ) -> Result<Goal, GoalStoreError> {
        let objective = objective.trim();
        if objective.is_empty() {
            return Err(GoalStoreError::Validation(
                crate::types::GoalValidationError::EmptyObjective,
            ));
        }
        let ts = now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query("DELETE FROM thread_goals WHERE thread_id = ?1")
            .bind(thread_id)
            .execute(&mut *tx)
            .await?;
        let sql = format!(
            "INSERT INTO thread_goals \
                 (thread_id, goal_id, objective, status, token_budget, tokens_used, \
                  time_used_seconds, verify_command, status_reason, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10) \
             RETURNING {SELECT_GOAL_COLUMNS}"
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .bind(goal_id)
            .bind(objective)
            .bind(status.as_str())
            .bind(token_budget)
            .bind(tokens_used.max(0))
            .bind(time_used_seconds.max(0))
            .bind(verify_command)
            .bind(status_reason)
            .bind(ts)
            .fetch_one(&mut *tx)
            .await?;
        let goal = goal_from_row(&row)?;
        tx.commit().await?;
        Ok(goal)
    }

    // ── 用户 mutation（pause / resume / clear / edit）────────────────────

    /// active → paused（§6.1「用户 pause」）；非 active 报
    /// [`GoalStoreError::NotFoundOrDisallowed`]。
    pub async fn pause(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
    ) -> Result<Goal, GoalStoreError> {
        self.transition_status(
            thread_id,
            expected_goal_id,
            &[GoalStatus::Active],
            GoalStatus::Paused,
            None,
        )
        .await
    }

    /// paused / blocked / usage_limited / budget_limited → active（§6.1
    /// 「用户 resume/set」；B2：budget_limited 软终态可提额后 resume，
    /// 对齐 codex 外部 `set(status=Active)`；complete 仍不可恢复）。
    pub async fn resume(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
    ) -> Result<Goal, GoalStoreError> {
        let sql = format!(
            "UPDATE thread_goals SET status = 'active', status_reason = NULL, updated_at_ms = ?3 \
             WHERE thread_id = ?1 AND goal_id = ?2 \
               AND status IN ('paused','blocked','usage_limited','budget_limited') \
             RETURNING {SELECT_GOAL_COLUMNS}"
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .bind(expected_goal_id)
            .bind(now_ms())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| goal_from_row(&r))
            .transpose()?
            .ok_or_else(|| GoalStoreError::NotFoundOrDisallowed(thread_id.to_string()))
    }

    /// 任意已存在 goal → 无 goal（§6.1「clear」）。无视 goal_id（clear 是
    /// 用户终决；此后所有记账/续跑因无行自然 Unchanged）。返回是否存在。
    pub async fn clear(&self, thread_id: &str) -> Result<bool, GoalStoreError> {
        let res = sqlx::query("DELETE FROM thread_goals WHERE thread_id = ?1")
            .bind(thread_id)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected() > 0)
    }

    /// 编辑 objective：仅非终态 goal 可编辑（§6.5 objective_updated steering
    /// 场景；终态 goal 不可「改目标」）。
    pub async fn edit_objective(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
        objective: &str,
    ) -> Result<Goal, GoalStoreError> {
        let trimmed = objective.trim();
        if trimmed.is_empty() {
            return Err(GoalStoreError::Validation(
                crate::types::GoalValidationError::EmptyObjective,
            ));
        }
        if trimmed.len() > crate::types::MAX_OBJECTIVE_LEN {
            return Err(GoalStoreError::Validation(
                crate::types::GoalValidationError::ObjectiveTooLong,
            ));
        }
        let sql = format!(
            "UPDATE thread_goals SET objective = ?3, objective_revision = objective_revision + 1, \
             updated_at_ms = ?4 \
             WHERE thread_id = ?1 AND goal_id = ?2 \
               AND status NOT IN ('budget_limited','complete') \
             RETURNING {SELECT_GOAL_COLUMNS}"
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .bind(expected_goal_id)
            .bind(trimmed)
            .bind(now_ms())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| goal_from_row(&r))
            .transpose()?
            .ok_or_else(|| GoalStoreError::NotFoundOrDisallowed(thread_id.to_string()))
    }

    /// B2：预算调整（「提额继续」）。仅 complete 拒绝；budget_limited 下
    /// 提额是核心流程（update_budget → resume）。保留 goal_id 与
    /// tokens_used（对齐 codex `GoalSetRequest.token_budget` 组合面，
    /// 不重置计量）。
    pub async fn update_budget(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
        token_budget: i64,
    ) -> Result<Goal, GoalStoreError> {
        if token_budget <= 0 {
            return Err(GoalStoreError::Validation(
                crate::types::GoalValidationError::BudgetMustBePositive,
            ));
        }
        let cap = crate::types::max_goal_token_budget();
        if token_budget > cap {
            return Err(GoalStoreError::Validation(
                crate::types::GoalValidationError::BudgetAboveCap(cap),
            ));
        }
        let sql = format!(
            "UPDATE thread_goals SET token_budget = ?3, updated_at_ms = ?4 \
             WHERE thread_id = ?1 AND goal_id = ?2 AND status != 'complete' \
             RETURNING {SELECT_GOAL_COLUMNS}"
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .bind(expected_goal_id)
            .bind(token_budget)
            .bind(now_ms())
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| goal_from_row(&r))
            .transpose()?
            .ok_or_else(|| GoalStoreError::NotFoundOrDisallowed(thread_id.to_string()))
    }

    /// C1：goal turn 迭代计数落库（仅创建方 runtime 在 `start_goal_turn`
    /// 成功后调用；create/set/replace 经新行默认 0 归零）。
    pub async fn set_iteration(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
        iteration: i64,
    ) -> Result<Goal, GoalStoreError> {
        let sql = format!(
            "UPDATE thread_goals SET iteration_count = ?, updated_at_ms = ? \
             WHERE thread_id = ? AND goal_id = ? \
             RETURNING {SELECT_GOAL_COLUMNS}"
        );
        let row = sqlx::query(&sql)
            .bind(iteration)
            .bind(now_ms())
            .bind(thread_id)
            .bind(expected_goal_id)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|r| goal_from_row(&r))
            .transpose()?
            .ok_or_else(|| GoalStoreError::NotFoundOrDisallowed(thread_id.to_string()))
    }

    // ── 系统置位（usage_limited / blocked / 模型 complete/blocked 落账）──

    /// active/budget_limited → usage_limited（§6.1 provider 用量，系统置位；
    /// B2：对齐 codex `can_stop` 的 BudgetLimited→UsageLimited 覆盖——用量
    /// 耗尽比预算触顶更严重且用户可操作）。
    pub async fn mark_usage_limited(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
        reason: &str,
    ) -> Result<Goal, GoalStoreError> {
        self.transition_status(
            thread_id,
            expected_goal_id,
            &[GoalStatus::Active, GoalStatus::BudgetLimited],
            GoalStatus::UsageLimited,
            Some(reason),
        )
        .await
    }

    /// active → blocked（不可恢复 turn error，§6.1；blocked 不得覆盖
    /// budget_limited——预算优先，与 codex can_stop 一致）。
    pub async fn mark_blocked(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
        reason: &str,
    ) -> Result<Goal, GoalStoreError> {
        self.transition_status(
            thread_id,
            expected_goal_id,
            &[GoalStatus::Active],
            GoalStatus::Blocked,
            Some(reason),
        )
        .await
    }

    /// 模型 `update_goal(complete)` 落账（§6.8/§6.9：verify 门在 runtime 层，
    /// 通过后才调本方法）；允许从 active 补最后一段 delta 后落 complete。
    pub async fn mark_complete(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
    ) -> Result<Goal, GoalStoreError> {
        self.transition_status(
            thread_id,
            expected_goal_id,
            &[GoalStatus::Active],
            GoalStatus::Complete,
            None,
        )
        .await
    }

    async fn transition_status(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
        from_any: &[GoalStatus],
        to: GoalStatus,
        reason: Option<&str>,
    ) -> Result<Goal, GoalStoreError> {
        let placeholders = from_any.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        // 注意：纯位置参数（sqlx 混用 ?N 与 ? 会错位绑定，导致 WHERE 永不命中）
        let sql = format!(
            "UPDATE thread_goals SET status = ?, status_reason = ?, updated_at_ms = ? \
             WHERE thread_id = ? AND goal_id = ? AND status IN ({placeholders}) \
             RETURNING {SELECT_GOAL_COLUMNS}"
        );
        let mut query = sqlx::query(&sql)
            .bind(to.as_str())
            .bind(reason)
            .bind(now_ms())
            .bind(thread_id)
            .bind(expected_goal_id);
        for status in from_any {
            query = query.bind(status.as_str());
        }
        let row = query.fetch_optional(&self.pool).await?;
        row.map(|r| goal_from_row(&r))
            .transpose()?
            .ok_or_else(|| GoalStoreError::NotFoundOrDisallowed(thread_id.to_string()))
    }

    // ── 原子记账（§6.2/§6.3）────────────────────────────────────────────

    /// 单语句 `UPDATE ... RETURNING`：累加 delta + budget 触顶翻转
    /// （active → budget_limited，终态）+ mode 门控 + `expected_goal_id` CAS。
    ///
    /// 返回 `Unchanged` 的情形（调用方语义相同——丢弃 delta）：
    /// 无此行 / goal_id 不匹配（陈旧写）/ 当前状态不在 mode 允许集。
    pub async fn account_thread_goal_usage(
        &self,
        thread_id: &str,
        expected_goal_id: &str,
        mode: AccountingMode,
        delta_tokens: i64,
        delta_seconds: i64,
    ) -> Result<AccountingOutcome, GoalStoreError> {
        // 防御：负 delta 一律钳 0（公式已在调用方 saturating，这里兜底）
        let delta_tokens = delta_tokens.max(0);
        let delta_seconds = delta_seconds.max(0);
        let sql = format!(
            "UPDATE thread_goals \
             SET tokens_used = tokens_used + ?3, \
                  time_used_seconds = time_used_seconds + ?4, \
                  status_reason = CASE \
                      WHEN token_budget IS NOT NULL \
                           AND tokens_used + ?3 >= token_budget \
                           AND status = 'active' \
                      THEN 'budget_limited' ELSE status_reason END, \
                  status = CASE \
                     WHEN token_budget IS NOT NULL \
                          AND tokens_used + ?3 >= token_budget \
                          AND status = 'active' \
                     THEN 'budget_limited' ELSE status END, \
                 updated_at_ms = ?5 \
             WHERE thread_id = ?1 AND goal_id = ?2 \
               AND status IN ({}) \
             RETURNING tokens_used, status",
            mode.allowed_statuses_sql()
        );
        let row = sqlx::query(&sql)
            .bind(thread_id)
            .bind(expected_goal_id)
            .bind(delta_tokens)
            .bind(delta_seconds)
            .bind(now_ms())
            .fetch_optional(&self.pool)
            .await?;
        match row {
            None => Ok(AccountingOutcome::Unchanged),
            Some(r) => {
                let tokens_used: i64 = r.try_get(0)?;
                let status_str: String = r.try_get(1)?;
                let status = GoalStatus::parse(&status_str)
                    .ok_or_else(|| GoalStoreError::CorruptStatus(status_str))?;
                Ok(AccountingOutcome::Updated {
                    tokens_used,
                    status,
                })
            }
        }
    }

    // ── deferral（§6.6）──────────────────────────────────────────────────

    /// 写入续跑推迟标记。无 goal 行时为 no-op（FK 约束；盲审 C2：fork 后新
    /// session 无 goal → 不携带 deferral，返回 `Ok(false)`）。
    pub async fn defer_continuation(&self, thread_id: &str) -> Result<bool, GoalStoreError> {
        if self.read(thread_id).await?.is_none() {
            return Ok(false);
        }
        let res = sqlx::query(
            "INSERT OR IGNORE INTO thread_goal_continuation_deferrals (thread_id) VALUES (?1)",
        )
        .bind(thread_id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    pub async fn has_continuation_deferral(&self, thread_id: &str) -> Result<bool, GoalStoreError> {
        let row =
            sqlx::query("SELECT 1 FROM thread_goal_continuation_deferrals WHERE thread_id = ?1")
                .bind(thread_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    /// `on_turn_start` 清除（§6.6）。返回是否确有 deferral 被清除。
    pub async fn clear_continuation_deferral(
        &self,
        thread_id: &str,
    ) -> Result<bool, GoalStoreError> {
        let res =
            sqlx::query("DELETE FROM thread_goal_continuation_deferrals WHERE thread_id = ?1")
                .bind(thread_id)
                .execute(&self.pool)
                .await?;
        Ok(res.rows_affected() > 0)
    }
}

async fn insert_new_tx(
    tx: &mut sqlx::SqliteConnection,
    req: &CreateGoalRequest,
    goal_id: &str,
) -> Result<Goal, GoalStoreError> {
    let ts = now_ms();
    let sql = format!(
        "INSERT INTO thread_goals \
             (thread_id, goal_id, objective, status, token_budget, tokens_used, \
              time_used_seconds, verify_command, status_reason, created_at_ms, updated_at_ms) \
         VALUES (?1, ?2, ?3, 'active', ?4, 0, 0, ?5, NULL, ?6, ?6) \
         RETURNING {SELECT_GOAL_COLUMNS}"
    );
    let row = sqlx::query(&sql)
        .bind(&req.thread_id)
        .bind(goal_id)
        .bind(req.trimmed_objective())
        .bind(req.token_budget)
        .bind(req.verify_command.clone())
        .bind(ts)
        .fetch_one(tx)
        .await?;
    Ok(goal_from_row(&row)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AccountingMode as Mode;
    use std::str::FromStr;

    async fn test_store() -> (tempfile::TempDir, GoalStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .expect("open TaskDb");
        (dir, GoalStore::from_task_db(&db))
    }

    fn req(thread: &str, objective: &str, budget: Option<i64>) -> CreateGoalRequest {
        CreateGoalRequest {
            thread_id: thread.into(),
            objective: objective.into(),
            token_budget: budget,
            verify_command: None,
        }
    }

    async fn make_active(store: &GoalStore, thread: &str, budget: Option<i64>) -> Goal {
        store
            .create(&req(thread, "objective", budget), "g1")
            .await
            .expect("create")
    }

    #[tokio::test]
    async fn create_and_read_roundtrip() {
        let (_d, store) = test_store().await;
        let mut r = req("t1", "  ship the thing  ", Some(1000));
        r.verify_command = Some("cargo test".into());
        let g = store.create(&r, "g1").await.expect("create");
        assert_eq!(g.status, GoalStatus::Active);
        assert_eq!(g.objective, "ship the thing");
        assert_eq!(g.verify_command.as_deref(), Some("cargo test"));
        assert_eq!(g.tokens_used, 0);
        assert_eq!(store.read("t1").await.expect("read"), Some(g));
        assert_eq!(store.read("missing").await.expect("read"), None);
    }

    /// §6.8：模型/系统 create 只能替换 complete；用户 replace 不受限。
    #[tokio::test]
    async fn create_only_replaces_complete_goal() {
        let (_d, store) = test_store().await;
        let active = make_active(&store, "t1", None).await;

        let err = store
            .create(&req("t1", "second", None), "g2")
            .await
            .expect_err("create over active must fail");
        assert!(matches!(err, GoalStoreError::ExistingUnfinishedGoal(_)));

        store
            .mark_complete("t1", &active.goal_id)
            .await
            .expect("complete");
        let completed_replacement = store
            .create(&req("t1", "second", None), "g2")
            .await
            .expect("create over complete");
        assert_eq!(completed_replacement.goal_id, "g2");

        let limited = make_active(&store, "limited", Some(1)).await;
        store
            .account_thread_goal_usage("limited", &limited.goal_id, Mode::ActiveStatusOnly, 1, 0)
            .await
            .expect("limit goal");
        let err = store
            .create(&req("limited", "replacement", None), "g-limited-2")
            .await
            .expect_err("create over budget_limited must fail");
        assert!(matches!(err, GoalStoreError::ExistingUnfinishedGoal(_)));

        // 用户替换入口不受限（盲审 A3）
        let g = store
            .replace(&req("t1", "third", None), "g3")
            .await
            .expect("replace");
        assert_eq!(g.goal_id, "g3");
    }

    /// §6.2：陈旧 goal_id 写入返回 Unchanged 且不落账（CAS）。
    #[tokio::test]
    async fn cas_stale_goal_id_is_unchanged() {
        let (_d, store) = test_store().await;
        make_active(&store, "t1", None).await;
        store
            .replace(&req("t1", "v2", None), "g2")
            .await
            .expect("replace");

        let out = store
            .account_thread_goal_usage("t1", "g1", Mode::ActiveStatusOnly, 500, 10)
            .await
            .expect("account");
        assert_eq!(out, AccountingOutcome::Unchanged);
        let g = store.read("t1").await.expect("read").expect("exists");
        assert_eq!(g.tokens_used, 0, "陈旧写的 delta 必须被丢弃");
    }

    /// §6.3：mode × 状态矩阵在真实 SQL 层逐格验证。
    #[tokio::test]
    async fn account_mode_matrix_over_sql() {
        for status in GoalStatus::ALL {
            let (_d, store) = test_store().await;
            let g = make_active(&store, "t1", None).await;
            // 把行推到目标状态
            match status {
                GoalStatus::Active => {}
                GoalStatus::Paused => {
                    store.pause("t1", &g.goal_id).await.expect("pause");
                }
                GoalStatus::Blocked => {
                    store
                        .mark_blocked("t1", &g.goal_id, "x")
                        .await
                        .expect("blocked");
                }
                GoalStatus::UsageLimited => {
                    store
                        .mark_usage_limited("t1", &g.goal_id, "q")
                        .await
                        .expect("usage");
                }
                GoalStatus::BudgetLimited => {
                    // 通过把预算调小实现触顶（直接经 account 不行——account 才是翻转路径；
                    // 这里用小预算 goal + 一次大 delta）
                    let _ = store.clear("t1").await;
                    store
                        .create(&req("t1", "obj", Some(100)), "gb")
                        .await
                        .expect("create");
                    let out = store
                        .account_thread_goal_usage("t1", "gb", Mode::ActiveStatusOnly, 150, 0)
                        .await
                        .expect("account");
                    assert!(matches!(
                        out,
                        AccountingOutcome::Updated {
                            status: GoalStatus::BudgetLimited,
                            ..
                        }
                    ));
                    continue;
                }
                GoalStatus::Complete => {
                    store
                        .mark_complete("t1", &g.goal_id)
                        .await
                        .expect("complete");
                }
            }
            for mode in [
                Mode::ActiveStatusOnly,
                Mode::ActiveOnly,
                Mode::ActiveOrComplete,
                Mode::ActiveOrStopped,
            ] {
                let out = store
                    .account_thread_goal_usage("t1", &g.goal_id, mode, 10, 1)
                    .await
                    .expect("account");
                let expected_updated = mode.allows(status);
                assert_eq!(
                    matches!(out, AccountingOutcome::Updated { .. }),
                    expected_updated,
                    "mode {mode:?} × status {status:?}"
                );
            }
        }
    }

    /// §6.5：越界翻转为 budget_limited（终态，非 cancelled）；
    /// ActiveOnly 仍可对 budget_limited 补账，ActiveStatusOnly 不行。
    #[tokio::test]
    async fn budget_flip_is_terminal_and_catchup_works() {
        let (_d, store) = test_store().await;
        store
            .create(&req("t1", "obj", Some(100)), "g1")
            .await
            .expect("create");

        let out = store
            .account_thread_goal_usage("t1", "g1", Mode::ActiveStatusOnly, 40, 0)
            .await
            .expect("account");
        assert_eq!(
            out,
            AccountingOutcome::Updated {
                tokens_used: 40,
                status: GoalStatus::Active
            }
        );

        let out = store
            .account_thread_goal_usage("t1", "g1", Mode::ActiveStatusOnly, 60, 5)
            .await
            .expect("account");
        assert_eq!(
            out,
            AccountingOutcome::Updated {
                tokens_used: 100,
                status: GoalStatus::BudgetLimited
            }
        );
        let limited = store.read("t1").await.expect("read").expect("goal");
        assert_eq!(limited.status_reason.as_deref(), Some("budget_limited"));

        let normal = store
            .account_thread_goal_usage("t1", "g1", Mode::ActiveStatusOnly, 10, 0)
            .await
            .expect("account");
        assert_eq!(normal, AccountingOutcome::Unchanged, "终态不接受普通记账");

        let catchup = store
            .account_thread_goal_usage("t1", "g1", Mode::ActiveOnly, 10, 0)
            .await
            .expect("account");
        assert!(matches!(
            catchup,
            AccountingOutcome::Updated {
                status: GoalStatus::BudgetLimited,
                ..
            }
        ));
    }

    /// §6.1 用户 mutation 转移 + resume 对终态拒绝。
    #[tokio::test]
    async fn user_transitions_pause_resume_clear() {
        let (_d, store) = test_store().await;
        let g = make_active(&store, "t1", None).await;

        let paused = store.pause("t1", &g.goal_id).await.expect("pause");
        assert_eq!(paused.status, GoalStatus::Paused);
        assert!(
            store.pause("t1", &g.goal_id).await.is_err(),
            "paused 不可再 pause"
        );

        let active = store.resume("t1", &g.goal_id).await.expect("resume");
        assert_eq!(active.status, GoalStatus::Active);

        // blocked/usage_limited 可恢复
        store
            .mark_usage_limited("t1", &g.goal_id, "quota")
            .await
            .expect("usage");
        assert_eq!(
            store.resume("t1", &g.goal_id).await.expect("resume").status,
            GoalStatus::Active
        );
        store
            .mark_blocked("t1", &g.goal_id, "boom")
            .await
            .expect("blocked");
        let r = store.resume("t1", &g.goal_id).await.expect("resume");
        assert_eq!(r.status, GoalStatus::Active);
        assert_eq!(r.status_reason, None, "resume 应清 status_reason");

        // 终态不可 resume
        store
            .mark_complete("t1", &g.goal_id)
            .await
            .expect("complete");
        assert!(store.resume("t1", &g.goal_id).await.is_err());

        // clear
        assert!(store.clear("t1").await.expect("clear"));
        assert!(
            !store.clear("t1").await.expect("clear again"),
            "二次 clear 幂等"
        );
        assert_eq!(store.read("t1").await.expect("read"), None);
    }

    /// §6.6：deferral 生命周期；FK 级联（TODO P1 单测项）。
    #[tokio::test]
    async fn deferral_lifecycle_and_fk_cascade() {
        let (_d, store) = test_store().await;
        let g = make_active(&store, "t1", None).await;

        assert!(store.defer_continuation("t1").await.expect("defer"));
        assert!(store.has_continuation_deferral("t1").await.expect("has"));

        // clear 删 goal 行 → deferral 级联消失
        store.clear("t1").await.expect("clear");
        assert!(
            !store.has_continuation_deferral("t1").await.expect("has"),
            "FK CASCADE 未生效：foreign_keys(true) 前置修复失败"
        );

        // 无 goal 行时 defer 为 no-op（盲审 C2：fork 后新 session）
        assert!(!store
            .defer_continuation("ghost")
            .await
            .expect("defer ghost"));
        assert!(!store
            .has_continuation_deferral("ghost")
            .await
            .expect("has ghost"));

        // on_turn_start 清除路径
        let _ = make_active(&store, "t2", None).await;
        store.defer_continuation("t2").await.expect("defer");
        assert!(store
            .clear_continuation_deferral("t2")
            .await
            .expect("clear deferral"));
        assert!(!store
            .clear_continuation_deferral("t2")
            .await
            .expect("clear again"));
        let _ = g;
    }

    /// 用户 set 替换入口（盲审 A3）：对 active/paused 直接覆盖，
    /// 替换后旧 goal 的记账变 Unchanged，deferral 一并级联清除。
    #[tokio::test]
    async fn replace_allows_any_status_and_cascades() {
        let (_d, store) = test_store().await;
        let g = make_active(&store, "t1", None).await;
        store.defer_continuation("t1").await.expect("defer");

        let v2 = store
            .replace(&req("t1", "v2", Some(50)), "g2")
            .await
            .expect("replace");
        assert_eq!(v2.goal_id, "g2");
        assert!(
            !store.has_continuation_deferral("t1").await.expect("has"),
            "替换须级联清 deferral"
        );

        let out = store
            .account_thread_goal_usage("t1", &g.goal_id, Mode::ActiveOrStopped, 999, 9)
            .await
            .expect("account");
        assert_eq!(out, AccountingOutcome::Unchanged, "旧 goal 写新行必须被拒");

        // paused 也可被用户 set 直接替换
        let g2 = store.pause("t1", "g2").await.expect("pause");
        assert_eq!(g2.status, GoalStatus::Paused);
        store
            .replace(&req("t1", "v3", None), "g3")
            .await
            .expect("replace paused");
        assert_eq!(
            store.read("t1").await.expect("read").map(|g| g.goal_id),
            Some("g3".into())
        );
    }

    #[tokio::test]
    async fn edit_objective_rules() {
        let (_d, store) = test_store().await;
        let g = make_active(&store, "t1", None).await;

        let edited = store
            .edit_objective("t1", &g.goal_id, "  new objective  ")
            .await
            .expect("edit");
        assert_eq!(edited.objective, "new objective");

        store
            .mark_complete("t1", &g.goal_id)
            .await
            .expect("complete");
        assert!(
            store
                .edit_objective("t1", &g.goal_id, "nope")
                .await
                .is_err(),
            "终态不可编辑"
        );
        assert!(
            store.edit_objective("t1", &g.goal_id, "   ").await.is_err(),
            "空 objective 应被拒"
        );
    }

    /// sqlx 连接串带 query 参数时 FromStr 解析冒烟（FK 修复回归的一部分）。
    /// 注意：0.8 的 SqliteConnectOptions 无 getter，FK 级联行为由
    /// deferral_lifecycle_and_fk_cascade 端到端验证。
    #[test]
    fn sqlite_connect_options_parse_with_query() {
        let _ = sqlx::sqlite::SqliteConnectOptions::from_str("sqlite:x.db?mode=rwc")
            .expect("parse connect options");
    }
}

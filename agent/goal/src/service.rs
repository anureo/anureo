//! [`GoalService`]：用户侧 API，供 ACP 扩展（`_anureo.dev/goal/*`）与 REPL
//! （`/goal`）共用（alignment §5）。
//!
//! Phase 1 提供无锁基础实现（纯 mutation 转发）；Phase 3 在此之上包
//! `goal_state_lock` 的「读→写→start_turn」窗口与续跑驱动（TODO P3）。
//! 用户侧方法不暴露 goal_id——先读当前 goal 再以其 goal_id 做 CAS。

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::accounting::LOCK_TIMEOUT;
use crate::metrics;
use crate::objective_file;
use crate::store::{GoalStore, GoalStoreError};
use crate::types::{CreateGoalRequest, Goal, GoalStatus};

/// goal_state_lock（§5/§6.6）：1 permit，acquire 带 timeout。
/// 覆盖 `GoalService::set/clear` 的「读→写」窗口与 runtime `continue_if_idle`
/// 的「读→渲染→start_turn」窗口，防止 idle 续跑与用户操作竞态。
#[derive(Clone)]
pub struct GoalStateLock {
    permit: Arc<Semaphore>,
}

impl GoalStateLock {
    pub fn new() -> Self {
        Self { permit: Arc::new(Semaphore::new(1)) }
    }

    /// 带超时获取；超时返回 `None`（调用方按「未获锁」处理，不阻塞用户操作）。
    pub async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        match tokio::time::timeout(LOCK_TIMEOUT, self.permit.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Some(permit),
            _ => None,
        }
    }
}

impl Default for GoalStateLock {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GoalServiceError {
    #[error("no goal on thread {0}")]
    NoGoal(String),
    #[error("goal is {status:?}, not resumable by user")]
    NotResumable { status: GoalStatus },
    #[error("goal_state_lock acquisition timed out; operation aborted to avoid racing idle continuation")]
    LockTimeout,
    #[error(transparent)]
    Store(#[from] GoalStoreError),
}

/// [`GoalService::set_with_verify_outcome`] 的结果：新 goal + 是否发生了
/// 快照替换（覆盖既有 goal，§6.6 deferral 写入场景之一，宿主据此调
/// [`crate::GoalRuntimeHandle::defer_continuation`]）。
#[derive(Debug, Clone)]
pub struct GoalSetOutcome {
    pub goal: Goal,
    pub replaced_existing: bool,
}

#[derive(Clone)]
pub struct GoalService {
    store: GoalStore,
    state_lock: Option<GoalStateLock>,
}

impl GoalService {
    pub fn new(store: GoalStore) -> Self {
        Self { store, state_lock: None }
    }

    /// 持 goal_state_lock 的构造（§5：set/clear 窗口）。
    pub fn with_state_lock(store: GoalStore, state_lock: GoalStateLock) -> Self {
        Self { store, state_lock: Some(state_lock) }
    }

    pub fn store(&self) -> &GoalStore {
        &self.store
    }

    async fn guard(&self) -> Result<Option<OwnedSemaphorePermit>, GoalServiceError> {
        match &self.state_lock {
            None => Ok(None),
            Some(lock) => lock.acquire().await.ok_or(GoalServiceError::LockTimeout).map(Some),
        }
    }

    /// 用户 set（§6.1「外部 set」）：替换入口，任何旧状态可覆盖；
    /// 置位后 goal 为 active。deferral 语义（§6.6 快照替换）由宿主层根据
    /// `replaced_existing` 补写。
    pub async fn set(
        &self,
        thread_id: &str,
        objective: &str,
        token_budget: Option<i64>,
    ) -> Result<Goal, GoalServiceError> {
        self.set_with_verify(thread_id, objective, token_budget, None).await
    }

    /// 同 [`Self::set`]，附带 verify_command（完成门载体，§6.9）。
    pub async fn set_with_verify(
        &self,
        thread_id: &str,
        objective: &str,
        token_budget: Option<i64>,
        verify_command: Option<String>,
    ) -> Result<Goal, GoalServiceError> {
        Ok(self
            .set_with_verify_outcome(thread_id, objective, token_budget, verify_command)
            .await?
            .goal)
    }

    /// 同 [`Self::set_with_verify`]，另返回是否覆盖了既有 goal（快照替换）。
    pub async fn set_with_verify_outcome(
        &self,
        thread_id: &str,
        objective: &str,
        token_budget: Option<i64>,
        verify_command: Option<String>,
    ) -> Result<GoalSetOutcome, GoalServiceError> {
        let _guard = self.guard().await?;
        let replaced_existing = self.store.read(thread_id).await?.is_some();
        let stored_objective = self.prepare_objective(thread_id, objective).await?;
        let req = CreateGoalRequest {
            thread_id: thread_id.to_string(),
            objective: stored_objective,
            token_budget,
            verify_command,
        };
        let goal = self.store.replace(&req, &new_goal_id()).await?;
        metrics::global().record_set(thread_id, replaced_existing);
        Ok(GoalSetOutcome {
            goal,
            replaced_existing,
        })
    }

    /// objective 文件化准备（P7）：超限文本写 `<goals_dir>/<thread>.md` 并返回
    /// `@file:` 标记（DB 存标记，文件为长文本事实源）；未超限返回原文并清理
    /// 残留文件。无 goals_dir 时原样返回——超长 objective 由 DB 内联校验拒绝。
    /// 写盘失败同样降级为校验错误（不让无文件的标记行落库）。
    async fn prepare_objective(
        &self,
        thread_id: &str,
        objective: &str,
    ) -> Result<String, GoalServiceError> {
        let trimmed = objective.trim();
        if !objective_file::needs_file_backing(trimmed) {
            if let Some(dir) = self.store.goals_dir() {
                objective_file::remove(dir, thread_id).await;
            }
            return Ok(trimmed.to_string());
        }
        let too_long =
            GoalServiceError::Store(crate::types::GoalValidationError::ObjectiveTooLong.into());
        if trimmed.chars().count() > objective_file::MAX_FILE_OBJECTIVE_CHARS {
            return Err(too_long);
        }
        let Some(dir) = self.store.goals_dir() else {
            // 无处落盘：返回原文，让 store 内联校验拒绝（错误语义与旧版一致）
            return Ok(trimmed.to_string());
        };
        if objective_file::write(dir, thread_id, trimmed)
            .await
            .is_err()
        {
            return Err(too_long);
        }
        Ok(objective_file::marker_for(thread_id))
    }

    pub async fn show(&self, thread_id: &str) -> Result<Option<Goal>, GoalServiceError> {
        Ok(self.store.read(thread_id).await?)
    }

    /// objective 全文还原（P7）：文件化 goal 读回全文，非文件化原样返回。
    /// 文本消费方（REPL show、legacy face、steering）在展示/注入前调用。
    pub async fn resolve_objective(&self, goal: Goal) -> Goal {
        self.store.resolve_objective(goal).await
    }

    /// 用户 pause：active → paused。
    pub async fn pause(&self, thread_id: &str) -> Result<Goal, GoalServiceError> {
        let goal = self.current(thread_id).await?;
        let paused = self.store.pause(thread_id, &goal.goal_id).await?;
        metrics::global().record_pause(thread_id);
        Ok(paused)
    }

    /// 用户 resume：仅 paused / blocked / usage_limited 可恢复（§6.1；
    /// 终态不可恢复）。
    pub async fn resume(&self, thread_id: &str) -> Result<Goal, GoalServiceError> {
        let goal = self.current(thread_id).await?;
        if !goal.status.user_resumable() {
            return Err(GoalServiceError::NotResumable { status: goal.status });
        }
        let resumed = self.store.resume(thread_id, &goal.goal_id).await?;
        metrics::global().record_resume(thread_id);
        Ok(resumed)
    }

    /// 用户 clear：任意状态 → 无 goal。
    pub async fn clear(&self, thread_id: &str) -> Result<bool, GoalServiceError> {
        let _guard = self.guard().await?;
        let cleared = self.store.clear(thread_id).await?;
        if cleared {
            if let Some(dir) = self.store.goals_dir() {
                objective_file::remove(dir, thread_id).await;
            }
            metrics::global().record_clear(thread_id);
        }
        Ok(cleared)
    }

    /// 用户 edit objective：仅非终态（§6.5 objective_updated steering）。
    pub async fn edit(
        &self,
        thread_id: &str,
        objective: &str,
    ) -> Result<Goal, GoalServiceError> {
        let goal = self.current(thread_id).await?;
        let stored = self.prepare_objective(thread_id, objective).await?;
        let edited = self
            .store
            .edit_objective(thread_id, &goal.goal_id, &stored)
            .await?;
        metrics::global().record_edit(thread_id);
        Ok(edited)
    }

    async fn current(&self, thread_id: &str) -> Result<Goal, GoalServiceError> {
        self.store
            .read(thread_id)
            .await?
            .ok_or_else(|| GoalServiceError::NoGoal(thread_id.to_string()))
    }
}

/// goal_id：时间戳前缀（可读、单调趋势）+ uuid 随机段（同毫秒防碰撞）。
pub(crate) fn new_goal_id() -> String {
    let ts = chrono::Utc::now().timestamp_millis();
    let rand = uuid::Uuid::new_v4().simple().to_string();
    format!("goal-{ts}-{}", &rand[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn service() -> (tempfile::TempDir, GoalService) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .expect("open TaskDb");
        (dir, GoalService::new(GoalStore::from_task_db(&db)))
    }

    #[tokio::test]
    async fn user_mutation_flow() {
        let (_d, svc) = service().await;

        // show：无 goal
        assert_eq!(svc.show("t1").await.expect("show"), None);
        assert!(matches!(
            svc.pause("t1").await,
            Err(GoalServiceError::NoGoal(_))
        ));

        // set → active；再 set 直接替换（用户终决）
        let g1 = svc.set("t1", "first", Some(1000)).await.expect("set");
        assert_eq!(g1.status, GoalStatus::Active);
        let g2 = svc.set("t1", "second", None).await.expect("set replace");
        assert_eq!(g2.objective, "second");

        // pause → resume
        assert_eq!(svc.pause("t1").await.expect("pause").status, GoalStatus::Paused);
        assert_eq!(svc.resume("t1").await.expect("resume").status, GoalStatus::Active);

        // 终态 resume 拒绝（usage_limited 可恢复、complete 不可恢复）
        assert!(matches!(
            svc.resume("t1").await,
            Err(GoalServiceError::NotResumable { status: GoalStatus::Active })
        ));
        svc.store()
            .mark_complete("t1", &g2.goal_id)
            .await
            .expect("complete");
        assert!(matches!(
            svc.resume("t1").await,
            Err(GoalServiceError::NotResumable { status: GoalStatus::Complete })
        ));

        // edit / clear
        assert!(matches!(
            svc.edit("t1", "nope").await,
            Err(GoalServiceError::Store(GoalStoreError::NotFoundOrDisallowed(_)))
        ));
        assert!(svc.clear("t1").await.expect("clear"));
        assert_eq!(svc.show("t1").await.expect("show"), None);
    }

    #[test]
    fn goal_id_format() {
        let id = new_goal_id();
        assert!(id.starts_with("goal-"), "{id}");
        assert_eq!(id.len(), "goal-".len() + 13 + 1 + 8, "{id}");
    }

    #[tokio::test]
    async fn set_outcome_reports_replacement() {
        let (_d, svc) = service().await;
        let first = svc
            .set_with_verify_outcome("t9", "a", None, None)
            .await
            .expect("first set");
        assert!(!first.replaced_existing, "首次 set 不算替换");
        let second = svc
            .set_with_verify_outcome("t9", "b", None, None)
            .await
            .expect("second set");
        assert!(second.replaced_existing, "覆盖既有 goal = 快照替换（§6.6）");
        assert_eq!(second.goal.objective, "b");
    }

    #[tokio::test]
    async fn objective_file_synced_on_set_edit_clear() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .expect("open TaskDb");
        let goals_dir = dir.path().join("goals");
        let store = GoalStore::from_task_db(&db).with_goals_dir(goals_dir.clone());
        let svc = GoalService::new(store);
        let path = objective_file::file_path(&goals_dir, "tf");
        let long = "x".repeat(objective_file::OBJECTIVE_INLINE_LIMIT_CHARS + 1);

        // 超限 set → 文件落盘，DB 存 @file: 标记（文件为长文本事实源）
        svc.set("tf", &long, None).await.expect("set long");
        assert!(path.exists(), "超限 objective 应落盘");
        let stored = svc.show("tf").await.expect("show").unwrap();
        assert!(stored.objective_file, "goal_from_row 应按标记置位");
        assert_eq!(stored.objective, objective_file::marker_for("tf"));
        assert_eq!(
            svc.resolve_objective(stored).await.objective,
            long,
            "resolve 应还原全文"
        );

        // 未超限 set → 标记清除、残留文件清理
        let short = svc.set("tf", "short", None).await.expect("set short");
        assert!(!short.objective_file);
        assert!(!path.exists(), "回到内联后应清理文件");

        // edit 超限 → 再落盘
        let edited = svc.edit("tf", &long).await.expect("edit long");
        assert!(edited.objective_file);
        assert!(path.exists(), "edit 超限同样落盘");

        // clear → 文件删除
        svc.clear("tf").await.expect("clear");
        assert!(!path.exists(), "clear 应删除文件");

        // 无 goals_dir 的 store：超限 objective 走 DB 内联校验拒绝
        let (_d2, svc2) = service().await;
        let err = svc2.set("tf2", &long, None).await.expect_err("should reject");
        assert!(matches!(
            err,
            GoalServiceError::Store(GoalStoreError::Validation(
                crate::types::GoalValidationError::ObjectiveTooLong
            ))
        ));

        // 字节超限但 chars 未超（CJK）：同样走文件化，不被 DB 字节校验误伤
        let cjk = "目".repeat(objective_file::OBJECTIVE_INLINE_LIMIT_CHARS);
        assert!(
            objective_file::needs_file_backing(&cjk),
            "字节超限（CJK 12000B）应判定需文件化"
        );
        let cjk_goal = svc.set("tfc", &cjk, None).await.expect("cjk set");
        assert!(cjk_goal.objective_file, "CJK 字节超限应文件化");
        assert_eq!(
            svc.resolve_objective(cjk_goal).await.objective, cjk,
            "CJK 全文可还原"
        );
    }
}

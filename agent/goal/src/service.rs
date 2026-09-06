//! [`GoalService`]：用户侧 API，供 ACP 扩展（`_anureo.dev/goal/*`）与 REPL
//! （`/goal`）共用（alignment §5）。
//!
//! Phase 1 提供无锁基础实现（纯 mutation 转发）；Phase 3 在此之上包
//! `goal_state_lock` 的「读→写→start_turn」窗口与续跑驱动（TODO P3）。
//! 用户侧方法不暴露 goal_id——先读当前 goal 再以其 goal_id 做 CAS。

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::accounting::LOCK_TIMEOUT;
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
    /// 置位后 goal 为 active。deferral 语义（§6.6 快照替换）由 runtime 层补。
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
        let _guard = self.guard().await?;
        let req = CreateGoalRequest {
            thread_id: thread_id.to_string(),
            objective: objective.to_string(),
            token_budget,
            verify_command,
        };
        Ok(self.store.replace(&req, &new_goal_id()).await?)
    }

    pub async fn show(&self, thread_id: &str) -> Result<Option<Goal>, GoalServiceError> {
        Ok(self.store.read(thread_id).await?)
    }

    /// 用户 pause：active → paused。
    pub async fn pause(&self, thread_id: &str) -> Result<Goal, GoalServiceError> {
        let goal = self.current(thread_id).await?;
        Ok(self.store.pause(thread_id, &goal.goal_id).await?)
    }

    /// 用户 resume：仅 paused / blocked / usage_limited 可恢复（§6.1；
    /// 终态不可恢复）。
    pub async fn resume(&self, thread_id: &str) -> Result<Goal, GoalServiceError> {
        let goal = self.current(thread_id).await?;
        if !goal.status.user_resumable() {
            return Err(GoalServiceError::NotResumable { status: goal.status });
        }
        Ok(self.store.resume(thread_id, &goal.goal_id).await?)
    }

    /// 用户 clear：任意状态 → 无 goal。
    pub async fn clear(&self, thread_id: &str) -> Result<bool, GoalServiceError> {
        let _guard = self.guard().await?;
        Ok(self.store.clear(thread_id).await?)
    }

    /// 用户 edit objective：仅非终态（§6.5 objective_updated steering）。
    pub async fn edit(
        &self,
        thread_id: &str,
        objective: &str,
    ) -> Result<Goal, GoalServiceError> {
        let goal = self.current(thread_id).await?;
        Ok(self.store.edit_objective(thread_id, &goal.goal_id, objective).await?)
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
}

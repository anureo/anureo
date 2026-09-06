//! `_anureo.dev/goal/*` extension — thread_goals 后端（P5b）。
//!
//! 后端 = `goal` crate（`thread_goals` 表，键 = session 的 thread_id）；
//! 旧的 goals.json 文件后端与重启恢复预约机制已随 P5b 退役（遗留 detached
//! runner 所需的 JSON 存取搬至 `crate::goal_runner`，随 P7 一并移除）。
//!
//! ## 兼容层（FE 在外部仓依赖，见 docs/acp-spec/extensions/14-*.md）
//!
//! - 方法名/参数/响应形状保持旧 API（`start/pause/resume/cancel/get/list`）；
//! - 键映射：`get/pause/resume/cancel` 以旧 goal-id 为键（store 反查），
//!   `start` 以 `sessionId` 定位 thread（`SessionStore` 反查 thread_id）；
//! - 状态投影：新 6 态 → 旧 6 态（`project_status`），详情进 `metadata`；
//! - 通知：响应内嵌旧 `notification`（goal/changed 形状）不变，另加
//!   `updated`（全量快照）；同时向所有连接广播 `_anureo.dev/goal/changed`
//!   与新增的 `_anureo.dev/goal/updated`。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::agent::AnureoAcpAgent;
use crate::connection_registry::ConnectionRegistry;
use crate::extensions::{auth, ExtensionContext, ExtensionError, ExtensionHandler};

// ---------------------------------------------------------------------------
// Wire DTOs（旧 API 形状，保持 FE 兼容；serde 形状与 P5b 前逐字段一致）
// ---------------------------------------------------------------------------

/// 旧 6 态（wire 枚举）。仅用于投影/筛选；后端真值是 `goal::GoalStatus`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GoalStatus {
    Pending,
    Active,
    Paused,
    Completed,
    Cancelled,
    Failed,
}

impl GoalStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            GoalStatus::Pending => "pending",
            GoalStatus::Active => "active",
            GoalStatus::Paused => "paused",
            GoalStatus::Completed => "completed",
            GoalStatus::Cancelled => "cancelled",
            GoalStatus::Failed => "failed",
        }
    }
}

/// 新 6 态 → 旧 6 态投影：
/// - `usage_limited`（用户可恢复）→ `paused`；
/// - `budget_limited`（终态、非成功）→ `failed`（详情见 `metadata.statusReason`）；
/// - 旧 `cancelled` 在新模型中无对应（= clear，goal 行不存在）。
fn project_status(status: goal::GoalStatus) -> GoalStatus {
    match status {
        goal::GoalStatus::Active => GoalStatus::Active,
        goal::GoalStatus::Paused | goal::GoalStatus::UsageLimited => GoalStatus::Paused,
        goal::GoalStatus::Blocked | goal::GoalStatus::BudgetLimited => GoalStatus::Failed,
        goal::GoalStatus::Complete => GoalStatus::Completed,
    }
}

/// 旧状态筛选 → 新状态集（`list` 的 `status` 参数）。
fn filter_statuses(old: &str) -> Vec<goal::GoalStatus> {
    use goal::GoalStatus as S;
    match old.trim().to_lowercase().as_str() {
        "pending" | "active" => vec![S::Active],
        "paused" => vec![S::Paused, S::UsageLimited],
        "completed" => vec![S::Complete],
        "failed" => vec![S::Blocked, S::BudgetLimited],
        // 旧 `cancelled` 在新模型中 = clear（无 goal 行），永不匹配。
        "cancelled" => vec![],
        // 未知筛选值：不筛（宽松处理，与旧实现一致）。
        _ => vec![
            S::Active,
            S::Paused,
            S::Blocked,
            S::UsageLimited,
            S::BudgetLimited,
            S::Complete,
        ],
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoalProgress {
    pub completed_steps: u32,
    pub total_steps: u32,
    pub percentage: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_step: Option<String>,
    pub sessions_spawned: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStepStatus {
    Pending,
    #[serde(rename = "in_progress")]
    InProgress,
    Completed,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalStep {
    pub index: u32,
    pub description: String,
    pub status: GoalStepStatus,
}

/// 旧 wire Goal 形状（camelCase）。新后端字段投影进来；
/// `progress`/`steps` 恒空（新架构无 step 跟踪），`sessionIds` 投影 thread_id。
/// `#[serde(default)]` 保留：P6 迁移命令要反序列化旧 goals.json。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Goal {
    pub id: String,
    pub title: String,
    pub description: String,
    pub status: GoalStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub session_ids: Vec<String>,
    #[serde(default)]
    pub progress: Option<GoalProgress>,
    #[serde(default)]
    pub metadata: Option<Value>,
    #[serde(default, skip_serializing)]
    pub steps: Vec<GoalStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_directory: Option<String>,
}

/// 全量投影：`goal::Goal` → 旧 wire 形状。
fn project_goal(g: &goal::Goal) -> Goal {
    let objective = g.objective.trim();
    let title: String = objective
        .lines()
        .next()
        .unwrap_or(objective)
        .chars()
        .take(80)
        .collect();
    let mut metadata = json!({
        "threadId": g.thread_id,
        "tokensUsed": g.tokens_used,
        "timeUsedSeconds": g.time_used_seconds,
    });
    if let Some(b) = g.token_budget {
        metadata["tokenBudget"] = json!(b);
    }
    if let Some(reason) = &g.status_reason {
        metadata["statusReason"] = json!(reason);
    }
    Goal {
        id: g.goal_id.clone(),
        title,
        description: objective.to_string(),
        status: project_status(g.status),
        created_at: ms_to_iso(g.created_at_ms),
        updated_at: ms_to_iso(g.updated_at_ms),
        session_ids: vec![g.thread_id.clone()],
        progress: None,
        metadata: Some(metadata),
        steps: Vec::new(),
        idempotency_key: None,
        working_directory: None,
    }
}

fn ms_to_iso(ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(ms)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

// ---------------------------------------------------------------------------
// Notifications（旧 changed 形状 + 新 updated 快照）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GoalChangeType {
    Started,
    Paused,
    Resumed,
    Cancelled,
    Completed,
    Progress,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalChangedNotification {
    pub id: String,
    pub change: GoalChangeType,
    pub status: GoalStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<GoalProgress>,
}

fn build_notification(
    id: &str,
    change: GoalChangeType,
    status: GoalStatus,
    progress: Option<GoalProgress>,
) -> GoalChangedNotification {
    GoalChangedNotification {
        id: id.to_string(),
        change,
        status,
        progress,
    }
}

fn internal_error(msg: impl Into<String>) -> ExtensionError {
    ExtensionError {
        code: -32603,
        message: "internal_error".into(),
        data: Some(Value::String(msg.into())),
    }
}

// ---------------------------------------------------------------------------
// Handler
// ---------------------------------------------------------------------------

/// goal 后端绑定：
/// - `Agent`：生产路径（AcpRuntime 构造后 `bind`；共享 TaskDb + runtime 钩子）；
/// - `Store`：测试/嵌入式（直接给 store，无 runtime 钩子）；
/// - `Unbound`：尚未 bind（降级：list 空、mutation 报 internal_error）。
#[derive(Clone)]
enum GoalBackend {
    Unbound,
    Store(goal::GoalStore),
    Agent(Weak<AnureoAcpAgent>),
}

pub struct GoalHandler {
    backend: Mutex<GoalBackend>,
    connections: Option<Arc<ConnectionRegistry>>,
    /// idempotencyKey → goal_id（旧 API 幂等语义；进程内，重启即失）。
    idempotency: Mutex<HashMap<String, String>>,
    /// 分页游标 generation（每次 mutation +1，旧游标失效）。
    generation: Mutex<u64>,
}

impl GoalHandler {
    pub fn new() -> Self {
        Self {
            backend: Mutex::new(GoalBackend::Unbound),
            connections: None,
            idempotency: Mutex::new(HashMap::new()),
            generation: Mutex::new(0),
        }
    }

    pub fn with_connections(mut self, connections: Arc<ConnectionRegistry>) -> Self {
        self.connections = Some(connections);
        self
    }

    /// 生产绑定：AcpRuntime 在 `Arc::new(agent)` 之后调用（见 runtime.rs）。
    pub fn bind(&self, agent: &Arc<AnureoAcpAgent>) {
        *self.backend.lock().unwrap_or_else(|e| e.into_inner()) =
            GoalBackend::Agent(Arc::downgrade(agent));
    }

    /// 测试/嵌入式：直接绑定 store。
    pub fn bind_store(&self, store: goal::GoalStore) {
        *self.backend.lock().unwrap_or_else(|e| e.into_inner()) = GoalBackend::Store(store);
    }

    /// 取后端快照（std Mutex guard 不跨 await）。
    fn backend(&self) -> GoalBackend {
        self.backend.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// sessionId → thread_id（生产路径经 SessionStore 反查；
    /// 查不到时降级用 sessionId 本身作键并告警）。
    fn resolve_thread_key(&self, session_id: &str) -> String {
        if let GoalBackend::Agent(weak) = self.backend() {
            if let Some(agent) = weak.upgrade() {
                let sid = crate::session::SessionId::new(session_id.to_string());
                if let Some(entry) = agent.sessions().get(&sid) {
                    return entry.thread_id;
                }
                tracing::warn!(
                    session_id,
                    "goal extension: session not in store; using session id as thread key"
                );
            }
        }
        session_id.to_string()
    }

    /// 打开目标 thread 的（service, 可选 runtime 钩子）。
    async fn open(
        &self,
        thread_id: &str,
    ) -> Result<(goal::GoalService, Option<Arc<goal::GoalRuntimeHandle>>), ExtensionError> {
        match self.backend() {
            GoalBackend::Unbound => Err(internal_error(
                "goal backend not bound (degraded runtime); goal wiring disabled",
            )),
            GoalBackend::Store(store) => Ok((goal::GoalService::new(store), None)),
            GoalBackend::Agent(weak) => {
                let agent = weak
                    .upgrade()
                    .ok_or_else(|| internal_error("goal backend unavailable (agent dropped)"))?;
                let runtime = agent.goal_runtime_for(thread_id).await.ok_or_else(|| {
                    internal_error("goal backend unavailable (task db open failed?)")
                })?;
                Ok((runtime.service().clone(), Some(runtime)))
            }
        }
    }

    /// 只读路径的 store（list/get 与 mutation 前的 goal 反查）。
    async fn store_for_read(&self) -> Result<goal::GoalStore, ExtensionError> {
        match self.backend() {
            GoalBackend::Unbound => Err(internal_error("goal backend not bound")),
            GoalBackend::Store(store) => Ok(store),
            GoalBackend::Agent(weak) => {
                let agent = weak
                    .upgrade()
                    .ok_or_else(|| internal_error("goal backend unavailable (agent dropped)"))?;
                let db = agent
                    .goal_task_db()
                    .await
                    .map_err(internal_error)?;
                Ok(goal::GoalStore::from_task_db(&db))
            }
        }
    }

    fn bump_generation(&self) {
        *self.generation.lock().unwrap_or_else(|e| e.into_inner()) += 1;
    }

    /// 广播 `_anureo.dev/goal/changed`（旧）与 `_anureo.dev/goal/updated`（新）
    /// 到所有连接。best-effort，不阻塞 mutation。
    fn broadcast(&self, changed_params: Value, updated_params: Value) {
        let Some(connections) = self.connections.clone() else {
            return;
        };
        tokio::spawn(async move {
            connections
                .broadcast_extension_notification("_anureo.dev/goal/changed", changed_params)
                .await;
            connections
                .broadcast_extension_notification("_anureo.dev/goal/updated", updated_params)
                .await;
        });
    }

    fn require_param_str(params: &Value, key: &str) -> Result<String, ExtensionError> {
        match params.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.trim().to_string()),
            Some(Value::String(_)) => Err(ExtensionError::invalid_params(format!(
                "{key} must not be empty"
            ))),
            Some(_) => Err(ExtensionError::invalid_params(format!(
                "{key} must be a string"
            ))),
            None => Err(ExtensionError::invalid_params(format!(
                "missing required parameter: {key}"
            ))),
        }
    }

    fn optional_param_str(params: &Value, key: &str) -> Option<String> {
        params
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// service 错误 → 旧 API 错误码（mutation 路径通用）。
    fn map_service_error(op: &str, e: goal::GoalServiceError) -> ExtensionError {
        match e {
            goal::GoalServiceError::NoGoal(thread) => {
                ExtensionError::not_found(format!("goal on thread {thread} not found"))
            }
            goal::GoalServiceError::NotResumable { status } => ExtensionError::invalid_params(
                format!("goal status is {status:?}; {op} rejected"),
            ),
            goal::GoalServiceError::Store(store_err) => match store_err {
                goal::GoalStoreError::NotFoundOrDisallowed(_) => ExtensionError::invalid_params(
                    format!("goal status transition rejected by store; {op} failed"),
                ),
                other => internal_error(format!("{op} failed: {other}")),
            },
            other => internal_error(format!("{op} failed: {other}")),
        }
    }
}

impl Default for GoalHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// 每请求打开的（service + 可选 runtime 钩子）。
struct GoalSession {
    service: goal::GoalService,
    runtime: Option<Arc<goal::GoalRuntimeHandle>>,
}

impl GoalSession {
    /// mutation 后的 runtime 钩子（与 agent.rs `/goal` 路径对齐）；错误只记日志。
    async fn after_start(&self, goal_id: &str) {
        if let Some(rt) = &self.runtime {
            rt.note_goal_armed(goal_id).await;
        }
    }

    async fn after_pause(&self) {
        if let Some(rt) = &self.runtime {
            if let Err(e) = rt.on_goal_status_changed(goal::GoalStatus::Paused).await {
                tracing::warn!(error = %e, "goal extension: on_goal_status_changed(Paused) failed");
            }
        }
    }

    async fn after_resume(&self) {
        if let Some(rt) = &self.runtime {
            if let Err(e) = rt.on_goal_status_changed(goal::GoalStatus::Active).await {
                tracing::warn!(error = %e, "goal extension: on_goal_status_changed(Active) failed");
            }
            // resume 后自动续跑（对齐旧 detached runner 的 resume 语义）。
            let cont = rt.clone();
            tokio::spawn(async move {
                if let Err(e) = cont.continue_if_idle().await {
                    tracing::warn!(error = %e, "goal extension: idle continuation failed");
                }
            });
        }
    }

    async fn after_clear(&self) {
        if let Some(rt) = &self.runtime {
            rt.on_goal_replaced(None, goal::TokenTotals::default()).await;
        }
    }
}

#[async_trait]
impl ExtensionHandler for GoalHandler {
    async fn handle(
        &self,
        method: &str,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        match method {
            "list" => self.handle_list(params, ctx).await,
            "get" => self.handle_get(params, ctx).await,
            "start" => self.handle_start(params, ctx).await,
            "pause" => self.handle_pause(params, ctx).await,
            "resume" => self.handle_resume(params, ctx).await,
            "cancel" => self.handle_cancel(params, ctx).await,
            _ => Err(ExtensionError::method_not_found()),
        }
    }

    fn capabilities(&self) -> Value {
        // 旧扁平形状（FE 依赖），保持逐字段一致。
        json!({
            "list": true,
            "get": true,
            "start": true,
            "pause": true,
            "resume": true,
            "cancel": true,
        })
    }
}

impl GoalHandler {
    // ── list / get ──────────────────────────────────────────────────────

    async fn handle_list(
        &self,
        params: Value,
        _ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        if matches!(self.backend(), GoalBackend::Unbound) {
            // 与旧行为一致：无 store 时返回空列表而非报错。
            return Ok(json!({"items": [], "nextCursor": null, "hasMore": false}));
        }
        let status_filter = Self::optional_param_str(&params, "status");
        let limit = params
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v.clamp(1, 200) as usize)
            .unwrap_or(50);
        let cursor_offset = match Self::optional_param_str(&params, "cursor") {
            Some(cursor) => decode_cursor(&cursor)?,
            None => 0,
        };

        let store = self.store_for_read().await?;
        let allowed: Option<Vec<goal::GoalStatus>> =
            status_filter.as_deref().map(filter_statuses);
        let all = store
            .list_all()
            .await
            .map_err(|e| internal_error(format!("goal list failed: {e}")))?;
        let filtered: Vec<goal::Goal> = all
            .into_iter()
            .filter(|(_, g)| allowed.as_ref().is_none_or(|a| a.contains(&g.status)))
            .map(|(_, g)| g)
            .collect();

        let total = filtered.len();
        let end = (cursor_offset + limit).min(total);
        let items: Vec<Value> = filtered[cursor_offset.min(total)..end]
            .iter()
            .map(|g| serde_json::to_value(project_goal(g)).unwrap_or(Value::Null))
            .collect();
        let has_more = end < total;
        let next_cursor = if has_more {
            let gen = *self.generation.lock().unwrap_or_else(|e| e.into_inner());
            Some(encode_cursor(gen, end))
        } else {
            None
        };
        Ok(json!({
            "items": items,
            "nextCursor": next_cursor,
            "hasMore": has_more,
        }))
    }

    async fn handle_get(
        &self,
        params: Value,
        _ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        let id = Self::require_param_str(&params, "id")?;
        if matches!(self.backend(), GoalBackend::Unbound) {
            return Err(ExtensionError::not_found(format!("goal '{id}' not found")));
        }
        let store = self.store_for_read().await?;
        let (_, goal) = store
            .find_by_goal_id(&id)
            .await
            .map_err(|e| internal_error(format!("goal get failed: {e}")))?
            .ok_or_else(|| ExtensionError::not_found(format!("goal '{id}' not found")))?;
        let mut projected =
            serde_json::to_value(project_goal(&goal)).unwrap_or(Value::Null);
        // 旧 get 响应含 steps（新后端无 step 跟踪，恒空数组）。
        if let Some(obj) = projected.as_object_mut() {
            obj.insert("steps".to_string(), json!([]));
        }
        Ok(projected)
    }

    // ── start ───────────────────────────────────────────────────────────

    async fn handle_start(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        auth::check_server_policy(ctx, "goal", "start")?;

        let title = Self::require_param_str(&params, "title")?;
        let description = Self::require_param_str(&params, "description")?;
        let session_id = Self::optional_param_str(&params, "sessionId")
            .or_else(|| ctx.session_id.clone())
            .ok_or_else(|| {
                // 兼容性偏离：旧实现允许无 sessionId 的 goal（仅落 JSON 记录）；
                // 新后端以 thread 为键，必须能定位 thread（见 P5b 报告）。
                ExtensionError::invalid_params("missing required parameter: sessionId")
            })?;
        let idempotency_key = Self::optional_param_str(&params, "idempotencyKey");

        let thread_id = self.resolve_thread_key(&session_id);
        let (service, runtime) = self.open(&thread_id).await?;
        let session = GoalSession {
            service,
            runtime,
        };

        // 幂等：同 key 已有 goal 且仍存在 → 返回既有。
        if let Some(key) = &idempotency_key {
            let existing_id = self
                .idempotency
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(key)
                .cloned();
            if let Some(gid) = existing_id {
                if let Ok(Some((_, goal))) = session.service.store().find_by_goal_id(&gid).await {
                    let projected = project_goal(&goal);
                    return Ok(json!({
                        "id": projected.id,
                        "title": projected.title,
                        "status": projected.status.as_str(),
                        "sessionId": session_id,
                        "createdAt": projected.created_at,
                        "notification": build_notification(
                            &projected.id,
                            GoalChangeType::Started,
                            projected.status,
                            None,
                        ),
                        "updated": projected,
                    }));
                }
            }
        }

        // objective = title + description 合成（title 与 description 相同时取一）。
        let objective = if description == title {
            title.clone()
        } else {
            format!("{title}: {description}")
        };

        let created = session
            .service
            .set_with_verify(&thread_id, &objective, None, None)
            .await
            .map_err(|e| Self::map_service_error("start", e))?;
        session.after_start(&created.goal_id).await;
        self.bump_generation();
        if let Some(key) = &idempotency_key {
            self.idempotency
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.clone(), created.goal_id.clone());
        }

        let projected = project_goal(&created);
        let notification =
            build_notification(&projected.id, GoalChangeType::Started, projected.status, None);
        let response = json!({
            "id": projected.id,
            "title": projected.title,
            "status": projected.status.as_str(),
            "sessionId": session_id,
            "createdAt": projected.created_at,
            "notification": notification,
            "updated": projected,
        });
        let changed = serde_json::to_value(&notification).unwrap_or(Value::Null);
        let updated = response["updated"].clone();
        self.broadcast(changed, updated);
        Ok(response)
    }

    // ── pause / resume / cancel ─────────────────────────────────────────

    /// 以旧 goal-id 反查（thread_id, goal, GoalSession）。
    async fn resolve_goal_by_id(
        &self,
        id: &str,
    ) -> Result<(String, goal::Goal, GoalSession), ExtensionError> {
        if matches!(self.backend(), GoalBackend::Unbound) {
            return Err(ExtensionError::not_found(format!("goal '{id}' not found")));
        }
        let store = self.store_for_read().await?;
        let (thread_id, goal) = store
            .find_by_goal_id(id)
            .await
            .map_err(|e| internal_error(format!("goal lookup failed: {e}")))?
            .ok_or_else(|| ExtensionError::not_found(format!("goal '{id}' not found")))?;
        let (service, runtime) = self.open(&thread_id).await?;
        Ok((
            thread_id,
            goal,
            GoalSession {
                service,
                runtime,
            },
        ))
    }

    async fn handle_pause(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        auth::check_server_policy(ctx, "goal", "pause")?;
        let id = Self::require_param_str(&params, "id")?;
        let (thread_id, _existing, session) = self.resolve_goal_by_id(&id).await?;

        let paused = session
            .service
            .pause(&thread_id)
            .await
            .map_err(|e| Self::map_service_error("pause", e))?;
        session.after_pause().await;
        self.bump_generation();

        let projected = project_goal(&paused);
        let notification =
            build_notification(&id, GoalChangeType::Paused, projected.status, None);
        let response = json!({
            "id": id,
            "status": "paused",
            "pausedAt": projected.updated_at,
            "notification": notification,
            "updated": projected,
        });
        let changed = serde_json::to_value(&notification).unwrap_or(Value::Null);
        let updated = response["updated"].clone();
        self.broadcast(changed, updated);
        Ok(response)
    }

    async fn handle_resume(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        auth::check_server_policy(ctx, "goal", "resume")?;
        let id = Self::require_param_str(&params, "id")?;
        let (thread_id, _existing, session) = self.resolve_goal_by_id(&id).await?;

        let resumed = session
            .service
            .resume(&thread_id)
            .await
            .map_err(|e| Self::map_service_error("resume", e))?;
        session.after_resume().await;
        self.bump_generation();

        let projected = project_goal(&resumed);
        let notification =
            build_notification(&id, GoalChangeType::Resumed, projected.status, None);
        let response = json!({
            "id": id,
            "status": "active",
            "resumedAt": projected.updated_at,
            "notification": notification,
            "updated": projected,
        });
        let changed = serde_json::to_value(&notification).unwrap_or(Value::Null);
        let updated = response["updated"].clone();
        self.broadcast(changed, updated);
        Ok(response)
    }

    async fn handle_cancel(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        auth::check_server_policy(ctx, "goal", "cancel")?;
        let id = Self::require_param_str(&params, "id")?;
        let reason = Self::optional_param_str(&params, "reason");
        let (thread_id, existing, session) = self.resolve_goal_by_id(&id).await?;

        let now = ms_to_iso(chrono::Utc::now().timestamp_millis());
        // 幂等：终态 goal 再次 cancel 不报错、不清行（新模型终态行保留）。
        if existing.status.is_terminal() {
            let notification =
                build_notification(&id, GoalChangeType::Cancelled, GoalStatus::Cancelled, None);
            return Ok(json!({
                "id": id,
                "status": "cancelled",
                "cancelledAt": now,
                "notification": notification,
                "updated": project_goal(&existing),
            }));
        }

        // 广播用快照必须在 clear 前取。
        let mut snapshot = project_goal(&existing);
        if let Some(r) = reason {
            let mut meta = snapshot.metadata.take().unwrap_or(json!({}));
            if let Some(obj) = meta.as_object_mut() {
                obj.insert("cancellationReason".to_string(), Value::String(r));
            }
            snapshot.metadata = Some(meta);
        }

        session
            .service
            .clear(&thread_id)
            .await
            .map_err(|e| Self::map_service_error("cancel", e))?;
        session.after_clear().await;
        self.bump_generation();

        let notification =
            build_notification(&id, GoalChangeType::Cancelled, GoalStatus::Cancelled, None);
        let response = json!({
            "id": id,
            "status": "cancelled",
            "cancelledAt": now,
            "notification": notification,
            "updated": snapshot,
        });
        let changed = serde_json::to_value(&notification).unwrap_or(Value::Null);
        let updated = response["updated"].clone();
        self.broadcast(changed, updated);
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Cursor（不透明 base64 JSON；generation 使旧游标在 mutation 后失效）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ListCursor {
    generation: u64,
    offset: usize,
}

fn encode_cursor(generation: u64, offset: usize) -> String {
    use base64::Engine;
    let raw = serde_json::to_vec(&ListCursor { generation, offset }).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
}

fn decode_cursor(cursor: &str) -> Result<usize, ExtensionError> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| ExtensionError::invalid_params("invalid list cursor"))?;
    let parsed: ListCursor = serde_json::from_slice(&raw)
        .map_err(|_| ExtensionError::invalid_params("invalid list cursor"))?;
    Ok(parsed.offset)
}

// ---------------------------------------------------------------------------
// Tests（temp TaskDb 后端；goals.json/recovery 类测试已随旧机制退役）
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_capabilities::ClientCapabilitiesInfo;
    use std::path::PathBuf;

    fn make_ctx(wd: PathBuf, session_id: Option<&str>) -> ExtensionContext {
        ExtensionContext {
            session_id: session_id.map(str::to_string),
            principal: "test-user".to_string(),
            connection_id: "test-conn".to_string(),
            working_directory: Some(wd),
            client_capabilities: ClientCapabilitiesInfo::default(),
        }
    }

    fn make_ctx_no_principal(wd: PathBuf, session_id: Option<&str>) -> ExtensionContext {
        ExtensionContext {
            principal: String::new(),
            ..make_ctx(wd, session_id)
        }
    }

    async fn make_handler() -> (GoalHandler, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .unwrap();
        let handler = GoalHandler::new();
        handler.bind_store(goal::GoalStore::from_task_db(&db));
        (handler, dir)
    }

    async fn start_goal(handler: &GoalHandler, ctx: &ExtensionContext, session: &str, title: &str) -> Value {
        handler
            .handle(
                "start",
                json!({"title": title, "description": title, "sessionId": session}),
                ctx,
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn list_empty_when_no_goals() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), None);
        let result = handler.handle("list", json!({}), &ctx).await.unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 0);
        assert_eq!(result["hasMore"], false);
    }

    #[tokio::test]
    async fn unbound_handler_degrades_to_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = make_ctx(dir.path().to_path_buf(), None);
        let handler = GoalHandler::new();
        let result = handler.handle("list", json!({}), &ctx).await.unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn start_persists_to_thread_and_projects_old_shape() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("session-1"));
        let started = start_goal(&handler, &ctx, "session-1", "do work").await;
        assert_eq!(started["status"], "active");
        assert_eq!(started["sessionId"], "session-1");
        assert!(started["notification"]["change"] == "started");
        assert!(started["updated"]["id"].is_string());

        let listed = handler.handle("list", json!({}), &ctx).await.unwrap();
        assert_eq!(listed["items"].as_array().unwrap().len(), 1);
        assert_eq!(listed["items"][0]["status"], "active");
        assert_eq!(listed["items"][0]["sessionIds"][0], "session-1");
        assert_eq!(
            listed["items"][0]["metadata"]["threadId"], "session-1",
            "store-bound mode falls back to session id as thread key"
        );
        // camelCase wire 形状（FE 兼容）
        assert!(listed["items"][0].get("created_at").is_none());
        assert!(listed["items"][0]["createdAt"].is_string());
    }

    #[tokio::test]
    async fn start_requires_session_id() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), None);
        let err = handler
            .handle("start", json!({"title": "A", "description": "d"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[tokio::test]
    async fn start_rejects_empty_title() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        let err = handler
            .handle(
                "start",
                json!({"title": "  ", "description": "d", "sessionId": "s"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[tokio::test]
    async fn start_rejects_missing_description() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        let err = handler
            .handle("start", json!({"title": "T", "sessionId": "s"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[tokio::test]
    async fn start_no_principal_returns_forbidden() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx_no_principal(dir.path().to_path_buf(), Some("s"));
        let err = handler
            .handle(
                "start",
                json!({"title": "T", "description": "D", "sessionId": "s"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, -32002);
    }

    #[tokio::test]
    async fn idempotency_key_returns_existing_goal() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        let first = handler
            .handle(
                "start",
                json!({"title": "A", "description": "d", "sessionId": "s", "idempotencyKey": "k1"}),
                &ctx,
            )
            .await
            .unwrap();
        let second = handler
            .handle(
                "start",
                json!({"title": "B", "description": "d2", "sessionId": "s", "idempotencyKey": "k1"}),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(first["id"], second["id"]);
        assert_eq!(
            second["title"], "A: d",
            "幂等重试不得替换 goal（title 为首次合成 objective 的首行投影）"
        );
    }

    #[tokio::test]
    async fn get_by_goal_id_roundtrip() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        let started = start_goal(&handler, &ctx, "s", "objective text").await;
        let id = started["id"].as_str().unwrap().to_string();
        let got = handler.handle("get", json!({"id": id}), &ctx).await.unwrap();
        assert_eq!(got["id"], id);
        assert_eq!(got["status"], "active");
        assert_eq!(got["description"], "objective text");
        assert!(got["steps"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_unknown_goal_is_not_found() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), None);
        let err = handler
            .handle("get", json!({"id": "missing"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32003);
    }

    #[tokio::test]
    async fn get_requires_id_param() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), None);
        let err = handler.handle("get", json!({}), &ctx).await.unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[tokio::test]
    async fn pause_resume_cycle_maps_to_old_statuses() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        let started = start_goal(&handler, &ctx, "s", "work").await;
        let id = started["id"].as_str().unwrap().to_string();

        let paused = handler.handle("pause", json!({"id": id}), &ctx).await.unwrap();
        assert_eq!(paused["status"], "paused");
        assert!(paused["notification"]["change"] == "paused");

        // 旧 API：非 active 状态 pause 报 invalid_params
        let err = handler
            .handle("pause", json!({"id": id}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32602);

        let resumed = handler.handle("resume", json!({"id": id}), &ctx).await.unwrap();
        assert_eq!(resumed["status"], "active");
        assert!(resumed["resumedAt"].is_string());

        // 旧 API：active 状态 resume 报 invalid_params
        let err = handler
            .handle("resume", json!({"id": id}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[tokio::test]
    async fn pause_nonexistent_returns_not_found() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), None);
        let err = handler
            .handle("pause", json!({"id": "nope"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32003);
    }

    #[tokio::test]
    async fn pause_no_principal_returns_forbidden() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx_no_principal(dir.path().to_path_buf(), None);
        let err = handler
            .handle("pause", json!({"id": "x"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32002);
    }

    #[tokio::test]
    async fn cancel_clears_goal_and_reports_reason() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        let started = start_goal(&handler, &ctx, "s", "work").await;
        let id = started["id"].as_str().unwrap().to_string();

        let cancelled = handler
            .handle("cancel", json!({"id": id, "reason": "user asked"}), &ctx)
            .await
            .unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert!(cancelled["cancelledAt"].is_string());
        assert!(
            cancelled["updated"]["metadata"]["cancellationReason"]
                .as_str()
                .unwrap()
                .contains("user asked")
        );

        // 新模型 clear 即无行：重复 cancel 视为对不存在 goal 的操作。
        let err = handler
            .handle("cancel", json!({"id": id}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32003);

        let listed = handler.handle("list", json!({}), &ctx).await.unwrap();
        assert_eq!(listed["items"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn cancel_no_principal_returns_forbidden() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx_no_principal(dir.path().to_path_buf(), None);
        let err = handler
            .handle("cancel", json!({"id": "x"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32002);
    }

    #[tokio::test]
    async fn cancel_terminal_goal_is_idempotent() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        let started = start_goal(&handler, &ctx, "s", "work").await;
        let id = started["id"].as_str().unwrap().to_string();

        // 直接经 store 推到终态 complete。
        {
            let store = handler.store_for_read().await.unwrap();
            let (thread, g) = store.find_by_goal_id(&id).await.unwrap().unwrap();
            store.mark_complete(&thread, &g.goal_id).await.unwrap();
        }
        let cancelled = handler
            .handle("cancel", json!({"id": id}), &ctx)
            .await
            .unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        // 终态行保留：get 仍可读（投影 completed）。
        let got = handler.handle("get", json!({"id": id}), &ctx).await.unwrap();
        assert_eq!(got["status"], "completed");
    }

    #[tokio::test]
    async fn list_status_filter_maps_old_to_new() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        start_goal(&handler, &ctx, "s", "A").await;

        let result = handler
            .handle("list", json!({"status": "active"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 1);
        let result = handler
            .handle("list", json!({"status": "paused"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 0);
        let result = handler
            .handle("list", json!({"status": "cancelled"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 0, "cancelled 永不匹配");
    }

    #[tokio::test]
    async fn list_status_filter_case_insensitive() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        start_goal(&handler, &ctx, "s", "A").await;
        let result = handler
            .handle("list", json!({"status": "Active"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result["items"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn list_pagination_no_duplicates() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), None);
        for i in 0..3 {
            let ctx_i = make_ctx(dir.path().to_path_buf(), Some(&format!("s{i}")));
            handler
                .handle(
                    "start",
                    json!({"title": format!("G{i}"), "description": "d", "sessionId": format!("s{i}")}),
                    &ctx_i,
                )
                .await
                .unwrap();
        }
        let page1 = handler.handle("list", json!({"limit": 1}), &ctx).await.unwrap();
        assert_eq!(page1["items"].as_array().unwrap().len(), 1);
        assert_eq!(page1["hasMore"], true);
        let cursor = page1["nextCursor"].as_str().unwrap().to_string();

        let page2 = handler
            .handle("list", json!({"limit": 1, "cursor": cursor}), &ctx)
            .await
            .unwrap();
        assert_eq!(page2["items"].as_array().unwrap().len(), 1);
        let id1 = page1["items"][0]["id"].as_str().unwrap();
        let id2 = page2["items"][0]["id"].as_str().unwrap();
        assert_ne!(id1, id2, "分页不得重复");
    }

    #[tokio::test]
    async fn invalid_cursor_is_rejected() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), None);
        let err = handler
            .handle("list", json!({"cursor": "not-base64!!"}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32602);
    }

    #[tokio::test]
    async fn unknown_method_returns_method_not_found() {
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), None);
        let err = handler
            .handle("frobnicate", json!({}), &ctx)
            .await
            .unwrap_err();
        assert_eq!(err.code, -32601);
    }

    #[tokio::test]
    async fn capabilities_keep_legacy_flat_shape() {
        let handler = GoalHandler::new();
        let caps = handler.capabilities();
        for m in ["list", "get", "start", "pause", "resume", "cancel"] {
            assert_eq!(caps[m], true, "capability {m}");
        }
    }

    #[tokio::test]
    async fn state_persists_across_handler_rebuild() {
        // 新后端下「重建 handler」仍可见同一 goal（DB 持久，替代旧
        // goals.json reload 语义）。
        let (handler, dir) = make_handler().await;
        let ctx = make_ctx(dir.path().to_path_buf(), Some("s"));
        let started = start_goal(&handler, &ctx, "s", "T").await;
        let id = started["id"].as_str().unwrap().to_string();
        handler
            .handle("pause", json!({"id": id}), &ctx)
            .await
            .unwrap();

        // 从同一 db 构造新 handler（模拟进程重启）。
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .unwrap();
        let handler2 = GoalHandler::new();
        handler2.bind_store(goal::GoalStore::from_task_db(&db));
        let goal = handler2.handle("get", json!({"id": id}), &ctx).await.unwrap();
        assert_eq!(goal["status"], "paused");
    }

    #[test]
    fn status_projection_matrix() {
        use goal::GoalStatus as S;
        assert_eq!(project_status(S::Active), GoalStatus::Active);
        assert_eq!(project_status(S::Paused), GoalStatus::Paused);
        assert_eq!(project_status(S::UsageLimited), GoalStatus::Paused);
        assert_eq!(project_status(S::Blocked), GoalStatus::Failed);
        assert_eq!(project_status(S::BudgetLimited), GoalStatus::Failed);
        assert_eq!(project_status(S::Complete), GoalStatus::Completed);
    }

    #[test]
    fn goal_wire_shape_is_camel_case() {
        let g = goal::Goal {
            thread_id: "t1".into(),
            goal_id: "g1".into(),
            objective: "Fix the login bug\nmore context".into(),
            status: goal::GoalStatus::Active,
            token_budget: Some(1000),
            tokens_used: 100,
            time_used_seconds: 42,
            verify_command: None,
            status_reason: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let projected = project_goal(&g);
        let v = serde_json::to_value(&projected).unwrap();
        assert!(v.get("sessionId").is_some() || v.get("sessionIds").is_some());
        assert!(v["created_at"].is_null() || v.get("created_at").is_none());
        assert_eq!(v["createdAt"], "1970-01-01T00:00:00.000Z");
        assert_eq!(v["title"], "Fix the login bug");
        assert_eq!(v["metadata"]["threadId"], "t1");
        assert_eq!(v["metadata"]["tokenBudget"], 1000);
        assert!(v.get("steps").is_none(), "list 投影不带 steps");
    }
}

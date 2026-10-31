//! `_anureo.dev/review/*` Background Review v2 control/query surface.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use agent::run::ResolvedModelConfig;
use anureo_curator::ReviewHistory;

use crate::connection_registry::ConnectionRegistry;
use crate::review_diff::project_actions;
use crate::review_runner::{
    self, CancelReviewError, ReviewChangedCallback, ReviewLaunch, ReviewState, ReviewStateStatus,
};
use crate::session::SessionId as StoreSessionId;
use crate::session_config_store::SessionConfigStore;
use crate::session_repository::{SessionMetadata, SessionRepository};
use crate::stream_bridge::{SessionNotifier, SessionUpdateEnvelope};

use super::{ExtensionContext, ExtensionError, ExtensionHandler};

const CHANGED_METHOD: &str = "_anureo.dev/review/changed";

#[derive(Clone)]
struct ReviewRuntime {
    db_path: PathBuf,
    session_updates: Option<mpsc::UnboundedSender<SessionUpdateEnvelope>>,
}

pub struct ReviewHandler {
    runtime: RwLock<Option<ReviewRuntime>>,
    connections: Option<Arc<ConnectionRegistry>>,
}

impl ReviewHandler {
    pub fn new(connections: Option<Arc<ConnectionRegistry>>) -> Self {
        Self {
            runtime: RwLock::new(None),
            connections,
        }
    }

    /// Late binding from `AnureoAcpAgent`: registration happens before the
    /// agent chooses its SQLite path and session-update channel.
    pub fn configure(
        &self,
        db_path: PathBuf,
        session_updates: Option<mpsc::UnboundedSender<SessionUpdateEnvelope>>,
    ) -> Result<(), String> {
        review_runner::ensure_running_schema(&db_path)?;
        let recovered = review_runner::recover_stale_reviews(&db_path)?;
        if recovered > 0 {
            tracing::warn!(recovered, "recovered stale running background reviews");
        }
        *self.runtime.write().unwrap_or_else(|e| e.into_inner()) = Some(ReviewRuntime {
            db_path,
            session_updates,
        });
        Ok(())
    }

    fn runtime(&self) -> Result<ReviewRuntime, ExtensionError> {
        self.runtime
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| Self::internal("review runtime is not configured"))
    }

    fn internal(message: impl Into<String>) -> ExtensionError {
        ExtensionError {
            code: -32603,
            message: "internal_error".into(),
            data: Some(Value::String(message.into())),
        }
    }

    fn parse<T: for<'de> Deserialize<'de>>(params: Value) -> Result<T, ExtensionError> {
        serde_json::from_value(params)
            .map_err(|error| ExtensionError::invalid_params(error.to_string()))
    }

    fn owned_session(
        runtime: &ReviewRuntime,
        principal: &str,
        session_id: &str,
    ) -> Result<SessionMetadata, ExtensionError> {
        if principal.trim().is_empty() {
            return Err(ExtensionError::forbidden(
                "authenticated principal required",
            ));
        }
        let repository = SessionRepository::new(&runtime.db_path)
            .map_err(|error| Self::internal(format!("open session repository: {error}")))?;
        let metadata = repository
            .get(session_id)
            .map_err(|error| Self::internal(format!("read session: {error}")))?
            .ok_or_else(|| ExtensionError::not_found("session not found"))?;
        if metadata.owner_principal != principal {
            return Err(ExtensionError::forbidden(
                "session is owned by another principal",
            ));
        }
        Ok(metadata)
    }

    fn changed_callback(&self) -> Option<ReviewChangedCallback> {
        let connections = self.connections.clone()?;
        Some(Arc::new(move |session_id, owner_principal, state| {
            let mut params = serde_json::to_value(state)
                .expect("ReviewState is serializable")
                .as_object()
                .cloned()
                .unwrap_or_default();
            params.insert("sessionId".into(), Value::String(session_id.to_string()));
            connections.notify_principal(owner_principal, CHANGED_METHOD, Value::Object(params));
        }))
    }

    fn launch(
        &self,
        runtime: &ReviewRuntime,
        metadata: &SessionMetadata,
        resolved: ResolvedModelConfig,
        scope: &str,
        trigger: &str,
    ) -> Result<review_runner::ReviewStart, String> {
        let scope = Some(scope.to_string());
        let (review_memory, review_skills) = review_runner::scope_to_review_config(&scope);
        review_runner::spawn_inprocess_review(ReviewLaunch {
            db_path: runtime.db_path.clone(),
            acp_session_id: metadata.session_id.clone(),
            thread_id: metadata.thread_id.clone(),
            owner_principal: metadata.owner_principal.clone(),
            resolved,
            review_memory,
            review_skills,
            trigger: trigger.to_string(),
            tx: runtime.session_updates.clone(),
            changed: self.changed_callback(),
        })
    }

    /// Entry used by post-prompt and `/review-skill`, preserving their model
    /// resolution while sharing the same v2 state machine as `review/start`.
    pub fn start_for_agent(
        &self,
        session_id: &str,
        thread_id: &str,
        owner_principal: &str,
        resolved: ResolvedModelConfig,
        scope: &str,
        trigger: &str,
    ) -> Result<review_runner::ReviewStart, String> {
        let runtime = self
            .runtime
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .ok_or_else(|| "review runtime is not configured".to_string())?;
        let metadata = SessionMetadata {
            session_id: session_id.to_string(),
            thread_id: thread_id.to_string(),
            owner_principal: owner_principal.to_string(),
            cwd: PathBuf::new(),
            lifecycle: "active".into(),
            title: None,
            updated_at: None,
            created_at: None,
            archived_at: None,
        };
        self.launch(&runtime, &metadata, resolved, scope, trigger)
    }

    fn publish_cancelled(
        &self,
        runtime: &ReviewRuntime,
        metadata: &SessionMetadata,
        state: &ReviewState,
    ) {
        if let Some(tx) = runtime.session_updates.as_ref() {
            SessionNotifier::new(
                tx.clone(),
                agent_client_protocol::schema::v1::SessionId::new(metadata.session_id.clone()),
            )
            .try_send_session_meta(state.to_meta());
        }
        if let Some(callback) = self.changed_callback() {
            callback(&metadata.session_id, &metadata.owner_principal, state);
        }
    }

    fn owned_thread_ids(
        runtime: &ReviewRuntime,
        principal: &str,
    ) -> Result<HashSet<String>, ExtensionError> {
        let connection =
            checkpoint_sqlite_store::sqlite_util::open_sqlite_with_wal(&runtime.db_path)
                .map_err(Self::internal)?;
        let mut statement = connection
            .prepare("SELECT thread_id FROM acp_sessions WHERE owner_principal = ?1")
            .map_err(|error| Self::internal(error.to_string()))?;
        let rows = statement
            .query_map([principal], |row| row.get::<_, String>(0))
            .map_err(|error| Self::internal(error.to_string()))?;
        let mut ids = HashSet::new();
        for row in rows {
            ids.insert(row.map_err(|error| Self::internal(error.to_string()))?);
        }
        Ok(ids)
    }

    async fn status(&self, params: Value, ctx: &ExtensionContext) -> Result<Value, ExtensionError> {
        let request: SessionRequest = Self::parse(params)?;
        let runtime = self.runtime()?;
        let metadata = Self::owned_session(&runtime, &ctx.principal, &request.session_id)?;
        let latest = review_runner::latest_review_state(&runtime.db_path, &metadata.thread_id)
            .map_err(Self::internal)?;
        let active = latest
            .as_ref()
            .is_some_and(|state| state.status == ReviewStateStatus::Running);
        Ok(json!({ "sessionId": request.session_id, "active": active, "latest": latest }))
    }

    async fn history(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        let request: HistoryRequest = Self::parse(params)?;
        if ctx.principal.trim().is_empty() {
            return Err(ExtensionError::forbidden(
                "authenticated principal required",
            ));
        }
        let runtime = self.runtime()?;
        let cursor = parse_i64_cursor(request.cursor.as_deref())?;
        let limit = request.limit.unwrap_or(50).clamp(1, 200);
        let history = ReviewHistory::with_db_path(runtime.db_path.clone());
        let page = if let Some(session_id) = request.session_id.as_deref() {
            let metadata = Self::owned_session(&runtime, &ctx.principal, session_id)?;
            history
                .list_page(Some(&metadata.thread_id), cursor, limit)
                .map_err(Self::internal)?
        } else {
            let owned = Self::owned_thread_ids(&runtime, &ctx.principal)?;
            let mut scan_cursor = cursor;
            let mut items = Vec::new();
            let mut next_cursor = None;
            while items.len() < limit {
                let page = history
                    // Scan one durable row at a time so an owner-filtered
                    // cursor never skips unconsumed rows from a larger page.
                    .list_page(None, scan_cursor, 1)
                    .map_err(Self::internal)?;
                items.extend(
                    page.items
                        .into_iter()
                        .filter(|item| owned.contains(&item.record.session_id)),
                );
                next_cursor = page.next_cursor;
                if next_cursor.is_none() {
                    break;
                }
                scan_cursor = next_cursor;
            }
            anureo_curator::ReviewHistoryPage { items, next_cursor }
        };
        let items: Vec<ReviewState> = page
            .items
            .iter()
            .map(ReviewState::from_status_record)
            .collect();
        Ok(json!({
            "items": items,
            "nextCursor": page.next_cursor.map(|cursor| cursor.to_string())
        }))
    }

    async fn details(
        &self,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        let request: DetailsRequest = Self::parse(params)?;
        let runtime = self.runtime()?;
        let metadata = Self::owned_session(&runtime, &ctx.principal, &request.session_id)?;
        let cursor = parse_usize_cursor(request.cursor.as_deref())?;
        let page = ReviewHistory::with_db_path(runtime.db_path.clone())
            .details(
                &metadata.thread_id,
                &request.review_id,
                cursor,
                request.limit.unwrap_or(50),
            )
            .map_err(Self::internal)?
            .ok_or_else(|| ExtensionError::not_found("review not found"))?;
        let actions = project_actions(
            &runtime.db_path,
            &request.review_id,
            &page.actions,
            request.include_diff.unwrap_or(false),
        )
        .map_err(Self::internal)?;
        Ok(json!({
            "reviewId": page.review_id,
            "actions": actions,
            "nextCursor": page.next_cursor.map(|cursor| cursor.to_string()),
            "actionsTruncated": page.next_cursor.is_some(),
            "legacy": page.legacy
        }))
    }

    async fn start(&self, params: Value, ctx: &ExtensionContext) -> Result<Value, ExtensionError> {
        let request: StartRequest = Self::parse(params)?;
        if request.trigger.as_deref().unwrap_or("manual") != "manual" {
            return Err(ExtensionError::invalid_params("trigger must be 'manual'"));
        }
        if !matches!(request.scope.as_str(), "all" | "memory" | "skills") {
            return Err(ExtensionError::invalid_params(
                "scope must be one of: all, memory, skills",
            ));
        }
        let runtime = self.runtime()?;
        let metadata = Self::owned_session(&runtime, &ctx.principal, &request.session_id)?;
        let config_store = SessionConfigStore::new(runtime.db_path.to_str().unwrap_or_default())
            .map_err(|error| Self::internal(format!("open session config: {error}")))?;
        let model = config_store
            .get(&StoreSessionId::new(request.session_id.clone()), "model")
            .map_err(|error| Self::internal(format!("read session model: {error}")))?;
        let resolved = agent::run::resolve_model_config(model.as_deref()).await;
        let result = self
            .launch(&runtime, &metadata, resolved, &request.scope, "manual")
            .map_err(Self::internal)?;
        Ok(json!({
            "sessionId": request.session_id,
            "reviewId": result.review_id,
            "started": result.started,
            "state": result.state
        }))
    }

    async fn cancel(&self, params: Value, ctx: &ExtensionContext) -> Result<Value, ExtensionError> {
        let request: CancelRequest = Self::parse(params)?;
        let runtime = self.runtime()?;
        let metadata = Self::owned_session(&runtime, &ctx.principal, &request.session_id)?;
        let state =
            review_runner::cancel_review(&runtime.db_path, &metadata.thread_id, &request.review_id)
                .map_err(|error| match error {
                    CancelReviewError::NotFound => ExtensionError::not_found("review not found"),
                    CancelReviewError::AlreadyCompleted => {
                        ExtensionError::conflict("already_completed")
                    }
                    CancelReviewError::Persistence => {
                        Self::internal("failed to persist cancellation")
                    }
                })?;
        self.publish_cancelled(&runtime, &metadata, &state);
        Ok(
            json!({ "sessionId": request.session_id, "reviewId": request.review_id, "state": state }),
        )
    }
}

#[async_trait]
impl ExtensionHandler for ReviewHandler {
    async fn handle(
        &self,
        method: &str,
        params: Value,
        ctx: &ExtensionContext,
    ) -> Result<Value, ExtensionError> {
        match method {
            "status" => self.status(params, ctx).await,
            "history" => self.history(params, ctx).await,
            "details" => self.details(params, ctx).await,
            "start" => self.start(params, ctx).await,
            "cancel" => self.cancel(params, ctx).await,
            _ => Err(ExtensionError::method_not_found()),
        }
    }

    fn capabilities(&self) -> Value {
        json!({
            "status": true,
            "history": true,
            "details": true,
            "diff": true,
            "start": true,
            "cancel": true
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionRequest {
    session_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HistoryRequest {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DetailsRequest {
    session_id: String,
    review_id: String,
    #[serde(default)]
    include_diff: Option<bool>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StartRequest {
    session_id: String,
    scope: String,
    #[serde(default)]
    trigger: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CancelRequest {
    session_id: String,
    review_id: String,
}

fn parse_i64_cursor(value: Option<&str>) -> Result<Option<i64>, ExtensionError> {
    value
        .map(|value| {
            value
                .parse::<i64>()
                .map_err(|_| ExtensionError::invalid_params("cursor must be an integer"))
        })
        .transpose()
}

fn parse_usize_cursor(value: Option<&str>) -> Result<Option<usize>, ExtensionError> {
    value
        .map(|value| {
            value.parse::<usize>().map_err(|_| {
                ExtensionError::invalid_params("cursor must be a non-negative integer")
            })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::state::ReActState;
    use anureo_llm::message::{AssistantToolCall, Message};
    use crate::connection::{AcpConnection, ConnectionOutbound};
    use crate::extensions::ExtensionHandler;
    use crate::session_repository::SessionRepository;

    fn context(principal: &str) -> ExtensionContext {
        ExtensionContext {
            session_id: None,
            principal: principal.into(),
            connection_id: "connection-1".into(),
            working_directory: None,
            client_capabilities: Default::default(),
        }
    }

    fn configured_handler() -> (tempfile::TempDir, ReviewHandler, String) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let repository = SessionRepository::new(&db).unwrap();
        repository
            .insert("session-1", "thread-1", "owner-1", dir.path())
            .unwrap();
        let handler = ReviewHandler::new(None);
        handler.configure(db, None).unwrap();
        (dir, handler, "session-1".into())
    }

    #[test]
    fn capabilities_are_separate_from_auto_review_and_enable_diff() {
        let handler = ReviewHandler::new(None);
        assert_eq!(handler.capabilities()["status"], true);
        assert_eq!(handler.capabilities()["diff"], true);
        assert_eq!(handler.capabilities()["cancel"], true);
    }

    #[test]
    fn registry_advertises_background_review_capability_name() {
        let mut registry = super::super::ExtensionRegistry::new();
        registry.register_review(Arc::new(ReviewHandler::new(None)));
        let capabilities = registry.build_capability_snapshot();
        assert_eq!(capabilities["backgroundReview"]["status"], true);
        assert!(capabilities.get("review").is_none());
        assert!(registry.has_domain("review"));
    }

    #[test]
    fn changed_notification_is_routed_to_all_owner_connections_only() {
        let connections = Arc::new(ConnectionRegistry::default());
        let (owner_tx_1, mut owner_rx_1) = mpsc::channel(2);
        let (owner_tx_2, mut owner_rx_2) = mpsc::channel(2);
        let (other_tx, mut other_rx) = mpsc::channel(2);
        let owner_1 = Arc::new(AcpConnection::shell(
            "owner-connection-1".into(),
            "owner-1".into(),
            owner_tx_1,
        ));
        owner_1.mark_initialized_for_test();
        connections.insert(owner_1);
        let owner_2 = Arc::new(AcpConnection::shell(
            "owner-connection-2".into(),
            "owner-1".into(),
            owner_tx_2,
        ));
        owner_2.mark_initialized_for_test();
        connections.insert(owner_2);
        connections.insert(Arc::new(AcpConnection::shell(
            "other-connection".into(),
            "owner-2".into(),
            other_tx,
        )));
        let handler = ReviewHandler::new(Some(connections));
        let callback = handler.changed_callback().unwrap();
        let state = ReviewState {
            status: ReviewStateStatus::Running,
            reviewed_at: None,
            memory_count: 0,
            skill_count: 0,
            skip_reason: None,
            duration_ms: 0,
            review_id: Some("review-1".into()),
            trigger: Some("background".into()),
            started_at: Some(chrono::Utc::now().to_rfc3339()),
            fail_reason: None,
            actions: Vec::new(),
            actions_truncated: false,
        };
        callback("session-1", "owner-1", &state);

        for receiver in [&mut owner_rx_1, &mut owner_rx_2] {
            let outbound = receiver.try_recv().unwrap();
            let ConnectionOutbound::ExtensionNotification { method, params } = outbound else {
                panic!("expected extension notification");
            };
            assert_eq!(method, CHANGED_METHOD);
            assert_eq!(params["sessionId"], "session-1");
            assert_eq!(params["review_id"], "review-1");
        }
        assert!(other_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn status_checks_session_owner() {
        let (_dir, handler, session_id) = configured_handler();
        let error = handler
            .handle(
                "status",
                json!({ "sessionId": session_id }),
                &context("owner-2"),
            )
            .await
            .unwrap_err();
        assert_eq!(error.message, "forbidden");
    }

    #[tokio::test]
    async fn status_uses_review_state_schema() {
        let (_dir, handler, session_id) = configured_handler();
        let value = handler
            .handle(
                "status",
                json!({ "sessionId": session_id }),
                &context("owner-1"),
            )
            .await
            .unwrap();
        assert_eq!(value["active"], false);
        assert!(value["latest"].is_null());
    }

    #[tokio::test]
    async fn details_pages_persisted_actions_and_keeps_full_shape() {
        let (_dir, handler, session_id) = configured_handler();
        let runtime = handler.runtime().unwrap();
        let record = anureo_curator::ReviewRecord {
            review_id: Some("review-details".into()),
            session_id: "thread-1".into(),
            reviewed_at: chrono::Utc::now(),
            trigger: "manual".into(),
            model: "model".into(),
            memory_update_count: 2,
            skill_update_count: 0,
            skipped: false,
            skip_reason: None,
            duration_ms: 10,
            actions: (0..2)
                .map(|index| anureo_curator::ReviewActionSummary {
                    kind: "memory".into(),
                    op: "add".into(),
                    target: format!("target-{index}"),
                    file_path: Some("USER.md".into()),
                    summary: format!("action-{index}"),
                    succeeded: true,
                    chars_before: None,
                    chars_after: None,
                    diff_ref: None,
                    persisted_diff: None,
                    diff_truncated: false,
                })
                .collect(),
        };
        ReviewHistory::with_db_path(runtime.db_path)
            .append(&record)
            .unwrap();

        let first = handler
            .handle(
                "details",
                json!({ "sessionId": session_id, "reviewId": "review-details", "limit": 1 }),
                &context("owner-1"),
            )
            .await
            .unwrap();
        assert_eq!(first["actions"].as_array().unwrap().len(), 1);
        assert_eq!(first["actions"][0]["file_path"], "USER.md");
        assert_eq!(first["nextCursor"], "1");

        let with_diff = handler
            .handle(
                "details",
                json!({ "sessionId": session_id, "reviewId": "review-details", "includeDiff": true }),
                &context("owner-1"),
            )
            .await
            .unwrap();
        assert_eq!(with_diff["actions"][0]["diff_available"], false);
        assert!(with_diff["actions"][0].get("diff").is_none());
    }

    #[tokio::test]
    async fn details_diff_is_rebuilt_from_checkpoint_only_for_owner() {
        let (_dir, handler, session_id) = configured_handler();
        let runtime = handler.runtime().unwrap();
        let mut state = ReActState::default();
        state.messages.push(Message::assistant_with_tool_calls(
            String::new(),
            vec![AssistantToolCall {
                id: "call-diff".into(),
                name: "memory".into(),
                arguments: json!({"action":"replace","target":"user","old_text":"旧值","content":"新值"}).to_string(),
            }],
        ));
        let connection = rusqlite::Connection::open(&runtime.db_path).unwrap();
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS checkpoints (thread_id TEXT, checkpoint_ns TEXT, checkpoint_id TEXT, metadata_created_at INTEGER, payload BLOB);",
        ).unwrap();
        connection.execute(
            "INSERT INTO checkpoints (thread_id, checkpoint_ns, checkpoint_id, metadata_created_at, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["review-diff", "background-review", "cp-1", 1_i64, serde_json::to_vec(&state).unwrap()],
        ).unwrap();
        drop(connection);
        let record = anureo_curator::ReviewRecord {
            review_id: Some("review-diff".into()),
            session_id: "thread-1".into(),
            reviewed_at: chrono::Utc::now(),
            trigger: "manual".into(),
            model: "model".into(),
            memory_update_count: 1,
            skill_update_count: 0,
            skipped: false,
            skip_reason: None,
            duration_ms: 10,
            actions: vec![anureo_curator::ReviewActionSummary {
                kind: "memory".into(),
                op: "replace".into(),
                target: "user".into(),
                file_path: Some("USER.md".into()),
                summary: "replaced".into(),
                succeeded: true,
                chars_before: Some(2),
                chars_after: Some(2),
                diff_ref: Some("call-diff".into()),
                persisted_diff: None,
                diff_truncated: false,
            }],
        };
        ReviewHistory::with_db_path(runtime.db_path.clone())
            .append(&record)
            .unwrap();
        let connection = rusqlite::Connection::open(&runtime.db_path).unwrap();
        let actions_json: String = connection
            .query_row(
                "SELECT actions_json FROM review_history WHERE review_id = 'review-diff'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!actions_json.contains("旧值"));
        assert!(!actions_json.contains("新值"));
        let owner = handler
            .handle(
                "details",
                json!({ "sessionId": session_id, "reviewId": "review-diff", "includeDiff": true }),
                &context("owner-1"),
            )
            .await
            .unwrap();
        assert_eq!(owner["actions"][0]["diff_available"], true);
        assert!(owner["actions"][0]["diff"]["text"]
            .as_str()
            .unwrap()
            .contains("+新值"));
        assert!(owner["actions"][0].get("diff_ref").is_none());

        let forbidden = handler
            .handle(
                "details",
                json!({ "sessionId": session_id, "reviewId": "review-diff", "includeDiff": true }),
                &context("owner-2"),
            )
            .await
            .unwrap_err();
        assert_eq!(forbidden.message, "forbidden");
    }
}

//! In-process Background Review v2 runner for ACP.
//!
//! The curator owns review execution and terminal history.  This module owns
//! the ACP-facing running state, idempotency, cancellation boundary, and the
//! single wire projection used by session metadata and review extensions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::stream_bridge::{SessionNotifier, SessionUpdateEnvelope};
use agent::run::ResolvedModelConfig;
use agent::state::ReActState;
use agent::{ReactBuildConfig, RunCancellation};
use agent_client_protocol::schema::v1::{Meta, SessionId, SessionNotification};
use anureo_curator::{
    run_review_with_cancellation, ReviewActionSummary, ReviewConfig, ReviewHistory, ReviewOutcome,
    ReviewRecord, ReviewRunStatus, ReviewStatusRecord,
};
use anureo_llm::message::Message;
use chrono::{DateTime, TimeZone, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::stream_bridge::SessionUpdateEnvelope as UpdateEnvelope;

const INLINE_ACTION_LIMIT: usize = 10;

fn utc_millis(value: DateTime<Utc>) -> DateTime<Utc> {
    Utc.timestamp_millis_opt(value.timestamp_millis())
        .single()
        .expect("valid UTC millisecond timestamp")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStateStatus {
    Running,
    Reviewed,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewAction {
    pub kind: String,
    pub op: String,
    pub target: String,
    pub succeeded: bool,
    pub summary: String,
}

impl From<&ReviewActionSummary> for ReviewAction {
    fn from(value: &ReviewActionSummary) -> Self {
        Self {
            kind: value.kind.clone(),
            op: value.op.clone(),
            target: value.target.clone(),
            succeeded: value.succeeded,
            summary: value.summary.clone(),
        }
    }
}

/// Frozen Background Review v2 wire object.  Every ACP projection serializes
/// this exact type so metadata/status/history/changed cannot drift.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewState {
    pub status: ReviewStateStatus,
    pub reviewed_at: Option<String>,
    pub memory_count: usize,
    pub skill_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<String>,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fail_reason: Option<String>,
    pub actions: Vec<ReviewAction>,
    pub actions_truncated: bool,
}

impl ReviewState {
    fn running(review_id: String, trigger: String, started_at: DateTime<Utc>) -> Self {
        Self {
            status: ReviewStateStatus::Running,
            reviewed_at: None,
            memory_count: 0,
            skill_count: 0,
            skip_reason: None,
            duration_ms: 0,
            review_id: Some(review_id),
            trigger: Some(trigger),
            // SQLite stores running timestamps as milliseconds; normalize the
            // first in-memory snapshot to that same precision so reconnect
            // reads are byte-for-byte stable.
            started_at: Some(started_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, false)),
            fail_reason: None,
            actions: Vec::new(),
            actions_truncated: false,
        }
    }

    pub fn from_status_record(value: &ReviewStatusRecord) -> Self {
        let record = &value.record;
        let status = match value.status {
            ReviewRunStatus::Reviewed => ReviewStateStatus::Reviewed,
            ReviewRunStatus::Skipped => ReviewStateStatus::Skipped,
            ReviewRunStatus::Failed => ReviewStateStatus::Failed,
        };
        let actions = record
            .actions
            .iter()
            .take(INLINE_ACTION_LIMIT)
            .map(ReviewAction::from)
            .collect();
        Self {
            status,
            reviewed_at: Some(record.reviewed_at.to_rfc3339()),
            memory_count: record.memory_update_count,
            skill_count: record.skill_update_count,
            skip_reason: (status == ReviewStateStatus::Skipped)
                .then(|| record.skip_reason.clone())
                .flatten(),
            duration_ms: record.duration_ms,
            review_id: record.review_id.clone(),
            trigger: Some(record.trigger.clone()),
            // The current curator persistence API has no started_at column;
            // derive the stable best available value for terminal records.
            started_at: Some(
                (record.reviewed_at
                    - chrono::Duration::milliseconds(
                        record.duration_ms.min(i64::MAX as u64) as i64
                    ))
                .to_rfc3339(),
            ),
            fail_reason: value.fail_reason.clone(),
            actions,
            actions_truncated: record.actions.len() > INLINE_ACTION_LIMIT,
        }
    }

    fn from_outcome(
        review_id: String,
        trigger: String,
        started_at: DateTime<Utc>,
        reviewed_at: DateTime<Utc>,
        outcome: &ReviewOutcome,
        duration_ms: u64,
    ) -> Self {
        let actions = outcome
            .actions
            .iter()
            .take(INLINE_ACTION_LIMIT)
            .map(ReviewAction::from)
            .collect();
        Self {
            status: if outcome.skipped {
                ReviewStateStatus::Skipped
            } else {
                ReviewStateStatus::Reviewed
            },
            reviewed_at: Some(reviewed_at.to_rfc3339()),
            memory_count: outcome.memory_count,
            skill_count: outcome.skill_count,
            skip_reason: outcome.skip_reason.clone(),
            duration_ms,
            review_id: Some(review_id),
            trigger: Some(trigger),
            started_at: Some(started_at.to_rfc3339()),
            fail_reason: None,
            actions,
            actions_truncated: outcome.actions.len() > INLINE_ACTION_LIMIT,
        }
    }

    fn failed(
        review_id: String,
        trigger: String,
        started_at: DateTime<Utc>,
        reviewed_at: DateTime<Utc>,
        duration_ms: u64,
        reason: &str,
    ) -> Self {
        Self {
            status: ReviewStateStatus::Failed,
            reviewed_at: Some(reviewed_at.to_rfc3339()),
            memory_count: 0,
            skill_count: 0,
            skip_reason: None,
            duration_ms,
            review_id: Some(review_id),
            trigger: Some(trigger),
            started_at: Some(started_at.to_rfc3339()),
            fail_reason: Some(reason.to_string()),
            actions: Vec::new(),
            actions_truncated: false,
        }
    }

    pub fn to_meta(&self) -> Meta {
        let mut meta = Meta::new();
        meta.insert(
            "review".to_string(),
            serde_json::to_value(self).expect("ReviewState is serializable"),
        );
        meta
    }
}

pub type ReviewChangedCallback = Arc<dyn Fn(&str, &str, &ReviewState) + Send + Sync>;

#[derive(Clone)]
pub struct ReviewLaunch {
    pub db_path: PathBuf,
    pub acp_session_id: String,
    pub thread_id: String,
    pub owner_principal: String,
    pub resolved: ResolvedModelConfig,
    pub review_memory: bool,
    pub review_skills: bool,
    pub trigger: String,
    pub tx: Option<mpsc::UnboundedSender<SessionUpdateEnvelope>>,
    pub changed: Option<ReviewChangedCallback>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewStart {
    pub review_id: String,
    pub started: bool,
    pub state: ReviewState,
}

#[derive(Clone)]
struct ActiveReview {
    state: ReviewState,
    cancel: Arc<AtomicBool>,
    run_cancel: RunCancellation,
    finishing: Arc<AtomicBool>,
    owner_principal: String,
}

fn active_reviews() -> &'static Mutex<HashMap<String, ActiveReview>> {
    static ACTIVE: OnceLock<Mutex<HashMap<String, ActiveReview>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn active_key(db_path: &Path, thread_id: &str) -> String {
    format!("{}\0{thread_id}", db_path.display())
}

/// Map `/review-skill` or `review/start` scope to curator flags.
pub fn scope_to_review_config(scope: &Option<String>) -> (bool, bool) {
    match scope.as_deref() {
        Some("memory") => (true, false),
        Some("skills") | Some("skill") => (false, true),
        _ => (true, true),
    }
}

fn build_review_react_config(resolved: &ResolvedModelConfig, db_path: &Path) -> ReactBuildConfig {
    let mut config = ReactBuildConfig::from_env();
    config.openai_api_key = resolved.api_key.clone();
    config.openai_base_url = resolved.base_url.clone();
    config.llm_provider = resolved.provider_type.clone();
    config.model = resolved.model.clone();
    config.working_folder = Some(PathBuf::from("."));
    config.db_path = Some(db_path.to_string_lossy().into_owned());
    config
}

fn extract_session_text(db_path: &Path, thread_id: &str) -> Result<String, String> {
    let conn =
        rusqlite::Connection::open(db_path).map_err(|e| format!("Failed to open database: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT payload FROM checkpoints WHERE thread_id = ?1 ORDER BY metadata_created_at ASC",
        )
        .map_err(|e| format!("Failed to prepare statement: {e}"))?;
    let payloads: Vec<Vec<u8>> = stmt
        .query_map([thread_id], |row| row.get(0))
        .map_err(|e| format!("Query failed: {e}"))?
        .filter_map(Result::ok)
        .collect();
    if payloads.is_empty() {
        return Err(format!("Session not found: {thread_id}"));
    }
    let mut parts = Vec::new();
    for data in &payloads {
        if let Ok(state) = serde_json::from_slice::<ReActState>(data) {
            for msg in &state.messages {
                match msg {
                    Message::User(user) => parts.push(format!("User: {}", user.as_text())),
                    Message::Assistant(assistant) if !assistant.content.is_empty() => {
                        parts.push(format!("Assistant: {}", assistant.content));
                    }
                    Message::Tool { content, .. } => {
                        if let Some(text) = content.as_text().filter(|text| !text.is_empty()) {
                            parts.push(format!("Tool: {text}"));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(anureo_llm::message::strip_background_review_harness(
        &parts.join("\n"),
    ))
}

pub fn ensure_running_schema(db_path: &Path) -> Result<(), String> {
    let conn = checkpoint_sqlite_store::sqlite_util::open_sqlite_with_wal(db_path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS acp_review_running (
            thread_id TEXT PRIMARY KEY,
            acp_session_id TEXT NOT NULL,
            owner_principal TEXT NOT NULL,
            review_id TEXT NOT NULL UNIQUE,
            trigger TEXT NOT NULL,
            started_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_acp_review_running_owner
            ON acp_review_running(owner_principal);",
    )
    .map_err(|e| format!("init ACP review running schema: {e}"))?;
    Ok(())
}

fn persist_running(
    db_path: &Path,
    launch: &ReviewLaunch,
    state: &ReviewState,
) -> Result<(), String> {
    ensure_running_schema(db_path)?;
    let conn = checkpoint_sqlite_store::sqlite_util::open_sqlite_with_wal(db_path)?;
    let started_at = state
        .started_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.timestamp_millis())
        .unwrap_or_else(|| Utc::now().timestamp_millis());
    conn.execute(
        "INSERT OR REPLACE INTO acp_review_running
         (thread_id, acp_session_id, owner_principal, review_id, trigger, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            launch.thread_id,
            launch.acp_session_id,
            launch.owner_principal,
            state.review_id,
            launch.trigger,
            started_at
        ],
    )
    .map_err(|e| format!("persist running review: {e}"))?;
    Ok(())
}

fn clear_running(db_path: &Path, thread_id: &str, review_id: &str) -> Result<(), String> {
    ensure_running_schema(db_path)?;
    let conn = checkpoint_sqlite_store::sqlite_util::open_sqlite_with_wal(db_path)?;
    conn.execute(
        "DELETE FROM acp_review_running WHERE thread_id = ?1 AND review_id = ?2",
        params![thread_id, review_id],
    )
    .map_err(|e| format!("clear running review: {e}"))?;
    Ok(())
}

fn running_state(db_path: &Path, thread_id: &str) -> Result<Option<ReviewState>, String> {
    ensure_running_schema(db_path)?;
    let conn = checkpoint_sqlite_store::sqlite_util::open_sqlite_with_wal(db_path)?;
    conn.query_row(
        "SELECT review_id, trigger, started_at FROM acp_review_running WHERE thread_id = ?1",
        [thread_id],
        |row| {
            let millis: i64 = row.get(2)?;
            let started_at = Utc
                .timestamp_millis_opt(millis)
                .single()
                .unwrap_or_else(Utc::now);
            Ok(ReviewState::running(row.get(0)?, row.get(1)?, started_at))
        },
    )
    .optional()
    .map_err(|e| format!("read running review: {e}"))
}

/// Return the authoritative latest state, preferring the durable running row
/// over curator terminal history.
pub fn latest_review_state(db_path: &Path, thread_id: &str) -> Result<Option<ReviewState>, String> {
    let running = running_state(db_path, thread_id)?;
    let terminal = ReviewHistory::with_db_path(db_path.to_path_buf())
        .find_status_by_session(thread_id)
        .map(|record| record.as_ref().map(ReviewState::from_status_record))?;
    // A terminal write can succeed even if deleting the running row fails.
    // Never regress the same review back to `running` on reconnect.
    if running
        .as_ref()
        .and_then(|state| state.review_id.as_deref())
        == terminal
            .as_ref()
            .and_then(|state| state.review_id.as_deref())
        && terminal.is_some()
    {
        return Ok(terminal);
    }
    Ok(running.or(terminal))
}

/// A curator task cannot survive process restart. Convert orphaned durable
/// running rows to an explicit failed terminal record instead of leaving the
/// client stuck in `running` forever. Reviews active in another agent instance
/// in this process are retained.
pub fn recover_stale_reviews(db_path: &Path) -> Result<usize, String> {
    ensure_running_schema(db_path)?;
    let connection = checkpoint_sqlite_store::sqlite_util::open_sqlite_with_wal(db_path)?;
    let mut statement = connection
        .prepare("SELECT thread_id, review_id, trigger, started_at FROM acp_review_running")
        .map_err(|error| format!("prepare stale review recovery: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .map_err(|error| format!("query stale reviews: {error}"))?;
    let stale: Vec<_> = rows
        .collect::<Result<_, _>>()
        .map_err(|error| format!("read stale review: {error}"))?;
    drop(statement);
    drop(connection);

    let history = ReviewHistory::with_db_path(db_path.to_path_buf());
    let mut recovered = 0;
    for (thread_id, review_id, trigger, started_at_ms) in stale {
        let key = active_key(db_path, &thread_id);
        let is_active = active_reviews()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .and_then(|entry| entry.state.review_id.as_deref())
            == Some(review_id.as_str());
        if is_active {
            continue;
        }
        // A terminal row may already exist if its running-row deletion failed
        // before shutdown. Preserve that result instead of overwriting it with
        // a spurious process_restarted failure.
        if history.find_by_review_id(&thread_id, &review_id)?.is_some() {
            clear_running(db_path, &thread_id, &review_id)?;
            continue;
        }
        let reviewed_at = Utc::now();
        let started_at = Utc
            .timestamp_millis_opt(started_at_ms)
            .single()
            .unwrap_or(reviewed_at);
        let record = ReviewRecord {
            review_id: Some(review_id.clone()),
            session_id: thread_id.clone(),
            reviewed_at,
            trigger,
            model: String::new(),
            memory_update_count: 0,
            skill_update_count: 0,
            skipped: true,
            skip_reason: None,
            duration_ms: (reviewed_at - started_at).num_milliseconds().max(0) as u64,
            actions: Vec::new(),
        };
        history.append_failed(&record, "process_restarted")?;
        clear_running(db_path, &thread_id, &review_id)?;
        recovered += 1;
    }
    Ok(recovered)
}

fn emit_state(launch: &ReviewLaunch, state: &ReviewState) {
    if let Some(tx) = launch.tx.as_ref() {
        let notifier =
            SessionNotifier::new(tx.clone(), SessionId::new(launch.acp_session_id.clone()));
        notifier.try_send_session_meta(state.to_meta());
    }
    if let Some(changed) = launch.changed.as_ref() {
        changed(&launch.acp_session_id, &launch.owner_principal, state);
    }
}

fn remove_active_if_same(db_path: &Path, thread_id: &str, review_id: &str) {
    let key = active_key(db_path, thread_id);
    let mut active = active_reviews().lock().unwrap_or_else(|e| e.into_inner());
    if active
        .get(&key)
        .and_then(|entry| entry.state.review_id.as_deref())
        == Some(review_id)
    {
        active.remove(&key);
    }
}

/// Atomically win the terminal transition against `review/cancel`.
fn claim_terminal(db_path: &Path, thread_id: &str, review_id: &str) -> bool {
    let key = active_key(db_path, thread_id);
    let mut active = active_reviews().lock().unwrap_or_else(|e| e.into_inner());
    let Some(entry) = active.get(&key) else {
        return false;
    };
    if entry.state.review_id.as_deref() != Some(review_id) || entry.cancel.load(Ordering::Acquire) {
        active.remove(&key);
        return false;
    }
    !entry.finishing.swap(true, Ordering::AcqRel)
}

/// Idempotently start a review.  Duplicate starts return the existing id and
/// do not create a second curator run.
pub fn spawn_inprocess_review(launch: ReviewLaunch) -> Result<ReviewStart, String> {
    let key = active_key(&launch.db_path, &launch.thread_id);
    let (review_id, started_at, state, cancel, run_cancel) = {
        // Keep admission + durable insert + registry insert in one critical
        // section. Concurrent `review/start` calls therefore cannot both pass
        // the empty check and launch duplicate curator runs.
        let mut active = active_reviews().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = active.get(&key) {
            return Ok(ReviewStart {
                review_id: existing.state.review_id.clone().unwrap_or_default(),
                started: false,
                state: existing.state.clone(),
            });
        }
        let review_id = format!("review-{}", uuid::Uuid::new_v4());
        let started_at = utc_millis(Utc::now());
        let state = ReviewState::running(review_id.clone(), launch.trigger.clone(), started_at);
        persist_running(&launch.db_path, &launch, &state)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let run_cancel = RunCancellation::new(0);
        let finishing = Arc::new(AtomicBool::new(false));
        active.insert(
            key,
            ActiveReview {
                state: state.clone(),
                cancel: cancel.clone(),
                run_cancel: run_cancel.clone(),
                finishing,
                owner_principal: launch.owner_principal.clone(),
            },
        );
        (review_id, started_at, state, cancel, run_cancel)
    };
    emit_state(&launch, &state);

    let thread_launch = launch.clone();
    let thread_review_id = review_id.clone();
    std::thread::spawn(move || {
        let text = match extract_session_text(&thread_launch.db_path, &thread_launch.thread_id) {
            Ok(text) => text,
            Err(message) => {
                if claim_terminal(
                    &thread_launch.db_path,
                    &thread_launch.thread_id,
                    &thread_review_id,
                ) {
                    finish_failed(
                        &thread_launch,
                        &thread_review_id,
                        started_at,
                        "session_unavailable",
                        &message,
                    );
                }
                return;
            }
        };
        if cancel.load(Ordering::Acquire) {
            claim_terminal(
                &thread_launch.db_path,
                &thread_launch.thread_id,
                &thread_review_id,
            );
            return;
        }
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(err) => {
                if claim_terminal(
                    &thread_launch.db_path,
                    &thread_launch.thread_id,
                    &thread_review_id,
                ) {
                    finish_failed(
                        &thread_launch,
                        &thread_review_id,
                        started_at,
                        "runtime_error",
                        &err.to_string(),
                    );
                }
                return;
            }
        };
        let config = ReviewConfig {
            review_memory: thread_launch.review_memory,
            review_skills: thread_launch.review_skills,
            checkpoint_thread_id: Some(thread_review_id.clone()),
            persist_diff: std::env::var("ANUREO_REVIEW_PERSIST_DIFF")
                .ok()
                .is_some_and(|value| value == "1"),
            ..Default::default()
        };
        let result = runtime.block_on(run_review_with_cancellation(
            build_review_react_config(&thread_launch.resolved, &thread_launch.db_path),
            uuid::Uuid::new_v4().to_string(),
            &text,
            &config,
            Some(run_cancel),
        ));
        if !claim_terminal(
            &thread_launch.db_path,
            &thread_launch.thread_id,
            &thread_review_id,
        ) {
            return;
        }
        match result {
            Ok(outcome) => finish_success(&thread_launch, &thread_review_id, started_at, outcome),
            Err(err) => finish_failed(
                &thread_launch,
                &thread_review_id,
                started_at,
                "llm_error",
                &err.to_string(),
            ),
        }
    });

    Ok(ReviewStart {
        review_id,
        started: true,
        state,
    })
}

fn finish_success(
    launch: &ReviewLaunch,
    review_id: &str,
    started_at: DateTime<Utc>,
    outcome: ReviewOutcome,
) {
    let reviewed_at = utc_millis(Utc::now());
    let duration_ms = (reviewed_at - started_at).num_milliseconds().max(0) as u64;
    let record = ReviewRecord {
        review_id: Some(review_id.to_string()),
        session_id: launch.thread_id.clone(),
        reviewed_at,
        trigger: launch.trigger.clone(),
        model: launch.resolved.model.clone().unwrap_or_default(),
        memory_update_count: outcome.memory_count,
        skill_update_count: outcome.skill_count,
        skipped: outcome.skipped,
        skip_reason: outcome.skip_reason.clone(),
        duration_ms,
        actions: outcome.actions.clone(),
    };
    let history = ReviewHistory::with_db_path(launch.db_path.clone());
    if let Err(err) = history.append(&record) {
        error!(thread_id = %launch.thread_id, error = %err, "failed to persist review terminal state");
        finish_failed(launch, review_id, started_at, "persistence_error", &err);
        return;
    }
    if let Err(err) = clear_running(&launch.db_path, &launch.thread_id, review_id) {
        warn!(thread_id = %launch.thread_id, error = %err, "failed to clear running review state");
    }
    let state = ReviewState::from_outcome(
        review_id.to_string(),
        launch.trigger.clone(),
        started_at,
        reviewed_at,
        &outcome,
        duration_ms,
    );
    notify_terminal(launch, Some(&outcome), &state);
    remove_active_if_same(&launch.db_path, &launch.thread_id, review_id);
    info!(thread_id = %launch.thread_id, review_id, status = ?state.status, "ACP background review completed");
}

fn finish_failed(
    launch: &ReviewLaunch,
    review_id: &str,
    started_at: DateTime<Utc>,
    fail_reason: &str,
    detail: &str,
) {
    let reviewed_at = utc_millis(Utc::now());
    let duration_ms = (reviewed_at - started_at).num_milliseconds().max(0) as u64;
    let record = ReviewRecord {
        review_id: Some(review_id.to_string()),
        session_id: launch.thread_id.clone(),
        reviewed_at,
        trigger: launch.trigger.clone(),
        model: launch.resolved.model.clone().unwrap_or_default(),
        memory_update_count: 0,
        skill_update_count: 0,
        skipped: true,
        skip_reason: None,
        duration_ms,
        actions: Vec::new(),
    };
    let history = ReviewHistory::with_db_path(launch.db_path.clone());
    if let Err(err) = history.append_failed(&record, fail_reason) {
        error!(thread_id = %launch.thread_id, error = %err, "failed to persist failed review");
    }
    if let Err(err) = clear_running(&launch.db_path, &launch.thread_id, review_id) {
        warn!(thread_id = %launch.thread_id, error = %err, "failed to clear failed review running state");
    }
    let state = ReviewState::failed(
        review_id.to_string(),
        launch.trigger.clone(),
        started_at,
        reviewed_at,
        duration_ms,
        fail_reason,
    );
    notify_terminal(launch, None, &state);
    remove_active_if_same(&launch.db_path, &launch.thread_id, review_id);
    error!(thread_id = %launch.thread_id, review_id, fail_reason, detail, "ACP background review failed");
}

fn notify_terminal(launch: &ReviewLaunch, outcome: Option<&ReviewOutcome>, state: &ReviewState) {
    if let (Some(tx), Some(outcome)) = (launch.tx.as_ref(), outcome) {
        let session_id = SessionId::new(launch.acp_session_id.clone());
        let chunk = agent_client_protocol::schema::v1::ContentChunk::new(
            agent_client_protocol::schema::v1::ContentBlock::Text(
                agent_client_protocol::schema::v1::TextContent::new(build_summary_line(outcome)),
            ),
        )
        .message_id(Some(agent_client_protocol::schema::v1::MessageId::new(
            uuid::Uuid::new_v4().to_string(),
        )));
        let notification = SessionNotification::new(
            session_id,
            agent_client_protocol::schema::v1::SessionUpdate::AgentMessageChunk(chunk),
        );
        if let Err(err) = tx.send(UpdateEnvelope::Session(notification)) {
            warn!(thread_id = %launch.thread_id, error = %err, "failed to send review summary");
        }
    }
    emit_state(launch, state);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelReviewError {
    NotFound,
    AlreadyCompleted,
    Persistence,
}

/// Cancel only the background agent, never the main prompt. The active lock
/// serializes terminal persistence against a concurrently finishing review.
pub fn cancel_review(
    db_path: &Path,
    thread_id: &str,
    review_id: &str,
) -> Result<ReviewState, CancelReviewError> {
    let key = active_key(db_path, thread_id);
    let mut reviews = active_reviews().lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = reviews.get(&key) else {
        drop(reviews);
        let history = ReviewHistory::with_db_path(db_path.to_path_buf());
        return match history.find_by_review_id(thread_id, review_id) {
            Ok(Some(_)) => Err(CancelReviewError::AlreadyCompleted),
            Ok(None) => Err(CancelReviewError::NotFound),
            Err(error) => {
                error!(thread_id, review_id, %error, "failed to look up review for cancellation");
                Err(CancelReviewError::Persistence)
            }
        };
    };
    if active.state.review_id.as_deref() != Some(review_id) {
        return Err(CancelReviewError::NotFound);
    }
    if active.finishing.load(Ordering::Acquire) || active.cancel.load(Ordering::Acquire) {
        return Err(CancelReviewError::AlreadyCompleted);
    }
    let started_at = active
        .state
        .started_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let reviewed_at = utc_millis(Utc::now());
    let duration_ms = (reviewed_at - started_at).num_milliseconds().max(0) as u64;
    let record = ReviewRecord {
        review_id: Some(review_id.to_string()),
        session_id: thread_id.to_string(),
        reviewed_at,
        trigger: active
            .state
            .trigger
            .clone()
            .unwrap_or_else(|| "manual".into()),
        model: String::new(),
        memory_update_count: 0,
        skill_update_count: 0,
        skipped: true,
        skip_reason: None,
        duration_ms,
        actions: Vec::new(),
    };
    ReviewHistory::with_db_path(db_path.to_path_buf())
        .append_failed(&record, "cancelled")
        .map_err(|error| {
            error!(thread_id, review_id, %error, "failed to persist cancelled review");
            CancelReviewError::Persistence
        })?;
    if let Err(error) = clear_running(db_path, thread_id, review_id) {
        warn!(thread_id, review_id, %error, "failed to clear cancelled review running state");
    }
    let state = ReviewState::failed(
        review_id.to_string(),
        record.trigger,
        started_at,
        reviewed_at,
        duration_ms,
        "cancelled",
    );
    let entry = reviews.get_mut(&key).expect("active review held by lock");
    entry.state = state.clone();
    entry.cancel.store(true, Ordering::Release);
    let run_cancel = entry.run_cancel.clone();
    drop(reviews);
    run_cancel.cancel();
    Ok(state)
}

pub fn active_review_owner(db_path: &Path, thread_id: &str) -> Option<String> {
    active_reviews()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&active_key(db_path, thread_id))
        .map(|entry| entry.owner_principal.clone())
}

fn build_summary_line(outcome: &ReviewOutcome) -> String {
    let secs = outcome.duration_ms as f64 / 1000.0;
    if outcome.skipped {
        return format!(
            "Background review skipped ({}).",
            outcome.skip_reason.as_deref().unwrap_or("skipped")
        );
    }
    let mut lines = vec![format!("Background review completed ({secs:.1}s):")];
    let memory: Vec<_> = outcome
        .actions
        .iter()
        .filter(|action| action.kind.contains("memory") && action.succeeded)
        .collect();
    let skills: Vec<_> = outcome
        .actions
        .iter()
        .filter(|action| action.kind.contains("skill") && action.succeeded)
        .collect();
    if !memory.is_empty() {
        lines.push(format!("  📝 {} memories saved:", memory.len()));
        lines.extend(
            memory
                .iter()
                .map(|action| format!("     • {}", action.summary)),
        );
    }
    if !skills.is_empty() {
        lines.push(format!("  🔧 {} skills updated:", skills.len()));
        lines.extend(
            skills
                .iter()
                .map(|action| format!("     • {}", action.summary)),
        );
    }
    if memory.is_empty() && skills.is_empty() {
        lines.push("  • nothing to save".to_string());
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(index: usize) -> ReviewActionSummary {
        ReviewActionSummary {
            kind: "memory".into(),
            op: "add".into(),
            target: format!("target-{index}"),
            file_path: Some("USER.md".into()),
            summary: format!("action {index}"),
            succeeded: true,
            chars_before: None,
            chars_after: None,
            diff_ref: None,
            persisted_diff: None,
            diff_truncated: false,
        }
    }

    #[test]
    fn scope_mapping_is_compatible() {
        assert_eq!(
            scope_to_review_config(&Some("memory".into())),
            (true, false)
        );
        assert_eq!(
            scope_to_review_config(&Some("skills".into())),
            (false, true)
        );
        assert_eq!(scope_to_review_config(&None), (true, true));
    }

    #[test]
    fn terminal_state_caps_inline_actions_at_ten() {
        let outcome = ReviewOutcome {
            actions: (0..12).map(action).collect(),
            memory_count: 12,
            duration_ms: 10,
            ..Default::default()
        };
        let now = Utc::now();
        let state =
            ReviewState::from_outcome("review-1".into(), "manual".into(), now, now, &outcome, 10);
        assert_eq!(state.actions.len(), 10);
        assert!(state.actions_truncated);
        let wire = serde_json::to_value(&state).unwrap();
        assert!(wire["actions"][0].get("file_path").is_none());
    }

    #[test]
    fn running_state_is_durable_and_is_the_latest_projection() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let launch = ReviewLaunch {
            db_path: db.clone(),
            acp_session_id: "session-1".into(),
            thread_id: "thread-1".into(),
            owner_principal: "owner-1".into(),
            resolved: ResolvedModelConfig::default(),
            review_memory: true,
            review_skills: true,
            trigger: "manual".into(),
            tx: None,
            changed: None,
        };
        let state = ReviewState::running("review-1".into(), "manual".into(), Utc::now());
        persist_running(&db, &launch, &state).unwrap();
        assert_eq!(latest_review_state(&db, "thread-1").unwrap(), Some(state));
        assert_eq!(recover_stale_reviews(&db).unwrap(), 1);
        let recovered = latest_review_state(&db, "thread-1")
            .unwrap()
            .expect("recovered terminal state");
        assert_eq!(recovered.status, ReviewStateStatus::Failed);
        assert_eq!(recovered.fail_reason.as_deref(), Some("process_restarted"));
    }

    #[test]
    fn cancel_signals_review_agent_and_persists_one_terminal_state() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let launch = ReviewLaunch {
            db_path: db.clone(),
            acp_session_id: "session-cancel".into(),
            thread_id: "thread-cancel".into(),
            owner_principal: "owner-1".into(),
            resolved: ResolvedModelConfig::default(),
            review_memory: true,
            review_skills: true,
            trigger: "manual".into(),
            tx: None,
            changed: None,
        };
        let state = ReviewState::running("review-cancel".into(), "manual".into(), Utc::now());
        persist_running(&db, &launch, &state).unwrap();
        let run_cancel = RunCancellation::new(0);
        let key = active_key(&db, &launch.thread_id);
        active_reviews().lock().unwrap().insert(
            key,
            ActiveReview {
                state,
                cancel: Arc::new(AtomicBool::new(false)),
                run_cancel: run_cancel.clone(),
                finishing: Arc::new(AtomicBool::new(false)),
                owner_principal: "owner-1".into(),
            },
        );

        let cancelled = cancel_review(&db, &launch.thread_id, "review-cancel").unwrap();
        assert!(run_cancel.token().is_cancelled());
        assert_eq!(cancelled.status, ReviewStateStatus::Failed);
        assert_eq!(cancelled.fail_reason.as_deref(), Some("cancelled"));
        assert_eq!(
            latest_review_state(&db, &launch.thread_id).unwrap(),
            Some(cancelled)
        );
        assert_eq!(
            cancel_review(&db, &launch.thread_id, "review-cancel"),
            Err(CancelReviewError::AlreadyCompleted)
        );
        assert!(!claim_terminal(&db, &launch.thread_id, "review-cancel"));
    }

    #[test]
    fn terminal_history_wins_over_stale_running_row_for_same_review() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let launch = ReviewLaunch {
            db_path: db.clone(),
            acp_session_id: "session-stale".into(),
            thread_id: "thread-stale".into(),
            owner_principal: "owner-1".into(),
            resolved: ResolvedModelConfig::default(),
            review_memory: true,
            review_skills: true,
            trigger: "manual".into(),
            tx: None,
            changed: None,
        };
        let state = ReviewState::running("review-stale".into(), "manual".into(), Utc::now());
        persist_running(&db, &launch, &state).unwrap();
        let record = ReviewRecord {
            review_id: Some("review-stale".into()),
            session_id: launch.thread_id.clone(),
            reviewed_at: Utc::now(),
            trigger: "manual".into(),
            model: String::new(),
            memory_update_count: 0,
            skill_update_count: 0,
            skipped: true,
            skip_reason: None,
            duration_ms: 0,
            actions: Vec::new(),
        };
        ReviewHistory::with_db_path(db.clone())
            .append_failed(&record, "cancelled")
            .unwrap();
        assert_eq!(
            latest_review_state(&db, &launch.thread_id)
                .unwrap()
                .unwrap()
                .status,
            ReviewStateStatus::Failed
        );
        assert_eq!(recover_stale_reviews(&db).unwrap(), 0);
        assert_eq!(
            latest_review_state(&db, &launch.thread_id)
                .unwrap()
                .unwrap()
                .fail_reason
                .as_deref(),
            Some("cancelled")
        );
    }

    #[test]
    fn same_state_shape_is_used_in_meta() {
        let state = ReviewState::running("review-1".into(), "background".into(), Utc::now());
        let direct = serde_json::to_value(&state).unwrap();
        assert_eq!(state.to_meta().get("review"), Some(&direct));
    }

    #[test]
    fn persisted_terminal_states_project_reviewed_skipped_and_failed() {
        let base = ReviewRecord {
            review_id: Some("review-1".into()),
            session_id: "thread-1".into(),
            reviewed_at: Utc::now(),
            trigger: "background".into(),
            model: "model".into(),
            memory_update_count: 1,
            skill_update_count: 0,
            skipped: false,
            skip_reason: None,
            duration_ms: 25,
            actions: vec![action(0)],
        };
        let reviewed = ReviewState::from_status_record(&ReviewStatusRecord {
            status: ReviewRunStatus::Reviewed,
            fail_reason: None,
            record: base.clone(),
        });
        assert_eq!(reviewed.status, ReviewStateStatus::Reviewed);

        let skipped = ReviewState::from_status_record(&ReviewStatusRecord {
            status: ReviewRunStatus::Skipped,
            fail_reason: None,
            record: ReviewRecord {
                skipped: true,
                skip_reason: Some("insufficient_content".into()),
                ..base.clone()
            },
        });
        assert_eq!(skipped.status, ReviewStateStatus::Skipped);
        assert_eq!(skipped.skip_reason.as_deref(), Some("insufficient_content"));

        let failed = ReviewState::from_status_record(&ReviewStatusRecord {
            status: ReviewRunStatus::Failed,
            fail_reason: Some("llm_error".into()),
            record: base,
        });
        assert_eq!(failed.status, ReviewStateStatus::Failed);
        assert_eq!(failed.fail_reason.as_deref(), Some("llm_error"));
        assert!(failed.skip_reason.is_none());
    }

    #[test]
    fn summary_preserves_human_readable_actions() {
        let outcome = ReviewOutcome {
            actions: vec![action(1)],
            memory_count: 1,
            duration_ms: 500,
            ..Default::default()
        };
        let text = build_summary_line(&outcome);
        assert!(text.contains("Background review completed (0.5s)"));
        assert!(text.contains("action 1"));
    }
}

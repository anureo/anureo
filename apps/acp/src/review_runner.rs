//! In-process background review runner for ACP.
//!
//! Spawns `run_review()` in a dedicated OS thread with its own tokio runtime,
//! so the ACP prompt returns immediately while review proceeds in the background.
//! This replaces the CLI path's external subprocess spawn (`anureo review session`),
//! which is a no-op under the ACP server (`anureo acp` has no review subcommand).

use std::path::PathBuf;

use crate::stream_bridge::SessionUpdateEnvelope;
use agent::run::ResolvedModelConfig;
use agent::state::ReActState;
use agent::ReactBuildConfig;
use agent_client_protocol::schema::v1::{Meta, SessionId, SessionNotification};
use anureo_curator::workflow::global_registry;
use anureo_curator::{run_review, ReviewConfig, ReviewHistory, ReviewOutcome, ReviewRecord};
use anureo_llm::message::Message;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::stream_bridge::SessionNotifier;

/// Map `/review-skill` scope to `(review_memory, review_skills)` booleans.
pub fn scope_to_review_config(scope: &Option<String>) -> (bool, bool) {
    match scope.as_deref() {
        Some("memory") => (true, false),
        Some("skills") | Some("skill") => (false, true),
        _ => (true, true),
    }
}

/// Build a minimal `ReactBuildConfig` suitable for `run_review()` from the
/// ACP-resolved model configuration. Mirrors `cli/src/review_skill_cmd.rs:40-53`.
fn build_review_react_config(resolved: &ResolvedModelConfig) -> ReactBuildConfig {
    let mut config = ReactBuildConfig::from_env();
    config.openai_api_key = resolved.api_key.clone();
    config.openai_base_url = resolved.base_url.clone();
    config.llm_provider = resolved.provider_type.clone();
    config.model = resolved.model.clone();
    config.working_folder = Some(PathBuf::from("."));
    config
}

/// Extract session conversation as plain text from the SQLite checkpoint store.
///
/// Replicates `cli/src/session.rs:325-348` — queries all checkpoints for the
/// given thread, deserializes `ReActState`, and joins messages as
/// `"User: ...\nAssistant: ...\nTool: ..."`.
fn extract_session_text(thread_id: &str) -> Result<String, String> {
    let db_path = checkpoint_sqlite_store::default_memory_db_path();
    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| format!("Failed to open database: {e}"))?;
    let mut stmt = conn
        .prepare(
            "SELECT payload FROM checkpoints WHERE thread_id = ?1 ORDER BY metadata_created_at ASC",
        )
        .map_err(|e| format!("Failed to prepare statement: {e}"))?;
    let payloads: Vec<Vec<u8>> = stmt
        .query_map([thread_id], |row| row.get(0))
        .map_err(|e| format!("Query failed: {e}"))?
        .filter_map(|r| r.ok())
        .collect();

    if payloads.is_empty() {
        return Err(format!("Session not found: {thread_id}"));
    }

    let mut parts = Vec::new();
    for data in &payloads {
        if let Ok(state) = serde_json::from_slice::<ReActState>(data) {
            for msg in &state.messages {
                match msg {
                    Message::User(u) => parts.push(format!("User: {}", u.as_text())),
                    Message::Assistant(a) => {
                        if !a.content.is_empty() {
                            parts.push(format!("Assistant: {}", a.content));
                        }
                    }
                    Message::Tool { content, .. } => {
                        if let Some(text) = content.as_text() {
                            if !text.is_empty() {
                                parts.push(format!("Tool: {}", text));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    // Strip the curator's `<background_review>` harness block so the
    // review-runner doesn't re-inject the curator's one-shot memory/skill
    // instructions back into the LLM context. Hermes `hermes_state.py`
    // #10 parity.
    let assembled = parts.join("\n");
    Ok(anureo_llm::message::strip_background_review_harness(
        &assembled,
    ))
}

/// Spawn an in-process background review thread.
///
/// Returns immediately. The review runs in a dedicated OS thread with an
/// independent tokio runtime, ensuring:
/// - The ACP prompt is not blocked.
/// - Review failures are logged but never crash the ACP process.
/// - Short sessions are auto-skipped by `run_review`'s `min_session_chars` gate.
///
/// When `tx` and `session_id` are provided (the normal ACP path), the runner
/// also emits two follow-up notifications once review completes:
///   1. An `AgentMessageChunk` summarizing what was saved / why review was
///      skipped (so the chat stream gets a human-readable nudge).
///   2. A `SessionInfoUpdate` carrying `_meta.review` (status + counts) so the
///      IDE session list can badge the row as reviewed/pending without
///      polling the SQLite history table.
///
/// Pass `None` for `tx` to disable both notifications (used by tests and any
/// future non-ACP embedding that doesn't have a session channel wired).
pub fn spawn_inprocess_review(
    thread_id: String,
    resolved: ResolvedModelConfig,
    review_memory: bool,
    review_skills: bool,
    trigger: String,
    tx: Option<mpsc::UnboundedSender<SessionUpdateEnvelope>>,
    session_id: Option<SessionId>,
) {
    // Per-session dedup: skip if a review is already in flight for this
    // thread. The returned guard is moved into the spawned thread below
    // so the slot is released only when the review itself completes.
    let guard = match global_registry().try_acquire(thread_id.clone()) {
        Some(g) => g,
        None => {
            info!(
                thread_id = %thread_id,
                trigger = %trigger,
                "Background review already in flight for session, skipping duplicate spawn"
            );
            return;
        }
    };

    std::thread::spawn(move || {
        let _guard = guard;
        let text = match extract_session_text(&thread_id) {
            Ok(t) => t,
            Err(e) => {
                warn!(thread_id = %thread_id, error = %e, "extract_session_text failed, skipping review");
                return;
            }
        };

        let react_config = build_review_react_config(&resolved);
        let config = ReviewConfig {
            review_memory,
            review_skills,
            ..Default::default()
        };
        let checkpoint_id = uuid::Uuid::new_v4().to_string();

        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                error!(error = %e, "Failed to create review runtime");
                return;
            }
        };

        let start = std::time::Instant::now();
        let result = rt.block_on(run_review(react_config, checkpoint_id, &text, &config));

        let history =
            ReviewHistory::with_db_path(checkpoint_sqlite_store::default_memory_db_path());

        match result {
            Ok(outcome) => {
                let record = ReviewRecord {
                    session_id: thread_id.clone(),
                    reviewed_at: chrono::Utc::now(),
                    trigger: trigger.clone(),
                    model: resolved.model.clone().unwrap_or_default(),
                    memory_update_count: outcome.memory_count,
                    skill_update_count: outcome.skill_count,
                    skipped: outcome.skipped,
                    skip_reason: outcome.skip_reason.clone(),
                    duration_ms: start.elapsed().as_millis() as u64,
                };
                if let Err(e) = history.append(&record) {
                    warn!(thread_id = %thread_id, error = %e, "Failed to persist review record");
                }
                info!(
                    thread_id = %thread_id,
                    skipped = outcome.skipped,
                    memory_count = outcome.memory_count,
                    skill_count = outcome.skill_count,
                    duration_ms = start.elapsed().as_millis() as u64,
                    "ACP in-process review completed"
                );
                notify_completion(
                    tx.as_ref(),
                    session_id.as_ref(),
                    &thread_id,
                    Ok(&outcome),
                    start.elapsed().as_millis() as u64,
                );
            }
            Err(e) => {
                let record = ReviewRecord {
                    session_id: thread_id.clone(),
                    reviewed_at: chrono::Utc::now(),
                    trigger: trigger.clone(),
                    model: resolved.model.clone().unwrap_or_default(),
                    memory_update_count: 0,
                    skill_update_count: 0,
                    skipped: true,
                    skip_reason: Some(format!("llm_error: {}", e)),
                    duration_ms: start.elapsed().as_millis() as u64,
                };
                if let Err(persist_err) = history.append(&record) {
                    warn!(thread_id = %thread_id, error = %persist_err, "Failed to persist review record");
                }
                error!(
                    thread_id = %thread_id,
                    error = %e,
                    duration_ms = start.elapsed().as_millis() as u64,
                    "ACP in-process review failed"
                );
                let skip_reason = format!("llm_error: {}", e);
                let synthetic = ReviewOutcome::skipped(skip_reason);
                notify_completion(
                    tx.as_ref(),
                    session_id.as_ref(),
                    &thread_id,
                    Ok(&synthetic),
                    start.elapsed().as_millis() as u64,
                );
            }
        }
    });
}

/// Push review-completion notifications to the ACP client, if a channel is wired.
///
/// Emits two notifications on success or skip:
///   - `AgentMessageChunk` with a human-readable one-liner so the chat pane
///     shows what the reviewer did.
///   - `SessionInfoUpdate` with `_meta.review = { status, reviewed_at,
///     memory_count, skill_count, skip_reason? }` so the session list can
///     display a reviewed/pending badge.
///
/// No-op when either `tx` or `session_id` is `None` (non-ACP embedding).
fn notify_completion(
    tx: Option<&mpsc::UnboundedSender<SessionUpdateEnvelope>>,
    session_id: Option<&SessionId>,
    thread_id: &str,
    outcome: Result<&ReviewOutcome, ()>,
    duration_ms: u64,
) {
    let (Some(tx), Some(session_id)) = (tx, session_id) else {
        return;
    };
    let outcome = match outcome {
        Ok(o) => o,
        Err(()) => return,
    };

    let notifier = SessionNotifier::new(tx.clone(), session_id.clone());

    let summary_line = build_summary_line(outcome);
    let msg_id = uuid::Uuid::new_v4().to_string();
    let chunk = agent_client_protocol::schema::v1::ContentChunk::new(
        agent_client_protocol::schema::v1::ContentBlock::Text(
            agent_client_protocol::schema::v1::TextContent::new(summary_line),
        ),
    )
    .message_id(Some(agent_client_protocol::schema::v1::MessageId::new(
        msg_id,
    )));
    let msg_notif = SessionNotification::new(
        session_id.clone(),
        agent_client_protocol::schema::v1::SessionUpdate::AgentMessageChunk(chunk),
    );
    if let Err(e) = tx.send(SessionUpdateEnvelope::Session(msg_notif)) {
        warn!(thread_id = %thread_id, error = %e, "Failed to send review summary chunk");
    }

    let meta = build_review_meta(outcome, duration_ms);
    notifier.try_send_session_meta(meta);
}

/// Render the human-readable multi-line summary for the chat pane.
///
/// Examples:
///   Success with both kinds:
///   ```
///   Background review completed (1.2s):
///     📝 2 memories saved:
///        • Memory "debug-logging" created (345 chars)
///        • Memory "api-patterns" updated (+120 chars)
///     🔧 1 skill updated:
///        • Skill "react-testing" updated (+567 chars)
///   ```
///   Skipped:
///   ```
///   Background review skipped (session too short).
///   ```
///   No actions:
///   ```
///   Background review: nothing to save (0.5s).
///   ```
fn build_summary_line(outcome: &ReviewOutcome) -> String {
    let secs = outcome.duration_ms as f64 / 1000.0;

    // Skip scenario: keep it concise
    if outcome.skipped {
        let reason = outcome.skip_reason.as_deref().unwrap_or("skipped");
        return format!("Background review skipped ({}).", reason);
    }

    // Main header line
    let mut lines = vec![format!("Background review completed ({:.1}s):", secs)];

    // Group actions by type
    let memory_actions: Vec<_> = outcome
        .actions
        .iter()
        .filter(|a| a.kind.contains("memory") && a.succeeded)
        .collect();
    let skill_actions: Vec<_> = outcome
        .actions
        .iter()
        .filter(|a| a.kind.contains("skill") && a.succeeded)
        .collect();

    // Memory operations details
    if !memory_actions.is_empty() {
        lines.push(format!("  📝 {} memories saved:", memory_actions.len()));
        for action in &memory_actions {
            let action_summary = format_action_summary(action);
            lines.push(format!("     • {}", action_summary));
        }
    }

    // Skill operations details
    if !skill_actions.is_empty() {
        lines.push(format!("  🔧 {} skills updated:", skill_actions.len()));
        for action in &skill_actions {
            let action_summary = format_action_summary(action);
            lines.push(format!("     • {}", action_summary));
        }
    }

    // No actions scenario
    if memory_actions.is_empty() && skill_actions.is_empty() {
        lines.push("  • nothing to save".to_string());
    }

    lines.join("\n")
}

/// Format a single action's detailed summary.
/// Uses the curator-provided summary which already contains
/// create/update/delete info and character counts.
fn format_action_summary(action: &anureo_curator::ReviewActionSummary) -> String {
    action.summary.clone()
}

/// Build the `_meta.review` payload for `SessionInfoUpdate`.
///
/// Mirrors `apps/acp/src/protocol.rs` schema note for `_meta.review`:
///   status: "reviewed" | "skipped"
///   reviewed_at: RFC3339
///   memory_count / skill_count: usize
///   skip_reason?: string (when status == "skipped")
///   duration_ms: u64
fn build_review_meta(outcome: &ReviewOutcome, duration_ms: u64) -> Meta {
    let mut meta = Meta::new();
    let status = if outcome.skipped {
        "skipped"
    } else {
        "reviewed"
    };
    let mut payload = serde_json::json!({
        "status": status,
        "reviewed_at": chrono::Utc::now().to_rfc3339(),
        "memory_count": outcome.memory_count,
        "skill_count": outcome.skill_count,
        "duration_ms": duration_ms,
    });
    if let Some(reason) = &outcome.skip_reason {
        payload.as_object_mut().unwrap().insert(
            "skip_reason".to_string(),
            serde_json::Value::String(reason.clone()),
        );
    }
    meta.insert("review".to_string(), payload);
    meta
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_memory_only() {
        assert_eq!(
            scope_to_review_config(&Some("memory".into())),
            (true, false)
        );
    }

    #[test]
    fn scope_skills_only() {
        assert_eq!(
            scope_to_review_config(&Some("skills".into())),
            (false, true)
        );
        assert_eq!(scope_to_review_config(&Some("skill".into())), (false, true));
    }

    #[test]
    fn scope_none_defaults_to_both() {
        assert_eq!(scope_to_review_config(&None), (true, true));
    }

    #[test]
    fn scope_unknown_defaults_to_both() {
        assert_eq!(
            scope_to_review_config(&Some("unknown".into())),
            (true, true)
        );
    }

    #[test]
    fn dedup_blocks_concurrent_acquires_for_same_session() {
        // Mirrors the dedup logic in `spawn_inprocess_review`: two consecutive
        // `try_acquire` calls for the same thread_id must yield one guard and
        // one rejection. Slot count must drop back to zero after the guard
        // is dropped, so a follow-up call can succeed again.
        let registry = global_registry();
        let session = format!("dedup-test-{}", uuid::Uuid::new_v4());

        let first = registry.try_acquire(session.clone()).expect("first");
        assert!(registry.active_sessions() >= 1);
        let second = registry.try_acquire(session.clone());
        assert!(
            second.is_none(),
            "second acquire for the same session must fail"
        );
        drop(first);
        assert!(
            registry.try_acquire(session).is_some(),
            "slot must be released after drop"
        );
    }

    #[test]
    fn summary_line_for_reviewed_with_both_kinds() {
        let outcome = ReviewOutcome {
            summary: "memory(x) · skill(y)".into(),
            reply: "ok".into(),
            actions: vec![
                anureo_curator::ReviewActionSummary {
                    kind: "memory_create".to_string(),
                    target: "debug-logging".to_string(),
                    summary: "Memory 'debug-logging' created (345 chars)".to_string(),
                    succeeded: true,
                },
                anureo_curator::ReviewActionSummary {
                    kind: "skill_update".to_string(),
                    target: "react-testing".to_string(),
                    summary: "Skill 'react-testing' updated (+567 chars)".to_string(),
                    succeeded: true,
                },
            ],
            tool_violations: vec![],
            memory_count: 1,
            skill_count: 1,
            duration_ms: 1234,
            skipped: false,
            skip_reason: None,
            tokens: Default::default(),
        };
        let s = build_summary_line(&outcome);
        assert!(
            s.contains("Background review completed (1.2s):"),
            "header line: {s}"
        );
        assert!(s.contains("📝 1 memories saved:"), "memory section: {s}");
        assert!(
            s.contains("Memory 'debug-logging' created (345 chars)"),
            "memory details: {s}"
        );
        assert!(s.contains("🔧 1 skills updated:"), "skill section: {s}");
        assert!(
            s.contains("Skill 'react-testing' updated (+567 chars)"),
            "skill details: {s}"
        );
    }

    #[test]
    fn summary_line_for_reviewed_with_no_actions() {
        let outcome = ReviewOutcome {
            summary: "".into(),
            reply: "nothing".into(),
            actions: vec![],
            tool_violations: vec![],
            memory_count: 0,
            skill_count: 0,
            duration_ms: 500,
            skipped: false,
            skip_reason: None,
            tokens: Default::default(),
        };
        let s = build_summary_line(&outcome);
        assert!(s.contains("nothing to save"), "got: {s}");
    }

    #[test]
    fn summary_line_for_skipped() {
        let outcome = ReviewOutcome {
            summary: "".into(),
            reply: "".into(),
            actions: vec![],
            tool_violations: vec![],
            memory_count: 0,
            skill_count: 0,
            duration_ms: 0,
            skipped: true,
            skip_reason: Some("session_too_short".into()),
            tokens: Default::default(),
        };
        let s = build_summary_line(&outcome);
        assert!(s.contains("skipped"), "got: {s}");
        assert!(s.contains("session_too_short"), "got: {s}");
    }

    #[test]
    fn review_meta_marked_reviewed_when_actions_present() {
        let outcome = ReviewOutcome {
            summary: "".into(),
            reply: "".into(),
            actions: vec![],
            tool_violations: vec![],
            memory_count: 3,
            skill_count: 0,
            duration_ms: 2000,
            skipped: false,
            skip_reason: None,
            tokens: Default::default(),
        };
        let meta = build_review_meta(&outcome, 2000);
        let v = meta.get("review").expect("review key");
        assert_eq!(v["status"], "reviewed");
        assert_eq!(v["memory_count"], 3);
        assert_eq!(v["skill_count"], 0);
        assert_eq!(v["duration_ms"], 2000);
        assert!(
            v.get("skip_reason").is_none(),
            "skip_reason absent on success"
        );
    }

    #[test]
    fn review_meta_marked_skipped_carries_reason() {
        let outcome = ReviewOutcome {
            summary: "".into(),
            reply: "".into(),
            actions: vec![],
            tool_violations: vec![],
            memory_count: 0,
            skill_count: 0,
            duration_ms: 0,
            skipped: true,
            skip_reason: Some("llm_error: rate_limited".into()),
            tokens: Default::default(),
        };
        let meta = build_review_meta(&outcome, 0);
        let v = meta.get("review").expect("review key");
        assert_eq!(v["status"], "skipped");
        assert_eq!(v["skip_reason"], "llm_error: rate_limited");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notify_completion_emits_chunk_and_session_info_with_meta() {
        use agent_client_protocol::schema::v1::{SessionId, SessionUpdate};

        let (tx, mut rx) = mpsc::unbounded_channel::<SessionUpdateEnvelope>();
        let session_id = SessionId::new("test-session-1");

        let outcome = ReviewOutcome {
            summary: "".into(),
            reply: "ok".into(),
            actions: vec![
                anureo_curator::ReviewActionSummary {
                    kind: "memory_create".to_string(),
                    target: "debug-logging".to_string(),
                    summary: "Memory 'debug-logging' created (345 chars)".to_string(),
                    succeeded: true,
                },
                anureo_curator::ReviewActionSummary {
                    kind: "skill_update".to_string(),
                    target: "react-testing".to_string(),
                    summary: "Skill 'react-testing' updated (+567 chars)".to_string(),
                    succeeded: true,
                },
            ],
            tool_violations: vec![],
            memory_count: 1,
            skill_count: 1,
            duration_ms: 750,
            skipped: false,
            skip_reason: None,
            tokens: Default::default(),
        };

        notify_completion(Some(&tx), Some(&session_id), "thread-1", Ok(&outcome), 750);

        // First notification: AgentMessageChunk with the summary line.
        let first = rx.recv().await.expect("chunk notification");
        let SessionUpdateEnvelope::Session(first) = first else {
            panic!("expected Session envelope");
        };
        match first.update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                let text = match chunk.content {
                    agent_client_protocol::schema::v1::ContentBlock::Text(t) => t.text,
                    other => panic!("expected TextContent, got {other:?}"),
                };
                assert!(
                    text.contains("Background review completed (0.8s):"),
                    "header line: {text}"
                );
                assert!(
                    text.contains("📝 1 memories saved:"),
                    "memory section: {text}"
                );
                assert!(
                    text.contains("Memory 'debug-logging' created (345 chars)"),
                    "memory details: {text}"
                );
                assert!(
                    text.contains("🔧 1 skills updated:"),
                    "skill section: {text}"
                );
                assert!(
                    text.contains("Skill 'react-testing' updated (+567 chars)"),
                    "skill details: {text}"
                );
                assert!(
                    chunk.message_id.is_some(),
                    "message_id must be set so the client treats it as a discrete turn"
                );
            }
            other => panic!("expected AgentMessageChunk, got {other:?}"),
        }

        // Second notification: SessionInfoUpdate with _meta.review.
        let second = rx.recv().await.expect("session_info notification");
        let SessionUpdateEnvelope::Session(second) = second else {
            panic!("expected Session envelope");
        };
        match second.update {
            SessionUpdate::SessionInfoUpdate(info) => {
                let m = info.meta.expect("meta must be present");
                let v = m.get("review").expect("_meta.review");
                assert_eq!(v["status"], "reviewed");
                assert_eq!(v["memory_count"], 1);
                assert_eq!(v["skill_count"], 1);
                assert_eq!(v["duration_ms"], 750);
            }
            other => panic!("expected SessionInfoUpdate, got {other:?}"),
        }

        // Drain: nothing else should have been emitted.
        assert!(rx.try_recv().is_err(), "exactly two notifications expected");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn notify_completion_noop_without_channel() {
        let outcome = ReviewOutcome {
            summary: "".into(),
            reply: "".into(),
            actions: vec![],
            tool_violations: vec![],
            memory_count: 0,
            skill_count: 0,
            duration_ms: 0,
            skipped: true,
            skip_reason: Some("noop".into()),
            tokens: Default::default(),
        };
        // Both args None → must not panic.
        notify_completion(None, None, "thread-2", Ok(&outcome), 0);
        // tx without session_id → also a no-op (we don't know who to address).
        let (tx, _rx) = mpsc::unbounded_channel::<SessionUpdateEnvelope>();
        notify_completion(Some(&tx), None, "thread-2", Ok(&outcome), 0);
    }
}

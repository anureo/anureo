//! Async agent registry: tracks background agent invocations.
//!
//! When `agent` tool is called with `background: true` (or times out in sync mode),
//! the agent is spawned in a tokio task and registered here. The caller receives an
//! `agent_id` immediately. Later, `agent_get` with the `agent_id` retrieves the result.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::{json, Value};
use tokio::task::AbortHandle;

use super::identity::SubagentIdentity;

/// Statistics from a completed/failed agent execution.
#[derive(Debug, Clone, Default)]
pub struct AgentCompletionStats {
    pub turn_count: u32,
    pub total_tokens: u32,
    pub tool_calls_count: u32,
}

/// Lifecycle phase of an async agent invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    Running {
        started_at: String,
    },
    /// Sync call timed out, continuing in background.
    Background {
        started_at: String,
        timed_out_at: String,
    },
    Completed {
        result: String,
        turn_count: u32,
        total_tokens: u32,
        tool_calls_count: u32,
        started_at: String,
        completed_at: String,
        duration_ms: u64,
    },
    Failed {
        error: String,
        turn_count: u32,
        total_tokens: u32,
        tool_calls_count: u32,
        started_at: String,
        failed_at: String,
        duration_ms: u64,
    },
}

impl AgentStatus {
    /// Returns `true` if the agent is still active (Running or Background).
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            AgentStatus::Running { .. } | AgentStatus::Background { .. }
        )
    }

    /// Extract `started_at` if the status is Running or Background.
    fn started_at_if_active(&self) -> Option<String> {
        match self {
            AgentStatus::Running { started_at } | AgentStatus::Background { started_at, .. } => {
                Some(started_at.clone())
            }
            _ => None,
        }
    }
}

/// Maximum number of terminal (completed/failed) entries to retain.
/// Older entries beyond this cap are auto-evicted.
const MAX_TERMINAL_ENTRIES: usize = 50;

/// A tracked entry in the registry.
#[derive(Debug, Clone)]
pub struct AgentEntry {
    pub agent_id: String,
    pub agent_name: String,
    pub thread_id: String,
    pub status: AgentStatus,

    /// The identity graph of this invocation (contract §3.1); `None` for
    /// legacy `register()` callers. Consumed by the lifecycle sink (Phase 2)
    /// and metadata projection (Phase 3).
    pub identity: Option<Arc<SubagentIdentity>>,

    /// AbortHandle to cancel the spawned tokio task.
    pub abort_handle: Option<AbortHandle>,
}

impl AgentEntry {
    /// Return a copy with internal handles stripped (for public API responses).
    /// The identity is metadata — it is kept, not stripped.
    fn strip_handles(&self) -> Self {
        Self {
            agent_id: self.agent_id.clone(),
            agent_name: self.agent_name.clone(),
            thread_id: self.thread_id.clone(),
            status: self.status.clone(),
            identity: self.identity.clone(),
            abort_handle: None,
        }
    }

    /// Unified JSON representation, shared by agent_get, agent_cancel, and thread_get.
    pub fn to_json(&self) -> Value {
        let status_str = match &self.status {
            AgentStatus::Running { .. } => "running",
            AgentStatus::Background { .. } => "background",
            AgentStatus::Completed { .. } => "completed",
            AgentStatus::Failed { .. } => "failed",
        };

        let mut map = json!({
            "agent_id": self.agent_id,
            "agent_name": self.agent_name,
            "thread_id": self.thread_id,
            "status": status_str,
        });

        match &self.status {
            AgentStatus::Running { started_at } => {
                map["started_at"] = json!(started_at);
            }
            AgentStatus::Background {
                started_at,
                timed_out_at,
            } => {
                map["started_at"] = json!(started_at);
                map["timed_out_at"] = json!(timed_out_at);
            }
            AgentStatus::Completed {
                result,
                turn_count,
                total_tokens,
                tool_calls_count,
                started_at,
                completed_at,
                duration_ms,
            } => {
                map["started_at"] = json!(started_at);
                map["completed_at"] = json!(completed_at);
                map["duration_ms"] = json!(duration_ms);
                map["result"] = json!(result);
                map["turn_count"] = json!(turn_count);
                map["total_tokens"] = json!(total_tokens);
                map["tool_calls_count"] = json!(tool_calls_count);
            }
            AgentStatus::Failed {
                error,
                turn_count,
                total_tokens,
                tool_calls_count,
                started_at,
                failed_at,
                duration_ms,
            } => {
                map["started_at"] = json!(started_at);
                map["failed_at"] = json!(failed_at);
                map["duration_ms"] = json!(duration_ms);
                map["error"] = json!(error);
                map["turn_count"] = json!(turn_count);
                map["total_tokens"] = json!(total_tokens);
                map["tool_calls_count"] = json!(tool_calls_count);
            }
        }

        map
    }
}

/// Thread-safe registry for async agent invocations.
#[derive(Debug, Clone, Default)]
pub struct AsyncAgentRegistry {
    inner: Arc<RwLock<HashMap<String, AgentEntry>>>,
}

/// RAII reservation returned by [`AsyncAgentRegistry::reserve`].
///
/// The entry is inserted as `Running` at reserve time; [`AgentReservation::commit`]
/// attaches the abort handle once the task is spawned. Dropping without commit
/// removes the entry — half-failed spawns leak nothing (codex SpawnReservation
/// pattern, evolution A1).
#[derive(Debug)]
pub struct AgentReservation {
    registry: AsyncAgentRegistry,
    agent_id: String,
    committed: bool,
}

impl AgentReservation {
    /// The invocation id this reservation holds.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Commit the reservation, attaching the spawned task's abort handle.
    pub fn commit(mut self, abort_handle: Option<AbortHandle>) {
        if let Some(entry) = self.registry.inner.write().get_mut(&self.agent_id) {
            entry.abort_handle = abort_handle;
        }
        self.committed = true;
    }
}

impl Drop for AgentReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.registry.inner.write().remove(&self.agent_id);
            tracing::debug!(
                agent_id = %self.agent_id,
                "Spawn reservation dropped without commit — entry removed"
            );
        }
    }
}

impl AsyncAgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve an entry as `Running` **before** spawning the task (evolution A1).
    ///
    /// Returns an RAII [`AgentReservation`]: `commit()` attaches the abort
    /// handle after spawn succeeds; dropping without commit removes the entry,
    /// so a failed spawn path leaves no permanently-Running ghosts.
    pub fn reserve(
        &self,
        identity: Arc<SubagentIdentity>,
        thread_id: String,
    ) -> AgentReservation {
        let agent_id = identity.invocation_id.clone();
        let entry = AgentEntry {
            agent_id: agent_id.clone(),
            agent_name: identity.agent_name.clone(),
            thread_id,
            identity: Some(identity),
            status: AgentStatus::Running {
                started_at: iso8601_now(),
            },
            abort_handle: None,
        };
        tracing::debug!(agent_id = %agent_id, "Reserved async agent entry");
        self.inner.write().insert(agent_id.clone(), entry);
        AgentReservation {
            registry: self.clone(),
            agent_id,
            committed: false,
        }
    }

    /// Register a new agent as `Running` (legacy path — no invocation context).
    pub fn register(
        &self,
        agent_id: String,
        agent_name: String,
        thread_id: String,
        abort_handle: Option<AbortHandle>,
    ) {
        let identity = Arc::new(SubagentIdentity::synthetic(&agent_id, &agent_name));
        self.reserve(identity, thread_id).commit(abort_handle);
    }

    /// Mark a running agent as background (sync timeout occurred).
    pub fn mark_background(&self, agent_id: &str) {
        if let Some(entry) = self.inner.write().get_mut(agent_id) {
            let Some(started_at) = entry.status.started_at_if_active() else {
                return;
            };
            entry.status = AgentStatus::Background {
                started_at,
                timed_out_at: iso8601_now(),
            };
            tracing::debug!(agent_id = %agent_id, "Marked agent as background (sync timeout)");
        }
    }

    /// Abort a running agent's tokio task and mark it as Failed("cancelled").
    /// Returns `Err(message)` if the agent is not found or already terminal.
    pub fn cancel(&self, agent_id: &str) -> Result<(), String> {
        let abort_handle = {
            let mut guard = self.inner.write();
            match guard.get_mut(agent_id) {
                Some(e) if e.status.is_active() => e.abort_handle.take(),
                Some(_) => return Err("agent already completed or failed".into()),
                None => return Err("agent_id not found".into()),
            }
        };

        if let Some(handle) = abort_handle {
            handle.abort();
            tracing::info!(agent_id = %agent_id, "Agent task aborted via agent_cancel tool");
        }

        if let Some(entry) = self.inner.write().get_mut(agent_id) {
            let Some(started_at) = entry.status.started_at_if_active() else {
                return Ok(()); // Race: went terminal between take and mark.
            };
            let failed_at = iso8601_now();
            let duration_ms = parse_duration_ms(&started_at, &failed_at);
            entry.status = AgentStatus::Failed {
                error: "cancelled".into(),
                turn_count: 0,
                total_tokens: 0,
                tool_calls_count: 0,
                started_at,
                failed_at,
                duration_ms,
            };
        }
        self.evict_if_needed();
        Ok(())
    }

    /// Update an agent's status to completed.
    pub fn complete(&self, agent_id: &str, result: String, stats: AgentCompletionStats) {
        if let Some(entry) = self.inner.write().get_mut(agent_id) {
            let Some(started_at) = entry.status.started_at_if_active() else {
                return; // Already terminal — don't overwrite.
            };
            let completed_at = iso8601_now();
            let duration_ms = parse_duration_ms(&started_at, &completed_at);
            entry.status = AgentStatus::Completed {
                result,
                turn_count: stats.turn_count,
                total_tokens: stats.total_tokens,
                tool_calls_count: stats.tool_calls_count,
                started_at,
                completed_at,
                duration_ms,
            };
            tracing::debug!(agent_id = %agent_id, "Async agent completed");
        }
        self.evict_if_needed();
    }

    /// Update an agent's status to failed.
    pub fn fail(&self, agent_id: &str, error: String, stats: AgentCompletionStats) {
        if let Some(entry) = self.inner.write().get_mut(agent_id) {
            let Some(started_at) = entry.status.started_at_if_active() else {
                return;
            };
            let failed_at = iso8601_now();
            let duration_ms = parse_duration_ms(&started_at, &failed_at);
            entry.status = AgentStatus::Failed {
                error,
                turn_count: stats.turn_count,
                total_tokens: stats.total_tokens,
                tool_calls_count: stats.tool_calls_count,
                started_at,
                failed_at,
                duration_ms,
            };
            tracing::debug!(agent_id = %agent_id, "Async agent failed");
        }
        self.evict_if_needed();
    }

    /// Get a single agent entry by ID (strips internal handles).
    pub fn get(&self, agent_id: &str) -> Option<AgentEntry> {
        self.inner.read().get(agent_id).map(|e| e.strip_handles())
    }

    /// List all entries (all statuses).
    pub fn list_all(&self) -> Vec<AgentEntry> {
        self.inner
            .read()
            .values()
            .map(|e| e.strip_handles())
            .collect()
    }

    /// List only running/background agents.
    pub fn list_running(&self) -> Vec<AgentEntry> {
        self.inner
            .read()
            .values()
            .filter(|e| e.status.is_active())
            .map(|e| e.strip_handles())
            .collect()
    }

    /// Evict oldest terminal entries if the registry exceeds `MAX_TERMINAL_ENTRIES`.
    /// Called after every terminal transition.
    fn evict_if_needed(&self) {
        let mut guard = self.inner.write();
        let terminal_count = guard.values().filter(|e| !e.status.is_active()).count();
        if terminal_count <= MAX_TERMINAL_ENTRIES {
            return;
        }

        // Collect terminal entries sorted by completion/failure time (oldest first).
        let mut terminal_ids: Vec<(String, String)> = guard
            .iter()
            .filter_map(|(id, e)| {
                let timestamp = match &e.status {
                    AgentStatus::Completed { completed_at, .. } => completed_at.clone(),
                    AgentStatus::Failed { failed_at, .. } => failed_at.clone(),
                    _ => return None,
                };
                Some((id.clone(), timestamp))
            })
            .collect();
        terminal_ids.sort_by(|a, b| a.1.cmp(&b.1));

        let to_remove = terminal_count - MAX_TERMINAL_ENTRIES;
        for (id, _) in terminal_ids.into_iter().take(to_remove) {
            guard.remove(&id);
            tracing::debug!(agent_id = %id, "Evicted old terminal entry from registry");
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Generate ISO 8601 timestamp in UTC (e.g. "2025-01-15T10:30:45.123Z").
pub(super) fn iso8601_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Parse duration between two ISO 8601 timestamps.
fn parse_duration_ms(start: &str, end: &str) -> u64 {
    let parse = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.with_timezone(&chrono::Utc))
    };
    match (parse(start), parse(end)) {
        (Some(s), Some(e)) => e.signed_duration_since(s).num_milliseconds().max(0) as u64,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_get() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);

        let entry = reg.get("a1").unwrap();
        assert_eq!(entry.agent_id, "a1");
        assert_eq!(entry.agent_name, "dev");
        assert_eq!(entry.thread_id, "t1");
        assert!(matches!(entry.status, AgentStatus::Running { .. }));
    }

    #[test]
    fn complete_transitions_from_running() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.complete(
            "a1",
            "done".into(),
            AgentCompletionStats {
                turn_count: 3,
                total_tokens: 100,
                tool_calls_count: 2,
            },
        );

        let entry = reg.get("a1").unwrap();
        match entry.status {
            AgentStatus::Completed {
                result, turn_count, ..
            } => {
                assert_eq!(result, "done");
                assert_eq!(turn_count, 3);
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn complete_does_not_overwrite_terminal() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.complete("a1", "first".into(), AgentCompletionStats::default());
        reg.complete("a1", "second".into(), AgentCompletionStats::default());

        let entry = reg.get("a1").unwrap();
        match entry.status {
            AgentStatus::Completed { result, .. } => assert_eq!(result, "first"),
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn fail_transitions_from_running() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.fail("a1", "boom".into(), AgentCompletionStats::default());

        let entry = reg.get("a1").unwrap();
        match entry.status {
            AgentStatus::Failed { error, .. } => assert_eq!(error, "boom"),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn mark_background_from_running() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.mark_background("a1");

        let entry = reg.get("a1").unwrap();
        assert!(matches!(entry.status, AgentStatus::Background { .. }));
    }

    #[test]
    fn cancel_on_terminal_returns_err() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.complete("a1", "done".into(), AgentCompletionStats::default());
        assert!(reg.cancel("a1").is_err());
    }

    #[test]
    fn cancel_nonexistent_returns_err() {
        let reg = AsyncAgentRegistry::new();
        assert!(reg.cancel("nope").is_err());
    }

    #[test]
    fn list_running_filters_terminal() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.register("a2".into(), "explore".into(), "t1".into(), None);
        reg.complete("a2", "done".into(), AgentCompletionStats::default());

        let running = reg.list_running();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].agent_id, "a1");
    }

    #[test]
    fn get_nonexistent() {
        let reg = AsyncAgentRegistry::new();
        assert!(reg.get("nope").is_none());
    }

    #[test]
    fn evict_keeps_newest_terminal_entries() {
        let reg = AsyncAgentRegistry::new();
        // Insert MAX_TERMINAL_ENTRIES + 2 completed agents.
        for i in 0..(MAX_TERMINAL_ENTRIES + 2) {
            let id = format!("a{i}");
            reg.register(id.clone(), "dev".into(), "t1".into(), None);
            reg.complete(&id, "done".into(), AgentCompletionStats::default());
            // Small delay to ensure distinct timestamps for eviction ordering.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let all = reg.list_all();
        assert_eq!(all.len(), MAX_TERMINAL_ENTRIES);

        // The first two ("a0", "a1") should have been evicted (oldest).
        assert!(reg.get("a0").is_none());
        assert!(reg.get("a1").is_none());
        assert!(reg.get("a2").is_some());
    }

    #[test]
    fn evict_preserves_running_entries() {
        let reg = AsyncAgentRegistry::new();
        // Fill with terminal entries up to the cap.
        for i in 0..MAX_TERMINAL_ENTRIES {
            let id = format!("c{i}");
            reg.register(id.clone(), "dev".into(), "t1".into(), None);
            reg.complete(&id, "done".into(), AgentCompletionStats::default());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // Add 2 running agents.
        reg.register("r1".into(), "explore".into(), "t1".into(), None);
        reg.register("r2".into(), "explore".into(), "t1".into(), None);

        let all = reg.list_all();
        let running = reg.list_running();
        assert_eq!(running.len(), 2);
        assert!(all.len() <= MAX_TERMINAL_ENTRIES + 2);
        assert!(reg.get("r1").is_some());
        assert!(reg.get("r2").is_some());
    }

    /// A1: dropping a reservation without commit must remove the entry —
    /// the spawn/register race leaves no Running ghosts.
    #[test]
    fn reservation_drop_removes_entry() {
        let reg = AsyncAgentRegistry::new();
        let identity = Arc::new(SubagentIdentity::generate(None, "dev"));
        let res = reg.reserve(identity, "t1".into());
        let agent_id = res.agent_id().to_string();
        assert!(reg.get(&agent_id).is_some(), "entry visible while reserved");
        drop(res);
        assert!(reg.get(&agent_id).is_none(), "entry removed on un-committed drop");
    }

    /// A1: commit persists the entry and attaches the abort handle.
    #[test]
    fn reservation_commit_persists_entry() {
        let reg = AsyncAgentRegistry::new();
        let identity = Arc::new(SubagentIdentity::generate(None, "dev"));
        let res = reg.reserve(Arc::clone(&identity), "t1".into());
        res.commit(None);
        let entry = reg.get(&identity.invocation_id).unwrap();
        assert!(matches!(entry.status, AgentStatus::Running { .. }));
        assert_eq!(entry.identity.as_ref().map(|i| i.invocation_id.clone()), Some(identity.invocation_id.clone()));
    }

    /// Identity must survive strip_handles (it is metadata, not a handle).
    #[test]
    fn legacy_register_carries_synthetic_identity() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        let entry = reg.get("a1").unwrap();
        let identity = entry.identity.as_ref().expect("synthetic identity present");
        assert_eq!(identity.invocation_id, "a1");
        assert_eq!(identity.agent_name, "dev");
    }
}

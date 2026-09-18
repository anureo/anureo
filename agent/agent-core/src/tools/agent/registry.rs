//! Async agent registry: tracks background agent invocations.
//!
//! When `agent` tool is called with `background: true` (or times out in sync mode),
//! the agent is spawned in a tokio task and registered here. The caller receives an
//! `agent_id` immediately. Later, `agent_get` with the `agent_id` retrieves the result.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::{json, Value};
use tokio::task::AbortHandle;

use super::identity::SubagentIdentity;
use super::lifecycle::{SubagentLifecycleSink, SubagentOutcome};

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

/// Default cap on concurrently *executing* sub-agents (codex
/// AgentExecutionLimiter analog, evolution A2). Overridable via
/// `ReactBuildConfig.max_concurrent_subagents` / env `MAX_CONCURRENT_SUBAGENTS`.
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: usize = 4;

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
#[derive(Clone, Default)]
pub struct AsyncAgentRegistry {
    inner: Arc<RwLock<HashMap<String, AgentEntry>>>,
    /// Optional lifecycle bridge (contract §4). `None` = no-op (CLI/tests).
    sink: Option<Arc<dyn SubagentLifecycleSink>>,
    /// A2: concurrently executing sub-agents.
    active_executions: Arc<AtomicUsize>,
    /// A2: cap for `active_executions` (0 until configured; then >= 1).
    max_concurrent: Arc<AtomicUsize>,
}

impl std::fmt::Debug for AsyncAgentRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncAgentRegistry")
            .field("entries", &self.inner.read().len())
            .field("has_sink", &self.sink.is_some())
            .field(
                "active_executions",
                &self.active_executions.load(Ordering::Relaxed),
            )
            .field("max_concurrent", &self.max_concurrent.load(Ordering::Relaxed))
            .finish()
    }
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

/// RAII guard for one concurrently-executing sub-agent slot (A2).
/// Released on Drop — including on panic unwind and on the sync→background
/// timeout transition (the task is still running, the slot stays held).
#[derive(Debug)]
pub struct ExecutionGuard {
    registry: AsyncAgentRegistry,
}

impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.registry.active_executions.fetch_sub(1, Ordering::AcqRel);
    }
}

impl AsyncAgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a lifecycle bridge (contract §4). Returns `self` for chaining.
    pub fn with_sink(mut self, sink: Arc<dyn SubagentLifecycleSink>) -> Self {
        self.sink = Some(sink);
        self
    }

    /// A2: set the concurrent-execution cap (minimum 1). Clones share the
    /// atomics, so this applies to the logical registry (all clones).
    pub fn with_max_concurrent(self, max: usize) -> Self {
        self.max_concurrent.store(max.max(1), Ordering::Relaxed);
        self
    }

    /// A2: current cap on concurrently executing sub-agents.
    pub fn max_concurrent(&self) -> usize {
        let max = self.max_concurrent.load(Ordering::Relaxed);
        if max == 0 {
            DEFAULT_MAX_CONCURRENT_SUBAGENTS
        } else {
            max
        }
    }

    /// A2: number of sub-agent executions currently in flight.
    pub fn active_executions(&self) -> usize {
        self.active_executions.load(Ordering::Relaxed)
    }

    /// A2: cheap availability probe (for fast-failing tool responses).
    pub fn execution_available(&self) -> bool {
        self.active_executions() < self.max_concurrent()
    }

    /// A2: acquire one execution slot; `None` when the cap is exhausted.
    /// The returned guard releases the slot on Drop.
    pub fn try_begin_execution(&self) -> Option<ExecutionGuard> {
        let max = self.max_concurrent();
        let mut current = self.active_executions.load(Ordering::Relaxed);
        loop {
            if current >= max {
                return None;
            }
            match self.active_executions.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(ExecutionGuard {
                        registry: self.clone(),
                    })
                }
                Err(v) => current = v,
            }
        }
    }

    /// Emit `created` **before** the task spawns; an error aborts the
    /// invocation (contract §4 — no task, no durable ghost).
    pub(crate) async fn emit_created(&self, identity: &SubagentIdentity) -> Result<(), String> {
        match &self.sink {
            Some(sink) => sink.created(identity).await,
            None => Ok(()),
        }
    }

    /// Emit `running` after the task has been spawned and committed.
    pub(crate) async fn emit_running(&self, identity: &SubagentIdentity) {
        if let Some(sink) = &self.sink {
            if let Err(e) = sink.running(identity).await {
                tracing::warn!(
                    agent_id = %identity.invocation_id,
                    error = %e,
                    "lifecycle `running` emission failed"
                );
            }
        }
    }

    /// Fire-and-forget terminal emission. Exactly-once converges on the
    /// terminal-guard in `complete`/`fail`/`cancel` — each calls this at
    /// most once per invocation.
    fn emit_terminal_spawned(
        &self,
        agent_id: &str,
        identity: Option<Arc<SubagentIdentity>>,
        outcome: SubagentOutcome,
    ) {
        let Some(sink) = self.sink.clone() else {
            return;
        };
        let Some(identity) = identity else {
            tracing::debug!(
                agent_id = %agent_id,
                "terminal transition without identity; sink not notified"
            );
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                agent_id = %identity.invocation_id,
                "terminal emission outside tokio runtime; dropped"
            );
            return;
        };
        handle.spawn(async move {
            if let Err(e) = sink.terminal(&identity, &outcome).await {
                tracing::warn!(
                    agent_id = %identity.invocation_id,
                    error = %e,
                    "lifecycle `terminal` emission failed"
                );
            }
        });
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

        let mut identity = None;
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
            identity = entry.identity.clone();
        }
        self.emit_terminal_spawned(agent_id, identity, SubagentOutcome::Cancelled);
        self.evict_if_needed();
        Ok(())
    }

    /// Update an agent's status to completed.
    pub fn complete(&self, agent_id: &str, result: String, stats: AgentCompletionStats) {
        let identity = {
            let mut inner = self.inner.write();
            let Some(entry) = inner.get_mut(agent_id) else {
                return;
            };
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
            entry.identity.clone()
        };
        tracing::debug!(agent_id = %agent_id, "Async agent completed");
        self.emit_terminal_spawned(
            agent_id,
            identity,
            SubagentOutcome::Completed { stats },
        );
        self.evict_if_needed();
    }

    /// Update an agent's status to failed.
    pub fn fail(&self, agent_id: &str, error: String, stats: AgentCompletionStats) {
        let outcome_error = error.clone();
        let identity = {
            let mut inner = self.inner.write();
            let Some(entry) = inner.get_mut(agent_id) else {
                return;
            };
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
            entry.identity.clone()
        };
        tracing::debug!(agent_id = %agent_id, "Async agent failed");
        self.emit_terminal_spawned(
            agent_id,
            identity,
            SubagentOutcome::Failed {
                error: outcome_error,
            },
        );
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

    // --- lifecycle sink (contract §4) ---

    struct RecordingSink {
        events: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                events: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn snapshot(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl SubagentLifecycleSink for RecordingSink {
        async fn created(&self, id: &SubagentIdentity) -> Result<(), String> {
            self.events
                .lock()
                .unwrap()
                .push(format!("created:{}", id.invocation_id));
            Ok(())
        }

        async fn running(&self, id: &SubagentIdentity) -> Result<(), String> {
            self.events
                .lock()
                .unwrap()
                .push(format!("running:{}", id.invocation_id));
            Ok(())
        }

        async fn terminal(
            &self,
            id: &SubagentIdentity,
            outcome: &SubagentOutcome,
        ) -> Result<(), String> {
            self.events
                .lock()
                .unwrap()
                .push(format!("terminal:{}:{}", outcome.kind(), id.invocation_id));
            Ok(())
        }
    }

    async fn wait_for_events(sink: &RecordingSink, n: usize) {
        for _ in 0..200 {
            if sink.snapshot().len() >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("sink events did not reach {n}: {:?}", sink.snapshot());
    }

    /// Contract §4: created → running → terminal, in order; the terminal
    /// guard guarantees exactly one terminal emission even across repeated
    /// terminal transitions.
    #[tokio::test]
    async fn lifecycle_ordering_and_exactly_once_terminal() {
        let sink = RecordingSink::new();
        let reg =
            AsyncAgentRegistry::new().with_sink(Arc::clone(&sink) as Arc<dyn SubagentLifecycleSink>);
        let identity = Arc::new(SubagentIdentity::generate(None, "dev"));

        let res = reg.reserve(Arc::clone(&identity), "t1".into());
        reg.emit_created(&identity).await.unwrap();
        res.commit(None);
        reg.emit_running(&identity).await;

        reg.complete(&identity.invocation_id, "done".into(), AgentCompletionStats::default());
        // Second attempt must not emit a second terminal.
        reg.complete(&identity.invocation_id, "done2".into(), AgentCompletionStats::default());

        wait_for_events(&sink, 3).await;
        assert_eq!(
            sink.snapshot(),
            vec![
                format!("created:{}", identity.invocation_id),
                format!("running:{}", identity.invocation_id),
                format!("terminal:completed:{}", identity.invocation_id),
            ]
        );
        // Room for a (wrong) second emission to surface.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert_eq!(sink.snapshot().len(), 3, "exactly one terminal");
    }

    /// Cancel path produces a Cancelled terminal via the same guard.
    #[tokio::test]
    async fn cancel_emits_cancelled_terminal() {
        let sink = RecordingSink::new();
        let reg =
            AsyncAgentRegistry::new().with_sink(Arc::clone(&sink) as Arc<dyn SubagentLifecycleSink>);
        let identity = Arc::new(SubagentIdentity::generate(None, "dev"));

        reg.reserve(Arc::clone(&identity), "t1".into()).commit(None);
        reg.emit_created(&identity).await.unwrap();
        reg.emit_running(&identity).await;
        reg.cancel(&identity.invocation_id).unwrap();

        wait_for_events(&sink, 3).await;
        let events = sink.snapshot();
        assert!(
            events.last().unwrap().contains("terminal:cancelled"),
            "last event should be cancelled terminal, got {events:?}"
        );
    }

    /// created rejection (Phase 2: durable child-session failure) surfaces
    /// to the caller so the invocation aborts — no task, no ghost.
    #[tokio::test]
    async fn created_rejection_surfaces_to_caller() {
        struct RejectingSink;
        #[async_trait::async_trait]
        impl SubagentLifecycleSink for RejectingSink {
            async fn created(&self, _id: &SubagentIdentity) -> Result<(), String> {
                Err("child session rejected".into())
            }
            async fn running(&self, _id: &SubagentIdentity) -> Result<(), String> {
                Ok(())
            }
            async fn terminal(
                &self,
                _id: &SubagentIdentity,
                _outcome: &SubagentOutcome,
            ) -> Result<(), String> {
                Ok(())
            }
        }
        let reg = AsyncAgentRegistry::new().with_sink(Arc::new(RejectingSink));
        let identity = SubagentIdentity::generate(None, "dev");
        let err = reg.emit_created(&identity).await.unwrap_err();
        assert!(err.contains("rejected"));
    }

    /// A2: execution cap enforces the concurrent limit; Drop releases slots.
    #[test]
    fn execution_limiter_caps_and_releases() {
        let reg = AsyncAgentRegistry::new().with_max_concurrent(2);
        let g1 = reg.try_begin_execution();
        let g2 = reg.try_begin_execution();
        assert!(g1.is_some() && g2.is_some(), "two slots available");
        assert_eq!(reg.active_executions(), 2);
        assert!(!reg.execution_available());
        assert!(reg.try_begin_execution().is_none(), "cap exhausted");
        drop(g1);
        assert_eq!(reg.active_executions(), 1);
        assert!(reg.try_begin_execution().is_some(), "slot released on drop");
    }

    /// A2: default cap is 4; a shared-registry clone sees the same slot state.
    #[test]
    fn execution_limiter_default_and_shared_state() {
        let reg = AsyncAgentRegistry::new();
        assert_eq!(reg.max_concurrent(), DEFAULT_MAX_CONCURRENT_SUBAGENTS);
        let clone = reg.clone();
        let _g = reg.try_begin_execution().unwrap();
        assert_eq!(clone.active_executions(), 1);
        assert!(clone.execution_available());
    }
}

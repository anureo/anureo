//! `SubagentLifecycleSink` — the agent-core side of the lifecycle bridge
//! (contract §4). The registry owns the sink and emits exactly-once terminal
//! events; runtimes (ACP in Phase 2) implement the sink to register durable
//! child sessions.
//!
//! Semantics:
//! - `created` fires **before** the sub-agent task is spawned; an error
//!   aborts the invocation (no task, no durable ghost).
//! - `running` fires once the task has been spawned and its reservation
//!   committed.
//! - `terminal` fires exactly once at the terminal transition. The
//!   "exactly-once" invariant converges on the registry's existing
//!   terminal-guard (`complete`/`fail` refuse to overwrite a terminal
//!   state), so the sink can never observe two terminals for one
//!   invocation.
//!
//! The sink never receives prompts, model output, or secrets (contract §4).

use async_trait::async_trait;

use super::identity::SubagentIdentity;
use super::registry::AgentCompletionStats;

/// Terminal outcome of one sub-agent invocation.
#[derive(Debug, Clone)]
pub enum SubagentOutcome {
    Completed { stats: AgentCompletionStats },
    Failed { error: String },
    Cancelled,
}

impl SubagentOutcome {
    /// Stable kind string for logs/tests.
    pub fn kind(&self) -> &'static str {
        match self {
            SubagentOutcome::Completed { .. } => "completed",
            SubagentOutcome::Failed { .. } => "failed",
            SubagentOutcome::Cancelled => "cancelled",
        }
    }
}

/// Lifecycle bridge trait. Implemented by runtimes; owned by the registry.
#[async_trait]
pub trait SubagentLifecycleSink: Send + Sync {
    /// The invocation has been accepted and is about to spawn.
    async fn created(&self, identity: &SubagentIdentity) -> Result<(), String>;

    /// The task has been spawned and its reservation committed.
    async fn running(&self, identity: &SubagentIdentity) -> Result<(), String>;

    /// The invocation reached a terminal state. Exactly once.
    async fn terminal(
        &self,
        identity: &SubagentIdentity,
        outcome: &SubagentOutcome,
    ) -> Result<(), String>;
}

/// Default no-op sink (CLI runs, tests). Phase 2 replaces it with the ACP
/// runtime implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopSubagentLifecycleSink;

#[async_trait]
impl SubagentLifecycleSink for NoopSubagentLifecycleSink {
    async fn created(&self, _identity: &SubagentIdentity) -> Result<(), String> {
        Ok(())
    }

    async fn running(&self, _identity: &SubagentIdentity) -> Result<(), String> {
        Ok(())
    }

    async fn terminal(
        &self,
        _identity: &SubagentIdentity,
        _outcome: &SubagentOutcome,
    ) -> Result<(), String> {
        Ok(())
    }
}

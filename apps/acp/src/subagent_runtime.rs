//! `AcpSubagentRuntime` — the ACP implementation of the sub-agent lifecycle
//! bridge (contract §4/§5). Registers every delegated sub-agent as a durable
//! first-class child session:
//!
//! - `created`: in-memory entry + canonical SessionIndex record
//!   (`parent_session_id` set) + child `session_config.current_agent` pinned
//!   to the delegated agent so continuing the conversation (E1-2) uses the
//!   delegated profile.
//! - `terminal`: `finish_subagent` — subagent metadata envelope and
//!   lifecycle convergence in ONE SQLite transaction (§5.4).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::json;

use agent::tools::agent::identity::SubagentIdentity;
use agent::tools::agent::lifecycle::{SubagentLifecycleSink, SubagentOutcome};

use crate::session::SessionStore;
use crate::session_repository::SessionRepository;
use crate::session::SessionId;

/// Versioned subagent metadata envelope (contract §6.1/§6.2).
fn subagent_envelope(
    identity: &SubagentIdentity,
    status: &str,
    stats: Option<&agent::tools::agent::registry::AgentCompletionStats>,
    error: Option<&str>,
) -> serde_json::Value {
    let stats_json = stats.map(|s| {
        json!({
            "turnCount": s.turn_count,
            "totalTokens": s.total_tokens,
            "toolCallsCount": s.tool_calls_count,
        })
    });
    json!({
        "subagent": {
            "version": 1,
            "invocationId": identity.invocation_id,
            "sessionId": identity.child_session_id,
            "parentSessionId": identity.parent_session_id,
            "parentToolCallId": identity.parent_tool_call_id,
            "agent": identity.agent_name,
            "model": serde_json::Value::Null,
            "depth": identity.depth,
            "status": status,
            "stats": stats_json,
            "error": error,
        }
    })
}

/// Durable lifecycle bridge backed by the ACP session store/repository.
#[derive(Clone)]
pub struct AcpSubagentRuntime {
    store: Arc<SessionStore>,
    repository: SessionRepository,
}

impl AcpSubagentRuntime {
    pub fn new(store: Arc<SessionStore>, repository: SessionRepository) -> Self {
        Self { store, repository }
    }
}

#[async_trait]
impl SubagentLifecycleSink for AcpSubagentRuntime {
    async fn created(&self, identity: &SubagentIdentity) -> Result<(), String> {
        let parent_session_id = identity
            .parent_session_id
            .clone()
            .ok_or_else(|| "sub-agent has no parent session id (CLI run)".to_string())?;
        let parent_sid = SessionId(parent_session_id.clone());
        let parent = self
            .store
            .get(&parent_sid)
            .ok_or_else(|| format!("parent session {} not found", parent_session_id))?;
        let cwd: PathBuf = parent.working_directory.clone().ok_or_else(|| {
            format!("parent session {} has no working directory", parent_session_id)
        })?;

        // 1) In-memory entry (idempotent: same id returns the existing entry).
        let child_sid = SessionId(identity.child_session_id.clone());
        self.store.create_with_owner(
            child_sid.clone(),
            Some(cwd.clone()),
            identity.child_thread_id.clone(),
            parent.owner_principal.clone(),
        );

        // 2) Durable canonical index record with parent relation + metadata.
        let metadata = subagent_envelope(identity, "created", None, None);
        self.repository
            .insert_index_record(
                &identity.child_session_id,
                &identity.child_thread_id,
                parent.owner_principal.as_str(),
                &cwd,
                Some(&parent_session_id),
                None,
                Some(&metadata),
            )
            .map_err(|e| format!("failed to index child session: {}", e))?;

        // 3) E1-2①: continuing the conversation uses the delegated agent.
        self.store
            .update_session_config(&child_sid, |config| {
                config.current_agent = identity.agent_name.clone();
            });

        Ok(())
    }

    async fn running(&self, _identity: &SubagentIdentity) -> Result<(), String> {
        // In-memory state is already Running; no additional durable write.
        Ok(())
    }

    async fn terminal(
        &self,
        identity: &SubagentIdentity,
        outcome: &SubagentOutcome,
    ) -> Result<(), String> {
        let (status, stats, error) = match outcome {
            SubagentOutcome::Completed { stats } => ("completed", Some(stats), None),
            SubagentOutcome::Failed { error } => ("failed", None, Some(error.as_str())),
            SubagentOutcome::Cancelled => ("cancelled", None, None),
        };
        let metadata = subagent_envelope(identity, status, stats, error);
        self.repository
            .finish_subagent(&identity.child_session_id, &metadata.to_string())
            .map_err(|e| format!("failed to converge child session terminal state: {}", e))?;
        Ok(())
    }
}

/// Shared helper: build the runtime as a sink object.
pub fn acp_subagent_runtime(
    store: Arc<SessionStore>,
    repository: SessionRepository,
) -> Arc<dyn SubagentLifecycleSink> {
    Arc::new(AcpSubagentRuntime::new(store, repository))
}

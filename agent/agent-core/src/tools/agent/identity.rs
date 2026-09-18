//! `SubagentIdentity` — the complete identity graph of one `agent` tool
//! invocation (contract §3.1). Generated **once** at the invocation entry,
//! then passed down — never recomputed (§3.2 invariant 2).

use std::sync::atomic::{AtomicU64, Ordering};

use tool_core::ToolCallContext;

/// Global sequence for unique invocation/thread ID composition
/// (contract §3.2 invariants 1/3).
static ASYNC_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_seq() -> u64 {
    ASYNC_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// Complete identity of a sub-agent invocation.
#[derive(Debug, Clone)]
pub struct SubagentIdentity {
    /// Tool-facing id — the `agent_id` returned by the `agent` tool and the
    /// registry key. Opaque string; consumers must not parse it.
    pub invocation_id: String,
    /// ACP session of the parent (`None` in CLI runs).
    pub parent_session_id: Option<String>,
    /// Parent checkpoint thread.
    pub parent_thread_id: Option<String>,
    /// The tool call that triggered this invocation (contract §3.1
    /// `parent_tool_call_id`).
    pub parent_tool_call_id: Option<String>,
    /// Durable child session id (UUID v4; consumed by ACP child-session
    /// registration in Phase 2).
    pub child_session_id: String,
    /// Child checkpoint thread — includes seq so concurrent/repeated
    /// invocations with the same name/depth never collide (GAP-02).
    pub child_thread_id: String,
    pub agent_name: String,
    /// Child depth = parent depth + 1 (saturating).
    pub depth: u32,
}

impl SubagentIdentity {
    /// Generate identity from the caller's tool context.
    pub fn generate(ctx: Option<&ToolCallContext>, agent_name: &str) -> Self {
        let seq = next_seq();
        let parent_thread_id = ctx.and_then(|c| c.thread_id.clone());
        let parent_depth = ctx.map(|c| c.depth).unwrap_or(0);
        let depth = parent_depth.saturating_add(1);
        let thread_prefix = parent_thread_id
            .clone()
            .unwrap_or_else(|| "root".to_string());
        Self {
            invocation_id: format!("sub-{thread_prefix}-{agent_name}-{parent_depth}-{seq}"),
            parent_session_id: ctx.and_then(|c| c.acp_session_id.clone()),
            parent_thread_id,
            parent_tool_call_id: ctx.and_then(|c| c.tool_call_id.clone()),
            child_session_id: uuid::Uuid::new_v4().to_string(),
            child_thread_id: format!("sub-{thread_prefix}-{agent_name}-{depth}-{seq}"),
            agent_name: agent_name.to_string(),
            depth,
        }
    }

    /// Synthetic identity for legacy `register()` callers that have no
    /// invocation context (tests, external registrations).
    pub(crate) fn synthetic(agent_id: &str, agent_name: &str) -> Self {
        Self {
            invocation_id: agent_id.to_string(),
            parent_session_id: None,
            parent_thread_id: None,
            parent_tool_call_id: None,
            child_session_id: uuid::Uuid::new_v4().to_string(),
            child_thread_id: agent_id.to_string(),
            agent_name: agent_name.to_string(),
            depth: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with(depth: u32, thread: &str, call: &str, session: &str) -> ToolCallContext {
        ToolCallContext {
            thread_id: Some(thread.to_string()),
            acp_session_id: Some(session.to_string()),
            tool_call_id: Some(call.to_string()),
            depth,
            ..Default::default()
        }
    }

    /// Contract §10.1: concurrent same-name/same-depth invocations must all
    /// produce unique IDs (invocation_id + child_thread_id + child_session_id).
    #[test]
    fn concurrent_generation_is_unique() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let ctx = ctx_with(0, "t-root", "call-1", "sess-1");
            let mut handles = Vec::new();
            for _ in 0..100 {
                let ctx = ctx.clone();
                handles
                    .push(tokio::spawn(async move { SubagentIdentity::generate(Some(&ctx), "dev") }));
            }
            let mut ids = Vec::new();
            for h in handles {
                ids.push(h.await.unwrap());
            }
            assert_eq!(ids.len(), 100);

            let count_unique = |mut v: Vec<String>| {
                v.sort();
                v.dedup();
                v.len()
            };
            assert_eq!(
                count_unique(ids.iter().map(|i| i.invocation_id.clone()).collect()),
                100,
                "invocation ids must be unique"
            );
            assert_eq!(
                count_unique(ids.iter().map(|i| i.child_thread_id.clone()).collect()),
                100,
                "child thread ids must be unique (GAP-02: seq must be included)"
            );
            assert_eq!(
                count_unique(ids.iter().map(|i| i.child_session_id.clone()).collect()),
                100,
                "child session ids must be unique"
            );
        });
    }

    #[test]
    fn identity_carries_parent_chain_and_depth() {
        let ctx = ctx_with(2, "t-parent", "call-9", "sess-7");
        let id = SubagentIdentity::generate(Some(&ctx), "explore");
        assert_eq!(id.parent_thread_id.as_deref(), Some("t-parent"));
        assert_eq!(id.parent_session_id.as_deref(), Some("sess-7"));
        assert_eq!(id.parent_tool_call_id.as_deref(), Some("call-9"));
        assert_eq!(id.depth, 3);
        assert_eq!(id.agent_name, "explore");
        assert!(id.child_thread_id.contains("explore"));
    }

    #[test]
    fn depth_is_saturated_not_wrapping() {
        let ctx = ctx_with(u32::MAX, "t", "c", "s");
        let id = SubagentIdentity::generate(Some(&ctx), "dev");
        assert_eq!(id.depth, u32::MAX);
    }

    #[test]
    fn no_ctx_falls_back_to_root() {
        let id = SubagentIdentity::generate(None, "dev");
        assert_eq!(id.parent_thread_id, None);
        assert_eq!(id.parent_session_id, None);
        assert_eq!(id.parent_tool_call_id, None);
        assert_eq!(id.depth, 1);
        assert!(id.invocation_id.starts_with("sub-root-dev-0-"));
    }
}

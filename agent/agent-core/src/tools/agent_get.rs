//! AgentGetTool: query the status and result of async agent invocations.
//!
//! Usage:
//!   {"agent_id": "sub-root-dev-0-3"}   → single agent status
//!   {}                                  → list all agents

use async_trait::async_trait;
use serde_json::{json, Value};
use tool_core::{Tool, ToolCallContent, ToolCallContext, ToolSourceError, ToolSpec};

use crate::tools::agent::registry::{AgentEntry, AsyncAgentRegistry};
use tool_core::tool_name::TOOL_AGENT_GET;

pub struct AgentGetTool {
    registry: AsyncAgentRegistry,
}

impl AgentGetTool {
    pub fn new(registry: AsyncAgentRegistry) -> Self {
        Self { registry }
    }
}

/// List-mode summary: everything except the (potentially huge) `result` text.
/// Keeps stats/durations so the model can compare agents cheaply (T1).
fn summary_json(entry: &AgentEntry) -> Value {
    let mut j = entry.to_json();
    if let Some(obj) = j.as_object_mut() {
        obj.remove("result");
    }
    j
}

#[async_trait]
impl Tool for AgentGetTool {
    fn name(&self) -> &str {
        TOOL_AGENT_GET
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_AGENT_GET.to_string(),
            description: Some(
                "Retrieve the status or result of an async agent. \
                 Pass `agent_id` to get the full entry (including the final result); omit it to \
                 list all agents (summaries only, no result bodies). Optional `status` filters the \
                 list. For thread-level aggregation, use `thread_get`. \
                 Shares state with the `agent` tool's registry."
                    .to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The agent_id returned by a background agent invocation. If omitted, lists all agents."
                    },
                    "status": {
                        "type": "string",
                        "description": "Only list agents with this status. Applies to list mode (no agent_id).",
                        "enum": ["running", "background", "completed", "failed"]
                    }
                }
            }),
            output_hint: None,
        }
    }

    async fn call(
        &self,
        args: Value,
        _ctx: Option<&ToolCallContext>,
    ) -> Result<ToolCallContent, ToolSourceError> {
        let agent_id = args.get("agent_id").and_then(|v| v.as_str());
        let status_filter = args.get("status").and_then(|v| v.as_str());

        if let Some(id) = agent_id {
            return match self.registry.get(id) {
                Some(entry) => {
                    // Single-agent mode: full entry, compact JSON (T1).
                    let response = entry.to_json();
                    Ok(ToolCallContent::text(serde_json::to_string(&response).map_err(
                        |e| ToolSourceError::Transport(e.to_string()),
                    )?))
                }
                None => Err(ToolSourceError::InvalidInput(format!(
                    "agent_id '{id}' not found (the entry may have been auto-evicted once the registry reached its terminal retention cap, or the id is invalid)"
                ))),
            };
        }

        let mut entries = self.registry.list_all();
        if let Some(status) = status_filter {
            entries.retain(|e| match status {
                "running" | "background" => e.status.is_active(),
                "completed" => matches!(e.status, crate::tools::agent::registry::AgentStatus::Completed { .. }),
                "failed" => matches!(e.status, crate::tools::agent::registry::AgentStatus::Failed { .. }),
                _ => true,
            });
        }
        let agents: Vec<Value> = entries.iter().map(summary_json).collect();
        // List mode: summaries only — never the result bodies (T1).
        let response = json!({ "agents": agents, "count": agents.len() });
        Ok(ToolCallContent::text(serde_json::to_string(&response).map_err(
            |e| ToolSourceError::Transport(e.to_string()),
        )?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_mode_omits_result_and_is_compact() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.complete("a1", "a very long final reply body".into(), {
            crate::tools::agent::registry::AgentCompletionStats {
                turn_count: 2,
                total_tokens: 42,
                tool_calls_count: 1,
            }
        });
        let tool = AgentGetTool::new(reg);
        let out = tool.call(json!({}), None).await.unwrap();
        let text = out.as_text().unwrap();
        assert!(!text.contains("a very long final reply body"), "list mode must not carry result bodies");
        assert!(!text.contains('\n'), "output must be compact JSON");
        assert!(text.contains("total_tokens"), "stats stay visible in summaries");
    }

    #[tokio::test]
    async fn single_mode_returns_full_result_compact() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.complete("a1", "full body".into(), crate::tools::agent::registry::AgentCompletionStats::default());
        let tool = AgentGetTool::new(reg);
        let out = tool.call(json!({ "agent_id": "a1" }), None).await.unwrap();
        let text = out.as_text().unwrap();
        assert!(text.contains("full body"));
        assert!(!text.contains('\n'), "output must be compact JSON");
    }

    #[tokio::test]
    async fn status_filter_applies_to_list_mode() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.register("a2".into(), "dev".into(), "t1".into(), None);
        reg.complete("a2", "done".into(), crate::tools::agent::registry::AgentCompletionStats::default());
        let tool = AgentGetTool::new(reg);
        let out = tool.call(json!({ "status": "running" }), None).await.unwrap();
        let text = out.as_text().unwrap();
        assert!(text.contains("a1"));
        assert!(!text.contains("a2"), "completed agent filtered out");
    }

    #[tokio::test]
    async fn not_found_message_mentions_eviction() {
        let tool = AgentGetTool::new(AsyncAgentRegistry::new());
        let err = tool.call(json!({ "agent_id": "gone" }), None).await.unwrap_err();
        assert!(err.to_string().contains("evicted"));
    }
}

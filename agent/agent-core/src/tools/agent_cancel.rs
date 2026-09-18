//! AgentCancelTool: abort a running background agent.

use async_trait::async_trait;
use serde_json::Value;
use tool_core::{
    tool_name::TOOL_AGENT_CANCEL, Tool, ToolCallContent, ToolCallContext, ToolSourceError, ToolSpec,
};

use crate::tools::agent::registry::AsyncAgentRegistry;

pub struct AgentCancelTool {
    registry: AsyncAgentRegistry,
}

impl AgentCancelTool {
    pub fn new(registry: AsyncAgentRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for AgentCancelTool {
    fn name(&self) -> &str {
        TOOL_AGENT_CANCEL
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_AGENT_CANCEL.to_string(),
            description: Some(
                "Abort a running background agent. \
                 The agent's tokio task is cancelled immediately. \
                 Cancelling an already completed/failed agent is a no-op success \
                 that returns its current state (idempotent)."
                    .to_string(),
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent_id": {
                        "type": "string",
                        "description": "The agent_id to cancel."
                    }
                },
                "required": ["agent_id"]
            }),
            output_hint: None,
        }
    }

    async fn call(
        &self,
        args: Value,
        _ctx: Option<&ToolCallContext>,
    ) -> Result<ToolCallContent, ToolSourceError> {
        let agent_id = args
            .get("agent_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ToolSourceError::InvalidInput("missing required argument: agent_id".into())
            })?;

        // T3: idempotent semantics (contract §7.2) — cancelling an
        // already-terminal agent is a success no-op returning current state,
        // not an error the model would retry.
        if let Some(entry) = self.registry.get(agent_id) {
            if !entry.status.is_active() {
                let response = serde_json::json!({
                    "agent_id": agent_id,
                    "status": "noop",
                    "message": "agent already reached a terminal state; nothing to cancel",
                    "current": entry.to_json(),
                });
                return Ok(ToolCallContent::text(
                    serde_json::to_string(&response)
                        .map_err(|e| ToolSourceError::Transport(e.to_string()))?,
                ));
            }
        }

        self.registry
            .cancel(agent_id)
            .map_err(ToolSourceError::InvalidInput)?;

        // Return the updated entry.
        let response = match self.registry.get(agent_id) {
            Some(entry) => entry.to_json(),
            None => serde_json::json!({ "agent_id": agent_id, "status": "cancelled" }),
        };
        Ok(ToolCallContent::text(
            serde_json::to_string(&response).map_err(|e| ToolSourceError::Transport(e.to_string()))?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::agent::registry::AsyncAgentRegistry;
    use serde_json::json;

    /// T3: cancelling a terminal agent is a success no-op (contract §7.2).
    #[tokio::test]
    async fn cancel_terminal_agent_is_idempotent_success() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        reg.complete(
            "a1",
            "done".into(),
            crate::tools::agent::registry::AgentCompletionStats::default(),
        );
        let tool = AgentCancelTool::new(reg);
        let out = tool.call(json!({ "agent_id": "a1" }), None).await.unwrap();
        let text = out.as_text().unwrap();
        assert!(text.contains("\"noop\""));
        assert!(text.contains("nothing to cancel"));
    }

    #[tokio::test]
    async fn cancel_running_agent_succeeds() {
        let reg = AsyncAgentRegistry::new();
        reg.register("a1".into(), "dev".into(), "t1".into(), None);
        let tool = AgentCancelTool::new(reg);
        let out = tool.call(json!({ "agent_id": "a1" }), None).await.unwrap();
        let text = out.as_text().unwrap();
        assert!(text.contains("\"failed\""));
        assert!(text.contains("cancelled"));
    }
}

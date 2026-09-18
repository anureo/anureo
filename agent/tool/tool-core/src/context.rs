use std::sync::Arc;

use crate::active_operation::RunCancellation;
use anureo_llm::Message;

#[derive(Clone, Default)]
pub struct ToolCallContext {
    pub recent_messages: Vec<Message>,
    pub any_stream_event_sender: Option<Arc<dyn Fn(serde_json::Value) + Send + Sync>>,
    pub thread_id: Option<String>,
    pub user_id: Option<String>,
    pub acp_session_id: Option<String>,
    pub depth: u32,
    pub run_cancellation: Option<RunCancellation>,
    /// The tool call that triggered this invocation (contract §3.1
    /// `parent_tool_call_id`). Filled by the act executor from the current
    /// `ToolCall` — upstream and reliable, unlike stream-side backfills.
    pub tool_call_id: Option<String>,
}

impl ToolCallContext {
    pub fn new(recent_messages: Vec<Message>) -> Self {
        Self {
            recent_messages,
            ..Default::default()
        }
    }
}

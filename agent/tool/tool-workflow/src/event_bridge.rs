use agent::agent::AgentEvent as AnureoAgentEvent;
use luft_core::contract::event::{AgentEvent as LuftAgentEvent, ProgressDelta};

pub fn map_anureo_event_to_delta(ev: &AnureoAgentEvent) -> Option<ProgressDelta> {
    match ev {
        AnureoAgentEvent::TextChunk(text) => Some(ProgressDelta::Message { text: text.clone() }),
        AnureoAgentEvent::ToolCallStart {
            name, arguments, ..
        } => {
            let summary = if arguments.len() > 200 {
                format!("{}({}...)", name, &arguments[..200])
            } else {
                format!("{}({})", name, arguments)
            };
            Some(ProgressDelta::ToolCall {
                name: name.clone(),
                summary,
            })
        }
        AnureoAgentEvent::ToolEnd { name, is_error, .. } => Some(ProgressDelta::ToolCall {
            name: name.clone(),
            summary: if *is_error {
                "error".to_string()
            } else {
                "done".to_string()
            },
        }),
        AnureoAgentEvent::ReasoningChunk(_) => None,
        AnureoAgentEvent::ToolOutput { .. } => None,
        AnureoAgentEvent::Usage { .. } => None,
    }
}

pub fn luft_event_to_json(ev: &LuftAgentEvent) -> serde_json::Value {
    serde_json::to_value(ev).unwrap_or(serde_json::json!({"type": "unknown"}))
}

//! ThreadGetTool: query thread information and agents.
//!
//! When an agent is invoked, it creates a thread ID hierarchy. This tool allows
//! querying thread information and the agents running/completed within it.

use std::collections::HashMap;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use tool_core::{
    tool_name::TOOL_THREAD_GET, Tool, ToolCallContent, ToolCallContext, ToolSourceError, ToolSpec,
};

use crate::tools::agent::registry::{AgentStatus, AsyncAgentRegistry};

pub struct ThreadGetTool {
    registry: AsyncAgentRegistry,
}

impl ThreadGetTool {
    pub fn new(registry: AsyncAgentRegistry) -> Self {
        Self { registry }
    }
}

#[async_trait]
impl Tool for ThreadGetTool {
    fn name(&self) -> &str {
        TOOL_THREAD_GET
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: TOOL_THREAD_GET.to_string(),
            description: Some(
                "Query thread information and agents. \
                 If `thread_id` is omitted, returns all threads with summary counts. \
                 If provided, returns the thread summary and all agents in that thread. \
                 For agent-level listing with full entries, use `agent_get`."
                    .to_string(),
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "thread_id": {
                        "type": "string",
                        "description": "Thread ID to query. If omitted, lists all threads."
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
        let thread_id = args.get("thread_id").and_then(|v| v.as_str());

        match thread_id {
            Some(tid) => self.get_single_thread(tid),
            None => self.list_all_threads(),
        }
    }
}

impl ThreadGetTool {
    fn list_all_threads(&self) -> Result<ToolCallContent, ToolSourceError> {
        let all_entries = self.registry.list_all();

        let mut threads: HashMap<&str, ThreadSummary> = HashMap::new();
        for entry in &all_entries {
            ThreadSummary::accumulate(threads.entry(&entry.thread_id).or_default(), &entry.status);
        }

        let mut thread_list: Vec<Value> = threads
            .into_iter()
            .map(|(tid, summary)| json!({ "thread_id": tid, "summary": summary }))
            .collect();
        thread_list.sort_by(|a, b| {
            a["thread_id"]
                .as_str()
                .unwrap_or("")
                .cmp(b["thread_id"].as_str().unwrap_or(""))
        });

        Ok(ToolCallContent::text(
            serde_json::to_string_pretty(&json!({ "threads": thread_list })).unwrap(),
        ))
    }

    fn get_single_thread(&self, thread_id: &str) -> Result<ToolCallContent, ToolSourceError> {
        let all_entries = self.registry.list_all();
        let thread_entries: Vec<_> = all_entries
            .into_iter()
            .filter(|e| e.thread_id == thread_id)
            .collect();

        if thread_entries.is_empty() {
            return Err(ToolSourceError::InvalidInput(format!(
                "thread_id '{thread_id}' not found"
            )));
        }

        let mut summary = ThreadSummary::default();
        for entry in &thread_entries {
            ThreadSummary::accumulate(&mut summary, &entry.status);
        }

        let agents: Vec<Value> = thread_entries.iter().map(|e| e.to_json()).collect();
        let response = json!({
            "thread_id": thread_id,
            "summary": summary,
            "agents": agents
        });

        Ok(ToolCallContent::text(
            serde_json::to_string_pretty(&response).unwrap(),
        ))
    }
}

/// Summary counts for a thread.
#[derive(Debug, Default, Serialize)]
struct ThreadSummary {
    agent_count: u32,
    running_count: u32,
    background_count: u32,
    completed_count: u32,
    failed_count: u32,
}

impl ThreadSummary {
    /// Accumulate one entry's status into this summary.
    fn accumulate(&mut self, status: &AgentStatus) {
        self.agent_count += 1;
        match status {
            AgentStatus::Running { .. } => self.running_count += 1,
            AgentStatus::Background { .. } => self.background_count += 1,
            AgentStatus::Completed { .. } => self.completed_count += 1,
            AgentStatus::Failed { .. } => self.failed_count += 1,
        }
    }
}

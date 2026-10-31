use anureo_llm::message::ToolCallContent;
use anureo_llm::tool::{ToolSourceError, ToolSpec};
use async_trait::async_trait;
use memory_v2::{
    AddResult, MemoryError, MemoryFile, MemoryProvenance, MemoryStore, RemoveResult, ReplaceResult,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tool_core::{Tool, ToolCallContext};

pub const TOOL_MEMORY: &str = "memory";

const MEMORY_DESCRIPTION: &str = "\
Save durable information to persistent memory that survives across sessions. \
Memory is injected into future turns, so keep it compact and focused on facts \
that will still matter later.\n\
\n\
WHEN TO SAVE (do this proactively, don't wait to be asked):\n\
- User corrects you or says 'remember this' / 'don't do that again'\n\
- User shares a preference, habit, or personal detail (name, role, timezone, coding style)\n\
- You discover something about the environment (OS, installed tools, project structure)\n\
- You learn a convention, API quirk, or workflow specific to this user's setup\n\
- You identify a stable fact that will be useful again in future sessions\n\
\n\
PRIORITY: User preferences and corrections > environment facts > procedural knowledge. \
The most valuable memory prevents the user from having to repeat themselves.\n\
\n\
Do NOT save task progress, session outcomes, completed-work logs, or temporary TODO \
state to memory.\n\
If you've discovered a new way to do something, solved a problem that could be \
necessary later, save it as a skill with the skill tool.\n\
\n\
TWO TARGETS:\n\
- 'user': who the user is -- name, role, preferences, communication style, pet peeves\n\
- 'memory': your notes -- environment facts, project conventions, tool quirks, lessons learned\n\
\n\
ACTIONS: add (new entry), replace (update existing -- old_text identifies it), \
remove (delete -- old_text identifies it).\n\
\n\
SKIP: trivial/obvious info, things easily re-discovered, raw data dumps, and temporary task state.";

pub struct MemoryTool {
    store: Arc<MemoryStore>,
    default_provenance: MemoryProvenance,
    user_profile_enabled: bool,
}

impl MemoryTool {
    pub fn new(store: Arc<MemoryStore>) -> Self {
        Self::for_foreground(store, true)
    }

    pub fn for_foreground(store: Arc<MemoryStore>, user_profile_enabled: bool) -> Self {
        Self {
            store,
            default_provenance: MemoryProvenance::foreground_default(),
            user_profile_enabled,
        }
    }

    pub fn for_background_review(
        store: Arc<MemoryStore>,
        session_id: impl Into<String>,
        parent_id: impl Into<String>,
    ) -> Self {
        Self::for_background_review_with_user_profile(store, session_id, parent_id, false)
    }

    pub fn for_background_review_with_user_profile(
        store: Arc<MemoryStore>,
        session_id: impl Into<String>,
        parent_id: impl Into<String>,
        user_profile_enabled: bool,
    ) -> Self {
        Self {
            store,
            default_provenance: MemoryProvenance::background_review(session_id, parent_id),
            user_profile_enabled,
        }
    }

    pub fn store(&self) -> &Arc<MemoryStore> {
        &self.store
    }

    pub fn default_provenance(&self) -> &MemoryProvenance {
        &self.default_provenance
    }

    fn parse_target(target: &str) -> Result<MemoryFile, Value> {
        match target.to_lowercase().as_str() {
            "user" => Ok(MemoryFile::User),
            "memory" => Ok(MemoryFile::Project),
            _ => Err(json!({
                "success": false,
                "error": format!("Unknown target '{}'. Use 'memory' or 'user'.", target)
            })),
        }
    }

    fn handle_add(&self, file: MemoryFile, content: &str) -> Value {
        match self
            .store
            .add_entry(file, content, &self.default_provenance)
        {
            Ok(AddResult {
                success,
                message,
                entry_count,
                usage,
                provenance,
            }) => {
                let mut response = json!({
                    "success": success,
                    "message": message,
                    "entries": entry_count,
                    "usage": usage,
                    "provenance": provenance,
                });
                if success {
                    response["op"] = json!("add");
                    response["file"] = json!(Self::file_wire_name(file));
                    response["chars"] = json!(content.trim().chars().count());
                }
                response
            }
            Err(e) => Self::fmt_error(&e),
        }
    }

    fn handle_replace(&self, file: MemoryFile, old_text: &str, new_content: &str) -> Value {
        match self.store.replace_entry(file, old_text, new_content) {
            Ok(ReplaceResult {
                success,
                message,
                matched_count,
                entry_count,
                usage,
            }) => {
                let mut response = json!({
                    "success": success,
                    "message": message,
                    "matched": matched_count,
                    "entries": entry_count,
                    "usage": usage
                });
                if success {
                    response["op"] = json!("replace");
                    response["file"] = json!(Self::file_wire_name(file));
                    response["chars"] = json!(new_content.trim().chars().count());
                }
                response
            }
            Err(e) => Self::fmt_error(&e),
        }
    }

    fn handle_remove(&self, file: MemoryFile, old_text: &str) -> Value {
        match self.store.remove_entry(file, old_text) {
            Ok(RemoveResult {
                success,
                message,
                entry_count,
                usage,
            }) => {
                let mut response = json!({
                    "success": success,
                    "message": message,
                    "entries": entry_count,
                    "usage": usage
                });
                if success {
                    response["op"] = json!("remove");
                    response["file"] = json!(Self::file_wire_name(file));
                }
                response
            }
            Err(e) => Self::fmt_error(&e),
        }
    }

    // The protocol calls project memory `memory`, not serde's `project`.
    fn file_wire_name(file: MemoryFile) -> &'static str {
        match file {
            MemoryFile::User => "user",
            MemoryFile::Project => "memory",
        }
    }

    fn fmt_error(e: &MemoryError) -> Value {
        json!({
            "success": false,
            "error": e.to_string()
        })
    }

    pub fn tool_spec() -> ToolSpec {
        ToolSpec {
            name: TOOL_MEMORY.to_string(),
            description: Some(MEMORY_DESCRIPTION.to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "replace", "remove"],
                        "description": "The action to perform."
                    },
                    "target": {
                        "type": "string",
                        "enum": ["memory", "user"],
                        "description": "Which memory store: 'memory' for personal notes, 'user' for user profile."
                    },
                    "content": {
                        "type": "string",
                        "description": "The entry content. Required for 'add' and 'replace'."
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Short unique substring identifying the entry to replace or remove."
                    }
                },
                "required": ["action", "target"]
            }),
            output_hint: None,
        }
    }

    pub fn dispatch(&self, args: &Value) -> Value {
        let action = args["action"].as_str().unwrap_or("");
        let target = args["target"].as_str().unwrap_or("memory");

        let file = match Self::parse_target(target) {
            Ok(f) => f,
            Err(err_resp) => return err_resp,
        };

        if file == MemoryFile::User && !self.user_profile_enabled {
            return json!({
                "success": false,
                "error": "User profile writes are disabled in this context."
            });
        }

        match action {
            "add" => {
                let content = args["content"].as_str().unwrap_or("");
                self.handle_add(file, content)
            }
            "replace" => {
                let old_text = args["old_text"].as_str().unwrap_or("");
                let new_content = args["content"].as_str().unwrap_or("");
                self.handle_replace(file, old_text, new_content)
            }
            "remove" => {
                let old_text = args["old_text"].as_str().unwrap_or("");
                self.handle_remove(file, old_text)
            }
            _ => json!({
                "success": false,
                "error": format!("Unknown action '{}'. Use add/replace/remove.", action)
            }),
        }
    }
}

#[async_trait]
impl Tool for MemoryTool {
    fn name(&self) -> &str {
        TOOL_MEMORY
    }

    fn spec(&self) -> ToolSpec {
        Self::tool_spec()
    }

    async fn call(
        &self,
        args: Value,
        _ctx: Option<&ToolCallContext>,
    ) -> Result<ToolCallContent, ToolSourceError> {
        let result = self.dispatch(&args);
        Ok(ToolCallContent::text(
            serde_json::to_string_pretty(&result).unwrap_or_else(|_| result.to_string()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memory_v2::MemoryStore;

    fn test_store() -> Arc<MemoryStore> {
        Arc::new(MemoryStore::new(&tempfile::tempdir().unwrap().keep()))
    }

    #[test]
    fn dispatch_add() {
        let store = test_store();
        let tool = MemoryTool::new(store);
        let result = tool.dispatch(&json!({
            "action": "add",
            "target": "user",
            "content": "prefers Rust"
        }));
        assert!(result["success"].as_bool().unwrap());
    }

    #[test]
    fn dispatch_replace() {
        let store = test_store();
        let tool = MemoryTool::new(store);
        tool.dispatch(&json!({"action": "add", "target": "user", "content": "likes tea"}));
        let result = tool.dispatch(&json!({
            "action": "replace",
            "target": "user",
            "old_text": "tea",
            "content": "likes coffee"
        }));
        assert!(result["success"].as_bool().unwrap());
    }

    #[test]
    fn dispatch_remove() {
        let store = test_store();
        let tool = MemoryTool::new(store);
        tool.dispatch(&json!({"action": "add", "target": "user", "content": "temp entry"}));
        let result = tool.dispatch(&json!({
            "action": "remove",
            "target": "user",
            "old_text": "temp"
        }));
        assert!(result["success"].as_bool().unwrap());
    }

    #[test]
    fn successful_responses_include_structured_wire_fields() {
        let store = test_store();
        let tool = MemoryTool::new(store);

        let add = tool.dispatch(&json!({
            "action": "add",
            "target": "memory",
            "content": "  偏爱 Rust  "
        }));
        assert_eq!(add["op"], "add");
        assert_eq!(add["file"], "memory");
        assert_eq!(add["chars"], 7);
        assert!(add["entries"].is_number());
        assert!(add["usage"].is_string());

        let replace = tool.dispatch(&json!({
            "action": "replace",
            "target": "memory",
            "old_text": "Rust",
            "content": "  偏爱 Go  "
        }));
        assert_eq!(replace["op"], "replace");
        assert_eq!(replace["file"], "memory");
        assert_eq!(replace["chars"], 5);
        assert!(replace["matched"].is_number());
        assert!(replace["entries"].is_number());
        assert!(replace["usage"].is_string());

        let remove = tool.dispatch(&json!({
            "action": "remove",
            "target": "memory",
            "old_text": "Go"
        }));
        assert_eq!(remove["op"], "remove");
        assert_eq!(remove["file"], "memory");
        assert!(remove.get("chars").is_none());
        assert!(remove["entries"].is_number());
        assert!(remove["usage"].is_string());

        let user = tool.dispatch(&json!({
            "action": "add",
            "target": "user",
            "content": "concise replies"
        }));
        assert_eq!(user["file"], "user");
    }

    #[test]
    fn failed_responses_do_not_include_structured_wire_fields() {
        let store = test_store();
        let tool = MemoryTool::new(store);
        let result = tool.dispatch(&json!({
            "action": "replace",
            "target": "memory",
            "old_text": "missing",
            "content": "replacement"
        }));

        assert_eq!(result["success"], false);
        assert!(result.get("op").is_none());
        assert!(result.get("file").is_none());
        assert!(result.get("chars").is_none());
        assert!(result["error"].is_string());
    }

    #[test]
    fn dispatch_unknown_action() {
        let store = test_store();
        let tool = MemoryTool::new(store);
        let result = tool.dispatch(&json!({"action": "read", "target": "user"}));
        assert!(!result["success"].as_bool().unwrap());
        assert!(result["error"].as_str().unwrap().contains("Unknown action"));
    }

    #[test]
    fn dispatch_unknown_target() {
        let store = test_store();
        let tool = MemoryTool::new(store);
        let result = tool.dispatch(&json!({"action": "add", "target": "invalid", "content": "x"}));
        assert!(!result["success"].as_bool().unwrap());
    }

    #[test]
    fn spec_matches_reference() {
        let spec = MemoryTool::tool_spec();
        assert_eq!(spec.name, "memory");
        let schema = &spec.input_schema;
        let action_enum = schema["properties"]["action"]["enum"].as_array().unwrap();
        assert_eq!(action_enum.len(), 3);
        assert!(action_enum.iter().any(|v| v == "add"));
        assert!(action_enum.iter().any(|v| v == "replace"));
        assert!(action_enum.iter().any(|v| v == "remove"));
        let target_enum = schema["properties"]["target"]["enum"].as_array().unwrap();
        assert!(target_enum.iter().any(|v| v == "memory"));
        assert!(target_enum.iter().any(|v| v == "user"));
    }

    #[tokio::test]
    async fn tool_call_serializes_structured_wire_fields() {
        let store = test_store();
        let tool = MemoryTool::new(store);
        let result = tool
            .call(
                json!({"action": "add", "target": "memory", "content": "test"}),
                None,
            )
            .await
            .unwrap();
        let text = match result {
            ToolCallContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["success"], true);
        assert_eq!(parsed["op"], "add");
        assert_eq!(parsed["file"], "memory");
        assert_eq!(parsed["chars"], 4);
        assert!(parsed["message"].is_string());
        assert!(parsed["entries"].is_number());
        assert!(parsed["usage"].is_string());
        assert!(parsed["provenance"].is_object());
    }

    #[test]
    fn target_memory_maps_to_project() {
        assert_eq!(
            MemoryTool::parse_target("memory").unwrap(),
            MemoryFile::Project
        );
        assert_eq!(MemoryTool::parse_target("user").unwrap(), MemoryFile::User);
    }

    #[test]
    fn foreground_default_provenance() {
        let store = test_store();
        let tool = MemoryTool::for_foreground(store, true);
        assert_eq!(tool.default_provenance().write_origin, "assistant_tool");
        assert_eq!(tool.default_provenance().execution_context, "foreground");
        assert!(tool.default_provenance().session_id.is_none());
    }

    #[test]
    fn background_review_memory_writes_with_provenance() {
        let store = test_store();
        let tool = MemoryTool::for_background_review_with_user_profile(
            store,
            "session-1",
            "parent-1",
            true,
        );
        assert_eq!(
            tool.default_provenance().execution_context,
            "background_review"
        );
        assert_eq!(tool.default_provenance().write_origin, "background_review");
        assert_eq!(
            tool.default_provenance().session_id.as_deref(),
            Some("session-1")
        );
        assert_eq!(
            tool.default_provenance().parent_session_id.as_deref(),
            Some("parent-1")
        );
    }

    #[test]
    fn background_review_defaults_to_user_profile_disabled() {
        let store = test_store();
        let tool = MemoryTool::for_background_review(store, "s", "p");
        assert!(!tool.user_profile_enabled);
    }

    #[test]
    fn background_review_empty_session_id_panics() {
        let store = test_store();
        let result = std::panic::catch_unwind(|| {
            MemoryTool::for_background_review(store, "", "p");
        });
        assert!(result.is_err());
    }

    #[test]
    fn background_review_empty_parent_id_panics() {
        let store = test_store();
        let result = std::panic::catch_unwind(|| {
            MemoryTool::for_background_review(store, "s", "");
        });
        assert!(result.is_err());
    }

    #[test]
    fn background_review_blocks_user_writes_by_default() {
        let store = test_store();
        let tool = MemoryTool::for_background_review(store.clone(), "s", "p");
        let result = tool.dispatch(&json!({
            "action": "add",
            "target": "user",
            "content": "should be blocked by default"
        }));
        assert!(!result["success"].as_bool().unwrap());
        assert!(result["error"].as_str().unwrap().contains("disabled"));
    }

    #[test]
    fn background_review_allows_user_writes_when_opted_in() {
        let store = test_store();
        let tool =
            MemoryTool::for_background_review_with_user_profile(store.clone(), "s", "p", true);
        let result = tool.dispatch(&json!({
            "action": "add",
            "target": "user",
            "content": "allowed via opt-in"
        }));
        assert!(result["success"].as_bool().unwrap());
        assert_eq!(
            result["provenance"]["execution_context"].as_str().unwrap(),
            "background_review"
        );
    }

    #[test]
    fn review_and_main_share_storage() {
        let store = test_store();
        let main = MemoryTool::for_foreground(store.clone(), true);
        let review = MemoryTool::for_background_review(store.clone(), "rv-1", "p-1");
        let result = main.dispatch(&json!({
            "action": "add",
            "target": "memory",
            "content": "shared fact"
        }));
        assert!(result["success"].as_bool().unwrap());
        let entries = review.store().read_entries(MemoryFile::Project).unwrap();
        assert!(entries.iter().any(|e| e.contains("shared fact")));
    }

    #[test]
    fn add_response_includes_provenance() {
        let store = test_store();
        let tool = MemoryTool::for_background_review(store.clone(), "s", "p");
        let result = tool.dispatch(&json!({
            "action": "add",
            "target": "memory",
            "content": "tagged entry"
        }));
        assert!(result["success"].as_bool().unwrap());
        assert_eq!(
            result["provenance"]["write_origin"].as_str().unwrap(),
            "background_review"
        );
        assert_eq!(
            result["provenance"]["execution_context"].as_str().unwrap(),
            "background_review"
        );
        assert_eq!(result["provenance"]["session_id"].as_str().unwrap(), "s");
    }

    #[test]
    fn user_profile_disabled_blocks_user_md_writes() {
        let store = test_store();
        let tool = MemoryTool::for_foreground(store.clone(), false);
        let result = tool.dispatch(&json!({
            "action": "add",
            "target": "user",
            "content": "should be blocked"
        }));
        assert!(!result["success"].as_bool().unwrap());
        assert!(result["error"].as_str().unwrap().contains("disabled"));
        let entries = store.read_entries(MemoryFile::User).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn user_profile_enabled_allows_user_md_writes() {
        let store = test_store();
        let tool = MemoryTool::for_foreground(store.clone(), true);
        let result = tool.dispatch(&json!({
            "action": "add",
            "target": "user",
            "content": "should be allowed"
        }));
        assert!(result["success"].as_bool().unwrap());
        let entries = store.read_entries(MemoryFile::User).unwrap();
        assert!(entries.iter().any(|e| e.contains("should be allowed")));
    }
}

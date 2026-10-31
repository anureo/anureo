//! Rebuild bounded review diffs from tool arguments in the review checkpoint.
//! By default history stores only a tool-call id; plaintext is opt-in.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use agent::state::ReActState;
use anureo_curator::{bounded_review_unified_diff, ReviewActionSummary};
use anureo_llm::message::Message;
use serde::Serialize;
use serde_json::Value;

#[cfg(test)]
const MAX_DIFF_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct UnifiedDiff {
    pub format: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewActionDetails {
    pub kind: String,
    pub op: String,
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    pub succeeded: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chars_before: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chars_after: Option<u64>,
    pub diff_available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<UnifiedDiff>,
    pub diff_truncated: bool,
}

pub fn project_actions(
    db_path: &Path,
    review_id: &str,
    actions: &[ReviewActionSummary],
    include_diff: bool,
) -> Result<Vec<ReviewActionDetails>, String> {
    let ids = actions
        .iter()
        .filter(|action| action.persisted_diff.is_none())
        .filter_map(|action| action.diff_ref.clone())
        .collect::<HashSet<_>>();
    let inputs = load_tool_inputs(db_path, review_id, &ids)?;

    Ok(actions
        .iter()
        .map(|action| {
            let source = action
                .diff_ref
                .as_ref()
                .and_then(|id| inputs.get(id))
                .and_then(|input| source_pair(action, input));
            let (diff, diff_truncated) = if include_diff {
                if let Some(text) = action.persisted_diff.clone() {
                    (
                        Some(UnifiedDiff {
                            format: "unified",
                            text,
                        }),
                        action.diff_truncated,
                    )
                } else {
                    source
                        .as_ref()
                        .map(|(before, after)| {
                            let (text, truncated) = bounded_review_unified_diff(before, after);
                            (
                                Some(UnifiedDiff {
                                    format: "unified",
                                    text,
                                }),
                                truncated,
                            )
                        })
                        .unwrap_or((None, false))
                }
            } else {
                (None, false)
            };
            ReviewActionDetails {
                kind: action.kind.clone(),
                op: action.op.clone(),
                target: action.target.clone(),
                file_path: action.file_path.clone(),
                succeeded: action.succeeded,
                summary: action.summary.clone(),
                chars_before: action.chars_before,
                chars_after: action.chars_after,
                diff_available: action.persisted_diff.is_some() || source.is_some(),
                diff,
                diff_truncated,
            }
        })
        .collect())
}

fn load_tool_inputs(
    db_path: &Path,
    review_id: &str,
    ids: &HashSet<String>,
) -> Result<HashMap<String, Value>, String> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    if !db_path.exists() {
        return Ok(HashMap::new());
    }
    let connection = checkpoint_sqlite_store::sqlite_util::open_sqlite_with_wal(db_path)?;
    let has_checkpoints: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'checkpoints')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| format!("inspect review checkpoints: {error}"))?;
    if !has_checkpoints {
        return Ok(HashMap::new());
    }
    let mut statement = connection
        .prepare(
            "SELECT payload FROM checkpoints
             WHERE thread_id = ?1 AND checkpoint_ns = 'background-review'
             ORDER BY metadata_created_at DESC, checkpoint_id DESC",
        )
        .map_err(|error| format!("prepare review checkpoint lookup: {error}"))?;
    let payloads = statement
        .query_map([review_id], |row| row.get::<_, Vec<u8>>(0))
        .map_err(|error| format!("query review checkpoints: {error}"))?;
    let mut found = HashMap::new();
    for payload in payloads {
        let payload = payload.map_err(|error| format!("read review checkpoint: {error}"))?;
        let Ok(state) = serde_json::from_slice::<ReActState>(&payload) else {
            continue;
        };
        for message in state.messages {
            let Message::Assistant(assistant) = message else {
                continue;
            };
            for call in assistant.tool_calls {
                if ids.contains(&call.id) && !found.contains_key(&call.id) {
                    if let Ok(input) = serde_json::from_str::<Value>(&call.arguments) {
                        found.insert(call.id, input);
                    }
                }
            }
        }
        if found.len() == ids.len() {
            break;
        }
    }
    Ok(found)
}

fn source_pair(action: &ReviewActionSummary, input: &Value) -> Option<(String, String)> {
    if !action.succeeded {
        return None;
    }
    let read = |key| input.get(key).and_then(Value::as_str).map(str::to_string);
    match (action.kind.as_str(), action.op.as_str()) {
        ("memory", "add") => Some((String::new(), read("content")?)),
        ("memory", "replace") => Some((read("old_text")?, read("content")?)),
        ("memory", "remove") => Some((read("old_text")?, String::new())),
        ("skill", "update") if input.get("old_string").is_some() => {
            Some((read("old_string")?, read("new_string")?))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anureo_llm::message::AssistantToolCall;

    fn action() -> ReviewActionSummary {
        ReviewActionSummary {
            kind: "memory".into(),
            op: "replace".into(),
            target: "user".into(),
            file_path: Some("USER.md".into()),
            summary: "replaced".into(),
            succeeded: true,
            chars_before: Some(2),
            chars_after: Some(2),
            diff_ref: Some("call-1".into()),
            persisted_diff: None,
            diff_truncated: false,
        }
    }

    #[test]
    fn diff_is_utf8_safe_and_bounded() {
        let (text, truncated) = bounded_review_unified_diff("旧".repeat(5000).as_str(), "新");
        assert!(truncated);
        assert!(text.len() <= MAX_DIFF_BYTES);
        assert!(text.is_char_boundary(text.len()));
        assert!(text.starts_with("--- before\n+++ after\n@@"));
    }

    #[test]
    fn checkpoint_reference_rebuilds_diff_without_exposing_reference() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let connection = rusqlite::Connection::open(&db).unwrap();
        connection.execute_batch("CREATE TABLE checkpoints (thread_id TEXT, checkpoint_ns TEXT, checkpoint_id TEXT, metadata_created_at INTEGER, payload BLOB);").unwrap();
        let mut state = ReActState::default();
        state.messages.push(Message::assistant_with_tool_calls(
            String::new(),
            vec![AssistantToolCall {
                id: "call-1".into(),
                name: "memory".into(),
                arguments:
                    serde_json::json!({"action":"replace","old_text":"旧值","content":"新值"})
                        .to_string(),
            }],
        ));
        connection.execute(
            "INSERT INTO checkpoints (thread_id, checkpoint_ns, checkpoint_id, metadata_created_at, payload) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params!["review-1", "background-review", "cp-1", 1_i64, serde_json::to_vec(&state).unwrap()],
        ).unwrap();
        let details = project_actions(&db, "review-1", &[action()], true).unwrap();
        let value = serde_json::to_value(&details[0]).unwrap();
        assert_eq!(value["diff_available"], true);
        assert!(value["diff"]["text"].as_str().unwrap().contains("-旧值"));
        assert!(value["diff"]["text"].as_str().unwrap().contains("+新值"));
        assert!(value.get("diff_ref").is_none());
        let without = project_actions(&db, "review-1", &[action()], false).unwrap();
        let value = serde_json::to_value(&without[0]).unwrap();
        assert!(value.get("diff").is_none());
        assert!(value.get("diff_ref").is_none());
    }

    #[test]
    fn opted_in_plaintext_diff_survives_without_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let mut stored = action();
        stored.diff_ref = None;
        stored.persisted_diff = Some("--- before\n+++ after\n@@ -1,1 +1,1 @@\n-旧\n+新\n".into());
        let details =
            project_actions(&dir.path().join("memory.db"), "review-1", &[stored], true).unwrap();
        assert!(details[0].diff_available);
        assert!(details[0].diff.as_ref().unwrap().text.contains("+新"));
    }

    #[test]
    fn missing_checkpoint_store_degrades_to_unavailable_diff() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("memory.db");
        let missing_file = project_actions(&db, "review-1", &[action()], true).unwrap();
        assert!(!missing_file[0].diff_available);
        assert!(missing_file[0].diff.is_none());
        let without_diff = project_actions(&db, "review-1", &[action()], false).unwrap();
        assert!(!without_diff[0].diff_available);

        rusqlite::Connection::open(&db).unwrap();
        let missing_table = project_actions(&db, "review-1", &[action()], true).unwrap();
        assert!(!missing_table[0].diff_available);
        assert!(missing_table[0].diff.is_none());
    }
}

use agent_client_protocol::schema::v1::{PlanEntry, PlanEntryPriority, PlanEntryStatus};

use agent::run::TypedAnyStreamEvent;
use agent::state::ReActState;
use anureo_acp::stream_bridge::{anureo_event_to_updates, StreamUpdate};
use stream_event::StreamEvent;

fn make_tool_end(name: &str, result: &str) -> StreamEvent<ReActState> {
    StreamEvent::ToolEnd {
        call_id: Some("call_test".to_string()),
        name: name.to_string(),
        result: result.to_string(),
        is_error: false,
        raw_result: None,
    }
}

fn find_plan_updates(updates: &[StreamUpdate]) -> Vec<&Vec<PlanEntry>> {
    updates
        .iter()
        .filter_map(|u| match u {
            StreamUpdate::Plan { entries } => Some(entries),
            _ => None,
        })
        .collect()
}

fn todo_result_json() -> &'static str {
    r#"3 todos
[
  { "id": "1", "content": "Analyze the codebase", "status": "pending", "priority": "high" },
  { "id": "2", "content": "Implement changes", "status": "pending", "priority": "high" },
  { "id": "3", "content": "Add tests", "status": "pending", "priority": "medium" }
]"#
}

fn todo_result_quoted_json() -> String {
    let inner = todo_result_json();
    serde_json::to_string(inner).unwrap()
}

#[test]
fn plan_emitted_on_todo_write_tool_end() {
    let ev = make_tool_end("todo_write", todo_result_json());
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].len(), 3);
}

#[test]
fn plan_all_entries_pending_on_create() {
    let ev = make_tool_end("todo_write", todo_result_json());
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    let entries = plans[0];
    assert!(entries.iter().all(|e| e.status == PlanEntryStatus::Pending));
}

#[test]
fn plan_entries_have_correct_content() {
    let ev = make_tool_end("todo_write", todo_result_json());
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    let entries = plans[0];
    assert_eq!(entries[0].content, "Analyze the codebase");
    assert_eq!(entries[1].content, "Implement changes");
    assert_eq!(entries[2].content, "Add tests");
}

#[test]
fn plan_entries_have_correct_priority() {
    let ev = make_tool_end("todo_write", todo_result_json());
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    let entries = plans[0];
    assert_eq!(entries[0].priority, PlanEntryPriority::High);
    assert_eq!(entries[1].priority, PlanEntryPriority::High);
    assert_eq!(entries[2].priority, PlanEntryPriority::Medium);
}

#[test]
fn plan_status_updates_on_todo_change() {
    let updated = r#"3 todos
[
  { "id": "1", "content": "Analyze the codebase", "status": "completed", "priority": "high" },
  { "id": "2", "content": "Implement changes", "status": "in_progress", "priority": "high" },
  { "id": "3", "content": "Add tests", "status": "pending", "priority": "medium" }
]"#;
    let ev = make_tool_end("todo_write", updated);
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    let entries = plans[0];
    assert_eq!(entries[0].status, PlanEntryStatus::Completed);
    assert_eq!(entries[1].status, PlanEntryStatus::InProgress);
    assert_eq!(entries[2].status, PlanEntryStatus::Pending);
}

#[test]
fn plan_completed_and_cancelled_map_to_completed() {
    let result = r#"2 todos
[
  { "id": "1", "content": "Task A", "status": "completed", "priority": "high" },
  { "id": "2", "content": "Task B", "status": "cancelled", "priority": "low" }
]"#;
    let ev = make_tool_end("todo_write", result);
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    let entries = plans[0];
    assert_eq!(entries[0].status, PlanEntryStatus::Completed);
    assert_eq!(entries[1].status, PlanEntryStatus::Completed);
}

#[test]
fn plan_full_replacement() {
    let updated = r#"4 todos
[
  { "id": "1", "content": "A", "status": "completed", "priority": "high" },
  { "id": "2", "content": "B", "status": "in_progress", "priority": "high" },
  { "id": "3", "content": "C", "status": "pending", "priority": "medium" },
  { "id": "4", "content": "D", "status": "pending", "priority": "low" }
]"#;
    let ev = make_tool_end("todo_write", updated);
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    assert_eq!(plans[0].len(), 4);
}

#[test]
fn plan_not_emitted_for_other_tools() {
    let ev = make_tool_end("read_file", "file contents here");
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    assert!(plans.is_empty());
}

#[test]
fn plan_emitted_alongside_tool_update() {
    let ev = make_tool_end("todo_write", todo_result_json());
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let has_tool_update = updates
        .iter()
        .any(|u| matches!(u, StreamUpdate::ToolCallUpdated { .. }));
    let has_plan = updates
        .iter()
        .any(|u| matches!(u, StreamUpdate::Plan { .. }));
    assert!(has_tool_update, "should still emit ToolCallUpdated");
    assert!(has_plan, "should also emit Plan");
}

#[test]
fn plan_not_emitted_on_error() {
    let ev = StreamEvent::ToolEnd {
        call_id: Some("call_test".to_string()),
        name: "todo_write".to_string(),
        result: "error".to_string(),
        is_error: true,
        raw_result: None,
    };
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    assert!(plans.is_empty());
}

#[test]
fn plan_not_emitted_on_malformed_result() {
    let ev = make_tool_end("todo_write", "not valid json");
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    assert!(plans.is_empty());
}

#[test]
fn plan_not_emitted_on_empty_todos() {
    let ev = make_tool_end("todo_write", "0 todos\n[]");
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    assert!(plans.is_empty());
}

#[test]
fn plan_works_with_quoted_json_result() {
    let quoted = todo_result_quoted_json();
    let ev = make_tool_end("todo_write", &quoted);
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    assert_eq!(plans.len(), 1);
    assert_eq!(plans[0].len(), 3);
    assert_eq!(plans[0][0].content, "Analyze the codebase");
}

fn make_sub_agent_tool_end(result: &str) -> StreamEvent<ReActState> {
    StreamEvent::ToolEnd {
        call_id: Some("sub-call-001".to_string()),
        name: "todo_write".to_string(),
        result: result.to_string(),
        is_error: false,
        raw_result: None,
    }
}

#[test]
fn sub_agent_any_stream_event_produces_plan_update() {
    let ev = make_sub_agent_tool_end(todo_result_json());
    let wrapped = TypedAnyStreamEvent::React(ev);
    let updates = anureo_event_to_updates(&wrapped);
    let plans = find_plan_updates(&updates);
    assert_eq!(
        plans.len(),
        1,
        "TypedAnyStreamEvent::React wrapping sub-agent ToolEnd should produce exactly one plan"
    );
    assert_eq!(plans[0].len(), 3);
}

#[test]
fn sub_agent_plan_entries_have_correct_priority_and_status() {
    let ev = make_sub_agent_tool_end(todo_result_json());
    let updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(ev));
    let plans = find_plan_updates(&updates);
    let entries = &plans[0];
    assert_eq!(entries[0].priority, PlanEntryPriority::High);
    assert_eq!(entries[0].status, PlanEntryStatus::Pending);
    assert_eq!(entries[1].priority, PlanEntryPriority::High);
    assert_eq!(entries[1].status, PlanEntryStatus::Pending);
    assert_eq!(entries[2].priority, PlanEntryPriority::Medium);
    assert_eq!(entries[2].status, PlanEntryStatus::Pending);
}

#[test]
fn sub_agent_plan_deduplicates_with_parent_plan() {
    let sub_ev = make_sub_agent_tool_end(todo_result_json());
    let sub_updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(sub_ev));
    assert_eq!(find_plan_updates(&sub_updates).len(), 1);

    let parent_result = r#"1 todos
[
  { "id": "1", "content": "Parent task", "status": "pending", "priority": "high" }
]"#;
    let parent_ev = make_tool_end("todo_write", parent_result);
    let parent_updates = anureo_event_to_updates(&TypedAnyStreamEvent::React(parent_ev));
    assert_eq!(find_plan_updates(&parent_updates).len(), 1);
    assert_eq!(
        find_plan_updates(&parent_updates)[0][0].content,
        "Parent task"
    );
}

mod session_notifier_tests {
    use agent::run::TypedAnyStreamEvent;
    use agent::state::ReActState;
    use anureo_acp::stream_bridge::SessionNotifier;
    use serde_json::Value;
    use stream_event::StreamEvent;

    fn sub_agent_todo_end() -> StreamEvent<ReActState> {
        StreamEvent::ToolEnd {
            call_id: Some("sub-call-001".to_string()),
            name: "todo_write".to_string(),
            result: r#"3 todos
[
  { "id": "1", "content": "Sub task A", "status": "pending", "priority": "high" },
  { "id": "2", "content": "Sub task B", "status": "pending", "priority": "medium" },
  { "id": "3", "content": "Sub task C", "status": "pending", "priority": "low" }
]"#
            .to_string(),
            is_error: false,
            raw_result: None,
        }
    }

    fn send_and_collect(event: &TypedAnyStreamEvent) -> Vec<Value> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let notifier = SessionNotifier::new(tx, "test-session".into());
        notifier.try_send_event(event);
        let mut results = Vec::new();
        while let Ok(n) = rx.try_recv() {
            if let Ok(v) = serde_json::to_value(&n) {
                results.push(v);
            }
        }
        results
    }

    #[test]
    fn sub_agent_tool_end_produces_plan_notification() {
        let ev = TypedAnyStreamEvent::React(sub_agent_todo_end());
        let notifs = send_and_collect(&ev);
        let has_plan = notifs.iter().any(|n| {
            n.pointer("/Session/update/sessionUpdate")
                .and_then(|v| v.as_str())
                == Some("plan")
        });
        assert!(
            has_plan,
            "sub-agent ToolEnd via TypedAnyStreamEvent::React should produce plan notification"
        );
    }

    #[test]
    fn sub_agent_plan_notification_has_correct_entries() {
        let ev = TypedAnyStreamEvent::React(sub_agent_todo_end());
        let notifs = send_and_collect(&ev);
        let plan_notif = notifs
            .iter()
            .find(|n| {
                n.pointer("/Session/update/sessionUpdate")
                    .and_then(|v| v.as_str())
                    == Some("plan")
            })
            .expect("should have plan notification");
        let entries = plan_notif
            .pointer("/Session/update/entries")
            .and_then(|v| v.as_array())
            .expect("plan should have entries array");
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0].pointer("/content").and_then(|v| v.as_str()),
            Some("Sub task A")
        );
        assert_eq!(
            entries[1].pointer("/content").and_then(|v| v.as_str()),
            Some("Sub task B")
        );
        assert_eq!(
            entries[2].pointer("/content").and_then(|v| v.as_str()),
            Some("Sub task C")
        );
    }

    #[test]
    fn sub_agent_and_parent_plans_both_produce_notifications() {
        let sub_ev = TypedAnyStreamEvent::React(sub_agent_todo_end());
        let parent_ev = TypedAnyStreamEvent::React(StreamEvent::ToolEnd {
            call_id: Some("parent-call-001".to_string()),
            name: "todo_write".to_string(),
            result: r#"1 todos
[
  { "id": "1", "content": "Parent task", "status": "pending", "priority": "high" }
]"#
            .to_string(),
            is_error: false,
            raw_result: None,
        });

        let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
        let notifier = SessionNotifier::new(tx2, "test-session".into());
        notifier.try_send_event(&sub_ev);
        notifier.try_send_event(&parent_ev);
        let mut all_notifs = Vec::new();
        while let Ok(n) = rx2.try_recv() {
            if let Ok(v) = serde_json::to_value(&n) {
                all_notifs.push(v);
            }
        }

        let plan_count = all_notifs
            .iter()
            .filter(|n| {
                n.pointer("/Session/update/sessionUpdate")
                    .and_then(|v| v.as_str())
                    == Some("plan")
            })
            .count();
        assert_eq!(
            plan_count, 2,
            "should produce 2 plan notifications (sub + parent)"
        );
    }
}

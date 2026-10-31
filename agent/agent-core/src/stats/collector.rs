//! `StatsCollector` trait and an in-memory implementation.

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::AgentEvent;

use super::session::{
    ErrorKind, ErrorRecord, LlmCallRecord, SessionStats, SessionStatus, SessionSummary,
    ToolCallRecord,
};

pub trait StatsCollector: Send + Sync {
    fn session_start(&self, session_id: &str, started_at_ms: i64);
    fn record_event(&self, session_id: &str, event: &AgentEvent);
    fn session_end(&self, session_id: &str, ended_at_ms: i64, status: SessionStatus);

    /// Hint for buffered collectors to flush pending writes. The default
    /// implementation is a no-op for in-memory collectors; persistent
    /// implementations override to force a WAL fsync.
    fn flush(&self) {}

    fn now_ms(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}

#[derive(Debug, Default)]
struct InMemorySession {
    summary: SessionSummary,
    llm_calls: Vec<LlmCallRecord>,
    tool_calls: Vec<ToolCallRecord>,
    errors: Vec<ErrorRecord>,
    pending_tool_starts: HashMap<String, i64>,
}

impl InMemorySession {
    fn new(session_id: &str, started_at_ms: i64) -> Self {
        Self {
            summary: SessionSummary {
                session_id: session_id.to_string(),
                started_at_ms,
                status: SessionStatus::Active,
                ..Default::default()
            },
            ..Default::default()
        }
    }
}

pub struct InMemoryCollector {
    sessions: Mutex<HashMap<String, InMemorySession>>,
}

impl InMemoryCollector {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
        })
    }

    pub fn snapshot(&self, session_id: &str) -> Option<SessionStats> {
        let map = self.sessions.lock();
        map.get(session_id).map(|s| SessionStats {
            summary: s.summary.clone(),
            llm_calls: s.llm_calls.clone(),
            tool_calls: s.tool_calls.clone(),
            errors: s.errors.clone(),
        })
    }

    pub fn sessions(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.sessions.lock().keys().cloned().collect();
        ids.sort();
        ids
    }
}

impl StatsCollector for InMemoryCollector {
    fn session_start(&self, session_id: &str, started_at_ms: i64) {
        let mut map = self.sessions.lock();
        map.entry(session_id.to_string())
            .or_insert_with(|| InMemorySession::new(session_id, started_at_ms));
    }

    fn record_event(&self, session_id: &str, event: &AgentEvent) {
        let mut map = self.sessions.lock();
        let Some(session) = map.get_mut(session_id) else {
            return;
        };
        let now = self.now_ms();
        match event {
            AgentEvent::Usage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
                cached_tokens,
            } => {
                session.summary.total_prompt_tokens += u64::from(*prompt_tokens);
                session.summary.total_completion_tokens += u64::from(*completion_tokens);
                session.summary.total_tokens += u64::from(*total_tokens);
                session.summary.total_cached_tokens += u64::from(cached_tokens.unwrap_or(0));
                session.summary.llm_call_count += 1;
                session.llm_calls.push(LlmCallRecord {
                    started_at_ms: now,
                    ended_at_ms: now,
                    latency_ms: 0,
                    prompt_tokens: *prompt_tokens,
                    completion_tokens: *completion_tokens,
                    total_tokens: *total_tokens,
                    cached_tokens: *cached_tokens,
                    model: None,
                });
            }
            AgentEvent::ToolCallStart { name, arguments: _, .. } => {
                session.pending_tool_starts.insert(name.clone(), now);
            }
            AgentEvent::ToolOutput { name: _, content: _ } => {}
            AgentEvent::ToolEnd {
                name,
                result,
                is_error,
                ..
            } => {
                let started_at_ms = session.pending_tool_starts.remove(name).unwrap_or(now);
                session.tool_calls.push(ToolCallRecord {
                    started_at_ms,
                    ended_at_ms: now,
                    latency_ms: now - started_at_ms,
                    name: name.clone(),
                    arguments: String::new(),
                    result_size: result.len(),
                    is_error: *is_error,
                });
                session.summary.tool_call_count += 1;
                if *is_error {
                    session.summary.error_count += 1;
                    session.errors.push(ErrorRecord {
                        at_ms: now,
                        kind: ErrorKind::Tool,
                        message: format!("tool {} failed", name),
                        tool_name: Some(name.clone()),
                    });
                }
            }
            AgentEvent::TextChunk(_) | AgentEvent::ReasoningChunk(_) => {}
        }
    }

    fn session_end(&self, session_id: &str, ended_at_ms: i64, status: SessionStatus) {
        let mut map = self.sessions.lock();
        if let Some(session) = map.get_mut(session_id) {
            session.summary.ended_at_ms = Some(ended_at_ms);
            session.summary.status = status;
        }
    }

    fn flush(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_session_start_creates_summary() {
        let collector = InMemoryCollector::new();
        let started = 1_700_000_000_000;
        collector.session_start("s1", started);
        let snap = collector.snapshot("s1").expect("snapshot present");
        assert_eq!(snap.summary.session_id, "s1");
        assert_eq!(snap.summary.started_at_ms, started);
        assert_eq!(snap.summary.status, SessionStatus::Active);
        assert_eq!(snap.summary.total_prompt_tokens, 0);
        assert!(snap.llm_calls.is_empty());
        assert!(snap.tool_calls.is_empty());
        assert!(snap.errors.is_empty());
        assert_eq!(collector.sessions(), vec!["s1".to_string()]);
    }

    #[test]
    fn in_memory_usage_accumulates_tokens() {
        let collector = InMemoryCollector::new();
        collector.session_start("s1", 1_000);

        collector.record_event(
            "s1",
            &AgentEvent::Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cached_tokens: Some(20),
            },
        );
        collector.record_event(
            "s1",
            &AgentEvent::Usage {
                prompt_tokens: 200,
                completion_tokens: 80,
                total_tokens: 280,
                cached_tokens: Some(40),
            },
        );

        let snap = collector.snapshot("s1").expect("snapshot present");
        assert_eq!(snap.summary.total_prompt_tokens, 300);
        assert_eq!(snap.summary.total_completion_tokens, 130);
        assert_eq!(snap.summary.total_tokens, 430);
        assert_eq!(snap.summary.total_cached_tokens, 60);
        assert_eq!(snap.summary.llm_call_count, 2);
        assert_eq!(snap.llm_calls.len(), 2);
    }

    #[test]
    fn in_memory_cached_tokens_default_zero() {
        let collector = InMemoryCollector::new();
        collector.session_start("s1", 1_000);

        collector.record_event(
            "s1",
            &AgentEvent::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                cached_tokens: None,
            },
        );

        let snap = collector.snapshot("s1").expect("snapshot present");
        assert_eq!(snap.summary.total_cached_tokens, 0);
        assert_eq!(snap.llm_calls[0].cached_tokens, None);
    }

    #[test]
    fn in_memory_tool_lifecycle_latency_non_negative() {
        let collector = InMemoryCollector::new();
        let started = collector.now_ms();
        collector.session_start("s1", started);

        collector.record_event(
            "s1",
            &AgentEvent::ToolCallStart {
                call_id: None,
                name: "bash".into(),
                arguments: "{}".into(),
            },
        );
        collector.record_event(
            "s1",
            &AgentEvent::ToolEnd {
                call_id: None,
                name: "bash".into(),
                result: "ok".into(),
                is_error: false,
            },
        );

        let snap = collector.snapshot("s1").expect("snapshot present");
        assert_eq!(snap.tool_calls.len(), 1);
        assert_eq!(snap.tool_calls[0].name, "bash");
        assert!(!snap.tool_calls[0].is_error);
        assert_eq!(snap.tool_calls[0].result_size, 2);
        assert!(
            snap.tool_calls[0].latency_ms >= 0,
            "latency must be non-negative, got {}",
            snap.tool_calls[0].latency_ms
        );
        assert_eq!(snap.summary.tool_call_count, 1);
        assert_eq!(snap.summary.error_count, 0);
        assert!(snap.errors.is_empty());
    }

    #[test]
    fn in_memory_tool_error_records_error() {
        let collector = InMemoryCollector::new();
        collector.session_start("s1", 1_000);

        collector.record_event(
            "s1",
            &AgentEvent::ToolCallStart {
                call_id: None,
                name: "bash".into(),
                arguments: "{}".into(),
            },
        );
        collector.record_event(
            "s1",
            &AgentEvent::ToolEnd {
                call_id: None,
                name: "bash".into(),
                result: "boom".into(),
                is_error: true,
            },
        );

        let snap = collector.snapshot("s1").expect("snapshot present");
        assert_eq!(snap.summary.tool_call_count, 1);
        assert_eq!(snap.summary.error_count, 1);
        assert_eq!(snap.errors.len(), 1);
        assert_eq!(snap.errors[0].kind, ErrorKind::Tool);
        assert_eq!(snap.errors[0].tool_name.as_deref(), Some("bash"));
        assert!(snap.tool_calls[0].is_error);
    }

    #[test]
    fn in_memory_session_end_marks_status() {
        let collector = InMemoryCollector::new();
        collector.session_start("s1", 1_000);
        let ended = collector.now_ms();
        collector.session_end("s1", ended, SessionStatus::Completed);

        let snap = collector.snapshot("s1").expect("snapshot present");
        assert_eq!(snap.summary.status, SessionStatus::Completed);
        assert_eq!(snap.summary.ended_at_ms, Some(ended));
    }

    #[test]
    fn in_memory_text_and_reasoning_chunks_are_noop() {
        let collector = InMemoryCollector::new();
        collector.session_start("s1", 1_000);

        collector.record_event("s1", &AgentEvent::TextChunk("hello".into()));
        collector.record_event("s1", &AgentEvent::ReasoningChunk("thinking".into()));
        collector.record_event("s1", &AgentEvent::ToolOutput {
            name: "bash".into(),
            content: "partial".into(),
        });

        let snap = collector.snapshot("s1").expect("snapshot present");
        assert_eq!(snap.summary.llm_call_count, 0);
        assert_eq!(snap.summary.tool_call_count, 0);
        assert_eq!(snap.summary.error_count, 0);
        assert!(snap.llm_calls.is_empty());
        assert!(snap.tool_calls.is_empty());
        assert!(snap.errors.is_empty());
    }
}

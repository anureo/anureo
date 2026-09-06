use anureo_llm::LlmUsage;

/// Exit code (EX_TEMPFAIL = 75 from sysexits.h) emitted by `anureo` when the
/// LLM provider returns a transient rate-limit / billing error AND the
/// caller is a Kanban orchestrator that knows how to back off and retry
/// the task instead of marking it failed.
///
/// Gated on `ANUREO_KANBAN_TASK` so the default `anureo` CLI still exits 1
/// for any error — only Kanban-style supervisors (which poll the
/// process exit code) opt in to the recoverable signal.
///
/// Hermes parity (`hermes/cli.py`): the Kanban supervisor already
/// interprets 75 as "re-enqueue, don't fail"; anureo gains the same
/// signal here without changing the protocol.
pub const KANBAN_RATE_LIMIT_EXIT_CODE: i32 = 75;

pub struct TurnResult {
    pub reply: String,
    pub reasoning_content: Option<String>,
    pub tool_calls_summary: Vec<ToolCallSummary>,
    pub usage: Option<LlmUsage>,
    /// Optional work summary for history injection (produced by the tool or post-processed).
    pub work_summary: Option<String>,
}

#[derive(Clone)]
pub struct ToolCallSummary {
    pub tool_name: String,
    pub result_preview: String,
}

/// Tool execution error.
///
/// Used by ActNode to communicate errors back to the LLM for self-correction.
#[derive(Debug, Clone)]
pub enum ToolError {
    /// Generic execution failure (tool not found, runtime error, etc.).
    ExecutionFailed(String),
    Timeout,
    Aborted,
    /// Transient API error (rate-limit, overload) — retryable with backoff.
    RateLimited(String),
    /// Arguments failed JSON validation — LLM should self-correct and retry.
    ///
    /// **Example**: MiniMax-M3 outputs `arguments: "pattern"` instead of
    /// `arguments: "{\"pattern\": \"...\"}"`. We return this error so the LLM
    /// sees the issue and regenerates properly formatted arguments.
    InvalidJsonArguments {
        tool_name: String,
        raw_args: String,
        parse_error: String,
    },
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExecutionFailed(msg) => write!(f, "execution failed: {msg}"),
            Self::Timeout => write!(f, "tool execution timed out"),
            Self::Aborted => write!(f, "tool execution aborted"),
            Self::RateLimited(msg) => write!(f, "rate limited: {msg}"),
            Self::InvalidJsonArguments {
                tool_name,
                raw_args,
                parse_error,
            } => {
                // Short preview of raw_args (first 100 chars).
                let preview = raw_args.chars().take(100).collect::<String>();
                write!(
                    f,
                    "[{tool_name}] invalid arguments: expected valid JSON object string. \
                     Parse error: {parse_error}. Raw input: {preview}",
                )
            }
        }
    }
}

impl ToolError {
    /// Returns true if the `ExecutionFailed` message looks like a transient
    /// API error that is worth retrying (rate-limit, overload, 429, 503, code 1305, etc.).
    pub fn is_transient_api_error(msg: &str) -> bool {
        let lower = msg.to_lowercase();
        // Chinese overload messages
        lower.contains("访问量过大")
            || lower.contains("请稍后再试")
            // Common rate-limit indicators
            || lower.contains("rate limit")
            || lower.contains("rate_limit")
            || lower.contains("too many requests")
            || lower.contains("429")
            || lower.contains("code: 1305")
            // Server overload / temporarily unavailable
            || lower.contains("overloaded")
            || lower.contains("503")
            || lower.contains("service unavailable")
            || lower.contains("please try again")
    }

    /// Returns true if this error indicates the LLM should self-correct.
    ///
    /// When `true`, the agent returns the error to the LLM (instead of silently
    /// falling back) so the model learns to generate valid arguments.
    pub fn is_self_correctable(&self) -> bool {
        matches!(self, Self::InvalidJsonArguments { .. })
    }

    /// Generates an LLM-facing hint for self-correction.
    ///
    /// Returns a message that tells the model what went wrong and how to fix it.
    pub fn self_correct_hint(&self) -> String {
        match self {
            Self::InvalidJsonArguments {
                tool_name,
                raw_args,
                ..
            } => {
                let preview = raw_args.chars().take(150).collect::<String>();
                format!(
                    "[{tool_name}] invalid arguments: expected a JSON object string, \
                     but received: \"{preview}\". \
                     Please provide valid JSON, e.g. {{\"pattern\": \"*.rs\", \"path\": \"src\"}}."
                )
            }
            _ => self.to_string(),
        }
    }
}

/// Result of the goal runner loop.
#[derive(Debug)]
pub enum GoalOutcome {
    Achieved,
    /// The runner stopped because progress was blocked by its safety limits.
    Blocked(String),
    Error(String),
    /// Token budget exhausted; the loop stopped to avoid overruns.
    UsageLimited {
        tokens_used: u32,
        token_budget: u32,
    },
}

impl std::fmt::Display for GoalOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GoalOutcome::Achieved => write!(f, "Goal achieved"),
            GoalOutcome::Blocked(reason) => write!(f, "Goal blocked: {}", reason),
            GoalOutcome::Error(e) => write!(f, "Goal error: {}", e),
            GoalOutcome::UsageLimited {
                tokens_used,
                token_budget,
            } => {
                write!(
                    f,
                    "Token budget exhausted ({}/{})",
                    tokens_used, token_budget
                )
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GoalError {
    #[error("database error: {0}")]
    Db(#[from] Box<dyn std::error::Error>),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("invalid resume state: {0}")]
    Resume(String),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalMeta {
    pub iteration: u32,
    pub tool: String,
    pub time_used_seconds: i64,
    /// Durable lifecycle state, independent from the coarse task scheduler state.
    #[serde(default)]
    pub lifecycle: GoalLifecycle,
    /// Human-readable reason for the latest non-active lifecycle state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_reason: Option<String>,
    /// Model configuration used by the anureo goal tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort used by the anureo goal tool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Optional hard cap on total tokens consumed across all iterations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_budget: Option<u32>,
    /// Cumulative tokens consumed so far.
    #[serde(default)]
    pub tokens_used: u32,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    /// Optional verification command (e.g. "cargo test") run after each iteration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_command: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalLifecycle {
    #[default]
    Active,
    Paused,
    Blocked,
    UsageLimited,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HistoryEntry {
    pub iteration: u32,
    pub timestamp: String,
    /// Short summary of what was accomplished in this iteration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl Default for GoalMeta {
    fn default() -> Self {
        Self {
            iteration: 0,
            tool: "anureo".to_string(),
            time_used_seconds: 0,
            lifecycle: GoalLifecycle::Active,
            lifecycle_reason: None,
            model: None,
            effort: None,
            token_budget: None,
            tokens_used: 0,
            history: Vec::new(),
            verify_command: None,
        }
    }
}

pub const DEFAULT_MAX_ITERATIONS: u32 = 100;
pub const MAX_CONSECUTIVE_FAILURES: u32 = 3;
pub const MAX_HISTORY_ENTRIES: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_error_execution_failed_display() {
        let error = ToolError::ExecutionFailed("Tool not found".to_string());
        let display = format!("{}", error);
        assert_eq!(display, "execution failed: Tool not found");
    }

    #[test]
    fn test_tool_error_timeout_display() {
        let error = ToolError::Timeout;
        let display = format!("{}", error);
        assert_eq!(display, "tool execution timed out");
    }

    #[test]
    fn test_tool_error_aborted_display() {
        let error = ToolError::Aborted;
        let display = format!("{}", error);
        assert_eq!(display, "tool execution aborted");
    }

    #[test]
    fn test_tool_error_rate_limited_display() {
        let error = ToolError::RateLimited("Too many requests".to_string());
        let display = format!("{}", error);
        assert_eq!(display, "rate limited: Too many requests");
    }

    #[test]
    fn test_tool_error_invalid_json_arguments_display() {
        let error = ToolError::InvalidJsonArguments {
            tool_name: "grep_tool".to_string(),
            raw_args: r#"pattern: "*.rs""#.to_string(),
            parse_error: "expected colon at line 1 column 9".to_string(),
        };
        let display = format!("{}", error);
        assert!(display.contains("[grep_tool]"));
        assert!(display.contains("invalid arguments: expected valid JSON object string"));
        assert!(display.contains("Parse error: expected colon at line 1 column 9"));
        assert!(display.contains("Raw input:"));
        assert!(display.contains(r#"pattern: "*.rs""#));
    }

    #[test]
    fn test_tool_error_invalid_json_arguments_long_truncation() {
        let long_args = "z".repeat(150);
        let error = ToolError::InvalidJsonArguments {
            tool_name: "test_tool".to_string(),
            raw_args: long_args.clone(),
            parse_error: "parse error".to_string(),
        };
        let display = format!("{}", error);

        // Display should truncate to 100 chars in the preview part
        assert!(display.contains("Raw input:"));

        // Check that the Display impl truncates to 100 chars in the preview (using 'z' which doesn't appear elsewhere)
        let z_count_in_display = display.matches("z").count();
        assert_eq!(
            z_count_in_display, 100,
            "Display should contain exactly 100 'z' characters in the truncated preview"
        );

        // Also check that self_correct_hint truncates to 150 chars
        let hint = error.self_correct_hint();
        let z_count_in_hint = hint.matches("z").count();
        assert_eq!(
            z_count_in_hint, 150,
            "Self-correct hint should contain exactly 150 'z' characters in the truncated preview"
        );
    }

    #[test]
    fn test_tool_call_summary_fields() {
        let summary = ToolCallSummary {
            tool_name: "read_file".to_string(),
            result_preview: "Successfully read file contents".to_string(),
        };

        assert_eq!(summary.tool_name, "read_file");
        assert_eq!(summary.result_preview, "Successfully read file contents");
    }

    #[test]
    fn test_turn_result_fields() {
        let turn_result = TurnResult {
            reply: "Task completed successfully".to_string(),
            reasoning_content: Some("Using grep to find the pattern".to_string()),
            tool_calls_summary: vec![ToolCallSummary {
                tool_name: "grep".to_string(),
                result_preview: "Found 5 matches".to_string(),
            }],
            usage: None,
            work_summary: Some("Completed grep search across source files".to_string()),
        };

        assert_eq!(turn_result.reply, "Task completed successfully");
        assert_eq!(
            turn_result.reasoning_content,
            Some("Using grep to find the pattern".to_string())
        );
        assert_eq!(turn_result.tool_calls_summary.len(), 1);
        assert_eq!(turn_result.tool_calls_summary[0].tool_name, "grep");
        assert!(turn_result.usage.is_none());
        assert_eq!(
            turn_result.work_summary,
            Some("Completed grep search across source files".to_string())
        );
    }

    #[test]
    fn test_turn_result_empty() {
        let turn_result = TurnResult {
            reply: String::new(),
            reasoning_content: None,
            tool_calls_summary: vec![],
            usage: None,
            work_summary: None,
        };

        assert!(turn_result.reply.is_empty());
        assert!(turn_result.reasoning_content.is_none());
        assert!(turn_result.tool_calls_summary.is_empty());
        assert!(turn_result.usage.is_none());
        assert!(turn_result.work_summary.is_none());
    }

    #[test]
    fn test_tool_error_is_transient_api_error_rate_limit() {
        let msg = "rate limit exceeded, please try again later";
        assert!(ToolError::is_transient_api_error(msg));
    }

    #[test]
    fn test_tool_error_is_transient_api_error_chinese_messages() {
        let chinese_msg1 = "访问量过大，请稍后再试";
        let chinese_msg2 = "请稍后再试";
        assert!(ToolError::is_transient_api_error(chinese_msg1));
        assert!(ToolError::is_transient_api_error(chinese_msg2));
    }

    #[test]
    fn test_tool_error_is_transient_api_error_429() {
        let msg = "HTTP 429 - Too Many Requests";
        assert!(ToolError::is_transient_api_error(msg));
    }

    #[test]
    fn test_tool_error_is_transient_api_error_503() {
        let msg = "HTTP 503 Service Unavailable";
        assert!(ToolError::is_transient_api_error(msg));
    }

    #[test]
    fn test_tool_error_is_transient_api_error_code_1305() {
        let msg = "Error code: 1305 - Rate limit exceeded";
        assert!(ToolError::is_transient_api_error(msg));
    }

    #[test]
    fn test_tool_error_is_transient_api_error_overloaded() {
        let msg = "Service overloaded, please try again";
        assert!(ToolError::is_transient_api_error(msg));
    }

    #[test]
    fn test_tool_error_is_transient_api_error_permanent_failure() {
        let msg = "Tool not found";
        assert!(!ToolError::is_transient_api_error(msg));
    }

    #[test]
    fn test_tool_error_is_self_correctable() {
        let json_error = ToolError::InvalidJsonArguments {
            tool_name: "grep_tool".to_string(),
            raw_args: "invalid".to_string(),
            parse_error: "parse error".to_string(),
        };
        assert!(json_error.is_self_correctable());

        let exec_error = ToolError::ExecutionFailed("Tool not found".to_string());
        assert!(!exec_error.is_self_correctable());

        let timeout_error = ToolError::Timeout;
        assert!(!timeout_error.is_self_correctable());

        let aborted_error = ToolError::Aborted;
        assert!(!aborted_error.is_self_correctable());

        let rate_limited_error = ToolError::RateLimited("Too many requests".to_string());
        assert!(!rate_limited_error.is_self_correctable());
    }

    #[test]
    fn test_tool_error_self_correct_hint_for_json_error() {
        let json_error = ToolError::InvalidJsonArguments {
            tool_name: "grep_tool".to_string(),
            raw_args: r#"pattern: "*.rs""#.to_string(),
            parse_error: "parse error".to_string(),
        };

        let hint = json_error.self_correct_hint();
        assert!(hint.contains("[grep_tool]"));
        assert!(hint.contains("invalid arguments"));
        assert!(hint.contains("expected a JSON object string"));
        assert!(hint.contains(r#"pattern: "*.rs""#));
        assert!(hint.contains("Please provide valid JSON"));
    }

    #[test]
    fn test_tool_error_self_correct_hint_for_other_errors() {
        let exec_error = ToolError::ExecutionFailed("Tool not found".to_string());
        let hint = exec_error.self_correct_hint();
        assert_eq!(hint, "execution failed: Tool not found");

        let timeout_error = ToolError::Timeout;
        let hint = timeout_error.self_correct_hint();
        assert_eq!(hint, "tool execution timed out");
    }

    #[test]
    fn test_goal_outcome_display() {
        assert_eq!(format!("{}", GoalOutcome::Achieved), "Goal achieved");

        assert_eq!(
            format!("{}", GoalOutcome::Blocked("no progress".to_string())),
            "Goal blocked: no progress"
        );

        assert_eq!(
            format!("{}", GoalOutcome::Error("Test error".to_string())),
            "Goal error: Test error"
        );

        assert_eq!(
            format!(
                "{}",
                GoalOutcome::UsageLimited {
                    tokens_used: 100,
                    token_budget: 1000
                }
            ),
            "Token budget exhausted (100/1000)"
        );
    }

    #[test]
    fn test_goal_meta_default() {
        let default_meta = GoalMeta::default();

        assert_eq!(default_meta.iteration, 0);
        assert_eq!(default_meta.tool, "anureo");
        assert_eq!(default_meta.time_used_seconds, 0);
        assert_eq!(default_meta.lifecycle, GoalLifecycle::Active);
        assert!(default_meta.lifecycle_reason.is_none());
        assert!(default_meta.token_budget.is_none());
        assert_eq!(default_meta.tokens_used, 0);
        assert!(default_meta.history.is_empty());
        assert!(default_meta.verify_command.is_none());
    }

    #[test]
    fn test_goal_meta_legacy_json_defaults_to_active() {
        let meta: GoalMeta = serde_json::from_str(
            r#"{
                "iteration": 3,
                "tool": "anureo",
                "time_used_seconds": 12,
                "tokens_used": 7,
                "history": []
            }"#,
        )
        .expect("legacy goal metadata should remain readable");

        assert_eq!(meta.iteration, 3);
        assert_eq!(meta.lifecycle, GoalLifecycle::Active);
        assert!(meta.lifecycle_reason.is_none());
    }

    #[test]
    fn test_goal_meta_with_values() {
        let meta = GoalMeta {
            iteration: 5,
            tool: "test_tool".to_string(),
            time_used_seconds: 120,
            lifecycle: GoalLifecycle::Active,
            lifecycle_reason: None,
            model: Some("test-model".to_string()),
            effort: Some("high".to_string()),
            token_budget: Some(5000),
            tokens_used: 2500,
            history: vec![],
            verify_command: Some("cargo test".to_string()),
        };

        assert_eq!(meta.iteration, 5);
        assert_eq!(meta.tool, "test_tool");
        assert_eq!(meta.time_used_seconds, 120);
        assert_eq!(meta.token_budget, Some(5000));
        assert_eq!(meta.tokens_used, 2500);
        assert_eq!(meta.verify_command, Some("cargo test".to_string()));
    }

    #[test]
    fn test_history_entry_fields() {
        let entry = HistoryEntry {
            iteration: 1,
            timestamp: "2025-08-19T10:30:00Z".to_string(),
            summary: Some("Completed grep search".to_string()),
        };

        assert_eq!(entry.iteration, 1);
        assert_eq!(entry.timestamp, "2025-08-19T10:30:00Z");
        assert_eq!(entry.summary, Some("Completed grep search".to_string()));
    }

    #[test]
    fn test_history_entry_without_summary() {
        let entry = HistoryEntry {
            iteration: 2,
            timestamp: "2025-08-19T10:31:00Z".to_string(),
            summary: None,
        };

        assert_eq!(entry.iteration, 2);
        assert_eq!(entry.timestamp, "2025-08-19T10:31:00Z");
        assert!(entry.summary.is_none());
    }

    #[test]
    fn test_constants_values() {
        assert_eq!(DEFAULT_MAX_ITERATIONS, 100);
        assert_eq!(MAX_CONSECUTIVE_FAILURES, 3);
        assert_eq!(MAX_HISTORY_ENTRIES, 20);
    }

    #[test]
    fn test_tool_error_clone() {
        let error = ToolError::ExecutionFailed("Test error".to_string());
        let cloned = error.clone();
        assert_eq!(format!("{}", error), format!("{}", cloned));

        let timeout = ToolError::Timeout;
        let cloned_timeout = timeout.clone();
        assert_eq!(format!("{}", timeout), format!("{}", cloned_timeout));
    }

    #[test]
    fn test_tool_call_summary_clone() {
        let summary = ToolCallSummary {
            tool_name: "test_tool".to_string(),
            result_preview: "Test result".to_string(),
        };

        let cloned = summary.clone();
        assert_eq!(summary.tool_name, cloned.tool_name);
        assert_eq!(summary.result_preview, cloned.result_preview);
    }

    #[test]
    fn test_goal_outcome_debug() {
        let achieved = GoalOutcome::Achieved;
        let blocked = GoalOutcome::Blocked("no progress".to_string());
        let error = GoalOutcome::Error("Test error".to_string());
        let limited = GoalOutcome::UsageLimited {
            tokens_used: 100,
            token_budget: 1000,
        };

        assert!(format!("{:?}", achieved).contains("Achieved"));
        assert!(format!("{:?}", blocked).contains("Blocked"));
        assert!(format!("{:?}", error).contains("Error"));
        assert!(format!("{:?}", limited).contains("UsageLimited"));
    }

    #[test]
    fn test_turn_result_with_multiple_tool_summaries() {
        let turn_result = TurnResult {
            reply: "Completed multiple operations".to_string(),
            reasoning_content: None,
            tool_calls_summary: vec![
                ToolCallSummary {
                    tool_name: "read_file".to_string(),
                    result_preview: "Read file contents".to_string(),
                },
                ToolCallSummary {
                    tool_name: "grep".to_string(),
                    result_preview: "Found 3 matches".to_string(),
                },
                ToolCallSummary {
                    tool_name: "write_file".to_string(),
                    result_preview: "Written 200 bytes".to_string(),
                },
            ],
            usage: None,
            work_summary: None,
        };

        assert_eq!(turn_result.tool_calls_summary.len(), 3);
        assert_eq!(turn_result.tool_calls_summary[0].tool_name, "read_file");
        assert_eq!(turn_result.tool_calls_summary[1].tool_name, "grep");
        assert_eq!(turn_result.tool_calls_summary[2].tool_name, "write_file");
    }

    #[test]
    fn test_tool_error_case_insensitive_transient_detection() {
        let msg = "RATE LIMIT EXCEEDED";
        assert!(ToolError::is_transient_api_error(msg));

        let msg = "Rate Limit Exceeded";
        assert!(ToolError::is_transient_api_error(msg));

        let msg = "rate_limit_exceeded";
        assert!(ToolError::is_transient_api_error(msg));
    }

    #[test]
    fn test_tool_error_all_transient_patterns() {
        let patterns = vec![
            "Rate limit reached",
            "rate_limit",
            "Too many requests",
            "HTTP 429",
            "Code: 1305",
            "Server overloaded",
            "503 Service unavailable",
            "Please try again",
        ];

        for pattern in patterns {
            assert!(
                ToolError::is_transient_api_error(pattern),
                "Pattern should be transient: {}",
                pattern
            );
        }
    }
}

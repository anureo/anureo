pub mod message;
pub mod state;

// Re-export key types at module level
pub use message::{build_continuation_prompt, escape_xml_text};
pub use state::{
    GoalError, GoalLifecycle, GoalMeta, GoalOutcome, HistoryEntry, ToolError, TurnResult,
    DEFAULT_MAX_ITERATIONS,
    MAX_CONSECUTIVE_FAILURES, MAX_HISTORY_ENTRIES,
};

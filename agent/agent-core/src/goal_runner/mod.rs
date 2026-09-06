//! **goal-codex-alignment P6 冻结（只读）**：旧 detached goal runner 的核心
//! 状态与提示词类型（GoalMeta/GoalLifecycle 等）。保留仅供 `anureo goal
//! --migrate` 迁移与审计读取，新路径 = `agent/goal` crate（thread_goals）；
//! 移除见 P7。注意：ToolError/TurnResult 仍被 agent-core react 循环与 CLI
//! 复用（act_utils.rs、run_flow.rs），移除时需先迁走共享类型。
pub mod message;
pub mod state;

// Re-export key types at module level
pub use message::{build_continuation_prompt, escape_xml_text};
pub use state::{
    GoalError, GoalLifecycle, GoalMeta, GoalOutcome, HistoryEntry, ToolError, TurnResult,
    DEFAULT_MAX_ITERATIONS,
    MAX_CONSECUTIVE_FAILURES, MAX_HISTORY_ENTRIES,
};

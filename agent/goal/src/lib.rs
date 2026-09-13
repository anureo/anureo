//! anureo goal 系统（Codex ext/goal 同构）。
//!
//! 语义基线见 `docs/goal/goal-codex-alignment.md` §6（原样搬运，禁止私自改进）。
//! Phase 1（本 crate 骨架）：types + store + `thread_goals` 迁移；
//! accounting/runtime/tools/service 由 Phase 2-4 落地。
//!
//! 依赖方向约束：`apps/* → goal → task-core`；本 crate **不依赖 agent-core**。

pub mod accounting;
pub mod metrics;
pub mod objective_file;
#[cfg(feature = "otel")]
pub mod otel;
pub mod runtime;
pub mod service;
pub mod steering;
pub mod store;
pub mod tool_accounting;
pub mod tools;
pub mod types;

pub use accounting::{goal_token_delta, GoalAccounting, TokenTotals, LOCK_TIMEOUT};
pub use metrics::{GoalMetrics, GoalMetricsSnapshot};
pub use objective_file::{objective_exceeds_limit, OBJECTIVE_INLINE_LIMIT_CHARS};
pub use runtime::{GoalRuntimeHandle, GoalTurnMeta, GoalTurnReason, StatusNotifier, TurnDriver};
pub use service::{GoalService, GoalServiceError, GoalStateLock};
pub use steering::{budget_limit, continuation, escape_xml_text, objective_updated};
pub use store::{GoalStore, GoalStoreError};
pub use tool_accounting::{ToolAccounting, EXEC_TOOL_NAMES, MAX_CONSECUTIVE_EXECUTION_FAILURES};
pub use tools::{
    goal_tools, goal_tools_with_usage, render_goal_snapshot, GoalUsageSnapshot, ShellVerifyRunner,
    VerifyOutcome, VerifyRunner,
};
pub use types::{
    max_goal_token_budget, AccountingMode, AccountingOutcome, CreateGoalRequest, Goal, GoalStatus,
    GoalValidationError, TurnErrorClass, MAX_OBJECTIVE_LEN,
};

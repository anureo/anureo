//! REPL `/goal` subcommand support (goal-codex-alignment P5).
//!
//! The REPL is a single long-lived agent thread, so its goal lifecycle binds
//! to the run's `--thread` id (falling back to the fixed id `"repl"` when
//! unset — the receipt says so when the fallback is used). Persists to the
//! shared task db (`<anureo_home>/tasks/tasks.db`), the same store the ACP
//! host uses, so goals follow the thread across frontends.
//!
//! Unlike the ACP host there is no in-session turn loop to hook here, so the
//! REPL talks straight to [`goal::GoalService`] (no runtime handle, no idle
//! auto-continuation: the user drives turns manually).

use agent::commands::GoalSubcommand;

/// Execute a `/goal` subcommand from the REPL loop and return the receipt
/// printed to stdout.
pub(crate) async fn run_goal_subcommand(subcommand: GoalSubcommand, thread_id: &str) -> String {
    let dir = config::home::anureo_home().join("tasks");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return format!("Goal command failed: create {}: {e}", dir.display());
    }
    let db = match task_core::TaskDb::open(&dir.join("tasks.db")).await {
        Ok(db) => db,
        Err(e) => return format!("Goal command failed: open task db: {e}"),
    };
    let service = goal::GoalService::new(goal::GoalStore::from_task_db(&db));
    match subcommand {
        GoalSubcommand::Set { description } => {
            match service.set_with_verify(thread_id, &description, None, None).await {
                // P7：文件化 goal 还原全文展示。
                Ok(goal) => format!("Goal armed: {}", service.resolve_objective(goal).await.objective),
                Err(e) => format!("Goal set failed: {e}"),
            }
        }
        GoalSubcommand::Show => match service.show(thread_id).await {
            Ok(Some(goal)) => {
                // P7 文件化：还原全文展示（REPL 可读全文）。
                goal::render_goal_snapshot(&service.resolve_objective(goal).await)
            }
            Ok(None) => "No goal set.".to_string(),
            Err(e) => format!("Goal show failed: {e}"),
        },
        GoalSubcommand::Pause => match service.pause(thread_id).await {
            Ok(goal) => format!("Goal paused ({} tokens used).", goal.tokens_used),
            Err(e) => format!("Goal pause failed: {e}"),
        },
        GoalSubcommand::Resume => match service.resume(thread_id).await {
            Ok(_) => "Goal resumed.".to_string(),
            Err(e) => format!("Goal resume failed: {e}"),
        },
        GoalSubcommand::Budget { tokens } => {
            match service.update_budget(thread_id, tokens).await {
                Ok(goal) if goal.status == goal::GoalStatus::BudgetLimited => {
                    "Budget updated (tokens used kept). Goal is budget_limited — run /goal resume to continue."
                        .to_string()
                }
                Ok(_) => format!("Budget updated to {tokens} tokens (tokens used kept)."),
                Err(e) => format!("Goal budget update failed: {e}"),
            }
        }
        GoalSubcommand::Clear => match service.clear(thread_id).await {
            Ok(true) => "Goal cleared.".to_string(),
            Ok(false) => "No goal set.".to_string(),
            Err(e) => format!("Goal clear failed: {e}"),
        },
        GoalSubcommand::Edit { description } => match service.edit(thread_id, &description).await {
            Ok(goal) => {
                format!("Goal objective updated: {}", service.resolve_objective(goal).await.objective)
            }
            Err(e) => format!("Goal edit failed: {e}"),
        },
    }
}

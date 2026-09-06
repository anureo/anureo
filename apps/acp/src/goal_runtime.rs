//! Goal runtime host wiring for the ACP agent (goal-codex-alignment P5).
//!
//! This module adapts the `goal` crate to the live ACP host:
//!
//! - [`AcpTurnDriver`] implements [`goal::TurnDriver`] by routing idle goal
//!   continuation prompts through the agent's normal `prompt` entry point,
//!   so every continuation is a real session turn — checkpointed, streamed,
//!   cancelable, and visible to the client.
//! - [`run_goal_subcommand`] executes `/goal set|show|pause|resume|clear|edit`
//!   against the thread's [`goal::GoalRuntimeHandle`] and returns a receipt
//!   string the prompt handler forwards to the client.

use std::sync::Weak;

use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, TextContent};

use crate::agent::AnureoAcpAgent;

/// Host adapter that starts idle goal-continuation turns through the normal
/// ACP prompt path.
///
/// Holds only a [`Weak`] reference to the agent so the runtime handle (cached
/// on the agent) never creates a reference cycle.
pub(crate) struct AcpTurnDriver {
    pub(crate) agent: Weak<AnureoAcpAgent>,
}

#[async_trait::async_trait]
impl goal::TurnDriver for AcpTurnDriver {
    async fn start_turn_if_idle(&self, thread_id: &str, message: &str) -> Result<bool, String> {
        let Some(agent) = self.agent.upgrade() else {
            // Agent dropped (embedded test runtime); nothing to drive.
            return Ok(false);
        };
        let Some(session_id) = agent.sessions().find_session_id_by_thread(thread_id) else {
            // No live session bound to this thread (e.g. goal restored at
            // startup before the client reopens the session). Skip; recovery
            // re-attempts on the next `goal/list`-driven reservation.
            return Ok(false);
        };
        if agent.sessions().has_active_prompt(&session_id) {
            // A user turn is already running; the goal runtime will fold its
            // usage in when that turn finishes.
            return Ok(false);
        }
        let request = PromptRequest::new(
            session_id.to_string(),
            vec![ContentBlock::Text(TextContent::new(message.to_string()))],
        );
        let session_log = session_id.to_string();
        let agent = agent.clone();
        // Detach: `start_turn_if_idle` must not block the goal state lock on a
        // full LLM turn. `begin_prompt` inside `prompt` is the authoritative
        // idempotency gate — if another prompt wins the race, it errors out
        // here and we just log.
        tokio::spawn(async move {
            if let Err(e) = agent.prompt(request).await {
                tracing::warn!(
                    session_id = %session_log,
                    error = ?e,
                    "goal continuation prompt failed"
                );
            }
        });
        Ok(true)
    }
}

/// Execute a parsed `/goal` subcommand against the thread's goal runtime and
/// produce a human-readable receipt for the client.
///
/// Errors are rendered into the receipt (rather than failing the prompt) so
/// `/goal show` on a broken store still answers instead of killing the turn.
pub(crate) async fn run_goal_subcommand(
    runtime: &std::sync::Arc<goal::GoalRuntimeHandle>,
    thread_id: &str,
    subcommand: agent::commands::GoalSubcommand,
) -> String {
    use agent::commands::GoalSubcommand;

    let service = runtime.service();
    match subcommand {
        GoalSubcommand::Set { description } => {
            match service.set_with_verify(thread_id, &description, None, None).await {
                Ok(goal) => {
                    runtime.note_goal_armed(&goal.goal_id).await;
                    format!("Goal armed: {}", goal.objective)
                }
                Err(e) => format!("Goal set failed: {e}"),
            }
        }
        GoalSubcommand::Show => match service.show(thread_id).await {
            Ok(Some(goal)) => goal::render_goal_snapshot(&goal),
            Ok(None) => "No goal set.".to_string(),
            Err(e) => format!("Goal show failed: {e}"),
        },
        GoalSubcommand::Pause => match service.pause(thread_id).await {
            Ok(goal) => {
                // Flush the wall clock and mirror the status in memory.
                let _ = runtime.on_goal_status_changed(goal::GoalStatus::Paused).await;
                format!("Goal paused ({} tokens used).", goal.tokens_used)
            }
            Err(e) => format!("Goal pause failed: {e}"),
        },
        GoalSubcommand::Resume => match service.resume(thread_id).await {
            Ok(_goal) => {
                let _ = runtime.on_goal_status_changed(goal::GoalStatus::Active).await;
                "Goal resumed.".to_string()
            }
            Err(e) => format!("Goal resume failed: {e}"),
        },
        GoalSubcommand::Clear => match service.clear(thread_id).await {
            Ok(true) => {
                // Drop the in-memory snapshot; accounting for the old goal
                // stops immediately (store rows are already gone).
                let _ = runtime
                    .on_goal_replaced(None, goal::TokenTotals::default())
                    .await;
                "Goal cleared.".to_string()
            }
            Ok(false) => "No goal set.".to_string(),
            Err(e) => format!("Goal clear failed: {e}"),
        },
        GoalSubcommand::Edit { description } => match service.edit(thread_id, &description).await {
            Ok(goal) => {
                // Objective changes are picked up by the runtime's
                // turn-boundary refresh (`on_turn_start` reloads the store
                // snapshot), so no in-memory hook is needed here.
                format!("Goal objective updated: {}", goal.objective)
            }
            Err(e) => format!("Goal edit failed: {e}"),
        },
    }
}

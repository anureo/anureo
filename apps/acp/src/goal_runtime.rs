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
        let thread_id = thread_id.to_string();
        let agent = agent.clone();
        // Detach: `start_turn_if_idle` must not block the goal state lock on a
        // full LLM turn. `begin_prompt` inside `prompt` is the authoritative
        // idempotency gate — if another prompt wins the race, it errors out
        // here and we just log.
        tokio::spawn(async move {
            if let Err(e) = agent.prompt_goal_continuation(&thread_id, request).await {
                tracing::warn!(
                    session_id = %session_log,
                    error = ?e,
                    "goal continuation prompt failed"
                );
            }
        });
        Ok(true)
    }

    /// C1（G9）：goal 驱动 turn 的元数据日志（codex `turn_trigger:"goal"`
    /// 等价物）。FE 实时区分依赖 `_meta.goal` 快照的 `iterationCount`（每次
    /// goal turn +1）；`goal/continuation` 实时通知需 ConnectionRegistry
    /// 注入 driver，随 FE 集成一并落地（见 gap-remediation §5 C1）。
    async fn start_goal_turn(
        &self,
        thread_id: &str,
        message: &str,
        meta: goal::GoalTurnMeta,
    ) -> Result<bool, String> {
        tracing::info!(
            thread_id = %thread_id,
            goal_id = %meta.goal_id,
            iteration = meta.iteration,
            reason = ?meta.reason,
            "goal-driven turn starting"
        );
        self.start_turn_if_idle(thread_id, message).await
    }
}

/// A2（gap-remediation G5）：`/goal` 命令 turn 在 `prompt()` 内提前 return，
/// 不会经过 prompt 尾部的 `continue_if_idle` 钩子，因此 fresh set / resume
/// 需在此处自行 kick（对齐 `_session/goal` set/resume 与 codex
/// `apply_external_goal_set` → `continue_if_idle`）。
///
/// 幂等由 `AcpTurnDriver::start_turn_if_idle` 的 busy gate + `prompt()` 的
/// `begin_prompt` 互斥双重保证；detach spawn 避免阻塞回执发送。
fn kick_idle_continuation(runtime: &std::sync::Arc<goal::GoalRuntimeHandle>) {
    let runtime = runtime.clone();
    tokio::spawn(async move {
        if let Err(e) = runtime.continue_if_idle().await {
            tracing::warn!(error = %e, "/goal command: idle continuation failed");
        }
    });
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
            match service
                .set_with_verify_outcome(thread_id, &description, None, None)
                .await
            {
                Ok(outcome) => {
                    runtime.note_goal_armed(&outcome.goal.goal_id).await;
                    if outcome.replaced_existing {
                        // §6.6 快照替换：用户刚介入，idle 续跑推迟到下一 turn 边界。
                        if let Err(e) = runtime.defer_continuation().await {
                            tracing::warn!(error = %e, "/goal set: defer after replace failed");
                        }
                    } else {
                        // A2：fresh set → 立即尝试 idle 续跑（三入口一致）。
                        kick_idle_continuation(runtime);
                    }
                    // 文件化 goal 还原全文用于回执展示。
                    let goal = service.resolve_objective(outcome.goal).await;
                    format!("Goal armed: {}", goal.objective)
                }
                Err(e) => format!("Goal set failed: {e}"),
            }
        }
        GoalSubcommand::Show => match service.show(thread_id).await {
            Ok(Some(goal)) => {
                // P7 文件化：还原全文展示（REPL 可读全文）。
                let goal = service.resolve_objective(goal).await;
                goal::render_goal_snapshot(&goal)
            }
            Ok(None) => "No goal set.".to_string(),
            Err(e) => format!("Goal show failed: {e}"),
        },
        GoalSubcommand::Pause => match service.pause(thread_id).await {
            Ok(goal) => {
                // Flush the wall clock and mirror the status in memory.
                let _ = runtime
                    .on_goal_status_changed(goal::GoalStatus::Paused)
                    .await;
                format!("Goal paused ({} tokens used).", goal.tokens_used)
            }
            Err(e) => format!("Goal pause failed: {e}"),
        },
        GoalSubcommand::Resume => match service.resume(thread_id).await {
            Ok(_goal) => {
                let _ = runtime
                    .on_goal_status_changed(goal::GoalStatus::Active)
                    .await;
                // A2：resume 后立即尝试 idle 续跑（对齐 `_session/goal` resume）。
                kick_idle_continuation(runtime);
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
        GoalSubcommand::Budget { tokens } => match service.update_budget(thread_id, tokens).await {
            Ok(goal) => {
                // B2：提额保留 goal_id/tokens_used；budget_limited 需再 resume
                // 才恢复续跑（回执给出引导；resume 路径自带 kick）。
                if goal.status == goal::GoalStatus::BudgetLimited {
                    "Budget updated (tokens_used kept). Goal is budget_limited — run /goal resume to continue.".to_string()
                } else {
                    format!("Budget updated to {tokens} tokens (tokens_used kept).")
                }
            }
            Err(e) => format!("Goal budget update failed: {e}"),
        },
        GoalSubcommand::Edit { description } => match service.edit(thread_id, &description).await {
            Ok(goal) => {
                // Objective changes are picked up by the runtime's
                // turn-boundary refresh (`on_turn_start` reloads the store
                // snapshot), so no in-memory hook is needed here.
                let goal = service.resolve_objective(goal).await;
                format!("Goal objective updated: {}", goal.objective)
            }
            Err(e) => format!("Goal edit failed: {e}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::run_goal_subcommand;
    use agent::commands::GoalSubcommand;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    struct RecordingDriver {
        started: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl goal::TurnDriver for RecordingDriver {
        async fn start_turn_if_idle(&self, thread_id: &str, message: &str) -> Result<bool, String> {
            self.started
                .lock()
                .await
                .push(format!("{thread_id}|{message}"));
            Ok(true)
        }
    }

    async fn setup() -> (Arc<goal::GoalRuntimeHandle>, Arc<RecordingDriver>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .expect("open TaskDb");
        let store = goal::GoalStore::from_task_db(&db);
        let driver = Arc::new(RecordingDriver {
            started: Mutex::new(Vec::new()),
        });
        let runtime = goal::GoalRuntimeHandle::new(store, "t-embed", driver.clone());
        std::mem::forget(dir); // 测试进程生命周期内保持
        (runtime, driver)
    }

    async fn wait_for_starts(driver: &RecordingDriver, expected: usize) {
        for _ in 0..200 {
            if driver.started.lock().await.len() >= expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("continuation kick never reached {expected} starts");
    }

    /// A2（G5）：`/goal set`（fresh）走 prompt 提前 return 路径，必须在此处
    /// 自行 kick idle 续跑——三入口行为一致。
    #[tokio::test]
    async fn fresh_set_kicks_idle_continuation() {
        let (runtime, driver) = setup().await;
        let receipt = run_goal_subcommand(
            &runtime,
            "t-embed",
            GoalSubcommand::Set {
                description: "do the thing".into(),
            },
        )
        .await;
        assert!(receipt.contains("Goal armed"), "{receipt}");
        wait_for_starts(&driver, 1).await;
        assert!(driver.started.lock().await[0].contains("Continue working"));
    }

    /// A2：替换（replaced_existing）保持 §6.6 deferral——不得立即续跑。
    #[tokio::test]
    async fn replaced_set_defers_continuation() {
        let (runtime, driver) = setup().await;
        let _ = run_goal_subcommand(
            &runtime,
            "t-embed",
            GoalSubcommand::Set {
                description: "first".into(),
            },
        )
        .await;
        wait_for_starts(&driver, 1).await;

        let receipt = run_goal_subcommand(
            &runtime,
            "t-embed",
            GoalSubcommand::Set {
                description: "second".into(),
            },
        )
        .await;
        assert!(receipt.contains("Goal armed"), "{receipt}");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        let started = driver.started.lock().await;
        assert_eq!(started.len(), 1, "替换 set 不得立即续跑（deferral 生效）");
    }

    /// A2：`/goal resume` 后 kick 续跑（对齐 `_session/goal` resume）；
    /// pause 期间不 kick。
    #[tokio::test]
    async fn resume_kicks_idle_continuation_but_pause_does_not() {
        let (runtime, driver) = setup().await;
        let _ = run_goal_subcommand(
            &runtime,
            "t-embed",
            GoalSubcommand::Set {
                description: "g".into(),
            },
        )
        .await;
        wait_for_starts(&driver, 1).await;

        let receipt = run_goal_subcommand(&runtime, "t-embed", GoalSubcommand::Pause).await;
        assert!(receipt.contains("paused"), "{receipt}");
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert_eq!(driver.started.lock().await.len(), 1, "paused 不得续跑");

        let receipt = run_goal_subcommand(&runtime, "t-embed", GoalSubcommand::Resume).await;
        assert!(receipt.contains("resumed"), "{receipt}");
        wait_for_starts(&driver, 2).await;
    }
}

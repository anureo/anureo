//! ACP-internal goal runner for `/goal` prompts.
//!
//! When the user types `/goal <description>` in the IDE, the ACP prompt handler
//! delegates to [`run_goal`] which creates a `GoalRunner` with a `AnureoTool` that
//! bridges events back to the IDE via `session/update` notifications.
//!
//! **goal-codex-alignment P6 冻结**：本模块是旧 detached goal runner，已不在
//! 关键路径（`/goal` 与 `_anureo.dev/goal/*` 均切至 `agent/goal` crate 的
//! thread_goals 后端）；保留仅供 `anureo goal` legacy CLI 与迁移审计，
//! 移除见 P7。本文件同时收容旧 goals.json 存取与 RuntimeControl 机制
//! （自 extensions/goal.rs 搬迁，原处已退役）。
#![allow(deprecated)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use agent::goal_runner::{
    build_continuation_prompt, GoalLifecycle, GoalMeta, GoalOutcome, HistoryEntry,
};
use agent::run::{build_react_config, run_agent_from_config, RunCmd, RunCompletion, RunOptions, RunParams};
use config::home::anureo_home;
use task_core::{CreateParams, TaskDb, TaskStatus};
use tokio_util::sync::CancellationToken;

use tool_core::active_operation::RunCancellation;
use crate::extensions::goal::GoalStatus;

pub(crate) use legacy_store::{
    register_runtime_goal, runtime_set_status, runtime_start, unregister_runtime_goal,
    RuntimeControl,
};

const MAX_ITERATIONS: u32 = 100;

/// Result of a goal run.
#[derive(Debug)]
pub struct GoalResult {
    pub task_id: String,
    pub outcome: GoalOutcome,
}

/// Errors that can occur during goal setup or execution.
#[derive(Debug, thiserror::Error)]
pub enum GoalRunError {
    #[error("goal init error: {0}")]
    Init(String),
    #[error("goal runner unavailable: {0}")]
    Unavailable(String),
}

/// Runs a goal loop inside the ACP process.
///
/// Each turn uses the normal agent runtime with the goal task MCP server
/// configured. The task ID is also used as the agent thread ID, so the agent's
/// checkpoint/session continuity survives across turns and is inspectable by
/// the existing task tools.
pub async fn run_goal(
    objective: String,
    working_dir: PathBuf,
    model_config: agent::run::ResolvedModelConfig,
    origin_session_id: Option<String>,
    cancel: CancellationToken,
    event_sender: Option<Arc<dyn Fn(agent::run::TypedAnyStreamEvent) + Send + Sync>>,
    run_cancellation: Option<RunCancellation>,
) -> Result<GoalResult, GoalRunError> {
    run_goal_for_task(
        None,
        None,
        objective,
        working_dir,
        model_config,
        origin_session_id,
        cancel,
        event_sender,
        run_cancellation,
        false,
        None,
    )
    .await
}

/// Resume a task-backed ACP goal without creating a second task.
///
/// P6 冻结：此入口原本由 `_anureo.dev/goal/resume` 调用；P5b 起扩展切至
/// `agent/goal` crate，本函数暂无调用方，保留供 legacy 路径审计（P7 随
/// 模块移除）。
#[allow(dead_code)]
pub(crate) async fn resume_goal(
    task_id: String,
    materialized_goal_id: String,
    objective: String,
    working_dir: PathBuf,
    model_config: agent::run::ResolvedModelConfig,
) -> Result<GoalResult, GoalRunError> {
    run_goal_for_task(
        Some(task_id),
        Some(materialized_goal_id),
        objective,
        working_dir,
        model_config,
        None,
        CancellationToken::new(),
        None,
        None,
        false,
        None,
    )
    .await
}

/// Resume an active task left behind by an ACP process restart. The persisted
/// task is still `in_progress` because no in-process runner survived the
/// restart; the recovery claim prevents duplicate runners in this process.
///
/// P6 冻结：goals.json 重启恢复预约机制已随 P5b 退役（恢复 = thread_goals
/// 天然持久 + continue_if_idle），此入口暂无调用方，保留供审计（P7 移除）。
#[allow(dead_code)]
pub(crate) async fn recover_goal(
    task_id: String,
    materialized_goal_id: String,
    objective: String,
    working_dir: PathBuf,
    model_config: agent::run::ResolvedModelConfig,
    runtime_control: RuntimeControl,
    event_sender: Option<Arc<dyn Fn(agent::run::TypedAnyStreamEvent) + Send + Sync>>,
) -> Result<GoalResult, GoalRunError> {
    let recovery_task_id = task_id.clone();
    let result = run_goal_for_task(
        Some(task_id),
        Some(materialized_goal_id),
        objective,
        working_dir,
        model_config,
        None,
        CancellationToken::new(),
        event_sender,
        None,
        true,
        Some(runtime_control),
    )
    .await;

    if result.is_err() {
        let db_path = anureo_home().join("tasks").join("tasks.db");
        if let Ok(db) = TaskDb::open(&db_path).await {
            let _ = db
                .atomic_update_status(
                    &recovery_task_id,
                    TaskStatus::InProgress,
                    TaskStatus::Pending,
                )
                .await;
        }
    }

    result
}

#[allow(clippy::too_many_arguments)]
async fn run_goal_for_task(
    existing_task_id: Option<String>,
    existing_materialized_goal_id: Option<String>,
    objective: String,
    working_dir: PathBuf,
    model_config: agent::run::ResolvedModelConfig,
    origin_session_id: Option<String>,
    cancel: CancellationToken,
    event_sender: Option<Arc<dyn Fn(agent::run::TypedAnyStreamEvent) + Send + Sync>>,
    run_cancellation: Option<RunCancellation>,
    allow_in_progress: bool,
    claimed_runtime_control: Option<RuntimeControl>,
) -> Result<GoalResult, GoalRunError> {
    let resuming = existing_task_id.is_some();
    let db_path = anureo_home().join("tasks").join("tasks.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| GoalRunError::Init(format!("create task DB directory: {e}")))?;
    }
    let db = Arc::new(
        TaskDb::open(&db_path)
            .await
            .map_err(|e| GoalRunError::Init(format!("open task DB: {e}")))?,
    );
    let task = if let Some(task_id) = existing_task_id {
        let resumed = db
            .atomic_update_status(&task_id, TaskStatus::Pending, TaskStatus::InProgress)
            .await
            .map_err(|e| GoalRunError::Init(format!("resume goal task: {e}")))?;
        if !(resumed
            || (allow_in_progress
                && db
                    .show_task(&task_id)
                    .await
                    .map(|task| task.status == TaskStatus::InProgress)
                    .unwrap_or(false)))
        {
            return Err(GoalRunError::Init(format!(
                "goal task '{task_id}' is not resumable"
            )));
        }
        db.show_task(&task_id)
            .await
            .map_err(|e| GoalRunError::Init(format!("read goal task: {e}")))?
    } else {
        db.create_task(&CreateParams {
            name: objective.clone(),
            description: objective.clone(),
            assignee: "acp".to_string(),
            start_time: None,
            status: TaskStatus::InProgress,
        })
        .await
        .map_err(|e| GoalRunError::Init(format!("create goal task: {e}")))?
    };
    let effective_cancellation = run_cancellation.unwrap_or_else(|| RunCancellation::new(0));

    // Finish all fallible checkpoint/config initialization before publishing
    // or registering a live runtime generation. On failure, restore a
    // resumable task state instead of leaking an in-process control.
    let initialization = async {
        let mcp_config_path = write_mcp_config(&db_path, &working_dir)
            .map_err(|e| GoalRunError::Init(format!("write goal MCP config: {e}")))?;
        let meta = if resuming {
            match db
                .get_meta(&task.id, "goal")
                .await
                .map_err(|e| GoalRunError::Init(format!("read goal metadata: {e}")))?
            {
                Some(value) => serde_json::from_value(value)
                    .map_err(|e| GoalRunError::Init(format!("decode goal metadata: {e}")))?,
                None => GoalMeta::default(),
            }
        } else {
            GoalMeta {
                tool: "anureo".to_string(),
                model: model_config.model.clone(),
                effort: model_config.effort.clone(),
                ..GoalMeta::default()
            }
        };
        let resolved = if resuming {
            agent::run::ResolvedModelConfig {
                model: meta.model.clone().or(model_config.model),
                effort: meta.effort.clone().or(model_config.effort),
                ..model_config
            }
        } else {
            model_config
        };
        Ok::<_, GoalRunError>((mcp_config_path, meta, resolved))
    }
    .await;
    let (mcp_config_path, mut meta, model_config) = match initialization {
        Ok(initialized) => initialized,
        Err(error) => {
            let rollback_status = if resuming {
                TaskStatus::Pending
            } else {
                TaskStatus::Cancelled
            };
            let _ = db
                .atomic_update_status(&task.id, TaskStatus::InProgress, rollback_status)
                .await;
            return Err(error);
        }
    };
    if resuming {
        meta.lifecycle = GoalLifecycle::Active;
        meta.lifecycle_reason = None;
    }
    let (materialized_goal_id, runtime_control) = match existing_materialized_goal_id {
        Some(id) => {
            let control = claimed_runtime_control.unwrap_or_else(|| register_runtime_goal(&id));
            (Some(id), Some(control))
        }
        None => match runtime_start(
            &working_dir,
            &objective,
            &objective,
            &task.id,
            origin_session_id.as_deref(),
            model_config.model.as_deref(),
            model_config.effort.as_deref(),
        ) {
            Ok((id, control)) => (Some(id), Some(control)),
            Err(error) => {
                let _ = db
                    .atomic_update_status(
                        &task.id,
                        TaskStatus::InProgress,
                        TaskStatus::Cancelled,
                    )
                    .await;
                return Err(GoalRunError::Init(format!(
                    "materialize ACP goal: {error}"
                )));
            }
        },
    };
    let tokens_used = Arc::new(Mutex::new(meta.tokens_used));
    let event_sender = event_sender.map(|forward| {
        let forward = forward.clone();
        let tokens_used = Arc::clone(&tokens_used);
        Arc::new(move |event: agent::run::TypedAnyStreamEvent| {
            if let agent::run::TypedAnyStreamEvent::React(
                stream_event::StreamEvent::TurnFinish { usage, .. },
            ) = &event
            {
                let mut total = tokens_used.lock().unwrap_or_else(|e| e.into_inner());
                *total = total.saturating_add(usage.input.saturating_add(usage.output));
            }
            forward(event);
        }) as Arc<dyn Fn(agent::run::TypedAnyStreamEvent) + Send + Sync>
    });

    let first_iteration = meta.iteration.saturating_add(1);
    for iteration in first_iteration..=MAX_ITERATIONS {
        if cancel.is_cancelled()
            || effective_cancellation.is_cancelled()
            || runtime_control
                .as_ref()
                .is_some_and(RuntimeControl::is_terminal)
        {
            let terminal = runtime_control
                .as_ref()
                .is_some_and(RuntimeControl::is_terminal);
            let _ = db
                .atomic_update_status(
                    &task.id,
                    TaskStatus::InProgress,
                    if terminal {
                        TaskStatus::Cancelled
                    } else {
                        TaskStatus::Pending
                    },
                )
                .await;
            save_meta(&db, &task.id, &meta).await;
            meta.lifecycle = if terminal {
                GoalLifecycle::Cancelled
            } else {
                GoalLifecycle::Paused
            };
            meta.lifecycle_reason = Some(if terminal {
                "cancelled by user".to_string()
            } else {
                "aborted by user".to_string()
            });
            save_meta(&db, &task.id, &meta).await;
            set_materialized_status(
                &working_dir,
                materialized_goal_id.as_deref(),
                if terminal {
                    GoalStatus::Cancelled
                } else {
                    GoalStatus::Paused
                },
            );
            unregister_materialized_goal(materialized_goal_id.as_deref(), runtime_control.as_ref());
            return Ok(GoalResult {
                task_id: task.id,
                outcome: GoalOutcome::Error(if terminal {
                    "cancelled by user".to_string()
                } else {
                    "aborted by user".to_string()
                }),
            });
        }

        meta.iteration = iteration;
        meta.tokens_used = *tokens_used.lock().unwrap_or_else(|e| e.into_inner());
        meta.history.push(HistoryEntry {
            iteration,
            timestamp: chrono::Utc::now().to_rfc3339(),
            summary: None,
        });
        if meta.history.len() > agent::goal_runner::MAX_HISTORY_ENTRIES {
            let keep_from = meta.history.len() - agent::goal_runner::MAX_HISTORY_ENTRIES;
            meta.history = meta.history.split_off(keep_from);
        }
        save_meta(&db, &task.id, &meta).await;

        let history_summary = if meta.history.len() > 1 {
            Some(format!(
                "Previous iterations:\n{}",
                meta.history
                    .iter()
                    .take(meta.history.len() - 1)
                    .map(|entry| format!("  iter {}: completed", entry.iteration))
                    .collect::<Vec<_>>()
                    .join("\n")
            ))
        } else {
            None
        };
        let budget_warning: Option<String> = None;
        let prompt = build_continuation_prompt(
            &task.id,
            &objective,
            0,
            0,
            None,
            &history_summary,
            &budget_warning,
            None,
        );
        let opts = RunOptions {
            message: anureo_llm::message::UserContent::Text(prompt),
            working_folder: Some(working_dir.clone()),
            session_id: Some(format!("goal-{}", &task.id[..task.id.floor_char_boundary(8)])),
            cancellation: Some(effective_cancellation.clone()),
            thread_id: Some(task.id.clone()),
            agent: None,
            verbose: false,
            verbose_level: 0,
            got_adaptive: false,
            display_max_len: 10000,
            output_json: false,
            model: model_config.model.clone(),
            mcp_config_path: Some(mcp_config_path.clone()),
            output_timestamp: false,
            dry_run: false,
            debug_llm: false,
            provider: model_config.provider.clone(),
            base_url: model_config.base_url.clone(),
            api_key: model_config.api_key.clone(),
            provider_type: model_config.provider_type.clone(),
            any_stream_event_sender: event_sender.clone(),
            bash_executor: None,
            extra_tools: None,
            default_extra_tools_provider: Some(tool_workflow::default_workflow_tool_provider()),
            acp_session_id: None,
            force_compact: false,
            chat_id: None,
            worktree: false,
            goal_mode: true,
            acp_mcp_servers: None,
            acp_mcp_sources: None,
            effort: model_config.effort.clone(),
            tier: model_config.tier.clone(),
        };
        let (config, _, _) = build_react_config(&opts);
        let params = RunParams {
            message: opts.message.clone(),
            verbose: false,
            cancellation: opts.cancellation.clone(),
            any_stream_event_sender: opts.any_stream_event_sender.clone(),
            llm_override: None,
            thread_id: opts.thread_id.clone(),
        };

        let run_future = run_agent_from_config(&config, &RunCmd::React, params, None);
        let run_result = if let Some(control) = runtime_control.as_ref() {
            tokio::select! {
                result = run_future => result,
                _ = control.token.cancelled() => {
                    effective_cancellation.cancel();
                    Ok(RunCompletion::Cancelled)
                }
            }
        } else {
            run_future.await
        };
        match run_result {
            Ok(RunCompletion::Finished(_)) => {
                meta.tokens_used = *tokens_used.lock().unwrap_or_else(|e| e.into_inner());
                save_meta(&db, &task.id, &meta).await;
            }
            Ok(RunCompletion::Cancelled) => {
                meta.tokens_used = *tokens_used.lock().unwrap_or_else(|e| e.into_inner());
                let terminal = runtime_control
                    .as_ref()
                    .is_some_and(RuntimeControl::is_terminal);
                meta.lifecycle = if terminal {
                    GoalLifecycle::Cancelled
                } else {
                    GoalLifecycle::Paused
                };
                meta.lifecycle_reason = Some(if terminal {
                    "cancelled by user".to_string()
                } else {
                    "aborted by user".to_string()
                });
                save_meta(&db, &task.id, &meta).await;
                let _ = db
                    .atomic_update_status(
                        &task.id,
                        TaskStatus::InProgress,
                        if terminal {
                            TaskStatus::Cancelled
                        } else {
                            TaskStatus::Pending
                        },
                    )
                    .await;
                set_materialized_status(
                    &working_dir,
                    materialized_goal_id.as_deref(),
                    if terminal {
                        GoalStatus::Cancelled
                    } else {
                        GoalStatus::Paused
                    },
                );
                unregister_materialized_goal(materialized_goal_id.as_deref(), runtime_control.as_ref());
                return Ok(GoalResult {
                    task_id: task.id,
                    outcome: GoalOutcome::Error(if terminal {
                        "cancelled by user".to_string()
                    } else {
                        "aborted by user".to_string()
                    }),
                });
            }
            Err(e) => {
                meta.tokens_used = *tokens_used.lock().unwrap_or_else(|e| e.into_inner());
                // A failed turn leaves the task pending and can be retried from
                // its checkpoint. Keep the materialized goal resumable too;
                // `failed` is reserved for an unrecoverable lifecycle failure.
                meta.lifecycle = GoalLifecycle::Paused;
                meta.lifecycle_reason = Some(format!("goal turn failed; retryable: {e}"));
                save_meta(&db, &task.id, &meta).await;
                let _ = db
                    .atomic_update_status(&task.id, TaskStatus::InProgress, TaskStatus::Pending)
                    .await;
                set_materialized_status(
                    &working_dir,
                    materialized_goal_id.as_deref(),
                    GoalStatus::Paused,
                );
                unregister_materialized_goal(materialized_goal_id.as_deref(), runtime_control.as_ref());
                return Err(GoalRunError::Unavailable(format!("goal turn failed: {e}")));
            }
        }

        let current = db
            .show_task(&task.id)
            .await
            .map_err(|e| GoalRunError::Init(format!("read goal task: {e}")))?;
        if current.status == TaskStatus::Completed {
            meta.lifecycle = GoalLifecycle::Completed;
            meta.lifecycle_reason = Some("task marked completed".to_string());
            save_meta(&db, &task.id, &meta).await;
            set_materialized_status(
                &working_dir,
                materialized_goal_id.as_deref(),
                GoalStatus::Completed,
            );
            unregister_materialized_goal(materialized_goal_id.as_deref(), runtime_control.as_ref());
            return Ok(GoalResult {
                task_id: task.id,
                outcome: GoalOutcome::Achieved,
            });
        }
    }

    let _ = db
        .atomic_update_status(&task.id, TaskStatus::InProgress, TaskStatus::Cancelled)
        .await;
    meta.lifecycle = GoalLifecycle::Blocked;
    meta.lifecycle_reason = Some(format!("max iterations ({MAX_ITERATIONS}) reached"));
    save_meta(&db, &task.id, &meta).await;
    set_materialized_status(
        &working_dir,
        materialized_goal_id.as_deref(),
        GoalStatus::Failed,
    );
    unregister_materialized_goal(materialized_goal_id.as_deref(), runtime_control.as_ref());
    Ok(GoalResult {
        task_id: task.id,
        outcome: GoalOutcome::Blocked(format!("max iterations ({MAX_ITERATIONS}) reached")),
    })
}

fn set_materialized_status(
    working_directory: &std::path::Path,
    goal_id: Option<&str>,
    status: GoalStatus,
) {
    if let Some(id) = goal_id {
        let _ = runtime_set_status(working_directory, id, status);
    }
}

fn unregister_materialized_goal(
    goal_id: Option<&str>,
    control: Option<&RuntimeControl>,
) {
    if let (Some(id), Some(control)) = (goal_id, control) {
        unregister_runtime_goal(id, control);
    }
}

async fn save_meta(db: &TaskDb, id: &str, meta: &GoalMeta) {
    if let Ok(value) = serde_json::to_value(meta) {
        let _ = db.set_meta(id, "goal", &value).await;
    }
}

fn write_mcp_config(db_path: &std::path::Path, working_dir: &std::path::Path) -> std::io::Result<PathBuf> {
    let path = working_dir.join(".anureo").join("goal-mcp.json");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let db_path = db_path.to_string_lossy().replace('\\', "\\\\");
    std::fs::write(
        &path,
        format!(r#"{{"mcpServers":{{"task":{{"command":"task-mcp-server","args":["--db-path","{}"]}}}}}}"#, db_path),
    )?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_goal_result_fields() {
        let result = GoalResult {
            task_id: "test-id-123".to_string(),
            outcome: GoalOutcome::Achieved,
        };
        assert_eq!(result.task_id, "test-id-123");
        assert!(matches!(result.outcome, GoalOutcome::Achieved));
    }

    #[tokio::test]
    async fn test_goal_run_error_display() {
        let err = GoalRunError::Init("test error".to_string());
        assert_eq!(format!("{}", err), "goal init error: test error");
    }
}

// ---------------------------------------------------------------------------
// Legacy goals.json store + RuntimeControl（P6 冻结；自 extensions/goal.rs
// 原样搬迁，仅供本模块 detached runner 使用；随 P7 一并移除。
// ---------------------------------------------------------------------------

mod legacy_store {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use tokio_util::sync::CancellationToken;

    use crate::client_capabilities::ClientCapabilitiesInfo;
    use config::home::anureo_home;
    use crate::extensions::{ExtensionContext, ExtensionError};
    use crate::extensions::goal::{Goal as LegacyGoalJson, GoalStatus};

    // The store is a JSON snapshot rather than a database transaction. Serialize
    // every mutation in this process so concurrent ACP requests cannot both read
    // the same snapshot and then lose one another's update. The final rename is
    // still atomic, so readers never observe a partially-written document.
    static GOAL_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    static RUNTIME_CONTROLS: OnceLock<Mutex<HashMap<String, RuntimeControl>>> = OnceLock::new();

    #[derive(Clone)]
    pub(crate) struct RuntimeControl {
        pub(crate) token: CancellationToken,
        terminal: Arc<AtomicBool>,
        instance_id: String,
    }

    impl RuntimeControl {
        pub(crate) fn is_terminal(&self) -> bool {
            self.terminal.load(Ordering::Acquire)
        }
    }

    fn lock_goal_store() -> std::sync::MutexGuard<'static, ()> {
        GOAL_STORE_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
    struct GoalFileStore {
        #[serde(default)]
        goals: Vec<LegacyGoalJson>,
    }

    fn goals_file_path(ctx: &ExtensionContext) -> PathBuf {
        if let Some(wd) = &ctx.working_directory {
            wd.join(".anureo").join("goals.json")
        } else {
            anureo_home().join("goals.json")
        }
    }

    fn load_store(ctx: &ExtensionContext) -> Result<GoalFileStore, ExtensionError> {
        let path = goals_file_path(ctx);
        match std::fs::read_to_string(&path) {
            Ok(contents) => {
                if contents.trim().is_empty() {
                    return Ok(GoalFileStore::default());
                }
                serde_json::from_str::<GoalFileStore>(&contents).map_err(|e| ExtensionError {
                    code: -32603,
                    message: "internal_error".into(),
                    data: Some(serde_json::Value::String(format!(
                        "failed to parse goals store at {}: {e}",
                        path.display()
                    ))),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GoalFileStore::default()),
            Err(e) => Err(ExtensionError {
                code: -32603,
                message: "internal_error".into(),
                data: Some(serde_json::Value::String(format!(
                    "failed to read goals store at {}: {e}",
                    path.display()
                ))),
            }),
        }
    }

    fn save_store(ctx: &ExtensionContext, store: &GoalFileStore) -> Result<(), ExtensionError> {
        let path = goals_file_path(ctx);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| ExtensionError {
                code: -32603,
                message: "internal_error".into(),
                data: Some(serde_json::Value::String(format!(
                    "failed to create directory {}: {e}",
                    parent.display()
                ))),
            })?;
        }
        let json = serde_json::to_string_pretty(store).map_err(|e| ExtensionError {
            code: -32603,
            message: "internal_error".into(),
            data: Some(serde_json::Value::String(format!(
                "failed to serialize goals store: {e}"
            ))),
        })?;
        // A unique temporary path prevents unrelated writers/processes from
        // clobbering one another's staging file before rename.
        let tmp = path.with_extension(format!("json.tmp-{}", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, &json).map_err(|e| ExtensionError {
            code: -32603,
            message: "internal_error".into(),
            data: Some(serde_json::Value::String(format!(
                "failed to write goals store at {}: {e}",
                tmp.display()
            ))),
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| ExtensionError {
            code: -32603,
            message: "internal_error".into(),
            data: Some(serde_json::Value::String(format!(
                "failed to rename goals store: {e}"
            ))),
        })
    }

    fn now_iso() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    fn generate_goal_id() -> String {
        format!("goal-{}", uuid::Uuid::new_v4())
    }

    /// Materialize a goal started by the ACP `/goal` command in the same view
    /// used by `_anureo.dev/goal/*`. The task DB remains the execution source of
    /// truth; this JSON record makes the running goal discoverable after a client
    /// reconnects.
    pub(crate) fn runtime_start(
        working_directory: &std::path::Path,
        title: &str,
        description: &str,
        task_id: &str,
        session_id: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
    ) -> Result<(String, RuntimeControl), String> {
        let ctx = runtime_context(working_directory, session_id);
        let _store_guard = lock_goal_store();
        let mut store = load_store(&ctx).map_err(|e| e.to_string())?;
        let now = now_iso();
        let id = generate_goal_id();
        // Register before publishing the active JSON record. A concurrent
        // goal/list recovery scan will then observe a live generation and skip it.
        let control = register_runtime_goal(&id);
        store.goals.push(LegacyGoalJson {
            id: id.clone(),
            title: title.to_string(),
            description: description.to_string(),
            status: GoalStatus::Active,
            created_at: now.clone(),
            updated_at: now,
            session_ids: session_id.into_iter().map(str::to_string).collect(),
            progress: None,
            metadata: Some(serde_json::json!({
                "source": "acp_goal_runner",
                "taskId": task_id,
                "model": model,
                "effort": effort,
            })),
            steps: Vec::new(),
            idempotency_key: None,
            working_directory: Some(working_directory.to_string_lossy().to_string()),
        });
        if let Err(error) = save_store(&ctx, &store) {
            unregister_runtime_goal(&id, &control);
            return Err(error.to_string());
        }
        Ok((id, control))
    }

    pub(crate) fn runtime_set_status(
        working_directory: &std::path::Path,
        id: &str,
        status: GoalStatus,
    ) -> Result<(), String> {
        let ctx = runtime_context(working_directory, None);
        let _store_guard = lock_goal_store();
        let mut store = load_store(&ctx).map_err(|e| e.to_string())?;
        let goal = store
            .goals
            .iter_mut()
            .find(|g| g.id == id)
            .ok_or_else(|| format!("goal '{id}' not found"))?;
        goal.status = status;
        goal.updated_at = now_iso();
        save_store(&ctx, &store).map_err(|e| e.to_string())
    }

    pub(crate) fn register_runtime_goal(id: &str) -> RuntimeControl {
        let control = new_runtime_control();
        RUNTIME_CONTROLS
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string(), control.clone());
        control
    }

    fn new_runtime_control() -> RuntimeControl {
        RuntimeControl {
            token: CancellationToken::new(),
            terminal: Arc::new(AtomicBool::new(false)),
            instance_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub(crate) fn unregister_runtime_goal(id: &str, control: &RuntimeControl) {
        if let Some(controls) = RUNTIME_CONTROLS.get() {
            let mut controls = controls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if controls
                .get(id)
                .is_some_and(|current| current.instance_id == control.instance_id)
            {
                controls.remove(id);
            }
        }
    }

    fn runtime_context(
        working_directory: &std::path::Path,
        session_id: Option<&str>,
    ) -> ExtensionContext {
        ExtensionContext {
            session_id: session_id.map(str::to_string),
            principal: "acp-goal-runner".to_string(),
            connection_id: "acp-goal-runner".to_string(),
            working_directory: Some(working_directory.to_path_buf()),
            client_capabilities: ClientCapabilitiesInfo::default(),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn ctx_in(dir: &std::path::Path) -> ExtensionContext {
            runtime_context(dir, Some("s-legacy"))
        }

        #[test]
        fn legacy_store_roundtrip() {
            let dir = tempfile::tempdir().unwrap();
            let ctx = ctx_in(dir.path());
            let (id, _control) =
                runtime_start(dir.path(), "T", "D", "task-1", Some("s-legacy"), None, None)
                    .unwrap();
            let store = load_store(&ctx).unwrap();
            assert_eq!(store.goals.len(), 1);
            assert_eq!(store.goals[0].id, id);
            assert_eq!(store.goals[0].status, GoalStatus::Active);
            assert_eq!(
                store.goals[0].metadata.as_ref().unwrap()["taskId"],
                "task-1"
            );

            runtime_set_status(dir.path(), &id, GoalStatus::Paused).unwrap();
            let store = load_store(&ctx).unwrap();
            assert_eq!(store.goals[0].status, GoalStatus::Paused);
        }

        #[test]
        fn runtime_control_register_unregister() {
            let control = register_runtime_goal("legacy-ctl-test");
            assert!(!control.is_terminal());
            unregister_runtime_goal("legacy-ctl-test", &control);
        }
    }
}

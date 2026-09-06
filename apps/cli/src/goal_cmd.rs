//! **goal-codex-alignment P6 冻结**：`anureo goal start/--resume` 是旧
//! detached goal runner 入口，已不在关键路径（新路径 = `/goal` 六子命令与
//! `_anureo.dev/goal/*`，后端 `agent/goal` crate）。保留供迁移审计与回退，
//! 移除见 P7。新迁移入口：`anureo goal --migrate`。

use std::sync::Arc;

use crate::args::GoalArgs;
use crate::goal_runner::{resume, write_mcp_config, AnureoTool, GoalRunner, ShellTool};
use agent::goal_runner::GoalOutcome;
use task_core::TaskDb;
use tokio_util::sync::CancellationToken;
use tool_core::active_operation::RunCancellation;

pub(crate) async fn handle_goal_command(ga: &GoalArgs) -> Result<(), Box<dyn std::error::Error>> {
    // P6：一次性迁移到 thread_goals（幂等，先备份）。优先于 legacy 路径。
    if ga.migrate {
        return crate::goal_migrate::run_migrate().await;
    }

    if ga.verbose {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("anureo=info")
            .with_writer(std::io::stderr)
            .try_init();
    }

    let working_dir = std::env::current_dir()?;
    let db_path = crate::task_db::ensure_task_db()?;
    let db = Arc::new(TaskDb::open(&db_path).await?);
    let cancel = CancellationToken::new();
    let run_cancellation = RunCancellation::new(0);

    let cancel_clone = cancel.clone();
    let rc_clone = run_cancellation.clone();
    ctrlc::set_handler(move || {
        cancel_clone.cancel();
        rc_clone.cancel();
    })?;

    if let Some(ref id) = ga.resume {
        eprintln!("resuming goal {}...", id);
        let mut runner = resume(id, working_dir, db, cancel, Some(run_cancellation)).await?;
        print_task_id(runner.task_id());
        let outcome = runner.run().await;
        print_outcome(&outcome);
        if matches!(outcome, GoalOutcome::Error(_) | GoalOutcome::Blocked(_)) {
            std::process::exit(1);
        }
        return Ok(());
    }

    let description = match &ga.description {
        Some(d) => d.clone(),
        None => {
            eprintln!("anureo goal: provide a goal description or use --resume <ID>");
            std::process::exit(1);
        }
    };

    // Create task first to get task_id for session_id
    let create_params = task_core::CreateParams {
            name: description.clone(),
            description: description.clone(),
            status: task_core::TaskStatus::InProgress,
            ..Default::default()
        };
    let task = match ga.id.as_deref().map(str::trim) {
        Some(id) if !id.is_empty() => db
            .create_task_with_id(id, &create_params)
            .await
            .map_err(|e| format!("failed to create task '{}': {}", id, e))?,
        Some(_) => return Err("--id must not be empty".into()),
        None => db
            .create_task(&create_params)
            .await
            .map_err(|e| format!("failed to create task: {}", e))?,
    };

    let task_id_short = task.id[..task.id.floor_char_boundary(8)].to_string();
    let session_id = format!("goal-{}", &task_id_short);

    let tool: Box<dyn crate::goal_runner::CodingTool> = match ga.tool.as_str() {
        "anureo" => {
            let mcp_config_path = write_mcp_config(&db_path, &working_dir)?;
            let mut anureo_tool =
                AnureoTool::new(session_id.clone(), working_dir.clone(), mcp_config_path)
                    .with_cancellation(run_cancellation.clone());
            if let Some(ref model) = ga.model {
                anureo_tool = anureo_tool.with_model(model.clone());
            }
            if let Some(ref effort) = ga.effort {
                anureo_tool = anureo_tool.with_effort(effort.clone());
            }
            Box::new(anureo_tool)
        }
        name => {
            let args = crate::goal_runner::shell_tool_args(name);
            Box::new(ShellTool::new(name.to_string(), args).with_cancel(cancel.clone()))
        }
    };

    let mut runner =
        GoalRunner::from_task(task.id, description, working_dir, db, tool, cancel).await?;
    runner = runner.with_model_config(ga.model.clone(), ga.effort.clone());
    if let Some(budget) = ga.token_budget {
        runner = runner.with_token_budget(budget);
    }
    if let Some(ref verify) = ga.verify {
        runner = runner.with_verify_command(verify.clone());
    }
    runner.persist_initial_state().await?;
    print_task_id(runner.task_id());
    let outcome = runner.run().await;
    print_outcome(&outcome);
    if matches!(outcome, GoalOutcome::Error(_) | GoalOutcome::Blocked(_)) {
        std::process::exit(1);
    }
    Ok(())
}

fn print_task_id(task_id: &str) {
    let end = task_id.floor_char_boundary(8);
    eprintln!("task_id: {}", &task_id[..end]);
}

fn print_outcome(outcome: &GoalOutcome) {
    match outcome {
        GoalOutcome::Error(e) => eprintln!("goal failed: {}", e),
        GoalOutcome::Blocked(reason) => eprintln!("goal blocked: {}", reason),
        GoalOutcome::UsageLimited {
            tokens_used,
            token_budget,
        } => {
            eprintln!(
                "goal stopped: token budget exhausted ({}/{})",
                tokens_used, token_budget
            );
        }
        GoalOutcome::Achieved => eprintln!("goal achieved"),
    }
}

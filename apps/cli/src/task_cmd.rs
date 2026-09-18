use std::sync::Arc;

use task_core::{CreateParams, ListParams, TaskDb, TaskStatus};
use tokio::sync::Notify;

use crate::args::{TaskArgs, TaskCommand};
use crate::display_limits::{generate_session_id, max_message_len};
use crate::output::{emit_run_output, make_stream_out, EventSink, OutputConfig};
use crate::repl::{run_one_turn, run_repl_loop};
use cli::RunOptions;
use anureo_llm::message::UserContent;
use tool_core::active_operation::RunCancellation;

use crate::args::Command;

pub(crate) async fn handle_task_command(ta: &TaskArgs) -> Result<(), Box<dyn std::error::Error>> {
    let db_path = crate::task_db::ensure_task_db()?;
    let db = Arc::new(TaskDb::open(&db_path).await?);

    match &ta.command {
        TaskCommand::New {
            description,
            agent,
            model,
        } => {
            let desc = description.join(" ");
            if desc.trim().is_empty() {
                eprintln!("anureo task new: provide a task description");
                std::process::exit(1);
            }

            let name = truncate_name(&desc);
            let task = db
                .create_task(&CreateParams {
                    name,
                    description: desc.clone(),
                    assignee: agent.clone(),
                    start_time: None,
                    status: TaskStatus::InProgress,
                })
                .await?;

            let task_id_short = &task.id[..8.min(task.id.len())];
            eprintln!("task_id: {}", task_id_short);
            eprintln!("status: in_progress");
            eprintln!("assignee: {}", agent);
            eprintln!();

            let working_dir = std::env::current_dir()?;
            let message = format!(
                "??????????,??????\n\n? Task ID: {}\n??:{}\n\n?? Task-Driven Workflow ???",
                task.id, desc
            );

            let run_cancellation = RunCancellation::new(0);
            let rc_clone = run_cancellation.clone();
            let last_ctrlc = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
            let lc_clone = last_ctrlc.clone();
            let force_quit = Arc::new(Notify::new());
            let fq_clone = force_quit.clone();
            ctrlc::set_handler(move || {
                rc_clone.cancel();
                let now = std::time::Instant::now();
                let is_double_press = {
                    let mut guard = lc_clone.lock().unwrap();
                    let prev = guard.replace(now);
                    prev.map(|p| now.duration_since(p) < std::time::Duration::from_secs(2))
                        .unwrap_or(false)
                };
                if is_double_press {
                    fq_clone.notify_one();
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    std::process::exit(130);
                }
            })?;

            let mut opts = RunOptions {
                message: UserContent::Text(message),
                working_folder: Some(working_dir),
                session_id: None,
                cancellation: Some(run_cancellation),
                thread_id: Some(generate_session_id()),
                agent: Some(agent.clone()),
                verbose: false,
                verbose_level: 0,
                got_adaptive: false,
                display_max_len: max_message_len(),
                output_json: false,
                model: model.clone(),
                mcp_config_path: None,
                output_timestamp: false,
                dry_run: false,
                provider: None,
                base_url: None,
                api_key: None,
                provider_type: None,
                any_stream_event_sender: None,
                bash_executor: None,
                extra_tools: None,
                default_extra_tools_provider: Some(cli::run::default_workflow_tool_provider()),
                acp_session_id: None,
                async_agent_registry: None,
                subagent_sink: None,
                force_compact: false,
                chat_id: None,
                worktree: false,
                goal_mode: false,
                acp_mcp_servers: None,

                acp_mcp_sources: None,
                debug_llm: false,
                effort: None,
                tier: None,
            };

            let output = OutputConfig {
                json: false,
                pretty: false,
                file: None,
            };
            let stream_out = make_stream_out(&output);
            let reply_len = max_message_len();

            let initial_message = Some(format!("Task {} ???", task_id_short));
            run_interactive_mode(
                &mut opts,
                &Command::React,
                initial_message,
                reply_len,
                &output,
                stream_out,
                force_quit,
            )
            .await?;
        }

        TaskCommand::List { status, assignee } => {
            let status_filter = status
                .as_deref()
                .and_then(|s| task_core::parse_status(s).ok());
            let list = db
                .list_tasks(&ListParams {
                    status: status_filter,
                    assignee: assignee.clone(),
                    name: None,
                    sort_by: "created_at".to_string(),
                    sort_order: "desc".to_string(),
                    limit: 50,
                    page: 1,
                })
                .await?;

            if list.tasks.is_empty() {
                println!("No tasks found.");
                return Ok(());
            }

            println!("{:<10} {:<12} {:<20} NAME", "ID", "STATUS", "ASSIGNEE");
            println!("{}", "-".repeat(70));
            for t in &list.tasks {
                let id_short = &t.id[..8.min(t.id.len())];
                println!(
                    "{:<10} {:<12} {:<20} {}",
                    id_short, t.status, t.assignee, t.name
                );
            }
            println!("\nTotal: {} tasks", list.total);
        }

        TaskCommand::Show { id } => {
            let task = db
                .show_task(id)
                .await
                .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;
            println!("ID:          {}", task.id);
            println!("Name:        {}", task.name);
            println!("Status:      {}", task.status);
            println!("Assignee:    {}", task.assignee);
            println!("Start Time:  {}", task.start_time);
            println!("Created At:  {}", task.created_at);
            println!("Description:\n{}", task.description);
        }

        TaskCommand::Continue { id, agent } => {
            let task = db
                .show_task(id)
                .await
                .map_err(|e| Box::<dyn std::error::Error>::from(e.to_string()))?;

            let task_id_short = &task.id[..8.min(task.id.len())];
            eprintln!("resuming task {} ...", task_id_short);

            let working_dir = std::env::current_dir()?;
            let message = format!(
                "??????????\n\n? Task ID: {}\n??:{}\n??:{}\n\n????????,?????",
                task.id, task.description, task.status
            );

            let run_cancellation = RunCancellation::new(0);
            let rc_clone = run_cancellation.clone();
            let last_ctrlc = Arc::new(std::sync::Mutex::new(None::<std::time::Instant>));
            let lc_clone = last_ctrlc.clone();
            let force_quit = Arc::new(Notify::new());
            let fq_clone = force_quit.clone();
            ctrlc::set_handler(move || {
                rc_clone.cancel();
                let now = std::time::Instant::now();
                let is_double_press = {
                    let mut guard = lc_clone.lock().unwrap();
                    let prev = guard.replace(now);
                    prev.map(|p| now.duration_since(p) < std::time::Duration::from_secs(2))
                        .unwrap_or(false)
                };
                if is_double_press {
                    fq_clone.notify_one();
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    std::process::exit(130);
                }
            })?;

            let mut opts = RunOptions {
                message: UserContent::Text(message),
                working_folder: Some(working_dir),
                session_id: None,
                cancellation: Some(run_cancellation),
                thread_id: Some(generate_session_id()),
                agent: Some(agent.clone()),
                verbose: false,
                verbose_level: 0,
                got_adaptive: false,
                display_max_len: max_message_len(),
                output_json: false,
                model: None,
                mcp_config_path: None,
                output_timestamp: false,
                dry_run: false,
                provider: None,
                base_url: None,
                api_key: None,
                provider_type: None,
                any_stream_event_sender: None,
                bash_executor: None,
                extra_tools: None,
                default_extra_tools_provider: Some(cli::run::default_workflow_tool_provider()),
                acp_session_id: None,
                async_agent_registry: None,
                subagent_sink: None,
                force_compact: false,
                chat_id: None,
                worktree: false,
                goal_mode: false,
                acp_mcp_servers: None,

                acp_mcp_sources: None,
                debug_llm: false,
                effort: None,
                tier: None,
            };

            let output = OutputConfig {
                json: false,
                pretty: false,
                file: None,
            };
            let stream_out = make_stream_out(&output);
            let reply_len = max_message_len();

            let initial_message = Some(format!("Task {} ???", task_id_short));
            run_interactive_mode(
                &mut opts,
                &Command::React,
                initial_message,
                reply_len,
                &output,
                stream_out,
                force_quit,
            )
            .await?;
        }
    }

    Ok(())
}

fn truncate_name(desc: &str) -> String {
    let line = desc.lines().next().unwrap_or(desc);
    if line.len() <= 60 {
        line.to_string()
    } else {
        let cut = line.floor_char_boundary(57);
        format!("{}...", &line[..cut])
    }
}

async fn run_interactive_mode(
    opts: &mut RunOptions,
    cmd: &Command,
    initial_message: Option<String>,
    reply_len: usize,
    output: &OutputConfig,
    stream_out: EventSink,
    force_quit: Arc<Notify>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(session_id) = opts.thread_id.as_deref() {
        eprintln!("Session: {}", session_id);
    }

    let stream_clone = stream_out.clone();
    if let Some(msg) = initial_message.filter(|m| !m.trim().is_empty()) {
        eprintln!("{}", msg);
    }

    let message = opts.message.clone();
    match run_one_turn(opts, cmd, stream_out).await {
        Ok(output_value) => emit_run_output(
            output_value,
            output,
            opts.thread_id.as_deref(),
            reply_len,
            opts.output_timestamp,
        )?,
        Err(err) => {
            eprintln!("error: {}", err);
            std::process::exit(1);
        }
    }

    opts.message = message;
    run_repl_loop(
        opts,
        cmd,
        reply_len,
        output.clone(),
        stream_clone,
        force_quit,
    )
    .await?;
    println!("Bye.");
    Ok(())
}

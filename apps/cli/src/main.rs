//! anureo CLI binary: run agentic workflows from the command line.
//!
//! Subcommands: `react` (default), `dup`, `tot`, `got`, `tool` (list/show tools), `models` (list models), `mcp` (manage MCP servers).
//! Dispatch lives here; see `args`, `bootstrap`, `display_limits`, `run_flow`, and `subcommands` for implementation.

mod args;
mod bootstrap;
mod codex_event_builder;
mod config_cmd;
mod display_limits;
mod goal_cmd;
mod goal_migrate;
mod goal_repl;
mod goal_runner;
mod logging;
mod mcp_manager;
mod output;
mod repl;
mod review_cmd;
mod review_history;
mod review_skill_cmd;
mod run_flow;
mod session;
mod session_view;
mod skill_inspect;
mod skill_usage_cmd;
mod subcommands;
mod task_cmd;
mod task_db;

pub(crate) use args::Command;

use cli::display;

use std::sync::Arc;

use clap::Parser;
use tokio::sync::Notify;

use args::{Args, Command as Cmd, GotArgs};
use bootstrap::{init_logging, preserve_shell_env, print_config_report};
use config_cmd::handle_config_command;
use display_limits::max_reply_len;
use run_flow::{
    build_run_options, output_config, resolve_user_message, run_interactive_mode,
    run_single_turn_mode,
};
use skill_usage_cmd::handle_skill_usage_command;
use subcommands::{
    handle_agent_command, handle_curator_command, handle_mcp_command, handle_memory_command,
    handle_models_command, handle_session_command, handle_skills_command, handle_tool_command,
};
use tool_core::active_operation::RunCancellation;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Apply the home override before anything else reads anureo_home() —
    // including the ACP branch below, which runs before config reporting.
    if let Some(home) = &args.home {
        config::home::set_override(Some(home.clone()));
    }

    // Validate tier argument before proceeding
    if let Err(e) = run_flow::validate_tier_arg(&args.tier) {
        eprintln!("anureo: {}", e);
        std::process::exit(1);
    }

    // Check for model/tier conflict
    if let Err(e) = run_flow::check_model_tier_conflict(&args) {
        eprintln!("anureo: {}", e);
        std::process::exit(1);
    }

    // ACP path: must branch before any stdout output (config report, logging).
    // The ACP protocol uses stdout for JSON-RPC; any extra output corrupts it.
    if let Some(Cmd::Acp(acp_args)) = &args.cmd {
        let log_config = anureo_acp::logging::LogConfig {
            level: args
                .log_level
                .clone()
                .unwrap_or_else(|| bootstrap::verbose_to_level(args.verbose).to_string()),
            file: args.log_file.clone(),
            rotate: config::tracing_init::LogRotate::from_str_or_daily(&args.log_rotate),
            format: args.log_format.parse().unwrap_or_default(),
        };
        anureo_acp::set_log_config(log_config);
        if let Err(e) =
            anureo_acp::ws_bridge::run_ws_bridge(acp_args.url.clone(), acp_args.pid_file.clone())
                .await
        {
            eprintln!("anureo acp: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    if args.acp {
        if args.cmd.is_some() {
            eprintln!(
                "anureo: --acp cannot be combined with a subcommand; pass the prompt as a message"
            );
            std::process::exit(2);
        }
        let message = resolve_user_message(&args);
        let cwd = args
            .working_folder
            .clone()
            .unwrap_or(std::env::current_dir()?);
        let options = anureo_acp::cli_client::CliAcpOptions {
            url: args.acp_url.clone(),
            cwd,
            session_id: args.session_id.clone(),
            message,
            json_output: args.json,
            pretty: args.pretty,
        };
        if let Err(error) = anureo_acp::cli_client::run(options).await {
            eprintln!("anureo --acp: {error}");
            std::process::exit(1);
        }
        return Ok(());
    }

    if let Some(Cmd::Server(server_options)) = &args.cmd {
        let log_config = anureo_server::logging::LogConfig {
            level: args
                .log_level
                .clone()
                .unwrap_or_else(|| bootstrap::verbose_to_level(args.verbose).to_string()),
            file: args.log_file.clone(),
            rotate: config::tracing_init::LogRotate::from_str_or_daily(&args.log_rotate),
            format: args.log_format.parse().unwrap_or_default(),
        };
        anureo_server::runtime::run(server_options.clone(), Some(log_config)).await?;
        return Ok(());
    }

    // Preserve shell environment variables BEFORE config.toml is loaded.
    // This allows us to distinguish between shell-set and config.toml-set LOG_FILE.
    let shell_env = preserve_shell_env();

    print_config_report(args.verbose);

    let _log_guard = init_logging(&args, shell_env);

    if let Some(Cmd::Session(sa)) = &args.cmd {
        handle_session_command(sa, args.json).await?;
        return Ok(());
    }
    if let Some(Cmd::Tool(ta)) = &args.cmd {
        if let Err(err) = handle_tool_command(&args, ta).await {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::Models(ma)) = &args.cmd {
        if let Err(err) = handle_models_command(&args, ma).await {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::Mcp(ma)) = &args.cmd {
        if let Err(err) = handle_mcp_command(ma, args.json) {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::Config(ca)) = &args.cmd {
        if let Err(err) = handle_config_command(ca, args.json) {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::Agent(aa)) = &args.cmd {
        if let Err(err) = handle_agent_command(aa) {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::Goal(ga)) = &args.cmd {
        goal_cmd::handle_goal_command(ga).await?;
        return Ok(());
    }
    if let Some(Cmd::Skills(sa)) = &args.cmd {
        if let Err(err) = handle_skills_command(sa, args.json, args.pretty, args.file.as_deref()) {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::SkillUsage(sa)) = &args.cmd {
        if let Err(err) = handle_skill_usage_command(sa).await {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::Evolve) = &args.cmd {
        eprintln!("evolve: not yet implemented (anureo-evolution crate removed)");
        std::process::exit(1);
    }
    if let Some(Cmd::Curator(ca)) = &args.cmd {
        if let Err(err) = handle_curator_command(ca, args.json).await {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::Memory(ma)) = &args.cmd {
        if let Err(err) = handle_memory_command(ma, args.json) {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::ReviewSkill(ra)) = &args.cmd {
        if let Err(err) = review_skill_cmd::handle_review_skill_command(ra).await {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::Review(ra)) = &args.cmd {
        if let Err(err) = review_cmd::handle_review_command(ra, args.json).await {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }
    if let Some(Cmd::Task(ta)) = &args.cmd {
        if let Err(err) = task_cmd::handle_task_command(ta).await {
            eprintln!("{}", err);
            std::process::exit(1);
        }
        return Ok(());
    }

    let message = resolve_user_message(&args);
    if !args.interactive && message.is_none() {
        eprintln!("anureo: provide a message via -m/--message or positional args");
        std::process::exit(1);
    }

    let cmd = args.cmd.clone().unwrap_or(Cmd::React);
    let got_adaptive = matches!(&cmd, Cmd::Got(GotArgs { got_adaptive: true }));

    // Set up Ctrl+C handling with double-press force-quit.
    // First press: cancel the running agent via CancellationToken.
    // Second press within 2s: force-exit the process (also triggers REPL exit in interactive mode).
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

    let mut opts = build_run_options(&args, message.clone().unwrap_or_default(), got_adaptive);
    opts.cancellation = Some(run_cancellation);
    let output = output_config(&args);
    let reply_len = max_reply_len();

    if args.interactive {
        run_interactive_mode(&mut opts, &cmd, message, reply_len, &output, force_quit).await?;
    } else {
        run_single_turn_mode(&mut opts, &cmd, reply_len, &output).await?;
    }

    Ok(())
}

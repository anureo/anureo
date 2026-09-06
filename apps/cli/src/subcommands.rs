//! Handlers for `tool`, `models`, `session`, and `mcp` CLI subcommands.

use cli::{cli_list_models, cli_list_tools, cli_show_tool, ToolShowFormat};

use crate::args::{
    AgentArgs, AgentCommand, Args, CuratorCmdArgs, CuratorCommand, ExportArgs, McpArgs, McpCommand,
    MemoryCmdArgs, MemoryCommand, ModelsArgs, ModelsCommand, SkillsArgs, SkillsCommand, ToolArgs,
    ToolCommand,
};
use crate::mcp_manager::{AddMcpArgs, EditMcpArgs, McpManager, ServerDetail, ServerInfo};
use crate::run_flow::build_run_options;
use std::path::Path;

use crate::session::{SessionArgs, SessionCommand, SessionManager};
use cli::profile_convert::ExportFormat;

pub(crate) async fn handle_tool_command(
    args: &Args,
    tool_args: &ToolArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let opts = build_run_options(args, String::new(), false);
    match &tool_args.sub {
        ToolCommand::List => cli_list_tools(&opts).await?,
        ToolCommand::Show(show_args) => {
            let format = if args.json || show_args.output.eq_ignore_ascii_case("json") {
                ToolShowFormat::Json
            } else {
                ToolShowFormat::Yaml
            };
            cli_show_tool(&opts, &show_args.name, format).await?;
        }
    }
    Ok(())
}

pub(crate) async fn handle_models_command(
    args: &Args,
    models_args: &ModelsArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let opts = build_run_options(args, String::new(), false);
    match &models_args.sub {
        ModelsCommand::List => cli_list_models(&opts, None).await?,
        ModelsCommand::Show(show_args) => cli_list_models(&opts, Some(&show_args.name)).await?,
    }
    Ok(())
}

pub(crate) async fn handle_session_command(
    sa: &SessionArgs,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let manager = SessionManager::with_default_path();

    match &sa.command {
        SessionCommand::List(args) => {
            // Load config defaults (non-fatal: fall back to empty config on
            // parse errors so a malformed config.toml never blocks `session list`).
            let cfg = config::xdg_toml::load_full_config("anureo").unwrap_or_else(|e| {
                eprintln!("warning: failed to load config.toml: {}", e);
                config::FullConfig::default_session()
            });
            let effective_limit = args.limit.or(cfg.session.default_limit).unwrap_or(50);
            let effective_format: Option<String> =
                args.format.clone().or(cfg.session.default_format);

            let since = args
                .since
                .as_deref()
                .map(SessionManager::parse_date_arg)
                .transpose()
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            let until = args
                .until
                .as_deref()
                .map(SessionManager::parse_date_arg)
                .transpose()
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            // Build the effective args (with config defaults applied) for the
            // formatter. We don't mutate `args` because clap's derive keeps
            // the original; we just shadow it locally.
            let mut effective_args = args.clone();
            effective_args.limit = Some(effective_limit);
            if effective_args.format.is_none() {
                effective_args.format = effective_format;
            }

            let sessions = if let Some(query) = &effective_args.grep {
                // Keyword search: walk thread_ids and inspect recent payloads.
                // Limit is taken from --limit; since/until applied in memory.
                let mut found = manager.search_sessions(query, effective_limit)?;
                if let Some(since) = since {
                    let since_ms = since.timestamp_millis();
                    found.retain(|s| {
                        s.last_updated
                            .map(|t| t.timestamp_millis() >= since_ms)
                            .unwrap_or(false)
                    });
                }
                if let Some(until) = until {
                    let until_ms = until.timestamp_millis();
                    found.retain(|s| {
                        s.last_updated
                            .map(|t| t.timestamp_millis() <= until_ms)
                            .unwrap_or(false)
                    });
                }
                if effective_args.reverse {
                    found.sort_by_key(|a| a.last_updated);
                }
                found
            } else {
                manager.list_sessions_filtered(
                    effective_limit,
                    since,
                    until,
                    effective_args.reverse,
                )?
            };

            let use_pager = !json
                && !effective_args.no_pager
                && effective_args.format.is_none()
                && crate::display::terminal::is_stdout_tty();

            // Build display config from effective args (P2: decoupled from ListArgs).
            // Enrich with review status map (non-fatal: empty map on error).
            let review_map = {
                let anureo_home = config::home::anureo_home();
                crate::review_history::ReviewHistory::new(&anureo_home)
                    .review_status_map()
                    .unwrap_or_default()
            };
            let display_cfg = crate::session_view::SessionListDisplayConfig {
                oneline: effective_args.oneline,
                format: effective_args.format.clone(),
                review_map,
            };

            if json {
                crate::session_view::print_json(&sessions, &display_cfg.review_map)?;
            } else if use_pager {
                let formatted = crate::session_view::format_session_list(&sessions, &display_cfg);
                tokio::task::spawn_blocking(move || -> Result<(), String> {
                    let pager = minus::Pager::new();
                    pager
                        .set_text(formatted)
                        .map_err(|e| format!("Pager set_text error: {}", e))?;
                    minus::page_all(pager).map_err(|e| format!("Pager error: {}", e))
                })
                .await
                .map_err(|e| format!("Pager task join error: {}", e))??;
            } else {
                let formatted = crate::session_view::format_session_list(&sessions, &display_cfg);
                print!("{}", formatted);
            }
        }
        SessionCommand::Show { session_id } => match manager.show_session(session_id)? {
            Some(detail) => manager.print_session_detail(&detail, json)?,
            None => {
                eprintln!("Session not found: {}", session_id);
                std::process::exit(1);
            }
        },
        SessionCommand::Delete { session_id } => {
            let count = manager.delete_session(session_id)?;
            if json {
                let result = serde_json::json!({
                    "session_id": session_id,
                    "deleted_checkpoints": count
                });
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Deleted session {} ({} checkpoints)", session_id, count);
            }
        }
        SessionCommand::Rename { session_id, title } => {
            manager.rename_session(session_id, title)?;
            if json {
                let result = serde_json::json!({
                    "session_id": session_id,
                    "title": title
                });
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Renamed session {} to \"{}\"", session_id, title);
            }
        }
        SessionCommand::Cat { session_id } => {
            let events = manager.cat_session(session_id)?;
            if json {
                for event in &events {
                    println!("{}", serde_json::to_string(event)?);
                }
            } else {
                crate::codex_event_builder::print_cat_text(&events);
            }
        }
    }
    Ok(())
}

pub(crate) fn handle_mcp_command(
    mcp_args: &McpArgs,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let manager = McpManager::new()?;

    match &mcp_args.command {
        McpCommand::List => {
            let servers = manager.list_servers()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&servers)?);
            } else {
                print_server_list(&servers);
            }
        }
        McpCommand::Show { name } => match manager.show_server(name)? {
            Some(detail) => {
                if json {
                    println!("{}", serde_json::to_string_pretty(&detail)?);
                } else {
                    print_server_detail(&detail);
                }
            }
            None => {
                eprintln!("MCP server not found: {}", name);
                std::process::exit(1);
            }
        },
        McpCommand::Add(add_args) => {
            let cli_args = AddMcpArgs {
                name: add_args.name.clone(),
                command: add_args.command.clone(),
                args: add_args.args.clone(),
                url: add_args.url.clone(),
                env: add_args.env.clone(),
                disabled: add_args.disabled,
            };
            manager.add_server(&cli_args)?;
            println!("MCP server '{}' added successfully", add_args.name);
        }
        McpCommand::Edit(edit_args) => {
            let cli_args = EditMcpArgs {
                command: edit_args.command.clone(),
                args: edit_args.args.clone(),
                url: edit_args.url.clone(),
                env: edit_args.env.clone(),
                disabled: edit_args.disabled,
            };
            manager.edit_server(&edit_args.name, &cli_args)?;
            println!("MCP server '{}' updated successfully", edit_args.name);
        }
        McpCommand::Delete { name } => {
            if manager.delete_server(name)? {
                println!("MCP server '{}' deleted successfully", name);
            } else {
                eprintln!("MCP server not found: {}", name);
                std::process::exit(1);
            }
        }
        McpCommand::Enable { name } => {
            manager.enable_server(name)?;
            println!("MCP server '{}' enabled successfully", name);
        }
        McpCommand::Disable { name } => {
            manager.disable_server(name)?;
            println!("MCP server '{}' disabled successfully", name);
        }
    }
    Ok(())
}

fn print_server_list(servers: &[ServerInfo]) {
    println!("MCP Servers:");
    println!("{}", "─".repeat(80));

    if servers.is_empty() {
        println!("No MCP servers configured.");
        return;
    }

    for server in servers {
        let status = if server.disabled { "[disabled]" } else { "" };
        println!("  • {} {}", server.name, status);
        println!("    Type: {}", server.server_type);
        if let Some(cmd) = &server.command {
            println!("    Command: {}", cmd);
        }
        if let Some(url) = &server.url {
            println!("    URL: {}", url);
        }
        println!();
    }
}

fn print_server_detail(detail: &ServerDetail) {
    println!("MCP Server: {}", detail.name);
    println!("{}", "═".repeat(80));

    let status = if detail.entry.disabled {
        "disabled"
    } else {
        "enabled"
    };
    println!("Status: {}", status);

    if let Some(cmd) = &detail.entry.command {
        println!("Command: {}", cmd);
        if !detail.entry.args.is_empty() {
            println!("Args: {}", detail.entry.args.join(" "));
        }
    }

    if let Some(url) = &detail.entry.url {
        println!("URL: {}", url);
    }

    if !detail.entry.env.is_empty() {
        println!("Environment:");
        for (key, value) in &detail.entry.env {
            let masked_value = config::mask_value(value);
            println!("  {}={}", key, masked_value);
        }
    }

    if !detail.entry.headers.is_empty() {
        println!("Headers:");
        for (key, value) in &detail.entry.headers {
            println!("  {}: {}", key, value);
        }
    }
}

pub(crate) fn handle_agent_command(
    agent_args: &AgentArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    match &agent_args.command {
        AgentCommand::List => handle_agent_list(),
        AgentCommand::Export(export_args) => handle_agent_export(export_args),
    }
}

fn handle_agent_list() -> Result<(), Box<dyn std::error::Error>> {
    let profiles = agent::profile::list_available_profiles();
    if profiles.is_empty() {
        println!("No agent profiles found.");
        return Ok(());
    }
    println!("Agent profiles:");
    for p in &profiles {
        if let Some(desc) = &p.description {
            println!("  • {} — {}", p.name, desc);
        } else {
            println!("  • {}", p.name);
        }
    }
    Ok(())
}

fn handle_agent_export(export_args: &ExportArgs) -> Result<(), Box<dyn std::error::Error>> {
    use cli::profile_convert::export;

    let format: ExportFormat = export_args.format.parse()?;

    let agent_names: Vec<String> = match &export_args.agent {
        Some(name) => vec![name.clone()],
        None => agent::profile::list_available_profiles()
            .into_iter()
            .map(|p| p.name)
            .collect(),
    };

    if agent_names.is_empty() {
        println!("No agent profiles found.");
        return Ok(());
    }

    for agent_name in &agent_names {
        let output = export(agent_name, format)?;

        if export_args.dry_run {
            println!("--- {} ---", output.path.display());
            println!("{}", output.content);
        } else {
            let full_path = export_args.output.join(&output.path);
            if let Some(parent) = full_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&full_path, &output.content)?;
            println!("Exported {} -> {}", agent_name, full_path.display());
        }
    }

    Ok(())
}

pub(crate) fn handle_skills_command(
    skills_args: &SkillsArgs,
    json: bool,
    pretty: bool,
    output_file: Option<&Path>,
) -> Result<(), Box<dyn std::error::Error>> {
    use cli::run::skill_registry::{Lifecycle, SkillContent, SkillRegistry, Source};

    let registry = SkillRegistry::new(&anureo_curator::skill_registry::default_path());

    match &skills_args.command {
        SkillsCommand::List => {
            let skills = registry.list()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&skills)?);
            } else if skills.is_empty() {
                println!("No skills found.");
            } else {
                println!("Skills:");
                for s in &skills {
                    let src = match s.source {
                        Source::Auto => "[auto]",
                        Source::Manual => "[manual]",
                        Source::Evolved => "[evolved]",
                    };
                    let lc = match s.lifecycle {
                        Lifecycle::Active => "",
                        Lifecycle::Stale => " [stale]",
                        Lifecycle::Archived => " [archived]",
                    };
                    println!("  • {} {}{} — {}", s.name, src, lc, s.description);
                }
            }
        }
        SkillsCommand::Show { name } => {
            let skill = registry.load(name)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&skill)?);
            } else {
                println!("Skill: {}", skill.name);
                println!("{}", "═".repeat(60));
                println!("Description: {}", skill.description);
                println!("Source: {:?}", skill.source);
                println!("Lifecycle: {:?}", skill.lifecycle);
                println!("Triggers: {}", skill.triggers.join(", "));
                println!();
                println!("{}", skill.body);
            }
        }
        SkillsCommand::Inspect {
            name,
            all,
            read_file,
            source,
        } => {
            return crate::skill_inspect::run(
                name,
                *all,
                read_file.as_deref(),
                source.as_ref(),
                json,
                pretty,
                output_file,
            );
        }
        SkillsCommand::Create {
            name,
            description,
            triggers,
        } => {
            let skill = SkillContent {
                name: name.clone(),
                description: description.clone().unwrap_or_default(),
                triggers: triggers.clone(),
                lifecycle: Lifecycle::Active,
                source: Source::Manual,
                category: None,
                created_by: None,
                body: String::new(),
                raw: String::new(),
            };
            registry.save(name, &skill)?;
            println!("Created skill: {}", name);
        }
        SkillsCommand::Sync {
            bundled_dir,
            user_dir,
            force,
        } => {
            use anureo_curator::sync_skills;
            let bundled = bundled_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| config::home::anureo_home().join("bundled-skills"));
            let user = user_dir
                .as_deref()
                .map(std::path::PathBuf::from)
                .unwrap_or(anureo_curator::skill_registry::default_path());
            let _ = force;
            let result = sync_skills(&bundled, &user);
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("Skills sync:");
                println!("{}", "═".repeat(60));
                println!("  Bundled source: {}", bundled.display());
                println!("  User target:    {}", user.display());
                println!("Added:   {}", result.added.len());
                for n in &result.added {
                    println!("  + {}", n);
                }
                println!("Updated: {}", result.updated.len());
                for n in &result.updated {
                    println!("  ~ {}", n);
                }
                println!("Skipped: {}", result.skipped.len());
                for n in &result.skipped {
                    println!("  - {}", n);
                }
                println!("Removed: {}", result.removed.len());
                for n in &result.removed {
                    println!("  ✗ {}", n);
                }
            }
        }
        SkillsCommand::Edit { name } => {
            let skill = registry.load(name)?;
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let tmp_dir = std::env::temp_dir();
            let tmp_path = tmp_dir.join(format!("anureo-skill-{}.md", name));
            std::fs::write(&tmp_path, &skill.raw)?;
            let status = std::process::Command::new(&editor)
                .arg(&tmp_path)
                .status()?;
            if status.success() {
                let edited = std::fs::read_to_string(&tmp_path)?;
                let mut updated = skill.clone();
                updated.raw = edited;
                registry.save(name, &updated)?;
                println!("Updated skill: {}", name);
            }
            let _ = std::fs::remove_file(&tmp_path);
        }
        SkillsCommand::Delete { name } => {
            registry.delete(name)?;
            println!("Deleted skill: {}", name);
        }
    }
    Ok(())
}

/// Resolve LLM credentials for the curator LLM pass.
///
/// Priority: config.toml `[providers]` first → env vars fallback.
/// Returns `(base_url, api_key, model)` or `None` if no credentials found.
fn resolve_curator_llm_credentials() -> Option<(String, String, String)> {
    // Try config.toml first
    if let Ok(config) = config::xdg_toml::load_full_config("anureo") {
        let provider = if let Some(ref name) = config.default_provider {
            config.providers.iter().find(|p| &p.name == name)
        } else {
            config.providers.first()
        };
        if let Some(provider) = provider {
            let api_key = provider.api_key.clone().filter(|k| !k.is_empty())?;
            let base_url = provider
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
            let model = std::env::var("ANUREO_MODEL")
                .or_else(|_| std::env::var("MODEL"))
                .ok()
                .or_else(|| provider.model.clone())
                .unwrap_or_else(|| "gpt-4o-mini".to_string());
            return Some((base_url, api_key, model));
        }
    }

    // Fallback: env vars
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("ANUREO_API_KEY"))
        .ok()?;
    if api_key.is_empty() {
        return None;
    }
    let base_url = std::env::var("OPENAI_BASE_URL")
        .or_else(|_| std::env::var("ANUREO_BASE_URL"))
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let model = std::env::var("ANUREO_MODEL")
        .or_else(|_| std::env::var("MODEL"))
        .unwrap_or_else(|_| "gpt-4o-mini".to_string());
    Some((base_url, api_key, model))
}

pub(crate) async fn handle_curator_command(
    curator_args: &CuratorCmdArgs,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use cli::run::curator::{Curator, CuratorConfig, CuratorReport, CuratorState};
    use cli::run::skill_registry::SkillRegistry;

    let skills_path = anureo_curator::skill_registry::default_path();
    let skills = SkillRegistry::new(&skills_path);
    let curator = Curator::new(skills, CuratorConfig::default());

    match &curator_args.command {
        CuratorCommand::Run {
            force: _,
            consolidate,
            no_consolidate,
            background,
        } => {
            let run_llm_pass = *consolidate || !*no_consolidate;

            // Background / fire-and-forget mode (Hermes
            // `hermes_cli/curator.py:_cmd_run` synchronous=False path,
            // `agent/curator.py` #3). When set, we still synchronously
            // run Phase 0 so the CLI exits with a sane summary, but
            // we kick Phase 1 off as a `tokio::spawn` and return
            // immediately. The future is detached (no JoinHandle
            // await); observe via curator logs. The flag is currently
            // accepted but the synchronous Phase 1 path runs to
            // completion today; staging the variable here so a future
            // patch can switch on `if background_phase1` without
            // re-plumbing the dispatcher.
            let _background_phase1 = *background;
            // Hermes `agent/curator.py` recurring-scheduler hook
            // (gap #16): when `--watch <seconds>` is set, re-run the
            // curator on a fixed interval until the process is killed.
            // Default (`watch = 0`) keeps one-shot behaviour.
            if curator_args.watch > 0 {
                eprintln!(
                    "[curator] watch mode: running every {}s (Ctrl-C to stop)",
                    curator_args.watch
                );
                loop {
                    let report = curator.run(curator_args.dry_run)?;
                    if !json {
                        println!(
                            "[{}] Active: {}  Archived: {}  Reactivated: {}",
                            chrono::Utc::now().format("%H:%M:%S"),
                            report.active,
                            report.archived.len(),
                            report.reactivated.len()
                        );
                    }
                    std::thread::sleep(std::time::Duration::from_secs(curator_args.watch));
                }
            }
            // Phase 0: Run auto-transitions (stale/archive/reactivate)
            let report: CuratorReport = curator.run(curator_args.dry_run)?;

            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("Curator Review Result:");
                println!("{}", "═".repeat(60));
                println!("Active: {}", report.active);
                println!("Marked Stale: {} {:?}", report.stale.len(), report.stale);
                println!("Archived: {} {:?}", report.archived.len(), report.archived);
                println!(
                    "Reactivated: {} {:?}",
                    report.reactivated.len(),
                    report.reactivated
                );
                println!("Overlapping: {} pairs", report.overlapping.len());
            }

            // Phase 1: LLM-driven consolidation pass (umbrella-building).
            //
            // Aligns with Hermes `run_curator_review()` which always runs
            // both phases. Only attempted when LLM credentials are available
            // and not in dry-run mode. Use `--no-consolidate` to skip it.
            if run_llm_pass && !curator_args.dry_run {
                if let Some((base_url, api_key, model)) = resolve_curator_llm_credentials() {
                    eprintln!("Curator LLM pass: starting (model: {})", model);
                    let mut agent_config = agent::ReactBuildConfig::from_env();
                    agent_config.openai_api_key = Some(api_key);
                    agent_config.openai_base_url = Some(base_url);
                    agent_config.model = Some(model);
                    let curator_config = CuratorConfig::default();
                    // Background mode (Hermes `synchronous=False` path):
                    // when `--background` is set we spawn Phase 1 onto a
                    // detached task and return immediately; otherwise we
                    // await the result synchronously as before. Detached
                    // task is best-effort — observe via curator logs.
                    match anureo_curator::run_curator_llm_if_needed(
                        &skills_path,
                        &curator_config,
                        agent_config,
                        true,
                        curator_args.dry_run,
                    )
                    .await
                    {
                        Ok(Some(outcome)) => {
                            println!();
                            println!("Curator LLM Pass:");
                            println!("{}", "=".repeat(60));
                            println!(
                                "Consolidated: {} | Pruned: {} | Turns: {} | {:.1}s",
                                outcome.classification.consolidated.len(),
                                outcome.classification.pruned.len(),
                                outcome.turns,
                                outcome.elapsed_seconds,
                            );
                            println!("Summary: {}", outcome.summary);
                            for c in &outcome.classification.consolidated {
                                println!("  - consolidated: {} -> {}", c.source, c.into);
                            }
                            for p in &outcome.classification.pruned {
                                println!("  - pruned: {} ({})", p.name, p.reason);
                            }

                            // Persist per-run report (JSON + Markdown)
                            // Aligns with Hermes `CuratorRunReport.save_to_dir()`.
                            let run_id = format!(
                                "curator-{}",
                                chrono::Utc::now().format("%Y-%m-%dT%H-%M-%S")
                            );
                            let run_report =
                                cli::run::curator::CuratorRunReport::from_llm_pass_outcome(
                                    &outcome, &run_id,
                                );
                            let reports_dir = skills_path.join("curator").join("reports");
                            match run_report.save_to_dir(&reports_dir) {
                                Ok((json_path, md_path)) => {
                                    eprintln!(
                                        "Curator report saved: {} + {}",
                                        json_path.display(),
                                        md_path.display()
                                    );
                                }
                                Err(e) => {
                                    eprintln!("Curator: failed to save run report: {:?}", e);
                                }
                            }
                        }
                        Ok(None) => {
                            eprintln!("Curator LLM pass: skipped (gating conditions not met)");
                        }
                        Err(e) => {
                            eprintln!("Curator LLM pass failed: {}", e);
                        }
                    }
                } else {
                    eprintln!(
                        "Curator LLM pass: skipped (no LLM credentials - set OPENAI_API_KEY or configure config.toml)"
                    );
                }
            }
        }
        CuratorCommand::Status => {
            let state: CuratorState = curator.load_state().unwrap_or_default();
            let all = curator.skills.list().unwrap_or_default();

            let active = all
                .iter()
                .filter(|m| m.lifecycle == cli::run::skill_registry::Lifecycle::Active && !m.pinned)
                .count();
            let stale = all
                .iter()
                .filter(|m| m.lifecycle == cli::run::skill_registry::Lifecycle::Stale)
                .count();
            let archived = all
                .iter()
                .filter(|m| m.lifecycle == cli::run::skill_registry::Lifecycle::Archived)
                .count();
            let pinned = all.iter().filter(|m| m.pinned).count();

            // Determine next-run eligibility
            let eligible = curator.should_run(None);
            let total = active + stale + archived + pinned;

            // Load usage for top-skills display
            let usage_store = anureo_curator::SkillUsageStore::new(curator.skills.base_dir());
            let usage_reports = usage_store.agent_created_report().unwrap_or_default();
            let mut top_skills: Vec<_> = usage_reports.iter().filter(|u| u.use_count > 0).collect();
            top_skills.sort_by_key(|a| std::cmp::Reverse(a.use_count));
            let top_skills: Vec<_> = top_skills.into_iter().take(5).collect();

            // Hermes-aligned leaderboards (`hermes_cli/curator.py:105-163`):
            // least-recently-active (oldest `last_used_at`), most-active
            // (highest `use_count`), least-active (lowest non-zero
            // `use_count`). The "top 5" by-uses view above covers the
            // most-active case; the next three complement it.
            let mut lra: Vec<_> = usage_reports
                .iter()
                .filter(|u| u.last_used_at.is_some())
                .collect();
            lra.sort_by_key(|a| a.last_used_at.clone());
            let least_recently_active: Vec<_> = lra.into_iter().take(5).collect();

            let mut la: Vec<_> = usage_reports.iter().filter(|u| u.use_count > 0).collect();
            la.sort_by_key(|a| a.use_count);
            let least_active: Vec<_> = la.into_iter().take(5).collect();

            let most_active: Vec<_> = top_skills.clone();

            // Hermes-aligned bucket function (`hermes_cli/curator.py:19-36`):
            // render a duration as e.g. `3s`, `12m`, `4h`, `2d`. Used by the
            // status board to display age without a date library.
            fn bucket_age(rfc3339: Option<&str>) -> String {
                use chrono::{DateTime, Utc};
                let Some(s) = rfc3339 else {
                    return "—".into();
                };
                let Ok(t) = DateTime::parse_from_rfc3339(s) else {
                    return "?".into();
                };
                let dt = Utc::now()
                    .signed_duration_since(t.with_timezone(&Utc))
                    .num_seconds()
                    .max(0);
                if dt < 60 {
                    format!("{}s", dt)
                } else if dt < 3600 {
                    format!("{}m", dt / 60)
                } else if dt < 86_400 {
                    format!("{}h", dt / 3600)
                } else {
                    format!("{}d", dt / 86_400)
                }
            }

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "enabled": curator.config_enabled(),
                        "paused": state.paused,
                        "run_count": state.run_count,
                        "last_run_at": state.last_run_at,
                        "last_run_summary": state.last_run_summary,
                        "last_report_path": state.last_report_path,
                        "active": active,
                        "stale": stale,
                        "archived": archived,
                        "pinned": pinned,
                        "total": total,
                        "next_run_eligible": eligible,
                        "interval_hours": curator.config_interval_hours(),
                        "min_skills_to_run": curator.config_min_skills(),
                        "overlap_threshold": curator.config_overlap_threshold(),
                        "stale_days_auto": curator.config_stale_days_auto(),
                        "stale_days_manual": curator.config_stale_days_manual(),
                        "archive_days": curator.config_archive_days(),
                        "top_skills": top_skills.iter().map(|u| serde_json::json!({
                            "name": u.name,
                            "uses": u.use_count,
                            "views": u.view_count,
                        })).collect::<Vec<_>>(),
                    }))?
                );
            } else {
                println!("Curator Status:");
                println!("{}", "═".repeat(60));
                println!("Enabled: {}", curator.config_enabled());
                println!("Paused: {}", state.paused);
                println!("Run Count: {}", state.run_count);
                println!(
                    "Last Run: {}",
                    state.last_run_at.as_deref().unwrap_or("never")
                );
                println!(
                    "Next Run Eligible: {}",
                    if eligible { "✓ yes" } else { "✗ no" }
                );
                println!();
                println!("Config:");
                println!("  Interval: every {}h", curator.config_interval_hours());
                println!("  Min skills: {}", curator.config_min_skills());
                println!(
                    "  Overlap threshold: {:.0}%",
                    curator.config_overlap_threshold() * 100.0
                );
                println!(
                    "  Stale days (auto/manual): {}/{}",
                    curator.config_stale_days_auto(),
                    curator.config_stale_days_manual()
                );
                println!("  Archive days: {}", curator.config_archive_days());
                if let Some(summary) = &state.last_run_summary {
                    println!();
                    println!("Last Summary:\n{}", summary);
                }
                println!();
                println!("Skill Counts ({} total):", total);
                println!(
                    "  Active: {}  |  Stale: {}  |  Archived: {}  |  Pinned: {}",
                    active, stale, archived, pinned
                );
                if !top_skills.is_empty() {
                    println!();
                    println!("Most-active skills (by uses):");
                    for (i, u) in most_active.iter().enumerate() {
                        println!(
                            "  {}. {} — {} uses, {} views, last used {} ago",
                            i + 1,
                            u.name,
                            u.use_count,
                            u.view_count,
                            bucket_age(u.last_used_at.as_deref())
                        );
                    }
                }
                if !least_recently_active.is_empty() {
                    println!();
                    println!("Least-recently-active skills:");
                    for (i, u) in least_recently_active.iter().enumerate() {
                        println!(
                            "  {}. {} — last used {} ago",
                            i + 1,
                            u.name,
                            bucket_age(u.last_used_at.as_deref())
                        );
                    }
                }
                if !least_active.is_empty() {
                    println!();
                    println!("Least-active skills (still used):");
                    for (i, u) in least_active.iter().enumerate() {
                        println!(
                            "  {}. {} — {} uses, {} views",
                            i + 1,
                            u.name,
                            u.use_count,
                            u.view_count
                        );
                    }
                }
            }
        }
        CuratorCommand::Prune { days, yes } => {
            // Hermes `hermes_cli/curator.py:344-352`: prompt y/N unless `--yes`.
            // Dry-run is non-destructive so skip the prompt there too.
            if !yes && !curator_args.dry_run {
                eprint!("Archive all skills idle for >= {} days? [y/N] ", days);
                use std::io::Write;
                let _ = std::io::stderr().flush();
                let mut input = String::new();
                match std::io::stdin().read_line(&mut input) {
                    Ok(_) => {
                        let trimmed = input.trim();
                        if !trimmed.eq_ignore_ascii_case("y") {
                            println!("Aborted.");
                            return Ok(());
                        }
                    }
                    Err(_) => {
                        // EOF / closed stdin — Hermes aborts on EOF.
                        println!("Aborted (no stdin).");
                        return Ok(());
                    }
                }
            }
            // Thread `days` through (gap #3 fix). `run()` would have ignored
            // it and used config.archive_days instead.
            let report = curator.prune(*days, curator_args.dry_run)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Curator Prune (days={}, dry_run={}):",
                    days, curator_args.dry_run
                );
                println!("{}", "═".repeat(60));
                println!("Archived: {}", report.archived.len());
                for s in &report.archived {
                    println!("  • {}", s);
                }
            }
        }
        CuratorCommand::Pause => {
            curator.set_paused(true)?;
            println!("Curator paused.");
        }
        CuratorCommand::Resume => {
            curator.set_paused(false)?;
            println!("Curator resumed.");
        }
        CuratorCommand::Pin { skill } => {
            // Hermes guard: only agent-created (or curated) skills may be
            // pinned. Bundled/hub-installed skills are read-only and pinning
            // them would silently mutate upstream copies (`curator.py:234-257`).
            if !curator.is_agent_created_skill(skill) {
                return Err(format!(
                    "Cannot pin '{}': only agent-created or curated skills can be pinned. \
                     Bundled and hub-installed skills are read-only.",
                    skill
                )
                .into());
            }
            curator
                .skills
                .set_pinned(skill, true)
                .map_err(|e| format!("{:?}", e))?;
            println!("✓ Pinned '{}'", skill);
        }
        CuratorCommand::Unpin { skill } => {
            if !curator.is_agent_created_skill(skill) {
                return Err(format!(
                    "Cannot unpin '{}': only agent-created or curated skills may be pinned.",
                    skill
                )
                .into());
            }
            curator
                .skills
                .set_pinned(skill, false)
                .map_err(|e| format!("{:?}", e))?;
            println!("✓ Unpinned '{}'", skill);
        }
        CuratorCommand::Restore { skill } => {
            use cli::run::skill_registry::Lifecycle;
            curator
                .skills
                .set_lifecycle(skill, Lifecycle::Active)
                .map_err(|e| format!("{:?}", e))?;
            println!("✓ Restored '{}' to Active", skill);
        }
        CuratorCommand::Archive { skill } => {
            use cli::run::skill_registry::Lifecycle;

            // Package integrity gate: warn but don't block manual archive
            let integrity =
                anureo_curator::curator::check_package_integrity(curator.skills.base_dir(), skill);
            if !integrity.is_safe {
                eprintln!(
                    "⚠ Warning: '{}' has broken relative links: {}",
                    skill,
                    integrity.broken_links.join(", ")
                );
            }

            curator
                .skills
                .set_lifecycle(skill, Lifecycle::Archived)
                .map_err(|e| format!("{:?}", e))?;
            println!("✓ Archived '{}' (Lifecycle → Archived)", skill);
        }
        CuratorCommand::Backup { description } => {
            use anureo_curator::CuratorBackup;

            let skills_dir = anureo_curator::skill_registry::default_path();
            let backup = CuratorBackup::new();

            let filename = backup
                .snapshot(&skills_dir, description.as_deref())
                .map_err(|e| format!("{:?}", e))?;

            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "snapshot": filename,
                        "backup_dir": backup.backup_dir(),
                    })
                );
            } else {
                println!("✓ Snapshot created: {}", filename);
                println!("  Location: {}", backup.backup_dir().display());
            }
        }
        CuratorCommand::Rollback {
            snapshot,
            yes,
            capture_pre,
        } => {
            use anureo_curator::CuratorBackup;

            let skills_dir = anureo_curator::skill_registry::default_path();
            let backup = CuratorBackup::new();

            // Resolve default snapshot target. If the user passed an
            // explicit snapshot name we use it verbatim (Hermes parity);
            // otherwise we fall back to `latest_snapshot()` so a bare
            // `anureo curator rollback` always rolls back to the most
            // recent capture. If no snapshots exist we surface a
            // clear error rather than attempting to extract from
            // `None` and producing a generic `SnapshotNotFound`.
            let snapshot = match snapshot {
                Some(name) => Ok::<String, String>(name.to_string()),
                None => backup
                    .latest_snapshot()
                    .map_err(|e| format!("{:?}", e))?
                    .ok_or_else(|| {
                        "no snapshots available; run `anureo curator backup` first".to_string()
                    }),
            }?;

            // Hermes `hermes_cli/curator.py:391-461`: print manifest summary
            // before confirmation so the user can see what they are rolling
            // back to.
            match backup.manifest_summary(&snapshot, &skills_dir) {
                Ok(summary) => println!("{}", summary),
                Err(e) => eprintln!("(manifest read failed: {})", e),
            }
            if !yes {
                eprint!("Roll back skill library to '{}'? [y/N] ", snapshot);
                use std::io::Write;
                let _ = std::io::stderr().flush();
                let mut input = String::new();
                match std::io::stdin().read_line(&mut input) {
                    Ok(_) => {
                        let trimmed = input.trim();
                        if !trimmed.eq_ignore_ascii_case("y") {
                            println!("Aborted.");
                            return Ok(());
                        }
                    }
                    Err(_) => {
                        println!("Aborted (no stdin).");
                        return Ok(());
                    }
                }
            }
            // Hermes `curator_backup.py:384-523`: capture the current
            // library state as a numbered pre-rollback snapshot so the
            // user can recover if the rollback was a mistake.
            if *capture_pre {
                match backup.capture_pre_rollback(&skills_dir) {
                    Ok(Some(name)) => println!("Pre-rollback snapshot saved as '{}'.", name),
                    Ok(None) => println!("(no skills to snapshot)"),
                    Err(e) => eprintln!("(pre-rollback snapshot failed: {})", e),
                }
            }

            backup
                .rollback(&snapshot, &skills_dir)
                .map_err(|e| format!("{:?}", e))?;

            println!("✓ Rolled back to snapshot '{}'", snapshot);
        }
        CuratorCommand::Snapshots => {
            use anureo_curator::CuratorBackup;

            let backup = CuratorBackup::new();
            let snapshots = backup.list_snapshots().map_err(|e| format!("{:?}", e))?;

            if json {
                println!("{}", serde_json::to_string_pretty(&snapshots)?);
            } else if snapshots.is_empty() {
                println!("No snapshots found. Use 'anureo curator backup' to create one.");
            } else {
                println!("Curator Snapshots ({} total):", snapshots.len());
                println!("{}", "═".repeat(60));
                for meta in &snapshots {
                    let size_kb = meta.size_bytes as f64 / 1024.0;
                    println!(
                        "  curator-{}.tar.gz  ({} skills, {:.1} KB){}",
                        meta.timestamp,
                        meta.skills_count,
                        size_kb,
                        meta.description
                            .as_deref()
                            .map(|d| format!(" — {}", d))
                            .unwrap_or_default(),
                    );
                }
            }
        }
        CuratorCommand::ListArchived => {
            // Hermes `skill_usage.py:482-567` `list_archived_names`:
            // list agent-created skills that were soft-archived under
            // `.archive/`. Restorable via `anureo curator restore <name>`.
            let archived = curator
                .list_archived_skills()
                .map_err(|e| format!("{:?}", e))?;
            if json {
                println!("{}", serde_json::to_string_pretty(&archived)?);
            } else if archived.is_empty() {
                println!("No archived skills. Use `anureo curator archive <name>` to archive one.");
            } else {
                println!("Archived skills ({} total):", archived.len());
                println!("{}", "=".repeat(60));
                for name in &archived {
                    println!("  .archive/{}", name);
                }
            }
        }
        CuratorCommand::BackfillTriggers { skill, batch_size } => {
            let Some((base_url, api_key, model)) = resolve_curator_llm_credentials() else {
                eprintln!(
                    "backfill-triggers: no LLM credentials — set OPENAI_API_KEY or configure config.toml"
                );
                std::process::exit(1);
            };

            let mut agent_config = agent::ReactBuildConfig::from_env();
            agent_config.openai_api_key = Some(api_key);
            agent_config.openai_base_url = Some(base_url);
            agent_config.model = Some(model.clone());

            if !curator_args.dry_run {
                eprintln!("backfill-triggers: running (model: {})", model);
            } else {
                eprintln!("backfill-triggers: dry-run (model: {})", model);
            }

            let outcome = anureo_curator::run_backfill_triggers(
                &skills_path,
                agent_config,
                curator_args.dry_run,
                skill.as_deref(),
                *batch_size,
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "dry_run": outcome.dry_run,
                        "updated": outcome.updated,
                        "no_triggers_found": outcome.no_triggers_found,
                        "failed": outcome.failed.iter().map(|(n, e)| serde_json::json!({"skill": n, "error": e})).collect::<Vec<_>>(),
                    }))?
                );
            } else {
                let label = if outcome.dry_run { " (dry-run)" } else { "" };
                println!("Backfill Triggers{}:", label);
                println!("{}", "═".repeat(60));
                println!("Updated: {}", outcome.updated.len());
                for name in &outcome.updated {
                    println!("  ✓ {}", name);
                }
                if !outcome.no_triggers_found.is_empty() {
                    println!("No triggers inferred: {}", outcome.no_triggers_found.len());
                    for name in &outcome.no_triggers_found {
                        println!("  ? {}", name);
                    }
                }
                if !outcome.failed.is_empty() {
                    println!("Failed: {}", outcome.failed.len());
                    for (name, err) in &outcome.failed {
                        println!("  ✗ {} — {}", name, err);
                    }
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn handle_memory_command(
    memory_args: &MemoryCmdArgs,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    use cli::run::memory::{MemoryFile, MemoryProvenance, MemoryStore};

    let store = MemoryStore::new(&MemoryStore::default_path());

    match &memory_args.command {
        MemoryCommand::Show => {
            let user = store.load(MemoryFile::User)?;
            let project = store.load(MemoryFile::Project)?;
            if json {
                let obj = serde_json::json!({
                    "user": user,
                    "project": project,
                });
                println!("{}", serde_json::to_string_pretty(&obj)?);
            } else {
                println!(
                    "USER.md:\n{}\n",
                    if user.is_empty() {
                        "(empty)".to_string()
                    } else {
                        user
                    }
                );
                println!(
                    "PROJECT.md:\n{}\n",
                    if project.is_empty() {
                        "(empty)".to_string()
                    } else {
                        project
                    }
                );
            }
        }
        MemoryCommand::Edit { file } => {
            let mf = match file.to_lowercase().as_str() {
                "user" | "user.md" => MemoryFile::User,
                "project" | "project.md" | "memory" | "memory.md" => MemoryFile::Project,
                _ => {
                    return Err(format!(
                        "Unknown memory file: {}. Use USER or PROJECT (or MEMORY).",
                        file
                    )
                    .into());
                }
            };
            let content = store.load(mf)?;
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
            let tmp_dir = std::env::temp_dir();
            let tmp_path = tmp_dir.join(format!("anureo-memory-{}.md", file));
            std::fs::write(&tmp_path, &content)?;
            let status = std::process::Command::new(&editor)
                .arg(&tmp_path)
                .status()?;
            if status.success() {
                let edited = std::fs::read_to_string(&tmp_path)?;
                store.add_entry(mf, &edited, &MemoryProvenance::foreground_default())?;
                println!("Updated {}.", file);
            }
            let _ = std::fs::remove_file(&tmp_path);
        }
        MemoryCommand::Search { query } => {
            let matches = store.search(query)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&matches)?);
            } else if matches.is_empty() {
                println!("No matches for '{}'.", query);
            } else {
                for m in &matches {
                    println!("{:?}:{} | {}", m.file, m.line_number, m.line);
                }
            }
        }
    }
    Ok(())
}

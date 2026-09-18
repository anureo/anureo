//! Wraps anureo::run_agent_with_options with stderr display callback.
//! Uses protocol format (type + payload) and optional envelope per protocol_spec.

use agent::build_react_run_context;
use agent::profile::list_available_profiles;
use agent::run::{
    build_react_config, run_agent_from_config, RunCmd, RunParams, TypedAnyStreamEvent,
};
use agent::state::ReActState;
use agent::state::ToolResult;
use agent::ResolvedAgent;
use agent::{DupState, GotState, TotState};
use chrono::Local;
use anureo_llm::support::uuid6::uuid6;
use anureo_llm::ToolCall;
use model_spec_core::resolver::{
    build_composite_resolver, ConfigModelEntry, ConfigProviderEntry, ModelResolver,
};
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use stream_event::Envelope;
use tracing::Instrument;

type StreamCallback = Arc<Mutex<dyn FnMut(Value) + Send>>;

use super::display::{
    format_dup_state_display, format_got_state_display, format_react_state_display,
    format_tot_state_display, truncate_display,
};

use crate::display as panel_format;
use stream_event::EnvelopeState;
use stream_event::StreamEvent;

use agent::run::RunError;

/// Provider for anureo's default extra tools (currently the workflow tool).
///
/// Thin re-export of `tool_workflow::default_workflow_tool_provider` so CLI
/// internals can use `crate::run::default_workflow_tool_provider()` without
/// importing `tool-workflow` everywhere. The real implementation lives on
/// the tool crate so non-CLI front-ends (`acp`, `telegram-bot`) can reuse it
/// without depending on the CLI binary.
pub fn default_workflow_tool_provider() -> agent::run::ExtraToolsProvider {
    tool_workflow::default_workflow_tool_provider()
}

fn load_config_providers() -> Vec<ConfigProviderEntry> {
    let full = config::load_full_config("anureo").ok();
    full.map(|f| {
        f.providers
            .into_iter()
            .filter(|p| !p.models.is_empty())
            .map(|p| ConfigProviderEntry {
                name: p.name,
                models: p
                    .models
                    .into_iter()
                    .map(|m| ConfigModelEntry {
                        id: m.id,
                        context_limit: m.context_limit,
                        output_limit: m.output_limit,
                    })
                    .collect(),
            })
            .collect()
    })
    .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStopReason {
    EndTurn,
    Cancelled,
}

fn completion_reply(result: agent::run::RunCompletion) -> (String, Option<String>, RunStopReason) {
    match result {
        agent::run::RunCompletion::Finished(result) => (
            result.reply,
            result.reasoning_content,
            RunStopReason::EndTurn,
        ),
        agent::run::RunCompletion::Cancelled => (String::new(), None, RunStopReason::Cancelled),
    }
}

/// Prints agent profile info to stderr at startup (structured panel format).
fn print_agent_banner(resolved: &Option<ResolvedAgent>) {
    match resolved {
        Some(ra) => {
            eprintln!(
                "{}",
                panel_format::format_agent_line(
                    &ra.name,
                    &ra.source.to_string(),
                    ra.description.as_deref(),
                )
            );
        }
        None => eprintln!("{}", panel_format::format_panel_line("AGENT", "(none)")),
    }
}

/// Prints current local time to stderr (when --timestamp is set, before each reply).
pub fn print_reply_timestamp() {
    eprintln!("{}", Local::now().format("%Y-%m-%d %H:%M:%S"));
}

/// Prints available agent names to stderr (use -P/--agent to switch).
fn print_available_agents() {
    let profiles = list_available_profiles();
    if profiles.is_empty() {
        return;
    }
    let names: Vec<&str> = profiles.iter().map(|p| p.name.as_str()).collect();
    eprintln!(
        "available agents: {} (use -P/--agent to switch)",
        names.join(", ")
    );
}

/// Single line when a node is entered (unified across agents).
fn log_node_enter(from: Option<&str>, node_id: &str, verbose: bool) {
    if !verbose {
        return;
    }
    let from = from.unwrap_or("START");
    eprintln!("Entering: {} (from {})", node_id, from);
}

/// Prints current model name and context info to stderr at startup (structured panel format).
async fn print_model_info(model: Option<&String>, model_tier: Option<&model_spec_core::ModelTier>) {
    let model_name = match model {
        Some(m) if !m.is_empty() => m.as_str(),
        _ => {
            let context = match model_tier {
                Some(tier) => format!("tier: {:?}", tier),
                None => "unknown context".to_string(),
            };
            eprintln!("{}", panel_format::format_model_line("(default)", &context));
            return;
        }
    };

    let providers = load_config_providers();
    let resolver = build_composite_resolver(None, providers);
    let spec = if model_name.contains('/') {
        resolver.resolve_combined(model_name).await
    } else {
        let providers = load_config_providers();
        let mut found = None;
        for p in &providers {
            if let Some(s) = resolver.resolve(&p.name, model_name).await {
                found = Some(s);
                break;
            }
        }
        found
    };
    match spec {
        Some(spec) => {
            eprintln!(
                "{}",
                panel_format::format_model_line(
                    model_name,
                    &format!(
                        "{} context",
                        panel_format::format_context_limit(spec.limit.context)
                    ),
                )
            );
        }
        None => {
            tracing::debug!(
                "Model spec resolution failed for '{}'. \
                 The model may not be in the models.dev database, or there was a network error.",
                model_name
            );
            eprintln!(
                "{}",
                panel_format::format_model_line(model_name, "context: unknown")
            );
        }
    }
}

#[derive(Debug)]
pub struct RunAgentOutput {
    pub reply: String,
    pub reasoning_content: Option<String>,
    pub events: Option<Vec<Value>>,
    pub reply_envelope: Option<Envelope>,
    pub stop_reason: RunStopReason,
}

/// Result of run_agent_wrapper.
pub type RunAgentResult = Result<RunAgentOutput, RunError>;

/// Runs the agent with stderr display for stream events.
/// When `opts.output_json` is true: if `stream_out` is Some, each event is written via it and returns (reply, None);
/// otherwise collects all events and returns (reply, Some(events)).
///
/// Wraps the whole run (config build + execution) in an `agent_run` root span
/// carrying the business `thread_id` so every log line emitted during the run
/// inherits it; the run phase is additionally covered by the span created in
/// `run_agent_from_config`.
pub async fn run_agent_wrapper(
    opts: &agent::RunOptions,
    cmd: &RunCmd,
    stream_out: Option<StreamCallback>,
) -> RunAgentResult {
    let root_span = match opts.thread_id.as_deref() {
        Some(tid) => tracing::info_span!("agent_run", thread_id = tid),
        None => tracing::info_span!("agent_run", thread_id = %uuid6()),
    };
    run_agent_wrapper_inner(opts, cmd, stream_out)
        .instrument(root_span)
        .await
}

async fn run_agent_wrapper_inner(
    opts: &agent::RunOptions,
    _cmd: &RunCmd,
    stream_out: Option<StreamCallback>,
) -> RunAgentResult {
    // NOTE: the business `thread_id` root span is attached by the
    // `run_agent_wrapper` shell (whole run: config build + execution) and by
    // `run_agent_from_config` (run phase) so the ACP/CLI entry points share
    // one mechanism. `.instrument()` is used instead of a manual span guard
    // to avoid the `!Send` future issue from holding a guard across awaits.

    let anureo_opts = opts.clone();
    let (mut config, resolved_agent, skill_registry) = build_react_config(&anureo_opts);

    if config.model.is_none() && config.model_tier.is_some() {
        config = agent::resolve_tier_and_build_config(&config).await;
    }

    // Default extra tools (e.g. the workflow tool) are already registered by
    // `build_react_config` via `default_extra_tools_provider`. The old
    // `register_extra_tools` call here was a no-op for the skill registry:
    // it mutated `config.extra_tools` AFTER the registry was finalized, so
    // the workflow tool's `workflow` builtin skill was never injected.

    print_loaded_tools(&config).await?;
    if !opts.output_json {
        if opts.dry_run {
            eprintln!("dry run: tools will not be executed");
        }
        print_agent_banner(&resolved_agent);
        print_available_agents();
        if config.role_setting.is_some() {
            eprintln!("agent profile role included in system prompt (see state.messages[0]).");
        }
        if config.agents_md.is_some() {
            eprintln!("AGENTS.md loaded; included in system prompt.");
        }
        print_model_info(config.model.as_ref(), config.model_tier.as_ref()).await;
        // Verbose level 2+ replaces the tools one-liner with a multiline block
        // listing each tool name + description. Level 1 keeps the one-liner.
        if opts.verbose_level >= 2 {
            print_tools_multiline(&config).await;
        }
        // Skill list (one-liner at -v, multiline with sources at -vv).
        if opts.verbose_level >= 1 {
            if let Some(registry) = skill_registry.as_ref() {
                if opts.verbose_level >= 2 {
                    print_skills_multiline(registry);
                } else {
                    print_skills_one_line(registry);
                }
            }
        }
    }

    let display_max_len = opts.display_max_len;

    if opts.output_json {
        let session_id = format!(
            "run-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        if let Some(ref out) = stream_out {
            let out = out.clone();
            let state = Arc::new(Mutex::new(EnvelopeState::new(session_id.clone())));
            let state_clone = state.clone();
            let on_event = Box::new(move |ev: TypedAnyStreamEvent| {
                let v = match state_clone.lock() {
                    Ok(mut s) => ev.to_protocol_format(&mut s),
                    Err(_) => return,
                };
                let v = match v {
                    Ok(x) => x,
                    Err(e) => {
                        eprintln!("anureo: failed to serialize stream event: {}", e);
                        serde_json::json!({ "type": "_error", "_serialize_error": format!("{}", e) })
                    }
                };
                if let Ok(mut f) = out.lock() {
                    f(v);
                }
            });
            let result = run_agent_from_config(
                &config,
                &RunCmd::React,
                RunParams {
                    message: opts.message.clone(),
                    verbose: opts.verbose,
                    cancellation: opts.cancellation.clone(),
                    any_stream_event_sender: opts.any_stream_event_sender.clone(),
                    llm_override: None,
                    thread_id: opts.thread_id.clone(),
                },
                Some(on_event),
            )
            .await?;
            let reply_env = state.lock().map(|s| s.reply_envelope()).ok();
            let (reply, reasoning_content, stop_reason) = completion_reply(result);
            return Ok(RunAgentOutput {
                reply,
                reasoning_content,
                events: None,
                reply_envelope: reply_env,
                stop_reason,
            });
        }
        let events: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let state = Arc::new(Mutex::new(EnvelopeState::new(session_id.clone())));
        let state_clone = state.clone();
        let on_event = Box::new(move |ev: TypedAnyStreamEvent| {
            let v = match state_clone.lock() {
                Ok(mut s) => ev.to_protocol_format(&mut s),
                Err(_) => return,
            };
            match v {
                Ok(value) => {
                    if let Ok(mut vec) = events_clone.lock() {
                        vec.push(value);
                    }
                }
                Err(e) => {
                    eprintln!("anureo: failed to serialize stream event to JSON: {}", e);
                    if let Ok(mut vec) = events_clone.lock() {
                        vec.push(serde_json::json!({
                            "type": "_error",
                            "_serialize_error": format!("{}", e),
                        }));
                    }
                }
            }
        });
        let result = run_agent_from_config(
            &config,
            &RunCmd::React,
            RunParams {
                message: opts.message.clone(),
                verbose: opts.verbose,
                cancellation: opts.cancellation.clone(),
                any_stream_event_sender: opts.any_stream_event_sender.clone(),
                llm_override: None,
                thread_id: opts.thread_id.clone(),
            },
            Some(on_event),
        )
        .await?;
        let events = events.lock().map(|v| v.clone()).unwrap_or_default();
        let reply_env = state.lock().map(|s| s.reply_envelope()).ok();
        let (reply, reasoning_content, stop_reason) = completion_reply(result);
        return Ok(RunAgentOutput {
            reply,
            reasoning_content,
            events: Some(events),
            reply_envelope: reply_env,
            stop_reason,
        });
    }

    let agent_display = resolved_agent
        .as_ref()
        .map(|ra| format!("{} ({})", ra.name, ra.source));
    let state = Arc::new(Mutex::new(EventState {
        agent_display,
        markdown_renderer: StreamingMarkdownRenderer::new(),
        ..EventState::default()
    }));

    let state_clone = state.clone();
    let verbose = opts.verbose;
    let output_timestamp = opts.output_timestamp;
    let on_event = Box::new(move |ev: TypedAnyStreamEvent| {
        let mut s = state_clone.lock().unwrap();
        match &ev {
            TypedAnyStreamEvent::React(e) => {
                on_event_react(e, &mut s, display_max_len, verbose, output_timestamp)
            }
            TypedAnyStreamEvent::Dup(e) => {
                on_event_dup(e, &mut s, display_max_len, verbose, output_timestamp)
            }
            TypedAnyStreamEvent::Tot(e) => {
                on_event_tot(e, &mut s, display_max_len, verbose, output_timestamp)
            }
            TypedAnyStreamEvent::Got(e) => {
                on_event_got(e, &mut s, display_max_len, verbose, output_timestamp)
            }
        }
    });

    let start = Instant::now();
    let result = run_agent_from_config(
        &config,
        &RunCmd::React,
        RunParams {
            message: opts.message.clone(),
            verbose: opts.verbose,
            cancellation: opts.cancellation.clone(),
            any_stream_event_sender: opts.any_stream_event_sender.clone(),
            llm_override: None,
            thread_id: opts.thread_id.clone(),
        },
        Some(on_event),
    )
    .await?;
    let duration = start.elapsed();

    let outcome = match &result {
        agent::run::RunCompletion::Finished(_) => "finished",
        agent::run::RunCompletion::Cancelled => "cancelled",
    };
    let (prompt_tokens, completion_tokens, last_node, agent) = state
        .lock()
        .map(|s| {
            (
                s.total_prompt_tokens,
                s.total_completion_tokens,
                s.last_node.clone(),
                s.agent_display.clone(),
            )
        })
        .unwrap_or((0, 0, None, None));
    tracing::debug!(
        stage = "agent_run_completed",
        duration_ms = duration.as_millis() as u64,
        outcome = outcome,
        prompt_tokens = prompt_tokens,
        completion_tokens = completion_tokens,
        last_node = last_node.as_deref().unwrap_or(""),
        agent = agent.as_deref().unwrap_or(""),
        "agent run completed; flushing display"
    );

    // Flush the streaming markdown renderer's remaining buffer
    if let Ok(mut s) = state.lock() {
        s.markdown_renderer.finish();
    }

    if verbose {
        if let Some(ref from) = state.lock().unwrap().last_node {
            eprintln!("flow: {} ? END", from);
        }
    }
    if let Ok(s) = state.lock() {
        let total_tokens = s.total_prompt_tokens as u64 + s.total_completion_tokens as u64;
        let secs = duration.as_secs_f64();
        let _tokens_per_sec = if secs > 0.0 {
            total_tokens as f64 / secs
        } else {
            0.0
        };
        eprintln!(
            "\n{}",
            panel_format::format_usage_line(
                duration,
                s.total_prompt_tokens,
                s.total_completion_tokens,
                s.last_prefill_duration,
                s.last_decode_duration,
                verbose,
            )
        );
    }
    let (reply, reasoning_content, stop_reason) = completion_reply(result);

    Ok(RunAgentOutput {
        reply,
        reasoning_content,
        events: None,
        reply_envelope: None,
        stop_reason,
    })
}

use crate::display::StreamingMarkdownRenderer;

fn print_stream_chunk(content: &str, renderer: &mut StreamingMarkdownRenderer) {
    renderer.push_text(content);
}

fn handle_stream_content(
    s: &mut EventState,
    content: &str,
    is_thinking: bool,
    output_timestamp: bool,
) {
    if is_thinking {
        s.in_thinking = true;
    }
    if let Some(sp) = s.spinner.take() {
        sp.finish_box();
    }
    if !s.reply_started {
        if let Some(ref ad) = s.agent_display {
            eprintln!("{}", panel_format::format_panel_line("AGENT", ad));
        }
        if output_timestamp {
            print_reply_timestamp();
        }
        s.reply_started = true;
    }
    if s.in_thinking && !is_thinking {
        eprintln!();
        eprintln!("{}", panel_format::format_thinking_separator());
        s.in_thinking = false;
    }
    print_stream_chunk(content, &mut s.markdown_renderer);
}

fn on_event_react(
    ev: &StreamEvent<ReActState>,
    s: &mut EventState,
    display_max_len: usize,
    verbose: bool,
    output_timestamp: bool,
) {
    let ev_tag: &'static str = match ev {
        StreamEvent::TaskStart { node_id, .. } => match node_id.as_str() {
            "think" => "TaskStart(think)",
            "act" => "TaskStart(act)",
            "observe" => "TaskStart(observe)",
            _ => "TaskStart(other)",
        },
        StreamEvent::TaskEnd { node_id, .. } => match node_id.as_str() {
            "think" => "TaskEnd(think)",
            "act" => "TaskEnd(act)",
            "observe" => "TaskEnd(observe)",
            _ => "TaskEnd(other)",
        },
        StreamEvent::TextDelta { .. } => "TextDelta",
        StreamEvent::ReasoningDelta { .. } => "ReasoningDelta",
        StreamEvent::Values(_) => "Values",
        StreamEvent::Updates { .. } => "Updates",
        StreamEvent::Custom(_) => "Custom",
        StreamEvent::Checkpoint(_) => "Checkpoint",
        StreamEvent::TotExpand { .. } => "TotExpand",
        StreamEvent::TotEvaluate { .. } => "TotEvaluate",
        StreamEvent::TotBacktrack { .. } => "TotBacktrack",
        _ => "other",
    };
    tracing::info!(ev = ev_tag, "on_event_react enter");
    match ev {
        StreamEvent::TaskStart { node_id, .. } => {
            if let Some(sp) = s.spinner.take() {
                sp.finish_box();
                eprintln!();
            }
            if node_id == "think" {
                let label = if s.turn == 0 {
                    "Thinking...".to_string()
                } else {
                    format!("Thinking... (turn {})", s.turn + 1)
                };
                s.spinner = Some(Box::new(super::spinner::Spinner::new(label)));
            }
            log_node_enter(s.last_node.as_deref(), node_id, verbose);
            s.last_node = Some(node_id.clone());
        }
        StreamEvent::TextDelta { content, .. } => {
            handle_stream_content(s, content, false, output_timestamp);
        }
        StreamEvent::ReasoningDelta { content, .. } => {
            handle_stream_content(s, content, true, output_timestamp);
        }
        StreamEvent::Updates {
            node_id,
            state: react_state,
            ..
        } => {
            // Always show title generation result (non-verbose too)
            if node_id == "title" {
                if let Some(ref title) = react_state.summary {
                    eprintln!("Session title: {}", title);
                }
            }
            // When thinking content ended without a trailing newline and the next
            // event is Updates (not Messages), ensure we close the thinking block
            // before printing state / tool call info.
            if s.in_thinking {
                eprintln!();
                eprintln!("{}", panel_format::format_thinking_separator());
                s.in_thinking = false;
            }
            if verbose {
                let label = match node_id.as_str() {
                    "think" => {
                        s.turn += 1;
                        format!("state after think (turn {})", s.turn)
                    }
                    "act" => "state after act".to_string(),
                    "observe" => "state after observe".to_string(),
                    _ => format!("state after {}", node_id),
                };
                eprintln!("--- {} ---", label);
                eprintln!(
                    "{}",
                    format_react_state_display(react_state, display_max_len)
                );
                if node_id == "think" && react_state.tool_calls.is_empty() {
                    eprintln!("(think ? END: tool_calls empty, LLM gave FINAL_ANSWER)");
                }
            } else {
                // Save tool_calls during think (non-verbose)
                if node_id == "think" && !react_state.tool_calls.is_empty() {
                    if let Some(sp) = s.spinner.take() {
                        sp.finish_box();
                    }
                    // Show tool call lines (name + args summary)
                    for tc in &react_state.tool_calls {
                        let summary = crate::display::tool_summary::format_call_summary(
                            &tc.name,
                            &tc.arguments,
                        );
                        eprintln!(
                            "{}",
                            panel_format::panel_format::format_tool_call(&tc.name, &summary)
                        );
                        // Show DIFF immediately for edit/multiedit (doesn't need result)
                        if let Some(diff) =
                            crate::display::format_diff(&tc.name, &tc.arguments, "", false)
                        {
                            eprintln!("{}", diff);
                        }
                    }
                    if let Some(tc) = react_state.tool_calls.first() {
                        let desc =
                            serde_json::from_str::<Value>(&tc.arguments)
                                .ok()
                                .and_then(|v| {
                                    v.get("description")
                                        .and_then(|d| d.as_str())
                                        .map(String::from)
                                });
                        let label = match desc {
                            Some(d) if !d.is_empty() => format!("{} - {}", tc.name, d),
                            _ => tc.name.clone(),
                        };
                        s.spinner = Some(Box::new(super::spinner::Spinner::new(label)));
                    }
                    s.pending_tool_calls = react_state.tool_calls.clone();
                    s.pending_tool_start = Some(std::time::Instant::now());
                }
                // Save tool_results during act (observe will clear them)
                if node_id == "act" && !react_state.tool_results.is_empty() {
                    s.pending_tool_results = react_state.tool_results.clone();
                }
            }
            // Print PREVIEW/DIFF and DONE lines on observe
            if node_id == "observe" {
                let elapsed = s.pending_tool_start.map(|t| t.elapsed());
                // Use cached tool results from act (observe clears tool_results)
                let tool_results = if react_state.tool_results.is_empty() {
                    &s.pending_tool_results
                } else {
                    // Fallback: some paths may have results directly in observe state
                    &react_state.tool_results
                };
                for tc in s.pending_tool_calls.drain(..) {
                    let result_text =
                        crate::display::find_tool_result(tool_results, &tc.name, &tc.id);
                    let is_error =
                        crate::display::find_tool_result_error(tool_results, &tc.name, &tc.id);
                    let is_edit_like = tc.name == "edit" || tc.name == "multiedit";

                    if is_error {
                        let err_msg = match &result_text {
                            Some(r) => r.lines().next().unwrap_or("error"),
                            None => "error",
                        };
                        eprintln!(
                            "{}",
                            panel_format::format_panel_line(
                                "ERROR",
                                &format!(
                                    "{}: {}",
                                    tc.name,
                                    crate::display::tool_summary::truncate(err_msg, 80)
                                )
                            )
                        );
                    }

                    // PREVIEW and result fallback: skip for edit/multiedit (diff already shown)
                    if !is_edit_like {
                        if let Some(ref result) = result_text {
                            if let Some(preview) = crate::display::format_preview(
                                &tc.name,
                                &tc.arguments,
                                result,
                                false,
                            ) {
                                eprintln!("{}", preview);
                            } else if !is_error && !result.trim().is_empty() {
                                eprintln!(
                                    "{}",
                                    crate::display::tool_preview::format_result_preview(
                                        &tc.name, result, elapsed,
                                    )
                                );
                            }
                        }
                    }

                    // DIFF for edit/multiedit already shown during think; skip here
                    if !is_edit_like {
                        if let Some(ref result) = result_text {
                            if let Some(diff) =
                                crate::display::format_diff(&tc.name, &tc.arguments, result, false)
                            {
                                eprintln!("{}", diff);
                            }
                        }
                    }

                    // Show DONE line for non-edit tools
                    if !is_edit_like {
                        let done_summary = crate::display::tool_summary::format_done_summary(
                            &tc.name,
                            result_text.as_deref().unwrap_or(""),
                            is_error,
                        );
                        eprintln!(
                            "{}",
                            panel_format::panel_format::format_tool_done(
                                &tc.name,
                                &done_summary,
                                elapsed
                            )
                        );
                    }
                }
                s.pending_tool_start = None;
                s.pending_tool_results.clear();
            }
        }
        StreamEvent::TurnFinish { usage, .. } => {
            s.accumulate_usage(usage.input, usage.output);
        }
        _ => {}
    }

    tracing::info!(ev = ev_tag, "on_event_react exit");
}

fn on_event_dup(
    ev: &StreamEvent<DupState>,
    s: &mut EventState,
    display_max_len: usize,
    verbose: bool,
    output_timestamp: bool,
) {
    match ev {
        StreamEvent::TaskStart { node_id, .. } => {
            log_node_enter(s.last_node.as_deref(), node_id, verbose);
            s.last_node = Some(node_id.clone());
        }
        StreamEvent::TextDelta { content, .. } => {
            handle_stream_content(s, content, false, output_timestamp);
        }
        StreamEvent::ReasoningDelta { content, .. } => {
            handle_stream_content(s, content, true, output_timestamp);
        }
        StreamEvent::Updates { node_id, state, .. } => {
            if verbose {
                match node_id.as_str() {
                    "understand" => {
                        if let Some(ref u) = state.understood {
                            eprintln!("--- Understanding ---");
                            eprintln!(
                                "  Core goal: {}",
                                truncate_display(&u.core_goal, display_max_len)
                            );
                            eprintln!("  Constraints: {:?}", u.key_constraints);
                            eprintln!(
                                "  Context: {}",
                                truncate_display(&u.relevant_context, display_max_len)
                            );
                        }
                    }
                    "plan" => s.turn += 1,
                    _ => {}
                }
                eprintln!("--- state after {} ---", node_id);
                eprintln!("{}", format_dup_state_display(state, display_max_len));
            } else if node_id == "plan" {
                s.turn += 1;
                if !state.core.tool_calls.is_empty() {
                    if let Some(sp) = s.spinner.take() {
                        sp.finish_box();
                    }
                    if let Some(tc) = state.core.tool_calls.first() {
                        let desc =
                            serde_json::from_str::<Value>(&tc.arguments)
                                .ok()
                                .and_then(|v| {
                                    v.get("description")
                                        .and_then(|d| d.as_str())
                                        .map(String::from)
                                });
                        let label = match desc {
                            Some(d) if !d.is_empty() => format!("{} - {}", tc.name, d),
                            _ => tc.name.clone(),
                        };
                        s.spinner = Some(Box::new(super::spinner::Spinner::new(label)));
                    }
                }
            }
        }
        StreamEvent::TurnFinish { usage, .. } => {
            s.accumulate_usage(usage.input, usage.output);
        }
        _ => {}
    }
}

fn on_event_tot(
    ev: &StreamEvent<TotState>,
    s: &mut EventState,
    display_max_len: usize,
    verbose: bool,
    output_timestamp: bool,
) {
    match ev {
        StreamEvent::TaskStart { node_id, .. } => {
            log_node_enter(s.last_node.as_deref(), node_id, verbose);
            s.last_node = Some(node_id.clone());
        }
        StreamEvent::TotExpand { candidates } => {
            if verbose {
                eprintln!("--- ToT expand: {} candidates ---", candidates.len());
                for (i, c) in candidates.iter().enumerate() {
                    eprintln!("  [{}] {}", i + 1, c);
                }
            }
        }
        StreamEvent::TotEvaluate { chosen, scores } => {
            if verbose {
                eprintln!(
                    "--- ToT evaluate: chosen={}, scores={:?} ---",
                    chosen, scores
                );
            }
        }
        StreamEvent::TotBacktrack { reason, to_depth } => {
            if verbose {
                eprintln!(
                    "--- ToT backtrack: reason={}, to_depth={} ---",
                    reason, to_depth
                );
            }
        }
        StreamEvent::TextDelta { content, .. } => {
            handle_stream_content(s, content, false, output_timestamp);
        }
        StreamEvent::ReasoningDelta { content, .. } => {
            handle_stream_content(s, content, true, output_timestamp);
        }
        StreamEvent::Updates { node_id, state, .. } => {
            if verbose {
                let label = match node_id.as_str() {
                    "think_expand" => "state after think_expand".to_string(),
                    "think_evaluate" => "state after think_evaluate".to_string(),
                    "act" => "state after act".to_string(),
                    "observe" => "state after observe".to_string(),
                    _ => format!("state after {}", node_id),
                };
                eprintln!("--- {} ---", label);
                eprintln!("{}", format_tot_state_display(state, display_max_len));
            } else if node_id == "act" && !state.core.tool_calls.is_empty() {
            }
        }
        StreamEvent::TurnFinish { usage, .. } => {
            s.accumulate_usage(usage.input, usage.output);
        }
        _ => {}
    }
}

#[derive(Default)]
struct EventState {
    turn: u32,
    last_node: Option<String>,
    /// When output_timestamp is true, we print timestamp once before the first reply chunk.
    reply_started: bool,
    /// Agent name (source) to print before first reply chunk when set.
    agent_display: Option<String>,
    /// Accumulated prompt tokens from all StreamEvent::TurnFinish in this run.
    total_prompt_tokens: u32,
    /// Accumulated completion tokens from all StreamEvent::TurnFinish in this run.
    total_completion_tokens: u32,
    /// Whether we're currently in a thinking state (for separator on transition).
    in_thinking: bool,
    /// Last prefill duration (for unified usage display).
    last_prefill_duration: Option<std::time::Duration>,
    /// Last decode duration (for unified usage display).
    last_decode_duration: Option<std::time::Duration>,
    /// Active spinner (if any). Created on TaskStart, finished when streaming begins.
    spinner: Option<Box<dyn super::spinner::SpinnerTrait>>,
    /// Tool names that were called in the current turn (for DONE lines).
    pending_tool_calls: Vec<ToolCall>,
    /// Tool results from the act node (saved before observe clears them).
    pending_tool_results: Vec<ToolResult>,
    /// Time when pending_tool_calls were received (for elapsed timing).
    pending_tool_start: Option<std::time::Instant>,
    /// Streaming markdown renderer for terminal output.
    markdown_renderer: StreamingMarkdownRenderer,
}

impl EventState {
    fn accumulate_usage(&mut self, input: u32, output: u32) {
        self.total_prompt_tokens = self.total_prompt_tokens.saturating_add(input);
        self.total_completion_tokens = self.total_completion_tokens.saturating_add(output);
        tracing::info!(input, output, total_tokens = input + output, "LLM usage");
    }
}

/// Prints loaded tools info to stderr at startup (structured panel format).
async fn print_loaded_tools(config: &agent::ReactBuildConfig) -> Result<(), RunError> {
    let ctx = build_react_run_context(config)
        .await
        .map_err(|e| RunError::Build(agent::BuildRunnerError::Context(e)))?;
    let tools = ctx.tool_source.list_tools().await;
    let names: Vec<&str> = tools.iter().map(|s| s.name.as_str()).collect();
    eprintln!("{}", panel_format::format_tools_line(&names));
    Ok(())
}

/// Prints each tool with its description, one per line (used at `-vv`).
/// Falls back to a single `(none)` line if no tools are loaded.
async fn print_tools_multiline(config: &agent::ReactBuildConfig) {
    if let Ok(ctx) = build_react_run_context(config).await {
        let tools = ctx.tool_source.list_tools().await;
        let rows: Vec<(&str, &str)> = tools
            .iter()
            .map(|s| {
                let desc = s.description.as_deref().unwrap_or("");
                (s.name.as_str(), desc)
            })
            .collect();
        eprintln!("{}", panel_format::format_tools_multiline_block(&rows));
    }
}

fn print_skills_one_line(registry: &std::sync::Arc<skill::SkillRegistry>) {
    let names: Vec<&str> = registry
        .list()
        .iter()
        .map(|e| e.metadata.name.as_str())
        .collect();
    eprintln!("{}", panel_format::format_skills_line(&names));
}

fn print_skills_multiline(registry: &std::sync::Arc<skill::SkillRegistry>) {
    let entries = registry.list();
    let owned: Vec<(String, &'static str, String, Vec<String>)> = entries
        .iter()
        .map(|e| {
            (
                e.metadata.name.clone(),
                e.source.label(),
                e.metadata.description.clone(),
                e.metadata.conditions().requires_tools.clone(),
            )
        })
        .collect();
    let refs: Vec<panel_format::SkillBannerRow<'_>> = owned
        .iter()
        .map(
            |(name, source, desc, requires)| panel_format::SkillBannerRow {
                name,
                source,
                description: desc.as_str(),
                requires_tools: requires.as_slice(),
            },
        )
        .collect();
    eprintln!("{}", panel_format::format_skills_multiline_block(&refs));
}

fn on_event_got(
    ev: &StreamEvent<GotState>,
    s: &mut EventState,
    display_max_len: usize,
    verbose: bool,
    output_timestamp: bool,
) {
    match ev {
        StreamEvent::TaskStart { node_id, .. } => {
            log_node_enter(s.last_node.as_deref(), node_id, verbose);
            s.last_node = Some(node_id.clone());
        }
        StreamEvent::GotPlan {
            node_count,
            edge_count,
            node_ids,
        } => {
            if verbose {
                eprintln!(
                    "--- GoT plan: {} nodes, {} edges ---",
                    node_count, edge_count
                );
                for id in node_ids {
                    eprintln!("  node: {}", id);
                }
            }
        }
        StreamEvent::GotNodeStart { node_id } => {
            if verbose {
                eprintln!("--- GoT node start: {} ---", node_id);
            }
        }
        StreamEvent::GotNodeComplete {
            node_id,
            result_summary,
        } => {
            if verbose {
                eprintln!("--- GoT node complete: {} ---", node_id);
                eprintln!("  result: {}", result_summary);
            }
        }
        StreamEvent::GotNodeFailed { node_id, error } => {
            if verbose {
                eprintln!("--- GoT node failed: {} ---", node_id);
                eprintln!("  error: {}", error);
            }
        }
        StreamEvent::GotExpand {
            node_id,
            nodes_added,
            edges_added,
        } => {
            if verbose {
                eprintln!(
                    "--- AGoT expand: {} ? +{} nodes, +{} edges ---",
                    node_id, nodes_added, edges_added
                );
            }
        }
        StreamEvent::TextDelta { content, .. } => {
            handle_stream_content(s, content, false, output_timestamp);
        }
        StreamEvent::ReasoningDelta { content, .. } => {
            handle_stream_content(s, content, true, output_timestamp);
        }
        StreamEvent::Updates { node_id, state, .. } => {
            if verbose {
                eprintln!("--- state after {} ---", node_id);
                eprintln!("{}", format_got_state_display(state, display_max_len));
            }
        }
        StreamEvent::TurnFinish { usage, .. } => {
            s.accumulate_usage(usage.input, usage.output);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent::run::RunCmd;
    use agent::{TaskGraph, TaskNode, TaskNodeState, TaskStatus, TotExtension, UnderstandOutput};

    use anureo_llm::{message::Message, ToolCall};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};

    fn react_state() -> ReActState {
        ReActState {
            messages: vec![Message::user("hi"), Message::assistant("hello")],
            ..ReActState::default()
        }
    }

    fn minimal_build_config() -> agent::ReactBuildConfig {
        agent::ReactBuildConfig {
            mcp_exa_url: "https://mcp.exa.ai/mcp".to_string(),
            mcp_remote_cmd: "npx".to_string(),
            mcp_remote_args: "-y mcp-remote".to_string(),
            mcp_github_cmd: "npx".to_string(),
            mcp_github_args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-github".to_string(),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn any_stream_event_to_format_a_and_protocol_format() {
        let ev = TypedAnyStreamEvent::React(StreamEvent::TaskStart {
            node_id: "think".to_string(),
            namespace: None,
        });
        let a = ev.to_format_a().unwrap();
        assert!(a.get("TaskStart").is_some());

        let mut state = EnvelopeState::new("sess-1".to_string());
        let p = ev.to_protocol_format(&mut state).unwrap();
        assert_eq!(p["type"], "node_enter");
        assert_eq!(p["id"], "think");
        assert_eq!(p["session_id"], "sess-1");
        assert_eq!(p["event_id"], 1);
    }

    #[test]
    fn on_event_react_updates_last_node_and_turn() {
        let mut s = EventState {
            turn: 0,
            last_node: None,
            reply_started: false,
            agent_display: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            in_thinking: false,
            last_prefill_duration: None,
            last_decode_duration: None,
            spinner: None,
            pending_tool_calls: Vec::new(),
            pending_tool_results: Vec::new(),
            pending_tool_start: None,
            ..EventState::default()
        };
        on_event_react(
            &StreamEvent::TaskStart {
                node_id: "think".to_string(),
                namespace: None,
            },
            &mut s,
            100,
            true,
            false,
        );
        assert_eq!(s.last_node.as_deref(), Some("think"));

        on_event_react(
            &StreamEvent::Updates {
                node_id: "think".to_string(),
                state: react_state(),
                namespace: None,
            },
            &mut s,
            100,
            true,
            false,
        );
        assert_eq!(s.turn, 1);
    }

    #[test]
    fn on_event_dup_and_tot_and_got_do_not_panic() {
        let mut s = EventState {
            turn: 0,
            last_node: None,
            reply_started: false,
            agent_display: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            in_thinking: false,
            last_prefill_duration: None,
            last_decode_duration: None,
            spinner: None,
            pending_tool_calls: Vec::new(),
            pending_tool_results: Vec::new(),
            pending_tool_start: None,
            ..EventState::default()
        };

        let dup_state = DupState {
            core: react_state(),
            understood: None,
        };
        on_event_dup(
            &StreamEvent::TaskStart {
                node_id: "understand".to_string(),
                namespace: None,
            },
            &mut s,
            120,
            true,
            false,
        );
        on_event_dup(
            &StreamEvent::Updates {
                node_id: "plan".to_string(),
                state: dup_state,
                namespace: None,
            },
            &mut s,
            120,
            true,
            false,
        );
        assert_eq!(s.turn, 1);

        let tot_state = TotState {
            core: react_state(),
            tot: TotExtension::default(),
        };
        on_event_tot(
            &StreamEvent::TaskStart {
                node_id: "think_expand".to_string(),
                namespace: None,
            },
            &mut s,
            120,
            true,
            false,
        );
        on_event_tot(
            &StreamEvent::TotExpand {
                candidates: vec!["a".to_string(), "b".to_string()],
            },
            &mut s,
            120,
            true,
            false,
        );
        on_event_tot(
            &StreamEvent::Updates {
                node_id: "observe".to_string(),
                state: tot_state,
                namespace: None,
            },
            &mut s,
            120,
            true,
            false,
        );

        let got_state = GotState {
            input_message: "q".to_string(),
            task_graph: TaskGraph {
                nodes: vec![TaskNode {
                    id: "n1".to_string(),
                    description: "d1".to_string(),
                    tool_calls: vec![ToolCall {
                        name: "search".to_string(),
                        arguments: "{}".to_string(),
                        id: None,
                    }],
                }],
                edges: vec![],
            },
            node_states: [(
                "n1".to_string(),
                TaskNodeState {
                    status: TaskStatus::Done,
                    result: Some("ok".to_string()),
                    error: None,
                },
            )]
            .into_iter()
            .collect(),
        };
        on_event_got(
            &StreamEvent::TaskStart {
                node_id: "plan_graph".to_string(),
                namespace: None,
            },
            &mut s,
            120,
            true,
            false,
        );
        on_event_got(
            &StreamEvent::GotPlan {
                node_count: 1,
                edge_count: 0,
                node_ids: vec!["n1".to_string()],
            },
            &mut s,
            120,
            true,
            false,
        );
        on_event_got(
            &StreamEvent::Updates {
                node_id: "execute_graph".to_string(),
                state: got_state,
                namespace: None,
            },
            &mut s,
            120,
            true,
            false,
        );
        assert_eq!(s.last_node.as_deref(), Some("plan_graph"));
    }

    #[test]
    fn non_verbose_paths_update_turns_without_panics() {
        let mut s = EventState {
            turn: 0,
            last_node: None,
            reply_started: false,
            agent_display: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            in_thinking: false,
            last_prefill_duration: None,
            last_decode_duration: None,
            spinner: None,
            pending_tool_calls: Vec::new(),
            pending_tool_results: Vec::new(),
            pending_tool_start: None,
            ..EventState::default()
        };
        let react_with_tool = ReActState {
            tool_calls: vec![ToolCall {
                name: "bash".to_string(),
                arguments: "{\"command\":\"echo hi\"}".to_string(),
                id: None,
            }],
            ..react_state()
        };
        on_event_react(
            &StreamEvent::Updates {
                node_id: "think".to_string(),
                state: react_with_tool,
                namespace: None,
            },
            &mut s,
            120,
            false,
            false,
        );
        assert_eq!(s.turn, 0);

        let dup_state = DupState {
            core: ReActState {
                tool_calls: vec![ToolCall {
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                    id: None,
                }],
                ..react_state()
            },
            understood: Some(UnderstandOutput {
                core_goal: "goal".to_string(),
                key_constraints: vec!["c1".to_string()],
                relevant_context: "ctx".to_string(),
            }),
        };
        on_event_dup(
            &StreamEvent::Updates {
                node_id: "plan".to_string(),
                state: dup_state,
                namespace: None,
            },
            &mut s,
            120,
            false,
            false,
        );
        assert_eq!(s.turn, 1);
    }

    #[test]
    fn verbose_tot_and_got_event_variants_are_handled() {
        let mut s = EventState {
            turn: 0,
            last_node: None,
            reply_started: false,
            agent_display: None,
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            in_thinking: false,
            last_prefill_duration: None,
            last_decode_duration: None,
            spinner: None,
            pending_tool_calls: Vec::new(),
            pending_tool_results: Vec::new(),
            pending_tool_start: None,
            ..EventState::default()
        };

        on_event_tot(
            &StreamEvent::TotEvaluate {
                chosen: 0,
                scores: vec![0.9],
            },
            &mut s,
            80,
            true,
            false,
        );
        on_event_tot(
            &StreamEvent::TotBacktrack {
                reason: "retry".to_string(),
                to_depth: 2,
            },
            &mut s,
            80,
            true,
            false,
        );
        on_event_tot(
            &StreamEvent::TextDelta {
                content: "tok".to_string(),
                metadata: stream_event::StreamMetadata {
                    anureo_node: "think_expand".to_string(),
                    namespace: None,
                },
            },
            &mut s,
            80,
            true,
            false,
        );

        on_event_got(
            &StreamEvent::GotNodeStart {
                node_id: "n1".to_string(),
            },
            &mut s,
            80,
            true,
            false,
        );
        on_event_got(
            &StreamEvent::GotNodeComplete {
                node_id: "n1".to_string(),
                result_summary: "done".to_string(),
            },
            &mut s,
            80,
            true,
            false,
        );
        on_event_got(
            &StreamEvent::GotNodeFailed {
                node_id: "n2".to_string(),
                error: "boom".to_string(),
            },
            &mut s,
            80,
            true,
            false,
        );
        on_event_got(
            &StreamEvent::GotExpand {
                node_id: "n1".to_string(),
                nodes_added: 2,
                edges_added: 1,
            },
            &mut s,
            80,
            true,
            false,
        );
        on_event_tot(
            &StreamEvent::TextDelta {
                content: "chunk".to_string(),
                metadata: stream_event::StreamMetadata {
                    anureo_node: "execute_graph".to_string(),
                    namespace: None,
                },
            },
            &mut s,
            80,
            true,
            false,
        );
    }

    #[tokio::test]
    async fn print_loaded_tools_succeeds_with_minimal_config() {
        let cfg = minimal_build_config();
        let res = print_loaded_tools(&cfg).await;
        assert!(res.is_ok());
    }

    fn invalid_opts(output_json: bool) -> agent::RunOptions {
        agent::RunOptions {
            message: anureo_llm::message::UserContent::text("hello".to_string()),
            // Deterministic failure path in build context (invalid file-tool root).
            working_folder: Some(PathBuf::from(
                "/definitely/not/exist/anureo-cli-run-agent-tests",
            )),
            session_id: None,
            cancellation: None,
            thread_id: None,
            agent: None,
            verbose: false,
            verbose_level: 0,
            got_adaptive: false,
            display_max_len: 200,
            output_json,
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
            default_extra_tools_provider: None,
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
        }
    }

    #[tokio::test]
    async fn run_agent_wrapper_errors_for_invalid_working_folder_plain_mode() {
        let res = run_agent_wrapper(&invalid_opts(false), &RunCmd::React, None).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn run_agent_wrapper_errors_for_invalid_working_folder_json_collect_mode() {
        let res = run_agent_wrapper(&invalid_opts(true), &RunCmd::React, None).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn run_agent_wrapper_errors_for_invalid_working_folder_json_stream_mode() {
        let sink: StreamCallback = Arc::new(Mutex::new(|_v: Value| {}));
        let res = run_agent_wrapper(&invalid_opts(true), &RunCmd::React, Some(sink)).await;
        assert!(res.is_err());
    }
}

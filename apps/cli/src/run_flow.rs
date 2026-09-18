//! Build run options and execute single-turn or interactive agent runs.

use std::sync::Arc;

use cli::RunOptions;
use tokio::sync::Notify;

use crate::args::{Args, Command};
use crate::display_limits::{generate_session_id, max_message_len};
use crate::output::{emit_run_output, make_stream_out, OutputConfig};
use crate::repl::{run_one_turn, run_repl_loop};
use anureo_llm::message::UserContent;

use config::default_model as config_default_model;

pub(crate) fn resolve_user_message(args: &Args) -> Option<String> {
    args.message.clone().or_else(|| {
        if args.rest.is_empty() {
            None
        } else {
            Some(args.rest.join(" "))
        }
    })
}

pub(crate) fn output_config(args: &Args) -> OutputConfig {
    OutputConfig {
        json: args.json,
        pretty: args.pretty,
        file: args.file.clone(),
    }
}

pub(crate) fn build_run_options(args: &Args, message: String, got_adaptive: bool) -> RunOptions {
    RunOptions {
        message: build_user_content_with_images(args, message),
        working_folder: args.working_folder.clone(),
        session_id: None,
        cancellation: None,
        thread_id: args.session_id.clone(),
        agent: args.agent.clone(),
        verbose: args.verbose >= 1,
        verbose_level: args.verbose,
        got_adaptive,
        display_max_len: max_message_len(),
        output_json: args.json,
        model: args.model.clone().or_else(|| Some(config_default_model())),
        mcp_config_path: args.mcp_config.clone(),
        output_timestamp: args.timestamp,
        dry_run: args.dry,
        debug_llm: args.debug_llm,
        provider: args.provider.clone(),
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
        worktree: args.worktree,
        goal_mode: false,
        acp_mcp_servers: None,
        acp_mcp_sources: None,
        effort: args.effort.clone(),
        tier: args.tier.clone(),
    }
}

pub fn validate_tier_arg(tier: &Option<String>) -> Result<(), String> {
    if let Some(tier_str) = tier {
        match tier_str.to_lowercase().as_str() {
            "light" | "standard" | "strong" => Ok(()),
            _ => Err(format!(
                "Invalid tier value: '{}'. Valid values are: light, standard, strong",
                tier_str
            )),
        }
    } else {
        Ok(())
    }
}

pub fn check_model_tier_conflict(args: &Args) -> Result<(), String> {
    if args.model.is_some() && args.tier.is_some() {
        Err(
            "Cannot specify both --model and --tier. Choose one method for model selection."
                .to_string(),
        )
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tier_tests {
    use super::*;

    #[test]
    fn test_validate_tier_valid_values() {
        assert!(validate_tier_arg(&Some("light".to_string())).is_ok());
        assert!(validate_tier_arg(&Some("standard".to_string())).is_ok());
        assert!(validate_tier_arg(&Some("strong".to_string())).is_ok());
        assert!(validate_tier_arg(&Some("LIGHT".to_string())).is_ok());
        assert!(validate_tier_arg(&Some("Standard".to_string())).is_ok());
        assert!(validate_tier_arg(&Some("STRONG".to_string())).is_ok());
    }

    #[test]
    fn test_validate_tier_invalid_values() {
        assert!(validate_tier_arg(&Some("invalid".to_string())).is_err());
        assert!(validate_tier_arg(&Some("medium".to_string())).is_err());
        assert!(validate_tier_arg(&Some("high".to_string())).is_err());
        assert!(validate_tier_arg(&Some("low".to_string())).is_err());
        assert!(validate_tier_arg(&Some("".to_string())).is_err());
    }

    #[test]
    fn test_validate_tier_none() {
        assert!(validate_tier_arg(&None).is_ok());
    }

    #[test]
    fn test_check_model_tier_conflict() {
        let mut args = Args {
            model: Some("gpt-4o".to_string()),
            tier: Some("light".to_string()),
            // ... other required fields
            ..Default::default()
        };
        assert!(check_model_tier_conflict(&args).is_err());

        args.model = None;
        assert!(check_model_tier_conflict(&args).is_ok());

        args.model = Some("gpt-4o".to_string());
        args.tier = None;
        assert!(check_model_tier_conflict(&args).is_ok());
    }

    #[test]
    fn test_check_model_tier_conflict_none() {
        let args = Args {
            model: None,
            tier: None,
            // ... other required fields
            ..Default::default()
        };
        assert!(check_model_tier_conflict(&args).is_ok());
    }
}

/// `--image` routing (priority #16 gap, Hermes parity `cli.py`).
///
/// Round-2 only declared the `image: Vec<PathBuf>` flag without
/// implementing routing; calling `anureo --image foo.png "describe this"`
/// was a no-op (the image was silently dropped). This function:
///   1. If `args.image` is empty, returns `UserContent::Text(message)`.
///   2. Otherwise probes the resolved model via
///      `decide_image_input_mode` (vision-capable or text fallback).
///   3. For the vision path, reads each file and base64-encodes as a
///      `ContentPart::ImageUrl` data URL (mirrors
///      `apps/acp/src/content.rs:323`).
///   4. For the text path, inlines a `[attached image: <name>]`
///      marker per file alongside the original message. The full
///      `vision_analyze` LLM round-trip is out of scope for a CLI
///      dispatch (the `run_flow` path doesn't carry a client); the
///      agent loop in `apps/cli/src/run/agent.rs` is the right place
///      for that fall-back once `--image` is wired through ACP. For
///      now we surface a clear message and degrade to text.
pub(crate) fn build_user_content_with_images(args: &Args, message: String) -> UserContent {
    if args.image.is_empty() {
        return UserContent::Text(message);
    }
    let mode = decide_image_input_mode(&args.model);
    match mode {
        ImageInputMode::Multimodal => build_multimodal(&args.image, message),
        ImageInputMode::TextFallback => build_text_fallback(&args.image, message),
    }
}

/// Routing decision: vision-capable vs. text fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ImageInputMode {
    /// Model advertises vision capability — emit `UserContent::Multimodal`
    /// with base64 data URLs.
    Multimodal,
    /// Model is text-only — inline markers and let the downstream
    /// agent loop call `vision_analyze` to convert image -> text.
    TextFallback,
}

/// Determine whether the resolved model supports vision. Used by
/// `build_user_content_with_images` and by tests.
///
/// Resolution order:
///   1. If `--model gpt-4o` / `--model claude-sonnet-4` / `--model gemini-2.0-*`
///      style names match the model catalog (TODO: integrate `ModelCatalog`),
///      consult that. For now we use a substring heuristic on the model id.
///   2. Otherwise default to TextFallback (the safe choice).
///
/// The heuristic is intentionally narrow — only well-known vision
/// prefixes trigger Multimodal. New models must be added to the
/// `VISION_MODEL_HINTS` slice, not the other way around; this avoids
/// accidentally inlining raw image bytes into a text-only model.
pub(crate) fn decide_image_input_mode(model_id: &Option<String>) -> ImageInputMode {
    const VISION_MODEL_HINTS: &[&str] = &[
        "gpt-4o",
        "gpt-4-vision",
        "gpt-5",
        "claude-3",
        "claude-4",
        "claude-sonnet-4",
        "claude-opus-4",
        "claude-haiku-4",
        "gemini-1.5",
        "gemini-2",
        "qwen-vl",
        "qwen2-vl",
        "qvq",
        "llava",
        "pixtral",
        "vision",
        "-v",
    ];
    let id = match model_id {
        Some(s) => s.to_ascii_lowercase(),
        None => return ImageInputMode::TextFallback,
    };
    if VISION_MODEL_HINTS.iter().any(|h| id.contains(h)) {
        ImageInputMode::Multimodal
    } else {
        ImageInputMode::TextFallback
    }
}

/// Construct a multimodal `UserContent` with one `ImageUrl` part per
/// file. Each file is read into memory and base64-encoded into a
/// `data:<mime>;base64,...` URL. Files larger than 8 MB are skipped
/// with a `tracing::warn!` rather than failing the whole call — the
/// user can retry with a smaller crop.
fn build_multimodal(images: &[std::path::PathBuf], message: String) -> UserContent {
    use base64::Engine as _;
    use anureo_llm::message::ContentPart;
    const MAX_BYTES: u64 = 8 * 1024 * 1024;
    let mut parts: Vec<ContentPart> = Vec::new();
    if !message.is_empty() {
        parts.push(ContentPart::Text { text: message });
    }
    for path in images {
        match std::fs::read(path) {
            Ok(bytes) if bytes.len() as u64 <= MAX_BYTES => {
                let mime = guess_mime(path);
                let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
                parts.push(ContentPart::ImageUrl {
                    url: format!("data:{};base64,{}", mime, encoded),
                    detail: None,
                });
            }
            Ok(_) => {
                tracing::warn!("image too large, skipping: {} (>8 MB)", path.display());
            }
            Err(e) => {
                tracing::warn!("failed to read image {}: {}", path.display(), e);
            }
        }
    }
    if parts.is_empty() {
        UserContent::Text(String::new())
    } else {
        UserContent::Multimodal(parts)
    }
}

/// Text-mode degradation: prepend a `[attached image: <name>]` marker
/// per file. The downstream ACP review-runner / agent loop is
/// responsible for calling `vision_analyze` if it wants richer
/// descriptions; here we just preserve the fact that an image was
/// attached.
fn build_text_fallback(images: &[std::path::PathBuf], message: String) -> UserContent {
    let mut buf = String::new();
    for path in images {
        buf.push_str(&format!(
            "[attached image: {}]\n",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<unknown>")
        ));
    }
    buf.push_str(&message);
    UserContent::Text(buf)
}

/// Minimal MIME guesser — three file extensions cover the common
/// cases (PNG/JPEG/WEBP). Anything else gets `application/octet-stream`
/// which is fine because the upstream LLM only uses the data URL for
/// opaque decoding anyway.
fn guess_mime(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}

fn print_session_status(session_id: Option<&str>, ended: bool, json: bool) {
    if json {
        return;
    }
    if let Some(session_id) = session_id {
        if ended {
            eprintln!("Session ended: {}", session_id);
            eprintln!(
                "  Hint: anureo session cat {}  |  anureo session list",
                session_id
            );
        } else {
            eprintln!("Session: {}", session_id);
        }
    }
}

fn ensure_session_id(opts: &mut RunOptions) {
    if opts.thread_id.is_none() {
        opts.thread_id = Some(generate_session_id());
    }
}

pub(crate) async fn run_single_turn_mode(
    opts: &mut RunOptions,
    cmd: &Command,
    reply_len: usize,
    output: &OutputConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_session_id(opts);
    print_session_status(opts.thread_id.as_deref(), false, output.json);
    let output_value = run_one_turn(opts, cmd, make_stream_out(output)).await?;
    emit_run_output(
        output_value,
        output,
        opts.thread_id.as_deref(),
        reply_len,
        opts.output_timestamp,
    )?;
    print_session_status(opts.thread_id.as_deref(), true, output.json);
    Ok(())
}

pub(crate) async fn run_interactive_mode(
    opts: &mut RunOptions,
    cmd: &Command,
    initial_message: Option<String>,
    reply_len: usize,
    output: &OutputConfig,
    force_quit: Arc<Notify>,
) -> Result<(), Box<dyn std::error::Error>> {
    ensure_session_id(opts);
    print_session_status(opts.thread_id.as_deref(), false, output.json);

    let stream_out = make_stream_out(output);
    if let Some(msg) = initial_message.filter(|msg| !msg.trim().is_empty()) {
        opts.message = UserContent::Text(msg);
        match run_one_turn(opts, cmd, stream_out.clone()).await {
            Ok(output_value) => emit_run_output(
                output_value,
                output,
                opts.thread_id.as_deref(),
                reply_len,
                opts.output_timestamp,
            )?,
            Err(err) => {
                eprintln!("error: {}", err);
                // Kanban-aware exit-code routing (Hermes parity,
                // `agent/cli.py` #1). When the caller's supervisor is
                // a Kanban orchestrator, surface a transient rate-limit
                // or billing error as EX_TEMPFAIL (75) so the
                // supervisor can re-enqueue the task instead of
                // marking it failed. Default UX (no `ANUREO_KANBAN_TASK`)
                // keeps the existing exit-1 behaviour so end users
                // aren't surprised by a different exit code in
                // scripts.
                if std::env::var_os("ANUREO_KANBAN_TASK").is_some() {
                    let msg = err.to_string().to_ascii_lowercase();
                    let is_transient = msg.contains("rate")
                        || msg.contains("429")
                        || msg.contains("too many requests")
                        || msg.contains("quota")
                        || msg.contains("billing")
                        || msg.contains("insufficient credit");
                    if is_transient {
                        std::process::exit(agent::goal_runner::state::KANBAN_RATE_LIMIT_EXIT_CODE);
                    }
                }
                std::process::exit(1);
            }
        }
    }

    run_repl_loop(opts, cmd, reply_len, output.clone(), stream_out, force_quit).await?;
    print_session_status(opts.thread_id.as_deref(), true, output.json);
    println!("Bye.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// `--effort` on the top-level `Args` must be plumbed through `build_run_options`
    /// into `RunOptions.effort` so the LLM client receives the value downstream.
    #[test]
    fn build_run_options_propagates_effort_flag() {
        let args = Args::parse_from(["anureo", "--effort", "high", "hello"]);
        let opts = build_run_options(&args, "hello".to_string(), false);
        assert_eq!(opts.effort.as_deref(), Some("high"));
    }

    /// `--effort auto` is treated as "use model default" downstream; we only verify
    /// the string flows through unchanged.
    #[test]
    fn build_run_options_propagates_effort_auto() {
        let args = Args::parse_from(["anureo", "--effort", "auto", "hi"]);
        let opts = build_run_options(&args, "hi".to_string(), false);
        assert_eq!(opts.effort.as_deref(), Some("auto"));
    }

    /// No `--effort` flag → `None` (preserves prior behaviour: don't send the
    /// `reasoning_effort` parameter to the API at all).
    #[test]
    fn build_run_options_default_effort_is_none() {
        let args = Args::parse_from(["anureo", "hi"]);
        let opts = build_run_options(&args, "hi".to_string(), false);
        assert!(opts.effort.is_none());
    }

    /// `--tier` on the top-level `Args` must be plumbed through `build_run_options`
    /// into `RunOptions.tier` so the model tier resolution works correctly.
    #[test]
    fn build_run_options_propagates_tier_flag() {
        let args = Args::parse_from(["anureo", "--tier", "light", "hello"]);
        let opts = build_run_options(&args, "hello".to_string(), false);
        assert_eq!(opts.tier.as_deref(), Some("light"));
    }

    /// `--tier strong` variant test.
    #[test]
    fn build_run_options_propagates_tier_strong() {
        let args = Args::parse_from(["anureo", "--tier", "strong", "hi"]);
        let opts = build_run_options(&args, "hi".to_string(), false);
        assert_eq!(opts.tier.as_deref(), Some("strong"));
    }

    /// No `--tier` flag → `None` (default behaviour).
    #[test]
    fn build_run_options_default_tier_is_none() {
        let args = Args::parse_from(["anureo", "hi"]);
        let opts = build_run_options(&args, "hi".to_string(), false);
        assert!(opts.tier.is_none());
    }
}

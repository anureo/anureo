//! Core sub-agent execution: profile → config → tier resolution → runner.

use std::sync::Arc;

use serde_json::Value;

use crate::agent::react::build::build_react_runner;
use crate::agent::react::tier_apply::resolve_tier_and_build_config;
use crate::agent::ReactBuildConfig;
use crate::profile::AgentProfile;
use crate::tools::agent::build_config::build_config_from_profile;
use crate::tools::agent::identity::SubagentIdentity;
use tool_core::{ToolCallContent, ToolCallContext, ToolSourceError};

/// Build sub-agent config, resolve model tier, construct ReactRunner, and execute.
///
/// This is the single shared path for all agent invocations (single, concurrent,
/// async). Callers handle their own pre-steps (depth check, worktree setup) and
/// pass the resolved `working_folder` here.
pub(super) async fn build_and_run_sub_agent(
    base_config: &Arc<ReactBuildConfig>,
    profile: &AgentProfile,
    task: &str,
    args: &Value,
    working_folder_override: Option<&std::path::Path>,
    ctx: Option<&ToolCallContext>,
    identity: &SubagentIdentity,
) -> Result<(ToolCallContent, super::registry::AgentCompletionStats), ToolSourceError> {
    tracing::info!(
        agent = %profile.name,
        task_length = task.len(),
        depth = identity.depth,
        invocation_id = %identity.invocation_id,
        "Starting execution of agent task"
    );
    // Note: depth check is performed by the caller (mod.rs).

    let agent_name = &profile.name;

    if let Some(folder) = working_folder_override {
        tracing::debug!(
            agent = %agent_name,
            working_folder = %folder.display(),
            "Using custom working folder"
        );
    }

    // --- build sub config ---
    tracing::debug!(
        agent = %agent_name,
        profile_name = %profile.name,
        "Building sub-agent configuration"
    );
    let mut sub_config = build_config_from_profile(profile, base_config, working_folder_override);

    tracing::debug!(
        agent = %agent_name,
        profile_name = %profile.name,
        profile_tier = ?sub_config.model_tier,
        parent_tier = ?base_config.model_tier,
        profile_model = ?sub_config.model,
        parent_model = ?base_config.model,
        profile_llm_provider = ?sub_config.llm_provider,
        parent_llm_provider = ?base_config.llm_provider,
        "Built sub-agent config from profile with model details"
    );

    // --- model_tier override ---
    if let Some(tier_str) = args.get("model_tier").and_then(|v| v.as_str()) {
        tracing::info!(
            agent = %agent_name,
            tier_override = %tier_str,
            current_profile_tier = ?sub_config.model_tier,
            current_profile_model = ?sub_config.model,
            "Processing model_tier override request"
        );
        match serde_json::from_str::<model_spec_core::ModelTier>(tier_str) {
            Ok(tier) => {
                tracing::info!(
                    agent = %agent_name,
                    old_tier = ?sub_config.model_tier,
                    new_tier = ?tier,
                    old_model = ?sub_config.model,
                    "Overriding model_tier from agent tool arguments"
                );
                sub_config.model_tier = Some(tier);
            }
            Err(e) => {
                tracing::warn!(
                    agent = %agent_name,
                    tier = %tier_str,
                    error = %e,
                    "Invalid model_tier format, ignoring override"
                );
            }
        }
    }

    // --- model override ---
    if let Some(model_str) = args.get("model").and_then(|v| v.as_str()) {
        if !model_str.is_empty() {
            tracing::info!(
                agent = %agent_name,
                model_override = %model_str,
                current_model = ?sub_config.model,
                "Overriding model from agent tool arguments"
            );
            sub_config.model = Some(model_str.to_string());
        }
    }

    // --- thread isolation ---
    // Child thread id comes from the invocation identity (contract §3.1):
    // includes a seq suffix so concurrent/repeated same-name invocations at
    // the same depth never collide (GAP-02).
    let sub_thread_id = identity.child_thread_id.clone();
    sub_config.thread_id = Some(sub_thread_id.clone());
    sub_config.trace_thread_id = base_config.trace_thread_id.clone();

    let depth = identity.depth;
    tracing::debug!(
        agent = %agent_name,
        thread_id = %sub_thread_id,
        depth = depth,
        "Configured sub-agent thread isolation"
    );

    // --- tier resolution ---
    tracing::debug!(
        agent = %agent_name,
        tier_to_resolve = ?sub_config.model_tier,
        current_model = ?sub_config.model,
        "Resolving tier and building final config"
    );
    let sub_config = resolve_tier_and_build_config(&sub_config).await;

    tracing::info!(
        agent = %agent_name,
        resolved_model = ?sub_config.model,
        resolved_provider = ?sub_config.llm_provider,
        resolved_base_url = ?sub_config.openai_base_url,
        tier_resolution_complete = true,
        "Model tier resolved successfully"
    );

    // --- build runner + run ---
    tracing::debug!(agent = %agent_name, "Building React runner");
    let child_depth = ctx.map_or(1u32, |c| c.depth + 1);
    let value_sender = ctx.and_then(|c| c.any_stream_event_sender.clone());
    let runner_sender = value_sender.as_ref().map(|vs| {
        let vs = vs.clone();
        Arc::new(move |ev: crate::run::TypedAnyStreamEvent| {
            if let Ok(value) = serde_json::to_value(&ev) {
                vs(value);
            }
        }) as Arc<dyn Fn(crate::run::TypedAnyStreamEvent) + Send + Sync>
    });
    let runner = build_react_runner(&sub_config, None, false, None, runner_sender)
        .await
        .map_err(|e| {
            tracing::error!(agent = %agent_name, error = %e, "Failed to build sub-agent runner");
            ToolSourceError::Transport(format!("failed to build sub-agent '{}': {}", agent_name, e))
        })?;

    tracing::debug!(agent = %agent_name, "Starting sub-agent execution");
    let agent_name_for_event = agent_name.to_string();
    let start = std::time::Instant::now();

    let on_event = Some(
        move |event: stream_event::StreamEvent<crate::state::ReActState>| match &event {
            stream_event::StreamEvent::TaskStart { .. }
            | stream_event::StreamEvent::TaskEnd { .. }
            | stream_event::StreamEvent::ToolStart { .. }
            | stream_event::StreamEvent::ToolEnd { .. } => {
                if let Some(formatted) = crate::subagent_display::format_subagent_event(
                    &event,
                    &agent_name_for_event,
                    child_depth,
                    start,
                ) {
                    eprintln!("{}", formatted);
                }
            }
            _ => {}
        },
    );

    let outcome = runner
        .stream_with_config(task, None, on_event)
        .await
        .map_err(|e| {
            tracing::error!(agent = %agent_name, error = %e, "Sub-agent execution failed");
            ToolSourceError::Transport(format!("sub-agent '{}' failed: {}", agent_name, e))
        })?;

    let (reply, stats) = match outcome {
        crate::runner_common::StreamRunOutcome::Finished(final_state) => {
            let reply = final_state
                .last_assistant_reply()
                .unwrap_or_else(|| "(no reply from sub-agent)".to_string());
            let stats = super::registry::AgentCompletionStats {
                turn_count: final_state.turn_count,
                total_tokens: final_state
                    .total_usage
                    .as_ref()
                    .map(|u| u.total_tokens)
                    .unwrap_or(0),
                tool_calls_count: final_state.tool_calls.len() as u32,
            };
            tracing::info!(
                agent = %agent_name,
                reply_length = reply.len(),
                turn_count = stats.turn_count,
                total_tokens = stats.total_tokens,
                tool_calls_count = stats.tool_calls_count,
                "Sub-agent completed successfully"
            );
            (reply, stats)
        }
        crate::runner_common::StreamRunOutcome::Cancelled => {
            tracing::warn!(agent = %agent_name, "Sub-agent was cancelled");
            return Err(ToolSourceError::Transport(format!(
                "sub-agent '{}' was cancelled",
                agent_name
            )));
        }
    };

    Ok((ToolCallContent::text(reply), stats))
}

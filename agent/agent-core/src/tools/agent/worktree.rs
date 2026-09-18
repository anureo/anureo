//! Worktree isolation lifecycle management for sub-agent invocations.

use std::sync::Arc;

use serde_json::Value;

use crate::agent::ReactBuildConfig;
use crate::profile::{list_available_profiles, resolve_profile};
use crate::tools::agent::identity::SubagentIdentity;
use tool_core::{ToolCallContent, ToolCallContext, ToolSourceError};

use super::runner::build_and_run_sub_agent;

/// Invoke a single agent in batch context.
///
/// Handles worktree isolation, then delegates to
/// [`build_and_run_sub_agent`] for the shared execution path.
pub(super) async fn invoke_single_agent(
    base_config: &Arc<ReactBuildConfig>,
    args: Value,
    ctx: Option<&ToolCallContext>,
    identity: &SubagentIdentity,
) -> Result<(ToolCallContent, super::registry::AgentCompletionStats), ToolSourceError> {
    let agent_name = args
        .get("agent")
        .and_then(|v| v.as_str())
        .unwrap_or("default");

    let task = args
        .get("task")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolSourceError::InvalidInput("missing required argument: task".into()))?;

    // Parse isolation and estimated_paths from tool call arguments
    let isolation_arg = args
        .get("isolation")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let estimated_paths: Vec<String> = args
        .get("estimated_paths")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    let current_depth = identity.depth;

    tracing::info!(
        agent = %agent_name,
        task_length = task.len(),
        depth = current_depth,
        invocation_id = %identity.invocation_id,
        isolation = ?isolation_arg,
        estimated_paths_count = estimated_paths.len(),
        "Starting single agent invocation"
    );

    // --- resolve profile (needed for worktree config) ---
    tracing::debug!(agent = %agent_name, "Resolving agent profile");
    let profile = resolve_profile(agent_name).map_err(|e| {
        let available = list_available_profiles();
        let names = if available.is_empty() {
            "(no profiles discovered)".to_string()
        } else {
            available
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        tracing::error!(agent = %agent_name, error = %e, "Failed to resolve agent profile");
        ToolSourceError::InvalidInput(format!(
            "failed to resolve agent '{}': {}. Available agents: {}",
            agent_name, e, names
        ))
    })?;

    // --- worktree isolation ---
    let use_worktree = isolation_arg.as_deref() == Some("worktree")
        || (isolation_arg.is_none() && profile.isolation.as_deref() == Some("worktree"));

    let worktree_handle = if use_worktree {
        let worktree_config = profile
            .worktree
            .as_ref()
            .map(|wc| wc.to_worktree_config())
            .unwrap_or_default();
        let current_dir = base_config
            .working_folder
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("."));

        match worktree::WorktreeManager::from_working_dir(current_dir, worktree_config) {
            Ok(manager) => {
                match manager
                    .create_for_agent(
                        agent_name,
                        None,
                        if estimated_paths.is_empty() {
                            None
                        } else {
                            Some(&estimated_paths)
                        },
                    )
                    .await
                {
                    Ok(handle) => {
                        tracing::info!(
                            agent = %agent_name,
                            worktree_path = %handle.path.display(),
                            branch = ?handle.branch,
                            "Created worktree for isolated agent execution"
                        );
                        Some(handle)
                    }
                    Err(e) => {
                        tracing::warn!(
                            agent = %agent_name,
                            error = %e,
                            "Failed to create worktree, falling back to shared directory"
                        );
                        None
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    agent = %agent_name,
                    error = %e,
                    "Cannot init worktree manager, falling back to shared directory"
                );
                None
            }
        }
    } else {
        None
    };

    let effective_working_folder = worktree_handle.as_ref().map(|h| h.path.clone());

    // --- delegate to shared execution ---
    let (mut content, stats) = build_and_run_sub_agent(
        base_config,
        &profile,
        task,
        &args,
        effective_working_folder.as_deref(),
        ctx,
        identity,
    )
    .await?;

    // --- worktree cleanup + placement info (T6) ---
    let mut worktree_note = String::new();
    if let Some(handle) = worktree_handle {
        let path_str = handle.path.display().to_string();
        let branch_str = format!("{:?}", handle.branch);
        let wt_config = profile
            .worktree
            .as_ref()
            .map(|wc| wc.to_worktree_config())
            .unwrap_or_default();
        let manager = worktree::WorktreeManager::new(handle.repo_root.clone(), wt_config.clone());
        let has_changes = manager.check_changes(&handle).await.unwrap_or(false);

        let mut preserved = true;
        if !has_changes && wt_config.auto_cleanup {
            preserved = false;
            tracing::info!(
                agent = %agent_name,
                worktree_path = %handle.path.display(),
                "Auto-cleaning worktree (no changes)"
            );
            if let Err(e) = manager.cleanup(handle).await {
                tracing::warn!(agent = %agent_name, error = %e, "Worktree cleanup failed");
            }
        } else if has_changes {
            tracing::info!(
                agent = %agent_name,
                worktree_path = %handle.path.display(),
                branch = ?handle.branch,
                "Worktree has changes — preserving for review/merge"
            );
        }
        // T6: surface placement + preservation to the parent model — it cannot
        // merge what it cannot locate.
        worktree_note = format!(
            "\n\n[worktree] path={}, branch={}, changes_preserved={}",
            path_str, branch_str, preserved
        );
    }
    if !worktree_note.is_empty() {
        if let Some(text) = content.as_text() {
            let text = text.to_string();
            content = ToolCallContent::text(format!("{text}{worktree_note}"));
        }
    }

    Ok((content, stats))
}

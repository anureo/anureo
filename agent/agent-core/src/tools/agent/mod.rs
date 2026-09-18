//! AgentTool: invoke a sub-agent by profile name.
//!
//! ```json
//! {"agent": "dev", "task": "Implement login API", "background": false}
//! ```
//! With `background: true`, starts in background and returns an `agent_id`:
//! ```json
//! {"agent": "dev", "task": "...", "background": true}
//! → {"agent_id": "sub-root-dev-0-3", "status": "running"}
//! ```
//! Use the `agent_get` tool to retrieve results from background agents.

pub mod build_config;
pub mod identity;
pub mod lifecycle;
pub mod registry;
mod runner;
mod worktree;

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::agent::ReactBuildConfig;
use crate::profile::list_available_profiles;
use crate::tool_output_normalizer::{ToolOutputHint, ToolOutputStrategy};
use tool_core::{Tool, ToolCallContent, ToolCallContext, ToolSourceError, ToolSpec};

pub use tool_core::tool_name::TOOL_AGENT;
pub use identity::SubagentIdentity;

pub(super) const DEFAULT_MAX_DEPTH: u32 = 3;

pub struct AgentTool {
    pub(super) base_config: Arc<ReactBuildConfig>,
    pub(super) max_depth: u32,
    pub(super) registry: registry::AsyncAgentRegistry,
}

impl AgentTool {
    pub fn new(
        base_config: Arc<ReactBuildConfig>,
        max_depth: Option<u32>,
        registry: registry::AsyncAgentRegistry,
    ) -> Self {
        Self {
            base_config,
            max_depth: max_depth.unwrap_or(DEFAULT_MAX_DEPTH),
            registry,
        }
    }

    fn available_agents_description(&self) -> String {
        let profiles = list_available_profiles();
        if profiles.is_empty() {
            return String::new();
        }
        let mut lines = vec![String::from("\n\nAvailable agents:")];
        for p in &profiles {
            let desc = p.description.as_deref().unwrap_or("(no description)");
            lines.push(format!("  - {}: {}", p.name, desc));
        }
        lines.join("\n")
    }

    /// Spawn the agent execution as a tokio task.
    ///
    /// Lifecycle order (contract §4): `created` before spawn (failure aborts
    /// the invocation — the RAII reservation Drop removes the entry),
    /// `running` after commit, `terminal` exactly once from the registry's
    /// terminal guard.
    async fn spawn_agent_task(
        &self,
        identity: Arc<SubagentIdentity>,
        args: Value,
        ctx: Option<&ToolCallContext>,
    ) -> Result<
        JoinHandle<Result<(ToolCallContent, registry::AgentCompletionStats), ToolSourceError>>,
        ToolSourceError,
    > {
        let base_config = self.base_config.clone();
        let ctx_clone = ctx.cloned();
        let registry = self.registry.clone();
        let agent_id = identity.invocation_id.clone();
        let identity_for_task = Arc::clone(&identity);

        // Reserve BEFORE spawn (A1). Drop without commit removes the entry.
        let thread_id = self
            .base_config
            .thread_id
            .clone()
            .unwrap_or_else(|| "root".to_string());
        let reservation = self.registry.reserve(Arc::clone(&identity), thread_id);
        debug_assert_eq!(reservation.agent_id(), &agent_id);

        // Lifecycle: created before spawn. Failure aborts the invocation;
        // the un-committed reservation removes the entry.
        if let Err(e) = self.registry.emit_created(&identity).await {
            return Err(ToolSourceError::Transport(format!(
                "sub-agent lifecycle 'created' rejected: {}",
                e
            )));
        }

        let handle = tokio::spawn(async move {
            // A2: hold an execution guard for the task's real lifetime. On
            // failure the invocation errors out (the un-committed reservation
            // removes the entry).
            let Some(_execution_guard) = registry.try_begin_execution() else {
                return Err(ToolSourceError::InvalidInput(format!(
                    "concurrent sub-agent execution limit reached ({}/{} running); \
                     wait for agents to finish (agent_get) or cancel one (agent_cancel)",
                    registry.active_executions(),
                    registry.max_concurrent()
                )));
            };

            tracing::info!(
                agent = %identity_for_task.agent_name,
                agent_id = %identity_for_task.invocation_id,
                child_session_id = %identity_for_task.child_session_id,
                child_thread_id = %identity_for_task.child_thread_id,
                depth = identity_for_task.depth,
                "Starting agent execution"
            );
            let result = worktree::invoke_single_agent(
                &base_config,
                args,
                ctx_clone.as_ref(),
                &identity_for_task,
            )
            .await;

            // Update registry with result.
            match &result {
                Ok((content, stats)) => {
                    let text = content.as_text().unwrap_or("").to_string();
                    registry.complete(&agent_id, text, stats.clone());
                }
                Err(e) => {
                    let stats = registry::AgentCompletionStats::default();
                    registry.fail(&agent_id, e.to_string(), stats);
                }
            }
            result
        });

        // Commit the reservation with the AbortHandle for real cancellation.
        let abort_handle = handle.abort_handle();
        reservation.commit(Some(abort_handle));

        // Lifecycle: running once the task is live and committed.
        self.registry.emit_running(&identity).await;

        Ok(handle)
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        TOOL_AGENT
    }

    fn spec(&self) -> ToolSpec {
        let agents_desc = self.available_agents_description();
        let description = format!(
            "Delegate work to a sub-agent by profile name. Each sub-agent runs a full ReAct loop \
             with its own tools and system prompt, then returns a final reply.\n\
             \n\
             Required: `task` (full context; sub-agents have no memory of the current conversation).\n\
             Optional: `agent` (profile name) — if omitted, uses the built-in `default` profile \
             (alias of `assistant`, a general-purpose agent with full tool access). \
             Default: \"default\".\n\
             \n\
             You may issue multiple `agent` calls in the same turn to run sub-agents in parallel.\n\
             \n\
             Optional: `background` (bool) — if true, starts agent in background and returns \
             `agent_id` immediately. Use `agent_get` tool to retrieve results. Default: false.\n\
             \n\
             Optional: `timeout` (number) — sync wait timeout in seconds (ignored when \
             `background` is true). If a sync call exceeds this time, the agent transitions to \
             background execution and returns `agent_id`. Default: 600.{}",
            agents_desc,
        );
        ToolSpec {
            name: TOOL_AGENT.to_string(),
            description: Some(description),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "agent": {
                        "type": "string",
                        "description": "Agent profile name or path to profile directory. If omitted, uses the built-in 'default' profile (alias of 'assistant')."
                    },
                    "task": {
                        "type": "string",
                        "description": "Natural-language task to delegate; include full context."
                    },
                    "background": {
                        "type": "boolean",
                        "description": "If true, start the agent in the background and return immediately with an agent_id. Use agent_get to retrieve the result later. Default: false.",
                        "default": false
                    },
                    "timeout": {
                        "type": "number",
                        "description": "Timeout in seconds. If a synchronous call exceeds this time, the agent transitions to background execution and returns an agent_id. Default: 600.",
                        "default": 600
                    },
                    "working_folder": {
                        "type": "string",
                        "description": "Override the working folder for this sub-agent."
                    },
                    "model_tier": {
                        "type": "string",
                        "enum": model_spec_core::ModelTier::variants().to_vec(),
                        "description": "Override the agent's model tier for this invocation."
                    },
                    "model": {
                        "type": "string",
                        "description": "Override the agent's model name for this invocation. Takes precedence over model_tier."
                    },
                    "isolation": {
                        "type": "string",
                        "description": "worktree: create an isolated git worktree. none: explicitly disable worktree even if profile configures it. If omitted, uses profile default.",
                        "enum": ["worktree", "none"]
                    },
                    "estimated_paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional hint: paths the task is expected to touch. Speeds up worktree isolation when `isolation: worktree` is used. Parsed by the runner even though it was missing from this schema (T2)."
                    }
                },
                "required": ["task"]
            }),
            output_hint: Some(ToolOutputHint::preferred(ToolOutputStrategy::SummaryOnly)),
        }
    }

    async fn call(
        &self,
        args: Value,
        ctx: Option<&ToolCallContext>,
    ) -> Result<ToolCallContent, ToolSourceError> {
        let agent_name = args
            .get("agent")
            .and_then(|v| v.as_str())
            .unwrap_or("default")
            .to_string();
        let task = args.get("task").and_then(|v| v.as_str()).ok_or_else(|| {
            ToolSourceError::InvalidInput("missing required argument: task".into())
        })?;
        let background = args
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let current_depth = ctx.map(|c| c.depth).unwrap_or(0);

        tracing::info!(
            agent = %agent_name,
            task_length = task.len(),
            background = background,
            depth = current_depth,
            "agent invoke"
        );

        if current_depth >= self.max_depth {
            tracing::warn!(
                agent = %agent_name,
                current_depth = current_depth,
                max_depth = self.max_depth,
                "Max depth reached, refusing agent invocation"
            );
            return Err(ToolSourceError::InvalidInput(format!(
                "max sub-agent depth ({}) reached; solve the task yourself instead of delegating further",
                self.max_depth,
            )));
        }

        // A2: fail fast when the concurrent-execution budget is exhausted.
        // The real guard is acquired inside the spawned task (held across the
        // sync→background transition; released on true terminal).
        if !self.registry.execution_available() {
            return Err(ToolSourceError::InvalidInput(format!(
                "concurrent sub-agent execution limit reached ({}/{} running); \
                 wait for agents to finish (agent_get) or cancel one (agent_cancel) before spawning more",
                self.registry.active_executions(),
                self.registry.max_concurrent(),
            )));
        }

        let identity = Arc::new(SubagentIdentity::generate(ctx, &agent_name));
        let agent_id = identity.invocation_id.clone();

        if background {
            // Reserve → created → spawn → commit (RAII reservation).
            self.spawn_agent_task(Arc::clone(&identity), args, ctx)
                .await?;
            let response = serde_json::json!({
                "agent_id": agent_id,
                "status": "running"
            });
            Ok(ToolCallContent::text(response.to_string()))
        } else {
            // Reserve → created → spawn → wait with timeout.
            let timeout_sec = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(600);
            let handle = self.spawn_agent_task(identity, args, ctx).await?;

            match tokio::time::timeout(std::time::Duration::from_secs(timeout_sec), handle).await {
                Ok(Ok(Ok((content, stats)))) => {
                    // T5: sync path keeps the completion stats instead of dropping them.
                    let mut text = content.as_text().unwrap_or_default().to_string();
                    text.push_str(&format!(
                        "\n\n[sub-agent stats] turn_count={}, total_tokens={}, tool_calls_count={}",
                        stats.turn_count, stats.total_tokens, stats.tool_calls_count
                    ));
                    Ok(ToolCallContent::text(text))
                }
                Ok(Ok(Err(e))) => Err(e),
                Ok(Err(_)) => {
                    // Task panicked.
                    self.registry.fail(
                        &agent_id,
                        "task panicked".into(),
                        registry::AgentCompletionStats::default(),
                    );
                    Err(ToolSourceError::Transport("sub-agent task panicked".into()))
                }
                Err(_) => {
                    // Timeout — transition to background.
                    // The task continues running; we just return the agent_id.
                    self.registry.mark_background(&agent_id);
                    let response = serde_json::json!({
                        "agent_id": agent_id,
                        "status": "background",
                        "message": format!("timeout after {}s, continuing in background", timeout_sec)
                    });
                    Ok(ToolCallContent::text(response.to_string()))
                }
            }
        }
    }
}

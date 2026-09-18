use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::ReactBuildConfig;
use crate::run::TypedAnyStreamEvent;
use tool_core::active_operation::RunCancellation;

/// Default working folder when not set (current directory).
pub const DEFAULT_WORKING_FOLDER: &str = ".";

/// Provider for default extra tools (e.g. the workflow tool) that should be
/// registered with every agent invocation.
///
/// `build_react_config` calls the provider with the resolved `ReactBuildConfig`
/// (after profile + env merging) so the tool factory can bind itself to the
/// working folder, model, and other derived settings. The returned tools are
/// merged into the agent's `extra_tools` slot and — critically — their
/// `builtin_skill()` hooks run *before* the skill registry is finalized, so
/// the workflow tool's `workflow` skill lands in the registry.
///
/// Without this provider, a tool that ships a builtin skill cannot make the
/// skill visible to the LLM: `build_react_config` is the single point that
/// assembles both the tool source and the skill registry, and any tool added
/// after it returns is invisible to the skill layer.
pub type ExtraToolsProvider =
    Arc<dyn Fn(&ReactBuildConfig) -> Vec<Arc<dyn tool_core::Tool>> + Send + Sync>;

/// Resolved model + provider configuration from a model string like "openai/gpt-4o".
#[derive(Debug, Clone, Default)]
pub struct ResolvedModelConfig {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub provider_type: Option<String>,
    pub effort: Option<String>,
    pub tier: Option<String>,
}

/// Options for running an agent.
pub struct RunOptions {
    pub message: anureo_llm::message::UserContent,
    pub working_folder: Option<PathBuf>,
    pub session_id: Option<String>,
    pub cancellation: Option<RunCancellation>,
    pub thread_id: Option<String>,
    pub agent: Option<String>,
    pub verbose: bool,
    /// Raw `-v` / `-vv` count from CLI. Level 0 = minimal, 1 = +skills one-liner
    /// + runtime details, 2+ = also multiline tools/skills with sources. Only the
    ///   CLI startup banner uses this; downstream runtime stays on `verbose: bool`.
    pub verbose_level: u8,
    pub got_adaptive: bool,
    pub display_max_len: usize,
    pub output_json: bool,
    pub model: Option<String>,
    pub mcp_config_path: Option<PathBuf>,
    pub output_timestamp: bool,
    pub dry_run: bool,
    pub debug_llm: bool,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub provider_type: Option<String>,
    pub any_stream_event_sender: Option<Arc<dyn Fn(TypedAnyStreamEvent) + Send + Sync>>,
    pub bash_executor: Option<Arc<dyn tool_basic::bash::CommandExecutor>>,
    pub extra_tools: Option<Arc<Vec<Arc<dyn tool_core::Tool>>>>,
    /// Provider for default extra tools (e.g. the workflow tool) that ship
    /// with anureo. See [`ExtraToolsProvider`] for the design rationale. CLI
    /// entry points set this once; `build_react_config` consumes it.
    pub default_extra_tools_provider: Option<ExtraToolsProvider>,
    pub acp_session_id: Option<String>,
    /// Shared sub-agent registry (Phase 2 seam). `None` = per-runner default.
    /// ACP injects a server-level instance so sub-agents are addressable
    /// across sessions.
    pub async_agent_registry: Option<crate::tools::AsyncAgentRegistry>,
    /// Lifecycle bridge for sub-agent invocations (contract §4). Attached to
    /// the registry built from this config; the ACP implementation registers
    /// durable child sessions.
    pub subagent_sink:
        Option<Arc<dyn crate::tools::agent::lifecycle::SubagentLifecycleSink>>,
    pub force_compact: bool,
    pub chat_id: Option<i64>,
    pub worktree: bool,
    pub goal_mode: bool,
    pub acp_mcp_servers: Option<Vec<env_config::McpServerDef>>,
    /// Already-connected ACP MCP sources reused for this session.
    pub acp_mcp_sources: Option<Vec<(String, Arc<tool_basic::McpToolSource>)>>,
    pub effort: Option<String>,
    pub tier: Option<String>,
}

impl std::fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOptions")
            .field("message", &"<UserContent>")
            .field("working_folder", &self.working_folder)
            .field("model", &self.model)
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("thread_id", &self.thread_id)
            .field("dry_run", &self.dry_run)
            .field("tier", &self.tier)
            .finish_non_exhaustive()
    }
}

impl Clone for RunOptions {
    fn clone(&self) -> Self {
        Self {
            message: self.message.clone(),
            working_folder: self.working_folder.clone(),
            session_id: self.session_id.clone(),
            cancellation: self.cancellation.clone(),
            thread_id: self.thread_id.clone(),
            agent: self.agent.clone(),
            verbose: self.verbose,
            verbose_level: self.verbose_level,
            got_adaptive: self.got_adaptive,
            display_max_len: self.display_max_len,
            output_json: self.output_json,
            model: self.model.clone(),
            mcp_config_path: self.mcp_config_path.clone(),
            output_timestamp: self.output_timestamp,
            dry_run: self.dry_run,
            debug_llm: self.debug_llm,
            provider: self.provider.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            provider_type: self.provider_type.clone(),
            any_stream_event_sender: self.any_stream_event_sender.clone(),
            bash_executor: self.bash_executor.clone(),
            extra_tools: self.extra_tools.clone(),
            default_extra_tools_provider: self.default_extra_tools_provider.clone(),
            acp_session_id: self.acp_session_id.clone(),
            async_agent_registry: self.async_agent_registry.clone(),
            subagent_sink: self.subagent_sink.clone(),
            force_compact: self.force_compact,
            chat_id: self.chat_id,
            worktree: self.worktree,
            goal_mode: self.goal_mode,
            acp_mcp_servers: self.acp_mcp_servers.clone(),
            acp_mcp_sources: self.acp_mcp_sources.clone(),
            effort: self.effort.clone(),
            tier: self.tier.clone(),
        }
    }
}

/// Result of a completed agent run.
#[derive(Debug, Clone)]
pub enum RunCompletion {
    Finished(AgentRunResult),
    Cancelled,
}

/// Result of a successfully completed agent run.
#[derive(Debug, Clone)]
pub struct AgentRunResult {
    pub reply: String,
    pub reasoning_content: Option<String>,
}

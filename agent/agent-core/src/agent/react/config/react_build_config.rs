use env_config::McpServerDef;
use std::path::PathBuf;
use std::sync::Arc;

use super::env_context::EnvContext;
use super::runner_config::{GotRunnerConfig, TotRunnerConfig};
use crate::compress::CompactionConfig;
use model_spec_core::ModelTier;
use skill::SkillRegistry;
use tool_core::BuiltinToolFilter;

#[derive(Clone)]
pub struct ReactBuildConfig {
    pub db_path: Option<String>,
    pub thread_id: Option<String>,
    pub trace_thread_id: Option<String>,
    pub user_id: Option<String>,
    pub system_prompt: Option<String>,
    pub exa_api_key: Option<String>,
    pub exa_codesearch_enabled: bool,
    pub mcp_exa_url: String,
    pub mcp_remote_cmd: String,
    pub mcp_remote_args: String,
    pub github_token: Option<String>,
    pub mcp_github_cmd: String,
    pub mcp_github_args: Vec<String>,
    pub mcp_github_url: Option<String>,
    pub mcp_verbose: bool,
    pub openai_api_key: Option<String>,
    pub openai_base_url: Option<String>,
    pub model: Option<String>,
    pub model_tier: Option<ModelTier>,
    pub parent_model_hint: Option<String>,
    pub aux_model: Option<String>,
    pub llm_provider: Option<String>,
    pub llm_provider_name: Option<String>,
    /// Hermes-aligned curator provider override (`curator.py:1250-1410`).
    /// When `Some`, takes precedence over `aux_model` for the curator LLM
    /// pass. `None` (or `Some("")`) falls through to the next tier.
    pub curator_provider: Option<String>,
    /// Hermes-aligned curator model override (`curator.py:1250-1410`).
    /// When `Some`, takes precedence over `aux_model` for the curator LLM
    /// pass. `None` (or `Some("")`) falls through to the next tier.
    pub curator_model: Option<String>,
    /// Hermes-aligned curator api-key override (`curator.py:1250-1410`).
    /// When `Some`, takes precedence over the global `openai_api_key` for
    /// the curator LLM pass.
    pub curator_api_key: Option<String>,
    /// Hermes-aligned curator base-url override (`curator.py:1250-1410`).
    /// When `Some`, takes precedence over the global `openai_base_url` for
    /// the curator LLM pass.
    pub curator_base_url: Option<String>,
    pub openai_temperature: Option<String>,
    pub embedding_api_key: Option<String>,
    pub embedding_base_url: Option<String>,
    pub embedding_model: Option<String>,
    pub working_folder: Option<PathBuf>,
    pub allow_paths_outside_workdir: bool,
    pub compaction_config: Option<CompactionConfig>,
    pub tot_config: TotRunnerConfig,
    pub got_config: GotRunnerConfig,
    pub mcp_servers: Option<Vec<McpServerDef>>,
    pub acp_mcp_sources: Option<Vec<(String, Arc<tool_basic::McpToolSource>)>>,
    pub skill_registry: Option<Arc<SkillRegistry>>,
    pub max_sub_agent_depth: Option<u32>,
    /// Invocation depth of the session built from this config (0 = top
    /// level). Cloned into sub configs and threaded through
    /// `build_runnable_config` into `RunnableConfig::depth` →
    /// `ToolCallContext::depth`, which is what the `agent` tool's max-depth
    /// guard reads. Previously this was hardcoded `None` in the checkpointer,
    /// leaving the guard dead (GAP-06).
    pub depth: Option<u32>,
    /// Shared sub-agent registry (A2/Phase-2 seam). `None` = each runner
    /// build creates its own (legacy behavior). ACP injects a server-level
    /// instance so sub-agents are addressable across sessions.
    pub async_agent_registry: Option<crate::tools::AsyncAgentRegistry>,
    /// Max concurrently *executing* sub-agents (codex AgentExecutionLimiter
    /// analog, evolution A2). `None` = env `MAX_CONCURRENT_SUBAGENTS` or 4.
    pub max_concurrent_subagents: Option<u32>,
    /// Curator-LLM-pass recursion limit (Hermes parity, `agent/curator.py`
    /// #15). When `Some(n)`, the curator's ObserveNode is constructed
    /// with `with_loop_max_turns(n)` so a runaway loop can never run
    /// forever. `None` (current default) leaves ObserveNode in its
    /// `with_loop()` unlimited state — `Agent::from_config` reads this
    /// field to apply the cap once plumbed.
    pub curator_max_iterations: Option<u32>,
    pub dry_run: bool,
    pub builtin_tool_filter: Option<BuiltinToolFilter>,
    pub call_tool_filter: Option<BuiltinToolFilter>,
    pub bash_executor: Option<Arc<dyn tool_basic::bash::CommandExecutor>>,
    pub extra_tools: Option<Arc<Vec<Arc<dyn tool_core::Tool>>>>,
    pub acp_session_id: Option<String>,
    pub goal_mode: bool,
    pub is_background_review: bool,
    pub memory_enabled: bool,
    pub user_profile_enabled: bool,
    pub memory_nudge_interval: u32,
    pub skill_nudge_interval: u32,
    pub role_setting: Option<String>,
    pub agents_md: Option<String>,
    pub system_prompt_override: Option<String>,
    pub skills_prompt: Option<String>,
    pub memory_prompt: Option<String>,
    pub env_context: Option<EnvContext>,
    pub reasoning_effort: Option<String>,

    /// When true, the agent loads state from checkpoint without appending
    /// a user message (used by workflow crash resume).
    pub resume_mode: bool,

    /// When true, the `llm` tool is registered, giving the agent direct
    /// LLM invocation with provider/model discovery.
    pub llm_tool_enabled: bool,
}

impl std::fmt::Debug for ReactBuildConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReactBuildConfig")
            .field("db_path", &self.db_path)
            .field("working_folder", &self.working_folder)
            .field("model", &self.model)
            .field("bash_executor", &self.bash_executor.as_ref().map(|_| "..."))
            .field("extra_tools", &self.extra_tools.as_ref().map(|t| t.len()))
            .finish()
    }
}

impl Default for ReactBuildConfig {
    fn default() -> Self {
        Self {
            db_path: None,
            thread_id: None,
            trace_thread_id: None,
            user_id: None,
            system_prompt: None,
            exa_api_key: None,
            exa_codesearch_enabled: false,
            mcp_exa_url: String::new(),
            mcp_remote_cmd: String::new(),
            mcp_remote_args: String::new(),
            github_token: None,
            mcp_github_cmd: String::new(),
            mcp_github_args: Vec::new(),
            mcp_github_url: None,
            mcp_verbose: false,
            openai_api_key: None,
            openai_base_url: None,
            openai_temperature: None,
            model: None,
            model_tier: None,
            parent_model_hint: None,
            aux_model: None,
            llm_provider: None,
            llm_provider_name: None,
            curator_provider: None,
            curator_model: None,
            curator_api_key: None,
            curator_base_url: None,
            embedding_api_key: None,
            embedding_base_url: None,
            embedding_model: None,
            working_folder: None,
            allow_paths_outside_workdir: true,
            compaction_config: None,
            tot_config: TotRunnerConfig::default(),
            got_config: GotRunnerConfig::default(),
            mcp_servers: None,
            acp_mcp_sources: None,
            skill_registry: None,
            max_sub_agent_depth: None,
            depth: None,
            async_agent_registry: None,
            max_concurrent_subagents: None,
            curator_max_iterations: Some(9999),
            dry_run: false,
            builtin_tool_filter: None,
            call_tool_filter: None,
            bash_executor: None,
            extra_tools: None,
            acp_session_id: None,
            goal_mode: false,
            is_background_review: false,
            memory_enabled: true,
            user_profile_enabled: true,
            memory_nudge_interval: 10,
            skill_nudge_interval: 10,
            role_setting: None,
            agents_md: None,
            system_prompt_override: None,
            skills_prompt: None,
            memory_prompt: None,
            env_context: None,
            reasoning_effort: None,
            resume_mode: false,
            llm_tool_enabled: false,
        }
    }
}

impl ReactBuildConfig {
    pub fn from_env() -> Self {
        Self {
            db_path: std::env::var("ANUREO_DB_PATH").ok(),
            thread_id: std::env::var("ANUREO_THREAD_ID").ok(),
            trace_thread_id: None,
            user_id: std::env::var("ANUREO_USER_ID").ok(),
            system_prompt: std::env::var("SYSTEM_PROMPT").ok(),
            exa_api_key: std::env::var("EXA_API_KEY").ok(),
            exa_codesearch_enabled: std::env::var("ANUREO_EXA_CODESEARCH")
                .ok()
                .map(|s| matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
            mcp_exa_url: std::env::var("MCP_EXA_URL")
                .unwrap_or_else(|_| "https://exa-cp.backend.mcp.dev".to_string()),
            mcp_remote_cmd: std::env::var("MCP_REMOTE_CMD").unwrap_or_else(|_| "npx".to_string()),
            mcp_remote_args: std::env::var("MCP_REMOTE_ARGS")
                .unwrap_or_else(|_| "mcp-remote".to_string()),
            github_token: std::env::var("GITHUB_TOKEN").ok(),
            mcp_github_cmd: std::env::var("MCP_GITHUB_CMD").unwrap_or_else(|_| "npx".to_string()),
            mcp_github_args: std::env::var("MCP_GITHUB_ARGS")
                .unwrap_or_else(|_| "-y @modelcontextprotocol/server-github".to_string())
                .split_whitespace()
                .map(|s| s.to_string())
                .collect(),
            mcp_github_url: std::env::var("MCP_GITHUB_URL").ok(),
            mcp_verbose: std::env::var("MCP_VERBOSE")
                .ok()
                .map(|s| matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
            openai_api_key: std::env::var("OPENAI_API_KEY").ok(),
            openai_base_url: std::env::var("OPENAI_BASE_URL").ok(),
            openai_temperature: std::env::var("OPENAI_TEMPERATURE").ok(),
            model: None,
            model_tier: None,
            parent_model_hint: None,
            aux_model: std::env::var("ANUREO_AUX_MODEL").ok(),
            llm_provider: None,
            llm_provider_name: None,
            curator_provider: std::env::var("ANUREO_CURATOR_PROVIDER").ok(),
            curator_model: std::env::var("ANUREO_CURATOR_MODEL").ok(),
            curator_api_key: std::env::var("ANUREO_CURATOR_API_KEY").ok(),
            curator_base_url: std::env::var("ANUREO_CURATOR_BASE_URL").ok(),
            embedding_api_key: std::env::var("EMBEDDING_API_KEY").ok(),
            embedding_base_url: std::env::var("EMBEDDING_BASE_URL").ok(),
            embedding_model: std::env::var("EMBEDDING_MODEL").ok(),
            working_folder: std::env::var("WORKING_FOLDER").ok().map(PathBuf::from),
            allow_paths_outside_workdir: true,
            compaction_config: None,
            tot_config: TotRunnerConfig::default(),
            got_config: GotRunnerConfig {
                adaptive: std::env::var("ANUREO_GOT_ADAPTIVE")
                    .ok()
                    .map(|s| matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
                    .unwrap_or(false),
                agot_llm_complexity: std::env::var("ANUREO_GOT_AGOT_LLM_COMPLEXITY")
                    .ok()
                    .map(|s| matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
                    .unwrap_or(false),
            },
            mcp_servers: None,
            acp_mcp_sources: None,
            skill_registry: None,
            max_sub_agent_depth: std::env::var("MAX_SUB_AGENT_DEPTH")
                .ok()
                .and_then(|s| s.parse().ok()),
            depth: None,
            async_agent_registry: None,
            max_concurrent_subagents: std::env::var("MAX_CONCURRENT_SUBAGENTS")
                .ok()
                .and_then(|s| s.parse().ok()),
            curator_max_iterations: std::env::var("ANUREO_CURATOR_MAX_ITERATIONS")
                .ok()
                .and_then(|s| s.parse().ok()),
            dry_run: std::env::var("ANUREO_DRY_RUN")
                .ok()
                .map(|s| matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
            builtin_tool_filter: None,
            call_tool_filter: None,
            bash_executor: None,
            extra_tools: None,
            acp_session_id: None,
            goal_mode: false,
            is_background_review: false,
            memory_enabled: true,
            user_profile_enabled: true,
            memory_nudge_interval: std::env::var("ANUREO_MEMORY_NUDGE_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            skill_nudge_interval: std::env::var("ANUREO_SKILL_NUDGE_INTERVAL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(10),
            role_setting: None,
            agents_md: None,
            system_prompt_override: None,
            skills_prompt: None,
            memory_prompt: None,
            env_context: None,
            reasoning_effort: None,
            resume_mode: false,
            llm_tool_enabled: std::env::var("ANUREO_LLM_TOOL_ENABLED")
                .ok()
                .map(|s| matches!(s.trim().to_lowercase().as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
        }
    }

    /// Hermes `curator.py:1250-1410` three-way provider resolution.
    ///
    /// For each slot (`provider`, `model`, `api_key`, `base_url`) the
    /// precedence is:
    ///
    /// 1. `curator_<slot>` override (Some non-empty wins).
    /// 2. The deprecated `aux_model` (or `llm_provider`/`llm_provider_name`)
    ///    auxiliary tier.
    /// 3. The global session value (`openai_api_key`, `openai_base_url`,
    ///    `model`, etc.).
    ///
    /// Returns a `CuratorProviderOverrides` struct with the resolved
    /// values. `None` means "no override; downstream code should fall back
    /// to its own defaults". Empty strings are treated as `None` so an
    /// unset env var or config field does not accidentally clobber the
    /// global value.
    pub fn resolve_curator_overrides(&self) -> CuratorProviderOverrides {
        fn pick(over: Option<&str>, aux: Option<&str>, glob: Option<&str>) -> Option<String> {
            let over = over.filter(|s| !s.is_empty());
            let aux = aux.filter(|s| !s.is_empty());
            let glob = glob.filter(|s| !s.is_empty());
            over.map(String::from)
                .or(aux.map(String::from))
                .or(glob.map(String::from))
        }
        CuratorProviderOverrides {
            provider: pick(
                self.curator_provider.as_deref(),
                self.llm_provider_name
                    .as_deref()
                    .or(self.llm_provider.as_deref()),
                None,
            ),
            model: pick(
                self.curator_model.as_deref(),
                self.aux_model.as_deref(),
                self.model.as_deref(),
            ),
            api_key: pick(
                self.curator_api_key.as_deref(),
                None,
                self.openai_api_key.as_deref(),
            ),
            base_url: pick(
                self.curator_base_url.as_deref(),
                None,
                self.openai_base_url.as_deref(),
            ),
        }
    }
}

/// Resolved curator provider overrides produced by
/// `ReactBuildConfig::resolve_curator_overrides`.
///
/// Mirrors Hermes `curator.py:1395-1410` provider-resolution helper.
#[derive(Debug, Clone, Default)]
pub struct CuratorProviderOverrides {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::ReactBuildConfig;

    fn with_env(key: &str, value: Option<&str>, f: impl FnOnce()) {
        crate::test_support::with_env(key, value, f);
    }

    #[test]
    fn from_env_github_token_and_mcp_override() {
        with_env("GITHUB_TOKEN", Some("test-token"), || {
            with_env("MCP_GITHUB_CMD", None, || {
                with_env("MCP_GITHUB_ARGS", None, || {
                    let config = ReactBuildConfig::from_env();
                    assert!(config.github_token.is_some());
                    assert_eq!(config.github_token.as_deref(), Some("test-token"));
                    assert_eq!(config.mcp_github_cmd, "npx");
                    assert!(config.mcp_github_args.contains(&"-y".to_string()));
                    assert!(config
                        .mcp_github_args
                        .iter()
                        .any(|a| a.contains("server-github")));
                });
            });
        });

        with_env("GITHUB_TOKEN", None, || {
            let config = ReactBuildConfig::from_env();
            assert!(config.github_token.is_none());
        });

        with_env("GITHUB_TOKEN", Some("x"), || {
            with_env("MCP_GITHUB_CMD", Some("custom-cmd"), || {
                with_env("MCP_GITHUB_ARGS", Some("arg1 arg2"), || {
                    let config = ReactBuildConfig::from_env();
                    assert_eq!(config.mcp_github_cmd, "custom-cmd");
                    assert_eq!(config.mcp_github_args, &["arg1", "arg2"]);
                });
            });
        });

        with_env(
            "MCP_GITHUB_URL",
            Some("https://api.githubcopilot.com/mcp/"),
            || {
                let config = ReactBuildConfig::from_env();
                assert_eq!(
                    config.mcp_github_url.as_deref(),
                    Some("https://api.githubcopilot.com/mcp/")
                );
            },
        );
        with_env("MCP_GITHUB_URL", None, || {
            let config = ReactBuildConfig::from_env();
            assert!(config.mcp_github_url.is_none());
        });
    }

    #[test]
    fn sub_agent_config_inherits_trace_thread_id() {
        let parent = ReactBuildConfig::from_env();
        let mut parent_with_trace = parent.clone();
        parent_with_trace.trace_thread_id = Some("parent-trace-id".to_string());

        let sub_config = ReactBuildConfig::from_env();

        assert_eq!(sub_config.trace_thread_id, None);
    }

    #[test]
    fn nested_sub_agent_keeps_same_trace_thread_id() {
        let parent = ReactBuildConfig::from_env();
        let mut parent_with_trace = parent.clone();
        parent_with_trace.trace_thread_id = Some("root-trace-id".to_string());

        let mut sub_config = ReactBuildConfig::from_env();
        sub_config.trace_thread_id = parent_with_trace.trace_thread_id.clone();

        assert_eq!(
            sub_config.trace_thread_id,
            Some("root-trace-id".to_string())
        );
    }

    #[test]
    fn trace_thread_id_falls_back_to_thread_id() {
        let config = ReactBuildConfig::from_env();

        let trace_id = config.trace_thread_id.clone();
        let thread_id = config.thread_id.clone();

        assert!(trace_id.is_none());

        let fallback = trace_id.or(thread_id);
        assert!(fallback.is_none());
    }

    #[test]
    fn aux_model_reads_from_env() {
        with_env("ANUREO_AUX_MODEL", Some("cheap-model-v1"), || {
            let config = ReactBuildConfig::from_env();
            assert_eq!(config.aux_model.as_deref(), Some("cheap-model-v1"));
        });
        with_env("ANUREO_AUX_MODEL", None, || {
            let config = ReactBuildConfig::from_env();
            assert!(config.aux_model.is_none());
        });
    }

    #[test]
    fn aux_model_default_is_none() {
        let config = ReactBuildConfig::default();
        assert!(config.aux_model.is_none());
    }

    #[test]
    fn nudge_intervals_default_to_10() {
        with_env("ANUREO_MEMORY_NUDGE_INTERVAL", None, || {
            with_env("ANUREO_SKILL_NUDGE_INTERVAL", None, || {
                let config = ReactBuildConfig::from_env();
                assert_eq!(config.memory_nudge_interval, 10);
                assert_eq!(config.skill_nudge_interval, 10);
            });
        });
    }

    #[test]
    fn nudge_intervals_read_from_env() {
        with_env("ANUREO_MEMORY_NUDGE_INTERVAL", Some("5"), || {
            with_env("ANUREO_SKILL_NUDGE_INTERVAL", Some("20"), || {
                let config = ReactBuildConfig::from_env();
                assert_eq!(config.memory_nudge_interval, 5);
                assert_eq!(config.skill_nudge_interval, 20);
            });
        });
    }

    #[test]
    fn nudge_intervals_fallback_on_invalid_env() {
        with_env("ANUREO_MEMORY_NUDGE_INTERVAL", Some("not-a-number"), || {
            with_env("ANUREO_SKILL_NUDGE_INTERVAL", Some(""), || {
                let config = ReactBuildConfig::from_env();
                assert_eq!(config.memory_nudge_interval, 10);
                assert_eq!(config.skill_nudge_interval, 10);
            });
        });
    }
}

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::tools::{AgentCancelTool, AgentGetTool, AgentTool, GitWorktreeTool, ThreadGetTool};
use anureo_graph_core::GraphError;
use lsp::LspManager;
use memory_v2::MemoryStore;
use skill::SkillUsageStore;
#[cfg(windows)]
use tool_basic::powershell::PowerShellTool;
use tool_basic::{
    bash::BashTool,
    batch::BatchTool,
    exa::ExaCodesearchTool,
    exa::ExaWebsearchTool,
    mcp::{McpToolSource, DEFAULT_TOOL_TIMEOUT},
    register_file_tools, register_mcp_tools,
    web::WebFetcherTool,
};
use tool_core::{ArcTool, ToolRegistryLocked, YamlSpecError};
use tool_experimental::{register_file_memory_tool_guarded, register_task_tools};
use tool_extensions::LspTool;

use env_config::McpServerDef;

use super::super::config::ReactBuildConfig;

fn to_agent_error(e: impl std::fmt::Display) -> GraphError {
    GraphError::ExecutionFailed(e.to_string())
}

const DEFAULT_MCP_STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const OPTIONAL_MCP_STARTUP_GRACE: Duration = Duration::from_secs(1);

struct McpStartupResult {
    name: String,
    required: bool,
    result: Result<(), String>,
}

async fn start_mcp_server(
    server: McpServerDef,
    aggregate: Arc<ToolRegistryLocked>,
    mcp_verbose: bool,
) -> McpStartupResult {
    let name = server.name().to_string();
    let required = server.required();
    let startup_timeout = server
        .startup_timeout_sec()
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_MCP_STARTUP_TIMEOUT);
    let tool_timeout = server
        .tool_timeout_sec()
        .map(Duration::from_secs)
        .unwrap_or(DEFAULT_TOOL_TIMEOUT);
    tracing::debug!(
        mcp_server = %name,
        required,
        startup_timeout_secs = startup_timeout.as_secs(),
        tool_timeout_secs = tool_timeout.as_secs(),
        "starting MCP server"
    );
    let result = tokio::time::timeout(startup_timeout, async {
        let mcp = match server {
            McpServerDef::Stdio {
                command, args, env, ..
            } => McpToolSource::new_with_env_and_tool_timeout(
                command,
                args,
                env,
                mcp_verbose,
                tool_timeout,
            )
            .await
            .map_err(|error| error.to_string())?,
            McpServerDef::Http { url, headers, .. } => {
                McpToolSource::new_http_with_tool_timeout(url, headers, tool_timeout)
                    .await
                    .map_err(|error| error.to_string())?
            }
        };
        register_mcp_tools(aggregate.as_ref(), Arc::new(mcp))
            .await
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| {
        format!(
            "startup timed out after {} seconds",
            startup_timeout.as_secs()
        )
    })
    .and_then(|result| result);
    McpStartupResult {
        name,
        required,
        result,
    }
}

async fn start_configured_mcp_servers(
    servers: &[McpServerDef],
    aggregate: Arc<ToolRegistryLocked>,
    mcp_verbose: bool,
) -> Result<(), GraphError> {
    let mut pending_required: HashSet<String> = servers
        .iter()
        .filter(|server| server.required())
        .map(|server| server.name().to_string())
        .collect();
    if servers.is_empty() {
        return Ok(());
    }

    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
    for server in servers.iter().cloned() {
        let aggregate = Arc::clone(&aggregate);
        let result_tx = result_tx.clone();
        tokio::spawn(async move {
            let startup_result = start_mcp_server(server, aggregate, mcp_verbose).await;
            match &startup_result.result {
                Ok(()) => tracing::info!(
                    mcp_server = %startup_result.name,
                    required = startup_result.required,
                    "MCP server ready"
                ),
                Err(error) => tracing::warn!(
                    mcp_server = %startup_result.name,
                    required = startup_result.required,
                    %error,
                    "MCP server failed to start"
                ),
            }
            let _ = result_tx.send(startup_result);
        });
    }
    drop(result_tx);

    let deadline = tokio::time::Instant::now() + OPTIONAL_MCP_STARTUP_GRACE;
    while let Some(result) = tokio::time::timeout_at(deadline, result_rx.recv())
        .await
        .ok()
        .flatten()
    {
        if result.required {
            pending_required.remove(&result.name);
            if let Err(error) = result.result {
                return Err(to_agent_error(format!(
                    "required MCP server `{}` failed to start: {error}",
                    result.name
                )));
            }
        }
    }

    while !pending_required.is_empty() {
        let Some(result) = result_rx.recv().await else {
            return Err(to_agent_error(
                "MCP startup tasks ended before required servers completed",
            ));
        };
        if result.required {
            pending_required.remove(&result.name);
            if let Err(error) = result.result {
                return Err(to_agent_error(format!(
                    "required MCP server `{}` failed to start: {error}",
                    result.name
                )));
            }
        }
    }
    Ok(())
}

pub(crate) async fn build_tool_source(
    config: &ReactBuildConfig,
    _store: &Option<Arc<dyn checkpoint::Store>>,
) -> Result<Arc<ToolRegistryLocked>, GraphError> {
    let working_folder_arc = config.working_folder.as_ref().map(|p| Arc::new(p.clone()));

    let aggregate = Arc::new(ToolRegistryLocked::new());

    aggregate
        .register_async(Box::new(WebFetcherTool::new()))
        .await;
    #[cfg(not(windows))]
    let bash_tool = if let Some(ref executor) = config.bash_executor {
        match &working_folder_arc {
            Some(wf) => {
                BashTool::with_working_folder_and_executor(Arc::clone(wf), executor.clone())
            }
            None => BashTool::with_executor(executor.clone()),
        }
    } else {
        match &working_folder_arc {
            Some(wf) => BashTool::with_working_folder(Arc::clone(wf)),
            None => BashTool::new(),
        }
    };
    #[cfg(not(windows))]
    aggregate.register_async(Box::new(bash_tool)).await;

    #[cfg(windows)]
    if let Some(ref executor) = config.bash_executor {
        let bash_tool = match &working_folder_arc {
            Some(wf) => {
                BashTool::with_working_folder_and_executor(Arc::clone(wf), executor.clone())
            }
            None => BashTool::with_executor(executor.clone()),
        };
        aggregate.register_async(Box::new(bash_tool)).await;
    } else {
        let ps_tool = match &working_folder_arc {
            Some(wf) => PowerShellTool::with_working_folder(Arc::clone(wf)),
            None => PowerShellTool::new(),
        };
        aggregate.register_async(Box::new(ps_tool)).await;
    }

    if let Some(ref key) = config.exa_api_key {
        aggregate
            .register_async(Box::new(ExaWebsearchTool::new(key.clone())))
            .await;
        if config.exa_codesearch_enabled {
            aggregate
                .register_async(Box::new(ExaCodesearchTool::new(key.clone())))
                .await;
        }
    }
    if let Some(ref wf) = config.working_folder {
        register_file_tools(
            aggregate.as_ref(),
            wf,
            config.allow_paths_outside_workdir,
            config.skill_registry.clone(),
            Some(SkillUsageStore::new(&wf.join(".anureo/skills"))),
            config.is_background_review,
        )
        .map_err(to_agent_error)?;

        let db_path = env_config::home::anureo_home().join("tasks").join("tasks.db");
        let db_dir = db_path.parent().unwrap();
        let _ = std::fs::create_dir_all(db_dir);
        if config.goal_mode {
            if let Ok(db) = task_core::TaskDb::open(&db_path).await {
                register_task_tools(&aggregate, Arc::new(db)).await;
            }
        }

        // Memory tool registration — gated by config flags (plan 011-03, aligns Hermes agent_init.py:1076).
        //
        // MemoryStore is created when EITHER memory_enabled OR user_profile_enabled is true.
        // MemoryTool is registered when memory_enabled OR user_profile_enabled is true,
        // but writes to USER.md are guarded by user_profile_enabled at the tool level.
        let needs_memory = config.memory_enabled || config.user_profile_enabled;
        if needs_memory {
            let memory_store = Arc::new(MemoryStore::new(&MemoryStore::default_path()));
            register_file_memory_tool_guarded(
                &aggregate,
                memory_store,
                config.user_profile_enabled,
            )
            .await;
        }
    }

    if config.llm_tool_enabled {
        if let Err(e) = register_llm_tool(&aggregate, config, working_folder_arc.as_ref()).await {
            tracing::warn!("llm tool registration failed: {}", e);
        }
    }
    if let Some(ref wf) = config.working_folder {
        aggregate.register_sync(Box::new(BatchTool::new(Arc::new(wf.clone()))));
    }
    let lsp_manager = LspManager::from_configs(env_config::get_default_lsp_servers());
    aggregate.register_sync(Box::new(LspTool::new(Arc::new(tokio::sync::RwLock::new(
        lsp_manager,
    )))));

    let reused_names: HashSet<String> = config
        .acp_mcp_sources
        .as_ref()
        .map(|sources| sources.iter().map(|(name, _)| name.clone()).collect())
        .unwrap_or_default();
    if let Some(ref servers) = config.mcp_servers {
        let uncached: Vec<_> = servers
            .iter()
            .filter(|server| !reused_names.contains(server.name()))
            .cloned()
            .collect();
        start_configured_mcp_servers(&uncached, Arc::clone(&aggregate), config.mcp_verbose).await?;
    }
    if let Some(ref sources) = config.acp_mcp_sources {
        for (_, source) in sources {
            register_mcp_tools(aggregate.as_ref(), Arc::clone(source))
                .await
                .map_err(|error| {
                    to_agent_error(format!("cached MCP registration failed: {error}"))
                })?;
        }
    }
    if let Some(ref token) = config.github_token {
        let use_http = config
            .mcp_github_url
            .as_deref()
            .map(|u| u.starts_with("http://") || u.starts_with("https://"))
            .unwrap_or(false);
        if use_http {
            let url = config.mcp_github_url.as_deref().unwrap();
            match McpToolSource::new_http(url, [("Authorization", format!("Bearer {}", token))])
                .await
            {
                Ok(mcp) => {
                    if let Err(e) = register_mcp_tools(aggregate.as_ref(), Arc::new(mcp)).await {
                        tracing::warn!(
                            "GitHub MCP (HTTP) registered but list/call may fail: {}",
                            e
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("GitHub MCP (HTTP) failed to connect, skipping: {}", e);
                }
            }
        } else {
            tracing::debug!("starting GitHub MCP (stdio)");
            let cmd = config.mcp_github_cmd.clone();
            let args = config.mcp_github_args.clone();
            let env_github = vec![("GITHUB_TOKEN".to_string(), token.clone())];
            let mcp_verbose = config.mcp_verbose;
            match McpToolSource::new_with_env(cmd, args, env_github, mcp_verbose).await {
                Ok(mcp) => {
                    if let Err(e) = register_mcp_tools(aggregate.as_ref(), Arc::new(mcp)).await {
                        tracing::warn!("GitHub MCP registered but list/call may fail: {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("GitHub MCP failed to start, skipping: {}", e);
                }
            }
        }
    }

    if let Some(ref tools) = config.extra_tools {
        for tool in tools.iter() {
            aggregate
                .register_async(Box::new(ArcTool(tool.clone())))
                .await;
        }
    }

    // Create shared registry for async agents
    let registry = crate::tools::AsyncAgentRegistry::new();

    aggregate
        .register_async(Box::new(AgentTool::new(
            Arc::new(config.clone()),
            config.max_sub_agent_depth,
            registry.clone(),
        )))
        .await;
    aggregate
        .register_async(Box::new(AgentGetTool::new(registry.clone())))
        .await;
    aggregate
        .register_async(Box::new(AgentCancelTool::new(registry.clone())))
        .await;
    aggregate
        .register_async(Box::new(ThreadGetTool::new(registry)))
        .await;
    aggregate
        .register_async(Box::new(GitWorktreeTool::new(Arc::new(config.clone()))))
        .await;

    apply_registry_config(&aggregate, config)
        .await
        .map_err(to_agent_error)?;

    Ok(aggregate)
}

async fn apply_registry_config(
    aggregate: &Arc<ToolRegistryLocked>,
    config: &ReactBuildConfig,
) -> Result<(), YamlSpecError> {
    aggregate.load_yaml_specs().await?;
    if let Some(ref filter) = config.builtin_tool_filter {
        if !filter.is_noop() {
            tracing::info!(
                enabled = ?filter.enabled,
                disabled = ?filter.disabled,
                "applying builtin tool filter"
            );
            aggregate.set_filter(Some(filter.clone())).await;
        }
    }
    if config.dry_run {
        aggregate.set_dry_run(true).await;
    }
    Ok(())
}

/// Pre-load provider + models.dev catalog and register the `LlmTool`.
///
/// Steps:
/// 1. Load `Vec<ProviderConfig>` from XDG config.
/// 2. Ask the global `ModelRegistry` for the combined model catalog.
/// 3. Group the catalog by `provider` field, attach each group's
///    connection info from the matching `ProviderConfig`.
/// 4. Build `LlmToolData` and register `LlmTool` on the aggregate.
///
/// Returns an error string if any step fails (the caller logs it and
/// continues without the LLM tool, rather than aborting the whole build).
async fn register_llm_tool(
    aggregate: &Arc<ToolRegistryLocked>,
    config: &ReactBuildConfig,
    working_folder: Option<&Arc<std::path::PathBuf>>,
) -> Result<(), String> {
    use model_spec_core::resolver::ModelsDevResolver;
    use tool_experimental::{LlmProviderData, LlmTool, LlmToolData};

    let providers: Vec<model_spec_core::ProviderConfig> =
        env_config::load_provider_configs_from_xdg().unwrap_or_default();

    if providers.is_empty() {
        return Err("no providers configured in XDG config".to_string());
    }

    // Fetch the full models.dev catalog: provider_id → Provider (with nested models).
    // Non-fatal: if the fetch fails, we register the tool with empty model lists
    // so that `invoke` still works — only discovery actions return empty results.
    let catalog: std::collections::HashMap<String, model_spec_core::Provider> =
        match ModelsDevResolver::new().fetch_all_providers().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "models.dev fetch failed; llm tool will have empty model catalogs"
                );
                std::collections::HashMap::new()
            }
        };

    let default_provider = config
        .llm_provider_name
        .clone()
        .or_else(|| providers.first().map(|p| p.name.clone()))
        .unwrap_or_default();
    let default_model = config
        .model
        .clone()
        .or_else(|| config.aux_model.clone())
        .unwrap_or_default();

    let mut provider_data: Vec<LlmProviderData> = Vec::with_capacity(providers.len());
    for p in &providers {
        // Match XDG provider name to models.dev provider_id (direct name match only).
        let models: Vec<model_spec_core::Model> = catalog
            .get(&p.name)
            .map(|provider| provider.models.values().cloned().collect())
            .unwrap_or_default();

        if models.is_empty() {
            tracing::debug!(
                provider = %p.name,
                "no models.dev catalog entry, provider will have empty model list"
            );
        }

        provider_data.push(LlmProviderData {
            name: p.name.clone(),
            base_url: p.base_url.clone().unwrap_or_default(),
            api_key: p.api_key.clone().unwrap_or_default(),
            models,
        });
    }

    let data = Arc::new(LlmToolData {
        default_provider,
        default_model,
        providers: provider_data,
    });

    let tool = LlmTool::new(data, working_folder.cloned(), Default::default());
    aggregate.register_async(Box::new(tool)).await;
    Ok(())
}

#[cfg(test)]
mod mcp_startup_tests {
    use super::*;
    use std::collections::HashMap;

    async fn unresponsive_http_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _connection = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        });
        format!("http://{address}/mcp")
    }

    fn http_server(
        name: &str,
        url: String,
        required: bool,
        startup_timeout_sec: u64,
    ) -> McpServerDef {
        McpServerDef::Http {
            name: name.to_string(),
            url,
            headers: HashMap::new(),
            oauth: None,
            required,
            startup_timeout_sec: Some(startup_timeout_sec),
            tool_timeout_sec: None,
        }
    }

    #[tokio::test]
    async fn optional_unresponsive_server_does_not_block_tool_source_build() {
        let server = http_server("slow", unresponsive_http_server().await, false, 5);
        let aggregate = Arc::new(ToolRegistryLocked::new());
        let started = tokio::time::Instant::now();

        start_configured_mcp_servers(&[server], aggregate, false)
            .await
            .unwrap();

        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn required_unresponsive_server_fails_after_its_startup_timeout() {
        let server = http_server("required", unresponsive_http_server().await, true, 1);
        let aggregate = Arc::new(ToolRegistryLocked::new());

        let error = start_configured_mcp_servers(&[server], aggregate, false)
            .await
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("required MCP server `required` failed to start"));
        assert!(error.to_string().contains("startup timed out"), "{error}");
    }
}

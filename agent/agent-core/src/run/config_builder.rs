//! Run orchestration for agent patterns (ReAct, ToT, GoT, DUP).
//!
//! This module retains orchestration functions that depend on anureo internals.
//! All types live in their own crates — consumers should import directly:
//! - Profile types → `crate::profile::*`
//! - `load_agents_md` → `crate::load_agents_md`
//! - `build_config_from_profile` → `crate::build_config_from_profile`

pub use super::runner::{RunCmd, RunError};
pub use super::types::{
    AgentRunResult, ResolvedModelConfig, RunCompletion, RunOptions, DEFAULT_WORKING_FOLDER,
};
use crate::agent::react::config::prompt_assembly::{assemble_system_prompt, SystemPromptInputs};

use crate::agent::ReactBuildConfig;
use crate::ResolvedAgent;
use crate::{EnvContext, ProjectInfo};
use skill::discovery::SkillRegistry;
use skill::usage::SkillUsageStore;
use std::path::PathBuf;
use std::sync::Arc;

use crate::load_agents_md;

// Internal profile helper — not re-exported.
use super::profile_helper::load_profile_from_options;
use crate::profile::AgentProfile;

/// Reads memory prompt from {anureo_home}/data/memory/.
pub fn load_memory_prompt() -> Option<String> {
    let memory_dir = env_config::home::anureo_home().join("data").join("memory");
    let store = memory_v2::MemoryStore::new(&memory_dir);
    store.capture_snapshot().ok().filter(|s| !s.is_empty())
}

/// `role_setting` from the resolved agent profile only (trimmed non-empty content).
fn role_content_from_profile(profile_role: Option<String>) -> Option<String> {
    profile_role.and_then(|s| {
        let t = s.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    })
}

/// Builds ReactBuildConfig from RunOptions.
/// Returns `(config, resolved_agent, skill_registry)`:
/// - `config`: the ReactBuildConfig ready to run.
/// - `resolved_agent`: which agent profile was loaded, if any.
/// - `skill_registry`: the discovered SkillRegistry (with builtin + agent skills
///   already injected and filters applied). `None` only if discovery was
///   short-circuited by callers using a different path. The CLI startup banner
///   uses this to print the loaded skills list at `-v` / `-vv`.
pub fn build_react_config(
    opts: &RunOptions,
) -> (
    ReactBuildConfig,
    Option<ResolvedAgent>,
    Option<Arc<SkillRegistry>>,
) {
    let loaded = load_profile_from_options(opts);
    let resolved_agent = loaded.as_ref().map(|(p, source)| ResolvedAgent {
        name: p.name.clone(),
        description: p.description.clone(),
        source: source.clone(),
    });
    let profile = loaded.map(|(p, _)| p);
    let mut effective_opts = opts.clone();
    apply_model_provider_resolution(&mut effective_opts);
    if let Some(ref p) = profile {
        apply_profile_to_run_options(p, &mut effective_opts);
    }

    let mut base = ReactBuildConfig::from_env();
    base.goal_mode = effective_opts.goal_mode;
    // Preserve caller-provided tools (ACP bridges, including get/create/update_goal)
    // before appending process-wide defaults below.  Dropping this field leaves
    // the tools constructed by the ACP host registered in logs but invisible to
    // both the final registry and the model.
    base.extra_tools = effective_opts.extra_tools.clone();
    base.acp_mcp_sources = effective_opts.acp_mcp_sources.clone();
    base.dry_run = effective_opts.dry_run;
    {
        let model = effective_opts.model.as_deref();
        let provider = effective_opts.provider.as_deref();
        let has_provider_prefix = model.is_some_and(|m| m.contains('/'));
        base.model = match (model, provider) {
            (Some(m), Some(p)) if !has_provider_prefix => Some(format!("{}/{}", p, m)),
            (Some(m), _) => Some(m.to_string()),
            _ => None,
        };
    }

    // Provider configuration from RunOptions (used by ACP to specify provider-specific settings)
    if let Some(ref url) = effective_opts.base_url {
        base.openai_base_url = Some(url.clone());
    }
    if let Some(ref key) = effective_opts.api_key {
        base.openai_api_key = Some(key.clone());
    }

    // CLI tier parameter processing (highest priority)
    if let Some(ref tier_str) = effective_opts.tier {
        match tier_str.parse::<model_spec_core::ModelTier>() {
            Ok(tier) => {
                base.model_tier = Some(tier);
                tracing::info!(
                    tier = ?tier,
                    "CLI tier override applied"
                );
            }
            Err(e) => {
                tracing::warn!(
                    tier_str = %tier_str,
                    error = %e,
                    "Failed to parse CLI tier, using tier from other sources"
                );
            }
        }
    }

    if let Some(ref prof) = profile {
        if let Some(t) = prof.model.as_ref().and_then(|m| m.temperature) {
            base.openai_temperature = Some(t.to_string());
        }
        let model_explicitly_set = effective_opts.model.is_some() || opts.model.is_some();
        if !model_explicitly_set {
            // Only apply profile tier if CLI tier is not set
            if base.model_tier.is_none() {
                if let Some(tier) = prof.model.as_ref().and_then(|m| m.tier) {
                    base.model_tier = Some(tier);
                    tracing::debug!(
                        tier = ?tier,
                        "No explicit model specified, applying profile tier configuration"
                    );
                }
            } else {
                tracing::debug!("CLI tier is set, skipping profile tier configuration");
            }
        } else {
            tracing::debug!(
                model = ?effective_opts.model,
                "Model explicitly specified, skipping tier configuration"
            );
        }
    }
    if let Some(ref key) = effective_opts.api_key {
        base.openai_api_key = Some(key.clone());
    }
    if let Some(ref t) = effective_opts.provider_type {
        base.llm_provider = Some(t.clone());
    }
    if let Some(ref effort) = effective_opts.effort {
        base.reasoning_effort = Some(effort.clone());
    }

    // Priority #21 (Hermes `gateway/run.py` #3): route the agent
    // construction through the bounded LRU `AgentCache` so repeated
    // turns with the same `(agent_id, model_id, working_folder)`
    // don't pay cold-start cost every time. The cache is process-wide
    // — the same builder keeps the handle alive across CLI repl
    // commands and ACP review-runner turns.
    //
    // Construction is lazy via `get_or_build`; on first call the
    // closure calls the existing build path below (`(base, resolved_agent)`).
    // On subsequent calls the cached handle is returned without
    // touching `ReactBuildConfig::from_env()` or `load_profile_from_options`.
    use std::sync::OnceLock;
    static AGENT_CACHE: OnceLock<std::sync::Arc<crate::agent_cache::AgentCache<ReactBuildConfig>>> =
        OnceLock::new();
    let cache =
        AGENT_CACHE.get_or_init(|| std::sync::Arc::new(crate::agent_cache::AgentCache::new()));
    let wf = effective_opts
        .working_folder
        .as_ref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let key = crate::agent_cache::AgentKey::new(
        effective_opts
            .agent
            .clone()
            .unwrap_or_else(|| "default".to_string()),
        wf,
    );
    let _cached = cache.get_or_build(key.clone(), || std::sync::Arc::new(base.clone()));
    // Refresh `last_used` so the idle sweeper doesn't evict an active
    // session's handle while the user is idle but still working.
    let _ = cache.sweep_idle();

    if let Some(ref prof) = profile {
        if let Some(t) = prof.model.as_ref().and_then(|m| m.temperature) {
            base.openai_temperature = Some(t.to_string());
        }
        let model_explicitly_set = effective_opts.model.is_some() || opts.model.is_some();
        if !model_explicitly_set {
            if let Some(tier) = prof.model.as_ref().and_then(|m| m.tier) {
                base.model_tier = Some(tier);
                tracing::debug!(
                    tier = ?tier,
                    "No explicit model specified, applying profile tier configuration"
                );
            }
        } else {
            tracing::debug!(
                model = ?effective_opts.model,
                "Model explicitly specified, skipping profile tier configuration to avoid override"
            );
        }
    }

    let working_folder = effective_opts
        .working_folder
        .clone()
        .unwrap_or_else(|| PathBuf::from(DEFAULT_WORKING_FOLDER));
    base.working_folder = Some(working_folder.clone());
    base.thread_id = effective_opts.thread_id.clone().or(base.thread_id.clone());

    // MCP config: CLI > profile > ANUREO_MCP_CONFIG_PATH > discover
    // NOTE: must load BEFORE the default-extra-tools provider below, so that
    // the workflow (Luft) tool's config_template captures `base.mcp_servers`
    // and inner spawned agents inherit MCP servers (e.g. chrome-devtools-mcp).
    let override_path = effective_opts.mcp_config_path.clone().or_else(|| {
        std::env::var("ANUREO_MCP_CONFIG_PATH")
            .ok()
            .map(PathBuf::from)
    });
    if let Some(path) =
        env_config::discover_mcp_config_path(override_path.as_deref(), Some(&working_folder))
    {
        match env_config::load_mcp_config_from_path(&path) {
            Ok(servers) => base.mcp_servers = Some(servers),
            Err(e) => tracing::warn!(path = %path.display(), "failed to load mcp config: {}", e),
        }
    }

    // Merge ACP-provisioned MCP servers (from session/new request) with file-based servers.
    // ACP servers override same-name mcp.json entries (priority: ACP > mcp.json).
    if let Some(ref acp_servers) = effective_opts.acp_mcp_servers {
        if !acp_servers.is_empty() {
            let mut merged = base.mcp_servers.unwrap_or_default();
            for srv in acp_servers {
                if let Some(pos) = merged.iter().position(|m| m.name() == srv.name()) {
                    tracing::debug!(name = srv.name(), "ACP MCP server overrides mcp.json entry");
                    merged[pos] = srv.clone();
                } else {
                    merged.push(srv.clone());
                }
            }
            tracing::info!(
                total = merged.len(),
                acp_count = acp_servers.len(),
                "MCP servers after merging ACP-provisioned servers"
            );
            base.mcp_servers = Some(merged);
        }
    }

    // Resolve default extra tools (e.g. the workflow tool) BEFORE the skill
    // registry is built. Their `builtin_skill()` hooks are picked up by
    // `inject_builtin_skills` below — see [`run::types::ExtraToolsProvider`]
    // for the design rationale. Putting this AFTER `build_react_config`
    // returns (the old `register_extra_tools` pattern) silently dropped the
    // builtin skills. Done AFTER MCP loading above so the provider's cloned
    // config_template carries mcp_servers for inner agents.
    if let Some(provider) = effective_opts.default_extra_tools_provider.as_ref() {
        let default_tools = provider(&base);
        if !default_tools.is_empty() {
            let mut tools = base
                .extra_tools
                .as_ref()
                .map(|v| v.as_ref().clone())
                .unwrap_or_default();
            for t in default_tools {
                tools.push(t);
            }
            base.extra_tools = Some(Arc::new(tools));
        }
    }

    let profile_role = profile
        .as_ref()
        .and_then(|p| p.role.as_ref().and_then(|r| r.content.clone()));

    let skill_registry = {
        let extra_dirs: Vec<PathBuf> = profile
            .as_ref()
            .and_then(|p| p.skills.as_ref())
            .and_then(|s| s.dirs.as_ref())
            .map(|dirs| dirs.iter().map(PathBuf::from).collect())
            .unwrap_or_default();
        let mut registry =
            SkillRegistry::discover(&working_folder, &extra_dirs).unwrap_or_else(|e| {
                tracing::warn!("skill discovery failed: {e}");
                SkillRegistry::empty()
            });

        inject_builtin_skills(&mut registry, base.extra_tools.as_deref());
        if let Some(ref p) = profile {
            if let Some(ref src) = p.source_dir {
                if let Err(e) = registry.add_agent_skills(&src.join("skills")) {
                    tracing::warn!("agent skills scan failed: {e}");
                }
            }
            if let Some(ref sc) = p.skills {
                let platform = Some(std::env::consts::OS);
                let platform_disabled: Vec<String> = sc
                    .platform_disabled
                    .as_ref()
                    .and_then(|m| m.get(std::env::consts::OS))
                    .cloned()
                    .unwrap_or_default();
                registry.apply_filters(
                    sc.enabled.as_deref(),
                    sc.disabled.as_deref(),
                    platform,
                    Some(&platform_disabled),
                );
                registry.apply_toolset_filters(None, None);
            }
        }
        let arc = Arc::new(registry);
        let prompt = arc.available_skills_prompt();
        (arc, prompt)
    };

    let skills_prompt = if skill_registry.1.is_empty() {
        None
    } else {
        let mut prompt = skill_registry.1.clone();
        if let Some(ref p) = profile {
            if let Some(preload) = p.skills.as_ref().and_then(|s| s.preload.as_ref()) {
                // Hermes parity (`skill_usage.py:bump_use_on_inject`): each
                // preloaded skill is actually injected into the prompt, so
                // this is the production bump_use site (not the manual
                // `touch_skill` in curator.rs). Without this call, use_count
                // telemetry would only increment on curator touch, defeating
                // the dual-counter design (view at view, use at inject).
                let usage_store = SkillUsageStore::new(&working_folder);
                let mut buf = String::new();
                for name in preload {
                    if let Ok(content) = skill_registry.0.load_skill(name) {
                        usage_store.bump_use(name);
                        buf.push_str(&format!("<skill name=\"{}\">\n{}</skill>\n", name, content));
                    }
                }
                if !buf.is_empty() {
                    prompt.push_str(&format!(
                        "\n\n<preloaded_skills>\n{}</preloaded_skills>",
                        buf
                    ));
                }
            }
        }
        Some(prompt)
    };

    let agent_instructions = role_content_from_profile(profile_role);

    tracing::trace!(
        agent_instructions_len = agent_instructions.as_ref().map(|s| s.len()),
        skills_prompt_len = skills_prompt.as_ref().map(|s| s.len()),
        "agent prompt",
    );

    // Transfer base into config for final assembly
    let mut config = base;

    // Set prompt assembly fields directly on config
    config.role_setting = agent_instructions;
    config.agents_md = load_agents_md(Some(&working_folder));
    config.skills_prompt = skills_prompt;
    config.memory_prompt = load_memory_prompt();
    config.env_context = Some({
        let mut ctx = EnvContext::detect().with_project(ProjectInfo::detect(&working_folder));
        if let Some(cid) = effective_opts.chat_id {
            ctx = ctx.with_chat_id(cid);
        }
        ctx
    });

    // Assemble system prompt from config fields
    let env_ctx = config.env_context.as_ref();
    let inputs = SystemPromptInputs {
        full_override: None,
        base_prompt_override: None,
        role_setting: config.role_setting.as_deref(),
        agents_md: config.agents_md.as_deref(),
        skills_prompt: config.skills_prompt.as_deref(),
        memory_prompt: config.memory_prompt.as_deref(),
        env_context: env_ctx,
        working_folder: config.working_folder.as_deref(),
    };
    config.system_prompt = Some(assemble_system_prompt(&inputs));

    let skill_registry_arc = skill_registry.0.clone();
    config.skill_registry = Some(skill_registry_arc.clone());
    config.max_sub_agent_depth = profile
        .as_ref()
        .and_then(|p| p.behavior.as_ref())
        .and_then(|b| b.max_sub_agent_depth);

    // Builtin tool filter from agent profile
    if let Some(ref prof) = profile {
        if let Some(ref tools) = prof.tools {
            if let Some(ref builtin) = tools.builtin {
                let filter = tool_core::BuiltinToolFilter {
                    enabled: builtin.enabled.clone(),
                    disabled: builtin.disabled.clone(),
                };
                if !filter.is_noop() {
                    config.builtin_tool_filter = Some(filter);
                }
            }
        }
    }

    (config, resolved_agent, Some(skill_registry_arc))
}

/// Resolve a model string (e.g. "openai/gpt-4o", "gpt-4o") into model id + provider config.
pub async fn resolve_model_config(model_str: Option<&str>) -> ResolvedModelConfig {
    let Some(model_str) = model_str else {
        tracing::debug!("No model string provided, using default configuration");
        return ResolvedModelConfig::default();
    };
    if model_str.is_empty() {
        tracing::debug!("Empty model string provided, using default configuration");
        return ResolvedModelConfig::default();
    }

    tracing::info!("🔍 Resolving model configuration for: {}", model_str);

    let providers: Vec<model_spec_core::ProviderConfig> =
        env_config::load_provider_configs_from_xdg().unwrap_or_default();

    // 1. Try ModelRegistry first
    tracing::debug!("🔎 Searching ModelRegistry for: {}", model_str);
    if let Some(entry) = model_spec_core::ModelRegistry::global()
        .get_model(model_str, &providers)
        .await
    {
        tracing::info!(
            "✅ Found model in registry: {} from provider {}",
            entry.id,
            entry.provider
        );
        return ResolvedModelConfig {
            model: Some(entry.id.clone()),
            provider: Some(entry.provider.clone()),
            base_url: entry.base_url,
            api_key: entry.api_key,
            provider_type: entry.provider_type,
            effort: None,
            tier: None,
        };
    }

    tracing::warn!("❌ Model not found in registry, trying provider/model split");

    // 2. Try "provider/model" split
    if let Some((provider_name, model_id)) = model_str.split_once('/') {
        let actual_model_id = model_id
            .rsplit_once('/')
            .map(|(_, m)| m)
            .unwrap_or(model_id);
        tracing::debug!(
            provider = %provider_name,
            model_id = %model_id,
            actual_model_id = %actual_model_id,
            "Model not in registry, loading provider config"
        );
        let provider_cfg = env_config::load_full_config("anureo")
            .ok()
            .and_then(|c| c.providers.into_iter().find(|p| p.name == provider_name));
        if let Some(p) = provider_cfg {
            tracing::info!(
                "✅ Resolved model from provider config: {} from provider {}",
                model_str,
                p.name
            );
            return ResolvedModelConfig {
                model: Some(model_str.to_string()),
                provider: Some(p.name),
                base_url: p.base_url,
                api_key: p.api_key,
                provider_type: p.provider_type,
                effort: None,
                tier: None,
            };
        } else {
            tracing::warn!(
                "⚠️  Provider '{}' not found in config, using bare model",
                provider_name
            );
        }
    }

    // 3. Bare model id — backward compat
    tracing::info!("🔧 Using bare model ID: {}", model_str);
    ResolvedModelConfig {
        model: Some(model_str.to_string()),
        ..Default::default()
    }
}

fn apply_model_provider_resolution(opts: &mut RunOptions) {
    tracing::debug!(
        "🔧 apply_model_provider_resolution called with model: {:?}, provider: {:?}",
        opts.model,
        opts.provider
    );

    if opts.model.is_none() && opts.provider.is_none() {
        tracing::debug!("⏭️  Skipping model resolution - no model or provider specified");
        return;
    }

    let provider_only = opts.provider.clone();
    let raw_model = match opts.model.as_deref() {
        Some(m) => m.to_string(),
        None => {
            tracing::debug!(
                "🔧 No model specified, resolving provider only: {:?}",
                provider_only
            );
            resolve_provider_fields_into_opts(provider_only.as_deref(), opts);
            return;
        }
    };

    tracing::debug!("🔧 Processing model: {}", raw_model);

    // Validate provider/model format
    if raw_model.is_empty() {
        tracing::warn!("❌ Model name cannot be empty");
        opts.model = None;
        return;
    }

    let (resolved_provider, model_name) = if let Some((p, m)) = raw_model.split_once('/') {
        // Validate provider and model parts
        if p.is_empty() {
            tracing::warn!("❌ Provider name in 'provider/model' format cannot be empty");
            opts.model = None;
            return;
        }
        if m.is_empty() {
            tracing::warn!("❌ Model name in 'provider/model' format cannot be empty");
            opts.model = None;
            return;
        }
        tracing::debug!(
            "✅ Parsed provider/model format: provider={}, model={}",
            p,
            m
        );
        (Some(p.to_string()), m.to_string())
    } else {
        // Bare model name - validate it's not just whitespace
        if raw_model.trim().is_empty() {
            tracing::warn!("❌ Model name cannot be empty or whitespace only");
            opts.model = None;
            return;
        }
        tracing::debug!("✅ Using bare model name: {}", raw_model.trim());
        (None, raw_model.trim().to_string())
    };

    let effective_provider = provider_only
        .clone()
        .or(resolved_provider.clone())
        .or_else(|| {
            env_config::load_full_config("anureo")
                .ok()
                .and_then(|c| c.default_provider)
        });

    opts.model = Some(model_name.clone());
    if opts.provider.is_none() {
        opts.provider = effective_provider.clone();
    }

    tracing::info!(
        "🎯 Final resolution: model_name={}, provider={:?}",
        model_name,
        effective_provider
    );

    if let Some(ref name) = effective_provider {
        resolve_provider_fields_into_opts(Some(name.as_str()), opts);
    }
}

fn resolve_provider_fields_into_opts(provider_name: Option<&str>, opts: &mut RunOptions) {
    let Some(name) = provider_name else { return };

    let full_config = match env_config::load_full_config("anureo") {
        Ok(c) => c,
        Err(_) => return,
    };

    let provider = match full_config
        .providers
        .iter()
        .find(|p| p.name.eq_ignore_ascii_case(name))
    {
        Some(p) => p,
        None => {
            tracing::warn!(
                provider = name,
                "Provider not found in config.toml [[providers]]"
            );
            return;
        }
    };

    if opts.api_key.is_none() {
        if let Some(ref key) = provider.api_key {
            opts.api_key = Some(key.clone());
        }
    }
    if opts.base_url.is_none() {
        if let Some(ref url) = provider.base_url {
            opts.base_url = Some(url.clone());
        }
    }
    if opts.provider_type.is_none() {
        if let Some(ref t) = provider.provider_type {
            opts.provider_type = Some(t.clone());
        }
    }
}

/// Pull `Tool::builtin_skill()` from every extra tool, and inject it into the
/// skill registry. User-supplied skills take precedence: builtin injection is
/// skipped when a skill with the same name was already discovered from disk.
pub fn inject_builtin_skills(
    registry: &mut SkillRegistry,
    extra_tools: Option<&Vec<Arc<dyn tool_core::Tool>>>,
) {
    let Some(tools) = extra_tools else { return };
    for tool in tools {
        let Some(skill) = tool.builtin_skill() else {
            continue;
        };
        tracing::debug!(
            skill = %skill.name,
            requires_tools = ?skill.requires_tools,
            references = skill.references.len(),
            "registering builtin skill from tool"
        );
        registry.add_builtin(
            &skill.name,
            &skill.description,
            &skill.content,
            skill.triggers,
            skill.requires_tools,
            skill.references,
        );
    }
}

fn apply_profile_to_run_options(profile: &AgentProfile, opts: &mut RunOptions) {
    if let Some(ref tools) = profile.tools {
        if let Some(ref mcp) = tools.mcp {
            if let Some(ref config) = mcp.config {
                if opts.mcp_config_path.is_none() {
                    opts.mcp_config_path = Some(config.clone());
                }
            }
        }
    }
    if let Some(ref model) = profile.model {
        if let Some(ref name) = model.name {
            if opts.model.is_none() {
                opts.model = Some(name.clone());
            }
        }
    }
    if let Some(ref env) = profile.environment {
        if opts.working_folder.is_none() {
            opts.working_folder = env.working_folder.clone();
        }
        if opts.thread_id.is_none() {
            opts.thread_id = env.thread_id.clone();
        }
    }
}

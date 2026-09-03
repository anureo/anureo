//! ACP Agent implementation: maps protocol requests to anureo execution.
//!
//! [`AnureoAcpAgent`] implements `agent_client_protocol::Agent` and maps ACP requests
//! to anureo sessions and execution. See [`crate::protocol`] for protocol and behavior details.

use crate::agent_registry::AgentRegistry;
use crate::client_capabilities::ClientCapabilitiesInfo;
use crate::content::content_blocks_to_user_content;
use crate::extensions::{register::register_default_extensions, ExtensionRegistry};
use crate::session::{SessionId as OurSessionId, SessionLifecycle, SessionStore};
use crate::session_config_store::SessionConfigStore;
use crate::session_repository::SessionRepository;
use crate::stream_bridge::{SessionNotifier, SessionUpdateEnvelope};
use crate::tools::{create_acp_tools, ClientBridgeTrait, NoOpClientBridge};
use agent::state::ReActState;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, McpCapabilities, PromptCapabilities, SessionCapabilities,
    SessionCloseCapabilities, SessionDeleteCapabilities, SessionListCapabilities,
    SessionResumeCapabilities,
};
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, DeleteSessionRequest, DeleteSessionResponse, ForkSessionRequest,
    ForkSessionResponse, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
    SessionConfigOptionValue, SessionId, SessionUpdate, SetSessionConfigOptionRequest,
    SetSessionConfigOptionResponse, SetSessionModeRequest, SetSessionModeResponse, StopReason,
    Usage,
};
use agent_client_protocol::schema::ProtocolVersion;
use checkpoint::{Checkpointer, JsonSerializer, RunnableConfig};
use checkpoint_sqlite_store::SqliteSaver;
use tool_basic::bash::LocalCommandExecutor;

use agent::run::TypedAnyStreamEvent;
use agent::run::{build_react_config, run_agent_from_config, RunCmd, RunError, RunParams};
use agent::run::{RunCompletion, RunOptions};
use config::load_full_config;
use anureo_llm::message::{Message, UserContent};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

fn canonicalize_existing_directory(
    path: &std::path::Path,
) -> agent_client_protocol::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("cwd must be an existing absolute directory"));
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| {
        agent_client_protocol::Error::invalid_params()
            .data("cwd must be an existing absolute directory")
    })?;
    if !canonical.is_dir() {
        return Err(agent_client_protocol::Error::invalid_params()
            .data("cwd must be an existing absolute directory"));
    }
    Ok(canonical)
}

/// Tail size for the `session/load` history replay, from
/// `ANUREO_ACP_LOAD_HISTORY_TAIL` (message count; default 50; 0 = replay all).
/// Earlier history is paged in through `_anureo.dev/session-history/page`.
fn load_history_tail_limit() -> usize {
    std::env::var("ANUREO_ACP_LOAD_HISTORY_TAIL")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(50)
}

/// Start index of the tail slice `session/load` replays. Extended backward
/// past leading `Tool` messages so the owning `Assistant` (with the matching
/// tool_calls) is included, and further back to the turn's `User` message so
/// the replayed tail starts on a turn boundary: ACP clients group assistant
/// replies under their user-message anchor and silently drop anchor-less
/// assistant messages, which would otherwise render the whole tail invisible.
/// `tail` is a floor, not a ceiling — a long tool-heavy turn may replay more.
fn history_tail_start(messages: &[Message], tail: usize) -> usize {
    if tail == 0 || messages.len() <= tail {
        return 0;
    }
    let mut start = messages.len() - tail;
    while start > 0 && matches!(messages[start], Message::Tool { .. }) {
        start -= 1;
    }
    if start > 0 && matches!(messages[start], Message::Assistant { .. }) {
        while start > 0 && !matches!(messages[start], Message::User(_)) {
            start -= 1;
        }
    }
    start
}

/// Read-only snapshot for `_anureo.dev/session-history/info`.
pub struct SessionHistoryInfo {
    pub session_id: String,
    pub total_messages: usize,
    /// Raw-message index where client-visible history currently begins.
    /// Sessions that never had a truncated replay (live sessions) report
    /// `total_messages` here.
    pub loaded_start_index: usize,
    pub has_more: bool,
}

/// One replayable message from `_anureo.dev/session-history/page`.
pub struct SessionHistoryMessage {
    /// Raw index in the checkpoint message list (stable across pages).
    pub index: usize,
    pub role: &'static str,
    pub updates: Vec<SessionUpdate>,
}

/// One backward page from `_anureo.dev/session-history/page`.
pub struct SessionHistoryPage {
    pub session_id: String,
    pub total_messages: usize,
    pub has_more: bool,
    pub messages: Vec<SessionHistoryMessage>,
}

#[async_trait::async_trait]
pub trait ModelProvider: Send + Sync {
    async fn fetch_models(&self) -> Vec<ModelOption>;
}

struct RealModelProvider;

#[async_trait::async_trait]
impl ModelProvider for RealModelProvider {
    async fn fetch_models(&self) -> Vec<ModelOption> {
        fetch_available_models().await
    }
}

async fn fetch_available_models() -> Vec<ModelOption> {
    let registry = model_spec_core::ModelRegistry::global();

    let providers: Vec<model_spec_core::ProviderConfig> = match load_full_config("anureo") {
        Ok(config) => config
            .providers
            .into_iter()
            .map(|p| model_spec_core::ProviderConfig {
                name: p.name,
                base_url: p.base_url,
                api_key: p.api_key,
                provider_type: p.provider_type,
                fetch_models: p.fetch_models.unwrap_or(false),
                cache_ttl: None,
                enable_tier_resolution: true,
                declared_models: p.models.into_iter().map(|m| m.id).collect(),
            })
            .collect(),
        Err(_) => vec![],
    };

    let entries = registry.list_all_models(&providers).await;

    let mut all_models: Vec<ModelOption> = entries
        .into_iter()
        .map(|entry| ModelOption {
            id: entry.id.clone(),
            name: entry.id,
            provider: entry.provider,
        })
        .collect();

    all_models.insert(
        0,
        ModelOption {
            id: "default".to_string(),
            name: "(default)".to_string(),
            provider: String::new(),
        },
    );

    all_models
}

/// Handle for anureo as an ACP Agent. Implements [`Agent`], holds the session store.
/// If [`session_update_tx`](Self::session_update_tx) is set, prompt execution sends
/// session/update notifications through this channel.
pub struct AnureoAcpAgent {
    pub(crate) sessions: SessionStore,
    pub(crate) agent_registry: AgentRegistry,
    pub(crate) config_store: SessionConfigStore,
    pub(crate) session_repository: SessionRepository,
    /// SQLite path used by both session metadata and checkpoint history.
    /// Keeping it on the agent lets embedded hosts and tests isolate agents
    /// without mutating the process-wide `ANUREO_HOME` environment variable.
    pub(crate) checkpoint_db_path: PathBuf,
    pub(crate) session_update_tx: Option<mpsc::Sender<SessionUpdateEnvelope>>,
    pub(crate) model_provider: Arc<dyn ModelProvider>,
    pub(crate) extension_registry: Arc<ExtensionRegistry>,
}

impl std::fmt::Debug for AnureoAcpAgent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnureoAcpAgent")
            .field("sessions", &"..")
            .field("agent_registry", &"..")
            .field("config_store", &"..")
            .field("session_update_tx", &self.session_update_tx.is_some())
            .finish()
    }
}

impl AnureoAcpAgent {
    /// Construct a new Agent instance (no session/update sending).
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut extension_registry = ExtensionRegistry::new();
        register_default_extensions(
            &mut extension_registry,
            std::sync::Arc::new(crate::global_events::GlobalEventBus::new()),
            None,
        );
        Self::new_with_extension_registry(Arc::new(extension_registry), None)
    }

    /// Construct an agent backed by an explicit SQLite database.
    ///
    /// This is useful for embedded runtimes that host more than one agent in
    /// a process, and prevents tests from racing through the global
    /// `ANUREO_HOME` path when Rust runs them in parallel.
    pub fn new_with_db_path(
        db_path: impl Into<PathBuf>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut extension_registry = ExtensionRegistry::new();
        register_default_extensions(
            &mut extension_registry,
            std::sync::Arc::new(crate::global_events::GlobalEventBus::new()),
            None,
        );
        Self::new_with_extension_registry_and_db_path(
            Arc::new(extension_registry),
            None,
            db_path.into(),
        )
    }

    pub(crate) fn new_with_extension_registry(
        extension_registry: Arc<ExtensionRegistry>,
        session_update_tx: Option<mpsc::Sender<SessionUpdateEnvelope>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::new_with_extension_registry_and_db_path(
            extension_registry,
            session_update_tx,
            checkpoint_sqlite_store::default_memory_db_path(),
        )
    }

    pub(crate) fn new_with_extension_registry_and_db_path(
        extension_registry: Arc<ExtensionRegistry>,
        session_update_tx: Option<mpsc::Sender<SessionUpdateEnvelope>>,
        db_path: PathBuf,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        if let Some(parent) = db_path.parent() {
            // ANUREO_HOME can be swapped by embedding hosts/tests while another
            // ACP agent is starting. Recreate the directory before opening
            // either SQLite store so a cleaned temporary home cannot turn
            // startup into an opaque "unable to open database file" error.
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("session database directory init failed: {error}"))?;
        }
        let config_store = SessionConfigStore::new(db_path.to_str().unwrap_or_default())
            .map_err(|e| format!("session config store init failed: {e}"))?;
        let session_repository = SessionRepository::new(&db_path)
            .map_err(|e| format!("session repository init failed: {e}"))?;

        let agent = Self {
            sessions: SessionStore::new(),
            agent_registry: AgentRegistry::new(),
            config_store,
            session_repository,
            checkpoint_db_path: db_path,
            session_update_tx,
            model_provider: Arc::new(RealModelProvider),
            extension_registry,
        };
        agent.restore_session_metadata()?;
        Ok(agent)
    }

    pub fn with_session_update_tx(
        tx: mpsc::Sender<SessionUpdateEnvelope>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut extension_registry = ExtensionRegistry::new();
        register_default_extensions(
            &mut extension_registry,
            std::sync::Arc::new(crate::global_events::GlobalEventBus::new()),
            None,
        );
        Self::new_with_extension_registry(Arc::new(extension_registry), Some(tx))
    }

    pub fn with_model_provider(mut self, provider: Arc<dyn ModelProvider>) -> Self {
        self.model_provider = provider;
        self
    }

    /// Returns read-only access to the session store.
    #[inline]
    pub fn sessions(&self) -> &SessionStore {
        &self.sessions
    }

    fn restore_session_metadata(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        for metadata in self.session_repository.list_for_restore()? {
            let session_id = OurSessionId::new(metadata.session_id);
            self.sessions.create_with_owner(
                session_id.clone(),
                Some(metadata.cwd),
                metadata.thread_id,
                metadata.owner_principal,
            );
            if metadata.lifecycle == "closed" {
                self.sessions.close(&session_id);
            } else {
                // A process-local running turn cannot survive restart.
                self.sessions.reopen(&session_id);
            }
        }
        Ok(())
    }

    /// Fetch available models from all configured providers.
    /// Returns a list of ModelOption for the ACP config_options response.
    /// Uses ModelRegistry for caching and unified model access.
    async fn get_available_models(&self) -> Vec<ModelOption> {
        self.model_provider.fetch_models().await
    }

    /// Returns the model-declared reasoning efforts subset for `model_id`.
    /// When the model is "default"/empty or not found in config, returns `None`
    /// (meaning all effort values are offered).
    async fn get_model_reasoning_efforts(&self, model_id: &str) -> Option<Vec<String>> {
        if model_id.is_empty() || model_id == "default" {
            return None;
        }
        let full_config = load_full_config("anureo").ok()?;
        let (_, model_name) = model_spec_core::ModelEntry::parse_id(model_id)?;
        for p in &full_config.providers {
            for m in &p.models {
                if m.id == model_name {
                    return m.reasoning_efforts.clone();
                }
            }
        }
        None
    }

    /// Resolve model configuration with tier awareness.
    /// Priority: ACP explicit model > last explicitly selected model > agent model name > agent tier > default config.
    async fn resolve_model_with_tier_awareness(
        &self,
        session_config: &crate::session::SessionConfig,
    ) -> agent::run::ResolvedModelConfig {
        let start_time = std::time::Instant::now();

        if let Some(ref acp_model) = session_config.model {
            let resolved = agent::run::resolve_model_config(Some(acp_model)).await;
            tracing::info!(
                acp_model = %acp_model,
                agent = %session_config.current_agent,
                resolved_model = %resolved.model.as_deref().unwrap_or("none"),
                resolution_time_ms = start_time.elapsed().as_millis(),
                "Using ACP selected model, overriding agent tier configuration"
            );
            return resolved;
        }

        if let Some(last_selected) = crate::last_model::load().filter(|m| !m.is_empty() && m != "default") {
            let resolved = agent::run::resolve_model_config(Some(&last_selected)).await;
            tracing::info!(
                last_selected_model = %last_selected,
                agent = %session_config.current_agent,
                resolved_model = %resolved.model.as_deref().unwrap_or("none"),
                resolution_time_ms = start_time.elapsed().as_millis(),
                "No session model configured, using last explicitly selected model"
            );
            return resolved;
        }

        // Try to get model settings from agent profile
        if let Some(profile) = self
            .agent_registry
            .get_agent_config(&session_config.current_agent)
        {
            if let Some(model_config) = profile.model {
                if let Some(ref model_name) = model_config.name {
                    let resolved = agent::run::resolve_model_config(Some(model_name)).await;
                    tracing::info!(
                        model = %model_name,
                        agent = %session_config.current_agent,
                        resolved_model = %resolved.model.as_deref().unwrap_or("none"),
                        resolution_time_ms = start_time.elapsed().as_millis(),
                        "Using agent configured model name"
                    );
                    return resolved;
                }

                if let Some(tier) = model_config.tier {
                    tracing::debug!(
                        tier = ?tier,
                        agent = %session_config.current_agent,
                        "Starting tier-based model resolution"
                    );

                    let mut config = agent::ReactBuildConfig::from_env();
                    config.model_tier = Some(tier);
                    let resolved_config = agent::resolve_tier_and_build_config(&config).await;

                    if resolved_config.model.is_some() {
                        let resolved = agent::run::ResolvedModelConfig {
                            model: resolved_config.model.clone(),
                            provider: resolved_config.llm_provider.clone(),
                            base_url: resolved_config.openai_base_url.clone(),
                            api_key: resolved_config.openai_api_key.clone(),
                            provider_type: resolved_config.llm_provider.clone(),
                            effort: None,
                            tier: None,
                        };

                        tracing::info!(
                            tier = ?tier,
                            agent = %session_config.current_agent,
                            resolved_model = %resolved.model.as_deref().unwrap_or("none"),
                            resolution_time_ms = start_time.elapsed().as_millis(),
                            "Tier resolution successful"
                        );
                        return resolved;
                    }

                    tracing::warn!(
                        tier = ?tier,
                        agent = %session_config.current_agent,
                        "Tier resolution failed, falling back to default provider config"
                    );
                }
            }
        }

        // Default case: no explicit configuration - provide a safe default model
        tracing::info!(
            agent = %session_config.current_agent,
            "No model or tier configuration, resolving from default provider config"
        );

        if let Ok(full_config) = load_full_config("anureo") {
            if let Some(ref pname) = full_config.default_provider {
                if let Some(p) = full_config.providers.iter().find(|p| p.name == *pname) {
                    if let Some(ref model_name) = p.model {
                        let mut resolved = agent::run::resolve_model_config(Some(model_name)).await;
                        if resolved.model.is_some() {
                            if resolved.api_key.is_none() {
                                resolved.api_key = p.api_key.clone();
                            }
                            if resolved.base_url.is_none() {
                                resolved.base_url = p.base_url.clone();
                            }
                            if resolved.provider.is_none() {
                                resolved.provider = Some(p.name.clone());
                            }
                            if resolved.provider_type.is_none() {
                                resolved.provider_type = p.provider_type.clone();
                            }
                            return resolved;
                        }
                    }
                    return agent::run::ResolvedModelConfig {
                        model: p.model.clone(),
                        provider: Some(p.name.clone()),
                        base_url: p.base_url.clone(),
                        api_key: p.api_key.clone(),
                        provider_type: p.provider_type.clone(),
                        effort: None,
                        tier: None,
                    };
                }
            }
        }

        let default_model = config::default_model();
        agent::run::resolve_model_config(Some(&default_model)).await
    }

    fn apply_session_mode(
        &self,
        session_id: &SessionId,
        key: &OurSessionId,
        mode_id: &str,
    ) -> agent_client_protocol::Result<()> {
        if self.sessions.get(key).is_none() {
            return Err(agent_client_protocol::Error::new(-32602, "unknown session"));
        }

        if !self.agent_registry.mode_exists(mode_id) {
            return Err(agent_client_protocol::Error::new(
                -32602,
                format!("unknown mode: {}", mode_id),
            ));
        }

        self.sessions.update_session_config(key, |c| {
            c.current_agent = mode_id.to_string();
        });

        if let Some(tx) = &self.session_update_tx {
            let notifier = SessionNotifier::new(tx.clone(), session_id.clone());
            notifier.try_send_current_mode(mode_id);
        }

        Ok(())
    }
}

impl Default for AnureoAcpAgent {
    fn default() -> Self {
        Self::new().expect("AnureoAcpAgent default init failed")
    }
}

impl AnureoAcpAgent {
    pub async fn initialize(
        &self,
        args: InitializeRequest,
    ) -> agent_client_protocol::Result<InitializeResponse> {
        tracing::info!(protocol_version = ?args.protocol_version, "initialize called");
        let caps_json = serde_json::to_value(&args.client_capabilities).ok();
        let caps = ClientCapabilitiesInfo::from_json(caps_json);
        tracing::info!(
            terminal = caps.supports_terminal(),
            fs_read = caps.can_read_text_file(),
            fs_write = caps.can_write_text_file(),
            mcp_http = caps.supports_mcp_http(),
            prompt_image = caps.supports_prompt_image(),
            "Client capabilities saved"
        );

        // Build base response with proper schema types (Gap 1, Gap 2, Gap 3 fix).
        // Uses builder API to avoid JSON roundtrip that drops unknown fields.
        //
        // v0.14.0 schema fields actually available:
        //   McpCapabilities       { http, sse, acp(unstable) }    — NO stdio
        //   PromptCapabilities    { image, audio, embedded_context } — NO text, NO resource_link
        //   SessionCapabilities   { list, delete, resume, close }
        //   AgentCapabilities     { load_session, mcp_capabilities, prompt_capabilities,
        //                            session_capabilities, auth }
        let mcp = McpCapabilities::new().http(true).sse(false);
        let prompts = PromptCapabilities::new()
            .image(true)
            .audio(true)
            .embedded_context(true);
        // Only advertise methods registered in `stdio_loop`. Resume/close/delete
        // are added to this response in the same change that registers their
        // handlers.
        let session = SessionCapabilities::new()
            .list(SessionListCapabilities::new())
            .delete(SessionDeleteCapabilities::new())
            .resume(SessionResumeCapabilities::new())
            .close(SessionCloseCapabilities::new());

        let mut extension_meta = serde_json::Map::new();
        extension_meta.insert(
            "anureo.dev".to_string(),
            self.extension_registry.build_capability_snapshot(),
        );
        if let Some(anureo) = extension_meta
            .get_mut("anureo.dev")
            .and_then(serde_json::Value::as_object_mut)
        {
            anureo.insert(
                "session-recovery".to_string(),
                serde_json::json!({
                    "version": 1,
                    "cursor": true,
                    "orderedUpdates": true,
                    "promptState": true,
                }),
            );
        }
        let agent_caps = AgentCapabilities::new()
            .load_session(true)
            .mcp_capabilities(mcp)
            .prompt_capabilities(prompts)
            .session_capabilities(session)
            .meta(extension_meta);

        let protocol_version = ProtocolVersion::V1;
        let response = InitializeResponse::new(protocol_version)
            .agent_info(agent_client_protocol::schema::v1::Implementation::new(
                "anureo",
                env!("CARGO_PKG_VERSION"),
            ))
            .agent_capabilities(agent_caps);

        tracing::info!("initialize completed");
        Ok(response)
    }

    pub async fn authenticate(
        &self,
        _args: AuthenticateRequest,
    ) -> agent_client_protocol::Result<AuthenticateResponse> {
        tracing::debug!("authenticate called");
        Ok(AuthenticateResponse::default())
    }

    pub async fn new_session(
        &self,
        args: NewSessionRequest,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        self.new_session_for_owner(args, "local-anonymous").await
    }

    pub async fn new_session_for_owner(
        &self,
        args: NewSessionRequest,
        owner_principal: &str,
    ) -> agent_client_protocol::Result<NewSessionResponse> {
        tracing::debug!(cwd = ?args.cwd, "new_session called");
        let canonical_cwd = canonicalize_existing_directory(&args.cwd)?;
        let extension_meta = args.meta.as_ref().and_then(|meta| meta.get("anureo.dev"));
        if let Some(metadata) = extension_meta.and_then(|meta| meta.get("metadata")) {
            if !metadata.is_object() {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("session metadata must be an object"));
            }
        }
        let parent_session_id = extension_meta
            .and_then(|meta| meta.get("parentSessionId"))
            .and_then(|value| value.as_str());
        if let Some(parent_session_id) = parent_session_id {
            let parent = self
                .session_repository
                .get_index_record(owner_principal, parent_session_id)
                .map_err(|error| {
                    agent_client_protocol::Error::internal_error()
                        .data(format!("failed to validate parentSessionId: {error}"))
                })?
                .ok_or_else(|| {
                    agent_client_protocol::Error::invalid_params()
                        .data("parent session is not available")
                })?;
            if parent.cwd != canonical_cwd || parent.archived_at.is_some() {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("parent session must be active in the same cwd"));
            }
        }
        // Logging is initialized at startup; this is a no-op if already initialized
        crate::logging::init_logging(Some(&canonical_cwd));

        let working_directory = Some(canonical_cwd);
        let our_id = self
            .sessions
            .create_owned(working_directory, owner_principal);
        let session_id = SessionId::new(our_id.as_str().to_string());
        let entry = self.sessions.get(&our_id).ok_or_else(|| {
            agent_client_protocol::Error::internal_error().data("session missing after creation")
        })?;
        let cwd = entry.working_directory.as_ref().ok_or_else(|| {
            agent_client_protocol::Error::internal_error()
                .data("session cwd missing after creation")
        })?;
        let title = extension_meta
            .and_then(|meta| meta.get("title"))
            .and_then(|value| value.as_str());
        let metadata = extension_meta.and_then(|meta| meta.get("metadata"));
        let created_index_records = self
            .session_repository
            .insert_index_record(
                our_id.as_str(),
                &entry.thread_id,
                owner_principal,
                cwd,
                parent_session_id,
                title,
                metadata,
            )
            .map_err(|error| {
                self.sessions.delete(&our_id);
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to persist session metadata: {error}"))
            })?;
        tracing::debug!(session_id = %session_id, "session created");

        // Store MCP servers from ACP session/new request
        if !args.mcp_servers.is_empty() {
            let anureo_mcp = crate::mcp_convert::acp_mcp_to_anureo(&args.mcp_servers);
            tracing::info!(
                session_id = %session_id,
                acp_count = args.mcp_servers.len(),
                anureo_count = anureo_mcp.len(),
                "MCP servers from session/new"
            );
            self.sessions.update_mcp_servers(&our_id, anureo_mcp);
        }

        let default_mode = self.agent_registry.default_mode_id();
        let current_model = crate::last_model::load().unwrap_or_default();
        let is_default = current_model.is_empty() || current_model == "default";
        self.sessions.update_session_config(&our_id, |c| {
            c.current_agent = default_mode.to_string();
            if !is_default {
                c.model = Some(current_model.clone());
            }
        });
        if !is_default {
            if let Err(e) = self.config_store.set(&our_id, "model", &current_model) {
                tracing::warn!(session_id = %our_id, error = %e, "Failed to persist initial model config");
            }
        }
        let display_model = if is_default {
            "default"
        } else {
            &current_model
        };
        let model_options = self.get_available_models().await;
        let current_mode = default_mode;
        let modes = self.agent_registry.to_session_modes();
        let current_effort = "auto";
        let model_reasoning_efforts = self.get_model_reasoning_efforts(display_model).await;
        let config_options = build_session_config_options(
            current_mode,
            display_model,
            current_effort,
            &modes,
            &model_options,
            model_reasoning_efforts.as_deref(),
        )
        .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;
        // Curator opportunistic hook (Hermes `agent/curator.py` #12 —
        // `maybe_run_curator` is defined at
        // `experimental/curator/src/workflow.rs:290` but had zero callers
        // in `apps/`). On every `session/new`, read the session idle
        // duration via `SkillUsageStore` (cheap mtime-based) and, if
        // the curator's interval gating decides a run is needed,
        // spawn the LLM pass as a detached task so the ACP handshake
        // isn't blocked on the LLM round-trip.
        //
        // Tracked using `tracing::info!` so missing-runs show up in
        // `anureo-acp` logs; `maybe_run_curator` itself swallows errors
        // and returns `None`.
        let cwd_for_curator = args.cwd.clone();
        tokio::spawn(async move {
            let skills_path = anureo_curator::skill_registry::default_path();
            let cfg = anureo_curator::CuratorConfig::default();
            let base_config = agent::ReactBuildConfig::from_env();
            let _ = cwd_for_curator; // reserved for future path resolution
                                     // Priority #17 (Hermes `agent/curator.py` #13): source the
                                     // idle gate from session-state so the auto-spawned path
                                     // honors the same idle threshold as a manual `curator run`.
                                     // Round-2 left this as `default()` with no `idle_for_seconds`
                                     // threading — the curator ran regardless of recent
                                     // activity. We resolve the idle window from the
                                     // `ANUREO_CURATOR_IDLE_SECS` env var (default 300s) so
                                     // operators can tune it without a recompile.
            let idle_for_seconds = std::env::var("ANUREO_CURATOR_IDLE_SECS")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(300);
            tracing::info!(
                "session/new — opportunistically running curator (idle_for_seconds={})",
                idle_for_seconds
            );
            anureo_curator::workflow::maybe_run_curator(
                &skills_path,
                &cfg,
                base_config,
                Some(idle_for_seconds),
            )
            .await;
        });
        let mut created_records = created_index_records.into_iter();
        let canonical = created_records.next();
        let affected_ancestors = created_records.collect::<Vec<_>>();
        let mut meta = serde_json::Map::new();
        if let Some(canonical) = canonical {
            let affected_sessions = affected_ancestors
                .iter()
                .map(|record| {
                    serde_json::json!({
                        "sessionId": record.session_id,
                        "parentSessionId": record.parent_session_id,
                        "cwd": record.cwd,
                        "title": record.title,
                        "metadata": record.metadata,
                        "createdAt": record.created_at,
                        "activityAt": record.activity_at,
                        "treeActivityAt": record.tree_activity_at,
                        "stateChangedAt": record.state_changed_at,
                        "metadataUpdatedAt": record.metadata_updated_at,
                        "archivedAt": record.archived_at,
                        "closedAt": record.closed_at,
                        "lifecycle": record.lifecycle,
                        "revision": record.revision,
                        "indexVersion": record.index_version,
                    })
                })
                .collect::<Vec<_>>();
            meta.insert(
                "anureo.dev".into(),
                serde_json::json!({
                    "session": {
                        "sessionId": canonical.session_id,
                        "parentSessionId": canonical.parent_session_id,
                        "cwd": canonical.cwd,
                        "title": canonical.title,
                        "metadata": canonical.metadata,
                        "createdAt": canonical.created_at,
                        "activityAt": canonical.activity_at,
                        "treeActivityAt": canonical.tree_activity_at,
                        "stateChangedAt": canonical.state_changed_at,
                        "metadataUpdatedAt": canonical.metadata_updated_at,
                        "archivedAt": canonical.archived_at,
                        "closedAt": canonical.closed_at,
                        "lifecycle": canonical.lifecycle,
                        "revision": canonical.revision,
                        "indexVersion": canonical.index_version,
                    },
                    "affectedSessions": affected_sessions,
                    "indexVersion": canonical.index_version,
                }),
            );
        }
        Ok(NewSessionResponse::new(session_id)
            .modes(self.agent_registry.to_session_mode_state(current_mode))
            .config_options(config_options)
            .meta(meta))
    }

    pub async fn cancel(&self, args: CancelNotification) -> agent_client_protocol::Result<()> {
        tracing::debug!(session_id = %args.session_id, "cancel called");
        let key = OurSessionId::new(args.session_id.to_string());
        self.sessions.cancel_current_generation(&key);
        Ok(())
    }

    pub fn cancel_all(&self) {
        self.sessions.cancel_all_generations();
    }

    pub async fn set_session_config_option(
        &self,
        args: SetSessionConfigOptionRequest,
    ) -> agent_client_protocol::Result<SetSessionConfigOptionResponse> {
        tracing::debug!(session_id = %args.session_id, config_id = ?args.config_id, value = ?args.value, "set_session_config_option called");
        let key = OurSessionId::new(args.session_id.to_string());
        if self.sessions.get(&key).is_none() {
            return Err(agent_client_protocol::Error::new(-32602, "unknown session"));
        }
        let config_id_str = args.config_id.to_string();
        let value_str = session_config_value_as_id(&args.value).ok_or_else(|| {
            agent_client_protocol::Error::new(
                -32602,
                format!(
                    "unsupported value type for config_id {}: expected select value id",
                    config_id_str
                ),
            )
        })?;
        match config_id_str.as_str() {
            "model" => {
                if value_str == "default" {
                    self.sessions
                        .update_session_config(&key, |c| c.model = None);
                    crate::last_model::clear();
                } else {
                    self.sessions
                        .update_session_config(&key, |c| c.model = Some(value_str.clone()));
                    crate::last_model::save(&value_str);
                }
                if let Err(e) = self.config_store.set(&key, "model", &value_str) {
                    tracing::warn!(session_id = %args.session_id, error = %e, "Failed to persist model config");
                }
            }
            "mode" => {
                self.apply_session_mode(&args.session_id, &key, &value_str)?;
                // Persist to database
                if let Err(e) = self.config_store.set(&key, "mode", &value_str) {
                    tracing::warn!(session_id = %args.session_id, error = %e, "Failed to persist mode config");
                }
            }
            "effort" => {
                let valid = matches!(
                    value_str.as_str(),
                    "auto" | "none" | "minimal" | "low" | "medium" | "high" | "xhigh"
                );
                if !valid {
                    return Err(agent_client_protocol::Error::new(
                        -32602,
                        format!("invalid effort value: {}", value_str),
                    ));
                }
                if value_str == "auto" {
                    self.sessions
                        .update_session_config(&key, |c| c.effort = None);
                } else {
                    self.sessions
                        .update_session_config(&key, |c| c.effort = Some(value_str.clone()));
                }
                if let Err(e) = self.config_store.set(&key, "effort", &value_str) {
                    tracing::warn!(session_id = %args.session_id, error = %e, "Failed to persist effort config");
                }
            }
            _ => {
                return Err(agent_client_protocol::Error::new(
                    -32602,
                    format!("unsupported config_id: {}", config_id_str),
                ));
            }
        }

        let entry = self
            .sessions
            .get(&key)
            .ok_or_else(|| agent_client_protocol::Error::new(-32602, "unknown session"))?;
        let current_mode = if entry.session_config.current_agent.is_empty() {
            self.agent_registry.default_mode_id().to_string()
        } else {
            entry.session_config.current_agent.clone()
        };
        let current_model = entry
            .session_config
            .model
            .clone()
            .unwrap_or_else(|| crate::last_model::load().unwrap_or_default());
        let modes = self.agent_registry.to_session_modes();
        let model_options = self.get_available_models().await;
        let current_effort = entry
            .session_config
            .effort
            .clone()
            .unwrap_or_else(|| "auto".to_string());
        let model_reasoning_efforts = self.get_model_reasoning_efforts(&current_model).await;
        build_set_session_config_option_response(
            &current_mode,
            &current_model,
            &current_effort,
            &modes,
            &model_options,
            model_reasoning_efforts.as_deref(),
        )
        .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))
    }

    pub async fn set_session_mode(
        &self,
        args: SetSessionModeRequest,
    ) -> agent_client_protocol::Result<SetSessionModeResponse> {
        let mode_id = args.mode_id.to_string();
        tracing::debug!(session_id = %args.session_id, mode_id = %mode_id, "set_session_mode called");

        let key = OurSessionId::new(args.session_id.to_string());
        self.apply_session_mode(&args.session_id, &key, &mode_id)?;

        // Persist to database
        if let Err(e) = self.config_store.set(&key, "mode", &mode_id) {
            tracing::warn!(session_id = %args.session_id, error = %e, "Failed to persist mode config");
        }

        Ok(SetSessionModeResponse::new())
    }

    pub async fn fork_session(
        &self,
        args: ForkSessionRequest,
    ) -> agent_client_protocol::Result<ForkSessionResponse> {
        tracing::debug!(session_id = %args.session_id, cwd = ?args.cwd, "fork_session called");
        let canonical_cwd = canonicalize_existing_directory(&args.cwd)?;
        // Logging is initialized at startup; this is a no-op if already initialized
        crate::logging::init_logging(Some(&canonical_cwd));

        let source_key = OurSessionId::new(args.session_id.to_string());
        let source_entry = self
            .sessions
            .get(&source_key)
            .ok_or_else(|| agent_client_protocol::Error::new(-32602, "unknown session"))?;
        if source_entry.working_directory.as_ref() != Some(&canonical_cwd) {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("cwd does not match the session working directory"));
        }

        // Create new session with the same working directory and config
        let new_our_id = self.sessions.create_owned(
            source_entry.working_directory.clone(),
            source_entry.owner_principal.clone(),
        );
        let new_session_id = SessionId::new(new_our_id.as_str().to_string());
        let new_entry = self.sessions.get(&new_our_id).ok_or_else(|| {
            agent_client_protocol::Error::internal_error()
                .data("forked session missing after creation")
        })?;
        self.session_repository
            .insert(
                new_our_id.as_str(),
                &new_entry.thread_id,
                &new_entry.owner_principal,
                new_entry.working_directory.as_ref().ok_or_else(|| {
                    agent_client_protocol::Error::internal_error()
                        .data("forked session cwd missing")
                })?,
            )
            .map_err(|error| {
                self.sessions.delete(&new_our_id);
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to persist forked session: {error}"))
            })?;

        // Copy source session config (model, mode) to the new session
        self.sessions.update_session_config(&new_our_id, |c| {
            *c = source_entry.session_config.clone();
        });

        // Inherit MCP servers from source session
        self.sessions
            .update_mcp_servers(&new_our_id, source_entry.mcp_servers.clone());

        // Copy persistent config from source to target
        if let Err(e) = self.config_store.copy_config(&source_key, &new_our_id) {
            tracing::warn!(
                source_session = %args.session_id,
                target_session = %new_session_id,
                error = %e,
                "Failed to copy persistent config during fork"
            );
        }
        tracing::info!(source_session = %args.session_id, new_session = %new_session_id, "session forked");

        let current_mode = if source_entry.session_config.current_agent.is_empty() {
            self.agent_registry.default_mode_id().to_string()
        } else {
            source_entry.session_config.current_agent.clone()
        };
        let current_model = source_entry
            .session_config
            .model
            .clone()
            .unwrap_or_else(|| crate::last_model::load().unwrap_or_default());
        // If model was resolved from fallback rather than source config, persist it
        if !current_model.is_empty() && source_entry.session_config.model.is_none() {
            self.sessions.update_session_config(&new_our_id, |c| {
                c.model = Some(current_model.clone());
            });
            if let Err(e) = self.config_store.set(&new_our_id, "model", &current_model) {
                tracing::warn!(
                    session_id = %new_session_id,
                    error = %e,
                    "Failed to persist initial model config in forked session"
                );
            }
        }

        let model_options = self.get_available_models().await;
        let modes = self.agent_registry.to_session_modes();
        let current_effort = source_entry
            .session_config
            .effort
            .clone()
            .unwrap_or_else(|| "auto".to_string());
        let model_reasoning_efforts = self.get_model_reasoning_efforts(&current_model).await;
        let config_options = build_session_config_options(
            &current_mode,
            &current_model,
            &current_effort,
            &modes,
            &model_options,
            model_reasoning_efforts.as_deref(),
        )
        .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;

        Ok(ForkSessionResponse::new(new_session_id)
            .modes(self.agent_registry.to_session_mode_state(&current_mode))
            .config_options(config_options))
    }

    pub async fn prompt(
        &self,
        args: PromptRequest,
    ) -> agent_client_protocol::Result<PromptResponse> {
        self.prompt_with_capabilities(
            args,
            ClientCapabilitiesInfo::default(),
            Arc::new(NoOpClientBridge),
        )
        .await
    }

    /// Execute a prompt using capabilities from the caller's transport.
    pub async fn prompt_with_capabilities(
        &self,
        args: PromptRequest,
        client_capabilities: ClientCapabilitiesInfo,
        client_bridge: Arc<dyn ClientBridgeTrait>,
    ) -> agent_client_protocol::Result<PromptResponse> {
        tracing::debug!(session_id = %args.session_id, prompt_blocks = args.prompt.len(), "prompt called");
        let key = OurSessionId::new(args.session_id.to_string());
        let entry = self
            .sessions
            .get(&key)
            .ok_or_else(|| agent_client_protocol::Error::new(-32602, "unknown session"))?;

        let cancellation = self.sessions.begin_prompt(&key).ok_or_else(|| {
            agent_client_protocol::Error::new(
                -32010,
                "a prompt is already in progress for this session",
            )
        })?;

        // RAII guard: ensures finish_prompt is called even if the future is
        // dropped (e.g., WS disconnect cancels the task mid-prompt).
        let _prompt_guard =
            crate::session::PromptGuard::new(&self.sessions, &key, cancellation.generation());

        let user_content =
            content_blocks_to_user_content(args.prompt.as_slice()).map_err(|_| {
                agent_client_protocol::Error::new(-32602, "content_blocks parse failed")
            })?;

        // Activity is recorded only after the prompt has been accepted and
        // the busy lease is held, immediately before any command/executor
        // work starts. This keeps sidebar recency independent of completion
        // timing and makes the mutation atomic with ancestor propagation.
        self.session_repository
            .record_activity(&args.session_id.to_string())
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to record session activity: {error}"))
            })?;

        self.seed_session_title_from_prompt(&args.session_id, &user_content);

        if let anureo_llm::message::UserContent::Text(ref text) = user_content {
            if let Some(cmd) = agent::commands::parse(text) {
                match cmd {
                    agent::commands::Command::ResetContext => {
                        self.sessions.cancel_current_generation(&key);
                        tracing::info!(session_id = %args.session_id, "Context cleared via /reset command");
                        return Ok(PromptResponse::new(StopReason::EndTurn));
                    }
                    agent::commands::Command::Goal { description } => {
                        tracing::info!(
                            session_id = %args.session_id,
                            goal = %description,
                            "Goal mode activated via /goal command"
                        );
                        let working_folder = entry.working_directory.clone().ok_or_else(|| {
                            agent_client_protocol::Error::internal_error()
                                .data("ACP session has no working directory")
                        })?;

                        let resolved_goal = self
                            .resolve_model_with_tier_awareness(&entry.session_config)
                            .await;
                        let goal_ctx_window =
                            resolve_context_window_size(resolved_goal.model.as_deref()).await;

                        let event_sender: Option<
                            std::sync::Arc<dyn Fn(agent::run::TypedAnyStreamEvent) + Send + Sync>,
                        > = self.session_update_tx.clone().map(|sender| {
                            let session_id = args.session_id.clone();
                            std::sync::Arc::new(move |ev: agent::run::TypedAnyStreamEvent| {
                                let notifier =
                                    SessionNotifier::new(sender.clone(), session_id.clone())
                                        .with_context_window_size(goal_ctx_window);
                                notifier.try_send_stream_event(&ev);
                            })
                                as std::sync::Arc<
                                    dyn Fn(agent::run::TypedAnyStreamEvent) + Send + Sync,
                                >
                        });

                        let cancel = tokio_util::sync::CancellationToken::new();

                        let result = crate::goal_runner::run_goal(
                            description,
                            working_folder,
                            cancel,
                            event_sender,
                            Some(cancellation.clone()),
                        )
                        .await;

                        match result {
                            Ok(goal_result) => {
                                tracing::info!(
                                    session_id = %args.session_id,
                                    task_id = %goal_result.task_id,
                                    outcome = %goal_result.outcome,
                                    "Goal finished"
                                );
                                return Ok(PromptResponse::new(StopReason::EndTurn));
                            }
                            Err(e) => {
                                tracing::error!(
                                    session_id = %args.session_id,
                                    error = %e,
                                    "Goal run failed"
                                );
                                return Ok(PromptResponse::new(StopReason::EndTurn));
                            }
                        }
                    }
                    agent::commands::Command::ReviewSkill { scope } => {
                        tracing::info!(
                            session_id = %args.session_id,
                            scope = ?scope,
                            "Review-skill command triggered"
                        );
                        let resolved = self
                            .resolve_model_with_tier_awareness(&entry.session_config)
                            .await;
                        let (review_memory, review_skills) =
                            crate::review_runner::scope_to_review_config(&scope);
                        crate::review_runner::spawn_inprocess_review(
                            entry.thread_id.clone(),
                            resolved,
                            review_memory,
                            review_skills,
                            "review-skill".to_string(),
                            self.session_update_tx.clone(),
                            Some(args.session_id.clone()),
                        );
                        return Ok(PromptResponse::new(StopReason::EndTurn));
                    }
                    agent::commands::Command::Models { .. }
                    | agent::commands::Command::ModelsUse { .. } => {
                        // ACP handles models via SetSessionConfigOption, not here
                    }
                    _ => {
                        return Ok(PromptResponse::new(StopReason::EndTurn));
                    }
                }
            }
        }

        let content_type = match &user_content {
            anureo_llm::message::UserContent::Text(_) => "text",
            anureo_llm::message::UserContent::Multimodal(parts) => {
                let has_image = parts
                    .iter()
                    .any(|p| matches!(p, anureo_llm::message::ContentPart::ImageBase64 { .. }));
                let has_audio = parts
                    .iter()
                    .any(|p| matches!(p, anureo_llm::message::ContentPart::AudioBase64 { .. }));
                if has_image && has_audio {
                    "multimodal(image+audio)"
                } else if has_image {
                    "multimodal(image)"
                } else if has_audio {
                    "multimodal(audio)"
                } else {
                    "multimodal"
                }
            }
        };
        tracing::info!(
            session_id = %args.session_id,
            content_type = content_type,
            text_len = user_content.as_text().len(),
            "User prompt"
        );

        let working_folder = entry.working_directory.clone().ok_or_else(|| {
            agent_client_protocol::Error::internal_error()
                .data("ACP session has no working directory")
        })?;

        let resolved = self
            .resolve_model_with_tier_awareness(&entry.session_config)
            .await;
        let resolved_for_review = resolved.clone();

        let context_window_size = resolve_context_window_size(resolved.model.as_deref()).await;

        let opts = RunOptions {
            message: user_content,
            working_folder: Some(working_folder),
            session_id: None,
            cancellation: Some(cancellation.clone()),
            thread_id: Some(entry.thread_id.clone()),
            agent: Some(
                self.agent_registry
                    .resolve_agent_name(&entry.session_config.current_agent),
            ),
            verbose: false,
            verbose_level: 0,
            got_adaptive: false,
            display_max_len: 4096,
            output_json: false,
            model: resolved.model,
            mcp_config_path: None,
            output_timestamp: false,
            dry_run: false,
            debug_llm: false,
            provider: resolved.provider,
            base_url: resolved.base_url,
            api_key: resolved.api_key,
            provider_type: resolved.provider_type,
            any_stream_event_sender: None,
            acp_session_id: Some(args.session_id.to_string()),
            bash_executor: {
                if client_capabilities.supports_terminal() {
                    tracing::info!("Using ACP client terminal executor");
                    Some(Arc::new(crate::tools::AcpBridgeCommandExecutor::new(
                        client_bridge.clone(),
                    ))
                        as Arc<dyn tool_basic::bash::CommandExecutor>)
                } else {
                    tracing::info!("Using local bash executor (ACP terminal unavailable)");
                    Some(Arc::new(LocalCommandExecutor)
                        as Arc<dyn tool_basic::bash::CommandExecutor>)
                }
            },
            extra_tools: {
                let tools = create_acp_tools(&client_capabilities, client_bridge.clone());
                if tools.is_empty() {
                    None
                } else {
                    tracing::info!(count = tools.len(), "Registering ACP tools");
                    Some(Arc::new(
                        tools
                            .into_iter()
                            .map(|t| Arc::from(t) as Arc<dyn tool_core::Tool>)
                            .collect(),
                    ))
                }
            },
            default_extra_tools_provider: Some(tool_workflow::default_workflow_tool_provider()),
            force_compact: false,
            chat_id: None,
            worktree: false,
            goal_mode: false,
            acp_mcp_servers: if entry.mcp_servers.is_empty() {
                None
            } else {
                Some(entry.mcp_servers.clone())
            },
            acp_mcp_sources: if entry.mcp_servers.is_empty() {
                None
            } else {
                Some(
                    entry
                        .mcp_runtime
                        .ensure_sources(&entry.mcp_servers)
                        .await
                        .map_err(|error| {
                            agent_client_protocol::Error::internal_error().data(error)
                        })?,
                )
            },
            effort: resolved.effort,
            tier: resolved.tier,
        };

        let session_id = args.session_id.clone();
        let tx = self.session_update_tx.clone();
        let usage_acc: Arc<Mutex<TurnUsage>> = Arc::new(Mutex::new(TurnUsage::default()));
        let on_event: Option<Box<dyn FnMut(TypedAnyStreamEvent) + Send>> = {
            let acc = usage_acc.clone();
            match tx {
                Some(ref sender) => {
                    let notifier = SessionNotifier::new(sender.clone(), session_id.clone())
                        .with_context_window_size(context_window_size)
                        .with_usage_acc(acc.clone());

                    // Enable high-frequency tracking with estimated base usage
                    // Base usage estimated from prompt message (rough approximation)
                    let estimated_base_tokens = match &opts.message {
                        UserContent::Text(text) => text.len() / 4, // Approx 4 chars per token
                        UserContent::Multimodal(parts) => {
                            parts
                                .iter()
                                .map(|p| {
                                    match p {
                                        anureo_llm::message::ContentPart::Text { text } => {
                                            text.len() / 4
                                        }
                                        _ => 0, // Non-text parts estimated as 0 tokens
                                    }
                                })
                                .sum::<usize>()
                        }
                    };
                    notifier.enable_high_freq_tracking(
                        estimated_base_tokens as u64,
                        context_window_size,
                    );

                    let title_repository = self.session_repository.clone();
                    let title_session_id = session_id.to_string();
                    let closure = move |ev: TypedAnyStreamEvent| {
                        capture_turn_usage(&ev, &acc);

                        // Spawn background task for async title persistence
                        let repo = title_repository.clone();
                        let session = title_session_id.clone();
                        let event = ev.clone();
                        tokio::spawn(async move {
                            persist_session_title(&repo, &session, &event).await;
                        });

                        notifier.try_send_event(&ev);
                    };
                    Some(Box::new(closure) as Box<dyn FnMut(TypedAnyStreamEvent) + Send>)
                }
                None => None,
            }
        };

        let (config, _, _) = build_react_config(&opts);
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
            on_event,
        )
        .await;

        // Disable high-frequency tracking after agent execution
        if let Some(ref tx) = tx {
            let notifier = SessionNotifier::new(tx.clone(), session_id.clone());
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                notifier.disable_high_freq_tracking().await;
            });
        }

        self.sessions.finish_prompt(&key, cancellation.generation());

        match result {
            Ok(RunCompletion::Finished(_reply)) => {
                crate::review_runner::spawn_inprocess_review(
                    entry.thread_id.clone(),
                    resolved_for_review,
                    true,
                    true,
                    "background".to_string(),
                    self.session_update_tx.clone(),
                    Some(session_id),
                );
                let mut resp = PromptResponse::new(StopReason::EndTurn);
                if let Some(usage) = build_acp_usage(&usage_acc) {
                    resp = resp.usage(usage);
                }
                Ok(resp)
            }
            Ok(RunCompletion::Cancelled) => Ok(PromptResponse::new(StopReason::Cancelled)),
            Err(e) => {
                tracing::error!(session_id = %args.session_id, error = %e, "run_agent errored");
                Err(map_run_error(e))
            }
        }
    }

    pub async fn load_session(
        &self,
        args: LoadSessionRequest,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        self.load_session_for_owner(args, "local-anonymous").await
    }

    pub async fn load_session_for_owner(
        &self,
        args: LoadSessionRequest,
        owner_principal: &str,
    ) -> agent_client_protocol::Result<LoadSessionResponse> {
        tracing::info!(session_id = %args.session_id, cwd = ?args.cwd, "load_session started");
        let request_value = serde_json::to_value(&args).ok();
        let recovery_meta = request_value
            .as_ref()
            .and_then(|value| value.get("_meta"))
            .and_then(|value| value.get("anureo.dev"))
            .and_then(|value| value.get("sessionRecovery"));
        let recovery_delta = recovery_meta
            .and_then(|value| value.get("cursor"))
            .is_some();
        let recovery_requested = recovery_meta.is_some();
        let canonical_cwd = canonicalize_existing_directory(&args.cwd)?;
        // Logging is initialized at startup; this is a no-op if already initialized
        crate::logging::init_logging(Some(&canonical_cwd));
        let session_id = args.session_id.clone();
        let our_session_id = OurSessionId::new(session_id.to_string());
        let entry_was_created = self.sessions.get(&our_session_id).is_none();
        let entry = if let Some(existing) = self.sessions.get(&our_session_id) {
            if existing.owner_principal != owner_principal {
                return Err(agent_client_protocol::Error::new(
                    -32000,
                    "session not available for this principal",
                ));
            }
            if existing.working_directory.as_ref() != Some(&canonical_cwd) {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("cwd does not match the session working directory"));
            }
            tracing::info!(
                session_id = %session_id,
                thread_id = %existing.thread_id,
                "Reusing existing session entry from memory"
            );
            existing
        } else {
            let metadata = self
                .session_repository
                .get(our_session_id.as_str())
                .map_err(|error| {
                    agent_client_protocol::Error::internal_error()
                        .data(format!("failed to read session metadata: {error}"))
                })?
                .ok_or_else(|| agent_client_protocol::Error::new(-32002, "session not found"))?;
            if metadata.owner_principal != owner_principal {
                return Err(agent_client_protocol::Error::new(
                    -32000,
                    "session not available for this principal",
                ));
            }
            if metadata.cwd != canonical_cwd {
                return Err(agent_client_protocol::Error::invalid_params()
                    .data("cwd does not match the session working directory"));
            }
            tracing::info!(
                session_id = %session_id,
                thread_id = %metadata.thread_id,
                "Restoring session entry from durable metadata"
            );
            self.sessions.create_with_owner(
                our_session_id.clone(),
                Some(metadata.cwd),
                metadata.thread_id,
                metadata.owner_principal,
            );
            self.sessions.mark_loading(&our_session_id);
            let default_mode = self.agent_registry.default_mode_id();
            self.sessions.update_session_config(&our_session_id, |c| {
                if c.current_agent.is_empty() {
                    c.current_agent = default_mode.to_string();
                }
            });
            self.sessions.get(&our_session_id).ok_or_else(|| {
                tracing::error!(session_id = %our_session_id, "Session not found after creation");
                agent_client_protocol::Error::internal_error().data(format!(
                    "Session {} not found after creation",
                    our_session_id
                ))
            })?
        };

        let db_path = self.checkpoint_db_path.clone();
        tracing::debug!(
            session_id = %session_id,
            thread_id = %entry.thread_id,
            db_path = %db_path.display(),
            "Querying checkpoint for session history"
        );
        let serializer = Arc::new(JsonSerializer);
        let checkpointer: Arc<dyn Checkpointer<ReActState>> = Arc::new(
            SqliteSaver::new(db_path.to_string_lossy().as_ref(), serializer).map_err(|e| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("Failed to create checkpointer: {}", e))
            })?,
        );

        let config = RunnableConfig {
            thread_id: Some(entry.thread_id.clone()),
            checkpoint_id: None,
            checkpoint_ns: String::new(),
            user_id: None,
            resume_from_node_id: None,
            depth: None,
            acp_session_id: None,
            resume_value: None,
            resume_values_by_namespace: Default::default(),
            resume_values_by_interrupt_id: Default::default(),
        };

        // A completed prompt response can reach the client just before the
        // SQLite saver makes its final checkpoint visible to a newly opened
        // connection. Retry a short, bounded window so an immediate reload
        // does not turn a durable conversation into an empty replay.
        let checkpoint = {
            let mut last_error = None;
            let mut found = None;
            for attempt in 0..=5 {
                match checkpointer.get_tuple(&config).await {
                    Ok(Some(value)) => {
                        found = Some(value);
                        break;
                    }
                    Ok(None) if attempt < 5 => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Ok(None) => break,
                    Err(error) => {
                        last_error = Some(error);
                        break;
                    }
                }
            }
            match last_error {
                Some(error) => Err(error),
                None => Ok(found),
            }
        };

        if !recovery_delta {
            match checkpoint {
                Ok(Some((checkpoint, _metadata))) => {
                    let state: ReActState = checkpoint.channel_values;
                    let user_count = state
                        .messages
                        .iter()
                        .filter(|m| matches!(m, anureo_llm::message::Message::User(_)))
                        .count();
                    let assistant_count = state
                        .messages
                        .iter()
                        .filter(|m| matches!(m, anureo_llm::message::Message::Assistant(_)))
                        .count();
                    let tool_count = state
                        .messages
                        .iter()
                        .filter(|m| matches!(m, anureo_llm::message::Message::Tool { .. }))
                        .count();
                    let system_count = state
                        .messages
                        .iter()
                        .filter(|m| matches!(m, anureo_llm::message::Message::System(_)))
                        .count();

                    tracing::info!(
                        session_id = %session_id,
                        thread_id = %entry.thread_id,
                        total = state.messages.len(),
                        user = user_count,
                        assistant = assistant_count,
                        tool = tool_count,
                        system = system_count,
                        "Checkpoint found, replaying session history"
                    );

                    let replay_start =
                        history_tail_start(&state.messages, load_history_tail_limit());
                    if replay_start > 0 {
                        tracing::info!(
                            session_id = %session_id,
                            thread_id = %entry.thread_id,
                            total = state.messages.len(),
                            replay_start,
                            "Tail-truncating history replay; earlier pages via _anureo.dev/session-history/page"
                        );
                    }
                    entry.history_cursor.store(replay_start, Ordering::Release);

                    if let Some(ref tx) = self.session_update_tx {
                        let notifier = SessionNotifier::new(tx.clone(), session_id.clone());
                        notifier
                            .send_history(
                                &state.messages[replay_start..],
                                replay_start,
                                recovery_requested,
                            )
                            .await;
                    } else {
                        tracing::warn!(
                            session_id = %session_id,
                            "No session_update_tx available, history not sent to client"
                        );
                    }

                    tracing::info!(
                        session_id = %session_id,
                        message_count = state.messages.len(),
                        "Session history replay completed"
                    );
                }
                Ok(None) => {
                    tracing::info!(
                        session_id = %session_id,
                        thread_id = %entry.thread_id,
                        "No checkpoint found for session, starting fresh"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        session_id = %session_id,
                        thread_id = %entry.thread_id,
                        error = %e,
                        "Failed to load checkpoint, starting fresh"
                    );
                }
            }
        }

        // Convert and store MCP servers from the load request
        if !args.mcp_servers.is_empty() {
            let anureo_mcp = crate::mcp_convert::acp_mcp_to_anureo(&args.mcp_servers);
            tracing::info!(
                session_id = %session_id,
                acp_count = args.mcp_servers.len(),
                anureo_count = anureo_mcp.len(),
                "MCP servers from session/load"
            );
            self.sessions.update_mcp_servers(&our_session_id, anureo_mcp);
        }

        // Return LoadSessionResponse with config_options and modes
        // First, try to load from persistent store
        let persisted_config = self.config_store.get_all(&our_session_id)
            .unwrap_or_else(|e| {
                tracing::warn!(session_id = %session_id, error = %e, "Failed to load persistent config");
                std::collections::HashMap::new()
            });

        let current_mode = persisted_config.get("mode").cloned().unwrap_or_else(|| {
            if entry.session_config.current_agent.is_empty() {
                self.agent_registry.default_mode_id().to_string()
            } else {
                entry.session_config.current_agent.clone()
            }
        });

        let current_model = persisted_config.get("model").cloned().unwrap_or_else(|| {
            entry
                .session_config
                .model
                .clone()
                .unwrap_or_else(|| crate::last_model::load().unwrap_or_default())
        });

        let current_effort = persisted_config.get("effort").cloned().unwrap_or_else(|| {
            entry
                .session_config
                .effort
                .clone()
                .unwrap_or_else(|| "auto".to_string())
        });
        if current_effort == "auto" {
            self.sessions
                .update_session_config(&our_session_id, |c| c.effort = None);
        } else {
            self.sessions.update_session_config(&our_session_id, |c| {
                c.effort = Some(current_effort.clone())
            });
        }

        let model_options = self.get_available_models().await;
        let available_modes = self.agent_registry.to_session_modes();
        let model_reasoning_efforts = self.get_model_reasoning_efforts(&current_model).await;
        let config_options = build_session_config_options(
            &current_mode,
            &current_model,
            &current_effort,
            &available_modes,
            &model_options,
            model_reasoning_efforts.as_deref(),
        )
        .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;

        let modes = self.agent_registry.to_session_mode_state(&current_mode);

        // Build LoadSessionResponse with configOptions and modes (protocol types are non_exhaustive)
        let json = serde_json::json!({
            "configOptions": config_options,
            "modes": modes,
            "meta": None::<()>
        });
        let response: LoadSessionResponse = serde_json::from_value(json)
            .map_err(|e| agent_client_protocol::Error::internal_error().data(e.to_string()))?;
        if (!recovery_delta || entry_was_created)
            && self
                .sessions
                .finish_restore(&our_session_id, SessionLifecycle::Idle)
        {
            self.session_repository
                .set_lifecycle(our_session_id.as_str(), "idle")
                .map_err(|error| {
                    agent_client_protocol::Error::internal_error()
                        .data(format!("failed to persist session lifecycle: {error}"))
                })?;
        }
        Ok(response)
    }

    // -----------------------------------------------------------------
    // Session history paging (`_anureo.dev/session-history/*`)
    // -----------------------------------------------------------------

    async fn read_checkpoint_messages(&self, thread_id: &str) -> Result<Vec<Message>, String> {
        let db_path = self.checkpoint_db_path.clone();
        let serializer = Arc::new(JsonSerializer);
        let checkpointer: Arc<dyn Checkpointer<ReActState>> = Arc::new(
            SqliteSaver::new(db_path.to_string_lossy().as_ref(), serializer)
                .map_err(|e| format!("failed to create checkpointer: {e}"))?,
        );
        let config = RunnableConfig {
            thread_id: Some(thread_id.to_string()),
            checkpoint_id: None,
            checkpoint_ns: String::new(),
            user_id: None,
            resume_from_node_id: None,
            depth: None,
            acp_session_id: None,
            resume_value: None,
            resume_values_by_namespace: Default::default(),
            resume_values_by_interrupt_id: Default::default(),
        };
        match checkpointer.get_tuple(&config).await {
            Ok(Some((checkpoint, _metadata))) => Ok(checkpoint.channel_values.messages),
            Ok(None) => Ok(Vec::new()),
            Err(e) => Err(format!("checkpoint read failed: {e}")),
        }
    }

    pub async fn session_history_info(
        &self,
        session_id: &str,
    ) -> Result<SessionHistoryInfo, String> {
        let entry = self
            .sessions
            .get(&OurSessionId::new(session_id.to_string()))
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let total = self.read_checkpoint_messages(&entry.thread_id).await?.len();
        let raw = entry.history_cursor.load(Ordering::Acquire);
        let loaded_start = if raw == usize::MAX {
            total
        } else {
            raw.min(total)
        };
        Ok(SessionHistoryInfo {
            session_id: session_id.to_string(),
            total_messages: total,
            loaded_start_index: loaded_start,
            has_more: loaded_start > 0,
        })
    }

    pub async fn session_history_page(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<SessionHistoryPage, String> {
        let entry = self
            .sessions
            .get(&OurSessionId::new(session_id.to_string()))
            .ok_or_else(|| format!("session not found: {session_id}"))?;
        let messages = self.read_checkpoint_messages(&entry.thread_id).await?;
        let total = messages.len();
        let limit = limit.clamp(1, 200);

        // Serialize concurrent pages on the control lock so overlapping page
        // requests cannot consume the same slice twice.
        let _guard = entry
            .control_lock
            .lock()
            .map_err(|_| "session control lock poisoned".to_string())?;
        let raw = entry.history_cursor.load(Ordering::Acquire);
        let cursor = if raw == usize::MAX {
            total
        } else {
            raw.min(total)
        };
        if cursor == 0 {
            return Ok(SessionHistoryPage {
                session_id: session_id.to_string(),
                total_messages: total,
                has_more: false,
                messages: Vec::new(),
            });
        }
        let mut start = cursor.saturating_sub(limit);
        while start > 0 && matches!(messages[start], Message::Tool { .. }) {
            start -= 1;
        }

        let mut owned = messages[start..cursor].to_vec();
        anureo_llm::message::strip_background_review_in_messages(&mut owned);
        let mut page: Vec<SessionHistoryMessage> = Vec::with_capacity(owned.len());
        for (offset, message) in owned.iter().enumerate() {
            let Some(updates) =
                SessionNotifier::message_session_updates(session_id, start + offset, message)
            else {
                continue;
            };
            let role = match message {
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::Tool { .. } => "tool",
                Message::System(_) => "system",
            };
            page.push(SessionHistoryMessage {
                index: start + offset,
                role,
                updates,
            });
        }
        entry.history_cursor.store(start, Ordering::Release);
        Ok(SessionHistoryPage {
            session_id: session_id.to_string(),
            total_messages: total,
            has_more: start > 0,
            messages: page,
        })
    }

    pub async fn resume_session_for_owner(
        &self,
        args: ResumeSessionRequest,
        owner_principal: &str,
    ) -> agent_client_protocol::Result<ResumeSessionResponse> {
        let canonical_cwd = canonicalize_existing_directory(&args.cwd)?;
        let session_id = args.session_id.clone();
        let key = OurSessionId::new(session_id.to_string());
        let entry = self
            .sessions
            .get(&key)
            .ok_or_else(|| agent_client_protocol::Error::new(-32002, "session not found"))?;
        if entry.owner_principal != owner_principal {
            return Err(agent_client_protocol::Error::new(
                -32000,
                "session not available for this principal",
            ));
        }
        if entry.working_directory.as_ref() != Some(&canonical_cwd) {
            return Err(agent_client_protocol::Error::invalid_params()
                .data("cwd does not match the session working directory"));
        }
        if !args.mcp_servers.is_empty() {
            self.sessions
                .update_mcp_servers(&key, crate::mcp_convert::acp_mcp_to_anureo(&args.mcp_servers));
        }
        self.sessions.reopen(&key);
        self.session_repository
            .set_lifecycle(key.as_str(), "idle")
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to persist session lifecycle: {error}"))
            })?;
        let mode = if entry.session_config.current_agent.is_empty() {
            self.agent_registry.default_mode_id().to_string()
        } else {
            entry.session_config.current_agent
        };
        Ok(ResumeSessionResponse::new().modes(self.agent_registry.to_session_mode_state(&mode)))
    }

    pub async fn close_session_for_owner(
        &self,
        args: CloseSessionRequest,
        owner_principal: &str,
    ) -> agent_client_protocol::Result<CloseSessionResponse> {
        let key = OurSessionId::new(args.session_id.to_string());
        let Some(entry) = self.sessions.get(&key) else {
            return Ok(CloseSessionResponse::new());
        };
        if entry.owner_principal != owner_principal {
            return Err(agent_client_protocol::Error::new(
                -32000,
                "session not available for this principal",
            ));
        }
        self.sessions.close(&key);
        self.session_repository
            .set_lifecycle(key.as_str(), "closed")
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to persist session lifecycle: {error}"))
            })?;
        Ok(CloseSessionResponse::new())
    }

    pub async fn delete_session_for_owner(
        &self,
        args: DeleteSessionRequest,
        owner_principal: &str,
    ) -> agent_client_protocol::Result<DeleteSessionResponse> {
        let key = OurSessionId::new(args.session_id.to_string());
        let Some(entry) = self.sessions.get(&key) else {
            let tombstone = self
                .session_repository
                .get_tombstone(key.as_str())
                .map_err(|error| {
                    agent_client_protocol::Error::internal_error()
                        .data(format!("failed to read session tombstone: {error}"))
                })?;
            let mut meta = serde_json::Map::new();
            if let Some(tombstone) = tombstone {
                meta.insert(
                    "anureo.dev".into(),
                    serde_json::json!({
                        "tombstone": {
                            "sessionId": tombstone.session_id,
                            "cwd": tombstone.cwd,
                            "parentSessionId": tombstone.parent_session_id,
                            "revision": tombstone.revision,
                            "indexVersion": tombstone.index_version,
                            "deletedAt": tombstone.deleted_at,
                            "deleted": true,
                        }
                    }),
                );
            }
            return Ok(DeleteSessionResponse::new().meta(meta));
        };
        if entry.owner_principal != owner_principal {
            return Err(agent_client_protocol::Error::new(
                -32000,
                "session not available for this principal",
            ));
        }
        let delete_result = self
            .session_repository
            .delete_all_indexed(key.as_str(), &entry.thread_id)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to delete session data: {error}"))
            })?
            .ok_or_else(|| {
                agent_client_protocol::Error::internal_error()
                    .data("session disappeared before delete transaction")
            })?;
        let tombstone = Some(delete_result.tombstone);
        self.sessions.delete(&key);
        let mut meta = serde_json::Map::new();
        if let Some(tombstone) = tombstone {
            let affected_sessions = delete_result
                .affected_ancestors
                .iter()
                .map(|record| {
                    serde_json::json!({
                        "sessionId": record.session_id,
                        "parentSessionId": record.parent_session_id,
                        "cwd": record.cwd,
                        "title": record.title,
                        "metadata": record.metadata,
                        "createdAt": record.created_at,
                        "activityAt": record.activity_at,
                        "treeActivityAt": record.tree_activity_at,
                        "stateChangedAt": record.state_changed_at,
                        "metadataUpdatedAt": record.metadata_updated_at,
                        "archivedAt": record.archived_at,
                        "closedAt": record.closed_at,
                        "lifecycle": record.lifecycle,
                        "revision": record.revision,
                        "indexVersion": record.index_version,
                    })
                })
                .collect::<Vec<_>>();
            meta.insert(
                "anureo.dev".into(),
                serde_json::json!({
                    "tombstone": {
                        "sessionId": tombstone.session_id,
                        "cwd": tombstone.cwd,
                        "parentSessionId": tombstone.parent_session_id,
                        "revision": tombstone.revision,
                        "indexVersion": tombstone.index_version,
                        "deletedAt": tombstone.deleted_at,
                        "deleted": true,
                    },
                    "affectedSessions": affected_sessions,
                    "indexVersion": tombstone.index_version,
                }),
            );
        }
        Ok(DeleteSessionResponse::new().meta(meta))
    }

    /// Seed a missing session title from the first user prompt so the
    /// sidebar never shows "untitled" when the fire-and-forget first-turn
    /// LLM title generation fails, is cancelled, or the process exits
    /// before the background task completes. The LLM-generated title
    /// overwrites the seed via `persist_session_title`.
    fn seed_session_title_from_prompt(
        &self,
        session_id: &agent_client_protocol::schema::v1::SessionId,
        content: &anureo_llm::message::UserContent,
    ) {
        let Some(title) = seed_title_text(content) else {
            return;
        };
        let existing = match self.session_repository.get(&session_id.0) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "failed to read session metadata for title seeding"
                );
                return;
            }
        };
        let has_title = existing
            .as_ref()
            .and_then(|metadata| metadata.title.as_deref())
            .map(|title| !title.trim().is_empty())
            .unwrap_or(false);
        if has_title {
            return;
        }
        if let Err(error) = self.session_repository.set_title(&session_id.0, &title) {
            tracing::warn!(
                session_id = %session_id,
                error = %error,
                "failed to seed session title"
            );
            return;
        }
        tracing::debug!(
            session_id = %session_id,
            title = %title,
            "seeded session title from first prompt"
        );
        if let Some(sender) = self.session_update_tx.clone() {
            SessionNotifier::new(sender, session_id.clone()).try_send_session_info_update(&title);
        }
    }

    /// List only durable sessions owned by the authenticated ACP principal.
    /// Metadata is the source of truth so a newly-created session is visible
    /// even before its first checkpoint is written.
    pub async fn list_sessions_for_owner(
        &self,
        args: ListSessionsRequest,
        owner_principal: &str,
    ) -> agent_client_protocol::Result<ListSessionsResponse> {
        let cwd_filter = match args.cwd {
            Some(cwd) => Some(canonicalize_existing_directory(&cwd)?),
            None => None,
        };
        let cwd_filter_string = cwd_filter
            .as_ref()
            .map(|cwd| cwd.to_string_lossy().to_string());
        let sessions = self
            .session_repository
            .list_index_for_owner(owner_principal, cwd_filter_string.as_deref(), "active")
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to list session index: {error}"))
            })?
            .into_iter()
            .map(|record| {
                let mut info = agent_client_protocol::schema::v1::SessionInfo::new(
                    record.session_id,
                    record.cwd,
                );
                info.title = record.title;
                info.updated_at = Some(record.activity_at);
                info
            })
            .collect();
        Ok(ListSessionsResponse::new(sessions))
    }

    /// Canonical SessionIndex projection shared by the private list extension
    /// and the standard ACP session/list compatibility projection.
    pub async fn list_index_records_for_owner(
        &self,
        owner_principal: &str,
        cwd: Option<&str>,
        archived: &str,
    ) -> agent_client_protocol::Result<(Vec<crate::session_repository::SessionIndexRecord>, i64)>
    {
        let records = self
            .session_repository
            .list_index_for_owner(owner_principal, cwd, archived)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to list session index: {error}"))
            })?;
        let version = self
            .session_repository
            .owner_index_version(owner_principal)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to read session index version: {error}"))
            })?;
        Ok((records, version))
    }

    /// Archive or restore a session owned by `owner_principal`. Returns the
    /// updated metadata, or `None` when the session is unknown or owned by
    /// another principal.
    pub async fn archive_session_for_owner(
        &self,
        owner_principal: &str,
        session_id: &str,
        archived: bool,
    ) -> agent_client_protocol::Result<Option<crate::session_repository::SessionMetadata>> {
        self.session_repository
            .set_archived(session_id, owner_principal, archived)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to update archive state: {error}"))
            })
    }

    /// Archive/restore and return every projection changed by that owner
    /// mutation. The repository assigns one index version to the target and
    /// any ancestor tree recalculations, so filtering by that version gives a
    /// stable response/event set after commit.
    pub async fn archive_session_index_for_owner(
        &self,
        owner_principal: &str,
        session_id: &str,
        archived: bool,
    ) -> agent_client_protocol::Result<Option<Vec<crate::session_repository::SessionIndexRecord>>>
    {
        self.session_repository
            .set_archived_index_records(session_id, owner_principal, archived)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to update archive index: {error}"))
            })
    }

    /// Replace anureo Desk-owned metadata for a session after checking the ACP
    /// principal. This is the ACP equivalent of the old session metadata
    /// PATCH endpoint used by goals, reviews, and client-side annotations.
    pub async fn update_session_metadata_for_owner(
        &self,
        owner_principal: &str,
        session_id: &str,
        metadata: serde_json::Value,
    ) -> agent_client_protocol::Result<Option<serde_json::Value>> {
        let metadata_json = serde_json::to_string(&metadata).map_err(|error| {
            agent_client_protocol::Error::invalid_params()
                .data(format!("invalid session metadata: {error}"))
        })?;
        let updated = self
            .session_repository
            .set_metadata_json(session_id, owner_principal, &metadata_json)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to update session metadata: {error}"))
            })?;
        if !updated {
            return Ok(None);
        }
        Ok(Some(metadata))
    }

    /// Atomically update title and Desk metadata and return the canonical
    /// index projection produced by the same repository transaction.
    pub async fn update_session_index_fields_for_owner(
        &self,
        owner_principal: &str,
        session_id: &str,
        title: Option<&str>,
        metadata: Option<serde_json::Value>,
    ) -> agent_client_protocol::Result<Option<crate::session_repository::SessionIndexRecord>> {
        let metadata_json = metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| {
                agent_client_protocol::Error::invalid_params()
                    .data(format!("invalid session metadata: {error}"))
            })?;
        self.session_repository
            .update_index_fields(session_id, owner_principal, title, metadata_json.as_deref())
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to update session index fields: {error}"))
            })
    }

    /// Read anureo Desk-owned metadata for a session after checking ownership.
    pub async fn session_metadata_for_owner(
        &self,
        owner_principal: &str,
        session_id: &str,
    ) -> agent_client_protocol::Result<Option<serde_json::Value>> {
        let owned = self
            .session_repository
            .get(session_id)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to read session metadata: {error}"))
            })?
            .is_some_and(|metadata| metadata.owner_principal == owner_principal);
        if !owned {
            return Ok(None);
        }
        let raw = self
            .session_repository
            .get_metadata_json(session_id)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to read session metadata payload: {error}"))
            })?;
        raw.map(|value| {
            serde_json::from_str(&value).map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("stored session metadata is invalid: {error}"))
            })
        })
        .transpose()
    }

    /// Return one durable session after checking ownership.
    pub async fn session_for_owner(
        &self,
        owner_principal: &str,
        session_id: &str,
    ) -> agent_client_protocol::Result<Option<crate::session_repository::SessionMetadata>> {
        self.session_repository
            .get(session_id)
            .map(|value| value.filter(|metadata| metadata.owner_principal == owner_principal))
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to read session: {error}"))
            })
    }

    pub async fn session_index_record_for_owner(
        &self,
        owner_principal: &str,
        session_id: &str,
    ) -> agent_client_protocol::Result<Option<crate::session_repository::SessionIndexRecord>> {
        self.session_repository
            .get_index_record(owner_principal, session_id)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to read session index record: {error}"))
            })
    }

    pub async fn session_tombstone_for_owner(
        &self,
        owner_principal: &str,
        session_id: &str,
    ) -> agent_client_protocol::Result<Option<crate::session_repository::SessionTombstone>> {
        let tombstone = self
            .session_repository
            .get_tombstone(session_id)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to read session tombstone: {error}"))
            })?;
        Ok(tombstone.filter(|item| item.owner_principal == owner_principal))
    }

    /// Set a user-visible session title after checking ownership.
    pub async fn update_session_title_for_owner(
        &self,
        owner_principal: &str,
        session_id: &str,
        title: &str,
    ) -> agent_client_protocol::Result<bool> {
        let Some(metadata) = self.session_for_owner(owner_principal, session_id).await? else {
            return Ok(false);
        };
        self.session_repository
            .set_title(&metadata.session_id, title)
            .map_err(|error| {
                agent_client_protocol::Error::internal_error()
                    .data(format!("failed to update session title: {error}"))
            })?;
        Ok(true)
    }
}

fn map_run_error(e: RunError) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error().data(e.to_string())
}

#[derive(Default, Clone, Debug)]
pub(crate) struct TurnUsage {
    pub(crate) input_tokens: u64,
    pub(crate) output_tokens: u64,
    pub(crate) total_tokens: u64,
    pub(crate) cached_tokens: u64,
}

/// Derive a fallback title from the first user prompt: skips slash
/// commands, normalizes whitespace (including newlines) to a single line,
/// and clamps to the same 50-char budget as LLM-generated titles.
fn seed_title_text(content: &anureo_llm::message::UserContent) -> Option<String> {
    let joined = match content {
        anureo_llm::message::UserContent::Text(text) => text.clone(),
        anureo_llm::message::UserContent::Multimodal(parts) => parts
            .iter()
            .filter_map(|part| match part {
                anureo_llm::message::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
    };
    if agent::commands::parse(&joined).is_some() {
        return None;
    }
    let normalized = joined.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.is_empty() {
        return None;
    }
    Some(agent::agent::react::title_generator::clamp_summary_chars(
        &normalized,
    ))
}

/// Persist the fire-and-forget first-turn title event
/// (`Updates { node_id: "title" }`) into the session repository so
/// `session/list` can serve the title across restarts. The push to the
/// client happens separately via `SessionNotifier` (`session_info_update`).
async fn persist_session_title(
    repository: &SessionRepository,
    session_id: &str,
    ev: &TypedAnyStreamEvent,
) {
    if let TypedAnyStreamEvent::React(stream_event::StreamEvent::Updates {
        node_id, state, ..
    }) = ev
    {
        if node_id == "title" {
            if let Some(title) = state.summary.as_ref().filter(|t| !t.trim().is_empty()) {
                // Add retry mechanism for title persistence
                let mut retries = 0;
                let max_retries = 3;
                let mut last_error = None;

                while retries <= max_retries {
                    match repository.set_title(session_id, title) {
                        Ok(_) => {
                            if retries > 0 {
                                tracing::info!(
                                    session_id,
                                    retries,
                                    "successfully persisted session title after retry"
                                );
                            }
                            return;
                        }
                        Err(error) => {
                            last_error = Some(error);
                            retries += 1;
                            if retries <= max_retries {
                                tracing::warn!(
                                    session_id,
                                    retries,
                                    max_retries,
                                    error = ?last_error,
                                    "failed to persist session title, retrying..."
                                );
                                tokio::time::sleep(tokio::time::Duration::from_millis(
                                    100 * retries as u64,
                                ))
                                .await;
                            }
                        }
                    }
                }

                if let Some(error) = last_error {
                    tracing::error!(
                        session_id,
                        max_retries,
                        error = %error,
                        "failed to persist session title after {} retries",
                        max_retries
                    );
                }
            }
        }
    }
}

fn extract_llm_usage<S>(ev: &stream_event::StreamEvent<S>) -> Option<(u32, u32, u32, Option<u32>)>
where
    S: Clone + Send + Sync + std::fmt::Debug + 'static,
{
    match ev {
        stream_event::StreamEvent::TurnFinish { usage, .. } => Some((
            usage.input,
            usage.output,
            usage.input + usage.output,
            usage.cache_read,
        )),
        _ => None,
    }
}

fn extract_llm_usage_from_any(ev: &TypedAnyStreamEvent) -> Option<(u32, u32, u32, Option<u32>)> {
    match ev {
        TypedAnyStreamEvent::React(e) => extract_llm_usage(e),
        TypedAnyStreamEvent::Dup(e) => extract_llm_usage(e),
        TypedAnyStreamEvent::Tot(e) => extract_llm_usage(e),
        TypedAnyStreamEvent::Got(e) => extract_llm_usage(e),
    }
}

pub(crate) fn capture_turn_usage(ev: &TypedAnyStreamEvent, acc: &Mutex<TurnUsage>) {
    if let Some((prompt, completion, total, cached)) = extract_llm_usage_from_any(ev) {
        let mut a = acc.lock().unwrap();
        a.input_tokens += prompt as u64;
        a.output_tokens += completion as u64;
        a.total_tokens += total as u64;
        if let Some(c) = cached {
            a.cached_tokens += c as u64;
        }
    }
}

fn build_acp_usage(acc: &Mutex<TurnUsage>) -> Option<Usage> {
    let a = acc.lock().unwrap();
    if a.total_tokens == 0 {
        return None;
    }
    let mut usage = Usage::new(a.total_tokens, a.input_tokens, a.output_tokens);
    if a.cached_tokens > 0 {
        usage = usage.cached_read_tokens(a.cached_tokens);
    }
    Some(usage)
}

/// Model option for ACP config dropdown.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelOption {
    pub id: String,
    pub name: String,
    pub provider: String,
}

/// If `current_model` is a bare model id (e.g. from `MODEL=`) but options use `provider/model`,
/// rewrite to the single matching option when unambiguous.
fn normalize_current_model_for_acp(current_model: &str, options: &[ModelOption]) -> String {
    if current_model.is_empty() || current_model == "default" {
        return "default".to_string();
    }
    if options.iter().any(|m| m.id == current_model) {
        return current_model.to_string();
    }
    let suffix = format!("/{}", current_model);
    let matches: Vec<_> = options.iter().filter(|m| m.id.ends_with(&suffix)).collect();
    if matches.len() == 1 {
        return matches[0].id.clone();
    }
    if options.iter().any(|m| m.id == "default") {
        return "default".to_string();
    }
    options
        .first()
        .map(|option| option.id.clone())
        .unwrap_or_else(|| "default".to_string())
}

/// Build config_options array with "mode" and "model" options (protocol types are non_exhaustive, so we construct via serde).
/// SessionConfigOption has kind flattened; SessionConfigKind uses tag "type" ? "type": "select" and SessionConfigSelect fields at top level (camelCase).
fn build_session_config_options(
    current_mode: &str,
    current_model: &str,
    current_effort: &str,
    modes: &[agent_client_protocol::schema::v1::SessionMode],
    model_options: &[ModelOption],
    model_reasoning_efforts: Option<&[String]>,
) -> Result<Vec<agent_client_protocol::schema::v1::SessionConfigOption>, serde_json::Error> {
    let current_model = normalize_current_model_for_acp(current_model, model_options);
    let mode_options: Vec<_> = modes
        .iter()
        .map(|m| {
            serde_json::json!({
                "value": m.id.to_string(),
                "name": m.name.to_string()
            })
        })
        .collect();
    let model_options: Vec<_> = model_options
        .iter()
        .map(|m| serde_json::json!({ "value": &m.id, "name": &m.name }))
        .collect();

    let all_efforts = [
        ("auto", "Auto", "Use model default"),
        ("none", "None", "No reasoning"),
        ("minimal", "Minimal", "Minimal reasoning"),
        ("low", "Low", "Low effort"),
        ("medium", "Medium", "Balanced"),
        ("high", "High", "Deep reasoning"),
        ("xhigh", "Xhigh", "Deepest reasoning"),
    ];
    let effort_options: Vec<_> = all_efforts
        .iter()
        .filter(|(v, _, _)| {
            *v == "auto" || model_reasoning_efforts.is_none_or(|r| r.iter().any(|e| e == v))
        })
        .map(|(v, n, d)| serde_json::json!({"value": v, "name": n, "description": d}))
        .collect();

    let json = serde_json::json!([
        {
            "id": "mode",
            "name": "Mode",
            "description": "Session behavior mode.",
            "category": "mode",
            "type": "select",
            "currentValue": current_mode,
            "options": mode_options
        },
        {
            "id": "model",
            "name": "Model",
            "description": "LLM model for this session.",
            "category": "model",
            "type": "select",
            "currentValue": current_model,
            "options": model_options
        },
        {
            "id": "effort",
            "name": "Reasoning Effort",
            "description": "Controls reasoning depth for thinking models.",
            "category": "thought_level",
            "type": "select",
            "currentValue": current_effort,
            "options": effort_options
        }
    ]);
    serde_json::from_value(json)
}

/// Build SetSessionConfigOptionResponse with a single "model" option (protocol types are non_exhaustive, so we construct via serde).
fn build_set_session_config_option_response(
    current_mode: &str,
    current_model: &str,
    current_effort: &str,
    modes: &[agent_client_protocol::schema::v1::SessionMode],
    model_options: &[ModelOption],
    model_reasoning_efforts: Option<&[String]>,
) -> Result<SetSessionConfigOptionResponse, serde_json::Error> {
    let config_options = build_session_config_options(
        current_mode,
        current_model,
        current_effort,
        modes,
        model_options,
        model_reasoning_efforts,
    )?;
    let json = serde_json::json!({
        "configOptions": config_options,
        "meta": None::<()>
    });
    serde_json::from_value(json)
}

fn session_config_value_as_id(value: &SessionConfigOptionValue) -> Option<String> {
    match value {
        SessionConfigOptionValue::ValueId { value } => Some(value.to_string()),
        SessionConfigOptionValue::Boolean { .. } => None,
        _ => None,
    }
}

/// Resolve model context window from model spec, falling back to 128k default.
async fn resolve_context_window_size(model: Option<&str>) -> u64 {
    use model_spec_core::resolver::{ConfigModelEntry, ConfigProviderEntry};

    let Some(model) = model else {
        return agent::compress::CompactionConfig::default().max_context_tokens as u64;
    };

    let providers: Vec<ConfigProviderEntry> = config::load_full_config("anureo")
        .ok()
        .map(|f| {
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
        .unwrap_or_default();

    match model_spec_core::resolver::resolve_model_context_limit(model, providers).await {
        Some(context_limit) => {
            tracing::info!(
                model = %model,
                context_limit,
                "resolved context window size from model spec"
            );
            context_limit as u64
        }
        None => {
            tracing::warn!(
                model = %model,
                "model spec not found, using default 128k context window"
            );
            agent::compress::CompactionConfig::default().max_context_tokens as u64
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_title_text_normalizes_and_skips_commands() {
        use anureo_llm::message::UserContent;

        let multiline = UserContent::Text("  fix the\nlogin   bug please ".to_string());
        assert_eq!(
            seed_title_text(&multiline).as_deref(),
            Some("fix the login bug please")
        );

        assert_eq!(
            seed_title_text(&UserContent::Text("/reset".to_string())),
            None
        );
        assert_eq!(seed_title_text(&UserContent::Text("   ".to_string())), None);

        let long = UserContent::Text("x".repeat(80));
        assert_eq!(
            seed_title_text(&long).map(|title| title.chars().count()),
            Some(50)
        );
        assert!(seed_title_text(&long).unwrap().ends_with("..."));
    }

    #[test]
    fn seed_title_text_joins_multimodal_text_parts() {
        use anureo_llm::message::{ContentPart, UserContent};

        let content = UserContent::Multimodal(vec![
            ContentPart::Text {
                text: "look at this".to_string(),
            },
            ContentPart::ImageUrl {
                url: "https://example.com/a.png".to_string(),
                detail: None,
            },
            ContentPart::Text {
                text: "screenshot".to_string(),
            },
        ]);
        assert_eq!(
            seed_title_text(&content).as_deref(),
            Some("look at this screenshot")
        );

        let image_only = UserContent::Multimodal(vec![ContentPart::ImageUrl {
            url: "https://example.com/a.png".to_string(),
            detail: None,
        }]);
        assert_eq!(seed_title_text(&image_only), None);
    }

    #[test]
    fn test_session_config_select_option_structure() {
        use agent_client_protocol::schema::v1::{SessionConfigSelectOption, SessionConfigValueId};

        let option_id = SessionConfigValueId::new("gpt-4o".to_string());
        let select_option = SessionConfigSelectOption::new(option_id, "GPT-4o".to_string());

        let json = serde_json::to_value(&select_option).unwrap();
        assert_eq!(json["value"], "gpt-4o");
        assert_eq!(json["name"], "GPT-4o");
    }

    #[test]
    fn test_build_session_config_options_populates_options() {
        let modes = vec![
            agent_client_protocol::schema::v1::SessionMode::new(
                agent_client_protocol::schema::v1::SessionModeId::new("ask"),
                "Ask",
            ),
            agent_client_protocol::schema::v1::SessionMode::new(
                agent_client_protocol::schema::v1::SessionModeId::new("default"),
                "Default",
            ),
        ];
        let model_options = vec![
            ModelOption {
                id: "openai/gpt-4o".to_string(),
                name: "openai/gpt-4o".to_string(),
                provider: "openai".to_string(),
            },
            ModelOption {
                id: "openai/gpt-4o-mini".to_string(),
                name: "openai/gpt-4o-mini".to_string(),
                provider: "openai".to_string(),
            },
        ];

        // Bare MODEL= id normalizes to the unique provider/model match
        let result =
            build_session_config_options("ask", "gpt-4o", "auto", &modes, &model_options, None);
        assert!(result.is_ok(), "Expected Ok, got Err: {:?}", result.err());

        let config_options = result.unwrap();
        assert_eq!(config_options.len(), 3);

        let json = serde_json::to_value(&config_options).unwrap();
        assert_eq!(json[0]["id"], "mode");
        assert_eq!(json[0]["category"], "mode");
        assert_eq!(json[0]["currentValue"], "ask");
        assert_eq!(json[1]["id"], "model");
        let model_config = &json[1];
        assert_eq!(model_config["id"], "model");
        assert_eq!(model_config["currentValue"], "openai/gpt-4o");

        let options = model_config["options"]
            .as_array()
            .expect("options should be an array");
        assert_eq!(options.len(), 2);
        assert_eq!(options[0]["value"], "openai/gpt-4o");
        assert_eq!(options[0]["name"], "openai/gpt-4o");
    }

    #[test]
    fn test_normalize_current_model_for_acp_ambiguous_bare_id() {
        let model_options = vec![
            ModelOption {
                id: "openai/gpt-4o".to_string(),
                name: "openai/gpt-4o".to_string(),
                provider: "openai".to_string(),
            },
            ModelOption {
                id: "azure/gpt-4o".to_string(),
                name: "azure/gpt-4o".to_string(),
                provider: "azure".to_string(),
            },
        ];
        assert_eq!(
            normalize_current_model_for_acp("gpt-4o", &model_options),
            "openai/gpt-4o"
        );
        assert_eq!(
            normalize_current_model_for_acp("openai/gpt-4o", &model_options),
            "openai/gpt-4o"
        );
    }

    #[test]
    fn test_load_session_response_has_config_options() {
        // Check if LoadSessionResponse supports config_options field
        let response = LoadSessionResponse::default();
        let json = serde_json::to_value(&response).unwrap();
        println!(
            "LoadSessionResponse default JSON: {}",
            serde_json::to_string_pretty(&json).unwrap()
        );

        // Check if configOptions field exists (it should be optional)
        let has_config_options = json.get("configOptions").is_some();
        println!("Has configOptions field: {}", has_config_options);
    }

    #[test]
    fn test_build_session_config_options_handles_empty_model_list() {
        let modes = vec![agent_client_protocol::schema::v1::SessionMode::new(
            agent_client_protocol::schema::v1::SessionModeId::new("ask"),
            "Ask",
        )];
        let result = build_session_config_options("ask", "", "auto", &modes, &[], None);
        assert!(result.is_ok(), "Expected Ok, got Err: {:?}", result.err());

        let config_options = result.unwrap();
        let json = serde_json::to_value(&config_options).unwrap();
        let options = json[1]["options"].as_array().unwrap();
        assert!(options.is_empty());
    }

    #[test]
    fn test_model_option_serialization() {
        let option = ModelOption {
            id: "anthropic/claude-3-opus".to_string(),
            name: "anthropic/claude-3-opus".to_string(),
            provider: "anthropic".to_string(),
        };

        let json = serde_json::to_value(&option).unwrap();
        assert_eq!(json["id"], "anthropic/claude-3-opus");
        assert_eq!(json["name"], "anthropic/claude-3-opus");
    }

    #[test]
    fn test_build_set_session_config_option_response() {
        let modes = vec![agent_client_protocol::schema::v1::SessionMode::new(
            agent_client_protocol::schema::v1::SessionModeId::new("ask"),
            "Ask",
        )];
        let model_options = vec![ModelOption {
            id: "openai/gpt-4o".to_string(),
            name: "openai/gpt-4o".to_string(),
            provider: "openai".to_string(),
        }];

        let result = build_set_session_config_option_response(
            "ask",
            "gpt-4o",
            "auto",
            &modes,
            &model_options,
            None,
        );
        assert!(result.is_ok());

        let response = result.unwrap();
        let json = serde_json::to_value(&response).unwrap();
        assert!(json["configOptions"].is_array());
    }

    #[test]
    fn test_session_config_value_as_id_accepts_value_id_only() {
        let value_id = SessionConfigOptionValue::value_id("ask");
        assert_eq!(
            session_config_value_as_id(&value_id).as_deref(),
            Some("ask")
        );

        let boolean = SessionConfigOptionValue::boolean(true);
        assert!(session_config_value_as_id(&boolean).is_none());
    }

    #[test]
    fn test_normalize_current_model_for_acp_default() {
        let options = vec![ModelOption {
            id: "default".to_string(),
            name: "(default)".to_string(),
            provider: String::new(),
        }];
        assert_eq!(
            normalize_current_model_for_acp("default", &options),
            "default"
        );
        assert_eq!(normalize_current_model_for_acp("", &options), "default");
    }

    #[test]
    fn test_normalize_current_model_for_acp_specific_model() {
        let options = vec![
            ModelOption {
                id: "default".to_string(),
                name: "(default)".to_string(),
                provider: String::new(),
            },
            ModelOption {
                id: "openai/gpt-4o".to_string(),
                name: "openai/gpt-4o".to_string(),
                provider: "openai".to_string(),
            },
        ];
        assert_eq!(
            normalize_current_model_for_acp("openai/gpt-4o", &options),
            "openai/gpt-4o"
        );
    }

    #[test]
    fn test_normalize_current_model_for_acp_invalid_value_falls_back_to_default() {
        let options = vec![
            ModelOption {
                id: "default".to_string(),
                name: "(default)".to_string(),
                provider: String::new(),
            },
            ModelOption {
                id: "openai/gpt-4o".to_string(),
                name: "openai/gpt-4o".to_string(),
                provider: "openai".to_string(),
            },
        ];
        assert_eq!(
            normalize_current_model_for_acp("stale-provider/stale-model", &options),
            "default"
        );
    }

    #[test]
    fn test_build_session_config_options_includes_default() {
        let modes = vec![agent_client_protocol::schema::v1::SessionMode::new(
            agent_client_protocol::schema::v1::SessionModeId::new("ask"),
            "Ask",
        )];
        let model_options = vec![
            ModelOption {
                id: "default".to_string(),
                name: "(default)".to_string(),
                provider: String::new(),
            },
            ModelOption {
                id: "openai/gpt-4o".to_string(),
                name: "openai/gpt-4o".to_string(),
                provider: "openai".to_string(),
            },
        ];

        let result =
            build_session_config_options("ask", "default", "auto", &modes, &model_options, None);
        assert!(result.is_ok());

        let config_options = result.unwrap();
        let json = serde_json::to_value(&config_options).unwrap();
        let model_config = json
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c.get("id").and_then(|v| v.as_str()) == Some("model"))
            .unwrap();
        let options = model_config.get("options").unwrap().as_array().unwrap();
        assert_eq!(options[0].get("value").unwrap().as_str(), Some("default"));
        assert_eq!(
            model_config.get("currentValue").unwrap().as_str(),
            Some("default")
        );
    }

    #[test]
    fn test_build_acp_usage_empty() {
        let acc = Mutex::new(TurnUsage::default());
        assert!(build_acp_usage(&acc).is_none());
    }

    #[test]
    fn test_build_acp_usage_populated() {
        let acc = Mutex::new(TurnUsage {
            input_tokens: 1000,
            output_tokens: 500,
            total_tokens: 1500,
            cached_tokens: 200,
        });
        let usage = build_acp_usage(&acc).expect("usage should be Some");
        assert_eq!(usage.input_tokens, 1000);
        assert_eq!(usage.output_tokens, 500);
        assert_eq!(usage.total_tokens, 1500);
        assert_eq!(usage.cached_read_tokens, Some(200));
        assert_eq!(usage.thought_tokens, None);
    }

    #[test]
    fn test_capture_turn_usage_accumulates() {
        use stream_event::StreamEvent;
        use ReActState;

        let acc = Arc::new(Mutex::new(TurnUsage::default()));

        let ev1 = TypedAnyStreamEvent::React(StreamEvent::<ReActState>::TurnFinish {
            reason: "stop".to_string(),
            usage: stream_event::Usage {
                input: 500,
                output: 100,
                reasoning: None,
                cache_read: Some(50),
                cache_write: None,
            },
        });
        let ev2 = TypedAnyStreamEvent::React(StreamEvent::<ReActState>::TurnFinish {
            reason: "stop".to_string(),
            usage: stream_event::Usage {
                input: 800,
                output: 200,
                reasoning: None,
                cache_read: None,
                cache_write: None,
            },
        });

        capture_turn_usage(&ev1, &acc);
        capture_turn_usage(&ev2, &acc);

        let usage = build_acp_usage(&acc).expect("usage should be Some");
        assert_eq!(usage.input_tokens, 1300);
        assert_eq!(usage.output_tokens, 300);
        assert_eq!(usage.total_tokens, 1600);
        assert_eq!(usage.cached_read_tokens, Some(50));
    }

    #[test]
    fn test_capture_turn_usage_ignores_non_usage() {
        use stream_event::StreamEvent;
        use ReActState;

        let acc = Arc::new(Mutex::new(TurnUsage::default()));

        let ev = TypedAnyStreamEvent::React(StreamEvent::<ReActState>::TaskStart {
            node_id: "test".to_string(),
            namespace: None,
        });
        capture_turn_usage(&ev, &acc);

        assert!(build_acp_usage(&acc).is_none());
    }

    // ── initialize capability unit tests ──────────────────────────────
    //
    // These tests exercise the `initialize` handler directly (no process
    // spawn) and assert the returned `agentCapabilities` structure.
    // The corresponding e2e smoke test in `tests/e2e_mega.rs` only verifies
    // that the binary spawns and returns a valid initialize response.

    fn assert_mcp_caps(resp: &InitializeResponse) {
        let json = serde_json::to_value(&resp.agent_capabilities).expect("serialize");
        let mcp = json
            .get("mcpCapabilities")
            .expect("agentCapabilities.mcpCapabilities must be present");
        assert_eq!(
            mcp.get("http").and_then(serde_json::Value::as_bool),
            Some(true),
            "mcpCapabilities.http must be true"
        );
        assert_eq!(
            mcp.get("sse").and_then(serde_json::Value::as_bool),
            Some(false),
            "mcpCapabilities.sse must be false"
        );
    }

    fn assert_prompt_caps(resp: &InitializeResponse) {
        let json = serde_json::to_value(&resp.agent_capabilities).expect("serialize");
        let prompts = json
            .get("promptCapabilities")
            .expect("agentCapabilities.promptCapabilities must be present");
        assert_eq!(
            prompts.get("image").and_then(serde_json::Value::as_bool),
            Some(true),
            "promptCapabilities.image must be true"
        );
        assert_eq!(
            prompts.get("audio").and_then(serde_json::Value::as_bool),
            Some(true),
            "promptCapabilities.audio must be true"
        );
        assert_eq!(
            prompts
                .get("embeddedContext")
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "promptCapabilities.embeddedContext must be true"
        );
    }

    #[tokio::test]
    async fn test_initialize_returns_mcp_capabilities() {
        let agent = AnureoAcpAgent::new().expect("agent");
        let req = InitializeRequest::new(1.into());
        let resp = agent.initialize(req).await.expect("initialize");
        assert!(resp.protocol_version >= 1.into());
        assert_mcp_caps(&resp);
    }

    #[tokio::test]
    async fn test_initialize_returns_prompt_capabilities() {
        let agent = AnureoAcpAgent::new().expect("agent");
        let req = InitializeRequest::new(1.into());
        let resp = agent.initialize(req).await.expect("initialize");
        assert_prompt_caps(&resp);
    }

    #[tokio::test]
    async fn test_initialize_accepts_client_mcp_capabilities() {
        let agent = AnureoAcpAgent::new().expect("agent");
        let req = InitializeRequest::new(1.into()).client_info(
            agent_client_protocol::schema::v1::Implementation::new(
                "test-client".to_string(),
                "0.1.0".to_string(),
            ),
        );
        let resp = agent.initialize(req).await.expect("initialize");
        assert_mcp_caps(&resp);
    }

    #[tokio::test]
    async fn test_initialize_accepts_client_prompt_capabilities() {
        let agent = AnureoAcpAgent::new().expect("agent");
        let req = InitializeRequest::new(1.into()).client_info(
            agent_client_protocol::schema::v1::Implementation::new(
                "test-client".to_string(),
                "0.1.0".to_string(),
            ),
        );
        let resp = agent.initialize(req).await.expect("initialize");
        assert_prompt_caps(&resp);
    }

    #[tokio::test]
    async fn initialize_advertises_registered_session_lifecycle_methods() {
        let agent = AnureoAcpAgent::new().expect("agent");
        let resp = agent
            .initialize(InitializeRequest::new(1.into()))
            .await
            .expect("initialize");
        let session = resp.agent_capabilities.session_capabilities;
        assert!(session.list.is_some());
        assert!(session.resume.is_some());
        assert!(session.close.is_some());
        assert!(session.delete.is_some());
    }

    fn tool_msg(id: &str) -> Message {
        Message::Tool {
            tool_call_id: id.to_string(),
            content: tool_core::ToolCallContent::Text("ok".to_string()),
        }
    }

    #[test]
    fn history_tail_start_replays_everything_when_short() {
        let messages = vec![Message::user("hi"), Message::assistant("hello")];
        assert_eq!(history_tail_start(&messages, 50), 0);
        assert_eq!(history_tail_start(&messages, 0), 0);
    }

    #[test]
    fn history_tail_start_truncates_on_plain_boundary() {
        let mut messages = Vec::new();
        for i in 0..10 {
            messages.push(Message::user(format!("q{i}")));
            messages.push(Message::assistant(format!("a{i}")));
        }
        // tail=4 lands on an assistant message; no backward extension needed.
        assert_eq!(history_tail_start(&messages, 4), 16);
    }

    #[test]
    fn history_tail_start_extends_back_past_leading_tool_messages() {
        // [u, a(tool_calls), tool, u, a, u, a, ...] — a tail that would
        // start at a Tool message must include the owning Assistant instead.
        let messages = vec![
            Message::user("q0"),
            Message::assistant_with_tool_calls(
                String::new(),
                vec![anureo_llm::message::AssistantToolCall {
                    id: "tc-1".to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                }],
            ),
            tool_msg("tc-1"),
            Message::user("q1"),
            Message::assistant("a1"),
            Message::user("q2"),
            Message::assistant("a2"),
        ];
        // tail=5 → naive start is index 2 (Tool) → past the Tool (1) to the
        // Assistant, then to the turn's User at 0.
        assert_eq!(history_tail_start(&messages, 5), 0);
        // tail=6 lands on the Assistant with tool_calls at index 1 → extends
        // back to the User at 0 so the replay starts on a turn boundary.
        assert_eq!(history_tail_start(&messages, 6), 0);
        // tail=4 lands on the User at index 3 — already a turn boundary.
        assert_eq!(history_tail_start(&messages, 4), 3);
    }

    #[test]
    fn history_tail_start_extends_mid_turn_tail_to_user_anchor() {
        // Long tool-heavy turn: the naive tail cut lands between the user
        // message and its many assistant/tool replies. Clients drop
        // anchor-less assistant messages, so the tail MUST reach back to
        // the owning User even though it exceeds the configured floor.
        let mut messages = vec![Message::user("q0"), Message::assistant("a0")];
        for i in 1..=6 {
            messages.push(Message::user(format!("q{i}")));
            messages.push(Message::assistant(format!("a{i}-part1")));
            messages.push(tool_msg(&format!("tc-{i}")));
            messages.push(Message::assistant(format!("a{i}-part2")));
        }
        // 2 + 6*4 = 26 messages; tail=8 → naive start=18 lands mid-turn on an
        // Assistant of q5's turn → extends back to the User of q5 (index 18
        // is a-part2 of q4? indices: q5 starts at 18) — verify exact boundary.
        let q5_index = 2 + 4 * 4; // q5 = messages[18]
        assert_eq!(history_tail_start(&messages, 8), q5_index);
        // tail smaller than one turn still replays the entire owning turn.
        assert_eq!(history_tail_start(&messages, 3), 22);
    }
}

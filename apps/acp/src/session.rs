//! Session state: session_id mapping, cancel flag, working directory
//!
//! Each ACP `session/new` corresponds to one [`SessionEntry`], stored by [`SessionStore`] keyed by session_id.
//! Protocol details are in [`crate::protocol`].
//!
//! ## NewSessionRequest -> SessionStore
//!
//! - **session_id**: Generated uniquely by Agent in new_session as a UUID; all later prompt/cancel/load use this ID.
//! - **thread_id**: Same as anureo's `RunOptions::thread_id`, usually the string form of session_id for checkpointer and multi-turn consistency.
//! - **working_directory**: From `NewSessionRequest::working_directory` (protocol requires **absolute path**), maps to `RunOptions::working_folder`; if absent the caller may use process cwd or a temp dir.
//! - **mcp_servers**: Stored in the session and connected lazily on the first
//!   prompt; idle connections are evicted and recreated on demand.
//!
//! ## Cancel semantics (session/cancel)
//!
//! - **cancelled**: Whether the session has been cancelled by the Client via `session/cancel`. On cancel call [`SessionStore::set_cancelled`]; the prompt path should **periodically** check [`SessionStore::is_cancelled`] and exit with StopReason::Cancelled when true. Any pending request_permission will get Cancelled from the Client.
//!
//! When integrated with ACP, session_id can use `agent_client_protocol::SessionId`; this module's [`SessionId`] is a placeholder type for unit tests without the ACP dependency.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tool_basic::McpToolSource;
use tool_core::active_operation::RunCancellation;
use uuid::Uuid;

fn recover_read<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| {
        tracing::warn!("RwLock read poisoned, recovering");
        e.into_inner()
    })
}

fn recover_write<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| {
        tracing::warn!("RwLock write poisoned, recovering");
        e.into_inner()
    })
}

/// Unique session identifier.
///
/// Without ACP this type (inner `String`) is used; at the boundary it can be converted to/from
/// `agent_client_protocol::SessionId`, or the protocol type can be used as the key directly.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SessionId(pub String);

impl SessionId {
    /// Create a SessionId from a string.
    #[inline]
    pub fn new(s: impl Into<String>) -> Self {
        SessionId(s.into())
    }

    /// Return the underlying string.
    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Per-session configuration (e.g. model, max_tokens). Set via `session/set_config_option`.
#[derive(Clone, Debug, Default)]
pub struct SessionConfig {
    /// LLM model id for this session (e.g. "gpt-4o", "gpt-4o-mini"). When set, overrides env at prompt time.
    pub model: Option<String>,
    /// Current agent/mode id for this session (e.g. "ask", "default", "dev"). Maps to ACP session mode.
    pub current_agent: String,
    /// Reasoning effort: "auto"|"none"|"minimal"|"low"|"medium"|"high"|"xhigh"|None
    pub effort: Option<String>,
}

/// Metadata and cancel flag for a single session.
///
/// Written by [`SessionStore::create`], read by [`SessionStore::get`].
/// Prompt handling uses `thread_id` and `working_directory` to build [`anureo::RunOptions`]
/// and [`SessionStore::is_cancelled`] to decide whether to abort with Cancelled.
#[derive(Debug)]
pub struct SessionEntry {
    /// Thread/session id used by anureo; 1:1 with ACP session_id.
    pub thread_id: String,
    /// Authenticated principal that owns this session.
    pub owner_principal: String,
    /// Working directory for this session (from NewSessionRequest); None lets the caller choose a default.
    pub working_directory: Option<PathBuf>,
    /// Whether this session has been cancelled via session/cancel; should be checked periodically in the prompt path.
    pub cancelled: Arc<AtomicBool>,
    /// Session-level config (model, etc.); updated by set_session_config_option.
    pub session_config: SessionConfig,
    /// Shared cancellation state for the current turn.
    pub cancellation: Arc<SessionCancellationState>,
    /// Serializes short prompt/lifecycle/binding transitions.
    pub control_lock: Arc<std::sync::Mutex<()>>,
    pub lifecycle: Arc<std::sync::RwLock<SessionLifecycle>>,
    /// Raw-message index boundary of history already delivered to clients.
    /// `session/load` stores its tail-truncated replay start index here;
    /// the `_anureo.dev/session-history/page` extension pages backward
    /// from it. `usize::MAX` means no truncated replay happened yet (fresh
    /// or live session — nothing earlier to fetch). Shared via Arc so entry
    /// clones observe the same cursor.
    pub history_cursor: Arc<std::sync::atomic::AtomicUsize>,
    /// MCP servers from ACP session/new or session/load, pre-converted to anureo's [`config::McpServerDef`].
    pub mcp_servers: Vec<config::McpServerDef>,
    pub mcp_runtime: Arc<SessionMcpRuntime>,
}

const DEFAULT_MCP_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

struct SessionMcpState {
    sources: HashMap<String, Arc<McpToolSource>>,
    definitions: HashMap<String, config::McpServerDef>,
}

/// Session-owned MCP connections. The runtime is independent from the
/// per-prompt Agent runner and evicts idle connections automatically.
pub struct SessionMcpRuntime {
    state: tokio::sync::Mutex<SessionMcpState>,
    idle_timeout: Duration,
    active: AtomicBool,
    last_used_at: std::sync::Mutex<Instant>,
}

impl std::fmt::Debug for SessionMcpRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionMcpRuntime")
            .field("idle_timeout", &self.idle_timeout)
            .field("active", &self.active.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl SessionMcpRuntime {
    pub fn new() -> Arc<Self> {
        Self::with_idle_timeout(DEFAULT_MCP_IDLE_TIMEOUT)
    }

    pub fn with_idle_timeout(idle_timeout: Duration) -> Arc<Self> {
        let runtime = Arc::new(Self {
            state: tokio::sync::Mutex::new(SessionMcpState {
                sources: HashMap::new(),
                definitions: HashMap::new(),
            }),
            idle_timeout,
            active: AtomicBool::new(false),
            last_used_at: std::sync::Mutex::new(Instant::now()),
        });
        let weak = Arc::downgrade(&runtime);
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let interval = idle_timeout
                    .min(Duration::from_secs(60))
                    .max(Duration::from_millis(50));
                loop {
                    tokio::time::sleep(interval).await;
                    let Some(runtime) = weak.upgrade() else { break };
                    runtime.reap_if_idle().await;
                }
            });
        }
        runtime
    }

    pub fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
        *self.last_used_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    pub async fn ensure_sources(
        &self,
        servers: &[config::McpServerDef],
    ) -> Result<Vec<(String, Arc<McpToolSource>)>, String> {
        let definitions: HashMap<_, _> = servers
            .iter()
            .cloned()
            .map(|server| (server.name().to_string(), server))
            .collect();
        let mut state = self.state.lock().await;
        let sources_healthy =
            if state.definitions == definitions && state.sources.len() == definitions.len() {
                let sources: Vec<_> = state.sources.values().cloned().collect();
                drop(state);
                let healthy =
                    futures::future::join_all(sources.iter().map(|source| source.is_closed()))
                        .await
                        .into_iter()
                        .all(|closed| !closed);
                state = self.state.lock().await;
                healthy
            } else {
                false
            };
        if sources_healthy
            && state.definitions == definitions
            && state.sources.len() == definitions.len()
        {
            *self.last_used_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
            return Ok(state
                .sources
                .iter()
                .map(|(name, source)| (name.clone(), Arc::clone(source)))
                .collect());
        }

        let old_sources: Vec<_> = state.sources.drain().map(|(_, source)| source).collect();
        state.definitions.clear();
        drop(state);
        for source in old_sources {
            source.shutdown().await;
        }

        let mut sources = HashMap::new();
        for server in servers {
            match start_session_mcp(server.clone()).await {
                Ok(source) => {
                    sources.insert(server.name().to_string(), source);
                }
                Err(error) if server.required() => {
                    return Err(format!(
                        "required MCP server `{}` failed to start: {error}",
                        server.name()
                    ));
                }
                Err(error) => {
                    tracing::warn!(
                        mcp_server = server.name(),
                        %error,
                        "optional session MCP server failed to start"
                    );
                }
            }
        }

        let mut state = self.state.lock().await;
        state.definitions = definitions;
        state.sources = sources;
        *self.last_used_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        Ok(state
            .sources
            .iter()
            .map(|(name, source)| (name.clone(), Arc::clone(source)))
            .collect())
    }

    pub async fn shutdown(&self) {
        let sources = {
            let mut state = self.state.lock().await;
            state.definitions.clear();
            state
                .sources
                .drain()
                .map(|(_, source)| source)
                .collect::<Vec<_>>()
        };
        for source in sources {
            source.shutdown().await;
        }
    }

    async fn reap_if_idle(&self) {
        let sources = {
            let mut state = self.state.lock().await;
            let last_used_at = *self.last_used_at.lock().unwrap_or_else(|e| e.into_inner());
            if self.active.load(Ordering::SeqCst)
                || state.sources.is_empty()
                || last_used_at.elapsed() < self.idle_timeout
            {
                return;
            }
            state.definitions.clear();
            state
                .sources
                .drain()
                .map(|(_, source)| source)
                .collect::<Vec<_>>()
        };
        tracing::debug!(count = sources.len(), "evicting idle ACP MCP connections");
        for source in sources {
            source.shutdown().await;
        }
    }
}

async fn start_session_mcp(server: config::McpServerDef) -> Result<Arc<McpToolSource>, String> {
    let startup_timeout = Duration::from_secs(server.startup_timeout_sec().unwrap_or(30));
    let tool_timeout = Duration::from_secs(server.tool_timeout_sec().unwrap_or(60));
    tokio::time::timeout(startup_timeout, async move {
        match server {
            config::McpServerDef::Stdio {
                command, args, env, ..
            } => McpToolSource::new_with_env_and_tool_timeout(
                command,
                args,
                env,
                false,
                tool_timeout,
            )
            .await
            .map(Arc::new)
            .map_err(|error| error.to_string()),
            config::McpServerDef::Http { url, headers, .. } => {
                McpToolSource::new_http_with_tool_timeout(url, headers, tool_timeout)
                    .await
                    .map(Arc::new)
                    .map_err(|error| error.to_string())
            }
        }
    })
    .await
    .map_err(|_| "MCP startup timed out".to_string())?
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SessionLifecycle {
    #[default]
    Idle,
    Running,
    Loading,
    Closed,
}

#[derive(Debug, Default)]
pub struct SessionCancellationState {
    pub current_generation: AtomicU64,
    pub current_turn: std::sync::RwLock<Option<Arc<RunningTurn>>>,
}

#[derive(Debug)]
pub struct RunningTurn {
    pub generation: u64,
    pub cancellation: RunCancellation,
}

/// In-memory session table: session_id -> [`SessionEntry`].
///
/// Concurrent reads and single-writer (RwLock); cancel flag is atomic so it can be checked without the lock.
/// Sessions live for the process; no persistence after exit.
#[derive(Debug, Default)]
pub struct SessionStore {
    inner: std::sync::RwLock<std::collections::HashMap<SessionId, SessionEntry>>,
}

impl SessionStore {
    /// Create an empty session store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new session and return its [`SessionId`].
    ///
    /// Corresponds to ACP `session/new`: Agent generates a unique session_id and adds an entry.
    /// `working_directory` comes from `NewSessionRequest::working_directory` (protocol requires absolute path);
    /// if not provided, pass `None`; prompt handling may use process cwd or another default.
    pub fn create(&self, working_directory: Option<PathBuf>) -> SessionId {
        self.create_owned(working_directory, "local-anonymous")
    }

    pub fn create_owned(
        &self,
        working_directory: Option<PathBuf>,
        owner_principal: impl Into<String>,
    ) -> SessionId {
        let session_id = SessionId(Uuid::new_v4().to_string());
        self.create_with_owner(
            session_id.clone(),
            working_directory,
            session_id.0.clone(),
            owner_principal,
        );
        session_id
    }

    /// Create a session with a specific session_id and thread_id.
    ///
    /// Used by `session/load` when loading an existing session.
    /// If a session with the same id already exists, returns the existing entry.
    pub fn create_with_id(
        &self,
        session_id: SessionId,
        working_directory: Option<PathBuf>,
        thread_id: String,
    ) -> SessionEntry {
        self.create_with_owner(session_id, working_directory, thread_id, "local-anonymous")
    }

    pub fn create_with_owner(
        &self,
        session_id: SessionId,
        working_directory: Option<PathBuf>,
        thread_id: String,
        owner_principal: impl Into<String>,
    ) -> SessionEntry {
        let mut guard = recover_write(&self.inner);
        if let Some(existing) = guard.get(&session_id) {
            return existing.clone();
        }
        let entry = SessionEntry {
            thread_id,
            owner_principal: owner_principal.into(),
            working_directory,
            cancelled: Arc::new(AtomicBool::new(false)),
            session_config: SessionConfig::default(),
            cancellation: Arc::new(SessionCancellationState::default()),
            control_lock: Arc::new(std::sync::Mutex::new(())),
            lifecycle: Arc::new(std::sync::RwLock::new(SessionLifecycle::Idle)),
            history_cursor: Arc::new(std::sync::atomic::AtomicUsize::new(usize::MAX)),
            mcp_servers: Vec::new(),
            mcp_runtime: SessionMcpRuntime::new(),
        };
        guard.insert(session_id.clone(), entry.clone());
        entry
    }

    /// Look up a session by session_id; returns `None` if not found.
    pub fn get(&self, session_id: &SessionId) -> Option<SessionEntry> {
        recover_read(&self.inner).get(session_id).cloned()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        recover_read(&self.inner).len()
    }

    /// Mark the given session as cancelled (call when receiving `session/cancel`).
    ///
    /// No-op if session_id is not in the store.
    pub fn set_cancelled(&self, session_id: SessionId) {
        self.cancel_current_generation(&session_id);
    }

    /// Begin a new prompt generation and return a fresh runtime cancellation handle.
    pub fn begin_prompt(&self, session_id: &SessionId) -> Option<RunCancellation> {
        if let Some(entry) = recover_read(&self.inner).get(session_id) {
            let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
            if *recover_read(&entry.lifecycle) != SessionLifecycle::Idle {
                return None;
            }
            // Check and install the turn under one write lock. Splitting this
            // into a read followed by a write allows two concurrent prompts
            // to both observe an empty slot.
            let mut current_turn = entry
                .cancellation
                .current_turn
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if current_turn.is_some() {
                return None;
            }
            let generation = entry
                .cancellation
                .current_generation
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            let cancellation = RunCancellation::new(generation);
            let turn = Arc::new(RunningTurn {
                generation,
                cancellation: cancellation.clone(),
            });
            *current_turn = Some(turn);
            *recover_write(&entry.lifecycle) = SessionLifecycle::Running;
            entry.mcp_runtime.set_active(true);
            entry.cancelled.store(false, Ordering::SeqCst);
            return Some(cancellation);
        }
        None
    }

    /// Mark the current generation as cancelled and trigger its runtime token.
    ///
    /// Also clears `current_turn` so that `begin_prompt` can succeed for the
    /// next prompt on this session.
    pub fn cancel_current_generation(&self, session_id: &SessionId) {
        let inner = recover_read(&self.inner);
        let Some(entry) = inner.get(session_id) else {
            return;
        };
        let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
        cancel_entry(entry);
    }

    pub fn has_active_prompt(&self, session_id: &SessionId) -> bool {
        recover_read(&self.inner)
            .get(session_id)
            .and_then(|entry| {
                entry
                    .cancellation
                    .current_turn
                    .read()
                    .ok()
                    .map(|turn| turn.is_some())
            })
            .unwrap_or(false)
    }

    /// Find the session that owns the given agent thread id.
    ///
    /// Used by the goal runtime's [`crate::goal_runtime::AcpTurnDriver`] to map a
    /// `thread_id` back to its session so idle continuation prompts can be routed
    /// through the normal `prompt` entry point. Returns the first match.
    pub fn find_session_id_by_thread(&self, thread_id: &str) -> Option<SessionId> {
        recover_read(&self.inner)
            .iter()
            .find(|(_, entry)| entry.thread_id == thread_id)
            .map(|(session_id, _)| session_id.clone())
    }

    pub fn cancel_all_generations(&self) {
        let inner = recover_read(&self.inner);
        for (session_id, entry) in inner.iter() {
            let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
            let had_turn = entry
                .cancellation
                .current_turn
                .read()
                .map(|turn| turn.is_some())
                .unwrap_or(false);
            cancel_entry(entry);
            if had_turn {
                tracing::info!(session_id = %session_id, "cancelled active generation on connection close");
            }
        }
    }

    pub fn finish_prompt(&self, session_id: &SessionId, generation: u64) {
        if let Some(entry) = recover_read(&self.inner).get(session_id) {
            if let Ok(mut current_turn) = entry.cancellation.current_turn.write() {
                let should_clear = current_turn
                    .as_ref()
                    .map(|turn| turn.generation == generation)
                    .unwrap_or(false);
                if should_clear {
                    *current_turn = None;
                    let mut lifecycle = recover_write(&entry.lifecycle);
                    if *lifecycle != SessionLifecycle::Closed {
                        *lifecycle = SessionLifecycle::Idle;
                    }
                    entry.mcp_runtime.set_active(false);
                }
            }
        }
    }

    /// Return whether this session has been cancelled.
    ///
    /// Returns `false` if session_id is not in the store.
    pub fn is_cancelled(&self, session_id: &SessionId) -> bool {
        recover_read(&self.inner)
            .get(session_id)
            .map(|e| e.cancelled.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    pub fn close(&self, session_id: &SessionId) -> bool {
        let inner = recover_read(&self.inner);
        let Some(entry) = inner.get(session_id) else {
            return false;
        };
        let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
        cancel_entry(entry);
        *recover_write(&entry.lifecycle) = SessionLifecycle::Closed;
        spawn_mcp_shutdown(&entry.mcp_runtime);
        true
    }

    pub fn reopen(&self, session_id: &SessionId) -> bool {
        let Some(entry) = recover_read(&self.inner).get(session_id).cloned() else {
            return false;
        };
        let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
        *recover_write(&entry.lifecycle) = SessionLifecycle::Idle;
        true
    }

    /// Reserve a session for a load/resume transition.
    ///
    /// The lifecycle change and active-turn check happen under the same
    /// control lock used by `begin_prompt`, so a prompt cannot start between
    /// the check and the caller's binding transition.
    #[allow(clippy::result_unit_err)]
    pub fn begin_restore(&self, session_id: &SessionId) -> Result<Option<SessionLifecycle>, ()> {
        let Some(entry) = recover_read(&self.inner).get(session_id).cloned() else {
            return Ok(None);
        };
        let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
        let current_turn = entry
            .cancellation
            .current_turn
            .read()
            .unwrap_or_else(|e| e.into_inner());
        if current_turn.is_some() {
            return Err(());
        }
        let mut lifecycle = recover_write(&entry.lifecycle);
        let previous = *lifecycle;
        *lifecycle = SessionLifecycle::Loading;
        Ok(Some(previous))
    }

    pub fn restore_lifecycle(&self, session_id: &SessionId, lifecycle: Option<SessionLifecycle>) {
        let Some(lifecycle) = lifecycle else {
            return;
        };
        if let Some(entry) = recover_read(&self.inner).get(session_id).cloned() {
            let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
            *recover_write(&entry.lifecycle) = lifecycle;
        }
    }

    /// Mark a session entry as `Loading`.
    ///
    /// Only transitions `Idle` sessions, so a `close` that raced an
    /// in-flight load is never undone. Used by `session/load` when it
    /// creates the in-memory entry for a session this process has not
    /// seen before: without this, a prompt pipelined behind the load
    /// would see `Idle` and interleave with the history replay.
    pub fn mark_loading(&self, session_id: &SessionId) {
        if let Some(entry) = recover_read(&self.inner).get(session_id).cloned() {
            let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut lifecycle = recover_write(&entry.lifecycle);
            if *lifecycle == SessionLifecycle::Idle {
                *lifecycle = SessionLifecycle::Loading;
            }
        }
    }

    /// Complete a restore transition by moving the session to `target`.
    ///
    /// Only moves sessions in `Loading` (normal completion) or `Idle`
    /// (direct `load_session` calls that never reserved the transition).
    /// A session that was `close`d while the load was in flight stays
    /// `Closed`, so the rollback path also must not resurrect it.
    /// Callers gate the durable lifecycle write on the return value.
    pub fn finish_restore(&self, session_id: &SessionId, target: SessionLifecycle) -> bool {
        if let Some(entry) = recover_read(&self.inner).get(session_id).cloned() {
            let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
            let mut lifecycle = recover_write(&entry.lifecycle);
            if matches!(
                *lifecycle,
                SessionLifecycle::Loading | SessionLifecycle::Idle
            ) {
                *lifecycle = target;
                return true;
            }
        }
        false
    }

    pub fn delete(&self, session_id: &SessionId) -> bool {
        let Some(entry) = recover_read(&self.inner).get(session_id).cloned() else {
            return false;
        };
        let _control = entry.control_lock.lock().unwrap_or_else(|e| e.into_inner());
        cancel_entry(&entry);
        spawn_mcp_shutdown(&entry.mcp_runtime);
        recover_write(&self.inner).remove(session_id).is_some()
    }

    /// Update session config for the given session. No-op if session_id is not in the store.
    pub fn update_session_config<F>(&self, session_id: &SessionId, f: F)
    where
        F: FnOnce(&mut SessionConfig),
    {
        if let Ok(mut guard) = self.inner.write() {
            if let Some(entry) = guard.get_mut(session_id) {
                f(&mut entry.session_config);
            }
        }
    }

    /// Update MCP servers for the given session. No-op if session_id is not in the store.
    pub fn update_mcp_servers(&self, session_id: &SessionId, servers: Vec<config::McpServerDef>) {
        if let Ok(mut guard) = self.inner.write() {
            if let Some(entry) = guard.get_mut(session_id) {
                entry.mcp_servers = servers;
            }
        }
    }
}

fn cancel_entry(entry: &SessionEntry) {
    entry.mcp_runtime.set_active(false);
    entry.cancelled.store(true, Ordering::SeqCst);
    let mut current_turn = entry
        .cancellation
        .current_turn
        .write()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(turn) = current_turn.as_ref() {
        turn.cancellation.cancel();
    }
    *current_turn = None;
    let mut lifecycle = recover_write(&entry.lifecycle);
    if *lifecycle == SessionLifecycle::Running {
        *lifecycle = SessionLifecycle::Idle;
    }
}

fn spawn_mcp_shutdown(runtime: &Arc<SessionMcpRuntime>) {
    let runtime = Arc::clone(runtime);
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move { runtime.shutdown().await });
    }
}

impl Clone for SessionEntry {
    fn clone(&self) -> Self {
        SessionEntry {
            thread_id: self.thread_id.clone(),
            owner_principal: self.owner_principal.clone(),
            working_directory: self.working_directory.clone(),
            cancelled: Arc::clone(&self.cancelled),
            session_config: self.session_config.clone(),
            cancellation: Arc::clone(&self.cancellation),
            control_lock: Arc::clone(&self.control_lock),
            lifecycle: Arc::clone(&self.lifecycle),
            history_cursor: Arc::clone(&self.history_cursor),
            mcp_servers: self.mcp_servers.clone(),
            mcp_runtime: Arc::clone(&self.mcp_runtime),
        }
    }
}

// ---------------------------------------------------------------------------
// Prompt guard — RAII cleanup for cancelled prompts
// ---------------------------------------------------------------------------

/// RAII guard that calls [`SessionStore::finish_prompt`] on drop.
///
/// When the prompt future is dropped mid-execution (e.g., the WebSocket
/// connection drops while a prompt is in flight), Rust's cancellation
/// semantics mean code after the last `.await` is never reached.  This guard
/// ensures `finish_prompt` is called regardless, preventing the session from
/// being permanently blocked.
pub(crate) struct PromptGuard<'a> {
    sessions: &'a SessionStore,
    session_id: &'a SessionId,
    generation: u64,
}

impl<'a> PromptGuard<'a> {
    pub(crate) fn new(
        sessions: &'a SessionStore,
        session_id: &'a SessionId,
        generation: u64,
    ) -> Self {
        Self {
            sessions,
            session_id,
            generation,
        }
    }
}

impl Drop for PromptGuard<'_> {
    fn drop(&mut self) {
        self.sessions
            .finish_prompt(self.session_id, self.generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newly_created_session_id_is_an_unprefixed_uuid() {
        let store = SessionStore::new();
        let id = store.create(None);

        assert!(!id.as_str().starts_with("session-"));
        assert!(Uuid::parse_str(id.as_str()).is_ok());
    }

    #[test]
    fn session_config_model_updated_by_update_session_config() {
        let store = SessionStore::new();
        let id = store.create(None);
        assert!(store.get(&id).unwrap().session_config.model.is_none());

        store.update_session_config(&id, |c| c.model = Some("gpt-4o".to_string()));
        assert_eq!(
            store.get(&id).unwrap().session_config.model.as_deref(),
            Some("gpt-4o")
        );
    }

    #[test]
    fn update_mcp_servers_sets_and_gets() {
        use config::McpServerDef;

        let store = SessionStore::new();
        let id = store.create(None);
        assert!(store.get(&id).unwrap().mcp_servers.is_empty());

        let servers = vec![McpServerDef::Http {
            name: "test".into(),
            url: "https://example.com/mcp".into(),
            headers: std::collections::HashMap::new(),
            oauth: None,
            required: false,
            startup_timeout_sec: None,
            tool_timeout_sec: None,
        }];
        store.update_mcp_servers(&id, servers);

        let entry = store.get(&id).unwrap();
        assert_eq!(entry.mcp_servers.len(), 1);
        assert_eq!(entry.mcp_servers[0].name(), "test");
    }

    #[test]
    fn begin_prompt_cancel_and_finish_manage_current_turn() {
        let store = SessionStore::new();
        let id = store.create(None);

        let cancellation = store.begin_prompt(&id).expect("begin prompt");
        assert!(!cancellation.token().is_cancelled());
        assert!(!store.is_cancelled(&id));

        store.cancel_current_generation(&id);
        assert!(store.is_cancelled(&id));
        assert!(cancellation.token().is_cancelled());

        store.finish_prompt(&id, cancellation.generation());
        let entry = store.get(&id).expect("session entry");
        assert!(entry
            .cancellation
            .current_turn
            .read()
            .expect("read current turn")
            .is_none());
    }

    #[test]
    fn begin_prompt_rejects_overlapping_turns() {
        let store = SessionStore::new();
        let id = store.create(None);
        let first = store.begin_prompt(&id).expect("first prompt");
        assert!(store.begin_prompt(&id).is_none());
        store.finish_prompt(&id, first.generation());
        assert!(store.begin_prompt(&id).is_some());
    }

    #[test]
    fn concurrent_begin_prompt_has_one_winner() {
        let store = Arc::new(SessionStore::new());
        let id = store.create(None);
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut threads = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            let id = id.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                store.begin_prompt(&id).is_some()
            }));
        }
        let winners = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1);
    }

    #[test]
    fn cancel_clears_current_turn() {
        let store = SessionStore::new();
        let id = store.create(None);
        let _ = store.begin_prompt(&id).expect("prompt");
        assert!(
            store.begin_prompt(&id).is_none(),
            "should block while active"
        );
        store.cancel_current_generation(&id);
        assert!(
            store.begin_prompt(&id).is_some(),
            "should allow new prompt after cancel clears current_turn"
        );
    }

    #[test]
    fn cancel_all_clears_current_turn() {
        let store = SessionStore::new();
        let id1 = store.create(None);
        let id2 = store.create(None);
        let _ = store.begin_prompt(&id1).expect("prompt 1");
        let _ = store.begin_prompt(&id2).expect("prompt 2");
        store.cancel_all_generations();
        assert!(
            store.begin_prompt(&id1).is_some(),
            "id1 should be free after cancel_all"
        );
        assert!(
            store.begin_prompt(&id2).is_some(),
            "id2 should be free after cancel_all"
        );
    }

    #[test]
    fn close_is_idempotent_and_load_can_reopen() {
        let store = SessionStore::new();
        let id = store.create(None);
        assert!(store.close(&id));
        assert!(store.close(&id));
        assert!(store.begin_prompt(&id).is_none());
        assert!(store.reopen(&id));
        assert!(store.begin_prompt(&id).is_some());
    }

    #[test]
    fn restore_transition_blocks_prompts_and_rolls_back() {
        let store = SessionStore::new();
        let id = store.create(None);
        let previous = store.begin_restore(&id).expect("reserve restore");
        assert_eq!(previous, Some(SessionLifecycle::Idle));
        assert!(store.begin_prompt(&id).is_none());
        store.restore_lifecycle(&id, previous);
        assert!(store.begin_prompt(&id).is_some());
    }

    #[test]
    fn restore_transition_rejects_an_active_prompt() {
        let store = SessionStore::new();
        let id = store.create(None);
        let prompt = store.begin_prompt(&id).expect("prompt");
        assert!(store.begin_restore(&id).is_err());
        store.finish_prompt(&id, prompt.generation());
        assert!(store.begin_restore(&id).is_ok());
    }

    #[test]
    fn delete_missing_is_idempotent() {
        let store = SessionStore::new();
        let id = SessionId::new("missing");
        assert!(!store.delete(&id));
        let existing = store.create(None);
        assert!(store.delete(&existing));
        assert!(!store.delete(&existing));
    }

    #[test]
    fn mark_loading_blocks_prompt_until_finish_restore() {
        let store = SessionStore::new();
        let id = store.create(None);
        store.mark_loading(&id);
        assert_eq!(
            *store.get(&id).unwrap().lifecycle.read().unwrap(),
            SessionLifecycle::Loading
        );
        assert!(
            store.begin_prompt(&id).is_none(),
            "prompt must not interleave with a history replay"
        );
        assert!(store.finish_restore(&id, SessionLifecycle::Idle));
        assert!(store.begin_prompt(&id).is_some());
    }

    #[test]
    fn finish_restore_does_not_resurrect_a_closed_session() {
        let store = SessionStore::new();
        let id = store.create(None);
        let previous = store.begin_restore(&id).expect("reserve restore");
        store.close(&id);
        assert!(!store.finish_restore(&id, previous.unwrap_or(SessionLifecycle::Idle)));
        assert_eq!(
            *store.get(&id).unwrap().lifecycle.read().unwrap(),
            SessionLifecycle::Closed,
            "close issued while a load was in flight must win"
        );
    }
}

//! Server-owned ACP runtime shared by all transports.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::sync::{oneshot, Mutex, Semaphore};
use uuid::Uuid;

use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, SessionId as AcpSessionId, SessionNotification, SessionUpdate,
    TextContent,
};

use crate::connection::{AcpConnection, ConnectionOutbound};
use crate::connection_registry::ConnectionRegistry;
use crate::extensions::{register::register_default_extensions, ExtensionRegistry};
use crate::notification_router::NotificationRouter;
use crate::prompt_executor::{AcpPromptExecutor, AnureoPromptExecutor};
use crate::session_bindings::SessionBindings;
use crate::session_update_log::SessionUpdateLog;
use crate::tools::ClientBridgeTrait;
use crate::AnureoAcpAgent;

pub struct OpenConnection {
    pub connection: Arc<AcpConnection>,
    pub outbound_rx: mpsc::Receiver<ConnectionOutbound>,
}

#[derive(Debug, Default)]
pub struct AcpRuntimeMetrics {
    total_prompts: AtomicU64,
    active_prompts: AtomicU64,
    prompt_busy_rejections: AtomicU64,
    route_failures: AtomicU64,
    session_rebinds: AtomicU64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AcpRuntimeMetricsSnapshot {
    pub total_prompts: u64,
    pub active_prompts: u64,
    pub active_sessions: u64,
    pub active_connections: u64,
    pub prompt_busy_rejections: u64,
    pub route_failures: u64,
    pub session_rebinds: u64,
    pub legacy_session_list_alias_calls: u64,
}

/// One anureo agent core plus all transient ACP connection state.
pub struct AcpRuntime {
    pub agent: Arc<AnureoAcpAgent>,
    pub bindings: Arc<SessionBindings>,
    pub connections: Arc<ConnectionRegistry>,
    pub notification_router: Arc<NotificationRouter>,
    pub extensions: Arc<ExtensionRegistry>,
    pub question_handler: Arc<crate::extensions::question::QuestionHandler>,
    pub global_bus: Arc<crate::global_events::GlobalEventBus>,
    pub session_update_log: Arc<SessionUpdateLog>,
    prompt_executor: Arc<dyn AcpPromptExecutor>,
    prompt_capacity: Arc<Semaphore>,
    metrics: Arc<AcpRuntimeMetrics>,
    session_list_handler: Arc<crate::extensions::session_list::SessionListHandler>,
    updates_tx: mpsc::UnboundedSender<crate::stream_bridge::SessionUpdateEnvelope>,
    flush_waiters: Arc<Mutex<std::collections::HashMap<String, oneshot::Sender<()>>>>,
    #[allow(clippy::type_complexity)]
    session_bridges: Arc<Mutex<HashMap<String, Vec<Arc<dyn ClientBridgeTrait>>>>>,
}

impl std::fmt::Debug for AcpRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpRuntime")
            .field("connections", &self.connections.len())
            .finish_non_exhaustive()
    }
}

impl AcpRuntime {
    pub fn new() -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        Self::with_prompt_executor(Arc::new(AnureoPromptExecutor))
    }

    pub fn with_prompt_executor(
        prompt_executor: Arc<dyn AcpPromptExecutor>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        Self::with_prompt_executor_and_db_path(
            prompt_executor,
            checkpoint_sqlite_store::default_memory_db_path(),
        )
    }

    /// Construct an embedded runtime whose session metadata, checkpoints and
    /// The session update log and checkpoint store share an explicit SQLite database.
    pub fn with_prompt_executor_and_db_path(
        prompt_executor: Arc<dyn AcpPromptExecutor>,
        db_path: impl Into<PathBuf>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let (updates_tx, mut updates_rx) = mpsc::unbounded_channel();
        let global_bus = Arc::new(crate::global_events::GlobalEventBus::new());
        let connections = Arc::new(ConnectionRegistry::default());
        let bindings = Arc::new(SessionBindings::new());
        let mut extension_registry = ExtensionRegistry::new();
        let extension_handles = register_default_extensions(
            &mut extension_registry,
            global_bus.clone(),
            Some(connections.clone()),
        );
        let extensions = Arc::new(extension_registry);
        let agent = Arc::new(AnureoAcpAgent::new_with_extension_registry_and_db_path(
            extensions.clone(),
            Some(updates_tx.clone()),
            db_path.into(),
        )?);
        let session_update_log = Arc::new(SessionUpdateLog::new(
            connections.clone(),
            bindings.clone(),
            &agent.checkpoint_db_path,
        )?);
        extension_handles.session_history.bind(&agent);
        let session_list_handler = extension_handles.session_list.clone();
        session_list_handler.bind(&agent);
        global_bus.bind_registry(connections.clone());
        let notification_router = Arc::new(NotificationRouter::new(
            bindings.clone(),
            connections.clone(),
        ));
        let flush_waiters = Arc::new(Mutex::new(std::collections::HashMap::new()));
        let metrics = Arc::new(AcpRuntimeMetrics::default());
        let session_bridges = Arc::new(Mutex::new(HashMap::new()));
        let runtime = Arc::new(Self {
            agent,
            bindings,
            connections,
            notification_router,
            extensions,
            question_handler: extension_handles.question,
            global_bus,
            session_update_log: session_update_log.clone(),
            prompt_executor,
            prompt_capacity: Arc::new(Semaphore::new(
                std::env::var("ANUREO_ACP_MAX_CONCURRENT_PROMPTS")
                    .ok()
                    .and_then(|value| value.parse::<usize>().ok())
                    .filter(|value| *value > 0)
                    .unwrap_or(4),
            )),
            metrics: metrics.clone(),
            session_list_handler,
            updates_tx,
            flush_waiters: flush_waiters.clone(),
            session_bridges,
        });

        let router = runtime.notification_router.clone();
        let session_update_log_for_router = session_update_log.clone();
        let metrics_for_router = metrics;
        tokio::spawn(async move {
            while let Some(envelope) = updates_rx.recv().await {
                let (update, recovery_replay) = match envelope {
                    crate::stream_bridge::SessionUpdateEnvelope::Session(notification) => {
                        (notification, false)
                    }
                    crate::stream_bridge::SessionUpdateEnvelope::SessionReplay {
                        notification,
                        recovery,
                    } => (notification, recovery),
                    crate::stream_bridge::SessionUpdateEnvelope::HistoryBatch {
                        session_id,
                        updates,
                    } => {
                        // Batched `session/load` tail replay: one envelope →
                        // one `_anureo.dev/session-history/batch` custom
                        // notification per bound connection. A load is a read,
                        // so no cross-connection `session.updated` broadcast.
                        if let Err(error) = router.send_history_batch(&session_id, updates).await {
                            metrics_for_router
                                .route_failures
                                .fetch_add(1, Ordering::Relaxed);
                            tracing::debug!(%error, "ACP history batch could not be routed");
                        }
                        continue;
                    }
                };
                let update_session_id = update.session_id.to_string();
                if update_session_id.starts_with("__anureo_flush__") {
                    if let Some(waiter) = flush_waiters.lock().await.remove(&update_session_id) {
                        let _ = waiter.send(());
                    }
                    continue;
                }
                if update_session_id.starts_with(crate::stream_bridge::REPLAY_BEGIN_MARKER_PREFIX)
                    || update_session_id.starts_with(crate::stream_bridge::REPLAY_END_MARKER_PREFIX)
                {
                    continue;
                }
                // Session content notifications are routed to the bound ACP
                // connection only. They must not masquerade as global
                // SessionIndex mutations: the canonical list event is
                // published by the mutation boundary with revision/index
                // metadata, while dropped events are recovered by resync.
                let routed_update = if recovery_replay {
                    // Extended full-load history is canonical ACP replay, but
                    // it is not a new event. The response baseline advances
                    // over the existing log; recording it here would append
                    // duplicate history on every load.
                    update
                } else {
                    session_update_log_for_router
                        .record(&update)
                        .await
                        .unwrap_or(update)
                };
                if let Err(error) = router.send(routed_update).await {
                    metrics_for_router
                        .route_failures
                        .fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(%error, "ACP notification could not be routed");
                }
            }
        });
        Ok(runtime)
    }

    pub async fn execute_prompt(
        &self,
        request: agent_client_protocol::schema::v1::PromptRequest,
        capabilities: crate::client_capabilities::ClientCapabilitiesInfo,
        bridge: Arc<dyn crate::tools::ClientBridgeTrait>,
    ) -> agent_client_protocol::Result<agent_client_protocol::schema::v1::PromptResponse> {
        let session_id = request.session_id.to_string();
        self.session_bridges
            .lock()
            .await
            .entry(session_id.clone())
            .or_default()
            .push(bridge.clone());
        let _permit = self.prompt_capacity.acquire().await.map_err(|_| {
            agent_client_protocol::Error::internal_error().data("prompt capacity is closed")
        })?;
        self.metrics.total_prompts.fetch_add(1, Ordering::Relaxed);
        self.metrics.active_prompts.fetch_add(1, Ordering::Relaxed);
        self.record_user_turn(&request.session_id, &request.prompt).await;
        let result = self
            .prompt_executor
            .execute(
                &self.agent,
                &self.notification_router,
                request,
                capabilities,
                bridge,
            )
            .await;
        self.metrics.active_prompts.fetch_sub(1, Ordering::Relaxed);
        if result
            .as_ref()
            .err()
            .map(|error| error.to_string().contains("already in progress"))
            .unwrap_or(false)
        {
            self.metrics
                .prompt_busy_rejections
                .fetch_add(1, Ordering::Relaxed);
        }
        self.cleanup_session_resources(&session_id).await;
        result
    }

    /// Registering bridges per prompt lets lifecycle handlers clean up client
    /// resources even while a prompt is still unwinding after cancellation.
    pub async fn cleanup_session_resources(&self, session_id: impl AsRef<str>) {
        let session_id = session_id.as_ref();
        let bridges = self
            .session_bridges
            .lock()
            .await
            .remove(session_id)
            .unwrap_or_default();
        for bridge in bridges {
            bridge.cleanup().await;
        }
    }

    pub fn metrics(&self) -> AcpRuntimeMetricsSnapshot {
        AcpRuntimeMetricsSnapshot {
            total_prompts: self.metrics.total_prompts.load(Ordering::Relaxed),
            active_prompts: self.metrics.active_prompts.load(Ordering::Relaxed),
            active_sessions: self.agent.sessions().len() as u64,
            active_connections: self.connections.len() as u64,
            prompt_busy_rejections: self.metrics.prompt_busy_rejections.load(Ordering::Relaxed),
            route_failures: self.metrics.route_failures.load(Ordering::Relaxed),
            session_rebinds: self.metrics.session_rebinds.load(Ordering::Relaxed),
            legacy_session_list_alias_calls: self.session_list_handler.legacy_alias_call_count(),
        }
    }

    pub fn record_session_rebind(&self) {
        self.metrics.session_rebinds.fetch_add(1, Ordering::Relaxed);
    }

    /// Establish an ingress and outbound FIFO barrier for history replay.
    pub async fn flush_notifications(
        &self,
        session_id: &crate::session::SessionId,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let marker_id = format!("__anureo_flush__{}", Uuid::new_v4());
        let (ack_tx, ack_rx) = oneshot::channel();
        self.flush_waiters
            .lock()
            .await
            .insert(marker_id.clone(), ack_tx);
        let marker = SessionNotification::new(
            AcpSessionId::new(marker_id.clone()),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new(String::new()),
            ))),
        );
        if self
            .updates_tx
            .send(crate::stream_bridge::SessionUpdateEnvelope::Session(marker))
            .is_err()
        {
            self.flush_waiters.lock().await.remove(&marker_id);
            return Err("ACP notification ingress is closed".into());
        }
        ack_rx.await.map_err(|_| "ACP ingress flush was dropped")?;
        self.notification_router.flush_session(session_id).await?;
        Ok(())
    }

    /// Record the user's prompt turn into the session event log and broadcast
    /// it to bound connections. Streaming never emits `user_message_chunk`
    /// (see stream_bridge), so without this the seq'd replay log — the source
    /// of `session/load` attach replays — contains no user turns and a
    /// restored conversation projects as one unbroken assistant turn (the
    /// legacy turn grouping only starts a turn at `role: "user"`).
    async fn record_user_turn(
        &self,
        session_id: &agent_client_protocol::schema::v1::SessionId,
        prompt: &[agent_client_protocol::schema::v1::ContentBlock],
    ) {
        let text = prompt
            .iter()
            .filter_map(|block| match block {
                agent_client_protocol::schema::v1::ContentBlock::Text(text) => {
                    Some(text.text.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return;
        }
        let notification = crate::stream_bridge::stream_update_to_session_notification(
            session_id,
            &crate::stream_bridge::StreamUpdate::UserMessageChunk {
                text,
                message_id: None,
            },
        );
        if let Some(notification) = notification {
            let _ = self
                .updates_tx
                .send(crate::stream_bridge::SessionUpdateEnvelope::Session(notification));
        }
    }

    pub fn open_connection(&self, principal: String) -> OpenConnection {
        let (outbound_tx, outbound_rx) = mpsc::channel(256);
        let connection = Arc::new(AcpConnection::shell(
            Uuid::new_v4().to_string(),
            principal,
            outbound_tx,
        ));
        self.connections.insert(connection.clone());
        OpenConnection {
            connection,
            outbound_rx,
        }
    }

    /// Deactivate one transport and remove only its session bindings.
    pub fn close_connection(
        &self,
        connection_id: &str,
        cancel_bound_turns: bool,
    ) -> Vec<crate::session::SessionId> {
        if let Some(connection) = self.connections.remove(connection_id) {
            connection.deactivate();
        }
        let sessions = self.bindings.unbind_connection(connection_id);
        for session_id in &sessions {
            if cancel_bound_turns {
                self.agent.sessions().cancel_current_generation(session_id);
            }
        }
        sessions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ClientBridgeTrait, TerminalExitResult, TerminalOutput};
    use std::sync::atomic::AtomicUsize;

    struct ParallelPromptProbe {
        rendezvous: tokio::sync::Barrier,
        active: AtomicUsize,
        max_active: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl AcpPromptExecutor for ParallelPromptProbe {
        async fn execute(
            &self,
            agent: &AnureoAcpAgent,
            _router: &NotificationRouter,
            request: agent_client_protocol::schema::v1::PromptRequest,
            _capabilities: crate::client_capabilities::ClientCapabilitiesInfo,
            _bridge: Arc<dyn ClientBridgeTrait>,
        ) -> agent_client_protocol::Result<agent_client_protocol::schema::v1::PromptResponse>
        {
            let session_id = crate::session::SessionId::new(request.session_id.to_string());
            let cancellation = agent.sessions().begin_prompt(&session_id).ok_or_else(|| {
                agent_client_protocol::Error::new(-32010, "prompt unexpectedly busy")
            })?;
            let _guard = crate::session::PromptGuard::new(
                agent.sessions(),
                &session_id,
                cancellation.generation(),
            );
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            self.rendezvous.wait().await;
            tokio::task::yield_now().await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(agent_client_protocol::schema::v1::PromptResponse::new(
                agent_client_protocol::schema::v1::StopReason::EndTurn,
            ))
        }
    }

    struct CleanupProbe(Arc<AtomicUsize>);

    #[async_trait::async_trait]
    impl ClientBridgeTrait for CleanupProbe {
        fn is_available(&self) -> bool {
            true
        }

        async fn cleanup(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }

        async fn read_text_file(
            &self,
            _path: &str,
            _line: Option<u32>,
            _limit: Option<u32>,
        ) -> Result<String, String> {
            Err("unused".into())
        }

        async fn write_text_file(&self, _path: &str, _content: &str) -> Result<(), String> {
            Err("unused".into())
        }

        async fn terminal_create(
            &self,
            _session_id: &str,
            _command: &str,
            _args: Vec<String>,
            _env: Vec<(String, String)>,
            _cwd: Option<String>,
            _output_byte_limit: Option<u64>,
        ) -> Result<String, String> {
            Err("unused".into())
        }

        async fn terminal_output(
            &self,
            _session_id: &str,
            _terminal_id: &str,
        ) -> Result<TerminalOutput, String> {
            Err("unused".into())
        }

        async fn terminal_wait_for_exit(
            &self,
            _session_id: &str,
            _terminal_id: &str,
        ) -> Result<TerminalExitResult, String> {
            Err("unused".into())
        }

        async fn terminal_kill(&self, _session_id: &str, _terminal_id: &str) -> Result<(), String> {
            Err("unused".into())
        }

        async fn terminal_release(
            &self,
            _session_id: &str,
            _terminal_id: &str,
        ) -> Result<(), String> {
            Err("unused".into())
        }
    }

    #[tokio::test]
    async fn connections_are_independent() {
        let runtime = AcpRuntime::new().expect("runtime");
        let first = runtime.open_connection("owner-a".into());
        let second = runtime.open_connection("owner-a".into());
        assert_ne!(first.connection.id, second.connection.id);
        assert_eq!(runtime.connections.len(), 2);

        runtime.close_connection(&second.connection.id, false);
        assert!(first.connection.is_active());
        assert!(!second.connection.is_active());
        assert_eq!(runtime.connections.len(), 1);
    }

    #[tokio::test]
    async fn different_sessions_execute_prompts_in_parallel() {
        let temp = tempfile::tempdir().expect("tempdir");
        let probe = Arc::new(ParallelPromptProbe {
            rendezvous: tokio::sync::Barrier::new(2),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
        });
        let runtime = AcpRuntime::with_prompt_executor_and_db_path(
            probe.clone(),
            temp.path().join("memory.db"),
        )
        .expect("runtime");
        let cwd = std::env::current_dir().expect("cwd");
        let session_a = runtime
            .agent
            .sessions()
            .create_owned(Some(cwd.clone()), "owner-a");
        let session_b = runtime.agent.sessions().create_owned(Some(cwd), "owner-a");
        let request = |session: &crate::session::SessionId| {
            agent_client_protocol::schema::v1::PromptRequest::new(
                session.as_str().to_owned(),
                vec![ContentBlock::Text(TextContent::new("parallel"))],
            )
        };
        let bridge_a: Arc<dyn ClientBridgeTrait> =
            Arc::new(CleanupProbe(Arc::new(AtomicUsize::new(0))));
        let bridge_b: Arc<dyn ClientBridgeTrait> =
            Arc::new(CleanupProbe(Arc::new(AtomicUsize::new(0))));

        let both = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(
                runtime.execute_prompt(
                    request(&session_a),
                    crate::client_capabilities::ClientCapabilitiesInfo::default(),
                    bridge_a,
                ),
                runtime.execute_prompt(
                    request(&session_b),
                    crate::client_capabilities::ClientCapabilitiesInfo::default(),
                    bridge_b,
                )
            )
        })
        .await
        .expect("different sessions should not serialize or deadlock");

        assert!(both.0.is_ok());
        assert!(both.1.is_ok());
        assert_eq!(probe.max_active.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn persist_disconnect_unbinds_without_cancelling_turn() {
        let runtime = AcpRuntime::new().expect("runtime");
        let opened = runtime.open_connection("owner-a".into());
        let session = runtime
            .agent
            .sessions()
            .create_owned(Some(std::env::current_dir().expect("cwd")), "owner-a");
        runtime
            .bindings
            .bind_new_session(session.clone(), opened.connection.id.clone());
        let cancellation = runtime
            .agent
            .sessions()
            .begin_prompt(&session)
            .expect("active prompt");

        runtime.close_connection(&opened.connection.id, false);

        assert!(runtime.bindings.connection_for(&session).is_none());
        assert!(!cancellation.token().is_cancelled());
        assert!(runtime.agent.sessions().has_active_prompt(&session));
        runtime
            .agent
            .sessions()
            .finish_prompt(&session, cancellation.generation());
    }

    #[tokio::test]
    async fn cancel_disconnect_cancels_only_bound_turns() {
        let runtime = AcpRuntime::new().expect("runtime");
        let first = runtime.open_connection("owner-a".into());
        let second = runtime.open_connection("owner-a".into());
        let cwd = std::env::current_dir().expect("cwd");
        let first_session = runtime
            .agent
            .sessions()
            .create_owned(Some(cwd.clone()), "owner-a");
        let second_session = runtime.agent.sessions().create_owned(Some(cwd), "owner-a");
        runtime
            .bindings
            .bind_new_session(first_session.clone(), first.connection.id.clone());
        runtime
            .bindings
            .bind_new_session(second_session.clone(), second.connection.id.clone());
        let first_turn = runtime
            .agent
            .sessions()
            .begin_prompt(&first_session)
            .expect("first prompt");
        let second_turn = runtime
            .agent
            .sessions()
            .begin_prompt(&second_session)
            .expect("second prompt");

        runtime.close_connection(&first.connection.id, true);

        assert!(first_turn.token().is_cancelled());
        assert!(!second_turn.token().is_cancelled());
        assert!(runtime.agent.sessions().has_active_prompt(&second_session));
        runtime
            .agent
            .sessions()
            .finish_prompt(&second_session, second_turn.generation());
    }

    #[tokio::test]
    async fn cleanup_session_resources_invokes_registered_bridges_once() {
        let runtime = AcpRuntime::new().expect("runtime");
        let calls = Arc::new(AtomicUsize::new(0));
        let bridge: Arc<dyn ClientBridgeTrait> = Arc::new(CleanupProbe(calls.clone()));
        runtime
            .session_bridges
            .lock()
            .await
            .insert("session-cleanup".into(), vec![bridge]);

        runtime.cleanup_session_resources("session-cleanup").await;
        runtime.cleanup_session_resources("session-cleanup").await;

        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }
}

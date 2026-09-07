//! Shared ACP connection logic.
//!
//! Exports [`run_agent_connection`] which drives the ACP JSON-RPC dispatch
//! loop over any line-based transport (stdin/stdout or WebSocket).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::client_capabilities::ClientCapabilitiesInfo;
use crate::connection::{AcpConnection, ConnectionOutbound};
use crate::extensions::session_list::{
    to_event_info_from_index, to_event_info_from_wire, to_tombstone_event_info,
    tombstone_event_payload,
};
use crate::runtime::AcpRuntime;
use crate::session::SessionId as AnureoSessionId;
use agent_client_protocol::schema::v1::{
    AuthenticateRequest, AuthenticateResponse, CancelNotification, CloseSessionRequest,
    CloseSessionResponse, DeleteSessionRequest, DeleteSessionResponse, ForkSessionRequest,
    ForkSessionResponse, InitializeRequest, InitializeResponse, ListSessionsRequest,
    ListSessionsResponse, LoadSessionRequest, LoadSessionResponse, NewSessionRequest,
    NewSessionResponse, PromptRequest, PromptResponse, ResumeSessionRequest, ResumeSessionResponse,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse, SetSessionModeRequest,
    SetSessionModeResponse,
};
use agent_client_protocol::{
    on_receive_notification, on_receive_request, Agent, Client, ConnectionTo, Handled, Lines,
    Responder, UntypedMessage,
};

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn is_connection_closed_error_str(s: &str) -> bool {
    s.contains("receiver dropped")
        || s.contains("receiver is gone")
        || s.contains("failed to send response")
        || s.contains("broken pipe")
        || s.contains("unexpected eof")
}

// ---------------------------------------------------------------------------
// `_anureo.dev/*` extension dispatch
// ---------------------------------------------------------------------------

/// Build the extension context for a connection-scoped extension call.
///
/// `working_directory` is resolved from well-known params (`cwd`, `directory`)
/// so a single browser connection can operate on multiple project roots.
/// When absent, it falls back to the resolved session's working directory
/// (params `sessionId` or the connection's last session), matching the spec's
/// "Server 从当前 session 的 workingDirectory 解析 root" rule.
/// Params carry absolute paths, so boundary checks remain meaningful when no
/// directory is provided.
async fn extension_context_for(
    runtime: &AcpRuntime,
    connection: &AcpConnection,
    params: &serde_json::Value,
) -> crate::extensions::ExtensionContext {
    let capabilities = connection.require_capabilities().await.unwrap_or_default();
    let params_session_id = params
        .get("sessionId")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.to_string());
    let session_id = match params_session_id {
        Some(id) => Some(id),
        // Fall back to the most recent session bound to this connection so
        // project create/remove authorization passes for browser clients
        // that call connection-scoped extensions without a session param.
        None => connection.last_session_id().await,
    };
    let working_directory = ["cwd", "directory"]
        .iter()
        .find_map(|key| params.get(*key).and_then(|v| v.as_str()))
        .filter(|raw| !raw.trim().is_empty() && *raw != "/")
        .map(std::path::PathBuf::from)
        .and_then(|p| {
            if p.is_dir() {
                std::fs::canonicalize(&p).ok()
            } else {
                None
            }
        })
        .or_else(|| {
            session_id
                .as_deref()
                .map(crate::session::SessionId::new)
                .and_then(|id| runtime.agent.sessions().get(&id))
                .and_then(|entry| entry.working_directory.clone())
        });
    crate::extensions::ExtensionContext {
        session_id,
        principal: connection.principal.clone(),
        connection_id: connection.id.clone(),
        working_directory,
        client_capabilities: capabilities,
    }
}

// ---------------------------------------------------------------------------
// Shared connection logic (used by WebSocket transport on the server side)
// ---------------------------------------------------------------------------

/// Register all ACP handlers on the builder and drive `connect_with` to
/// completion over any `Lines`-compatible transport.
///
/// Creates an internal notification drain task that forwards
/// [`SessionNotification`]s from the agent channel to the client connection.
pub async fn run_agent_connection<S, St, F>(
    runtime: Arc<AcpRuntime>,
    connection: Arc<AcpConnection>,
    outbound_rx: tokio::sync::mpsc::Receiver<ConnectionOutbound>,
    transport: Lines<S, St>,
    shutdown: F,
) -> Result<(), agent_client_protocol::Error>
where
    S: futures::Sink<String, Error = std::io::Error> + Send + 'static,
    St: futures::Stream<Item = std::io::Result<String>> + Send + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    // Spawn the notification drain task.
    let drain_conn = connection.sdk_client_slot();
    let mut rx = outbound_rx;
    tokio::spawn(async move {
        while let Some(outbound) = rx.recv().await {
            let guard = drain_conn.read().await;
            if let Some(conn) = guard.as_ref() {
                match outbound {
                    ConnectionOutbound::ExtensionNotification { method, params } => {
                        let message = agent_client_protocol::UntypedMessage {
                            method: method.clone(),
                            params,
                        };
                        if let Err(e) = conn.send_notification(message) {
                            tracing::debug!(error = ?e, method = %method, "Failed to send extension notification");
                        }
                    }
                    ConnectionOutbound::Notification { value, enqueued } => {
                        if let Err(e) = conn.send_notification(value) {
                            tracing::error!(error = ?e, "Failed to send session notification");
                        } else if let Some(enqueued) = enqueued {
                            let _ = enqueued.send(());
                        }
                    }
                    ConnectionOutbound::GlobalNotification { method, params } => {
                        let message = agent_client_protocol::UntypedMessage {
                            method: method.clone(),
                            params,
                        };
                        if let Err(e) = conn.send_notification(message) {
                            tracing::debug!(error = ?e, method = %method, "Failed to send global notification");
                        }
                    }
                    ConnectionOutbound::Barrier(enqueued) => {
                        let _ = enqueued.send(());
                    }
                }
            } else {
                tracing::trace!("Session notification dropped (connection not initialized)");
            }
        }
        tracing::info!("Session notification channel closed");
    });

    let a_init = runtime.agent.clone();
    let a_auth = runtime.agent.clone();
    let r_new = runtime.clone();
    let r_prompt = runtime.clone();
    let r_fork = runtime.clone();
    let r_load = runtime.clone();
    let r_resume = runtime.clone();
    let r_close = runtime.clone();
    let r_delete = runtime.clone();
    let a_list = runtime.agent.clone();
    let r_config = runtime.clone();
    let r_mode = runtime.clone();
    let r_cancel = runtime.clone();
    let conn_for_init = connection.clone();
    let conn_for_new = connection.clone();
    let conn_for_prompt = connection.clone();
    let conn_for_fork = connection.clone();
    let conn_for_load = connection.clone();
    let conn_for_resume = connection.clone();
    let conn_for_close = connection.clone();
    let conn_for_delete = connection.clone();
    let conn_for_list = connection.clone();
    let conn_for_config = connection.clone();
    let conn_for_mode = connection.clone();
    let conn_for_cancel = connection.clone();
    let r_ext = runtime.clone();
    let conn_for_ext = connection.clone();

    let result = Agent.builder()
        .on_receive_request(
            move |req: InitializeRequest,
                  responder: Responder<InitializeResponse>,
                  conn: ConnectionTo<Client>| {
                let agent = a_init.clone();
                let conn_for_init = conn_for_init.clone();
                async move {
                    let caps = ClientCapabilitiesInfo::from_json(
                        serde_json::to_value(&req.client_capabilities).ok(),
                    );
                    let result = agent.initialize(req).await;
                    if result.is_ok() {
                        if let Err(error) = conn_for_init.bind_client(conn, caps).await {
                            let _ = responder.respond_with_result(Err(
                                agent_client_protocol::Error::invalid_request()
                                    .data(error.to_string()),
                            ));
                            return Ok(());
                        }
                    }
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: AuthenticateRequest,
                  responder: Responder<AuthenticateResponse>,
                  _conn: ConnectionTo<Client>| {
                let agent = a_auth.clone();
                async move {
                    let result = agent.authenticate(req).await;
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: NewSessionRequest,
                  responder: Responder<NewSessionResponse>,
                  _conn: ConnectionTo<Client>| {
                let runtime = r_new.clone();
                let connection = conn_for_new.clone();
                async move {
                    if !connection.is_initialized() {
                        let _ = responder.respond_with_result(Err(
                            agent_client_protocol::Error::invalid_request()
                                .data("initialize must complete before session/new"),
                        ));
                        return Ok(());
                    }
                    let result = runtime
                        .agent
                        .new_session_for_owner(req, &connection.principal)
                        .await;
                    if let Ok(response) = &result {
                        let session_id = AnureoSessionId::new(response.session_id.to_string());
                        runtime
                            .bindings
                            .bind_new_session(session_id.clone(), connection.id.clone());
                        connection.note_session(session_id.as_str()).await;
                        if let Ok(Some(record)) = runtime
                            .agent
                            .session_index_record_for_owner(&connection.principal, session_id.as_str())
                            .await
                        {
                            runtime.global_bus.publish(
                                "session",
                                "session.created",
                                serde_json::json!({ "info": to_event_info_from_index(&record) }),
                            );

                            // A child create can also change the visible
                            // tree projection of each active ancestor. The
                            // session/new response carries those canonical
                            // records in nearest-ancestor-first order; emit
                            // them as separate updated events so clients that
                            // miss the response still converge through the
                            // global session stream.
                            if let Ok(response_json) = serde_json::to_value(response) {
                                if let Some(ancestors) = response_json
                                    .get("_meta")
                                    .and_then(|meta| meta.get("anureo.dev"))
                                    .and_then(|meta| meta.get("affectedSessions"))
                                    .and_then(serde_json::Value::as_array)
                                {
                                    for ancestor in ancestors {
                                        let Some(ancestor_id) = ancestor
                                            .get("sessionId")
                                            .and_then(serde_json::Value::as_str)
                                        else {
                                            continue;
                                        };
                                        if let Ok(Some(ancestor_record)) = runtime
                                            .agent
                                            .session_index_record_for_owner(
                                                &connection.principal,
                                                ancestor_id,
                                            )
                                            .await
                                        {
                                            runtime.global_bus.publish(
                                                "session",
                                                "session.updated",
                                                serde_json::json!({ "info": to_event_info_from_index(&ancestor_record) }),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: PromptRequest,
                  responder: Responder<PromptResponse>,
                  _conn: ConnectionTo<Client>| {
                let runtime = r_prompt.clone();
                let connection = conn_for_prompt.clone();
                // A prompt belongs to the server runtime, not to the transient
                // JSON-RPC connection. Detach it from the SDK connection task
                // set so a WebSocket close only drops the old response path;
                // the run keeps producing checkpointed/session-update events and
                // a replacement connection can attach immediately.
                tokio::spawn(async move {
                    let session_id = AnureoSessionId::new(req.session_id.to_string());
                    let result = if !runtime.bindings.is_connection_bound_to_session(&session_id, &connection.id) {
                        Err(agent_client_protocol::Error::new(
                            -32011,
                            "session is not bound to this connection",
                        ))
                    } else {
                        match connection.require_capabilities().await {
                            Ok(capabilities) => {
                                let bridge = Arc::new(
                                    crate::tools::AcpClientBridge::new(
                                        session_id.to_string(),
                                        runtime.connections.clone(),
                                        runtime.bindings.clone(),
                                    )
                                    .with_question_handler(
                                        runtime.question_handler.clone(),
                                    ),
                                );
                                match runtime.execute_prompt(req, capabilities, bridge).await {
                                    Ok(response) => {
                                        match runtime.flush_notifications(&session_id).await {
                                            Ok(()) => Ok(response),
                                            Err(error) => Err(
                                                agent_client_protocol::Error::internal_error()
                                                    .data(format!(
                                                        "failed to flush prompt updates: {error}"
                                                    )),
                                            ),
                                        }
                                    }
                                    Err(error) => Err(error),
                                }
                            }
                            Err(error) => Err(
                                agent_client_protocol::Error::invalid_request()
                                    .data(error.to_string()),
                            ),
                        }
                    };
                    let _ = responder.respond_with_result(result);
                });
                async { Ok(()) }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: ForkSessionRequest,
                  responder: Responder<ForkSessionResponse>,
                  _conn: ConnectionTo<Client>| {
                let runtime = r_fork.clone();
                let connection = conn_for_fork.clone();
                async move {
                    let source_id = AnureoSessionId::new(req.session_id.to_string());
                    let result = if !runtime.bindings.is_connection_bound_to_session(&source_id, &connection.id) {
                        Err(agent_client_protocol::Error::new(
                            -32011,
                            "session is not bound to this connection",
                        ))
                    } else {
                        runtime.agent.fork_session(req).await
                    };
                    if let Ok(response) = &result {
                        let session_id = AnureoSessionId::new(response.session_id.to_string());
                        runtime
                            .bindings
                            .bind_new_session(session_id.clone(), connection.id.clone());
                        connection.note_session(session_id.as_str()).await;
                    }
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: LoadSessionRequest,
                  responder: Responder<LoadSessionResponse>,
                  conn: ConnectionTo<Client>| {
                let runtime = r_load.clone();
                let connection = conn_for_load.clone();
                // Load/replay state transitions live in SessionLoadCoordinator;
                // this transport layer only owns task isolation and response delivery.
                let _ = conn.clone().spawn(async move {
                    let result = if !connection.is_initialized() {
                        Err(agent_client_protocol::Error::invalid_request()
                            .data("initialize must complete before session/load"))
                    } else {
                        crate::session_load::load_session(runtime, connection, req).await
                    };
                    let _ = responder.respond_with_result(result);
                    Ok(())
                });
                async { Ok(()) }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: ResumeSessionRequest,
                  responder: Responder<ResumeSessionResponse>,
                  _conn: ConnectionTo<Client>| {
                let runtime = r_resume.clone();
                let connection = conn_for_resume.clone();
                async move {
                    if !connection.is_initialized() {
                        let _ = responder.respond_with_result(Err(
                            agent_client_protocol::Error::invalid_request()
                                .data("initialize must complete before session/resume"),
                        ));
                        return Ok(());
                    }
                    let session_id = AnureoSessionId::new(req.session_id.to_string());
                    let previous_lifecycle =
                        match runtime.agent.sessions().begin_restore(&session_id) {
                            Ok(lifecycle) => lifecycle,
                            Err(()) => {
                                let _ = responder.respond_with_result(Err(
                                    agent_client_protocol::Error::new(
                                        -32010,
                                        "a prompt is already in progress for this session",
                                    ),
                                ));
                                return Ok(());
                            }
                        };
                    runtime.bindings.add_connection_to_session(&session_id, connection.id.clone());
                    let result = runtime
                        .agent
                        .resume_session_for_owner(req, &connection.principal)
                        .await;
                    if result.is_ok() {
                        runtime.record_session_rebind();
                    }
                    if result.is_err() {
                        runtime
                            .agent
                            .sessions()
                            .restore_lifecycle(&session_id, previous_lifecycle);
                        runtime.bindings.remove_connection_from_session(&session_id, &connection.id);
                    }
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: CloseSessionRequest,
                  responder: Responder<CloseSessionResponse>,
                  _conn: ConnectionTo<Client>| {
                let runtime = r_close.clone();
                let connection = conn_for_close.clone();
                async move {
                    let session_id = AnureoSessionId::new(req.session_id.to_string());
                    let result = if !runtime.bindings.is_connection_bound_to_session(&session_id, &connection.id) {
                        Err(agent_client_protocol::Error::new(
                            -32011,
                            "session is not bound to this connection",
                        ))
                    } else {
                        runtime.agent.close_session_for_owner(req, &connection.principal).await
                    };
                    if result.is_ok() {
                        runtime.bindings.unbind_session(&session_id);
                        runtime.cleanup_session_resources(session_id.as_str()).await;
                        if let Ok(Some(tombstone)) = runtime
                            .agent
                            .session_tombstone_for_owner(&connection.principal, session_id.as_str())
                            .await
                        {
                            runtime.global_bus.publish(
                                "session",
                                "session.deleted",
                                serde_json::json!({
                                    "info": to_tombstone_event_info(&tombstone),
                                    "sessionID": tombstone.session_id,
                                }),
                            );
                        }
                    }
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: DeleteSessionRequest,
                  responder: Responder<DeleteSessionResponse>,
                  _conn: ConnectionTo<Client>| {
                let runtime = r_delete.clone();
                let connection = conn_for_delete.clone();
                async move {
                    let session_id = AnureoSessionId::new(req.session_id.to_string());
                    let was_bound = runtime
                        .bindings
                        .is_connection_bound_to_session(&session_id, &connection.id);
                    let result = if was_bound {
                        runtime.agent.delete_session_for_owner(req, &connection.principal).await
                    } else {
                        // A retry after the first successful delete has
                        // already unbound the session. Permit it to reach the
                        // durable tombstone path so retries remain idempotent;
                        // unrelated unbound deletes still fail closed.
                        match runtime
                            .agent
                            .session_tombstone_for_owner(&connection.principal, session_id.as_str())
                            .await
                        {
                            Ok(Some(_)) => runtime.agent.delete_session_for_owner(req, &connection.principal).await,
                            Ok(None) => Err(agent_client_protocol::Error::new(
                                -32011,
                                "session is not bound to this connection",
                            )),
                            Err(error) => Err(error),
                        }
                    };
                    if result.is_ok() {
                        runtime.bindings.unbind_session(&session_id);
                        runtime.cleanup_session_resources(session_id.as_str()).await;
                        // A successful first delete owns the global tombstone
                        // event. Retries after the binding was removed remain
                        // idempotent but must not emit a duplicate event.
                        if was_bound {
                            if let Ok(Some(tombstone)) = runtime
                                .agent
                                .session_tombstone_for_owner(
                                    &connection.principal,
                                    session_id.as_str(),
                                )
                                .await
                            {
                                runtime.global_bus.publish(
                                    "session",
                                    "session.deleted",
                                    serde_json::json!({
                                        "info": to_tombstone_event_info(&tombstone),
                                        "sessionID": tombstone.session_id,
                                        "tombstone": tombstone_event_payload(&tombstone),
                                    }),
                                );
                                if let Ok(response) = &result {
                                    if let Ok(response_json) = serde_json::to_value(response) {
                                    if let Some(ancestors) = response_json
                                        .get("_meta")
                                        .and_then(|meta| meta.get("anureo.dev"))
                                        .and_then(|meta| meta.get("affectedSessions"))
                                        .and_then(serde_json::Value::as_array)
                                    {
                                        for ancestor in ancestors {
                                            if let Some(info) = to_event_info_from_wire(ancestor) {
                                                runtime.global_bus.publish(
                                                    "session",
                                                    "session.updated",
                                                    serde_json::json!({ "info": info }),
                                                );
                                            }
                                        }
                                    }
                                    }
                                }
                            }
                        }
                    }
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: ListSessionsRequest,
                  responder: Responder<ListSessionsResponse>,
                  _conn: ConnectionTo<Client>| {
                let agent = a_list.clone();
                let connection = conn_for_list.clone();
                async move {
                    let result = agent
                        .list_sessions_for_owner(req, &connection.principal)
                        .await;
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: SetSessionConfigOptionRequest,
                  responder: Responder<SetSessionConfigOptionResponse>,
                  _conn: ConnectionTo<Client>| {
                let runtime = r_config.clone();
                let connection = conn_for_config.clone();
                async move {
                    let session_id = AnureoSessionId::new(req.session_id.to_string());
                    let result = if !runtime.bindings.is_connection_bound_to_session(&session_id, &connection.id) {
                        Err(agent_client_protocol::Error::new(-32011, "session is not bound to this connection"))
                    } else {
                        runtime.agent.set_session_config_option(req).await
                    };
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_request(
            move |req: SetSessionModeRequest,
                  responder: Responder<SetSessionModeResponse>,
                  _conn: ConnectionTo<Client>| {
                let runtime = r_mode.clone();
                let connection = conn_for_mode.clone();
                async move {
                    let session_id = AnureoSessionId::new(req.session_id.to_string());
                    let result = if !runtime.bindings.is_connection_bound_to_session(&session_id, &connection.id) {
                        Err(agent_client_protocol::Error::new(-32011, "session is not bound to this connection"))
                    } else {
                        runtime.agent.set_session_mode(req).await
                    };
                    let _ = responder.respond_with_result(result);
                    Ok(())
                }
            },
            on_receive_request!(),
        )
        .on_receive_notification(
            move |notif: CancelNotification, _conn: ConnectionTo<Client>| {
                let runtime = r_cancel.clone();
                let connection = conn_for_cancel.clone();
                async move {
                    let session_id = AnureoSessionId::new(notif.session_id.to_string());
                    if runtime.bindings.is_connection_bound_to_session(&session_id, &connection.id) {
                        if let Err(e) = runtime.agent.cancel(notif).await {
                            tracing::error!(error = ?e, "cancel notification handler failed");
                        }
                    } else {
                        tracing::warn!(session_id = %session_id, "ignoring cancel from unbound connection");
                    }
                    Ok(())
                }
            },
            on_receive_notification!(),
        )
        .on_receive_request(
            move |req: UntypedMessage,
                  responder: Responder<serde_json::Value>,
                  _conn: ConnectionTo<Client>| {
                let runtime = r_ext.clone();
                let connection = conn_for_ext.clone();
                async move {
                    // P8：`_session/goal`（中立 goal 控制方法）与 `_anureo.dev/*`
                    // 扩展方法走同一分发表（registry 别名路由）。
                    if !req.method.starts_with(crate::extensions::EXTENSION_PREFIX)
                        && req.method != crate::extensions::goal::NEUTRAL_GOAL_CONTROL_METHOD
                    {
                        return Ok(Handled::No {
                            message: (req, responder),
                            retry: false,
                        });
                    }
                    let method = req.method.clone();
                    let params = req.params.clone();
                    let ctx = extension_context_for(&runtime, &connection, &params).await;
                    match runtime.extensions.dispatch(&method, params, &ctx).await {
                        Ok(result) => {
                            let _ = responder.respond_with_result(Ok(result));
                        }
                        Err(err) => {
                            tracing::warn!(method = %method, error = %err, "Extension dispatch failed");
                            let error = agent_client_protocol::Error::new(
                                err.code,
                                err.message,
                            );
                            let error = if let Some(data) = err.data {
                                error.data(data)
                            } else {
                                error
                            };
                            let _ = responder.respond_with_result(Err(error));
                        }
                    }
                    Ok(Handled::Yes)
                }
            },
            on_receive_request!(),
        )
        .connect_with(transport, move |_conn: ConnectionTo<Client>| async move {
            shutdown.await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok(())
        })
        .await
        .map_err(|e| {
            let err_str = format!("{:?}", e);
            if is_connection_closed_error_str(&err_str) {
                tracing::info!("connect_to finished: connection closed");
            } else {
                tracing::error!(?e, "connect_to failed");
            }
            agent_client_protocol::Error::internal_error().data(err_str)
        });

    result?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn extension_context_falls_back_to_last_session_working_directory() {
        let runtime = AcpRuntime::new().expect("runtime");
        let opened = runtime.open_connection("owner-a".into());
        let tmp = tempfile::TempDir::new().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        let session = runtime
            .agent
            .sessions()
            .create_owned(Some(canonical.clone()), "owner-a");
        opened.connection.note_session(session.as_str()).await;

        let ctx = extension_context_for(
            &runtime,
            &opened.connection,
            &serde_json::json!({"path": "."}),
        )
        .await;
        assert_eq!(ctx.working_directory.as_deref(), Some(canonical.as_path()));
        assert_eq!(ctx.session_id.as_deref(), Some(session.as_str()));
    }

    #[tokio::test]
    async fn extension_context_param_cwd_takes_precedence_over_session() {
        let runtime = AcpRuntime::new().expect("runtime");
        let opened = runtime.open_connection("owner-a".into());
        let session_tmp = tempfile::TempDir::new().unwrap();
        let session = runtime
            .agent
            .sessions()
            .create_owned(Some(session_tmp.path().to_path_buf()), "owner-a");
        opened.connection.note_session(session.as_str()).await;

        let param_tmp = tempfile::TempDir::new().unwrap();
        let param_canonical = std::fs::canonicalize(param_tmp.path()).unwrap();
        let ctx = extension_context_for(
            &runtime,
            &opened.connection,
            &serde_json::json!({"cwd": param_tmp.path().to_string_lossy()}),
        )
        .await;
        assert_eq!(
            ctx.working_directory.as_deref(),
            Some(param_canonical.as_path())
        );
    }

    #[tokio::test]
    async fn extension_context_session_param_resolves_working_directory() {
        let runtime = AcpRuntime::new().expect("runtime");
        let opened = runtime.open_connection("owner-a".into());
        let tmp = tempfile::TempDir::new().unwrap();
        let canonical = std::fs::canonicalize(tmp.path()).unwrap();
        let session = runtime
            .agent
            .sessions()
            .create_owned(Some(canonical.clone()), "owner-a");
        let ctx = extension_context_for(
            &runtime,
            &opened.connection,
            &serde_json::json!({"sessionId": session.as_str()}),
        )
        .await;
        assert_eq!(ctx.working_directory.as_deref(), Some(canonical.as_path()));
    }

    #[tokio::test]
    async fn extension_context_without_session_or_param_has_no_directory() {
        let runtime = AcpRuntime::new().expect("runtime");
        let opened = runtime.open_connection("owner-a".into());
        let ctx = extension_context_for(&runtime, &opened.connection, &serde_json::json!({})).await;
        assert!(ctx.working_directory.is_none());
    }
}

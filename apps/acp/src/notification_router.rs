//! Route session notifications to the transport currently bound to a session.

use std::sync::Arc;

use agent_client_protocol::schema::v1::SessionNotification;
use tokio::sync::oneshot;

use crate::connection::{ConnectionOutbound, ConnectionStateError};
use crate::connection_registry::ConnectionRegistry;
use crate::session::SessionId;
use crate::session_bindings::SessionBindings;

#[derive(Debug, thiserror::Error)]
pub enum NotificationRouteError {
    #[error("session is not bound to a connection: {0}")]
    Unbound(SessionId),
    #[error("bound connection is not active: {0}")]
    ConnectionUnavailable(String),
    #[error("connection outbound queue is closed")]
    QueueClosed,
    #[error("connection outbound queue is full: {0}")]
    QueueFull(String),
    #[error("notification flush acknowledgement was dropped")]
    FlushDropped,
    #[error(transparent)]
    ConnectionState(#[from] ConnectionStateError),
}

#[derive(Debug)]
pub struct NotificationRouter {
    bindings: Arc<SessionBindings>,
    connections: Arc<ConnectionRegistry>,
}

impl NotificationRouter {
    pub fn new(bindings: Arc<SessionBindings>, connections: Arc<ConnectionRegistry>) -> Self {
        Self {
            bindings,
            connections,
        }
    }

    pub async fn send(
        &self,
        notification: SessionNotification,
    ) -> Result<(), NotificationRouteError> {
        self.route(notification, None, false).await
    }

    /// Route a batch and wait until the final notification has been accepted
    /// by the SDK connection. Callers may then enqueue the JSON-RPC response.
    pub async fn send_and_flush(
        &self,
        notifications: impl IntoIterator<Item = SessionNotification>,
    ) -> Result<(), NotificationRouteError> {
        let mut values = notifications.into_iter().peekable();
        let mut final_ack = None;
        while let Some(notification) = values.next() {
            if values.peek().is_some() {
                self.route(notification, None, true).await?;
            } else {
                let (ack_tx, ack_rx) = oneshot::channel();
                self.route(notification, Some(ack_tx), true).await?;
                final_ack = Some(ack_rx);
            }
        }
        if let Some(ack) = final_ack {
            ack.await
                .map_err(|_| NotificationRouteError::FlushDropped)?;
        }
        Ok(())
    }

    /// Wait until all outbound items previously routed for this session have
    /// passed through the connection drain.
    pub async fn flush_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(), NotificationRouteError> {
        let connection_ids = self.bindings.connections_for(session_id);

        if connection_ids.is_empty() {
            return Err(NotificationRouteError::Unbound(session_id.clone()));
        }

        let mut last_error = None;
        let mut success_count = 0;

        for connection_id in connection_ids {
            let connection = self
                .connections
                .get(&connection_id)
                .filter(|connection| connection.is_active());

            if let Some(connection) = connection {
                let (ack_tx, ack_rx) = oneshot::channel();
                match connection
                    .outbound_tx
                    .send(ConnectionOutbound::Barrier(ack_tx))
                    .await
                {
                    Ok(()) => {
                        if ack_rx
                            .await
                            .map_err(|_| NotificationRouteError::FlushDropped)
                            .is_ok()
                        {
                            success_count += 1;
                        } else {
                            last_error = Some(Err(NotificationRouteError::FlushDropped));
                        }
                    }
                    Err(_) => {
                        last_error = Some(Err(NotificationRouteError::QueueClosed));
                    }
                }
            } else {
                last_error = Some(Err(NotificationRouteError::ConnectionUnavailable(
                    connection_id.clone(),
                )));
            }
        }

        if success_count > 0 {
            Ok(())
        } else {
            last_error.unwrap_or(Err(NotificationRouteError::Unbound(session_id.clone())))
        }
    }

    async fn route(
        &self,
        notification: SessionNotification,
        mut enqueued: Option<oneshot::Sender<()>>,
        wait_for_capacity: bool,
    ) -> Result<(), NotificationRouteError> {
        let session_id = SessionId::new(notification.session_id.to_string());
        let connection_ids = self.bindings.connections_for(&session_id);

        if connection_ids.is_empty() {
            return Err(NotificationRouteError::Unbound(session_id.clone()));
        }

        let mut last_error = None;
        let mut success_count = 0;

        for (index, connection_id) in connection_ids.iter().enumerate() {
            let connection = self
                .connections
                .get(connection_id)
                .filter(|connection| connection.is_active());

            if let Some(connection) = connection {
                if !connection.is_initialized() {
                    last_error = Some(Err(NotificationRouteError::ConnectionState(
                        ConnectionStateError::NotInitialized,
                    )));
                    continue;
                }

                // Create notification clone for each connection
                let notification_clone = notification.clone();
                let enqueued_for_connection =
                    if index == connection_ids.len() - 1 && enqueued.is_some() {
                        enqueued.take() // Take the sender for the last connection only
                    } else {
                        None
                    };

                let outbound = ConnectionOutbound::Notification {
                    value: notification_clone,
                    enqueued: enqueued_for_connection,
                };
                let sent = if wait_for_capacity {
                    connection
                        .outbound_tx
                        .send(outbound)
                        .await
                        .map_err(|_| NotificationRouteError::QueueClosed)
                } else {
                    try_send_live(&connection.outbound_tx, outbound, connection_id)
                };
                match sent {
                    Ok(()) => success_count += 1,
                    Err(error) => {
                        last_error = Some(Err(error));
                    }
                }
            } else {
                last_error = Some(Err(NotificationRouteError::ConnectionUnavailable(
                    connection_id.clone(),
                )));
            }
        }

        if success_count > 0 {
            Ok(())
        } else {
            last_error.unwrap_or(Err(NotificationRouteError::Unbound(session_id.clone())))
        }
    }

    /// Route a batched history replay as ONE custom notification
    /// (`_anureo.dev/session-history/batch`) to the connections bound to
    /// the session, replacing N `session/update` frames for a `session/load`
    /// tail replay.
    pub async fn send_history_batch(
        &self,
        session_id: &agent_client_protocol::schema::v1::SessionId,
        updates: Vec<agent_client_protocol::schema::v1::SessionUpdate>,
    ) -> Result<(), NotificationRouteError> {
        let session_id = SessionId::new(session_id.to_string());
        let connection_ids = self.bindings.connections_for(&session_id);

        if connection_ids.is_empty() {
            return Err(NotificationRouteError::Unbound(session_id.clone()));
        }

        let update_values: Vec<serde_json::Value> = updates
            .iter()
            .map(|update| {
                serde_json::to_value(update)
                    .map_err(|error| {
                        tracing::error!(
                            session_id = %session_id,
                            %error,
                            "Failed to serialize batched history update"
                        )
                    })
                    .unwrap_or(serde_json::Value::Null)
            })
            .collect();

        let mut last_error = None;
        let mut success_count = 0;

        for connection_id in connection_ids {
            let connection = self
                .connections
                .get(&connection_id)
                .filter(|connection| connection.is_active());

            if let Some(connection) = connection {
                if !connection.is_initialized() {
                    last_error = Some(Err(NotificationRouteError::ConnectionState(
                        ConnectionStateError::NotInitialized,
                    )));
                    continue;
                }

                match connection
                    .outbound_tx
                    .send(ConnectionOutbound::GlobalNotification {
                        method: crate::stream_bridge::HISTORY_BATCH_METHOD.to_string(),
                        params: serde_json::json!({
                            "sessionId": session_id.to_string(),
                            "updates": update_values.clone(),
                        }),
                    })
                    .await
                {
                    Ok(()) => success_count += 1,
                    Err(_) => {
                        last_error = Some(Err(NotificationRouteError::QueueClosed));
                    }
                }
            } else {
                last_error = Some(Err(NotificationRouteError::ConnectionUnavailable(
                    connection_id.clone(),
                )));
            }
        }

        if success_count > 0 {
            Ok(())
        } else {
            last_error.unwrap_or(Err(NotificationRouteError::Unbound(session_id.clone())))
        }
    }
}

fn try_send_live(
    sender: &tokio::sync::mpsc::Sender<ConnectionOutbound>,
    outbound: ConnectionOutbound,
    connection_id: &str,
) -> Result<(), NotificationRouteError> {
    sender.try_send(outbound).map_err(|error| match error {
        tokio::sync::mpsc::error::TrySendError::Full(_) => {
            NotificationRouteError::QueueFull(connection_id.to_string())
        }
        tokio::sync::mpsc::error::TrySendError::Closed(_) => NotificationRouteError::QueueClosed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{
        ContentBlock, ContentChunk, SessionId as AcpSessionId, SessionUpdate, TextContent,
    };

    fn notification(session_id: &str) -> SessionNotification {
        SessionNotification::new(
            AcpSessionId::new(session_id),
            SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                TextContent::new("update"),
            ))),
        )
    }

    #[test]
    fn full_live_queue_does_not_prevent_another_connection_enqueue() {
        let (slow_tx, _slow_rx) = tokio::sync::mpsc::channel(1);
        slow_tx
            .try_send(ConnectionOutbound::Notification {
                value: notification("session-a"),
                enqueued: None,
            })
            .unwrap();
        let slow = try_send_live(
            &slow_tx,
            ConnectionOutbound::Notification {
                value: notification("session-a"),
                enqueued: None,
            },
            "connection-a",
        );
        assert!(matches!(
            slow,
            Err(NotificationRouteError::QueueFull(ref id)) if id == "connection-a"
        ));

        let (healthy_tx, mut healthy_rx) = tokio::sync::mpsc::channel(1);
        try_send_live(
            &healthy_tx,
            ConnectionOutbound::Notification {
                value: notification("session-b"),
                enqueued: None,
            },
            "connection-b",
        )
        .unwrap();
        assert!(healthy_rx.try_recv().is_ok());
    }
}

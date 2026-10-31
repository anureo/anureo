//! Per-transport ACP connection state.
//!
//! A connection is created as a shell when the transport is accepted. The ACP
//! SDK only exposes [`ConnectionTo<Client>`] to request handlers, so the client
//! handle and capabilities are bound exactly once by `initialize`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v1::SessionNotification;
use agent_client_protocol::{Client, ConnectionTo};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, RwLock};

use crate::client_capabilities::ClientCapabilitiesInfo;

/// Unique connection identifier (UUID string).
pub type ConnectionId = String;

/// Messages drained by one transport connection.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum ConnectionOutbound {
    Notification {
        value: SessionNotification,
        /// Acknowledged after `send_notification` accepted the value.
        enqueued: Option<oneshot::Sender<()>>,
    },
    /// Cross-connection global event (`_anureo.dev/global/update`).
    GlobalNotification { method: String, params: Value },
    /// Agent-originated extension notification sent to this connection's client.
    ExtensionNotification { method: String, params: Value },
    /// FIFO barrier used to order session/load history before its response.
    Barrier(oneshot::Sender<()>),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConnectionStateError {
    #[error("ACP connection has not been initialized")]
    NotInitialized,
    #[error("ACP connection was already initialized")]
    AlreadyInitialized,
    #[error("ACP connection is closed")]
    Closed,
}

/// State owned by one WebSocket or stdio transport.
pub struct AcpConnection {
    pub id: ConnectionId,
    pub principal: String,
    sdk_client: Arc<RwLock<Option<ConnectionTo<Client>>>>,
    capabilities: RwLock<Option<ClientCapabilitiesInfo>>,
    /// Most recent session this connection created or loaded; extension
    /// authorization (e.g. `_anureo.dev/project/create`) requires a
    /// session-scoped principal even for connection-level calls.
    last_session_id: RwLock<Option<String>>,
    pub outbound_tx: mpsc::Sender<ConnectionOutbound>,
    active: AtomicBool,
    initialized: AtomicBool,
}

impl std::fmt::Debug for AcpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpConnection")
            .field("id", &self.id)
            .field("principal", &self.principal)
            .field("active", &self.is_active())
            .field("initialized", &self.is_initialized())
            .finish()
    }
}

impl AcpConnection {
    /// Create a transport shell. No reverse RPC may be issued before
    /// [`Self::bind_client`] succeeds.
    pub fn shell(
        id: ConnectionId,
        principal: String,
        outbound_tx: mpsc::Sender<ConnectionOutbound>,
    ) -> Self {
        Self {
            id,
            principal,
            sdk_client: Arc::new(RwLock::new(None)),
            capabilities: RwLock::new(None),
            last_session_id: RwLock::new(None),
            outbound_tx,
            active: AtomicBool::new(true),
            initialized: AtomicBool::new(false),
        }
    }

    /// Record the most recent session created/loaded on this connection so
    /// connection-scoped extension calls can authorize with session context.
    pub async fn note_session(&self, session_id: &str) {
        *self.last_session_id.write().await = Some(session_id.to_string());
    }

    pub async fn last_session_id(&self) -> Option<String> {
        self.last_session_id.read().await.clone()
    }

    /// Bind the SDK client and capabilities exactly once.
    pub async fn bind_client(
        &self,
        client: ConnectionTo<Client>,
        capabilities: ClientCapabilitiesInfo,
    ) -> Result<(), ConnectionStateError> {
        if !self.is_active() {
            return Err(ConnectionStateError::Closed);
        }
        let mut slot = self.sdk_client.write().await;
        if slot.is_some() || self.initialized.load(Ordering::Acquire) {
            return Err(ConnectionStateError::AlreadyInitialized);
        }
        *self.capabilities.write().await = Some(capabilities);
        *slot = Some(client);
        self.initialized.store(true, Ordering::Release);
        Ok(())
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub fn is_initialized(&self) -> bool {
        self.initialized.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn mark_initialized_for_test(&self) {
        self.initialized.store(true, Ordering::Release);
    }

    pub async fn require_capabilities(
        &self,
    ) -> Result<ClientCapabilitiesInfo, ConnectionStateError> {
        if !self.is_active() {
            return Err(ConnectionStateError::Closed);
        }
        self.capabilities
            .read()
            .await
            .clone()
            .ok_or(ConnectionStateError::NotInitialized)
    }

    /// Shared late-bound SDK slot used by a session-scoped client bridge.
    pub fn sdk_client_slot(&self) -> Arc<RwLock<Option<ConnectionTo<Client>>>> {
        self.sdk_client.clone()
    }

    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn shell_rejects_capability_access_before_initialize() {
        let (tx, _rx) = mpsc::channel(1);
        let connection = AcpConnection::shell("connection-a".into(), "owner-a".into(), tx);
        assert!(connection.is_active());
        assert!(!connection.is_initialized());
        assert_eq!(
            connection.require_capabilities().await.unwrap_err(),
            ConnectionStateError::NotInitialized
        );
    }

    #[tokio::test]
    async fn deactivated_shell_reports_closed() {
        let (tx, _rx) = mpsc::channel(1);
        let connection = AcpConnection::shell("connection-a".into(), "owner-a".into(), tx);
        connection.deactivate();
        assert_eq!(
            connection.require_capabilities().await.unwrap_err(),
            ConnectionStateError::Closed
        );
    }
}

//! Registry of active ACP transports.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::connection::{AcpConnection, ConnectionId};

#[derive(Debug, Default)]
pub struct ConnectionRegistry {
    connections: RwLock<HashMap<ConnectionId, Arc<AcpConnection>>>,
}

impl ConnectionRegistry {
    pub fn insert(&self, connection: Arc<AcpConnection>) {
        self.connections
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(connection.id.clone(), connection);
    }

    pub fn get(&self, connection_id: &str) -> Option<Arc<AcpConnection>> {
        self.connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(connection_id)
            .cloned()
    }

    pub fn remove(&self, connection_id: &str) -> Option<Arc<AcpConnection>> {
        self.connections
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(connection_id)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn len(&self) -> usize {
        self.connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Best-effort broadcast of an extension notification to every live
    /// connection (P5b `_anureo.dev/goal/changed|updated` push).
    ///
    /// Fire-and-forget: closed/full channels are skipped, errors are logged
    /// at debug level — a notification must never fail the mutation that
    /// produced it.
    pub async fn broadcast_extension_notification(&self, method: &str, params: serde_json::Value) {
        let targets: Vec<Arc<AcpConnection>> = self
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        for connection in targets {
            let outbound = crate::connection::ConnectionOutbound::ExtensionNotification {
                method: method.to_string(),
                params: params.clone(),
            };
            if let Err(e) = connection.outbound_tx.send(outbound).await {
                tracing::debug!(
                    connection_id = %connection.id,
                    method,
                    error = ?e,
                    "extension notification broadcast skipped"
                );
            }
        }
    }

    /// Deliver a session-scoped extension update only to transports owned by
    /// the same principal. Review metadata must not cross owner boundaries.
    pub fn notify_principal(&self, principal: &str, method: &str, params: serde_json::Value) {
        let targets: Vec<Arc<AcpConnection>> = self
            .connections
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|connection| {
                connection.principal == principal
                    && connection.is_active()
                    && connection.is_initialized()
            })
            .cloned()
            .collect();
        for connection in targets {
            let outbound = crate::connection::ConnectionOutbound::ExtensionNotification {
                method: method.to_string(),
                params: params.clone(),
            };
            if let Err(error) = connection.outbound_tx.try_send(outbound) {
                tracing::debug!(connection_id = %connection.id, method, error = ?error,
                    "owner notification delivery skipped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::ConnectionOutbound;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn notify_principal_does_not_send_to_other_owners() {
        let registry = ConnectionRegistry::default();
        let (owned_tx, mut owned_rx) = mpsc::channel(2);
        let (other_tx, mut other_rx) = mpsc::channel(2);
        let owned = Arc::new(AcpConnection::shell(
            "owned".into(),
            "alice".into(),
            owned_tx,
        ));
        owned.mark_initialized_for_test();
        registry.insert(owned);
        registry.insert(Arc::new(AcpConnection::shell(
            "other".into(),
            "bob".into(),
            other_tx,
        )));

        registry.notify_principal(
            "alice",
            "_anureo.dev/review/changed",
            serde_json::json!({"sessionId": "s"}),
        );
        assert!(matches!(
            owned_rx.try_recv(),
            Ok(ConnectionOutbound::ExtensionNotification { method, .. })
                if method == "_anureo.dev/review/changed"
        ));
        assert!(other_rx.try_recv().is_err());
    }

    #[test]
    fn notify_principal_skips_uninitialized_connection() {
        let registry = ConnectionRegistry::default();
        let (tx, mut rx) = mpsc::channel(1);
        registry.insert(Arc::new(AcpConnection::shell(
            "pending".into(),
            "alice".into(),
            tx,
        )));
        registry.notify_principal("alice", "_anureo.dev/review/changed", serde_json::json!({}));
        assert!(rx.try_recv().is_err());
    }
}

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
}

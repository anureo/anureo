# 02 — AcpClient 设计

> **Scope**: `AcpClient` 结构体的完整设计：WebSocket 连接、JSON-RPC id 关联、通知分发、reader loop  
> **File**: `apps/cli/src/server_transport/acp_client.rs`

## 设计概述

`AcpClient` 是一个**主动的 ACP 客户端**。它：

1. 建立 WebSocket 连接到 `ws://host:port/acp`
2. 运行后台 reader loop 持续接收消息
3. 对 JSON-RPC response 按 `id` 关联到对应的 `oneshot::Sender`
4. 对 `session/update` 通知通过 `mpsc::UnboundedSender` 推给 prompt 调用方

与 `ws_bridge` 的本质区别：`ws_bridge` 是**透明中继**（不理解 JSON-RPC），`AcpClient` 是**协议客户端**（理解 JSON-RPC 语义并主动编排请求）。

## 完整结构体

```rust
//! ACP WebSocket client.
//!
//! Sends JSON-RPC requests and receives streamed session updates over a
//! single WebSocket connection. Unlike `ws_bridge` (transparent relay),
//! this module understands ACP JSON-RPC semantics and orchestrates the
//! request/response/notification flow.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// WebSocket connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default response timeout for JSON-RPC requests (5 minutes).
/// `session/prompt` can take minutes for long agent runs.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(300);

/// Channel buffer for session updates.
const UPDATE_BUFFER: usize = 256;

/// Error type for ACP client operations.
#[derive(Debug, thiserror::Error)]
pub enum AcpClientError {
    #[error("WebSocket connect failed: {0}")]
    Connect(String),
    #[error("WebSocket send error: {0}")]
    Send(String),
    #[error("JSON-RPC error (code {code}): {message}")]
    JsonRpc { code: i64, message: String },
    #[error("Response timeout after {0:?}")]
    Timeout(Duration),
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("Serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("Protocol error: {0}")]
    Protocol(String),
}

pub type AcpResult<T> = std::result::Result<T, AcpClientError>;

/// ACP WebSocket client.
///
/// Owns a WebSocket connection and a background reader task. The reader
/// task routes incoming JSON-RPC responses to pending request waiters
/// (by id) and forwards `session/update` notifications to a channel.
pub struct AcpClient {
    /// WebSocket write half (guarded by the struct; only `send_request`
    /// writes to it).
    ws_tx: mpsc::UnboundedSender<String>,

    /// Pending request waiters: request id → oneshot sender.
    /// Stored in an `Arc<Mutex<>>` so the reader task can access it.
    pending: Arc<tokio::sync::Mutex<HashMap<i64, oneshot::Sender<AcpResult<Value>>>>>,

    /// Next JSON-RPC request id.
    next_id: AtomicI64,

    /// Handle to the background reader task.
    reader_handle: Option<JoinHandle<()>>,
}
```

## 连接逻辑

```rust
impl AcpClient {
    /// Connect to `ws://host:port/acp` and start the background reader.
    ///
    /// Returns `(client, update_rx)` — the receiver is used to consume
    /// `session/update` notifications. The caller holds it across prompt
    /// turns.
    pub async fn connect(
        url: &str,
    ) -> AcpResult<(
        Self,
        mpsc::UnboundedReceiver<AcpSessionUpdate>,
    )> {
        // Build the WS upgrade request with optional auth header.
        let request = build_ws_request(url)?;

        // Connect with timeout.
        let (ws, response) = tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| AcpClientError::Timeout(CONNECT_TIMEOUT))?
        .map_err(|e| AcpClientError::Connect(e.to_string()))?;

        tracing::info!(status = %response.status(), "ACP WebSocket connected");

        // Split into read/write halves.
        let (mut ws_sink, ws_stream) = ws.split();

        // Channel for writing WS messages (avoids async borrow conflicts).
        let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<String>();

        // Channel for session updates forwarded to caller.
        let (update_tx, update_rx) = mpsc::unbounded_channel::<AcpSessionUpdate>();

        // Shared pending table.
        let pending = Arc::new(tokio::sync::Mutex::new(
            HashMap::<i64, oneshot::Sender<AcpResult<Value>>>::new(),
        ));

        // Spawn WS writer task: forwards ws_tx messages to the WebSocket sink.
        tokio::spawn(async move {
            while let Some(text) = ws_rx.recv().await {
                if ws_sink.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            }
        });

        // Spawn reader task.
        let pending_clone = pending.clone();
        let update_tx_clone = update_tx.clone();
        let reader_handle = tokio::spawn(async move {
            Self::reader_loop(ws_stream, pending_clone, update_tx_clone).await;
        });

        Ok((
            Self {
                ws_tx,
                pending,
                next_id: AtomicI64::new(1),
                reader_handle: Some(reader_handle),
            },
            update_rx,
        ))
    }
}
```

## Reader Loop

The reader loop is the heart of the client. It distinguishes three message types:

```rust
impl AcpClient {
    /// Background reader: parses incoming WS text frames and routes them.
    async fn reader_loop(
        mut ws_stream: impl StreamExt<
            Item = std::result::Result<Message, tokio_tungstenite::tungstenite::Error>,
        > + Unpin,
        pending: Arc<tokio::sync::Mutex<HashMap<i64, oneshot::Sender<AcpResult<Value>>>>>,
        update_tx: mpsc::UnboundedSender<AcpSessionUpdate>,
    ) {
        while let Some(frame) = ws_stream.next().await {
            let text = match frame {
                Ok(Message::Text(t)) => t.to_string(),
                Ok(Message::Close(_)) => {
                    tracing::info!("ACP WebSocket closed by server");
                    break;
                }
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(Message::Binary(_)) => {
                    tracing::warn!("ignoring binary frame");
                    continue;
                }
                Ok(Message::Frame(_)) => continue,
                Err(e) => {
                    tracing::error!(error = %e, "ACP WS recv error");
                    break;
                }
            };

            // Parse as JSON.
            let value: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to parse ACP message as JSON");
                    continue;
                }
            };

            // Route based on message structure.
            if let Some(id) = value.get("id").and_then(|v| v.as_i64()) {
                // ── Response (has "id") ──────────────────────────
                Self::route_response(id, value, &pending).await;
            } else if let Some(method) = value.get("method").and_then(|v| v.as_str()) {
                // ── Notification (no "id", has "method") ─────────
                Self::route_notification(method, value, &update_tx);
            }
            // else: malformed — ignore
        }

        // Connection ended: fail all pending requests.
        tracing::info!("ACP reader loop exiting, failing pending requests");
        let mut guard = pending.lock().await;
        for (_id, sender) in guard.drain() {
            let _ = sender.send(Err(AcpClientError::ConnectionClosed));
        }
    }

    /// Route a JSON-RPC response to its pending waiter.
    async fn route_response(
        id: i64,
        value: Value,
        pending: &Arc<tokio::sync::Mutex<HashMap<i64, oneshot::Sender<AcpResult<Value>>>>>,
    ) {
        let sender = {
            let mut guard = pending.lock().await;
            guard.remove(&id)
        };

        match sender {
            Some(tx) => {
                if let Some(error) = value.get("error") {
                    let code = error
                        .get("code")
                        .and_then(|c| c.as_i64())
                        .unwrap_or(-1);
                    let message = error
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown error")
                        .to_string();
                    let _ = tx.send(Err(AcpClientError::JsonRpc { code, message }));
                } else {
                    let result = value.get("result").cloned().unwrap_or(Value::Null);
                    let _ = tx.send(Ok(result));
                }
            }
            None => {
                tracing::warn!(id, "received response with no pending request");
            }
        }
    }

    /// Route a notification by method name.
    fn route_notification(
        method: &str,
        value: Value,
        update_tx: &mpsc::UnboundedSender<AcpSessionUpdate>,
    ) {
        match method {
            "session/update" => {
                if let Some(update) = Self::parse_session_update(&value) {
                    if update_tx.send(update).is_err() {
                        tracing::debug!("session update channel closed");
                    }
                }
            }
            "session/request_permission" => {
                tracing::debug!("ignoring permission request");
            }
            _ => {
                tracing::trace!(method, "unhandled notification");
            }
        }
    }
}
```

## Request 发送（通用）

```rust
impl AcpClient {
    /// Send a JSON-RPC request and wait for the response.
    async fn send_request(
        &self,
        method: &str,
        params: Value,
    ) -> AcpResult<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let request_str = serde_json::to_string(&request)?;

        // Register a pending waiter.
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id, tx);
        }

        // Send via WS writer channel.
        self.ws_tx
            .send(request_str)
            .map_err(|_| AcpClientError::ConnectionClosed)?;

        // Wait for response (with timeout).
        match tokio::time::timeout(RESPONSE_TIMEOUT, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AcpClientError::ConnectionClosed),
            Err(_) => {
                // Remove from pending on timeout.
                let mut guard = self.pending.lock().await;
                guard.remove(&id);
                Err(AcpClientError::Timeout(RESPONSE_TIMEOUT))
            }
        }
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    fn send_notification(
        &self,
        method: &str,
        params: Value,
    ) -> AcpResult<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });

        let str = serde_json::to_string(&notification)?;
        self.ws_tx
            .send(str)
            .map_err(|_| AcpClientError::ConnectionClosed)
    }
}
```

## 高层方法

### `initialize()`

```rust
impl AcpClient {
    /// Send `initialize` and wait for the response.
    pub async fn initialize(&self) -> AcpResult<Value> {
        let params = serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
        });
        let result = self.send_request("initialize", params).await?;
        tracing::info!("ACP initialized");
        Ok(result)
    }
}
```

### `new_session()`

```rust
impl AcpClient {
    /// Send `session/new` and return the session id.
    pub async fn new_session(
        &self,
        cwd: &str,
        mode: Option<&str>,
    ) -> AcpResult<String> {
        let params = serde_json::json!({
            "cwd": cwd,
            "mcpServers": [],
            "mode": mode,
        });
        let result = self.send_request("session/new", params).await?;
        let session_id = result
            .get("sessionId")
            .and_then(|s| s.as_str())
            .ok_or_else(|| {
                AcpClientError::Protocol("missing sessionId in session/new response".into())
            })?
            .to_string();
        tracing::info!(session_id = %session_id, "ACP session created");
        Ok(session_id)
    }
}
```

### `prompt()`

`session/prompt` 是长请求——在收到最终 response 之前，服务端会持续发送 `session/update` 通知。

```rust
impl AcpClient {
    /// Send `session/prompt`.
    ///
    /// Returns the id of the prompt request. The caller should then
    /// drain `session/update` notifications from `update_rx` while
    /// waiting for the response via `await_prompt_response()`.
    pub async fn prompt(
        &self,
        session_id: &str,
        message: &str,
    ) -> AcpResult<i64> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let params = serde_json::json!({
            "sessionId": session_id,
            "prompt": [{
                "type": "content",
                "content": [{ "type": "text", "text": message }]
            }]
        });

        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": params,
        });

        // Register waiter BEFORE sending to avoid race.
        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.pending.lock().await;
            guard.insert(id, tx);
        }

        let request_str = serde_json::to_string(&request)?;
        self.ws_tx
            .send(request_str)
            .map_err(|_| AcpClientError::ConnectionClosed)?;

        // Store rx for the caller — we return the id so the caller
        // can use await_prompt_response(id).
        // NOTE: In practice, rx is stored in a local and the caller
        // uses select! to drain updates while awaiting rx.
        Ok(id)
    }

    /// Await the response for a previously initiated prompt request.
    ///
    /// The caller should have received `rx` from the internal pending
    /// table. In practice, `prompt()` returns the id and the caller
    /// uses `take_pending(id)` to get the receiver.
    pub async fn await_response(&self, id: i64) -> AcpResult<Value> {
        let rx = {
            let mut guard = self.pending.lock().await;
            guard.remove(&id).ok_or(AcpClientError::ConnectionClosed)?
        };

        match rx.await {
            Ok(result) => result,
            Err(_) => Err(AcpClientError::ConnectionClosed),
        }
    }
}
```

> **设计决策 — update channel 所有权**：
>
> `connect()` 返回 `(Self, UnboundedReceiver<AcpSessionUpdate>)`。调用方（`run_acp_mode`）持有 receiver，在每次 `prompt()` 调用前 drain 旧消息，在 prompt 执行期间消费新消息。
>
> 这避免了内部 `Mutex<Option<Sender>>` 的复杂性，同时天然支持交互式多轮对话。

### `cancel()`

```rust
impl AcpClient {
    /// Send `session/cancel` notification (no response expected).
    pub fn cancel(&self, session_id: &str) -> AcpResult<()> {
        self.send_notification("session/cancel", serde_json::json!({
            "sessionId": session_id,
        }))
    }
}
```

### `load_session()`

```rust
impl AcpClient {
    /// Send `session/load` to resume an existing session.
    pub async fn load_session(&self, session_id: &str) -> AcpResult<()> {
        let params = serde_json::json!({
            "sessionId": session_id,
        });
        let _result = self.send_request("session/load", params).await?;
        tracing::info!(session_id, "ACP session loaded");
        Ok(())
    }
}
```

### `shutdown()`

```rust
impl AcpClient {
    /// Graceful shutdown: close WS writer, wait for reader to exit.
    pub async fn shutdown(mut self) {
        // Close the WS writer channel (signals writer task to finish).
        drop(self.ws_tx);

        // Wait for reader to finish.
        if let Some(handle) = self.reader_handle.take() {
            let _ = handle.await;
        }
    }
}
```

## `AcpSessionUpdate` 枚举

```rust
/// A parsed session update notification, ready for display rendering.
#[derive(Debug, Clone, serde::Serialize)]
pub enum AcpSessionUpdate {
    /// Chunk of agent text output.
    AgentMessageChunk {
        text: String,
        message_id: Option<String>,
    },

    /// Chunk of agent reasoning/thinking.
    AgentThoughtChunk {
        text: String,
        message_id: Option<String>,
    },

    /// Tool call started.
    ToolCallStarted {
        tool_call_id: String,
        name: String,
        input: Option<Value>,
    },

    /// Tool call status/result update.
    ToolCallUpdated {
        tool_call_id: String,
        status: String, // "running" | "success" | "failure"
        output: Option<String>,
        raw_output: Option<String>,
    },

    /// File diff update.
    Diff {
        tool_call_id: String,
        path: String,
        old_text: Option<String>,
        new_text: String,
    },

    /// Token usage update.
    UsageUpdate {
        used: u64,
        size: u64,
    },

    /// Session info (title) update.
    SessionInfoUpdate {
        title: String,
    },

    /// Execution plan.
    Plan {
        entries: Vec<Value>,
    },

    /// Current mode changed.
    CurrentModeUpdate {
        mode: String,
    },
}
```

## `parse_session_update` 实现

```rust
impl AcpClient {
    fn parse_session_update(value: &Value) -> Option<AcpSessionUpdate> {
        let update = value.get("params")?.get("update")?;
        let kind = update.get("kind")?.as_str()?;

        match kind {
            "agent_message_chunk" => {
                let text = extract_text_from_content(update)?;
                Some(AcpSessionUpdate::AgentMessageChunk {
                    text,
                    message_id: update
                        .get("messageId")
                        .and_then(|m| m.as_str())
                        .map(String::from),
                })
            }
            "agent_thought_chunk" => {
                let text = extract_text_from_content(update)?;
                Some(AcpSessionUpdate::AgentThoughtChunk {
                    text,
                    message_id: update
                        .get("messageId")
                        .and_then(|m| m.as_str())
                        .map(String::from),
                })
            }
            "tool_call" => {
                let tool_call = update.get("toolCall")?;
                Some(AcpSessionUpdate::ToolCallStarted {
                    tool_call_id: tool_call.get("toolCallId")?.as_str()?.to_string(),
                    name: tool_call.get("name")?.as_str()?.to_string(),
                    input: tool_call.get("input").cloned(),
                })
            }
            "tool_call_update" => {
                let tcu = update.get("update")?;
                let tool_call_id = update.get("toolCallId")?.as_str()?.to_string();

                // Check for diff content
                if let Some(content) = tcu.get("content").and_then(|c| c.as_array()) {
                    for block in content {
                        if block.get("type").and_then(|t| t.as_str()) == Some("diff") {
                            return Some(AcpSessionUpdate::Diff {
                                tool_call_id: tool_call_id.clone(),
                                path: block.get("path")?.as_str()?.to_string(),
                                old_text: block
                                    .get("oldText")
                                    .and_then(|t| t.as_str())
                                    .map(String::from),
                                new_text: block.get("newText")?.as_str()?.to_string(),
                            });
                        }
                    }
                }

                Some(AcpSessionUpdate::ToolCallUpdated {
                    tool_call_id,
                    status: tcu
                        .get("status")
                        .and_then(|s| s.as_str())
                        .unwrap_or("running")
                        .to_string(),
                    output: tcu
                        .get("output")
                        .and_then(|o| o.as_str())
                        .map(String::from),
                    raw_output: tcu
                        .get("rawOutput")
                        .and_then(|o| o.as_str())
                        .map(String::from),
                })
            }
            "usage_update" => Some(AcpSessionUpdate::UsageUpdate {
                used: update.get("used")?.as_u64()?,
                size: update.get("size")?.as_u64()?,
            }),
            "session_info_update" => Some(AcpSessionUpdate::SessionInfoUpdate {
                title: update.get("title")?.as_str()?.to_string(),
            }),
            "plan" => {
                let entries = update
                    .get("entries")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default();
                Some(AcpSessionUpdate::Plan { entries })
            }
            "current_mode_update" => Some(AcpSessionUpdate::CurrentModeUpdate {
                mode: update.get("currentMode")?.as_str()?.to_string(),
            }),
            _ => {
                tracing::trace!(kind, "unhandled session update kind");
                None
            }
        }
    }
}

/// Extract the text string from an ACP content block.
fn extract_text_from_content(update: &Value) -> Option<String> {
    update
        .get("content")?
        .get("content")?
        .as_array()?
        .first()?
        .get("text")?
        .as_str()
        .map(String::from)
}
```

## `build_ws_request` 辅助函数

```rust
/// Build the WebSocket upgrade request with optional auth header.
fn build_ws_request(
    ws_url: &str,
) -> AcpResult<tokio_tungstenite::tungstenite::handshake::client::Request> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request = ws_url
        .into_client_request()
        .map_err(|e| AcpClientError::Connect(format!("invalid URL {ws_url}: {e}")))?;

    if let Ok(token) = std::env::var("ANUREO_AUTH_TOKEN") {
        let value = format!("Bearer {token}")
            .parse()
            .map_err(|e| AcpClientError::Connect(format!("invalid ANUREO_AUTH_TOKEN: {e}")))?;
        request.headers_mut().insert("Authorization", value);
    }

    Ok(request)
}
```

## 并发模型

```
                         ┌──────────────────────────┐
                         │       AcpClient           │
                         │                          │
  prompt() ───►          │  next_id: AtomicI64      │
                         │  pending: Mutex<HashMap> │
  send_request() ──►     │  ws_tx: UnboundedSender  │
                         └────────┬──────┬──────────┘
                                  │      │
                    ┌─────────────┘      └──────────────┐
                    │                                   │
                    ▼                                   ▼
         ┌──────────────────┐               ┌────────────────────┐
         │  WS Writer Task   │               │  Reader Task        │
         │                  │               │                    │
         │  rx.recv() ──►   │               │  ws_stream.next()   │
         │  ws_sink.send()  │               │  ┌─ has id?         │
         │                  │               │  │   └─► pending[id] │
         └──────────────────┘               │  └─ is notification? │
                                            │      └─► update_tx   │
                                            └────────────────────┘
```

## 错误处理策略

| 场景 | 行为 |
|------|------|
| WS 连接失败 | `connect()` 返回 `Connect` 错误 |
| 请求超时 (300s) | `send_request` 返回 `Timeout`，自动从 pending 表移除 |
| JSON-RPC error response | `send_request` 返回 `JsonRpc { code, message }` |
| WS 断开 | Reader loop 退出，所有 pending waiter 收到 `ConnectionClosed` |
| JSON 解析失败 | 记录 warn 日志，跳过该消息（不中断 reader loop） |

## 单元测试

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_agent_message_chunk() {
        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "test",
                "update": {
                    "kind": "agent_message_chunk",
                    "content": {
                        "type": "content",
                        "content": [{ "type": "text", "text": "hello" }]
                    },
                    "messageId": "msg-1"
                }
            }
        });
        let update = AcpClient::parse_session_update(&raw).unwrap();
        assert!(matches!(update, AcpSessionUpdate::AgentMessageChunk { ref text, .. } if text == "hello"));
    }

    #[test]
    fn test_parse_tool_call_started() {
        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "test",
                "update": {
                    "kind": "tool_call",
                    "toolCallId": "tc-1",
                    "toolCall": {
                        "type": "tool_call",
                        "toolCallId": "tc-1",
                        "name": "read_file",
                        "input": { "path": "test.rs" },
                        "status": "pending"
                    }
                }
            }
        });
        let update = AcpClient::parse_session_update(&raw).unwrap();
        assert!(matches!(update, AcpSessionUpdate::ToolCallStarted { ref name, .. } if name == "read_file"));
    }

    #[test]
    fn test_parse_usage_update() {
        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "test",
                "update": {
                    "kind": "usage_update",
                    "used": 100,
                    "size": 200000
                }
            }
        });
        let update = AcpClient::parse_session_update(&raw).unwrap();
        assert!(matches!(update, AcpSessionUpdate::UsageUpdate { used: 100, size: 200000 }));
    }

    #[test]
    fn test_parse_unknown_kind_returns_none() {
        let raw = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/update",
            "params": {
                "sessionId": "test",
                "update": { "kind": "future_unknown_kind" }
            }
        });
        assert!(AcpClient::parse_session_update(&raw).is_none());
    }
}
```

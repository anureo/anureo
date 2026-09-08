# 01 — ACP Wire Protocol

> **Scope**: ACP JSON-RPC 消息的完整 wire 格式定义  
> **Reference**: `agent_client_protocol` crate, `apps/acp/src/stdio_loop.rs`, `apps/acp/src/agent.rs`

## 传输层

ACP 使用 **JSON-RPC 2.0** over **WebSocket text frames**。

- 每个 WebSocket text frame 携带一条完整的 JSON-RPC 消息
- 服务端路径：`ws://host:port/acp`（见 `apps/server/src/routes.rs`）
- 最大 frame size: 1 MiB（`MAX_MESSAGE_BYTES`, `apps/server/src/handlers/acp.rs:23`）
- 可选认证：`Authorization: Bearer <ANUREO_AUTH_TOKEN>` header

## 消息类型

JSON-RPC 2.0 有三种消息类型：

```json
// Request (client → server)
{ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { ... } }

// Response (server → client)
{ "jsonrpc": "2.0", "id": 1, "result": { ... } }
// or
{ "jsonrpc": "2.0", "id": 1, "error": { "code": -32602, "message": "..." } }

// Notification (server → client, no id, no response expected)
{ "jsonrpc": "2.0", "method": "session/update", "params": { ... } }
```

## 协议交互序列

```
Client                                              Server
  │                                                    │
  │── initialize (id=1) ─────────────────────────────►│
  │◄── response (id=1): protocolVersion, agentInfo ────│
  │                                                    │
  │── session/new (id=2) ────────────────────────────►│
  │◄── response (id=2): sessionId, modes ──────────────│
  │                                                    │
  │── session/prompt (id=3) ─────────────────────────►│
  │◄── notification: session/update (agent_chunk) ─────│
  │◄── notification: session/update (tool_call) ───────│
  │◄── notification: session/update (tool_result) ─────│
  │◄── notification: session/update (agent_chunk) ─────│
  │◄── notification: session/update (usage) ───────────│
  │◄── response (id=3): stop, content ─────────────────│
  │                                                    │
  │── session/prompt (id=4) ─────────────────────────►│ (next turn)
  │   ...                                              │
```

---

## 1. `initialize`

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": 1,
    "clientCapabilities": {}
  }
}
```

**Rust 类型** (`agent_client_protocol::schema::v1`):

```rust
pub struct InitializeRequest {
    pub protocol_version: u32,           // = 1
    pub client_capabilities: ClientCapabilities,
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": 1,
    "agentInfo": { "name": "anureo", "version": "0.x.x" },
    "agentCapabilities": {
      "loadSession": true,
      "mcp": { "http": true, "sse": false },
      "promptCapabilities": { "image": true, "audio": true, "embeddedContext": true },
      "sessionCapabilities": { "list": {}, "resume": {} }
    }
  }
}
```

**Rust 类型**:

```rust
pub struct InitializeResponse {
    pub protocol_version: ProtocolVersion,  // ProtocolVersion::V1
    pub agent_info: Implementation,
    pub agent_capabilities: AgentCapabilities,
}
```

**服务端实现**: `apps/acp/src/agent.rs:354` — `anureoAcpAgent::initialize()`

---

## 2. `session/new`

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session/new",
  "params": {
    "cwd": "/path/to/working/directory",
    "mcpServers": [],
    "mode": "react"
  }
}
```

**Rust 类型**:

```rust
pub struct NewSessionRequest {
    pub cwd: String,
    #[serde(default)]
    pub mcp_servers: Vec<McpServerConfig>,
    pub mode: Option<SessionModeId>,
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `cwd` | `String` | Agent 工作目录（必须存在） |
| `mcpServers` | `Vec<McpServerConfig>` | MCP 服务器配置（可选，默认空） |
| `mode` | `Option<SessionModeId>` | 初始 agent mode（如 `"react"`, `"dup"`, `"tot"`） |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "sessionId": "a1b2c3d4",
    "modes": {
      "currentMode": "react",
      "modes": [
        { "name": "react", "namePretty": "React" }
      ]
    },
    "configOptions": {
      "currentMode": "react",
      "currentModel": "default",
      "models": [...],
      "modes": [...]
    }
  }
}
```

**Rust 类型**:

```rust
pub struct NewSessionResponse {
    pub session_id: SessionId,
    pub modes: SessionModeState,
    pub config_options: SessionConfigOptions,
}
```

**服务端实现**: `apps/acp/src/agent.rs:417` — `anureoAcpAgent::new_session()`

---

## 3. `session/prompt`

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "session/prompt",
  "params": {
    "sessionId": "a1b2c3d4",
    "prompt": [
      {
        "type": "content",
        "content": [
          { "type": "text", "text": "实现一个二分查找" }
        ]
      }
    ]
  }
}
```

**Rust 类型**:

```rust
pub struct PromptRequest {
    pub session_id: SessionId,
    pub prompt: Vec<PromptBlock>,
}
```

`PromptBlock` 是一个枚举，支持文本和 embedded context:

```rust
pub enum PromptBlock {
    Content { content: Vec<ContentBlock> },
    // 其他变体（resource、image 等）
}

pub enum ContentBlock {
    Text { text: String },
    Image { data: String, media_type: String },
    // ...
}
```

### Response

在请求和最终响应之间，服务端会发送多条 `session/update` 通知（见下一节）。

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "stop": {
      "reason": "end_turn"
    },
    "content": [
      { "type": "text", "text": "这是最终回复文本..." }
    ]
  }
}
```

**Rust 类型**:

```rust
pub struct PromptResponse {
    pub stop: StopReason,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

pub enum StopReason {
    EndTurn,
    Refusal,
    MaxTokens,
    // ...
}
```

**服务端实现**: `apps/acp/src/agent.rs:744` — `anureoAcpAgent::prompt()`

> **注意**：`prompt` 是一个**长请求**——服务端在收到 prompt 后会启动 ReAct graph 并持续发送 `session/update` 通知，直到完成后才返回 `PromptResponse`。客户端必须同时处理流式通知和最终响应。

---

## 4. `session/update` (Notification)

这是最核心的流式通知。服务端在 prompt 执行期间持续发送。

```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "a1b2c3d4",
    "update": { ... }
  }
}
```

`update` 字段是 `SessionUpdate` 枚举，有以下变体（见 `apps/acp/src/stream_bridge.rs:58-136`）：

### 4.1 AgentMessageChunk — Agent 文本输出

```json
{
  "update": {
    "kind": "agent_message_chunk",
    "content": { "type": "content", "content": [{ "type": "text", "text": "chunk text" }] },
    "messageId": "msg-001"
  }
}
```

### 4.2 AgentThoughtChunk — Agent 推理/思维链

```json
{
  "update": {
    "kind": "agent_thought_chunk",
    "content": { "type": "content", "content": [{ "type": "text", "text": "thinking..." }] },
    "messageId": "msg-001"
  }
}
```

### 4.3 ToolCallStarted — 工具调用开始

```json
{
  "update": {
    "kind": "tool_call",
    "toolCallId": "tc-001",
    "toolCall": {
      "type": "tool_call",
      "toolCallId": "tc-001",
      "name": "read_file",
      "input": { "path": "src/main.rs" },
      "status": "pending"
    }
  }
}
```

### 4.4 ToolCallUpdated — 工具调用状态更新

```json
{
  "update": {
    "kind": "tool_call_update",
    "toolCallId": "tc-001",
    "update": {
      "type": "tool_call_update",
      "status": "success",
      "output": "file contents here...",
      "rawOutput": "full un-normalized output",
      "content": [
        { "type": "text", "text": "output text" },
        { "type": "diff", "path": "src/main.rs", "oldText": "...", "newText": "..." }
      ]
    }
  }
}
```

### 4.5 Diff — 文件差异（ToolCallUpdated 的特殊子类型）

```json
{
  "update": {
    "kind": "tool_call_update",
    "toolCallId": "tc-002",
    "update": {
      "type": "tool_call_update",
      "status": "running",
      "content": [
        {
          "type": "diff",
          "path": "src/lib.rs",
          "oldText": "old code",
          "newText": "new code"
        }
      ]
    }
  }
}
```

### 4.6 UsageUpdate — Token 使用量

```json
{
  "update": {
    "kind": "usage_update",
    "used": 5000,
    "size": 128000,
    "_meta": {
      "token_usage": {
        "input_tokens": 4000,
        "output_tokens": 1000,
        "total_tokens": 5000,
        "cached_tokens": 0
      }
    }
  }
}
```

### 4.7 SessionInfoUpdate — 会话元信息

```json
{
  "update": {
    "kind": "session_info_update",
    "title": "实现二分查找",
    "_meta": null
  }
}
```

### 4.8 Plan — 执行计划

```json
{
  "update": {
    "kind": "plan",
    "entries": [
      {
        "id": "step-1",
        "title": "分析需求",
        "priority": "high",
        "status": "completed"
      }
    ]
  }
}
```

### 4.9 CurrentModeUpdate — 当前模式变更

```json
{
  "update": {
    "kind": "current_mode_update",
    "currentMode": "react"
  }
}
```

---

## 5. `session/cancel` (Notification)

客户端发送，无 id，无响应。

```json
{
  "jsonrpc": "2.0",
  "method": "session/cancel",
  "params": {
    "sessionId": "a1b2c3d4"
  }
}
```

**服务端实现**: `apps/acp/src/agent.rs:522` — `anureoAcpAgent::cancel()`

---

## 6. `session/load` — 恢复已有 session

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "session/load",
  "params": {
    "sessionId": "existing-session-id"
  }
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "sessionId": "existing-session-id",
    "modes": { ... },
    "configOptions": { ... }
  }
}
```

---

## 7. `session/list` — 列出所有 sessions

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "session/list",
  "params": {}
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "result": {
    "sessions": [
      { "sessionId": "a1b2", "cwd": "/path", "modes": { ... } }
    ]
  }
}
```

---

## 8. 其他 RPC 方法

| 方法 | 方向 | 说明 |
|------|------|------|
| `authenticate` | request/response | 认证（当前为空实现） |
| `session/fork` | request/response | 从现有 session 分叉 |
| `session/setmode` | request/response | 切换 agent mode |
| `session/setconfigoption` | request/response | 设置配置选项（如 model） |

---

## 客户端解析策略

客户端的 reader loop 需要区分三类消息：

```rust
fn handle_message(raw: &str) {
    let value: Value = serde_json::from_str(raw)?;

    if let Some(id) = value.get("id") {
        // 1. Response (has "id" + "method" is absent)
        //    → route to pending request's oneshot::Sender
        let id = id.as_i64().unwrap();
        route_response(id, value);

    } else if let Some(method) = value.get("method") {
        // 2. Notification (no "id", has "method")
        //    → dispatch by method name
        match method.as_str() {
            Some("session/update") => handle_session_update(value),
            _ => { /* ignore unknown notifications */ }
        }
    }
    // 3. Invalid — ignore
}
```

**关键**：`session/update` 通知没有 `id`，不会与任何 pending request 关联。它们通过独立的 channel 直接推送给 prompt 调用方。

---

## 协议版本协商

当前 anureo-server 实现 **Protocol Version 1**（`ProtocolVersion::V1`）。

客户端在 `initialize` 请求中发送 `protocolVersion: 1`。服务端在响应中回传它支持的版本。如果版本不匹配，客户端应报错或降级处理。

## 完整 Rust 类型引用

所有 JSON-RPC 消息类型定义在 `agent_client_protocol` crate 中：

```rust
use agent_client_protocol::schema::v1::{
    // Requests
    InitializeRequest, InitializeResponse,
    NewSessionRequest, NewSessionResponse,
    PromptRequest, PromptResponse,
    LoadSessionRequest, LoadSessionResponse,
    ListSessionsRequest, ListSessionsResponse,
    CancelNotification,
    SetSessionModeRequest, SetSessionModeResponse,
    SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    // Types
    SessionId, SessionUpdate, StopReason,
    ContentBlock, ContentChunk, PromptBlock,
    ToolCall, ToolCallUpdate, ToolCallStatus,
    SessionNotification, SessionModeState, SessionConfigOptions,
};
```

> **设计决策**：客户端复用这些类型而非自定义。`agent_client_protocol` 是服务端和客户端共享的协议库，CLI 只需在 `Cargo.toml` 中添加依赖即可。

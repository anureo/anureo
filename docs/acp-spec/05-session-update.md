# Session Update Notification

> **命名空间**: 标准 ACP v1
> **方向**: Agent → Client notification
> **实现状态**: ✅ 已实现
> **源码**: `apps/acp/src/stream_bridge.rs`、`apps/acp/src/notification_router.rs`

---

## Notification 框架

```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "thread-abc123",
    "update": {
      "type": "<variant>"
    }
  }
}
```

所有 variant 通过 `SessionNotifier` 发送。`SessionNotifier` 封装发送端、session ID 和上下文窗口大小。

---

## SessionUpdate Variant 完整列表

### 1. `agent_message_chunk`

Agent 响应文本的流式分块。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "agent_message_chunk",
    "text": "Let me fix the bug in "
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `text` | string | 文本分块内容 |

**逻辑**: 来自 anureo stream event 的文本块，直接转发为 ACP `TextChunk`。

---

### 2. `agent_thought_chunk`

Agent reasoning/thinking 的流式分块。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "agent_thought_chunk",
    "text": "Analyzing the auth module..."
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `text` | string | 思考内容分块 |

---

### 3. `user_message_chunk`

User 消息的回显。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "user_message_chunk",
    "contentBlocks": [
      { "type": "text", "text": "Fix the bug" }
    ]
  }
}
```

---

### 4. `tool_call`

工具调用通知——通知 Client 某个工具被调用。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "tool_call",
    "toolCallId": "call-001",
    "title": "Write to file",
    "kind": "create_file",
    "status": "pending",
    "locations": [
      { "path": "src/auth.rs", "line": 42 }
    ],
    "rawInput": {
      "path": "src/auth.rs",
      "content": "fn auth() { ... }"
    }
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `toolCallId` | string | 工具调用唯一 ID |
| `title` | string | 人类可读标题 |
| `kind` | string | 工具类型（`create_file`、`edit`、`bash`、`fetch`、`search` 等） |
| `status` | string | `pending` / `running` / `success` / `failed` |
| `locations` | array | 文件位置列表 |
| `locations[].path` | string | 相对路径 |
| `locations[].line` | int | 行号（可选） |
| `rawInput` | object | 原始工具参数 |

**Tool Kind 列表**: `create_file`、`str_replace`、`insert`、`bash`、`fetch`、`search`、`mcp`、`workflow` 等

---

### 5. `tool_call_update`

工具调用状态更新——更新已通知的 tool call。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "tool_call_update",
    "toolCallId": "call-001",
    "content": [
      {
        "type": "content",
        "content": {
          "type": "text",
          "text": "File written successfully"
        }
      }
    ],
    "status": "success"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `toolCallId` | string | 对应的 tool call ID |
| `content` | array | 工具输出内容列表 |
| `content[].type` | string | `content` / `diff` / `terminal` |
| `content[].content` | ContentBlock | 工具输出（文本/图片等） |
| `content[].diff` | object | 文件差异 |
| `content[].diff.path` | string | 文件路径 |
| `content[].diff.oldText` | string | 原文本 |
| `content[].diff.newText` | string | 新文本 |
| `content[].terminal` | object | 终端输出 |
| `content[].terminal.terminalId` | string | 终端 ID |
| `status` | string | 更新后的状态 |

**ToolCallContent 类型**（`content.rs`）:

```rust
pub enum ToolCallContent {
    Content { content: ContentBlock },
    Diff { path: String, old_text: String, new_text: String },
    Terminal { terminal_id: String },
}
```

---

### 6. `plan`

Agent 的工作计划更新。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "plan",
    "plan": {
      "steps": [
        { "id": "1", "content": "Analyze auth module", "status": "completed" },
        { "id": "2", "content": "Fix token validation", "status": "in_progress" },
        { "id": "3", "content": "Add tests", "status": "pending" }
      ]
    }
  }
}
```

---

### 7. `session_info_update`

Session 元信息更新。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "session_info_update",
    "title": "Fix auth module token validation bug",
    "metadata": {
      "custom_key": "custom_value"
    },
    "review": {
      "status": "approved",
      "reviewedAt": "2025-08-19T11:00:00Z"
    }
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `title` | string | session 标题（可由 Agent 或 server 自动生成） |
| `metadata` | object | 自定义元数据 |
| `review` | object | background review 结果 |

---

### 8. `current_mode_update`

Session 当前 agent profile 变更。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "current_mode_update",
    "mode": "dev"
  }
}
```

---

### 9. `config_option_update`

Session 配置项变更。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "config_option_update",
    "category": "model",
    "configId": "model",
    "value": "glm-4.6"
  }
}
```

---

### 10. `usage_update`

Token 用量更新——在 turn 结束时发送。

```json
{
  "sessionId": "thread-abc123",
  "update": {
    "type": "usage_update",
    "inputTokens": 5000,
    "outputTokens": 1500,
    "totalTokens": 6500,
    "cachedTokens": 3000
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `inputTokens` | u64 | 输入 token |
| `outputTokens` | u64 | 输出 token |
| `totalTokens` | u64 | 总 token |
| `cachedTokens` | u64 | 缓存命中的 token |

**高频节流**: `high_freq_usage.rs` 实现了高频 token usage 的节流逻辑，避免每 token 发送 notification。

---

## Lifecycle Variant

除内容 variant 外，还有 lifecycle 信号：

### `started`

Generation 开始。

```json
{
  "sessionId": "thread-abc123",
  "update": { "type": "started" }
}
```

### `in_progress`

消息开始（turn 内的中间消息）。

### `message_completed`

单条消息完成。

### `turn_finished`

整个 turn 完成（所有消息和工具调用结束），随后 prompt response 返回。

---

## 发送顺序保证

```text
session/prompt accepted
  → started
  → [agent_message_chunk × N]
  → [tool_call + tool_call_update × N]
  → message_completed
  → usage_update
  → turn_finished
  → prompt response (stopReason)
```

**关键约束**: prompt response 不能先于 `turn_finished` 被发送。

---

## Rust 类型

```rust
// stream_bridge.rs
pub struct SessionNotifier {
    sender: mpsc::Sender<SessionNotification>,
    session_id: String,
    context_window: Option<u32>,
}

impl SessionNotifier {
    fn try_send_event(&self, event: SessionUpdate) -> Result<()>;
    fn try_send_stream_event(&self, event: StreamEvent) -> Result<()>;
    fn with_usage_acc(&self, acc: TurnUsage) -> Self;
    fn enable_high_freq_tracking(&mut self);
    fn disable_high_freq_tracking(&mut self);
}

// 高频用量节流
// high_freq_usage.rs
pub struct HighFreqUsageTracker {
    interval: Duration,
    last_sent: Instant,
}
```

---

## Notification 路由

```rust
// notification_router.rs
pub enum SessionNotification {
    Update { session_id: String, update: SessionUpdate },
    // ...
}

// SessionUpdate 最终通过 AcpConnection 的 outbound channel 发送
pub enum ConnectionOutbound {
    Notification { value: SessionNotification, enqueued: Option<oneshot::Sender<()>> },
    Barrier(oneshot::Sender<()>),
}
```

notification 从 `SessionNotifier` → `mpsc channel` → `notification_router` → `AcpConnection.outbound_tx` → JSON-RPC text frame。

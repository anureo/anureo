# Prompt 和 Cancel

> **命名空间**: 标准 ACP v1
> **实现状态**: ✅ 已实现
> **源码**: `apps/acp/src/agent.rs`、`apps/acp/src/content.rs`

---

## 1. `session/prompt`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | session 必须已 new/load/resume |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "method": "session/prompt",
  "params": {
    "sessionId": "thread-abc123",
    "contentBlocks": [
      { "type": "text", "text": "Fix the bug in auth.rs" },
      { "type": "resource_link", "uri": "file:///src/auth.rs" },
      { "type": "image", "mimeType": "image/png", "data": "<base64>" }
    ]
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sessionId` | string | 是 | 目标 session |
| `contentBlocks` | array | 是 | content block 列表 |

### Content Block 类型

| Content block | anureo 支持 | 说明 |
|---|---|---|
| `text` | ✅ | 按顺序拼接为 user content |
| `resource_link` | ✅ | 转换为资源引用文本 |
| `image` | 条件支持 | 需要 Client `prompt.image` capability |
| `audio` | 条件支持 | 需要 Client `prompt.audio` capability |
| `resource` (embedded) | 条件支持 | 需要 `embeddedContext` capability |

**无法转换的 content 必须返回 `invalid_params` 或 capability error，不能静默丢弃。**

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "result": {
    "stopReason": "finished"
  }
}
```

| `stopReason` | 说明 |
|---|---|
| `finished` | Agent 正常完成 |
| `max_tokens` | 达到 token 上限 |
| `max_turns` | 达到 turn 上限 |
| `refused` | Agent 拒绝执行 |
| `cancelled` | 被 `session/cancel` 取消 |

### 逻辑说明

1. **验证 session binding**: 检查 sessionId 是否属于当前 connection
2. **验证 content blocks**: 通过 `content_blocks_to_user_content()` 转换
3. **捕获 snapshot**: model/mode/agent/effort 在此时捕获，不能在执行中途从 mutable state 重新解析
4. **Generation 唯一性**: 一个 session 同时只能有一个 active generation；已有 active generation 时返回 `busy`（code `-32010`）错误
5. **启动 generation**: 创建 `GenerationCancellation`，启动 agent 执行循环
6. **流式输出**: 执行过程中通过 `session/update` notification 发送更新
7. **Response 顺序**: prompt response 不能先于已生成的 update 被发送

### Content 转换规则

```rust
// content.rs
fn content_blocks_to_user_content(blocks: &[ContentBlock]) -> Result<UserContent, ContentError>

// 转换映射:
Text       → ContentPart::Text
Image      → ContentPart::ImageBase64 { media_type, data }
             或 ContentPart::ImageUrl { url }（如有 URL）
Audio      → ContentPart::AudioBase64 { media_type, data }
Resource   → TextResourceContents → ContentPart::Text（格式化）
             BlobResourceContents → 按 MIME 分发到 Image/Audio/Text
ResourceLink → ContentPart::Text（格式化引用）

// 快捷路径: 若所有 parts 都是 Text，合并为单个 UserContent::Text
```

### Prompt 执行流程

```text
validate session binding
  → validate content blocks
  → capture model/mode/config snapshot
  → start generation (创建 GenerationCancellation)
  → emit session/update (agent_message_chunk, tool_call, etc.)
  → flush final update
  → return PromptResponse(stopReason)
```

### Rust 类型

```rust
async fn prompt(&self, args: PromptRequest)
    -> agent_client_protocol::Result<PromptResponse>

async fn prompt_with_capabilities(
    &self,
    args: PromptRequest,
    client_capabilities: ClientCapabilitiesInfo,
    client_bridge: Arc<dyn ClientBridgeTrait>
) -> agent_client_protocol::Result<PromptResponse>
```

### Error

| Error code | 触发条件 |
|---|---|
| `session_not_found` | session 不存在或不属于当前连接 |
| `busy (-32010)` | session 已有 active generation |
| `wrong connection (-32011)` | session 绑定在其他 connection |
| `Invalid Params (-32602)` | content blocks 为空或包含不支持类型 |

---

## 2. `session/cancel`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent **notification**（无 response） |
| anureo 状态 | ✅ 已实现 |

### Notification

```json
{
  "jsonrpc": "2.0",
  "method": "session/cancel",
  "params": {
    "sessionId": "thread-abc123"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sessionId` | string | 是 | 要取消的 session |

### 逻辑说明

1. **立即设置 cancellation flag**: `GenerationCancellation` 被标记为 cancelled
2. **周期性检查**: generation 循环、tool execution、permission wait 周期性观察该 flag
3. **最终 response**: 被 cancel 的 prompt 最终返回 `stopReason = cancelled`
4. **幂等**: 重复 cancel 是 no-op
5. **隔离**: cancel 不影响其他 session

### Cancel 传播链

```text
session/cancel notification
  → SessionStore 设置 cancellation flag
  → generation loop 观察到 flag → 停止
  → tool execution 观察到 flag → 中断
  → permission wait 观察到 flag → 取消
  → prompt response 返回 stopReason = cancelled
```

### Rust 类型

```rust
async fn cancel(&self, args: CancelNotification)
    -> agent_client_protocol::Result<()>

fn cancel_all(&self)  // 取消所有 session（用于 shutdown）

// GenerationCancellation 负责传播取消信号
```

### Error

cancel 是 notification，不返回 JSON-RPC response。如 session 不存在，anureo 静默忽略。

---

## Generation 状态转换

```text
created → prompting → completed
                    ├→ cancelled
                    └→ failed
```

| 状态 | 含义 | 触发 |
|---|---|---|
| `created` | session 刚创建 | `session/new` |
| `prompting` | prompt 执行中 | `session/prompt` accepted |
| `completed` | 正常结束 | `stopReason = finished` |
| `cancelled` | 被取消 | `session/cancel` |
| `failed` | 执行失败 | Agent/runtime 错误 |

**历史 session 记录不能被用来推断当前仍在运行；当前运行状态必须来自 live generation state。**

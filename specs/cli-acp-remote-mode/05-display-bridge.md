# 05 — ACP Notification → Display Bridge

> **Scope**: 将 ACP `session/update` 通知转换为 CLI display 层可消费的事件  
> **File**: `apps/cli/src/server_transport/display_bridge.rs`

## 问题

CLI 的 display 层（`apps/cli/src/display/`）是为**本地 ReAct graph** 设计的——它消费的是 `StreamEvent`（anureo 内部事件类型），而非 ACP `SessionUpdate`。

在 remote ACP 模式下，事件来源是 ACP `session/update` 通知，需要转换到 display 层能理解的格式。

## 两条路径

### 路径 A：ACP → StreamEvent → Display（推荐）

将 `AcpSessionUpdate` 转换回 anureo 的 `StreamEvent`，然后复用现有的 display 回调。

```
ACP SessionNotification
    │
    ▼
AcpSessionUpdate (acp_client.rs 解析)
    │
    ▼  ★ convert_acp_to_stream_event()
StreamEvent (anureo 内部类型)
    │
    ▼
on_event_react / on_event_dup (display/event_handler.rs)
    │
    ▼
终端渲染 (markdown, tool_preview, spinner)
```

**优势**：完全复用现有 display 代码，零修改。

### 路径 B：ACP → 直接渲染

`DisplayBridge` 直接消费 `AcpSessionUpdate`，自己实现渲染逻辑。

**劣势**：大量重复代码（markdown 渲染、tool preview 格式化等）。

**结论**：选择路径 A。

## StreamEvent 类型

查看 anureo 内部事件类型（`stream_event` crate）：

```rust
pub enum StreamEvent<S> {
    /// LLM 回复文本 chunk
    MessageChunk(MessageChunk),
    /// 工具调用
    ToolCall(ToolCall),
    /// 工具结果
    ToolResult(ToolResult),
    /// 节点进入（act/think/observe）
    NodeEnter { node: String, .. },
    /// 使用量
    Usage { input_tokens: u32, output_tokens: u32, .. },
    /// 思考
    ThinkingChunk { text: String, .. },
    // ...
}

pub struct MessageChunk {
    pub kind: MessageChunkKind,  // Text, Thinking, etc.
    pub text: String,
}

pub enum MessageChunkKind {
    Text,
    Thinking,
    // ...
}
```

## 转换映射

### AcpSessionUpdate → StreamEvent

```rust
//! ACP session update → anureo StreamEvent conversion.
//!
//! Allows the remote ACP mode to reuse the existing display layer
//! (event_handler.rs, streaming_markdown.rs, tool_preview.rs) without
//! modification.

use agent::run::TypedAnyStreamEvent;
use stream_event::{MessageChunk, MessageChunkKind, StreamEvent};
use super::acp_client::AcpSessionUpdate;

/// Convert an ACP session update into a type-erased stream event
/// compatible with the display layer.
///
/// Returns `None` for updates that have no display equivalent
/// (e.g. `SessionInfoUpdate`, `Plan`).
pub fn convert_acp_to_stream_event(
    update: AcpSessionUpdate,
) -> Option<TypedAnyStreamEvent> {
    match update {
        AcpSessionUpdate::AgentMessageChunk { text, .. } => {
            // → StreamEvent::MessageChunk (Text)
            Some(TypedAnyStreamEvent::React(StreamEvent::MessageChunk(MessageChunk {
                kind: MessageChunkKind::Text,
                text,
            })))
        }

        AcpSessionUpdate::AgentThoughtChunk { text, .. } => {
            // → StreamEvent::MessageChunk (Thinking)
            Some(TypedAnyStreamEvent::React(StreamEvent::MessageChunk(MessageChunk {
                kind: MessageChunkKind::Thinking,
                text,
            })))
        }

        AcpSessionUpdate::ToolCallStarted { tool_call_id, name, input } => {
            // → StreamEvent::ToolCall
            Some(TypedAnyStreamEvent::React(StreamEvent::ToolCall(
                agent::state::ToolCall {
                    id: tool_call_id,
                    name,
                    args: input.unwrap_or_default(),
                    ..Default::default()
                },
            )))
        }

        AcpSessionUpdate::ToolCallUpdated { tool_call_id, status, output, raw_output } => {
            // → StreamEvent::ToolResult
            let is_error = status == "failure";
            Some(TypedAnyStreamEvent::React(StreamEvent::ToolResult(
                agent::state::ToolResult {
                    tool_call_id,
                    output: raw_output.or(output).unwrap_or_default(),
                    is_error,
                },
            )))
        }

        AcpSessionUpdate::Diff { path, old_text, new_text, .. } => {
            // → StreamEvent::ToolCallUpdated with diff content
            // The display layer has special handling for diffs (format_diff)
            Some(TypedAnyStreamEvent::React(StreamEvent::ToolResult(
                agent::state::ToolResult {
                    tool_call_id: String::new(),
                    output: format_diff_output(&path, old_text.as_deref(), &new_text),
                    is_error: false,
                },
            )))
        }

        AcpSessionUpdate::UsageUpdate { used, size: _ } => {
            // → StreamEvent::Usage
            // Note: ACP usage_update gives cumulative used/total, not per-call deltas.
            // The display layer uses these for the context limit bar.
            Some(TypedAnyStreamEvent::React(StreamEvent::Usage {
                input_tokens: used as u32,
                output_tokens: 0, // Not separately available in ACP usage_update
            }))
        }

        AcpSessionUpdate::SessionInfoUpdate { .. } => {
            // No display equivalent — session title update
            None
        }

        AcpSessionUpdate::Plan { .. } => {
            // Could map to a custom display event in the future
            None
        }

        AcpSessionUpdate::CurrentModeUpdate { .. } => {
            // No display equivalent — mode change notification
            None
        }
    }
}

fn format_diff_output(path: &str, old_text: Option<&str>, new_text: &str) -> String {
    // Use the existing diff formatter from display::tool_preview
    crate::display::format_diff(path, old_text.unwrap_or(""), new_text)
}
```

> **注意**：`TypedAnyStreamEvent` 是一个枚举（`React(StreamEvent<...>)` / `Dup(...)` / `Tot(...)` / `Got(...)`），需要选择正确的变体。由于 ACP 模式不关心具体的 graph 类型，使用 `React` 变体即可——display 层的 `on_event_react` 处理器会消费它。

## DisplayBridge 结构体

```rust
//! Bridge between ACP session updates and the CLI display layer.

use std::sync::Mutex;

use agent::run::TypedAnyStreamEvent;
use crate::args::Args;
use crate::display::{
    create_stdio_event_callback, EventState, StreamDisplayConfig,
};
use super::acp_client::AcpSessionUpdate;

/// Connects ACP session updates to the existing CLI display layer.
///
/// Converts `AcpSessionUpdate` → `StreamEvent` and feeds it through
/// the same rendering pipeline used by local mode.
pub struct DisplayBridge {
    /// Display configuration (verbose, spinner, timestamp, etc.)
    config: StreamDisplayConfig,
    /// Mutable display state (turn counter, tool calls, spinner, etc.)
    state: Mutex<EventState>,
}

impl DisplayBridge {
    /// Create from CLI args.
    pub fn from_args(args: &Args) -> Self {
        let config = StreamDisplayConfig {
            verbose: args.verbose > 0,
            display_max_len: 200, // crate::display_limits::max_tool_preview_len()
            output_timestamp: args.timestamp,
            agent_display: args.agent.clone(),
            use_spinner: crate::display::is_stdout_tty(),
        };

        let state = EventState::new(args.agent.clone(), config.use_spinner);

        Self {
            config,
            state: Mutex::new(state),
        }
    }

    /// Handle a single ACP session update.
    pub fn handle_update(&self, update: AcpSessionUpdate) {
        // Convert to stream event.
        if let Some(typed_event) = convert_acp_to_stream_event(update) {
            // Feed through the display callback.
            let callback = create_stdio_event_callback(
                &self.config,
                self.state.lock().unwrap().deref_mut(),
            );
            callback(&typed_event);
        }
    }

    /// Print the final prompt result.
    pub fn print_result(&self, response: &agent_client_protocol::schema::v1::PromptResponse) {
        // The final reply is in response.content — extract text.
        for block in &response.content {
            if let agent_client_protocol::schema::v1::ContentBlock::Text(text_block) = block {
                // The text was already streamed via agent_message_chunk
                // notifications, so we don't need to print it again.
                // Only print a final newline if needed.
            }
        }

        // Print usage summary if available.
        let state = self.state.lock().unwrap();
        if state.total_prompt_tokens > 0 || state.total_completion_tokens > 0 {
            crate::display::panel_format::format_usage_line(
                state.total_prompt_tokens,
                state.total_completion_tokens,
            );
        }
    }
}
```

## 事件映射完整性

| AcpSessionUpdate 变体 | StreamEvent 目标 | Display 效果 |
|-----------------------|-----------------|-------------|
| `AgentMessageChunk` | `MessageChunk(Text)` | 流式 markdown 渲染 |
| `AgentThoughtChunk` | `MessageChunk(Thinking)` | dim 灰色 thinking 文本 |
| `ToolCallStarted` | `ToolCall` | 工具调用面板（名称 + 参数预览） |
| `ToolCallUpdated` (success) | `ToolResult` | 绿色 ✓ + 结果预览 |
| `ToolCallUpdated` (failure) | `ToolResult(is_error)` | 红色 ✗ + 错误信息 |
| `ToolCallUpdated` (running) | `NodeEnter(act)` | spinner 动画 |
| `Diff` | `ToolResult` + diff format | DIFF 面板（`format_diff`） |
| `UsageUpdate` | `Usage` | context limit 进度条 |
| `SessionInfoUpdate` | — (无操作) | 标题更新（不显示） |
| `Plan` | — (无操作) | 计划展示（未来增强） |
| `CurrentModeUpdate` | — (无操作) | 模式切换（不显示） |

## 复杂点：工具调用关联

ACP 使用 `tool_call_id` 关联 `ToolCallStarted` 和 `ToolCallUpdated`。display 层的 `EventState` 维护 `pending_tool_calls` 列表。

在本地模式中，ReAct graph 保证 `ToolCall` → `ToolResult` 的顺序。在 ACP 模式中，通知可能交错（多个工具并发），需要通过 `tool_call_id` 正确匹配。

```rust
impl DisplayBridge {
    /// Track active tool calls by id for proper result matching.
    fn handle_update_with_tracking(&self, update: AcpSessionUpdate) {
        let mut state = self.state.lock().unwrap();

        match &update {
            AcpSessionUpdate::ToolCallStarted { tool_call_id, name, .. } => {
                // Add to pending list.
                state.pending_tool_calls.push(agent::state::ToolCall {
                    id: tool_call_id.clone(),
                    name: name.clone(),
                    ..Default::default()
                });
            }
            AcpSessionUpdate::ToolCallUpdated { tool_call_id, status, .. } => {
                if status == "success" || status == "failure" {
                    // Remove from pending, add to results.
                    if let Some(idx) = state.pending_tool_calls.iter()
                        .position(|tc| &tc.id == tool_call_id)
                    {
                        let tc = state.pending_tool_calls.remove(idx);
                        state.pending_tool_results.push(agent::state::ToolResult {
                            tool_call_id: tool_call_id.clone(),
                            ..Default::default()
                        });
                    }
                }
            }
            _ => {}
        }

        // Also feed through the standard conversion + display.
        drop(state);
        self.handle_update(update);
    }
}
```

## JSON 输出模式

当 `--json` 被指定时，不使用 display 层，而是直接输出 ACP 通知为 JSON：

```rust
impl DisplayBridge {
    pub fn handle_update_json(&self, update: &AcpSessionUpdate, pretty: bool) {
        let json = if pretty {
            serde_json::to_string_pretty(update).unwrap_or_default()
        } else {
            serde_json::to_string(update).unwrap_or_default()
        };
        println!("{}", json);
    }
}
```

`AcpSessionUpdate` 需要实现 `Serialize`：

```rust
#[derive(Debug, Clone, Serialize)]
pub enum AcpSessionUpdate {
    // ... same variants ...
}
```

## 边界情况

| 场景 | 处理 |
|------|------|
| 收到未知 `kind` 的 session/update | 记录 trace 日志，跳过 |
| 工具结果先于工具调用到达（理论上不应发生） | 缓存到 pending，等待 ToolCallStarted |
| 同一 message 的多个 chunk 交错 | 按顺序追加到 markdown renderer |
| Agent 文本 + thinking 交替 | `EventState.in_thinking` 跟踪当前状态 |
| Prompt 被取消 | 收到 `PromptResponse` 且 `stop.reason` 不是 `EndTurn` |

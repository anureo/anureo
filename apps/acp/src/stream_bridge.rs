//! Map anureo stream events to ACP SessionUpdate-equivalent structures
//!
//! [`agent::run::run_agent_with_options`]'s `on_event` callback receives [`agent::TypedAnyStreamEvent`].
//! This module provides [`anureo_event_to_updates`] to turn a single anureo event into zero or more [`StreamUpdate`],
//! which the upper layer sends as **session/update notifications** (no response) via the `agent_client_protocol` connection.
//! Protocol details are in [`crate::protocol`].
//!
//! ## SessionUpdate variants and anureo sources
//!
//! | Variant | Meaning | anureo source |
//! |---------|---------|-------------|
//! | **user_message_chunk** | Chunk of user message | History replay only (`Message::User`). |
//! | **agent_message_chunk** | Chunk of agent reply (streamed text) | Any node's non-Thinking text output. |
//! | **agent_thought_chunk** | Chunk of agent reasoning | `StreamEvent::ReasoningDelta`, or `TaskStart` (node entry). |
//! | **tool_call** | New tool call started | Act node decides to call a tool: tool_call_id, name, input, kind, status: Pending. |
//! | **tool_call_update** | Update to existing tool call | Start -> Pending/Running; done -> Success/Failure + output/content. |
//! | plan / available_commands_update / current_mode_update | Plan, command list, mode | Optional; DUP/ToT/GoT etc. can map. |
//! | **session_info_update** | Session metadata (title) update | Agent pushes title or other metadata to client. |
//!
//! ## Tool call and request_permission order
//!
//! 1. Send **ToolCall** (new tool, status: Pending).
//! 2. If permission needed: call **session/request_permission**, wait for Client response.
//! 3. If allowed: send **ToolCallUpdate** (status: Running) -> execute tool -> **ToolCallUpdate** (Success/Failure + output).
//! 4. If denied or Cancelled: send **ToolCallUpdate** (Failure or denied), do not execute; on Cancelled end the turn with StopReason::Cancelled.
//!
//! [`StreamUpdate`] is a protocol-agnostic intermediate form; when wired to ACP it is converted to `SessionUpdate` and sent.
//!
//! [`stream_update_to_session_notification`] converts this module's [`StreamUpdate`] into
//! `agent_client_protocol::SessionNotification` for the upper layer to send via the connection.

use crate::agent::TurnUsage;
use crate::content::extract_locations;
use crate::high_freq_usage::HighFreqUsageTracker;
use agent::run::TypedAnyStreamEvent;
use agent_client_protocol::schema::v1::{
    ContentBlock, ContentChunk, CurrentModeUpdate, Diff, MessageId, Meta, Plan, PlanEntry,
    PlanEntryPriority, PlanEntryStatus, SessionId, SessionInfoUpdate, SessionModeId,
    SessionNotification, SessionUpdate, Terminal, TerminalId, TextContent, ToolCall,
    ToolCallContent, ToolCallId, ToolCallLocation, ToolCallStatus, ToolCallUpdate,
    ToolCallUpdateFields, ToolKind, UsageUpdate,
};
use anureo_llm::message::Message;
use anureo_util::text::truncate::truncate_tail;
use serde_json::Value;
use std::sync::Arc;
use std::sync::Mutex;
use stream_event::StreamEvent;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Ingress marker sent before a `send_history` replay batch. The runtime
/// ingress loop uses it to suppress `session.updated` global broadcasts while
/// replaying history: a `session/load` is a read, not a mutation, so it must
/// not fan out one cross-connection `session.updated` per replayed message.
pub const REPLAY_BEGIN_MARKER_PREFIX: &str = "__anureo_replay_begin__";
/// Ingress marker sent after a `send_history` replay batch.
pub const REPLAY_END_MARKER_PREFIX: &str = "__anureo_replay_end__";

/// JSON-RPC method of the batched history-replay notification.
pub const HISTORY_BATCH_METHOD: &str = "_anureo.dev/session-history/batch";

/// Element type flowing through the session-update channel from
/// `SessionNotifier` to the `AcpRuntime` ingress loop.
///
/// `Session` is the standard per-notification path (live streaming,
/// replay markers). `HistoryBatch` carries an entire `session/load` tail
/// replay as ONE envelope; the runtime forwards it to the bound connection
/// as a single [`HISTORY_BATCH_METHOD`] JSON-RPC notification instead of N
/// `session/update` frames.
// 544-byte largest variant is fine here: envelopes move through a bounded
// mpsc channel (one memmove per send), and boxing would add an allocation
// per live-streaming notification on the hot path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, serde::Serialize)]
pub enum SessionUpdateEnvelope {
    Session(SessionNotification),
    /// A history replay update for the session-recovery extension. It uses the
    /// canonical session/update route, but is marked so the runtime can assign
    /// a durable stream cursor without treating legacy replay as a mutation.
    SessionReplay {
        notification: SessionNotification,
        recovery: bool,
    },
    HistoryBatch {
        session_id: SessionId,
        updates: Vec<SessionUpdate>,
    },
}

/// Whether `send_history` packs the replay into a single batch notification
/// (default) or falls back to per-update `session/update` notifications
/// (`ANUREO_ACP_LOAD_HISTORY_BATCH=0`).
pub fn history_batch_enabled() -> bool {
    std::env::var("ANUREO_ACP_LOAD_HISTORY_BATCH")
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
}

impl SessionUpdateEnvelope {
    /// Session id the envelope is scoped to (both variants carry one).
    pub fn session_id(&self) -> &SessionId {
        match self {
            Self::Session(notification) => &notification.session_id,
            Self::SessionReplay { notification, .. } => &notification.session_id,
            Self::HistoryBatch { session_id, .. } => session_id,
        }
    }
}

fn replay_marker(prefix: &str, session_id: &SessionId) -> SessionNotification {
    SessionNotification::new(
        SessionId::new(format!("{prefix}{session_id}")),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            String::new(),
        )))),
    )
}

/// A single "sendable to Client" stream update, corresponding to ACP SessionUpdate variants.
///
/// Kept in sync with `agent_client_protocol::SessionUpdate` so the prompt callback can
/// convert to the protocol type and call `connection.send_notification(session/update)`.
#[derive(Clone, Debug)]
pub enum StreamUpdate {
    /// Chunk of user message (ACP `user_message_chunk`). History replay only; streaming never produces this variant.
    UserMessageChunk {
        text: String,
        message_id: Option<String>,
    },

    /// Chunk of model output text (ACP `agent_message_chunk`).
    AgentMessageChunk {
        text: String,
        message_id: Option<String>,
    },

    /// Chunk of agent reasoning / node entry (ACP `agent_thought_chunk`).
    AgentThoughtChunk {
        text: String,
        message_id: Option<String>,
    },

    /// New tool call started (ACP `tool_call`, status: Pending).
    ToolCallStarted {
        /// Unique tool call id for this session; ToolCallUpdated uses the same id.
        tool_call_id: String,
        /// Tool name (e.g. "read_file").
        name: String,
        /// Raw arguments (JSON); can be turned into ToolCall input for ACP.
        input: Option<Value>,
        /// For Client icon/display; maps to ToolKind in ACP.
        kind: Option<String>,
    },

    /// Status/result update for an existing tool call (ACP `tool_call_update`).
    ToolCallUpdated {
        tool_call_id: String,
        /// e.g. "running" | "success" | "failure"; maps to ToolCallStatus in ACP.
        status: String,
        /// Result or error message (possibly normalized/truncated).
        output: Option<String>,
        /// Full un-normalized result. When set, used for ACP `raw_output` instead of `output`.
        raw_output: Option<String>,
    },

    /// File diff update (ACP `tool_call_update` with diff content).
    /// Shows file modifications in a format suitable for client display.
    Diff {
        /// The tool call that produced this diff.
        tool_call_id: String,
        /// File path
        path: String,
        /// Previous content (optional)
        old_text: Option<String>,
        /// New content
        new_text: String,
    },

    /// Session metadata update (ACP `session_info_update`).
    /// Used to push title and related metadata changes to the client in real time.
    /// When `meta` is `Some`, it is attached to the notification as `_meta`
    /// (e.g. for background-review status sync to the session list).
    SessionInfoUpdate { title: String, meta: Option<Meta> },

    /// Agent execution plan (ACP `plan`).
    /// Reports the agent's planned tasks with their priority and status.
    Plan { entries: Vec<PlanEntry> },

    /// Context window usage update (ACP `usage_update`).
    /// Reports current context token usage and total window size, plus optional
    /// billing-level token breakdown carried in ACP's `_meta` extension channel.
    UsageUpdate {
        /// Tokens currently in context (per LLM call prompt).
        used: u64,
        /// Total context window size in tokens.
        size: u64,
        /// Optional `_meta` payload: session-level billing token breakdown
        /// (`token_usage.{input_tokens,output_tokens,total_tokens,cached_tokens}`).
        /// Only populated when `SessionNotifier` is wired with a `TurnUsage` accumulator.
        meta: Option<Meta>,
    },
}

/// Convert one anureo stream event into zero or more [`StreamUpdate`]s.
///
/// If the event does not need to be pushed to the Client (e.g. some Checkpoint, Usage), returns an empty vec.
/// Within a single prompt turn, `tool_call_id` generation and consistency are the caller's responsibility (e.g. by call_id or incrementing id).
///
/// # Arguments
///
/// - `ev`: anureo's type-erased stream event (one of React/Dup/Tot/GoT).
///
/// # Returns
///
/// The list of updates for this event; may be empty.
///
/// # Example (in on_event callback)
///
/// ```ignore
/// let updates = anureo_acp::anureo_event_to_updates(ev);
/// for u in updates {
///     connection.send_session_update(session_id, u).await?;
/// }
/// ```
pub fn anureo_event_to_updates(ev: &TypedAnyStreamEvent) -> Vec<StreamUpdate> {
    match ev {
        TypedAnyStreamEvent::React(e) => {
            let mut updates = stream_event_to_updates_inner(e);
            if let Some(title_update) = extract_title_from_react_event(e) {
                updates.push(title_update);
            }
            updates
        }
        TypedAnyStreamEvent::Dup(e) => stream_event_to_updates_inner(e),
        TypedAnyStreamEvent::Tot(e) => stream_event_to_updates_inner(e),
        TypedAnyStreamEvent::Got(e) => stream_event_to_updates_inner(e),
    }
}

fn extract_title_from_react_event(
    ev: &StreamEvent<agent::state::ReActState>,
) -> Option<StreamUpdate> {
    match ev {
        StreamEvent::Updates { node_id, state, .. } if node_id == "title" => {
            tracing::debug!("Title update event detected, extracting title from state");
            state.summary.as_ref().map(|title| {
                tracing::debug!(title = %title, "Successfully extracted title from ReAct event");
                StreamUpdate::SessionInfoUpdate {
                    title: title.clone(),
                    meta: None,
                }
            })
        }
        _ => None,
    }
}

fn resolve_tool_call_id(call_id: &Option<String>) -> String {
    call_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("tool-{}", Uuid::new_v4()))
}

/// Uniform mapping for any `StreamEvent<S>` (uses only S-independent fields).
fn stream_event_to_updates_inner<S>(ev: &StreamEvent<S>) -> Vec<StreamUpdate>
where
    S: std::fmt::Debug + Clone + Send + Sync + 'static,
{
    match ev {
        StreamEvent::TaskStart { node_id: _, .. } => vec![],
        StreamEvent::TextDelta { content, .. } => {
            vec![StreamUpdate::AgentMessageChunk {
                text: content.clone(),
                message_id: None,
            }]
        }
        StreamEvent::ReasoningDelta { content, .. } => {
            vec![StreamUpdate::AgentThoughtChunk {
                text: content.clone(),
                message_id: None,
            }]
        }
        StreamEvent::ToolCall {
            call_id,
            name,
            arguments,
        } => {
            let id = resolve_tool_call_id(call_id);
            vec![StreamUpdate::ToolCallStarted {
                tool_call_id: id,
                name: name.clone(),
                input: Some(arguments.clone()),
                kind: None,
            }]
        }
        StreamEvent::ToolStart { call_id, name: _ } => {
            let id = resolve_tool_call_id(call_id);
            vec![StreamUpdate::ToolCallUpdated {
                tool_call_id: id,
                status: "running".to_string(),
                output: None,
                raw_output: None,
            }]
        }
        StreamEvent::ToolOutput {
            call_id, content, ..
        } => {
            let id = resolve_tool_call_id(call_id);

            // Try to deserialize content as ToolCallContent to check for Diff
            if let Ok(tool_core::ToolCallContent::Diff {
                path,
                old_text,
                new_text,
            }) = serde_json::from_str::<tool_core::ToolCallContent>(content)
            {
                return vec![StreamUpdate::Diff {
                    tool_call_id: id,
                    path,
                    old_text,
                    new_text,
                }];
            }

            // Handle regular tool output
            vec![StreamUpdate::ToolCallUpdated {
                tool_call_id: id,
                status: "running".to_string(),
                output: Some(content.clone()),
                raw_output: None,
            }]
        }
        StreamEvent::ToolEnd {
            call_id,
            name,
            result,
            is_error,
            raw_result,
        } => {
            let id = resolve_tool_call_id(call_id);

            let is_diff_output = raw_result
                .as_deref()
                .or(Some(result.as_str()))
                .and_then(|s| serde_json::from_str::<tool_core::ToolCallContent>(s).ok())
                .map(|c| matches!(c, tool_core::ToolCallContent::Diff { .. }))
                .unwrap_or(false);

            let mut updates = if is_diff_output {
                let diff_content = raw_result
                    .as_deref()
                    .or(Some(result.as_str()))
                    .and_then(|s| serde_json::from_str::<tool_core::ToolCallContent>(s).ok());

                let mut updates = Vec::new();

                if let Some(tool_core::ToolCallContent::Diff {
                    path,
                    old_text,
                    new_text,
                }) = diff_content
                {
                    updates.push(StreamUpdate::Diff {
                        tool_call_id: id.clone(),
                        path,
                        old_text,
                        new_text,
                    });
                }

                updates.push(StreamUpdate::ToolCallUpdated {
                    tool_call_id: id,
                    status: if *is_error {
                        "failure".to_string()
                    } else {
                        "success".to_string()
                    },
                    output: None,
                    raw_output: None,
                });
                updates
            } else {
                vec![StreamUpdate::ToolCallUpdated {
                    tool_call_id: id,
                    status: if *is_error {
                        "failure".to_string()
                    } else {
                        "success".to_string()
                    },
                    output: Some(result.clone()),
                    raw_output: raw_result.clone(),
                }]
            };

            if name == "todo_write" && !is_error {
                if let Some(entries) = parse_todo_result_to_plan_entries(result) {
                    updates.push(StreamUpdate::Plan { entries });
                }
            }

            updates
        }
        _ => vec![],
    }
}

/// Convert this crate's [`StreamUpdate`] into ACP's [`SessionNotification`] for sending via the connection.
///
/// Returns `None` for `ToolCallUpdated` with empty `tool_call_id` (anureo ToolOutput may lack call_id).
pub fn stream_update_to_session_notification(
    session_id: &SessionId,
    u: &StreamUpdate,
) -> Option<SessionNotification> {
    let update = match u {
        StreamUpdate::UserMessageChunk { text, message_id } => {
            let mut chunk = ContentChunk::new(text.clone().into());
            chunk = chunk.message_id(message_id.clone().map(MessageId::new));
            SessionUpdate::UserMessageChunk(chunk)
        }
        StreamUpdate::AgentMessageChunk { text, message_id } => {
            let mut chunk = ContentChunk::new(text.clone().into());
            chunk = chunk.message_id(message_id.clone().map(MessageId::new));
            SessionUpdate::AgentMessageChunk(chunk)
        }
        StreamUpdate::AgentThoughtChunk { text, message_id } => {
            let mut chunk = ContentChunk::new(text.clone().into());
            chunk = chunk.message_id(message_id.clone().map(MessageId::new));
            SessionUpdate::AgentThoughtChunk(chunk)
        }
        StreamUpdate::ToolCallStarted {
            tool_call_id,
            name,
            input,
            kind,
        } => {
            let tc = create_tool_call(tool_call_id, name, input.as_ref(), kind.as_deref());
            tracing::trace!(
                tool_call_id = %tool_call_id,
                name = %name,
                input = ?input,
                kind = ?kind,
                tc = ?tc,
                "tool_call session update"
            );
            SessionUpdate::ToolCall(tc)
        }
        StreamUpdate::ToolCallUpdated {
            tool_call_id,
            status,
            output,
            raw_output,
        } => {
            if tool_call_id.is_empty() {
                return None;
            }
            let status = match status.as_str() {
                "running" => ToolCallStatus::InProgress,
                "success" => ToolCallStatus::Completed,
                "failure" => ToolCallStatus::Failed,
                _ => ToolCallStatus::InProgress,
            };
            let mut fields = ToolCallUpdateFields::new().status(status);
            if let Some(ref s) = output {
                let effective_raw = raw_output.as_deref().unwrap_or(s);
                fields = fields
                    .content(vec![s.clone().into()])
                    .raw_output(parse_text_output_to_raw_value(effective_raw));
            }
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(tool_call_id.as_str()),
                fields,
            ))
        }
        StreamUpdate::Diff {
            tool_call_id,
            path,
            old_text,
            new_text,
        } => {
            let mut fields = ToolCallUpdateFields::new().content(vec![ToolCallContent::Diff(
                Diff::new(path.clone(), new_text.clone()).old_text(old_text.clone()),
            )]);

            let status = ToolCallStatus::Completed;
            fields = fields.status(status);

            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new(tool_call_id.as_str()),
                fields,
            ))
        }
        StreamUpdate::SessionInfoUpdate { title, meta } => {
            let mut info = SessionInfoUpdate::new().title(title.clone());
            if let Some(m) = meta {
                info = info.meta(m.clone());
            }
            SessionUpdate::SessionInfoUpdate(info)
        }
        StreamUpdate::Plan { entries } => SessionUpdate::Plan(Plan::new(entries.clone())),
        StreamUpdate::UsageUpdate { used, size, meta } => {
            let mut u = UsageUpdate::new(*used, *size);
            if let Some(m) = meta {
                u = u.meta(m.clone());
            }
            SessionUpdate::UsageUpdate(u)
        }
    };
    Some(SessionNotification::new(session_id.clone(), update))
}

fn extract_text_from_result(result: &str) -> Option<String> {
    if let Ok(content) = serde_json::from_str::<tool_core::ToolCallContent>(result) {
        return Some(content.into_text());
    }
    if let Ok(s) = serde_json::from_str::<String>(result) {
        return Some(s);
    }
    Some(result.to_string())
}

fn parse_todo_result_to_plan_entries(result: &str) -> Option<Vec<PlanEntry>> {
    let text = extract_text_from_result(result)?;
    let json_start = text.find('[')?;
    let json_str = &text[json_start..];
    let items: Vec<serde_json::Value> = serde_json::from_str(json_str).ok()?;
    let entries: Vec<PlanEntry> = items
        .iter()
        .filter_map(|t| {
            let content = t.get("content")?.as_str()?.to_string();
            let priority = match t.get("priority")?.as_str()? {
                "high" => PlanEntryPriority::High,
                "medium" => PlanEntryPriority::Medium,
                _ => PlanEntryPriority::Low,
            };
            let status = match t.get("status")?.as_str()? {
                "in_progress" => PlanEntryStatus::InProgress,
                "completed" | "cancelled" => PlanEntryStatus::Completed,
                _ => PlanEntryStatus::Pending,
            };
            Some(PlanEntry::new(content, priority, status))
        })
        .collect();
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

fn parse_text_output_to_raw_value(output: &str) -> serde_json::Value {
    serde_json::from_str(output).unwrap_or_else(|_| serde_json::json!(output))
}

pub fn name_to_tool_kind(name: &str) -> ToolKind {
    let n = name.to_lowercase();
    if n.contains("read") {
        ToolKind::Read
    } else if n.contains("write") || n.contains("edit") {
        ToolKind::Edit
    } else if n.contains("delete") || n.contains("remove") {
        ToolKind::Delete
    } else if n.contains("move") || n.contains("rename") {
        ToolKind::Move
    } else if n.contains("search") || n.contains("grep") || n.contains("glob") {
        ToolKind::Search
    } else if n.contains("run")
        || n.contains("bash")
        || n.contains("command")
        || n.contains("exec")
        || n.contains("shell")
    {
        ToolKind::Execute
    } else if n.contains("think") || n.contains("reason") {
        ToolKind::Think
    } else if n.contains("fetch") {
        ToolKind::Fetch
    } else if n.contains("switch_mode")
        || n.contains("switchmode")
        || n.contains("set_mode")
        || n.contains("setmode")
    {
        ToolKind::SwitchMode
    } else {
        ToolKind::Other
    }
}

pub struct SessionNotifier {
    tx: mpsc::UnboundedSender<SessionUpdateEnvelope>,
    session_id: SessionId,
    current_message_id: Mutex<Option<String>>,
    /// Context window size in tokens; when set, usage_update notifications are emitted.
    context_window_size: Option<u64>,
    /// Session-level billing token accumulator. When set, each `UsageUpdate`
    /// notification carries an extra `_meta.token_usage` field with cumulative
    /// input/output/cached/total tokens across all LLM calls in this prompt.
    usage_acc: Option<Arc<Mutex<TurnUsage>>>,
    /// High-frequency usage tracker for real-time token updates.
    high_freq_tracker: Arc<Mutex<Option<HighFreqUsageTracker>>>,
    /// Buffer for tool call events to ensure they are sent after text content,
    /// maintaining consistent order with history replay.
    pending_tool_calls: Mutex<Vec<StreamUpdate>>,
}

impl SessionNotifier {
    pub fn new(tx: mpsc::UnboundedSender<SessionUpdateEnvelope>, session_id: SessionId) -> Self {
        Self {
            tx,
            session_id,
            current_message_id: Mutex::new(None),
            context_window_size: None,
            usage_acc: None,
            high_freq_tracker: Arc::new(Mutex::new(None)),
            pending_tool_calls: Mutex::new(Vec::new()),
        }
    }

    /// Set the context window size for `usage_update` notifications.
    /// When set, each `StreamEvent::Usage` emits an `UsageUpdate` to the client.
    pub fn with_context_window_size(mut self, size: u64) -> Self {
        self.context_window_size = Some(size);
        self
    }

    /// Attach a session-level token accumulator so each `UsageUpdate` notification
    /// carries an ACP `_meta.token_usage` payload with cumulative billing tokens
    /// across all LLM calls in the prompt.
    pub(crate) fn with_usage_acc(mut self, acc: Arc<Mutex<TurnUsage>>) -> Self {
        self.usage_acc = Some(acc);
        self
    }

    /// Snapshot the current token accumulator into an ACP `_meta` payload.
    /// Returns `None` when no accumulator is wired or no usage has been
    /// observed yet (to avoid emitting an empty `_meta` block).
    fn snapshot_token_usage_meta(&self) -> Option<Meta> {
        let acc = self.usage_acc.as_ref()?;
        let guard = acc.lock().ok()?;
        if guard.total_tokens == 0 && guard.input_tokens == 0 && guard.output_tokens == 0 {
            return None;
        }
        let mut map = Meta::new();
        map.insert(
            "token_usage".to_string(),
            serde_json::json!({
                "input_tokens": guard.input_tokens,
                "output_tokens": guard.output_tokens,
                "total_tokens": guard.total_tokens,
                "cached_tokens": guard.cached_tokens,
            }),
        );
        Some(map)
    }

    pub async fn send_event(&self, event: &TypedAnyStreamEvent) {
        let mut updates = anureo_event_to_updates(event);

        // 处理高频更新
        if let Some(size) = self.context_window_size {
            if let Some(usage_delta) = extract_usage_delta(event) {
                // 在 await 之前释放锁
                let update_info_opt = {
                    let mut tracker = self.high_freq_tracker.lock().unwrap();
                    tracker.as_mut().and_then(|t| t.update_tokens(usage_delta))
                };

                if let Some(update_info) = update_info_opt {
                    // 发送高频更新
                    self.send_usage_update(
                        update_info.used,
                        update_info.size,
                        update_info.increment,
                    )
                    .await;
                } else {
                    // 降级到原始逻辑
                    if let Some(used) = extract_usage_tokens(event) {
                        let meta = self.snapshot_token_usage_meta();
                        updates.push(StreamUpdate::UsageUpdate { used, size, meta });
                    }
                }
            }
        }

        // 处理工具调用顺序控制
        let has_text_content = updates.iter().any(|u| {
            matches!(
                u,
                StreamUpdate::AgentMessageChunk { .. } | StreamUpdate::AgentThoughtChunk { .. }
            )
        });

        // 如果当前事件包含文本内容，先发送缓冲的工具调用
        if has_text_content {
            self.flush_pending_tool_calls().await;
        }

        for u in updates {
            let u = self.inject_message_id(u);

            // 缓冲工具调用事件，延迟发送
            if matches!(u, StreamUpdate::ToolCallStarted { .. }) {
                self.buffer_tool_call(u);
                continue;
            }

            if let Some(notif) = stream_update_to_session_notification(&self.session_id, &u) {
                if let Err(e) = self.tx.send(SessionUpdateEnvelope::Session(notif)) {
                    tracing::error!(session_id = %self.session_id, error = %e, "Failed to send stream event notification");
                }
            }
        }
    }

    pub fn try_send_event(&self, event: &TypedAnyStreamEvent) {
        let mut updates = anureo_event_to_updates(event);
        if let Some(size) = self.context_window_size {
            if let Some(used) = extract_usage_tokens(event) {
                let meta = self.snapshot_token_usage_meta();
                updates.push(StreamUpdate::UsageUpdate { used, size, meta });
            }
        }
        self.send_updates(updates);
    }

    pub fn try_send_stream_event(&self, event: &agent::run::TypedAnyStreamEvent) {
        self.try_send_event(event);
    }

    fn send_updates(&self, updates: Vec<StreamUpdate>) {
        for u in updates {
            let u = self.inject_message_id(u);
            if let Some(notif) = stream_update_to_session_notification(&self.session_id, &u) {
                match self.tx.send(SessionUpdateEnvelope::Session(notif)) {
                    Ok(_) => {
                        tracing::trace!(
                            session_id = %self.session_id,
                            update_type = ?u,
                            "Session notification sent successfully"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            session_id = %self.session_id,
                            update_type = ?u,
                            error = %e,
                            "Failed to send session notification (channel closed)"
                        );
                    }
                }
            }
        }
    }

    fn inject_message_id(&self, update: StreamUpdate) -> StreamUpdate {
        match update {
            StreamUpdate::AgentMessageChunk { text, .. } => {
                let id = self
                    .current_message_id
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| Uuid::new_v4().to_string())
                    .clone();
                StreamUpdate::AgentMessageChunk {
                    text,
                    message_id: Some(id),
                }
            }
            StreamUpdate::AgentThoughtChunk { text, .. } => {
                let id = self
                    .current_message_id
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| Uuid::new_v4().to_string())
                    .clone();
                StreamUpdate::AgentThoughtChunk {
                    text,
                    message_id: Some(id),
                }
            }
            StreamUpdate::UserMessageChunk { text, .. } => {
                let id = self
                    .current_message_id
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| Uuid::new_v4().to_string())
                    .clone();
                StreamUpdate::UserMessageChunk {
                    text,
                    message_id: Some(id),
                }
            }
            other => {
                *self.current_message_id.lock().unwrap() = None;
                other
            }
        }
    }

    pub fn try_send_plan(&self, entries: Vec<PlanEntry>) {
        let notif = stream_update_to_session_notification(
            &self.session_id,
            &StreamUpdate::Plan { entries },
        );
        if let Some(notif) = notif {
            if let Err(e) = self.tx.send(SessionUpdateEnvelope::Session(notif)) {
                tracing::warn!(
                    session_id = %self.session_id,
                    error = %e,
                    "Failed to send plan notification"
                );
            }
        }
    }

    /// Convert one persisted LLM message into the ordered ACP session
    /// updates used for history replay (both the `session/load` tail replay
    /// and the `_anureo.dev/session-history/page` extension response).
    ///
    /// Returns `None` for System messages and empty assistant turns — they
    /// carry nothing user-visible for a replaying client.
    pub fn message_session_updates(
        session_id: &str,
        message_index: usize,
        message: &Message,
    ) -> Option<Vec<SessionUpdate>> {
        let message_id = MessageId::new(format!("history:{session_id}:{message_index}"));
        match message {
            Message::User(content) => Some(vec![SessionUpdate::UserMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(
                    content.as_text().to_string(),
                )))
                .message_id(Some(message_id)),
            )]),
            Message::Assistant(payload) => {
                let is_empty_assistant = payload.content.trim().is_empty()
                    && payload
                        .reasoning_content
                        .as_ref()
                        .is_none_or(|s| s.trim().is_empty())
                    && payload.tool_calls.is_empty();
                if is_empty_assistant {
                    return None;
                }

                let mut updates = Vec::new();

                // Reasoning first so thinking renders above the assistant
                // reply, matching live streaming order where ReasoningDelta
                // arrives before TextDelta.
                if let Some(ref reasoning) = payload.reasoning_content {
                    if !reasoning.trim().is_empty() {
                        updates.push(SessionUpdate::AgentThoughtChunk(
                            ContentChunk::new(reasoning.clone().into())
                                .message_id(Some(message_id.clone())),
                        ));
                    }
                }

                updates.push(SessionUpdate::AgentMessageChunk(
                    ContentChunk::new(payload.content.clone().into()).message_id(Some(message_id)),
                ));

                for tc in &payload.tool_calls {
                    let args = serde_json::from_str::<Value>(&tc.arguments).ok();
                    let tool_call = create_tool_call(&tc.id, &tc.name, args.as_ref(), None);
                    updates.push(SessionUpdate::ToolCall(tool_call));
                }

                Some(updates)
            }
            Message::Tool {
                tool_call_id,
                content,
            } => {
                let id = ToolCallId::new(tool_call_id.clone());
                let acp_content = match content {
                    tool_core::ToolCallContent::Text(t) => {
                        ToolCallContent::from(ContentBlock::Text(TextContent::new(t.clone())))
                    }
                    tool_core::ToolCallContent::Diff {
                        path,
                        old_text,
                        new_text,
                    } => ToolCallContent::Diff(
                        Diff::new(path.clone(), new_text.clone()).old_text(old_text.clone()),
                    ),
                    tool_core::ToolCallContent::Terminal { terminal_id } => {
                        ToolCallContent::Terminal(Terminal::new(TerminalId::new(
                            terminal_id.clone(),
                        )))
                    }
                };
                let fields = ToolCallUpdateFields::new()
                    .status(ToolCallStatus::Completed)
                    .content(vec![acp_content])
                    .raw_output(tool_call_content_to_raw_output(content));
                Some(vec![SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    id, fields,
                ))])
            }
            Message::System(_) => None,
        }
    }

    pub async fn send_history(
        &self,
        messages: &[Message],
        start_index: usize,
        recovery_replay: bool,
    ) {
        tracing::debug!(
            session_id = %self.session_id,
            total_messages = messages.len(),
            "send_history started"
        );
        // Wrap the replay batch in begin/end markers so the runtime ingress
        // loop suppresses `session.updated` global broadcasts for every
        // replayed message (a load is a read, not a mutation).
        let _ = self.tx.send(SessionUpdateEnvelope::Session(replay_marker(
            REPLAY_BEGIN_MARKER_PREFIX,
            &self.session_id,
        )));
        let mut sent_count: usize = 0;
        let mut skipped_system: usize = 0;

        // Priority #13 gap (Hermes `hermes_state.py` #10): strip the
        // curator's `<background_review>...</background_review>`
        // harness from each Message variant before sending. Round-2
        // only wrapped the two `extract_session_text` text-returning
        // helpers; this site takes `&[Message]` directly and was
        // skipped, leaving a forked-review leak able to reach
        // user-visible ACP notifications. The walker is
        // `anureo_llm::message::strip_background_review_in_messages`
        // and is ContentKind-aware (User::Text / Multimodal parts /
        // Assistant payload + tool_calls args / Tool content / System).
        let mut owned: Vec<Message> = messages.to_vec();
        anureo_llm::message::strip_background_review_in_messages(&mut owned);
        let messages: &[Message] = &owned;

        let mut replay_updates: Vec<SessionUpdate> = Vec::new();
        for (idx, message) in messages.iter().enumerate() {
            let msg_type = match message {
                Message::User(_) => "user",
                Message::Assistant(_) => "assistant",
                Message::Tool { .. } => "tool",
                Message::System(_) => "system",
            };
            let stable_index = start_index + idx;
            let updates = match Self::message_session_updates(
                &self.session_id.to_string(),
                stable_index,
                message,
            ) {
                Some(updates) => updates,
                None => {
                    if matches!(message, Message::System(_)) {
                        skipped_system += 1;
                    }
                    continue;
                }
            };

            tracing::trace!(
                session_id = %self.session_id,
                index = stable_index,
                msg_type = msg_type,
                notification_count = updates.len(),
                "Replaying history message"
            );

            replay_updates.extend(updates);
        }

        if !recovery_replay && history_batch_enabled() {
            // One envelope → one `_anureo.dev/session-history/batch`
            // JSON-RPC notification on the wire. The client applies the
            // whole tail in a single commit instead of N frames.
            let update_count = replay_updates.len();
            if let Err(e) = self.tx.send(SessionUpdateEnvelope::HistoryBatch {
                session_id: self.session_id.clone(),
                updates: replay_updates,
            }) {
                tracing::error!(
                    session_id = %self.session_id,
                    error = %e,
                    "Failed to send batched history replay"
                );
            } else {
                sent_count = update_count;
            }
        } else {
            for update in replay_updates {
                let notif = SessionNotification::new(self.session_id.clone(), update);
                let envelope = SessionUpdateEnvelope::SessionReplay {
                    notification: notif,
                    recovery: recovery_replay,
                };
                if let Err(e) = self.tx.send(envelope) {
                    tracing::error!(session_id = %self.session_id, error = %e, "Failed to send session update during history replay");
                } else {
                    sent_count += 1;
                }
            }
        }

        tracing::debug!(
            session_id = %self.session_id,
            total_messages = messages.len(),
            notifications_sent = sent_count,
            system_skipped = skipped_system,
            "send_history completed"
        );
        let _ = self.tx.send(SessionUpdateEnvelope::Session(replay_marker(
            REPLAY_END_MARKER_PREFIX,
            &self.session_id,
        )));
    }

    pub async fn send_current_mode(&self, mode_id: &str) {
        let notif = SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::new(
                mode_id.to_string(),
            ))),
        );
        if let Err(e) = self.tx.send(SessionUpdateEnvelope::Session(notif)) {
            tracing::error!(session_id = %self.session_id, error = %e, "Failed to send current mode update");
        }
    }

    pub fn try_send_current_mode(&self, mode_id: &str) {
        let notif = SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(SessionModeId::new(
                mode_id.to_string(),
            ))),
        );
        let _ = self.tx.send(SessionUpdateEnvelope::Session(notif));
    }

    pub fn try_send_session_info_update(&self, title: &str) {
        let notif = SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::SessionInfoUpdate(SessionInfoUpdate::new().title(title.to_string())),
        );
        if let Err(e) = self.tx.send(SessionUpdateEnvelope::Session(notif)) {
            tracing::warn!(
                session_id = %self.session_id,
                error = %e,
                "Failed to send session title update"
            );
        }
    }

    /// Send a session metadata update with an `_meta` payload (no title change).
    /// Used by background-review completion to surface `_meta.review` to the
    /// session list without disturbing the chat stream.
    pub fn try_send_session_meta(&self, meta: Meta) {
        let mut info = SessionInfoUpdate::new();
        info = info.meta(meta);
        let notif = SessionNotification::new(
            self.session_id.clone(),
            SessionUpdate::SessionInfoUpdate(info),
        );
        if let Err(e) = self.tx.send(SessionUpdateEnvelope::Session(notif)) {
            tracing::warn!(
                session_id = %self.session_id,
                error = %e,
                "Failed to send session info update with _meta payload"
            );
        }
    }

    /// Enable high-frequency usage tracking mode.
    /// When enabled, token usage updates are sent more frequently based on
    /// incremental thresholds, time intervals, and percentage thresholds.
    pub fn enable_high_freq_tracking(&self, base_used: u64, size: u64) {
        let mut tracker = self.high_freq_tracker.lock().unwrap();
        *tracker = Some(HighFreqUsageTracker::new(base_used, size));
    }

    /// Enable high-frequency usage tracking with custom configuration.
    pub fn enable_high_freq_tracking_with_config(
        &self,
        base_used: u64,
        size: u64,
        min_increment: u64,
        min_interval_ms: u64,
    ) {
        let mut tracker = self.high_freq_tracker.lock().unwrap();
        *tracker = Some(HighFreqUsageTracker::with_config(
            base_used,
            size,
            min_increment,
            min_interval_ms,
        ));
    }

    /// Enable high-frequency usage tracking with custom percentage thresholds.
    pub fn enable_high_freq_tracking_with_thresholds(
        &self,
        base_used: u64,
        size: u64,
        thresholds: Vec<f64>,
    ) {
        let mut tracker = self.high_freq_tracker.lock().unwrap();
        *tracker = Some(HighFreqUsageTracker::with_custom_thresholds(
            base_used, size, thresholds,
        ));
    }

    /// Disable high-frequency usage tracking mode and send final update.
    pub async fn disable_high_freq_tracking(&self) {
        // Acquire the update data within lock scope
        let update_opt = {
            let mut tracker_opt = self.high_freq_tracker.lock().unwrap();
            let result = if let Some(tracker) = tracker_opt.as_mut() {
                tracker
                    .force_update()
                    .map(|info| (info.used, info.size, info.increment))
            } else {
                None
            };
            // Clear the tracker
            *tracker_opt = None;
            result
        };

        // Send update outside of lock scope
        if let Some(update_data) = update_opt {
            self.send_usage_update(update_data.0, update_data.1, update_data.2)
                .await;
        }
    }

    /// Get current high-frequency tracker status.
    pub fn get_high_freq_tracker_status(&self) -> Option<(u64, u64, f64)> {
        let tracker = self.high_freq_tracker.lock().unwrap();
        tracker.as_ref().map(|t| {
            (
                t.get_current_usage(),
                t.get_size(),
                t.get_usage_percentage(),
            )
        })
    }

    /// Test-only helper: update tokens on the internal high-freq tracker.
    #[cfg(test)]
    pub(crate) fn test_update_high_freq_tokens(
        &self,
        delta: u64,
    ) -> Option<crate::high_freq_usage::UsageUpdateInfo> {
        let mut tracker = self.high_freq_tracker.lock().unwrap();
        tracker.as_mut().and_then(|t| t.update_tokens(delta))
    }

    /// Test-only helper: adjust frequency based on system load.
    #[cfg(test)]
    pub(crate) fn test_adjust_freq_based_on_load(&self, system_load: f64) {
        let mut tracker = self.high_freq_tracker.lock().unwrap();
        if let Some(t) = tracker.as_mut() {
            t.adjust_frequency_based_on_load(system_load);
        }
    }

    /// Test-only helper: check if high-freq tracker is enabled.
    #[cfg(test)]
    pub(crate) fn test_is_high_freq_enabled(&self) -> bool {
        let tracker = self.high_freq_tracker.lock().unwrap();
        tracker.is_some()
    }

    /// Buffer a tool call event for delayed sending to maintain proper order.
    fn buffer_tool_call(&self, update: StreamUpdate) {
        let mut pending = self.pending_tool_calls.lock().unwrap();
        pending.push(update);
    }

    /// Send all buffered tool call events and clear the buffer.
    async fn flush_pending_tool_calls(&self) {
        let pending = {
            let mut buffer = self.pending_tool_calls.lock().unwrap();
            std::mem::take(&mut *buffer)
        };

        for update in pending {
            if let Some(notif) = stream_update_to_session_notification(&self.session_id, &update) {
                if let Err(e) = self.tx.send(SessionUpdateEnvelope::Session(notif)) {
                    tracing::error!(
                        session_id = %self.session_id,
                        error = %e,
                        "Failed to send buffered tool call notification"
                    );
                }
            }
        }
    }

    /// Send usage update notification with enhanced metadata.
    async fn send_usage_update(&self, used: u64, size: u64, increment: u64) {
        let meta = self.snapshot_token_usage_meta();
        let mut extended_meta = meta.unwrap_or_default();

        // Add high-frequency tracking metadata
        extended_meta.insert("increment".to_string(), serde_json::json!(increment));
        extended_meta.insert(
            "timestamp".to_string(),
            serde_json::json!(std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()),
        );
        extended_meta.insert("precision".to_string(), serde_json::json!("incremental"));
        extended_meta.insert("source".to_string(), serde_json::json!("high_freq_tracker"));

        let update = StreamUpdate::UsageUpdate {
            used,
            size,
            meta: Some(extended_meta),
        };
        if let Some(notif) = stream_update_to_session_notification(&self.session_id, &update) {
            if let Err(e) = self.tx.send(SessionUpdateEnvelope::Session(notif)) {
                tracing::error!(session_id = %self.session_id, error = %e,
                    "Failed to send high-freq usage update");
            }
        }
    }
}

/// Extract prompt-token count from a anureo stream event if it is a Usage event.
fn extract_usage_tokens(ev: &TypedAnyStreamEvent) -> Option<u64> {
    let usage = match ev {
        TypedAnyStreamEvent::React(e) => extract_usage_inner(e),
        TypedAnyStreamEvent::Dup(e) => extract_usage_inner(e),
        TypedAnyStreamEvent::Tot(e) => extract_usage_inner(e),
        TypedAnyStreamEvent::Got(e) => extract_usage_inner(e),
    };
    usage.map(|p| p as u64)
}

/// Extract token usage delta from a anureo stream event for high-frequency tracking.
fn extract_usage_delta(ev: &TypedAnyStreamEvent) -> Option<u64> {
    match ev {
        TypedAnyStreamEvent::React(e) => extract_usage_delta_inner(e),
        TypedAnyStreamEvent::Dup(e) => extract_usage_delta_inner(e),
        TypedAnyStreamEvent::Tot(e) => extract_usage_delta_inner(e),
        TypedAnyStreamEvent::Got(e) => extract_usage_delta_inner(e),
    }
}

fn extract_usage_delta_inner<S>(ev: &StreamEvent<S>) -> Option<u64>
where
    S: Clone + Send + Sync + std::fmt::Debug + 'static,
{
    match ev {
        StreamEvent::TurnFinish { usage, .. } => {
            let total = (usage.input + usage.output) as u64;
            let cached = usage.cache_read.unwrap_or(0) as u64;
            Some(total.saturating_sub(cached))
        }
        _ => None,
    }
}

fn extract_usage_inner<S>(ev: &StreamEvent<S>) -> Option<u32>
where
    S: Clone + Send + Sync + std::fmt::Debug + 'static,
{
    match ev {
        StreamEvent::TurnFinish { usage, .. } => Some(usage.input),
        _ => None,
    }
}

fn tool_call_content_to_raw_output(content: &tool_core::ToolCallContent) -> Value {
    match content {
        tool_core::ToolCallContent::Text(text) => serde_json::json!(text),
        tool_core::ToolCallContent::Diff {
            path,
            old_text,
            new_text,
        } => serde_json::json!({
            "type": "diff",
            "path": path,
            "oldText": old_text,
            "newText": new_text,
        }),
        tool_core::ToolCallContent::Terminal { terminal_id } => serde_json::json!({
            "type": "terminal",
            "terminalId": terminal_id,
        }),
    }
}

pub fn create_tool_call(
    tool_call_id: &str,
    name: &str,
    input: Option<&Value>,
    kind_override: Option<&str>,
) -> ToolCall {
    let id = ToolCallId::new(tool_call_id);
    let title = generate_tool_title(name, input);
    let effective_kind = kind_override
        .map(name_to_tool_kind)
        .unwrap_or_else(|| name_to_tool_kind(name));
    let mut tc = ToolCall::new(id.clone(), title)
        .status(ToolCallStatus::Pending)
        .kind(effective_kind);
    // The UI keys specialized rendering (icons, previews, collapsed summaries)
    // off the TOOL NAME, while `kind` is only a canonical category ("execute",
    // "edit", "other"). Expose the raw name via _meta so clients can use it.
    let mut meta = Meta::new();
    meta.insert("toolName".to_string(), Value::String(name.to_string()));
    tc = tc.meta(meta);

    if let Some(v) = input {
        tc = tc.raw_input(v.clone());
        let locations: Vec<ToolCallLocation> = extract_locations(name, v)
            .into_iter()
            .map(|loc| ToolCallLocation::new(loc.path).line(loc.line))
            .collect();
        if !locations.is_empty() {
            tc = tc.locations(locations);
        }
    }
    tc
}

pub fn generate_tool_title(name: &str, input: Option<&serde_json::Value>) -> String {
    let kind = name_to_tool_kind(name);
    let target = extract_target_from_input(name, input);

    match kind {
        // Execute and Other: show command directly without "Running" prefix
        ToolKind::Execute | ToolKind::Other => target.unwrap_or_else(|| name.to_string()),
        // Others: use verb prefix
        _ => {
            let verb = match kind {
                ToolKind::Read => "Reading",
                ToolKind::Edit => "Editing",
                ToolKind::Delete => "Deleting",
                ToolKind::Move => "Moving",
                ToolKind::Search => "Searching",
                ToolKind::Think => "Thinking",
                ToolKind::Fetch => "Fetching",
                ToolKind::SwitchMode => "Switching mode",
                // Execute/Other are handled above; Unknown/future variants
                // (ToolKind is #[non_exhaustive]) fall back to the tool name.
                _ => return target.unwrap_or_else(|| name.to_string()),
            };
            match target {
                Some(t) => format!("{} {}", verb, t),
                None => format!("{} {}", verb, name),
            }
        }
    }
}

fn extract_target_from_input(name: &str, input: Option<&serde_json::Value>) -> Option<String> {
    let obj = input?.as_object()?;
    let n = name.to_lowercase();

    let keys: &[&[&str]] = if n.contains("read")
        || n.contains("file")
        || n.contains("write")
        || n.contains("edit")
        || n.contains("delete")
        || n.contains("remove")
    {
        &[&["path", "file_path", "filepath"]]
    } else if n.contains("move") || n.contains("rename") {
        &[
            &["source", "src", "path"],
            &["destination", "dest", "target"],
        ]
    } else if n.contains("search") || n.contains("grep") || n.contains("glob") {
        &[&["pattern", "query", "search"]]
    } else if n.contains("run")
        || n.contains("bash")
        || n.contains("command")
        || n.contains("exec")
        || n.contains("shell")
    {
        &[&["command", "cmd"]]
    } else if n.contains("fetch") {
        &[&["url", "uri"]]
    } else if n == "agent" || (n.contains("invoke") && n.contains("agent")) {
        // Special handling for agent tool: extract agent name
        if let Some(agent) = obj.get("agent").and_then(|v| v.as_str()) {
            return Some(format!("agent: {}", agent));
        }
        &[]
    } else {
        &[]
    };

    for key_group in keys {
        for &key in *key_group {
            if let Some(val) = obj.get(key).and_then(|v| v.as_str()) {
                // Commands should not be truncated - they're the primary information
                let display = if key == "command" || key == "cmd" {
                    val.to_string()
                } else {
                    truncate_tail(val, 60)
                };
                return Some(display);
            }
        }
    }
    None
}

#[cfg(test)]
mod token_usage_meta_tests {
    use super::*;
    use crate::agent::capture_turn_usage;
    use crate::agent::TurnUsage;

    use stream_event::StreamEvent;

    /// Capture should leave the snapshot empty when no LLM usage has been observed.
    #[test]
    fn snapshot_meta_is_none_when_acc_is_empty() {
        let (tx, _rx) = mpsc::unbounded_channel::<SessionUpdateEnvelope>();
        let acc: Arc<Mutex<TurnUsage>> = Arc::new(Mutex::new(TurnUsage::default()));
        let notifier = SessionNotifier::new(tx, SessionId::new("sess"))
            .with_context_window_size(8192)
            .with_usage_acc(acc);
        assert!(notifier.snapshot_token_usage_meta().is_none());
    }

    /// Capture then snapshot: meta must contain all four billing fields.
    #[test]
    fn snapshot_meta_includes_all_billing_fields_after_capture() {
        let (tx, _rx) = mpsc::unbounded_channel::<SessionUpdateEnvelope>();
        let acc: Arc<Mutex<TurnUsage>> = Arc::new(Mutex::new(TurnUsage::default()));
        let notifier = SessionNotifier::new(tx, SessionId::new("sess"))
            .with_context_window_size(8192)
            .with_usage_acc(acc.clone());

        // Inject a synthetic Usage event covering all four fields.
        let ev = TypedAnyStreamEvent::React(StreamEvent::TurnFinish {
            reason: "stop".to_string(),
            usage: stream_event::Usage {
                input: 100,
                output: 50,
                reasoning: None,
                cache_read: Some(20),
                cache_write: None,
            },
        });
        capture_turn_usage(&ev, &acc);

        let meta = notifier.snapshot_token_usage_meta().expect("meta present");
        let token_usage = meta
            .get("token_usage")
            .expect("token_usage key present")
            .as_object()
            .expect("token_usage is object");
        assert_eq!(token_usage["input_tokens"], serde_json::json!(100));
        assert_eq!(token_usage["output_tokens"], serde_json::json!(50));
        assert_eq!(token_usage["total_tokens"], serde_json::json!(150));
        assert_eq!(token_usage["cached_tokens"], serde_json::json!(20));
    }

    /// Multi-turn accumulation must monotonically grow the snapshot.
    #[test]
    fn snapshot_meta_grows_across_multiple_llm_calls() {
        let (tx, _rx) = mpsc::unbounded_channel::<SessionUpdateEnvelope>();
        let acc: Arc<Mutex<TurnUsage>> = Arc::new(Mutex::new(TurnUsage::default()));
        let notifier = SessionNotifier::new(tx, SessionId::new("sess"))
            .with_context_window_size(8192)
            .with_usage_acc(acc.clone());

        for (prompt, completion, cached) in [
            (100u32, 50, Some(20u32)),
            (200, 80, None),
            (50, 30, Some(10)),
        ] {
            let ev = TypedAnyStreamEvent::React(StreamEvent::TurnFinish {
                reason: "stop".to_string(),
                usage: stream_event::Usage {
                    input: prompt,
                    output: completion,
                    reasoning: None,
                    cache_read: cached,
                    cache_write: None,
                },
            });
            capture_turn_usage(&ev, &acc);
        }

        let meta = notifier.snapshot_token_usage_meta().expect("meta present");
        let tu = meta["token_usage"].as_object().unwrap();
        assert_eq!(tu["input_tokens"], serde_json::json!(350));
        assert_eq!(tu["output_tokens"], serde_json::json!(160));
        assert_eq!(tu["total_tokens"], serde_json::json!(510));
        assert_eq!(tu["cached_tokens"], serde_json::json!(30));
    }

    /// StreamUpdate -> SessionNotification serialization must carry _meta.token_usage
    /// when the notifier is wired with an accumulator and a Usage event arrives.
    #[tokio::test]
    async fn notifier_emits_usage_update_with_token_usage_meta() {
        let (tx, mut rx) = mpsc::unbounded_channel::<SessionUpdateEnvelope>();
        let acc: Arc<Mutex<TurnUsage>> = Arc::new(Mutex::new(TurnUsage::default()));
        let notifier = SessionNotifier::new(tx, SessionId::new("sess"))
            .with_context_window_size(8192)
            .with_usage_acc(acc.clone());

        let ev = TypedAnyStreamEvent::React(StreamEvent::TurnFinish {
            reason: "stop".to_string(),
            usage: stream_event::Usage {
                input: 4096,
                output: 512,
                reasoning: None,
                cache_read: Some(1024),
                cache_write: None,
            },
        });
        capture_turn_usage(&ev, &acc);
        notifier.try_send_event(&ev);

        // Drain and inspect the last notification.
        let mut last: Option<SessionNotification> = None;
        while let Ok(n) = rx.try_recv() {
            if let SessionUpdateEnvelope::Session(session_notif) = n {
                last = Some(session_notif);
            }
        }
        let notif = last.expect("at least one notification");
        let raw = serde_json::to_value(&notif).expect("serialize notification");
        let update = &raw["update"];
        assert_eq!(update["sessionUpdate"], "usage_update");
        assert_eq!(update["used"], 4096);
        assert_eq!(update["size"], 8192);
        let meta = update["_meta"].as_object().expect("_meta is object");
        assert!(meta.contains_key("token_usage"));
        assert_eq!(meta["token_usage"]["input_tokens"], serde_json::json!(4096));
        assert_eq!(meta["token_usage"]["output_tokens"], serde_json::json!(512));
        assert_eq!(meta["token_usage"]["total_tokens"], serde_json::json!(4608));
        assert_eq!(
            meta["token_usage"]["cached_tokens"],
            serde_json::json!(1024)
        );
    }

    /// Without `with_usage_acc`, the notification should still go out but
    /// without `_meta.token_usage` (backward compatibility).
    #[tokio::test]
    async fn notifier_without_acc_omits_token_usage_meta() {
        let (tx, mut rx) = mpsc::unbounded_channel::<SessionUpdateEnvelope>();
        let notifier =
            SessionNotifier::new(tx, SessionId::new("sess")).with_context_window_size(8192);

        let ev = TypedAnyStreamEvent::React(StreamEvent::TurnFinish {
            reason: "stop".to_string(),
            usage: stream_event::Usage {
                input: 100,
                output: 20,
                reasoning: None,
                cache_read: Some(0),
                cache_write: None,
            },
        });
        notifier.try_send_event(&ev);

        let mut last: Option<SessionNotification> = None;
        while let Ok(n) = rx.try_recv() {
            if let SessionUpdateEnvelope::Session(session_notif) = n {
                last = Some(session_notif);
            }
        }
        let notif = last.expect("at least one notification");
        let raw = serde_json::to_value(&notif).expect("serialize");
        let update = &raw["update"];
        assert_eq!(update["used"], 100);
        // No _meta, or _meta present but no token_usage key — either is acceptable.
        let has_token_usage = update["_meta"]["token_usage"].is_object();
        assert!(
            !has_token_usage,
            "_meta.token_usage must not be present without acc"
        );
    }

    #[test]
    fn history_message_identity_is_stable_and_index_scoped() {
        let message = Message::user("same content");
        let first =
            SessionNotifier::message_session_updates("session-a", 7, &message).expect("updates");
        let repeated =
            SessionNotifier::message_session_updates("session-a", 7, &message).expect("updates");
        let other_index =
            SessionNotifier::message_session_updates("session-a", 8, &message).expect("updates");
        assert_eq!(
            serde_json::to_value(&first).expect("serialize"),
            serde_json::to_value(&repeated).expect("serialize")
        );
        assert_ne!(
            serde_json::to_value(&first).expect("serialize"),
            serde_json::to_value(&other_index).expect("serialize")
        );
    }
}

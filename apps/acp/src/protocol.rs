//! # ACP protocol and anureo mapping summary
//!
//! This section condenses [Agent Client Protocol](https://agentclientprotocol.com) request/response,
//! behavior, and phasing relevant to the anureo implementation into rustdoc.
//!
//! ## Transport (stdio)
//!
//! - Only **stdio** is implemented: newline-delimited JSON-RPC 2.0, one complete JSON per line (Request/Response/Notification).
//! - Flow: Client -> stdin -> anureo acp; anureo acp -> stdout -> Client. **stderr is for logs only**; no JSON-RPC on stderr.
//! - Use `agent_client_protocol::AgentSideConnection::new(agent, stdin, stdout, spawn)` to drive the I/O loop.
//! - **Entrypoint**: Runs via `anureo acp` subcommand on the same binary as the CLI.
//!
//! ## initialize (capability negotiation)
//!
//! - **When**: Once, after connection and before any other method.
//! - **InitializeRequest**: `protocol_version` (required), `client_capabilities` (optional), `implementation` (optional).
//! - **InitializeResponse**: `protocol_version` (negotiated), `agent_capabilities`/`agent_info`, `auth_methods` (anureo uses `[]`).
//! - **anureo**: Return version, implementation "anureo" + crate version; promptCapabilities at least text + resource_link baseline; loadSession, mcpCapabilities as actually supported.
//!
//! ## authenticate
//!
//! - Client only calls when Agent returns `auth_required`. anureo does not declare auth_methods, so it is never called; implement as immediate success.
//!
//! ## session/new (new session)
//!
//! - **NewSessionRequest**: `working_directory` (optional, absolute path), `mcp_servers` (required).
//! - **NewSessionResponse**: `session_id` (required, Agent-generated).
//! - **anureo**: Generate unique session_id; session_id <-> thread_id 1:1 (can be equal); working_directory -> `RunOptions::working_folder`; iterate mcp_servers to start MCP and register tools.
//! - **MCP**: McpServerStdio (command, args, env) starts subprocess stdio; McpServerHttp/Sse connect as anureo supports. Disconnect that session's MCP when the session is "closed" or process exits; tools are per-session (one MCP set per session), not shared across sessions.
//!
//! ## session/prompt (handle user input)
//!
//! - **PromptRequest**: `session_id`, `content_blocks` (required).
//! - **PromptResponse**: `stop_reason` (Finished | MaxTokens | MaxTurns | Refused | **Cancelled**).
//! - **anureo flow**: Look up session -> content_blocks -> message -> RunOptions -> `run_agent_with_options`; send session/update during stream; request_permission when needed; finally return stop_reason based on cancellation.
//!
//! ## ContentBlock and user message (content module)
//!
//! | Variant        | Description     | anureo support           |
//! |-----------------|-----------------|------------------------|
//! | **Text**        | Plain/Markdown  | Required; concatenate in order |
//! | **ResourceLink**| Resource URI   | Required; e.g. "Reference: …" |
//! | **Image**       | Image           | Optional; needs promptCapabilities.image |
//! | **Audio**       | Audio           | Optional; needs promptCapabilities.audio |
//! | **Resource**    | Embedded resource | Optional; needs embeddedContext |
//!
//! Empty blocks may return `Ok(String::new())` or `Err(EmptyMessage)`.
//!
//! ## session/update (progress and streaming)
//!
//! - **Method**: Notification (Agent -> Client, no response).
//! - **SessionNotification**: `session_id`, `session_update` (SessionUpdate union).
//! - **SessionUpdate variants**: user_message_chunk, **agent_message_chunk**, **agent_thought_chunk**, **tool_call**, **tool_call_update**, plan, available_commands_update, current_mode_update, config_option_update, **session_info_update**.
//! - anureo sources: think output -> agent_message_chunk/agent_thought_chunk; Act decides to call tool -> tool_call (Pending); during/after execution -> tool_call_update (Running/Success/Failure).
//! - **Background Review v2**: post-prompt, `/review-skill`, and
//!   `_anureo.dev/review/start` share one durable four-state projection:
//!   `running | reviewed | skipped | failed`. `_meta.review`,
//!   `review/status`, `review/history`, and `review/changed` serialize the same
//!   `ReviewState` schema. Inline action summaries are capped at 10; complete
//!   persisted actions are paged through `review/details`.
//! - Initialize advertises this surface as
//!   `_meta["anureo.dev"].backgroundReview`. Diff support is advertised as
//!   `false` until curator persists content-level diff data; an
//!   `includeDiff: true` details request is rejected explicitly.
//! - A running row is persisted before the curator thread starts. Every
//!   terminal path writes curator history (including failures) and only then
//!   clears the running row, so reconnecting clients converge via
//!   `session/list` or `review/status` even when best-effort notifications are
//!   lost. On process restart, orphaned running rows become durable failed
//!   records with `fail_reason = "process_restarted"`.
//! - **Background review completion**: When the review finishes, two `session/update` notifications are emitted:
//!   1. `agent_message_chunk` — human-readable summary ("Background review saved 2 memories + 1 skill (1.2s)." or "... skipped (...)"). Uses a random UUID `message_id`; per ACP spec the client renders a new `messageId` as a distinct assistant message.
//!   2. `session_info_update` with the complete v2 `_meta.review` snapshot,
//!      including `review_id`, `trigger`, `started_at`, bounded `actions`, and
//!      `fail_reason` where applicable.
//! - Both notifications are best-effort (`try_send` on the session mpsc channel); review failures never crash the ACP process.
//! - `_anureo.dev/review/{status,history,details,start,cancel}` checks the
//!   durable session owner before reading or mutating review data. Cancellation
//!   is review-local and never forwards `session/cancel` to the main prompt.
//!
//! ## Tool call and SessionUpdate mapping (stream_bridge)
//!
//! - **ToolCall**: tool_call_id, title, kind (ToolKind), status, input, output, content, locations.
//! - **ToolCallUpdate**: All optional except tool_call_id; only changed fields.
//! - **ToolCallStatus**: Pending | Running | Success | Failure.
//! - **Order**: Send ToolCall (Pending) -> if permission needed then request_permission -> if allowed ToolCallUpdate (Running) -> execute -> ToolCallUpdate (Success/Failure + output).
//!
//! ## session/request_permission (tool execution permission)
//!
//! - **Direction**: Agent calls Client (anureo asks IDE for user approval).
//! - **RequestPermissionRequest**: session_id, tool_call_update, permission_options (AllowOnce/AllowAlways/DenyOnce/DenyAlways, etc.).
//! - **RequestPermissionResponse**: outcome is SelectedPermissionOutcome (with permission_option_id) or **Cancelled** (user cancelled or Client sent session/cancel).
//! - anureo: Checks tool against permission policy; await request_permission before executing tool; execute or write denial and return Cancelled based on result.
//!
//! ## session/cancel (cancellation)
//!
//! - **CancelNotification**: session_id (required).
//! - Agent should stop LLM, abort tools, return PromptResponse(StopReason::Cancelled) as soon as possible.
//! - anureo: SessionStore keeps per-session cancel flag; set_cancelled on cancel; poll is_cancelled in prompt path; if there is a pending request_permission, Client responds with Cancelled.
//! - **Fallback**: If anureo has no interruptible run_agent API yet, check cancel flag on next poll or node entry and return Cancelled, or add an extension point.
//!
//! ## session/load
//!
//! - Only when capabilities.loadSession: true. Request: session_id, working_directory, mcp_servers.
//! - Agent uses request **session_id as thread_id** to load messages/state from storage; send **user_message_chunk / agent_message_chunk** etc. via session/update until history is sent; connect mcp_servers from request; return LoadSessionResponse.
//!
//! ## session/list
//!
//! - Only when capabilities.sessionCapabilities.list is present. Request: optional `cwd` (filter by working directory), optional `cursor` (pagination).
//! - Agent reads the owner-scoped `acp_sessions` SessionIndex; the checkpoint
//!   database is history storage, not a second membership/order source. The
//!   response contains active records for the authenticated owner and optional
//!   cwd, projected to standard ACP fields (`sessionId`, `cwd`, `title`,
//!   `updatedAt`).
//! - The compatibility implementation intentionally returns one complete page
//!   with `nextCursor = null`; private anureo Desk pagination is provided by
//!   `_anureo.dev/session/list` and its immutable snapshot cursor.
//!
//! ## Session mode and session config
//!
//! - Mode: e.g. ask/architect/code, mappable to ReAct/DUP/ToT/GoT; set_session_mode switches RunCmd.
//! - Config: model, max_tokens, etc.; set_session_config_option sends it; inject into RunOptions/config at prompt time.
//! - Model switching has no dedicated RPC in agent-client-protocol 0.15.x (SetSessionModelRequest
//!   was removed in schema 0.14.0). It is handled through set_session_config_option("model", ...),
//!   which is the same path that exposes the model as a SessionConfigOption with category="model".
//!
//! ## session/fork
//!
//! - Only when capabilities.sessionCapabilities.fork is present. Clones current session into a new one (same config, new ID).
//! - Copies session_config (model, mode) to the forked session.
//! - Does NOT copy conversation history (that's load_session's job).
//!
//! ## Client capabilities (fs, terminal)
//!
//! - fs/read_text_file, fs/write_text_file, terminal/* (create, output, kill, release, wait_for_exit) are Agent->Client requests; only available when Client declares them in initialize. May request_permission first then call or fall back to anureo local execution. **When calling**, anureo side must hold AgentSideConnection (implementing ACP Client trait) and await its `read_text_file` / `write_text_file` / `create_terminal` etc.
//!
//! ## Errors and JSON-RPC error codes
//!
//! - On failure return JSON-RPC 2.0 **Error**: `code` (integer or ACP predefined), `message` (human-readable), optional `data`. ACP defines **ErrorCode** enum (e.g. invalid_params, method_not_found, auth_required); see protocol schema.
//! - Invalid session_id -> invalid_params, "unknown session".
//! - content_blocks parse failure -> invalid_params.
//! - run_agent internal error -> server-style error, message with brief reason; avoid putting stack in message (use data or logs only).
//! - **After cancel must return StopReason::Cancelled**, not Finished.
//!
//! ## References
//!
//! - [ACP Architecture](https://agentclientprotocol.com/overview/architecture)
//! - [Protocol schema](https://agentclientprotocol.com/protocol/schema)
//! - [Initialization](https://agentclientprotocol.com/protocol/initialization)
//! - [Session Setup](https://agentclientprotocol.com/protocol/session-setup)
//! - [Prompt Turn](https://agentclientprotocol.com/protocol/prompt-turn)
//! - [Transports (stdio)](https://agentclientprotocol.com/protocol/transports)
//! - [agent-client-protocol (crates.io)](https://crates.io/crates/agent-client-protocol)
//! - [JetBrains ACP Registry](https://blog.jetbrains.com/ai/2026/01/acp-agent-registry/)

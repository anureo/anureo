//! # anureo ACP — Run anureo as an ACP Agent
//!
//! This crate implements the **Agent side** of the [Agent Client Protocol (ACP)](https://agentclientprotocol.com).
//! IDEs (Zed, JetBrains, Neovim, etc.) launch `anureo acp` as a subprocess; the process
//! connects to a local **anureo-server** via WebSocket and relays JSON-RPC between the IDE's
//! stdio and the server's WebSocket endpoint.
//!
//! ## Design principles
//!
//! - **Transport-only bridge**: `anureo acp` is a thin stdio↔WebSocket relay — all agent logic
//!   runs on anureo-server.
//! - **Reuse**: Do not reimplement ReAct/ToT/GoT; only the ACP ↔ anureo adapter layer.
//! - **Session consistency**: ACP `session_id` maps 1:1 to anureo `thread_id` for multi-turn
//!   and checkpointer consistency.
//! - **Auto-spawn**: If anureo-server is not running, the bridge spawns it automatically.
//! - **Auto-reconnect**: On WebSocket disconnect, the bridge retries with exponential
//!   back-off; stdin/stdout channels persist across reconnections.
//!
//! ## Architecture overview
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────────┐
//! │                          IDE (Zed / JetBrains / Neovim)                        │
//! └──────────────────────┬───────────────────────────────────┬─────────────────────┘
//!                        │ stdin (JSON-RPC requests)         │ stdout (JSON-RPC responses)
//!                        ▼                                   ▲
//! ┌───────────────────────────────────────────────────────────────────────────────┐
//! │                         anureo acp (this crate)                                 │
//! │                                                                               │
//! │  ┌─────────────────────────────────────────────────────────────────────────┐  │
//! │  │  ws_bridge.rs — stdio↔WebSocket relay                                    │  │
//! │  │                                                                          │  │
//! │  │  stdin reader ──► unbounded channel ──┐                                 │  │
//! │  │                                        ├──► relay_loop ──► WebSocket ──► │  │
//! │  │  stdout writer ◄── std mpsc channel ◄──┘    (cancel-aware)               │  │
//! │  │                                                                          │  │
//! │  │  Auto-spawn: probe /global/health ──► spawn anureo-server if not running   │  │
//! │  │  Auto-reconnect: exponential back-off (500ms → 10s max)                  │  │
//! │  └─────────────────────────────────────────────────────────────────────────┘  │
//! └───────────────────────────────────┬───────────────────────────────────────────┘
//!                                     │ WebSocket (ws://127.0.0.1:3030/acp)
//!                                     ▼
//! ┌───────────────────────────────────────────────────────────────────────────────┐
//! │                         anureo-server                                           │
//! │                                                                               │
//! │  ┌─────────────────────────────────────────────────────────────────────────┐  │
//! │  │  handlers/acp.rs — WebSocket accept + per-connection task                │  │
//! │  │  acp_hub.rs — spawn run_agent_connection() per connection                │  │
//! │  └───────────────────────────────┬─────────────────────────────────────────┘  │
//! │                                  ▼                                             │
//! │  ┌─────────────────────────────────────────────────────────────────────────┐  │
//! │  │  stdio_loop.rs — run_agent_connection()                                  │  │
//! │  │  Drives ACP JSON-RPC dispatch over any Lines transport.                  │  │
//! │  │  Spawns notification drain task (SessionNotification → client).          │  │
//! │  └───────────────────────────────┬─────────────────────────────────────────┘  │
//! │                                  ▼                                             │
//! │  ┌─────────────────────────────────────────────────────────────────────────┐  │
//! │  │  agent.rs — AnureoAcpAgent (impl Agent)                                    │  │
//! │  │  initialize / authenticate / new_session / prompt / cancel /             │  │
//! │  │  fork_session / load_session / list_sessions /                           │  │
//! │  │  set_session_config_option / set_session_mode                            │  │
//! │  └──────┬───────────┬──────────────┬────────────────┬────────────────────────┘  │
//! │         ▼           ▼              ▼                ▼                            │
//! │  ┌──────────┐ ┌───────────┐ ┌─────────────┐ ┌──────────────────┐                │
//! │  │ session  │ │ content   │ │ stream_     │ │ tools            │                │
//! │  │ .rs      │ │ .rs       │ │ bridge.rs   │ │ (fs/terminal)    │                │
//! │  │          │ │           │ │             │ │                  │                │
//! │  │ Session  │ │ ContentBlk│ │ anureo event  │ │ ReadTextFile     │                │
//! │  │ Store    │ │ → message │ │ → ACP Update│ │ WriteTextFile    │                │
//! │  │ Cancel   │ │           │ │ Token usage │ │ ClientBridge     │                │
//! │  └──────────┘ └───────────┘ └─────────────┘ └──────────────────┘                │
//! │         │           │              │                                           │
//! │         └───────────┴──────────────┘                                           │
//! │                     ▼                                                          │
//! │  ┌─────────────────────────────────────────────────────────────────────────┐  │
//! │  │  anureo core — run_agent_from_config / build_react_config                 │  │
//! │  │  ReAct graph execution, MCP tools, checkpoint persistence               │  │
//! │  └─────────────────────────────────────────────────────────────────────────┘  │
//! └───────────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! ## Entrypoint
//!
//! ```sh
//! anureo acp                          # connect to ws://127.0.0.1:3030/acp
//! anureo acp ws://host:port/acp       # custom URL
//! ```
//!
//! All ACP protocol parameters (working directory, MCP servers, etc.) come from
//! the IDE's `session/new` request — no CLI flags needed beyond the URL.
//!
//! ## Request/response flow (single prompt turn)
//!
//! ```text
//!   IDE                ws_bridge              anureo-server               AnureoAcpAgent           anureo
//!    │                    │                       │                          │                    │
//!    │── initialize ─────►│ relay ──► WS ───────►│ run_agent_connection ──►│ initialize()        │
//!    │◄─── response ──────│◄── WS ◄──────────────│◄────────────────────────│                    │
//!    │                    │                       │                          │                    │
//!    │── session/new ────►│ relay ──► WS ───────►│ ──────────────────────►│ SessionStore::create│
//!    │◄─── response ──────│◄── WS ◄──────────────│◄────────────────────────│                    │
//!    │                    │                       │                          │                    │
//!    │── session/prompt ─►│ relay ──► WS ───────►│ ──────────────────────►│ resolve model       │
//!    │                    │                       │                          │ content→message     │
//!    │                    │                       │                          │ run_agent ─────────►│
//!    │                    │                       │                          │                    │
//!    │   session/update   │◄── WS ◄──────────────│◄── SessionNotifier ◄────│ ◄── stream events   │
//!    │◄── (agent_chunk) ──│                       │                          │                    │
//!    │   session/update   │◄── WS ◄──────────────│◄── SessionNotifier ◄────│ ◄── tool calls      │
//!    │◄── (tool_call) ────│                       │                          │                    │
//!    │                    │                       │                          │                    │
//!    │                    │                       │                          │ ◄── reply           │
//!    │◄── prompt resp ────│◄── WS ◄──────────────│◄────────────────────────│ PromptResponse      │
//! ```
//!
//! ## Module dependency map
//!
//! ```text
//!                          ┌──────────────┐
//!                          │   lib.rs     │  set_log_config / get_log_config
//!                          └──────┬───────┘
//!                                 │
//!          ┌──────────────────────┼──────────────────────┐
//!          ▼                      ▼                      ▼
//!   ┌─────────────┐      ┌───────────────┐      ┌──────────────┐
//!   │ ws_bridge   │      │ stdio_loop    │      │ agent        │
//!   │ (client     │      │ run_agent_    │      │ AnureoAcpAgent │
//!   │  side)      │      │  connection   │      │ (server side)│
//!   └─────────────┘      └───────┬───────┘      └──────┬───────┘
//!                                 │                     │
//!                    ┌────────────┼────────────┐        │
//!                    ▼            ▼            ▼        │
//!              ┌──────────┐ ┌──────────┐ ┌──────────┐   │
//!              │ session  │ │ content  │ │ stream_  │   │
//!              │ Store    │ │ parser   │ │ bridge   │   │
//!              └──────────┘ └──────────┘ └──────────┘   │
//!                    │                                   │
//!                    ▼                                   ▼
//!              ┌──────────────────────────────────────────┐
//!              │  anureo core (agent crate)                  │
//!              │  run_agent_from_config / build_react_config│
//!              └──────────────────────────────────────────┘
//! ```
//!
//! ## Cancellation
//!
//! - **User cancel**: IDE sends `session/cancel` → `SessionStore::cancel_current_generation()`
//!   → increments generation counter → active turn's `RunCancellation` token fires →
//!   `run_agent` returns `RunCompletion::Cancelled` → prompt returns `StopReason::Cancelled`.
//! - **WS disconnect**: The `_prompt_guard` RAII drops on task cancellation, calling
//!   `finish_prompt()` to release the per-session prompt lock.
//! - **Process exit**: `ws_bridge` installs SIGINT/SIGTERM handlers via `CancellationToken`;
//!   the relay loop closes the WebSocket and drains stdout before exiting.
//!
//! ## Session lifecycle
//!
//! | Phase | Handler | Key actions |
//! |-------|---------|-------------|
//! | `initialize` | `agent::initialize` | Negotiate protocol version, save client capabilities |
//! | `session/new` | `agent::new_session` | Create `SessionEntry`, store MCP servers, trigger curator |
//! | `session/prompt` | `agent::prompt` | Resolve model, run agent graph, stream updates |
//! | `session/cancel` | `agent::cancel` | Increment generation, fire cancellation token |
//! | `session/fork` | `agent::fork_session` | Clone config + MCP, new session ID |
//! | `session/load` | `agent::load_session` | Restore from checkpoint, replay history |
//! | `session/list` | `agent::list_sessions_for_owner` | Project owner-scoped SessionIndex records |
//! | `set_config_option` | `agent::set_session_config_option` | Update model/mode/effort, persist |
//! | `set_mode` | `agent::set_session_mode` | Switch agent profile (ask/dev/default) |
//!
//! ## Module overview
//!
//! | Module | Lines | Responsibility |
//! |--------|-------|---------------|
//! | [`agent`] | ~2200 | [`AnureoAcpAgent`]: ACP Agent trait impl, model resolution, prompt dispatch |
//! | [`ws_bridge`] | ~520 | Stdio↔WebSocket relay with auto-spawn and auto-reconnect |
//! | [`stdio_loop`] | ~250 | `run_agent_connection()`: generic ACP dispatch loop |
//! | [`stream_bridge`] | ~1400 | anureo `TypedAnyStreamEvent` → ACP `SessionUpdate`, token usage tracking |
//! | [`session`] | ~450 | `SessionStore`, `SessionEntry`, cancellation state, prompt guard |
//! | [`content`] | ~640 | ACP `ContentBlock` → anureo `UserContent` conversion |
//! | [`connection`]) | ~75 | Per-connection state: capabilities, notification channel, bridge |
//! | [`tools`] | ~670 | FS tools (read/write text file), client bridge, terminal executor |
//! | [`review_runner`] | ~690 | Background curator review (memory + skills) |
//! | [`high_freq_usage`] | ~540 | Throttled token usage notifications |
//! | [`client_methods`] | ~230 | ACP reverse-RPC wrappers (fs/terminal) |
//! | [`terminal`]) | ~440 | Terminal session manager (unused — ACP terminal disabled) |
//! | [`session_config_store`] | ~220 | Persistent session config (SQLite) |
//! | [`client_capabilities`] | ~470 | Parse and query client capabilities from `initialize` |
//! | [`mcp_convert`] | ~210 | Convert ACP MCP server defs → anureo MCP config |
//! | [`agent_registry`] | ~75 | Agent profile registry (modes: ask, default, dev, etc.) |
//! | [`goal_runner`] | ~70 | `/goal` command handler |
//! | [`logging`] | ~160 | Log initialization with working-directory-aware file paths |
//! | [`protocol`]) | ~130 | Protocol version and feature mapping reference |
//! | [`last_model`] | ~30 | Persist last-used model across sessions |
//!
//! [ACP]: https://agentclientprotocol.com

use std::sync::OnceLock;

pub mod agent;
pub mod agent_registry;
pub mod cli_client;
pub mod client_capabilities;
pub mod client_methods;
pub mod connection;
pub mod connection_registry;
pub mod content;
pub mod extensions;
pub mod global_events;

pub mod goal_runtime;
pub mod goal_runner;
pub mod high_freq_usage;
pub mod last_model;
pub mod logging;
pub mod mcp_convert;
pub mod notification_router;
pub mod prompt_executor;
pub mod protocol;
pub mod review_runner;
pub mod runtime;
pub mod session;
pub mod session_bindings;
pub mod session_config_store;
pub mod session_load;
pub mod session_repository;
pub mod session_update_log;
pub mod stdio_loop;
pub mod stream_bridge;
pub mod terminal;
pub mod tools;
pub mod ws_bridge;
pub use agent::{AnureoAcpAgent, ModelOption, ModelProvider};
pub use content::{content_blocks_to_message, ContentBlockLike, ContentError};
pub use high_freq_usage::{HighFreqUsageTracker, UsageUpdateInfo};
pub use session::{SessionConfig, SessionEntry, SessionId, SessionStore};
pub use stdio_loop::run_agent_connection;
pub use stream_bridge::{
    anureo_event_to_updates, stream_update_to_session_notification, StreamUpdate,
};

static LOG_CONFIG: OnceLock<logging::LogConfig> = OnceLock::new();

/// Set log config from CLI args (called once at startup).
pub fn set_log_config(config: logging::LogConfig) {
    let _ = LOG_CONFIG.set(config);
}

/// Get log config (returns None if not set).
pub fn get_log_config() -> Option<&'static logging::LogConfig> {
    LOG_CONFIG.get()
}

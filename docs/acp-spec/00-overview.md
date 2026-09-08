# anureo ACP 协议规范总览

> **状态**: Spec（anureo 后端实现指南）
> **协议基线**: ACP v1（`agent-client-protocol` crate v0.15.1）
> **扩展命名空间**: `_anureo.dev/*`

## 目录结构

```text
docs/acp-spec/
├── 00-overview.md                         本文件
├── 01-connection-protocol.md              initialize + authenticate
├── 02-session-lifecycle.md                session/new/load/list/fork/resume/close/delete
├── 03-prompt-and-cancel.md                session/prompt + session/cancel
├── 04-session-configuration.md            set_config_option + set_mode
├── 05-session-update.md                   全部 SessionUpdate variant
├── 06-reverse-rpc.md                      permission + fs + terminal reverse-RPC
├── 07-transport.md                        WebSocket + stdio + Relay
├── 08-cross-cutting-patterns.md           分页/权限/进度/能力可变性/metadata/resync/子流
└── extensions/
    ├── 10-worktree.md
    ├── 11-git.md
    ├── 12-files.md
    ├── 13-mcp.md
    ├── 14-goal-scheduled-task.md
    ├── 15-connection-relay-pairing-auth.md
    ├── 16-question.md
    ├── 17-github.md
    ├── 18-notification.md
    ├── 19-tts-dictation.md
    ├── 20-skills.md
    ├── 21-session-folder.md
    ├── 22-snippet-command.md
    ├── 23-plugin.md
    ├── 24-quota-provider.md
    ├── 25-agent-profile.md
    ├── 26-diagnostics.md
    ├── 27-project-config.md
    ├── 28-tunnel.md
    ├── 29-multi-run.md
    ├── 30-settings.md
    ├── 31-session-assist.md
    ├── 32-small-model.md
    ├── 33-auto-review.md
    ├── 34-preview.md
    ├── 35-terminal.md
    ├── 36-session-history.md
    └── 37-session-list.md
```

## 现有实现状态

| 组件 | 状态 | 关键文件 |
|---|---|---|
| ACP Agent | ✅ 已实现 | `apps/acp/src/agent.rs` — `anureoAcpAgent` |
| initialize | ✅ 已实现 | `agent.rs::initialize()` |
| authenticate | ✅ handler 存在 | `agent.rs::authenticate()`（返回空） |
| session/new | ✅ 已实现 | `agent.rs::new_session()` |
| session/load | ✅ 已实现 | `agent.rs::load_session()` |
| session/list | ✅ 已实现 | `agent.rs::list_sessions()` |
| session/fork | ⚠️ Handler 已实现，capability 未声明 | `agent.rs::fork_session()`（handler 已在 `stdio_loop.rs` 注册，但 `initialize` 响应的 `sessionCapabilities` 未包含 `fork` — 代码 bug）|
| session/resume | ✅ 已实现 | `agent.rs::resume_session_for_owner()` |
| session/close | ✅ 已实现 | `agent.rs::close_session_for_owner()` |
| session/delete | ✅ 已实现 | `agent.rs::delete_session_for_owner()` |
| session/prompt | ✅ 已实现 | `agent.rs::prompt()` |
| session/cancel | ✅ 已实现 | `agent.rs::cancel()` |
| set_config_option | ✅ 已实现 | `agent.rs::set_session_config_option()` |
| set_mode | ✅ 已实现 | `agent.rs::set_session_mode()` |
| session/update | ✅ 已实现 | `apps/acp/src/stream_bridge.rs` |
| request_permission | ✅ 已实现 | `apps/acp/src/client_methods.rs` |
| fs/read_text_file | ✅ 已实现 | 同上 |
| fs/write_text_file | ✅ 已实现 | 同上 |
| terminal/* | ✅ 已实现 | 同上（create/output/wait_for_exit/kill/release） |
| WebSocket /acp | ✅ 已实现 | `apps/server/src/handlers/acp.rs` |
| AcpHub 多连接 | ✅ 已实现 | `apps/server/src/acp_hub.rs` |
| stdio bridge | ✅ 已实现 | `apps/acp/src/ws_bridge.rs` |
| _anureo.dev/* | ⚠️ 框架已实现，传输层未接线 | `apps/acp/src/extensions/`（32 个域 handler 已注册，`wrap_incoming_stream` 无生产调用方；详见 `docs/dev/acp/02-adding-methods.md` §4）|

## 协议版本

- `agent-client-protocol` crate: `0.15.1`
- Features: `unstable_boolean_config`, `unstable_session_fork`, `unstable_cancel_request`, `unstable_model_config_category`, `unstable_end_turn_token_usage`

## 实体关系

```text
connection (AcpConnection)
  └── session (SessionEntry, sessionId ↔ thread_id)
        └── generation (一次 prompt 执行)
              └── notification sink (session/update 输出通道)
```

| 实体 | Rust 类型 | 生命周期 |
|---|---|---|
| connection | `AcpConnection` | transport 连接存活期间 |
| session | `SessionEntry` | 持久化到 SQLite |
| generation | `GenerationCancellation` | prompt 开始到 response 返回 |
| notification sink | `mpsc::Sender<SessionNotification>` | session 绑定期间 |

## 能力声明（initialize 响应）

anureo 在 `initialize` 返回的 `agentCapabilities`：

```json
{
  "loadSession": true,
  "promptCapabilities": { "image": true, "audio": true, "embeddedContext": true },
  "mcpCapabilities": { "http": true, "sse": false },
    "sessionCapabilities": {
    "list": {},
    "delete": {},
    "resume": {},
    "close": {}
    // 注意: fork handler 已实现但 capability 未在此声明（代码 bug，待修复）
  }
}
```

扩展能力放在 `agentCapabilities._meta["anureo.dev"]`，内容为 `ExtensionRegistry` 的 capability 快照（32 个域，随域注册自动生成，见 `agent.rs::initialize()`）。注意扩展方法当前未接入传输层分发（见上表 `_anureo.dev/*` 行）。

**注意**: `session/fork` handler 已实现并在 `stdio_loop.rs` 注册，但 `initialize` 响应的 `sessionCapabilities` 未包含 `fork` 字段。这是已知代码 bug（`agent.rs:436-440`），标准客户端不会调用 fork。

## 源码索引

| 模块 | 文件 | 核心类型 |
|---|---|---|
| Agent | `apps/acp/src/agent.rs` | `anureoAcpAgent` |
| 协议 | `apps/acp/src/protocol.rs` | `ProtocolVersion::V1` |
| Session | `apps/acp/src/session.rs` | `SessionStore`, `SessionEntry`, `SessionConfig` |
| Session 持久化 | `apps/acp/src/session_repository.rs` | `SessionRepository` |
| Session 配置存储 | `apps/acp/src/session_config_store.rs` | `SessionConfigStore` |
| Session 绑定 | `apps/acp/src/session_bindings.rs` | thread_id ↔ session_id |
| Stream 桥接 | `apps/acp/src/stream_bridge.rs` | `SessionNotifier` |
| Content 转换 | `apps/acp/src/content.rs` | `content_blocks_to_user_content()` |
| 反向 RPC | `apps/acp/src/client_methods.rs` | permission/fs/terminal |
| Client 桥接 | `apps/acp/src/tools/client_bridge.rs` | `ClientBridgeTrait` |
| 连接管理 | `apps/acp/src/connection.rs` | `AcpConnection` |
| 连接注册 | `apps/acp/src/connection_registry.rs` | `ConnectionRegistry` |
| 通知路由 | `apps/acp/src/notification_router.rs` | `SessionNotification` |
| 运行时 | `apps/acp/src/runtime.rs` | `AcpRuntime` |
| WS Bridge | `apps/acp/src/ws_bridge.rs` | `run_ws_bridge()` |
| stdio 循环 | `apps/acp/src/stdio_loop.rs` | `run_agent_connection()` |
| Agent 注册 | `apps/acp/src/agent_registry.rs` | `AgentRegistry` |
| Client 能力 | `apps/acp/src/client_capabilities.rs` | `ClientCapabilitiesInfo` |
| 高频用量 | `apps/acp/src/high_freq_usage.rs` | token usage 节流 |
| MCP 转换 | `apps/acp/src/mcp_convert.rs` | ACP MCP → anureo MCP |
| Server WS | `apps/server/src/handlers/acp.rs` | `/acp` WebSocket upgrade |
| Server Hub | `apps/server/src/acp_hub.rs` | `AcpHub`, `SessionOwner`, `DisconnectPolicy` |

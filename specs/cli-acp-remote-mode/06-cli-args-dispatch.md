# 06 — CLI 参数与分发

> **Scope**: `--remote` 参数定义与 `main.rs` 分发逻辑  
> **Files**: `apps/cli/src/args.rs`, `apps/cli/src/main.rs`, `apps/cli/src/server_transport/mod.rs`

## 参数定义

### 新增字段（`apps/cli/src/args.rs`）

在 `Args` 结构体中新增 `remote` 字段：

```rust
// apps/cli/src/args.rs — Args struct

pub(crate) struct Args {
    // ... existing fields ...

    /// Run against a remote anureo-server via ACP WebSocket protocol.
    ///
    /// When set, agent execution happens on the server instead of in-process.
    /// Accepts an optional URL; defaults to ws://127.0.0.1:3030/acp.
    ///
    /// Examples:
    ///   anureo --remote "hello"
    ///   anureo --remote ws://192.168.1.100:3030/acp "hello"
    ///   anureo --remote -i
    #[arg(
        long,
        value_name = "URL",
        num_args = 0..=1,
        default_missing_value = "ws://127.0.0.1:3030/acp",
        global = false,
    )]
    pub(crate) remote: Option<String>,
}
```

### 参数行为

| 命令 | `args.remote` 值 | 效果 |
|------|-------------------|------|
| `anureo "msg"` | `None` | 本地模式（默认） |
| `anureo --remote "msg"` | `Some("ws://127.0.0.1:3030/acp")` | ACP 远程模式，默认地址 |
| `anureo --remote ws://host:port/acp "msg"` | `Some("ws://host:port/acp")` | ACP 远程模式，自定义地址 |

`clap` 配置说明：
- `num_args = 0..=1`：`--remote` 可不带值（使用默认）或带 URL 值
- `default_missing_value`：不带值时的默认 URL
- `global = false`：不是全局参数，仅顶层可用（不能放在子命令后）

### 与现有参数的兼容性

| 参数 | 与 `--remote` 共存 | 说明 |
|------|-------------------|------|
| `-m` / positional | ✅ | 消息传递给 `session/prompt` |
| `--working-folder` / `-w` | ✅ | 传递给 `session/new` 的 `cwd` |
| `--session-id` / `-s` | ✅ | 使用 `session/load` 恢复 |
| `--model` / `-M` | ✅ | 通过 `setconfigoption` 设置 |
| `--tier` | ✅ | 通过 `setconfigoption` 设置 |
| `--agent` / `-P` | ✅ | 通过 `setmode` 设置 |
| `--interactive` / `-i` | ✅ | 多轮对话 |
| `--json` | ✅ | JSON 输出 |
| `-v` / `--verbose` | ✅ | 详细输出 |
| `--timestamp` | ✅ | 回复时间戳 |
| `react` / `dup` / `tot` | ✅ | 映射到 session mode |
| `server` 子命令 | ❌ | 互斥（server 或 remote） |
| `acp` 子命令 | ❌ | 互斥（acp bridge 或 remote client） |
| `tool` / `session` 等管理命令 | ❌ | 这些是本地操作 |

## main.rs 分发位置

分发必须在**日志初始化之后**（remote 模式需要日志），但在**subcommand 分发之前**（避免被管理命令拦截）。

### 分发顺序

```
main()
  │
  ├─ 1. 参数解析 (Args::parse)
  ├─ 2. 验证 tier/model conflict
  │
  ├─ 3. ACP bridge (cmd == Acp) → ws_bridge, return     ← 已有
  ├─ 4. Server (cmd == Server) → run server, return      ← 已有
  │
  ├─ 5. preserve_shell_env
  ├─ 6. print_config_report
  ├─ 7. init_logging                                      ← 日志必须在 remote 之前
  │
  ├─ 8. ★ Remote ACP mode (--remote) → run_acp_mode, return  ← 新增
  │
  ├─ 9. Session/Tool/Models/Mcp/Agent/... subcommands     ← 已有
  │
  └─ 10. Local mode (default) → run_flow                  ← 已有
```

### 具体代码

在 `main.rs` 的 `init_logging` 之后、`Session` 子命令分发之前插入：

```rust
// apps/cli/src/main.rs

async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // ── Validation ──────────────────────────────────────────────
    if let Err(e) = run_flow::validate_tier_arg(&args.tier) {
        eprintln!("anureo: {}", e);
        std::process::exit(1);
    }
    if let Err(e) = run_flow::check_model_tier_conflict(&args) {
        eprintln!("anureo: {}", e);
        std::process::exit(1);
    }

    // ── ACP bridge (existing) ───────────────────────────────────
    if let Some(Cmd::Acp(acp_args)) = &args.cmd {
        let log_config = anureo_acp::logging::LogConfig { /* ... */ };
        anureo_acp::set_log_config(log_config);
        if let Err(e) = anureo_acp::ws_bridge::run_ws_bridge(acp_args.url.clone()).await {
            eprintln!("anureo acp: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // ── Server (existing) ───────────────────────────────────────
    if let Some(Cmd::Server(server_options)) = &args.cmd {
        anureo_server::runtime::run(server_options.clone()).await?;
        return Ok(());
    }

    // ── Logging init (must precede remote mode) ─────────────────
    let shell_env = preserve_shell_env();
    print_config_report(args.verbose);
    let _log_guard = init_logging(&args, shell_env);

    // ════════════════════════════════════════════════════════════
    // ── ★ Remote ACP mode (NEW) ─────────────────────────────────
    // ════════════════════════════════════════════════════════════
    if let Some(remote_url) = &args.remote {
        server_transport::run_acp_mode::run_acp_mode(&args, Some(remote_url.clone()))
            .await
            .map_err(|e| {
                eprintln!("anureo --remote: {e}");
                std::process::exit(1);
            })?;
        return Ok(());
    }

    // ── Session/Tool/Models/Mcp/... subcommands (existing) ──────
    if let Some(Cmd::Session(sa)) = &args.cmd {
        handle_session_command(sa, args.json).await?;
        return Ok(());
    }
    // ... other subcommands ...

    // ── Local mode (existing) ───────────────────────────────────
    let message = resolve_user_message(&args);
    // ...
}
```

### 为什么不能放在 ACP/Server 旁边？

ACP bridge 和 Server 分支在日志初始化**之前**：
- ACP bridge 使用 stdout 进行 JSON-RPC，任何额外的 stdout 输出都会破坏协议
- Server 不需要 CLI 的日志配置

Remote ACP mode 是**客户端模式**，需要：
1. 日志初始化（便于调试 WS 连接、JSON-RPC 错误）
2. stdout 用于显示渲染（与本地模式一致的 UX）
3. stderr 用于日志输出

## server_transport/mod.rs 更新

```rust
// apps/cli/src/server_transport/mod.rs

//! Transport layers for communicating with anureo-server.
//!
//! # Modules
//!
//! - [`http`] / [`sse`] / [`client`] — HTTP + SSE transport (for `--server-url` mode, future)
//! - [`acp_client`] — ACP WebSocket client (for `--remote` mode)
//! - [`run_acp_mode`] — ACP mode runner / orchestrator

mod acp_client;        // NEW
mod run_acp_mode;      // NEW

// Existing HTTP/SSE modules
mod client;
mod error;
mod http;
mod session;
mod sse;

pub use client::anureoServerClient;
pub use error::{TransportError, TransportResult};
pub use http::HttpTransport;
pub use session::{PromptRequest as HttpPromptRequest, PromptResponse as HttpPromptResponse, SessionCreateRequest, SessionInfo};
pub use sse::{SseChannelKind, SseEvent, SseStream};

// Re-export for main.rs
pub use acp_client::{AcpClient, AcpSessionUpdate};
pub use run_acp_mode::run_acp_mode;
```

## Cargo.toml 依赖

### `apps/cli/Cargo.toml`

CLI 已依赖 `anureo_acp`（通过 workspace）和 `agent_client_protocol`。如果没有，需要添加：

```toml
[dependencies]
# Existing
anureo_acp = { path = "../acp" }
agent_client_protocol = { workspace = true }

# WebSocket client (if not already present)
tokio-tungstenite = { workspace = true }

# Async utilities (likely already present)
futures = { workspace = true }
tokio = { workspace = true, features = ["full"] }
```

### 验证现有依赖

```bash
# Check if agent_client_protocol is already a dependency
grep "agent_client_protocol" apps/cli/Cargo.toml

# Check if tokio-tungstenite is already a dependency
grep "tokio-tungstenite" apps/cli/Cargo.toml
```

如果 `agent_client_protocol` 不在 CLI 依赖中但 `anureo_acp` 在，可以通过 `anureo_acp` re-export：

```rust
// apps/acp/src/lib.rs — add re-export
pub use agent_client_protocol;
```

然后 CLI 使用 `anureo_acp::agent_client_protocol::schema::v1::*`。

## 帮助文本

`anureo --help` 的输出应包含 `--remote`：

```
Options:
  ...
  --remote [URL]          Run against a remote anureo-server via ACP WebSocket
                          Defaults to ws://127.0.0.1:3030/acp
  ...
```

## 测试

### 参数解析测试

```rust
#[cfg(test)]
mod tests {
    use clap::Parser;
    use super::Args;

    #[test]
    fn remote_flag_no_url() {
        let args = Args::try_parse_from(["anureo", "--remote", "hello"]).unwrap();
        assert_eq!(
            args.remote.as_deref(),
            Some("ws://127.0.0.1:3030/acp")
        );
    }

    #[test]
    fn remote_flag_with_url() {
        let args = Args::try_parse_from([
            "anureo", "--remote", "ws://10.0.0.1:8080/acp", "hello",
        ]).unwrap();
        assert_eq!(
            args.remote.as_deref(),
            Some("ws://10.0.0.1:8080/acp")
        );
    }

    #[test]
    fn no_remote_flag() {
        let args = Args::try_parse_from(["anureo", "hello"]).unwrap();
        assert!(args.remote.is_none());
    }

    #[test]
    fn remote_with_interactive() {
        let args = Args::try_parse_from(["anureo", "--remote", "-i"]).unwrap();
        assert!(args.remote.is_some());
        assert!(args.interactive);
    }

    #[test]
    fn remote_conflicts_with_server_subcommand() {
        // --remote + server subcommand should fail
        let result = Args::try_parse_from(["anureo", "--remote", "server"]);
        // clap should handle this via conflicts_with, or we validate in main.rs
        // assert!(result.is_err()); // if conflicts_with is set
    }
}
```

### 分发测试（集成测试）

```rust
#[cfg(test)]
mod dispatch_tests {
    // These require a running anureo-server, so they're integration tests.

    #[tokio::test]
    #[ignore] // Run with: cargo test -- --ignored
    async fn test_remote_mode_dispatches_to_acp() {
        // 1. Start a test server
        // 2. Run: anureo --remote "hello"
        // 3. Verify output contains agent response
    }
}
```

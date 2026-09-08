# 03 — Server 自动检测与启动

> **Scope**: 从 `ws_bridge.rs` 提取 server auto-spawn 逻辑供 `AcpClient` 复用  
> **Reference**: `apps/acp/src/ws_bridge.rs:350-497`

## 现状

`ws_bridge.rs` 中的 auto-spawn 逻辑是**私有函数**，与 stdio 中继逻辑耦合在同一个文件中：

```
// apps/acp/src/ws_bridge.rs (private functions)
fn parse_host_port(ws_url: &str) -> Option<(String, u16)>     // line 351
fn health_url(ws_url: &str) -> Option<String>                 // line 362
fn probe_client() -> reqwest::Client                          // line 373
async fn probe_server(client: &reqwest::Client, url: &str) -> bool   // line 389
fn resolve_anureo_binary() -> BridgeResult<PathBuf>             // line 397
fn spawn_server(host: &str, port: u16) -> BridgeResult<Child> // line 403
fn spawn_reaper(child: Child)                                 // line 439
async fn ensure_server_ready(...)                             // line 460
fn build_ws_request(ws_url: &str) -> BridgeResult<Request>   // line 51
```

这些函数是 transport 层通用逻辑，与 stdio 透传无关，应提取为独立模块。

## 重构方案

### 新增文件：`apps/acp/src/server_bootstrap.rs`

```rust
//! Server auto-spawn utilities.
//!
//! Shared by `ws_bridge` (IDE stdio relay) and `acp_client` (CLI remote
//! mode). These functions probe whether a anureo-server is reachable and
//! spawn one if not.

use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Default WebSocket ACP endpoint.
pub const DEFAULT_WS_URL: &str = "ws://127.0.0.1:3030/acp";

/// Health-check probe timeout.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait for a spawned server to become healthy.
pub const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Interval between health-check probes.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(300);

/// WS connect timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Initial back-off after a WebSocket disconnect.
pub const RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Maximum back-off between reconnection attempts.
pub const RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(10);

type BootstrapResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Extract `(host, port)` from `ws://host:port/...`.
pub fn parse_host_port(ws_url: &str) -> Option<(String, u16)> {
    let stripped = ws_url
        .strip_prefix("ws://")
        .or_else(|| ws_url.strip_prefix("wss://"))?;
    let host_port = stripped.split('/').next()?;
    let (host, port_str) = host_port.rsplit_once(':')?;
    let port = port_str.parse::<u16>().ok()?;
    Some((host.to_string(), port))
}

/// Derive the HTTP health-check URL from a WebSocket URL.
pub fn health_url(ws_url: &str) -> Option<String> {
    let (host, port) = parse_host_port(ws_url)?;
    let scheme = if ws_url.starts_with("wss://") { "https" } else { "http" };
    Some(format!("{scheme}://{host}:{port}/global/health"))
}

/// Build a reusable HTTP client for health probes.
/// Injects `ANUREO_AUTH_TOKEN` as a bearer header if set.
pub fn probe_client() -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(PROBE_TIMEOUT);
    if let Ok(token) = std::env::var("ANUREO_AUTH_TOKEN") {
        if !token.is_empty() {
            let value = format!("Bearer {token}");
            if let Ok(hv) = reqwest::header::HeaderValue::from_str(&value) {
                builder = builder.default_headers(
                    [(reqwest::header::AUTHORIZATION, hv)].into_iter().collect(),
                );
            }
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Probe whether anureo-server is alive at the health endpoint.
pub async fn probe_server(client: &reqwest::Client, health_url: &str) -> bool {
    let Ok(resp) = client.get(health_url).send().await else {
        return false;
    };
    resp.status().is_success()
}

/// Resolve the current `anureo` executable path.
pub fn resolve_anureo_binary() -> BootstrapResult<std::path::PathBuf> {
    std::env::current_exe()
        .map_err(|e| format!("failed to resolve anureo executable: {e}").into())
}

/// Spawn `anureo server --host <host> --port <port>` as a detached child.
pub fn spawn_server(host: &str, port: u16) -> BootstrapResult<Child> {
    let bin = resolve_anureo_binary()?;
    tracing::info!(bin = %bin.display(), host, port, "spawning anureo server");

    let mut cmd = Command::new(&bin);
    cmd.args(["server", "--host", host, "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }

    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to spawn anureo server: {e}"))?;

    tracing::info!(pid = child.id(), "anureo server spawned");
    Ok(child)
}

/// Reap a child process to prevent zombies (container-safe).
pub fn spawn_reaper(child: Child) {
    let pid = child.id();
    std::thread::Builder::new()
        .name("anureo-server-reaper".into())
        .spawn(move || {
            let mut child = child;
            match child.wait() {
                Ok(status) => tracing::info!(pid, %status, "reaped anureo-server child"),
                Err(e) => tracing::warn!(pid, error = %e, "failed to reap child"),
            }
        })
        .ok();
}

/// Ensure a anureo-server is reachable at the given WebSocket URL.
///
/// 1. Probe the health endpoint.
/// 2. If not running, spawn `anureo server` and poll until healthy.
///
/// Returns `Some(child)` if a server was spawned, `None` if already running.
pub async fn ensure_server_ready(
    ws_url: &str,
    probe_client: &reqwest::Client,
) -> BootstrapResult<Option<Child>> {
    let Some(h_url) = health_url(ws_url) else {
        return Err(format!("cannot derive health URL from {ws_url}").into());
    };

    if probe_server(probe_client, &h_url).await {
        tracing::info!("anureo-server already running");
        return Ok(None);
    }

    tracing::info!("anureo-server not detected, auto-spawning");
    let (host, port) = parse_host_port(ws_url)
        .ok_or_else(|| format!("cannot parse host:port from {ws_url}"))?;
    let child = spawn_server(&host, port)?;

    let deadline = tokio::time::Instant::now() + SERVER_READY_TIMEOUT;
    loop {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "anureo-server did not become healthy within {}s",
                SERVER_READY_TIMEOUT.as_secs()
            )
            .into());
        }
        if probe_server(probe_client, &h_url).await {
            tracing::info!("anureo-server is ready");
            return Ok(Some(child));
        }
        tokio::time::sleep(PROBE_INTERVAL).await;
    }
}

/// Build the WebSocket upgrade request with optional auth header.
pub fn build_ws_request(
    ws_url: &str,
) -> BootstrapResult<tokio_tungstenite::tungstenite::handshake::client::Request> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let mut request = ws_url
        .into_client_request()
        .map_err(|e| format!("invalid WebSocket URL {ws_url}: {e}"))?;

    if let Ok(token) = std::env::var("ANUREO_AUTH_TOKEN") {
        let value = format!("Bearer {token}")
            .parse()
            .map_err(|e| format!("invalid ANUREO_AUTH_TOKEN value: {e}"))?;
        request.headers_mut().insert("Authorization", value);
    }

    Ok(request)
}
```

### 修改 `apps/acp/src/lib.rs`

```rust
// 新增模块声明
pub mod server_bootstrap;

// Re-export for convenience
pub use server_bootstrap::{
    ensure_server_ready, probe_client, build_ws_request,
    DEFAULT_WS_URL, CONNECT_TIMEOUT,
};
```

### 修改 `apps/acp/src/ws_bridge.rs`

将原有私有函数替换为引用 `server_bootstrap` 模块：

```rust
// Before (private functions in ws_bridge.rs):
fn parse_host_port(...) { ... }
fn health_url(...) { ... }
async fn ensure_server_ready(...) { ... }
// ...

// After (import from server_bootstrap):
use crate::server_bootstrap::{
    ensure_server_ready, probe_client, build_ws_request,
    spawn_reaper, DEFAULT_WS_URL, CONNECT_TIMEOUT,
    RECONNECT_INITIAL_BACKOFF, RECONNECT_MAX_BACKOFF,
};

// All existing call sites remain unchanged because the function
// signatures are identical.
```

**注意**：`ws_bridge.rs` 还需要 `RECONNECT_INITIAL_BACKOFF` 和 `RECONNECT_MAX_BACKOFF` 常量。这些是 bridge 特有的重连逻辑，可以保留在 `ws_bridge.rs` 或一并迁移到 `server_bootstrap.rs`。建议迁移到 `server_bootstrap.rs` 以统一管理所有常量。

## 迁移检查清单

| 函数/常量 | 原位置 (`ws_bridge.rs` line) | 新位置 (`server_bootstrap.rs`) | `ws_bridge` 需要引用 |
|-----------|-----|-----|------|
| `DEFAULT_WS_URL` | 33 | `pub const` | ✓ |
| `SERVER_READY_TIMEOUT` | 36 | `pub const` | ✓ |
| `PROBE_INTERVAL` | 39 | `pub const` | ✓ |
| `RECONNECT_INITIAL_BACKOFF` | 42 | `pub const` | ✓ (bridge 特有但可共享) |
| `RECONNECT_MAX_BACKOFF` | 45 | `pub const` | ✓ |
| `CONNECT_TIMEOUT` | 48 | `pub const` | ✓ |
| `build_ws_request` | 51 | `pub fn` | ✓ |
| `parse_host_port` | 351 | `pub fn` | (仅 `health_url` 内部用) |
| `health_url` | 362 | `pub fn` | (仅 `ensure_server_ready` 内部用) |
| `probe_client` | 373 | `pub fn` | ✓ |
| `probe_server` | 389 | `pub async fn` | (仅 `ensure_server_ready` 内部用) |
| `resolve_anureo_binary` | 397 | `pub fn` | (仅 `spawn_server` 内部用) |
| `spawn_server` | 403 | `pub fn` | ✓ (可能需要) |
| `spawn_reaper` | 439 | `pub fn` | ✓ |
| `ensure_server_ready` | 460 | `pub async fn` | ✓ |

## 现有测试迁移

`ws_bridge.rs` 的测试（`parse_host_port_ws`, `health_url_ws` 等）应迁移到 `server_bootstrap.rs`：

```rust
// apps/acp/src/server_bootstrap.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_host_port_ws() {
        assert_eq!(
            parse_host_port("ws://127.0.0.1:3030/acp"),
            Some(("127.0.0.1".into(), 3030))
        );
    }

    #[test]
    fn parse_host_port_wss() {
        assert_eq!(
            parse_host_port("wss://example.com:8443/path"),
            Some(("example.com".into(), 8443))
        );
    }

    #[test]
    fn parse_host_port_missing_port() {
        assert_eq!(parse_host_port("ws://localhost/acp"), None);
    }

    #[test]
    fn parse_host_port_bad_scheme() {
        assert_eq!(parse_host_port("http://127.0.0.1:3030/acp"), None);
    }

    #[test]
    fn health_url_ws() {
        assert_eq!(
            health_url("ws://127.0.0.1:3030/acp").as_deref(),
            Some("http://127.0.0.1:3030/global/health")
        );
    }

    #[test]
    fn health_url_wss() {
        assert_eq!(
            health_url("wss://secure.example.com:443/acp").as_deref(),
            Some("https://secure.example.com:443/global/health")
        );
    }

    #[tokio::test]
    async fn probe_server_returns_false_for_unreachable() {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .unwrap();
        assert!(!probe_server(&client, "http://127.0.0.1:59999/health").await);
    }
}
```

## `AcpClient` 调用方使用方式

```rust
// apps/cli/src/server_transport/run_acp_mode.rs
use anureo_acp::server_bootstrap::{ensure_server_ready, probe_client, DEFAULT_WS_URL};

pub async fn run_acp_mode(args: &Args, server_url: Option<String>) -> Result<(), String> {
    let url = server_url.unwrap_or_else(|| DEFAULT_WS_URL.to_string());

    // 1. Ensure server is running (auto-spawn if needed)
    let probe = probe_client();
    if let Err(e) = ensure_server_ready(&url, &probe).await {
        return Err(format!("failed to start anureo-server: {e}"));
    }

    // 2. Connect ACP client
    let (client, update_rx) = AcpClient::connect(&url).await
        .map_err(|e| format!("ACP connect failed: {e}"))?;

    // ... rest of orchestration ...
}
```

## 与 `ws_bridge` 的关系

重构后，`ws_bridge.rs` 和 `acp_client.rs` 是两个独立的消费者，共享 `server_bootstrap.rs` 的基础设施：

```
                    ┌─────────────────────────┐
                    │   server_bootstrap.rs    │
                    │  (probe + spawn + URL)   │
                    └──────────┬──────────────┘
                               │
                    ┌──────────┴──────────┐
                    │                     │
                    ▼                     ▼
         ┌──────────────┐     ┌──────────────────┐
         │  ws_bridge   │     │   acp_client     │
         │  (stdio)     │     │   (CLI remote)   │
         │  透传中继    │     │   协议客户端      │
         └──────────────┘     └──────────────────┘
```

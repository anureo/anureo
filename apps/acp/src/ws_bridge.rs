//! Stdio-to-WebSocket bridge.
//!
//! When the user runs `anureo acp --websocket [URL]`, this module connects to
//! the WebSocket ACP endpoint on a anureo-server and relays JSON-RPC messages
//! between the IDE's stdio and the server's WebSocket.
//!
//! If the target anureo-server is not running, it will be spawned automatically.
//!
//! Architecture:
//!
//! ```text
//! IDE  ──stdin──►  [stdin reader thread] ──unbounded channel──►
//!                                                    relay loop ──WS text──►  anureo-server
//! IDE  ◄─stdout── [stdout writer thread] ◄─std mpsc channel──
//!                                                    relay loop ◄─WS text──  anureo-server
//! ```
//!
//! The bridge is transport-only: it does not interpret JSON-RPC content.
//! All agent logic runs on the anureo-server side.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
#[cfg(unix)]
use tokio::signal;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

type BridgeResult<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Default WebSocket ACP endpoint on a local anureo-server.
const DEFAULT_WS_URL: &str = "ws://127.0.0.1:3030/acp";

/// How long to wait for a spawned anureo-server to become healthy.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);

/// Interval between health-check probes.
const PROBE_INTERVAL: Duration = Duration::from_millis(300);

/// Initial back-off after a WebSocket disconnect.
const RECONNECT_INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Maximum back-off between reconnection attempts.
const RECONNECT_MAX_BACKOFF: Duration = Duration::from_secs(10);

/// Timeout for the WebSocket TCP connect handshake.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the WebSocket upgrade request with optional auth header.
fn build_ws_request(
    ws_url: &str,
) -> BridgeResult<tokio_tungstenite::tungstenite::handshake::client::Request> {
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

/// Outcome of the relay loop — tells the caller whether to reconnect or exit.
enum RelayOutcome {
    /// stdin closed (IDE exited); the bridge should terminate.
    StdinClosed,
    /// WebSocket dropped; caller may reconnect.
    WsDisconnected,
    /// stdout pipe broke; the bridge should terminate.
    StdoutClosed,
}

/// Relay messages bidirectionally between `stdin_rx`/`stdout_tx` and `ws`.
/// Returns when either side closes.
async fn relay_loop(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    stdin_rx: &mut mpsc::UnboundedReceiver<String>,
    stdout_tx: &std::sync::mpsc::Sender<String>,
    cancel: &tokio_util::sync::CancellationToken,
) -> RelayOutcome {
    loop {
        tokio::select! {
            biased;
            // Cancel signal (SIGINT/SIGTERM) — close WS and exit immediately.
            _ = cancel.cancelled() => {
                tracing::info!("cancel signal received in relay loop");
                let _ = ws.send(Message::Close(None)).await;
                return RelayOutcome::StdinClosed;
            }
            line = stdin_rx.recv() => {
                match line {
                    Some(text) => {
                        if let Err(e) = ws.send(Message::Text(text)).await {
                            tracing::error!(error = %e, "WebSocket send error");
                            return RelayOutcome::WsDisconnected;
                        }
                    }
                    None => {
                        tracing::info!("stdin closed, shutting down bridge");
                        let _ = ws.send(Message::Close(None)).await;
                        return RelayOutcome::StdinClosed;
                    }
                }
            }
            msg = ws.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        if stdout_tx.send(text.to_string()).is_err() {
                            tracing::info!("stdout closed, shutting down bridge");
                            return RelayOutcome::StdoutClosed;
                        }
                    }
                    Some(Ok(Message::Close(_))) => {
                        tracing::info!("WebSocket closed by server");
                        return RelayOutcome::WsDisconnected;
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        continue;
                    }
                    Some(Ok(Message::Binary(_))) => {
                        tracing::warn!("ignoring binary WebSocket frame");
                        continue;
                    }
                    Some(Ok(Message::Frame(_))) => {
                        continue;
                    }
                    Some(Err(e)) => {
                        tracing::error!(error = %e, "WebSocket recv error");
                        return RelayOutcome::WsDisconnected;
                    }
                    None => {
                        tracing::info!("WebSocket stream ended");
                        return RelayOutcome::WsDisconnected;
                    }
                }
            }
        }
    }
}

/// Run the stdio↔WebSocket bridge with auto-reconnect.
///
/// 1. Probe the target URL — if anureo-server is already running, use it.
/// 2. If not running, spawn `anureo server` as a detached child.
/// 3. Connect WebSocket and relay.
/// 4. On disconnect, re-probe / re-spawn and reconnect (exponential back-off).
/// 5. Exit only when stdin (IDE) or stdout closes.
pub async fn run_ws_bridge(url: Option<String>, pid_file: Option<PathBuf>) -> BridgeResult<()> {
    let ws_url = url.unwrap_or_else(|| DEFAULT_WS_URL.to_string());

    // stdin reader → unbounded channel (persists across reconnections).
    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<String>();
    std::thread::Builder::new()
        .name("acp-ws-stdin".into())
        .spawn(move || {
            let stdin = std::io::stdin();
            for line in stdin.lock().lines() {
                match line {
                    Ok(l) => {
                        if stdin_tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            tracing::info!("stdin reader thread exiting (EOF or error)");
        })
        .expect("spawn acp-ws-stdin reader thread");

    // stdout writer: std channel → blocking writes (persists across reconnections).
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel::<String>();
    std::thread::Builder::new()
        .name("acp-ws-stdout".into())
        .spawn(move || {
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            for line in stdout_rx.iter() {
                let mut bytes = line.into_bytes();
                bytes.push(b'\n');
                if handle.write_all(&bytes).is_err() || handle.flush().is_err() {
                    break;
                }
            }
            tracing::info!("stdout writer thread exiting");
        })
        .expect("spawn acp-ws-stdout writer thread");

    // Shutdown signal from OS (SIGINT / SIGTERM).
    let cancel = tokio_util::sync::CancellationToken::new();
    let c = cancel.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use signal::unix::{signal, SignalKind};
            let mut sigint = signal(SignalKind::interrupt()).ok();
            let mut sigterm = signal(SignalKind::terminate()).ok();
            tokio::select! {
                _ = async {
                    match &mut sigint {
                        Some(s) => { s.recv().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {}
                _ = async {
                    match &mut sigterm {
                        Some(s) => { s.recv().await; }
                        None => std::future::pending::<()>().await,
                    }
                } => {}
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.ok();
        }
        tracing::info!("shutdown signal received, stopping ACP WebSocket bridge");
        c.cancel();
    });

    let mut backoff = RECONNECT_INITIAL_BACKOFF;
    let probe_client = probe_client();
    let mut server_child: Option<std::process::Child> = None;

    loop {
        if cancel.is_cancelled() {
            tracing::info!("shutdown requested, exiting bridge");
            break;
        }

        // Ensure server is alive (probe → spawn if needed).
        match ensure_server_ready(&ws_url, pid_file.as_deref(), &cancel, &probe_client).await {
            Ok(Some(child)) => {
                // New server spawned — reap the old child to prevent zombies.
                if let Some(old) = server_child.take() {
                    spawn_reaper(old);
                }
                server_child = Some(child);
            }
            Ok(None) => {}
            Err(e) => {
                if cancel.is_cancelled() {
                    break;
                }
                tracing::error!(error = %e, "failed to ensure server ready");
                tracing::info!(backoff = ?backoff, "retrying after back-off");
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => { break; }
                }
                backoff = (backoff * 2).min(RECONNECT_MAX_BACKOFF);
                continue;
            }
        }

        // Connect WebSocket.
        let request = match build_ws_request(&ws_url) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(error = %e, "failed to build WebSocket request");
                return Err(e);
            }
        };

        let (mut ws, response) =
            match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(request))
                .await
            {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    tracing::error!(error = %e, url = %ws_url, "WebSocket connect failed");
                    tracing::info!(backoff = ?backoff, "retrying after back-off");
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => { break; }
                    }
                    backoff = (backoff * 2).min(RECONNECT_MAX_BACKOFF);
                    continue;
                }
                Err(_) => {
                    tracing::error!(
                        timeout = ?CONNECT_TIMEOUT,
                        url = %ws_url,
                        "WebSocket connect timed out"
                    );
                    tracing::info!(backoff = ?backoff, "retrying after back-off");
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = cancel.cancelled() => { break; }
                    }
                    backoff = (backoff * 2).min(RECONNECT_MAX_BACKOFF);
                    continue;
                }
            };

        tracing::info!(status = %response.status(), "WebSocket connected, bridge active");
        backoff = RECONNECT_INITIAL_BACKOFF;

        // Relay until one side closes.
        match relay_loop(&mut ws, &mut stdin_rx, &stdout_tx, &cancel).await {
            RelayOutcome::StdinClosed | RelayOutcome::StdoutClosed => {
                // IDE went away — exit for good.
                break;
            }
            RelayOutcome::WsDisconnected => {
                tracing::info!(
                    backoff = ?backoff,
                    "WebSocket disconnected, will reconnect"
                );
                tokio::select! {
                    _ = tokio::time::sleep(backoff) => {}
                    _ = cancel.cancelled() => { break; }
                }
                backoff = (backoff * 2).min(RECONNECT_MAX_BACKOFF);
            }
        }
    }

    drop(stdout_tx);
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Reap the last spawned server child to prevent zombie processes. Tests
    // can request ownership cleanup because a detached auto-spawned server
    // otherwise keeps the test process's Windows job alive after the bridge
    // has closed its stdio pipes. Production keeps the historical behavior
    // (the local server remains available for the next bridge connection).
    let shutdown_spawned_server = std::env::var("ANUREO_ACP_BRIDGE_EXIT_SHUTDOWN")
        .ok()
        .is_some_and(|value| value == "1");
    if let Some(mut child) = server_child.take() {
        match child.try_wait() {
            Ok(Some(status)) => {
                tracing::info!(%status, "anureo-server child already exited");
            }
            Ok(None) => {
                if shutdown_spawned_server {
                    tracing::info!("stopping anureo-server child for owned bridge shutdown");
                    if let Err(error) = child.kill() {
                        tracing::warn!(error = %error, "failed to stop owned anureo-server child");
                    }
                    let _ = child.wait();
                } else {
                    tracing::info!("anureo-server still running, detaching");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to check anureo-server child status");
            }
        }
    }

    tracing::info!("ACP WebSocket bridge terminated");
    Ok(())
}

// ---------------------------------------------------------------------------
// Auto-spawn
// ---------------------------------------------------------------------------

/// Extract `(host, port)` from a `ws://host:port/...` URL.
fn parse_host_port(ws_url: &str) -> Option<(String, u16)> {
    let stripped = ws_url
        .strip_prefix("ws://")
        .or_else(|| ws_url.strip_prefix("wss://"))?;
    let host_port = stripped.split('/').next()?;
    let (host, port_str) = host_port.rsplit_once(':')?;
    let port = port_str.parse::<u16>().ok()?;
    Some((host.to_string(), port))
}

/// Derive the HTTP health-check URL from the WebSocket URL.
fn health_url(ws_url: &str) -> Option<String> {
    let (host, port) = parse_host_port(ws_url)?;
    let scheme = if ws_url.starts_with("wss://") {
        "https"
    } else {
        "http"
    };
    Some(format!("{scheme}://{host}:{port}/global/health"))
}

/// Build a reusable HTTP client for health-check probes.
///
/// If `ANUREO_AUTH_TOKEN` is set, the bearer token is injected as a default
/// `Authorization` header on every probe request — the server's auth
/// middleware enforces the token on all routes including `/global/health`.
fn probe_client() -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(2));
    if let Ok(token) = std::env::var("ANUREO_AUTH_TOKEN") {
        if !token.is_empty() {
            let value = format!("Bearer {token}");
            if let Ok(hv) = reqwest::header::HeaderValue::from_str(&value) {
                builder = builder
                    .default_headers([(reqwest::header::AUTHORIZATION, hv)].into_iter().collect());
            }
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

/// Probe whether anureo-server is already serving at the target URL.
async fn probe_server(client: &reqwest::Client, health_url: &str) -> bool {
    let Ok(resp) = client.get(health_url).send().await else {
        return false;
    };
    resp.status().is_success()
}

/// Resolve the current `anureo` executable.
fn resolve_anureo_binary() -> BridgeResult<std::path::PathBuf> {
    std::env::current_exe()
        .map_err(|error| format!("failed to resolve current anureo executable: {error}").into())
}

/// Spawn `anureo server --host <host> --port <port>` as a detached child.
///
/// `home` (the `--home` override active in this process, if any) is passed
/// explicitly: the override is process state, not an environment variable,
/// so it does not propagate to children automatically.
fn spawn_server(
    host: &str,
    port: u16,
    pid_file: Option<&Path>,
    home: Option<&Path>,
) -> BridgeResult<std::process::Child> {
    let bin = resolve_anureo_binary()?;

    tracing::info!(bin = %bin.display(), host, port, "spawning anureo server");

    let mut cmd = std::process::Command::new(&bin);
    cmd.args(["server", "--host", host, "--port", &port.to_string()]);
    if let Some(home) = home {
        cmd.arg("--home").arg(home);
        // The companion server is private to this bridge (loopback, freshly
        // bound port). Ambient desktop auth config the bridge cannot satisfy
        // (UI password, shared Express data dir with `jwt-secret`) must not
        // flip it into pre-auth mode: `/health` would answer 401, the probe
        // would loop forever and the WebSocket relay would never start.
        // `ANUREO_AUTH_TOKEN` is kept — the bridge presents it as Bearer.
        cmd.env("ANUREO_DATA_DIR", home);
    }
    cmd.env_remove("ANUREO_UI_PASSWORD");
    cmd.env_remove("ANUREO_JWT_SECRET");
    if let Some(pid_file) = pid_file {
        cmd.arg("--pid-file").arg(pid_file);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

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

    tracing::info!(pid = child.id(), "anureo server spawned successfully");
    Ok(child)
}

/// Spawn a background thread that calls `wait()` on the old child to reap
/// zombie processes.  This prevents zombie accumulation in containers where
/// PID 1 does not have an init system that auto-reaps children.
fn spawn_reaper(child: std::process::Child) {
    let pid = child.id();
    std::thread::Builder::new()
        .name("acp-ws-reaper".into())
        .spawn(move || {
            let mut child = child;
            match child.wait() {
                Ok(status) => {
                    tracing::info!(pid = pid, %status, "reaped old anureo-server child");
                }
                Err(e) => {
                    tracing::warn!(pid = pid, error = %e, "failed to reap old anureo-server child");
                }
            }
        })
        .ok();
}

/// Ensure a anureo-server is reachable at the target WebSocket URL.
///
/// If not, spawn one and poll until healthy.
async fn ensure_server_ready(
    ws_url: &str,
    pid_file: Option<&Path>,
    cancel: &tokio_util::sync::CancellationToken,
    probe_client: &reqwest::Client,
) -> BridgeResult<Option<std::process::Child>> {
    let Some(h_url) = health_url(ws_url) else {
        return Err(format!("cannot derive health URL from {ws_url}").into());
    };

    if probe_server(probe_client, &h_url).await {
        tracing::info!("anureo-server already running");
        return Ok(None);
    }

    tracing::info!("anureo-server not detected, auto-spawning");
    let (host, port) =
        parse_host_port(ws_url).ok_or_else(|| format!("cannot parse host:port from {ws_url}"))?;
    let child = spawn_server(
        &host,
        port,
        pid_file,
        config::home::override_path().as_deref(),
    )?;

    let deadline = tokio::time::Instant::now() + SERVER_READY_TIMEOUT;
    loop {
        if cancel.is_cancelled() {
            return Err("cancelled by signal while waiting for server".into());
        }
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
        assert!(!probe_server(&client, "http://127.0.0.1:1/nope").await);
    }

    #[tokio::test]
    async fn probe_server_returns_true_for_200() {
        use axum::{routing::get, Router};
        let app = Router::new().route("/global/health", get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let url = format!("http://{addr}/global/health");
        assert!(probe_server(&client, &url).await);
    }

    #[tokio::test]
    async fn ensure_server_ready_returns_none_when_already_running() {
        use axum::{routing::get, Router};
        let app = Router::new().route("/global/health", get(|| async { "ok" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let ws_url = format!("ws://{addr}/acp");
        let cancel = tokio_util::sync::CancellationToken::new();
        let client = probe_client();
        let result = ensure_server_ready(&ws_url, None, &cancel, &client).await;
        assert!(result.is_ok(), "ensure_server_ready should succeed");
        assert!(
            result.unwrap().is_none(),
            "should return None (no child spawned) when server is already running"
        );
    }

    #[test]
    fn resolve_anureo_binary_returns_current_executable() {
        assert!(resolve_anureo_binary().unwrap().is_file());
    }
}

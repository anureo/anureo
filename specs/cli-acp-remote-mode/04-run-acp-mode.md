# 04 — ACP 模式 Runner

> **Scope**: `run_acp_mode()` 的完整编排逻辑：连接 → 初始化 → 创建 session → prompt → display  
> **File**: `apps/cli/src/server_transport/run_acp_mode.rs`

## 入口函数

```rust
//! Run the CLI in remote ACP mode.
//!
//! Called from `main.rs` when `--remote` is set. Orchestrates the full
//! ACP client lifecycle: server bootstrap → connect → initialize →
//! create session → prompt → display → shutdown.

use std::sync::Arc;

use agent_client_protocol::schema::v1::PromptResponse;

use crate::args::Args;
use crate::run_flow::resolve_user_message;

use super::acp_client::{AcpClient, AcpResult, AcpSessionUpdate};
use anureo_acp::server_bootstrap::{ensure_server_ready, probe_client, DEFAULT_WS_URL};

/// Run the CLI in remote ACP mode.
pub(crate) async fn run_acp_mode(
    args: &Args,
    server_url: Option<String>,
) -> Result<(), String> {
    let url = server_url.unwrap_or_else(|| DEFAULT_WS_URL.to_string());

    // ── 1. Ensure server is running ──────────────────────────────
    let probe = probe_client();
    if let Err(e) = ensure_server_ready(&url, &probe).await {
        return Err(format!("failed to start anureo-server: {e}"));
    }

    // ── 2. Connect ACP client ────────────────────────────────────
    let (client, mut update_rx) = AcpClient::connect(&url)
        .await
        .map_err(|e| format!("ACP connect failed: {e}"))?;

    // ── 3. Initialize protocol ───────────────────────────────────
    client
        .initialize()
        .await
        .map_err(|e| format!("ACP initialize failed: {e}"))?;

    // ── 4. Create or resume session ──────────────────────────────
    let cwd = args
        .working_folder
        .as_deref()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| ".".to_string())
        });

    let mode = args.cmd.as_ref().and_then(|c| match c {
        crate::args::Command::React => Some("react"),
        crate::args::Command::Dup => Some("dup"),
        crate::args::Command::Tot => Some("tot"),
        _ => None,
    });

    let session_id = if let Some(existing_id) = &args.session_id {
        // Resume existing session
        client
            .load_session(existing_id)
            .await
            .map_err(|e| format!("ACP session/load failed: {e}"))?;
        existing_id.clone()
    } else {
        // Create new session
        let session = client
            .new_session(&cwd, mode)
            .await
            .map_err(|e| format!("ACP session/new failed: {e}"))?;
        session.session_id.to_string()
    };

    tracing::info!(session_id = %session_id, "ACP session ready");

    // ── 5. Run single-turn or interactive ────────────────────────
    if args.interactive {
        run_acp_interactive(&client, &mut update_rx, &session_id, args).await?;
    } else {
        let message = resolve_user_message(args)
            .ok_or_else(|| "no message provided: use -m/--message or positional args".to_string())?;
        run_acp_single_turn(&client, &mut update_rx, &session_id, &message, args).await?;
    }

    // ── 6. Shutdown ──────────────────────────────────────────────
    client.shutdown().await;

    Ok(())
}
```

## 单次执行

```rust
/// Execute a single prompt turn via ACP.
///
/// Sends `session/prompt`, drains `session/update` notifications to the
/// display layer, and waits for the final `PromptResponse`.
async fn run_acp_single_turn(
    client: &AcpClient,
    update_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AcpSessionUpdate>,
    session_id: &str,
    message: &str,
    args: &Args,
) -> Result<(), String> {
    let display = DisplayBridge::from_args(args);

    // Initiate the prompt.
    let prompt_stream = client
        .prompt(session_id, message)
        .await
        .map_err(|e| format!("ACP prompt failed: {e}"))?;

    // Consume updates while waiting for the final response.
    let result = display_prompt_turn(update_rx, prompt_stream, &display, args).await?;

    // Print final reply.
    display.print_result(&result);

    Ok(())
}
```

## 交互式 REPL

```rust
/// Interactive REPL: read user input, send prompts, display responses.
///
/// Reuses the same ACP session across turns for conversation continuity.
async fn run_acp_interactive(
    client: &AcpClient,
    update_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AcpSessionUpdate>,
    session_id: &str,
    args: &Args,
) -> Result<(), String> {
    let display = DisplayBridge::from_args(args);

    println!("ACP remote session: {} (Ctrl+C to exit)", session_id);
    println!();

    let mut turn = 0u32;
    let initial_message = resolve_user_message(args);

    loop {
        turn += 1;

        // Read user input.
        let message = if turn == 1 {
            if let Some(msg) = &initial_message {
                msg.clone()
            } else {
                match read_line("› ") {
                    Some(line) if !line.trim().is_empty() => line,
                    _ => continue,
                }
            }
        } else {
            match read_line("› ") {
                Some(line) if !line.trim().is_empty() => line,
                _ => continue,
            }
        };

        // Exit commands.
        if message.trim() == "/exit" || message.trim() == "/quit" {
            break;
        }

        // Initiate prompt.
        let prompt_stream = match client.prompt(session_id, &message).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("prompt error: {e}");
                continue;
            }
        };

        // Display updates and wait for final response.
        match display_prompt_turn(update_rx, prompt_stream, &display, args).await {
            Ok(result) => {
                display.print_result(&result);
            }
            Err(e) => {
                eprintln!("turn error: {e}");
            }
        }

        println!();
    }

    Ok(())
}

/// Read a line from stdin with a prompt.
fn read_line(prompt: &str) -> Option<String> {
    use std::io::{self, BufRead, Write};
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    let _ = write!(stderr, "{prompt}");
    let _ = stderr.flush();

    let stdin = io::stdin();
    let mut line = String::new();
    match stdin.lock().read_line(&mut line) {
        Ok(0) => None, // EOF
        Ok(_) => Some(line.trim_end().to_string()),
        Err(_) => None,
    }
}
```

## Prompt Turn 执行（核心循环）

```rust
/// Drive a single prompt turn: drain updates to display, await final response.
///
/// This is the shared logic between single-turn and interactive modes.
async fn display_prompt_turn(
    update_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AcpSessionUpdate>,
    prompt_stream: super::acp_client::PromptStream,
    display: &DisplayBridge,
    args: &Args,
) -> Result<PromptResponse, String> {
    let mut result_rx = prompt_stream.result;

    // Concurrent: drain updates while waiting for the final response.
    loop {
        tokio::select! {
            // Session update notification arrived.
            Some(update) = update_rx.recv() => {
                display.handle_update(update);
            }
            // Final PromptResponse arrived.
            result = &mut result_rx => {
                return match result {
                    Ok(Ok(response)) => Ok(response),
                    Ok(Err(e)) => Err(format!("prompt response error: {e}")),
                    Err(_) => Err("connection closed during prompt".to_string()),
                };
            }
        }
    }
}
```

## Ctrl+C 取消

```rust
/// Set up Ctrl+C handler to cancel the active ACP prompt.
///
/// In interactive mode, the first Ctrl+C cancels the current turn.
/// In single-turn mode, Ctrl+C cancels and exits.
fn setup_cancel_handler(client: Arc<AcpClient>, session_id: String) {
    let client_clone = client.clone();
    let session_id_clone = session_id.clone();

    ctrlc::set_handler(move || {
        tracing::info!("Ctrl+C received, cancelling ACP prompt");
        let _ = client_clone.cancel(&session_id_clone);
    })
    .ok();
}
```

## 完整文件结构

```rust
//! apps/cli/src/server_transport/run_acp_mode.rs

mod display_bridge;
use display_bridge::DisplayBridge;

use crate::args::Args;
use crate::run_flow::resolve_user_message;
use super::acp_client::{AcpClient, AcpResult, AcpSessionUpdate, PromptStream};
use anureo_acp::server_bootstrap::{ensure_server_ready, probe_client, DEFAULT_WS_URL};
use agent_client_protocol::schema::v1::PromptResponse;

pub(crate) async fn run_acp_mode(args: &Args, server_url: Option<String>) -> Result<(), String> { ... }
async fn run_acp_single_turn(...) -> Result<(), String> { ... }
async fn run_acp_interactive(...) -> Result<(), String> { ... }
async fn display_prompt_turn(...) -> Result<PromptResponse, String> { ... }
fn read_line(prompt: &str) -> Option<String> { ... }
```

## 配置透传映射

CLI 参数如何映射到 ACP session：

| CLI 参数 | ACP 影响 | 说明 |
|----------|---------|------|
| `--message` / positional | `session/prompt` 的 prompt text | 用户消息 |
| `--working-folder` | `session/new` 的 `cwd` | Agent 工作目录 |
| `--session-id` | `session/load` 恢复 | 对话续接 |
| `--model` | `session/setconfigoption` (model) | 模型覆盖（见下文） |
| `--tier` | `session/setconfigoption` (model) | 通过 tier 解析模型 |
| `--agent` | `session/setmode` | Agent profile |
| `--interactive` | 多轮 prompt 循环 | REPL 模式 |
| `--json` | JSON 输出格式 | 流事件 JSON |
| React/Dup/Tot/GoT | `session/new` 的 `mode` | Agent 模式 |

### 模型/Agent 配置（prompt 前）

在 `session/new` 之后、首次 `prompt` 之前，如果用户指定了 `--model`、`--tier` 或 `--agent`，需要发送配置变更请求：

```rust
/// Apply CLI overrides (model, agent, tier) to the ACP session.
async fn apply_session_overrides(
    client: &AcpClient,
    session_id: &str,
    args: &Args,
) -> Result<(), String> {
    // Set model if specified.
    if let Some(model) = &args.model {
        client.set_config_option(session_id, "model", model).await
            .map_err(|e| format!("failed to set model: {e}"))?;
    }

    // Set tier if specified (resolves to model on server side).
    if let Some(tier) = &args.tier {
        client.set_config_option(session_id, "modelTier", tier).await
            .map_err(|e| format!("failed to set tier: {e}"))?;
    }

    // Set agent profile if specified.
    if let Some(agent) = &args.agent {
        client.set_mode(session_id, agent).await
            .map_err(|e| format!("failed to set agent: {e}"))?;
    }

    Ok(())
}
```

## 错误处理

| 场景 | 处理 |
|------|------|
| Server 启动失败 | 返回错误，提示用户检查环境 |
| WS 连接失败 | 返回错误，提示检查 URL 或 server 状态 |
| `initialize` 失败 | 返回错误（可能是协议版本不匹配） |
| `session/new` 失败 | 返回错误（可能是 cwd 不存在） |
| `prompt` 超时 | 客户端 300s 超时，提示用户检查网络或 server 日志 |
| WS 断开 | Reader loop 退出，pending waiter 收到 `ConnectionClosed` |
| Ctrl+C | 发送 `session/cancel`，等待 prompt 返回 |

## 线程模型

```
                         ┌─────────────────────────┐
                         │    run_acp_mode()       │
                         │    (main tokio task)    │
                         └────────┬────────────────┘
                                  │
                    ┌─────────────┼──────────────┐
                    │             │              │
                    ▼             ▼              ▼
          ┌──────────────┐ ┌───────────┐ ┌──────────────┐
          │  AcpClient   │ │ update_rx │ │ DisplayBridge│
          │  (ws_tx)     │ │  channel  │ │  (rendering) │
          └──────┬───────┘ └─────┬─────┘ └──────────────┘
                 │               │
                 ▼               ▼
          ┌──────────────┐ ┌───────────────────┐
          │ WS Writer    │ │   WS Reader        │
          │ Task         │ │   Task             │
          │              │ │ ┌─ route response  │
          └──────────────┘ │ │   by id → oneshot│
                           │ └─ route notify    │
                           │     → update_tx    │
                           └───────────────────┘
```

- **Main task**：执行 `run_acp_mode` 编排逻辑，在 `display_prompt_turn` 中 `select!` 消费 updates 和 final response
- **WS Writer task**：后台 task，从 `ws_tx` channel 读取消息写入 WebSocket
- **WS Reader task**：后台 task，读取 WebSocket frames，按 JSON-RPC 语义路由
- **DisplayBridge**：同步渲染，运行在 main task 中（display 不是 async）

# CLI ACP Remote Mode — Overview

> **Status**: Design specification  
> **Directory**: `specs/cli-acp-remote-mode/`

## 目标

为 CLI 增加 ACP（Agent Client Protocol）远程模式。CLI 作为 ACP 客户端，通过 WebSocket 连接 anureo-server，使用 ACP JSON-RPC 协议发送 prompt、接收流式响应，由 anureo-server 远程执行 agent 逻辑。

## 动机

当前 CLI 有三种工作模式：

| 模式 | 命令 | Agent 执行位置 | 传输方式 |
|------|------|---------------|---------|
| Local（默认） | `anureo "msg"` | 进程内 | 无（直接调用 ReAct graph） |
| Server | `anureo server` | N/A（启动服务器） | HTTP + WebSocket |
| ACP Bridge | `anureo acp [url]` | anureo-server | stdio↔WebSocket 透传 |

`anureo acp` 是纯透传中继，专为 IDE 集成设计——它不理解 JSON-RPC 语义，只逐行转发。CLI 缺少一种**主动作为 ACP 客户端**与 anureo-server 交互的能力。

此外 `server_transport/run_server_mode.rs` 实现了 HTTP/SSE 远程客户端，但**未接入 `main.rs`**，属于死代码。

## 约束

- 不修改 `anureo acp` 的 stdio 透传行为（IDE 集成不受影响）
- 不修改 anureo-server 的 ACP Agent 实现（服务端协议不变）
- 复用 `ws_bridge.rs` 的 auto-spawn 逻辑（自动检测/启动 anureo-server）
- 复用 `apps/cli/src/display/` 的渲染层（流式 markdown、tool preview）
- ACP 协议消息类型优先从 `anureo_acp` crate 复用，避免重复定义

## 详细设计文档

| 文档 | 内容 |
|------|------|
| [01-acp-wire-protocol.md](./01-acp-wire-protocol.md) | ACP JSON-RPC 消息格式：initialize / session/new / session/prompt / session/update 的完整 wire 格式 |
| [02-acp-client.md](./02-acp-client.md) | `AcpClient` 结构体设计：WebSocket 连接、JSON-RPC id 关联、通知分发、reader loop |
| [03-auto-spawn.md](./03-auto-spawn.md) | Server 自动检测与启动：从 `ws_bridge.rs` 提取共享逻辑 |
| [04-run-acp-mode.md](./04-run-acp-mode.md) | ACP 模式 Runner：单次执行 + 交互式 REPL 的编排逻辑 |
| [05-display-bridge.md](./05-display-bridge.md) | ACP `SessionNotification` → CLI `StreamEvent` 的转换桥接 |
| [06-cli-args-dispatch.md](./06-cli-args-dispatch.md) | `--remote` 参数定义与 `main.rs` 分发逻辑 |
| [07-implementation-steps.md](./07-implementation-steps.md) | S1→S7 增量实现步骤，含精确文件变更 |

## 公开接口

```bash
# 单次执行（remote ACP 模式）
anureo --remote "实现一个二分查找"

# 指定 server 地址
anureo --remote ws://192.168.1.100:3030/acp "review this code"

# 交互式 REPL（复用同一 ACP session）
anureo --remote -i

# JSON 流输出
anureo --remote --json "list files"
```

## 目标架构

```
┌──────────┐                                               ┌──────────────┐
│ anureo CLI │ ── ACP JSON-RPC over WebSocket ──────────────► │ anureo-server  │
│ (ACP     │   initialize / session/new / session/prompt   │ (ACP Agent)  │
│  Client) │ ◄── session/update (流式) / response ──────── │              │
│          │                                               │  ReAct Graph │
│  渲染层   │                                               │  (远程执行)  │
│  (display)│                                               │              │
└──────────┘                                               └──────────────┘
```

## 三种远程入口的关系

```
                    ┌─────────────────────────────────┐
                    │         anureo CLI main()         │
                    └───────────────┬─────────────────┘
                                    │
                    ┌───────────────┼───────────────┐
                    │               │               │
                    ▼               ▼               ▼
            ┌──────────────┐ ┌──────────────┐ ┌──────────────┐
            │  Local Mode  │ │ Remote Mode  │ │ Subcommands  │
            │  (默认)      │ │ (--remote)   │ │ (tool/skill  │
            │              │ │              │ │  /mcp/...)   │
            │  in-process  │ │ ┌──────────┐ │ │              │
            │  ReAct graph │ │ │ ACP WS   │ │ └──────────────┘
            │              │ │ │ (新增)   │ │
            │              │ │ └──────────┘ │
            └──────────────┘ └──────────────┘
                                      │
                    ┌─────────────────┴──────────────┐
                    │           anureo-server           │
                    │  (ACP Agent + HTTP API + SSE)   │
                    └────────────────────────────────┘
```

## 代码组织

```
apps/cli/src/server_transport/
├── mod.rs                      # 模块声明（新增 acp_client, run_acp_mode）
├── http.rs                     # HTTP transport（existing）
├── sse.rs                      # SSE stream（existing）
├── client.rs                   # anureoServerClient（existing）
├── session.rs                  # HTTP session types（existing）
├── error.rs                    # TransportError（existing）
├── acp_client.rs               # ★ NEW: ACP WebSocket client
└── run_acp_mode.rs             # ★ NEW: ACP mode runner

apps/acp/src/
├── ws_bridge.rs                # 提取 auto-spawn 公共函数（重构）
├── server_bootstrap.rs         # ★ NEW: 提取后的 auto-spawn 逻辑
```

## 复用与新增对比

| 组件 | 复用来源 | 说明 |
|------|---------|------|
| WebSocket 连接管理 | `ws_bridge.rs` 的 `ensure_server_ready`, `probe_server`, `spawn_server` | 自动检测/启动 anureo-server |
| ACP 协议消息类型 | `agent_client_protocol::schema::v1::*` | 复用已有 Rust 类型定义 |
| Display 渲染 | `apps/cli/src/display/` | 流式 markdown、tool preview |
| Args 解析 | `apps/cli/src/args.rs` | 新增 `--remote` flag |

| 组件 | 新增 |
|------|------|
| `AcpClient` | JSON-RPC 客户端（id 关联、通知分发） |
| `run_acp_mode` | CLI 执行编排（connect → init → prompt → display） |
| `--remote` 参数 | CLI 参数定义 + main.rs 分发 |
| ACP → Display 桥接 | `SessionNotification` → `StreamEvent` 转换 |

## 优势

1. **零额外依赖**：ACP WebSocket 协议已在 anureo-server 端完整实现，客户端只需实现 JSON-RPC 发送/接收
2. **完整功能**：ACP 协议支持流式输出、工具调用展示、session 管理、模型切换——比 HTTP/SSE 模式功能更完整
3. **一致性**：IDE 用户（通过 `anureo acp`）和 CLI 用户（通过 `anureo --remote`）共享同一 server 实例和 session 存储
4. **自动启动**：复用 `ws_bridge` 的 auto-spawn 逻辑，用户无需手动启动 server

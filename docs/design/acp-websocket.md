# anureo Server ACP WebSocket

`anureo-server` 可在既有 HTTP/SSE 端口上提供 ACP（Agent Client Protocol）WebSocket：

```text
ws://127.0.0.1:18081/acp
```

每一个 WebSocket **文本帧**都是一条完整的 ACP JSON-RPC 消息。二进制帧会被拒绝，单条消息与帧的上限均为 1 MiB。

## 启动

```powershell
cargo run -p anureo-server -- serve --host 127.0.0.1 --port 18081
```

CLI 应维持一条持久 WS，在该连接上先发送 `initialize`，再发送 `session/new`、`session/prompt` 等标准 ACP 方法。传统 IDE 子进程集成仍可使用 `anureo acp` 的 stdio 入口。

## 架构

`/acp` WebSocket 端点使用**进程内分发**，不 spawn 任何子进程：

```text
Client ──WS text frames──→ handlers/acp.rs
                               │
                               ├── AcpHub::attach() → 获取持久 anureoAcpAgent
                               ├── WS frame ↔ Lines transport 适配
                               └── run_agent_connection() 驱动 ACP JSON-RPC dispatch
                                       │
                                       ├── initialize / session/new / session/prompt / ...
                                       ├── notification drain task → session/update 推送
                                       └── anureoAcpAgent → anureo graph 执行
```

核心函数 `anureo_acp::stdio_loop::run_agent_connection()` 接受任意 `Lines` 传输层（stdin/stdout 或 WebSocket），内含全部 ACP handler 注册和 `connect_with` 驱动。stdio 入口 `run_stdio_loop()` 是它的薄封装。

## 断线与重连

服务端通过 `AcpHub` 持有 ACP agent 与 session store，而非 WebSocket 连接持有它们：

- WebSocket 断开不会取消正在运行的 prompt；取消必须显式发送 `session/cancel`。
- 同一客户端重新连接并初始化后会重新绑定通知投递目标。
- 服务端保留最近 512 条 `session/update` 通知；重连时先投递该缓冲区，再投递实时更新。
- 同一个 ACP session 同时只允许一个 prompt；第二个 prompt 会返回 JSON-RPC server error。

当前恢复机制是单个逻辑 CLI 的最新连接接管通知流；多用户/跨身份 session owner 隔离仍不支持。

如需短命令语义，可设置 `ANUREO_ACP_DISCONNECT_POLICY=cancel`，使 WS 关闭时取消所有正在运行的 ACP generation。默认值为 `persist`。

## 安全

既有 `ANUREO_AUTH_TOKEN` Bearer 鉴权同样覆盖 `/acp` 的升级请求。

浏览器 WebSocket 会额外校验 `Origin`：未配置时仅允许 loopback (`localhost`、`127.0.0.1`、`[::1]`) 来源。部署远程 Web UI 时配置精确白名单：

```powershell
$env:ANUREO_ACP_ALLOWED_ORIGINS = "https://ui.example.com,https://staging.example.com"
```

原生 CLI 不发送 `Origin`，因此不会被该浏览器防护规则阻断；仍应启用 Bearer token 并经由 WSS 反向代理暴露公网服务。

## 快速拉起服务端（`anureo acp --websocket`）

如果 IDE / 远程 CLI 报告 `ws://127.0.0.1:3030/acp` 拒连，可以直接在仓库根目录运行：

```powershell
anureo acp --websocket
anureo acp --websocket --server http://127.0.0.1:18081
```

命令会探测目标端口是否已有健康的 `anureo-server`，缺失时后台拉起一个并等待就绪，随后退出 0，
不接管终端。子进程作为共享 detached daemon 运行；如需关闭：`pkill -f anureo-server`。
详细设计见 [acp-websocket-cli-ensure.md](../opencode-protocol/archive/2025-snapshots/acp-adjacent/acp-websocket-cli-ensure.md)。

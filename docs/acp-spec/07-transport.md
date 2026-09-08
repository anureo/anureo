# Transport 层

> **实现状态**: ✅ 已实现
> **源码**: `apps/server/src/handlers/acp.rs`、`apps/server/src/acp_hub.rs`、`apps/acp/src/ws_bridge.rs`、`apps/acp/src/stdio_loop.rs`

---

## 1. WebSocket Transport

### 1.1 Endpoint

```
ws://<host>:<port>/acp
```

默认: `ws://127.0.0.1:3030/acp`

### 1.2 WebSocket Upgrade

| 项目 | 值 |
|---|---|
| 最大消息 | 1 MiB（1024 × 1024 字节） |
| 最大帧 | 1 MiB |
| 帧类型 | 仅 Text；Binary 帧断开连接 |
| JSON-RPC 分帧 | 每个 WebSocket text frame = 一个完整 JSON-RPC message |

### 1.3 Origin 校验

环境变量 `ANUREO_ACP_ALLOWED_ORIGINS`（逗号分隔）控制允许的 origin。

| 配置 | 行为 |
|---|---|
| 未设置 | 仅允许 localhost / 127.0.0.1 / [::1] |
| 设置 | 只允许列表中的 origin |

校验逻辑（`apps/server/src/handlers/acp.rs`）:
1. 提取 `ORIGIN` 请求头
2. 为空则允许
3. 解析 host:port
4. 校验端口为合法 u16（防止 `localhost:3000.evil.com` 绕过）
5. 白名单匹配 host

### 1.4 Bearer Auth

```
Authorization: Bearer <token>
```

| 配置 | 行为 |
|---|---|
| `ANUREO_AUTH_TOKEN` 未设置 | principal = `"local-anonymous"` |
| `ANUREO_AUTH_TOKEN` 已设置 + token 匹配 | principal = `"token-{hash}"`（hash = token 十六进制的高 64 位） |
| `ANUREO_AUTH_TOKEN` 已设置 + token 不匹配 | principal = `"local-anonymous"` |

SessionOwner 创建:
- `SessionOwner::anonymous()` → `"local-anonymous"`
- `SessionOwner::from_bearer(subject)` → 使用 subject 或 anonymous

### 1.5 JSON-RPC 适配

```rust
// apps/server/src/handlers/acp.rs
agent_client_protocol::Lines::new(outgoing, incoming)
```

`Lines` 是 ACP SDK 提供的适配器，将 WebSocket text frame 与 JSON-RPC message 互转。

---

## 2. stdio Transport

### 2.1 使用方式

```bash
anureo acp [url]
# 默认 url: ws://127.0.0.1:3030/acp
```

### 2.2 架构

```text
Client (Anureo)
  ←→ stdio (newline-delimited JSON) ←→
    anureo_acp ws_bridge
      ←→ WebSocket ←→
        anureo server /acp
```

`ws_bridge.rs` 实现 stdio↔WebSocket 双向桥接：
1. 从 WebSocket 读取 JSON-RPC message → 写入 stdout（换行分隔）
2. 从 stdin 读取换行分隔 JSON-RPC message → 写入 WebSocket

### 2.3 stdio 分帧

- 输入: stdin，每行一个 JSON-RPC message（`\n` 分隔）
- 输出: stdout，每行一个 JSON-RPC message（`\n` 分隔）
- stderr: 日志输出（不参与 JSON-RPC）

### 2.4 auto-spawn 和 reconnect

ws_bridge 支持：
- WebSocket 断开后自动重连
- session 持久化——进程退出后 session 可由新 connection load/resume

---

## 3. Relay Transport（计划中）

### 3.1 Relay 是 Transport，不是新协议

Relay 在 ACP 之外提供 E2EE tunnel，内部传输原始 ACP JSON-RPC message。Relay 不扩大或修改 ACP capability。

### 3.2 安全要求

| 检查项 | 要求 |
|---|---|
| E2EE handshake | 必须在发送 `initialize` 前完成 |
| Relay 可见性 | Relay 看不到 ACP 明文（opaque ciphertext） |
| Allowlist | `/acp` 必须加入 Relay WebSocket allowlist |
| 断线行为 | Relay 断线只触发 transport reconnect |
| Capability | Relay 不扩大 ACP capability |

---

## 4. 多连接管理

### 4.1 AcpHub

```rust
// apps/server/src/acp_hub.rs
pub struct AcpHub {
    runtime: Mutex<Option<Arc<anureo_acp::runtime::AcpRuntime>>>,
    // ...
}
```

所有连接共享同一个 `AcpRuntime` 实例。

### 4.2 连接生命周期

```text
attach_with(principal)
  → 创建 AcpConnection（唯一 connectionId）
  → 绑定到 AcpHub
  → 业务交互
  → close_connection
    → 根据 DisconnectPolicy 处理 active sessions
    → 释放资源
```

### 4.3 断开策略

| `DisconnectPolicy` | 行为 |
|---|---|
| `Persist`（默认） | session 保留；如 `idle_ttl_secs > 0`，超时后取消 generation |
| `Cancel` | 立即取消所有绑定 session 的 active generation |

配置: `ANUREO_ACP_DISCONNECT_POLICY=cancel|persist`

### 4.4 统计

```rust
pub struct AcpHubStats {
    pub total_connections: u64,
    pub active_connections: usize,
    pub active_sessions: usize,
    pub active_prompts: usize,
    pub session_rebinds: u64,
}
```

---

## 5. Transport Parity 要求

三种 Transport 的 response/update 语义必须完全一致：

| 检查项 | stdio | WebSocket | Relay |
|---|---|---|---|
| initialize | ✅ | ✅ | ✅ |
| session/new/load/prompt/cancel | ✅ | ✅ | ✅ |
| session/update 全 variant | ✅ | ✅ | ✅ |
| reverse-RPC | ✅ | ✅ | ✅ |
| reconnect 恢复 | ✅ | ✅ | ✅ |

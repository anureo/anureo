# Tunnel（第三方隧道管理）

> 命名空间: `_anureo.dev/tunnel/*`
> Capability key: `tunnel`

## Capability

```json
{
  "tunnel": {
    "list": true,
    "create": true,
    "delete": true,
    "doctor": true
  }
}
```

- 声明 `tunnel` capability 后，client 可以列出、创建、删除第三方隧道并执行诊断。
- 未声明 `tunnel.create` 时，UI 隐藏创建入口；调用返回 `capability_not_supported`。
- Tunnel capability 可在运行时变化（如 provider 凭据被撤销），通过 `_anureo.dev/capability_changed` 通知。

### 与 Relay 的区别

| 维度 | `_anureo.dev/relay/*` | `_anureo.dev/tunnel/*` |
|---|---|---|
| 传输层 | Anureo 自有 E2EE tunnel transport | 第三方 provider（Cloudflare Tunnel、Ngrok 等） |
| 认证 | 内置 pairing / client-auth | Provider API token / 自动配置 |
| 协议影响 | 改变连接可达性，不扩大 ACP capability | 改变连接可达性，**不扩大 ACP capability** |
| 管理面 | `relay/status` 只读 | `tunnel/list`、`create`、`delete`、`doctor` 读写 |

> **关键约束**：通过 tunnel 建立的连接不获得任何额外 ACP 权限。Tunnel 仅影响 transport 可达性，server authorization 层不感知 tunnel 的存在。

---

## Methods

### `_anureo.dev/tunnel/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `tunnel.list` |
| 权限 | Server-side authorization（已认证连接即可） |
| 分页 | 支持标准 cursor 分页（`08-cross-cutting-patterns.md` §1） |

**Request:**

```json
{
  "cursor": null,
  "limit": 50
}
```

**Response:**

```json
{
  "items": [
    {
      "id": "tun_abc123",
      "provider": "cloudflare",
      "name": "my-dev-tunnel",
      "status": "connected",
      "publicUrl": "https://abc123.example.trycloudflare.com",
      "localPort": 8787,
      "createdAt": "2025-08-19T10:00:00Z",
      "connectedAt": "2025-08-19T10:00:05Z",
      "metadata": {
        "region": "auto",
        "quickTunnel": true
      }
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**Rust 类型:**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelEntry {
    pub id: String,
    pub provider: TunnelProvider,
    pub name: String,
    pub status: TunnelStatus,
    pub public_url: Option<String>,
    pub local_port: u16,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub connected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum TunnelProvider {
    Cloudflare,
    Ngrok,
    #[serde(other)]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TunnelStatus {
    Connecting,
    Connected,
    Reconnecting,
    Error,
    Disconnected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelListResponse {
    pub items: Vec<TunnelEntry>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

**逻辑说明:**

1. Server 从本地 tunnel registry 读取所有已创建的隧道条目。
2. `provider` 字段标识隧道提供商；`status` 反映实时连接状态。
3. **Provider token 和 secret 永远不出现在 response 中**——即使是 `metadata` 字段也不得包含凭据。
4. `publicUrl` 为隧道对外的公开地址；`localPort` 为本地被代理的端口。
5. 小型集合（< 100 项）可忽略分页参数，返回 `nextCursor: null`。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `tunnel` capability | `initialize` 中未声明 tunnel |
| `internal_error` | Server 内部错误 | Tunnel registry 读取失败 |

---

### `_anureo.dev/tunnel/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `tunnel.create` |
| 权限 | Server-side authorization（需要写权限 scope） |
| 进度 | 长时操作，支持 `_anureo.dev/tunnel/progress` notification（`08-cross-cutting-patterns.md` §3） |
| 幂等 | 支持 `idempotencyKey`，相同 key 返回已有 tunnel |

**Request:**

```json
{
  "provider": "cloudflare",
  "name": "my-dev-tunnel",
  "localPort": 8787,
  "config": {
    "region": "auto",
    "quickTunnel": true
  },
  "providerToken": "optional-token-string",
  "idempotencyKey": "run-2025-08-19-001"
}
```

**Response:**

```json
{
  "id": "tun_abc123",
  "provider": "cloudflare",
  "name": "my-dev-tunnel",
  "status": "connecting",
  "publicUrl": null,
  "localPort": 8787,
  "createdAt": "2025-08-19T10:00:00Z",
  "connectedAt": null,
  "metadata": {
    "region": "auto",
    "quickTunnel": true
  }
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct TunnelCreateRequest {
    pub provider: TunnelProvider,
    pub name: String,
    pub local_port: u16,
    #[serde(default)]
    pub config: serde_json::Value,
    /// Provider API token；仅用于创建/认证，**永不回传**。
    #[serde(skip_serializing)]
    pub provider_token: Option<String>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelCreateResponse {
    pub id: String,
    pub provider: TunnelProvider,
    pub name: String,
    pub status: TunnelStatus,
    pub public_url: Option<String>,
    pub local_port: u16,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub connected_at: Option<chrono::DateTime<chrono::Utc>>,
    pub metadata: serde_json::Value,
}
```

**逻辑说明:**

1. Server 校验 `provider` 是否为受支持的 provider（Cloudflare、Ngrok 等）。
2. 如果提供了 `providerToken`，server 将其存入 secure credential store（不落日志、不回传），用于 provider API 认证。
3. 如果 `quickTunnel: true`（Cloudflare Quick Tunnel），不需要 token，server 自动创建临时隧道。
4. 创建是异步的：response 返回 `status: "connecting"`，实际连接就绪后通过 `tunnel/changed` 通知。
5. `idempotencyKey` 相同时返回已创建的 tunnel（无论当前状态），避免重复创建。
6. Server 验证 `localPort` 是否可用且未被其他 tunnel 占用。
7. **`providerToken` 字段使用 `#[serde(skip_serializing)]`，确保序列化时永远不输出。**

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `tunnel.create` | initialize 未声明 |
| `forbidden` | 无权限创建 tunnel | server authorization 拒绝 |
| `invalid_params` | 参数校验失败 | 端口越界、provider 不支持、name 为空 |
| `conflict` | 端口已被占用或 name 冲突 | `localPort` 已有活跃 tunnel |
| `provider_error` | Provider API 返回错误 | Cloudflare/Ngrok API 调用失败 |
| `internal_error` | Server 内部错误 | 隧道进程启动失败 |

**进度通知（长时操作）:**

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/tunnel/progress",
  "params": {
    "operationId": "tun_abc123",
    "progress": 50,
    "phase": "negotiating",
    "message": "Establishing tunnel with Cloudflare edge...",
    "cancelable": true
  }
}
```

---

### `_anureo.dev/tunnel/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `tunnel.delete` |
| 权限 | Server-side authorization（需要写权限 scope） |
| 幂等 | 是——删除不存在的 tunnel 返回成功 |

**Request:**

```json
{
  "id": "tun_abc123"
}
```

**Response:**

```json
{
  "id": "tun_abc123",
  "deleted": true
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct TunnelDeleteRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelDeleteResponse {
    pub id: String,
    pub deleted: bool,
}
```

**逻辑说明:**

1. Server 销毁隧道进程，释放 `localPort` 绑定。
2. 从 tunnel registry 中移除条目。
3. 清除 secure credential store 中关联的 provider token（如有）。
4. 删除不存在的 tunnel 是 no-op，返回 `deleted: true`。
5. 删除成功后发送 `tunnel/changed` 通知。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `tunnel.delete` | initialize 未声明 |
| `forbidden` | 无权限删除 | server authorization 拒绝 |
| `internal_error` | Server 内部错误 | 隧道进程无法终止 |

---

### `_anureo.dev/tunnel/doctor`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `tunnel.doctor` |
| 权限 | Server-side authorization（已认证连接即可） |
| 超时 | 建议 30s |

**Request:**

```json
{
  "id": "tun_abc123"
}
```

**Response:**

```json
{
  "id": "tun_abc123",
  "healthy": false,
  "checks": [
    {
      "name": "provider_reachable",
      "passed": true,
      "latencyMs": 45,
      "detail": "Cloudflare API reachable"
    },
    {
      "name": "tunnel_connected",
      "passed": false,
      "latencyMs": null,
      "detail": "Tunnel process exited unexpectedly (code=1)"
    },
    {
      "name": "local_port_listening",
      "passed": true,
      "latencyMs": 2,
      "detail": "Port 8787 accepting connections"
    },
    {
      "name": "dns_resolved",
      "passed": false,
      "latencyMs": null,
      "detail": "Public URL DNS not resolving"
    }
  ],
  "recommendation": "Tunnel process has exited. Try recreating the tunnel or check provider credentials."
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct TunnelDoctorRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelDoctorResponse {
    pub id: String,
    pub healthy: bool,
    pub checks: Vec<TunnelCheck>,
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelCheck {
    pub name: String,
    pub passed: bool,
    pub latency_ms: Option<u32>,
    pub detail: String,
}
```

**逻辑说明:**

1. Server 执行一系列健康检查（provider 可达性、隧道进程存活、本地端口监听、DNS 解析、端到端连通性）。
2. 每个检查独立执行，一个检查失败不阻断其他检查。
3. `healthy` 为所有检查全部通过时才为 `true`。
4. `recommendation` 为 server 基于失败检查生成的人类可读建议。
5. Doctor 操作是只读的——不修改 tunnel 状态，不发送 `tunnel/changed`。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `tunnel.doctor` | initialize 未声明 |
| `not_found` | Tunnel 不存在 | `id` 在 registry 中不存在 |
| `internal_error` | Server 内部错误 | 诊断过程异常 |

---

## Notifications

### `_anureo.dev/tunnel/changed`

| 项目 | 内容 |
|---|---|
| 方向 | Server → Client notification |
| 触发 | 隧道创建、删除、状态变化（connected → error 等） |

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/tunnel/changed",
  "params": {
    "change": "status",
    "id": "tun_abc123",
    "status": "connected",
    "publicUrl": "https://abc123.example.trycloudflare.com"
  }
}
```

**params 字段:**

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | `"created"` \| `"deleted"` \| `"status"` | 变化类型 |
| `id` | string | 受影响的 tunnel ID |
| `status` | string? | 当前状态（status 变化时必填） |
| `publicUrl` | string? | 公开 URL（首次连接成功时携带） |

**逻辑说明:**

1. `change: "created"` 在 `tunnel/create` 响应后、隧道实际连接前发送。
2. `change: "status"` 在状态转移时发送（connecting → connected、connected → error 等）。
3. `change: "deleted"` 在 `tunnel/delete` 完成后发送。
4. Notification 不携带完整 tunnel 列表——client 收到后调用 `tunnel/list` 获取完整快照。
5. 多个 client 连接同一 server 时，所有 client 都会收到 notification。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `tunnel/changed` | `tunnel/list` | 完整 tunnel 列表（含状态） |

- Client 重连后必须调用 `tunnel/list` 获取完整隧道快照，不能依赖缓存的 notification 状态。
- 如果 `tunnel/list` 调用失败，client 必须保留旧状态（显示 stale 指示），不能当作空集合处理。

# Connection、Relay、Pairing、Client Auth

> 命名空间: `_anureo.dev/auth/*`、`_anureo.dev/connection/*`、`_anureo.dev/relay/*`、`_anureo.dev/pairing/*`、`_anureo.dev/client-auth/*`
> Capability key: `auth`、`connection`、`relay`、`pairing`、`client-auth`

## 设计原则

- **Relay 是 Transport，不是新的 ACP message protocol**：Relay 只能改变连接可达性（使远程 client 可以连接 server），不能扩大 ACP capability 或绕过 server authorization。
- **Secrets 一次性、不存储、不落日志**：Pairing secret 和 client token 只在创建/兑换时出现一次。Server 不存储明文 secret，不将 secret 写入日志。
- **Client token 只在创建时返回一次**：后续不可查询。丢失 token 需要重新创建。
- **已认证 vs 未兑换**：`client-auth/*` 管理已认证的远程 client（区分于 pairing 的未兑换流程）。

---

# Auth

> 命名空间: `_anureo.dev/auth/*`
> Capability key: `auth`

## Capability

```json
{
  "auth": {
    "status": true
  }
}
```

### `_anureo.dev/auth/status`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 前置状态 | `initialize` 已成功；或 server pre-auth gate |
| 能力 | authenticated connection 使用 `auth.status`；pre-auth gate 为固定协议能力 |
| 权限 | 只读，不返回 credential/token |

认证前由 WebSocket pre-auth gate 返回密码/Passkey 配置；认证后的标准 ACP connection 由扩展 handler 返回当前 connection 已认证状态。Client 必须先完成 `initialize` 或 `authenticate`，再顺序发送 `auth/status`，不得与初始化并发。

```json
{
  "authenticated": true,
  "passwordConfigured": false,
  "passkeyEnabled": false,
  "hasPasskeys": false,
  "passkeyCount": 0,
  "rpId": null
}
```

---

# Connection

> 命名空间: `_anureo.dev/connection/*`
> Capability key: `connection`

## Capability

```json
{
  "connection": {
    "info": true,
    "capabilities": true
  }
}
```

## Rust 类型

```rust
pub struct ConnectionInfo {
    /// 连接类型
    pub transport: TransportType,
    /// 本地 endpoint
    pub local_endpoint: Option<String>,
    /// 远程 endpoint
    pub remote_endpoint: Option<String>,
    /// 连接建立时间
    pub connected_at: String,
    /// 当前认证身份
    pub auth_identity: Option<AuthIdentity>,
    /// Client scope
    pub client_scope: Vec<String>,
}

pub enum TransportType {
    Stdio,
    WebSocket,
    Relay,
}

pub struct AuthIdentity {
    /// 认证方式
    pub method: String,
    /// 脱敏标识（不返回完整 token）
    pub identity: String,
}

pub struct ConnectionCapabilities {
    /// 当前 transport 支持的 ACP 标准 capability
    pub standard: Vec<String>,
    /// 当前 transport 支持的扩展 domain
    pub extensions: Vec<String>,
    /// 限制说明（如 relay 不支持某些大 payload 操作）
    pub limitations: Vec<String>,
}
```

## Methods

---

### `_anureo.dev/connection/info`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `connection.info` |
| 权限 | Server policy（只读） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "transport": "websocket",
  "localEndpoint": "ws://0.0.0.0:51717",
  "remoteEndpoint": "127.0.0.1:54321",
  "connectedAt": "2025-08-19T10:00:00Z",
  "authIdentity": {
    "method": "bearer_token",
    "identity": "client-abc***"
  },
  "clientScope": ["session:read", "session:write", "git:read"]
}
```

**逻辑说明:**
- 返回当前 connection 的 transport 和认证信息。
- `authIdentity.identity` 为脱敏标识，不返回完整 token。
- `clientScope` 列出当前连接拥有的权限 scope。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | 获取连接信息失败 |

---

### `_anureo.dev/connection/capabilities`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `connection.capabilities` |
| 权限 | Server policy（只读） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "standard": [
    "initialize",
    "session/new",
    "session/load",
    "session/prompt",
    "session/update",
    "session/cancel",
    "fs/read_text_file",
    "fs/write_text_file",
    "terminal/create"
  ],
  "extensions": [
    "worktree",
    "git",
    "files",
    "mcp",
    "goal",
    "connection",
    "relay",
    "pairing",
    "client-auth"
  ],
  "limitations": [
    "relay transport: max payload size 1MB",
    "stdio transport: no concurrent sessions"
  ]
}
```

**逻辑说明:**
- 查询当前 transport 支持的 ACP 标准 capability 和扩展 domain。
- `limitations` 描述当前 transport 的限制条件。
- Client 可据此调整 UI 行为（如 relay transport 下隐藏大文件操作）。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | 查询失败 |

---

# Relay

> 命名空间: `_anureo.dev/relay/*`
> Capability key: `relay`

## Capability

```json
{
  "relay": {
    "status": true
  }
}
```

## Rust 类型

```rust
pub struct RelayStatus {
    /// Relay 是否已启用
    pub enabled: bool,
    /// Relay 是否已连接
    pub connected: bool,
    /// Relay server URL
    pub relay_url: Option<String>,
    /// 本地在 Relay 上的标识
    pub relay_id: Option<String>,
    /// 已连接的远程 client 数量
    pub remote_clients: u32,
    /// 最后连接时间
    pub last_connected: Option<String>,
    /// 错误信息
    pub last_error: Option<String>,
}
```

## Methods

---

### `_anureo.dev/relay/status`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `relay.status` |
| 权限 | Server policy（只读） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "enabled": true,
  "connected": true,
  "relayUrl": "wss://relay.anureo.dev",
  "relayId": "relay-abc123",
  "remoteClients": 2,
  "lastConnected": "2025-08-19T10:00:00Z",
  "lastError": null
}
```

**逻辑说明:**
- 返回 Relay 的连接状态。
- Relay 是 Anureo 自有的 E2EE tunnel transport，不是新的 ACP message protocol。
- `relayId` 为 relay 上的会话标识，可用于 pairing 流程中标识目标 server。
- Relay 连接经过 relay 不扩大 ACP capability——远程 client 仍需通过标准认证流程。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | 查询失败 |

---

## Relay 安全规则

1. **Relay 是 Transport**：Relay 只改变连接可达性，不改变 ACP 协议语义。
2. **不绕过 authorization**：通过 relay 连接的远程 client 仍需通过标准 server authorization（bearer token / pairing redeem）。
3. **E2EE**：Relay 使用端到端加密，relay server 无法解密 ACP 消息内容。
4. **Payload 限制**：Relay transport 可以有 payload 大小限制（通常 1MB），超出限制的操作应使用其他 transport。

---

# Pairing

> 命名空间: `_anureo.dev/pairing/*`
> Capability key: `pairing`

## Capability

```json
{
  "pairing": {
    "create": true,
    "redeem": true,
    "pending_list": true,
    "cancel": true,
    "transports": true
  }
}
```

## Rust 类型

```rust
pub struct PairingPayload {
    /// Pairing code（一次性，不落日志）
    pub secret: String,
    /// Pairing ID
    pub pairing_id: String,
    /// 过期时间
    pub expires_at: String,
    /// 可用 transport 候选
    pub transports: Vec<PairingTransport>,
}

pub struct PairingTransport {
    pub type: PairingTransportType,
    pub url: Option<String>,
    pub relay_id: Option<String>,
}

pub enum PairingTransportType {
    Direct,
    Relay,
}

pub struct PendingPairing {
    pub pairing_id: String,
    /// 创建时间
    pub created_at: String,
    /// 过期时间
    pub expires_at: String,
    /// 尝试兑换次数
    pub attempts: u32,
}

pub struct PairingCreateParams {
    /// 过期时间（秒），默认 300
    pub ttl_seconds: Option<u64>,
    /// 允许的 transport 类型
    pub allowed_transports: Option<Vec<String>>,
}

pub struct PairingRedeemParams {
    /// Pairing secret（一次性）
    pub secret: String,
    /// Client 标识信息
    pub client_info: ClientInfo,
}
```

## Methods

---

### `_anureo.dev/pairing/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `pairing.create` |
| 权限 | Server policy（scope: `pairing:create`） |

**Request:**

```json
{
  "ttlSeconds": 300,
  "allowedTransports": ["direct", "relay"]
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `ttlSeconds` | number? | 过期时间（秒），默认 300（5 分钟） |
| `allowedTransports` | string[]? | 允许的 transport 类型 |

**Response:**

```json
{
  "secret": "pair-secret-abc123-def456",
  "pairingId": "pair-001",
  "expiresAt": "2025-08-19T10:05:00Z",
  "transports": [
    {
      "type": "direct",
      "url": "ws://192.168.1.100:51717"
    },
    {
      "type": "relay",
      "relayId": "relay-abc123"
    }
  ]
}
```

**逻辑说明:**
- 创建一次性 pairing payload。
- **安全关键**：
  - `secret` 是一次性的，兑换后立即失效。
  - **secret 不落日志**：server 日志中不记录 secret 值。
  - **secret 不存储明文**：server 只存储 secret 的 hash，用于验证兑换。
  - **secret 只在 response 中出现一次**：后续不可查询。
- `transports` 返回可用的 transport 候选，client 可据此选择连接方式。

**Error:**

| kind | 触发条件 |
|---|---|
| `forbidden` | 无 `pairing:create` scope |
| `invalid_params` | ttlSeconds 超出允许范围 |
| `internal_error` | 生成 pairing 失败 |

---

### `_anureo.dev/pairing/redeem`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `pairing.redeem` |
| 权限 | Server policy |

**Request:**

```json
{
  "secret": "pair-secret-abc123-def456",
  "clientInfo": {
    "name": "Anureo iOS",
    "version": "1.0.0",
    "platform": "ios"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `secret` | string | Pairing secret（一次性） |
| `clientInfo` | object | Client 标识信息 |

**Response:**

```json
{
  "pairingId": "pair-001",
  "redeemed": true,
  "clientToken": "ct-xyz789-abc456",
  "transport": {
    "type": "relay",
    "relayId": "relay-abc123"
  }
}
```

**逻辑说明:**
- **安全关键**：
  - `secret` 为一次性使用，兑换成功后立即作废。
  - 兑换成功后返回 `clientToken`，用于后续连接的 bearer token。
  - **`clientToken` 只在此次 response 中返回一次**，后续不可查询。Server 存储 token 的 hash。
  - **`no-store`**：兑换过程中不将 secret 或 token 写入任何持久化日志。
  - Secret 过期后兑换返回 `forbidden`。
  - Secret 尝试次数超过限制后自动作废（防暴力破解）。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | secret 格式错误 |
| `forbidden` | secret 已过期、已兑换或不匹配 |
| `not_found` | pairing 不存在（secret 可能被撤销） |
| `internal_error` | 兑换过程失败 |

---

### `_anureo.dev/pairing/pending_list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `pairing.pending_list` |
| 权限 | Server policy（scope: `pairing:create`） |

**Request:**

```json
{
  "cursor": null,
  "limit": 20
}
```

**Response:**

```json
{
  "items": [
    {
      "pairingId": "pair-002",
      "createdAt": "2025-08-19T10:01:00Z",
      "expiresAt": "2025-08-19T10:06:00Z",
      "attempts": 0
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- 返回未兑换的 pairing 列表。
- **不返回 secret**：list 中只有 pairingId 和元数据，不包含 secret。
- 已过期或已兑换的 pairing 不出现在列表中。

**Error:**

| kind | 触发条件 |
|---|---|
| `forbidden` | 无 `pairing:create` scope |
| `internal_error` | 查询失败 |

---

### `_anureo.dev/pairing/cancel`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `pairing.cancel` |
| 权限 | Server policy（scope: `pairing:create`） |

**Request:**

```json
{
  "pairingId": "pair-002"
}
```

**Response:**

```json
{
  "pairingId": "pair-002",
  "cancelled": true
}
```

**逻辑说明:**
- 取消 pending pairing，使对应 secret 立即失效。
- 已兑换或已过期的 pairing 取消返回 `not_found`（无需取消）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | pairing 不存在或已非 pending |
| `forbidden` | 无 `pairing:create` scope |

---

### `_anureo.dev/pairing/transports`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `pairing.transports` |
| 权限 | Server policy（只读） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "transports": [
    {
      "type": "direct",
      "available": true,
      "url": "ws://192.168.1.100:51717",
      "label": "Local Network"
    },
    {
      "type": "relay",
      "available": true,
      "relayId": "relay-abc123",
      "label": "Cloud Relay"
    }
  ]
}
```

**逻辑说明:**
- 返回可用的 direct/relay transport 候选。
- `available = false` 表示 transport 不可用（如 relay 未连接）。
- Client 可据此在 pairing UI 中显示连接选项。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | 查询失败 |

---

# Client Auth

> 命名空间: `_anureo.dev/client-auth/*`
> Capability key: `client-auth`

已认证的远程 client 管理（区分于 pairing 的未兑换流程）。此域管理通过 pairing redeem 成功后的已认证 client。

## Capability

```json
{
  "client-auth": {
    "list": true,
    "create": true,
    "revoke": true,
    "purge_revoked": true
  }
}
```

## Rust 类型

```rust
pub struct ClientAuthEntry {
    /// Client 唯一标识
    pub client_id: String,
    /// 显示名称
    pub name: String,
    /// Client 平台
    pub platform: String,
    /// 认证方式
    pub auth_method: String,
    /// 创建时间
    pub created_at: String,
    /// 最后活跃时间
    pub last_active: Option<String>,
    /// 是否已撤销
    pub revoked: bool,
    /// 撤销时间
    pub revoked_at: Option<String>,
    /// Client scope
    pub scope: Vec<String>,
}

pub struct ClientAuthCreateParams {
    pub name: String,
    pub platform: String,
    /// 初始 scope
    pub scope: Vec<String>,
    /// 过期时间（秒），0 = 永不过期
    pub ttl_seconds: Option<u64>,
}
```

## Methods

---

### `_anureo.dev/client-auth/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `client-auth.list` |
| 权限 | Server policy（scope: `client-auth:manage`） |

**Request:**

```json
{
  "includeRevoked": false,
  "cursor": null,
  "limit": 50
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `includeRevoked` | boolean? | 是否包含已撤销的 client，默认 false |
| `cursor` | string? | 分页游标 |
| `limit` | number? | 每页数量 |

**Response:**

```json
{
  "items": [
    {
      "clientId": "ct-abc123",
      "name": "Anureo iOS",
      "platform": "ios",
      "authMethod": "pairing",
      "createdAt": "2025-08-19T10:00:00Z",
      "lastActive": "2025-08-19T11:30:00Z",
      "revoked": false,
      "revokedAt": null,
      "scope": ["session:read", "session:write", "git:read"]
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- 返回已认证的远程 client 列表。
- **不返回 token**：list 中只有 clientId 和元数据，不包含 client token（token 只在创建时返回一次）。
- `lastActive` 反映 client 最后一次活跃时间。

**Error:**

| kind | 触发条件 |
|---|---|
| `forbidden` | 无 `client-auth:manage` scope |
| `internal_error` | 查询失败 |

---

### `_anureo.dev/client-auth/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `client-auth.create` |
| 权限 | Server policy（scope: `client-auth:manage`） |

**Request:**

```json
{
  "name": "Anureo iPad",
  "platform": "ios",
  "scope": ["session:read", "session:write"],
  "ttlSeconds": 0
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `name` | string | Client 显示名称 |
| `platform` | string | Client 平台标识 |
| `scope` | string[] | 初始权限 scope |
| `ttlSeconds` | number? | 过期时间（秒），0 = 永不过期 |

**Response:**

```json
{
  "clientId": "ct-xyz789",
  "name": "Anureo iPad",
  "platform": "ios",
  "clientToken": "ct-token-abc123-def456-ghi789",
  "createdAt": "2025-08-19T10:00:00Z",
  "scope": ["session:read", "session:write"]
}
```

**逻辑说明:**
- 创建新的 client token。
- **安全关键**：
  - **`clientToken` 只在此次 response 中返回一次**，后续不可查询。
  - **Server 只存储 token 的 hash**，不存储明文。
  - **Token 不落日志**。
  - 丢失 token 需要撤销 client 后重新创建。
- `ttlSeconds = 0` 表示永不过期。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | name 为空或 scope 包含无效 scope |
| `forbidden` | 无 `client-auth:manage` scope |
| `internal_error` | 创建失败 |

---

### `_anureo.dev/client-auth/revoke`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `client-auth.revoke` |
| 权限 | Server policy（scope: `client-auth:manage`） |

**Request:**

```json
{
  "clientId": "ct-abc123"
}
```

**Response:**

```json
{
  "clientId": "ct-abc123",
  "revoked": true,
  "revokedAt": "2025-08-19T12:00:00Z"
}
```

**逻辑说明:**
- 撤销 client 认证。该 client 的 token 立即失效。
- 已撤销的 client 不能再建立新连接。
- 当前正在使用该 token 的活跃连接会被 server 主动断开。
- 撤销操作幂等：对已撤销的 client 再次 revoke 不报错。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | client 不存在 |
| `forbidden` | 无 `client-auth:manage` scope，或尝试撤销自己 |

---

### `_anureo.dev/client-auth/purge_revoked`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `client-auth.purge_revoked` |
| 权限 | Server policy（scope: `client-auth:manage`） |

**Request:**

```json
{
  "olderThanDays": 30
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `olderThanDays` | number? | 清理多少天前撤销的 client，默认 30 |

**Response:**

```json
{
  "purged": 5,
  "purgedAt": "2025-08-19T12:00:00Z"
}
```

**逻辑说明:**
- 清理已撤销的 client 记录。
- 只删除 `revokedAt` 在 `olderThanDays` 之前的记录。
- 删除后 client 记录不可恢复。

**Error:**

| kind | 触发条件 |
|---|---|
| `forbidden` | 无 `client-auth:manage` scope |
| `internal_error` | 清理失败 |

---

## 安全总结

| 安全规则 | 适用域 |
|---|---|
| Secret 一次性、兑换后作废 | `pairing/redeem` |
| Secret 不落日志、不存储明文 | `pairing/create`、`pairing/redeem` |
| Client token 只在创建时返回一次 | `client-auth/create`、`pairing/redeem` |
| Client token hash 存储、明文不持久化 | `client-auth/*` |
| Relay 不扩大 ACP capability | `relay/*` |
| Relay 是 transport 不是 protocol | `relay/*` |
| 远程 client 仍需标准 authorization | `relay/*`、`pairing/*` |
| 不撤销自身 | `client-auth/revoke` |
| 防暴力破解：兑换尝试次数限制 | `pairing/redeem` |

## Reconnect Resync

此域无需要 resync 的 notification。Connection 和 Relay 状态为实时查询，Pairing 和 Client Auth 的状态变化由 client 主动管理。

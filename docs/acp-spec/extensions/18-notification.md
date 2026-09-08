# Notification 扩展

> 命名空间: `_anureo.dev/notification/*`
> Capability key: `notification`
> 实现状态: ❌ 未实现

---

## Capability

```json
{
  "notification": {
    "vapid_public_key": true,
    "subscribe": true,
    "unsubscribe": true,
    "set_visibility": true,
    "apns_register": true,
    "apns_unregister": true,
    "test": true
  }
}
```

通知系统支持两条推送通道：

| 通道 | 平台 | 协议 | 用途 |
|---|---|---|---|
| Web Push (VAPID) | Web / Desktop (Electron) | HTTP Web Push API | 浏览器原生通知 |
| APNS | iOS / iPadOS | Apple Push Notification Service | 原生移动推送 |

**安全规则：**
- VAPID public key 可返回；VAPID **private key** 永不出现在 response 中
- APNS token 和 push endpoint **不进入** `session/update` 或扩展 response 的 `data` 中
- 通知内容截断和摘要由 server-side notification preparation 生成

---

## Methods

### `_anureo.dev/notification/vapid_public_key`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `notification.vapid_public_key` |
| 权限 | 无 |

获取 VAPID public key，用于 Web Push 订阅。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/notification/vapid_public_key",
  "params": {}
}
```

无参数。

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "vapidPublicKey": "BG3b...base64url-encoded-key...xyz",
    "enabled": true
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `vapidPublicKey` | string | base64url 编码的 VAPID public key |
| `enabled` | bool | Server 是否启用了 Web Push 功能 |

#### 逻辑说明

1. **固定 key**: VAPID key pair 由 server 启动时生成或从配置加载，整个 server 生命周期不变。
2. **禁用检查**: `enabled` 为 false 时，Client 不应尝试 subscribe。
3. **浏览器 API**: Client 使用此 key 调用 `PushManager.subscribe()`。

#### Rust 类型

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VapidPublicKeyResponse {
    pub vapid_public_key: String,
    pub enabled: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | VAPID key 未配置 |

---

### `_anureo.dev/notification/subscribe`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `notification.subscribe` |
| 权限 | 无 |

注册 Web Push (VAPID) 订阅。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/notification/subscribe",
  "params": {
    "subscription": {
      "endpoint": "https://fcm.googleapis.com/fcm/send/abc123...",
      "keys": {
        "p256dh": "BN4b...base64...",
        "auth": "x8j...base64..."
      }
    },
    "sessionId": "session-abc123"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `subscription` | object | 是 | PushSubscription 对象（来自浏览器 `PushManager.subscribe()`） |
| `subscription.endpoint` | string | 是 | Push service endpoint URL |
| `subscription.keys.p256dh` | string | 是 | 客户端公钥 |
| `subscription.keys.auth` | string | 是 | 认证密钥 |
| `sessionId` | string | 否 | 关联的 session ID（多 session 场景下精确路由） |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "subscribed": true,
    "subscriptionId": "sub-001"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `subscribed` | bool | 是否订阅成功 |
| `subscriptionId` | string | Server 内部订阅 ID |

#### 逻辑说明

1. **持久化**: Subscription 持久化到 server 端存储，跨连接有效。
2. **多设备**: 同一用户可有多个 subscription（不同浏览器/设备）。Server 维护 subscription 列表。
3. **Presence-aware**: Server 记录每个 subscription 对应的客户端是否在线（通过 ACP connection）。在线时可能不推送（取决于 notification policy），离线时推送。
4. **Endpoint 安全**: `subscription.keys.auth` 是敏感数据，存储在 server 端加密存储中，不出现在后续 response 中。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscriptionKeys {
    pub p256dh: String,
    pub auth: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PushSubscription {
    pub endpoint: String,
    pub keys: PushSubscriptionKeys,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationSubscribeRequest {
    pub subscription: PushSubscription,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationSubscribeResponse {
    pub subscribed: bool,
    pub subscription_id: String,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | subscription 格式不合法 |
| `Internal Error (-32603)` | 存储失败或 VAPID 未配置 |

---

### `_anureo.dev/notification/unsubscribe`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `notification.unsubscribe` |
| 权限 | 无 |

取消 Web Push 订阅。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_anureo.dev/notification/unsubscribe",
  "params": {
    "subscriptionId": "sub-001"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `subscriptionId` | string | 否 | 要取消的订阅 ID；省略则取消当前连接的所有订阅 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "unsubscribed": true,
    "count": 1
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `unsubscribed` | bool | 是否成功取消 |
| `count` | number | 取消的订阅数量 |

#### 逻辑说明

1. **幂等**: 取消不存在的 subscription 不报错，返回 `unsubscribed: false`。
2. **清理**: Server 从存储中删除 subscription 记录，后续不再向该 endpoint 推送。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationUnsubscribeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationUnsubscribeResponse {
    pub unsubscribed: bool,
    pub count: u32,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | 存储失败 |

---

### `_anureo.dev/notification/set_visibility`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `notification.set_visibility` |
| 权限 | 无 |

设置客户端可见性（presence-aware 路由）。Server 根据可见性决定是否推送通知。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "_anureo.dev/notification/set_visibility",
  "params": {
    "visible": true,
    "sessionId": "session-abc123"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `visible` | bool | 是 | `true` 表示客户端当前可见/活跃 |
| `sessionId` | string | 否 | 可见性限定到特定 session |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "acknowledged": true
  }
}
```

#### 逻辑说明

1. **Presence 路由**: Server 根据 visibility 决定通知投递方式：
   - `visible: true` → 通知通过 ACP connection 实时推送（不触发 Web Push / APNS）
   - `visible: false` → 通知通过 Web Push / APNS 推送（若已订阅）
2. **自动更新**: Client 可在页面 visibility change（`document.hidden`）时自动调用此方法。
3. **心跳**: Server 也可通过 ACP connection 的 keepalive 判断在线状态，`set_visibility` 是补充手段。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationSetVisibilityRequest {
    pub visible: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationSetVisibilityResponse {
    pub acknowledged: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | 状态更新失败 |

---

### `_anureo.dev/notification/apns_register`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `notification.apns_register` |
| 权限 | 无 |

注册 APNS device token（iOS/iPadOS 客户端）。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "_anureo.dev/notification/apns_register",
  "params": {
    "token": "a1b2c3d4e5f6...hex-device-token...",
    "bundleId": "dev.anureo.ios",
    "sessionId": "session-abc123"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `token` | string | 是 | APNS device token（十六进制字符串） |
| `bundleId` | string | 否 | App bundle identifier（用于区分开发/生产环境） |
| `sessionId` | string | 否 | 关联的 session ID |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "registered": true,
    "deviceId": "device-001"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `registered` | bool | 是否注册成功 |
| `deviceId` | string | Server 内部设备标识 |

#### 逻辑说明

1. **Token 更新**: APNS token 可能在 app 更新或系统恢复后变化。Client 应在每次启动时检查并重新注册。
2. **环境区分**: Server 根据 `bundleId` 或 token 前缀判断使用 APNS development 还是 production 环境。
3. **Token 安全**: APNS token 存储在 server 端加密存储中，不出现在后续 response 中。
4. **互斥**: 同一设备不应同时注册 Web Push 和 APNS。Server 以最后注册的通道为准。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApnsRegisterRequest {
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApnsRegisterResponse {
    pub registered: bool,
    pub device_id: String,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | token 格式不合法 |
| `Internal Error (-32603)` | APNS 配置未启用或存储失败 |

---

### `_anureo.dev/notification/apns_unregister`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `notification.apns_unregister` |
| 权限 | 无 |

注销 APNS device token。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "_anureo.dev/notification/apns_unregister",
  "params": {
    "deviceId": "device-001"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `deviceId` | string | 否 | 要注销的设备 ID；省略则注销当前连接的所有 APNS 注册 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "result": {
    "unregistered": true,
    "count": 1
  }
}
```

#### 逻辑说明

1. **触发时机**: 用户关闭通知权限或卸载 app 时，Client 应调用此方法。
2. **幂等**: 注销不存在的 device token 不报错。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApnsUnregisterRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApnsUnregisterResponse {
    pub unregistered: bool,
    pub count: u32,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | 存储失败 |

---

### `_anureo.dev/notification/test`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `notification.test` |
| 权限 | 无 |

发送测试通知，验证推送通道是否正常工作。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "_anureo.dev/notification/test",
  "params": {
    "channel": "web_push",
    "title": "Test Notification",
    "body": "This is a test from Anureo."
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `channel` | `"web_push"` \| `"apns"` \| `"auto"` | 否 | 测试通道，默认 `"auto"`（自动选择当前活跃通道） |
| `title` | string | 否 | 通知标题 |
| `body` | string | 否 | 通知正文 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "result": {
    "sent": true,
    "channel": "web_push",
    "message": "Notification sent to 1 endpoint"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `sent` | bool | 是否成功投递到 push service |
| `channel` | string | 实际使用的通道 |
| `message` | string | 人类可读的结果描述 |

#### 逻辑说明

1. **投递≠收到**: `sent: true` 表示 push service 接受了请求，不代表用户设备收到了通知（可能通知权限被关闭）。
2. **限流**: 测试通知有频率限制（如每分钟 1 次），防止滥用。
3. **无订阅**: 若未注册任何 subscription/token，返回 `sent: false` 并在 `message` 中说明原因。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotificationChannel {
    WebPush,
    Apns,
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationTestRequest {
    #[serde(default = "default_channel")]
    pub channel: NotificationChannel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

fn default_channel() -> NotificationChannel { NotificationChannel::Auto }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationTestResponse {
    pub sent: bool,
    pub channel: String,
    pub message: String,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | 指定 channel 未配置 |
| `Internal Error (-32603)` | Push service 不可达 |

---

## Notifications

本扩展无 notification。通知投递通过外部 push service（Web Push API / APNS）完成，不通过 ACP connection。

> 在线客户端的通知通过标准 `session/update` 或其他域的 notification（如 `session-assist/recap`）传递。本扩展只管理 push 通道的注册和配置。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| （无） | （无） | Push subscription 状态是 client 注册、server 存储；无需 resync |

> Push subscription 注册是一次性操作，不依赖 notification 同步。Client 重连后无需重新 subscribe（subscription 已持久化）。若 subscription 过期（push service 返回 410 Gone），Server 自动清理并在下次 subscribe 时重新建立。

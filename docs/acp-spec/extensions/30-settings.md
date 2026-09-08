# Settings（设置管理）

> 命名空间: `_anureo.dev/settings/*`
> Capability key: `settings`

## Capability

```json
{
  "settings": {
    "load": true,
    "save": true,
    "restart_opencode": true
  }
}
```

- 声明 `settings` capability 后，client 可以加载、保存设置，以及触发 OpenCode server 重启。
- Settings 包含 UI 偏好（主题、字体、diff 布局、git 变更视图模式等）、项目级配置和 input 行为偏好。
- `restart_opencode` 为高危操作，需要独立权限 scope。

### 敏感字段排除规则

以下字段**不进入跨 client 同步**（不通过 `settings/changed` 传播），仅在本地 client 持久化：

| 字段 | 原因 |
|---|---|
| `securityScopedBookmarks` | macOS 安全作用域书签，绑定本地文件系统路径 |
| `localFilePaths` | 本地绝对路径，跨设备无意义 |
| `oauthState` | OAuth 临时 state，不可复用 |
| `clientInstanceId` | 客户端唯一标识 |

---

## Methods

### `_anureo.dev/settings/load`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `settings.load` |
| 权限 | Server-side authorization（已认证连接即可） |

**Request:**

```json
{
  "keys": null
}
```

- `keys` 为 `null` 时加载全部设置；为数组时只加载指定 key。

**Response:**

```json
{
  "settings": {
    "ui": {
      "theme": "dark",
      "fontFamily": "JetBrains Mono",
      "fontSize": 14,
      "diffLayout": "split",
      "gitChangeViewMode": "tree"
    },
    "editor": {
      "tabSize": 2,
      "wordWrap": true,
      "minimap": false
    },
    "input": {
      "autoComplete": true,
      "sendOnEnter": true,
      "multilineSupport": true
    },
    "project": {
      "defaultMode": "code",
      "autoSaveInterval": 30
    },
    "notification": {
      "desktopNotifications": true,
      "soundEnabled": false
    }
  },
  "version": 15,
  "syncedAt": "2025-08-19T10:00:00Z"
}
```

**Rust 类型:**

```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize)]
pub struct SettingsLoadRequest {
    /// None = load all; Some(keys) = load only specified keys
    #[serde(default)]
    pub keys: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsLoadResponse {
    pub settings: serde_json::Value,
    /// Settings version for optimistic concurrency
    pub version: u64,
    pub synced_at: chrono::DateTime<chrono::Utc>,
}

/// Fields excluded from cross-client sync (settings/changed)
pub const SYNC_EXCLUDED_KEYS: &[&str] = &[
    "securityScopedBookmarks",
    "localFilePaths",
    "oauthState",
    "clientInstanceId",
];
```

**逻辑说明:**

1. Server 从 settings store 读取当前设置。
2. Settings 以扁平或嵌套 JSON 对象返回，server 不强制 schema——不同 runtime/client 可以有自定义字段。
3. `version` 为单调递增的设置版本号，用于 `settings/save` 的乐观并发控制。
4. `syncedAt` 为最后一次跨 client 同步的时间戳。
5. 返回的 settings 不包含敏感字段——`securityScopedBookmarks` 等由 client 本地管理，不存储在 server settings store 中。
6. `keys` 过滤为浅层 key 匹配（`"ui"` 返回整个 `ui` 对象）。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `settings.load` | initialize 未声明 |
| `internal_error` | Server 内部错误 | settings store 读取失败 |

---

### `_anureo.dev/settings/save`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `settings.save` |
| 权限 | Server-side authorization（需要写权限 scope） |
| 幂等 | 增量 merge，非全量替换 |

**Request:**

```json
{
  "changes": {
    "ui.theme": "light",
    "ui.fontSize": 16,
    "editor.wordWrap": false
  },
  "expectedVersion": 15
}
```

**Response:**

```json
{
  "applied": true,
  "version": 16,
  "merged": {
    "ui.theme": "light",
    "ui.fontSize": 16,
    "editor.wordWrap": false
  }
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct SettingsSaveRequest {
    /// Dot-notation key → value, merged into existing settings
    pub changes: HashMap<String, serde_json::Value>,
    /// Optimistic concurrency: must match current version
    #[serde(default)]
    pub expected_version: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsSaveResponse {
    pub applied: bool,
    pub version: u64,
    pub merged: HashMap<String, serde_json::Value>,
}
```

**逻辑说明:**

1. `changes` 使用 dot-notation（点分隔路径）表示增量变更，server 执行深度 merge。
2. `expectedVersion` 用于乐观并发控制：如果当前 server 版本号与 `expectedVersion` 不匹配，返回 `version_conflict` error，client 需重新 `load` 后重试。
3. `expectedVersion` 为 `null` 或省略时跳过版本检查（fire-and-forget 模式）。
4. Merge 完成后 version +1，触发 `settings/changed` 通知所有连接的 client。
5. **敏感字段排除**：如果 `changes` 中包含 `securityScopedBookmarks` 等排除键，server 静默忽略这些 key（不报错、不存储），并在 `merged` 中不包含它们。
6. 设置值为 `null` 时删除对应 key（语义为 unset）。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `settings.save` | initialize 未声明 |
| `forbidden` | 无写权限 | server authorization 拒绝 |
| `version_conflict` | 乐观并发冲突 | `expectedVersion` 与当前版本不匹配 |
| `invalid_params` | 参数校验失败 | `changes` 为空对象 |
| `internal_error` | Server 内部错误 | settings store 写入失败 |

---

### `_anureo.dev/settings/restart_opencode`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `settings.restart_opencode` |
| 权限 | Server-side authorization（需要高危操作 scope） |
| 超时 | 建议 60s（重启过程可能较长） |

**Request:**

```json
{
  "confirmToken": "restart-2025-08-19T10:00:00Z"
}
```

**Response:**

```json
{
  "restarted": true,
  "message": "OpenCode server is restarting. Please reconnect."
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct RestartOpencodeRequest {
    /// Client-generated confirm token to prevent accidental restart
    pub confirm_token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestartOpencodeResponse {
    pub restarted: bool,
    pub message: String,
}
```

**逻辑说明:**

1. 此操作重启整个 OpenCode server 进程——所有活跃 session 的 generation 会被 cancel，所有 connection 会断开。
2. `confirmToken` 为 client 生成的确认令牌（建议包含时间戳），server 不校验其内容但要求非空，作为防误触的 client-side gate。
3. 重启后 client 必须重新 `initialize` 并 `session/load` 恢复状态。
4. Server 在返回 response 后异步执行重启——response 表示重启已调度。
5. 此操作是破坏性的——server policy 应限制调用频率（如 1 次/分钟）。
6. 通常在修改了需要重启生效的配置（如 provider 凭据、MCP server 配置）后调用。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `settings.restart_opencode` | initialize 未声明 |
| `forbidden` | 无高危操作权限 | server authorization 拒绝 |
| `invalid_params` | 参数校验失败 | `confirmToken` 为空 |
| `rate_limited` | 调用频率超限 | 短时间内多次重启请求 |
| `internal_error` | Server 内部错误 | 重启调度失败 |

---

## Notifications

### `_anureo.dev/settings/changed`

| 项目 | 内容 |
|---|---|
| 方向 | Server → Client notification |
| 触发 | 任一 client 通过 `settings/save` 修改了设置 |

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/settings/changed",
  "params": {
    "version": 16,
    "changedKeys": ["ui.theme", "ui.fontSize", "editor.wordWrap"],
    "syncedAt": "2025-08-19T10:00:05Z"
  }
}
```

**params 字段:**

| 字段 | 类型 | 说明 |
|---|---|---|
| `version` | number | 新的设置版本号 |
| `changedKeys` | string[] | 本次变更涉及的 key 列表 |
| `syncedAt` | string | 同步时间戳 |

**逻辑说明:**

1. 通知发送给除发起 `settings/save` 的 client 外的所有连接的 client。
2. 发起方已通过 `settings/save` response 获知变更，不重复通知。
3. `changedKeys` 只包含同步允许的 key——**排除敏感字段**（`securityScopedBookmarks` 等）。
4. Client 收到后自行决定是否调用 `settings/load` 获取完整快照，或仅基于 `changedKeys` 做局部更新。
5. 如果 client 离线期间发生了多次变更，重连后必须通过 `settings/load` 获取完整快照。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `settings/changed` | `settings/load` | 完整设置快照（含 version） |

- Client 重连后必须调用 `settings/load` 获取完整设置快照。
- `version` 号可以帮助 client 检测是否错过了变更（与本地缓存版本对比）。
- 如果 `settings/load` 调用失败，client 必须保留旧设置（显示 stale 指示），不能当作空设置处理。

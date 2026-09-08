# Plugin 管理

> **命名空间**: `_anureo.dev/plugin/*`
> **Capability key**: `plugin`
> **实现状态**: ❌ 未实现

---

## Capability

```json
{
  "plugin": {
    "list": true,
    "install": true,
    "uninstall": true,
    "enable": true,
    "disable": true
  }
}
```

- Client 必须在 `initialize` 时声明 `agentCapabilities._meta["anureo.dev"].plugin` 的 method 粒度。
- Plugin 安装/卸载可能改变其他 domain 的 capability（如 MCP server、command、hook），安装/卸载成功后 server 应发送 `_anureo.dev/capability_changed` notification。

---

## Methods

### `_anureo.dev/plugin/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `plugin.list` |
| 权限 | 无（读取操作） |
| 分页 | 支持（`08-cross-cutting-patterns.md` §1） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/plugin/list",
  "params": {
    "cursor": null,
    "limit": 50
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | string \| null | 否 | 分页游标 |
| `limit` | int | 否 | 每页数量建议值 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "items": [
      {
        "id": "plg_001",
        "name": "git-enhanced",
        "version": "1.2.0",
        "description": "Enhanced Git integration with PR templates",
        "author": "Anureo",
        "homepage": "https://github.com/anureo/git-enhanced",
        "enabled": true,
        "installed": true,
        "state": "active",
        "capabilities": ["mcp", "command", "hook"],
        "mcpServers": ["git-enhanced-server"],
        "commands": ["/pr-template", "/sync-fork"],
        "hooks": ["pre-commit-check"],
        "installedAt": "2025-08-19T10:00:00Z",
        "updatedAt": "2025-08-19T10:00:00Z"
      }
    ],
    "nextCursor": null,
    "hasMore": false
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].id` | string | Plugin 唯一标识 |
| `items[].name` | string | Plugin 名称 |
| `items[].version` | string | 当前安装版本 |
| `items[].description` | string | 简短描述 |
| `items[].author` | string | 作者 |
| `items[].homepage` | string | 主页 URL |
| `items[].enabled` | bool | 是否启用 |
| `items[].installed` | bool | 是否已安装（未安装但可安装的 plugin 也可能出现） |
| `items[].state` | string | 运行时状态：`active` / `inactive` / `error` / `installing` |
| `items[].capabilities` | string[] | Plugin 提供的能力类型：`mcp` / `command` / `hook` |
| `items[].mcpServers` | string[] | 关联的 MCP server 名称列表 |
| `items[].commands` | string[] | 注册的 command 名称列表 |
| `items[].hooks` | string[] | 注册的 hook 名称列表 |
| `items[].installedAt` | string (ISO 8601) | 安装时间 |
| `items[].updatedAt` | string (ISO 8601) | 最后更新时间 |
| `nextCursor` | string \| null | 下一页游标 |
| `hasMore` | bool | 是否还有更多数据 |

#### 逻辑说明

1. Plugin 列表包含已安装和可安装（但未安装）的 plugin。
2. `state` 为 `error` 时，plugin 可能有加载错误，UI 应展示错误信息（通过 `errorMessage` 字段，可选）。
3. `mcpServers`、`commands`、`hooks` 帮助 UI 在卸载前展示影响范围。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginItem {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: Option<String>,
    pub homepage: Option<String>,
    pub enabled: bool,
    pub installed: bool,
    pub state: PluginState,
    pub capabilities: Vec<PluginCapability>,
    pub mcp_servers: Vec<String>,
    pub commands: Vec<String>,
    pub hooks: Vec<String>,
    pub error_message: Option<String>,
    pub installed_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginState {
    Active,
    Inactive,
    Error,
    Installing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginCapability {
    Mcp,
    Command,
    Hook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginListRequest {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginListResponse {
    pub items: Vec<PluginItem>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `plugin.list` 未声明 |
| `Invalid Params (-32602)` | cursor 格式非法 |

---

### `_anureo.dev/plugin/install`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `plugin.install` |
| 权限 | Server-side authorization（写操作） |
| 幂等 | 支持 `clientRequestId` 幂等键 |
| 进度 | 支持 progress notification（`08-cross-cutting-patterns.md` §3） |
| Timeout | 建议 120 秒 |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/plugin/install",
  "params": {
    "clientRequestId": "req-install-001",
    "source": "registry",
    "identifier": "anureo/git-enhanced",
    "version": "1.2.0",
    "autoEnable": true
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `clientRequestId` | string | 否 | 幂等键 |
| `source` | string | 是 | 安装来源：`registry` / `url` / `path` |
| `identifier` | string | 是 | Plugin 标识（registry 名称、URL 或本地路径） |
| `version` | string | 否 | 指定版本（不指定时安装最新） |
| `autoEnable` | bool | 否 | 安装后自动启用，默认 `true` |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "id": "plg_001",
    "name": "git-enhanced",
    "version": "1.2.0",
    "description": "Enhanced Git integration with PR templates",
    "enabled": true,
    "installed": true,
    "state": "active",
    "capabilities": ["mcp", "command", "hook"],
    "mcpServers": ["git-enhanced-server"],
    "commands": ["/pr-template", "/sync-fork"],
    "hooks": ["pre-commit-check"],
    "installedAt": "2025-08-19T10:00:00Z",
    "updatedAt": "2025-08-19T10:00:00Z"
  }
}
```

#### 逻辑说明

1. 安装过程可能包含下载、解压、依赖解析、注册 MCP server / command / hook。
2. 安装进度通过 `_anureo.dev/plugin/progress` notification 上报（`08-cross-cutting-patterns.md` §3 长时操作进度）。
3. 安装失败必须回滚到安装前状态（类似 `skills/install` 的回滚语义）。
4. 安装成功后：
   - 发送 `plugin/changed` notification。
   - 发送 `_anureo.dev/capability_changed` notification（新增 MCP server / command / hook 可用）。
   - 如果 plugin 注册了 command，同步触发 `available_commands_update`。
5. `source` 为 `path` 时，server 必须校验路径在允许的范围内（不允许任意本地路径安装）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInstallRequest {
    pub client_request_id: Option<String>,
    pub source: PluginSource,
    pub identifier: String,
    pub version: Option<String>,
    pub auto_enable: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PluginSource {
    Registry,
    Url,
    Path,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `plugin.install` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `source` 或 `identifier` 为空 |
| `not_found (-32004)` | registry / URL 中找不到指定 plugin |
| `conflict (-32003)` | 同名 plugin 已安装（除非支持覆盖安装） |
| `Internal Error (-32603)` | 安装过程中失败且无法回滚（partial result 见下方卸载逻辑） |

---

### `_anureo.dev/plugin/uninstall`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `plugin.uninstall` |
| 权限 | Server-side authorization（写操作） |
| 幂等 | 支持 `clientRequestId` 幂等键 |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_anureo.dev/plugin/uninstall",
  "params": {
    "clientRequestId": "req-uninstall-001",
    "id": "plg_001",
    "cleanup": true
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `clientRequestId` | string | 否 | 幂等键 |
| `id` | string | 是 | Plugin ID |
| `cleanup` | bool | 否 | 是否清理关联资源（MCP server / command / hook），默认 `true` |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "id": "plg_001",
    "deleted": true,
    "cleanup": {
      "mcpServers": {
        "removed": ["git-enhanced-server"],
        "failed": []
      },
      "commands": {
        "removed": ["/pr-template", "/sync-fork"],
        "failed": []
      },
      "hooks": {
        "removed": ["pre-commit-check"],
        "failed": []
      }
    },
    "errors": []
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `result.deleted` | bool | Plugin 是否已删除 |
| `result.cleanup` | object | 清理详情 |
| `result.cleanup.mcpServers.removed` | string[] | 成功移除的 MCP server |
| `result.cleanup.mcpServers.failed` | string[] | 移除失败的 MCP server |
| `result.cleanup.commands.removed` | string[] | 成功移除的 command |
| `result.cleanup.commands.failed` | string[] | 移除失败的 command |
| `result.cleanup.hooks.removed` | string[] | 成功移除的 hook |
| `result.cleanup.hooks.failed` | string[] | 移除失败的 hook |
| `result.errors` | string[] | 清理过程中的错误消息列表 |

#### 逻辑说明

1. **卸载必须清理关联资源**: `cleanup` 为 `true` 时，server 必须尝试移除该 plugin 注册的所有 MCP server、command 和 hook。
2. **Partial failure 语义**: 即使部分清理失败，plugin 本身仍被卸载。Server 在 response 中报告 partial result：
   - `deleted: true` 表示 plugin 已从注册表中移除。
   - `cleanup.*.failed` 列出未能清理的关联资源。
   - `errors` 包含人类可读的错误描述。
3. Client 收到 partial failure 后应提示用户手动处理残留资源。
4. 卸载成功后：
   - 发送 `plugin/changed` notification。
   - 发送 `_anureo.dev/capability_changed` notification（移除的 MCP server / command / hook）。
   - 如果移除了 command，同步触发 `available_commands_update`。
5. `cleanup` 为 `false` 时，只移除 plugin 注册记录，不清理关联资源（适用于调试场景）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUninstallRequest {
    pub client_request_id: Option<String>,
    pub id: String,
    pub cleanup: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CleanupResult {
    pub mcp_servers: CleanupDetail,
    pub commands: CleanupDetail,
    pub hooks: CleanupDetail,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CleanupDetail {
    pub removed: Vec<String>,
    pub failed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginUninstallResponse {
    pub id: String,
    pub deleted: bool,
    pub cleanup: Option<CleanupResult>,
    pub errors: Vec<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `plugin.uninstall` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |

---

### `_anureo.dev/plugin/enable`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `plugin.enable` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "_anureo.dev/plugin/enable",
  "params": {
    "id": "plg_001"
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "id": "plg_001",
    "enabled": true,
    "state": "active"
  }
}
```

#### 逻辑说明

1. 启用 plugin 会激活其注册的 MCP server、command 和 hook。
2. 启用失败（如 MCP server 启动失败）时，plugin 状态为 `error`，response 中包含 `errorMessage`。
3. 启用成功后发送 `plugin/changed` 和 `_anureo.dev/capability_changed` notification。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEnableRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEnableResponse {
    pub id: String,
    pub enabled: bool,
    pub state: PluginState,
    pub error_message: Option<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `plugin.enable` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |

---

### `_anureo.dev/plugin/disable`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `plugin.disable` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "_anureo.dev/plugin/disable",
  "params": {
    "id": "plg_001"
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "id": "plg_001",
    "enabled": false,
    "state": "inactive"
  }
}
```

#### 逻辑说明

1. 禁用 plugin 会停用其注册的 MCP server、command 和 hook，但不卸载。
2. 禁用是可逆操作，后续可通过 `plugin/enable` 恢复。
3. 禁用成功后发送 `plugin/changed` 和 `_anureo.dev/capability_changed` notification。
4. 如果 plugin 注册了 command，禁用后同步触发 `available_commands_update`。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDisableRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginDisableResponse {
    pub id: String,
    pub enabled: bool,
    pub state: PluginState,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `plugin.disable` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |

---

## Notifications

### `_anureo.dev/plugin/changed`

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/plugin/changed",
  "params": {
    "change": "installed | uninstalled | enabled | disabled | updated",
    "id": "plg_001"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | string | 变更类型 |
| `id` | string | 受影响的 plugin ID |

- Client 收到后必须调用 `plugin/list` 进行完整 resync。
- Plugin 变更通常伴随 `_anureo.dev/capability_changed` notification，Client 需要同时处理两者。

### `_anureo.dev/plugin/progress`

安装过程中的进度 notification（`08-cross-cutting-patterns.md` §3 长时操作进度）：

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/plugin/progress",
  "params": {
    "operationId": "req-install-001",
    "progress": 45,
    "phase": "downloading",
    "message": "Downloading git-enhanced v1.2.0...",
    "cancelable": true
  }
}
```

| 阶段 (`phase`) | 说明 |
|---|---|
| `downloading` | 下载 plugin 包 |
| `extracting` | 解压 |
| `resolving_dependencies` | 解析依赖 |
| `registering_mcp` | 注册 MCP server |
| `registering_commands` | 注册 command |
| `registering_hooks` | 注册 hook |
| `activating` | 激活 plugin |

取消通过 JSON-RPC `notifications/cancelled`（ID 为 `operationId`）。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `plugin/changed` | `plugin/list` | 完整 plugin 列表 |

Client 重连后若丢失 notification，必须调用 `plugin/list` 获取完整快照。Plugin 变更可能影响多个 domain 的 capability，Client 应同时检查 `_anureo.dev/capability_changed` 是否需要处理。

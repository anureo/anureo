# Project Config 项目配置

> **命名空间**: `_anureo.dev/project/*`
> **Capability key**: `project`
> **实现状态**: ✅ 已实现（`apps/acp/src/extensions/project.rs`；`create`/`remove` 于 2025-08 加入）
> **持久化**: `anureo_home()/projects.json`（原子写，重启保留）

---

## Capability

```json
{
  "project": {
    "list": true,
    "get": true,
    "create": true,
    "remove": true,
    "update": true,
    "icon": true
  }
}
```

- Client 必须在 `initialize` 时声明 `agentCapabilities._meta["anureo.dev"].project` 的 method 粒度。
- Project 是 Anureo 中的工作区概念——每个 project 对应一个工作目录，可以拥有独立的 agent profile、MCP 配置、command 集等。

---

## Methods

### `_anureo.dev/project/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `project.create` |
| 权限 | 需 principal + session（`-32002`） |
| 幂等 | 按 path 规范化后匹配：同 path 重复 create 直接返回已有记录（`existed: true`） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/project/create",
  "params": {
    "path": "C:\\Users\\heycj\\dev\\anureo",
    "preferredId": "dev-anureo",
    "name": "anureo",
    "color": "#4A90D9",
    "defaultModel": "anthropic/claude",
    "iconBackground": "#112233",
    "sidebarCollapsed": false
  }
}
```

- 仅 `path` 必填，其余可选；`name` 缺省取路径末段。
- `preferredId` 可选：非空、≤128、`[A-Za-z0-9._-]`；被占用 → `-32005 conflict`。
- 无 `preferredId` 时生成 `proj-<FNV-1a 路径哈希 10 位>`。
- Response 为与 `get` 相同的 snapshot，外加 `existed: bool`。

### `_anureo.dev/project/remove`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `project.remove` |
| 权限 | 需 principal + session（`-32002`） |

#### Request

```json
{ "jsonrpc": "2.0", "id": 1, "method": "_anureo.dev/project/remove",
  "params": { "id": "proj_001" } }
```

#### Response

```json
{ "jsonrpc": "2.0", "id": 1, "result": { "removed": true, "id": "proj_001" } }
```

- 不存在 → `-32003 not_found`；不删除磁盘上的项目文件，仅注销注册。

### `_anureo.dev/project/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `project.list` |
| 权限 | 无（读取操作） |
| 分页 | 支持（`08-cross-cutting-patterns.md` §1） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/project/list",
  "params": {
    "cursor": null,
    "limit": 50
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "items": [
      {
        "id": "proj_001",
        "name": "anureo",
        "path": "/home/user/dev/anureo",
        "description": "anureo ACP backend",
        "icon": "custom",
        "iconUrl": null,
        "color": "#4A90D9",
        "isActive": true,
        "agentProfile": "default",
        "mcpServers": ["git-enhanced"],
        "sessionCount": 12,
        "lastOpenedAt": "2025-08-19T14:00:00Z",
        "createdAt": "2025-08-01T10:00:00Z",
        "updatedAt": "2025-08-19T10:00:00Z"
      },
      {
        "id": "proj_002",
        "name": "anureo",
        "path": "/home/user/dev/anureo",
        "description": "anureo frontend",
        "icon": "react",
        "iconUrl": null,
        "color": "#61DAFB",
        "isActive": false,
        "agentProfile": "architect",
        "mcpServers": [],
        "sessionCount": 5,
        "lastOpenedAt": "2025-08-18T09:00:00Z",
        "createdAt": "2025-08-05T10:00:00Z",
        "updatedAt": "2025-08-18T09:00:00Z"
      }
    ],
    "nextCursor": null,
    "hasMore": false
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].id` | string | Project 唯一标识 |
| `items[].name` | string | 项目名称 |
| `items[].path` | string | 项目工作目录（server 解析的绝对路径） |
| `items[].description` | string | 项目描述 |
| `items[].icon` | string | 图标标识：`custom`（自定义）/ 内置图标名 |
| `items[].iconUrl` | string \| null | 自定义图标 URL（base64 data URL 或文件路径） |
| `items[].color` | string | UI 主题色 |
| `items[].isActive` | bool | 是否为当前活跃项目 |
| `items[].agentProfile` | string | 默认 agent profile |
| `items[].mcpServers` | string[] | 关联的 MCP server 列表 |
| `items[].sessionCount` | int | 项目下的 session 数量 |
| `items[].lastOpenedAt` | string (ISO 8601) | 最后打开时间 |
| `items[].createdAt` | string (ISO 8601) | 创建时间 |
| `items[].updatedAt` | string (ISO 8601) | 最后更新时间 |
| `nextCursor` | string \| null | 下一页游标 |
| `hasMore` | bool | 是否还有更多数据 |

#### 逻辑说明

1. `path` 为 server 解析后的规范路径，client 不能通过 list 修改路径。
2. `isActive` 标记当前 connection 绑定的项目。切换项目通过 client 内部逻辑（可能触发新的 `initialize` 或 session 绑定）。
3. `sessionCount` 为预估值，不保证实时精确。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectItem {
    pub id: String,
    pub name: String,
    pub path: String,
    pub description: String,
    pub icon: String,
    pub icon_url: Option<String>,
    pub color: String,
    pub is_active: bool,
    pub agent_profile: Option<String>,
    pub mcp_servers: Vec<String>,
    pub session_count: u32,
    pub last_opened_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectListRequest {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectListResponse {
    pub items: Vec<ProjectItem>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `project.list` 未声明 |
| `Invalid Params (-32602)` | cursor 格式非法 |

---

### `_anureo.dev/project/get`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `project.get` |
| 权限 | 无（读取操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/project/get",
  "params": {
    "id": "proj_001"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string | 是 | Project ID |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "id": "proj_001",
    "name": "anureo",
    "path": "/home/user/dev/anureo",
    "description": "anureo ACP backend",
    "icon": "custom",
    "iconUrl": null,
    "color": "#4A90D9",
    "isActive": true,
    "agentProfile": "default",
    "mcpServers": ["git-enhanced"],
    "config": {
      "defaultMode": "default",
      "defaultModel": "claude-sonnet-4-20250514",
      "defaultProvider": "anthropic",
      "allowFileAccess": true,
      "allowedPaths": [],
      "denyPatterns": ["**/.env", "**/secrets/**"],
      "env": {
        "RUST_LOG": "info"
      },
      "mcp": {
        "git-enhanced": {
          "type": "stdio",
          "command": "git-enhanced-mcp",
          "args": []
        }
      },
      "gitIdentity": "personal"
    },
    "sessionCount": 12,
    "lastOpenedAt": "2025-08-19T14:00:00Z",
    "createdAt": "2025-08-01T10:00:00Z",
    "updatedAt": "2025-08-19T10:00:00Z"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `result.config` | object | 项目级配置详情 |
| `result.config.defaultMode` | string | 默认 agent mode |
| `result.config.defaultModel` | string | 默认模型 |
| `result.config.defaultProvider` | string | 默认 provider |
| `result.config.allowFileAccess` | bool | 是否允许文件访问 |
| `result.config.allowedPaths` | string[] | 额外允许的路径 |
| `result.config.denyPatterns` | string[] | 禁止访问的 glob 模式 |
| `result.config.env` | object | 环境变量覆盖 |
| `result.config.mcp` | object | MCP server 配置 |
| `result.config.gitIdentity` | string | Git 身份 profile |

> 其他字段同 `project/list` 中的 item。

#### 逻辑说明

1. `project/get` 返回比 `project/list` 更详细的配置信息（包含 `config` 对象）。
2. `config.denyPatterns` 中的模式不包含敏感信息（只是 glob 模式），但 server 应确保 `config.env` 中的环境变量值已脱敏（如包含 key/token 的值替换为 `****`）。
3. `config.allowedPaths` 和 `config.denyPatterns` 由 server 强制执行，client 不能绕过。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectGetRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    pub default_mode: Option<String>,
    pub default_model: Option<String>,
    pub default_provider: Option<String>,
    pub allow_file_access: bool,
    pub allowed_paths: Vec<String>,
    pub deny_patterns: Vec<String>,
    pub env: HashMap<String, String>,
    pub mcp: HashMap<String, McpServerConfig>,
    pub git_identity: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    #[serde(rename = "type")]
    pub server_type: String,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectGetResponse {
    pub id: String,
    pub name: String,
    pub path: String,
    pub description: String,
    pub icon: String,
    pub icon_url: Option<String>,
    pub color: String,
    pub is_active: bool,
    pub agent_profile: Option<String>,
    pub mcp_servers: Vec<String>,
    pub config: ProjectConfig,
    pub session_count: u32,
    pub last_opened_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `project.get` 未声明 |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |

---

### `_anureo.dev/project/update`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `project.update` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_anureo.dev/project/update",
  "params": {
    "id": "proj_001",
    "name": "anureo (ACP Backend)",
    "description": "anureo ACP backend implementation",
    "color": "#E8A838",
    "agentProfile": "architect",
    "config": {
      "defaultMode": "architect",
      "defaultModel": "claude-opus-4-20250514",
      "denyPatterns": ["**/.env", "**/secrets/**", "**/target/**"],
      "env": {
        "RUST_LOG": "debug"
      }
    }
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string | 是 | Project ID |
| `name` | string | 否 | 新名称 |
| `description` | string | 否 | 新描述 |
| `color` | string | 否 | 新主题色 |
| `agentProfile` | string | 否 | 新默认 agent profile |
| `config.defaultMode` | string | 否 | 新默认 mode |
| `config.defaultModel` | string | 否 | 新默认模型 |
| `config.defaultProvider` | string | 否 | 新默认 provider |
| `config.allowFileAccess` | bool | 否 | 文件访问开关 |
| `config.allowedPaths` | string[] | 否 | 新允许路径 |
| `config.denyPatterns` | string[] | 否 | 新禁止模式 |
| `config.env` | object | 否 | 新环境变量（增量 merge） |
| `config.mcp` | object | 否 | MCP 配置变更 |
| `config.gitIdentity` | string | 否 | Git 身份 |

#### Response

返回更新后的完整 `ProjectGetResponse`（结构同 `project/get`）。

#### 逻辑说明

1. 增量更新：未提供的字段保持不变。
2. `config.env` 为增量 merge——只更新提供的 key，不替换整个 env 对象。要删除某个 key，将其值设为 `null`。
3. `config.allowedPaths` 和 `config.denyPatterns` 为整体替换（提供时覆盖原有值）。
4. 修改 `config.mcp` 需要权限——server 校验 MCP 配置的合法性。
5. 更新成功后发送 `project/changed` notification。
6. 如果更新了 `agentProfile` 或 `config.defaultMode`，当前活跃 session 的 mode 不会自动切换——需要显式调用 `session/set_mode`。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectUpdateRequest {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub color: Option<String>,
    pub agent_profile: Option<String>,
    pub config: Option<ProjectConfigUpdate>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfigUpdate {
    pub default_mode: Option<String>,
    pub default_model: Option<String>,
    pub default_provider: Option<String>,
    pub allow_file_access: Option<bool>,
    pub allowed_paths: Option<Vec<String>>,
    pub deny_patterns: Option<Vec<String>>,
    pub env: Option<HashMap<String, Option<String>>>,
    pub mcp: Option<HashMap<String, Option<McpServerConfig>>>,
    pub git_identity: Option<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `project.update` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |
| `conflict (-32003)` | `agentProfile` 引用不存在的 profile |

---

### `_anureo.dev/project/icon`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `project.icon` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "_anureo.dev/project/icon",
  "params": {
    "id": "proj_001",
    "icon": "custom",
    "iconData": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA..."
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string | 是 | Project ID |
| `icon` | string | 是 | 图标类型：`custom` / `none`（移除自定义图标）/ 内置图标名 |
| `iconData` | string | `icon=custom` 时必填 | Base64 data URL（支持 `image/png`、`image/jpeg`、`image/svg+xml`） |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "id": "proj_001",
    "icon": "custom",
    "iconUrl": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAA..."
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `result.id` | string | Project ID |
| `result.icon` | string | 图标类型 |
| `result.iconUrl` | string \| null | 图标数据 URL |

#### 逻辑说明

1. `iconData` 为 base64 编码的 data URL，支持 PNG/JPEG/SVG 格式。
2. Server 应限制图标大小（建议 < 256KB），超出时返回 `Invalid Params`。
3. `icon` 设为 `none` 时移除自定义图标，恢复为默认行为（基于项目名称首字母或内置图标）。
4. Server 存储图标数据后，在 `project/list` 和 `project/get` 的 `iconUrl` 字段中返回。
5. 更新成功后发送 `project/changed` notification。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIconRequest {
    pub id: String,
    pub icon: String,
    pub icon_data: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIconResponse {
    pub id: String,
    pub icon: String,
    pub icon_url: Option<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `project.icon` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空、`icon=custom` 但缺少 `iconData`、图标格式不支持、图标过大 |
| `not_found (-32004)` | `id` 不存在 |

---

## Notifications

### `_anureo.dev/project/changed`

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/project/changed",
  "params": {
    "change": "updated | icon_changed",
    "id": "proj_001"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | string | 变更类型 |
| `id` | string | 受影响的项目 ID |

- Client 收到后必须调用 `project/get`（当前项目）或 `project/list`（全部项目）进行完整 resync。
- 项目配置变更可能影响多个 domain（MCP server 列表、agent profile 可用性等），Client 应检查是否需要刷新关联 UI。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `project/changed` | `project/get` | 当前项目配置 |

Client 重连后若丢失 notification，必须调用 `project/get`（当前活跃项目）或 `project/list`（全部项目）获取完整快照。`project/get` 返回的配置为权威快照，client 不得使用缓存推断。

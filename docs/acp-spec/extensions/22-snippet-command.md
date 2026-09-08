# Snippet 与 Command 管理

> **命名空间**: `_anureo.dev/snippet/*`、`_anureo.dev/command/*`
> **Capability key**: `snippet`、`command`
> **实现状态**: ❌ 未实现

---

## Capability

```json
{
  "snippet": {
    "list": true,
    "create": true,
    "update": true,
    "delete": true
  },
  "command": {
    "list": true,
    "create": true,
    "update": true,
    "delete": true
  }
}
```

- Client 必须在 `initialize` 时声明 `agentCapabilities._meta["anureo.dev"].snippet` 和 `.command` 的 method 粒度。
- 未声明的 method 对 UI 隐藏，对 request 返回 `capability_not_supported`。
- `session/update` 中的 `available_commands_update` 只通知可用 command 列表的变化（如 command 被新增/删除/启停后，UI 显示的 slash command 列表更新）；实际的 CRUD 由本扩展负责。

---

## 第一部分: Snippet

### `_anureo.dev/snippet/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `snippet.list` |
| 权限 | 无（读取操作） |
| 分页 | 支持（`08-cross-cutting-patterns.md` §1） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/snippet/list",
  "params": {
    "cursor": null,
    "limit": 50
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | string \| null | 否 | 分页游标，首次请求省略或 `null` |
| `limit` | int | 否 | 每页数量建议值，server 可 clamp |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "items": [
      {
        "id": "snip_001",
        "name": "Explain Error",
        "description": "Explain the last error in detail",
        "body": "Please explain the following error in detail:\n\n{{error}}",
        "variables": ["error"],
        "category": "debugging",
        "tags": ["error", "debug"],
        "createdAt": "2025-08-19T10:00:00Z",
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
| `items[].id` | string | Snippet 唯一标识 |
| `items[].name` | string | 显示名称 |
| `items[].description` | string | 简短描述 |
| `items[].body` | string | Snippet 模板正文，支持 `{{variable}}` 占位符 |
| `items[].variables` | string[] | 模板变量列表 |
| `items[].category` | string | 分类标识（UI 分组用） |
| `items[].tags` | string[] | 标签列表 |
| `items[].createdAt` | string (ISO 8601) | 创建时间 |
| `items[].updatedAt` | string (ISO 8601) | 最后更新时间 |
| `nextCursor` | string \| null | 下一页游标 |
| `hasMore` | bool | 是否还有更多数据 |

#### 逻辑说明

1. Snippet 是 UI 层的快捷 prompt 模板，不直接影响 agent 行为。
2. Client 在 UI 中展示 snippet 列表，用户选择后填充变量并作为 `session/prompt` 的内容发送。
3. 小型集合（< 100 项且增长缓慢）可忽略分页参数，返回 `nextCursor: null`。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetItem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub body: String,
    pub variables: Vec<String>,
    pub category: Option<String>,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetListRequest {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetListResponse {
    pub items: Vec<SnippetItem>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `snippet.list` 未声明 |
| `Invalid Params (-32602)` | cursor 格式非法 |

---

### `_anureo.dev/snippet/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `snippet.create` |
| 权限 | Server-side authorization（写操作） |
| 幂等 | 支持 `clientRequestId` 幂等键 |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/snippet/create",
  "params": {
    "clientRequestId": "req-abc-123",
    "name": "Explain Error",
    "description": "Explain the last error in detail",
    "body": "Please explain the following error in detail:\n\n{{error}}",
    "variables": ["error"],
    "category": "debugging",
    "tags": ["error", "debug"]
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `clientRequestId` | string | 否 | 幂等键，防止重复创建 |
| `name` | string | 是 | Snippet 名称 |
| `description` | string | 否 | 简短描述 |
| `body` | string | 是 | 模板正文 |
| `variables` | string[] | 否 | 变量列表（可从 body 自动推导） |
| `category` | string | 否 | 分类 |
| `tags` | string[] | 否 | 标签 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "id": "snip_001",
    "name": "Explain Error",
    "description": "Explain the last error in detail",
    "body": "Please explain the following error in detail:\n\n{{error}}",
    "variables": ["error"],
    "category": "debugging",
    "tags": ["error", "debug"],
    "createdAt": "2025-08-19T10:00:00Z",
    "updatedAt": "2025-08-19T10:00:00Z"
  }
}
```

#### 逻辑说明

1. Server 生成 `id`、`createdAt`、`updatedAt`，忽略 client 传入的时间戳。
2. `variables` 未提供时，server 可从 `body` 中自动解析 `{{variable}}` 占位符。
3. `name` 唯一性由 server 决定——允许重名或要求唯一，取决于实现策略。
4. 创建成功后，server 发送 `snippet/changed` notification。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetCreateRequest {
    pub client_request_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub body: String,
    pub variables: Option<Vec<String>>,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `snippet.create` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `name` 或 `body` 为空 |

---

### `_anureo.dev/snippet/update`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `snippet.update` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_anureo.dev/snippet/update",
  "params": {
    "id": "snip_001",
    "name": "Explain Error (Updated)",
    "description": "Explain the last error with code context",
    "body": "Please explain the following error with code context:\n\n{{error}}\n\nContext:\n{{context}}",
    "variables": ["error", "context"],
    "category": "debugging",
    "tags": ["error", "debug", "context"]
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string | 是 | Snippet ID |
| `name` | string | 否 | 新名称 |
| `description` | string | 否 | 新描述 |
| `body` | string | 否 | 新模板正文 |
| `variables` | string[] | 否 | 新变量列表 |
| `category` | string | 否 | 新分类 |
| `tags` | string[] | 否 | 新标签 |

#### Response

与 `snippet/create` 的 response 结构相同，返回更新后的完整 snippet。

#### 逻辑说明

1. 增量更新：未提供的字段保持不变。
2. `updatedAt` 由 server 自动更新为当前时间。
3. 更新成功后，server 发送 `snippet/changed` notification。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetUpdateRequest {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub body: Option<String>,
    pub variables: Option<Vec<String>>,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `snippet.update` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |

---

### `_anureo.dev/snippet/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `snippet.delete` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "_anureo.dev/snippet/delete",
  "params": {
    "id": "snip_001"
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "id": "snip_001",
    "deleted": true
  }
}
```

#### 逻辑说明

1. 删除后 server 发送 `snippet/changed` notification。
2. 删除不存在的 snippet 返回 `not_found`（idempotent delete 可选：若已删除则返回 `deleted: false`）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetDeleteRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnippetDeleteResponse {
    pub id: String,
    pub deleted: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `snippet.delete` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |

---

## 第二部分: Command 管理

> **与 `session/update` 的关系**: `session/update` 中的 `available_commands_update` 只通知可用 command 列表的变化（如新增/删除/启停 command 后，UI 显示的 slash command 列表更新）。实际的 command CRUD 由本扩展的 `_anureo.dev/command/*` 方法负责。

### `_anureo.dev/command/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `command.list` |
| 权限 | 无（读取操作） |
| 分页 | 支持（`08-cross-cutting-patterns.md` §1） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "_anureo.dev/command/list",
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
  "id": 5,
  "result": {
    "items": [
      {
        "id": "cmd_001",
        "name": "/test",
        "description": "Run the project test suite",
        "promptTemplate": "Please run the test suite and report results.",
        "enabled": true,
        "scope": "project",
        "agentMode": "default",
        "icon": "test-tube",
        "shortcut": "ctrl+t",
        "createdAt": "2025-08-19T10:00:00Z",
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
| `items[].id` | string | Command 唯一标识 |
| `items[].name` | string | Command 名称（slash command，如 `/test`） |
| `items[].description` | string | 简短描述 |
| `items[].promptTemplate` | string | Command 触发时发送的 prompt 模板 |
| `items[].enabled` | bool | 是否启用 |
| `items[].scope` | string | 作用域：`global` / `project` |
| `items[].agentMode` | string | 触发时切换到的 agent mode（可选） |
| `items[].icon` | string | UI 图标标识 |
| `items[].shortcut` | string | 快捷键绑定（可选） |
| `items[].createdAt` | string (ISO 8601) | 创建时间 |
| `items[].updatedAt` | string (ISO 8601) | 最后更新时间 |
| `nextCursor` | string \| null | 下一页游标 |
| `hasMore` | bool | 是否还有更多数据 |

#### 逻辑说明

1. Command 是 UI 层的 slash command 定义，用户触发后将其 `promptTemplate` 作为 `session/prompt` 内容发送。
2. `enabled` 为 `false` 的 command 在 UI 中灰显或隐藏，`available_commands_update` notification 会反映该变化。
3. `scope` 为 `project` 的 command 只在对应项目目录下可见。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandItem {
    pub id: String,
    pub name: String,
    pub description: String,
    pub prompt_template: String,
    pub enabled: bool,
    pub scope: CommandScope,
    pub agent_mode: Option<String>,
    pub icon: Option<String>,
    pub shortcut: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CommandScope {
    Global,
    Project,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandListRequest {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandListResponse {
    pub items: Vec<CommandItem>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `command.list` 未声明 |
| `Invalid Params (-32602)` | cursor 格式非法 |

---

### `_anureo.dev/command/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `command.create` |
| 权限 | Server-side authorization（写操作） |
| 幂等 | 支持 `clientRequestId` 幂等键 |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "_anureo.dev/command/create",
  "params": {
    "clientRequestId": "req-def-456",
    "name": "/test",
    "description": "Run the project test suite",
    "promptTemplate": "Please run the test suite and report results.",
    "scope": "project",
    "agentMode": "default",
    "icon": "test-tube",
    "shortcut": "ctrl+t"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `clientRequestId` | string | 否 | 幂等键 |
| `name` | string | 是 | Command 名称（必须以 `/` 开头） |
| `description` | string | 否 | 简短描述 |
| `promptTemplate` | string | 是 | Prompt 模板 |
| `scope` | string | 否 | 作用域，默认 `global` |
| `agentMode` | string | 否 | 触发时切换的 agent mode |
| `icon` | string | 否 | UI 图标 |
| `shortcut` | string | 否 | 快捷键 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "result": {
    "id": "cmd_001",
    "name": "/test",
    "description": "Run the project test suite",
    "promptTemplate": "Please run the test suite and report results.",
    "enabled": true,
    "scope": "project",
    "agentMode": "default",
    "icon": "test-tube",
    "shortcut": "ctrl+t",
    "createdAt": "2025-08-19T10:00:00Z",
    "updatedAt": "2025-08-19T10:00:00Z"
  }
}
```

#### 逻辑说明

1. Server 生成 `id`（`enabled` 默认 `true`）、`createdAt`、`updatedAt`。
2. `name` 必须以 `/` 开头，且在同一 scope 内唯一。
3. 创建成功后，server 发送 `command/changed` notification，同时触发 `session/update` 中的 `available_commands_update`（如有活跃 session）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandCreateRequest {
    pub client_request_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub prompt_template: String,
    pub scope: Option<CommandScope>,
    pub agent_mode: Option<String>,
    pub icon: Option<String>,
    pub shortcut: Option<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `command.create` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `name` 不以 `/` 开头、`promptTemplate` 为空 |
| `conflict (-32003)` | `name` 在同一 scope 下已存在 |

---

### `_anureo.dev/command/update`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `command.update` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "_anureo.dev/command/update",
  "params": {
    "id": "cmd_001",
    "description": "Run the full test suite with coverage",
    "promptTemplate": "Please run the full test suite with coverage and report results.",
    "enabled": false,
    "icon": "shield-check",
    "shortcut": "ctrl+shift+t"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string | 是 | Command ID |
| `name` | string | 否 | 新名称 |
| `description` | string | 否 | 新描述 |
| `promptTemplate` | string | 否 | 新 prompt 模板 |
| `enabled` | bool | 否 | 启用/禁用 |
| `scope` | string | 否 | 新作用域 |
| `agentMode` | string | 否 | 新 agent mode |
| `icon` | string | 否 | 新图标 |
| `shortcut` | string | 否 | 新快捷键 |

#### Response

返回更新后的完整 `CommandItem`（结构同 `command/list` 中的 item）。

#### 逻辑说明

1. 增量更新：未提供的字段保持不变。
2. 如果更新了 `enabled` 或 `name`，server 同步触发 `available_commands_update`。
3. 更新成功后，server 发送 `command/changed` notification。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandUpdateRequest {
    pub id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub prompt_template: Option<String>,
    pub enabled: Option<bool>,
    pub scope: Option<CommandScope>,
    pub agent_mode: Option<String>,
    pub icon: Option<String>,
    pub shortcut: Option<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `command.update` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空、`name` 不以 `/` 开头 |
| `not_found (-32004)` | `id` 不存在 |
| `conflict (-32003)` | 修改后的 `name` 在同一 scope 下冲突 |

---

### `_anureo.dev/command/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `command.delete` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "method": "_anureo.dev/command/delete",
  "params": {
    "id": "cmd_001"
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "result": {
    "id": "cmd_001",
    "deleted": true
  }
}
```

#### 逻辑说明

1. 删除后 server 发送 `command/changed` notification，同时触发 `available_commands_update`（从可用列表中移除）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDeleteRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandDeleteResponse {
    pub id: String,
    pub deleted: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `command.delete` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |

---

## Notifications

### `_anureo.dev/snippet/changed`

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/snippet/changed",
  "params": {
    "change": "created | updated | deleted",
    "id": "snip_001"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | string | 变更类型 |
| `id` | string | 受影响的 snippet ID |

- Client 收到后必须调用 `snippet/list` 进行完整 resync。
- 未识别的 notification params 必须安全忽略。

### `_anureo.dev/command/changed`

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/command/changed",
  "params": {
    "change": "created | updated | deleted",
    "id": "cmd_001"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | string | 变更类型 |
| `id` | string | 受影响的 command ID |

- Client 收到后必须调用 `command/list` 进行完整 resync。
- 此 notification 与 `session/update` 中的 `available_commands_update` 是互补关系：前者通知数据层 CRUD 变更，后者通知运行时可用列表变更。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `snippet/changed` | `snippet/list` | 完整 snippet 列表 |
| `command/changed` | `command/list` | 完整 command 列表 |

Client 重连后若丢失 notification，必须调用 authoritative method 获取完整快照，不能使用缓存或增量推断。

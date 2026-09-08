# Agent Profile 管理

> **命名空间**: `_anureo.dev/agent/*`
> **Capability key**: `agent`
> **实现状态**: ❌ 未实现

---

## Capability

```json
{
  "agent": {
    "list": true,
    "create": true,
    "update": true,
    "delete": true
  }
}
```

- Client 必须在 `initialize` 时声明 `agentCapabilities._meta["anureo.dev"].agent` 的 method 粒度。
- **与 `session/set_mode` 的关系**: `session/set_mode` 负责运行时切换当前 session 的 agent mode（即时生效）；本扩展负责 agent profile 的持久化 CRUD（配置管理）。Profile 定义了可用的 agent mode、其系统 prompt、工具集、模型配置等。

---

## Methods

### `_anureo.dev/agent/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agent.list` |
| 权限 | 无（读取操作） |
| 分页 | 支持（`08-cross-cutting-patterns.md` §1） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/agent/list",
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
        "id": "agent_default",
        "name": "default",
        "displayName": "Default",
        "description": "General-purpose coding assistant",
        "systemPrompt": "You are a helpful coding assistant.",
        "model": "claude-sonnet-4-20250514",
        "provider": "anthropic",
        "tools": ["read", "write", "edit", "grep", "glob", "bash"],
        "toolChoice": "auto",
        "temperature": 0.7,
        "maxTokens": 8192,
        "contextWindow": 200000,
        "isBuiltIn": true,
        "isActive": false,
        "icon": "code",
        "color": "#4A90D9",
        "tags": ["coding", "general"],
        "createdAt": "2025-08-01T10:00:00Z",
        "updatedAt": "2025-08-19T10:00:00Z"
      },
      {
        "id": "agent_architect",
        "name": "architect",
        "displayName": "Architect",
        "description": "System design and architecture review",
        "systemPrompt": "You are a senior software architect. Focus on design patterns, trade-offs, and long-term maintainability.",
        "model": "claude-opus-4-20250514",
        "provider": "anthropic",
        "tools": ["read", "grep", "glob", "lsp"],
        "toolChoice": "auto",
        "temperature": 0.3,
        "maxTokens": 16384,
        "contextWindow": 200000,
        "isBuiltIn": false,
        "isActive": true,
        "icon": "building",
        "color": "#E8A838",
        "tags": ["architecture", "design"],
        "createdAt": "2025-08-10T10:00:00Z",
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
| `items[].id` | string | Profile 唯一标识 |
| `items[].name` | string | Profile 标识（用于 `session/set_mode`） |
| `items[].displayName` | string | UI 显示名称 |
| `items[].description` | string | 简短描述 |
| `items[].systemPrompt` | string | 系统 prompt |
| `items[].model` | string | 默认模型 |
| `items[].provider` | string | 默认 provider |
| `items[].tools` | string[] | 可用工具列表 |
| `items[].toolChoice` | string | 工具选择策略：`auto` / `none` / `required` |
| `items[].temperature` | number | 温度参数 |
| `items[].maxTokens` | int | 最大输出 token 数 |
| `items[].contextWindow` | int | 上下文窗口大小 |
| `items[].isBuiltIn` | bool | 是否为内置 profile（不可删除） |
| `items[].isActive` | bool | 是否为当前活跃 profile（每个 connection 只有一个 active） |
| `items[].icon` | string | UI 图标标识 |
| `items[].color` | string | UI 主题色 |
| `items[].tags` | string[] | 标签列表 |
| `items[].createdAt` | string (ISO 8601) | 创建时间 |
| `items[].updatedAt` | string (ISO 8601) | 最后更新时间 |
| `nextCursor` | string \| null | 下一页游标 |
| `hasMore` | bool | 是否还有更多数据 |

#### 逻辑说明

1. Profile 列表包含内置 profile（`isBuiltIn: true`）和用户自定义 profile。
2. `isActive` 标记当前 connection 正在使用的 profile。多个 connection 可能使用不同的 profile。
3. `name` 与 `session/set_mode` 的 mode 参数对应——切换 mode 即切换 profile。
4. `systemPrompt` 中可能包含模板变量，在 session 初始化时由 server 渲染。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub description: String,
    pub system_prompt: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tools: Vec<String>,
    pub tool_choice: ToolChoice,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub context_window: Option<u32>,
    pub is_built_in: bool,
    pub is_active: bool,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListRequest {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListResponse {
    pub items: Vec<AgentProfile>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `agent.list` 未声明 |
| `Invalid Params (-32602)` | cursor 格式非法 |

---

### `_anureo.dev/agent/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agent.create` |
| 权限 | Server-side authorization（写操作） |
| 幂等 | 支持 `clientRequestId` 幂等键 |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/agent/create",
  "params": {
    "clientRequestId": "req-agent-create-001",
    "name": "architect",
    "displayName": "Architect",
    "description": "System design and architecture review",
    "systemPrompt": "You are a senior software architect. Focus on design patterns, trade-offs, and long-term maintainability.",
    "model": "claude-opus-4-20250514",
    "provider": "anthropic",
    "tools": ["read", "grep", "glob", "lsp"],
    "toolChoice": "auto",
    "temperature": 0.3,
    "maxTokens": 16384,
    "icon": "building",
    "color": "#E8A838",
    "tags": ["architecture", "design"]
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `clientRequestId` | string | 否 | 幂等键 |
| `name` | string | 是 | Profile 名称（唯一，用于 `session/set_mode`） |
| `displayName` | string | 否 | UI 显示名称 |
| `description` | string | 否 | 简短描述 |
| `systemPrompt` | string | 是 | 系统 prompt |
| `model` | string | 否 | 默认模型 |
| `provider` | string | 否 | 默认 provider |
| `tools` | string[] | 否 | 可用工具列表 |
| `toolChoice` | string | 否 | 工具选择策略，默认 `auto` |
| `temperature` | number | 否 | 温度参数 |
| `maxTokens` | int | 否 | 最大输出 token |
| `icon` | string | 否 | UI 图标 |
| `color` | string | 否 | UI 主题色 |
| `tags` | string[] | 否 | 标签 |

#### Response

返回创建的完整 `AgentProfile`（结构同 `agent/list` 中的 item），包含 server 生成的 `id`、`createdAt`、`updatedAt`、`isBuiltIn: false`、`isActive: false`。

#### 逻辑说明

1. Server 生成 `id`、`createdAt`、`updatedAt`。
2. `name` 必须全局唯一，不能与内置 profile 名称冲突。
3. 创建成功后发送 `agent/changed` notification。
4. 新建的 profile 不会自动激活——需要通过 `session/set_mode` 切换。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCreateRequest {
    pub client_request_id: Option<String>,
    pub name: String,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub system_prompt: String,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tools: Option<Vec<String>>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub tags: Option<Vec<String>>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `agent.create` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `name` 或 `systemPrompt` 为空 |
| `conflict (-32003)` | `name` 已存在 |

---

### `_anureo.dev/agent/update`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agent.update` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_anureo.dev/agent/update",
  "params": {
    "id": "agent_architect",
    "displayName": "Software Architect",
    "description": "System design, architecture review, and code quality",
    "systemPrompt": "You are a senior software architect. Focus on design patterns, trade-offs, scalability, and long-term maintainability.",
    "model": "claude-opus-4-20250514",
    "temperature": 0.2,
    "maxTokens": 32768
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `id` | string | 是 | Profile ID |
| `name` | string | 否 | 新名称 |
| `displayName` | string | 否 | 新显示名称 |
| `description` | string | 否 | 新描述 |
| `systemPrompt` | string | 否 | 新系统 prompt |
| `model` | string | 否 | 新模型 |
| `provider` | string | 否 | 新 provider |
| `tools` | string[] | 否 | 新工具列表 |
| `toolChoice` | string | 否 | 新工具选择策略 |
| `temperature` | number | 否 | 新温度 |
| `maxTokens` | int | 否 | 新最大 token |
| `icon` | string | 否 | 新图标 |
| `color` | string | 否 | 新主题色 |
| `tags` | string[] | 否 | 新标签 |

#### Response

返回更新后的完整 `AgentProfile`。

#### 逻辑说明

1. 增量更新：未提供的字段保持不变。
2. 更新内置 profile 时，`name` 不可修改（返回 `forbidden`）。
3. 如果更新的 profile 当前正在使用（`isActive: true`），更新立即生效于新发起的 session/prompt，不影响进行中的 generation。
4. 更新成功后发送 `agent/changed` notification。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentUpdateRequest {
    pub id: String,
    pub name: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    pub system_prompt: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub tools: Option<Vec<String>>,
    pub tool_choice: Option<ToolChoice>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u32>,
    pub icon: Option<String>,
    pub color: Option<String>,
    pub tags: Option<Vec<String>>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `agent.update` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝；或尝试修改内置 profile 的 `name` |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |
| `conflict (-32003)` | 修改后的 `name` 已被其他 profile 使用 |

---

### `_anureo.dev/agent/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agent.delete` |
| 权限 | Server-side authorization（写操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "_anureo.dev/agent/delete",
  "params": {
    "id": "agent_architect"
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "id": "agent_architect",
    "deleted": true
  }
}
```

#### 逻辑说明

1. **删除正在使用的 profile 必须返回 `forbidden`**。Client 应先通过 `session/set_mode` 切换到其他 profile，再删除。
2. 内置 profile（`isBuiltIn: true`）不可删除，返回 `forbidden`。
3. 删除成功后发送 `agent/changed` notification。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDeleteRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDeleteResponse {
    pub id: String,
    pub deleted: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `agent.delete` 未声明 |
| `forbidden (-32002)` | Profile 为 `isActive: true`（正在使用）；或 `isBuiltIn: true`（内置不可删除） |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |

---

## Notifications

### `_anureo.dev/agent/changed`

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/agent/changed",
  "params": {
    "change": "created | updated | deleted",
    "id": "agent_architect"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | string | 变更类型 |
| `id` | string | 受影响的 profile ID |

- Client 收到后必须调用 `agent/list` 进行完整 resync。
- Profile 变更可能影响 `session/set_mode` 的可用 mode 列表，Client 应同步更新 UI 中的 mode 选择器。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `agent/changed` | `agent/list` | 完整 profile 列表 |

Client 重连后若丢失 notification，必须调用 `agent/list` 获取完整快照。如果当前 session 的 active profile 在断连期间被另一个 client 删除，session 将继续使用内存中的 profile 配置直到下次 `session/set_mode` 或重连。

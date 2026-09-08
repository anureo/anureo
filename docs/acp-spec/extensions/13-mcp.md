# MCP 管理

> 命名空间: `_anureo.dev/mcp/*`
> Capability key: `mcp`

## 设计原则

- **MCP tool invocation 仍遵循 MCP 协议**：Agent 调用 MCP server 提供的工具时，走标准 MCP tool call 流程，不经过此扩展。
- **扩展只管理 MCP 配置和状态**：此扩展负责 MCP server 的列表查询、配置修改、启用/停用管理，不涉及 MCP 工具的实际调用。
- **配置变更触发重载**：修改 MCP 配置后，server 内部负责 MCP client 的重连/断开。

## Capability

```json
{
  "mcp": {
    "list": true,
    "get": true,
    "configure": true,
    "enable": true,
    "disable": true
  }
}
```

## Rust 类型

```rust
pub struct McpServerEntry {
    /// MCP server 标识
    pub id: String,
    /// 显示名称
    pub name: String,
    /// 启动命令或连接 URL
    pub transport: McpTransport,
    /// 是否已启用
    pub enabled: bool,
    /// 运行状态
    pub status: McpServerStatus,
    /// 提供的工具数量
    pub tool_count: u32,
    /// 最后连接时间
    pub last_connected: Option<String>,
    /// 是否为项目级配置
    pub scope: McpScope,
}

pub enum McpTransport {
    /// stdio 子进程
    Stdio { command: String, args: Vec<String>, env: HashMap<String, String> },
    /// SSE / HTTP
    Sse { url: String },
    /// WebSocket
    WebSocket { url: String },
}

pub enum McpServerStatus {
    Connected,
    Disconnected,
    Error,
    Starting,
    Disabled,
}

pub enum McpScope {
    /// 用户全局配置
    Global,
    /// 项目级配置
    Project,
}

pub struct McpConfigureParams {
    pub id: String,
    pub name: Option<String>,
    pub transport: Option<McpTransport>,
    pub enabled: Option<bool>,
    /// 是否覆盖已有配置
    pub overwrite: Option<bool>,
}

pub struct McpEnableParams {
    pub id: String,
}

pub struct McpDisableParams {
    pub id: String,
}
```

## Methods

---

### `_anureo.dev/mcp/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `mcp.list` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "scope": null,
  "cursor": null,
  "limit": 50
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `scope` | string? | 筛选配置范围：`global` / `project`；省略返回全部 |
| `cursor` | string? | 分页游标 |
| `limit` | number? | 每页数量 |

**Response:**

```json
{
  "items": [
    {
      "id": "filesystem",
      "name": "Filesystem MCP",
      "transport": {
        "type": "stdio",
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]
      },
      "enabled": true,
      "status": "connected",
      "toolCount": 5,
      "lastConnected": "2025-08-19T10:00:00Z",
      "scope": "project"
    },
    {
      "id": "github-api",
      "name": "GitHub MCP",
      "transport": {
        "type": "sse",
        "url": "http://localhost:3001/sse"
      },
      "enabled": false,
      "status": "disabled",
      "toolCount": 0,
      "lastConnected": null,
      "scope": "global"
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- 返回所有已配置的 MCP server（全局 + 项目级）。
- `transport` 中的环境变量 `env` 字段不返回（安全考虑）。
- `status` 反映 server 内部 MCP client 的实时连接状态。
- `toolCount` 来自最后一次成功的 MCP `tools/list` 调用。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | 配置读取失败 |

---

### `_anureo.dev/mcp/get`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `mcp.get` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "id": "filesystem"
}
```

**Response:**

```json
{
  "id": "filesystem",
  "name": "Filesystem MCP",
  "transport": {
    "type": "stdio",
    "command": "npx",
    "args": ["-y", "@modelcontextprotocol/server-filesystem", "/workspace"]
  },
  "enabled": true,
  "status": "connected",
  "toolCount": 5,
  "lastConnected": "2025-08-19T10:00:00Z",
  "lastError": null,
  "scope": "project",
  "tools": [
    {
      "name": "read_file",
      "description": "Read file contents"
    },
    {
      "name": "write_file",
      "description": "Write file contents"
    }
  ]
}
```

**逻辑说明:**
- 返回单个 MCP server 的详细信息，包含最近一次获取的工具列表。
- `lastError` 非 null 时表示上次连接或工具调用出错。
- `tools` 来自 MCP `tools/list` 结果的脱敏摘要。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | MCP server 不存在 |

---

### `_anureo.dev/mcp/configure`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `mcp.configure` |
| 权限 | Server policy（scope: `mcp:write`） |

**Request:**

```json
{
  "id": "new-server",
  "name": "New MCP Server",
  "transport": {
    "type": "stdio",
    "command": "node",
    "args": ["server.js"],
    "env": { "API_KEY": "secret-value" }
  },
  "enabled": true,
  "overwrite": false
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | MCP server 标识（唯一） |
| `name` | string? | 显示名称 |
| `transport` | object? | 启动命令或连接 URL |
| `transport.env` | object? | 环境变量（含可能的 secret） |
| `enabled` | boolean? | 是否启用 |
| `overwrite` | boolean? | 已存在时是否覆盖 |

**Response:**

```json
{
  "id": "new-server",
  "configured": true,
  "status": "starting"
}
```

**逻辑说明:**
- 创建或更新 MCP server 配置。
- 配置写入后，如果 `enabled = true`，server 自动启动 MCP client 连接。
- `status = "starting"` 表示 server 正在初始化连接。
- **安全**：`transport.env` 中的 secret 值由 server 加密存储，不出现在后续 `list`/`get` response 中。
- `overwrite = false` 且 id 已存在时返回 `already_exists`。

**Error:**

| kind | 触发条件 |
|---|---|
| `already_exists` | id 已存在且 `overwrite = false` |
| `invalid_params` | transport 格式错误或 command 不存在 |
| `forbidden` | 无 `mcp:write` scope |
| `internal_error` | 配置写入失败 |

---

### `_anureo.dev/mcp/enable`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `mcp.enable` |
| 权限 | Server policy（scope: `mcp:write`） |

**Request:**

```json
{
  "id": "github-api"
}
```

**Response:**

```json
{
  "id": "github-api",
  "enabled": true,
  "status": "starting"
}
```

**逻辑说明:**
- 启用已配置但当前 disabled 的 MCP server。
- Server 启动 MCP client 连接流程，状态变为 `starting` → `connected`。
- 如果 server 之前因 error 停用，enable 会尝试重新连接。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | MCP server 不存在 |
| `forbidden` | 无 `mcp:write` scope |
| `internal_error` | 启动失败（MCP server 进程无法启动等） |

---

### `_anureo.dev/mcp/disable`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `mcp.disable` |
| 权限 | Server policy（scope: `mcp:write`） |

**Request:**

```json
{
  "id": "filesystem"
}
```

**Response:**

```json
{
  "id": "filesystem",
  "enabled": false,
  "status": "disconnected"
}
```

**逻辑说明:**
- 停用 MCP server。
- Server 断开 MCP client 连接，终止子进程（stdio transport）。
- 当前正在执行的 MCP tool call 不被中断（等待完成或超时）。
- 已停用的 server 仍在配置列表中可见，但 `status = "disabled"`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | MCP server 不存在 |
| `forbidden` | 无 `mcp:write` scope |
| `internal_error` | 停用失败 |

---

## Notifications

### `_anureo.dev/mcp/status_changed`

当 MCP server 状态发生变化（连接、断开、错误、工具列表更新）时推送。

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/mcp/status_changed",
  "params": {
    "id": "filesystem",
    "status": "disconnected",
    "lastError": "Connection reset by peer"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | MCP server ID |
| `status` | string | 新状态：`connected` / `disconnected` / `error` / `starting` / `disabled` |
| `lastError` | string? | 如果 status = error，附带错误信息 |

- notification 丢失后，client 必须调用 `mcp/list` 获取完整列表。
- notification 只推送状态变更提示。

## 安全注意

1. **MCP server 环境变量中的 secret**：由 server 加密存储，不返回到 `list`/`get` response 中。
2. **stdio command 安全**：server 可以对 command 做白名单检查或沙箱限制。
3. **MCP tool 调用不经此扩展**：Agent 运行时调用 MCP tool 走标准 MCP 协议（`tools/call`），由 server 内部 MCP client 处理。

## Reconnect Resync

| Notification | Authoritative method |
|---|---|
| `_anureo.dev/mcp/status_changed` | `_anureo.dev/mcp/list` |

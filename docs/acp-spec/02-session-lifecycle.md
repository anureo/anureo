# Session 生命周期

> **命名空间**: 标准 ACP v1
> **实现状态**: ✅ 已实现
> **源码**: `apps/acp/src/agent.rs`、`apps/acp/src/session.rs`、`apps/acp/src/session_repository.rs`、`apps/acp/src/session_bindings.rs`、`apps/acp/src/session_config_store.rs`

---

## 实体关系

```text
connection
  └── session (持久化到 SQLite)
        └── generation (一次 prompt 执行代次)
              └── notification sink (session/update 输出通道)
```

| 实体 | Rust 类型 | ID 空间 |
|---|---|---|
| connection | `AcpConnection` | `ConnectionId` (String) |
| session | `SessionEntry` | `SessionId` (String, 等同 anureo thread_id) |
| generation | `GenerationCancellation` | 隐含在 session 的 active prompt 中 |

**关键规则**: `connectionId ≠ sessionId`。anureo 将 session 绑定到 thread/checkpoint，`sessionId` 等同于 anureo 的 `thread_id`。

---

## 1. `session/new`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | 无（initialize 后即可调用） |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "session/new",
  "params": {
    "workingDirectory": "/home/user/project",
    "mcpServers": [
      {
        "name": "my-server",
        "transport": "http",
        "url": "https://example.com/mcp"
      }
    ]
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `workingDirectory` | string | 否 | 工作目录路径 |
| `mcpServers` | array | 否 | MCP server 配置列表 |
| `mcpServers[].name` | string | 是 | server 名称 |
| `mcpServers[].transport` | string | 是 | `"http"` 或 `"stdio"` |
| `mcpServers[].url` | string | http 时 | HTTP endpoint |
| `mcpServers[].command` | string | stdio 时 | 可执行文件路径 |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "sessionId": "thread-abc123"
  }
}
```

### 逻辑说明

1. anureo 创建新 thread（checkpoint store），生成 `thread_id` 作为 `sessionId`
2. 绑定 `workingDirectory` 到 session，用于后续 prompt 和工具执行
3. MCP server 配置通过 `mcp_convert.rs` 转换为 anureo 的 `anureoMcpServer` 格式
4. 初始化 `SessionConfig`（`current_agent`、`model`、`effort` 从默认值加载）
5. 绑定到当前 connection 和 owner principal
6. 持久化 session 元数据到 SQLite（`SessionRepository::insert`）

### Rust 类型

```rust
async fn new_session(&self, args: NewSessionRequest)
    -> agent_client_protocol::Result<NewSessionResponse>

async fn new_session_for_owner(
    &self, args: NewSessionRequest, owner_principal: &str
) -> agent_client_protocol::Result<NewSessionResponse>

// SessionEntry
pub struct SessionEntry {
    pub thread_id: String,
    pub working_directory: Option<PathBuf>,
    pub owner_principal: String,
    pub session_config: SessionConfig,
    pub mcp_servers: Vec<anureoMcpServer>,
    pub mcp_runtime: Arc<McpRuntime>,
    pub cancellation: Option<GenerationCancellation>,
    pub state: Arc<SessionState>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub struct SessionConfig {
    pub current_agent: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}
```

### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `workingDirectory` 不存在或不可访问 |
| `Internal Error (-32603)` | checkpoint store 写入失败 |

---

## 2. `session/load`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agentCapabilities.loadSession = true`（anureo 已声明） |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "session/load",
  "params": {
    "sessionId": "thread-abc123",
    "workingDirectory": "/home/user/project",
    "mcpServers": []
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sessionId` | string | 是 | 已存在的 session ID |
| `workingDirectory` | string | 否 | 覆盖工作目录 |
| `mcpServers` | array | 否 | 覆盖 MCP server 配置 |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "sessionId": "thread-abc123"
  }
}
```

### 逻辑说明

1. 验证 session 存在且属于当前 owner principal
2. 从 checkpoint store 恢复历史上下文
3. 从 `SessionConfigStore` (SQLite) 恢复 session 配置
4. 重新绑定到当前 connection
5. **历史内容通过 `session/update` notification 推送**（`user_message_chunk`、`agent_message_chunk` 等）
6. **不能把历史记录当作当前 generation 仍在运行的证据**——当前运行状态必须来自 live generation state

> **尾部重放（tail replay，2025-08）**：第 5 步默认仅重放最近 50 条原始消息
> （`ANUREO_ACP_LOAD_HISTORY_TAIL` 可调，`0` = 全量），截断起点记入
> `SessionEntry.history_cursor`；边界自动向前扩展，绝不以 `Tool` 消息开头
> （工具结果必须跟随其发起的 `Assistant`）。更早历史由客户端按需通过
> [`_anureo.dev/session-history/page`](extensions/36-session-history.md) 分页拉取。
> 线上形态为单条 `_anureo.dev/session-history/batch` 批量通知
> （`ANUREO_ACP_LOAD_HISTORY_BATCH=0` 回退逐条），replay 标记与缓冲抑制行为不变。

### Rust 类型

```rust
async fn load_session(&self, args: LoadSessionRequest)
    -> agent_client_protocol::Result<LoadSessionResponse>

async fn load_session_for_owner(
    &self, args: LoadSessionRequest, owner_principal: &str
) -> agent_client_protocol::Result<LoadSessionResponse>
```

### Error

| Error code | 触发条件 |
|---|---|
| `session_not_found` | session 不存在或不属于当前连接 |
| `Invalid Params (-32602)` | `workingDirectory` 无效 |

---

## 3. `session/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agentCapabilities.sessionCapabilities.list` |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "session/list",
  "params": {
    "cwd": "/home/user/project",
    "cursor": null
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cwd` | string | 否 | 按工作目录过滤 |
| `cursor` | string | 否 | 分页游标 |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "sessions": [
      {
        "sessionId": "thread-abc123",
        "cwd": "/home/user/project",
        "title": "Fix bug in auth module",
        "updatedAt": "2025-08-19T10:30:00Z",
        "meta": {
          "checkpointCount": 15,
          "messageCount": 42,
          "latestStep": 41,
          "latestSource": "agent",
          "review": {
            "status": "approved",
            "reviewedAt": "2025-08-19T11:00:00Z",
            "memoryUpdateCount": 2,
            "skillUpdateCount": 0
          }
        }
      }
    ]
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `sessions[].sessionId` | string | session ID（thread_id） |
| `sessions[].cwd` | string | 工作目录 |
| `sessions[].title` | string | session 标题 |
| `sessions[].updatedAt` | string | 最后更新时间 |
| `sessions[].meta.checkpointCount` | int | checkpoint 数量 |
| `sessions[].meta.messageCount` | int | 消息数量 |
| `sessions[].meta.review` | object | background review 结果 |

### 逻辑说明

1. 查询 `SessionRepository::list`（SQLite），按 owner principal 过滤
2. 可选 `cwd` 过滤，只返回该工作目录的 session
3. `meta.review` 来自 anureo background review（curator）结果
4. **fetch failure 不等于空列表**——查询失败时不能返回空 `sessions`，应返回 error

### Rust 类型

```rust
async fn list_sessions(&self, args: ListSessionsRequest)
    -> agent_client_protocol::Result<ListSessionsResponse>

async fn list_sessions_for_owner(
    &self, args: ListSessionsRequest, owner_principal: &str
) -> agent_client_protocol::Result<ListSessionsResponse>

pub struct SessionInfo {
    pub session_id: String,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub updated_at: Option<String>,
    pub meta: Option<SessionMeta>,
}
```

### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | 数据库查询失败 |

---

## 4. `session/fork`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agentCapabilities.sessionCapabilities.fork`（需要 `unstable_session_fork` feature） |
| anureo 状态 | ⚠️ Handler 已实现，capability 未声明 |

> **已知问题**: `fork_session()` 已实现并在 `stdio_loop.rs:236` 注册，但 `initialize` 响应的 `sessionCapabilities` 未包含 `fork`（`agent.rs:436-440`）。标准客户端基于 capability snapshot 不会调用 fork。修复方式：在 `SessionCapabilities::new()` 链中添加 `.fork(SessionForkCapabilities::new())`。

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "session/fork",
  "params": {
    "sessionId": "thread-abc123",
    "forkMode": "branch"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sessionId` | string | 是 | 源 session ID |
| `forkMode` | string | 否 | fork 模式 |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "result": {
    "sessionId": "thread-def456"
  }
}
```

### 逻辑说明

1. Fork 复制 session configuration（agent、model、effort、working directory、MCP servers）
2. 生成新 session ID（新 thread_id）
3. **不复制完整 conversation history**——新 session 从空白状态开始
4. 新 session 属于同一 owner principal

### Rust 类型

```rust
async fn fork_session(&self, args: ForkSessionRequest)
    -> agent_client_protocol::Result<ForkSessionResponse>
```

### Error

| Error code | 触发条件 |
|---|---|
| `session_not_found` | 源 session 不存在 |

---

## 5. `session/resume`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agentCapabilities.sessionCapabilities.resume` |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "method": "session/resume",
  "params": {
    "sessionId": "thread-abc123"
  }
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 7,
  "result": {
    "sessionId": "thread-abc123"
  }
}
```

### 逻辑说明

1. Resume 将持久化 session 重新绑定到当前 connection
2. **已有 active prompt 时必须拒绝**
3. 不 replay 历史消息（区别于 `session/load`）
4. 恢复 session 配置和 MCP runtime

### Rust 类型

```rust
async fn resume_session_for_owner(
    &self, args: ResumeSessionRequest, owner_principal: &str
) -> agent_client_protocol::Result<ResumeSessionResponse>
```

### Error

| Error code | 触发条件 |
|---|---|
| `session_not_found` | session 不存在 |
| `busy (-32010)` | session 有 active prompt 或 restore in progress |
| `forbidden` | session 属于其他 principal |

---

## 6. `session/close`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agentCapabilities.sessionCapabilities.close` |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "method": "session/close",
  "params": {
    "sessionId": "thread-abc123"
  }
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 8,
  "result": {}
}
```

### 逻辑说明

1. 关闭 session 的活动资源和 binding
2. **不等同于删除历史**——持久化数据保留
3. 释放 notification sink
4. 如有 active generation，根据 `DisconnectPolicy` 决定继续或取消

### Rust 类型

```rust
async fn close_session_for_owner(
    &self, args: CloseSessionRequest, owner_principal: &str
) -> agent_client_protocol::Result<CloseSessionResponse>
```

### Error

| Error code | 触发条件 |
|---|---|
| `session_not_found` | session 不存在 |

---

## 7. `session/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `agentCapabilities.sessionCapabilities.delete` |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 9,
  "method": "session/delete",
  "params": {
    "sessionId": "thread-abc123"
  }
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 9,
  "result": {}
}
```

### 逻辑说明

1. 删除 session 的**持久化数据和 binding**
2. 从 checkpoint store 删除该 thread 的所有 checkpoint
3. 从 `SessionConfigStore` 删除配置
4. 从 `SessionRepository` 删除元数据
5. 如有 active generation，先取消

### Rust 类型

```rust
async fn delete_session_for_owner(
    &self, args: DeleteSessionRequest, owner_principal: &str
) -> agent_client_protocol::Result<DeleteSessionResponse>
```

### Error

| Error code | 触发条件 |
|---|---|
| `session_not_found` | session 不存在 |
| `forbidden` | session 属于其他 principal |

---

## Binding 规则汇总

| 操作 | binding 变化 | 历史处理 |
|---|---|---|
| `session/new` | 创建 → 绑定当前 connection | 无历史 |
| `session/load` | 验证 → 重新绑定 → replay | 通过 update 推送历史 |
| `session/resume` | 验证 → 重新绑定 | 不 replay |
| `session/close` | 释放资源 → 解绑 | 保留持久化数据 |
| `session/delete` | 授权 → 删除数据 → 解绑 | 彻底删除 |

binding 失败必须回滚旧 binding。不得出现 session 已从 UI 移除但 server 仍持有 active generation 的状态。

---

## 断开行为

| DisconnectPolicy | 行为 |
|---|---|
| `Persist`（默认） | 保留 session，如设置了 `idle_ttl_secs` 则超时后取消 |
| `Cancel` | 立即取消所有绑定 session 的当前 generation |

配置: 环境变量 `ANUREO_ACP_DISCONNECT_POLICY=cancel|persist`

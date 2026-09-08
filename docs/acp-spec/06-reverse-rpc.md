# Reverse-RPC（反向请求）

> **命名空间**: 标准 ACP v1
> **方向**: Agent → Client request（与正向 request 相反）
> **实现状态**: ✅ 已实现
> **源码**: `apps/acp/src/client_methods.rs`、`apps/acp/src/tools/client_bridge.rs`、`apps/acp/src/client_capabilities.rs`

---

## 概述

Reverse-RPC 是 Agent 向 Client 发起的请求。在 anureo 的 prompt 执行过程中，Agent 可能需要：
- 请求用户授权（permission）
- 读写文件（filesystem）
- 执行终端命令（terminal）

这些操作由 Client（Anureo）执行，Agent 通过 JSON-RPC request 等待结果。

### 能力检查

Agent 在发起 reverse-RPC 前必须检查 Client 声明的能力：

```rust
// client_capabilities.rs
pub struct ClientCapabilitiesInfo {
    // 从 initialize 的 clientCapabilities 解析
}

impl ClientCapabilitiesInfo {
    fn supports_terminal(&self) -> bool;
    fn can_read_text_file(&self) -> bool;
    fn can_write_text_file(&self) -> bool;
    fn supports_mcp_http(&self) -> bool;
    fn supports_prompt_image(&self) -> bool;
}
```

---

## 1. `session/request_permission`

| 项目 | 内容 |
|---|---|
| 方向 | Agent → Client request |
| 触发 | Agent 工具执行需要用户授权 |
| 能力 | 无（总是可用） |

### Request（Agent → Client）

```json
{
  "jsonrpc": "2.0",
  "id": 100,
  "method": "session/request_permission",
  "params": {
    "sessionId": "thread-abc123",
    "permission": {
      "toolName": "bash",
      "actions": [
        {
          "path": "/home/user/project",
          "access": "execute"
        }
      ]
    }
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `sessionId` | string | 当前 session |
| `permission.toolName` | string | 需要授权的工具名称 |
| `permission.actions[].path` | string | 涉及的路径 |
| `permission.actions[].access` | string | 访问类型（`read`/`write`/`execute`） |

### Response（Client → Agent）

```json
{
  "jsonrpc": "2.0",
  "id": 100,
  "result": {
    "behavior": "allow_always",
    "updatedAbility": {
      "toolName": "bash",
      "actions": [
        { "path": "/home/user/project", "access": "execute" }
      ]
    }
  }
}
```

| `behavior` | 说明 |
|---|---|
| `allow_once` | 允许本次 |
| `allow_always` | 允许且记住（后续不再询问同一操作） |
| `deny_once` | 拒绝本次 |
| `deny_always` | 拒绝且记住 |

### Permission 行为

| Client 选择 | Agent 行为 |
|---|---|
| `allow_once` / `allow_always` | 继续执行工具 |
| `deny_once` / `deny_always` | 工具执行失败，返回 permission denied |

**关键规则**:
- permission denied ≠ success
- permission cancel → Agent 的 prompt 最终返回 `cancelled`
- Client 断开后，pending permission 不能自动变成 approved
- permission timeout → Agent 视为 denied 或 cancelled

### Rust 类型

```rust
// tools/client_bridge.rs
async fn session_request_permission(
    &self,
    request: SessionRequestPermissionParams
) -> Result<SessionRequestPermissionResult>

// SessionRequestPermissionParams
pub struct SessionRequestPermissionParams {
    pub session_id: String,
    pub permission: PermissionRequest,
}

pub struct PermissionRequest {
    pub tool_name: String,
    pub actions: Vec<PermissionAction>,
}

pub struct PermissionAction {
    pub path: Option<String>,
    pub access: String,
}

pub struct SessionRequestPermissionResult {
    pub behavior: String,
    pub updated_ability: Option<UpdatedAbility>,
}
```

---

## 2. `fs/read_text_file`

| 项目 | 内容 |
|---|---|
| 方向 | Agent → Client request |
| 能力 | `clientCapabilities.fs.readTextFile` |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 101,
  "method": "fs/read_text_file",
  "params": {
    "path": "/home/user/project/src/auth.rs"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `path` | string | 文件绝对路径 |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 101,
  "result": {
    "content": "fn auth() {\n    // ...\n}\n"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `content` | string | 文件文本内容 |

### 逻辑说明

1. Client 读取指定路径的文本文件
2. 路径必须在 workspace/worktree 范围内
3. 二进制文件返回 `invalid_params`
4. 文件不存在返回 `file_not_found`

### Rust 类型

```rust
async fn fs_read_text_file(
    &self,
    params: FsReadTextFileParams
) -> Result<FsReadTextFileResult>

pub struct FsReadTextFileParams {
    pub path: String,
}

pub struct FsReadTextFileResult {
    pub content: String,
}
```

---

## 3. `fs/write_text_file`

| 项目 | 内容 |
|---|---|
| 方向 | Agent → Client request |
| 能力 | `clientCapabilities.fs.writeTextFile` |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 102,
  "method": "fs/write_text_file",
  "params": {
    "path": "/home/user/project/src/auth.rs",
    "content": "fn auth() {\n    // fixed\n}\n"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `path` | string | 文件绝对路径 |
| `content` | string | 要写入的内容 |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 102,
  "result": {}
}
```

### 逻辑说明

1. Client 写入指定路径
2. 路径必须在 workspace/worktree 范围内
3. 受 workspace/path policy 约束
4. 写操作通常需要先经过 permission 流程

### Rust 类型

```rust
async fn fs_write_text_file(
    &self,
    params: FsWriteTextFileParams
) -> Result<FsWriteTextFileResult>

pub struct FsWriteTextFileParams {
    pub path: String,
    pub content: String,
}
```

---

## 4. `terminal/create`（`terminal_spawn`）

| 项目 | 内容 |
|---|---|
| 方向 | Agent → Client request |
| 能力 | `clientCapabilities.terminal` |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 103,
  "method": "terminal/create",
  "params": {
    "terminalId": "term-001"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `terminalId` | string | Agent 分配的终端 ID |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 103,
  "result": {
    "terminalId": "term-001",
    "isSuccess": true
  }
}
```

### Rust 类型

```rust
async fn terminal_spawn(
    &self,
    params: TerminalSpawnParams
) -> Result<TerminalSpawnResult>

pub struct TerminalSpawnParams {
    pub terminal_id: String,
}

pub struct TerminalSpawnResult {
    pub terminal_id: String,
    pub is_success: bool,
}
```

---

## 5. `terminal/resize`

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 104,
  "method": "terminal/resize",
  "params": {
    "terminalId": "term-001",
    "rows": 24,
    "cols": 80
  }
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 104,
  "result": {
    "terminalId": "term-001"
  }
}
```

### Rust 类型

```rust
async fn terminal_resize(
    &self,
    params: TerminalResizeParams
) -> Result<TerminalResizeResult>

pub struct TerminalResizeParams {
    pub terminal_id: String,
    pub rows: u32,
    pub cols: u32,
}
```

---

## 6. `terminal/close`

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 105,
  "method": "terminal/close",
  "params": {
    "terminalId": "term-001"
  }
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 105,
  "result": {
    "terminalId": "term-001"
  }
}
```

### Rust 类型

```rust
async fn terminal_close(
    &self,
    params: TerminalCloseParams
) -> Result<TerminalCloseResult>

pub struct TerminalCloseParams {
    pub terminal_id: String,
}
```

---

## ClientBridgeTrait 完整接口

```rust
// tools/client_bridge.rs
#[async_trait]
pub trait ClientBridgeTrait: Send + Sync {
    async fn session_request_permission(
        &self,
        request: SessionRequestPermissionParams
    ) -> Result<SessionRequestPermissionResult>;

    async fn fs_read_text_file(
        &self,
        params: FsReadTextFileParams
    ) -> Result<FsReadTextFileResult>;

    async fn fs_write_text_file(
        &self,
        params: FsWriteTextFileParams
    ) -> Result<FsWriteTextFileResult>;

    async fn terminal_spawn(
        &self,
        params: TerminalSpawnParams
    ) -> Result<TerminalSpawnResult>;

    async fn terminal_resize(
        &self,
        params: TerminalResizeParams
    ) -> Result<TerminalResizeResult>;

    async fn terminal_close(
        &self,
        params: TerminalCloseParams
    ) -> Result<TerminalCloseResult>;
}
```

`prompt_with_capabilities()` 接收 `Arc<dyn ClientBridgeTrait>` 作为参数，在 generation 过程中使用它发起 reverse-RPC。

---

## 安全规则

| 规则 | 说明 |
|---|---|
| 路径范围 | 所有 `fs/*` 和 terminal 操作必须在 workspace/worktree 范围内 |
| Session 绑定 | 每个 reverse-RPC 带 sessionId，校验 session 仍绑定当前 connection |
| Capability gate | Client 未声明能力时不发起对应 reverse-RPC |
| Timeout | 每个 reverse-RPC 有超时限制 |
| Error 安全 | error message 不包含 token/secret/敏感路径 |
| Permission isolation | pending permission 在 connection 断开时不能自动 approved |

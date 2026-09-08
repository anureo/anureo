# Files

> 命名空间: `_anureo.dev/files/*`
> Capability key: `files`

## 设计原则

- **标准 `fs/*` 负责 Agent 的文件读写**：ACP 标准 `fs/read_text_file` 和 `fs/write_text_file` 是 Agent → Client reverse-RPC，用于 Agent 运行时的文件操作。
- **`_anureo.dev/files/*` 负责 Anureo 文件浏览器**：以下扩展方法为 UI 文件浏览器提供目录列表、搜索、metadata 等只读功能，以及有限的写操作。
- **`_anureo.dev/files/read` 是明确禁止的 method**：Web UI 的 server-side 文本读取使用 `_anureo.dev/files/read_text_file`；标准 `fs/read_text_file` 仍只用于 Agent → Client reverse-RPC。
- **目录/Worktree 边界强制执行**：所有操作必须限定在当前 directory/worktree 范围内。Server 从 authoritative runtime/worktree state 解析最终路径，不接受 client 传入的任意绝对路径。

## Capability

```json
{
  "files": {
    "list": true,
    "search": true,
    "stat": true,
    "create_directory": true,
    "read_file_binary": true,
    "read_text_file": true,
    "write_file": true,
    "delete": true,
    "rename": true,
    "reveal_path": true,
    "exec_commands": true,
    "download_file": true
  }
}
```

## Rust 类型

```rust
pub struct FileEntry {
    pub name: String,
    pub path: String,
    pub is_directory: bool,
    pub size: u64,
    pub modified: String,
    pub created: Option<String>,
    pub permissions: Option<String>,
    pub is_symlink: bool,
    pub symlink_target: Option<String>,
    pub is_hidden: bool,
}

pub struct FileListParams {
    /// 相对于 worktree root 的目录路径
    pub path: Option<String>,
    /// 是否递归列出
    pub recursive: Option<bool>,
    /// 是否显示隐藏文件
    pub show_hidden: Option<bool>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

pub struct FileSearchParams {
    pub query: String,
    /// 搜索范围限定目录
    pub path: Option<String>,
    /// 文件名匹配模式 (glob)
    pub pattern: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

pub struct FileStatParams {
    pub path: String,
}

pub struct WriteFileParams {
    pub path: String,
    pub content: String,
    /// 是否创建父目录
    pub create_parent: Option<bool>,
}

pub struct DeleteFileParams {
    pub path: String,
    /// 非空目录是否递归删除
    pub recursive: Option<bool>,
}

pub struct RenameFileParams {
    pub old_path: String,
    pub new_path: String,
}

pub struct ExecCommandsParams {
    /// 工作目录
    pub cwd: String,
    pub commands: Vec<String>,
    /// 超时（毫秒）
    pub timeout_ms: Option<u64>,
}

pub struct ExecResult {
    pub command: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

pub struct ReadBinaryParams {
    pub path: String,
    /// 返回的 MIME 类型限制
    pub mime_type: Option<String>,
}
```

## Methods

---

### `_anureo.dev/files/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.list` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "path": "src",
  "recursive": false,
  "showHidden": false,
  "cursor": null,
  "limit": 200
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `path` | string? | 相对于 worktree root 的目录路径；省略时为 worktree root |
| `recursive` | boolean? | 是否递归列出子目录 |
| `showHidden` | boolean? | 是否显示隐藏文件（以 `.` 开头） |
| `cursor` | string? | 分页游标 |
| `limit` | number? | 每页数量上限 |

**Response:**

```json
{
  "items": [
    {
      "name": "main.rs",
      "path": "src/main.rs",
      "isDirectory": false,
      "size": 4096,
      "modified": "2025-08-19T10:00:00Z",
      "created": "2025-08-15T08:00:00Z",
      "permissions": "rw-r--r--",
      "isSymlink": false,
      "symlinkTarget": null,
      "isHidden": false
    },
    {
      "name": "utils",
      "path": "src/utils",
      "isDirectory": true,
      "size": 0,
      "modified": "2025-08-18T12:00:00Z",
      "created": "2025-08-15T08:00:00Z",
      "permissions": "rwxr-xr-x",
      "isSymlink": false,
      "symlinkTarget": null,
      "isHidden": false
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- Server 从 worktree root 解析 `path`，拼接为绝对路径后读取目录。
- 路径校验：`path` 不允许包含 `..`（防穿越）。Server 校验最终路径在 worktree root 内。
- symlink 解析：如果 symlink 指向 worktree 外部，返回 `isSymlink: true` 但不跟随。
- 分页：大量文件的目录使用 cursor 分页。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 目录不存在 |
| `invalid_params` | 路径在 worktree 外或包含 `..` |
| `internal_error` | 文件系统读取失败 |

---

### `_anureo.dev/files/search`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.search` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "query": "main",
  "path": "src",
  "pattern": "*.rs",
  "cursor": null,
  "limit": 50
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `query` | string | 搜索关键词（文件名模糊匹配） |
| `path` | string? | 搜索范围限定目录；省略时搜索整个 worktree |
| `pattern` | string? | glob 匹配模式（如 `*.rs`） |

**Response:**

```json
{
  "items": [
    {
      "name": "main.rs",
      "path": "src/main.rs",
      "isDirectory": false,
      "size": 4096,
      "modified": "2025-08-19T10:00:00Z",
      "isHidden": false
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- 搜索限定在 worktree 范围内。
- 支持 `query`（文件名包含匹配）和 `pattern`（glob 匹配）组合使用。
- 隐藏文件（`.` 开头）默认不返回，除非文件名匹配 `query`。
- 使用 Rust 的 `ignore` crate 或 `ripgrep` 内核实现快速搜索，遵循 `.gitignore` 规则。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 搜索路径不存在 |
| `invalid_params` | 路径在 worktree 外 |
| `internal_error` | 搜索失败 |

---

### `_anureo.dev/files/stat`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.stat` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "path": "src/main.rs"
}
```

**Response:**

```json
{
  "name": "main.rs",
  "path": "src/main.rs",
  "isDirectory": false,
  "size": 4096,
  "modified": "2025-08-19T10:00:00Z",
  "created": "2025-08-15T08:00:00Z",
  "permissions": "rw-r--r--",
  "isSymlink": false,
  "symlinkTarget": null,
  "isHidden": false,
  "mimeType": "text/x-rust"
}
```

**逻辑说明:**
- 返回单个文件/目录的详细 metadata。
- `mimeType` 由 server 推断（基于扩展名或内容检测）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件/目录不存在 |
| `invalid_params` | 路径在 worktree 外 |

---

### `_anureo.dev/files/create_directory`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.create_directory` |
| 权限 | Server policy（scope: `files:write`） |

**Request:**

```json
{
  "path": "src/new_module",
  "createParent": true
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `path` | string | 相对于 worktree root 的目录路径 |
| `createParent` | boolean? | 是否创建中间目录（`mkdir -p`） |

**Response:**

```json
{
  "path": "src/new_module",
  "created": true
}
```

**逻辑说明:**
- Server 执行 `mkdir -p <worktree_root>/<path>`。
- 目录已存在且 `createParent = true` 时不报错（幂等）。
- 目录已存在且 `createParent = false` 时返回 `already_exists`。

**Error:**

| kind | 触发条件 |
|---|---|
| `already_exists` | 目录已存在且 `createParent = false` |
| `forbidden` | 无 `files:write` scope |
| `invalid_params` | 路径在 worktree 外或包含 `..` |
| `internal_error` | 创建失败（权限不足等） |

---

### `_anureo.dev/files/read_text_file`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.read_text_file` |
| 权限 | Server policy（只读） |

**Request:**

```json
{ "path": "config/settings.json", "optional": false }
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `path` | string | 相对当前 working directory 的路径 |
| `optional` | boolean? | 为 `true` 时，不存在返回 `found=false`，不产生 error frame |

**Response:**

```json
{
  "path": "config/settings.json",
  "content": "{\"enabled\":true}",
  "encoding": "utf-8",
  "size": 17,
  "found": true
}
```

可选文件不存在时：

```json
{
  "path": "config/settings.json",
  "content": "",
  "encoding": "utf-8",
  "size": 0,
  "found": false
}
```

- `path` 相对当前 session working directory，禁止绝对路径和路径穿越。
- 默认最大 1 MiB；目录、非 UTF-8 文件和越界返回明确错误。不存在文件仅在 `optional != true` 时返回 `not_found`。
- 该方法用于 Web UI 读取 server-side 文本；Agent 文件操作仍使用标准 reverse-RPC `fs/read_text_file`。

---

### `_anureo.dev/files/read_file_binary`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.read_file_binary` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "path": "assets/logo.png",
  "mimeType": null
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `path` | string | 文件路径 |
| `mimeType` | string? | 期望返回的 MIME 类型（可选，用于验证） |

**Response:**

```json
{
  "path": "assets/logo.png",
  "dataUrl": "data:image/png;base64,iVBORw0KGgo...",
  "mimeType": "image/png",
  "size": 2048
}
```

**逻辑说明:**
- 用于读取二进制文件（图片、PDF、压缩包等），返回 `dataUrl` 格式。
- 文本文件读取**不应**使用此方法；使用标准 `fs/read_text_file`。
- 如果文件超过大小限制（server 配置，默认 10MB），返回 `invalid_params`。
- `mimeType` 参数不匹配时返回 `invalid_params`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件不存在 |
| `invalid_params` | 文件过大或 MIME 类型不匹配 |
| `forbidden` | 文件是二进制可执行文件且 server policy 禁止读取 |

---

### `_anureo.dev/files/write_file`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.write_file` |
| 权限 | Server policy（scope: `files:write`） |

**Request:**

```json
{
  "path": "config/settings.json",
  "content": "{\n  \"theme\": \"dark\"\n}",
  "createParent": true
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `path` | string | 文件路径 |
| `content` | string | 文件内容（UTF-8 文本） |
| `createParent` | boolean? | 是否创建父目录 |

**Response:**

```json
{
  "path": "config/settings.json",
  "written": true,
  "size": 26
}
```

**逻辑说明:**
- 写入文件内容（覆盖已有内容）。
- 如果父目录不存在且 `createParent = false`，返回 `not_found`。
- 此方法**不是** Agent 运行时的文件写入入口（那是标准 `fs/write_text_file` reverse-RPC）。此方法为 UI 文件编辑器提供写入能力。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 父目录不存在且 `createParent = false` |
| `forbidden` | 无 `files:write` scope |
| `invalid_params` | 路径在 worktree 外 |
| `internal_error` | 写入失败（磁盘空间不足等） |

---

### `_anureo.dev/files/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.delete` |
| 权限 | Server policy（scope: `files:write`） |

**Request:**

```json
{
  "path": "old_file.txt",
  "recursive": false
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `path` | string | 文件或空目录路径 |
| `recursive` | boolean? | `false`（默认）= 只删除空目录和文件；`true` = 递归删除非空目录 |

**Response:**

```json
{
  "path": "old_file.txt",
  "deleted": true
}
```

**逻辑说明:**
- `recursive = false` 时，删除非空目录返回 `invalid_params`。
- `recursive = true` 时，递归删除目录及其内容。
- 删除 worktree root 本身返回 `forbidden`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件/目录不存在 |
| `forbidden` | 无 `files:write` scope 或尝试删除 worktree root |
| `invalid_params` | 删除非空目录但 `recursive = false` |
| `internal_error` | 删除失败 |

---

### `_anureo.dev/files/rename`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.rename` |
| 权限 | Server policy（scope: `files:write`） |

**Request:**

```json
{
  "oldPath": "src/old_name.rs",
  "newPath": "src/new_name.rs"
}
```

**Response:**

```json
{
  "oldPath": "src/old_name.rs",
  "newPath": "src/new_name.rs",
  "renamed": true
}
```

**逻辑说明:**
- 重命名或移动文件/目录。
- `oldPath` 和 `newPath` 都必须在 worktree 范围内。
- 如果 `newPath` 已存在，返回 `already_exists`（不覆盖）。
- 跨设备移动（如 worktree 在不同挂载点）由 server 处理。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | oldPath 不存在 |
| `already_exists` | newPath 已存在 |
| `forbidden` | 无 `files:write` scope 或路径在 worktree 外 |
| `internal_error` | 重命名失败 |

---

### `_anureo.dev/files/reveal_path`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.reveal_path` |
| 权限 | Server policy |

**Request:**

```json
{
  "path": "src/main.rs"
}
```

**Response:**

```json
{
  "path": "src/main.rs",
  "revealed": true
}
```

**逻辑说明:**
- 请求 client 在系统文件管理器中显示指定路径。
- macOS: Finder `open -R`，Windows: Explorer `explorer /select`，Linux: `xdg-open`。
- 实际 UI 行为由 client 实现，server 只提供触发信号。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件/目录不存在 |
| `invalid_params` | 路径在 worktree 外 |

---

### `_anureo.dev/files/exec_commands`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.exec_commands` |
| 权限 | Server policy（scope: `files:exec`） |

**Request:**

```json
{
  "cwd": "src",
  "commands": [
    "cargo build --release",
    "cargo test -- --test-threads=4"
  ],
  "timeoutMs": 120000
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `cwd` | string | 工作目录（相对 worktree root） |
| `commands` | string[] | 按顺序执行的命令列表 |
| `timeoutMs` | number? | 总超时（毫秒），默认 120000 |

**Response:**

```json
{
  "results": [
    {
      "command": "cargo build --release",
      "exitCode": 0,
      "stdout": "   Compiling myapp v0.1.0\n    Finished release [optimized]",
      "stderr": "",
      "durationMs": 45000
    },
    {
      "command": "cargo test -- --test-threads=4",
      "exitCode": 0,
      "stdout": "running 10 tests\ntest result: ok. 10 passed",
      "stderr": "",
      "durationMs": 30000
    }
  ]
}
```

**逻辑说明:**
- 在指定目录执行命令（非 PTY 模式，非交互式）。
- 命令按顺序执行；前一个命令失败（exitCode != 0）时后续命令不执行。
- **不是终端**：此方法为非交互式批量执行，不支持 stdin、信号处理或实时输出。交互式终端使用 ACP 标准 `terminal/*`。
- 工作目录 `cwd` 必须在 worktree 范围内。
- 命令白名单/黑名单由 server policy 决定。

**Error:**

| kind | 触发条件 |
|---|---|
| `forbidden` | 无 `files:exec` scope 或命令被 server policy 拒绝 |
| `invalid_params` | `cwd` 在 worktree 外 |
| `internal_error` | 命令执行失败（命令不存在等） |

---

### `_anureo.dev/files/download_file`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `files.download_file` |
| 权限 | Server policy |

**Request:**

```json
{
  "path": "reports/output.pdf"
}
```

**Response:**

```json
{
  "path": "reports/output.pdf",
  "downloadUrl": "blob:anureo/download/abc123",
  "mimeType": "application/pdf",
  "size": 1048576,
  "expiresAt": "2025-08-19T11:00:00Z"
}
```

**逻辑说明:**
- 触发浏览器/桌面客户端下载指定文件。
- `downloadUrl` 是一次性临时 URL，过期后不可用。
- 文件大小超过 server 限制时返回 `invalid_params`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件不存在 |
| `invalid_params` | 文件过大 |
| `internal_error` | 生成下载 URL 失败 |

---

## Notifications

### `_anureo.dev/files/changed`

当文件系统发生变化（外部编辑、git 操作、server 内部写入）时推送。

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/files/changed",
  "params": {
    "change": "modified",
    "path": "src/main.rs"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|
| `change` | string | `created` / `deleted` / `modified` / `renamed` |
| `path` | string | 变化的文件路径（相对于 worktree root） |

- notification 可能批量推送（如 git checkout 导致多个文件变化）。
- Client 收到后应调用 `files/list`（当前目录）获取最新快照。

## 禁止的方法

| Method | 状态 | 替代 |
|---|---|---|
| `_anureo.dev/files/read` | **FORBIDDEN** | `_anureo.dev/files/read_text_file` |

Server 对禁止的 method 返回 `method_not_found`。文件文本读取必须通过标准 ACP `fs/read_text_file`（Agent → Client reverse-RPC）。

## 目录/Worktree 边界强制执行

1. **Worktree root 权威性**：所有路径操作以 worktree root 为基准。Server 从当前 session 的 `workingDirectory` 或 git worktree 状态解析 root。
2. **路径穿越拒绝**：`path` 参数中的 `..` 段被 server 规范化后检查是否仍在 worktree root 内。
3. **Symlink 限制**：symlink 指向 worktree 外部的文件不跟随、不泄露目标路径的完整绝对路径。
4. **绝对路径拒绝**：client 传入的路径如果是绝对路径（非相对路径），server 拒绝并返回 `invalid_params`。
5. **跨 worktree 操作禁止**：所有操作只能影响当前 session 绑定的 worktree。

## Reconnect Resync

| Notification | Authoritative method |
|---|---|
| `_anureo.dev/files/changed` | `_anureo.dev/files/list`（当前目录） |

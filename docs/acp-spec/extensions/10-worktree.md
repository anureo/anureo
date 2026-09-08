# Worktree

> 命名空间: `_anureo.dev/worktree/*`
> Capability key: `worktree`

## Capability

```json
{
  "worktree": {
    "list": true,
    "get": true,
    "validate": true,
    "preview": true,
    "bootstrap_status": true,
    "create": true,
    "delete": true
  }
}
```

## Rust 类型

```rust
/// Worktree 条目
pub struct WorktreeInfo {
    /// worktree 绝对路径（server 解析，非 client 传入）
    pub path: String,
    /// HEAD commit SHA
    pub head: String,
    /// 当前 checkout 的分支名；detached HEAD 时为 null
    pub branch: Option<String>,
    /// 是否为主 worktree（main worktree）
    pub is_main: bool,
    /// 是否处于 detached HEAD 状态
    pub is_detached: bool,
    /// 是否有未提交更改
    pub is_dirty: bool,
    /// 前置操作标识（如 bootstrap 脚本是否执行完毕）
    pub attention_reason: Option<String>,
}

pub struct WorktreeListParams {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

pub struct WorktreeListResponse {
    pub items: Vec<WorktreeInfo>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

pub struct WorktreeCreateParams {
    /// 目标分支名
    pub branch: String,
    /// 基准 commit/branch/tag
    pub base_ref: Option<String>,
    /// 是否在创建后执行 bootstrap 脚本
    pub run_bootstrap: bool,
    /// 幂等键（防止重复创建）
    pub idempotency_key: Option<String>,
}

pub struct WorktreeDeleteParams {
    pub path: String,
    /// 是否强制删除（含未提交更改）
    pub force: bool,
}

pub struct WorktreeValidateParams {
    pub branch: String,
    pub base_ref: Option<String>,
}

pub struct WorktreeBootstrapStatusParams {
    pub path: String,
}
```

## Methods

---

### `_anureo.dev/worktree/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `worktree.list` |
| 权限 | Server policy（只读，一般允许） |

**Request:**

```json
{
  "cursor": null,
  "limit": 50
}
```

**Response:**

```json
{
  "items": [
    {
      "path": "/home/user/project",
      "head": "a1b2c3d",
      "branch": "main",
      "isMain": true,
      "isDetached": false,
      "isDirty": false,
      "attentionReason": null
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- Server 通过 `git worktree list --porcelain` 获取所有 linked worktree。
- `path` 字段由 server 从 git 内部状态解析，不接受 client 传入路径。
- `isDirty` 通过 `git status --porcelain` 判定。
- 支持标准 cursor 分页（见 `08-cross-cutting-patterns.md` §1）。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | git 命令执行失败 |
| `not_found` | 当前目录不在 git 仓库内 |

---

### `_anureo.dev/worktree/get`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `worktree.get` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "path": "/home/user/project/.worktrees/feature-x"
}
```

**Response:**

```json
{
  "path": "/home/user/project/.worktrees/feature-x",
  "head": "a1b2c3d",
  "branch": "feature-x",
  "isMain": false,
  "isDetached": false,
  "isDirty": true,
  "attentionReason": "bootstrap_pending"
}
```

**逻辑说明:**
- Server 验证 `path` 在已知 worktree 列表中。
- 如果 worktree 不存在返回 `not_found`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 指定路径不是有效的 linked worktree |
| `invalid_params` | path 参数缺失 |

---

### `_anureo.dev/worktree/validate`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `worktree.validate` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "branch": "feature-y",
  "baseRef": "main"
}
```

**Response:**

```json
{
  "valid": false,
  "conflicts": [
    {
      "type": "branch_exists",
      "detail": "Branch 'feature-y' already exists and is checked out in another worktree"
    }
  ]
}
```

**逻辑说明:**
- 检查分支名是否已被其他 worktree checkout（git 不允许同一分支在多个 worktree 中 checkout）。
- 检查 `baseRef` 是否存在。
- 不修改任何状态。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | 分支名格式非法 |
| `not_found` | baseRef 不存在 |

---

### `_anureo.dev/worktree/preview`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `worktree.preview` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "branch": "feature-z",
  "baseRef": "main",
  "runBootstrap": true
}
```

**Response:**

```json
{
  "targetPath": "/home/user/project/.worktrees/feature-z",
  "baseCommit": "a1b2c3d",
  "bootstrapPlan": [
    { "step": 1, "command": "npm install", "estimatedMs": 30000 },
    { "step": 2, "command": "npm run build", "estimatedMs": 60000 }
  ],
  "warnings": []
}
```

**逻辑说明:**
- 预览 worktree 创建后的预期结果，不实际执行。
- `bootstrapPlan` 根据 `.anureo/bootstrap.json` 或项目配置生成。
- 如果分支冲突，返回 warning 但不报错（因为不实际创建）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | baseRef 不存在 |
| `internal_error` | bootstrap 配置解析失败 |

---

### `_anureo.dev/worktree/bootstrap_status`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `worktree.bootstrap_status` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "path": "/home/user/project/.worktrees/feature-z"
}
```

**Response:**

```json
{
  "path": "/home/user/project/.worktrees/feature-z",
  "status": "completed",
  "steps": [
    { "step": 1, "command": "npm install", "status": "success", "durationMs": 28500 },
    { "step": 2, "command": "npm run build", "status": "success", "durationMs": 55200 }
  ],
  "error": null
}
```

**逻辑说明:**
- `status` 可选值：`pending`、`running`、`completed`、`failed`。
- 如果 worktree 未配置 bootstrap，返回 `status: "completed"` 且 steps 为空。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | worktree 不存在 |

---

### `_anureo.dev/worktree/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `worktree.create` |
| 权限 | Server-side authorization（写操作，需要 scope: `worktree:write`） |

**Request:**

```json
{
  "branch": "feature-new",
  "baseRef": "main",
  "runBootstrap": true,
  "idempotencyKey": "wt-2025-08-19-feature-new"
}
```

**Response:**

```json
{
  "path": "/home/user/project/.worktrees/feature-new",
  "head": "a1b2c3d",
  "branch": "feature-new",
  "isMain": false,
  "isDetached": false,
  "isDirty": false,
  "attentionReason": null,
  "bootstrapStatus": {
    "status": "completed",
    "steps": [
      { "step": 1, "command": "npm install", "status": "success", "durationMs": 28500 }
    ]
  }
}
```

**逻辑说明:**
- Server 执行 `git worktree add <path> <branch>`。
- 目标路径由 server 根据 worktree root 策略决定，client 不能指定任意路径。
- 如果 `baseRef` 存在，先从该 ref 创建分支再 checkout。
- `runBootstrap = true` 时，创建完成后执行 bootstrap 脚本；脚本执行失败不回滚 worktree 创建，但 `bootstrapStatus.status = "failed"`。
- 幂等键：同一 `idempotencyKey` 在短时间内重复请求返回已有结果。
- 长时操作，支持 progress notification（见 `08-cross-cutting-patterns.md` §3）。

**Error:**

| kind | 触发条件 |
|---|---|
| `already_exists` | 分支已存在且已被其他 worktree checkout |
| `invalid_params` | 分支名非法或 baseRef 不存在 |
| `forbidden` | 当前连接无 `worktree:write` scope |
| `internal_error` | git worktree add 失败（磁盘空间不足等） |

---

### `_anureo.dev/worktree/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `worktree.delete` |
| 权限 | Server-side authorization + 建议显式 UI 确认（scope: `worktree:write`） |

**Request:**

```json
{
  "path": "/home/user/project/.worktrees/feature-old",
  "force": false
}
```

**Response:**

```json
{
  "path": "/home/user/project/.worktrees/feature-old",
  "deleted": true,
  "branchCleaned": true
}
```

**逻辑说明:**
- 删除操作不能由普通 prompt 隐式触发，必须由 client 显式调用。
- `force = false` 时，如果 worktree 有未提交更改，返回 `invalid_params`（拒绝删除）。
- `force = true` 时，强制删除 worktree 目录并执行 `git worktree prune`。
- 主 worktree（`isMain = true`）不可删除，返回 `forbidden`。
- `branchCleaned` 表示关联的分支是否也被删除（仅当分支是当前 worktree 创建的且未合并到 main 时）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | worktree 不存在 |
| `forbidden` | 尝试删除主 worktree，或无 `worktree:write` scope |
| `invalid_params` | `force = false` 且 worktree 有未提交更改 |
| `internal_error` | 文件系统删除失败 |

---

## Notifications

### `_anureo.dev/worktree/changed`

当 worktree 列表发生变化（创建、删除、分支切换、dirty 状态变化）时推送。

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/worktree/changed",
  "params": {
    "change": "created",
    "path": "/home/user/project/.worktrees/feature-new"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | string | `created` / `deleted` / `updated` |
| `path` | string | 变化的 worktree 路径 |

- notification 丢失后，client 必须调用 `worktree/list` 获取完整列表。
- notification 只推送变更提示，不包含完整 worktree 数据。

## 目录边界校验

所有 worktree 操作的路径校验遵循以下规则：

1. **Server-authoritative 路径**：client 传入的 `path` 参数仅作为查找线索，实际操作路径由 server 从 `git worktree list` 解析。
2. **不允许路径穿越**：`path` 中的 `..` 段被拒绝（`invalid_params`）。
3. **Symlink 拒绝**：如果 worktree 路径包含 symlink，server 拒绝操作。
4. **worktree root 限制**：创建 worktree 的目标目录必须在 server 配置的 worktree root 内（如 `.worktrees/`）。

## Reconnect Resync

| Notification | Authoritative method |
|---|---|
| `_anureo.dev/worktree/changed` | `_anureo.dev/worktree/list` |

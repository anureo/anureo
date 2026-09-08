# Git

> 命名空间: `_anureo.dev/git/*`
> Capability key: `git`

## 设计原则

- **结构化返回**：所有 Git 结果必须以结构化 JSON 返回。UI 不解析 terminal 文本输出。
- **写操作权限门控**：所有写操作（commit、push、pull、merge、rebase、reset、cherry-pick、revert）必须经过 server-side policy 检查和确认门槛。
- **SSH 密钥安全**：SSH private key 不出现在任何 response、日志或 session/update 中。
- **路径权威性**：所有路径操作不依赖 client 传入的路径，server 从 authoritative runtime/worktree state 解析。

## Capability

```json
{
  "git": {
    "check": true,
    "status": true,
    "diff": true,
    "file_diff": true,
    "log": true,
    "commit_files": true,
    "commit_file_diff": true,
    "stage_file": true,
    "stage_files": true,
    "unstage_file": true,
    "unstage_files": true,
    "stage_hunk": true,
    "unstage_hunk": true,
    "revert_file": true,
    "revert_hunk": true,
    "branches": true,
    "checkout_branch": true,
    "create_branch": true,
    "rename_branch": true,
    "delete_branch": true,
    "delete_remote_branch": true,
    "remotes": true,
    "remote_url": true,
    "remove_remote": true,
    "fetch": true,
    "commit": true,
    "generate_commit_message": true,
    "generate_pr_description": true,
    "push": true,
    "pull": true,
    "stash_list": true,
    "stash_create": true,
    "stash_pop": true,
    "stash_apply": true,
    "stash_drop": true,
    "stash_count": true,
    "merge": true,
    "merge_abort": true,
    "merge_continue": true,
    "rebase": true,
    "rebase_abort": true,
    "rebase_continue": true,
    "conflict_details": true,
    "checkout_commit": true,
    "cherry_pick": true,
    "revert_commit": true,
    "reset_to_commit": true,
    "validate_worktree_directory": true,
    "canonicalize_worktree_state": true,
    "is_linked_worktree": true,
    "identity_list": true,
    "identity_get": true,
    "identity_get_global": true,
    "identity_create": true,
    "identity_update": true,
    "identity_delete": true,
    "identity_set": true,
    "identity_discover_credentials": true
  }
}
```

## 轻量仓库探测

`_anureo.dev/git/check` 只判断当前 authoritative working directory 是否位于 Git repository 内，不枚举 changed files。项目列表、worktree discovery 等只需要布尔结果的调用方必须使用此方法，不得用完整 `status` 代替。

请求：

```json
{}
```

响应：

```json
{
  "isGitRepository": true
}
```

非 Git 目录返回成功结果 `{ "isGitRepository": false }`；Git executable、I/O 等实际执行错误仍返回协议错误。

## Rust 类型

```rust
// ── 状态和 Diff ──

pub struct GitStatus {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<GitStatusFile>,
    pub in_progress: Option<GitInProgress>,
}

pub struct GitStatusFile {
    pub path: String,
    pub index_status: GitFileStatus,
    pub working_status: GitFileStatus,
}

pub enum GitFileStatus {
    Unmodified,
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    Unmerged,
    Untracked,
    Ignored,
}

pub struct GitInProgress {
    pub operation: GitOperation,
    pub conflict_files: Vec<String>,
}

pub enum GitOperation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

pub struct GitDiffSummary {
    pub hunks: Vec<GitDiffHunk>,
    pub stat: GitDiffStat,
}

pub struct GitDiffHunk {
    pub old_path: String,
    pub new_path: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub header: String,
    pub lines: Vec<GitDiffLine>,
}

pub struct GitDiffLine {
    pub kind: GitDiffLineKind,
    pub content: String,
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
}

pub enum GitDiffLineKind {
    Context,
    Addition,
    Deletion,
    NoNewline,
}

pub struct GitDiffStat {
    pub files_changed: u32,
    pub insertions: u32,
    pub deletions: u32,
}

pub struct GitCommitInfo {
    pub sha: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_email: String,
    pub author_date: String,
    pub committer: String,
    pub committer_email: String,
    pub committer_date: String,
    pub message: String,
    pub refs: Vec<String>,
}

// ── 分支 ──

pub struct GitBranch {
    pub name: String,
    pub is_current: bool,
    pub is_remote: bool,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub last_commit_sha: String,
    pub last_commit_date: String,
}

// ── Remote ──

pub struct GitRemote {
    pub name: String,
    pub url: String,
    pub url_type: RemoteUrlType,
}

pub enum RemoteUrlType {
    Https,
    Ssh,
    File,
}

// ── Stash ──

pub struct GitStashEntry {
    pub index: u32,
    pub message: String,
    pub date: String,
    pub branch: String,
}

// ── Identity ──

pub struct GitIdentity {
    pub profile_id: String,
    pub name: String,
    pub email: String,
    pub scope: IdentityScope,
}

pub enum IdentityScope {
    Global,
    Repo,
    Worktree,
}

// ── 写操作权限 ──

pub enum GitWriteScope {
    /// stage/unstage/revert 等本地操作
    GitStage,
    /// commit 操作
    GitCommit,
    /// push/pull/fetch 等网络操作
    GitRemote,
    /// merge/rebase 等改变历史的操作
    GitHistory,
    /// reset --hard 等破坏性操作
    GitDestructive,
}
```

---

## 状态和 Diff

### `_anureo.dev/git/status`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.status` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "path": null
}
```

- `path`：可选，指定 worktree 路径；省略时使用当前 session 工作目录。

**Response:**

```json
{
  "branch": "main",
  "upstream": "origin/main",
  "ahead": 2,
  "behind": 0,
  "files": [
    {
      "path": "src/main.rs",
      "indexStatus": "modified",
      "workingStatus": "unmodified"
    },
    {
      "path": "README.md",
      "indexStatus": "unmodified",
      "workingStatus": "modified"
    },
    {
      "path": "new_file.txt",
      "indexStatus": "untracked",
      "workingStatus": "untracked"
    }
  ],
  "inProgress": null
}
```

**逻辑说明:**
- Server 执行 `git status --porcelain=v2 --branch` 并解析为结构化数据。
- `inProgress` 非 null 时表示仓库处于 merge/rebase/cherry-pick/revert 中间状态。
- `ahead`/`behind` 来自 branch 与 upstream 的比较。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 当前目录不是 git 仓库 |
| `internal_error` | git 命令执行失败 |

---

### `_anureo.dev/git/diff`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.diff` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "staged": true,
  "path": null,
  "unified": 3
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `staged` | boolean | `true` = staged diff（`--cached`），`false` = unstaged diff |
| `path` | string? | 限定文件路径（可选） |
| `unified` | number? | 上下文行数，默认 3 |

**Response:**

```json
{
  "hunks": [
    {
      "oldPath": "src/main.rs",
      "newPath": "src/main.rs",
      "oldStart": 10,
      "oldLines": 5,
      "newStart": 10,
      "newLines": 8,
      "header": "@@ -10,5 +10,8 @@",
      "lines": [
        { "kind": "context", "content": "fn main() {", "oldLine": 10, "newLine": 10 },
        { "kind": "addition", "content": "    println!(\"hello\");", "oldLine": null, "newLine": 11 },
        { "kind": "deletion", "content": "    println!(\"world\");", "oldLine": 11, "newLine": null }
      ]
    }
  ],
  "stat": { "filesChanged": 1, "insertions": 3, "deletions": 2 }
}
```

**逻辑说明:**
- Server 执行 `git diff [--cached] [--stat] --unified=<n> [<path>]` 并解析为结构化 hunks。
- 禁止返回 raw diff 文本；UI 不做 terminal 文本解析。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件路径不存在或不在仓库内 |
| `internal_error` | git diff 失败 |

---

### `_anureo.dev/git/file_diff`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.file_diff` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "filePath": "src/main.rs",
  "staged": false,
  "unified": 3
}
```

**Response:**

```json
{
  "filePath": "src/main.rs",
  "originalContent": "fn main() {\n    println!(\"world\");\n}\n",
  "modifiedContent": "fn main() {\n    println!(\"hello\");\n    println!(\"world\");\n}\n",
  "hunks": [
    {
      "oldStart": 1,
      "oldLines": 3,
      "newStart": 1,
      "newLines": 4,
      "lines": [
        { "kind": "context", "content": "fn main() {", "oldLine": 1, "newLine": 1 },
        { "kind": "addition", "content": "    println!(\"hello\");", "oldLine": null, "newLine": 2 },
        { "kind": "context", "content": "    println!(\"world\");", "oldLine": 2, "newLine": 3 }
      ]
    }
  ]
}
```

**逻辑说明:**
- 返回单文件的 original/modified 内容和结构化 diff。
- `originalContent` 来自 HEAD（staged=false）或 index（staged=true）。
- `modifiedContent` 来自工作区（staged=false）或 staged area（staged=true）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件不在仓库内 |
| `internal_error` | git diff 失败 |

---

### `_anureo.dev/git/log`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.log` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "limit": 30,
  "cursor": null,
  "branch": null,
  "filePath": null
}
```

**Response:**

```json
{
  "items": [
    {
      "sha": "a1b2c3d4e5f6",
      "parents": ["f6e5d4c3b2a1"],
      "author": "Alice",
      "authorEmail": "alice@example.com",
      "authorDate": "2025-08-19T10:00:00Z",
      "committer": "Alice",
      "committerEmail": "alice@example.com",
      "committerDate": "2025-08-19T10:00:00Z",
      "message": "feat: add new feature\n\nDetailed description.",
      "refs": ["HEAD", "main", "origin/main"]
    }
  ],
  "nextCursor": "a1b2c3d4e5f6",
  "hasMore": true
}
```

**逻辑说明:**
- Server 执行 `git log --format=<structured> --skip=<cursor-offset> -n=<limit>`。
- `cursor` 为 opaque 字符串，server 内部解码为 skip offset + commit SHA。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | branch 或 filePath 不存在 |
| `internal_error` | git log 失败 |

---

### `_anureo.dev/git/commit_files`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.commit_files` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "commitSha": "a1b2c3d4e5f6"
}
```

**Response:**

```json
{
  "commitSha": "a1b2c3d4e5f6",
  "files": [
    {
      "path": "src/main.rs",
      "status": "modified",
      "insertions": 5,
      "deletions": 2
    },
    {
      "path": "src/new.rs",
      "status": "added",
      "insertions": 30,
      "deletions": 0
    }
  ],
  "totalInsertions": 35,
  "totalDeletions": 2
}
```

**逻辑说明:**
- Server 执行 `git show --stat --name-status <sha>`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | commitSha 不存在 |

---

### `_anureo.dev/git/commit_file_diff`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.commit_file_diff` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "commitSha": "a1b2c3d4e5f6",
  "filePath": "src/main.rs",
  "unified": 3
}
```

**Response:**

```json
{
  "commitSha": "a1b2c3d4e5f6",
  "filePath": "src/main.rs",
  "hunks": [
    {
      "oldStart": 10,
      "oldLines": 5,
      "newStart": 10,
      "newLines": 8,
      "lines": [
        { "kind": "addition", "content": "    println!(\"hello\");", "oldLine": null, "newLine": 11 }
      ]
    }
  ],
  "stat": { "insertions": 3, "deletions": 2 }
}
```

**逻辑说明:**
- Server 执行 `git show <sha> -- <path>` 并解析 diff。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | commit 或文件路径不存在 |

---

## 暂存操作

### `_anureo.dev/git/stage_file`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.stage_file` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "filePath": "src/main.rs"
}
```

**Response:**

```json
{
  "filePath": "src/main.rs",
  "staged": true
}
```

**逻辑说明:**
- Server 执行 `git add <filePath>`。
- 路径校验：filePath 必须在当前 worktree 范围内。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件不存在 |
| `forbidden` | 无 `git:stage` scope |
| `invalid_params` | 路径在 worktree 外 |

---

### `_anureo.dev/git/stage_files`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.stage_files` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "filePaths": ["src/main.rs", "README.md"]
}
```

**Response:**

```json
{
  "staged": ["src/main.rs", "README.md"],
  "failed": []
}
```

**逻辑说明:**
- 批量执行 `git add`。
- 如果部分文件失败，返回 `failed` 列表和对应错误信息。整体不报错，使用 partial failure 语义。

**Error:**

| kind | 触发条件 |
|---|---|
| `forbidden` | 无 `git:stage` scope |
| `invalid_params` | 所有文件路径都无效 |

---

### `_anureo.dev/git/unstage_file`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.unstage_file` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "filePath": "src/main.rs"
}
```

**Response:**

```json
{
  "filePath": "src/main.rs",
  "unstaged": true
}
```

**逻辑说明:**
- Server 执行 `git restore --staged <filePath>`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件不在 index 中 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/unstage_files`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.unstage_files` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "filePaths": ["src/main.rs", "README.md"]
}
```

**Response:**

```json
{
  "unstaged": ["src/main.rs", "README.md"],
  "failed": []
}
```

**逻辑说明:**
- 批量 `git restore --staged`。Partial failure 语义同 `stage_files`。

**Error:**

| kind | 触发条件 |
|---|---|
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/stage_hunk`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.stage_hunk` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "filePath": "src/main.rs",
  "hunkHeader": "@@ -10,5 +10,8 @@",
  "patch": "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,5 +10,8 @@\n..."
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `filePath` | string | 目标文件 |
| `hunkHeader` | string | hunk 的 `@@` header，用于定位 |
| `patch` | string | 该 hunk 的完整 unified diff patch（由 client 从 `git/diff` 获取） |

**Response:**

```json
{
  "filePath": "src/main.rs",
  "staged": true
}
```

**逻辑说明:**
- Server 使用 `git apply --cached` 将指定 hunk patch 应用到 index。
- patch 格式必须与 `git diff` 输出一致。
- Server 验证 patch 对应的 hunk 与当前工作区状态匹配；如果文件已变更导致 patch 无法应用，返回 `invalid_params`。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | patch 格式错误或无法 apply |
| `not_found` | 文件不存在 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/unstage_hunk`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.unstage_hunk` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "filePath": "src/main.rs",
  "hunkHeader": "@@ -10,5 +10,8 @@",
  "patch": "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -10,5 +10,8 @@\n..."
}
```

**Response:**

```json
{
  "filePath": "src/main.rs",
  "unstaged": true
}
```

**逻辑说明:**
- Server 使用 `git apply --cached --reverse` 反向应用 hunk。
- 语义同 `stage_hunk`，但方向相反。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | patch 格式错误或无法 apply |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/revert_file`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.revert_file` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "filePath": "src/main.rs",
  "scope": "working"
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `filePath` | string | 目标文件 |
| `scope` | string | `working` = 恢复工作区到 index（`git checkout -- <file>`），`all` = 恢复 index 和工作区到 HEAD（`git restore --staged --worktree <file>`） |

**Response:**

```json
{
  "filePath": "src/main.rs",
  "reverted": true
}
```

**逻辑说明:**
- `scope = "working"` 执行 `git checkout -- <filePath>`。
- `scope = "all"` 执行 `git restore --staged --worktree <filePath>`，丢弃所有未提交更改。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 文件不存在 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/revert_hunk`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.revert_hunk` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "filePath": "src/main.rs",
  "hunkHeader": "@@ -10,5 +10,8 @@",
  "patch": "@@ -10,5 +10,8 @@\n..."
}
```

**Response:**

```json
{
  "filePath": "src/main.rs",
  "reverted": true
}
```

**逻辑说明:**
- Server 使用 `git apply --reverse` 撤销指定 hunk 的工作区更改。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | patch 无法 apply |
| `forbidden` | 无 `git:stage` scope |

---

## 分支操作

### `_anureo.dev/git/branches`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.branches` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "remote": true,
  "cursor": null,
  "limit": 50
}
```

**Response:**

```json
{
  "items": [
    {
      "name": "main",
      "isCurrent": true,
      "isRemote": false,
      "upstream": "origin/main",
      "ahead": 0,
      "behind": 0,
      "lastCommitSha": "a1b2c3d4e5f6",
      "lastCommitDate": "2025-08-19T10:00:00Z"
    },
    {
      "name": "origin/main",
      "isCurrent": false,
      "isRemote": true,
      "upstream": null,
      "ahead": 0,
      "behind": 0,
      "lastCommitSha": "a1b2c3d4e5f6",
      "lastCommitDate": "2025-08-19T10:00:00Z"
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- Server 执行 `git for-each-ref --format=<structured>` 解析分支列表。
- `remote = true` 时包含远程分支。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | git 命令失败 |

---

### `_anureo.dev/git/checkout_branch`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.checkout_branch` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "branch": "feature-x"
}
```

**Response:**

```json
{
  "branch": "feature-x",
  "previousBranch": "main",
  "checkedOut": true
}
```

**逻辑说明:**
- Server 执行 `git checkout <branch>`。
- 如果工作区有未提交更改且与目标分支冲突，返回 `invalid_params`。
- `previousBranch` 返回切换前的分支名，用于 UI 显示。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 分支不存在 |
| `invalid_params` | 工作区有未提交更改与目标冲突 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/create_branch`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.create_branch` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "branch": "feature-new",
  "baseRef": "main",
  "checkout": true
}
```

**Response:**

```json
{
  "branch": "feature-new",
  "baseCommit": "a1b2c3d4e5f6",
  "created": true
}
```

**逻辑说明:**
- `checkout = true` 时执行 `git checkout -b <branch> <baseRef>`。
- `checkout = false` 时执行 `git branch <branch> <baseRef>`。

**Error:**

| kind | 触发条件 |
|---|---|
| `already_exists` | 分支名已存在 |
| `not_found` | baseRef 不存在 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/rename_branch`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.rename_branch` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "oldName": "feature-old",
  "newName": "feature-renamed"
}
```

**Response:**

```json
{
  "oldName": "feature-old",
  "newName": "feature-renamed",
  "renamed": true
}
```

**逻辑说明:**
- Server 执行 `git branch -m <old> <new>`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 旧分支不存在 |
| `already_exists` | 新分支名已存在 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/delete_branch`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.delete_branch` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "branch": "feature-old",
  "force": false
}
```

**Response:**

```json
{
  "branch": "feature-old",
  "deleted": true
}
```

**逻辑说明:**
- `force = false` 时执行 `git branch -d`（安全删除，拒绝未合并的分支）。
- `force = true` 时执行 `git branch -D`（强制删除）。
- 当前 checkout 的分支不可删除，返回 `forbidden`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 分支不存在 |
| `forbidden` | 分支是当前分支，或无 `git:stage` scope |
| `invalid_params` | `force = false` 且分支未合并 |

---

### `_anureo.dev/git/delete_remote_branch`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.delete_remote_branch` |
| 权限 | Server policy（scope: `git:remote`） |

**Request:**

```json
{
  "remote": "origin",
  "branch": "feature-old"
}
```

**Response:**

```json
{
  "remote": "origin",
  "branch": "feature-old",
  "deleted": true
}
```

**逻辑说明:**
- Server 执行 `git push <remote> --delete <branch>`。
- 建议客户端显式确认（删除远程分支是破坏性操作）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | remote 或分支不存在 |
| `forbidden` | 无 `git:remote` scope，或远程拒绝删除 |
| `internal_error` | 网络故障 |

---

## Remote 操作

### `_anureo.dev/git/remotes`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.remotes` |
| 权限 | Server policy（只读） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "remotes": [
    {
      "name": "origin",
      "url": "git@github.com:user/repo.git",
      "urlType": "ssh"
    },
    {
      "name": "upstream",
      "url": "https://github.com/upstream/repo.git",
      "urlType": "https"
    }
  ]
}
```

**逻辑说明:**
- Server 执行 `git remote -v` 解析。
- URL 中的 token/credentials 被脱敏（如 HTTPS URL 中的密码不返回）。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | git 命令失败 |

---

### `_anureo.dev/git/remote_url`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.remote_url` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "remote": "origin"
}
```

**Response:**

```json
{
  "remote": "origin",
  "url": "git@github.com:user/repo.git",
  "urlType": "ssh"
}
```

**逻辑说明:**
- 获取指定 remote 的 URL（脱敏）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | remote 不存在 |

---

### `_anureo.dev/git/remove_remote`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.remove_remote` |
| 权限 | Server policy（scope: `git:remote`） |

**Request:**

```json
{
  "remote": "upstream"
}
```

**Response:**

```json
{
  "remote": "upstream",
  "removed": true
}
```

**逻辑说明:**
- Server 执行 `git remote remove <name>`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | remote 不存在 |
| `forbidden` | 无 `git:remote` scope |

---

### `_anureo.dev/git/fetch`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.fetch` |
| 权限 | Server policy（scope: `git:remote`） |

**Request:**

```json
{
  "remote": "origin",
  "branch": null,
  "prune": false
}
```

**Response:**

```json
{
  "remote": "origin",
  "updated": true,
  "fetchedRefs": [
    { "ref": "refs/heads/main", "oldSha": "a1b2c3d", "newSha": "d4e5f6g" }
  ]
}
```

**逻辑说明:**
- Server 执行 `git fetch [--prune] [<remote> [<branch>]]`。
- 长时操作，支持 progress notification。
- SSH/HTTPS 凭据由 server 管理的 credential helper 提供，不暴露给 client。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | remote 不存在 |
| `forbidden` | 无 `git:remote` scope |
| `internal_error` | 网络故障或认证失败 |

---

## Commit 操作

### `_anureo.dev/git/commit`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.commit` |
| 权限 | Server policy（scope: `git:commit`） |

**Request:**

```json
{
  "message": "feat: add new feature\n\nDetailed description.",
  "amend": false,
  "signoff": false
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `message` | string | commit message（支持多行） |
| `amend` | boolean | 是否 amend 上一个 commit |
| `signoff` | boolean | 是否添加 `Signed-off-by` |

**Response:**

```json
{
  "sha": "b2c3d4e5f6g7",
  "branch": "main",
  "message": "feat: add new feature",
  "filesChanged": 3,
  "insertions": 15,
  "deletions": 8
}
```

**逻辑说明:**
- Server 执行 `git commit -m <message> [--amend] [--signoff]`。
- 没有 staged changes 时返回 `invalid_params`。
- commit 使用当前 git identity（由 `identity/*` 管理）。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | 无 staged changes |
| `forbidden` | 无 `git:commit` scope |
| `internal_error` | git commit 失败 |

---

### `_anureo.dev/git/generate_commit_message`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.generate_commit_message` |
| 权限 | Server policy |

**Request:**

```json
{
  "stagedDiff": null
}
```

- `stagedDiff`：可选，client 传入的 diff 内容。省略时 server 自动获取当前 staged diff。

**Response:**

```json
{
  "message": "feat(parser): handle edge case in token splitting",
  "body": "Fixes issue where tokens containing nested brackets\nwere incorrectly split.",
  "alternative": "fix: resolve token splitting with nested brackets"
}
```

**逻辑说明:**
- 使用 small model 生成 commit message（见 `extensions/32-small-model.md`）。
- Small model 优先使用 session 当前的 provider/model；`restrictToPreferredProvider` 禁止全局 fallback。
- 不产生 ACP `session/update`，不消耗 session token。
- 生成的 message 遵循 conventional commits 格式。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | 无 staged diff 内容 |
| `internal_error` | small model 调用失败 |

---

### `_anureo.dev/git/generate_pr_description`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.generate_pr_description` |
| 权限 | Server policy |

**Request:**

```json
{
  "baseBranch": "main",
  "headBranch": "feature-x"
}
```

**Response:**

```json
{
  "title": "Add token splitting with nested bracket support",
  "body": "## Summary\n\nThis PR adds support for splitting tokens containing\nnested brackets...\n\n## Test Plan\n\n- [x] Unit tests added\n- [x] Integration tests pass"
}
```

**逻辑说明:**
- 使用 small model 根据 `baseBranch...headBranch` 的 diff 生成 PR description。
- 语义同 `generate_commit_message`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 分支不存在 |
| `internal_error` | small model 调用失败 |

---

### `_anureo.dev/git/push`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.push` |
| 权限 | Server policy（scope: `git:remote`） |

**Request:**

```json
{
  "remote": "origin",
  "branch": "feature-x",
  "force": false,
  "setUpstream": false
}
```

**Response:**

```json
{
  "remote": "origin",
  "branch": "feature-x",
  "pushed": true,
  "remoteSha": "b2c3d4e5f6g7"
}
```

**逻辑说明:**
- Server 执行 `git push [-u] [--force|--force-with-lease] <remote> <branch>`。
- 长时操作，支持 progress notification。
- `force = true` 时使用 `--force-with-lease` 而非 `--force`，避免覆盖他人推送。
- SSH private key 不出现在 response 中。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | remote 或分支不存在 |
| `forbidden` | 无 `git:remote` scope 或远程拒绝 |
| `invalid_params` | 非快进推送且 `force = false` |
| `internal_error` | 网络故障 |

---

### `_anureo.dev/git/pull`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.pull` |
| 权限 | Server policy（scope: `git:remote`） |

**Request:**

```json
{
  "remote": "origin",
  "branch": "main"
}
```

**Response:**

```json
{
  "remote": "origin",
  "branch": "main",
  "pulled": true,
  "updated": true,
  "fastForward": true,
  "mergeCommit": null
}
```

**逻辑说明:**
- Server 执行 `git pull <remote> <branch>`。
- 长时操作，支持 progress notification。
- 如果产生 merge conflict，返回 `invalid_params` 并附带冲突文件列表。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | remote 或分支不存在 |
| `forbidden` | 无 `git:remote` scope |
| `invalid_params` | 产生 merge conflict |
| `internal_error` | 网络故障 |

---

## Stash

### `_anureo.dev/git/stash/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.stash_list` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "cursor": null,
  "limit": 30
}
```

**Response:**

```json
{
  "items": [
    {
      "index": 0,
      "message": "WIP on main: a1b2c3d",
      "date": "2025-08-19T10:00:00Z",
      "branch": "main"
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- Server 执行 `git stash list --format=<structured>`。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | git 命令失败 |

---

### `_anureo.dev/git/stash/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.stash_create` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "message": "work in progress on feature-x",
  "includeUntracked": false,
  "keepIndex": false
}
```

**Response:**

```json
{
  "index": 0,
  "message": "work in progress on feature-x",
  "created": true
}
```

**逻辑说明:**
- Server 执行 `git stash push [-u] [--keep-index] -m <message>`。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | 无可 stash 的更改 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/stash/pop`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.stash_pop` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "index": 0
}
```

**Response:**

```json
{
  "index": 0,
  "popped": true
}
```

**逻辑说明:**
- Server 执行 `git stash pop stash@{<index>}`。
- 如果产生冲突，stash 不会被删除，返回 `invalid_params`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | stash index 不存在 |
| `invalid_params` | pop 产生冲突 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/stash/apply`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.stash_apply` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "index": 0
}
```

**Response:**

```json
{
  "index": 0,
  "applied": true
}
```

**逻辑说明:**
- Server 执行 `git stash apply stash@{<index>}`。
- 与 `pop` 不同，apply 不删除 stash entry。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | stash index 不存在 |
| `invalid_params` | apply 产生冲突 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/stash/drop`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.stash_drop` |
| 权限 | Server policy（scope: `git:stage`） |

**Request:**

```json
{
  "index": 1
}
```

**Response:**

```json
{
  "index": 1,
  "dropped": true
}
```

**逻辑说明:**
- Server 执行 `git stash drop stash@{<index>}`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | stash index 不存在 |
| `forbidden` | 无 `git:stage` scope |

---

### `_anureo.dev/git/stash/count`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.stash_count` |
| 权限 | Server policy（只读） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "count": 3,
  "files": [
    { "path": "src/main.rs", "insertions": 10, "deletions": 5 },
    { "path": "README.md", "insertions": 2, "deletions": 0 }
  ]
}
```

**逻辑说明:**
- 返回 stash 数量及最新 stash 的文件统计。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | git 命令失败 |

---

## Merge 和 Rebase

### `_anureo.dev/git/merge`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.merge` |
| 权限 | Server policy（scope: `git:history`） |

**Request:**

```json
{
  "branch": "feature-x",
  "strategy": "merge",
  "noFastForward": false
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `branch` | string | 要 merge 的分支 |
| `strategy` | string | `merge` / `squash` |
| `noFastForward` | boolean | `true` 时 `--no-ff`，强制创建 merge commit |

**Response:**

```json
{
  "branch": "feature-x",
  "merged": true,
  "fastForward": false,
  "mergeCommit": "c3d4e5f6g7h8"
}
```

**逻辑说明:**
- Server 执行 `git merge [--no-ff] <branch>` 或 `git merge --squash <branch>`。
- 如果产生冲突，返回 `invalid_params` 并附带冲突文件列表。Client 需使用 `merge_abort` 或解决冲突后 `merge_continue`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 分支不存在 |
| `invalid_params` | 产生 merge conflict |
| `forbidden` | 无 `git:history` scope |

---

### `_anureo.dev/git/merge_abort`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.merge_abort` |
| 权限 | Server policy（scope: `git:history`） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "aborted": true
}
```

**逻辑说明:**
- Server 执行 `git merge --abort`。
- 如果当前没有 merge in-progress，返回 `invalid_params`。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | 当前没有 merge in-progress |
| `forbidden` | 无 `git:history` scope |

---

### `_anureo.dev/git/merge_continue`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.merge_continue` |
| 权限 | Server policy（scope: `git:history`） |

**Request:**

```json
{
  "message": null
}
```

**Response:**

```json
{
  "continued": true,
  "mergeCommit": "c3d4e5f6g7h8"
}
```

**逻辑说明:**
- Server 执行 `git merge --continue`。
- 调用前 client 应确保所有冲突文件已 resolve 并 staged。
- 如果仍有未 resolve 的冲突，返回 `invalid_params`。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | 仍有未 resolve 的冲突，或无 merge in-progress |
| `forbidden` | 无 `git:history` scope |

---

### `_anureo.dev/git/rebase`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.rebase` |
| 权限 | Server policy（scope: `git:history`） |

**Request:**

```json
{
  "branch": "main",
  "interactive": false
}
```

**Response:**

```json
{
  "branch": "main",
  "rebased": true,
  "conflicts": []
}
```

**逻辑说明:**
- Server 执行 `git rebase <branch>` 或 `git rebase -i <branch>`。
- 如果产生冲突，返回 `conflicts` 文件列表，状态为 rebase in-progress。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 分支不存在 |
| `invalid_params` | 产生 rebase conflict |
| `forbidden` | 无 `git:history` scope |

---

### `_anureo.dev/git/rebase_abort`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.rebase_abort` |
| 权限 | Server policy（scope: `git:history`） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "aborted": true
}
```

**逻辑说明:**
- Server 执行 `git rebase --abort`。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | 当前无 rebase in-progress |
| `forbidden` | 无 `git:history` scope |

---

### `_anureo.dev/git/rebase_continue`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.rebase_continue` |
| 权限 | Server policy（scope: `git:history`） |

**Request:**

```json
{
  "skip": false
}
```

**Response:**

```json
{
  "continued": true,
  "remainingConflicts": 0
}
```

**逻辑说明:**
- `skip = false` 时执行 `git rebase --continue`。
- `skip = true` 时执行 `git rebase --skip`（跳过当前 commit）。
- 如果还有更多冲突，返回 `remainingConflicts > 0`。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | 无 rebase in-progress |
| `forbidden` | 无 `git:history` scope |

---

### `_anureo.dev/git/conflict_details`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.conflict_details` |
| 权限 | Server policy（只读） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "operation": "merge",
  "conflictFiles": [
    {
      "path": "src/main.rs",
      "hunks": [
        {
          "oursStart": 10,
          "theirsStart": 10,
          "lines": [
            { "kind": "ours", "content": "    println!(\"hello\");" },
            { "kind": "theirs", "content": "    println!(\"world\");" },
            { "kind": "conflict_marker", "content": "<<<<<<< HEAD" }
          ]
        }
      ]
    }
  ]
}
```

**逻辑说明:**
- 返回当前 merge/rebase in-progress 的冲突详情。
- 包含 unmerged files 和每个文件的 conflict diff。
- 如果没有 in-progress 操作，返回空列表。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | git 命令失败 |

---

## 高级操作

### `_anureo.dev/git/checkout_commit`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.checkout_commit` |
| 权限 | Server policy（scope: `git:history`） |

**Request:**

```json
{
  "commitSha": "a1b2c3d4e5f6"
}
```

**Response:**

```json
{
  "commitSha": "a1b2c3d4e5f6",
  "detachedHead": true
}
```

**逻辑说明:**
- Server 执行 `git checkout <sha>`，进入 detached HEAD 状态。
- 如果工作区有未提交更改冲突，返回 `invalid_params`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | commit 不存在 |
| `invalid_params` | 工作区更改冲突 |
| `forbidden` | 无 `git:history` scope |

---

### `_anureo.dev/git/cherry_pick`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.cherry_pick` |
| 权限 | Server policy（scope: `git:history`） |

**Request:**

```json
{
  "commitSha": "a1b2c3d4e5f6"
}
```

**Response:**

```json
{
  "commitSha": "a1b2c3d4e5f6",
  "cherryPicked": true,
  "newCommitSha": "d4e5f6g7h8i9"
}
```

**逻辑说明:**
- Server 执行 `git cherry-pick <sha>`。
- 如果产生冲突，返回 `invalid_params` 并附带冲突文件。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | commit 不存在 |
| `invalid_params` | 产生冲突 |
| `forbidden` | 无 `git:history` scope |

---

### `_anureo.dev/git/revert_commit`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.revert_commit` |
| 权限 | Server policy（scope: `git:history`） |

**Request:**

```json
{
  "commitSha": "a1b2c3d4e5f6"
}
```

**Response:**

```json
{
  "commitSha": "a1b2c3d4e5f6",
  "reverted": true,
  "revertCommitSha": "d4e5f6g7h8i9"
}
```

**逻辑说明:**
- Server 执行 `git revert <sha>`，创建一个反向 commit。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | commit 不存在 |
| `invalid_params` | 产生冲突 |
| `forbidden` | 无 `git:history` scope |

---

### `_anureo.dev/git/reset_to_commit`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.reset_to_commit` |
| 权限 | Server policy + 建议显式 UI 确认（scope: `git:destructive`） |

**Request:**

```json
{
  "commitSha": "a1b2c3d4e5f6",
  "mode": "soft"
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `commitSha` | string | 目标 commit |
| `mode` | string | `soft`（保留 index 和工作区）/ `mixed`（保留工作区，重置 index）/ `hard`（丢弃所有更改） |

**Response:**

```json
{
  "commitSha": "a1b2c3d4e5f6",
  "mode": "soft",
  "reset": true
}
```

**逻辑说明:**
- Server 执行 `git reset --<mode> <sha>`。
- `mode = "hard"` 是破坏性操作，建议 client 实现 UI 确认弹窗。
- 协议不强制 UI 确认，但 server-side authorization 必须检查 `git:destructive` scope。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | commit 不存在 |
| `forbidden` | 无 `git:destructive` scope |
| `invalid_params` | mode 参数非法 |

---

## Worktree 目录校验

### `_anureo.dev/git/validate_worktree_directory`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.validate_worktree_directory` |
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
  "valid": true,
  "worktreeRoot": "/home/user/project",
  "normalizedPath": "/home/user/project/.worktrees/feature-x"
}
```

**逻辑说明:**
- 校验 `path` 在 server 配置的 worktreeRoot 内。
- 处理 `..` 路径段、symlink 和大小写差异。
- 不修改任何状态。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | 路径在 worktreeRoot 外或包含非法段 |

---

### `_anureo.dev/git/canonicalize_worktree_state`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.canonicalize_worktree_state` |
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
  "branch": "feature-x",
  "head": "a1b2c3d4e5f6",
  "attentionReason": null,
  "state": "clean"
}
```

**逻辑说明:**
- 返回 worktree 的规范化状态信息。
- `state` 可选值：`clean`、`dirty`、`merge_in_progress`、`rebase_in_progress`、`detached`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | worktree 不存在 |

---

### `_anureo.dev/git/is_linked_worktree`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.is_linked_worktree` |
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
  "isLinked": true,
  "mainWorktree": "/home/user/project"
}
```

**逻辑说明:**
- 判断指定路径是否为 linked worktree（非主 worktree）。
- `isLinked = false` 且 `mainWorktree = path` 时为主 worktree。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 路径不是 git worktree |

---

## Git Identity

> 命名空间: `_anureo.dev/git/identity/*`
> Capability 子域: `git.identity_*`

### `_anureo.dev/git/identity/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.identity_list` |
| 权限 | Server policy（只读） |

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
      "profileId": "work",
      "name": "Alice Work",
      "email": "alice@company.com",
      "scope": "global"
    },
    {
      "profileId": "personal",
      "name": "Alice Personal",
      "email": "alice@personal.com",
      "scope": "repo"
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- 返回所有已配置的 Git 身份 profile。
- profile 存储在 server 管理的配置文件中，不依赖 git 全局配置。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | 配置读取失败 |

---

### `_anureo.dev/git/identity/get`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.identity_get` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "path": null
}
```

**Response:**

```json
{
  "profileId": "work",
  "name": "Alice Work",
  "email": "alice@company.com",
  "source": "repo_config"
}
```

**逻辑说明:**
- 返回指定 worktree 目录当前生效的 Git 身份。
- 解析顺序：worktree local config > repo config > global config。
- `source` 标识身份来源。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | 无配置身份 |
| `internal_error` | 解析失败 |

---

### `_anureo.dev/git/identity/get_global`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.identity_get_global` |
| 权限 | Server policy（只读） |

**Request:**

```json
{}
```

**Response:**

```json
{
  "name": "Alice",
  "email": "alice@example.com",
  "source": "global_config"
}
```

**逻辑说明:**
- 返回全局 Git 身份配置。
- 如果未配置全局身份，返回 null 字段。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | git config 读取失败 |

---

### `_anureo.dev/git/identity/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.identity_create` |
| 权限 | Server policy（scope: `git:identity`） |

**Request:**

```json
{
  "profileId": "work",
  "name": "Alice Work",
  "email": "alice@company.com"
}
```

**Response:**

```json
{
  "profileId": "work",
  "name": "Alice Work",
  "email": "alice@company.com",
  "created": true
}
```

**逻辑说明:**
- 创建新的身份 profile，存储在 server 配置中。
- `profileId` 必须唯一。

**Error:**

| kind | 触发条件 |
|---|---|
| `already_exists` | profileId 已存在 |
| `invalid_params` | name 或 email 格式非法 |
| `forbidden` | 无 `git:identity` scope |

---

### `_anureo.dev/git/identity/update`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.identity_update` |
| 权限 | Server policy（scope: `git:identity`） |

**Request:**

```json
{
  "profileId": "work",
  "name": "Alice Smith",
  "email": "alice.smith@company.com"
}
```

**Response:**

```json
{
  "profileId": "work",
  "name": "Alice Smith",
  "email": "alice.smith@company.com",
  "updated": true
}
```

**逻辑说明:**
- 更新已有 profile 的 name/email。
- 如果 profile 当前被某 worktree 使用，更新后 `identity/changed` notification 会被推送。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | profileId 不存在 |
| `forbidden` | 无 `git:identity` scope |

---

### `_anureo.dev/git/identity/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.identity_delete` |
| 权限 | Server policy（scope: `git:identity`） |

**Request:**

```json
{
  "profileId": "old-profile"
}
```

**Response:**

```json
{
  "profileId": "old-profile",
  "deleted": true
}
```

**逻辑说明:**
- 删除身份 profile。
- 如果 profile 当前被某 worktree 使用，不影响已有配置（只删除 profile 定义）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | profileId 不存在 |
| `forbidden` | 无 `git:identity` scope |

---

### `_anureo.dev/git/identity/set`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.identity_set` |
| 权限 | Server policy（scope: `git:identity`） |

**Request:**

```json
{
  "profileId": "work",
  "path": null
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `profileId` | string | 要应用的 profile |
| `path` | string? | 目标 worktree 路径；省略时使用当前 session 工作目录 |

**Response:**

```json
{
  "profileId": "work",
  "path": "/home/user/project/.worktrees/feature-x",
  "applied": true,
  "scope": "repo"
}
```

**逻辑说明:**
- 将指定 profile 应用到目标 worktree 的 git config。
- `scope` 取决于 server 策略：`repo`（写入 repo `.git/config`）或 `worktree`（写入 worktree local config）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | profile 或 worktree 不存在 |
| `forbidden` | 无 `git:identity` scope |

---

### `_anureo.dev/git/identity/discover_credentials`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `git.identity_discover_credentials` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "remote": "origin"
}
```

**Response:**

```json
{
  "remote": "origin",
  "credentialHelpers": [
    { "type": "store", "available": true },
    { "type": "credential_helper", "available": true, "helper": "osxkeychain" },
    { "type": "ssh_key", "available": true, "keyType": "ed25519" }
  ],
  "hasCredentials": true
}
```

**逻辑说明:**
- 检测当前 git 环境可用的凭据方式。
- **安全关键**：返回结果不包含任何 private key 内容、密码、token。
- `credentialHelpers` 只返回类型和可用性。
- `ssh_key.keyType` 只返回密钥算法类型（如 `ed25519`、`rsa`），不返回密钥内容。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | remote 不存在 |
| `internal_error` | 检测失败 |

---

## Notifications

### `_anureo.dev/git/status_changed`

当 Git 状态发生变化（stage/unstage/commit/branch 切换/fetch/pull/外部 git 操作）时推送。

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/git/status_changed",
  "params": {
    "branch": "main",
    "dirty": true
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `branch` | string | 当前分支名 |
| `dirty` | boolean | 是否有未提交更改 |

- notification 只推送变更提示，不包含完整 diff。
- Client 收到后应调用 `git/status` 获取完整状态。

### `_anureo.dev/git/identity/changed`

当 Git 身份配置发生变化时推送。

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/git/identity/changed",
  "params": {
    "change": "updated",
    "profileId": "work"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | string | `created` / `updated` / `deleted` / `set` |
| `profileId` | string | 变化的 profile ID |

## SSH 密钥安全

1. SSH private key 内容**绝不**出现在任何 response、日志、`session/update` 或 `data` 字段中。
2. `identity/discover_credentials` 只返回密钥类型（`ed25519` / `rsa`），不返回密钥内容。
3. Git 操作使用的 SSH agent / credential helper 由 server 内部调用，不经 client。
4. 如果 SSH 认证失败，返回 `forbidden` 或 `internal_error`，不暴露密钥相关信息。

## Reconnect Resync

| Notification | Authoritative method |
|---|---|
| `_anureo.dev/git/status_changed` | `_anureo.dev/git/status` |
| `_anureo.dev/git/identity/changed` | `_anureo.dev/git/identity/list` |

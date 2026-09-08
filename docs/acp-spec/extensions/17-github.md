# GitHub 扩展

> 命名空间: `_anureo.dev/github/*`
> Capability key: `github`
> 实现状态: ❌ 未实现

---

## Capability

```json
{
  "github": {
    "auth_status": true,
    "auth_start": true,
    "auth_complete": true,
    "auth_disconnect": true,
    "auth_activate": true,
    "auth_set_gh_cli_disabled": true,
    "pr_status": true,
    "prs_list": true,
    "pr_context": true,
    "pr_create": true,
    "pr_update": true,
    "pr_merge": true,
    "pr_ready": true,
    "issues_list": true,
    "issue_get": true,
    "issue_comments": true,
    "repo_upstream": true,
    "repo_branches": true
  }
}
```

GitHub OAuth device flow、token 刷新和 Octokit client 属于 server 实现；扩展只暴露结构化结果。

**Token 安全规则：**
- GitHub access token、refresh token **不得**出现在任何 response、`session/update` 或普通日志中
- Token 存储在 server 端加密的 credential store 中
- Response 只返回脱敏标识（如 `****1234`）和 scope 信息
- `auth_changed` notification 不携带 token

---

## Methods

### 1.1 认证（Auth）

---

### `_anureo.dev/github/auth_status`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.auth_status` |
| 权限 | 无 |

查询当前 GitHub 认证状态，包括已授权 scope 和多账号信息。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/github/auth_status",
  "params": {}
}
```

无参数。

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "authenticated": true,
    "accounts": [
      {
        "id": "gh-user-12345",
        "login": "octocat",
        "displayName": "The Octocat",
        "avatarUrl": "https://avatars.githubusercontent.com/u/583231?v=4",
        "scopes": ["repo", "workflow", "read:org"],
        "active": true,
        "tokenType": "oauth",
        "tokenMasked": "gho_****abcd"
      }
    ],
    "activeAccountId": "gh-user-12345",
    "ghCliAvailable": true,
    "ghCliDisabled": false
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `authenticated` | bool | 是否至少有一个已认证账号 |
| `accounts` | Account[] | 已认证账号列表 |
| `accounts[].id` | string | GitHub 用户 ID |
| `accounts[].login` | string | GitHub 用户名 |
| `accounts[].displayName` | string | 显示名称 |
| `accounts[].avatarUrl` | string | 头像 URL |
| `accounts[].scopes` | string[] | 已授权 scope 列表 |
| `accounts[].active` | bool | 是否为当前活跃账号 |
| `accounts[].tokenType` | `"oauth"` \| `"gh_cli"` | Token 来源 |
| `accounts[].tokenMasked` | string | 脱敏 token 标识 |
| `activeAccountId` | string\|null | 当前活跃账号 ID |
| `ghCliAvailable` | bool | 系统是否安装了 gh CLI |
| `ghCliDisabled` | bool | 用户是否手动禁用了 gh CLI fallback |

#### 逻辑说明

1. **多账号**: Server 支持多个 GitHub 账号同时认证。`activeAccountId` 指向当前操作的账号。
2. **gh CLI fallback**: 当 OAuth 未认证但系统已安装 gh CLI 且未禁用时，Server 可通过 gh CLI 获取有限的 GitHub 功能。
3. **Token 不外泄**: `tokenMasked` 只保留后 4 位用于用户识别，不泄露完整 token。

#### Rust 类型

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAccount {
    pub id: String,
    pub login: String,
    pub display_name: String,
    pub avatar_url: String,
    pub scopes: Vec<String>,
    pub active: bool,
    pub token_type: GithubTokenType,
    pub token_masked: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GithubTokenType {
    Oauth,
    GhCli,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthStatusResponse {
    pub authenticated: bool,
    pub accounts: Vec<GithubAccount>,
    pub active_account_id: Option<String>,
    pub gh_cli_available: bool,
    pub gh_cli_disabled: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | Credential store 不可用 |

---

### `_anureo.dev/github/auth_start`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.auth_start` |
| 权限 | 无 |
| Timeout | 30s（仅启动阶段；device flow 轮询由 `auth_complete` 负责） |

启动 OAuth device flow，返回 device code 和用户验证 URL。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/github/auth_start",
  "params": {
    "scopes": ["repo", "workflow", "read:org"]
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `scopes` | string[] | 否 | 请求的 OAuth scope；默认 `["repo"]` |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "deviceCode": "device-code-internal-id",
    "userCode": "ABCD-1234",
    "verificationUri": "https://github.com/login/device",
    "verificationUriComplete": "https://github.com/login/device?user_code=ABCD-1234",
    "expiresIn": 900,
    "interval": 5
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `deviceCode` | string | Server 内部 device flow 标识（非 GitHub device_code） |
| `userCode` | string | 用户在浏览器输入的 code |
| `verificationUri` | string | 用户访问的验证 URL |
| `verificationUriComplete` | string | 包含 userCode 的完整 URL（可直接打开） |
| `expiresIn` | number | device code 过期秒数 |
| `interval` | number | `auth_complete` 轮询间隔（秒） |

#### 逻辑说明

1. **Device flow**: Server 调用 GitHub `POST /login/device/code` 获取 device code。
2. **Client 职责**: Client 打开 `verificationUriComplete` 并显示 `userCode`，然后调用 `auth_complete` 轮询完成状态。
3. **Client ID 安全**: OAuth Client ID 属于 server 配置，不暴露在 response 中。
4. **单次**: 同一时间只能有一个 active device flow。重复调用 `auth_start` 取消前一个。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthStartRequest {
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
}

fn default_scopes() -> Vec<String> {
    vec!["repo".to_string()]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthStartResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | GitHub device code endpoint 不可达 |
| `Too Many Requests (-32000)` | 超过 GitHub rate limit |

---

### `_anureo.dev/github/auth_complete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.auth_complete` |
| 权限 | 无 |

轮询 device flow 完成状态。Server 内部按 `interval` 间隔轮询 GitHub token endpoint。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_anureo.dev/github/auth_complete",
  "params": {
    "deviceCode": "device-code-internal-id"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `deviceCode` | string | 是 | `auth_start` 返回的 deviceCode |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "status": "complete",
    "account": {
      "id": "gh-user-12345",
      "login": "octocat",
      "displayName": "The Octocat",
      "avatarUrl": "https://avatars.githubusercontent.com/u/583231?v=4",
      "scopes": ["repo", "workflow", "read:org"],
      "active": true,
      "tokenType": "oauth",
      "tokenMasked": "gho_****abcd"
    }
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `status` | `"pending"` \| `"complete"` \| `"expired"` \| `"error"` | 认证流程状态 |
| `account` | Account\|null | `complete` 时返回新认证的账号信息 |
| `error` | string\|null | `error` 状态时的错误描述 |

#### 逻辑说明

1. **轮询模式**: Client 按 `auth_start` 返回的 `interval` 调用此方法。
2. **`pending`**: 用户尚未在浏览器完成授权，Client 应等待后重试。
3. **`expired`**: Device code 过期，Client 需重新调用 `auth_start`。
4. **`complete`**: 认证成功，Server 已获取并存储 access token。Client 收到后发送 `auth_changed` notification（Server 侧触发）。
5. **Token 安全**: 完成后返回的 `account` 中不包含 access token，只有脱敏标识。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubAuthCompleteStatus {
    Pending,
    Complete,
    Expired,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthCompleteRequest {
    pub device_code: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthCompleteResponse {
    pub status: GithubAuthCompleteStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<GithubAccount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `deviceCode` 不存在或已过期 |
| `Internal Error (-32603)` | GitHub token endpoint 不可达 |

---

### `_anureo.dev/github/auth_disconnect`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.auth_disconnect` |
| 权限 | Server-side authorization（`github:write` scope） |

断开指定 GitHub 账号的认证，撤销存储的 token。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "_anureo.dev/github/auth_disconnect",
  "params": {
    "accountId": "gh-user-12345"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `accountId` | string | 否 | 要断开的账号 ID；省略则断开活跃账号 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "disconnected": true,
    "accountId": "gh-user-12345",
    "remainingAccounts": []
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `disconnected` | bool | 是否成功断开 |
| `accountId` | string | 被断开的账号 ID |
| `remainingAccounts` | Account[] | 剩余已认证账号（可能为空） |

#### 逻辑说明

1. **Token 撤销**: Server 调用 GitHub `DELETE /applications/{client-id}/token` 撤销 token（best-effort），然后从本地 credential store 删除。
2. **活跃账号切换**: 若断开的是活跃账号且存在其他账号，Server 自动切换到下一个。
3. **Capability 变化**: 断开最后一个账号后，`github/pr_*`、`github/issues_*` 等 method 可能变为不可用（依赖 gh CLI fallback 是否启用）。Server 发送 `_anureo.dev/capability_changed` notification。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthDisconnectRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthDisconnectResponse {
    pub disconnected: bool,
    pub account_id: String,
    pub remaining_accounts: Vec<GithubAccount>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `accountId` 不存在 |
| `Forbidden (-32603)` | Server-side authorization 拒绝 |

---

### `_anureo.dev/github/auth_activate`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.auth_activate` |
| 权限 | 无 |

切换活跃 GitHub 账号（多账号场景）。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "_anureo.dev/github/auth_activate",
  "params": {
    "accountId": "gh-user-67890"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `accountId` | string | 是 | 要激活的账号 ID |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "activated": true,
    "accountId": "gh-user-67890"
  }
}
```

#### 逻辑说明

1. **Octokit 切换**: Server 将内部 Octokit client 切换到新活跃账号的 token。
2. **Notification**: 激活后发送 `auth_changed` notification。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthActivateRequest {
    pub account_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthActivateResponse {
    pub activated: bool,
    pub account_id: String,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `accountId` 不存在或已断开 |

---

### `_anureo.dev/github/auth_set_gh_cli_disabled`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.auth_set_gh_cli_disabled` |
| 权限 | 无 |

启用或禁用 gh CLI fallback。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "method": "_anureo.dev/github/auth_set_gh_cli_disabled",
  "params": {
    "disabled": true
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `disabled` | bool | 是 | `true` 禁用 gh CLI fallback |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 6,
  "result": {
    "ghCliDisabled": true
  }
}
```

#### 逻辑说明

1. **持久化**: 设置持久化到 server 配置，跨连接生效。
2. **Capability 影响**: 禁用 gh CLI 后，若 OAuth 也未认证，所有 `github/*` method 返回 `capability_not_supported`。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubSetGhCliDisabledRequest {
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubSetGhCliDisabledResponse {
    pub gh_cli_disabled: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | 配置持久化失败 |

---

### 1.2 Pull Request

---

### `_anureo.dev/github/pr_status`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.pr_status` |
| 权限 | 无 |

查询当前分支关联的 PR 状态、CI 检查和 mergeable 信息。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "method": "_anureo.dev/github/pr_status",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "branch": "feature/new-api"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `owner` | string | 否 | repo owner；省略则从 git remote 推断 |
| `repo` | string | 否 | repo 名称；省略则从 git remote 推断 |
| `branch` | string | 否 | 分支名；省略则使用当前分支 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "result": {
    "found": true,
    "pullRequest": {
      "number": 42,
      "title": "Add new API endpoint",
      "state": "open",
      "draft": false,
      "url": "https://github.com/myorg/myrepo/pull/42",
      "headRefName": "feature/new-api",
      "baseRefName": "main",
      "mergeable": "MERGEABLE",
      "mergeStateStatus": "CLEAN",
      "author": {
        "login": "octocat",
        "avatarUrl": "https://avatars.githubusercontent.com/u/583231?v=4"
      },
      "createdAt": "2025-01-15T10:00:00Z",
      "updatedAt": "2025-01-16T08:30:00Z",
      "reviewDecision": "APPROVED",
      "checks": {
        "totalCount": 3,
        "passedCount": 2,
        "failedCount": 0,
        "pendingCount": 1,
        "conclusion": "pending"
      }
    }
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `found` | bool | 当前分支是否关联了 PR |
| `pullRequest` | object\|null | PR 详情（`found` 为 true 时） |
| `pullRequest.mergeable` | `"MERGEABLE"` \| `"CONFLICTING"` \| `"UNKNOWN"` | GitHub mergeable 状态 |
| `pullRequest.mergeStateStatus` | string | GitHub merge state（CLEAN/DIRTY/BLOCKED/BEHIND 等） |
| `pullRequest.reviewDecision` | string\|null | review 结果（APPROVED/REVIEW_REQUIRED/CHANGES_REQUESTED） |
| `pullRequest.checks` | object | CI check 汇总 |

#### 逻辑说明

1. **Remote 推断**: 未指定 `owner`/`repo` 时，Server 从 git remote 的 `origin` URL 解析。
2. **GraphQL**: Server 通过 GitHub GraphQL API 一次性获取 PR + checks + review 状态，减少 API 调用。
3. **缓存**: 结果可短期缓存（30s），避免频繁 API 调用。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrStatusRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubAuthor {
    pub login: String,
    pub avatar_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubCheckSummary {
    pub total_count: u32,
    pub passed_count: u32,
    pub failed_count: u32,
    pub pending_count: u32,
    pub conclusion: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPullRequest {
    pub number: u32,
    pub title: String,
    pub state: String,
    pub draft: bool,
    pub url: String,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub mergeable: String,
    pub merge_state_status: String,
    pub author: GithubAuthor,
    pub created_at: String,
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_decision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks: Option<GithubCheckSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrStatusResponse {
    pub found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<GithubPullRequest>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Capability Not Supported (-32001)` | 未认证且 gh CLI 不可用 |
| `Invalid Params (-32602)` | 无法推断 owner/repo 且未显式传入 |
| `Internal Error (-32603)` | GitHub API 不可达或返回错误 |

---

### `_anureo.dev/github/prs_list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.prs_list` |
| 权限 | 无 |

分页查询 PR 列表。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 11,
  "method": "_anureo.dev/github/prs_list",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "state": "open",
    "cursor": null,
    "limit": 20
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `owner` | string | 否 | repo owner；省略从 remote 推断 |
| `repo` | string | 否 | repo 名称；省略从 remote 推断 |
| `state` | `"open"` \| `"closed"` \| `"all"` | 否 | PR 状态过滤，默认 `"open"` |
| `cursor` | string\|null | 否 | 分页游标（见 `08-cross-cutting-patterns.md` §1 分页协议） |
| `limit` | number | 否 | 每页数量，默认 20 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 11,
  "result": {
    "items": [
      {
        "number": 42,
        "title": "Add new API endpoint",
        "state": "open",
        "draft": false,
        "url": "https://github.com/myorg/myrepo/pull/42",
        "headRefName": "feature/new-api",
        "baseRefName": "main",
        "author": { "login": "octocat", "avatarUrl": "..." },
        "updatedAt": "2025-01-16T08:30:00Z"
      }
    ],
    "nextCursor": "cursor-abc",
    "hasMore": true
  }
}
```

遵循统一分页协议（`08-cross-cutting-patterns.md` §1）。`items` 中每项为精简版 PR（无 checks/review 字段）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrsListRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default = "default_pr_state")]
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_page_limit")]
    pub limit: u32,
}

fn default_pr_state() -> String { "open".to_string() }
fn default_page_limit() -> u32 { 20 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrSummary {
    pub number: u32,
    pub title: String,
    pub state: String,
    pub draft: bool,
    pub url: String,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub author: GithubAuthor,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrsListResponse {
    pub items: Vec<GithubPrSummary>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Capability Not Supported (-32001)` | 未认证 |
| `Invalid Params (-32602)` | 无法推断 owner/repo |
| `Internal Error (-32603)` | GitHub API 错误 |

---

### `_anureo.dev/github/pr_context`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.pr_context` |
| 权限 | 无 |

获取 PR 完整上下文：review comments、files、diff、check runs。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 12,
  "method": "_anureo.dev/github/pr_context",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "number": 42
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `owner` | string | 否 | repo owner |
| `repo` | string | 否 | repo 名称 |
| `number` | number | 是 | PR 编号 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 12,
  "result": {
    "pullRequest": { "...": "完整 PR 对象" },
    "reviewThreads": [
      {
        "id": "rt-001",
        "path": "src/api.rs",
        "line": 42,
        "resolved": false,
        "comments": [
          {
            "author": { "login": "reviewer1", "avatarUrl": "..." },
            "body": "建议添加错误处理",
            "createdAt": "2025-01-15T12:00:00Z"
          }
        ]
      }
    ],
    "files": [
      {
        "path": "src/api.rs",
        "additions": 15,
        "deletions": 3,
        "status": "modified"
      }
    ],
    "checkRuns": [
      {
        "name": "CI / test",
        "status": "completed",
        "conclusion": "success",
        "url": "https://github.com/myorg/myrepo/runs/123"
      }
    ]
  }
}
```

#### 逻辑说明

1. **批量获取**: Server 通过单次 GraphQL 请求获取全部上下文，减少 API 往返。
2. **Diff 截断**: 大 diff（>500 行）按文件返回摘要，完整 diff 可通过 `git/file_diff` 获取。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrContextRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub number: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubReviewThread {
    pub id: String,
    pub path: String,
    pub line: u32,
    pub resolved: bool,
    pub comments: Vec<GithubReviewComment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubReviewComment {
    pub author: GithubAuthor,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrFile {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubCheckRun {
    pub name: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrContextResponse {
    pub pull_request: GithubPullRequest,
    pub review_threads: Vec<GithubReviewThread>,
    pub files: Vec<GithubPrFile>,
    pub check_runs: Vec<GithubCheckRun>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | PR number 不存在 |
| `Capability Not Supported (-32001)` | 未认证 |
| `Internal Error (-32603)` | GitHub API 错误 |

---

### `_anureo.dev/github/pr_create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.pr_create` |
| 权限 | Server-side authorization（`github:write` scope）；建议 UI 确认 |
| Timeout | 30s |

创建 PR。支持 fork upstream 场景。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 13,
  "method": "_anureo.dev/github/pr_create",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "title": "Add new API endpoint",
    "body": "## Summary\n\nThis PR adds a new REST API endpoint for user preferences.\n\n## Changes\n\n- New `GET /api/preferences` route\n- Updated OpenAPI spec",
    "headBranch": "feature/new-api",
    "baseBranch": "main",
    "draft": false,
    "forkUpstream": false
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `owner` | string | 否 | repo owner |
| `repo` | string | 否 | repo 名称 |
| `title` | string | 是 | PR 标题 |
| `body` | string | 否 | PR 描述 |
| `headBranch` | string | 否 | 源分支；省略使用当前分支 |
| `baseBranch` | string | 否 | 目标分支；省略使用 upstream default branch |
| `draft` | bool | 否 | 是否创建 draft PR |
| `forkUpstream` | bool | 否 | 若当前 fork 无 origin 权限，是否 fork 到 upstream 并从 fork 创建 PR |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 13,
  "result": {
    "pullRequest": {
      "number": 43,
      "title": "Add new API endpoint",
      "url": "https://github.com/myorg/myrepo/pull/43",
      "draft": false,
      "state": "open"
    }
  }
}
```

#### 逻辑说明

1. **Push 前提**: 创建 PR 前 Server 检查 head branch 是否已 push 到 remote。未 push 时返回错误。
2. **Fork upstream**: `forkUpstream` 为 true 时，Server 先 fork repo 到活跃账号，push 分支到 fork，再从 fork 向 upstream 创建 PR。
3. **AI 生成**: PR body 可通过 `git/generate_pr_description`（small model）预生成。
4. **进度**: 此操作可能超过 5s（尤其涉及 fork），支持进度 notification（`08-cross-cutting-patterns.md` §3）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrCreateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub fork_upstream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrCreateResponse {
    pub pull_request: GithubPrSummary,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | title 为空或分支不存在 |
| `Forbidden (-32603)` | 无 repo 写权限 |
| `Capability Not Supported (-32001)` | 未认证 |
| `Internal Error (-32603)` | GitHub API 错误（如同名 PR 已存在） |

---

### `_anureo.dev/github/pr_update`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.pr_update` |
| 权限 | Server-side authorization（`github:write` scope） |

更新 PR 的 title 和/或 body。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 14,
  "method": "_anureo.dev/github/pr_update",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "number": 42,
    "title": "Updated: Add new API endpoint",
    "body": "Updated description..."
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `number` | number | 是 | PR 编号 |
| `title` | string | 否 | 新标题（省略则不修改） |
| `body` | string | 否 | 新描述（省略则不修改） |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 14,
  "result": {
    "updated": true,
    "pullRequest": { "number": 42, "title": "Updated: Add new API endpoint", "...": "..." }
  }
}
```

#### 逻辑说明

1. **部分更新**: `title` 和 `body` 均可选，只更新传入的字段。
2. **权限**: 需要对目标 repo 有写权限或是 PR 作者。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrUpdateRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub number: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrUpdateResponse {
    pub updated: bool,
    pub pull_request: GithubPrSummary,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | title 和 body 均未提供，或 PR 不存在 |
| `Forbidden (-32603)` | 无修改权限 |

---

### `_anureo.dev/github/pr_merge`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.pr_merge` |
| 权限 | Server-side authorization（`github:write` scope）；建议 UI 显式确认 |

合并 PR。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 15,
  "method": "_anureo.dev/github/pr_merge",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "number": 42,
    "mergeMethod": "squash",
    "commitTitle": "Add new API endpoint (#42)",
    "commitMessage": "Squashed commit body",
    "deleteBranch": true
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `number` | number | 是 | PR 编号 |
| `mergeMethod` | `"merge"` \| `"squash"` \| `"rebase"` | 否 | 合并方式，默认 `"merge"` |
| `commitTitle` | string | 否 | 自定义 merge commit 标题 |
| `commitMessage` | string | 否 | 自定义 merge commit body |
| `deleteBranch` | bool | 否 | 合并后删除 head 分支，默认 `false` |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 15,
  "result": {
    "merged": true,
    "mergeCommitSha": "abc123def456",
    "branchDeleted": true
  }
}
```

#### 逻辑说明

1. **前置检查**: Server 先验证 PR mergeable 状态、CI 通过、review approved。不满足时返回 `Invalid Params` 并附带原因。
2. **不可逆**: 合并是破坏性操作。Client 应在 UI 层弹出确认。
3. **删除分支**: `deleteBranch` 为 true 时，合并成功后删除远程和本地 head 分支。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GithubMergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrMergeRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub number: u32,
    #[serde(default = "default_merge_method")]
    pub merge_method: GithubMergeMethod,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
    #[serde(default)]
    pub delete_branch: bool,
}

fn default_merge_method() -> GithubMergeMethod { GithubMergeMethod::Merge }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrMergeResponse {
    pub merged: bool,
    pub merge_commit_sha: String,
    pub branch_deleted: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | PR 不可合并（冲突/CI 未通过/review 未通过） |
| `Forbidden (-32603)` | 无合并权限 |
| `Conflict (-32000)` | mergeable 状态为 CONFLICTING |

---

### `_anureo.dev/github/pr_ready`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.pr_ready` |
| 权限 | Server-side authorization（`github:write` scope） |

将 draft PR 标记为 ready for review。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 16,
  "method": "_anureo.dev/github/pr_ready",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "number": 42
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 16,
  "result": {
    "ready": true
  }
}
```

#### 逻辑说明

1. **GraphQL**: Server 使用 GraphQL `markPullRequestReadyForReview` mutation。
2. **非 draft**: 对非 draft PR 调用时返回 `ready: true`（幂等）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrReadyRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub number: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubPrReadyResponse {
    pub ready: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | PR 不存在 |
| `Forbidden (-32603)` | 非 PR 作者 |

---

### 1.3 Issue

---

### `_anureo.dev/github/issues_list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.issues_list` |
| 权限 | 无 |

分页查询 issue 列表。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "_anureo.dev/github/issues_list",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "state": "open",
    "labels": ["bug", "enhancement"],
    "cursor": null,
    "limit": 20
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `state` | `"open"` \| `"closed"` \| `"all"` | 否 | 默认 `"open"` |
| `labels` | string[] | 否 | 标签过滤 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "result": {
    "items": [
      {
        "number": 100,
        "title": "Fix memory leak in parser",
        "state": "open",
        "url": "https://github.com/myorg/myrepo/issues/100",
        "author": { "login": "octocat", "avatarUrl": "..." },
        "labels": ["bug"],
        "createdAt": "2025-01-10T09:00:00Z",
        "updatedAt": "2025-01-15T14:00:00Z"
      }
    ],
    "nextCursor": null,
    "hasMore": false
  }
}
```

遵循统一分页协议（`08-cross-cutting-patterns.md` §1）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubIssuesListRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default = "default_pr_state")]
    pub state: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_page_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubIssueSummary {
    pub number: u32,
    pub title: String,
    pub state: String,
    pub url: String,
    pub author: GithubAuthor,
    pub labels: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubIssuesListResponse {
    pub items: Vec<GithubIssueSummary>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Capability Not Supported (-32001)` | 未认证 |
| `Invalid Params (-32602)` | 无法推断 owner/repo |

---

### `_anureo.dev/github/issue_get`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.issue_get` |
| 权限 | 无 |

获取单个 issue 详情。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "method": "_anureo.dev/github/issue_get",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "number": 100
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 21,
  "result": {
    "number": 100,
    "title": "Fix memory leak in parser",
    "state": "open",
    "url": "https://github.com/myorg/myrepo/issues/100",
    "body": "## Description\n\nThe parser leaks memory when...",
    "author": { "login": "octocat", "avatarUrl": "..." },
    "labels": ["bug", "high-priority"],
    "assignees": [{ "login": "dev1", "avatarUrl": "..." }],
    "createdAt": "2025-01-10T09:00:00Z",
    "updatedAt": "2025-01-15T14:00:00Z",
    "commentsCount": 5
  }
}
```

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubIssueGetRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub number: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubIssueDetail {
    pub number: u32,
    pub title: String,
    pub state: String,
    pub url: String,
    pub body: String,
    pub author: GithubAuthor,
    pub labels: Vec<String>,
    pub assignees: Vec<GithubAuthor>,
    pub created_at: String,
    pub updated_at: String,
    pub comments_count: u32,
}

pub type GithubIssueGetResponse = GithubIssueDetail;
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | issue 不存在 |
| `Capability Not Supported (-32001)` | 未认证 |

---

### `_anureo.dev/github/issue_comments`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.issue_comments` |
| 权限 | 无 |

获取 issue 评论列表。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 22,
  "method": "_anureo.dev/github/issue_comments",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "number": 100,
    "cursor": null,
    "limit": 30
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 22,
  "result": {
    "items": [
      {
        "id": "comment-001",
        "author": { "login": "dev1", "avatarUrl": "..." },
        "body": "I can reproduce this on v2.3.1",
        "createdAt": "2025-01-11T10:00:00Z"
      }
    ],
    "nextCursor": null,
    "hasMore": false
  }
}
```

遵循统一分页协议（`08-cross-cutting-patterns.md` §1）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubIssueCommentsRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    pub number: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_page_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubIssueComment {
    pub id: String,
    pub author: GithubAuthor,
    pub body: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubIssueCommentsResponse {
    pub items: Vec<GithubIssueComment>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | issue 不存在 |
| `Capability Not Supported (-32001)` | 未认证 |

---

### 1.4 Repo

---

### `_anureo.dev/github/repo_upstream`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.repo_upstream` |
| 权限 | 无 |

查询当前 repo 的 fork upstream 信息和 default branch。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 30,
  "method": "_anureo.dev/github/repo_upstream",
  "params": {
    "owner": "myfork",
    "repo": "myrepo"
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 30,
  "result": {
    "isFork": true,
    "upstream": {
      "owner": "original-org",
      "repo": "myrepo",
      "defaultBranch": "main",
      "url": "https://github.com/original-org/myrepo"
    },
    "current": {
      "owner": "myfork",
      "repo": "myrepo",
      "defaultBranch": "main",
      "url": "https://github.com/myfork/myrepo"
    }
  }
}
```

#### 逻辑说明

1. **非 fork**: 若当前 repo 不是 fork，`isFork` 为 false，`upstream` 为 null。
2. **PR 路由**: `pr_create` 使用此信息决定默认 base branch。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubRepoInfo {
    pub owner: String,
    pub repo: String,
    pub default_branch: String,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubRepoUpstreamRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubRepoUpstreamResponse {
    pub is_fork: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<GithubRepoInfo>,
    pub current: GithubRepoInfo,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Capability Not Supported (-32001)` | 未认证 |
| `Invalid Params (-32602)` | repo 不存在 |

---

### `_anureo.dev/github/repo_branches`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `github.repo_branches` |
| 权限 | 无 |

查询指定 repo 的远程分支列表。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 31,
  "method": "_anureo.dev/github/repo_branches",
  "params": {
    "owner": "myorg",
    "repo": "myrepo",
    "cursor": null,
    "limit": 50
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 31,
  "result": {
    "items": [
      {
        "name": "main",
        "isDefault": true,
        "protected": true,
        "lastCommitSha": "abc123",
        "lastCommitDate": "2025-01-15T10:00:00Z"
      },
      {
        "name": "develop",
        "isDefault": false,
        "protected": false,
        "lastCommitSha": "def456",
        "lastCommitDate": "2025-01-14T08:00:00Z"
      }
    ],
    "nextCursor": null,
    "hasMore": false
  }
}
```

遵循统一分页协议（`08-cross-cutting-patterns.md` §1）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubRepoBranchesRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_page_limit")]
    pub limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubBranch {
    pub name: String,
    pub is_default: bool,
    pub protected: bool,
    pub last_commit_sha: String,
    pub last_commit_date: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GithubRepoBranchesResponse {
    pub items: Vec<GithubBranch>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Capability Not Supported (-32001)` | 未认证 |
| `Invalid Params (-32602)` | repo 不存在 |

---

## Notifications

### `_anureo.dev/github/auth_changed`

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/github/auth_changed",
  "params": {
    "authenticated": true,
    "activeAccountId": "gh-user-12345"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `authenticated` | bool | 新的认证状态 |
| `activeAccountId` | string\|null | 当前活跃账号 ID |

**触发场景：**
- `auth_complete` 成功后
- `auth_disconnect` 后
- `auth_activate` 切换账号后
- Token 后台刷新失败导致认证过期

**通知内容安全：** notification 不携带 token、scope 明细等敏感信息。Client 收到后调用 `auth_status` 获取完整状态。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `github/auth_changed` | `github/auth_status` | 完整认证状态（含多账号、scope、gh CLI 状态） |

> Client 重连后调用 `auth_status` 获取完整快照。若认证状态在断连期间变化（如 token 过期），Client 通过 `auth_changed` + resync 感知。

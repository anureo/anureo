# Auto-Review（自动代码审查）

> 命名空间: `_anureo.dev/auto-review/*`
> Capability key: `auto-review`

## Capability

```json
{
  "auto-review": {
    "start": true,
    "stop": true,
    "status": true
  }
}
```

- 声明 `auto-review` capability 后，client 可以为 session 启动/停止后台代码审查、查询审查状态。
- Auto-Review 在 session turn 完成后自动触发代码审查，结果以结构化形式返回。
- Auto-Review **不修改原始 session 的 message 流**——审查结果是独立的。

### 审查流程

```
Session turn completed (prompt response returned)
  → Auto-Review watcher detects turn completion
  → Collect changed files (from session tool calls: edit, write, etc.)
  → Invoke review generation (may use small model or dedicated review model)
  → Produce structured review:
      ├── file-level inline comments
      ├── summary
      └── severity (info / warning / error / critical)
  → Result stored as:
      ├── independent review session (recommended), OR
      └── session metadata: metadata.anureo.review
  → Emit _anureo.dev/auto-review/result notification
```

- Auto-Review 触发时机：session 的 generation 完成（`stopReason` 非 `cancelled`）。
- 审查范围：当前 turn 中通过工具调用修改的文件（edit/write 操作的文件）。
- 如果 turn 中没有文件修改，不触发 review。
- Review 结果产出后，client 可以选择在原始 session UI 中显示 inline comments，或打开独立 review session 查看完整审查报告。

### 结果存储策略

| 模式 | 说明 | 适用场景 |
|---|---|---|
| 独立 review session | 创建新的 ACP session 存储完整审查报告 | 大型审查、需要对话式跟进 |
| Session metadata | 结果摘要写入 `metadata.anureo.review` | 轻量审查、快速反馈 |

- 默认策略由 server 配置决定；client 可以通过 `auto-review/start` 的参数覆盖。
- 两种模式可以共存——metadata 中始终写入摘要，同时可选创建独立 review session。

---

## Methods

### `_anureo.dev/auto-review/start`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `auto-review.start` |
| 权限 | Server-side authorization（需要写权限 scope） |
| 进度 | 长时操作，支持 `_anureo.dev/auto-review/progress` notification（`08-cross-cutting-patterns.md` §3） |
| 幂等 | 对同一 session 的重复 start 是 no-op（返回当前状态） |

**Request:**

```json
{
  "sessionId": "sess_abc123",
  "options": {
    "trigger": "on_turn_complete",
    "createReviewSession": true,
    "severityFilter": ["warning", "error", "critical"],
    "focusAreas": ["security", "performance", "readability"],
    "model": "claude-sonnet-4-20250514"
  }
}
```

**Response:**

```json
{
  "sessionId": "sess_abc123",
  "active": true,
  "trigger": "on_turn_complete",
  "message": "Auto-review enabled. Will review code changes after each turn."
}
```

**Rust 类型:**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct AutoReviewStartRequest {
    pub session_id: String,
    #[serde(default)]
    pub options: AutoReviewOptions,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct AutoReviewOptions {
    /// "on_turn_complete" (default) | "manual_only"
    #[serde(default = "default_trigger")]
    pub trigger: String,
    /// When true, create independent review session for full report
    #[serde(default)]
    pub create_review_session: bool,
    /// Only report severities in this list; null = all
    #[serde(default)]
    pub severity_filter: Option<Vec<ReviewSeverity>>,
    /// Focus areas for review
    #[serde(default)]
    pub focus_areas: Vec<String>,
    /// Override review model
    #[serde(default)]
    pub model: Option<String>,
}

fn default_trigger() -> String { "on_turn_complete".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ReviewSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct AutoReviewStartResponse {
    pub session_id: String,
    pub active: bool,
    pub trigger: String,
    pub message: String,
}
```

**逻辑说明:**

1. Server 为指定 session 注册 auto-review watcher。
2. `trigger: "on_turn_complete"`（默认）：每次 session turn 完成后自动触发 review。
3. `trigger: "manual_only"`：只注册 watcher，review 由其他机制手动触发（如 client 调用内部 API）。
4. `createReviewSession: true` 时，review 结果会创建独立的 ACP session 存储完整报告。
5. `severityFilter` 过滤只报告特定 severity 的问题（默认全部报告）。
6. `focusAreas` 引导 review 关注特定维度（security、performance、readability 等），非强制约束。
7. `model` 可选，覆盖默认的 review 模型；为空时使用 server 配置的默认 review model。
8. 重复 `start` 同一 session 返回当前活跃状态，不报错。
9. Auto-review watcher 在 session 关闭后自动停止。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `auto-review.start` | initialize 未声明 |
| `forbidden` | 无权限 | server authorization 拒绝 |
| `not_found` | Session 不存在 | `sessionId` 无效 |
| `invalid_params` | 参数校验失败 | `trigger` 不合法、`severityFilter` 包含无效值 |
| `internal_error` | Server 内部错误 | Watcher 注册失败 |

---

### `_anureo.dev/auto-review/stop`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `auto-review.stop` |
| 权限 | Server-side authorization（需要写权限 scope） |
| 幂等 | 是——停止未启用的 auto-review 是 no-op |

**Request:**

```json
{
  "sessionId": "sess_abc123"
}
```

**Response:**

```json
{
  "sessionId": "sess_abc123",
  "active": false,
  "message": "Auto-review disabled."
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct AutoReviewStopRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AutoReviewStopResponse {
    pub session_id: String,
    pub active: bool,
    pub message: String,
}
```

**逻辑说明:**

1. Server 注销指定 session 的 auto-review watcher。
2. 如果有正在进行的 review 生成，设置为 cancelled 状态。
3. 已完成的历史 review 结果保留——`stop` 只影响未来的自动触发。
4. 重复 `stop` 未启用的 session 是 no-op，返回 `active: false`。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `auto-review.stop` | initialize 未声明 |
| `not_found` | Session 不存在 | `sessionId` 无效 |
| `forbidden` | 无权限 | server authorization 拒绝 |
| `internal_error` | Server 内部错误 | Watcher 注销失败 |

---

### `_anureo.dev/auto-review/status`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `auto-review.status` |
| 权限 | Server-side authorization（已认证连接即可） |

**Request:**

```json
{
  "sessionId": "sess_abc123"
}
```

**Response:**

```json
{
  "sessionId": "sess_abc123",
  "active": true,
  "trigger": "on_turn_complete",
  "lastReview": {
    "reviewId": "rev_001",
    "reviewedTurnIndex": 3,
    "reviewSessionId": "sess_review_001",
    "status": "completed",
    "severity": {
      "critical": 0,
      "error": 1,
      "warning": 3,
      "info": 2
    },
    "filesReviewed": ["src/auth.rs", "src/session.rs"],
    "summary": "Password verification correctly uses bcrypt, but error handling in session.rs leaks internal details. 3 minor style issues.",
    "generatedAt": "2025-08-19T10:02:00Z"
  },
  "pendingReview": null
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct AutoReviewStatusRequest {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AutoReviewStatusResponse {
    pub session_id: String,
    pub active: bool,
    pub trigger: String,
    pub last_review: Option<ReviewSummary>,
    pub pending_review: Option<PendingReview>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewSummary {
    pub review_id: String,
    pub reviewed_turn_index: u32,
    pub review_session_id: Option<String>,
    pub status: ReviewStatus,
    pub severity: SeverityCounts,
    pub files_reviewed: Vec<String>,
    pub summary: String,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SeverityCounts {
    pub critical: u32,
    pub error: u32,
    pub warning: u32,
    pub info: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingReview {
    pub turn_index: u32,
    pub status: ReviewStatus,
    pub started_at: chrono::DateTime<chrono::Utc>,
}
```

**逻辑说明:**

1. 返回 auto-review 的当前状态和最近一次 review 结果摘要。
2. `lastReview` 包含最近一次完成的 review 的结构化摘要。
3. `reviewSessionId` 如果非空，client 可以通过 `session/load` 加载完整 review 报告。
4. `severity` 为按严重程度统计的问题数量。
5. `pendingReview` 如果非空，表示有正在进行的 review 生成。
6. `status` 反映 review 生成的生命周期状态。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `auto-review.status` | initialize 未声明 |
| `not_found` | Session 不存在 | `sessionId` 无效 |
| `internal_error` | Server 内部错误 | 状态读取失败 |

---

## Notifications

### `_anureo.dev/auto-review/result`

| 项目 | 内容 |
|---|---|
| 方向 | Server → Client notification |
| 触发 | Review 生成完成（无论是否有问题） |

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/auto-review/result",
  "params": {
    "sessionId": "sess_abc123",
    "reviewId": "rev_001",
    "reviewedTurnIndex": 3,
    "reviewSessionId": "sess_review_001",
    "status": "completed",
    "severity": {
      "critical": 0,
      "error": 1,
      "warning": 3,
      "info": 2
    },
    "filesReviewed": ["src/auth.rs", "src/session.rs"],
    "inlineComments": [
      {
        "file": "src/session.rs",
        "line": 42,
        "severity": "error",
        "rule": "security/information-disclosure",
        "message": "Error message exposes internal path structure. Consider using a generic error message for users.",
        "suggestion": "return Err(AuthError::InvalidCredentials)"
      },
      {
        "file": "src/auth.rs",
        "line": 78,
        "severity": "warning",
        "rule": "readability/naming",
        "message": "Variable name 'x' is too short for its scope.",
        "suggestion": "Rename to 'hashed_password'"
      }
    ],
    "summary": "Password verification correctly uses bcrypt, but error handling in session.rs leaks internal details. 3 minor style issues.",
    "generatedAt": "2025-08-19T10:02:00Z"
  }
}
```

**params 字段:**

| 字段 | 类型 | 说明 |
|---|---|---|
| `sessionId` | string | 被审查的 session ID |
| `reviewId` | string | 本次 review 的唯一 ID |
| `reviewedTurnIndex` | number | 被审查的 turn 索引 |
| `reviewSessionId` | string? | 独立 review session ID（如有） |
| `status` | string | Review 状态 |
| `severity` | object | 按严重程度的问题统计 |
| `filesReviewed` | string[] | 被审查的文件列表 |
| `inlineComments` | object[] | 文件级 inline 评论列表 |
| `summary` | string | 总体审查总结 |
| `generatedAt` | string | Review 完成时间 |

**Rust 类型:**

```rust
#[derive(Debug, Clone, Serialize)]
pub struct AutoReviewResultParams {
    pub session_id: String,
    pub review_id: String,
    pub reviewed_turn_index: u32,
    pub review_session_id: Option<String>,
    pub status: ReviewStatus,
    pub severity: SeverityCounts,
    pub files_reviewed: Vec<String>,
    pub inline_comments: Vec<InlineComment>,
    pub summary: String,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InlineComment {
    pub file: String,
    pub line: u32,
    pub severity: ReviewSeverity,
    pub rule: String,
    pub message: String,
    pub suggestion: Option<String>,
}
```

**逻辑说明:**

1. Review 完成后发送此 notification，包含完整的结构化审查结果。
2. `inlineComments` 为文件级评论，每条包含文件路径、行号、severity、规则名、描述和修复建议。
3. Notification 携带完整结果——client 收到后可直接展示，不一定需要再调用 `auto-review/status`。
4. 同时，review 结果摘要写入 session metadata `metadata.anureo.review`（`08-cross-cutting-patterns.md` §5），通过标准 `session/update` 传播。
5. 如果 `createReviewSession: true`，`reviewSessionId` 非空——client 可以 `session/load` 加载完整审查报告 session。
6. **Auto-Review 结果不修改原始 session 的 message 流**——审查意见只出现在 notification、session metadata 或独立 review session 中。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `auto-review/result` | `auto-review/status` | 当前 review 状态和最近一次结果 |

- Client 重连后必须调用 `auto-review/status` 获取完整 review 状态快照。
- `lastReview` 包含最近一次 review 的摘要，足以重建 UI 展示。
- 如果需要完整 inline comments，client 可以通过 `reviewSessionId` 调用 `session/load` 加载独立 review session。
- 如果 `auto-review/status` 调用失败，client 必须保留旧状态（显示 stale 指示），不能当作无 review 处理。

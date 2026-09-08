# Multi-Run（批量会话执行）

> 命名空间: `_anureo.dev/multi-run/*`
> Capability key: `multi-run`

## Capability

```json
{
  "multi-run": {
    "create": true,
    "cancel": true,
    "status": true
  }
}
```

- 声明 `multi-run` capability 后，client 可以创建批量执行任务、取消任务、查询进度。
- Multi-Run 是协调层：管理多个 session 的 prompt 发送和结果收集，**不改变单个 session 的 ACP 语义**。
- 每个 sub-session 仍遵循标准生命周期：`session/new` → `session/prompt` → `session/update` → prompt response。

### 设计原则

```
Multi-Run coordinator (server-side)
├── session A: session/new → session/prompt → session/update → done
├── session B: session/new → session/prompt → session/update → done
├── session C: session/new → session/prompt → session/update → error
└── aggregate result → multi-run/status + multi-run/changed
```

- Multi-Run 不创建独立的 message protocol——sub-session 的所有 `session/update` 仍然走标准 ACP 流。
- Multi-Run coordinator 负责创建 sub-session、发送 prompt、收集结果、上报进度。
- Client 可以通过标准 `session/load` 加载任一 sub-session 查看完整对话。

---

## Methods

### `_anureo.dev/multi-run/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `multi-run.create` |
| 权限 | Server-side authorization（需要写权限 scope） |
| 进度 | 长时操作，支持 `_anureo.dev/multi-run/progress` notification（`08-cross-cutting-patterns.md` §3） |
| 幂等 | 支持 `idempotencyKey` |

**Request:**

```json
{
  "name": "Compare refactor approaches",
  "runs": [
    {
      "label": "approach-a",
      "prompt": {
        "text": "Refactor src/auth.rs to use trait objects"
      },
      "config": {
        "mode": "code",
        "model": "claude-sonnet-4-20250514"
      }
    },
    {
      "label": "approach-b",
      "prompt": {
        "text": "Refactor src/auth.rs to use enum dispatch"
      },
      "config": {
        "mode": "code",
        "model": "claude-sonnet-4-20250514"
      }
    }
  ],
  "concurrency": 2,
  "stopOnError": false,
  "idempotencyKey": "multirun-2025-08-19-001"
}
```

**Response:**

```json
{
  "id": "mr_xyz789",
  "name": "Compare refactor approaches",
  "status": "running",
  "totalRuns": 2,
  "completedRuns": 0,
  "failedRuns": 0,
  "sessionIds": ["sess_001", "sess_002"],
  "createdAt": "2025-08-19T10:00:00Z"
}
```

**Rust 类型:**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct MultiRunCreateRequest {
    pub name: String,
    pub runs: Vec<MultiRunEntry>,
    #[serde(default = "default_concurrency")]
    pub concurrency: u8,
    #[serde(default)]
    pub stop_on_error: bool,
    pub idempotency_key: Option<String>,
}

fn default_concurrency() -> u8 { 1 }

#[derive(Debug, Clone, Deserialize)]
pub struct MultiRunEntry {
    pub label: String,
    pub prompt: PromptPayload,
    #[serde(default)]
    pub config: MultiRunSessionConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PromptPayload {
    pub text: String,
    #[serde(default)]
    pub context: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct MultiRunSessionConfig {
    #[serde(default)]
    pub mode: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MultiRunCreateResponse {
    pub id: String,
    pub name: String,
    pub status: MultiRunStatus,
    pub total_runs: u32,
    pub completed_runs: u32,
    pub failed_runs: u32,
    pub session_ids: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MultiRunStatus {
    Pending,
    Running,
    Completed,
    PartiallyCompleted,
    Cancelled,
    Failed,
}
```

**逻辑说明:**

1. Server 为每个 `run` 创建独立的 ACP session（`session/new`），session ID 记录在 `sessionIds` 中。
2. `concurrency` 控制同时执行的 prompt 数量；超出部分排队等待。
3. 每个 sub-session 的 prompt 发送遵循标准 `session/prompt` 语义——Multi-Run coordinator 内部调用，对 client 透明。
4. `stopOnError: true` 时，任一 run 失败后取消所有未开始的 run。
5. `stopOnError: false`（默认）时，即使部分 run 失败也继续执行其余 run。
6. 每个 sub-session 的 `session/update` 正常通过 ACP 流传输；Multi-Run 不拦截或修改 update。
7. Sub-session metadata 写入 `metadata.anureo.multirun`（`08-cross-cutting-patterns.md` §5），记录关联的 multi-run ID。
8. `idempotencyKey` 相同时返回已存在的 multi-run 状态。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `multi-run.create` | initialize 未声明 |
| `forbidden` | 无权限创建 | server authorization 拒绝 |
| `invalid_params` | 参数校验失败 | `runs` 为空、`concurrency` 为 0、`prompt.text` 为空 |
| `conflict` | 已有同 idempotencyKey 的活跃 multi-run | 幂等冲突且状态不兼容 |
| `internal_error` | Server 内部错误 | Sub-session 创建失败 |

---

### `_anureo.dev/multi-run/cancel`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `multi-run.cancel` |
| 权限 | Server-side authorization（需要写权限 scope） |
| 幂等 | 是——取消已完成的 multi-run 是 no-op |

**Request:**

```json
{
  "id": "mr_xyz789"
}
```

**Response:**

```json
{
  "id": "mr_xyz789",
  "status": "cancelled",
  "cancelledRuns": 1,
  "completedRuns": 1
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct MultiRunCancelRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MultiRunCancelResponse {
    pub id: String,
    pub status: MultiRunStatus,
    pub cancelled_runs: u32,
    pub completed_runs: u32,
}
```

**逻辑说明:**

1. Server 对所有未完成的 sub-session 执行 `session/cancel`。
2. 已完成的 sub-session 不受影响——其结果和 session 历史保留。
3. 正在执行 prompt 的 sub-session 设置 cancellation flag，generation 最终返回 `cancelled`。
4. 排队中未开始的 run 直接标记为 cancelled。
5. Cancel 操作完成后发送 `multi-run/changed` 通知。
6. 重复 cancel 已 cancelled 的 multi-run 是 no-op。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `multi-run.cancel` | initialize 未声明 |
| `not_found` | Multi-run 不存在 | `id` 不存在 |
| `forbidden` | 无权限取消 | server authorization 拒绝 |
| `internal_error` | Server 内部错误 | 取消过程异常 |

---

### `_anureo.dev/multi-run/status`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `multi-run.status` |
| 权限 | Server-side authorization（已认证连接即可） |
| 分页 | 支持标准 cursor 分页（`08-cross-cutting-patterns.md` §1），对 `runs` 数组分页 |

**Request:**

```json
{
  "id": "mr_xyz789",
  "cursor": null,
  "limit": 50
}
```

**Response:**

```json
{
  "id": "mr_xyz789",
  "name": "Compare refactor approaches",
  "status": "partially_completed",
  "totalRuns": 2,
  "completedRuns": 1,
  "failedRuns": 1,
  "createdAt": "2025-08-19T10:00:00Z",
  "completedAt": "2025-08-19T10:02:30Z",
  "runs": [
    {
      "label": "approach-a",
      "sessionId": "sess_001",
      "status": "completed",
      "stopReason": "end_turn",
      "error": null,
      "startedAt": "2025-08-19T10:00:01Z",
      "completedAt": "2025-08-19T10:01:15Z"
    },
    {
      "label": "approach-b",
      "sessionId": "sess_002",
      "status": "failed",
      "stopReason": null,
      "error": {
        "code": "internal_error",
        "message": "Provider returned 429 rate limit"
      },
      "startedAt": "2025-08-19T10:00:01Z",
      "completedAt": "2025-08-19T10:00:30Z"
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct MultiRunStatusRequest {
    pub id: String,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: u32,
}

fn default_limit() -> u32 { 50 }

#[derive(Debug, Clone, Serialize)]
pub struct MultiRunStatusResponse {
    pub id: String,
    pub name: String,
    pub status: MultiRunStatus,
    pub total_runs: u32,
    pub completed_runs: u32,
    pub failed_runs: u32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub runs: Vec<MultiRunRunStatus>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct MultiRunRunStatus {
    pub label: String,
    pub session_id: String,
    pub status: MultiRunStatus,
    pub stop_reason: Option<String>,
    pub error: Option<MultiRunError>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MultiRunError {
    pub code: String,
    pub message: String,
}
```

**逻辑说明:**

1. 返回完整的 multi-run 状态和每个 run 的详细执行状态。
2. 每个 run 的 `status` 映射到对应 sub-session 的 generation 状态。
3. `stopReason` 来自 sub-session prompt response 的 `stopReason`（如 `end_turn`、`cancelled`）。
4. `completedAt` 在所有 run 完成或取消后才设置。
5. `status` 聚合规则：
   - 全部 completed → `completed`
   - 部分 completed + 部分 failed → `partially_completed`
   - 全部 failed → `failed`
   - 被 cancel → `cancelled`
6. Client 可以通过 `sessionId` 调用 `session/load` 查看任一 sub-session 的完整对话。
7. 当 `runs` 数量超过 `limit` 时分页返回。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `multi-run.status` | initialize 未声明 |
| `not_found` | Multi-run 不存在 | `id` 不存在 |
| `internal_error` | Server 内部错误 | 状态读取失败 |

---

## Notifications

### `_anureo.dev/multi-run/changed`

| 项目 | 内容 |
|---|---|
| 方向 | Server → Client notification |
| 触发 | Run 完成、run 失败、全部完成、被取消 |

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/multi-run/changed",
  "params": {
    "id": "mr_xyz789",
    "status": "partially_completed",
    "completedRuns": 1,
    "failedRuns": 1,
    "totalRuns": 2,
    "lastRunLabel": "approach-b",
    "lastRunStatus": "failed"
  }
}
```

**params 字段:**

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | Multi-run ID |
| `status` | string | 当前聚合状态 |
| `completedRuns` | number | 已完成的 run 数 |
| `failedRuns` | number | 失败的 run 数 |
| `totalRuns` | number | 总 run 数 |
| `lastRunLabel` | string? | 最近变化的 run label |
| `lastRunStatus` | string? | 最近变化的 run 状态 |

**逻辑说明:**

1. 每次 run 完成或失败时发送。
2. 不携带完整 run 列表——client 收到后调用 `multi-run/status` 获取详情。
3. 多个 client 连接同一 server 时，所有 client 都会收到 notification。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `multi-run/changed` | `multi-run/status` | 完整执行状态（含所有 run 详情） |

- Client 重连后必须调用 `multi-run/status` 获取完整状态快照。
- 如果 `multi-run/status` 调用失败，client 必须保留旧状态（显示 stale 指示），不能当作空集合处理。

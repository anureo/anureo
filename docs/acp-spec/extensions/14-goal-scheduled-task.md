# Goal 和 Scheduled Task

> 命名空间: `_anureo.dev/goal/*`、`_anureo.dev/scheduled-task/*`
> Capability key: `goal`、`scheduled-task`
> **实施注记（2026-09-07，goal-codex-alignment P5b）**：goal 后端已切至 `thread_goals` 表（`<anureo_home>/tasks/tasks.db`），goals.json 文件后端退役。六方法 API 面与响应形状保持兼容；新增 `_anureo.dev/goal/updated` 全量快照通知（与 `goal/changed` 并存双发，前者待 FE 迁移后收回）；mutation 响应新增 `updated` 字段（全量快照）。状态投影兼容：新 `usage_limited`→旧 `paused`，`blocked`/`budget_limited`→旧 `failed`（详情在 `metadata.statusReason`）；`sessionIds` 投影 thread_id；`progress`/`steps` 恒空。
> **实施注记（2026-09-08，goal-codex-alignment P8）**：本扩展降级为**不广播的 legacy alias**（capability 不再出现在协商中，六方法仍可用，映射同一 GoalService）。对外权威面 = **中立 goal 扩展**（见下节「中立 goal 面（P8）」，对齐 codex-acp provider-neutral goal extension）。P7 objective 文件化：objective 超过内联上限时 DB 存 `@file:<name>` 标记、文本落 `<anureo_home>/goals/<sessionId>.md`；本扩展六方法的读取面（get/list/start/pause/resume/cancel）经 `resolve_objective` 返回**全文**。

---

# 中立 goal 面（P8，2026-09-08 落地）

> 权威基准：codex-acp `docs/goal-extension.md`（provider-neutral）；规范细节与状态映射表见 `docs/goal/goal-codex-alignment.md` 附录 C。

## Capability 协商

`initialize` 响应**顶层 `_meta.goal`**（codex-acp / 客户端识别位置），`agentCapabilities._meta.goal` 同形保留：

```json
{
  "goal": {
    "version": 1,
    "controlMethod": "_session/goal",
    "actions": ["set", "pause", "resume", "clear"]
  }
}
```

客户端按 `version===1 && controlMethod==='_session/goal'` 识别；`show`/`edit` 为 anureo 扩展动作，不进 `actions`。

## 控制方法 `_session/goal`

请求 `{sessionId, action}`：

| action | 参数 | 语义 |
|---|---|---|
| `set` | `objective`（非空，可带 `tokenBudget`） | 置位/快照替换；替换既有 goal 时宿主写 deferral（§6.6） |
| `pause` | — | active → paused |
| `resume` | — | 仅 paused/blocked/usage_limited 可恢复（budget_limited/终态拒绝） |
| `clear` | — | 任意状态 → 无 goal |

未广播的 action 一律拒绝（JSON-RPC error）。

## 快照发布 `session_info_update._meta.goal`

goal 每次变更（控制动作 / `/goal` 命令 / turn 完成钩子）经 `session/update` 的 `session_info_update` 携带全量快照；**清除时 `goal: null`**。字段（camelCase，Unix **毫秒**）：

| 字段 | 说明 |
|---|---|
| `objective` | 全文；文件化 goal（P7）以 `objectiveFile: true` 代替（文本在 `<anureo_home>/goals/<sessionId>.md`，FE 按需拉取） |
| `status` | 5 态投影：`active` / `paused` / `blocked` / `complete` / `limited`（`usage_limited` 与 `budget_limited` 归并；原始成因在可选 `statusReason`） |
| `createdAt` / `updatedAt` | Unix 毫秒 |
| `tokenBudget` / `tokensUsed` / `timeUsedSeconds` | 预算与记账 |
| `controlMethod` | 恒 `"_session/goal"` |

可选扩展（未实现，预留）：`iterationCount`、`lastContinuationReason`。

## 生命周期解耦

goal `active` ≠ prompt 运行中。turn 完成钩子驱动 `continue_if_idle` 自主续跑，产生的 `session/update` 在已完成的 prompt 请求之外发布；deferral（§6.6：goal 快照替换 / session fork 保护窗）推迟续跑到下一 turn 边界。

## 设计原则

- **Goal 持久化由 server 所有**：Goal 的持久化状态由 server 管理，ACP connection 断开**不能**停止 goal。Client 断线重连后通过 `goal/list` 恢复 goal 状态。
- **Goal 运行独立于 session**：Goal 可跨 session 存在，一个 goal 可以驱动多个 session 的 prompt。
- **Scheduled Task 需要独立执行锁**：Scheduled Task 需要独立的执行锁、重试、幂等键和运行记录。

---

# Goal

> 命名空间: `_anureo.dev/goal/*`
> Capability key: `goal`

## Capability

> **P8（2026-09-08）起不再广播**：legacy alias 保持可用但不进能力协商；客户端改用上文 `agentCapabilities._meta.goal`。以下为历史形状（保留备查）：

```json
{
  "goal": {
    "list": true,
    "get": true,
    "start": true,
    "pause": true,
    "resume": true,
    "cancel": true
  }
}
```

## Rust 类型

```rust
pub struct Goal {
    /// Goal 唯一标识
    pub id: String,
    /// Goal 标题
    pub title: String,
    /// 目标描述
    pub description: String,
    /// 当前状态
    pub status: GoalStatus,
    /// 创建时间
    pub created_at: String,
    /// 最后更新时间
    pub updated_at: String,
    /// 关联的 session ID 列表
    pub session_ids: Vec<String>,
    /// 进度摘要
    pub progress: Option<GoalProgress>,
    /// Goal metadata (存储在 session metadata.anureo.goal)
    pub metadata: Option<serde_json::Value>,
}

pub enum GoalStatus {
    /// 已创建但未启动
    Pending,
    /// 运行中
    Active,
    /// 已暂停
    Paused,
    /// 已完成
    Completed,
    /// 已取消
    Cancelled,
    /// 失败
    Failed,
}

pub struct GoalProgress {
    /// 已完成步骤数
    pub completed_steps: u32,
    /// 总步骤数
    pub total_steps: u32,
    /// 百分比 (0-100)
    pub percentage: u32,
    /// 当前步骤描述
    pub current_step: Option<String>,
    /// 已产生的 session 数量
    pub sessions_spawned: u32,
}

pub struct GoalStartParams {
    /// 目标描述
    pub title: String,
    pub description: String,
    /// 初始 session ID（可选）
    pub session_id: Option<String>,
    /// 工作目录
    pub working_directory: Option<String>,
    /// 幂等键
    pub idempotency_key: Option<String>,
}

pub struct GoalListParams {
    /// 状态筛选
    pub status: Option<GoalStatus>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}
```

## Methods

---

### `_anureo.dev/goal/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `goal.list` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "status": null,
  "cursor": null,
  "limit": 50
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `status` | string? | 状态筛选：`pending` / `active` / `paused` / `completed` / `cancelled` / `failed`；省略返回全部 |
| `cursor` | string? | 分页游标 |
| `limit` | number? | 每页数量 |

**Response:**

```json
{
  "items": [
    {
      "id": "goal-001",
      "title": "Refactor authentication module",
      "description": "Extract auth logic into dedicated module with proper tests",
      "status": "active",
      "createdAt": "2025-08-19T10:00:00Z",
      "updatedAt": "2025-08-19T10:30:00Z",
      "sessionIds": ["sess-001", "sess-002"],
      "progress": {
        "completedSteps": 3,
        "totalSteps": 7,
        "percentage": 42,
        "currentStep": "Writing integration tests for new auth flow",
        "sessionsSpawned": 2
      },
      "metadata": null
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- Goal 持久化在 server 端，独立于 ACP connection。
- Client 断线重连后必须调用此方法获取完整 goal 列表。
- `status` 筛选用于只显示 active 或 completed 的 goal。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | 读取 goal 存储失败 |

---

### `_anureo.dev/goal/get`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `goal.get` |
| 权限 | Server policy（只读） |

**Request:**

```json
{
  "id": "goal-001"
}
```

**Response:**

```json
{
  "id": "goal-001",
  "title": "Refactor authentication module",
  "description": "Extract auth logic into dedicated module with proper tests",
  "status": "active",
  "createdAt": "2025-08-19T10:00:00Z",
  "updatedAt": "2025-08-19T10:30:00Z",
  "sessionIds": ["sess-001", "sess-002"],
  "progress": {
    "completedSteps": 3,
    "totalSteps": 7,
    "percentage": 42,
    "currentStep": "Writing integration tests for new auth flow",
    "sessionsSpawned": 2
  },
  "metadata": null,
  "steps": [
    { "index": 0, "description": "Analyze existing auth code", "status": "completed" },
    { "index": 1, "description": "Design new module structure", "status": "completed" },
    { "index": 2, "description": "Extract auth service", "status": "completed" },
    { "index": 3, "description": "Write integration tests", "status": "in_progress" },
    { "index": 4, "description": "Update documentation", "status": "pending" }
  ]
}
```

**逻辑说明:**
- 返回 goal 详细信息，包含 step 级别的进度。
- `steps` 数组描述 goal 的执行计划（由 server/agent 规划生成）。
- step status：`pending` / `in_progress` / `completed` / `skipped` / `failed`。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | goal 不存在 |

---

### `_anureo.dev/goal/start`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `goal.start` |
| 权限 | Server policy（scope: `goal:write`） |

**Request:**

```json
{
  "title": "Refactor authentication module",
  "description": "Extract auth logic into dedicated module with proper tests",
  "sessionId": null,
  "workingDirectory": "/home/user/project",
  "idempotencyKey": "goal-2025-08-19-auth-refactor"
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `title` | string | Goal 标题 |
| `description` | string | 目标描述 |
| `sessionId` | string? | 初始 session ID；省略时 server 创建新 session |
| `workingDirectory` | string? | 工作目录；省略时使用当前 session 工作目录 |
| `idempotencyKey` | string? | 幂等键（防止重复创建） |

**Response:**

```json
{
  "id": "goal-001",
  "title": "Refactor authentication module",
  "status": "active",
  "sessionId": "sess-001",
  "createdAt": "2025-08-19T10:00:00Z"
}
```

**逻辑说明:**
- 创建并启动 goal。Server 创建持久化 goal 记录，开始驱动 agent 执行。
- Goal 启动后独立于 client connection 运行。即使 client 断开，goal 继续。
- `sessionId` 如果提供，server 将 goal 绑定到该 session。如果省略，server 创建新 session。
- 幂等键：同一 `idempotencyKey` 在短时间内重复请求返回已有 goal。

**Error:**

| kind | 触发条件 |
|---|---|
| `invalid_params` | title 或 description 为空 |
| `forbidden` | 无 `goal:write` scope |
| `internal_error` | goal 创建失败 |

---

### `_anureo.dev/goal/pause`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `goal.pause` |
| 权限 | Server policy（scope: `goal:write`） |

**Request:**

```json
{
  "id": "goal-001"
}
```

**Response:**

```json
{
  "id": "goal-001",
  "status": "paused",
  "pausedAt": "2025-08-19T10:30:00Z"
}
```

**逻辑说明:**
- 暂停 goal 执行。当前正在执行的 session prompt 会完成，但不再发起新的 prompt。
- Goal 状态变为 `paused`，持久化保存。
- 暂停后可以通过 `resume` 恢复。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | goal 不存在 |
| `invalid_params` | goal 状态不是 `active`（无法暂停） |
| `forbidden` | 无 `goal:write` scope |

---

### `_anureo.dev/goal/resume`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `goal.resume` |
| 权限 | Server policy（scope: `goal:write`） |

**Request:**

```json
{
  "id": "goal-001"
}
```

**Response:**

```json
{
  "id": "goal-001",
  "status": "active",
  "resumedAt": "2025-08-19T11:00:00Z"
}
```

**逻辑说明:**
- 恢复已暂停的 goal。
- Server 从上次暂停的位置继续执行。
- 只有 `paused` 状态的 goal 可以 resume。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | goal 不存在 |
| `invalid_params` | goal 状态不是 `paused` |
| `forbidden` | 无 `goal:write` scope |

---

### `_anureo.dev/goal/cancel`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `goal.cancel` |
| 权限 | Server policy（scope: `goal:write`） |

**Request:**

```json
{
  "id": "goal-001",
  "reason": "User cancelled"
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | Goal ID |
| `reason` | string? | 取消原因 |

**Response:**

```json
{
  "id": "goal-001",
  "status": "cancelled",
  "cancelledAt": "2025-08-19T11:30:00Z"
}
```

**逻辑说明:**
- 取消 goal。当前正在执行的 session prompt 会被 `session/cancel` 中断。
- Goal 状态变为 `cancelled`，持久化保存。
- 取消后的 goal 不能 resume，只能重新 start 新 goal。
- 取消操作幂等：对已 cancelled 的 goal 再次 cancel 不报错。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | goal 不存在 |
| `forbidden` | 无 `goal:write` scope |

---

## Notifications

### `_anureo.dev/goal/changed`

当 goal 状态发生变化（启动、暂停、恢复、取消、进度更新、step 完成）时推送。

> P5b 注记：与新 `goal/updated` 并存双发（兼容保留）；FE 迁移后随 P7 收回。

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/goal/changed",
  "params": {
    "id": "goal-001",
    "change": "progress",
    "status": "active",
    "progress": {
      "completedSteps": 4,
      "totalSteps": 7,
      "percentage": 57,
      "currentStep": "Updating documentation",
      "sessionsSpawned": 2
    }
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | Goal ID |
| `change` | string | `started` / `paused` / `resumed` / `cancelled` / `completed` / `progress` / `failed` |
| `status` | string | 当前 goal 状态 |
| `progress` | object? | 进度信息（当 `change = "progress"` 时包含） |

- notification 丢失后，client 必须调用 `goal/list` 获取完整列表。

### `_anureo.dev/goal/updated`（P5b 新增）

每次 goal 变更后推送的**全量快照**（与 `_anureo.dev/goal/get` 响应同构）：UI 订阅快照而非增量猜状态。payload 即 Goal 对象（camelCase，含 `metadata.threadId`/`tokensUsed`/`tokenBudget`/`statusReason`）。此外每个 mutation RPC 响应也携带 `updated` 字段（同一快照），同步路径无需等通知。

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/goal/updated",
  "params": {
    "id": "goal-001",
    "title": "...",
    "description": "...",
    "status": "active",
    "createdAt": "2026-09-07T00:00:00.000Z",
    "updatedAt": "2026-09-07T00:00:00.000Z",
    "sessionIds": ["thread-42"],
    "metadata": { "threadId": "thread-42", "tokensUsed": 1200, "timeUsedSeconds": 30 }
  }
}
```

- 广播到当前进程全部连接（best-effort）；丢失后以 `goal/get`/`goal/list` 兑底。

---

# Scheduled Task

> 命名空间: `_anureo.dev/scheduled-task/*`
> Capability key: `scheduled-task`

## Capability

```json
{
  "scheduled-task": {
    "list": true,
    "create": true,
    "update": true,
    "delete": true,
    "run": true
  }
}
```

## Rust 类型

```rust
pub struct ScheduledTask {
    /// Task 唯一标识
    pub id: String,
    /// Task 名称
    pub name: String,
    /// Task 描述
    pub description: String,
    /// 是否启用
    pub enabled: bool,
    /// Client-defined schedule object; the handler preserves its fields.
    pub schedule: serde_json::Value,
    /// Prompt/model execution configuration preserved for the scheduler.
    pub execution: serde_json::Value,
    /// Scheduler-owned state preserved for the UI.
    pub state: serde_json::Value,
    /// 上次运行时间
    pub last_run: Option<String>,
    /// 上次运行状态
    pub last_run_status: Option<TaskRunStatus>,
    /// 下次计划运行时间
    pub next_run: Option<String>,
}

pub enum TaskRunStatus {
    Success,
    Failed,
    Running,
    Skipped,
}

pub struct ScheduledTaskRunParams {
    pub id: String,
    /// 幂等键（必需，防止重复触发）
    pub idempotency_key: String,
}
```

## Methods

---

### `_anureo.dev/scheduled-task/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `scheduled-task.list` |
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
      "id": "daily-review",
      "name": "Daily Code Review",
      "description": "Automatically review code changes from the last 24 hours",
      "enabled": true,
      "schedule": "0 9 * * *",
      "lastRun": "2025-08-19T09:00:00Z",
      "lastRunStatus": "success",
      "nextRun": "2025-08-20T09:00:00Z"
    },
    {
      "id": "weekly-summary",
      "name": "Weekly Summary Report",
      "description": "Generate weekly progress summary",
      "enabled": false,
      "schedule": "0 18 * * FRI",
      "lastRun": "2025-08-16T18:00:00Z",
      "lastRunStatus": "success",
      "nextRun": null
    }
  ],
  "nextCursor": null,
  "hasMore": false
}
```

**逻辑说明:**
- 返回所有已配置的 scheduled task。
- `lastRunStatus` 为上次执行结果。
- `nextRun` 为 null 表示 task disabled 或 schedule 无效。

**Error:**

| kind | 触发条件 |
|---|---|
| `internal_error` | 读取配置失败 |

### `_anureo.dev/scheduled-task/create|update|delete`

配置变更同样按项目工作目录落盘到 `<cwd>/.anureo/scheduled-tasks.json`。`create` 和 `update` 接收 `{ "task": { ... } }`，`update` 还需要 `id`；`delete` 接收 `id`。响应包含 `tasks`，创建或更新时额外包含 `task`。这些方法只负责配置和运行记录，不会在当前版本内自行启动 scheduler。

---

### `_anureo.dev/scheduled-task/run`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `scheduled-task.run` |
| 权限 | Server policy（scope: `scheduled-task:run`） |

**Request:**

```json
{
  "id": "daily-review",
  "idempotencyKey": "manual-run-2025-08-19-001"
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` | string | Scheduled task ID |
| `idempotencyKey` | string | **必需**。幂等键，防止重复触发同一任务 |

**Response:**

```json
{
  "id": "daily-review",
  "runId": "run-2025-08-19-001",
  "status": "running",
  "startedAt": "2025-08-19T10:30:00Z"
}
```

**逻辑说明:**
- 手动触发一次任务执行，不等候 schedule。
- **幂等键必需**：同一 `idempotencyKey` 在短时间内重复请求返回已有 run 结果，不重复执行。
- **执行锁**：同一 task 同时只能有一个 run 在执行。如果已有 run in-progress，返回 `invalid_params`。
- **独立执行记录**：每次 run 产生独立的执行记录，包含 start time、end time、status、output。
- **重试**：执行失败的 task 由 server 按 retry policy 自动重试（不影响手动触发）。

**Error:**

| kind | 触发条件 |
|---|---|
| `not_found` | task 不存在 |
| `invalid_params` | 缺少 `idempotencyKey`，或已有 run in-progress |
| `forbidden` | 无 `scheduled-task:run` scope，或 task disabled |
| `already_in_progress` | 同一 task 已有 run in-progress |

---

## Reconnect Resync

| Notification | Authoritative method |
|---|---|
| `_anureo.dev/goal/changed` | `_anureo.dev/goal/list` |

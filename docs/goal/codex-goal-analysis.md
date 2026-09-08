# Codex Goal 功能源码导读

> **状态**：源码分析完成（基于 OpenAI Codex `main` 快照）
>
> **源码快照**：`e3e5ad28470f6a225301518c30a66e749a880164`（2026-08-20 本地读取）
>
> **上游仓库**：[openai/codex](https://github.com/openai/codex)
>
> **用途**：解释 Codex 的持久化 goal、自动 continuation、token/time 记账、模型工具和 app-server API；本文是源码事实记录，不等同于 anureo 的实现说明。
>
> **相关 anureo 文档**：[Goal 系统工作流](./goal-system-workflow.md)、[Session Goal 集成](./session-goal-integration.md)、[Goal 用户指南](../user-guide/09-goal-task-experimental.md)

---

## 1. 一句话结论

Codex Goal 不是一个更长的 prompt，而是一个跨 turn 持久化的运行时扩展：用户或模型创建一个 thread goal 后，Codex 将其写入独立 SQLite 数据库；每次 turn、tool、token usage 和 idle 生命周期事件都会经过 goal extension；当线程空闲且 goal 仍为 `active` 时，extension 自动启动一个新的 continuation turn，直到模型明确完成、经过系统/用量限制，或被用户暂停/清除。

实现可以抽象为三条闭环：

```text
持久化闭环：app-server/TUI → GoalService → GoalStore → goals_1.sqlite
运行时闭环：thread/turn/tool hooks → GoalAccountingState → GoalStore.account_thread_goal_usage
续跑闭环：thread idle → GoalRuntimeHandle.continue_if_idle → start_turn_if_idle → continuation prompt
```

最重要的边界是：

| 问题 | Codex 的答案 |
|---|---|
| 谁定义目标 | 用户或明确要求下的模型；模型不能从普通任务自行推断要创建 goal |
| 谁能改变状态 | 模型工具只能标记 `complete`/`blocked`；用户 API 可设置 objective、pause/resume 等状态；系统可设置 `budget_limited`/`usage_limited` |
| 谁决定自动续跑 | `GoalRuntimeHandle::continue_if_idle`，且只续跑 `active` goal |
| 如何避免重复收费 | 每个 thread 一个 accounting semaphore；快照写入成功后才推进内存基线 |
| 如何避免旧写覆盖新 goal | SQL 更新带 `expected_goal_id` 条件 |
| goal 是否属于 prompt 历史 | 是。设置 goal 会写入 rollout；continuation 以 steering `ResponseItem` 启动新的 turn |

## 2. 源码目录与职责

以下路径均相对于上游仓库的 `codex-rs/`。

| 层 | 源文件 | 主要职责 |
|---|---|---|
| Extension 装配 | `ext/goal/src/extension.rs` | 注册 thread、turn、token、tool 生命周期 hooks，并暴露三个模型工具 |
| Runtime 编排 | `ext/goal/src/runtime.rs` | 串行化 goal state、外部修改、错误停止、idle continuation、steering |
| 内存记账 | `ext/goal/src/accounting.rs` | 保存当前 turn token 快照、上次记账基线、active goal 和墙钟时间 |
| 外部服务 | `ext/goal/src/api.rs` | 给 TUI/app-server 使用的 get/set/clear API；把持久化变化应用到 runtime |
| 模型工具 | `ext/goal/src/tool.rs` | `get_goal`、`create_goal`、`update_goal` 的参数校验与执行 |
| 工具 schema | `ext/goal/src/spec.rs` | Responses API tool schema 与模型可见的行为约束 |
| Prompt/steering | `ext/goal/templates/goals/*.md`、`ext/goal/src/steering.rs` | continuation、预算到达、objective 修改时的提示 |
| 状态模型 | `state/src/model/thread_goal.rs` | 状态枚举、`ThreadGoal` 领域模型、SQLite row 转换 |
| 状态存储 | `state/src/runtime/goals.rs` | CRUD、CAS 更新、原子 usage accounting、deferral |
| 数据库 | `state/goals_migrations/*.sql`、`state/src/sqlite.rs` | 独立 goals DB 和表结构 |
| app-server | `app-server/src/request_processors/thread_goal_processor.rs` | JSON-RPC set/get/clear、持久化 rollout、通知顺序 |
| TUI | `tui/src/chatwidget/goal_menu.rs`、`goal_status.rs`、`goal_display.rs`、`goal_files.rs` | `/goal` 菜单、状态栏、编辑/暂停/恢复和大 objective 文件 |
| 测试 | `ext/goal/tests/*`、`state/src/runtime/goals.rs` 内测试 | accounting、后端生命周期、并发/CAS/预算边界 |

Extension 的安装入口在 `app-server/src/extensions.rs`。配置通过 `GoalExtensionConfig { enabled, max_goal_token_budget }` 传入；feature flag 在 `features/src/lib.rs` 中定义为 `goals`。

## 3. 数据模型与存储

### 3.1 `ThreadGoal`

`state/src/model/thread_goal.rs` 定义的核心模型如下：

```rust
pub struct ThreadGoal {
    pub thread_id: ThreadId,
    pub goal_id: String,
    pub objective: String,
    pub status: ThreadGoalStatus,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

`goal_id` 是每次新建/替换时生成的 UUID。它不是 thread id：同一个 thread 的新一代 goal 会有新的 `goal_id`，这正是 stale-write protection 的版本标识。

### 3.2 状态枚举

```rust
Active,
Paused,
Blocked,
UsageLimited,
BudgetLimited,
Complete,
```

状态属性来自 `ThreadGoalStatus`：

| 状态 | 含义 | 自动 continuation | 典型控制者 |
|---|---|---:|---|
| `active` | 目标正在追踪，允许 turn 结束后续跑 | 是 | 用户、模型创建、resume |
| `paused` | 用户暂时停止 | 否 | 用户 |
| `blocked` | 当前 turn 错误或模型在严格规则下确认无法继续 | 否 | 系统或模型 |
| `usage_limited` | provider/账户用量限制阻止继续 | 否 | 系统 |
| `budget_limited` | goal 的 token budget 已达到 | 否 | 系统；模型也不能直接设置 |
| `complete` | 模型确认 objective 已完成 | 否 | 模型或用户/API |

代码中的 `is_terminal()` 只把 `budget_limited` 和 `complete` 视为 terminal；`blocked`、`usage_limited` 仍保留为可由用户恢复/处理的停止原因。

### 3.3 SQLite 表

goal 使用独立数据库文件 `goals_1.sqlite`，而非普通 thread state 表。runtime 在 `state/src/runtime.rs` 中打开该数据库，并把 `GoalStore` 作为 `StateRuntime::thread_goals()` 暴露。

```sql
CREATE TABLE thread_goals (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    objective TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'active', 'paused', 'blocked',
        'usage_limited', 'budget_limited', 'complete'
    )),
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE thread_goal_continuation_deferrals (
    thread_id TEXT PRIMARY KEY NOT NULL
        REFERENCES thread_goals(thread_id) ON DELETE CASCADE
);
```

每个 thread 同时只有一个 goal。`insert_thread_goal` 的 SQL 只在没有旧 goal，或旧 goal 为 `complete` 时插入；普通未完成 goal 不会被模型工具静默替换。外部用户/API 要替换 objective 则走 `GoalService::set_thread_goal`，更新同一条记录或调用 `replace_thread_goal`。

`replace_thread_goal_snapshot` 用于 fork/恢复类场景：它会完整覆盖 goal snapshot，并同时写入 continuation deferral，防止复制出来的 thread 在用户第一次显式操作前自动续跑。

## 4. 生命周期装配：Extension 如何接入 Agent

`GoalExtension::install_with_backend` 注册六类能力：

```rust
registry.thread_lifecycle_contributor(extension.clone());
registry.config_contributor(extension.clone());
registry.turn_lifecycle_contributor(extension.clone());
registry.token_usage_contributor(extension.clone());
registry.tool_lifecycle_contributor(extension.clone());
registry.tool_contributor(extension);
```

### 4.1 Thread start/resume/idle/stop

`on_thread_start` 做四件事：

1. 从 host config 读取 `enabled` 和最大 token budget；
2. 只有持久化 thread 且不是 review sub-agent 时才允许 goal tools；
3. 在 thread-local `ExtensionData` 中创建/复用 `GoalAccountingState` 和 `GoalRuntimeHandle`；
4. 把 runtime 注册到 `GoalService`，使 app-server 外部请求可以找到同一个 runtime。

`on_thread_resume` 调用 `restore_after_resume`。若持久化 goal 为 `active`，恢复 idle 墙钟基线并将其重新标记为 active；否则清除内存中的 active 标记。

`on_thread_idle` 调用 `continue_if_idle`。这是自动续跑的唯一主要入口：它读取数据库中的最新状态、检查 deferral 和 live thread，然后尝试 `start_turn_if_idle`。

`on_thread_stop` 从 `GoalService` 的 weak runtime registry 注销 runtime，避免外部 API 持有失效句柄。

### 4.2 Turn start/stop/abort/error

| Hook | 作用 |
|---|---|
| `on_turn_start` | 清除 continuation deferral，建立 token baseline；Plan mode 不为 goal 记 token；若 goal 为 `active` 或 `budget_limited`，把当前 turn 关联到 goal |
| `on_turn_stop` | 记账当前 active goal，再移除 turn accounting 状态 |
| `on_turn_abort` | 与 stop 类似，冲账后结束 turn；不会自行把 goal 标成 blocked |
| `on_turn_error` | 普通不可恢复错误 → `blocked`；`UsageLimitExceeded` → `usage_limited` |

错误停止时，runtime 先取得 `goal_state_lock`，再记账和更新状态；这样外部 pause/clear 与错误处理不会交错产生错误的 continuation。

### 4.3 Token usage 与 tool finish

`on_token_usage` 只更新内存中的当前累计 token snapshot，不直接写数据库。

`on_tool_finish` 对完成的 tool 或 handler 已执行的失败 tool 记为 goal progress；blocked、未执行 handler 的失败、aborted 不计为 progress。`update_goal` 本身被排除，避免“标记完成”这个动作把自己的 tool 调用再算成一段工作。

tool finish 使用 `BudgetLimitedGoalDisposition::KeepActive`：如果这一刻越过 token budget，数据库会转为 `budget_limited`，但当前 turn 不被强制打断；runtime 只向正在运行的 turn 注入一次 budget steering。turn 结束后的继续逻辑不会再自动启动新 turn。

## 5. 运行时状态与并发控制

### 5.1 `GoalRuntimeHandle`

runtime 内部保存：

```rust
struct GoalRuntimeInner {
    thread_id: ThreadId,
    state_dbs: Arc<StateRuntime>,
    thread_manager: Weak<ThreadManager>,
    accounting_state: Arc<GoalAccountingState>,
    enabled: AtomicBool,
    tools_available_for_thread: bool,
    goal_state_lock: Semaphore,
}
```

### 5.2 两把锁的职责

| 锁 | 覆盖范围 | 解决的问题 |
|---|---|---|
| `goal_state_lock`（1 permit） | 读 goal → 外部写入/状态更新 → 启动 continuation 的窗口 | 防止 idle continuation 读到旧 goal 后，用户 clear/set 再启动旧目标 |
| `progress_accounting_lock`（1 permit） | 取 snapshot → SQLite 更新成功 → 推进内存 baseline | 防止多个 tool finish/turn stop 同时消费同一 token/time delta |

`continue_if_idle` 明确持有 `goal_state_lock` 直到 `start_turn_if_idle` 返回；`GoalService::set_thread_goal` 和 `clear_thread_goal` 也在 prepare/write 窗口持有它。两把锁不要混为一谈：前者保护状态与调度，后者保护计量。

### 5.3 Continuation deferral

fork 可以将当前 goal snapshot 复制到新 thread，但同时写入 `thread_goal_continuation_deferrals`。`continue_if_idle` 发现该行就跳过自动续跑；新 thread 的下一次 `on_turn_start` 删除 deferral。这样 fork 不会在客户端尚未发出显式 turn 时自行开始工作。

## 6. Progress Accounting：token 与时间如何计算

### 6.1 内存 accounting state

```rust
struct GoalAccountingState {
    inner: Mutex<GoalAccountingInner>,
    progress_accounting_lock: Semaphore,
}

struct GoalAccountingInner {
    current_turn_id: Option<String>,
    turns: HashMap<String, GoalTurnAccounting>,
    wall_clock: GoalWallClockAccounting,
    budget_limit_reported_goal_id: Option<String>,
}
```

每个 turn 保存 `current_token_usage`、`last_accounted_token_usage`、`active_goal_id` 和 `account_tokens`。Plan mode 设置 `account_tokens = false`，因此计划讨论不会消耗 goal token budget。

### 6.2 Token 公式

每次 token usage 到达时，先计算相对上次 baseline 的字段差，再使用：

```rust
goal_tokens = (input_tokens - cached_input_tokens) + max(output_tokens, 0)
```

源码使用 saturating arithmetic。`cache_write_input_tokens`、reasoning output 等字段参与差分对象，但最终 goal 公式只把非 cached input 与 output 纳入计量。这是 goal budget accounting，不是 billing report。

当 goal 在当前 turn 中途创建或重新激活时，`mark_current_turn_goal_active` 会把 token baseline 重置到当前累计值，避免把 goal 开始前的 token 追记到新 goal。

### 6.3 墙钟时间

`GoalWallClockAccounting` 用 `Instant` 保存最近一次记账时间和 active goal id：

- goal 变为 active 时重置 baseline；
- active turn 或 idle 外部 mutation 时计算 elapsed seconds；
- pause/blocked/complete/usage-limited 时清除 active goal 并重置 baseline；
- 只在 active goal 被标记时计时，普通 idle 不计入。

因此 `time_used_seconds` 是 goal active 期间的墙钟时间，不是 thread 从创建到现在的总存活时间。

### 6.4 原子 SQL 记账

`GoalStore::account_thread_goal_usage` 在一个 `UPDATE ... RETURNING` 中完成：

```text
tokens_used += max(token_delta, 0)
time_used_seconds += max(time_delta_seconds, 0)
若当前状态允许且 tokens_used + token_delta >= token_budget：status = budget_limited
WHERE thread_id = ? AND 状态匹配 AND（可选）goal_id = expected_goal_id
```

`GoalAccountingMode` 决定允许冲账的状态：

| Mode | 允许状态 | 用途 |
|---|---|---|
| `ActiveStatusOnly` | active | 只处理仍为 active 的正常进度 |
| `ActiveOnly` | active、budget_limited | tool/turn 结束，允许记下越界前后最后一段 in-flight 使用量 |
| `ActiveOrComplete` | active、budget_limited、complete | 模型完成时，补齐完成 tool/turn 的最后使用量 |
| `ActiveOrStopped` | active、paused、blocked、usage_limited、budget_limited | 错误/停止路径补齐 in-flight 使用量 |

SQL 没有更新行时返回 `Unchanged`，内存基线只在 `Updated` 后推进。这样即使 goal 已被替换，旧 turn 产生的 usage 也不会写入新 goal。

## 7. 状态机与状态控制权

### 7.1 典型转移

```text
无 goal ──(insert/外部 set)──> active
active ──(用户 pause)───────> paused
paused ──(用户 resume/set)──> active
active ──(模型 update_goal)─> complete 或 blocked
active ──(token budget)─────> budget_limited
active ──(provider 用量)────> usage_limited
active ──(不可恢复 turn error) -> blocked
任意已存在 goal ──(clear)──> 无 goal
```

### 7.2 模型工具的非对称权限

`spec.rs` 不只是 JSON schema，也把状态机规则写进模型可见 description：

- `create_goal` 只在用户或 system/developer 明确要求时调用；普通任务不能自行创建 goal；
- `create_goal` 不能覆盖 unfinished goal；已有 goal 必须先完成，或由用户/API 修改；
- `update_goal` 只能传 `complete` 或 `blocked`；
- `blocked` 要求同一阻塞条件连续至少三次 goal turn，并且确实无法取得进展；
- 第一次遇到困难、需要澄清、工作很慢、预算快用完，都不能标成 blocked/complete；
- pause、resume、budget-limited、usage-limited 由用户或系统控制。

这里的“blocked 三次”主要是模型自律规则，工具 schema 本身不计数；状态数据库也不会因为一次 `update_goal(blocked)` 自动验证三轮，continuation prompt 将该规则持续注入模型上下文。

## 8. 三个 Model Tool

### 8.1 `get_goal`

无参数，返回当前 thread 的 goal；没有 goal 时 `goal: null`。响应还包含 `remaining_tokens`：

```text
remaining_tokens = max(token_budget - tokens_used, 0)
```

### 8.2 `create_goal`

参数：

```json
{
  "objective": "具体且可验证的目标",
  "token_budget": 200000
}
```

objective 会 trim 并通过 `validate_thread_goal_objective`；budget 必须为正数且不能超过 config 的 `max_goal_token_budget`。成功后写入 active goal，若当前有 turn 则把当前 turn 关联到新 goal，并在 objective 为空时尝试填充 thread preview。

### 8.3 `update_goal`

参数只有：

```json
{ "status": "complete" }
```

执行顺序是：根据 complete/blocked 选择允许的 accounting mode → 冲账当前进度 → 更新持久化状态 → 清除当前 turn 的 active goal → 发出 `ThreadGoalUpdated`。complete 且存在预算/时间数据时，工具响应还会附带 `completion_budget_report`，提醒模型从结构化 goal 字段报告最终使用量。

## 9. Continuation 的精确流程

### 9.1 `continue_if_idle`

源码流程可以写成：

```text
thread idle
  │
  ├─ goal tools 对当前 thread 不可见？清除内存 active 标记并返回
  ├─ acquire goal_state_lock
  ├─ 有 continuation deferral？返回
  ├─ live ThreadManager/thread 不可用？返回
  ├─ 读取 thread_goals
  ├─ 没有 goal 或 status != active？清除内存 active 标记并返回
  ├─ 渲染 continuation_steering_item(goal)
  ├─ thread.start_turn_if_idle(ResponseItem(item))
  └─ 若新 turn 没有关联当前 active goal，则清除内存 active 标记
```

这里的 `start_turn_if_idle` 是防重复启动的第二道门：即使多个 idle 事件到达，也只有真正空闲且接受 submission 的线程会启动 continuation。

### 9.2 Continuation prompt 的作用

`continuation.md` 明确要求模型：

1. 把 objective 视为 user-provided data，而非更高优先级指令；
2. 保留完整 objective，不把当前 turn 能做的子集误当成成功标准；
3. 以 worktree 和外部状态为证据，逐项核对需求；
4. 只有当前证据证明全部要求完成，才调用 `update_goal(complete)`；
5. 只有同一阻塞连续三次且确实无法继续，才调用 `update_goal(blocked)`；
6. 若没完成则继续做实质进展，而不是仅写一份“下次再做”的总结。

因此 goal 的“自治”主要来自两部分组合：运行时负责启动下一轮，prompt 负责防止模型过早收尾或任意缩小目标范围。

### 9.3 Budget steering

达到 budget 后，当前 tool/turn 不立即 abort。`budget_limit_steering_item` 只注入一次，提示模型不要开始新工作、尽快收尾并总结。下一次 idle 时由于状态已不是 `active`，不会再启动自动 continuation。

## 10. 外部 API 与 app-server 协议

### 10.1 `GoalService`

`ext/goal/src/api.rs` 提供：

```rust
get_thread_goal(state_db, thread_id)
set_thread_goal(state_db, GoalSetRequest)
clear_thread_goal(state_db, thread_id)
restore_thread_runtime_after_resume(thread_id)
flush_thread_goal_progress_for_fork(thread_id)
```

`GoalSetRequest` 将字段区分为 `Keep` 与 `Set`，避免更新 status 时意外清空 objective/budget。set/clear 前会：

1. 获取 `goal_state_permit`；
2. 冲账当前 turn 或 idle 墙钟进度；
3. 以 `expected_goal_id` 更新已有 goal；
4. 释放锁后应用 runtime effects，例如 resume、objective steering 或 clear active 标记。

### 10.2 app-server 方法

当前 app-server 暴露：

| 方法/通知 | 语义 |
|---|---|
| `thread/goal/set` | 创建或更新 materialized thread 的单个持久化 goal |
| `thread/goal/get` | 获取 goal；无 goal 返回 `goal: null` |
| `thread/goal/clear` | 删除 goal；实际删除时发 `thread/goal/cleared` |
| `thread/goal/updated` | goal 改变时通知，包含完整 goal |
| `thread/goal/cleared` | goal 删除通知 |

协议层使用 camelCase：`tokenBudget`、`tokensUsed`、`timeUsedSeconds`；SQLite 状态使用 snake_case。app-server 在 `thread_goal_processor.rs` 中负责转换。

`thread/goal/set` 的关键限制：feature `goals` 必须开启；ephemeral thread 不支持 goal；目标所属 rollout 不存在时会先 reconcile；goal-first thread materialize 时，会按顺序写入 settings snapshot 和 goal rollout item，避免恢复时缺少初始设置。

### 10.3 事件顺序

live thread 有 listener channel 时，app-server 先把 goal update/clear 排入 listener；channel 不可用才直接发送 server notification。这样 turn/item 事件与 goal 事件尽量保持 thread 内顺序，而不是让客户端看到数据库已变、rollout 还没变的逆序状态。

## 11. Fork、恢复与生命周期边界

### Fork

fork 前调用 `flush_thread_goal_progress_for_fork`，在复制 source snapshot 前冲掉尚未写入数据库的 token/time。若请求要求延迟 continuation，复制 snapshot 时写 deferral；fork 后第一轮显式 turn start 才清除。

### Resume

恢复 thread 后，app-server 发 goal snapshot；runtime 的 `restore_after_resume` 只把 active goal 重新连接到 idle accounting。paused、blocked、usage-limited、budget-limited、complete 都不会因为进程恢复自动变 active。

### Sub-agent 与 Plan mode

- review sub-agent 不暴露 goal tools；
- ephemeral thread 不支持持久化 goal；
- Plan mode 可以执行 turn，但 `account_tokens = false`，且 turn start 不会把 goal 标记为当前 active goal。

## 12. 可观测性与事件

goal extension 同时持有 `GoalAnalytics`、`GoalMetrics` 与 `GoalEventEmitter`：

- created、cleared、status changed、usage accounted 等行为写 analytics/metrics；
- 每次成功 usage accounting 或状态变化可发 `ThreadGoalUpdated`；
- 工具调用也通过 event emitter 发出带 `call_id`/`turn_id` 的 goal 更新；
- budget steering 通过 `budget_limit_reported_goal_id` 去重，目标换代后重新允许报告。

这意味着客户端不要只监听 `/goal` 命令结果；正确的 UI 应以 `thread/goal/updated` 和 `thread/goal/cleared` 为最终状态源。

## 13. 测试覆盖与阅读入口

源码测试重点不是单纯 CRUD，而是边界和并发：

| 测试区域 | 覆盖内容 |
|---|---|
| `state/src/runtime/goals.rs` | replace/insert/update、预算立即触顶、旧 goal version 忽略、并发 partial update、terminal 状态保护、多个 token delta 相加 |
| `ext/goal/tests/accounting.rs` | token 差分、cached input、Plan mode、墙钟 baseline、budget disposition |
| `ext/goal/tests/goal_extension_backend.rs` | lifecycle hooks、idle continuation、工具调用、错误停止、外部 set/clear |
| `tui/src/chatwidget/tests/goal_menu.rs` | active/paused/blocked/budget-limited 菜单与用户动作 |
| `app-server/tests/suite/v2/thread_fork.rs` | fork goal snapshot、延迟 continuation、source progress flush |
| `app-server/tests/suite/v2/thread_resume.rs` | 恢复时 goal snapshot、feature gate、预算配置、materialized thread |

建议的源码阅读顺序：

1. `state/src/model/thread_goal.rs`：先明确领域对象和状态；
2. `state/src/runtime/goals.rs`：看 SQL 的 CAS、预算和 accounting mode；
3. `ext/goal/src/accounting.rs`：看内存基线如何避免重复记账；
4. `ext/goal/src/runtime.rs`：看锁、idle continuation 和外部 mutation；
5. `ext/goal/src/extension.rs`：把所有 hooks 串起来；
6. `ext/goal/src/tool.rs`/`spec.rs`：理解模型能做什么、不能做什么；
7. `app-server/src/request_processors/thread_goal_processor.rs`：理解客户端协议和事件顺序。

## 14. 对 anureo 移植的关键启示

这些是从 Codex 源码得到的可迁移原则，不是对 anureo 当前代码的断言：

| Codex 原则 | 移植时必须保留的语义 |
|---|---|
| 持久化 goal 与 runtime 分离 | metadata/DB 是事实源；内存 runtime 只保存短期 baseline 和调度状态 |
| goal id CAS | 所有冲账和状态写回都带 generation/goal id，防止旧 turn 写到新 goal |
| 状态锁 + accounting 锁 | 调度一致性与计量一致性分别串行化，不能仅靠“通常按顺序执行” |
| `start_turn_if_idle` | continuation 必须是受 idle 条件保护的幂等提交，而不是普通异步 spawn |
| budget KeepActive steering | 达到预算时优先引导收尾，不在 tool hook 中粗暴中断当前工作 |
| deferral | fork、恢复、外部 mutation 场景要有明确的“下一次显式 turn 前不自动续跑”标记 |
| completion/blocked prompt 规则 | 运行时只能提供机会；成功标准和 blocked 门槛必须反复注入模型上下文 |
| 独立 app-server 通知 | UI 订阅完整 goal snapshot，而不是从零散命令结果猜状态 |

anureo 当前的具体差异和落地计划见 [session-goal-integration.md](./session-goal-integration.md)；不要直接把本文的 `ThreadGoal` 字段替换成 anureo metadata，而应先确认两边的生命周期、checkpoint usage 和事件顺序是否等价。

## 15. 上游源码链接

以下链接固定到本文使用的 commit，便于未来对照变化：

- [goal extension](https://github.com/openai/codex/tree/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/ext/goal)
- [extension.rs](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/ext/goal/src/extension.rs)
- [runtime.rs](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/ext/goal/src/runtime.rs)
- [accounting.rs](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/ext/goal/src/accounting.rs)
- [api.rs](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/ext/goal/src/api.rs)
- [tool.rs](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/ext/goal/src/tool.rs)
- [spec.rs](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/ext/goal/src/spec.rs)
- [GoalStore](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/state/src/runtime/goals.rs)
- [ThreadGoal model](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/state/src/model/thread_goal.rs)
- [app-server processor](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/app-server/src/request_processors/thread_goal_processor.rs)
- [continuation prompt](https://github.com/openai/codex/blob/e3e5ad28470f6a225301518c30a66e749a880164/codex-rs/ext/goal/templates/goals/continuation.md)

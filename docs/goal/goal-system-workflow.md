# Goal 系统 Workflow 方案

> 状态：**已被取代**（2026-09-06）——方向性评审未通过（Rust/Lua 双头编排未决、无 ACP 定位、对 Codex 语义转述失实，详见后继文档附录 A），由 [goal-codex-alignment.md](./goal-codex-alignment.md) 取代；本文保留作历史参考，勿据此实现。
>
> 基于 Codex `/goal` 系统的完整架构分析，将每一块拆分为独立 Agent，设计 anureo 的 Workflow 实现方案。
>
> 参考文档：`docs/goal/codex-goal-analysis.md`

**创建时间**：2025-08-25｜**最后更新**：2025-08-25（日期存疑，参考文档快照为 2026-08-20）

---

## 目录

1. [问题](#1-问题)
2. [方案总览](#2-方案总览)
3. [Agent 分解](#3-agent-分解)
4. [Workflow 编排](#4-workflow-编排)
5. [数据结构](#5-数据结构)
6. [核心流程](#6-核心流程)
7. [集成方案](#7-集成方案)
8. [Edge Cases](#8-edge-cases)
9. [性能分析](#9-性能分析)
10. [实现步骤](#10-实现步骤)

---

## 1. 问题

### 1.1 当前 anureo Goal 系统的局限

anureo 现有 `GoalRunner` 是一个**单体循环**，在一个进程中顺序执行所有逻辑：

```
GoalRunner (loop)
  ├─ build_continuation_prompt()
  ├─ tool.execute()          ← 调用外部编码工具（ShellTool / anureoTool）
  ├─ save_iteration_state()
  ├─ check_token_budget()
  ├─ run_verify_command()
  └─ check_task_status()
```

**问题**：
1. **单点故障** — 整个循环在单一进程中，任何错误导致全部中断
2. **不可扩展** — 无法方便地添加新能力（如 wall-clock tracking、blocked audit、goal steering）
3. **无状态持久化** — 使用 TaskDb meta 存储，不是专用 schema，缺少原子 accounting
4. **无 TUI 集成** — `/goal` 在 REPL 中只是 stub
5. **无自动 continuation** — 依赖用户手动在 REPL 中继续
6. **无不对称控制** — model 可以随意 pause/resume/clear，没有安全边界

### 1.2 Codex 的 6 层架构优势

Codex 将 goal 系统分为 6 层，每层独立演进：

```
TUI → Extension → Runtime → Store → SQLite → Protocol
```

每层有清晰的职责边界，通过事件总线通信，通过 `Extension` 钩子注入生命周期。

### 1.3 目标

将 Codex 的 6 层架构映射为 anureo 的 **Workflow Agent 架构**，每个关键模块是一个独立 Agent，通过 Workflow 编排协作。

---

## 2. 方案总览

### 2.1 架构图

```
┌──────────────────────────────────────────────────────────────────┐
│                         REPL / TUI Layer                         │
│  /goal <desc>  /goal pause/resume  /goal status  /goal edit     │
│         │                                    ▲                   │
│         ▼                                    │                   │
│  ┌──────────────────────────────────────────────────────┐       │
│  │  Goal Orchestrator Agent (workflow)                  │       │
│  │  ┌─────────────┐  ┌──────────────┐  ┌────────────┐  │       │
│  │  │ State Agent │  │ Audit Agent  │  │ Steering   │  │       │
│  │  │ (persist)   │  │ (completion  │  │ Agent      │  │       │
│  │  │             │  │  + blocked)  │  │ (prompts)  │  │       │
│  │  └─────────────┘  └──────────────┘  └────────────┘  │       │
│  └──────────────────────────────────────────────────────┘       │
│         │                                    ▲                   │
│         ▼                                    │                   │
│  ┌──────────────────────────────────────────────────────┐       │
│  │  Coding Agent (anureoTool / ShellTool)                  │       │
│  │  ┌─────────────┐  ┌──────────────┐  ┌────────────┐  │       │
│  │  │ anureoTool    │  │ ShellTool    │  │ CodexTool  │  │       │
│  │  │ (internal)  │  │ (external)   │  │ (external) │  │       │
│  │  └─────────────┘  └──────────────┘  └────────────┘  │       │
│  └──────────────────────────────────────────────────────┘       │
│         │                                    ▲                   │
│         ▼                                    │                   │
│  ┌──────────────────────────────────────────────────────┐       │
│  │  Task DB / SQLite Layer                               │       │
│  │  thread_goals 表  |  GoalMeta (JSON)  |  Accounting  │       │
│  └──────────────────────────────────────────────────────┘       │
└──────────────────────────────────────────────────────────────────┘
```

### 2.2 Agent 分解对应

| Codex 模块 | anureo Agent | 行数预估 | 职责 |
|---|---|---|---|
| GoalStore (1728 行) | **State Agent** | ~400 | SQLite CRUD + 原子 accounting |
| GoalExtension (hooks) | **Orchestrator Agent** | ~500 | 生命周期编排、continuation 调度 |
| GoalRuntimeHandle | **Orchestrator Agent** | — | 内置于 Orchestrator |
| GoalAccountingState | **Accounting Agent** | ~300 | 内存 token + wall-clock 追踪 |
| GoalTool (3 tools) | **Tool Agent** | ~300 | `get_goal` / `create_goal` / `update_goal` 工具实现 |
| GoalService (API) | **Orchestrator Agent** | — | 外部 API 端点 |
| GoalPrompts (3 模板) | **Steering Agent** | ~200 | 3 种 steering prompt 构建 |
| GoalMenu/Status | **Display Agent** | ~200 | TUI 状态栏 + 摘要面板 |
| — | **Audit Agent** | ~300 | 完成审计 + blocked 审计 |

### 2.3 关键设计决策

| 决策 | 选择 | 理由 |
|---|---|---|
| Agent 通信方式 | Event Bus + RPC | 解耦，支持异步 continuation |
| 持久化 | SQLite 专用表 `thread_goals` | 原子 accounting，支持并发 |
| 不对称控制 | model 只能 `create/update(complete\|blocked)` | 安全边界，防止模型误操作 |
| Continuation | 自动触发 + 可 defer | 减少用户操作，但保留退出机制 |
| Budget 控制 | 软停止 + steering prompt | 不中断当前 turn，引导优雅收尾 |
| Accounting 时机 | tool finish + turn stop | 细粒度追踪，串行化防并发 |

---

## 3. Agent 分解

### 3.1 Orchestrator Agent（编排 Agent）

**位置**：`agent/agent-core/src/goal/orchestrator.rs`

**职责**：
- 接收 `/goal` 命令并启动 goal 生命周期
- 协调其他 Agent 的工作
- 调度 continuation（`on_thread_idle` 钩子）
- 管理 goal 状态机（Active ↔ Paused ↔ Blocked ↔ BudgetLimited ↔ Complete）
- 处理外部 mutation（用户 pause/resume/edit）

**核心接口**：

```rust
pub struct GoalOrchestrator {
    state: Arc<GoalStateAgent>,
    accounting: Arc<GoalAccountingAgent>,
    steering: Arc<GoalSteeringAgent>,
    audit: Arc<GoalAuditAgent>,
    tools: Arc<GoalToolAgent>,
    config: GoalConfig,
}

impl GoalOrchestrator {
    /// 创建新 goal
    pub async fn create_goal(&self, req: CreateGoalRequest) -> Result<Goal>;

    /// 暂停 goal
    pub async fn pause_goal(&self, thread_id: &str) -> Result<()>;

    /// 恢复 goal
    pub async fn resume_goal(&self, thread_id: &str) -> Result<()>;

    /// 清除 goal
    pub async fn clear_goal(&self, thread_id: &str) -> Result<()>;

    /// 编辑 objective
    pub async fn edit_goal(&self, thread_id: &str, objective: &str) -> Result<()>;

    /// 获取当前 goal
    pub async fn get_goal(&self, thread_id: &str) -> Result<Option<Goal>>;

    /// 检查是否应该 continuation（idle 时调用）
    pub async fn try_continue_if_idle(&self, thread_id: &str) -> Result<ContinuationResult>;
}
```

**状态机**：

```rust
pub enum GoalStatus {
    Active,        // 活跃
    Paused,        // 用户暂停
    Blocked,       // 3 轮重复阻塞
    UsageLimited,  // 用量超限
    BudgetLimited, // 预算用尽
    Complete,      // 完成
}
```

**状态转移规则**（与 Codex 一致）：

```
(无) ──> Active ──> Paused ──> Active
              │        │
              │        └──> Blocked (3轮) ──> Active (resume)
              │
              ├──> BudgetLimited ──> Complete
              │         │
              │         └──> (clear)
              │
              ├──> UsageLimited ──> Active (resume)
              │
              └──> Complete ──> Active (new goal)
```

### 3.2 State Agent（状态 Agent）

**位置**：`agent/agent-core/src/goal/state.rs`

**职责**：
- 封装 `GoalStore` 所有 SQLite 操作
- 提供原子 accounting 更新（`account_thread_goal_usage`）
- 提供 `expected_goal_id` 校验（stale update protection）
- 管理 `continuation_deferral` 表

**核心接口**：

```rust
pub struct GoalStateAgent {
    db: Arc<TaskDb>,
}

impl GoalStateAgent {
    /// 获取 goal
    pub async fn get(&self, thread_id: &str) -> Result<Option<Goal>>;

    /// 替换 goal（无视旧状态，生成新 goal_id）
    pub async fn replace(&self, thread_id: &str, objective: &str) -> Result<Goal>;

    /// 插入 goal（仅当旧 goal 为 complete 时才替换）
    pub async fn insert_if_complete(&self, thread_id: &str, objective: &str) -> Result<Option<Goal>>;

    /// 更新 goal 状态（带 expected_goal_id 校验）
    pub async fn update_status(
        &self,
        thread_id: &str,
        status: GoalStatus,
        expected_goal_id: &str,
    ) -> Result<Goal>;

    /// 原子 accounting（累加 token + 时间，自动检测 budget limit）
    pub async fn account_usage(
        &self,
        thread_id: &str,
        token_delta: i64,
        time_delta_seconds: i64,
        mode: AccountingMode,
    ) -> Result<AccountingOutcome>;

    /// 删除 goal
    pub async fn delete(&self, thread_id: &str) -> Result<bool>;

    /// 设置/清除 continuation deferral
    pub async fn set_deferral(&self, thread_id: &str, deferred: bool) -> Result<()>;
}
```

**AccountingMode**：

```rust
pub enum AccountingMode {
    ActiveStatusOnly,    // 仅活跃状态
    ActiveOnly,          // 活跃 + budget_limited
    ActiveOrComplete,    // 活跃 + complete（完成结算）
    ActiveOrStopped,     // 所有非 complete 状态
}
```

### 3.3 Accounting Agent（记账 Agent）

**位置**：`agent/agent-core/src/goal/accounting.rs`

**职责**：
- 内存中的 token 用量追踪（按 turn 粒度）
- 墙钟时间追踪（`Instant::now()`）
- 计算 `ProgressSnapshot`（token_delta + time_delta）
- 通过 `Semaphore(1)` 串行化 progress accounting

**核心结构**：

```rust
pub struct GoalAccountingAgent {
    inner: Mutex<GoalAccountingInner>,
    progress_lock: tokio::sync::Semaphore,  // 信号量 1
}

struct GoalAccountingInner {
    current_turn_id: Option<String>,
    turns: HashMap<String, TurnAccounting>,
    wall_clock: WallClockTracking,
}

struct TurnAccounting {
    token_usage: TokenUsageSnapshot,
    last_accounted: TokenUsageSnapshot,
    active_goal_id: Option<String>,
}

struct WallClockTracking {
    last_accounted_at: Instant,
    active_goal_id: Option<String>,
}

struct TokenUsageSnapshot {
    input_tokens: i64,
    cached_input_tokens: i64,
    output_tokens: i64,
}
```

**核心方法**：

```rust
impl GoalAccountingAgent {
    /// 标记 turn 开始（记录基线）
    pub fn mark_turn_start(&self, turn_id: &str, goal_id: &str, usage: TokenUsage);

    /// 标记 turn 结束（计算最终 delta）
    pub fn mark_turn_stop(&self, turn_id: &str) -> Option<ProgressSnapshot>;

    /// 标记 tool finish（增量 delta）
    pub fn mark_tool_finish(&self, turn_id: &str, usage: TokenUsage) -> Option<ProgressSnapshot>;

    /// 计算 token delta
    fn token_delta(snapshot: &TokenUsageSnapshot, current: &TokenUsage) -> i64 {
        current.input_tokens
            .saturating_sub(snapshot.cached_input_tokens)
            .saturating_add(current.output_tokens.max(0))
            .saturating_sub(snapshot.input_tokens + snapshot.output_tokens)
    }
}
```

### 3.4 Steering Agent（引导 Agent）

**位置**：`agent/agent-core/src/goal/steering.rs`

**职责**：
- 构建 3 种 steering prompt
- 生成 `InternalModelContextFragment` 用于注入

**核心接口**：

```rust
pub struct GoalSteeringAgent;

impl GoalSteeringAgent {
    /// 构建 continuation prompt（自动续跑时注入）
    pub fn continuation_prompt(&self, goal: &Goal) -> String;

    /// 构建 budget limit prompt（预算用尽时注入）
    pub fn budget_limit_prompt(&self, goal: &Goal) -> String;

    /// 构建 objective updated prompt（用户编辑时注入）
    pub fn objective_updated_prompt(&self, goal: &Goal, old_objective: &str) -> String;

    /// 构建 blocked audit prompt（检查是否真的 blocked）
    pub fn blocked_audit_prompt(&self, goal: &Goal, consecutive_blocks: u32) -> String;
}
```

**Prompt 模板继承**：

| anureo 模板 | Codex 来源 | 关键改进 |
|---|---|---|
| `continuation_prompt` | `continuation.md` | 保留 anureo 现有的 `RESEARCH & VERIFY` + `COMPLETION AUDIT` |
| `budget_limit_prompt` | `budget_limit.md` | 新增：不开始新工作，总结进度，给用户下一步 |
| `objective_updated_prompt` | `objective_updated.md` | 新增：新 objective 覆盖旧目标 |
| `blocked_audit_prompt` | — | 新增：3 轮重复阻塞检测规则 |

### 3.5 Audit Agent（审计 Agent）

**位置**：`agent/agent-core/src/goal/audit.rs`

**职责**：
- 完成审计：验证 goal 是否真正完成
- Blocked 审计：检测 3 轮重复阻塞条件
- 向 Orchestrator 返回审计结果

**核心接口**：

```rust
pub struct GoalAuditAgent {
    state: Arc<GoalStateAgent>,
}

impl GoalAuditAgent {
    /// 完成审计 — 检查当前状态是否满足 objective
    pub async fn audit_completion(&self, goal: &Goal) -> CompletionAuditResult;

    /// Blocked 审计 — 检查是否达到了 3 轮重复阻塞阈值
    pub async fn audit_blocked(&self, goal: &Goal, turn_id: &str) -> BlockedAuditResult;

    /// 记录阻塞轮次
    pub async fn record_blocked_turn(&self, goal: &Goal, turn_id: &str) -> Result<u32>;
}

pub enum CompletionAuditResult {
    /// 确认完成
    Confirmed,
    /// 未完成（列出未满足的需求）
    Incomplete { missing: Vec<String> },
    /// 证据不足
    InsufficientEvidence { reason: String },
}

pub enum BlockedAuditResult {
    /// 已满足 3 轮阈值，可以标记 blocked
    CanBlock,
    /// 尚未达到阈值
    Insufficient { consecutive_blocks: u32, required: u32 },
    /// 阻塞条件发生变化，重置计数
    Reset,
}
```

### 3.6 Tool Agent（工具 Agent）

**位置**：`agent/agent-core/src/goal/tools.rs`

**职责**：
- 注册 3 个 Model 工具：`get_goal`、`create_goal`、`update_goal`
- 实现工具执行逻辑
- 验权（不对称控制）

**核心接口**：

```rust
pub struct GoalToolAgent {
    orchestrator: Arc<GoalOrchestrator>,
}

impl GoalToolAgent {
    /// 注册工具到工具注册表
    pub fn register_tools(&self, registry: &mut ToolRegistry);

    /// 处理 get_goal
    pub async fn handle_get_goal(&self, thread_id: &str) -> GoalToolResponse;

    /// 处理 create_goal（仅当旧 goal 为 complete 时允许）
    pub async fn handle_create_goal(&self, thread_id: &str, objective: &str, token_budget: Option<i64>) -> Result<GoalToolResponse>;

    /// 处理 update_goal（仅允许 complete 或 blocked）
    pub async fn handle_update_goal(&self, thread_id: &str, status: ModelGoalStatus, expected_goal_id: &str) -> Result<GoalToolResponse>;
}

pub struct GoalToolResponse {
    pub goal: Option<Goal>,
    pub remaining_tokens: Option<i64>,
    pub completion_budget_report: Option<String>,
}
```

### 3.7 Display Agent（显示 Agent）

**位置**：`apps/cli/src/goal/display.rs`

**职责**：
- 渲染 goal 状态摘要
- 渲染状态栏指示器
- 处理 `/goal` 斜杠命令的 UI 交互

**核心接口**：

```rust
pub struct GoalDisplayAgent;

impl GoalDisplayAgent {
    /// 渲染 goal 摘要面板
    pub fn render_summary(&self, goal: &Goal) -> String;

    /// 渲染状态栏指示器
    pub fn render_status_bar(&self, goal: &Goal) -> String;

    /// 渲染可用命令列表
    pub fn render_commands(&self, status: GoalStatus) -> Vec<String>;
}
```

**状态栏显示格式**：

| 状态 | 显示 |
|---|---|
| Active (有 budget) | `12.5K / 50K` |
| Active (无 budget) | `2m` |
| Paused | `paused` |
| Blocked | `stalled` |
| UsageLimited | `usage limited` |
| BudgetLimited | `limited by budget` |
| Complete | `40K tokens` 或 `10h 12m` |

---

## 4. Workflow 编排

### 4.1 核心 Workflow：GoalRun

```
╔══════════════════════════════════════════════════════════════╗
║                    GoalRun Workflow                         ║
╠══════════════════════════════════════════════════════════════╣
║                                                             ║
║  [Start] →  Orchestrator.create_goal()                      ║
║                  │                                           ║
║                  ▼                                           ║
║              State Agent.replace()                          ║
║                  │                                           ║
║                  ▼                                           ║
║              ┌─────────────────────────────────────────┐    ║
║              │         Iteration Loop                   │    ║
║              │  ┌──────────────┐  ┌───────────────┐    │    ║
║              │  │ Steering     │  │ Coding Agent   │    │    ║
║              │  │ Agent        │──│ (anureoTool)     │    │    ║
║              │  │ .continuation│  │ .execute()     │    │    ║
║              │  │ _prompt()    │  └───────┬───────┘    │    ║
║              │  └──────┬───────┘          │            │    ║
║              │         │                  ▼            │    ║
║              │         │           Accounting Agent    │    ║
║              │         │           .mark_turn_stop()   │    ║
║              │         │                  │            │    ║
║              │         │                  ▼            │    ║
║              │         │           State Agent         │    ║
║              │         │           .account_usage()    │    ║
║              │         │                  │            │    ║
║              │         │           ┌──────┴──────┐     │    ║
║              │         │           │ Budget       │     │    ║
║              │         │           │ Exhausted?   │     │    ║
║              │         │           └──┬───┬───────┘     │    ║
║              │         │          Yes │   │ No          │    ║
║              │         │              ▼   ▼             │    ║
║              │         │      Steering  Audit Agent     │    ║
║              │         │      .budget_  .audit_         │    ║
║              │         │      limit_   completion()    │    ║
║              │         │      prompt()  │               │    ║
║              │         │          │     ├── Complete    │    ║
║              │         │          │     ├── Incomplete  │    ║
║              │         │          │     └── Blocked     │    ║
║              │         │          ▼                     │    ║
║              │         │     [End: BudgetLimited]       │    ║
║              │         │                                │    ║
║              └─────────┴───────────────────────────────┘    ║
║                                                             ║
║  [End: Complete] → State Agent.update_status(complete)      ║
║  [End: Blocked] → State Agent.update_status(blocked)        ║
║  [End: BudgetLimited] → State Agent → BudgetLimited         ║
║  [End: Error] → State Agent → Paused / UsageLimited        ║
║                                                             ║
╚══════════════════════════════════════════════════════════════╝
```

### 4.2 自动 Continuation Workflow

```
[Thread Idle]
    │
    ▼
Orchestrator.try_continue_if_idle()
    │
    ├─→ 检查 deferral
    │     └─→ 有 deferral → 跳过
    │
    ├─→ State Agent.get()
    │     └─→ status != Active → 跳过
    │
    ├─→ Steering Agent.continuation_prompt(goal)
    │
    ├─→ 启动新 Coding Agent 迭代
    │
    └─→ 返回 ContinuationResult::Started
```

### 4.3 外部 Mutation Workflow（用户 pause/resume/edit）

```
[User /goal pause]
    │
    ▼
Orchestrator.pause_goal()
    │
    ├─→ 获取 goal_state_lock（防止并发 mutation）
    ├─→ Accounting Agent.mark_turn_stop()
    ├─→ State Agent.account_usage()
    ├─→ State Agent.update_status(paused)
    ├─→ 释放 goal_state_lock
    └─→ 通知 Display Agent 更新 UI
```

### 4.4 Budget Steering Workflow

```
[State Agent.account_usage() 返回 BudgetLimited]
    │
    ▼
Orchestrator.on_budget_limited()
    │
    ├─→ Steering Agent.budget_limit_prompt(goal)
    │
    ├─→ 注入到当前活跃 turn（不中断）
    │     └─→ 引导模型：不开始新工作，总结进度，收尾
    │
    └─→ 当前 turn 结束后，不再自动 continuation
```

---

## 5. 数据结构

### 5.1 SQLite 表

```sql
-- 主表：每个 thread 一个 goal
CREATE TABLE thread_goals (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,                     -- UUID，每次替换生成新 ID
    objective TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'active', 'paused', 'blocked',
        'usage_limited', 'budget_limited', 'complete'
    )),
    token_budget INTEGER,                      -- 可选的 token 预算
    tokens_used INTEGER NOT NULL DEFAULT 0,    -- 已用 token
    time_used_seconds INTEGER NOT NULL DEFAULT 0, -- 已用时间（秒）
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

-- 延迟 continuation 表
CREATE TABLE thread_goal_continuation_deferrals (
    thread_id TEXT PRIMARY KEY NOT NULL
    REFERENCES thread_goals(thread_id) ON DELETE CASCADE
);

-- 阻塞轮次表（用于 blocked audit）
CREATE TABLE thread_goal_blocked_blocks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    thread_id TEXT NOT NULL REFERENCES thread_goals(thread_id) ON DELETE CASCADE,
    turn_id TEXT NOT NULL,
    block_reason TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL
);
```

### 5.2 Rust 结构

```rust
/// Goal 核心数据
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Goal {
    pub thread_id: String,
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Goal 状态枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    UsageLimited,
    BudgetLimited,
    Complete,
}

/// 创建 Goal 请求
pub struct CreateGoalRequest {
    pub thread_id: String,
    pub objective: String,
    pub token_budget: Option<i64>,
}

/// 外部 mutation 请求
pub struct GoalSetRequest {
    pub thread_id: String,
    pub objective: GoalObjectiveOp,
    pub status: Option<GoalStatus>,
    pub token_budget: GoalTokenBudgetOp,
}

pub enum GoalObjectiveOp {
    Keep,
    Set(String),
}

pub enum GoalTokenBudgetOp {
    Keep,
    Set(Option<i64>),
}

/// 完成审计结果
pub struct CompletionAuditResult {
    pub is_complete: bool,
    pub missing: Vec<String>,
    pub evidence: Vec<String>,
}

/// Blocked 审计结果
pub struct BlockedAuditResult {
    pub can_block: bool,
    pub consecutive_blocks: u32,
    pub required_blocks: u32, // 固定为 3
}

/// 工具响应
pub struct GoalToolResponse {
    pub goal: Option<Goal>,
    pub remaining_tokens: Option<i64>,
    pub completion_budget_report: Option<String>,
}
```

### 5.3 现有 GoalMeta 的演进

当前 `GoalMeta` 存储在 TaskDb 中，作为 `meta("goal")` JSON。新架构中，`GoalMeta` 仍然保留作为**历史记录**，但**状态管理**迁移到 `thread_goals` 表。

```rust
// 保留作为历史记录，但不再用于状态管理
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GoalMeta {
    pub iteration: u32,
    pub tool: String,
    pub time_used_seconds: i64,
    pub token_budget: Option<u32>,
    pub tokens_used: u32,
    pub history: Vec<HistoryEntry>,
    pub verify_command: Option<String>,
}
```

---

## 6. 核心流程

### 6.1 创建 Goal（用户通过 /goal 发起）

```pseudo
1. 用户输入 `/goal 将项目从 JS 迁移到 TS`
2. REPL 解析 → 调用 Orchestrator.create_goal()
3. Orchestrator:
   a. 获取 goal_state_lock（信号量）
   b. Accounting Agent 标记当前 turn 停止
   c. State Agent.replace(thread_id, objective)
      - 生成新 goal_id (UUID)
      - INSERT OR REPLACE 到 thread_goals
      - 重置 tokens_used = 0, time_used_seconds = 0
   d. 释放 goal_state_lock
   e. 返回 Goal 给 Display Agent
4. Display Agent 渲染状态摘要
5. Orchestrator 调度 continuation（启动 Coding Agent）
```

### 6.2 迭代执行（每次 turn）

```pseudo
1. Orchestrator 调用 Steering Agent.continuation_prompt(goal)
2. Orchestrator 调用 Coding Agent.tool.execute(prompt)
3. Coding Agent 内部运行 anureoTool（调用 agent 处理）
4. 每次 tool finish:
   a. Accounting Agent.mark_tool_finish() → 计算 delta
   b. State Agent.account_usage(delta) → 原子更新
   c. 返回 BudgetLimited 则注入 steering
5. Turn 结束:
   a. Accounting Agent.mark_turn_stop() → 最终 delta
   b. State Agent.account_usage(delta) → 原子更新
   c. 检查 token budget 是否耗尽
6. 调用 Audit Agent:
   a. 检查 model 是否标记 complete
   b. 如果 model 说 complete，运行完成审计
   c. 如果 model 说 blocked，运行 blocked 审计
7. 根据审计结果:
   - Complete → 状态机进入 Complete
   - Blocked (3轮) → 状态机进入 Blocked
   - Incomplete → 继续循环
```

### 6.3 暂停 / 恢复

```pseudo
暂停:
1. 用户输入 `/goal pause`
2. Orchestrator.pause_goal()
3. 获取 goal_state_lock
4. Accounting Agent 标记 turn 停止
5. State Agent.update_status(paused)
6. 释放锁
7. 通知 Display Agent

恢复:
1. 用户输入 `/goal resume`
2. Orchestrator.resume_goal()
3. 获取 goal_state_lock
4. State Agent.update_status(active)
5. 释放锁
6. 调度 continuation
7. 通知 Display Agent
```

### 6.4 自动 Continuation

```pseudo
1. 系统检测到 thread idle
2. Orchestrator.try_continue_if_idle()
3. 检查 deferral → 有则跳过
4. 检查 goal 状态 → 不是 active 则跳过
5. Steering Agent 构建 continuation prompt
6. 启动新 turn（注入 continuation prompt）
7. 返回 ContinuationResult::Started
```

---

## 7. 集成方案

### 7.1 文件改动清单

| 文件 | 操作 | 说明 |
|---|---|---|
| **新建** | | |
| `agent/agent-core/src/goal/mod.rs` | 🆕 新建 | 模块入口，声明子模块 |
| `agent/agent-core/src/goal/orchestrator.rs` | 🆕 新建 | Orchestrator Agent |
| `agent/agent-core/src/goal/state.rs` | 🆕 新建 | State Agent（SQLite 封装） |
| `agent/agent-core/src/goal/accounting.rs` | 🆕 新建 | Accounting Agent |
| `agent/agent-core/src/goal/steering.rs` | 🆕 新建 | Steering Agent（prompt 构建） |
| `agent/agent-core/src/goal/audit.rs` | 🆕 新建 | Audit Agent |
| `agent/agent-core/src/goal/tools.rs` | 🆕 新建 | Tool Agent（model 工具） |
| `agent/agent-core/src/goal/types.rs` | 🆕 新建 | 公共类型定义 |
| `apps/cli/src/goal/display.rs` | 🆕 新建 | Display Agent |
| `apps/cli/src/goal/mod.rs` | 🆕 新建 | CLI 模块入口 |
| **修改** | | |
| `agent/agent-core/src/lib.rs` | ✏️ 修改 | 添加 `pub mod goal` |
| `apps/cli/src/args.rs` | ✏️ 修改 | 扩展 Goal 命令变体 |
| `apps/cli/src/repl.rs` | ✏️ 修改 | 实现 `/goal` 处理 |
| `apps/cli/src/main.rs` | ✏️ 修改 | 更新 goal 命令处理 |
| `apps/cli/src/lib.rs` | ✏️ 修改 | 添加 goal 模块声明 |
| `apps/cli/src/goal_runner/runner.rs` | ✏️ 修改 | 适配新架构，保留向后兼容 |
| `apps/cli/src/goal_runner/tool.rs` | ✏️ 修改 | 适配新架构 |
| **保留** | | |
| `agent/agent-core/src/goal_runner/` | 🔒 保留 | 现有 runner 作为旧模式保留 |
| `apps/cli/src/goal_runner/` | 🔒 保留 | 现有 runner 工具保留 |
| `apps/cli/src/goal_cmd.rs` | 🔒 保留 | 现有 CLI 入口保留 |

### 7.2 与现有 GoalRunner 的兼容

新架构与现有 `GoalRunner` 共存，提供两种模式：

| 模式 | 触发方式 | 架构 |
|---|---|---|
| **旧模式**（单体） | `anureo goal <desc>` | 现有 GoalRunner 循环 |
| **新模式**（Workflow） | `/goal <desc>` 在 REPL 中 | Agent 协作 |

**过渡策略**：
1. 第一阶段：实现 `/goal` 斜杠命令，使用新架构
2. 第二阶段：`anureo goal` 内部也切换到新架构
3. 第三阶段：废弃 `GoalRunner` 单体循环

### 7.3 REPL 集成

在 `repl.rs` 中，当前 `/goal` 是 stub，修改为：

```rust
// repl.rs 中
anureo_command::Command::Goal { subcommand } => {
    match subcommand {
        GoalSubcommand::Set { description } => {
            // 启动 Orchestrator
            let orchestrator = ctx.goal_orchestrator.clone();
            let result = orchestrator.create_goal(CreateGoalRequest {
                thread_id: ctx.session_id.clone(),
                objective: description,
                token_budget: None,
            }).await?;
            // 显示摘要
            let display = GoalDisplayAgent.render_summary(&result);
            eprintln!("{}", display);
        }
        GoalSubcommand::Show => {
            let goal = ctx.goal_orchestrator.get_goal(&ctx.session_id).await?;
            if let Some(g) = goal {
                eprintln!("{}", GoalDisplayAgent.render_summary(&g));
            } else {
                eprintln!("No active goal. Use /goal <description> to set one.");
            }
        }
        GoalSubcommand::Pause => {
            ctx.goal_orchestrator.pause_goal(&ctx.session_id).await?;
            eprintln!("Goal paused. Use /goal resume to continue.");
        }
        GoalSubcommand::Resume => {
            ctx.goal_orchestrator.resume_goal(&ctx.session_id).await?;
            eprintln!("Goal resumed.");
        }
        GoalSubcommand::Clear => {
            ctx.goal_orchestrator.clear_goal(&ctx.session_id).await?;
            eprintln!("Goal cleared.");
        }
    }
    Ok(())
}
```

### 7.4 命令解析

在 `commands/parser.rs` 中扩展 Goal 命令：

```rust
pub enum Command {
    // ... 现有命令 ...
    Goal {
        subcommand: GoalSubcommand,
    },
}

pub enum GoalSubcommand {
    Set { description: String },
    Show,
    Pause,
    Resume,
    Clear,
}
```

---

## 8. Edge Cases

| # | 场景 | 处理策略 |
|---|---|---|
| 1 | **并发创建 goal** | `goal_id` UUID 防冲突，`expected_goal_id` 校验 |
| 2 | **并发 accounting** | `Semaphore(1)` 串行化，确保原子性 |
| 3 | **中断后恢复** | 读取 SQLite 中保存的 goal 状态，从上次中断点继续 |
| 4 | **Budget 刚好用完** | 软停止：不中断当前 turn，注入 steering 引导收尾 |
| 5 | **Model 不标记 complete** | 无限循环由 `max_iterations` 限制（默认 100） |
| 6 | **Model 过早标记 complete** | 完成审计拒绝，返回继续工作 |
| 7 | **Model 过早标记 blocked** | Blocked 审计拒绝（3轮阈值），返回继续工作 |
| 8 | **用户编辑 objective** | 注入 `objective_updated` steering，调整当前 turn |
| 9 | **用户清除 goal** | 删除 SQLite 记录，取消当前 turn |
| 10 | **Thread fork** | 先 flush progress，新 fork 从 checkpoint 继续 |
| 11 | **大目标（>4000 chars）** | 写入文件，objective 替换为文件引用 |
| 12 | **REPL 退出时 goal 未完成** | 保存暂停状态，下次启动时提示恢复 |
| 13 | **多个 thread 同时 goal** | 每个 thread 独立 goal，互不干扰 |
| 14 | **Rate limit 连续失败** | 指数退避 2^n 秒，最大 6 次重试 |
| 15 | **Tool 执行超时** | 不计数为失败，跳过当前 iteration |
| 16 | **goal_state_lock 死锁** | 所有锁操作加 timeout，超时自动释放 |

---

## 9. 性能分析

### 9.1 内存开销

| Agent | 内存 | 说明 |
|---|---|---|
| Orchestrator | ~10KB | 状态引用 + 配置 |
| State Agent | ~20KB | SQLite 连接池 |
| Accounting Agent | ~1KB/turn | 仅追踪当前 turn |
| Steering Agent | ~5KB | 静态模板 |
| Audit Agent | ~10KB | 审计结果缓存 |
| Tool Agent | ~5KB | 工具定义 |
| Display Agent | ~1KB | 渲染缓存 |

**总计**：~50KB 基线，每个活跃 turn 增加 ~1KB

### 9.2 延迟分析

| 操作 | 延迟 | 说明 |
|---|---|---|
| `create_goal` | ~10ms | SQLite 写入 |
| `account_usage` | ~5ms | 原子 UPDATE，带 budget 检测 |
| `continuation_prompt` | <1ms | 字符串拼接 |
| `audit_completion` | <1ms | 规则检查（无 LLM 调用） |
| `audit_blocked` | <1ms | 轮次计数检查 |
| 状态栏渲染 | <1ms | 字符串格式化 |

### 9.3 与现有系统对比

| 指标 | 现有 GoalRunner | 新 Workflow 架构 |
|---|---|---|
| 启动延迟 | ~50ms | ~10ms |
| 每次迭代开销 | ~5ms | ~5ms |
| 内存占用 | ~100KB | ~50KB |
| 并发能力 | 1 个 thread | 任意 thread 并行 |
| 持久化 | JSON meta | SQLite 专用表 |
| 可扩展性 | 低（单体） | 高（Agent 可独立替换） |

---

## 10. 实现步骤

### Phase 1: 基础骨架（1-2 天）

**目标**：新建所有 Agent 的骨架代码，定义接口和类型。

| 步骤 | 操作 | 验证 |
|---|---|---|
| 1.1 | 新建 `agent/agent-core/src/goal/types.rs`，定义所有公共类型 | `cargo check` |
| 1.2 | 新建 `agent/agent-core/src/goal/state.rs`，实现 State Agent 骨架 | `cargo check` |
| 1.3 | 新建 `agent/agent-core/src/goal/accounting.rs`，实现 Accounting Agent 骨架 | `cargo check` |
| 1.4 | 新建 `agent/agent-core/src/goal/steering.rs`，实现 Steering Agent 骨架 | `cargo check` |
| 1.5 | 新建 `agent/agent-core/src/goal/audit.rs`，实现 Audit Agent 骨架 | `cargo check` |
| 1.6 | 新建 `agent/agent-core/src/goal/tools.rs`，实现 Tool Agent 骨架 | `cargo check` |
| 1.7 | 新建 `agent/agent-core/src/goal/mod.rs`，声明模块 | `cargo check` |
| 1.8 | 新建 `agent/agent-core/src/goal/orchestrator.rs`，实现 Orchestrator Agent 骨架 | `cargo check` |

### Phase 2: State Agent 实现（2-3 天）

**目标**：完整的 SQLite 持久化层，包括原子 accounting。

| 步骤 | 操作 | 验证 |
|---|---|---|
| 2.1 | 添加 `thread_goals` 表迁移 | `cargo test` |
| 2.2 | 实现 `get` / `replace` / `insert_if_complete` | `cargo test` |
| 2.3 | 实现 `update_status` + `expected_goal_id` 校验 | `cargo test` |
| 2.4 | 实现 `account_usage` 原子 UPDATE + budget 检测 | `cargo test` |
| 2.5 | 实现 `delete` / `set_deferral` | `cargo test` |
| 2.6 | 实现 `BlockedBlocks` 表操作 | `cargo test` |

### Phase 3: Accounting + Steering + Audit（2-3 天）

**目标**：完整的记账、提示注入、审计逻辑。

| 步骤 | 操作 | 验证 |
|---|---|---|
| 3.1 | 实现 Accounting Agent 的 token 追踪 | `cargo test` |
| 3.2 | 实现 Accounting Agent 的 wall-clock 追踪 | `cargo test` |
| 3.3 | 实现 `mark_turn_start` / `mark_tool_finish` / `mark_turn_stop` | `cargo test` |
| 3.4 | 实现 Steering Agent 的 3 种 prompt 构建 | 手动检查 prompt 质量 |
| 3.5 | 实现 Audit Agent 的完成审计 | `cargo test` |
| 3.6 | 实现 Audit Agent 的 blocked 审计 | `cargo test` |

### Phase 4: REPL 集成（1-2 天）

**目标**：`/goal` 在 REPL 中可用。

| 步骤 | 操作 | 验证 |
|---|---|---|
| 4.1 | 扩展命令解析器，添加 `GoalSubcommand` | `cargo test` |
| 4.2 | 实现 REPL 中的 `/goal` 处理 | 手动测试 |
| 4.3 | 实现 Display Agent 摘要渲染 | 手动测试 |
| 4.4 | 后端集成 GoalOrchestrator | `cargo test` |
| 4.5 | 实现 `try_continue_if_idle` 自动 continuation | 手动测试 |

### Phase 5: Tool Agent + 不对称控制（1-2 天）

**目标**：Model 可以调用 goal 工具，但受不对称控制限制。

| 步骤 | 操作 | 验证 |
|---|---|---|
| 5.1 | 实现 `get_goal` 工具注册 | `cargo test` |
| 5.2 | 实现 `create_goal` 工具 + 验权 | `cargo test` |
| 5.3 | 实现 `update_goal` 工具 + 验权 | `cargo test` |
| 5.4 | 集成到工具注册表 | `cargo test` |

### Phase 6: 兼容性 + 过渡（1-2 天）

**目标**：旧模式可用，新模式稳定。

| 步骤 | 操作 | 验证 |
|---|---|---|
| 6.1 | 确保 `anureo goal` 旧模式不受影响 | `cargo test` |
| 6.2 | 新模式与旧模式共享同一个 TaskDb | `cargo test` |
| 6.3 | 文档更新 | — |
| 6.4 | 端到端测试 | 手动测试 |

### Phase 7: 高级功能（可选，按需）

| 步骤 | 操作 | 优先级 |
|---|---|---|
| 7.1 | 状态栏指示器（TUI footer） | P2 |
| 7.2 | 大目标文件化 | P2 |
| 7.3 | Continuation deferral（用户选择不自动续跑） | P3 |
| 7.4 | Analytics / OTel 指标 | P3 |

---

---

## 11. Workflow Lua 文件清单

### 11.1 文件列表

| 文件 | 行数 | 职责 |
|---|---|---|
| `.anureo/workflows/goal-run.lua` | 367 | **主 Workflow**：编排迭代循环，协调 coding+audit+steering |
| `.anureo/workflows/goal-audit.lua` | 99 | **子 Workflow**：完成审计 + blocked 审计 |
| `.anureo/workflows/goal-steering.lua` | 145 | **子 Workflow**：4 种 steering prompt 构建 |
| `.anureo/workflows/goal-introspect.lua` | 84 | **子 Workflow**：goal 状态摘要显示 |

### 11.2 调用关系

```
goal-run.lua (主编排)
  │
  ├─→ coding agent（内置 agent() 调用）— 执行实际编码工作
  │
  ├─→ goal-audit.lua（子 workflow）
  │     └─→ audit agent — 判断 complete / incomplete / blocked
  │
  ├─→ goal-steering.lua（子 workflow）
  │     ├─→ continuation prompt — 继续迭代
  │     ├─→ budget_limit prompt — 预算用尽
  │     ├─→ objective_updated prompt — 目标变更
  │     └─→ blocked prompt — 阻塞
  │
  └─→ goal-introspect.lua（子 workflow）
        └─→ 渲染状态摘要
```

### 11.3 使用方式

```bash
# 通过 workflow 工具启动
workflow_start({
  workflow = ".anureo/workflows/goal-run.lua",
  args = {
    objective = "将项目从 JS 迁移到 TS，strict mode",
    thread_id = "session-001",
    token_budget = 50000,
  },
})

# 查看 goal 状态
workflow_start({
  workflow = ".anureo/workflows/goal-introspect.lua",
  args = {
    goal = { status = "active", objective = "...", tokens_used = 12345 },
  },
})
```

### 11.4 状态机（6 种状态）

| 状态 | 进入条件 | 后续操作 |
|---|---|---|
| `active` | 创建 goal / 用户 resume | 自动 continuation |
| `paused` | 用户 /goal pause / 达到 max_iterations | 用户 /goal resume |
| `blocked` | 3 轮相同阻塞条件 | 用户 /goal resume 或 clear |
| `budget_limited` | token 用量超过 budget | 用户 /goal clear |
| `usage_limited` | coding agent 失败 | 用户 /goal clear |
| `complete` | audit agent 确认完成 | 用户 /goal clear |

---

## 附录：与 Codex 的架构映射

| Codex 文件 | anureo 对应 | 状态 |
|---|---|---|
| `state/src/runtime/goals.rs` (1728L) | `goal/state.rs` | 待实现 |
| `ext/goal/src/extension.rs` | `goal/orchestrator.rs` | 待实现 |
| `ext/goal/src/runtime.rs` | `goal/orchestrator.rs` | 待实现 |
| `ext/goal/src/accounting.rs` | `goal/accounting.rs` | 待实现 |
| `ext/goal/src/tool.rs` | `goal/tools.rs` | 待实现 |
| `ext/goal/src/api.rs` | `goal/orchestrator.rs` | 待实现 |
| `ext/goal/src/spec.rs` | `goal/tools.rs` | 待实现 |
| `ext/goal/src/steering.rs` | `goal/steering.rs` | 待实现 |
| `ext/goal/src/events.rs` | `goal/orchestrator.rs` | 待实现 |
| `ext/goal/src/analytics.rs` | 暂不实现 | — |
| `ext/goal/src/metrics.rs` | 暂不实现 | — |
| `prompts/templates/goals/continuation.md` | `goal/steering.rs` | 待实现 |
| `prompts/templates/goals/budget_limit.md` | `goal/steering.rs` | 待实现 |
| `prompts/templates/goals/objective_updated.md` | `goal/steering.rs` | 待实现 |
| `tui/src/chatwidget/goal_menu.rs` | `goal/display.rs` | 待实现 |
| `tui/src/chatwidget/goal_status.rs` | `goal/display.rs` | 待实现 |
| `tui/src/goal_display.rs` | `goal/display.rs` | 待实现 |
| `tui/src/goal_files.rs` | 暂不实现 | — |
| `tui/src/slash_command.rs` | `commands/parser.rs` | 待扩展 |
| `state/goals_migrations/*.sql` | 迁移脚本 | 待创建 |
| `protocol/src/protocol.rs` | `goal/types.rs` | 待实现 |
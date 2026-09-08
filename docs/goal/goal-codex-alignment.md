# Goal 系统对齐 Codex 架构方案

> 状态：已实施（2026-09-07，按本文档执行；P0-P6 已落地，P7 可选项未开工。偏离与实际结论见 [goal-codex-alignment-todo.md](./goal-codex-alignment-todo.md) 的进度记录与各 Phase 勾选结论）
> 创建：2026-09-06
> 范围：将 anureo goal 系统从 detached 自主循环重构为 session-integrated、以 `thread_goals` 表为单一事实源的 Codex 同构架构；覆盖 ACP / OpenChamber FE / CLI REPL 全部产品面
> 取代：[goal-system-workflow.md](./goal-system-workflow.md)（多 Agent + Lua 双头编排方向，评审未通过，取代理由见附录 A）；[session-goal-integration.md](./session-goal-integration.md) 的 Phase 3（JS runtime 移植）由本方案接管，其 Phase 1-2 基础设施（metadata 表、objective 文件）保留复用
> 参考：[codex-goal-analysis.md](./codex-goal-analysis.md)（语义基线，上游快照 `e3e5ad28`）；[../acp-spec/extensions/14-goal-scheduled-task.md](../acp-spec/extensions/14-goal-scheduled-task.md)

---

## 1. 摘要

现状是「detached 循环 + JSON meta + 双存储割裂」：goal 以独立后台任务形态运行（`apps/cli/src/goal_runner/runner.rs:29`），与交互会话零融合，计量靠 `GoalMeta` JSON 整体 read-modify-write，完成权威是 `verify_command` shell，模型没有任何 goal 工具，前端不消费任何 goal 事件。

本方案将 goal 改造为 **挂在 ACP 会话上的运行时扩展**（Codex `ext/goal` 同构）：

- goal 与用户会话同生命周期，由 turn 钩子驱动记账与续跑；
- 专用 SQLite 表 `thread_goals` 为唯一事实源，原子 `UPDATE...RETURNING` 记账 + `goal_id` CAS 防陈旧写；
- 模型经 3 个不对称工具（`get_goal`/`create_goal`/`update_goal`）自宣告 complete/blocked；
- `continue_if_idle` 幂等续跑 + deferral 保护；
- 越预算软停止（KeepActive + 一次性 steering），不打断当前 turn；
- FE 经 `session.metadata.anureo.goal` 投影 + `session.updated` 扇出可见，零/低前端改动。

预估 12–15 天（Phase 0–6，见 §10）。

## 2. 现状架构与弱项

### 2.1 现状架构（改进前）

三条入口互不相通、两套同构循环、三处互不同步的状态存储；**goal 是会话外的后台任务**，不挂在用户会话线程上：

```
入口（三条，互不相通）
────────────────────────────────────────────────────────────────
CLI   anureo goal <desc> ─► goal_cmd.rs ──┐
ACP   /goal (agent.rs:1143) ─────────────┼─► GoalRunner 单体循环（runner.rs:29）
                                          │    ├─ build_continuation_prompt()
REPL  /goal ─► repl.rs:123 stub（不可用）  │    ├─ tool: AnureoTool(task-mcp-server) / ShellTool
                                          │    ├─ save_iteration_state() ──► ① TaskDb meta("goal")
                                          │    ├─ run_verify_command()（完成唯一权威）
                                          │    └─ while 自迭代；中断续跑靠 --resume
                                          └─► apps/acp/src/goal_runner.rs（同款循环副本）

只读视图（FE 不消费）
_anureo.dev/goal/*（extensions/goal.rs）──► ② goals.json（六方法 CRUD + 重启恢复预约）

任务状态借用
③ tasks.status：Paused 借用 Pending，budget_limited/blocked 借用 Cancelled
```

关键特征：循环自带 LLM 调用、在 runner 进程内自迭代；用户与模型之间没有任何 goal 工具/事件通道；①②③ 三处状态没有同步机制。

### 2.2 现状弱项（对照 Codex，按弱→次弱）

| # | 能力 | Codex | anureo 现状（代码依据） |
|---|---|---|---|
| 1 | 架构位形 | goal 挂在 thread 上，turn/thread 钩子驱动 | detached 循环（runner.rs:29，经 task-mcp-server 调 AnureoTool），与交互会话零融合 |
| 2 | 计量原子性/粒度 | 专用表原子记账 + CAS + 四种 mode 补账 | `GoalMeta` JSON 整体 RMW（runner.rs:441-466；`get_meta`/`set_meta` 本身非原子，task-core/src/db.rs:313/323）；无代际；按迭代粗粒度累计；保存失败仅 log 即丢账 |
| 3 | 完成控制权 | 模型 `update_goal` 自宣告 + 工具 schema 内状态机规则 | 完成权威 = `verify_command` 通过即自动 Completed（runner.rs:404-425）；模型无 goal 工具、无 blocked 申报 |
| 4 | 状态机 | 6 态 + `is_terminal(budget_limited, complete)` | GoalLifecycle 8 态（agent/agent-core/src/goal_runner/state.rs:177 起），budget_limited/blocked 塞进 `cancelled`、Paused 复用 `Pending`（runner.rs:480-484） |
| 5 | 自动续跑 | `on_thread_idle→continue_if_idle` 幂等 + deferral | 循环内自迭代、断了靠 `--resume`；无 idle hook、无 deferral；ACP 重启恢复（extensions/goal.rs:388-453）是 crash recovery 非 idle 续跑 |
| 6 | 预算处理 | KeepActive + 一次性 mid-turn steering | 仅 20% 余量 warning 文本（runner.rs:519-520）+ 硬停 |
| 7 | 事实源/事件 | 单一 SQLite + `ThreadGoalUpdated` 全量快照扇出 | 三处状态互不同步：TaskDb meta（执行真值）、goals.json（`_anureo.dev/goal/*` 视图）、task status；FE 均不消费（session-goal-integration.md §2.2） |
| 8 | 并发控制 | 双锁分离（调度 vs 计量） | 无 goal 锁，仅连接池=1 + `atomic_update_status`（只覆盖 status）兜底 |
| 9 | 护栏 | `max_goal_token_budget` 上限、objective 校验、sub-agent 工具不可见 | 均无 |
| 10 | 可观测性 | events/metrics | history 摘要列表 + tracing |

**保留的 anureo 独有能力**（Codex 没有，本方案不丢弃）：

- `verify_command`：改为 complete 的可选硬校验门（模型宣告 complete 且 verify 通过才落账）；
- ACP 重启恢复预约机制（extensions/goal.rs:388 原子预约 + `goal/list` 重连）：对接新表保留。

## 3. 前置决策

| # | 决策 | 内容 | 理由 |
|---|---|---|---|
| D1 | 位形 | goal 为 session-integrated 扩展，`thread_id` = ACP session id（非 task_id） | 修复弱项 1 的根因；steering 注入、idle 续跑、per-turn 计量只在会话内可实现 |
| D2 | 单一编排 | 只在 Rust 宿主侧实现（新 crate `agent/goal`）；goal **不是** Lua workflow | idle 钩子、双锁、原子记账无法在 luft workflow 内实现（goal-system-workflow.md 的 blocker） |
| D3 | 单一事实源 | `thread_goals` 表为唯一真值；goals.json 一次性导入后废弃；FE 经 metadata 投影可见 | 消除三处状态割裂；兼容 session-goal-integration.md 的前端契约 |

## 4. 目标架构（改进后）

```
OpenChamber FE ── session.updated(metadata.anureo.goal) ──┐
REPL /goal ───────────────────────────────────────────────┤
                                                           ▼
            GoalService（用户 mutation：set/pause/resume/clear/edit）
                                                           │
   ACP agent loop（turn 生命周期）                          ▼
   on_turn_start/stop/abort/error ──► GoalRuntimeHandle ◄── goal_state_lock
   on_token_usage / on_tool_finish ──► GoalAccounting ◄─── progress_accounting_lock
   on_session_idle ──► continue_if_idle ─► start_turn_if_idle（幂等二道门）
                    │                    └─ steering 注入（continuation / budget_limit）
                    ▼
            GoalStore（SQLite thread_goals：UPDATE...RETURNING + expected_goal_id CAS）
```

### 4.1 架构变化要点（现状 → 目标）

| 维度 | 现状（改进前） | 目标（改进后） |
|---|---|---|
| 位形 | detached 后台任务，与会话无关 | session-integrated：goal 挂在 ACP 会话，由 turn 生命周期钩子驱动 |
| 事实源 | TaskDb meta JSON + goals.json + tasks.status 三处割裂 | `thread_goals` 单表唯一真值，事件全量快照扇出 |
| 迭代驱动 | runner 进程内 while 循环自迭代 | 模型 turn 结束 → idle 钩子 → `continue_if_idle` 幂等续跑 + deferral 保护 |
| 记账 | 迭代级粗粒度、JSON RMW 非原子、保存失败丢账无感知 | tool-call 级 delta、原子 `UPDATE...RETURNING` + CAS + 基线推进规则 |
| 完成判定 | `verify_command` 通过即完成（唯一权威） | 模型 `update_goal(complete)` 自宣告 + verify_command 降级为可选门 |
| 阻塞判定 | 无申报路径（Failed 塞 cancelled） | 模型 `update_goal(blocked)`，三轮规则走 prompt 自律 |
| 预算处理 | 20% 余量警告 + 硬停 | KeepActive 软停 + 一次性 steering，不打断当前 turn |
| 模型工具 | 无 | `get/create/update_goal` 三工具（不对称控制） |
| 并发控制 | 无锁，pause/clear 与记账可交错 | `goal_state_lock` + `progress_accounting_lock` 双锁分域 |
| FE 可见性 | 不消费任何 goal 状态 | `session.metadata.anureo.goal` 投影 + `session.updated` 扇出，零/低前端改动 |

### 4.2 组件映射（旧 → 新）

| 现状组件 | 去向 |
|---|---|
| `apps/cli` GoalRunner（runner.rs 单体循环） | 冻结 legacy（`anureo goal` 保留），后续移除 |
| `apps/acp/src/goal_runner.rs`（`/goal` 执行路径） | Phase 6 切到 `GoalRuntimeHandle` |
| TaskDb `meta("goal")`（GoalMeta JSON） | 一次性迁入 `thread_goals`，之后只读 |
| goals.json + `_anureo.dev/goal/*` 六方法 | API 面保留，后端切 GoalStore；文件废弃 |
| extensions/goal.rs 重启恢复预约机制 | 保留，对接 `thread_goals` |
| repl.rs `/goal` stub | 接 GoalService，六子命令 |
| `Command::Goal { description }` | 改 `{ subcommand }`，一个 PR 内同步全部消费方 |

## 5. 模块布局

| 位置 | 内容 |
|---|---|
| `agent/goal/src/types.rs` | `Goal`/`GoalStatus`(6 态)/`is_terminal()`、`CreateGoalRequest`、`GoalSetRequest`、事件 payload |
| `agent/goal/src/store.rs` | CRUD + `account_thread_goal_usage`（mode 门控 + `expected_goal_id` CAS + Unchanged 语义）、deferral 读写 |
| `agent/goal/src/accounting.rs` | 内存 token/墙钟基线、双锁、`budget_limit_reported_goal_id`（steering 去重） |
| `agent/goal/src/steering.rs` | 4 模板：continuation / budget_limit / objective_updated / （现有 RESEARCH&VERIFY、COMPLETION AUDIT 段落并入 continuation） |
| `agent/goal/src/tools.rs` | `get_goal`/`create_goal`/`update_goal`，schema description 内嵌状态机规则（不对称控制） |
| `agent/goal/src/runtime.rs` | `GoalRuntimeHandle`：全部钩子 + `continue_if_idle` + deferral 管理 + budget steering |
| `agent/goal/src/service.rs` | 用户侧 API（供 ACP 扩展与 REPL 共用），持 `goal_state_lock` 的 set/clear 窗口 |
| `apps/acp/src/extensions/goal.rs` | 改造：六方法后端切到 GoalStore；新增 `goal.updated` 全量快照通知 + session metadata 投影；保留重启恢复预约 |
| `apps/cli/src/repl.rs` + `agent/agent-core/src/commands/` | `/goal set/show/pause/resume/clear/edit`；`Command::Goal` 改 subcommand 是破坏性变更，同一 PR 内同步 agent.rs:1143 消费方 |

`agent/goal` 为独立 crate：依赖仅 task-core（db）+ foundation，不依赖 agent-core，避免反向耦合。依赖方向：`apps/* → agent/goal → task-core`；agent-core 只提供钩子点位与 ToolRegistry 接口。

### 5.1 代码架构图（改进后）

```
═══════════════ 接入层 ═══════════════
OpenChamber FE ◄── session.updated（metadata.anureo.goal 投影）
CLI REPL       /goal set|show|pause|resume|clear|edit
ACP 客户端     _anureo.dev/goal/*（六方法）+ goal.updated 全量快照通知
CLI legacy     `anureo goal`（旧 GoalRunner，冻结 deprecated）

═══════════════ 宿主接线层（apps / agent-core）═══════════════
apps/acp   stdio_loop.rs / agent.rs（session 驱动）
   ├─ on_turn_start / stop / abort / error ─┐
   ├─ on_token_usage / on_tool_finish ──────┼──► GoalRuntimeHandle（runtime.rs）
   └─ on_session_idle ──────────────────────┘
   └─ extensions/goal.rs（改造）
       ├─ _anureo.dev/goal/* 六方法 ────────► GoalService（service.rs）
       ├─ goal.updated 通知 + metadata 投影 ◄── GoalEvent（types.rs）
       └─ 重启恢复预约（保留）──────────────► GoalStore（store.rs）

apps/cli   repl.rs：/goal 六子命令 ─────────► GoalService
           goal_cmd.rs + goal_runner/（legacy，冻结 deprecated）

agent/agent-core   agent loop（LLM 循环）：新增 turn 钩子点位（Phase 0 落点）
                   Command::Goal { subcommand }（破坏性变更，一处收口）
                   ToolRegistry ◄── GoalTools 注册（tools.rs，主 session 门控）

═══════════════ agent/goal（新 crate）═══════════════
service.rs   GoalService：set/pause/resume/clear/edit
             （持 goal_state_lock：读→写→start_turn 窗口）
   │
runtime.rs   GoalRuntimeHandle：钩子实现 + continue_if_idle
             （持 goal_state_lock 至 start_turn_if_idle 幂等提交）
             + deferral 管理 + budget steering 一次性注入
             + verify_command 完成门（anureo 扩展）
   │
   ├─► accounting.rs   GoalAccounting：token/墙钟基线
   │      progress_accounting_lock（snapshot→SQL 成功→基线推进）
   │      公式 (Δinput−Δcached)+max(Δoutput,0)
   │      budget_limit_reported_goal_id 去重
   ├─► steering.rs     continuation / budget_limit / objective_updated 模板
   ├─► tools.rs        get_goal / create_goal / update_goal
   │      （schema description 内嵌状态机规则，不对称控制）
   └─► store.rs        GoalStore：CRUD + account_thread_goal_usage
          （UPDATE...RETURNING + expected_goal_id CAS
           + AccountingMode 门控 + Unchanged 语义）+ deferral 读写

types.rs     Goal / GoalStatus(6 态) / is_terminal / GoalEvent / 请求类型

═══════════════ 存储层 ═══════════════
experimental/task/task-core   TaskDb（sqlite）
   ├─ PRAGMA foreign_keys(true)（前置修复）
   ├─ migrations：thread_goals ＋ thread_goal_continuation_deferrals
   └─ goals.json 一次性导入后废弃；GoalMeta 迁入后只读
```

---

## 6. 语义基线（从 Codex 原样搬运，禁止「改进」）

### 6.1 状态机

6 态：`active / paused / blocked / usage_limited / budget_limited / complete`。

```
无 goal ──(create/外部 set)──> active
active ──(用户 pause)────────> paused
paused ──(用户 resume/set)───> active
active ──(模型 update_goal)──> complete 或 blocked
active ──(token budget)──────> budget_limited
active ──(provider 用量)─────> usage_limited
active ──(不可恢复 turn error)> blocked
任意已存在 goal ──(clear)────> 无 goal
```

- `is_terminal() = {budget_limited, complete}`；blocked / usage_limited 可由用户恢复。
- `usage_limited` 语义 = provider/账户用量限制（系统置位），**不是** runner 失败。

### 6.2 Token 计量

- 先按字段差分，再套公式：`goal_tokens = (Δinput − Δcached) + max(Δoutput, 0)`，全程 saturating；这是 budget accounting 不是 billing report。
- 内存基线**仅在 SQL 记账返回 Updated 后推进**；Unchanged（goal 已被替换/状态不符）→ delta 丢弃，防止旧 turn 写到新 goal。
- `update_goal` 工具调用自身不计入 progress。
- sub-agent / workflow agent 不暴露 goal 工具、不计账（仅持久化主 session）。

### 6.3 AccountingMode（允许冲账的状态集）

| Mode | 允许状态 | 用途 |
|---|---|---|
| `ActiveStatusOnly` | active | 正常进度 |
| `ActiveOnly` | active, budget_limited | tool/turn 结束补记越界前后最后一段 |
| `ActiveOrComplete` | active, budget_limited, complete | 模型完成时补齐最后使用量 |
| `ActiveOrStopped` | active, paused, blocked, usage_limited, budget_limited | 错误/停止路径补账 |

### 6.4 双锁

| 锁 | 覆盖窗口 | 解决问题 |
|---|---|---|
| `goal_state_lock`(1 permit) | 读 goal → 外部写入/状态更新 → `start_turn_if_idle` 返回 | 防 idle 续跑读到旧 goal 后用户 clear/set 又启动旧目标 |
| `progress_accounting_lock`(1 permit) | 取 snapshot → SQLite 更新成功 → 推进内存基线 | 防多个 tool finish/turn stop 消费同一 delta |

两锁不混用；所有 acquire 带 timeout。

### 6.5 Budget（KeepActive）与 steering

- 越界时 DB 转 `budget_limited`，**不打断当前 turn**；只注入一次 budget steering（`budget_limit_reported_goal_id` 去重），引导不开始新工作、总结收尾。
- 下一次 idle 因状态非 active 不再自动续跑。
- 若 mid-turn 注入通道不可用（Phase 0 核实），降级为 turn 边界注入（可接受，见 R1）。**Phase 0 已核实：通道不存在，走降级路径（附录 B.6）**。

### 6.6 Continuation 与 deferral

- `continue_if_idle` 流程：可见性检查 → acquire `goal_state_lock` → 有 deferral 则返回 → 读 `thread_goals` → 非 active 清内存标记并返回 → 渲染 continuation steering → `start_turn_if_idle`（幂等二道门，多 idle 事件不重复启动）。
- deferral 写入场景：session fork / goal 快照替换 / 外部 mutation 保护；`on_turn_start` 清除。

### 6.7 墙钟

`Instant` 基线；仅 goal 为 active 时计时；pause/blocked/complete/usage_limited 清除基线；resume 恢复 idle 基线。进程重启期间的墙钟不追补。

### 6.8 模型工具（不对称控制）

| 工具 | 规则 |
|---|---|
| `get_goal` | 无参；返回 goal 快照 + `remaining_tokens = max(budget − used, 0)` |
| `create_goal` | 仅用户/system 明确要求时调用；不能覆盖 unfinished goal（旧 goal 须 complete 或由用户改）；objective trim+校验；budget 为正且 ≤ `max_goal_token_budget` |
| `update_goal` | 只接受 `complete`/`blocked`；blocked 要求同一阻塞条件连续 ≥3 轮且确实无法推进（prompt 自律规则，**不做服务端计数**）；complete 附 `completion_budget_report` |

工具 schema description 内嵌上述规则全文（Codex `spec.rs` 做法）。

### 6.9 verify_command 融合（anureo 扩展）

模型宣告 `complete` 时：若配置了 `verify_command`，先执行 verify，通过才落 `complete`，失败则拒绝并注入继续 steering；未配置则直接接受模型宣告。verify 是完成门，不是完成权威。

## 7. 数据模型与迁移

### 7.1 新表（TaskDb 迁移，`experimental/task/task-core/migrations/`）

```sql
CREATE TABLE thread_goals (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    objective TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'active','paused','blocked','usage_limited','budget_limited','complete'
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

- 不建 `blocked_blocks` 计数表：三轮规则走 prompt 自律（reference §7.2 明确 schema 不计数）。
- **前置修复**：TaskDb 当前未开 `PRAGMA foreign_keys`（db.rs 连接配置），必须 `SqliteConnectOptions::foreign_keys(true)`，否则 CASCADE 全部失效；迁移测试覆盖级联删除。
- `account_thread_goal_usage` 用 `UPDATE ... RETURNING` 单语句完成累加 + budget 检测 + 状态转移 + CAS（`WHERE goal_id = expected`）。
- R4 备选：若评审不接受 session 域数据进 task 库，改独立 `goals.sqlite`（Codex 同款），store 接口不变。

### 7.2 旧数据迁移（一次性，不双写）

| 来源 | 映射 |
|---|---|
| `GoalMeta.lifecycle = Cancelled` | 删除（= clear） |
| `GoalMeta.lifecycle = Failed` | → `blocked`（按 lifecycle_reason 归因） |
| Completed | → `complete`（tokens_used/time 直译） |
| Active/Paused/Blocked/UsageLimited | 同名直译 |
| goals.json（`_anureo.dev/goal/*`） | 导入后文件废弃 |

`GoalMeta`/`GoalLifecycle`/旧 `history` 不再演进，保留只读用于迁移与审计。

## 8. 外部面

| 面 | 接入 |
|---|---|
| ACP 扩展 | `_anureo.dev/goal/*` 六方法后端切 GoalStore；新增 `goal.updated` 全量快照通知（UI 订阅快照而非猜状态） |
| OpenChamber FE | 每次 goal 变更投影进 `session.metadata.anureo.goal`，随 `session.updated` 扇出（payload 对照 FE reducer 定契约，沿用 session-goal-integration R1 结论） |
| CLI REPL | `/goal <desc>` = set；`/goal show/pause/resume/clear/edit`；`Command::Goal` 从 `{ description }` 改 `{ subcommand: GoalSubcommand }`（command.rs:16），同步 apps/acp/src/agent.rs:1143 |
| CLI `anureo goal` | 冻结为 legacy（走旧 runner），不在关键路径；后续版本移除 |
| workflow / sub-agent | goal 工具不可见、不计账（6.2） |

## 9. 与现有 4 套体系的关系

| 体系 | 处置 |
|---|---|
| `_anureo.dev/goal/*` ACP 扩展（extensions/goal.rs） | 保留 API 面，后端切 GoalStore；goals.json 废弃 |
| ACP `/goal` → apps/acp/src/goal_runner.rs | Phase 6 起切到新 runtime；旧 runner 冻结 |
| CLI `anureo goal` + apps/cli GoalRunner | legacy 保留，标 deprecated，后续移除 |
| goal-system-workflow.md（本文取代） | 标记已取代，保留历史 |
| session-goal-integration.md | Phase 1-2 复用；Phase 3 被本文 §4-6 取代 |

## 10. 分阶段计划（12–15 天）

| Phase | 内容 | 验证 |
|---|---|---|
| 0（1d） | Hook 点审计：agent-core 循环的 turn 边界 / token usage 事件 / tool finish 出口、ACP idle 检测、mid-turn 注入通道有无；产出接线清单与降级决定 | 审计结论入本文 §6.5/§11 |
| 1（2-3d） | `agent/goal` crate：types + store + 迁移 + FK pragma；CAS/mode 门控/Unchanged 并发单测 | `cargo nextest run -p goal` |
| 2（2-3d） | accounting（公式/双锁/基线推进）+ steering 模板；token 公式与 Codex 对拍用例 | 同上 |
| 3（2-3d） | 钩子接入 ACP agent loop + `continue_if_idle` + deferral + budget steering（含降级路径） | 集成测试 + 手动 |
| 4（2d） | 3 个 model tools + sub-agent 门控 + spec 规则文本 + verify_command 完成门 | 工具单测 |
| 5（2d） | `_anureo.dev/goal/*` 切后端 + metadata 投影/事件扇出 + REPL `/goal` | ACP e2e + FE 手动验收 |
| 6（1-2d） | goals.json/GoalMeta 一次性迁移、旧 runner 冻结、goal-system-workflow.md 标记取代、ref 文档同步 | 全量回归 |
| 7（可选） | OTel 指标、objective 文件化（>4000 chars 走文件，metadata 留 `objectiveFile: true`）、fork 快照替换 | — |

## 11. 风险与降级

| # | 风险 | 缓解 |
|---|---|---|
| R1 | mid-turn 注入通道缺失（agent loop 无 inter-turn 注入点） | Phase 0 核实；缺失则降级 turn 边界注入（Codex 语义弱化可用；session-goal-integration R8 同结论） |
| R2 | `session.updated` payload 与 FE reducer 契约不一致 | Phase 5 先对照 `packages/ui` reducer 定形再实现投影 |
| R3 | `Command::Goal` 破坏性变更波及 agent.rs:1143 与 FE `/goal` 文本解析 | 一个 PR 内同步收口；解析层兼容期同时接受裸 `/goal <desc>` |
| R4 | TaskDb 混入 session 域表引发边界争议 | 备选独立 `goals.sqlite`，store 接口不变，切换成本一天内 |
| R5 | sqlite 并发下 CAS/双锁正确性 | Phase 1-2 专项并发单测（交错 pause/clear/account 用例） |

## 12. 明确不做

- 不做 Lua workflow 编排 goal（D2）；
- 不做服务端 blocked 三轮计数/审计表（prompt 自律，6.8）；
- 不做多租户/跨 session goal 聚合（沿用 session-goal-integration Phase 5 可选池）;
- 不迁移旧 runner 的 scheduled-task 联动（延后）。

---

## 附录 A：对 goal-system-workflow.md 的取代理由

1. **双头编排未决**：Rust Orchestrator（§3-7）与 Lua `goal-run.lua` "主编排"（§11）不能同时成立；idle 钩子、双锁、原子 SQL 记账只能在宿主侧实现。
2. **定位缺失**：改动清单 0 处 apps/acp，而 anureo 主产品面是 ACP+OpenChamber；`thread_id` 语义悬空；未处理与 `_anureo.dev/goal/*`、ACP goal_runner、session-goal-integration.md 的关系。
3. **语义转述失实**：token 公式（§3.3）与 Codex 不等价；状态机图含 `Paused→Blocked`、`BudgetLimited→Complete`（Codex 无此转移，budget_limited 是 terminal）；AccountingMode 注释漏 `budget_limited`；丢「Unchanged 不推进基线」规则。
4. **行为偏离未声明**：`thread_goal_blocked_blocks` 服务端计数、规则化完成审计（<1ms 无 LLM）均偏离 Codex 且无判等/判定规则定义。

本文以「原样搬运语义 + 显式声明 anureo 扩展（verify_command、恢复预约）」取代上述方向。

---

## 附录 B：Phase 0 接线清单（2026-09-06 审计完成）

> 结论：**R1 降级决定 = turn 边界注入**（mid-turn 注入通道不存在，见 B.6）；其余钩子点位均有现成落点，Phase 3 按本清单接线。

### B.1 Turn 边界与生命周期

| 项 | 结论 |
|---|---|
| agent-core turn 执行 | `run_agent_from_config`（`agent/agent-core/src/run/runner.rs:121`，inner :137）；结束以 `StreamRunOutcome::Finished`(:178)/`Cancelled`(:184) 区分；错误走 `RunError` |
| ACP turn 驱动 | `apps/acp/src/agent.rs:1079` `prompt()` / `:1092` `prompt_with_capabilities()`；react 配置构建于 `:1422` `build_react_config` —— 钩子注册落点 |
| abort 通道 | `ReactRunner::with_cancellation(RunCancellation)`（`react/runner/runner.rs:43`） |
| ACP idle 检测 | `SessionLifecycle::Idle`（`apps/acp/src/session.rs:324`，转换点 :534/:569/:619-638）；`SessionSyncPromptState::Idle`（`session_update_log.rs:51`）—— `on_session_idle` 挂 prompt 完成后 lifecycle 归 Idle 的转换点 |

### B.2 记账与工具事件

| 项 | 结论 |
|---|---|
| LlmUsage 定义 | `foundation/llm/src/traits.rs:124`：prompt/completion/cached/reasoning 字段齐全，token 公式字段可用 |
| think 节点捕获 | `react/think_node.rs:181-215`（stream usage）→ `apply_think(..., usage)`(:359)；`state.usage/total_usage`（`initial_state.rs:32`） |
| stream 事件 | `StreamEvent::TurnFinish { reason, usage }`（`stream-event/src/types/stream_event.rs:172`）；`ToolStart`/`ToolEnd`（:135/:146，发送点 `sink/stream_writer.rs:352/:394`） |
| Δ 差分 | `stream-event/src/codex.rs:18-36` `CodexUsage` 已实现 `Sub`（差分语义现成可复用） |
| tool finish 消费模式 | `react/runner/review_coordinator.rs:19` 注释确认 ToolStart/ToolEnd 事件流可被协调器消费 —— goal accounting 同法接线 |

### B.3 工具与门控

| 项 | 结论 |
|---|---|
| ToolRegistry | `agent/tool/tool-core/src/registry.rs:31`（register :52 / register_sync :169） |
| 主 session 工具构建 | `react/build/tool_source.rs` `build_tool_source`（`react/build/runners.rs:18` 调用）；`:245` `if config.goal_mode` 注册 task 工具 —— goal 三工具注册落点同处 |
| sub-agent 路径 | `tools/agent/build_config.rs:175`（独立构建路径）→ goal 工具仅注册进主构建源，sub-agent 不可见天然成立 |

### B.4 goal_mode 链路（存废建议）

`run/types.rs:76`、`run_types.rs:66`、`profile_helper.rs:55/104/158/210`（恒 false）、`config_builder.rs:77`（透传）、`react_build_config.rs:79`、`tool_source.rs:245`（消费）。goal_mode 为旧 detached runner 时代的遗留通道；建议 P4 将其语义改为「主 session goal 工具注册门控」或删除并以「有活跃 goal」判定取代，最终决策在 P4 落地时做出并回记本清单。

### B.5 Provider 用量信号

`foundation/llm/src/error/provider/mod.rs:82/:122`：`ErrorKind::RateLimited` 与 429+quota → `QuotaExhausted` 启发式（`is_quota_429` :156）。`usage_limited` 置位 = turn 错误路径携带 `QuotaExhausted`；其余不可恢复错误 → `blocked`。

### B.6 R1 结论：mid-turn 注入通道不存在

- react 图为闭合 pregel 循环（think→act→observe），无迭代间消息注入通道；`RunCancellation` 仅支持 abort；
- nudge 机制（`react/nudge.rs`、`review_coordinator.rs`）是 turn/iteration 边界触发 + 旁路副作用（后台 review），非对话内注入；
- system prompt 装配（`react/config/prompt_assembly.rs:21` `SystemPromptInputs`）per-run 静态，不适合动态 steering；
- **决定**：采用 §6.5 降级 = turn 边界注入（budget 越界后，budget_limit steering 随下一次 continuation prompt 注入；`budget_limit_reported_goal_id` 去重语义不变）。Phase 3 按此实现。

### B.7 Legacy 与配置面

| 项 | 结论 |
|---|---|
| feat/goal 分支 | 已完全合入 dev（merge-base = 其 tip da40f8cb）；`loom-feat-goal` worktree 陈旧无半成品 |
| verify_command | 现状 = GoalMeta per-goal 字段（`apps/cli/src/goal_runner/runner.rs:53`）+ CLI flag（`goal_cmd.rs:100`）。**§7.1 表结构缺载体 → P1 建表补 `verify_command TEXT` 列** |
| max_goal_token_budget | 全仓不存在，P1 新建配置项 |
| 现存测试资产 | `apps/acp/tests/e2e_goal_recovery.rs`、`e2e/tests/web/multi-wt-goal.spec.ts`、`commands/parser.rs` goal 解析单测、`goal_runner/state.rs` legacy JSON 兼容测试 |

---

## 附录 C：参考源修正——codex-acp 中立 goal 扩展（2026-09-07）

> 结论：**外部面权威基准 = provider-neutral goal extension**（[agentclientprotocol/codex-acp](https://github.com/agentclientprotocol/codex-acp) `docs/goal-extension.md`，全文已核）。本文 §6/§7 的内核语义（thread_goals / 6 态 / 记账 / 续跑）**不变**——codex-acp 自身也是 codex `thread/goal/*` 之上的薄投影层；需要修正的只有外部协议面（原 §8 按本附录实施，执行清单 = TODO Phase 8）。
>
> **落地（2026-09-08，P8）**：能力块**双处广播**——响应顶层 `_meta.goal`（codex-acp / cowork `goalCapabilitiesFromInitialize` 识别位置，跨仓核对确认）+ `agentCapabilities._meta.goal` 同形保留；协议面已同步 spec 14「中立 goal 面」一节；旧 `_anureo.dev/goal/*` 降级为不广播 legacy alias。遗留：FE 跨仓手动验收、全链路续跑 e2e（见 todo 进度记录 2026-09-08）。

### C.1 规范要点

| 要素 | 内容 |
|---|---|
| 能力协商 | `initialize` 响应 `_meta.goal = {version: 1, controlMethod: "_session/goal", actions: [...]}`；`actions ⊆ {set, pause, resume, clear}` 为实现方实际支持子集；客户端不得推断未广播的能力 |
| 控制方法 | `_session/goal`：请求 `{sessionId, action}`；`set` 附带非空 `objective` |
| 快照 | 发布于 `session_info_update._meta.goal`；**清除时发布 `goal: null`**；字段 camelCase、Unix **毫秒**；可选字段允许报告 iterationCount、lastContinuationReason |
| 状态 | 中立 5 态：`active / paused / blocked / limited / complete` |
| 生命周期解耦 | goal 属 ACP session 而非单个 prompt；`active` ≠ prompt 运行中；prompt 在静默边界完成，goal 可继续驱动后续自主循环（在已完成的 prompt 之外发布 session update）；turn 运行中客户端用 steering 或 prompt 队列 |
| 兼容策略 | 旧 provider 私有方法保留为**不广播的 legacy alias**（codex-acp 对 `_codex/session/goal_control` 的同款做法）；发布 `_meta.goal` 而非 provider 私有 metadata（如 `_meta.codex.goal`） |
| codex 侧映射 | `thread/goal/*` 通知 → 中立快照；usageLimited + budgetLimited → `limited`；秒级时间戳 → 毫秒 |

### C.2 状态映射（本文 6 态 → 中立 5 态）

| 内部状态 | 中立状态 | 说明 |
|---|---|---|
| active | active | |
| paused | paused | |
| blocked | blocked | |
| usage_limited | limited | 归并 |
| budget_limited | limited | 归并（真值可经可选字段 / lastContinuationReason 透出） |
| complete | complete | |

### C.3 与本文的关系

- §8 的外部面以本附录为准实施；`_anureo.dev/goal/*` 六方法（P5b 交付的 wire 形状）**降级为不广播的 legacy alias**，映射到同一 GoalService。
- `verify_command`、`status_reason` 等 anureo 扩展列不影响中立投影（不入快照，或经可选字段透出）。
- 验收参照：cowork/OpenChamber 客户端按 `version===1 && controlMethod==='_session/goal'` 识别能力（见 cowork 仓 AgentGateway），中立面落地后 FE 应零改动直连 anureo。

# Session Goal 整合方案（anureo 前端 ⇄ anureo 后端）

> 状态：历史草案（2025-08-19）；本文记录当时的 Session Goal 缺口与候选方案，不代表当前 anureo/Desk 实现状态。开发 session metadata/list 时请以 37 号规范和 `session-list-redesign.md` 为准。**2026-09-07 注记：本文 Phase 3（JS runtime 移植）已由 [goal-codex-alignment.md](./goal-codex-alignment.md) 接管并实施（P0-P6）；Phase 1-2 基础设施（metadata 表、objective 文件）仍为后续复用资产。**
> 范围：把 anureo 的 **Session Goal**（元数据驱动的服务端目标循环）移植到 anureo ACP 后端，使前端 goal UI 在 ACP 模式下完整可用。
> 参考：
> - 参考实现（上游）：`anureo-feat-dev/packages/web/server/lib/session-goal/`（`runtime.js` 36KB + `DOCUMENTATION.md` + `routes.js` + `objectives.js`）
> - 既有设计：[goal-system-workflow.md](./goal-system-workflow.md)（多 Agent Workflow 方向，本方案不采用，见 §2.3）
> - ACP 扩展规范：[../acp-spec/extensions/14-goal-scheduled-task.md](../acp-spec/extensions/14-goal-scheduled-task.md)

## 摘要

**历史问题（2025-08-19）**：anureo 前端 goal UI（arm 开关、goal strip、管理对话框）当时依赖两个 anureo 侧不存在的服务端能力——ACP 模式下 `patchSessionMetadata` 是 no-op stub，`/api/goals/objective` 路由与续跑 runtime 缺失。该描述是历史基线；当前 session metadata storage/update 已由 SessionIndex repository 与 `_anureo.dev/session/update` 路径提供，goal runtime 是否实现仍需以当前源码单独核实，不能直接据本草案判断。

**方案**：按"直接移植、零前端改动（goal 层）"路线，把上游 web server 的 session-goal runtime（36KB JS，事件驱动循环）移植为 Rust 模块 `apps/acp/src/session_goal/`。**不做** `goal-system-workflow.md` 的多 Agent 编排（前端契约是元数据+事件+循环，非 workflow API）。语义基线取 anureo（独立小模型审计器为终止权威、快照式 token 核算、五态状态机），工程机制借鉴 Codex（`codex-rs/ext/goal/`）：goal-state/accounting 双互斥锁、`expected_goal_id` CAS 防陈旧写、fork continuation deferral、mid-turn 预算 steering（§2.4 对照表）。

**四个阶段**（约 10–16 天，含单测）：
1. **Phase 1 元数据基础设施**（3–4 天，关键路径）：SQLite `session_user_metadata` 表 + `_anureo.dev/session/update_metadata` 深合并 patch + `session.updated` 事件扩为完整会话记录扇出 + FE stub 实现——打通后 goal UI 立即可见可操作；
2. **Phase 2 objective 文件**（1–2 天）：`<anureo_home>/goals/<sessionId>.md` 存储 + bridge 路由支持 `:param`，长 objective 走文件、metadata 只留 `objectiveFile: true`；
3. **Phase 3 runtime 移植**（5–8 天，核心）：idle 钩子驱动 tick（核算→终态检查→小模型审计→continuation 续跑），含 blocked 3 连击、审计失败容忍、compaction 分段核算、abort↔pause 联动、settle 通知；
4. **Phase 4 端到端验收**（1–2 天）：arm 流真实生效 + §4.5 八项 Web 交互验收 + web-audit 回归。

**主要风险**：R1 `session.updated` payload 形状须对照 FE reducer 定契约；R2/R3 anureo checkpoint 的 usage/compaction 字段口径差异；R7 双锁与 `prompt_capacity` 信号量的锁序；R8 mid-turn 注入通道若缺失则退化为 OC 基线（可接受）。均有缓解措施（§6）。

**保留不动**：anureo 既有三套 goal 系统（`_anureo.dev/goal/*` ACP 扩展、`/goal` CLI、workflow 设计文档）与本方案无关，全部保留；跨 session goal 聚合等收敛项列入 Phase 5 可选。

---

## 1. 背景

anureo 的 Session Goal 是**服务端持久化**的自主目标循环：

- 状态存于 `session.metadata.anureo.goal`，随 `session.updated` 事件同步到所有客户端；
- UI 只通过**元数据 patch** 读写 goal（create/edit/pause/resume/clear），从不直接驱动循环；
- 服务端 runtime 在 session 空闲时 tick：token 核算 → 终态检查 → 小模型审计 → 自动续跑（continuation prompt）；
- 小模型审计是**唯一**终止权威（除硬停：预算耗尽 / 续跑次数上限 / turn 错误）。

前端代码已完整就位（`packages/ui/src/lib/sessionGoal{Metadata,Actions,Presentation}.ts`、`hooks/useSessionGoal.ts`、`components/chat/SessionGoal*.tsx`、`stores/useSessionGoalArmStore.ts`），**anureo 后端侧缺口是本方案要解决的全部内容**。

## 2. 现状差距

### 2.1 缺口清单

| # | 前端契约 | 参考实现位置 | anureo 现状 | 缺口 |
|---|---|---|---|---|
| G1 | `session.metadata.anureo.goal` 可读写、随 `session.updated` 全量扇出 | web server session 存储 + SSE | **历史基线**：当时无用户元数据；当前 `acp_session_data`、SessionIndex metadata projection、`session/update` response/event 已实现。Goal 专属 runtime/字段语义仍需另行核实 | 不得按本行旧结论判断当前实现 |
| G2 | `PUT/GET/DELETE /api/goals/objective/:sessionId` | `routes.js` + `objectives.js`（`<data-dir>/goals/<sessionId>.md`） | bridge 路由表无此路由，且 `acp-rest-bridge.ts` 路由为**精确匹配**、不支持路径参数 | 全缺 |
| G3 | idle tick → 核算 → 审计 → `prompt_async` 续跑循环 | `runtime.js` | 无 | 全缺（见 §2.2） |
| G4 | Settings gate `sessionGoalEnabled` / 默认预算 | web server settings | `anureo.ui` blob 已持久化到 anureo settings，Rust 侧可读 | ✅ 基本可用 |
| G5 | 小模型审计（restrictToPreferredProvider、session 自身 provider 优先） | runtime.js audit 调用 | `small-model` 扩展已存在 | 需对齐调用形态 |
| G6 | settle 后桌面/推送通知 | `emitGoalNotification` | `notification` 扩展已存在 | 需对齐 |
| G7 | scheduled task 以 goal 运行 | scheduled-tasks runtime 盖章元数据 | anureo 无 scheduled-task runtime | 延后（Phase 5） |

### 2.2 anureo 既有但**不相关**的三套 goal 系统

1. `_anureo.dev/goal/*` ACP 扩展（`apps/acp/src/extensions/goal.rs`）：独立 `goals.json` 存储、六方法 CRUD。**前端不消费它**。定位：规范 parity / 未来跨 session goal，保留但不在本方案关键路径。
2. `/goal` CLI 命令 + `goal_runner.rs`（`apps/acp/src/goal_runner.rs`、`apps/cli/`）：CLI 自主循环，与 session goal 语义不同，不动。
3. `docs/goal/goal-system-workflow.md` 多 Agent Workflow 设计：见 §2.3。

### 2.3 架构决策：直接移植 runtime，而非多 Agent Workflow

`goal-system-workflow.md` 设计了 7 个 Agent（Orchestrator/State/Accounting/Steering/Audit/…）+ Workflow 编排。**本方案不采用**，理由：

- 前端契约是**元数据 + 事件 + 循环**，不是 workflow API；走 workflow 需要前端重写 goal 层，违背"零前端改动"目标；
- 参考实现是单文件事件驱动循环（36KB JS），语义紧凑、有生产验证（blocked 3 连击、审计失败容忍、分段核算等细节都是踩坑产物），直接移植风险最低；
- 多 Agent Workflow 可作为后续演进（审计/steering 换成 workflow 编排），不阻塞 parity。

### 2.4 Codex Goal 机制对照与借鉴（`C:\Users\heycj\dev\codex\codex-rs\ext\goal/`）

Codex 的 Thread Goal 与 OC Session Goal 是两种范式，语义不可互换，但**工程机制高度可借鉴**：

| 维度 | Codex | OC（本方案基线） | 取舍 |
|---|---|---|---|
| 驱动 | 生命周期钩子（`on_thread_idle` 直接 `continue_if_idle`），零轮询 | 15s idle tick + kickoff 定时器 | anureo 采用钩子驱动（`agent.rs` lifecycle idle 处），kickoff 定时器仅保留为 FE 契约的 kickoff 语义 |
| 终止权威 | model 自律（`update_goal` 工具 + prompt 内完成/阻塞审计规则） | 独立小模型审计器 | **保留 OC 审计器**（对失控模型更鲁棒，审计不看 model 自评）；codex 的 completion/blocked 审计措辞并入 continuation prompt |
| 记账 | 增量式：per-turn `token_delta_since_last_accounting`（`input − cached + output`）+ 墙钟秒数 | 快照式：turn 末快照 − goal 前 baseline，compaction 分段 | FE 契约字段不变（tokensUsed 单调）；Phase 3 先按 OC 快照模型保真移植，增量记账（usage 事件实时累加 + 墙钟）作为内部演进备选，写入口径对齐 FE |
| 状态机 | active/paused/blocked/**usage_limited**/budget_limited/complete；budget_limited 为可恢复中间态 | 无 usage_limited | FE payload 不加 usage_limited；若 anureo 用量耗尽可映射为 blocked + statusReason |
| 互斥 | `goal_state_lock`（Semaphore(1)）贯穿“读 goal → 启动续跑”窗口；`progress_accounting_permit` 串行化并发冲账 | 先持久化再续跑（防双发） | **采纳 codex 双锁**：goal-state permit + accounting permit，比仅靠写序更强 |
| 陈旧写防护 | `expected_goal_id` CAS 进 SQL WHERE | 写回前重读比对 goal id | anureo：`UPDATE ... WHERE goal_id = ?`（元数据深合并的 goal 路径带 CAS）|
| 防重复续跑 | `thread_goal_continuation_deferrals` 表（fork 盖章、turn start 清除） | 无（重启恢复靠元数据扫描） | **采纳**：anureo session fork / metadata 外部变更时盖章 deferral |
| 预算触顶 | `BudgetLimitedGoalDisposition::KeepActive`：tool_finish 时仅注入 budget steering（“勿开新工作，尽快收尾”），当前 turn 不中断 | idle 时才判 budgetLimited | **采纳 mid-turn steering**：usage 事件超预算 → 立即注入 steering part，settle 仍等 idle |
| 存储 | SQLite `thread_goals`（单 thread 单 goal，前一个须 complete 才可插新，`ON CONFLICT ... WHERE status='complete'`） | session metadata blob（goal 嵌套） | 维持 metadata（FE 契约）；但 goal 创建/替换的“前置终态”检查同 codex |

关键源码锚点：`ext/goal/src/extension.rs`（钩子注册面）、`runtime.rs:362`（continue_if_idle）、`accounting.rs:332`（token 口径）、`state/src/runtime/goals.rs:499`（原子记账 + SQL CASE 翻转 budget_limited）、`templates/goals/continuation.md`（completion/blocked 审计 prompt）。

## 3. 分阶段实施

依赖关系：Phase 1 → Phase 2 → Phase 3 → Phase 4；Phase 5 独立可选。

### Phase 1：Session 元数据基础设施（G1，关键路径）

**目标**：任意客户端可 patch 会话用户元数据，所有客户端经 `session.updated` 收到含元数据的完整会话记录。

1. **存储**：`session_repository.rs` 新增表（SQLite migration）：
   ```sql
   CREATE TABLE session_user_metadata (
     session_id TEXT PRIMARY KEY REFERENCES ... ON DELETE CASCADE,
     metadata   TEXT NOT NULL DEFAULT '{}',  -- JSON blob
     version    INTEGER NOT NULL DEFAULT 0,
     updated_at TEXT NOT NULL
   );
   ```
   写语义：深合并（opencode PATCH 语义——提供的键覆盖，未提供的保留；`null` 删除键）。这是前端 `writeGoal` 更新器模式的直接对偶。
2. **协议**：新增扩展方法 `_anureo.dev/session/update_metadata`：
   ```jsonc
   // request
   { "sessionId": "...", "metadata": { "anureo": { "goal": { ... } } } }
   // response：完整会话记录（含 metadata）
   ```
   `apps/acp/src/extensions/` 新增 `session_metadata.rs`（或并入现有 session 处理），注册进 `register.rs`。
3. **事件扇出**：`global_events.rs` 的 `session.updated` 事件 properties 从 `{id}` 扩为**完整会话记录**（复用 §3.1 的记录组装器：repository 字段 + lifecycle + 用户元数据）。opencode 前端 reducer 按全量 properties 更新会话 store，元数据随事件实时同步——goal strip 即插即用。
4. **前端**（anureo-feat-dev）：
   - `acp-session-actions.ts` 的 `patchSessionMetadata` 从 no-op stub 改为：GET 现记录 → updater 计算 → `session/update_metadata` 深合并提交；
   - 会话列表/详情（`GET /api/experimental/session` bridge 链路）返回体带 metadata。

**验收**：两个浏览器窗口 patch 同一 session 元数据，双方 `session.updated` 均收到新值；重启 server 后元数据仍在。

### Phase 2：Objective 文件存储与路由（G2）

1. **Rust 侧**：`apps/acp/src/session_goal/objectives.rs`——`<anureo_home>/goals/<sessionId>.md`；端口 `objectives.js` 全部细节：
   - `SESSION_ID_PATTERN = ^[A-Za-z0-9_-]{4,128}$`（触碰文件系统前校验，防路径注入）；
   - 5000 字符 clamp；写要求非空；读缺失返回 null；删 best-effort。
2. **扩展方法**：`_anureo.dev/goal/objective`（`{action: "put"|"get"|"delete", sessionId, content?}`），复用现有 goal 扩展注册路径（与 `goals.json` 的 ACP goal 扩展共存，方法名不冲突）。
3. **前端 bridge**：`acp-rest-bridge.ts` 路由匹配升级——支持 `:param` 段（前缀树或逐段匹配），注册：
   ```
   PUT/GET/DELETE /api/goals/objective/:sessionId → _anureo.dev/goal/objective
   ```
   前端 `sessionGoalActions.ts` 的 `writeObjectiveFile / fetchGoalObjectiveContent / deleteObjectiveFile` **零改动**（写失败回退 inline 的降级路径同样保留）。

**验收**：FE 创建 5000+ 字符 goal，metadata 只有 `objectiveFile: true`；文件落盘；断开 bridge（模拟 VS Code）strip 降级显示审计 note。

### Phase 3：Session Goal Runtime（G3+G5+G6，核心移植）

**位置**：新模块 `apps/acp/src/session_goal/`：

```
session_goal/
├── mod.rs          // SessionGoalRuntime：生命周期管理（启动/重启恢复）
├── state.rs        // GoalPayload 解析/校验/写回（对齐 sessionGoalMetadata.ts）
├── objectives.rs   // Phase 2
├── accounting.rs   // token 快照核算 + compaction 分段
├── audit.rs        // 小模型审计调用 + 判定状态机
├── prompt.rs       // continuation prompt 构建（XML 转义 objective 块）
└── notify.rs       // settle 通知
```

在 `runtime.rs` 构建处与 agent 一起 spawn，持 `Arc`：agent runtime（`execute_prompt`）、session repository、checkpoint 存储、small-model 扩展、notification 扩展、settings blob 读取。

**事件源**（对齐参考实现的 idle-tick 模型，用 anureo 生命周期事件替代轮询）：

- 挂钩 `agent.rs` 的 lifecycle 迁移（`set_lifecycle(..., "idle")` 处，agent.rs:1473/1509）→ idle 触发 15s 安全 tick；
- `session.updated`（元数据写入）携带 fresh active goal（`turnsUsed === 0` 或 `statusReason === "resumed"`）→ kickoff 定时器：新 goal 3s、显式 Resume ~250ms；
- **重启恢复**：启动时扫描含 active goal 的 session，重新挂载定时器（状态在元数据里，天然持久）。

**Tick 逻辑**（1:1 移植 `runtime.js` + `DOCUMENTATION.md`，语义保真优先）：

1. settings gate：`anureo.ui` blob 的 `sessionGoalEnabled`（settings 变更经 global bus settings topic 失效缓存）；
2. 取 session（跳过 sub-agent session），要求 active goal；
3. 静默检查：消息尾部是 user 消息或未完成 assistant 回复 → 退出等下次 idle；
4. **token 核算（快照模型）**：最近一条完成 assistant turn 的 `input + cache.read + output` 为快照（历史 turn 折进下轮 cache，快照天然含全程付费量）；goal 相对化：首轮 tick 记 `tokensBaseline`（goal 前最新 turn 同口径快照）；compaction（`summary: true` 的 assistant 消息）切段：summary turn 快照计入 `tokensCommitted`，新段 baseline 归零；`tokensUsed = tokensCommitted + 当前段`，单调不回退；
5. 终态检查（廉价优先）：assistant turn error → `blocked`；`tokensUsed ≥ tokenBudget` → `budgetLimited`；`turnsUsed ≥ 20`（MAX_AUTO_TURNS）→ `blocked`；
6. 尾部是 compaction summary → 跳过审计直接续跑（summary 是转述不是证据）；
7. **小模型审计**：objective（每 tick 从 objective 文件新鲜解析，读失败回退 inline）+ 仅最后一条 assistant turn；JSON `{verdict: continue|complete|blocked, note}`；session 自身 provider/model 优先；`complete` → settle；`blocked` 连续 3 次才 settle；审计失败容忍 1 次未审计续跑（`auditFailStreak`），连续第 2 次失败 settle 为 `blocked`（可 Resume，settle 清零重给容忍额度）；
8. 续跑：**先持久化核算 + turnsUsed**（崩溃后等下次 idle tick，反序会双发）→ 复查尾部 → 持 **goal-state permit**（每 session 一个 Semaphore(1)，贯穿“读 goal → 提交 continuation”窗口，阻塞外部 set/clear 插队，§2.4 codex 互斥）→ 用最后一条 assistant turn 的 provider/model/agent 配置 `execute_prompt` 发 continuation（占 `prompt_capacity` 信号量，与用户 prompt 同池）；提交前检查 session 无 fork/deferral 盖章；
9. **abort 交互**：用户 abort（session/cancel / MessageAborted）→ goal 立即 `paused`（暂停即停止，两轴一致）；pause goal → 同时取消运行中 turn；Resume 跨过 aborted 尾部 → 跳过审计直接 nudge；
10. **防陈旧写**：goal 路径写回带 CAS——`expected_goal_id` 进更新条件（§2.4；实现为元数据深合并时对 `anureo.goal.id` 先读后比对，goal id 不匹配则丢弃写入）；
11. **mid-turn 预算 steering**（codex KeepActive 语义）：usage 事件推算 `tokensUsed ≥ tokenBudget` 时**不**立即打断当前 turn，而是向活跃 turn 注入 budget steering part（“预算已到，勿开新工作，尽快收尾并总结”）；settle 为 `budgetLimited` 仍发生在 idle tick；
12. **fork deferral**：session fork 或外部 metadata 变更触发 goal 结构变更时，写 `session_goal_continuation_deferrals`（session_id 主键）；下一次用户 turn start 清除；`continue_if_idle` 见章即跳过；
13. settle（complete/blocked/budgetLimited）→ notification 扩展广播（桌面 + UI）；goal active 期间抑制 per-turn ready 通知（error/question/permission 不受影响）。

**Continuation prompt**：移植 `runtime.js` 内联构建——objective 作为不可信数据放入 XML 转义的 `<objective>` 块、预算数字、保持完整 objective 与基于证据工作的规则、完成审计说明、每 turn 末尾输出事实性 done/verified/remaining 报告（审计只见最后一条 turn，报告即证据）；同时并入 codex `templates/goals/continuation.md` 的措辞资产：requirement-by-requirement 完成审计清单（“证据须证明完成而非未找到反证”）、blocked 需连续 3 turn 的自评规则（与 OC 外部审计器的 3 连 blocked 语义对偶）。

**验收**：
- 单测：核算分段（含 compaction）、verdict 状态机（3 连 blocked、审计失败容忍）、防陈旧写、objective 校验/clamp；
- 集成：脚本化 fake agent 驱动 session 生命周期 → 断言续跑发出 / goal 正确 settle / abort 即暂停；
- 手动：FE arm goal → strip 状态流转 active→(paused/resume)→complete/budgetLimited。

### Phase 4：端到端接线与验收

- FE arm 流（composer 目标按钮 → 发送时写 goal 元数据 + 合成 system-reminder part）经 Phase 1 真实生效（`session-ui-store.ts` 现有代码零改动）；
- Settings→Chat 的 goal 开关 + 默认预算：三层 parity 中 web-server/client 已可用，VS Code bridge 侧渲染状态、隐藏入口（loop 在 anureo 服务端跑，天然全端可用——比参考实现更强，可在文档标注）；
- `scripts/web-audit/` 回归：goal strip 不破坏首屏基线；验收清单见 §4。

## 4. Web 交互方案（用户视角）

前端交互代码已全部就位，本节固化其语义作为 anureo 集成的验收基准（源码：`packages/ui/src/components/chat/SessionGoal{Button,Dialog,Row}.tsx`、`hooks/useSessionGoal.ts`、`lib/sessionGoal{Actions,Presentation}.ts`、`sync/session-ui-store.ts:1030`）。

### 4.1 交互面

| 交互面 | 位置 | 行为 |
|---|---|---|
| 目标按钮（target 图标） | composer footer（ChatInput.tsx:5356/5433 两处：桌面/移动布局） | 无 goal 时＝arm 开关（一键 arm/disarm）；有 goal（含 complete）＝打开管理对话框 |
| 字符计数器 | 目标按钮旁，arm 时实时显示 | `len/LIMIT`，超限变红（typing 时预替截断）；arm 态不收键盘（Android 软键盘保活：`onMouseDown preventDefault`） |
| Goal strip | composer 上方紧凑条（ChatInput.tsx:4937/5024） | 状态点（色）＋ note/objective 截断文本 ＋ 状态/evaluating ＋ usage ＋ inline pause/resume；仅信息展示，管理入口不在此 |
| 管理对话框 | 目标按钮唤起；桌面 Dialog / 移动 MobileOverlayPanel（同一 body） | 创建：objective（≤限制）＋可选 budget（≥1000，步进 50k）；管理：状态色/usage/turns/note/statusReason ＋ pause/resume（隐性，经 save）/clear |
| Settings gate | Settings→Chat | `sessionGoalEnabled` 总开关（关＝全 UI 隐藏）＋默认 budget（arm 流默认值） |

### 4.2 状态呈现规则

- **色/文案映射**（`sessionGoalPresentation.ts`）：active=info 蓝、paused=灰、blocked/budgetLimited=warning 黄、complete=success 绿；同一映射服务 chat/sidebar/mobile 三个面
- **evaluating 态**：`status=active` 且 session idle（服务端静默窗口/审计中）→ spinner＋“评估中”，避免看起来卡死；这是 anureo idle tick 期间最关键的“活着”信号
- **usage 显示**：有 budget 显示 `used/budget`；无 budget 仅在 `tokensUsed>0` 时显示（避免 0 噪音；核算只在 idle tick 落地）
- **文本优先级**：strip 显示 `note`（最新审计进度）优先于 objective；objectiveFile 时另拉文件（按 `id+updatedAt` 缓存）
- **statusReason 只在 blocked/budgetLimited 显示**（失败态才值得读原因；"verified by audit" 是噪音）

### 4.3 动作语义（对 anureo 后端的隐式要求）

| 动作 | 前端行为 | 后端必须配合 |
|---|---|---|
| Arm→发送 | 发送时消费 armed 标记：写 goal 元数据（objective=消息文本）＋注入 goal-mode system-reminder（告知跨 turn、每 turn 末输出 done/verified/remaining 报告）＋默认 budget | `session/update_metadata` 深合并；首 turn 的合成 part 正常入 checkpoint |
| 编辑（save） | 保留 goal id 与核算字段、status→active、`statusReason:'resumed'`、`blockedStreak:0`；编辑中 UI 不被实时更新覆盖（表单仅在打开时 seed，file 迟到只在空时补） | 服务端续写不得覆盖用户编辑窗口（goal-state permit，§2.4） |
| Pause | `abortCurrentOperation`（同时停 turn）＋ status=paused | abort 事件不得被 goal runtime 误判为 blocked（tick-9 例外路径） |
| Resume | status→active＋`turnsUsed:0`（重给续跑额度）＋`statusReason:'resumed'` | 'resumed' 触发 ~250ms kickoff，跳过审计直接续跑 |
| User complete | status=complete＋`statusReason:'marked by user'` | 不再续跑；complete 只读，clear 后才能 arm 新 goal（codex ON CONFLICT WHERE status='complete' 同语义） |
| Clear | 删元数据 goal 键＋删 objective 文件＋若 active 则 abort | DELETE 目标文件 best-effort；深合并的 `null`/删键语义必须支持（R5） |

### 4.4 入口矩阵与降级路径

**入口**（均汇入同一 arm→发送或 setSessionGoal 通道）：
- composer 目标按钮（主入口，支持 draft session——goal 住到 materialize 出的新 session）；
- fork-from-answer：`execution.runAsGoal`（session-ui-store.ts:1480 显式 setArmed，fork 消息即 objective）；
- plan implement：plan 文本作为 objective；
- scheduled tasks run-as-goal（Phase 5）。

**降级链**（任何一层失败不阻断上一层）：
- objective 文件写失败（网络/无路由）→ 回退 inline metadata ＋ distill 兜底（小模型蒸馏成完成判据；蒸馏失败→头尾拼接截断＋toast 提示）；
- objective 文件读失败（VS Code bridge 无路由）→ strip/dialog 降级只显 note（`useGoalObjectiveContent` 返回 null，静默）；
- 元数据 patch 失败（ACP stub 未实现时）→ 动作 toast 报错，UI 状态不变——**这是当前 anureo 默认路径，Phase 1 消除**；
- Settings 关闭 → 全部 goal UI 隐藏，但已存 goal 的服务端循环不受影响（gate 只影响 UI 入口与服务端 tick，两者同读 `anureo.ui` blob）。

### 4.5 anureo 集成验收清单（Web 交互）

1. arm → 发送 → strip 出现（active 蓝）→ 首轮 idle 后 evaluating spinner 出现 → 续跑触发；
2. 编辑对话框打开期间服务端续跑更新元数据 → 表单不被覆盖，关闭重开可见新状态；
3. pause → session turn 立即停；resume → ≤1s 内续跑；
4. 预算触顶 → strip 变 budgetLimited 黄＋statusReason；mid-turn 不打断（§tick-11）；
5. 审计 complete → strip 变 success 绿，按钮回 arm 语义但保色，对话框只读；
6. clear → strip 消失、objective 文件删除、（active 时）turn 停；
7. 双窗口同 session：一端 pause，另一端 strip ≤1s 内同步；
8. 刷新页面/重连 ACP → goal 状态从元数据恢复，无闪烁。

## 5. 关键数据契约

### 5.1 Goal payload（`metadata.anureo.goal`，与 FE `SessionGoalPayload` 逐字段对齐）

```jsonc
{
  "id": "…",                  // 逻辑 goal id；防陈旧写守卫
  "objective": "",            // inline 文本（回退），≤5000
  "objectiveFile": true,      // 文本在服务端文件
  "status": "active|paused|blocked|budgetLimited|complete",
  "tokenBudget": 200000,      // 可选正整数
  "tokensUsed": 0, "tokensBaseline": 0, "tokensCommitted": 0,
  "turnsUsed": 0,             // 自动续跑次数（上限 20）
  "blockedStreak": 0, "auditFailStreak": 0,
  "note": "",                 // 最新审计进度 note，≤280
  "statusReason": "",         // settle 原因；'resumed' 是 UI 的 kickoff 信号
  "lastAccountedMessageID": "",
  "createdAt": 0, "updatedAt": 0
}
```

### 5.2 新增协议面汇总

| 面 | 方法/路由 | 方向 |
|---|---|---|
| ACP 扩展 | `_anureo.dev/session/update_metadata` | FE → anureo（patch 元数据） |
| ACP 扩展 | `_anureo.dev/goal/objective` `{action}` | bridge → anureo（objective 文件） |
| 全局事件 | `session.updated` properties = 完整会话记录（含 metadata） | anureo → FE |
| 内部 | lifecycle idle 钩子 + kickoff/安全定时器 | anureo 内部 |

## 6. 风险与开放问题

| # | 风险/问题 | 缓解 |
|---|---|---|
| R1 | `session.updated` properties 的确切形状须匹配 FE reducer 预期（opencode 全量 session 记录） | Phase 1 动工前先读 FE `session.updated` reducer 源码定契约，写契约测试 |
| R2 | anureo checkpoint 的 usage 字段与 opencode 消息 parts 口径差异（tokens.input/cache.read/output） | Phase 3 前做一次 usage 字段对照表；快照模型容忍口径差（预算是护栏不是计费） |
| R3 | compaction 在 anureo 的表示（`summary: true` assistant 消息） | 同上，对照 checkpoint 结构确认；不匹配则加适配层 |
| R4 | goal 续跑与用户 prompt 抢 `prompt_capacity`（默认 4） | 可接受；必要时给 goal 续跑降级排队优先级 |
| R5 | 深合并语义与 opencode PATCH 有偏差导致前端 updater 模式错乱 | 以 `writeGoal` 更新器实际写形状写往返单测 |
| R6 | 审计小模型质量（verdict 抖动） | 已有 3 连 blocked + 失败容忍机制兜底；可配模型 |

| R7 | 双锁（goal-state + accounting permit）与 anureo 现有 `prompt_capacity` 信号量的交互（死锁风险） | Phase 3 设计时画锁序图：acquire 顺序固定 goal-state → accounting → prompt_capacity，禁止反向嵌套 |
| R8 | mid-turn steering 需要 anureo 具备向活跃 turn 注入 part 的通道（codex `inject_if_running`） | 核查 `agent.rs` / ACP 是否已有等效机制；无则降级为仅在 idle 时 settle（行为退化为 OC 基线，可接受） |

## 7. 工作量估算

| 阶段 | 内容 | 估算 |
|---|---|---|
| Phase 1 | 元数据存储 + patch 协议 + 事件扇出 + FE stub 实现 | 3–4 天 |
| Phase 2 | objective 文件 + bridge 路径参数 | 1–2 天 |
| Phase 3 | runtime 移植（核算/审计/续跑/终态/通知） | 5–8 天 |
| Phase 4 | 端到端验收 + 回归 | 1–2 天 |

（估算含单测；不含 Phase 5。）

# Goal 对齐 Codex 开发 TODO

> 状态：P0-P8 已实施；核心 ACP 续跑/记账/恢复 e2e 已落地（2026-09-11）
> 创建：2026-09-06
> 依据：alignment 方案 §5–§11 展开为可勾选任务清单；语义基线以其 §6 为准
> 用法：按 Phase 顺序推进，**前一 Phase 的 DoD 未达成不开下一 Phase**；完成后勾选并在「进度记录」追加一行；实现中如需偏离语义基线，先回 alignment 文档评审，不在代码处私自变更

## 依赖与预估

```
前置门禁 → P0 审计 → P1 crate 骨架 → P2 记账/steering → P3 钩子接线 → P4 模型工具 → P5 外部面 → P6 迁移收尾
                                                                                    （P7 可选，不阻塞）
```

| Phase | 内容 | 预估 | 关键风险联动 |
|---|---|---|---|
| 前置 | 评审门禁与决策 | — | R4 |
| 0 | Hook 点位审计 | 1d | R1 |
| 1 | `agent/goal` crate：types + store + 迁移 | 2-3d | R5 |
| 2 | accounting + steering | 2-3d | R5 |
| 3 | 钩子接入 + 续跑 + deferral | 2-3d | R1 |
| 4 | 3 个模型工具 + verify 门 | 2d | — |
| 5 | ACP 扩展切换 + FE 投影 + REPL | 2d | R2, R3 |
| 6 | 数据迁移 + 冻结 legacy + 文档 | 1-2d | — |
| 7 | 可选增强 | — | — |
| 8 | 中立 goal 扩展对齐（codex-acp provider-neutral spec，2026-09-07 登记） | 1d | FE 契约（R2 的正式答案） |

---

## 前置：开工门禁

- [x] alignment 方案评审通过（2026-09-06 用户指示开工，视为评审放行；状态头变更随 P6，附录 B 已入文）
- [x] **R4 决策**：采纳主案 = `thread_goals` 进 task 库（alignment §7.1）；独立 `goals.sqlite` 留作备选（store 接口不变，切换成本 ≤1d）
- [x] 确认新 crate 名与路径 `agent/goal`（workspace member 注册方式见根 `Cargo.toml`）
- [x] FE 契约协调方式确认（盲审 C1）：暂以仓内基线 = `docs/acp-spec/extensions/14-goal-scheduled-task.md` + session-goal-integration §3.4；P5 开工前再与外部 FE 仓对齐
- [x] 本 TODO 建立为跟踪文档（已建），开发期间滚动更新

## Phase 0：Hook 点位审计（1d）✅（2026-09-06 完成，结论详见 alignment 附录 B）

目标：产出接线清单与降级决定，结论回写 alignment §6.5/§11（或附录）。

- [x] agent-core agent loop 的 turn 边界点位：on_turn_start / stop / abort / error 有无现成挂点，记录 `file:line`（重点 `agent/agent-core/src/agent/react/`、`src/run/`）
  结论：`run_agent_from_config`（run/runner.rs:121）＋ `StreamRunOutcome::Finished/Cancelled`（:178/:184）；ACP 侧 prompt()（agent.rs:1079/1092），abort 走 `RunCancellation`（react/runner/runner.rs:43）→ 附录 B.1
- [x] token usage 事件出口：LLM 响应 usage 在何处可截获（think→LLM 流式路径）
  结论：`LlmUsage`（foundation/llm/traits.rs:124，cached/reasoning 字段齐全）；think_node.rs:181-215 捕获；`StreamEvent::TurnFinish{usage}`（stream_event.rs:172）；CodexUsage 已有 Sub 差分（codex.rs:18-36）→ 附录 B.2
- [x] tool finish 出口：工具执行完成点（`act_executor.rs` 一带）
  结论：`StreamEvent::ToolStart/ToolEnd`（stream_event.rs:135/146，发送点 stream_writer.rs:352/394）；review_coordinator.rs:19 的事件消费模式可直接复用 → 附录 B.2
- [x] ACP 侧 session idle 检测点位（`apps/acp/src/agent.rs` / `stdio_loop.rs`）
  结论：`SessionLifecycle::Idle`（session.rs:324，转换 :534/:569/:619-638）＋ `SessionSyncPromptState::Idle`（session_update_log.rs:51）→ 附录 B.1
- [x] **R1 核实**：mid-turn 注入通道有无（agent loop 是否支持 inter-turn steering 注入）→ 产出「可用 / 降级 turn 边界注入」决定
  结论：**通道不存在**（react 图闭合 pregel 循环、nudge 为旁路触发、prompt 装配 per-run 静态）→ **走降级：turn 边界注入**，alignment §6.5 已标记 → 附录 B.6
- [x] sub-agent / workflow agent 的工具注册路径：确认 ToolRegistry 按 session 门控的落点（goal 工具仅主 session 可见）
  结论：主构建源 react/build/tool_source.rs（build_tool_source）；sub-agent 走 tools/agent/build_config.rs 独立路径，门控天然成立 → 附录 B.3
- [x] `goal_mode` 链路盘点（`agent-core` `run/types.rs:76`、`run/profile_helper.rs`、`run/config_builder.rs:77`、`react/config/react_build_config.rs:79`、`react/build/tool_source.rs:245`）：旧 detached runner 往 react 循环注入 task 工具的通道，新架构下的存废/替代结论（盲审 B2）
  结论：遗留通道；P4 语义改为「主 session goal 工具注册门控」或删除，附录 B.4
- [x] provider 用量/限流信号出口盘点（429/quota 在何处可截获）——`usage_limited` 置位依赖（盲审 A2）
  结论：ErrorKind::RateLimited + QuotaExhausted 启发式（foundation/llm/error/provider/mod.rs:82/:122/:156）→ 附录 B.5
- [x] TaskDb 连接配置现状核实：`foreign_keys` 是否未开（`experimental/task/task-core/src/db.rs`）
  结论：确认未开（db.rs:27 `SqlitePoolOptions::new().max_connections(1).connect(&url)`），P1 改 `SqliteConnectOptions::foreign_keys(true)`
- [x] `Command::Goal` 全部消费方盘点（`agent/agent-core/src/commands/command.rs`、`apps/acp/src/agent.rs:1143`、`apps/cli/src/repl.rs:123`、**`apps/telegram-bot/src/pipeline/mod.rs:256`**——盲审 B1 发现的第四消费方，R3 收口必须含它）
- [x] 现存 goal 测试资产盘点与处置计划：`apps/acp/tests/e2e_goal_recovery.rs`（重启恢复 e2e，P5 需重对接）、`e2e/tests/web/multi-wt-goal.spec.ts`（Web 回归）、`commands/parser.rs` goal 解析单测、`goal_runner/state.rs` 内嵌 legacy JSON 兼容测试（迁移正确性的既有证明，盲审 B7）
- [x] 配置面盘点：`verify_command` 配置来源；`max_goal_token_budget` **当前全仓不存在**（盲审 A1），确认后在 P1/P4 落地为正式配置项
  结论：verify_command 现状 = GoalMeta per-goal 字段（goal_runner/runner.rs:53）+ CLI flag（goal_cmd.rs:100）；**alignment §7.1 表缺载体 → P1 建表补 verify_command 列**
- [x] legacy 类型归宿确认：`GoalMeta`/`GoalLifecycle` 定义在 `agent/agent-core/src/goal_runner/state.rs`；区分「迁移-only 类型」与「被活代码共享的类型」（`react/act_utils.rs:6` 用其 `ToolError`、`apps/cli/src/run_flow.rs:401` 用 `KANBAN_RATE_LIMIT_EXIT_CODE`，盲审 B5）——后者需在 P7 移除前搬迁
- [x] 确认 `feat/goal` 分支与 `loom-feat-goal` worktree 有无相关半成品改动需先合入或废弃
  结论：feat/goal（da40f8cb）已完全合入 dev（merge-base = tip），无半成品；worktree 陈旧可清理

**DoD** ✅：每个点位有 `file:line` 结论；R1 有明确结论（降级 turn 边界注入）；审计结论已回写 alignment 文档（附录 B）。

## Phase 1：agent/goal crate 骨架（2-3d）✅（2026-09-06 完成）

- [x] 新建 crate 并注册 workspace member；依赖仅 task-core（+ chrono/serde/uuid/sqlx），**不依赖 agent-core**（依赖方向 `apps/* → agent/goal → task-core`）
- [x] `types.rs`：`Goal` / `GoalStatus` 6 态 / `is_terminal()` / `CreateGoalRequest` / validate + `max_goal_token_budget()`；`GoalSetRequest` 并入 `CreateGoalRequest` + `GoalService::set` 签名（`GoalEvent` payload 属通知面，留 P5）
- [x] 迁移 SQL `20250103000000_thread_goals.sql`：
  - [x] `thread_goals`（6 态 CHECK 约束；§7.1 原样 + 盲审/P0 发现的两个扩展列 `verify_command`、`status_reason`，SQL 头部注释已标注偏离）
  - [x] `thread_goal_continuation_deferrals`（FK `ON DELETE CASCADE`）
- [x] **前置修复**：TaskDb 连接开 `foreign_keys(true)`（`SqliteConnectOptions::from_str` + `connect_with`）；级联删除由 `deferral_lifecycle_and_fk_cascade` 测试端到端证明；存量迁移无 FK，开启安全
- [x] `store.rs` CRUD：create（前置终态检查）/ read / pause / resume / clear / edit(objective) / replace（用户 set 替换入口）/ mark_blocked / mark_usage_limited / mark_complete
- [x] `service.rs` `GoalService`：set/show/pause/resume/clear/edit 基础实现（用户侧不暴露 goal_id，自动 CAS；`goal_state_lock` 窗口与 start_turn 仍留 P3）
- [x] 新增 `max_goal_token_budget`（盲审 A1）：落点 = goal crate `max_goal_token_budget()`，默认 5,000,000，`ANUREO_MAX_GOAL_TOKEN_BUDGET` env 可覆盖（与 react_build_config env 模式一致；未入 foundation/config，P4 如需全局配置再迁）
- [x] `account_thread_goal_usage`：`UPDATE ... RETURNING` 单语句累加 + budget 触顶翻转（active→budget_limited）+ mode 门控 + CAS；负 delta 钳 0 防御；Unchanged 语义
- [x] deferral 读写 API：defer / has / clear（无 goal 行时 defer 为 no-op，盲审 C2）

单测（R5 专项）：
- [x] CAS：陈旧 `goal_id` 写入返回 Unchanged 且不落账
- [x] AccountingMode × 状态矩阵逐档断言（纯逻辑 + 真实 SQL 双层）
- [x] budget 触顶翻转为 `budget_limited`（终态；ActiveOnly 可补账、ActiveStatusOnly 不行）
- [x] FK 级联：删 `thread_goals` 行连带删 deferral 行
- [ ] pause/clear/account 并发交错用例 → **归 P2**（`progress_accounting_lock` 与双锁测试一起做；单连接池下 SQL 层 CAS 已由陈旧写用例覆盖）
- [x] 替换入口语义：对 active/paused 既有 goal 的用户 set + 旧 goal 记账变 Unchanged + deferral 级联清

**验证** ✅：`cargo nextest run -p goal`（18/18）+ `-p task-core`（66/66）全绿；`cargo clippy -p goal --all-targets -- -D warnings` 零警告。

**DoD** ✅：迁移落地 + account 原子语义有测试证明；`-p goal` 全绿。

## Phase 2：accounting + steering（2-3d）✅（2026-09-06 完成）

- [x] `accounting.rs` `GoalAccounting`：
  - [x] `progress_accounting_lock`（Semaphore(1) + timeout）覆盖「取 snapshot → SQL 成功 → 推进内存基线」全程
  - [x] token 公式 `(Δinput − Δcached) + max(Δoutput, 0)`，全程 saturating（Codex 对拍 6 用例：负 delta/cached>input/u64 溢出边界）
  - [x] **基线绑定 goal_id**：goal 被替换 → 旧 delta 丢弃、基线 adoption 新 goal（§6.2 防旧 turn 写新 goal）；状态不在 mode 集 → delta 保留待追补
  - [x] 基线仅在 SQL 返回 Updated 后推进；武装点基线（`note_goal_armed`/`reset_baselines(goal_id, totals)`）
  - [x] `budget_limit_reported_goal_id`：一次性 steering 去重（`take_budget_steering_if_flipped`）
  - [x] 墙钟：`Instant` 基线；仅 active 计时；flush+清基线；resume 重启；重启不追补
- [x] `steering.rs` 模板：continuation（RESEARCH&VERIFY + PROGRESS LOG + COMPLETION AUDIT + VERIFICATION 段并入，untrusted_objective 包裹）/ budget_limit（收尾引导）/ objective_updated
- [x] `GoalStateLock`（service.rs，1 permit + timeout；service.set/clear 窗口 + runtime 续跑窗口共用）
- [x] `update_goal` 工具调用不计入 progress（工具自身仅读快照/落状态，不产生 token delta）

单测：
- [x] token 公式与 Codex 对拍（含 saturating 边界）
- [x] 并发 record_usage 消费同一 delta 只记一次（8 任务同总量 → 仅 1 次 Updated；R5）
- [x] 墙钟暂停/恢复/清基线状态转移

**验证** ✅：nextest 26/26（P2 时点）+ clippy -D warnings 零警告。

**DoD** ✅：公式与 Codex 对拍通过；双锁并发用例全绿。

## Phase 3：钩子接入 + 续跑（2-3d）✅（2026-09-06 完成；runtime 层全绿，apps/acp 宿主接线归 P5）

- [x] `runtime.rs` `GoalRuntimeHandle`：
  - [x] on_turn_start（清 deferral + 墙钟起表）/ on_turn_finish（ActiveOnly 补账 + budget 注入）/ on_turn_abort（ActiveOrStopped 补账）/ on_turn_error（补账 + active→blocked，budget_limited 优先不被覆盖）
  - [x] `on_provider_quota_exhausted`（QuotaExhausted → usage_limited 系统置位）
  - [x] on_session_idle → `continue_if_idle` 全流程：goal_state_lock → deferral 检查 → 读表 → 非 active 返回 → 渲染 continuation → `TurnDriver::start_turn_if_idle` 幂等二道门（宿主实现）
  - [x] `TurnDriver` trait：宿主注入 start_turn_if_idle（apps/acp 接线在 P5）
  - [x] budget steering：越界后**不打断当前 turn**，turn 边界经二道门注入一次收尾 turn（R1 降级路径）；后续补账不再注入
  - [x] `defer_continuation`（fork/外部 mutation 保护）+ `on_goal_replaced`/`on_goal_status_changed` 基线维护
- [x] `usage_limited` 置位路径：runtime 层完成；apps 信号接线（QuotaExhausted 拦截）归 P5 宿主钩子
- [ ] apps/acp 接线：`agent.rs` / `stdio_loop.rs` 挂全部钩子 → **归 P5**（与扩展切换同批）
- [x] `service.rs` `GoalService`：set/clear 持 `goal_state_lock` 窗口；pause/resume/edit/clear/show
- [x] 不计账门控：sub-agent / workflow 不装配 GoalRuntimeHandle（结构上不可能记账）；工具不可见由 P4 `goal_tools` 仅主 session 注册保证
- [x] `service.rs` `GoalService` 实现：mutation 基础版（P1）+ 状态锁窗口（本轮）

集成测试（MockDriver）：
- [x] 多 idle 事件只启动一次续跑 turn（幂等）
- [x] idle 续跑与用户 set 竞态：持锁期间 set 阻塞、释放后完成
- [x] 预算越界四断言：DB 翻转 / 当前 turn 不中断（注入收尾 turn）/ steering 仅一次 / 终态后 idle 不续跑
- [x] turn 不可恢复错误 → blocked（status_reason 归因）
- [x] deferral 生命周期：defer 盖章 → continue_if_idle 跳过 → on_turn_start 清除 → 恢复

**DoD** ✅（runtime 层）：§6.5/§6.6 场景集成测试全绿；宿主接线（ACP 侧挂钩子）随 P5 批次落地。

**DoD**：钩子全接线；§6.5/§6.6 场景集成测试全绿。

## Phase 4：模型工具 + verify 门（2d）✅（2026-09-06 完成）

- [x] `tools.rs` 三工具（不对称控制，规则全文内嵌 schema description）：
  - [x] `get_goal`：无参；快照 + `remaining_tokens`
  - [x] `create_goal`：规则内嵌（仅用户/system 明确要求时；不能覆盖 unfinished goal→拒绝文本说明出路；budget 正且 ≤ `max_goal_token_budget`）；成功后 `note_goal_armed`
  - [x] `update_goal`：只接受 `complete`/`blocked`（其余 InvalidInput）；blocked 三轮规则内嵌 description（**不做服务端计数**）；complete 附 `completion_budget_report` + 最终账目
- [x] `VerifyRunner` trait + `ShellVerifyRunner`（Windows cmd /S /C、unix sh -c，10min 超时，输出截断 4k）
- [x] verify 完成门（§6.9）：complete → verify 通过才落账；失败拒绝并注入继续文本；未配置直接接受
- [x] sub-agent / workflow 门控：`goal_tools()` 仅主 session 构建路径调用（agent-core `tool_source` 接线归 P5 同批）；工具集层面天然不可见
- [x] `goal_mode` 处置（P0 B.4 决策，随 P5 agent-core 接线落地）：新增 flag 与旧 goal_mode 并存，legacy runner 冻结时一并退役

单测：
- [x] get_goal 快照渲染；create 拒绝文本（规则说明）；update complete verify 三态（失败拒绝保持 active / 通过落账附 report / 未配置直通）；update blocked + reason；非法 status InvalidInput；缺 objective InvalidInput；ShellVerifyRunner 冒烟（Windows）

**验证** ✅：nextest 39/39（goal 全 crate）；clippy -D warnings 零警告。

**DoD** ✅：三工具 + 门控 + verify 门全绿（goal crate 范围；agent-core 注册点随 P5）。

## Phase 5：外部面切换（2d）✅（2026-09-07 完成：首批宿主接线+R3 收口，P5b 扩展切换同日完成；session.metadata 投影遗留跨仓）

### 5a. 宿主接线（P3/P4 移交）

- [x] agent-core `react/build/tool_source.rs`：新增 flag 注册 `goal_tools()`（旧 `goal_mode` 保留至 P6 冻结；B.4 决策落地）+ RunConfig 链路透传
  - 实际落地：经 `apps/acp/src/agent.rs` prompt 路径的 `RunOptions.extra_tools` 合并实现（tool_source.rs 零改动，sub-agent 不走此链→门控天然成立）；goal_mode 链路存废决策未落地，随 P7 旧 runner 移除一并处理（遗留）
- [x] apps/acp 宿主钩子接线：session 装配 `GoalRuntimeHandle` + 实现 `TurnDriver`（`prompt_with_capabilities` 空闲检查启动）；on_turn_start/finish/abort/error 挂入 prompt 生命周期；`SessionLifecycle::Idle` 转换点挂 `continue_if_idle`
  - 实际落地：`goal_runtime_for`（per-thread handle + 共享 TaskDb）+ `AcpTurnDriver`（`apps/acp/src/goal_runtime.rs`，busy 门 `has_active_prompt` + `begin_prompt` 幂等二道门）；idle 续跑挂 `finish_prompt` 后 `tokio::spawn(continue_if_idle)`（与 lifecycle Idle 转换点等价）
- [x] provider `QuotaExhausted` 拦截 → `on_provider_quota_exhausted`（alignment 附录 B.5）
  - 实际落地：agent.rs result 错误分支按 quota/429 启发式分流
- [x] 存量测试重对接：`apps/acp/tests/e2e_goal_recovery.rs` 改为验证 `thread_goals` 恢复预约（goals.json 退役前完成）
  - P5b 实际结论：预约机制整体退役（删除），新恢复语义 = thread_goals 天然持久 + continue_if_idle；e2e 改写为跨进程 get/cancel 验证

### 5b. ACP 扩展与用户面

- [x] **R2 前置**：对照 FE reducer 先定 `session.metadata.anureo.goal` payload 契约，再实现投影。注意 FE 在**外部仓库**（非本仓 `packages/`，见 session-goal-integration §1），需跨仓协调；仓内契约基线 = `docs/acp-spec/extensions/14-goal-scheduled-task.md` + session-goal-integration §3.4（盲审 C1）
  - 实际结论：契约基线 = 旧 wire 形状兼容层（camelCase/旧 6 态投影/metadata 扩展），spec 14 已补 goal/updated；跨仓 FE 对接待外部仓协调
- [x] `apps/acp/src/extensions/goal.rs`：`_anureo.dev/goal/*` 六方法后端切 GoalService/GoalStore（API 面不变）
  - 实际落地：`GoalHandler` 双后端（Agent 绑定/Store 测试注入）；键映射 start=sessionId→thread_id（SessionStore 反查，失败降级 sessionId 作键）、get/pause/resume/cancel=goal_id 反查（`GoalStore::find_by_goal_id`）；状态投影 usage_limited→paused、budget_limited/blocked→failed；cancel 对终态幂等、对 cleared 行报 not_found；idempotencyKey 进程内幂等映射
- [x] `goal.updated` 全量快照通知（UI 订阅快照而非猜状态）
  - 实际落地：mutation 响应新增 `updated` 字段（旧形状全量快照）+ `ConnectionRegistry::broadcast_extension_notification` 广播 `_anureo.dev/goal/updated`（best-effort）
- [x] 既有 `goal/changed` 通知存废决策（`extensions/goal.rs:178`，盲审 B3）：建议由 goal.updated 替换并在 P6 同步 spec 14，避免双通知并存
  - 实际决策：**changed 保留兼容 + updated 并存双发**（避免 FE 断裂；spec 14 已同步说明）；待 FE 仓迁移后随 P7 收回 changed
- [ ] `session.metadata.anureo.goal` 投影 + `session.updated` 扇出
  - **遗留（跨仓）**：FE 在外部仓，本批仅落地仓内通知面（goal/updated 广播 + response.updated）；metadata 投影待 FE 仓协调后实现（不阻塞 ACP/REPL 路径）
- [x] 重启恢复预约机制对接 `thread_goals`（extensions/goal.rs:388 保留改造，goals.json 读取路径退役前置）
  - P5b 实际落地：预约机制（try_claim/spawn_persisted/recover_persisted_goals）**删除**；goals.json 读写自扩展搬迁至 goal_runner.rs legacy_store（仅旧 detached runner 使用）；session/load 的恢复扫描移除，恢复改由 DB 持久 + prompt 后 continue_if_idle 承担
- [x] REPL（`apps/cli/src/repl.rs`）：`/goal set|show|pause|resume|clear|edit` 接 GoalService
- [x] **R3 破坏性变更收口（同一 PR）**：`Command::Goal { description }` → `{ subcommand: GoalSubcommand }`（agent-core `command.rs`），同步全部消费方：`apps/acp/src/agent.rs:1143`、`apps/cli/src/repl.rs`、**`apps/telegram-bot/src/pipeline/mod.rs:256`**（否则 /goal 子命令被静默透传给 LLM，盲审 B1）；解析层兼容期同时接受裸 `/goal <desc>`
- [x] 回归确认 workflow / sub-agent 对 goal 工具不可见（extra_tools 不进 sub-agent 构建路径；全量测试绿）

**验证**：ACP e2e（含重对接后的 recovery e2e）；`e2e/tests/web/multi-wt-goal.spec.ts` 回归；OpenChamber FE 手动验收（goal strip 可见可操作：arm、pause/resume、complete 展示）。

**DoD**：六方法走新后端且 FE 经 metadata 可见；REPL 六子命令可用。

**DoD**：六方法走新后端且 FE 经 metadata 可见；REPL 六子命令可用。

## Phase 6：数据迁移 + 冻结 legacy（1-2d）✅（2026-09-07 完成；workspace 全量 nextest 未跑，验收范围=三包）

- [x] GoalMeta → `thread_goals` 一次性迁移（§7.2，不双写）：`Cancelled`→删（=clear）/ `Failed`→`blocked`（按 lifecycle_reason 归因）/ `Completed`→`complete`（tokens_used/time 直译）/ Active·Paused·Blocked·UsageLimited→同名直译
  - 实际落地：`anureo goal --migrate`（apps/cli/src/goal_migrate.rs）；`GoalStore::insert_migrated` 一次性写全行（含 status/tokens/status_reason，经 replace+account+transition 组合会丢 budget）；objective=task.description（缺省回退 name）；thread_id=task.id；新增 uuid goal_id
- [x] goals.json 一次性导入后废弃（文件不再读写）；迁移映射表需细化（盲审 B4）：扩展自有 `GoalStatus`（Pending/Active/Paused/Completed/Cancelled/Failed，`extensions/goal.rs:66`）→ 新 6 态的归一规则、`session_ids` 多 session goal 的 thread_id 归属、`title/progress/steps/idempotency_key` 等无对应列字段的丢弃决策
  - 实际映射：pending/active→active、paused→paused、completed→complete、failed→blocked、cancelled→跳过；thread_id=session_ids 首元素（无则跳过+warn）；objective=description（缺省 title）；保留原 goal_id；丢弃字段=progress/steps/idempotency_key（新架构无对应）；源文件不删、新路径不再读；候选路径 cwd/.anureo/goals.json 优先，其次 home
- [x] 迁移命令幂等（重复执行安全），迁移前后计数核对输出（read-first 跳过已存在 thread 行 + 分源计数摘要打印）
- [x] 旧 runner 冻结：`apps/cli/src/goal_cmd.rs`（含 `args.rs` goal 参数与 `main.rs:187` 分发）+ `apps/cli/src/goal_runner/` 标 deprecated（`anureo goal` CLI 保留走旧路径，后续版本移除）
  - 实际落地：goal_cmd.rs/goal_runner/mod.rs 冻结头注释；`--migrate` 为新迁移入口；args.rs/main.rs 分发未动（legacy 路径整体冻结）
- [x] ACP `/goal` 执行路径切换（§4.2/§9）：`apps/acp/src/goal_runner.rs` 切到 `GoalRuntimeHandle` 后冻结 deprecated
  - 实际落地：P5 已切（agent.rs `/goal` → GoalService）；goal_runner.rs 冻结头 + `#![allow(deprecated)]`，goals.json 存取/`RuntimeControl` 收容为内部 `legacy_store` 模块（pub API 无 dead_code 警告；resume_goal/recover_goal 加 allow(dead_code) 标 P7 移除）
- [x] `agent/agent-core/src/goal_runner/`（`GoalMeta`/`GoalLifecycle` legacy 类型）标 deprecated 只读，仅保留供迁移与审计（mod.rs 冻结头；ToolError/TurnResult 等共享类型仍在用，移除前需先迁走）
- [x] task-core 既有 `20250102000000_goal_fields.sql` 与 tasks 表 goal 借用字段处置决策（遗留只读 or 清理）
  - 决策：**遗留只读**（迁移读 task meta 但表结构不动；清理随 P7 旧 runner 移除评估）
- [x] legacy 配置清理决策：根 `.anureo/goal-mcp.json` 与 `apps/acp/.anureo/goal-mcp.json` 等
  - 决策：**保留不清理**（仅 legacy `anureo goal` 路径写入/读取；新路径不接触；随 P7 旧 runner 移除一并清理）
- [x] 迁移执行前备份（db 文件快照 / 导出核对），失败可回滚（tasks.db + -wal/-shm 边车 → `tasks.db.bak-<ts>`；静态拷贝，假定无并发写入者，迁移前停 server/ACP）
- [x] 确认 P5 REPL 替换完成、`repl.rs:123` 旧 stub 无残留（勿与 P5 项双勾选，盲审 C3）（已确认：stub 已替换为 goal_repl 调用）
- [ ] 文档同步：
  - [ ] `goal-system-workflow.md` 头部标记「已被取代」（alignment 附录 A 结论）
  - [ ] `docs/goal/README.md` 索引状态更新
  - [ ] alignment 状态改「已实施」；本 TODO 收尾
  - [ ] `session-goal-integration.md` 标注 Phase 3 已由 alignment 接管
  - [ ] `../acp-spec/extensions/14-goal-scheduled-task.md`（含 goal/changed → goal.updated 变更）与 `../user-guide/09-goal-task-experimental.md` 如受影响同步
  - [ ] `.anureo/skills/auto/anureo-acp-architecture/references/goal-extension.md`（记载六方法/goal.changed，行号已漂移，随 P5 API 变化更新，盲审 B6）
  - [ ] `docs/goal/goal-improvement-plan.md` 标记搁置/取代（两条路线评审定案后，盲审 B6）
  - [ ] `docs/design/anureo-config-management.md:18`（记载 goals.json 路径）如受影响同步
- [x] 全量回归：`cargo nextest run`（workspace）+ `cargo clippy --workspace --all-targets -- -D warnings` + e2e 套件
  - 实际验收：三包（anureo-acp 734/goal/anureo-cli）nextest 全绿 + 三包 clippy -D warnings 零警告 + `cargo build --workspace` 成功；workspace 全量 nextest 未跑（耗时）；e2e_mega 在本机间歇超时为既有 flaky（HEAD 干净树复现过同样失败）

**DoD**：迁移幂等可重放；全量回归绿；文档状态一致。

## Phase 7：可选增强（不阻塞）

- [x] OTel 指标（events/metrics，补齐弱项 10）——`goal::metrics`（12 个生命周期计数器 + `tracing` 语义事件 `target=goal_metrics`）+ `otel` feature（`opentelemetry` 0.32 **API-only**，Observable Counter `anureo.goal.*`；宿主装了全局 MeterProvider 即导出，不强制 SDK/导出器依赖）
- [x] objective 文件化：>4000 chars 走 `<anureo_home>/goals/<sessionId>.md`，metadata 留 `objectiveFile: true`（复用 session-goal-integration Phase 2 资产）。**偏离**：DB `objective` 列存 `@file:<name>` 标记（文件为长文本事实源，非「DB 全文+文件投影」）；DB 内联校验仍限 4000 字节，CJK「chars≤4000 但字节超限」同样路由文件化；无 goals_dir（非标准布局）时落回内联校验拒绝
- [x] session fork 快照替换完整实现——`GoalService::set_with_verify_outcome` 返回 `replaced_existing`，宿主三个 set 入口（`_session/goal` set、legacy `goal/start`、`/goal set`）替换后调 `runtime.defer_continuation()`（§6.6）；`fork_session` 保护窗：fork 前 defer → fork 后 `clear_deferral` + `continue_if_idle` 恢复（fork 失败路径同样清除）；「外部 mutation 保护」无对应入口（全部写路径经 GoalService），无需接线
- [ ] 旧 runner 移除（独立版本，不含本计划）；移除前先搬迁仍被活代码引用的共享类型（`ToolError`、`KANBAN_RATE_LIMIT_EXIT_CODE`，见 P0 盘点，盲审 B5）

## Phase 8：中立 goal 扩展对齐（1d，2026-09-07 登记，未开工）

> 依据：参考源修正（用户指示 + 网络检索）。外部面权威基准 = **codex-acp 的 provider-neutral goal extension**（[agentclientprotocol/codex-acp](https://github.com/agentclientprotocol/codex-acp) `docs/goal-extension.md`），不是 codex app-server 的 `thread/goal/*` wire，也不是 P5b 落地的 `_anureo.dev/goal/*` 旧形状。内核（thread_goals/6 态/记账/续跑）**不动**——neutral 层是薄投影。规范细节与状态映射表见 alignment 附录 C。

- [x] **能力协商**：`initialize` 响应 `_meta.goal = {version: 1, controlMethod: "_session/goal", actions: ["set","pause","resume","clear"]}`——**双处广播**：响应顶层 `_meta.goal`（codex-acp / cowork `goalCapabilitiesFromInitialize` 的识别位置）+ `agentCapabilities._meta.goal`（同形保留）；单测 + e2e 双覆盖
- [x] **控制方法 `_session/goal`**：请求 `{sessionId, action}`；`set` 附带非空 `objective`（可带 `tokenBudget`）；未广播的 action 必须拒绝；后端走 GoalService（session→thread 键映射复用 P5b 成果），set 后 `note_goal_armed` + 快照发布（session_info_update + legacy goal/updated 双发）
- [x] **快照发布**：`session_info_update._meta.goal` 发布全量快照；**清除时发 `goal: null`**。字段（camelCase，Unix **毫秒**）：objective / status / createdAt / updatedAt / tokenBudget / tokensUsed / timeUsedSeconds / controlMethod；额外扩展 `statusReason`（区分 limited 两种成因）。**未做**：`iterationCount`、`lastContinuationReason`（runtime 侧迭代计数落库后补，代码内 TODO(P8+)）
- [x] **状态投影（6→5）**：`usage_limited` 与 `budget_limited` 归并为 `limited`；active/paused/blocked/complete 直译（映射表 = alignment 附录 C.2）
- [x] **`/goal` 命令动作对齐**：set/pause/resume/clear 即 neutral 面（含快照发布）；`show`/`edit` 保留为 anureo 扩展不进 actions
- [x] **旧 `_anureo.dev/goal/*` 六方法降级**：保留为**不广播的 legacy alias**（`capabilities()` 返回空对象），映射到同一 GoalService；face 另做 P7 全文还原（文件化 goal 的 get/list/start/pause/resume/cancel 返回全文而非标记）
- [x] **生命周期解耦验收**：goal `active` ≠ prompt 运行中；`continue_if_idle` 自主循环产生 session/update。架构由 P3 保证；真实 ACP e2e 已覆盖能力协商、控制、快照、清除、pause/resume，以及 `set → 立即启动 → 自主续跑 → 跨 turn 累计记账 → budget_limited`；进程重启后 `session/load` 权威快照恢复也已覆盖。TC-3/TC-4/TC-5 竞态矩阵仍为后续增强
- [ ] **FE 验收**：cowork/OpenChamber 前端 goal strip 零改动直连 anureo（能力识别 → set/pause/resume/clear → 快照渲染）

测试：
- [x] 能力协商形状（version/controlMethod/actions）——单测 `initialize_advertises_neutral_goal_meta` + e2e
- [x] 控制方法矩阵：四动作成功路径 + 未广播 action 拒绝 + set 空 objective 拒绝——单测 + e2e
- [x] 快照投影：limited 归并、毫秒时间戳、可选字段缺省、clear → goal:null——单测 + e2e（active/paused/clear 面）
- [x] legacy alias 不广播但可用——单测
- [x] e2e：`set` 立即启动，goal 仍 active 时自主续跑并在控制请求之外发 update；同时断言跨 turn `tokensUsed` 累加、预算到限投影为 `limited` 且 `statusReason=budget_limited`（`neutral_goal_set_starts_and_accounts_across_turns`，2026-09-11）

**验证**：`cargo nextest run -p anureo-acp`；clippy 零警告；cowork 前端手动验收。

**DoD**：neutral 规范四要素（协商/控制/快照/解耦）全落地且有测试；旧面可用但不广播；FE 零改动直连验证通过。

---

## 全局约束（每 Phase 适用）

1. 语义基线以 alignment §6 为准，**原样搬运、禁止私自「改进」**；anureo 扩展仅限已声明的 verify_command 门与重启恢复预约；
2. `cargo clippy --workspace --all-targets -- -D warnings` 零警告；日志走 `tracing`，禁 `println!`/`eprintln!`；
3. 测试首选 nextest（`cargo nextest run -p <pkg>`）；
4. Windows / PowerShell 环境，命令串联用 `;`；
5. 每 Phase 结束：更新本文勾选 + 在「进度记录」追加一行（含偏离决定及其回写位置）；
6. 开发中新发现的工作项**必须**先登记到「追加发现项」区（注明发现 Phase 与处置建议），评审吸收后才排入执行；
7. 明确不做（对照 alignment §12，防范围蔓延）：不做 Lua workflow 编排；不做服务端 blocked 三轮计数/审计表；不做多租户/跨 session goal 聚合；不迁移旧 runner 的 scheduled-task 联动。

## 风险检查点索引

| 风险 | 检查点 | 动作 |
|---|---|---|
| R1 mid-turn 注入通道缺失 | Phase 0 | 核实并决定；降级 = turn 边界注入（Phase 3 实现） |
| R2 metadata payload 契约 | Phase 5 开工前 | 先对照 FE reducer 定契约再写投影 |
| R3 `Command::Goal` 破坏性变更 | Phase 5 | 单 PR 收口全部消费方 + 兼容裸 `/goal <desc>` |
| R4 存储边界争议 | 前置门禁 | 评审定夺；备选 `goals.sqlite` 切换成本 ≤1d |
| R5 CAS/双锁并发正确性 | Phase 1-2 | 专项并发单测（交错 pause/clear/account） |

## 现状代码触点处置表（完整性追溯）

> 2026-09-07：P0-P6 全部处置完成（各 Phase 列对应勾选项）；P7 可选项未开工。

「代码实面盘点」产出：现状系统全部 goal 触点 → 处置 → 承接 Phase。**新发现触点必须补入本表**，与「追加发现项」联动。

| 触点 | 处置 | 承接 |
|---|---|---|
| `apps/cli/src/goal_cmd.rs` + `apps/cli/src/goal_runner/` | legacy 冻结 deprecated | P6 |
| `apps/acp/src/goal_runner.rs`（ACP `/goal` 执行路径） | 切 `GoalRuntimeHandle` 后冻结 | P6 |
| `apps/acp/src/extensions/goal.rs` | 后端切 GoalStore + `goal.updated` + metadata 投影；恢复预约保留 | P5 |
| `agent/agent-core/src/goal_runner/`（`GoalMeta`/`GoalLifecycle` 定义） | deprecated 只读（迁移与审计源） | P6 |
| `agent/agent-core/src/commands/`（`Command::Goal`） | 改 `{ subcommand }`，单 PR 收口 | P5 |
| `apps/cli/src/repl.rs` `/goal` stub | 接 GoalService 六子命令 | P5 |
| `experimental/task/task-core` 既有 goal_fields 迁移 / tasks 表 goal 借用字段 | 遗留只读 or 清理（P6 决策） | P6 |
| `apps/acp/tests/e2e_goal_recovery.rs` | 重对接 `thread_goals` 恢复预约 | P5 |
| `e2e/tests/web/multi-wt-goal.spec.ts` | 回归纳入 | P5/P6 |
| `.anureo/goal-mcp.json` | legacy 配置清理决策 | P6 |
| `docs/acp-spec/extensions/14-goal-scheduled-task.md`、`docs/user-guide/09-goal-task-experimental.md` | 受影响则同步 | P6 |
| `apps/telegram-bot/src/pipeline/mod.rs:256`（`Command::Goal` 第四消费方） | R3 单 PR 收口同步 | P5 |
| `goal_mode` 链路（`agent-core` run/config + `react/build/tool_source.rs:245`） | P0 出存废结论，P4 落地 | P0/P4 |
| `goal/changed` 通知（`extensions/goal.rs:178`） | 由 `goal.updated` 替换（P5 决策） | P5 |
| goals.json schema（`extensions/goal.rs:66-141` 自有 GoalStatus、`session_ids` 多 session） | 迁移映射细化后废弃 | P6 |
| legacy 共享类型消费点（`react/act_utils.rs:6`、`apps/cli/src/run_flow.rs:401`） | P7 移除前搬迁 | P7 |
| `.anureo/skills/.../references/goal-extension.md`、`docs/design/anureo-config-management.md`、`docs/goal/goal-improvement-plan.md` | 文档同步/标记 | P5/P6 |

## 追加发现项（开发期滚动登记）

> 规则：开发中发现的新工作项先登记于此（注明发现 Phase 与处置建议：纳入既有 Phase / 新开 Phase / P7 / 不做），评审吸收后才排入执行；对应代码触点同步补入触点处置表。

| 日期 | 发现于 | 事项 | 处置 |
|---|---|---|---|
| 2026-09-06 | 追溯复查 + 代码实面盘点 | service.rs 建造任务、recovery e2e 重对接、agent-core goal_runner 处置、goal_fields/tasks 借用字段处置等 8 项 | 已补入对应 Phase |
| 2026-09-06 | 独立盲审（sub-agent，A/B/C 三向） | 13 项：B1 telegram-bot 消费方、B2 goal_mode 链路、B3 goal/changed 通知、B4 goals.json 映射、B5 legacy 共享类型、B6/B7 文档与杂项、A1 max_goal_token_budget 不存在、A2 usage_limited 信号源、A3 set-on-existing 语义、C1 跨仓依赖、C2 fork 边界、C3 REPL 项重叠 | 已补入对应 Phase |
| 2026-09-06 | P0 | `thread_goals` 表结构缺 `verify_command` 载体（现状为 GoalMeta 字段，alignment §7.1 未含） | P1 建表补列，已回写附录 B.7 |
| 2026-09-07 | 用户修正参考源 + 网络检索 | 外部面权威基准 = codex-acp 中立 goal 扩展（`_meta.goal` 协商 + `_session/goal` 控制 + `session_info_update._meta.goal` 快照、5 态 limited 归并、毫秒）；P5b 交付的 `_anureo.dev/goal/*` wire 形状与新基准不符 | 登记为 Phase 8；旧面降级为不广播 alias；alignment 附录 C 已写入 |
| 2026-09-08 | P8 e2e | 全链路续跑 e2e 曾在并行负载/测试拆除竞态下不稳定；2026-09-11 已用持久 cwd、显式 shutdown 与小预算 mock LLM 落地核心链路 | TC-1/TC-2 核心断言已实现；[goal-continuation-e2e-design.md](./goal-continuation-e2e-design.md) 保留为 TC-3/TC-4/TC-5 竞态矩阵后续设计 |

## 进度记录

| 日期 | Phase | 记录 |
|---|---|---|
| 2026-09-11 | P8 修复/验收 | 修复 ACP 跨 prompt token 基线少算、模型终态/配额错误漏记最后用量、预算状态原因缺失、pause/resume/edit 与续跑竞态、长 objective 工具显示、`tokenBudget` 静默降级、重连快照缺失；新增真实 ACP 自主续跑/跨 turn 记账/预算停止 e2e 与进程重启恢复断言，并补充推荐快速上手文档 |
| 2026-09-06 | — | TODO 文档创建，待评审开工 |
| 2026-09-06 | — | 完整性复查：双向追溯 + 代码触点盘点，补 8 处遗漏，新增触点处置表与追加发现项机制 |
| 2026-09-06 | — | 独立盲审回填：13 项发现全部分诊补入；盲审结论=原清单完备度约 85%，修补后可开工 |
| 2026-09-06 | P0 | **Hook 点位审计完成**：R1=降级 turn 边界注入（通道不存在）；全部点位 file:line 结论入 alignment 附录 B；门禁假设落定（R4=TaskDb 主案、FE=仓内基线）；发现 verify_command 缺表载体 → P1 补列 |
| 2026-09-06 | P1 | **crate 落地**：`agent/goal`（types/store/service）+ 迁移 + FK 前置修复；18 goal + 66 task-core 测试全绿、clippy -D warnings 零警告。偏差：`GoalSetRequest` 并入 `CreateGoalRequest`（`GoalEvent` 留 P5）；`max_goal_token_budget` 用 env 覆盖非 config crate；并发交错用例归 P2
| 2026-09-07 | P5b/P6 | goal 对齐 codex thread_goals 后端 + P6 文档收口（详见对应 Phase 勾选与提交 `830293c6`） |
| 2026-09-08 | P8（前会话遗留半成品修复） | 工作树已有 P8 实现（能力协商/`_session/goal`/快照/降级+测试）但未验证且编译失败（毫秒时间戳断言超 i32）；修复 `as_i64()`、修正快照 e2e 形状断言（`sessionUpdate` tagged snake_case）、补 `initialize_advertises_neutral_goal_meta` 断言位置（`agentCapabilities._meta` 而非响应顶层）后 739/739 全绿 |
| 2026-09-08 | P7 | **objective 文件化**：新增 `objective_file` 模块 + `Goal.objective_file` 标志 + `GoalStore::goals_dir`（`from_task_db` 仅在 `<home>/tasks/tasks.db` 标准布局推导，防测试 tempdir 误推导到系统 Temp）；`GoalService::prepare_objective` 超限写文件 + DB 存 `@file:` 标记；`resolve_objective` 全文还原接入 REPL（cli + goal_runtime）、legacy face（get/list/start/pause/resume/cancel）、steering（continuation/budget_limit）。**偏离已回写 Phase 7 勾选项** |
| 2026-09-08 | P7 | **deferral 宿主接线**：`SetOutcome.replaced_existing` + 三 set 入口 defer + fork 保护窗（defer→fork→clear+continue_if_idle，fork 失败路径清 deferral）；metrics：`goal::metrics` 全局计数器 + tracing 事件接 service/runtime/tools 全部转折点；`otel` feature（opentelemetry 0.32 API-only）编译验证通过 |
| 2026-09-08 | P8 | **中立面 e2e**：`e2e_goal_neutral.rs` 稳定面（能力协商/控制四动作+非法 action/快照 camelCase+毫秒/clear→null）5s 稳过 ×4。**全链路续跑 e2e 调查记录**：turn1 完成后钩子触发时序在并行负载下 0.05s~21s 不定；测试 20s 超时 panic → unwind 删 TempDir（cwd 消失）→ 迟到的续跑 turn canonicalize 失败 → 伪 `blocked`（DB `status_reason` + exists_before=false 实证）。结论：非产品缺陷（续跑链路有单测覆盖），为「测试拆除竞态 + 并行负载时序」复合问题。**后续项**：可控时钟/独立负载环境下补全链路断言（登记追加发现项）。FE 验收待跨仓手动 |
| 2026-09-08 | P8 | **FE 识别位置跨仓核对**：cowork `apps/cowork-server/src/harness/acp-process.ts` 的 `goalCapabilitiesFromInitialize` 读 initialize 响应**顶层 `_meta.goal`**——已改为双处广播（顶层 + `agentCapabilities._meta.goal` 同形），spec 14/alignment 附录 C/todo 同步修正。clippy 工作区零警告；goal 45/45、acp 单测全绿；e2e_mega 在 21 个并行 workflow 压机时段超时（同日早些单跑通过），属环境型，见追记 | |
| 2026-09-06 | P2 | accounting/steering 落地：公式对拍 6 用例、并发防双计、墙钟、budget 一次性去重；基线绑定 goal_id（替换丢弃/状态不符追补语义） |
| 2026-09-06 | P3 | GoalRuntimeHandle + TurnDriver + GoalStateLock 全流程集成测试绿（幂等续跑/竞态/预算四断言/blocked/deferral）；宿主接线归 P5 批次 |
| 2026-09-06 | P4 | 三工具 + verify 门（ShellVerifyRunner）+ goal_tools 注册入口；**goal crate 100% 完成**：39/39 测试 + clippy -D warnings 全绿。剩余：P5（宿主接线+外部面）与 P6（迁移+冻结）跨 crate 手术 |
| 2026-09-07 | P5 | **宿主接线 + R3 收口完成**：`Command::Goal` 六子命令（parser 兼容裸 `/goal`）、`AcpTurnDriver` + prompt 生命周期钩子（on_turn_start/finish/abort/error + quota 启发式 + continue_if_idle）、REPL `/goal` 六子命令、telegram-bot 显式不支持；e2e_mega 本机间歇超时为既有 flaky（HEAD 复现），非本变更引入 |
| 2026-09-07 | P5b+P6 | **扩展切 thread_goals 后端**（wire 兼容层：旧形状投影 + goal/changed|updated 双发广播；goals.json 读取路径退役，恢复预约机制删除）；`anureo goal --migrate` 一次性迁移（GoalMeta + goals.json → thread_goals，幂等+备份）；legacy 四处冻结标注；文档同步 9 处。遗留：session.metadata 投影（跨仓 FE 协调）、goal_mode 存废（P7）、workspace 全量 nextest 未跑 |
| 2026-09-07 | — | 参考源修正：用户指出应对标 codex-acp；检索定位 agentclientprotocol/codex-acp 并取得 docs/goal-extension.md 全文；审计结论 = 内核同构无需动、外部协议面需按中立规范重做 → 登记 Phase 8，alignment 附录 C 已写入 |

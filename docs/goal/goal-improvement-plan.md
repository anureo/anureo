# Goal 功能改进方案（借鉴 codex-rs/ext/goal）

> **状态**: **已搁置**（2026-09-07）——评审定夺由 [goal-codex-alignment.md](./goal-codex-alignment.md)（全量对齐路线）胜出并已实施（P0-P6 落地），本文保留作历史参考，勿据此实现。
> **日期**: 2026-09-06
> **相关代码**: `apps/acp/src/extensions/goal.rs`、`apps/acp/src/goal_runner.rs`、`apps/acp/src/agent.rs`（`/goal` 命令入口）、`agent/agent-core/src/goal_runner/{mod,state,message}.rs`、`apps/cli/src/goal_runner/`
> **交叉参考**: [codex 架构分析](../analysis/codex-architecture-20260906.md)、[codex 执行过程分析](../analysis/codex-execution-flow-20260906.md)、[Goal 规范](../acp-spec/extensions/14-goal-scheduled-task.md)、[goal/task 用户指南](../user-guide/09-goal-task-experimental.md)、[运行时改进方案](../design/runtime-improvement-plan.md)（执行链路侧改进，与本文互补）
> **相关既有文档**: [Codex Goal 功能源码导读](./codex-goal-analysis.md)（2026-08-20 快照的源码事实记录，本文 §0 速览的详细版）、[Goal 系统工作流](./goal-system-workflow.md)、[Session Goal 集成](./session-goal-integration.md)（anureo goal 既有设计；本方案是在其基础上的差距修补，不推翻 server-owned 架构）、[goal-codex-alignment.md](./goal-codex-alignment.md)（同日平行路线：全量对齐 Codex 架构、session-integrated 重构，与本文方向相反，待评审定夺）

---

## 0. 背景：codex 的 goal 设计速览

codex（`codex-rs/ext/goal`，约 10 个模块）的 goal 是**挂在会话线程上的持久状态 + 模型驱动的生命周期**：

| 机制 | 实现 | 关键文件 |
|---|---|---|
| **模型侧工具** | `get_goal` / `create_goal(objective, token_budget?)` / `update_goal(status)`，模型自己标记完成/受阻；`update_goal` **只允许** `complete`/`blocked`，pause/resume 由用户控制 | `ext/goal/src/spec.rs`、`tool.rs` |
| **预算记账** | 每轮 delta = `input − cached + output` + 墙钟时间；信号量串行化记账写入；`UsageLimited`/`BudgetLimited` 双状态；完成时工具返回 `remaining_tokens` | `accounting.rs` |
| **blocked 审计** | 模型只能在**同一阻碍连续出现 ≥3 个 goal turn** 后标记 blocked；resume 后审计重置 | `spec.rs` |
| **空闲续跑 steering** | `on_thread_idle` → goal active 且线程空闲 → 注入 continuation prompt（模板含预算余量）；另有 `budget_limit.md`、`objective_updated.md` 模板 | `runtime.rs`、`steering.rs`、`templates/goals/*.md` |
| **外部 API** | `set/get/clear`，`expected_goal_id` 乐观并发；`goal_state_permit` 防外部修改与 idle 续跑竞态；fork 前 flush 记账；rollout 持久化 `thread.goal.updated` 事件 | `api.rs` |
| **状态机** | `Active / Paused / Blocked / UsageLimited / BudgetLimited / Complete`，每 thread 至多一个未完成 goal | `codex-state` thread_goals |

## 1. 本项目现状与差距

本项目有三套 goal 实现，互相脱节：

1. **CLI runner**（`apps/cli/src/goal_runner/runner.rs`）——功能最全：token 预算强制执行（剩余 20% 告警，`BUDGET_WARNING_FRACTION`）、`UsageLimited` 终态、连续失败 3 次 → `Blocked`（`MAX_CONSECUTIVE_FAILURES`）、rate-limit 不计入失败、`--verify` 验证命令。
2. **ACP runner**（`apps/acp/src/goal_runner.rs`）——`/goal` 命令（`agent.rs:1143` 起）触发的 server-owned 后台循环（task DB 做检查点 + task MCP + continuation prompt），但：
   - **预算记账半接线**：`tokens_used` 已按 TurnFinish usage 累计并持久化（`goal_runner.rs:282-296`，口径 `input+output` 未扣 cached），但 `token_budget` 从未设置、`budget_warning` 恒为 `None`（`:378`）——预算从不强制执行；`time_used_seconds` 不更新；
   - 单轮错误直接 Paused，没有 CLI 已有的连续失败/rate-limit 重试语义；
   - `HistoryEntry.summary` 恒为 `None`，注入 history 是无信息量的 `"iter N: completed"`；
   - `verify_command` 字段存在但 ACP 路径永远不设置；
   - blocked 只能靠跑满 100 次迭代，模型没有主动出口。
3. **扩展存储**（`apps/acp/src/extensions/goal.rs`，`_anureo.dev/goal/*`）——JSON 文件 CRUD：
   - `start` 创建 **Active 但无人执行的"惰性" goal**（无 runner 挂接），语义误导；
   - `GoalChangedNotification` 只嵌在 RPC 响应里，没有推送（对比 `auto_review.rs:560` 已有 `notify()` 通道），客户端只能轮询 `goal/list`；
   - 规范定义的 `progress`/`steps` 字段从未填充；
   - `GoalStatus` 枚举缺 `blocked`/`usage_limited`，一律折叠成 `Failed`；
   - 无运行中改目标的手段（objective/budget 冻结在 `/goal` 时刻）。

**结论：最大差距不是架构，而是「记账与状态推进的执行语义」没接全，以及三套实现逻辑重复且互相漂移。**

## 2. 改进方案（P0→P4，每阶段独立可交付）

### P0：ACP runner 补齐执行语义（小改动、高价值，最先做）

把 CLI runner 已验证的逻辑**下沉到 `agent-core::goal_runner` 共享**，消除漂移：

1. **预算接线**：抽出 `BudgetTracker`（tokens_used 累计、cached 扣减口径、20% 余量告警、`UsageLimited` 判定）到 `agent/agent-core/src/goal_runner/state.rs`；ACP runner 现有的 usage 累计（`goal_runner.rs:282-296`）换成 BudgetTracker 以补齐强制与告警；`/goal` 命令语法扩展支持预算（如 `/goal <desc> --budget 500k`），持久化进 `GoalMeta`。
2. **失败语义对齐**：连续失败计数、rate-limit 不计入（CLI `runner.rs:294-315` 已有），ACP 单轮错误先重试再 Paused。
3. **History 摘要真实化**：每轮结束后取最终 assistant 回复（或工具摘要）截断填充 `HistoryEntry.summary`，让 history 注入真正约束"别重复已做的工作"。
4. **verify 接线**：`/goal ... --verify "cargo test"` → `GoalMeta.verify_command`，复用 CLI 的每轮验证逻辑。

**验收**：ACP `/goal` 带 budget 跑到超限产生 `usage_limited` 且 task DB 元数据正确；history 注入含真实摘要；`cargo nextest run`（acp 相关包）通过。

### P1：模型侧 goal 工具 + blocked 审计（codex `tool.rs` 模式）

1. task MCP server（`.anureo/goal-mcp.json` 挂载）新增 `goal_get` / `goal_update`：
   - `goal_update` 仅接受 `complete | blocked`（pause/resume 仍归用户），返回 `remaining_tokens`，完成时附使用报告提示；
   - completion audit prompt 保留（`message.rs` 已完整），工具是它的结构化出口。
2. **blocked 审计**：`GoalMeta` 增加 `blocked_streak` 字段，同一 blocker 连续 ≥3 轮才允许 `blocked`；`resume` 时重置（照搬 codex 规则，防止模型遇难点就弃）。
3. continuation prompt 中把「跑满 100 轮才 blocked」的兜底降级为最后防线。

### P2：运行中目标可编辑（steering）

1. 扩展新增 `_anureo.dev/goal/update`：改 objective / 调 budget，带 `revision` 乐观并发（对标 codex `expected_goal_id`）。
2. runner 每轮循环开头读取最新 objective/budget：objective 变化 → 下一轮 prompt 注入「objective updated」确认段（参考 codex `objective_updated.md` 模板，沿用 `<untrusted_objective>` 转义防注入）。
3. **提高 budget 可复活 `usage_limited` 的 goal**（resume 语义扩展：resume 前允许先 update budget）。

### P3：推送通知 + 进度可见性

1. 复用 `auto_review` 的 notify 通道，每次状态迁移/每轮结束推送 `_anureo.dev/goal/changed`（负载即现有 `GoalChangedNotification` + iteration/tokens_used/time_used）——客户端不再轮询。
2. `progress` 字段真实填充：completed iterations、token 使用、耗时放 `metadata.progress`（steps 若 task DB 无子任务模型则暂缓）。
3. 同步更新 [14 号规范](../acp-spec/extensions/14-goal-scheduled-task.md) 与 CHANGELOG；扩展变更按 [ACP 通信审查指南](../dev/acp/04-communication-reasonableness.md) 检查事件顺序与幂等。

### P4（收敛期）：语义统一与存储升级

1. **`start` 语义决策**：start 支持触发 runner（与 `/goal` 等价，带 workingDirectory），消除"Active 却无人执行"的误导；或至少创建为 `Pending`。
2. **状态枚举扩展**：`GoalStatus` 增加 `blocked`/`usage_limited`——为兼容旧客户端，先落在 `metadata.lifecycle`（`GoalLifecycle` 枚举已有这些值，纯属展示层没透出），规范升版后再进顶层枚举。
3. **存储迁移**：goals.json → task DB 同库（或 SQLite），解决 CLI 与 ACP 进程并发写 JSON 无跨进程锁的问题（目前只有进程内 mutex + 原子 rename）。迁移方式参考 codex `state` crate 的组织（基础设施/领域模块/迁移/恢复分离，见架构分析文档 §10）。

## 3. 明确**不**照搬的部分

- **per-thread goal + idle 自动续跑**：codex 的 goal 活在普通会话线程内；本项目的 goal 是 server-owned 后台任务（`agent.rs:1175-1182` 注释明确该设计），且前端依赖现有扩展面。保持现架构，只借记账/审计/steering 机制。
- **BudgetLimited/UsageLimited 双状态**：本项目暂无全局用量上限集成，先只做 `usage_limited`，避免协议面膨胀。

## 4. 风险与顺序

| 风险 | 缓解 |
|---|---|
| objective 更新引入 prompt 注入 | 沿用 `escape_xml_text` + untrusted 包裹（P2）|
| 新状态值破坏旧客户端 | 先进 metadata，规范升版后再进枚举（P4）|
| 三套实现改出行为分歧 | P0 的共享化是前提，先做它再动 P1/P2 |
| workspace 常有并行改动 | 每阶段落地后 `cargo nextest run` + `cargo clippy --workspace --all-targets -- -D warnings` 兜底；改完重新 grep 确认变更仍在 |

**推荐执行顺序：P0 → P3（通知，前端立刻受益）→ P1 → P2 → P4。** P0+P3 合计约一个中等 PR；P1/P2 各自独立成 PR。

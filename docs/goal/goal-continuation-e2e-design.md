# goal 续跑全链路 e2e 测试用例设计（P8 生命周期解耦验收）

> 状态：核心链路已实现（2026-09-11）；TC-3/TC-4/TC-5 竞态矩阵待补
> 关联：[goal-codex-alignment.md](./goal-codex-alignment.md) §6.5/附录 C、[goal-codex-alignment-todo.md](./goal-codex-alignment-todo.md) Phase 8「生命周期解耦验收」

> 实施记录：`apps/acp/tests/e2e_goal_neutral.rs::neutral_goal_set_starts_and_accounts_across_turns` 已通过真实 stdio bridge、WebSocket server 与 mock LLM 覆盖 set 立即启动及 TC-1/TC-2 核心语义；`apps/acp/tests/e2e_goal_recovery.rs::goal_persists_across_acp_process_restart` 覆盖进程重启后的 `session/load` 快照恢复。本文其余内容保留为后续竞态场景矩阵。
> 现有覆盖：`apps/acp/tests/e2e_goal_neutral.rs`（稳定面：能力协商/控制/快照/清除）、`apps/acp/tests/e2e_goal_recovery.rs`（legacy 持久性）

---

## 1. 验收目标

验证 alignment §6.5「生命周期解耦」在**真实进程拓扑**（stdio bridge → server 子进程 → mock LLM）下成立：

1. **续跑发生在 prompt 之外**：turn 1 完成且 prompt 响应返回后，active goal 经 `continue_if_idle` 启动的续跑 turn 不占用任何 prompt 请求-响应通道；
2. **更新在 prompt 之外发布**：续跑产生的 `session/update`（消息块 / goal 快照）独立到达；
3. **预算耗尽收口**：`budget_limited` 经 6→5 投影为 `limited` 并发布快照，自主循环**自然停止**（含一次性预算 steering 收尾 turn）；
4. **清理与恢复语义**：手动 clear 终止循环；fork 保护窗后恢复续跑。

## 2. 已知障碍与前置基础设施修复（必须先做，否则用例不可稳定实施）

2026-09-08 调查实证（见 todo 进度记录）：

| # | 障碍 | 实证 | 修复方案 |
|---|---|---|---|
| F1 | **server 日志不可见**：`anureo acp` bridge 会 DETACHED 再 spawn server 子进程（`ws_bridge::spawn_server`），stderr=null、不带 `--log-level/--log-file` | 断言失败时零日志可查，只能靠 DB `status_reason` 反推 | `spawn_server` 透传 bridge 的 `--log-level`，日志文件落到 harness 受管目录（**产品改进**，一并提升现场可诊断性） |
| F2 | **测试拆除竞态**：测试 20s 超时 panic → unwind 删 `TestEnv` TempDir（含 session cwd）→ 迟到的续跑 turn `canonicalize(cwd)` 失败 → 伪 `blocked`（DB `status_reason` + `exists_before=false` 实证） | 伪 blocked 污染断言，且覆盖真实信号 | cwd 改用**独立 scratch 目录**（不受 TempDir drop 管理，测试末尾显式清理）；或 harness teardown 先 `shutdown()` 等 server 退出再 drop TempDir |
| F3 | **钩子触发时序不定**：turn 1 完成后钩子/续跑 turn 启动在并行负载下 0.05s~21s 不定 | 20s 轮询窗不够；sleep 式等待不可接受 | **推荐方案 A**：测试专用开关（如 `ANUREO_GOAL_E2E_IMMEDIATE=1`）下，turn 完成钩子同步执行 `continue_if_idle` 后再返回响应（确定性最强、侵入最小）。备选 B：TurnDriver 可控时钟注入。备选 C（现状）：宽窗轮询——已证不可靠，弃 |
| F4 | **无进程外探针**：无法观测「LLM 收到几次请求」「是否有 prompt in flight」 | 循环停止 / 收尾 turn 无法断言 | harness 增加：wiremock 请求计数（`MockServer::received_requests`）或自定义计数挂载；AcpTestHarness 增加 in-flight prompt 计数 |

## 3. 被测行为与规范依据

- alignment §6.5：turn 完成钩子 → `continue_if_idle` → `start_turn_if_idle` 二道门（无 deferral / goal active / 无活跃 prompt）→ `agent.prompt` 注入 steering 续跑；
- alignment §6.8：预算耗尽 → `budget_limited`（越界当轮结束即翻转）+ 一次性 steering 收尾 turn（ActiveOnly 不再记账）；
- alignment 附录 C.2：`budget_limited`/`usage_limited` → 中立面 `limited`；`statusReason` 保留原始成因；
- §6.6：快照替换 / session fork → deferral，`on_turn_start` 清除。

## 4. 场景矩阵

### TC-1 续跑 happy path：prompt 外 turn + prompt 外更新（核心验收）

- **前置**：usage SSE 挂载（每 completion 记账 2 token，`up_to_n_times(10)`）；`_session/goal set {objective, tokenBudget: 100}`；F1-F4 已就位。
- **步骤**：
  1. `initialize` → 断言 `_meta.goal` 能力块（回归守卫）；
  2. `session/new`；
  3. `set` → 断言响应 + active 快照通知；
  4. `session/prompt "go"` → 断言 `end_turn`（turn 1）；
  5. 等待（事件轮询，超时 60s）第 2 个 `agent_message_chunk`。
- **断言**：
  - A1 出现第 2 个 chunk（turn 2 = 续跑 turn 的输出）；
  - A2 该 chunk 到达时 **in-flight prompt 计数 = 0**（F4 探针）——「prompt 之外」的直接证据；
  - A3 goal 快照 `tokensUsed ≥ 4`（两次记账 → 第二个 turn 真实发生）；
  - A4 快照序列 `tokensUsed` 单调递增、`status` 保持 `active`。
- **清理**：`clear` → 断言 `goal: null` 快照；宽窗（15s）断言无新 LLM 请求（循环已停）。

### TC-2 预算耗尽 → limited 投影 + 自然停止

- **前置**：usage 每 turn 2 token；`tokenBudget: 3`。
- **步骤**：set（立即启动 turn 1，2/3）→ 事件轮询等待 `limited`。
- **断言**：
  - B1 快照 `status == "limited"` 且 `tokensUsed ≥ 4`（6→5 投影在真实 wire 上成立）；
  - B2 `statusReason` 保留原始成因（`budget_limited`）；
  - B3 预算 steering 收尾 turn 恰好一次（LLM 请求计数 = turn1 + turn2 + 收尾 = 3；或收尾 turn 的 user chunk 含预算文本）；
  - B4 **循环停止**：宽窗 15s 内 LLM 请求计数不再增长、无新消息块；
  - B5 `_session/goal resume` → 错误（`not resumable`，中立错误面回归）。

### TC-3 快照替换 → deferral 推迟续跑（§6.6）

- **风险**：替换与续跑的竞态窗口不可控（2026-09-08 已证）。**实现策略**：依赖 F3-A 确定性开关后再做，否则**降级为单测覆盖**（`set_with_verify_outcome.replaced_existing → defer_continuation` 已有单测，e2e 跳过）。
- 步骤（F3-A 就位后）：set A → prompt（turn 1）→ 紧接 `set B`（替换）→ 断言：turn 2 **未**立即启动（deferral 生效）；下一次 turn 边界（新 prompt 后）恢复续跑且 objective 为 B。

### TC-4 fork 保护窗

- **步骤**：set（budget 100）→ prompt → `session/fork` → 宽窗观察。
- **断言**：D1 fork 成功；D2 源 session goal 仍 active（快照连续）；D3 fork 后新 session 无 goal（盲审 C2：FK 按 thread_id）；D4 保护窗结束后源 session 续跑恢复（无 deferral 残留——fork 失败路径同理）。
- 时序敏感度：中（fork 同步完成，窗口内不做时序断言）→ **可做**。

### TC-5 多轮鲁棒性

- **步骤**：连续 3 轮「prompt → 等待一次续跑 turn 完成」，最后 clear。
- **断言**：每轮续跑都发生（防「仅首轮续跑」回归）；`tokensUsed` 全程单调递增；clear 后循环停止。

## 5. 断言与稳定性原则

1. **零 sleep**：全部断言基于通知轮询（`wait_for_notification`）或探针计数 + 显式超时；
2. **超时预算**：轮询窗 60s（续跑类）/ 15s（停止类），nextest slow-timeout 需相应调整（当前 20s×2 会在压机时段误杀）；
3. **失败自诊断**：F1 日志落盘 + 失败输出附加 goal 状态转储（status/tokensUsed/deferrals/LLM 请求计数）；
4. **验收标准**：TC-1/2/4/5 连续 10 次全绿（**含 18+ workflow 并行压机时段**）；TC-3 视 F3 方案落地情况。

## 6. 基础设施改动清单

| 层 | 改动 | 性质 |
|---|---|---|
| `ws_bridge::spawn_server` | 透传 `--log-level`、日志落受管目录 | 产品改进（F1） |
| `TestEnv` / harness | cwd scratch 化；teardown 顺序（shutdown → 等待子进程 → drop）；in-flight prompt 探针；LLM 请求计数 | 测试基建（F2/F4） |
| turn 完成钩子 | `ANUREO_GOAL_E2E_IMMEDIATE`（`#[cfg]`/env 门控）同步续跑 | 测试专用开关（F3-A，需评审命名与门控方式） |
| `e2e_goal_neutral.rs`（或新 `e2e_goal_continuation.rs`） | TC-1/2/4/5 用例 | 本体 |
| `.nextest.toml` | goal e2e 的 slow-timeout 调整 | 配置 |

## 7. 排障方法论（2026-09-08 调查沉淀，供实施者复用）

1. server 日志不可见 → **DB `status_reason`**（`tasks.db` 的 `thread_goals` 表）是最可靠的失败证据；
2. 通知缓冲（harness panic dump）看 wire 序列；`sessionUpdate` 是 tagged snake_case（`session_info_update`），goal 快照在 `params.update._meta.goal`；
3. 怀疑时序问题时：env 门控文件 breadcrumb（`continue_if_idle` 各出口 + driver 各门）打点后对毫秒时间轴；
4. 「目录消失/伪 blocked」先查测试拆除时序再怀疑产品（TempDir unwind 竞态）。

## 8. 工作量估算

F1 0.5h + F2 0.5h + F3-A 1~2h（含门控评审）+ TC-1/2/5 实现 2~3h + TC-4 1h + TC-3 评审决定 ≈ **1d**（与 todo 原估一致）。

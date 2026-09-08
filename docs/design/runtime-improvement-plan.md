# Agent 运行时改进方案（借鉴 codex 执行模型）

> **状态**: Draft（方案定稿，未开工；R2/R5 可直接指导开发）
> **日期**: 2026-09-06
> **相关代码**: `agent/agent-core/src/agent/react/act_executor.rs`、`agent/agent-core/src/tool_output_normalizer.rs`、`agent/agent-core/src/run/runner.rs`、`apps/acp/src/session.rs`
> **交叉参考**: [codex 执行过程分析](../analysis/codex-execution-flow-20260906.md)（本文差距项的事实来源）、[Codex Goal 功能源码导读](../goal/codex-goal-analysis.md)、[Goal 功能改进方案](../goal/goal-improvement-plan.md)（goal 侧改进，与本文互补，构成完整路线图）
> **调查方式**: 2026-09-06 对 workspace 源码逐项核实，所有 file:line 为当日快照

---

## 0. 背景与定位

[codex 执行过程分析](../analysis/codex-execution-flow-20260906.md) §11 给出了 codex 与 anureo 执行链路的对照表。本文把其中「可借鉴项」逐条落到代码事实，形成运行时侧改进方案。

改进分两条线，本文只覆盖运行时线：

| 线 | 覆盖内容 | 文档 |
|---|---|---|
| goal 线 | 预算记账、模型侧工具、blocked 审计、steering、推送通知 | [goal-improvement-plan.md](../goal/goal-improvement-plan.md)（P0-P4，仍未开工，差距核实仍准确） |
| 运行时线 | 工具并行、输出截断、审批回流、steering 队列、可观测性 | 本文（R1-R6） |

## 1. 现状与差距（2026-09-06 核实）

| # | codex 机制 | anureo 现状 | 差距评级 |
|---|---|---|---|
| 1 | `FuturesOrdered`：一次响应内多工具**并行执行、结果按序回流** | `ToolCallExecutor::execute` 纯串行 for 循环（`act_executor.rs:109-129`），结果顺序天然有序 | 高：多工具 turn 吞吐直接受限 |
| 2 | shell 输出 head-tail 截断（保头保尾防上下文爆炸） | 截断框架完整（inline 4000 / head-tail 各 600 / 单 turn 8000，`tool_output_normalizer.rs:37-51`），但 `TRUNCATABLE_TOOLS` 白名单仅 web 工具（:279）；shell 20000 字符输出仍全量 Inline（测试 :823-836 注释 "bash is no longer whitelisted"，系有意移除） | 高：长输出直接侵蚀上下文预算 |
| 3 | usage 挂 tracing span 字段，span 层级即执行层级 | 全 crate 唯一 `info_span!("agent_run", thread_id)`（`run/runner.rs:117-133`）；usage 仅 `trace!` 事件（`think_node.rs:195-202`），无 turn/采样/工具嵌套 span | 中：可观测性缺失，排障只能靠日志拼 |
| 4 | 审批决定经同一 Op 队列回流，等待人批不阻塞其他操作 | 生产代码**未实现**审批路径；期望流程仅存于文档注释（`stream_bridge.rs:20-25`、`protocol.rs:65-78`），仅 e2e 测试有客户端 stub（`tests/e2e/common/harness.rs:137`） | 中高：安全特性空缺，危险命令无闸门 |
| 5 | steering：turn 进行中新输入进 `input_queue`，turn 收尾后合并续跑 | busy 时新 prompt 直接拒绝 `-32010`（`session.rs:443-477`、`agent.rs:1105-1110`）；cancel 为硬 abort（generation + `RunCancellation`，`cancellable.rs:25-52`，工具 future 直接中止） | 中：体验差距，但 ACP 同步语义下有折衷空间 |
| 6 | rollout JSONL 事件追加日志，可重放重建对话 | SQLite 快照（`SqliteSaver` 整份 `ReActState` + channel_versions，`sqlite_saver.rs:152`）；恢复=读最后快照（`initial_state.rs:53-76`） | 低：快照已满足 resume，见 §3 |
| 7 | 双层记账完整（turn 级 + 线程级，cached/reasoning 明细） | 双层存在（`state.rs:136,242-252`），但 `LlmUsage::accumulate` 不累加 cached/reasoning 明细（`traits.rs:139-146`）；LLM 层已捕获 `cached_tokens`（`llm_client.rs:357-369,423-478`） | 低：明细丢失影响 goal 预算精度（goal P0 需顺手修） |

## 2. 改进方案（R1-R6）

### R2：shell 输出重回截断白名单（最小成本，最先做）

1. 先 `git log -S "bash is no longer whitelisted"` 考古当初移除原因；若是「全量输出对排障重要」，则改为**可配置**：`ToolOutputConfig.truncate_shell_output`（默认保持现状），开启后 shell/powershell 类工具纳入 `determine_strategy` 的 head-tail 路径。
2. 单 turn 观察预算（8000）对多工具长输出场景偏紧，配置化并允许 per-tool 覆盖。
3. 超大输出落盘 `<home>/tool-output/` 已有（`persist_output :511-543`），补一条「落盘路径回填给模型」的 excerpt 提示，让模型知道可主动读文件尾部。

**验收**：开启配置后 20000 字符 shell 输出触发 head-tail；默认行为零变化；`tool_output_normalizer` 测试更新。

### R5：span 层级 + usage 字段（观测性，可与 R2 同 PR 或独立小 PR）

对标 codex「span 即执行层级 + usage 可观测」：

```text
agent_run(thread_id)                    # 已有
 └─ turn(turn_id, model)                # 新增，包住一次 ReAct 迭代循环
     ├─ think(response_id, prompt_tokens, completion_tokens, cached_tokens)   # 新增
     └─ act_tool(tool_name, call_id, duration_ms, bytes)                      # 新增
```

1. turn span 在 `run/runner.rs` 循环外层包一层；think/act 分别进 `think_node.rs` / `act_executor.rs` 加子 span。
2. 现有 `trace!` usage 事件（`think_node.rs:195-202`）升级为 think span 字段；span 关闭时 duration 自动可得。
3. 工具 span 记录截断前后字节数，与 R2 联动可观测「上下文被吃掉多少」。

**验收**：`tracing` 订阅器（console/otel）输出嵌套层级；`cargo nextest run` 不回归。

### R1：工具并行执行、结果按序回流（吞吐，独立 PR）

对标 codex `FuturesOrdered`，但比它更保守——codex 全量并行，anureo 工具面有隐式顺序依赖（write_file 后紧接 read_file 是常见模式）：

1. **分级并行**：只读类工具（read/grep/glob/ls/web_*）并行；写/执行类（write_file/edit/bash/powershell/apply_patch）维持串行。工具元数据新增 `readonly: bool`（tool-core spec 层）。
2. 执行：把 `act_executor.rs` 串行 for 改为「readonly 批进入 `FuturesOrdered`（并发上限可配，默认 4），非 readonly 逐个 await」；结果仍按 call 顺序 push，`backfill_call_ids`（:357）对齐逻辑不变。
3. cancel 语义不变：`run_cancellable` 的 abortable 包住整个 FuturesOrdered（`cancellable.rs:25-52` 已支持整批 abort）。
4. **与 goal 线的依赖**：并行后 tool finish 将并发到达，[goal 方案](../goal/goal-improvement-plan.md) P0 的 BudgetTracker 若先落地，必须用信号量串行化记账写入（codex `progress_accounting_lock` 的对应物），建议 P0 设计时即预留，避免 R1 落地时返工。

**验收**：5 个并行 read 的 turn 墙钟接近最慢单个工具；结果顺序与 call 顺序一致；clippy 零警告。

### R3：审批回流通道（安全特性，需前端配合）

生产实现 ACP `session/requestPermission`，对标 codex ExecApproval：

1. `ToolCallExecutor` 执行前经 approval policy 判定（按工具类别 + 命令风险，先做最粗粒度：bash/powershell 一律请求，其余放行，配置可关）。
2. 等待路径：工具 future 挂起 → `session/requestPermission` 发客户端 → decision 经 session 回流通道唤醒；**等待期间 cancel 仍可硬中止**（复用 `RunCancellation`，审批等待与 cancel 在同一 select）。
3. 超时默认拒绝（如 120s），拒绝结果作为 `ToolResult::error` 正常回填模型，不炸 turn。
4. 依赖 OpenChamber 前端支持 permission 请求 UI（跨仓协作项，落地前先对齐协议负载）。

**验收**：e2e harness 已有 stub（`harness.rs:137`）基础上补全真实路径测试；无审批配置时行为与现状完全一致。

### R4：steering 输入队列（按需，观察后再决定）

busy 时新 prompt 从「拒绝 -32010」改为「排队」的折衷：

1. `session.rs` 增加单深度 input slot（最多积压 1 条，多了仍拒绝，防失控）；turn 收尾时若有积压，作为下一轮输入合并续跑（RegularTask 循环的对应物）。
2. ACP prompt 是同步请求，排队意味着请求挂起——必须带超时（如 10 分钟）与 cancel 路径；客户端也要能感知「已排队」状态。
3. **此项收益依赖真实使用模式**：若前端在 busy 时根本不让用户发 prompt（当前 OpenChamber 行为），则无需求。建议观察后再排期，默认不动。

### R6：会话累计 usage 明细修复（顺手项）

`LlmUsage::accumulate` 补齐 cached/reasoning 累加（`traits.rs:139-146`），使 `total_usage` 与 goal 预算记账（goal 方案 P0）拿到与 codex 公式 `input − cached + output` 一致的明细。改动一处 + 测试。

## 3. 明确**不**做的部分

- **rollout JSONL 事件重放**：codex 的 rollout 服务于多客户端共享 thread 与 fork 快照复制；anureo 的 SQLite 快照已满足 resume 语义，重放日志是基础设施级改造，无对应需求，不做。
- **Op 队列全盘照搬**：codex 用消息化 Session 解耦宿主与 core；anureo 的 ACP 同步请求 + server-owned goal runner 架构下，全队列化收益不抵协议面改造成本。只在 R3/R4 内借「决定回流」与「积压合并」的**语义**，不搬**结构**。
- **硬取消改软取消**：当前 generation + abortable 的硬取消简单可靠，codex 的 steering 软路径由 R4 按需引入，不动现有 cancel。

## 4. 统一路线图（两线合并）

| 批次 | 内容 | 规模 | 依赖 |
|---|---|---|---|
| 1 | goal P0 + R6（记账接线与明细修复同 PR 最自然） | 中 PR | 无 |
| 1' | R2 + R5（截断配置化 + span 层级，纯观测与输出策略，互不冲突） | 小 PR | 无（可与批次 1 并行） |
| 2 | goal P3（推送通知，前端立刻受益） | 小 PR | 批次 1 |
| 3 | R1（工具分级并行） | 中 PR | 批次 1 的 BudgetTracker 需并发安全 |
| 4 | goal P1 / P2（模型侧工具 + steering 编辑） | 各一中 PR | 批次 1 |
| 5 | R3（审批回流） | 大 PR（跨仓） | OpenChamber 前端配合 |
| 按需 | R4（steering 队列）、goal P4（存储升级） | — | 观察真实需求后排期 |

每批次落地后 `cargo nextest run` + `cargo clippy --workspace --all-targets -- -D warnings` 兜底。

## 5. 风险表

| 风险 | 缓解 |
|---|---|
| R2 改变 shell 输出截断行为影响既有会话 | 默认保持现状，配置开启；考古移除原因后再定默认值 |
| R1 并行改变工具执行顺序假设 | 分级并行（readonly 才并行），call 顺序回填不变；e2e 覆盖多工具 turn |
| R1 与 goal 记账并发冲突 | BudgetTracker 信号量串行化写入（批次 1 预留，批次 3 才暴露） |
| R3 审批等待期间死锁/漏 cancel | 审批等待与 RunCancellation 同 select；超时默认拒绝 |
| R4 排队请求超时挂死 | 单深度 + 强制超时 + 可 cancel；默认不启用 |
| workspace 常有并行改动 | 每批次前 fresh read 目标文件；落地后 grep 确认变更仍在 |

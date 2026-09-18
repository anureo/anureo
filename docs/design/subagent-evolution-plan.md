# 子代理架构演进方案：codex 设计吸收路线（2026-09-18）

> **状态**: Proposal（提案，待评审）。定位：[契约 MVP](./subagent-impl-plan.md)（Phase 1-4）**之上**的演进路线——不改冻结契约语义，每项标注依赖的 MVP Phase 与启动信号。codex 机制出处见[架构详析](../analysis/codex-subagent-architecture-20260918.md)（下文引用为「codex §N」）。
> **背景**: loom 现有子代理是"tokio task 上的 ReAct 闭包"（registry 内存表承载一切）；契约 MVP 把它升级为一等 ACP child session。本方案回答：**在此之后，codex 还有哪些设计值得继续吸收、以什么顺序、到什么程度**。
> **交叉参考**: [ACP 子代理契约](./acp-subagent-contract.md)、[实现详细方案](./subagent-impl-plan.md)、[差距审计](./subagent-interaction-gap.md)

---

## 0. 吸收判据与处置总览

**判据**（三项全过才吸收）：

1. 解决 anureo 的真实痛点（不是"codex 有所以我要有"）；
2. 与现有结构映射自然（registry / ACP SessionIndex / checkpoint / stream 事件，不另起炉灶）;
3. 不破坏冻结契约（wire 只增不改）。

**处置总览**（详细设计见后文对应节）：

| codex 机制（出处） | 处置 | anureo 落点 |
| --- | --- | --- |
| 身份一次生成 / 随 rollout 持久化（§3/§6.3） | ✅ 已随 MVP 吸收 | Phase 1 SubagentIdentity |
| 终态恰一次收敛（§3.6/§15-5） | ✅ 已随 MVP 吸收 | registry 守卫 + sink |
| 树级共享 registry（§4.1） | ✅ 已随 MVP 吸收 | Phase 2 server 级注入 |
| RAII 预留 reserve→commit→Drop（§3.6） | **A1 并入 Commit 1** | registry 预留 + 竞态修复 |
| 并发执行限制 AgentExecutionLimiter（§7） | **A2 并入 Phase 1/2** | registry 执行信号量 |
| 生命周期/通信审计 tracing（§8.5/§14） | **A3 并入 Phase 2** | `anureo::subagent` target |
| 完成通知注入父上下文（§8.4/§9） | **E1-1** | 父 checkpoint 注入摘要 |
| followup 继续对话（§8.1 TriggerTurn） | **E1-2（最小版）** | child session `current_agent` |
| 树状寻址/展示（§3.1 AgentPath） | **E1-3** | SessionIndex 树 + depth 展示 |
| usage 归并到父（§14） | **E1-4** | 父 TurnUsage 累计器 |
| wait_agent 活动等待（§8.3） | **E1-5** | agent_get `wait` 参数 |
| 驻留 LRU 换出/重载（§11 V2Residency） | E2-1（信号驱动） | park/reload |
| 运行中 steer/信箱（§8 InputQueue） | E2-2（信号驱动） | ReAct turn 边界 drain |
| fork 过滤/净化规则表（§10） | E2-3（信号驱动） | fork 接管立项时套用 |
| 角色只收窄+继承交集（§13-1/3） | E2-4（依赖权限系统） | profile 权限边界 |
| 子代理=ThreadManager 会话树（§1-2） | ❌ 不吸收 | SessionIndex 已承载 |
| V1/V2 双版本并存（§2/§5.3） | ❌ 不吸收 | 无历史包袱 |
| 加密通信通道（§3.3） | ❌ 不吸收 | 过度设计 |
| agent-graph-store 独立边表（§12.1） | ❌ 不吸收 | lifecycle+metadata 已表达 |
| AgentControl Weak 回引防环（§4.1） | ❌ 不吸收 | 注入式已达成同效 |

## 1. 已随 MVP 吸收（无需立项，备查）

Phase 1-4 本身就是一次 codex 思想的定向移植：SubagentIdentity（≈ SubAgentSource 七要素）、恰一次终态（registry 守卫收敛点唯一化）、树级共享 registry（AgentControl 的共享 state 思想）、depth 进持久化身份（SessionSource 思想）、`MAX_TERMINAL_ENTRIES=50` 终态容量上限（registry.rs:75，V2Residency 的极简前身）。此节只为避免重复立项。

## 2. A 档：低成本，直接并入 MVP 提交

### A1 RAII 预留 + spawn/register 竞态修复（并入 Commit 1）

- **codex 原型**：`SpawnReservation`（详析 §3.6）——reserve 占位，commit 落账，Drop 未 commit 自动回滚，半途失败零泄漏。
- **anureo 现状缺陷**（本次阅读发现）：`spawn_agent_task`（mod.rs:100-130）**先 `tokio::spawn` 后 `registry.register`**——若 profile 解析瞬间失败，子任务可能在 register 前就 `registry.fail`（查无此 id 落空），条目随后以 Running 注册，**永久卡死**。
- **吸收方案**：调序为 register → spawn；register 返回 RAII `AgentReservation`，spawn 成功后 `commit(abort_handle)` 补挂取消句柄，Drop 未 commit 即移除条目。与 Commit 1（identity 统一）天然同改一处。
- **验收**：新增回归测试——resolve_profile 失败路径下 registry 无残留 Running 条目。

### A2 并发执行限制（并入 Commit 4）

- **codex 原型**：`AgentExecutionLimiter`（详析 §7）——限"同时执行回合数"而非总量；RAII guard 释放；已开跑的回合豁免复检。
- **anureo 映射**：`AsyncAgentRegistry` 增 `active_executions: AtomicUsize` + `try_begin_execution() -> Option<ExecutionGuard>`（guard Drop 递减）。`AgentTool::call` spawn 前预检，子任务结束后释放。上限取 `max_concurrent_subagents`（默认 4，与 `MAX_SUB_AGENT_DEPTH` 同链配置：profile.behavior > env > 默认，react_build_config.rs:246 模式照抄）。超限报错文案给模型可执行建议（等待 agent_get 或降低并发）。
- **与既有语义的交互**：同步超时降级后台（mod.rs:281）**不释放** guard——任务仍在跑；仅真正终态释放。`MAX_TERMINAL_ENTRIES` 管的是表容量，本档管的是执行资源，两者正交。
- **T8 顺车**（[实现方案 §2.8](./subagent-impl-plan.md)）：agent 工具 schema 补 `timeout` 仅同步模式生效的说明；新增 `max_runtime` 总时限参数（超时终态 Failed("deadline exceeded")）——修复后台代理无限运行。
- **验收**：并发上限 N 时第 N+1 个 spawn 被拒；终态后名额归还；max_runtime 超时终态正确。

### A3 子代理生命周期审计 tracing（并入 Phase 2）

- **codex 原型**：专属 target `codex_otel.agent_communication`（详析 §8.5），send/receive/内容占位，独立于业务日志。
- **anureo 映射**：`AcpSubagentRuntime` 三个 sink 方法 + registry 终态守卫处，统一 `tracing::info!(target: "anureo::subagent", ...)`：created（ids 快照）/ running / terminal（outcome + stats + duration）。后续 E1-2/E2-2 的 followup/steer 事件同 target 扩展。价值：一次 `tracing` 过滤即可审计全部子代理生命周期，排障时按 invocation_id 串链。
- **验收**：测试断言 target 事件序列完整（created→running→terminal）。

## 3. E1：MVP 后第一波（高价值，1-2 个迭代）

> 架构定位：本节各项与 E2-2 共同构成**通信三原语**（[实现方案 §1.2](./subagent-impl-plan.md) 原则 4）——把现状"工具返回值"单一信道显式化为三种原语：结果注入（E1-1）、继续对话（E1-2）、运行中 steer（E2-2）；前两者零新协议，仅第三者需要 codex 式信箱语义。

### E1-1 后台完成通知注入父上下文（codex §8.4/§9 的 V1 思路）

- **痛点**：后台代理完成后，结果只进了 registry 与 UI（MVP Phase 4 的 terminal update）；**父会话的模型不知道**——下一 turn 模型若不主动 `agent_get`，后台成果等于丢失。
- **codex 原型**：completion watcher 把 `SubagentNotification`（子代理摘要，非全文）`inject_fragment_without_turn` 进父上下文——不打断父回合，下一回合可见。
- **anureo 方案**：`AcpSubagentRuntime.terminal` 在现有 durable 写入后，向**父会话 checkpoint 追加一条摘要记录**（复用 checkpoint 的 message 通道 + subagent metadata 标注 invocation_id/agent/duration；内容 = registry result 的截断摘要，**非全文**——防上下文膨胀，codex 同款取舍）。父会话下一 prompt 回放 checkpoint 时模型自然可见。
- **依赖**：M4（Phase 4 完成推送链路）。
- **验收**：后台代理完成 → 不调用 agent_get → 父下一 turn 的模型输入包含该摘要；BDD 断言 checkpoint 中的记录带 invocation_id。

### E1-2 child session 一等"继续对话"（codex §8.1 followup 的最小版）

- **痛点**：codex 的 `followup_task` 允许对同一 agent 继续派活；anureo 契约 §8 规定 terminal invocation 不可隐式恢复，但**允许把 child session 当普通会话对话**（产品设置控制）——这条路的"一等化"就是 followup 的最小实现。
- **现状缺口**：child session 的 `session_config.current_agent` 若继承父会话的值（agent.rs:1526 按 `entry.session_config.current_agent` 解析 agent），继续对话会用**父的 agent 配置**而非被委派 agent 的——语义漂移。
- **方案**：① Phase 2 `created()` 时设置 child entry 的 `session_config.current_agent = 被委派 agent 名`（一行改动，放进 MVP Phase 2 实现，标注为契约 §5.2"继承父模型/agent 配置再应用 override"的具体化）；② Desk 按产品设置放开 child prompt 后，继续对话自动沿用该 agent 的 system prompt/工具面。
- **不做**（划入 E2-2）：运行中子代理接收新输入（steer）——tokio task 闭包模型做不到，需信箱。
- **验收**：terminal child 会话中 prompt → 响应使用被委派 agent 的配置；只读默认不受影响。

### E1-3 嵌套树展示与 depth（codex §3.1 AgentPath 的展示层映射）

- **痛点**：嵌套子代理（MVP 后 depth 链已修）在 Desk 只能靠 parentSessionId 逐级点开，无全局树感与深度感知。
- **codex 原型**：AgentPath 文件系统式路径 + `list_agents(path_prefix)` 前缀过滤 + 树形渲染。
- **anureo 方案**：**不引入存储层新 ID**——SessionIndex 的 parent 链已是权威树；在 session list 投影与 subagent metadata（已有 `depth` 字段）之上，Desk 渲染缩进树 + depth 徽标；list 过滤参数 `parentSessionId`/`depth` 走 37 号规范扩展。codex 的教训（详析 §16）：路径寻址的全部收益（前缀过滤/相对引用）在 UI 层用 parent 链递归即可获得，不值得进存储。
- **依赖**：M2。
- **验收**：三层嵌套在 Desk 呈现树 + depth；list 按 parent 过滤正确。

### E1-4 子代理 token 统计归并（codex §14 usage 归并）

- **痛点**：子代理消耗（registry stats 里有 total_tokens）不进父会话账本——用户看到的 usage_update 只含父 turn 直接消耗，实际成本被低估。
- **codex 原型**：子代理 usage 汇入父线程账目（详析 §14 / fork 时 LastNTurns 丢弃 usage 防双计的对应设计）。
- **anureo 落点**：`TurnUsage` 累计器已存在且已被 goal 工具复用（agent.rs:1516-1573 同款模式）——`AcpSubagentRuntime.terminal` 把 stats 累加进父会话的 TurnUsage（需要 sink 持有父 usage 的弱引用，随 SessionEntry 生命周期）。防双计：仅 terminal 时累加一次（终态守卫天然保证）；E1-1 的摘要注入不带 usage 数值（checkpoint 与 usage 账本分离）。
- **依赖**：M4。
- **验收**：子代理完成后父 usage_update 包含其 tokens；重复 terminal 不重复累计。

### E1-5 agent_get 阻塞等待（codex wait_agent 的工具级映射，实现方案 §2.8 T9）

- **痛点**：agent_get 纯轮询——模型每查一次耗一个 turn 的 round-trip，长任务下浪费显著。
- **codex 原型**：`wait_agent` 订阅活动信号而非盯单个代理，带 min/max/default 超时夹取（详析 §8.3）。
- **anureo 方案**：agent_get 增 `wait: bool` + `wait_timeout` 参数，服务端阻塞至终态再返回；超时返回当前状态 JSON（不报错）。实现依赖 registry 终态转换的广播（Phase 1 sink 落地后的 watch 基建，watch::channel 即可）。
- **依赖**：Phase 1（sink/终态收敛）+ T1（列表瘦身，避免等待返回时又带全文列表）。
- **验收**：wait=true 单次调用拿到终态；超时路径返回 running 状态而非错误。

## 4. E2：需求信号驱动的第二波（先立信号，后立项）

| 项 | codex 原型 | anureo 映射与代价 | 启动信号 |
| --- | --- | --- | --- |
| E2-1 驻留换出/重载 | V2Residency LRU + 授权快照 + ensure_v2_agent_loaded 校验链（§11） | 空闲后台代理 park：flush checkpoint → abort runner → registry 标 `Parked`（新增态，与 ACP idle/closed 正交）→ durable child 保留；followup 时 session/load + 新 runner 从 checkpoint 续跑 = reload。代价：checkpoint 续跑对子代理 thread 的支持 + Parked 态贯通。 | 后台代理常驻内存/句柄压力实测超阈值；或 E1-2 使用率高出现"长尾会话囤积" |
| E2-2 运行中 steer/信箱 | InputQueue + trigger_turn + supersede 合并 + MailboxDeliveryPhase（§8.2） | ReAct loop turn 边界加邮箱 drain；ACP 增 `_anureo.dev/subagent/steer`；父工具面加 steer 参数。代价：大（改 loop 调度 + 协议 + UI） | 用户明确要求引导运行中子代理；E1-2 无法满足的"中途纠偏"反馈 |
| E2-3 fork 边界接管 | fork_turns 三模式 + keep/scrub 规则表 + 授权剥离（§10） | 审计 Phase 5 的 fork 语义设计**直接以 codex §10.3 规则表为蓝本**：保留消息/FinalAnswer、剥工具噪音/usage/guardian 类授权痕迹、inherited 标记 | fork 接管立项（audit Phase 5 解锁） |
| E2-4 权限收窄 | 角色只收窄 + 运行时回填 + reload 交集（§13-1/3） | profile 增 permission boundary；`build_config_from_profile` 继承时 intersect；E2-1 reload 时复核。代价：依赖 anureo 权限系统（method-policy/审批面）演进到位 | 子代理被委派高危工具面的实际需求 |

## 5. 明确不吸收清单（备查，防蔓延）

1. **ThreadManager 会话树**：codex 需要 it 是因为子代理=线程对象；anureo 的 ACP SessionIndex（durable）+ 共享 registry（运行时）分工已覆盖，再造一棵树是重复账本。
2. **V1/V2 双版本并存**：无存量协议包袱，直接建信箱版语义（E2-2 时）即可。
3. **加密通信通道**：单进程内工具调用，无跨信任域传输。
4. **agent-graph-store 独立边表**：`parent_session_id`（SessionIndex）+ subagent metadata（status）已表达 Open/Closed 语义。
5. **AgentControl Weak 回引防环**：anureo 用构造注入（Arc 自上而下），不存在 codex 的循环引用问题。

## 6. 节奏与依赖图

```
M1(今晚 Phase1) ─► M2(child session) ─► M3(Desk) ─► M4(取消/后台/签收)
   │A1 竞态修复        │A3 审计 tracing       │E1-2① current_agent(并入 Phase2)
   └A2 执行限制 ───────┘                     └► E1-1 完成注入 ─┐
                                             └► E1-4 usage 归并 ┤► E1 完成
                              M2 ─► E1-3 树展示 ─────────────────┘
E1 完成 ─► E2 按信号逐项立项（E2-3 最可能先来：随 audit Phase 5）
```

- A 档并入 MVP 提交（Commit 1/4、Phase 2），**不新增里程碑**。
- E1 各项互相独立，可并行或任选先后；建议顺序：E1-2①（一行，随 Phase 2）→ E1-3（纯展示）→ E1-1/E1-4（都挂 terminal 钩子，可同 PR）→ E1-5（随 sink watch 基建）。
- E2 任何一项立项前必须在本文档补"信号证据"小节。

## 7. 风险

| 风险 | 对策 |
| --- | --- |
| E1-1 摘要注入造成父上下文膨胀 | 注入截断摘要（codex SubagentNotification 同款），全文只留 child session；超长时仅注入"已完成 + 统计 + session 指针" |
| E1-2 与契约 §8 只读默认冲突 | 无冲突：契约本就允许产品设置放开 prompt；E1-2 只修 current_agent 语义 |
| A2 执行限制与后台降级交互产生名额泄漏 | guard 挂在子任务真实生命周期（spawn 的 async 块内 Drop），非父工具调用期；测试覆盖降级路径 |
| E2-1 Parked 态污染现有四态语义 | Parked 显式新态 + `is_active=false` + 完全独立的恢复路径；上线前写状态机迁移测试 |
| 吸收蔓延（为吸收而吸收） | §0 判据 + E2 信号证据门槛；本文档每新增一项须注明对应 codex 出处与 anureo 痛点 |

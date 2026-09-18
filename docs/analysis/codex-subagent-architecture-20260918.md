# codex-rs 子代理（multi-agent）代码架构与详细说明（2026-09-18 详细版）

> **状态**: 详细参考版（替代同日早前的快照摘要版）。基于对 `C:\Users\heycj\dev\codex`（codex-rs）子代理全部核心源码的逐文件阅读，含关键代码摘录、逐字段说明、调用链与不变量清单。行号为 2026-09-18 快照，上游演进会产生偏移。
> **阅读对象**: 需要**实现**多代理/子代理系统的工程师（尤其 anureo 子代理契约的实现者）。
> **交叉参考**: [ACP 子代理契约](../design/acp-subagent-contract.md)（anureo 侧 wire 标准）、[差距审计](../design/subagent-interaction-gap.md)、[Phase 1 代码方案](../design/subagent-phase1-code-plan.md)、[codex-rs 架构分析](./codex-architecture-20260906.md)、[codex 执行过程分析](./codex-execution-flow-20260906.md)

---

## 1. 总览：五条核心设计决策

codex 的"子代理"**不是轻量任务闭包，而是一个完整的 `CodexThread`**——带独立 Session、独立 rollout 持久化、独立配置的 agent 会话。由此派生整套架构：

1. **子代理 = 完整会话**：天然获得持久化、恢复、steering、压缩、以及自身再 spawn 子树的能力。Review、Compact、MemoryConsolidation 也走 `SessionSource::SubAgent` 通道——审阅/压缩/记忆整理在 codex 内部同样是子代理线程。
2. **控制面与会话解耦**：`AgentControl` 每个根线程只建一次，clone 进每个子会话的 `SessionServices`；对 `ThreadManagerState` 只持 `Weak`，杜绝 `ThreadManager → CodexThread → Session → SessionServices → ThreadManager` 引用环。
3. **树级作用域共享**：Registry、并发限制器、驻留管理器、RolloutBudget、root_service_tier 挂在 `AgentControl` 的共享 state 上，作用域是"会话树"而非进程——多棵树互不干扰。
4. **V1/V2 两代并存**：V1（`multi_agent_v1::` 命名空间，父驱动轮询）与 V2（plain 工具名，信箱模型）由 `effective_multi_agent_version_for_spawn`（thread_manager.rs:1568）逐 spawn 判定，支持 override、继承（resumed/forked 沿用原版本）与 feature 回退三级解析。
5. **消息与回合触发解耦**：一切代理间通信统一为 `InterAgentCommunication`，`trigger_turn` 区分"通知"与"派活"；wait 语义简化为"等活动信号"。

## 2. 代码地图

```
codex-rs/
├── core/src/
│   ├── agent/
│   │   ├── control.rs              AgentControl：spawn/send/interrupt/list/resume 中枢
│   │   ├── control/spawn.rs        spawn_agent_internal / ensure_v2_agent_loaded / fork / resume
│   │   ├── control/execution.rs    AgentExecutionLimiter（V2 并发执行限制）
│   │   ├── control/residency.rs    V2Residency（LRU 驻留/驱逐/重载）
│   │   ├── registry.rs             AgentRegistry（内存树 + 总量配额 + 昵称池 + RAII 预留）
│   │   └── role.rs                 角色解析与 apply_role_to_config
│   ├── thread_manager.rs           ThreadManagerState：全局线程表 + spawn/fork/resume + 版本判定
│   ├── codex_thread.rs             CodexThread：线程外壳（start_or_steer_turn、inject_fragment_without_turn、shutdown_and_wait）
│   ├── agent_communication.rs      通信审计 tracing（send/receive 事件）
│   ├── session/
│   │   ├── input_queue.rs          InputQueue：信箱 + 活动信号 watch channel
│   │   └── multi_agents.rs         usage hints 解析
│   └── tools/handlers/
│       ├── multi_agents_spec.rs    V1/V2 工具 schema（namespace = "multi_agent_v1"）
│       ├── multi_agents_common.rs  共享：config 构建/角色叠加/模型校验/thread_spawn_source
│       ├── multi_agents/           V1 handlers（spawn_agent/send_input/wait_agent/close_agent/resume_agent）
│       └── multi_agents_v2/        V2 handlers（spawn_agent/send_message/followup_task/wait_agent/list_agents/interrupt_agent/resume_agent）
├── protocol/src/
│   ├── protocol.rs                 InterAgentCommunication / SessionSource / SubAgentSource / MultiAgentVersion
│   └── agent_path.rs               AgentPath（路径类型）
├── agent-graph-store/              thread_spawn 持久化边（Open/Closed 状态）
├── agent-roles/                    角色 TOML 加载（description / config 覆盖 / nickname_candidates）
└── core/assets/agent/agent_names.txt   默认昵称池
```

V1/V2 工具对照（`multi_agents_spec.rs`）：

| 能力 | V1（`multi_agent_v1::` 前缀） | V2（plain 名） |
| --- | --- | --- |
| 生成 | `spawn_agent`（fork_context: bool） | `spawn_agent`（fork_turns: "all"/"none"/N，task_name，agent_type） |
| 消息 | `send_input` | `send_message`（QueueOnly）/ `followup_task`（TriggerTurn，禁打根代理） |
| 等待 | `wait_agent`（等指定子代理终态） | `wait_agent`（等本会话任意活动信号，带 min/max/default 超时夹取） |
| 中断/关闭 | `close_agent` | `interrupt_agent` |
| 枚举/恢复 | `resume_agent` | `list_agents`（path_prefix 过滤）+ `resume_agent` |

## 3. 核心数据类型

### 3.1 `AgentPath`（protocol/src/agent_path.rs）

文件系统式路径（如 `/root/analysis/writer`）。`ROOT = "/root"`。关键操作：`join(segment)` 派生子路径（`thread_spawn_source` 用父 path join task_name）、`try_from(&str)` 解析、`rsplit_once('/')` 取父路径（completion watcher 用它从子 path 反推父）。V2 的所有消息路由都以 AgentPath 为地址；ThreadId 只在进程内索引。

### 3.2 `SessionSource` / `SubAgentSource`（protocol）

```rust
SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
    parent_thread_id,   // 所有权链
    depth,              // 根=0，子=父+1（registry.rs:87 next_thread_spawn_depth）
    agent_path,         // 树状地址（可为 None：BFS 恢复的降级路径）
    agent_nickname,     // 恢复时兜底显示
    agent_role,         // 冷重载时重新应用角色限制
})
```

随 rollout 的 SessionMeta 持久化，是恢复身份的唯一来源。`Review` / `Compact` / `MemoryConsolidation` 等其他 `SubAgentSource` 变体同样走子代理基建，但 depth=0、不参与 ThreadSpawn 树。

### 3.3 `InterAgentCommunication`（protocol.rs:803）

```rust
pub struct InterAgentCommunication {
    pub id: Option<ResponseItemId>,       // 提交后由会话赋 id（审计日志用）
    pub author: AgentPath,                // 发送方路径
    pub recipient: AgentPath,             // 主接收方
    pub other_recipients: Vec<AgentPath>, // 广播
    pub content: String,                  // 明文内容
    pub encrypted_content: Option<String>,// 密文通道（new_encrypted 构造）
    pub internal_chat_message_metadata_passthrough: Option<...>, // 聊天元数据透传（含 turn id）
    pub trigger_turn: bool,               // true=启动新回合；false=进信箱
}
```

`to_response_input_item()`（protocol.rs:868）把通信序列化为 assistant `Commentary` 消息进模型上下文；`to_model_input_item()` 优先取密文。**同一个原语表达 spawn 初始消息、followup、通知、结果回传四种语义**，靠 `AgentCommunicationKind`（agent_communication.rs:7：Spawn/Message/Followup/Result）区分审计类别。

### 3.4 `AgentRegistry`（core/src/agent/registry.rs:25）

```rust
pub(crate) struct AgentRegistry {
    active_agents: Mutex<ActiveAgents>,   // agent_tree: HashMap<path, AgentMetadata>
                                         // thread_paths: HashMap<ThreadId, RegisteredAgent{path, evicted_environments}>
                                         // used_agent_nicknames + nickname_reset_count
    total_count: AtomicUsize,             // 会话树内子代理总数（不含 root）
}
```

双索引：path→metadata（树形状 + `live_agents()` 枚举 + `list_agents` 的 path_prefix 前缀过滤）与 threadId→path（工具收到 agent_id 后反查）。`RegisteredAgent.evicted_environments` 是**驱逐时保存的环境选择快照**（见 §11）。

### 3.5 `AgentMetadata`（registry.rs:53）

```rust
pub(crate) struct AgentMetadata {
    pub agent_id: Option<ThreadId>,     // spawn 完成后回填
    pub agent_path: Option<AgentPath>,
    pub agent_nickname: Option<String>,
    pub agent_role: Option<String>,
}
```

预留期 agent_id=None（`release_reserved_agent_path` 仅清除"无 id"的占位项——区分预留与已提交）。commit 时 `register_spawned_thread` 双向落账，同 path 旧 thread 被移除（同 task_name 重 spawn 的替换语义）。

### 3.6 `SpawnReservation`（registry.rs:356）—— RAII 预留

```rust
pub(crate) struct SpawnReservation {
    state: Arc<AgentRegistry>,
    active: bool,
    reserved_agent_nickname: Option<String>,
    reserved_agent_path: Option<AgentPath>,
}
impl SpawnReservation {
    pub(crate) fn commit(mut self, agent_metadata: AgentMetadata) { /* 落账，active=false */ }
}
impl Drop for SpawnReservation {
    fn drop(&mut self) { if self.active { /* 释放路径预留 + total_count-1 */ } }
}
```

`reserve_spawn_slot(max_threads)`（registry.rs:96）：`max_threads=None` 直接计数（恢复元数据/V2 驻留路径）；`Some(n)` 走 `try_increment_spawned` 的 **CAS 循环**（registry.rs:337）——load → 检查 `>= max` → `compare_exchange_weak` 重试，超限报 `AgentLimitReached`。**半途失败的 spawn 零泄漏**：路径、昵称、计数三者都挂在 Drop 回滚上。

## 4. AgentControl：树级控制面

### 4.1 构造与注入

```rust
// control.rs:148
pub(crate) fn new(manager: Weak<ThreadManagerState>, thread_id_generator: ThreadIdGenerator,
                  rollout_budget: Option<RolloutBudgetConfig>) -> Self
// control.rs:169
pub(crate) fn with_session_id(mut self, session_id: SessionId, max_threads: usize) -> Self {
    self.session_id = session_id;
    self.agent_execution_limiter.initialize(max_threads);  // OnceLock：首个根会话定上限
    self
}
```

字段全是 `Arc`：`state: Arc<AgentRegistry>`、`v2_residency: Arc<V2Residency>`、`agent_execution_limiter: Arc<AgentExecutionLimiter>`、`rollout_budget: Arc<RolloutBudget>`、`root_service_tier: Arc<ArcSwapOption<String>>`——clone 即共享。**根线程首次创建，子线程 spawn 时经 `spawn_new_thread_with_source(..., agent_control: self.clone(), ...)` 传入同一实例**。`upgrade()`（control.rs:721）把 Weak 升级为 `Arc<ThreadManagerState>`，失败报 "thread manager dropped"。

### 4.2 方法面（按调用者分组）

| 方法 | 行 | 职责 |
| --- | --- | --- |
| `spawn_agent_with_communication` | spawn.rs:279 | V2 工具入口：prompt 即 InterAgentCommunication |
| `spawn_agent_with_metadata` | spawn.rs:263 | V1/内部入口：UserInput 列表 |
| `send_input` | control.rs:188 | V1 `send_input` 工具：`start_or_steer_turn`，Steered 时**生成新 UUID 返回**（保持工具面 ID 唯一，control.rs:200-207） |
| `send_inter_agent_communication` | control.rs:217 | 统一投递：trigger_turn 先 `ensure_execution_capacity_for_turn_start` 再提交 |
| `submit_inter_agent_communication` | control.rs:307 | 经 `Op::InterAgentCommunication` 进会话事件循环；触发审计日志 |
| `interrupt_agent` | control.rs:356 | `Op::Interrupt` |
| `maybe_start_completion_watcher` | control.rs:573 | V1 结果回传（§10） |
| `list_agents` / `get_status` / `ensure_agent_known` | control.rs:280/391/416 | 元数据查询（list 支持 path_prefix/include_root） |
| `ensure_v2_agent_loaded` | spawn.rs:315 | 驱逐后重载（§11.3） |
| `restore_v2_agent_metadata` | spawn.rs:174 | 恢复期只重建元数据（§13） |
| `handle_thread_request_result` | control.rs:373 | **子代理死亡清理**：`InternalAgentDied` → remove_thread + forget_v2_residency + release_spawned_thread（配额归还） |

## 5. Spawn 全链路（V2 主线）

### 5.1 时序

```
模型调用 spawn_agent
  │ multi_agents_v2/spawn.rs:93 handle_spawn_agent
  ├─ 解析 args（fork_turns → SpawnAgentForkMode；V2 拒绝 fork_context 参数）
  ├─ next_thread_spawn_depth(turn.session_source) + 1   ← 深度来自父会话源
  ├─ build_agent_spawn_config(base_instructions, turn)   ← §5.2
  ├─ apply_requested_spawn_agent_model_overrides(...)    ← 模型/推理力度覆盖 + 校验
  ├─ apply_spawn_agent_role(...)                         ← 角色叠加（full-fork+无角色时继承父 developer_instructions）
  ├─ apply_spawn_agent_service_tier(...)                 ← 根 service_tier 按子模型能力裁剪
  ├─ apply_spawn_agent_runtime_overrides(...)            ← 权限运行时态回填（§5.2）
  ├─ thread_spawn_source(...)                            ← AgentPath = 父path.join(task_name)
  ├─ communication_from_tool_message(..., trigger_turn=true) ← 初始消息
  └─ AgentControl::spawn_agent_with_communication(...)
       │ spawn.rs:603 spawn_agent_internal
       ├─ effective_multi_agent_version_for_spawn(...)   ← 三级版本解析（§5.3）
       ├─ ensure_execution_capacity(version, source)     ← V2 并发预检
       ├─ [V2] reserve_v2_residency_slot()               ← 驻留槽（满则驱逐，§11）
       ├─ reserve_spawn_slot(max_threads)                 ← 总量 RAII（V2 驻留路径传 None 免重复计）
       ├─ inherited_environments_for_source / inherited_exec_policy_for_source
       ├─ prepare_thread_spawn()                          ← depth==1 时惰性注册根；预留 path+昵称
       ├─ fork? spawn_forked_thread : spawn_new_thread_with_source
       ├─ reservation.commit(metadata) [+ residency_slot.commit(thread_id)]
       ├─ emit_subagent_session_started(...)（父客户端元数据透传）
       ├─ notify_thread_created(thread_id)                ← 客户端订阅/排水新线程
       ├─ persist_thread_spawn_edge_for_source(...)       ← agent-graph-store Open 边
       ├─ 投递初始输入（SpawnInitialInput，§5.4）
       └─ [V1] maybe_start_completion_watcher(...)        ← V2 不起（等信箱回传）
```

### 5.2 配置继承管线（multi_agents_common.rs）

`build_agent_shared_config`（:195）从 `turn.config` 克隆起步，然后：

| 步骤 | 函数 | 要点 |
| --- | --- | --- |
| 模型定格 | — | `config.model = turn.model_info().slug`；provider、reasoning summary 同步 |
| V2 子代理指令 | — | `multi_agent_v2.subagent_developer_instructions` 覆盖 developer_instructions |
| 运行时态回填 | `apply_spawn_agent_runtime_overrides`（:238） | **approval_policy、approvals_reviewer、cwd、permission_profile snapshot 全部从活 turn 拉取**——注释明言：绕过此步克隆陈旧 config 会让孩子带着错误的 provider 或运行时策略出门 |
| 模型覆盖 | `apply_requested_spawn_agent_model_overrides`（:267） | `model`/`reasoning_effort` 或 `agent_default_subagent_*` 配置默认；模型必须在可用列表且 `model_supports_multi_agent_backend`；推理力度按模型 supported levels 校验 |
| 角色叠加 | `apply_spawn_agent_role`（:355）→ `apply_role_to_config` | TOML 层叠；若角色改了模型，重新校验推理力度。**顺序关键：角色在前、runtime overrides 在后**——角色改不掉权限边界 |
| 服务层级 | `apply_spawn_agent_service_tier`（:325） | 根的 service_tier 下传，子模型不支持则丢弃 |

`build_agent_spawn_config` vs `build_agent_resume_config`（:177/:187）：后者 base_instructions 置 None（沿用 rollout 里的），且不重新应用 spawn-only 的步骤。

### 5.3 版本解析（thread_manager.rs:1568）

```
override（config.multi_agent_version_override）
  └─ 继承（父线程的 multi_agent_version：resumed 沿用会话历史、forked 沿用 fork 源、ThreadSpawn 沿用 parent_thread_id）
      └─ config.multi_agent_version_from_features()
```

混版本树安全：V1 父 spawn 的子是 V1（起 watcher），V2 父的子是 V2（信箱回传）——completion watcher 按**子线程实际版本**分流（control.rs:610-651），不是按父。

### 5.4 初始输入延迟投递（spawn.rs:38）

```rust
enum SpawnInitialInput {
    UserInput(Vec<UserInput>),                                  // V1/内部
    InterAgentCommunication(InterAgentCommunication, AgentCommunicationContext), // V2
}
```

V2 把通信与其上下文**绑定成单一值**穿过 spawn 内部——集中投递与生命周期日志不可能"只见消息不见上下文"。投递走 `send_inter_agent_communication_after_capacity_check`：**等执行容量确认后才发**，spawn 返回的 LiveAgent 里 status 是投递时刻的快照。

## 6. 身份与命名

### 6.1 昵称池（registry.rs:264）

`reserve_agent_nickname(names, preferred)`：preferred 优先（恢复场景）；否则从候选（角色 `nickname_candidates` 或 `assets/agent/agent_names.txt`）过滤未用后随机。**池耗尽**：清空 used 集合、`nickname_reset_count += 1`、打 `codex.multi_agent.nickname_pool_reset` 指标，然后 `format_agent_nickname`（:60）生成 "Atlas the 2nd" 式序号（含 11-13 的 th 特判）。

### 6.2 路径预留（registry.rs:304）

`reserve_agent_path`：`agent_tree.entry(path)` Occupied 即报 "agent path already exists"——**路径即互斥锁**，同 task_name 并发 spawn 只有一个成功。Vacant 插入 `agent_id: None` 占位。`release_reserved_agent_path`（:323）只删"无 id"的占位项，不误伤已提交代理。

### 6.3 深度

`next_thread_spawn_depth`（registry.rs:87）= 父 session_source depth + 1（saturating）。超限检查在工具层（V2 spawn 前置检查；V1 在 handler 内），`exceeds_thread_spawn_depth_limit`（:91）报错文案即"Solve the task yourself."。注意 depth 记在 **SessionSource** 上随 rollout 持久化，跨进程恢复后仍正确。

## 7. 配额三层总览

| 层 | 结构 | 计什么 | 何时检 | 何时放 |
| --- | --- | --- | --- | --- |
| 总量 | `AgentRegistry.total_count` | 树内已 spawn 子代理数（root 不计） | reserve_spawn_slot（CAS） | Drop 未 commit / release_spawned_thread（死亡清理） |
| 并发 | `AgentExecutionLimiter`（execution.rs:14） | V2 下**同时执行回合**的子代理数 | spawn 预检 + 每次回合启动复检（`ensure_execution_capacity_for_turn_start`，已在跑的回合豁免） | `AgentExecutionGuard` Drop（回合结束） |
| 驻留 | `V2Residency`（residency.rs:18） | 常驻内存的 V2 子代理数（residents + pending_slots ≤ capacity） | reserve_v2_residency_slot | V2ResidencySlot Drop（未 commit 释放 pending） |

`AgentExecutionLimiter.max_threads` 是 `OnceLock`——`with_session_id` 首次初始化即固化（control.rs:171）。`is_execution_limited`（execution.rs:91）只对 `V2 + SubAgent` 生效：根线程与 V1 不受限。

## 8. 通信模型（V2 信箱）

### 8.1 发送侧：send_message / followup_task 共路（message_tool.rs:53）

```
handle_message_string_tool(invocation, mode, target, message)
  ├─ resolve_agent_target(session, turn, target)      ← target 是 path/nickname/threadId 的模糊解析
  ├─ ensure_agent_known(receiver)                      ← registry 反查元数据
  ├─ [TriggerTurn] 禁止 recipient.is_root()            ← followup 不能打根代理
  ├─ build_agent_resume_config(turn) + ensure_v2_agent_loaded(receiver, parent=None)  ← 被驱逐则先重载（§11.3）
  ├─ communication_from_tool_message(author, receiver_path, message, source, mode.trigger_turn())
  └─ send_inter_agent_communication(receiver, communication, context, TurnStartOptions{
         parent_turn_id: TriggerTurn 时 = 本回合 sub_id,   ← 归因链
         root_turn_id:   turn_metadata_state.root_turn_id(),
         ...})
```

`MessageDeliveryMode`（:13）：QueueOnly（send_message）/ TriggerTurn（followup_task）——**唯一差别就是 trigger_turn 布尔**。

### 8.2 接收侧：InputQueue（session/input_queue.rs:81）

```rust
pub(crate) struct InputQueue {
    activity_tx: watch::Sender<InputQueueActivity>,        // Mailbox | Steer 两种活动信号
    mailbox_pending_mails: Mutex<VecDeque<PendingMailboxCommunication>>, // 信箱
}
```

- `enqueue_mailbox_communication`（:124）：入队 + `send_replace(Mailbox)` 广播活动。附 `GaugeGuard` 追踪 `core.mailbox.pending` 指标。
- **QueueOnly 投递**：`Op::InterAgentCommunication` 进会话事件循环后，非 trigger_turn 的消息走 `enqueue_mailbox_communication`（不动当前回合）。
- `drain_mailbox_input_items`（:152）：**批量清空**信箱进下一回合。start_options 合并规则（有单测锁定，:561/:608）：
  - `trigger_turn=false` 的邮件不贡献 start_options；
  - **后到的 followup 覆盖先前的选择（含显式省略）**——supersede 语义；
  - `parent_turn_id`：全部 trigger 邮件一致才用，有歧义置 None；
  - `root_turn_id`：取首个 trigger 邮件的（父 id 非空为前提）。
- **MailboxDeliveryPhase**（:213/:236）：回合内的投递相位控制——`defer_mailbox_delivery_to_next_turn` 把"仅含 queue-only 邮件"的待投输入推迟到下一回合（不中断采样）；`accept_mailbox_delivery_for_current_turn` 允许当前回合立即吃进。
- `get_pending_input`（:291）：原子地（锁 active_turn + turn_state）合并回合内 pending 与信箱 drain。

### 8.3 wait_agent（multi_agents_v2/wait.rs）

**不是等某个子代理，而是订阅本会话的活动信号**：`subscribe_activity(turn_state)` 返回 (watch::Receiver, 已有活动快照)。等待结果三态：MailboxActivity（"Wait completed."）/ Steered（"Wait interrupted by new input."）/ TimedOut。超时三段夹取：`min_wait_timeout_ms ≤ requested ≤ max_wait_timeout_ms`，未给用 `default_wait_timeout_ms`；请求低于下限时结果里附加说明。等待前后发 `TurnItem::CollabAgentToolCall(Wait, InProgress→Completed)` 供 UI 渲染。

### 8.4 结果回传（completion watcher 的 V2 分支，control.rs:616-651）

子代理到终态后，watcher 从 `child_agent_path.rsplit_once('/')` 反推父路径，`format_inter_agent_completion_message` 生成摘要，发 `InterAgentCommunication{kind: Result, trigger_turn: false}` 进父信箱——**子→父的结果投递与普通消息同构**，父在下个 drain 周期统一消费。

### 8.5 通信审计（agent_communication.rs）

专属 tracing target `codex_otel.agent_communication`：send 事件带 communication_id/kind/sender/receiver/content（密文或 `[plaintext]` 占位）；receive 事件带 id。`logging_enabled()` 先探测再克隆消息，避免无谓开销。

## 9. 通信模型（V1 legacy）

- `multi_agent_v1::send_input` → `AgentControl::send_input`（control.rs:188）：`start_or_steer_turn`——子正在跑则 Steered，**返回新造 UUID** 保持工具面 submission id 唯一（control.rs:200-207 注释详述此兼容动机）。
- 结果回传走 watcher 的 **V1 分支**（control.rs:653-661）：`inject_fragment_without_turn(SubagentNotification{child_reference, status})`——**不打断父当前回合**注入上下文片段。child_reference 优先 agent_path 字符串。
- `multi_agent_v1::wait_agent` 等指定子代理终态（带超时选项）；`close_agent` 关闭。

## 10. Fork：上下文分叉（control/spawn.rs:809）

### 10.1 模式

V2 `fork_turns: "all"（默认）| "none" | N`（spawn.rs:291 解析，0 拒绝）；V1 `fork_context: bool`（= FullHistory）。fork 必须 `fork_parent_spawn_call_id`（工具 call_id）。

### 10.2 流程

```
spawn_forked_thread
  ├─ 父线程 ensure_rollout_materialized + flush_rollout     ← 注释：record_conversation_items 异步持久化，必须先冲刷
  ├─ load_agent_model_context(父) → forked_rollout_items    ← Legacy 全量 / Paginated 最新页
  ├─ [LastNTurns] truncate_rollout_to_last_n_fork_turns
  ├─ 收集父 usage hints 文本（V2，用于剥离）
  ├─ preserve_reference_context_item = FullHistory && 最后一个 Compacted 有 replacement_history
  ├─ retain_mut 逐项过滤（§10.3）
  ├─ [V2] 兜底补子代理 developer_instructions / subagent usage hint（防 FullHistory 引用上下文丢失指令）
  └─ fork_thread_with_source(InitialHistory::Forked(items), ...)
```

### 10.3 过滤规则（keep_forked_rollout_item :65 + retain_forked_item :936）

| RolloutItem | 处理 |
| --- | --- |
| system/developer/user 消息 | 保留；developer 经 `retain_forked_developer_message`（:109）**剥离子代理角色指令、多代理 usage hints、时间提醒、guardian 授权痕迹**；user 消息打 `inherited_user_message` 元数据标记 |
| assistant 消息 | 仅 `phase == FinalAnswer` 保留 |
| FunctionCallOutput(call_id: None) / ConfigurationUpdate | 保留（无副作用注入） |
| 其余工具调用/输出、Reasoning、AgentMessage、Compaction 触发器、TokenUsageRecord、RetainedContext、SecurityRiskScore | **丢弃**——孩子继承模型上下文，不继承父的累计用量与工具噪音 |
| Compacted | `guardian_history = None`（**父的本地授权证据不得变成子的授权**）；`retained_context` 仅 V2 重置为空默认（V1 保持不完整——V2 会活取根授权）；`latest_token_usage_record = None`；replacement_history 同规则递归清洗 |
| TurnContext / WorldState | 仅 FullHistory 保留（**缓存 prompt 前缀复用**：孩子直接从父的 durable 基线继续 diff，KV cache 免重建）；V2 额外移除 WorldState 里的 `multi_agent_usage_hint` |
| InterAgentCommunication | 丢弃（孩子的通信历史从零开始） |

`preserve_reference_context_item` 的细节（:917-931）：若最后一个 Compacted 是**旧版无 replacement_history 的 checkpoint**，强制 false——旧 checkpoint 不携带可复用的引用基线，孩子必须重建上下文。

## 11. 驻留治理：V2Residency（control/residency.rs）

### 11.1 槽位与驱逐

```rust
struct V2ResidencyState { residents: VecDeque<ThreadId>, pending_slots: usize }  // LRU 队列
```

`reserve_slot`（:81）循环：`try_reserve_pending_slot`（residents+pending < capacity 则 pending+1）失败则 `try_unload_one_resident`，仍失败报 `AgentLimitReached`。`V2ResidencySlot` 同款 RAII：commit 落账（pending-1 + touch LRU 尾部），Drop 释放 pending。

### 11.2 驱逐判定

- 候选：LRU 头部弹出（`pop_lru_candidate`，protected_thread_id 保护当前正在处理的线程——比如 ensure_v2_agent_loaded 正要给某个嵌套父腾位时不能驱逐它自己）。
- `is_resident_candidate`（:224）：V2 + SubAgent 源。
- `is_unloadable`（:233）：**终态（Completed/Errored/Interrupted）&& 无活动回合 && 信箱无待处理**。不可驱逐的 touch 回队尾重试下一个。

驱逐动作（:139-155）：`ensure_rollout_materialized` → `shutdown_and_wait` → **`save_evicted_environments`**（环境选择存进 Registry）→ `manager.remove_thread`。对模型完全透明。

### 11.3 重载：ensure_v2_agent_loaded（spawn.rs:315）

两级模式：`parent = Some`（owner-validated，followup/嵌套场景）或 `None`（sender-driven）。**校验链**（任一失败即 InvalidRequest）：

1. 父必须已注册于 manager 且 `Arc::ptr_eq` 同一实例、is_running、V2、同一 AgentControl state（防跨树引用）；
2. 三方 parent 记录一致：session_source.parent_thread_id == resumed 记录 == stored 记录；
3. 已加载则 `validate_loaded_v2_child`（is_running + V2 + parent 匹配 + ptr_eq）后 touch LRU 直接返回；
4. 角色重应用后**再次回填运行时权限**（approval_policy/approvals_reviewer/cwd/permission_profile snapshot——与 spawn 同构，冷重载不放宽角色限制）；
5. `child_uses_parent_exec_policy` 校验父执行策略未变；
6. 被驱逐环境逐项比对：`FromThread` 状态重钉为父当前配置（"Pin current owner authority instead of re-inferring child settings"）；子配置与父不一致即拒；权限差异走 `intersect_effective_permission_profiles` **收窄**（远程执行器上权限变更直接拒绝）；本地路径才允许 materialize+intersect；
7. reserve 新驻留槽（**注释点破顺序玄机：预约本身可能驱逐一个空闲的嵌套父，所以先在第 6 步捕获其授权**）；
8. `resume_thread_with_history_with_source` 成功 → validate → `clear_evicted_environments` → commit → notify；失败但线程已在（并发重载）→ 降级为成功。

## 12. 持久化与恢复

### 12.1 agent-graph-store

方向性 `thread_spawn` 边（parent→child）+ `ThreadSpawnEdgeStatus::{Open, Closed}`。写入点：`persist_thread_spawn_edge_for_source`（spawn/resume 都调）；查询点：恢复遍历。**子代理的存活历史比线程对象长寿**——内存里驱逐/死亡的代理，图上的 Open 边仍在。

### 12.2 V2 恢复（restore_v2_agent_metadata，spawn.rs:174）

从 agent_graph_store 拉 root 的 Open 后代，逐个：读 stored_thread（agent_path/role/nickname 兜底链：显式列 > session_source）→ `reserve_spawn_slot(None)`（不设上限——恢复已有代理不该被 max_threads 卡住）→ `prepare_agent_metadata` → **metadata.agent_id = thread_id** → commit。**不重开运行时**——与驻留驱逐配合，"存活代理数"与"驻留线程数"解耦，惰性加载。

### 12.3 V1 恢复（resume_agent_from_rollout，spawn.rs:1127）

恢复根代理后 **BFS 整棵 spawn 树**（队列携带深度）：每个 Open 子边构造降级 SessionSource（path/role/nickname 为 None，靠 stored_thread 兜底）→ `resume_single_agent_from_rollout`。单个失败 warn 后继续（尽力恢复）。V2 直接短路（元数据已在 12.2 恢复）。

`resume_single_agent_from_rollout`（:1202）与 spawn 对称：同样的 reserve→prepare→resume→commit→notify→persist_edge→[V1]watcher。

## 13. 安全边界汇总（实现者清单）

1. **运行时权限回填**（spawn 与 reload 同构）：角色/配置叠加后，approval_policy、cwd、permission_profile 一律从**活 turn/活父会话**强制恢复——角色与持久化配置都换不掉权限边界（multi_agents_common.rs:238、spawn.rs:400-436）。
2. **fork 授权剥离**：guardian_history 置 None、guardian.approved_action 内容项滤除、inherited_user_message 标记防 resume 重捕获（spawn.rs:1029-1037、:941-947、:121-126）。
3. **重载所有权校验**：ptr_eq + 三方 parent 一致 + exec policy 未变 + 环境逐项比对（§11.3）。
4. **权限只收窄**：intersect_effective_permission_profiles；远程执行器权限差异直接拒绝（spawn.rs:509-513）。
5. **followup 不可打根**（message_tool.rs:75-84）；空消息拒绝（:43）。
6. **序列化边界**：annotated response item 禁止过 turn-input 序列化边界（input_queue.rs:36-66 的 serde wrapper 显式拒绝 metadata，防伪造 client_authored——附测试锁定 forged 输入被洗掉 metadata）。

## 14. 观测面

| 信号 | 位置 | 内容 |
| --- | --- | --- |
| `TurnItem::CollabAgentToolCall` | 各工具 handler | sender/receiver/prompt/model/reasoning_effort/agents_states——UI 协作时间线 |
| `TurnItem::SubAgentActivity` | spawn/message handler 经 `emit_sub_agent_activity`（control.rs:240） | Started/Interacted 活动 |
| tracing `codex_otel.agent_communication` | agent_communication.rs | send/receive，内容密文或 [plaintext] |
| counter `codex.multi_agent.spawn` | spawn.rs:253 | role + version 标签 |
| counter `codex.multi_agent.nickname_pool_reset` | registry.rs:285 | 昵称池重置 |
| gauge `core.mailbox.pending` | input_queue.rs:18 | 信箱积压（GaugeGuard 随邮件生命周期） |
| analytics `track_collab_tool_call` / `emit_subagent_session_started` | spawn.rs:69/:743 | 工具调用耗时与子会话启动（透传父客户端元数据） |

## 15. 关键不变量（给实现者的验收清单）

1. **恰一次配额占用**：reserve→commit 或 Drop，无泄漏路径（含 fork 失败、驱逐、死亡清理）。
2. **路径唯一**：同 path 并发 spawn 恰一个成功；预留占位不阻塞他人 commit；同 path 重 spawn 是替换而非并存。
3. **昵称唯一**（池内）：耗尽重置计数并加序号，绝不重名。
4. **深度单调**：depth 记在 SessionSource 随 rollout 持久化，跨恢复仍正确；超限拒绝。
5. **terminal 恰一次投递**：V1 注入恰一次（watcher 到终态才动）；V2 Result 消息恰一封（终态不可逆，watcher 不重启）。
6. **信箱 drain 原子性**：邮件全部按序进同一回合；start_options 合并无歧义（有单测表锁定）。
7. **死亡清理完备**：InternalAgentDied → 三处清理（manager/residency/registry 计数）。
8. **驱逐透明性**：被驱逐代理经 ensure_v2_agent_loaded 重载后，授权不宽于驱逐前。
9. **fork 不泄露授权**：guardian 证据剥离 + inherited 标记。
10. **版本一致性**：混版本树中 watcher 分流按子线程实际版本，恢复沿用历史版本。

## 16. 对照 anureo

详见[Phase 1 代码方案](../design/subagent-phase1-code-plan.md)（已按本分析落地排期）。映射速查：

| codex 机制 | anureo 契约对应 | Phase |
| --- | --- | --- |
| SubAgentSource（身份随 rollout 持久化） | SubagentIdentity（GAP-02） | 1 |
| depth 传播链（SessionSource→RunnableConfig） | ReactBuildConfig.depth + checkpointer 修复（GAP-06） | 1 |
| completion watcher 的恰一次 terminal | SubagentLifecycleSink 挂 registry（GAP-03 接缝） | 1→2 |
| AgentRegistry 树+配额 | AsyncAgentRegistry 注入化（GAP-06） | 1→4 |
| V2 信箱 + trigger_turn | 远期（契约外，Phase 5+ 候选） | — |
| V2Residency 驱逐/重载 | 远期 | — |

## 17. 未展开部分

- `thread_manager.rs` 的线程生命周期细节（中断传播、线程表轮换）——见[架构分析](./codex-architecture-20260906.md)。
- TUI `/subagents` 视图与 app-server 事件转发协议。
- `agent-identity` crate（仅确认存在，未见多代理主链路引用）。
- V1 `wait_agent`/`close_agent` handler 细节（multi_agents/ 目录未逐行读，语义从 spec 与 control 方法面推定）。
- `resolve_agent_target` 的模糊解析规则（multi_agents_v2 模块内，未逐行读）。

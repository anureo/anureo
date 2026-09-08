# codex-rs 架构分析（2026-09-06）

> **状态**: 快照分析。基于对 `C:\Users\heycj\dev\codex`（codex-rs）源码的定向阅读：workspace 清单、扩展系统、协议层、状态层、core 引擎的 turn/task/submission 路径、exec 家族、工程治理配置。行号与文件结构为阅读时快照，后续上游演进会产生偏移。
> **方法**: 按「分层 → 每层职责/关键文件/设计要点 → 与本仓（anureo）对照」组织。未逐行阅读的部分（TUI、realtime、cloud-tasks 等）只给定位不展开。
> **交叉参考**: [codex 执行过程分析](./codex-execution-flow-20260906.md)、[Goal 功能改进方案](../goal/goal-improvement-plan.md)、[Codex Goal 功能源码导读](../goal/codex-goal-analysis.md)

---

## 1. 总览：约 140 个 crate 的分层逻辑

codex-rs 是 OpenAI Codex 的 Rust 实现（CLI + TUI + app-server + 多宿主）。workspace 成员约 140 个，但组织有清晰的单向依赖原则：

```
protocol（纯类型，无逻辑）
    ↑
config / features / prompts / tools（纯定义层）
    ↑
core（引擎：Session、turn 循环、工具编排、沙箱调用）
    ↑
core-api（窄门面：#![deny(private_bounds, private_interfaces, unreachable_pub)]）
    ↑
宿主：cli / tui / mcp-server / app-server
         ↑
app-server-protocol（v1/v2 版本化 + TS 导出）→ app-server-client / app-server-test-client

ext/extension-api（贡献者 trait 集）← ext/goal、ext/memories、ext/skills…（扩展只依赖 api）
state / rollout（持久化：SQLite 状态 + JSONL 事件流，互相独立，被 core 引用）
exec 家族（命令执行：exec / execpolicy / sandboxing / unified_exec / exec-server）
model 家族（model-provider-info / models-manager / codex-api / codex-client / login）
utils/*（一函数一 crate 的工具集）
```

核心思想四句话：

1. **类型向内收敛**——`protocol` crate 只有类型没有逻辑，`core` 依赖它而非定义它；
2. **依赖向外单向**——宿主（app-server/tui）只见 `core-api` 门面，不碰 core 内部；
3. **扩展只见贡献接口**——`ext/*` 依赖 `extension-api`，不依赖 core；
4. **协议自带版本与导出**——`app-server-protocol` 有 v1/v2 目录和 TypeScript 生成器。

## 2. protocol：纯类型层

- **定位**：`Op`（宿主 → core 的操作枚举）、`EventMsg`（core → 宿主的事件枚举）、`ResponseItem`（模型对话项）、`TokenUsage`、`TurnAbortReason`、config 类型（`ModeKind`、`ShellEnvironmentPolicy` 等）全部在此定义。无 async、无 IO。
- **设计要点**：core 的一切输入输出都先变成 protocol 类型，宿主与 core 之间的契约因此可以独立编译、独立测试（`codex-mcp`、`app-server-protocol` 都直接复用它）。
- **对照 anureo**：anureo 的协议面是外部 `agent-client-protocol` crate + `foundation/stream-event`；`_anureo.dev/*` 扩展类型散在各扩展文件（手写 `serde_json::json!` 响应），缺一个集中的「纯类型」层。

## 3. config / features：配置与特性开关

- **config**：分层配置栈（`ConfigLayerStack`、`ProjectConfig`、用户 config.toml），环境变量优先级明确。
- **features**（`features/src/lib.rs`）：**特性开关带生命周期 Stage**——`UnderDevelopment / Experimental{name, menu_description, announcement} / Stable / Deprecated / Removed`。实验功能有「菜单里叫什么、公告说什么」的元数据；带 `legacy_feature_keys` 迁移旧开关名。由 config 解析出有效 feature 集，core 各处用 `Feature::X` 查询。
- **对照 anureo**：`experimental/*` 目录靠位置表达成熟度，无代码级开关体系；goal/task 的"实验性"只写在文档里。借鉴点：引入 Stage 化 feature 注册表，让"哪些能默认开"变成代码问题。

## 4. core 引擎：Session / tasks / turn 循环

core 是最大的 crate（`core/src` 单文件超 100KB 的有 session/mod.rs 168KB、turn.rs 110KB），内部再分层：

| 模块 | 职责 |
|---|---|
| `thread_manager.rs`（85KB）| Thread（会话）生命周期：创建、恢复、fork、中断、线程列表 |
| `session/` | 单个 Session 的运行时：`session.rs`（状态与事件发送）、`turn.rs`（turn 主循环，见执行过程文档）、`input_queue.rs`（steering/interrupt 输入队列）、`handlers.rs`（Op 分发 submission_loop）、`turn_context.rs`（每 turn 的模型/策略快照）、`rollout_reconstruction.rs`（从持久化事件重建对话） |
| `tasks/` | `SessionTask` trait（`mod.rs`）：Regular/Compact/Review/UserShell 四种任务；任务在独立 tokio task 上跑，session 持 `RunningTask{done, handle, cancellation_token}` |
| `tools/` | 工具编排：`registry.rs`（注册）、`router.rs`（分发）、`orchestrator.rs`、`parallel.rs`（并行执行策略）、`approvals.rs`/`network_approval.rs`（审批）、`sandboxing.rs`（沙箱策略判定）、`spec_plan.rs`（工具规格计划） |
| `agent/` | 多 agent：`registry.rs`（agent 注册）、`role.rs`（角色定义，内置 `builtins/*.toml`）、`control/`（spawn/residency/execution——子 agent 的生成与驻留） |
| `exec.rs` + `exec_policy.rs` | shell 命令执行与策略（见执行过程文档 §工具执行） |
| `mcp_tool_call.rs` + `session/mcp.rs` | MCP 服务器管理与工具调用 |
| `compact.rs` / `compact_remote*.rs` | 上下文压缩（本地摘要与远端压缩） |
| `unified_exec/` | 统一进程管理（后台进程、head-tail 输出缓冲、async watcher） |
| `context/`、`context_manager/` | 上下文片段（context-fragments crate 的消费端） |

- **设计要点**：
  - **Op 队列模型**：宿主对 core 的一切操作（`Op::TurnInput`、`Op::Interrupt`、`Op::ExecApproval`…）经 channel 进 `submission_loop`（handlers.rs:515）串行分发——core 内部没有宿主直接调用栈，全部消息化；
  - **steering 优先于 interrupt**：新输入在 turn 进行中排队为 steer（input_queue），turn 自然收尾时消费；显式 Op::Interrupt 才硬中断（详见执行过程文档）；
  - **生命周期钩子内建**：turn start/stop/abort/error、thread idle 都有 extension contributor 发射点（tasks/lifecycle.rs）。
- **对照 anureo**：`agent/agent-core` 是同位物，但无 Op 队列消息化（调用直接进 runtime）、无 steering/interrupt 区分（prompt 即 turn）、生命周期钩子无统一发射点（goal/extension 只能各自 hook）。

## 5. ext/extension-api + ext/*：扩展系统（与 anureo 反差最大）

- **设计**：没有"扩展"这个大 trait，而是**约 12 个细粒度 contributor trait**（`ext/extension-api/src/registry.rs`）：
  - `ThreadLifecycleContributor`（start/resume/idle/stop）
  - `TurnLifecycleContributor`（start/stop/abort/error）
  - `ToolContributor`（向模型贡献工具）+ `ToolLifecycleContributor`（工具开始/结束钩子）
  - `TurnInputContributor`（注入 steering/continuation 输入）
  - `TurnItemContributor`（有序注入对话项）
  - `ContextContributor`（贡献 prompt 片段，带 `PromptSlot` 位置语义）
  - `McpServerContributor`（贡献运行时 MCP server）
  - `TokenUsageContributor`、`ApprovalReviewContributor`、`ConfigContributor`、`SkillInvocationContributor`
  - builder 注册 → **不可变 `ExtensionRegistry<C>`**，宿主按能力切片查询；扩展状态分 scope 存储（session/thread/turn 三级 `ExtensionData`）。
- **ext/* 家族**：goal、memories、skills、mcp、queue、web-search、image-generation、guardian（安全审查）、git-attribution、connectors、items（共享扩展数据类型）、agent（agent 即扩展）。全部只用 extension-api，core 不 import 它们（编译期隔离）。
- **对照 anureo**：`apps/acp/src/extensions/*` 是 `handle(params: Value) -> Result<Value>` 的 JSON RPC 分发器（ACP 扩展面，职责不同），但 agent 运行时内部需要往对话流注入内容的功能（goal runner、auto-review、未来的 memories）没有贡献者接口，只能硬编码特判。借鉴方式：ACP RPC 面保持不变；运行时内部引入 contributor 模式。

## 6. core-api：窄门面 crate

- **设计**：整个 crate 就是 `pub use codex_core::…` 的 re-export 列表 + `#![deny(private_bounds, private_interfaces, unreachable_pub)]`。宿主（app-server/tui/mcp-server）只依赖它。
- **收益**：core 大规模重构时宿主编译面稳定；`unreachable_pub` 让"本想 crate 内用却标成 pub"的泄漏在 CI 报错。
- **对照 anureo**：apps/acp、apps/server 直接依赖 agent-core 全量。规模尚可暂不需要独立 crate，但 deny lint 纪律值得引入。

## 7. app-server 家族：协议、传输、守护进程、客户端

| crate | 职责 |
|---|---|
| `app-server-protocol` | 类型层：`protocol/v1.rs` 与 `protocol/v2/*.rs` 按域分文件（thread/turn/model/mcp/permissions…）；`export.rs` 用 schemars 生成 TypeScript（`GENERATED_TS_HEADER`）；`schema_fixtures_tests.rs` 防 schema 漂移；实验性方法用 `experimental_fields` 宏 + `EXPERIMENTAL_*_METHODS` 白名单集中管理 |
| `app-server-transport` | 传输层（stdio/UDS/WS 等） |
| `app-server` | 宿主：`message_processor.rs`（入口分发）、`request_processors/`（**每个 RPC 域一个 processor 文件**：thread_goal_processor、turn_processor、config_processor…）、`bespoke_event_handling.rs`（165KB，core 事件 → server 通知的定制映射）、`thread_state.rs`/`thread_status.rs`（线程状态机） |
| `app-server-daemon` / `app-server-client` / `app-server-test-client` | 守护进程化、给前端/测试用的协议客户端 crate |
| `app-server-protocol-noop-macros` | noop 实现宏（测试/桩用） |

- **设计要点**：**协议与实现严格分离**（protocol crate 无逻辑）；**processor-per-domain**（对比 anureo 的 extension-per-domain 是同构的，但 codex 的 processor 是 typed 的）；**TS 导出内建**（OpenChamber/cowork 这类前端不再手抄类型）。
- **对照 anureo**：`_anureo.dev/*` 缺 typed protocol + TS 生成。这是对前端协作 ROI 最高的借鉴点。

## 8. exec 家族：命令执行与安全

| crate | 职责 |
|---|---|
| `exec` | `codex exec` 非交互执行入口 + 事件处理器（human/jsonl 输出两种） |
| `core::exec_policy` + `execpolicy` | 命令策略评估：**Starlark** 编写的策略脚本（外部仓库），决定命令是自动放行/需审批/拒绝；core 侧 `exec_policy.rs` 是其客户端 |
| `sandboxing` | 沙箱策略类型（`SandboxMode`：read-only / workspace-write / danger-full-access）与平台调度 |
| `linux-sandbox` / `windows-sandbox-rs` / bwrap | 平台沙箱实现（landlock/seccomp、Windows 沙箱、bubblewrap） |
| `network-proxy` | 网络代理沙箱：出网请求经代理审批（与 `tools/network_approval.rs` 配合） |
| `core::unified_exec` | 统一进程管理：后台进程驻留、`head_tail_buffer`（输出只保留头尾，防上下文爆炸）、`process_manager`、async watcher |
| `exec-server` / `exec-server-protocol` | 命令在独立进程/server 执行的架构（环境注册、noise 加密通道 rendezvous） |
| `process-hardening` | 进程加固 |

- **设计要点**：**审批与沙箱是两道独立闸门**（approval policy 管"要不要问人"，sandbox policy 管"允许做什么"）；**策略可编程**（Starlark 而非硬编码 if-else）；**输出截断内建**（head-tail buffer 是防上下文膨胀的一等公民，不是后处理）。
- **对照 anureo**：工具执行走 agent/tool + 权限询问（ACP requestPermission），无独立策略层与输出 head-tail 机制。短期借鉴：head-tail buffer 思路可用于 shell 工具输出截断。

## 9. state 家族：持久化

| crate | 职责 |
|---|---|
| `state` | SQLite 状态库：`runtime.rs`/`sqlite.rs`/`migrations.rs` 基础设施 + 领域模块（`thread_goal.rs`、`thread_metadata.rs`、`queued_items/`、`memories/`、`thread_sections.rs`、`graph.rs`）+ `recovery.rs`/`backfill.rs`（恢复与回填是一等公民）；测试同目录（`*_tests.rs`）；audit 日志、log_db、遥测内建 |
| `thread-store` | 线程抽象的存储接口层（core 通过 `PersistContext` 使用） |
| `rollout` | **事件溯源 JSONL**：追加式记录对话项与事件；`ordinal.rs` 定序、`compression.rs` 压缩、`reverse_jsonl_scanner.rs` 反向扫描、`session_index.rs`/`rollout_reference_index.rs` 索引、`recorder.rs` 写入器（带持久化指标） |
| `rollout-trace` | 推理 trace 记录（rollout 之上的采样追踪） |
| `history` / `message-history` | 历史会话列表与消息历史 |

- **设计要点**：**状态（SQLite）与事件流（JSONL）分离**——线程对话可重放（rollout_reconstruction.rs），跨设备状态可查询（state）；**迁移内建**（migrations.rs + 测试）；**恢复路径有专门模块与测试**。
- **对照 anureo**：`foundation/checkpoint-sqlite-store` 是类似思路的 SQLite 快照；goal 的 `.anureo/goals.json` 是无迁移、无跨进程锁的 JSON（见 goal 方案 P4）。借鉴方式：goal 存储迁移直接按 state crate 的组织做。

## 10. model 家族：模型接入

| crate | 职责 |
|---|---|
| `model-provider-info` | provider 定义（内置 provider 表、`OPENAI_PROVIDER_ID`） |
| `model-provider` | provider 抽象（流式能力、远端压缩支持等） |
| `models-manager` | 模型列表管理与 etag 刷新（`SharedModelsManager`、`RefreshStrategy`）——SSE 响应里带 ModelsEtag 时自动刷新 |
| `codex-api` / `codex-client` | Responses API 客户端：`client.rs`（core 内 101KB）流式 SSE 解析、`ModelClientSession` 会话预热（prewarm） |
| `responses-api-proxy` | API 代理（本地代理请求） |
| `login` / `chatgpt` / `backend-client` / `workload-identity` / `aws-auth` | 认证与后端接入 |
| `lmstudio` / `ollama` | 本地模型接入 |

- **设计要点**：**预热**（prewarmed client session）：首个 turn 不等握手；**模型列表 etag 化**，流式响应顺带刷新；**usage 结构统一**（TokenUsage 含 cached/reasoning 细分，goal 记账直接消费）。
- **对照 anureo**：`foundation/llm` + `foundation/model-spec-core` 同位。预热与 etag 刷新是可选优化；TokenUsage 细分字段值得对齐（goal 记账需要 input−cached+output）。

## 11. prompts / tools：提示词与工具定义

- **prompts**：所有系统级 prompt 是 `templates/**/*.md` 文件（compact、goals/continuation、permissions/approval_policy/*、review_*），每个模板一个 Rust 加载模块 + 同目录 `*_tests.rs`。prompt 修改可 code review、可 diff。`collaboration-mode-templates` crate 同理（模式模板）。
- **tools**：`json_schema.rs` 提供 `JsonSchema::object/string_enum/integer/…` 构建器（编译期防错的 schema DSL）；`tool_spec.rs` 统一 Responses API 工具形状；`tool_discovery.rs`/`tool_search.rs` 工具发现与检索；`dynamic_tool.rs` 动态工具；每个模块配 `*_tests.rs`。
- **对照 anureo**：goal continuation prompt 是 `message.rs` 里 100 行 format! 硬编码；MCP 工具 schema 手写 JSON。借鉴：模板文件化（goal 方案 P0 顺带）+ JsonSchema 构建器（约 200 行可搬进 `agent/tool`）。

## 12. skills / plugins / hooks / connectors

- `skills`：技能发现与注入（路径→技能名、显式提及解析 `collect_explicit_skill_mentions`）；`ext/skills` 把技能作为扩展贡献进上下文。
- `plugin` + `utils/plugins` + `core-plugins`：插件（外部封装的技能/模板/MCP bundle）加载与推荐（`RecommendedPluginCandidatesInput`）。
- `hooks`：用户可配置 hook（`build_hook_prompt_message`、`run_hooks_and_record_inputs`——hook 结果在 turn 边界排空）。
- `connectors`：外部应用连接器（`AppToolPolicyEvaluator` 管控 app 工具策略）。
- **对照 anureo**：`agent/skill` 同位；hooks 无对应（goal runner 的每轮逻辑近似）。

## 13. 测试基建

- **test-support crate 化**：`core/tests/common`（`core_test_support`）、`app-server/tests/common`（`app_test_support`）、`mcp-server/tests/common`、`exec-server/tests/support` 都是 workspace 成员，被多个集成测试复用。
- **套件规模**：`app-server/tests/suite/` 112 个文件，按域组织（thread_fork、thread_resume、thread_goal…）。
- **快照测试**：大量使用 `insta`（协议序列化快照，`*.snap` 文件入库，如 `session/snapshots/`）。
- **协议测试客户端**：`app-server-test-client` 是可复用的完整协议客户端，测试直接用它打真 server。
- **同目录测试**：单元测试普遍以 `foo.rs` + `foo_tests.rs` 同目录成对出现。
- **对照 anureo**：`.nextest.toml` 体系已有；缺 test-support crate 化与快照测试。ACP 扩展测试可先做 `acp-test-support`（fake connection + typed 断言）。

## 14. 工程治理（workspace hygiene）

根 `Cargo.toml`：

- **`[workspace.lints.clippy]` 30+ 条 deny**：`unwrap_used`、`expect_used`、`await_holding_lock`、`await_holding_invalid_type`、`uninlined_format_args`、`manual_*` 系列、`redundant_clone`……全 workspace 强制，panic 与跨 await 持锁在 CI 即死。
- **`[workspace.dependencies]`**：所有内部/外部依赖版本单点管理（内部 crate 也以 `codex-xxx = { path = … }` 集中声明）。
- **profiles**：`ci-test`（降磁盘压力）、`dev-small`、`profiling`；release 保留符号便于打包剥离。
- **cargo-shear** 元数据防依赖漂移（平台特例白名单）。
- **Bazel 支持**（BUILD 文件 + runfiles）——OpenAI 内部构建需求，外部可忽略。
- **patch.crates-io 分叉管理**：crossterm/tungstenmine 等 fork 集中登记。

**对照 anureo**：CI 已有 clippy `-D warnings` 门禁，但无 workspace 级 lint 白名单与集中依赖管理。半天工作量即可搬，收益是消灭整类问题。

## 15. 其余块（定位不展开）

| 块 | 一句话职责 |
|---|---|
| `tui` | 终端 UI（ratatui）宿主 |
| `cli` | `codex` 主命令入口（arg0 多路分发：`arg0` crate） |
| `codex-mcp` / `mcp-server` | 把 codex 作为 MCP server 暴露 |
| `code-mode*`（4 crate）| "代码模式"：模型生成的代码在受控运行时执行（v8-poc、code-mode-runtime） |
| `realtime_conversation`（core 内）| 实时语音对话 |
| `cloud-tasks*`（3 crate）| 云端任务 |
| `analytics` | 遥测事实与事件（`TurnProfileFact`/`TurnTokenUsageFact`） |
| `otel` | OpenTelemetry 集成（turn 级 metric：E2E 时长/内存/token/工具调用数） |
| `guardian`（ext/）| 安全审查扩展（拒绝电路断路器，见 core `guardian_rejection_circuit_breaker`） |
| `agent-graph-store` / `agent-identity` | 多 agent 关系图与身份 |
| `memories/read`、`memories/write` | 记忆读写（`ext/memories` 注入） |
| `context-fragments` | 上下文片段类型（`ContextualUserFragment` 等，被 core 与扩展共用） |
| `external-agent-migration` | 从其他 agent（claude 等）迁移会话 |
| `utils/*` | 约 30 个单函数 crate（absolute-path、pty、fuzzy-match、template、stream-parser……） |

## 16. 值得借鉴与不建议照搬（汇总）

**值得借鉴（按 ROI 排序）**：

1. **workspace lints + 集中依赖**（§14）——半天，零风险；
2. **`_anureo.dev/*` typed protocol + TS 导出**（§7）——前端协作刚需；
3. **prompt 模板文件化**（§11）——与 goal 方案 P0 同 PR；
4. **运行时 contributor trait 化**（§5）——新功能先行，旧的渐进迁移；
5. **state crate 组织模式**（§9）——goal 存储迁移时落地；
6. **JsonSchema 构建器**（§11）——搬进 agent/tool；
7. **head-tail 输出缓冲**（§8）——shell 工具输出截断；
8. **test-support crate 化 + insta 快照**（§13）；
9. **feature Stage 化**（§3）——experimental 治理；
10. **TokenUsage 细分字段**（§10）——goal 记账前置。

**不建议照搬**：

| codex 做法 | 不抄理由 |
|---|---|
| utils/* 一函数一 crate、140 crate 碎片化 | anureo 26 crate 规模合适，拆碎徒增维护面 |
| v1/v2 协议并存 | 扩展面小，向前兼容 + capability 协商够用 |
| execpolicy Starlark 策略引擎 | 重量级，当前无此威胁模型 |
| `bespoke_event_handling.rs` 165KB 单文件 | 反面教材：规模失控的教训 |
| Bazel 构建、cloud-tasks、noise 加密通道、code-mode/v8 | 无对应需求 |

## 17. 各块与 anureo 的映射总表

| codex 块 | anureo 同位物 | 差距/动作 |
|---|---|---|
| protocol | agent-client-protocol（外部）+ foundation/stream-event | 扩展类型集中化 |
| core（Session/tasks/turn） | agent/agent-core | steering 队列、生命周期发射点 |
| core-api | ——（直接依赖 agent-core） | 引入 deny unreachable_pub 纪律即可 |
| ext/extension-api + ext/* | apps/acp/src/extensions/*（RPC 面） | 运行时 contributor 层是缺的 |
| app-server 家族 | apps/server + apps/acp | typed protocol + TS 导出 |
| exec 家族 | agent/tool（shell 工具） | head-tail buffer、策略层可选 |
| state / rollout | foundation/checkpoint-sqlite-store、experimental/task/task-core | goal 存储迁移（见 goal 方案 P4） |
| model 家族 | foundation/llm、foundation/model-spec-core | TokenUsage 细分 |
| prompts / tools | 各处硬编码 | 模板化 + JsonSchema builder |
| features | 无 | Stage 化注册表（可选） |
| tui / cli | apps/cli | —— |
| analytics / otel | tracing（已有） | turn 级 metric 可选 |

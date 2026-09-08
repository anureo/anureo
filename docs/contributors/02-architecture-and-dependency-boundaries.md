# anureo 架构与依赖边界

> **状态**：基于当前源码的贡献者说明
> **相关代码**：`Cargo.toml`、`agent/agent-core`、`agent/tool`、`foundation`、`apps`

本文面向 anureo 贡献者，说明当前 workspace 中各层的职责、运行时调用路径和可扩展位置。所有结论均以本文列出的源码为准；没有在这些源码中出现的 API、命令或配置行为不作为已实现能力描述。

### 阅读前提与术语速览

第一次参与 anureo 的 Rust 贡献者可以先读[`仓库地图`](./01-environment-and-repository-map.md)、[`Agent 执行模型`](./03-agent-execution-model.md)、[`Tool/MCP/Skill 开发`](./07-tool-mcp-and-skill-development.md)、[`配置与持久化`](./08-configuration-and-persistence.md)、[`测试、调试与可观测性`](./09-testing-debugging-and-observability.md)和[`端到端功能 walkthrough`](./10-end-to-end-feature-walkthrough.md)。本文假定读者知道 Rust workspace、trait、async/await 和单元测试；不假定知道 anureo 的运行术语：ReAct 是“模型思考/调用工具/继续”的循环，DUP/ToT/GoT 是三种 agent pattern，BSP 是 Pregel 使用的批量同步并行模型，ACP 是 IDE 等客户端与 Agent 通信的协议，LSP 是语言服务器协议，XDG 是用户配置/数据目录约定，SSE 是服务器推送事件，Pregel 是图 runtime，`stream mode` 是 graph 暴露的事件订阅类别。配置、ACP 和测试的背景分别见上述链接。

调用链中的源码入口：[`agent-core/src/agent/mod.rs`](../../agent/agent-core/src/agent/mod.rs) → [`agent-core/src/run/runner.rs`](../../agent/agent-core/src/run/runner.rs) → [`tool-core/src/registry.rs`](../../agent/tool/tool-core/src/registry.rs) / [`foundation/llm/src/lib.rs`](../../foundation/llm/src/lib.rs) → [`foundation/graph-core/src/lib.rs`](../../foundation/graph-core/src/lib.rs) / [`foundation/pregel/src/lib.rs`](../../foundation/pregel/src/lib.rs) → [`foundation/checkpoint/src/lib.rs`](../../foundation/checkpoint/src/lib.rs)。应用适配入口是 [`apps/cli/src/lib.rs`](../../apps/cli/src/lib.rs)、[`apps/acp/src/lib.rs`](../../apps/acp/src/lib.rs) 和 [`apps/server/src/lib.rs`](../../apps/server/src/lib.rs)。

## 1. 总体边界

根 `Cargo.toml` 是 Rust `2021` virtual workspace，`resolver = "2"`，workspace 版本为 `0.5.0`。成员按职责分为四层：

| 层 | 代表 crate | 当前职责 |
| --- | --- | --- |
| Foundation | `foundation/graph-core`、`foundation/pregel`、`foundation/llm`、`foundation/config`、`foundation/checkpoint` | 图模型与编译、Pregel-style 执行、LLM 抽象、配置加载、checkpoint/store 抽象 |
| Agent | `agent/agent-core` | ReAct、DUP、ToT、GoT agent pattern；配置构建；运行编排；stream 消费与恢复 |
| Tool | `agent/tool/tool-core`、`tool-basic`、`tool-workflow` | Tool trait、registry、过滤与调用；文件/命令/网络/skill/workflow 工具实现 |
| Apps | `apps/cli`、`apps/acp`、`apps/server` | CLI、ACP stdio JSON-RPC 适配、server 的 route/transport 适配 |

依赖方向应保持为：应用入口组合 Agent 和基础能力，Agent 使用 Foundation 与 Tool 抽象，具体工具实现依赖 `tool-core`；Foundation 不应反向依赖 CLI、ACP 或 HTTP handler。`agent-core/src/lib.rs` 的 re-export 是当前公共组合面：它导出 agent 类型、run API、runner common、checkpoint 相关运行辅助以及取消类型。

不要仅凭 workspace 成员名称推断未阅读的 crate API。本文只讨论指定源码中实际公开的模块、类型和函数。

## 2. Foundation：稳定的运行基础

### 2.1 图模型与 Pregel runtime

`foundation/graph-core/src/lib.rs` 导出 `StateGraph`、`CompiledStateGraph`、`GraphStream`、`Node`、`Channel`、`RunContext`、`RetryPolicy`、`GraphError`、`Interrupt` 和 `START`/`END` 等类型。它的边界是描述、编译和暴露状态图运行接口；agent 层通过 `CompiledStateGraph` 执行图，而不是在 CLI 中重新实现节点调度。

`foundation/pregel/src/lib.rs` 是低层 BSP runtime，公开 `PregelRuntime`、`PregelGraph`、`PregelNode`、`PregelConfig`、`PregelDurability`、`ExecutableTask`、`TaskOutcome`、channel/state 类型以及 resume 相关类型。贡献者修改调度、任务准备、channel 写入或 durability 时，应从 `pregel` 入手；修改用户可组合的图节点、条件路由、middleware 或图可视化时，应从 `graph-core` 入手。

当前源码没有在这两个 `lib.rs` 中声明 CLI 参数或 Agent 专用工具。不要把 Pregel 的内部 task 类型当成 CLI/API 的稳定协议。

### 2.2 LLM 抽象

`foundation/llm/src/lib.rs` 将消息、tool call、错误、provider、client 和模型 registry 分开公开：

- 消息侧包括 `Message`、`UserContent`、`AssistantPayload`、`ToolCallContent`。
- 工具描述侧包括 `ToolSpec`、`ToolCall`、`ToolSourceError`、`ToolOutputHint` 和 `ToolOutputStrategy`。
- provider/client 侧包括 `LlmClient`、`LlmProvider`、`LlmResponse`、stream chunk、usage、`ModelCapabilities`、`ProviderConfig` 和模型列表类型。
- 当前实现 re-export 了 `ChatOpenAI`、`ChatOpenAICompat`、`OpenAIProvider` 和 `OpenAICompatProvider`。

Agent 或工具代码需要模型调用时，应依赖这些 trait/type；不要在应用入口直接拼接 provider 请求。模型配置的解析属于 `foundation/config` 和 Agent 的 config builder，具体 HTTP client 属于 `foundation/llm`。

### 2.3 配置

`foundation/config/src/lib.rs` 的模块边界包含 `.env`、home、LSP、MCP、model、provider 和 XDG TOML。它公开 `load_and_apply`、`load_and_apply_with_report`、`ConfigLoadReport`、`ConfigSource`、`ProviderDef` 以及 MCP/LSP 配置读写接口。

`load_and_apply_with_report` 的优先级（高到低）是：

```text
已有进程环境 > 项目 .env > [default].provider 选中的 [[providers]] > config.toml 的 [env]
```

代码只在 key 尚未存在于进程环境时写入环境变量；provider 名称使用大小写不敏感匹配，找不到选中的 provider 时 provider map 为空。provider 没有 `OPENAI_BASE_URL` 时，源码会先看 `ANUREO_MODELS_DEV_API_JSON`，否则请求 `MODELS_DEV_URL`，缺省 URL 为 `https://models.dev/api.json`。这是当前实现的 fallback，不是本文建议新增的稳定 discovery API。

配置报告会通过 `is_secret_key` 和 `mask_value` 遮蔽 key/token/secret/password/credential/auth 等敏感值。日志和诊断应使用 `value_masked` 或 `summary()`，不得直接输出 API key。

### 2.4 Checkpoint 与 Store

`foundation/checkpoint/src/lib.rs` 明确区分两种持久化能力：

| 能力 | 抽象 | 用途 |
| --- | --- | --- |
| Checkpoint | `Checkpointer`、`Checkpoint`、`CheckpointTuple`、`RunnableConfig` | 单个 thread/run 的状态快照、resume、replay、branch；键包含 `thread_id`、可选 `checkpoint_ns` 和 `checkpoint_id` |
| Store | `Store`、`Item`、`Namespace`、`StoreOp` | 跨 session 的长期数据，如 preferences、facts 和 search index |

默认公开实现是 `MemorySaver` 和 `InMemoryStore`。使用 checkpointer 时，`RunnableConfig.thread_id` 是关键运行标识；不要把 Store 当成单次 graph checkpoint，也不要把 checkpoint 状态误当成应用入口的 session 表。

## 3. Agent 层：pattern、编排与恢复

### 3.1 公共模块面

`agent/agent-core/src/lib.rs` 公开：

- `Agent`、`AgentConfig`、`AgentEvent`、`AgentResult`、`ReactRunner` 和 React build/context 函数；
- `DupRunner`、`TotRunner`、`GotRunner` 及其 state/error 类型；
- `build_react_config`、`resolve_model_config`、`run_agent_from_config`、`RunOptions`、`RunCmd`、`RunCompletion`；
- `load_from_checkpoint_or_build`、`resume_from_checkpoint`、`run_stream_with_config`；
- profile/tier 解析和 `RunCancellation`。

`agent/mod.rs` 给出了新增 agent pattern 的源码级扩展路径：新增子模块，提供 state、runner 和 `build_*_initial_state`；在 React build 侧补 runner builder，按需复用 `build_react_run_context` 和 checkpointer builder；若由 CLI 使用，再在 `RunCmd`、runner builder 和 `run_agent` 分支中接入。这里的“若由 CLI 使用”很重要：pattern 本身属于 agent-core，CLI wiring 不应下沉到 foundation。

### 3.2 Run orchestration

`agent/run/mod.rs` 将运行编排拆成三个子模块：

```text
config_builder  -> build_react_config / load_memory_prompt / resolve_model_config
runner          -> build_runner / run_agent_from_config / run_agent_from_config_traced
types           -> RunOptions / RunCompletion / AgentRunResult / ExtraToolsProvider
```

`ExtraToolsProvider` 是当前可见的额外工具注入点。`tool-workflow` 的 `default_workflow_tool_provider()` 接收 `ReactBuildConfig`，构造共享的 `WorkflowRuntime`，再把 workflow tools 作为 `Arc<dyn Tool>` 返回给 Agent。新增一组与 run 配置相关的工具时，应优先评估是否能通过该 provider 注入，而不是在每个 app 入口重复注册。

### 3.3 Stream 与 checkpoint 调用流程

`runner_common.rs` 是多个 runner 共用的执行骨架：

```text
构造 initial state
    │
    ├─ 有 checkpointer + RunnableConfig.thread_id
    │      ├─ resume_from_checkpoint：恢复 channel_values，不追加 user message
    │      └─ load_from_checkpoint_or_build：恢复后调用 merge 追加新 user message
    │
    └─ 否则执行 build_fresh
    │
    ▼
CompiledStateGraph::stream(...)
    │
    ├─ 消费 StreamEvent
    ├─ 记录最后一个 StreamEvent::Values 作为 final state
    ├─ 可选调用 on_event
    └─ 等待 graph completion
         ├─ Finished(state)
         ├─ Cancelled（GraphError::Cancelled）
         └─ StreamRunError
```

`run_stream_with_config` 固定订阅 `Messages`、`Tasks`、`Tools`、`Updates`、`Values`、`Custom`、`Checkpoints` 七种 `StreamMode`。它会并发观察 completion，避免 leaked sender 让事件循环永久等待；completion 成功但没有 `Values` 时返回 `StreamEndedWithoutState`。因此新增 stream consumer 时不要假设“stream 结束就一定有 final state”，也不要只监听消息事件而忽略取消和 completion 错误。

## 4. Tool 层：注册、策略与具体实现

### 4.1 `tool-core`

[`agent/tool/tool-core/src/lib.rs`](../../agent/tool/tool-core/src/lib.rs) 公开 `Tool`、`ToolSpec`、`ToolCallContext`、`ToolRegistry`、`ToolRegistryLocked`、`BuiltinToolFilter`、`ToolSourceError` 以及 active operation 类型。

`registry.rs` 中的 `ToolRegistry` 保存 `HashMap<String, Box<dyn Tool>>`，并提供四个关键策略：

- `filter` 控制 `list()` 和普通调用是否允许该工具；
- `call_filter` 在实际调用前再次限制工具；
- `dry_run` 返回 `(dry run: <name> was not executed)`，不会执行工具；
- `yaml_specs` 可覆盖已注册工具的 `ToolSpec`，只影响列出的 spec，不替换实际 tool object。

`call()` 的顺序是检查 `filter`、检查 `call_filter`、检查 `dry_run`、按 name 查找 tool，最后调用 `Tool::call(args, ctx)`。禁用或不存在的工具都通过 `ToolSourceError::NotFound` 表达，但错误消息能区分 disabled、call_filter denied 与普通 not found。

`ToolRegistryLocked` 用 Tokio `RwLock` 包裹 registry，提供异步注册、异步 list/call 和同步注册。`register_sync` 会新建 current-thread Tokio runtime，并在独立线程中获取写锁；因此它不是可在任意 Tokio runtime 内直接 `block_on` 的替代品。注册工具时优先使用 `register_async`；只有同步 wiring 确实需要时才使用 `register_sync`。

通过 [`ToolRegistryLocked::call_tool`](../../agent/tool/tool-core/src/registry.rs) 调用时，传入 `Some(&ToolCallContext)` 才会写入 registry 级共享 context；后续不传 context 的 `call_tool` 会尝试复用最近一次 context。直接调用 `ToolRegistry::call` 只把调用方传入的 context 转交给 Tool，不会更新或读取这份共享状态。因此共享复用并非 per-call 隔离，可能造成并发或跨请求泄漏；调用方应显式传 context，或为不同请求隔离 registry。

### 4.2 `tool-basic`

[`tool-basic/src/lib.rs`](../../agent/tool/tool-basic/src/lib.rs) 注册并 re-export bash、batch、date、file、MCP、PowerShell、skill、todo 和 web 工具。`register_file_tools` 会先 canonicalize working folder，并在不是目录或路径不存在时返回 `ToolSourceError::InvalidInput`；随后把同一个 canonical `Arc<PathBuf>` 和 `allow_outside` 传给 ls/read/write/edit/multiedit/apply-patch/move/delete/create-dir/glob/grep 等文件工具。

该函数还根据传入的 skill registry/usage 注册 skill tools，并依据 `is_background_review` 选择 foreground 或 background-review 的 `SkillManagerTool`。不要在 file tool 内自行推断 working folder；working folder 的边界由注册函数统一确定。

`allow_outside` 默认必须为 `false`。它不是普通测试覆盖项：[`file/path.rs`](../../agent/tool/tool-basic/src/file/path.rs) 在 `true` 时跳过 working-folder containment check，绝对路径直接解析，因而读写、编辑、multiedit、apply-patch、移动、删除和创建目录都可能越出 working folder（同一开关也传给 glob/grep/ls）。只有明确、受审计且确实需要的场景才能开启；错误配置会把项目沙箱扩展为进程可访问的任意路径。任何修改都应验证 `false` 拒绝 `..`、绝对路径和指向 working folder 外部的 symlink escape；测试应使用临时目录和无敏感内容 fixture，不要用真实 secret。

### 4.3 `tool-workflow`（实验性边界）

workspace 中存在 `agent/tool/tool-workflow`，当前 `lib.rs` 公开 `WorkflowRuntime`、`resolve_workflow`、`WorkflowStartTool`、`WorkflowCancelTool`、`WorkflowStatusTool`、`WorkflowListTool`、`WorkflowEventsTool`、`WorkflowSourceTool`、`WorkflowFilesTool` 和 `register_workflow_tools`。注册函数为所有 workflow tools 共享一个 `Arc<WorkflowRuntime>`；provider 版本也为每次 `ReactBuildConfig` 构造一个 runtime，再返回 `Arc<dyn Tool>`。

源码测试验证了七个 tool name/spec 与 constants 一致，并验证 `instances_root()`、`runs_root()` 和 `workflows_dir()` 的路径布局。本文将 workflow 扩展视为实验性边界：虽然工具已经存在并有测试，但指定源码没有提供稳定性/兼容性承诺；新增 workflow 行为应同时补 runtime、tool contract 和事件/持久化测试。

## 5. 应用入口与调用流程

### 5.1 CLI

`apps/cli/src/lib.rs` 是 CLI library surface，公开 args、display、envelope、MCP/model/session/tool command 以及 run 模块。它 re-export `run_agent`、`run_cli_turn`、`RunCmd`、`RunOptions`、`RunOutput`、`RunStopReason`、stream output 和 model/tool listing helpers。

当前源码能确认的边界是：CLI 负责解析和展示，并调用 Agent 的 run orchestration；Agent pattern、tool registry 和 graph runtime 不应在 CLI 中复制实现。新增 CLI 参数时，在 `apps/cli` 的 args 与 run wiring 中完成，并确认它最终进入 `RunOptions`/`RunCmd` 或对应的配置路径；不要只修改展示层。

### 5.2 ACP

`apps/acp/src/lib.rs` 明确将 ACP 定义为 Agent-side adapter：`run_stdio_loop()` 使用 `agent_client_protocol::AgentSideConnection` 在 stdin/stdout 上处理 JSON-RPC，stderr 仅用于日志；`anureoAcpAgent` 实现 ACP `Agent`，`SessionStore` 维护 session 与 thread/cancel 状态，`content_blocks_to_message` 负责内容转换，`stream_bridge` 将 anureo stream event 转成 ACP `SessionUpdate`。

单次 prompt 的源码契约可概括为：

```text
initialize
  -> session/new -> SessionStore::create
  -> session/prompt
       -> content_blocks_to_message
       -> SessionStore::get
       -> run_agent_with_options / on_event
       -> stream_bridge -> session/update
  -> PromptResponse
```

工具需要用户确认时，ACP 层通过 `session/request_permission` 与客户端交互，再执行或拒绝工具。无效 session_id 和内容解析失败属于 JSON-RPC invalid_params；内部 run error 属于 server error。取消必须映射为 `PromptResponse(StopReason::Cancelled)`，不能报告为 Finished。

ACP 的协议适配应留在 `apps/acp`；不要为了支持 IDE 而修改 Agent core 的事件含义，也不要把 ACP `SessionUpdate` 当成 graph 的基础事件类型。

### 5.3 Server

`apps/server/src/lib.rs` 公开 `acp_hub`、`agent_runner`、`auth`、`handlers`、`location`、`pty`、`routes`、`sse`、`state`、`translator` 和 `v2_event`。该 `lib.rs` 只确定 server 的模块边界；具体 HTTP route、SSE payload、auth 行为和 event translation 必须继续阅读对应模块后才能修改，不能从模块名臆测 API。

因此，新增 HTTP/SSE 能力时应从 `routes`/`handlers`/`sse`/`translator` 追踪到 `agent_runner`，而不是在 CLI 或 ACP 中复制 handler。涉及 ACP hub、PTY 或 auth 时，分别在其 owning module 处理。

## 6. 扩展点选择

| 需求 | 首选位置 | 不应放置的位置 |
| --- | --- | --- |
| 新 agent pattern | `agent/agent-core/src/agent/<pattern>`，并接入 `agent/mod.rs` 的 builder 路径 | CLI、HTTP handler、foundation |
| 新 run 配置或 runner wiring | `agent/agent-core/src/run` | 各 app 入口分别复制 |
| 新 graph node/channel/middleware | `foundation/graph-core` | Agent pattern 内部私有重实现 |
| 调度、task、durability、resume | `foundation/pregel` | CLI display |
| 新 provider/client 抽象 | `foundation/llm` | ACP/CLI handler |
| 配置来源、provider env、MCP/LSP 配置 | `foundation/config` | 直接在 app 中 set_var |
| 新 Tool trait/registry 策略 | `agent/tool/tool-core` | 每个具体工具自行实现过滤 |
| 文件、命令、web、skill 工具 | `agent/tool/tool-basic` | Agent core 中硬编码 |
| workflow tool | `agent/tool/tool-workflow`（实验性） | Foundation 公共层 |
| CLI 参数、输出、subcommand | `apps/cli` | foundation/agent core |
| ACP request/notification/permission | `apps/acp` | graph event 类型 |
| HTTP/SSE route 或 translator | `apps/server` | CLI/ACP 入口 |

## 7. 测试与验证

修改前先从 workspace 根目录运行：

```powershell
cargo check --workspace
cargo test --workspace
```

按改动层补充验证：

| 改动 | 应覆盖的验证 |
| --- | --- |
| graph/Pregel | graph compile、节点/条件路由、stream completion、取消、retry 或 Pregel task 状态 |
| Agent runner | fresh build、checkpoint 命中/未命中、resume 不重复追加 user message、最终 `Values` 缺失、取消与错误映射 |
| config | 优先级、unknown provider、provider fallback、`ANUREO_MODELS_DEV_API_JSON`、secret masking；测试需恢复进程环境变量并使用源码已有的环境锁模式 |
| tool-core | filter、call_filter、dry_run、yaml spec override、显式/复用 `ToolCallContext`、missing tool |
| tool-basic | working folder canonicalize、非目录错误；默认 `allow_outside = false`，拒绝 `..`、绝对路径和 symlink escape；另验证 `true` 仅在受审计场景放开，并覆盖读写、编辑、apply-patch、移动、删除、创建目录的共同开关 |
| workflow | 七个 tool name/spec、runtime 路径、start/cancel/status/events/files 的行为和持久化 |
| CLI | args 到 `RunOptions`/run wiring、输出与 stop reason |
| ACP | initialize、session/new、prompt、stream update、permission、unknown session、cancelled stop reason |
| server | 只在阅读具体 route/handler 后运行对应 integration tests；`lib.rs` 本身不定义 route 契约 |

测试配置时要特别小心环境变量是进程全局状态；`foundation/config/src/lib.rs` 的测试通过 `CONFIG_TEST_LOCK` 串行化并恢复变量。不要把真实 secret 写入 fixture 或日志。涉及外部 models.dev 请求时，优先使用源码支持的 `ANUREO_MODELS_DEV_API_JSON` 进行确定性测试。

## 8. 常见坑

- 把 `resume_from_checkpoint` 与 `load_from_checkpoint_or_build` 混用：前者明确不追加 user message，后者通过 `merge` 追加新消息。
- 只监听 event stream，不等待 graph completion：completion 可能报告取消、执行错误、join failure，且没有最终 `Values` 时必须报错。
- 以为 `ToolRegistry::list` 和 `call` 只受一个 filter 控制：实际还有独立的 `call_filter` 与 `dry_run`。
- 以为 YAML spec 会替换工具实现：它只覆盖列出的 `ToolSpec`；真实调用仍来自 registry 中注册的 tool。
- 在 Tokio runtime 中随意调用 `register_sync`：该函数自身创建独立线程/runtime并 join，优先使用异步注册。
- 误把 registry 的最近一次 context 当成 per-call 隔离：无显式 context 的调用可能复用共享 context。
- 让具体文件工具自行接受任意路径：`register_file_tools` 会 canonicalize working folder，并集中传递 `allow_outside`。
- 把 ACP transport 和 anureo graph 混为一层：ACP 负责 JSON-RPC、session、permission 和 stream translation，Agent core 才负责运行逻辑。
- 从 `apps/server/src/lib.rs` 的模块名推测 HTTP API：route、SSE、auth 和 translator 的行为不在该文件中。
- 把 workflow tools 当成稳定公共 API：当前实现存在且有单元测试，但本文将其标记为实验性；任何兼容性承诺都必须有对应源码和测试证据。
- 直接打印配置值：`ConfigLoadReport` 的设计就是 masked display；新增日志必须沿用 masked value。

## 9. 最小贡献流程

1. 从根 `Cargo.toml` 和目标 crate 的 `lib.rs` 确认 owning layer 与公开边界。
2. 沿调用方向追踪：app adapter → agent run → tool/LLM → graph/Pregel → checkpoint；不要跨层复制实现。
3. 先补该层的 unit/integration test，再接入 CLI、ACP 或 server wiring。
4. 对配置、checkpoint、取消和 stream completion 等跨层行为写出明确的状态/错误断言。
5. 运行 `cargo check --workspace` 与 `cargo test --workspace`，并按入口补充针对性测试。
6. 检查文档中的命令、路径、配置项和行为是否仍与源码一致；未实现或实验性内容必须明确标注。

### 9.1 可复制的安全小修改：为 `dry_run` 增加回归测试

下面的闭环只触及 `tool-core`，不启动 Agent、不访问网络、不需要 API key，也不应使用真实 secret。它适合第一次贡献：

1. 修改 [`agent/tool/tool-core/src/registry.rs`](../../agent/tool/tool-core/src/registry.rs)，在现有 registry 测试模块（若当前源码没有该模块，就在文件末尾新增 `#[cfg(test)] mod tests`）增加一个最小 `Tool` double 和 `#[tokio::test] async fn registry_dry_run_does_not_execute_tool()`。注册名为 `probe` 的 tool，设置 `registry.set_dry_run(true)`，调用 `registry.call("probe", serde_json::json!({}), None)`，断言结果文本为 `(dry run: probe was not executed)`，并断言 double 的执行计数仍为 0。
2. 最小 patch 只应覆盖上述测试和必要的 mock；不要修改 dry-run 生产分支、文件工具注册或 CLI wiring。若要测试过滤而非 dry-run，可用同样的 double 设置 `BuiltinToolFilter`，断言 `list()` 不含被过滤工具且 `call()` 返回 `ToolSourceError::NotFound`。
3. 在 workspace 根目录执行：

   ```powershell
   cargo fmt --check
   cargo test -p tool-core registry_dry_run_does_not_execute_tool
   cargo check --workspace
   git diff --check
   git diff -- agent/tool/tool-core/src/registry.rs
   ```

   预期 `cargo fmt --check`、针对性测试、workspace check 和 diff check 均成功；针对性测试应显示 1 个通过。若只改测试，`cargo check --workspace` 是必要的边界确认，失败时先修复编译问题再扩大修改范围。
4. 最后查看 `git diff` 和 `git status --short`，确认只包含目标测试；不要把 token、API key、真实路径或真实项目数据写入 fixture、断言、日志或提交。文件工具安全测试另需遵守上面的 `allow_outside = false`、`..`、绝对路径和 symlink escape 要求，并参照[`测试、调试与可观测性`](./09-testing-debugging-and-observability.md)的 package 级命令。

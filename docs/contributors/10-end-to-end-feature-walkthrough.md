# anureo 端到端功能实现 walkthrough

> **状态**：基于当前源码的贡献者说明
> **相关代码**：`agent/tool`、`agent/agent-core`、`apps/cli`、`apps/acp`、`foundation/config`、`foundation/checkpoint`、`agent/tool/tool-workflow`
> **目标读者**：anureo 贡献者

本文用一条端到端路径说明：一个能力如何从 Tool 或 Graph 实现进入 Agent，再由 CLI、ACP 或 Workflow 宿主运行，并如何测试和持久化结果。文中的 API、路径、命令和行为均以当前仓库源码为准；没有在所列源码中出现的 wiring 不在本文承诺范围内。

## 1. 先建立边界：功能由哪一层拥有

| 层 | 当前源码中的职责 | 不应放入的职责 |
| --- | --- | --- |
| `tool-core` | 定义 `Tool`、`ToolSpec`、`ToolCallContext`、`ToolCallContent`、`ToolRegistry` | 文件系统、MCP transport、CLI 参数解析 |
| `tool-basic` | 文件、Skill、Bash、MCP 等具体 Tool，以及注册组合入口 | Agent runner 的生命周期 |
| `agent-core` | 根据 `ReactBuildConfig` 构造 runner，执行 `React`、`Dup`、`Tot`、`Got` 并转发 stream event | 应用侧 worktree、debug 或 curator 副作用 |
| `apps/cli` | 构造 CLI 使用的 config/context，并展示 Tool spec | 重新实现 Tool 调用 |
| `apps/acp` | 按 ACP client capabilities 创建客户端文件工具，把 ACP MCP 描述转换成 anureo 模型 | 假设客户端支持未声明的能力 |
| `tool-workflow` | 把 Workflow backend 接到 `Agent`，注入 schema tool、allowlist 和取消处理 | 把结构化输出协议塞进通用 registry |
| `foundation/config` | 读取环境变量、`config.toml`、`.env` 和 MCP 配置模型 | 自动建立完整 Agent session |
| `foundation/checkpoint` | 定义 checkpoint 之外的 durable `Store` 数据模型和搜索接口 | 把长期 memory 当作一次运行快照 |

贡献新功能时先确定 owning crate，再沿着“构造 → 注册/注入 → 调用 → 输出/事件 → 测试”追踪；不要从最终 CLI 名称反推底层 API。

## 2. 最小 Tool：从实现到 registry

### 2.1 `Tool` 的最小契约

`agent/tool/tool-core/src/tool.rs` 中的 `Tool` trait 要求：

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn spec(&self) -> ToolSpec;
    async fn call(
        &self,
        args: serde_json::Value,
        ctx: Option<&ToolCallContext>,
    ) -> Result<ToolCallContent, ToolSourceError>;
    fn builtin_skill(&self) -> Option<BuiltinSkill>;
}
```

`builtin_skill` 默认返回 `None`。实现了 `Some(BuiltinSkill)` 的 Tool 可以用 `include_str!` 把完整 `SKILL.md` 和 references 编译进 binary；它仍然是 guidance/discovery，不是另一套调用协议。`ToolSpec.name` 应与 `name()` 相同，schema 只描述输入，`call` 仍需自己检查字段和类型。

`agent/tool/tool-basic/src/file/read_file.rs` 是一个完整的 local Tool 例子。它要求 `path`，用 `resolve_path` 解析路径，用 `std::fs::read_to_string` 读取 UTF-8；`offset` 是 0-based line index，`limit` 默认 2000，超过 2000 的行按字符边界截断，输出带 1-based 行号的 `cat -n` 风格文本。schema 中虽然有 `encoding`，当前实现并没有按该字段选择任意编码。

### 2.2 Registry 的真实调用流程

`agent/tool/tool-core/src/registry.rs` 的 `ToolRegistry` 以 `HashMap<String, Box<dyn Tool>>` 保存实例；重复注册同名 Tool 会覆盖旧值。`ToolRegistryLocked` 用 `Arc<tokio::sync::RwLock<ToolRegistry>>` 包装它，并暴露异步注册、列举和调用：

```text
register_async/register_sync
        ↓
HashMap[name] = Tool
        ↓
list_tools → tool.spec → filter → optional YAML override
        ↓
call_tool(name, args, ctx)
  → list filter
  → call_filter
  → dry_run（若启用则不执行）
  → Tool::call(args, effective_ctx)
```

`filter` 影响列表和调用，`call_filter` 只额外限制调用，`dry_run` 返回 `(dry run: <name> was not executed)`。显式传入的 `ToolCallContext` 会被保存；后续不传 context 的调用可能复用最近一次 context，因此它不是 per-call 隔离机制。

异步代码应使用 `register_async`。`register_sync` 会另起线程创建 current-thread Tokio runtime 后 join；它是同步边界的适配方式，不应在已有 Tokio runtime 的路径中作为首选。

## 3. 从工作目录注册一组能力

`agent/tool/tool-basic/src/lib.rs` 的 `register_file_tools` 是文件能力的组合入口，而不是单一文件 Tool。它先 canonicalize `working_folder` 并确认是目录，然后把同一个 `Arc<PathBuf>` 和 `allow_outside` 传给 `LsTool`、`ReadFileTool`、`WriteFileTool`、`EditFileTool`、`MultieditTool`、`ApplyPatchTool`、`MoveFileTool`、`DeleteFileTool`、`CreateDirTool`、`GlobTool`、`GrepTool`，以及 Todo/Date tools。

该函数还按参数选择 Skill discovery：有 `SkillRegistry` 时使用 registry；否则可以从 working folder 构造；最后总会创建 `.anureo/skills` 下的 `SkillStorageRegistry` 和 `SkillUsageStore`，并根据 `is_background_review` 选择 `SkillManagerTool::for_background_review` 或 `for_foreground`。

文件路径安全是工具族的共同约束：默认应把 canonical path 限制在 working folder；`allow_outside = true` 会绕过 containment check，是高风险开关。新增文件 Tool 应复用现有路径解析逻辑，不要自己拼接相对路径。

对应的集成测试 `agent/tool/tool-basic/tests/register_file_tools_origin.rs` 验证三件事：background review 创建的 Skill 会写入 agent-created provenance，foreground 创建的 Skill 不会自动标记，且 `skill_manage` 会注册到 registry。这个测试体现了一个重要边界：注册参数不仅影响“有哪些 Tool”，也影响 Skill 写入生命周期。

## 4. Agent 运行：从 config 到 stream event

`agent/agent-core/src/run/runner.rs` 明确把 config building 和 execution 分开：调用方应先构造 `ReactBuildConfig`，再调用 `run_agent_from_config`。`RunCmd` 当前有 `React`、`Dup`、`Tot`、`Got { got_adaptive }` 四种选择；`build_runner` 按选择调用对应的 `build_*_runner`，并注入 cancellation、verbose、LLM override 和 event sender。

`run_agent_from_config` 的主要行为是：

1. clone config；`Got` 模式根据 `got_adaptive` 覆盖 adaptive 设置。
2. 构造 runner。
3. 调用对应 runner 的 `stream_with_config`。
4. 把不同 state 的 stream event 包装成 `TypedAnyStreamEvent`，可转换成 format A 或 protocol envelope。
5. 正常完成时从 state 提取 assistant reply；取消时返回 `RunCompletion::Cancelled`。

`run_agent_from_config_traced` 只是在外层建立 `agent_run` tracing span；thread id 缺失时生成 uuid6。源码注释也说明，旧的、把 config building 和 app-side worktree/debug/curator side effects 混在一起的 `run_agent` convenience wrapper 已移除。应用层应该明确安排这些副作用。

`agent/agent-core/src/tools/mod.rs` 的职责很窄：公开 `AgentTool`、`AgentCancelTool`、`AgentGetTool`、`GitWorktreeTool`、`ThreadGetTool` 和 `AsyncAgentRegistry`。新增 Agent-facing Tool 应从这里确认 re-export 是否必要，不要把 runner 逻辑放进该模块。

## 5. CLI 与 ACP 是两条宿主路径

### 5.1 CLI Tool inspection

`apps/cli/src/tool_cmd.rs` 的 `list_tools` 和 `show_tool` 都调用 `build_react_config`，再调用 `build_react_run_context`，最后从 `ctx.tool_source.list_tools()` 获取 `Vec<ToolSpec>`。这意味着 CLI 展示的是实际构造出来的 Agent tool source，而不是单独维护的一份静态列表；workflow tool 及其 builtin skill 由 `RunOptions::default_extra_tools_provider` 进入该构造流程。

`list_tools` 支持表格或 JSON 输出；空列表输出专门的空结果提示。`show_tool` 找不到名称会返回 `RunError::ToolNotFound`，支持 JSON 和 YAML 字符串格式。贡献者修改 Tool spec 时，应同时检查这里的序列化字段和 `apps/cli/src/tool_cmd.rs` 中的格式化测试。

本文件不把未在计划源码中出现的 clap wiring 约定为命令名；确认用户可执行的 CLI 子命令时，应继续从当前 CLI manifest/命令注册处追踪，而不是根据函数名臆造命令。

### 5.2 ACP client filesystem tools

`apps/acp/src/tools/mod.rs` 的 `create_acp_tools` 根据 `ClientCapabilitiesInfo` 创建 ACP client tools：只有 client 声明对应能力时，才加入 `ReadTextFileTool` 或 `WriteTextFileTool`。这类 Tool 通过 ACP client bridge 执行，不等同于 `tool-basic` 中以 working folder 为根的 local file tools。

`apps/acp/tests/test_fs_tools_integration.rs` 给出了可观察契约：写新文件时返回 `ToolCallContent::Diff`，`old_text` 为 `None`；更新已有文件时包含旧文本和新文本；读文件返回 `Text`；不存在的文件返回错误；write → read 可以 round-trip。修改 ACP file tool 时应保留这些结果类型和调用顺序的测试。

`apps/acp/src/mcp_convert.rs` 的 `acp_mcp_to_anureo` 只负责把 ACP 的 stdio/http/sse server 描述转换成 `foundation::config::McpServerDef`；SSE 映射为 HTTP，注释说明由 rmcp 的 `StreamableHttpClientTransport` 处理。转换函数不会因此自动创建 MCP session 或注册 Tool。

## 6. MCP 与配置：模型转换不等于运行时接入

`foundation/config/src/lib.rs` re-export MCP 配置读写函数和 `McpServerDef`。普通配置加载的优先级是 existing process env > project `.env` > active provider > `config.toml [env]`；secret-like key 在 report 中会 mask。配置层只负责解析和应用配置，不能把它描述成已建立 Agent/MCP session。

ACP 转换后的 `McpServerDef` 仍需由应用或 Tool 层选择 transport、建立 session、列出工具并注册 adapter。当前代码证据支持的边界是：转换保留顺序和 stdio 参数/env，HTTP/SSE 进入 HTTP 定义；没有证据表明转换函数本身负责连接、认证或 retry。

贡献 MCP 功能时应分别测试：配置字段解析、ACP 转换、session/transport 建立、`tools/list` spec 映射、`tools/call` 结果归一化和同名 collision。尤其不要因为配置模型出现某个字段，就宣称完整运行时能力已经可用。

## 7. Workflow：Tool 触发后台 Lua 运行，Backend 连接 Agent

`agent/tool/tool-workflow/src/tool_start.rs` 中的 `WorkflowStartTool` 实现了 `Tool`。它的 schema 当前要求三种互斥入口之一：`script`、`workflow`、`resume_from_id`；新运行可带 `args`（脚本内的 `_G._args`）和 `concurrency`（1..=64，默认 4）。`call` 委托给 `crate::service::start_workflow`，Tool 描述明确说明它立即返回，之后用 `workflow_status` 等工具查看进度。

这个 Tool 还返回名为 `workflow` 的 `BuiltinSkill`，内容和 references 都由 `include_str!` 编译进 binary，并声明依赖 `workflow_start`、`workflow_status`。这是 local Tool + builtin guidance 的组合例子：Tool schema 负责机器可调用契约，Skill references 负责较长的 Lua DSL、架构和验证说明。

`agent/tool/tool-workflow/src/backend.rs` 的 `anureoAgentBackend` 则是另一条边界：它实现 `luft_core::contract::backend::AgentBackend`，把 `AgentTask` 转成 anureo `Agent::from_config` 和 `agent.run`。运行前它可以：

- 用 `thread_id` 设置 `resume_mode`；
- 用 `model` 覆盖 model；
- 用 `workdir_override` 覆盖工作目录；
- 注入 `WorkflowValidateSchemaTool`，把结构化结果写入 output slot；
- 把 allow/deny 转成 `BuiltinToolFilter`；
- 把 anureo event 映射为 Luft `AgentProgress`，同时累计 token usage。

最终输出有明确的优先级：schema tool 写入 slot 时优先返回 slot；没有 slot 时把 agent reply 包成 `{ "_agent_fallback_text": true, "text": ... }`；即使 Agent 出错但 slot 已有结构化结果，也会 salvage slot；两者都没有才返回 `BackendError::Execution`。文件中的四个 unit tests 正是这四种组合的证据。

`agent/tool/tool-workflow/tests/instance_smoke.rs` 是可选 fixture smoke test：设置 `ANUREO_TEST_INSTANCES_DIR` 后，读取 `checkpoint.json` 和 `events.jsonl`，调用 `build_instance_meta`/`write_instance_artifacts`，检查 `instance.json` 的 `schema_version`、completed status、agent 列表、64 字符 checkpoint hash 和 event stats。未设置环境变量时该测试会打印提示并返回，不是失败。

## 8. Checkpoint 与长期 memory 的区别

`foundation/checkpoint/src/store.rs` 明确区分：Checkpointer 保存一次运行的 execution snapshot；`Store` 保存可跨运行存在的 durable key-value memory。`Store` 的接口包括 `put`、`get`、`get_item`、`delete`、`list`、`search`、`list_namespaces` 和 `batch`；数据通过 `Namespace = Vec<String>` 隔离，`SearchOptions` 支持 query、limit、offset，namespace 还可以用 prefix/suffix match condition。

仓库中的 examples 展示了不同层级：

- `anureo-examples/examples/echo.rs`：直接实现 `anureo_graph_core::Agent`，只处理 `EchoState`。
- `state_graph_echo.rs`：用 `StateGraph`、`START`、`END` 和 `AgentNode` 编排同一个 echo node。
- `memory_checkpoint.rs`：用 `MemorySaver`、`RunnableConfig { thread_id: ... }` 和 `compile_with_checkpointer` 保存一次运行的最终 state。
- `memory_persistence.rs`：用 `SqliteSaver` 和 `ANUREO_CHECKPOINT_DB` 将 checkpoint 持久化到 SQLite，进程重启后可继续使用同一数据库。
- `openai_embedding.rs`：用 `OpenAIEmbedder::new("text-embedding-3-small")` 创建 embedding，再将 LanceStore 的 namespace memory 写入并按语义搜索。
- `react_memory.rs`：把短期消息、Tool result、`Store` 和 SQLite checkpointer 组合到一个自定义 ReAct graph；其中 embedding 使用 example 内的 mock embedder，不能据此承诺生产 embedding 配置。

因此，新增 memory 能力时先决定数据是否是“运行快照”还是“跨运行事实”。前者接 Checkpointer/`RunnableConfig.thread_id`，后者实现 `Store` 或使用已有 Store backend；不要用一个接口同时承担两种恢复语义。

## 9. 扩展点：按类型选择改动位置

### 新增 local Tool

1. 在拥有副作用的 crate 实现 `Tool`，提供稳定的 `name`、完整 `ToolSpec`、参数校验、错误映射和必要的 `ToolCallContext` 使用。
2. 在 owning crate 的注册函数接入 `ToolRegistryLocked`，异步路径使用 `register_async`。
3. 若需要较长 prompt guidance，返回 `BuiltinSkill`；声明 `requires_tools` 时只使用实际注册的 Tool name。
4. 为成功、输入错误、底层错误、取消/超时和输出类型补测试。

### 扩展 ACP 文件能力

先在 `create_acp_tools` 增加 capability gate，再确保实现只调用 ACP client bridge，并保持 `Text`/`Diff`/error 结果契约。不要把 client capability 当成 local filesystem 权限；两者的安全边界不同。

### 扩展 Workflow

Lua 入口参数和后台生命周期改 `WorkflowStartTool`/service；Agent 的 model、workdir、allowlist、schema output 和取消改 `anureoAgentBackend`。如果新增结果形态，先更新 `finalize_output` 的优先级测试，再更新实例 artifact 和事件验证。

### 扩展 memory/checkpoint

只需运行内恢复时使用 Checkpointer；需要跨运行查询时实现/复用 `Store`。如果引入 embedding，明确 embedder、dimension、backend 和失败行为；`openai_embedding.rs` 中的 model 是示例代码中的 `text-embedding-3-small`，不是全局默认配置。

## 10. 测试方式与推荐顺序

先运行最靠近改动的测试，再扩大范围：

```powershell
cargo test -p tool-basic
cargo test -p tool-basic --test register_file_tools_origin
cargo test -p tool-workflow
cargo test -p tool-workflow --test instance_smoke -- --nocapture
cargo test -p config
cargo check --workspace
```

运行 `instance_smoke` 时，如需真正读取 fixture，先设置当前 shell 的 `ANUREO_TEST_INSTANCES_DIR`；没有 fixture 时测试按源码设计跳过。示例可以按各文件注释中的命令运行，例如：

```powershell
cargo run -p anureo-examples --example echo
cargo run -p anureo-examples --example state_graph_echo
cargo run -p anureo-examples --example memory_checkpoint -- "hello"
cargo run -p anureo-examples --example memory_persistence -- "hello"
cargo run -p anureo-examples --example openai_embedding
```

涉及 ACP file tool 时重点运行 `apps/acp/tests/test_fs_tools_integration.rs` 对应的 integration tests；涉及 CLI spec 展示时检查 `apps/cli/src/tool_cmd.rs` 中的 formatter tests；涉及 MCP CLI 配置时运行 `apps/cli/tests/mcp_cli_test.rs`，但注意该测试注释说明其 manager 使用固定路径，不能把临时文件本身当成真实配置发现路径。

## 11. 常见坑与实验性/未完成面

- `ToolRegistry` 的同名注册是覆盖，不是多值集合；新增 MCP server namespace 前必须设计命名和 collision 测试。
- `list` filter、`call_filter` 和 `dry_run` 是三个独立控制面；只测列表可见性不足以证明调用被禁止。
- 不要把 registry 最近一次 `ToolCallContext` 当作 request isolation；无 context 调用可能复用共享值。
- `register_sync` 会创建线程和 Tokio runtime；异步调用方应使用 `register_async`。
- `ReadFileTool` 的 `encoding` 只在 schema 中出现；当前实现仍是 UTF-8 `read_to_string`。
- `allow_outside` 会取消文件 containment check，不能当作普通便利参数。
- ACP 的 `ReadTextFileTool`/`WriteTextFileTool` 受 client capability 控制，与 `tool-basic` local file tools 不是同一个实现。
- `acp_mcp_to_anureo` 只做数据转换；MCP 配置或 ACP server 定义出现，不代表 session、认证、工具注册或 reconnect 已完成。
- Workflow start 是后台启动语义；返回 `instance_dir` 后还要通过 status/events/source 查看结果，不应假设 `call` 已等待完成。
- Workflow structured output 可能被 fallback text 包装；只有 schema tool 成功写入 output slot 时才有结构化 slot 优先级。
- `MemorySaver` 是进程内示例；`SqliteSaver` 才是 `memory_persistence.rs` 展示的跨进程持久化路径。
- `Store` 与 Checkpointer 语义不同；不要把 namespace memory 的查询结果写成 checkpoint resume state。
- `react_memory.rs` 的 mock embedder 和示例中的 OpenAI embedding 不代表统一的默认 embedding backend。
- `TOOL_YAML_FILES` 当前为空时，YAML spec 不能创建 Tool，也不能替代 `Tool::spec`；只有已注册同名 Tool 才可能被覆盖描述。

## 12. 最小端到端核对表

提交前按以下顺序核对：

1. owning crate 和公开 re-export 是否正确。
2. `Tool::name`、`ToolSpec.name`、registry key 和 model-facing tool name 是否一致。
3. 构造路径是否真的把 Tool 注入 Agent/CLI/ACP/Workflow，而不是只有孤立实现。
4. 输入校验、working folder/capability/allowlist 等边界是否在执行前生效。
5. 输出是 `Text`、`Diff`、结构化 JSON、fallback envelope 还是 stream event，是否有对应测试。
6. 取消、错误、恢复、checkpoint/store 语义是否分别验证。
7. 文档中的路径、命令、配置项和“实验性”标签是否仍与当前源码一致。

# anureo Agent 执行模型

> **状态**：基于当前源码的贡献者说明
> **相关代码**：[`agent/agent-core/src/lib.rs`](../../agent/agent-core/src/lib.rs)、[`agent/agent-core/src/run`](../../agent/agent-core/src/run)、[`agent/agent-core/src/agent`](../../agent/agent-core/src/agent)

本文所有源码路径均以 anureo 仓库根为基准；例如 `agent/agent-core/src/state.rs` 可直接从仓库根打开。链接目标使用本文所在目录 `docs/contributors/` 回到仓库根的相对链接。

本文面向 anureo 贡献者，说明一次 Agent run 如何从配置进入 graph、如何在 graph 中执行 LLM 与 Tool、如何通过 checkpoint/stream/cancellation 完成运行，以及 ReAct、DUP、ToT、GoT 的边界。结论只依据本文列出的当前源码；未在源码中出现的 API、命令或行为不视为已实现能力。

### 继续阅读

- [环境与仓库地图](./01-environment-and-repository-map.md)：先确认 workspace、crate、环境变量和测试隔离边界。
- [CLI 贡献路径](./04-cli-contribution-path.md)：需要把 agent-core 能力接到 CLI 时阅读。
- [Tool、MCP 与 Skill 开发](./07-tool-mcp-and-skill-development.md)：新增工具、MCP server 或 skill 时阅读。
- [测试、调试与可观测性](./09-testing-debugging-and-observability.md)：选择测试层、处理 stream/日志/进程问题时阅读。
- [贡献与 review 流程](./11-contribution-and-review-process.md)：提交改动前阅读。

## 1. 公共执行面与模块边界

[`agent/agent-core/src/lib.rs`](../../agent/agent-core/src/lib.rs) 是组合面：re-export `Agent`、`AgentConfig`、`ReactRunner`、`ReactBuildConfig`、`build_react_config`、`run_agent_from_config`、`RunOptions`、`RunCmd`、`RunCompletion`、`RunnerError`、四种 pattern runner 和 `RunCancellation`。它把 app 入口与 graph/pattern 实现连接起来，但不承载 CLI、ACP 或 HTTP handler 的副作用。

| 模块 | 当前职责 | 贡献者应在此处修改的内容 |
| --- | --- | --- |
| [`agent/agent-core/src/state.rs`](../../agent/agent-core/src/state.rs) | `ReActState`、`ModelConfig`、`ToolResult` 及 usage/summary/compaction 标记 | ReAct 状态字段、序列化默认值、状态转换 |
| [`agent/agent-core/src/run/config_builder.rs`](../../agent/agent-core/src/run/config_builder.rs) | `RunOptions` → `ReactBuildConfig`，profile/model/provider/skills/prompt 合并 | 配置来源、prompt assembly、额外工具 provider |
| [`agent/agent-core/src/run/runner.rs`](../../agent/agent-core/src/run/runner.rs) | 解析 `RunCmd`、构造 `AnyRunner`、消费 runner 结果、统一 typed event | pattern dispatch、run-level callback 和 completion 映射 |
| [`agent/agent-core/src/agent/react`](../../agent/agent-core/src/agent/react) | Think/Act/Observe/Compress 图、tool registry、checkpoint 和 stream | 默认 Agent loop 或 ReAct 扩展 |
| [`agent/agent-core/src/agent/dup`](../../agent/agent-core/src/agent/dup)、[`tot`](../../agent/agent-core/src/agent/tot)、[`got`](../../agent/agent-core/src/agent/got) | 各自的状态包装、graph runner 和 pattern-specific 节点 | 对应 pattern 的策略，不要塞进 ReAct 节点 |
| [`agent/agent-core/src/compress/compaction.rs`](../../agent/agent-core/src/compress/compaction.rs) | tool result pruning 与 LLM summary compaction | 上下文压缩策略与其测试 |
| [`agent/agent-core/src/runner_common.rs`](../../agent/agent-core/src/runner_common.rs) | 统一 stream 消费、final `Values`、completion、取消 | 所有 runner 共用的生命周期行为 |
| [`agent/agent-core/src/runner_error.rs`](../../agent/agent-core/src/runner_error.rs) | compilation、checkpoint、graph execution、provider 和无 final state 的错误归类 | 跨层错误映射 |
| [`agent/agent-core/src/stats/event.rs`](../../agent/agent-core/src/stats/event.rs) | 当前只有“Phase 2 will expand”的内部事件占位 | 不要把它描述成已实现的统计协议 |

[`agent/agent-core/src/run/runner.rs`](../../agent/agent-core/src/run/runner.rs) 的注释明确说明：旧的、同时处理 worktree/debug_llm/curator 等 app-side side effects 的 `run_agent` wrapper 已移除。当前调用者应先 `build_react_config`，再调用 `run_agent_from_config`；这些 app 行为不属于 `agent` crate 的 runner 契约。

## 2. 从 `RunOptions` 到 graph 的调用流程

```text
RunOptions
  │
  ├─ build_react_config
  │    ├─ load_profile_from_options
  │    ├─ 合并 profile 到 effective options
  │    ├─ ReactBuildConfig::from_env
  │    ├─ 解析 model/provider/base_url/api_key/tier
  │    ├─ 发现 skills，注入 builtin skills，应用 filters
  │    ├─ 组装 role / AGENTS.md / skills / memory / EnvContext prompt
  │    └─ 返回 (ReactBuildConfig, ResolvedAgent, SkillRegistry)
  │
  └─ run_agent_from_config(config, RunCmd, RunParams, on_event)
       ├─ GoT 时复制并设置 got_config.adaptive
       ├─ 将 on_event 适配为 TypedAnyStreamEvent sender（若调用者未提供）
       ├─ build_runner → resolve_tier_and_build_config
       ├─ 注入可选 llm_override 为 FixedLlmProvider
       └─ AnyRunner::{React,Dup,Tot,Got}.stream_with_config
            └─ runner_common::run_stream_with_config
```

`RunCmd` 当前只有 `React`、`Dup`、`Tot` 和 `Got { got_adaptive: bool }`。`RunParams` 携带 user `message`、`verbose`、可选 `RunCancellation`、typed event sender 和 `llm_override`。`RunOptions` 还包含 working folder、thread/session、model/provider、MCP、dry-run、extra tools、tier/effort 等字段；字段存在不等于每个入口都暴露了同名 CLI 参数，入口 wiring 必须单独核对。

`build_react_config` 的关键顺序是：先加载 profile，复制 [`RunOptions`](../../agent/agent-core/src/run/types.rs)，做 model/provider 解析，再把 profile 补入未显式设置的值；随后从环境构造 `ReactBuildConfig`。显式 `base_url`/`api_key` 覆盖 config 中对应字段；显式 `tier` 能解析时设置 `model_tier`，解析失败只记录 warning。profile 的 model tier 仅在没有显式 model 且没有更高优先级 tier 时应用。

skills 是此阶段的一部分：extra tools 的 `builtin_skill()` 必须在 skill registry finalize 前注入，否则工具自身的 builtin skill 不会进入 LLM prompt。`load_memory_prompt()` 从 `env_config::home::anureo_home()/data/memory` 读取 snapshot；prompt 最后由 role、AGENTS.md、skills、memory、`EnvContext` 和 working folder 组装。

`run_agent_from_config` 最终只返回 `RunCompletion::Finished(AgentRunResult)` 或 `RunCompletion::Cancelled`。React/DUP/ToT 从 final state 取 `last_assistant_reply()` 和 reasoning；GoT 使用 `summary_result()`，reasoning 为 `None`。失败返回 `RunError`，不会被伪装成 Finished。

### 2.1 前置知识与术语

- **Pattern**：本文的四种执行策略。ReAct 是 Think → Act → Observe 循环；DUP 在计划前增加 Understand；ToT 维护候选并可回溯；GoT/AGoT 把任务拆成带依赖边的 task graph。DUP、ToT、GoT 不是四个 CLI crate 名，而是 `agent/agent-core` 中的 runner/state 实现。
- **Graph node / graph**：graph 是由节点和边组成的可编译执行图；graph node 是一次可测试的步骤，例如 `ThinkNode` 或 `ObserveNode`。条件边根据 state 选择下一个节点。
- **final `Values` / completion**：stream 中的 `Values` 事件携带最终 state；`runner_common` 只把最后一个 `Values` 当作成功运行的 state。`RunCompletion` 是更高层的结果（`Finished` 或 `Cancelled`）；stream channel 关闭、最后一条消息或最后一个 event 都不能替代 completion。
- **observation / display / raw**：`raw` 是工具原始输出；`observation` 是给下一次 LLM 判断使用的 normalized 文本；`display` 是面向 CLI/UI 的文本。它们可能因截断、strategy 或 storage reference 不同而不同，具体 accessor 见 [`agent/agent-core/src/state.rs`](../../agent/agent-core/src/state.rs)。
- **`Checkpointer` / `Store` / `resume_mode`**：`Checkpointer` 保存并读取 graph state，`Store` 保存运行相关的持久化数据；React 的 `resume_mode` 表示历史中已经有本轮 user message，恢复时不要再次追加。DUP/ToT 的通用 checkpoint merge 路径则会追加新的 user message。
- **profile / tier / `TypedAnyStreamEvent`**：profile 是可解析的 Agent 配置来源；tier 是 provider/model 的等级选择，实际优先级仍由 `config_builder` 源码决定；`TypedAnyStreamEvent` 是把四种 pattern 的 stream event 包在统一 enum 中的跨 runner 事件类型。

一个可复制的最小定位路径如下：先从 [`agent/agent-core/src/run/types.rs`](../../agent/agent-core/src/run/types.rs) 的 `RunOptions` 进入 [`agent/agent-core/src/run/config_builder.rs`](../../agent/agent-core/src/run/config_builder.rs)，再到 [`agent/agent-core/src/run/runner.rs`](../../agent/agent-core/src/run/runner.rs)、目标 pattern runner、[`agent/agent-core/src/runner_common.rs`](../../agent/agent-core/src/runner_common.rs)，最后到 graph node。若目标是 `apply_think`，从仓库根执行 `rg -n "apply_think|apply_think_" agent/agent-core/src`，再运行 `cargo test -p agent apply_think`；这里的 package 名是 `agent`，目录名仍是 `agent-core`，库 target 也不应据此猜成 `agent-core`。

## 3. ReAct 的状态与 graph

### 3.1 状态转换

[`agent/agent-core/src/state.rs`](../../agent/agent-core/src/state.rs) 中 `ReActState` 的核心字段是 `messages`、`tool_calls`、`tool_results`、`turn_count`、`think_count`、当前/累计 `usage`、`summary`、`should_continue` 和 `force_compact`。`ModelConfig` 保存 `model_id`、`ModelTier`、temperature 和 `tool_choice`。

`apply_think` 会：

1. 为缺少 id 的 tool call 生成 `call_<uuid6>`；
2. 将 assistant content、reasoning 或 tool calls 变成 assistant `Message`；三者全空时不追加空消息；
3. 更新 `last_reasoning_content`、当前和累计 usage、`message_count_after_last_think`；
4. 增加 `think_count`，并把固定后的 tool calls 写回状态。

`ToolResult` 同时保留 `content`、`raw_content`、`observation_text`、`display_text`、`storage_ref`、输出 strategy、字符计数、truncated 和 `is_error`。Observe 使用 `observation()`，展示/事件通常使用 `display()`；不要在新代码中把 raw output、observation 和 display 当成同一份数据。

### 3.2 图拓扑

[`agent/agent-core/src/agent/react/runner/runner.rs`](../../agent/agent-core/src/agent/react/runner/runner.rs) 的 `ReactRunner::new` 构造并 compile 如下图：

```text
START → think ──(tool_calls 非空)→ act → observe → compress → think
          │
          └────(tool_calls 为空)──────────────→ END
```

`think` 的条件函数只检查 `state.tool_calls.is_empty()`。`observe` 默认通过 `with_loop()` 开启回到 think 的循环；它把每个 `ToolResult` 转成 `Message::Tool`，附加 storage reference（若有），清空 tool buffers，并增加 `turn_count`。`with_loop_max_turns` 存在，但 `ReactRunner::new` 当前使用的是无上限的 `with_loop()`；不要把 `turn_count` 误写成默认的全局最大轮数。

`compress` 是 `CompressionGraphNode`，由 `CompactionConfig` 构造。`prune` 只扫描 tool-result message，从最新向旧累计 token，超过 `prune_keep_tokens` 的旧结果替换为 `[Old tool result cleared]`；待清理量小于 `prune_minimum` 时不变。`compact` 把较早消息交给 LLM，总结为一个 System message，并保留最近 `compact_keep_recent` 条；如果最近部分包含没有对应 assistant tool call 的 Tool message，会丢弃该孤儿消息，避免产生不合法的 tool message 配对。

### 3.3 Think、Act、Observe 的职责

`ThinkNode` 根据 `ReActState.model_config` 选择 model：显式 `model_id` 优先，否则按 tier 从 provider 配置解析，最后使用 provider default model；同一 model 的 client 在 node 内缓存。它支持普通 `invoke` 和 `invoke_stream`；stream 模式下发 `TurnStart`、reasoning/text/tool input 增量、`TurnFinish` 和 `Finish`，调用受 graph cancellation 包装。LLM response 经 `apply_think` 后才提交到 state。

`ActNode` 只是 `ToolCallExecutor` 的 graph adapter。executor 对 tool calls 顺序执行：解析 JSON 参数 → 检查空 tool name → 构造 `ToolCallContext`（recent messages、thread/user/depth、ACP session、cancellation 和 typed event adapter）→ 调用 `ToolRegistryLocked` → 做 output normalization → 发 ToolStart/ToolEnd 或 Custom progress。参数错误和工具返回错误会变成 `ToolResult.is_error`，供 LLM 自我纠正；取消则返回 `GraphError::Cancelled`。缺失 call id 会在 ToolCall 与 ToolResult 两侧同步补 `call_<uuid6>`。

`ObserveNode` 只消费 normalized observation view，不重新读取 raw output。它为每项结果生成 `Tool <name> result/error:` 的 Tool message；有 `storage_ref` 时追加 `Full output saved to: <path>`。没有 call id 时生成 synthetic id。之后清空 `tool_calls`/`tool_results` 并按 loop 配置返回 Continue 或 End。

## 4. checkpoint、stream、取消与错误

Runner 持有可选 `Checkpointer`、`Store` 和 `RunnableConfig`。ReAct 有 `resume_mode`：恢复模式调用 `build_react_initial_state_for_resume`，不追加 user message，因为消息已在历史中；普通模式调用 `build_react_initial_state`。DUP/ToT 的 initial state 使用 `load_from_checkpoint_or_build`：命中 checkpoint 后通过 merge 追加新 user message，并清空本轮 tool buffers。混淆这两条路径会造成重复 user message 或丢失新输入。

所有 pattern runner 都交给 [`runner_common.rs`](../../agent/agent-core/src/runner_common.rs) 的 `run_stream_with_config` 消费 compiled graph。它以 `StreamEvent` 通知调用者，并需要观察 graph completion；正常结束是 `Finished(state)`，取消是 `Cancelled`，结束前没有 `Values` 则是 `StreamEndedWithoutState`。因此 consumer 不能仅凭 stream channel 关闭或“看到了最后一条消息”判断成功。

跨 pattern 的 `TypedAnyStreamEvent` 有 `React`、`Dup`、`Tot`、`Got` 四个变体，并可转成 format A 或 protocol envelope。`on_event` 通过 `Arc<Mutex<FnMut...>>` 串行调用；若 `RunParams.any_stream_event_sender` 已有值，则不会用 `on_event` 覆盖它。工具侧的 `ToolCallContext` sender 通过 JSON value 反序列化回 typed event，不能假设任意 JSON 都会成功转换。

`RunCancellation` 最终转成 graph/tool 使用的 cancellation token。Think 在 LLM 前后检查 cancellation，tool invocation 通过 `run_cancellable`，runner 将取消映射为 `RunCompletion::Cancelled`。[`runner_error.rs`](../../agent/agent-core/src/runner_error.rs) 当前区分 compilation、checkpoint、graph execution、结构化 `ProviderError` 和 `StreamEndedWithoutState`；`LlmError::Provider` 保留 provider error 分类与 retry policy，其它 LLM error 转为 graph execution failure。`agent/agent-core/src/stats/event.rs` 目前没有公开统计事件模型，不能据此承诺 metrics API。

## 5. 其他 Agent pattern 的边界

### 5.1 DUP

`DupRunner` 的图是：

```text
START → understand → plan ──(core.tool_calls 非空)→ act → observe → plan
                             └────────────────────→ END
```

它用 `DupState { core: ReActState, understood }`，UnderstandNode 与 PlanNode 共享同一 LLM；Act/Observe 是 DUP adapter node。checkpoint、stream 和 cancellation 仍复用 runner common，但 pattern-specific 的理解/计划逻辑不应加入 `ThinkNode`。

### 5.2 ToT

ToT 的图是：

```text
START → think_expand → think_evaluate ──(tool_calls)→ act → observe
                              └────────────────────→ END
                                             │
                           suggest_backtrack 且还有候选 → backtrack → act
                           否则 → think_expand
```

`TotState` 包含 ReAct core 和 `TotExtension`。`ThinkExpandNode` 产生候选，`ThinkEvaluateNode` 选择候选，Observe 后可由条件函数回溯到 `backtrack`。`max_depth` 参数当前在构造函数中以 `let _ = max_depth` 保留，源码没有实现该参数的执行限制；不要把它写成已生效的深度上限。

### 5.3 GoT / AGoT

GoT 使用 `GotState` 的 task DAG。`ExecuteGraphNode` 找到 ready node，一次执行一个；每个 sub-task 内部又直接顺序运行 Think → Act → Observe，最多 `MAX_SUB_TASK_TURNS = 10` 次。前置节点结果写入后继 sub-task message，但每个结果最多取 500 字符。

成功会记录 `TaskStatus::Done` 和完整 result，失败记录 `TaskStatus::Failed` 并让 graph `Next::End`；Custom stream mode 下分别发 `GotNodeStart`、`GotNodeComplete` 或 `GotNodeFailed`。`got_adaptive` 为 true 时，完成后可按 heuristic 或 `agot_llm_complexity` 走复杂度判断并由 LLM 扩展 subgraph，产生 `GotExpand`；这是当前源码中的 adaptive/AGoT 行为，属于应谨慎依赖的实验性路径，不是独立稳定 API。

## 6. 扩展点

- 新增配置字段或 profile 合并：修改 [`agent/agent-core/src/run/config_builder.rs`](../../agent/agent-core/src/run/config_builder.rs) 及其 owning profile/config 类型，并覆盖 explicit option、profile fallback、环境默认值的优先级测试。
- 新增默认工具：评估 `RunOptions.default_extra_tools_provider`。provider 接收最终 `ReactBuildConfig`，其返回工具的 `builtin_skill()` 会在 skill registry finalize 前执行；不要在 `build_react_config` 返回后才注入依赖 builtin skill 的工具。
- 新增 ReAct 行为：优先在 `ThinkNode`、`ToolCallExecutor`、`ObserveNode` 或 `CompressionGraphNode` 的所属边界实现，并补 graph/状态测试；保持 `ActNode` 作为薄 adapter。
- 新增 Agent pattern：在 `agent/agent-core/src/agent/<pattern>` 提供 state、runner、initial state 和 builder，在 [`agent/agent-core/src/run/runner.rs`](../../agent/agent-core/src/run/runner.rs) 增加 `RunCmd`、`AnyRunner`、dispatch 与 typed event；只有确实需要入口支持时再修改 app wiring。
- 新增跨入口 stream 事件：先扩展对应 `StreamEvent<State>` 或 `TypedAnyStreamEvent` 的转换，再检查 ACP/server/CLI consumer；不要在 app adapter 中重定义 Agent state 语义。
- 新增 checkpoint/resume：明确“恢复已有输入”还是“恢复后追加新 user message”，分别选择 resume initializer 或 `load_from_checkpoint_or_build`。
- 修改 tool output：同时考虑 LLM observation、UI display、raw/storage reference、字符预算和 `is_error`；Observe 只消费 observation view。

## 7. 测试与验证

### 7.1 安全前置与范围

以下命令涉及两种范围：`cargo test -p agent` 从 anureo 仓库根执行；若要同时隔离测试的 working folder，则使用下面的 manifest 形式，从临时目录调用同一个 package。首次验证先把 anureo home 和当前 working folder 隔离到临时目录，并确保没有把真实 API key 或网络 provider 注入测试进程：

```powershell
$repoRoot = (Get-Location).Path
$testHome = Join-Path $env:TEMP ("anureo-agent-test-" + [guid]::NewGuid())
$testWorkdir = Join-Path $env:TEMP ("anureo-agent-workdir-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $testHome | Out-Null
New-Item -ItemType Directory -Path $testWorkdir | Out-Null
$previousanureoHome = $env:ANUREO_HOME
$isolatedVars = @("OPENAI_API_KEY", "OPENAI_BASE_URL", "MCP_GITHUB_URL", "GITHUB_TOKEN", "ANUREO_MCP_CONFIG_PATH")
$previousVars = @{}
try {
    $env:ANUREO_HOME = $testHome
    foreach ($name in $isolatedVars) {
        $previousVars[$name] = [System.Environment]::GetEnvironmentVariable($name)
        Remove-Item "Env:$name" -ErrorAction SilentlyContinue
    }
    Push-Location $testWorkdir
    cargo test --manifest-path (Join-Path $repoRoot "agent/agent-core/Cargo.toml")
} finally {
    Pop-Location
    if ($null -eq $previousanureoHome) { Remove-Item Env:ANUREO_HOME -ErrorAction SilentlyContinue }
    else { $env:ANUREO_HOME = $previousanureoHome }
    foreach ($name in $isolatedVars) {
        if ($null -eq $previousVars[$name]) { Remove-Item "Env:$name" -ErrorAction SilentlyContinue }
        else { Set-Item "Env:$name" $previousVars[$name] }
    }
    Remove-Item -LiteralPath $testHome -Recurse -Force
    Remove-Item -LiteralPath $testWorkdir -Recurse -Force
}
```

上述脚本暂时清除常见 API key、provider endpoint、MCP 配置路径并在退出时恢复；仍应在没有其他凭据注入的 shell 中运行。涉及 provider 的测试应使用源码已有 `MockLlm` 或本地 TCP fixture。`ANUREO_HOME` 会影响 [`agent/agent-core/src/run/config_builder.rs`](../../agent/agent-core/src/run/config_builder.rs) 读取的 memory/profile 等用户数据；临时 working folder 则避免测试把项目级 `AGENTS.md`、skills 或相对路径数据当作输入。[`agent/agent-core/src/test_support.rs`](../../agent/agent-core/src/test_support.rs) 的 `with_env()`/`env_lock()` 只为 agent crate 内测试提供进程级环境变量锁和恢复，不会自动保护其他 crate 的测试。

`cargo test -p agent` 是本 crate 的直接验证；`cargo test --manifest-path agent/agent-core/Cargo.toml` 是等价的 manifest 形式。package 名是 `agent`，目录名是 `agent-core`，不能写成 `cargo test -p agent-core`。`cargo test --workspace` 是扩大范围的可选验证，不是本页最小命令；它可能运行访问文件系统、启动子进程或依赖本机服务的集成测试，例如 workflow resume、`apps/acp/tests/e2e` 和其他应用测试。需要运行这些目标时，应按 [测试、调试与可观测性](./09-testing-debugging-and-observability.md) 的 fixture、临时目录和日志说明单独隔离。

源码已有测试覆盖以下事实：[`agent/agent-core/src/state.rs`](../../agent/agent-core/src/state.rs) 中的 `ReActState::apply_think` 空消息、reasoning、tool call id 和 usage accumulate；[`agent/agent-core/src/agent/react/mod.rs`](../../agent/agent-core/src/agent/react/mod.rs) 的 `tools_condition` 路由；[`agent/agent-core/src/agent/react/observe_node.rs`](../../agent/agent-core/src/agent/react/observe_node.rs) 的 tool message、错误标记、循环、turn count、synthetic id 和 storage ref；[`agent/agent-core/src/agent/react/act_executor.rs`](../../agent/agent-core/src/agent/react/act_executor.rs) 的 raw text 和 call-id backfill；DUP/ToT runner 用 `MockLlm` 完成无 tool call 的 stream；[`agent/agent-core/src/agent/got/execute_engine.rs`](../../agent/agent-core/src/agent/got/execute_engine.rs) 验证 predecessor result 拼接；[`agent/agent-core/src/compress`](../../agent/agent-core/src/compress) 验证 prune/summary 的边界；[`agent/agent-core/src/runner_error.rs`](../../agent/agent-core/src/runner_error.rs) 的 tests 验证 provider error 与无 final state 的显示。测试文件中的 `#[test]` 名称可用 `rg -n "apply_think|observe|dup|StreamEndedWithoutState" agent/agent-core/src` 定位。

建议按改动范围运行：

```powershell
cargo test -p agent
cargo check --workspace
cargo test --workspace # 可选：扩大范围，先完成上面的隔离步骤
```

pattern 或 stream 改动至少应补充 `MockLlm`、tool registry、取消、checkpoint hit/miss、最终 `Values` 缺失和错误映射测试。可直接用 `cargo test -p agent apply_think`、`cargo test -p agent observe_loop` 或 `cargo test -p agent dup_runner` 运行窄范围测试；将过滤词替换为测试名的一部分即可。涉及 provider/tier 时应使用确定性 provider fixture 或源码已有 mock，不要在单元测试中依赖真实模型服务。

测试 checkpoint 时要断言：resume 不重复追加 user message；普通 checkpoint merge 会追加新输入；不同 state wrapper 的 checkpointer 类型不能互换。测试 stream 时同时断言 callback event、completion 和 final state；只断言 event 数量不足以证明运行成功。

## 8. 常见坑

- 把 `build_react_config` 和 `run_agent_from_config` 当成一个 API：当前源码明确要求分两步。
- 把 `RunOptions.verbose_level` 当成 graph 的详细等级：注释说明它主要供 CLI startup banner；runtime 使用的是 `verbose: bool`。
- 误以为 `RunCmd::Got { got_adaptive }` 只影响展示：它会复制 config 并设置 `got_config.adaptive`。
- 把 `ObserveNode` 的无上限 `with_loop()` 误写成默认有最大轮数；GoT sub-task 的 10-turn 限制也只适用于 GoT 子任务。
- 把 ToT 的 `max_depth` 当成已生效配置；当前 runner 明确将它保留但未执行。
- 让 tool consumer 使用 `ToolResult.content` 代替 observation/display；大输出可能已 normalized、truncated 或写入 storage。
- 只监听 `StreamEvent`，不等待 completion；可能得到 cancelled、execution error 或 `StreamEndedWithoutState`。
- 把 provider error 都降成普通字符串；`RunnerError::Llm` 保留 `ProviderError` 的分类和 retry policy。
- 把 `stats/event.rs` 当成现成 metrics contract；当前文件只有占位注释。
- 把 GoT adaptive/AGoT、workflow 等演进路径承诺成稳定 API；本文只把源码已存在且有测试的行为描述为当前实现，并对 adaptive 路径标为实验性。

## 9. 最小贡献流程

1. 从 [`agent/agent-core/src/lib.rs`](../../agent/agent-core/src/lib.rs) 和目标模块确认公开面与 owning boundary。
2. 沿 `RunOptions → config_builder → run/runner → pattern runner → runner_common → graph node` 追踪数据和错误；对应仓库根相对路径分别从 `agent/agent-core/src` 下定位。
3. 先为 state transition、graph routing、checkpoint/resume、stream completion 或 cancellation 写针对性测试。
4. 新增工具时同时验证 `ToolCallContext`、output normalization、storage reference 和 event adapter。
5. 在隔离的 `ANUREO_HOME` 下运行 `cargo test -p agent apply_think` 或 `cargo test -p agent`，再运行 `cargo check --workspace`；按需运行扩大范围的 `cargo test --workspace`。
6. 修改后重新核对本文件中的路径、命令、配置字段和“实验性”标签是否仍与源码一致。

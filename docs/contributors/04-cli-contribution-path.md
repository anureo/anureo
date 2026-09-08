# anureo CLI 贡献路径

> **状态**：基于当前源码的贡献者说明
> **相关代码**：`apps/cli/src/main.rs`、`apps/cli/src/args.rs`、`apps/cli/src/run_flow.rs`、`apps/cli/src/run/`、`apps/cli/src/repl.rs`、`apps/cli/src/display/`
> **测试**：`apps/cli/tests/mcp_cli_test.rs` 及各 CLI 模块内的 `#[cfg(test)]`

本文面向需要修改 anureo CLI 的贡献者。文中的命令、配置项、模块职责和行为均以当前列出的源码为准；未在这些文件中出现的 API 不作为已实现接口。涉及占位或仍在演进的行为会明确标注。

## 1. CLI 的边界

`apps/cli` 同时提供 `anureo` binary 和 `cli` library。`lib.rs` 将 CLI 的参数、run orchestration、session、MCP、model/tool 查询及 display 相关模块公开给其它入口；`main.rs` 负责进程级启动和 dispatch。Agent graph、`RunOptions`、`RunCmd`、`RunError` 与 `TypedAnyStreamEvent` 来自 `agent` crate，CLI 不在入口中重新实现 Agent loop。

当前边界可以概括为：

| 模块 | owning responsibility | 贡献者通常在此修改什么 |
| --- | --- | --- |
| `args.rs` | Clap schema 和 command tree | 新增/修改 flag、subcommand、help 文本 |
| `bootstrap.rs` | 配置报告、shell 环境快照、logging 初始化 | 启动顺序和日志来源 |
| `run_flow.rs` | CLI 参数到 `RunOptions` 的映射、单轮/交互模式 | 新增运行参数的 plumbing、session 起点 |
| `run/agent.rs` | Agent config 构建后的 CLI wrapper、stream display callback | CLI 侧事件消费、banner、JSON envelope |
| `run/contract.rs` | `run_cli_turn`、`RunOutput`、JSON stream contract | 入口和 Agent 之间的结果契约 |
| `repl.rs` | stdin REPL 和每轮调用 | 多轮输入、退出和 REPL command |
| `subcommands.rs` | 管理型 command 的 dispatch | tool/model/session/MCP/skills/agent 等命令接线 |
| `session.rs`、`run/session_store.rs` | session 查询、展示、checkpoint payload 投影 | session list/show/cat/delete/rename |
| `display/`、`output.rs` | terminal formatting、stream event 和 stdout/file 输出 | 展示格式，不改 Agent state 语义 |
| `mcp_manager.rs` | MCP 配置文件的 CRUD | `mcp` 管理命令的文件操作 |

Foundation、Agent、Tool 或 ACP 的公共能力应回到其 owning crate；CLI 只负责应用入口适配。

## 2. 从进程入口到 Agent run

一次普通 run 的调用路径如下：

```text
Args::parse()
  │
  ├─ validate_tier_arg / check_model_tier_conflict
  ├─ ACP command? ──> anureo_acp::server（stdout 保留给 JSON-RPC）
  ├─ preserve_shell_env -> print_config_report -> init_logging
  ├─ 管理型 Command ──> subcommands.rs / 专用 command module
  └─ React / Dup / Tot / Got
       ├─ resolve_user_message
       ├─ build_run_options -> RunOptions
       ├─ 注册 Ctrl+C cancellation
       └─ run_single_turn_mode 或 run_interactive_mode
            └─ repl::run_one_turn -> run_cli_turn
                 └─ run_agent_wrapper
                      ├─ build_react_config
                      ├─ resolve tier（需要时）
                      ├─ run_agent_from_config
                      └─ RunOutput / stream events
```

`main.rs` 先解析参数并校验 tier。`--model` 与 `--tier` 同时存在会在执行前报错；`--tier` 当前只接受 `light`、`standard`、`strong`，大小写不敏感。ACP 分支必须早于 config report 和 logging，因为 stdout 是 ACP JSON-RPC 通道；`acp --show-log-dir`、`acp reload/start/stop/restart` 也在此处直接处理。

非 ACP 路径先保存 `LOG_FILE`、`RUST_LOG` 等 shell 环境快照，再打印配置摘要并初始化 logging。管理型 command 在启动后立即 return，不进入 Agent run。没有 `-m/--message`、也没有 positional message 且不是 `--interactive` 时，进程报错退出。

`run_flow::build_run_options` 把 CLI 字段写入 `RunOptions`：`working_folder`、`thread_id`（来自 `--session-id`）、`agent`、`model`、`provider`、`tier`、`effort`、`mcp_config_path`、`dry_run`、`debug_llm`、`worktree`、`output_json`、`output_timestamp` 和 `got_adaptive` 等。默认 `model` 取 `config::default_model()`；默认 extra tools provider 是 workflow tool provider。

单轮模式会在缺省时生成 session id，运行一轮后输出 reply；交互模式复用同一个 `thread_id`，可先运行初始 message，再进入 stdin REPL。Ctrl+C 第一次 cancel 当前 run；两秒内第二次会 force-exit。REPL 还监听 force-quit notification，避免在等待 stdin 时无法退出。

## 3. 参数和命令扩展

### 3.1 顶层参数

`Args` 当前包含这些影响 run 的顶层参数：

| 参数 | 当前行为 |
| --- | --- |
| `-m/--message`、positional args | 用户输入；显式 `-m` 优先，positional args 以空格拼接 |
| `-w/--working-folder <DIR>` | file tools 使用的工作目录；未指定时由运行时使用当前目录 |
| `-M/--model <MODEL>` | model override；源码 help 支持 bare name 或 `provider/model` 形式 |
| `--provider <PROVIDER>` | 从 config 的 `[[providers]]` 选择 provider |
| `--tier <TIER>` | `light`、`standard`、`strong`，不能与 `--model` 同用 |
| `-P/--agent <NAME>` | 从 `.anureo/agents/<NAME>` 或 `~/.anureo/agents/<NAME>` 读取 named profile |
| `-s/--session-id <ID>` | 设置连续对话使用的 checkpoint thread id |
| `-v`、`-vv` | 增加 CLI 展示信息；`-vvv` 及以上与 `-vv` 相同 |
| `-i/--interactive` | 运行 REPL |
| `--json`、`--pretty`、`--file <PATH>` | JSON event/reply 输出；`--file` 使用追加写入 |
| `--image <PATH>` | 可重复；以 `working_folder` 或 cwd 为相对路径基准 |
| `--mcp-config <PATH>` | 覆盖 MCP 配置路径参数 |
| `--dry`、`--worktree`、`--debug-llm` | dry run、隔离 worktree、将完整 prompt/messages 输出到 stderr |
| `--effort <LEVEL>` | 原样传给下游 reasoning effort；help 列出 `auto`、`none`、`minimal`、`low`、`medium`、`high`、`xhigh` |
| `--log-level`、`--log-file`、`--log-rotate`、`--log-format` | logging CLI 参数；rotation 为 `none`、`daily`、`hourly`、`minutely`，format 为 `text` 或 `json` |

新增顶层 flag 时，不能只修改 `Args`：应同时检查 `build_run_options`、冲突/合法性校验、JSON/REPL 路径及测试。字段存在于 `RunOptions` 不表示 CLI 已经暴露该参数。

### 3.2 Command dispatch

当前 `Command` 包括：

```text
react（默认）、dup、tot、got
tool、session、models、mcp、agent
goal、skills、skill-usage、curator、memory
review-skill、review、task、acp、evolve
```

`react`、`dup`、`tot`、`got` 是唯一进入通用 Agent run 的四个模式；REPL 的 `cmd_to_runcmd` 也只映射这四类。`got` 的 `--got-adaptive` 通过 `RunCmd::Got { got_adaptive }` 传递。

其它 command 在 `main.rs` 中分派到 `subcommands.rs` 或对应模块后 return。扩展管理命令时，应在 `args.rs` 增加 Clap 类型，在 `main.rs` 增加 dispatch，并将实际业务放进 owning module；不要把数据库、MCP 配置或 skill registry 逻辑塞进 `main.rs`。

## 4. Agent、stream 和输出契约

`run/agent.rs::run_agent_wrapper` 先调用 `build_react_config`，必要时对 tier 做 `resolve_tier_and_build_config`，然后调用 `run_agent_from_config`。wrapper 同时负责加载工具/agent 信息、普通 terminal display、JSON protocol envelope 和 completion 映射。Agent 运行结果只有 `Finished` 或 `Cancelled` 两类 completion；CLI 映射到 `RunStopReason::EndTurn` 或 `Cancelled`。

`run/contract.rs` 定义 `run_cli_turn`：

- `stream_out = Some`：事件到达即通过 sink 转发，不在内存中累计，返回 `RunOutput::Reply`；
- `stream_out = None` 且 `--json`：累计事件，返回 `RunOutput::Json`；
- 非 JSON：返回 `RunOutput::Reply`，普通终端输出由 display callback 负责。

JSON reply 会带 `reply` 和 `stop_reason`；有 session 时还带 `session_id`，有 reasoning 或 protocol envelope 时追加对应字段。`--file` 以 JSON line 方式追加；`--pretty` 只改变序列化格式。`output.rs` 对 `node_enter` event 仍会将 `Entering: <id>` 写到 stderr，因此脚本只能解析 stdout/file 中的 JSON。

display 层接收 `TypedAnyStreamEvent`，按 `React`、`Dup`、`Tot`、`Got` 分派到各自 handler。handler 处理 `TaskStart`、`TextDelta`、`ReasoningDelta`、`Updates`、`TurnFinish` 等 stream event；verbose 模式显示 state、tool summary、DUP/ToT/GoT 的额外事件，普通模式则使用 spinner、tool call/result preview 和 markdown renderer。`display/format.rs` 只做截断和状态展示，不应成为新的运行时状态来源。

新增 event 时，先在 Agent/stream event 所属层定义语义，再检查 `TypedAnyStreamEvent` 转换、CLI display、JSON envelope 和 ACP/server consumer；不要只在 terminal handler 中发明一个未被其它 consumer 理解的字段。

## 5. 主要管理命令的扩展点

### 5.1 session

`SessionManager` 使用默认 checkpoint 数据库；`subcommands.rs` 的 `session list` 会读取 config 中的 session defaults，并支持 `limit`、`format`、`since`、`until`、`grep`、reverse、oneline、no-pager 等展示/筛选路径。stdout 为 TTY、非 JSON、且没有显式 format 时可能使用 pager。`show`、`delete`、`rename`、`cat` 分别投影 checkpoint 信息、删除 checkpoint、更新最近 checkpoint 的 title、重建 Codex events。

session 搜索在 `session.rs` 中优先探测 SQLite FTS5/trigram，失败时回退到 per-token `LIKE`；不要把 FTS5 当成运行环境必备能力。修改 payload projection 时应同步检查 `cat_session`、`extract_session_text`、JSON 输出和 search 结果。

### 5.2 tool、models、agent

`tool list/show` 通过 `run::cli_list_tools`、`cli_show_tool` 调用 CLI library 的 tool 查询；show 支持 YAML，`--json` 或 `--output json` 选择 JSON。`models` 通过 `model_cmd` 读取 `config::load_full_config("anureo")`，从 `ModelRegistry` 查询 configured providers；每个 provider 最多展示 30 个模型，JSON 结果包含 `provider`、`models`、`error`。

`agent list` 使用 `agent::profile::list_available_profiles`；`agent export` 使用 profile conversion，支持 dry-run 或写到指定 output 目录。profile 的发现和解析属于 Agent/profile 层，CLI 只负责 command wiring 和输出。

### 5.3 MCP

`McpManager::new` 的发现优先级是当前项目 `.anureo/mcp.json`，再到 `~/.anureo/mcp.json`；没有文件时会创建 home 下的文件。`mcp add` 必须提供 `--command` 或 `--url` 之一；stdio entry 保存 command/args，HTTP entry 保存 url。`--env KEY=VALUE` 被拆成环境映射，show 时通过 `config::mask_value` 遮蔽环境值。

MCP 的编辑会保留未提供的 existing fields，delete/enable/disable 通过 config crate 的读写函数完成。这里的 `--mcp-config` 是 Agent run 的路径参数；当前 `McpManager::new` 本身调用 discovery，不接受该 CLI 参数。不要假设管理命令已经能操作任意 `--mcp-config` 指定文件。

`apps/cli/tests/mcp_cli_test.rs` 目前只验证 `AddMcpArgs`、`EditMcpArgs` 的结构和 command/url 数据，并明确注明真实文件操作测试尚待适配；它不是完整的 CRUD 集成测试。

### 5.4 skills、goal、task 和 review

`skills` command 直接操作 skill registry，当前支持 list/show/inspect/create/edit/delete/sync；`inspect` 还支持 `--all`、`--read-file`、`--source`、JSON/pretty/file 输出。`skill-usage`、`curator`、`memory`、`review-skill`、`review`、`goal`、`task` 各自有独立模块，新增行为应沿现有 command → handler → owning service 的边界接线。

`evolve` 当前在 `main.rs` 直接输出 `evolve: not yet implemented (anureo-evolution crate removed)` 并退出非零，属于明确的未实现能力。REPL 中 `/models`、`/model`、`/tools`、`/resume`、`/undo`、`/retry`、`/history`、`/exit` 等部分 command 当前只返回 stub 文本或“不支持”提示；不要把它们写成已完成的 session/model 操作。

## 6. 图片和其它实验性路径

`--image` 的当前实现位于 `run_flow.rs`：没有图片时是 `UserContent::Text`；模型 id 命中源码中的 vision hint 时，将不超过 8 MB 的文件读入并编码为 data URL 的 `UserContent::Multimodal`；读取失败或超大文件只 warning 并跳过。非 vision hint 模型走 text fallback，仅在 message 前加入 `[attached image: <name>]` 标记。

源码注释明确说明：text fallback 不在 CLI dispatch 中执行完整 `vision_analyze` LLM round-trip；下游 agent/ACP 路径是否进一步处理属于未完成 wiring。因此扩展图片能力时必须补模型能力解析、文件错误策略和端到端测试，不能把当前 marker 描述成图片理解 API。

GoT adaptive、workflow tool、Kanban transient exit-code 等路径在源码中已有接线，但仍应谨慎视为演进中的行为；尤其不要从 CLI flag 推导出 Agent crate 中不存在的稳定 API。

## 7. 测试和验证

建议按改动范围验证：

```powershell
cargo fmt --check
cargo check -p anureo-cli
cargo test -p anureo-cli
cargo test -p anureo-cli --test mcp_cli_test
```

如果 workspace 中 Cargo package 名称与 `cli` 不一致，应以 `apps/cli/Cargo.toml` 的 package name 为准。修改跨 crate 的 Agent/Tool/Config 行为时，再运行相应 crate tests 和 `cargo check --workspace`；不应仅凭 CLI unit tests 证明下游 graph 行为正确。

应优先补这些测试：

| 改动 | 最小验证 |
| --- | --- |
| 新 top-level flag | `Args::parse_from` + `build_run_options` 字段断言 |
| model/tier/effort plumbing | 合法值、非法值、冲突和 `None` 默认值 |
| single-turn/REPL | message precedence、session id、quit/EOF、`RunCmd` mapping |
| JSON output | stream sink 即时转发、file append、pretty、reply envelope 和 session id |
| display event | 每个新增 `StreamEvent` 在 verbose/non-verbose 下不 panic 且语义正确 |
| session | list filter、pager 条件、cat/search projection 和空结果 |
| MCP | command/url 校验、merge 保留字段、mask secret；当前 placeholder 测试不足时应先隔离 config path |
| error/cancel | invalid working folder、`RunError`、首次 Ctrl+C cancel、completion 状态 |

测试中要避免真实 LLM/provider；`run_agent.rs` 已有 invalid working folder 的确定性失败测试。涉及环境变量、当前目录、MCP 或 session 数据库时使用临时目录，并恢复进程级环境/工作目录，避免测试相互污染。

## 8. 常见坑

- 只改 `args.rs`，忘记 `build_run_options`；这样 Clap 能解析，但 Agent 根本收不到新值。
- 把 `--model` 和 `--tier` 当成可叠加选项；`main.rs` 会在运行前拒绝二者并用明确错误退出。
- 在 ACP 路径打印 config report 或普通日志到 stdout；这会破坏 JSON-RPC。
- 以为 `--json` 只输出最终 reply；当前还会输出 stream events，`--file` 是追加写入，且 `node_enter` 的提示在 stderr。
- 只监听 event stream 判断成功；最终结果还由 `RunCompletion`/`RunStopReason` 表示，可能是 cancelled 或 error。
- 把 display state 当成 Agent state；`EventState` 只是 CLI 展示缓存，Observe/tool result 的语义在 Agent 层。
- 把 `--session-id` 误认为普通 UI 标题；它实际进入 `RunOptions.thread_id`，决定 checkpoint continuity。
- 把 MCP 管理的 discovery 路径和 run 的 `--mcp-config` 混为一谈；前者由 `McpManager::new` 固定 discovery，后者只是 run option。
- 把 `session list` 的 FTS5/trigram 当成硬依赖；源码有 `LIKE` fallback。
- 把 REPL slash command、`evolve` 或 `--image` text fallback 写成完整功能；它们当前分别是 stub、明确未实现或降级路径。
- 在日志、MCP detail 或新增诊断中输出 secret；已有 MCP detail 使用 `config::mask_value`，新代码应保持同样边界。

## 9. 最小贡献流程

1. 先在 `args.rs` 找到用户输入的 owning type，再沿 `main.rs → run_flow.rs → run/contract.rs` 追踪它是否真的到达 `RunOptions` 或 `RunCmd`。
2. 判断改动属于 Agent runtime、Tool、Config、session store 还是 CLI display；只在 CLI owning boundary 做 wiring。
3. 明确普通 text、JSON、file、interactive、ACP 和管理 command 是否都受影响。
4. 先补确定性 unit/integration tests，再运行相关 `cargo test` 和 `cargo check`。
5. 重新检查 stdout/stderr、退出码、session continuity、secret masking 和实验性标记，最后更新贡献者文档中的源码路径与命令。

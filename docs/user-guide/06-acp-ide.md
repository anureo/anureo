# 在 IDE 中使用 anureo（ACP stdio）

本指南面向支持 Agent Client Protocol（ACP）的 IDE 使用者和集成维护者。anureo 的 IDE 接入点是一个基于 stdin/stdout 的 JSON-RPC agent server；CLI 与 ACP 共用项目配置、模型解析和 anureo 的 session/checkpoint 语义。

> **实验性说明**：ACP 集成仍在演进。本文只描述当前源码和集成测试已经覆盖的 stdio 路径；不承诺多客户端共享一个进程、生产级断线恢复或其他未验证的部署场景。

## 前置条件

- 已安装可执行的 `anureo`，并能在集成 IDE 启动它。
- IDE 支持 ACP 的 stdio transport。
- 在 ACP 进程启动时准备好 anureo 使用的 provider/API key。anureo 会读取 `~/.anureo/config.toml`（可用 `$ANUREO_HOME` 覆盖配置目录）的 `[env]` 表，并按进程启动时的环境和当前目录解析配置；`session/new` 的 `cwd` 不会使 anureo 重新加载该目录中的项目 `.env`。请让 ACP 进程从目标项目目录启动，或显式配置所需环境变量。
- IDE 允许为 agent 配置一个 command、工作区目录，以及（若支持）file、terminal、MCP 和 permission 能力。

## 启动命令与 IDE 配置

最小启动命令是：

```text
anureo acp
```

在 IDE 的 ACP agent 配置中，将 command 设置为 `anureo`，arguments 设置为 `acp`。如果 IDE 要求一个完整 command line，则使用 `anureo acp`。Windows 上应填写 anureo 可执行文件的绝对路径，或确保 IDE 启动时的 `PATH` 能找到它；working directory 设置为当前项目根目录。

不要把 `anureo acp start` 当作 stdio command：`start` 会启动隐藏的后台 daemon，关闭 stdio，不向 IDE 提供 JSON-RPC 输入输出。`anureo acp stop`、`anureo acp restart` 和 Unix-only 的 `anureo acp reload` 是 daemon 运维命令。

如果机器上有多个 anureo 或多个配置目录，先在 IDE 配置中固定 executable 与环境变量，再检查 IDE 传给 `session/new` 的绝对 `cwd`。ACP 启动阶段静默加载 anureo 配置，不输出 CLI 的 config report。

## stdout、日志与故障排查

stdio transport 的约束很严格：stdout 只承载逐行 JSON-RPC response/notification。任何 shell banner、debug `println!` 或启动提示都会破坏 IDE 的 ACP parser。诊断信息应看日志，而不是把 `stdout` 重定向为普通文本。

查看 ACP 日志/PID 目录：

```text
anureo acp --show-log-dir
```

当前目录函数返回 `~/.anureo/acp`，或 `$ANUREO_HOME/acp`。正常情况下可在该目录查看 `anureo-acp.log` 和 `anureo-acp.pid`；fatal error 也会提示检查该日志。若需要更详细的 tracing，可把全局 CLI 参数放在 subcommand 前：

```text
anureo --log-level debug acp
anureo --log-file C:\\path\\to\\anureo-acp.log --log-level debug acp
```

`--log-file` 覆盖 `LOG_FILE`；未指定日志文件时日志可能被丢弃。`--log-rotate` 仅在配合 `--log-file` 时有意义。故障排查顺序建议是：确认 IDE 实际启动了同一个 `anureo`、确认 stdout 中没有额外文本、运行 `anureo acp --show-log-dir`、再在 ACP 日志中查 `initialize`、session ID、tool 和连接关闭记录。

## 一次 ACP session 的可见流程

典型生命周期如下：

1. IDE 启动 `anureo acp`，通过 `initialize` 协商 protocol version 和 client capabilities。anureo 返回 agent 信息 `anureo`、`load_session`、session list/resume、MCP HTTP 和 prompt 能力等声明；其 `mcpCapabilities.http` 为 `true`、`mcpCapabilities.sse` 明确为 `false`。这与客户端在 `client_capabilities` 中声明自己支持的 MCP transport 是两组独立能力。客户端声明的 fs、terminal、MCP、prompt、session 能力会被保存到本次 connection。
2. IDE 发送 `session/new`，至少带一个项目工作目录 `cwd`。为获得可预测的工作目录和正确的工具执行行为，客户端应提供绝对路径；当前实现会直接接收并保存传入值，并未在 handler 中拒绝相对路径。anureo 生成新的 `session_id`，保存该 session 的工作目录，并返回当前 `modes` 与 `configOptions`。
3. IDE 发送 `session/prompt`。运行过程中 IDE 接收 `session/update`，包括 agent message/thought chunks、tool call/update、plan、当前 mode 或 config 更新。
4. 若 IDE 支持恢复，发送 `session/load`，提供 `session_id`、`cwd` 和可选 MCP servers。anureo 按该 ID 读取 checkpoint 历史并通过 updates 重放；没有 checkpoint 时从空历史开始。
5. 若 IDE 声明 session list 能力，可发送 `session/list`。anureo 从 SQLite checkpoints 按最近更新时间返回 session ID、工作目录、标题、更新时间和 `_meta` 统计；当前 SQLite checkpoint 不保存原始 `cwd`，所以列表中的每个 session 都会回退为默认工作目录。请求中的 `cwd` filter 当前也会被忽略。

`session/list` 的分页 `nextCursor` 当前为 `None`。**实验性实现限制**：请求中的 `cwd` filter 在当前 SQLite 查询中尚未生效，因此不要把它当作项目隔离保证；客户端应自行按返回的 `cwd` 检查和展示。

### ACP session_id 与 anureo thread/session

ACP 返回的 `session_id` 是 anureo 内部 session 的唯一键，并直接作为 anureo checkpoint 的 `thread_id`。新建 session 的两者相同；load 一个此前未驻留内存的 ID 时，anureo 会用该 ID 创建对应的内存 entry，再按相同 thread ID 查 checkpoint。因而恢复时必须保留原始 ACP `session_id`，不能每次打开 IDE 都随机生成新 ID。

这不是“仅恢复 UI 标签”：checkpoint 中的消息和工具历史属于该 thread。一个 session 的 prompt 正在运行时，另一个 prompt 会被拒绝；应等待当前 turn 结束或先取消。

## working directory、model、mode、effort 与持久化

`session/new` 的 `cwd` 是项目的工作目录。客户端应提供 absolute path，以确保 `working_folder`、文件工具和 terminal 工具的行为可预测；当前源码没有对相对路径作 `is_absolute` 校验。`session/load` 仍要求客户端提供 `cwd`；它用于本次加载和运行，不应假定 anureo 从 session config 自动恢复目录。

session config 的用户可见选项是：

| config ID | 含义 | 可选值/规则 |
| --- | --- | --- |
| `model` | 本 session 使用的模型；设为 `default` 会清除 session override | 由已配置 provider 返回的 model options；也可沿用 CLI/环境解析出的当前模型 |
| `mode` | agent mode，决定使用的 agent/run command | 当前内置测试覆盖 `ask` 和默认的 `dev`；IDE 必须使用返回的 mode ID，未知 ID 会报错 |
| `effort` | reasoning effort | `auto`、`none`、`minimal`、`low`、`medium`、`high`、`xhigh`；`auto` 表示使用模型默认值 |

`session/new` 返回 `configOptions`，其中包含 mode、model，并在当前模型提供 reasoning effort 列表时包含 effort。IDE 可用 `session/set_config_option` 更新它们；成功响应会返回刷新后的 `configOptions`。也可用 `session/set_mode` 更新 mode，但该请求成功时返回空的 success response，不携带更新后的 config options 或 mode payload；客户端应使用 mode update notification，或按需重新加载状态。传入未知 config ID、未知 session 或无效 effort 会失败。

model、mode、effort 会写入 checkpoint 数据库旁的 `session_config` SQLite 表，因此可以跨 anureo 进程恢复。`fork` 已在实现中，会复制 session config 和 MCP server，并生成新的 session/thread ID，之后两者独立；但当前 `initialize` 的 agent capabilities 没有广告 fork，因此它是未广告、对兼容性敏感的操作。只有在客户端明确支持并实际接受该请求时才使用，或先确认目标版本已广告该能力。

CLI 与 ACP 的模型语义保持一致：ACP 进程启动时的环境和当前目录、`~/.anureo/config.toml`/`$ANUREO_HOME` 环境配置以及 provider 配置参与解析；改变 session `cwd` 不会单独触发该项目 `.env` 的加载。ACP 的 session-level model override 优先于环境默认值。CLI 也会保存最近选择的 model，ACP 新建 session 会把它作为初始显示值（`default` 除外）。因此“IDE 显示的 model”和“CLI 默认 model”可能不同：先检查 session config，再检查 ACP 启动环境、anureo home 配置和最近模型。

## file、terminal、MCP 与 permission

IDE 应在 `initialize.client_capabilities` 中只声明自己实际实现的能力：

- `fs.readTextFile` / `fs.writeTextFile`：anureo 可通过 ACP reverse request 读写 IDE workspace。读操作支持 `path`、可选的 1-based `line` 和 `limit`，并可能看到未保存 buffer；写操作返回 diff，IDE 可能显示确认或未保存状态。
- `terminal: true`：anureo 可请求 IDE 创建 terminal、等待退出、读取 output、kill 和 release。Windows 命令使用 `powershell -NoProfile -Command`，其他平台使用 `sh -c`；working directory 和环境变量随请求传递。超时会返回部分输出或错误，取决于执行路径。
- `mcp.http`、`mcp.stdio`、`mcp.sse`：表示 IDE 能支持的 MCP transport。session/new 和 session/load 中的 MCP servers 按 session 保存，不与其他 session 共享。

能力未声明时，anureo 不应把 IDE 的 fs/terminal bridge 当作可用；应检查 IDE 的 capability 配置和 ACP 日志。工具调用的顺序通常是 `session/update` 的 pending tool call，随后由客户端决定是否需要确认，最后发送 running 以及 success/failure update。当前审计到的 anureo 工具实现直接调用客户端的 file/terminal bridge；它没有在这些调用中实现 permission scope 或 `Allow once` 语义。因此下述授权范围是 IDE/client 的策略期望，不应当作 anureo 的强制保证。

### permission 的风险边界

如果客户端产生 `session/request_permission`，IDE 用户应同时核对 tool、目标路径或完整 command、working directory 和客户端显示的 permission scope。`Allow once`、session authorization 和 project authorization 的范围由客户端策略决定；anureo 当前实现不保证这些语义，也不保证每个 pending permission 会因 `session/cancel` 变为 Cancelled。实现层面可确认的是：文件工具直接调用客户端读写 bridge；terminal 工具直接调用 terminal create/wait/output/release bridge，超时路径会 kill/release terminal。拒绝、取消和超时的最终展示应以客户端的 ACP 行为和返回结果为准。

- **Allow once**：适合一次明确、低影响的读取或命令；下一次调用仍应重新判断。
- **session authorization**：减少同一 ACP session 内的重复确认，风险范围仍包括该 session 后续可能产生的 tool calls。
- **project authorization**：范围更大，可能覆盖项目中的文件写入和 terminal 命令；只对可信项目、可信 command 和可接受的破坏半径使用。
- 拒绝或取消 permission 时，tool 不应被视为成功；客户端应展示 denied/cancelled，并保留原始 target/command 供审计。

## streaming、取消、stop reason 与断线诊断

ACP prompt 是 streaming 的：先通过 `session/update` 接收增量消息和工具状态，最终 `session/prompt` response 给出 `stopReason`。当前实现的正常完成值序列化为 `end_turn`，取消值序列化为 `cancelled`。不要把 `finished` 当作 anureo 的正常完成值；对于 max-token、max-turn 或 refusal 等其他情况，除非目标 ACP schema 和目标 anureo 版本的实现已核实，否则不要在客户端逻辑中假定具体 stopReason 拼写。

取消当前 turn 时，IDE 发送 `session/cancel` 并带同一个 `session_id`。anureo 会设置 session cancel flag，同时触发当前 generation 的 runtime cancellation；当前实现确认的 prompt 最终 `stopReason` 是 `cancelled`。挂起 permission 的取消结果和授权语义属于客户端行为，不能仅凭 anureo ACP handler 推断。

连接关闭时 stdio loop 会取消所有 active generations，stdin EOF 会结束 transport；这不是已验证的跨进程恢复协议。若 IDE 断开或没有显示最终结果：

1. 保存并查阅 IDE 的 ACP transport 日志，以及 `anureo acp --show-log-dir` 指向的 `anureo-acp.log`。
2. 搜索同一 `session_id` 的 `prompt`、`session/update`、`cancel`、`connection closed` 和 `fatal error`。
3. 重新连接后用原 session ID 调用 `session/load`，并提供正确的绝对 `cwd`；检查是否重放 checkpoint history。
4. 若 `load` 没有历史，确认原进程启动时实际使用的 `$ANUREO_HOME`、环境/当前目录和 SQLite checkpoint 位置一致；不要只根据 IDE 的 tab 标题判断 session 是否相同，也不要假定 `session/load` 的 `cwd` 会重新加载该目录的 `.env`。

## 一个可运行的最小检查

先在终端验证二进制和日志目录：

```text
anureo --version
anureo acp --show-log-dir
```

然后在 IDE 中配置：

```text
command: anureo
arguments: acp
working directory: C:\\work\\my-project
```

打开项目后，确认 IDE 的 ACP trace 中依次出现 `initialize`、`session/new` 和带有 `session_id` 的 `session/prompt`。修改一次 model、mode 或 effort，再重新打开 IDE，用相同 `session_id` 执行 `session/load`；返回的 `configOptions` 和 `modes` 应反映已保存的值。最后用一个明确的只读 prompt 检查 file/terminal capability；若出现 permission 请求，优先选择 `Allow once` 并核对实际路径或 command。

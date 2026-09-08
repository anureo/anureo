# anureo Troubleshooting

> **状态**：已实现的 CLI、session、ACP 与 workflow 排查路径；workflow 与 `--worktree` 相关行为仍是实验性

本文面向遇到 configuration、model、permission、session、ACP、MCP 或 workflow 问题的 anureo 开发者。目标是先收集可复现、可脱敏的最小证据，再沿症状缩小范围；不要把完整环境变量、API key、cookie、私有源码或全部 raw events 复制到 issue。

## 1. 先固定最小诊断上下文

每次排查先记录以下字段。`session ID` 是 `--session-id` 或输出中的 `thread_id`；workflow 还要记录 `instance_dir`。ACP 同时记录 IDE 发来的 `session_id` 与 anureo 日志位置。

| 字段 | 记录方式 | 注意事项 |
| --- | --- | --- |
| version / commit | `anureo --version`；源码 checkout 用 `git rev-parse --short HEAD` | 版本和 commit 要与日志对应 |
| entry point | 完整的 `anureo ...` 或 `cargo run -p anureo-cli -- ...` 命令 | 保留选项，不要保留 secret 值 |
| working directory | `Get-Location`、`git rev-parse --show-toplevel`、实际的 `--working-folder` | `.env` 按 effective working directory 查找 |
| session / instance ID | `--session-id <ID>`、`anureo session list --oneline`、`workflow_start` 返回的 `instance_dir` | ID 可公开；不要附带完整 session 内容 |
| model / provider | `--model`、`--provider`、`--tier`，以及脱敏的 `-vvv` config report | 记录名称和 tier，不记录 key/base URL 中的凭据 |
| error class | `LlmError` 的 `Provider`、`InvokeFailed`、`EmptyResponse`、`Cancelled`，或 CLI/ACP/workflow 的 terminal status | “failed”不是根因；记录可重试性和原始短消息 |
| log location | CLI 的 `--log-file` 或默认 `~/.anureo/logs/cli/anureo-cli.log`；ACP 的 state/PID 目录用 `anureo acp --show-log-dir`，默认日志文件见第 5 节 | 不要把 log 内容原样上传 |

可以先生成一个低风险的配置摘要；`-vvv` 会报告 `.env`、`config.toml`、active provider 和脱敏的 key summary：

```powershell
anureo --version
git rev-parse --short HEAD
Get-Location
git rev-parse --show-toplevel
anureo -vvv --working-folder . -m "只读报告当前配置来源和模型选择；不要输出秘密，不修改文件"
```

注意：当前 `-vvv` 报告实现没有把 `--working-folder` 传给配置加载器，因此报告中的 `.env` 路径按进程当前目录解析。若 `--working-folder` 与当前目录不同，必须另外检查 `<effective-working-folder>/.env`；不要把报告显示的 `.env` 路径当作 effective working folder 下的确认结果。

配置加载器的来源优先级通常是 shell environment > 项目 `.env` > 被选中的 `[[providers]]` > `config.toml` `[env]`；但普通 CLI 启动不会统一执行这套环境注入，`-vvv` 报告是单独的加载路径。`.env` 只支持单行 `KEY=VALUE`、以 `#` 开头的注释和引号，不支持 multiline 或 line continuation；不要用 `echo $env:OPENAI_API_KEY` 等方式验证凭据。

## 2. 按症状排查配置与模型

### missing API key / auth failure

先确认 shell、目标项目 `.env`、`$ANUREO_HOME/config.toml`（未设置时为 `~/.anureo/config.toml`）的优先级和 active provider。只检查“变量存在且来源正确”，不要打印值：

```powershell
anureo -vvv --working-folder . -m "只读检查配置来源、active provider 和凭据是否已配置；只报告存在/缺失，不输出值"
anureo models list
```

若是 401/403 或 `missing API key`，核对 `[default].provider`、对应 `[[providers]]` 的 `name`、`type` 和 model 选择；确认 `.env` 位于 effective working directory，而不是另一个 checkout。必要时用一个最小只读请求验证，但先设置明确日志文件并评估成本。

### model not found

运行 `anureo models list`，核对 model ID 是否存在、`provider/model` 的两段是否都非空，以及 provider 是否确实在配置中。裸 model 名称依赖默认 provider；可用显式 provider 缩小歧义：

```powershell
anureo models list
anureo --provider <provider> --model <model> -m "只读执行一个最小请求，确认 model/provider 路由"
```

`anureo models show <provider>` 会按 provider 过滤；若输出为空或报 `Provider not found`，核对 provider 名称与 `[[providers]]` 中的 `name` 一致。`models list` / `models show` 均输出该 provider 的完整模型清单（带总数），不截断。

### provider / tier conflict

`--model` 与 `--tier` 不能同时出现，CLI 会在 LLM 调用前失败。 `--provider` 可以和其中之一配合，但要检查 model 前缀是否又指定了另一个 provider：

```powershell
anureo --provider <provider> --tier standard -m "最小只读请求"
anureo --provider <provider> --model <model> -m "最小只读请求"
```

`light`、`standard`、`strong` 是当前允许的 tier。先移除一个覆盖项再重试，不要在同一失败任务上循环替换 model。

### rate limit、timeout 或 provider failure

先记录 provider 返回的短错误、`error class`、retry 次数和 log location。 `LlmError::Provider` 可能带有结构化 retry 判断；网络层的 timeout、connection reset、broken pipe、unexpected EOF、TLS/SSL 等可被 classifier 视为 retryable，而 `Cancelled` 不可重试。rate limit 仍需检查 quota、并发和 provider policy；不要把“可重试”理解为可以无限重跑。

```powershell
anureo --log-level debug --log-file .\.anureo\logs\diagnostic.log --model <provider>/<model> -m "只读执行一次最小请求"
```

## 3. 文件、修改位置、permission 与 MCP

### agent 找不到文件 / wrong modification location

先在同一 shell 中确认项目边界、Git 根目录和当前状态：

```powershell
Get-Location
git rev-parse --show-toplevel
git status --short
anureo --working-folder C:\path\to\project -m "只读确认当前 working directory、目标文件路径和项目边界；不要修改文件"
```

相对路径以 effective working directory 为准；`--working-folder` 只应指向明确授权的项目目录。若 agent 修改了错误 checkout 或错误文件，先保存证据，再看 `git diff`，不要用 session resume 代替文件回滚。

### tool denied

区分三类状态：permission prompt 被拒绝/取消、目标路径在项目边界外、MCP policy 或 shell policy 拒绝。记录 tool name、目标 path/command、working directory 和 permission scope；不要只记录“tool failed”。ACP 客户端的拒绝会让 tool call 结束为 denied/cancelled，不是 success。重新授权前确认目标和范围，优先使用 “Allow once”。

### MCP unavailable

先查看管理器发现的配置，再检查本次 run 的 override：

```powershell
anureo mcp list
anureo mcp show <name>
anureo --mcp-config .\.anureo\mcp.json -m "只读列出可用 MCP 工具；不要执行外部副作用"
```

按顺序核对 `--mcp-config`、profile、`ANUREO_MCP_CONFIG_PATH`、项目 `.anureo/mcp.json`、全局 `$ANUREO_HOME/mcp.json`，再检查 JSON、stdio command/args、URL、网络和 credential 是否可用。不要复制 MCP `env` 或 HTTP headers；disabled server 会被跳过。

## 4. Session、context、review 与 usage

### session not resumed / context seems wrong

恢复必须同时使用相同 session ID 和正确 project directory。先看摘要，不要立即读取完整 checkpoint：

```powershell
anureo session list --oneline
anureo session show <ID>
anureo --working-folder C:\path\to\project --session-id <ID> "只读确认当前项目、会话主题和待处理问题；不要修改文件"
```

`session show` 可帮助核对摘要、checkpoint 数量和首末消息；anureo 的 session 记录按 `thread_id` 索引，但当前 `SessionInfo` 不替你验证 project working folder。同一个 ID 在另一个项目中可能恢复旧上下文。必要时才用 `anureo session cat <ID>`，并只摘录 bounded summary；不要把完整对话、prompt 或秘密发给维护者。

### review / usage failure

review 或 usage 管理命令失败时，不要阻塞主任务或重复运行主模型请求。先记录命令、ID、storage path 的权限错误和日志，再检查：

```powershell
anureo review pending
anureo skill-usage show
```

确认 session/workflow 的实际结果后再重试管理命令；管理命令的失败不是主任务失败的唯一证据。若涉及 experimental review、skill usage 或 storage schema，必须在报告中标注实验性，并写出 unresolved issue，而不是声称已修复。

## 5. ACP / IDE 连接路径

### ACP 无法连接或 model mismatch

ACP 的 stdout 是 JSON-RPC transport，任何 debug/config 文本都会污染协议。先在 terminal 单独运行，并把诊断输出放到 stderr/log file：

```powershell
anureo acp --show-log-dir
anureo acp
```

`anureo acp --show-log-dir` 当前打印的是 ACP state/PID 目录：`$ANUREO_HOME/acp`（未设置 `ANUREO_HOME` 时通常为 `~/.anureo/acp`），不是日志文件目录。默认 ACP 日志文件是 `$ANUREO_HOME/logs/acp/anureo-acp.log`；`--log-file <PATH>`、`LOGS_ACP` 或 `config.toml` 的 `[logging.acp].path` 可以覆盖它。相对的自定义日志路径还可能按 ACP session 的 effective working folder 解析。`ANUREO_HOME` 会同时重定位这些 user-level 路径。

IDE 配置应使用 `anureo acp`，并核对 executable、working directory、model/provider config 与 terminal 一致。ACP `session_id` 一对一映射 anureo thread；切换项目要显式创建或 load 对应 session，不要跨项目复用同一个 ID。model mismatch 时同时检查项目 `.env`、`~/.anureo/config.toml`/`$ANUREO_HOME/config.toml` 和 session-level config。

### ACP permission / session restore

核对 IDE 声明的 file、terminal、MCP capabilities 及 `session/request_permission` 的 tool、target path、command、working directory 和授权范围。 `session/cancel` 后应是 `cancelled`，不是 `success`；连接关闭也可能是正常 EOF/connection-closed 路径。若 IDE 没显示完整错误，检查 `anureo acp --show-log-dir` 返回的目录与 ACP log；当前 server 使用 PID file `~/.anureo/acp/anureo-acp.pid`，重复 daemon 或 stale PID 也要核对。

ACP 默认日志文件为 `$ANUREO_HOME/logs/acp/anureo-acp.log`（未设置 `ANUREO_HOME` 时通常为 `~/.anureo/logs/acp/anureo-acp.log`）；`anureo acp --show-log-dir` 返回的是 `$ANUREO_HOME/acp`，其中包含 ACP 的 PID/state 文件。ACP 的 `--log-file <PATH>`、`LOGS_ACP` 和 `[logging.acp].path` 可覆盖默认日志路径。CLI 文件日志默认位置为 `$ANUREO_HOME/logs/cli/anureo-cli.log`。日志中只保留错误 class、时间、session/instance ID、model/provider 名称和短消息。

## 6. Workflow：bounded summary 优先

**实验性功能**：workflow interface 和 instance model 仍在演进；不可逆的 production operation 必须先在 isolated project 或 worktree 验证。

workflow 失败必须按下面顺序，且每一步只取所需信息：

```text
workflow_list(status_filter="failed")
→ workflow_status(instance)
→ workflow_events(instance, types=["agent_done", "run_done"], events_limit=...)
→ workflow_source(instance)
```

先使用 `workflow_status` 的 bounded summary：它包含 status、agent overview、phases、tokens、event statistics 和 bounded report preview。 `status=running` 时 sleep 后再 poll，不要 tight loop。只有 summary 指向具体阶段时，才用 `workflow_events`；它支持 `offset`、`events_limit`（1..=500）、`types` 和 `agent_id` 过滤。最后才用 `workflow_source` 查看 bounded Lua source preview。

不要同时读取所有 raw checkpoints、完整 events、agent outputs 和 source；这会扩大 context、泄露不必要内容并重复高成本任务。缺失 terminal event 不能单独证明 workflow 失败：以 status、有限 filtered events、日志和实际产物交叉确认。

检查 workflow-specific 的 working-directory lock、concurrency、model limit、script input 和 instance ownership。 `workflow_cancel` 只接受当前 process 所拥有的 running instance；取消后继续 `workflow_status`，直到 `cancelled` 或其他 terminal status。不能 resume 时修正 input/script 后 restart，不要假定从中断点继续。

## 7. 取消、重试与变更成本

第一次 `Ctrl+C` 请求 graceful cancellation；2 秒内第二次会 force quit，退出码为 130。取消不是 rollback：已写入的文件、Git index、外部副作用和 session checkpoint 可能保留。任何取消、permission denial 或半失败后，先运行：

```powershell
git status --short
git diff
anureo session list --oneline
```

修改任务可考虑 `--worktree`，但当前 CLI 只把选项传入 `RunOptions.worktree`，顶层路径未显示完整的创建、检查和清理实现，故标为**实验性**。使用前记录 branch、baseline 和现有 diff；使用后检查：

```powershell
git status --short
git diff
git worktree list
```

重试前评估：本次请求是否已产生 provider cost、重复写入或外部副作用？是否已有 session checkpoint/partial output？错误 class 是否 retryable？是否应先改 config、权限、working directory 或 script？只在最小、只读或明确授权的重试后，再扩大任务。

## 8. 脱敏报告与 unresolved issue

提交 issue 或内部报告时，提供：最小命令、version/commit、entry point、working directory、session/instance ID、model/provider、error class、短日志片段、status 和预期/实际行为。对日志和诊断信息做 secret redaction：替换 API key、token、cookie、authorization header、MCP env、完整 URL query、完整环境变量和私有源码；保留字段名、是否存在、错误码和非敏感路径结构即可。

实验性功能（workflow、`--worktree`、review/usage、ACP client capability 差异等）必须明确写 `实验性`，并记录：已验证的事实、尚未验证的假设、复现命令、影响范围、是否可安全重试，以及 `unresolved issue`。不要用 provider 内部 support 手册替代 anureo 侧证据，也不要提供自动修改代码的修复脚本。

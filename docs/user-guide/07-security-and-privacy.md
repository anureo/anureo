# anureo 安全与隐私：任务前检查与副作用处置

> **状态**：已实现；本文把当前 CLI、ACP 和工具源码中的权限边界整理为可执行流程。
> **适用范围**：真实代码仓库、含敏感数据的项目和团队环境。
> **相关代码**：`apps/cli/src/args.rs`、`apps/cli/src/main.rs`、`apps/acp/src/agent.rs`、`apps/acp/src/client_capabilities.rs`、`apps/acp/src/tools/fs_tools.rs`、`apps/acp/src/tools/terminal_executor.rs`

anureo 能读取和写入文件、启动 shell、连接 MCP、使用 browser，并保存 session、memory、skill 和 workflow 数据。把它当作一个有权限的 development tool 使用，而不是一个无副作用的聊天窗口。一次运行报告 `completed` 只表示这次 run 结束，不表示代码正确、业务结论正确、远端状态正确，或所有子进程和外部操作都已撤销。

本文的核心流程是：先确认项目边界，再逐项确认动作和授权，优先隔离执行，运行后审查实际变化，最后清理仍然有效的凭据、session、skill 和 workflow 数据。

## 1. 前置条件：确认边界和敏感数据

### 1.1 确认 working directory

在项目根目录启动，并明确本次任务允许访问的 project boundary：

```powershell
Get-Location
git status --short
anureo --working-folder . -m "只读检查当前项目结构，不修改文件"
```

`--working-folder DIR` 是 CLI 的 file-tool 工作目录；省略时使用当前目录。`apps/cli/src/args.rs` 将它定义为可选 `PathBuf`，因此不要假设 IDE、shell 或上一次 session 的当前目录就是目标项目。开始前检查：

- 绝对路径是否确实是目标仓库，而不是父目录、另一个 worktree 或含多个项目的目录。
- 任务中的相对路径是否都落在该目录内；任何绝对路径、父目录引用、共享目录和 mounted volume 都要单独确认。
- 仓库是否有未提交改动、未跟踪文件、submodule 或生成目录；它们可能是用户的工作，不要覆盖或清理。
- 文件内容是否包含 `.env`、config、token、个人数据或客户数据；“读取”也会把内容送入模型上下文和可能的日志。

ACP 中，IDE 传给 `session/new` 或 `session/load` 的 absolute `cwd` 会成为本次 `working_folder`。session 恢复并不会替你重新验证项目；切换项目时创建或明确加载对应 session，并再次核对返回的 `cwd`。ACP 的 `session/list` 当前不能把 `cwd` filter 当成隔离保证，因此客户端必须自己检查每条结果。

### 1.2 配置和凭据放置

anureo home 由 `$ANUREO_HOME` 决定；未设置时使用平台用户目录下的 `.anureo`（源码函数为 `anureo_home()`）。下文的 `{anureo_home}` 表示这个有效目录：session 数据位于 `{anureo_home}/thread/{session_id}/`，canonical 日志目录是 `{anureo_home}/logs/cli`、`{anureo_home}/logs/acp` 和 `{anureo_home}/logs/llm`。配置文件的位置相应是 `{anureo_home}/config.toml` 和 `{anureo_home}/mcp.json`；只有未设置 `$ANUREO_HOME` 时，才可把它们简写为默认的 `~/.anureo/config.toml` 和 `~/.anureo/mcp.json`。

日志目录是路径约定，不代表每个目录都会有文件：CLI 只有设置 `--log-file` 或 `LOG_FILE` 时才写日志，否则日志会被丢弃；ACP 有自己的配置/默认日志行为。请用实际的 `--log-file`、`LOG_FILE`、ACP 日志配置以及 `anureo acp --show-log-dir` 检查 effective log configuration 和实际文件。

项目 `.env` 会由 `foundation/config/src/dotenv.rs` 读取；这是一个 minimal `KEY=VALUE` parser，支持注释、空值、有限的单/双引号处理和双引号转义，但不支持多行/续行，也不是完整的 dotenv expansion。项目 `.env` 可覆盖配置中的同名值。建议把真实 credential 放在受访问控制的环境变量、未提交的项目 `.env` 或受控 secret manager 中，并在任务前确认它不会进入模型输入、命令行历史、shell 输出、MCP `args`、skill、memory、workflow source、普通 prompt 或日志。

明确禁止：

- 不要把 API key、password、cookie、OAuth token、私钥或生产连接串写进 prompt、memory、skill、workflow source、Git、issue 或共享日志。
- 不要用 `--debug-llm`（`args.rs` 说明它会把完整 system prompt 和 messages 写到 stderr）处理含 secret 的任务。
- 不要把真实 credential 放进 `mcp.json` 的 `command`、`args` 或 `env`。MCP environment 不是安全存储；HTTP headers 不能假定会自动脱敏。
- 不要把 provider 输出、路径、tool output 或诊断日志直接上传；分享前先脱敏并确认接收方、用途和保留时间。

## 2. 四要素授权确认

任何有影响的 tool call 都要把下面四项写清楚，并逐次确认：

| 要素 | 必须回答的问题 | 示例 |
| --- | --- | --- |
| `tool` | 哪个 tool 或 server 将执行？定义是什么？ | `write_file`、shell、某个 MCP server、ACP `fs_write_text_file` |
| `action` | 它具体读、写、执行、提交、删除还是发布什么？ | 修改一份 Markdown、运行测试、上传表单附件 |
| `target/scope` | 精确路径、URL、仓库、分支、账号和数据范围是什么？ | `docs/user-guide/07-security-and-privacy.md`、当前仓库、单个 issue |
| `duration` | 授权只对一次调用、当前 session、当前 project，还是截止何时？ | `once`、本次 run、30 分钟；默认选择一次 |

确认文本应接近：`允许 tool=<...> action=<...> target/scope=<...> duration=once；不允许其它路径、命令、上传、删除或发布。` 如果 tool、目标或时长发生变化，重新确认；“允许写文件”不等于允许 shell、安装、网络、删除或 publish。

调用前可先检查当前 registry 的定义：

```powershell
anureo tool list
anureo tool show <name> --output json
```

检查 `input_schema` 中的 path、command、URL、upload、delete、overwrite 等字段，以及 description 是否声明额外范围。definition 是审计材料，不是授权本身。

## 3. 按副作用分类的检查

### 3.1 文件写入、移动和覆盖

文件工具可能创建、覆盖、移动、patch 或删除文件。ACP 的 `fs_write_text_file` 接收 `path` 和完整 `content`，先尝试读取旧内容，再通过 IDE bridge 写入，并返回 diff；路径既可相对 workspace root，也可 absolute，IDE 可能显示 unsaved 状态或再次询问。因此要确认精确文件、是否覆盖、旧内容是否已备份，以及 IDE 中的 unsaved buffer 是否才是权威版本。

操作前：保存或记录 `git diff`，限制到列出的文件，优先使用 `--worktree`；操作后重新检查 diff、status 和文件内容。不要用“写入项目”作为宽泛授权。

### 3.2 shell、install 和 terminal

shell 是任意命令边界，不只是测试入口。它可能安装依赖、执行下载脚本、改环境、删除数据、启动持久进程、访问工作目录之外的文件、联网、commit、push 或 publish。ACP terminal capability 由 client 声明；`terminal_executor.rs` 在 Windows 通过 `powershell -NoProfile -Command` 执行，在 Unix 通过 `sh -c` 执行；timeout 只限制等待，不应被理解为撤销已经发生的副作用。超时可能返回 partial output 或杀掉 bridge terminal，外部进程的实际影响仍需检查。

批准前逐字查看 command、args、cwd、env、网络和恢复方式。安装类操作明确包源、版本、写入位置和是否会执行 post-install script；未知内容先 dry-run 或只读查询。CLI 的 `--dry` 只让 agent run 不执行 tools，不能把它当作 shell 工具自己的事务回滚。

### 3.3 network、MCP、browser form 和 upload

MCP 配置发现可由 `--mcp-config PATH` 覆盖，也会查找 working folder 下的 `.anureo/mcp.json` 和 `{anureo_home}/mcp.json`（未设置 `$ANUREO_HOME` 时，后者是默认的 `~/.anureo/mcp.json`）。先审查再启用：

```powershell
anureo mcp list
anureo mcp show <server>
anureo mcp add --name <server> --url https://example.invalid/endpoint --disabled
anureo mcp enable <server>
```

审查 server 的 `command`、全部 `args`、包来源、URL、headers、environment、TLS、账号权限、数据出境和工具 definition。`enable` 只是改 `disabled` 字段，不等于连通性检查；下一次 run 才会按配置加载。第三方 MCP 可能同时读写本地和远端资源。

browser、web fetcher 和 MCP HTTP 都可能把敏感内容发到外部。表单提交、登录、支付、创建 issue、发送消息和上传文件必须单独确认 site、URL、login state、submit action、file list、接收方和授权时长。即使任务描述为“只读”，网页内容可能变化，MCP server 也可能返回写入工具。未确认就不要 submit、upload、delete 或 publish。

### 3.4 delete、publish 和不可逆操作

删除 session、skill、文件、MCP entry、远端记录或 worktree，和发布包、提交代码、push、deploy、发送消息一样，必须每次显式确认，不能依赖永久 allow。先显示目标并记录恢复路径，再执行；`anureo mcp delete <name>` 只从配置移除 entry，不清理已安装包、远端资源或已产生的副作用。

## 4. CLI 与 ACP：确认机制不同

CLI 是命令行和 script 的基线入口。交互模式 `anureo -i -m "..."` 只表示可以继续对话，不自动提供每个高风险 tool 的人工批准；non-interactive 或没有确认机制时，不应让写入、shell install、外部提交、删除或发布静默执行。CLI 的 `--json` 是机器可读事件和结果输出，`--file PATH` 可把 JSON 写入文件；stdout 的结果不能替代人工审查。

ACP 通过 IDE 的 `session/request_permission` 请求权限。客户端能力解析中，未声明的 `fs_*` 和 MCP transport capabilities 默认是 `false`；能力过滤的 ACP extra tools（例如 `fs_read_text_file`、`fs_write_text_file` 及相应 MCP transport 工具）会被省略。这里不能据此断言 terminal 不可用：`apps/acp/src/agent.rs` 对 ACP run 仍会无条件安装 `LocalCommandExecutor`，因此 built-in/local command path 是否可用要按该具体路径检查，而不是只看客户端的 terminal flag。客户端 capability declaration 也不是 OS sandbox。IDE 拒绝或取消时，tool call 状态是 denied/cancelled，不是 success；客户端应把 tool、path/command、cwd、scope 和 duration 呈现给用户。ACP `session/cancel` 结束当前 run 的状态应为 `cancelled`，但 cancellation 不是 rollback。

**实验性边界**：ACP 的 capabilities 和 bridge 只说明客户端声明的能力与传输方式，不构成 OS sandbox；IDE 的确认 UI 也不保证已经撤销子进程、远端请求或已写入的数据。`anureo acp` 的 stdout 专供 JSON-RPC，诊断应写日志并用 `anureo acp --show-log-dir` 查找日志，不能把 debug 文本混入 stdout。

## 5. 隔离、审查和清理

### 5.1 使用 `--worktree` 并审查 retained worktree

修改任务优先使用：

```powershell
anureo --worktree --working-folder . -m "修改并测试此功能，完成后报告 diff"
```

`--worktree` 创建 isolated Git worktree；没有变化时清理，有变化时保留 worktree branch/directory 供 review。它隔离 Git 工作目录，不隔离 provider、MCP、browser、shell、安装、push、上传或其它外部副作用。完成或取消后，在原仓库和 retained worktree 中分别执行：

```powershell
git status --short
git diff --stat
git diff -- docs/user-guide/07-security-and-privacy.md
```

再检查 untracked files、生成物、依赖 lockfile、配置和是否有远端状态变化。只有确认 diff、测试、目标分支、作者和外部副作用后，才合并或删除 retained worktree；不要把 `--worktree` 当成自动批准或回滚。

### 5.2 completed 后的结果审查

至少分别验证：

1. `completed`/`success` 是否只代表 agent 或 protocol 生命周期结束，而不是业务验收。
2. 文件内容、Git diff、测试结果、构建结果和生成物是否符合任务；必要时由另一位负责人 review。
3. shell 是否启动了仍在运行的进程，terminal 是否因 timeout 留有 detached process。
4. MCP、browser、install、commit/push/publish 是否发生了已批准范围以外的动作。
5. 输出、日志、session checkpoint、memory、skill 和 workflow instance 是否包含 credential 或敏感数据。

不要只复制最终 reply；保留经过脱敏的 command、目标、授权、status、diff、验证命令和时间戳。

### 5.3 删除和可恢复性

删除前先确认精确 ID/name/path，并保存必要的脱敏审计记录：

```powershell
anureo session list
anureo session show <session-id>
anureo session delete <session-id>
anureo skills list
anureo skills delete <skill-name>
anureo memory list
```

`session delete` 删除 conversation data，但不删除 project files、memory 或 skills。skill 删除、memory 编辑和 workflow 清理是不同对象，不能用 session delete 代替；先查看命令的当前 definition 和实际文件，再决定是否备份。`ANUREO_HOME` 下的 session/thread、logs、memory/skill data，以及项目 `.anureo/` 中的配置和 workflow state，可能没有统一的 undo；Git 只可能恢复受 Git 管理的文件，不能恢复远端副作用、已泄露 credential 或所有 SQLite/checkpoint 数据。

**实验性说明**：具体 workflow instance 的存储和删除入口可能随 workflow 实现变化。不要按目录名猜测删除命令，也不要递归删除整个 `~/.anureo` 或项目 `.anureo/`；先列出 instance、确认是否有 export/backup/restore，再按该实现提供的 lifecycle command 清理，并验证其它 session、skill 和 memory 仍然存在。

## 6. 取消、错误和外部副作用的应急步骤

发生取消、tool error、意外写入、异常网络请求或疑似 credential 泄露时，按顺序处理：

1. 立即停止后续高风险调用；CLI 第一次 Ctrl+C 请求 graceful cancellation，第二次在短窗口内 force-quit；ACP 使用 `session/cancel`。两者都不是 rollback。
2. 保存 session ID、workflow instance ID、tool name、command、cwd、target、时间、日志位置和错误摘要；不要把 secret 原文复制到 issue 或 prompt。
3. 立即检查 `git status --short`、`git diff`、未跟踪文件、正在运行的 process/terminal、MCP 状态、browser 任务和远端活动；必要时冻结发布或撤销 token。
4. 若凭据出现在 prompt、log、memory、skill、workflow source、shell history、MCP env/header 或上传内容中，先 revoke/rotate，再清理所有副本，并通知凭据 owner；删除日志不能证明第三方已忘记数据。
5. 用备份或 Git 恢复受控的本地文件；逐项处理远端删除、消息、发布、push、安装和上传，确认是否可由 provider 侧撤销。记录“已撤销”“不可撤销”及负责人。
6. 最后检查 ACP/CLI 日志和输出是否脱敏，并在再次运行前缩小 working folder、tool scope 和 duration。若问题涉及安全缺陷，只通过项目指定的 private reporting channel 报告，不在普通 prompt 中披露利用细节。

## 7. 最小可执行清单

开始前逐项回答：

```text
[ ] working_folder、仓库、分支、未提交改动和敏感文件已确认
[ ] tool / action / target-scope / duration 已写明；高风险操作为一次性授权
[ ] MCP、browser、shell/install、写入、删除和 publish 的外部接收方已确认
[ ] credential 只在受控环境中，未进入 prompt、memory、skill、workflow source 或 logs
[ ] 修改任务已决定是否使用 --worktree，并知道 retained worktree 的审查路径
```

结束后逐项回答：

```text
[ ] completed 状态、实际 diff、Git status、测试和业务验收分别已验证
[ ] process/terminal、MCP、browser、远端和发布状态已检查
[ ] retained worktree、session、memory、skill、workflow instance 的保留或删除决定已记录
[ ] logs、outputs、checkpoint 和 exports 已脱敏；需要的 backup 仍可用
[ ] 取消、错误或凭据泄露时的 revoke/rotate、恢复和通知已完成
```

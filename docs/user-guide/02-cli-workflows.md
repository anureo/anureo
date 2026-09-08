# CLI 工作流

本文面向已经完成首次运行、需要持续用 anureo CLI 完成开发任务的开发者。示例默认在目标项目根目录执行；如果使用 `--working-folder`，请把它替换成你明确授权 anureo 访问的项目目录。

## 前置条件

先确认命令可用、当前目录是正确的 Git 项目，并准备好模型提供方配置。模型可以是裸名称，也可以是 `provider/model`；提供方来自 `~/.anureo`（或 `ANUREO_HOME` 指定目录）的 `config.toml` 中的 `[[providers]]`。项目目录中的 `.env` 会覆盖配置文件 `[env]` 注入的环境变量。

```powershell
anureo --help
git rev-parse --show-toplevel
anureo models list
```

`--working-folder DIR`（短参数 `-w`）只改变文件工具和项目发现所使用的目录；不指定时使用当前目录。每次执行前都应确认项目边界，尤其不要在另一个项目目录中误恢复旧会话。

## 选择运行模式

四种模式的用户可见差异如下：

| 模式 | 用户可见行为 | 适合的任务 |
| --- | --- | --- |
| `react`（默认） | 直接进入常规 agent 运行；工具调用和回答按普通开发回合推进 | 日常调查、实现、测试和小型修复 |
| `dup` | 先进行 deep-understanding 阶段，再执行任务 | 需要先建立较完整上下文、跨文件分析的任务 |
| `tot` | 使用 multiple reasoning paths，再汇总/继续执行 | 需要比较多种方案或排查不确定根因 |
| `got` | 使用 adaptive graph planning；`got --got-adaptive` 显式打开自适应模式 | 依赖关系复杂、需要分解成图状计划的任务 |

模式是子命令；顶层运行选项必须放在子命令之前。消息可用 `-m/--message` 或位置参数传入：

```powershell
anureo -m "调查登录失败的调用链，并指出最可能的根因"
anureo -m "分析这个跨模块重构的影响面，先给出执行计划" dup
anureo -m "比较三种缓存失效方案，并说明测试证据" tot
anureo -m "分解并执行这个多阶段迁移任务" got --got-adaptive
```

省略子命令时使用默认的 `react` 模式；`got --got-adaptive` 中的 `--got-adaptive` 是 `got` 子命令自己的选项，因此放在 `got` 之后。

不要把 `goal`、`task`、`curator`、实验性 skill/review 管理命令当作本文的稳定日常路径；它们不属于本指南覆盖范围。

## 选择模型、提供方、agent 和 effort

`--model MODEL`（短参数 `-M`）选择具体模型，例如 `gpt-4o` 或 `provider/model`。`--provider PROVIDER` 选择配置中的提供方；显式 `--provider` 优先于 `--model` 中的 provider 前缀。只给 `--provider` 时，anureo 会从该提供方补齐 API key、base URL 和 provider type。

`--tier TIER` 将等级传给运行配置，等级严格为 `light`、`standard`、`strong`（大小写不敏感）。`--model` 和 `--tier` 不能同时指定，CLI 会在执行前报错；`--provider` 可以与其中任一项配合使用。

`--agent NAME`（短参数 `-P`）加载命名 profile：优先使用项目 `.anureo/agents/NAME`，也可使用 `~/.anureo/agents/NAME`。profile 可能提供模型、工作目录和工具设置；显式 CLI 参数用于覆盖可覆盖的 profile 设置。

已核实的当前实现限制：CLI 运行路径会先把配置默认模型写入 `RunOptions.model`。因此只给 `--tier` 时，等级虽然会被传入，但默认模型仍然存在，不能保证按等级重新选择模型；带有模型的 agent profile 也可能因该默认模型已存在而不生效。显式 `--model` 会使 profile 中的模型选择被跳过；它与 `--tier` 不能同时使用。若依赖 tier 或 profile 中的模型，请先用实际配置和 `--dry`/日志验证最终模型；这里不把“tier 选择最佳模型”作为已实现保证。

`--effort LEVEL` 把 reasoning effort 原样传给模型客户端。支持的常用值为 `auto`、`none`、`minimal`、`low`、`medium`、`high`、`xhigh`；`auto` 表示使用模型默认值，未指定则不发送该参数。未知值可能由 provider 拒绝或警告，不要把它当作跨 provider 通用值。

```powershell
anureo --tier strong --effort high -m "定位这个复杂并发失败"
anureo --model openai/gpt-4o --provider openai --effort medium -m "解释该模块并列出验证步骤"
anureo --agent coding --tier standard -m "实现这个小功能并运行相关测试"
```

下面的组合会在 LLM 调用前失败：

```powershell
anureo --model gpt-4o --tier strong -m "不要执行"
```

## 交互会话与会话管理

一次运行会确保有 `thread_id`；若未给 `--session-id`（短参数 `-s`），CLI 会生成一个并在普通输出中打印。用 `-i/--interactive` 可在首个回合后继续输入：

```powershell
anureo -i -m "调查这个失败测试；先只收集证据"
anureo --session-id <ID> "继续上次调查，补充最小复现"
```

也可以只进入 REPL，再逐回合输入：

```powershell
anureo -i
```

稳定的会话管理命令是：

```powershell
anureo session list
anureo session show <ID>
anureo session rename <ID> "登录失败调查"
anureo session cat <ID>
anureo session delete <ID>
```

`session list` 可选 `--limit`、`--since`、`--until`、`--reverse`、`--oneline`、`--no-pager`、`--grep` 和 `--format`；日期接受 `YYYY-MM-DD` 或 RFC 3339。`show` 展示摘要、checkpoint 数量和首条/末条消息；`cat` 读取会话内容；`delete` 会删除目标 checkpoint，并按共享摘要清理相关 delegate checkpoint，**不会删除项目文件**。

会话数据库使用默认的 anureo memory DB 路径，session 记录按 `thread_id` 索引；当前 `SessionInfo` 没有项目 working folder 字段。因此会话恢复并不会替你验证项目身份：同一个 ID 在另一项目中可能读到旧上下文，并让模型把旧项目事实误用于新项目。恢复前先用 `session show`/`cat` 检查首条消息和主题，并显式指定正确目录：

```powershell
anureo --working-folder C:\work\acme-api --session-id <ID> "仅在 acme-api 中继续"
```

不要把“取消当前回合”误认为“回滚会话”；会话的检查点和工作区文件需要分别核查。

## JSON 输出、文件和时间戳

`--json` 将运行事件和最终 reply 写成 JSON；运行事件按一行一个 JSON 值输出，最终 reply 也作为一行记录。`--file PATH` 把 JSON 输出追加/写入文件而不是 stdout，适合让 stdout 保持干净；`--pretty` 使用多行 JSON，便于人工阅读但不适合作为严格的逐行流解析输入。

```powershell
New-Item -ItemType Directory -Force .anureo\results | Out-Null
anureo --json -m "调查测试结构" > run.ndjson
anureo --json --file .anureo\results\investigation.ndjson -m "调查测试结构"
anureo --json --pretty --file .anureo\results\readable.json -m "汇总验证结果"
```

机器消费建议：把 `--json` 的 stdout 或 `--file` 内容当作事件流，逐行解析；日志不要混入 stdout。`--json` 也适用于支持 JSON 的管理命令，例如 `anureo --json session list`。

`--timestamp` 已实现为在流式文本 reply 开始前向 stderr 打印本地时间。当前活动的流式事件处理路径会使用它；限制是运行结束后的 `output.rs` 文本 emitter fallback 目前为空操作。因此普通流式文本输出可以使用该选项，若依赖 post-run fallback 的场景则应标注为未覆盖。

## 日志和诊断

`-v/--verbose` 是计数参数：`-v` 增加 skill 列表和运行步骤信息，`-vv` 进一步展开工具/skill 的 source、description 和 toolset requirements，`-vvv` 与 `-vv` 相同。日志控制项为：

```powershell
anureo -v -m "调查失败测试"
anureo -vv --log-level debug --log-file .anureo\logs\cli.log -m "调查失败测试"
anureo --log-file .anureo\logs\cli.json --log-format json --log-rotate daily -m "运行验证"
```

`--log-level` 使用 tracing `EnvFilter` 语法并覆盖 `RUST_LOG`；`--log-format` 为 `text`（默认）或 `json`；`--log-rotate` 为 `none`、`daily`（默认）、`hourly`、`minutely`。日志文件路径的实际优先级是：CLI `--log-file` > `config.toml` 的 `[logging.cli].path` > shell 的 `LOG_FILE` > 默认文件 `~/.anureo/logs/cli/anureo-cli.log`。路径支持 `{working_folder}` 替换。`--log-rotate` 作用于解析出的日志文件，包括默认文件；不指定 `--log-file` 并不意味着写入 sink。

## 修改任务的隔离与 Git 检查

`--worktree` 的接口说明是“在隔离 Git worktree 中运行，若无修改则清理，有修改则保留供检查”。但在当前 CLI 源码中它只是传入 `RunOptions.worktree`；指定的顶层 CLI 运行路径没有展示完整的 Git 创建、变更检查和清理实现。故此选项目前标记为**实验性**：不要仅凭命令成功就认为修改已隔离或可恢复。

```powershell
anureo --worktree -m "升级依赖，运行测试，并说明所有变更"
```

使用前必须满足并手动核查：当前目录确实是 Git 仓库；工作树基线、分支和未提交修改已记录；执行后用 `git status --short`、`git diff` 和 `git worktree list` 检查实际落点。若实现创建了 worktree，只有 Git 检查确认无变更时才可接受自动清理；发现变更时应保留 worktree/分支供 review 和 merge。不要把未提交修改带入一次你不理解生命周期的实验性运行。

## 取消与安全边界

运行中第一次按 `Ctrl+C` 会调用 cancellation token，请求 graceful cancellation；第二次在 2 秒内按下会以退出码 130 force quit。交互 REPL 等待输入时也监听 force-quit 通知。

取消只表示停止当前运行，不是 rollback：工具已经写入的文件、Git index、外部服务副作用和已经保存的 session checkpoint 都可能保留。取消后始终检查：

```powershell
git status --short
git diff
anureo session list
```

需要恢复时先用 `session show`/`session cat` 确认 ID 和项目，再决定是否手动撤销文件；不要用 `session delete` 代替 Git 回滚。

## 可复制流程：调查任务

```powershell
cd C:\work\acme-api
git status --short
New-Item -ItemType Directory -Force .anureo\results, .anureo\logs | Out-Null
anureo --tier standard --effort medium --json --file .anureo\results\investigation.ndjson --log-file .anureo\logs\investigation.log -m "调查最近的认证失败：只读代码、配置和测试，定位根因，列出证据、涉及文件和建议的最小修复；不要修改文件"
anureo session list --oneline
anureo session show <ID>
```

如果需要继续调查，使用同一目录和已核对的 ID：

```powershell
anureo --working-folder C:\work\acme-api --session-id <ID> -i "基于上次证据补充最小复现和测试入口"
```

## 可复制流程：修改并测试

先保存基线，再让 anureo 明确修改范围和验证命令：

```powershell
cd C:\work\acme-api
git status --short
git branch --show-current
anureo --agent coding --tier standard --effort high --worktree -m "修复认证失败。只修改必要文件；实现后运行相关单元测试和集成测试，报告命令、结果和未解决问题"
git status --short
git diff
git worktree list
```

由于 `--worktree` 当前标为实验性，审阅实际 `git diff`/worktree 后再合并。若不需要隔离，应省略该选项并在独立分支执行；无论是否取消，都要运行测试并检查 Git 状态。完成后用会话 ID 保留审计线索：

```powershell
anureo session list --oneline
anureo session cat <ID>
```

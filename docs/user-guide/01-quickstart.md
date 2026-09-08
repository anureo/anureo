# anureo Quickstart

> **状态**：本地 CLI 快速路径；配置加载部分存在当前实现限制（见第 2 节）

本页只覆盖第一次在本地项目中完成一次可验证任务所需的路径。anureo 是 local-first AI Agent runtime：它从 CLI 启动 agent，在受控的项目上下文中调用工具、维护 session，并把结果交回开发者。CLI 的入口名是 `anureo`；当前 workspace 中对应的 binary crate 是 `cli`。

## 1. 前提与构建

你需要一个可用的 Rust/Cargo 环境、anureo 源码 checkout，以及目标模型 provider 的凭据。当前仓库提供的构建方式是：

```powershell
cargo build -p anureo-cli
```

在源码 checkout 中，下面两种入口传递相同的 CLI 参数，都是启动 `cli` crate 的 `anureo` binary，并默认运行 ReAct：

```powershell
cargo run -p anureo-cli -- -m "列出这个项目的测试入口，并说明如何验证它们"
anureo -m "列出这个项目的测试入口，并说明如何验证它们"
```

第二种写法要求已经构建并把 `anureo` 放在 `PATH` 中；本项目源码直接提供的、无需额外安装步骤的入口是第一种 `cargo run -p anureo-cli --`。

## 2. 配置模型凭据

从项目根目录开始，把示例复制成项目级 `.env`，再填入真实值：

```powershell
Copy-Item .env.example .env
```

`.env.example` 当前示例包含 OpenAI Chat 配置，以及可选的 embedding 和 Exa MCP 配置。对于 anureo CLI 的正常运行，配置变量名应使用 `OPENAI_API_KEY`、`OPENAI_BASE_URL` 和 `MODEL`；`OPENAI_API_BASE`、`OPENAI_MODEL` 不是配置加载器为 CLI 解析的等价名称。不要把真实 key 提交到 Git、prompt、session 或日志。

注意：当前仓库的 `.env.example` 仍写着旧变量名。复制后请在 `.env` 中把这两行改为下面的名称（或直接手动创建这些行）：

```dotenv
OPENAI_BASE_URL=https://api.openai.com/v1
MODEL=gpt-4o-mini
```

**当前实现限制（已由源码核对）**：普通 CLI 启动路径目前不会在构建 `RunOptions` 前调用 `config::load_and_apply`；只有使用至少三个 `-v`（`-vvv`）时，`print_config_report` 才会触发配置加载。因此，仅复制 `.env.example` 为 `.env` 并填写值，不能保证普通 `cargo run -p anureo-cli -- ...` 读取项目 `.env` 或用户 `config.toml`。在该启动行为修复前，请把下面的规范变量直接放入进程环境，或使用第 3 节的 `-vvv` 配置检查路径：

```powershell
$env:OPENAI_API_KEY = "your-api-key-here"
$env:OPENAI_BASE_URL = "https://api.openai.com/v1"
$env:MODEL = "gpt-4o-mini"
```

anureo 还会读取用户级配置：默认是 `~/.anureo/config.toml`；设置 `ANUREO_HOME` 后，文件位置变为 `$ANUREO_HOME/config.toml`。该 TOML 的 `[env]` 表会提供环境变量，`[[providers]]` 可定义 provider。基本关系如下：

```text
进程环境变量  >  项目工作目录/.env  >  选中的 [[providers]]  >  ~/.anureo/config.toml（或 ANUREO_HOME/config.toml）的 [env]
```

配置加载器按启动命令的进程当前目录查找项目 `.env`；它当前不会把 `--working-folder` 传给 dotenv 查找。因此，使用项目 `.env` 时必须从该项目目录启动命令；`--working-folder` 只定义 agent 文件工具使用的工作目录，并不会改变 `.env` 的发现位置。`.env` 中的值覆盖 `config.toml`，但已经存在的进程环境变量优先级更高。`config.toml` 的示例结构见 `foundation/config/examples/config.toml.example`。凭据只保存在其中一个受控位置即可，不要在多个位置放互相冲突的值。

## 3. 先确认边界、模型和工具权限

anureo 的文件工具以 effective working directory 为项目边界。`--working-folder DIR` 指定文件工具使用的目录；省略它时使用当前目录。先进入你明确要调查的项目根目录，或显式指定它：

```powershell
Set-Location C:\path\to\your-project
anureo --working-folder . -m "只读调查：列出测试入口，不修改任何文件"
```

首次运行前逐项确认：

1. `Get-Location`（或 `pwd`）显示的是预期项目根目录；`--working-folder` 没有指向包含不应暴露给 agent 的更大目录。
2. 模型和 provider 已按预期生效。可用 `anureo models list` 查看已配置 provider 的模型；也可在试运行时显式指定 `--model MODEL`，例如 `--model gpt-4o-mini`。`-M` 是 `--model` 的短写。
3. 工具权限符合任务影响范围。用 `anureo tool list` 查看本次 CLI 加载的工具定义，并检查任何 permission prompt、shell command、文件写入、网络/MCP 操作；不要只因为 agent 报告“允许”就跳过确认。

若要查看并触发 anureo 实际加载到的 `.env`、`config.toml` 和配置摘要，使用至少三个 `-v`；这也是当前 CLI 中会执行配置加载的路径：

```powershell
anureo -vvv --working-folder . -m "只读调查：说明当前项目结构"
```

该报告会把 secret 值遮蔽；仍然不要复制完整环境变量到公开日志。注意：`-vvv` 会改变当前行为，不能把它当作普通启动路径已经自动加载 `.env` 的证明。模型的最终结果、工具权限和项目边界都需要由开发者确认。

## 4. 第一次运行：默认 ReAct

没有写子命令时，CLI 默认是 `react`。`-m/--message` 接收一条 user message；也可以把消息作为位置参数传入。`-m/--message` 优先于位置参数，位置参数的多个 token 会按空格拼接：

```powershell
# 推荐：从项目根目录执行一次只读任务
cargo run -p anureo-cli -- --working-folder . -m "调查这个项目的测试入口，列出命令和预期验证结果"

# 等价的位置参数写法
cargo run -p anureo-cli -- --working-folder . "调查这个项目的测试入口，列出命令和预期验证结果"
```

这里的 `--` 是 Cargo 与 anureo CLI 的分隔符；它之后的参数才传给 `anureo`。默认 ReAct 会根据 message 选择并调用可用工具，再输出本次任务结果。若没有 `-m` 且没有位置参数，CLI 不会得到 user message，不能作为本页的首次任务路径。

## 5. 只读调查与修改任务

只读调查的目标是读取、搜索、解释或列出信息；prompt 应明确写出“不修改文件”，并在结束后检查 `git status` 和关键输出。例如：

```powershell
cargo run -p anureo-cli -- --working-folder . -m "只读调查：找出失败测试的入口并解释原因，不修改文件"
git status --short
```

修改任务会产生更高影响：它可能写文件、运行 shell 命令或触发网络/MCP 工具。执行前再次确认边界和权限；需要修改时优先使用隔离的 Git worktree：

```powershell
cargo run -p anureo-cli -- --working-folder . --worktree -m "修复一个明确的测试失败，并运行相关测试"
```

`--worktree` 会在隔离 worktree 中运行；无改动时清理，有改动时保留以便 review。它不是自动审查或自动合并，也不替代开发者对 diff、命令和外部副作用的授权。以下内容不属于本 quickstart：ACP/IDE 配置、Lua workflow DSL、experimental task/goal/curator/memory-v2/vector-store，以及生产环境无人值守自动化。

## 6. 验证结果

anureo 的任务完成只表示一次运行结束，不表示代码、数据或结论正确。开发者必须自行验证：

1. 复查 stdout/stderr 中的结论、引用的路径和实际工具调用。
2. 只读任务：手动打开相关文件，或运行与调查结论相符的检查命令。
3. 修改任务：查看 `git status`、`git diff`，运行相关测试，并确认没有越过项目边界的文件变化。
4. 若结果不可信，记录 command、effective working directory、session ID、model/provider 和错误信息；不要记录 API key。

完成这些检查后，才把结果当作本地项目中的可验证任务结果。

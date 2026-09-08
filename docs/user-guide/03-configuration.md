# anureo 配置

> **状态**：已实现；本文按当前源码说明配置加载和 CLI 覆盖行为。

本文面向需要接入或切换 LLM provider、维护团队/项目配置的 anureo 开发者。示例使用 PowerShell；`anureo` 也可以替换为 `cargo run -p anureo-cli --`（Cargo 参数前的 `--` 是分隔符）。

## 1. 前提与配置位置

需要 Rust/Cargo、anureo checkout，以及目标 provider 的凭据。先在目标项目根目录执行：

```powershell
cargo build -p anureo-cli
anureo models list
```

anureo 的用户级目录是 `$ANUREO_HOME`；未设置时，Windows 使用 `%USERPROFILE%\.anureo`，Unix 使用 `$HOME/.anureo`。因此本文把默认路径写作 `~/.anureo`。配置文件是：

```text
$ANUREO_HOME/config.toml       # 默认 ~/.anureo/config.toml
$ANUREO_HOME/mcp.json          # 默认 MCP 配置
项目根目录/.anureo/mcp.json    # 项目级 MCP 配置
项目根目录/.env              # 项目环境变量
```

`config.toml` 没有项目级变体：项目 `.anureo` 目录当前用于 MCP 和 agent profile 等项目数据；不要把 `config.toml` 放在项目 `.anureo` 目录并期待它被读取。

## 2. 实际加载路径与优先级

配置 crate 提供读取 `$ANUREO_HOME/config.toml`（文件缺失按空配置处理）和项目 `.env` 的 API，但普通 CLI 启动路径不会统一调用环境加载器。当前运行主要通过 `load_full_config`、`ReactBuildConfig::from_env` 等路径读取部分配置；`-vvv` 的配置报告才会在报告路径中调用 `load_and_apply_with_report("anureo", None)`。因此，不能把下述环境注入优先级理解为普通 CLI 每次启动都会自动执行的步骤。

如果要让 `[env]`、active provider 派生变量和项目 `.env` 通过配置 crate 注入当前进程环境，应在运行前显式调用 `load_and_apply_with_report("anureo", effective_working_folder)`；其中 `effective_working_folder` 必须由调用方传入。仅使用普通 `anureo` CLI 参数并不会自动完成这一步。

对同一个环境变量，优先级从高到低是：

```text
shell environment（进程启动时已存在）
  > 项目 .env
  > config.toml 中被 [default].provider 选中的 [[providers]]
  > config.toml 的 [env]
```

加载器只在变量尚未存在于进程环境时写入值，所以 shell 中已有的值不会被 `.env` 或 `config.toml` 改写。项目 `.env` 的格式是每行 `KEY=VALUE`；支持以 `#` 开头的注释、单引号/双引号和 `\"`，不支持 multiline 或 line continuation。`.env` 不存在不是错误。

`config.toml` 的 `[env]` 值会作为环境变量注入；它不是 shell 脚本，也不做 `${NAME}`、`$NAME` 或其他环境变量插值。下面的 `your-key` 只是占位文本，必须替换为真实值。`[[providers]]` 只有在 `[default].provider` 指向它时，才会参与通用环境注入；provider 解析也可以直接按名称选择它。

可用 `-vvv` 查看配置报告路径、active provider 和脱敏后的配置摘要。注意：当前报告实现调用加载器时没有传入 CLI 的 `--working-folder`，因此报告中的 `.env` 路径按进程当前目录解析；这不等同于普通运行目录的自动配置加载：

```powershell
anureo -vvv --working-folder . -m "只读检查当前项目配置"
```

摘要会遮蔽看起来像 key、token、secret、password、credential 的值；不要因此把完整环境变量复制到 issue 或日志。

## 3. 配置 provider 与凭据

最小用户配置可以只写 `[env]`：

```toml
# ~/.anureo/config.toml 或 $ANUREO_HOME/config.toml
[env]
OPENAI_API_KEY = "replace-with-a-secret"
OPENAI_BASE_URL = "https://api.openai.com/v1"
MODEL = "gpt-4o"
```

需要多个 provider 时，使用 `[[providers]]` 和 `[default].provider`：

```toml
[default]
provider = "openai"

[[providers]]
name = "openai"
api_key = "replace-with-a-secret"
base_url = "https://api.openai.com/v1"
model = "gpt-4o"
type = "openai"
fetch_models = false

[[providers]]
name = "local"
api_key = "none"
base_url = "http://localhost:11434/v1"
model = "llama3.2"
type = "openai_compat"
```

provider 字段含义如下：`name` 是选择时使用的名称；`api_key` 映射到 `OPENAI_API_KEY`；`base_url` 映射到 `OPENAI_BASE_URL`；`model` 映射到 `MODEL`；`type` 反序列化并保存在 `ProviderConfig` 的 `provider_type` 中，供运行时解析使用，不应直接理解为通用环境注入的 `LLM_PROVIDER` 映射；`temperature` 映射到 `OPENAI_TEMPERATURE`。这些 provider 派生环境变量只有在调用配置 crate 的环境加载 API 时才会注入。`fetch_models = true` 时会尝试从 `{base_url}/models` 获取模型；否则使用 models.dev catalog。缺少 `base_url` 时，部分 provider 还可能从 models.dev 的 provider API 字段补齐地址。

建议凭据只放在权限受控的 `~/.anureo/config.toml`、`$ANUREO_HOME/config.toml` 或未提交的项目 `.env` 之一，并把 `.env` 加入项目的 ignore 规则。不要把 secret 放进 prompt、memory、skill、workflow source 或 logs；也不要用 `--debug-llm` 处理含秘密的任务，因为它会输出完整 prompt/messages。

## 4. 选择 provider、model 与 tier

这三个概念的边界是：

| 概念 | 含义 | CLI 形式 |
| --- | --- | --- |
| provider | API 厂商/兼容端点的命名配置，携带 key、base URL 和类型 | `--provider NAME` |
| model | 要调用的具体模型 ID，可写裸名称或 `provider/model` | `-M/--model MODEL` |
| tier | `light`、`standard`、`strong` 三档抽象选择，由 resolver 选出具体 model | `--tier TIER` |

CLI 覆盖规则是：`--provider` 优先于 `--model` 中的 provider 前缀；若 `--model` 是裸名称且同时给了 `--provider`，运行配置会组合成 `provider/model`。`--model` 和 `--tier` 互斥，二者同时出现会在 LLM 调用前失败：

```powershell
anureo --provider openai --model gpt-4o -m "解释这个项目的测试入口"
anureo --model openai/gpt-4o -m "解释这个项目的测试入口"
anureo --provider openai --tier standard -m "调查一个普通复杂度的问题"
```

`--tier` 只接受 `light`、`standard`、`strong`，大小写不敏感。tier resolver 的顺序是 embedded tier plan → models.dev spec → provider API（仅 provider 的 `fetch_models` 开启时）→ 失败；provider 的 `enable_tier_resolution = false` 会关闭该 provider 的 tier 解析。显式 model 会跳过 profile 的 tier 配置；CLI tier 又优先于 profile tier。若没有 CLI 覆盖，默认 model 依次考虑进程环境中的 `MODEL`、默认 provider 的 `model`、名称含 `coding-plan` 的 provider、首个有 model 的 provider，最后是 `gpt-4o-mini`。

## 5. 列出和命名模型

先列出当前配置 provider 可见的模型：

```powershell
anureo models list
anureo models show openai
```

当前 CLI 接受 `show PROVIDER` 参数，但 handler 仍调用与 `list` 相同的列表函数并传入空 provider filter；因此 `show openai` 目前可能仍输出全部模型。这是当前实现行为，不要把它当作 provider 过滤已生效。

模型命名优先使用 `provider/model`，例如：

```text
openai/gpt-4o
zhipuai-coding-plan/glm-5.1
```

裸名称（如 `gpt-4o`）依赖默认 provider 或显式 `--provider`。`provider/model` 中两段都不能为空；如果模型不在 registry，anureo 会再尝试按斜杠拆分并从 `[[providers]]` 取得该 provider 的连接字段，但 provider 不存在时不会凭空创建配置。

## 6. MCP 配置

agent 运行时的 MCP 路径优先级是：

```text
--mcp-config PATH
  > agent profile 中的 MCP config
  > ANUREO_MCP_CONFIG_PATH
  > 项目/.anureo/mcp.json
  > $ANUREO_HOME/mcp.json
```

`--mcp-config` 会传入本次 run；如果给出的 override 文件不存在，底层 discovery 会继续尝试项目和全局路径。可复制的 stdio 配置如下：

```json
{
  "mcpServers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "C:/work/project"],
      "env": {"MCP_TOKEN": "replace-with-a-secret"}
    }
  }
}
```

远程 MCP 使用 `url`（只接受 `http://` 或 `https://`），可附加 `headers` 和 `oauth`；同一条目同时有 `url` 与 `command` 时 `url` 胜出；`disabled: true` 的条目会被跳过。

```powershell
anureo --mcp-config .\.anureo\readonly-mcp.json -m "列出可用 MCP 工具，不修改文件"
anureo mcp list
anureo mcp show filesystem
anureo mcp add --name docs --url https://mcp.example.com/sse
anureo mcp disable docs
```

`anureo mcp list/show/add/edit/delete/enable/disable` 管理器自身按当前目录的项目 `.anureo/mcp.json` 优先、其次全局 `mcp.json`；没有文件时会创建全局文件。管理命令不会把 `--mcp-config` 作为参数传给 manager。

## 7. 诊断入口

先记录 anureo 版本/commit、命令、effective working directory、session/instance ID、model/provider 和错误分类，但不要记录 key、cookie、完整环境变量或私有源码。

| 症状 | 入口 |
| --- | --- |
| provider auth、missing API key、401/403 | 检查 shell、项目 `.env`、`config.toml` 的优先级；确认 `[default].provider`、`api_key`、`base_url` 和 `type`。不要用 `echo` 打印 key。 |
| model 不存在 | 运行 `anureo models list`；核对 provider 名称、`provider/model` 格式和 `[[providers]]`；必要时先用 `--provider` 明确 provider。 |
| model/provider/tier 冲突 | 移除 `--model` 或 `--tier` 之一；若 model 已含 provider 前缀，检查是否又传了不同的 `--provider`。 |
| 限流、请求失败或 retry 异常 | 检查 provider 返回的错误分类、quota、网络和 retry 相关日志；避免盲目重复高成本任务。使用 `--log-level debug` 和明确的 `--log-file` 收集脱敏日志。 |
| MCP 不可用 | 检查 `--mcp-config`、`ANUREO_MCP_CONFIG_PATH`、项目/全局路径、JSON 格式、command/URL、网络和 MCP 凭据；用 `anureo mcp list` 查看管理器发现的文件。 |

可用的诊断命令示例：

```powershell
anureo -vvv --working-folder . -m "只读报告当前配置来源和模型选择，不输出秘密"
anureo --log-level debug --log-file .\.anureo\logs\diagnostic.log -M openai/gpt-4o -m "执行一个最小只读请求"
```

日志内容仍可能包含请求元数据或错误上下文；提交前应脱敏。更完整的症状分类和项目边界排查见 [`docs/guides/troubleshooting.md`](../guides/troubleshooting.md)。

## 8. 安全清单

- 把真实凭据放在受控的 `config.toml` 或本地 `.env`，不要提交到 Git。
- 不要把 secrets 放进 prompt、memory、skill、workflow source 或 logs；不要把 MCP `env`/HTTP `headers` 中的 token 复制到文档或 issue。
- 使用 `-vvv`、`--log-file` 或 `--debug-llm` 前先确认输出位置和内容；`-vvv` 的 config summary 会遮蔽常见 secret key，但不是任意业务字段的保密边界。
- 配置变更后用 `git status --short` 检查项目 `.env`、`.anureo` 和日志目录，确认没有意外纳入版本控制。

**不确定性与边界**：本文未展开 workflow、memory-v2、vector-store 或第三方 provider 的完整 API；这些不属于本配置路径的稳定基础。`--worktree` 是已支持的隔离运行选项，会创建并清理临时 worktree；使用它执行修改任务时仍应检查最终 diff。ACP 客户端配置 UI 的产品状态不在本文所核对的 CLI/配置源码范围内，不能仅据此标为实验性；无论使用哪种入口，都应人工核验配置来源、凭据和实际 diff。

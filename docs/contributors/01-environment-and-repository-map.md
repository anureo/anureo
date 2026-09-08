# anureo 环境与仓库地图

本文面向 anureo 贡献者。内容以当前仓库的 [`README.md`](../../README.md)、workspace manifest、`Cargo.lock`、`.env.example`、[`foundation/config` 文档](../../foundation/config/README.md)与源码，以及四个应用/示例 manifest 为依据；未在这些文件中出现的能力不在本文中当作已实现 API。

## 1. 项目定位与当前状态

根 README 将 anureo 定义为 local-first AI Agent runtime：Agent 可以从 CLI、IDE 的 ACP（Agent Client Protocol）和 messaging bot 进入，工具调用、session、memory、skills 与 workflows 都保持在可控的 project context 中。目标是让任务持续、可解释地完成，而不是替代 code review 或让 Agent 无人看管地修改系统。

当前 workspace/package 版本是 `0.5.0`，edition 是 Rust `2021`，license 是 MIT。`Cargo.lock` 使用 lockfile version `4`；依赖版本应以它记录的精确解析结果为准，而不是只看 manifest 中的宽版本约束。锁文件同时包含 `agent-client-protocol` `0.14.0` 和 `0.15.1`；ACP crate 的直接依赖是 `0.15.1`，不要因为锁文件中存在另一版本就修改协议代码的目标版本。

README 明确指出项目仍在演进：workflows、browser extension、task modes 属于实验性能力，`evolve` 尚未实现。本文把这些内容标为“实验性”，不会把它们描述成稳定扩展接口。

## 2. Workspace 与主要边界

根 `Cargo.toml` 是 virtual workspace（只有 `[workspace]`，`resolver = "2"`），workspace 成员覆盖 foundation、agent、apps 与 experimental 等目录。贡献者应先定位功能所属层，再选择 crate；不要把应用入口的行为复制到 foundation crate。

从当前 manifest 可以确认的边界如下：

| 区域 | 当前可见职责与证据 |
| --- | --- |
| `foundation/config` | 统一读取 anureo home 下的 `config.toml`、项目 `.env`、provider 配置，并把有效值应用到进程环境；还 re-export LSP、MCP、model/provider 与 XDG TOML 相关接口。 |
| `agent/agent-core` 与工具依赖 | `cli`、`acp`、`server` 都通过 `agent`/tool crates 使用 Agent 能力；这些应用 manifest 没有把 Agent 实现放在入口 crate 中。 |
| `apps/cli` | 二进制名是 `anureo`，库名是 `cli`；依赖 agent、LLM、graph、skill、tool、memory、checkpoint、ACP、stream event 与 config，并启用 `config` 的 `tracing-init` feature。 |
| `apps/acp` | 库名是 `anureo_acp`，package 名是 `acp`；提供 ACP server，并直接依赖 `agent-client-protocol = 0.15.1`，启用 unstable session/model/cancel/token-usage 等 feature。 |
| `apps/server` | 二进制名和 package 名是 `anureo-server`，库名是 `anureo_server`；是 HTTP+SSE、opencode-compatible kernel 的 External mode 入口，使用 Axum `0.7`、WebSocket、`anureo-acp` 与 PTY protocol。`test-support` feature 只为 black-box integration suite 暴露确定性的 LLM 注入路径，manifest 明确说明生产 routes 不选择它。 |
| `anureo-examples` | 通过显式 `[[example]]` 声明 `echo`、`state_graph_echo`、`memory_checkpoint`、`memory_persistence`、`react_memory`、`openai_embedding`；`embedding` feature 默认关闭，启用后才把 `vector-store/lance` 拉入 `react_memory` 等示例。 |

根 workspace 还集中声明了 `tokio`、`clap`、`serde`、`serde_json`、`tracing`、`dotenv` 等共享依赖；具体 crate 可以在自己的 `Cargo.toml` 中增加 feature 或版本约束。不要仅凭 workspace dependency 名称推断某个应用一定暴露了同名 CLI/API。

## 3. 配置目录、文件与优先级

### 3.1 anureo home

`foundation/config/src/home.rs` 是用户级数据目录的单一边界：若进程环境中存在 `ANUREO_HOME`，`anureo_home()` 直接使用它；否则 Unix 使用 `$HOME/.anureo`，Windows 使用 `%USERPROFILE%/.anureo`；若相应 home 环境变量也缺失，源码最后回退到当前目录 `.`。不要在未设置 `HOME`/`USERPROFILE` 且当前目录是源码仓库时运行 anureo：config、thread 或日志可能落在仓库中，并把运行数据与源码混在一起。需要验证这个边界时，使用临时 `ANUREO_HOME`，并检查实际生效路径（例如查看 `config_path()`、`logs_dir()` 的返回值或对应诊断输出）。

home 下的路径由纯路径拼接函数返回，函数本身不创建目录：

```text
{ANUREO_HOME}/config.toml
{ANUREO_HOME}/thread/{session_id}/
{ANUREO_HOME}/acp/
{ANUREO_HOME}/logs/cli/
{ANUREO_HOME}/logs/acp/anureo-acp.log
{ANUREO_HOME}/logs/llm/
```

`logs_dir()` 是 `{home}/logs`，`cli_logs_dir()`、`acp_logs_dir()`、`llm_logs_dir()` 在其下分层；`default_acp_log_file()` 固定追加 `anureo-acp.log`。这些函数只表达路径，不承诺调用处一定已经创建目录。

### 3.2 `.env` 与 `config.toml`

根 [`.env.example`](../../.env.example) 展示的是示例变量：`LANGGRAPH_API_KEY`（可选 server request auth）、`OPENAI_API_KEY`、`OPENAI_API_BASE`、`OPENAI_MODEL`、`OPENAI_TEMPERATURE`、`OPENAI_TOOL_CHOICE`、embedding 对应的 `EMBEDDING_API_KEY`/`EMBEDDING_API_BASE`/`EMBEDDING_MODEL`，以及 Exa MCP 的 `EXA_API_KEY`、`MCP_EXA_URL`、`MCP_REMOTE_CMD`、`MCP_REMOTE_ARGS`。但 CLI/config loader 的等价配置应使用 `OPENAI_API_KEY`、`OPENAI_BASE_URL` 和 `MODEL`；`OPENAI_API_BASE`、`OPENAI_MODEL` 不是它们的别名。首次配置请以[`快速开始`](../user-guide/01-quickstart.md)与[`config README`](../../foundation/config/README.md)为准。示例值不是凭据；复制为 `.env` 后才填写真实值。

`foundation/config/README.md` 给出的 TOML 入口是 `~/.anureo/config.toml`，也可以由 `ANUREO_HOME` 改址：

```toml
[env]
OPENAI_API_KEY = "sk-..."
OPENAI_BASE_URL = "https://api.openai.com/v1"
RUST_LOG = "info"
```

当前 `load_and_apply_with_report` 的实际优先级（高到低）是：

```text
已存在的进程环境 > 项目 .env > [default].provider 选中的 [[providers]] > config.toml 的 [env]
```

这里的“高”表示不会被较低层覆盖。`override_dir` 若为 `Some`，`.env` 从该目录读取；否则从当前目录读取。不存在的 config 文件按空配置处理；不存在的 `.env` 也不会因为“缺少文件”而报错。配置路径、读取、TOML 解析和 `.env` 读取失败分别通过 `LoadError` 暴露。

### 3.3 provider 与安全行为

`[default].provider` 只负责选择一个同名 `[[providers]]`；找不到对应 provider 时源码测试确认不会应用该 provider 的值。provider 映射可以产生 `OPENAI_API_KEY`、`OPENAI_BASE_URL`、`MODEL` 等环境变量；provider 的 `type` 还可以产生 `LLM_PROVIDER`。若 provider 没有 base URL，源码会优先使用 `ANUREO_MODELS_DEV_API_JSON` 中的内联 JSON，否则请求 `MODELS_DEV_URL`，默认 URL 是 `https://models.dev/api.json`，再按 provider 名称提取 API 地址。这是当前实现的网络回退机制，属于应谨慎依赖的实验性行为；它不是稳定的 provider discovery API。

配置报告中的 secret 不直接输出：`is_secret_key` 会识别 key/token/secret/password/credential/auth 等命名，`mask_value` 保留前后各两个字符（过短值显示为 `***`）。日志和新诊断代码应使用报告的 masked value，不要打印原始 API key。

## 4. 从命令到运行时的调用流程

当前可以由根 README 直接确认的 CLI 路径是：

```text
cargo run -p anureo-cli -- -m "task"
        │
        ├─ --working-folder <dir>：显式指定 Agent 工作目录
        ├─ --session-id <id>：继续已有 session
        └─ --worktree：修改任务使用隔离 Git worktree
```

CLI 的用户命令还包括 `anureo session list`、`anureo models`、`anureo tool list`、`anureo mcp list`、`anureo skills list`、`anureo memory list` 与 `anureo acp`。从 crate 依赖关系可以确定，CLI 入口把配置、Agent、LLM、graph、tool、checkpoint、stream event 和 ACP 组合起来；但本文指定源码没有给出每个 subcommand 的内部 route，因此不把它们扩写为未验证的 API。

ACP 路径是 `anureo acp` 启动 `apps/acp` 提供的 ACP server，供兼容 IDE 接入；server 路径是独立的 `anureo-server` HTTP+SSE/WS 进程。二者共享 `config`、Agent/LLM 等基础 crate，但协议边界不同：扩展 IDE 通信应先看 ACP crate 和[`ACP/IDE 指南`](../guides/acp-ide.md)，扩展 CLI 行为应先看[`CLI 指南`](../guides/cli.md)，扩展 HTTP/SSE route 应先看 server crate，不要跨入口复用未经抽象的 handler。安全边界见[`安全与隐私指南`](../guides/security-and-privacy.md)。

## 5. 贡献者的扩展点

- 增加用户级目录或日志分类：在 `foundation/config/src/home.rs` 增加路径函数，并同步其平台测试；保持函数只负责路径计算，创建目录由调用方负责。
- 增加配置来源或字段：先在 `foundation/config` 的 TOML/provider 模型中增加结构化字段，再经过环境映射进入应用；遵守“已有进程环境不覆盖”的契约，并扩展 `ConfigSource`/`ConfigLoadReport` 测试。
- 增加 CLI 行为：修改 `apps/cli` 的 clap 入口和相应 service wiring；命令名、参数名和默认值必须以 CLI 源码为准，不能从 README 示例反推不存在的参数。
- 增加 IDE 能力：在 `apps/acp` 的 ACP 协议适配边界实现，并注意当前 `agent-client-protocol` 的 `0.15.1` 与启用的 unstable features。
- 增加 HTTP 能力：在 `apps/server` 的 Axum route/middleware 边界实现；`test-support` 是测试注入开关，不应被生产路径选用。
- 增加示例：在 `anureo-examples/Cargo.toml` 添加显式 `[[example]]`；涉及向量存储时保留 feature gating，避免默认测试引入 LanceDB。
- 增加 workflows 或 Lua 运行能力：根 README 把 workflows 归为实验性；在没有对应稳定接口与测试证据前，应明确标注实验性，不能把 Lua workflow 当成稳定公共 API。

## 6. 测试与验证方式

先用 workspace 级别的标准检查确认依赖和编译：

```powershell
cargo check
cargo build -p anureo-cli
cargo test -p anureo-cli
```

修改 `foundation/config` 后，应至少运行该 crate 的测试（若 crate 名称被 Cargo 解析为 `config`，使用 `cargo test -p config`），并覆盖环境恢复、优先级、provider 选择、masked report 和 home 路径。源码已有测试使用临时目录、`ANUREO_HOME` 与全局 mutex，测试后恢复原环境变量；新增测试必须遵守同样规则，因为环境变量是进程全局状态。

`anureo-examples` 是目录下存在但未列入根 `[workspace].members` 的 package，不能使用 `cargo test -p anureo-examples` 从根 workspace 定位。当前它的 manifest 仍隐式属于父 workspace；直接验证会报“current package believes it's in a workspace”，因此以下命令是该 package 在仓库状态明确为独立 manifest（例如 manifest 增加空的 `[workspace]`）后的可复制入口，当前文档不把它们宣称为已通过：

```powershell
cd anureo-examples
cargo test --manifest-path Cargo.toml --no-run
cargo test --manifest-path Cargo.toml --no-run --features embedding
```

上述第二条会启用 `vector-store/lance`，属于可选路径；没有必要时不要让默认测试承担它的构建成本。其他入口可分别运行 `cargo test -p acp` 与 `cargo test -p anureo-server`。ACP manifest 显式声明了 `e2e_mega` 和 `e2e` 两个 test target；涉及 ACP 行为时应检查这两个目标，而不只跑 library unit tests。测试隔离、日志和调试细节见[`测试、调试与可观测性`](09-testing-debugging-and-observability.md)。

## 7. 常见坑

- 把 `~/.anureo` 当成不可变路径：源码支持 `ANUREO_HOME`，测试和自定义部署应使用它；Windows fallback 使用 `%USERPROFILE%`，Unix fallback 使用 `$HOME`。若二者都缺失，源码会回退到当前目录；不要在仓库目录中依赖这个回退，验证时设置临时 `ANUREO_HOME` 并检查 effective home/log path。
- 误以为 `.env` 会覆盖 shell 环境：实际最高优先级是已有进程环境；`.env` 也高于 provider 和 `[env]`，但只填补尚未设置的 key。
- 误把 `OPENAI_API_BASE` 与 TOML 示例中的 `OPENAI_BASE_URL` 当成同一个源码字段：两者都出现在当前材料中，但配置加载器的 provider fallback 明确操作的是 `OPENAI_BASE_URL`；新增逻辑前应追踪实际 consumer。
- 直接写入 secret report 或日志：`ConfigLoadReport` 设计为 masked display；不要绕过 `value_masked`。
- 认为路径函数会创建目录：`thread_session_dir`、`acp_data_dir`、日志路径函数都只返回 `PathBuf`。
- 运行示例时忘记 feature：`react_memory` 和 `openai_embedding` 要求 `embedding`；默认 `cargo test` 不等于完整 embedding 验证。
- 把 `apps/acp` 的 package 名、库名和协议 crate 名混淆：package 是 `acp`，库名是 `anureo_acp`，其他 manifest 以 `anureo-acp` 作为依赖别名。
- 把 server 的 `test-support` 当作生产开关：manifest 已明确说明它只服务黑盒集成测试。
- 忽略实验性标记：workflows、browser extension、task modes 当前仍在演进，`evolve` 未实现；贡献说明和文档不要承诺它们的稳定兼容性。
- 只改一个 manifest 却忽略 `Cargo.lock`：workspace 依赖解析可能同时保留多个版本；改依赖后应让 Cargo 更新锁文件，并检查变更是否属于本次任务。

## 8. 最小贡献流程

下面是一条不启动 Agent、不需要 API key、网络、模型费用或工具权限的低风险闭环，适合先改 `foundation/config` 的路径文档或单元测试。需要真实 Agent/入口命令时，先阅读[`CLI 指南`](../guides/cli.md)和[`安全与隐私指南`](../guides/security-and-privacy.md)：这类命令可能访问网络、消耗 API 配额/费用，并在授权范围内读写工作目录。

```powershell
git status --short
# 仅修改 foundation/config 的文档或测试；不要启动 anureo/Agent
cargo fmt --check
cargo test -p config
cargo check -p config
git diff --check
git diff -- foundation/config
git status --short
```

在开始前确认当前工作目录和已有改动；根 [`README.md`](../../README.md) 对修改任务建议使用 `--worktree`，需要运行 Agent 时才采用该隔离入口。提交前确认 diff 只包含本次改动，不要把不相关的用户改动带入提交。完成上述低风险闭环后，再按目标边界补充测试。

1. 从根 [`README.md`](../../README.md) 和目标 crate 的 `Cargo.toml` 确认入口、package/lib/bin 名称与 feature。
2. 追踪配置来源：进程环境 → 项目 `.env` → active provider → `ANUREO_HOME/config.toml` `[env]`。
3. 在所属层实现，避免把 CLI、ACP、HTTP route 逻辑下沉到不拥有该边界的 crate。
4. 为路径、环境优先级、feature 或协议行为补测试；测试中隔离并恢复环境变量。
5. 运行相关 crate 的 `cargo test`，再按需要运行 workspace check/build 和入口级集成测试。
6. 检查文档与命令是否仍与当前源码一致，并对实验性内容显式标注；涉及环境隔离、日志或失败诊断时继续阅读[`测试、调试与可观测性`](09-testing-debugging-and-observability.md)。

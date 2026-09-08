# ACP 与 Backend 集成

> **状态**: 已实现；本文同时标出尚未接线的实验性能力和演进方向
> **相关代码**: `apps/acp/src/`、`apps/server/src/`
> **交叉参考**: [ACP agent refactor](../design/acp-agent-refactor.md)（架构背景与未完成拆分）、[ACP IDE guide](../guides/acp-ide.md)（IDE 使用与权限前置）

本文面向 anureo 贡献者，解释 IDE/ACP、stdio ACP server、HTTP/WebSocket backend 与 Agent runtime 如何连接。所有接口、命令和行为均以当前源码为准；没有在源码或测试中出现的能力不在本文中承诺。

## 阅读顺序与术语

第一次修改 ACP 前，先读本文第 1、3 节，再读 `../guides/acp-ide.md` 了解 IDE 的 permission/working-directory 前置；需要理解设计目标时再读 `../design/acp-agent-refactor.md`，该文包含尚未完全落地的拆分方案。仓库贡献环境和 Rust toolchain 以仓库根目录的配置及现有 CI 为准。

本文使用以下术语：connection 是一次 stdio/WS transport 连接；session 是 ACP 的会话键及其 `SessionEntry`；bridge 是向该 client 发反向 RPC 的 `ClientBridgeTrait` 实现；agent 是 `anureoAcpAgent`；generation 是 `SessionStore` 为一次 prompt 保存的取消/运行代次；notification sink 是 agent 上用于发送 `session/update` 的 channel。`active generation` 属于 `AcpHub` 的断线/reconnect 状态，不等于 bridge 或 session。

| 对象 | 创建/持有者 | 断开行为 | 主要测试位置 |
| --- | --- | --- | --- |
| connection | stdio loop 或 `apps/server/src/acp_hub.rs` | transport 关闭；当前 stdio bridge registry 不自动按真实 session 清理 | `apps/acp/src/stdio_loop.rs`、hub tests |
| session | `apps/acp/src/session.rs` 的 `SessionStore` | 可持久化/恢复；不能把 session id 当 connection id | `agent_integration`、`model_persistence` |
| bridge | `apps/acp/src/tools/client_bridge.rs` 的 `SESSION_BRIDGES` | registry 支持显式移除，但当前 stdio 只注册 `default` | fs/terminal tool tests |
| agent | `anureoAcpAgent` 或 `AcpHub` | hub 可按 `DisconnectPolicy` 保留/取消 generation | `apps/server/src/acp_hub.rs` |
| generation / sink | session store / agent | `cancel` 取消；重连替换 notification sink 的路径由 hub 管理 | hub 与 agent integration tests |

## 1. 边界与入口

`apps/acp` 的 package 名是 `acp`，library crate 名是 `anureo_acp`；`apps/acp/src/lib.rs` 导出 `anureoAcpAgent`、`SessionStore`、`run_agent_connection` 和 stream 相关类型。`apps/acp/src/main.rs` 是 CLI 入口，`apps/acp/src/server.rs` 负责 daemon 的 `start`、`stop`、`restart`、`reload` 及 PID/log，JSON-RPC stdio 处理在 `apps/acp/src/stdio_loop.rs`。

`apps/server` 是独立的 HTTP/WebSocket backend。`apps/server/src/routes.rs` 组装 Axum `Router`；ACP WebSocket 入口由 `apps/server/src/handlers/acp.rs` 暴露并交给 `apps/server/src/acp_hub.rs`，普通 prompt 则由 session handler 调用 `apps/server/src/agent_runner.rs`。两条路径共享 anureo 的 agent/config/model 基础设施，但不是同一个 transport。

## 2. 模块职责

| 模块 | 当前职责与源码证据 |
| --- | --- |
| `apps/acp/src/stdio_loop.rs` | 构造 stdin/stdout transport、`ConnectionTo<Client>` 与 agent；处理 EOF、shutdown 和连接关闭错误。initialize responder 当前唯一注册调用是 `set_connection_for_session("default", ...)`。 |
| `apps/acp/src/agent.rs` | 实现 initialize、session、prompt、cancel 与 session config；prompt 的 `RunOptions.bash_executor` 当前无条件是 `LocalCommandExecutor`。 |
| `apps/acp/src/session.rs` | 保存 `SessionEntry`、`SessionConfig`、取消状态、MCP 定义及持久化 thread/session 映射。 |
| `apps/acp/src/agent_registry.rs` | 注册内建 Agent mode，并提供默认 mode；新增 mode 应从这里追踪到实际 `agent::run` 配置。 |
| `apps/acp/src/client_capabilities.rs` | 解析 fs、terminal、MCP、prompt content 与 session 能力；未声明的 capability 默认不可用。 |
| `apps/acp/src/client_methods.rs` | 封装 `read_text_file`、`write_text_file`、`terminal/*` 等反向 ACP RPC。 |
| `apps/acp/src/tools/client_bridge.rs` | 定义 bridge trait/实现和 `SESSION_BRIDGES`；数据结构支持按 key 存储，但不是自动完成 session 绑定的证明。 |
| `apps/acp/src/tools/fs_tools.rs` | 将 fs tool 映射到 bridge；从 `ToolCallContext.acp_session_id` 取真实 session id，找不到 bridge 即返回 transport error。 |
| `apps/acp/src/tools/terminal_executor.rs` | 独立定义本地 `TerminalCommandExecutor` 与 ACP `AcpBridgeCommandExecutor`；后者当前未接入 agent prompt production path。 |
| `apps/acp/src/mcp_convert.rs`、`apps/acp/src/stream_bridge.rs` | 分别转换 MCP 定义和 anureo stream event 为 ACP 通知；改 event/schema 时应追踪协议转换测试。 |
| `apps/server/src/acp_hub.rs` | 持有可重连的 agent、session store 与 connection lease；`DisconnectPolicy::Persist` 为默认，`ANUREO_ACP_DISCONNECT_POLICY=cancel` 可选择取消。 |
| `apps/server/src/agent_runner.rs`、`apps/server/src/storage.rs` | 分别运行 HTTP prompt/发布事件，及提供 backend 持久化/内存存储边界。 |

## 3. 调用流程与当前实现

### 3.1 stdio ACP

```text
anureo acp -> apps/acp/src/main.rs -> server::run_server
  -> apps/acp/src/stdio_loop.rs -> run_agent_connection
  -> initialize -> session/new 或 session/load -> agent::prompt
  -> anureo Agent/LLM/tool loop -> stream_bridge -> session/update/response
```

`initialize` 先于 `session/new`。当前 stdio 实现会在 initialize responder 中把连接保存到 bridge registry 的键 `default`（`apps/acp/src/stdio_loop.rs:258-263`），没有证据表明随后会在 session 创建/加载成功后自动用真实 session id 注册，也没有相应的移除接线。因此应准确表述为：registry 的数据结构支持 session key，但当前 stdio 不承诺真实 session 隔离；不要据此设计多连接安全语义。若要实现真实隔离，需在 `session/new`/`session/load` 成功后用真实 id 注册，并在连接/会话结束时移除或更新 bridge，同时补 fs/terminal reverse-RPC 集成测试。

### 3.2 session 与 prompt

`anureoAcpAgent` 通过 `SessionStore` 创建或恢复 `SessionEntry`，读取/更新 `SessionConfig`，再构造运行配置。模型切换由 `dynamic_model_switching.rs` 和 `model_persistence.rs` 覆盖。

`create_acp_tools(&ClientCapabilitiesInfo)` 只按 fs read/write capability 添加 `ReadTextFileTool`/`WriteTextFileTool`（`apps/acp/src/tools/mod.rs:44-62`）。terminal capability 并不会在当前 prompt 装配 `AcpBridgeCommandExecutor`：`apps/acp/src/agent.rs:910-939` 无条件设置 `Some(Arc::new(LocalCommandExecutor))`，并记录 `Using local bash executor (ACP terminal disabled)`。因此当前 ACP prompt 使用本地 `LocalCommandExecutor`；`AcpBridgeCommandExecutor` 是已定义的独立实现/测试能力，尚未由当前 agent prompt 接线。若未来按 capability 选择 ACP terminal，必须先在 `RunOptions`/工具装配处完成选择，并补 production-path 测试。

### 3.3 HTTP/WebSocket backend

```text
HTTP/WS request -> apps/server/src/routes.rs
  -> handlers/acp.rs (WS ACP) 或 session handler (HTTP prompt)
  -> AcpHub::attach / agent_runner::run_agent
  -> anureoAcpAgent 或 run_agent_from_config -> event bus/SSE/WS
```

`AcpHub::attach_with` 创建或复用 durable agent；重连会替换 notification sink，owner 不一致时 takeover 被拒绝。断开默认保留 active generation，`cancel` 策略才取消。durable、replay、idle-TTL 仍是演进语境，不应写成完整跨进程 replay 服务。

## 4. 反向工具、MCP、生命周期与安全边界

ACP fs/terminal 是 client-facing reverse RPC。capability gate 只是 client 声明的协议条件，不等于 authorization、workspace sandbox 或用户确认；这些仍由 client/IDE permission UI、宿主策略和 anureo 工具实现共同决定。`../guides/acp-ide.md` 中的确认流程不能被 capability 字段替代。

审阅或修改 fs/terminal 时必须明确：

- `fs_tools.rs` 接受相对 workspace root 的路径，也接受 absolute path；必须检查越界路径、符号链接/工作区策略和 IDE 用户确认。写工具会先读旧内容，再调用 `write_text_file`，不能把它当作只读能力。
- terminal reverse RPC 接受 working directory 并启动 client 侧命令；本地 fallback 则执行宿主环境命令，可能继承环境变量/凭据。检查命令注入、路径、环境、退出状态和输出中的敏感数据泄露。
- capability gate、authorization、路径策略和确认失败都要有负向测试；禁止用真实 workspace、真实凭据或真实生产 IDE 验证。

terminal bridge 的生命周期是 `terminal/create`、输出读取、`terminal/wait_for_exit`，必要时 `terminal/kill`，最后 `terminal/release`。当前错误边界不是全量传播：`terminal_wait_for_exit` 出错时只记录日志后继续（`apps/acp/src/tools/terminal_executor.rs:255-263`）；超时路径对 kill/release 使用 `let _ =`（`:265-269`），正常路径也忽略 release 错误（`:284-301`）。贡献者不要把它描述成完整的 cleanup-error propagation；若要改变语义，应传播错误并补测试。

MCP server 来自 `session/new` 或 `session/load`，经 `apps/acp/src/mcp_convert.rs` 转换并保存到 session。HTTP routes 中标为 TODO、compat 或 501 的能力不是稳定 API。

## 5. 可扩展点

1. **新增 Agent mode**：修改 `apps/acp/src/agent_registry.rs` 的注册/默认 mode，追踪 `apps/acp/src/agent.rs` 的解析，并补 agent mode 单测。
2. **新增模型来源**：扩展 `ModelProvider`/registry，保留 mock provider 注入点，避免测试依赖真实 LLM。
3. **新增 ACP client 能力**：先在 `client_capabilities.rs` 解析，再在 tool factory 做 gate，随后实现 RPC、负向测试和 e2e responder。
4. **接线 ACP terminal**：在 `agent.rs`/`RunOptions` 装配处分支选择 executor，测试 capability true/false、session bridge 缺失和 production prompt 路径。
5. **新增 HTTP API/断线策略**：分别追踪 `routes.rs`、handler、storage、event consumer 与 `AcpHubConfig`/`DisconnectPolicy`，不要绕过 hub 或重新引入全局 bridge。

## 6. 安全小练习：给 agent registry 增加无副作用 mode

这是第一次贡献者可独立完成的实验性练习，不需要真实 LLM、网络、API key、IDE 凭据或 e2e。只修改 mode 注册和对应 deterministic unit test；不要改 prompt 装配、权限、fs/terminal 或共享 storage。

1. 从仓库根目录定位 `apps/acp/src/agent_registry.rs` 的注册函数和现有 mode test；在同一文件增加一个仅返回静态元数据的 `contributor-demo` mode，复制现有 mode 的最小字段，不改变默认 mode。
2. 在该文件的 unit test 中断言 registry 能找到 `contributor-demo`，并断言默认 mode 仍不变。最小 diff 应只包含注册表的一项和一个测试函数；不要加入真实 provider 或命令执行。
3. 先运行最小验证：

   ```powershell
   cargo test -p acp agent_registry
   cargo fmt --check
   cargo check -p acp
   ```

   预期是目标测试通过、fmt 无输出、`cargo check -p acp` 成功。若测试过滤器在当前 toolchain 下没有匹配，改跑 `cargo test -p acp agent_registry -- --list` 确认名称，再使用实际测试名。

4. 检查边界：`git diff -- apps/acp/src/agent_registry.rs` 只应显示这两个小改动；`git status --short` 应确认没有意外文件。失败时先记录是 Rust/toolchain、依赖、编译错误还是外部服务问题。
5. 撤销本地练习：若文件没有其他并行改动，可使用 `git diff -- apps/acp/src/agent_registry.rs` 保存审阅结果后由版本控制工具恢复该文件；若有他人改动，不要整文件恢复，应手工反向删除练习的注册项和测试。

## 7. 分层验证与环境前置

所有命令从 anureo 仓库根目录执行，并使用仓库指定的 Rust toolchain。先跑不需要外部服务的验证：

```powershell
cargo fmt --check
cargo test -p acp --lib
cargo test -p acp --test agent_integration
cargo test -p acp --test dynamic_model_switching
cargo test -p acp --test model_persistence
cargo check -p acp
```

修改 `apps/server` 再运行 `cargo test -p anureo-server` 和 `cargo check -p anureo-server`。变更 workspace 依赖时按项目惯例检查 `Cargo.lock`。

`cargo test -p acp --test e2e` 是有效 target（`apps/acp/Cargo.toml` 的 `[[test]]` 指向 `tests/e2e/main.rs`），但属于扩展验证：需要可启动的 ACP binary、相应模型配置/环境变量，部分场景需要网络、真实 provider 或凭据。harness 默认 request timeout 为 30 秒、graceful-exit timeout 为 10 秒；没有这些前置条件时先跳过 e2e，使用上述 deterministic tests，不要把环境失败当产品失败。只有在环境满足后再运行：

```powershell
cargo test -p acp --test e2e
```

修改 reverse RPC、connection/session 绑定或 permission 边界时，还必须补相应负向测试；不得用真实 workspace 或凭据替代隔离 fixture。

## 8. 常见坑

- `apps/server/src/acp_hub.rs` 的路径包含 `src/`；所有源码引用均从仓库根目录复制。
- `SESSION_BRIDGES` 支持按 key 存储不代表 stdio 已按真实 session 绑定；当前 initialize 只有 `default` 注册。
- terminal capability 不代表 ACP terminal executor 已接线；当前 prompt 明确使用本地 executor。
- `DisconnectPolicy::Persist` 不保证断线后的 client reverse RPC 可用；旧 bridge 可能返回 transport error。
- 未声明 capability 时不要注册 ACP fs tool；声明 capability 也不等于授权或路径沙箱。
- 不要把 HTTP event 与 ACP `session/update` 等同，也不要把 TODO/compat/501 route 当成生产 API。
- package `acp`、library `anureo_acp`、依赖别名 `anureo-acp` 不是同一个名称。

## 9. 实验性与未实现边界

`apps/server` 的 ACP hub 已有 reconnect、owner 检查、断线策略和统计测试，但 durable/replay/idle-TTL 仍是演进方向。`AcpBridgeCommandExecutor` 已定义并有测试能力，却未接入当前 ACP agent prompt；按 terminal capability 自动切换属于待实现工作。真实 session-id bridge 绑定、连接结束清理和多连接隔离也不能从当前 stdio 实现中推导出来。

Lua workflow、browser extension、`evolve` 或其他未出现在上述调用链中的能力不在本文 API 范围。没有源码和测试证据时，只能标为实验性。

## 10. 贡献前检查清单

1. 确认改动属于 `apps/acp`、`apps/server` 还是共享 agent/tool crate。
2. 从 transport/route 入口追踪到 session、model、tool、bridge 和 event consumer。
3. 明确行为是 ACP protocol、HTTP API、内部 trait 还是测试辅助能力。
4. 检查 capability、authorization、路径/命令、环境/凭据、断线、session 隔离和持久化边界；为允许与拒绝路径补测试。
5. 先运行 deterministic 最小验证，再按环境前置运行 e2e；最后检查 `git diff` 和 `git status`，确认只修改预期文件。

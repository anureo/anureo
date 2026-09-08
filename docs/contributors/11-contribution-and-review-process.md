# anureo 贡献与 Review 流程

> **状态**：基于当前源码的贡献者说明
> **相关代码**：`README.md`、`.github/workflows`、`agent/agent-core`、`apps/cli`、`apps/acp`、`apps/server`、`foundation/config`、`foundation/checkpoint-sqlite-store`、`agent/tool/tool-workflow`
> **适用范围**：anureo workspace 的 Rust、CLI、ACP、HTTP/SSE、配置、checkpoint/store 与 workflow 改动

本文面向 anureo 贡献者和 reviewer。结论只来自当前仓库及列出的设计、分析、协议和 spec 文件；没有源码证据的 API、路由、配置项或 CI 门禁不在本文中当作已实现能力。

## 1. 提交前先确认范围

anureo 是 local-first AI Agent runtime，入口包括 CLI、IDE 的 ACP 和 server。根 `Cargo.toml` 是 Rust 2021、`resolver = "2"` 的 virtual workspace，当前 workspace 版本为 `0.5.0`。先确认改动属于哪一层：

| 层 | 当前职责 | Review 时重点检查 |
| --- | --- | --- |
| `foundation` | graph/Pregel、LLM、config、checkpoint 与 SQLite store 等基础能力 | 不反向依赖 CLI、ACP handler 或 HTTP route；公共 trait 的兼容性与错误语义 |
| `agent/agent-core` | Agent pattern、run config、runner、stream、恢复与 cancellation | `RunCmd`、runner 构造和执行是否仍分层；是否错误地把宿主副作用塞进 agent-core |
| `agent/tool` | `Tool`、registry、local/MCP/workflow Tool | Tool spec、参数校验、context、注册和调用过滤是否一致 |
| `apps/cli` | clap 入口、config/context 组装、结果展示 | 命令参数是否来自实际 parser；是否重复实现 Agent/Tool 逻辑 |
| `apps/acp` | ACP `0.15.1` 协议适配与 IDE transport | capability gate、ACP wire 行为、session/cancel 与 e2e target |
| `apps/server` | Axum HTTP+SSE、WebSocket、PTY 和 OpenCode-compatible 外部入口 | route/handler、状态码、SSE envelope、认证 middleware 与黑盒测试注入 |

贡献前应查看目标 crate 的 `Cargo.toml`，确认 package/lib/bin 名称和 feature。比如 CLI package/bin 是 `cli`/`anureo`，ACP package/lib 是 `acp`/`anureo_acp`，server bin 是 `anureo-server`；server 的 `test-support` 只用于黑盒测试的 deterministic LLM 注入，生产 routes 不选择它。

README 建议首次运行前核对 effective working directory、model 和 tool permissions；修改任务可使用 `--worktree` 隔离 Git worktree。不要把 `ANUREO_HOME`、`.env` 或 worktree 语义写成未在当前代码中确认的全局约定。

## 2. 从入口追踪到运行时

### 2.1 Agent 与宿主调用流程

当前 `agent-core` 的可验证骨架是：

```text
CLI / ACP / server 构造 ReactBuildConfig
        ↓
build_runner(RunCmd)
        ↓
React / Dup / Tot / Got runner
        ↓
run_agent_from_config
        ↓
stream event、AgentResult / RunCompletion、checkpoint 或取消
```

`RunCmd` 当前包含 `React`、`Dup`、`Tot` 和 `Got { got_adaptive }`。`agent-core` 的 `run` 模块把 config builder、runner 和 types 分开；应用层应先构造 config，再调用 `run_agent_from_config`，不要把 worktree、debug、curator 等 app-side side effect 混进旧式 convenience wrapper。新增 agent pattern 时，先在 agent 层提供 state、runner、initial state 和 builder；只有需要 CLI 暴露时，才额外接入 `RunCmd` 与 CLI wiring。

额外 Tool 的当前注入点是 `ExtraToolsProvider`。`tool-workflow` 的 `default_workflow_tool_provider()` 根据 `ReactBuildConfig` 构造共享 `WorkflowRuntime`，再返回 workflow Tools；新增一组只服务某类 run 的 Tool 时，先评估这个注入点，而不是在 CLI、ACP、server 各注册一份。

### 2.2 Tool 调用流程

`ToolRegistry` 以 name 保存 Tool，同名注册会覆盖旧值；`ToolRegistryLocked` 提供异步注册、列举和调用。调用前后需要分别考虑：列表 `filter`、调用 `call_filter`、`dry_run`、effective `ToolCallContext` 和 `Tool::call` 的输出类型。异步代码使用 `register_async`；`register_sync` 会另起线程创建 current-thread Tokio runtime，不应作为已有 Tokio runtime 中的首选路径。

一个 local Tool 的最小审查链是：

```text
实现 Tool trait
  → 提供 name/spec/参数校验/call/builtin_skill
  → 在 owning crate 的 register/provider 接入
  → Agent tool source 构造
  → list_tools 或 call_tool
  → Text / Diff / 结构化结果 / stream event
  → 对应 unit、integration 或 e2e test
```

`BuiltinSkill` 是 guidance/discovery，不是第二套调用协议；`ToolSpec.name`、`Tool::name()`、registry key 和 model-facing name 必须保持一致。新增文件 Tool 应复用已有路径 containment 逻辑；`allow_outside = true` 会取消 containment check，是高风险开关。

### 2.3 Workflow 的特殊边界

`tool-workflow` 的当前设计拆成六个职责单一的 Tool：`workflow_start`、`workflow_status`、`workflow_list`、`workflow_events`、`workflow_source`、`workflow_files`。`workflow_start` 是后台启动语义，立即返回 `instance_dir` 与 `status: "running"`；调用方应使用 shell 的 `sleep 5` 或 PowerShell `Start-Sleep -Seconds 5` 后查询 `workflow_status`，不能紧密轮询，也不能把 sleep 与 status 查询放入并行 batch。

后台 task 负责等待完成、取消传播、事件转发、checkpoint/events 读取和 finalize；`RecvError::Lagged` 不应直接判定失败，finalize 失败还必须写入可读的 failed fallback，否则 status 会永久显示 running。公开 summary 要清洗绝对路径、`report_ref`、`output_ref`、`checkpoint_hash` 等内部字段。新增结果形态时，先更新 `finalize_output`/状态优先级测试，再更新实例 artifact 和事件验证。

workflows、browser extension、task modes 在 README 中明确标为实验性，`evolve` 尚未实现；workflow DSL 的长期改进方案和 ACP WebSocket 的 Phase B–E 方案也仍含待实施项。Review 不得把 spec 或 design 中的目标状态写成稳定 API。

## 3. 配置、持久化与协议改动的 review 规则

`foundation/config` 的实际配置优先级是：已有进程环境 > 项目 `.env` > `[default].provider` 选中的 `[[providers]]` > `config.toml` 的 `[env]`。`ANUREO_HOME` 可改变用户目录；配置报告会 mask secret-like key。新增配置必须追踪到实际 consumer，并补优先级、provider 未找到、secret masking 和环境恢复测试；不能只因为 TOML 模型有字段就宣称运行时已经使用。

checkpoint 和 Store 语义不同：前者服务 thread/run 状态快照、resume/replay/branch，后者服务跨运行的 durable 数据。`checkpoint-sqlite-store` 提供 SQLite-backed 实现。新增持久化字段要说明迁移/旧数据行为、进程重启和失败写入语义，不能把长期 memory 当作 checkpoint resume state。

server 协议改动必须同时核对 route、handler、wire envelope 和探针。当前协议文档确认 v1 裸路径与 v2 `/api/*` 路径并存，`/global/event`、`/api/event` 和 `/api/session/:id/event` 是不同 SSE 边界；`/api/skill` 虽已注册，当前 handler 仍返回空列表，不能写成已有完整 skill registry。

`scripts/check-protocol.ps1` 与 `.sh` 会按当前配置启动 `cargo run -p anureo-server -- serve --host 127.0.0.1 --port ...`，检查 health、bootstrap routes、v1/v2 SSE、session create/update/read/shell/abort/delete 以及若干 stub surface；支持 `ANUREO_PROTOCOL_NO_BOOT`，脚本还提供端口、base URL 和 authorization 覆盖。它们当前对 DELETE session 期望 204，但 `CURRENT-CONTRACT.md`/`CURRENT-STATE-AUDIT.md` 记录的 handler 行为是 200 `{"success":true}`，所以执行前必须先统一契约，不能据脚本存在就报告 protocol gate 全绿。

SSE 测试应区分 legacy wrapper、v2 durable/live envelope、session cursor、replay、session isolation、restart 和 keepalive。`SERVER-SSE-INTEGRATION-TEST-PLAN.md` 要求使用真实 Axum Router 或 loopback TCP 读取原始 SSE bytes，并通过 anureo 自身 `MockLlm`/`MultiRoundMockLlm` 驱动 agent；不能用 translator 单测或“连接返回 200”代替端到端 wire 验证。该计划仍明确列出持久化重启、append failure、broadcast lag/reconnect、legacy keepalive、tool/failure/cancel 场景等未完成项。

## 4. 测试与验证顺序

先跑离改动最近的测试，再扩大范围。常用入口以当前 README、spec 和 crate manifest 为准：

```powershell
cargo fmt --check
cargo test -p <changed-package>
cargo clippy -p <changed-package> --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

针对不同改动补充：

- CLI：`cargo build -p anureo-cli`、`cargo test -p anureo-cli`；涉及 Tool 展示时覆盖 `list_tools`/`show_tool` 的 formatter tests。
- ACP：`cargo test -p acp`，并检查 manifest 声明的 `e2e_mega` 与 `e2e` test target；文件 Tool 要覆盖 capability gate、Text/Diff、错误和 round-trip。
- server：运行 crate tests 和 server integration tests；协议改动再运行对应平台的 `scripts/check-protocol.ps1` 或 `.sh`，但先处理上文 DELETE 状态码不一致。
- config：`cargo test -p config`；涉及环境变量时使用临时目录并恢复全局环境，避免并行测试共享 `ANUREO_HOME`。
- workflow：`cargo test -p tool-workflow`；必要时运行 `cargo test -p tool-workflow --test instance_smoke -- --nocapture`。fixture smoke test 由 `ANUREO_TEST_INSTANCES_DIR` 控制，未设置时按源码设计跳过。
- SQLite/checkpoint：覆盖临时数据库、重启后的读取、thread/checkpoint key、Store 与 Checkpointer 的边界；不要只测内存实现。

`.nextest.toml` 定义了 `default` 与 `ci` profile：default 使用 `num-cpus` test threads，普通慢测 10 秒超时并最多终止 3 次；`e2e_`、`anureo-acp` 有更长超时；ci profile 使用 8 threads、60 秒慢测超时。若环境安装 cargo-nextest，可用 `cargo nextest run` 采用这些配置；它不是当前 GitHub workflow 中已声明的命令。`.github/workflows/rust-release.yml` 在 PR 和 main/master push 上实际执行的是 stable toolchain、cache 和 `cargo build --release`；仓库当前没有在该 workflow 中声明 fmt、clippy、test 或 protocol gate 步骤。reviewer 应把本地验证结果与 CI 实际步骤分开记录。

## 5. Review 检查清单

### 5.1 代码与边界

- [ ] 改动文件属于真正 owning crate，依赖方向没有从 foundation 反向进入 app。
- [ ] public API、Tool name、CLI 参数、feature、路由和配置项都能在当前源码中定位。
- [ ] config、LLM、checkpoint/store、Tool registry 和宿主 transport 的边界没有被快捷复制绕过。
- [ ] 取消、错误、超时、重试、恢复和并发写入语义有明确结果，而不是通过日志让调用方猜测。
- [ ] secret、绝对路径、内部 reference 和 prompt/token 没有进入不应公开的日志或响应。

### 5.2 测试与证据

- [ ] 成功、输入错误、底层错误、取消/超时和边界输出均有测试。
- [ ] 协议测试断言 HTTP status、headers、SSE frame、event name、payload 字段、cursor 和 session isolation。
- [ ] mock 测试没有调用真实 Provider；server black-box 使用 test-only 注入路径。
- [ ] 环境变量、临时数据库、端口和 `ANUREO_HOME` 在测试间隔离，并在结束时清理/恢复。
- [ ] 设计文档中的“已完成”与当前测试/源码一致；实验性、stub、待验收项有明确标签。

### 5.3 文档与变更说明

提交说明至少应包含：改动 owning crate、调用流程、公开行为变化、测试命令及结果、未解决风险。协议、workflow 和 ACP 改动还要说明兼容性边界；不能用历史 archive 数字或旧设计状态替代当前测试。`specs/review-prompt-alignment.md` 只说明 background review prompt 与 runtime tool gate 的对齐差异，不能据此扩展出未实现的 review API。

## 6. 常见坑

- 把 README 的示例命令当成完整 CLI contract；参数和 subcommand 仍须回到 clap parser 与 command module。
- 把同名 Tool 注册理解成合并；当前 registry 是覆盖语义。
- 在 Tokio runtime 内使用 `register_sync`，或把共享的最近一次 `ToolCallContext` 当成 per-call isolation。
- 把 ACP client capability 当成 local filesystem 权限；两者调用桥接和安全边界不同。
- 把 workflow `start` 的 receipt 当作完成结果；必须按 status 轮询并在终态后读取 events/source/files。
- 把 `/api/skill`、experimental routes 或协议 stub 的可路由性当成功能完成。
- 把 `test-support` feature 带进生产 route，或让测试依赖真实 model/provider。
- 只运行 unit test 就宣称 SSE 兼容；原始 bytes、重连、cursor、隔离和状态码都需要单独验证。
- 看到 `.nextest.toml` 就认为 GitHub 已执行 nextest；当前 workflow 只明确执行 release build。
- 直接输出 config report、日志或 workflow summary 中的 secret、绝对路径和内部 reference。

## 7. 最小贡献流程

1. 读取根 `README.md`、目标 manifest 和 owning crate 的入口模块，确认现状与实验性边界。
2. 从宿主入口追踪到 config、runner、Tool/provider、输出事件和持久化；先画出实际调用链再改代码。
3. 在 owning crate 实现，并同步 public contract、错误/取消语义和最小测试。
4. 按“近端测试 → `cargo fmt --check`/clippy → workspace check/test → 协议探针”的顺序验证；协议状态码不一致时先修正基线。
5. Review 变更范围，确认除目标文档外没有无关文件被修改；在说明中记录命令、结果和未验收风险。


# anureo 测试、调试与可观测性

> **状态**：基于当前源码的贡献者说明
> **相关代码**：`agent/agent-core/src/test_support.rs`、`agent/tool/tool-workflow/tests`、`apps/acp/tests`、`apps/server/tests`、`foundation/llm/src/client/openai/tests.rs`、`foundation/stream-event/tests/stream_event.rs`、`apps/cli/src/logging.rs`、`apps/acp/src/logging.rs`

本文面向 anureo 贡献者，说明当前测试层级、可复用测试替身、workflow/ACP/server/LLM 的调试路径，以及日志和 stream event 的观测边界。所有命令、路径、配置项和行为均以当前源码为准；没有在指定源码中出现的 API 不在本文中当作已实现能力。

## 1. 测试地图与边界

anureo 的测试大致沿着“纯数据/状态 → crate 集成 → 真实进程 → HTTP/ACP 协议 → Web UI”分层。测试应尽量在拥有该行为的层验证，只有跨进程或跨协议行为才提升到 e2e。

| 层级 | 当前源码证据 | 适合验证的内容 |
| --- | --- | --- |
| Foundation unit | `foundation/llm/src/client/openai/tests.rs`、`foundation/stream-event/tests/stream_event.rs` | OpenAI 请求/响应、retry 分类、SSE chunk、event envelope 的字段和序号 |
| Agent/tool crate integration | `agent/tool/tool-basic/src/skill/manage/tests/coverage.rs`、`agent/tool/tool-workflow/tests/*.rs` | Tool 输入校验、回滚、runtime registry、cancel、checkpoint resume |
| 应用内 router | `apps/server/tests/endpoint_integration.rs`、`apps/server/tests/protocol.rs` | Axum route 的 status/body/header、event replay、SSE、协议形状 |
| 真实 ACP 进程 | `apps/acp/tests/e2e/common/harness.rs`、`apps/acp/tests/e2e/reload.rs`、`apps/acp/tests/e2e_mega.rs` | `anureo-acp` 子进程、JSON-RPC request/notification、reverse RPC、日志和退出；Cargo test target 名为 `e2e_mega` |
| Web e2e | `e2e/playwright.config.ts`、`e2e/tests/web/smoke.spec.ts` | 浏览器中的 session、发送消息、刷新恢复、空状态和 JS 失败 |

根 `README.md` 的开发命令是 `cargo build -p anureo-cli` 和 `cargo test -p anureo-cli`；workflow、browser extension 和 task modes 在 README 中仍标为实验性，`evolve` 尚未实现。不要因为存在测试文件就把实验性或过时测试描述成稳定 API。

## 2. Rust 测试运行方式

仓库根 `.nextest.toml` 是当前 nextest 配置：默认 `test-threads = "num-cpus"`、单测试 slow timeout 为 10 秒且最多终止 3 次；名称匹配 `test(e2e_)` 的测试为 30 秒/2 次；当前文件中的 ACP 覆盖条件仍是 `package(anureo-acp)`，但它与实际 Cargo package 名 `acp` 不匹配，因此不会覆盖 ACP package。若修复 nextest 配置本身，应使用 `package(acp)`；本文件只记录该现状，不修改配置文件。`ci` profile 使用 8 个线程、60 秒/3 次，且默认 profile `fail-fast = false`。

贡献者可先运行针对性测试，再扩大范围：

```powershell
cargo test -p anureo-cli
cargo test -p tool-workflow
cargo test -p anureo-server
cargo test -p acp
cargo test --workspace
```

上面的 package 名必须以当前 Cargo manifest 为准：ACP 的 package 名是 `acp`，库名是 `anureo_acp`，而 `anureo-acp` 只是在产品/二进制语境中使用的名称。修改 foundation 或跨 crate 协议后，再运行 `cargo check --workspace` 与对应 integration test。源码未定义统一的 `make test` 或 coverage 命令，不要在贡献文档中虚构一个。

### 2.1 环境变量测试必须串行化

`agent/agent-core/src/test_support.rs` 提供 crate 内共享的 `env_lock()` 和 `with_env()`。原因是 Rust 测试默认并行，而 `std::env::set_var`/`remove_var` 影响整个进程。`with_env()` 的实现会：

1. 对最外层调用取得 `OnceLock<Mutex<()>>` 中的进程级锁；
2. 保存旧值，设置或删除目标变量；
3. 通过 RAII 在闭包结束或 panic 时恢复旧值；
4. 用 thread-local depth 支持同线程嵌套调用，避免嵌套死锁。

新增环境测试应复用该 helper，不要自行设置 `ANUREO_HOME`、`OPENAI_BASE_URL` 或类似全局变量而不恢复。需要跨 crate 共享环境状态时，测试仍需在自己的 owning crate 设计隔离方案；不能假设 `agent-core` 的锁自动覆盖其他 crate。

## 3. Tool 与 workflow 测试

### 3.1 Skill manager：优先覆盖 Tool contract

`agent/tool/tool-basic/src/skill/manage/tests/coverage.rs` 使用临时 storage 和 `SkillManagerTool`，验证的是 Tool 的输入、JSON 响应和持久化副作用，而不是调用真实模型。当前证据包括：

- `validate_category` 拒绝空字符串、超过 64 个字符以及 `;`/空格等非法字符；`devops`、`devops/cicd` 和 `my_category` 通过。
- `for_foreground` 构造出的 spec 名称是 `skill_manage`。
- `create` 成功响应带回 `category`；非法 category 返回 `success: false` 和 category 错误。
- `edit` 对不存在 skill、无 frontmatter 和非法内容失败；校验失败会 rollback，原内容仍保留。
- `edit` 成功响应包含 `_change` 和描述 full rewrite 的 message。
- `patch` 要求非空 `old_string`；找不到匹配失败；多处匹配默认失败；`replace_all: true` 才允许全部替换。

这组测试体现一个重要边界：Tool 的可观测结果是结构化 `ToolCallContent`（测试再解析为 JSON），而文件 storage 是另一个断言面。新增 action 时应同时断言错误路径、持久化结果和响应字段，不要只测试 happy path。

### 3.2 workflow：runtime registry、cancel、status

`agent/tool/tool-workflow/tests/common/mod.rs` 提供 Lua script fixture：`SCRIPT_3PHASE` 依次执行 `collect`/`analyze`/`report` 三个 phase，每个 phase 调用一个 named agent；`SCRIPT_MULTI_AGENT` 在同一 phase 中调用多个 agent；`SCRIPT_NO_AGENT` 只发 phase 和 report。这些 fixture 用来构造可重复的 phase/agent 边界。

`cancel_basic.rs` 使用共享的 `Arc<WorkflowRuntime>` 和 `AgentConfig`。测试证据明确了以下调用流程：

```text
WorkflowRuntime::register_run(instance)
        ↓
WorkflowCancelTool::call({ instance 或 instance_dir })
        ↓
runtime registry 查找并请求取消
        ↓
{ result: "cancelling", instance_dir }
```

同一个 run 连续 cancel 仍返回 `cancelling`，说明当前 registry 命中路径是幂等的；未知实例返回 `not_found_or_terminal`。缺少 `instance` 会产生 Tool error，但 `instance_dir` 可作为 fallback。`../escape`、包含 `/` 或 `\` 的名称被拒绝，避免 path traversal。测试还要求 cancel 在约 50ms 内返回，说明 cancel tool 不应等待完整 workflow 结束。

状态读取由 `WorkflowStatusTool` 覆盖：checkpoint 中 `status: "cancelled"` 映射为 cancelled；存在 instance directory 但尚无 terminal checkpoint 时为 running；未知目录是 error。当前测试使用 `.anureo/instances/<instance>` 下的 `checkpoint.json`、`instance.json`、`events.jsonl` 和 `workflow.lua` fixture。不要把这些测试 fixture 推导成未经源码确认的公开文件格式；它们是当前 workflow runtime 的观测材料。

### 3.3 workflow：resume 与 crash recovery

`cancel_resume.rs` 分成两个 track。Tool-layer track 只测试 registry cancel；engine-level track 直接调用 `LuftBuilder::start_resume()`。已完成 workflow resume 后，三次原始 agent dispatch 仍只有 3 次，说明 journal/cache 命中后不会重新 dispatch；不存在的目录在 5 秒 timeout 内返回 error，不应 hang。

`resume_crash.rs` 通过 `resume_child` 子进程制造 crash：父进程从 stderr 的 `[child] run_dir: ...` 取 run id，再调用 `start_resume()`，用 `CountingBackend` 断言只重新执行 crash 点之后的 agent。当前覆盖：phase 之间 crash、phase 内多 agent 之间 crash、接近完成时 crash。这里的关键调用流程是：

```text
resume_child(base_dir, script, crash_after)
        ↓ stderr 输出 [child] run_dir
父测试提取 instance/run directory
        ↓ LuftBuilder::start_resume(run_dir)
checkpoint/journal 命中 → 已完成 agent 使用缓存
        ↓
CountingBackend::total_calls() 断言剩余 dispatch 数
```

resume 测试不应只断言最终 `result.is_ok()`；必须同时断言 dispatch count，否则缓存失效、重复执行仍可能“看起来成功”。

### 3.4 明确过时的 workflow 测试

`parallel_mapper.rs` 和 `terminal_events.rs` 顶部都写明：旧的 `WorkflowTool` 已在重构中拆成 `WorkflowStartTool`/`WorkflowRuntime`，这些测试需要按新 API 重写。它们是迁移提示，不是当前可执行 contract。贡献者修复 workflow 测试时应以当前公开类型和 `cancel_basic.rs`/`cancel_resume.rs` 的调用方式为准，不能恢复已删除的 `WorkflowTool`。

`instance_smoke.rs` 是可选 smoke：设置 `ANUREO_TEST_INSTANCES_DIR` 后读取真实 instance 下的 `checkpoint.json` 与 `events.jsonl`，调用 `build_instance_meta` 和 `write_instance_artifacts`，然后断言 `instance.json` 的 `schema_version = 1`、`status = completed`、非空 agents、64 字符 `checkpoint_hash`、正的 event total 和 `agent_done` 类型。未设置变量或 fixture 不存在时测试只打印提示并 return，不等于验证通过。

## 4. LLM、stream event 与协议测试

### 4.1 OpenAI client：本地 TCP mock，不依赖真实 provider

`foundation/llm/src/client/openai/tests.rs` 用 `tokio::net::TcpListener` 作为最小 HTTP server，读取请求后返回 JSON 或 SSE。测试覆盖：

- `ChatOpenAI::new`、`with_config`、`with_tools`、`with_temperature` 等 builder 可构造；
- `OPENAI_BASE_URL` 和 `OPENAI_API_BASE` 都能形成 `/v1/chat/completions` URL；测试用环境锁保护变量；
- 401/不可达服务和 stream 连接关闭返回 error；
- 400 的 tool-message contract 错误只尝试一次，映射为结构化 `LlmError::Provider` 且 `is_retryable() == false`；
- 500 会 retry，mock server 先返回 500 再返回 200，最终 response content 为 `ok` 且请求次数为 2；
- `invoke_stream(..., None, ...)` 委托到非 stream invoke 的等价路径；
- mock response 可解析 assistant content、tool call 和 usage；空 choices 返回包含 `no choices` 的错误；
- SSE stream 的 `data:` chunks 与 `[DONE]` 可进入 `TestSink`，并产生非空 stream chunks。

因此，新增 provider/client 行为时应使用本地 mock server 断言请求次数、status 分类、响应内容和 stream sink；不要在单元测试中使用真实 API key 或网络 provider。retry 的测试尤其要区分不可重试 400 和可重试 500，不能只断言最终 error string。

### 4.2 stream event envelope：序号和 node 归属

`foundation/stream-event/tests/stream_event.rs` 以 `EnvelopeState::new("sess-1")` 和 `to_json()` 验证协议 envelope：

- event id 从 1 单调递增；
- 第一个 node 前 `node_id` 为 `run-0`；
- `NodeEnter("think")`、`NodeEnter("act")` 产生 `run-think-0`、`run-act-1`；
- node active 期间的 `TextDelta` 与 `NodeExit` 继续使用当前 node id；
- event 自己或已有 envelope 中的 `session_id`、`node_id`、`event_id` 不会被注入逻辑覆盖。

这定义了调试 stream 时应查看的关联键：`session_id` 定位 session，`node_id` 定位 graph node/run span，`event_id` 定位顺序。若新增 `ProtocolEvent`，应补充序号、node 生命周期和 pre-existing key 的断言。

## 5. Server 与 ACP 的黑盒调试路径

### 5.1 Server router integration

`apps/server/tests/endpoint_integration.rs` 通过 `new_state()`/`build_router()` 构造真实 Axum router，再用 `tower::ServiceExt::oneshot` 发 GET、POST、PATCH、PUT；JSON body 上限为 64 KiB。当前测试以 HTTP contract 为中心：

- `/global/health` 与 `/api/health` 返回完全相同且只有 `{ "healthy": true }`；
- `/global/version` 返回字符串 `version` 和 `kind: "external-kernel"`；
- `/global/dispose` 返回 `ok: true`、`shutdown: true` 并清空 session；
- `/global/config`、`/config` 及 `/api/config` 支持 round-trip patch；未知配置键进入 `extra`；
- `/global/event/replay` 返回 array，并包含已经 emit 的 `session.created`；
- `/global/event` 返回 `200` 和 `Content-Type` 以 `text/event-stream` 开头；
- upgrade 和 instance update 当前明确返回 501；config reload 至少返回 success status 且 body 有 `status` 或 `error`。

配置 patch 的 event 测试还要求产生 `server.config.changed`，不能错误地产生 `server.instance.disposed`。新增 route 时应优先复用这些 in-process helpers，断言 status、JSON shape、header、replay 和副作用；不要用启动真实 server 来测试纯 handler contract。

`apps/server/tests/protocol.rs` 使用临时 TCP stream 读写最小 HTTP/JSON/SSE 响应，验证协议层响应读取和 stream body。它适合覆盖不属于 Axum router 构造的协议边界；mock response 必须明确 `Content-Length` 或 `text/event-stream`，避免把测试死锁误判为业务失败。

### 5.2 ACP harness 的进程、JSON-RPC 与 reverse RPC

`apps/acp/tests/e2e/common/harness.rs` 的 `AcpTestHarness::spawn()` 启动真实 `anureo-acp` binary，设置：

```text
OPENAI_BASE_URL=<llm_url>/v1
OPENAI_API_KEY=test-key
OPENAI_MODEL=openai/gpt-4o
anureo acp --log-file <temp ANUREO_HOME>/anureo-acp.log --log-level info
```

harness 拥有 child 的 stdin/stdout/stderr、`JsonRpcClient`、notification buffer 和 `ReverseRpcResponder`。stdin 通过 unbounded channel 写入，stdout reader 将 response 放入 pending map，将 notification 缓存，并自动回答 agent→client 的 `session/request_permission`、`fs/read_text_file`、`fs/write_text_file`、`terminal/create`、`terminal/output` 和 `terminal/kill`。request 默认 timeout 是 30 秒；等待 graceful shutdown 是 10 秒，超时会 kill child。

调试 ACP test timeout 时，应先看 `dump_log_tail()` 输出的 log file 最后 60 行，再检查 buffered notifications；不要只增加 timeout。`shutdown()` 先关闭 write sender、abort reader task，使 writer 看到 EOF，之后等待 child 正常退出。harness 的 `Drop` 只负责 `start_kill()`，所以需要验证正常退出的测试应显式调用 `shutdown()`。

`apps/acp/tests/e2e/reload.rs` 当前直接 spawn binary：`anureo acp --show-log-dir` 应成功并打印含 `anureo`/`log` 的路径；`anureo acp reload` 在无 PID file 时不应 panic 或 hang，Windows 可以输出 not supported，Unix 在无 PID file 时可以非零退出。这是平台差异，不能把 reload 无 PID 的退出码写成跨平台固定值。

`apps/acp/tests/common/test_setup.rs` 通过临时目录创建 `.anureo/agents` 并设置 `ANUREO_HOME`，Drop 时删除变量。测试并行运行时仍应谨慎：该 helper 本身没有 `agent-core::env_lock()`，跨测试同时改变 `ANUREO_HOME` 可能互相影响。

## 6. 日志、配置与故障定位

`apps/cli/src/logging.rs` 的 CLI 日志优先级是：显式 `--log-file` → `config.toml` 的 `[logging.cli].path` → 启动前捕获的 shell `LOG_FILE` → 默认 `~/.anureo/logs/cli/anureo-cli.log`。`bootstrap` 在加载 `config.toml` 前捕获 shell 环境，因此配置路径存在时会跳过该 shell 路径；由 `config.toml` 的 `[env]` 注入的 `LOG_FILE` 不等同于启动前捕获的 shell `LOG_FILE`。`--log-level` 覆盖 `RUST_LOG`，再回退到 `info`；`--log-format` 支持 `text`/`json`，非法值在当前 `LogArgs::new` 中静默回退为默认的 `text`；rotation 支持 `none`、`daily`、`hourly`、`minutely`，非法值静默回退为默认的 `none`。`{working_folder}` 路径变量由 `resolve_log_path` 解析，写文件时自动创建父目录；没有 CLI/env/config 覆盖时仍写入默认文件，而不是使用丢弃 sink。只有绕过 `resolve_cli_log_path` 并直接以 `None` 初始化时，才会进入 sink 路径。

`apps/acp/src/logging.rs` 的 ACP 日志路径优先级是 CLI file → `LOGS_ACP` → `[logging.acp].path` → `~/.anureo/logs/acp/anureo-acp.log`。ACP stdio 入口在请求循环启动时直接调用 `logging::init_logging(None)`，并由 `OnceLock` 保持只初始化一次的 worker guard；此时相对路径按进程当前工作目录解析。`new_session` handler 是在已启动的连接循环中才调用 `agent.new_session(req)`，因此首个 session 的 `working_folder` 不会参与当前日志初始化。若要让该目录决定路径，必须调整初始化时序，并明确多 session 下 `OnceLock` 的路径选择语义。ACP 的 `text`/`json` 格式和 rotation 也来自当前 `LogConfig`/config。

建议按以下最小信息排查问题：anureo version/commit、入口（CLI/ACP/server/workflow）、effective working directory、session/instance id、model/provider、错误分类和 log path。不要粘贴 API key、cookie、完整环境变量或私有源码。[故障排查指南](../guides/troubleshooting.md) 还要求：模型问题先运行 `anureo models`，ACP 可用 `anureo acp --show-log-dir`，workflow 先查看 `workflow_status`，失败时只查询必要的 `agent_done`/`run_done` 事件。

## 7. Web e2e 与 Playwright

`e2e/package.json` 的脚本是：

```powershell
Set-Location e2e
npm test
npm run report
npm run ui
```

这些命令必须在 `e2e` 目录执行，分别运行 `e2e/package.json` 中的 `npx playwright test`、`npx playwright show-report` 和 `npx playwright test --ui`。若保持在仓库根，也可使用 `npm --prefix e2e test`、`npm --prefix e2e run report`、`npm --prefix e2e run ui`。这只改变 npm script 的工作目录；`e2e/playwright.config.ts` 的 Playwright `webServer` 仍明确使用仓库根作为 cwd。该配置默认 `E2E_BASE_URL=http://localhost:3000`，`CI` 时 retries 为 2、本地为 0，workers 固定为 1 以避免 shared localStorage/React state 不稳定；CI 使用 HTML + GitHub reporter，本地失败时打开 HTML report。默认 trace 为 first retry、失败保留 screenshot/video；action timeout 10 秒、navigation timeout 15 秒。

默认 web server 是仓库根的 `node packages/web/bin/cli.js serve --foreground`，timeout 120 秒；`E2E_NO_AUTOSTART=1` 时不自动启动外部 server。源码注释还说明 `E2E_NODE_FALLBACK=1` 的 node fallback 语义，但 Playwright config 本身未根据该变量建立另一 command，不要把它描述为已自动生效的配置分支。项目分为 Desktop Chrome 和 Mobile Chrome；mobile 使用 `mobile.spec.ts`，desktop 排除该文件。

`e2e/tests/web/smoke.spec.ts` 当前验证的用户级事实包括：应用不是白屏、可新建 session、发送 `Hello, what can you do?` 后消息可见或输入框被清空、刷新后侧栏恢复、零 session 显示空状态或侧栏内容。测试使用 `waitUntil: "commit"`、显式等待 sidebar/input，并允许 send button 不可见时用 Enter fallback。新增 UI 测试应优先等待可观察 UI 状态，不要用固定 sleep 代替必要的 locator assertion；现有代码中少量 `waitForTimeout(500)` 只属于该 smoke 的兼容处理。

## 8. 扩展点与常见坑

### 扩展点

- 新增环境相关测试：复用 `env_lock()`/`with_env()`，并恢复所有变量。
- 新增 Tool action：在 owning tool crate 里覆盖 success、invalid input、rollback 和结构化 response。
- 新增 workflow 状态：同时更新 runtime registry、checkpoint/instance artifact、status tool 与 cancel/resume 测试。
- 新增 LLM provider 行为：用本地 TCP mock 覆盖 status、retry、response parse 和 stream sink。
- 新增 protocol event：验证 envelope 注入不覆盖已有字段，并保持 event/node 序列。
- 新增 server route：先用 `build_router` + `oneshot` 做 in-process contract test，再按需补真实协议测试。
- 新增 ACP capability：扩展 reverse RPC responder、notification drain 和 shutdown 断言；不要把测试用的自动应答误当成真实 IDE 行为。
- 新增日志选项：同时核对 CLI 参数、`config.toml`、启动前 shell env 和默认路径的优先级，并保持 stdout 协议干净。

### 常见坑

- 把 `cargo test -p anureo-cli` 当成 workspace 全量测试；它不会替代 workflow、server、ACP 和 Web e2e。
- 在并行测试中直接修改 `ANUREO_HOME` 或 `OPENAI_BASE_URL`，造成 flaky。
- 只断言 workflow resume 成功，不断言 `CountingBackend` dispatch count，漏掉重复执行。
- 将 `parallel_mapper.rs`/`terminal_events.rs` 的旧 `WorkflowTool` 当成当前 API。
- 把 `instance_smoke` 未设置 fixture 时的 return 当成通过；它需要 `ANUREO_TEST_INSTANCES_DIR` 和指定 instance 目录。
- 把 cancel 的 `cancelling` 当成 workflow 已完成；它只是 registry 接受取消请求，terminal status 需另查。
- 把 server 501 的 upgrade/instance update 写成待实现却未标实验性；当前测试明确它们返回 NOT_IMPLEMENTED。
- 把 ACP 无 PID reload 的退出码固定为 0 或 1；源码允许 Windows/Unix 差异。
- 在 OpenAI 单测里访问真实网络或凭据；当前测试已经提供 TCP/HTTP/SSE mock。
- 把 `event_id`、`node_id` 注入覆盖已有字段；stream-event 测试明确禁止覆盖。
- 认为配置中的 `LOG_FILE` 与 CLI 一样高优先级；CLI logging 源码特别区分 shell env 与 config `[env]`。
- 只看最后一条 stream event，不看 event sequence、completion、HTTP status 或日志 tail。

## 9. 最小测试与调试流程

1. 先确定 owning crate、入口、working folder、session/instance id 和实际 log path。
2. 先运行最小目标测试：Tool 用 crate integration，LLM/stream 用 foundation test，server 用 router test，ACP 用 harness，Web 用 Playwright smoke。
3. 涉及环境变量时使用锁和临时目录；涉及 provider 时使用 local mock；涉及 workflow resume 时记录并断言 dispatch count。
4. 若是协议问题，分别检查 JSON shape、HTTP status/header、SSE body、ACP notification/reverse RPC 和 stream envelope，不要把不同边界混成一个错误。
5. 读取 CLI/ACP log tail、必要的 replay/event，并按 troubleshooting 指引脱敏后生成最小复现。
6. 相关测试稳定后运行 `cargo check --workspace`、`cargo test --workspace`；Web e2e 从仓库根运行 `npm --prefix e2e test`（或先进入 `e2e` 再运行 `npm test`），并依据 `E2E_BASE_URL`/`E2E_NO_AUTOSTART` 检查 server 来源。

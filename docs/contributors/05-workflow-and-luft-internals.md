# Workflow 与 Luft internals：贡献者源码导读

> **状态：实验性**
>
> 本文描述当前源码，不是 workflow DSL 或 Luft 的稳定兼容性承诺。`agent/tool/tool-workflow` 在 anureo workspace 中已注册并有测试，但仓库仍把 workflow 标为实验性；修改时必须以源码和测试为准，不能把设计文档、旧 `WorkflowTool` 测试或注释中的目标状态当作 API。
>
> **目标读者**：第一次修改 anureo workflow、Tool 或 Luft adapter 的 Rust 贡献者。
>
> **源码范围**：anureo `agent/tool/tool-workflow`；其 Cargo 依赖为 `luft = "0.4"`、`luft-core = "0.4"`、`luft-service = "0.4"`（`agent/tool/tool-workflow/Cargo.toml`）。Luft facade 的实现位于相邻仓库 `C:/Users/heycj/dev/luft/crates/luft/src/builder.rs`。本文只修改 anureo 文档；Luft 源码是本地源码证据，不是本文的修改目标。

## 1. 先建立边界

一次 workflow run 横跨四层：

```text
Agent Tool registry
  -> WorkflowStartTool / StatusTool / ...
  -> service.rs（参数、路径、Luft、响应）
  -> WorkflowRuntime（共享 config、取消 registry、实例布局）
  -> LuftBuilder -> RunHandle -> Lua/Luft runtime -> anureoAgentBackend
  -> checkpoint.json / events.jsonl / workflow.lua / instance.json
```

入口和职责不要混淆：

| 层 | 当前职责 | 修改入口 |
| --- | --- | --- |
| Tool wrapper | 暴露 `Tool` trait、name/spec、输入 schema、builtin skill | `src/tool_start.rs`、`tool_status.rs`、`tool_cancel.rs`、`tool_list.rs`、`tool_events.rs`、`tool_source.rs`、`tool_files.rs` |
| service | 解析输入、选择 workflow、构建 Luft、后台等待、格式化 JSON、读取实例 | `src/service.rs` |
| runtime | 持有 `AgentConfig` 模板、`Arc<Mutex<...>>` 的 active-run registry、路径和 finalize/rebuild | `src/runtime.rs` |
| backend | 将 Luft agent 请求接回 anureo agent 配置/模型能力 | `src/backend.rs` |
| Luft | 负责 Lua workflow 执行、RunHandle、事件、取消和 resume facade | `C:/Users/heycj/dev/luft/crates/luft/src/builder.rs` 及其内部 crates |

`src/lib.rs` 的 `register_workflow_tools` 和 `default_workflow_tool_provider` 为同一个 `Arc<WorkflowRuntime>` 注册七个工具：`workflow_start`、`workflow_cancel`、`workflow_status`、`workflow_list`、`workflow_events`、`workflow_source`、`workflow_files`（`src/lib.rs:18-74`）。不要把每个 app 入口各自注册一份，也不要依据旧注释把它写成六个工具。

## 2. 从 Tool 到后台 run 的真实调用链

### 2.1 注册与共享状态

`default_workflow_tool_provider()` 接收 `ReactBuildConfig`，复制为 `AgentConfig`，创建一个 `WorkflowRuntime`，再把七个 `Arc<dyn Tool>` 返回给 Agent（`src/lib.rs:44-74`）。因此，同一 Agent run 内的 workflow tool 共享 active-run registry；它不是跨进程服务，也不是持久化的取消队列。

`WorkflowRuntime` 的 `active_runs` 是 `Arc<Mutex<HashMap<String, Arc<CancellationToken>>>>`（`src/runtime.rs:22-32`）。`register_run` 在后台 finalize task 启动前登记 run，`unregister_run` 在 task 返回后删除；锁 poisoned 时当前实现会 `expect`，贡献者若改并发语义必须补测试和错误策略，而不能悄悄换成跨进程假设（`src/runtime.rs:35-82`）。

### 2.2 `workflow_start`

`WorkflowStartTool::call` 只是委托 `service::start_workflow`（`src/tool_start.rs:111-119`）。service 的顺序是：

1. 读取调用深度；`depth >= 3` 直接返回 `ToolError`，防止 workflow 无限嵌套（`src/service.rs:33-45`）。
2. 从 `resume_from_id`、兼容别名 `instance`/`instance_dir`、`script`、`workflow` 取输入；resume 与 script/workflow 互斥，三者都缺失也报 `InvalidInput`（`src/service.rs:47-116`）。
3. 对 resume id 做 instance-name 校验；对文件 workflow 调用 `resolve_workflow`，然后读取 Lua 源码。搜索顺序是：绝对 `.lua` 文件、项目 `.anureo/workflows/<name>.lua`、用户 `~/.config/anureo/workflows/<name>.lua`、working folder 下的 `<name>.lua`、最后是传入路径（`src/workflow_resolver.rs:3-55`）。
4. 用 `params::parse_concurrency` 解析并限制 `1..=64`；缺省值是 `4`（`src/service.rs:27、120-123`；schema 在 `src/tool_start.rs:64-98`）。`args` 通过 `params::inject_args_globals` 注入 Lua 的 `_G._args`，只适用于 fresh run。
5. 用 `runtime.instances_root()` 创建 `.anureo/instances`，构造 `anureoAgentBackend`，再调用 `LuftBuilder::new().backend(...).base_dir(...).concurrency(...).build()`（`src/service.rs:126-158`）。
6. fresh run 调用 `luft.start_script(&lua_source)`；resume 调用 `luft.start_resume(id)`。两者都返回 `RunHandle`，错误被转换为 `ToolSourceError::ToolError`（`src/service.rs:160-193`）。
7. 取得 `run_dir_name`，登记取消 token，spawn `background_finalize`，立即返回 `{instance_dir, status: "running"}`；resume 额外返回 `resumed_from`（`src/service.rs:195-250`）。这就是后台启动，不是完成结果。

Luft facade 的对应证据是：`LuftBuilder` 的 builder 方法在 `crates/luft/src/builder.rs:34-119`，`start_script`/`start_resume` 在 `:254-283`，`RunHandle` 的目录名、订阅、取消和 `join` 在 `:418-463`。修改 anureo adapter 时，先确认这些方法的返回/生命周期，再决定是否需要改变 service。

### 2.3 后台 finalize、事件和取消

`background_finalize` 同时监听：取消 oneshot、`RunHandle::subscribe()` 的 Luft 事件、`join()`、以及每 100ms 对 terminal checkpoint 的检查（`src/service.rs:254-365`）。收到 `RunDone` 后映射 `Completed/Failed/Cancelled/Partial`；broadcast lag 会继续接收，closed 且没有终态时视为 failed/cancelled。随后调用 `WorkflowRuntime::finalize`；若 finalize 失败，写入最小 failed `instance.json`（`src/service.rs:369-394`）。

取消是进程内、宽松的信号：`workflow_cancel` 查 `active_runs`，命中时只返回 `result="cancelling"`；finalize loop 再调用 `run_handle.cancel()`，并等待 agent 当前 turn 的取消边界。run 已终止、由别的进程拥有或不在 registry 时返回 `not_found_or_terminal`（`src/service.rs:496-551`；`src/runtime.rs:49-72`）。因此不能把 `cancelling` 当作 terminal `cancelled`，也不能承诺跨进程取消。

## 3. Luft facade 与持久化边界

anureo 不直接实现 Lua 调度；它把 `base_dir` 指向 `.anureo/instances`，把 anureo 的 `AgentConfig` 包装成 `anureoAgentBackend`。Luft 的 `RunHandle` 是“启动后观察”的句柄：anureo service 持有它、订阅事件并在后台 finalize；Tool 调用者只拿到实例目录名。

Luft 外部 API 与 anureo adapter 的边界如下：

| 行为 | Luft 侧 | anureo 侧可见行为 |
| --- | --- | --- |
| fresh run | `start_script` 返回 `RunHandle` | `workflow_start` 立即返回 running receipt |
| resume | `start_resume(run_dir)` | 只能传 instance id；不可与 script/workflow 同时传 |
| 事件 | `RunHandle::subscribe` / `RunDone` | 可选的 stream callback，加工后由 `workflow_events` 读取落盘 JSONL |
| cancel | `RunHandle::cancel` | 先由 `WorkflowRuntime` registry 命中，再由 finalize loop 调用 |
| agent 请求 | Luft backend trait | `anureoAgentBackend` 使用 anureo 的 Agent/config/model 边界 |
| checkpoint/history | Luft 内部运行状态与 anureo 的实例目录 | status/list/resume 依赖 checkpoint；详细历史不可由 `instance.json` 单独推导 |

当前 resume 的语义来自 workflow skill 与测试：Lua 脚本会重新执行，但已完成 agent/phase 可由 journal cache 命中；中断 agent 的对话历史由 `SqliteSaver` 恢复。`completed` 和 `cancelled` 是不能 resume 的终态，`failed`/crashed run 才是恢复场景（`src/workflow_skill.md:73-111`；`tests/resume_basic.rs`、`resume_crash.rs`、`cancel_resume.rs`）。这段语义属于当前 Luft 0.4 集成，改动时必须用 dispatch-count 测试证明没有重复调用。

## 4. 路径、实例产物与公开视图

`WorkflowRuntime` 从 `AgentConfig.working_folder` 计算路径；缺失时使用当前目录 `.`（`src/runtime.rs:84-118`）：

```text
<working_folder>/.anureo/instances/<instance_dir>/   # 当前目录
<working_folder>/.luft/runs/<instance_dir>/        # legacy fallback
<working_folder>/.anureo/workflows/                  # workflow 搜索目录
```

当前 finalize 读取 `checkpoint.json`，解析 `events.jsonl`，读取 `workflow.lua`，构造 `InstanceMeta`，然后写 `instance.json` 及必要的报告/大输出引用；checkpoint 尚未就绪时最多重试 10 次、每次等待 200ms（`src/runtime.rs:134-246`）。不要把 fixture 中的字段当成稳定文件格式；它们是当前 runtime 的观测材料。

status 的读取顺序是：优先 `instance.json`；没有时从 `checkpoint.json` 重建 summary；新目录存在但没有 terminal checkpoint 时返回 running；legacy 目录缺 checkpoint 则报 incomplete/error（`src/service.rs:397-489`）。list 同时扫描 current 和 legacy roots，但只保留 terminal checkpoint/summary。events、source、files 工具也会先校验 instance name，再通过 runtime 解析目录（`src/service.rs:574-845`）。

所有用户可见 summary 都经过 `sanitize_instance_for_public`；当前会移除内部 checkpoint hash、绝对路径和内部 reference，并对 source/report/agent output 做预览或大小限制（`src/common.rs:9-55`；`src/runtime.rs:185-224`）。新增字段时先决定它是否是公开 contract，禁止直接把原始 checkpoint、日志或 secret 透传给 Tool。

instance name 的安全边界是单个目录名：空值、`.`/`..`、包含 `/` 或 `\\` 的值被拒绝（`src/common.rs:9-29`）。读取和 resume 都必须复用此校验；不要用字符串拼接自行绕过 `resolve_instance_path` 的 current/legacy 规则。

## 5. 修改 workflow 时的最小路径

按行为选择 owning code：

| 需求 | 首先检查 | 同步更新 |
| --- | --- | --- |
| 新输入或输出字段 | 对应 `tool_*.rs` schema + `service.rs` | invalid input、sanitization、工具 contract 测试 |
| 新状态/终态 | `runtime.rs`、`service.rs`、`instance.rs` | checkpoint 映射、status/list/events、resume/cancel 测试 |
| 新 agent 调度能力 | `backend.rs` 与 Luft API | 不要在 CLI/server 复制 agent 调用；补 mock backend 和 dispatch count |
| 新 workflow 文件发现规则 | `workflow_resolver.rs` | 顺序、绝对路径、not-found 测试；同时检查路径安全 |
| 新工具 | `lib.rs` provider/registry 注入点 | Tool name/spec、builtin skill requires_tools、契约和集成测试 |
| 新 DSL 语义 | `src/workflow_skill.md` 与 `references/`，再追 Luft implementation | 标注实验性；先证明 Lua 执行、并发、resume 和持久化语义 |

不要把 `WorkflowStartTool` 改成同步等待；调用方必须先等待，再调用 `workflow_status`。PowerShell 使用 `Start-Sleep -Seconds 5`，普通 shell 使用 `sleep 5`；sleep 与 status 查询不能放进并行 batch（`src/workflow_skill.md:61`、`references/tool-usage.md:35-42`）。

## 6. 验证矩阵

最小验证从快到慢：

```text
cargo test -p tool-workflow
cargo test -p tool-workflow --test cancel_basic -- --nocapture
cargo test -p tool-workflow --test cancel_resume -- --nocapture
cargo test -p tool-workflow --test resume_basic -- --nocapture
cargo test -p tool-workflow --test instance_smoke -- --nocapture   # 仅在设置 fixture 目录时
```

重点断言而不是只看最终字符串：

- start receipt 是 `running`，最终状态要由 status 读取；
- `concurrency` 的边界、默认 4 和 working folder 传递；
- cancel 首次返回 `cancelling`，随后 status 才变成 `cancelled`；重复 cancel 和未知 id 的行为；
- resume 的 completed-agent dispatch count，证明 journal/cache 命中而非重复 LLM 调用；
- `instance.json`、`checkpoint.json`、`events.jsonl` 缺失或损坏时的错误/降级；
- absolute path、legacy `.luft/runs` fallback、目录穿越输入和公开视图清洗。

`instance_smoke` 受 `ANUREO_TEST_INSTANCES_DIR` 等 fixture 环境影响；没有设置时不要把“跳过”解释成 runtime 不支持。涉及模型请求应使用 mock backend，不使用真实 API key 或网络 provider。仓库的 CI/release workflow 与本地 test 命令是两件事，提交说明应分别记录实际执行的命令和结果。

## 7. 常见误读

- 把 `workflow_start` 的 receipt 当作完成结果；它只证明 Luft 已接受启动。
- 把 `cancelling` 当作 `cancelled`；前者是 registry 已发信号，后者需要 checkpoint/status 终态。
- 认为 active-run registry 或 cancel 能跨进程；它是 `Arc<Mutex<HashMap<...>>>`，只属于当前 `WorkflowRuntime`。
- 认为所有历史都在 `instance.json`；resume 还依赖 checkpoint/journal/SQLite history。
- 看到 `.luft/runs` 就把它写成当前唯一布局；当前优先 `.anureo/instances`，`.luft/runs` 是 legacy fallback。
- 看到旧测试中的 `WorkflowTool` 就恢复旧 API；当前公开的是七个拆分后的 tool。
- 依据 Luft 或 anureo 的设计文档推导未实现的稳定 API；先找到对应 Rust consumer、Tool schema 和测试。

## 8. 提交前 checklist

- [ ] 从 Tool wrapper 追到 `service`、`WorkflowRuntime`、`LuftBuilder`、`RunHandle` 和磁盘产物。
- [ ] 明确改动影响的是 anureo adapter 还是 Luft crate；本仓库修改不应偷偷改变另一个仓库。
- [ ] 保留 `1..=64` / 默认 4、depth 上限 3、路径校验和公开视图清洗，除非行为变更有对应测试和说明。
- [ ] 对 start/status/cancel/resume 分别验证 receipt、terminal state、错误边界和 dispatch count。
- [ ] 文档、Tool description、fixture 和测试没有把实验性行为描述成稳定兼容性承诺。
- [ ] 提交中写明源码路径、调用链、测试命令、结果和仍未解决的实验性风险。


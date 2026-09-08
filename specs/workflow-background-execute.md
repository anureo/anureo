# Workflow Background Execute

## 目标

将原来的单一 `workflow` action-dispatch 工具拆成六个职责单一的工具：

- `workflow_start`
- `workflow_status`
- `workflow_list`
- `workflow_events`
- `workflow_source`
- `workflow_files`

`workflow_start` 启动后立即返回；后台任务继续等待、收尾和生成终态；调用方使用 shell 的 `sleep 5` 或 PowerShell `Start-Sleep -Seconds 5`，再调用 `workflow_status` 轮询。

## 约束

- 不新增 `sleep` 工具
- 不新增 `status.json` 或其他状态文件
- 不增加 `cancel` action/tool
- 不保留 `workflow`、`run`、`list-runs`、`run-status` legacy alias
- 不要求 LLM 使用文件读取工具
- LLM-facing 返回中不暴露绝对路径、源文件路径、`report_ref`、`output_ref` 或存储文件名

## 公开工具契约

### `workflow_start`

输入：

- `script` 或 `workflow` 二选一
- `args` 可选，注入 Lua 的 `_G._args`
- `concurrency` 可选，范围 `1..=64`，默认 `4`

启动成功立即返回：

```json
{
  "instance_dir": "anureo-instance_xxx",
  "status": "running"
}
```

工具描述必须明确：

1. 该调用不会等待 workflow 完成；
2. 使用 shell 执行 `sleep 5`；
3. 再调用 `workflow_status(instance_dir=...)`；
4. 只有 `status == "running"` 时才重复；
5. 不要进行紧密轮询，也不要将 sleep 和 status 查询放入并行 batch。

### `workflow_status`

输入：`instance_dir`。

运行中返回：

```json
{
  "instance_dir": "anureo-instance_xxx",
  "status": "running"
}
```

终态返回完整的公开 summary，`status` 为 `completed`、`failed` 或 `cancelled`，包含 workflow 名称、agent 摘要、token、phase timing、event statistics 和有界 report 内容。

终态 summary 必须经过公开数据清洗：

- 删除 `workflow.path`
- 删除 agent 的 `output_ref`
- `ReportRef::File` 只保留 `preview`，删除 `ref`
- 删除 `checkpoint_hash` 等纯内部字段
- 不返回任何绝对路径或存储文件名

### `workflow_list`

列出已完成的 workflow 实例，支持 `limit`、`cursor`、`status_filter`。

返回 instance identifier、status、workflow name、时间、token 总数和 agent 数量；不返回 source 标签或 filesystem path。运行中的实例不出现在列表中，应继续使用 `workflow_start` 返回的 identifier 查询。

### `workflow_events`

按 `instance_dir` 分页读取事件，支持 `offset`、`events_limit`、`types` 和 `agent_id` 过滤。该工具用于终态后的调查，不用于替代 `workflow_status` 轮询。

### `workflow_source`

按 `instance_dir` 返回实例捕获的 Lua source 内容和 `truncated` 标记。内容由工具直接提供，不返回 source reference 或路径。

### `workflow_files`

无参数，列出可用于 `workflow_start` 的 Lua workflow definitions，返回名称、大小和首个非空行。它不是实例结果查询工具。

## 代码改动

### 1. 模块结构

**目录**：`agent/tool/tool-workflow/src/`

采用两层架构：接口层（`tool_*.rs`）+ 实现层（`service.rs`）。

```
src/
  service.rs         实现层：六个 handler 函数 + 所有 helper + 单元测试
  tool_start.rs      接口层：WorkflowStartTool (Tool impl + builtin_skill)
  tool_status.rs     接口层：WorkflowStatusTool (Tool impl)
  tool_list.rs       接口层：WorkflowListTool (Tool impl)
  tool_events.rs     接口层：WorkflowEventsTool (Tool impl)
  tool_source.rs     接口层：WorkflowSourceTool (Tool impl)
  tool_files.rs      接口层：WorkflowFilesTool (Tool impl)
  runtime.rs         WorkflowRuntime 共享上下文 (路径布局、finalize、rebuild_summary)
  common.rs          跨工具 helper (sanitize、truncate、validation、terminal checks)
```

**runtime.rs** 持有共享运行时上下文，避免六个工具重复配置和路径逻辑：

```rust
#[derive(Clone)]
pub(crate) struct WorkflowRuntime {
    pub(crate) config_template: agent::agent::AgentConfig,
}
```

**tool_*.rs** 中的六个公开 struct 各持有 `Arc<WorkflowRuntime>`，`call()` 仅一行委托：

```rust
pub struct WorkflowStartTool { pub(crate) runtime: Arc<WorkflowRuntime> }
pub struct WorkflowStatusTool { pub(crate) runtime: Arc<WorkflowRuntime> }
pub struct WorkflowListTool { pub(crate) runtime: Arc<WorkflowRuntime> }
pub struct WorkflowEventsTool { pub(crate) runtime: Arc<WorkflowRuntime> }
pub struct WorkflowSourceTool { pub(crate) runtime: Arc<WorkflowRuntime> }
pub struct WorkflowFilesTool { pub(crate) runtime: Arc<WorkflowRuntime> }
```

```rust
// tool_status.rs — call() 仅委托到 service
async fn call(&self, args: Value, _: Option<&ToolCallContext>) -> Result<..> {
    crate::service::read_status(&self.runtime, &args).await
}
```

**service.rs** 集中所有业务逻辑：

```rust
pub(crate) async fn start_workflow(runtime, args, ctx) -> Result<..>  // tool_start
pub(crate) async fn read_status(runtime, args) -> Result<..>          // tool_status
pub(crate) fn list_instances(runtime, args) -> Result<..>             // tool_list
pub(crate) fn read_events(runtime, args) -> Result<..>               // tool_events
pub(crate) fn read_source(runtime, args) -> Result<..>               // tool_source
pub(crate) fn list_workflow_files(runtime) -> Result<..>             // tool_files
```

六个工具统一使用：

```rust
ToolOutputHint::preferred(ToolOutputStrategy::Inline)
```

### 2. `workflow_start` 后台化

对应 `service::start_workflow` 的启动链路：

1. 校验 `script`/`workflow`、depth、args 和 concurrency；
2. 构建 Luft；
3. `start_script().await`；
4. 获取 `run_dir_name`；
5. 克隆 `WorkflowRuntime` 和所有需要的 owned 参数；
6. `tokio::spawn(background_finalize(...))`；
7. 立即返回 `{instance_dir, status: "running"}`。

以下逻辑全部放进后台 task：

- 等待 `RunDone`；
- 接收父级 cancellation 并调用 `run_handle.cancel()`；
- 等待 `run_handle.into_future().await` 完成 drain；
- 读取 checkpoint、events 并生成终态摘要；
- 写入终态实例数据。

事件转发也由后台 task 持续执行，不能因为 `workflow_start` 已返回而停止。

### 3. 快速完成和事件竞态

不能只依赖 `RunDone` broadcast 事件，因为 workflow 可能在后台 task 建立 receiver 前完成。

`service::background_finalize` 除了等待事件，还每 100ms 检查一次终态 checkpoint：

```rust
loop {
    tokio::select! {
        event = done_rx.recv() => { ... }
        _ = cancel_token.cancelled(), if !cancelled => {
            cancelled = true;
            run_handle.cancel();
        }
        _ = tokio::time::sleep(Duration::from_millis(100)) => {
            if let Some(status) = runtime.terminal_checkpoint_status(&run_dir_name).await {
                final_status = Some(status);
                break;
            }
        }
    }
}
```

`broadcast::RecvError::Lagged` 必须继续消费，不能把事件量大误判为 workflow failed；只有 `Closed` 才进入失败/取消收尾。

### 4. finalize 重构

当前 `finalize` 仍是 `WorkflowRuntime` 的 async 方法，通过后台 task 捕获 owned 的 `Arc<WorkflowRuntime>` 满足 `'static`，不新增 `tools.rs`。

修改内容：

- 返回类型从 `Result<String, ToolSourceError>` 改为 `Result<(), ToolSourceError>`；
- 不再构建包含 `report_ref` 的 compact summary；
- checkpoint 重试从 `std::thread::sleep` 改为 `tokio::time::sleep`；
- checkpoint/events 读取使用 async I/O；
- `write_instance_artifacts` 失败必须返回 `Err`，不能只打印 warning；
- `final_status` 覆盖生成的 `InstanceMeta.status`，保证取消和失败状态不会被 checkpoint 的旧状态覆盖。

### 5. finalize 失败兜底

后台 task 必须接住 `finalize` 的 `Err`。否则 finalize 失败后没有终态数据，`workflow_status` 会一直将实例视为 running，导致调用方无限轮询。

```rust
match runtime
    .finalize(
        &run_dir_name,
        final_status,
        final_report.as_ref(),
        &display_name,
        is_inline_script,
        workflow_arg_owned.as_deref(),
    )
    .await
{
    Ok(()) => {}
    Err(error) => {
        runtime.write_failed_instance(&run_dir_name, &error);
    }
}
```

`write_failed_instance` 写入最小公开可读终态：

```json
{
  "schema_version": 1,
  "instance_dir": "anureo-instance_xxx",
  "status": "failed",
  "error": "finalize failed"
}
```

如果 fallback 写入本身失败，属于磁盘或权限故障，不在本次 orphan recovery 范围内。

### 6. `workflow_status` 状态推断

不增加状态文件，使用已有实例数据判断：

```text
1. instance.json 存在
   -> 读取并清洗，返回终态 summary

2. instance.json 不存在，但 checkpoint.json 存在
   -> 使用 checkpoint + events 重建并返回终态 summary

3. 当前实例目录存在，但两个终态数据都不存在
   -> 返回 {instance_dir, status:"running"}

4. legacy 目录存在但没有 checkpoint
   -> 返回 incomplete/not found，不误判为 running
```

当前实例与 legacy 实例都支持查询；公开响应不暴露它们的物理来源。

### 7. 公开 summary 清洗

新增/保留 `sanitize_instance_for_public(Value)`，用于 `workflow_status` 的完整 summary 和 checkpoint 重建结果：

```rust
fn sanitize_instance_for_public(mut value: Value) -> Value {
    if let Some(workflow) = value.get_mut("workflow").and_then(Value::as_object_mut) {
        workflow.remove("path");
    }
    if let Some(agents) = value.get_mut("agents").and_then(Value::as_array_mut) {
        for agent in agents {
            if let Some(agent) = agent.as_object_mut() {
                agent.remove("output_ref");
            }
        }
    }
    if let Some(report) = value.get_mut("report").and_then(Value::as_object_mut) {
        report.remove("ref");
    }
    value.as_object_mut().map(|object| {
        object.remove("checkpoint_hash");
    });
    value
}
```

内部落盘结构可以继续保留完整 reference；清洗只作用于返回给 LLM 的 JSON。

### 8. `workflow_files`

原 `list-workflows` 逻辑改为列出可启动的 Lua definitions：

- 无 `instance_dir` 参数；
- 扫描 workflow definitions；
- 只返回 name、size_bytes、first_line、count；
- 不返回 definitions directory path。

### 9. 注册和常量

在 `agent/tool/tool-core/src/tool_name.rs` 增加：

```rust
pub const TOOL_WORKFLOW_START: &str = "workflow_start";
pub const TOOL_WORKFLOW_STATUS: &str = "workflow_status";
pub const TOOL_WORKFLOW_LIST: &str = "workflow_list";
pub const TOOL_WORKFLOW_EVENTS: &str = "workflow_events";
pub const TOOL_WORKFLOW_SOURCE: &str = "workflow_source";
pub const TOOL_WORKFLOW_FILES: &str = "workflow_files";
```

`agent/tool/tool-workflow/src/lib.rs`：

- 导出六个独立工具；
- 声明 `mod service;`（实现层）和 `mod runtime;`（共享上下文）；
- `default_workflow_tool_provider()` 返回六个工具；
- `register_workflow_tools()` 注册六个工具；
- 删除 `WorkflowTool` export；
- 只有 `WorkflowStartTool` 提供 builtin workflow skill；
- skill `requires_tools` 至少包含 `workflow_start` 和 `workflow_status`。

### 10. 删除 alias

删除：

- `run` → `execute`
- `list-runs` → `list-instances`
- `run-status` → `instance-summary`
- `with_deprecation`
- 旧 action enum/分派
- `WorkflowTool` 兼容 shim

### 11. 文档和测试

更新：

- `agent/tool/tool-workflow/src/workflow_skill.md`
- `agent/tool/tool-workflow/src/references/tool-usage.md`
- `docs/design/workflow-instance-model.md`

文档只引导调用六个工具，不引导 LLM 读取内部文件。

测试覆盖：

- 六个工具名称、schema、Inline output hint；
- `workflow_files` 无参数并返回 definitions；
- `workflow_start` 返回 running receipt，并在终态出现前立即返回；
- `workflow_status` 返回 running 后最终返回 terminal summary；
- 目录存在但终态数据不存在时 status 返回 running；
- checkpoint-only 实例重建为终态；
- legacy 无 checkpoint 不误判为 running；
- finalize failure fallback 返回 failed；
- parent cancellation 传播；
- `RecvError::Lagged` 不误判失败；
- status response 不包含路径、`report_ref` 或 `output_ref`；
- 删除旧 alias 测试；
- builtin skill 只绑定 `WorkflowStartTool`。

## 验证

```powershell
cargo fmt --check
cargo test -p tool-workflow
cargo clippy -p tool-workflow --all-targets -- -D warnings
cargo check --workspace
```

当前已验证：

```text
cargo test -p tool-workflow                 PASS
cargo test -p tool-workflow --test builtin_skill  PASS
```

`cargo clippy -p tool-workflow --all-targets -- -D warnings` 当前被仓库已有的 `agent/agent-core/src/run/types.rs:51` 文档 lint 阻塞，不是 `tool-workflow` 的编译错误。

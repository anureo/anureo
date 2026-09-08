# Workflows（实验性）

> **实验性功能警告**：anureo workflow 是由 Agent tools 驱动的 Lua 多 Agent 编排能力，不是独立的 `anureo workflow` CLI 子命令，也不是生产级 scheduler。它会启动真实的 Agent，可能读写项目文件、调用 shell/MCP 或产生其它外部副作用。请只在 disposable branch 或 disposable worktree 中试用，并在接受结果前检查实际 diff、测试和外部状态。

本文面向已经能运行 anureo Agent、希望把复杂任务拆成多个 Lua 阶段和 Agent 的开发者。普通 Agent session、`goal`、`task`、memory/review/curator 不在本文展开。

## 1. 先确认运行边界

Workflow tools 由 Agent 运行时注册；当前源码注册了以下七个 tool：

| 目的 | tool | 必填输入 |
| --- | --- | --- |
| 启动新 workflow | `workflow_start` | `script` 或 `workflow` |
| 恢复失败/中断的 workflow | `workflow_start` | `resume_from_id` |
| 取消当前进程拥有的运行 | `workflow_cancel` | `instance` |
| 查找已结束的 instance | `workflow_list` | 无；可选分页/状态过滤 |
| 查看一个 instance 状态 | `workflow_status` | `instance` |
| 查看详细事件 | `workflow_events` | `instance` |
| 查看 captured Lua source | `workflow_source` | `instance` |
| 列出可用 Lua 定义 | `workflow_files` | 无 |

工具参数名以当前 tool schema 为准：status/events/source 的实现也接受文档中称为 `instance_dir` 的 instance 标识；新调用优先使用 schema 中的 `instance`。这些名称是 Agent tool calls，不应改写成假想的 `anureo workflow ...` 命令。

开始前完成以下检查：

1. 从正确的项目目录启动 anureo，并确认 Agent 的 working folder 是预期项目根目录。
2. 记录当前分支、`git status --short` 和已有 diff；workflow 不会替你创建隔离 worktree。
3. 确认模型凭据、MCP、shell 和文件权限；workflow 中的 `agent()` 会继承运行时配置。
4. 先用小型、无破坏性的 workflow 验证 tool 可见性和输出，再交给它修改文件。

**不确定性**：workflow tool 是否出现在某个具体入口，取决于该入口是否加载默认 workflow tool provider；以该运行中的 tool list 和 `--help` 为准。源码已核对 CLI、ACP 和相关 Agent 运行路径的 provider 接线，但这不等于所有第三方宿主都会提供这些 tools。

## 2. Workflow 的执行模型

Lua 只是 orchestrator。脚本负责 `phase`、`agent`、`pipeline`、`parallel` 和 `report` 等调度；真正的文件、shell、MCP 或其它操作发生在 Agent prompt 所触发的 Agent turns 中。Lua sandbox 禁用 `io`、`os` 和 `require`，不要把脚本当作 shell 或通用文件脚本。

`workflow_start` 是异步的：立即返回 `instance_dir` 和 `status: "running"`，不等待 Agent 完成。公开生命周期以 `workflow_status` 为准；结束状态为 `completed`、`failed` 或 `cancelled`。`report(value)` 是 workflow 的结果，但 Agent 报告完成不代表代码正确。

默认最大并发 Agent 数是 `4`；`concurrency` 范围是 `1..=64`。嵌套 workflow 的最大深度是 `3`。新运行可以传 `args`；当前实现把它注入脚本的 `_G._args`（新运行适用，resume 不重新接受新的 source/args）。

最小 Lua 结构：

```lua
meta = {
  reasoning = "Split the task into inspect, change, and verify",
  phases = {
    { label = "inspect" },
    { label = "verify" },
  },
}

local RESULT = {
  type = "object",
  properties = { summary = { type = "string" } },
  required = { "summary" },
}

function main()
  phase("inspect")
  local inspected = agent({
    name = "inspector",
    prompt = "Inspect source and tests. Do not modify files. Return evidence.",
    schema = RESULT,
  })
  if not inspected.ok then
    log("inspection failed: " .. inspected.status, "error")
    report({ status = "failed", stage = "inspect" })
    return
  end

  phase("verify")
  local checked = agent({
    name = "verifier",
    prompt = "Verify the findings against source and report unresolved issues.",
  })
  report({ inspection = inspected.output, verification_ok = checked.ok })
end
```

分析/验证 Agent 应提供 schema，以便稳定消费结构化结果；执行 Agent 不要强行使用会阻止 tool calls 的复杂 JSON-mode schema。每次读取 `agent()` 返回值前检查 `ok`；`report()` 后立即 `return`。

## 3. 启动：inline script 或保存的文件

### Inline script

在 Agent tool call 中传 `script`：

```json
{
  "script": "function main() phase('verify'); report({ok=true}) end",
  "concurrency": 2
}
```

这是 tool-call 参数示例，不是可直接在 PowerShell 中执行的命令。成功后保存返回的 `instance_dir`，例如 `anureo-instance_...`。

### Saved workflow

将 Lua 文件放在项目的 `.anureo/workflows/`，再传文件名或名称：

```text
.anureo/workflows/review.lua
```

```json
{ "workflow": "review" }
```

`workflow_files` 只列出当前 working folder 下 `.anureo/workflows/` 中的 Lua 文件。resolver 当前搜索顺序是：项目 `.anureo/workflows/<name>.lua`、用户 `$HOME/.config/anureo/workflows/<name>.lua`、项目 working folder 下 `<name>.lua`，最后是传入的 path；存在的绝对 `.lua` path 也可直接使用。相同名称优先项目 `.anureo/workflows/`。

**安全提示**：workflow 文件中的 `agent()` prompt 可以导致真实修改。提交或复用前审查 source、目标路径、tool 权限和 prompt；不要把 token、密码或其它 secrets 写入 Lua、`args`、prompt、report 或日志。

## 4. 正确的轮询与验收顺序

启动后保存返回的 instance 标识，并按以下顺序观察：

```text
workflow_start
→ Start-Sleep -Seconds 5
→ workflow_status(instance="<returned-instance_dir>")
→ 仅当 status == "running" 时重复等待和 status
```

不要 tight loop，也不要把等待和 status 并行发出。status 终态摘要包含 Agent 状态、token usage、phase timing、event statistics 和有界 report preview；它不返回内部文件引用。

完成后仍要人工验收：

```text
git status --short
git diff
项目自己的测试/构建命令
```

`completed` 只表示 workflow 生命周期成功结束；它不证明 Agent 建议正确、文件符合需求、测试充分或外部操作已回滚。

## 5. 失败调查、事件和 source

先用状态摘要缩小问题，再获取有限信息：

```text
workflow_status(instance="<id>")
workflow_events(instance="<id>", types=["agent_done", "run_done"],
                offset=0, events_limit=50)
workflow_source(instance="<id>")
```

`workflow_events` 支持 `offset`、`events_limit`（`1..=500`，默认 `50`）、`types` 和 `agent_id` 过滤；分页读取，不要一次请求完整事件流。常见事件包括 `agent_started`、`agent_done`、phase span events 和 `run_done`。source tool 只返回 captured Lua source 的有界 preview；超限时 `truncated` 为 `true`。

如果丢失 instance 标识，可使用：

```text
workflow_list()
workflow_list(status_filter="failed", limit=20)
```

`workflow_list` 只列终态 instance，不列仍在运行的任务；`limit` 范围为 `1..=100`，默认 `20`，并使用不透明 `cursor` 分页。过滤值为 `completed`、`failed` 或 `cancelled`。

不要使用 file-reading tool 猜测结果或绕过这些 bounded responses；workflow tools 是对外的查看接口。运行时文件虽存在于项目 working folder，但其 schema 和保留方式属于实验性实现。

## 6. 取消与恢复

### 取消运行

将以下参数传给 `workflow_cancel`：

```json
{ "instance": "<running-instance_dir>" }
```

取消信号接受后返回 `result: "cancelling"`；当前 Agent turn 可能先完成，然后用 status 等待终态 `cancelled`。取消只查当前进程内的 active-run registry；已终止或由另一个进程拥有的运行会返回 `not_found_or_terminal`。取消不是文件回滚。

`cancelled` instance 不能 resume；修复脚本或参数后重新 `workflow_start`。

### 恢复失败/中断的运行

对可恢复的 failed 或 crash/interrupted instance，把原标识传给 `workflow_start` 的 `resume_from_id`：

```json
{ "resume_from_id": "anureo-instance_..." }
```

不要同时传 `script` 或 `workflow`；三种启动模式互斥。恢复会加载原 checkpoint 和 Agent conversation history，已完成 phase 可由 journal cache 跳过，进行中的 Agent 从最后成功的 turn 继续。它返回**新的** `instance_dir` 和 `resumed_from`；之后必须查询新的 instance，而不是旧的 snapshot。

恢复前确认当前 working folder、分支、依赖、模型配置和 tool 权限。源码将 workflow 绑定到当前 runtime 的 working folder；不要假设它会回到原来启动时的目录。

**不确定性**：source 将 failed run 描述为可恢复，并将 completed/cancelled 视为不可恢复；“interrupted”可能表现为 failed，具体取决于中断位置。若 resume 被拒绝，保留原 instance，先读 status/events，再决定是否以全新 run 重试。

## 7. 存储边界与删除注意事项

当前 runtime 的主要路径是：

| 内容 | 路径（相对于 working folder） |
| --- | --- |
| workflow instance | `.anureo/instances/<instance_dir>/` |
| workflow 定义 | `.anureo/workflows/` |
| 兼容读取的旧 runs | `.luft/runs/<instance_dir>/` |

instance 可能包含 `checkpoint.json`、`events.jsonl`、captured `workflow.lua`、`instance.json`，以及较大的 report/Agent output 的有界或文件化表示。tool 响应会隐藏内部 source/reference/path 信息，但本地 artifacts 仍可能包含敏感 prompt、输出或项目路径。

不要递归删除整个 `.anureo`、`.luft` 或 `~/.anureo` 来“清理 workflow”。先列出目标 instance、保存必要的 report/events、确认没有其它 session/skill/memory 共用目录，再使用当前版本明确支持的清理方式。删除 artifacts 不是撤销 Agent 已执行的文件、shell、MCP、网络或上传副作用。

## 8. 版本差异与源码核对点

本文按当前 checkout 核对了：

- `agent/tool/tool-workflow/src/tool_*.rs`：七个 tool 的名称、schema、默认值和响应边界；
- `agent/tool/tool-workflow/src/service.rs`、`runtime.rs`、`workflow_resolver.rs`：启动、路径、生命周期、取消、恢复、分页和 source resolution；
- `agent/tool/tool-workflow/src/workflow_skill.md`、`references/tool-usage.md`、`references/dsl-reference.md`：Lua DSL 与推荐操作顺序；
- `apps/cli/src/run/agent.rs` 及相关运行路径：默认 workflow tool provider 的接线。

默认并发、路径、参数别名、checkpoint schema、resume 行为和 bounded preview 都可能变化；每次使用前以当前 checkout 的 tool schema、`workflow_skill.md` 和 source 为准。若实际 tool 返回与本文不同，以实际 schema 和 status/events 为事实，并记录 unresolved issue，而不要静默套用旧命令或旧路径。


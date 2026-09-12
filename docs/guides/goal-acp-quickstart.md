# Goal ACP 快速上手

> **状态**: 已实现并经真实 ACP E2E 验证
> **适用版本**: session-integrated goal（`_session/goal` version 1）
> **相关代码**: `apps/acp/src/extensions/goal.rs`、`apps/acp/src/agent.rs`、`agent/goal/`
> **协议参考**: [Goal 和 Scheduled Task](../acp-spec/extensions/14-goal-scheduled-task.md)

---

本文只介绍当前推荐的 ACP goal 用法。`anureo goal ...` 是旧 detached runner，除迁移或兼容性验证外不要用于新接入。

## 1. 最小可用流程

Goal 不是一次 `session/prompt` 的参数。客户端建立 ACP session 后，通过 `set` 设置目标并立即启动首轮：

```text
initialize
  → session/new
  → _session/goal { action: "set" }
  → session/update × N（后续自主 turn 也从这里到达）
```

关键语义：

- `set` 会持久化 goal，并像当前 codex-acp 一样立即尝试启动首轮；
- 首轮完成后，只要 goal 仍为 `active`，server 会在 session idle 时自主启动下一轮；
- 自主轮次发生在原 prompt response 之后，因此客户端必须持续消费 `session/update`；
- 模型通过 `update_goal` 将目标置为 `complete` 或 `blocked`；达到 token budget 时转为 `limited` 并停止普通续跑。

## 2. Capability 检测

连接上的第一个业务请求必须是 `initialize`。客户端读取响应顶层 `_meta.goal`：

```json
{
  "_meta": {
    "goal": {
      "version": 1,
      "controlMethod": "_session/goal",
      "actions": ["set", "pause", "resume", "clear"]
    }
  }
}
```

只有 `version == 1` 且 `controlMethod == "_session/goal"` 时才启用 goal UI。`agentCapabilities._meta.goal` 当前也会返回同形数据，但顶层 `_meta.goal` 是首选识别位置。

## 3. 创建 session 并设置 goal

先创建 session：

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "session/new",
  "params": {
    "cwd": "C:\\work\\my-project",
    "mcpServers": []
  }
}
```

从响应取得 `sessionId`，然后设置目标：

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_session/goal",
  "params": {
    "sessionId": "<session-id>",
    "action": "set",
    "objective": "修复登录失败并让相关测试通过",
    "tokenBudget": 20000
  }
}
```

`objective` 必须是非空字符串。`tokenBudget` 是 anureo 的向后兼容扩展，可省略；提供时必须是正整数，非法类型、0 或负数返回 JSON-RPC `-32602 invalid_params`。需要同时兼容原生 codex-acp 的客户端不应依赖该请求字段。

成功结果包含当前快照：

```json
{
  "goal": {
    "objective": "修复登录失败并让相关测试通过",
    "status": "active",
    "tokenBudget": 20000,
    "tokensUsed": 0,
    "timeUsedSeconds": 0,
    "controlMethod": "_session/goal"
  }
}
```

## 4. 观察工作与追加 prompt

`set` 返回后客户端必须继续消费 `session/update`，首轮和后续自主 turn 的文本、工具事件与 goal 快照都会从这里到达。会话空闲时，客户端仍可发送普通 prompt 追加要求：

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "session/prompt",
  "params": {
    "sessionId": "<session-id>",
    "prompt": [
      {"type": "text", "text": "补充要求：先运行登录模块的定向测试"}
    ]
  }
}
```

客户端收到该请求的 `end_turn` response 后也不能停止读流。后续自主 turn 仍会以 `session/update` notification 到达。

Goal 快照位于：

```text
params.update._meta.goal
```

其中 `params.update.sessionUpdate == "session_info_update"`。`tokensUsed` 在每轮后累计增长，不是单次 prompt usage。

## 5. pause、resume 与 clear

暂停：

```json
{"sessionId":"<session-id>","action":"pause"}
```

恢复：

```json
{"sessionId":"<session-id>","action":"resume"}
```

清除：

```json
{"sessionId":"<session-id>","action":"clear"}
```

以上对象均作为 `_session/goal` 的 `params`。`clear` 成功结果为 `{ "cleared": true, "goal": null }`，并额外发布 `params.update._meta.goal == null` 的清除快照。

`resume` 只接受 `paused`、`blocked`、`usage_limited` 对应的可恢复状态。`complete` 和预算耗尽形成的 `budget_limited` 是终态，不能 resume；需要通过 `set` 建立新目标。

## 6. 断线与重连

Goal 状态持久化在 `<ANUREO_HOME>/tasks/tasks.db` 的 `thread_goals` 表，notification 本身不是事实源。

重连顺序必须是：

```text
initialize → session/load → 等待 session_info_update._meta.goal
```

`session/load` 会重新发布当前 goal 快照，包括 `goal: null`，客户端应以该快照覆盖本地缓存。恢复到 active goal 后，发送普通 prompt 可重新进入自主续跑循环。

## 7. 状态与排错

| 状态 | 含义 | 可 resume |
|---|---|---|
| `active` | goal 已武装，可能正在运行或等待 idle continuation | 不需要 |
| `paused` | 用户暂停 | 是 |
| `blocked` | 模型或不可恢复执行错误报告阻塞 | 是 |
| `limited` | provider quota 或 token budget 到限 | 仅 provider quota 对应内部状态可恢复 |
| `complete` | 模型完成审计后宣告完成 | 否 |

常见问题：

- `set` 后没有输出：检查 session 是否仍绑定当前连接、goal 快照是否为 `active`，以及 provider/凭据是否可用。
- prompt response 已返回但仍有事件：这是自主续跑的正常行为。
- `tokensUsed` 不增长：确认 provider 返回 usage；没有 usage 的 provider 响应无法计入 token budget。
- 收到 `limited`：检查可选 `statusReason`；`budget_limited` 需要 `set` 新目标，quota 类限制解除后可以 `resume`。
- 重连后 UI 状态不一致：不要复用旧通知缓存，以 `session/load` 后的新快照为准。

## 8. 仓库内验证

```powershell
cargo build -p anureo-cli
cargo nextest run -p goal
cargo test -p anureo-acp --test e2e_goal_neutral `
  neutral_goal_set_starts_and_accounts_across_turns -- --nocapture
```

最后一条测试通过真实的 stdio bridge、WebSocket server 和 mock LLM，验证 `set → 立即启动 → 自主续跑 → 跨 turn 累计记账 → budget limited`。

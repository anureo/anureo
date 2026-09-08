---
acp-version: 1
anureo-extension-domain: session-history
methods:
  - info
  - page
status: 已实现（v0.1, 2025-08）
implementations:
  - apps/acp/src/extensions/session_history.rs
  - apps/acp/src/agent.rs（cursor + checkpoint 分页读取）
---

# 36 — session-history：会话历史按需分页加载

> 状态：已实现。方法名 `_anureo.dev/session-history/info` 与 `_anureo.dev/session-history/page`。

## 目标

`session/load` 历史上对 checkpoint 全量 replay：把所有历史消息逐条转成
`session/update` 通知发送（apps/acp/src/stream_bridge.rs `send_history`）。消息多时
（数百条）逐条通知经 WS 转发 + 前端逐条应用，加载非常慢。

本扩展把 replay 改为：

1. **`session/load` 只 replay 尾部**（默认最近 50 条原始消息，见
   `ANUREO_ACP_LOAD_HISTORY_TAIL`，0 = 全量），大幅减少通知数量；
2. **更早的历史按需分页拉取**：前端在用户滚动到顶/点击"加载更早"时调用
   `_anureo.dev/session-history/page`，一次请求取回一整段消息（同一响应内批量），
   前端批量渲染，不再逐条通知。

## 决策记录

| 决策 | 理由 |
|---|---|
| replay 尾部而非全量 | 加载延迟随历史长度线性增长；绝大多数会话打开时只关心最近上下文 |
| 尾部 replay 合并为单条 `_anureo.dev/session-history/batch` 通知 | 50 条原始消息 ≈ 100+ 条 `session/update` JSON-RPC 帧；一条批量通知 + 前端单次 commit 应用，网络帧数与投影重渲从 O(N) 降到 O(1)（`ANUREO_ACP_LOAD_HISTORY_BATCH=0` 回退逐条） |
| 分页返回 ACP `SessionUpdate` 数组而非原始 LLM 消息 | 前端复用 `session/update` 的 reducer/渲染逻辑，零新渲染代码 |
| 服务端游标（每 session）而非前端传 `before` | 前端不需要理解原始消息索引；load 后游标即锚点，多连接重放自动归位 |
| 游标存在 `SessionEntry.history_cursor`（`Arc<AtomicUsize>` 共享） | entry clone 共享同一游标，`sessions.get()` 拿到的副本读写一致 |
| Tool 消息不允许作为截断/分页首条 | replay 从 ToolCallUpdate 开始会导致前端出现孤立工具结果；向前扩展到拥有它的 Assistant |

## 批量通知：`_anureo.dev/session-history/batch`

`session/load` 尾部 replay 的线上形态（默认启用，`ANUREO_ACP_LOAD_HISTORY_BATCH=0` 回退为逐条 `session/update`）：

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/session-history/batch",
  "params": {
    "sessionId": "…",
    "updates": [ /* SessionUpdate[]，顺序同逐条 replay（BEGIN marker 与 END marker 之间的全部内容） */ ]
  }
}
```

- 复用 marker 语义：通知流仍是 `BEGIN marker → batch → END marker`（marker 不外发，仅 agent 内部抑制 `session.updated` 广播）；
- updates 元素与 `session/update` 的 update 对象同构，客户端批量 apply 时逐元素走同一 reducer；
- 路由：`NotificationRouter::send_history_batch` 经 `ConnectionOutbound::GlobalNotification` 发到绑定连接（同 `_anureo.dev/global/update` 通道）；
- 前端（anureo）：`AcpRuntime.subscribeHistoryBatch` → `applyHistoryBatch`（单次 zustand commit，投影只重渲一次）。

## 方法

### `info` — 只读探针（不推进游标）

```json
// 请求
{ "sessionId": "…" }          // 省略时取连接绑定的会话
// 响应
{
  "sessionId": "…",
  "totalMessages": 120,       // checkpoint 原始消息总数
  "loadedStartIndex": 45,     // 客户端可见历史的起始原始索引
  "hasMore": true             // = loadedStartIndex > 0
}
```

### `page` — 向前翻一页（推进游标）

```json
// 请求
{ "sessionId": "…", "limit": 50 }   // limit 默认 50，clamp 到 [1, 200]
// 响应
{
  "sessionId": "…",
  "totalMessages": 120,
  "hasMore": true,
  "messages": [
    {
      "index": 45,            // checkpoint 原始消息索引，跨页稳定
      "role": "user",         // user | assistant | tool | system
      "updates": [ /* SessionUpdate[]，顺序同 send_history replay */ ]
    }
  ]
}
```

语义：

- 返回区间 `[start, cursor)`，`cursor` 为该会话当前游标（load 尾部起点），
  `start = max(0, cursor - limit)` 且向前扩展越过 Tool 消息；
- `updates` 由 `SessionNotifier::message_session_updates` 生成（与
  `send_history` 同一转换，含 background review 剥离），System / 空 Assistant
  条目被省略但索引保持稳定；
- 成功响应后游标推进到 `start`；`hasMore = start > 0`；
- 游标为 0 或会话为全新（从未 load 截断，游标 `usize::MAX` 视作 `total`）时返回
  空数组 + `hasMore: false`，不报错；
- 并发 `page` 由 `SessionEntry.control_lock` 串行化，同段不会消费两次。

错误码：`-32601` 未知方法；`-32602` 缺 sessionId；`-32002` 会话不存在
（须先 `session/load` 建立会话）；`-32603` agent 未绑定/内部错误。

## 客户端流程

1. `session/load` → 收到尾部 replay 的 `session/update` 流（原逻辑不变）；
2. `info` 决定是否展示"加载更早"入口；
3. 用户触发 → `page { limit }` → 把 `messages[]` 按 `index` 升序插到当前历史头部，
   每条消息的 `updates` 依次套用现有 `session/update` reducer；
4. 重复 3 直至 `hasMore == false`。

## 实现位置

| 组件 | 位置 |
|---|---|
| Message→SessionUpdate 转换（复用） | apps/acp/src/stream_bridge.rs `SessionNotifier::message_session_updates` |
| load 尾部截断 + 游标写入 | apps/acp/src/agent.rs `history_tail_start` / `load_session_for_owner` |
| checkpoint 分页读取 + info/page 业务 | apps/acp/src/agent.rs `read_checkpoint_messages` / `session_history_info` / `session_history_page` |
| 扩展 handler（late-bind agent） | apps/acp/src/extensions/session_history.rs |
| 注册与绑定 | apps/acp/src/extensions/register.rs（`ExtensionRegistryHandles`）+ apps/acp/src/runtime.rs |

## 测试

- `agent::tests::history_tail_start_*`：截断对齐（短历史/普通边界/Tool 前扩）；
- `extensions::session_history::tests`：未绑定/缺参/未知方法错误码；
- e2e：`tests/agent_modes.rs` 覆盖 load 路径（含尾部截断后的 replay marker 行为）。

## 交叉引用

- [02-session-lifecycle.md](../02-session-lifecycle.md) — `session/load` 尾部 replay 行为
- [35-session-assist.md](35-session-assist.md) — 会话列表/时间戳

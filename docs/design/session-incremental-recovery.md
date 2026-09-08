# anureo Desk 会话 IndexedDB 缓存与断线增量恢复设计

> **状态**: Implemented（v1；reset 使用标准 `session/load` fallback）
> **日期**: 2026-08-22
> **涉及仓库**: `anureo`（本仓库，ACP server/agent）/ `../anureo-feat-dev`（anureo Desk 前端）
> **相关代码**: `apps/acp/src/session_sync.rs`、`apps/acp/src/extensions/session_sync.rs`、`apps/acp/src/runtime.rs`、`apps/acp/src/stdio_loop.rs`、`../anureo-feat-dev/packages/ui/src/lib/acp/acp-session-cache.ts`、`../anureo-feat-dev/packages/ui/src/lib/acp/acp-session-sync.ts`、`../anureo-feat-dev/packages/ui/src/lib/acp/acp-runtime.ts`
> **交叉参考**: [ACP WebSocket 持久化实现方案](./acp-websocket-persistent-implementation.md)、[标准 ACP 单 Server 多 Session 实现方案](./acp-single-server-multi-session.md)、[session-history 扩展](../acp-spec/extensions/36-session-history.md)、[session 生命周期](../acp-spec/02-session-lifecycle.md)

---

## 0. 实现状态（2026-08-22）

v1 已落地以下闭环：

- anureo 在 checkpoint SQLite 中持久化 per-session `streamId`、`nextSeq` 和有界 event window；
- event insert、sequence allocation 与窗口裁剪使用同一事务，服务重启后游标连续；
- prompt task 归 runtime 所有，transport drop 不再隐式取消 run；client bridge 和待回答 question 可随 session binding 切换到新连接；
- history replay message identity 由 `sessionId + messageIndex` 稳定派生；
- anureo Desk 使用 runtime/principal/cwd/session namespace 的 IndexedDB record 原子保存 projection 与 cursor；
- 页面恢复优先执行 session-sync delta，缺少/失效 cursor 时才执行标准 `session/load`；
- WebSocket supervisor 捕获真实 close/error，执行带 jitter 的重连、重新 initialize，并在标记 connected 前恢复 active sync sessions；
- `/acp` 通过 `openRuntimeWebSocket`，并加入 relay dispatcher 与 URL-token auth 双 allowlist。
- 已增加真实 Chromium E2E：用故障代理切断实际 ACP WebSocket，在 prompt 继续运行期间阻止重连，随后验证浏览器携带 IndexedDB cursor 只补一条 delta、全程不发送 `session/load`，并在刷新后从缓存恢复完整 user/assistant turn。

v1 与本文最初方案的一个有意差异是：`open` notification 可以早于 response，client 在 opening
阶段缓存并按 seq 合并；这替代 server paused subscription，仍消除了 catch-up/live gap。另一个差异是
reset response 只给出原因与 high-water mark，authoritative projection 由标准 `session/load` 重建，
而不是在扩展响应中重复实现一套 snapshot wire shape。

尚未作为 v1 发布门禁的增强项包括多 tab 写入仲裁、10k event 独立 benchmark 与浏览器自动化的
IndexedDB quota 压测；核心事务、quota eviction、restart、gap、真实 WebSocket 断线增量恢复和 relay
allowlist 均已有自动化测试。真实浏览器链路的运行方式见 anureo Desk
`docs/E2E_TESTING.md` 的“会话增量恢复 E2E”。

第 2–16 节保留实现前的源码基线、完整目标架构和分阶段提案，便于追溯设计取舍；其中描述的
扩展内 snapshot、server paused subscription、跨平台 E2E、metrics 和 multi-tab 仲裁不是 v1
现状。发生差异时，以本节的实现状态和
[session-sync 扩展规范](../acp-spec/extensions/38-session-sync.md)为准。

---

## 1. 背景与问题

anureo Desk 当前可以在重新打开页面后，通过标准 ACP `session/load` 从 anureo 的
SQLite checkpoint 恢复会话历史。长会话默认只 replay 最近 50 条原始消息，更早
内容再通过 `_anureo.dev/session-history/page` 向前分页。

这条链路解决了“服务端有持久历史，可以重新打开”的问题，但没有满足以下产品要求：

1. 浏览器把已渲染的完整会话保存在 IndexedDB，刷新后立即显示，不等待网络 replay；
2. 恢复同一会话时只传输浏览器尚未收到的增量；
3. WebSocket 短暂断线后自动建立新连接，并立即补齐断线窗口内的消息；
4. 增量 replay 与后续 live stream 之间不能丢消息、重复消息或改变顺序；
5. 正在运行的 prompt 不应仅因 UI transport 断开而被隐式终止。

本设计增加一个 anureo 扩展协议完成可靠增量同步。标准 ACP v1
`session/load` / `session/resume` 保持不变，作为旧 client 和不支持扩展时的兼容路径。

---

## 2. 当前源码事实（2026-08-22）

### 2.1 anureo Desk 只持久化会话列表，不持久化消息正文

`packages/ui/src/sync/persist-cache.ts` 当前通过 `localStorage` 保存 VCS、project
metadata、icon 和最多 50 条 session list record。消息、part、tool call 和 ACP native
session projection 只存在 Zustand 内存 store 中。

`acpSessionLoad()` 在调用 `session/load` 前执行 `resetSession(sessionId)`，随后依赖
服务端重新发送 `_anureo.dev/session-history/batch` 或逐条 `session/update` 来重建会话。

结果是：

- 刷新页面后不能先从浏览器本地显示完整对话；
- 每次恢复都会重新发送尾部历史；
- replay 产生的新 `messageId` 不能与浏览器旧记录稳定对齐。

### 2.2 浏览器 ACP runtime 没有 transport supervisor

`packages/ui/src/lib/acp/acp-runtime.ts` 创建一次 `createWebSocketStream()`，成功
initialize 后将 runtime status 设为 `connected`。runtime status 只有在调用显式
`close()` 时才变成 `disconnected`；底层 WebSocket 的异常 `close` / `error` 没有映射
为 runtime status 变化，也没有创建新 stream、重新 initialize 的循环。

`acp-notification-bridge.ts` 虽然每 2 秒轮询 status，并定义了 `onReconnect` resync，
但真实异常断线不会可靠触发该状态转换。ACP SDK 1.3.0 的 transport 也明确要求调用方
为 ACP v1 reconnect 创建新 stream，并说明断线期间的 in-flight transport message 不会
自动 replay。

### 2.3 anureo 已移除 connection-level replay

`apps/server/src/acp_hub.rs` 仍保留 `EventCursor`、`replay_capacity` 和
`attach_with(..., resume_from)` 的兼容形状，但 `resume_from` 当前被忽略，注释明确说明
标准 `session/load` 已替代 connection-level replay。

`NotificationRouter` 只把 session notification 发送给当时绑定且 active 的 connection。
连接断开后，`close_connection()` 移除 binding；断线窗口内产生的 notification 没有进入
可由新连接读取的 session event log。

因此旧设计文档中“event cursor replay 已完成”的描述已与当前源码不一致。实现本设计前，
必须以源码为基线修正文档状态，不能在已不存在的 Hub replay 行为上继续开发。

### 2.4 `session/load` 不是运行中重连协议

当前 `session/load` 与 `session/resume` 都先执行 `SessionStore::begin_restore()`。如果
session 仍有 active prompt，会返回 `-32010 a prompt is already in progress`，不会把新
connection 绑定到运行中的 session。

同时，`session/prompt` handler 使用 ACP SDK `ConnectionTo::spawn()` 执行 prompt。
该 task 只在 JSON-RPC connection 被 serve 时存活；WebSocket 断开会 drop prompt future。
`PromptGuard` 可以清理 lifecycle，但不能让 run 跨 transport 继续执行。

所以“断线期间继续运行，重连立即接收增量”需要同时解决：

- run 生命周期与 transport 生命周期解耦；
- running session 允许新 connection 只做 subscribe/attach，而不是 restore；
- 断线窗口事件可 replay。

### 2.5 当前能力矩阵

| 能力 | 当前状态 | 结论 |
| --- | --- | --- |
| 服务端历史持久化 | SQLite checkpoint | 已有，可复用 |
| `session/load` 尾部 replay | 默认最近 50 条原始消息 | 已有，但不是增量恢复 |
| 更早历史分页 | `_anureo.dev/session-history/page` | 已有，可保留 |
| 浏览器完整消息缓存 | 无 | 需新增 IndexedDB |
| 浏览器自动 WebSocket 重连 | 无可靠实现 | 需新增 transport supervisor |
| session event sequence | 无 | 需新增 |
| 断线事件 replay | 无 | 需新增 |
| stable replay message identity | 无，历史转换生成 UUID | 需新增 |
| running session reconnect attach | active prompt 时 load/resume 被拒绝 | 需新增独立 sync/attach 方法 |
| prompt 跨 transport 存活 | task 与 connection 生命周期绑定 | 需解耦 |

---

## 3. 目标与非目标

### 3.1 目标

1. anureo Desk 在 IndexedDB 中保存已物化会话的 canonical projection。
2. 页面刷新后先本地恢复 UI，再异步与 anureo 校准。
3. 每个 session update 具有稳定、严格单调的 sequence。
4. client 使用已提交的 `lastSeq` 请求增量，不重复下载完整历史。
5. replay 与 live stream 原子交接，提供无 gap 的 at-least-once delivery。
6. client 通过 sequence 幂等去重，重复发送不产生重复消息或重复 tool call。
7. WebSocket 异常断开后自动创建新 runtime、initialize、恢复 active session 订阅。
8. 短断线期间 prompt 可以继续；重连后优先补齐当前 session，再恢复冷 session。
9. event log 不可用、cursor 过旧或 cache 不兼容时，显式回退 authoritative snapshot。
10. 不支持扩展的标准 ACP client 行为不变。

### 3.2 非目标

- 不修改标准 ACP v1 的 `session/load` / `session/resume` wire contract。
- 不要求第三方 ACP client 使用 IndexedDB。
- 不把 IndexedDB 当作服务端 checkpoint 的替代品或唯一事实源。
- 不保证浏览器清站点数据后仍可做增量恢复。
- 不在首版实现跨设备同步同一个浏览器 cache。
- 不在首版提供浏览器端自定义加密密钥；IndexedDB 仍依赖浏览器 profile/OS 的存储保护。
- 不允许离线发送新 prompt；离线阶段只展示缓存并等待 transport 恢复。
- 不把 SessionIndex 的 `indexVersion` 复用为消息流 sequence；列表索引与会话内容流是两个独立版本域。

---

## 4. 核心设计决策

| 维度 | 决定 | 原因 |
| --- | --- | --- |
| 协议位置 | 新增 `_anureo.dev/session-sync/*` 扩展 | 不改变标准 ACP v1，旧 client 可继续 load |
| 增量标识 | 每 session 独立 `{streamId, seq}` | connection cursor 无法表达多 session；streamId 可检测数据代际变化 |
| 投递语义 | at-least-once + client 幂等 | 比 exactly-once 可实现、可恢复；sequence 足以去重 |
| 本地缓存 | IndexedDB 保存 projection + cursor | 容量、事务和异步 IO 均优于 localStorage |
| 服务端恢复源 | durable event head + 有界 replay window + checkpoint snapshot | 短断线低延迟，长缺口可显式 reset |
| live/replay 交接 | server 端 high-watermark 原子切换 | 避免 replay 完成与订阅建立之间丢消息 |
| running attach | 独立 `open`，不执行 `begin_restore` | running prompt 必须允许新 transport 订阅 |
| message identity | 服务端 canonical stable ID | 当前 replay UUID 无法稳定 merge |
| prompt 生命周期 | run 由 session runtime 拥有，不由 ACP connection task 拥有 | transport 断开不应隐式终止 run |
| 降级 | `-32601` 或 capability 缺失时走现有 `session/load` | 向后兼容旧 anureo/旧 client |
| reset | 明确返回 `mode: "reset"`，不把失败伪装成空增量 | 防止 silent data loss |

---

## 5. 总体架构

```text
anureo Desk
┌──────────────────────────────────────────────────────────────┐
│ React / Zustand projection                                   │
│          ▲                         │                         │
│          │ hydrate                 │ apply batch             │
│          │                         ▼                         │
│ IndexedDB session cache ── last {streamId, seq}              │
│                                  │                           │
│                          session-sync/open                   │
│                                  │                           │
│ ACP transport supervisor ────────┼── new WS + initialize     │
└──────────────────────────────────┼───────────────────────────┘
                                   │
                                   ▼
anureo server
┌──────────────────────────────────────────────────────────────┐
│ SessionSyncRegistry                                         │
│   attach running session / capture high-watermark           │
│            │                         ▲                       │
│            ▼                         │ live updates           │
│ durable event head + bounded replay window                  │
│            │                                                 │
│            └── cursor expired ──► checkpoint snapshot        │
│                                                              │
│ SessionRuntime owns prompt task and rebindable client bridge │
└──────────────────────────────────────────────────────────────┘
```

恢复时，IndexedDB 是 UI 的快速本地副本，anureo 始终是 authoritative source。client 不因
本地已有内容而跳过校准；它只把“全量 `session/load`”替换为“带 cursor 的 sync open”。

---

## 6. 服务端数据模型

### 6.1 Session stream identity

每个 session 增加：

```rust
pub struct SessionStreamHead {
    pub session_id: SessionId,
    pub stream_id: String,
    pub next_seq: u64,
    pub min_replay_seq: u64,
    pub checkpoint_seq: u64,
}
```

- `stream_id`：持久 UUID。只有 session 内容存储被重建、导入覆盖或不兼容迁移时改变；
  普通 server restart 不改变。
- `seq`：从 1 开始、严格递增、仅在该 session 内有意义。
- `next_seq`：下一事件序号；分配与事件持久化必须在同一事务中完成。
- `min_replay_seq`：服务端仍可提供增量的最小 sequence。
- `checkpoint_seq`：当前 authoritative checkpoint/snapshot 已覆盖到的 sequence。

禁止使用 process-global sequence。多 session 并行时，client 只需要维护当前 session 的
cursor，也避免全局高频锁。

### 6.2 Canonical event

```rust
pub struct SessionSyncEvent {
    pub stream_id: String,
    pub seq: u64,
    pub event_id: String,
    pub emitted_at: String,
    pub payload: SessionSyncPayload,
}

pub enum SessionSyncPayload {
    SessionUpdate(SessionUpdate),
    PromptState(PromptStateSnapshot),
    SessionDeleted,
}
```

约束：

- `event_id` 可使用 `<streamId>:<seq>`，用于日志和诊断；幂等判断以二元组为准。
- 现有 `session/update` payload 可以复用，但 replay 时不得重新生成 message/tool identity。
- `PromptState` 至少包含 `idle | running | waiting_permission | cancelled | failed | completed`，
  live activity 不再从历史消息推断。
- delete 是终止事件；应用后 client 删除 projection，并保存短期 tombstone 防止旧 batch 复活。

### 6.3 Stable message identity

当前 `SessionNotifier::message_session_updates()` 在每次历史转换时为 user/assistant chunk
生成新 UUID。增量缓存要求同一 canonical message 在 live、checkpoint snapshot、历史分页中
始终使用同一个 `messageId`。

首版建议新增持久映射：

```text
acp_session_message_ids(
  session_id,
  message_index,
  message_id,
  PRIMARY KEY(session_id, message_index)
)
```

- checkpoint 中每条原始 message 的稳定 index 对应一个 canonical `message_id`；
- assistant reasoning 与 visible content 使用同一 `message_id`；
- tool call 继续使用模型产生的稳定 `tool_call_id`；
- 历史分页、reset snapshot 和 `session/load` 兼容 replay 都查询同一映射；
- 新 live message 在创建时分配 ID，并在 checkpoint commit 时写入 index 映射。

如果后续把 `message_id` 直接写入 LLM message/checkpoint schema，可迁移掉映射表，但不是
本功能的前置条件。

### 6.4 Event log 与 compaction

不能无限持久化 token delta。采用两级保留：

1. durable stream head：始终持久化 `streamId`、`nextSeq`、`checkpointSeq`；
2. replay window：保留最近事件，受条数、字节数和 TTL 三重限制；
3. checkpoint snapshot：作为超过 replay window 时的 authoritative reset 来源。

建议默认值（实现前用 benchmark 校准）：

| 配置 | 默认建议 | 说明 |
| --- | --- | --- |
| 每 session 最大事件数 | 10,000 | 短时 token burst |
| 每 session 最大字节数 | 8 MiB | 防止单个大 tool output 占满内存/DB |
| replay TTL | 24 小时 | 覆盖日常刷新和移动网络断线 |
| 单次 sync response | 1 MiB 或 2,000 events | 超出时分页 continuation |

compaction 只能推进 `minReplaySeq`，不能复用旧 sequence。cursor 早于
`minReplaySeq - 1` 时返回 reset，不尝试猜测或静默跳过缺口。

### 6.5 Prompt 与 connection 生命周期解耦

新增 session-owned `RunningTurn` task：

```text
SessionRuntime
  ├─ running_turn: JoinHandle / cancellation token
  ├─ stream_head + event writer
  └─ RebindableClientBridge
       ├─ current connection generation
       └─ waiters / deadline
```

`session/prompt` handler 只验证并启动 run，然后由 `SessionRuntime` 持有任务。ACP request
response 可继续等待最终结果；如果原 connection 消失，response 丢失不影响 run 本身。
最终状态写入 event log/checkpoint，重连 client 通过 sync 获得。

对依赖 client reverse-RPC 的能力采用以下规则：

- filesystem/terminal/permission/question 请求通过 `RebindableClientBridge` 查找当前 generation；
- transport 断开后进入有界等待，而不是自动批准或把请求发给旧 connection；
- 新 connection 完成 session-sync attach 后可接管 bridge；
- deadline 到期后以明确错误结束相关 tool/run，并写入 `PromptState::Failed`；
- permission 永不因断线自动批准。

---

## 7. `_anureo.dev/session-sync` 协议

### 7.1 Capability

anureo 在 initialize response 的扩展 capability 中声明：

```json
{
  "_meta": {
    "anureo.dev": {
      "sessionSync": {
        "open": true,
        "version": 1,
        "maxBatchEvents": 2000
      }
    }
  }
}
```

client 未看到 capability 时不得探测性依赖该协议；直接使用现有 `session/load`。

### 7.2 Open request

方法：`_anureo.dev/session-sync/open`

```jsonc
{
  "sessionId": "session-abc",
  "cwd": "C:\\Users\\heycj\\dev\\anureo",
  "cursor": {
    "streamId": "67b8d393-...",
    "seq": 4182
  },
  "clientInstanceId": "browser-installation-uuid",
  "limit": 2000
}
```

- `cursor` 可省略，表示 client 没有可信缓存；
- `cwd` 继续执行与 `session/load` 相同的 canonical owner/cwd 校验；
- `clientInstanceId` 只用于日志、订阅替换和诊断，不作为授权凭据；
- open 只 attach/subscribe，不执行 `begin_restore`，因此 running session 也允许调用；
- 同 connection 重复 open 同一 session 幂等，新的 cursor 不得使已确认位置倒退。

### 7.3 Delta response

```jsonc
{
  "sessionId": "session-abc",
  "mode": "delta",
  "streamId": "67b8d393-...",
  "fromSeq": 4183,
  "throughSeq": 4210,
  "events": [
    {
      "seq": 4183,
      "eventId": "67b8d393-...:4183",
      "emittedAt": "2026-08-22T12:00:00.000Z",
      "payload": {
        "type": "session_update",
        "update": { "sessionUpdate": "agent_message_chunk", "messageId": "msg-...", "content": { "type": "text", "text": "..." } }
      }
    }
  ],
  "hasMore": false,
  "nextCursor": null,
  "promptState": { "status": "running" }
}
```

空增量是合法成功：`events: []` 且 `fromSeq = throughSeq + 1`。fetch failure 必须通过
JSON-RPC error 表达，不能返回空增量伪装成功。

如果遗漏事件超过 response 限制：

- response 返回当前 page 的 `throughSeq`、`hasMore: true` 和 opaque `nextCursor`；
- server 已为该 subscription 暂停 live delivery；
- client 继续调用 `_anureo.dev/session-sync/continue`；
- catch-up 完成后再进入 live 状态。

### 7.4 Reset response

```jsonc
{
  "sessionId": "session-abc",
  "mode": "reset",
  "reason": "cursor_expired",
  "streamId": "67b8d393-...",
  "throughSeq": 4210,
  "snapshot": {
    "messages": [],
    "toolCalls": [],
    "plan": null,
    "usage": null,
    "currentMode": "default",
    "promptState": { "status": "idle" },
    "hasMoreHistory": true
  }
}
```

`reason` 枚举：

| reason | 含义 |
| --- | --- |
| `no_cursor` | client 无本地 cursor |
| `stream_changed` | client 的 streamId 与 server 不同 |
| `cursor_expired` | cursor 早于 minReplaySeq |
| `cursor_ahead` | client seq 大于 server head，可能发生 DB rollback/恢复 |
| `schema_incompatible` | snapshot/event schema 无法增量迁移 |

reset snapshot 必须带 stable message ID，并声明 `throughSeq`。client 在一个 IndexedDB
transaction 中替换 projection 和 cursor，不能先清空旧 UI 再等待 snapshot。

### 7.5 Live notification

方法：`_anureo.dev/session-sync/update`

```jsonc
{
  "sessionId": "session-abc",
  "streamId": "67b8d393-...",
  "events": [
    {
      "seq": 4211,
      "eventId": "67b8d393-...:4211",
      "emittedAt": "2026-08-22T12:00:01.000Z",
      "payload": { "type": "session_update", "update": {} }
    }
  ]
}
```

允许一个 notification 批量携带连续 events，以复用当前前端 40ms coalescing 思路。每个
batch 必须满足：

- 同一 `streamId`；
- `seq` 严格递增；
- 不跨 session；
- 序列出现 gap 时 client 停止应用后续事件，并重新 open 已提交 cursor；
- `seq <= lastSeq` 的重复事件直接丢弃。

### 7.6 Replay/live 原子交接

server 必须满足以下顺序：

```text
validate owner/cwd/cursor
  -> 注册 paused subscription
  -> 在 session stream lock 下捕获 highWatermark
  -> 读取 (lastSeq, highWatermark] replay
  -> 将 delta/reset response 排入 connection outbound FIFO
  -> 激活 subscription
  -> 发送 seq > highWatermark 的 buffered/live events
```

强制 invariant：

```text
response.throughSeq = N
=> client 在该 response 前不会收到 seq > N 的 live event
=> client 在该 response 后不会遗漏任何 seq > N 的 event
```

不得用 sleep、客户端二次 list 或“通常先注册再读取”替代该原子边界。

### 7.7 Close

方法：`_anureo.dev/session-sync/close`

```json
{ "sessionId": "session-abc" }
```

close 仅删除当前 connection 的 sync subscription 和 reverse-RPC bridge binding；不关闭
session、不取消 prompt、不删除 IndexedDB cache。WebSocket 关闭时 server 自动执行等价清理。

---

## 8. anureo Desk IndexedDB 设计

### 8.1 Database 与 key scope

数据库名建议：`anureo-session-cache-v1`。

所有 key 必须包含：

```text
runtimeKey + ownerScope + sessionId
```

- `runtimeKey`：规范化 ACP endpoint/runtime identity，防止切换实例后串缓存；
- `ownerScope`：principal 的不可逆 hash 或服务端返回的稳定非敏感 owner ID；
- `sessionId`：anureo canonical session ID。

只用 `sessionId` 做 key 会把不同 server 或不同登录主体的同名 session 混在一起，禁止。

### 8.2 Object stores

#### `sessionSnapshots`

```ts
type PersistedSessionSnapshot = {
  key: [runtimeKey: string, ownerScope: string, sessionId: string]
  schemaVersion: 1
  streamId: string
  lastSeq: number
  projection: {
    messageOrder: string[]
    messages: Record<string, AcpNativeMessage>
    toolCalls: Record<string, AcpToolCallRecord>
    plan: AcpPlanEntry[] | null
    usage: { used: number; size: number } | null
    currentMode: string | null
    title: string | null
    promptState: PromptStateSnapshot
    hasMoreHistory: boolean
  }
  updatedAt: number
  lastAccessAt: number
  estimatedBytes: number
}
```

不持久化以下瞬时字段：

- `loadingHistory`；
- in-flight Promise、AbortController、timer；
- connection phase；
- 未得到服务端 event/canonical ID 确认的纯 optimistic assistant chunk。

用户已提交但尚未确认的 prompt 可另存 queue/draft store，不能混入 authoritative snapshot。

#### `sessionTombstones`

保存 `{key, streamId, deletedSeq, deletedAt}`。作用是阻止迟到 batch 或旧 tab 把已删除
session 复活。TTL 建议 7 天，SessionIndex authoritative tombstone 仍是服务端事实源。

#### `cacheMeta`

保存 runtime/owner 级 schema、最近清理时间、总估算字节数和 migration 状态。

首版不要求持久化完整 event WAL；projection 与 `lastSeq` 在同一 transaction 中提交已经能
支持崩溃恢复。如果需要审计或 time travel，再独立增加有界 `sessionEvents` store。

### 8.3 原子提交

收到连续 batch 时：

```text
read snapshot
  -> verify streamId
  -> drop seq <= lastSeq
  -> require first seq == lastSeq + 1
  -> reduce events into private projection
  -> one IndexedDB readwrite transaction:
       put projection
       put lastSeq = batch.last.seq
       update byte accounting
  -> transaction committed
  -> publish one Zustand update
```

cursor 只能表示 IndexedDB 已提交的数据，不能在网络接收时提前推进。否则页面在
`cursor 已前进、projection 未落盘` 的窗口崩溃后，会永久跳过消息。

### 8.4 Hydration

启动顺序：

1. 解析 route/current session；
2. 读取对应 IndexedDB snapshot；
3. 校验 schema、runtimeKey、ownerScope 和 tombstone；
4. 一次写入 ACP native store，立即渲染；
5. UI 标记 `stale/catching_up`，不能把历史 `promptState: running` 当作在线事实；
6. transport connected 后以 `{streamId,lastSeq}` 调 session-sync open；
7. delta/reset commit 后切换为 `live`。

历史缓存只恢复上下文，不自行推断 agent 当前仍在运行。`running` 状态必须由 open response
或后续 live `PromptState` 确认。

### 8.5 Cache 限额与淘汰

同时按 count 和 bytes 限制，建议初始策略：

| 限制 | 默认建议 |
| --- | --- |
| 已物化 session 数 | 100 |
| 总估算字节数 | 100 MiB |
| 单 session | 20 MiB |
| 未访问 TTL | 30 天 |

LRU 淘汰时保留 session list metadata，但可删除正文 projection。当前 active session、正在
catch-up 的 session 和含未发送 draft/queue 的 session 不可淘汰。

使用 `navigator.storage.estimate()` 监控实际 quota；写入 `QuotaExceededError` 时先淘汰冷
session，再重试一次。仍失败则继续使用内存态并显式记录 cache degraded 状态，不能影响
实时聊天。

### 8.6 隐私与清理

- IndexedDB 包含 prompt、模型回复、tool output，属于敏感本地数据；
- logout、删除 runtime credential 或显式“清除本地数据”必须清理对应 owner/runtime scope；
- runtime endpoint switch 只切换 namespace，不误删另一实例缓存；
- 日志不得输出 projection、prompt 正文或 auth token；
- 浏览器 IndexedDB 不是应用层加密。若产品要求共享设备上的额外保护，需要另写加密与
  key lifecycle 设计，不能宣称当前方案已加密。

---

## 9. 自动重连与恢复时序

### 9.1 Transport supervisor

anureo Desk 新增 process-wide ACP transport supervisor，职责是：

1. 监听底层 WebSocket `close` / `error` 和 SDK connection future 结束；
2. 原子更新 runtime status：`connected -> reconnecting -> connected|auth_required`；
3. 每次重连创建全新的 WebSocket stream 和 ACP client connection；
4. 重新执行 pre-auth/initialize，不复用已关闭的 `ClientContext`；
5. endpoint generation 变化时取消旧 attempt，防止迟到 promise 复活旧 runtime；
6. 按 `navigator.onLine`、页面 visibility、错误 HTTP/auth 分类和连续失败次数退避；
7. `online`、页面重新 visible 和用户手动 retry 可中断等待立即重试。

建议 backoff：500ms 起步，指数增长；前台在线 cap 5s，离线/后台/永久 4xx cap 60s，
加入 jitter 防止多 tab 同时重连。

### 9.2 恢复优先级

连接建立后的顺序固定：

1. current/visible session；
2. 有本地 `running` 或 pending permission 提示、需要 server 确认的 session；
3. 已物化且当前页面会读取的 session；
4. 其余 session 不自动打开，等用户访问时再 sync。

禁止因为 localStorage/SessionIndex 中列出了几十个 session 就全部 `session/load` 或全部
subscribe。这会阻塞当前会话恢复并扩大 server fanout。

### 9.3 时序图

```text
Browser                         anureo
   |                              |
   |-- live seq=4182 committed -->|  (normal state)
   X        WebSocket lost        |
   |                              |-- run continues
   |                              |-- persist seq=4183..4210
   |-- new WebSocket ------------>|
   |-- initialize --------------->|
   |-- session-sync/open          |
   |   cursor=(stream,4182) ------>|
   |                              |-- paused subscribe
   |                              |-- highWatermark=4210
   |<-- delta 4183..4210 ----------|
   |-- IndexedDB transaction       |
   |   commit projection+4210      |
   |<-- live batch 4211..N --------|
   |-- commit + render             |
```

目标不是“同一 WebSocket 恢复”，而是“新 ACP connection 对同一 session stream 无缝续读”。

### 9.4 多 tab

首版允许多 tab 各自 connection/订阅，server 已支持一个 session 绑定多个 connection。
每个 tab 使用自己的 `clientInstanceId`，但共享 IndexedDB namespace。

为避免并发写 cursor 倒退：

- IndexedDB transaction 读取当前 `lastSeq` 后只允许 `max(current, incoming)`；
- 较旧 tab 的重复事件被 sequence 去重；
- streamId 不同的写入必须触发 reset 协调，不能覆盖较新 stream；
- 后续可用 BroadcastChannel 选主以减少连接数，但不作为正确性前提。

---

## 10. 失败与降级语义

| 场景 | 行为 |
| --- | --- |
| anureo 不声明 sessionSync capability | 使用现有 `session/load` + history page |
| 方法返回 `-32601` | 记录兼容 fallback，当前 runtime 生命周期内不再重试扩展 |
| IndexedDB 不可用 | 使用内存 store；恢复走 reset/load，不阻断聊天 |
| cursor 正常 | delta catch-up |
| cursor 过期 | reset snapshot |
| streamId 改变 | reset snapshot，原子替换旧 projection |
| cursor ahead | reset + 诊断日志/metric，提示可能发生 DB restore |
| batch 重复 | 丢弃 `seq <= lastSeq` |
| batch 出现 gap | 停止应用，从已提交 cursor 重新 open |
| live event 持久化失败 | 不推进 cursor；保留内存 UI并进入 cache degraded |
| auth 过期 | 进入 `auth_required`，不盲目高频重连，不清除本地会话正文 |
| session 删除 | 应用 tombstone，删除 projection，停止订阅 |
| reverse-RPC 等待重连超时 | tool/run 明确失败并产生持久终态，不自动批准 |
| server restart | durable stream 可继续；若 replay window 不完整则 reset |

任何 authoritative fetch/sync 失败都必须与“成功但没有增量”区分。失败时保留最后一份
IndexedDB/Zustand projection，并显示 stale/reconnecting，不把现有内容清空。

---

## 11. 两端实现落点

### 11.1 anureo

| 文件 | 改动类型 | 说明 |
| --- | --- | --- |
| `apps/acp/src/session_sync.rs` | 新增 | stream head、event log、subscription handoff、协议类型 |
| `apps/acp/src/extensions/session_sync.rs` | 新增 | `open` / `continue` / `close` handler |
| `apps/acp/src/extensions/register.rs` | 修改 | 注册 session-sync domain 与 capability |
| `apps/acp/src/runtime.rs` | 修改 | update 先写 canonical event，再路由 sync subscribers |
| `apps/acp/src/notification_router.rs` | 修改 | 支持 paused/live subscription 与 batch route |
| `apps/acp/src/session.rs` | 修改 | session-owned running task 与 stable stream state |
| `apps/acp/src/stdio_loop.rs` | 修改 | 注册扩展；prompt handler 不再拥有 run lifetime |
| `apps/acp/src/stream_bridge.rs` | 修改 | stable message ID；live/history/snapshot identity 一致 |
| `apps/acp/src/session_repository.rs` | 修改 | stream head、message ID mapping、event window schema/事务 |
| `apps/server/src/acp_hub.rs` | 修改 | 去除无效 connection cursor 兼容假象，接入真实 session sync metrics |
| `apps/server/src/handlers/acp.rs` | 修改 | transport close 只 detach；不 drop session-owned run |
| `docs/acp-spec/extensions/38-session-sync.md` | 新增 | 冻结 extension wire contract |
| `docs/design/acp-websocket-todo.md` | 修改 | 修正与源码不一致的 replay 完成状态 |

文件名与 module 拆分可在实现阶段按 crate 边界微调，但不能把 stream/event persistence
重新塞进 `handlers/acp.rs`；HTTP/WS handler 只拥有 transport 生命周期。

### 11.2 anureo Desk

| 文件 | 改动类型 | 说明 |
| --- | --- | --- |
| `packages/ui/src/lib/acp/acp-transport-supervisor.ts` | 新增 | 自动重连、generation、backoff、status event |
| `packages/ui/src/lib/acp/acp-runtime.ts` | 修改 | 单次 connection 构造与 supervisor 分层；暴露 close/error |
| `packages/ui/src/lib/acp/acp-runtime-shared.ts` | 修改 | singleton 指向 supervisor 管理的当前 generation |
| `packages/ui/src/lib/acp/acp-session-sync.ts` | 新增 | capability、open/continue/close、batch 校验 |
| `packages/ui/src/lib/acp/acp-session-cache.ts` | 新增 | IndexedDB schema、transaction、LRU、migration |
| `packages/ui/src/lib/acp/acp-session-store.ts` | 修改 | hydrate/replace/applyCanonicalBatch，保持 narrow commit |
| `packages/ui/src/lib/acp/acp-native-wire.ts` | 修改 | sync batch 进入 native store；旧 session/update fallback |
| `packages/ui/src/lib/acp/acp-bootstrap.ts` | 修改 | 优先 IndexedDB + session-sync，兼容时才 `session/load` |
| `packages/ui/src/sync/sync-context.tsx` | 修改 | reconnect 恢复优先级，不再清空后全量 replay |
| `packages/ui/src/lib/runtime-switch.ts` | 修改 | runtime namespace 与旧 generation 隔离 |
| `packages/ui/src/stores/DOCUMENTATION.md` | 修改 | 记录 IndexedDB cache ownership 与非事实源边界 |

实现涉及共享 ACP transport 和 WebSocket，anureo Desk 开发时必须同时遵循其
`ui-api-decoupling` 与 `relay-transport` 项目技能，验证 direct、Relay、Electron、Web 和
VS Code runtime 的一致性。

---

## 12. 分阶段实施

### Phase 0 — 契约与基线修正

1. 新增 `38-session-sync.md`，冻结 capability、method、error/reset reason 和 event schema。
2. 修正旧 WebSocket todo/design 中与当前 `attach_with`、replay buffer 不一致的状态。
3. 为当前行为增加黑盒基线：异常断线不会自动重连、load replay message ID 不稳定、
   active prompt 拒绝 load/resume。

验收：团队对“当前没有可靠 cursor replay”达成同一事实基线，协议评审通过后再写存储。

### Phase 1 — Stable identity 与服务端 event stream

1. 增加 stream head/event schema migration。
2. live update 分配 durable sequence。
3. 历史转换使用 stable message ID。
4. 实现 bounded replay window 和 reset snapshot builder。
5. 覆盖 restart、compaction、cursor ahead/expired。

验收：给定同一 checkpoint，重复 snapshot 的 message/tool identity 完全一致；sequence 不
重复、不倒退。

### Phase 2 — Session-owned run 与 running attach

1. prompt task 移出 connection task actor，由 SessionRuntime 持有。
2. 实现 rebindable client bridge 与 permission deadline。
3. 实现 `session-sync/open` paused subscription/high-watermark handoff。
4. 断线不取消 run；显式 cancel 仍立即生效。

验收：prompt streaming 中断开 WS，run 继续完成；新 WS 在 active run 期间 open 成功并收到
连续增量或最终状态。

### Phase 3 — anureo Desk IndexedDB

1. IndexedDB schema、runtime/owner namespace、migration 和 quota/LRU。
2. native projection hydrate/atomic commit。
3. session-sync client 与 gap/duplicate/reset reducer。
4. 保留不支持扩展时的 `session/load` fallback。

验收：刷新后首帧从 IndexedDB 出现；网络只收到 `lastSeq` 后的事件；transaction fault
injection 后 cursor 与 projection 不分裂。

### Phase 4 — Transport supervisor

1. 捕获真实 WS close/error/SDK connection completion。
2. 新 stream + initialize + active-session-first resubscribe。
3. endpoint generation、auth、online/visibility backoff。
4. Relay、Electron、Web、VS Code parity。

验收：拔网、恢复网络无需刷新页面；当前会话先 catch-up，UI 不清空、不重复消息。

### Phase 5 — E2E、性能与发布

1. Rust deterministic prompt + forced disconnect E2E。
2. Playwright 页面刷新/离线/重连/多 tab/IndexedDB quota 场景。
3. 10k event catch-up 与大 tool output benchmark。
4. feature flag 灰度、metrics 和兼容矩阵。

建议短期 feature flag：

```text
ANUREO_ACP_SESSION_SYNC=0|1
VITE_EXPERIMENTAL_ACP_SESSION_SYNC=0|1
```

flag 只用于灰度和回滚；协议稳定并完成两个版本兼容验证后删除双路径中的实验分支，保留
标准 ACP fallback。

---

## 13. 测试矩阵

### 13.1 anureo unit/integration

| 用例 | 断言 |
| --- | --- |
| sequence concurrent allocation | 同 session 严格递增，不同 session 互不阻塞 |
| transaction rollback | event 与 nextSeq 同时回滚，无空洞 |
| stable replay identity | 相同 checkpoint 多次 snapshot ID 一致 |
| cursor exact head | delta events 为空，promptState authoritative |
| cursor inside window | 只返回 cursor 后事件 |
| cursor expired | `mode=reset, reason=cursor_expired` |
| stream changed | reset，不混合旧 stream event |
| cursor ahead | reset + metric |
| catch-up/live race | response throughSeq 后第一条 live 正好是 `throughSeq+1` |
| replay pagination | paused 期间 live event 不越过 response |
| running attach | active prompt 时 open 成功，load/resume 仍保持标准 busy 语义 |
| disconnect persist | WS 断开后 run 继续且 event 被记录 |
| explicit cancel | 新 connection attach 后 cancel 生效 |
| permission disconnect | 不自动批准；重连接管或 deadline fail |
| restart | durable stream head 不倒退；窗口不足时 reset |
| cross-owner open | 不泄露 session 是否存在或内容 |

### 13.2 anureo Desk unit

| 用例 | 断言 |
| --- | --- |
| hydrate | IndexedDB projection 一次进入 native store |
| duplicate batch | `seq <= lastSeq` 不产生 store write |
| gap batch | 不应用 gap 后事件，重新 open 已提交 cursor |
| atomic commit fault | transaction abort 后 projection/lastSeq 均保持旧值 |
| reset | projection 与 cursor 原子替换 |
| runtime switch | 不读取/覆盖旧 endpoint namespace |
| owner switch | 不显示上一 principal 消息 |
| quota exceeded | 淘汰冷 session 后重试；失败降级内存 |
| stale prompt state | hydrate 不直接把 session 标为 live running |
| fallback | capability 缺失/`-32601` 走 `session/load` |
| endpoint generation | 旧 runtime 迟到 promise 不替换新 runtime |
| multi-tab writes | cursor 不倒退，重复事件幂等 |

### 13.3 E2E

```gherkin
Feature: session content survives refresh and reconnects incrementally

  Scenario: refresh uses IndexedDB and requests only the delta
    Given a session is materialized through sequence 100
    And its projection and cursor are committed to IndexedDB
    When the page reloads
    Then the cached conversation is visible before network catch-up completes
    And session-sync/open sends cursor 100
    And the server does not replay events 1 through 100

  Scenario: reconnect catches up a running prompt without a gap
    Given a prompt is streaming and the browser committed sequence 120
    When the WebSocket disconnects
    And the server produces sequences 121 through 150
    And the browser reconnects
    Then the browser receives 121 through 150 in order
    And subsequent live delivery starts at 151
    And no message is duplicated

  Scenario: an expired cursor falls back to a snapshot
    Given the browser cursor is older than the server replay window
    When the browser opens session sync
    Then the server returns an authoritative reset snapshot
    And the browser atomically replaces its projection and cursor

  Scenario: an old anureo server remains compatible
    Given anureo does not advertise sessionSync
    When the browser opens an existing session
    Then the browser restores it through session/load
```

测试必须采集 wire frames，断言实际只发送增量，不能只根据最终 UI 正确就判定通过。

---

## 14. 性能与可观测性

### 14.1 性能预算

初始验收预算：

| 场景 | 预算 |
| --- | --- |
| IndexedDB 读取 2,000 条消息 projection | p95 < 100ms（常规桌面浏览器） |
| 500 events catch-up reduce + transaction | p95 < 150ms |
| 无增量 open response | p95 < 50ms（loopback，不含 WS initialize） |
| 10k replay event server 查询 | p95 < 250ms |
| streaming IndexedDB commit | 最多每 40–100ms 一次，不按 token 每条 transaction |

基准必须同时采样 payload bytes、heap、CPU 和 transaction duration。不能用事件条数替代
字节限制，因为 tool output 大小差异显著。

### 14.2 Metrics

anureo 增加：

| 指标 | 类型 | 说明 |
| --- | --- | --- |
| `acp_session_sync_open_total{mode}` | counter | delta/reset/fallback |
| `acp_session_sync_replayed_events_total` | counter | 实际补发事件数 |
| `acp_session_sync_reset_total{reason}` | counter | reset 原因 |
| `acp_session_sync_gap_total` | counter | server/client 报告 gap |
| `acp_session_sync_active_subscriptions` | gauge | 当前订阅数 |
| `acp_session_sync_replay_bytes_total` | counter | replay 流量 |
| `acp_orphan_run_total{outcome}` | counter | 断线后 run 完成/失败/超时 |

不得把 sessionId、principal、prompt、token 或 cwd 放进 metric label。结构化日志可以带
connectionId、streamId 前缀、seq 范围和 principal hash，但不记录正文。

anureo Desk 仅在 debug/telemetry policy 允许时记录：hydrate duration、cache bytes、delta
count、reset reason、reconnect attempts 和 IndexedDB failure kind，不记录消息内容。

---

## 15. 向后兼容、迁移与回滚

### 15.1 Wire 兼容

- 标准 ACP initialize/session 方法不变；
- session-sync 仅在 capability 协商后使用；
- old client 继续 `session/load`；
- new client 连接 old anureo 时自动 fallback；
- live `session/update` 可在兼容窗口继续发送给未开启 session-sync 的 connection；开启
  session-sync 的 connection 对同一 session 只消费一种 canonical content stream，避免双写。

### 15.2 IndexedDB migration

- DB schemaVersion 独立于 wire sessionSync version；
- migration 必须使用新 object store/version transaction，不原地部分改写 cursor；
- migration 失败时删除对应 cache namespace并走 reset，不影响服务端会话；
- 不从当前 localStorage 推断消息正文；只迁移 session metadata，正文首次由 snapshot 建立。

### 15.3 回滚

关闭 feature flag 后：

1. anureo 停止声明 sessionSync capability；
2. anureo Desk 自动走 `session/load`；
3. IndexedDB cache 可暂时保留但不作为 render source，避免来回灰度反复下载；
4. event log schema 保留，禁止紧急回滚直接 drop table；
5. 确认旧 client 的 load/history page/cancel/permission 全部正常后再发布。

---

## 16. 风险与待评审决策

| 风险/决策 | 建议 |
| --- | --- |
| token delta event log 写放大 | 40–100ms coalesce + checkpoint 后 compaction；以 bytes 双限额 |
| prompt response 属于已断开的 JSON-RPC request | run 结果以 persistent PromptState 为事实源；旧 response 丢失可接受 |
| reverse-RPC 必须有 client | rebindable bridge + deadline；禁止静默转 server 权限或自动批准 |
| stable message ID 改动 checkpoint 周边 | 先用独立 mapping table，降低 LLM message schema 迁移风险 |
| 多 connection 同 session | 每 connection 独立 cursor；event log 共享；不以 connection ownership 作为 stream ownership |
| server DB restore 导致 cursor ahead | reset + streamId/metric，不尝试负增量 |
| IndexedDB 明文敏感内容 | 明确产品边界；logout/清理 scope；应用层加密另案设计 |
| Relay 下重连行为不同 | transport supervisor 必须通过共享 runtime transport；真实 Relay E2E 是发布门槛 |
| old `session/update` 与 sync update 双消费 | connection/session 级 delivery mode 二选一，不能靠 client reducer 猜测去重 |
| 旧设计文档状态失真 | Phase 0 先修正文档，再以本设计/协议规范作为实现事实源 |

实现前需要评审确认的两个产品决策：

1. IndexedDB 默认保留 30 天/100 MiB 是否符合隐私与磁盘预算；
2. 断线后遇到依赖 client 的 permission/tool 时，默认等待时长是 30 秒、5 分钟，还是跟随
   session orphan TTL。

---

## 17. v1 验收与后续增强

v1 已验收：

- [x] anureo Desk 将物化后的会话正文与 cursor 原子写入 IndexedDB，不再只缓存 session list。
- [x] 页面恢复先 hydrate IndexedDB，再用 cursor 请求增量；缺失或失效时才 `session/load`。
- [x] 每个 session 有 durable `streamId` 和严格递增 `seq`；事务失败不消耗 sequence。
- [x] history replay 使用由 `sessionId + messageIndex` 派生的稳定 message identity。
- [x] catch-up、并发 live notification、乱序、重复和 gap recovery 有确定性测试。
- [x] `missing_cursor`、`stream_changed`、`cursor_ahead`、`replay_window_exceeded` 显式要求 reset。
- [x] WebSocket 异常关闭后创建全新 transport、重新 initialize，并在 connected 前补齐增量。
- [x] prompt task 与 transport 生命周期解耦；running session 可由 replacement connection attach。
- [x] pending question 与 filesystem/terminal client bridge 可重新绑定到当前 connection，不自动批准。
- [x] capability 缺失或 `-32601` 时兼容旧 anureo，标准 ACP client 不受扩展影响。
- [x] runtime/principal/cwd/session namespace 隔离；quota eviction 与事务 abort 已测试。
- [x] `/acp` 同时进入 relay WebSocket 与 URL-token auth allowlist。

后续增强，不属于 v1 完成条件：

- [ ] 扩展响应内的原子 snapshot，替代 reset 时的标准 `session/load` fallback。
- [ ] 多 tab writer election / BroadcastChannel cursor 仲裁。
- [ ] Web、Electron、VS Code、Relay 的真实浏览器离线与配额压力 E2E。
- [ ] 10k event benchmark、可观测 metrics 和冷 session 分级恢复策略。

## 18. 最终结论

`session/load` 适合标准 ACP client 的 authoritative history replay，但不适合作为 anureo Desk
每次刷新和短断线后的高效恢复协议。目标架构应把三件事明确分层：

1. anureo checkpoint 是长期 authoritative history；
2. session event stream 是短期、连续、可按 cursor 恢复的实时事实；
3. anureo Desk IndexedDB 是按 runtime/owner 隔离的本地 projection cache。

只有同时具备 stable identity、durable sequence、原子 replay/live handoff、真正的 transport
supervisor，以及 connection-independent run lifecycle，才能实现“刷新只收增量”和“断线
重连立即追上”，而不把偶然正确的全量 replay 误当成可靠增量同步。

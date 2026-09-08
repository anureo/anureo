# Session Sync 扩展

> 状态：Implemented
> 版本：v1
> 日期：2026-08-22

`_anureo.dev/session-sync/*` 为 anureo Desk 提供按会话排序的增量恢复通道。标准 ACP `session/load` 仍是兼容回退路径。

## 游标

```json
{ "streamId": "uuid", "seq": 42 }
```

- `streamId` 标识服务端的一代事件流；服务重启或流被重建后会变化。
- `seq` 在同一 `sessionId + streamId` 内严格递增。
- 客户端只有在事件已应用并持久化到 IndexedDB 后，才能推进游标。

## `open`

请求：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/session-sync/open",
  "params": {
    "sessionId": "session-id",
    "cwd": "C:\\absolute\\project",
    "cursor": { "streamId": "uuid", "seq": 42 }
  }
}
```

服务端验证 principal、session 所有权和规范化后的 `cwd`，然后在同一临界区捕获 high-water mark 并注册当前连接。成功响应分为两种模式：

```json
{
  "mode": "delta",
  "sessionId": "session-id",
  "streamId": "uuid",
  "throughSeq": 45,
  "minReplaySeq": 10,
  "promptState": "running",
  "events": []
}
```

`delta` 中 `events` 覆盖 `(cursor.seq, throughSeq]`。没有缺失事件时可以为空。
`promptState` 为 `idle` 或 `running`，浏览器不得从缓存中的旧 `working` 字段推断运行态。

```json
{
  "mode": "reset_required",
  "sessionId": "session-id",
  "streamId": "new-uuid",
  "throughSeq": 3,
  "minReplaySeq": 1,
  "events": [],
  "resetReason": "stream_changed"
}
```

以下情况返回 `reset_required`：

- `missing_cursor`
- `stream_changed`
- `cursor_ahead`
- `replay_window_exceeded`

v1 的 reset 响应不携带快照；客户端必须回退到标准 `session/load`，完成后用新流建立基线。未来版本可增加原子 snapshot，但不能把空事件数组解释为空会话。

## 实时通知

方法：`_anureo.dev/session-sync/update`

```json
{
  "sessionId": "session-id",
  "streamId": "uuid",
  "events": [
    {
      "streamId": "uuid",
      "seq": 46,
      "eventId": "uuid",
      "emittedAt": 1787390000000,
      "payload": {
        "type": "session_update",
        "update": { "sessionUpdate": "agent_message_chunk" }
      }
    }
  ]
}
```

通知可能先于对应 `open` 响应到达。客户端必须先注册通知处理器，并在 `open` 完成前缓存同一会话的通知；之后按 `seq` 排序、去重并应用。序号不连续时不得跳跃游标，应重新执行 `open`。

历史回放产生的 `session/update` 不会再次写入增量流，避免把一次读取误记为新事件。

## `close`

`_anureo.dev/session-sync/close` 参数为 `{ "sessionId": "..." }`。它只取消当前连接的 sync 订阅，不删除会话或持久化历史。连接断开时服务端自动清理全部订阅。

## 保留窗口

v1 默认每个会话最多保留 4096 个事件或 8 MiB payload，以先到者为准。stream head 与窗口和 checkpoint 共用 SQLite 数据库；服务重启不改变 `streamId`，也不会让 sequence 倒退。数据库被重建或窗口不足时才返回 reset。

完整端到端设计与分阶段迁移见 [会话增量恢复设计](../../design/session-incremental-recovery.md)。

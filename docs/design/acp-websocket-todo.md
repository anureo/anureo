# ACP WebSocket 后续工作

## 当前范围

`anureo-server` 提供完整的 `GET /acp` WebSocket 入口。进程内分发、event cursor replay、disconnect policy、bearer auth、origin 校验、metrics 和全面的 e2e 测试均已落地。除权限相关功能外，全部完成。

## P0：连接隔离

- [x] 删除 `apps/acp/src/tools/client_bridge.rs` 中的 `GLOBAL_BRIDGE` / `OnceLock`。
- [x] 将 `ClientBridge` 作为 `AcpConnection` 的依赖注入到文件与终端工具；按 ACP session 找到所属连接，不能使用进程级全局状态。
- [x] `/acp` handler 改为进程内分发：`AcpHub::attach()` → WS `Lines` transport → `run_agent_connection()`；删除子进程 spawn、stdio line bridge 和 `ANUREO_ACP_BINARY` 依赖。
- [x] 多 session 隔离测试（`two_sessions_get_different_ids`）：同一连接创建两个 session，验证 ID 不同且 `session/list` 返回两个条目。

## P1：服务端会话中心（AcpHub）

- [x] 在 `apps/server/src/state.rs` 增加 `AcpHub`，由 `AppState` 持有。
- [x] `AcpHub::attach()` 返回持久 `anureoAcpAgent` + notification channel + lease guard；WS 连接通过 lease 机制绑定/解绑。
- [x] `attach_with(owner, resume_from)` 支持 owner 身份和 event cursor 重放。
- [x] `note_detach()` 在连接断开时通知 hub，更新 metrics 和 idle TTL 状态。
- [x] ACP session 的 thread ID、cwd、MCP 配置、模型/模式由持久 `anureoAcpAgent` 的 `SessionStore` 管理，不随 WebSocket 生命周期销毁。
- [x] 把 `anureoAcpAgent` 拆为无连接的 session/run core 与连接特有的 capabilities/bridge/output sink。`GLOBAL_BRIDGE` 已删除，替换为 per-session bridge registry（`SESSION_BRIDGES`）。`SessionEntry.connection` 字段已添加，`AcpConnection` 结构体已定义。
- [x] 每个 ACP session 使用 `SessionStore::begin_prompt` 拒绝并发 prompt，返回 JSON-RPC error -32000。

## P1：断线、重连与事件恢复

- [x] 在 `AcpHub` 中维护每 session 的递增 event cursor 和有界 `session/update` 缓冲区（容量可配置，默认 512）。
- [x] `attach_with` 支持 `resume_from: Option<EventCursor>` 参数，重放 cursor 之后的通知。
- [x] 默认断线策略设为 `persist`：连接关闭不自动取消 run。
- [x] 支持可配置的 `disconnect_policy=cancel`（`ANUREO_ACP_DISCONNECT_POLICY=cancel`），重连时取消所有活跃 generation。
- [x] Idle TTL 后台清理（`AcpHubConfig::idle_ttl_secs`），超时后取消孤儿 run。
- [x] 重连测试（`reconnect_keeps_session_store`）：断开重连后 `session/load` 恢复已有 session。

## ~~P1：权限与反向 RPC~~

> **暂缓**：`PendingPermission`、离线暂停、TTL 超时拒绝、连接版本检查均未实现。

## P2：网络安全与运营

- [x] 为 `/acp` 单独校验 `Origin`；默认仅允许 loopback browser origin，远程来源通过 `ANUREO_ACP_ALLOWED_ORIGINS` 明确配置。
- [x] Bearer 鉴权从 `Authorization` header 提取，写入 `SessionOwner` 并传入 `AcpHub::attach_with`；跨主体 attach 被拒绝。
- [x] 最大帧/消息大小限制（1 MiB）、binary frame 拒绝、invalid JSON 处理。
- [x] 结构化日志：连接/断开/reconnect/replay/stats 均有 tracing span。
- [x] `AcpHubStats`：total_connections、total_reconnects、total_disconnects、total_replay_dropped；连接关闭时输出。
- [ ] 最大并发连接数限制（当前单连接模型不需要）。
- [ ] 初始化超时（30s 内未发送 `initialize` 则关闭连接）。

## P2：测试与文档

- [x] e2e harness 覆盖全链路：
  - `full_lifecycle_initialize_new_disconnect_reconnect_load`
  - `binary_frame_is_rejected`
  - `invalid_json_returns_error_or_closes`
  - `initialize_response_contains_protocol_version`
  - `concurrent_prompt_returns_error`
  - `two_sessions_get_different_ids`
  - `reconnect_keeps_session_store`
  - `ping_pong_does_not_break_protocol`
- [x] AcpHub 单元测试：
  - `reconnect_keeps_the_same_agent_and_session_store`
  - `cross_owner_attach_is_rejected`
  - `disconnect_policy_cancel_aborts_on_reconnect`
  - `stats_track_connections_and_reconnects`
- [x] handler 单元测试：origin 校验、bearer 提取
- [x] 在 [ACP WebSocket 接入文档](acp-websocket.md) 中记录 `/acp` URL、认证、架构和 CLI 重连行为。

## 剩余工作

| 优先级 | 任务 | 说明 |
|--------|------|------|
| P1 | `anureoAcpAgent` 拆分 | 当前 monolithic 设计在单连接下足够；多连接需要拆 core/adapter |
| P2 | 初始化超时 | 30s 内未 `initialize` 则关闭连接 |
| P2 | 最大并发连接限制 | 单连接模型下不需要 |
| ~~P1~~ | ~~权限与反向 RPC~~ | 暂缓 |

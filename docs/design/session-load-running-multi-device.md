# 运行中会话的多端加载：`-32010` 全量加载拒绝问题诊断与设计修正

> **状态**: Implemented（attach 路径已落地，2026-09-03；stdio `session/resume` 保持现状）
> **日期**: 2026-09-03
> **涉及仓库**: `anureo`（本仓库，ACP server/agent）/ `../openchamber-feat-dev`（anureo Desk 前端）
> **相关代码**: `apps/acp/src/session_load/coordinator.rs`、`apps/acp/src/session.rs`、`apps/acp/src/agent.rs`、`apps/acp/src/session_update_log.rs`、`apps/acp/src/notification_router.rs`、`apps/acp/src/session_bindings.rs`、`../openchamber-feat-dev/packages/ui/src/lib/acp/acp-session-load.ts`、`../openchamber-feat-dev/packages/ui/src/components/chat/ChatContainer.tsx`
> **交叉参考**: [会话 IndexedDB 缓存与断线增量恢复](./session-incremental-recovery.md)、[session-sync 扩展规范](../acp-spec/extensions/38-session-sync.md)、[session-history 扩展](../acp-spec/extensions/36-session-history.md)、[session 生命周期](../acp-spec/02-session-lifecycle.md)、[本地环境](../dev/local-environment.md)

---

## 1. 问题现象

local 环境（pm2 `anureo-local` :3051 + `anureo-desk-local` :3151）中，打开一个**正在执行 prompt 的会话**时，聊天区骨架屏无限期停留，直到该 run 结束才能加载出内容。

- 刷新页面或换浏览器/清缓存后打开运行中会话：必现。
- 同浏览器保持打开的热视图（持有 live 流或有效 cursor）：不受影响。

## 2. 排查与证据（2026-09-03）

**日志侧**（`C:\Users\heycj\.pm2\logs\`）：

- `anureo-local-error.log`：除子代理进度刷屏（`↳ [explore] ...`）外无任何错误；`AddrInUse` 为 09-01 旧账。
- `anureo-desk-local-error.log`：仅 `[ACP Proxy] Error: read ECONNRESET`（页面刷新断开 WS 的正常噪音）。

**时间线**：

| 时刻 | 事件 |
|---|---|
| 13:16 / 13:21 | anureo-local 两次重启；13:21 起实例启用密码门（`ui-password.txt` + `jwt-secret` 新建） |
| 13:53–14:13 | 会话 `ae92b298`（workspace 架构 review）运行中，explore 子代理持续刷屏 |
| 14:01:51 | desk-local 记录 ECONNRESET —— 用户刷新页面，此时会话仍在运行 |
| 14:17 | run 收尾（memory 写入） |
| 16:05 | WS 探测：会话可正常加载，`promptState=idle` |

**WS 探测**（PowerShell ClientWebSocket，in-band 认证链 `initialize → -32001 → login → authenticate → initialize`）：

- 对 `ws://127.0.0.1:3051/acp`（直连）与 `ws://127.0.0.1:3151/acp`（Express hpm 代理）分别执行：能力通告 `session-recovery {version:1, cursor:true, orderedUpdates:true, promptState:true}` 正常；带 `_meta.anureo.dev.sessionRecovery` 的全量 `session/load` 成功，响应含 `{mode:"full", streamId, throughSeq, promptState}`。

**结论**：传输、代理、认证、能力协商均健康。卡点唯一：**运行中会话的全量 `session/load` 被后端拒绝（`-32010`）**。

## 3. 根因链

### 3.1 拒绝点

`load_full_session`（coordinator.rs:112-137）无条件调用 `sessions().begin_restore()`；`begin_restore`（session.rs:579-596）在与 `begin_prompt` 共用的 `control_lock` 下检查 `current_turn`，非空即 `Err(())` → `-32010 "a prompt is already in progress for this session"`。互斥的目的（session.rs:573-577 注释）是杜绝「检查通过后、绑定完成前 prompt 才启动」的竞态。

### 3.2 前端表现

`ChatContainer.tsx:1015-1026`：`isSessionBusyError`（-32010）→ 骨架屏 + 每 2s 静默重试全量加载，直到 run 结束。冷客户端（无 cursor）无法走 delta 路径，只能全量 → 必然被拒 → 观感即「加载不出」。

### 3.3 多端查看的架构本已存在

- **绑定模型**：`session_to_connections: HashMap<SessionId, HashSet<ConnectionId>>`（session_bindings.rs:11）——一个会话对应连接集合。
- **live 广播**：`NotificationRouter` 按 `connections_for(session_id)` 集合 fan-out（notification_router.rs:74-100）。
- **运行中增量补齐**：delta 路径 `read_after_cursor(..., prompt_state)`（coordinator.rs:39-110）不经过 `begin_restore`，运行中放行；事件持久化于 `acp_session_sync_events`（streamId/seq 重启连续）。

即「多设备同时观看运行中会话」是设计内能力，入口是 delta；缺口在冷客户端没有 cursor，只能走全量。

### 3.4 全量加载被拒的理由并不成立于活会话

- **活会话全量加载不做任何 reset**：`load_session_for_owner`（agent.rs:1494-1510）发现条目在内存中即直接复用（"Reusing existing session entry from memory"），不触发磁盘恢复；危险的生命周期切换只存在于进程重启后从 checkpoint 重建的路径。
- **但重放源是 checkpoint**：非 delta 全量加载一律读 SqliteSaver checkpoint 重放历史（agent.rs:1557-1651），checkpoint 是 turn 边界快照，运行中读到的是滞后视图；且有 tail 截断（`history_tail_start`），依赖 36-session-history 翻页补齐。这是拒绝的实质动机，但代价是把无害的活会话 attach 一并挡掉。

## 4. 设计修正提案（已实现，2026-09-03）

落地情况（`apps/acp`，727 单测全绿 + clippy 零警告）：

- **coordinator 分流**：`load_full_session` 先查 `sessions().get()`——条目在内存中则跳过 `begin_restore`、不触碰 lifecycle（含运行中 turn），bind 后由 coordinator 直接重放；仅冷条目走 restore lease（`-32010` 只在冷条目 + 运行中 turn 的矛盾态出现）。
- **重放源改为 seq 化事件流**：`SessionUpdateLog::read_full_stream` 新方法，cursor 取 `min_replay_seq - 1`，重放整个保留窗口（含 mid-turn 事件），响应 `mode:"full"` + `{streamId, throughSeq, promptState}`，与 live 更新共享同一 (streamId, seq) 连续空间；多端下其他连接按 seq 去重（与 delta 同机制）。
- **checkpoint 重放收窄**：`load_session_for_owner` 仅在 `entry_was_created`（真恢复）或无 sessionRecovery 元数据的 legacy 客户端时做 checkpoint 重放；`finish_restore→Idle` 收窄为 `entry_was_created`，活会话 attach 不再可能把 Running lifecycle 打回 Idle。
- **顺带修复**：`UpdateLogRepository::append` 原先在 stream 未初始化时静默丢弃事件（`Ok(None)`），现改为经 `ensure_stream` 自建——新会话首个 live 事件不再可能丢失。
- **保留现状**：stdio `session/resume`（editor 单客户端协议）仍走 restore lease + `-32010`；冷恢复路径（`mark_loading` + checkpoint + tail 截断 + session-history 翻页）不变。

原始提案中「question rebind 多端归属」与「FE 可选加固」仍未改动：rebind 转移语义天然满足 4.3 约定；FE 冷打开现在直接成功，busy 重试路径成为纯防御。

## 5. 验证

已验证：

- **单测**：`session_update_log` 6 项含新增 `full_stream_replays_retained_window_without_cursor`（窗口裁剪后全窗口重放 + promptState 透传）；`anureo-acp` 全量 727 项通过；`cargo clippy -p anureo-acp --all-targets` 零警告。
- **真实浏览器实测**（Playwright 冷上下文，经 3151 Express 代理 → 3051）：门禁登录后点击会话 → `session/load` 发出 → 3652 条 `session/update` 回放帧 + 响应 `mode:"full"` `throughSeq:3652` 全部到达 FE，native store（useAcpSessionStore）`messages=7 / messageOrder=7` 完整。

实测中发现的 **FE 侧剩余问题**（`openchamber-feat-dev`，与本后端修复正交）：

1. **投影解析器曾是「注册时一次性」绑定**——SyncProvider 在冷启动会重挂载多次（实测 13s 内 7 次，dir `/`→`C:/Users/heycj/dev/loom`），而 `setAcpProjectionResolver` 保留最后注册的闭包；若某次 remount 后未重新注册，replay 会投影进已卸载 manager 的 store（分裂脑：store 有数据、UI 永远为 0、无任何报错）。已修：resolver 注册拆分为独立 effect（不依赖 acpRuntime），每次注册均从 native store 全量重投影，remount 自愈。
2. **冷门禁登录后 `currentSessionId` 不落库**（已修，根因不在路由链）：门禁登录 → 点击会话 → 路由已至 `/session/<id>`、`setCurrentSession(id)` 正常落库、回放全部到达、投影 `records list=7`——但 **turn 分组为 0**：`useChatTimelineController` 的 turn 模型只认 `role==="user"` 起头，而 **live 流水线从不产生 `user_message_chunk` 事件**（stream_bridge.rs 顶部注释言明该变体仅由旧 checkpoint 回放合成），seq 化事件日志里没有用户轮 → 投影全为 assistant → turns=0 → 消息区恒空。已在后端 `execute_prompt` 落点补 `record_user_turn`（合成 `user_message_chunk` 经 updates_tx 记录+广播），端到端验证：既有会话追加一轮新对话 → 换浏览器冷打开 → 1.5s 内渲染，多次稳定复现。附带发现：门禁登录需 `ANUREO_ACP_ALLOWED_ORIGINS` 含桌面 origin（如 `http://localhost:3151`），否则 WS 握手 403；该配置已从运行时 env 固化进 `scripts/ecosystem.config.cjs`（local profile）。

## 6. 相关讨论

- 诊断过程中的 WS 探测脚本（in-band 认证 + session/load）为一次性脚本，已删除；要点见第 2 节，重写成本低。
- 排查时的现场快照：ui-password/jwt-secret 均在 `.anureo-home-local`（实例隔离正确，未触发 [本地环境](../dev/local-environment.md) 第 2 节的 jwt-secret 传染坑）。

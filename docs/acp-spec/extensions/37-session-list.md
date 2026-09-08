# 37 - Session Index（全局会话列表、归档与实时同步）

> **状态**: Draft（协议已在 anureo/Desk 主路径接入；跨平台兼容与性能验收仍未完成）
> **相关代码**: `apps/acp/src/extensions/session_list.rs`、`apps/acp/src/session_repository.rs`、`../anureo-feat-dev/packages/ui/src/lib/acp/acp-api.ts`、`../anureo-feat-dev/packages/ui/src/stores/globalSessions.ts`
> **交叉参考**: [Session List 索引与稳定排序重设计](../../design/session-list-redesign.md)

---

## 1. 范围与兼容边界

标准 ACP `session/list` 与 anureo Desk 私有 `_anureo.dev/session/list` 是不同 JSON-RPC 方法，不存在线路冲突：

| 方法 | 职责 |
| --- | --- |
| `session/list` | ACP-compatible active session projection；按当前 owner/cwd 隔离。 |
| `_anureo.dev/session/list` | anureo Desk 全局 SessionIndex；支持 active/archived、metadata、parent、snapshot、revision。 |
| `_anureo.dev/session/list-global` | 迁移期 legacy projection adapter；共享新 query/snapshot core，最终删除。 |

当前实现已经提供签名 snapshot cursor、canonical global event projection 与 Desk subscription；`list-global` 仍作为迁移期 alias 保留。本文其余部分描述目标协议；在实现、测试和兼容矩阵全部完成前，不得将其标记为“已实现”。

### 1.1 Alias 观测接口

迁移期提供独立的只读扩展方法 `_anureo.dev/session-metrics/status`，避免向旧 `session` capability 增加新方法：

| 项目 | Contract |
| --- | --- |
| params | JSON object；当前为空对象 `{}`，非 object 返回 `-32602`。 |
| 权限 | `ctx.principal` 去空白后不能为空，否则返回 `-32002 forbidden`。 |
| response | `{ "legacyListGlobalCalls": number }`；只统计 `_anureo.dev/session/list-global`，不统计 canonical `list`。 |
| 一致性 | 进程内原子累计值；重启归零。它用于诊断/灰度，不是持久化审计日志。 |
| 发布门槛 | 只有真实生产环境连续 14 天为 0，且最低支持 Desk 已包含 `listIndex`，才允许删除 alias。 |

## 2. Capability

过渡期 anureo 在 `initialize.agentCapabilities._meta["anureo.dev"].session.methods` 发布：

```json
["list", "list-global", "archive", "update", "delete"]
```

Desk 选择规则：

1. 明确含 `list`：调用新方法。
2. 明确只有 `list-global`：直接调用 alias。
3. capability 不可判定：先调用新方法，只在 JSON-RPC `-32601 method_not_found` 时回退 alias。
4. `-32001 capability_not_supported` 表示 `session` extension 域不存在，调用同域 alias 不能恢复；权限、数据库、参数、cursor 与 snapshot 错误也不得 fallback。

兼容窗口结束后新 anureo 的 methods 收敛为 `["list", "archive", "update"]`。新 Desk 对最低支持的旧 anureo 继续保留 `list-global` 直调/`-32601` fallback；只有最低支持 anureo 版本另行提高后才能删除客户端 fallback。

## 3. `_anureo.dev/session/list`

### 3.1 请求

首次请求：

```json
{
  "archived": "all",
  "directory": "C:/repo",
  "limit": 200
}
```

| 字段 | 类型 | 语义 |
| --- | --- | --- |
| `archived` | `"all" \| "active" \| "archived"` | 默认 `"all"`。Desk full load 与 directory refresh 必须使用 `"all"`，一次 snapshot 覆盖两个 membership 分区。 |
| `directory` | string/null | 可选，按服务端规范化后的 cwd 精确匹配；Windows verbatim prefix 与分隔符处理必须与标准 list 共用。 |
| `limit` | integer | 默认 200，范围 1～1000。 |
| `cursor` | opaque string | 后续页原样回传；带 cursor 时不得改变其他 filter。 |

后续页：

```json
{ "cursor": "opaque" }
```

### 3.2 响应与 `SessionIndexRecord`

```json
{
  "sessions": [
    {
      "sessionId": "ses_123",
      "parentSessionId": null,
      "cwd": "C:/repo",
      "title": "Implement session index",
      "createdAt": "2026-08-21T10:00:00Z",
      "activityAt": "2026-08-21T10:10:00Z",
      "treeActivityAt": "2026-08-21T10:12:00Z",
      "stateChangedAt": "2026-08-21T10:08:00Z",
      "metadataUpdatedAt": "2026-08-21T10:09:00Z",
      "archivedAt": null,
      "closedAt": null,
      "lifecycle": "idle",
      "metadata": { "anureo": {} },
      "revision": 42,
      "indexVersion": 108
    }
  ],
  "nextCursor": "opaque",
  "hasMore": true,
  "snapshotVersion": 108
}
```

所有时间均为 UTC RFC 3339、固定 6 位小数。`parentSessionId`、`title`、`stateChangedAt`、`metadataUpdatedAt`、`archivedAt`、`closedAt` 可为 `null`；其他示例字段必须存在。`revision` 是单 record version，`indexVersion` 是该 record 最近变化所属的 owner-wide transaction version；二者为 JSON safe integer `1..=9_007_199_254_740_991`。`snapshotVersion` 是 materialize 时的 owner version并在全部页面保持不变，范围为 `0..=9_007_199_254_740_991`；尚无任何 mutation 的空 owner 返回 0。所有 increment 必须 checked；溢出返回 `-32603 version_exhausted` 且 transaction 回滚。`lifecycle` 只能为 durable `"idle" | "closed"`，与 archive membership 正交；live busy 状态不进入 index。`metadata` 必须为 JSON object，空值返回 `{}`。wire 唯一 ID 字段为 `sessionId`；不得混用 `sessionID` 或 `id`。

排序形成完整总序：active effective root 按 `treeActivityAt DESC, sessionId ASC`；同 active parent child 按 `activityAt DESC, sessionId ASC`；archived 按 `archivedAt DESC, sessionId ASC`。effective root 是 parent 为 null，或 parent 不在同一 active scope 的 active record。后者覆盖 parent 已归档/删除、partial failure 和损坏 legacy parent；child 必须保持可见且 parent ID 不被静默改写，parent 恢复后自动重新挂回。服务端返回平铺记录和 parent ID，Desk 负责构树。

`archived="all"` 的 materialized sequence 为 active effective-root forest 的稳定 pre-order traversal，随后是 archived records。每个 active ID 必须恰好可达一次；orphan/cycle 用 effective-root/cycle guard 安全降级。`hasMore=true` 时 `nextCursor` 必须非 null；`hasMore=false` 时必须为 null。

### 3.3 Snapshot 与 cursor

首次请求创建 owner/filter/sort 绑定的短期 snapshot。Desk 使用 `archived="all"`，因此 active/archived 来自同一时点，不允许再用两个独立 snapshot 拼接。

Snapshot 首次 materialize 在同一个 SQLite read transaction 中读取 owner `current_version` 和完整 records，确保 `snapshotVersion` 与 projection 同时点；随后使用 anureo 进程内 immutable projection，冻结顺序/filter/metadata，后续页不回表。accounted bytes = compact canonical record JSON 的 UTF-8 bytes + 64 bytes/record + 256 bytes/snapshot。TTL 从创建起固定 5 分钟且不续期；每 owner 最多 4 个/64 MiB，全进程最多 256 MiB。创建前清过期项，再按 `last_access_at` 最旧优先淘汰（tie-break snapshot ID ASC）；单 snapshot 超过 64 MiB或淘汰后仍超全局上限返回 `snapshot_capacity_exceeded`。

Opaque cursor 逻辑上包含 version、128-bit random snapshot identity、offset 与 filter hash，并用进程启动时生成的 256-bit secret 做 HMAC-SHA256；wire 最大 1024 bytes。签名/格式/未知 version/非法 offset 返回 `invalid_cursor`；签名有效但 snapshot 不存在、过期、owner/scope 不匹配或 server 已重启返回 `snapshot_expired`。后续请求只允许 `cursor`，limit/filter 由 snapshot 冻结。客户端不得把这些错误解释为空成功。

### 3.4 Authoritative absence

只有全部页面成功完成后，snapshot 才对绑定 scope 具有 authoritative membership 语义：

- 无 directory 的 `all` snapshot 覆盖该 owner 全部 session。
- 带 directory 的 `all` snapshot 只覆盖规范化 cwd 精确匹配的记录。
- commit 删除旧 cache 中属于成功 scope、但未出现在 snapshot 的记录；保留 scope 外记录。
- 失败、过期或未完成 snapshot 不删除旧 cache。
- event/tombstone overlay 按 revision 合并后胜出；显式 optimistic create shadow 最后叠加，普通 directory live record不是 membership 例外。
- client tombstone 只有在成功完成 owner-wide `all` snapshot 且 `snapshotVersion >= tombstone.indexVersion` 后才能清理；directory snapshot 不清 tombstone。

## 4. SessionIndex 字段与 mutation 语义

`acp_sessions` 是列表事实源。目标 schema 增加 `parent_session_id`、`activity_at`、`tree_activity_at`、`state_changed_at`、`metadata_updated_at`、`revision` 与 `index_version`；另建 owner version 表 `acp_session_index_state` 和持久化 delete 结果表 `acp_session_tombstones`。

- `activity_at` 只表示实际 user/agent 工作。prompt 完成校验并取得 busy lease 后、executor 启动前提交一次 Activity；提交失败则不启动 executor。校验失败、busy 拒绝、history replay 不更新；受理后即使取消、模型或 executor 失败也保留 activity。token/part/tool progress 不修改它；title、metadata、archive、restore、close 也不修改它。
- `tree_activity_at` 是当前 session 与沿全 active parent edges 可达后代的最大 activity，不是历史 high-water。closed 但未 archived 的记录仍计入；archived parent 切断上方 tree，仍 active child 成为 effective root。
- archive/delete/reparent 可能使旧祖先的 tree activity 下降，并使新祖先上升。
- create、activity、archive/restore、lifecycle、metadata、内部 repair reparent、delete 必须走统一 repository mutation API；公开 extension 不提供 reparent，parent 创建后不可变。
- 每个可见 projection 变化递增 record revision；tree 重算改变祖先时，祖先也递增 revision 并发布 updated event。
- 同一个 mutation transaction 只递增一次 owner `current_version`，所有改变 records/tombstone 共用该 `indexVersion`。内容相同的 title/metadata、相同 archive/lifecycle 状态是 no-op，不更新时间/version或发布 event。
- 尚无 state row 的 owner 读取为 0，第一次真实 mutation 在同一 transaction 插入/增至 1。每个 index 时间写入固定 6 位小数，取 `max(truncate_to_microseconds(clock.now), previous + 1µs)`，避免系统时钟回拨造成时间倒退。

Migration 在一个 `BEGIN IMMEDIATE` transaction 中回填旧 records：parent 为 null，activity/tree 回退 `updated_at/created_at`，state time 回退 `archived_at/closed_at`，metadata time 为 null，两个 version 均为 1。`acp_session_data` 必须重建为 `ON DELETE CASCADE` foreign key；所有 connection 启用 `PRAGMA foreign_keys=ON`，既有 orphan 不复制并记录数量，最后执行 `foreign_key_check`。任一步失败整体回滚。

### 4.1 创建与 parent contract

标准 `session/new` 使用 `_meta["anureo.dev"]` 携带 `title`、`parentSessionId` 和初始 `metadata`。parent 与 child 必须属于同一 owner，规范化后的 cwd 必须相同；unknown parent、self-parent、cycle 或跨 cwd 返回 `-32602 invalid_params`。顶层使用 `parentSessionId: null`。

anureo 必须在同一事务内写入 index、metadata、owner version 与 ancestor tree updates，然后在标准 response 的 `_meta["anureo.dev"]` 返回 `{ session: <full record>, affectedSessions: <nearest-ancestor-first full records>, indexVersion }`。anureo 先发布同构 `session.created`，再发布 ancestor updated；Desk 以 response 替换 optimistic shadow并更新 ancestors，同 revision events 只作幂等 echo。

本规范不扩展 message-bounded fork。标准 `session/fork` 不接收 `sourceMessageId`；Desk 不得传入后静默忽略该参数。“从 assistant plan 创建 session”是客户端读取 plan、新建 session、再发送 prompt，不表示服务端在某条 message 处截断历史。

## 5. Global session events

topic 为 `session`。created/updated 的 `properties.info` 与 list item 使用同一个 serializer/schema：

```json
{
  "topic": "session",
  "event": {
    "type": "session.updated",
    "properties": {
      "info": { "sessionId": "ses_123", "cwd": "C:/repo", "createdAt": "...", "activityAt": "...", "treeActivityAt": "...", "lifecycle": "idle", "metadata": {}, "revision": 42, "indexVersion": 108 }
    }
  }
}
```

删除使用唯一 tombstone envelope：

```json
{
  "topic": "session",
  "event": {
    "type": "session.deleted",
    "properties": {
      "tombstone": { "sessionId": "ses_123", "cwd": "C:/repo", "parentSessionId": null, "revision": 43, "indexVersion": 109, "deletedAt": "...", "deleted": true }
    }
  }
}
```

嵌套 `{topic,event}` 是现有 GlobalEventBus 与 Desk event source 的 transport contract。Desk 的 list adapter 与 event router 必须复用一个 strict `SessionIndexRecord` parser。兼容窗口内的 legacy OpenCode-like `id/time` event 经过隔离 adapter，不得进入新 parser。客户端拒绝 revision 小于等于当前 record/tombstone 的数据；分页期间事件作为 overlay 暂存，snapshot 成功后原子合并。

Global session topic 只允许已提交的 SessionIndex mutation。当前 runtime 对每个标准 `session/update` 发布的 minimal `{id}` `session.updated` 必须删除；标准聊天增量不进入 global index topic。每个 prompt 至多产生一次 Activity mutation/full-record event，history replay 不产生 index event。

Global bus 是 fire-and-forget change signal，不是可靠日志：queue full/connection closed 时允许 drop，已提交 DB transaction 不回滚。actor 以 RPC response 收敛；其他 client 在 bootstrap、reconnect、runtime switch 后立即 owner-wide resync，并在 visible+online 时由 global store 每 60 秒 singleflight resync。hidden/offline 停止 timer，恢复且距上次成功满 60 秒时立即 resync。drop 必须有 counter 和不含内容/metadata 的结构化日志。

## 6. `archive` 与 `update`

### `archive`

```json
{ "sessionId": "ses_123", "archived": true }
```

archive/restore 更新 `archived_at`、`state_changed_at` 与 revision，不修改 `activity_at`；若可见树变化，必须重算祖先并发布对应 updated events。

响应固定为：

```json
{
  "session": { "sessionId": "ses_123", "revision": 43, "indexVersion": 109 },
  "affectedSessions": [
    { "sessionId": "ses_parent", "revision": 18, "indexVersion": 109 }
  ],
  "indexVersion": 109
}
```

示例 record 是缩写；线上均为完整 `SessionIndexRecord`。`affectedSessions` 不含 target，按 nearest-ancestor-first。no-op 返回当前 target、空数组和当前 owner version，不发 event。

### `update`

```json
{
  "sessionId": "ses_123",
  "title": "New title",
  "metadata": { "anureo": { "goal": { "status": "active" } } }
}
```

`title` 与 `metadata` 至少提供一个；metadata 必须是 object。两字段在同一 transaction 原子更新；任一非法时均不写。真实变化更新 `metadata_updated_at`、revision/indexVersion，不修改 activity；相同内容为 no-op。响应固定为 `{ "session": <full SessionIndexRecord>, "metadata": <object>, "indexVersion": n }`。

### 标准 `session/delete` response extension

delete 在同一 transaction 持久化 tombstone 后删除 session-owned rows；ACP response 在 `_meta["anureo.dev"].tombstone` 返回与 event 完全同构的 tombstone。重复 delete 永久返回原值，不递增 version或发 event。服务端保留不含 metadata/title 的紧凑 tombstone，不做 TTL 清理，session ID 永不复用；客户端可按覆盖 snapshot 清理本地 tombstone。

## 7. 标准 ACP `session/list`

标准 list 使用同一个 SessionIndex repository、cwd normalization 和 active ordering，但只投影 ACP `SessionInfo`。为兼容当前只调用一次的 clients，本次固定一次返回当前 owner/cwd 的全部 active records，`nextCursor=null`；不启用标准分页。未来改变该行为必须另立兼容设计。

## 8. 错误

| code | message | 场景 |
| --- | --- | --- |
| `-32601` | `method_not_found` | extension 域存在，但子方法不存在；未知 capability 时可回退旧 alias。 |
| `-32602` | `invalid_params` / `invalid_cursor` | 请求字段、enum、limit、cursor 格式/version/offset 非法。 |
| `-32001` | `capability_not_supported` | `session` extension 域未注册；不得回退同域 alias。 |
| `-32003` | `not_found` | mutation 目标不存在或不属于当前 owner。 |
| `-32004` | `snapshot_expired` | snapshot 过期、不存在或与 owner/filter 不匹配；客户端丢 page accumulator 后有限次从第一页重载。 |
| `-32005` | `snapshot_capacity_exceeded` | 单 snapshot 超过 64 MiB，或按规则淘汰后进程仍无法满足 256 MiB 上限；保留旧 cache，不 fallback。 |
| `-32603` | `internal_error` / `version_exhausted` | SQLite、version checked increment 溢出或未分类内部错误；transaction 回滚，保留旧 cache，不 fallback。 |

## 9. 迁移与实现索引

1. 新 anureo 同时注册 `list` 与 `list-global`。两者共享同一 repository/snapshot query core；alias 仍接受旧 boolean `archived`，并返回旧 Desk 依赖的 `updatedAt` descriptor。它不是第二套事实源，但不能把新 record 原样返回给旧客户端。
2. 新 Desk 增加 `listIndex` adapter、三态 capability 选择、owner-wide snapshot、scoped authoritative replace、revision merge 与 production global subscription。
3. 三种兼容组合全部通过后，要求 stable/canary 连续 14 天 alias 零调用且最低支持 Desk 已含 `listIndex`；无集中遥测时至少保留 2 个 stable releases，然后从 anureo 删除 alias 并收敛 capability。Desk 的旧 anureo fallback 保留到最低支持 anureo 版本另行提高。
4. 标准 ACP `session/list` 与私有 list 都从 SessionIndex 派生，但保留不同 projection。

实现入口：

- anureo handler/event：`apps/acp/src/extensions/session_list.rs`
- anureo repository/schema：`apps/acp/src/session_repository.rs`
- anureo owner-scoped dispatch：`apps/acp/src/agent.rs`
- Desk ACP adapter：`packages/ui/src/lib/acp/acp-api.ts`
- Desk mapping：`packages/ui/src/lib/acp/type-mapping.ts`
- Desk global store：`packages/ui/src/stores/globalSessions.ts`

## 10. 当前实现状态（2026-08-21）

已实现 SessionIndex schema/query、owner version projection、`list` handler 的签名 snapshot cursor、标准 ACP active projection、Desk `listIndex` adapter/能力声明选择与旧 anureo fallback、标准 ACP delete 的 durable tombstone `_meta`、Desk global session stream 接线，以及 global event drop counter；prompt activity 边界、active ancestor tree activity 传播、稳定树前序排序、单事务 title/metadata mutation、canonical created/updated/deleted event、基于 canonical JSON 的 snapshot byte accounting、Desk tombstone shadow/versioned cleanup、60 秒 singleflight resync、UTC 微秒固定格式/单调时间写入，以及 create/archive/update canonical response merge 也已接入；archive/restore changed records 在事务提交前 materialize。`session/new` 现在通过单个 repository transaction 原子写入 target、parent、title 和 metadata，target 与 nearest ancestors 共享同一个 `indexVersion`，标准 response `_meta` 返回完整 `affectedSessions`；Desk create action 已消费该字段并有回归测试。标准 ACP `session/new` 成功路径也会按 target created、nearest-ancestor-first updated 的顺序发布 global events，event info 补齐 SessionIndex 字段。snapshot quota 已按 canonical wire JSON + 64 bytes/record + 256 bytes/snapshot 精确计费，并有 metadata 计费回归测试；新增真实 `anureoAcpAgent + SessionListHandler` 分页 loopback 回归，验证固定 `snapshotVersion`、cursor 连续性和跨页无重复/遗漏。Desk global store 的 runtime-switch generation guard 也会保护 singleflight 引用，旧 runtime 的迟到 promise 不会清空新 runtime 的 in-flight load，并有旧/新响应交错回归测试。Desk `listIndex` 仅在 `-32601 method_not_found` 或无 code 的 legacy 文本错误时回退 `list-global`，并有错误分类和 legacy projection 回归测试。Desk 新增 10k rich snapshot merge 基准，每条 session 带 1 KiB metadata，严格模式下 p95 预算 500ms。Desk archive action 也增加了 target-only optimistic move 与避免覆盖飞行期间 canonical event 的 rollback guard；create action 增加了未注册 ACP 临时 record 的确认/失败回滚；shared ACP runtime 增加 endpoint switch 清理和 stale generation guard；global topic refcount 按 runtime 隔离；update response 显式返回空的 `affectedSessions`；SessionListHandler 支持注入 clock 以覆盖 TTL 测试，并在新 snapshot quota 判断前淘汰 owner 最旧快照；owner indexVersion 使用 JSON-safe checked increment，session/tombstone revision 也由 checked increment 与 SQLite trigger 共同限制在 JSON safe integer 范围内；Desk archived metadata patch 已移除 active-only `session.get` 预读；repository 已加入 10k session × 20 次 full-read smoke fixture；SessionListHandler 已加入真实 anureo agent archive wire-level target/ancestor fixture。anureo clippy 与 Desk lint 已通过；global stream 真实 App remount 的 runtime-switch、update response 的跨 ancestor 联调与完整兼容/性能矩阵仍在后续任务中；本规范仍保持 Draft。

### 最新验证补充（2026-08-22）

以下结果更新并 supersede 上述“后续任务中”的历史状态描述；规范仍保持 Draft，仅表示最终跨平台发布门槛尚未关闭。

- `e2e/features/web/runtime-switch.feature` 已在 real ACP 环境通过：runtime endpoint changed 事件触发 App remount 后重新发出 session list 请求；随后 UI archive 收到真实 `_anureo.dev/global/update` / `session.updated` wire event，active session 正确移出列表。
- `session/update` wire 回归确认 title/metadata mutation 是 target-only，返回固定 `affectedSessions: []`，parent revision/indexVersion 不变；archive mutation 仍返回 target 与 changed ancestors。
- Desk `updateSessionTitle` action 回归确认会合并 canonical target 与 response 中的 affected records，保持 target/ancestor 的 revision/indexVersion 字段。
- Desk compatibility matrix 覆盖 canonical/legacy capability 选择、`-32601`/`-32001`/`-32602`/业务/存储/连接错误分类、legacy active+archived 合并的 active 优先去重；anureo capability wire 回归确认迁移窗口同时声明 `list`、`list-global`、`archive`、`update`、`delete`。
- COMP-02 wire 回归确认旧 `list-global` 从共享 SessionIndex/query core 返回 legacy projection，并不会泄漏 `revision/indexVersion` 等 canonical 新字段。
- Desk COMP-01 fallback 回归确认仅支持 `list-global` 的旧 anureo 会被直接读取 active/archived 两个分区，不先 probe canonical method；合并后 active 优先去重并明确不可分页。
- Desk compatibility behavior fixture 已覆盖新 Desk+新 anureo、旧 anureo capability、未知 capability + canonical method missing 三种 request path；分别验证 canonical、直接 legacy fallback、以及仅 `-32601` 触发 fallback。
- 10k canonical SessionIndex full-read fixture（每条 1 KiB metadata、重复 20 次）在严格模式下通过；extension snapshot fixture 覆盖 opaque cursor 的跨页稳定性。旧 `updated_at` keyset benchmark 已随 legacy repository 旁路删除。Desk merge 额外输出 heap 与 `process.cpuUsage()` 采样。
- ACP lib 当前全量为 594 tests passed，`cargo clippy -p anureo-acp --lib -- -D warnings` 通过；兼容窗口与跨平台 CPU/RAM 复测仍未达到最终发布完成定义。
- 并发补充：`SessionConfigStore` 与 SessionRepository 统一使用 30 秒 SQLite busy timeout，atomic `session/new` index write 对 busy/locked 做 bounded exponential 8-attempt retry，并有 8-worker × 4-session 压力回归；此前分页/创建并行测试中的偶发 `database is locked` 已消除。
- Agent startup 在打开 SQLite stores 前会幂等重建 `ANUREO_HOME` 父目录，archive mutation 也复用 SQLite retry helper；连续全量运行保持 594/594 通过。
- 标准 ACP `session/delete` 首次成功会发布带完整 tombstone 的 `session.deleted` global event；重复删除只返回 durable tombstone，不再次广播。删除 response 的 `_meta["anureo.dev"]` 同时包含共享 `indexVersion` 与 nearest-ancestor-first `affectedSessions`；删除 target 后，服务端在同一事务内按剩余可见后代重算受影响 ancestor 的 `tree_activity_at`/`revision`，并为这些 ancestor 发布 `session.updated`。extension delete 与标准 ACP delete 遵循相同 contract。
- Desk delete action 会先写入 target tombstone，再按 response 的 `affectedSessions` 合并 ancestor canonical records；事件是幂等 echo，而不是 Desk 修正 ancestor 的唯一来源。
- ACP lib 串行全量测试当前为 598/598 通过，`cargo clippy -p anureo-acp --lib -- -D warnings` 通过；跨平台 CPU/RAM 采样、完整兼容矩阵和多版本 Desk 联调仍未关闭最终发布门槛。
- Desk 20-run 10k rich merge strict benchmark 已通过，并输出 runner platform/arch/CPU/RAM；当前仅有 Windows 采样，Linux/macOS 仍需相同命令复测。
- anureo 源码事实源审计确认标准 list、canonical private list 与 legacy alias 均使用 SessionIndex；`SessionRepository::list_for_restore` 只用于进程重启恢复，不参与任何外部 membership/order projection。
- 标准 delete 的连接边界：live session 必须绑定当前 ACP connection 才能首次删除；未绑定 live session 返回 `-32011`。只有同 owner 且已有 durable tombstone 的解绑后重试才允许幂等返回，且不重复广播 delete event。
- anureo runtime metrics 现在维护 `legacy_session_list_alias_calls`，并通过受 principal 保护的 `_anureo.dev/session-metrics/status` 只读返回 `{ legacyListGlobalCalls }`；它只计数 legacy `list-global` 请求，不能替代生产 14 天观测。
- Desk CI 已新增 `session-index-performance` 的 Ubuntu/macOS/Windows 矩阵，统一运行 strict 20-run rich merge benchmark；CI 输出的 runner 信息用于补齐跨平台性能验收，本地 Windows 结果不再被误报为完整矩阵。
- strict benchmark 现在同时 enforced p95 ≤500ms 与 heap delta ≤64MiB；当前 Windows 20-run 结果约 32ms/26MiB，通过。

# Session List 索引与稳定排序重设计

> **状态**: Draft（可执行开发版；未达到完成定义）
> **相关代码**: `apps/acp/src/session_repository.rs`、`apps/acp/src/extensions/session_list.rs`、`apps/acp/src/agent.rs`、`../anureo-feat-dev/packages/ui/src/stores/globalSessions.ts`
> **交叉参考**: [Session List 规范](../acp-spec/extensions/37-session-list.md)、[标准 ACP 单 Server 多 Session 实现方案](./acp-single-server-multi-session.md)
>
> **签收模板**: [Session List Release Sign-off](./session-list-release-signoff.md)

---

## 0. 开发指导完整性评审（2026-08-22）

**结论：目前可以指导分阶段开发，但还不能声称“开发者只看本文即可完成并验收”。** 文档已经具备 RFC 所需的协议字段、数据模型、错误语义、依赖顺序、代码落点、测试编号和完成定义；实现人员可以据此拆分任务并开始编码。可是，当前实现仍处于迁移中的 Draft 状态，目标契约与已落地代码之间还存在必须在开发/联调阶段关闭的差距。

这里的“不能完全指导”不是指核心实现缺少设计，而是指交付闭环尚未封口：真实新旧版本组合、跨平台 CI、生产 alias 观测、迁移/回滚操作手册和最终删除 alias 的批准证据仍属于外部发布工作。另有若干章节保留了 2026-08-21 的源码现状快照；这些章节用于解释问题来源，**当前代码状态以 §13（2026-08-22）和 37 号规范为准**，不能把历史快照中的“尚未实现”直接当作今天的结论。

| 评审项 | 当前结论 | 完成前必须补齐 |
| --- | --- | --- |
| 契约可执行性 | 已足够 | 以 37 号规范为唯一 wire source of truth，并把任何实现偏差登记为变更记录。 |
| 代码落点 | 已锁定 | 核心 repository、handlers、Desk adapter/store/action 与对应测试入口已列明；最终仍需一次源码检索确认没有旁路写入。 |
| anureo 实现 | 核心完成 | tree activity 下降、mutation/event contract、标准 list SessionIndex 收敛、legacy helper 删除和 delete tombstone/ancestor response 已有源码与回归证据；global topic 的最终发布清理仍受兼容窗口约束。 |
| Desk 实现 | 核心完成 | canonical list、revision/tombstone merge、delete ancestor merge、runtime switch/reconnect guard 与失败回滚已有实现和测试；真实多版本运行时联调仍未完成。 |
| 兼容性 | 测试完成、联调未完成 | fake behavior fixture 已覆盖三种 request path；COMP-01～03 仍需要真实新旧 anureo/Desk 组合联调，不能只用同版本 fake transport 作为发布证据。 |
| 性能与发布 | 部分完成 | Windows 20-run p95/CPU/RAM/OS 证据已补齐；Linux/macOS 采样、alias 调用量观测和 14 天删除门槛仍未达成。 |

### 0.1 仍不能由本文单独完成的交付事项

| 项目 | 当前状态 | 需要补的证据/材料 |
| --- | --- | --- |
| 多版本兼容 | 有 fake fixture 和兼容矩阵，尚无完整真实组合结果 | 新 Desk+旧 anureo、旧 Desk+新 anureo、新 Desk+新 anureo 的真实 ACP 运行记录，包含 capability、fallback 和业务错误不降级。 |
| 跨平台性能 | CI 矩阵和按 OS 的 JSON artifact 已提交，只有 Windows 本地结果 | Linux/macOS CI 成功链接、20-run 原始输出和阈值判定；失败时要能定位 runner/资源差异。 |
| Alias 下线 | 计数器和受保护 metrics endpoint 已实现 | 连续 14 天生产 `legacyListGlobalCalls=0` 的留档、最低支持 Desk 版本确认，以及删除后的回滚窗口。 |
| 数据迁移运维 | schema/migration 契约、测试和操作手册已写 | 真实数据库备份、升级、降级/回滚演练结果，以及 orphan metadata/foreign-key-check 的实际输出。 |
| 交付验收 | 代码级测试大部分通过 | 一份按 §12 执行的签收表，列出命令、环境、提交版本、失败项和责任人；否则无法让接手者独立判断“完成”。 |

因此，本文件当前的正确使用方式是：先按 §10 的依赖顺序实施，按 §11 的编号补测试，再以 §12 逐条签收；在所有未完成项关闭前，不得把状态改为“已实现”，也不得删除 `list-global` alias。若需要交接给另一位开发者，必须同时提供本文件、37 号规范、当前实现进度和最后一次验证命令/输出。

---

## 1. 背景与问题

设计基线曾有两条对外会话列表方法：标准 ACP `session/list` 与 anureo Desk 的 `_anureo.dev/session/list-global`。这部分先描述迁移前的排序和 projection 问题；2026-08-22 当前实现已增加 canonical `_anureo.dev/session/list`，并让标准 list、canonical list 和 legacy alias 共享 SessionIndex/query core，详见 §13。

`list-global` 以单个 `updated_at` RFC 3339 字符串作为降序排序键和 keyset cursor。当前查询使用严格小于：

```sql
WHERE COALESCE(updated_at, created_at) < ?cursor
ORDER BY COALESCE(updated_at, created_at) DESC
```

这不是完整的总序：多个会话拥有相同时间戳时，跨页记录可能被跳过；分页期间会话更新也会让结果重复或遗漏。前端的 ID 去重只能消除重复，不能恢复后端漏掉的会话。

此外，标题、归档、恢复和生命周期更新都复用 `updated_at`。这使管理操作被错误解释为对话活动，造成“恢复旧会话后直接排到最近”的体验。全局列表也未持久化或返回 `parent_session_id`，使 Desk 无法可靠构造 session/subagent 树。

### 1.1 基线 `acp_sessions` 表（2026-08-21 快照）

`acp_sessions` 是当前 durable session metadata 的事实源，也是 anureo Desk 全局 sidebar 的主要数据源。它与 checkpoint 消息历史分离；checkpoint 仍由 `thread_id` 关联，用于 history/load 和统计。

| 字段 | 基线语义 |
| --- | --- |
| `session_id` | 主键，anureo session ID。 |
| `thread_id` | checkpoint 的 thread ID。 |
| `owner_principal` | owner 隔离键。 |
| `cwd` | session 工作目录。 |
| `lifecycle` / `closed_at` | 生命周期和关闭时间。 |
| `created_at` | 创建时写入，保持不变。 |
| `updated_at` | 当前列表排序键；同时承载多种不同行为。 |
| `title` | 自动或手动标题。 |
| `archived_at` | 已归档时的时间；旧数据库通过兼容 migration 增加。 |

同一 SQLite 文件中的 `acp_session_data(session_id, metadata_json)` 保存 anureo Desk-owned JSON metadata。它没有数据库级 foreign key；`delete` / `delete_all` 通过 repository transaction 显式清理该表和其他 session-owned 行。

当前 mutation 语义并不一致：新建写入 `created_at = updated_at`；标题更新不修改 `updated_at`；archive/restore 与 lifecycle 变更都会修改 `updated_at`。因此 `updated_at` 既不是纯粹的 activity 时间，也不是纯粹的 metadata 时间，不能继续作为“最近会话”的长期定义。

### 1.2 基线 anureo 生产调用链（2026-08-21 快照）

以下是 2026-08-21 对 anureo 源码的直接审查结果，用于解释问题来源、区分“基线现状”与后文目标设计；它不是 2026-08-22 当前实现的替代描述，当前状态以 §13 为准：

| 路径 | 基线生产行为 |
| --- | --- |
| capability/dispatch | `extensions/session_list.rs` 注册 `list/list-global/archive/update/delete`；extension registry 按 `_anureo.dev/<domain>/<submethod>` 分发，缺 domain 返回 `-32001`，缺 submethod 返回 `-32601`。 |
| private list | canonical `list` 使用 owner-scoped immutable SessionIndex snapshot；迁移期 `list-global` 只把同一 canonical page 投影为旧字段，metadata 不再逐条查询。 |
| repository order | canonical `list_index_for_owner` 负责 membership、tree order 和 cwd 精确匹配；旧 `list_for_owner_paged` timestamp 分页旁路已删除。 |
| standard list | production owner-aware `session/list` 直接从 `acp_sessions` 读取 active records，再按 cwd 过滤并投影标准字段；代码中另有 checkpoint 聚合的 legacy helper，但当前 stdio dispatch 不走它。 |
| create/fork | `session/new` 规范化 cwd 后只插入核心 record，当前忽略 request `_meta`；`session/fork` 创建新 record并复制配置/MCP，但没有 source-message 历史边界，也不持久化 parent/初始 metadata。 |
| mutation | title 与 metadata 不更新 `updated_at`；archive/restore、lifecycle 会更新；durable lifecycle 实际取值为 `idle/closed`。delete 会事务清理 session-owned rows，但不发布 global delete tombstone。 |
| global event | bus wire 为 `{topic,event:{type,properties}}`。archive/title/metadata 发布 OpenCode-like `properties.info`；`runtime.rs` 又把每条非 history-replay 标准 `session/update` 高频广播为只有 `properties.id` 的同名 `session.updated`；create/delete 没有完整 membership event。 |

因此，目标实现不能只重命名 handler：还要消除 timestamp 分页、metadata N+1、写入语义分叉和同名 event shape 冲突，并把 create/delete/activity 纳入同一 index mutation/event contract。

### 1.3 基线缺口（2026-08-21；当前收敛状态见 §13）

| 优先级 | 缺口 | 当前影响 |
| --- | --- | --- |
| P0 | `updated_at` 单键分页 | 相同时间戳跨页时会话可能被跳过；没有 `session_id` tie-break。 |
| P1 | 无 snapshot 语义 | 第 1 页与第 N 页之间发生活动、归档或恢复时，遍历可能重复或漏项。 |
| P1 | 不存在 `parent_session_id` | 全局列表无法可靠构造主 session/subagent 树。 |
| P1 | 多套列表实现 | 标准 ACP 与 Desk 扩展的 projection 仍不同；canonical/legacy 必须共享 query core，避免字段、cwd filter 与排序漂移。 |
| P2 | `updated_at` 混合多种时间 | archive/restore/close 等管理操作会伪造“最近对话”活动。 |
| P2 | 不存在 record `revision` | 重连或乱序 `session.updated` 事件不能可靠拒绝旧状态。 |
| P3 | `closed_at` 不在列表投影中 | 客户端无法在不加载额外记录的情况下区分最近关闭与最近活动。 |
| P3 | `acp_session_data` 无 foreign key | 当前依赖 repository transaction 清理；新增写入路径若遗漏清理，可能留下孤儿 metadata。 |

## 2. 目标：要完成的事情

本次改造要把 anureo 的 session list 从“以 `updated_at` 临时拼出的全局列表”，升级为由 SessionIndex 驱动、支持稳定分页和实时合并的正式能力；同时让 anureo Desk 只依赖一个明确的私有列表契约，并在迁移期兼容旧版本。完成后，列表的集合、顺序、层级和实时状态都应由服务端强语义决定，客户端不再通过时间戳、标题或事件先后顺序猜测。

### 2.1 本次要完成的工作

| 工作面 | 要做的事情 | 交付结果 |
| --- | --- | --- |
| anureo 数据模型 | 扩展 `acp_sessions`，增加 parent、activity、tree activity、状态/metadata 时间和 revision；提供幂等 migration 与查询索引。 | 一个可持久化、可排序、可表达父子关系的 SessionIndex。 |
| anureo 写入语义 | 将新建、实际对话活动、archive/restore、lifecycle、标题和 metadata 写入收敛到统一 repository mutation API。 | 每类操作只更新其负责的字段；管理操作不再伪造最近活动。 |
| 私有列表协议 | 用 `_anureo.dev/session/list` 替代 `list-global`，实现 owner-scoped snapshot、opaque cursor、完整总序、filter 校验和明确错误。 | 多页遍历不重复、不漏项；分页期间发生写入也能得到一致结果。 |
| 标准 ACP 对齐 | 让标准 ACP `session/list` 与私有列表都从 SessionIndex 派生，但保留各自的 projection 和职责边界。 | 消除 production 中多套事实源，同时不破坏 ACP v1 客户端。 |
| anureo Desk 适配 | 增加 `listIndex` adapter、capability 选择、snapshot 失效恢复和 revision merge；统一全量/局部/upsert 排序，并补齐 parent、rollback、retention 与 create 参数。 | Sidebar、Recent、归档和目录视图使用同一记录语义，旧事件和失败请求不会破坏缓存。 |
| 兼容与发布 | 新 anureo 在一个窗口内同时支持 `list` 与 `list-global`；新 Desk 优先 `list`，仅在方法缺失时回退旧 alias，最后按调用量从 anureo 删除 alias；Desk 对最低支持旧 anureo 的 fallback 独立保留。 | 新旧 anureo/Desk 组合可渐进升级，真实业务错误不会被 fallback 掩盖。 |
| 自动化验证 | 建立可控时钟、snapshot harness、ACP fake transport 和查询/write instrumentation，完成 repository、协议、Desk、兼容、E2E 与性能用例。 | 排序、snapshot、revision、层级和性能约束成为持续回归门槛。 |

本设计不只是修改一个 RPC 名称。最终必须同时完成 schema、所有写入路径、列表读取、实时事件、Desk 合并逻辑、兼容迁移和自动化测试；只完成其中一层会继续保留字段漂移、分页漏项或缓存乱序问题。

### 2.2 预期结果

1. 对同一 owner 提供唯一、稳定、可分页的会话列表事实源。
2. 区分对话活动、生命周期变化和展示元数据变化，明确“最近”含义。
3. 让跨页加载获得一致快照：不重复、不漏项，并对快照失效提供可恢复错误。
4. 持久化并返回父子关系，使所有客户端能构造同一 session 树。
5. 保持 ACP v1 `session/list` 兼容；anureo Desk 扩展可渐进迁移。

### 2.3 非目标

- 不改变 session/load、消息历史或 checkpoint 的持久化格式。
- 不把 Desk 的 Pin、文件夹或项目手动顺序移入 anureo；它们仍是客户端偏好。
- 不导入非 anureo 的历史会话记录。
- 不在本设计中改变 session 的 owner、连接绑定或 prompt 并发模型。

## 3. 设计决策

| 维度 | 决定 | 原因 |
| --- | --- | --- |
| 列表事实源 | `acp_sessions` 的 session index | 新建 session 在首个 checkpoint 前也必须可见。 |
| “最近”活动 | `activity_at` | 每个非 replay prompt 被接受时至多更新一次；流式消息/工具进度不单独写 index。 |
| 树排序 | 根节点使用 `tree_activity_at` | 子 session 活动应使其所属任务出现在最近位置，而不是脱离父节点。 |
| 管理操作 | 独立 `state_changed_at` / `metadata_updated_at` | 归档、恢复、改标题不得伪造对话活动。 |
| 排序稳定性 | 所有排序增加 `session_id` 次级键 | 排序必须形成总序。 |
| 分页一致性 | owner-scoped 短期 snapshot | 单纯 timestamp cursor 无法抵抗分页期间的写入。 |
| active/archived 一致性 | 一次 owner-wide snapshot 同时覆盖两个分区 | 两个独立 snapshot 之间的 archive/restore 会让记录在两边都缺席，revision 无法恢复未返回的记录。 |
| 实时事件 | `revision` 单调递增 | 客户端可拒绝重连后的旧事件。 |
| 层级 | `parent_session_id` 持久化并返回 | 不通过 title、创建时间或事件顺序猜测父子关系。 |
| Snapshot 存储 | materialized immutable projection | 只冻结 ID/排序键无法保证翻页期间 title、metadata、parent 等字段不漂移。 |

### 3.1 实现契约冻结

以下参数与失败语义属于协议的一部分，开发时不得再自行选择另一套行为：

| 维度 | 冻结决定 |
| --- | --- |
| owner-wide version | 新增持久化 `acp_session_index_state(owner_principal, current_version)`。尚无 state row 的 owner 读取为 0；第一次真实 mutation 在同一 transaction 中插入/递增到 1。每个产生可见 projection 变化的 repository transaction 将对应 owner 的 `current_version` 恰好加一，同一 transaction 改变的 target/祖先 records 共用该 `indexVersion`。`revision`/record `indexVersion` 的 DB/wire 范围是 `1..=9_007_199_254_740_991`，`snapshotVersion`/owner `current_version` 是 `0..=9_007_199_254_740_991`；checked increment 溢出时返回 `-32603 version_exhausted` 并整体回滚。 |
| record version | `revision` 是单 record 版本；`indexVersion` 是 owner-wide transaction 版本。客户端以 `revision` 解决同 ID 乱序，以 `snapshotVersion/indexVersion` 判断 snapshot 与 tombstone 的覆盖关系，二者不得互相替代。 |
| snapshot version | 首次 materialize 在一个 SQLite read transaction 中先读 owner `current_version`，再读/排序完整 projection；两者属于同一 DB snapshot。所有页面返回同一个 `snapshotVersion`。只有完成的 owner-wide `archived="all"` snapshot 且 `snapshotVersion >= tombstone.indexVersion` 时，客户端才能清理该 tombstone。 |
| snapshot 存储 | anureo 进程内保存完整 immutable projection；TTL 固定为自创建起 5 分钟，不因翻页续期。accounted bytes = 每条 record 的 compact canonical JSON UTF-8 bytes + 64 bytes overhead，再加每 snapshot 256 bytes。每 owner 最多 4 个 snapshot/64 MiB，全进程最多 256 MiB；先清过期项，再按 `last_access_at` 最旧优先淘汰（相同时按 snapshot ID ASC）。单个新 snapshot 超过 64 MiB 或淘汰后仍超过全局上限时返回 `-32005 snapshot_capacity_exceeded`。 |
| cursor | cursor 使用进程启动时生成的 256-bit secret 做 HMAC-SHA256，snapshot ID 使用 128-bit CSPRNG；wire 最大 1024 bytes。签名/格式/version/offset 非法为 `invalid_cursor`；签名有效但 snapshot 不存在（含 server restart）为 `snapshot_expired`。后续页只允许 `cursor`，page size/filter 均由 snapshot 冻结。 |
| global event 可靠性 | `GlobalEventBus` 保持 fire-and-forget；DB commit 不因队列满/连接关闭回滚。actor 依赖 mutation response 立即收敛，其他 client 依赖 event 降低延迟并依赖 authoritative snapshot 保证最终正确。publish drop 必须计数并记录不含 metadata/prompt 的结构化日志。 |
| resync 上界 | Desk 在 bootstrap、ACP reconnect、runtime switch 后立即 owner-wide load；visible 且 online 时由 global store 每 60 秒执行一次 singleflight owner-wide resync。hidden/offline 时停止 timer；重新 visible/online 且距上次成功已满 60 秒时立即 resync。Sidebar/Tray 等 consumer 不再各自建立重复 full-load timer。 |
| activity 边界 | prompt 完成参数/owner/session 校验并成功取得该 session 的 busy lease 后、executor 启动前，先提交一次 Activity mutation；提交失败则 prompt 在 executor 启动前失败。校验失败、busy 拒绝、history replay 不更新；取得 lease 后即使 executor 启动失败、用户取消或模型失败也保留本次 activity；每次新的成功受理 prompt 各记一次。 |
| 时间规范 | 所有 index 时间写为 UTC RFC 3339、固定 6 位小数。每个字段的新值取 `max(truncate_to_microseconds(clock.now), previous + 1µs)`；首次创建没有旧值。这样系统时钟回拨也不会让一次新 activity、state 或 metadata mutation 排到旧值之前。migration 保留旧值代表的时刻并规范化为同一 wire 格式。 |
| no-op | title/metadata 内容相同、archive 状态相同、lifecycle 相同均返回当前 canonical record，但不更新时间、不增加 record revision/owner indexVersion、不发布 event。create 总是创建 revision/indexVersion；重复 delete 返回已持久化 tombstone，不再次递增。 |
| mutation/event 顺序 | transaction commit 后从 immutable result 发布 event，再返回 response；publish 失败不改变 response。create 先发布 target created，再 nearest-ancestor-first 发布 ancestors；其他非 delete mutation 先发布 target updated，再发布 ancestors；delete 先发布 tombstone，再发布 ancestors。所有同 transaction 结果共享 `indexVersion`，客户端仍按每个 record 的 `revision` 幂等合并。 |
| metadata 完整性 | 重建 `acp_session_data`，固定使用 `FOREIGN KEY(session_id) REFERENCES acp_sessions(session_id) ON DELETE CASCADE`；每个 repository connection 执行 `PRAGMA foreign_keys=ON`。migration 只复制有 parent row 的 metadata，记录并删除既有 orphan，最后执行 `foreign_key_check`。不保留“仅靠调用方事务”的备选方案。 |
| 标准 ACP list | 标准 `session/list` 改用相同 SessionIndex repository/projection，但兼容期继续一次返回当前 owner/cwd 的全部 active records并令 `nextCursor=null`；不在本次改造中改变旧 ACP client 的分页行为。 |

上述固定值如需调整，必须修改 37 号规范、相应测试和 release note；不能只改实现常量。

## 4. 数据模型

`acp_sessions` 增加以下列：

```sql
ALTER TABLE acp_sessions ADD COLUMN parent_session_id TEXT;
ALTER TABLE acp_sessions ADD COLUMN activity_at TEXT;
ALTER TABLE acp_sessions ADD COLUMN tree_activity_at TEXT;
ALTER TABLE acp_sessions ADD COLUMN state_changed_at TEXT;
ALTER TABLE acp_sessions ADD COLUMN metadata_updated_at TEXT;
ALTER TABLE acp_sessions ADD COLUMN revision INTEGER NOT NULL DEFAULT 1
  CHECK(revision BETWEEN 1 AND 9007199254740991);
ALTER TABLE acp_sessions ADD COLUMN index_version INTEGER NOT NULL DEFAULT 1
  CHECK(index_version BETWEEN 1 AND 9007199254740991);

CREATE TABLE acp_session_index_state (
  owner_principal TEXT PRIMARY KEY,
  current_version INTEGER NOT NULL
    CHECK(current_version BETWEEN 0 AND 9007199254740991)
);

CREATE TABLE acp_session_tombstones (
  session_id TEXT PRIMARY KEY,
  owner_principal TEXT NOT NULL,
  cwd TEXT NOT NULL,
  parent_session_id TEXT,
  revision INTEGER NOT NULL CHECK(revision BETWEEN 1 AND 9007199254740991),
  index_version INTEGER NOT NULL CHECK(index_version BETWEEN 1 AND 9007199254740991),
  deleted_at TEXT NOT NULL
);
```

字段语义：

- `created_at`：会话创建时间，永不改变。
- `activity_at`：当前 session 最后一次实际 user/agent 工作边界。本设计将边界冻结为“非 history replay 的 `session/prompt` 被 anureo 接受并开始执行”且每个 prompt 只记录一次；token、message part、tool progress 等流式 `session/update` 不写该字段。
- `tree_activity_at`：当前 session 及其任意后代的最大 `activity_at`。
- `state_changed_at`：archive/restore/close/delete 前的生命周期变更时间。
- `metadata_updated_at`：标题或 anureo Desk-owned metadata 的变更时间。
- `revision`：每次可见列表投影变更时 checked increment，wire 为 JSON safe integer。
- `index_version`：该 record 最近一次可见变化所属的 owner-wide transaction version，wire 为 JSON safe integer。

所有 index 时间以 UTC RFC 3339、固定 6 位小数持久化与传输。新值必须使用 `max(truncate_to_microseconds(clock.now), previous + 1µs)`；不得因系统时钟回拨让一次新 mutation 的对应时间倒退。

Migration 在单个 `BEGIN IMMEDIATE` transaction 中执行。旧 record 固定回填：`parent_session_id=NULL`，`activity_at=COALESCE(updated_at,created_at)`，`tree_activity_at=activity_at`，`state_changed_at=COALESCE(archived_at,closed_at)`，`metadata_updated_at=NULL`，`revision=1`，`index_version=1`；每个既有 owner 的 `current_version` 初始化为 `1`。migration 任一步或 `foreign_key_check` 失败必须整体回滚，禁止以部分 schema 启动。

所有 session index 写入必须收敛到一个 repository API。禁止 extension handler、lifecycle handler 或 checkpoint adapter 分别手写 `UPDATE acp_sessions`，避免字段语义再次漂移。

```rust
pub enum SessionIndexMutation {
    Create { record: NewSessionIndexRecord },
    Activity { at: DateTime<Utc> },
    Archive { archived_at: Option<DateTime<Utc>>, at: DateTime<Utc> },
    Lifecycle { lifecycle: SessionLifecycle, closed_at: Option<DateTime<Utc>>, at: DateTime<Utc> },
    Metadata { title: Option<String>, metadata: Option<Value>, at: DateTime<Utc> },
    Reparent { parent_session_id: Option<String>, at: DateTime<Utc> },
    Delete { at: DateTime<Utc> },
}

pub fn apply_index_mutation(
    &self,
    session_id: &str,
    mutation: SessionIndexMutation,
) -> Result<SessionIndexMutationResult>;
```

`SessionIndexMutationResult` 固定包含 `index_version`、target record 或 delete tombstone，以及因 tree 重算真正改变的 ancestor records；handler 不得在 transaction 结束后重新查询并拼装另一个时点的 response/event。

`Activity` 同一事务更新当前 session 的 `activity_at`、`tree_activity_at`、`revision`，并重算祖先链。`Archive`、`Lifecycle`、`Metadata`、`Reparent` 和 `Delete` 不修改 `activity_at`；但 archive、restore、delete 和 reparent 会改变“当前可见树”，因此必须重算旧、新祖先链的 `tree_activity_at`，值既可以上升也可以下降。任何祖先的可见投影发生变化时，该祖先也必须递增 `revision` 并发布 `session.updated`。

`lifecycle` 是 durable execution lifecycle，目标枚举沿用当前 repository 的 `"idle" | "closed"`；它不表示 active/archived membership，也不承载 live `busy` 状态。实时 busy/idle 继续由标准 ACP status/update 与 directory child store 管理。公开 extension 不提供 reparent RPC，`parent_session_id` 在创建后不可由 Desk 修改；`Reparent` 只保留为 migration/repair 所需的内部 repository mutation，并执行与普通写入相同的 owner、cwd、cycle 与 revision 校验。

`tree_activity_at` 定义为当前 session 自身与沿 **全部节点均未删除、未归档的 parent edge** 可达后代的最大 `activity_at`。closed 但未 archived 的记录仍属于当前可见树；archived parent 会切断其上方祖先与仍 active child 的可见链，该 child 作为 effective root 建立自己的 tree activity。该字段不是历史 high-water mark：最后一个活跃 child 被 archive/delete 或移到另一棵树后，旧祖先必须下降到剩余可见树的最大值；新祖先则按新树重算。固定使用同一 transaction 内的 recursive CTE 读取 ancestor/descendant closure，并在写入前检测跨 owner、self-parent、循环与断裂 parent；任一非法关系整体失败，禁止部分提交。

## 5. 列表与排序语义

### 5.1 活跃 session

活跃“有效根节点”的基础顺序：

```text
tree_activity_at DESC, session_id ASC
```

有效根节点定义为：自身未归档，且 `parent_session_id IS NULL`，或 parent 不存在于同一 active scope（parent 已归档/删除，或 legacy 数据损坏）。这保证 Desk 当前逐项 archive/delete 出现 partial failure 时，仍 active 的 child 会暂时提升为 root，而不是从 active traversal 消失；parent 恢复后 child 按持久化 parent 自动重新挂回。服务端不得静默改写 parent ID。

同一父节点内的直接子节点顺序：

```text
activity_at DESC, session_id ASC
```

服务端返回平铺的完整记录和 `parentSessionId`；客户端负责渲染树。服务端不得因当前页边界截断父子关系：一个 snapshot 包含所有可见记录，客户端在加载全部页面前不得把未出现的 parent 视为不存在。

### 5.2 归档 session

归档视图按：

```text
archived_at DESC, session_id ASC
```

恢复只清除 `archived_at` 并更新 `state_changed_at`；恢复后的活跃顺序由历史 `tree_activity_at` 决定。

### 5.3 客户端展示层

anureo 只定义上述基础顺序。Desk 可以在每个树层级再应用 Pin 优先、文件夹、项目手动顺序等用户偏好，但不得将其写回 anureo，也不得以客户端临时状态替代服务端的 `activity_at` 或 hierarchy。

## 6. `_anureo.dev/session/list` 协议

不另建版本化的新方法。以现有 `list-global` 的 repository 能力为起点，将最终公开方法重命名为 `_anureo.dev/session/list`。标准 ACP `session/list` 与私有 `_anureo.dev/session/list` 在线路上不冲突：前者由 ACP handler 分发，后者经 `_anureo.dev/` extension registry 分发到 `domain=session`、`method=list`。

首次请求：

```json
{
  "archived": "all",
  "directory": "C:/repo",
  "limit": 200
}
```

响应：

```json
{
  "sessions": [
    {
      "sessionId": "ses_123",
      "parentSessionId": null,
      "cwd": "C:/repo",
      "title": "Implement session list",
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

后续页只需回传 `cursor`。snapshot 信息全部封装在 opaque cursor 内部，不增加单独的公开 `snapshotId` 参数：

```json
{
  "version": 2,
  "snapshotId": "server-owned-snapshot",
  "offset": 200,
  "filterHash": "owner+archived+directory+sort"
}
```

上例只是服务端解码后的逻辑结构；wire 上仍为 opaque string。cursor 必须绑定 owner、archived、directory 和排序模式，不能跨查询复用。`archived` 是枚举：`"all"` 同时返回 active/archived，`"active"` 与 `"archived"` 只用于明确的局部读取；Desk authoritative full load 必须使用 `"all"`，不得再并行拼接两个 snapshot。

所有时间均为 UTC RFC 3339 字符串。`parentSessionId`、`title`、`archivedAt`、`closedAt`、`stateChangedAt`、`metadataUpdatedAt` 可为 `null`；`createdAt`、`activityAt`、`treeActivityAt`、`cwd`、`lifecycle`、`revision`、`indexVersion` 必须存在。`snapshotVersion` 是首次 materialize 时的 owner version，所有页面必须相同。`lifecycle` 只能是 `"idle"` 或 `"closed"`，与 `archivedAt` 正交；live busy 状态不写入 index。`metadata` 必须是 JSON object，空值返回 `{}`。`limit` 默认 200，范围 1～1000；未知字段、非法枚举、非法时间或非 object metadata 返回 `-32602 invalid_params`，客户端不得用 `Date.now()` 修复非法必填时间。

`archived="all"` 的 materialized sequence 先放 active tree，再放 archived records。active tree 按有效 root 基础顺序做稳定 pre-order traversal，每个 parent 的 active children 使用 child 基础顺序递归展开；随后 archived records 按 archived 基础顺序排列。构造前必须验证每个 active ID 恰好可达一次；orphan/cycle 按 effective-root/cycle-guard 规则安全降级。`hasMore` 为 `true` 时 `nextCursor` 必须非 null；`hasMore=false` 时 `nextCursor=null`。

兼容 alias `_anureo.dev/session/list-global` 不是第二套查询实现，但也不能直接返回新 record：旧 Desk 的 mapper 依赖 boolean `archived`、`updatedAt` 等 legacy 字段。alias 使用与 canonical list 完全相同的 immutable snapshot/cursor core，接受旧 request shape，再把当前 page 投影为旧 descriptor；新字段、`archived="all"` 与 authoritative full-load 语义只属于新方法。旧客户端若携带升级前生成的 cursor，按 opaque cursor 的正常失效语义重新从第一页加载；发布期通过 alias 调用量归零后删除。

### 6.1 Snapshot 实现

首次查询由服务端创建 owner、过滤条件和排序模式绑定的短期 snapshot。Desk 的全量加载使用 `archived="all"`，一次 snapshot 同时冻结 active 与 archived membership，避免两个独立读取之间的 archive/restore 漏项。snapshot 保存创建时已经排序的 **完整不可变列表投影**、`snapshotVersion` 和 byte accounting，包括 metadata；只保存 `session_id` 或排序键后再回表读取不满足字段冻结要求。本次固定使用进程内 materialized snapshot store，资源上限、HMAC cursor、TTL 与淘汰行为遵循 §3.1，不再保留临时表/MVCC 的实现分支。

snapshot 不可用、过期或 owner/过滤条件不匹配时返回：

```json
{
  "code": -32004,
  "message": "snapshot_expired"
}
```

Desk 收到该错误必须丢弃本次未完成的分页结果并从第一页重新加载，不能把它当作空列表。

### 6.2 Authoritative absence 与原子提交

成功完成 snapshot 后，服务端结果对其绑定 scope 具有 authoritative membership 语义：`archived="all"` 且无 directory 时覆盖当前 owner 的全部 session；带 directory 时只覆盖规范化 cwd 精确匹配的记录。客户端必须在一次原子 commit 中执行：

1. 以 snapshot projection 建立该 scope 的新 normalized records。
2. 移除旧 cache 中属于该 scope、但未出现在成功 snapshot 的记录。
3. 保留 scope 外记录；请求失败、`snapshot_expired` 或未完成分页时不删除任何旧记录。
4. 再按 revision 合并加载期间的 event/tombstone overlay；overlay 高 revision 胜出。
5. 最后叠加显式登记的 optimistic create shadow；普通 directory live record 不构成 membership 例外。

directory refresh 只能替换成功返回的 directory scope；多个 scope 部分失败时，失败 scope 保留旧 cache 并暴露错误。一次成功的 owner-wide `all` snapshot 是 tombstone 清理的必要条件；仅当 `snapshotVersion >= tombstone.indexVersion` 且不存在更旧的在途 snapshot commit 时清理。directory snapshot 永不清理 tombstone。

### 6.3 实时事件与删除 tombstone

新协议沿用 global event topic `session`，但冻结 payload，不再发送当前 OpenCode-like `id/time` 形状：

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

`session.created` 与 `session.updated` 的 `properties.info` 必须是与 list item 完全同构的 `SessionIndexRecord`；wire 唯一 ID 字段为 `sessionId`，不混用 `sessionID` 或 `info.id`。anureo Desk 的 list adapter 与 event router 必须复用同一个严格 descriptor parser。兼容窗口内，旧 `list-global` 的 OpenCode-like event 只能经过独立 legacy adapter，不能污染新 record 类型。客户端只接受 `revision` 大于本地记录的事件；相等或更小的事件视为重复/过期。

`session.deleted` 的 wire envelope 为 `{ "topic":"session", "event": { "type":"session.deleted", "properties": { "tombstone": { "sessionId":"ses_123", "cwd":"C:/repo", "parentSessionId":null, "revision":43, "indexVersion":109, "deletedAt":"...", "deleted":true } } } }`。该嵌套结构与当前 `GlobalEventBus.publish()` 及 Desk `acp-event-source.ts` 的真实 transport contract 一致。删除 mutation 在同一 transaction 中分配最终 record revision、owner `indexVersion` 并持久化 tombstone，再删除 session-owned rows。重复 delete 返回既有 tombstone，不产生新 version/event。客户端保留每个已删除 session 的最高 tombstone revision；revision 小于或等于 tombstone 的 snapshot item、created 或 updated event 均不得重新创建记录。

当前 `runtime.rs` 会把几乎每条非 history-replay 标准 `session/update` 再发布成只有 `{ "id": ... }` 的 global `session.updated`，而 archive/update handler 又发布 OpenCode-like `properties.info`。目标协议禁止继续复用同一 event type 表示这些不兼容形状：移除 runtime ingress 上的逐 update global 广播；聊天增量只留在标准 `session/update`。只有已提交的 SessionIndex mutation 才发布 global `session.created/updated/deleted`，且 updated 总是完整 record。每个 prompt 的 Activity mutation/event 至多一次，不能按 token、part 或 tool progress 放大 revision、SQLite 写入或列表重排。

实时事件不是 snapshot 的一部分，也不是可靠日志。客户端在分页期间把事件与 tombstone 暂存为 overlay；snapshot 完成后，以 `sessionId` 为键按 revision 合并 snapshot、加载前 cache 和 overlay，再原子提交。若事件引用本次 snapshot 未包含的新 session，则插入 normalized map 并按基础排序重算其受影响的树分支。`snapshot_expired` 时丢弃 page accumulator，但保留加载期间收到的 event/tombstone overlay，重新从第一页获取后再合并。队列满、连接关闭导致 event drop 不回滚 mutation；actor 用 response 收敛，其他 client 最迟由 §3.1 的 60 秒 authoritative resync 修复。

### 6.4 Mutation response contract

`_anureo.dev/session/archive` 保持旧 Desk 可读取的 `session` 字段，并增加受 tree 重算影响的 records：

```json
{
  "session": { "sessionId": "ses_123", "revision": 43, "indexVersion": 109 },
  "affectedSessions": [
    { "sessionId": "ses_parent", "revision": 18, "indexVersion": 109 }
  ],
  "indexVersion": 109
}
```

示例省略了 record 的其他字段；线上 `session` 和 `affectedSessions` 每一项都必须是完整 `SessionIndexRecord`。数组不重复 target，并按 nearest-ancestor-first 排列。archive/restore no-op 返回当前完整 `session`、空数组及当前 owner version，不发 event。

`_anureo.dev/session/update` 固定返回 `{ "session": <full record>, "metadata": <object>, "indexVersion": n }`。title 与 metadata 在一个 repository transaction 中原子应用；任一字段非法时两者均不写。内容完全相同时按 no-op 处理。

标准 `session/delete` response 使用 ACP `_meta` 返回 canonical tombstone：

```json
{
  "_meta": {
    "anureo.dev": {
      "tombstone": {
        "sessionId": "ses_123",
        "cwd": "C:/repo",
        "parentSessionId": null,
        "revision": 43,
        "indexVersion": 109,
        "deletedAt": "2026-08-21T10:20:00Z",
        "deleted": true
      }
    }
  }
}
```

新 Desk 只有拿到成功 response 才提交 optimistic delete，并用 tombstone 替换 shadow；旧 client 忽略 `_meta`。若 response 丢失后重试，anureo 从 `acp_session_tombstones` 返回同一个 tombstone。服务端 tombstone 是不含 metadata/title 的紧凑永久记录，不执行 TTL 清理，session ID 永不复用；客户端 tombstone 仍按 `snapshotVersion` 规则及时清理。

## 7. 标准 ACP `session/list` 对齐

标准 `session/list` 继续返回 ACP schema 允许的字段，但其记录来源切换为 session index，不再从 checkpoint 聚合排序。本次不返回 `checkpoint_count`、最后摘要等 `_meta` 统计；如以后需要，另立不会改变 membership/ordering 的协议扩展。

`cwd` filter 必须在 repository 层生效，并与私有 `_anureo.dev/session/list` 使用相同的 Windows verbatim-prefix 与分隔符规范化逻辑。为避免新 anureo 截断仍只调用一次的旧 ACP clients，本次固定保持单页语义：返回当前 owner/cwd 的全部 active records并令 `nextCursor=null`；它的排序和可见记录与 index 一致。标准分页属于后续独立兼容设计。

两者职责保持明确：标准方法提供 active session 的 ACP-compatible projection；私有方法提供 sidebar 所需的 active/archived、metadata、parent、snapshot 和 revision。Desk 的正确性不依赖标准 `SessionInfo._meta` 被任意中间 client 完整保留。

## 8. anureo Desk session 消费方

anureo Desk 不是只渲染一份 session 数组，而是有两层刻意分离的数据范围：

| 层 | 主要模块 | 用途 |
| --- | --- | --- |
| 全局 session cache | `packages/ui/src/stores/useGlobalSessionsStore.ts` | 全部 active/archived session、sidebar、归档和 retention。 |
| directory child store | `packages/ui/src/sync/sync-context.tsx` | 当前已初始化目录的实时 session/message/part/permission/question/status。 |
| ACP-native store | `packages/ui/src/lib/acp/acp-session-store.ts` | ACP notification 的原生投影，并向 legacy UI store 过渡。 |

全局 cache 当前默认通过 `acpApi.session.listIndex` 初始化，优先请求 `_anureo.dev/session/list`；只有旧 capability 或 legacy caller 才使用 `_anureo.dev/session/list-global`。目录 child store 用于实时聊天真相；它不是完整的全局列表，不能取代全局 cache。Desk sidebar 在得到记录后还会应用 Pin、文件夹、项目/worktree 分组、折叠状态、搜索和本地排序等展示偏好。

> **源码审查结论（2026-08-21）**：`acp-runtime.ts` 已提供 `subscribeGlobalUpdate`，`acp-event-source.ts` 已提供 `openAcpGlobalEventStream`，`session-event-router.ts` 也能处理 session event；但当前 production 路径没有调用 `openAcpGlobalEventStream`，也没有把 global session topic 接到 `applySessionEventToGlobalSessions`。因此 global event 目前只是具备组件和单测，不是已接通的缓存补充通道。本设计必须把接线、重连和 runtime switch 清理列为显式开发任务。

### 8.1 已有功能

- 创建 session、草稿生命周期、目录/worktree 目标选择、当前 session 恢复与切换。
- ACP `session/prompt` optimistic send、busy/idle 状态、停止、permission 与 question 处理。
- `session/load` history replay 与 `_anureo.dev/session-history/*` 的更早历史分页。
- session 标题更新、归档、删除、批量操作、Recent、Pin、文件夹、拖拽、搜索和 session switcher。
- session metadata 上的 Goal、review/plan 关联、导出、fork 与 worktree session 流程。
- retention：保护当前、已分享和最近五个 session 后，按配置批量 archive 或 delete。

### 8.2 查询、分页与缓存合并逻辑（2026-08-21 基线 + 当前入口说明）

本节的旧流程图和缺口描述保留 2026-08-21 基线；当前默认入口已切换为 `listIndex`，具体兼容选择以 `acp-api.ts` 的 capability/fallback 实现和 §13 为准。

查询入口在 `packages/ui/src/stores/globalSessions.ts::listGlobalSessionPagesAcp`，global cache 的协调入口在 `packages/ui/src/stores/useGlobalSessionsStore.ts`。当前流程如下：

```text
SyncProvider / Sidebar / Retention
  -> useGlobalSessionsStore.loadSessions()
      -> 并行 listGlobalSessionPages(archived=false)
      -> 并行 listGlobalSessionPages(archived=true)
          -> 循环调用 _anureo.dev/session/list-global（当前）
             / _anureo.dev/session/list（迁移后）
          -> map ACP descriptor 到 Desk Session
          -> 按 session id 去重、追加
          -> 使用 nextCursor 继续，直到 hasMore=false
      -> 写 activeSessions / archivedSessions / sessionsByDirectory
```

#### 初始全局加载

1. `loadSessions()` 用模块级 `inflightLoad` 合并并发调用，避免重复全量请求；同时递增 generation 以丢弃 runtime switch 前发起的迟到响应。
2. active 和 archived 使用 `Promise.allSettled` 并行拉取，均调用 `listGlobalSessionPages(undefined, { archived, pageSize: 500 })`。
3. 单个分页请求通过 `retry(..., { attempts: 3, delay: 500 })` 进行最多三次重试；每页将 ACP item 映射为 Desk `Session`，将 RFC 3339 `createdAt` / `updatedAt` / `archivedAt` 转为毫秒时间，并用目录补充本地 project/worktree metadata。
4. 分页循环维护 `seenIds`：只有此前未见的 ID 才加入结果；若后端声明仍有下一页但本页全部 ID 已见，客户端立即停止，防止 cursor 异常导致无限循环。
5. `hasMore=false`、`nextCursor` 缺失或空 payload 都结束遍历。当前 cursor 完全由 anureo v1 后端提供的时间戳 opaque token 决定。
6. 两路均成功时 cache status 为 `ready`；任一路失败时 status 为 `error`，但成功结果照常写入。active 失败时保留既有 active cache 并合并 directory sync fallback；archived 失败时保留既有 archived cache。失败绝不应清空既有 session。

#### 目录级刷新

sidebar 发现项目、worktree 或 scheduled-task 相关目录变化时，调用 `refreshSessionsForDirectories(directories)`，而不是重新替换所有 global cache：

1. 将目录路径规范化并去重。
2. 对每个目录分别请求 active 和 archived 列表，并在每一类中使用 `Promise.allSettled`。
3. 只有成功返回的目录才会替换 cache 中该目录的 slice；失败目录保留旧 slice，防止一次局部网络失败删除已有 session。
4. 替换后合并 directory sync fallback，重建 `sessionsByDirectory` 与 review-transfer map。

`replaceSessionsForDirectories` 在局部刷新后按 `updated/created DESC, id DESC` 排序。然而全量加载直接保留服务端分页顺序，`upsertSession` 又将新记录放到数组开头；因此 global cache 的数组顺序不是单一契约。当前 sidebar 会再按 Pin 和时间重新排序，不能把 cache 数组本身当作最终呈现排序。

#### action 与事件合并

- create、标题更新、metadata patch 成功后调用 `upsertSession`；archive/delete 则先乐观移动或删除 global cache，再执行 RPC。
- `applySessionEventToGlobalSessions` 已实现 `session.created` / `session.updated` 的时间字段旧事件过滤和 upsert，也能在收到 `session.deleted` 时从两个列表移除 ID；但它当前仅被测试直接调用，production global event subscription 尚未接线。
- anureo 当前 `extensions/session_list.rs` 只在 archive/restore 和 title/metadata update 路径发布 `session.updated`；尚未形成覆盖 create/update/archive/restore/delete 的完整列表事件集合，删除也没有 tombstone revision。
- directory child store 的消息、part、permission 与 status 更新走 ACP notification bridge；它只覆盖本次应用已初始化的目录。global cache 负责未打开目录和 archived session 的完整性，因此两者不能互相替代。

#### v1 的已知边界

当前 ID 去重和“重复页停止”只能避免无限循环与重复展示，不能修复 anureo 单 timestamp cursor 造成的漏项；也无法保证第 1 页到最后一页是同一时刻的集合。客户端的时间戳旧事件过滤也不能取代服务端 record revision。

### 8.3 新扩展协议必须满足的客户端契约

1. `_anureo.dev/session/list` 必须返回 `parentSessionId`、目录、`activityAt`、`treeActivityAt`、`archivedAt` 和 `revision`，否则 Desk 不能构造稳定树或拒绝过期事件。
2. snapshot 过期必须是显式错误；global cache 不得把 fetch failure 或过期 snapshot 解释成空成功并清除现有数据。
3. Desk 在 snapshot 完成后按 revision 合并实时事件；Pin、文件夹和项目手动顺序不写回 anureo。
4. active/archived 的服务端基础顺序只用于数据分页和默认展示；Desk 可在同一树层级叠加用户偏好，但不得重新定义 `activity_at` 的语义。
5. Desk 内部 wrapper 使用 `listIndex`（或继续保留 `listGlobal` 作为兼容名称），不能占用已经代表标准 ACP 方法的 `acpApi.session.list`。
6. Desk 必须根据 extension capability 的 `methods` 选择 `list` 或兼容 `list-global`；明确只有旧方法时直调 alias，capability 不可判定时只有 `-32601 method_not_found` 可以触发 fallback。`-32001 capability_not_supported`、业务和数据库错误不得降级重试旧方法。

### 8.4 基线客户端缺口（当前收敛状态见 §13）

| 优先级 | 缺口 | 影响与新协议后续处理 |
| --- | --- | --- |
| P1（基线） | `createSession(title, directoryOverride, parentID, metadata)` 早期只使用目录 | 当前已通过 `session/new` request 与 canonical response/affectedSessions 回填 `parentID`、metadata；历史问题保留用于说明改造动机。 |
| P1 | `forkFromMessage(sessionId, messageId)` 目前不传 `messageId` | anureo 只新建 session 并复制配置/MCP，不按消息截断历史；本次已明确 message-bounded fork 为非目标，应移除被忽略参数并更正 UI/API 语义。 |
| P1（基线） | delete/archive 早期先乐观更新 global cache，失败后不回滚 | 当前已使用 shadow/rollback、tombstone 与 affectedSessions；历史问题保留用于说明改造动机。 |
| P2（已收敛） | retention 早期直接调用 `anureoAgentClient` | 当前逐项调用 ACP `archiveSession`/`deleteSession` facade，并处理 `false` partial failure；历史问题保留用于说明改造动机。 |
| P2 | global cache 与 directory child store 需要手动协调 | 这是有意分层，但新协议的 revision 与 snapshot 合并规则必须成为跨 store 的明确一致性边界。 |

### 8.5 跨 store 的事实源与字段所有权

迁移后的 Desk 必须明确区分“列表事实”和“聊天实时状态”，不能继续用对象展开或时间戳猜测决定字段胜负：

| 数据 | 权威来源 | 合并规则 |
| --- | --- | --- |
| active/archived/deleted membership | SessionIndex global cache | global snapshot ready 后，普通 live-only record 不得自动追加；仅显式登记的 optimistic create 可以临时显示。 |
| `parentSessionId`、cwd、title、metadata、所有 index 时间、archive 状态、`revision` | SessionIndex record | versioned record 按 revision 合并；directory child store 不覆盖这些字段。 |
| message、part、permission、question、status、当前 streaming 状态 | directory child store / ACP notification | 保持现有实时链路，不写入 global index record。 |
| Pin、文件夹、项目手动顺序、折叠状态 | Desk 本地偏好 | 只影响展示，不写回 anureo，不改变 SessionIndex 基础顺序。 |

当前 `SessionSidebar.tsx` 会先用 live session 覆盖 global record，再把不在 global active 中的所有 live session 追加回来；`MobileSessionStatusBar` 与 `MobileSessionsSheet` 也有同类逻辑。这会让已归档或已删除 session 被 stale child store 重新显示。新实现必须使用 normalized `SessionIndexRecord` map 作为 membership 真相，并为 optimistic create 建立独立 shadow map；archive/delete 成功时还要同步清理或迁移 directory list projection，失败时恢复 shadow snapshot。

新协议的 authoritative full load 必须以 `archived="all"` 取得单个 owner-wide snapshot，再按 `archivedAt` 派生 active/archived view。两个独立 snapshot 即使按 revision 合并，也无法恢复同时缺席于两者的记录：session 若在 active snapshot 前被 archive、又在 archived snapshot 前 restore，会被两边同时排除。局部 directory refresh 同样使用该 directory 的 `all` scope。加载过程中收到的 event 与 tombstone 作为 overlay 参与同一次原子提交，避免迟到 snapshot 覆盖新事件。

authoritative absence 只在 snapshot 全部分页成功后生效：commit 时删除旧 cache 中属于成功 scope 但未出现在 snapshot 的记录；scope 外记录、失败 scope 与 optimistic shadow 均保留。这样“服务端明确不存在”与“请求失败没有拿到数据”在 store 中始终是两种状态。

### 8.6 Desk canonical record 与排序消费方

现有 vendor `Session` 没有 `revision`、`activityAt`、`treeActivityAt`、`stateChangedAt` 或 `metadataUpdatedAt`，private descriptor 也没有完整字段。Desk 必须新增本文定义的 `SessionIndexRecord`，global store 使用该类型保存 index 字段；`time.updated = activityAt` 只作为 legacy UI 兼容投影，`treeActivityAt` 必须独立保留。缺失或非法服务端时间不得用 `Date.now()` 填充，否则异常记录会被错误排到最前。

排序不能只修改 Sidebar。至少以下消费方必须迁移到一个共享的 activity/rank helper：Recent sections、worktree grouping、session row 时间、retention、edge-swipe session switch、Tray、mobile widget、MobileSessionsSheet、MobileSessionStatusBar、SessionSwitcher 和 Command Palette。共享模块应提供 `getOwnActivity`、`getTreeActivity`、root/child comparator 与 display timestamp；同一组 session 的不同视图不得自行重新推导顺序。

directory ACP standard event 可能没有 index revision。versioned 与 unversioned 数据的合并必须服从字段所有权：unversioned live event 可更新聊天实时字段，但不能覆盖已有 versioned index 字段；只有 legacy anureo 模式才允许以 `time.updated` 做窄范围的旧事件过滤。

### 8.7 Capability、fallback 与错误分类

Desk 已保存完整 `initialize.agentCapabilities`，anureo 也会在 `_meta["anureo.dev"].session.methods` 发布扩展方法，因此无需新增 RuntimeAPI、Web route、Electron IPC 或 VS Code bridge。需要新增严格 capability parser，并按当前 ACP runtime 隔离缓存：

1. 明确声明 `list`：调用 `_anureo.dev/session/list`。
2. 明确只有 `list-global`：直接调用旧 alias，不先制造一次必然失败的新请求。
3. capability 不可判定：先尝试 `list`，仅 JSON-RPC `-32601 Method not found` 回退旧 alias。
4. 权限、数据库、非法 cursor、`snapshot_expired` 和其他业务错误必须向上抛出，不得触发 alias fallback。
5. retry 只覆盖可恢复 transport/server readiness 错误；当前 `retryIf: () => true` 必须移除，永久错误不能重复请求。

### 8.8 Create 与 fork wire contract

标准 ACP `session/new` request 已允许 `_meta`，因此 title、parent 和初始 metadata 可以通过扩展参数传递，无需新建私有 create RPC。本设计冻结以下 namespace；anureo 必须验证 owner、parent、cwd 边界，并在返回 `session/new` 之前原子持久化 index record、metadata、revision，随后发布 `session.created`：

```json
{
  "cwd": "C:/repo",
  "mcpServers": [],
  "_meta": {
    "anureo.dev": {
      "title": "Implement session list",
      "parentSessionId": "ses_parent",
      "metadata": {}
    }
  }
}
```

anureo 在标准 `session/new` response 的 `_meta["anureo.dev"]` 返回 `{ session: <full record>, affectedSessions: <nearest-ancestor-first full records>, indexVersion }`；Desk 用它立即替换 optimistic shadow并更新 ancestors，随后收到的同 revision events 作为幂等 echo。初始 index record、metadata、owner version与 ancestor tree updates 必须在同一事务内写入，response 不能先于 durable commit。

父子关系冻结为 parent 与 child 必须属于同一 owner、规范化后 cwd 必须相同；unknown parent、self-parent、cycle 或跨 cwd 均返回 `-32602 invalid_params`。顶层 session 使用 `parentSessionId: null`，创建后 public API 不支持 reparent。客户端构树仍需 visited-set/cycle guard，使损坏的 legacy 数据降级为 root/孤儿显示，而不是递归溢出。

本次明确不实现“按指定 message 截断历史的 fork”。标准 `session/fork` 继续使用标准 request，Desk 不再暴露或静默丢弃 `messageId` 参数；UI 文案只描述标准 fork/复制配置能力。`createSessionFromAssistantMessage` 的真实行为是客户端读取 assistant plan 文本、创建一个全新 session，再发送组合后的 prompt；它应命名为“从 assistant plan 创建 session”，不得宣称 server 在该 message 处 fork。若以后需要 message-bounded fork，另立协议设计和 history ownership 测试，不在本接口改造中捎带实现。

### 8.9 当前生产调用链完整清单

本节只描述 2026-08-21 源码中实际存在的 anureo Desk 行为，不代表 §6–§8.8 的目标状态已经实现。仓库中 `_anureo.dev/session/list-global` 的 production 直接引用只有两层：`acpApi.session.listGlobal()` 发出请求，`listGlobalSessionPagesAcp()` 负责翻页；其余模块都通过 `useGlobalSessionsStore` 间接消费。`packages/web`、`packages/electron` 和 `packages/vscode` 没有该方法的独立 route/IPC/proxy 实现，共享 UI 直接使用当前 ACP runtime。

```text
ACP runtime
  -> acpApi.session.listGlobal()
  -> listGlobalSessionPagesAcp()
  -> useGlobalSessionsStore
      |- activeSessions
      |- archivedSessions
      |- sessionsByDirectory
      `- reviewTransferBySessionId
  -> Sidebar / Mobile / Tray / Retention / Switcher / Worktree consumers

旁路写入：
  session actions / review / multi-run -> upsert/remove/archive
  standard session/update title       -> child store + global upsert
  global session event router         -> （2026-08-21 基线快照）当时仅有 reducer；当前 production subscription 已由 `SyncProvider` 接线，具体以 §13 为准
```

#### 8.9.1 ACP adapter

`packages/ui/src/lib/acp/acp-api.ts::session.listGlobal` 当前硬编码发送 `_anureo.dev/session/list-global`：

| 行为 | 当前实现 |
| --- | --- |
| request | 可选 `archived`、`directory`、`cursor`、`limit`；只有 truthy 值写入 params，`archived=false` 被省略。 |
| directory | 不调用同文件已有的 `normDir()`；按调用方字符串原样发送。 |
| transport | `waitForAcpRuntime()` 后调用 `runtime.getContext().request()`；没有 RuntimeAPI/HTTP/Electron/VS Code 旁路。 |
| singleflight | `list-global` 不在 `READ_METHOD_TTL_MS`，API 层不合并；由 global store 的 `inflightLoad` 合并全量加载。 |
| response normalization | 非数组 `sessions` 变为 `[]`；非字符串 cursor 变为 `null`；只有 `hasMore === true` 才继续。 |
| error | ACP request error 原样抛出；adapter 没有 method-not-found 分类或 alias fallback。 |

标准 `acpApi.session.list(directory)` 是另一条路径：它调用标准 ACP `session/list`，再在客户端按 cwd 过滤并映射。当前两者没有共享 wrapper，也没有 capability-based 选择。源码中尚不存在 `listIndex` 或 `"_anureo.dev/session/list"` production 调用。

#### 8.9.2 Descriptor 与类型映射

`AcpGlobalSessionDescriptor` 当前只包含 `sessionId`、cwd、title、created/updated/archived 时间和 metadata。`acpGlobalSessionToanureoSession()` 的投影如下：

| ACP 字段 | Desk 字段/行为 |
| --- | --- |
| `sessionId` | 同时写入 `id`、`slug`。 |
| `cwd` | 去除 Windows verbatim prefix 后写入 `directory`。 |
| `title` | 缺失时使用空字符串。 |
| `createdAt` | `Date.parse` 后写 `time.created`；字段缺失时使用 `Date.now()`。 |
| `updatedAt` | `Date.parse` 后写 `time.updated`；字段缺失时回退 created。 |
| `archivedAt` | 非空时解析并写 `time.archived`。 |
| `metadata` | 原样保留。 |
| 固定值 | `projectID = ""`、`version = "acp"`。 |

非法日期字符串可能产生 `NaN`；当前投影不包含 parent、revision、activity/tree activity、state/metadata update time。分页层还会按 directory 人工写入 `record.project = { id: directory, worktree: directory }`，用于项目/worktree 分组。

#### 8.9.3 分页与终止条件

`packages/ui/src/stores/globalSessions.ts::listGlobalSessionPagesAcp` 的当前循环：

1. cursor 从 `undefined` 开始，每页调用 `listGlobal({ archived, directory, limit, cursor })`。
2. 每页通过 `retry({ attempts: 3, delay: 500, retryIf: () => true })`；权限、业务、cursor 和 transient error 当前全部重试。
3. 每条 item 映射成 Desk `Session`，以 `seenIds` 跨页去重并追加到结果。
4. 只有当前页至少追加一条新 ID 时才调用 `onPage`，但 callback 收到的是未过滤重复项的整个 payload。
5. 空 payload、`hasMore !== true`、缺少 `nextCursor`，或下一页全部 ID 已见，任一条件都会终止。
6. cursor 原样回传，客户端不解析；没有 snapshot identity、snapshot-expired restart 或 event overlay。

公开函数 `listGlobalSessionPages(_apiClient, options)` 已忽略 `_apiClient`；`roots` 参数也没有使用。它们是旧 HTTP/experimental client API 留下的兼容形状。

### 8.10 当前 global store 行为

`useGlobalSessionsStore` 保存 active、archived、active-by-directory、review transfer map、loaded/status。它同时承担全量加载、目录局部替换、引用复用和 mutation 后的本地 reconciliation。

#### 8.10.1 全量加载与失败保留

- `loadSessions()` 用模块级 `inflightLoad` 合并并发调用，page size 固定 500。
- active/archived 通过 `Promise.allSettled` 并行拉取；两者都成功才为 `ready`，任一失败则为 `error`。
- active 失败时保留旧 active 并合并调用方传入的 live fallback；archived 失败时保留旧 archived。
- `loadGeneration` 在 runtime switch 时递增，旧 runtime 的迟到 snapshot 会被丢弃。
- store 通常把失败转换为 fallback `LoadResult` 而不是继续 reject，因此上层 `.catch()` 不能作为主要失败信号。
- `ensureGlobalSessionsLoaded()` 在已加载且非 error 时直接返回 cache；`refreshGlobalSessions()` 总是进入 `loadSessions()`，但会复用正在进行的请求。

#### 8.10.2 目录刷新

- `refreshSessionsForDirectories()` 先规范化目录并去重，再对每个目录分别请求 active/archived。
- 同类目录使用 `Promise.allSettled`；只有成功目录替换对应 slice，失败目录保留旧数据。
- active 局部结果再合并 live fallback，随后重建 `sessionsByDirectory` 和 review transfer map。
- directory resolve 优先 raw `session.directory`，其次 `session.project.worktree`。

#### 8.10.3 顺序、引用与 mutation

| 路径 | 当前行为 |
| --- | --- |
| full load | 保留服务端分页顺序。 |
| directory refresh | 按 `time.updated ?? time.created DESC`，tie-break 为 `id DESC`。 |
| new upsert | 插入目标数组头部。 |
| existing upsert | 保持原数组位置，仅替换该元素。 |
| snapshot equality | signature 比较 ID、title、三类 `time`、share URL、metadata JSON、directory；相同则复用数组引用。 |
| active/archived partition | 只看 `time.archived`；upsert 时从另一分区移除。 |
| remove | 从两分区删除并重建索引；不保留 tombstone。 |
| archive | 用本地 `Date.now()` 设置 archive time，从 active 移到 archived 头部。 |
| runtime reset | 清空所有列表/索引、取消 inflight 引用并回到 `idle`。 |

当前 signature 不包含 parent、revision 或新时间字段；full、局部和 upsert 也没有单一排序契约。

#### 8.10.4 Global/live merge

`mergeLiveSessionWithGlobalSession(live, global)` 以 live object 为主体，只在 live 缺失 directory/worktree 时保留 global 稳定目录，并强制保留 global share。title、time、parent、metadata 等其余字段默认由 live 覆盖。

Sidebar、MobileSessionsSheet 与 MobileSessionStatusBar 都先用 live 覆盖同 ID global record，再把所有 global 中不存在的 live record 追加到结果。这让未打开目录也能由 global list 展示、已打开目录又能获得最新实时状态，但也会使 stale live record 重新显示已归档或已删除 session。

### 8.11 加载、刷新与重连触发点

| 触发点 | 当前逻辑 |
| --- | --- |
| `SyncProvider` mount | 完成 ACP global bootstrap 后 fire-and-forget 调用 global `loadSessions()`，是应用级初始加载。 |
| Sidebar mount | 用当前 live session snapshot 触发一次全量 refresh。 |
| `scheduled-task-ran` | Sidebar debounce 500ms 后全量 refresh。 |
| project/worktree directory 新增 | Sidebar 对新增目录执行 active/archived 局部 refresh；VS Code 初次目录集合也走该路径。 |
| Mobile status UI | 移动端启用时 ensure；panel 打开时全量 refresh。 |
| MobileSessionsSheet | 打开时全量 refresh，并将 live sessions 作为 active fallback。 |
| Tray | mount 时 ensure；owner-wide 全量 refresh 由 SyncProvider 的 60 秒 authoritative scheduler 统一负责；Tray 只轮询跨目录 status。 |
| Retention | hook mount 与每次实际 cleanup 前 ensure。 |
| ScheduledTasksDialog | 手动运行任务成功后刷新 task 与 global sessions。 |
| runtime endpoint switch | `resetForRuntimeSwitch()` 清空旧 instance cache，随后由新 SyncProvider 重载。 |
| ACP reconnect | 当前只重放/校验 directory child sessions，没有主动刷新 global list；等待 Sidebar、Tray 或其他显式 refresh。 |

### 8.12 Mutation、event 与所有下游消费方

#### 8.12.1 Action 对 global store 的直接写入

| 动作 | 当前写入顺序与边界 |
| --- | --- |
| create | 标准 `session/new` 只传 cwd；title、parentID、metadata 参数被忽略；本地构造最小 Session，注册 native store、设置当前 session 并 upsert global。 |
| standard fork | `forkFromMessage(sessionId, messageId)` 当前只把 source session/cwd 交给标准 `session/fork`，`messageId` 不进入 wire；anureo 创建新 session 并复制配置/MCP，不实现 message history boundary。 |
| assistant-plan session | `createSessionFromAssistantMessage` 在客户端定位 assistant message、拼出 plan 文本，可先建 worktree，然后调用普通 create 并发送组合 prompt；它不是 server-side fork，也没有持久化 source message relation。 |
| metadata patch | 先通过标准 active-only `session/list` 间接取现有 session，再调用 `_anureo.dev/session/update`；因此 archived session 可能在 RPC 前就无法取到。目标实现应从 canonical global record 取基础值，或直接使用 private update 的完整 response，不能依赖标准 active list pre-read。 |
| title update | 调 `_anureo.dev/session/update`；response 带 session 时映射并 upsert。 |
| archive | 先本地移到 archived 并清当前选择，再调用 `_anureo.dev/session/archive`；失败返回 false，但不 rollback。 |
| delete | 先从 global 删除并清当前选择，再调用标准 `session/delete`；失败返回 false，但不 rollback。 |
| review/multi-run create | `reviewFlow` 与 `useMultiRunStore` 在创建成功后直接 upsert。 |
| retention | 按 session 逐项调用 `anureoAgentClient`；完成后对成功 ID 批量 archive/remove，保留 partial failure。 |

标准 ACP `session/update` 中的 `session_info_update` title 会直接更新对应 directory child store；若 global cache 已有该 ID，也会以现有 record 加新 title 后 upsert。

#### 8.12.2 Global event 现状

`openAcpGlobalEventStream()` 已能通过 ref-counted topics 订阅 `_anureo.dev/global/update`，transport 收到 `{ topic, event }` 后只向 consumer 交付内层 `event`；`applySessionEventToGlobalSessions()` 也能处理 created/updated/delete，created/updated 以 `time.updated/created` 过滤旧事件，delete 按 ID 移除。但是 production 没有把这两者连接起来，notification bridge 只订阅标准 `session/update` 与 question。当前 reconnect 也不会重建 global session topic subscription，因为该 subscription 尚不存在。

anureo 侧还有一条必须拆除的冲突路径：`runtime.rs` 对每个非 history replay 的标准 `session/update` 发布 global `session.updated`，payload 只有 `properties.id`，会在 streaming 期间高频触发；`extensions/session_list.rs` 的 archive/title/metadata 则发布另一个 OpenCode-like `properties.info` 形状。当前 create/delete 不发布完整 global membership event。目标实现必须让 global session topic 只承载已提交、带 revision 的 SessionIndex record/tombstone；标准内容更新不得混入该 topic。

#### 8.12.3 下游读取清单

| 消费模块 | 使用 global list 的逻辑 |
| --- | --- |
| `SessionSidebar.tsx` | active+live 合并、archived persistence、known-directory filter、Pin/time 排序、Recent、文件夹、project/worktree 分组和 parent tree。 |
| `MobileSessionsSheet.tsx` | 全项目/worktree session tree、搜索、计数、切换与归档；打开时刷新。 |
| `MobileSessionStatusBar.tsx` | 跨项目最近 session/status panel；与 live aggregate 合并。 |
| `VSCodeLayout.tsx` | 用 active/archived 辅助当前 workspace 与 session 恢复/布局。 |
| `Header.tsx` | 当前 ID 不在 live child store 时，从 global active 查标题/目录等 session 信息。 |
| `SessionSwitcherDropdown.tsx` / `useSwitcherItems.ts` | Dropdown 负责入口与当前目录解析，hook 生成 project-scoped、Pin-aware items；缺失时间的局部 fallback 当前可能使用 `Date.now()`。 |
| `CommandPalette.tsx` | 生成 session commands，并按 `time.updated/created` 排序。 |
| `useEdgeSwipeSessionSwitch.ts` | 只取 active root session，按 `time.updated/created` 作为移动端滑动切换序列。 |
| `useTraySync.ts` | 全项目 root session 最多显示 20 条；构建 parent/descendant 关系，将 status/unseen/error 向 root 汇总；mount 时 seed，持续刷新由 SyncProvider 统一调度，避免重复 timer。 |
| `mobileWidgetSnapshot.ts` | 由 active sessions、project 与 unread state 生成移动端 widget snapshot。 |
| `useSessionAutoCleanup.ts` | 用 `time.updated ?? created` 排序，保护当前、share 和最近五个，按 cutoff archive/delete。 |
| `MultiRunFusionDialog.tsx` | 合并 active、archived 与 live，作为 fusion source session 候选。 |
| `WorktreeSectionContent.tsx` | 从 active+archived 查找直接 session 及递归 subsession，服务 worktree 操作。 |
| `MobileDeleteWorktreeDialog.tsx` | 合并 global active 与 live，找出 worktree sessions 并批量 archive。 |
| `session-ui-store.ts` | 用 `sessionsByDirectory` 解析 deep link/action directory；live 未命中时从 active+archived 回退。 |
| `router/routeSync.ts` | deep link 缺少目录时订阅 global store，最长等待约 15 秒，直到用 session ID 解析出目标目录。 |
| `reviewFlow.ts` / `useMultiRunStore.ts` | 新建 review 或 multi-run session 后直接 upsert global，并同步相关 child/directory 状态。 |
| `apps/runtimeEndpointReset.ts` | runtime endpoint 切换时调用 global store reset，隔离旧 instance 的 cache 与迟到请求。 |
| `useProjectSessionLists.ts` / `useSessionGrouping.ts` | Sidebar 内部派生项目、worktree、文件夹和树形分组；属于 SessionSidebar 排序/层级链路。 |
| `MessageList.tsx` / review flow | 读取 `reviewTransferBySessionId`，在 original/review session 之间转移或跳转。 |
| `ScheduledTasksDialog.tsx` | task 运行后显式刷新列表，使新 session 出现。 |

### 8.13 基线测试与目标实现边界（2026-08-21 快照）

本节记录目标开发开始前的 Desk 源码审查，不是当前实现状态；当前代码与验证结果以 §13（2026-08-22）为准。

当前测试覆盖非常有限：

- `globalSessions.test.ts` 名义上测试 sanitization 和多页 cursor，但仍构造旧 `AgentClient.experimental.session.list`；production 函数已经忽略 `_apiClient` 并改走 `acpApi.session.listGlobal()`，所以这些 fixture 不能真实覆盖当前 wire path。
- `useGlobalSessionsStore.test.ts` 只覆盖 share 更新、directory metadata 保留/优先级，以及归档时目录不丢失。
- `sync-context-session-events.test.ts` 只验证一个旧 `time.updated` title event 不覆盖新 title 的 reducer 场景，没有验证 production global subscription。

源码搜索确认以下目标逻辑尚未实现：

- `_anureo.dev/session/list` / `listIndex` 调用与 capability parser；
- 仅 `-32601` 触发的 alias fallback 和分类 retry；
- snapshot-expired restart、event overlay、revision merge；
- active/archived 跨分区 normalized reconciliation；
- delete tombstone、optimistic shadow 与 archive/delete rollback；
- production global session subscription；
- 现有逐标准 `session/update` 的 minimal global broadcast 拆除与单 prompt activity 边界；
- parent/tree activity 和 canonical `SessionIndexRecord` 映射；
- archived metadata patch 不依赖标准 active-only pre-read；
- 所有消费方共用的 activity/rank helper。

因此当前可运行能力应准确描述为“旧 `list-global` + 时间戳数组 cache + directory live overlay + action 直接 mutation”；SessionIndex 客户端目标状态必须通过 E1–E14 实现，不能由现有 helper 名称或单测推断为已经具备。

## 9. 迁移与兼容性

1. schema migration 按 §4 的单 transaction 规则添加 columns、owner state、tombstone、固定索引和 metadata FK；任何失败整体回滚。
2. 旧记录按冻结 backfill 初始化，parent 一律 `NULL`，禁止推测补全；既有 metadata orphan 记录数量后清理。
3. 引入 repository 的统一 mutation API，并将新建、prompt 受理边界、archive/restore、lifecycle 和标题/metadata 写入迁入；禁止 message delta 触发 index mutation。
4. anureo 注册 `_anureo.dev/session/list`，并在一个兼容窗口内保留 `_anureo.dev/session/list-global` alias；两者共享 repository/snapshot query core，alias 只保留旧 request/response projection adapter，不复制列表事实源。过渡期 capability 为 `methods: ["list", "list-global", "archive", "update"]`。
5. Desk 新增 `acpApi.session.listIndex`：capability 明确支持 `list` 时调用新方法；明确只支持 `list-global` 时直接调用 alias；capability 不可判定时先调用 `list`，只有 `-32601 method_not_found` 才回退。`-32001 capability_not_supported` 表示 session extension 域不存在，不能通过调用同域 alias 恢复。新 Desk + 旧 anureo、旧 Desk + 新 anureo、新 Desk + 新 anureo 都必须通过测试。
6. 兼容窗口结束后仅从新 anureo 删除 `list-global` alias，capability 收敛为 `methods: ["list", "archive", "update"]`。新 Desk 为最低支持的旧 anureo 保留 capability 直调与 `-32601` 回退；只有产品明确提高最低 anureo 版本、且不再支持任何仅含 `list-global` 的 anureo 后，才能另行删除客户端 fallback。标准 ACP `session/list` 改为统一 session index projection。

### 9.1 实际迁移、校验与回滚手册

当前没有独立的 migration CLI。`anureoAcpAgent::new` 创建 `SessionRepository` 时会调用 `ensure_schema()`，自动执行 `acp_sessions` 增量列、SessionIndex 表/索引和 `acp_session_data` foreign-key 重建；ACP 使用的数据库由 `checkpoint_sqlite_store::default_memory_db_path()` 决定，通常是 `{ANUREO_HOME}/memory.db`（未设置 `ANUREO_HOME` 时为平台默认 anureo home 下的 `memory.db`）。因此发布迁移必须按下面的顺序执行：

1. **停写并确认路径**：停止所有使用该 anureo home 的 ACP/Desk host，确认没有第二个 `anureo-acp` writer；记录实际 `ANUREO_HOME` 和数据库绝对路径，不要只依赖默认值。
2. **做可恢复备份**：ACP 停止后复制 `memory.db`，若存在同时复制 `memory.db-wal` 与 `memory.db-shm`；备份文件名必须带版本和时间。不要在 ACP 运行时直接复制 SQLite 主文件。
   - PowerShell：`Copy-Item $dbPath "$dbPath.pre-session-index-<version>-<utc>.bak"`
   - POSIX shell：`cp "$dbPath" "$dbPath.pre-session-index-<version>-<utc>.bak"`
3. **启动新 anureo 触发 migration**：只启动一个新版本 ACP，等待 initialize 成功；启动失败时保留原始数据库和日志，不重复执行并发启动。
4. **数据库校验**：在只读 SQLite 连接上确认 `acp_sessions` 含 `parent_session_id/activity_at/tree_activity_at/state_changed_at/metadata_updated_at/revision/index_version`，存在 `acp_session_index_state`、`acp_session_tombstones`，且 `PRAGMA foreign_key_check` 返回零行；同时记录 metadata orphan 查询结果。`PRAGMA foreign_keys=ON` 是 repository 每个连接的运行时设置，不能用 SQLite CLI 默认连接的 pragma 值代替。
5. **应用回归**：在源码 workspace 执行 `cargo test -p anureo-acp --lib -- --test-threads=1`、`cargo check -p anureo-acp --lib` 和 `cargo clippy -p anureo-acp --lib -- -D warnings`；发布记录必须保存命令、commit、数据库备份名和输出摘要。
6. **失败处理**：若 schema 初始化或校验失败，停止 ACP，保留失败数据库副本和日志，再从备份恢复主文件（连同 WAL/SHM 一起处理）后启动旧版本。不要手工删除新列/表，也不要在未备份的原文件上反复重试。
7. **回滚边界**：迁移前备份是唯一批准的数据库回滚点；恢复后必须重新执行一次只读完整性检查并验证 session/list、session/load、session/delete。备份恢复不能回滚已经发出的 global event 或 Desk 本地缓存，回滚后应让 Desk 做 runtime switch/reconnect 的 authoritative resync。

ACP wire-level E2E harness 支持通过 `ANUREO_ACP_BINARY` 指定待测 `anureo` executable；未设置时仍使用 Cargo 自动发现的当前 binary。兼容联调应为每个 anureo 版本使用独立 `ANUREO_HOME`，保存 initialize capability、canonical/legacy list 请求序列和错误响应，不能让旧版本与新版本共享数据库或缓存。

仓库提供 `scripts/run-session-list-compat.ps1 -NewanureoBinary <PATH> [-OldanureoBinary <PATH>] [-OutputDir <DIR>]` 作为可重复 runner。默认逐个设置 `ANUREO_ACP_BINARY`，执行不依赖模型的 `cargo test -p anureo-acp --test e2e_session_list -- --nocapture`；`-OldanureoBinary` 运行会额外设置 `ANUREO_SESSION_LIST_EXPECT_LEGACY=1`，测试只断言旧 peer 的 `list-global` fallback 与标准 list，不把 canonical 方法缺失误判为失败；可通过 `-TestName e2e_mega` 显式运行完整 mega 流程（`e2e` target 仅是 harness crate 根，不包含测试）。每次 stdout/stderr 保存为独立 log，并写出包含 binary、`expectedMode`、UTC 时间、exit code、实际 test count、OS/架构、PowerShell 版本和 log 路径的 `manifest.json`；若 target 意外执行零测试，runner 失败而不会报告假绿；脚本不会删除输入 binary、数据库或旧结果目录。

以上步骤是运维签收要求，不是“代码测试通过”的替代品；真实数据库备份、升级和恢复演练仍需在发布环境完成并附在 §12 验收记录中。

必须创建的索引：

```sql
CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_active_root
  ON acp_sessions(owner_principal, tree_activity_at DESC, session_id ASC)
  WHERE archived_at IS NULL AND parent_session_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_active_child
  ON acp_sessions(owner_principal, parent_session_id, activity_at DESC, session_id ASC)
  WHERE archived_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_archived
  ON acp_sessions(owner_principal, archived_at DESC, session_id ASC);
CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_cwd_membership
  ON acp_sessions(owner_principal, cwd, archived_at, session_id ASC);
```

## 10. 详细开发任务列表

任务按依赖顺序分为七个阶段。每项应独立提交并保持可测试；兼容 alias 存续期间只能保留薄 legacy projection adapter，不得复制 repository 查询或列表事实源。

### 10.1 阶段 A：契约冻结与回归基线

| ID | 任务 | 涉及文件 | 验收标准 | 依赖 |
| --- | --- | --- | --- | --- |
| A1 | 冻结 `_anureo.dev/session/list` request/response/event、错误码、cursor version 和 capability 形状，修正 `session_list.rs` 顶部误指向不存在的 `39-session-list.md` 注释，并立即替换过期的 37 号规范 | `docs/acp-spec/extensions/37-session-list.md`、本设计文档、`apps/acp/src/extensions/session_list.rs` | 文档明确 `archived="all"` owner-wide snapshot、完整字段/nullability、嵌套 `{topic,event}` wire、`idle/closed` lifecycle、authoritative absence、标准/私有职责与三态 fallback；源码注释指向 37；无版本化新 RPC。 | 无 |
| A2 | 为现有 `list-global` 建立行为基线测试 | `apps/acp/src/extensions/session_list.rs`、`apps/acp/src/session_repository.rs` tests | 覆盖 owner、archived、directory、limit、cursor、metadata；测试能稳定复现同 timestamp 跨页漏项。 | A1 |
| A3 | 固化 Desk 当前分页和失败保留行为 | `packages/ui/src/stores/globalSessions.test.ts`、`useGlobalSessionsStore.test.ts` | 覆盖多页、重复 ID、空页、重复页停止、active/archived 部分失败、runtime switch 迟到响应。 | A1 |

### 10.2 阶段 B：SessionIndex schema 与 repository

| ID | 任务 | 涉及文件 | 验收标准 | 依赖 |
| --- | --- | --- | --- | --- |
| B1 | 增加 session index columns、owner state/tombstone tables 与原子 migration | `apps/acp/src/session_repository.rs` | 严格按 §3.1/§4 回填；metadata table 重建为 cascade FK；orphan 计数清理；`foreign_key_check`、失败回滚和重复 migration 全部通过。 | A2 |
| B2 | 扩展 `SessionMetadata`/`SessionIndexRecord` 与 row mapping | `apps/acp/src/session_repository.rs` | 所有 get/list/mutation 路径完整读写 `revision/index_version` 和新字段；旧数据只使用冻结 backfill，不在读路径猜测。 | B1 |
| B3 | 增加 active root、active child、archived 与 cwd membership 索引 | `apps/acp/src/session_repository.rs` | migration 创建 partial/covering target indexes；root、child、archive 和 directory scope 查询计划不退化为无界全表扫描。 | B1 |
| B4 | 引入完整 `SessionIndexMutation` API 与 owner version allocation | `apps/acp/src/session_repository.rs` | create 到 delete 均返回 immutable mutation result；真实 transaction 只分配一个 owner indexVersion；改变 records 各自 revision +1；no-op 不改时间/version/event。 | B2 |
| B5 | 实现双向 tree activity 重算 | `apps/acp/src/session_repository.rs` | activity 可上推；archive/delete/reparent 可使旧祖先下降并更新新祖先；受影响祖先 revision/event 完整，循环或断裂 parent 链不产生部分更新。 | B4 |
| B6 | 实现稳定基础查询与有效 root 投影 | `apps/acp/src/session_repository.rs` | active effective-root、child、archived 均使用文档定义的总序和 `session_id` tie-break；parent archived/missing 时 active child 不丢失；cwd 规范化一致。 | B3、B5 |
| B7 | 新增 owner-scoped in-memory snapshot store 与 HMAC cursor | `apps/acp/src/session_list_snapshot.rs`、repository | 严格执行 5 分钟、4/owner、64 MiB/owner、256 MiB/process、LRU、1024-byte cursor 和 restart-expired 语义；完整 projection/snapshotVersion 冻结，后续页零 DB 查询。 | B6 |
| B8 | 固定 `acp_session_data` FK/cascade 与 connection pragma | `apps/acp/src/session_repository.rs` | 每个 connection 开启 foreign keys；delete/delete_all 无 orphan；migration fault injection 证明 table rebuild 与 session schema 同进同退。 | B1 |
| B9 | 实现 durable tombstone lifecycle | `apps/acp/src/session_repository.rs` | delete 同事务写紧凑永久 tombstone并清 session rows；任意重试/重启返回原 tombstone；session ID reuse 被拒绝。 | B4、B8 |

### 10.3 阶段 C：anureo 写入链路与层级

| ID | 任务 | 涉及文件 | 验收标准 | 依赖 |
| --- | --- | --- | --- | --- |
| C1 | session/new 持久化 parent 与初始 metadata | `apps/acp/src/agent.rs`、`session_repository.rs`、相关 request adapter | 顶层 parent 为 NULL；显式 child 正确持久化；owner/cwd 边界不被 metadata 绕过。 | B4 |
| C2 | 按冻结边界接入 prompt activity并拆除逐 `session/update` global 广播 | `apps/acp/src/agent.rs`、`apps/acp/src/runtime.rs`、`session_repository.rs` | busy lease 后、executor 前提交；DB 失败不启动 executor；validation/busy/replay 为零，cancel/model/start failure 保留一次；delta 不写 index、不发布 minimal `{id}`。 | B4、B5 |
| C3 | archive/restore/lifecycle/title/metadata/reparent/delete 改用专属 mutation | `agent.rs`、`extensions/session_list.rs`、`session_repository.rs` | 管理操作不改变 activity；tree membership 变化重算旧/新祖先；title+metadata 原子；no-op、重复 delete 与真实 mutation 严格遵循 §3.1。 | B4、B5、B9 |
| C4 | 删除直接写 `updated_at` 的旁路 | `apps/acp/src/**/*.rs` | 源码检索确认 session index 时间写入仅通过 repository mutation API；例外有注释与测试。 | C2、C3 |

### 10.4 阶段 D：anureo 列表协议与事件

| ID | 任务 | 涉及文件 | 验收标准 | 依赖 |
| --- | --- | --- | --- | --- |
| D1 | 将 extension handler 重构为 `handle_list` | `apps/acp/src/extensions/session_list.rs` | `_anureo.dev/session/list` 支持 `archived=all/active/archived`；Desk 全量使用单个 owner-wide snapshot；payload 满足完整 schema/nullability。 | B7、C3 |
| D2 | 增加 `list-global` 兼容 projection adapter | `extensions/session_list.rs` | 两方法共享 snapshot/query core；alias 接受 boolean archived 并返回旧 `updatedAt` descriptor，新方法返回完整 record；过渡期 capability 精确声明两方法。 | D1 |
| D3 | 消除列表 metadata N+1 查询 | `session_repository.rs`、`extensions/session_list.rs` | metadata 随 repository page 批量读取或 JOIN；每页查询次数不随 session 数线性增长。 | D1 |
| D4 | 统一 created/updated/deleted 列表事件与 drop observability | `extensions/session_list.rs`、`runtime.rs`、global bus、create/delete handler | wire/serializer/tombstone 符合规范；commit 后 fire-and-forget，drop 不回滚且有 counter/安全日志；不存在同名 minimal event；同 transaction records 共享 indexVersion。 | C1–C3 |
| D5 | 标准 ACP `session/list` 改为兼容单页 SessionIndex projection | `apps/acp/src/agent.rs`、`stdio_loop.rs` | owner/cwd 有效；一次返回全部 active records，`nextCursor=null`；ACP 字段兼容，本次不附加 checkpoint `_meta`。 | B6、C1 |
| D6 | 删除遗留 checkpoint 列表聚合路径 | `apps/acp/src/agent.rs`、相关 docs/tests | 删除 `list_sessions_from_db` 及仅服务该路径的 helper/tests；production 与测试均不存在第二个 membership/order 实现。 | D5 |
| D7 | 实现 create `_meta`/response contract，并收紧 fork 语义 | `agent.rs`、ACP request adapter、`37-session-list.md` | `session/new` 原子接收 title/parent/metadata，response `_meta` 返回 target+ancestors+indexVersion并按序发 events；message-bounded fork 为非目标，Desk 不再传/忽略 messageId。 | A1、C1 |
| D8 | 实现 archive/update/delete 精确 response contract | `extensions/session_list.rs`、`agent.rs`、standard delete handler | archive 返回 target+nearest-first ancestors；update 原子返回 record/metadata；delete `_meta` 返回 durable tombstone；旧 client 可忽略新增字段。 | B9、C3、D4 |

### 10.5 阶段 E：anureo Desk 适配与状态一致性

| ID | 任务 | 涉及文件 | 验收标准 | 依赖 |
| --- | --- | --- | --- | --- |
| E1 | 新增 canonical `SessionIndexRecord`/tombstone 与共享 strict parser | `packages/ui/src/lib/acp/type-mapping.ts`、`types.ts`、global store types | list/event/response 共用 parser；parent/time/archive/revision/indexVersion/snapshotVersion 无损映射，非法必填字段直接报错。 | D1、D4、D8 |
| E2 | 新增 `acpApi.session.listIndex`、capability parser 与错误分类 | `packages/ui/src/lib/acp/acp-api.ts`、`extensions.ts`、`acp-runtime.ts`、`types.ts` | 明确仅旧方法时直调 alias；capability 未知时只有 `-32601` fallback；`-32001` 与业务错误不 fallback；标准 `acpApi.session.list` 不变。 | D2 |
| E3 | 分页循环处理 owner-wide snapshot、容量/过期与 retry policy | `packages/ui/src/stores/globalSessions.ts` | full/directory 使用 all；校验每页 snapshotVersion 相同；过期有限重启；capacity/invalid cursor 不 fallback、不清 cache；仅 transient error 重试。 | E2 |
| E4 | global cache 改为 normalized revision merge 与 scoped replace | `useGlobalSessionsStore.ts`、`globalSessions.ts` | 成功 snapshot 对其 scope 执行 authoritative absence；失败 scope 保留；snapshot/cache/event overlay 原子合并后再派生 active/archived；未变化 record 保持引用。 | E1、E3 |
| E5 | 实现 versioned tombstone 与 optimistic shadow map | `useGlobalSessionsStore.ts`、`session-event-router.ts` | 旧 snapshot/event 不复活；只有完成 owner-wide snapshot 且 snapshotVersion 覆盖 tombstone 才清理；directory snapshot 不清；仅登记的 optimistic create 可 live-only。 | E4、D4 |
| E6 | 接通 global session event production 链路 | `acp-event-source.ts`、`sync-context.tsx` 或 notification bridge、`session-event-router.ts` | 每个 runtime 只有一个 session topic subscription；新事件使用共享 parser，legacy event 走隔离 adapter；断线/runtime switch 正确 dispose/resubscribe，旧 runtime event 被丢弃。 | D4、E4、E5 |
| E7 | 定义 global index 与 directory child store 字段所有权 | `useGlobalSessionsStore.ts`、`SessionSidebar.tsx`、mobile session surfaces | index 决定 membership 和 versioned 字段；live store 只补聊天实时字段；authoritative load 后 stale live record 不会复活 archived/deleted session。 | E4–E6 |
| E8 | 统一 activity/rank helper 与所有消费方 | sidebar、Recent、worktree grouping、retention、Tray、mobile、switcher、command palette | root 使用 tree activity、child 使用 own activity；所有视图共享 comparator/display helper；legacy fallback 被隔离。 | E1、E7 |
| E9 | sidebar parent tree 与防御性构树 | `SessionSidebar.tsx`、`sidebar/hooks/useSessionGrouping.ts` | Pin 仅作为同层偏好；Recent 不重复展示 child；缺失 parent/cycle/cross-cwd 按冻结 contract 安全降级。 | E8 |
| E10 | archive/delete action response reconciliation 与回滚 | `lib/acp/acp-session-actions.ts`、global/directory stores | archive 合并 target+ancestors；delete 用 response tombstone提交 shadow；失败恢复 global/directory/selection；response 丢失重试幂等；event echo 不重复移动。 | E5–E7、D8 |
| E11 | retention 收敛到 canonical ACP action surface | `hooks/useSessionAutoCleanup.ts`、`lib/acp/acp-session-actions.ts` | 保留 per-item partial failure；使用 shared activity helper；成功项更新所有相关 store，失败项不消失。 | E8、E10 |
| E12 | 创建 response 与 fork/action 语义补齐 | `acp-session-actions.ts`、`session-ui-store.ts`、anureo 对应 handler | create 传 title/parent/metadata，并用 response target+ancestors 原子替换 shadow/global tree；移除被忽略的 fork messageId，把 assistant-plan action 表述为新建+发送 plan。 | D7、E1 |
| E13 | 修复 archived metadata update 的基础记录读取 | `acp-session-actions.ts`、global store | archived session 的 metadata patch 不经过标准 active-only `session/list` pre-read；成功使用完整 private response/revision merge，失败保留原 record。 | E1、E4 |
| E14 | 集中 authoritative resync scheduler | global store、`sync-context.tsx`、Tray/Sidebar loaders | bootstrap/reconnect/runtime switch 立即 load；visible+online 每 60 秒 singleflight；hidden/offline 暂停并按规则恢复；移除 consumer 重复 timer；event drop 最迟一个周期修复。 | E3–E6 |

### 10.6 阶段 F：验证、兼容矩阵与性能

| ID | 任务 | 验收标准 | 依赖 |
| --- | --- | --- | --- |
| F1 | Rust repository/extension unit tests | migration/FK、owner version/no-op、total order、固定 snapshot limit/HMAC cursor、durable tombstone、activity 边界和 tree propagation 全覆盖。 | B1–D4、D8 |
| F2 | ACP integration/E2E | 标准单页/private snapshot、精确 mutation responses、event drop+resync、重启与幂等 delete 一致。 | D4–D8 |
| F3 | Desk store/action tests | snapshotVersion、capacity、overlay、tombstone cleanup、防 live 复活、response reconciliation、60 秒 resync、runtime switch 全覆盖。 | E1–E14 |
| F4 | 新旧兼容矩阵 | 新 Desk+旧 anureo、旧 Desk+新 anureo、新 Desk+新 anureo 均通过；明确旧 capability 直调 alias，未知 capability 仅 `-32601` 触发 fallback。 | D2、E2 |
| F5 | 固定性能预算 | 使用 10k records、每条 1 KiB metadata 的 fixture：snapshot SQL statements ≤3、后续页 0 SQL、构建 p95 ≤500 ms、20 页 loopback traversal p95 ≤2 s、Desk merge p95 ≤100 ms、retained snapshot accounting ≤64 MiB/owner；10k token deltas只产生 1 次 activity transaction。基准记录 runner CPU/RAM/OS，连续 20 次取 p95。 | B7、C2、D3、E4、E8 |
| F6 | 验证命令 | anureo 运行 scoped `cargo nextest` 与 `cargo clippy`；Desk 运行相关 workspace `type-check`、`lint`、测试和因 export/type 变化需要的 `dead-code`。 | F1–F5 |

### 10.7 阶段 G：发布与兼容 alias 清理

| ID | 任务 | 验收标准 | 依赖 |
| --- | --- | --- | --- |
| G1 | 发布含新方法和旧 alias 的 anureo | capability 同时声明 `list`/`list-global`；记录旧 alias 调用量但不记录敏感参数。 | F1、F2 |
| G2 | 发布使用 `listIndex` 的 Desk | capability 优先新方法；错误指标能区分 snapshot expired、method missing 和数据库失败。 | F3、F4 |
| G3 | 满足量化门槛后从 anureo 删除 alias | stable/canary 聚合指标连续 14 天为 0 次 alias call，且最低支持 Desk 版本已包含 `listIndex`；无集中遥测的发行渠道至少保留 2 个 stable releases。只删除 anureo alias branch/capability 与 COMP-02 成功 fixture；Desk 的旧 anureo fallback 和 COMP-01 持续保留，直到最低支持 anureo 版本另行提高。 | G1、G2 |
| G4 | 收尾文档 | `37-session-list.md` 标记最终方法；本设计状态改为已实现并记录验证结果；HTML 同步。 | G3、F6 |

### 10.8 固定执行顺序

```text
A1 -> A2/A3
   -> B1 -> B2/B3/B8 -> B4 -> B5/B6/B9 -> B7
   -> C1/C2/C3 -> C4
   -> D1 -> D2/D3/D4/D7/D8 -> D5 -> D6
   -> E1/E2 -> E3/E4 -> E5/E6/E7 -> E8/E9 -> E10/E11/E12/E13/E14
   -> F1/F2/F3 -> F4/F5 -> F6
   -> G1 -> G2 -> G3 -> G4
```

最小可发布切片是 B1–B9 + C1–C4 + D1–D4 + D7/D8 + E1–E14 + F1–F4：它提供新方法、稳定 snapshot/version、精确 mutation response、丢事件恢复、跨 store 事实源和失败回滚，同时保留旧 alias。D5/D6 的标准 ACP 统一和 G3 的 alias 删除不得提前到兼容矩阵通过之前。

## 11. 测试用例开发清单

测试代码应与实现任务同步提交，而不是在功能完成后集中补写。Repository 测试优先放在现有 `apps/acp/src/session_repository.rs` 的 test module；extension handler 测试放在 `apps/acp/src/extensions/session_list.rs`；跨 handler、标准 ACP dispatch 和重启持久化场景放在 `apps/acp/tests/`。Desk 优先扩展现有 `packages/ui/src/stores/globalSessions.test.ts`、`packages/ui/src/sync/__tests__/sync-context-session-events.test.ts` 和 `packages/ui/src/components/session/sidebar/utils.test.ts`；只有隔离 adapter 行为时才新增 `packages/ui/src/lib/acp/__tests__/session-list-index.test.ts`。

### 11.1 测试基础设施

| ID | 开发项 | 实现要求 | 服务用例 |
| --- | --- | --- | --- |
| TFX-01 | Session fixture builder | 可声明 owner、cwd、parent、activity/tree/state/meta 时间、archived、revision/indexVersion；默认值固定，不读取真实时钟。 | R-01～R-22、API-01～API-17 |
| TFX-02 | 可控时钟 | Repository mutation 与 snapshot TTL 通过注入 clock 获取时间；测试禁止 `sleep` 等待过期。 | R-04、R-05、R-11、R-12 |
| TFX-03 | Snapshot harness | 可创建、翻页、推进 TTL、篡改 cursor，并读取 snapshot 数量用于清理断言。 | R-03～R-08、UI-04、UI-05 |
| TFX-04 | ACP fake transport | 可配置 capability、逐页 response/snapshotVersion、method/domain/capacity 错误、迟到 response、drop event、global subscription 生命周期和事件插入点；记录每次 wire method/params。 | UI-01～UI-28、COMP-01～COMP-03 |
| TFX-05 | Query/write instrumentation | 测量单页 SQL 查询次数和一次流式响应产生的 session-index 写次数。 | API-08、PERF-01、PERF-02 |
| TFX-06 | Shared assertion helpers | 断言 session ID 唯一、总序、完整 projection、引用复用，以及 global/directory store 一致。 | 全部列表与 Desk store 用例 |

### 11.2 anureo repository 测试

| ID | 前置状态与操作 | 核心断言 | 关联任务 |
| --- | --- | --- | --- |
| R-01 | 用旧 schema 建库并连续运行 migration 两次。 | 新列、索引和默认值正确；重复 migration 不报错、不覆盖旧数据。 | B1、B3 |
| R-02 | 插入多个相同 `activity_at` 的 active root，ID 顺序打乱后查询。 | 严格按 `activity_at DESC, session_id ASC` 返回，重复执行结果一致。 | B6 |
| R-03 | 相同时间戳记录跨越 page size，连续读取全部页。 | 每个 session 恰好出现一次；无遗漏、重复和空洞页。 | B6、B7 |
| R-04 | 读取第 1 页后，更新尚未读取 session 的 activity，再读取后续页。 | 同一 snapshot 的成员、字段和顺序保持创建时视图；新查询可看到更新。 | B7 |
| R-05 | 创建 cursor，推进可控时钟超过 TTL 后请求下一页。 | 返回稳定的 `snapshot_expired` 错误；不返回空成功或部分列表；snapshot 被清理。 | B7 |
| R-06 | owner A 创建 cursor，owner B 复用。 | 明确拒绝，且错误不泄露 owner A 的记录或总数。 | B7 |
| R-07 | 用 active/cwd=A 创建 cursor，再改为 archived 或 cwd=B 复用。 | filter hash 不匹配时拒绝；原 snapshot 仍可用原 filter 继续。 | B7 |
| R-08 | 修改 cursor 字节、offset、version 或使用超界 offset。 | 统一返回 invalid-cursor 类错误，不 panic、不回退成第一页。 | B7 |
| R-09 | 创建 root → child → grandchild，对 grandchild 记录 activity。 | leaf `activity_at` 更新；所有祖先 `tree_activity_at` 前移；无关树不变；revision 按可见变更递增。 | B4、B5 |
| R-10 | 构造缺失 parent、自引用和循环 parent 链后记录 activity。 | mutation 原子失败或按约定安全截断；不得死循环、产生部分祖先更新或跨 owner 传播。 | B5 |
| R-11 | 依次执行 title、metadata、archive、restore、lifecycle mutation。 | `activity_at` 不变；只更新对应时间/字段；每次可见变更 revision 恰好 +1。 | B4、C3 |
| R-12 | 对同一 session 执行 no-op 与真实 mutation。 | no-op 不增加 revision；真实 mutation 单调 +1；并发写入不产生重复 revision。 | B4 |
| R-13 | 创建 metadata 后删除 session，并模拟 transaction rollback。 | 成功删除无 orphan；失败时 session 与 metadata 同时保留。 | B8 |
| R-14 | 创建 snapshot 后修改尚未读取记录的 title、metadata、parent 与 lifecycle。 | 旧 snapshot 后续页返回创建时完整 projection，新 snapshot 返回新字段；证明不是按 ID 回表读取。 | B7 |
| R-15 | root 有两个 child，依次 archive/delete 最活跃 child，再 restore/reparent。 | 旧祖先 tree activity 可下降，新祖先可上升；受影响祖先 revision 递增且不影响无关树。 | B4、B5、C3 |
| R-16 | 用 `archived="all"` 创建 snapshot，在翻页间执行 archive、restore 和分区往返。 | 每个 session 恰好出现一次，membership 与字段保持 snapshot 创建时视图；不会因两分区切换而双缺席。 | B7、D1 |
| R-17 | 单独 archive parent、保留 active child，再 restore parent；另构造缺失 parent legacy row。 | archive 后 child 成为 effective root 且仍出现一次；restore 后重新挂回；缺失 parent 不使 active record 丢失。 | B5、B6、D1 |
| R-18 | 从旧 schema 含有效 metadata/orphan metadata 执行 migration，并在每个步骤注入失败。 | 回填值完全符合 §4；orphan 计数后不复制；FK/cascade/foreign_key_check 生效；任一失败恢复原 schema/data；重复 migration no-op。 | B1、B8 |
| R-19 | 先读取空 owner snapshot，再执行多 record tree mutation、各类 no-op、另一 owner mutation、系统时钟回拨，并将 version/revision 预置到 JSON safe integer 上界。 | 空 owner snapshotVersion=0；第一次真实 mutation=1；每个真实 transaction 只分配一个 owner indexVersion，受影响 records 共享；no-op 不分配；不同 owner 独立单调；字段时间至少前移 1µs；越界返回 version_exhausted 且无部分写入/event。 | B4、B5 |
| R-20 | 填满 per-owner/process snapshot 数量和 byte limit，篡改 HMAC，超过 1024 bytes，并模拟 server restart。 | TTL/LRU/capacity code 精确；无越界内存；非法签名为 invalid_cursor，restart 为 snapshot_expired；后续页零 DB。 | B7 |
| R-21 | 分别触发 validation fail、busy、lease 后 DB fail、executor start fail、cancel、model fail、success 和 history replay。 | 只有取得 lease且 Activity commit 成功的非 replay prompt各写一次；DB fail 不启动 executor；其余不多写。 | C2 |
| R-22 | delete 后丢 response并多次重试、重启，再尝试复用 ID。 | 始终返回同 tombstone且不增 version/event；tombstone 不含 metadata/title；ID reuse 永久拒绝。 | B9、D8 |

### 11.3 anureo extension、标准 ACP 与事件测试

| ID | 前置状态与操作 | 核心断言 | 关联任务 |
| --- | --- | --- | --- |
| API-01 | 以等价 filter 分别调用 `_anureo.dev/session/list` 与 `list-global`。 | membership、顺序、cursor/error 语义来自同一 core；新方法返回完整 record，alias 返回含 `updatedAt` 的 legacy descriptor；无双 repository 查询实现。 | D1、D2 |
| API-02 | 读取过渡期 capability snapshot。 | `session` 同时声明 `list`、`list-global`、`archive`、`update`；标准 ACP capability 不被私有方法覆盖。 | D2 |
| API-03 | 混合两个 owner、两个 cwd、active/archived 数据查询私有列表。 | owner 强隔离；cwd 规范化后一致匹配；active/archived 过滤准确。 | D1 |
| API-04 | 查询包含 parent、metadata、关闭时间和不同 revision 的记录。 | list item 完整投影 `parentSessionId/activityAt/treeActivityAt/stateChangedAt/metadataUpdatedAt/closedAt/revision/metadata`。 | D1 |
| API-05 | 对 expired、篡改、跨 owner/filter cursor 调用 handler。 | JSON-RPC 错误 code/data 可区分 snapshot expired 与 invalid cursor；不伪装为空列表。 | D1 |
| API-06 | 执行 create/update/archive/restore/delete 并捕获 global session events，再重放旧 revision。 | created/updated 与 list item 同构；delete 带最终 revision tombstone；revision 单调；同一 mutation 不发字段矛盾的事件。 | D4 |
| API-07 | 调用标准 ACP `session/list`，数据含 archived、不同 owner/cwd。 | 只返回当前 owner/cwd 的 active sessions，标准字段保持兼容，基础时间来自 SessionIndex。 | D5 |
| API-08 | 分别查询 1、100、1000 条带 metadata 的一页。 | SQL 查询次数为常数级，不随 session 数线性增长；无 metadata N+1。 | D3 |
| API-09 | 通过标准 `session/new` `_meta` 新建 child 并传 title/parent/metadata；重启后再次查询。 | target/ancestor/indexVersion 原子持久化；response `_meta`、created/ancestor events、list 与重启结果一致且顺序固定。 | C1、D7、F2 |
| API-10 | 运行生产标准/private dispatch 并监测 legacy checkpoint list path。 | 两个 projection 均由 SessionIndex 派生；生产路径不再调用 legacy aggregation。 | D5、D6 |
| API-11 | 删除 session 后重放删除前 snapshot item、created 和 updated event。 | tombstone revision 均拒绝旧记录；directory 或旧 owner-wide snapshot 不清理，只有完成 `snapshotVersion >= tombstone.indexVersion` 的 owner-wide `all` snapshot 才清理对应 client tombstone。 | D4、E5 |
| API-12 | 调用标准 fork 与 Desk 的 assistant-plan 新建动作。 | 标准 fork 不携带 source message 扩展；plan 动作不声称 server history fork，也不静默丢弃 message ID 参数。 | D7、E12 |
| API-13 | 捕获 created/updated/deleted wire JSON，并让同一 parser 读取 list item 与 event info。 | transport 严格为 `{topic,event}`；新事件只使用 `sessionId`；created/updated `properties.info` 与 list item 同构；delete 只使用冻结 tombstone；legacy `id/time` 不进入新 parser。 | D4、E1 |
| API-14 | 对一个长 prompt 产生大量 token/part/tool `session/update`，并捕获 global topic 与 repository writes。 | 每个 prompt 至多一次 Activity revision/full-record event；没有 minimal `{id}` 同名事件；history replay 为零；写入和 global event 数不随 delta 数增长。 | C2、D4 |
| API-15 | 捕获 archive/update/delete 成功、no-op、非法参数和 retry response。 | response shape/完整 record/ancestor order/atomicity/tombstone `_meta` 精确；旧 client 可忽略新增字段；no-op 无 event/version。 | D8 |
| API-16 | global outbound queue 填满后执行 mutation，再恢复连接并 resync。 | mutation/actor response 成功；drop counter + 安全日志出现；无 DB rollback；authoritative snapshot 最终恢复另一 client。 | D4、E14 |
| API-17 | owner/cwd 下创建超过 200 个 active records，使用只调用一次的标准 ACP client。 | 一次返回全部 active records，`nextCursor=null`，顺序来自 SessionIndex，无 archived/其他 owner/cwd。 | D5 |

### 11.4 anureo Desk adapter、store 与 sidebar 测试

| ID | 前置状态与操作 | 核心断言 | 关联任务 |
| --- | --- | --- | --- |
| UI-01 | capability 同时含 `list` 与 `list-global`，调用 `listIndex`。 | 只发送 `_anureo.dev/session/list`；标准 `acpApi.session.list` 未被调用或改写。 | E2 |
| UI-02 | capability 只含 `list-global`，模拟旧 anureo。 | 发送旧方法且返回正常；不先制造一次必然失败的新方法请求。 | E2、F4 |
| UI-03 | capability 声明 `list`，服务端返回 DB/权限/参数错误；再以未知 capability 返回 `-32001`。 | 错误向上抛并保留 cache；不得回退 `list-global` 掩盖真实故障。只有未知 capability 下的 `-32601` 允许兼容 fallback；`-32001` 不 fallback。 | E2 |
| UI-04 | fake transport 返回三页 opaque cursors。 | cursor 原样传回，不解析或重建；全部记录按 ID 去重，终止条件正确。 | E3 |
| UI-05 | 第 2 页返回 `snapshot_expired`，随后新 snapshot 成功。 | 丢弃旧 snapshot 的部分 accumulator，有限次从第一页重载；重载失败则保留旧 cache，不提交半套结果。 | E3 |
| UI-06 | directory `archived="all"` scope 成功后再失败，并并行让另一 directory scope 成功。 | 成功 scope 原子替换 active/archived membership；失败 scope 两分区均保留；错误状态可见。 | E3、F3 |
| UI-07 | 先加载 revision 10，再依次注入 revision 9、10、11 事件。 | 9/10 不覆盖或移动记录；11 更新一次；未变记录保持对象引用。 | E4 |
| UI-08 | snapshot 正在翻页时注入同一 session 的更新/删除事件。 | overlay 在原子 commit 时参与 revision 合并；无复活、重复或事件被 snapshot 旧值覆盖。 | E3–E5 |
| UI-09 | 对同一数据分别执行 full load、directory refresh 和 upsert。 | 三条路径使用同一 normalized merge 与 comparator；结果顺序一致；无变化时引用复用。 | E4、E8 |
| UI-10 | 构造两个 root、多个 child、Pin 和不同 tree/activity 时间。 | root 按 tree activity；child 归入正确 parent并按 activity；Pin 只改变同层规则，Recent 不重复展示 child。 | E8、E9 |
| UI-11 | archive/delete 乐观更新后 RPC 失败，再测试成功和 event echo。 | 失败恢复 global/directory/selection/review-transfer；成功只提交一次，echo 幂等。 | E10 |
| UI-12 | retention 批量处理多个 session，其中部分 RPC 失败。 | 使用 shared activity helper；成功项提交、失败项保留且可重试，不产生 cache 分叉。 | E11 |
| UI-13 | 创建 root/child 并传 title、parentID、metadata。 | request 完整；response target+ancestors 原子替换 shadow/tree；event echo 幂等；已知 cwd 不依赖迟到 store lookup。 | E12 |
| UI-14 | 触发标准 fork 与“从 assistant plan 创建 session”。 | 标准 fork request 不携带被忽略的 messageId；plan 动作只使用客户端已读取的 plan 文本，新建 session 后发送 prompt，UI 不宣称 message-bounded fork。 | E12 |
| UI-15 | owner-wide `all` snapshot 翻页时执行 archive/restore，并注入更高 revision event。 | snapshot 保持创建时分区；commit 后 event overlay 胜出；同一 ID 只在一个分区。 | E3、E4 |
| UI-16 | 建立 runtime A global subscription，重连后切换 runtime B，并从 A/B 注入事件。 | 每个 runtime 只有一个有效订阅；A 被 dispose；仅 B 的 event 更新 store。 | E6 |
| UI-17 | authoritative active list 不含某 ID，但 directory live store 仍保留它；分别模拟 archived、deleted 和 optimistic create。 | archived/deleted 不被 live append 复活；只有 shadow map 中的 optimistic create 临时显示。 | E5、E7 |
| UI-18 | 同一 fixture 进入 Sidebar、Recent、worktree、retention、Tray、mobile、switcher 和 command palette。 | 所有消费方调用共享 helper 并得到一致 rank/display time；管理操作不改变最近活动。 | E8 |
| UI-19 | 输入缺失 parent、自引用、循环和按 A1 判定非法的跨 cwd parent。 | 构树不递归溢出；按 contract 作为 root/孤儿安全降级并保留诊断信息。 | E9 |
| UI-20 | 分别返回 `-32601`、权限、数据库、invalid cursor、snapshot expired 和 transient transport error。 | 只有 `-32601` 回退 alias；snapshot expired 重启 snapshot；仅 transient error 重试；其他错误保留 cache 并向上报告。 | E2、E3 |
| UI-21 | 旧 cache 有 A/B/C；成功 directory scope snapshot 只返回 A，随后让同 scope 请求失败。 | 首次 commit 删除同 scope 的 B、保留 scope 外 C；失败请求不删除 A/C；optimistic shadow 不因 absence 被清理。 | E3–E5、E7 |
| UI-22 | 复现“active snapshot 前 archive、archived snapshot 前 restore”的双缺席时序，再改用新 full load。 | 新客户端只发送一次 `archived="all"` owner-wide traversal；session 保持可见且唯一，不再并行请求两个 membership snapshot。 | E3、E4 |
| UI-23 | 对 archived session 执行 metadata patch，并让标准 `session/list` 不返回该 ID。 | 不调用 active-only pre-read；private update 成功后按 response revision 更新 archived record，失败不清除或移动它。 | E13 |
| UI-24 | parent archive 成功而 child RPC 失败，随后刷新 `all` snapshot。 | active child 作为 effective root 保持可见并暴露 partial failure；parent restore 后 child 重新挂回且不重复。 | E4、E9、E10 |
| UI-25 | client 有多个 tombstones，完成 directory snapshot、旧 owner-wide snapshot和覆盖版本的新 owner-wide snapshot。 | 前两者不清 tombstone；仅 `snapshotVersion >= indexVersion` 的成功 owner-wide commit 清理对应项。 | E5 |
| UI-26 | fake clock 驱动 visible/hidden、online/offline、reconnect/runtime switch 和多个 consumer mount。 | visible+online 每 60 秒仅一个 singleflight load；hidden/offline 零 timer 请求；恢复按 last-success 规则立即或延后；无 Sidebar/Tray 重复 timer。 | E14 |
| UI-27 | list 返回 capacity exceeded、invalid cursor、snapshot expired 和 transport error。 | capacity/invalid 不 fallback、不 retry、不清 cache；expired 有限重启；仅 transient transport retry。 | E2、E3 |
| UI-28 | 第二页返回不同 `snapshotVersion`，同时插入更高 indexVersion events。 | 整个 accumulator 作为 protocol violation 丢弃并保留 cache/overlay；重新开始后按 revision/indexVersion 原子收敛。 | E3–E5 |

### 11.5 兼容性、端到端与性能测试

| ID | 场景 | 核心断言 | 关联任务 |
| --- | --- | --- | --- |
| COMP-01 | 新 Desk + capability 明确仅支持 `list-global` 的旧 anureo。 | 直接使用旧方法，active/archived 全量加载可用；不先试新方法。 | F4 |
| COMP-02 | 仍调用 `list-global` 的旧 Desk + 同时支持两方法的新 anureo。 | alias 返回旧 Desk 可解析的 boolean-filter/`updatedAt` projection，archive/update 行为不回归；数据来自新 query core。 | F4、G1 |
| COMP-03 | 新 Desk + 新 anureo。 | 首选 `list`；正常请求不触发 alias；标准 ACP list 仍可由其他 client 使用。 | F4、G2 |
| E2E-01 | 建立 root/child、产生 child activity、跨页加载、重启并继续接收事件。 | 父子关系、tree order、revision 和 snapshot 在重启前后语义一致。 | F2 |
| E2E-02 | owner-wide 分页中制造 activity/archive/restore 并令 snapshot 过期。 | 当前 snapshot 的集合/字段/顺序稳定；Desk 恢复无空闪、双缺席、重复或旧记录复活。 | F2、F3 |
| E2E-03 | 使用独立 anureo binary 运行无模型 initialize capability、`session/new`、canonical list、cursor continuation、legacy alias、metrics、标准 list、archive 和 delete wire 流程。 | 新 peer 声明 `list`/`list-global`；不依赖 `session/prompt` 或模型解析；两页 snapshot 的 `snapshotVersion` 固定且无重复/遗漏；legacy projection 不泄漏新 revision/indexVersion；metrics 能导出 alias 调用计数；archive 后进入 archived projection，delete 返回 tombstone 且 authoritative list 不再返回 target。旧 peer 分支只断言 legacy fallback 与标准 list。 | F2、F4 |
| PERF-01 | 单 owner/cwd 下准备 10k sessions、每条 1 KiB metadata，以 page size 500 遍历 20 页并连续运行 20 次。 | snapshot SQL statements ≤3、后续页 0 SQL、构建 p95 ≤500 ms、loopback traversal p95 ≤2 s、Desk merge p95 ≤100 ms、retained snapshot accounting ≤64 MiB/owner；记录 runner CPU/RAM/OS。 | F5 |
| PERF-02 | 模拟一次包含大量 token delta 的长响应。 | SessionIndex 写入按语义 activity 边界合并，不逐 token 写 SQLite；revision 不被 delta 放大。 | C2、F5 |
| PERF-03 | 对非当前 session 高频注入状态/消息事件。 | sidebar 排序只在约定 lifecycle edge 更新，不发生每个 delta 的全列表重排或宽订阅渲染。 | E7–E9、F5 |

### 11.6 执行门槛与用例维护

- 每个修复型用例必须先在旧实现上稳定失败，再由对应实现任务转绿；禁止依赖真实时间、随机 ID 或不受控并发。
- Snapshot、排序和 revision 用例使用 table-driven fixtures，并在失败信息中输出 owner、filter、cursor page、预期/实际 ID 序列。
- anureo 合并前运行相关 package 的 scoped `cargo nextest` 与 `cargo clippy`；Desk 合并前运行相关 UI tests、package-level type-check/lint。新增/改名测试模块或 export 时再运行 `dead-code`。
- F1 至少覆盖 TFX-01～03、R-01～R-22；F2 覆盖 API-01～17 与 E2E-01～02；F3 覆盖 TFX-04/06、UI-01～28；F4 覆盖 COMP-01～03；F5 覆盖 TFX-05 与 PERF-01～03。
- `cargo test -p anureo-acp --test e2e_session_list -- --nocapture` 必须在独立 binary 上通过；该测试不得依赖模型、默认 3030 端口或正在运行的开发服务器。
- anureo alias 删除前，COMP-01～03 必须全部通过。G3 的 alias 删除 commit 将 COMP-02 从“成功兼容”改为“新 anureo capability 不声明 alias，直接调用旧方法返回 `-32601`”的负向 fixture；COMP-01 与 Desk fallback fixture 持续通过，直到产品另行提高最低支持 anureo 版本。

## 12. 完成定义

- `_anureo.dev/session/list` 的排序使用完整总序；Desk full load 的 active/archived 来自同一 owner-wide `all` snapshot，集合、字段和顺序均冻结。
- 不另建版本化的新方法；`list-global` 仅作为短期 alias，最终删除。
- owner/record versions 使用 JSON safe integer checked increment；空 owner 的 snapshotVersion 为 0，record revision/indexVersion 从 1 开始；同 transaction 共用 indexVersion，no-op 不增加版本，系统时钟回拨不使字段时间倒退。
- snapshot 固定为同一 SQLite read transaction materialize 的 immutable projection，TTL 5 分钟、4 个/64 MiB 每 owner、256 MiB 每进程，并使用 HMAC-SHA256 opaque cursor；容量、非法 cursor、过期和重启有互不混淆的错误。
- 页面边界、相同时间戳和分页过程中的 session 活动均不造成重复或漏项。
- `activity_at` 不被标题、归档、恢复或 lifecycle 操作修改。
- global wire 使用真实 `{topic,event:{type,properties}}` envelope；created/updated 的 `properties.info` 与 list item 共用完整 `SessionIndexRecord` schema/parser，唯一 ID 为 `sessionId`；deleted event 使用冻结的 revision tombstone envelope；标准内容 `session/update` 不再产生同名 minimal global event。
- Desk production global subscription 已接通；旧 runtime event 的隔离由 teardown/generation guard 实现，现有单元测试覆盖 event-source teardown，但真实 runtime switch/reconnect 集成证据仍待补齐。
- global event 允许 fire-and-forget 丢失但有 drop 指标；Desk 在 bootstrap/reconnect/runtime switch 立即 resync，visible+online 时由唯一 scheduler 每 60 秒 singleflight resync，确保最终收敛。
- global index 决定 membership 和 versioned 字段，directory live store 不会复活 archived/deleted session。
- 成功 snapshot 对绑定 scope 执行 authoritative absence；失败 scope 保留旧 cache。snapshot/event/optimistic mutation 经同一个 normalized revision merge，fetch failure 不会被当作空成功。
- `tree_activity_at` 表示当前可见树而非历史 high-water；archive/delete/reparent 可使其下降，并对所有变化祖先递增 revision、发布事件。
- Sidebar、Recent、worktree、retention、Tray、mobile 与 switcher 使用同一个 activity/rank 语义。
- `session/new` title/parent/metadata wire、同 cwd parent 约束与 canonical response contract 已冻结；message-bounded fork 明确退出本次范围，Desk 不再忽略 message ID，assistant-plan 动作按真实的新建+发送 prompt 语义命名。
- archive/update/delete/create response 的 target、changed ancestors、tombstone 和 indexVersion 形状固定；event publish 失败不改变已提交 response，重复 delete 跨重启返回同一持久化 tombstone。
- metadata FK 固定 `ON DELETE CASCADE`、所有连接启用 foreign keys；migration、orphan cleanup 与 `foreign_key_check` 全部原子。
- 标准 ACP `session/list` 与全局列表从同一 session index 派生可见记录和基础时间语义；标准方法一次返回当前 owner/cwd 的全部 active records，`nextCursor=null`。
- anureo 与 Desk 都有覆盖 snapshot、层级、活动传播与乱序事件的自动化测试。
- F5 的 10k/1 KiB/20-run 性能预算以及 G3 的 14 天零调用或 2 个 stable release 门槛均达标并留存运行环境与结果。

## 13. 当前实现进度（2026-08-22）

本轮已落地：

- `acp_sessions` 的 SessionIndex 扩展列、owner version/tombstone 表、metadata `ON DELETE CASCADE` migration 和 owner/cwd canonical query。
- `_anureo.dev/session/list` handler：`all/active/archived` filter、immutable in-memory snapshot、5 分钟 TTL、owner 内最多 4 个 snapshot、HMAC-SHA256 opaque cursor、`snapshotVersion` 与 `revision/indexVersion` projection。
- 标准 ACP `session/list` 改为从 SessionIndex 派生 active projection，并保持单页 `nextCursor=null`。
- Desk `acpApi.session.listIndex`、仅 `method_not_found` 的旧 anureo fallback、canonical index 字段映射，以及 full load 的单 owner-wide `all` traversal。
- global event bus 的 drop counter；title/metadata/archive/lifecycle mutation 已开始写入 record revision 与 owner indexVersion。
- prompt 接受边界已接入一次性 `activity_at` mutation，并在同一事务内向 active ancestors 传播 `tree_activity_at`；active projection 已改为稳定的树前序（根/子节点均按 `tree_activity_at DESC, session_id ASC`）。
- `session/new` 已在创建前校验 metadata、同 cwd 且 active 的 parent，避免明显的无效 parent 创建。
- metadata/title 合并更新已改为单事务、单 `indexVersion`；archive/restore 会重算受影响祖先并在 response/event 中返回 canonical projection；`session/new`/标准 delete 已接入 canonical created/deleted global event。
- snapshot 增加 owner 64 MiB、process 256 MiB 的估算配额；Desk 增加 tombstone shadow，阻止旧 snapshot/event 复活已删除 session；SyncProvider 增加唯一 60 秒、visible+online 条件的 singleflight resync。
- snapshot quota 现在按 canonical `SessionIndexRecord` JSON 序列化长度加 vector/allocator 保守开销计量；schema migration 对共享默认数据库的并发 `ADD COLUMN` 竞态保持幂等。
- 新写入的 create/activity/metadata/state/delete 时间统一使用 UTC、固定 6 位微秒；同一字段已有值时使用微秒级单调递增候选，避免时钟回拨造成排序倒退。
- Desk `session/new` 在传入 title/parent/metadata 时直接携带 `_meta["anureo.dev"]`，并消费 canonical create response；archive/update response 的 `affectedSessions` 已继续合并到 global store。
- 标准 `session/delete` 在首次成功后解绑连接；重试会先检查 durable tombstone 并继续走幂等删除路径，不再因 binding 已移除而错误返回 `-32011`。
- Desk canonical timestamp parser 不再用 `Date.now()` 填充缺失服务字段；无效时间使用确定性 fallback，避免坏数据伪装成最新会话。
- Desk mutation adapter 已保留旧调用方的 target return 形状，同时透传 `affectedSessions`/`indexVersion`，因此 archive/update 的 ancestor merge 不要求 UI 直接理解 ACP extension envelope。
- 最终质量门槛当前已通过：anureo `cargo clippy -p anureo-acp --lib -- -D warnings` 与 Desk `bun run --cwd packages/ui lint` 均无新增问题。
- Desk `session/new` wire builder 已独立成契约模块并覆盖“完整 `_meta` / 空扩展 envelope”两种用例。
- Desk `listIndex` 现在优先读取 initialize 返回的 `_meta["anureo.dev"].session.methods`；明确不支持 `list` 的 peer 直接走 `list-global` alias，未声明扩展能力的 legacy peer 才保留 `-32601` probe fallback。
- owner-wide `all` snapshot 的 `snapshotVersion` 已贯穿到 Desk store；成功覆盖 tombstone 版本且确认 ID 缺席时，tombstone shadow 会被回收，失败/目录级请求不会误清理。
- Desk 已有 tombstone reconciliation 回归测试，验证覆盖版本的 owner-wide snapshot 清理 shadow 且不复活已删除记录。
- archive/restore 的 repository API 现在在写事务提交前 materialize changed index records，Desk extension response/event 不再依赖提交后的二次全量查询。
- Desk 新增 ACP global topic refcount 回归测试，覆盖 session 与 notification 并发订阅、最后一个 consumer 释放才发送 unsubscribe，以及订阅失败后的 refcount rollback。
- Desk archive action 现在先做 target-only optimistic move；请求失败只恢复仍带本次 optimistic timestamp 的记录，不会覆盖飞行期间已经到达的 canonical event。
- Desk create action 现在把未注册 ACP 的临时 record 放入 global shadow；canonical `session/new` response 到达后替换，失败时移除，避免假 session 进入 ACP native store。
- runtime endpoint/transport switch 现在会关闭并清空 shared ACP runtime、取消旧 pending promise 的复用；generation guard 防止旧 endpoint 的迟到连接重新注册，且保留 global session store 的切换清理。
- global topic refcount 与串行订阅队列现在按 ACP runtime 隔离；旧 runtime 的延迟 release 不会影响新 runtime 的 session/notification subscription。
- anureo `_anureo.dev/session/update` response 现在显式返回 `affectedSessions: []`，与 create/archive mutation envelope 保持固定字段形状。
- `SessionListHandler` 增加 `with_clock` 注入点，snapshot TTL/quota 测试不再必须依赖真实墙上时钟；生产默认仍使用 `SystemTime::now`。
- snapshot admission 在 byte quota 判断前先淘汰 owner 最旧的第 5 个候选，避免“本可容纳但被旧 4 个快照预占空间误拒绝”；新增 eviction 回归测试。
- owner `indexVersion` 分配现在使用 checked increment，并在超过 JSON safe integer 上限时拒绝 mutation；新增 exhaustion 回归测试。
- owner `indexVersion` 与 session/tombstone `revision` 均受 JSON safe integer 上限保护；repository 使用 checked increment，SQLite trigger 阻止直接写入越界 revision，并新增 revision exhaustion 回归测试。
- Desk metadata patch 不再通过 active-only `session.get` 预读；优先使用 global/index cache，因此 archived session 的 metadata update 可直接按 private update response 收敛；新增 archived patch 与 target/ancestor archive merge 回归测试。
- anureo repository 的 10k session fixture 现在包含每条 1 KiB metadata、20 次 canonical SessionIndex full-read 计时和 p95 输出；旧 `updated_at` keyset benchmark 已随旁路删除。canonical snapshot 分页由 extension fixture 覆盖（固定 `snapshotVersion`、opaque cursor、无重复/遗漏），snapshot quota 已按 canonical wire JSON + 64 bytes/record + 256 bytes/snapshot 精确计费，并有回归测试。完整 CPU/RAM 与跨运行时兼容矩阵仍需专门基准环境。
- anureo 新增真实 `anureoAcpAgent` + `SessionListHandler` wire-level archive 测试，验证 target/ancestor canonical response 与共享 `indexVersion`；Desk 新增 global event source teardown 测试，验证关闭后不再消费旧事件。Desk `session/new` action 现在也会消费 `_meta["anureo.dev"].affectedSessions`，并以回归测试验证 create 的 target/ancestor 合并与 parent 字段保留。
- `session/new` 的 anureo 持久化现在通过单个 repository transaction 写入 target、parent、title 和 metadata；target 与可见 ancestors 共享同一个 `indexVersion`，wire fixture 覆盖 title/metadata 与 ancestor response 的一致性。
- 标准 ACP `session/new` 成功路径现在会先发布 target `session.created`，再按 response 的 nearest-ancestor-first `affectedSessions` 发布完整 `session.updated` ancestor events；event info 补齐 `cwd`、`createdAt` 和 `archivedAt` 等 SessionIndex 字段。
- 新增 `session/update` wire 回归：title/metadata mutation 明确是 target-only，不传播 tree activity，也固定返回 `affectedSessions: []`；测试同时读取 parent 前后 revision/indexVersion，证明 ancestor 不会被无关 metadata/title 更新误 bump。Desk action 对该字段保持兼容消费。
- Desk `updateSessionTitle` 新增 canonical response merge 回归：target title、revision/indexVersion 与服务端返回的 affected records 都会进入 global store；即使 anureo 当前 update contract 返回空 affected list，未来兼容扩展也不会丢 ancestor record。
- 新增真实 `anureoAcpAgent + SessionListHandler` 分页 loopback 回归：5 条 active sessions 以 limit=2 跨页读取，验证 `snapshotVersion` 固定、cursor 可继续、无重复且无遗漏。
- Desk global store 的 runtime-switch generation guard 现在同时保护 singleflight 引用：旧 runtime 的迟到 promise 不会清空新 runtime 的 in-flight load；新增回归测试覆盖旧/新响应交错。
- runtime endpoint switching 测试现在验证实际 `anureo:runtime-endpoint-changed` 事件 detail，确保 remount 消费者收到新 endpoint/runtime key，而不是只验证 URL getter。
- 新增真实浏览器级 BDD 场景 `e2e/features/web/runtime-switch.feature`：在 real ACP 环境中先确认 sidebar 有 active session，再注入 runtime endpoint changed 事件，等待新的 `_anureo.dev/session/list-global`（兼容旧 alias/标准 list）请求，并确认原 session 在 remount 后仍可见；随后通过 UI archive 同一 session，等待 `_anureo.dev/global/update` 的 `session.updated` wire event，并确认该 session 离开 active list。该场景已通过 `E2E_BASE_URL=http://127.0.0.1:5180 E2E_REAL_ACP=1` 验证。
- Desk `listIndex` fallback 现在使用集中错误分类和纯 capability 判定：只有 capability 明确缺少 canonical `list` 且存在其他 session methods 时直调旧 alias；空 capability 保持未知并先 probe canonical method。只有 ACP `-32601 method_not_found`（或无 code 的 legacy 文本错误）允许调用 `list-global`；`-32001 capability_not_supported`、`-32602` 和其他业务错误不会降级。兼容性矩阵已覆盖 JSON-RPC code、legacy 文本、unknown-method 文本、capability/permission、invalid params、存储错误和连接错误，并验证 legacy active+archived 合并保持顺序、active 优先去重且明确不可分页。
- anureo `SessionListHandler` 新增 capability wire 回归，明确兼容窗口同时声明 canonical `list` 与 legacy `list-global`，并保留 archive/update/delete；这与 Desk 的 capability 直选和旧 alias fallback 矩阵保持一致。
- 新增 COMP-02 wire 回归：旧 `list-global` 请求从同一 SessionIndex/query core 读取，并只返回 legacy `sessionId/cwd/title/createdAt/updatedAt/archivedAt/metadata` projection，不泄漏 `revision/indexVersion` 等新字段。
- Desk 新增 COMP-01 fallback 集成回归：旧 anureo 仅提供 `list-global` 时，`listIndexLegacyFallback({ archived: "all" })` 只发 active/archived 两个旧请求，合并时 active 优先去重，并返回不可分页的 compatibility response。
- Desk 新增三组合 compatibility behavior fixture：纯路由规划测试覆盖新 Desk+新 anureo 走 canonical `list`、新 Desk+旧 anureo 按 capability 走两个 legacy 分区、未知 capability 在 canonical `-32601` 后才 fallback；真实 request method 序列仍需跨版本运行时联调验证。
- Desk 新增 10k rich snapshot merge 基准：每条 session 带 1 KiB metadata，连续应用 20 次 full snapshot，并输出 wall-clock p95、heap delta、`process.cpuUsage()` CPU 时间/单核占用比及 runner platform/arch/CPU 数/RAM；strict 模式同时强制 p95 ≤500ms 与 heap delta ≤64MiB。当前 Windows 重复实测 p95 约 35ms、heap delta 约 26 MiB，预算通过；CPU 时间随机器负载波动，不作为跨 runner 的硬阈值。
- Desk 的直接 `session/new` wire builder 现在始终携带标准必填 `mcpServers: []`；真实 ACP BDD 已验证 canonical session ID、`session/prompt` request/response、sidebar duplicate-safe assertion、reload 后**新发出的** canonical `session/load`、对应 history batch、UI 消息恢复，以及服务端 history `totalMessages` 持久化（场景已通过）。anureo `session/load` 已加入最多 500ms 的 checkpoint 可见性重试，用于覆盖 prompt 完成与新连接读取之间的 SQLite 可见性窗口；Desk reload/deep-link 路径现在会强制权威 load，即使本地持久化消息看起来已可渲染。
- E2E WebSocket capture 增加 frame cursor，等待器不会再把 reload 前的缓存帧当成新请求；ACP history paging/store 回归测试 11 项通过。
- ACP 全量 lib 测试当时为 594 通过；并发初始化场景的 `archived_at` 迁移已改为幂等，SQLite repository connection 增加 30 秒 busy timeout 以吸收并发写入窗口，`cargo clippy -p anureo-acp --lib -- -D warnings` 通过。
- 并发回归曾暴露 `session/new` 在 checkpoint/config 连接重叠时的 `database is locked`；现在 `SessionConfigStore` 也使用 30 秒 busy timeout，SessionIndex atomic create 对 SQLite busy/locked 做 bounded exponential 8-attempt backoff，并有 8-worker × 4-session 压力回归。该阶段修复后全量 594 tests 在并行执行下通过。
- Agent 初始化现在会在打开 config/repository 前幂等重建 `ANUREO_HOME` 数据库父目录，覆盖临时 home 被清理后的 startup race；archive mutation 也复用同一 SQLite retry helper。该阶段重复全量运行仍为 594/594 通过。
- 已删除 `anureoAcpAgent::list_sessions_from_db`、`SessionInfo`/旧 checkpoint 聚合类型和标准 `list_sessions` 旁路入口；标准 `session/list` 现在只有 `list_sessions_for_owner` 这条 SessionIndex 生产路径。`protocol.rs` 也已移除“从 checkpoints 聚合/未实现分页”的过期说明。该阶段串行全量为 594/594 通过；当前新增删除回归后为 596/596。并行全量测试仍可能受外部临时 `ANUREO_HOME` 清理竞态影响，验证基线以隔离环境或串行命令为准。
- `list-global` alias 已改为直接调用 canonical `list` 的 immutable snapshot/cursor core，再做 `sessionId/cwd/title/createdAt/updatedAt/archivedAt/metadata` legacy projection；不再调用 timestamp keyset 或逐条 metadata 查询。新增跨页回归验证 alias 返回 canonical opaque cursor、第二页无重复且不泄漏 `revision/indexVersion`。
- 标准 ACP `session/delete` 首次成功删除现在发布带完整 tombstone 的 `session.deleted` global event；解绑后的幂等重试不重复发布。extension delete 同样先确认 target 仍存在，重复 tombstone 只返回原结果，不重复广播。
- `delete_all_indexed` 已成为删除的原子 repository mutation：同一事务写入 durable tombstone、删除 target 及其附属数据、按剩余可见后代重算所有受影响 ancestor 的 `tree_activity_at`，并以同一 `indexVersion` 返回 tombstone 与 nearest-ancestor-first `affectedAncestors`。标准 ACP 与 extension delete response 都透传这些 canonical records，且为每个变更 ancestor 发布 `session.updated`；新增删除祖先下降、response projection 和事件回归测试。
- Desk `deleteSession` 现在在记录 target tombstone 后立即合并标准 `session/delete` response 中的 `affectedSessions`，不会等待 global event 或下一次 snapshot 才修正 ancestor；新增 target tombstone + ancestor revision/indexVersion 回归测试。
- ACP lib 串行全量测试当前为 602/602 通过；`cargo check -p anureo-acp --lib` 与 `cargo clippy -p anureo-acp --lib -- -D warnings` 通过。跨平台 CPU/RAM 采样、完整兼容矩阵和多版本 Desk 联调仍是最终发布门槛。
- 源码事实源审计已完成：生产 `session/list`、canonical private list、legacy alias 均落到 SessionIndex；旧 checkpoint 聚合入口不存在。仅保留用于 ACP 重启恢复的 `SessionRepository::list_for_restore`，并明确标注其不是对外列表 projection。串行全量仍为 602/602。
- 删除绑定边界已核对：标准 ACP 只有当前连接绑定的 live session 才能执行首次 delete；未绑定的 live session 返回 `-32011`，防止跨连接越权。连接解绑后的同 owner 重试仅在 durable tombstone 已存在时进入幂等路径，并不会重复发布 `session.deleted`。
- alias 观测已接入 `SessionListHandler::legacy_alias_call_count`、`AcpRuntimeMetricsSnapshot::legacy_session_list_alias_calls`，并通过受 principal 保护的 `_anureo.dev/session-metrics/status` 提供只读导出；计数只统计 `list-global`，不统计 canonical `list`。14 天生产样本仍未积累，发布门槛仍保持未关闭。
- Desk `acpApi.sessionMetrics.status()` 已作为 typed read adapter 接入 `_anureo.dev/session-metrics/status`；`parseAcpSessionMetricsResponse` 拒绝缺失、负数、非整数、超出 JavaScript safe-integer 范围和错误类型，避免观测失败被误判为零调用。对应兼容测试 8/8、UI type-check 与 lint 已通过。
- 跨平台性能验收已接入 Desk CI：`.github/workflows/test.yml` 的 `session-index-performance` 矩阵在 `ubuntu-latest`、`macos-15`、`windows-2022` 运行同一 `ANUREO_DESK_STRICT_PERF=1` 20-run benchmark，并记录各 runner 的 platform/arch/CPU/RAM；本地仍只有 Windows 实测，CI 结果作为 Linux/macOS 发布证据来源。
- ACP wire-level E2E harness 新增 `ANUREO_ACP_BINARY` 覆盖，可在不改测试代码的情况下针对独立构建的 anureo binary 重跑协议场景；真实新旧 Desk/anureo 组合仍需在发布环境使用该入口完成并留档。
- 新增 `scripts/run-session-list-compat.ps1`，为新/旧 anureo binary 顺序运行 ACP E2E 并保存 manifest/log；旧 binary 分支明确验证 legacy fallback，不强求 canonical list；它解决“如何重复采样”的问题，但不替代真实 Desk 版本组合的发布验收。
- 新增 `e2e_session_list` 无模型 wire 回归与独立 ACP 端口：验证新 peer capability 中的 `list/list-global`、canonical cursor 续页可只携带 `cursor/limit`、legacy projection、标准 list、archive projection 和 delete tombstone/authoritative absence，不再因本地默认 3030 服务或模型解析失败产生假阴性；兼容 runner 可用 `ANUREO_ACP_BINARY` 重跑该目标，旧 peer 则走明确的 legacy 断言分支。
- `.github/workflows/session-list-compat.yml` 新增 Ubuntu/macOS/Windows wire 门禁：各 runner 构建独立 `cli` binary，运行无模型 `e2e_session_list`，校验 manifest 的模式、测试数、退出码、binary SHA-256、文件大小和 OS/架构字段，并上传 `manifest.json` 与逐次日志；它提供跨平台 CI 证据，但不等价于新旧 Desk/anureo 版本矩阵。
- runner 默认 target 已切换为无模型的 `e2e_session_list`（`e2e` Cargo target 仅包含 harness 根、不会执行测试）；完整 `e2e_mega` 仍可显式运行，但在 session/prompt 前可能受既有模型解析环境影响，不将该失败归因于 session-list 改造。
- Desk `acp-event-source` 队列现在按 `sessionId/id` coalesce 同一 session 的待处理事件，保留跨 session 的原始相对顺序；无 session key 的事件队列上限为 256，超限丢弃由 owner-wide 60 秒 authoritative resync 修复。事件源关闭时同时清空 queue/index，避免 runtime switch 后迟到事件泄漏到新 runtime。
- session-list 组合回归已固化为 `packages/ui` 的 `bun run test:session`（内部使用 Bun `--isolate`）；当前 12 个相关文件共 65/65 通过。普通多文件运行会受到 Bun 全局 module mock 污染，不能作为该组合门禁。
- `.github/workflows/test.yml` 新增独立 `session-list-ui` job，在 Ubuntu 上执行同一个 `bun run test:session`；它与全量 `ui-tests` 分离，确保 session-list 门禁不会被其他 UI 测试环境故障掩盖。
- 性能 benchmark 在设置 `ANUREO_DESK_PERF_OUTPUT` 时会写出结构化 JSON；CI 按 runner OS 上传 `session-index-performance-<os>` artifact，保存 p95、heap、CPU、平台、架构、CPU 数和内存，避免只依赖截断后的 console log。
- 最新验证补充：Desk 全仓 `bun run type-check` 与 `bun run lint` 通过；`packages/ui` 的 `bun run test:session` 为 65/65（12 个测试文件，183 个 expect）；Windows strict 20-run rich merge benchmark 为 p95 33.66ms、heap delta 25.17MiB（≤500ms/64MiB）。性能测试使用 Node-compatible `fs/promises` 写 JSON，并在输出路径带父目录时创建目录，避免依赖 Bun ambient types 或 `mkdir(".")` 的运行时差异。anureo 使用独立 `target/session-list-build` binary（避免占用中的默认 `target/debug/anureo.exe`）运行 `e2e_session_list` canonical 与 `ANUREO_SESSION_LIST_EXPECT_LEGACY=1` 两个分支，均为 1/1 通过；旧默认 binary 因文件锁不能作为当前源码证据。
- 兼容 runner 本地复验：同一独立 binary 分别作为 `-NewanureoBinary` 与 `-OldanureoBinary` 运行，manifest 中 canonical/legacy 两条记录均为 `exitCode=0`、`testCount=1`；这只是 runner/断言路径回归，不替代真实历史 binary 的跨版本证据。
- Desk 分页恢复已补齐：`listGlobalSessionPagesAcp` 与 `listGlobalSessionIndex` 对 `-32004`/`snapshot_expired` 最多重启两次，并从第一页创建新 cursor；重启前清空 partial accumulator，渐进式调用方通过 `onReset` 清理已展示页。达到上限或遇到 invalid cursor/权限/存储错误则原样抛出，不返回空成功、不触发 legacy fallback；新增普通分页和 owner-wide index 回归测试。
- UI-28 协议一致性已补齐：分页过程中若相邻页 `snapshotVersion` 改变，Desk 不再用 `Math.max` 混合不同 snapshot，而是丢弃整份 accumulator、清理 cursor 并通过 `onReset` 通知渐进式展示层；随后有限次从第一页重载。连续版本漂移超过上限会抛错并由 store 保留旧 cache。对应版本漂移、过期上限和 cursor 重启测试已通过。
- mutation/list wire guard 已补齐：`parseAcpGlobalSessionDescriptor` 统一拒绝缺失、空白或非字符串 `sessionId`；list、archive/update response 以及 create/delete/update 的 `affectedSessions` merge 都先经过 guard，malformed JSON 不会写入空 ID session。ACP API 边界还会将非数组/非法 fan-out 归一化为空或过滤掉，standard/authoritative list envelope malformed 时显式失败，tombstone store 也会拒绝非法 counter；Desk session 门禁当前 65/65 通过。
- 同一 guard 还拒绝负数、浮点、非安全整数的 `revision/indexVersion`；`listIndex` 对 `snapshotVersion` 使用同样的 JSON-safe 非负整数校验，异常值降为无版本（0）而不会参与 tombstone/freshness 判定。
- UI-08 overlay 收敛已补齐：`useGlobalSessionsStore.loadSessions` 在原子提交时重新读取最新 Zustand state；对 snapshot 中同 ID 记录按 `indexVersion`、`revision`、更新时间选择更新者，并保留加载期间产生的 optimistic/newer live records，避免异步 snapshot 覆盖事件。新增 deferred owner-snapshot 回归测试。
- active/archived 分区也在同一 reconciliation 中处理：若 live archive/restore 在 snapshot 期间改变归属，live partition 优先且同一 ID 只进入一个分区；snapshot 外的 optimistic/newer record 按现有 live 顺序追加，不改变 SessionIndex 树顺序。对应跨分区竞态测试已加入 session gate（65/65）。
- UI-26 调度去重已补齐：`useTraySync` 只在 mount 时执行 seed，移除原有 45 秒全量刷新 timer；owner-wide 全量刷新统一由 `SyncProvider` 的 visible+online 60 秒 singleflight scheduler 负责，避免 Sidebar/Tray 双定时器造成重复请求。
- UI-26 唤醒门控已补齐：`visibilitychange`/`online` 只在从未成功或距上次成功至少 60 秒时触发立即 resync；失败不更新时间戳，hidden/offline 期间不发请求。
- 排序展示的缺失时间 fallback 已统一为确定性 epoch：`SessionSwitcherDropdown` 与 `SessionNodeItem` 不再用 `Date.now()` 把 malformed/legacy session 伪装成最近活动。
- SessionIndex 排序事实源已统一：`session-activity.ts` 对 root 使用 `treeActivityAt`、对 child 使用 `activityAt`，再回退到 `time.updated/created`；global store、Sidebar、Recent 和 Tray 共用该 helper。
- 自动清理与 Sidebar 排序 memo 也已接入同一 helper：保护最近 session 使用 tree/child activity，tree activity 改变会刷新 `sessionOrderIndex`，不会沿用只看 `time.updated` 的旧缓存。
- 其余排序/展示路径也已收敛：Switcher、SessionNode、worktree grouping、agent groups 的时间显示与聚合均使用 `getSessionActivityTimestamp`，agent group 缺失时间不再回退当前时间。
- retention 已迁移到 ACP `archiveSession`/`deleteSession` facade，复用 tombstone、affectedSessions、乐观回滚和统一错误语义；不会再直接调用 legacy `anureoAgentClient`，且明确处理 facade 返回 `false` 的 partial failure。
- 标准 ACP `session/list` 的 `rawSessionList` 也已拒绝 malformed envelope/全量非法 record，目录级读取不会再把协议失败当成空成功。
- 分页 adapter 的渐进式 `onPage` 回调现在复用累加器的 ID 去重结果；即使异常页面重复 `sessionId`，渐进式 UI 也不会先显示重复行。新增回归后 session gate 为 65/65。
- Sidebar、Switcher 与 mobile session bucket 现在共用 defensive session tree builder；self-parent、cycle 和缺失 parent 会被安全截断或提升为 root，级联 archive/delete 也共用 visited-set descendant helper；新增 4 个 cycle/orphan 回归后门禁为 65/65。
- ACP global event source 现在按请求的 topic allowlist 过滤共享 runtime 回调，避免 notification topic 混入 session reducer；新增 topic-isolation 回归测试。
- Web notification consumer 也按 `notification` topic 过滤共享 callback，避免 session event 被送入通知 payload parser；两个 ACP global consumer 的 topic 隔离规则保持一致。
- anureo 生产源码旁路审计确认 `acp_sessions` 的写入只存在于 `session_repository.rs` 的 repository/migration/test 路径；`cargo test -p anureo-acp --lib -- --test-threads=1` 当前 602/602 通过。
- `docs/design/acp-subagent-contract.md` 已标记为 2026-08-19 历史草案，并明确旧 `parent_id` 方案不能直接使用；当前实现统一使用 `parent_session_id` 与本设计/37 号规范。
- Sidebar 的 archive/delete 级联收集也增加 visited-set 防护；即使 legacy 数据出现 self-parent 或循环 parent 链，操作路径也不会无限递归。
- `SyncProvider` 已接入生产 ACP global session subscription：使用共享 topic refcount、事件按 session coalesce、runtime teardown 自动关闭，并由 visible+online 的 owner-wide resync 做丢事件恢复；当前仍缺少真实 runtime switch/reconnect 的集成测试证据。
- ACP bootstrap 现在复用严格标准 `session/list` parser；malformed envelope/全量非法 records 不会伪装成空成功，且所有 bootstrap directory 都失败时会抛出错误，只有部分 directory 失败才保留可见分区并记录失败。
- `session/load` cwd mismatch 的 fallback lookup 也复用同一 strict parser，不会因 malformed standard list 把“找不到 session”误判为正常空结果。
- 新增 `legacy_schema_migration_is_idempotent_and_removes_orphan_metadata` repository 回归：用旧版 `acp_sessions/acp_session_data` schema 构造有效记录与 orphan metadata，验证 SessionIndex 列、owner state/tombstone 表、cascade FK、`foreign_key_check`、orphan 清理和第二次初始化幂等；发布签收中的真实数据库备份、失败注入回滚和生产 14 天 alias 指标仍需在目标环境执行。
- `anureoAcpAgent::new_with_db_path` 现在允许 embedded host/test 显式注入 SQLite 文件，并让 session metadata 与 checkpoint history 共用该路径；session-list handler 测试不再修改进程级 `ANUREO_HOME`，因此并行 `cargo test -p anureo-acp --lib` 已稳定通过 602/602。
- 新增 `metadata_foreign_key_migration_rolls_back_when_rebuild_cannot_start`：故意制造 metadata rebuild 目标表冲突，验证 migration 失败后原表和数据完整保留，移除冲突后可重试成功且 `foreign_key_check` 仍为零。
- `ensure_schema` 现在在单个 `BEGIN IMMEDIATE` transaction 中执行基础表、兼容列、索引、owner state/tombstone、trigger 和 metadata FK migration；新增并发初始化回归证明多个 agent 同时启动时会先串行化 DDL，而不是在 deferred transaction 升级阶段返回 `SQLITE_BUSY`。
- schema transaction 在 commit 前强制执行 `PRAGMA foreign_key_check`；任何残留 FK 错误都会触发整体 rollback，而不是让部分迁移后的数据库继续启动。
- migration rollback 回归还会检查失败后不存在 `archived_at`、SessionIndex owner/tombstone 表等半成品 schema，证明回滚覆盖前置 DDL，而不仅是 metadata table rename。
- 新增 `foreign_key_check_failure_aborts_schema_initialization`：已有 cascade FK 但存在 orphan 时，启动被拒绝，前置 `archived_at` 变更回滚，原 orphan 数据保持可审计。
- Desk Agent Manager 的 worktree/group loader 已改用同一 `listGlobalSessionPages` ACP adapter（active projection）；旧 anureo 仍由 adapter 内部显式走 `list-global` fallback，避免 Agent Manager 私自调用 legacy `anureoAgentClient.listSessions` 并形成第二套 membership/order 事实源。
- Review flow 在“元数据关联失败”回滚时也直接调用统一 ACP `deleteSession` action；旧 `anureoAgentClient.deleteSession` 名称不再出现在 session 删除旁路中，仍保留失败告警并继续抛出原始关联错误。
- runtime endpoint switch 在 ACP 模式跳过无意义的 Express `oc_url_token` mint；避免切换/重连路径产生预期的 ACP rejection 和未处理 promise。新增 runtime-switch/review-flow 回归共 7/7 通过。
- 最新 anureo wire 复验使用独立 `target/session-list-build/debug/anureo.exe`：`e2e_session_list` canonical 与 `ANUREO_SESSION_LIST_EXPECT_LEGACY=1` legacy 分支各 1/1 通过；直接使用被锁的默认 `target/debug/anureo.exe` 会在第二页返回 `snapshot_expired`，因此不作为当前源码证据。anureo `cargo test -p anureo-acp --lib` 并行运行当前为 602/602 通过，`cargo clippy -p anureo-acp --lib -- -D warnings` 也通过。
- 兼容 runner manifest 现在为每次 binary 记录 `binarySha256`、`binarySizeBytes` 与 `sameBinaryAsEarlierRun`，CI validator 会拒绝缺失/非法 identity；本地同 binary 双模式复验明确生成相同 hash/重复标记，证明它只是 runner/legacy 分支回归，不会冒充真实历史版本矩阵。

仍需按 §10 继续实现：跨平台 CPU/RAM 采样和完整兼容/性能测试矩阵；无模型 wire 回归已覆盖单 binary 的协议闭环，但 update response 的 target-only/empty-ancestor contract 仍需与多版本 Desk 客户端做跨版本联调。当前代码不应标记为“完成定义”已满足。

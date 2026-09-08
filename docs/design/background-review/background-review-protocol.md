# Background Review ACP 协议设计（v2 修订）

> **状态**：Draft v2 — 待评审
> **日期**：2026-09-06（v1: 2026-08-20）
> **范围**：anureo ACP Background review 的 session metadata、修改明细、内容 diff、查询方法、通知和结果契约
> **相关代码**：`apps/acp/src/review_runner.rs`、`apps/acp/src/agent.rs`、`apps/acp/src/protocol.rs`、`experimental/curator/src/review.rs`、`experimental/curator/src/history.rs`、`agent/tool/tool-experimental/src/file_memory.rs`、`agent/tool/tool-basic/src/skill/manage.rs`
> **架构文档**：[background-review-design.md](./background-review-design.md)
> **交互文档**：anureo Desk `anureo-feat-dev/docs/design/background-review-interaction.md`

---

## 0. v2 修订记录与动机

v1 定义了 `_meta.review` 契约、running 态通知和 `_anureo.dev/review/*` 六方法，但**评审前实现已按简化版上线**，且 v1 本身留有三个缺口。本修订以现状为兼容基线做增量扩展，不引入 breaking change。

| # | 缺口 | 现状证据 | v2 对策 |
|---|---|---|---|
| G1 | **修改明细不入 wire、不持久化**：客户端只能拿到 `memory_count`/`skill_count` 计数；「改了哪条 memory、什么操作」只存在于一条自然语言 `agent_message_chunk` 里，通知发完即失 | `ReviewRecord`（history.rs:11）只有计数列，无 actions；`_meta.review` 无 actions | §3 `actions[]` 内联进 `ReviewState`；§5 持久化 `actions_json`；§6 `review/details` 完整查询 |
| G2 | **无内容级 diff**：`ReviewActionSummary.target` 存的是工具名（`"memory"`/`"skill_manage"`）而非 memory/skill 名字；old/new 内容在工具入参里，`run_review` 事件处理只看 result 不看 input | review.rs:217 `target: name.to_string()`；file_memory.rs 结果 JSON 无结构化 op/target 字段 | §5 工具 result 增结构化字段 + `run_review` 捕获 input；§7 bounded unified diff |
| G3 | **失败静默**：review 执行失败时 `notify_completion` 直接 return，不发任何通知，`session/list` 停留在旧状态 | review_runner.rs:262-265 `Err(()) => return` | §3 status 增加 `failed`；§4 失败也发通知 |

同时修正 v1 与实现的**契约偏差**（v1 的 camelCase 字段、`completed` 状态从未上线）：

| v1 Draft | 实际实现（上线） | v2 决定 |
|---|---|---|
| `reviewedAt` / `memoryUpdateCount`（camelCase） | `reviewed_at` / `memory_count`（snake_case） | **保留 snake_case**（已上线，FE 已消费）；v1 camelCase 契约作废 |
| `status: running\|completed\|skipped\|failed` | `status: reviewed\|skipped` | 保留 `reviewed` 为成功终态；**增量新增** `running`、`failed` |
| 无 `reviewId` | 无 | 新增 `review_id`（可空，兼容旧记录） |

---

## 1. 设计目标

- session list 展示最近一次 Background review 状态与**修改摘要**；
- review 运行中 / 完成 / **失败**的 realtime 状态通知；
- 刷新、重连后的状态恢复（持久化 + 查询收敛，通知仅 best-effort）；
- **结构化修改明细**：哪条 memory/skill、什么操作（op）、成功与否——UI 不解析自然语言摘要作为数据源；
- **内容级 diff 可查**（P2）：这次 review 对 memory/skill 文件改了什么；
- 手动启动和取消；与代码审查循环 `auto-review` 域保持边界。

---

## 2. 现状基线（v2 的兼容起点）

已上线的三条暴露路径（实现：`review_runner.rs:242-289`、`protocol.rs:55-58`、`agent.rs` session/list 投影）：

1. **`session/list` 的 `_meta.review`**（持久化，重连收敛来源）；
2. **`session/update` → `session_info_update` 的 `_meta.review`**（完成时 realtime）；
3. **`agent_message_chunk`** 人类可读摘要（仅 UI 展示，非数据源）。

现状 `_meta.review` wire 契约（snake_case，v2 起为**冻结基线**，只增不改）：

```jsonc
{
  "status": "reviewed" | "skipped",
  "reviewed_at": "RFC3339",
  "memory_count": 2,
  "skill_count": 1,
  "skip_reason": "…",       // 仅 skipped
  "duration_ms": 4200
}
```

已知问题（v2 修复）：无 `review_id`/`trigger`/`started_at`；无 actions；失败静默（G3）；明细易失（G1）。

---

## 3. `ReviewState` v2 统一 schema

v2 定义**单一 `ReviewState` 对象**，四个载体共用同一 schema（含字段增减规则）：

| 载体 | 方向 | 内容 |
|---|---|---|
| `session/list` → `_meta.review` | 持久化回读 | 完整 ReviewState（内联 actions 为摘要版） |
| `session/update` → `session_info_update` → `_meta.review` | realtime | 完整 ReviewState（同上） |
| `_anureo.dev/review/status` → `latest` | 查询 | 完整 ReviewState |
| `_anureo.dev/review/changed`（notification） | realtime | 完整 ReviewState（超集快照） |

### 3.1 字段

```jsonc
{
  // ---- v2 冻结基线（现有字段，语义不变）----
  "status": "running | reviewed | skipped | failed",
  "reviewed_at": "RFC3339 | null",          // 终态时间；running 时为 null
  "memory_count": 2,
  "skill_count": 1,
  "skip_reason": "insufficient_content",     // 仅 skipped
  "duration_ms": 4200,

  // ---- v2 新增（全部 optional，老客户端可忽略）----
  "review_id": "review-uuid",                // 贯穿通知/查询/明细的稳定 ID
  "trigger": "background | review-skill | manual",
  "started_at": "RFC3339 | null",
  "fail_reason": "llm_error",                // 仅 failed，机器可读
  "actions": [ /* 内联摘要版，见 3.2 */ ],
  "actions_truncated": false                  // actions 超过内联上限时为 true
}
```

**status 语义**：

| 值 | 含义 | 与现状关系 |
|---|---|---|
| `running` | review 进行中（§4 start 通知） | 新增 |
| `reviewed` | 成功完成（可能 0 写入） | 既有，保留 |
| `skipped` | 跳过（内容不足等），`skip_reason` 必带 | 既有，保留 |
| `failed` | 执行失败，`fail_reason` 必带（G3：现状静默，v2 起必须通知 + 落库） | 新增 |

客户端规则：**未知 status 值按 `running` 处理**（向后兼容未来的新中间态）。

### 3.2 内联 `actions[]`（摘要版）

- 上限 **10 条**（按工具调用顺序），超出置 `actions_truncated: true`，完整列表走 `review/details`；
- 每条为 `ReviewAction` 的**去 diff 子集**：`kind`、`op`、`target`、`succeeded`、`summary`；
- 不携带 diff 内容（防止 `_meta.review` 膨胀与敏感内容外泄）。

```jsonc
"actions": [
  { "kind": "memory", "op": "add",    "target": "user",     "succeeded": true,
    "summary": "Memory 'timezone-cst' added (86 chars)" },
  { "kind": "skill",  "op": "update", "target": "rust-cli-cross-layer-feedback",
    "succeeded": true, "summary": "Skill updated (+567 chars)" }
]
```

### 3.3 `ReviewAction` 完整结构（`review/details` 返回）

```jsonc
{
  "kind": "memory | skill | other",
  "op": "add | replace | remove | create | update | delete | …",  // 工具动作规范化，见 §5.1
  "target": "memory | user | <skill-name>",   // ★ v2 修正：不再是工具名
  "file_path": "USER.md | PROJECT.md | <skill-relative-path>",   // optional
  "succeeded": true,
  "summary": "≤160 chars 的工具消息",
  "chars_before": 1234,        // optional
  "chars_after": 1567,         // optional
  "diff": { … },               // 仅 review/details + includeDiff，见 §7
  "diff_truncated": false
}
```

---

## 4. Realtime 通知

状态迁移通过现有 `session/update` 通道发送 `session_info_update`（`_meta.review` = 完整 ReviewState）：

```jsonc
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "sess-123",
    "update": {
      "sessionUpdate": "session_info_update",
      "_meta": { "review": { "status": "running", "review_id": "review-uuid",
                             "trigger": "background", "started_at": "…" } }
    }
  }
}
```

| 时机 | status | 必带字段 |
|---|---|---|
| review 启动（含 `/review-skill`） | `running` | `review_id`、`trigger`、`started_at` |
| 完成 | `reviewed` | `review_id`、`actions[]`、计数、`duration_ms` |
| 跳过 | `skipped` | `skip_reason`、`review_id` |
| **失败**（G3） | `failed` | `fail_reason`、`review_id` |

- 完成态继续附带 `agent_message_chunk` 人类摘要（不变），但客户端以 `_meta.review` 为状态真源；
- 通知 **best-effort**：丢失不导致状态丢失，客户端在 session list / `review/status` / `review/history` 查询时收敛；
- 失败也必须落 `ReviewHistory`（`review_status` 表 upsert 为 `failed`），保证重连后可见。

---

## 5. 数据侧前置改动（协议的数据来源，P1a/P1b）

协议字段不是免费的：G1/G2 的根因在数据侧。以下改动是 §3/§6 的前置条件。

### 5.1 工具 result 结构化字段（消除自然语言解析）

**memory 工具**（`file_memory.rs`）result JSON 增量添加（现有字段不动）：

```jsonc
// add 成功
{ "success": true, "message": "…", "entries": 3, "usage": {…},
  "op": "add", "file": "user",              // ★ 新增
  "chars": 86, "entry_key": "timezone-cst" } // ★ 新增（可用于 target 之外的展示）
```

- `op` ∈ `add | replace | remove`（与工具 action 参数一致）；
- `file` ∈ `user | memory`（即 MemoryFile 的 wire 表示）；
- `chars`：本次写入内容长度（add/replace）；`entry_key`：条目摘要 key（若 store 层可得）。

**skill_manage 工具**（`tool-basic/src/skill/manage.rs`）result JSON 同理增加：

```jsonc
{ "success": true, "message": "…",
  "op": "patch",                // 工具 action 原样：create/edit/patch/write_file/remove_file/delete/…
  "name": "rust-cli-…",         // ★ skill 名
  "file_path": "SKILL.md" }     // ★ 有子文件操作时携带
```

`op` 规范化映射（协议层展示用）：`create → create`；`edit/patch/write_file → update`；`remove_file/delete → delete`；其余原样透传并归入 `other`。规范化结果存 `ReviewAction.op`，原始 action 不单独上 wire（需要时从 summary 可见）。

### 5.2 `ReviewActionSummary` v2（`experimental/curator/src/review.rs`）

```rust
pub struct ReviewActionSummary {
    pub kind: String,        // "memory" | "skill" | "other"（不变）
    pub op: String,          // ★ 新增
    pub target: String,      // ★ 语义修正：memory → "user"/"memory"；skill → skill 名
    pub file_path: Option<String>,   // ★ 新增
    pub summary: String,     // 保留（≤160 chars）
    pub succeeded: bool,     // 保留
    pub chars_before: Option<u64>,   // ★ P2
    pub chars_after: Option<u64>,    // ★ P2
}
```

`parse_action` 改为优先读取 §5.1 的结构化字段；旧格式（无新字段的结果 JSON）降级为现状行为（`target` = 工具名，`op = "unknown"`）。

### 5.3 `run_review` 捕获工具入参（P2 diff 的前置）

`AgentEvent::ToolCallStart` 已携带 input kwargs；`run_review` 的事件处理目前只处理 `ToolEnd`。v2：
- 以 `(tool_call_id → input)` 维护在途调用表，`ToolEnd` 时配对，得到 `old_text`/`new_content`（memory replace/remove）、`content`（add）、`old_string`/`new_string`（skill patch）；
- 该配对同时用于 §7 的 diff 构造（add/replace/remove/patch 类可直接从入参构造，无需读文件）。

### 5.4 持久化（`experimental/curator/src/history.rs`）

`review_history` 表增量迁移（旧行 NULL 兼容）：

| 新列 | 类型 | 说明 |
|---|---|---|
| `review_id` | TEXT nullable | uuid，upsert 进 `review_status` 的 value JSON |
| `actions_json` | TEXT nullable | `Vec<ReviewActionSummary>` 序列化（含 P2 的 diff 引用） |

- `review_status`（每 session 最新状态）的 value JSON 即持久化的 `ReviewState`，session/list 投影直接读它；
- **失败必须落库**（`status=failed` + `fail_reason`），修复 G3 的持久化缺口；
- `review/history` 查询基于 `review_history` 按 `reviewed_at` 降序 + cursor 分页。

---

## 6. `_anureo.dev/review/*` 扩展域

> 命名空间：`_anureo.dev/review/*`；envelope 层 ACP 标准字段（`sessionId`、`cursor`）用 camelCase，ReviewState/ReviewAction 对象内部维持 snake_case（与 `_meta.review` 一份 schema）。

### 6.1 Capability

```jsonc
{ "backgroundReview": { "status": true, "history": true, "details": true,
                        "diff": true, "start": true, "cancel": true } }
```

客户端按子标志降级：无 `details` → 只展示内联 `actions[]`；无 `diff` → `includeDiff` 请求被拒或忽略。

### 6.2 方法

| 方法 | 方向 | 用途 |
|---|---|---|
| `review/status` | request | 查询 session 当前 ReviewState（含 running） |
| `review/history` | request | 分页查询 review 历史（摘要版 actions） |
| `review/details` | request | 某次 review 的完整 actions（可选 diff） |
| `review/start` | request | 手动启动，`scope: all \| memory \| skills` |
| `review/cancel` | request | 取消当前 review |
| `review/changed` | notification | 状态迁移时推送完整 ReviewState |

**`review/status`**

```json
{ "sessionId": "sess-123" }
```
```jsonc
{ "sessionId": "sess-123", "active": false,
  "latest": { "status": "reviewed", "review_id": "…", "memory_count": 2,
              "skill_count": 1, "actions": [ /* 摘要版 */ ] } }
```

**`review/history`**：`{ sessionId?, cursor?, limit? }` → `{ items: [ReviewState 摘要版], nextCursor }`，`reviewed_at` 降序；缺省 `sessionId` 时为跨 session 全局历史（需 owner 权限）。

**`review/details`**：`{ sessionId, reviewId, includeDiff?, cursor?, limit? }` → `{ reviewId, actions: [ReviewAction 完整版], nextCursor }`。分页返回 actions（默认 50/页）。`includeDiff: true` 需 capability `diff`。

**`review/start`**：`{ sessionId, scope, trigger: "manual" }` → 幂等：同 session 已有 running review 时**返回现有 `review_id`**，不并发；否则创建并推 `running` 通知。

**`review/cancel`**：`{ sessionId, reviewId }` → 只取消 Background review，不发 `session/cancel`、不中断主 prompt；目标不存在返回 `not_found`，已完成返回 `already_completed`，不静默成功。

**`review/changed`**：payload 为完整 ReviewState + `sessionId`；与 `session_info_update._meta.review` 冗余但自包含，供不订阅 session update 流的面板使用。

### 6.3 错误与权限

- 所有 request 校验 session 存在性与 principal 的 owner 可见性（沿用 session 可见性规则）；
- memory/skill 内容可能含用户敏感信息：`details`/`diff` 与 owner 通道同等安全级别，禁止跨 owner 暴露；
- `start`/`cancel` 为写操作，使用独立 `backgroundReview` capability（不借用 `auto-review`）；
- 明细超限返回结构化错误 + 分页 cursor，不截断为不可解释文本。

---

## 7. 内容 diff 规范（P2）

- **来源**：优先从工具入参构造（§5.3 配对）——memory `replace`（old_text→new_content）、`remove`（old_text→空）、`add`（空→content）；skill `patch`（old_string→new_string，按 file_path 定位）。`write_file`/`edit` 全量写场景 P2 先不做 before 快照，`diff` 缺省并置 `diff_available: false`；
- **格式**：unified diff（`---/+++ @@`），UTF-8 安全；
- **上限**：每条 action ≤ 8 KiB，超出截断并置 `diff_truncated: true`；
- **不落明文**：`review_history.actions_json` 持久化 diff **引用**（入参已在 checkpoint/会话记录中），可选配置 `persist_diff: true` 才落明文（默认 false，控制 memory.db 体积与敏感面）。

---

## 8. 兼容性与迁移

1. `_meta.review` 既有字段名与取值**不变**（`reviewed`/`skipped` 保留）；新增字段全部 optional；
2. 未知 `status` 值（`running`/`failed`）→ 老客户端按未知处理；建议 FE 将未知值渲染为「进行中/异常」而非崩溃；
3. 无扩展 capability 时，客户端退回 `_meta.review` 内联 `actions[]`（已是摘要版，无 diff）；
4. 旧 `review_history` 行 `actions_json = NULL`：`review/details` 返回 `actions: []` + `actions_truncated: false` + `legacy: true`（可选标记），不得报错；
5. `agent_message_chunk` 摘要保留，纯文本消费者不受影响。

---

## 9. 测试计划

| 测试 | 验证点 |
|---|---|
| serde 契约 | ReviewState/ReviewAction v2 字段、optional/default、旧行 NULL 兼容 |
| 四状态通知 | running/reviewed/skipped/failed 均发送 `session_info_update`（含 G3 失败路径） |
| 内联截断 | actions > 10 时 `actions_truncated=true`，无 diff 字段外泄 |
| details 分页 | cursor 翻页、`includeDiff` 无 capability 时被拒 |
| diff 上限 | > 8 KiB 截断 + `diff_truncated`；UTF-8/中文安全 |
| target 修正 | `parse_action` v2 从结构化字段取 target；旧格式降级不 panic |
| 迁移 | 旧 `review_history` 行（无新列值）可读、history 查询不报错 |
| start 幂等 | 并发/重复 start 返回同一 `review_id` |
| cancel 边界 | 不影响主 prompt；not_found / already_completed 显式错误 |
| 收敛 | 丢通知后 `session/list` / `review/status` 恢复一致状态 |

---

## 10. 实施顺序与文件清单

| 阶段 | 内容 | 文件 |
|---|---|---|
| P1a | 工具 result 结构化字段（op/file/name/file_path）+ `parse_action` v2 | `file_memory.rs`、`skill/manage.rs`、`curator/src/review.rs` |
| P1b | `ReviewActionSummary` v2 + `run_review` 入参配对（无 diff）+ 持久化迁移（review_id/actions_json/failed 落库） | `curator/src/review.rs`、`curator/src/history.rs` |
| P1c | wire 增量：`_meta.review` 新字段 + 内联 actions + running/failed 通知 | `apps/acp/src/review_runner.rs`、`apps/acp/src/agent.rs`（session/list 投影） |
| P1d | 扩展域：`review/status`、`history`、`details`、`changed`（start/cancel 可随后） | `apps/acp/src/extensions/review.rs`（新增）、`extensions/register.rs` |
| P2 | 入参 diff 构造 + `details.includeDiff` + chars_before/after | `curator/src/review.rs`、`extensions/review.rs` |
| P3 | candidate/confirm/rollback（沿架构文档 Phase 1/2 方向） | 另立设计，不在本修订展开 |

同步文档：`background-review-design.md`（§2 现状表、过期行号）、`docs/user-guide/10-memory-review-experimental.md`（通知字段说明）。

---

## 11. 开放问题

1. `skill_manage` 动作枚举的 `op` 规范化映射表需在 P1a 实现时对齐实际枚举（create/edit/patch/write_file/remove_file/delete…）；
2. `entry_key`/`chars` 是否由 memory-v2 store 层直接返回（`AddResult` 已有 usage/provenance，扩展顺路）还是工具层计算；
3. `review/changed` 与 `session_info_update._meta.review` 双通道并存是否长期保留，或收敛为单通道 + 订阅过滤；
4. 跨 session 全局 `review/history` 的 owner 语义（个人 home 下基本单 owner，多 owner 场景待定）；
5. P3 candidate 存储位置（memory.db 新表 vs 独立 candidates 文件）与 rollback 粒度（逐条 vs 整次）。

# 跨域设计模式

> **适用范围**: 所有 `_anureo.dev/*` 扩展域共享的设计模式
> **实现状态**: ❌ 未实现（扩展框架需新建）

---

## 1. 分页协议

所有 `list` 类扩展方法使用统一的分页协议。

### Request 参数

```json
{
  "cursor": null,
  "limit": 50
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | string \| null | 否 | 上一页返回的 `nextCursor`；`null` 表示第一页 |
| `limit` | int | 否 | 每页数量，默认 50 |

### Response 结构

```json
{
  "items": [],
  "nextCursor": "eyJpZCI6IjEyMyJ9",
  "hasMore": false
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `items` | array | 当前页数据 |
| `nextCursor` | string \| null | 下一页游标；`null` 表示没有更多 |
| `hasMore` | bool | 是否还有更多数据 |

### 规则

- `nextCursor` 是 opaque string，Client 不解析其内容
- `hasMore = true` 时 `nextCursor` 必须非 null
- `hasMore = false` 时 `nextCursor` 必须为 null
- 空列表返回 `{ items: [], nextCursor: null, hasMore: false }`
- fetch failure 不等于空列表

---

## 2. 写操作权限三层 Gate

所有写操作（create/update/delete/write/stage/push 等）必须通过三层检查：

```text
1. Capability gate — Client 声明了该 method 的 capability
2. Server policy — 路径/资源在允许范围内
3. Explicit confirm — 高危操作需要用户显式确认
```

### Capability Gate

```json
{
  "anureo.dev": {
    "git": {
      "commit": true,
      "push": true,
      "force_push": false
    }
  }
}
```

未声明的 method 返回 `capability_not_supported`。

### Server Policy

| 检查 | 说明 |
|---|---|
| Directory boundary | 路径在 worktree 范围内 |
| Resource ownership | 资源属于当前 principal |
| Rate limit | 未超出频率限制 |

### Explicit Confirm

高危操作通过 `session/request_permission` 请求用户确认：

```text
扩展 method 调用
  → 检查是否需要确认（push、delete、force 操作）
  → 发起 session/request_permission
  → 等待用户确认
  → 执行或拒绝
```

---

## 3. 长时操作进度

commit、push、pull、merge、rebase 等操作可能耗时长，需要进度上报。

### 进度通知

```json
{
  "jsonrpc": "2.0",
  "method": "session/update",
  "params": {
    "sessionId": "thread-abc123",
    "update": {
      "type": "anureo_progress",
      "operationId": "op-001",
      "domain": "git",
      "method": "push",
      "status": "in_progress",
      "message": "Pushing to origin...",
      "percent": 50
    }
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `operationId` | string | 操作唯一 ID（与 request id 无关） |
| `domain` | string | 扩展域名（`git`、`worktree` 等） |
| `method` | string | 具体操作 |
| `status` | string | `started` / `in_progress` / `completed` / `failed` |
| `message` | string | 人类可读状态 |
| `percent` | int | 0-100（可选） |

### 进度生命周期

```text
request accepted → progress(started)
  → progress(in_progress) × N
  → progress(completed) 或 progress(failed)
  → request response
```

response 必须在最后一个 progress 之后发送。

---

## 4. Capability 可变性

扩展 capability 可能在运行时变化（安装插件、配置 MCP server 等）。

### `capability_changed` Notification

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/capability_changed",
  "params": {
    "domains": ["git", "mcp"],
    "added": {
      "git": { "cherry_pick": true }
    },
    "removed": null
  }
}
```

| 场景 | `added` | `removed` |
|---|---|---|
| 新增能力 | 增量对象 | `null` |
| 移除能力 | `null` | 增量对象 |
| 变化过大 | `null` | `null`（Client 需全量重新获取） |

### `capability/describe` Request

```json
{
  "jsonrpc": "2.0",
  "id": 200,
  "method": "_anureo.dev/capability/describe",
  "params": {
    "domains": ["git", "mcp"]
  }
}
```

Response:
```json
{
  "capabilities": {
    "git": { "status": true, "commit": true },
    "mcp": { "list": true }
  }
}
```

不指定 `domains` 时返回全量 capability。

---

## 5. Session Metadata Namespace

扩展可通过 `session_info_update` 的 `metadata` 字段存储自定义数据。

### Namespace 约定

```json
{
  "type": "session_info_update",
  "metadata": {
    "anureo.sessionFolder": "folder-001",
    "anureo.tags": ["bug", "auth"],
    "anureo.customField": "value"
  }
}
```

### 规则

- 扩展使用 `anureo.<domain>.<field>` 命名空间
- 标准 ACP 字段（`title` 等）不在 `metadata` 中
- `metadata` 不影响 Agent 行为
- 大数据不放入 metadata（用独立存储）

---

## 6. Reconnect Resync

每个有 notification 的扩展域必须提供 authoritative resync method。

### 映射表

| Notification | Authoritative method |
|---|---|
| `_anureo.dev/worktree/changed` | `_anureo.dev/worktree/list` |
| `_anureo.dev/git/status_changed` | `_anureo.dev/git/status` |
| `_anureo.dev/git/identity/changed` | `_anureo.dev/git/identity/list` |
| `_anureo.dev/files/changed` | `_anureo.dev/files/list` |
| `_anureo.dev/mcp/status_changed` | `_anureo.dev/mcp/list` |
| `_anureo.dev/goal/changed` | `_anureo.dev/goal/list` |
| `_anureo.dev/skills/changed` | `_anureo.dev/skills/list` |
| `_anureo.dev/session-folder/changed` | `_anureo.dev/session-folder/list` |
| `_anureo.dev/snippet/changed` | `_anureo.dev/snippet/list` |
| `_anureo.dev/command/changed` | `_anureo.dev/command/list` |
| `_anureo.dev/plugin/changed` | `_anureo.dev/plugin/list` |
| `_anureo.dev/agent/changed` | `_anureo.dev/agent/list` |
| `_anureo.dev/project/changed` | `_anureo.dev/project/list` |
| `_anureo.dev/tunnel/changed` | `_anureo.dev/tunnel/list` |
| `_anureo.dev/multi-run/changed` | `_anureo.dev/multi-run/status` |
| `_anureo.dev/settings/changed` | `_anureo.dev/settings/load` |
| `_anureo.dev/github/auth_changed` | `_anureo.dev/github/auth_status` |
| `_anureo.dev/session-assist/recap` | 无（事件型，不需 resync） |
| `_anureo.dev/auto-review/result` | `_anureo.dev/auto-review/status` |
| `_anureo.dev/capability_changed` | `_anureo.dev/capability/describe` |

### Resync 触发

```text
connection reconnect
  → initialize（重新获取 capability snapshot）
  → 对每个活跃扩展域调用 authoritative method
  → 对比结果，增量更新本地状态
  → 清除已不存在的条目（仅在明确成功时）
```

---

## 7. WebSocket 子流

TTS 和 Dictation 等实时音频场景使用独立 WebSocket 子流，不走标准 JSON-RPC。

### 子流生命周期

```text
主连接 /acp（JSON-RPC）
  → 扩展 method 协商子流参数
  → 独立 WebSocket 连接（带子流 token）
  → 二进制帧传输音频
  → 子流结束 → 关闭
```

### 安全

- 子流 token 一次性，限时
- 子流不扩大 ACP capability
- 子流断开不影响主连接
- 主连接断开应级联关闭子流

---

## 8. 扩展错误码

所有扩展域共享以下错误码约定：

| Error code | message | 触发条件 |
|---|---|---|
| `-32601` | `method_not_found` | 未注册的扩展 method |
| `-32602` | `invalid_params` | 参数缺失/类型错误/路径非法 |
| `-32001` | `capability_not_supported` | Client 未声明对应 capability |
| `-32002` | `forbidden` | 已认证但无权限 |
| `-32003` | `not_found` | 目标资源不存在 |
| `-32004` | `timeout` | 操作超时 |
| `-32005` | `conflict` | 状态冲突（如重复创建） |
| `-32006` | `partial_failure` | 批量操作部分失败（response 包含 per-item result） |
| `-32007` | `directory_boundary_violation` | 路径超出 worktree 范围 |
| `-32008` | `rate_limited` | 上游限流（small-model / settings 域） |
| `-32009` | `provider_error` | 上游 provider 失败（small-model 域） |
| `-32013` | `proxy_target_unreachable` | 本地 preview 目标不可达（preview 域） |
| `-32603` | `internal_error` | 服务器内部错误 |

核心协议保留码，扩展域不得复用：

| Error code | message | 核心语义 |
|---|---|---|
| `-32010` | `a prompt is already in progress for this session` | 同 session 重入 prompt / load / resume |
| `-32011` | `session is attached to another connection` | session 绑定在其他 connection |
| `-32012` | `ssrf_blocked` | preview 代理目标非 loopback |

### Partial Failure 格式

```json
{
  "error": {
    "code": -32006,
    "message": "partial_failure",
    "data": {
      "results": [
        { "path": "a.txt", "success": true },
        { "path": "b.txt", "success": false, "error": "permission denied" }
      ]
    }
  }
}
```

---

## 9. 扩展实现框架

### Rust 模块结构（计划）

```text
apps/acp/src/extensions/
├── mod.rs                   ExtensionRegistry, dispatch
├── capability.rs            CapabilityManager
├── pagination.rs            分页辅助
├── progress.rs              进度上报
├── auth.rs                  扩展权限检查
├── boundary.rs              目录/worktree 边界校验
├── worktree.rs
├── git/
│   ├── mod.rs
│   ├── status.rs
│   ├── diff.rs
│   ├── commit.rs
│   ├── branch.rs
│   ├── remote.rs
│   ├── stash.rs
│   ├── merge.rs
│   ├── rebase.rs
│   └── identity.rs
├── files.rs
├── mcp.rs
├── goal.rs
├── scheduled_task.rs
├── connection.rs
├── relay.rs
├── pairing.rs
├── client_auth.rs
├── question.rs
├── github/
├── notification.rs
├── tts.rs
├── dictation.rs
├── skills.rs
├── session_folder.rs
├── snippet.rs
├── command.rs
├── plugin.rs
├── quota.rs
├── provider.rs
├── agent_profile.rs
├── diagnostics.rs
├── project.rs
├── tunnel.rs
├── multi_run.rs
├── settings.rs
├── session_assist.rs
├── small_model.rs
├── auto_review.rs
├── preview.rs
└── terminal_ext.rs
```

### ExtensionRegistry

```rust
pub struct ExtensionRegistry {
    handlers: HashMap<String, Box<dyn ExtensionHandler>>,
    capabilities: RwLock<CapabilitySnapshot>,
}

#[async_trait]
pub trait ExtensionHandler: Send + Sync {
    async fn handle(
        &self,
        method: &str,
        params: serde_json::Value,
        context: &ExtensionContext,
    ) -> Result<serde_json::Value, ExtensionError>;
}

pub struct ExtensionContext {
    pub session_id: Option<String>,
    pub principal: String,
    pub connection_id: String,
    pub working_directory: Option<PathBuf>,
    pub client_capabilities: ClientCapabilitiesInfo,
}
```

### Dispatch 流程

```text
收到 _anureo.dev/{domain}/{method} JSON-RPC request
  → 提取 domain 和 method
  → 检查 domain 是否在 capability snapshot 中
  → 检查 method 是否注册
  → 检查 directory boundary（如有路径参数）
  → 检查 server policy
  → 调用 ExtensionHandler::handle()
  → 返回 result 或 error
```

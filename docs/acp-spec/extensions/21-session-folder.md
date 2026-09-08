# Session Folder 扩展

> 命名空间: `_anureo.dev/session-folder/*`
> Capability key: `session-folder`
> 实现状态: ❌ 未实现

---

## Capability

```json
{
  "session-folder": {
    "list": true,
    "create": true,
    "update": true,
    "delete": true,
    "assign": true
  }
}
```

**核心原则：**
- Session folder 是 **UI 组织层**，不影响 session 的生命周期和 Agent 行为
- 删除文件夹**不删除**其中的 session；session 变为"未分配"状态
- Folder 信息存储在 server 端，跨连接同步
- Agent **不感知** folder 结构；folder 只影响 client UI 展示

---

## Methods

### `_anureo.dev/session-folder/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `session-folder.list` |
| 权限 | 无 |

列出所有文件夹及其包含的 session 引用。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/session-folder/list",
  "params": {
    "cursor": null,
    "limit": 50
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `cursor` | string\|null | 否 | 分页游标（见 `08-cross-cutting-patterns.md` §1） |
| `limit` | number | 否 | 每页数量，默认 50 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "items": [
      {
        "id": "folder-001",
        "name": "前端开发",
        "color": "#3b82f6",
        "sortOrder": 0,
        "createdAt": "2025-01-10T08:00:00Z",
        "updatedAt": "2025-01-15T12:00:00Z",
        "sessionCount": 3,
        "sessions": [
          { "sessionId": "session-abc", "title": "Fix React hooks", "updatedAt": "2025-01-15T12:00:00Z" },
          { "sessionId": "session-def", "title": "Add CSS animations", "updatedAt": "2025-01-14T10:00:00Z" },
          { "sessionId": "session-ghi", "title": "TypeScript migration", "updatedAt": "2025-01-13T15:00:00Z" }
        ]
      },
      {
        "id": "folder-002",
        "name": "后端开发",
        "color": "#10b981",
        "sortOrder": 1,
        "createdAt": "2025-01-11T09:00:00Z",
        "updatedAt": "2025-01-16T08:00:00Z",
        "sessionCount": 2,
        "sessions": [
          { "sessionId": "session-jkl", "title": "API refactoring", "updatedAt": "2025-01-16T08:00:00Z" },
          { "sessionId": "session-mno", "title": "Database migration", "updatedAt": "2025-01-15T14:00:00Z" }
        ]
      }
    ],
    "unassignedCount": 5,
    "nextCursor": null,
    "hasMore": false
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].id` | string | 文件夹唯一标识 |
| `items[].name` | string | 文件夹名称 |
| `items[].color` | string\|null | 文件夹颜色（hex 格式，UI 用） |
| `items[].sortOrder` | number | 排序权重（升序） |
| `items[].createdAt` | string | 创建时间 |
| `items[].updatedAt` | string | 最后更新时间 |
| `items[].sessionCount` | number | 文件夹内 session 数量 |
| `items[].sessions` | SessionRef[] | 文件夹内的 session 引用（精简信息） |
| `unassignedCount` | number | 未分配到任何文件夹的 session 数量 |

#### 逻辑说明

1. **排序**: 文件夹按 `sortOrder` 升序排列。Session 在文件夹内按 `updatedAt` 降序排列。
2. **session 精简**: `sessions` 数组中的每个项只包含 `sessionId`、`title`、`updatedAt`。完整 session 信息通过标准 `session/load` 获取。
3. **未分配 session**: `unassignedCount` 帮助 UI 显示未分类的 session 数量。未分配 session 的列表通过标准 `session/list` 获取。
4. **分页**: 遵循统一分页协议（`08-cross-cutting-patterns.md` §1）。小型集合（< 100 文件夹）可忽略分页参数。

#### Rust 类型

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    pub session_id: String,
    pub title: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolder {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    pub sort_order: i32,
    pub created_at: String,
    pub updated_at: String,
    pub session_count: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<SessionRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_page_limit")]
    pub limit: u32,
}

fn default_page_limit() -> u32 { 50 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderListResponse {
    pub items: Vec<SessionFolder>,
    pub unassigned_count: u32,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | 存储不可用 |

---

### `_anureo.dev/session-folder/create`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `session-folder.create` |
| 权限 | 无 |

创建文件夹。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/session-folder/create",
  "params": {
    "name": "测试任务",
    "color": "#f59e0b",
    "sortOrder": 2
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | 是 | 文件夹名称（非空，最大 100 字符） |
| `color` | string | 否 | 文件夹颜色（hex 格式） |
| `sortOrder` | number | 否 | 排序权重；省略则追加到末尾（当前最大值 + 1） |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "folder": {
      "id": "folder-003",
      "name": "测试任务",
      "color": "#f59e0b",
      "sortOrder": 2,
      "createdAt": "2025-01-19T10:00:00Z",
      "updatedAt": "2025-01-19T10:00:00Z",
      "sessionCount": 0,
      "sessions": []
    }
  }
}
```

#### 逻辑说明

1. **名称唯一性**: 文件夹名称不要求唯一（允许同名），但 UI 可自行检查。
2. **sortOrder**: 若指定，插入到对应位置，后续文件夹 sortOrder 自动调整。若省略，追加到末尾。
3. **初始空文件夹**: 新建文件夹的 `sessionCount` 为 0，`sessions` 为空数组。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderCreateRequest {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderCreateResponse {
    pub folder: SessionFolder,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `name` 为空或超过 100 字符 |
| `Internal Error (-32603)` | 存储失败 |

---

### `_anureo.dev/session-folder/update`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `session-folder.update` |
| 权限 | 无 |

更新文件夹属性（名称、排序、颜色）。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_anureo.dev/session-folder/update",
  "params": {
    "folderId": "folder-001",
    "name": "前端开发 (Q1)",
    "color": "#6366f1",
    "sortOrder": 0
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `folderId` | string | 是 | 要更新的文件夹 ID |
| `name` | string | 否 | 新名称（省略则不修改） |
| `color` | string\|null | 否 | 新颜色（传 `null` 清除颜色） |
| `sortOrder` | number | 否 | 新排序权重 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "updated": true,
    "folder": {
      "id": "folder-001",
      "name": "前端开发 (Q1)",
      "color": "#6366f1",
      "sortOrder": 0,
      "createdAt": "2025-01-10T08:00:00Z",
      "updatedAt": "2025-01-19T10:30:00Z",
      "sessionCount": 3,
      "sessions": []
    }
  }
}
```

#### 逻辑说明

1. **部分更新**: `name`、`color`、`sortOrder` 均可选，只更新传入的字段。
2. **sortOrder 调整**: 修改 `sortOrder` 后，后续文件夹的 sortOrder 自动调整（保持有序）。
3. **color 清除**: 传入 `color: null` 清除颜色（变为默认/无色）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderUpdateRequest {
    pub folder_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderUpdateResponse {
    pub updated: bool,
    pub folder: SessionFolder,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `folderId` 不存在或 `name` 超过 100 字符 |
| `Internal Error (-32603)` | 存储失败 |

---

### `_anureo.dev/session-folder/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `session-folder.delete` |
| 权限 | 无 |

删除文件夹。**删除文件夹不删除其中的 session。**

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "_anureo.dev/session-folder/delete",
  "params": {
    "folderId": "folder-003"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `folderId` | string | 是 | 要删除的文件夹 ID |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "deleted": true,
    "folderId": "folder-003",
    "releasedSessions": 2
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `deleted` | bool | 是否删除成功 |
| `folderId` | string | 被删除的文件夹 ID |
| `releasedSessions` | number | 从文件夹释放的 session 数量（变为未分配） |

#### 逻辑说明

1. **Session 安全**: 删除文件夹时，其中的 session 不被删除。Session 的 folder 归属被清除，变为"未分配"状态。
2. **sortOrder 重排**: 删除文件夹后，剩余文件夹的 sortOrder 自动压缩（保持连续）。
3. **幂等**: 删除不存在的文件夹返回 `deleted: false`，不报错。
4. **Agent 无感知**: Agent 不感知 folder 删除。Session 继续正常运行。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderDeleteRequest {
    pub folder_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderDeleteResponse {
    pub deleted: bool,
    pub folder_id: String,
    pub released_sessions: u32,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | 存储失败 |

---

### `_anureo.dev/session-folder/assign`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `session-folder.assign` |
| 权限 | 无 |

将 session 分配到文件夹，或从文件夹移除。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "_anureo.dev/session-folder/assign",
  "params": {
    "sessionId": "session-abc",
    "folderId": "folder-001"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sessionId` | string | 是 | 要分配的 session ID |
| `folderId` | string\|null | 是 | 目标文件夹 ID；`null` 表示移除文件夹归属（变为未分配） |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "assigned": true,
    "sessionId": "session-abc",
    "folderId": "folder-001",
    "previousFolderId": null
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `assigned` | bool | 是否分配成功 |
| `sessionId` | string | 被分配的 session ID |
| `folderId` | string\|null | 当前文件夹 ID（`null` 为未分配） |
| `previousFolderId` | string\|null | 之前的文件夹 ID（首次分配为 null） |

#### 逻辑说明

1. **移动语义**: session 只能属于一个文件夹。分配到新文件夹时自动从旧文件夹移除。
2. **Session 存在性校验**: 若 `sessionId` 不存在，返回 `Invalid Params`。
3. **Folder 存在性校验**: 若 `folderId` 不存在，返回 `Invalid Params`。
4. **Agent 无感知**: 分配操作不影响 Agent 对 session 的处理。Agent 不读取 folder 信息。
5. **批量操作**: 批量分配可通过多次调用实现（每次一个 session）。未来可扩展批量 API。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderAssignRequest {
    pub session_id: String,
    pub folder_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionFolderAssignResponse {
    pub assigned: bool,
    pub session_id: String,
    pub folder_id: Option<String>,
    pub previous_folder_id: Option<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `sessionId` 不存在或 `folderId` 不存在 |
| `Internal Error (-32603)` | 存储失败 |

---

## Notifications

### `_anureo.dev/session-folder/changed`

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/session-folder/changed",
  "params": {
    "change": "update",
    "folderId": "folder-001"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | `"create"` \| `"update"` \| `"delete"` \| `"assign"` | 变更类型 |
| `folderId` | string\|null | 受影响的文件夹 ID（`assign` 到 null 时为 null） |

**触发场景：**
- `session-folder/create` 成功后
- `session-folder/update` 成功后
- `session-folder/delete` 成功后
- `session-folder/assign` 成功后
- 外部变更（用户在另一个 client 操作了同一 server）

**多 client 同步：** 所有连接到同一 server 的 client 都收到此 notification。Client 收到后调用 `session-folder/list` 获取最新文件夹树。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `session-folder/changed` | `session-folder/list` | 完整文件夹树（含 session 引用） |

> Client 重连后调用 `session-folder/list` 获取完整文件夹树快照。Folder 结构是 client 可重建的 UI 组织数据。若 notification 丢失（网络断开），resync 保证最终一致。
>
> **注意：** Session folder 不影响 session 生命周期。即使 folder 数据完全丢失，所有 session 仍可通过标准 `session/list` 恢复，只是丢失 UI 分组信息。

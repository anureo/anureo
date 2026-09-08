# Skills Catalog 扩展

> 命名空间: `_anureo.dev/skills/*`
> Capability key: `skills`
> 实现状态: ❌ 未实现

---

## Capability

```json
{
  "skills": {
    "list": true,
    "search": true,
    "install": true,
    "uninstall": true,
    "configure": true
  }
}
```

**职责边界：**
- 本扩展只管理 **catalog 发现、安装、配置和卸载**
- Skill 的**运行时加载**由 Agent 负责，不属于本扩展
- 安装后的 skill 是否被 Agent 使用取决于 Agent 的 skill registry 逻辑

**安装安全规则：**
- 安装失败必须**回滚到安装前状态**（atomic install）
- Skill 源可以是本地路径、Git URL 或 registry URL
- Skill 内容在安装时校验签名（若 registry 支持）
- 卸载不影响正在使用该 skill 的活跃 session（session 保持已加载状态，重启后生效）

---

## Methods

### `_anureo.dev/skills/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `skills.list` |
| 权限 | 无 |

列出所有已安装的 skill。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/skills/list",
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
        "id": "rust-architecture",
        "name": "Rust Architecture",
        "version": "1.2.0",
        "description": "Rust 架构最佳实践参考",
        "category": "coding",
        "source": "registry",
        "sourceUrl": "https://registry.anureo.dev/skills/rust-architecture",
        "installedAt": "2025-01-10T08:00:00Z",
        "updatedAt": "2025-01-15T12:00:00Z",
        "enabled": true,
        "configSchema": {
          "type": "object",
          "properties": {
            "strictMode": { "type": "boolean", "default": false }
          }
        },
        "config": {
          "strictMode": true
        }
      }
    ],
    "nextCursor": null,
    "hasMore": false
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].id` | string | Skill 唯一标识 |
| `items[].name` | string | 显示名称 |
| `items[].version` | string | 已安装版本 |
| `items[].description` | string | 简短描述 |
| `items[].category` | string | 分类（如 `coding`、`devops`、`data-science`） |
| `items[].source` | `"registry"` \| `"git"` \| `"local"` | 安装来源 |
| `items[].sourceUrl` | string | 来源 URL（registry/git 地址） |
| `items[].installedAt` | string | 安装时间 |
| `items[].updatedAt` | string | 最后更新时间 |
| `items[].enabled` | bool | 是否启用 |
| `items[].configSchema` | object\|null | 配置 JSON Schema（供 UI 渲染配置表单） |
| `items[].config` | object\|null | 当前配置值 |

#### 逻辑说明

1. **排序**: 默认按 `name` 字母序排列。
2. **configSchema**: 若 skill 支持配置，`configSchema` 为 JSON Schema 描述可配置字段。UI 可据此渲染配置界面。
3. **分页**: 遵循统一分页协议（`08-cross-cutting-patterns.md` §1）。小型集合（< 100 项）可忽略分页参数。

#### Rust 类型

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillSource {
    Registry,
    Git,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    pub source: SkillSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_url: Option<String>,
    pub installed_at: String,
    pub updated_at: String,
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsListRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_page_limit")]
    pub limit: u32,
}

fn default_page_limit() -> u32 { 50 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsListResponse {
    pub items: Vec<SkillInfo>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | Skill 存储不可用 |

---

### `_anureo.dev/skills/search`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `skills.search` |
| 权限 | 无 |

搜索 skill catalog（包含已安装和可安装的 skill）。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/skills/search",
  "params": {
    "query": "rust",
    "category": "coding",
    "cursor": null,
    "limit": 20
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `query` | string | 否 | 搜索关键词（匹配 name、description、tags） |
| `category` | string | 否 | 分类过滤 |
| `cursor` | string\|null | 否 | 分页游标 |
| `limit` | number | 否 | 每页数量，默认 20 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "items": [
      {
        "id": "rust-architecture",
        "name": "Rust Architecture",
        "version": "1.3.0",
        "description": "Rust 架构最佳实践参考",
        "category": "coding",
        "tags": ["rust", "architecture", "clean-architecture"],
        "installed": true,
        "installedVersion": "1.2.0",
        "updateAvailable": true,
        "downloadCount": 1542,
        "rating": 4.7,
        "registryUrl": "https://registry.anureo.dev/skills/rust-architecture"
      }
    ],
    "nextCursor": "cursor-xyz",
    "hasMore": true
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].installed` | bool | 是否已安装 |
| `items[].installedVersion` | string\|null | 已安装版本（若已安装） |
| `items[].updateAvailable` | bool | 是否有可用更新 |
| `items[].downloadCount` | number | 下载次数（registry 统计） |
| `items[].rating` | number | 评分（0-5） |
| `items[].registryUrl` | string | Registry 详情页 URL |

#### 逻辑说明

1. **空 query**: `query` 为空时返回热门/推荐 skill 列表。
2. **Registry 查询**: Server 查询配置的 skill registry（如 `registry.anureo.dev`）。
3. **离线模式**: Registry 不可达时，只返回已安装 skill 的搜索结果。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsSearchRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default = "default_search_limit")]
    pub limit: u32,
}

fn default_search_limit() -> u32 { 20 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSearchResult {
    pub id: String,
    pub name: String,
    pub version: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(default)]
    pub update_available: bool,
    #[serde(default)]
    pub download_count: u64,
    #[serde(default)]
    pub rating: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsSearchResponse {
    pub items: Vec<SkillSearchResult>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Internal Error (-32603)` | Registry 查询失败（降级为只返回已安装 skill） |

---

### `_anureo.dev/skills/install`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `skills.install` |
| 权限 | Server-side authorization（`skills:write` scope）；建议 UI 确认 |
| Timeout | 120s（取决于 skill 大小和网络） |

安装 skill。**失败必须回滚到安装前状态。**

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_anureo.dev/skills/install",
  "params": {
    "source": "registry",
    "skillId": "rust-architecture",
    "version": "latest",
    "force": false
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `source` | `"registry"` \| `"git"` \| `"local"` | 否 | 安装来源，默认 `"registry"` |
| `skillId` | string | 条件必填 | registry skill ID（`source: "registry"` 时必填） |
| `url` | string | 条件必填 | Git URL 或本地路径（`source: "git"` 或 `"local"` 时必填） |
| `version` | string | 否 | 要安装的版本，默认 `"latest"` |
| `force` | bool | 否 | 强制重装（即使已安装相同版本），默认 `false` |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "installed": true,
    "skill": {
      "id": "rust-architecture",
      "name": "Rust Architecture",
      "version": "1.3.0",
      "description": "Rust 架构最佳实践参考",
      "category": "coding",
      "source": "registry",
      "sourceUrl": "https://registry.anureo.dev/skills/rust-architecture",
      "installedAt": "2025-01-19T10:00:00Z",
      "updatedAt": "2025-01-19T10:00:00Z",
      "enabled": true,
      "configSchema": null,
      "config": null
    },
    "previousVersion": "1.2.0"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `installed` | bool | 是否安装成功 |
| `skill` | SkillInfo | 安装后的 skill 信息 |
| `previousVersion` | string\|null | 之前的版本（升级场景）；首次安装为 null |

#### 逻辑说明

1. **Atomic install（回滚保证）**: 安装过程分为下载 → 校验 → 写入 → 注册。任何步骤失败，Server 回滚到安装前状态（删除已写入的文件、恢复旧版本文件）。
2. **版本升级**: 若 skill 已安装且 `version` 不同，先备份当前版本，再安装新版本。失败时恢复备份。
3. **签名校验**: 若 registry 提供 skill 签名，Server 在写入前校验。校验失败中止安装。
4. **进度 notification**: 安装是长时操作，Server 发送进度 notification（`08-cross-cutting-patterns.md` §3）：
   ```json
   {
     "jsonrpc": "2.0",
     "method": "_anureo.dev/skills/progress",
     "params": {
       "operationId": "install-rust-architecture",
       "progress": 45,
       "phase": "downloading",
       "message": "Downloading skill package...",
       "cancelable": true
     }
   }
   ```
5. **Agent 通知**: 安装成功后，Server 通过内部通道通知 Agent 的 skill registry 重新加载。但 skill 是否被 Agent 使用取决于 Agent 逻辑。
6. **并发**: 同一 skill 的并发安装请求需加锁，避免竞态。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillInstallSource {
    Registry,
    Git,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsInstallRequest {
    #[serde(default = "default_source")]
    pub source: SkillInstallSource,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skill_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default)]
    pub force: bool,
}

fn default_source() -> SkillInstallSource { SkillInstallSource::Registry }
fn default_version() -> String { "latest".to_string() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsInstallResponse {
    pub installed: bool,
    pub skill: SkillInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `skillId`/`url` 缺失，或 skill 不存在 |
| `Forbidden (-32603)` | Server-side authorization 拒绝 |
| `Internal Error (-32603)` | 下载失败、签名校验失败或写入失败（已回滚） |
| `Conflict (-32000)` | 已安装相同版本且 `force` 为 false |

---

### `_anureo.dev/skills/uninstall`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `skills.uninstall` |
| 权限 | Server-side authorization（`skills:write` scope）；建议 UI 确认 |

卸载 skill。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "_anureo.dev/skills/uninstall",
  "params": {
    "skillId": "rust-architecture"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `skillId` | string | 是 | 要卸载的 skill ID |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "uninstalled": true,
    "skillId": "rust-architecture"
  }
}
```

#### 逻辑说明

1. **活跃 session 不受影响**: 卸载不影响正在使用该 skill 的活跃 session。Session 保持已加载状态，直到 session 结束。新 session 不再加载此 skill。
2. **配置清理**: 卸载时删除 skill 文件和关联的配置数据。
3. **幂等**: 卸载不存在的 skill 返回 `uninstalled: false`，不报错。
4. **内置 skill**: 内置 skill 不可卸载，返回 `Forbidden`。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsUninstallRequest {
    pub skill_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsUninstallResponse {
    pub uninstalled: bool,
    pub skill_id: String,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `skillId` 不存在 |
| `Forbidden (-32603)` | 内置 skill 不可卸载或 authorization 拒绝 |

---

### `_anureo.dev/skills/configure`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `skills.configure` |
| 权限 | 无 |

配置 skill 参数。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "_anureo.dev/skills/configure",
  "params": {
    "skillId": "rust-architecture",
    "config": {
      "strictMode": true,
      "maxFileLines": 500
    }
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `skillId` | string | 是 | 要配置的 skill ID |
| `config` | object | 是 | 配置值（增量 merge，不覆盖未传入的字段） |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "configured": true,
    "skillId": "rust-architecture",
    "config": {
      "strictMode": true,
      "maxFileLines": 500
    }
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `configured` | bool | 是否配置成功 |
| `config` | object | 合并后的完整配置 |

#### 逻辑说明

1. **增量 merge**: `config` 中的字段与现有配置 merge，未传入的字段保持不变。传入 `null` 可删除字段。
2. **Schema 校验**: Server 根据 `configSchema` 校验配置值。不符合 schema 的字段返回 `Invalid Params`。
3. **Agent 通知**: 配置变更后 Server 通过内部通道通知 Agent。Agent 决定是否需要重新加载 skill。
4. **持久化**: 配置持久化到 server 端存储，跨连接有效。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfigureRequest {
    pub skill_id: String,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfigureResponse {
    pub configured: bool,
    pub skill_id: String,
    pub config: serde_json::Value,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `skillId` 不存在或 `config` 不符合 `configSchema` |
| `Internal Error (-32603)` | 配置持久化失败 |

---

## Notifications

### `_anureo.dev/skills/changed`

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/skills/changed",
  "params": {
    "change": "install",
    "skillId": "rust-architecture"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `change` | `"install"` \| `"uninstall"` \| `"update"` \| `"configure"` | 变更类型 |
| `skillId` | string | 受影响的 skill ID |

**触发场景：**
- `skills/install` 成功后
- `skills/uninstall` 成功后
- `skills/configure` 成功后
- 外部变更（用户在另一个 client 操作了同一 server）

**多 client 同步：** 所有连接到同一 server 的 client 都收到此 notification。Client 收到后调用 `skills/list` 获取最新列表。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `skills/changed` | `skills/list` | 完整已安装 skill 列表 |

> Client 重连后调用 `skills/list` 获取完整已安装 skill 快照。若 skill 在断连期间被安装/卸载，Client 通过 `skills/changed` + resync 感知。`skills/changed` notification 可能丢失（网络断开），resync 保证最终一致。

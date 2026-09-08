# anureo Projects 迁移到 anureo 扩展协议

> **状态**: 草案（待评审）
> **日期**: 2025-08-19
> **涉及仓库**: `anureo`（本仓库，后端）/ `anureo-feat-dev`（前端）
> **关联文档**: [docs/acp-spec/extensions/27-project-config.md](../acp-spec/extensions/27-project-config.md)

---

## 1. 背景与目标

anureo 前端的项目管理（多项目注册表、图标、排序、活跃切换）目前完全落在
localStorage + Zustand（`packages/ui/src/stores/useProjectsStore.ts`，1014 行），无法跨设备/跨客户端共享，且与后端会话体系脱节。

anureo ACP 扩展协议已有 `_anureo.dev/project/*` 域（`apps/acp/src/extensions/project.rs`），
但只实现了 `list` / `get` / `update` / `icon` 四个方法，且只有内存 store、通知为 no-op。

**目标**：补齐 anureo 侧 project 扩展（方法、持久化、广播、图标发现），anureo 前端把 projects 读写切到 anureo，localStorage 仅作首启迁移源。

**非目标**：
- 不做 opencode v2 HTTP `/project` 端点（anureo-kernel HTTP API 属另一条线，见 `opencode/specs/v2/anureo-kernel/protocols/http/instance/project.md`，字段冲突留待那条线统一）
- 不做项目级权限/多用户
- `sessionCount` 首版恒为 0

---

## 2. 现状盘点

### 2.1 anureo 侧（`apps/acp/src/extensions/project.rs`）

| 项 | 现状 | 问题 |
|---|---|---|
| 方法 | `list` / `get` / `update` / `icon` | 缺 `create` / `remove` / `reorder` / `touch` / `icon discover` |
| Store | `MemoryProjectStore`（重启即失） | 需要文件持久化 |
| 通知 | `PROJECT_CHANGED_METHOD` 已定义，但 `DefaultNotifier` 是 no-op（project.rs:222） | 需接入 hub 广播 |
| 注册 | `register.rs:30` 用 `ProjectHandler::default()` | 需换成 `with_dependencies(...)` 接真 store/notifier |
| 文档 | `27-project-config.md` 标记 ❌ 未实现 | 文档 stale，需更新 |

已有的能力（可直接复用）：图标 base64 校验 + SVG 消毒（`validate_icon`，256KB 上限）、
`#RRGGBB` 颜色校验、secret 脱敏（`redact_value`）、分页（`pagination.rs`）、
路径规范化（`server_path`）、目录边界校验（`boundary::validate_path`）。

### 2.2 anureo 侧（`useProjectsStore.ts` + `lib/api/types.ts`）

`ProjectEntry`：

```ts
{ id, path, label?, icon?, iconImage?: { mime, updatedAt, source: 'custom'|'auto' } | null,
  iconBackground?, color?, defaultModel?, addedAt?, lastOpenedAt?, sidebarCollapsed? }
```

Store 动作：`addProject` / `removeProject` / `setActiveProject` / `updateProjectMeta` /
`updateProjectIcon` / `removeProjectIcon` / `discoverProjectIcon` / `reorderProjects`。
持久化：localStorage（projects + manualOrder + activeProjectId）+ `updateDesktopSettings` 桌面同步。
VS Code runtime 下 projects 由 workspace folders 派生，不落盘。

---

## 3. 契约设计

### 3.1 方法总表（`_anureo.dev/project/*`）

| 方法 | 方向 | 现有/新增 | MVP | 说明 |
|---|---|---|---|---|
| `list` | req | 现有 | ✅ | 分页 |
| `get` | req | 现有 | ✅ | item + config（脱敏） |
| `create` | req | **新增** | ✅ | 注册项目（幂等 by path） |
| `remove` | req | **新增** | ✅ | 注销项目 |
| `update` | req | 现有 | ✅ | 名称/颜色/config/新 UI 字段 |
| `icon` | req | 现有 | ✅ | set/replace/remove |
| `reorder` | req | **新增** | 可选 | 全量 id 顺序 |
| `touch` | req | **新增** | 可选 | 更新 lastOpenedAt |
| `icon/discover` | req | **新增** | 可选 | 扫描项目目录发现图标 |
| `changed` | ntf | 现有（未接线） | 可选 | 广播，排除发起连接 |

### 3.2 字段映射（anureo ↔ anureo）

| anureo `ProjectEntry` | anureo `ProjectItem` / `ProjectConfig` | 处理 |
|---|---|---|
| `id`（path 派生） | `id` | create 可带 preferredId，迁移零重映射 |
| `path` | `path` | server 规范化为绝对路径（`server_path`） |
| `label` | `name` | 前端适配层改名 |
| `icon`（内置名或 null） | `icon`（`"none"` / 内置名） | null ↔ `"none"` 适配 |
| `iconImage` | **新增** `icon_image: { mime, updatedAt, source }` | item 层新字段，与 iconUrl 并存 |
| `iconBackground` | **新增** `icon_background` | 复用 `valid_color` 校验 |
| `color` | `color` | 一致 |
| `defaultModel` | `config.defaultModel` | item 顶层新增只读镜像 `default_model`，update 走 config |
| `addedAt` | `createdAt` | 前端转换 epoch ms ↔ ISO 8601 |
| `lastOpenedAt` | `lastOpenedAt` | 同上 |
| `sidebarCollapsed` | **新增** `sidebar_collapsed` | UI 状态随项目存 |
| — | `description` / `agentProfile` / `mcpServers` | 前端暂不展示，保留 |
| — | `isActive` | 弃用：活跃项目由前端持有，`touch` 只更新时间戳 |

**决定**：时间戳统一 ISO 8601（server 侧已是 `DateTime<Utc>`），前端适配层做 epoch 转换。

### 3.3 新方法契约

#### `create`

```jsonc
// request
{ "path": "C:\\Users\\heycj\\dev\\anureo", "preferredId": "dev-anureo", "name": "anureo",
  "color": "#4A90D9" /* 可选 */ }
// response: 与 get 相同的 snapshot（item + config）
```

- path 必填，经 `server_path` 规范化后作为唯一键：同 path 已存在 → 直接返回已有记录（幂等，`existed: true`）
- `preferredId` 可选：非空、≤128、`[A-Za-z0-9._-]`；被占用且属其它 path → `-32005 conflict`（`ExtensionError::conflict`；注意 project.rs 现有手写 `conflict` 用了 -32003，与 mod.rs 构造器 -32005 不一致，实现时统一走构造器）
- 无 preferredId 时 server 生成：`proj-<路径 hash 10 位>`

#### `remove`

```jsonc
{ "id": "proj_001" }
// response: { "removed": true, "id": "proj_001" }
```

- 同时从 manualOrder 中移除；不存在 → `-32003 not_found`（`ExtensionError::not_found`；project.rs 现有手写 `not_found` 用 -32004，实现时统一）

#### `reorder`

```jsonc
{ "ids": ["proj_002", "proj_001", "proj_003"] }
// response: { "orderedIds": [...] }
```

- 全量数组；缺漏/含未知 id → `-32602`；server 补齐未列出的 id 置于末尾

#### `touch`

```jsonc
{ "id": "proj_001" }   // response: { "lastOpenedAt": "2025-08-19T10:00:00Z" }
```

#### `icon/discover`

```jsonc
{ "id": "proj_001", "force": false }
// response: { "ok": true, "found": true, "reason": "logo.png",
//             "icon": "custom", "iconUrl": "data:image/png;base64,...",
//             "iconImage": { "mime": "image/png", "updatedAt": 1234567890, "source": "auto" } }
// 未找到: { "ok": true, "found": false, "reason": "no candidate in [root, public, assets]" }
```

扫描规则（按优先级，首个命中即用）：

1. 候选目录：项目根、`public/`、`assets/`、`static/`、`resources/`
2. 候选文件名：`logo.svg` > `logo.png` > `logo.ico` > `icon.svg` > `icon.png` > `app-icon.png` > `favicon.ico` > `favicon.svg` > `favicon.png`
3. 移动端：`ios/App/App/Assets.xcassets/AppIcon.appiconset/` 下首个 png、`android/app/src/main/res/mipmap-xxxhdpi/ic_launcher.png`
4. 约束：≤256KB、MIME ∈ {png, jpeg, svg}（ico 按 png 处理）、SVG 走既有 `validate_icon` 消毒
5. 已有 `source: "custom"` 且 `force: false` → `{ ok: true, found: false, reason: "custom icon present" }`

#### `changed` 通知（接线后语义）

```jsonc
{ "jsonrpc": "2.0", "method": "_anureo.dev/project/changed",
  "params": { "change": "created|updated|removed|icon_changed|reordered", "id": "proj_001" } }
```

广播给除发起连接（`ctx.connection_id`）外的所有已认证连接 —— 与 `command/changed`、`snippet/changed` 一致。

### 3.4 Capability 声明扩展

```json
{ "project": { "list": true, "get": true, "create": true, "remove": true, "update": true,
               "icon": true, "icon/discover": true, "reorder": true, "touch": true } }
```

---

## 4. 后端实现设计（anureo 仓库）

### 4.1 持久化：`FileProjectStore`

- 路径：`anureo_home()/projects.json`（全局注册表，跨项目共享 —— **不**用 `wd/.anureo/`，那是 per-project 数据的位置）。MVP 图标内嵌 base64（沿用现有 256KB 上限）；外置文件模式见 backlog
- 结构：

```jsonc
{
  "version": 1,
  "records": { "<id>": { "item": {...}, "config": {...} } },
  "manualOrder": ["proj_002", "proj_001"],
  "activeHint": null   // 预留，首版不写入
}
```

- 读写模式照抄 `scheduled_task.rs:97-165`（`load_store` / `save_store`），增加原子写：先写 `projects.json.tmp` 再 `rename`
- `ProjectStore` trait 需扩为 `insert` / `remove` / `reorder` / `load_all`（当前只有 list/get/update）
- store 内部仍用 `Mutex<BTreeMap>`；handler 已有 `operation_lock` 串行化写操作

### 4.2 通知接线（可选，见 backlog）

- 新增 `HubProjectNotifier`：实现 `ProjectNotifier::publish(change, id, excluded_connection)`，向 hub 发送 §3.3 的 `changed` 通知
- 接线点：`register.rs:30` 改为 `ProjectHandler::with_dependencies(Arc<FileProjectStore>, authorizer, Arc<HubProjectNotifier>)`
- hub 侧复用现有 broadcast 通道（与 `command/changed` 相同的推送路径）；`route_failures` 计数顺带覆盖

### 4.3 `ProjectItem` 字段扩展

新增（均 `#[serde(default)]` 向后兼容）：
`icon_image: Option<IconImage>`、`icon_background: Option<String>`、`sidebar_collapsed: bool`、
`default_model: Option<String>`（写路径仍走 `config.default_model`，save 时同步镜像）。

### 4.4 `create`/`remove` 鉴权

沿用 `DefaultAuthorizer`（principal + session 非空）——与 `update`/`icon` 同级。

### 4.5 改动文件清单

| 文件 | 改动 |
|---|---|
| `apps/acp/src/extensions/project.rs` | 字段扩展、新方法、`FileProjectStore`、icon discover |
| `apps/acp/src/extensions/register.rs` | 接线 with_dependencies |
| `docs/acp-spec/extensions/27-project-config.md` | 状态 ✅ + 新方法契约 |

---

## 5. 前端改造设计（anureo 仓库）

### 5.1 API client：`packages/ui/src/lib/api/projects.ts`

封装 `_anureo.dev/project/*` 请求/通知订阅（复用现有 ACP WS 通道），暴露：
`listProjects` / `createProject` / `removeProject` / `updateProject` / `setProjectIcon` /
`discoverProjectIcon` / `reorderProjects` / `touchProject` / `onProjectChanged`。

### 5.2 `useProjectsStore` 适配

- **数据源**：连接建立后 `list()` 拉全量；`onProjectChanged` 增量刷新（received on其它连接时）
- **写穿**：所有 mutation 调 server，成功后用返回值更新本地 state（不再直接 `persistProjects`）
- **activeProjectId**：保留前端 localStorage（UI 会话态，不下沉 server）
- **迁移**：首启检测 server 返回空 + localStorage 非空 → 逐条 `create`（带 preferredId + label/color/iconBackground/defaultModel/sidebarCollapsed + icon data URL），最后 `reorder` 同步 manualOrder；迁移成功后清 localStorage 的 projects 键
- **降级**：ACP 未连接时只读展示 + 提示，禁止本地静默写（避免双写分歧）
- **VS Code runtime**：保持 workspace 派生分支，不接 server

---

## 6. 实施计划

### Phase 0 — 后端 MVP（先行）

1. `ProjectItem` 字段扩展 + `update` 支持新字段（含校验）
2. `FileProjectStore`（原子写 + trait 扩展 `insert`/`remove`）+ 单测：持久化往返、损坏文件回退
3. `create` / `remove` + 单测：幂等、conflict、not_found
4. `27-project-config.md` 补新方法契约（增量，不全量重写）

**验收**：`cargo test` 全绿；`cargo clippy --workspace --all-targets -- -D warnings` 零警告。

### Phase 0.5 — 前端 MVP（anureo）

1. `lib/api/projects.ts` client 子集：`list`/`create`/`remove`/`update`/`icon`
2. `useProjectsStore` 写穿改造（对外签名不变，组件零改动）
3. 基础迁移：localStorage 逐条 `create`（id/path/label/color/iconBackground/defaultModel/sidebarCollapsed），不含图标、不含顺序

**验收**：添加/删除/重命名/换色/重启后数据保留；上传小图标（≤256KB）可用。

### 可选 backlog（按优先级，任一可独立补加）

| 项 | 推迟的代价 | 补加成本 |
|---|---|---|
| `reorder` + `touch` | 无手工排序；"最近打开"失效（lastOpenedAt 停在创建时刻） | 1 方法 + 1 数组字段 |
| `icon/discover` | 图标需手动上传 | 照搬 OC 规则：favicon 全搜 + 最浅路径优先（`project-icon-routes.js:319`） |
| Hub 广播接线 | 仅单客户端无影响（写穿返回值即刷新） | `with_dependencies` 换 notifier，半日 |
| 图标外置文件 + 5MB | >256KB 上传被拒（真实 logo 多为 svg/小 png，概率低） | 对齐 OC：外置 `project-icons/` + 仅存元数据 |
| 图标数据迁移 | 迁移后图标手动重传 | 迁移流程补逐个 `icon` 调用 |
| 断线只读降级 | 未连接时转圈 | 后补 |
| 错误码统一 | 无 | 顺手 |

非目标维持：`sessionCount`（恒 0）、`activeHint`。

---

## 7. 风险与决策点

| 风险/决策 | 处理 |
|---|---|
| projects.json 内嵌 base64 图标（每枚 ≤~350KB 文本） | MVP 接受；backlog 外置文件 |
| 前端旧 id 与 server 生成规则不同 | create 支持 preferredId，迁移零重映射 |
| localStorage 与 server 双写分歧 | 迁移后前端不再本地写；断线只读 |
| 多客户端同时 reorder 冲突 | last-write-wins + changed 通知收敛；无 OT 需求 |
| `deny_unknown_fields` 与新字段 | item 端 struct 新字段全部 `#[serde(default)]`；request struct 保持 deny 以尽早暴露契约错误 |
| anureo-kernel HTTP `/project` 未来统一 | 本方案只动 ACP 扩展层；HTTP 层字段冲突留给那条线（`Project.Info` 的 worktree/vcs/commands 未纳入） |

---

## 8. 测试矩阵（后端）

| 用例 | 断言 |
|---|---|
| create 幂等（同 path 两次） | 第二次返回同 id + `existed: true` |
| create preferredId 被占 | `-32005` |
| remove 后 list | 不含该 id；manualOrder 已剔除 |
| reorder 缺 id（可选方法） | `-32602`；server 补齐语义仅在全量通过校验后生效 |
| 错误码统一 | project.rs 手写 not_found(-32004)/conflict(-32003) 改用 mod.rs 构造器 -32003/-32005 |
| icon/discover force=false 且 custom 存在 | `found: false, reason: "custom icon present"` |
| icon/discover svg 含 `<script>` | `-32602 unsafe SVG data` |
| 重启（drop store 重建） | 数据完整（FileProjectStore 往返） |
| 双连接 update | 另一连接收到 `changed`，发起方不收 |
| config env 含 TOKEN | get 响应中被脱敏为 `****` |

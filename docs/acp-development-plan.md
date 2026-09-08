# ACP 对接开发计划

> **基于**: `anureo-feat-dev/docs/acp/10-unified-roadmap.md`（统一 Roadmap）
> **规范**: `docs/acp-spec/`（9 份核心 + 26 份扩展）
> **后端**: anureo `apps/acp/` + `apps/server/`（ACP v1 已实现）
> **前端**: anureo `packages/ui/` + `packages/web/server/`
> **原则**: 不使用 Mock，所有开发直连真实 anureo ACP Agent

---

## 1. anureo 后端实现状态（已验证）

> **重要修正**: Roadmap §2.1 声称 `session/close`、`session/delete`、`session/resume` 未实现。经代码验证，**全部已实现**。下表为实际状态。

### 1.1 标准协议（✅ 已就绪）

| 能力 | 代码位置 | 状态 |
|---|---|---|
| `initialize` | `agent.rs:419-457` | ✅ 返回完整 agentInfo + capabilities + protocolVersion |
| `authenticate` | `agent.rs:460-465` | ✅ |
| `session/new` | `agent.rs` | ✅ |
| `session/load` | `agent.rs` | ✅ 含 history replay |
| `session/list` | `agent.rs:1561+` | ✅ 含 DB 持久化 + cursor 分页 |
| `session/fork` | `agent.rs:731-843` | ⚠️ Handler 已实现，**capability 未声明**（`agent.rs:436-440` 缺少 `.fork(...)`） |
| `session/prompt` | `agent.rs` | ✅ |
| `session/cancel` | `agent.rs` | ✅ 幂等 |
| `session/close` | `agent.rs:1511-1534` | ✅ 释放资源 + 持久化 lifecycle |
| `session/delete` | `agent.rs:1536-1559` | ✅ 删除 checkpoint + 持久化记录 |
| `session/resume` | `agent.rs:1470-1509` | ✅ 恢复 session + 校验 owner/cwd |
| `session/set_config_option` | `agent.rs:610` | ✅ |
| `session/set_mode` | `agent.rs:713` | ✅ |
| WebSocket `/acp` Bearer auth | `apps/server/src/handlers/acp.rs:81-97` | ✅ `ANUREO_AUTH_TOKEN` + Origin 校验 |
| WebSocket frame 约束 | `acp.rs:23` | ✅ 1 MiB 上限，text frame |
| Origin allowlist | `acp.rs:44-74` | ✅ `ANUREO_ACP_ALLOWED_ORIGINS` + localhost |
| stdio `anureo acp` | `stdio_loop.rs` | ✅ 全部 session 方法已注册 |

### 1.2 Stream Bridge（✅ 全部 10 种 variant 已映射）

| SessionUpdate variant | `stream_bridge.rs` 位置 |
|---|---|
| `UserMessageChunk` | :348-351 |
| `AgentMessageChunk` | :353-356 |
| `AgentThoughtChunk` | :358-361 |
| `ToolCall` (Pending) | :363-378 |
| `ToolCallUpdate` (Running/Success/Failure) | :380-422 |
| `SessionInfoUpdate` | :425+ |
| `Plan` | :330+ |
| `UsageUpdate` | :425+ |
| `CurrentModeUpdate` | mapped |
| `ConfigOptionUpdate` | mapped |

### 1.3 Reverse-RPC（✅ 已就绪）

| 能力 | 代码位置 |
|---|---|
| `fs/read_text_file` | `tools/fs_tools.rs` |
| `fs/write_text_file` | `tools/fs_tools.rs` |
| `terminal/create` | `client_methods.rs:61` + `tools/terminal_executor.rs` |
| `terminal/output` | `client_methods.rs:113` |
| `terminal/wait_for_exit` | `client_methods.rs` |
| `terminal/kill` | `client_methods.rs` |
| `terminal/release` | `client_methods.rs` |
| `session/request_permission` | `client_methods.rs` |

### 1.4 实际差距

| 差距 | 说明 | 阻塞 Phase |
|---|---|---|
| Fork capability 未声明 | `agent.rs:436` 缺少 `.fork(SessionForkCapabilities::new())` | 不阻塞（workaround: client 忽略 capability） |
| 连接状态机不显式 | 当前靠 WebSocket handler 隐式管理，无 `created→authenticated→initialized→active` 显式状态 | Phase 0（低优先级） |
| Relay `/acp` allowlist | 未实现 | Phase 5 |
| `_anureo.dev/*` 扩展 | 26 个扩展文件，33 域，全部未实现 | Phase 4 |
| WebSocket server-side reconnect 辅助 | 当前 client 驱动 reconnect；server 侧无额外逻辑 | 不阻塞 |

**结论**: 标准协议 surface **完全就绪**，无阻塞前端迁移的后端工作。Phase 0-3 的 anureo 侧工作仅限 fork capability 修复和联调 bug fix。

---

## 2. anureo 前端现状

| 检查项 | 状态 |
|---|---|
| `packages/ui/src/lib/acp/` | ❌ 不存在 |
| `packages/ui/src/sync/` | ❌ 不存在 |
| `packages/ui/src/lib/opencode/` | ❌ 不存在 |
| `@agentclientprotocol/sdk` 依赖 | ❌ 未安装 |
| 当前 SDK | `@opencode-ai/sdk@1.17.18` |
| `packages/web/server/lib/acp/` | ❌ 不存在 |
| `packages/web/server/lib/opencode/` | ✅ 存在 |
| 多端 App 组件 | ✅ `VSCodeApp.tsx`、`ElectronMiniChatApp.tsx`、`MobileApp.tsx` |

**结论**: 前端 ACP 对接**从零开始**。

---

## 3. Phase 总览

```
Phase 0: SDK 接入 + anureo 连通性
    │
Phase 1: Core Session Flow（vertical slice）
    │
Phase 2: Reverse-RPC（fs/terminal）
    │
Phase 3: 共享 UI 全面迁移
    │
Phase 4: Anureo Extensions（分批 P0→P1→P2）
    │
Phase 5: Transport Parity（stdio + Relay）
    │
Phase 6: 灰度 + 旧代码删除
```

| Phase | Exit Gate | 预估 | anureo 侧工作 |
|---|---|---|---|
| 0 | 前端通过 ACP SDK 连接 anureo `/acp`，initialize 成功 | 1–2 周 | fork capability 修复 |
| 1 | Web 端 new → prompt → stream → cancel 完整 turn | 2–3 周 | 联调 bug fix |
| 2 | tool call → fs/terminal reverse-RPC E2E | 2–3 周 | 联调 bug fix |
| 3 | 三端共享 ACP client，旧 SDK 仅存于 adapter | 3–4 周 | 无阻塞项 |
| 4 | P0 扩展三端可用；P1/P2 按 batch 交付 | 4–8 周 | **大量后端工作**（扩展实现） |
| 5 | stdio/WebSocket/Relay 三 Transport parity | 2–3 周 | Relay allowlist |
| 6 | Feature flag 关闭，旧 `opencode/*` 代码删除 | 2–3 周 | — |

**合计**: 16–26 周

---

## Phase 0: SDK 接入 + anureo 连通性

> **目标**: 前端通过官方 `@agentclientprotocol/sdk` 连接 anureo WebSocket `/acp`，完成 `initialize` 握手。

### anureo 侧

| # | 文件 | 工作内容 |
|---|---|---|
| L0-1 | `apps/acp/src/agent.rs:436-440` | **修复 fork capability**: 添加 `.fork(SessionForkCapabilities::new())` 到 `SessionCapabilities::new()` 链 |
| L0-2 | `apps/server/src/handlers/acp.rs` | 增加连接诊断日志（connect/disconnect/initialize/auth_fail），便于前端联调 |
| L0-3 | — | 配合前端导出 fixture：initialize response、capability snapshot |

### anureo 侧

| # | 文件 | 工作内容 |
|---|---|---|
| F0-1 | `packages/ui/package.json` | 添加 `@agentclientprotocol/sdk`（锁定版本，与 Rust crate 0.15.1 wire 兼容） |
| F0-2 | `packages/ui/src/lib/acp/acp-runtime.ts` | `AcpRuntime` 接口 + 工厂函数 |
| F0-3 | `packages/ui/src/lib/acp/websocket-stream.ts` | WebSocket stream adapter：open/close/error、text frame = 单 JSON-RPC、Bearer auth |
| F0-4 | `packages/ui/src/lib/acp/anureo-client.ts` | `client()` 创建和生命周期（官方 SDK） |
| F0-5 | `packages/ui/src/lib/acp/extensions.ts` | `_anureo.dev/*` TypeScript 类型（仅类型定义） |
| F0-6 | `packages/ui/src/lib/acp/fixtures/` | 从 anureo 实际 endpoint 导出 golden JSON |
| F0-7 | `packages/web/server/lib/acp/` | Server-side ACP runtime：anureo endpoint 配置、auth 映射 |

### 交叉依赖

```
L0-1 (fork capability) ──→ F0-6 (fixture 对齐)
L0-2 (诊断日志)         ──→ F0-3 (WebSocket stream 联调)
```

### Exit Gate

- [ ] 前端连接 anureo `/acp` WebSocket 成功
- [ ] `initialize` 成功并返回 capability snapshot
- [ ] 无效 token / 无效 Origin 返回明确错误
- [ ] WebSocket 断线后可重连
- [ ] TS 类型与 anureo Rust fixture 语义一致
- [ ] fork capability 出现在 initialize response 中

---

## Phase 1: Core Session Flow

> **目标**: Web 端完成完整 Agent prompt turn。

### anureo 侧

| # | 工作内容 |
|---|---|
| L1-1 | 配合前端验证 stream 完整性（10 种 SessionUpdate variant） |
| L1-2 | 配合前端验证 cancel 幂等性、并发 prompt 拒绝 |
| L1-3 | 修复前端联调中发现的 protocol/schema 不一致 |

> **注意**: Roadmap §5.2 声称需要实现 `session/close`、`session/delete`、`session/resume`、`set_config_option` 对齐——这些**全部已实现**（见 §1.1）。此 Phase 的 anureo 侧工作仅为联调验证。

### anureo 侧

| # | 文件 | 工作内容 |
|---|---|---|
| F1-1 | `packages/ui/src/lib/acp/acp-session-actions.ts` | session actions: create/load/list/resume/close/delete/fork |
| F1-2 | 同上 | `session/prompt`: 组装 text/file/agent content blocks |
| F1-3 | 同上 | `session/cancel` |
| F1-4 | `packages/ui/src/lib/acp/acp-event-reducer.ts` | SessionUpdate variants → Zustand state |
| F1-5 | `packages/ui/src/lib/acp/acp-bootstrap.ts` | initialize → session/list → session/load |
| F1-6 | `packages/ui/src/lib/acp/websocket-stream.ts` | reconnect: exponential backoff、online/visibility aware |
| F1-7 | — | E2E 验证脚本：连接真实 anureo，完整 prompt turn |

### Exit Gate

- [ ] Web 端 new → prompt → stream → cancel 完整流程
- [ ] streaming 内容（text、thought、tool_call、usage）正确显示
- [ ] 断线后 `session/load` / `session/resume` 恢复
- [ ] close/delete 不影响其他 session
- [ ] 并发 prompt 被拒绝
- [ ] cancel 幂等

---

## Phase 2: Reverse-RPC（FS / Terminal）

> **目标**: 打通 Agent → Client reverse-RPC 链路。

### anureo 侧

| # | 工作内容 |
|---|---|
| L2-1 | 配合验证 `fs/read_text_file`、`fs/write_text_file` workspace 边界 |
| L2-2 | 配合验证 terminal reverse-RPC 全链路（create → output → wait_for_exit → kill → release） |
| L2-3 | 修复联调中发现的问题 |

### anureo 侧

| # | 文件 | 工作内容 |
|---|---|---|
| F2-1 | `packages/ui/src/lib/acp/client-bridge.ts` | reverse-RPC bridge: 注册 fs/terminal handler |
| F2-2 | 同上 | `fs/read_text_file` → RuntimeAPIs filesystem read |
| F2-3 | 同上 | `fs/write_text_file` → RuntimeAPIs filesystem write |
| F2-4 | 同上 | `terminal/*` → `packages/web/server/lib/terminal/runtime.js` |
| F2-5 | — | E2E: tool call → fs/terminal → success/failure |

### Exit Gate

- [ ] tool call → fs/terminal 操作成功返回
- [ ] 文件操作受 workspace/path policy 约束
- [ ] terminal lifecycle 完整

---

## Phase 3: 共享 UI 全面迁移

> **目标**: 三端共用同一 ACP client、reducer、bootstrap。

### anureo 侧

无阻塞项。配合修复前端联调中发现的 protocol/schema 问题。

### anureo 侧

| # | 文件 | 工作内容 |
|---|---|---|
| F3-1 | `packages/ui/src/sync/sync-context.tsx` | `sdk` prop → `acpClient`；初始化 ACP notification handler |
| F3-2 | `packages/ui/src/sync/bootstrap.ts` | 拆分: ACP bootstrap + runtime bootstrap |
| F3-3 | `packages/ui/src/sync/session-actions.ts` | 逐方法替换为 ACP actions（dual-run via flag） |
| F3-4 | `packages/ui/src/sync/event-pipeline.ts` | 移除旧 SSE，改为 ACP notification dispatch |
| F3-5 | `packages/ui/src/sync/event-reducer.ts` | 替换为 ACP update reducer |
| F3-6 | `packages/ui/src/sync/session-event-router.ts` | ACP session update 替代旧事件 |
| F3-7 | `packages/ui/src/sync/global-session-status.ts` | ACP live state 替代历史推断 |
| F3-8 | `packages/ui/src/apps/VSCodeApp.tsx` | 注入 ACP client |
| F3-9 | `packages/ui/src/apps/ElectronMiniChatApp.tsx` | 注入 ACP client |
| F3-10 | `packages/ui/src/lib/opencode/client.ts` | 退化为 facade（Phase 6 删除） |
| F3-11 | `packages/ui/src/components/chat/ChatInput.tsx` | session/prompt + cancel + directory |
| F3-12 | `packages/web/server/index.js` | 注册 ACP route、graceful shutdown |
| F3-13 | `packages/vscode/webview/main.tsx` | ACP runtime bridge |

### 性能保护（必须保留）

- delta coalescing（`session/update` ~60/sec 时不逐条 re-render）
- no-op skip（相同 role/finish/timestamp 不触发 state 更新）
- item identity preservation
- 窄订阅 store
- message row React.memo comparator

### Exit Gate

- [ ] 三端共享 ACP client 运行
- [ ] session create/prompt/stream/cancel/reload 在三端可用
- [ ] store/reducer 的 dedupe、ordering、引用稳定性验证
- [ ] bootstrap 失败时保留已有 session 列表
- [ ] 旧 `opencode/client.ts` 仅作为 adapter
- [ ] 高频 streaming 下无渲染级联

---

## Phase 4: Anureo Extensions

> **目标**: 分批实现 `_anureo.dev/*` 扩展协议。这是工作量最大的 Phase，需要 **anureo 后端 + anureo 前端同步推进**。

### 扩展框架（前置）

| # | 仓库 | 文件 | 工作内容 |
|---|---|---|---|
| E-0 | anureo | `apps/acp/src/extensions/mod.rs` | `ExtensionRegistry`、`ExtensionHandler` trait、dispatch |
| E-1 | anureo | `apps/acp/src/extensions/capability.rs` | `CapabilityManager`、capability snapshot 管理 |
| E-2 | anureo | `apps/acp/src/extensions/pagination.rs` | 统一 cursor 分页（见 `08-cross-cutting-patterns.md` §1） |
| E-3 | anureo | `apps/acp/src/extensions/progress.rs` | 长时操作进度上报（§3） |
| E-4 | anureo | `apps/acp/src/extensions/auth.rs` | 扩展权限三层 gate（§2） |
| E-5 | anureo | `apps/acp/src/extensions/boundary.rs` | 目录/worktree 边界校验 |

### Batch P0：Files + Git 读取 + Worktree 读取

#### anureo 侧

| # | 扩展域 | 方法 | 规范 |
|---|---|---|---|
| L4P0-1 | files | list/search/stat | `extensions/12-files.md` |
| L4P0-2 | git | status/diff/branches | `extensions/11-git.md` |
| L4P0-3 | worktree | list/get | `extensions/10-worktree.md` |
| L4P0-4 | initialize | 声明上述扩展 capability | — |
| L4P0-5 | auth | directory/worktree 边界校验 | `08-cross-cutting-patterns.md` §2 |

#### anureo 侧

| # | 工作内容 |
|---|---|
| F4P0-1 | `extensions.ts` 实现 files/git/worktree 方法和 capability 类型 |
| F4P0-2 | `SidebarFilesTree.tsx` → `_anureo.dev/files/list` |
| F4P0-3 | Git UI → `_anureo.dev/git/status` |
| F4P0-4 | Worktree selector → `_anureo.dev/worktree/list` |

#### Exit Gate

- [ ] capability 声明缺失时 UI 隐藏对应入口
- [ ] directory/worktree 边界被 server-side 校验
- [ ] partial failure 返回 per-item result

### Batch P1：Git 写操作 + Worktree 写操作 + MCP

| # | anureo 侧 | anureo 侧 |
|---|---|---|
| 1 | `_anureo.dev/git/commit`、`/git/push`、`/git/pull` | Git UI 写操作迁移 |
| 2 | `_anureo.dev/worktree/create`、`/worktree/delete` | Worktree CRUD UI 迁移 |
| 3 | `_anureo.dev/mcp/list`、`/mcp/get`、`/mcp/status` | MCP 管理页面迁移 |

### Batch P2：Goal + Scheduled Task + Connection/Relay

| # | anureo 侧 | anureo 侧 |
|---|---|---|
| 1 | `_anureo.dev/goal/*` (6 方法) | Goal UI 迁移 |
| 2 | `_anureo.dev/scheduled-task/*` | Scheduled task UI 迁移 |
| 3 | `_anureo.dev/connection/*`、`/relay/*`、`/pairing/*` | Relay/pairing UI 迁移 |
| 4 | `_anureo.dev/question/*`（仅标准 elicitation 不足时） | Question dialog 迁移 |

### Batch P3+：剩余扩展域

按 `08-cross-cutting-patterns.md` §9 模块结构逐步实现：
session-folder、snippet、command、plugin、quota、agent、diagnostics、project、tunnel、multi-run、settings、session-assist、small-model、auto-review、preview、terminal、tts、dictation、notification、github、client-auth

### 每个 Extension 的 Definition of Done

```
□ method/params/result/error schema 已登记到 docs/acp-spec/extensions/
□ capability 可细化到 method 级别
□ server-side authorization 已实现（三层 gate）
□ directory/worktree 边界已校验
□ retry/timeout/idempotency 已定义
□ notification 有 authoritative resync method
□ Web/Desktop/VS Code UI gating 已完成
□ cursor 分页（list 方法）
□ 进度上报（长时操作）
□ partial failure 有 per-item result
□ 安全：secret/token/敏感路径不出现在 response 中
□ parity test（stdio/WS/Relay）
```

---

## Phase 5: Transport Parity（stdio + Relay）

> **目标**: 三种 Transport 对同一组 tests 语义完全一致。

### stdio

| # | 仓库 | 工作内容 |
|---|---|---|
| T5-S1 | anureo | 验证 `anureo acp` stdio framing、auto-spawn、session persistence |
| T5-S2 | anureo | `packages/ui/src/lib/acp/stdio-stream.ts`: native host stdio adapter |
| T5-S3 | anureo | Desktop/Electron 集成 stdio transport |

### Relay

| # | 仓库 | 工作内容 |
|---|---|---|
| T5-R1 | anureo | `/acp` 加入 Relay WebSocket allowlist |
| T5-R2 | anureo | `relay-stream.ts`: Relay-backed ACP stream |
| T5-R3 | anureo | direct/tunnel socket 共用 ACP client |
| T5-R4 | anureo | E2EE handshake 后才发 initialize |
| T5-R5 | anureo | Relay 断线只触发 transport reconnect |
| T5-R6 | — | 验证 relay 看不到 ACP 明文 |

### Parity Test Matrix

```
                    stdio    WebSocket    Relay
initialize            ✅         ✅         ✅
session/new           ✅         ✅         ✅
session/load          ✅         ✅         ✅
session/prompt        ✅         ✅         ✅
session/cancel        ✅         ✅         ✅
session/update        ✅         ✅         ✅
request_permission    ✅         ✅         ✅
fs/read_text_file     ✅         ✅         ✅
fs/write_text_file    ✅         ✅         ✅
terminal/create       ✅         ✅         ✅
extension: git/status ✅         ✅         ✅
extension: files/list ✅         ✅         ✅
reconnect             ✅         ✅         ✅
```

### Exit Gate

- [ ] 三 Transport 通过同一组 tests
- [ ] response/update 语义完全一致
- [ ] reconnect/restart/expired-auth 场景通过

---

## Phase 6: 灰度 + 旧代码删除

### Feature Flags

```bash
ANUREO_ACP_ENABLED=false
ANUREO_ACP_UI_MODE=legacy|shadow|primary
ANUREO_ACP_TRANSPORT=ws|stdio|relay
```

### 灰度顺序

1. internal Web users (shadow)
2. Desktop direct WebSocket (primary + fallback)
3. VS Code (primary)
4. Relay clients (Mobile)
5. external standard ACP clients
6. ACP primary by default（删除 fallback）

### 删除旧路径的 Gate

```
□ 三端 core flow smoke test（7 天无 regression）
□ 三 Transport parity 通过
□ terminal/filesystem 无数据丢失
□ reconnect/restart/expired-auth 通过
□ extension contract tests 通过
□ legacy fallback 未被依赖
□ rollback 不需要数据迁移
□ 引用扫描确认无残留 @opencode-ai/sdk
```

### 删除清单

| 文件/目录 | 操作 |
|---|---|
| `packages/ui/src/lib/opencode/client.ts` | 删除 |
| `packages/ui/src/sync/event-pipeline.ts`（旧 SSE 部分） | 删除 |
| `packages/web/server/lib/opencode/` | 删除 |
| `packages/ui/package.json` `@opencode-ai/sdk` | 移除 |
| `packages/web/package.json` `@opencode-ai/sdk` | 移除 |

---

## 交叉依赖图

```
Phase 0
  L0-1 fork capability ──────→ F0-6 fixture 对齐
  L0-2 诊断日志 ───────────────→ F0-3 WS stream 联调
      │
Phase 1
  L1-* 联调验证 ──────────────→ F1-* session actions + reducer
      │
Phase 2
  L2-* 联调验证 ──────────────→ F2-* client-bridge
      │
Phase 3（anureo 无阻塞项）
  F3-1~7 sync 层迁移
  F3-8~13 三端注入 + 组件改造
      │
Phase 4
  E-0~5 扩展框架 ─────────────→ F4P0-* extension UI
  L4P0-* P0 扩展实现 ─────────→ F4P0-* P0 UI 迁移
  L4P1-* P1 扩展实现 ─────────→ F4P1-* P1 UI 迁移
  L4P2-* P2 扩展实现 ─────────→ F4P2-* P2 UI 迁移
      │
Phase 5
  T5-S1 stdio ────────────────→ T5-S2~3 stdio adapter
  T5-R1 relay allowlist ──────→ T5-R2~6 relay adapter
      │
Phase 6
  删除旧代码 ← 所有 Gate 通过
```

---

## 风险与应对

| # | 风险 | 影响 | 概率 | 应对 |
|---|---|---|---|---|
| R1 | anureo stream event 与 UI 需求不一致 | stream 内容丢失或状态错误 | 中 | 以 `stream_bridge.rs` 为基线建立映射表；Phase 1 验证 |
| R2 | 高速 delta 导致 UI 重渲染 | 性能下降，60fps 无法维持 | 高 | delta coalescing、窄订阅 store、message row memo comparator；Phase 3 压测 |
| R3 | SDK 与 anureo 版本不兼容 | wire 行为不匹配 | 中 | Phase 0 compatibility check；每次 SDK 升级重新验证 |
| R4 | capability snapshot 过期 | UI 展示错误入口 | 中 | 每次 initialize/reconnect 重建 snapshot；capability_changed notification |
| R5 | 空响应覆盖本地状态 | session、permission 丢失 | 中 | authoritative API 用 throw/null 区分 fetch failure vs empty success |
| R6 | Relay 与新 WebSocket endpoint 不兼容 | 远程设备功能失效 | 低 | 所有 realtime endpoint 通过 parity test |
| R7 | 扩展实现工作量（anureo + 前端）大 | Phase 4 延期 | 高 | P0/P1/P2 分批；先核心域后辅助域；capability-gated 隐藏未实现域 |
| R8 | 旧代码删除过早 | 不可回滚的 regression | 低 | 灰度 + shadow mode + rollback gate |
| R9 | 直连 anureo 开发环境不稳定 | 开发效率下降 | 中 | 保持 `anureo serve` 稳定运行；增加连接诊断日志 |

---

## 完成标准

- [ ] 所有 Agent-facing 操作均可在代码中定位到 ACP method
- [ ] anureo 使用官方 ACP SDK，而非自研 ACP core
- [ ] `SyncProvider` 不再依赖旧 Agent SDK client
- [ ] Agent stream 只由 `session/update` 驱动
- [ ] fs/terminal reverse-RPC 在三端有明确 bridge
- [ ] direct WebSocket、Relay、stdio 通过同一协议 fixture
- [ ] bootstrap/reconnect 能区分失败和空成功
- [ ] Web/Desktop/VS Code/Mobile 使用同一 ACP client 和 reducer
- [ ] Anureo extensions 完成 capability gating 和权限控制
- [ ] `@opencode-ai/sdk` 依赖完全移除
- [ ] 全部测试和 rollback gate 通过

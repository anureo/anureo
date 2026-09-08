# ACP WebSocket 持久化实现方案

> **状态**: Phase A（连接模型与隔离）已完成——子进程桥接已替换为进程内分发。Phase B–E 待实施。
> **相关代码**: `apps/server/src/handlers/acp.rs`、`apps/server/src/acp_hub.rs`、`apps/acp/src/tools/client_bridge.rs`
> **交叉参考**: [ACP WebSocket 后续工作](./acp-websocket-todo.md)、[ACP WebSocket 接入文档](./acp-websocket.md)

---

## 1. 目标与边界

将 `GET /acp` 从“每条 WebSocket 连接启动一个 `anureo acp` 子进程”的临时桥接，替换为 server 进程内、可重连的 ACP transport。

本轮完成后，单一逻辑 CLI 在断线后重新连接，或在 server 重启后重新启动，均能够加载原 ACP session、继续使用原 thread 与配置，并收到可恢复的最终状态。实现范围只覆盖 loopback / 单主体的正确性基础；多主体隔离、配额和完整运营指标放入后续加固阶段。

不做：变更标准 ACP wire contract、自动批准权限、将 server 暴露到公网、同时实施 OpenCode HTTP/SSE 的无关重构。

## 2. 当前事实与关键决策

| 维度 | 当前实现 | 本方案决定 |
| --- | --- | --- |
| `/acp` | ~~spawn `anureo acp` 子进程，断线即退出~~ → **已改为进程内分发** | ✅ 直接从 `AppState.acp_hub` 获取持久 agent |
| 进程模型 | ~~WebSocket 连接拥有 agent 子进程~~ → **已消除** | ✅ ACP handler 不 spawn 任何 `anureo`/ACP 子进程；一个 server 进程内管理所有 ACP session |
| session 状态 | 跟随子进程连接生命周期 | `AcpHub` 持有 session/run 状态 |
| server 重启 | 内存状态丢失 | session 元数据与可恢复 run/checkpoint 状态写入持久 store；启动时恢复为可 load/continue 的 session |
| client bridge | `GLOBAL_BRIDGE` 进程级单例 | 每个 ACP connection/session 显式注入 bridge |
| prompt 并发 | 未定义 | 每 session 串行；冲突时返回 JSON-RPC server error |
| 断线 | 子进程被杀，更新丢失 | 默认 persist；有界 replay buffer；显式 cancel |
| 权限 | 连接级反向 RPC | 绑定 session、run、connection generation 与 TTL |

## 3. 分阶段实现

### Phase A — 连接模型与隔离（P0）✅ 已完成

1. ~~定义 `AcpConnection`：保存 connection id、认证主体（当前可为 local anonymous）、capabilities、notification sink、connection generation 和 client bridge。~~
2. ~~将 `ClientBridge` 从 `GLOBAL_BRIDGE` 移为 session/run 构建参数；fs 与 terminal tool 必须按 session 查找当前 connection bridge。~~
3. ~~将 `/acp` handler 改为：upgrade → 初始化 connection → `AcpHub::attach` → 以 SDK connection dispatch ACP 请求 → 任务结束时 detach；删除子进程、stdio line bridge 和 `ANUREO_ACP_BINARY` 依赖。~~
   - **实现**：`handlers/acp.rs` 通过 `AcpHub::attach()` 获取持久 agent，构建 WS ↔ `Lines` transport 适配器，调用 `anureo_acp::run_agent_connection()` 驱动 ACP JSON-RPC dispatch。
   - **核心函数**：`stdio_loop.rs::run_agent_connection()` 提取为公共 API，接受任意 `Lines` 传输层（stdin 或 WebSocket），内含全部 handler 注册 + notification drain task + `connect_with` 驱动。
   - **测试**：`acp_ws_mega_e2e.rs` 覆盖 initialize → session/new → disconnect → reconnect → session/load 全链路（通过）。
4. 为两个并发 WebSocket client 增加 fs/terminal 请求隔离测试。 _(待做)_

验收：第二条连接的 initialize、断开或 capability 更新不会影响第一条连接的工具 RPC 目标。 _(部分满足：单连接 e2e 通过；多连接隔离测试待补)_

### Phase B — 持久 session 与 prompt 串行（P0）

1. 让 `AcpHub` 以 session id 索引 `SessionRuntime`，保存 thread id、cwd、MCP 配置、model/mode、active cancellation、owner 与当前 bridge generation。
2. 将 `anureoAcpAgent` 分解为无连接的 session/run core 与连接专属 adapter；新建、加载、prompt、cancel 都经由 hub。
3. 每个 session 采用 actor 或 mutex-backed command queue；正在 prompt 时的第二个 prompt 返回稳定的 JSON-RPC error code/message。
4. 保留 `session/cancel` 为唯一立即取消路径。
5. 新增 `AcpSessionStore`（复用现有 SQLite/checkpoint 基础设施优先），原子持久化 session runtime 元数据、最后 checkpoint/thread、run 状态与可恢复错误；`AcpHub` 初始化时加载这些记录。
6. server 重启时：已完成/已取消 session 可立即 `session/load` 并继续 prompt；重启中断的 active run 标记为 `interrupted_by_restart`，保留 checkpoint 与诊断信息，不伪造“仍在运行”。

验收：同一 session reconnect 或 server 重启后 `session/load` 与后续 prompt 使用同一 thread；并发 prompt 不会产生双 run；重启期间未完成 run 有明确的可恢复终态。

### Phase C — 断线、恢复与权限（P1）

1. 为每 session 维护单调 event cursor 与容量/TTL 可配置的 update ring buffer；关键状态转换同时落入持久 store，内存 buffer 仅用于低延迟重放。
2. 在 `_meta.eventCursor` / `_meta.resumeFrom` 中提供 anureo 扩展；无法协商扩展的 client 至少收到当前状态快照。
3. 默认 `disconnect_policy=persist`，仅 `ANUREO_ACP_DISCONNECT_POLICY=cancel` 时取消该连接拥有的 run；无重连 session/run 的 TTL 清理必须释放 MCP/PTY 资源。
4. 定义 `PendingPermission`，绑定 session、run、owner、connection generation、deadline。离线时暂停，不自动允许；超时拒绝并发送可诊断状态。

验收：运行中断线后 run 继续；重连能重放遗漏 update 或获得最终状态；权限等待不会在断线后执行。

### Phase D — transport 防护与可观测性（P1）

1. 保留 Origin 校验，并将 HTTP Bearer 的认证主体写入 connection 与 session owner。
2. 对 initialize deadline、空闲 ping/pong、最大帧/消息、每主体连接数与并发 run 设置配置项与拒绝行为。
3. binary frame、非法 JSON-RPC 和协议版本错误返回明确的 protocol error/close，而非静默忽略。
4. 添加不记录 prompt/token 的连接、重连、run、权限超时与 replay 丢弃结构化日志/指标。

验收：跨主体 session 接管、非法 Origin、超大帧、未 initialize prompt 均被拒绝；本地无 token 场景仍可运行。

### Phase E — 端到端验证与文档（P1）

1. 构建 WS test harness：initialize → session/new → prompt → update → cancel。
2. 覆盖 reconnect/replay、双 client 隔离、权限超时、binary/invalid JSON、ping/pong、认证、超大帧及 session 并发。
3. 以协商的 `protocolVersion` 与 capabilities 写断言，不以 Rust crate 版本替代 wire compatibility。
4. 将 `acp-websocket.md` 与 todo 更新为真实语义；删除“已实现”但尚未接线的描述。

## 4. 预期改动文件

| 文件 | 改动 |
| --- | --- |
| `apps/server/src/handlers/acp.rs` | ✅ 已替换为 hub-backed WS dispatcher（`AcpHub::attach` → `run_agent_connection`） |
| `apps/server/src/acp_hub.rs` | session runtime、attach/detach、replay、TTL、启动恢复与 prompt 串行 |
| `apps/server/src/state.rs` | hub 配置与生命周期注入 |
| `apps/server/src/acp_session_store.rs` | 新增持久 session/run/checkpoint 元数据 store 与迁移 |
| `apps/acp/src/stdio_loop.rs` | ✅ 提取 `run_agent_connection()` 公共函数，stdio 与 WS 共用 |
| `apps/acp/src/agent.rs` | 抽离 connection-independent session/run core _(待做)_ |
| `apps/acp/src/tools/client_bridge.rs` | 删除全局单例，改为显式依赖 |
| `apps/acp/src/tools/{fs_tools,terminal_tools}.rs` | 按 session connection 使用 bridge |
| `apps/server/tests/acp_ws_mega_e2e.rs` | 扩展为真实 prompt/reconnect 测试 |
| `apps/server/tests/acp_ws_*.rs` | 新增隔离、权限、防护和协议测试 |

## 5. Workflow 执行约束

- 可执行 workflow：`.anureo/workflows/acp-websocket-persistent.lua`。
- anureo 先完成任务拆分与风险检查，随后自动执行；任一测试门槛失败立即停止。
- workflow 将 Phase A–E 拆给多个职责明确的开发 agent，所有 agent 的模型固定为 `huoshan/deepseek-v4-flash-260425`。
- workflow 最大并发为 `1`；agent 与 phase 均严格串行，后续 phase 以前一 phase 的测试通过为前置条件。
- 每个 phase 独立 worktree/分支，禁止自动 commit、merge、push 或修改本设计文档。
- 每 phase 完成后记录 diff、测试结果与未决风险；只有前置测试通过才自动进入下一 phase。
- `/acp` 及其连接生命周期中不得 spawn `anureo`、`anureo acp` 或任何 ACP agent 子进程；server restart recovery 作为 Phase B/C 的强制验收项。

启动时必须使用 `workflow_start` 的 `concurrency: 1`；Lua 脚本不使用 `parallel` 或 `pipeline`，因此多个 development agent 不会并发写入同一 worktree。

## 6. 自动执行门槛

workflow 启动和每个 phase 切换前，必须确认：

- [ ] Phase A 的 SDK dispatch 接入点经代码勘查可行，不以 spawn 子进程兜底。
- [ ] `ClientBridge` 的显式依赖边界和 session → connection 查找方式明确。
- [ ] session actor/queue 的错误语义、取消语义和锁粒度已定义。
- [ ] reconnect replay、owner 与权限 TTL 的数据模型可测试。
- [ ] 每阶段文件冲突、测试命令和回滚方式已列出。

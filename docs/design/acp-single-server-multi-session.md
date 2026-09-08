# 标准 ACP 单 Server 多 Session 实现方案

> **状态**: 已实现（自动化验收通过；permission/MCP 资源生命周期与外部 metrics exporter 接入待完成）
> **目标版本**: ACP v1（`agent-client-protocol = 0.15.1`）
> **相关代码**: `apps/server/src/acp_hub.rs`、`apps/server/src/handlers/acp.rs`、`apps/acp/src/agent.rs`、`apps/acp/src/session.rs`、`apps/acp/src/stdio_loop.rs`
> **交叉参考**: [ACP WebSocket](./acp-websocket.md)、[ACP WebSocket 持久化实现方案](./acp-websocket-persistent-implementation.md)、[anureoAcpAgent 拆分设计](./acp-agent-refactor.md)、[ACP 官方 Session Setup](https://agentclientprotocol.com/protocol/v1/session-setup)、[Session Delete](https://agentclientprotocol.com/protocol/v1/session-delete)、[File System](https://agentclientprotocol.com/protocol/v1/file-system)

---

## 1. 背景与目标

改造前，`anureo server` 每个进程虽然只创建一个 `AppState` 和一个 `AcpHub`，但连接模型仍然面向“一个逻辑客户端反复重连”：新的 WebSocket attach 会取消上一条 lease，并把全局通知接收者替换为最新连接。当前实现已改为一个 server-owned `AcpRuntime` 管理多条独立连接和多个 session。

本方案将并行、交替和多工作区能力统一建模为标准 ACP session：

```text
一个 anureo server
  └── 一个 anureo ACP Agent
        ├── session A：cwd = project-a
        ├── session B：cwd = project-b
        └── session C：cwd = project-c
```

客户端可以在同一条 ACP 连接上交替操作多个 session，也可以通过多条 ACP 连接访问同一个 server。协议线上只使用 ACP v1 标准方法和字段，不引入 `instanceId`、`_anureo/instance/*` 或自定义路由。

### 1.1 目标

1. 一个 `anureo server` 进程承载一个 anureo ACP Agent。
2. 一个 ACP 连接可以创建、加载和操作多个独立 session。
3. 不同 session 可以并行执行；同一 session 的 prompt 严格串行。
4. 多条 ACP 连接可以同时存在，第二条连接不能终止第一条连接。
5. 每个 session 独立保存 `cwd`、MCP 配置、model/mode、thread 和取消状态；当前连接绑定由 `SessionBindings` 独立管理。
6. `session/update`、fs/terminal 反向 RPC 和权限请求必须路由到该 session 当前绑定的连接。
7. 断线后通过标准 `session/load` 或 `session/resume` 恢复，不依赖自定义 replay cursor。
8. `initialize` 只声明已经真实注册并可调用的 capabilities。
9. Zed、Node.js E2E Client 和 `anureo --acp` 使用同一套标准 ACP 行为。

### 1.2 非目标

- 不在一个 server 进程中创建多个 anureo Agent 实例。
- 不设计 Agent 实例注册、负载均衡或 `instanceId -> Agent` 路由。
- 不修改 ACP wire schema。
- 不要求多个 server 共享数据库或迁移 active run。
- 不在本方案中重构所有 HTTP/SSE handler 的进程级工作目录语义。
- 不保证断线时仍在执行的 JSON-RPC `session/prompt` 请求可以跨连接返回原 response。

## 2. 实施前基线与缺口

> 本节保留架构 review 时的基线，描述的是改造前状态；实际落地结果见 2.4 和第 17 节。

### 2.1 当前启动模型

`apps/server/src/runtime.rs` 当前只创建一次 state、router 和 listener：

```rust
let app = build_router(new_server_state());
let listener = TcpListener::bind(address).await?;
axum::serve(listener, app).await?;
```

`new_server_state()` 在 `apps/server/src/state.rs` 中只创建一个：

```rust
acp_hub: Arc::new(crate::acp_hub::AcpHub::default()),
```

因此“单 server、单 Agent”已经成立，不需要增加实例管理层。

### 2.2 当前已经具备的基础

| 能力 | 当前状态 | 代码位置 |
| --- | --- | --- |
| 多 session 存储 | 已有内存 `HashMap<SessionId, SessionEntry>` | `apps/acp/src/session.rs` |
| session 级 cwd | 已存入 `SessionEntry.working_directory` | `apps/acp/src/session.rs` |
| 每 session prompt 串行 | `begin_prompt()` 拒绝重叠 turn | `apps/acp/src/session.rs` |
| 不同 session 独立取消 | `cancel_current_generation()` | `apps/acp/src/session.rs` |
| checkpoint 历史 | SQLite checkpointer，以 thread id 查询 | `apps/acp/src/agent.rs` |
| 标准 `session/load` | 已实现历史回放 | `apps/acp/src/agent.rs` |
| 标准 `session/list` | 已实现数据库查询 | `apps/acp/src/agent.rs` |
| WS 与 stdio 共用 dispatch | `run_agent_connection()` | `apps/acp/src/stdio_loop.rs` |

### 2.3 必须修复的缺口

| 缺口 | 当前行为 | 影响 |
| --- | --- | --- |
| 单 lease 接管 | `AcpHub::attach_with()` 发送上一条 `lease_cancel` | 两个 Zed thread 若对应两条连接，后连接会关闭前连接 |
| 单通知 recipient | `HubInner.recipient` 只有一个 sender | 所有 session update 只能发给最新连接 |
| capabilities 属于 Agent 全局状态 | `anureoAcpAgent.client_capabilities` 是单个 `RwLock` | 第二个连接的 `initialize` 会覆盖第一个连接能力 |
| bridge 未绑定真实 session | initialize 时只调用 `set_connection_for_session("default", ...)` | fs/terminal 工具按真实 session id 查找时无法可靠得到 bridge |
| 两套连接状态并存 | `SessionEntry.connection` 已存在，但 dispatch 未写入 | 结构存在，生命周期未接线 |
| 全局静态 bridge registry | `SESSION_BRIDGES: OnceLock<HashMap<...>>` | 生命周期与 server state 分离，测试污染且难以 owner 隔离 |
| capability 与 handler 不一致 | initialize 声明 `sessionCapabilities.resume`，dispatch 没有 `session/resume` handler | Client 会调用一个 Agent 声称支持但实际未注册的方法 |
| owner 只属于 Hub | `HubInner.owner` 只有一个 principal | 不能对单个 session 做 owner 校验，多连接只能整体拒绝 |
| replay 以连接为中心 | Hub 维护一个全局通知 ring buffer | 多 session、多连接时无法判断通知目标，且 replay cursor 不是 ACP 标准恢复机制 |
| 进程级 cwd | server 启动时调用 `std::env::set_current_dir()` | 若 ACP 执行路径继续读取 process cwd，会破坏 session 工作区隔离 |

### 2.4 当前落地状态

| 范围 | 状态 | 实现结果 |
| --- | --- | --- |
| 单 runtime、多连接 | 已完成 | `AcpHub` 懒加载一个 `AcpRuntime`；每条 transport 创建独立 `AcpConnection` |
| session 归属与通知 | 已完成 | `SessionBindings` 原子维护双向索引，`NotificationRouter` 按 session 路由并提供 flush barrier |
| capability 与 reverse RPC | 已完成 | capability 属于 connection；fs/terminal 显式注入固定真实 session id 的 bridge |
| lifecycle 与恢复 | 已完成 | 注册 load/resume/close/delete；owner/canonical cwd 校验；`acp_sessions` SQLite metadata 支持 server restart；delete transaction fault-injection 已覆盖 |
| 并发控制 | 已完成 | 同 session busy 为 `-32010`；load/resume 与 prompt 原子互斥；全局 prompt 上限默认 4 |
| 自动化验收 | 已完成 | ACP unit 145/145、WebSocket E2E 8/8、Node.js BDD 10/10（含 Zed-compatible ACP smoke） |
| 后续加固 | 部分完成 | permission/MCP 的细粒度回收、外部 metrics exporter 的长期接入；terminal session 级回收、Zed UI smoke 与 runtime `/metrics` 聚合指标已完成 |

## 3. 设计决策

| 维度 | 决定 | 说明 |
| --- | --- | --- |
| Agent 数量 | 每个 server 一个 | 不引入多实例概念 |
| 并行单位 | ACP session | 与 ACP 标准模型一致 |
| 连接数量 | 允许一个或多个 | 单连接可多 session，多连接互不接管 |
| session 归属 | `owner + SessionBindings` | owner 持久化在 session；临时 connection 归属只存在于原子双向索引中 |
| 同 session 并发 | 拒绝第二个 prompt | 返回稳定 server error，不在首版引入队列 |
| 不同 session 并发 | 允许 | 受 server 全局 semaphore 限制 |
| session 切换 | Client 在请求中使用 `sessionId` | 不存在 `instance/select` |
| session 接管 | 仅 `session/load` / `session/resume` 可重绑连接 | 普通 `session/prompt` 不能隐式抢占 |
| 断线策略 | 保持当前 `persist` 兼容默认；`cancel` 继续作为显式配置 | ACP 不规定 transport 断线策略；默认值变更必须单独 ADR 决策 |
| 历史恢复 | `session/load` 完整回放 | ACP 标准语义 |
| 快速恢复 | `session/resume` 不回放 | 仅在 handler 实现后声明 capability |
| cwd | session 请求中的绝对路径 | ACP 执行链不得依赖 process cwd |
| bridge | state-owned per-connection bridge | 删除全局静态 registry |
| capabilities | per connection | 不存放在共享 Agent core 上 |
| 协议扩展 | 核心流程不使用 | `_meta` 只能保留为可忽略附加信息，不能参与正确性 |

## 4. 目标架构

```text
Zed / Node / anureo --acp
          │
          │ ACP v1 JSON-RPC
          ▼
┌─────────────────────────────────────────────────────────────┐
│ anureo server（单进程、单 AppState）                          │
│                                                             │
│  /acp                                                       │
│    └── AcpHub                                               │
│          ├── AcpRuntime（单 Agent core）                    │
│          │     ├── SessionStore                             │
│          │     ├── SessionConfigStore                      │
│          │     ├── AgentRegistry                           │
│          │     ├── ModelProvider                           │
│          │     ├── NotificationRouter                      │
│          │     └── global prompt Semaphore                 │
│          │                                                  │
│          └── ConnectionRegistry                            │
│                ├── connection-1                            │
│                │     ├── owner                             │
│                │     ├── client capabilities               │
│                │     ├── late-bound SDK client/bridge      │
│                │     └── outbound sender                   │
│                └── connection-2                            │
│                                                             │
│  SessionBindings（唯一连接归属事实源）                      │
│    ├── sess-a -> connection-1                              │
│    ├── sess-b -> connection-1                              │
│    └── sess-c -> connection-2                              │
│                                                             │
│  SessionStore                                               │
│    ├── sess-a -> owner-1, cwd-a, active turn               │
│    ├── sess-b -> owner-1, cwd-b, active turn               │
│    └── sess-c -> owner-1, cwd-c, idle                      │
└─────────────────────────────────────────────────────────────┘
```

### 4.1 生命周期边界

| 对象 | 生命周期 | 持有内容 |
| --- | --- | --- |
| `AcpHub` | server 进程 | runtime、连接表、session bindings、统计 |
| `AcpRuntime` | server 进程 | Agent 核心依赖和 session store |
| `AcpConnection` | 一条 WS 或 stdio transport | owner、late-bound SDK client、capabilities、outbound sender、active 标记 |
| `SessionBindings` | server 进程 | session 与 connection 的原子双向索引，唯一连接归属事实源 |
| `SessionEntry` | session 创建到 close/delete | thread、cwd、配置、owner、active turn；不持有 connection 引用 |
| `RunningTurn` | 一次 `session/prompt` | generation、取消 token、全局并发 permit |

## 5. 核心数据结构改动

### 5.1 `AcpRuntime`

从 `anureoAcpAgent` 中拆出连接无关的共享核心：

```rust
pub struct AcpRuntime {
    sessions: SessionStore,
    agent_registry: AgentRegistry,
    config_store: SessionConfigStore,
    model_provider: Arc<dyn ModelProvider>,
    prompt_executor: Arc<dyn AcpPromptExecutor>,
    notification_router: Arc<NotificationRouter>,
    prompt_limit: Arc<Semaphore>,
}
```

`AcpRuntime` 不保存：

- 单个 client 的 capabilities；
- 单个 `ConnectionTo<Client>`；
- 单个 notification receiver；
- WebSocket lease；
- 当前 owner。

现有 `anureoAcpAgent` 可以分两步迁移：

1. 先让它内部持有 `Arc<AcpRuntime>`，把 handler 改为接收 `Arc<AcpConnection>`。
2. 再重命名为 `anureoAcpConnectionAgent`，作为 ACP SDK dispatch adapter。

这样可以避免一次性移动 `agent.rs` 中全部业务逻辑。

### 5.2 `AcpConnection` 的两阶段初始化

现有 `apps/acp/src/connection.rs` 已定义骨架，但 `ConnectionTo<Client>` 只有 ACP SDK 进入 `initialize` handler 后才可获得。因此 Hub 不能在 WebSocket upgrade 时构造一个已经可调用反向 RPC 的完整 bridge。目标模型分为两个阶段：

```text
WebSocket / stdio transport 建立
  -> AcpConnection shell（Created）
initialize handler 收到 ConnectionTo<Client>
  -> bind_client()（Initialized）
session/new/load/resume
  -> 允许绑定 session
```

connection shell 在 upgrade 时即可创建 notification channel 和 connection id；SDK client 使用 late-bound slot：

```rust
pub struct AcpConnection {
    pub id: ConnectionId,
    pub principal: String,
    pub sdk_client: Arc<RwLock<Option<ConnectionTo<Client>>>>,
    pub capabilities: RwLock<Option<ClientCapabilitiesInfo>>,
    pub outbound_tx: mpsc::Sender<ConnectionOutbound>,
    pub active: AtomicBool,
    pub initialized: AtomicBool,
}
```

关键方法：

```rust
impl AcpConnection {
    pub fn bind_client(
        &self,
        client: ConnectionTo<Client>,
        caps: ClientCapabilitiesInfo,
    ) -> Result<(), ConnectionStateError>;
    pub fn session_bridge(&self, session_id: SessionId) -> Result<SessionClientBridge, ConnectionStateError>;
    pub fn deactivate(&self);
    pub fn is_active(&self) -> bool;
    pub fn is_initialized(&self) -> bool;
}
```

`bind_client()` 只允许成功一次；重复 initialize 返回 invalid request。`SessionClientBridge` 持有真实 session id 和 late-bound SDK client，而不是在 Hub attach 阶段提前构造。`outbound_tx` 每条连接独立，`run_agent_connection()` 只消费本连接的 receiver，不再消费 Hub 全局 receiver。

### 5.3 `SessionEntry`

在现有结构上增加 owner 和显式状态：

```rust
pub struct SessionEntry {
    pub thread_id: String,
    pub working_directory: PathBuf,
    pub owner_principal: String,
    pub lifecycle: Arc<RwLock<SessionLifecycle>>,
    pub cancellation: Arc<SessionCancellationState>,
    /// Serializes short lifecycle/binding transitions; never held for a full prompt turn.
    pub control_lock: Arc<tokio::sync::Mutex<()>>,
    pub session_config: SessionConfig,
    pub mcp_servers: Vec<config::McpServerDef>,
}

pub enum SessionLifecycle {
    Idle,
    Running { generation: u64 },
    Closed,
}
```

`working_directory` 改为必填 `PathBuf`，因为 ACP v1 的 `session/new`、`session/load` 和 `session/resume` 都要求 cwd。边界 handler 校验后，内部不再处理 `None` fallback。

### 5.4 `ConnectionRegistry`

```rust
pub struct ConnectionRegistry {
    connections: RwLock<HashMap<ConnectionId, Arc<AcpConnection>>>,
}

pub struct SessionBindings {
    inner: Mutex<BindingState>,
}

struct BindingState {
    session_to_connection: HashMap<SessionId, ConnectionId>,
    connection_to_sessions: HashMap<ConnectionId, HashSet<SessionId>>,
}
```

职责：

- 创建 connection id；
- 注册/注销连接；
- 按 connection id 查询；
- 通过 `SessionBindings` 在断线时找到该连接绑定的 sessions；
- 只取消这些 sessions 的 active turns；
- 不影响其他连接。

`SessionBindings` 是 session 连接归属的唯一写入者。`SessionEntry` 和 `AcpConnection` 都不保存第二份绑定集合。owner/cwd/lifecycle 仍由 `SessionEntry` 负责；runtime 持有该 session 的短期 `control_lock` 完成校验和绑定迁移，避免 load、delete、disconnect 路径在校验后互相穿越。所有 create/load/resume/close/disconnect 路径必须调用 Hub 的原子 API：

```rust
pub fn bind_new_session(&self, session_id: SessionId, connection_id: ConnectionId);
pub fn rebind_session(
    &self,
    session_id: &SessionId,
    new_connection_id: ConnectionId,
) -> Result<Option<ConnectionId>, BindError>;
pub fn unbind_connection(&self, connection_id: &ConnectionId) -> Vec<SessionId>;
```

### 5.5 `NotificationRouter`

替换 `anureoAcpAgent.session_update_tx` 和 `HubInner.recipient`：

```rust
pub struct NotificationRouter {
    bindings: Arc<SessionBindings>,
    connections: Arc<ConnectionRegistry>,
}

impl NotificationRouter {
    pub async fn send(
        &self,
        notification: SessionNotification,
    ) -> Result<(), NotificationRouteError>;

    pub async fn send_and_flush(
        &self,
        notifications: impl IntoIterator<Item = SessionNotification>,
    ) -> Result<(), NotificationRouteError>;
}
```

发送步骤：

1. 从 notification 读取 `sessionId`。
2. 通过 `SessionBindings` 查找 connection id。
3. 通过 `ConnectionRegistry` 读取 connection。
4. 检查 connection active 且 initialized。
5. 发送到该 connection 的 bounded outbound channel。

不能再有“当前 recipient”或“最新连接”概念。

`session/load` 有更强的顺序要求：完整历史的 `session/update` 必须先进入底层 ACP connection 的发送队列，随后才能发送 load response。为此 outbound item 带可选 ack：

```rust
enum ConnectionOutbound {
    Notification {
        value: SessionNotification,
        enqueued: Option<oneshot::Sender<()>>,
    },
}
```

notification drain 在 `ConnectionTo<Client>::send_notification()` 成功后发送 ack。`send_and_flush()` 等待最后一个 ack；load handler 只有在 barrier 完成后才调用 `responder.respond_with_result(...)`。这里的 flush 表示“已按顺序进入同一个 SDK connection 的 FIFO 发送队列”，不要求等待对端处理通知。

普通流式 `session/update` 使用 `send()`；历史回放、必须先于 response 的状态同步使用 `send_and_flush()`。禁止使用 sleep 猜测发送完成。

## 6. 标准 ACP 方法语义

### 6.1 Capability 声明

Capability 必须与 dispatch handler 同一个提交落地。目标声明：

```rust
let session_caps = SessionCapabilities::new()
    .list(SessionListCapabilities::new())
    .resume(SessionResumeCapabilities::new())
    .close(SessionCloseCapabilities::new())
    .delete(SessionDeleteCapabilities::new());

let agent_caps = AgentCapabilities::new()
    .load_session(true)
    .session_capabilities(session_caps)
    .prompt_capabilities(prompt_caps)
    .mcp_capabilities(mcp_caps);
```

实施期间采用保守策略：

- `session/resume` handler 未完成前，移除 `.resume(...)`。
- `session/close` handler 未完成前，不声明 `.close(...)`。
- `session/delete` handler 未完成前，不声明 `.delete(...)`。
- `session/list` 和 `loadSession` 保持现有声明，但增加 owner/cwd 过滤测试。
- `session/fork` 当前属于 crate feature 下的 unstable 能力，不作为本方案的标准基线验收项。

### 6.2 方法矩阵

| 方法 | 类型 | session/connection 行为 | 并发行为 |
| --- | --- | --- | --- |
| `initialize` | request | 初始化当前 connection 的 capabilities | 每连接一次；重复调用返回 invalid request |
| `authenticate` | request | 完成认证，不改变其他连接 | 每连接独立 |
| `session/new` | request | 创建 session，写 owner/cwd，绑定当前连接 | 快速操作 |
| `session/load` | request | 校验 owner/cwd，原子重绑，完整回放并 flush | active turn 存在时拒绝 |
| `session/resume` | request | 校验 owner/cwd，原子重绑，不回放历史 | active turn 存在时拒绝 |
| `session/list` | request | 只返回当前 owner 可访问的 sessions | 可并行 |
| `session/prompt` | request | 仅操作当前连接已绑定的 session | 同 session 串行，不同 session 并行 |
| `session/cancel` | notification | 取消指定 session 当前 generation | 不清除其他 session |
| `session/close` | request | 取消 active turn，释放内存资源，保留持久历史 | 幂等 |
| `session/delete` | request | close 后删除 session 元数据和 checkpoint；不存在也成功 | active turn 存在时先取消并等待收尾 |
| `session/update` | notification | 路由到 session 当前绑定连接 | 不允许跨 session 串线 |

### 6.3 `session/new`

处理顺序：

1. 确认连接已经 initialize。
2. 验证 `cwd.is_absolute()`。
3. 验证 cwd 存在且为目录；失败返回 `InvalidParams (-32602)`。
4. 创建 UUID session id 和同名 thread id。
5. 写入 owner、cwd、MCP servers、默认 model/mode。
6. 绑定当前 connection。
7. 持久化 session metadata。
8. 返回标准 `NewSessionResponse`。

禁止调用 `std::env::set_current_dir()`。

### 6.4 `session/prompt`

处理顺序：

```text
lookup session
  -> verify owner
  -> verify attached connection == caller connection
  -> acquire per-session turn
  -> acquire global prompt permit
  -> build RunOptions from session cwd/thread/config
  -> build tools from caller connection capabilities/bridge
  -> run agent
  -> route session/update by session id
  -> persist checkpoint
  -> release turn + permit
  -> return PromptResponse
```

同 session 第二个 prompt 不排队，返回：

```json
{
  "code": -32010,
  "message": "a prompt is already in progress for this session"
}
```

不同 session 使用独立 `RunningTurn`，可以同时获得全局 semaphore permit。建议默认并发数为 CPU/模型限制的较小值，例如 4，并允许通过 server 配置调整；该配置不进入 ACP wire contract。

### 6.5 `session/load` 与 `session/resume`

两者都必须从当前 handler 获得 caller connection，并重绑 session：

```rust
runtime.load_session(req, connection.clone()).await
runtime.resume_session(req, connection.clone()).await
```

差异严格遵循 ACP：

- `load`：从 checkpoint 读取完整对话，通过 `send_and_flush()` 发送标准 `session/update` 历史通知；flush barrier 完成后才响应 request。
- `resume`：恢复 session 内存状态和 MCP 配置，不发送历史回放，直接返回当前 mode/config state。

两者在重绑前都必须 canonicalize 请求 cwd，并与持久 session cwd 完全一致：

```rust
let requested = canonicalize_existing_directory(&args.cwd)?;
if requested != session.working_directory {
    return Err(Error::invalid_params().data("cwd does not match the session working directory"));
}
```

load/resume 不得静默修改 session cwd。cwd 是 session 创建时确定的不可变文件系统上下文。

如果 session 原来绑定另一条连接：

1. 取得 `SessionEntry.control_lock`；锁顺序固定为 session control → `SessionBindings`，其他路径不得反向持锁。
2. 在锁内校验 owner、canonical cwd 和 session 没有 active turn。
3. 调用 `SessionBindings::rebind_session()`，在一个锁内同时更新正向和反向索引。
4. 释放 control lock，旧连接后续对该 session 的 prompt 返回“未绑定到当前连接”。

不允许普通 `session/prompt` 隐式完成此接管。

### 6.6 `session/close` 与 `session/delete`

`close`：

- 取消 active turn；
- 从 connection 解绑；
- 释放 MCP/terminal 等 session runtime 资源；
- 将 metadata 标记为 closed；
- 保留 checkpoint，之后仍可 `session/load`。

`delete`：

- 执行 close 语义；
- 删除 session metadata；
- 删除 checkpoint、config 和 review/status 关联记录；
- 后续 load 返回 `ResourceNotFound (-32002)`。
- 如果 session 不存在或已经删除，直接返回成功空结果；delete 必须幂等。

删除必须使用事务，避免只删 metadata 或只删 checkpoint。

### 6.7 错误映射

| 条件 | ACP/JSON-RPC code | 对外 message |
| --- | --- | --- |
| cwd 不是绝对目录 | `InvalidParams (-32602)` | `cwd must be an existing absolute directory` |
| `load/resume/prompt` 的 session 不存在 | `ResourceNotFound (-32002)` | `session not found` |
| `close/delete` 的 session 不存在或已关闭/删除 | 成功空结果 | 无 error |
| owner 不匹配 | `AuthRequired (-32000)` | `session not available for this principal` |
| session 未绑定当前连接 | `Other (-32011)` | `session is attached to another connection` |
| session 已有 active prompt | `Other (-32010)` | `a prompt is already in progress for this session` |
| request 被取消 | `RequestCancelled (-32800)` | `request cancelled` |
| 未声明/未实现方法 | `MethodNotFound (-32601)` | SDK 默认行为 |

owner 不匹配时不返回 session 真实 metadata，避免泄露跨主体 session 是否存在。

## 7. 连接与断线模型

### 7.1 attach 不再接管旧连接

删除 `HubInner` 中面向单连接的字段：

```rust
recipient: Arc<Mutex<Option<Sender<_>>>>,
lease_cancel: Option<oneshot::Sender<()>>,
owner: SessionOwner,
generation: u64,
```

`AcpHub::attach_with()` 改为：

```rust
pub async fn open_connection(
    &self,
    owner: SessionOwner,
) -> Result<AcpConnectionLease, AcpHubError>;
```

返回值：

```rust
pub struct AcpConnectionLease {
    pub runtime: Arc<AcpRuntime>,
    /// 尚未 initialize 的 transport connection shell。
    pub connection: Arc<AcpConnection>,
    pub outbound_rx: mpsc::Receiver<ConnectionOutbound>,
}
```

此处 `connection.sdk_client == None` 且 `initialized == false`。`run_agent_connection()` 注册完 ACP handlers 后，由 initialize handler 调用 `connection.bind_client(sdk_conn, capabilities)`。在此之前收到 `session/new`、`session/load` 或 `session/prompt` 必须返回 invalid request。

lease drop 或 handler 收到 WS close 时只执行：

```rust
hub.close_connection(connection.id()).await;
```

不能向其他 connection 发送 shutdown signal。

### 7.2 断线处理

标准兼容路径不依赖 `_meta.resumeFrom` 或 event cursor。ACP 没有规定 transport 断线时必须 cancel 还是 persist，因此本方案不把其中任一行为描述为协议要求。

为了保持现有行为兼容，首版继续使用 `DisconnectPolicy::Persist` 默认值：

```text
WS disconnect
  -> connection.active = false
  -> SessionBindings::unbind_connection(connection_id)
  -> 清除 reverse RPC bridge
  -> active turns 默认继续；需要 client reverse RPC 的工具返回 connection disconnected
  -> orphan TTL 到期后取消仍未完成的 turns
  -> session metadata/checkpoint 保留
  -> client reconnect + initialize
  -> active turn 已结束后，client session/load（回放）或 session/resume（不回放）
```

Persist 下旧 JSON-RPC prompt response 无法跨连接返回，Client 必须把原请求视为 transport failure，并在 turn 结束或 orphan TTL 取消后通过 load/resume 恢复。active turn 存在期间，load/resume 返回 session busy，不能抢占绑定。

`ANUREO_ACP_DISCONNECT_POLICY=cancel` 继续提供确定性取消：断线时立即取消该 connection 原先绑定的 active turns。是否将 cancel 改为未来默认值必须单独形成 ADR，评估 `anureo acp` 自动重连、短暂网络抖动和后台副作用后再决定；本 RFC 不改变当前默认值。

### 7.3 stdio 模式

`anureo acp` stdio 仍使用同一个 `run_agent_connection()`：

- stdio EOF 等价于 connection disconnect；
- 一个 stdio process 可以承载多个 session；
- session 的反向 RPC 都使用该 stdio connection；
- 若 Zed 为不同 thread 启动多个 stdio process，每个 process 可连接同一个 `anureo server`，server 侧按多 connection 模型隔离。

### 7.4 启动与 Zed 配置

server 只启动一次：

```powershell
anureo server --host 127.0.0.1 --port 3030
```

Zed 通过 `anureo acp` stdio bridge 连接该 server：

```json
{
  "agent_servers": {
    "anureo": {
      "type": "custom",
      "command": "anureo",
      "args": ["acp", "ws://127.0.0.1:3030/acp"],
      "env": {}
    }
  }
}
```

命令行一次性 prompt 使用相同 endpoint：

```powershell
anureo --acp --acp-url ws://127.0.0.1:3030/acp "检查当前项目"
```

这些入口只影响 transport 和 client 生命周期，不创建第二个 anureo Agent。所有 session 最终都由同一个 server-side `AcpRuntime` 管理。

## 8. 工作目录与工具隔离

### 8.1 cwd 的唯一来源

ACP run 的工作目录只从 `SessionEntry.working_directory` 获取：

```rust
let working_folder = session.working_directory.clone();

let options = RunOptions {
    working_folder,
    thread_id: Some(session.thread_id.clone()),
    acp_session_id: Some(session_id.to_string()),
    ..
};
```

以下代码不得出现在 ACP prompt 执行链：

```rust
std::env::current_dir()
std::env::set_current_dir(...)
```

`anureo server --directory` 可以继续作为 HTTP/SSE server 的默认 project directory，但 ACP 的 `session/new.cwd` 必须覆盖且完全隔离。

### 8.2 删除 `SESSION_BRIDGES`

删除：

```rust
static SESSION_BRIDGES: OnceLock<SessionBridgeMap>;
set_connection_for_session("default", ...);
get_session_bridge(session_id);
```

connection-level `AcpClientBridge` 只负责持有 late-bound SDK client；每个 session 创建一个 `SessionClientBridge`，固定携带真实 session id：

```rust
pub struct AcpClientBridge {
    sdk_client: Arc<RwLock<Option<ConnectionTo<Client>>>>,
}

pub struct SessionClientBridge {
    session_id: SessionId,
    client: Arc<AcpClientBridge>,
}

impl SessionClientBridge {
    pub async fn read_text_file(&self, path: &str, ...) -> Result<String, String> {
        let conn = self.client.require_connection().await?;
        client_methods::read_text_file(&conn, &self.session_id, path, ...).await
    }
}

pub fn create_acp_tools(
    capabilities: &ClientCapabilitiesInfo,
    bridge: Arc<SessionClientBridge>,
) -> Vec<Box<dyn Tool>>;
```

fs/read、fs/write、terminal/create/output/wait/kill/release 必须全部从 `SessionClientBridge.session_id` 构建协议参数，不能保留任何 `SessionId::new("default")` fallback。fs/terminal tool 不再访问全局函数，而是持有构造时注入的 bridge。这样：

- session A 的工具只能调用 A 当前连接；
- session B 重绑不会覆盖 A；
- 单元测试可以直接注入 fake bridge；
- transport 断开后 bridge 可准确返回 connection closed。

### 8.3 capabilities 隔离

构建工具时从 session 当前 connection 读取 capabilities：

```rust
let conn = runtime.require_bound_connection(session_id, caller_connection_id)?;
let caps = conn.require_capabilities()?;
let bridge = Arc::new(conn.session_bridge(session_id.clone())?);
let tools = create_acp_tools(&caps, bridge);
```

不能再从 `anureoAcpAgent.client_capabilities` 读取全局值。

## 9. Session 持久化

当前 checkpoint 可以恢复对话，但 `SessionStore` 本身只在内存中。增加 `acp_sessions` metadata 表：

```sql
CREATE TABLE IF NOT EXISTS acp_sessions (
    session_id       TEXT PRIMARY KEY,
    thread_id        TEXT NOT NULL,
    owner_principal  TEXT NOT NULL,
    cwd              TEXT NOT NULL,
    lifecycle        TEXT NOT NULL DEFAULT 'idle',
    created_at       TEXT NOT NULL,
    updated_at       TEXT NOT NULL,
    closed_at        TEXT
);

CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_updated
    ON acp_sessions(owner_principal, updated_at DESC);
```

不持久化 connection id，因为连接只在当前进程有效。

不直接持久化 MCP server 中可能包含 secret 的 env/header；`session/load` 和 `session/resume` 请求会再次携带 MCP server 配置。若未来需要持久化，必须先设计 secret storage，不写入明文 session metadata。

### 9.1 server 启动恢复

启动时只恢复 metadata：

```text
acp_sessions row
  -> SessionEntry
  -> SessionBindings 中没有连接记录
  -> lifecycle running 改为 idle/interrupted
  -> 等待 load/resume 后建立 MCP 与 connection 资源
```

active run 不跨进程恢复。server 崩溃前处于 running 的 session 在启动后不能继续显示为 running。

### 9.2 一致性边界

- `session/new`：metadata insert 成功后才返回 session id。
- prompt checkpoint：沿用现有 checkpointer。
- `session/close`：metadata update 与资源释放可重试。
- `session/delete`：metadata、checkpoint、config、review rows 在同一数据库事务中删除；若当前表分属不同连接，先提供 repository-level transaction API。

## 10. `run_agent_connection()` 改造

当前函数只接收共享 `anureoAcpAgent` 和一个 notification receiver，handler 无法知道稳定的 caller connection context。目标签名：

```rust
pub async fn run_agent_connection<S, St, F>(
    runtime: Arc<AcpRuntime>,
    connection: Arc<AcpConnection>,
    outbound_rx: mpsc::Receiver<ConnectionOutbound>,
    transport: Lines<S, St>,
    shutdown: F,
) -> Result<(), agent_client_protocol::Error>
```

所有 handler 显式捕获同一个 `connection`：

```rust
.on_receive_request(move |req: NewSessionRequest, responder, _sdk_conn| {
    let runtime = runtime.clone();
    let caller = connection.clone();
    async move {
        responder.respond_with_result(runtime.new_session(req, caller).await)?;
        Ok(())
    }
})
```

这里的 `connection` 是 transport shell，不要求调用函数前已经取得 `ConnectionTo<Client>`。initialize handler 使用 SDK callback 提供的 `sdk_conn` 完成 late binding：

```rust
.on_receive_request(move |req: InitializeRequest, responder, sdk_conn| {
    let connection = connection.clone();
    async move {
        let caps = ClientCapabilitiesInfo::from_initialize(&req);
        let response = runtime.initialize(req).await?;
        connection.bind_client(sdk_conn, caps)?;
        responder.respond_with_result(Ok(response))?;
        Ok(())
    }
})
```

load handler 不得沿用“先发送到普通 mpsc，再 sleep”的方式：

```rust
let (history, response) = runtime.prepare_load(req, caller.clone()).await?;
runtime.notification_router.send_and_flush(history).await?;
responder.respond_with_result(Ok(response))?;
```

`prepare_load()` 在返回前完成 owner/cwd 校验和原子 rebind；`send_and_flush()` 保证所有历史通知先进入当前 SDK connection 的 FIFO 队列。

必须新增注册：

```rust
ResumeSessionRequest
CloseSessionRequest
DeleteSessionRequest
```

initialize handler 只 bind 当前 `AcpConnection.sdk_client` 和 capabilities，不写 Agent 全局状态，也不再写入 `"default"` bridge。

函数退出时调用 `connection.deactivate()`；Hub 的 `close_connection()` 负责 session 清理。

## 11. 具体文件改动清单

| 文件 | 类型 | 详细改动 |
| --- | --- | --- |
| `apps/server/src/acp_hub.rs` | 重构 | `HubInner` 改为 runtime + connection registry；删除单 recipient/lease takeover；新增 open/close connection、按连接取消、metrics |
| `apps/server/src/handlers/acp.rs` | 修改 | upgrade 后创建 connection lease；把 connection 传入 dispatch；断线只关闭当前连接 |
| `apps/server/src/state.rs` | 修改 | 构造共享 `AcpRuntime`、session repository、全局 prompt semaphore；test-support 下允许注入 runtime |
| `apps/server/src/runtime.rs` | 小改 | 明确 `--directory` 只提供 server 默认 project；ACP 不以 process cwd 作为 session cwd |
| `apps/acp/src/runtime.rs` | 新增 | `AcpRuntime`，承载连接无关的 Agent/session 业务 |
| `apps/acp/src/notification_router.rs` | 新增 | 按 SessionBindings 路由 `session/update`；实现 load 所需 `send_and_flush` barrier |
| `apps/acp/src/connection.rs` | 重构 | transport shell、late-bound SDK client/capabilities、active/initialized 生命周期 |
| `apps/acp/src/session.rs` | 修改 | owner、必填 cwd、lifecycle；移除 connection 引用；持久 repository 接口 |
| `apps/acp/src/session_bindings.rs` | 新增 | session ↔ connection 原子双向索引和 rebind/unbind API，连接归属唯一事实源 |
| `apps/acp/src/agent.rs` | 重构 | 业务迁移到 runtime；new/load/resume/prompt 接收 caller connection；移除全局 capabilities/tx |
| `apps/acp/src/prompt_executor.rs` | 新增 | 抽象 `AcpPromptExecutor`；生产实现运行 anureo graph，测试实现返回确定性 stream/update |
| `apps/acp/src/stdio_loop.rs` | 修改 | 新签名；每个 handler 捕获 caller；注册 resume/close/delete；移除 default bridge |
| `apps/acp/src/tools/client_bridge.rs` | 重构 | 删除全局 registry；connection bridge late-bind SDK client；`SessionClientBridge` 固定真实 session id |
| `apps/acp/src/tools/mod.rs` | 修改 | `create_acp_tools` 显式接收 capabilities 和 `SessionClientBridge` |
| `apps/acp/src/tools/fs_tools.rs` | 修改 | 构造时注入 bridge，不再查询全局 registry |
| `apps/acp/src/tools/terminal_executor.rs` | 修改 | 构造时注入 bridge 和 session id；删除 `default` fallback |
| `apps/acp/src/stream_bridge.rs` | 修改 | `SessionNotifier` 改用 `NotificationRouter`/sink，而非单 tx |
| `apps/acp/src/review_runner.rs` | 修改 | background update 通过 session-aware sink 路由 |
| `apps/acp/src/session_store.rs` | 新增 | `acp_sessions` schema、CRUD、owner/cwd 查询和事务删除；也可命名 `session_repository.rs` |
| `apps/acp/src/lib.rs` | 修改 | 导出 runtime/connection/router，更新架构说明 |
| `apps/acp/Cargo.toml` | 可能修改 | 若 persistence 需要 migration helper，增加最小依赖；不升级 ACP crate 作为本改造前置 |
| `apps/server/Cargo.toml` | 修改 | 复用 `test-support` feature；声明 `acp-test-server` binary，并用 `required-features = ["test-support"]` 隔离生产构建 |
| `apps/server/tests/acp_ws_e2e.rs` | 扩展 | 单连接多 session、交替 prompt、断线恢复 |
| `apps/server/tests/acp_ws_mega_e2e.rs` | 扩展 | 多连接隔离、接管、通知路由、owner 校验 |
| `apps/server/src/bin/acp_test_server.rs` | 新增（test-support） | 注入 deterministic `AcpPromptExecutor`，供 Node 启动真实 router/WS transport，不进入生产 binary |
| `e2e/features/acp/anureo-acp-multi-session.feature` | 新增 | Node.js BDD 场景 |
| `e2e/tests/acp-bdd/anureo-acp-multi-session.test.mjs` | 新增 | 启动一个 server，执行多 session 端到端测试 |
| `e2e/package.json` | 修改 | 将新 BDD suite 纳入 `test:bdd:acp` |

## 12. 分阶段实施

### Phase 0 — Capability 真实性修复（P0，已完成）

1. 从 initialize 暂时移除 `sessionCapabilities.resume`。
2. 增加 capability/handler 对照测试。
3. 确认 baseline `initialize`、`session/new`、`session/prompt`、`session/cancel`、`session/update` 不回归。

验收：Client 不会根据 capability 调用未注册方法。

### Phase 1 — 单连接多 session 正确性（P0，已完成）

1. WebSocket/stdio transport 建立时创建尚未初始化的 `AcpConnection` shell。
2. initialize handler 将 SDK `ConnectionTo<Client>` 和 capabilities late-bind 到 shell；initialize 前拒绝 session 方法。
3. `session/new`、`load`、`fork` 将当前 connection 绑定到真实 session id。
4. 删除 `"default"` bridge，fs/terminal 工具显式注入带真实 session id 的 `SessionClientBridge`。
5. capabilities 改为 per connection。
6. 完成一个连接上 A/B/A 交替 prompt 和 reverse RPC session id 测试。

验收：一个 WS/stdio 连接可以操作两个 cwd 不同的 session，工具和通知不串线。

### Phase 2 — 路由顺序与原子绑定（P0，已完成）

1. 引入 `SessionBindings`，以单锁维护 session → connection 和 connection → sessions 双向索引。
2. 删除 `SessionEntry`、`AcpConnection` 中重复的 connection 归属字段。
3. `NotificationRouter` 按 `SessionBindings` 路由到每连接独立的 outbound channel。
4. 为历史回放增加 `send_and_flush()` barrier，保证 load response 之前完成通知入队。
5. 对 concurrent rebind、disconnect/unbind 和 load replay 顺序做确定性测试。

验收：连接归属始终双向一致；`session/load` 的所有历史 update 在 response 之前发送，不依赖 sleep。

### Phase 3 — 单 server 多连接（P0，已完成）

1. `AcpHub` 从单 lease 改为 connection registry。
2. 每连接创建独立 connection shell、notification channel 和 drain task。
3. 两条连接分别绑定 session，并行执行 prompt。
4. 断线只注销本连接的 bindings，不关闭其他连接。
5. 覆盖默认 persist 和显式 cancel 两种断线策略。

验收：第二条连接 initialize、prompt、断开均不影响第一条连接。

### Phase 4 — 标准恢复、生命周期与资源（P1，核心语义已完成）

1. 接入 `acp_sessions` metadata store。
2. `session/load`、`session/resume` canonicalize 请求 cwd，并要求与已存 cwd 完全一致。
3. 完成 `session/resume` handler 后重新声明 capability；实现幂等 close/delete。
4. 删除核心流程对 replay cursor 的依赖；close/delete 和 prompt unwind 会回收仍登记的 ACP terminal，MCP/permission 资源回收仍需各自的生命周期 API。
5. 已增加全局 prompt semaphore；默认容量为 4，可通过 `ANUREO_ACP_MAX_CONCURRENT_PROMPTS` 设置正整数覆盖。ACP prompt 执行路径不再对缺失 session cwd fallback 到 process cwd。
6. 已增加 runtime metrics snapshot 和 `/metrics` Prometheus endpoint（active connections/sessions/prompts、total prompts、busy reject、route failure、session rebind）；外部 metrics exporter 的长期接入仍待实现。

验收：server 重启后可 load；同进程断线后可 resume；cwd 不可被恢复请求改写；达到容量上限时行为可预测。

### Phase 5 — Node.js BDD 与 Zed smoke（P1，自动化完成）

1. 增加仅在 `test-support` feature 下编译的 `acp-test-server`，注入 deterministic `AcpPromptExecutor`。
2. Node.js 启动该真实 router/ACP WebSocket server，并从 stdout 获取随机监听地址。
3. 建立一个或两个 ACP WebSocket，执行标准 ACP BDD 场景和消息顺序断言。
4. 使用生产 `anureo server` 与 Zed custom agent 配置完成 UI smoke；协议级 Zed-compatible smoke 同时保留为自动化门槛。

验收：Rust integration、Node BDD、Zed 三条路径行为一致。

当前环境使用 Zed custom agent `anureo` 完成了本次 UI smoke：第一个 thread 返回 `ZED_SMOKE_OK`，随后在同一 workspace 新建第二个 thread，返回 `ZED_SMOKE_THREAD_2_OK`。当前 ACP 日志 `logs/acp.log` 记录了两个不同 session id（`session-e790eca7-b8c9-4953-8b80-c49be793d19b`、`session-9e284e08-79de-47b6-98ca-9284f7e7096a`）的 prompt、`session/update` 和 `stopReason=end_turn`；第二个 thread 同时在 Zed 左侧 thread 列表中可见。该证据覆盖真实 Zed UI → stdio ACP → anureo agent → session/update 返回链路。

## 13. 测试方案

### 13.1 Rust 单元测试

| 测试 | 验证点 |
| --- | --- |
| `two_sessions_can_run_concurrently` | 不同 session 的 `RunningTurn` 独立 |
| `same_session_rejects_overlapping_prompt` | 第二个 prompt 返回 busy |
| `connection_capabilities_are_isolated` | connection B initialize 不覆盖 A |
| `session_methods_require_initialize` | late-bound client 尚未绑定时拒绝 session 方法 |
| `session_bridge_uses_real_session_id` | fs/terminal 请求到正确 fake client，且参数使用实际 session id |
| `binding_indexes_remain_consistent` | new/rebind/unbind 后两个方向的索引保持一致 |
| `concurrent_rebind_has_one_winner` | 并发接管不会产生双重归属 |
| `persist_disconnect_unbinds_without_global_cancel` | 默认断线只解绑当前连接，其他 session 不受影响，turn 按策略继续 |
| `cancel_disconnect_cancels_only_bound_sessions` | 显式 cancel 只取消该连接绑定的 active turns |
| `load_rebinds_same_owner_session` | 新连接接管成功，旧连接失去操作权 |
| `load_history_flushes_before_response` | 最后一条历史 update 的发送 ack 先于 load response |
| `cross_owner_load_is_hidden` | 返回 auth error，不泄露 metadata |
| `capabilities_match_registered_handlers` | initialize 声明与 dispatch 注册一致 |
| `cwd_must_be_absolute_directory` | 相对路径和文件路径被拒绝 |
| `load_resume_require_stored_cwd` | canonical cwd 与 session 已存 cwd 不一致时拒绝恢复 |
| `close_is_idempotent` | 重复 close 不产生资源泄漏 |
| `delete_missing_is_success` | 不存在或已删除 session 返回成功空结果 |

### 13.2 Rust WebSocket integration

新增或扩展 `apps/server/tests/acp_ws_e2e.rs`：

```text
initialize
  -> session/new(cwd-a) => sess-a
  -> session/new(cwd-b) => sess-b
  -> prompt(sess-a)
  -> prompt(sess-b)
  -> prompt(sess-a)
  -> assert every update carries the expected sessionId
```

多连接测试：

```text
WS-A initialize -> new sess-a
WS-B initialize -> new sess-b
prompt A and B concurrently
close WS-B
assert WS-A remains alive and sess-a can continue
```

恢复测试：

```text
WS-A new sess-a -> prompt -> disconnect
WS-B initialize -> session/load(sess-a)
record every inbound frame sequence number
assert the final history update sequence < load response sequence
prompt again -> assert same thread/checkpoint
```

另加 reverse RPC 测试：在 sess-a 和 sess-b 中分别触发 fs/read 或 terminal/create，mock ACP client 记录请求参数，断言请求携带对应的真实 `sessionId`，不存在 `"default"`。

### 13.3 Node.js BDD

```gherkin
Feature: one anureo server supports multiple ACP sessions

  Background:
    Given one anureo server is running
    And an ACP client has initialized protocol version 1

  Scenario: alternate prompts between two sessions
    Given session A uses workspace A
    And session B uses workspace B
    When I prompt session A
    And I prompt session B
    And I prompt session A again
    Then every update is routed to its requested session
    And both conversation histories remain isolated

  Scenario: run different sessions concurrently
    Given session A and session B exist
    When both sessions receive prompts concurrently
    Then both prompts complete
    And their updates may interleave without changing session id

  Scenario: reject overlapping turns in one session
    Given session A is processing a prompt
    When I send another prompt to session A
    Then I receive error code -32010

  Scenario: a second connection does not replace the first
    Given connection A owns session A
    And connection B owns session B
    When connection B initializes
    Then connection A remains connected
    And session A can continue

  Scenario: load rebinds a session after disconnect
    Given session A has completed one turn
    And its connection disconnects
    When a new connection loads session A
    Then every history update arrives before the load response
    And the next turn uses the same session id

  Scenario: a restore request cannot change the workspace
    Given session A uses workspace A
    When I load session A with workspace B
    Then I receive an invalid params error

  Scenario: deleting a missing session is idempotent
    When I delete a session id that does not exist
    Then the request succeeds with an empty result
```

Node fixture 必须经过真实 server router、WebSocket upgrade、ACP SDK dispatch、runtime、notification drain 和 JSON-RPC response 路径。测试启动 `acp-test-server`，它与生产 server 使用同一组装函数，只通过 `AcpRuntime.prompt_executor: Arc<dyn AcpPromptExecutor>` 注入确定性 executor。executor 按输入脚本产生固定的 update、延迟、reverse RPC 请求和最终结果，不访问模型网络。

现有 `apps/acp/src/agent.rs::ModelProvider` 只替换 config option 使用的模型列表发现，不替换 `run_agent_with_options` 执行，因此不能作为 prompt E2E fake。`acp-test-server` 仅在 `test-support` feature 下编译，不得被生产 `anureo server` 引用。Node 的职责仅是启动 fixture、交换 wire message 和断言顺序，不复刻 Agent 业务逻辑。

`apps/server/Cargo.toml` 显式声明 binary，避免文件名与命令名不一致：

```toml
[[bin]]
name = "acp-test-server"
path = "src/bin/acp_test_server.rs"
required-features = ["test-support"]
```

### 13.4 验证命令

```powershell
cargo test -p acp --lib
cargo test -p anureo-server-core --test acp_ws_e2e
cargo build -p anureo-server-core --features test-support --bin acp-test-server
cargo check -p anureo-cli
npm.cmd --prefix e2e run test:bdd:acp
```

2026-08-10 的隔离构建验收结果：

| 门槛 | 结果 |
| --- | --- |
| `cargo test -p acp --lib` | 145 passed，0 failed |
| `cargo test -p acp --lib session_repository::tests` | 2 passed，0 failed（含 delete transaction rollback fault-injection） |
| `cargo test -p anureo-server-core acp_hub::tests` | 3 passed，0 failed |
| `cargo test -p anureo-server-core --test acp_ws_e2e` | 8 passed，0 failed |
| `npm.cmd --prefix e2e run test:bdd:acp` | 10 passed，0 failed |
| `Zed ACP smoke`（Node BDD） | 1 passed，0 failed |
| `git diff --check` | 通过 |

Node BDD 的 binary 发现逻辑支持 `CARGO_TARGET_DIR`；CLI 可再用 `ANUREO_BIN` 覆盖，deterministic server 可用 `ACP_TEST_SERVER_BIN` 覆盖。因此并行 CI 不需要共享仓库默认 `target` 目录。

真实模型测试保持 ignored，不作为本设计的常规 CI 门槛。

## 14. 可观测性

`GET /metrics` 返回 Prometheus text format。当前实现提供以下 ACP 聚合指标：

| 指标 | 类型 | 标签 |
| --- | --- | --- |
| `acp_active_connections` | gauge | principal hash/transport |
| `acp_active_sessions` | gauge | lifecycle |
| `acp_active_prompts` | gauge | 无 session id 高基数标签 |
| `acp_total_prompts` | counter | 无高基数标签 |
| `acp_prompt_busy_total` | counter | transport |
| `acp_notification_route_failures_total` | counter | reason |
| `acp_session_rebind_total` | counter | 无高基数标签 |
| `acp_connection_total` | counter | transport |
| `acp_disconnect_total` | counter | transport |

metrics 不包含 session id、prompt 内容、token、MCP secret 或文件内容。外部 exporter 配置属于后续运维接入。

日志包含 connection id、session id、generation 和 principal hash，但不记录 prompt 正文、token、MCP secret 或文件内容。

## 15. 向后兼容与发布策略

### 15.1 Wire 兼容

- baseline ACP 方法和 JSON shape 不变。
- 不增加必需 `_meta` 字段。
- Client 不认识 optional capability 时不受影响。
- `anureo acp` stdio 与 `/acp` WebSocket 使用同一 dispatch。

### 15.2 行为变化

| 行为 | 旧实现 | 新实现 |
| --- | --- | --- |
| 第二条连接建立 | 接管并关闭第一条 | 两条连接并存 |
| capabilities | 最新 initialize 覆盖全局 | 每连接独立 |
| notification | 发给最新 recipient | 发给 session 绑定连接 |
| bridge | 全局 `default` registry | session/connection 显式依赖 |
| 断线 active prompt | 默认 persist | 保持 persist 默认并增加 orphan TTL；cancel 为显式配置 |
| 恢复 | Hub replay buffer | `session/load` / `session/resume` |
| session cwd | 可能 fallback process cwd | 必填绝对目录 |

ACP 标准不规定 transport 断线时 active prompt 必须 persist 或 cancel。本改造不改变现有默认值；若后续要改为 cancel，必须另写 ADR，评估 CLI/Zed 的重连行为并在 release notes 中说明。

### 15.3 Feature flag 建议

不建议长期保留两套 Hub。若需要灰度，可使用短期 server flag：

```text
ANUREO_ACP_CONNECTION_MODEL=legacy|multi-session
```

该 flag 只控制内部连接实现，不影响 ACP wire。完成两个版本验证后删除 legacy 分支，避免长期维护双语义。

## 16. 风险与缓解

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| `agent.rs` 体积大，拆分容易产生大 diff | 回归难定位 | 先参数化 connection，再移动 runtime 代码 |
| transport 建立时尚无 SDK client | 提前构造 bridge 会得到不可用或错误连接 | 创建 connection shell；仅由 initialize handler late-bind 一次；初始化前拒绝 session 方法 |
| notification router backpressure | 丢失终态或阻塞 run | bounded queue；终态发送失败转为 run error并记录指标 |
| load response 越过历史通知 | Client 在恢复完成后仍收到旧历史 | 同一 outbound drain + ack barrier；集成测试按 wire frame 序号断言，不使用 sleep |
| session 重绑与旧 prompt 竞争 | 更新发错连接 | active turn 时禁止 load/resume；`SessionBindings` 使用单锁原子更新双向索引 |
| owner metadata 迁移 | 旧 session 没有 owner | 本地旧记录迁移为 `local-anonymous`；启用 token 后要求显式导入/重新创建 |
| restore 请求借 cwd 改写 session 工作区 | 越权读取或上下文漂移 | new 时保存 canonical cwd；load/resume 再 canonicalize 并要求与已存值相等，不更新 cwd |
| cwd 指向符号链接或被删除 | 工具越界或执行失败 | new/load/resume 时 canonicalize；每次敏感文件操作继续做边界检查 |
| delete 涉及多张表 | 部分删除 | repository transaction + failure injection test |
| background review 在连接断开后发通知 | route failure 噪声 | review 结果先持久化；无连接时不发送实时通知，load 时从持久状态恢复 |
| 全局并发过高 | 模型/CPU/终端资源耗尽 | server semaphore + 每 owner/session 限制 |
| test executor 进入生产构建 | 测试行为被误启用 | 独立 `test-support` feature 和 binary；CI 检查默认 features 下不存在测试入口 |

## 17. 完成定义

- [x] 一个 `anureo server` 只创建一个 `AcpRuntime`。
- [x] transport 先创建 connection shell，SDK client 仅在 initialize 中 late-bind；initialize 前 session 方法被拒绝。
- [x] 一个连接可以创建并交替操作至少两个 session。
- [x] 两个连接可以同时存在，互不触发 lease cancellation。
- [x] 不同 session 可并行，同一 session prompt 重叠稳定返回 busy。
- [x] `SessionBindings` 是连接归属唯一事实源，rebind/unbind 后双向索引一致。
- [x] 每条 `session/update` 都发送到 session 当前绑定连接。
- [x] `session/load` 的完整历史 update 在 load response 前 flush；实现和测试均不依赖 sleep。
- [x] fs/terminal reverse RPC 使用请求 session 的真实 id，不再使用 `"default"` 或全局 bridge registry。
- [x] capabilities 是 per connection，且声明与 handler 完全一致。
- [x] `session/load` 完整回放，`session/resume` 不回放。
- [x] load/resume 的 canonical cwd 必须等于 session 已存 cwd，恢复不能改写工作区。
- [x] close/delete 行为和持久化语义通过测试；delete 不存在或已删除 session 也成功。
- [x] session close/delete 与 prompt unwind 会调用 session bridge cleanup，回收登记的 ACP terminal。
- [x] 默认 persist 与显式 cancel 的断线行为分别通过测试，且都不影响其他连接。
- [x] ACP prompt run 的 cwd 全程来自 session；缺失 cwd 不再 fallback 到 process cwd。
- [x] test-support server 注入 deterministic `AcpPromptExecutor`，生产 binary 不包含测试入口。
- [x] Rust unit、WS integration、Node.js BDD 全部通过，Node 测试覆盖真实 WebSocket/dispatch 路径。
- [x] Zed-compatible `clientInfo`/capabilities 的双 stdio bridge 多 session smoke 通过。
- [x] Zed 使用一个 anureo custom agent 配置完成 UI 级多 thread smoke；两个独立 thread 均收到预期响应。

## 18. 最终结论

单个 `anureo server` 不需要模拟多个 anureo 实例。标准 ACP 已经把独立工作单元定义为 session；正确实现 session 级上下文、连接绑定、通知路由、反向 RPC、并发控制和恢复后，一个 anureo Agent 就能同时支持多个工作区、多个 thread 以及 A/B/A 式交替执行。

本改造最关键的边界是：`AcpRuntime` 属于 server，`AcpConnection` 属于 transport，`SessionEntry` 属于会话。三者生命周期分开后，多 session 和多连接都不需要协议扩展。

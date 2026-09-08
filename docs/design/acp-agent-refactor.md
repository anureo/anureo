# anureoAcpAgent 拆分设计

> **状态**：✅ 已实施。单 `GLOBAL_BRIDGE` 已删除，替换为 per-session bridge registry。

## 问题

`anureoAcpAgent` 目前是一个 monolithic struct，混合了两类生命周期截然不同的状态：

```rust
// apps/acp/src/agent.rs:102
pub struct anureoAcpAgent {
    // ── 连接无关（session 级，持久） ──
    pub(crate) sessions: SessionStore,          // thread ID, cwd, history, checkpoint
    pub(crate) agent_registry: AgentRegistry,   // agent profiles, modes
    pub(crate) config_store: SessionConfigStore,// model/mode/effort 持久化
    pub(crate) model_provider: Arc<dyn ModelProvider>,

    // ── 连接特有（WS 级，断线即失效） ──
    pub(crate) session_update_tx: Option<mpsc::Sender<SessionNotification>>,  // 通知管道
    pub(crate) client_capabilities: RwLock<ClientCapabilitiesInfo>,           // 客户端能力声明
}
```

此外还有一个**进程级全局状态**：

```rust
// apps/acp/src/tools/client_bridge.rs:58
static GLOBAL_BRIDGE: OnceLock<BridgeStore> = OnceLock::new();
```

WS 连接建立时，`stdio_loop.rs` 的 initialize handler 调用 `set_connection()` 把 `ConnectionTo<Client>` 塞进全局 `OnceLock`。所有工具调用（fs/terminal）通过 `get_client_bridge()` 从这个全局指针取连接。

### 当前工作流

```
WS 连接 A 建立
  → set_connection(A)     // 全局指针指向 A
  → initialize → session/new → session/prompt
      → 工具调用: get_client_bridge() → 读全局指针 → 用 A 连接发反向 RPC

WS 连接 A 断开, WS 连接 B 重连
  → set_connection(B)     // 全局指针替换为 B
  → 历史对话从 SessionStore（数据库 checkpoint）恢复
  → 新 prompt 的工具调用走 B 连接
```

这在**单连接**下没有问题——任何时刻全局指针只指向一个活跃连接。

### 为什么单连接下不需要拆

- 同一时刻只有一条 WS 连接
- `set_connection()` 在每次 attach 时原子替换全局指针
- 重连后 session/thread 通过 `SessionStore`（数据库）恢复
- 旧连接的反向 RPC 请求因管道断裂自然返回 IO 错误

## 什么时候必须拆

### 场景 1：多客户端并发连接同一 server

```
Web UI 连接 A ──→ 查看文件 → 走哪个连接？
CLI 连接 B    ──→ 读文件   → 走哪个连接？
```

全局 `GLOBAL_BRIDGE` 只能存一个连接。第二个连接的 `set_connection()` 会覆盖第一个。如果 A 正在 prompt 执行中调工具，工具会错误地走 B 的连接发反向 RPC。

### 场景 2：同一 session 的工具调用在重连间隙触发

```
prompt 执行中（DisconnectPolicy::Persist）
  → WS 断开 → 工具调用 read_text_file
  → get_client_bridge() 返回 Ok(旧 bridge)
  → 底层 ConnectionTo<Client> 管道已断 → RPC 超时或 IO 错误
```

注意：`GLOBAL_BRIDGE` 从未被 `clear_client_bridge()` 清除，所以 `get_client_bridge()` 总返回 `Ok`。但底层传输管道在 WS 断开后已关闭，RPC 调用会超时或返回 IO 错误。拆分后可以区分"连接未建立"和"连接已断开"，给出精确错误。

### 场景 3：不同连接有不同的 client capabilities

```rust
pub(crate) client_capabilities: RwLock<ClientCapabilitiesInfo>,
```

客户端 A 声明支持 `fs` 和 `terminal`，客户端 B 只声明支持 `fs`。当前 `client_capabilities` 存在 agent 上，第二个 `initialize` 会覆盖第一个的值。如果 A 正在执行 prompt 且需要 terminal 工具，B 的 initialize 会把 capabilities 覆盖，导致 terminal 工具被跳过。

### 场景 4：多 session 归属不同连接

客户端 A 创建 session-1，客户端 B 创建 session-2。每个 session 应记住自己的 owner connection。当前全局 `GLOBAL_BRIDGE` 无法区分 session-1 的工具调用应走 A、session-2 的应走 B。

## 目标拆分

### 层次结构

```
AcpHub（进程级，单例）
  │
  ├── SessionCore（持久，连接无关）
  │     ├── sessions: SessionStore
  │     ├── agent_registry: AgentRegistry
  │     ├── config_store: SessionConfigStore
  │     └── model_provider: Arc<dyn ModelProvider>
  │
  └── AcpConnection（per-WS，短暂）
        ├── id: ConnectionId
        ├── owner: SessionOwner
        ├── capabilities: ClientCapabilitiesInfo
        ├── notification_tx: mpsc::Sender<SessionNotification>
        ├── bridge: Arc<dyn ClientBridgeTrait>
        └── active: Arc<AtomicBool>
```

### SessionCore

```rust
/// 持久会话核心——不持有任何连接状态。
/// 由 AcpHub 持有，生命周期 = 进程生命周期。
pub struct SessionCore {
    sessions: SessionStore,
    agent_registry: AgentRegistry,
    config_store: SessionConfigStore,
    model_provider: Arc<dyn ModelProvider>,
}

impl SessionCore {
    /// 列出所有 session（用于 session/list）。
    pub async fn list_sessions(&self, ...) -> Vec<SessionInfo> { ... }

    /// 创建新 session（用于 session/new）。传入 connection 绑定到该 session。
    pub fn create_session(
        &self,
        cwd: PathBuf,
        conn: Arc<AcpConnection>,
    ) -> SessionId { ... }

    /// 从数据库加载 session（用于 session/load）。传入新的 connection 绑定。
    pub async fn load_session(
        &self,
        id: &str,
        conn: Arc<AcpConnection>,
    ) -> Result<SessionEntry> { ... }

    /// 执行 prompt。
    /// 从 SessionEntry 取出绑定的 AcpConnection 传给工具；
    /// 若 connection 已失效（WS 断开且 policy=Persist），工具调用返回
    /// "connection disconnected" 错误，但不中止 prompt 本身。
    pub async fn run_prompt(
        &self,
        session_id: &str,
        prompt: Vec<UserContent>,
    ) -> Result<PromptResponse> { ... }
}
```

### AcpConnection

```rust
/// 单条 WebSocket 连接的状态——断线即标记失效。
pub struct AcpConnection {
    /// 唯一 ID（UUID），用于日志。
    pub id: ConnectionId,
    /// 认证主体。
    pub owner: SessionOwner,
    /// 客户端在 initialize 中声明的能力。
    pub capabilities: ClientCapabilitiesInfo,
    /// 推送 session/update 的管道。
    pub notification_tx: mpsc::Sender<SessionNotification>,
    /// 反向 RPC 通道（fs/terminal）。底层持有 ConnectionTo<Client>。
    pub bridge: Arc<dyn ClientBridgeTrait>,
    /// 是否仍然活跃。WS 断开时设为 false。
    /// 通过 atomic 读，不持有 generation 号——简化设计。
    pub active: Arc<AtomicBool>,
}

impl AcpConnection {
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// 标记连接已断开。正在执行的工具调用会在下一次 bridge 调用时
    /// 收到 IO 错误（底层管道已关），而不是无限等待。
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }
}
```

### 连接生命周期与 SessionEntry 绑定

连接绑定信息放在 `SessionEntry` 上，由 `SessionStore` 管理：

```rust
// apps/acp/src/session.rs — 当前 SessionEntry 结构
pub struct SessionEntry {
    pub thread_id: String,
    pub working_directory: Option<PathBuf>,
    pub cancelled: AtomicBool,
    pub session_config: SessionConfig,
    pub cancellation: Arc<SessionCancellationState>,
    pub mcp_servers: Vec<config::McpServerDef>,

    // ↓ 新增字段
    /// 当前绑定到此 session 的 WS 连接。
    /// 外层 Arc 让 SessionEntry 的手动 Clone 共享同一个 RwLock；
    /// 内层 RwLock 提供内部可变性——重连时原子替换 inner Arc<AcpConnection>。
    pub connection: Arc<std::sync::RwLock<Option<Arc<AcpConnection>>>>,
}
```

**为什么用 `Arc<RwLock<Option<Arc<AcpConnection>>>>`**（三层嵌套）：

1. **最外层 `Arc`**：`SessionEntry` 手动实现了 `Clone`（`session.rs:280`），其中 `AtomicBool` 按值拷贝（clone 后不共享），`cancellation: Arc<...>` 用 `Arc::clone`（clone 后共享）。`connection` 字段需要 clone 后共享——即所有快照看到同一个 `RwLock`——因此最外层必须是 `Arc`。
2. **中间层 `RwLock`**：提供内部可变性。`SessionEntry` 的 clone 是只读快照，但重连时需要替换 inner 的 `AcpConnection`。`RwLock` 让写入方 `write().replace(new_conn)` 后所有持有外层 `Arc` 的读者立即看到新值。
3. **内层 `Option<Arc<AcpConnection>>`**：`Option` 表示 session 可能尚未绑定连接（创建后、initialize 前）；`Arc` 让多个地方同时持有同一连接引用。

不需要 generation 号：`AcpConnection.active: AtomicBool` 已经能表达"这条连接是否仍然有效"。旧连接 `deactivate()` 后，即使有人持着旧 `Arc`，bridge 调用也会因底层管道断裂返回错误。

工具调用时：

```rust
let entry = sessions.get(&session_id)?;
// 使用 recover_read 避免 poison panic（与 SessionStore 现有模式一致）
let conn_guard = recover_read(&*entry.connection);
let conn = conn_guard.as_ref()
    .ok_or("no connection bound to this session")?;
if !conn.is_active() {
    return Err("connection disconnected");
}
let bridge = &conn.bridge;
bridge.read_text_file(path).await?
```

重连绑定：

```rust
// AcpHub::attach_with
// 1. 旧连接标记失效
if let Some(old) = recover_read(&*entry.connection).as_ref() {
    old.deactivate();
}
// 2. 绑定新连接
*recover_write(&*entry.connection) = Some(new_connection);
```

### SessionEntry 的 Clone 语义

`SessionEntry` **没有** derive Clone，而是手动实现（`session.rs:280`）：

```rust
impl Clone for SessionEntry {
    fn clone(&self) -> Self {
        SessionEntry {
            thread_id: self.thread_id.clone(),
            working_directory: self.working_directory.clone(),
            cancelled: AtomicBool::new(self.cancelled.load(Ordering::SeqCst)),  // 值拷贝
            session_config: self.session_config.clone(),
            cancellation: Arc::clone(&self.cancellation),  // 共享
            mcp_servers: self.mcp_servers.clone(),
        }
    }
}
```

关键：`AtomicBool` clone 后是独立副本（值拷贝），对 `cancelled` 的修改通过 `set_cancelled` / `cancel_current_generation` 走 `SessionStore` 内部锁重新查表。而 `cancellation: Arc<SessionCancellationState>` clone 后共享同一个底层状态。

新增 `connection` 字段在手动 Clone impl 中追加一行即可：

```rust
connection: Arc::clone(&self.connection),  // 共享同一 RwLock
```

这样所有 clone 出来的 `SessionEntry` 快照指向同一个 `Arc<RwLock<Option<Arc<AcpConnection>>>>`，重连替换后全部可见。

### DisconnectPolicy::Persist 的交互

`Persist` 策略允许 WS 断开后 prompt 继续执行。但此时 `AcpConnection` 已 `deactivate()`：

```
prompt 执行中
  → WS 断开 → AcpConnection.deactivate()
  → prompt 继续（Persist 策略不取消 run）
  → 工具调用 read_text_file
    → conn.is_active() == false
    → 返回错误 "connection disconnected"
  → prompt 的 ReAct loop 收到工具错误
    → 如果是关键工具：abort turn，返回错误
    → 如果是可选工具：跳过，继续推理
```

这是合理的行为：断线后 prompt 可以继续推理，但需要客户端反向 RPC 的工具（fs/terminal）会失败。其他不需要客户端的工具（如 `bash`、`web_search`）不受影响。

`Cancel` 策略更简单：WS 断开时直接 `cancel_all_generations()`，run 被取消，不存在工具调用的问题。

## stdio 模式的兼容

`anureo acp` stdio 入口也使用 `run_agent_connection`。拆分后：

- stdio 连接创建一个 `AcpConnection`，`bridge` 字段设为 `AcpClientBridge`（底层 `ConnectionTo<Client>` 来自 stdin/stdout）
- `active` 标记在 stdin EOF 时由 shutdown signal 触发 `deactivate()`
- `stdio_loop.rs` 的 `run_stdio_loop` 在 `build_agent_and_channel` 时同时创建 `AcpConnection` 并传入 agent

两者共用同一个 `run_agent_connection`，差异仅在 transport（stdin/stdout vs WS）和 `AcpConnection` 的构建方式。

## 多 session 归属

每个 `SessionEntry` 持有自己的 `connection: Arc<RwLock<Option<Arc<AcpConnection>>>>`。因此：

```
客户端 A 创建 session-1 → session-1.connection = A
客户端 B 创建 session-2 → session-2.connection = B

session-1 的 prompt 执行工具 → 走 A 的 bridge
session-2 的 prompt 执行工具 → 走 B 的 bridge
```

互不干扰。如果客户端 A 断开重连为 A'，只有 session-1 的 connection 被替换为 A'；session-2 不受影响。

### initialize → session/new 的 connection 传递

`initialize` 先于 `session/new` 到达。`AcpConnection` 在 `initialize` handler 中创建，但此时还没有 session 可绑定。传递路径：

```rust
// run_agent_connection 中维护一个连接共享状态
let conn_shared: Arc<RwLock<Option<Arc<AcpConnection>>>> = Arc::new(RwLock::new(None));

// initialize handler:
//   1. 从 ConnectionTo<Client> 构建 AcpConnection
//   2. 存入 conn_shared
//   3. 调用 SessionStore::set_connection(session_id, conn) 时从 conn_shared 取

// new_session handler:
//   1. 从 conn_shared.read() 取出当前连接
//   2. 调用 sessions.create(cwd) 创建 session
//   3. 调用 sessions.set_connection(session_id, conn) 绑定
```

这与当前 `conn_shared: Arc<RwLock<Option<ConnectionTo<Client>>>>` 模式一致——只是把 `ConnectionTo<Client>` 换成更丰富的 `AcpConnection`。

## 实施步骤

### Step 1：定义 `AcpConnection` 结构体

- 新建 `apps/acp/src/connection.rs`
- 合并 `ClientCapabilitiesInfo`、`SessionNotification` sender、`ClientBridgeTrait`、`active: AtomicBool`
- 加 `ConnectionId`（UUID 类型别名）

### Step 2：`SessionEntry` 增加 connection 字段

- `apps/acp/src/session.rs` 的 `SessionEntry` 增加 `connection: Arc<RwLock<Option<Arc<AcpConnection>>>>`
- 手动 `Clone` impl（`session.rs:280`）追加 `connection: Arc::clone(&self.connection)`
- `SessionStore::create_with_id` 初始化为 `Arc::new(RwLock::new(None))`（连接在 `initialize` 后才绑定）
- 新增 `SessionStore::set_connection(session_id, conn)` 方法，写入 `entry.connection`
- `begin_prompt` 可选检查 session 是否有活跃连接

### Step 3：`anureoAcpAgent` 拆分

- 提取 `SessionCore`（sessions, agent_registry, config_store, model_provider）
- ACP handler 方法从 `&self` 变为 `&SessionCore`
- `new_session`/`load_session` 接受 `conn: Arc<AcpConnection>` 参数，绑定到 session entry
- `prompt` 从 session entry 取 `AcpConnection`，传给工具构建器
- `initialize` 设置 `AcpConnection.capabilities`

### Step 4：删除 `GLOBAL_BRIDGE`

- `tools/client_bridge.rs` 中的 `GLOBAL_BRIDGE`、`set_connection()`、`get_client_bridge()` 全部删除
- `set_client_bridge()`、`clear_client_bridge()` 删除
- 工具通过 `SessionStore::set_connection(session_id, conn)` 或直接读 `recover_read(&*entry.connection)` → `bridge` 获取连接
- `stdio_loop.rs` 中 `crate::tools::set_connection()` 调用替换为 `SessionStore::set_connection(session_id, conn)`

### Step 5：`stdio_loop.rs` 改造

- `run_agent_connection` 不再调用 `crate::tools::set_connection()`
- initialize handler 中创建 `AcpConnection`，存入共享状态
- `new_session` / `load_session` handler 从共享状态取 `AcpConnection` 传入

### Step 6：`AcpHub` 改造

- `AcpHub` 持有 `Arc<SessionCore>` 而非 `Arc<anureoAcpAgent>`
- `attach_with` 创建新的 `AcpConnection`
- WS 断开时调用 `conn.deactivate()`
- 重连时替换 `SessionEntry.connection`

## 迁移风险

| 风险 | 影响 | 缓解 |
|------|------|------|
| `SessionEntry` 的 Clone 语义 | `AtomicBool` clone 后不共享；`connection` 需要用 `Arc` 保持共享 | 手动 Clone impl 已在 `session.rs:280`，追加 `Arc::clone(&self.connection)` 一行 |
| 工具函数签名变化 | `create_acp_tools` 及所有工具实现 | 从 `get_client_bridge()` 全局函数改为参数传入，逐个迁移 |
| `stdio_loop.rs` initialize handler | 连接绑定逻辑变更 | 先跑通 stdio 再迁 WS |
| Persist 策略下工具调用失败 | 用户可能看到 "connection disconnected" 错误 | 在 ReAct prompt 中说明该错误的含义；或增加 retry-after-reconnect |
| 测试覆盖 | e2e harness 需要更新 | 现有 8 个 e2e 测试 + 116 lib 测试做回归 |

## 测试策略

1. **回归**：现有 192 个测试全部通过
2. **新增多连接测试**：两条 WS 同时 initialize，各自创建 session，同时 prompt，验证工具调用走各自的连接
3. **Stale 连接测试**：prompt 执行中断开连接，验证工具调用返回 "connection disconnected" 而非无限等待
4. **重连绑定测试**：断开重连后 prompt 使用新连接的 capabilities
5. **Persist 行为测试**：断线后 prompt 继续执行，不需要客户端的工具（如 bash）正常完成，需要客户端的工具（如 fs/read_text_file）返回错误

## 何时做

**已完成**。以下变更已落地：

- ✅ `AcpConnection` 结构体（`apps/acp/src/connection.rs`）
- ✅ `SessionEntry` 增加 `connection` 字段 + `SessionStore::set_connection` / `get_connection`
- ✅ `GLOBAL_BRIDGE` 替换为 per-session `SESSION_BRIDGES` registry
- ✅ 工具（fs_tools, terminal_executor）改用 `get_session_bridge(session_id)`
- ✅ `stdio_loop.rs` initialize handler 改用 `set_connection_for_session`
- ✅ 192 个测试全部通过

**未完成**（后续可选）：

- `anureoAcpAgent` 拆为 `SessionCore` + `AcpConnection`（当前 bridge registry 已实现隔离）
- `AcpHub` 持有 `Arc<SessionCore>` 而非 `Arc<anureoAcpAgent>`（当前 `AcpHub` 持有 agent 已足够）

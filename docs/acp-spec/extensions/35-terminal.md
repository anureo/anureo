# Terminal（终端生命周期扩展）

> 命名空间: `_anureo.dev/terminal/*`
> Capability key: `terminal`

## Capability

```json
{
  "terminal": {
    "restart": true,
    "force_kill": true
  }
}
```

- 声明 `terminal` capability 后，client 可以重启终端 session 和强制终止终端进程树。
- ACP 标准 `terminal/*` reverse-RPC 覆盖 create/connect/send/resize/close；本扩展覆盖 Anureo 特有的终端生命周期管理。

### 与标准 ACP `terminal/*` 的关系

| 操作 | 标准 ACP `terminal/*` | `_anureo.dev/terminal/*` 扩展 |
|---|---|---|
| 创建终端 | `terminal/create` | — |
| 连接终端 | `terminal/connect` | — |
| 发送输入 | `terminal/send` | — |
| 调整大小 | `terminal/resize` | — |
| 关闭终端 | `terminal/close` | — |
| 重启终端 | — | `terminal/restart` |
| 强制终止 | — | `terminal/force_kill` |

- 标准操作覆盖终端的正常生命周期（从创建到关闭）。
- 扩展操作处理 Anureo 特有的场景：
  - **Restart**：销毁并重建 PTY，但保持 sessionId 不变——UI 不需要重新订阅。
  - **Force kill**：当子进程不响应 SIGTERM 时的最后手段，使用 SIGKILL 终止整个进程树。

---

## Methods

### `_anureo.dev/terminal/restart`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `terminal.restart` |
| 权限 | Server-side authorization（需要写权限 scope） |
| 超时 | 建议 10s |

**Request:**

```json
{
  "sessionId": "term_abc123",
  "preserveHistory": true,
  "clearScreen": false
}
```

**Response:**

```json
{
  "sessionId": "term_abc123",
  "restarted": true,
  "newPid": 12345,
  "message": "Terminal restarted successfully."
}
```

**Rust 类型:**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct TerminalRestartRequest {
    /// Terminal session ID (remains unchanged after restart)
    pub session_id: String,
    /// When true, preserve scrollback history in UI
    #[serde(default = "default_true")]
    pub preserve_history: bool,
    /// When true, clear terminal screen before new PTY output
    #[serde(default)]
    pub clear_screen: bool,
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize)]
pub struct TerminalRestartResponse {
    pub session_id: String,
    pub restarted: bool,
    /// New process ID of the restarted terminal
    pub new_pid: u32,
    pub message: String,
}
```

**逻辑说明:**

1. **sessionId 保持不变**——这是 restart 与 close+create 的核心区别。UI 不需要重新订阅 terminal updates。
2. Server 销毁当前的 PTY 进程及其所有子进程（先尝试 SIGTERM，等待 2 秒后 SIGKILL）。
3. Server 使用原始终端配置（command、cwd、env、cols、rows）创建新的 PTY。
4. `preserveHistory: true`（默认）时，server 通知 client 保留 scrollback 历史——新的 PTY 输出追加到历史之后。
5. `preserveHistory: false` 时，client 清空 scrollback，只显示新 PTY 的输出。
6. `clearScreen: true` 时，新 PTY 启动后先发送 clear screen escape sequence（`\x1b[2J\x1b[H`）。
7. 新 PTY 的 PID 记录在 `newPid` 中，client 可以用于进程监控。
8. Restart 期间，terminal session 暂时不可用（无法 send/resize），restart 完成后恢复正常。
9. 如果原始终端配置包含 `cwd`，restart 后 cwd 为原始值（不继承之前 cd 的目录）。
10. Restart 用于：终端卡死、shell 崩溃、需要重置环境变量等场景。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `terminal.restart` | initialize 未声明 |
| `not_found` | Terminal session 不存在 | `sessionId` 无效 |
| `forbidden` | 无权限 | server authorization 拒绝 |
| `internal_error` | Server 内部错误 | PTY 重建失败 |

---

### `_anureo.dev/terminal/force_kill`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `terminal.force_kill` |
| 权限 | Server-side authorization（需要高危操作 scope） |
| 幂等 | 是——kill 已终止的进程是 no-op |

**Request:**

```json
{
  "sessionId": "term_abc123",
  "confirmToken": "force-kill-2025-08-19T10:00:00Z"
}
```

**Response:**

```json
{
  "sessionId": "term_abc123",
  "killed": true,
  "killedPids": [12344, 12345, 12346],
  "signal": "SIGKILL",
  "message": "Process tree terminated (3 processes killed)."
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct TerminalForceKillRequest {
    pub session_id: String,
    /// Client-generated confirm token to prevent accidental kill
    pub confirm_token: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TerminalForceKillResponse {
    pub session_id: String,
    pub killed: bool,
    /// All PIDs that received SIGKILL (including children)
    pub killed_pids: Vec<u32>,
    pub signal: String,
    pub message: String,
}
```

**逻辑说明:**

1. Server 使用 SIGKILL 终止终端的**整个进程树**（shell 进程及其所有子进程）。
2. 进程树遍历：从 terminal session 的 root PID 开始，递归查找所有子进程。
3. `confirmToken` 为 client 生成的确认令牌，server 要求非空作为防误触的 client-side gate。
4. 与 `terminal/restart` 不同，`force_kill` **不重建 PTY**——terminal session 进入 terminated 状态。
5. Terminal session 的标准 `terminal/close` 仍需要执行（释放 session 资源）。
6. **Server 必须记录使用日志**：包括 timestamp、sessionId、killed PIDs、调用者身份。这是审计要求。
7. Force kill 用于：子进程不响应 SIGTERM、进程僵死、资源泄露等紧急场景。
8. `killedPids` 列出所有被 SIGKILL 的进程 ID，client 可以用于确认和日志记录。
9. 如果进程已终止（PID 不存在），返回 `killed: true`、`killedPids: []`（no-op）。
10. 跨平台行为：
    - Unix (macOS/Linux)：`kill(pid, SIGKILL)` 递归子进程。
    - Windows：`taskkill /F /T /PID` 终止进程树。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `terminal.force_kill` | initialize 未声明 |
| `not_found` | Terminal session 不存在 | `sessionId` 无效 |
| `forbidden` | 无高危操作权限 | server authorization 拒绝 |
| `invalid_params` | 参数校验失败 | `confirmToken` 为空 |
| `internal_error` | Server 内部错误 | 进程树遍历或 kill 失败 |

### 审计日志格式

Server 必须为每次 `force_kill` 调用记录审计日志：

```json
{
  "timestamp": "2025-08-19T10:00:05Z",
  "action": "terminal.force_kill",
  "sessionId": "term_abc123",
  "callerConnectionId": "conn_xyz",
  "callerIdentity": "user@example.com",
  "killedPids": [12344, 12345, 12346],
  "reason": "client_request"
}
```

- 审计日志存储在 server diagnostics 日志目录中。
- 日志保留时间由 server 配置决定（建议至少 30 天）。
- 日志不包含终端内容或用户数据。

---

## Notifications

Terminal 扩展域**没有专属 notification**。

终端的状态变化通过标准 ACP `terminal/on_data`、`terminal/on_exit` 等 reverse-RPC notification 传输。

- `terminal/restart` 完成后，client 会收到标准 `terminal/on_exit`（旧 PTY 退出）和新的 `terminal/on_data`（新 PTY 输出）。
- `terminal/force_kill` 后，client 会收到标准 `terminal/on_exit`（进程被杀死）。

---

## Reconnect Resync 映射

Terminal 扩展域**没有 reconnect resync 需求**。

- 终端的当前状态（存活/已退出）通过标准 ACP `terminal/connect` 恢复。
- `restart` 和 `force_kill` 是一次性操作，不维护需要 resync 的状态。
- Client 重连后如果需要确认终端状态，通过标准 `terminal/connect` 重新建立连接。

# 07 — 实现步骤

> **Scope**: S1→S7 增量实现，含精确文件变更和验证方式  
> **依赖**: [01-wire-protocol], [02-acp-client], [03-auto-spawn], [04-run-acp-mode], [05-display-bridge], [06-cli-args-dispatch]

## 概览

```
S1: server_bootstrap 提取     ← 重构，不改变行为
S2: AcpClient 核心             ← 新增，编译通过
S3: AcpClient 高层方法         ← 新增，编译通过
S4: DisplayBridge              ← 新增，编译通过
S5: run_acp_mode 单次执行      ← 新增，手动验证
S6: CLI 接线（args + main.rs）  ← 接线，端到端验证
S7: 交互式 REPL                ← 增强，手动验证
```

每步完成后必须：编译通过 + 相关测试通过。

---

## S1: server_bootstrap 提取

**目标**：从 `ws_bridge.rs` 提取 auto-spawn 逻辑为独立公共模块。

### 文件变更

| 文件 | 操作 | 说明 |
|------|------|------|
| `apps/acp/src/server_bootstrap.rs` | 新增 | 从 `ws_bridge.rs` 提取的所有函数和常量 |
| `apps/acp/src/lib.rs` | 修改 | 添加 `pub mod server_bootstrap;` |
| `apps/acp/src/ws_bridge.rs` | 修改 | 删除提取的私有函数，改为 `use crate::server_bootstrap::*` |

### 具体步骤

1. 创建 `apps/acp/src/server_bootstrap.rs`，内容见 [03-auto-spawn.md](./03-auto-spawn.md)
2. 在 `apps/acp/src/lib.rs` 添加：
   ```rust
   pub mod server_bootstrap;
   ```
3. 在 `apps/acp/src/ws_bridge.rs` 中：
   - 删除：`parse_host_port`, `health_url`, `probe_client`, `probe_server`, `resolve_anureo_binary`, `spawn_server`, `spawn_reaper`, `ensure_server_ready`, `build_ws_request`
   - 删除：`DEFAULT_WS_URL`, `SERVER_READY_TIMEOUT`, `PROBE_INTERVAL`, `RECONNECT_INITIAL_BACKOFF`, `RECONNECT_MAX_BACKOFF`, `CONNECT_TIMEOUT` 常量
   - 添加：`use crate::server_bootstrap::{...};` 导入所需函数
   - 迁移测试到 `server_bootstrap.rs` 的 `#[cfg(test)] mod tests`
4. 删除 `run_server_mode.rs` 中的重复 `build_ws_request`（如果存在）

### 验证

```bash
# 编译
cargo build -p anureo-acp

# 单元测试（迁移的测试）
cargo test -p anureo-acp server_bootstrap

# ws_bridge 仍然正常工作
cargo test -p anureo-acp ws_bridge

# 确认 IDE 集成不受影响
cargo build -p anureo
anureo acp --help
```

---

## S2: AcpClient 核心（connect + reader loop）

**目标**：实现 `AcpClient` 的连接和后台 reader loop。

### 文件变更

| 文件 | 操作 | 说明 |
|------|------|------|
| `apps/cli/src/server_transport/acp_client.rs` | 新增 | `AcpClient` 结构体、`connect()`、`reader_loop()`、`route_response()`、`route_notification()` |
| `apps/cli/src/server_transport/mod.rs` | 修改 | 添加 `mod acp_client;` |

### 具体步骤

1. 创建 `apps/cli/src/server_transport/acp_client.rs`
2. 定义 `AcpClientError` 枚举（见 [02-acp-client.md](./02-acp-client.md)）
3. 定义 `AcpSessionUpdate` 枚举
4. 定义 `AcpClient` 结构体
5. 实现 `connect()` — 建立 WS 连接，启动 reader/writer task
6. 实现 `reader_loop()` — 解析 JSON-RPC 消息，路由 response/notification
7. 实现 `route_response()` / `route_notification()`
8. 实现 `parse_session_update()` — JSON → `AcpSessionUpdate`
9. 实现 `build_ws_request()` — 复制自 `server_bootstrap`（或直接引用）
10. 在 `mod.rs` 添加 `mod acp_client;`

### 验证

```bash
# 编译
cargo build -p anureo

# 单元测试
cargo test -p anureo acp_client::tests::test_parse_agent_message_chunk
cargo test -p anureo acp_client::tests::test_parse_tool_call_started
cargo test -p anureo acp_client::tests::test_parse_usage_update
```

---

## S3: AcpClient 高层方法

**目标**：实现 `initialize()`, `new_session()`, `prompt()`, `cancel()`, `load_session()`, `shutdown()`。

### 文件变更

| 文件 | 操作 | 说明 |
|------|------|------|
| `apps/cli/src/server_transport/acp_client.rs` | 修改 | 添加高层方法 |

### 具体步骤

1. 实现 `send_request()` — 通用 JSON-RPC 请求/响应（见 [02-acp-client.md](./02-acp-client.md)）
2. 实现 `send_notification()` — JSON-RPC 通知
3. 实现 `initialize()` — 发送 initialize 请求
4. 实现 `new_session()` — 发送 session/new 请求
5. 实现 `prompt()` — 发送 session/prompt，返回 `PromptStream`
6. 实现 `cancel()` — 发送 session/cancel 通知
7. 实现 `load_session()` — 发送 session/load 请求
8. 实现 `shutdown()` — 关闭 WS，等待 reader 退出

### 验证

```bash
# 编译
cargo build -p anureo

# 手动测试（需要 anureo-server 运行）
anureo server &
cargo test -p anureo acp_client -- --ignored
```

---

## S4: DisplayBridge

**目标**：实现 ACP notification → CLI display 层的转换桥接。

### 文件变更

| 文件 | 操作 | 说明 |
|------|------|------|
| `apps/cli/src/server_transport/display_bridge.rs` | 新增 | `DisplayBridge` 结构体 + `convert_acp_to_stream_event()` |
| `apps/cli/src/server_transport/mod.rs` | 修改 | 添加 `mod display_bridge;` |

### 具体步骤

1. 研究 `StreamEvent` 和 `TypedAnyStreamEvent` 的确切字段（`lsp hover` 或 `grep`）
2. 实现 `convert_acp_to_stream_event()` — 见 [05-display-bridge.md](./05-display-bridge.md)
3. 实现 `DisplayBridge::from_args()` — 从 Args 构建 StreamDisplayConfig + EventState
4. 实现 `DisplayBridge::handle_update()` — 转换 + 回调
5. 实现 `DisplayBridge::print_result()` — 打印最终 usage
6. 为 `AcpSessionUpdate` 添加 `Serialize`（用于 JSON 输出模式）
7. 在 `mod.rs` 添加 `mod display_bridge;`

### 验证

```bash
# 编译
cargo build -p anureo

# 确认现有 display 层未被破坏
cargo test -p anureo display
```

---

## S5: run_acp_mode（单次执行）

**目标**：实现 `run_acp_mode()` 的完整编排逻辑（单次执行）。

### 文件变更

| 文件 | 操作 | 说明 |
|------|------|------|
| `apps/cli/src/server_transport/run_acp_mode.rs` | 新增 | 编排逻辑 |
| `apps/cli/src/server_transport/mod.rs` | 修改 | 添加 `mod run_acp_mode;` 和 `pub use` |

### 具体步骤

1. 实现 `run_acp_mode()` 入口函数 — 见 [04-run-acp-mode.md](./04-run-acp-mode.md)
2. 实现 `run_acp_single_turn()` — 单次 prompt
3. 实现 `display_prompt_turn()` — `select!` 循环消费 updates + final response
4. 实现 `apply_session_overrides()` — model/tier/agent 配置
5. 在 `mod.rs` 添加 `mod run_acp_mode;` 和 `pub use run_acp_mode::run_acp_mode;`

### 验证

```bash
# 编译
cargo build -p anureo

# 手动端到端测试
anureo server &
sleep 2
# 通过环境变量临时启用（或直接调用函数）
cargo test -p anureo run_acp_mode -- --ignored
```

---

## S6: CLI 接线（args + main.rs）

**目标**：添加 `--remote` 参数并在 `main.rs` 中分发。

### 文件变更

| 文件 | 操作 | 说明 |
|------|------|------|
| `apps/cli/src/args.rs` | 修改 | 添加 `remote` 字段 |
| `apps/cli/src/main.rs` | 修改 | 添加 remote 分发 |

### 具体步骤

1. 在 `args.rs` 的 `Args` 结构体中添加 `remote: Option<String>` 字段（见 [06-cli-args-dispatch.md](./06-cli-args-dispatch.md)）
2. 在 `main.rs` 的 `init_logging()` 之后、`Session` 子命令之前插入 remote 分支
3. 添加参数解析测试

### 验证

```bash
# 编译
cargo build -p anureo

# 参数解析测试
cargo test -p anureo args::tests::remote

# 帮助文本
anureo --help | grep remote

# 端到端测试
anureo server &
sleep 2
anureo --remote "hello, what can you do?"
# 应看到流式输出和工具调用展示
```

---

## S7: 交互式 REPL

**目标**：实现交互式多轮对话（`--remote -i`）。

### 文件变更

| 文件 | 操作 | 说明 |
|------|------|------|
| `apps/cli/src/server_transport/run_acp_mode.rs` | 修改 | 添加 `run_acp_interactive()` |

### 具体步骤

1. 实现 `run_acp_interactive()` — REPL 循环（见 [04-run-acp-mode.md](./04-run-acp-mode.md)）
2. 实现 `read_line()` — stdin 读取
3. 添加 Ctrl+C 取消处理（取消当前 turn，不退出 REPL）
4. 支持 `/exit` / `/quit` 命令

### 验证

```bash
# 编译
cargo build -p anureo

# 手动测试
anureo server &
sleep 2
anureo --remote -i
# 在 REPL 中：
# › hello
# › what tools do you have?
# › /exit
```

---

## 完整文件清单

### 新增文件

| 文件 | 步骤 | 行数估计 |
|------|------|---------|
| `apps/acp/src/server_bootstrap.rs` | S1 | ~200 |
| `apps/cli/src/server_transport/acp_client.rs` | S2-S3 | ~500 |
| `apps/cli/src/server_transport/display_bridge.rs` | S4 | ~200 |
| `apps/cli/src/server_transport/run_acp_mode.rs` | S5, S7 | ~300 |

### 修改文件

| 文件 | 步骤 | 改动量 |
|------|------|--------|
| `apps/acp/src/lib.rs` | S1 | +1 行 (`pub mod server_bootstrap;`) |
| `apps/acp/src/ws_bridge.rs` | S1 | 重构（删除提取的函数，添加 import） |
| `apps/cli/src/server_transport/mod.rs` | S2,S4,S5 | +4 行 (`mod` 声明 + `pub use`) |
| `apps/cli/src/args.rs` | S6 | +10 行（`remote` 字段） |
| `apps/cli/src/main.rs` | S6 | +8 行（remote 分发） |

---

## 风险与缓解

| 风险 | 概率 | 缓解 |
|------|------|------|
| `TypedAnyStreamEvent` 类型构造复杂，无法直接构造 | 中 | S4 前先用 `lsp hover` 确认类型结构；可能需要添加 `From` 实现 |
| `session/update` 的 `kind` 字段命名与预期不同 | 低 | S2 中用真实 server 测试 `parse_session_update` |
| WS writer task 与 reader task 的生命周期管理 | 中 | S2 中确保 `shutdown()` 正确等待 reader 退出 |
| display 层的 `create_stdio_event_callback` 签名不匹配 | 中 | S4 前检查签名，必要时适配 |
| `agent_client_protocol` 不在 CLI 依赖中 | 低 | S1 前检查 Cargo.toml，通过 `anureo_acp` re-export |

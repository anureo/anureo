# 标题生成功能测试方案

## 测试目标

验证 session 标题生成的正确性和可靠性，确保：
1. 标题在第一轮对话后正确生成
2. 生成的标题正确持久化到数据库
3. 标题正确发送到客户端
4. 标题生成失败时的回退机制正常工作

## 测试场景

### 场景 1：正常标题生成

**步骤：**
1. 启动 anureo server
2. 创建新的 session
3. 发送第一轮用户消息
4. 等待 agent 响应完成
5. 检查数据库中的 title 字段
6. 检查客户端是否收到 `SessionInfoUpdate` 事件

**预期结果：**
- 数据库中 `acp_sessions` 表的 `title` 字段不为 NULL
- 标题长度 ≤ 50 字符
- 客户端收到包含 title 的 `SessionInfoUpdate` 事件

### 场景 2：标题生成失败（LLM 调用失败）

**步骤：**
1. 模拟 LLM 调用失败（例如：网络错误、API 错误）
2. 创建新的 session
3. 发送第一轮用户消息
4. 等待 agent 响应完成
5. 检查数据库中的 title 字段
6. 检查 session/list 接口返回的 title

**预期结果：**
- 数据库中 `acp_sessions` 表的 `title` 字段为 NULL
- `session/list` 接口返回的 title 为 `"Session {session_id 前8位}"` 格式

### 场景 3：多轮对话（标题只在第一轮生成）

**步骤：**
1. 创建新的 session
2. 发送第一轮用户消息，等待响应完成
3. 记录生成的标题
4. 发送第二轮用户消息，等待响应完成
5. 检查数据库中的 title 字段是否变化

**预期结果：**
- 标题只在第一轮对话后生成
- 后续对话不会覆盖或更新标题

### 场景 4：空字符串标题处理

**步骤：**
1. 模拟 LLM 返回空字符串或只包含空白字符
2. 创建新的 session
3. 发送第一轮用户消息
4. 等待 agent 响应完成
5. 检查数据库中的 title 字段

**预期结果：**
- 数据库中 `acp_sessions` 表的 `title` 字段为 NULL（`persist_session_title` 会过滤空字符串）

## 测试方法

### 方法 1：手动测试（CLI）

```powershell
# 1. 启动 dev server
cargo run -p anureo-cli -- server --port 3031 --home .anureo-home --pid-file .anureo-home/anureo-server.pid

# 2. 在另一个终端启动 ACP agent
cargo run -p anureo-cli -- acp

# 3. 发送测试消息并观察日志
# 查看 title 生成相关的日志（warn! 级别会记录失败情况）

# 4. 检查数据库
# 使用 SQLite 工具查看 .anureo-home/agents.db
sqlite3 .anureo-home/agents.db "SELECT session_id, title, created_at FROM acp_sessions ORDER BY created_at DESC LIMIT 5;"
```

### 方法 2：单元测试

在 `agent/agent-core/src/agent/react/title_generator.rs` 中已有单元测试：
- `clamp_*` 系列测试：验证标题截断逻辑
- `build_title_messages_*` 测试：验证消息构建逻辑

可以添加：
- `generate_title_success`：模拟成功的 LLM 调用
- `generate_title_failure`：模拟失败的 LLM 调用
- `generate_title_empty_response`：模拟空响应

### 方法 3：集成测试

创建集成测试文件 `apps/acp/tests/title_generation.rs`：

```rust
#[tokio::test]
async fn test_title_generated_on_first_turn() {
    // 1. 创建临时数据库
    // 2. 启动 agent runner
    // 3. 发送第一轮消息
    // 4. 等待完成
    // 5. 检查数据库中的 title 字段
}

#[tokio::test]
async fn test_title_fallback_when_llm_fails() {
    // 1. Mock LLM provider 返回错误
    // 2. 创建 session 并发送消息
    // 3. 检查回退逻辑是否正确
}
```

### 方法 4：端到端测试（WebSocket）

使用 WebSocket 客户端测试：

```typescript
// 测试脚本
const ws = new WebSocket('ws://localhost:3031/acp');
ws.onmessage = (event) => {
    const data = JSON.parse(event.data);
    if (data.type === 'session_info_update') {
        console.log('Received title update:', data.title);
        // 断言 title 不为 undefined/null
    }
};
```

## 诊断命令

```powershell
# 查看最近的 session 和标题
sqlite3 .anureo-home/agents.db "SELECT session_id, title, created_at FROM acp_sessions ORDER BY created_at DESC LIMIT 10;"

# 检查没有标题的 session
sqlite3 .anureo-home/agents.db "SELECT session_id, created_at FROM acp_sessions WHERE title IS NULL;"

# 查看 server 日志中的标题生成警告
Get-Content .anureo-home/anureo-server.log | Select-String "Title generation"

# 实时监控日志
Get-Content .anureo-home/anureo-server.log -Wait -Tail 50 | Select-String "title|Title"
```

## 常见问题排查

### 问题 1：所有 session 都显示 "Untitled session"

**可能原因：**
- 前端没有正确处理 `SessionInfoUpdate` 事件
- `session/list` 接口返回的 title 格式不匹配前端期望

**排查步骤：**
1. 检查数据库中是否有 title：`SELECT session_id, title FROM acp_sessions LIMIT 10;`
2. 检查 server 日志中是否有标题生成的日志
3. 使用 WebSocket 监控工具查看 `session_info_update` 事件

### 问题 2：标题只在部分 session 中生成

**可能原因：**
- 第一轮对话被中断（用户取消、网络错误等）
- LLM provider 配置问题

**排查步骤：**
1. 检查 server 日志中的 "Title generation failed" 警告
2. 验证 LLM provider 配置和凭证
3. 检查 `is_first_turn` 判断逻辑是否正确

### 问题 3：标题更新不及时

**可能原因：**
- `set_title` 执行失败（数据库锁定、权限问题等）
- 事件发送失败

**排查步骤：**
1. 检查 server 日志中的 "failed to persist session title" 警告
2. 检查数据库文件权限
3. 使用 WebSocket 工具验证事件是否发送

## 测试检查清单

- [ ] 场景 1：正常标题生成
- [ ] 场景 2：标题生成失败（LLM 调用失败）
- [ ] 场景 3：多轮对话（标题只在第一轮生成）
- [ ] 场景 4：空字符串标题处理
- [ ] 数据库持久化验证
- [ ] 客户端事件接收验证
- [ ] 回退机制验证
- [ ] 日志和错误处理验证

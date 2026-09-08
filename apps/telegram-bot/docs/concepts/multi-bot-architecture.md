# 多 Bot 架构

单进程运行多个 Telegram Bot，各自独立长轮询，共享同一个 anureo Agent 引擎。

## 什么时候用

- 你有多个 Bot（如：客服 Bot + 开发 Bot），想用一个进程管理
- 你想在不同群组中部署不同人格/模型的 Bot

## 核心概念

### Bot 配置映射

每个 Bot 在 `telegram-bot.toml` 的 `[[bots]]` 数组中定义。`load_config()` 解析后，为每个 *enabled* 的 Bot 生成一个独立的 teloxide `Bot` 实例和 `Dispatcher`。

```toml
[[bots]]
name = "assistant"
token = "${ASSISTANT_TOKEN}"
allowed_chats = [-1001234567890]
interaction_mode = "streaming"
model = "gpt-4"

[[bots]]
name = "dev-bot"
token = "${DEV_BOT_TOKEN}"
allowed_chats = []
interaction_mode = "periodic_summary"
model = "claude-3-opus"
```

### 长轮询与并发

`run_with_config()` 为每个 Bot 启动一个独立的 tokio 任务：

1. 创建 `Dispatcher`，绑定 `default_handler`
2. 调用 `dispatcher.dispatch()` 进入长轮询
3. 每个任务持有独立的 `CancellationToken`，支持优雅关闭

所有 Bot 共享同一个 `HandlerDeps` 容器（含配置、模型搜索、Session 管理），但 `ChatRunRegistry` 确保同一聊天不会并发执行两次 Agent。

### 依赖注入

`HandlerDeps` 是所有消息处理依赖的容器：

- `Bot` — teloxide 客户端
- `Settings` — 全局配置
- `CommandDispatcher` — 命令分发器
- `InMemorySearchSession` — 模型搜索会话
- `ChatRunRegistry` — 防止同一聊天并发执行
- `BotMetrics` — 指标收集

## 最佳实践

✅ 多个 Bot 时，用环境变量引用 Token（`"${BOT_TOKEN}"`），不要硬编码
✅ 用 `allowed_chats` 限制 Bot 响应范围，避免误响应
⚠️ 同一进程内的 Bot 共享 `Settings`，不支持每个 Bot 独立 LLM provider

## 本页覆盖范围

- 覆盖：多 Bot 配置、长轮询并发、依赖注入
- 不覆盖：消息处理流程（见 [消息处理管线](message-pipeline.md)）、流式响应细节（见 [流式 Agent 响应](streaming-agent.md)）

## 下一步

- [流式 Agent 响应](streaming-agent.md) — 理解消息如何实时生成
- [配置系统](configuration.md) — 完整配置选项

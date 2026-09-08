# 流式 Agent 响应

Bot 收到消息后调用 anureo Agent，实时在 Telegram 中更新 Think / Act / Tool 阶段的输出。

## 什么时候用

- 你想让用户在等待 AI 回复时看到实时进展，而不是等全部生成完再发
- 你需要调试 Agent 的思考过程和工具调用

## 核心概念

### 两种交互模式

| 模式 | 行为 | 适用场景 |
|------|------|---------|
| `streaming`（推荐） | 实时编辑消息，展示 Agent 每一步输出 | 大多数场景 |
| `periodic_summary` | 周期性更新摘要，减少 API 调用 | 高流量群组、消息频繁的场景 |

### 流式处理管线

消息到达 Agent 后，数据流经三个阶段：

1. **Agent 执行**（`streaming/agent.rs`）— 调用 `anureo::run_agent_with_options()`，产生 `AnyStreamEvent` 事件流
2. **事件映射**（`streaming/event_mapper.rs`）— 将 anureo 事件转为 `StreamCommand`（Adapter 模式）
3. **消息处理**（`streaming/message_handler.rs`）— 消费 `StreamCommand`，按策略编辑 Telegram 消息

```
anureo Agent → AnyStreamEvent → StreamEventMapper → StreamCommand → StreamMessageHandler → Telegram API
```

### 节流策略

Telegram API 对消息编辑有频率限制。`StreamMessageHandler` 使用自适应节流：

- 小消息（< 200 字符）：每 300ms 更新一次
- 大消息（> 3000 字符）：降低更新频率
- 关键命令（如错误、完成）：立即发送，不受节流限制

### 并发控制

`ChatRunRegistry` 确保同一聊天（chat_id）同时只有一个 Agent 在运行。新消息到达时如果前一轮未完成，会返回"请稍等"提示。

## 最佳实践

✅ 默认使用 `streaming` 模式，体验最佳
✅ 在高频群组中切换为 `periodic_summary`，减少 API 压力
⚠️ 不要在 `allowed_chats` 为空的高流量群组使用 `streaming` 模式

## 本页覆盖范围

- 覆盖：流式模式、事件映射、节流策略、并发控制
- 不覆盖：消息如何到达 Agent（见 [消息处理管线](message-pipeline.md)）、配置字段（见 [配置文件参考](../reference/config-reference.md)）

## 下一步

- [消息处理管线](message-pipeline.md) — 完整的消息流转过程
- [斜杠命令系统](slash-commands.md) — 如何切换模型和模式

# anureo Telegram Bot

多 Bot 管理框架，单进程运行多个 AI 对话机器人。

## 选择你的起点

| 如果你想 | 读这里 | 为什么 |
|---------|--------|--------|
| 5 分钟跑通第一个 Bot | [Quickstart](quickstart.md) | 最快上手路径 |
| 理解整体架构 | [多 Bot 架构](concepts/multi-bot-architecture.md) | 先懂全貌再深入 |
| 配置你的 Bot | [配置系统](concepts/configuration.md) | 搞懂配置文件和优先级 |
| 自定义命令 | [斜杠命令系统](concepts/slash-commands.md) | 扩展 Bot 行为 |
| 排查问题 | [故障排查](troubleshooting.md) | 常见错误和解决方案 |

## 推荐阅读顺序

1. [Quickstart](quickstart.md) — 第一次使用必读
2. [多 Bot 架构](concepts/multi-bot-architecture.md) — 理解核心设计
3. [流式 Agent 响应](concepts/streaming-agent.md) — 理解消息如何实时更新
4. [配置系统](concepts/configuration.md) — 按需配置

大多数开发者读到这里就够了。以下是进阶内容：

- [消息处理管线](concepts/message-pipeline.md) — 消息从接收到回复的完整流程
- [配置文件参考](reference/config-reference.md) — 所有配置字段的完整说明
- [Bot API 参考](reference/bot-api.md) — 核心 trait 和接口说明

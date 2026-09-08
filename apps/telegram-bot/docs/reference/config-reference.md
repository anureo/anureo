# 配置文件参考

`telegram-bot.toml` 完整字段说明。文件路径：`~/.anureo/telegram-bot.toml`。

## `[settings]` — 全局设置

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `download_dir` | string | 否 | `"downloads"` | 文件下载目录（相对或绝对路径） |
| `log_level` | string | 否 | `"info"` | 日志级别（trace/debug/info/warn/error） |
| `log_file` | string | 否 | — | 日志文件路径（不设置则仅输出到控制台） |

## `[[bots]]` — Bot 实例

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `name` | string | 是 | Bot 名称标识，用于日志和调试 |
| `token` | string | 是 | Telegram Bot Token，支持 `${ENV_VAR}` 引用 |
| `enabled` | bool | 否 | 是否启用，默认 `true` |
| `allowed_chats` | int[] | 否 | 允许的 chat_id 列表。空数组 = 允许所有 |
| `interaction_mode` | string | 否 | `"streaming"`（推荐）或 `"periodic_summary"` |
| `model` | string | 否 | 默认模型 ID，可通过 `/model` 命令切换 |
| `system_prompt` | string | 否 | 自定义系统提示词 |
| `agent_name` | string | 否 | anureo Agent 名称 |

## `[settings.agent]` — Agent 配置

| 字段 | 类型 | 必填 | 说明 |
|------|------|------|------|
| `max_tokens` | int | 否 | 单次回复最大 token 数 |
| `temperature` | float | 否 | 生成温度，0.0-1.0 |

## `[settings.streaming]` — 流式配置

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `edit_throttle_ms` | int | 否 | 300 | 消息编辑最小间隔（毫秒） |
| `small_threshold` | int | 否 | 200 | 小消息字符阈值 |
| `large_threshold` | int | 否 | 3000 | 大消息字符阈值 |

## 完整配置示例

```toml
# ~/.anureo/telegram-bot.toml

[settings]
download_dir = "downloads"
log_level = "info,teloxide=off"
log_file = "/var/log/telegram-bot.log"

[settings.agent]
max_tokens = 4096
temperature = 0.7

[settings.streaming]
edit_throttle_ms = 300

[[bots]]
name = "assistant"
token = "${ASSISTANT_BOT_TOKEN}"
enabled = true
allowed_chats = []
interaction_mode = "streaming"
model = "gpt-4"
system_prompt = "你是一个有帮助的助手。"

[[bots]]
name = "dev-bot"
token = "${DEV_BOT_TOKEN}"
enabled = true
allowed_chats = [-1001234567890]
interaction_mode = "periodic_summary"
model = "claude-3-opus"
```

## 相关链接

- [配置系统](../concepts/configuration.md) — 配置概念和优先级
- [Quickstart](../quickstart.md) — 最小配置示例

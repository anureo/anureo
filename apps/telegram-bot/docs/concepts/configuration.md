# 配置系统

Bot 从 `~/.anureo/telegram-bot.toml` 加载配置，支持环境变量插值和优先级覆盖。

## 什么时候用

- 你需要配置 Bot Token、模型、下载目录等
- 你在不同环境（开发/生产）使用不同的配置值

## 核心概念

### 配置文件结构

```
~/.anureo/
├── config.toml              # anureo 主配置（LLM provider、API Key）
└── telegram-bot.toml        # Bot 专用配置
```

### 优先级

变量按以下顺序应用（后者覆盖前者）：

1. **anureo config.toml `[env]` 段** — 基础值
2. **项目 `.env` 文件** — 项目级覆盖
3. **已有环境变量** — 最高优先级

### 环境变量插值

配置文件中用 `${VAR_NAME}` 引用环境变量：

```toml
[[bots]]
name = "my-bot"
token = "${TELEGRAM_BOT_TOKEN}"   # 从环境变量读取
```

如果变量不存在，启动时会报 `ConfigError`。

### 配置段说明

- `[settings]` — 全局设置（下载目录、日志级别等）
- `[[bots]]` — Bot 实例数组，每个元素是一个独立 Bot
- `[settings.agent]` — Agent 配置（prompt、system message）

## 最佳实践

✅ Token 和 API Key 始终用 `${ENV_VAR}` 引用，不要明文写入配置
✅ 开发环境用 `.env` 文件，生产环境用环境变量
⚠️ 修改配置后需要重启 Bot，不支持热加载

## 本页覆盖范围

- 覆盖：配置文件结构、优先级、环境变量插值
- 不覆盖：每个配置字段的详细说明（见 [配置文件参考](../reference/config-reference.md)）

## 下一步

- [配置文件参考](../reference/config-reference.md) — 完整字段说明
- [Quickstart](../quickstart.md) — 快速配置示例

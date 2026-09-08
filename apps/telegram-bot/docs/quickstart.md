# Quickstart — 5 分钟跑通你的第一个 Telegram Bot

## 前置条件

- Rust 1.75+（`rustc --version`）
- Telegram Bot Token（从 [@BotFather](https://t.me/BotFather) 获取）
- anureo 配置目录（默认 `~/.anureo/`）

## 步骤

### Step 1: 创建配置目录

```bash
mkdir -p ~/.anureo
```

### Step 2: 写入 Bot 配置

创建 `~/.anureo/telegram-bot.toml`：

```toml
[settings]
download_dir = "downloads"
log_level = "info"

[[bots]]
name = "my-bot"
token = "YOUR_BOT_TOKEN"        # 或用环境变量: "${MY_BOT_TOKEN}"
allowed_chats = []              # 空 = 允许所有聊天
interaction_mode = "streaming"  # 推荐：流式响应
model = "gpt-4"                 # 默认模型
```

### Step 3: 配置 anureo 主配置

确保 `~/.anureo/config.toml` 中有 LLM provider 配置：

```toml
[default]
provider = "openai"

[env]
OPENAI_API_KEY = "sk-..."
```

### Step 4: 构建并运行

```bash
cargo build -p telegram-bot
cargo run -p telegram-bot
```

预期输出：

```
INFO  telegram_bot::bot > Starting bot "my-bot" ...
INFO  telegram_bot::bot > Bot "my-bot" is running (long polling)
```

### Step 5: 验证

在 Telegram 中找到你的 Bot，发送 `/help`。如果收到回复，说明运行成功。

## 下一步

- [多 Bot 架构](concepts/multi-bot-architecture.md) — 了解如何同时运行多个 Bot
- [配置系统](concepts/configuration.md) — 完整配置选项

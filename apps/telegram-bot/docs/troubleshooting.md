# 故障排查

常见问题和解决方案。

## 问题列表

| 症状 | 原因 | 解决方案 |
|------|------|---------|
| Bot 启动后无响应 | Token 无效或网络不通 | 检查 Token 和代理设置 |
| `ConfigError` 启动失败 | 配置文件缺失或格式错误 | 检查 `~/.anureo/telegram-bot.toml` |
| 消息不回复 | 群聊中未 @Bot | 检查 Mention 过滤逻辑 |
| "请稍等" 提示 | 前一轮 Agent 未完成 | 等待当前轮完成，或重启 |
| 流式消息卡住 | Telegram API 频率限制 | 调整 `edit_throttle_ms` |
| 模型切换失败 | 模型 ID 不存在 | 用 `/model list` 查看可用模型 |
| 文件下载失败 | 磁盘空间或权限不足 | 检查 `download_dir` 权限 |

## 详细排查

### Bot 启动后无响应

1. 检查日志中是否有 `Starting bot "xxx"` 输出
2. 确认 Token 有效：访问 `https://api.telegram.org/bot<TOKEN>/getMe`
3. 检查网络代理设置（如需要）

### ConfigError 启动失败

常见原因：
- `telegram-bot.toml` 文件不存在 → 复制示例文件并修改
- `${ENV_VAR}` 引用的环境变量未设置 → 设置对应环境变量
- TOML 格式错误 → 用 `toml validate` 工具检查

```bash
# 检查配置文件是否存在
ls ~/.anureo/telegram-bot.toml

# 验证环境变量
echo $TELEGRAM_BOT_TOKEN
```

### 群聊中 Bot 不响应

Bot 在群聊中需要被 @提及 或回复才会响应：
- 确认 Bot 的 username 配置正确
- 确认消息格式为 `@bot_username 你的消息`
- 检查 `allowed_chats` 是否包含目标群组的 chat_id

### 流式消息卡住或频繁报错

1. 检查 Telegram API 限流：`RUST_LOG=debug` 查看请求日志
2. 增大 `edit_throttle_ms`（如设为 500 或 1000）
3. 考虑切换为 `periodic_summary` 模式

```toml
[settings.streaming]
edit_throttle_ms = 500
```

### Docker 部署问题

1. 确认 `ANUREO_CONFIG_PATH` 指向正确的配置目录
2. 确认容器内可访问该路径（Docker volume 挂载正确）
3. 检查 `.env` 文件中的 Bot Token

```bash
# 检查容器日志
docker-compose logs assistant

# 检查配置挂载
docker-compose exec assistant ls /root/.anureo/
```

## 相关链接

- [配置文件参考](reference/config-reference.md) — 完整配置字段
- [Quickstart](quickstart.md) — 重新检查安装步骤

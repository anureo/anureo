# Contributing to telegram-bot

## Setup

```bash
# 1. 配置
cp telegram-bot.example.toml ~/.anureo/telegram-bot.toml
# 编辑 token、LLM 设置

# 2. 构建
cargo build -p telegram-bot

# 3. 测试 (需要先修复 anureo crate 编译错误)
cargo test -p telegram-bot

# 4. Lint
cargo clippy -p telegram-bot --all-targets -- -W clippy::all
```

## Project Structure

```
apps/telegram-bot/
├── src/
│   ├── main.rs              # 入口
│   ├── lib.rs               # 公共 API 导出
│   ├── bot.rs               # 多机器人管理
│   ├── router.rs            # teloxide 消息路由
│   ├── pipeline/mod.rs      # 消息处理流水线
│   ├── command/mod.rs       # 斜杠命令
│   ├── streaming/           # 流式响应
│   │   ├── agent.rs         # Agent 调用
│   │   ├── event_mapper.rs  # 事件适配
│   │   ├── message_handler.rs # 消息节流
│   │   └── retry.rs         # API 重试
│   ├── config/              # 配置加载
│   ├── formatting/          # Markdown/HTML 格式化
│   ├── traits.rs            # DI trait 定义
│   ├── handler_deps.rs      # 依赖组装
│   ├── mock.rs              # 测试 Mock
│   └── ...
├── tests/                   # 集成测试
├── ARCHITECTURE.md          # 架构文档
├── REVIEW.md                # Code review 记录
└── CHANGELOG.md             # 变更日志
```

## PR Checklist

提交 PR 前确认以下事项：

### 必须通过

- [ ] `cargo clippy -p telegram-bot --all-targets -- -W clippy::all` 无 warning
- [ ] `cargo test -p telegram-bot` 全部通过
- [ ] 无新增 `unwrap()` 在非测试代码中 (使用 `?`、`unwrap_or_default`、`ok_or` 替代)
- [ ] 无新增 `unsafe` 代码
- [ ] 公共函数/struct 有 rustdoc 注释
- [ ] 错误路径有对应测试

### 强烈建议

- [ ] 新增 trait 实现有 mock 和对应测试
- [ ] 新增配置项有对应 `telegram-bot.example.toml` 示例
- [ ] 新增依赖已在 `Cargo.toml` (workspace) 中统一管理
- [ ] 魔术数字提取为命名常量
- [ ] `CHANGELOG.md` 已更新

### 禁止

- ❌ 生产代码中使用 `unwrap()` — 使用 `?` 或 `unwrap_or_else`
- ❌ 硬编码 API token / secret
- ❌ 新增 `unsafe` 块 (除非有详细安全论证)
- ❌ 直接依赖 `reqwest`/`hyper` — 通过 `teloxide` 或 `anureo` 间接使用

## Code Style

- 缩进: 4 spaces (Rust 标准)
- 命名: `snake_case` 函数/变量, `PascalCase` 类型
- 错误处理: 使用 `BotError` 枚举 + `thiserror`, 避免 stringly-typed errors
- 异步: 所有 I/O 操作使用 `async fn`, 入口通过 `tokio::main`
- 注释: 用英文写 doc comments, 用 "why" 而非 "what"

## Adding a New Command

1. 在 `command/mod.rs` 中创建实现 `BotCommand` 的 struct
2. 在 `CommandDispatcher::new()` 中注册
3. 在 `src/tests/` 或 `tests/` 添加测试
4. 更新 `CHANGELOG.md`

## Adding a New Streaming Phase

1. 在 `streaming/message_handler.rs` 的 `StreamCommand` 枚举中添加变体
2. 在 `streaming/event_mapper.rs` 中处理新的 `AnyStreamEvent`
3. 在 `streaming/message_handler.rs` 中实现 UI 更新逻辑
4. 在 `tests/streaming_message_handler_test.rs` 添加回归测试

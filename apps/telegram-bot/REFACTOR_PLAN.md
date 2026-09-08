# telegram-bot 架构重构方案

> 基于 2025-08-19 代码审查，按优先级排列。

---

## P0 — 立即修复（死代码 / 重复 / 错误处理）

### 0.1 清理 `constants.rs` 中的 dead_code

**问题**：5 个常量标注 `#[allow(dead_code)]`，要么是过度设计，要么是重构遗留。

**文件**：`src/constants.rs`

**方案**：
- 检索全部 `dead_code` 常量的引用点
- 有引用的：去掉 `#[allow(dead_code)]`，确保使用方改为引用常量
- 无引用的：直接删除
- 具体地：
  - `SMALL_MESSAGE_THRESHOLD` → 如果 `message_handler.rs` 中有硬编码 200 的地方，改为引用此常量；否则删除
  - `LARGE_MESSAGE_THRESHOLD` → 同上，检查 3000 的硬编码
  - `MAX_MESSAGE_LEN` → 检查 4096 硬编码，统一为常量引用
  - `THINK_HEADER` → 检查 "💭 Thinking..." 字符串字面量
  - `MAX_RETRIES` → 已被 `sender.rs` 通过 `use crate::constants::retry::MAX_RETRIES` 引用，去掉 dead_code 即可
  - `MAX_FILE_ID_LEN` / `MAX_EXT_LEN` → 见 0.2

### 0.2 消除 `download.rs` 与 `constants.rs` 的常量重复

**问题**：`download.rs:16-17` 定义了 `MAX_FILE_ID_LEN = 24` 和 `MAX_EXT_LEN = 10`，与 `constants.rs:44-50` 完全重复。`constants.rs` 中标记 `dead_code` 正是因为 `download.rs` 没引用它。

**文件**：`src/download.rs`, `src/constants.rs`

**方案**：
1. 删除 `download.rs` 中的 `const MAX_FILE_ID_LEN` 和 `const MAX_EXT_LEN`
2. 在 `download.rs` 顶部添加：
   ```rust
   use crate::constants::download::{MAX_FILE_ID_LEN, MAX_EXT_LEN};
   ```
3. 去掉 `constants.rs` 中对应常量的 `#[allow(dead_code)]`

### 0.3 修复 `SqliteSessionManager::exists()` 的错误吞没

**问题**：`session.rs:33` 使用 `.unwrap_or(0)` 吞掉了 rusqlite 查询错误。如果数据库损坏或 schema 不匹配，会静默返回 `false`（认为 session 不存在），导致用户无法 reset。

**文件**：`src/session.rs`

**方案**：
```rust
async fn exists(&self, thread_id: &str) -> Result<bool, BotError> {
    let db_path = anureo::memory::default_memory_db_path();
    let conn = rusqlite::Connection::open(&db_path)
        .map_err(|e| BotError::Database(e.to_string()))?;

    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM checkpoints WHERE thread_id = ?1",
            [thread_id],
            |row| row.get(0),
        )
        .map_err(|e| BotError::Database(e.to_string()))?;

    Ok(count > 0)
}
```

变更点：`unwrap_or(0)` → `.map_err(|e| BotError::Database(e.to_string()))`。

### 0.4 统一 `main.rs` 启动错误处理

**问题**：anureo 全局配置加载用 `if let Ok` 静默忽略失败，telegram-bot 配置用 `process::exit(1)`。两条路径风格不一致。

**文件**：`src/main.rs`

**方案**：
```rust
fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 全局配置：仅打印 warning，不中断启动
    if let Err(e) = config::load_and_apply_with_report("anureo", None::<&std::path::Path>) {
        tracing::warn!("Failed to load anureo global config (non-fatal): {}", e);
    } else if let Ok(report) = config::load_and_apply_with_report("anureo", None::<&std::path::Path>) {
        // ... 原有的 eprintln 信息 ...
    }
    // ... 其余不变 ...
}
```

同时将 `eprintln!` 统一为 `tracing::info!`（`_guard` 已在 `load_config` 之后初始化，所以这里 eprintln 是合理的，但可以加注释说明顺序）。

---

## P1 — 结构优化（streaming 状态机 / 职责拆分）

### 1.1 为 `MessageState` 引入阶段状态机

**问题**：`streaming/message_handler.rs` 中 `MessageState` 用扁平字段 + 隐式阶段判断（think_text 是否为空 → 是否在 think 阶段）。当阶段增多时，状态组合爆炸，容易出 bug。

**文件**：`src/streaming/message_handler.rs`

**方案**：

定义阶段枚举：

```rust
#[derive(Debug, Clone, PartialEq)]
enum Phase {
    Idle,
    Thinking { count: u32 },
    Acting { count: u32 },
    ToolExecuting { name: String },
    Completed,
}

#[derive(Debug, Clone)]
struct ToolBlock {
    name: String,
    arguments: Option<String>,
    result: Option<String>,
    is_error: bool,
}

struct MessageState {
    phase: Phase,
    think_text: String,
    act_text: String,
    tool_blocks: Vec<ToolBlock>,
    think_count: u32,
    act_count: u32,
}
```

好处：
- `format_current_display()` 可以用 `match &self.phase` 替代 if-else 链
- 编译器强制覆盖所有阶段
- 新增阶段（如 `Streaming`、`WaitingForInput`）只需扩展 enum

**迁移步骤**：
1. 添加 `Phase` enum，初始值 `Idle`
2. 在 `process_command()` 的 `StartThink`/`StartAct` 分支中设置 `self.phase`
3. `format_current_display()` 改为 `match &self.phase`
4. 添加单元测试验证阶段转换：`Idle → Thinking → Acting → Completed`

### 1.2 从 `event_mapper.rs` 提取背压策略

**问题**：`send_stream_command()` 同时负责"决定是否发送"（背压策略）和"发送命令"。两个关注点混在一起。

**文件**：`src/streaming/event_mapper.rs`

**方案**：

提取背压策略为独立结构：

```rust
// 新文件：src/streaming/backpressure.rs
pub(crate) struct ChannelBackpressure {
    tx: mpsc::Sender<StreamCommand>,
    dropped_best_effort: AtomicU64,
}

impl ChannelBackpressure {
    pub fn new(tx: mpsc::Sender<StreamCommand>) -> Self { ... }

    pub fn send(&self, cmd: StreamCommand, priority: CommandPriority) {
        match priority {
            CommandPriority::Critical => {
                // block or force-send
            }
            CommandPriority::BestEffort => {
                // try_send, log on drop
            }
        }
    }
}
```

`event_mapper` 只负责 `AnyStreamEvent → StreamCommand` 的映射，调用 `backpressure.send()`。

### 1.3 拆分 `config/telegram.rs`

**问题**：~400 行，包含所有配置 struct、错误类型、env 插值解析器、文件查找逻辑。

**文件**：`src/config/telegram.rs`

**方案**：

```
src/config/
├── mod.rs           # pub use 重导出
├── loader.rs        # 文件查找 + env 插值（已有）
├── types.rs         # 所有配置 struct：Settings, BotConfig, AgentConfig, etc.
├── error.rs         # ConfigError enum
└── telegram.rs      # 保留为兼容性 re-export（deprecated），或直接删
```

types.rs 内容：
```rust
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum InteractionMode { ... }

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramBotConfig { ... }

#[derive(Debug, Clone, Deserialize)]
pub struct BotConfig { ... }

// ... 其他 struct ...
```

error.rs 内容：
```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError { ... }
```

### 1.4 从 `pipeline/mod.rs` 提取 agent 编排逻辑

**问题**：`run_agent_for_chat()` 混合了并发守卫、download、prompt 构建、agent 调用。pipeline 层应只做消息路由。

**文件**：`src/pipeline/mod.rs`

**方案**：

新建 `src/pipeline/agent_orchestrator.rs`：

```rust
pub async fn run_agent_for_chat(ctx: &MessageContext<'_>, prompt: &str) -> Result<(), BotError> {
    // 并发守卫
    let guard = acquire_run_guard(&ctx)?;
    
    // agent 调用
    let result = ctx.deps.agent_runner.run(prompt, chat_id, context).await?;
    
    // 后处理
    Ok(())
}
```

`pipeline/mod.rs` 的 `handle_text_message()` 变为：
```rust
async fn handle_text_message(ctx: &MessageContext<'_>, text: &str) -> Result<(), BotError> {
    let prompt = build_prompt(ctx, text);
    crate::pipeline::agent_orchestrator::run_agent_for_chat(ctx, &prompt).await
}
```

---

## P2 — 设计改进（trait 审视 / metrics 接入 / 所有权统一）

### 2.1 重新审视 trait 抽象层

**问题**：5 个 trait 各只有 1 个生产实现 + 1 个 mock 实现。`async_trait` 的 `Box<dyn Future>` 有运行时开销。

**涉及的 trait**：`AgentRunner`, `MessageSender`, `SessionManager`, `FileDownloader`

**方案 A（推荐）：Feature-gated mock**

```rust
// src/sender.rs
pub struct TeloxideSender { bot: Bot }

#[cfg(test)]
pub struct MockSender { messages: Arc<RwLock<Vec<(i64, String)>>> }
```

`HandlerDeps` 改为泛型或使用具体类型：

```rust
pub struct HandlerDeps {
    pub sender: TeloxideSender,
    pub agent_runner: anureoAgentRunner,
    pub session_manager: SqliteSessionManager,
    pub file_downloader: TeloxideDownloader,
    // ...
}

#[cfg(test)]
impl HandlerDeps {
    pub fn mock() -> MockHandlerDeps { ... }
}
```

好处：消除 `Box<dyn Future>` 开销、编译时单态化、代码更直观。

**方案 B（保守）：保持 trait 但清理**

如果预期未来会有多种实现（如 webhook sender、in-memory session），保留 trait 但：
- `AgentRunContext` 改为具体 struct（它不是 trait，只是数据，已经如此）
- `MessageSender` 保留（Telegram API vs 测试确实不同）
- `SessionManager` 和 `FileDownloader` 考虑合并或去掉

**决策标准**：如果 3 个月内不会出现第二种实现，选方案 A。

### 2.2 接入 Metrics 管线

**问题**：`BotMetrics` 定义了 7 个 counter，但没有接入运行时。health endpoint 只返回 `{"status": "ok"}`，不暴露 metrics。

**文件**：`src/metrics.rs`, `src/health.rs`

**方案**：

1. **在 `HandlerDeps` 中持有 `Arc<BotMetrics>`**：

```rust
pub struct HandlerDeps {
    // ... 现有字段 ...
    pub metrics: Arc<BotMetrics>,
}
```

2. **在关键路径埋点**：
   - `router.rs` 的 `handle_message_with_deps` 入口：`metrics.increment_messages()`
   - `pipeline` agent 调用前后：`metrics.increment_agent_calls()` / `increment_agent_failures()`
   - `sender.rs` 的 send/edit 后：`metrics.increment_messages_sent()` / `increment_messages_edited()`
   - `download.rs` 下载完成后：`metrics.increment_downloads()`

3. **在 health endpoint 暴露 metrics**：

```rust
// health.rs
async fn health_handler(State(state): State<HealthState>) -> Json<serde_json::Value> {
    let metrics = state.metrics.snapshot();
    Json(serde_json::json!({
        "status": "ok",
        "metrics": metrics,
    }))
}
```

4. **删除 `create_metrics_middleware`**（当前无用，改用直接调用）。

### 2.3 统一 `Settings` 所有权

**问题**：`bot.rs` 用 `Arc<Settings>`，`anureoAgentRunner` 持有 owned `Settings`。两个地方对同一配置的所有权模型不一致，可能导致配置更新不同步。

**文件**：`src/agent.rs`, `src/bot.rs`

**方案**：

```rust
pub struct anureoAgentRunner {
    bot: Bot,
    settings: Arc<Settings>,
}

impl anureoAgentRunner {
    pub fn new(bot: Bot, settings: Arc<Settings>) -> Self {
        Self { bot, settings }
    }
}
```

同时 `run_anureo_agent_streaming` 改为接收 `&Settings`（当前已经如此），内部传引用。

### 2.4 `SqliteSessionManager` 连接管理

**问题**：每次 `exists()` 调用都打开新连接，无连接池。

**方案**：

短期（P2）：在 `SqliteSessionManager` 内持有 `Mutex<Connection>`：

```rust
pub struct SqliteSessionManager {
    conn: Mutex<rusqlite::Connection>,
}

impl SqliteSessionManager {
    pub fn new() -> Result<Self, BotError> {
        let db_path = anureo::memory::default_memory_db_path();
        let conn = rusqlite::Connection::open(&db_path)
            .map_err(|e| BotError::Database(e.to_string()))?;
        Ok(Self { conn: Mutex::new(conn) })
    }
}
```

长期：如果并发量增加，考虑 `r2d2` 连接池或迁移到 `sqlx`（async-native）。

---

## P3 — 长期改进（可选）

### 3.1 `formatting` 模块与 `message_handler` 的边界

当前 `formatting/telegram.rs` 提供 `FormattedMessage`，但 `message_handler.rs` 的 `format_current_display()` 内部也在做格式化（拼接 Think/Act/Tool 文本）。建议：
- `FormattedMessage` 只定义数据结构
- 新建 `formatting/streaming.rs`，包含 `fn format_streaming_phase(state: &MessageState) -> FormattedMessage`
- `message_handler` 只调用格式化函数，不自己拼字符串

### 3.2 错误类型细化

`BotError::Agent(String)` 和 `BotError::Database(String)` 使用字符串承载错误信息，丢失了原始错误类型。建议：

```rust
pub enum BotError {
    Agent(#[from] AgentError),
    Database(#[from] DatabaseError),
    Telegram(#[from] teloxide::ApiError),
    // ...
}
```

### 3.3 测试覆盖补全

当前 `streaming/message_handler.rs` 约 500 行，无独立单元测试（仅在 integration test 中间接覆盖）。建议：
- 为 `MessageState` 的阶段转换添加单元测试
- 为 `format_current_display()` 的各阶段输出添加快照测试
- 为 `StreamEventMapper` 添加 mock event → StreamCommand 的映射测试

---

## 实施计划

| 阶段 | 内容 | 预估工作量 | 风险 |
|------|------|-----------|------|
| P0 | 死代码清理 + 常量去重 + 错误处理 | 1-2 小时 | 低 |
| P1.1 | MessageState 状态机 | 3-4 小时 | 中（需充分测试） |
| P1.2 | 背压策略提取 | 1-2 小时 | 低 |
| P1.3 | config 拆分 | 1 小时 | 低（纯重构） |
| P1.4 | pipeline 拆分 | 2 小时 | 低 |
| P2.1 | trait 审视 | 4-6 小时 | 中（影响面广） |
| P2.2 | metrics 接入 | 2-3 小时 | 低 |
| P2.3 | Settings 所有权 | 1 小时 | 低 |
| P2.4 | 连接管理 | 2 小时 | 低 |

**建议顺序**：P0 全部 → P1.3 → P1.4 → P1.1 → P1.2 → P2.3 → P2.2 → P2.4 → P2.1

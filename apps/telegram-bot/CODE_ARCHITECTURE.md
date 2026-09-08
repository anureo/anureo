# telegram-bot 代码架构文档

## 1. 项目概览

telegram-bot 是一个基于 [teloxide](https://github.com/teloxide/teloxide) 的 Telegram 多机器人管理框架，集成 anureo Agent 提供 AI 对话能力。核心特性：

- **多 bot 长轮询** — 单进程运行多个 Telegram Bot，各自独立轮询
- **流式 Agent 响应** — 实时展示 Think / Act / Tool 阶段，边生成边更新消息
- **斜杠命令系统** — `/model`、`/reset`、`/status` 等可扩展命令
- **媒体下载** — 图片、视频、文档的安全下载与元数据管理
- **模型切换** — SQLite 持久化的模型目录与模糊搜索
- **健康检查** — axum HTTP 端点，支持 readiness / liveness 探针

### 技术栈

| 依赖 | 用途 |
|------|------|
| teloxide 0.13 | Telegram Bot API |
| anureo (workspace) | AI Agent 运行时 |
| rusqlite 0.31 | SQLite（模型选择、session） |
| axum 0.7 | 健康检查 HTTP 服务 |
| tokio | 异步运行时 |
| serde + toml | 配置序列化 |
| tracing | 结构化日志 |

---

## 2. 目录结构

```
apps/telegram-bot/
├── src/
│   ├── main.rs              # 入口：配置加载 → 日志 → 启动 BotManager
│   ├── lib.rs               # 库入口：模块声明与 re-export
│   ├── bot.rs               # BotManager：多 bot 生命周期管理
│   ├── router.rs            # 消息路由：teloxide handler → pipeline
│   ├── pipeline/
│   │   └── mod.rs           # 消息处理管线：命令分发 → mention gate → agent
│   ├── command/
│   │   └── mod.rs           # 斜杠命令（Command 模式）
│   ├── handler_deps.rs      # HandlerDeps 依赖注入容器
│   ├── traits.rs            # 核心 trait 定义
│   ├── agent.rs             # anureoAgentRunner 实现
│   ├── sender.rs            # TeloxideSender：Telegram API 封装
│   ├── session.rs           # SqliteSessionManager
│   ├── download.rs          # 媒体文件下载
│   ├── model_selection.rs   # 模型目录与切换
│   ├── streaming/
│   │   ├── mod.rs
│   │   ├── agent.rs         # Agent 流式执行入口
│   │   ├── event_mapper.rs  # anureo 事件 → StreamCommand 适配器
│   │   ├── message_handler.rs # 流式消息状态机与 Telegram 更新
│   │   └── retry.rs         # Telegram API 重试（指数退避 + jitter）
│   ├── formatting/
│   │   ├── mod.rs
│   │   └── telegram.rs      # FormattedMessage + MarkdownV2/HTML 转义
│   ├── config/
│   │   ├── mod.rs
│   │   ├── loader.rs        # 配置文件查找与 env 变量插值
│   │   └── telegram.rs      # 所有配置类型定义
│   ├── constants.rs         # 命名常量
│   ├── error.rs             # BotError 错误枚举
│   ├── health.rs            # 健康检查 HTTP 端点
│   ├── metrics.rs           # 原子计数器（messages, agent calls, downloads...）
│   ├── mock.rs              # 测试 mock 实现
│   ├── utils.rs             # 工具函数（truncate_text 等）
│   └── logging.rs           # 日志初始化（main.rs 内部模块）
├── tests/                   # 集成测试
│   ├── common/              # 测试 fixtures
│   ├── bot_startup_test.rs
│   ├── concurrency_test.rs
│   ├── handler_dispatch_mock_test.rs
│   ├── integration_test.rs
│   ├── message_flow_test.rs
│   └── streaming_message_handler_test.rs
├── Cargo.toml
├── ARCHITECTURE.md
└── telegram-bot.example.toml
```

---

## 3. 启动流程

```
main()
 │
 ├─ 1. config::load_and_apply_with_report("anureo", ...)
 │     加载 ~/.anureo/config.toml + .env，设置环境变量
 │     （OPENAI_API_KEY, MODEL, LLM_PROVIDER 等）
 │
 ├─ 2. load_config()
 │     加载 ~/.anureo/telegram-bot.toml → TelegramBotConfig
 │
 ├─ 3. logging::setup_logging()
 │     初始化 tracing（stdout + 可选文件输出）
 │
 └─ 4. run_with_config(config)
       └─ BotManager::new(config)
            ├─ start_health_server()     ← axum /health, /ready
            └─ 对每个 enabled bot:
                 └─ spawn run_bot(bot_config)
                      └─ teloxide Dispatcher + long polling
```

### 源文件引用

- 入口：`src/main.rs`
- BotManager：`src/bot.rs`
- 配置加载：`src/config/loader.rs`
- 日志初始化：`src/logging.rs`（`main.rs` 内部 `mod logging`）

---

## 4. 核心数据流

### 4.1 消息处理主流程

```
Telegram Server
     │
     ▼ (long polling)
teloxide Dispatcher
     │
     ▼
router::default_handler(bot, msg, settings, bot_username, run_registry)
     │
     ├─ HandlerDeps::production()  ← 构建依赖容器
     │
     ▼
router::handle_message_with_deps(deps, msg)
     │
     ├─ MessageKind::Common → pipeline::handle_common_message()
     │
     ▼
pipeline::MessageContext
     │
     ├─ ensure_download_dir()
     │
     ├─ extract_text() → handle_text_message()
     │   ├─ strip_bot_mention()
     │   ├─ CommandDispatcher::dispatch()  ← 命令匹配
     │   │   ├─ /reset  → SessionManager::reset()
     │   │   ├─ /status → 回复当前状态
     │   │   ├─ /model → ModelSelectionService
     │   │   └─ 无匹配 → 继续
     │   ├─ mention gate (群聊中需要 @mention 或 reply)
     │   ├─ build_prompt_with_reply()
     │   └─ run_agent_for_chat()
     │       ├─ ChatRunRegistry 并发守卫（每 chat 一次一个 agent 调用）
     │       └─ AgentRunner::run(prompt, chat_id, context)
     │
     └─ extract_media() → handle_media_message()
         └─ FileDownloader::download()
```

### 4.2 流式 Agent 响应

```
agent.rs: run_anureo_agent_streaming()
 │
 ├─ 1. 创建 mpsc::channel::<StreamCommand>(100)
 │
 ├─ 2. spawn stream_message_handler_with_context(rx, sender, ...)
 │     └─ 消费 StreamCommand → 更新 Telegram 消息
 │
 ├─ 3. StreamEventMapper::new(tx, show_think, show_act)
 │     └─ 将 mapper callback 注册到 anureo
 │
 ├─ 4. anureo::run_agent_with_options(&opts, RunCmd::React, on_event)
 │     │
 │     │  anureo 内部产生 AnyStreamEvent:
 │     │    ├─ ThinkStart / ThinkDelta / ThinkEnd
 │     │    ├─ ActStart / ActDelta / ActEnd
 │     │    ├─ ToolCallStart / ToolCallEnd
 │     │    └─ ...
 │     │
 │     ▼
 │     event_mapper: AnyStreamEvent → StreamCommand
 │       ├─ ThinkStart     → StartThink { count }
 │       ├─ ThinkDelta     → ThinkContent { content }
 │       ├─ ActStart       → StartAct { count }
 │       ├─ ActDelta       → ActContent { content }
 │       ├─ ToolCallStart  → ToolStart { name, arguments }
 │       ├─ ToolCallEnd    → ToolEnd { name, result, is_error }
 │       │
 │       └─ send_stream_command(tx, cmd, priority)
 │           ├─ Critical  → block on send
 │           └─ BestEffort → try_send, drop on full
 │
 └─ 5. tx.send(Flush).await
        handler_task.await → final_text
```

---

## 5. 模块详解

### 5.1 `config/` — 配置系统

**职责**：从 `~/.anureo/telegram-bot.toml` 加载配置，支持环境变量插值。

```
config/
├── mod.rs       → re-export
├── loader.rs    → load_config(), load_from_path()
└── telegram.rs  → 所有类型定义 + ConfigError
```

**核心类型**：

```rust
TelegramBotConfig          // 根配置
├── settings: Settings     // 全局设置（download_dir, log_level, log_file）
│   └── streaming: StreamingConfig  // show_think_phase, show_act_phase, edit_interval_ms
├── bots: HashMap<String, BotConfig>  // 多 bot 定义
│   └── BotConfig
│       ├── token: String
│       ├── enabled: bool
│       ├── allowed_chats: Option<Vec<i64>>
│       └── agent: AgentConfig  // system_prompt, model_override, interaction_mode
```

**环境变量插值**：配置中的 `"${TELOXIDE_TOKEN}"` 会被替换为实际环境变量值。

**配置查找顺序**：
1. `$ANUREO_HOME/telegram-bot.toml`
2. `~/.anureo/telegram-bot.toml`
3. `./telegram-bot.toml`（当前目录）

### 5.2 `traits.rs` — 核心抽象

定义 5 个 trait 实现依赖反转：

| Trait | 生产实现 | 用途 |
|-------|---------|------|
| `AgentRunner` | `anureoAgentRunner` | 执行 Agent 对话 |
| `MessageSender` | `TeloxideSender` | 发送/编辑/删除 Telegram 消息 |
| `SessionManager` | `SqliteSessionManager` | 会话重置与检查 |
| `FileDownloader` | `TeloxideDownloader` | 下载 Telegram 媒体文件 |
| `AgentRunContext` | (struct) | Agent 调用上下文数据 |

```rust
#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn run(&self, prompt: &str, chat_id: i64, context: AgentRunContext) -> Result<String, BotError>;
}

#[async_trait]
pub trait MessageSender: Send + Sync {
    async fn send_text_returning_id(&self, chat_id: i64, text: &str) -> Result<i32, BotError>;
    async fn edit_text(&self, chat_id: i64, message_id: i32, text: &str, parse_mode: Option<ParseMode>) -> Result<(), BotError>;
    async fn send_formatted(&self, chat_id: i64, msg: &FormattedMessage) -> Result<i32, BotError>;
    async fn edit_formatted(&self, chat_id: i64, message_id: i32, msg: &FormattedMessage) -> Result<(), BotError>;
    async fn delete_message(&self, chat_id: i64, message_id: i32) -> Result<(), BotError>;
    async fn set_reaction(&self, chat_id: i64, message_id: i32, emoji: &str) -> Result<(), BotError>;
}

#[async_trait]
pub trait SessionManager: Send + Sync {
    async fn reset(&self, thread_id: &str) -> Result<usize, BotError>;
    async fn exists(&self, thread_id: &str) -> Result<bool, BotError>;
}

#[async_trait]
pub trait FileDownloader: Send + Sync {
    async fn download_photo(&self, photo: &PhotoSize, base_dir: &Path) -> Result<FileMetadata, BotError>;
    async fn download_video(&self, video: &Video, base_dir: &Path) -> Result<FileMetadata, BotError>;
    async fn download_document(&self, doc: &Document, base_dir: &Path) -> Result<FileMetadata, BotError>;
}
```

### 5.3 `handler_deps.rs` — 依赖容器

`HandlerDeps` 将所有运行时依赖聚合为一个结构体，是生产/测试切换的关键：

```rust
pub struct HandlerDeps {
    pub bot: Bot,
    pub settings: Arc<Settings>,
    pub bot_username: Arc<String>,
    pub run_registry: Arc<ChatRunRegistry>,
    pub agent_runner: Box<dyn AgentRunner>,
    pub sender: Arc<dyn MessageSender>,
    pub session_manager: Arc<dyn SessionManager>,
    pub file_downloader: Arc<dyn FileDownloader>,
    pub model_service: Arc<ModelSelectionService>,
}
```

- `HandlerDeps::production()` — 构建所有生产实现
- `HandlerDeps::mock()` — 构建测试 mock（来自 `mock.rs`）

### 5.4 `router.rs` — 消息路由

极薄的路由层（46 行），只做一件事：从 teloxide handler 提取 `Message`，委托给 `handle_message_with_deps`。

```rust
pub async fn default_handler(bot, msg, settings, bot_username, run_registry) {
    let deps = HandlerDeps::production(bot, settings, bot_username, run_registry);
    handle_message_with_deps(&deps, &msg).await
}
```

### 5.5 `pipeline/` — 消息处理管线

**核心编排层**，处理一条消息的完整生命周期：

1. `ensure_download_dir()` — 确保下载目录存在
2. `handle_common_message()` — 分发到文本/媒体处理
3. `handle_text_message()` — 命令匹配 → mention gate → agent 调用
4. `handle_media_message()` — 媒体文件下载

**关键函数**：

```rust
fn strip_bot_mention(text: &str, bot_username: &str) -> String
// 去掉 "@botname " 前缀

fn build_prompt_with_reply(msg: &Message, clean_text: &str) -> String
// 如果是回复消息，将被回复内容拼接到 prompt

async fn run_agent_for_chat(ctx: &MessageContext<'_>, prompt: &str) -> Result<(), BotError>
// 并发守卫 + AgentRunner::run() + 结果后处理
```

**并发控制**：`ChatRunRegistry`（`HashMap<i64, Arc<Mutex<()>>>`）确保同一 chat 同时只有一个 agent 调用。

### 5.6 `command/` — 斜杠命令系统

使用 **Command 模式**，每个命令是独立的类型：

```rust
#[async_trait]
pub trait BotCommand: Send + Sync {
    fn matches(&self, text: &str) -> bool;
    async fn execute(&self, ctx: &CommandContext<'_>) -> Result<(), BotError>;
}
```

**已注册命令**：

| 命令 | 实现 | 功能 |
|------|------|------|
| `/reset` | `ResetCommand` | 重置会话历史 |
| `/status` | `StatusCommand` | 显示当前配置状态 |
| `/model` | `ModelCommand` | 模型搜索、选择、切换 |

`CommandDispatcher` 按注册顺序逐个匹配，first-match-wins。

### 5.7 `streaming/` — 流式响应系统

这是系统最复杂的子系统，负责将 anureo Agent 的实时事件转化为 Telegram 消息更新。

#### 5.7.1 `agent.rs` — 流式执行入口

```rust
pub async fn run_anureo_agent_streaming(
    message: &str,
    chat_id: i64,
    sender: Arc<dyn MessageSender>,
    context: AgentRunContext,
    settings: &Settings,
) -> Result<String>
```

流程：
1. 创建 `mpsc::channel::<StreamCommand>(100)`
2. `tokio::spawn` 消息处理任务（消费端）
3. 构建 `StreamEventMapper`，注册为 anureo callback
4. 调用 `anureo::run_agent_with_options()`
5. 发送 `Flush` 命令，等待处理任务返回最终文本

#### 5.7.2 `event_mapper.rs` — 事件适配器（Adapter 模式）

将 anureo 的 `AnyStreamEvent` 映射为内部 `StreamCommand`：

| anureo Event | StreamCommand | Priority |
|------------|---------------|----------|
| `ThinkStart` | `StartThink { count }` | Critical |
| `ThinkDelta` | `ThinkContent { content }` | BestEffort |
| `ActStart` | `StartAct { count }` | Critical |
| `ActDelta` | `ActContent { content }` | BestEffort |
| `ToolCallStart` | `ToolStart { name, arguments }` | Critical |
| `ToolCallEnd` | `ToolEnd { name, result, is_error }` | Critical |

**背压策略**：
- `Critical`：阻塞等待 channel 有空间
- `BestEffort`：`try_send()`，channel 满时丢弃并计数

#### 5.7.3 `message_handler.rs` — 流式消息状态机

`MessageState` 维护当前消息的完整渲染状态：

```
MessageState
├── message_id: Option<i32>     ← Telegram 消息 ID（首次发送后设置）
├── think_text: String          ← Think 阶段累积文本
├── act_text: String            ← Act 阶段累积文本
├── tool_blocks: Vec<(name, arguments, result, is_error)>
├── think_count / act_count: u32
├── last_edit: Instant          ← 节流用时间戳
└── current_tool_name: Option<String>
```

**处理循环** `stream_message_handler_with_context()`：

```
loop {
    select! {
        cmd = rx.recv() → process_command(state, cmd)
        _ = edit_throttle.tick() → flush_pending_edit(state)
    }
}
```

- 接收 `StreamCommand` → 更新 `MessageState`
- 按节流间隔（默认 300ms）批量刷新 Telegram 消息
- `Flush` 命令触发最终渲染并返回

**渲染格式**：

```
💭 Thinking... (n)
<think_text>

⚡ Acting... (n)
<act_text>

🔧 Tool: <name>
<Result or "Running...">
```

#### 5.7.4 `retry.rs` — API 重试

指数退避 + 随机 jitter 的重试策略：

```rust
fn classify_error(error: &RequestError) -> RetryKind {
    // RetryAfter → RateLimited（尊重 Retry-After 头）
    // Network    → Transient（可重试）
    // 其他       → Fatal（不重试）
}
```

每次重试延迟：`min(BASE_DELAY * 2^attempt, MAX_DELAY) * (1 ± JITTER)`

### 5.8 `formatting/` — 消息格式化

```rust
struct FormattedMessage {
    text: String,                    // 最终渲染文本
    parse_mode: Option<ParseMode>,   // MarkdownV2 / Html / None
    plain_text_fallback: String,     // 格式化失败时的降级文本
}
```

工具函数：
- `escape_markdown_v2()` — 转义 Telegram MarkdownV2 保留字符
- `escape_html()` — HTML 实体转义
- `markdown_notice()` — 构造 MarkdownV2 格式的通知消息

### 5.9 `sender.rs` — Telegram API 封装

`TeloxideSender` 实现 `MessageSender` trait，封装所有与 Telegram 的交互：

- `send_text_returning_id` → `bot.send_message()`
- `send_formatted` → `bot.send_message()` + ParseMode
- `edit_text` / `edit_formatted` → `bot.edit_message_text()`
- `delete_message` → `bot.delete_message()`
- `set_reaction` → `bot.set_message_reaction()`

所有写操作通过 `retry.rs` 的 `*_with_retry` 函数执行。

### 5.10 `download.rs` — 媒体下载

`TeloxideDownloader` 实现 `FileDownloader` trait，支持三种媒体类型：

| 类型 | 文件命名 | 安全措施 |
|------|---------|---------|
| Photo | `{file_id}.{ext}` | path traversal 防护 |
| Video | `{file_id}.{ext}` | 文件名长度限制 |
| Document | `{original_name}` | `sanitize_filename()` |

`ensure_within_base()` 防止路径穿越攻击。

### 5.11 `model_selection.rs` — 模型选择系统

三层架构：

```
ModelSelectionService              ← 业务层
├── store: Box<dyn ModelSelectionStore>    ← 持久化层
└── catalog: Box<dyn ModelCatalog>         ← 模型目录

trait ModelSelectionStore:
  get_selected_model(chat_id) → Option<String>
  save_selected_model(chat_id, model)
  clear_selected_model(chat_id)

trait ModelCatalog:
  search(query, page) → ModelSearchResult
```

- `SqliteModelSelectionStore` — 持久化到 SQLite（`model_selection` 表）
- `StaticModelCatalog` — 内置模型列表 + 模糊搜索
- `InMemorySearchSessionStore` — 跟踪用户搜索状态（翻页）

### 5.12 `session.rs` — 会话管理

`SqliteSessionManager` 实现会话重置（清除 anureo checkpoints）：

```rust
async fn reset(&self, thread_id: &str) → 调用 download::reset_session()
async fn exists(&self, thread_id: &str) → 查询 anureo 的 SQLite checkpoints 表
```

`thread_id` 格式：`telegram_{chat_id}`

### 5.13 `bot.rs` — BotManager

管理多个 bot 的生命周期：

```rust
pub struct BotManager {
    config: TelegramBotConfig,
}

impl BotManager {
    pub fn new(config) → Self
    pub async fn run(self) → Result<()>
        // 1. 启动健康检查服务器
        // 2. 对每个 enabled bot:
        //    a. 创建 teloxide Bot
        //    b. 构建 Dispatcher
        //    c. spawn run_bot() with CancellationToken
        // 3. 等待所有 bot 完成
}
```

每个 bot 有独立的 `CancellationToken`，支持优雅停机。

### 5.14 `health.rs` — 健康检查

axum HTTP 服务，两个端点：

| 端点 | 功能 | 返回 |
|------|------|------|
| `GET /health` | 存活探针 | `{"status": "ok", "timestamp": "..."}` |
| `GET /ready` | 就绪探针 | `{"ready": true, "uptime_secs": 123}` |

`HealthState` 使用 `AtomicBool` 跟踪健康/就绪状态。

### 5.15 `metrics.rs` — 指标收集

`BotMetrics` 使用 7 个 `AtomicU64` 计数器：

```
messages_total / messages_failed
files_downloaded
agent_calls / agent_failures
messages_sent / messages_edited
```

通过 `MetricsSnapshot` 序列化为 JSON。

### 5.16 `error.rs` — 错误体系

```rust
pub enum BotError {
    Config(String),
    Telegram(String),
    Agent(String),
    Database(String),
    Io(String),
    Download(String),
    RateLimited { retry_after: u64 },
    Cancelled,
    Unknown(String),
}

pub type Result<T> = std::result::Result<T, BotError>;
```

### 5.17 `constants.rs` — 命名常量

| 模块 | 常量 | 值 | 含义 |
|------|------|-----|------|
| streaming | `EDIT_THROTTLE_BASE_MS` | 300 | 消息编辑最小间隔 |
| streaming | `MAX_MESSAGE_LEN` | 4096 | Telegram 单消息字符上限 |
| retry | `MAX_RETRIES` | 3 | API 重试次数 |
| model | `SEARCH_PAGE_SIZE` | 8 | 模型搜索每页结果数 |
| download | `MAX_FILE_ID_LEN` | 24 | file_id 截断长度 |
| download | `MAX_EXT_LEN` | 10 | 扩展名最大长度 |

### 5.18 `mock.rs` — 测试替身

为每个 trait 提供测试 mock：

| Mock | 功能 |
|------|------|
| `MockSender` | 记录所有发送的消息，支持 `fail_send_remaining` 模拟失败 |
| `MockFileDownloader` | 记录下载请求，返回预设元数据 |

`HandlerDeps::mock()` 用来构建完全隔离的测试环境。

---

## 6. 关键设计模式

### 6.1 依赖注入（DI）

通过 `HandlerDeps` 容器 + trait object 实现构造器注入：

```
生产: HandlerDeps::production() → 真实实现
测试: HandlerDeps::mock()       → mock.rs 替身
```

router 和 pipeline 只依赖 `&HandlerDeps`，不知道具体实现。

### 6.2 Adapter 模式

`StreamEventMapper` 将 anureo 的 `AnyStreamEvent` 适配为内部的 `StreamCommand`，解耦了 Agent 运行时和 Telegram UI。

### 6.3 Command 模式

斜杠命令各自实现 `BotCommand` trait，`CommandDispatcher` 做第一匹配分发。新增命令只需实现 trait 并注册。

### 6.4 Producer-Consumer

流式响应使用 `mpsc::channel` 连接：
- **Producer**：`StreamEventMapper`（在 anureo callback 中）
- **Consumer**：`stream_message_handler`（独立的 tokio task）

### 6.5 背压（Backpressure）

channel 满时的两种策略：
- **Critical** 命令（阶段转换、ToolStart/End）：阻塞等待
- **BestEffort** 命令（文本增量）：`try_send()` 丢弃

---

## 7. 配置示例

```toml
# ~/.anureo/telegram-bot.toml

[settings]
download_dir = "downloads"
log_level = "info"
# log_file = "logs/telegram-bot.log"

[settings.streaming]
show_think_phase = true
show_act_phase = true
edit_interval_ms = 300

[bots.assistant]
token = "${TELOXIDE_TOKEN}"
enabled = true
# allowed_chats = [123456789]

[bots.assistant.agent]
system_prompt = "You are a helpful assistant."
model_override = "gpt-4o"
interaction_mode = "streaming"  # or "periodic_summary"
```

---

## 8. 测试架构

### 单元测试

- `metrics.rs` 内联 `#[cfg(test)]` — counter 递增验证
- `config/tests.rs` — 配置加载与解析
- `formatting_tests.rs` — MarkdownV2 转义验证
- `handler_tests.rs` — handler 逻辑验证

### 集成测试（`tests/`）

| 测试文件 | 覆盖场景 |
|----------|---------|
| `bot_startup_test.rs` | Bot 启动与配置加载 |
| `concurrency_test.rs` | 并发 agent 调用守卫 |
| `handler_dispatch_mock_test.rs` | 消息分发到正确处理器 |
| `integration_test.rs` | 端到端消息流 |
| `message_flow_test.rs` | 文本/媒体/命令消息流 |
| `streaming_message_handler_test.rs` | 流式消息更新 |

测试使用 `HandlerDeps::mock()` 完全隔离 Telegram API 和 anureo Agent。

---

## 9. 模块依赖关系图

```
                    main.rs
                      │
                      ▼
                   lib.rs
                      │
          ┌───────────┼───────────┐
          ▼           ▼           ▼
       config       bot.rs     logging.rs
          │           │
          │           ▼
          │      BotManager
          │           │
          │     ┌─────┴─────┐
          │     ▼           ▼
          │  health.rs   router.rs
          │                  │
          │                  ▼
          │           handler_deps.rs ◄── traits.rs
          │                  │               ▲
          │                  ▼               │
          │            pipeline/             │
          │           ┌──┴──┐          实现 │
          │           ▼     ▼               │
          │       command  agent.rs ─────────┤
          │                   │              │
          │                   ▼              │
          │             streaming/           │
          │            ┌──┬──┬──┐           │
          │            ▼  ▼  ▼  ▼           │
          │      agent event msg retry      │
          │      .rs  _map  _handler.rs     │
          │           per.rs                │
          │                                │
          │         sender.rs ──────────────┤
          │         session.rs ─────────────┤
          │         download.rs ────────────┤
          │                                │
          └────────────────────────────────┘

  辅助模块: formatting/, constants.rs, error.rs,
            utils.rs, metrics.rs, mock.rs
```

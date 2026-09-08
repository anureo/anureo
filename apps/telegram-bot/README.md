# Telegram Bot 多机器人方案

基于 teloxide 的多机器人框架，支持配置文件管理、Long Polling、声明式消息路由。

## 项目结构

```
~/.anureo/
├── config.toml              # anureo 主配置（LLM/通用）
├── telegram-bot.toml        # Bot 专用配置
└── .env                     # 环境变量

apps/telegram-bot/
├── Cargo.toml
├── telegram-bot.example.toml # 示例配置
└── src/
    ├── config/              # 配置模块
    │   ├── mod.rs
    │   ├── telegram.rs      # Bot 配置定义
    │   └── loader.rs        # 配置加载器
    ├── bot.rs               # Bot 实例 + Long Polling
    ├── handler.rs           # dptree 消息分发
    └── lib.rs               # 库入口
```

## 快速开始

```bash
# 1. 复制示例配置到 ANUREO_HOME
mkdir -p ~/.anureo
cp apps/telegram-bot/telegram-bot.example.toml ~/.anureo/telegram-bot.toml

# 2. 编辑配置，填写 bot token
vim ~/.anureo/telegram-bot.toml

# 3. 设置环境变量（可选，推荐用于 token）
export TELOXIDE_TOKEN="123456:ABC-DEF"

# 4. 运行
cargo run -p telegram-bot
```

## 配置文件

配置文件位置（按优先级）：

1. `$ANUREO_HOME/telegram-bot.toml`（推荐）
2. `./telegram-bot.toml`（当前目录）

### 配置示例

```toml
# ~/.anureo/telegram-bot.toml

[settings]
# 下载目录（相对于工作目录或绝对路径）
download_dir = "downloads"
# 日志级别: trace, debug, info, warn, error
log_level = "info"

# 机器人配置
[bots.assistant]
# 使用环境变量插值（推荐，更安全）
token = "${TELOXIDE_TOKEN}"
enabled = true
description = "助手机器人"

[bots.notification]
# 或直接写 token（不推荐）
token = "987654321:XYZabcDEF..."
enabled = false
description = "通知机器人"

# Agent 集成（未来功能）
[agent]
enabled = false
provider = "openai"
model = "gpt-4o-mini"
```

### 环境变量插值

支持在配置中使用 `${VAR}` 语法引用环境变量：

```toml
[bots.my_bot]
token = "${TELOXIDE_TOKEN}"        # 从环境变量读取
# 或
api_url = "${TELOXIDE_API_URL}"    # 自定义 API 地址
```

### 配置项说明

| 配置项 | 类型 | 默认值 | 说明 |
|--------|------|--------|------|
| `settings.download_dir` | String | `"downloads"` | 媒体文件下载目录 |
| `settings.log_level` | String | `"info"` | 日志级别 |
| `bots.<name>.token` | String | 必填 | Bot Token（支持环境变量插值）|
| `bots.<name>.enabled` | bool | `true` | 是否启用 |
| `bots.<name>.description` | String | 可选 | 机器人描述 |
| `agent.enabled` | bool | `false` | 启用 Agent 集成（未来功能）|

### ANUREO_HOME 环境变量

默认配置目录为 `~/.anureo`，可通过环境变量覆盖：

```bash
export ANUREO_HOME="/custom/path/.anureo"
```

## 核心概念

### 1. 多机器人架构

每个机器人独立运行在自己的 tokio task 中：

```
main()
  ├── tokio::spawn(bot1)  ─→  Polling  ─→  Dispatcher  ─→  Handlers
  ├── tokio::spawn(bot2)  ─→  Polling  ─→  Dispatcher  ─→  Handlers
  └── tokio::spawn(bot3)  ─→  Polling  ─→  Dispatcher  ─→  Handlers
```

### 2. Long Polling

teloxide 内置 polling 机制：

- **工作原理**: 客户端持续向 Telegram 服务器请求更新
- **指数退避**: 网络断开时自动重试，延迟递增
- **优势**: 简单可靠，无需公网 IP、无需 HTTPS

```rust
// bot.rs
let polling = Polling::builder(bot.clone())
    .timeout(30)                    // 请求超时 30s
    .allowed_updates(AllowedUpdates::all())
    .build();
```

### 3. dptree 消息分发

声明式路由，基于责任链模式：

```rust
// handler.rs
fn schema() -> UpdateHandler<BoxError> {
    dptree::entry()
        // 分支1: 处理命令
        .branch(Update::filter_message()
            .branch(filter_command::<Command, _>()
                .branch(case![Command::Start].endpoint(start))
                .branch(case![Command::Help].endpoint(help))
            )
        )
        // 分支2: 处理普通消息
        .branch(Update::filter_message()
            .endpoint(message_handler)
        )
}
```

### 4. 依赖注入

Handler 参数自动从 context 中获取：

```rust
async fn start(bot: Bot, msg: Message) -> HandlerResult {
    // Bot 和 Message 自动注入
    bot.send_message(msg.chat.id, "Hello!").await?;
    Ok(())
}

async fn message_handler(bot: Bot, msg: Message, dialogue: MyDialogue) -> HandlerResult {
    // Dialogue 状态管理器也可注入
    let state = dialogue.get_or_default().await?;
    // ...
}
```

## 扩展功能

### 状态管理（对话）

```rust
use teloxide::dispatching::dialogue::{Dialogue, InMemStorage};

#[derive(Clone, Default)]
enum State {
    #[default]
    Start,
    WaitingForName,
    WaitingForAge,
}

type MyDialogue = Dialogue<State, InMemStorage<State>>;

async fn start(bot: Bot, dialogue: MyDialogue, msg: Message) -> HandlerResult {
    dialogue.update(State::WaitingForName).await?;
    bot.send_message(msg.chat.id, "What's your name?").await?;
    Ok(())
}
```

### 持久化存储

```rust
// SQLite
use teloxide::dispatching::dialogue::SqliteStorage;

type MyDialogue = Dialogue<State, SqliteStorage<State>>;

// Redis
use teloxide::dispatching::dialogue::RedisStorage;

type MyDialogue = Dialogue<State, RedisStorage<State>>;
```

### 命令定义

```rust
use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase")]
enum Command {
    #[command(description = "启动机器人")]
    Start,
    #[command(description = "显示帮助")]
    Help,
    #[command(description = "设置 <key> <value>")]
    Set { key: String, value: String },
}
```

### 中间件

```rust
fn schema() -> UpdateHandler<BoxError> {
    dptree::entry()
        // 日志中间件
        .inspect(|update: Update| {
            tracing::info!("Received update: {:?}", update.id);
        })
        // 权限检查
        .filter(|msg: Message| {
            msg.from().map(|u| u.id == ADMIN_ID).unwrap_or(false)
        })
        .branch(/* handlers */)
}
```

## 消息格式

### Update 结构

Telegram 推送的每条消息都包装在 `Update` 结构中：

```rust
use teloxide::types::Update;

pub struct Update {
    pub id: i64,           // Update ID（递增）
    pub kind: UpdateKind,  // 更新类型
}

pub enum UpdateKind {
    Message(Message),              // 新消息
    EditedMessage(Message),        // 编辑的消息
    ChannelPost(Message),          // 频道帖子
    EditedChannelPost(Message),    // 编辑的频道帖子
    InlineQuery(InlineQuery),      // 内联查询
    CallbackQuery(CallbackQuery),  // 回调查询（按钮点击）
    // ... 更多类型
}
```

### Message 结构

`Message` 是最常用的类型：

```rust
pub struct Message {
    pub id: MessageId,              // 消息 ID
    pub date: DateTime<Utc>,        // 发送时间
    pub chat: Chat,                 // 所属聊天
    pub from: Option<User>,         // 发送者（频道消息可能为空）
    pub kind: MessageKind,          // 消息内容类型
}

pub enum MessageKind {
    Common(MessageCommon),          // 普通消息（文本/图片/视频等）
    NewChatMembers(...),            // 新成员加入
    LeftChatMember(...),            // 成员离开
    // ... 其他系统消息
}
```

### MessageCommon - 普通消息内容

```rust
pub struct MessageCommon {
    pub text: Option<String>,           // 文本内容
    pub entities: Option<Vec<MessageEntity>>,  // Markdown/链接等
    pub caption: Option<String>,        // 媒体说明文字
    
    // 媒体类型（互斥，只有一个有值）
    pub photo: Option<Vec<PhotoSize>>,  // 图片
    pub video: Option<Video>,           // 视频
    pub audio: Option<Audio>,           // 音频
    pub document: Option<Document>,     // 文件
    pub sticker: Option<Sticker>,       // 贴纸
    pub voice: Option<Voice>,           // 语音
    // ...
}
```

### 处理示例

```rust
async fn handle_message(bot: Bot, msg: Message) -> HandlerResult {
    // 获取文本
    if let Some(text) = msg.text() {
        bot.send_message(msg.chat.id, format!("收到: {}", text)).await?;
    }
    
    // 检查是否有图片
    if let Some(photos) = msg.photo() {
        let photo = &photos[0]; // 取第一个尺寸
        bot.send_message(msg.chat.id, format!("图片 ID: {}", photo.file.id)).await?;
    }
    
    // 获取发送者信息
    if let Some(user) = msg.from() {
        let name = user.full_name();
        let user_id = user.id.0;
    }
    
    Ok(())
}
```

### CallbackQuery - 按钮回调

```rust
async fn handle_callback(bot: Bot, q: CallbackQuery) -> HandlerResult {
    if let Some(data) = q.data {
        // data 是按钮的 callback_data
        match data.as_str() {
            "confirm" => { /* 确认操作 */ },
            "cancel" => { /* 取消操作 */ },
            _ => {}
        }
        
        // 必须应答回调，否则按钮会一直转圈
        bot.answer_callback_query(q.id).await?;
    }
    Ok(())
}
```

---

## 图片下载方案

### 配置下载目录

在 `config.toml` 中配置下载目录：

```toml
[settings]
download_dir = "downloads"  # 可使用相对路径或绝对路径
```

### 基本流程

```text
1. 从 Message 中获取 Photo/Document
2. 提取 FileId
3. 调用 bot.get_file() 获取文件路径
4. 使用 DownloadConfig 获取保存路径
5. 调用 bot.download_file() 下载到本地
```

### 代码实现

```rust
use teloxide::net::Download;
use teloxide::types::{FileId, PhotoSize};
use tokio::fs;
use std::path::{Path, PathBuf};

/// 下载配置（从配置文件读取）
pub struct DownloadConfig {
    pub dir: PathBuf,  // 从 settings.download_dir 读取
}

impl DownloadConfig {
    /// 获取文件保存路径
    pub fn get_path(&self, filename: &str) -> PathBuf {
        self.dir.join(filename)
    }
}

/// 下载图片到本地
async fn download_photo(
    bot: &Bot, 
    photos: &[PhotoSize],
    config: &DownloadConfig
) -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    // 选择最大尺寸的图片
    let largest = photos.last().unwrap();
    
    // 构建保存路径（使用配置的下载目录）
    let filename = format!("photo_{}.jpg", chrono::Utc::now().timestamp());
    let path = config.get_path(&filename);
    
    // 获取文件信息
    let file = bot.get_file(FileId(largest.file.id.clone())).await?;
    
    // 创建目标文件（确保目录存在）
    fs::create_dir_all(&config.dir).await?;
    let mut dst = fs::File::create(&path).await?;
    
    // 下载文件
    bot.download_file(&file.path, &mut dst).await?;
    
    log::info!("图片已保存到: {:?}", path);
    Ok(path)
}

/// 处理图片消息
async fn handle_photo(bot: Bot, msg: Message, config: Arc<DownloadConfig>) -> HandlerResult {
    if let Some(photos) = msg.photo() {
        let path = download_photo(&bot, photos, &config).await?;
        
        bot.send_message(
            msg.chat.id, 
            format!("图片已保存: {}", path.display())
        ).await?;
    }
    Ok(())
}
```

支持相对路径（相对于工作目录）或绝对路径。

### 使用 DownloadConfig

`handler.rs` 提供了 `DownloadConfig` 结构，支持从配置读取下载目录：

```rust
use crate::handler::{DownloadConfig, download_file, download_photo};

// 从配置创建 DownloadConfig
let download_config = DownloadConfig::new(&config.settings.download_dir);

// 初始化下载目录
download_config.init().await?;

// 下载图片（自动使用配置的目录）
if let Some(photos) = msg.photo() {
    let path = download_config.get_path(&format!("photo_{}.jpg", msg.id.0), None);
    download_photo(&bot, photos, &path).await?;
}
```

### 代码实现

```rust
use teloxide::net::Download;
use teloxide::types::{FileId, PhotoSize};
use tokio::fs;
use std::path::{Path, PathBuf};

/// DownloadConfig - 下载配置
pub struct DownloadConfig {
    pub dir: PathBuf,              // 下载目录
    pub organize_by_date: bool,    // 按日期分目录
    pub organize_by_chat: bool,    // 按聊天 ID 分目录
}

impl DownloadConfig {
    /// 从配置文件创建
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            organize_by_date: false,
            organize_by_chat: false,
        }
    }
    
    /// 获取完整保存路径
    pub fn get_path(&self, filename: &str, chat_id: Option<i64>) -> PathBuf {
        let mut path = self.dir.clone();
        
        // 可选：按聊天 ID 分目录
        if self.organize_by_chat {
            if let Some(id) = chat_id {
                path.push(format!("chat_{}", id));
            }
        }
        
        // 可选：按日期分目录
        if self.organize_by_date {
            let date = chrono::Local::now().format("%Y-%m-%d").to_string();
            path.push(date);
        }
        
        path.push(filename);
        path
    }
    
    /// 初始化下载目录
    pub async fn init(&self) -> std::io::Result<()> {
        fs::create_dir_all(&self.dir).await
    }
}

/// 下载图片到本地
async fn download_photo(
    bot: &Bot, 
    photos: &[PhotoSize],
    save_path: &Path
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 选择最大尺寸的图片（最后一个）
    let largest = photos.last().unwrap();
    
    // 获取文件信息
    let file = bot.get_file(FileId(largest.file.id.clone())).await?;
    
    // 确保父目录存在
    if let Some(parent) = save_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    
    // 创建目标文件并下载
    let mut dst = fs::File::create(save_path).await?;
    bot.download_file(&file.path, &mut dst).await?;
    
    log::info!("图片已保存到: {:?}", save_path);
    Ok(())
}

/// 处理图片消息
async fn handle_photo(bot: Bot, msg: Message, config: DownloadConfig) -> HandlerResult {
    if let Some(photos) = msg.photo() {
        // 使用配置的下载目录
        let filename = format!("photo_{}.jpg", msg.id.0);
        let save_path = config.get_path(&filename, Some(msg.chat.id.0));
        
        // 下载
        download_photo(&bot, photos, &save_path).await?;
        
        // 回复用户
        bot.send_message(
            msg.chat.id, 
            format!("图片已保存: {:?}", save_path)
        ).await?;
    }
    Ok(())
}
```

### 处理其他文件类型

```rust
use teloxide::types::Document;

async fn handle_document(bot: Bot, msg: Message) -> HandlerResult {
    if let Some(doc) = msg.document() {
        let filename = doc.file_name.as_deref().unwrap_or("unknown");
        let ext = filename.rsplit('.').next().unwrap_or("");
        
        match ext.to_lowercase().as_str() {
            "jpg" | "jpeg" | "png" | "gif" | "webp" => {
                // 图片文件
                let save_path = format!("downloads/{}", filename);
                download_file(&bot, &doc.file.id, &save_path).await?;
            }
            "pdf" => {
                // PDF 文件
                let save_path = format!("downloads/{}", filename);
                download_file(&bot, &doc.file.id, &save_path).await?;
            }
            "mp4" | "mov" | "avi" => {
                // 视频文件
                let save_path = format!("downloads/{}", filename);
                download_file(&bot, &doc.file.id, &save_path).await?;
            }
            _ => {
                bot.send_message(msg.chat.id, "不支持的文件类型").await?;
            }
        }
    }
    Ok(())
}

/// 通用文件下载
async fn download_file(
    bot: &Bot,
    file_id: &str,
    save_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let file = bot.get_file(FileId(file_id.to_string())).await?;
    let mut dst = fs::File::create(save_path).await?;
    bot.download_file(&file.path, &mut dst).await?;
    Ok(())
}
```

### 流式下载（大文件）

对于大文件，使用流式下载避免内存占用过高：

```rust
use futures::StreamExt;

async fn download_large_file(
    bot: &Bot,
    file_id: &str,
    save_path: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let file = bot.get_file(FileId(file_id.to_string())).await?;
    let mut stream = bot.download_file_stream(&file.path);
    let mut dst = fs::File::create(save_path).await?;
    
    use tokio::io::AsyncWriteExt;
    
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        dst.write_all(&bytes).await?;
    }
    
    Ok(())
}
```

### 添加到 dptree 路由

```rust
use teloxide::dispatching::UpdateFilterExt;

fn schema() -> UpdateHandler<Box<dyn std::error::Error + Send + Sync + 'static>> {
    dptree::entry()
        // 图片消息
        .branch(Update::filter_message().filter(|msg: Message| msg.photo().is_some()).endpoint(handle_photo))
        // 文档消息
        .branch(Update::filter_message().filter(|msg: Message| msg.document().is_some()).endpoint(handle_document))
        // 文本消息
        .branch(Update::filter_message().endpoint(handle_text))
}
```

---

## Webhook 模式（可选）

如果需要 serverless 部署：

```rust
use teloxide::update_listeners::webhooks::{Options, axum};

let (addr, server) = axum(bot.clone(), Options::new(addr)).await?;

// server 是 axum 服务器
tokio::spawn(server);

// 使用 dispatcher 处理
Dispatcher::builder(bot, schema())
    .build()
    .dispatch_with_listener(
        listener,
        LoggingErrorHandler::with_custom_text("Error"),
    )
    .await;
```

## 部署建议

### Docker

```dockerfile
FROM rust:1.77 AS builder
WORKDIR /app
COPY . .
RUN cargo build -p telegram-bot --release

FROM debian:bookworm-slim
COPY --from=builder /app/target/release/telegram-bot /usr/local/bin/
CMD ["telegram-bot"]
```

### 环境变量

也可以通过环境变量覆盖配置：

```bash
export BOT_TOKEN_1="123456:ABC"
export BOT_TOKEN_2="789012:XYZ"
cargo run -p telegram-bot
```

## 参考资料

- [teloxide 官方文档](https://docs.rs/teloxide)
- [teloxide GitHub](https://github.com/teloxide/teloxide)
- [dptree 路由库](https://docs.rs/dptree)
- [Telegram Bot API](https://core.telegram.org/bots/api)

## 许可证

MIT

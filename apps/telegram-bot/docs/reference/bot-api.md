# Bot API 参考

telegram-bot 的核心 trait 和接口定义，用于扩展和测试。

## 核心 Trait

### `AgentRunner`

AI Agent 执行器抽象。`anureoAgentRunner` 是默认实现。

| 方法 | 参数 | 返回值 | 说明 |
|------|------|--------|------|
| `run` | `AgentRunContext` | `Result<RunCompletion, BotError>` | 执行 Agent 并返回流式事件 |

```rust
#[async_trait]
pub trait AgentRunner: Send + Sync {
    async fn run(&self, context: AgentRunContext) -> Result<RunCompletion, BotError>;
}
```

### `MessageSender`

消息发送抽象。`TeloxideSender` 是默认实现。

| 方法 | 参数 | 返回值 | 说明 |
|------|------|--------|------|
| `send_text` | `chat_id, text` | `Result<Message, BotError>` | 发送纯文本消息 |
| `send_formatted` | `chat_id, FormattedMessage` | `Result<Message, BotError>` | 发送格式化消息（MarkdownV2/HTML） |
| `edit_message_text` | `chat_id, message_id, text` | `Result<Message, BotError>` | 编辑已有消息 |
| `send_reaction` | `chat_id, message_id, emoji` | `Result<(), BotError>` | 发送消息反应 |

```rust
#[async_trait]
pub trait MessageSender: Send + Sync {
    async fn send_text(&self, chat_id: i64, text: &str) -> Result<Message, BotError>;
    async fn send_formatted(&self, chat_id: i64, msg: &FormattedMessage) -> Result<Message, BotError>;
    async fn edit_message_text(&self, chat_id: i64, msg_id: i32, text: &str) -> Result<Message, BotError>;
}
```

### `FileDownloader`

文件下载抽象。`TeloxideDownloader` 是默认实现。

| 方法 | 参数 | 返回值 | 说明 |
|------|------|--------|------|
| `download_photo` | `Bot, PhotoSize` | `Result<FileMetadata, BotError>` | 下载图片 |
| `download_video` | `Bot, Video` | `Result<FileMetadata, BotError>` | 下载视频 |
| `download_document` | `Bot, Document` | `Result<FileMetadata, BotError>` | 下载文档 |

### `SessionManager`

会话管理抽象。`SqliteSessionManager` 是默认实现。

| 方法 | 参数 | 返回值 | 说明 |
|------|------|--------|------|
| `reset_session` | `chat_id` | `Result<(), BotError>` | 重置指定聊天的会话 |
| `get_model` | `chat_id` | `Result<Option<String>, BotError>` | 获取当前模型 |
| `set_model` | `chat_id, model_id` | `Result<(), BotError>` | 设置当前模型 |

### `BotCommand`

斜杠命令抽象。详见 [斜杠命令系统](../concepts/slash-commands.md)。

## 数据类型

### `AgentRunContext`

Agent 执行上下文：

```rust
pub struct AgentRunContext {
    pub chat_id: i64,
    pub message_id: i32,
    pub prompt: String,
    pub model: Option<String>,
    pub files: Vec<FileMetadata>,
    pub system_prompt: Option<String>,
}
```

### `FormattedMessage`

格式化消息，支持 MarkdownV2 和 HTML：

```rust
pub struct FormattedMessage {
    pub text: String,
    pub parse_mode: Option<ParseMode>,  // MarkdownV2 | Html
    pub plain_text_fallback: String,
}
```

### `FileMetadata`

下载文件元数据：

```rust
pub struct FileMetadata {
    pub path: PathBuf,
    pub file_name: String,
    pub mime_type: Option<String>,
    pub size_bytes: u64,
}
```

### `BotError`

统一错误类型：

| 变体 | 含义 |
|------|------|
| `Config(String)` | 配置错误 |
| `Network(teloxide::RequestError)` | Telegram API 网络错误 |
| `Io(std::io::Error)` | 文件 I/O 错误 |
| `Agent(String)` | Agent 执行错误 |
| `AgentRun(anureo::RunError)` | anureo 运行时错误 |
| `RateLimit` | API 频率限制 |
| `Session(String)` | 会话管理错误 |

## 测试 Mock

`mock.rs` 提供了所有 trait 的 mock 实现，用于单元测试：

- `MockAgentRunner` — 可配置返回结果的 Agent mock
- `MockMessageSender` — 记录所有调用的 Sender mock
- `MockFileDownloader` — 返回固定元数据的 Downloader mock

## 相关链接

- [消息处理管线](../concepts/message-pipeline.md) — 这些 trait 在管线中的使用
- [斜杠命令系统](../concepts/slash-commands.md) — `BotCommand` trait 详解

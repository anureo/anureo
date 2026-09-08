# TTS 与 Dictation 扩展

> 命名空间: `_anureo.dev/tts/*`、`_anureo.dev/dictation/*`
> Capability key: `tts`、`dictation`
> 实现状态: ❌ 未实现

---

## Capability

```json
{
  "tts": {
    "synthesize": true,
    "summarize": true
  },
  "dictation": {
    "stream": true
  }
}
```

TTS 和 Dictation 使用独立 WebSocket 子流返回音频和识别结果，不走标准 JSON-RPC request/response 循环。

**音频隔离规则：**
- 音频 payload **不混入** ACP `session/update`
- TTS 摘要的文本部分可使用 `agent_message_chunk` 传递，但音频 payload 必须通过 WebSocket 子流
- Dictation 是双向 WebSocket 子流，不是标准 JSON-RPC request
- Transport 层保持 dictation/tts frame 与 ACP message 语义独立
- 断线和重连不中断当前 ACP session

---

## TTS Methods

### `_anureo.dev/tts/synthesize`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `tts.synthesize` |
| 权限 | 无 |
| Timeout | 60s（文本较长时可能需要更长时间） |

将文本合成为语音音频。音频通过 chunked response 或 WebSocket 子流返回。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/tts/synthesize",
  "params": {
    "text": "Hello, this is a text to speech synthesis test.",
    "sessionId": "session-abc123",
    "voice": "alloy",
    "format": "mp3",
    "speed": 1.0,
    "substream": true
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `text` | string | 是 | 要合成的文本 |
| `sessionId` | string | 否 | 关联的 session ID |
| `voice` | string | 否 | 语音选择（如 `alloy`/`nova`/`echo`），取决于 TTS provider |
| `format` | `"mp3"` \| `"opus"` \| `"wav"` \| `"aac"` | 否 | 音频格式，默认 `"mp3"` |
| `speed` | number | 否 | 语速倍率（0.25 - 4.0），默认 `1.0` |
| `substream` | bool | 否 | 是否通过 WebSocket 子流返回音频；默认 `true` |

#### Response（substream 模式）

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "mode": "substream",
    "substreamId": "tts-stream-001",
    "substreamUrl": "wss://anureo.example.com/substream?type=tts&sessionId=session-abc123",
    "format": "mp3",
    "estimatedDurationMs": 3200
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `mode` | `"substream"` \| `"chunked"` | 音频返回方式 |
| `substreamId` | string | 子流标识（`substream` 模式） |
| `substreamUrl` | string | WebSocket 子流 URL（`substream` 模式） |
| `format` | string | 音频格式 |
| `estimatedDurationMs` | number | 预计音频时长（毫秒） |

#### Response（chunked 模式）

当 `substream: false` 时，音频以 base64 编码嵌入 response：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "mode": "chunked",
    "audio": "base64-encoded-audio-data...",
    "format": "mp3",
    "durationMs": 3100
  }
}
```

#### 逻辑说明

1. **子流优先**: 默认使用 WebSocket 子流，因为 TTS 音频可能较大（数百 KB 到数 MB），嵌入 JSON-RPC response 会阻塞消息通道。
2. **子流生命周期**: Client 连接 `substreamUrl` 后，Server 推送音频帧（`type: "audio"`），完成后发送 `control: { command: "end_of_stream" }`。
3. **Provider 抽象**: Server 内部对接 TTS provider（如 OpenAI TTS、Azure Speech），Client 不关心具体实现。
4. **文本长度**: 超长文本（> 4096 字符）Server 自动分段合成，通过子流连续推送。
5. **缓存**: 相同文本 + voice + format 的合成结果可短期缓存（由 server policy 决定）。

#### Rust 类型

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TtsAudioFormat {
    Mp3,
    Opus,
    Wav,
    Aac,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsSynthesizeRequest {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default = "default_format")]
    pub format: TtsAudioFormat,
    #[serde(default = "default_speed")]
    pub speed: f32,
    #[serde(default = "default_true")]
    pub substream: bool,
}

fn default_format() -> TtsAudioFormat { TtsAudioFormat::Mp3 }
fn default_speed() -> f32 { 1.0 }
fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TtsResponseMode {
    Substream,
    Chunked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsSynthesizeResponse {
    pub mode: TtsResponseMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub substream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub substream_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    pub format: TtsAudioFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `text` 为空或超过长度限制 |
| `Capability Not Supported (-32001)` | TTS provider 未配置 |
| `Internal Error (-32603)` | TTS provider 返回错误 |

---

### `_anureo.dev/tts/summarize`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `tts.summarize` |
| 权限 | 无 |

先用 small model 摘要文本，再合成语音。一步操作完成"摘要并朗读"。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/tts/summarize",
  "params": {
    "text": "（长文本，例如 session 中 assistant 的完整回复）...",
    "sessionId": "session-abc123",
    "maxSummaryLength": 500,
    "voice": "nova",
    "format": "mp3",
    "substream": true
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `text` | string | 是 | 要摘要并合成的长文本 |
| `sessionId` | string | 否 | 关联的 session ID |
| `maxSummaryLength` | number | 否 | 摘要最大字符数，默认 500 |
| `voice` | string | 否 | 语音选择 |
| `format` | TtsAudioFormat | 否 | 音频格式，默认 `"mp3"` |
| `substream` | bool | 否 | 是否通过子流返回，默认 `true` |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "summary": "This is a concise summary of the long text...",
    "mode": "substream",
    "substreamId": "tts-stream-002",
    "substreamUrl": "wss://anureo.example.com/substream?type=tts&sessionId=session-abc123",
    "format": "mp3",
    "estimatedDurationMs": 1500
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `summary` | string | Small model 生成的摘要文本 |
| 后续字段 | - | 同 `synthesize` response |

#### 逻辑说明

1. **两阶段**: Server 先调用 small model（见 `extensions/32-small-model.md`）生成摘要，再调用 TTS 合成。两步在 server 内部完成，Client 只发一次 request。
2. **摘要策略**: Small model 对 `text` 生成 `maxSummaryLength` 以内的摘要。摘要失败时降级为截取前 N 个字符。
3. **文本+音频**: Client 可同时使用 `summary` 文本显示字幕，并通过子流播放音频。
4. **不产生 session/update**: TTS 操作不是 agent turn，不产生 ACP `session/update` 事件。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsSummarizeRequest {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default = "default_summary_len")]
    pub max_summary_length: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<String>,
    #[serde(default = "default_format")]
    pub format: TtsAudioFormat,
    #[serde(default = "default_true")]
    pub substream: bool,
}

fn default_summary_len() -> u32 { 500 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TtsSummarizeResponse {
    pub summary: String,
    pub mode: TtsResponseMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub substream_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub substream_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<String>,
    pub format: TtsAudioFormat,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `text` 为空 |
| `Internal Error (-32603)` | Small model 或 TTS provider 返回错误 |

---

## Dictation Methods

### `_anureo.dev/dictation/stream`

| 项目 | 内容 |
|---|---|
| 方向 | 双向 WebSocket 子流 |
| Capability | `dictation.stream` |
| 权限 | 无 |

双向流式语音识别。Client 发送音频帧，Server 返回识别结果。

> **注意：** Dictation **不是标准 JSON-RPC request**。它是独立的 WebSocket 子流，遵循 `08-cross-cutting-patterns.md` §7 的子流生命周期规范。本节描述子流的建立协议和帧格式。

#### 子流建立

Client 发起 WebSocket 连接：

```
wss://anureo.example.com/substream?type=dictation&sessionId=<session-id>
```

请求必须携带与 parent ACP connection 相同的 Bearer token。

Server 校验后分配 `substreamId` 并进入 `active` 状态，发送第一个 control frame：

```json
{
  "substreamId": "dict-001",
  "type": "control",
  "payload": {
    "command": "start",
    "config": {
      "sampleRate": 16000,
      "encoding": "linear16",
      "language": "en-US",
      "interimResults": true
    }
  }
}
```

#### Client → Server 帧

**音频帧（binary WebSocket frame）：**

```
[2 bytes: frame header][N bytes: raw audio data]
```

或 JSON control frame：

```json
{
  "substreamId": "dict-001",
  "type": "control",
  "payload": {
    "command": "end_of_stream"
  }
}
```

| Client control command | 说明 |
|---|---|
| `start` | 开始识别（可选，连接建立后自动开始） |
| `stop` | 停止识别但保持连接 |
| `end_of_stream` | 音频输入结束，Server 返回最终结果后关闭子流 |
| `error` | Client 侧错误 |

#### Server → Client 帧

**识别结果帧（text frame）：**

```json
{
  "substreamId": "dict-001",
  "type": "text",
  "payload": {
    "transcript": "hello world",
    "isFinal": false,
    "confidence": 0.92,
    "alternatives": [
      { "transcript": "hello world", "confidence": 0.92 },
      { "transcript": "hello Word", "confidence": 0.75 }
    ]
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `payload.transcript` | string | 识别文本 |
| `payload.isFinal` | bool | 是否为最终结果（`false` 为中间结果） |
| `payload.confidence` | number | 置信度 0-1 |
| `payload.alternatives` | Alternative[] | 候选识别结果 |

**Control 帧：**

```json
{
  "substreamId": "dict-001",
  "type": "control",
  "payload": {
    "command": "error",
    "message": "Audio format not supported"
  }
}
```

#### 逻辑说明

1. **双向独立**: Dictation 子流与 ACP JSON-RPC 消息完全独立。音频帧不通过 JSON-RPC 传输，避免序列化开销和阻塞。
2. **中间结果**: `interimResults: true` 时，Server 持续推送 `isFinal: false` 的中间识别结果。Client 可实时显示部分识别文本。
3. **最终结果**: 音频流结束（`end_of_stream`）或检测到静音间隔后，Server 推送 `isFinal: true` 的最终结果。
4. **持久化**: 最终识别结果可通过 `session/prompt` 发送给 Agent，或写入 session metadata。中间结果不持久化。
5. **语言**: 支持多语言识别。`language` 在 `start` control frame 中指定。

#### Backpressure

1. **Server 输出 buffer 满时**丢弃最旧的中间帧（`isFinal: false`），不阻塞 ACP session。
2. **Client 必须能处理帧丢弃**（中间结果不连续是正常的）。
3. **最终结果**不可丢弃；Server 保证至少推送一个 `isFinal: true` 的结果。
4. **Client 输入**：Client 应以合理速率发送音频帧（如每 100ms 一帧）。Server 输入 buffer 满时发送 `control: { command: "error", message: "backpressure" }` 并暂停处理，Client 应减慢发送速率。

#### 断线恢复

1. **子流断开不中断 parent ACP connection**。
2. **Client 可重新 open 子流**，不需要重新 `initialize` 或 `load session`。
3. **丢弃的中间帧不可恢复**；最终结果在 session metadata 或 ACP `session/update` 中持久化。
4. **重连续传不支持**：Dictation 是实时流，重连后从头开始新识别。

#### Rust 类型

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictationConfig {
    pub sample_rate: u32,
    pub encoding: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default = "default_true")]
    pub interim_results: bool,
}

fn default_true() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictationControlPayload {
    pub command: DictationControlCommand,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<DictationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DictationControlCommand {
    Start,
    Stop,
    EndOfStream,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictationTranscriptAlternative {
    pub transcript: String,
    pub confidence: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictationTranscriptPayload {
    pub transcript: String,
    pub is_final: bool,
    #[serde(default = "default_confidence")]
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alternatives: Vec<DictationTranscriptAlternative>,
}

fn default_confidence() -> f32 { 1.0 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DictationFrame {
    pub substream_id: String,
    #[serde(rename = "type")]
    pub frame_type: DictationFrameType,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DictationFrameType {
    Audio,
    Text,
    Control,
}
```

#### Error（通过 control frame 返回）

| Control error | 触发条件 |
|---|---|
| `Audio format not supported` | 编码格式不支持 |
| `Rate limit exceeded` | 音频帧速率过高 |
| `Backpressure` | Server 输入 buffer 满 |
| `Session not found` | sessionId 无效或未绑定 |
| `Authentication failed` | Bearer token 无效 |

---

## Notifications

本扩展无 notification。TTS 和 Dictation 的结果通过子流帧或 request response 传递。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| （无） | （无） | TTS/Dictation 是 transient 流式操作，不持久化状态 |

> **TTS**: 合成结果是一次性音频流，重连后不恢复。
>
> **Dictation**: 子流断开后，中间结果丢失。Client 重新 open 子流从头开始新识别。最终识别结果已通过 `session/prompt` 发送给 Agent 或写入 session metadata，可在 session load 时恢复。

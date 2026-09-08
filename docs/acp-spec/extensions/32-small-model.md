# Small Model（轻量 AI 生成服务）

> 命名空间: `_anureo.dev/small-model/*`
> Capability key: `small-model`

## Capability

```json
{
  "small-model": {
    "describe": true,
    "generate": true
  }
}
```

- 声明 `small-model` capability 后，client 可以查询可用性和发起轻量文本生成请求。
- Small Model 是 **server-managed** 的轻量 AI 生成服务，用于 commit message 生成、PR description 生成、session recap 等辅助场景。
- Small Model **不是一个独立的 agent turn**——不产生 ACP `session/update`，不修改 session message 流。

### 与标准 Session 的区别

| 维度 | 标准 `session/prompt` | `_anureo.dev/small-model/generate` |
|---|---|---|
| Agent turn | 是 | **否** |
| `session/update` | 产生 | **不产生** |
| 工具调用 | 支持 | **不支持** |
| Message 流 | 写入 session 历史 | **不写入** |
| Provider/Model | session 配置 | 优先用 session 当前配置，可 fallback |
| 用途 | 代码生成、对话、任务执行 | commit msg、PR desc、recap 等辅助文本 |

### Provider 解析策略

```
small-model/generate request
  → 解析 session 当前 provider/model
  → 如果可用 → 使用该 provider/model
  → 如果不可用（404 / rate limit）：
      → restrictToPreferredProvider = false → 尝试全局 fallback provider
      → restrictToPreferredProvider = true → 返回空结果（静默跳过）
```

- `restrictToPreferredProvider: true` 禁止全局 fallback，适用于 session-assist 等场景（不允许跨 provider 生成）。
- Resolver 404（provider 配置不存在）时**静默跳过**，不阻塞调用方——返回 `result: null`。
- Small Model 调用是同步的，但通常在 5 秒内完成（非 agentic，无工具循环）。

---

## Methods

### `_anureo.dev/small-model/describe`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `small-model.describe` |
| 权限 | Server-side authorization（已认证连接即可） |

**Request:**

```json
{
  "sessionId": "sess_abc123"
}
```

- `sessionId` 可选；提供时返回该 session 当前 provider/model 的 small model 可用性。

**Response:**

```json
{
  "available": true,
  "preferredProvider": "zhipu",
  "preferredModel": "glm-4-flash",
  "fallbackProvider": "openai",
  "fallbackModel": "gpt-4o-mini",
  "restrictToPreferredProvider": false,
  "supportedTasks": ["commit_message", "pr_description", "recap", "general"],
  "maxTokens": 1024,
  "estimatedLatencyMs": 2000
}
```

**Rust 类型:**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct SmallModelDescribeRequest {
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SmallModelDescribeResponse {
    pub available: bool,
    pub preferred_provider: Option<String>,
    pub preferred_model: Option<String>,
    pub fallback_provider: Option<String>,
    pub fallback_model: Option<String>,
    pub restrict_to_preferred_provider: bool,
    pub supported_tasks: Vec<String>,
    pub max_tokens: u32,
    pub estimated_latency_ms: u32,
}
```

**逻辑说明:**

1. Server 查询当前 small model 配置，包括 preferred provider/model 和 fallback 配置。
2. 如果提供了 `sessionId`，preferred provider/model 解析自该 session 的当前配置。
3. `available: false` 表示没有任何可用的 provider（preferred 和 fallback 都不可用）。
4. `supportedTasks` 列出 server 支持的生成任务类型，UI 可以据此决定显示哪些 AI 辅助功能。
5. `estimatedLatencyMs` 为基于历史数据的预估延迟，非保证值。
6. 此方法是只读查询，不产生任何副作用。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `small-model.describe` | initialize 未声明 |
| `not_found` | Session 不存在 | `sessionId` 无效（仅当提供了 sessionId 时） |
| `internal_error` | Server 内部错误 | 配置读取失败 |

---

### `_anureo.dev/small-model/generate`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| 能力 | `small-model.generate` |
| 权限 | Server-side authorization（已认证连接即可） |
| 超时 | 建议 30s |

**Request:**

```json
{
  "sessionId": "sess_abc123",
  "task": "commit_message",
  "input": "Refactored authentication module:\n- Replaced plaintext password comparison with bcrypt::verify\n- Removed 2 unsafe blocks\n- Added 3 unit tests for password verification",
  "instructions": "Use conventional commits format. Keep the first line under 72 characters.",
  "restrictToPreferredProvider": false,
  "maxTokens": 256
}
```

**Response:**

```json
{
  "result": "feat(auth): replace plaintext password comparison with bcrypt verification\n\n- Replace unsafe plaintext comparison with bcrypt::verify()\n- Remove 2 unsafe blocks in src/auth.rs\n- Add unit tests for password verification edge cases",
  "modelUsed": "glm-4-flash",
  "providerUsed": "zhipu",
  "fellBack": false,
  "tokensUsed": {
    "input": 85,
    "output": 62
  },
  "generatedAt": "2025-08-19T10:00:05Z"
}
```

**静默跳过时 Response（provider 不可用且禁止 fallback）:**

```json
{
  "result": null,
  "modelUsed": null,
  "providerUsed": null,
  "fellBack": false,
  "tokensUsed": null,
  "generatedAt": "2025-08-19T10:00:05Z"
}
```

**Rust 类型:**

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct SmallModelGenerateRequest {
    /// Associated session for provider/model resolution
    #[serde(default)]
    pub session_id: Option<String>,
    /// Task type: "commit_message" | "pr_description" | "recap" | "general"
    pub task: String,
    /// Input text to process (diff, session context, etc.)
    pub input: String,
    /// Optional style/format instructions
    #[serde(default)]
    pub instructions: Option<String>,
    /// When true, disables global provider fallback
    #[serde(default)]
    pub restrict_to_preferred_provider: bool,
    /// Max output tokens (server may clamp)
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}

fn default_max_tokens() -> u32 { 1024 }

#[derive(Debug, Clone, Serialize)]
pub struct SmallModelGenerateResponse {
    /// null when silently skipped (provider unavailable + no fallback)
    pub result: Option<String>,
    pub model_used: Option<String>,
    pub provider_used: Option<String>,
    /// true if fallback provider was used instead of preferred
    pub fell_back: bool,
    pub tokens_used: Option<TokenUsage>,
    pub generated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TokenUsage {
    pub input: u32,
    pub output: u32,
}
```

**逻辑说明:**

1. Server 解析 `sessionId` 对应的 provider/model 作为 preferred provider。
2. 如果 `sessionId` 为空，server 使用全局默认 small model 配置。
3. `task` 字段帮助 server 选择合适的 system prompt 和生成参数（commit_message 使用 conventional commits 格式，pr_description 使用 markdown 格式等）。
4. `input` 为生成所需的原始上下文（diff 内容、session 摘要等）。
5. `restrictToPreferredProvider: true` 时，preferred provider 不可用则返回 `result: null`（静默跳过），**不报错**。
6. `restrictToPreferredProvider: false`（默认）时，preferred provider 不可用则尝试 fallback provider。
7. `fellBack: true` 表示使用了 fallback provider 而非 preferred。
8. 生成结果**不写入 session message 流**——small model 不产生 agent turn。
9. `maxTokens` 由 server clamp 到合法范围（如 64-2048），防止滥用。
10. Token usage 仅在生成成功时返回，用于用量统计。

| Error code | 说明 | 触发条件 |
|---|---|---|
| `capability_not_supported` | 未声明 `small-model.generate` | initialize 未声明 |
| `not_found` | Session 不存在 | `sessionId` 无效 |
| `invalid_params` | 参数校验失败 | `input` 为空、`task` 不在 supportedTasks 中 |
| `rate_limited` | Provider 频率限制 | preferred 和 fallback 都被 rate limit |
| `provider_error` | Provider 返回错误 | 生成过程中 provider API 异常（非静默跳过场景） |
| `timeout` | 生成超时 | 超过 server 配置的超时时间 |
| `internal_error` | Server 内部错误 | 解析/调用过程异常 |

### 静默跳过 vs 报错的边界

| 场景 | 行为 | 返回 |
|---|---|---|
| Preferred provider 配置不存在（resolver 404） | 静默跳过 | `result: null` |
| Preferred provider rate limited + fallback 可用 | Fallback | `fellBack: true` |
| Preferred provider rate limited + fallback 不可用 + `restrictToPreferredProvider: true` | 静默跳过 | `result: null` |
| Preferred provider rate limited + fallback 不可用 + `restrictToPreferredProvider: false` | 报错 | `rate_limited` error |
| Provider 返回内容安全拒绝 | 报错 | `provider_error` |
| Server 内部异常 | 报错 | `internal_error` |

---

## Notifications

Small Model 域**没有 notification**。所有交互通过同步 request/response 完成。

Small Model 的结果不通过 notification 推送——调用方（如 `git/generate_commit_message`、`session-assist/recap`）负责将结果整合到各自的域语义中。

---

## Reconnect Resync 映射

Small Model 是无状态服务，**没有 reconnect resync 需求**。

- `small-model/describe` 可以在 reconnect 后随时调用获取最新配置。
- Small Model 的生成结果不持久化在 server 状态中——如果调用方需要保留结果，由调用方负责（如写入 session metadata）。

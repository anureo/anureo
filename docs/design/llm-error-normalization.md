# LLM Provider 错误归一化与状态码处理架构设计

> **状态**：✅ 已实现（阶段一至四 + error_classifier 收敛，2025-08-19）
> **日期**：2025-08-19
> **关联**：`docs/analysis/models-dev-provider-status-codes.md`（177 个 provider 状态码全量参考）

## 1. 背景与问题

anureo 通过 `foundation/llm` 调用 177 个模型提供商（models.dev 收录），其中 146 个遵循 OpenAI 错误协议、9 个遵循 Anthropic 协议，其余为 Google/Azure/Bedrock 及专有协议。各 provider 的错误语义差异（HTTP 码、业务码、`error.type` 词汇）已在统一状态码文档中整理，但**代码层目前没有任何结构化的错误模型**。

### 1.1 现状盘点

| 模块 | 位置 | 现状 |
|---|---|---|
| 错误类型 | `foundation/llm/src/error.rs:1` | 只有 `LlmError::InvokeFailed(String)` 一个变体，所有错误扁平化为字符串 |
| reqwest 裸调（智谱/DeepSeek 等） | `foundation/llm/src/client/openai_compat/llm_client.rs:118-249` | 传输层指数退避 10 次 + 应用层按 HTTP 码重试 5 次 |
| 状态码判定 | `foundation/llm/src/client/openai_compat/retry.rs:48-77` | `is_retryable_status_for` 只认 429/500/502/503/504 |
| 错误体解析 | `foundation/llm/src/client/openai_compat/retry.rs:79-158` | 解析 `error.message/code/type`，但 `error_type` 解析后从未使用 |
| 提供商限流特判 | `foundation/llm/src/client/openai_compat/retry.rs:93-104` | 硬编码智谱 `1000`/`1301`，漏了 `1310` 等 |
| 字符串分类器 | `foundation/llm/src/support/error_classifier/{openai,bigmodel,minimax}.rs` | 只服务 `ChatOpenAI`（async_openai）路径，且按错误消息字符串匹配 |
| 传输重试 | `foundation/anureo-http-retry/src/lib.rs` | `is_retryable_reqwest_error`、`retry_backoff_for_attempt`（500ms→4s） |
| 客户端重试包装 | `foundation/llm/src/client/retry.rs` | `RetryLlmClient` 在返回错误后追加重试层 |
| agent-core 消费 | `agent/agent-core/src/runner_error.rs` | `RunnerError` 不含 `LlmError`，结构化信息经 `GraphError::ExecutionFailed(String)` 丢失 |

### 1.2 核心痛点

1. **错误无分类**：429 / 401 / 402 / 451 全部变成 `InvokeFailed(String)`，消费端无法区分"退避重试 / 换 Key / 充值 / 改内容"。
2. **两套并行的重试逻辑**：`ChatOpenAICompat`（按 HTTP 码）与 `ChatOpenAI`（按消息字符串）判定结果可能不一致。
3. **provider 特殊语义缺失**：402 余额不足、421/451 内容拦截、智谱 `1310` 周限额、OpenRouter `error_type`、平台 Key 隔离（kimi.com vs kimi.ai）全部未建模。
4. **`Retry-After` 头未利用**：固定指数退避，忽略服务端给出的等待时长。
5. **SSE 流内错误**（HTTP 200 后的 error 事件）不进入同一错误管道。

### 1.3 已有依赖基础

`agent-core` 与 `llm` **均已依赖 `model-spec-core`**（`agent/agent-core/Cargo.toml:80`、`foundation/llm/Cargo.toml:53`），且 `model-spec-core` 的 `models_dev` 模块已按 provider 建模（`Provider { id, name, api, npm, models }`、`ModelLimit`）。这为错误类型提供了天然的共享层：放 `model-spec-core` 可免去新建 crate，`agent-core` 直接消费结构化错误。

## 2. 目标与非目标

### 2.1 目标

- 一套**跨 provider 统一的错误分类模型**（`ErrorKind`），语义稳定、可序列化。
- 默认 OpenAI 协议解析 + 少量 provider 覆写，覆盖 177 家。
- 统一的重试决策（`RetryPolicy`）替换两套并行逻辑，尊重 `Retry-After`。
- 消费端（agent-core / TUI / 用户提示）能拿到结构化错误，产出人类可读提示。
- SSE 流内错误进入同一管道。

### 2.2 非目标

- 不做跨 provider 全量业务码表进代码（完整表保留在 Markdown 文档，代码只维护**影响重试/提示决策**的子集）。
- 不改变各 provider 的调用协议本身（OpenAI/Anthropic 请求仍各走各的）。
- 不重构 HTTP 传输层（`anureo-http-retry` 保留）。

- Google / Azure / Bedrock 的专有错误协议（共 6 个 provider，见文档 §5/§6）：**本轮不实现解析器**，暂保持现有错误消息路径，后续迭代追加。

## 3. 总体架构：三层归一化

```
传输层 (HTTP status + headers + body / SSE error 事件)
   │   每个 provider 一个解析器（默认 OpenAI 协议 + 覆写）
   ▼
语义层 (ProviderError { kind, code, message, user_message, retry_policy })
   │   纯数据，跨 crate 可序列化
   ▼
消费层 (agent-core 停止/重试/提示、TUI 展示、日志/审计)
```

### 3.1 语义枚举（核心，全 provider 共用）

```rust
// foundation/model-spec-core/src/error/kind.rs
pub enum ErrorKind {
    BadRequest,        // 400/422 参数错误
    AuthFailed,        // 401 Key 无效/过期/平台混用（kimi.com vs kimi.ai）
    Permission,        // 403 地区限制/未订阅
    Billing,           // 402 / 403 余额不足（智谱 1113、小米 402）
    NotFound,          // 404 模型/资源不存在
    ContentFilter,     // 421/451/content_filter 内容审核
    RateLimited,       // 429 限流（可退避重试）
    QuotaExhausted,    // 429 配额/额度耗尽（智谱 1308-1311、小米 Token Plan）
    Overloaded,        // 503/529 服务过载
    Server,            // 500/502/504 服务端错误
    RequestTooLarge,   // 413
    Unknown,
}
```

`QuotaExhausted` 与 `RateLimited` 分离是刻意设计：前者重试无效（需充值/等待重置），后者重试有效。

### 3.2 错误载体

```rust
// foundation/model-spec-core/src/error/mod.rs
pub struct ProviderError {
    pub provider_id: String,         // models.dev id，如 "zhipuai"
    pub kind: ErrorKind,
    pub status: u16,                 // 原始 HTTP 状态码（SSE 流内为 0）
    pub code: Option<String>,        // error.code / 业务码 / error_type 原始值
    pub message: String,             // 原始 message（用于审计）
    pub user_message: String,        // 人类可读提示（消费端直接展示）
    pub retry_policy: RetryPolicy,
    pub request_id: Option<String>,
    pub partial_tokens: bool,        // 仅 SSE 流内错误：是否已返回部分 token
}

pub enum RetryPolicy {
    Retry,                          // 429/500/502/503/504/529，走统一指数退避
    RetryAfter(u64),                // 尊重 Retry-After / retry-after 头，单位毫秒
    NoRetry { action: UserAction }, // 重试无效，给用户明确动作
}

pub enum UserAction {
    CheckApiKey,        // 401 → "API Key 无效或已过期，请检查凭据"
    CheckPermission,    // 403 → "当前账号无权限（地区限制/未开通）"
    TopUp,              // 402/余额 → "账户余额不足，请充值后重试"
    AdjustContent,      // 421/451 → "请求或输出触发内容审核，请修改内容"
    WaitQuotaReset,     // QuotaExhausted → "周/月配额已耗尽，请等待重置或升级套餐"
    ChangeModel,        // NotFound → "模型不存在或不可用，请检查模型名"
    None,
}
```

`user_message` 的生成策略：解析器只产出 `ErrorKind` + `UserAction` + 原始 `message`；消费者端维护一个以 `ErrorKind` 为键的本地化消息表来填充 `user_message`（中文/英文），避免各覆写解析器硬编码文案。

`ProviderErrorParser` trait 定义（放 `model-spec-core`，仅 serde，无 HTTP 类型依赖）：

```rust
// foundation/model-spec-core/src/error/parse.rs
pub trait ProviderErrorParser: Send + Sync {
    fn parse(&self, status: u16, headers: &[(String, String)], body: &[u8]) -> ProviderError;
}
```

`decide()` 先做 `HeaderMap → Vec<(String,String)>` 转换，再调 `parser.parse()`。

`ErrorKind` → 默认 `RetryPolicy` 映射（`decide()` 只在检测到 `Retry-After` 头时覆写为 `RetryAfter`）：

| ErrorKind | 默认 RetryPolicy |
|---|---|
| Server / Overloaded / RateLimited | `Retry` |
| 其余全部 | `NoRetry { action }`（action = kind → UserAction 映射，见 §3.2 枚举注释） |

### 3.3 Provider 解析器

**默认解析器（覆盖 146 家 OpenAI 兼容）**：一张 `status → ErrorKind` 映射 + 一张 `error.type/code 词汇 → ErrorKind` 映射。

```rust
// foundation/llm/src/error/provider/openai.rs
pub struct OpenAiCompatParser;      // 默认
```

映射规则（与状态码文档 §2/§7/§8 对齐）：

| 来源 | 值 | → ErrorKind |
|---|---|---|
| HTTP | 400 / 422 | BadRequest |
| HTTP | 401 | AuthFailed |
| HTTP | 402 | Billing |
| HTTP | 403 | Permission |
| HTTP | 404 | NotFound |
| HTTP | 413 | RequestTooLarge |
| HTTP | 429 | RateLimited / QuotaExhausted* |
| HTTP | 500 / 502 / 504 | Server |
| HTTP | 503 / 529 | Overloaded |
| error.type | `content_filter` / `content_filter_error` | ContentFilter |
| error.type | `invalid_authentication_error` / `incorrect_api_key_error` | AuthFailed |
| error.type | `engine_overloaded_error` | Overloaded |
| error.type | `exceeded_current_quota_error` | Billing |
| error.type | `rate_limit_reached_error` | RateLimited |
| error.type | `insufficient_quota` | QuotaExhausted |

\* 429 的细分（RateLimited vs QuotaExhausted）由 `code`/`error.type`/`message` 决定；均无匹配时默认 RateLimited。

**Provider 覆写注册表**（混合方案：默认解析器 + 特殊 provider 覆写）：

```rust
// foundation/llm/src/error/provider/registry.rs
pub fn parser_for(provider_id: &str) -> Box<dyn ProviderErrorParser> {
    match provider_id {
        "zhipuai" | "zai" | ... => Box::new(ZhipuParser),
        "minimax" | "minimax-cn" | ... => Box::new(MinimaxParser),
        "xiaomi" | ... => Box::new(XiaomiParser),
        "stepfun" | ... => Box::new(StepFunParser),
        "moonshotai" | ... => Box::new(MoonshotParser),
        "openrouter" => Box::new(OpenRouterParser),
        "longcat" => Box::new(LongCatParser),
        _ => Box::new(OpenAiCompatParser),
    }
}
```

每个覆写解析器只负责差异点（其余走默认逻辑）：

| 覆写 | 差异 |
|---|---|
| `XiaomiParser` | HTTP **421** → ContentFilter；402 → Billing |
| `StepFunParser` | HTTP **451** → ContentFilter；402 → Billing |
| `ZhipuParser` | 业务码 1000/1001/1003/1005→AuthFailed、1113→Billing、1210-1215→BadRequest、1302-1311→QuotaExhausted（**含 1310**） |
| `MiniMaxParser` | 业务码 1004→AuthFailed、1008→Billing、1026/1027→ContentFilter、1002→RateLimited |
| `MoonshotParser` | error.type `invalid_authentication_error`/`incorrect_api_key_error`→AuthFailed；提示平台 Key 隔离 |
| `OpenRouterParser` | `error_type` 词汇优先于 HTTP 码；SSE 内错误携带 `provider_code` |
| `LongCatParser` | 402→Billing（失败不计费规则由消费者侧提示） |


**Anthropic 协议默认解析器（覆盖 9 个 provider）**：错误体 `{"type":"error","error":{"type":"...","message":"..."}}`，解析器 `foundation/llm/src/error/provider/anthropic.rs`。

| 来源 | 值 | → ErrorKind |
|---|---|---|
| HTTP | 400 | BadRequest |
| HTTP | 401 | AuthFailed |
| HTTP | 402 | Billing |
| HTTP | 403 | Permission |
| HTTP | 404 | NotFound |
| HTTP | 409 | BadRequest |
| HTTP | 413 | RequestTooLarge |
| HTTP | 429 | RateLimited |
| HTTP | 500 | Server |
| HTTP | 504 | Server |
| HTTP | 529 | Overloaded |
| error.type | `invalid_request_error` | BadRequest |
| error.type | `authentication_error` | AuthFailed |
| error.type | `billing_error` | Billing |
| error.type | `permission_error` | Permission |
| error.type | `not_found_error` | NotFound |
| error.type | `request_too_large` | RequestTooLarge |
| error.type | `rate_limit_error` | RateLimited |
| error.type | `api_error` | Server |
| error.type | `timeout_error` | Server |
| error.type | `overloaded_error` | Overloaded |

`parser_for()` 根据 provider 的 `npm` 字段区分：`@ai-sdk/anthropic` 的 9 家用 Anthropic 解析器；但 `minimax`（Anthropic 包名但自有业务码）和 `xiaomi`（双协议）走各自的覆写。

### 3.4 统一重试决策

`RetryPolicy` 的产生集中到一个函数，替换 `is_retryable_status_for` 与字符串分类器：

```rust
// foundation/llm/src/error/decide.rs
pub fn decide(parser: &dyn ProviderErrorParser, status: u16, headers: &HeaderMap, body: &[u8]) -> ProviderError
```

- 解析 `Retry-After` / `retry-after` / `X-RateLimit-Reset` → `RetryPolicy::RetryAfter`。
- 其余可重试 kind → `RetryPolicy::Retry`。
- 不可重试 kind → `RetryPolicy::NoRetry { action }`。

**传输层与应用层协作**：`anureo-http-retry` 处理 reqwest 传输错误（连接/超时/TLS，不产生 HTTP 响应），走 `LlmError::InvokeFailed` 路径；`decide()` 仅在**收到 HTTP 响应**后才调用，负责结构化错误的分类。传输层重试次数与退避策略不变，应用层重试次数由 `RetryLlmClient` 的 `max_application_retries` 控制（当前 5 次），仅 `retry_policy` 为 `Retry` 或 `RetryAfter` 的 `ProviderError` 才触发应用层重试。

- 重试计数与退避仍由 `anureo-http-retry` 与 `RetryLlmClient` 负责，但**是否重试**改为读 `retry_policy`。

## 4. 代码落点

```
foundation/
  model-spec-core/               # 类型层（纯数据，仅 serde）
    src/
      error/
        mod.rs                   # ProviderError, RetryPolicy, UserAction
        kind.rs                  # ErrorKind
        parse.rs                 # ProviderErrorParser trait（无 HTTP 依赖）
  llm/
    src/
      error.rs                   # LlmError 增加 ProviderError 载体（保持 InvokeFailed 兼容）或迁移
      error/provider/
        mod.rs
        openai.rs                # 默认解析器
        openai_native.rs         # async_openai 路径适配
        anthropic.rs             # 529 overloaded_error → Overloaded
        zhipu.rs
        minimax.rs
        moonshot.rs
        xiaomi.rs
        stepfun.rs
        openrouter.rs
        longcat.rs
        registry.rs              # parser_for()
      client/
        openai_compat/
          llm_client.rs          # send_with_retry 改用 decide()
          retry.rs               # 删除 is_retryable_status_for，保留 format_api_error_body 迁移
      support/
        error_classifier/        # 已收敛：仅保留网络错误判定（async_openai 路径用），bigmodel/minimax 业务码已删除
```

**决策记录（D1）**：错误类型放 `model-spec-core::error`（models_dev 同 crate）。依据：`agent-core` 与 `llm` 均已依赖 `model-spec-core`（§1.3），无需新建 crate；`status` 用 `u16`，类型层仅 serde 依赖。运行时解析器（`decide()`、provider 覆写）留在 `llm`。

## 5. agent-core 消费

```rust
// agent/agent-core/src/runner_error.rs（改造）
use model_spec_core::error::ProviderError;

pub enum RunnerError {
    // ...现有变体
    Llm(ProviderError),   // 新增：携带结构化错误
}
```

- `RetryLlmClient` 重试耗尽后返回 `ProviderError`，`GraphError` 不再吞掉结构化信息。
- 消费端按 `retry_policy.action` 产出用户可读提示（§3.2 的 `UserAction` 文案），不再透出原始 JSON。
- 可选：Billing / QuotaExhausted 触发 agent 停止并提示，而非继续空转重试。

## 6. SSE 流内错误

- OpenAI / Anthropic / OpenRouter 的 SSE error 事件（HTTP 200 后）统一反序列化为 `ProviderError`：
  - OpenAI 系：`data: {..., "error": {"message","type","code"}}`
  - Anthropic 系：`error: {"type","message"}`
  - OpenRouter：`data: {..., "error": {"code","message","metadata":{"error_type","provider_code"}}}`
- 与 HTTP 层共用 `decide()`，`status=0` 表示流内错误，kind 由 `error.type`/`error_type` 词汇决定。`partial_tokens` 在反序列化时由 SSE 解析器置位（如果已收到至少一个 content delta 事件），消费端据此决定是否保留已输出的文本。

**决策记录（D2）**：SSE 流内错误本轮纳入统一管道。✅ 推荐。

## 7. 迁移路径

1. **在 `model-spec-core` 建 `error` module**：`ErrorKind` / `ProviderError` / `RetryPolicy` / `UserAction` + `ProviderErrorParser` trait（仅 serde 依赖）+ 单元测试。
2. **实现默认解析器**：`OpenAiCompatParser` + `decide()`（含 Retry-After 解析），覆盖 429/401/402/403/404/413/5xx + 常见 `error.type`。
3. **接入 `ChatOpenAICompat`**：`send_with_retry` 改用 `decide()`，`is_retryable_status_for` 删除；用真实 provider 列表 + 文档 §8 写集成测试（含智谱 1310、小米 421、StepFun 451）。
4. **接入 `ChatOpenAI`（async_openai）**：错误消息 → `OpenAiCompatParser`，删除字符串分类器路径。
5. **实现 6-8 个覆写解析器**（§3.3 表），覆盖文档 §8 中"影响决策"的差异。
6. **SSE 错误接入**。
7. **agent-core 消费**：`RunnerError::Llm(ProviderError)` + 用户提示。
8. 回归测试：全部 `LlmError::InvokeFailed` 调用点确认可读。

**`LlmError` 迁移方向**：先新增 `Structured(ProviderError)` 变体（不影响现有调用点），`decide()` 返回 `ProviderError` 后由上层调用包装进此变体。逐步将 `InvokeFailed` 的调用点改走结构化路径。最终 `InvokeFailed` 仅保留传输层错误。

**分阶段实施**：

| 阶段 | 步骤 | 覆盖 |
|---|---|---|
| 一 | 步骤 1-3 | model-spec-core error module + OpenAI 默认解析器 + ChatOpenAICompat 接入 |
| 二 | 步骤 4-5 | ChatOpenAI（async_openai）+ Zhipu / Xiaomi / StepFun 三个最常用中国厂商覆写 |
| 三 | 步骤 6-7 | Anthropic 默认解析器 + SSE 错误 + agent-core 消费 |
| 四 | 后续 | 其余覆写（Moonshot / OpenRouter / LongCat / MiniMax）+ Google / Azure / Bedrock 解析器 |

**不迁移**：`anureo-http-retry` 传输层、`RetryLlmClient` 的退避逻辑本身。

## 8. 决策记录

| # | 决策点 | 选项 | 推荐 |
|---|---|---|---|
| D1 | 错误类型放哪 | A) `foundation/llm` 内；B) 独立 `foundation/llm-error`；C) `model-spec-core` 的 `error` module（models_dev 同层） | C（agent-core 与 llm 均已依赖 model-spec-core，无需新 crate） |
| D2 | SSE 流内错误 | A) 本轮纳入；B) 后续迭代 | A |
| D3 | provider 差异表达 | A) 纯代码 trait；B) 数据驱动注册表；C) 默认 OpenAI + 覆写 | C（177 家覆盖率最高、代码量最小） |
| D4 | 业务码进代码范围 | A) 全量；B) 只进影响重试/提示的子集 | B（完整表保留在状态码文档） |

## 9. 关联文档

- `docs/analysis/models-dev-provider-status-codes.md` —— 177 个 provider 状态码全量参考（本设计的业务依据）
- `foundation/llm/src/client/openai_compat/retry.rs` —— 待删除/迁移的现状逻辑
- `foundation/llm/src/support/error_classifier/` —— 已收敛：仅保留网络错误判定（业务码分类迁移至 `error::provider`）

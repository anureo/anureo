# 模型提供商 API 状态码统一参考

> 来源：models.dev `/api.json` 快照（2025-08-19，共 177 个 provider）
> 目的：把各模型提供商 API 文档中的 HTTP 状态码 / 业务错误码汇总为一份统一参考，
> 用于 anureo 多 provider 场景下的错误分类、重试策略与用户可读错误消息。

## 1. 总览：177 个 provider 按 API 风格分类

| API 风格 | provider 数 | 判定依据（models.dev npm 字段） | 错误协议章节 |
|---|---|---|---|
| OpenAI 兼容 | 140 | `@ai-sdk/openai-compatible` | §2 |
| OpenAI 原生 | 4 | `@ai-sdk/openai` | §2 |
| Anthropic 兼容 | 9 | `@ai-sdk/anthropic` | §3 |
| Azure OpenAI | 2 | `@ai-sdk/azure` | §4 |
| Google / Vertex | 3 | `@ai-sdk/google` / `@ai-sdk/google-vertex` | §5 |
| Amazon Bedrock | 1 | `@ai-sdk/amazon-bedrock` | §6 |
| 其它自定义 SDK | 18 | 其它 SDK 包 | §7 |

**关键结论**：177 个 provider 中绝大多数（140 OpenAI 兼容 + 4 OpenAI 原生，以及 18 个"其它"中大部分
也提供 OpenAI 兼容端点）遵循 **OpenAI 错误协议**；9 个遵循 **Anthropic 错误协议**。
因此统一文档的核心是两张主表（§2 / §3），其余为专有协议与偏差明细。
中国厂商（24 家母公司 / 46 个 provider id）的明细集中在 §8。

## 2. OpenAI 错误协议（OpenAI 兼容系主表）

### 2.1 统一错误响应体

```json
{
  "error": {
    "message": "人类可读的错误描述",
    "type": "invalid_request_error",
    "param": null,
    "code": null
  }
}
```

- `type` 是稳定错误类别，客户端判断逻辑应**优先用 `type` 而非 HTTP 状态码**。
- `code` 仅在部分错误出现，多为计费/配额类：
  `insufficient_quota`、`credit_balance_exhausted`、`organization_spend_limit_exceeded`、
  `project_spend_limit_exceeded`、`organization_usage_limit_exceeded`。
- 部分实现（DeepSeek、Moonshot 等）额外返回 `request_id` / `req_id` 字段。

### 2.2 HTTP 状态码主表

| HTTP | error.type | 含义 | 可重试 |
|---|---|---|---|
| 400 | `invalid_request_error` | 请求格式/参数错误 | 否 |
| 401 | `authentication_error` | 认证失败 / API key 无效 / IP 未授权 | 否 |
| 402 | `insufficient_quota` | 余额不足（DeepSeek、OpenRouter 等） | 否（需充值） |
| 403 | `permission_error` | 无权限（含地区限制、内容过滤命中） | 否 |
| 404 | `not_found_error` | 模型/资源/端点不存在 | 否 |
| 409 | `conflict_error` | 资源状态冲突 | 否 |
| 413 | `request_too_large` | 请求体超过大小上限 | 否（需削减） |
| 422 | `invalid_request_error` | 参数校验失败（DeepSeek、Moonshot 等） | 否 |
| 429 | `rate_limit_error` / `insufficient_quota` | 限流或配额/额度耗尽 | 是（尊重 `Retry-After`） |
| 500 | `api_error` | 服务端内部错误 | 是（退避） |
| 502 | `api_error` | 网关错误 | 是 |
| 503 | `api_error` / `overloaded_error` | 服务过载 / 引擎繁忙 | 是 |
| 504 | `timeout_error` | 网关/上游超时 | 是 |

> 注意：DeepSeek 官方明确**不使用 404**；OpenAI 兼容系内部对 400/422、500/502/503 的划分略有差异。

### 2.3 OpenAI 官方错误明细（2025-08 文档）

| HTTP | 场景 | 附加 code |
|---|---|---|
| 401 | Invalid Authentication | — |
| 401 | Incorrect API key provided | — |
| 401 | Must be a member of an organization | — |
| 401 | IP not authorized | — |
| 403 | Country, region, or territory not supported | — |
| 429 | Credit balance exhausted | `credit_balance_exhausted` |
| 429 | Rate limit reached for requests | — |
| 429 | Organization spend limit reached | `organization_spend_limit_exceeded` |
| 429 | Project spend limit reached | `project_spend_limit_exceeded` |
| 429 | Organization usage limit reached | `organization_usage_limit_exceeded` |
| 500 | Server error | — |
| 503 | Engine currently overloaded | — |
| 503 | Slow Down（突发请求速率过高） | — |

WebSocket 模式（Responses API）额外错误：`previous_response_not_found`、`websocket_connection_limit_reached`。

### 2.4 通用 Header 与流式错误

- Header：`x-request-id` / `request-id`、`retry-after`、`x-ratelimit-*`。
- SSE 流式场景：HTTP 200 之后发生的错误通过流内 `data: {..., "error": {...}}` 事件下发，
  **HTTP 状态码保持 200**，不能只看状态码。

## 3. Anthropic 错误协议（9 个 provider）

| HTTP | error.type | 含义 |
|---|---|---|
| 400 | `invalid_request_error` | 请求格式/内容问题（也用于其它未列出的 4xx） |
| 401 | `authentication_error` | API key 无效/过期/吊销；AWS 场景含 SigV4 问题 |
| 402 | `billing_error` | 计费/支付信息问题 |
| 403 | `permission_error` | 无权访问指定资源 |
| 404 | `not_found_error` | 资源不存在 |
| 409 | `conflict_error` | 状态冲突（并发修改、唯一值冲突） |
| 413 | `request_too_large` | 超过请求体上限（Messages/Tokens 32MB，Batch 256MB，Files 500MB） |
| 429 | `rate_limit_error` | 限流；罕见场景为组织加速限制 |
| 500 | `api_error` | 服务端内部错误 |
| 504 | `timeout_error` | 处理超时 |
| 529 | `overloaded_error` | API 临时过载（高流量） |

错误响应体：

```json
{
  "type": "error",
  "error": { "type": "not_found_error", "message": "..." },
  "request_id": "req_..."
}
```

- Header：`request-id`、`anthropic-ratelimit-*`、`retry-after`。
- 413 特殊：直连 Claude API 时由 Cloudflare 在到达 API 前拦截返回。
- 官方 SDK 默认对瞬时错误（连接、429、5xx）指数退避重试 2 次。
- SSE 流内错误通过 error 事件下发，不占用 HTTP 状态码。

## 4. Azure OpenAI（2 个 provider）

错误协议与 OpenAI 相同（同一 error schema），附加注意点：

- **deployment 不存在** → 404 或 403（区别于 OpenAI 的 404/400）。
- **内容过滤命中** → 400，`code: content_filter`。
- 认证失败 → 401 `invalid-api-key`；无权访问 → 403 `access_denied`。
- 限流 → 429，带 `Retry-After`。
- 503 常见于区域过载，可重试。

## 5. Google / Vertex（Gemini，3 个 provider）

REST 错误体（gRPC 状态映射）：

```json
{
  "error": {
    "code": 400,
    "message": "API key not valid. Please pass a valid API key.",
    "status": "INVALID_ARGUMENT",
    "details": [ { "@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "API_KEY_INVALID" } ]
  }
}
```

| HTTP | status（gRPC 枚举） |
|---|---|
| 400 | `INVALID_ARGUMENT`、`FAILED_PRECONDITION`、`OUT_OF_RANGE` |
| 401 | `UNAUTHENTICATED` |
| 403 | `PERMISSION_DENIED` |
| 404 | `NOT_FOUND` |
| 409 | `ABORTED`、`ALREADY_EXISTS`、`CONFLICT` |
| 429 | `RESOURCE_EXHAUSTED` |
| 499 | `CANCELLED` |
| 500 | `INTERNAL`、`DATA_LOSS`、`UNKNOWN` |
| 501 | `UNIMPLEMENTED` |
| 503 | `UNAVAILABLE` |
| 504 | `DEADLINE_EXCEEDED` |

新 Gemini API（Interactions）改用更简单的 code 词汇表：
`invalid_request`(400)、`parameter_unknown`(400)、`authentication_*`(401)、`permission_*`(403)、
`not_found`(404)、`rate_limit_exceeded`(429)、`quota_exceeded`(429)、`api_error`(500)。
流式请求中错误以 `event_type: "error"` 事件下发。

## 6. Amazon Bedrock（1 个 provider）

非 OpenAI 协议，错误以异常名返回（Converse API，SDK 层处理为主）：

| HTTP | 异常名 |
|---|---|
| 400 | `ValidationException` |
| 403 | `AccessDeniedException` |
| 404 | `ResourceNotFoundException` |
| 408 | `ModelTimeoutException` |
| 409 | `ModelNotReadyException` |
| 429 | `ThrottlingException` |
| 424 | `ModelErrorException` |
| 500 | `InternalServerException` |
| 503 | `ServiceUnavailableException` |
| 200 流内 | `ModelStreamErrorException` |

## 7. 其它海外 provider 状态码明细

### 7.1 OpenRouter（网关 + typed error_type）

响应体：

```json
{
  "error": {
    "code": 429,
    "message": "Rate limit exceeded",
    "metadata": { "error_type": "rate_limit_exceeded", "provider_code": "rate_limited" }
  }
}
```

| HTTP | error_type | 含义 |
|---|---|---|
| 400 | `invalid_request` / CORS | 参数错误或跨域 |
| 401 | `authentication` | 凭据无效 |
| 402 | `insufficient_quota` | 账户或 key 余额不足 |
| 403 | `permission` / guardrail / 审查 | 无权访问或内容被拒 |
| 408 | `timeout` | 请求超时 |
| 429 | `rate_limit_exceeded` | 限流 |
| 502 | `bad_gateway` / 模型不可用 | 上游响应无效 |
| 503 | `provider_failed` | 无满足路由要求的可用 provider |

- HTTP 状态码与 `error.code` 一致；**但流式场景下错误可能发生在 HTTP 200 之后**（body / SSE 内）。
- `error_type` 是 OpenRouter 把上游错误归一化后的稳定词汇（跨 Chat/Responses/Anthropic 格式一致），`metadata.provider_code` 保留上游原始错误码。应优先依赖 `error_type`。
- 429 / 503 可能带 `Retry-After`；平台级限流错误带 `X-RateLimit-*` header。

### 7.2 其它知名海外 provider 速查

| provider | 协议 | 备注 |
|---|---|---|
| Groq | OpenAI 兼容 | 429 带 `Retry-After`，error.type 用 OpenAI 词汇 |
| Cerebras / xAI / Together / DeepInfra | OpenAI 兼容 | 标准 OpenAI 错误协议 |
| Mistral | OpenAI 兼容端点 | 400/401/404/429/500 |
| NVIDIA NIM / Fireworks / Novita | OpenAI 兼容 | 标准 OpenAI 错误协议 |
| Cohere | 原生 API | `{"message": ...}`；400/401/403/404/429/500/503 |
| GitHub Copilot / GitHub Models | OpenAI 兼容 | 429 限流、404 模型不在 plan |
| Cloudflare Workers AI / AI Gateway | OpenAI 兼容 /v1 | 400/401/403/429/500；网关透传上游错误 |
| Vercel AI Gateway | 代理层 | 透传上游错误 + 网关自身 401/429 |
| AIHubMix / AnyAPI 等聚合站 | OpenAI 兼容 | 标准 OpenAI 错误协议 |

## 8. 中国供应商错误码明细（24 家 / 46 个 provider id）

> 数据来源：各厂商官方错误码文档（2025-08 检索）。以下每家厂商均给出独立的 HTTP 状态码表与业务错误码表。

### 8.1 DeepSeek（deepseek）

- provider id：`deepseek`｜协议：OpenAI 兼容｜Base URL：`https://api.deepseek.com`

| HTTP | error.type | 含义 | 可重试 |
|---|---|---|---|
| 400 | `invalid_request_error` | 参数格式错误 | 否 |
| 401 | `authentication_error` | API Key 无效 | 否 |
| 402 | `insufficient_quota` | 余额不足（预付费耗尽） | 否（需充值） |
| 422 | `invalid_request_error` | 参数校验失败 | 否 |
| 429 | `rate_limit_error` | 动态并发限流（按服务器负载调整） | 是（退避） |
| 500 | `api_error` | 服务端内部错误 | 是 |
| 503 | `overloaded_error` | Server Overloaded | 是 |

- 官方明确**不使用 404**。
- 429 为动态并发限流，收到后应降低并发并退避。

### 8.2 智谱 Zhipu AI / Z.AI（zhipuai / zhipuai-coding-plan / zai / zai-coding-plan）

- 协议：OpenAI 兼容｜Base URL：`https://open.bigmodel.cn/api/paas/v4`（国际站 `https://api.z.ai/api/paas/v4`）

| HTTP | error.type | 含义 |
|---|---|---|
| 400 | `invalid_request_error` | 参数错误 / 内容审查命中 |
| 401 | `authentication_error` | 身份验证失败 |
| 403 | `permission_error` | 无权限 |
| 404 | `not_found_error` | 资源不存在 |
| 429 | `rate_limit_error` | 限流 / 欠费 / 套餐配额耗尽 |
| 500 | `api_error` | API 调用失败 |

业务错误码（响应体 `{"error": {"code": "1001", "message": "..."}}`）：

| 业务码 | HTTP | 含义 |
|---|---|---|
| 1000 | 401 | 身份验证失败 |
| 1001 | 401 | Header 中未收到 Authentication |
| 1003 | 401 | Token 已过期 |
| 1005 | 401 | 需要二次认证 |
| 1113 | 429 | 账户欠费 |
| 1200 | 500 | API 调用失败 |
| 1210 | 400 | 参数有误 |
| 1211 | 400 | 模型不存在 |
| 1212 | 400 | 当前模型不支持该调用方式 |
| 1213 | 400 | 缺少必填字段 |
| 1214 | 400 | 字段非法 |
| 1215 | 400 | 字段互斥 |
| 1302 | 429 | 速率限制 |
| 1305 | 429 | 模型访问量过大 |
| 1308 | 429 | 使用量上限（限额到期重置） |
| 1309 | 429 | Coding Plan 套餐到期 |
| 1310 | 429 | 周/月限额耗尽 |
| 1311 | 429 | 订阅未包含该模型 |
| - | 400 | 输入或输出触发敏感内容审查 |

### 8.3 月之暗面 Moonshot / Kimi（moonshotai / moonshotai-cn / kimi-for-coding）

- 协议：OpenAI 兼容｜Base URL：`https://api.moonshot.ai/v1`（中国站 `https://api.moonshot.cn/v1`，Kimi For Coding `https://api.kimi.com/coding/v1`）

| HTTP | error.type | 含义 |
|---|---|---|
| 400 | `content_filter` | 输入或输出触发内容安全审查 |
| 400 | `invalid_request_error` | 参数格式错误、token 超限、文件超限 |
| 401 | `invalid_authentication_error` | API Key 无效 |
| 401 | `incorrect_api_key_error` | API Key 不正确 |
| 403 | `permission_error` | 无权限 |
| 404 | `not_found_error` | 资源不存在 |
| 429 | `engine_overloaded_error` | 服务节点过载（按 `Retry-After` 退避重试） |
| 429 | `exceeded_current_quota_error` | 账户欠费 / token 额度不足 |
| 429 | `rate_limit_reached_error` | 超过 RPM / TPM / TPD 限制 |
| 500 | `api_error` | 服务端内部错误 |

- 响应体：`{"error": {"type": "content_filter", "message": "..."}}`，部分接口带 `request_id`。
- **平台 Key 隔离**：`platform.kimi.com`（中国站）与 `platform.kimi.ai`（国际站）账户、余额、API Key 完全独立，混用返回 401。
- 429 中 `engine_overloaded_error` 由服务端容量导致，充值或升级 Tier 不能消除；`exceeded_current_quota_error` 需充值。

### 8.4 MiniMax（minimax / minimax-cn / minimax-coding-plan / minimax-cn-coding-plan）

- 协议：Anthropic 兼容端点｜Base URL：`https://api.minimax.io/anthropic/v1`（中国站 `https://api.minimaxi.com/anthropic/v1`）

| HTTP | error.type | 含义 |
|---|---|---|
| 400 | `invalid_request_error` | 参数错误 |
| 401 | `authentication_error` | 未授权 / Token 不匹配 |
| 403 | `permission_error` | 无权限 |
| 429 | `rate_limit_error` | 频率超限 |
| 500 | `api_error` | 服务端内部错误 |

业务错误码：

| 业务码 | 含义 |
|---|---|
| 1000 | 未知错误 |
| 1001 | 请求超时 |
| 1002 | 频率超限 |
| 1004 | 未授权 / Token 不匹配 |
| 1008 | 余额不足 |
| 1024 | 内部错误 |
| 1026 | 输入内容涉敏 |
| 1027 | 输出内容涉敏 |
| 1033 | 系统错误 |
| 1039 | Token 限制 |
| 1041 | 连接数限制 |
| 1042 | 非法字符 |
| 2013 | 参数错误 |
| 2049 | 非法 URL |

- Header：`trace_id`、`X-RateLimit-*`。

### 8.5 阶跃星辰 StepFun（stepfun / stepfun-ai / stepfun-step-plan / stepfun-ai-step-plan）

- 协议：OpenAI 兼容｜Base URL：`https://api.stepfun.com/v1`（国际站 `https://api.stepfun.ai/v1`）

| HTTP | error.type | 含义 | 可重试 |
|---|---|---|---|
| 400 | `invalid_request_error` | 参数错误 | 否 |
| 401 | `authentication_error` | 认证失败 | 否 |
| 402 | `insufficient_quota` | 余额不足 | 否（需充值） |
| 404 | `not_found_error` | 路径不存在 | 否 |
| 429 | `rate_limit_error` | 限流 | 是 |
| 451 | `content_filter` | 内容审核未通过 | 否 |
| 500 | `api_error` | 服务端内部错误 | 是 |
| 503 | `overloaded_error` | 服务过载 | 是 |
| 504 | `timeout_error` | 网关超时 | 是 |

- Header：`X-Trace-Id`（问题排查用）。

### 8.6 阿里百炼 DashScope（alibaba / alibaba-cn / alibaba-coding-plan / alibaba-coding-plan-cn / alibaba-token-plan / alibaba-token-plan-cn）

- 协议：OpenAI 兼容｜Base URL：`https://dashscope.aliyuncs.com/compatible-mode/v1`（国际站 `https://dashscope-intl.aliyuncs.com/compatible-mode/v1`）

| HTTP | error.type / code | 含义 |
|---|---|---|
| 400 | `invalid_parameter_error` | 参数错误；**模型未在模型市场开通时同样返回此码**（提示 "The product is not activated"） |
| 401 | `InvalidApiKey` / `invalid access token` | 无 Key / Key 无效 / Token 过期 / 误用其它计费方式的 Base URL |
| 404 | — | model not found（模型名称拼写错误或不在支持列表） |
| 429 | `Requests rate limit exceeded` | RPM 限流（请求过于密集） |
| 429 | `Allocated quota exceeded` / `Throttling.AllocationQuota` | TPM / 月度配额耗尽 |
| 500 | — | 服务端错误 |

- 模态生成模型（图像/视频）使用独立接口，不能通过文本模型 Base URL 调用（400）。
- 按量付费 / Token Plan / Coding Plan 的 API Key 与 Base URL 相互独立，混用返回 401。

### 8.7 魔搭 ModelScope（modelscope）

- 协议：OpenAI 兼容｜Base URL：`https://api-inference.modelscope.cn/v1`

| HTTP | error.type | 含义 |
|---|---|---|
| 400 | `invalid_request_error` | 参数错误 |
| 401 | `authentication_error` | 认证失败 |
| 403 | `permission_error` | 无权限 |
| 404 | `not_found_error` | 模型/资源不存在 |
| 408 | `timeout_error` | 请求超时 |
| 429 | `rate_limit_error` | 限流 / 配额耗尽 |
| 500 | `api_error` | 服务端错误 |
| 503 | `overloaded_error` | 服务不可用 |

- 透传上游 provider 业务码（如智谱 `1210`）。

### 8.8 腾讯云 LKEAP / 混元（tencent-tokenhub / tencent-token-plan / tencent-coding-plan）

- 协议：OpenAI 兼容（v3）+ 腾讯云 API｜Base URL：`https://tokenhub.tencentmaas.com/v1`、`https://api.lkeap.cloud.tencent.com/plan/v3`、`https://api.lkeap.cloud.tencent.com/coding/v3`

| HTTP | 场景 |
|---|---|
| 400 | 参数错误 |
| 401 | 认证失败 / Key 无效 / Token 过期 |
| 403 | 无权限 |
| 404 | 模型不存在 |
| 429 | 限流（OpenAI 兼容端点提示 `concurrency exceeded`） |
| 500 | 服务端错误 |

腾讯云 API 错误码（OpenAI 兼容端点之外的云 API 体系）：

| 错误码 | 含义 |
|---|---|
| `AuthFailure.*` | 签名错误 / 密钥无效 / 凭据过期 |
| `MissingParameter` | 缺少必填参数 |
| `LimitExceeded` | 超过配额 |
| `RequestLimitExceeded.*` | 请求频控超限 |
| `ServiceUnavailable` | 服务不可用 |
| `UnauthorizedOperation` | 未授权操作 |
| `UnknownParameter` | 未知参数 |
| `FailedOperation.UserUnAuthError` | 用户未认证 |

### 8.9 小米 MiMo（xiaomi / xiaomi-token-plan-cn / xiaomi-token-plan-ams / xiaomi-token-plan-sgp）

- 协议：OpenAI + Anthropic 兼容｜Base URL：`https://api.xiaomimimo.com/v1`

| HTTP | 错误码名 | 含义 |
|---|---|---|
| 400 | 格式错误 | 请求体 JSON 格式错误 / 参数越界 / 模型不存在 / 多模态文件违规 |
| 401 | 认证失败 | Key 缺失或无效、Authorization 头格式错误；**混用 Token Plan 与按量付费 Key** |
| 402 | 余额不足 | 账户余额不足，需充值 |
| 403 | 拒绝访问 | 地区不支持 / Key 被风控 |
| 404 | 资源未找到 | 模型或接口不支持图像输入等 |
| 421 | 内容拦截 | 内容审核拦截 |
| 429 | 请求超限 | 请求过于频繁，或 Token Plan 额度耗尽 |
| 500 | 服务器失败 | 内部故障 |
| 503 | 服务器故障 | 负载过高 |

- Key 体系：按量付费 `sk-xxx`；Token Plan `tp-xxx`，**两者不可混用**（混用返回 401）。
- 限流按账号级 RPM / TPM 计算（同一模型下所有 Key 的请求合并计数）。
- 429 建议指数退避重试，Token Plan 额度耗尽需升级套餐或切换按量付费。

### 8.10 硅基流动 SiliconFlow（siliconflow / siliconflow-cn）

- 协议：OpenAI 兼容｜Base URL：`https://api.siliconflow.cn/v1`（国际站 `https://api.siliconflow.com/v1`）

| HTTP | error.type | 含义 |
|---|---|---|
| 400 | `invalid_request_error` | 参数错误 |
| 401 | `authentication_error` | API Key 无效 |
| 403 | `permission_error` | 无权限 |
| 429 | `rate_limit_error` | 限流 |
| 500 | `api_error` | 服务端错误 |
| 503 | `overloaded_error` | 服务过载 |
| 504 | `timeout_error` | 网关超时 |

- 业务码示例：`20012`（Model does not exist）。
- 429 细分维度：RPM（每分钟请求）/ RPD（每日请求）/ TPM（每分钟 Token）/ TPD（每日 Token）/ IPM / IPD。

### 8.11 302.AI（302ai）

- 协议：OpenAI 兼容｜Base URL：`https://api.302.ai/v1`

| HTTP | 含义 |
|---|---|
| 400 | 参数错误 |
| 401 | 认证失败 |
| 403 | 无权限 |
| 404 | 资源不存在 |
| 413 | 请求体过大 |
| 429 | 限流 |
| 500 | 服务端错误 |
| 503 | 服务不可用 |

- 错误体：`{code, message, request_id}`。
- 业务 code 词汇：`InvalidParameter` / `MissingParameter` / `Unauthorized` / `RateLimitExceeded` / `InsufficientBalance` / `InternalError`。

### 8.12 七牛 AI Token（qiniu-ai）

- 协议：OpenAI 兼容｜Base URL：`https://api.qnaigc.com/v1`

| HTTP | 含义 |
|---|---|
| 400 | 参数错误 |
| 401 | `Invalid API Key`（`authentication_error`） |
| 429 | `UID rate limit reached for TPD / RPM / TPM` |
| 500 | 服务端错误 |

### 8.13 心流 iFlow（iflowcn）

- 协议：OpenAI 兼容｜Base URL：`https://apis.iflow.cn/v1`

| HTTP | 含义 |
|---|---|
| 400 | 参数错误 |
| 401 | 认证失败 |
| 404 | 资源不存在 |
| 429 | 限流 |
| 503 | 服务不可用 |
| 504 | 网关超时 |

业务错误码：

| 业务码 | 含义 |
|---|---|
| 60400 | 积分不足 |
| 90402 | API Key 无效 |
| 40303 | 频率超限 |
| 90001 / 90002 | 搜索类错误 |

### 8.14 模力方舟 Moark（moark）

- 协议：OpenAI 兼容｜Base URL：`https://moark.com/v1`

| HTTP | 含义 |
|---|---|
| 400 | 业务类错误：服务不存在、免费额度用尽、token 未绑定资源包、资源包过期 |
| 401 | 认证失败 |
| 403 | 无权限 |
| 429 | 限流 |
| 500 | 服务端错误 |

- 400 之外的错误走标准 OpenAI 状态码。

### 8.15 摩尔线程 KUAE Cloud Coding Plan（kuae-cloud-coding-plan）

- 协议：兼容协议｜Base URL：`https://coding-plan-endpoint.kuaecloud.net/v1`

| HTTP | 含义 |
|---|---|
| 401 | `invalid access token` |
| 403 | 模型不支持 / 未订阅 / 订阅过期 |
| 429 | `Allocated Quota Exceeded` / hour / week / month allocated quota exceeded |
| 452 | 资源包不存在 |

### 8.16 接口AI Jiekou（jiekou）

- 协议：OpenAI + Anthropic 兼容｜Base URL：`https://api.jiekou.ai/openai`

| 错误名 | HTTP | 含义 |
|---|---|---|
| `INVALID_API_KEY` | 403 | API Key 无效 |
| `MODEL_NOT_FOUND` | 404 | 模型不存在 |
| `FAILED_TO_AUTH` | 401 | 认证失败 |
| `NOT_ENOUGH_BALANCE` | 403 | 余额不足 |
| `INVALID_REQUEST_BODY` | 400 | 请求体无效 |
| `RATE_LIMIT_EXCEEDED` | 429 | 限流 |
| `TOKEN_LIMIT_EXCEEDED` | 429 | Token 超限 |
| `SERVICE_NOT_AVAILABLE` | 503 | 服务不可用 |
| `ACCESS_DENY` | 403 | 拒绝访问 |

### 8.17 启航 AI QiHang（qihang-ai）

- 协议：OpenAI 兼容｜Base URL：`https://api.qhaigc.net/v1`

| HTTP | error.code | 含义 |
|---|---|---|
| 200 | — | 成功 |
| 400 | — | 参数错误 |
| 401 | `invalid_api_key` | API Key 无效 |
| 429 | — | 限流 |
| 500 | — | 服务端错误 |

### 8.18 蚂蚁百灵 Bailing（bailing）

- 协议：OpenAI 兼容｜Base URL：`https://api.tbox.cn/api/llm/v1/chat/completions`

| HTTP | 含义 |
|---|---|
| 400 | 参数错误 |
| 401 | 认证失败 |
| 403 | 无权限 |
| 429 | 限流（含日免费额度耗尽） |
| 500 | 服务端错误 |

- 无专门错误码文档，遵循 OpenAI 协议。

### 8.19 D.Run（drun）

- 协议：OpenAI 兼容｜Base URL：`https://chat.d.run/v1`

| HTTP | 含义 |
|---|---|
| 400 | 参数错误 |
| 401 | 认证失败 |
| 429 | 限流 |
| 500 | 服务端错误 |
| 503 | 服务不可用 |

- 无专门错误码文档，遵循 OpenAI 协议。

### 8.20 英博云 EBCloud（ebcloud）

- 协议：OpenAI 兼容｜Base URL：`https://maas-api.ebcloud.com/v1`

| HTTP | 含义 |
|---|---|
| 400 | 参数错误 |
| 401 | 认证失败 |
| 429 | 限流 |
| 500 | 服务端错误 |

- 无专门错误码文档，遵循 OpenAI 协议。

### 8.21 美团 LongCat（longcat）

- 协议：OpenAI 兼容｜Base URL：`https://api.longcat.chat/openai`

| HTTP | error.type / code | 含义 |
|---|---|---|
| 400 | `invalid_request_error` / `invalid_parameter` / `invalid_json` | 参数或 JSON 错误 |
| 401 | `authentication_error` / `invalid_api_key` | API Key 无效 |
| 402 | `insufficient_quota` | Token 额度不足 |
| 403 | `permission_error` / `insufficient_quota` | 无权限 |
| 429 | `rate_limit_error` / `rate_limit_exceeded` | 限流 |
| 500 | `server_error` / `internal_error` | 服务端错误 |
| 502 | — | 网关错误 |
| 503 | — | 服务不可用 |

- **失败响应不计费**：仅 HTTP 200 按实际 Token 计费，401/403/429/500 不扣费。

### 8.22 HPC-AI（hpc-ai）

- 协议：OpenAI 兼容｜Base URL：`https://api.hpc-ai.com/inference/v1`

| HTTP | 含义 |
|---|---|
| 400 | 参数错误 |
| 401 | 认证失败 |
| 429 | 限流 |
| 500 | 服务端错误 |

- 无专门错误码文档，遵循 OpenAI 协议。

### 8.23 DaoXE（daoxe）

- 协议：OpenAI + Anthropic 兼容｜Base URL：`https://daoxe.com/v1`

| HTTP | 含义 |
|---|---|
| 400 | 参数错误 |
| 401 | 认证失败 |
| 429 | 限流 |
| 500 | 服务端错误 |

- 无专门错误码文档，遵循 OpenAI / Anthropic 协议。

### 8.24 Vivgrid（vivgrid）

- 协议：OpenAI 原生｜Base URL：`https://api.vivgrid.com/v1`

| HTTP | error.type | 含义 |
|---|---|---|
| 400 | `invalid_request_error` | 参数错误 |
| 401 | `authentication_error` | 认证失败 |
| 403 | `permission_error` | 无权限 |
| 429 | `rate_limit_error` | 限流 |
| 500 | `api_error` | 服务端错误 |

- 无专门错误码文档，遵循 OpenAI 协议（错误体为标准 `{error: {message, type, param, code}}`）。
## 9. Provider 全量清单（177）

| # | id | name | API 风格 | base URL |
|---|---|---|---|---|
| 1 | 302ai | 302.AI | OpenAI 兼容 | https://api.302.ai/v1 |
| 2 | abacus | Abacus | OpenAI 兼容 | https://routellm.abacus.ai/v1 |
| 3 | abliteration-ai | abliteration.ai | OpenAI 兼容 | https://api.abliteration.ai/v1 |
| 4 | ai-router | AI-ROUTER | OpenAI 兼容 | https://api.ai-router.dev/v1 |
| 5 | aiand | ai& | OpenAI 兼容 | https://api.aiand.com/v1 |
| 6 | aihubmix | AIHubMix | 其它 | - |
| 7 | aki-io | AKI.IO | OpenAI 兼容 | https://aki.io/v1 |
| 8 | alibaba | Alibaba | OpenAI 兼容 | https://dashscope-intl.aliyuncs.com/compatible-mode/v1 |
| 9 | alibaba-cn | Alibaba (China) | OpenAI 兼容 | https://dashscope.aliyuncs.com/compatible-mode/v1 |
| 10 | alibaba-coding-plan | Alibaba Coding Plan | OpenAI 兼容 | https://coding-intl.dashscope.aliyuncs.com/v1 |
| 11 | alibaba-coding-plan-cn | Alibaba Coding Plan (China) | OpenAI 兼容 | https://coding.dashscope.aliyuncs.com/v1 |
| 12 | alibaba-token-plan | Alibaba Token Plan | OpenAI 兼容 | https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1 |
| 13 | alibaba-token-plan-cn | Alibaba Token Plan (China) | OpenAI 兼容 | https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1 |
| 14 | amazon-bedrock | Amazon Bedrock | Bedrock | - |
| 15 | ambient | Ambient | OpenAI 兼容 | https://api.ambient.xyz/v1 |
| 16 | anthropic | Anthropic | Anthropic | - |
| 17 | anyapi | AnyAPI | OpenAI 兼容 | https://api.anyapi.ai/v1 |
| 18 | atomic-chat | Atomic Chat | OpenAI 兼容 | http://127.0.0.1:1337/v1 |
| 19 | auriko | Auriko | OpenAI 兼容 | https://api.auriko.ai/v1 |
| 20 | azure | Azure | Azure | - |
| 21 | azure-cognitive-services | Azure Cognitive Services | Azure | - |
| 22 | bailing | Bailing | OpenAI 兼容 | https://api.tbox.cn/api/llm/v1/chat/completions |
| 23 | baseten | Baseten | OpenAI 兼容 | https://inference.baseten.co/v1 |
| 24 | berget | Berget.AI | OpenAI 兼容 | https://api.berget.ai/v1 |
| 25 | blueclaw | Blue Claw | OpenAI 兼容 | https://openai.blueclaw.network/v1 |
| 26 | cerebras | Cerebras | 其它 | - |
| 27 | chutes | Chutes | OpenAI 兼容 | https://llm.chutes.ai/v1 |
| 28 | clarifai | Clarifai | OpenAI 兼容 | https://api.clarifai.com/v2/ext/openai/v1 |
| 29 | claudinio | Claudinio | OpenAI 兼容 | https://api.claudin.io/v1 |
| 30 | cline-pass | ClinePass | OpenAI 兼容 | https://api.cline.bot/api/v1 |
| 31 | cloudferro-sherlock | CloudFerro Sherlock | OpenAI 兼容 | https://api-sherlock.cloudferro.com/openai/v1/ |
| 32 | cloudflare-ai-gateway | Cloudflare AI Gateway | 其它 | - |
| 33 | cloudflare-workers-ai | Cloudflare Workers AI | OpenAI 兼容 | https://api.cloudflare.com/client/v4/accounts/${CLOUDFLARE_ACCOUNT_ID}/ai/v1 |
| 34 | cohere | Cohere | 其它 | - |
| 35 | cortecs | Cortecs | OpenAI 兼容 | https://api.cortecs.ai/v1 |
| 36 | crof | CrofAI | OpenAI 兼容 | https://crof.ai/v1 |
| 37 | crossmodel | CrossModel | OpenAI 兼容 | https://api.crossmodel.ai/v1 |
| 38 | daoxe | DaoXE | OpenAI 兼容 | https://daoxe.com/v1 |
| 39 | databricks | Databricks | OpenAI 兼容 | https://${DATABRICKS_HOST}/ai-gateway/mlflow/v1 |
| 40 | deepinfra | Deep Infra | 其它 | - |
| 41 | deepseek | DeepSeek | OpenAI 兼容 | https://api.deepseek.com |
| 42 | digitalocean | DigitalOcean | OpenAI 兼容 | https://inference.do-ai.run/v1 |
| 43 | dinference | DInference | OpenAI 兼容 | https://api.dinference.com/v1 |
| 44 | drun | D.Run (China) | OpenAI 兼容 | https://chat.d.run/v1 |
| 45 | ebcloud | EBCloud | OpenAI 兼容 | https://maas-api.ebcloud.com/v1 |
| 46 | empiriolabs | EmpirioLabs AI | OpenAI 兼容 | https://api.empiriolabs.ai/v1 |
| 47 | evroc | evroc | OpenAI 兼容 | https://models.think.evroc.com/v1 |
| 48 | fastrouter | FastRouter | OpenAI 兼容 | https://go.fastrouter.ai/api/v1 |
| 49 | fireworks-ai | Fireworks AI | OpenAI 兼容 | https://api.fireworks.ai/inference/v1/ |
| 50 | freemodel | FreeModel | Anthropic | https://cc.freemodel.dev/v1 |
| 51 | friendli | Friendli | OpenAI 兼容 | https://api.friendli.ai/serverless/v1 |
| 52 | frogbot | FrogBot | OpenAI 兼容 | https://app.frogbot.ai/api/v1 |
| 53 | github-copilot | GitHub Copilot | OpenAI 兼容 | https://api.githubcopilot.com |
| 54 | github-models | GitHub Models | OpenAI 兼容 | https://models.github.ai/inference |
| 55 | gitlab | GitLab Duo | 其它 | - |
| 56 | gmicloud | GMI Cloud | OpenAI 兼容 | https://api.gmi-serving.com/v1 |
| 57 | google | Google | Google/Vertex | - |
| 58 | google-vertex | Vertex | Google/Vertex | - |
| 59 | google-vertex-anthropic | Vertex (Anthropic) | Google/Vertex | - |
| 60 | greenpt | GreenPT | OpenAI 兼容 | https://api.greenpt.ai/v1 |
| 61 | groq | Groq | 其它 | - |
| 62 | helicone | Helicone | OpenAI 兼容 | https://ai-gateway.helicone.ai/v1 |
| 63 | hetzner | Hetzner | OpenAI 兼容 | https://inference.hetzner.com/api/v1 |
| 64 | hpc-ai | HPC-AI | OpenAI 兼容 | https://api.hpc-ai.com/inference/v1 |
| 65 | huggingface | Hugging Face | OpenAI 兼容 | https://router.huggingface.co/v1 |
| 66 | hyper | Charm Hyper | OpenAI 兼容 | https://hyper.charm.land/v1 |
| 67 | iflowcn | iFlow | OpenAI 兼容 | https://apis.iflow.cn/v1 |
| 68 | inception | Inception | OpenAI 兼容 | https://api.inceptionlabs.ai/v1/ |
| 69 | inceptron | Inceptron | OpenAI 兼容 | https://api.inceptron.io/v1 |
| 70 | inference | Inference | OpenAI 兼容 | https://inference.net/v1 |
| 71 | inferx | InferX | OpenAI 兼容 | https://model.inferx.net/endpoints/v1 |
| 72 | io-net | IO.NET | OpenAI 兼容 | https://api.intelligence.io.solutions/api/v1 |
| 73 | jiekou | Jiekou.AI | OpenAI 兼容 | https://api.jiekou.ai/openai |
| 74 | kenari | Kenari | OpenAI 兼容 | https://kenari.id/v1 |
| 75 | kilo | Kilo Gateway | OpenAI 兼容 | https://api.kilo.ai/api/gateway |
| 76 | kimi-for-coding | Kimi For Coding | Anthropic | https://api.kimi.com/coding/v1 |
| 77 | kuae-cloud-coding-plan | KUAE Cloud Coding Plan | OpenAI 兼容 | https://coding-plan-endpoint.kuaecloud.net/v1 |
| 78 | lilac | Lilac | OpenAI 兼容 | https://api.getlilac.com/v1 |
| 79 | llama | Llama | OpenAI 兼容 | https://api.llama.com/compat/v1/ |
| 80 | llmgateway | LLM Gateway | OpenAI 兼容 | https://api.llmgateway.io/v1 |
| 81 | llmtr | LLMTR | OpenAI 兼容 | https://llmtr.com/v1 |
| 82 | lmstudio | LMStudio | OpenAI 兼容 | http://127.0.0.1:1234/v1 |
| 83 | longcat | LongCat | OpenAI 兼容 | https://api.longcat.chat/openai |
| 84 | lucidquery | LucidQuery | OpenAI 兼容 | https://api.lucidquery.com/v1 |
| 85 | lynkr | Lynkr | OpenAI 兼容 | http://127.0.0.1:8081/v1 |
| 86 | meganova | Meganova | OpenAI 兼容 | https://api.meganova.ai/v1 |
| 87 | merge-gateway | Merge Gateway | 其它 | - |
| 88 | meta | Meta | OpenAI 原生 | https://api.meta.ai/v1 |
| 89 | minimax | MiniMax (minimax.io) | Anthropic | https://api.minimax.io/anthropic/v1 |
| 90 | minimax-cn | MiniMax (minimaxi.com) | Anthropic | https://api.minimaxi.com/anthropic/v1 |
| 91 | minimax-cn-coding-plan | MiniMax Token Plan (minimaxi.com) | Anthropic | https://api.minimaxi.com/anthropic/v1 |
| 92 | minimax-coding-plan | MiniMax Token Plan (minimax.io) | Anthropic | https://api.minimax.io/anthropic/v1 |
| 93 | mistral | Mistral | 其它 | - |
| 94 | mixlayer | Mixlayer | OpenAI 兼容 | https://models.mixlayer.ai/v1 |
| 95 | moark | Moark | OpenAI 兼容 | https://moark.com/v1 |
| 96 | modal | Modal | OpenAI 兼容 | https://inference.us-west.modal.direct/v1 |
| 97 | model-oracle-ai | Model Oracle AI | OpenAI 兼容 | https://api.modeloracle.com/api/v1 |
| 98 | modelscope | ModelScope | OpenAI 兼容 | https://api-inference.modelscope.cn/v1 |
| 99 | moonshotai | Moonshot AI | OpenAI 兼容 | https://api.moonshot.ai/v1 |
| 100 | moonshotai-cn | Moonshot AI (China) | OpenAI 兼容 | https://api.moonshot.cn/v1 |
| 101 | morph | Morph | OpenAI 兼容 | https://api.morphllm.com/v1 |
| 102 | nano-gpt | NanoGPT | OpenAI 兼容 | https://nano-gpt.com/api/v1 |
| 103 | nearai | NEAR AI Cloud | OpenAI 兼容 | https://cloud-api.near.ai/v1 |
| 104 | nebius | Nebius Token Factory | OpenAI 兼容 | https://api.tokenfactory.nebius.com/v1 |
| 105 | neon | Neon | OpenAI 兼容 | ${NEON_AI_GATEWAY_BASE_URL}/v1 |
| 106 | neuralwatt | Neuralwatt | OpenAI 兼容 | https://api.neuralwatt.com/v1 |
| 107 | nova | Nova | OpenAI 兼容 | https://api.nova.amazon.com/v1 |
| 108 | novita-ai | NovitaAI | OpenAI 兼容 | https://api.novita.ai/openai |
| 109 | nvidia | Nvidia | OpenAI 兼容 | https://integrate.api.nvidia.com/v1 |
| 110 | ofox | Ofox | OpenAI 兼容 | https://api.ofox.ai/v1 |
| 111 | ollama-cloud | Ollama Cloud | OpenAI 兼容 | https://ollama.com/v1 |
| 112 | openai | OpenAI | OpenAI 原生 | - |
| 113 | opencode | OpenCode Zen | OpenAI 兼容 | https://opencode.ai/zen/v1 |
| 114 | opencode-go | OpenCode Go | OpenAI 兼容 | https://opencode.ai/zen/go/v1 |
| 115 | openrouter | OpenRouter | 其它 | https://openrouter.ai/api/v1 |
| 116 | orcarouter | OrcaRouter | OpenAI 兼容 | https://api.orcarouter.ai/v1 |
| 117 | ovhcloud | OVHcloud AI Endpoints | OpenAI 兼容 | https://oai.endpoints.kepler.ai.cloud.ovh.net/v1 |
| 118 | perplexity | Perplexity | 其它 | - |
| 119 | perplexity-agent | Perplexity Agent | OpenAI 原生 | https://api.perplexity.ai/v1 |
| 120 | pioneer | Pioneer | OpenAI 兼容 | https://api.pioneer.ai/v1 |
| 121 | poe | Poe | OpenAI 兼容 | https://api.poe.com/v1 |
| 122 | poolside | Poolside | OpenAI 兼容 | https://inference.poolside.ai/v1 |
| 123 | privatemode-ai | Privatemode AI | OpenAI 兼容 | http://localhost:8080/v1 |
| 124 | qihang-ai | QiHang | OpenAI 兼容 | https://api.qhaigc.net/v1 |
| 125 | qiniu-ai | Qiniu | OpenAI 兼容 | https://api.qnaigc.com/v1 |
| 126 | qvac | QVAC | 其它 | - |
| 127 | regolo-ai | Regolo AI | OpenAI 兼容 | https://api.regolo.ai/v1 |
| 128 | requesty | Requesty | OpenAI 兼容 | https://router.requesty.ai/v1 |
| 129 | routing-run | routing.run | OpenAI 兼容 | https://api.routing.run/v1 |
| 130 | sakana | Sakana AI | OpenAI 兼容 | https://api.sakana.ai/v1 |
| 131 | sap-ai-core | SAP AI Core | 其它 | - |
| 132 | sarvam | Sarvam AI | OpenAI 兼容 | https://api.sarvam.ai/v1 |
| 133 | scaleway | Scaleway | OpenAI 兼容 | https://api.scaleway.ai/v1 |
| 134 | siliconflow | SiliconFlow | OpenAI 兼容 | https://api.siliconflow.com/v1 |
| 135 | siliconflow-cn | SiliconFlow (China) | OpenAI 兼容 | https://api.siliconflow.cn/v1 |
| 136 | snowflake-cortex | Snowflake Cortex | OpenAI 兼容 | https://${SNOWFLAKE_ACCOUNT}.snowflakecomputing.com/api/v2/cortex/v1 |
| 137 | stackit | STACKIT | OpenAI 兼容 | https://api.openai-compat.model-serving.eu01.onstackit.cloud/v1 |
| 138 | stepfun | StepFun (China) | OpenAI 兼容 | https://api.stepfun.com/v1 |
| 139 | stepfun-ai | StepFun (Global) | OpenAI 兼容 | https://api.stepfun.ai/v1 |
| 140 | stepfun-ai-step-plan | StepFun Step Plan (Global) | OpenAI 兼容 | https://api.stepfun.ai/step_plan/v1 |
| 141 | stepfun-step-plan | StepFun Step Plan (China) | OpenAI 兼容 | https://api.stepfun.com/step_plan/v1 |
| 142 | subconscious | Subconscious | Anthropic | https://api.subconscious.dev/v1 |
| 143 | submodel | submodel | OpenAI 兼容 | https://llm.submodel.ai/v1 |
| 144 | synthetic | Synthetic | OpenAI 兼容 | https://api.synthetic.new/openai/v1 |
| 145 | tencent-coding-plan | Tencent Coding Plan (China) | OpenAI 兼容 | https://api.lkeap.cloud.tencent.com/coding/v3 |
| 146 | tencent-token-plan | Tencent Token Plan | OpenAI 兼容 | https://api.lkeap.cloud.tencent.com/plan/v3 |
| 147 | tencent-tokenhub | Tencent TokenHub | OpenAI 兼容 | https://tokenhub.tencentmaas.com/v1 |
| 148 | tensorx | TensorX | OpenAI 兼容 | https://api.tensorx.ai/v1 |
| 149 | the-grid-ai | The Grid AI | OpenAI 兼容 | https://api.thegrid.ai/v1 |
| 150 | thinkingmachines | Thinking Machines | Anthropic | https://tinker.thinkingmachines.dev/services/tinker-prod/anthropic/api/v1 |
| 151 | tinfoil | Tinfoil | OpenAI 兼容 | https://inference.tinfoil.sh/v1 |
| 152 | togetherai | Together AI | 其它 | - |
| 153 | trustedrouter | TrustedRouter | OpenAI 兼容 | https://api.trustedrouter.com/v1 |
| 154 | umans-ai | Umans AI | OpenAI 兼容 | https://api.code.umans.ai/v1 |
| 155 | umans-ai-coding-plan | Umans AI Coding Plan | OpenAI 兼容 | https://api.code.umans.ai/v1 |
| 156 | unorouter | UnoRouter | OpenAI 兼容 | https://api.unorouter.com/v1 |
| 157 | upstage | Upstage | OpenAI 兼容 | https://api.upstage.ai/v1/solar |
| 158 | v0 | v0 | 其它 | - |
| 159 | venice | Venice AI | 其它 | - |
| 160 | vercel | Vercel AI Gateway | 其它 | - |
| 161 | vivgrid | Vivgrid | OpenAI 原生 | https://api.vivgrid.com/v1 |
| 162 | vultr | Vultr | OpenAI 兼容 | https://api.vultrinference.com/v1 |
| 163 | wafer.ai | Wafer | OpenAI 兼容 | https://pass.wafer.ai/v1 |
| 164 | wandb | Weights & Biases | OpenAI 兼容 | https://api.inference.wandb.ai/v1 |
| 165 | xai | xAI | 其它 | - |
| 166 | xiaomi | Xiaomi | OpenAI 兼容 | https://api.xiaomimimo.com/v1 |
| 167 | xiaomi-token-plan-ams | Xiaomi Token Plan (Europe) | OpenAI 兼容 | https://token-plan-ams.xiaomimimo.com/v1 |
| 168 | xiaomi-token-plan-cn | Xiaomi Token Plan (China) | OpenAI 兼容 | https://token-plan-cn.xiaomimimo.com/v1 |
| 169 | xiaomi-token-plan-sgp | Xiaomi Token Plan (Singapore) | OpenAI 兼容 | https://token-plan-sgp.xiaomimimo.com/v1 |
| 170 | xpersona | Xpersona | OpenAI 兼容 | https://www.xpersona.co/v1 |
| 171 | zai | Z.AI | OpenAI 兼容 | https://api.z.ai/api/paas/v4 |
| 172 | zai-coding-plan | Z.AI Coding Plan | OpenAI 兼容 | https://api.z.ai/api/coding/paas/v4 |
| 173 | zeldoc | Zeldoc | OpenAI 兼容 | https://api.zeldoc.ai/v1 |
| 174 | zenifra | Zenifra | OpenAI 兼容 | https://ai.zenifra.com/v1 |
| 175 | zenmux | ZenMux | OpenAI 兼容 | https://zenmux.ai/api/v1 |
| 176 | zhipuai | Zhipu AI | OpenAI 兼容 | https://open.bigmodel.cn/api/paas/v4 |
| 177 | zhipuai-coding-plan | Zhipu AI Coding Plan | OpenAI 兼容 | https://open.bigmodel.cn/api/coding/paas/v4 |

## 10. 错误处理建议（跨 provider 通用）

- **可重试**：429、408、500、502、503、504、529 —— 尊重 `Retry-After` / `retry-after`，指数退避 + jitter。
- **不可重试**：400、401、402、403、404、409、413、422 —— 重试只会重复失败。
- **计费/配额类不重试**：402、429（`insufficient_quota` / `credit_balance_exhausted` / 1113 / 1308-1311）、
  重试不能恢复访问，需先充值或调整限额。
- **判断优先顺序**：`error.type`（OpenAI 系）或 `error_type`（OpenRouter）→ `error.code`（业务码）→ HTTP 状态码。
- **流式场景**：错误可能出现在 HTTP 200 之后（SSE error 事件），不要只检查状态码。
- **用户可读消息**：优先展示 `message` 字段；对计费/配额/敏感内容类错误给出明确的中文提示，不展示原始 JSON。

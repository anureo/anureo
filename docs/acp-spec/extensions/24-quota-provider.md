# Quota 与 Provider 凭据管理

> **命名空间**: `_anureo.dev/quota/*`
> **Capability key**: `quota`
> **实现状态**: ❌ 未实现

---

## Capability

```json
{
  "quota": {
    "usage": true,
    "balance": true,
    "provider_list": true,
    "provider_save": true,
    "provider_delete": true
  }
}
```

- Client 必须在 `initialize` 时声明 `agentCapabilities._meta["anureo.dev"].quota` 的 method 粒度。
- **与 `session/update` 的关系**: `session/update` 中的 `usage_update` 覆盖单个 session 内的 token usage（input/output tokens、cache hits 等）；本扩展覆盖 billing/quota 维度（账户级用量统计、余额查询）和 provider 凭据管理。
- **安全约束**: Provider secret（API key 的完整值）不出现在任何 response 中。`provider/list` 只返回脱敏标识（如 `sk-****-1234`）。

---

## Methods

### `_anureo.dev/quota/usage`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `quota.usage` |
| 权限 | 无（读取操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/quota/usage",
  "params": {
    "range": "current_month",
    "provider": "openai",
    "granularity": "daily"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `range` | string | 否 | 时间范围：`today` / `current_week` / `current_month` / `custom` |
| `rangeStart` | string (ISO 8601) | `range=custom` 时必填 | 自定义起始时间 |
| `rangeEnd` | string (ISO 8601) | `range=custom` 时必填 | 自定义结束时间 |
| `provider` | string | 否 | 按提供方过滤 |
| `granularity` | string | 否 | 聚合粒度：`hourly` / `daily` / `monthly` |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "range": "current_month",
    "rangeStart": "2025-08-01T00:00:00Z",
    "rangeEnd": "2025-08-31T23:59:59Z",
    "granularity": "daily",
    "summary": {
      "totalTokens": 1520000,
      "inputTokens": 980000,
      "outputTokens": 540000,
      "cacheReadTokens": 280000,
      "cacheWriteTokens": 120000,
      "estimatedCost": 12.50,
      "currency": "USD",
      "requestCount": 342
    },
    "breakdown": [
      {
        "period": "2025-08-19",
        "provider": "openai",
        "model": "gpt-4o",
        "inputTokens": 52000,
        "outputTokens": 28000,
        "cacheReadTokens": 15000,
        "cacheWriteTokens": 6000,
        "requestCount": 18,
        "estimatedCost": 0.85,
        "currency": "USD"
      }
    ]
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `result.range` | string | 实际使用的范围 |
| `result.rangeStart` | string | 范围起始时间 |
| `result.rangeEnd` | string | 范围结束时间 |
| `result.granularity` | string | 聚合粒度 |
| `result.summary` | object | 汇总统计 |
| `result.summary.totalTokens` | int | 总 token 数 |
| `result.summary.inputTokens` | int | 输入 token 总数 |
| `result.summary.outputTokens` | int | 输出 token 总数 |
| `result.summary.cacheReadTokens` | int | 缓存读取 token 数 |
| `result.summary.cacheWriteTokens` | int | 缓存写入 token 数 |
| `result.summary.estimatedCost` | number | 预估费用 |
| `result.summary.currency` | string | 货币单位 |
| `result.summary.requestCount` | int | 请求总数 |
| `result.breakdown[]` | array | 按粒度+provider+model 的分项统计 |
| `result.breakdown[].period` | string | 时间区间标识 |
| `result.breakdown[].provider` | string | 提供方 |
| `result.breakdown[].model` | string | 模型标识 |
| `result.breakdown[].estimatedCost` | number | 该项预估费用 |
| `result.breakdown[].requestCount` | int | 该项请求次数 |

#### 逻辑说明

1. 用量数据来源于 server 端的计费记录，不是 session 的 `usage_update`（后者只是实时 token 统计）。
2. `estimatedCost` 为预估值，最终费用以提供方账单为准。
3. `granularity` 决定 `breakdown` 中每个条目的时间粒度；`provider` 过滤时只返回该提供方的数据。
4. 数据量较大时，server 可限制 `breakdown` 条目数并提示 client 分段查询。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaUsageRequest {
    pub range: Option<UsageRange>,
    pub range_start: Option<DateTime<Utc>>,
    pub range_end: Option<DateTime<Utc>>,
    pub provider: Option<String>,
    pub granularity: Option<UsageGranularity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UsageRange {
    Today,
    CurrentWeek,
    CurrentMonth,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UsageGranularity {
    Hourly,
    Daily,
    Monthly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub total_tokens: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub estimated_cost: f64,
    pub currency: String,
    pub request_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageBreakdownItem {
    pub period: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub request_count: u64,
    pub estimated_cost: f64,
    pub currency: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaUsageResponse {
    pub range: String,
    pub range_start: DateTime<Utc>,
    pub range_end: DateTime<Utc>,
    pub granularity: String,
    pub summary: UsageSummary,
    pub breakdown: Vec<UsageBreakdownItem>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `quota.usage` 未声明 |
| `Invalid Params (-32602)` | `range=custom` 但缺少 `rangeStart`/`rangeEnd` |

---

### `_anureo.dev/quota/balance`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `quota.balance` |
| 权限 | 无（读取操作） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/quota/balance",
  "params": {}
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "balance": 85.30,
    "currency": "USD",
    "plan": "pro",
    "resetDate": "2025-09-01T00:00:00Z",
    "limits": {
      "monthlyTokenBudget": 5000000,
      "monthlyTokenUsed": 1520000,
      "dailyRequestLimit": 1000,
      "dailyRequestUsed": 42
    },
    "overageAllowed": false,
    "updatedAt": "2025-08-19T14:30:00Z"
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `result.balance` | number | 当前余额 |
| `result.currency` | string | 货币单位 |
| `result.plan` | string | 当前套餐标识 |
| `result.resetDate` | string (ISO 8601) | 额度重置日期 |
| `result.limits` | object | 额度限制 |
| `result.limits.monthlyTokenBudget` | int | 月度 token 预算 |
| `result.limits.monthlyTokenUsed` | int | 已使用 token |
| `result.limits.dailyRequestLimit` | int | 每日请求上限 |
| `result.limits.dailyRequestUsed` | int | 今日已用请求数 |
| `result.overageAllowed` | bool | 是否允许超额使用 |
| `result.updatedAt` | string (ISO 8601) | 余额最后更新时间 |

#### 逻辑说明

1. 余额为 server 端维护的计费状态，可能来自提供方 API 同步或本地计量。
2. 如果用户使用自带 provider 凭据（BYOK），`balance` 可能为 `null` 或显示 "unlimited"（取决于实现）。
3. `limits` 字段为可选——预付费套餐有预算限制，后付费套餐可能不设限制。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaBalanceResponse {
    pub balance: Option<f64>,
    pub currency: String,
    pub plan: Option<String>,
    pub reset_date: Option<DateTime<Utc>>,
    pub limits: Option<QuotaLimits>,
    pub overage_allowed: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuotaLimits {
    pub monthly_token_budget: Option<u64>,
    pub monthly_token_used: Option<u64>,
    pub daily_request_limit: Option<u64>,
    pub daily_request_used: Option<u64>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `quota.balance` 未声明 |
| `Internal Error (-32603)` | 无法连接到计费后端 |

---

### `_anureo.dev/quota/provider/list`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `quota.provider_list` |
| 权限 | 无（读取操作） |
| 安全 | **Secret 不出现在 response 中** |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "method": "_anureo.dev/quota/provider/list",
  "params": {}
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 3,
  "result": {
    "providers": [
      {
        "id": "prov_001",
        "name": "openai",
        "displayName": "OpenAI",
        "type": "openai-compatible",
        "baseUrl": "https://api.openai.com/v1",
        "maskedApiKey": "sk-****-1234",
        "models": ["gpt-4o", "gpt-4o-mini", "o1"],
        "isActive": true,
        "isDefault": true,
        "createdAt": "2025-08-01T10:00:00Z",
        "updatedAt": "2025-08-19T10:00:00Z"
      },
      {
        "id": "prov_002",
        "name": "anthropic",
        "displayName": "Anthropic",
        "type": "anthropic",
        "baseUrl": "https://api.anthropic.com",
        "maskedApiKey": "sk-ant-****-5678",
        "models": ["claude-sonnet-4-20250514", "claude-opus-4-20250514"],
        "isActive": true,
        "isDefault": false,
        "createdAt": "2025-08-05T10:00:00Z",
        "updatedAt": "2025-08-19T10:00:00Z"
      }
    ]
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `providers[].id` | string | Provider 凭据唯一标识 |
| `providers[].name` | string | Provider 标识（用于 routing） |
| `providers[].displayName` | string | UI 显示名称 |
| `providers[].type` | string | Provider 类型：`openai-compatible` / `anthropic` / `google` / `custom` |
| `providers[].baseUrl` | string | API base URL |
| `providers[].maskedApiKey` | string | **脱敏的 API key**（只显示首尾几位，中间用 `****` 替代） |
| `providers[].models` | string[] | 可用模型列表 |
| `providers[].isActive` | bool | 是否启用 |
| `providers[].isDefault` | bool | 是否为默认 provider |
| `providers[].createdAt` | string (ISO 8601) | 创建时间 |
| `providers[].updatedAt` | string (ISO 8601) | 更新时间 |

#### 逻辑说明

1. **Provider secret 永远不出现在 response 中**。`maskedApiKey` 是脱敏后的标识，格式为 `prefix-****-suffix`。
2. Client 通过 `maskedApiKey` 帮助用户识别凭据，但不能从中恢复完整 key。
3. 不支持分页——provider 数量通常很少（< 20），直接返回完整列表。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCredential {
    pub id: String,
    pub name: String,
    pub display_name: String,
    #[serde(rename = "type")]
    pub provider_type: ProviderType,
    pub base_url: String,
    pub masked_api_key: String,
    pub models: Vec<String>,
    pub is_active: bool,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProviderType {
    OpenAiCompatible,
    Anthropic,
    Google,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderListResponse {
    pub providers: Vec<ProviderCredential>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `quota.provider_list` 未声明 |

---

### `_anureo.dev/quota/provider/save`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `quota.provider_save` |
| 权限 | Server-side authorization（**敏感操作**） |
| 幂等 | 支持 `clientRequestId` 幂等键 |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "method": "_anureo.dev/quota/provider/save",
  "params": {
    "clientRequestId": "req-save-prov-001",
    "id": "prov_001",
    "name": "openai",
    "displayName": "OpenAI",
    "type": "openai-compatible",
    "baseUrl": "https://api.openai.com/v1",
    "apiKey": "sk-full-api-key-value-here",
    "models": ["gpt-4o", "gpt-4o-mini", "o1"],
    "isActive": true,
    "isDefault": true
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `clientRequestId` | string | 否 | 幂等键 |
| `id` | string | 否 | 已有 provider 的 ID（更新时提供）；省略时为新建 |
| `name` | string | 是 | Provider 标识 |
| `displayName` | string | 否 | UI 显示名称 |
| `type` | string | 是 | Provider 类型 |
| `baseUrl` | string | 是 | API base URL |
| `apiKey` | string | 是 | **完整 API key**（仅在 request 中出现，永不回传） |
| `models` | string[] | 否 | 可用模型列表 |
| `isActive` | bool | 否 | 是否启用 |
| `isDefault` | bool | 否 | 是否设为默认 provider |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 4,
  "result": {
    "id": "prov_001",
    "name": "openai",
    "displayName": "OpenAI",
    "type": "openai-compatible",
    "baseUrl": "https://api.openai.com/v1",
    "maskedApiKey": "sk-****-1234",
    "models": ["gpt-4o", "gpt-4o-mini", "o1"],
    "isActive": true,
    "isDefault": true,
    "createdAt": "2025-08-01T10:00:00Z",
    "updatedAt": "2025-08-19T14:00:00Z"
  }
}
```

#### 逻辑说明

1. **`apiKey` 是完整 API key，只在 request 中传输，response 中永远不出现。** Server 接收后应立即加密存储（如 OS keychain 或加密文件），不写入日志、不放入 `session/update`、不缓存到内存明文。
2. `id` 省略时为新建，提供时为更新（覆盖式）。
3. 更新时如果未提供 `apiKey`，保持原有 key 不变。
4. 设置 `isDefault: true` 时，server 自动取消其他 provider 的 default 标记。
5. Server 应在保存后验证凭据有效性（可选的 connectivity check），但不应阻塞保存操作。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderSaveRequest {
    pub client_request_id: Option<String>,
    pub id: Option<String>,
    pub name: String,
    pub display_name: Option<String>,
    #[serde(rename = "type")]
    pub provider_type: ProviderType,
    pub base_url: String,
    pub api_key: String,
    pub models: Option<Vec<String>>,
    pub is_active: Option<bool>,
    pub is_default: Option<bool>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `quota.provider_save` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `name`、`type`、`baseUrl` 或 `apiKey` 为空 |
| `conflict (-32003)` | `name` 已存在且 `id` 不匹配 |

---

### `_anureo.dev/quota/provider/delete`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `quota.provider_delete` |
| 权限 | Server-side authorization（**敏感操作**） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "method": "_anureo.dev/quota/provider/delete",
  "params": {
    "id": "prov_002"
  }
}
```

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 5,
  "result": {
    "id": "prov_002",
    "deleted": true
  }
}
```

#### 逻辑说明

1. 删除 provider 凭据后，server 必须从安全存储中彻底移除 API key，不可仅标记为已删除。
2. 如果删除的是默认 provider，server 应自动选择另一个 active provider 作为默认，或要求用户重新设置。
3. 删除正在被活跃 session 使用的 provider 时：
   - 不阻止删除（凭据管理优先）。
   - 后续 session 的 LLM 调用将失败，server 应在 `session/update` 中返回错误信息。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDeleteRequest {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderDeleteResponse {
    pub id: String,
    pub deleted: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `quota.provider_delete` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Invalid Params (-32602)` | `id` 为空 |
| `not_found (-32004)` | `id` 不存在 |

---

## Notifications

本扩展域不定义独立 notification。Provider 凭据变更可能影响 `_anureo.dev/capability_changed`（如删除默认 provider 后部分功能不可用），但 quota/usage 数据变化通过 `session/update` 的 `usage_update` 或 client 主动轮询获取。

---

## Reconnect Resync 映射

本扩展域无 notification，无需 resync 映射。Client 重连后主动调用 `quota/balance` 和 `quota/provider/list` 刷新状态。

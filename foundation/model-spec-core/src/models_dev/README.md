# models_dev

[models.dev](https://models.dev) 数据的 Rust 抽象层：schema 类型、JSON 解析器、HTTP resolver。

属于 `model-spec-core` crate，是所有 models.dev 相关代码的**唯一入口**。

## 架构

```
models_dev/
├── mod.rs        模块入口 & re-exports
├── provider.rs   Provider schema
├── model.rs      Model schema + enums + 便捷方法
├── cost.rs       Cost / CostTier schema
├── limit.rs      ModelLimit / Modalities schema
├── parser.rs     手写容错 JSON 解析器
└── resolver.rs   HTTP resolver（feature = "resolver"）
```

五层抽象，自底向上：

| 层 | 职责 | 关键类型 |
|---|---|---|
| **Schema** | JSON schema → 强类型 struct | `Provider`, `Model`, `Cost`, `ModelLimit`, `Modalities` |
| **Enum** | 标签联合体 → Rust enum | `ModelStatus`, `ReasoningOption`, `Interleaved`, `ProviderShape` |
| **Config override** | 模型/实验性配置覆盖 | `ModelProviderConfig`, `Experimental`, `ExperimentalMode` |
| **Parser** | 容错 JSON 解析 | `parse_provider`, `parse_model`, `parse_all_providers` |
| **Resolver** | HTTP 获取 + 查询 | `ModelsDevResolver`, `HttpClient` trait |

## Feature gates

| Feature | 启用内容 | 额外依赖 |
|---|---|---|
| *(default)* | Schema 类型 + Parser（纯 serde/serde_json） | 无 |
| `resolver` | `ModelsDevResolver` + `HttpClient` + `ReqwestHttpClient` | reqwest, tokio, async-trait, anureo-http-retry, tracing |
| `tier` | 启用 `resolver` + tier 分级 | toml, thiserror |

不含 `resolver` feature 时，模块只有类型定义和解析器，零网络依赖。

## 快速上手

### 解析本地 JSON

```rust
use model_spec_core::parse_all_providers;

let body = std::fs::read_to_string("api.json")?;
let providers = parse_all_providers(&body)?;

for (id, provider) in &providers {
    println!("{}: {} ({} models)", id, provider.name, provider.models.len());
}
```

### 从 models.dev 远程获取

```rust
use model_spec_core::ModelsDevResolver;

let resolver = ModelsDevResolver::new();

// 获取全部模型，key 格式 "provider/model_id"
let all = resolver.fetch_all().await?;
let model = all.get("anthropic/claude-3-5-sonnet-20241022").unwrap();

// 按 provider + model_id 查询单个模型
let model = resolver.fetch_model("anthropic", "claude-3-5-sonnet-20241022").await?;

// 裸模型名搜索（遍历所有 provider）
let model = resolver.resolve_by_bare_model_name("gpt-4o").await;
```

### 通过 ModelResolver trait 使用

`ModelsDevResolver` 实现了上层 `ModelResolver` trait，可与 `CachedResolver`、`CompositeResolver` 等组合：

```rust
use model_spec_core::resolver::{ModelResolver, ModelsDevResolver, CachedResolver};

let inner = ModelsDevResolver::new();
let cached = CachedResolver::new(inner);

// 统一接口
let model = cached.resolve("openai", "gpt-4o").await;
let model = cached.resolve_combined("openai/gpt-4o").await;
```

### 提取 provider API 地址

```rust
use model_spec_core::extract_provider_api_from_models_dev_json;

let api = extract_provider_api_from_models_dev_json(&body, "zhipuai-coding-plan");
// → Some("https://open.bigmodel.cn/api/paas/v4")
```

## Schema 类型

### Provider

```rust
pub struct Provider {
    pub id: String,
    pub name: String,
    pub env: Vec<String>,       // 环境变量名，如 ["ANTHROPIC_API_KEY"]
    pub npm: Option<String>,    // npm 包名
    pub doc: Option<String>,    // 文档 URL
    pub api: Option<String>,    // API base URL
    pub models: HashMap<String, Model>,
}
```

### Model

核心字段（共 20+）：

| 字段 | 类型 | 说明 |
|---|---|---|
| `id` / `name` | `String` | 模型标识与显示名 |
| `family` | `Option<String>` | 模型族（如 `claude-sonnet`） |
| `attachment` | `bool` | 附件支持 |
| `reasoning` | `bool` | 推理能力 |
| `reasoning_options` | `Option<Vec<ReasoningOption>>` | 推理选项 |
| `tool_call` | `bool` (默认 true) | 工具调用支持 |
| `interleaved` | `Option<Interleaved>` | 交错推理 |
| `structured_output` | `Option<bool>` | 结构化输出 |
| `temperature` | `Option<bool>` | 温度参数支持 |
| `knowledge` | `Option<String>` | 知识截止日期 |
| `modalities` | `Modalities` | 输入输出模态 |
| `open_weights` | `bool` | 开源权重 |
| `limit` | `ModelLimit` | token 限制 |
| `cost` | `Option<Cost>` | 定价 |
| `status` | `Option<ModelStatus>` | alpha/beta/deprecated |
| `experimental` | `Option<Experimental>` | 实验性 per-mode 覆盖 |
| `provider` | `Option<ModelProviderConfig>` | 模型级 provider 配置 |

便捷方法：

```rust
model.is_reasoning()              // -> bool
model.supports_tools()            // -> bool
model.supports_vision()           // -> bool (委托 modalities)
model.supports_audio()            // -> bool
model.context_window()            // -> u32
model.max_output_tokens()         // -> u32
model.input_price_per_million()   // -> Option<f64>
model.output_price_per_million()  // -> Option<f64>
model.tier()                      // -> ModelTier
```

### Cost

```rust
pub struct Cost {
    pub input: f64,                        // $/M tokens
    pub output: f64,
    pub reasoning: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub input_audio: Option<f64>,
    pub output_audio: Option<f64>,
    pub context_over_200k: Option<Box<Cost>>,  // 递归：超 200k context 的分段计价
    pub tiers: Option<Vec<CostTier>>,           // 按 context 大小分档计价
}
```

```rust
// 估算费用
let cost = model.cost.unwrap();
let usd = cost.estimate(input_tokens, output_tokens);
```

### Modalities

```rust
pub struct Modalities {
    pub input: Vec<ModalityType>,   // Text | Image | Audio | Video | Pdf
    pub output: Vec<ModalityType>,
}

modalities.supports_text()    // -> bool
modalities.supports_vision()  // -> bool
modalities.supports_audio()   // -> bool
modalities.supports_video()   // -> bool
modalities.supports_pdf()     // -> bool
```

## 枚举映射

models.dev JSON 中的标签联合体映射为 Rust enum：

### ReasoningOption

```jsonc
// JSON 三种形态
{ "type": "toggle" }
{ "type": "effort", "values": ["low", "medium", "high", null] }
{ "type": "budget_tokens", "min": 1024, "max": 32768 }
```

```rust
pub enum ReasoningOption {
    Toggle,
    Effort { values: Vec<Option<ReasoningEffort>> },
    BudgetTokens { min: Option<f64>, max: Option<f64> },
}
```

`ReasoningEffort` 变体：`None` / `Minimal` / `Low` / `Medium` / `High` / `Xhigh` / `Max` / `Default`

### Interleaved

```jsonc
// JSON 两种形态
true
{ "field": "reasoning_content" }
```

```rust
pub enum Interleaved {
    Simple,
    Field { field: InterleavedField },  // ReasoningContent | ReasoningDetails
}
```

### ModelStatus / ProviderShape

```rust
enum ModelStatus  { Alpha, Beta, Deprecated }          // serde rename_all = "lowercase"
enum ProviderShape { Responses, Completions }           // serde rename_all = "lowercase"
```

## Config override

### ModelProviderConfig

模型级 provider 配置覆盖，允许单个模型自定义 API 端点、请求体、headers：

```rust
pub struct ModelProviderConfig {
    pub npm: Option<String>,
    pub api: Option<String>,
    pub shape: Option<ProviderShape>,      // responses | completions
    pub body: Option<HashMap<String, Value>>,
    pub headers: Option<HashMap<String, String>>,
}
```

### Experimental

实验性 per-mode 覆盖，按 mode 名称（如 `"reasoning"`）索引，可覆盖 cost 和 provider 配置：

```rust
pub struct Experimental {
    pub modes: Option<HashMap<String, ExperimentalMode>>,
}

pub struct ExperimentalMode {
    pub cost: Option<Cost>,
    pub provider: Option<ExperimentalProviderConfig>,  // body + headers
}
```

## Parser

手写容错解析器（`parser.rs`），非纯 serde derive。

**设计决策：手写而非 serde derive**

- models.dev 的 JSON 字段经常缺失或为 null，serde derive 在字段类型不匹配时会整体失败
- 手写解析器逐字段提取，缺失给默认值，类型不符跳过
- **`limit` 是必填门控** — 没有 `limit` 字段的模型直接返回 `None`，不进入结果

公开函数：

| 函数 | 输入 | 输出 |
|---|---|---|
| `parse_provider(id, &Value)` | provider_id + JSON Value | `Option<Provider>` |
| `parse_model(id, &Value)` | model_id + JSON Value | `Option<Model>` |
| `parse_all_providers(&str)` | JSON 字符串 | `Result<HashMap<String, Provider>, String>` |
| `parse_model_limit(&Value)` | limit JSON Value | `Option<ModelLimit>` |
| `extract_provider_api_from_models_dev_json(body, name)` | JSON 字符串 + provider 名 | `Option<String>` |

`extract_provider_api_from_models_dev_json` 支持大小写不敏感匹配。

## Resolver

> 需要 `resolver` feature。

### HttpClient trait

抽象 HTTP 传输，解耦网络层：

```rust
#[async_trait]
pub trait HttpClient: Send + Sync {
    async fn get(&self, url: &str) -> Result<String, String>;
}
```

`ReqwestHttpClient` 是生产实现，内置重试逻辑（`anureo_http_retry`，瞬态错误自动退避重试）。测试时注入 `MockHttpClient`。

### ModelsDevResolver

```rust
pub struct ModelsDevResolver {
    base_url: String,
    http_client: Arc<dyn HttpClient>,
}
```

构造：

```rust
// 默认 URL (https://models.dev/api.json) + reqwest client
let r = ModelsDevResolver::new();

// 自定义 URL + client（测试/代理）
let r = ModelsDevResolver::with_client(url, Arc::new(my_client));
```

查询方法：

| 方法 | 返回 | 说明 |
|---|---|---|
| `fetch_all()` | `HashMap<"provider/model", Model>` | 全部模型，key 含 provider 前缀 |
| `fetch_all_providers()` | `HashMap<String, Provider>` | 全部 provider |
| `fetch_provider(id)` | `Option<Provider>` | 单个 provider |
| `fetch_model(p, m)` | `Option<Model>` | 单个模型 |
| `resolve_by_bare_model_name(name)` | `Option<Model>` | 裸名搜索（遍历所有 provider） |
| `resolve(p, m)` | `Option<Model>` | 实现 `ModelResolver` trait |

`resolve` 的 fallback 逻辑：如果 `model_id` 不含 `/`，会尝试 `"{provider_id}/{model_id}"` 格式匹配（兼容 `zenmux/openai/gpt-5` 这类嵌套 key）。

## 测试

```bash
# 纯类型 + parser 测试（不需要网络）
cargo test -p model-spec-core models_dev

# resolver 测试（使用 MockHttpClient，无真实网络请求）
cargo test -p model-spec-core --features resolver models_dev
```

Resolver 测试覆盖：正常解析、未知模型返回 None、HTTP 失败返回 None、JSON 无效返回 None、裸 model_id fallback、完整 provider 元数据。

# OpenAI-compat 流式响应解码错误分析

## 问题概述

Workflow agent 在执行长时间任务（大量文件读取 + 分析）时，以 `LLM invoke failed: OpenAI-compat stream body: error decoding response body` 错误终止。错误为 **non-retryable**，直接导致整个 workflow 失败。

## 影响

- `protocol-code-diff` workflow 两次执行均在 analyze 阶段失败（analyze-bootstrap / fn-hunter）
- 换模型（glm-5.2 → minimax-m2.7）不能解决——两个模型走同一个代理 `api.modelgate.dev/v1`
- 前序 agent（canary / inventory）正常通过，失败只发生在长上下文 agent

## 复现条件

1. Agent 读取大量文件（10+ 次 tool call），上下文积累到 200K+ tokens
2. 单次 LLM 请求（prefill + generation）耗时超过代理超时阈值（~300s）
3. LLM 通过 modelgate.dev OpenAI-compat 代理
4. 代理切断连接 → `reqwest::Error { kind: Decode }`

## 根因

### 调用链

```
agent.run()
  → think_node.run_with_context()
    → LlmClient::invoke_stream()
      → ChatOpenAICompat::invoke_stream()
        → send_with_retry()        ← POST 请求，有重试 ✅
        → res.chunk().await        ← 流式读取，无重试 ❌
          → Err(reqwest::Error { kind: Decode })
            → LlmError::InvokeFailed("OpenAI-compat stream body: error decoding response body")
```

### 代码定位

`foundation/llm/src/client/openai_compat/llm_client.rs:404-412`：

```rust
'sse: loop {
    let bytes = match res.chunk().await {
        Ok(Some(bytes)) => bytes,
        Ok(None) => break 'sse,
        Err(e) => {
            // ❌ 直接返回错误，不重试
            let msg = format!("OpenAI-compat stream body: {e}");
            return Err(self.audit_error(&ctx, 0, msg));
        }
    };
    // ...
}
```

`send_with_retry`（`llm_client.rs:160-249`）只重试初始 POST 的传输错误和 HTTP 状态码。流式连接建立后，数据传输阶段没有任何容错。

### 错误性质

`error decoding response body` 是 reqwest 的 `ErrorKind::Decode`（Body 解码错误），通常原因：

- 网关/代理超时切断长连接（TCP RST / incomplete response）
- SSE 流中途格式损坏（chunked transfer encoding 错误）
- 网络抖动导致连接中断

### 为什么不是 compaction / context window 问题

日志证据（`acp.log`）：

```
resolved model spec  model=minimax-cn-coding-plan/minimax-m2.7  context_limit=1000000
compact node entered  message_count=546  auto=true  max_context_tokens=1000000
context window check  current_tokens=243833  overflow=false
compact skipped       reason="no_overflow"
```

- Compaction 正常工作，模型正确解析
- 上下文从未溢出（最高 260K / 1M 阈值）
- 失败发生在 LLM API 层，不是本地推理层

## 为什么只有特定阶段失败

### 代理超时是核心触发因素

modelgate.dev 代理有一个 **~300s（5 分钟）的连接超时**。当单次 LLM 请求（prefill + generation）超过这个时间，代理强制切断连接。

### 日志证据

从 `acp.log` 提取 analyze-bootstrap 执行期间的所有 `chat create_stream`（请求开始）和 `stream response`（响应完成）配对：

```
trace=1f18cafa  16:15:49 START  → 无 DONE（5min 后连接被切断）
trace=1f18cb05  16:20:49 START  → 无 DONE（5min 后连接被切断）
16:25:50                          ERROR: error decoding response body
```

39 次成功的 LLM 调用耗时统计：

| 指标 | 值 |
|------|-----|
| 总成功调用数 | 39 |
| 平均耗时 | 9.1s |
| 最长耗时 | 44.6s |
| >60s 的调用 | 0（成功） |

**所有超过 60s 的 LLM 调用全部失败**（连接被代理切断）。

### 上下文大小 vs 单次请求耗时

| Agent 阶段 | 典型上下文 | 单次 LLM 耗时 | 结果 |
|-----------|-----------|-------------|------|
| canary | ~10K tokens | < 10s | ✅ |
| inventory | ~50K tokens | 10-45s | ✅ |
| analyze（读 10+ 文件后） | 200K+ tokens | 60s → 300s+ | ❌ |

上下文越大，LLM prefill 时间越长。当单次请求超过代理超时（~300s），连接被切断。

### 关键区分

**不是 agent 总运行时间长导致失败**——是**单次 LLM API 调用**的响应时间超过代理超时。agent 运行 10 分钟但每次 LLM 调用都在 45s 内完成 → 正常。agent 运行 5 分钟但某一次 LLM 调用花了 6 分钟 → 失败。

## 证据

### 失败实例 1：fn-hunter（glm-5.2）

| 指标 | 值 |
|------|-----|
| Instance | `luft-workflow_1784663573` |
| 模型 | `zhipuai-coding-plan/glm-5.2` |
| 运行时长 | 585,220ms (9.7 min) |
| Token 使用 | 0（Usage 事件从未触发） |
| Progress 事件 | 262（240 message + 22 tool_call） |
| 错误日志 | `LLM invoke failed: OpenAI-compat stream body: error decoding response body` |

### 失败实例 2：analyze-bootstrap（minimax-m2.7）

| 指标 | 值 |
|------|-----|
| Instance | `luft-workflow_1785479543` |
| 模型 | `minimax-cn-coding-plan/minimax-m2.7` |
| 运行时长 | 2,146,382ms (35 min) |
| Token 使用 | 0 |
| Progress 事件 | 322 |
| 错误日志 | `LLM invoke failed: OpenAI-compat stream body: error decoding response body`（完全相同） |

### 成功的 agent（对照组）

| Agent | 模型 | Tokens | 运行时长 | 状态 |
|-------|------|--------|----------|------|
| canary | minimax-m2.7 | 225K | 108s | ✅ |
| oc-inventory | minimax-m2.7 | 522K | 364s | ✅ |
| anureo-inventory | minimax-m2.7 | 744K | 588s | ✅ |

成功 agent 运行时间均 < 10 分钟，失败 agent 均 > 10 分钟。

## 修复方案

### 方案 A：流式重试（推荐）

在 `invoke_stream` 的 SSE 读取循环中，当 `res.chunk().await` 返回 decode error 时，重试整个请求而非直接报错。

```rust
// 伪代码
const STREAM_MAX_RETRIES: usize = 2;

for retry in 0..=STREAM_MAX_RETRIES {
    let mut res = self.send_with_retry(&url, &body, &request_id, ...).await?;
    
    'sse: loop {
        match res.chunk().await {
            Ok(Some(bytes)) => { /* process SSE */ }
            Ok(None) => break,   // 正常结束
            Err(e) => {
                if retry < STREAM_MAX_RETRIES {
                    tracing::warn!(retry = retry + 1, "stream interrupted, retrying");
                    // 重置累积状态，重新发送请求
                    break 'sse;   // 回到外层 for 循环重试
                }
                return Err(/* original error */);
            }
        }
    }
}
```

**注意事项**：
- 重试时需重置 `full_content`、`tool_calls_acc`、`tool_calls_forwarder` 等所有累积状态
- 上游 LLM 可能已经处理了部分请求（消耗了 tokens），但无法恢复——需接受重复消费
- SSE JSON 解析错误（`serde_json::from_str` 失败）应继续跳过而非重试（当前行为正确，`llm_client.rs:431-432`）

### 方案 B：降级为非流式

流式失败时回退到 `invoke()`（非流式 POST）。代码中已有类似模式（`llm_client.rs:537-583`，流式空响应回退），可扩展为流式错误时也回退。

```rust
Err(e) => {
    tracing::warn!(error = %e, "stream failed, falling back to non-streaming");
    return self.invoke(messages).await;
}
```

**优点**：实现简单，一行改动
**缺点**：非流式请求响应体更大，更容易超时

### 方案 C：增加超时配置

调整 reqwest client 的 timeout/pool 配置，减少连接被中间层切断的概率。

```rust
// 在 ChatOpenAICompat 构建时
reqwest::Client::builder()
    .timeout(Duration::from_secs(600))        // 总超时 10 分钟
    .pool_idle_timeout(Duration::from_secs(90))
    .tcp_keepalive(Duration::from_secs(60))
    .build()
```

**注意**：当前代码中 reqwest client 的 timeout 配置需确认。

### 推荐组合

**A + B**：流式读取失败时先尝试流式重试（最多 2 次），仍失败则降级为非流式请求。C 作为辅助。

## 受影响的代码路径

| 文件 | 行 | 作用 |
|------|----|------|
| `foundation/llm/src/client/openai_compat/llm_client.rs:404-412` | SSE 错误处理 | 直接返回错误，需加重试 |
| `foundation/llm/src/client/openai_compat/llm_client.rs:537-583` | 空响应回退 | 已有的非流式降级模式，可参考 |
| `foundation/llm/src/client/openai_compat/llm_client.rs:160-249` | send_with_retry | 初始请求重试逻辑 |
| `foundation/llm/src/client/openai_compat/stream.rs:1-51` | SSE DTO | JSON 反序列化，已正确跳过解析错误 |
| `agent/agent-core/src/agent/react/think_node.rs:310-312` | think_node | LLM 错误直接传播为 GraphError |
| `agent/tool/tool-workflow/src/backend.rs:268-287` | finalize_output | Err + empty slot → BackendError |

## 相关实例

| Instance | Workflow | 失败 Agent | 模型 | 运行时长 |
|----------|----------|-----------|------|----------|
| `luft-workflow_1784663573` | protocol-code-diff | fn-hunter | glm-5.2 | 585s |
| `luft-workflow_1785479543` | protocol-code-diff | analyze-bootstrap | minimax-m2.7 | 2146s |

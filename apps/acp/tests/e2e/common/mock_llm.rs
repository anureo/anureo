#![allow(dead_code)]

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub struct MockLlmServer {
    pub server: MockServer,
}

impl MockLlmServer {
    pub async fn start() -> Self {
        let server = MockServer::start().await;
        Self { server }
    }

    pub fn url(&self) -> String {
        self.server.uri()
    }

    pub async fn mount_default_chat_completion(&self) {
        // SSE (streaming) — mounted first, checked first (wiremock iterates from 0)
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(move |_req: &Request| {
                let body = std::str::from_utf8(&_req.body).unwrap_or("");
                body.contains(r#""stream":true"#)
            })
            .respond_with(Self::sse_response(&Self::sse_text_chunks("ok")))
            .mount(&self.server)
            .await;
        // JSON (non-streaming) fallback — mounted second, checked second
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(Self::simple_text("ok")))
            .mount(&self.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "object": "list",
                "data": [{
                    "id": "mock-model",
                    "object": "model",
                    "created": 1_700_000_000_u64,
                    "owned_by": "mock"
                }]
            })))
            .mount(&self.server)
            .await;
    }

    pub async fn expect_chat_completion_sse(
        &self,
        sse_body: &str,
        up_to_n_times: u64,
        delay: Option<std::time::Duration>,
    ) {
        let mut tpl = Self::sse_response(sse_body);
        if let Some(d) = delay {
            tpl = tpl.set_delay(d);
        }
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(move |req: &Request| {
                let body = std::str::from_utf8(&req.body).unwrap_or("");
                body.contains(r#""stream":true"#)
            })
            .respond_with(tpl)
            .up_to_n_times(up_to_n_times)
            .with_priority(4)
            .mount(&self.server)
            .await;
    }

    /// 为同一条 streaming endpoint 按请求顺序提供响应。
    ///
    /// 不要用多个 `up_to_n_times(1)` mock 模拟序列：wiremock 会为一次请求
    /// 评估所有匹配 mock，导致后续 mock 的匹配额度也可能被消耗。本 responder
    /// 把队列放在一个 mock 中，确保每次请求只弹出一个响应。后台 review 的
    /// streaming 请求始终使用默认响应，避免插入主 agent 的调用序列。
    pub async fn expect_chat_completion_sse_sequence(&self, sse_bodies: Vec<String>) {
        let responses = sse_bodies
            .iter()
            .map(|body| Self::sse_response(body))
            .collect();
        self.mount_chat_completion_sse_sequence(responses).await;
    }

    /// 与 [`Self::expect_chat_completion_sse_sequence`] 相同，但允许为每个
    /// 主 agent 响应设置独立延迟；用于控制面与 in-flight turn 的竞态测试。
    pub async fn expect_chat_completion_sse_sequence_with_delays(
        &self,
        sse_bodies: Vec<(String, Option<std::time::Duration>)>,
    ) {
        let responses = sse_bodies
            .into_iter()
            .map(|(body, delay)| {
                let mut response = Self::sse_response(&body);
                if let Some(delay) = delay {
                    response = response.set_delay(delay);
                }
                response
            })
            .collect();
        self.mount_chat_completion_sse_sequence(responses).await;
    }

    async fn mount_chat_completion_sse_sequence(&self, responses: VecDeque<ResponseTemplate>) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(move |req: &Request| {
                let body = std::str::from_utf8(&req.body).unwrap_or("");
                body.contains(r#""stream":true"#)
                    && !body.contains("Here is the conversation to review:")
            })
            .respond_with(SequentialSseResponder::new(responses))
            .with_priority(1)
            .mount(&self.server)
            .await;
    }

    pub fn simple_text(content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "created": 1_700_000_000_u64,
            "model": "mock-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": content},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
        })
    }

    fn sse_response(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "text/event-stream")
            .set_body_raw(body, "")
    }

    pub fn sse_text_chunks(content: &str) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        writeln!(out, "data: {}", serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
        })).ok();
        out.push('\n');
        writeln!(
            out,
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 1_700_000_000_u64,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]
            })
        )
        .ok();
        out.push('\n');
        writeln!(
            out,
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 1_700_000_000_u64,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]
            })
        )
        .ok();
        out.push('\n');
        writeln!(out, "data: [DONE]").ok();
        out
    }

    /// SSE stream with usage data attached to the final chunk (OpenAI-style).
    /// Use this to exercise the `_meta.token_usage` extension in usage_update notifications.
    #[allow(dead_code)]
    pub fn sse_text_chunks_with_usage(
        content: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
        total_tokens: u32,
        cached_tokens: Option<u32>,
    ) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        writeln!(out, "data: {}", serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": ""}, "finish_reason": null}]
        })).ok();
        out.push('\n');
        writeln!(
            out,
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 1_700_000_000_u64,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {"content": content}, "finish_reason": null}]
            })
        )
        .ok();
        out.push('\n');
        let mut usage_obj = serde_json::json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": total_tokens,
        });
        if let Some(c) = cached_tokens {
            let mut details = serde_json::Map::new();
            details.insert("cached_tokens".to_string(), serde_json::json!(c));
            usage_obj["prompt_tokens_details"] = serde_json::Value::Object(details);
        }
        writeln!(
            out,
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 1_700_000_000_u64,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": usage_obj,
            })
        )
        .ok();
        out.push('\n');
        writeln!(out, "data: [DONE]").ok();
        out
    }

    #[allow(dead_code)]
    pub fn sse_tool_call_chunks(tool_name: &str, arguments_json: &str) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        writeln!(out, "data: {}", serde_json::json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "created": 1_700_000_000_u64,
            "model": "mock-model",
            "choices": [{"index": 0, "delta": {
                "role": "assistant", "content": null,
                "tool_calls": [{"index": 0, "id": "call_mock", "type": "function", "function": {"name": tool_name, "arguments": arguments_json}}]
            }, "finish_reason": null}]
        })).ok();
        out.push('\n');
        writeln!(
            out,
            "data: {}",
            serde_json::json!({
                "id": "chatcmpl-mock",
                "object": "chat.completion.chunk",
                "created": 1_700_000_000_u64,
                "model": "mock-model",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]
            })
        )
        .ok();
        out.push('\n');
        writeln!(out, "data: [DONE]").ok();
        out
    }
}

struct SequentialSseResponder {
    responses: Arc<Mutex<VecDeque<ResponseTemplate>>>,
}

impl SequentialSseResponder {
    fn new(responses: VecDeque<ResponseTemplate>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses)),
        }
    }
}

impl Respond for SequentialSseResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.responses
            .lock()
            .expect("sequential mock response queue poisoned")
            .pop_front()
            .unwrap_or_else(|| {
                ResponseTemplate::new(500).set_body_string(
                    "unexpected extra streaming chat-completion request after scripted sequence",
                )
            })
    }
}

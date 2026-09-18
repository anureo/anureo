//! Tool execution engine for the Act node.
//!
//! [`ToolCallExecutor`] is fully self-contained — it owns all execution state
//! derived from `RunContext` and requires no lifetime parameters.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::state::{ReActState, ToolResult};
use crate::tool_output_normalizer::{normalize_tool_output, NormalizationConfig, ToolOutputHint};
use checkpoint::uuid6;
use anureo_graph_core::GraphError;
use anureo_graph_core::{run_cancellable, RunContext};
use anureo_llm::ToolCall;
use stream_event::{StreamEvent, StreamMode};
use tool_core::active_operation::RunCancellation;
use tool_core::{ToolCallContent, ToolCallContext, ToolRegistryLocked, ToolSourceError};

use super::act_utils::{
    parse_tool_arguments, step_progress_payload, truncate_for_log, DEFAULT_EXECUTION_ERROR_TEMPLATE,
};

// ────────────────────── ToolCallExecutor ──────────────────────

/// Fully self-contained tool execution engine.
///
/// Owns all state derived from `RunContext` — no lifetime parameters, no
/// external borrows. Construct via [`ToolCallExecutor::new`], then call
/// [`ToolCallExecutor::execute`].
pub(crate) struct ToolCallExecutor {
    // ── tool registry ──
    tools: Arc<ToolRegistryLocked>,

    // ── derived from RunContext (was ActExecCtx) ──
    messages: Vec<anureo_llm::Message>,
    thread_id: Option<String>,
    user_id: Option<String>,
    depth: u32,
    run_cancellation: Option<RunCancellation>,
    acp_session_id: Option<String>,
    tools_mode: bool,
    stream_tx: Option<mpsc::Sender<StreamEvent<ReActState>>>,
    cancellation: Option<tokio_util::sync::CancellationToken>,

    // ── derived from hints (was OutcomeNormalizer) ──
    hints: HashMap<String, ToolOutputHint>,
    used_chars: AtomicUsize,

    // ── cross-layer adapter ──
    any_stream_event_sender: Option<Arc<dyn Fn(crate::run::TypedAnyStreamEvent) + Send + Sync>>,
}

/// Internal result of normalizing one tool call.
struct NormalizedOutcome {
    result: ToolResult,
    display_text: String,
    summary: String,
    raw_text: Option<String>,
}

impl ToolCallExecutor {
    /// Construct from a `RunContext`, loading tool hints from the registry.
    pub async fn new(
        tools: Arc<ToolRegistryLocked>,
        messages: Vec<anureo_llm::Message>,
        run_ctx: &RunContext<ReActState>,
        run_cancellation: Option<RunCancellation>,
        any_stream_event_sender: Option<Arc<dyn Fn(crate::run::TypedAnyStreamEvent) + Send + Sync>>,
    ) -> Self {
        let tools_mode = run_ctx.stream_mode.contains(&StreamMode::Tools)
            || run_ctx.stream_mode.contains(&StreamMode::Debug);

        let hints = {
            let specs = tools.list_tools().await;
            specs
                .into_iter()
                .filter_map(|spec| spec.output_hint.map(|hint| (spec.name, hint)))
                .collect()
        };

        Self {
            tools,
            messages,
            thread_id: run_ctx.config.thread_id.clone(),
            user_id: run_ctx.config.user_id.clone(),
            depth: run_ctx.config.depth.unwrap_or(0),
            run_cancellation,
            acp_session_id: run_ctx.config.acp_session_id.clone(),
            tools_mode,
            stream_tx: run_ctx.stream_tx.clone(),
            cancellation: run_ctx.cancellation.clone(),
            hints,
            used_chars: AtomicUsize::new(0),
            any_stream_event_sender,
        }
    }

    // ──────────── batch execution ────────────

    /// Execute all tool calls in sequence, returning normalized results with
    /// backfilled call_ids.
    ///
    /// If a `ToolCall.id` is empty, a fallback id is generated and written back
    /// to **both** the ToolCall and the ToolResult to keep them in sync.
    pub async fn execute(
        &self,
        tool_calls: &mut [ToolCall],
    ) -> Result<Vec<ToolResult>, GraphError> {
        if self.is_cancelled() {
            return Err(GraphError::Cancelled);
        }

        let mut tool_results = Vec::with_capacity(tool_calls.len());
        for tc in tool_calls.iter() {
            if self.is_cancelled() {
                return Err(GraphError::Cancelled);
            }
            let outcome = self.execute_one(tc).await?;
            tool_results.push(outcome.result);
        }

        backfill_call_ids(tool_calls, &mut tool_results);

        Ok(tool_results)
    }

    // ──────────── single tool execution ────────────

    async fn execute_one(&self, tc: &ToolCall) -> Result<NormalizedOutcome, GraphError> {
        debug!(
            call_id = ?tc.id,
            tool_name = %tc.name,
            args_len = tc.arguments.len(),
            "act:call"
        );

        // ① parse arguments
        let args = match parse_tool_arguments(&tc.name, &tc.arguments) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    tool_name = %tc.name,
                    call_id = ?tc.id,
                    error = %e,
                    "invalid tool arguments; returning error to LLM for self-correction"
                );
                let outcome =
                    self.normalize(tc, &serde_json::json!({}), &e.self_correct_hint(), true);
                self.emit_end(tc, &outcome, true);
                return Ok(outcome);
            }
        };

        // ② empty name check
        if tc.name.trim().is_empty() {
            let hint = "你提交了一个空的工具名，请从可用工具列表中选择一个有效工具。";
            warn!(call_id = ?tc.id, "ToolCall with empty name received; skipping and hinting LLM to retry");
            let outcome = self.normalize(tc, &args, hint, false);
            self.emit_end(tc, &outcome, false);
            return Ok(outcome);
        }

        // ③ run tool (build ctx → emit_start → call_tool → cancellation check)
        let result = self.run_tool(tc, &args).await?;

        // ④ normalize result + emit end
        let outcome = match result {
            Ok(content) => {
                let raw_text = Self::extract_raw_text(&content);
                trace!(
                    tool = %tc.name,
                    result_len = raw_text.len(),
                    result_preview = %truncate_for_log(&raw_text, 200),
                    "Tool returned"
                );
                let outcome = self.normalize(tc, &args, &raw_text, false);
                self.emit_end(tc, &outcome, false);
                outcome
            }
            Err(e) => {
                warn!(tool = %tc.name, error = %e, "Tool call failed");
                let error_text = DEFAULT_EXECUTION_ERROR_TEMPLATE
                    .replace("{tool_name}", &tc.name)
                    .replace("{tool_kwargs}", &args.to_string())
                    .replace("{error}", &e.to_string());
                let outcome = self.normalize(tc, &args, &error_text, true);
                self.emit_end(tc, &outcome, true);
                outcome
            }
        };

        Ok(outcome)
    }

    // ──────────── raw tool invocation (was ToolRunner) ────────────

    async fn run_tool(
        &self,
        tc: &ToolCall,
        args: &serde_json::Value,
    ) -> Result<Result<ToolCallContent, ToolSourceError>, GraphError> {
        let tool_ctx = self.build_tool_ctx(tc);
        self.emit_start(tc);

        debug!(tool = %tc.name, args = ?args, "Calling tool");

        let result = match self.cancellation.as_ref() {
            Some(token) => {
                run_cancellable(
                    self.tools
                        .call_tool(&tc.name, args.clone(), Some(&tool_ctx)),
                    Some(token),
                )
                .await?
            }
            None => {
                self.tools
                    .call_tool(&tc.name, args.clone(), Some(&tool_ctx))
                    .await
            }
        };

        if self.is_cancelled() {
            return Err(GraphError::Cancelled);
        }

        Ok(result)
    }

    fn build_tool_ctx(&self, tc: &ToolCall) -> ToolCallContext {
        let any_stream_event_sender = self
            .any_stream_event_sender
            .as_ref()
            .map(adapt_stream_sender);

        ToolCallContext {
            recent_messages: self.messages.clone(),
            thread_id: self.thread_id.clone(),
            user_id: self.user_id.clone(),
            depth: self.depth,
            run_cancellation: self.run_cancellation.clone(),
            any_stream_event_sender,
            acp_session_id: self.acp_session_id.clone(),
            tool_call_id: tc.id.clone(),
        }
    }

    // ──────────── normalization (was OutcomeNormalizer) ────────────

    fn normalize(
        &self,
        tc: &ToolCall,
        args: &serde_json::Value,
        text: &str,
        is_error: bool,
    ) -> NormalizedOutcome {
        let prev = self.used_chars.load(Ordering::Relaxed);
        let normalized = normalize_tool_output(
            &tc.name,
            args,
            text,
            is_error,
            self.hints.get(&tc.name),
            NormalizationConfig::runtime_default().with_used_observation_chars(prev),
        );
        self.used_chars
            .store(prev + normalized.observation_chars, Ordering::Relaxed);

        let summary = truncate_for_log(&normalized.display_text, 200);
        let display_text = normalized.display_text.clone();
        let raw_text = if display_text != text {
            Some(text.to_string())
        } else {
            None
        };

        NormalizedOutcome {
            result: ToolResult::from(normalized)
                .with_call_id(tc.id.clone())
                .with_name(Some(tc.name.clone()))
                .with_is_error(is_error),
            display_text,
            summary,
            raw_text,
        }
    }

    fn extract_raw_text(content: &ToolCallContent) -> String {
        match content {
            ToolCallContent::Text(_) => content.clone().into_text(),
            _ => serde_json::to_string(content).unwrap_or_else(|_| content.clone().into_text()),
        }
    }

    // ──────────── event emission (was ToolEventEmitter) ────────────

    fn emit_start(&self, tc: &ToolCall) {
        if self.tools_mode {
            if let Some(tx) = &self.stream_tx {
                let _ = tx.try_send(StreamEvent::ToolStart {
                    call_id: tc.id.clone(),
                    name: tc.name.clone(),
                });
            }
        }
    }

    fn emit_end(&self, tc: &ToolCall, outcome: &NormalizedOutcome, is_error: bool) {
        if self.tools_mode {
            if let Some(tx) = &self.stream_tx {
                let _ = tx.try_send(StreamEvent::ToolEnd {
                    call_id: tc.id.clone(),
                    name: tc.name.clone(),
                    result: outcome.display_text.clone(),
                    is_error,
                    raw_result: outcome.raw_text.clone(),
                });
            }
        } else if self.stream_tx.is_some() {
            let call_id = tc.id.as_deref().unwrap_or("");
            let payload = step_progress_payload(&tc.name, call_id, &outcome.summary);
            if let Some(tx) = &self.stream_tx {
                let _ = tx.try_send(StreamEvent::Custom(payload));
            }
        }
    }

    // ──────────── misc ────────────

    fn is_cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(tokio_util::sync::CancellationToken::is_cancelled)
    }
}

// ────────────────────── helpers ──────────────────────

/// Adapts a typed event sender to the type-erased `serde_json::Value` interface
/// expected by `ToolCallContext`.
fn adapt_stream_sender(
    sender: &Arc<dyn Fn(crate::run::TypedAnyStreamEvent) + Send + Sync>,
) -> Arc<dyn Fn(serde_json::Value) + Send + Sync> {
    let sender = sender.clone();
    Arc::new(move |value: serde_json::Value| {
        if let Ok(ev) = serde_json::from_value::<crate::run::TypedAnyStreamEvent>(value) {
            sender(ev);
        }
    })
}

/// Ensures each [`ToolResult`] has a non-empty `call_id`, syncing the id to
/// both sides (ToolCall and ToolResult) when a fallback is generated.
fn backfill_call_ids(tool_calls: &mut [ToolCall], tool_results: &mut [ToolResult]) {
    for (tc, tr) in tool_calls.iter_mut().zip(tool_results.iter_mut()) {
        let needs_fill_tr = tr.call_id.as_deref().is_none_or(|s| s.is_empty());
        let needs_fill_tc = tc.id.as_deref().is_none_or(|s| s.is_empty());
        if !needs_fill_tr && !needs_fill_tc {
            continue;
        }
        // Determine the canonical id (prefer existing non-empty)
        let canonical_id = if !needs_fill_tc {
            tc.id.clone().unwrap()
        } else if !needs_fill_tr {
            tr.call_id.clone().unwrap()
        } else {
            let id = format!("call_{}", uuid6());
            warn!(
                tool_name = %tc.name,
                "both ToolCall.id and ToolResult.call_id empty; generated fallback id"
            );
            id
        };
        // Write to BOTH sides
        if needs_fill_tc {
            tc.id = Some(canonical_id.clone());
        }
        if needs_fill_tr {
            tr.call_id = Some(canonical_id);
        }
    }
    let paired = tool_calls.len().min(tool_results.len());
    for tr in tool_results.iter_mut().skip(paired) {
        let needs_fill = tr.call_id.as_deref().is_none_or(|s| s.is_empty());
        if needs_fill {
            tr.call_id = Some(format!("call_{}", uuid6()));
            warn!("unpaired ToolResult missing call_id; generated fallback id");
        }
    }
    if tool_results.len() != tool_calls.len() {
        warn!(
            tool_calls_len = tool_calls.len(),
            tool_results_len = tool_results.len(),
            "tool_calls and tool_results length mismatch"
        );
    }
}

// ────────────────────── tests ──────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_raw_text_text_variant() {
        let content = ToolCallContent::Text("hello\nworld".into());
        assert_eq!(ToolCallExecutor::extract_raw_text(&content), "hello\nworld");
    }

    #[test]
    fn backfill_call_ids_from_toolcall_id() {
        let mut tcs = vec![ToolCall {
            id: Some("call-1".into()),
            name: "get_time".into(),
            arguments: "{}".into(),
        }];
        let mut results = vec![ToolResult::simple(
            None,
            Some("get_time".into()),
            "ok".into(),
            false,
        )];
        backfill_call_ids(&mut tcs, &mut results);
        assert_eq!(results[0].call_id.as_deref(), Some("call-1"));
    }

    #[test]
    fn backfill_call_ids_generates_when_both_missing() {
        let mut tcs = vec![ToolCall {
            id: None,
            name: "x".into(),
            arguments: "{}".into(),
        }];
        let mut results = vec![ToolResult::simple(None, None, "y".into(), false)];
        backfill_call_ids(&mut tcs, &mut results);
        assert!(results[0].call_id.as_deref().is_some_and(|s| !s.is_empty()));
        // Both sides should get the same id
        assert_eq!(tcs[0].id, results[0].call_id);
    }
}

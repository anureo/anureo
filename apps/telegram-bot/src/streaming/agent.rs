//! Agent execution with streaming support
//!
//! Provides functions for running anureo agent with real-time streaming.

use crate::config::Settings;
use crate::error::{BotError, Result};
use crate::streaming::event_mapper::StreamEventMapper;
use crate::streaming::message_handler::StreamCommand;
use crate::traits::{AgentRunContext, MessageSender};
use agent::run::{build_react_config, run_agent_from_config, RunCmd, RunParams};
use agent::run::{RunCompletion, RunOptions};
use tool_extensions::set_current_chat_id;

use anureo_llm::message::UserContent;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub async fn run_anureo_agent_streaming(
    message: &str,
    chat_id: i64,
    sender: Arc<dyn MessageSender>,
    context: AgentRunContext,
    settings: &Settings,
    force_compact: bool,
) -> Result<String> {
    tracing::info!("Running anureo agent (streaming) for chat {}", chat_id);

    let thread_id = format!("telegram_{}", chat_id);

    set_current_chat_id(chat_id);

    let (tx, rx) = mpsc::channel::<StreamCommand>(100);

    let model_for_run = context.model_override.clone();
    let handler_sender = sender.clone();
    let handler_settings = settings.streaming.clone();
    let handler_task = tokio::spawn(async move {
        crate::streaming::message_handler::stream_message_handler_with_context(
            rx,
            handler_sender,
            chat_id,
            context,
            handler_settings,
        )
        .await
    });

    let opts = RunOptions {
        message: UserContent::Text(message.to_string()),
        thread_id: Some(thread_id),
        working_folder: Some(PathBuf::from(
            std::env::var("WORKING_DIR").unwrap_or_else(|_| ".".to_string()),
        )),
        session_id: None,
        agent: None,
        verbose: false,
        verbose_level: 0,
        got_adaptive: false,
        display_max_len: 2000,
        output_json: false,
        model: model_for_run,
        provider: None,
        base_url: None,
        api_key: None,
        provider_type: None,
        mcp_config_path: None,
        cancellation: None,
        output_timestamp: false,
        dry_run: false,
        debug_llm: false,
        any_stream_event_sender: None,
        bash_executor: None,
        extra_tools: None,
        default_extra_tools_provider: Some(tool_workflow::default_workflow_tool_provider()),
        acp_session_id: None,
        async_agent_registry: None,
        subagent_sink: None,
        force_compact,
        chat_id: Some(chat_id),
        worktree: false,
        goal_mode: false,
        acp_mcp_servers: None,

        acp_mcp_sources: None,
        effort: None,
        tier: None,
    };

    let mapper = StreamEventMapper::new(tx.clone());
    let on_event = mapper.boxed_callback();

    let (config, _, _) = build_react_config(&opts);
    let result = run_agent_from_config(
        &config,
        &RunCmd::React,
        RunParams {
            message: opts.message.clone(),
            verbose: opts.verbose,
            cancellation: opts.cancellation.clone(),
            any_stream_event_sender: opts.any_stream_event_sender.clone(),
            llm_override: None,
            thread_id: opts.thread_id.clone(),
        },
        Some(on_event),
    )
    .await;

    let completion = result?;

    if let Err(send_error) = tx
        .send(crate::streaming::message_handler::StreamCommand::Flush)
        .await
    {
        tracing::error!("Failed to send Flush to stream handler: {}", send_error);
    }
    let final_text = handler_task.await.unwrap_or_default();

    match completion {
        RunCompletion::Finished(agent_result) => {
            let text = if final_text.trim().is_empty() {
                agent_result.reply
            } else {
                final_text
            };
            Ok(text)
        }
        RunCompletion::Cancelled => Err(BotError::Agent("Agent run cancelled".to_string())),
    }
}

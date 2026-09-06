//! Message processing pipeline: download dir, text flow (commands -> mention gate -> agent), media.
//!
//! Keeps [`crate::router::handle_message_with_deps`] thin by centralizing common-message logic here.

mod agent_orchestrator;

use crate::command::{try_handle_model_command_input, CommandContext, CommandDispatcher};
use crate::download::{is_bot_mentioned, is_reply_to_bot};
use crate::error::BotError;
use crate::formatting::telegram::markdown_notice;
use crate::handler_deps::HandlerDeps;
use crate::traits::AgentRunContext;
use agent::commands as anureo_command;
use teloxide::types::Message;

pub struct MessageContext<'a> {
    pub deps: &'a HandlerDeps,
    pub msg: &'a Message,
}

impl<'a> MessageContext<'a> {
    pub fn new(deps: &'a HandlerDeps, msg: &'a Message) -> Self {
        Self { deps, msg }
    }

    pub fn chat_id(&self) -> i64 {
        self.msg.chat.id.0
    }

    pub fn message_id(&self) -> i32 {
        self.msg.id.0
    }
}

pub async fn ensure_download_dir(deps: &HandlerDeps) {
    if let Err(e) = tokio::fs::create_dir_all(&deps.settings.download_dir).await {
        tracing::error!("Failed to create download directory: {}", e);
    }
}

fn strip_bot_mention(text: &str, bot_username: &str) -> String {
    if bot_username.is_empty() {
        return text.to_string();
    }
    let mention = format!("@{} ", bot_username);
    text.replace(&mention, "")
        .replace(&format!("@{}", bot_username), "")
}

fn build_prompt_with_reply(msg: &Message, clean_text: &str) -> String {
    if let Some(replied_msg) = msg.reply_to_message() {
        if let Some(replied_text) = replied_msg.text() {
            return format!(
                "[Replying to this message]:\n{}\n\n[User's reply]:\n{}",
                replied_text, clean_text
            );
        }
    }
    clean_text.to_string()
}

use agent_orchestrator::run_agent_for_chat;

async fn handle_media_attachments(ctx: &MessageContext<'_>) -> Result<(), BotError> {
    let chat_id = ctx.chat_id();
    let message_id = ctx.message_id();

    if let Some(photos) = ctx.msg.photo() {
        match ctx
            .deps
            .downloader
            .download_photo(chat_id, message_id, photos)
            .await
        {
            Ok((_path, metadata)) => {
                tracing::info!(?metadata, "Photo downloaded");
            }
            Err(e) => {
                tracing::error!("Failed to download photo: {}", e);
            }
        }
    }

    if let Some(doc) = ctx.msg.document() {
        match ctx
            .deps
            .downloader
            .download_document(chat_id, message_id, doc)
            .await
        {
            Ok((_path, metadata)) => {
                tracing::info!(?metadata, "Document downloaded");
            }
            Err(e) => {
                tracing::error!("Failed to download document: {}", e);
            }
        }
    }

    if let Some(video) = ctx.msg.video() {
        match ctx
            .deps
            .downloader
            .download_video(chat_id, message_id, video)
            .await
        {
            Ok((_path, metadata)) => {
                tracing::info!(?metadata, "Video downloaded");
            }
            Err(e) => {
                tracing::error!("Failed to download video: {}", e);
            }
        }
    }

    Ok(())
}

pub async fn handle_common_message(ctx: &MessageContext<'_>) -> Result<(), BotError> {
    ensure_download_dir(ctx.deps).await;

    if let Some(text) = ctx.msg.text() {
        tracing::info!("Text: {}", text);

        // 1. Try anureo core commands (/reset, /clear, /new)
        if let Some(cmd) = anureo_command::parse(text) {
            match cmd {
                anureo_command::Command::ResetContext => {
                    let thread_id = format!("telegram_{}", ctx.chat_id());
                    match ctx.deps.session.reset(&thread_id).await {
                        Ok(count) => {
                            let message = markdown_notice(
                                "Session Reset",
                                &format!("🔄 Deleted {} checkpoints.", count),
                            );
                            ctx.deps
                                .sender
                                .send_formatted(ctx.chat_id(), &message)
                                .await?;
                        }
                        Err(error) => {
                            tracing::error!("Failed to reset session: {}", error);
                            let message = markdown_notice("Reset Failed", &format!("❌ {}", error));
                            ctx.deps
                                .sender
                                .send_formatted(ctx.chat_id(), &message)
                                .await?;
                        }
                    }
                    return Ok(());
                }
                anureo_command::Command::Compact { instructions } => {
                    let _thread_id = format!("telegram_{}", ctx.chat_id());
                    let compact_prompt = instructions.as_deref().unwrap_or(
                        "Compress the conversation history, keeping the most important context.",
                    );

                    ctx.deps
                        .sender
                        .send_text(ctx.chat_id(), "⏳ 正在压缩上下文...")
                        .await?;

                    // Run agent with force_compact=true
                    let chat_id = ctx.chat_id();
                    let message_id = ctx.message_id();
                    let sender = ctx.deps.sender.clone();
                    let settings = ctx.deps.settings.clone();
                    let model = ctx.deps.model_selection.current_model(chat_id)?;

                    let run_context = AgentRunContext {
                        user_message_id: Some(message_id),
                        ack_message_id: None,
                        model_override: Some(model),
                    };

                    match crate::streaming::run_anureo_agent_streaming(
                        compact_prompt,
                        chat_id,
                        sender,
                        run_context,
                        &settings,
                        true, // force_compact
                    )
                    .await
                    {
                        Ok(reply) => {
                            let msg = if reply.trim().is_empty() {
                                "✅ 上下文已压缩".to_string()
                            } else {
                                reply
                            };
                            ctx.deps.sender.send_text(ctx.chat_id(), &msg).await?;
                        }
                        Err(e) => {
                            tracing::error!("Compact failed: {}", e);
                            ctx.deps
                                .sender
                                .send_text(ctx.chat_id(), &format!("❌ 压缩失败: {}", e))
                                .await?;
                        }
                    }
                    return Ok(());
                }
                anureo_command::Command::Summarize => {
                    let _thread_id = format!("telegram_{}", ctx.chat_id());

                    ctx.deps
                        .sender
                        .send_text(ctx.chat_id(), "⏳ 正在生成摘要...")
                        .await?;

                    // Run agent with force_compact=true for summarize too
                    let chat_id = ctx.chat_id();
                    let message_id = ctx.message_id();
                    let sender = ctx.deps.sender.clone();
                    let settings = ctx.deps.settings.clone();
                    let model = ctx.deps.model_selection.current_model(chat_id)?;

                    let run_context = AgentRunContext {
                        user_message_id: Some(message_id),
                        ack_message_id: None,
                        model_override: Some(model),
                    };

                    match crate::streaming::run_anureo_agent_streaming(
                        "Summarize the conversation so far.",
                        chat_id,
                        sender,
                        run_context,
                        &settings,
                        true,
                    )
                    .await
                    {
                        Ok(reply) => {
                            let msg = if reply.trim().is_empty() {
                                "✅ 摘要已生成".to_string()
                            } else {
                                reply
                            };
                            ctx.deps.sender.send_text(ctx.chat_id(), &msg).await?;
                        }
                        Err(e) => {
                            tracing::error!("Summarize failed: {}", e);
                            ctx.deps
                                .sender
                                .send_text(ctx.chat_id(), &format!("❌ 摘要失败: {}", e))
                                .await?;
                        }
                    }
                    return Ok(());
                }
                anureo_command::Command::Models { .. } | anureo_command::Command::ModelsUse { .. } => {
                    // fall through to existing model handling below
                }
                anureo_command::Command::Goal { .. } => {
                    // /goal subcommands (P5/R3) target the session goal
                    // runtime, which is not wired into the telegram pipeline.
                    // Reply explicitly instead of leaking the command text to
                    // the LLM as a normal message.
                    ctx.deps
                        .sender
                        .send_text(
                            ctx.chat_id(),
                            "⚠️ /goal 命令在 Telegram 端暂不支持，请在 ACP/REPL 客户端使用。",
                        )
                        .await?;
                    return Ok(());
                }
                anureo_command::Command::ReviewSkill { .. } => {
                    // not supported in telegram bot; fall through
                }
                anureo_command::Command::Help { .. }
                | anureo_command::Command::Tools
                | anureo_command::Command::Model { .. }
                | anureo_command::Command::Resume { .. }
                | anureo_command::Command::Undo
                | anureo_command::Command::Retry
                | anureo_command::Command::History { .. }
                | anureo_command::Command::Exit => {
                    // not supported in telegram bot; fall through
                }
            }
        }

        // 2. Bot-specific commands (/status, /model via dispatcher)
        let cmd_ctx = CommandContext {
            chat_id: ctx.chat_id(),
            deps: ctx.deps,
        };
        let dispatcher = CommandDispatcher::default();

        if let Some(result) = dispatcher.try_dispatch(&cmd_ctx, text).await {
            return result;
        }

        // 3. /models handling (existing)
        if try_handle_model_command_input(&cmd_ctx, text).await? {
            return Ok(());
        }

        let is_mentioned = is_bot_mentioned(ctx.msg, &ctx.deps.bot_username);
        let is_reply = is_reply_to_bot(ctx.msg, &ctx.deps.bot_username);
        let should_respond = is_mentioned || is_reply;

        tracing::debug!(
            bot_username = %ctx.deps.bot_username,
            only_respond_when_mentioned = ctx.deps.settings.only_respond_when_mentioned,
            is_mentioned,
            is_reply,
            should_respond,
            text = %text,
            "Evaluated message routing"
        );

        if ctx.deps.settings.only_respond_when_mentioned && !should_respond {
            tracing::debug!(
                bot_username = %ctx.deps.bot_username,
                only_respond_when_mentioned = ctx.deps.settings.only_respond_when_mentioned,
                is_mentioned,
                is_reply,
                text = %text,
                "Ignoring message (bot not mentioned and not a reply)"
            );
            return Ok(());
        }

        let clean_text = strip_bot_mention(text, &ctx.deps.bot_username);
        tracing::debug!(
            original_text = %text,
            clean_text = %clean_text,
            is_reply,
            "Prepared clean text for agent"
        );
        let prompt = build_prompt_with_reply(ctx.msg, &clean_text);
        tracing::info!("Agent prompt:\n{}", prompt);

        run_agent_for_chat(ctx, &prompt).await?;
    }

    handle_media_attachments(ctx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_bot_mention_removes_mention() {
        assert_eq!(
            strip_bot_mention("Hello @MyBot how are you?", "MyBot"),
            "Hello how are you?"
        );
    }

    #[test]
    fn strip_bot_mention_removes_mention_without_trailing_space() {
        assert_eq!(strip_bot_mention("Hello @MyBot", "MyBot"), "Hello ");
    }

    #[test]
    fn strip_bot_mention_with_trailing_space() {
        // "@MyBot " is replaced first (includes trailing space), leaving "Hello "
        assert_eq!(
            strip_bot_mention("Hello @MyBot world", "MyBot"),
            "Hello world"
        );
    }

    #[test]
    fn strip_bot_mention_empty_username() {
        assert_eq!(strip_bot_mention("Hello @MyBot", ""), "Hello @MyBot");
    }

    #[test]
    fn strip_bot_mention_no_mention_present() {
        assert_eq!(strip_bot_mention("Hello world", "MyBot"), "Hello world");
    }
}

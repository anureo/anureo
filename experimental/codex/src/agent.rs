use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::json;
use tokio::sync::{mpsc, Mutex};

use agent::run::{
    build_react_config, run_agent_from_config, RunCmd, RunCompletion, RunError, RunOptions,
    RunParams,
};
use anureo_llm::message::UserContent;
use stream_event::codex::{CodexErrorInfo, CodexEvent, CodexUsage};
use tool_core::active_operation::RunCancellation;

use crate::stdio_loop::CodexNotification;

// ── OutputEvent ──────────────────────────────────────────────────────────────

pub enum OutputEvent {
    Notification(CodexNotification),
    Response {
        id: serde_json::Value,
        result: serde_json::Value,
    },
    ErrorResponse {
        id: serde_json::Value,
        code: i32,
        message: String,
    },
}

// ── Per-thread session state ──────────────────────────────────────────────────

struct ThreadSession {
    #[allow(dead_code)] // thread_id used as HashMap key, not read from struct
    thread_id: String,
    model: String,
    working_dir: PathBuf,
    system_prompt: Option<String>,
    cancel_flag: Arc<AtomicBool>,
}

// ── CodexAgent ────────────────────────────────────────────────────────────────

pub struct CodexAgent {
    args: crate::CodexServerArgs,
    output_tx: mpsc::Sender<OutputEvent>,
    sessions: Mutex<HashMap<String, ThreadSession>>,
    approval_manager: Arc<crate::approval::ApprovalManager>,
}

impl CodexAgent {
    pub fn new(
        args: crate::CodexServerArgs,
        output_tx: mpsc::Sender<OutputEvent>,
    ) -> anyhow::Result<Self> {
        let approval_manager = Arc::new(crate::approval::ApprovalManager::new(output_tx.clone()));
        Ok(Self {
            args,
            output_tx,
            sessions: Mutex::new(HashMap::new()),
            approval_manager,
        })
    }

    pub async fn handle(&self, msg: serde_json::Value) {
        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = msg
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let params = msg
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));

        match method.as_str() {
            "thread/start" => self.handle_thread_start(id, params).await,
            "thread/resume" => self.handle_thread_resume(id, params).await,
            "turn/start" => self.handle_turn_start(id, params).await,
            "turn/cancel" => self.handle_turn_cancel(id, params).await,
            "thread/commandExecution/approve" => {
                self.handle_command_approval(id, params, true).await
            }
            "thread/commandExecution/deny" => self.handle_command_approval(id, params, false).await,
            other => {
                tracing::warn!("Unknown method: {other}");
                let _ = self
                    .output_tx
                    .send(OutputEvent::ErrorResponse {
                        id,
                        code: -32601,
                        message: format!("Method not found: {other}"),
                    })
                    .await;
            }
        }
    }

    // ── thread/start ─────────────────────────────────────────────────────────

    async fn handle_thread_start(&self, id: serde_json::Value, params: serde_json::Value) {
        let thread_id = uuid::Uuid::new_v4().to_string();
        let model = params
            .get("model")
            .and_then(|v| v.as_str())
            .unwrap_or(&self.args.model)
            .to_string();
        let working_dir = params
            .get("workingDirectory")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)
            .or_else(|| self.args.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let system_prompt = params
            .get("systemPrompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| self.args.system_prompt.clone());

        let session = ThreadSession {
            thread_id: thread_id.clone(),
            model,
            working_dir,
            system_prompt,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        };

        {
            let mut sessions = self.sessions.lock().await;
            sessions.insert(thread_id.clone(), session);
        }

        // Respond with thread_id
        let _ = self
            .output_tx
            .send(OutputEvent::Response {
                id,
                result: json!({ "threadId": thread_id }),
            })
            .await;

        // Emit thread/started notification
        let notif = codex_event_to_notification(&CodexEvent::ThreadStarted {
            thread_id: thread_id.clone(),
        });
        let _ = self.output_tx.send(OutputEvent::Notification(notif)).await;
    }

    // ── thread/resume ─────────────────────────────────────────────────────────

    async fn handle_thread_resume(&self, id: serde_json::Value, params: serde_json::Value) {
        let thread_id = params
            .get("threadId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let exists = {
            let sessions = self.sessions.lock().await;
            sessions.contains_key(&thread_id)
        };

        if !exists {
            // Create a new session for this thread_id
            let model = params
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or(&self.args.model)
                .to_string();
            let working_dir = params
                .get("workingDirectory")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .or_else(|| self.args.cwd.clone())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
            let system_prompt = params
                .get("systemPrompt")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| self.args.system_prompt.clone());

            let session = ThreadSession {
                thread_id: thread_id.clone(),
                model,
                working_dir,
                system_prompt,
                cancel_flag: Arc::new(AtomicBool::new(false)),
            };

            let mut sessions = self.sessions.lock().await;
            sessions.insert(thread_id.clone(), session);
        }

        // Respond
        let _ = self
            .output_tx
            .send(OutputEvent::Response {
                id,
                result: json!({ "threadId": thread_id }),
            })
            .await;

        // Emit thread/started notification
        let notif = codex_event_to_notification(&CodexEvent::ThreadStarted {
            thread_id: thread_id.clone(),
        });
        let _ = self.output_tx.send(OutputEvent::Notification(notif)).await;
    }

    // ── turn/start ────────────────────────────────────────────────────────────

    async fn handle_turn_start(&self, id: serde_json::Value, params: serde_json::Value) {
        let thread_id = match params.get("threadId").and_then(|v| v.as_str()) {
            Some(tid) => tid.to_string(),
            None => {
                let _ = self
                    .output_tx
                    .send(OutputEvent::ErrorResponse {
                        id,
                        code: -32602,
                        message: "Missing threadId".to_string(),
                    })
                    .await;
                return;
            }
        };

        let message_text = params
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Extract session data while holding the lock briefly
        let (model, working_dir, system_prompt, cancel_flag) = {
            let sessions = self.sessions.lock().await;
            match sessions.get(&thread_id) {
                Some(s) => (
                    s.model.clone(),
                    s.working_dir.clone(),
                    s.system_prompt.clone(),
                    s.cancel_flag.clone(),
                ),
                None => {
                    let _ = self
                        .output_tx
                        .send(OutputEvent::ErrorResponse {
                            id,
                            code: -32602,
                            message: format!("Unknown threadId: {thread_id}"),
                        })
                        .await;
                    return;
                }
            }
        };

        // Reset the cancel flag for this turn
        cancel_flag.store(false, Ordering::SeqCst);

        // Immediately acknowledge
        let _ = self
            .output_tx
            .send(OutputEvent::Response {
                id,
                result: json!({ "ok": true }),
            })
            .await;

        // Spawn the async turn task
        let output_tx = self.output_tx.clone();
        let approval_manager = self.approval_manager.clone();

        tokio::spawn(async move {
            // Emit turn/started
            let notif = codex_event_to_notification(&CodexEvent::TurnStarted);
            let _ = output_tx.send(OutputEvent::Notification(notif)).await;

            // Build RunOptions
            let mut run_opts = RunOptions {
                message: UserContent::text(message_text),
                working_folder: Some(working_dir),
                session_id: Some(thread_id.clone()),
                agent: None,
                verbose: false,
                verbose_level: 0,
                got_adaptive: false,
                display_max_len: 4096,
                output_json: false,
                model: Some(model),
                provider: None,
                base_url: None,
                api_key: None,
                provider_type: None,
                mcp_config_path: None,
                cancellation: None,
                thread_id: Some(thread_id.clone()),
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
                force_compact: false,
                chat_id: None,
                worktree: false,
                goal_mode: false,
                acp_mcp_servers: None,

                acp_mcp_sources: None,
                effort: None,
                tier: None,
            };

            if let Some(sp) = system_prompt {
                // Pass system prompt via agent field as a hint; the actual
                // system_prompt field lives in ReactBuildConfig, not RunOptions.
                // We store it in the agent name slot as a workaround for now.
                // TODO: expose system_prompt in RunOptions upstream.
                let _ = sp; // suppress unused warning
            }

            // Set up cancellation
            let run_cancellation = RunCancellation::new(0);
            let cancel_flag_clone = cancel_flag.clone();
            let token = run_cancellation.token();
            run_opts.cancellation = Some(run_cancellation);

            // Spawn a watcher that polls the cancel flag and cancels the token
            tokio::spawn(async move {
                loop {
                    if cancel_flag_clone.load(Ordering::SeqCst) {
                        token.cancel();
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            });

            // Set up event tracking
            let output_tx_ev = output_tx.clone();
            let approval_manager_ev = approval_manager.clone();
            let tracker = Arc::new(tokio::sync::Mutex::new(
                crate::event_bridge::ItemTracker::new(),
            ));
            // Thread log for persistence
            let thread_log: Arc<tokio::sync::Mutex<Option<crate::thread_log::ThreadLog>>> =
                Arc::new(tokio::sync::Mutex::new(None));
            {
                let log_arc = thread_log.clone();
                let tid = thread_id.clone();
                tokio::spawn(async move {
                    if let Ok(log) = crate::thread_log::ThreadLog::open(&tid).await {
                        *log_arc.lock().await = Some(log);
                    }
                });
            }

            let on_event: Box<dyn FnMut(agent::run::TypedAnyStreamEvent) + Send> = {
                let output_tx_ev = output_tx_ev.clone();
                let approval_manager_ev = approval_manager_ev.clone();
                let tracker = tracker.clone();
                let thread_log = thread_log.clone();
                Box::new(move |ev: agent::run::TypedAnyStreamEvent| {
                    let output_tx_ev = output_tx_ev.clone();
                    let approval_manager_ev = approval_manager_ev.clone();
                    let tracker = tracker.clone();
                    let thread_log = thread_log.clone();
                    tokio::spawn(async move {
                        let codex_events = {
                            let mut t = tracker.lock().await;
                            crate::event_bridge::codex_events_from_stream_event(
                                &ev,
                                &mut t,
                                &approval_manager_ev,
                            )
                        };
                        for codex_ev in &codex_events {
                            // Persist to thread log
                            {
                                let mut log_guard = thread_log.lock().await;
                                if let Some(ref mut log) = *log_guard {
                                    let _ = log.append(codex_ev).await;
                                }
                            }
                            // Emit as notification
                            let notif = codex_event_to_notification(codex_ev);
                            let _ = output_tx_ev.send(OutputEvent::Notification(notif)).await;
                        }
                    });
                })
            };

            let (config, _, _) = build_react_config(&run_opts);
            let result: Result<RunCompletion, RunError> = run_agent_from_config(
                &config,
                &RunCmd::React,
                RunParams {
                    message: run_opts.message.clone(),
                    verbose: run_opts.verbose,
                    cancellation: run_opts.cancellation.clone(),
                    any_stream_event_sender: run_opts.any_stream_event_sender.clone(),
                    llm_override: None,
                    thread_id: run_opts.thread_id.clone(),
                },
                Some(on_event),
            )
            .await;

            match result {
                Ok(RunCompletion::Finished(_run_result)) => {
                    let usage = CodexUsage::zero();
                    let notif = codex_event_to_notification(&CodexEvent::TurnCompleted { usage });
                    let _ = output_tx.send(OutputEvent::Notification(notif)).await;
                }
                Ok(RunCompletion::Cancelled) => {
                    let notif = codex_event_to_notification(&CodexEvent::TurnFailed {
                        error: CodexErrorInfo {
                            message: "Turn cancelled".to_string(),
                        },
                    });
                    let _ = output_tx.send(OutputEvent::Notification(notif)).await;
                }
                Err(e) => {
                    let notif = codex_event_to_notification(&CodexEvent::TurnFailed {
                        error: CodexErrorInfo {
                            message: e.to_string(),
                        },
                    });
                    let _ = output_tx.send(OutputEvent::Notification(notif)).await;
                }
            }
        });
    }

    // ── turn/cancel ───────────────────────────────────────────────────────────

    async fn handle_turn_cancel(&self, id: serde_json::Value, params: serde_json::Value) {
        let thread_id = params
            .get("threadId")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        {
            let sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get(thread_id) {
                session.cancel_flag.store(true, Ordering::SeqCst);
            }
        }

        let _ = self
            .output_tx
            .send(OutputEvent::Response {
                id,
                result: json!({ "ok": true }),
            })
            .await;
    }

    // ── approval ──────────────────────────────────────────────────────────────

    async fn handle_command_approval(
        &self,
        id: serde_json::Value,
        params: serde_json::Value,
        approved: bool,
    ) {
        let call_id = params
            .get("commandExecutionId")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        self.approval_manager.resolve(call_id, approved);

        let _ = self
            .output_tx
            .send(OutputEvent::Response {
                id,
                result: json!({ "ok": true }),
            })
            .await;
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn codex_event_to_notification(ev: &CodexEvent) -> CodexNotification {
    let v = serde_json::to_value(ev).unwrap_or_default();
    let event_type = v["type"].as_str().unwrap_or("error");
    let method = match event_type {
        "thread_started" => "thread/started",
        "turn_started" => "turn/started",
        "turn_completed" => "turn/completed",
        "turn_failed" => "turn/failed",
        "item_started" => "item/started",
        "item_updated" => "item/updated",
        "item_completed" => "item/completed",
        _ => "error",
    };
    let mut params = v;
    if let serde_json::Value::Object(ref mut m) = params {
        m.remove("type");
    }
    CodexNotification {
        jsonrpc: "2.0",
        method: method.to_string(),
        params,
    }
}

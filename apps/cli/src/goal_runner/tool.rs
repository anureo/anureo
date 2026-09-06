use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use agent::run::RunCompletion;
use agent::run::{
    build_react_config, run_agent_from_config, RunParams,
    TypedAnyStreamEvent as FullTypedAnyStreamEvent,
};
use anureo_llm::LlmUsage;
use stream_event::StreamEvent;

use agent::goal_runner::state::{ToolCallSummary, ToolError, TurnResult};

const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 300;

#[async_trait]
pub trait CodingTool: Send + Sync {
    async fn execute(&self, prompt: &str, working_dir: &Path) -> Result<TurnResult, ToolError>;
    fn name(&self) -> &str;
}

pub struct ShellTool {
    command: String,
    args: Vec<String>,
    timeout: Duration,
    cancel: Option<CancellationToken>,
}

impl ShellTool {
    pub fn new(command: String, args: Vec<String>) -> Self {
        Self {
            command,
            args,
            timeout: Duration::from_secs(DEFAULT_TOOL_TIMEOUT_SECS),
            cancel: None,
        }
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }
}

#[async_trait]
impl CodingTool for ShellTool {
    async fn execute(&self, prompt: &str, working_dir: &Path) -> Result<TurnResult, ToolError> {
        let mut cmd = tokio::process::Command::new(&self.command);
        cmd.args(&self.args)
            .current_dir(working_dir)
            .env("ANUREO_GOAL_PROMPT", prompt)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd.spawn().map_err(|e| {
            ToolError::ExecutionFailed(format!("failed to spawn {}: {}", self.command, e))
        })?;

        let output_fut = async {
            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let status = child.wait().await?;
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            if let Some(mut out) = stdout {
                use tokio::io::AsyncReadExt;
                let _ = out.read_to_end(&mut stdout_buf).await;
            }
            if let Some(mut err) = stderr {
                use tokio::io::AsyncReadExt;
                let _ = err.read_to_end(&mut stderr_buf).await;
            }
            Ok::<_, std::io::Error>((status, stdout_buf, stderr_buf))
        };

        let result = if let Some(ref cancel) = self.cancel {
            tokio::select! {
                res = output_fut => res,
                _ = cancel.cancelled() => {
                    let _ = child.kill().await;
                    return Err(ToolError::Aborted);
                }
            }
        } else {
            match tokio::time::timeout(self.timeout, output_fut).await {
                Ok(res) => res,
                Err(_) => {
                    let _ = child.kill().await;
                    return Err(ToolError::Timeout);
                }
            }
        }
        .map_err(|e| ToolError::ExecutionFailed(format!("{} failed: {}", self.command, e)))?;

        let (status, stdout_buf, stderr_buf) = result;

        if let Some(ref cancel) = self.cancel {
            if cancel.is_cancelled() {
                return Err(ToolError::Aborted);
            }
        }

        let stdout = String::from_utf8_lossy(&stdout_buf).to_string();
        let stderr = String::from_utf8_lossy(&stderr_buf).to_string();

        if !status.success() {
            return Err(ToolError::ExecutionFailed(format!(
                "{} exited with {}: {}",
                self.command,
                status,
                stderr.trim()
            )));
        }

        Ok(TurnResult {
            reply: stdout,
            reasoning_content: None,
            tool_calls_summary: Vec::new(),
            usage: None,
            work_summary: None,
        })
    }

    fn name(&self) -> &str {
        &self.command
    }
}

pub struct AnureoTool {
    session_id: String,
    _working_dir: PathBuf,
    mcp_config_path: PathBuf,
    cancellation: Option<tool_core::active_operation::RunCancellation>,
    verbose: bool,
    model: Option<String>,
    provider: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    provider_type: Option<String>,
    agent: Option<String>,
    effort: Option<String>,
    tier: Option<String>,
    any_stream_event_sender: Option<Arc<dyn Fn(FullTypedAnyStreamEvent) + Send + Sync>>,
}

impl AnureoTool {
    pub fn new(session_id: String, working_dir: PathBuf, mcp_config_path: PathBuf) -> Self {
        Self {
            session_id,
            _working_dir: working_dir,
            mcp_config_path,
            cancellation: None,
            verbose: false,
            model: None,
            provider: None,
            base_url: None,
            api_key: None,
            provider_type: None,
            agent: None,
            effort: None,
            tier: None,
            any_stream_event_sender: None,
        }
    }

    pub fn with_cancellation(mut self, c: tool_core::active_operation::RunCancellation) -> Self {
        self.cancellation = Some(c);
        self
    }

    pub fn with_model(mut self, m: String) -> Self {
        self.model = Some(m);
        self
    }

    /// Sets the reasoning effort level forwarded to the LLM client per turn.
    /// Accepts the same values as the CLI `--effort` flag
    /// (auto|none|minimal|low|medium|high|xhigh).
    pub fn with_effort(mut self, e: String) -> Self {
        self.effort = Some(e);
        self
    }

    pub fn with_event_sender(
        mut self,
        sender: Arc<dyn Fn(FullTypedAnyStreamEvent) + Send + Sync>,
    ) -> Self {
        self.any_stream_event_sender = Some(sender);
        self
    }
}

#[async_trait]
impl CodingTool for AnureoTool {
    async fn execute(&self, prompt: &str, working_dir: &Path) -> Result<TurnResult, ToolError> {
        use agent::run::RunCmd;
        use agent::run::RunOptions;
        use anureo_llm::message::UserContent;

        let tool_summaries: Arc<Mutex<Vec<ToolCallSummary>>> = Arc::new(Mutex::new(Vec::new()));
        let usage: Arc<Mutex<Option<LlmUsage>>> = Arc::new(Mutex::new(None));

        let on_event: Option<Box<dyn FnMut(FullTypedAnyStreamEvent) + Send>> =
            if let Some(ref sender) = self.any_stream_event_sender {
                let sender = sender.clone();
                let summaries = tool_summaries.clone();
                let usage = usage.clone();
                Some(Box::new(move |ev: FullTypedAnyStreamEvent| {
                    collect_tool_summary(&ev, &summaries);
                    collect_usage(&ev, &usage);
                    sender(ev);
                }))
            } else {
                let mut original = crate::display::create_stdio_event_callback(
                    crate::display::StreamDisplayConfig {
                        verbose: self.verbose,
                        display_max_len: 10000,
                        output_timestamp: false,
                        agent_display: None,
                        use_spinner: true,
                    },
                );
                let summaries = tool_summaries.clone();
                let usage = usage.clone();
                Some(Box::new(move |ev: FullTypedAnyStreamEvent| {
                    collect_tool_summary(&ev, &summaries);
                    collect_usage(&ev, &usage);
                    original(ev);
                }))
            };

        let opts = RunOptions {
            message: UserContent::Text(prompt.to_string()),
            working_folder: Some(working_dir.to_path_buf()),
            session_id: Some(self.session_id.clone()),
            agent: self.agent.clone(),
            verbose: self.verbose,
            verbose_level: 0,
            got_adaptive: false,
            display_max_len: 10000,
            output_json: false,
            model: self.model.clone(),
            provider: self.provider.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            provider_type: self.provider_type.clone(),
            mcp_config_path: Some(self.mcp_config_path.clone()),
            cancellation: self.cancellation.clone(),
            thread_id: Some(self.session_id.clone()),
            output_timestamp: false,
            dry_run: false,
            debug_llm: false,
            any_stream_event_sender: self.any_stream_event_sender.clone(),
            bash_executor: None,
            extra_tools: None,
            default_extra_tools_provider: Some(cli::run::default_workflow_tool_provider()),
            acp_session_id: None,
            force_compact: false,
            chat_id: None,
            worktree: false,
            goal_mode: true,
            acp_mcp_servers: None,

            acp_mcp_sources: None,
            effort: self.effort.clone(),
            tier: self.tier.clone(),
        };

        let (config, _, _) = build_react_config(&opts);
        let params = RunParams {
            message: opts.message.clone(),
            verbose: opts.verbose,
            cancellation: opts.cancellation.clone(),
            any_stream_event_sender: opts.any_stream_event_sender.clone(),
            llm_override: None,
            thread_id: opts.thread_id.clone(),
        };
        let result = run_agent_from_config(&config, &RunCmd::React, params, on_event)
            .await
            .map_err(|e| {
                let msg = format!("anureo agent error: {}", e);
                if ToolError::is_transient_api_error(&msg) {
                    ToolError::RateLimited(msg)
                } else {
                    ToolError::ExecutionFailed(msg)
                }
            })?;

        let tool_calls_summary = tool_summaries.lock().unwrap().drain(..).collect();
        let usage = usage.lock().unwrap().take();

        match result {
            RunCompletion::Finished(agent_result) => Ok(TurnResult {
                reply: agent_result.reply,
                reasoning_content: agent_result.reasoning_content,
                tool_calls_summary,
                usage,
                work_summary: None,
            }),
            RunCompletion::Cancelled => Err(ToolError::Aborted),
        }
    }

    fn name(&self) -> &str {
        "anureo"
    }
}

pub fn generate_mcp_config(_tool_name: &str, db_path: &Path) -> String {
    let db_path_str = db_path.to_string_lossy().replace('\\', "\\\\");
    format!(
        r#"{{"mcpServers":{{"task":{{"command":"task-mcp-server","args":["--db-path","{}"]}}}}}}"#,
        db_path_str
    )
}

/// Returns default CLI arguments for known shell-based coding tools.
pub fn shell_tool_args(tool_name: &str) -> Vec<String> {
    match tool_name {
        "codex" | "claude" | "cursor" => vec!["--goal-prompt".to_string()],
        _ => vec![],
    }
}

/// Extract tool name + result preview from `StreamEvent::ToolEnd` events
/// into a shared summary list for the goal runner to display after each turn.
fn collect_tool_summary(
    ev: &FullTypedAnyStreamEvent,
    summaries: &Arc<Mutex<Vec<ToolCallSummary>>>,
) {
    if let FullTypedAnyStreamEvent::React(StreamEvent::ToolEnd { name, result, .. }) = ev {
        let preview =
            crate::display::tool_summary::truncate(result.lines().next().unwrap_or(result), 80);
        summaries.lock().unwrap().push(ToolCallSummary {
            tool_name: name.clone(),
            result_preview: preview.to_string(),
        });
    }
}

/// Convert the agent's streamed turn usage into the goal runner's accounting
/// type. The callback can observe more than one model call in a turn, so add
/// the headline counts instead of keeping only the last event.
fn collect_usage(ev: &FullTypedAnyStreamEvent, usage: &Arc<Mutex<Option<LlmUsage>>>) {
    let FullTypedAnyStreamEvent::React(StreamEvent::TurnFinish { usage: turn, .. }) = ev else {
        return;
    };

    let mut guard = usage.lock().unwrap();
    let current = guard.get_or_insert_with(LlmUsage::default);
    current.prompt_tokens = current.prompt_tokens.saturating_add(turn.input);
    current.completion_tokens = current.completion_tokens.saturating_add(turn.output);
    current.total_tokens = current
        .total_tokens
        .saturating_add(turn.input.saturating_add(turn.output));
}

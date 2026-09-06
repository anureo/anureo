use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use task_core::{TaskDb, TaskStatus};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;
use tracing;

use super::tool::CodingTool;
use agent::goal_runner::message;
use agent::goal_runner::state::{
    GoalError, GoalLifecycle, GoalMeta, GoalOutcome, HistoryEntry, ToolError,
    DEFAULT_MAX_ITERATIONS,
    MAX_CONSECUTIVE_FAILURES, MAX_HISTORY_ENTRIES,
};
use agent::run::TypedAnyStreamEvent as FullTypedAnyStreamEvent;

/// Fraction of token budget remaining that triggers the budget-limit prompt.
const BUDGET_WARNING_FRACTION: f64 = 0.2;

enum VerifyResult {
    Passed,
    Failed,
    Aborted,
}

pub struct GoalRunner {
    task_id: String,
    objective: String,
    db: Arc<TaskDb>,
    tool: Box<dyn CodingTool>,
    mcp_server: Option<Child>,
    working_dir: PathBuf,
    iteration: u32,
    max_iterations: u32,
    cancel: CancellationToken,
    consecutive_failures: u32,
    last_errors: Vec<String>,
    time_used_seconds: i64,
    /// Optional hard cap on cumulative token usage.
    token_budget: Option<u32>,
    /// Cumulative tokens consumed across all iterations.
    tokens_used: u32,
    lifecycle: GoalLifecycle,
    lifecycle_reason: Option<String>,
    /// Persisted anureo model override, if configured.
    model: Option<String>,
    /// Persisted reasoning effort override, if configured.
    effort: Option<String>,
    /// Optional shell command to verify objective after each iteration.
    verify_command: Option<String>,
    /// Number of consecutive rate-limit retries in the current streak.
    rate_limit_retries: u32,
}

impl GoalRunner {
    /// Construct a runner for an already-created task.
    ///
    /// The CLI needs the task id before constructing `AnureoTool`, because the
    /// id is also used as the agent thread id. Keeping task creation outside
    /// the runner avoids creating one task to obtain an id and a second task
    /// when the runner is initialized.
    pub async fn from_task(
        task_id: String,
        objective: String,
        working_dir: PathBuf,
        db: Arc<TaskDb>,
        tool: Box<dyn CodingTool>,
        cancel: CancellationToken,
    ) -> Result<Self, GoalError> {
        let mcp_server = spawn_mcp_server(&db).ok();

        Ok(Self {
            task_id,
            objective,
            db,
            tool,
            mcp_server,
            working_dir,
            iteration: 0,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            cancel,
            consecutive_failures: 0,
            last_errors: Vec::new(),
            time_used_seconds: 0,
            token_budget: None,
            tokens_used: 0,
            lifecycle: GoalLifecycle::Active,
            lifecycle_reason: None,
            model: None,
            effort: None,
            verify_command: None,
            rate_limit_retries: 0,
        })
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    /// Set an optional hard cap on cumulative token usage.
    pub fn with_token_budget(mut self, budget: u32) -> Self {
        self.token_budget = Some(budget);
        self
    }

    /// Persist the model and reasoning effort selected for future resumes.
    pub fn with_model_config(mut self, model: Option<String>, effort: Option<String>) -> Self {
        self.model = model;
        self.effort = effort;
        self
    }

    /// Set an optional verification command to run after each iteration.
    pub fn with_verify_command(mut self, cmd: String) -> Self {
        self.verify_command = Some(cmd);
        self
    }

    /// Save the goal configuration before the first turn starts.
    pub async fn persist_initial_state(&self) -> Result<(), GoalError> {
        let mut meta = self.load_meta_async().await.unwrap_or_default();
        meta.tool = self.tool.name().to_string();
        meta.token_budget = self.token_budget;
        meta.model = self.model.clone();
        meta.effort = self.effort.clone();
        meta.lifecycle = self.lifecycle;
        meta.lifecycle_reason = self.lifecycle_reason.clone();
        meta.verify_command = self.verify_command.clone();
        self.save_meta_async(&meta).await
    }

    fn set_lifecycle(&mut self, lifecycle: GoalLifecycle, reason: impl Into<String>) {
        self.lifecycle = lifecycle;
        self.lifecycle_reason = Some(reason.into());
    }

    pub async fn run(&mut self) -> GoalOutcome {
        let start = Instant::now();
        loop {
            self.iteration += 1;

            if self.cancel.is_cancelled() {
                tracing::info!(session_id = %self.task_id, iteration = self.iteration, "goal cancelled");
                self.save_paused_state().await;
                self.cleanup().await;
                return GoalOutcome::Error("aborted by user".into());
            }

            if self.iteration > self.max_iterations {
                tracing::warn!(
                    session_id = %self.task_id,
                    "max iterations ({}) reached",
                    self.max_iterations
                );
                self.save_terminal_state(
                    GoalLifecycle::Blocked,
                    "maximum iterations reached",
                )
                .await;
                self.cleanup().await;
                return GoalOutcome::Blocked(format!(
                    "max iterations ({}) reached",
                    self.max_iterations
                ));
            }

            // Build prompt with optional budget warning and history summary.
            let history_summary = self.build_history_summary().await;
            let budget_warning = self.build_budget_warning();
            let prompt = message::build_continuation_prompt(
                &self.task_id,
                &self.objective,
                self.time_used_seconds,
                self.tokens_used,
                self.token_budget,
                &history_summary,
                &budget_warning,
                self.verify_command.as_deref(),
            );

            tracing::info!(
                session_id = %self.task_id,
                iteration = self.iteration,
                tool = self.tool.name(),
                time_used_seconds = self.time_used_seconds,
                "executing goal turn"
            );

            eprintln!(
                "\n{}",
                crate::display::panel_format::format_panel_line(
                    "GOAL",
                    &format!(
                        "iteration {} | tool: {} | time: {}s",
                        self.iteration,
                        self.tool.name(),
                        self.time_used_seconds
                    ),
                )
            );

            let work_summary: Option<String>;

            match self.tool.execute(&prompt, &self.working_dir).await {
                Ok(turn_result) => {
                    self.consecutive_failures = 0;
                    self.last_errors.clear();
                    self.rate_limit_retries = 0;

                    // Accumulate token usage.
                    if let Some(ref usage) = turn_result.usage {
                        self.tokens_used = self.tokens_used.saturating_add(usage.total_tokens);
                    }

                    // Store work summary for history injection.
                    work_summary = turn_result.work_summary.clone();

                    if let Some(ref reasoning) = turn_result.reasoning_content {
                        if !reasoning.trim().is_empty() {
                            eprintln!(
                                "{}",
                                crate::display::panel_format::format_panel_line(
                                    "THINKING",
                                    &crate::display::render_markdown(reasoning).to_string()
                                )
                            );
                        }
                    }
                    if !turn_result.reply.trim().is_empty() {
                        eprintln!("{}", crate::display::render_markdown(&turn_result.reply));
                    }
                    for tc in &turn_result.tool_calls_summary {
                        eprintln!(
                            "{}",
                            crate::display::panel_format::format_panel_line(
                                "TOOL",
                                &format!("{} → {}", tc.tool_name, tc.result_preview)
                            )
                        );
                    }
                }
                Err(ToolError::Aborted) => {
                    tracing::info!(session_id = %self.task_id, iteration = self.iteration, "tool aborted");
                    self.save_paused_state().await;
                    self.cleanup().await;
                    return GoalOutcome::Error("aborted by user".into());
                }
                Err(ToolError::Timeout) => {
                    tracing::warn!(session_id = %self.task_id, iteration = self.iteration, "tool timeout");
                    self.save_iteration_state().await;
                    continue;
                }
                Err(ToolError::RateLimited(msg)) => {
                    self.rate_limit_retries += 1;
                    let backoff_secs = 2u64.pow(self.rate_limit_retries.min(5));
                    let max_rate_limit_retries: u32 = 6;
                    if self.rate_limit_retries > max_rate_limit_retries {
                        tracing::error!(
                            session_id = %self.task_id,
                            retries = self.rate_limit_retries,
                            "rate-limit retries exhausted"
                        );
                        self.save_paused_state().await;
                        self.set_lifecycle(GoalLifecycle::Blocked, "rate-limit retries exhausted");
                        self.save_iteration_state().await;
                        self.cleanup().await;
                        return GoalOutcome::Blocked(format!(
                            "API rate-limited after {} retries: {}",
                            self.rate_limit_retries, msg
                        ));
                    }
                    tracing::warn!(
                        session_id = %self.task_id,
                        iteration = self.iteration,
                        retry = self.rate_limit_retries,
                        backoff_secs = backoff_secs,
                        "API rate-limited, backing off"
                    );
                    eprintln!(
                        "\n  ⚠ API rate-limited, retrying in {}s (attempt {}/{}) …",
                        backoff_secs, self.rate_limit_retries, max_rate_limit_retries
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                        _ = self.cancel.cancelled() => {
                            self.save_paused_state().await;
                            self.cleanup().await;
                            return GoalOutcome::Error("aborted by user".into());
                        }
                    }
                    // Don't count rate-limit as a consecutive failure — retry the same iteration.
                    self.iteration -= 1;
                    self.save_iteration_state().await;
                    continue;
                }
                Err(ToolError::ExecutionFailed(e)) => {
                    self.consecutive_failures += 1;
                    self.last_errors.push(e.clone());
                    if self.consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        tracing::error!(
                            session_id = %self.task_id,
                            consecutive_failures = self.consecutive_failures,
                            iteration = self.iteration,
                            "consecutive failures limit reached"
                        );
                        let details = self.last_errors.join("\n");
                        self.save_paused_state().await;
                        self.set_lifecycle(GoalLifecycle::Blocked, "consecutive tool failures");
                        self.save_iteration_state().await;
                        self.cleanup().await;
                        return GoalOutcome::Blocked(format!(
                            "consecutive tool failures:\n{}",
                            details
                        ));
                    }
                    tracing::error!(
                        session_id = %self.task_id,
                        iteration = self.iteration,
                        error = %e,
                        "tool failed"
                    );
                    self.save_iteration_state().await;
                    continue;
                }
                Err(ToolError::InvalidJsonArguments {
                    tool_name,
                    raw_args,
                    parse_error,
                }) => {
                    // InvalidJsonArguments is returned when the LLM generates malformed
                    // tool_call.arguments (e.g. MiniMax-M3 outputs "pattern" instead of
                    // '{"pattern": "..."}'). We treat this as a non-fatal error so the
                    // LLM receives the error feedback in the next turn and can self-correct.
                    tracing::warn!(
                        session_id = %self.task_id,
                        tool_name = %tool_name,
                        raw_args_preview = %raw_args.chars().take(100).collect::<String>(),
                        parse_error = %parse_error,
                        "tool arguments JSON parse failed; returning error to LLM for self-correction"
                    );
                    // Don't count as consecutive failure — this is a recoverable LLM error.
                    // Let the iteration complete normally; LLM will see the error in tool_result.
                    self.save_iteration_state().await;
                    continue;
                }
            }

            self.time_used_seconds = start.elapsed().as_secs() as i64;
            self.save_iteration_state_with_summary(work_summary.as_deref())
                .await;

            // A successful task update is authoritative for this iteration.
            // Check it before the budget so a completion turn is not reported
            // as usage-limited merely because its final response crossed the
            // configured cap.
            let task = match self.db.show_task(&self.task_id).await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(session_id = %self.task_id, error = %e, "failed to read task");
                    self.set_lifecycle(GoalLifecycle::Failed, format!("db error: {}", e));
                    self.save_iteration_state().await;
                    self.cleanup().await;
                    return GoalOutcome::Error(format!("db error: {}", e));
                }
            };
            if task.status == TaskStatus::Completed {
                tracing::info!(
                    session_id = %self.task_id,
                    iterations = self.iteration,
                    time_used_seconds = self.time_used_seconds,
                    "goal achieved"
                );
                self.set_lifecycle(GoalLifecycle::Completed, "task marked completed");
                self.save_iteration_state().await;
                self.cleanup().await;
                return GoalOutcome::Achieved;
            }

            // Check token budget exhaustion.
            if let Some(budget) = self.token_budget {
                if self.tokens_used >= budget {
                    tracing::warn!(
                        session_id = %self.task_id,
                        tokens_used = self.tokens_used,
                        budget = budget,
                        "token budget exhausted"
                    );
                    self.save_terminal_state(
                        GoalLifecycle::UsageLimited,
                        "token budget exhausted",
                    )
                    .await;
                    self.cleanup().await;
                    return GoalOutcome::UsageLimited {
                        tokens_used: self.tokens_used,
                        token_budget: budget,
                    };
                }
            }

            // Run verify command if configured.
            if let Some(ref verify_cmd) = self.verify_command {
                match self.run_verify_command(verify_cmd).await {
                    VerifyResult::Passed => {
                        tracing::info!(session_id = %self.task_id, "verify command passed");
                        // Auto-mark complete.
                        if let Err(e) = self
                            .db
                            .atomic_update_status(
                                &self.task_id,
                                TaskStatus::InProgress,
                                TaskStatus::Completed,
                            )
                            .await
                        {
                            tracing::error!(session_id = %self.task_id, error = %e, "failed to mark complete after verify");
                        }
                        self.set_lifecycle(GoalLifecycle::Completed, "verification passed");
                        self.save_iteration_state().await;
                        self.cleanup().await;
                        return GoalOutcome::Achieved;
                    }
                    VerifyResult::Failed => {}
                    VerifyResult::Aborted => {
                        self.save_paused_state().await;
                        self.cleanup().await;
                        return GoalOutcome::Error("aborted by user".into());
                    }
                }
            }
        }
    }

    async fn save_iteration_state(&self) {
        self.save_iteration_state_with_summary(None).await;
    }

    async fn save_iteration_state_with_summary(&self, summary: Option<&str>) {
        let mut meta = self.load_meta_async().await.unwrap_or_default();
        meta.iteration = self.iteration;
        meta.tool = self.tool.name().to_string();
        meta.time_used_seconds = self.time_used_seconds;
        meta.token_budget = self.token_budget;
        meta.tokens_used = self.tokens_used;
        meta.lifecycle = self.lifecycle;
        meta.lifecycle_reason = self.lifecycle_reason.clone();
        meta.model = self.model.clone();
        meta.effort = self.effort.clone();

        meta.history.push(HistoryEntry {
            iteration: self.iteration,
            timestamp: Utc::now().to_rfc3339(),
            summary: summary.map(|s| s.to_string()),
        });
        if meta.history.len() > MAX_HISTORY_ENTRIES {
            let start = meta.history.len() - MAX_HISTORY_ENTRIES;
            meta.history = meta.history.split_off(start);
        }

        if let Err(e) = self.save_meta_async(&meta).await {
            tracing::error!(session_id = %self.task_id, error = %e, "failed to save iteration state");
        }
    }

    async fn save_paused_state(&mut self) {
        self.set_lifecycle(GoalLifecycle::Paused, "aborted by user");
        if let Err(e) = self
            .db
            .atomic_update_status(&self.task_id, TaskStatus::InProgress, TaskStatus::Pending)
            .await
        {
            tracing::error!(session_id = %self.task_id, error = %e, "failed to set task to paused");
        }
        self.save_iteration_state().await;
    }

    /// Persist a non-resumable stop in the task store.
    ///
    /// The task database has no dedicated `budget_limited` or `blocked`
    /// status. `cancelled` is therefore used for hard runner limits, while
    /// retryable failures continue to use `pending` via `save_paused_state`.
    async fn save_terminal_state(&mut self, lifecycle: GoalLifecycle, reason: &str) {
        self.set_lifecycle(lifecycle, reason);
        self.save_iteration_state().await;
        if let Err(e) = self
            .db
            .atomic_update_status(&self.task_id, TaskStatus::InProgress, TaskStatus::Cancelled)
            .await
        {
            tracing::error!(
                session_id = %self.task_id,
                reason,
                error = %e,
                "failed to persist terminal goal state"
            );
        }
    }

    /// Build a short summary of recent iteration history for prompt injection.
    async fn build_history_summary(&self) -> Option<String> {
        let meta = self.load_meta_async().await.ok()?;
        if meta.history.is_empty() {
            return None;
        }
        let lines: Vec<String> = meta
            .history
            .iter()
            .map(|h| match &h.summary {
                Some(s) => format!("  iter {}: {}", h.iteration, s),
                None => format!("  iter {}: completed", h.iteration),
            })
            .collect();
        Some(format!("Previous iterations:\n{}", lines.join("\n")))
    }

    /// Build budget warning text if close to limit.
    fn build_budget_warning(&self) -> Option<String> {
        let budget = self.token_budget?;
        if budget == 0 {
            return None;
        }
        let remaining = budget.saturating_sub(self.tokens_used);
        let fraction = remaining as f64 / budget as f64;
        if fraction <= BUDGET_WARNING_FRACTION {
            Some(format!(
                "WARNING: Token budget almost exhausted. {}/{} tokens used ({}% remaining). \
                 Prioritize wrapping up or calling task_update(status='completed').",
                self.tokens_used,
                budget,
                (fraction * 100.0) as u32
            ))
        } else {
            None
        }
    }

    /// Run the verify command while honoring the same cancellation token as
    /// the coding tool. A cancelled verification is resumable, not a failed
    /// goal and not a successful verification.
    async fn run_verify_command(&self, cmd: &str) -> VerifyResult {
        tracing::info!(session_id = %self.task_id, cmd = cmd, "running verify command");
        eprintln!(
            "{}",
            crate::display::panel_format::format_panel_line("VERIFY", cmd,)
        );
        // Use cmd.exe on Windows, sh elsewhere.
        #[cfg(windows)]
        let mut child = {
            let mut c = tokio::process::Command::new("cmd");
            c.args(["/C", cmd])
                .current_dir(&self.working_dir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            c.creation_flags(CREATE_NO_WINDOW);
            match c.spawn() {
                Ok(child) => child,
                Err(e) => {
                    return self.report_verify_error(e);
                }
            }
        };
        #[cfg(not(windows))]
        let mut child = match tokio::process::Command::new("sh")
                .args(["-c", cmd])
                .current_dir(&self.working_dir)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => return self.report_verify_error(e),
            };

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

        let result = tokio::select! {
            result = output_fut => result,
            _ = self.cancel.cancelled() => {
                let _ = child.kill().await;
                return VerifyResult::Aborted;
            }
        };

        match result {
            Ok((status, _stdout, stderr)) => {
                if status.success() {
                    eprintln!(
                        "{}",
                        crate::display::panel_format::format_panel_line("VERIFY", "passed",)
                    );
                    VerifyResult::Passed
                } else {
                    let stderr = String::from_utf8_lossy(&stderr);
                    eprintln!(
                        "{}",
                        crate::display::panel_format::format_panel_line(
                            "VERIFY",
                            &format!("failed: {}", stderr.trim()),
                        )
                    );
                    VerifyResult::Failed
                }
            }
            Err(e) => {
                self.report_verify_error(e)
            }
        }
    }

    fn report_verify_error(&self, error: std::io::Error) -> VerifyResult {
        tracing::error!(session_id = %self.task_id, error = %error, "verify command failed to execute");
        eprintln!(
            "{}",
            crate::display::panel_format::format_panel_line(
                "VERIFY",
                &format!("error: {}", error),
            )
        );
        VerifyResult::Failed
    }

    async fn load_meta_async(&self) -> Result<GoalMeta, GoalError> {
        let val = self
            .db
            .get_meta(&self.task_id, "goal")
            .await
            .map_err(|e| GoalError::Db(e))?;
        match val {
            Some(v) => serde_json::from_value(v).map_err(|e| GoalError::Db(Box::new(e))),
            None => Ok(GoalMeta::default()),
        }
    }

    async fn save_meta_async(&self, meta: &GoalMeta) -> Result<(), GoalError> {
        let val = serde_json::to_value(meta).map_err(|e| GoalError::Db(Box::new(e)))?;
        self.db
            .set_meta(&self.task_id, "goal", &val)
            .await
            .map_err(|e| GoalError::Db(e))
    }

    async fn cleanup(&mut self) {
        self.cancel.cancel();
        if let Some(ref mut child) = self.mcp_server {
            let _ = child.kill().await;
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await;
        }
    }
}

fn spawn_mcp_server(db: &TaskDb) -> Result<Child, GoalError> {
    let db_path = db.path();
    let mut cmd = tokio::process::Command::new("task-mcp-server");
    cmd.arg("--db-path")
        .arg(db_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped());
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let child = cmd.spawn().map_err(GoalError::Io)?;
    Ok(child)
}

pub async fn resume(
    id: &str,
    working_dir: PathBuf,
    db: Arc<TaskDb>,
    cancel: CancellationToken,
    run_cancellation: Option<tool_core::active_operation::RunCancellation>,
) -> Result<GoalRunner, GoalError> {
    resume_with_event_sender(id, working_dir, db, cancel, run_cancellation, None).await
}

pub async fn resume_with_event_sender(
    id: &str,
    working_dir: PathBuf,
    db: Arc<TaskDb>,
    cancel: CancellationToken,
    run_cancellation: Option<tool_core::active_operation::RunCancellation>,
    event_sender: Option<Arc<dyn Fn(FullTypedAnyStreamEvent) + Send + Sync>>,
) -> Result<GoalRunner, GoalError> {
    let updated = db
        .atomic_update_status(id, TaskStatus::Pending, TaskStatus::InProgress)
        .await
        .map_err(|e| GoalError::Db(Box::new(e)))?;
    if !updated {
        let task = db
            .show_task(id)
            .await
            .map_err(|e| GoalError::Db(Box::new(e)))?;
        return Err(GoalError::Resume(format!(
            "task {} is not paused (status: {})",
            id, task.status
        )));
    }

    let task = db
        .show_task(id)
        .await
        .map_err(|e| GoalError::Db(Box::new(e)))?;
    let meta_val = db
        .get_meta(id, "goal")
        .await
        .map_err(|e| GoalError::Db(e))?;
    let meta: GoalMeta = match meta_val {
        Some(v) => serde_json::from_value(v).map_err(|e| GoalError::Db(Box::new(e)))?,
        None => GoalMeta::default(),
    };

    let tool: Box<dyn CodingTool> = resolve_tool(
        &meta.tool,
        id,
        db.path(),
        &working_dir,
        &run_cancellation,
        &event_sender,
        &cancel,
        meta.model.as_deref(),
        meta.effort.as_deref(),
    )?;
    let mcp_server = spawn_mcp_server(&db).ok();

    Ok(GoalRunner {
        task_id: task.id,
        objective: task.description,
        db,
        tool,
        mcp_server,
        working_dir,
        iteration: meta.iteration,
        max_iterations: DEFAULT_MAX_ITERATIONS,
        cancel,
        consecutive_failures: 0,
        last_errors: Vec::new(),
        time_used_seconds: meta.time_used_seconds,
        token_budget: meta.token_budget,
        tokens_used: meta.tokens_used,
        lifecycle: GoalLifecycle::Active,
        lifecycle_reason: None,
        model: meta.model,
        effort: meta.effort,
        verify_command: meta.verify_command,
        rate_limit_retries: 0,
    })
}

#[allow(clippy::too_many_arguments)]
fn resolve_tool(
    tool_name: &str,
    id: &str,
    db_path: &std::path::Path,
    working_dir: &std::path::Path,
    run_cancellation: &Option<tool_core::active_operation::RunCancellation>,
    event_sender: &Option<Arc<dyn Fn(FullTypedAnyStreamEvent) + Send + Sync>>,
    cancel: &CancellationToken,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Box<dyn CodingTool>, GoalError> {
    match tool_name {
        "anureo" => {
            let mcp_config_path = write_mcp_config(db_path, working_dir)?;
            let session_id = format!("goal-{}", &id[..id.floor_char_boundary(8)]);
            let mut tool = super::tool::AnureoTool::new(
                session_id,
                working_dir.to_path_buf(),
                mcp_config_path,
            );
            if let Some(ref rc) = run_cancellation {
                tool = tool.with_cancellation(rc.clone());
            }
            if let Some(ref sender) = event_sender {
                let sender = sender.clone();
                let adapted: Arc<dyn Fn(FullTypedAnyStreamEvent) + Send + Sync> =
                    Arc::new(move |ev: FullTypedAnyStreamEvent| {
                        sender(ev);
                    });
                tool = tool.with_event_sender(adapted);
            }
            if let Some(model) = model {
                tool = tool.with_model(model.to_string());
            }
            if let Some(effort) = effort {
                tool = tool.with_effort(effort.to_string());
            }
            Ok(Box::new(tool))
        }
        name => {
            let args = super::tool::shell_tool_args(name);
            Ok(Box::new(
                super::tool::ShellTool::new(name.to_string(), args).with_cancel(cancel.clone()),
            ))
        }
    }
}

pub fn write_mcp_config(
    db_path: &std::path::Path,
    working_dir: &std::path::Path,
) -> Result<std::path::PathBuf, GoalError> {
    let config_content = super::tool::generate_mcp_config("task", db_path);
    let config_path = working_dir.join(".anureo").join("goal-mcp.json");
    std::fs::create_dir_all(config_path.parent().unwrap())?;
    std::fs::write(&config_path, config_content)?;
    Ok(config_path)
}

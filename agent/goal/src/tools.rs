//! Goal 三模型工具（alignment §6.8/§6.9）：`get_goal` / `create_goal` / `update_goal`。
//!
//! 不对称控制 + **规则全文内嵌 schema description**（Codex `spec.rs` 做法）：
//! - `get_goal`：无参只读；
//! - `create_goal`：仅用户/system 明确要求时；不能覆盖 unfinished goal；
//!   budget 为正且 ≤ `max_goal_token_budget`；
//! - `update_goal`：只接受 `complete` / `blocked`；blocked 三轮规则走 prompt
//!   自律（**不做服务端计数**）；`complete` 附 `completion_budget_report`。
//!
//! `verify_command` 完成门（§6.9）：complete 宣告 → verify 通过才落账；
//! 失败拒绝并注入继续 steering；未配置直接接受。
//!
//! 门控（§6.2）：宿主仅在**主 session** 的工具构建中注册（`goal_tools`）；
//! sub-agent / workflow 不注册 → 天然不可见、不计账。

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tool_core::{Tool, ToolCallContent, ToolSourceError, ToolSpec};

use crate::accounting::TokenTotals;
use crate::runtime::GoalRuntimeHandle;
use crate::store::GoalStoreError;
use crate::types::{CreateGoalRequest, Goal};

/// 宿主提供的当前 turn usage 快照。模型工具在 turn 中途改变 goal 状态时，
/// 需要先按该快照补账；goal crate 不依赖具体 LLM/ACP usage 类型。
pub trait GoalUsageSnapshot: Send + Sync {
    fn totals(&self) -> TokenTotals;
}

// ============================================================================
// verify 完成门（§6.9）
// ============================================================================

#[derive(Debug, Clone)]
pub struct VerifyOutcome {
    pub ok: bool,
    pub output: String,
}

/// verify 命令执行器（注入以便测试；默认 [`ShellVerifyRunner`]）。
#[async_trait]
pub trait VerifyRunner: Send + Sync {
    async fn run(&self, command: &str) -> VerifyOutcome;
}

/// 默认实现：shell 执行（Windows `cmd /S /C`，unix `sh -c`），超时默认 10 分钟。
pub struct ShellVerifyRunner {
    pub timeout: Duration,
}

impl Default for ShellVerifyRunner {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(600),
        }
    }
}

#[async_trait]
impl VerifyRunner for ShellVerifyRunner {
    async fn run(&self, command: &str) -> VerifyOutcome {
        let run = async {
            let output = if cfg!(windows) {
                tokio::process::Command::new("cmd")
                    .args(["/S", "/C", command])
                    .output()
                    .await
            } else {
                tokio::process::Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .output()
                    .await
            };
            match output {
                Ok(out) => {
                    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
                    text.push_str(&String::from_utf8_lossy(&out.stderr));
                    let mut text = text.chars().take(4000).collect::<String>();
                    if out.status.success() {
                        VerifyOutcome {
                            ok: true,
                            output: text,
                        }
                    } else {
                        if text.trim().is_empty() {
                            text = format!("exit code: {}", out.status.code().unwrap_or(-1));
                        }
                        VerifyOutcome {
                            ok: false,
                            output: text,
                        }
                    }
                }
                Err(e) => VerifyOutcome {
                    ok: false,
                    output: format!("failed to spawn: {e}"),
                },
            }
        };
        match tokio::time::timeout(self.timeout, run).await {
            Ok(outcome) => outcome,
            Err(_) => VerifyOutcome {
                ok: false,
                output: format!("verify command timed out after {:?}", self.timeout),
            },
        }
    }
}

// ============================================================================
// 共用：快照渲染
// ============================================================================

fn render_snapshot(goal: &Goal) -> String {
    let mut s = format!(
        "Goal {} status: {}\nObjective: {}\nBudget: {}/{} tokens used ({} remaining)\nTime spent: {}s",
        goal.goal_id,
        goal.status.as_str(),
        goal.objective,
        goal.tokens_used,
        goal.token_budget.map(|b| b.to_string()).unwrap_or_else(|| "∞".into()),
        goal.remaining_tokens().map(|r| r.to_string()).unwrap_or_else(|| "∞".into()),
        goal.time_used_seconds,
    );
    if let Some(cmd) = &goal.verify_command {
        s.push_str(&format!("\nVerify command: `{cmd}`"));
    }
    if let Some(reason) = &goal.status_reason {
        s.push_str(&format!("\nReason: {reason}"));
    }
    s
}

/// Host-facing snapshot renderer (`/goal show`, REPL, ACP receipts): same
/// text the `get_goal` model tool returns, so every surface shows the goal
/// identically.
pub fn render_goal_snapshot(goal: &Goal) -> String {
    render_snapshot(goal)
}

fn no_goal_text(thread_id: &str) -> String {
    format!("No goal is set on thread {thread_id}. Use create_goal to arm one (only when the user explicitly asks).")
}

// ============================================================================
// get_goal
// ============================================================================

pub struct GetGoalTool {
    handle: Arc<GoalRuntimeHandle>,
}

#[async_trait]
impl Tool for GetGoalTool {
    fn name(&self) -> &str {
        "get_goal"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "get_goal",
            Some(
                "Get the current goal state for this session: status, objective, \
                 token budget usage (tokens_used / token_budget / remaining) and \
                 time spent. Check this before planning long work or when unsure \
                 whether a goal is active."
                    .into(),
            ),
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        )
    }

    async fn call(
        &self,
        _args: Value,
        _ctx: Option<&tool_core::ToolCallContext>,
    ) -> Result<ToolCallContent, ToolSourceError> {
        let goal = self
            .handle
            .service()
            .store()
            .read(self.handle.thread_id())
            .await
            .map_err(|e| ToolSourceError::ToolError(e.to_string()))?;
        let text = match goal {
            None => no_goal_text(self.handle.thread_id()),
            Some(g) => {
                let g = self.handle.service().resolve_objective(g).await;
                render_snapshot(&g)
            }
        };
        Ok(ToolCallContent::text(text))
    }
}

// ============================================================================
// create_goal
// ============================================================================

pub struct CreateGoalTool {
    handle: Arc<GoalRuntimeHandle>,
    usage: Option<Arc<dyn GoalUsageSnapshot>>,
}

const CREATE_DESCRIPTION: &str =
    "Create the goal for this session. ONLY create a goal when the user \
or the system explicitly asks to set/track a goal — never on your own initiative. Rules:\n\
- You cannot create a new goal while an unfinished one exists (complete or get it blocked first).\n\
- `objective` must be a concrete, verifiable statement of done.\n\
- `token_budget` is optional; when set it must be positive and within the allowed maximum.\n\
- The new goal starts in `active` status immediately.";

#[async_trait]
impl Tool for CreateGoalTool {
    fn name(&self) -> &str {
        "create_goal"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "create_goal",
            Some(CREATE_DESCRIPTION.into()),
            json!({
                "type": "object",
                "properties": {
                    "objective": { "type": "string", "minLength": 1,
                        "description": "Concrete, verifiable statement of done." },
                    "token_budget": { "type": "integer", "exclusiveMinimum": 0,
                        "description": "Optional token budget for the goal." }
                },
                "required": ["objective"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(
        &self,
        args: Value,
        _ctx: Option<&tool_core::ToolCallContext>,
    ) -> Result<ToolCallContent, ToolSourceError> {
        let objective = args
            .get("objective")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                ToolSourceError::InvalidInput("objective (string) is required".into())
            })?;
        let token_budget = match args.get("token_budget") {
            None | Some(Value::Null) => None,
            Some(value) => Some(value.as_i64().ok_or_else(|| {
                ToolSourceError::InvalidInput("token_budget must be an integer".into())
            })?),
        };

        let req = CreateGoalRequest {
            thread_id: self.handle.thread_id().to_string(),
            objective: objective.to_string(),
            token_budget,
            verify_command: None,
        };
        match self
            .handle
            .service()
            .store()
            .create(&req, &crate::service::new_goal_id())
            .await
        {
            Err(GoalStoreError::ExistingUnfinishedGoal(_)) => {
                Ok(ToolCallContent::text(String::from(
                    "Refused: this session already has an unfinished goal. Check it with \
                     get_goal, drive it to `complete` via update_goal, or mark it `blocked` \
                     with a reason; then create the new goal. You may not overwrite an \
                     unfinished goal.",
                )))
            }
            Err(GoalStoreError::Validation(e)) => {
                Ok(ToolCallContent::text(format!("Invalid goal: {e}")))
            }
            Err(e) => Err(ToolSourceError::ToolError(e.to_string())),
            Ok(goal) => {
                // 基线重置到新 goal 武装点（沿用最近已知累计用量）
                if let Some(usage) = &self.usage {
                    self.handle
                        .note_goal_armed_at(&goal.goal_id, usage.totals())
                        .await;
                } else {
                    self.handle.note_goal_armed(&goal.goal_id).await;
                }
                Ok(ToolCallContent::text(format!(
                    "Goal created and armed.\n\n{}",
                    render_snapshot(&goal)
                )))
            }
        }
    }
}

// ============================================================================
// update_goal
// ============================================================================

pub struct UpdateGoalTool {
    handle: Arc<GoalRuntimeHandle>,
    verify: Arc<dyn VerifyRunner>,
    usage: Option<Arc<dyn GoalUsageSnapshot>>,
}

const UPDATE_DESCRIPTION: &str = "Update the active goal's lifecycle status. Rules:\n\
- `status` must be `complete` or `blocked` — nothing else is accepted.\n\
- Use `complete` ONLY after performing a completion audit: restate the objective \
as concrete deliverables, verify every item against the actual current state, and \
include a `completion_budget_report` (what was delivered, what was intentionally \
left out, remaining risks).\n\
- If a verify command is configured, it will run before completion is accepted; \
on failure you must continue working.\n\
- Use `blocked` when progress is impossible and needs user intervention. If you \
have found yourself blocked on this goal for roughly three consecutive attempts \
(each time reporting blocked and re-attempting without new information), stop \
attempting and clearly surface the blocker to the user instead of repeating.";

#[async_trait]
impl Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        "update_goal"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "update_goal",
            Some(UPDATE_DESCRIPTION.into()),
            json!({
                "type": "object",
                "properties": {
                    "status": { "type": "string", "enum": ["complete", "blocked"] },
                    "reason": { "type": "string",
                        "description": "For blocked: what is blocking progress." },
                    "completion_budget_report": { "type": "string",
                        "description": "For complete: what was delivered / left out / remaining risks." }
                },
                "required": ["status"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(
        &self,
        args: Value,
        _ctx: Option<&tool_core::ToolCallContext>,
    ) -> Result<ToolCallContent, ToolSourceError> {
        let status = args
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolSourceError::InvalidInput("status (string) is required".into()))?;
        let reason = args.get("reason").and_then(Value::as_str).unwrap_or("");
        let report = args
            .get("completion_budget_report")
            .and_then(Value::as_str)
            .unwrap_or("");
        let thread = self.handle.thread_id().to_string();
        let store = self.handle.service().store();

        match status {
            "blocked" => {
                let goal = store
                    .read(&thread)
                    .await
                    .map_err(|e| ToolSourceError::ToolError(e.to_string()))?;
                let Some(goal) = goal else {
                    return Ok(ToolCallContent::text(no_goal_text(&thread)));
                };
                self.handle
                    .before_model_status_update(self.usage.as_ref().map(|u| u.totals()))
                    .await
                    .map_err(|e| ToolSourceError::ToolError(e.to_string()))?;
                let reason = if reason.is_empty() {
                    "blocked by model"
                } else {
                    reason
                };
                match store.mark_blocked(&thread, &goal.goal_id, reason).await {
                    Err(GoalStoreError::NotFoundOrDisallowed(_)) => {
                        Ok(ToolCallContent::text(format!(
                            "Goal {} is not in active state; cannot mark blocked.",
                            goal.goal_id
                        )))
                    }
                    Err(e) => Err(ToolSourceError::ToolError(e.to_string())),
                    Ok(g) => {
                        crate::metrics::global().record_blocked(&thread);
                        Ok(ToolCallContent::text(format!(
                            "Goal {} marked blocked. Surface the blocker to the user clearly; \
                             do not keep retrying the same failing approach.",
                            g.goal_id
                        )))
                    }
                }
            }
            "complete" => {
                let goal = store
                    .read(&thread)
                    .await
                    .map_err(|e| ToolSourceError::ToolError(e.to_string()))?;
                let Some(goal) = goal else {
                    return Ok(ToolCallContent::text(no_goal_text(&thread)));
                };
                // §6.9 verify 完成门：配置了 verify_command → 通过才落账
                if let Some(cmd) = &goal.verify_command {
                    let outcome = self.verify.run(cmd).await;
                    if !outcome.ok {
                        return Ok(ToolCallContent::text(format!(
                            "Completion REFUSED: verify command `{cmd}` failed.\n\
                             --- verify output ---\n{}\n---------------------\n\
                             Fix the issue and continue working toward the goal.",
                            outcome.output
                        )));
                    }
                }
                self.handle
                    .before_model_status_update(self.usage.as_ref().map(|u| u.totals()))
                    .await
                    .map_err(|e| ToolSourceError::ToolError(e.to_string()))?;
                match store.mark_complete(&thread, &goal.goal_id).await {
                    Err(GoalStoreError::NotFoundOrDisallowed(_)) => {
                        Ok(ToolCallContent::text(format!(
                            "Goal {} is not in active state; cannot complete.",
                            goal.goal_id
                        )))
                    }
                    Err(e) => Err(ToolSourceError::ToolError(e.to_string())),
                    Ok(g) => {
                        crate::metrics::global().record_complete(&thread);
                        let mut text = format!("Goal {} marked complete.", g.goal_id);
                        if !report.is_empty() {
                            text.push_str(&format!("\nCompletion report:\n{report}"));
                        }
                        text.push_str(&format!(
                            "\nFinal accounting: {}/{} tokens, {}s.",
                            g.tokens_used,
                            g.token_budget
                                .map(|b| b.to_string())
                                .unwrap_or_else(|| "∞".into()),
                            g.time_used_seconds,
                        ));
                        Ok(ToolCallContent::text(text))
                    }
                }
            }
            other => Err(ToolSourceError::InvalidInput(format!(
                "status must be `complete` or `blocked`, got `{other}`"
            ))),
        }
    }
}

// ============================================================================
// 注册入口（仅主 session）
// ============================================================================

/// 构建主 session 的 goal 工具集；sub-agent / workflow 的构建路径**不得**调用。
pub fn goal_tools(
    handle: Arc<GoalRuntimeHandle>,
    verify: Arc<dyn VerifyRunner>,
) -> Vec<Arc<dyn Tool>> {
    goal_tools_with_usage(handle, verify, None)
}

/// 构建带实时 turn usage 的主 session goal 工具集。
pub fn goal_tools_with_usage(
    handle: Arc<GoalRuntimeHandle>,
    verify: Arc<dyn VerifyRunner>,
    usage: Option<Arc<dyn GoalUsageSnapshot>>,
) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(GetGoalTool {
            handle: handle.clone(),
        }),
        Arc::new(CreateGoalTool {
            handle: handle.clone(),
            usage: usage.clone(),
        }),
        Arc::new(UpdateGoalTool {
            handle,
            verify,
            usage,
        }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::TurnDriver;
    use crate::store::GoalStore;
    use tokio::sync::Mutex;

    struct MockDriver;
    #[async_trait]
    impl TurnDriver for MockDriver {
        async fn start_turn_if_idle(&self, _t: &str, _m: &str) -> Result<bool, String> {
            Ok(false)
        }
    }

    struct MockVerify {
        ok: Mutex<bool>,
        calls: Mutex<Vec<String>>,
    }

    struct StaticUsage(TokenTotals);

    impl GoalUsageSnapshot for StaticUsage {
        fn totals(&self) -> TokenTotals {
            self.0
        }
    }
    impl MockVerify {
        fn passing() -> Arc<Self> {
            Arc::new(Self {
                ok: Mutex::new(true),
                calls: Mutex::new(Vec::new()),
            })
        }
    }
    #[async_trait]
    impl VerifyRunner for MockVerify {
        async fn run(&self, command: &str) -> VerifyOutcome {
            self.calls.lock().await.push(command.to_string());
            let ok = *self.ok.lock().await;
            VerifyOutcome {
                ok,
                output: if ok {
                    "all tests passed".into()
                } else {
                    "1 test FAILED".into()
                },
            }
        }
    }

    async fn setup(budget: Option<i64>, verify: Option<String>) -> Arc<GoalRuntimeHandle> {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .expect("open TaskDb");
        let store = GoalStore::from_task_db(&db);
        let handle = GoalRuntimeHandle::new(store, "t1", Arc::new(MockDriver));
        let g = handle
            .service()
            .set_with_verify("t1", "objective", budget, verify)
            .await
            .expect("set");
        handle.note_goal_armed(&g.goal_id).await;
        std::mem::forget(dir); // 测试进程生命周期内保持
        handle
    }

    #[tokio::test]
    async fn get_goal_returns_snapshot() {
        let handle = setup(Some(5000), None).await;
        let tool = &goal_tools(handle.clone(), MockVerify::passing())[0];
        let out = tool.call(json!({}), None).await.expect("call");
        assert!(matches!(&out, ToolCallContent::Text(t)
            if t.contains("status: active") && t.contains("0/5000 tokens used") && t.contains("5000 remaining")));
    }

    #[tokio::test]
    async fn create_rejects_over_unfinished_with_rule_text() {
        let handle = setup(None, None).await;
        let tools = goal_tools(handle.clone(), MockVerify::passing());
        let create = &tools[1];
        let out = create
            .call(json!({ "objective": "second" }), None)
            .await
            .expect("call");
        assert!(
            matches!(&out, ToolCallContent::Text(t) if t.contains("unfinished goal")),
            "拒绝文本必须说明规则：{out:?}"
        );
    }

    #[tokio::test]
    async fn update_complete_verify_gate() {
        let handle = setup(Some(10_000), Some("cargo test".into())).await;
        let verify = MockVerify::passing();
        let tools = goal_tools(handle.clone(), verify.clone());
        let update = &tools[2];

        // verify 失败 → 拒绝，状态保持 active
        *verify.ok.lock().await = false;
        let out = update
            .call(
                json!({ "status": "complete", "completion_budget_report": "done things" }),
                None,
            )
            .await
            .expect("call");
        assert!(matches!(&out, ToolCallContent::Text(t) if t.contains("REFUSED")));
        let g = handle
            .service()
            .show("t1")
            .await
            .expect("show")
            .expect("exists");
        assert_eq!(
            g.status,
            crate::types::GoalStatus::Active,
            "verify 失败不得落账"
        );

        // verify 通过 → 落账 complete + 附 report
        *verify.ok.lock().await = true;
        let out = update
            .call(
                json!({ "status": "complete", "completion_budget_report": "done things" }),
                None,
            )
            .await
            .expect("call");
        assert!(
            matches!(&out, ToolCallContent::Text(t) if t.contains("marked complete") && t.contains("done things"))
        );
        let g = handle
            .service()
            .show("t1")
            .await
            .expect("show")
            .expect("exists");
        assert_eq!(g.status, crate::types::GoalStatus::Complete);
        assert_eq!(verify.calls.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn update_complete_accounts_usage_before_terminal_transition() {
        let handle = setup(Some(10_000), None).await;
        handle.on_turn_start().await.expect("turn start");
        let usage = Arc::new(StaticUsage(TokenTotals {
            input_tokens: 120,
            output_tokens: 30,
            cached_tokens: 20,
        }));
        let tools = goal_tools_with_usage(handle.clone(), MockVerify::passing(), Some(usage));

        tools[2]
            .call(
                json!({"status": "complete", "completion_budget_report": "done"}),
                None,
            )
            .await
            .expect("complete");

        let goal = handle
            .service()
            .show("t1")
            .await
            .expect("show")
            .expect("goal");
        assert_eq!(goal.status, crate::types::GoalStatus::Complete);
        assert_eq!(goal.tokens_used, 130, "(120 - 20 cached) + 30 output");
    }

    #[tokio::test]
    async fn get_goal_resolves_file_backed_objective_for_model() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = task_core::TaskDb::open(&dir.path().join("tasks.db"))
            .await
            .expect("open TaskDb");
        let store = GoalStore::from_task_db(&db).with_goals_dir(dir.path().join("goals"));
        let handle = GoalRuntimeHandle::new(store, "long", Arc::new(MockDriver));
        let objective = "目标".repeat(2500);
        handle
            .service()
            .set("long", &objective, None)
            .await
            .expect("set long objective");

        let out = goal_tools(handle, MockVerify::passing())[0]
            .call(json!({}), None)
            .await
            .expect("get_goal");
        assert!(
            matches!(out, ToolCallContent::Text(text) if text.contains(&objective) && !text.contains("@file:"))
        );
    }

    #[tokio::test]
    async fn update_blocked_and_invalid_status() {
        let handle = setup(None, None).await;
        let tools = goal_tools(handle.clone(), MockVerify::passing());
        let update = &tools[2];

        let out = update
            .call(
                json!({ "status": "blocked", "reason": "missing credentials" }),
                None,
            )
            .await
            .expect("call");
        assert!(matches!(&out, ToolCallContent::Text(t) if t.contains("blocked")));
        let g = handle
            .service()
            .show("t1")
            .await
            .expect("show")
            .expect("exists");
        assert_eq!(g.status, crate::types::GoalStatus::Blocked);
        assert_eq!(g.status_reason.as_deref(), Some("missing credentials"));

        let err = update
            .call(json!({ "status": "paused" }), None)
            .await
            .expect_err("paused 必须被拒");
        assert!(matches!(err, ToolSourceError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn missing_objective_is_invalid_input() {
        let handle = setup(None, None).await;
        let tools = goal_tools(handle.clone(), MockVerify::passing());
        let err = tools[1]
            .call(json!({}), None)
            .await
            .expect_err("must reject");
        assert!(matches!(err, ToolSourceError::InvalidInput(_)));
    }

    /// ShellVerifyRunner 真实执行冒烟（Windows：cmd /S /C）。
    #[cfg(windows)]
    #[tokio::test]
    async fn shell_verify_runner_executes() {
        let runner = ShellVerifyRunner::default();
        let ok = runner.run("cmd /C exit 0").await;
        assert!(ok.ok, "{:?}", ok.output);
        let fail = runner.run("cmd /C exit 3").await;
        assert!(!fail.ok);
    }
}

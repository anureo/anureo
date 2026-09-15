//! P8 中立 goal 扩展 e2e（alignment 附录 C + Phase 8 验收）。
//!
//! 覆盖（稳定面）：
//! 1. `initialize` → `agentCapabilities._meta.goal` 能力块（version/
//!    controlMethod/actions）；
//! 2. `_session/goal` set → 响应中立快照 + `session_info_update._meta.goal`
//!    全量快照（camelCase / 毫秒 / controlMethod）经 session/update 推送；
//! 3. codex-acp legacy alias / pause / invalid action（兼容与错误面）；
//! 4. `_session/goal` clear → `goal: null` 清除快照。
//!
//! 续跑的关键终态也由本文件覆盖：预算耗尽会停止普通 continuation；模型
//! 在第二轮调用 `update_goal(complete)`、或在第三轮调用 `update_goal(blocked)`
//! 后同样必须停止后续 goal turn。

#[path = "e2e/common/mod.rs"]
mod common;

use std::time::Duration;

use common::{with_anureo_home, AcpTestHarness, MockLlmServer, TestEnv};
use common::jsonrpc::SessionNotification;
use serde_json::{json, Value};
use wiremock::Request;

/// `session_info_update._meta.goal`（裸 JSON：harness 通知保留原始 params）。
fn goal_meta(notif: &SessionNotification) -> Option<&Value> {
    notif.params["update"]["_meta"].get("goal")
}

/// 仅统计驱动真实 goal turn 的 LLM streaming 请求；标题与后台 review 都不应
/// 影响 continuation 的请求数断言。
fn is_goal_turn_chat_request(request: &Request) -> bool {
    request.url.path() == "/v1/chat/completions"
        && !std::str::from_utf8(&request.body)
            .unwrap_or("")
            .contains("Here is the conversation to review:")
        && std::str::from_utf8(&request.body)
            .unwrap_or("")
            .contains(r#""stream":true"#)
}

async fn goal_turn_chat_request_count(llm: &MockLlmServer) -> usize {
    llm.server
        .received_requests()
        .await
        .expect("read mock LLM requests")
        .into_iter()
        .filter(is_goal_turn_chat_request)
        .count()
}

async fn wait_for_goal_turn_chat_request_count(
    llm: &MockLlmServer,
    expected: usize,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let observed = goal_turn_chat_request_count(llm).await;
        if observed >= expected {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {expected} goal-turn LLM requests; observed {observed}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn neutral_goal_capability_control_and_snapshot_publication() {
    let env = TestEnv::setup();
    let llm = MockLlmServer::start().await;
    llm.mount_default_chat_completion().await;

    with_anureo_home(&env, async {
        let h = AcpTestHarness::spawn(&env, &llm.url()).await;

        // ── 1. initialize：_meta.goal 能力块（顶层 = codex-acp/FE 识别位置；
        //    agentCapabilities._meta 同形保留） ──────────────────────────
        let init = h
            .request(
                "initialize",
                json!({
                    "protocolVersion": 1,
                    "clientInfo": {"name": "goal-neutral-e2e", "version": "0.1.0"},
                    "capabilities": {}
                }),
            )
            .await;
        let goal_cap = &init["agentCapabilities"]["_meta"]["goal"];
        assert_eq!(
            goal_cap["version"].as_i64(),
            Some(1),
            "capability block: {init:#}"
        );
        assert_eq!(goal_cap["controlMethod"], "_session/goal");
        assert_eq!(
            goal_cap["actions"],
            json!(["set", "pause", "resume", "clear", "edit", "editBudget"]),
            "capability block: {init:#}"
        );
        let top_goal = &init["_meta"]["goal"];
        assert_eq!(top_goal["version"], 1, "top-level _meta.goal: {init:#}");
        assert_eq!(top_goal["controlMethod"], "_session/goal");

        // ── 2. session/new + _session/goal set → 中立快照发布 ──────────
        let session = h
            .request(
                "session/new",
                json!({"cwd": env.cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        let session_id = session["sessionId"].as_str().expect("sessionId").to_string();

        let unknown = h
            .request_raw(
                "_session/goal",
                json!({
                    "sessionId": "unknown-session",
                    "action": "set",
                    "objective": "must not create an orphan goal"
                }),
            )
            .await;
        assert_eq!(unknown["error"]["code"], -32602, "unknown session: {unknown}");

        let set = h
            .request(
                "_session/goal",
                json!({
                    "sessionId": session_id,
                    "action": "set",
                    "objective": "make steady progress",
                    "tokenBudget": 3,
                }),
            )
            .await;
        assert_eq!(set["goal"]["status"], "active", "set response: {set}");
        assert_eq!(set["goal"]["objective"], "make steady progress");
        assert_eq!(set["goal"]["controlMethod"], "_session/goal");
        assert_eq!(set["goal"]["tokenBudget"], 3);

        // set 触发的快照发布（active）：camelCase / 毫秒时间戳。
        let snap = h
            .wait_for_notification(
                |n| goal_meta(n).is_some_and(|g| g["status"] == "active"),
                Duration::from_secs(10),
            )
            .await;
        let g = goal_meta(&snap[0]).expect("goal meta");
        assert_eq!(g["objective"], "make steady progress");
        assert_eq!(g["controlMethod"], "_session/goal");
        assert_eq!(g["tokenBudget"], 3);
        assert!(
            g["createdAt"].as_i64().is_some_and(|t| t > 1_700_000_000_000),
            "createdAt must be unix ms: {g}"
        );

        // ── 3. pause → 快照翻转 paused；非法 action → 中立错误面 ───────
        let paused = h
            .request(
                "_session/goal",
                json!({"sessionId": session_id, "action": "pause"}),
            )
            .await;
        assert_eq!(paused["goal"]["status"], "paused", "pause response: {paused}");
        h.wait_for_notification(
            |n| goal_meta(n).is_some_and(|g| g["status"] == "paused"),
            Duration::from_secs(10),
        )
        .await;

        // resume 对 paused 合法；通过 codex-acp 未广播 legacy alias 调用。
        let resumed = h
            .request(
                "_codex/session/goal_control",
                json!({"sessionId": session_id, "action": "resume"}),
            )
            .await;
        assert_eq!(resumed["goal"]["status"], "active", "resume response: {resumed}");

        let raw = h
            .request_raw(
                "_session/goal",
                json!({"sessionId": session_id, "action": "purge"}),
            )
            .await;
        assert!(
            raw["error"].is_object(),
            "unknown action must be rejected: {raw}"
        );

        // ── 4. clear → goal: null 清除快照 ─────────────────────────────
        let cleared = h
            .request(
                "_session/goal",
                json!({"sessionId": session_id, "action": "clear"}),
            )
            .await;
        assert!(
            cleared["goal"].is_null(),
            "clear response must carry goal: null: {cleared}"
        );
        h.wait_for_notification(
            |n| goal_meta(n).is_some_and(|g| g.is_null()),
            Duration::from_secs(10),
        )
        .await;

        let status = h.shutdown().await;
        assert!(status.success(), "ACP process exited non-zero: {status:?}");
    })
    .await;
}

/// 真实 ACP 链路：stdio bridge → WebSocket server → mock LLM。
/// `set` 本身必须启动首轮，随后 active goal 自主续跑；两轮各记 2 tokens，
/// 达到 budget=3 后发布 limited 快照并停止普通 continuation。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn neutral_goal_set_starts_and_accounts_across_turns() {
    let env = TestEnv::setup();
    let llm = MockLlmServer::start().await;
    llm.mount_default_chat_completion().await;
    let usage_sse = MockLlmServer::sse_text_chunks_with_usage("ok", 1, 1, 2, None);
    llm.expect_chat_completion_sse_sequence(vec![
        usage_sse.clone(),
        usage_sse.clone(),
        usage_sse,
    ])
    .await;

    with_anureo_home(&env, async {
        let h = AcpTestHarness::spawn(&env, &llm.url()).await;
        h.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "goal-basic-e2e", "version": "0.1.0"},
                "capabilities": {}
            }),
        )
        .await;
        let session = h
            .request(
                "session/new",
                json!({"cwd": env.cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        let session_id = session["sessionId"].as_str().expect("sessionId");

        h.request(
            "_session/goal",
            json!({
                "sessionId": session_id,
                "action": "set",
                "objective": "finish the bounded mock task",
                "tokenBudget": 3
            }),
        )
        .await;
        let limited = h
            .wait_for_notification(
                |n| {
                    goal_meta(n).is_some_and(|goal| {
                        goal["status"] == "limited"
                            && goal["tokensUsed"].as_i64().is_some_and(|used| used >= 4)
                    })
                },
                Duration::from_secs(30),
            )
            .await;
        let goal = goal_meta(&limited[0]).expect("limited goal snapshot");
        assert_eq!(goal["statusReason"], "budget_limited", "snapshot: {goal}");

        h.request(
            "_session/goal",
            json!({"sessionId": session_id, "action": "clear"}),
        )
        .await;
        let status = h.shutdown().await;
        assert!(status.success(), "ACP process exited non-zero: {status:?}");
    })
    .await;
}

/// 真实 ACP 链路：第一轮只汇报进展，idle 钩子必须启动第二轮；第二轮通过
/// `update_goal(complete)` 结束目标。完成快照落地后，不得再启动第三个
/// goal-driven turn。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn neutral_goal_completes_after_multiple_turns_and_stops_continuing() {
    let env = TestEnv::setup();
    let llm = MockLlmServer::start().await;
    llm.mount_default_chat_completion().await;

    // 一个顺序 mock 为三次 LLM 调用依次提供响应：turn 1 文本结束 → turn 2
    // 发起 update_goal → tool result 后的最终文本。
    let progress_sse =
        MockLlmServer::sse_text_chunks_with_usage("I will continue.", 1, 1, 2, None);
    let complete_sse = MockLlmServer::sse_tool_call_chunks(
        "update_goal",
        r#"{"status":"complete","completion_budget_report":"Delivered the bounded mock task."}"#,
    );
    let final_sse = MockLlmServer::sse_text_chunks("The bounded mock task is complete.");
    llm.expect_chat_completion_sse_sequence(vec![progress_sse, complete_sse, final_sse])
        .await;

    with_anureo_home(&env, async {
        let h = AcpTestHarness::spawn(&env, &llm.url()).await;
        h.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "goal-complete-e2e", "version": "0.1.0"},
                "capabilities": {}
            }),
        )
        .await;
        let session = h
            .request(
                "session/new",
                json!({"cwd": env.cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        let session_id = session["sessionId"].as_str().expect("sessionId");

        h.request(
            "_session/goal",
            json!({
                "sessionId": session_id,
                "action": "set",
                "objective": "finish the bounded mock task"
            }),
        )
        .await;

        let completed = h
            .wait_for_notification(
                |n| goal_meta(n).is_some_and(|goal| goal["status"] == "complete"),
                Duration::from_secs(30),
            )
            .await;
        let goal = goal_meta(&completed[0]).expect("complete goal snapshot");
        assert_eq!(goal["iterationCount"], 2, "snapshot: {goal}");
        assert_eq!(goal["tokensUsed"], 2, "snapshot: {goal}");

        // 让 turn 尾部的 continue_if_idle 钩子有机会运行；complete 必须令它
        // no-op。忽略后台 review 后，三次主 agent 请求正好是两轮（第二轮带
        // 一次 tool-result follow-up）；任何第四次都意味着错误地启动了第三轮。
        tokio::time::sleep(Duration::from_millis(750)).await;
        let goal_chat_requests = goal_turn_chat_request_count(&llm).await;
        assert_eq!(
            goal_chat_requests, 3,
            "complete must stop further goal continuations"
        );

        let status = h.shutdown().await;
        assert!(status.success(), "ACP process exited non-zero: {status:?}");
    })
    .await;
}

/// 真实 ACP 链路：模型在连续三个 goal turn 报告同一外部依赖阻塞后，第三轮
/// 才用 `update_goal(blocked)` 终止目标。blocked 快照发布后不得再续跑。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn neutral_goal_blocks_after_three_turns_and_stops_continuing() {
    let env = TestEnv::setup();
    let llm = MockLlmServer::start().await;
    llm.mount_default_chat_completion().await;

    let first_turn = MockLlmServer::sse_text_chunks_with_usage(
        "The same external dependency is unavailable; I will retry.",
        1,
        1,
        2,
        None,
    );
    let second_turn = MockLlmServer::sse_text_chunks_with_usage(
        "The same external dependency remains unavailable.",
        1,
        1,
        2,
        None,
    );
    let blocked_turn = MockLlmServer::sse_tool_call_chunks(
        "update_goal",
        r#"{"status":"blocked","reason":"the same external dependency remained unavailable across three goal turns"}"#,
    );
    let final_sse = MockLlmServer::sse_text_chunks("The goal is blocked on that dependency.");
    llm.expect_chat_completion_sse_sequence(vec![
        first_turn,
        second_turn,
        blocked_turn,
        final_sse,
    ])
    .await;

    with_anureo_home(&env, async {
        let h = AcpTestHarness::spawn(&env, &llm.url()).await;
        h.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "goal-blocked-e2e", "version": "0.1.0"},
                "capabilities": {}
            }),
        )
        .await;
        let session = h
            .request(
                "session/new",
                json!({"cwd": env.cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        let session_id = session["sessionId"].as_str().expect("sessionId");

        h.request(
            "_session/goal",
            json!({
                "sessionId": session_id,
                "action": "set",
                "objective": "complete the externally dependent bounded task"
            }),
        )
        .await;

        let blocked = h
            .wait_for_notification(
                |n| goal_meta(n).is_some_and(|goal| goal["status"] == "blocked"),
                Duration::from_secs(30),
            )
            .await;
        let goal = goal_meta(&blocked[0]).expect("blocked goal snapshot");
        assert_eq!(goal["iterationCount"], 3, "snapshot: {goal}");
        assert_eq!(goal["tokensUsed"], 4, "snapshot: {goal}");
        assert_eq!(
            goal["statusReason"],
            "the same external dependency remained unavailable across three goal turns",
            "snapshot: {goal}"
        );

        tokio::time::sleep(Duration::from_millis(750)).await;
        let goal_chat_requests = goal_turn_chat_request_count(&llm).await;
        assert_eq!(
            goal_chat_requests, 4,
            "blocked must stop further goal continuations"
        );

        let status = h.shutdown().await;
        assert!(status.success(), "ACP process exited non-zero: {status:?}");
    })
    .await;
}

/// 真实 ACP 链路：pause 必须拦住 in-flight goal turn 的尾部续跑；resume 后才
/// 能启动下一轮，最终由模型 complete。此处首轮 LLM 延迟，避免测试只覆盖到
/// 空闲窗口而遗漏 turn-finish 与 pause 的竞争。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn neutral_goal_pause_blocks_continuation_until_resume() {
    let env = TestEnv::setup();
    let llm = MockLlmServer::start().await;
    llm.mount_default_chat_completion().await;

    let first_turn = MockLlmServer::sse_text_chunks("First turn finished while paused.");
    let complete_turn = MockLlmServer::sse_tool_call_chunks(
        "update_goal",
        r#"{"status":"complete","completion_budget_report":"Completed after the user resumed the goal."}"#,
    );
    let final_sse = MockLlmServer::sse_text_chunks("Completed after resume.");
    llm.expect_chat_completion_sse_sequence_with_delays(vec![
        (first_turn, Some(Duration::from_secs(3))),
        (complete_turn, None),
        (final_sse, None),
    ])
    .await;

    with_anureo_home(&env, async {
        let h = AcpTestHarness::spawn(&env, &llm.url()).await;
        h.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "goal-pause-resume-e2e", "version": "0.1.0"},
                "capabilities": {}
            }),
        )
        .await;
        let session = h
            .request(
                "session/new",
                json!({"cwd": env.cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        let session_id = session["sessionId"].as_str().expect("sessionId");

        h.request(
            "_session/goal",
            json!({
                "sessionId": session_id,
                "action": "set",
                "objective": "finish only after the user resumes the goal"
            }),
        )
        .await;
        h.wait_for_notification(
            |n| {
                goal_meta(n).is_some_and(|goal| {
                    goal["status"] == "active" && goal["iterationCount"] == 1
                })
            },
            Duration::from_secs(10),
        )
        .await;
        wait_for_goal_turn_chat_request_count(&llm, 1, Duration::from_secs(10)).await;

        let paused = h
            .request(
                "_session/goal",
                json!({"sessionId": session_id, "action": "pause"}),
            )
            .await;
        assert_eq!(paused["goal"]["status"], "paused", "pause response: {paused}");

        // 首轮已在执行，等它结束；paused 不得使其 turn-finish 再启动第二轮。
        tokio::time::sleep(Duration::from_millis(3_500)).await;
        let paused_goal_turns = goal_turn_chat_request_count(&llm).await;
        assert_eq!(
            paused_goal_turns, 1,
            "pause must stop continuation after the in-flight turn"
        );

        h.request(
            "_session/goal",
            json!({"sessionId": session_id, "action": "resume"}),
        )
        .await;
        let completed = h
            .wait_for_notification(
                |n| goal_meta(n).is_some_and(|goal| goal["status"] == "complete"),
                Duration::from_secs(30),
            )
            .await;
        let goal = goal_meta(&completed[0]).expect("complete goal snapshot");
        assert_eq!(goal["iterationCount"], 2, "snapshot: {goal}");

        tokio::time::sleep(Duration::from_millis(750)).await;
        let resumed_goal_turns = goal_turn_chat_request_count(&llm).await;
        assert_eq!(
            resumed_goal_turns, 3,
            "resume must start exactly one final goal turn"
        );

        let status = h.shutdown().await;
        assert!(status.success(), "ACP process exited non-zero: {status:?}");
    })
    .await;
}

//! P8 中立 goal 扩展 e2e（alignment 附录 C + Phase 8 验收）。
//!
//! 覆盖（稳定面）：
//! 1. `initialize` → `agentCapabilities._meta.goal` 能力块（version/
//!    controlMethod/actions）；
//! 2. `_session/goal` set → 响应中立快照 + `session_info_update._meta.goal`
//!    全量快照（camelCase / 毫秒 / controlMethod）经 session/update 推送；
//! 3. pause / invalid action（中立错误面）；
//! 4. `_session/goal` clear → `goal: null` 清除快照。
//!
//! **idle 续跑全链路（prompt 外 turn + budget_limited 投影）不在此覆盖**：
//! 2026-09-08 调查结论——turn 1 完成后宿主钩子/续跑 turn 在并行负载
//! （多 workflow 进程 + Defender）下触发时序不确定（0.05s~21s+），且
//! 测试超时拆除（TempDir 清理删 cwd）会与续跑 turn 竞态出伪 blocked，
//! 无法稳定断言；续跑链路已由 goal crate 单测（runtime gates/deferral/
//! budget flip/steering）+ 本文件快照发布面覆盖。全链路 e2e 待后续
//! 配合可控时钟/独立负载环境补（见 todo P8 进度记录）。

#[path = "e2e/common/mod.rs"]
mod common;

use std::time::Duration;

use common::{with_anureo_home, AcpTestHarness, MockLlmServer, TestEnv};
use common::jsonrpc::SessionNotification;
use serde_json::{json, Value};

/// `session_info_update._meta.goal`（裸 JSON：harness 通知保留原始 params）。
fn goal_meta(notif: &SessionNotification) -> Option<&Value> {
    notif.params["update"]["_meta"].get("goal")
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
            json!(["set", "pause", "resume", "clear"]),
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

        // resume 对 paused 合法（暂停态可恢复），回到 active。
        let resumed = h
            .request(
                "_session/goal",
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

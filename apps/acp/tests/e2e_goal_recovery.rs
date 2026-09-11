//! P5b 重对接：`_anureo.dev/goal/*` 的持久性来自 thread_goals 表
//! （`<anureo_home>/tasks/tasks.db`）。旧的 goals.json 重启恢复预约机制已
//! 退役——goal 天然持久，跨进程 `get` 直接读表。
//!
//! 流程：进程 1 `goal/start`（绑定 session）→ 退出 → 进程 2 `goal/get`
//! 读到同一 goal（status=active）→ `goal/cancel` 清除 → `goal/get` 404。

#[path = "e2e/common/mod.rs"]
mod common;

use std::time::Duration;

use common::{with_anureo_home, AcpTestHarness, MockLlmServer, TestEnv};
use common::jsonrpc::SessionNotification;
use serde_json::{json, Value};

fn goal_meta(notif: &SessionNotification) -> Option<&Value> {
    notif.params["update"]["_meta"].get("goal")
}

fn initialize_params() -> serde_json::Value {
    json!({
        "protocolVersion": 1,
        "clientInfo": {"name": "goal-persist-e2e", "version": "0.1.0"},
        "capabilities": {"session": {"resume": {}}}
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn goal_persists_across_acp_process_restart() {
    let env = TestEnv::setup();
    // 无 prompt 流量，但 spawn 需要一个 llm_url。
    let llm = MockLlmServer::start().await;

    with_anureo_home(&env, async {
        // ── 进程 1：初始化 + 建会话 + 启动 goal ────────────────────────
        let first = AcpTestHarness::spawn(&env, &llm.url()).await;
        first.request("initialize", initialize_params()).await;
        let session = first
            .request(
                "session/new",
                json!({"cwd": env.cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        let session_id = session["sessionId"].as_str().expect("sessionId").to_string();

        let started = first
            .request(
                "_anureo.dev/goal/start",
                json!({
                    "title": "persistent goal",
                    "description": "survive an ACP process restart",
                    "sessionId": session_id,
                }),
            )
            .await;
        let goal_id = started["id"].as_str().expect("goal id").to_string();
        assert_eq!(started["status"], "active", "start response: {started}");

        let status = first.shutdown().await;
        assert!(status.success(), "first ACP process exited non-zero: {status:?}");

        // ── 进程 2：新进程直接 get 到持久 goal（thread_goals 表）──────
        let second = AcpTestHarness::spawn(&env, &llm.url()).await;
        second.request("initialize", initialize_params()).await;
        second
            .request(
                "session/load",
                json!({
                    "sessionId": session_id,
                    "cwd": env.cwd.to_string_lossy(),
                    "mcpServers": []
                }),
            )
            .await;
        let restored = second
            .wait_for_notification(
                |notification| {
                    goal_meta(notification)
                        .is_some_and(|goal| goal["status"] == "active")
                },
                Duration::from_secs(10),
            )
            .await;
        assert_eq!(
            goal_meta(&restored[0]).expect("restored goal")["objective"],
            "persistent goal: survive an ACP process restart"
        );

        let got = second
            .request("_anureo.dev/goal/get", json!({"id": goal_id}))
            .await;
        assert_eq!(got["status"], "active", "goal must persist in thread_goals: {got}");
        assert!(
            got["description"]
                .as_str()
                .is_some_and(|d| d.contains("survive an ACP process restart")),
            "persisted objective mismatch: {got}"
        );

        // cancel 清除（clear 语义），随后 get 报 not found。
        let cancelled = second
            .request(
                "_anureo.dev/goal/cancel",
                json!({"id": goal_id, "reason": "recovery test"}),
            )
            .await;
        assert_eq!(cancelled["status"], "cancelled");

        let raw = second
            .request_raw("_anureo.dev/goal/get", json!({"id": goal_id}))
            .await;
        assert_eq!(
            raw["error"]["code"].as_i64(),
            Some(-32003),
            "cleared goal must be gone from thread_goals: {raw}"
        );

        let status = second.shutdown().await;
        assert!(status.success(), "second ACP process exited non-zero: {status:?}");
    })
    .await;
}

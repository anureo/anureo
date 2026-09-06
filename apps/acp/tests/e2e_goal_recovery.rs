//! Process-level regression for persisted ACP goal recovery.
//!
//! The fixture represents the state left by a process that stopped while a
//! goal was active. A fresh ACP process must claim it during `session/load`;
//! an explicit goal cancel then reaches that recovered runtime and transitions
//! the task checkpoint to `cancelled`.

#[path = "e2e/common/mod.rs"]
mod common;

use std::time::Duration;

use common::{with_anureo_home, AcpTestHarness, TestEnv};
use serde_json::json;
use task_core::{CreateParams, TaskDb, TaskStatus};

async fn initialize(harness: &AcpTestHarness) {
    harness
        .request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientInfo": {"name": "goal-recovery-e2e", "version": "0.1.0"},
                "capabilities": {"session": {"resume": {}}}
            }),
        )
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[serial_test::serial]
async fn session_load_reclaims_active_goal_and_cancel_reaches_task() {
    let env = TestEnv::setup();

    with_anureo_home(&env, async {
        // First process creates the durable ACP session that will later be
        // loaded. No model request is needed for session creation.
        let first = AcpTestHarness::spawn(&env, "http://127.0.0.1:9").await;
        initialize(&first).await;
        let session = first
            .request(
                "session/new",
                json!({"cwd": env.cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        let session_id = session["sessionId"]
            .as_str()
            .expect("session id")
            .to_string();
        let status = first.shutdown().await;
        assert!(status.success(), "first ACP process exited non-zero: {status:?}");

        // Simulate the durable snapshot left by a process that stopped while
        // the goal runner and its task were active.
        let task_id = "goal-recovery-task";
        let db_path = env.anureo_home().join("tasks").join("tasks.db");
        std::fs::create_dir_all(db_path.parent().expect("task db parent"))
            .expect("create task db parent");
        let db = TaskDb::open(&db_path).await.expect("open task db");
        db.create_task_with_id(
            task_id,
            &CreateParams {
                name: "recover me".to_string(),
                description: "recover me".to_string(),
                status: TaskStatus::InProgress,
                ..Default::default()
            },
        )
        .await
        .expect("create active goal task");

        let goal_id = "goal-process-recovery";
        let goal_dir = env.cwd.join(".anureo");
        std::fs::create_dir_all(&goal_dir).expect("create goal store directory");
        std::fs::write(
            goal_dir.join("goals.json"),
            serde_json::to_vec_pretty(&json!({
                "goals": [{
                    "id": goal_id,
                    "title": "recover me",
                    "description": "recover me",
                    "status": "active",
                    "createdAt": "2026-09-06T00:00:00Z",
                    "updatedAt": "2026-09-06T00:00:00Z",
                    "sessionIds": [session_id.clone()],
                    "progress": null,
                    "metadata": {
                        "source": "acp_goal_runner",
                        "taskId": task_id,
                        "model": "openai/gpt-4o",
                        "effort": "medium"
                    },
                    "workingDirectory": env.cwd.to_string_lossy()
                }]
            }))
            .expect("serialize goal store"),
        )
        .expect("write goal store");

        // A new process has an empty runtime registry. session/load must claim
        // the persisted active goal before returning the response.
        let second = AcpTestHarness::spawn(&env, "http://127.0.0.1:9").await;
        initialize(&second).await;
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
        second
            .request(
                "_anureo.dev/goal/cancel",
                json!({"id": goal_id, "reason": "recovery test"}),
            )
            .await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let task = db.show_task(task_id).await.expect("read recovered task");
            if task.status == TaskStatus::Cancelled {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "recovered goal cancel did not reach task; status={}",
                task.status
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let goal = second
            .request("_anureo.dev/goal/get", json!({"id": goal_id}))
            .await;
        assert_eq!(goal["status"], "cancelled");

        let status = second.shutdown().await;
        assert!(status.success(), "second ACP process exited non-zero: {status:?}");
    })
    .await;
}

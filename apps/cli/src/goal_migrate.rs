//! `anureo goal migrate`（goal-codex-alignment P6）。
//!
//! 一次性把两类旧 goal 存储写入 `thread_goals`（`<anureo_home>/tasks/tasks.db`）：
//!
//! 1. **task meta 的 GoalMeta**（旧 detached runner，`agent-core goal_runner`）：
//!    thread_id = task.id（task ID 即 agent thread ID），
//!    objective = task.description（缺省回退 task.name）；
//!    lifecycle 映射：Active→active、Paused→paused、Blocked→blocked、
//!    UsageLimited→usage_limited、Completed→complete、Failed→blocked
//!    （lifecycle_reason 进 status_reason）、Cancelled→跳过（= clear）。
//!    注：用户指令曾把 Failed 归入跳过；按 alignment §7.2 语义取 blocked。
//! 2. **goals.json**（旧 `_anureo.dev/goal/*` 文件后端）：cwd/.anureo/goals.json
//!    优先，其次 anureo_home()/goals.json；status 映射：pending/active→active、
//!    paused→paused、completed→complete、failed→blocked、cancelled→跳过；
//!    thread_id = session_ids 首元素（无则跳过并 warn）；objective =
//!    description（缺省回退 title）；保留原 goal id。
//!
//! 幂等：目标 thread 已有 goal 行则跳过。迁移前把 tasks.db（连同 WAL 边车）
//! 备份为 `tasks.db.bak-<时间戳>`。源数据一律不删（goals.json 保留原样但新
//! 路径不再读取）。
//!
//! 注意：迁移假定没有并发写入者（先停 server/ACP 进程再执行）。

use std::path::{Path, PathBuf};

use agent::goal_runner::state::{GoalLifecycle, GoalMeta};
use goal::{GoalStatus, GoalStore};
use task_core::{ListParams, TaskDb};

/// 旧 goals.json 条目（wire 形状子集，camelCase；本地定义避免依赖
/// anureo-acp crate）。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct LegacyGoalJson {
    id: String,
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    session_ids: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct LegacyGoalsFile {
    #[serde(default)]
    goals: Vec<LegacyGoalJson>,
}

#[derive(Debug, Default)]
struct MigrateSummary {
    task_meta_migrated: u32,
    task_meta_skipped_cancelled: u32,
    task_meta_existing: u32,
    json_migrated: u32,
    json_skipped_cancelled: u32,
    json_skipped_no_session: u32,
    json_existing: u32,
}

impl MigrateSummary {
    fn total_migrated(&self) -> u32 {
        self.task_meta_migrated + self.json_migrated
    }
}

pub(crate) async fn run_migrate() -> Result<(), Box<dyn std::error::Error>> {
    let home = config::home::anureo_home();
    let tasks_dir = home.join("tasks");
    std::fs::create_dir_all(&tasks_dir)?;
    let db_path = tasks_dir.join("tasks.db");

    // ── 备份（在任何写入前；连同 WAL 边车，静态拷贝）────────────────
    if db_path.exists() {
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        backup_file(&db_path, &format!("tasks.db.bak-{ts}"))?;
        for sidecar in ["tasks.db-wal", "tasks.db-shm"] {
            let p = tasks_dir.join(sidecar);
            if p.exists() {
                backup_file(&p, &format!("{sidecar}.bak-{ts}"))?;
            }
        }
    }

    let db = TaskDb::open(&db_path).await?;
    let store = GoalStore::from_task_db(&db);
    let mut summary = MigrateSummary::default();

    migrate_task_meta(&db, &store, &mut summary).await?;
    migrate_goals_json(&store, &mut summary).await?;

    println!(
        "goal 迁移完成：\n  task meta（GoalMeta）：迁移 {}，跳过(cancelled) {}，已存在 {}\n  goals.json：迁移 {}，跳过(cancelled) {}，跳过(无 session) {}，已存在 {}\n  合计写入 {} 行到 thread_goals",
        summary.task_meta_migrated,
        summary.task_meta_skipped_cancelled,
        summary.task_meta_existing,
        summary.json_migrated,
        summary.json_skipped_cancelled,
        summary.json_skipped_no_session,
        summary.json_existing,
        summary.total_migrated(),
    );
    println!("源数据未删除（task meta 与 goals.json 原样保留）。");
    Ok(())
}

fn backup_file(src: &Path, backup_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dst = src.with_file_name(backup_name);
    std::fs::copy(src, &dst)?;
    eprintln!("备份：{} -> {}", src.display(), dst.display());
    Ok(())
}

/// 源 1：task meta 里的 GoalMeta。
async fn migrate_task_meta(
    db: &TaskDb,
    store: &GoalStore,
    summary: &mut MigrateSummary,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut page = 1u32;
    loop {
        let params = ListParams {
            limit: 200,
            page,
            ..ListParams::default()
        };
        let list = db.list_tasks(&params).await?;
        if list.tasks.is_empty() {
            break;
        }
        let fetched = list.tasks.len();
        for task in &list.tasks {
            let meta = task.metadata_value();
            let Some(goal_meta_value) = meta.get("goal") else {
                continue;
            };
            let goal_meta: GoalMeta = match serde_json::from_value(goal_meta_value.clone()) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "warn: task {} 的 goal meta 解析失败，跳过：{e}",
                        task.id
                    );
                    continue;
                }
            };

            use GoalLifecycle as L;
            let (status, reason) = match goal_meta.lifecycle {
                L::Active => (GoalStatus::Active, None),
                L::Paused => (GoalStatus::Paused, None),
                L::Blocked => (GoalStatus::Blocked, goal_meta.lifecycle_reason.clone()),
                L::UsageLimited => (GoalStatus::UsageLimited, goal_meta.lifecycle_reason.clone()),
                L::Completed => (GoalStatus::Complete, None),
                // alignment §7.2：Failed → blocked（原因进 status_reason）。
                L::Failed => (
                    GoalStatus::Blocked,
                    Some(
                        goal_meta
                            .lifecycle_reason
                            .clone()
                            .unwrap_or_else(|| "migrated from GoalLifecycle::Failed".to_string()),
                    ),
                ),
                // Cancelled → clear 语义，不迁移。
                L::Cancelled => {
                    summary.task_meta_skipped_cancelled += 1;
                    continue;
                }
            };

            // task ID 即 agent thread ID（goal_cmd 约定）。
            let thread_id = task.id.clone();
            if store.read(&thread_id).await?.is_some() {
                summary.task_meta_existing += 1;
                continue;
            }
            let objective = if !task.description.trim().is_empty() {
                task.description.trim().to_string()
            } else {
                task.name.trim().to_string()
            };
            let goal_id = format!("goal-{}", uuid::Uuid::new_v4());
            store
                .insert_migrated(
                    &thread_id,
                    &goal_id,
                    &objective,
                    goal_meta.token_budget.map(i64::from),
                    i64::from(goal_meta.tokens_used),
                    goal_meta.time_used_seconds,
                    goal_meta.verify_command.as_deref(),
                    status,
                    reason.as_deref(),
                )
                .await?;
            summary.task_meta_migrated += 1;
        }
        if fetched < 200 {
            break;
        }
        page += 1;
    }
    Ok(())
}

/// 源 2：goals.json（cwd/.anureo 优先，其次 home）。
async fn migrate_goals_json(
    store: &GoalStore,
    summary: &mut MigrateSummary,
) -> Result<(), Box<dyn std::error::Error>> {
    for path in goals_json_candidates() {
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(Box::new(e)),
        };
        let file: LegacyGoalsFile = serde_json::from_str(&content).map_err(|e| {
            format!("解析 {} 失败：{e}", path.display())
        })?;
        eprintln!("读取旧 goal 存储：{}（{} 条）", path.display(), file.goals.len());

        for goal in file.goals {
            let status = match goal.status.trim().to_lowercase().as_str() {
                "pending" | "active" => GoalStatus::Active,
                "paused" => GoalStatus::Paused,
                "completed" => GoalStatus::Complete,
                "failed" => GoalStatus::Blocked,
                "cancelled" => {
                    summary.json_skipped_cancelled += 1;
                    continue;
                }
                other => {
                    eprintln!("warn: goal {} 状态 “{other}” 未知，跳过", goal.id);
                    continue;
                }
            };
            let Some(thread_id) = goal.session_ids.first().cloned() else {
                eprintln!(
                    "warn: goal {}（{}）无 sessionIds，无 thread 可绑，跳过",
                    goal.id, goal.title
                );
                summary.json_skipped_no_session += 1;
                continue;
            };
            if store.read(&thread_id).await?.is_some() {
                summary.json_existing += 1;
                continue;
            }
            let objective = if !goal.description.trim().is_empty() {
                goal.description.trim().to_string()
            } else {
                goal.title.trim().to_string()
            };
            store
                .insert_migrated(
                    &thread_id,
                    &goal.id,
                    &objective,
                    None,
                    0,
                    0,
                    None,
                    status,
                    Some("migrated from goals.json"),
                )
                .await?;
            summary.json_migrated += 1;
        }
    }
    Ok(())
}

fn goals_json_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        paths.push(cwd.join(".anureo").join("goals.json"));
    }
    paths.push(config::home::anureo_home().join("goals.json"));
    paths
}

use std::path::Path;

use chrono::Local;
use thiserror::Error;

use crate::models::{Task, TaskStatus};
use crate::params::{CreateParams, ListParams, UpdateParams};

#[derive(Debug, Error)]
pub enum TaskDbError {
    #[error("database error: {0}")]
    Sqlx(#[from] sqlx::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Other(String),
}

pub struct TaskDb {
    pool: sqlx::SqlitePool,
    db_path: std::path::PathBuf,
}

impl TaskDb {
    pub async fn open(db_path: &Path) -> Result<Self, TaskDbError> {
        let url = format!("sqlite:{}?mode=rwc", db_path.display());
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&url)
            .await?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| TaskDbError::Other(e.to_string()))?;

        Ok(Self {
            pool,
            db_path: db_path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.db_path
    }

    pub async fn create_task(&self, p: &CreateParams) -> Result<Task, TaskDbError> {
        self.create_task_with_id(&uuid::Uuid::new_v4().to_string(), p)
            .await
    }

    /// Create a task with a caller-supplied stable ID.
    ///
    /// Goal runners use this to bind the persisted task to an external
    /// scheduler or an agent session. The database's primary-key constraint
    /// rejects accidental reuse instead of silently creating a second task.
    pub async fn create_task_with_id(
        &self,
        id: &str,
        p: &CreateParams,
    ) -> Result<Task, TaskDbError> {
        let now = Local::now().to_rfc3339();
        let start_time = p
            .start_time
            .as_deref()
            .map(parse_time_input)
            .transpose()
            .map_err(TaskDbError::Other)?
            .unwrap_or_else(|| now.clone());
        let status_str = p.status.as_str().to_string();

        sqlx::query(
            "INSERT INTO tasks (id, name, description, assignee, start_time, created_at, status, metadata) VALUES (?, ?, ?, ?, ?, ?, ?, '{}')",
        )
        .bind(id)
        .bind(&p.name)
        .bind(&p.description)
        .bind(&p.assignee)
        .bind(&start_time)
        .bind(&now)
        .bind(&status_str)
        .execute(&self.pool)
        .await?;

        Ok(Task {
            id: id.to_string(),
            name: p.name.clone(),
            description: p.description.clone(),
            assignee: p.assignee.clone(),
            start_time,
            created_at: now,
            status: p.status,
            metadata: "{}".to_string(),
        })
    }

    pub async fn show_task(&self, id_prefix: &str) -> Result<Task, ShowError> {
        let tasks = self.find_by_id_prefix(id_prefix).await?;
        match tasks.len() {
            0 => Err(ShowError::NotFound(id_prefix.to_string())),
            1 => Ok(tasks.into_iter().next().unwrap()),
            _ => Err(ShowError::Ambiguous {
                prefix: id_prefix.to_string(),
                matches: tasks
                    .into_iter()
                    .map(|t| (t.id.clone(), t.name.clone()))
                    .collect(),
            }),
        }
    }

    pub async fn list_tasks(&self, p: &ListParams) -> Result<TaskList, TaskDbError> {
        let mut where_clauses = Vec::new();
        let mut param_idx = 1usize;

        let status_filter;
        if let Some(ref status) = p.status {
            where_clauses.push(format!("status = ?{}", param_idx));
            status_filter = Some(status.as_str().to_string());
            param_idx += 1;
        } else {
            status_filter = None;
        }

        let assignee_filter;
        if let Some(ref assignee) = p.assignee {
            where_clauses.push(format!("assignee = ?{}", param_idx));
            assignee_filter = Some(assignee.clone());
            param_idx += 1;
        } else {
            assignee_filter = None;
        }

        let name_filter;
        if let Some(ref name) = p.name {
            where_clauses.push(format!("name LIKE ?{}", param_idx));
            name_filter = Some(format!("%{}%", name));
            param_idx += 1;
        } else {
            name_filter = None;
        }

        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        let count_sql = format!("SELECT COUNT(*) as count FROM tasks {}", where_sql);
        let total: i64 =
            if status_filter.is_some() || assignee_filter.is_some() || name_filter.is_some() {
                let mut q = sqlx::query_scalar::<_, i64>(&count_sql);
                if let Some(ref v) = status_filter {
                    q = q.bind(v);
                }
                if let Some(ref v) = assignee_filter {
                    q = q.bind(v);
                }
                if let Some(ref v) = name_filter {
                    q = q.bind(v);
                }
                q.fetch_one(&self.pool).await?
            } else {
                sqlx::query_scalar::<_, i64>(&count_sql)
                    .fetch_one(&self.pool)
                    .await?
            };
        let total = total as u32;

        let sort_field = match p.sort_by.as_str() {
            "start_time" => "start_time",
            "name" => "name",
            "status" => "status",
            _ => "created_at",
        };
        let sort_dir = match p.sort_order.as_str() {
            "asc" => "ASC",
            _ => "DESC",
        };

        let offset = (p.page.saturating_sub(1)) * p.limit;
        let data_sql = format!(
            "SELECT id, name, description, assignee, start_time, created_at, status, metadata FROM tasks {} ORDER BY {} {} LIMIT ?{} OFFSET ?{}",
            where_sql, sort_field, sort_dir, param_idx, param_idx + 1
        );

        let mut q = sqlx::query_as::<_, Task>(&data_sql);
        if let Some(ref v) = status_filter {
            q = q.bind(v);
        }
        if let Some(ref v) = assignee_filter {
            q = q.bind(v);
        }
        if let Some(ref v) = name_filter {
            q = q.bind(v);
        }
        q = q.bind(p.limit);
        q = q.bind(offset);

        let tasks = q.fetch_all(&self.pool).await?;

        Ok(TaskList {
            tasks,
            total,
            limit: p.limit,
            page: p.page,
            has_more: offset + p.limit < total,
        })
    }

    pub async fn update_task(
        &self,
        p: &UpdateParams,
    ) -> Result<Task, Box<dyn std::error::Error + Send + Sync>> {
        let existing = self
            .show_task(&p.id)
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) })?;
        let mut set_clauses = Vec::new();
        let mut param_idx = 1usize;

        let name_val;
        if let Some(ref name) = p.name {
            set_clauses.push(format!("name = ?{}", param_idx));
            name_val = Some(name.clone());
            param_idx += 1;
        } else {
            name_val = None;
        }

        let desc_val;
        if let Some(ref description) = p.description {
            set_clauses.push(format!("description = ?{}", param_idx));
            desc_val = Some(description.clone());
            param_idx += 1;
        } else {
            desc_val = None;
        }

        let assignee_val;
        if let Some(ref assignee) = p.assignee {
            set_clauses.push(format!("assignee = ?{}", param_idx));
            assignee_val = Some(assignee.clone());
            param_idx += 1;
        } else {
            assignee_val = None;
        }

        let start_time_val;
        if let Some(ref start_time) = p.start_time {
            let parsed = parse_time_input(start_time)?;
            set_clauses.push(format!("start_time = ?{}", param_idx));
            start_time_val = Some(parsed);
            param_idx += 1;
        } else {
            start_time_val = None;
        }

        let status_val;
        if let Some(ref status) = p.status {
            set_clauses.push(format!("status = ?{}", param_idx));
            status_val = Some(status.as_str().to_string());
            param_idx += 1;
        } else {
            status_val = None;
        }

        if set_clauses.is_empty() {
            return Ok(existing);
        }

        let sql = format!(
            "UPDATE tasks SET {} WHERE id = ?{}",
            set_clauses.join(", "),
            param_idx
        );

        let mut q = sqlx::query(&sql);
        if let Some(ref v) = name_val {
            q = q.bind(v);
        }
        if let Some(ref v) = desc_val {
            q = q.bind(v);
        }
        if let Some(ref v) = assignee_val {
            q = q.bind(v);
        }
        if let Some(ref v) = start_time_val {
            q = q.bind(v);
        }
        if let Some(ref v) = status_val {
            q = q.bind(v);
        }
        q = q.bind(&existing.id);

        q.execute(&self.pool).await?;

        self.show_task(&existing.id)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }

    pub async fn delete_task(&self, id_prefix: &str) -> Result<Task, ShowError> {
        let task = self.show_task(id_prefix).await?;
        sqlx::query("DELETE FROM tasks WHERE id = ?")
            .bind(&task.id)
            .execute(&self.pool)
            .await
            .map_err(|e| ShowError::DbError(e.to_string()))?;
        Ok(task)
    }

    pub async fn get_meta(
        &self,
        id_prefix: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, Box<dyn std::error::Error + Send + Sync>> {
        let task = self.show_task(id_prefix).await?;
        let meta: serde_json::Value = serde_json::from_str(&task.metadata)?;
        Ok(meta.get(key).cloned())
    }

    pub async fn set_meta(
        &self,
        id_prefix: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let task = self.show_task(id_prefix).await?;
        let mut metadata: serde_json::Value = serde_json::from_str(&task.metadata)?;
        if let Some(obj) = metadata.as_object_mut() {
            obj.insert(key.to_string(), value.clone());
        }
        let metadata_str = metadata.to_string();
        sqlx::query("UPDATE tasks SET metadata = ? WHERE id = ?")
            .bind(&metadata_str)
            .bind(&task.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn atomic_update_status(
        &self,
        id_prefix: &str,
        from: TaskStatus,
        to: TaskStatus,
    ) -> Result<bool, TaskDbError> {
        let like_pattern = format!("{}%", id_prefix);
        let matching_ids = sqlx::query_scalar::<_, String>(
            "SELECT id FROM tasks WHERE id LIKE ? ORDER BY id",
        )
        .bind(&like_pattern)
        .fetch_all(&self.pool)
        .await?;
        let id = match matching_ids.as_slice() {
            [] => return Ok(false),
            [id] => id,
            _ => {
                return Err(TaskDbError::Other(format!(
                    "ambiguous task id prefix '{}': matched {} pending tasks",
                    id_prefix,
                    matching_ids.len()
                )))
            }
        };
        let result = sqlx::query(
            "UPDATE tasks SET status = ? WHERE id = ? AND status = ?",
        )
        .bind(to.as_str())
        .bind(id)
        .bind(from.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn find_by_id_prefix(&self, prefix: &str) -> Result<Vec<Task>, ShowError> {
        let like_pattern = format!("{}%", prefix);
        sqlx::query_as::<_, Task>(
            "SELECT id, name, description, assignee, start_time, created_at, status, metadata FROM tasks WHERE id LIKE ?",
        )
        .bind(&like_pattern)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ShowError::DbError(e.to_string()))
    }
}

#[derive(Debug, serde::Serialize)]
pub struct TaskList {
    pub tasks: Vec<Task>,
    pub total: u32,
    pub limit: u32,
    pub page: u32,
    pub has_more: bool,
}

#[derive(Debug)]
pub enum ShowError {
    NotFound(String),
    Ambiguous {
        prefix: String,
        matches: Vec<(String, String)>,
    },
    DbError(String),
}

impl std::fmt::Display for ShowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShowError::NotFound(id) => write!(f, "task not found: {}", id),
            ShowError::Ambiguous { prefix, matches } => {
                writeln!(
                    f,
                    "ambiguous id '{}', matched {} tasks:",
                    prefix,
                    matches.len()
                )?;
                for (id, name) in matches {
                    writeln!(f, "  {} ... {}", &id[..8.min(id.len())], name)?;
                }
                Ok(())
            }
            ShowError::DbError(e) => write!(f, "database error: {}", e),
        }
    }
}

impl std::error::Error for ShowError {}

fn parse_time_input(input: &str) -> Result<String, String> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(input) {
        return Ok(dt.to_rfc3339());
    }

    let attempts = [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d",
    ];

    for fmt in &attempts {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(input, fmt) {
            let local = naive
                .and_local_timezone(Local)
                .single()
                .ok_or_else(|| format!("ambiguous local time: {}", input))?;
            return Ok(local.to_rfc3339());
        }
        if fmt == &"%Y-%m-%d" {
            if let Ok(date) = chrono::NaiveDate::parse_from_str(input, fmt) {
                let local = date
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
                    .and_local_timezone(Local)
                    .single()
                    .ok_or_else(|| format!("ambiguous local time: {}", input))?;
                return Ok(local.to_rfc3339());
            }
        }
    }

    Err(format!(
        "invalid time format: '{}'. Expected formats: 2025-08-20T10:00:00, 2025-08-20 10:00:00, 2025-08-20",
        input
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    async fn test_db() -> TaskDb {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        std::mem::forget(f);
        TaskDb::open(&path).await.unwrap()
    }

    #[tokio::test]
    async fn test_create_and_show() {
        let db = test_db().await;
        let task = db
            .create_task(&CreateParams {
                name: "test".into(),
                description: "desc".into(),
                assignee: "alice".into(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();

        let found = db.show_task(&task.id[..8]).await.unwrap();
        assert_eq!(found.name, "test");
        assert_eq!(found.status, TaskStatus::Pending);
    }

    #[tokio::test]
    async fn test_create_with_caller_supplied_id() {
        let db = test_db().await;
        let task = db
            .create_task_with_id(
                "scheduler-goal-42",
                &CreateParams {
                    name: "stable goal".into(),
                    description: "desc".into(),
                    assignee: String::new(),
                    start_time: None,
                    status: TaskStatus::InProgress,
                },
            )
            .await
            .unwrap();

        assert_eq!(task.id, "scheduler-goal-42");
        assert_eq!(db.show_task("scheduler-goal-42").await.unwrap().id, task.id);
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let db = test_db().await;
        for i in 0..3 {
            db.create_task(&CreateParams {
                name: format!("task-{}", i),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();
        }

        let list = db.list_tasks(&ListParams::default()).await.unwrap();
        assert_eq!(list.total, 3);
    }

    #[tokio::test]
    async fn test_update_task() {
        let db = test_db().await;
        let task = db
            .create_task(&CreateParams {
                name: "before".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();

        let updated = db
            .update_task(&UpdateParams {
                id: task.id.clone(),
                name: Some("after".into()),
                description: None,
                assignee: None,
                start_time: None,
                status: Some(TaskStatus::InProgress),
            })
            .await
            .unwrap();

        assert_eq!(updated.name, "after");
        assert_eq!(updated.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn test_delete_task() {
        let db = test_db().await;
        let task = db
            .create_task(&CreateParams {
                name: "to-delete".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();

        let deleted = db.delete_task(&task.id).await.unwrap();
        assert_eq!(deleted.name, "to-delete");

        assert!(db.show_task(&task.id).await.is_err());
    }

    #[tokio::test]
    async fn test_get_set_meta() {
        let db = test_db().await;
        let task = db
            .create_task(&CreateParams {
                name: "meta-test".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::InProgress,
            })
            .await
            .unwrap();

        let val = serde_json::json!({"iteration": 5, "tool": "anureo"});
        db.set_meta(&task.id, "goal", &val).await.unwrap();

        let got = db.get_meta(&task.id, "goal").await.unwrap().unwrap();
        assert_eq!(got["iteration"], 5);
        assert_eq!(got["tool"], "anureo");
    }

    #[tokio::test]
    async fn test_atomic_update_status() {
        let db = test_db().await;
        let task = db
            .create_task(&CreateParams {
                name: "atomic".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();

        let ok = db
            .atomic_update_status(&task.id, TaskStatus::Pending, TaskStatus::InProgress)
            .await
            .unwrap();
        assert!(ok);

        let ok = db
            .atomic_update_status(&task.id, TaskStatus::Pending, TaskStatus::InProgress)
            .await
            .unwrap();
        assert!(!ok);

        let found = db.show_task(&task.id).await.unwrap();
        assert_eq!(found.status, TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn test_atomic_update_status_rejects_ambiguous_prefix() {
        let db = test_db().await;
        let params = |name: &str| CreateParams {
            name: name.to_string(),
            description: String::new(),
            assignee: String::new(),
            start_time: None,
            status: TaskStatus::Pending,
        };
        db.create_task_with_id("goal-aaa", &params("a"))
            .await
            .unwrap();
        db.create_task_with_id("goal-aab", &params("b"))
            .await
            .unwrap();

        let error = db
            .atomic_update_status("goal-aa", TaskStatus::Pending, TaskStatus::InProgress)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ambiguous task id prefix"));

        assert_eq!(
            db.show_task("goal-aaa").await.unwrap().status,
            TaskStatus::Pending
        );
        assert_eq!(
            db.show_task("goal-aab").await.unwrap().status,
            TaskStatus::Pending
        );
    }

    #[tokio::test]
    async fn test_atomic_update_status_rejects_prefix_mixed_statuses() {
        let db = test_db().await;
        let pending = CreateParams {
            name: "pending".into(),
            description: String::new(),
            assignee: String::new(),
            start_time: None,
            status: TaskStatus::Pending,
        };
        db.create_task_with_id("mixed-a", &CreateParams { name: "a".into(), ..pending })
            .await
            .unwrap();
        db.create_task_with_id(
            "mixed-b",
            &CreateParams {
                name: "b".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Completed,
            },
        )
            .await
            .unwrap();

        let error = db
            .atomic_update_status("mixed-", TaskStatus::Pending, TaskStatus::InProgress)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("ambiguous task id prefix"));
        assert_eq!(
            db.show_task("mixed-a").await.unwrap().status,
            TaskStatus::Pending
        );
    }

    #[tokio::test]
    async fn test_migration_from_old_db() {
        let f = NamedTempFile::new().unwrap();
        let path = f.path().to_path_buf();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
                assignee TEXT NOT NULL DEFAULT '', start_time TEXT NOT NULL, created_at TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending');
            INSERT INTO tasks (id, name, description, assignee, start_time, created_at, status)
                VALUES ('old-id', 'old-task', '', '', '2025-01-01T00:00:00Z', '2025-01-01T00:00:00Z', 'pending');"
        ).unwrap();
        drop(conn);

        let db = TaskDb::open(&path).await.unwrap();
        let task = db.show_task("old-id").await.unwrap();
        assert_eq!(task.name, "old-task");
    }

    // ── Additional edge-case tests ──

    #[test]
    fn test_show_error_display_not_found() {
        let err = ShowError::NotFound("abc".to_string());
        assert_eq!(format!("{}", err), "task not found: abc");
    }

    #[test]
    fn test_show_error_display_db_error() {
        let err = ShowError::DbError("connection failed".to_string());
        assert_eq!(format!("{}", err), "database error: connection failed");
    }

    #[test]
    fn test_show_error_display_ambiguous() {
        let err = ShowError::Ambiguous {
            prefix: "ab".to_string(),
            matches: vec![
                ("abcdef01-1234".to_string(), "Task A".to_string()),
                ("abcd5678-9012".to_string(), "Task B".to_string()),
            ],
        };
        let s = format!("{}", err);
        assert!(s.contains("ambiguous id 'ab'"));
        assert!(s.contains("matched 2 tasks"));
        assert!(s.contains("Task A"));
        assert!(s.contains("Task B"));
    }

    #[test]
    fn test_parse_time_input_rfc3339() {
        let result = parse_time_input("2025-08-20T10:00:00Z").unwrap();
        assert!(result.starts_with("2025-08-20"));
    }

    #[test]
    fn test_parse_time_input_date_only() {
        let result = parse_time_input("2025-08-20").unwrap();
        assert!(result.starts_with("2025-08-20"));
    }

    #[test]
    fn test_parse_time_input_datetime_space() {
        let result = parse_time_input("2025-08-20 10:30:00").unwrap();
        assert!(result.starts_with("2025-08-20"));
    }

    #[test]
    fn test_parse_time_input_datetime_t_separator() {
        let result = parse_time_input("2025-08-20T10:30:00").unwrap();
        assert!(result.starts_with("2025-08-20"));
    }

    #[test]
    fn test_parse_time_input_hm_space() {
        let result = parse_time_input("2025-08-20 10:30").unwrap();
        assert!(result.starts_with("2025-08-20"));
    }

    #[test]
    fn test_parse_time_input_hm_t() {
        let result = parse_time_input("2025-08-20T10:30").unwrap();
        assert!(result.starts_with("2025-08-20"));
    }

    #[test]
    fn test_parse_time_input_invalid() {
        let err = parse_time_input("not-a-date").unwrap_err();
        assert!(err.contains("invalid time format"));
        assert!(err.contains("not-a-date"));
    }

    #[tokio::test]
    async fn test_show_task_not_found() {
        let db = test_db().await;
        let result = db.show_task("nonexistent").await;
        assert!(matches!(result, Err(ShowError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_show_task_ambiguous_prefix() {
        let db = test_db().await;
        // Create two tasks with known IDs
        let _t1 = db
            .create_task(&CreateParams {
                name: "Task 1".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();
        let _t2 = db
            .create_task(&CreateParams {
                name: "Task 2".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();

        // Use empty prefix which matches both
        let result = db.show_task("").await;
        assert!(matches!(result, Err(ShowError::Ambiguous { .. })));
    }

    #[tokio::test]
    async fn test_list_tasks_with_status_filter() {
        let db = test_db().await;
        db.create_task(&CreateParams {
            name: "pending-task".into(),
            description: String::new(),
            assignee: String::new(),
            start_time: None,
            status: TaskStatus::Pending,
        })
        .await
        .unwrap();
        db.create_task(&CreateParams {
            name: "done-task".into(),
            description: String::new(),
            assignee: String::new(),
            start_time: None,
            status: TaskStatus::Completed,
        })
        .await
        .unwrap();

        let list = db
            .list_tasks(&ListParams {
                status: Some(TaskStatus::Completed),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(list.total, 1);
        assert_eq!(list.tasks[0].name, "done-task");
    }

    #[tokio::test]
    async fn test_list_tasks_with_assignee_filter() {
        let db = test_db().await;
        db.create_task(&CreateParams {
            name: "alice-task".into(),
            description: String::new(),
            assignee: "alice".into(),
            start_time: None,
            status: TaskStatus::Pending,
        })
        .await
        .unwrap();
        db.create_task(&CreateParams {
            name: "bob-task".into(),
            description: String::new(),
            assignee: "bob".into(),
            start_time: None,
            status: TaskStatus::Pending,
        })
        .await
        .unwrap();

        let list = db
            .list_tasks(&ListParams {
                assignee: Some("alice".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(list.total, 1);
        assert_eq!(list.tasks[0].name, "alice-task");
    }

    #[tokio::test]
    async fn test_list_tasks_with_name_filter() {
        let db = test_db().await;
        db.create_task(&CreateParams {
            name: "alpha-task".into(),
            description: String::new(),
            assignee: String::new(),
            start_time: None,
            status: TaskStatus::Pending,
        })
        .await
        .unwrap();
        db.create_task(&CreateParams {
            name: "beta-task".into(),
            description: String::new(),
            assignee: String::new(),
            start_time: None,
            status: TaskStatus::Pending,
        })
        .await
        .unwrap();

        let list = db
            .list_tasks(&ListParams {
                name: Some("alpha".into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(list.total, 1);
        assert_eq!(list.tasks[0].name, "alpha-task");
    }

    #[tokio::test]
    async fn test_list_tasks_pagination() {
        let db = test_db().await;
        for i in 0..5 {
            db.create_task(&CreateParams {
                name: format!("task-{}", i),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();
        }

        // Page 1, limit 2
        let page1 = db
            .list_tasks(&ListParams {
                limit: 2,
                page: 1,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page1.tasks.len(), 2);
        assert_eq!(page1.total, 5);
        assert!(page1.has_more);

        // Page 3, limit 2
        let page3 = db
            .list_tasks(&ListParams {
                limit: 2,
                page: 3,
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(page3.tasks.len(), 1);
        assert!(!page3.has_more);
    }

    #[tokio::test]
    async fn test_list_tasks_sort_by_name_asc() {
        let db = test_db().await;
        db.create_task(&CreateParams {
            name: "charlie".into(),
            description: String::new(),
            assignee: String::new(),
            start_time: None,
            status: TaskStatus::Pending,
        })
        .await
        .unwrap();
        db.create_task(&CreateParams {
            name: "alpha".into(),
            description: String::new(),
            assignee: String::new(),
            start_time: None,
            status: TaskStatus::Pending,
        })
        .await
        .unwrap();

        let list = db
            .list_tasks(&ListParams {
                sort_by: "name".into(),
                sort_order: "asc".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(list.tasks[0].name, "alpha");
        assert_eq!(list.tasks[1].name, "charlie");
    }

    #[tokio::test]
    async fn test_update_task_no_changes_returns_existing() {
        let db = test_db().await;
        let task = db
            .create_task(&CreateParams {
                name: "unchanged".into(),
                description: "original".into(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();

        let updated = db
            .update_task(&UpdateParams {
                id: task.id.clone(),
                name: None,
                description: None,
                assignee: None,
                start_time: None,
                status: None,
            })
            .await
            .unwrap();

        assert_eq!(updated.name, "unchanged");
        assert_eq!(updated.description, "original");
    }

    #[tokio::test]
    async fn test_delete_nonexistent_task() {
        let db = test_db().await;
        let result = db.delete_task("nonexistent-id").await;
        assert!(matches!(result, Err(ShowError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_get_meta_nonexistent_key() {
        let db = test_db().await;
        let task = db
            .create_task(&CreateParams {
                name: "meta".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: None,
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();

        let result = db.get_meta(&task.id, "nonexistent").await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_db_path() {
        let db = test_db().await;
        assert!(db.path().exists());
    }

    #[tokio::test]
    async fn test_create_task_with_start_time() {
        let db = test_db().await;
        let task = db
            .create_task(&CreateParams {
                name: "scheduled".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: Some("2025-08-20T10:00:00Z".to_string()),
                status: TaskStatus::Pending,
            })
            .await
            .unwrap();
        assert!(task.start_time.contains("2025-08-20"));
    }

    #[tokio::test]
    async fn test_create_task_with_invalid_start_time() {
        let db = test_db().await;
        let result = db
            .create_task(&CreateParams {
                name: "bad-time".into(),
                description: String::new(),
                assignee: String::new(),
                start_time: Some("not-a-valid-time".to_string()),
                status: TaskStatus::Pending,
            })
            .await;
        assert!(result.is_err());
    }
}

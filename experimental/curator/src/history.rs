//! Review history persistence (SQLite).

use crate::review::ReviewActionSummary;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// A single review execution record (append-only audit log).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewRecord {
    /// Stable identifier for one review execution. Records written by older
    /// versions deserialize with `None`; SQLite append assigns an id when this
    /// field is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_id: Option<String>,
    pub session_id: String,
    pub reviewed_at: DateTime<Utc>,
    pub trigger: String,
    pub model: String,
    pub memory_update_count: usize,
    pub skill_update_count: usize,
    pub skipped: bool,
    pub skip_reason: Option<String>,
    pub duration_ms: u64,
    /// Ordered mutation summaries. Legacy JSON and SQLite rows default to an
    /// empty list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<ReviewActionSummary>,
}

/// Aggregated review status for a session, derived from the latest record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewStatus {
    Reviewed,
    Skipped,
    Pending,
}

impl ReviewStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ReviewStatus::Reviewed => "reviewed",
            ReviewStatus::Skipped => "skipped",
            ReviewStatus::Pending => "pending",
        }
    }

    pub fn from_skipped(skipped: bool) -> Self {
        if skipped {
            ReviewStatus::Skipped
        } else {
            ReviewStatus::Reviewed
        }
    }
}

/// Persisted status for one review execution. This is separate from the
/// legacy aggregate [`ReviewStatus`] so adding `failed` does not break
/// downstream exhaustive matches over that public enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewRunStatus {
    Reviewed,
    Skipped,
    Failed,
}

impl ReviewRunStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reviewed => "reviewed",
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }

    fn from_skipped(skipped: bool) -> Self {
        if skipped {
            Self::Skipped
        } else {
            Self::Reviewed
        }
    }
}

impl ReviewRecord {
    /// Compatibility status inference for callers that still express review
    /// failures as `skipped=true, skip_reason="llm_error: ..."`.
    pub fn status(&self) -> ReviewRunStatus {
        if self
            .skip_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("llm_error:"))
        {
            ReviewRunStatus::Failed
        } else {
            ReviewRunStatus::from_skipped(self.skipped)
        }
    }
}

/// One page from the append-only review audit log. The cursor is the next
/// exclusive SQLite history id and can be passed back unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewHistoryPage {
    pub items: Vec<ReviewStatusRecord>,
    pub next_cursor: Option<i64>,
}

/// Status projection used by ACP status/history adapters without requiring a
/// second query or re-inferring failures from legacy fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewStatusRecord {
    pub status: ReviewRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_reason: Option<String>,
    #[serde(flatten)]
    pub record: ReviewRecord,
}

/// Paginated actions for a single review. `legacy` identifies rows written
/// before actions_json existed; those rows return an empty page, not an error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewDetailsPage {
    pub review_id: String,
    pub actions: Vec<ReviewActionSummary>,
    pub next_cursor: Option<usize>,
    pub legacy: bool,
}

/// SQLite-backed review history stored in `memory.db`.
///
/// Two tables:
/// - `review_history` — append-only audit log (all records)
/// - `review_status`  — per-session upsert (latest status only)
pub struct ReviewHistory {
    db_path: PathBuf,
}

impl ReviewHistory {
    /// Creates a handle backed by `<anureo_home>/memory.db`.
    pub fn new(anureo_home: &Path) -> Self {
        let db_path = anureo_home.join("memory.db");
        Self { db_path }
    }

    /// Creates a handle backed by an explicit db path (for tests).
    pub fn with_db_path(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open(&self) -> Result<Connection, String> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create db dir: {}", e))?;
        }
        let conn = checkpoint_sqlite_store::sqlite_util::open_sqlite_with_wal(&self.db_path)?;
        self.init_schema(&conn)?;
        self.migrate_from_jsonl_if_needed(&conn)?;
        Ok(conn)
    }

    /// One-time migration: if the legacy `history.jsonl` exists and
    /// `history.jsonl.bak` does NOT (migration not yet done), import all
    /// records then rename the file to `.bak`.
    fn migrate_from_jsonl_if_needed(&self, conn: &Connection) -> Result<(), String> {
        let Some(anureo_home) = self.db_path.parent() else {
            return Ok(());
        };
        let jsonl_path = anureo_home
            .join("data")
            .join("review")
            .join("history.jsonl");
        let bak_path = anureo_home
            .join("data")
            .join("review")
            .join("history.jsonl.bak");

        // Already migrated (.bak exists) or nothing to migrate (.jsonl absent)
        if bak_path.exists() || !jsonl_path.exists() {
            return Ok(());
        }

        let content = std::fs::read_to_string(&jsonl_path)
            .map_err(|e| format!("Migration: failed to read {}: {}", jsonl_path.display(), e))?;

        let mut imported = 0usize;
        for (lineno, line) in content.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let record: ReviewRecord = serde_json::from_str(line).map_err(|e| {
                format!(
                    "Migration: failed to parse {} line {}: {}",
                    jsonl_path.display(),
                    lineno + 1,
                    e
                )
            })?;
            self.append_record_in_tx(conn, &record)?;
            imported += 1;
        }

        let bak = jsonl_path.with_extension("jsonl.bak");
        let _ = std::fs::rename(&jsonl_path, &bak);
        eprintln!(
            "Review history: migrated {} records from JSONL to SQLite",
            imported
        );
        Ok(())
    }

    /// Inserts a record into both tables using an existing connection (no new open).
    fn append_record_in_tx(&self, conn: &Connection, record: &ReviewRecord) -> Result<(), String> {
        let review_id = record
            .review_id
            .clone()
            .unwrap_or_else(|| format!("review-{}", crate::review::uuid_v4()));
        let actions_json = serde_json::to_string(&record.actions)
            .map_err(|e| format!("Serialize review actions: {}", e))?;
        let status = record.status();
        let fail_reason = inferred_fail_reason(record, status);
        conn.execute(
            "INSERT INTO review_history
                (review_id, session_id, reviewed_at, trigger, model,
                 memory_update_count, skill_update_count,
                 skipped, skip_reason, duration_ms, actions_json, status, fail_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                review_id,
                record.session_id,
                record.reviewed_at.timestamp_millis(),
                record.trigger,
                record.model,
                record.memory_update_count as i64,
                record.skill_update_count as i64,
                record.skipped as i64,
                record.skip_reason,
                record.duration_ms as i64,
                actions_json,
                status.as_str(),
                fail_reason,
            ],
        )
        .map_err(|e| format!("Migration insert review_history: {}", e))?;

        let history_id = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO review_status
                (session_id, status, review_id, reviewed_at, trigger, model,
                 memory_update_count, skill_update_count,
                 skip_reason, duration_ms, actions_json, fail_reason, history_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(session_id) DO UPDATE SET
                status = excluded.status,
                review_id = excluded.review_id,
                reviewed_at = excluded.reviewed_at,
                trigger = excluded.trigger,
                model = excluded.model,
                memory_update_count = excluded.memory_update_count,
                skill_update_count = excluded.skill_update_count,
                skip_reason = excluded.skip_reason,
                duration_ms = excluded.duration_ms,
                actions_json = excluded.actions_json,
                fail_reason = excluded.fail_reason,
                history_id = excluded.history_id",
            params![
                record.session_id,
                status.as_str(),
                review_id,
                record.reviewed_at.timestamp_millis(),
                record.trigger,
                record.model,
                record.memory_update_count as i64,
                record.skill_update_count as i64,
                record.skip_reason,
                record.duration_ms as i64,
                actions_json,
                fail_reason,
                history_id,
            ],
        )
        .map_err(|e| format!("Migration upsert review_status: {}", e))?;
        Ok(())
    }

    fn init_schema(&self, conn: &Connection) -> Result<(), String> {
        Self::ensure_schema(conn)
    }

    /// Ensures the review tables exist in the given connection (idempotent).
    ///
    /// Callers that open `memory.db` directly — e.g. to LEFT JOIN against
    /// `review_status` from the ACP session list — should invoke this before
    /// querying so a database with no review ever recorded does not yield
    /// "no such table" errors.
    pub fn ensure_schema(conn: &Connection) -> Result<(), String> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS review_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                review_id TEXT,
                session_id TEXT NOT NULL,
                reviewed_at INTEGER NOT NULL,
                trigger TEXT NOT NULL,
                model TEXT NOT NULL,
                memory_update_count INTEGER NOT NULL,
                skill_update_count INTEGER NOT NULL,
                skipped INTEGER NOT NULL,
                skip_reason TEXT,
                duration_ms INTEGER NOT NULL,
                actions_json TEXT,
                status TEXT,
                fail_reason TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_review_history_session
                ON review_history(session_id);
            CREATE INDEX IF NOT EXISTS idx_review_history_id_desc
                ON review_history(id DESC);

            CREATE TABLE IF NOT EXISTS review_status (
                session_id TEXT PRIMARY KEY,
                status TEXT NOT NULL,
                review_id TEXT,
                reviewed_at INTEGER NOT NULL,
                trigger TEXT NOT NULL,
                model TEXT NOT NULL,
                memory_update_count INTEGER NOT NULL,
                skill_update_count INTEGER NOT NULL,
                skip_reason TEXT,
                duration_ms INTEGER NOT NULL,
                actions_json TEXT,
                fail_reason TEXT,
                history_id INTEGER NOT NULL REFERENCES review_history(id)
            );",
        )
        .map_err(|e| format!("Failed to init review schema: {}", e))?;

        // Additive migration for databases created before background-review
        // protocol v2. SQLite lacks portable `ADD COLUMN IF NOT EXISTS`, so
        // inspect table_info before each ALTER.
        ensure_column(conn, "review_history", "review_id", "TEXT")?;
        ensure_column(conn, "review_history", "actions_json", "TEXT")?;
        ensure_column(conn, "review_history", "status", "TEXT")?;
        ensure_column(conn, "review_history", "fail_reason", "TEXT")?;
        ensure_column(conn, "review_status", "review_id", "TEXT")?;
        ensure_column(conn, "review_status", "actions_json", "TEXT")?;
        ensure_column(conn, "review_status", "fail_reason", "TEXT")?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_review_history_review_id
                 ON review_history(review_id);
             CREATE INDEX IF NOT EXISTS idx_review_history_session_id_desc
                 ON review_history(session_id, id DESC);",
        )
        .map_err(|e| format!("Failed to init review indexes: {}", e))?;
        Ok(())
    }

    /// Appends a record in a single transaction, updating both tables.
    pub fn append(&self, record: &ReviewRecord) -> Result<(), String> {
        self.append_with_status(record, record.status(), None)
    }

    /// Appends an explicitly failed review while retaining the legacy
    /// `ReviewRecord` construction shape used by existing CLI/ACP callers.
    pub fn append_failed(
        &self,
        record: &ReviewRecord,
        fail_reason: impl Into<String>,
    ) -> Result<(), String> {
        self.append_with_status(record, ReviewRunStatus::Failed, Some(fail_reason.into()))
    }

    fn append_with_status(
        &self,
        record: &ReviewRecord,
        status: ReviewRunStatus,
        fail_reason: Option<String>,
    ) -> Result<(), String> {
        let mut conn = self.open()?;
        let tx = conn.transaction().map_err(|e| format!("Begin tx: {}", e))?;
        let review_id = record
            .review_id
            .clone()
            .unwrap_or_else(|| format!("review-{}", crate::review::uuid_v4()));
        let actions_json = serde_json::to_string(&record.actions)
            .map_err(|e| format!("Serialize review actions: {}", e))?;
        let fail_reason = fail_reason.or_else(|| inferred_fail_reason(record, status));

        tx.execute(
            "INSERT INTO review_history
                (review_id, session_id, reviewed_at, trigger, model,
                 memory_update_count, skill_update_count,
                 skipped, skip_reason, duration_ms, actions_json, status, fail_reason)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                review_id,
                record.session_id,
                record.reviewed_at.timestamp_millis(),
                record.trigger,
                record.model,
                record.memory_update_count as i64,
                record.skill_update_count as i64,
                record.skipped as i64,
                record.skip_reason,
                record.duration_ms as i64,
                actions_json,
                status.as_str(),
                fail_reason,
            ],
        )
        .map_err(|e| format!("Insert review_history: {}", e))?;

        let history_id = tx.last_insert_rowid();

        tx.execute(
            "INSERT INTO review_status
                (session_id, status, review_id, reviewed_at, trigger, model,
                 memory_update_count, skill_update_count,
                 skip_reason, duration_ms, actions_json, fail_reason, history_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(session_id) DO UPDATE SET
                status = excluded.status,
                review_id = excluded.review_id,
                reviewed_at = excluded.reviewed_at,
                trigger = excluded.trigger,
                model = excluded.model,
                memory_update_count = excluded.memory_update_count,
                skill_update_count = excluded.skill_update_count,
                skip_reason = excluded.skip_reason,
                duration_ms = excluded.duration_ms,
                actions_json = excluded.actions_json,
                fail_reason = excluded.fail_reason,
                history_id = excluded.history_id",
            params![
                record.session_id,
                status.as_str(),
                review_id,
                record.reviewed_at.timestamp_millis(),
                record.trigger,
                record.model,
                record.memory_update_count as i64,
                record.skill_update_count as i64,
                record.skip_reason,
                record.duration_ms as i64,
                actions_json,
                fail_reason,
                history_id,
            ],
        )
        .map_err(|e| format!("Upsert review_status: {}", e))?;

        tx.commit().map_err(|e| format!("Commit tx: {}", e))?;
        Ok(())
    }

    /// Returns the most recent `limit` records from the audit log (newest first).
    pub fn list(&self, limit: usize) -> Result<Vec<ReviewRecord>, String> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, reviewed_at, trigger, model,
                        memory_update_count, skill_update_count,
                        skipped, skip_reason, duration_ms,
                        review_id, actions_json
                 FROM review_history
                 ORDER BY id DESC
                 LIMIT ?1",
            )
            .map_err(|e| format!("Prepare list: {}", e))?;

        let rows = stmt
            .query_map(params![limit as i64], row_to_record)
            .map_err(|e| format!("Query list: {}", e))?;

        let mut records = Vec::new();
        for row in rows {
            records.push(row.map_err(|e| format!("Row list: {}", e))?);
        }
        Ok(records)
    }

    /// Returns the latest status record for `session_id` (O(1) primary key lookup).
    pub fn find_by_session(&self, session_id: &str) -> Result<Option<ReviewRecord>, String> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare(
                "SELECT session_id, reviewed_at, trigger, model,
                        memory_update_count, skill_update_count,
                        CASE WHEN status IN ('skipped', 'failed') THEN 1 ELSE 0 END AS skipped,
                        skip_reason, duration_ms,
                        review_id, actions_json
                 FROM review_status
                 WHERE session_id = ?1",
            )
            .map_err(|e| format!("Prepare find: {}", e))?;

        let result = stmt
            .query_row(params![session_id], row_to_record)
            .optional()
            .map_err(|e| format!("Query find: {}", e))?;

        Ok(result)
    }

    /// Returns the latest status plus its complete persisted record. This is
    /// the status-query counterpart to the legacy `find_by_session` helper.
    pub fn find_status_by_session(
        &self,
        session_id: &str,
    ) -> Result<Option<ReviewStatusRecord>, String> {
        let conn = self.open()?;
        conn.query_row(
            "SELECT status, fail_reason,
                    session_id, reviewed_at, trigger, model,
                    memory_update_count, skill_update_count,
                    CASE WHEN status IN ('skipped', 'failed') THEN 1 ELSE 0 END,
                    skip_reason, duration_ms, review_id, actions_json
             FROM review_status
             WHERE session_id = ?1",
            params![session_id],
            |row| {
                Ok(ReviewStatusRecord {
                    status: parse_run_status(&row.get::<_, String>(0)?),
                    fail_reason: row.get(1)?,
                    record: row_to_record_from(row, 2)?,
                })
            },
        )
        .optional()
        .map_err(|e| format!("Query review status: {}", e))
    }

    /// Cursor-paginates review history newest-first. Passing `None` for
    /// `session_id` returns global history; ACP is responsible for owner checks.
    pub fn list_page(
        &self,
        session_id: Option<&str>,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<ReviewHistoryPage, String> {
        let conn = self.open()?;
        let limit = limit.clamp(1, 200);
        let fetch = limit.saturating_add(1) as i64;
        let before_id = cursor.unwrap_or(i64::MAX);
        let columns = "id,
                       COALESCE(status,
                           CASE WHEN skipped != 0 THEN 'skipped' ELSE 'reviewed' END),
                       fail_reason,
                       session_id, reviewed_at, trigger, model,
                       memory_update_count, skill_update_count,
                       skipped, skip_reason, duration_ms, review_id, actions_json";

        let mut rows_with_ids = if let Some(session_id) = session_id {
            let sql = format!(
                "SELECT {} FROM review_history
                 WHERE session_id = ?1 AND id < ?2
                 ORDER BY id DESC LIMIT ?3",
                columns
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| format!("Prepare session history page: {}", e))?;
            let rows = stmt
                .query_map(params![session_id, before_id, fetch], |row| {
                    Ok((row.get::<_, i64>(0)?, row_to_status_record_from(row, 1)?))
                })
                .map_err(|e| format!("Query session history page: {}", e))?;
            collect_rows(rows, "session history page")?
        } else {
            let sql = format!(
                "SELECT {} FROM review_history
                 WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
                columns
            );
            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| format!("Prepare history page: {}", e))?;
            let rows = stmt
                .query_map(params![before_id, fetch], |row| {
                    Ok((row.get::<_, i64>(0)?, row_to_status_record_from(row, 1)?))
                })
                .map_err(|e| format!("Query history page: {}", e))?;
            collect_rows(rows, "history page")?
        };

        let has_more = rows_with_ids.len() > limit;
        rows_with_ids.truncate(limit);
        let next_cursor = has_more
            .then(|| rows_with_ids.last().map(|(id, _)| *id))
            .flatten();
        Ok(ReviewHistoryPage {
            items: rows_with_ids
                .into_iter()
                .map(|(_, status_record)| status_record)
                .collect(),
            next_cursor,
        })
    }

    /// Finds one review by the owner-visible `(session_id, review_id)` pair.
    pub fn find_by_review_id(
        &self,
        session_id: &str,
        review_id: &str,
    ) -> Result<Option<ReviewStatusRecord>, String> {
        let conn = self.open()?;
        conn.query_row(
            "SELECT COALESCE(status,
                        CASE WHEN skipped != 0 THEN 'skipped' ELSE 'reviewed' END),
                    fail_reason,
                    session_id, reviewed_at, trigger, model,
                    memory_update_count, skill_update_count,
                    skipped, skip_reason, duration_ms, review_id, actions_json
             FROM review_history
             WHERE session_id = ?1 AND review_id = ?2
             LIMIT 1",
            params![session_id, review_id],
            |row| {
                Ok(ReviewStatusRecord {
                    status: parse_run_status(&row.get::<_, String>(0)?),
                    fail_reason: row.get(1)?,
                    record: row_to_record_from(row, 2)?,
                })
            },
        )
        .optional()
        .map_err(|e| format!("Query review by id: {}", e))
    }

    /// Returns one page of actions for a review. Old rows with NULL
    /// `actions_json` are reported as `legacy=true` and an empty action list.
    pub fn details(
        &self,
        session_id: &str,
        review_id: &str,
        cursor: Option<usize>,
        limit: usize,
    ) -> Result<Option<ReviewDetailsPage>, String> {
        let conn = self.open()?;
        let actions_json: Option<Option<String>> = conn
            .query_row(
                "SELECT actions_json FROM review_history
                 WHERE session_id = ?1 AND review_id = ?2 LIMIT 1",
                params![session_id, review_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("Query review details: {}", e))?;
        let Some(actions_json) = actions_json else {
            return Ok(None);
        };

        let legacy = actions_json.is_none();
        let actions: Vec<ReviewActionSummary> = match actions_json {
            Some(json) => {
                serde_json::from_str(&json).map_err(|e| format!("Decode review actions: {}", e))?
            }
            None => Vec::new(),
        };
        let start = cursor.unwrap_or(0).min(actions.len());
        let limit = limit.clamp(1, 200);
        let end = start.saturating_add(limit).min(actions.len());
        let next_cursor = (end < actions.len()).then_some(end);

        Ok(Some(ReviewDetailsPage {
            review_id: review_id.to_string(),
            actions: actions[start..end].to_vec(),
            next_cursor,
            legacy,
        }))
    }

    /// Returns all session_ids that have been processed (reviewed or skipped).
    /// Sessions absent from this set are truly "pending" (never reviewed).
    pub fn reviewed_session_ids(&self) -> Result<HashSet<String>, String> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare("SELECT session_id FROM review_status")
            .map_err(|e| format!("Prepare reviewed_ids: {}", e))?;

        let rows = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|e| format!("Query reviewed_ids: {}", e))?;

        let mut ids = HashSet::new();
        for row in rows {
            ids.insert(row.map_err(|e| format!("Row reviewed_ids: {}", e))?);
        }
        Ok(ids)
    }

    /// Returns a `session_id → ReviewStatus` map for all reviewed/skipped sessions.
    /// Sessions not in the map are implicitly `Pending`.
    pub fn review_status_map(&self) -> Result<HashMap<String, ReviewStatus>, String> {
        let conn = self.open()?;
        let mut stmt = conn
            .prepare("SELECT session_id, status FROM review_status")
            .map_err(|e| format!("Prepare status_map: {}", e))?;

        let rows = stmt
            .query_map([], |row| {
                let session_id: String = row.get(0)?;
                let status: String = row.get(1)?;
                Ok((session_id, status))
            })
            .map_err(|e| format!("Query status_map: {}", e))?;

        let mut map = HashMap::new();
        for row in rows {
            let (session_id, status) = row.map_err(|e| format!("Row status_map: {}", e))?;
            let st = match status.as_str() {
                "reviewed" => ReviewStatus::Reviewed,
                "skipped" => ReviewStatus::Skipped,
                // Preserve the legacy aggregate API: a failed terminal review
                // is processed but unsuccessful, so old callers see skipped.
                "failed" => ReviewStatus::Skipped,
                _ => ReviewStatus::Reviewed,
            };
            map.insert(session_id, st);
        }
        Ok(map)
    }
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReviewRecord> {
    row_to_record_from(row, 0)
}

fn row_to_status_record_from(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<ReviewStatusRecord> {
    Ok(ReviewStatusRecord {
        status: parse_run_status(&row.get::<_, String>(offset)?),
        fail_reason: row.get(offset + 1)?,
        record: row_to_record_from(row, offset + 2)?,
    })
}

fn row_to_record_from(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<ReviewRecord> {
    let reviewed_at_ms: i64 = row.get(offset + 1)?;
    let reviewed_at = DateTime::from_timestamp_millis(reviewed_at_ms)
        .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());
    let skipped_int: i64 = row.get(offset + 6)?;
    let actions_json: Option<String> = row.get(offset + 10)?;
    let actions = actions_json
        .as_deref()
        .and_then(|json| serde_json::from_str(json).ok())
        .unwrap_or_default();
    Ok(ReviewRecord {
        review_id: row.get(offset + 9)?,
        session_id: row.get(offset)?,
        reviewed_at,
        trigger: row.get(offset + 2)?,
        model: row.get(offset + 3)?,
        memory_update_count: row.get::<_, i64>(offset + 4)? as usize,
        skill_update_count: row.get::<_, i64>(offset + 5)? as usize,
        skipped: skipped_int != 0,
        skip_reason: row.get(offset + 7)?,
        duration_ms: row.get::<_, i64>(offset + 8)? as u64,
        actions,
    })
}

fn parse_run_status(status: &str) -> ReviewRunStatus {
    match status {
        "skipped" => ReviewRunStatus::Skipped,
        "failed" => ReviewRunStatus::Failed,
        _ => ReviewRunStatus::Reviewed,
    }
}

fn inferred_fail_reason(record: &ReviewRecord, status: ReviewRunStatus) -> Option<String> {
    (status == ReviewRunStatus::Failed)
        .then(|| record.skip_reason.clone())
        .flatten()
}

fn ensure_column(
    conn: &Connection,
    table: &str,
    column: &str,
    declaration: &str,
) -> Result<(), String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({})", table))
        .map_err(|e| format!("Inspect {} schema: {}", table, e))?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| format!("Read {} schema: {}", table, e))?;
    for existing in columns {
        if existing.map_err(|e| format!("Read {} column: {}", table, e))? == column {
            return Ok(());
        }
    }
    conn.execute(
        &format!(
            "ALTER TABLE {} ADD COLUMN {} {}",
            table, column, declaration
        ),
        [],
    )
    .map_err(|e| format!("Add {}.{}: {}", table, column, e))?;
    Ok(())
}

fn collect_rows<T, F>(rows: rusqlite::MappedRows<'_, F>, context: &str) -> Result<Vec<T>, String>
where
    F: FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mut values = Vec::new();
    for row in rows {
        values.push(row.map_err(|e| format!("Row {}: {}", context, e))?);
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, ReviewHistory) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("memory.db");
        let history = ReviewHistory::with_db_path(db_path);
        (dir, history)
    }

    fn make_record(session_id: &str, skipped: bool) -> ReviewRecord {
        ReviewRecord {
            review_id: None,
            session_id: session_id.to_string(),
            reviewed_at: Utc::now(),
            trigger: "manual".to_string(),
            model: "gpt-4o-mini".to_string(),
            memory_update_count: 1,
            skill_update_count: 0,
            skipped,
            skip_reason: if skipped {
                Some("too_short".to_string())
            } else {
                None
            },
            duration_ms: 100,
            actions: Vec::new(),
        }
    }

    fn make_action(index: usize) -> ReviewActionSummary {
        ReviewActionSummary {
            kind: "memory".to_string(),
            op: "add".to_string(),
            target: "user".to_string(),
            file_path: Some("USER.md".to_string()),
            summary: format!("action {}", index),
            succeeded: true,
            chars_before: None,
            chars_after: None,
            diff_ref: None,
            persisted_diff: None,
            diff_truncated: false,
        }
    }

    #[test]
    fn test_append_and_list() {
        let (_dir, history) = setup();
        history.append(&make_record("s1", false)).unwrap();
        history.append(&make_record("s2", false)).unwrap();
        let records = history.list(10).unwrap();
        assert_eq!(records.len(), 2);
        // newest first
        assert_eq!(records[0].session_id, "s2");
        assert_eq!(records[1].session_id, "s1");
    }

    #[test]
    fn test_list_with_limit() {
        let (_dir, history) = setup();
        for i in 0..5 {
            history
                .append(&make_record(&format!("s{}", i), false))
                .unwrap();
        }
        let records = history.list(3).unwrap();
        assert_eq!(records.len(), 3);
        // newest first: s4, s3, s2
        assert_eq!(records[0].session_id, "s4");
    }

    #[test]
    fn test_reviewed_session_ids_includes_skipped() {
        let (_dir, history) = setup();
        history.append(&make_record("s1", false)).unwrap();
        history.append(&make_record("s2", true)).unwrap();
        history.append(&make_record("s3", false)).unwrap();
        let ids = history.reviewed_session_ids().unwrap();
        // All processed sessions should be present (reviewed + skipped),
        // so that show_pending only shows truly unprocessed sessions.
        assert!(ids.contains("s1"));
        assert!(ids.contains("s2"));
        assert!(ids.contains("s3"));
    }

    #[test]
    fn test_find_by_session() {
        let (_dir, history) = setup();
        history.append(&make_record("s1", false)).unwrap();
        assert!(history.find_by_session("s1").unwrap().is_some());
        assert!(history.find_by_session("s99").unwrap().is_none());
    }

    #[test]
    fn test_review_status_map() {
        let (_dir, history) = setup();
        history.append(&make_record("s1", false)).unwrap();
        history.append(&make_record("s2", true)).unwrap();

        let map = history.review_status_map().unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("s1"), Some(&ReviewStatus::Reviewed));
        assert_eq!(map.get("s2"), Some(&ReviewStatus::Skipped));
    }

    #[test]
    fn test_upsert_keeps_latest() {
        let (_dir, history) = setup();
        // First review: success
        history.append(&make_record("s1", false)).unwrap();
        // Second review: skipped (session became too short on re-review)
        history.append(&make_record("s1", true)).unwrap();

        // review_status should reflect latest (skipped)
        let map = history.review_status_map().unwrap();
        assert_eq!(map.get("s1"), Some(&ReviewStatus::Skipped));

        // reviewed_session_ids returns all processed sessions, so s1 is present
        let ids = history.reviewed_session_ids().unwrap();
        assert!(ids.contains("s1"));

        // find_by_session should return the skipped record
        let rec = history.find_by_session("s1").unwrap().unwrap();
        assert!(rec.skipped);

        // audit log keeps both
        let all = history.list(usize::MAX).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_empty_db_returns_empty() {
        let (_dir, history) = setup();
        assert!(history.list(10).unwrap().is_empty());
        assert!(history.review_status_map().unwrap().is_empty());
        assert!(history.reviewed_session_ids().unwrap().is_empty());
        assert!(history.find_by_session("any").unwrap().is_none());
    }

    #[test]
    fn test_migrate_from_jsonl() {
        let (dir, history) = setup();
        let anureo_home = dir.path();
        let review_dir = anureo_home.join("data").join("review");
        std::fs::create_dir_all(&review_dir).unwrap();
        let jsonl_path = review_dir.join("history.jsonl");

        let r1 = make_record("s1", false);
        let r2 = make_record("s2", true);
        let r3 = make_record("s3", false);
        let jsonl_content = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&r1).unwrap(),
            serde_json::to_string(&r2).unwrap(),
            serde_json::to_string(&r3).unwrap(),
        );
        std::fs::write(&jsonl_path, &jsonl_content).unwrap();

        // Opening triggers migration (table is empty + jsonl exists)
        let map = history.review_status_map().unwrap();
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("s1"), Some(&ReviewStatus::Reviewed));
        assert_eq!(map.get("s2"), Some(&ReviewStatus::Skipped));
        assert_eq!(map.get("s3"), Some(&ReviewStatus::Reviewed));

        // JSONL renamed to .bak
        assert!(!jsonl_path.exists());
        assert!(jsonl_path.with_extension("jsonl.bak").exists());
    }

    #[test]
    fn test_migration_not_repeated() {
        let (dir, history) = setup();
        let anureo_home = dir.path();
        let review_dir = anureo_home.join("data").join("review");
        std::fs::create_dir_all(&review_dir).unwrap();
        let jsonl_path = review_dir.join("history.jsonl");

        let r1 = make_record("s1", false);
        std::fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&r1).unwrap()),
        )
        .unwrap();

        // First open migrates
        history.list(10).unwrap();
        assert_eq!(history.review_status_map().unwrap().len(), 1);

        // Recreate a stale jsonl to simulate "leftover" — migration should NOT re-run
        // because .bak already exists (migration marker).
        std::fs::write(
            &jsonl_path,
            format!(
                "{}\n",
                serde_json::to_string(&make_record("s2", false)).unwrap()
            ),
        )
        .unwrap();
        history.list(10).unwrap();
        // Still only s1, not s2
        assert_eq!(history.review_status_map().unwrap().len(), 1);
    }

    #[test]
    fn review_status_pending_as_str() {
        assert_eq!(ReviewStatus::Pending.as_str(), "pending");
    }

    #[test]
    fn review_status_from_skipped_variants() {
        assert_eq!(ReviewStatus::from_skipped(true), ReviewStatus::Skipped);
        assert_eq!(ReviewStatus::from_skipped(false), ReviewStatus::Reviewed);
        assert_eq!(ReviewRunStatus::Failed.as_str(), "failed");
    }

    #[test]
    fn legacy_json_record_defaults_v2_fields() {
        let json = serde_json::json!({
            "session_id": "legacy-json",
            "reviewed_at": Utc::now(),
            "trigger": "background",
            "model": "legacy-model",
            "memory_update_count": 0,
            "skill_update_count": 0,
            "skipped": false,
            "skip_reason": null,
            "duration_ms": 12
        });
        let record: ReviewRecord = serde_json::from_value(json).unwrap();
        assert_eq!(record.review_id, None);
        assert!(record.actions.is_empty());
    }

    #[test]
    fn append_round_trips_review_id_and_actions() {
        let (_dir, history) = setup();
        let mut record = make_record("v2", false);
        record.review_id = Some("review-v2".to_string());
        record.actions = vec![make_action(1), make_action(2)];
        history.append(&record).unwrap();

        let loaded = history.find_by_session("v2").unwrap().unwrap();
        assert_eq!(loaded.review_id.as_deref(), Some("review-v2"));
        assert_eq!(loaded.actions, record.actions);

        let status = history.find_status_by_session("v2").unwrap().unwrap();
        assert_eq!(status.status, ReviewRunStatus::Reviewed);
        assert_eq!(status.record.actions.len(), 2);
        let by_id = history
            .find_by_review_id("v2", "review-v2")
            .unwrap()
            .unwrap();
        assert_eq!(by_id.record.review_id.as_deref(), Some("review-v2"));
    }

    #[test]
    fn append_failed_persists_failed_status_and_reason() {
        let (_dir, history) = setup();
        let mut record = make_record("failed", false);
        record.review_id = Some("review-failed".to_string());
        history.append_failed(&record, "rate_limited").unwrap();

        let status = history.find_status_by_session("failed").unwrap().unwrap();
        assert_eq!(status.status, ReviewRunStatus::Failed);
        assert_eq!(status.fail_reason.as_deref(), Some("rate_limited"));
        assert_eq!(
            history.review_status_map().unwrap().get("failed"),
            Some(&ReviewStatus::Skipped)
        );
        let page = history.list_page(Some("failed"), None, 10).unwrap();
        assert_eq!(page.items[0].status, ReviewRunStatus::Failed);
        assert_eq!(page.items[0].fail_reason.as_deref(), Some("rate_limited"));
    }

    #[test]
    fn history_and_details_are_cursor_paginated() {
        let (_dir, history) = setup();
        for i in 0..3 {
            let mut record = make_record("paged", false);
            record.review_id = Some(format!("review-{}", i));
            record.actions = vec![make_action(0), make_action(1), make_action(2)];
            history.append(&record).unwrap();
        }

        let first = history.list_page(Some("paged"), None, 2).unwrap();
        assert_eq!(first.items.len(), 2);
        assert!(first.next_cursor.is_some());
        let second = history
            .list_page(Some("paged"), first.next_cursor, 2)
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert!(second.next_cursor.is_none());

        let details = history
            .details("paged", "review-0", None, 2)
            .unwrap()
            .unwrap();
        assert!(!details.legacy);
        assert_eq!(details.actions.len(), 2);
        assert_eq!(details.next_cursor, Some(2));
        let tail = history
            .details("paged", "review-0", details.next_cursor, 2)
            .unwrap()
            .unwrap();
        assert_eq!(tail.actions.len(), 1);
        assert_eq!(tail.next_cursor, None);
    }

    #[test]
    fn additive_migration_reads_legacy_sqlite_rows() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("memory.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE review_history (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    session_id TEXT NOT NULL,
                    reviewed_at INTEGER NOT NULL,
                    trigger TEXT NOT NULL,
                    model TEXT NOT NULL,
                    memory_update_count INTEGER NOT NULL,
                    skill_update_count INTEGER NOT NULL,
                    skipped INTEGER NOT NULL,
                    skip_reason TEXT,
                    duration_ms INTEGER NOT NULL
                );
                CREATE INDEX idx_review_history_session ON review_history(session_id);
                CREATE INDEX idx_review_history_id_desc ON review_history(id DESC);
                CREATE TABLE review_status (
                    session_id TEXT PRIMARY KEY,
                    status TEXT NOT NULL,
                    reviewed_at INTEGER NOT NULL,
                    trigger TEXT NOT NULL,
                    model TEXT NOT NULL,
                    memory_update_count INTEGER NOT NULL,
                    skill_update_count INTEGER NOT NULL,
                    skip_reason TEXT,
                    duration_ms INTEGER NOT NULL,
                    history_id INTEGER NOT NULL REFERENCES review_history(id)
                );
                INSERT INTO review_history
                    (session_id, reviewed_at, trigger, model,
                     memory_update_count, skill_update_count,
                     skipped, skip_reason, duration_ms)
                VALUES ('legacy', 1, 'background', 'old', 0, 0, 0, NULL, 5);
                INSERT INTO review_status
                    (session_id, status, reviewed_at, trigger, model,
                     memory_update_count, skill_update_count,
                     skip_reason, duration_ms, history_id)
                VALUES ('legacy', 'reviewed', 1, 'background', 'old', 0, 0, NULL, 5, 1);",
            )
            .unwrap();
        }

        let history = ReviewHistory::with_db_path(db_path);
        let record = history.find_by_session("legacy").unwrap().unwrap();
        assert_eq!(record.review_id, None);
        assert!(record.actions.is_empty());

        // A migrated row that is assigned an id by a later backfill remains
        // queryable even though its actions_json is NULL.
        let conn = Connection::open(&history.db_path).unwrap();
        conn.execute(
            "UPDATE review_history SET review_id = 'legacy-review' WHERE session_id = 'legacy'",
            [],
        )
        .unwrap();
        drop(conn);
        let legacy_details = history
            .details("legacy", "legacy-review", None, 50)
            .unwrap()
            .unwrap();
        assert!(legacy_details.legacy);
        assert!(legacy_details.actions.is_empty());

        // Legacy rows have no review id, but the schema is migrated and new
        // records can immediately use all v2 fields.
        let mut next = make_record("new", false);
        next.review_id = Some("review-new".to_string());
        next.actions = vec![make_action(1)];
        history.append(&next).unwrap();
        assert_eq!(history.list(10).unwrap().len(), 2);
    }

    #[test]
    fn review_history_new_joins_memory_db() {
        let dir = TempDir::new().unwrap();
        let history = ReviewHistory::new(dir.path());
        history.append(&make_record("new-1", false)).unwrap();
        assert_eq!(history.list(10).unwrap().len(), 1);
    }

    #[test]
    fn open_creates_nested_parent_dirs() {
        let dir = TempDir::new().unwrap();
        let nested = dir.path().join("a/b/c/memory.db");
        let history = ReviewHistory::with_db_path(nested.clone());
        history.append(&make_record("nested-1", false)).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn migrate_from_jsonl_skips_blank_lines() {
        let dir = TempDir::new().unwrap();
        let anureo_home = dir.path();
        let review_dir = anureo_home.join("data").join("review");
        std::fs::create_dir_all(&review_dir).unwrap();
        let jsonl_path = review_dir.join("history.jsonl");

        let r1 = make_record("blank-1", false);
        let valid = serde_json::to_string(&r1).unwrap();
        // Mix of empty lines (L102) and valid records
        std::fs::write(&jsonl_path, format!("\n{}\n\n", valid)).unwrap();

        let history = ReviewHistory::with_db_path(anureo_home.join("memory.db"));
        let records = history.list(10).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "blank-1");
    }

    #[test]
    fn migrate_from_jsonl_invalid_json_returns_err() {
        let dir = TempDir::new().unwrap();
        let anureo_home = dir.path();
        let review_dir = anureo_home.join("data").join("review");
        std::fs::create_dir_all(&review_dir).unwrap();
        let jsonl_path = review_dir.join("history.jsonl");

        std::fs::write(&jsonl_path, "{\"session_id\":\"x\"}\nNOT_JSON\n").unwrap();

        let history = ReviewHistory::with_db_path(anureo_home.join("memory.db"));
        let result = history.list(10);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("Migration: failed to parse"), "got: {}", err);
    }

    #[test]
    fn review_status_map_fallback_for_unknown_status() {
        let (_dir, history) = setup();
        history.append(&make_record("known", false)).unwrap();

        // Directly corrupt the status to an unrecognized value to exercise the
        // `_ => ReviewStatus::Reviewed` fallback arm.
        let conn = rusqlite::Connection::open(&history.db_path).unwrap();
        conn.execute(
            "UPDATE review_status SET status = 'unknown' WHERE session_id = 'known'",
            [],
        )
        .unwrap();

        let map = history.review_status_map().unwrap();
        assert_eq!(map.get("known"), Some(&ReviewStatus::Reviewed));
    }
}

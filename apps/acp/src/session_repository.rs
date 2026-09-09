//! Durable ACP session metadata stored beside anureo checkpoints.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

fn utc_now_microseconds() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn next_monotonic_timestamp(previous: Option<&str>, candidate: &str) -> String {
    let Some(previous) = previous else {
        return candidate.to_string();
    };
    let Ok(previous) = chrono::DateTime::parse_from_rfc3339(previous) else {
        return candidate.to_string();
    };
    let Ok(candidate_time) = chrono::DateTime::parse_from_rfc3339(candidate) else {
        return candidate.to_string();
    };
    let next = if candidate_time <= previous {
        previous + chrono::Duration::microseconds(1)
    } else {
        candidate_time
    };
    next.with_timezone(&chrono::Utc)
        .to_rfc3339_opts(chrono::SecondsFormat::Micros, true)
}

fn is_sqlite_busy(error: &rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked,
                ..
            },
            _,
        )
    ) || {
        let message = error.to_string().to_ascii_lowercase();
        message.contains("database is locked") || message.contains("database is busy")
    }
}

fn retry_sqlite_busy<T, F>(mut operation: F) -> rusqlite::Result<T>
where
    F: FnMut() -> rusqlite::Result<T>,
{
    for attempt in 0..8 {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_sqlite_busy(&error) && attempt < 7 => {
                thread::sleep(Duration::from_millis(50_u64 << attempt));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("SQLite retry loop must return");
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMetadata {
    pub session_id: String,
    pub thread_id: String,
    pub owner_principal: String,
    pub cwd: PathBuf,
    pub lifecycle: String,
    /// Auto-generated session title (first-turn LLM title), when known.
    pub title: Option<String>,
    /// RFC 3339 timestamp of the last metadata update.
    pub updated_at: Option<String>,
    /// RFC 3339 timestamp of creation.
    pub created_at: Option<String>,
    /// RFC 3339 timestamp of archival; `None` while active.
    pub archived_at: Option<String>,
}

/// Canonical session-index projection used by the anureo Desk extension.
/// `SessionMetadata` remains the compatibility projection for existing ACP
/// callers; new list/event code must use this complete shape.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SessionIndexRecord {
    pub session_id: String,
    pub parent_session_id: Option<String>,
    pub owner_principal: String,
    pub cwd: PathBuf,
    pub lifecycle: String,
    pub title: Option<String>,
    pub metadata: serde_json::Value,
    pub created_at: String,
    pub activity_at: String,
    pub tree_activity_at: String,
    pub state_changed_at: Option<String>,
    pub metadata_updated_at: Option<String>,
    pub archived_at: Option<String>,
    pub closed_at: Option<String>,
    pub revision: i64,
    pub index_version: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SessionTombstone {
    pub session_id: String,
    pub owner_principal: String,
    pub cwd: PathBuf,
    pub parent_session_id: Option<String>,
    pub revision: i64,
    pub index_version: i64,
    pub deleted_at: String,
}

/// Canonical result of deleting a session index record. The tombstone and
/// ancestor projections are produced by one SQLite transaction so response
/// and global events cannot observe different tree versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDeleteResult {
    pub tombstone: SessionTombstone,
    pub affected_ancestors: Vec<SessionIndexRecord>,
}

fn order_active_tree(records: Vec<SessionIndexRecord>) -> Vec<SessionIndexRecord> {
    let mut by_id: HashMap<String, SessionIndexRecord> = records
        .into_iter()
        .map(|record| (record.session_id.clone(), record))
        .collect();
    let ids: std::collections::HashSet<String> = by_id.keys().cloned().collect();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots = Vec::new();
    for record in by_id.values() {
        if let Some(parent) = record
            .parent_session_id
            .as_deref()
            .filter(|parent| ids.contains(*parent))
        {
            children
                .entry(parent.to_string())
                .or_default()
                .push(record.session_id.clone());
        } else {
            roots.push(record.session_id.clone());
        }
    }
    let rank = |id: &String| {
        let record = by_id.get(id).expect("tree id exists");
        (
            std::cmp::Reverse(record.tree_activity_at.clone()),
            id.clone(),
        )
    };
    roots.sort_by_key(&rank);
    for values in children.values_mut() {
        values.sort_by_key(&rank);
    }
    fn visit(
        id: &str,
        by_id: &mut HashMap<String, SessionIndexRecord>,
        children: &HashMap<String, Vec<String>>,
        output: &mut Vec<SessionIndexRecord>,
    ) {
        if let Some(record) = by_id.remove(id) {
            output.push(record);
            if let Some(child_ids) = children.get(id) {
                for child_id in child_ids {
                    visit(child_id, by_id, children, output);
                }
            }
        }
    }
    let mut output = Vec::with_capacity(by_id.len());
    for root in roots {
        visit(&root, &mut by_id, &children, &mut output);
    }
    // Defensive fallback for corrupted cycles: retain deterministic ordering
    // instead of dropping records that were not reachable from a root.
    let mut leftovers: Vec<_> = by_id.into_values().collect();
    leftovers.sort_by(|a, b| {
        b.tree_activity_at
            .cmp(&a.tree_activity_at)
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    output.extend(leftovers);
    output
}

#[derive(Clone, Debug)]
pub struct SessionRepository {
    db_path: PathBuf,
}

fn allocate_owner_version(
    transaction: &rusqlite::Transaction<'_>,
    owner_principal: &str,
) -> rusqlite::Result<i64> {
    // Keep versions representable as JSON/JavaScript safe integers. SQLite's
    // INTEGER range is wider than the value Desk can compare without loss.
    const MAX_JSON_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
    let current: Option<i64> = transaction
        .query_row(
            "SELECT current_version FROM acp_session_index_state WHERE owner_principal = ?1",
            [owner_principal],
            |row| row.get(0),
        )
        .optional()?;
    let next = current.unwrap_or(0).checked_add(1).ok_or_else(|| {
        rusqlite::Error::InvalidParameterName("owner index version overflow".into())
    })?;
    if next > MAX_JSON_SAFE_INTEGER {
        return Err(rusqlite::Error::InvalidParameterName(
            "owner index version exceeds JSON safe integer range".into(),
        ));
    }
    if current.is_some() {
        transaction.execute(
            "UPDATE acp_session_index_state SET current_version = ?2 WHERE owner_principal = ?1",
            params![owner_principal, next],
        )?;
    } else {
        transaction.execute(
            "INSERT INTO acp_session_index_state(owner_principal, current_version) VALUES (?1, ?2)",
            params![owner_principal, next],
        )?;
    }
    Ok(next)
}

fn ancestor_chain(
    transaction: &rusqlite::Transaction<'_>,
    owner_principal: &str,
    start: Option<&str>,
) -> rusqlite::Result<Vec<String>> {
    let mut chain = Vec::new();
    let mut current = start.map(str::to_string);
    let mut seen = std::collections::HashSet::new();
    while let Some(session_id) = current {
        if !seen.insert(session_id.clone()) {
            return Err(rusqlite::Error::InvalidParameterName("parent cycle".into()));
        }
        let Some((parent_session_id, archived_at)) = transaction
            .query_row(
                "SELECT parent_session_id, archived_at FROM acp_sessions WHERE session_id = ?1 AND owner_principal = ?2",
                params![session_id, owner_principal],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?
        else {
            break;
        };
        if archived_at.is_some() {
            break;
        }
        chain.push(session_id);
        current = parent_session_id;
    }
    Ok(chain)
}

impl SessionRepository {
    pub fn new(db_path: impl Into<PathBuf>) -> rusqlite::Result<Self> {
        let repository = Self {
            db_path: db_path.into(),
        };
        repository.ensure_schema()?;
        Ok(repository)
    }

    fn connection(&self) -> rusqlite::Result<Connection> {
        let connection = Connection::open(&self.db_path)?;
        // Multiple ACP tasks can create/update sessions concurrently. SQLite
        // otherwise surfaces a transient `database is locked` before the
        // writer transaction has a chance to commit, which would turn a
        // successful session/new into a spurious ACP internal error.
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=30000;")?;
        Ok(connection)
    }

    fn ensure_schema(&self) -> rusqlite::Result<()> {
        let mut connection = self.connection()?;
        // Acquire the write lock before reading any legacy schema state. A
        // deferred transaction lets several initializers read concurrently
        // and then fail with SQLITE_BUSY when they all upgrade to DDL.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS acp_sessions (
                session_id      TEXT PRIMARY KEY,
                thread_id       TEXT NOT NULL,
                owner_principal TEXT NOT NULL,
                cwd             TEXT NOT NULL,
                lifecycle       TEXT NOT NULL DEFAULT 'idle',
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL,
                closed_at       TEXT,
                title           TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_updated
                ON acp_sessions(owner_principal, updated_at DESC);
            CREATE TABLE IF NOT EXISTS acp_session_data (
                session_id TEXT PRIMARY KEY,
                metadata_json TEXT NOT NULL DEFAULT '{}'
            );
            "#,
        )?;
        Self::ensure_title_column(&transaction)?;
        Self::ensure_archived_at_column(&transaction)?;
        Self::ensure_session_index_columns(&transaction)?;
        Self::ensure_index_tables(&transaction)?;
        Self::ensure_metadata_foreign_key(&transaction)?;
        let has_foreign_key_error = {
            let mut check = transaction.prepare("PRAGMA foreign_key_check")?;
            let result = check.query([])?.next()?.is_some();
            result
        };
        if has_foreign_key_error {
            return Err(rusqlite::Error::InvalidQuery);
        }
        transaction.commit()
    }

    fn ensure_session_index_columns(connection: &Connection) -> rusqlite::Result<()> {
        let columns = [
            ("parent_session_id", "TEXT"),
            ("activity_at", "TEXT"),
            ("tree_activity_at", "TEXT"),
            ("state_changed_at", "TEXT"),
            ("metadata_updated_at", "TEXT"),
            ("revision", "INTEGER NOT NULL DEFAULT 1"),
            ("index_version", "INTEGER NOT NULL DEFAULT 1"),
        ];
        for (name, definition) in columns {
            let exists = connection
                .prepare("SELECT 1 FROM pragma_table_info('acp_sessions') WHERE name = ?1")?
                .query_row([name], |_| Ok(()))
                .optional()?
                .is_some();
            if !exists {
                if let Err(error) = connection.execute_batch(&format!(
                    "ALTER TABLE acp_sessions ADD COLUMN {name} {definition};"
                )) {
                    // Multiple ACP agents can initialize the shared default
                    // database concurrently in tests or embedded runtimes.
                    // Another initializer may have won the race between the
                    // pragma read and ALTER; treat only that exact outcome as
                    // an idempotent migration success.
                    if !error
                        .to_string()
                        .to_ascii_lowercase()
                        .contains("duplicate column name")
                    {
                        return Err(error);
                    }
                }
            }
        }
        connection.execute(
            "UPDATE acp_sessions SET activity_at = COALESCE(activity_at, updated_at, created_at), tree_activity_at = COALESCE(tree_activity_at, activity_at, updated_at, created_at), revision = COALESCE(revision, 1), index_version = COALESCE(index_version, 1)",
            [],
        )?;
        connection.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_tree_activity
                ON acp_sessions(owner_principal, tree_activity_at DESC, session_id ASC);
            CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_parent_activity
                ON acp_sessions(owner_principal, parent_session_id, activity_at DESC, session_id ASC);
            CREATE INDEX IF NOT EXISTS idx_acp_sessions_owner_archived
                ON acp_sessions(owner_principal, archived_at, session_id ASC);
            "#,
        )?;
        Ok(())
    }

    fn ensure_index_tables(connection: &Connection) -> rusqlite::Result<()> {
        connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS acp_session_index_state (
                owner_principal TEXT PRIMARY KEY,
                current_version INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS acp_session_tombstones (
                session_id TEXT PRIMARY KEY,
                owner_principal TEXT NOT NULL,
                cwd TEXT NOT NULL,
                parent_session_id TEXT,
                revision INTEGER NOT NULL,
                index_version INTEGER NOT NULL,
                deleted_at TEXT NOT NULL
            );
            CREATE TRIGGER IF NOT EXISTS acp_sessions_revision_json_safe
            BEFORE UPDATE OF revision ON acp_sessions
            WHEN NEW.revision > 9007199254740991
            BEGIN
                SELECT RAISE(ABORT, 'session revision exceeds JSON safe integer range');
            END;
            CREATE TRIGGER IF NOT EXISTS acp_session_tombstones_revision_json_safe
            BEFORE INSERT ON acp_session_tombstones
            WHEN NEW.revision > 9007199254740991
            BEGIN
                SELECT RAISE(ABORT, 'tombstone revision exceeds JSON safe integer range');
            END;
            "#,
        )?;
        connection.execute(
            "INSERT INTO acp_session_index_state(owner_principal, current_version) SELECT owner_principal, MAX(COALESCE(index_version, 1)) FROM acp_sessions GROUP BY owner_principal ON CONFLICT(owner_principal) DO UPDATE SET current_version = MAX(current_version, excluded.current_version)",
            [],
        )?;
        Ok(())
    }

    fn ensure_metadata_foreign_key(connection: &Connection) -> rusqlite::Result<()> {
        let has_cascade: bool = connection
            .prepare("PRAGMA foreign_key_list('acp_session_data')")?
            .query_map([], |row| {
                let on_delete: String = row.get(6)?;
                Ok(on_delete.eq_ignore_ascii_case("CASCADE"))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .any(|value| value);
        if has_cascade {
            return Ok(());
        }
        connection.execute_batch(
            r#"
            ALTER TABLE acp_session_data RENAME TO acp_session_data_legacy;
            CREATE TABLE acp_session_data (
                session_id TEXT PRIMARY KEY,
                metadata_json TEXT NOT NULL DEFAULT '{}',
                FOREIGN KEY(session_id) REFERENCES acp_sessions(session_id) ON DELETE CASCADE
            );
            INSERT INTO acp_session_data(session_id, metadata_json)
                SELECT d.session_id, d.metadata_json
                FROM acp_session_data_legacy d
                WHERE EXISTS (SELECT 1 FROM acp_sessions s WHERE s.session_id = d.session_id);
            DROP TABLE acp_session_data_legacy;
            "#,
        )
    }

    /// Add the `title` column to databases created before it existed.
    fn ensure_title_column(connection: &Connection) -> rusqlite::Result<()> {
        let has_title = connection
            .prepare("SELECT 1 FROM pragma_table_info('acp_sessions') WHERE name = 'title'")?
            .exists([])?;
        if !has_title {
            connection.execute_batch("ALTER TABLE acp_sessions ADD COLUMN title TEXT")?;
        }
        Ok(())
    }

    /// Add the `archived_at` column to databases created before it existed.
    fn ensure_archived_at_column(connection: &Connection) -> rusqlite::Result<()> {
        let has_archived = connection
            .prepare("SELECT 1 FROM pragma_table_info('acp_sessions') WHERE name = 'archived_at'")?
            .exists([])?;
        if !has_archived {
            if let Err(error) =
                connection.execute_batch("ALTER TABLE acp_sessions ADD COLUMN archived_at TEXT")
            {
                // Another ACP runtime may have added the column after the
                // pragma check and before this ALTER. Treat only that exact
                // race as a successful, idempotent migration.
                if !error
                    .to_string()
                    .to_ascii_lowercase()
                    .contains("duplicate column name")
                {
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    pub fn insert(
        &self,
        session_id: &str,
        thread_id: &str,
        owner_principal: &str,
        cwd: &Path,
    ) -> rusqlite::Result<()> {
        let now = utc_now_microseconds();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let index_version = allocate_owner_version(&transaction, owner_principal)?;
        transaction.execute(
            r#"
            INSERT INTO acp_sessions
                (session_id, thread_id, owner_principal, cwd, lifecycle, created_at, updated_at,
                 activity_at, tree_activity_at, revision, index_version)
            VALUES (?1, ?2, ?3, ?4, 'idle', ?5, ?5, ?5, ?5, 1, ?6)
            "#,
            params![
                session_id,
                thread_id,
                owner_principal,
                cwd.to_string_lossy().as_ref(),
                now,
                index_version,
            ],
        )?;
        transaction.commit()
    }

    /// Atomically create a SessionIndex record and its optional parent/title/
    /// metadata projection. The target and every visible ancestor share one
    /// owner-wide index version so `session/new` can return a self-consistent
    /// target/ancestor response.
    #[allow(clippy::too_many_arguments)]
    pub fn insert_index_record(
        &self,
        session_id: &str,
        thread_id: &str,
        owner_principal: &str,
        cwd: &Path,
        parent_session_id: Option<&str>,
        title: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> rusqlite::Result<Vec<SessionIndexRecord>> {
        // A separate checkpoint/config connection can briefly hold the same
        // SQLite file while session/new is materializing its index record.
        // busy_timeout handles ordinary overlap; bounded retries also cover
        // transactions that return SQLITE_BUSY/LOCKED after a snapshot was
        // invalidated, without retrying validation or constraint failures.
        retry_sqlite_busy(|| {
            self.insert_index_record_once(
                session_id,
                thread_id,
                owner_principal,
                cwd,
                parent_session_id,
                title,
                metadata,
            )
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_index_record_once(
        &self,
        session_id: &str,
        thread_id: &str,
        owner_principal: &str,
        cwd: &Path,
        parent_session_id: Option<&str>,
        title: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> rusqlite::Result<Vec<SessionIndexRecord>> {
        let now = utc_now_microseconds();
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        if let Some(parent_id) = parent_session_id {
            let parent: Option<(String, String, Option<String>)> = transaction
                .query_row(
                    "SELECT owner_principal, cwd, archived_at FROM acp_sessions WHERE session_id = ?1",
                    [parent_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((parent_owner, parent_cwd, archived_at)) = parent else {
                return Err(rusqlite::Error::InvalidParameterName(
                    "unknown parent".into(),
                ));
            };
            if parent_owner != owner_principal
                || parent_cwd != cwd.to_string_lossy()
                || archived_at.is_some()
            {
                return Err(rusqlite::Error::InvalidParameterName(
                    "invalid parent".into(),
                ));
            }
        }
        let ancestors = ancestor_chain(&transaction, owner_principal, parent_session_id)?;
        let version = allocate_owner_version(&transaction, owner_principal)?;
        transaction.execute(
            r#"
            INSERT INTO acp_sessions
                (session_id, thread_id, owner_principal, cwd, parent_session_id,
                 lifecycle, title, created_at, updated_at, activity_at,
                 tree_activity_at, revision, index_version)
            VALUES (?1, ?2, ?3, ?4, ?5, 'idle', ?6, ?7, ?7, ?7, ?7, 1, ?8)
            "#,
            params![
                session_id,
                thread_id,
                owner_principal,
                cwd.to_string_lossy().as_ref(),
                parent_session_id,
                title,
                now,
                version,
            ],
        )?;
        if let Some(metadata) = metadata {
            transaction.execute(
                "INSERT INTO acp_session_data(session_id, metadata_json) VALUES (?1, ?2)",
                params![
                    session_id,
                    serde_json::to_string(metadata).map_err(|error| {
                        rusqlite::Error::ToSqlConversionFailure(Box::new(error))
                    })?
                ],
            )?;
        }
        for ancestor_id in &ancestors {
            let tree_activity: Option<String> = transaction.query_row(
                r#"WITH RECURSIVE descendants(session_id) AS (
                     SELECT ?1
                     UNION ALL
                     SELECT child.session_id
                     FROM acp_sessions child
                     JOIN descendants parent ON child.parent_session_id = parent.session_id
                     WHERE child.owner_principal = ?2 AND child.archived_at IS NULL
                   )
                   SELECT MAX(COALESCE(s.activity_at, s.updated_at, s.created_at))
                   FROM acp_sessions s JOIN descendants d ON d.session_id = s.session_id
                   WHERE s.owner_principal = ?2 AND s.archived_at IS NULL"#,
                params![ancestor_id, owner_principal],
                |row| row.get(0),
            )?;
            transaction.execute(
                "UPDATE acp_sessions SET tree_activity_at = COALESCE(?2, activity_at, updated_at, created_at), revision = revision + 1, index_version = ?3 WHERE session_id = ?1 AND owner_principal = ?4",
                params![ancestor_id, tree_activity, version, owner_principal],
            )?;
        }
        transaction.commit()?;

        let mut records = Vec::with_capacity(ancestors.len() + 1);
        if let Some(record) = self.get_index_record(owner_principal, session_id)? {
            records.push(record);
        }
        for ancestor_id in ancestors {
            if let Some(record) = self.get_index_record(owner_principal, &ancestor_id)? {
                records.push(record);
            }
        }
        Ok(records)
    }

    pub fn get(&self, session_id: &str) -> rusqlite::Result<Option<SessionMetadata>> {
        self.connection()?
            .query_row(
                r#"
                SELECT session_id, thread_id, owner_principal, cwd, lifecycle,
                       title, updated_at, created_at, archived_at
                FROM acp_sessions WHERE session_id = ?1
                "#,
                [session_id],
                Self::metadata_from_row,
            )
            .optional()
    }

    /// Read the canonical index record, including Desk-owned metadata.
    pub fn get_index_record(
        &self,
        owner_principal: &str,
        session_id: &str,
    ) -> rusqlite::Result<Option<SessionIndexRecord>> {
        let connection = self.connection()?;
        connection
            .query_row(
                r#"
                SELECT s.session_id, s.parent_session_id, s.owner_principal, s.cwd,
                       s.lifecycle, s.title, d.metadata_json, s.created_at,
                       COALESCE(s.activity_at, s.updated_at, s.created_at),
                       COALESCE(s.tree_activity_at, s.activity_at, s.updated_at, s.created_at),
                       s.state_changed_at, s.metadata_updated_at, s.archived_at, s.closed_at,
                       COALESCE(s.revision, 1), COALESCE(s.index_version, 1)
                FROM acp_sessions s
                LEFT JOIN acp_session_data d ON d.session_id = s.session_id
                WHERE s.owner_principal = ?1 AND s.session_id = ?2
                "#,
                params![owner_principal, session_id],
                Self::index_record_from_row,
            )
            .optional()
    }

    /// Return the persisted parent chain for a session, nearest parent first.
    ///
    /// This is used by ACP mutation responses that must include the canonical
    /// target plus any ancestor projections changed by a tree mutation. The
    /// chain is collected before records are read so each returned descriptor
    /// is owner-scoped and no caller needs to infer hierarchy from titles or
    /// event ordering.
    pub fn ancestor_index_records(
        &self,
        owner_principal: &str,
        session_id: &str,
    ) -> rusqlite::Result<Vec<SessionIndexRecord>> {
        let connection = self.connection()?;
        let mut current: Option<String> = connection
            .query_row(
                "SELECT parent_session_id FROM acp_sessions WHERE session_id = ?1 AND owner_principal = ?2",
                params![session_id, owner_principal],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let mut ids = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while let Some(parent_id) = current {
            if !seen.insert(parent_id.clone()) {
                break;
            }
            ids.push(parent_id.clone());
            current = connection
                .query_row(
                    "SELECT parent_session_id FROM acp_sessions WHERE session_id = ?1 AND owner_principal = ?2",
                    params![parent_id, owner_principal],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
        }
        drop(connection);
        ids.into_iter()
            .map(|id| self.get_index_record(owner_principal, &id))
            .filter_map(|result| result.transpose())
            .collect()
    }

    /// Read the complete owner-scoped SessionIndex projection. The query is
    /// deliberately shared by the private extension and standard ACP list;
    /// callers choose the projection and pagination policy above this layer.
    pub fn list_index_for_owner(
        &self,
        owner_principal: &str,
        cwd: Option<&str>,
        archived: &str,
    ) -> rusqlite::Result<Vec<SessionIndexRecord>> {
        let connection = self.connection()?;
        let mut sql = String::from(
            r#"
            SELECT s.session_id, s.parent_session_id, s.owner_principal, s.cwd,
                   s.lifecycle, s.title, d.metadata_json, s.created_at,
                   COALESCE(s.activity_at, s.updated_at, s.created_at),
                   COALESCE(s.tree_activity_at, s.activity_at, s.updated_at, s.created_at),
                   s.state_changed_at, s.metadata_updated_at, s.archived_at, s.closed_at,
                   COALESCE(s.revision, 1), COALESCE(s.index_version, 1)
            FROM acp_sessions s
            LEFT JOIN acp_session_data d ON d.session_id = s.session_id
            WHERE s.owner_principal = ?1
            "#,
        );
        match archived {
            "active" => sql.push_str(" AND s.archived_at IS NULL"),
            "archived" => sql.push_str(" AND s.archived_at IS NOT NULL"),
            "all" => {}
            _ => return Err(rusqlite::Error::InvalidParameterName("archived".into())),
        }
        if cwd.is_some() {
            sql.push_str(
                r#" AND replace(replace(s.cwd, '\\?\', ''), '\', '/')
                     = replace(replace(?2, '\\?\', ''), '\', '/')"#,
            );
        }
        let mut statement = connection.prepare(&sql)?;
        let rows: rusqlite::Result<Vec<SessionIndexRecord>> = if cwd.is_some() {
            statement
                .query_map(params![owner_principal, cwd], Self::index_record_from_row)?
                .collect()
        } else {
            statement
                .query_map(params![owner_principal], Self::index_record_from_row)?
                .collect()
        };
        let records = rows?;
        let (mut active, mut archived_records): (Vec<_>, Vec<_>) = records
            .into_iter()
            .partition(|record| record.archived_at.is_none());
        active = order_active_tree(active);
        archived_records.sort_by(|a, b| {
            b.archived_at
                .cmp(&a.archived_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        active.extend(archived_records);
        Ok(active)
    }

    pub fn owner_index_version(&self, owner_principal: &str) -> rusqlite::Result<i64> {
        self.connection()?
            .query_row(
                "SELECT current_version FROM acp_session_index_state WHERE owner_principal = ?1",
                [owner_principal],
                |row| row.get(0),
            )
            .optional()
            .map(|value| value.unwrap_or(0))
    }

    pub fn get_tombstone(&self, session_id: &str) -> rusqlite::Result<Option<SessionTombstone>> {
        self.connection()?
            .query_row(
                "SELECT session_id, owner_principal, cwd, parent_session_id, revision, index_version, deleted_at FROM acp_session_tombstones WHERE session_id = ?1",
                [session_id],
                |row| Ok(SessionTombstone {
                    session_id: row.get(0)?,
                    owner_principal: row.get(1)?,
                    cwd: PathBuf::from(row.get::<_, String>(2)?),
                    parent_session_id: row.get(3)?,
                    revision: row.get(4)?,
                    index_version: row.get(5)?,
                    deleted_at: row.get(6)?,
                }),
            )
            .optional()
    }

    /// Set a session parent while maintaining the owner/cwd and cycle
    /// invariants. Tree activity is recomputed for both the old and new
    /// ancestor chains in the same SQLite transaction.
    pub fn set_parent(
        &self,
        owner_principal: &str,
        session_id: &str,
        parent_session_id: Option<&str>,
    ) -> rusqlite::Result<Option<SessionIndexRecord>> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let child: Option<(String, String, Option<String>)> = transaction
            .query_row(
                "SELECT owner_principal, cwd, parent_session_id FROM acp_sessions WHERE session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((child_owner, child_cwd, old_parent)) = child else {
            return Ok(None);
        };
        if child_owner != owner_principal || old_parent.as_deref() == parent_session_id {
            if child_owner != owner_principal {
                return Ok(None);
            }
            drop(transaction);
            return self.get_index_record(owner_principal, session_id);
        }
        if let Some(parent_id) = parent_session_id {
            if parent_id == session_id {
                return Err(rusqlite::Error::InvalidParameterName("self parent".into()));
            }
            let parent: Option<(String, String)> = transaction
                .query_row(
                    "SELECT owner_principal, cwd FROM acp_sessions WHERE session_id = ?1",
                    [parent_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((parent_owner, parent_cwd)) = parent else {
                return Err(rusqlite::Error::InvalidParameterName(
                    "unknown parent".into(),
                ));
            };
            if parent_owner != owner_principal || parent_cwd != child_cwd {
                return Err(rusqlite::Error::InvalidParameterName(
                    "invalid parent".into(),
                ));
            }
            let mut cursor = Some(parent_id.to_string());
            let mut seen = std::collections::HashSet::new();
            while let Some(current) = cursor {
                if !seen.insert(current.clone()) || current == session_id {
                    return Err(rusqlite::Error::InvalidParameterName("parent cycle".into()));
                }
                cursor = transaction
                    .query_row(
                        "SELECT parent_session_id FROM acp_sessions WHERE session_id = ?1 AND owner_principal = ?2",
                        params![current, owner_principal],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
            }
        }
        let mut affected = ancestor_chain(&transaction, owner_principal, old_parent.as_deref())?;
        affected.extend(ancestor_chain(
            &transaction,
            owner_principal,
            parent_session_id,
        )?);
        affected.push(session_id.to_string());
        let version = allocate_owner_version(&transaction, owner_principal)?;
        transaction.execute(
            "UPDATE acp_sessions SET parent_session_id = ?2, revision = revision + 1, index_version = ?3 WHERE session_id = ?1 AND owner_principal = ?4",
            params![session_id, parent_session_id, version, owner_principal],
        )?;
        affected.sort();
        affected.dedup();
        for affected_id in affected {
            let tree_activity: Option<String> = transaction.query_row(
                r#"WITH RECURSIVE descendants(session_id) AS (
                     SELECT ?1
                     UNION ALL
                     SELECT child.session_id
                     FROM acp_sessions child
                     JOIN descendants parent ON child.parent_session_id = parent.session_id
                     WHERE child.owner_principal = ?2 AND child.archived_at IS NULL
                   )
                   SELECT MAX(COALESCE(s.activity_at, s.updated_at, s.created_at))
                   FROM acp_sessions s JOIN descendants d ON d.session_id = s.session_id
                   WHERE s.owner_principal = ?2 AND s.archived_at IS NULL"#,
                params![affected_id, owner_principal],
                |row| row.get(0),
            )?;
            transaction.execute(
                "UPDATE acp_sessions SET tree_activity_at = COALESCE(?2, activity_at, updated_at, created_at), revision = revision + 1, index_version = ?3 WHERE session_id = ?1 AND owner_principal = ?4",
                params![affected_id, tree_activity, version, owner_principal],
            )?;
        }
        transaction.commit()?;
        self.get_index_record(owner_principal, session_id)
    }

    /// Record one accepted prompt at the repository boundary. The session's
    /// own activity and every active ancestor's tree activity are updated in
    /// the same transaction and receive one owner-scoped index version.
    pub fn record_activity(&self, session_id: &str) -> rusqlite::Result<Vec<SessionIndexRecord>> {
        // Concurrent prompts share the session-index database. Retry the
        // complete mutation so SQLITE_BUSY/LOCKED never reuses a stale read
        // snapshot from a failed transaction upgrade.
        retry_sqlite_busy(|| self.record_activity_once(session_id))
    }

    fn record_activity_once(&self, session_id: &str) -> rusqlite::Result<Vec<SessionIndexRecord>> {
        let mut connection = self.connection()?;
        // Acquire the writer reservation before reading activity/version
        // state. A deferred transaction can let two prompts obtain read
        // snapshots and then fail immediately when both attempt to upgrade.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some((owner_principal, parent_session_id, previous_activity)) = transaction
            .query_row(
                "SELECT owner_principal, parent_session_id, activity_at FROM acp_sessions WHERE session_id = ?1",
                [session_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(Vec::new());
        };
        let now = next_monotonic_timestamp(previous_activity.as_deref(), &utc_now_microseconds());
        let version = allocate_owner_version(&transaction, &owner_principal)?;
        transaction.execute(
            "UPDATE acp_sessions SET activity_at = ?2, tree_activity_at = ?2, revision = revision + 1, index_version = ?3 WHERE session_id = ?1",
            params![session_id, now, version],
        )?;

        let mut affected =
            ancestor_chain(&transaction, &owner_principal, parent_session_id.as_deref())?;
        affected.push(session_id.to_string());
        affected.sort();
        affected.dedup();
        for affected_id in &affected {
            let tree_activity: Option<String> = transaction.query_row(
                r#"WITH RECURSIVE descendants(session_id) AS (
                     SELECT ?1
                     UNION ALL
                     SELECT child.session_id
                     FROM acp_sessions child
                     JOIN descendants parent ON child.parent_session_id = parent.session_id
                     WHERE child.owner_principal = ?2 AND child.archived_at IS NULL
                   )
                   SELECT MAX(COALESCE(s.activity_at, s.updated_at, s.created_at))
                   FROM acp_sessions s JOIN descendants d ON d.session_id = s.session_id
                   WHERE s.owner_principal = ?2 AND s.archived_at IS NULL"#,
                params![affected_id, owner_principal],
                |row| row.get(0),
            )?;
            transaction.execute(
                "UPDATE acp_sessions SET tree_activity_at = COALESCE(?2, activity_at, updated_at, created_at), revision = CASE WHEN session_id = ?1 THEN revision ELSE revision + 1 END, index_version = ?3 WHERE session_id = ?1 AND owner_principal = ?4",
                params![affected_id, tree_activity, version, owner_principal],
            )?;
        }
        transaction.commit()?;
        affected
            .into_iter()
            .map(|id| self.get_index_record(&owner_principal, &id))
            .filter_map(|result| result.transpose())
            .collect()
    }

    /// Restore process-local session handles after an ACP restart.
    ///
    /// This is intentionally not a public session-list projection: external
    /// membership/order queries must use `list_index_for_owner` so restore
    /// bookkeeping cannot become a second list implementation.
    pub fn list_for_restore(&self) -> rusqlite::Result<Vec<SessionMetadata>> {
        let connection = self.connection()?;
        let mut statement = connection.prepare(
            r#"
            SELECT session_id, thread_id, owner_principal, cwd, lifecycle,
                   title, updated_at, created_at, archived_at
            FROM acp_sessions ORDER BY updated_at ASC
            "#,
        )?;
        let rows = statement.query_map([], Self::metadata_from_row)?.collect();
        rows
    }

    /// Persist an auto-generated session title. Does not bump `updated_at`
    /// so title generation never reorders the session list.
    pub fn set_title(&self, session_id: &str, title: &str) -> rusqlite::Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let current: Option<(String, Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT owner_principal, title, metadata_updated_at FROM acp_sessions WHERE session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((owner_principal, current_title, previous_metadata_updated_at)) = current else {
            return Ok(());
        };
        if current_title.as_deref() == Some(title) {
            return Ok(());
        }
        let version = allocate_owner_version(&transaction, &owner_principal)?;
        let now = next_monotonic_timestamp(
            previous_metadata_updated_at.as_deref(),
            &utc_now_microseconds(),
        );
        transaction.execute(
            "UPDATE acp_sessions SET title = ?2, metadata_updated_at = ?3, revision = revision + 1, index_version = ?4 WHERE session_id = ?1",
            params![session_id, title, now, version],
        )?;
        transaction.commit()
    }

    /// Read arbitrary anureo Desk metadata owned by a session.
    pub fn get_metadata_json(&self, session_id: &str) -> rusqlite::Result<Option<String>> {
        self.connection()?
            .query_row(
                "SELECT metadata_json FROM acp_session_data WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
    }

    /// Replace arbitrary anureo Desk metadata when the session belongs to the
    /// requested principal. The metadata is deliberately separate from ACP's
    /// protocol-owned session columns so extensions cannot corrupt lifecycle
    /// or routing state.
    pub fn set_metadata_json(
        &self,
        session_id: &str,
        owner_principal: &str,
        metadata_json: &str,
    ) -> rusqlite::Result<bool> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let owned: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM acp_sessions WHERE session_id = ?1 AND owner_principal = ?2)",
            params![session_id, owner_principal],
            |row| row.get(0),
        )?;
        if !owned {
            return Ok(false);
        }
        let previous: String = transaction
            .query_row(
                "SELECT COALESCE(metadata_json, '{}') FROM acp_session_data WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "{}".into());
        if previous == metadata_json {
            return Ok(true);
        }
        let (owner_principal, previous_metadata_updated_at): (String, Option<String>) = transaction.query_row(
            "SELECT owner_principal, metadata_updated_at FROM acp_sessions WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let version = allocate_owner_version(&transaction, &owner_principal)?;
        transaction.execute(
            "INSERT INTO acp_session_data (session_id, metadata_json) VALUES (?1, ?2) ON CONFLICT(session_id) DO UPDATE SET metadata_json = excluded.metadata_json",
            params![session_id, metadata_json],
        )?;
        let now = next_monotonic_timestamp(
            previous_metadata_updated_at.as_deref(),
            &utc_now_microseconds(),
        );
        transaction.execute(
            "UPDATE acp_sessions SET metadata_updated_at = ?2, revision = revision + 1, index_version = ?3 WHERE session_id = ?1",
            params![session_id, now, version],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// Atomically apply the title and/or Desk metadata fields. A combined
    /// update consumes one owner index version and returns the canonical
    /// target projection, avoiding response/event reads from different
    /// mutation points.
    pub fn update_index_fields(
        &self,
        session_id: &str,
        owner_principal: &str,
        title: Option<&str>,
        metadata_json: Option<&str>,
    ) -> rusqlite::Result<Option<SessionIndexRecord>> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let Some((current_title, current_metadata, previous_metadata_updated_at)) = transaction
            .query_row(
                r#"SELECT s.title,
                          (SELECT metadata_json FROM acp_session_data d WHERE d.session_id = s.session_id),
                          s.metadata_updated_at
                   FROM acp_sessions s
                   WHERE s.session_id = ?1 AND s.owner_principal = ?2"#,
                params![session_id, owner_principal],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        let metadata_changed =
            metadata_json.is_some_and(|value| current_metadata.as_deref().unwrap_or("{}") != value);
        let title_changed = title.is_some_and(|value| current_title.as_deref() != Some(value));
        if !metadata_changed && !title_changed {
            drop(transaction);
            return self.get_index_record(owner_principal, session_id);
        }

        let version = allocate_owner_version(&transaction, owner_principal)?;
        let now = next_monotonic_timestamp(
            previous_metadata_updated_at.as_deref(),
            &utc_now_microseconds(),
        );
        if let Some(title) = title.filter(|_| title_changed) {
            transaction.execute(
                "UPDATE acp_sessions SET title = ?2 WHERE session_id = ?1 AND owner_principal = ?3",
                params![session_id, title, owner_principal],
            )?;
        }
        if let Some(metadata_json) = metadata_json.filter(|_| metadata_changed) {
            transaction.execute(
                "INSERT INTO acp_session_data (session_id, metadata_json) VALUES (?1, ?2) ON CONFLICT(session_id) DO UPDATE SET metadata_json = excluded.metadata_json",
                params![session_id, metadata_json],
            )?;
        }
        transaction.execute(
            "UPDATE acp_sessions SET metadata_updated_at = ?2, revision = revision + 1, index_version = ?3 WHERE session_id = ?1 AND owner_principal = ?4",
            params![session_id, now, version, owner_principal],
        )?;
        let canonical = transaction
            .query_row(
                r#"SELECT s.session_id, s.parent_session_id, s.owner_principal, s.cwd,
                          s.lifecycle, s.title, d.metadata_json, s.created_at,
                          COALESCE(s.activity_at, s.updated_at, s.created_at),
                          COALESCE(s.tree_activity_at, s.activity_at, s.updated_at, s.created_at),
                          s.state_changed_at, s.metadata_updated_at, s.archived_at, s.closed_at,
                          COALESCE(s.revision, 1), COALESCE(s.index_version, 1)
                   FROM acp_sessions s
                   LEFT JOIN acp_session_data d ON d.session_id = s.session_id
                   WHERE s.session_id = ?1 AND s.owner_principal = ?2"#,
                params![session_id, owner_principal],
                Self::index_record_from_row,
            )
            .optional()?;
        transaction.commit()?;
        Ok(canonical)
    }

    /// Set or clear the archival timestamp. Bumps `updated_at` (matching
    /// anureo semantics where archiving is a list-reordering mutation)
    /// and returns the stored metadata, or `None` when the session does not
    /// exist or belongs to another principal.
    fn set_archived_internal(
        &self,
        session_id: &str,
        owner_principal: &str,
        archived: bool,
    ) -> rusqlite::Result<Option<(SessionMetadata, Vec<SessionIndexRecord>)>> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let current: Option<(Option<String>, Option<String>, Option<String>)> = transaction
            .query_row(
                "SELECT archived_at, parent_session_id, state_changed_at FROM acp_sessions WHERE session_id = ?1 AND owner_principal = ?2",
                params![session_id, owner_principal],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((current_archived_at, parent_session_id, previous_state_changed_at)) = current
        else {
            return Ok(None);
        };
        if current_archived_at.is_some() == archived {
            drop(transaction);
            let metadata = self.get(session_id)?;
            let record = self.get_index_record(owner_principal, session_id)?;
            return Ok(metadata.zip(record.map(|item| vec![item])));
        }
        let now = next_monotonic_timestamp(
            previous_state_changed_at.as_deref(),
            &utc_now_microseconds(),
        );
        let archived_at = archived.then_some(now.clone());
        let version = allocate_owner_version(&transaction, owner_principal)?;
        transaction.execute(
            r#"
            UPDATE acp_sessions
            SET archived_at = ?2, updated_at = ?3, state_changed_at = ?3,
                revision = revision, index_version = ?5
            WHERE session_id = ?1 AND owner_principal = ?4
            "#,
            params![session_id, archived_at, now, owner_principal, version],
        )?;
        let mut affected =
            ancestor_chain(&transaction, owner_principal, parent_session_id.as_deref())?;
        affected.push(session_id.to_string());
        affected.sort();
        affected.dedup();
        let changed_ids = affected.clone();
        for affected_id in affected {
            let tree_activity: Option<String> = transaction.query_row(
                r#"WITH RECURSIVE descendants(session_id) AS (
                     SELECT ?1
                     UNION ALL
                     SELECT child.session_id
                     FROM acp_sessions child
                     JOIN descendants parent ON child.parent_session_id = parent.session_id
                     WHERE child.owner_principal = ?2 AND child.archived_at IS NULL
                   )
                   SELECT MAX(COALESCE(s.activity_at, s.updated_at, s.created_at))
                   FROM acp_sessions s JOIN descendants d ON d.session_id = s.session_id
                   WHERE s.owner_principal = ?2 AND s.archived_at IS NULL"#,
                params![affected_id, owner_principal],
                |row| row.get(0),
            )?;
            transaction.execute(
                "UPDATE acp_sessions SET tree_activity_at = COALESCE(?2, activity_at, updated_at, created_at), revision = revision + 1, index_version = ?3 WHERE session_id = ?1 AND owner_principal = ?4",
                params![affected_id, tree_activity, version, owner_principal],
            )?;
        }
        let mut records = Vec::with_capacity(changed_ids.len());
        for changed_id in changed_ids {
            if let Some(record) = transaction
                .query_row(
                    r#"SELECT s.session_id, s.parent_session_id, s.owner_principal, s.cwd,
                              s.lifecycle, s.title, d.metadata_json, s.created_at,
                              COALESCE(s.activity_at, s.updated_at, s.created_at),
                              COALESCE(s.tree_activity_at, s.activity_at, s.updated_at, s.created_at),
                              s.state_changed_at, s.metadata_updated_at, s.archived_at, s.closed_at,
                              COALESCE(s.revision, 1), COALESCE(s.index_version, 1)
                       FROM acp_sessions s
                       LEFT JOIN acp_session_data d ON d.session_id = s.session_id
                       WHERE s.session_id = ?1 AND s.owner_principal = ?2"#,
                    params![changed_id, owner_principal],
                    Self::index_record_from_row,
                )
                .optional()?
            {
                records.push(record);
            }
        }
        transaction.commit()?;
        let metadata = self
            .get(session_id)?
            .ok_or(rusqlite::Error::QueryReturnedNoRows)?;
        Ok(Some((metadata, records)))
    }

    pub fn set_archived(
        &self,
        session_id: &str,
        owner_principal: &str,
        archived: bool,
    ) -> rusqlite::Result<Option<SessionMetadata>> {
        Ok(self
            .set_archived_internal(session_id, owner_principal, archived)?
            .map(|(metadata, _)| metadata))
    }

    /// Archive/restore projection captured before the write transaction
    /// commits, so response and event consumers share one mutation snapshot.
    pub fn set_archived_index_records(
        &self,
        session_id: &str,
        owner_principal: &str,
        archived: bool,
    ) -> rusqlite::Result<Option<Vec<SessionIndexRecord>>> {
        retry_sqlite_busy(|| {
            self.set_archived_internal(session_id, owner_principal, archived)
                .map(|result| result.map(|(_, records)| records))
        })
    }

    fn metadata_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionMetadata> {
        Ok(SessionMetadata {
            session_id: row.get(0)?,
            thread_id: row.get(1)?,
            owner_principal: row.get(2)?,
            cwd: PathBuf::from(row.get::<_, String>(3)?),
            lifecycle: row.get(4)?,
            title: row.get(5)?,
            updated_at: row.get(6)?,
            created_at: row.get(7)?,
            archived_at: row.get(8)?,
        })
    }

    fn index_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionIndexRecord> {
        let metadata_json: Option<String> = row.get(6)?;
        let metadata = metadata_json
            .as_deref()
            .map(serde_json::from_str)
            .transpose()
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?
            .unwrap_or_else(|| serde_json::json!({}));
        Ok(SessionIndexRecord {
            session_id: row.get(0)?,
            parent_session_id: row.get(1)?,
            owner_principal: row.get(2)?,
            cwd: PathBuf::from(row.get::<_, String>(3)?),
            lifecycle: row.get(4)?,
            title: row.get(5)?,
            metadata,
            created_at: row.get(7)?,
            activity_at: row.get(8)?,
            tree_activity_at: row.get(9)?,
            state_changed_at: row.get(10)?,
            metadata_updated_at: row.get(11)?,
            archived_at: row.get(12)?,
            closed_at: row.get(13)?,
            revision: row.get(14)?,
            index_version: row.get(15)?,
        })
    }

    pub fn set_lifecycle(&self, session_id: &str, lifecycle: &str) -> rusqlite::Result<()> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let current: Option<(String, String, Option<String>)> = transaction
            .query_row(
                "SELECT owner_principal, lifecycle, state_changed_at FROM acp_sessions WHERE session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((owner_principal, current_lifecycle, previous_state_changed_at)) = current else {
            return Ok(());
        };
        if current_lifecycle == lifecycle {
            return Ok(());
        }
        let now = next_monotonic_timestamp(
            previous_state_changed_at.as_deref(),
            &utc_now_microseconds(),
        );
        let closed_at = (lifecycle == "closed").then_some(now.clone());
        let version = allocate_owner_version(&transaction, &owner_principal)?;
        transaction.execute(
            r#"
            UPDATE acp_sessions
            SET lifecycle = ?2, updated_at = ?3, closed_at = ?4,
                state_changed_at = ?3, revision = revision + 1, index_version = ?5
            WHERE session_id = ?1
            "#,
            params![session_id, lifecycle, now, closed_at, version],
        )?;
        transaction.commit()
    }

    /// Delete ACP metadata and all session-owned rows in one SQLite
    /// transaction. Optional tables are skipped when their feature has never
    /// initialized its schema. The compatibility wrapper discards the
    /// canonical ancestor result; production delete paths should use
    /// `delete_all_indexed`.
    pub fn delete_all(&self, session_id: &str, thread_id: &str) -> rusqlite::Result<()> {
        self.delete_all_indexed(session_id, thread_id).map(|_| ())
    }

    /// Delete a session and return the durable tombstone plus every ancestor
    /// whose visible tree projection changed. Target deletion, tombstone
    /// insertion, and ancestor recomputation share one owner index version and
    /// one SQLite transaction.
    pub fn delete_all_indexed(
        &self,
        session_id: &str,
        thread_id: &str,
    ) -> rusqlite::Result<Option<SessionDeleteResult>> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction()?;
        let Some((owner_principal, cwd, parent_session_id, revision)) = transaction
            .query_row(
                "SELECT owner_principal, cwd, parent_session_id, revision FROM acp_sessions WHERE session_id = ?1",
                [session_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?, row.get::<_, i64>(3)?)),
            )
            .optional()?
        else {
            transaction.commit()?;
            return Ok(None);
        };

        let ancestors =
            ancestor_chain(&transaction, &owner_principal, parent_session_id.as_deref())?;
        let index_version = allocate_owner_version(&transaction, &owner_principal)?;
        let deleted_at = utc_now_microseconds();
        transaction.execute(
            "INSERT INTO acp_session_tombstones(session_id, owner_principal, cwd, parent_session_id, revision, index_version, deleted_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(session_id) DO NOTHING",
            params![session_id, owner_principal, cwd, parent_session_id, revision + 1, index_version, deleted_at],
        )?;

        let table_exists = |name: &str| -> rusqlite::Result<bool> {
            transaction
                .query_row(
                    "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [name],
                    |_| Ok(()),
                )
                .optional()
                .map(|value| value.is_some())
        };
        if table_exists("review_status")? {
            transaction.execute(
                "DELETE FROM review_status WHERE session_id = ?1",
                [session_id],
            )?;
        }
        if table_exists("review_history")? {
            transaction.execute(
                "DELETE FROM review_history WHERE session_id = ?1",
                [session_id],
            )?;
        }
        if table_exists("session_config")? {
            transaction.execute(
                "DELETE FROM session_config WHERE session_id = ?1",
                [session_id],
            )?;
        }
        if table_exists("acp_session_data")? {
            transaction.execute(
                "DELETE FROM acp_session_data WHERE session_id = ?1",
                [session_id],
            )?;
        }
        if table_exists("checkpoint_writes")? {
            transaction.execute(
                "DELETE FROM checkpoint_writes WHERE thread_id = ?1",
                [thread_id],
            )?;
        }
        if table_exists("checkpoints")? {
            transaction.execute("DELETE FROM checkpoints WHERE thread_id = ?1", [thread_id])?;
        }
        transaction.execute(
            "DELETE FROM acp_sessions WHERE session_id = ?1",
            [session_id],
        )?;

        // The deleted node is no longer part of the visible descendant CTE.
        // Recompute every persisted ancestor after removal so tree activity
        // can move backwards instead of remaining a stale high-water mark.
        for ancestor_id in &ancestors {
            let tree_activity: Option<String> = transaction.query_row(
                r#"WITH RECURSIVE descendants(session_id) AS (
                     SELECT ?1
                     UNION ALL
                     SELECT child.session_id
                     FROM acp_sessions child
                     JOIN descendants parent ON child.parent_session_id = parent.session_id
                     WHERE child.owner_principal = ?2 AND child.archived_at IS NULL
                   )
                   SELECT MAX(COALESCE(s.activity_at, s.updated_at, s.created_at))
                   FROM acp_sessions s JOIN descendants d ON d.session_id = s.session_id
                   WHERE s.owner_principal = ?2 AND s.archived_at IS NULL"#,
                params![ancestor_id, owner_principal],
                |row| row.get(0),
            )?;
            transaction.execute(
                "UPDATE acp_sessions SET tree_activity_at = COALESCE(?2, activity_at, updated_at, created_at), revision = revision + 1, index_version = ?3 WHERE session_id = ?1 AND owner_principal = ?4",
                params![ancestor_id, tree_activity, index_version, owner_principal],
            )?;
        }
        transaction.commit()?;

        let tombstone = self
            .get_tombstone(session_id)?
            .ok_or_else(|| rusqlite::Error::QueryReturnedNoRows)?;
        let affected_ancestors = ancestors
            .into_iter()
            .filter_map(|ancestor_id| {
                self.get_index_record(&owner_principal, &ancestor_id)
                    .transpose()
            })
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Some(SessionDeleteResult {
            tombstone,
            affected_ancestors,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_schema_migration_is_idempotent_and_removes_orphan_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("legacy-sessions.db");

        // Reproduce the pre-SessionIndex shape: no index columns, no owner
        // state/tombstone tables, and metadata without a foreign key.  Keep
        // one valid row and one orphan so the migration's filtering contract
        // is exercised rather than merely checking that CREATE TABLE works.
        {
            let connection = rusqlite::Connection::open(&db_path).unwrap();
            connection
                .execute_batch(
                    r#"
                    PRAGMA foreign_keys=OFF;
                    CREATE TABLE acp_sessions (
                        session_id TEXT PRIMARY KEY,
                        thread_id TEXT NOT NULL,
                        owner_principal TEXT NOT NULL,
                        cwd TEXT NOT NULL,
                        lifecycle TEXT NOT NULL DEFAULT 'idle',
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL,
                        closed_at TEXT,
                        title TEXT
                    );
                    CREATE TABLE acp_session_data (
                        session_id TEXT PRIMARY KEY,
                        metadata_json TEXT NOT NULL DEFAULT '{}'
                    );
                    INSERT INTO acp_sessions
                      (session_id, thread_id, owner_principal, cwd, lifecycle, created_at, updated_at)
                    VALUES ('legacy-session', 'legacy-thread', 'owner-a', '/tmp', 'idle',
                            '2026-08-22T00:00:00Z', '2026-08-22T00:00:01Z');
                    INSERT INTO acp_session_data(session_id, metadata_json)
                      VALUES ('legacy-session', '{"valid":true}');
                    INSERT INTO acp_session_data(session_id, metadata_json)
                      VALUES ('orphan-session', '{"orphan":true}');
                    "#,
                )
                .unwrap();
        }

        let repository = SessionRepository::new(&db_path).unwrap();
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let columns = connection
            .prepare("SELECT name FROM pragma_table_info('acp_sessions')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for required in [
            "parent_session_id",
            "activity_at",
            "tree_activity_at",
            "state_changed_at",
            "metadata_updated_at",
            "revision",
            "index_version",
            "archived_at",
        ] {
            assert!(
                columns.iter().any(|column| column == required),
                "missing {required}"
            );
        }

        let orphan_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM acp_session_data WHERE session_id = 'orphan-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphan_count, 0);
        let foreign_key_errors = connection
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count();
        assert_eq!(foreign_key_errors, 0);
        assert!(repository.get("legacy-session").unwrap().is_some());

        // A second repository initialization must be a no-op and preserve
        // both the migrated record and the cleaned metadata table.
        drop(repository);
        let repository = SessionRepository::new(&db_path).unwrap();
        assert!(repository.get("legacy-session").unwrap().is_some());
        let metadata_count: i64 = rusqlite::Connection::open(&db_path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM acp_session_data", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(metadata_count, 1);
    }

    #[test]
    fn metadata_foreign_key_migration_rolls_back_when_rebuild_cannot_start() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("migration-rollback.db");
        {
            let connection = rusqlite::Connection::open(&db_path).unwrap();
            connection
                .execute_batch(
                    r#"
                    PRAGMA foreign_keys=OFF;
                    CREATE TABLE acp_sessions (
                        session_id TEXT PRIMARY KEY,
                        thread_id TEXT NOT NULL,
                        owner_principal TEXT NOT NULL,
                        cwd TEXT NOT NULL,
                        lifecycle TEXT NOT NULL DEFAULT 'idle',
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL,
                        closed_at TEXT,
                        title TEXT
                    );
                    CREATE TABLE acp_session_data (
                        session_id TEXT PRIMARY KEY,
                        metadata_json TEXT NOT NULL DEFAULT '{}'
                    );
                    CREATE TABLE acp_session_data_legacy (sentinel TEXT NOT NULL);
                    INSERT INTO acp_sessions
                      (session_id, thread_id, owner_principal, cwd, lifecycle, created_at, updated_at)
                    VALUES ('session-a', 'thread-a', 'owner-a', '/tmp', 'idle',
                            '2026-08-22T00:00:00Z', '2026-08-22T00:00:01Z');
                    INSERT INTO acp_session_data(session_id, metadata_json)
                      VALUES ('session-a', '{"kind":"keep"}');
                    "#,
                )
                .unwrap();
        }

        // The pre-existing destination makes the table-rebuild step fail.
        // SQLite must roll back the rename, leaving the original data usable.
        assert!(SessionRepository::new(&db_path).is_err());
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let table_names = connection
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('acp_session_data', 'acp_session_data_legacy') ORDER BY name",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(table_names, ["acp_session_data", "acp_session_data_legacy"]);
        let session_columns = connection
            .prepare("SELECT name FROM pragma_table_info('acp_sessions')")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for migrated_column in [
            "archived_at",
            "parent_session_id",
            "activity_at",
            "tree_activity_at",
            "revision",
            "index_version",
        ] {
            assert!(
                !session_columns
                    .iter()
                    .any(|column| column == migrated_column),
                "failed migration left partial column {migrated_column}"
            );
        }
        let migrated_table_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('acp_session_index_state', 'acp_session_tombstones')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(migrated_table_count, 0);
        let metadata: String = connection
            .query_row(
                "SELECT metadata_json FROM acp_session_data WHERE session_id = 'session-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(metadata, r#"{"kind":"keep"}"#);

        // Once the conflicting object is removed, the same database can be
        // retried and the normal migration completes.
        connection
            .execute("DROP TABLE acp_session_data_legacy", [])
            .unwrap();
        let repository = SessionRepository::new(&db_path).unwrap();
        assert!(repository.get("session-a").unwrap().is_some());
        let foreign_key_errors = rusqlite::Connection::open(&db_path)
            .unwrap()
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count();
        assert_eq!(foreign_key_errors, 0);
    }

    #[test]
    fn concurrent_schema_initialization_serializes_before_ddl() {
        use std::sync::Arc;
        use std::thread;

        let temp = tempfile::tempdir().unwrap();
        let db_path = Arc::new(temp.path().join("concurrent-migration.db"));
        let workers = (0..8)
            .map(|_| {
                let db_path = Arc::clone(&db_path);
                thread::spawn(move || SessionRepository::new(db_path.as_ref()).map(|_| ()))
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker
                .join()
                .expect("migration worker panicked")
                .expect("concurrent schema initialization failed");
        }
        let repository = SessionRepository::new(db_path.as_ref()).unwrap();
        assert!(repository
            .list_index_for_owner("owner-a", None, "all")
            .unwrap()
            .is_empty());
    }

    #[test]
    fn foreign_key_check_failure_aborts_schema_initialization() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("corrupt-fk.db");
        {
            let connection = rusqlite::Connection::open(&db_path).unwrap();
            connection
                .execute_batch(
                    r#"
                    PRAGMA foreign_keys=OFF;
                    CREATE TABLE acp_sessions (
                        session_id TEXT PRIMARY KEY,
                        thread_id TEXT NOT NULL,
                        owner_principal TEXT NOT NULL,
                        cwd TEXT NOT NULL,
                        lifecycle TEXT NOT NULL DEFAULT 'idle',
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL,
                        closed_at TEXT,
                        title TEXT
                    );
                    CREATE TABLE acp_session_data (
                        session_id TEXT PRIMARY KEY,
                        metadata_json TEXT NOT NULL DEFAULT '{}',
                        FOREIGN KEY(session_id) REFERENCES acp_sessions(session_id) ON DELETE CASCADE
                    );
                    INSERT INTO acp_session_data(session_id, metadata_json)
                      VALUES ('missing-session', '{"kind":"orphan"}');
                    "#,
                )
                .unwrap();
        }

        assert!(SessionRepository::new(&db_path).is_err());
        let connection = rusqlite::Connection::open(&db_path).unwrap();
        let has_archived_column: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info('acp_sessions') WHERE name = 'archived_at')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!has_archived_column);
        let orphan_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM acp_session_data WHERE session_id = 'missing-session'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(orphan_count, 1);
    }

    #[test]
    fn metadata_round_trip_and_delete_are_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("session-a", "thread-a", "owner-a", temp.path())
            .unwrap();
        let metadata = repository.get("session-a").unwrap().unwrap();
        assert_eq!(metadata.owner_principal, "owner-a");
        assert_eq!(metadata.cwd, temp.path());
        assert!(metadata.created_at.is_some());
        assert!(metadata.archived_at.is_none());

        repository.set_lifecycle("session-a", "closed").unwrap();
        assert_eq!(
            repository.get("session-a").unwrap().unwrap().lifecycle,
            "closed"
        );
        assert_eq!(repository.get("session-a").unwrap().unwrap().title, None);
        repository.set_title("session-a", "Fix login bug").unwrap();
        assert_eq!(
            repository.get("session-a").unwrap().unwrap().title,
            Some("Fix login bug".to_string())
        );
        repository
            .delete_all_indexed("session-a", "thread-a")
            .unwrap()
            .expect("first delete");
        assert!(repository
            .delete_all_indexed("session-a", "thread-a")
            .unwrap()
            .is_none());
        assert!(repository.get("session-a").unwrap().is_none());
        let tombstone = repository.get_tombstone("session-a").unwrap().unwrap();
        assert_eq!(tombstone.session_id, "session-a");
    }

    #[test]
    fn archive_round_trip_and_paged_filters() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        for id in ["s1", "s2"] {
            repository
                .insert(id, &format!("t-{id}"), "owner-a", temp.path())
                .unwrap();
        }

        let archived = repository
            .set_archived("s1", "owner-a", true)
            .unwrap()
            .expect("archive target exists");
        assert!(archived.archived_at.is_some());
        // updated_at bumped, so s1 sorts first among archived.
        assert!(archived.updated_at.is_some());

        // Wrong owner cannot archive.
        assert!(repository
            .set_archived("s2", "owner-b", true)
            .unwrap()
            .is_none());

        // Core listing excludes archived.
        let active = repository
            .list_index_for_owner("owner-a", None, "active")
            .unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].session_id, "s2");

        // Canonical SessionIndex listing: archived view only sees s1.
        let archived_rows = repository
            .list_index_for_owner("owner-a", None, "archived")
            .unwrap();
        assert_eq!(archived_rows.len(), 1);
        assert_eq!(archived_rows[0].session_id, "s1");

        // Unarchive restores it to the active view.
        let restored = repository
            .set_archived("s1", "owner-a", false)
            .unwrap()
            .expect("unarchive target exists");
        assert!(restored.archived_at.is_none());
        assert_eq!(
            repository
                .list_index_for_owner("owner-a", None, "active")
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn delete_index_recomputes_ancestors_and_returns_canonical_result() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert_index_record(
                "root",
                "thread-root",
                "owner-a",
                temp.path(),
                None,
                Some("Root"),
                None,
            )
            .unwrap();
        repository
            .insert_index_record(
                "child",
                "thread-child",
                "owner-a",
                temp.path(),
                Some("root"),
                Some("Child"),
                None,
            )
            .unwrap();
        repository
            .insert_index_record(
                "leaf",
                "thread-leaf",
                "owner-a",
                temp.path(),
                Some("child"),
                Some("Leaf"),
                None,
            )
            .unwrap();
        repository.record_activity("leaf").unwrap();
        let before = repository
            .get_index_record("owner-a", "root")
            .unwrap()
            .unwrap();

        let result = repository
            .delete_all_indexed("child", "thread-child")
            .unwrap()
            .expect("delete result");
        assert_eq!(result.tombstone.session_id, "child");
        assert_eq!(result.tombstone.parent_session_id.as_deref(), Some("root"));
        assert_eq!(result.tombstone.index_version, before.index_version + 1);
        assert_eq!(
            result
                .affected_ancestors
                .iter()
                .map(|record| record.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["root"]
        );
        let after = &result.affected_ancestors[0];
        assert!(after.revision > before.revision);
        assert_eq!(after.tree_activity_at, after.activity_at);
        assert!(repository
            .get_index_record("owner-a", "child")
            .unwrap()
            .is_none());
        assert!(repository.get_tombstone("child").unwrap().is_some());
    }

    #[test]
    fn session_index_projection_has_version_and_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("s1", "t1", "owner-a", temp.path())
            .unwrap();
        assert_eq!(repository.owner_index_version("owner-a").unwrap(), 1);
        repository
            .set_metadata_json("s1", "owner-a", r#"{"kind":"review"}"#)
            .unwrap();
        let records = repository
            .list_index_for_owner("owner-a", None, "all")
            .unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].session_id, "s1");
        assert_eq!(records[0].revision, 2);
        assert_eq!(records[0].index_version, 2);
        assert_eq!(records[0].metadata["kind"], "review");
    }

    #[test]
    fn parent_update_recomputes_tree_activity_and_rejects_cycles() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("root", "t-root", "owner-a", temp.path())
            .unwrap();
        repository
            .insert("child", "t-child", "owner-a", temp.path())
            .unwrap();
        let child = repository
            .set_parent("owner-a", "child", Some("root"))
            .unwrap()
            .unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some("root"));
        let root = repository
            .get_index_record("owner-a", "root")
            .unwrap()
            .unwrap();
        assert!(root.tree_activity_at >= root.activity_at);
        assert!(repository
            .set_parent("owner-a", "root", Some("child"))
            .is_err());
    }

    #[test]
    fn activity_updates_target_and_ancestors_once() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("root", "t-root", "owner-a", temp.path())
            .unwrap();
        repository
            .insert("child", "t-child", "owner-a", temp.path())
            .unwrap();
        repository
            .set_parent("owner-a", "child", Some("root"))
            .unwrap();
        let before_root = repository
            .get_index_record("owner-a", "root")
            .unwrap()
            .unwrap();
        let before_child = repository
            .get_index_record("owner-a", "child")
            .unwrap()
            .unwrap();

        let affected = repository.record_activity("child").unwrap();
        assert_eq!(affected.len(), 2);
        let after_root = repository
            .get_index_record("owner-a", "root")
            .unwrap()
            .unwrap();
        let after_child = repository
            .get_index_record("owner-a", "child")
            .unwrap()
            .unwrap();
        assert!(after_child.activity_at > before_child.activity_at);
        assert!(after_root.tree_activity_at >= after_child.activity_at);
        assert!(after_root.index_version > before_root.index_version);
        assert_eq!(after_root.index_version, after_child.index_version);
    }

    #[test]
    fn activity_does_not_propagate_through_archived_parent() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("root", "t-root", "owner-a", temp.path())
            .unwrap();
        repository
            .insert("child", "t-child", "owner-a", temp.path())
            .unwrap();
        repository
            .set_parent("owner-a", "child", Some("root"))
            .unwrap();
        repository.set_archived("root", "owner-a", true).unwrap();
        let before = repository
            .get_index_record("owner-a", "root")
            .unwrap()
            .unwrap();
        repository.record_activity("child").unwrap();
        let after = repository
            .get_index_record("owner-a", "root")
            .unwrap()
            .unwrap();
        assert_eq!(after.index_version, before.index_version);
    }

    #[test]
    fn archive_and_restore_recompute_visible_tree_activity() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("root", "t-root", "owner-a", temp.path())
            .unwrap();
        repository
            .insert("child", "t-child", "owner-a", temp.path())
            .unwrap();
        repository
            .set_parent("owner-a", "child", Some("root"))
            .unwrap();
        repository.record_activity("child").unwrap();
        let before = repository
            .get_index_record("owner-a", "root")
            .unwrap()
            .unwrap();
        repository.set_archived("child", "owner-a", true).unwrap();
        let archived = repository
            .get_index_record("owner-a", "root")
            .unwrap()
            .unwrap();
        assert!(archived.tree_activity_at <= before.tree_activity_at);
        assert!(archived.revision > before.revision);
        repository.set_archived("child", "owner-a", false).unwrap();
        let restored = repository
            .get_index_record("owner-a", "root")
            .unwrap()
            .unwrap();
        assert!(restored.tree_activity_at >= before.tree_activity_at);
    }

    #[test]
    fn archive_index_projection_returns_target_and_ancestors_with_one_version() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("root", "t-root", "owner-a", temp.path())
            .unwrap();
        repository
            .insert("child", "t-child", "owner-a", temp.path())
            .unwrap();
        repository
            .set_parent("owner-a", "child", Some("root"))
            .unwrap();

        let changed = repository
            .set_archived_index_records("child", "owner-a", true)
            .unwrap()
            .expect("target exists");
        assert_eq!(
            changed
                .iter()
                .map(|record| record.session_id.as_str())
                .collect::<Vec<_>>(),
            ["child", "root"]
        );
        assert!(changed
            .iter()
            .all(|record| record.index_version == changed[0].index_version));
        assert!(changed.iter().all(|record| record.revision >= 2));
        assert!(changed
            .iter()
            .find(|record| record.session_id == "child")
            .unwrap()
            .archived_at
            .is_some());
    }

    #[test]
    fn list_index_10k_fixture_supports_repeated_full_reads() {
        use std::time::Instant;

        // A deterministic smoke guard for the design's 10k-session workload.
        // The strict p95/CPU/RAM budget remains an environment benchmark, but
        // this catches accidental query failures or quadratic regressions in
        // the repository path used by both list handlers.
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        {
            let mut connection = repository.connection().unwrap();
            let transaction = connection.transaction().unwrap();
            let metadata = "m".repeat(1024);
            transaction
                .execute(
                    "INSERT INTO acp_session_index_state(owner_principal, current_version) VALUES ('owner-a', 1)",
                    [],
                )
                .unwrap();
            for index in 0..10_000 {
                transaction
                    .execute(
                        "INSERT INTO acp_sessions (session_id, thread_id, owner_principal, cwd, lifecycle, created_at, updated_at, activity_at, tree_activity_at, revision, index_version) VALUES (?1, ?2, 'owner-a', ?3, 'idle', '2026-08-22T00:00:00.000000Z', '2026-08-22T00:00:00.000000Z', '2026-08-22T00:00:00.000000Z', '2026-08-22T00:00:00.000000Z', 1, 1)",
                        params![format!("session-{index}"), format!("thread-{index}"), temp.path().to_string_lossy().as_ref()],
                    )
                    .unwrap();
                transaction
                    .execute(
                        "INSERT INTO acp_session_data (session_id, metadata_json) VALUES (?1, ?2)",
                        params![
                            format!("session-{index}"),
                            format!(r#"{{"payload":"{metadata}"}}"#)
                        ],
                    )
                    .unwrap();
            }
            transaction.commit().unwrap();
        }
        let mut durations = Vec::with_capacity(20);
        for _ in 0..20 {
            let started = Instant::now();
            let records = repository
                .list_index_for_owner("owner-a", None, "all")
                .unwrap();
            assert_eq!(records.len(), 10_000);
            durations.push(started.elapsed());
        }
        durations.sort_unstable();
        let p95 = durations[(durations.len() * 95).div_ceil(100) - 1];
        eprintln!(
            "session index 10k full-read p95: {} ms",
            p95.as_secs_f64() * 1000.0
        );
        if std::env::var_os("ANUREO_ACP_STRICT_PERF").is_some() {
            assert!(
                p95 <= std::time::Duration::from_millis(500),
                "10k full-read p95 exceeded 500ms: {p95:?}"
            );
        }
    }

    #[test]
    fn concurrent_atomic_index_creates_retry_sqlite_busy_and_commit_all() {
        use std::sync::Arc;
        use std::thread;

        let temp = tempfile::tempdir().unwrap();
        let repository = Arc::new(SessionRepository::new(temp.path().join("sessions.db")).unwrap());
        let mut workers = Vec::new();
        for worker in 0..8 {
            let repository = Arc::clone(&repository);
            let cwd = temp.path().to_path_buf();
            workers.push(thread::spawn(move || {
                for index in 0..4 {
                    let session_id = format!("concurrent-{worker}-{index}");
                    repository
                        .insert_index_record(
                            &session_id,
                            &format!("thread-{session_id}"),
                            "owner-a",
                            &cwd,
                            None,
                            Some("Concurrent"),
                            Some(&serde_json::json!({ "worker": worker, "index": index })),
                        )
                        .unwrap_or_else(|error| {
                            panic!("atomic create {session_id} failed: {error}")
                        });
                }
            }));
        }
        for worker in workers {
            worker.join().expect("concurrent worker");
        }

        let records = repository
            .list_index_for_owner("owner-a", None, "active")
            .unwrap();
        assert_eq!(records.len(), 32);
        assert!(records
            .iter()
            .all(|record| record.metadata["worker"].is_number()));
    }

    #[test]
    fn concurrent_activity_updates_serialize_and_commit_all() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        const WORKERS: usize = 8;
        let temp = tempfile::tempdir().unwrap();
        let repository = Arc::new(SessionRepository::new(temp.path().join("sessions.db")).unwrap());
        for worker in 0..WORKERS {
            repository
                .insert(
                    &format!("activity-{worker}"),
                    &format!("thread-{worker}"),
                    "owner-a",
                    temp.path(),
                )
                .unwrap();
        }

        let barrier = Arc::new(Barrier::new(WORKERS));
        let workers = (0..WORKERS)
            .map(|worker| {
                let repository = Arc::clone(&repository);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let session_id = format!("activity-{worker}");
                    repository
                        .record_activity(&session_id)
                        .unwrap_or_else(|error| {
                            panic!("activity update {session_id} failed: {error}")
                        })
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            let records = worker.join().expect("activity worker panicked");
            assert_eq!(records.len(), 1);
        }
        for worker in 0..WORKERS {
            let record = repository
                .get_index_record("owner-a", &format!("activity-{worker}"))
                .unwrap()
                .unwrap();
            assert_eq!(record.revision, 2);
        }

        let connection = repository.connection().unwrap();
        let current_version: i64 = connection
            .query_row(
                "SELECT current_version FROM acp_session_index_state WHERE owner_principal = 'owner-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(current_version, (WORKERS * 2) as i64);
    }

    #[test]
    fn combined_index_update_uses_one_version_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("s1", "t1", "owner-a", temp.path())
            .unwrap();
        let changed = repository
            .update_index_fields("s1", "owner-a", Some("Title"), Some(r#"{"tag":"x"}"#))
            .unwrap()
            .unwrap();
        assert_eq!(changed.revision, 2);
        assert_eq!(changed.index_version, 2);
        let unchanged = repository
            .update_index_fields("s1", "owner-a", Some("Title"), Some(r#"{"tag":"x"}"#))
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.revision, 2);
        assert_eq!(unchanged.index_version, 2);
    }

    #[test]
    fn index_timestamps_use_utc_microsecond_wire_format() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("s1", "t1", "owner-a", temp.path())
            .unwrap();
        let record = repository
            .get_index_record("owner-a", "s1")
            .unwrap()
            .unwrap();
        assert!(record.activity_at.ends_with('Z'));
        assert_eq!(record.activity_at.split('.').nth(1).unwrap().len(), 7);
        assert!(record.activity_at.contains('.'));
    }

    #[test]
    fn legacy_database_without_title_column_is_migrated() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("sessions.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                r#"
                CREATE TABLE acp_sessions (
                    session_id      TEXT PRIMARY KEY,
                    thread_id       TEXT NOT NULL,
                    owner_principal TEXT NOT NULL,
                    cwd             TEXT NOT NULL,
                    lifecycle       TEXT NOT NULL DEFAULT 'idle',
                    created_at      TEXT NOT NULL,
                    updated_at      TEXT NOT NULL,
                    closed_at       TEXT
                );
                INSERT INTO acp_sessions
                    (session_id, thread_id, owner_principal, cwd, lifecycle, created_at, updated_at)
                VALUES
                    ('session-old', 'thread-old', 'owner-a', 'C:\\tmp', 'idle', 't0', 't0');
                "#,
            )
            .unwrap();
        }
        let repository = SessionRepository::new(&db_path).unwrap();
        let metadata = repository.get("session-old").unwrap().unwrap();
        assert_eq!(metadata.title, None);
        assert!(metadata.updated_at.is_some());
        assert!(
            metadata.archived_at.is_none(),
            "legacy db migrates archived_at"
        );
        repository.set_title("session-old", "Migrated").unwrap();
        assert_eq!(
            repository.get("session-old").unwrap().unwrap().title,
            Some("Migrated".to_string())
        );
    }

    #[test]
    fn delete_all_rolls_back_when_owned_table_delete_fails() {
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("session-a", "thread-a", "owner-a", temp.path())
            .unwrap();

        let connection = repository.connection().unwrap();
        connection
            .execute_batch(
                r#"
                CREATE TABLE review_status (
                    session_id TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                INSERT INTO review_status(session_id, value)
                    VALUES ('session-a', 'keep');
                CREATE TRIGGER fail_review_delete
                BEFORE DELETE ON review_status
                BEGIN
                    SELECT RAISE(ABORT, 'injected delete failure');
                END;
                "#,
            )
            .unwrap();

        assert!(repository.delete_all("session-a", "thread-a").is_err());
        assert!(repository.get("session-a").unwrap().is_some());
        assert_eq!(
            connection
                .query_row(
                    "SELECT value FROM review_status WHERE session_id = 'session-a'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "keep"
        );
    }

    #[test]
    fn owner_version_rejects_values_above_json_safe_integer_range() {
        const MAX_JSON_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        let mut connection = repository.connection().unwrap();
        connection
            .execute(
                "INSERT INTO acp_session_index_state(owner_principal, current_version) VALUES (?1, ?2)",
                params!["owner-a", MAX_JSON_SAFE_INTEGER],
            )
            .unwrap();
        let transaction = connection.transaction().unwrap();

        assert!(allocate_owner_version(&transaction, "owner-a").is_err());
    }

    #[test]
    fn session_revision_rejects_values_above_json_safe_integer_range() {
        const MAX_JSON_SAFE_INTEGER: i64 = 9_007_199_254_740_991;
        let temp = tempfile::tempdir().unwrap();
        let repository = SessionRepository::new(temp.path().join("sessions.db")).unwrap();
        repository
            .insert("session-a", "thread-a", "owner-a", temp.path())
            .unwrap();
        let connection = repository.connection().unwrap();
        connection
            .execute(
                "UPDATE acp_sessions SET revision = ?2 WHERE session_id = ?1",
                params!["session-a", MAX_JSON_SAFE_INTEGER],
            )
            .unwrap();
        drop(connection);

        assert!(repository.set_title("session-a", "next").is_err());
    }
}

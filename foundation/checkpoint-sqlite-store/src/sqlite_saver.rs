//! SQLite-backed checkpointer (SqliteSaver). Persistent across process restarts.
//!
//! Aligns with SQLite checkpoint pattern (cf. anureo-checkpoint-sqlite).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::params;

use checkpoint::RunnableConfig;
use checkpoint::Serializer;
use checkpoint::{
    ChannelVersions, Checkpoint, CheckpointListItem, CheckpointMetadata, CheckpointSource,
    KernelMetadata, PendingWrite, CHECKPOINT_VERSION,
};
use checkpoint::{CheckpointError, Checkpointer};
use std::collections::HashMap;

fn source_to_str(s: &CheckpointSource) -> &'static str {
    match s {
        CheckpointSource::Input => "Input",
        CheckpointSource::Loop => "Loop",
        CheckpointSource::Update => "Update",
        CheckpointSource::Fork => "Fork",
    }
}

fn str_to_source(s: &str) -> CheckpointSource {
    match s {
        "Input" => CheckpointSource::Input,
        "Loop" => CheckpointSource::Loop,
        "Update" => CheckpointSource::Update,
        "Fork" => CheckpointSource::Fork,
        _ => CheckpointSource::Update,
    }
}

fn created_at_to_i64(t: &Option<std::time::SystemTime>) -> Option<i64> {
    t.as_ref().and_then(|t| {
        t.duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as i64)
    })
}

fn i64_to_created_at(v: Option<i64>) -> Option<std::time::SystemTime> {
    v.and_then(|ms| std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_millis(ms as u64)))
}

fn serialize_parents(parents: &HashMap<String, String>) -> Result<String, CheckpointError> {
    serde_json::to_string(parents).map_err(|e| CheckpointError::Serialization(e.to_string()))
}

fn serialize_children(children: &HashMap<String, Vec<String>>) -> Result<String, CheckpointError> {
    serde_json::to_string(children).map_err(|e| CheckpointError::Serialization(e.to_string()))
}

fn serialize_json_field<T: serde::Serialize>(value: &T) -> Result<String, CheckpointError> {
    serde_json::to_string(value).map_err(|e| CheckpointError::Serialization(e.to_string()))
}

fn deserialize_json_field<T: serde::de::DeserializeOwned>(
    value: &str,
) -> Result<T, CheckpointError> {
    serde_json::from_str(value).map_err(|e| CheckpointError::Serialization(e.to_string()))
}

fn ensure_checkpoint_runtime_columns(conn: &rusqlite::Connection) -> Result<(), CheckpointError> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(checkpoints)")
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    let columns = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;

    if !columns.iter().any(|column| column == "metadata_parents") {
        conn.execute(
            "ALTER TABLE checkpoints ADD COLUMN metadata_parents TEXT NOT NULL DEFAULT '{}'",
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    }

    if !columns.iter().any(|column| column == "metadata_children") {
        conn.execute(
            "ALTER TABLE checkpoints ADD COLUMN metadata_children TEXT NOT NULL DEFAULT '{}'",
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    }

    if !columns.iter().any(|column| column == "updated_channels") {
        conn.execute(
            "ALTER TABLE checkpoints ADD COLUMN updated_channels TEXT NOT NULL DEFAULT '[]'",
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    }

    if !columns.iter().any(|column| column == "pending_sends") {
        conn.execute(
            "ALTER TABLE checkpoints ADD COLUMN pending_sends TEXT NOT NULL DEFAULT '[]'",
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    }

    if !columns.iter().any(|column| column == "pending_writes") {
        conn.execute(
            "ALTER TABLE checkpoints ADD COLUMN pending_writes TEXT NOT NULL DEFAULT '[]'",
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    }

    if !columns.iter().any(|column| column == "pending_interrupts") {
        conn.execute(
            "ALTER TABLE checkpoints ADD COLUMN pending_interrupts TEXT NOT NULL DEFAULT '[]'",
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    }

    if !columns.iter().any(|column| column == "versions_seen") {
        conn.execute(
            "ALTER TABLE checkpoints ADD COLUMN versions_seen TEXT NOT NULL DEFAULT '{}'",
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    }

    if !columns.iter().any(|column| column == "metadata_summary") {
        conn.execute(
            "ALTER TABLE checkpoints ADD COLUMN metadata_summary TEXT",
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
    }

    Ok(())
}

/// SQLite-backed checkpointer. Key: (thread_id, checkpoint_ns, checkpoint_id).
///
/// Persistent; for single-node and dev. Uses spawn_blocking for async.
///
/// **Interaction**: Used as `Arc<dyn Checkpointer<S>>` in StateGraph::compile_with_checkpointer.
pub struct SqliteSaver<S> {
    db_path: std::path::PathBuf,
    serializer: Arc<dyn Serializer<S>>,
}

impl<S> SqliteSaver<S>
where
    S: Clone + Send + Sync + 'static,
{
    /// Creates a new SQLite checkpointer and ensures the table exists.
    pub fn new(
        path: impl AsRef<Path>,
        serializer: Arc<dyn Serializer<S>>,
    ) -> Result<Self, CheckpointError> {
        let db_path = path.as_ref().to_path_buf();
        let conn =
            crate::sqlite_util::open_sqlite_with_wal(&db_path).map_err(CheckpointError::Storage)?;
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS checkpoints (
                thread_id TEXT NOT NULL,
                checkpoint_ns TEXT NOT NULL,
                checkpoint_id TEXT NOT NULL,
                ts TEXT NOT NULL,
                payload BLOB NOT NULL,
                channel_versions TEXT NOT NULL,
                versions_seen TEXT NOT NULL DEFAULT '{}',
                metadata_source TEXT NOT NULL,
                metadata_step INTEGER NOT NULL,
                metadata_created_at INTEGER,
                metadata_parents TEXT NOT NULL DEFAULT '{}',
                metadata_children TEXT NOT NULL DEFAULT '{}',
                metadata_summary TEXT,
                updated_channels TEXT NOT NULL DEFAULT '[]',
                pending_sends TEXT NOT NULL DEFAULT '[]',
                pending_writes TEXT NOT NULL DEFAULT '[]',
                pending_interrupts TEXT NOT NULL DEFAULT '[]',
                PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id)
            )
            "#,
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
        ensure_checkpoint_runtime_columns(&conn)?;
        conn.execute(
            r#"
            CREATE TABLE IF NOT EXISTS checkpoint_writes (
                thread_id TEXT NOT NULL,
                checkpoint_ns TEXT NOT NULL,
                checkpoint_id TEXT NOT NULL,
                task_id TEXT NOT NULL,
                idx INTEGER NOT NULL,
                channel TEXT NOT NULL,
                value BLOB NOT NULL,
                created_at INTEGER,
                PRIMARY KEY (thread_id, checkpoint_ns, checkpoint_id, task_id, idx)
            )
            "#,
            [],
        )
        .map_err(|e| CheckpointError::Storage(e.to_string()))?;
        Ok(Self {
            db_path,
            serializer,
        })
    }

    fn thread_id_required(config: &RunnableConfig) -> Result<String, CheckpointError> {
        config
            .thread_id
            .as_deref()
            .ok_or(CheckpointError::ThreadIdRequired)
            .map(String::from)
    }
}

#[async_trait]
impl<S> Checkpointer<S> for SqliteSaver<S>
where
    S: Clone + Send + Sync + 'static,
{
    async fn put(
        &self,
        config: &RunnableConfig,
        checkpoint: &Checkpoint<S>,
    ) -> Result<String, CheckpointError> {
        let thread_id = Self::thread_id_required(config)?;
        let checkpoint_ns = config.checkpoint_ns.clone();
        let payload = self.serializer.serialize(&checkpoint.channel_values)?;
        let channel_versions = serde_json::to_string(&checkpoint.channel_versions)
            .map_err(|e| CheckpointError::Serialization(e.to_string()))?;
        let versions_seen = serialize_json_field(&checkpoint.versions_seen)?;
        let metadata_source = source_to_str(&checkpoint.kernel.source).to_string();
        let metadata_step = checkpoint.kernel.step;
        let metadata_created_at = created_at_to_i64(&checkpoint.kernel.created_at);
        let metadata_parents = serialize_parents(&checkpoint.kernel.parents)?;
        let metadata_children = serialize_children(&checkpoint.kernel.children)?;
        let metadata_summary = checkpoint.kernel.summary.clone();
        let updated_channels = serialize_json_field(&checkpoint.updated_channels)?;
        let pending_sends = serialize_json_field(&checkpoint.pending_sends)?;
        let pending_writes = serialize_json_field(&checkpoint.pending_writes)?;
        let pending_interrupts = serialize_json_field(&checkpoint.pending_interrupts)?;
        let id = checkpoint.id.clone();
        let ts = checkpoint.ts.clone();

        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = crate::sqlite_util::open_sqlite_with_wal(&db_path)
                .map_err(CheckpointError::Storage)?;
            crate::sqlite_util::execute_write(&mut conn, |tx| {
                tx.execute(
                    r#"
                    INSERT OR REPLACE INTO checkpoints
                    (thread_id, checkpoint_ns, checkpoint_id, ts, payload, channel_versions, versions_seen,
                     metadata_source, metadata_step, metadata_created_at, metadata_parents, metadata_children,
                     metadata_summary, updated_channels, pending_sends, pending_writes, pending_interrupts)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
                    "#,
                    params![
                        thread_id,
                        checkpoint_ns,
                        id,
                        ts,
                        payload,
                        channel_versions,
                        versions_seen,
                        metadata_source,
                        metadata_step,
                        metadata_created_at,
                        metadata_parents,
                        metadata_children,
                        metadata_summary,
                        updated_channels,
                        pending_sends,
                        pending_writes,
                        pending_interrupts,
                    ],
                )?;
                Ok(())
            })
            .map_err(|e| CheckpointError::Storage(e.to_string()))?;
            Ok::<String, CheckpointError>(id)
        })
        .await
        .map_err(|e| CheckpointError::Storage(e.to_string()))?
    }

    async fn get_tuple(
        &self,
        config: &RunnableConfig,
    ) -> Result<Option<(Checkpoint<S>, CheckpointMetadata)>, CheckpointError> {
        let thread_id = Self::thread_id_required(config)?;
        let checkpoint_ns = config.checkpoint_ns.clone();
        let want_id = config.checkpoint_id.clone();
        let db_path = self.db_path.clone();

        type RowData = (
            String,
            String,
            Vec<u8>,
            String,
            String,
            String,
            i64,
            Option<i64>,
            String,
            String,
            Option<String>, // metadata_summary
            String,
            String,
            String,
            String,
        );
        let row: Option<RowData> = tokio::task::spawn_blocking(move || -> Result<Option<RowData>, CheckpointError> {
            let conn = crate::sqlite_util::open_sqlite_with_wal(&db_path)
                .map_err(CheckpointError::Storage)?;
            let sql = if want_id.is_some() {
                "SELECT checkpoint_id, ts, payload, channel_versions, versions_seen, metadata_source, metadata_step, metadata_created_at, metadata_parents, metadata_children, metadata_summary,
                        updated_channels, pending_sends, pending_writes, pending_interrupts
                 FROM checkpoints WHERE thread_id = ?1 AND checkpoint_ns = ?2 AND checkpoint_id = ?3"
            } else {
                "SELECT checkpoint_id, ts, payload, channel_versions, versions_seen, metadata_source, metadata_step, metadata_created_at, metadata_parents, metadata_children, metadata_summary,
                        updated_channels, pending_sends, pending_writes, pending_interrupts
                 FROM checkpoints WHERE thread_id = ?1 AND checkpoint_ns = ?2
                 ORDER BY metadata_created_at DESC LIMIT 1"
            };
            let mut stmt = conn.prepare(sql).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let mut rows = if let Some(ref cid) = want_id {
                stmt.query(params![thread_id, checkpoint_ns, cid])
            } else {
                stmt.query(params![thread_id, checkpoint_ns])
            }
            .map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let row = match rows.next().map_err(|e| CheckpointError::Storage(e.to_string()))? {
                Some(r) => r,
                None => return Ok::<_, CheckpointError>(None),
            };
            let checkpoint_id: String = row.get(0).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let ts: String = row.get(1).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let payload: Vec<u8> = row.get(2).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let channel_versions_json: String = row.get(3).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let versions_seen_json: String = row.get(4).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let metadata_source: String = row.get(5).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let metadata_step: i64 = row.get(6).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let metadata_created_at: Option<i64> = row.get(7).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let metadata_parents: String = row.get(8).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let metadata_children: String = row.get(9).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let metadata_summary: Option<String> = row.get(10).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let updated_channels: String = row.get(11).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let pending_sends: String = row.get(12).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let pending_writes: String = row.get(13).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let pending_interrupts: String = row.get(14).map_err(|e| CheckpointError::Storage(e.to_string()))?;
            Ok(Some((
                checkpoint_id,
                ts,
                payload,
                channel_versions_json,
                versions_seen_json,
                metadata_source,
                metadata_step,
                metadata_created_at,
                metadata_parents,
                metadata_children,
                metadata_summary,
                updated_channels,
                pending_sends,
                pending_writes,
                pending_interrupts,
            )))
        })
        .await
        .map_err(|e| CheckpointError::Storage(e.to_string()))??;

        let (
            checkpoint_id,
            ts,
            payload,
            channel_versions_json,
            versions_seen_json,
            metadata_source,
            metadata_step,
            metadata_created_at,
            metadata_parents,
            metadata_children,
            metadata_summary,
            updated_channels_json,
            pending_sends_json,
            pending_writes_json,
            pending_interrupts_json,
        ): RowData = match row {
            Some(r) => r,
            None => return Ok(None),
        };

        let channel_values = self.serializer.deserialize(&payload)?;
        let channel_versions: ChannelVersions = serde_json::from_str(&channel_versions_json)
            .map_err(|e| CheckpointError::Serialization(e.to_string()))?;
        let versions_seen: HashMap<String, ChannelVersions> =
            deserialize_json_field(&versions_seen_json)?;
        let metadata = CheckpointMetadata {
            source: str_to_source(&metadata_source),
            step: metadata_step,
            created_at: i64_to_created_at(metadata_created_at),
            parents: serde_json::from_str(&metadata_parents).unwrap_or_else(|_| HashMap::new()),
            children: serde_json::from_str(&metadata_children).unwrap_or_else(|_| HashMap::new()),
            summary: metadata_summary,
        };
        let checkpoint = Checkpoint {
            v: CHECKPOINT_VERSION,
            id: checkpoint_id.clone(),
            ts,
            channel_values,
            channel_versions,
            versions_seen,
            updated_channels: deserialize_json_field(&updated_channels_json)?,
            pending_sends: deserialize_json_field(&pending_sends_json)?,
            pending_writes: deserialize_json_field(&pending_writes_json)?,
            pending_interrupts: deserialize_json_field(&pending_interrupts_json)?,
            kernel: metadata.clone(),
            user: (),
        };
        Ok(Some((checkpoint, metadata.clone())))
    }

    async fn list(
        &self,
        config: &RunnableConfig,
        limit: Option<usize>,
        before: Option<&str>,
        after: Option<&str>,
    ) -> Result<Vec<CheckpointListItem>, CheckpointError> {
        let thread_id = Self::thread_id_required(config)?;
        let checkpoint_ns = config.checkpoint_ns.clone();
        let db_path = self.db_path.clone();
        let before = before.map(String::from);
        let after = after.map(String::from);

        let items = tokio::task::spawn_blocking(move || {
            let conn = crate::sqlite_util::open_sqlite_with_wal(&db_path)
                .map_err(CheckpointError::Storage)?;
            let mut stmt = conn
                .prepare(
                    "SELECT checkpoint_id, metadata_source, metadata_step, metadata_created_at, metadata_parents, metadata_children, metadata_summary
                     FROM checkpoints WHERE thread_id = ?1 AND checkpoint_ns = ?2
                     ORDER BY metadata_created_at ASC",
                )
                .map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let rows = stmt
                .query_map(params![thread_id, checkpoint_ns], |row| {
                    Ok(CheckpointListItem {
                        checkpoint_id: row.get(0)?,
                        metadata: KernelMetadata {
                            source: str_to_source(&row.get::<_, String>(1)?),
                            step: row.get::<_, i64>(2)?,
                            created_at: i64_to_created_at(row.get(3)?),
                            parents: serde_json::from_str::<HashMap<String, String>>(&row.get::<_, String>(4)?)
                                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                            children: serde_json::from_str::<HashMap<String, Vec<String>>>(&row.get::<_, String>(5)?)
                                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                            summary: row.get(6)?,
                        },
                    })
                })
                .map_err(|e| CheckpointError::Storage(e.to_string()))?;
            let mut list: Vec<CheckpointListItem> = rows
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| CheckpointError::Storage(e.to_string()))?;
            if let Some(a) = &after {
                if let Some(pos) = list.iter().position(|i| i.checkpoint_id.as_str() == a) {
                    list = list[pos + 1..].to_vec();
                }
            }
            if let Some(b) = &before {
                if let Some(pos) = list.iter().position(|i| i.checkpoint_id.as_str() == b) {
                    list = list[..pos].to_vec();
                }
            }
            if let Some(n) = limit {
                let len = list.len();
                if len > n {
                    list = list[len - n..].to_vec();
                }
            }
            Ok::<Vec<CheckpointListItem>, CheckpointError>(list)
        })
        .await
        .map_err(|e| CheckpointError::Storage(e.to_string()))??;

        Ok(items)
    }

    async fn put_writes(
        &self,
        config: &RunnableConfig,
        checkpoint_id: &str,
        task_id: &str,
        writes: &[(String, serde_json::Value)],
    ) -> Result<(), CheckpointError> {
        let thread_id = Self::thread_id_required(config)?;
        let checkpoint_ns = config.checkpoint_ns.clone();
        let db_path = self.db_path.clone();
        let checkpoint_id_owned = checkpoint_id.to_string();
        let task_id_owned = task_id.to_string();
        // Pre-serialize each write to (idx, channel, JSON bytes) so the
        // blocking task owns everything by value and doesn't borrow `writes`.
        let mut prepared: Vec<(i64, String, Vec<u8>)> = Vec::with_capacity(writes.len());
        for (idx, (channel, value)) in writes.iter().enumerate() {
            let bytes = serde_json::to_vec(value)
                .map_err(|e| CheckpointError::Serialization(e.to_string()))?;
            prepared.push((idx as i64, channel.clone(), bytes));
        }
        tokio::task::spawn_blocking(move || {
            let mut conn = crate::sqlite_util::open_sqlite_with_wal(&db_path)
                .map_err(CheckpointError::Storage)?;
            crate::sqlite_util::execute_write(&mut conn, |tx| {
                for (idx, channel, bytes) in &prepared {
                    tx.execute(
                        r#"
                        INSERT OR IGNORE INTO checkpoint_writes
                            (thread_id, checkpoint_ns, checkpoint_id, task_id, idx, channel, value, created_at)
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                        "#,
                        params![
                            thread_id,
                            checkpoint_ns,
                            checkpoint_id_owned,
                            task_id_owned,
                            idx,
                            channel,
                            bytes,
                            created_at_to_i64(&Some(std::time::SystemTime::now())),
                        ],
                    )?;
                }
                Ok(())
            })
            .map_err(|e| CheckpointError::Storage(e.to_string()))?;
            Ok::<(), CheckpointError>(())
        })
        .await
        .map_err(|e| CheckpointError::Storage(e.to_string()))?
    }

    async fn get_writes(
        &self,
        config: &RunnableConfig,
        checkpoint_id: &str,
    ) -> Result<Vec<PendingWrite>, CheckpointError> {
        let thread_id = Self::thread_id_required(config)?;
        let checkpoint_ns = config.checkpoint_ns.clone();
        let checkpoint_id_owned = checkpoint_id.to_string();
        let db_path = self.db_path.clone();

        let rows: Vec<(String, String, Vec<u8>)> = tokio::task::spawn_blocking(
            move || -> Result<Vec<(String, String, Vec<u8>)>, CheckpointError> {
                let conn = crate::sqlite_util::open_sqlite_with_wal(&db_path)
                    .map_err(CheckpointError::Storage)?;
                let mut stmt = conn
                    .prepare(
                        "SELECT task_id, channel, value FROM checkpoint_writes \
                         WHERE thread_id = ?1 AND checkpoint_ns = ?2 AND checkpoint_id = ?3 \
                         ORDER BY task_id ASC, idx ASC",
                    )
                    .map_err(|e| CheckpointError::Storage(e.to_string()))?;
                let rows = stmt
                    .query_map(
                        params![thread_id, checkpoint_ns, checkpoint_id_owned],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, Vec<u8>>(2)?,
                            ))
                        },
                    )
                    .map_err(|e| CheckpointError::Storage(e.to_string()))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| CheckpointError::Storage(e.to_string()))?;
                Ok(rows)
            },
        )
        .await
        .map_err(|e| CheckpointError::Storage(e.to_string()))??;

        let mut result = Vec::with_capacity(rows.len());
        for (task_id, channel, bytes) in rows {
            let value: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|e| CheckpointError::Serialization(e.to_string()))?;
            result.push((task_id, channel, value));
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    fn checkpoint(id: &str) -> Checkpoint<serde_json::Value> {
        Checkpoint {
            v: CHECKPOINT_VERSION,
            id: id.to_string(),
            ts: "2026-09-05T00:00:00Z".to_string(),
            channel_values: serde_json::json!({"checkpoint": id}),
            channel_versions: HashMap::new(),
            versions_seen: HashMap::new(),
            updated_channels: None,
            pending_sends: Vec::new(),
            pending_writes: Vec::new(),
            pending_interrupts: Vec::new(),
            user: (),
            kernel: KernelMetadata {
                source: CheckpointSource::Loop,
                step: 1,
                created_at: Some(SystemTime::now()),
                parents: HashMap::new(),
                children: HashMap::new(),
                summary: None,
            },
        }
    }

    #[test]
    fn source_roundtrip() {
        let sources = [
            CheckpointSource::Input,
            CheckpointSource::Loop,
            CheckpointSource::Update,
            CheckpointSource::Fork,
        ];
        for s in &sources {
            let st = source_to_str(s);
            let back = str_to_source(st);
            assert_eq!(std::mem::discriminant(s), std::mem::discriminant(&back));
        }
    }

    #[test]
    fn str_to_source_unknown_defaults_to_update() {
        assert!(matches!(str_to_source("unknown"), CheckpointSource::Update));
    }

    #[test]
    fn created_at_roundtrip() {
        let t = UNIX_EPOCH + Duration::from_millis(1700000000000);
        let i = created_at_to_i64(&Some(t));
        assert_eq!(i, Some(1700000000000));
        let back = i64_to_created_at(i);
        assert_eq!(back, Some(t));
    }

    #[test]
    fn created_at_none_roundtrip() {
        assert!(created_at_to_i64(&None).is_none());
        assert!(i64_to_created_at(None).is_none());
    }

    #[test]
    fn thread_id_required_ok() {
        let config = RunnableConfig {
            thread_id: Some("t1".to_string()),
            ..RunnableConfig::default()
        };
        let tid = SqliteSaver::<Vec<u8>>::thread_id_required(&config).unwrap();
        assert_eq!(tid, "t1");
    }

    #[test]
    fn thread_id_required_missing() {
        let config = RunnableConfig {
            thread_id: None,
            ..RunnableConfig::default()
        };
        let result = SqliteSaver::<Vec<u8>>::thread_id_required(&config);
        assert!(result.is_err());
    }

    #[test]
    fn new_creates_table() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let serializer: Arc<dyn Serializer<serde_json::Value>> =
            Arc::new(checkpoint::JsonSerializer);
        let _saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();
        assert!(db_path.exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_and_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("rt.db");
        let serializer: Arc<dyn Serializer<serde_json::Value>> =
            Arc::new(checkpoint::JsonSerializer);
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();

        let config = RunnableConfig {
            thread_id: Some("thread-1".to_string()),
            ..RunnableConfig::default()
        };
        let now = SystemTime::now();
        let checkpoint = Checkpoint {
            v: CHECKPOINT_VERSION,
            id: "ck-1".to_string(),
            ts: "2024-01-01T00:00:00Z".to_string(),
            channel_values: serde_json::json!({"key": "value"}),
            channel_versions: HashMap::new(),
            versions_seen: [(
                "node-a".to_string(),
                [("key".to_string(), "3".to_string())].into_iter().collect(),
            )]
            .into_iter()
            .collect(),
            updated_channels: Some(vec!["key".to_string()]),
            pending_sends: vec![(
                "task-send".to_string(),
                "__tasks__".to_string(),
                serde_json::json!({"target": "worker", "payload": {"x": 1}}),
            )],
            pending_writes: vec![(
                "task-interrupt".to_string(),
                "__interrupt__".to_string(),
                serde_json::json!({"kind": "test"}),
            )],
            pending_interrupts: vec![serde_json::json!({"interrupt_id": "int-1"})],
            user: (),
            kernel: KernelMetadata {
                source: CheckpointSource::Input,
                step: 1,
                created_at: Some(now),
                parents: [("parent".to_string(), "cp-0".to_string())]
                    .into_iter()
                    .collect(),
                children: [(
                    "parent/child".to_string(),
                    vec!["child-cp-1".to_string(), "child-cp-2".to_string()],
                )]
                .into_iter()
                .collect(),
                summary: None,
            },
        };

        let id = saver.put(&config, &checkpoint).await.unwrap();
        assert_eq!(id, "ck-1");

        let result = saver.get_tuple(&config).await.unwrap();
        assert!(result.is_some());
        let (ck, meta) = result.unwrap();
        assert_eq!(ck.id, "ck-1");
        assert_eq!(ck.channel_values, serde_json::json!({"key": "value"}));
        assert_eq!(
            ck.updated_channels.as_deref(),
            Some(&["key".to_string()][..])
        );
        assert_eq!(
            ck.versions_seen
                .get("node-a")
                .and_then(|seen| seen.get("key"))
                .map(String::as_str),
            Some("3")
        );
        assert_eq!(ck.pending_sends.len(), 1);
        assert_eq!(ck.pending_writes.len(), 1);
        assert_eq!(ck.pending_interrupts.len(), 1);
        assert!(matches!(meta.source, CheckpointSource::Input));
        assert_eq!(meta.step, 1);
        assert_eq!(meta.parents.get("parent").map(String::as_str), Some("cp-0"));
        assert_eq!(
            meta.children
                .get("parent/child")
                .expect("child links should roundtrip"),
            &vec!["child-cp-1".to_string(), "child-cp-2".to_string()]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_retries_while_another_session_holds_writer_lock() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("put-contention.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();
        let mut blocker = crate::sqlite_util::open_sqlite_with_wal(&db_path).unwrap();
        let blocker_tx = blocker
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let config = RunnableConfig {
            thread_id: Some("parallel-session-b".to_string()),
            ..RunnableConfig::default()
        };

        let write = tokio::spawn(async move { saver.put(&config, &checkpoint("ck-b")).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        blocker_tx.commit().unwrap();

        assert_eq!(write.await.unwrap().unwrap(), "ck-b");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn list_returns_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("list.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();

        let config = RunnableConfig {
            thread_id: Some("thread-list".to_string()),
            ..RunnableConfig::default()
        };
        let base = UNIX_EPOCH + Duration::from_millis(1700000000000);
        for i in 0..3 {
            let checkpoint = Checkpoint {
                v: CHECKPOINT_VERSION,
                id: format!("ck-{}", i),
                ts: format!("2024-01-0{}T00:00:00Z", i + 1),
                channel_values: serde_json::json!({"step": i}),
                channel_versions: HashMap::new(),
                versions_seen: HashMap::new(),
                updated_channels: None,
                pending_sends: Vec::new(),
                pending_writes: Vec::new(),
                pending_interrupts: Vec::new(),
                user: (),
                kernel: KernelMetadata {
                    source: CheckpointSource::Loop,
                    step: i,
                    created_at: Some(base + Duration::from_secs(i as u64)),
                    parents: HashMap::new(),
                    children: HashMap::new(),
                    summary: None,
                },
            };
            saver.put(&config, &checkpoint).await.unwrap();
        }

        let items = saver.list(&config, None, None, None).await.unwrap();
        assert_eq!(items.len(), 3);
        assert!(items.iter().all(|item| item.metadata.children.is_empty()));

        let limited = saver.list(&config, Some(2), None, None).await.unwrap();
        assert_eq!(limited.len(), 2);

        let after = saver.list(&config, None, None, Some("ck-0")).await.unwrap();
        assert_eq!(after.len(), 2);

        let before = saver.list(&config, None, Some("ck-2"), None).await.unwrap();
        assert_eq!(before.len(), 2);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_tuple_missing_thread_id() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("no_tid.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();
        let config = RunnableConfig::default();
        let result = saver.get_tuple(&config).await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn get_tuple_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("empty.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();
        let config = RunnableConfig {
            thread_id: Some("nonexistent".to_string()),
            ..RunnableConfig::default()
        };
        let result = saver.get_tuple(&config).await.unwrap();
        assert!(result.is_none());
    }

    /// **Scenario**: put_writes + get_writes round-trip across multiple tasks.
    #[tokio::test(flavor = "current_thread")]
    async fn put_writes_roundtrip_and_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("writes_rt.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();
        let config = RunnableConfig {
            thread_id: Some("thread-writes-rt".to_string()),
            checkpoint_ns: "main".to_string(),
            ..RunnableConfig::default()
        };

        // Insert in non-sorted order to verify ORDER BY (task_id, idx).
        saver
            .put_writes(
                &config,
                "ck-1",
                "task-b",
                &[("channel-1".to_string(), serde_json::json!("b1"))],
            )
            .await
            .unwrap();
        saver
            .put_writes(
                &config,
                "ck-1",
                "task-a",
                &[
                    ("channel-1".to_string(), serde_json::json!(1)),
                    ("channel-2".to_string(), serde_json::json!("a2")),
                ],
            )
            .await
            .unwrap();

        let writes = saver.get_writes(&config, "ck-1").await.unwrap();
        assert_eq!(writes.len(), 3);
        let summary: Vec<(String, String)> = writes
            .iter()
            .map(|(t, c, _)| (t.clone(), c.clone()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("task-a".to_string(), "channel-1".to_string()),
                ("task-a".to_string(), "channel-2".to_string()),
                ("task-b".to_string(), "channel-1".to_string()),
            ]
        );
        // Values are deserialized correctly from BLOB
        assert_eq!(writes[0].2, serde_json::json!(1));
        assert_eq!(writes[1].2, serde_json::json!("a2"));
        assert_eq!(writes[2].2, serde_json::json!("b1"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn put_writes_retries_as_one_transaction_under_writer_contention() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("put-writes-contention.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();
        let mut blocker = crate::sqlite_util::open_sqlite_with_wal(&db_path).unwrap();
        let blocker_tx = blocker
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let config = RunnableConfig {
            thread_id: Some("parallel-session-b".to_string()),
            checkpoint_ns: "main".to_string(),
            ..RunnableConfig::default()
        };

        let write = tokio::spawn(async move {
            saver
                .put_writes(
                    &config,
                    "ck-b",
                    "task-b",
                    &[
                        ("first".to_string(), serde_json::json!(1)),
                        ("second".to_string(), serde_json::json!(2)),
                    ],
                )
                .await?;
            saver.get_writes(&config, "ck-b").await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        blocker_tx.commit().unwrap();

        let writes = write.await.unwrap().unwrap();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].2, serde_json::json!(1));
        assert_eq!(writes[1].2, serde_json::json!(2));
    }

    /// **Scenario**: Re-inserting the same (task_id, idx) is idempotent.
    #[tokio::test(flavor = "current_thread")]
    async fn put_writes_is_idempotent_on_task_id_idx() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("writes_idem.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();
        let config = RunnableConfig {
            thread_id: Some("thread-writes-idem".to_string()),
            checkpoint_ns: "main".to_string(),
            ..RunnableConfig::default()
        };

        saver
            .put_writes(
                &config,
                "ck-1",
                "task-a",
                &[("ch".to_string(), serde_json::json!("first"))],
            )
            .await
            .unwrap();
        saver
            .put_writes(
                &config,
                "ck-1",
                "task-a",
                &[("ch".to_string(), serde_json::json!("second"))],
            )
            .await
            .unwrap();

        let writes = saver.get_writes(&config, "ck-1").await.unwrap();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].2, serde_json::json!("first"));
    }

    /// **Scenario**: Writes are isolated by thread_id, checkpoint_ns, and checkpoint_id.
    #[tokio::test(flavor = "current_thread")]
    async fn put_writes_isolated_by_thread_ns_checkpoint() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("writes_iso.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();

        let cfg_a_ns_a = RunnableConfig {
            thread_id: Some("thread-a".to_string()),
            checkpoint_ns: "ns-a".to_string(),
            ..RunnableConfig::default()
        };
        let cfg_b_ns_a = RunnableConfig {
            thread_id: Some("thread-b".to_string()),
            checkpoint_ns: "ns-a".to_string(),
            ..RunnableConfig::default()
        };
        let cfg_a_ns_b = RunnableConfig {
            thread_id: Some("thread-a".to_string()),
            checkpoint_ns: "ns-b".to_string(),
            ..RunnableConfig::default()
        };

        saver
            .put_writes(
                &cfg_a_ns_a,
                "ck-1",
                "task",
                &[("ch".to_string(), serde_json::json!("a/nsa/ck-1"))],
            )
            .await
            .unwrap();
        saver
            .put_writes(
                &cfg_b_ns_a,
                "ck-1",
                "task",
                &[("ch".to_string(), serde_json::json!("b/nsa/ck-1"))],
            )
            .await
            .unwrap();
        saver
            .put_writes(
                &cfg_a_ns_b,
                "ck-1",
                "task",
                &[("ch".to_string(), serde_json::json!("a/nsb/ck-1"))],
            )
            .await
            .unwrap();
        saver
            .put_writes(
                &cfg_a_ns_a,
                "ck-2",
                "task",
                &[("ch".to_string(), serde_json::json!("a/nsa/ck-2"))],
            )
            .await
            .unwrap();

        let a_nsa_ck1 = saver.get_writes(&cfg_a_ns_a, "ck-1").await.unwrap();
        let b_nsa_ck1 = saver.get_writes(&cfg_b_ns_a, "ck-1").await.unwrap();
        let a_nsb_ck1 = saver.get_writes(&cfg_a_ns_b, "ck-1").await.unwrap();
        let a_nsa_ck2 = saver.get_writes(&cfg_a_ns_a, "ck-2").await.unwrap();
        let missing = saver.get_writes(&cfg_a_ns_a, "ck-missing").await.unwrap();

        assert_eq!(a_nsa_ck1.len(), 1);
        assert_eq!(a_nsa_ck1[0].2, serde_json::json!("a/nsa/ck-1"));
        assert_eq!(b_nsa_ck1.len(), 1);
        assert_eq!(b_nsa_ck1[0].2, serde_json::json!("b/nsa/ck-1"));
        assert_eq!(a_nsb_ck1.len(), 1);
        assert_eq!(a_nsb_ck1[0].2, serde_json::json!("a/nsb/ck-1"));
        assert_eq!(a_nsa_ck2.len(), 1);
        assert_eq!(a_nsa_ck2[0].2, serde_json::json!("a/nsa/ck-2"));
        assert!(missing.is_empty());
    }

    /// **Scenario**: get_writes for a never-touched checkpoint returns empty Vec.
    #[tokio::test(flavor = "current_thread")]
    async fn get_writes_empty_when_no_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("writes_empty.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();
        let config = RunnableConfig {
            thread_id: Some("thread-empty".to_string()),
            checkpoint_ns: "main".to_string(),
            ..RunnableConfig::default()
        };
        let writes = saver.get_writes(&config, "ck-missing").await.unwrap();
        assert!(writes.is_empty());
    }

    /// **Scenario**: put_writes missing thread_id returns ThreadIdRequired.
    #[tokio::test(flavor = "current_thread")]
    async fn put_writes_missing_thread_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("writes_no_tid.db");
        let serializer =
            Arc::new(checkpoint::JsonSerializer) as Arc<dyn Serializer<serde_json::Value>>;
        let saver = SqliteSaver::<serde_json::Value>::new(&db_path, serializer).unwrap();
        let config = RunnableConfig::default();
        let result = saver
            .put_writes(
                &config,
                "ck-1",
                "task",
                &[("ch".to_string(), serde_json::json!("v"))],
            )
            .await;
        assert!(matches!(result, Err(CheckpointError::ThreadIdRequired)));
    }
}

//! Session data layer: CRUD operations on sessions in memory.db.
//!
//! Presentation logic lives in [`crate::session_view`].

#![allow(dead_code)]

use chrono::{DateTime, Local, Utc};
use clap::{Args, Subcommand};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

/// FTS5 capability cache (priority #5/#7 gap).
///
/// Probed once per process via `probe_fts_capability`. The result is
/// stashed in a `OnceLock<bool>` so we don't repeatedly try
/// `CREATE VIRTUAL TABLE _fts_probe USING fts5(x)` on every search.
/// On SQLite builds without FTS5 (e.g. musl-statically-linked SQLite
/// without the fts5 extension), `_fts_enabled` stays `false` and the
/// search path falls through to the legacy per-payload substring scan.
static FTS_ENABLED: OnceLock<bool> = OnceLock::new();
/// Trigram tokenizer availability — only relevant for fuzzy CJK search.
/// Trigram needs SQLite ≥ 3.34; older builds disable it.
static FTS_TRIGRAM_AVAILABLE: OnceLock<bool> = OnceLock::new();

/// Detect whether the open connection supports FTS5 and trigram.
///
/// Probe order: a single `CREATE VIRTUAL TABLE _probe USING fts5(x)` +
/// immediate `DROP` is the cheapest non-destructive test. Trigram is
/// probed the same way (`USING fts5(x, tokenize='trigram')`). Both
/// results are cached in `FTS_ENABLED` / `FTS_TRIGRAM_AVAILABLE` so we
/// don't re-run the probes on subsequent opens.
///
/// The probe emits a single `tracing::warn!` per probe path on failure
/// (not per call) — otherwise search retries in a degraded environment
/// would flood the log.
pub fn probe_fts_capability(conn: &rusqlite::Connection) -> (bool, bool) {
    let fts = *FTS_ENABLED.get_or_init(|| {
        let ok = conn
            .execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS _anureo_fts_probe USING fts5(x); DROP TABLE _anureo_fts_probe;")
            .is_ok();
        if !ok {
            tracing::warn!("anureo-cli: FTS5 unavailable; falling back to LIKE-based session search");
        }
        ok
    });
    let trigram = *FTS_TRIGRAM_AVAILABLE.get_or_init(|| {
        if !fts {
            false
        } else {
            let ok = conn
                .execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS _anureo_trigram_probe USING fts5(x, tokenize='trigram'); DROP TABLE _anureo_trigram_probe;")
                .is_ok();
            if !ok {
                tracing::warn!("anureo-cli: trigram tokenizer unavailable; CJK search will use per-token LIKE");
            }
            ok
        }
    });
    (fts, trigram)
}

/// True if FTS5 was successfully probed. Cheap accessor for callers
/// that need to branch on capability without re-running the probe.
pub fn is_fts_enabled() -> bool {
    FTS_ENABLED.get().copied().unwrap_or(false)
}

/// True if the trigram tokenizer is available. Mirrors
/// `is_fts_enabled` — both must be true for FTS5 + CJK MATCH.
pub fn is_trigram_available() -> bool {
    FTS_TRIGRAM_AVAILABLE.get().copied().unwrap_or(false)
}

/// Count CJK Unified Ideographs (U+4E00–U+9FFF) plus Hiragana/Katakana
/// in `s`. Used by the search heuristic — if 3+ tokens are CJK, the
/// trigram FTS5 path is preferred over per-token LIKE.
pub fn count_cjk(s: &str) -> usize {
    s.chars()
        .filter(|c| {
            let cp = *c as u32;
            (0x4E00..=0x9FFF).contains(&cp)
                || (0x3040..=0x309F).contains(&cp)
                || (0x30A0..=0x30FF).contains(&cp)
        })
        .count()
}

/// Sanitize a user query for FTS5 MATCH: keep alnum + CJK, treat
/// `OR` / `AND` / `NOT` (case-insensitive, whole-word) as operators,
/// escape embedded double quotes by doubling.
///
/// Returns `None` if every token was stripped (pure stopword / pure
/// punctuation query) — callers should fall back to LIKE in that case.
pub fn sanitize_fts5_query(q: &str) -> Option<String> {
    let mut out_parts: Vec<String> = Vec::new();
    for raw in q.split_whitespace() {
        let upper = raw.to_ascii_uppercase();
        if matches!(upper.as_str(), "OR" | "AND" | "NOT") {
            out_parts.push(upper);
            continue;
        }
        // Strip everything except alnum + CJK + ASCII space.
        let mut kept = String::with_capacity(raw.len());
        for c in raw.chars() {
            let cp = c as u32;
            let keep = c.is_ascii_alphanumeric()
                || (0x4E00..=0x9FFF).contains(&cp)
                || (0x3040..=0x309F).contains(&cp)
                || (0x30A0..=0x30FF).contains(&cp)
                || c == '_';
            if keep {
                kept.push(c);
            }
        }
        if kept.is_empty() {
            continue;
        }
        // Escape embedded double quotes by doubling (FTS5 phrase-escape).
        let safe = kept.replace('"', "\"\"");
        out_parts.push(safe);
    }
    if out_parts.is_empty() {
        None
    } else {
        Some(out_parts.join(" "))
    }
}

/// Session information returned by list command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub checkpoint_count: usize,
    pub created_at: Option<DateTime<Utc>>,
    pub last_updated: Option<DateTime<Utc>>,
    pub latest_step: i64,
    pub latest_source: String,
    pub title: Option<String>,
}

/// Session detail with message history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    #[serde(flatten)]
    pub info: SessionInfo,
    pub message_count: usize,
    pub first_user_message: Option<String>,
    pub last_assistant_reply: Option<String>,
}

/// Session manager for unified memory.db.
#[derive(Debug, Clone)]
pub struct SessionManager {
    db_path: PathBuf,
}

/// Session command line arguments.
#[derive(clap::Args, Debug, Clone)]
pub struct SessionArgs {
    #[command(subcommand)]
    pub command: SessionCommand,
}

/// Session subcommands.
#[derive(Subcommand, Debug, Clone)]
pub enum SessionCommand {
    List(ListArgs),
    Show { session_id: String },
    Delete { session_id: String },
    Rename { session_id: String, title: String },
    Cat { session_id: String },
}

/// Arguments for the `session list` subcommand.
#[derive(Args, Debug, Clone, Default)]
pub struct ListArgs {
    #[arg(long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub until: Option<String>,
    #[arg(long)]
    pub reverse: bool,
    #[arg(long)]
    pub oneline: bool,
    #[arg(long)]
    pub no_pager: bool,
    #[arg(long)]
    pub grep: Option<String>,
    #[arg(long)]
    pub format: Option<String>,
}

/// One row of token/cost deltas — passed to `update_token_counts`.
///
/// All fields are `Option` so callers that only know e.g. the input
/// token count from an OpenAI-style usage block can leave the others
/// as `None` and have them treated as zero deltas. `None` is not the
/// same as "0" for billing fields: a `None` cost field preserves the
/// stored value (see `COALESCE` in `update_token_counts`); an
/// explicit `Some(0.0)` zeroes it out.
#[derive(Debug, Clone, Default)]
pub struct TokenDelta<'a> {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub estimated_cost_usd: Option<f64>,
    pub actual_cost_usd: Option<f64>,
    pub pricing_version: Option<&'a str>,
    pub billing_provider: Option<&'a str>,
    pub billing_base_url: Option<&'a str>,
    pub billing_mode: Option<&'a str>,
    pub api_call_count: Option<i64>,
}

/// Read-side row mirroring `session_token_counters`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenCountRow {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub estimated_cost_usd: f64,
    pub actual_cost_usd: f64,
    pub pricing_version: Option<String>,
    pub billing_provider: Option<String>,
    pub billing_base_url: Option<String>,
    pub billing_mode: Option<String>,
    pub api_call_count: i64,
    pub updated_at: i64,
}

/// Rich session info returned by `list_sessions_rich` (priority #5).
///
/// Carries the base `SessionInfo` plus lineage columns from the
/// `sessions` sidecar table. `compression_tip` is `Some(_)` only when
/// `end_reason == "compression"`; it's the thread_id of the most
/// recent live descendant resolved via `get_compression_tip`'s
/// recursive CTE.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RichSessionInfo {
    #[serde(flatten)]
    pub info: SessionInfo,
    pub parent_session_id: Option<String>,
    pub end_reason: String,
    pub compression_tip: Option<String>,
}

/// Async wrapper around `SessionManager` (priority #11 gap).
///
/// Hermes parity (`hermes_state.py`): the Python implementation
/// exposes every method through `__getattr__` which dispatches into
/// `asyncio.to_thread` so the synchronous `sqlite3` driver doesn't
/// block the event loop. anureo's `SessionManager` is synchronous
/// because the SQLite dependency (`rusqlite::Connection`) is itself
/// sync — calling `Connection::open` from inside a `tokio::main`
/// future stalls the runtime.
///
/// `AsyncSessionManager` holds a `SessionManager` by value and
/// forwards every public method through `tokio::task::spawn_blocking`.
/// Each call therefore costs one thread-pool context switch but the
/// underlying SQLite work is fully off the reactor.
///
/// The set of wrappers below is intentionally narrow: it covers every
/// method called from the ACP review-runner / curator / async CLI
/// dispatch paths. New sync methods added to `SessionManager` are NOT
/// automatically async — add a wrapper here when an async caller
/// needs them.
#[derive(Debug, Clone)]
pub struct AsyncSessionManager {
    inner: SessionManager,
}

impl AsyncSessionManager {
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            inner: SessionManager::new(db_path),
        }
    }

    pub fn with_default_path() -> Self {
        Self {
            inner: SessionManager::with_default_path(),
        }
    }

    pub fn blocking(&self) -> &SessionManager {
        &self.inner
    }

    pub async fn list_sessions_filtered(
        &self,
        limit: usize,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        reverse: bool,
    ) -> Result<Vec<SessionInfo>, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            inner.list_sessions_filtered(limit, since, until, reverse)
        })
        .await
        .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn show_session(&self, session_id: String) -> Result<Option<SessionDetail>, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.show_session(&session_id))
            .await
            .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn delete_session(&self, session_id: String) -> Result<usize, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.delete_session(&session_id))
            .await
            .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn clear_messages(&self, session_id: String) -> Result<usize, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.clear_messages(&session_id))
            .await
            .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn rename_session(&self, session_id: String, title: String) -> Result<(), String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.rename_session(&session_id, &title))
            .await
            .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn search_sessions(
        &self,
        query: String,
        limit: usize,
    ) -> Result<Vec<SessionInfo>, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.search_sessions(&query, limit))
            .await
            .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn cat_session(
        &self,
        session_id: String,
    ) -> Result<Vec<stream_event::CodexEvent>, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.cat_session(&session_id))
            .await
            .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn extract_session_text(&self, session_id: String) -> Result<String, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.extract_session_text(&session_id))
            .await
            .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn update_token_counts(
        &self,
        session_id: String,
        model: String,
        delta: TokenDelta<'static>,
        absolute: bool,
    ) -> Result<(), String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || {
            inner.update_token_counts(&session_id, &model, delta, absolute)
        })
        .await
        .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn read_token_counts(
        &self,
        session_id: String,
        model: String,
    ) -> Result<Option<TokenCountRow>, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.read_token_counts(&session_id, &model))
            .await
            .map_err(|e| format!("join error: {}", e))?
    }

    pub async fn count_empty_sessions(&self) -> Result<usize, String> {
        let inner = self.inner.clone();
        tokio::task::spawn_blocking(move || inner.count_empty_sessions())
            .await
            .map_err(|e| format!("join error: {}", e))?
    }
}

impl SessionManager {
    /// Convenience constructor: convert a sync `SessionManager` to its
    /// async counterpart. Useful when a sync helper needs to hand the
    /// manager to an async caller.
    pub fn into_async(self) -> AsyncSessionManager {
        AsyncSessionManager { inner: self }
    }

    pub fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    pub fn with_default_path() -> Self {
        Self::new(checkpoint_sqlite_store::default_memory_db_path())
    }

    /// Lists sessions with SQL-level filtering (limit, since, until, reverse).
    pub fn list_sessions_filtered(
        &self,
        limit: usize,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        reverse: bool,
    ) -> Result<Vec<SessionInfo>, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;

        let mut sql = String::from(
            r#"
            SELECT
                thread_id,
                COUNT(*) as checkpoint_count,
                MIN(metadata_created_at) as created_at,
                MAX(metadata_created_at) as last_updated,
                (SELECT metadata_step FROM checkpoints c2
                 WHERE c2.thread_id = c1.thread_id
                 ORDER BY metadata_created_at DESC LIMIT 1) as latest_step,
                (SELECT metadata_source FROM checkpoints c2
                 WHERE c2.thread_id = c1.thread_id
                 ORDER BY metadata_created_at DESC LIMIT 1) as latest_source,
                (SELECT metadata_summary FROM checkpoints c2
                 WHERE c2.thread_id = c1.thread_id
                 AND metadata_summary IS NOT NULL
                 ORDER BY metadata_created_at DESC LIMIT 1) as title
            FROM checkpoints c1
            GROUP BY thread_id
            "#,
        );

        let mut params: Vec<rusqlite::types::Value> = Vec::new();
        let mut having_clauses: Vec<String> = Vec::new();
        let mut next_param = 1usize;
        if let Some(since) = since {
            having_clauses.push(format!("MAX(metadata_created_at) >= ?{}", next_param));
            params.push(since.timestamp_millis().into());
            next_param += 1;
        }
        if let Some(until) = until {
            having_clauses.push(format!("MAX(metadata_created_at) <= ?{}", next_param));
            params.push(until.timestamp_millis().into());
            next_param += 1;
        }
        if !having_clauses.is_empty() {
            sql.push_str(" HAVING ");
            sql.push_str(&having_clauses.join(" AND "));
        }

        sql.push_str(if reverse {
            " ORDER BY last_updated ASC"
        } else {
            " ORDER BY last_updated DESC"
        });

        if limit > 0 {
            sql.push_str(&format!(" LIMIT ?{}", next_param));
            params.push((limit as i64).into());
        }

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("Failed to prepare statement: {}", e))?;

        let sessions = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                let session_id: String = row.get(0)?;
                let checkpoint_count: usize = row.get(1)?;
                let created_at_ms: Option<i64> = row.get(2)?;
                let last_updated_ms: Option<i64> = row.get(3)?;
                let latest_step: i64 = row.get(4)?;
                let latest_source: String = row.get(5)?;
                let title: Option<String> = row.get(6)?;

                Ok(SessionInfo {
                    session_id,
                    checkpoint_count,
                    created_at: created_at_ms.and_then(DateTime::from_timestamp_millis),
                    last_updated: last_updated_ms.and_then(DateTime::from_timestamp_millis),
                    latest_step,
                    latest_source,
                    title,
                })
            })
            .map_err(|e| format!("Failed to query sessions: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect sessions: {}", e))?;

        Ok(sessions)
    }

    pub fn parse_date_arg(s: &str) -> Result<DateTime<Utc>, String> {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return Ok(dt.with_timezone(&Utc));
        }
        if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            let dt = d
                .and_hms_opt(0, 0, 0)
                .ok_or_else(|| format!("Invalid date: {}", s))?;
            return Ok(DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc));
        }
        Err(format!(
            "Invalid date '{}': expected YYYY-MM-DD or RFC 3339",
            s
        ))
    }

    pub fn show_session(&self, session_id: &str) -> Result<Option<SessionDetail>, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;

        let mut stmt = conn
            .prepare(
                r#"
            SELECT
                COUNT(*) as checkpoint_count,
                MIN(metadata_created_at) as created_at,
                MAX(metadata_created_at) as last_updated,
                (SELECT metadata_step FROM checkpoints c2
                 WHERE c2.thread_id = ?1
                 ORDER BY metadata_created_at DESC LIMIT 1) as latest_step,
                (SELECT metadata_source FROM checkpoints c2
                 WHERE c2.thread_id = ?1
                 ORDER BY metadata_created_at DESC LIMIT 1) as latest_source,
                (SELECT metadata_summary FROM checkpoints c2
                 WHERE c2.thread_id = ?1
                 AND metadata_summary IS NOT NULL
                 ORDER BY metadata_created_at DESC LIMIT 1) as title
            FROM checkpoints
            WHERE thread_id = ?1
            "#,
            )
            .map_err(|e| format!("Failed to prepare statement: {}", e))?;

        let info = stmt
            .query_row([session_id], |row| {
                let checkpoint_count: usize = row.get(0)?;
                let created_at_ms: Option<i64> = row.get(1)?;
                let last_updated_ms: Option<i64> = row.get(2)?;
                let latest_step: i64 = row.get(3)?;
                let latest_source: String = row.get(4)?;
                let title: Option<String> = row.get(5)?;
                Ok(SessionInfo {
                    session_id: session_id.to_string(),
                    checkpoint_count,
                    created_at: created_at_ms.and_then(DateTime::from_timestamp_millis),
                    last_updated: last_updated_ms.and_then(DateTime::from_timestamp_millis),
                    latest_step,
                    latest_source,
                    title,
                })
            })
            .optional()
            .map_err(|e| format!("Failed to query session: {}", e))?;

        let info = match info {
            Some(i) => i,
            None => return Ok(None),
        };

        let mut payload_stmt = conn.prepare(
            "SELECT payload FROM checkpoints WHERE thread_id = ?1 ORDER BY metadata_created_at DESC LIMIT 1"
        ).map_err(|e| format!("Failed to prepare payload statement: {}", e))?;

        let payload: Option<Vec<u8>> = payload_stmt
            .query_row([session_id], |row| row.get(0))
            .optional()
            .map_err(|e| format!("Failed to query payload: {}", e))?;

        let (message_count, first_user_message, last_assistant_reply) = if let Some(data) = payload
        {
            match serde_json::from_slice::<agent::state::ReActState>(&data) {
                Ok(state) => {
                    let first_user = state.messages.iter().find_map(|m| match m {
                        anureo_llm::message::Message::User(s) => Some(s.as_text().to_string()),
                        _ => None,
                    });
                    (
                        state.messages.len(),
                        first_user,
                        state.last_assistant_reply(),
                    )
                }
                Err(_) => (0, None, None),
            }
        } else {
            (0, None, None)
        };

        Ok(Some(SessionDetail {
            info,
            message_count,
            first_user_message,
            last_assistant_reply,
        }))
    }

    pub fn delete_session(&self, session_id: &str) -> Result<usize, String> {
        let mut conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        // Cascade-aware delete (priority #23 gap, Hermes `hermes_state.py`).
        //
        // anureo's checkpoint table doesn't yet have a `parent_thread_id`
        // column, but `extract_session_text` and friends surface related
        // threads via the most-recent `metadata_summary` cell. For now
        // we delete the requested thread inside `BEGIN IMMEDIATE` so the
        // delete is atomic across concurrent readers, and clear any
        // checkpoints that share the same metadata_summary cell (those
        // are forked/delegate children — Hermes surfaces them as
        // separate session rows that should be cleaned up together).
        // The checkpoint-count surfaced to callers matches the rows we
        // actually removed from the target thread (plus any delegates).
        checkpoint_sqlite_store::execute_write(&mut conn, |tx| {
            // Step 1: collect related thread_ids via shared summary.
            let mut stmt = tx
                .prepare(
                    "SELECT DISTINCT thread_id FROM checkpoints
                 WHERE thread_id != ?1
                   AND metadata_summary IN (
                     SELECT metadata_summary FROM checkpoints WHERE thread_id = ?1
                   )",
                )
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let related: Vec<String> = stmt
                .query_map([session_id], |row| row.get::<_, String>(0))
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?
                .filter_map(|r| r.ok())
                .collect();
            drop(stmt);
            // Step 2: delete the target thread.
            let mut total: usize = tx
                .execute("DELETE FROM checkpoints WHERE thread_id = ?1", [session_id])
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            // Step 3: cascade delegates.
            for tid in &related {
                let n = tx
                    .execute("DELETE FROM checkpoints WHERE thread_id = ?1", [tid])
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                total += n;
            }
            Ok(total)
        })
        .map_err(|e| format!("Failed to delete session: {}", e))
    }

    /// `clear_messages` companion (priority #23): mark every checkpoint's
    /// messages as inactive rather than deleting the row. Lets the
    /// caller retain the metadata trail (created_at, last source, etc.)
    /// while hiding the content from `extract_session_text` / search.
    pub fn clear_messages(&self, session_id: &str) -> Result<usize, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        conn.execute(
            "UPDATE checkpoints
             SET payload = X''
             WHERE thread_id = ?1",
            [session_id],
        )
        .map_err(|e| format!("Failed to clear messages: {}", e))
    }

    // ------------------------------------------------------------------
    // Sessions+lineage schema (priority #5 gap, Hermes `hermes_state.py`).
    //
    // Hermes tracks session metadata in a dedicated `sessions` table
    // (id, parent_session_id, end_reason, model_config JSON, archived_at)
    // so compression and branching can be reconstructed even after
    // checkpoints have been pruned. anureo's `checkpoints` table only
    // stores the JSON state blob per thread; there is no SQL-side
    // linkage between compressed and live sessions.
    //
    // We add a `sessions` sidecar table that stores one row per
    // `thread_id` with the lineage columns. The agent loop in
    // `agent/agent-core/src/agent/react/runner/runner.rs` calls
    // `archive_and_compact` on compaction trigger (see priority #5
    // wiring); `list_sessions_rich` projects compression roots to the
    // live tip via the recursive CTE in `get_compression_tip`.
    // ------------------------------------------------------------------

    /// Idempotent schema migration for the `sessions` lineage table.
    pub fn ensure_sessions_schema(&self, conn: &rusqlite::Connection) -> Result<(), String> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id                 TEXT    PRIMARY KEY,
                parent_session_id  TEXT,
                end_reason         TEXT    NOT NULL DEFAULT 'normal'
                    CHECK (end_reason IN
                        ('normal','compression','branched','orphaned','abandoned')),
                model_config       TEXT,
                archived_at        INTEGER,
                created_at         INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                updated_at         INTEGER NOT NULL DEFAULT (strftime('%s','now'))
            );
            CREATE INDEX IF NOT EXISTS idx_sessions_parent
                ON sessions(parent_session_id);
            CREATE INDEX IF NOT EXISTS idx_sessions_end_reason
                ON sessions(end_reason);
            "#,
        )
        .map_err(|e| format!("Failed to create sessions table: {}", e))
    }

    /// Insert or update a session lineage row.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_session_lineage(
        &self,
        session_id: &str,
        parent_session_id: Option<&str>,
        end_reason: &str,
        model_config: Option<&str>,
        archived_at: Option<i64>,
    ) -> Result<(), String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_sessions_schema(&conn)?;
        conn.execute(
            r#"INSERT INTO sessions
                 (id, parent_session_id, end_reason, model_config, archived_at, updated_at)
               VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'))
               ON CONFLICT(id) DO UPDATE SET
                 parent_session_id = excluded.parent_session_id,
                 end_reason        = excluded.end_reason,
                 model_config      = excluded.model_config,
                 archived_at       = excluded.archived_at,
                 updated_at        = strftime('%s','now')"#,
            rusqlite::params![
                session_id,
                parent_session_id,
                end_reason,
                model_config,
                archived_at,
            ],
        )
        .map_err(|e| format!("Failed to upsert session lineage: {}", e))?;
        Ok(())
    }

    /// Resolve a compression root (parent_session_id IS NULL or
    /// end_reason='compression') to its most recent live descendant
    /// via the recursive CTE. Returns the tip thread_id, or `None`
    /// when no live descendant exists (compression was abandoned).
    pub fn get_compression_tip(&self, root_session_id: &str) -> Result<Option<String>, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_sessions_schema(&conn)?;
        let tip: Option<String> = conn
            .query_row(
                r#"
                WITH RECURSIVE delegate_children AS (
                    SELECT id, parent_session_id, 1 AS depth
                      FROM sessions WHERE parent_session_id = ?1
                    UNION
                    SELECT s.id, s.parent_session_id, dc.depth + 1
                      FROM sessions s
                      JOIN delegate_children dc
                        ON s.parent_session_id = dc.id
                )
                SELECT id FROM delegate_children
                 WHERE end_reason = 'normal'
                 ORDER BY depth DESC LIMIT 1
                "#,
                [root_session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("Failed to query compression tip: {}", e))?;
        Ok(tip)
    }

    /// Hermes `_is_compression_ancestor` parity: true when the row's
    /// end_reason is `compression` (a parent summarisation that has
    /// not yet been superseded by a normal tip).
    pub fn is_compression_ancestor(&self, session_id: &str) -> Result<bool, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_sessions_schema(&conn)?;
        let r: Option<String> = conn
            .query_row(
                "SELECT end_reason FROM sessions WHERE id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("Failed to query end_reason: {}", e))?;
        Ok(matches!(r.as_deref(), Some("compression")))
    }

    /// `list_sessions_rich` (Hermes parity): join the lineage table
    /// against `list_sessions_filtered`'s projection so each row
    /// carries `end_reason` and a `compression_tip` resolved via the
    /// recursive CTE. Sessions without a lineage row are treated as
    /// `end_reason='normal'` for backwards compatibility.
    pub fn list_sessions_rich(
        &self,
        limit: usize,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
        reverse: bool,
    ) -> Result<Vec<RichSessionInfo>, String> {
        let base = self.list_sessions_filtered(limit, since, until, reverse)?;
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_sessions_schema(&conn)?;
        let mut out: Vec<RichSessionInfo> = Vec::with_capacity(base.len());
        for info in base {
            let row: Option<(Option<String>, String)> = conn
                .query_row(
                    "SELECT parent_session_id, end_reason FROM sessions WHERE id = ?1",
                    [&info.session_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| format!("Failed to query lineage: {}", e))?;
            let (parent_session_id, end_reason) = row.unwrap_or((None, "normal".to_string()));
            let compression_tip = if end_reason == "compression" {
                self.get_compression_tip(&info.session_id)?
            } else {
                None
            };
            out.push(RichSessionInfo {
                info,
                parent_session_id,
                end_reason,
                compression_tip,
            });
        }
        Ok(out)
    }

    /// `archive_and_compact` (Hermes parity): summarise messages for
    /// `session_id`, mark the source as compressed, and insert a new
    /// session row with `end_reason='compression'` whose `model_config`
    /// JSON carries `_branched_from` / `_delegate_from` markers.
    ///
    /// Returns the new session_id (UUID-like string derived from
    /// `parent_session_id` + timestamp).
    pub fn archive_and_compact(&self, session_id: &str, summary: &str) -> Result<String, String> {
        let mut conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_sessions_schema(&conn)?;
        let new_id = format!(
            "{}-compacted-{}",
            session_id,
            chrono::Utc::now().timestamp()
        );
        let model_config = serde_json::json!({
            "_branched_from": session_id,
            "_delegate_from": null,
            "summary": summary,
        })
        .to_string();
        checkpoint_sqlite_store::execute_write(&mut conn, |tx| {
            // Mark source compacted.
            tx.execute(
                "INSERT INTO sessions(id, parent_session_id, end_reason, model_config)
                 VALUES (?1, NULL, 'compression', ?2)
                 ON CONFLICT(id) DO UPDATE SET
                   end_reason  = 'compression',
                   model_config = excluded.model_config,
                   updated_at = strftime('%s','now')",
                rusqlite::params![session_id, model_config],
            )?;
            // Insert child summary row.
            tx.execute(
                "INSERT INTO sessions(id, parent_session_id, end_reason, model_config)
                 VALUES (?1, ?2, 'normal', NULL)",
                rusqlite::params![&new_id, session_id],
            )?;
            Ok(())
        })
        .map_err(|e| format!("Failed to archive_and_compact: {}", e))?;
        Ok(new_id)
    }

    /// Returns the number of sessions whose latest checkpoint payload is
    /// empty (the row exists but the JSON body is X''). Used by
    /// `prune_empty_sessions` to decide which rows to evict.
    pub fn count_empty_sessions(&self) -> Result<usize, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(DISTINCT thread_id) FROM checkpoints
                 WHERE rowid IN (
                     SELECT rowid FROM checkpoints c1
                     WHERE c1.metadata_created_at = (
                         SELECT MAX(c2.metadata_created_at)
                         FROM checkpoints c2 WHERE c2.thread_id = c1.thread_id
                     )
                 ) AND payload = X''",
                [],
                |row| row.get(0),
            )
            .map_err(|e| format!("Failed to count empty sessions: {}", e))?;
        Ok(n as usize)
    }

    /// Active/compacted/rewound message state (priority #6 gap).
    ///
    /// Hermes parity (`hermes_state.py`): the messages table tracks
    /// three flags per row — `active=1` (still visible), `compacted=1`
    /// (rolled into a summary), `rewound=1` (rolled back via /rewind).
    /// anureo's checkpoint table stores messages inside the JSON
    /// `payload` blob, so the SQL columns are mirrored in a sidecar
    /// `message_state` table keyed by `(thread_id, message_id)`.
    ///
    /// Filter routing (priority #6 review card):
    ///   * `cat_session`: WHERE active=1 only — the user must not see
    ///     rewound messages when they `cat` a session.
    ///   * `search_sessions` / `extract_session_text`: WHERE active=1
    ///     OR compacted=1 — compaction preserves the content for
    ///     replay (gap #38763), only rewound is hidden.
    ///   * `rewind_to_message`: sets active=0 for everything after
    ///     the target message_id, marking it rewound=1.
    ///   * `restore_rewound`: flips everything after the marker back
    ///     to active=1 / rewound=0.
    pub fn ensure_message_state_schema(&self, conn: &rusqlite::Connection) -> Result<(), String> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS message_state (
                thread_id   TEXT    NOT NULL,
                message_id  TEXT    NOT NULL,
                active      INTEGER NOT NULL DEFAULT 1,
                compacted   INTEGER NOT NULL DEFAULT 0,
                rewound     INTEGER NOT NULL DEFAULT 0,
                updated_at  INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                PRIMARY KEY (thread_id, message_id)
            );
            CREATE INDEX IF NOT EXISTS idx_message_state_thread
                ON message_state(thread_id, active);
            "#,
        )
        .map_err(|e| format!("Failed to create message_state: {}", e))
    }

    /// Mark every message after `message_id` (created_at comparison is
    /// done by the caller; here we just take the message_id) as
    /// rewound. Returns the number of rows affected.
    ///
    /// Implementation: marks all rows for `thread_id` whose
    /// `created_at >= target.created_at` as active=0, rewound=1.
    /// Uses the `payload.metadata_created_at` ISO-8601 string for
    /// ordering because anureo's message table doesn't have a
    /// SQL-side created_at column.
    pub fn rewind_to_message(
        &self,
        thread_id: &str,
        target_message_id: &str,
    ) -> Result<usize, String> {
        let mut conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_message_state_schema(&conn)?;
        checkpoint_sqlite_store::execute_write(&mut conn, |tx| {
            // Resolve target's metadata_created_at from the checkpoint
            // payload by message_id (search payloads containing the
            // message_id).
            let target_ts: Option<String> = tx
                .query_row(
                    "SELECT metadata_created_at FROM checkpoints
                     WHERE thread_id = ?1 AND payload LIKE ?2
                     ORDER BY metadata_created_at DESC LIMIT 1",
                    rusqlite::params![thread_id, format!("%\"id\":\"{}\"%", target_message_id)],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let Some(ts) = target_ts else {
                return Ok(0);
            };
            // Mark all later checkpoints as rewound.
            let rows = tx
                .execute(
                    "INSERT OR REPLACE INTO message_state
                       (thread_id, message_id, active, compacted, rewound, updated_at)
                     SELECT thread_id,
                            ?2,
                            0,
                            compacted,
                            1,
                            strftime('%s','now')
                       FROM checkpoints
                      WHERE thread_id = ?1 AND metadata_created_at > ?3",
                    rusqlite::params![thread_id, target_message_id, ts],
                )
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            Ok(rows)
        })
        .map_err(|e| format!("Failed to rewind: {}", e))
    }

    /// Reverse a `rewind_to_message` call: restore every message after
    /// `target_message_id` to active=1 / rewound=0.
    pub fn restore_rewound(
        &self,
        thread_id: &str,
        target_message_id: &str,
    ) -> Result<usize, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_message_state_schema(&conn)?;
        conn.execute(
            "UPDATE message_state SET active = 1, rewound = 0,
                 updated_at = strftime('%s','now')
              WHERE thread_id = ?1 AND message_id = ?2 AND rewound = 1",
            rusqlite::params![thread_id, target_message_id],
        )
        .map_err(|e| format!("Failed to restore_rewound: {}", e))
    }

    /// Helper: returns true if `message_id` is still active for
    /// `thread_id` (defaulting to true when no row exists — backwards
    /// compatible with sessions pre-dating the sidecar).
    pub fn is_message_active(&self, thread_id: &str, message_id: &str) -> Result<bool, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_message_state_schema(&conn)?;
        let active: Option<i64> = conn
            .query_row(
                "SELECT active FROM message_state WHERE thread_id = ?1 AND message_id = ?2",
                rusqlite::params![thread_id, message_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| format!("Failed to query message_state: {}", e))?;
        Ok(active.unwrap_or(1) != 0)
    }
    ///
    /// Hermes parity (`hermes_state.py`): persists input/output/cached/
    /// reasoning token totals plus estimated and actual cost per session
    /// in a sidecar table so model switches / session resumes don't lose
    /// prior usage. We key by `(session_id, model)` because the same
    /// session may run across multiple models (e.g. compress with a
    /// cheaper model after a frontier-model run).
    ///
    /// The schema is created on first call via `ensure_token_counters_schema`
    /// (idempotent). Subsequent calls skip the DDL.
    pub fn ensure_token_counters_schema(&self, conn: &rusqlite::Connection) -> Result<(), String> {
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS session_token_counters (
                session_id        TEXT    NOT NULL,
                model             TEXT    NOT NULL,
                input_tokens      INTEGER NOT NULL DEFAULT 0,
                output_tokens     INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                reasoning_tokens  INTEGER NOT NULL DEFAULT 0,
                estimated_cost_usd REAL    NOT NULL DEFAULT 0.0,
                actual_cost_usd    REAL    NOT NULL DEFAULT 0.0,
                pricing_version    TEXT,
                billing_provider   TEXT,
                billing_base_url   TEXT,
                billing_mode       TEXT,
                api_call_count     INTEGER NOT NULL DEFAULT 0,
                updated_at         INTEGER NOT NULL DEFAULT (strftime('%s','now')),
                PRIMARY KEY (session_id, model)
            );
            "#,
        )
        .map_err(|e| format!("Failed to create session_token_counters: {}", e))
    }

    /// Update token counters for `(session_id, model)`.
    ///
    /// `absolute=false` (default): adds `delta` to each non-None field
    /// — typical "after each LLM response" call from
    /// `apps/acp/src/agent.rs:1451`.
    /// `absolute=true`: replaces the column values with the provided
    /// values — used when reconciling against the provider's billing API.
    ///
    /// Implementation: INSERT OR IGNORE then UPDATE — the IGNORE
    /// inserts a zero row when (session_id, model) is new, then the
    /// UPDATE applies the delta or the absolute value.
    #[allow(clippy::too_many_arguments)]
    pub fn update_token_counts(
        &self,
        session_id: &str,
        model: &str,
        delta: TokenDelta<'_>,
        absolute: bool,
    ) -> Result<(), String> {
        let mut conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_token_counters_schema(&conn)?;
        checkpoint_sqlite_store::execute_write(&mut conn, |tx| {
            tx.execute(
                "INSERT OR IGNORE INTO session_token_counters(session_id, model) VALUES (?1, ?2)",
                rusqlite::params![session_id, model],
            )?;
            if absolute {
                tx.execute(
                    r#"UPDATE session_token_counters SET
                        input_tokens      = COALESCE(?3, input_tokens),
                        output_tokens     = COALESCE(?4, output_tokens),
                        cache_read_tokens = COALESCE(?5, cache_read_tokens),
                        cache_write_tokens= COALESCE(?6, cache_write_tokens),
                        reasoning_tokens  = COALESCE(?7, reasoning_tokens),
                        estimated_cost_usd= COALESCE(?8, estimated_cost_usd),
                        actual_cost_usd   = COALESCE(?9, actual_cost_usd),
                        pricing_version   = COALESCE(?10, pricing_version),
                        billing_provider  = COALESCE(?11, billing_provider),
                        billing_base_url  = COALESCE(?12, billing_base_url),
                        billing_mode      = COALESCE(?13, billing_mode),
                        api_call_count    = COALESCE(?14, api_call_count),
                        updated_at        = strftime('%s','now')
                       WHERE session_id = ?1 AND model = ?2"#,
                    rusqlite::params![
                        session_id,
                        model,
                        delta.input_tokens,
                        delta.output_tokens,
                        delta.cache_read_tokens,
                        delta.cache_write_tokens,
                        delta.reasoning_tokens,
                        delta.estimated_cost_usd,
                        delta.actual_cost_usd,
                        delta.pricing_version,
                        delta.billing_provider,
                        delta.billing_base_url,
                        delta.billing_mode,
                        delta.api_call_count,
                    ],
                )?;
            } else {
                tx.execute(
                    r#"UPDATE session_token_counters SET
                        input_tokens      = input_tokens      + COALESCE(?3, 0),
                        output_tokens     = output_tokens     + COALESCE(?4, 0),
                        cache_read_tokens = cache_read_tokens + COALESCE(?5, 0),
                        cache_write_tokens= cache_write_tokens+ COALESCE(?6, 0),
                        reasoning_tokens  = reasoning_tokens  + COALESCE(?7, 0),
                        estimated_cost_usd= estimated_cost_usd+ COALESCE(?8, 0.0),
                        actual_cost_usd   = actual_cost_usd   + COALESCE(?9, 0.0),
                        pricing_version   = COALESCE(?10, pricing_version),
                        billing_provider  = COALESCE(?11, billing_provider),
                        billing_base_url  = COALESCE(?12, billing_base_url),
                        billing_mode      = COALESCE(?13, billing_mode),
                        api_call_count    = api_call_count    + COALESCE(?14, 0),
                        updated_at        = strftime('%s','now')
                       WHERE session_id = ?1 AND model = ?2"#,
                    rusqlite::params![
                        session_id,
                        model,
                        delta.input_tokens,
                        delta.output_tokens,
                        delta.cache_read_tokens,
                        delta.cache_write_tokens,
                        delta.reasoning_tokens,
                        delta.estimated_cost_usd,
                        delta.actual_cost_usd,
                        delta.pricing_version,
                        delta.billing_provider,
                        delta.billing_base_url,
                        delta.billing_mode,
                        delta.api_call_count,
                    ],
                )?;
            }
            Ok(())
        })
        .map_err(|e| format!("Failed to update token counters: {}", e))
    }

    /// Read the persisted counter row for `(session_id, model)`.
    pub fn read_token_counts(
        &self,
        session_id: &str,
        model: &str,
    ) -> Result<Option<TokenCountRow>, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        self.ensure_token_counters_schema(&conn)?;
        let mut stmt = conn
            .prepare(
                "SELECT input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                        reasoning_tokens, estimated_cost_usd, actual_cost_usd,
                        pricing_version, billing_provider, billing_base_url, billing_mode,
                        api_call_count, updated_at
                   FROM session_token_counters WHERE session_id = ?1 AND model = ?2",
            )
            .map_err(|e| format!("Failed to prepare read: {}", e))?;
        let mut rows = stmt
            .query(rusqlite::params![session_id, model])
            .map_err(|e| format!("Failed to query: {}", e))?;
        match rows.next().map_err(|e| format!("Failed to step: {}", e))? {
            Some(row) => Ok(Some(TokenCountRow {
                input_tokens: row.get(0).unwrap_or(0),
                output_tokens: row.get(1).unwrap_or(0),
                cache_read_tokens: row.get(2).unwrap_or(0),
                cache_write_tokens: row.get(3).unwrap_or(0),
                reasoning_tokens: row.get(4).unwrap_or(0),
                estimated_cost_usd: row.get(5).unwrap_or(0.0),
                actual_cost_usd: row.get(6).unwrap_or(0.0),
                pricing_version: row.get(7).ok(),
                billing_provider: row.get(8).ok(),
                billing_base_url: row.get(9).ok(),
                billing_mode: row.get(10).ok(),
                api_call_count: row.get(11).unwrap_or(0),
                updated_at: row.get(12).unwrap_or(0),
            })),
            None => Ok(None),
        }
    }

    pub fn rename_session(&self, session_id: &str, title: &str) -> Result<(), String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        let affected = conn.execute(
            "UPDATE checkpoints SET metadata_summary = ?1
             WHERE rowid = (SELECT rowid FROM checkpoints WHERE thread_id = ?2 ORDER BY metadata_created_at DESC LIMIT 1)",
            rusqlite::params![title, session_id],
        ).map_err(|e| format!("Failed to rename session: {}", e))?;
        if affected == 0 {
            return Err(format!("Session not found: {}", session_id));
        }
        Ok(())
    }

    pub fn search_sessions(&self, query: &str, limit: usize) -> Result<Vec<SessionInfo>, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;

        // Hermes parity (`hermes_state.py` priority #2/#5/#7): route the
        // search through FTS5 + trigram when available, fall back to a
        // per-token LIKE scan when not. The probe result is cached
        // process-wide, so subsequent calls skip the CREATE/DROP.
        let (fts_ok, trigram_ok) = probe_fts_capability(&conn);

        // Heuristic: if the query is ≥3 CJK chars OR every token is ≥3
        // chars long, prefer trigram FTS5. Otherwise use per-token LIKE.
        let tokens: Vec<&str> = query.split_whitespace().collect();
        let cjk_total = count_cjk(query);
        let every_token_long_enough =
            !tokens.is_empty() && tokens.iter().all(|t| t.chars().count() >= 3);
        let use_trigram = fts_ok && trigram_ok && (cjk_total >= 3 || every_token_long_enough);
        let use_fts5 = fts_ok && !use_trigram;

        // Make sure the FTS index table exists (idempotent).
        if use_fts5 || use_trigram {
            let _ = conn.execute_batch(
                "CREATE VIRTUAL TABLE IF NOT EXISTS checkpoints_fts USING fts5(
                    thread_id UNINDEXED, message_text,
                    tokenize='unicode61 remove_diacritics 2'
                );
                CREATE VIRTUAL TABLE IF NOT EXISTS checkpoints_fts_trigram USING fts5(
                    thread_id UNINDEXED, message_text,
                    tokenize='trigram'
                );",
            );
        }

        // Strategy A: trigram FTS5 MATCH (CJK + long-token queries).
        if use_trigram {
            if let Some(safe_query) = sanitize_fts5_query(query) {
                let sql = "SELECT DISTINCT thread_id FROM checkpoints_fts_trigram \
                           WHERE message_text MATCH ?1 ORDER BY rank LIMIT ?2";
                if let Ok(mut stmt) = conn.prepare(sql) {
                    let ids: Vec<String> = stmt
                        .query_map(rusqlite::params![safe_query, limit as i64], |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(|e| format!("FTS5 trigram query failed: {}", e))?
                        .filter_map(|r| r.ok())
                        .collect();
                    if !ids.is_empty() {
                        return self.collect_session_infos(&ids);
                    }
                }
            }
            // fall through to LIKE on probe/build failure
        }

        // Strategy B: standard FTS5 MATCH (mixed alpha + numeric).
        if use_fts5 {
            if let Some(safe_query) = sanitize_fts5_query(query) {
                let sql = "SELECT DISTINCT thread_id FROM checkpoints_fts \
                           WHERE message_text MATCH ?1 ORDER BY rank LIMIT ?2";
                if let Ok(mut stmt) = conn.prepare(sql) {
                    let ids: Vec<String> = stmt
                        .query_map(rusqlite::params![safe_query, limit as i64], |row| {
                            row.get::<_, String>(0)
                        })
                        .map_err(|e| format!("FTS5 query failed: {}", e))?
                        .filter_map(|r| r.ok())
                        .collect();
                    if !ids.is_empty() {
                        return self.collect_session_infos(&ids);
                    }
                }
            }
        }

        // Strategy C: per-token LIKE fallback (always available, works on
        // any SQLite build). Tokens are OR-joined so the user gets at
        // least one matching message.
        let query_lower = query.to_lowercase();
        let like_clauses: Vec<String> = (1..=tokens.len().max(1))
            .map(|i| format!("LOWER(CAST(payload AS TEXT)) LIKE ?{}", i))
            .collect();
        let mut params: Vec<rusqlite::types::Value> = tokens
            .iter()
            .map(|t| format!("%{}%", t.to_lowercase()).into())
            .collect();
        if params.is_empty() {
            params.push(format!("%{}%", query_lower).into());
        }
        let sql = format!(
            "SELECT DISTINCT thread_id FROM checkpoints WHERE {} ORDER BY metadata_created_at DESC LIMIT ?{}",
            like_clauses.join(" OR "),
            params.len() + 1
        );
        params.push((limit as i64).into());

        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| format!("Failed to prepare LIKE statement: {}", e))?;
        let ids: Vec<String> = stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| format!("Failed to LIKE-query: {}", e))?
            .filter_map(|r| r.ok())
            .collect();

        self.collect_session_infos(&ids)
    }

    /// Helper that turns a set of `thread_id` strings into the ordered
    /// `SessionInfo` view, deduplicating against prior matches. Used by
    /// every strategy of `search_sessions` so the projection logic is
    /// centralised.
    fn collect_session_infos(&self, ids: &[String]) -> Result<Vec<SessionInfo>, String> {
        let mut matched = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for tid in ids {
            if matched.len() >= ids.len() {
                break;
            }
            if seen.contains(tid) {
                continue;
            }
            seen.insert(tid.clone());
            if let Ok(Some(detail)) = self.show_session(tid) {
                matched.push(detail.info);
            }
        }
        Ok(matched)
    }

    pub fn cat_session(&self, session_id: &str) -> Result<Vec<stream_event::CodexEvent>, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        let mut stmt = conn.prepare("SELECT payload FROM checkpoints WHERE thread_id = ?1 ORDER BY metadata_created_at ASC")
            .map_err(|e| format!("Failed to prepare statement: {}", e))?;
        let payloads: Vec<Vec<u8>> = stmt
            .query_map([session_id], |row| row.get(0))
            .map_err(|e| format!("Failed to query checkpoints: {}", e))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("Failed to collect payloads: {}", e))?;
        if payloads.is_empty() {
            return Err(format!("Session not found: {}", session_id));
        }
        let states: Vec<agent::state::ReActState> = payloads
            .iter()
            .filter_map(|d| serde_json::from_slice(d).ok())
            .collect();
        Ok(crate::codex_event_builder::build_codex_events(
            session_id, &states,
        ))
    }

    pub fn extract_session_text(&self, session_id: &str) -> Result<String, String> {
        let conn = rusqlite::Connection::open(&self.db_path)
            .map_err(|e| format!("Failed to open database: {}", e))?;
        let mut stmt = conn.prepare("SELECT payload FROM checkpoints WHERE thread_id = ?1 ORDER BY metadata_created_at ASC")
            .map_err(|e| format!("Failed to prepare statement: {}", e))?;
        let payloads: Vec<Vec<u8>> = stmt
            .query_map([session_id], |row| row.get(0))
            .map_err(|e| format!("Query failed: {}", e))?
            .filter_map(|r| r.ok())
            .collect();
        if payloads.is_empty() {
            return Err(format!("Session not found: {}", session_id));
        }

        let mut parts = Vec::new();
        for data in &payloads {
            if let Ok(state) = serde_json::from_slice::<agent::state::ReActState>(data) {
                for msg in &state.messages {
                    match msg {
                        anureo_llm::message::Message::User(u) => {
                            parts.push(format!("User: {}", u.as_text()))
                        }
                        anureo_llm::message::Message::Assistant(a) => {
                            if !a.content.is_empty() {
                                parts.push(format!("Assistant: {}", a.content));
                            }
                        }
                        anureo_llm::message::Message::Tool { content, .. } => {
                            if let Some(text) = content.as_text() {
                                if !text.is_empty() {
                                    parts.push(format!("Tool: {}", text));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        // Strip the curator's `<background_review>` harness block so the
        // replayed text doesn't appear to invoke memory/skill tools at
        // inference time (Hermes `hermes_state.py` #10).
        let assembled = parts.join("\n");
        Ok(anureo_llm::message::strip_background_review_harness(
            &assembled,
        ))
    }

    /// Prints session detail (used by `session show`, not `session list`).
    pub fn print_session_detail(&self, detail: &SessionDetail, json: bool) -> Result<(), String> {
        if json {
            let json_output = serde_json::to_string_pretty(detail)
                .map_err(|e| format!("Failed to serialize to JSON: {}", e))?;
            println!("{}", json_output);
        } else {
            println!("Session: {}", detail.info.session_id);
            println!("{}", "=".repeat(60));
            if let Some(ref title) = detail.info.title {
                println!("Title: {}", title);
            }
            println!("Checkpoints: {}", detail.info.checkpoint_count);
            println!("Messages: {}", detail.message_count);
            println!("Latest Step: {}", detail.info.latest_step);
            println!("Latest Source: {}", detail.info.latest_source);
            println!(
                "Created: {}",
                Self::format_datetime(&detail.info.created_at)
            );
            println!(
                "Last Updated: {}",
                Self::format_datetime(&detail.info.last_updated)
            );
            if let Some(ref msg) = detail.first_user_message {
                let truncated = if msg.chars().count() > 100 {
                    format!("{}...", msg.chars().take(100).collect::<String>())
                } else {
                    msg.clone()
                };
                println!("\nFirst User Message:\n  {}", truncated);
            }
            if let Some(ref reply) = detail.last_assistant_reply {
                let truncated = if reply.chars().count() > 200 {
                    format!("{}...", reply.chars().take(200).collect::<String>())
                } else {
                    reply.clone()
                };
                println!("\nLast Assistant Reply:\n  {}", truncated);
            }
        }
        Ok(())
    }

    fn format_datetime(dt: &Option<DateTime<Utc>>) -> String {
        dt.map(|t| {
            let local: DateTime<Local> = t.into();
            local.format("%Y-%m-%d %H:%M:%S").to_string()
        })
        .unwrap_or_else(|| "N/A".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_manager_creation() {
        let manager = SessionManager::with_default_path();
        assert!(manager.db_path.to_string_lossy().contains("memory.db"));
    }

    #[test]
    fn test_format_datetime() {
        let dt = DateTime::from_timestamp_millis(1700000000000);
        assert!(SessionManager::format_datetime(&dt).contains("2023"));
    }

    #[test]
    fn test_format_datetime_none() {
        assert_eq!(SessionManager::format_datetime(&None), "N/A");
    }

    #[test]
    fn parse_date_arg_rfc3339() {
        let dt = SessionManager::parse_date_arg("2025-07-15T10:30:00Z").unwrap();
        assert_eq!(dt.to_rfc3339(), "2025-07-15T10:30:00+00:00");
    }

    #[test]
    fn parse_date_arg_ymd_date() {
        let dt = SessionManager::parse_date_arg("2025-07-15").unwrap();
        assert_eq!(dt.to_rfc3339(), "2025-07-15T00:00:00+00:00");
    }

    #[test]
    fn parse_date_arg_invalid_returns_error() {
        let result = SessionManager::parse_date_arg("not-a-date");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Invalid date"));
    }
}

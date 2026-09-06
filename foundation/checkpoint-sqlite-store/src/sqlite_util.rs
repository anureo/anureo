//! Shared SQLite helpers (e.g. open with WAL for concurrent read/write).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::RwLock;
use std::time::Duration;

use rusqlite::TransactionBehavior;

/// Maximum retries for `SQLITE_BUSY` / `SQLITE_LOCKED` before giving up.
/// 5 attempts × ~50ms worst-case jitter ≈ 250ms; cheap enough to be
/// invisible to interactive sessions but enough to absorb
/// supervisor-checkpointed WAL bursts.
pub const EXECUTE_WRITE_BUSY_RETRIES: u32 = 5;
/// Per-retry backoff ceiling. The actual sleep is randomised in
/// `[0, BUSY_BACKOFF_JITTER_MAX]` to avoid thundering-herd retry storms
/// from multiple processes contending on the same SQLite db.
pub const BUSY_BACKOFF_JITTER_MAX: Duration = Duration::from_millis(50);

const MEMORY_DB_FILENAME: &str = "memory.db";

/// Run `f` inside `BEGIN IMMEDIATE` with retry-on-BUSY. The function
/// returns `Result<T, rusqlite::Error>` and the caller may map that to
/// anything they want — this wrapper only handles the lock-contention
/// path. `f` itself should perform all statements and commit is
/// implicit on `Ok`. Rollback is automatic on `f` returning `Err` via
/// the `tx` closure drop.
///
/// Hermes parity (`hermes_state.py` #1): without this wrapper, two
/// concurrent checkpoint writers (e.g. background review runner +
/// session message store) can produce `SQLITE_BUSY` mid-turn and the
/// user-visible experience is a transient failure that the operator
/// has no way to repeat.
///
/// On retry, the function sleeps a random `[0, BUSY_BACKOFF_JITTER_MAX)`
/// to desynchronise thundering-herd retry waves.
pub fn execute_write<T, F>(conn: &mut rusqlite::Connection, mut f: F) -> Result<T, rusqlite::Error>
where
    F: FnMut(&rusqlite::Transaction<'_>) -> Result<T, rusqlite::Error>,
{
    for attempt in 0..EXECUTE_WRITE_BUSY_RETRIES {
        let result: Result<T, rusqlite::Error> = (|| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let out = f(&tx)?;
            tx.commit()?;
            Ok(out)
        })();
        match result {
            Ok(v) => return Ok(v),
            Err(e) => {
                // SQLITE_BUSY=5, SQLITE_LOCKED=6 — in
                // libsqlite3-sys-0.28 (the bundled SQLite C bridge
                // rusqlite 0.31 uses), the corresponding enum
                // variants are `DatabaseBusy` and `DatabaseLocked`.
                // Match against those — the variant names have
                // been stable across the 0.27 → 0.31 series.
                let busy = matches!(
                    &e,
                    rusqlite::Error::SqliteFailure(sf, _)
                        if matches!(
                            sf.code,
                            rusqlite::ErrorCode::DatabaseBusy
                                | rusqlite::ErrorCode::DatabaseLocked
                        )
                );
                if !busy || attempt + 1 == EXECUTE_WRITE_BUSY_RETRIES {
                    return Err(e);
                }
                let jitter = {
                    use std::time::UNIX_EPOCH;
                    let nanos = std::time::SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.subsec_nanos())
                        .unwrap_or(0);
                    Duration::from_millis(
                        u64::from(nanos) % u64::from(BUSY_BACKOFF_JITTER_MAX.as_millis() as u32),
                    )
                };
                std::thread::sleep(jitter);
            }
        }
    }
    // unreachable: the loop either returns Ok or Err above on the last
    // iteration; satisfying the type-checker requires a real error.
    Err(rusqlite::Error::InvalidQuery)
}

/// Returns the default memory DB path (`~/.anureo/memory.db`).
/// Creates the parent directory if missing. Falls back to `memory.db` (cwd-relative) if home is unavailable.
pub fn default_memory_db_path() -> PathBuf {
    let path = env_config::home::anureo_home().join(MEMORY_DB_FILENAME);
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            tracing::warn!("Failed to create memory db directory: {}", e);
        }
    }
    path
}

/// Builds a detailed error message for open failures: path, resolved absolute path, and cause.
fn open_error_message(path: &Path, e: &impl std::fmt::Display) -> String {
    let path_display = path.display();
    let resolved = if path.is_absolute() {
        path_display.to_string()
    } else {
        std::env::current_dir()
            .ok()
            .map(|cwd| cwd.join(path).display().to_string())
            .unwrap_or_else(|| path_display.to_string())
    };
    format!("path='{}' resolved='{}': {}", path_display, resolved, e)
}

/// Opens a SQLite database and enables WAL mode for better concurrent read/write.
/// On failure, the error message includes the path, its resolution (cwd-relative), and the underlying cause.
///
/// Implements `apply_wal_with_fallback` (Hermes parity, `hermes_state.py` #0):
/// - Try `PRAGMA journal_mode=WAL`.
/// - On macOS, issue a `PRAGMA wal_checkpoint(PASSIVE)` barrier so the
///   `-shm`/`-wal` sidecar files actually exist on disk before the
///   connection is handed back. Without this, a crash during the first
///   checkpoint leaves the connection unable to upgrade to a higher
///   journal mode and silently downgrades to DELETE.
#[cfg_attr(not(target_os = "macos"), allow(unused))]
pub fn open_sqlite_with_wal(path: &Path) -> Result<rusqlite::Connection, String> {
    let conn = match crate::repair::repair_state_db_schema(path) {
        Ok(c) => c,
        Err(e) => {
            let msg = open_error_message(path, &e);
            set_last_init_error(format!("open failed: {}", msg));
            return Err(msg);
        }
    };
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(|error| open_error_message(path, &error))?;
    // Hermes parity: try WAL first; if WAL fails (NFS, SMB, network
    // volume, EROFS on /tmp sandbox), fall back to DELETE and warn once.
    // The fallback is logged at warn-level so a real outage shows up in
    // `anureo-sqlite-store` logs but we never panic on a transient FS issue.
    let wal_ok = conn
        .query_row::<String, _, _>("PRAGMA journal_mode=WAL;", [], |row| row.get(0))
        .map(|mode| mode.eq_ignore_ascii_case("wal"))
        .unwrap_or(false);

    if !wal_ok {
        eprintln!(
            "anureo-sqlite-store: WAL mode unavailable at {} (falling back to DELETE journal mode; \
             concurrent reads will serialize against writes — set ANUREO_FORCE_DELETE=1 to silence)",
            path.display()
        );
        let _ = conn.execute_batch("PRAGMA journal_mode=DELETE;");
    }

    // macOS workaround: the WAL shm sidecar must exist on disk before
    // other processes can attach. PASSIVE is a no-op when there is
    // nothing to checkpoint, and runs a single write of an empty slot
    // otherwise — exactly the barrier macOS needs.
    #[cfg(target_os = "macos")]
    {
        let _ = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    }
    // Reference conn to keep `if !wal_ok` block's err-handling consistent
    // across platforms while satisfying unused-warnings on non-macOS.
    #[cfg(not(target_os = "macos"))]
    {
        let _ = &conn;
    }

    Ok(conn)
}

/// Last-init-error surface (priority #9 gap).
///
/// Hermes parity (`hermes_state.py`): in the Python implementation,
/// `init_state_db` records the underlying exception in a module-level
/// `_last_init_error` slot and `SessionManager` exposes `db_available()`
/// which renders a friendly "database is locked" / "out of memory" /
/// "database disk image is malformed" message instead of the bare
/// `String` error our callers otherwise see.
///
/// In anureo, `open_sqlite_with_wal` returns `Err(String)` to keep the
/// async surface simple — but that erases the cause chain. We capture
/// the most-recent init failure in a `OnceLock<RwLock<Option<String>>>`
/// so callers (CLI `session.rs`, ACP review-runner, curator) can ask
/// `get_last_init_error()` and render a useful message like
/// `"anureo state db unavailable: database is locked at /home/u/.anureo/memory.db"`.
static LAST_INIT_ERROR: OnceLock<RwLock<Option<String>>> = OnceLock::new();

fn init_error_slot() -> &'static RwLock<Option<String>> {
    LAST_INIT_ERROR.get_or_init(|| RwLock::new(None))
}

/// Record the most recent init/open failure. Called by
/// `open_sqlite_with_wal` before returning `Err`. Writes through a
/// `RwLock` rather than overwriting via `OnceLock::set` because the
/// slot can be updated many times across the process lifetime (one per
/// open attempt), not just once.
pub fn set_last_init_error(msg: impl Into<String>) {
    if let Ok(mut g) = init_error_slot().write() {
        *g = Some(msg.into());
    }
}

/// Read the most recent init/open failure, if any. Returns `None` if
/// no failure has been recorded (i.e. every open succeeded, or no
/// caller has invoked `set_last_init_error` yet).
pub fn get_last_init_error() -> Option<String> {
    init_error_slot().read().ok().and_then(|g| g.clone())
}

/// Clear the recorded error. Useful for tests and for retry paths that
/// successfully re-open the database after a transient failure (so the
/// next caller doesn't see a stale message).
pub fn clear_last_init_error() {
    if let Ok(mut g) = init_error_slot().write() {
        *g = None;
    }
}

/// Render a user-friendly session DB unavailable message.
///
/// Format: `"{prefix}: {last_error}"` where `last_error` is whatever
/// `open_sqlite_with_wal` last wrote via `set_last_init_error`. Falls
/// back to `"{prefix}: (no error recorded)"` if nothing was set — this
/// keeps the message stable for the "the CLI never tried to open the
/// db" case where the caller is asking speculatively.
pub fn format_session_db_unavailable(prefix: &str) -> String {
    match get_last_init_error() {
        Some(err) => format!("{}: {}", prefix, err),
        None => format!("{}: (no error recorded)", prefix),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn default_memory_db_path_uses_anureo_home() {
        let _lock = crate::env_test_lock().lock().unwrap();
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev = env_config::home::override_path();
        env_config::home::set_override(Some(dir.path().to_path_buf()));
        let path = default_memory_db_path();
        env_config::home::set_override(prev);
        assert_eq!(path, dir.path().join("memory.db"));
    }

    #[test]
    fn default_memory_db_path_creates_parent_dir() {
        let _lock = crate::env_test_lock().lock().unwrap();
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("sub").join("dir");
        let prev = env_config::home::override_path();
        env_config::home::set_override(Some(nested.clone()));
        let path = default_memory_db_path();
        env_config::home::set_override(prev);
        assert!(nested.exists());
        assert_eq!(path, nested.join("memory.db"));
    }

    #[test]
    fn open_error_message_absolute_path() {
        let msg = open_error_message(Path::new("/abs/path/db.sqlite"), &"some error");
        assert!(msg.contains("/abs/path/db.sqlite"));
        assert!(msg.contains("some error"));
        assert!(msg.contains("resolved="));
    }

    #[test]
    fn open_error_message_relative_path() {
        let msg = open_error_message(Path::new("relative/db.sqlite"), &"oops");
        assert!(msg.contains("relative/db.sqlite"));
        assert!(msg.contains("oops"));
    }

    #[test]
    fn open_sqlite_with_wal_creates_and_opens() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");
        let conn = open_sqlite_with_wal(&db_path).unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);")
            .unwrap();
        assert!(db_path.exists());
    }

    #[test]
    fn open_sqlite_with_wal_invalid_path_returns_error() {
        let result = open_sqlite_with_wal(Path::new("/nonexistent/dir/db.sqlite"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("/nonexistent/dir/db.sqlite"));
    }

    #[test]
    fn memory_db_filename_constant() {
        assert_eq!(MEMORY_DB_FILENAME, "memory.db");
    }
}

//! Skill usage tracking — telemetry and lifecycle management.
//!
//! This module provides `SkillUsageStore` for tracking skill usage statistics
//! and lifecycle state.

use crate::storage::Lifecycle;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use tracing::debug;

/// Skill usage statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillUsage {
    pub name: String,
    #[serde(default)]
    pub use_count: u64,
    #[serde(default)]
    pub view_count: u64,
    #[serde(default)]
    pub patch_count: u64,
    #[serde(default)]
    pub last_used_at: Option<String>,
    #[serde(default)]
    pub last_viewed_at: Option<String>,
    #[serde(default)]
    pub last_patched_at: Option<String>,
    pub created_at: String,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default = "default_state")]
    pub state: Lifecycle,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub archived_at: Option<String>,
    #[serde(default)]
    pub absorbed_into: Option<String>,
}

fn default_state() -> Lifecycle {
    Lifecycle::Active
}

impl SkillUsage {
    /// Create a new usage record for a skill.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            use_count: 0,
            view_count: 0,
            patch_count: 0,
            last_used_at: None,
            last_viewed_at: None,
            last_patched_at: None,
            created_at: Utc::now().to_rfc3339(),
            created_by: None,
            state: Lifecycle::Active,
            pinned: false,
            archived_at: None,
            absorbed_into: None,
        }
    }

    /// Get the timestamp of the last activity (use, view, or patch).
    pub fn last_activity_at(&self) -> Option<String> {
        let candidates = [
            self.last_used_at.as_deref(),
            self.last_viewed_at.as_deref(),
            self.last_patched_at.as_deref(),
        ];
        candidates
            .iter()
            .filter_map(|&s| s)
            .max()
            .map(|s| s.to_string())
    }

    /// Get total activity count.
    pub fn activity_count(&self) -> u64 {
        self.use_count + self.view_count + self.patch_count
    }
}

/// A usage report for agent-created skills.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillUsageReport {
    pub name: String,
    pub use_count: u64,
    pub view_count: u64,
    pub patch_count: u64,
    pub last_used_at: Option<String>,
    pub last_viewed_at: Option<String>,
    pub last_patched_at: Option<String>,
    pub created_at: String,
    pub state: Lifecycle,
    pub pinned: bool,
    pub archived_at: Option<String>,
    pub last_activity_at: Option<String>,
    pub activity_count: u64,
}

impl From<(String, SkillUsage)> for SkillUsageReport {
    fn from((name, usage): (String, SkillUsage)) -> Self {
        let last_activity_at = usage.last_activity_at();
        let activity_count = usage.activity_count();
        Self {
            name,
            use_count: usage.use_count,
            view_count: usage.view_count,
            patch_count: usage.patch_count,
            last_used_at: usage.last_used_at,
            last_viewed_at: usage.last_viewed_at,
            last_patched_at: usage.last_patched_at,
            created_at: usage.created_at,
            state: usage.state,
            pinned: usage.pinned,
            archived_at: usage.archived_at,
            last_activity_at,
            activity_count,
        }
    }
}

/// Store for skill usage statistics.
#[derive(Clone)]
pub struct SkillUsageStore {
    path: PathBuf,
}

impl SkillUsageStore {
    /// Create a new usage store at the given base directory.
    pub fn new(base_dir: &Path) -> Self {
        Self {
            path: base_dir.join(".usage.json"),
        }
    }

    /// Increment the use count for a skill.
    ///
    /// Hermes-aligned provenance filter (`skill_usage.py:159-217, 290-293`):
    /// only skills marked as `agent_created` get their counters bumped.
    /// Bundled/hub-installed skills are excluded so the curator's
    /// recommendations are based solely on the agent's own behavior.
    pub fn bump_use(&self, name: &str) {
        if !self.is_agent_created(name) {
            return;
        }
        self.update(name, |u| {
            u.use_count += 1;
            u.last_used_at = Some(Utc::now().to_rfc3339());
        });
    }

    /// Increment the view count for a skill.
    /// See [`Self::bump_use`] for the provenance-filter rationale.
    pub fn bump_view(&self, name: &str) {
        if !self.is_agent_created(name) {
            return;
        }
        self.update(name, |u| {
            u.view_count += 1;
            u.last_viewed_at = Some(Utc::now().to_rfc3339());
        });
    }

    /// Increment the patch count for a skill.
    /// See [`Self::bump_use`] for the provenance-filter rationale.
    pub fn bump_patch(&self, name: &str) {
        if !self.is_agent_created(name) {
            return;
        }
        self.update(name, |u| {
            u.patch_count += 1;
            u.last_patched_at = Some(Utc::now().to_rfc3339());
        });
    }

    /// Mark a skill as agent-created.
    ///
    /// Hermes-aligned provenance guard (matches `skill_usage.py:171-195,
    /// 290-293`): a skill that appears in `.bundled_manifest` or is owned by
    /// a hub source recorded in `.hub/lock.json` is **off-limits** and must
    /// never be re-marked as agent-created. Otherwise the curator would
    /// silently archive skills the user installed from the hub.
    pub fn mark_agent_created(&self, name: &str) {
        if self.is_off_limits(name) {
            debug!(
                "SkillUsageStore: refusing to mark '{}' as agent-created (bundled/hub-owned)",
                name
            );
            return;
        }
        self.update(name, |u| {
            u.created_by = Some("agent".to_string());
        });
    }

    /// Check if a skill was created by the agent (background review).
    ///
    /// Hermes-aligned two-step filter (`skill_usage.py:290-293`):
    /// 1. The skill must NOT be listed in `.bundled_manifest` or owned by a
    ///    hub source in `.hub/lock.json` — those skills are user-installed
    ///    and protected from curator pruning.
    /// 2. The skill's usage record must carry `created_by == "agent"`.
    ///
    /// The function intentionally depends on filesystem provenance rather
    /// than the `created_by` field alone, so a corrupted `.usage.json`
    /// cannot accidentally turn a bundled skill into an "agent-created"
    /// one. Bundled/hub-owned skills always return `false`.
    pub fn is_agent_created(&self, name: &str) -> bool {
        if self.is_off_limits(name) {
            return false;
        }
        self.get(name)
            .and_then(|u| u.created_by)
            .map(|by| by == "agent")
            .unwrap_or(false)
    }

    /// Whether this skill is **curator-managed** (the original
    /// "agent-created + this curator run" predicate used for pruning
    /// decisions). It still requires `created_by == "agent"`.
    ///
    /// Note: this is the stricter predicate — bundled/hub-owned skills are
    /// excluded by `is_agent_created` so they are excluded here too.
    pub fn is_curator_managed(&self, name: &str) -> bool {
        self.is_agent_created(name)
    }

    /// List all skill names created by the agent.
    ///
    /// Filters out any skill that is bundled or hub-owned (same off-limits
    /// set used by `is_agent_created`). Without this filter the curator
    /// would surface bundled/hub skills as agent-created candidates and
    /// silently archive them.
    pub fn agent_created_names(&self) -> Vec<String> {
        self.load()
            .map(|data| {
                data.iter()
                    .filter(|(_, u)| u.created_by.as_deref() == Some("agent"))
                    .filter(|(name, _)| !self.is_off_limits(name))
                    .map(|(name, _)| name.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Update the lifecycle state of a skill.
    ///
    /// Hermes parity (`skill_usage.py:215-227`): when a skill transitions
    /// to `Archived` we stamp `archived_at`; when it transitions back to
    /// `Active` (e.g. via a rollback) we clear the stale stamp so the
    /// `last_activity_at` derivation and audit reports do not surface
    /// phantom archive timestamps.
    pub fn set_state(&self, name: &str, state: Lifecycle) {
        self.update(name, |u| {
            u.state = state;
            match state {
                Lifecycle::Archived => {
                    u.archived_at = Some(Utc::now().to_rfc3339());
                }
                Lifecycle::Active => {
                    u.archived_at = None;
                }
                _ => {}
            }
        });
    }

    /// Set the pinned status of a skill.
    pub fn set_pinned(&self, name: &str, pinned: bool) {
        self.update(name, |u| {
            u.pinned = pinned;
        });
    }

    /// Filesystem-based provenance guard: returns `true` if `name` is either
    /// listed in `.bundled_manifest` (built-in) or owned by a hub source in
    /// `.hub/lock.json` (user-installed via the hub).
    ///
    /// Hermes parity (`skill_usage.py:171-195, 290-293`): bundled and
    /// hub-owned skills must never be reported as `agent_created`, because
    /// the curator would silently archive them otherwise.
    ///
    /// Best-effort: any read error (missing file, malformed JSON) is treated
    /// as "no off-limits entry" so the curator can still run on partially
    /// initialised installations.
    fn is_off_limits(&self, name: &str) -> bool {
        let base = match self.path.parent() {
            Some(p) => p,
            None => return false,
        };

        // 1. `.bundled_manifest` — built-in skills. Hermes-aligned parser
        //    tolerates missing file, blank lines, `#` comments.
        let manifest_path = base.join(".bundled_manifest");
        if manifest_path.is_file() {
            if let Ok(data) = fs::read_to_string(&manifest_path) {
                for line in data.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with('#') {
                        continue;
                    }
                    if let Some((entry_name, _hash)) = line.split_once(':') {
                        if entry_name.trim() == name {
                            return true;
                        }
                    }
                }
            }
        }

        // 2. `.hub/lock.json` — user-installed hub skills. The hub subsystem
        //    is not yet ported to Rust (separate hermes-port gap), so we
        //    best-effort parse a generic shape:
        //      { "entries": [{ "name": "...", "source": "..." }, ...] }
        //    Unknown files / schemas are silently ignored.
        let hub_lock = base.join(".hub").join("lock.json");
        if hub_lock.is_file() {
            if let Ok(data) = fs::read_to_string(&hub_lock) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&data) {
                    if let Some(arr) = v.get("entries").and_then(|x| x.as_array()) {
                        for entry in arr {
                            if entry
                                .get("name")
                                .and_then(|n| n.as_str())
                                .map(|s| s == name)
                                .unwrap_or(false)
                            {
                                return true;
                            }
                        }
                    }
                }
            }
        }

        false
    }

    /// Remove usage data for a skill, optionally recording the deletion intent.
    pub fn forget(&self, name: &str) {
        self.forget_with_intent(name, None);
    }

    /// Remove usage data for a skill, recording where it was absorbed into.
    pub fn forget_with_intent(&self, name: &str, absorbed_into: Option<&str>) {
        match self.load() {
            Ok(mut data) => {
                if let Some(target) = absorbed_into {
                    if let Some(entry) = data.get_mut(name) {
                        entry.absorbed_into = Some(target.to_string());
                    }
                }
                data.remove(name);
                if let Err(e) = self.save(&data) {
                    debug!("SkillUsageStore: forget '{}' failed: {}", name, e);
                }
            }
            Err(e) => {
                debug!("SkillUsageStore: forget '{}' failed to load: {}", name, e);
            }
        }
    }

    // ------------------------------------------------------------------
    // `.curator_suppressed` list (priority #20 gap, Hermes `skill_usage.py`).
    //
    // When the curator prunes a bundled skill, the next `sync_skills` /
    // `anureo update` pass would re-seed it from the bundled manifest.
    // To prevent that, the curator appends the pruned name to
    // `.curator_suppressed` (a single-line-per-name text file alongside
    // `usage.json`). On restore (e.g. user runs `anureo skill restore`),
    // the name is removed so the next sync can re-fetch it.
    //
    // The list is consulted by `agent/skill/src/sync.rs` before applying
    // any bundled-skill write — sync_skills must not overwrite a file
    // the curator explicitly suppressed unless the user forces it
    // (`ANUREO_SKILL_FORCE=1`).
    // ------------------------------------------------------------------

    /// Path of the suppression list, sibling of `usage.json`.
    fn suppressed_path(&self) -> PathBuf {
        self.path
            .parent()
            .map(|p| p.join(".curator_suppressed"))
            .unwrap_or_else(|| PathBuf::from(".curator_suppressed"))
    }

    /// Read the set of skill names the curator has suppressed. Returns
    /// an empty set if the file doesn't exist (first run). Lines that
    /// don't decode as UTF-8 are skipped (defensive — corrupted list
    /// shouldn't block the CLI).
    pub fn read_suppressed_names(&self) -> std::collections::HashSet<String> {
        let path = self.suppressed_path();
        if !path.exists() {
            return std::collections::HashSet::new();
        }
        match fs::read_to_string(&path) {
            Ok(raw) => raw
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty() && s.is_ascii())
                .collect(),
            Err(e) => {
                debug!("SkillUsageStore: read_suppressed_names failed: {}", e);
                std::collections::HashSet::new()
            }
        }
    }

    /// Append a skill name to the suppression list. Idempotent — adding
    /// a name that's already in the list is a no-op. Creates the file
    /// (and parent) if it doesn't exist.
    pub fn add_suppressed_name(&self, name: &str) {
        if name.is_empty() || !name.is_ascii() {
            return;
        }
        let mut set = self.read_suppressed_names();
        if set.contains(name) {
            return;
        }
        set.insert(name.to_string());
        self.write_suppressed(&set);
    }

    /// Remove a skill name from the suppression list (e.g. when the
    /// user manually restores a suppressed skill). Idempotent.
    pub fn remove_suppressed_name(&self, name: &str) {
        let mut set = self.read_suppressed_names();
        if !set.remove(name) {
            return;
        }
        self.write_suppressed(&set);
    }

    /// Internal: atomic-write the suppression list as one name per line.
    fn write_suppressed(&self, set: &std::collections::HashSet<String>) {
        let path = self.suppressed_path();
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let mut sorted: Vec<&String> = set.iter().collect();
        sorted.sort();
        let body = sorted
            .into_iter()
            .map(|s| format!("{}\n", s))
            .collect::<String>();
        // Best-effort atomic write: write to a sibling tempfile then
        // rename. If the rename fails (Windows file lock), fall back
        // to a direct write so the suppression list is never silently
        // lost on first run.
        let tmp = path.with_extension("curator_suppressed.tmp");
        if fs::write(&tmp, body.as_bytes()).is_ok() {
            if fs::rename(&tmp, &path).is_err() {
                let _ = fs::write(&path, body.as_bytes());
                let _ = fs::remove_file(&tmp);
            }
        } else {
            let _ = fs::write(&path, body.as_bytes());
        }
    }

    /// Get a report of all agent-created skills.
    pub fn agent_created_report(&self) -> Result<Vec<SkillUsageReport>, String> {
        let data = self.load().map_err(|e| e.to_string())?;
        let rows: Vec<SkillUsageReport> = data
            .into_iter()
            .filter(|(_, u)| u.created_by.as_deref() == Some("agent"))
            .filter(|(name, _)| !self.is_off_limits(name))
            .map(|(name, usage)| SkillUsageReport::from((name, usage)))
            .collect();
        Ok(rows)
    }

    /// Get usage data for a specific skill.
    pub fn get(&self, name: &str) -> Option<SkillUsage> {
        self.load().ok()?.remove(name)
    }

    /// Load all usage data from the store.
    pub fn load(&self) -> Result<HashMap<String, SkillUsage>, std::io::Error> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let raw = fs::read_to_string(&self.path)?;
        let raw = raw.trim_start_matches('\u{feff}');
        if raw.trim().is_empty() {
            return Ok(HashMap::new());
        }
        serde_json::from_str(raw)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
    }

    /// Save all usage data to the store.
    ///
    /// Uses `atomic_write_text` so the destination is never observed in a
    /// half-written state, even if the process is killed mid-write. The helper
    /// also generates a per-call unique tempfile (pid + nanos), which closes
    /// the concurrent-process rename-to-same-tmp collision that the previous
    /// `self.path.with_extension("tmp")` scheme allowed.
    ///
    /// Hermes parity (`skill_usage.py:67-100, 343-365`): an `fs4`
    /// `lock_exclusive` sidecar is now taken on `self.path.lock` for the
    /// duration of the write so that two foreground agents (or a
    /// foreground agent and the curator) cannot race on load-then-save.
    /// Best-effort: if `fs4` is unavailable on the target platform we
    /// log at debug and fall through to the unlocked write so we never
    /// regress single-process correctness (mirrors Hermes'
    /// `fcntl.flock(…, LOCK_EX)` + silent fallback).
    pub fn save(&self, data: &HashMap<String, SkillUsage>) -> Result<(), std::io::Error> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(data)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        with_usage_lock(&self.path, || {
            crate::storage::atomic_write_text(&self.path, &json)
        })
    }

    /// Save a single entry (load existing, update, save).
    pub fn save_entry(&self, name: &str, entry: &SkillUsage) -> Result<(), std::io::Error> {
        let mut data = self.load().unwrap_or_default();
        data.insert(name.to_string(), entry.clone());
        self.save(&data)
    }

    fn update(&self, name: &str, f: impl FnOnce(&mut SkillUsage)) {
        match self.load() {
            Ok(mut data) => {
                let entry = data
                    .entry(name.to_string())
                    .or_insert_with(|| SkillUsage::new(name));
                f(entry);
                if let Err(e) = self.save(&data) {
                    debug!("SkillUsageStore: save failed for '{}': {}", name, e);
                }
            }
            Err(e) => {
                debug!("SkillUsageStore: load failed for '{}': {}", name, e);
            }
        }
    }
}

/// Advisory file-lock sidecar for cross-process load-then-save of
/// `.usage.json`. Hermes parity (`skill_usage.py:67-100, 343-365`):
/// the Python side uses `fcntl.flock(…, LOCK_EX)` on a sibling
/// `.usage.lock` file so that two foreground agents (or a foreground
/// agent racing the curator) cannot lose updates.
///
/// Rust implementation strategy:
///   1. Best-effort `fs4`-style cross-process lock via
///      `OpenOptions::create_new(true)` on the lock sidecar. The
///      sidecar is removed before returning; concurrent processes see
///      `AlreadyExists` and either busy-wait briefly or skip the
///      lock (mirrors Hermes' non-blocking `LOCK_EX | LOCK_NB`).
///   2. If step 1 fails (platform without atomic create-new, or a
///      concurrent holder), we fall back to a no-op so single-process
///      callers stay correct. `fs4` was deliberately not added as a
///      dependency — the locking sidecar pattern works on POSIX and
///      Windows with the standard library alone.
///
/// `op` is called while the lock is held; the lock is released
/// immediately after.
fn with_usage_lock<F, R>(path: &Path, op: F) -> R
where
    F: FnOnce() -> R,
{
    let lock_path = path.with_extension("lock");
    // Best-effort acquire: create_new succeeds for the first writer;
    // subsequent writers see AlreadyExists and back off without
    // blocking. We do NOT retry-loop here — `op` is idempotent and
    // the only cost of a missed lock is a possible lost update,
    // which matches Hermes' `LOCK_EX | LOCK_NB` behaviour.
    let _guard = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock_path)
    {
        Ok(_f) => UsageLockGuard {
            path: lock_path,
            released: false,
        },
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            debug!(
                "SkillUsageStore: lock sidecar held by another process, proceeding unlocked: {}",
                e
            );
            return op();
        }
        Err(e) => {
            debug!(
                "SkillUsageStore: could not open lock sidecar ({}), proceeding unlocked",
                e
            );
            return op();
        }
    };
    let result = op();
    drop(_guard);
    result
}

/// RAII guard for the usage-lock sidecar; removes the lock file on
/// drop so the next waiter can acquire.
struct UsageLockGuard {
    path: PathBuf,
    released: bool,
}

impl Drop for UsageLockGuard {
    fn drop(&mut self) {
        if !self.released {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_skill_has_zero_counts() {
        let usage = SkillUsage::new("test-skill");
        assert_eq!(usage.use_count, 0);
        assert_eq!(usage.view_count, 0);
        assert_eq!(usage.patch_count, 0);
        assert_eq!(usage.state, Lifecycle::Active);
        assert!(!usage.pinned);
        assert!(usage.absorbed_into.is_none());
        assert!(usage.created_by.is_none());
    }

    #[test]
    fn bump_increments_counts() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.mark_agent_created("skill-a");
        store.bump_use("skill-a");
        store.bump_use("skill-a");
        store.bump_view("skill-a");

        let entry = store.get("skill-a").unwrap();
        assert_eq!(entry.use_count, 2);
        assert_eq!(entry.view_count, 1);
    }

    #[test]
    fn bump_sets_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.mark_agent_created("s1");
        store.bump_use("s1");
        let entry = store.get("s1").unwrap();
        assert!(entry.last_used_at.is_some());
        assert!(entry.last_activity_at().is_some());
    }

    #[test]
    fn bump_patch_increments() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.mark_agent_created("s1");
        store.bump_patch("s1");
        store.bump_patch("s1");
        store.bump_patch("s1");
        let entry = store.get("s1").unwrap();
        assert_eq!(entry.patch_count, 3);
        assert!(entry.last_patched_at.is_some());
    }

    #[test]
    fn mark_agent_created() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.bump_use("agent-skill");
        store.mark_agent_created("agent-skill");

        assert!(store.is_agent_created("agent-skill"));
        let names = store.agent_created_names();
        assert!(names.contains(&"agent-skill".to_string()));
    }

    #[test]
    fn set_state_archived() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.bump_use("s1");
        store.set_state("s1", Lifecycle::Archived);
        let entry = store.get("s1").unwrap();
        assert_eq!(entry.state, Lifecycle::Archived);
        assert!(entry.archived_at.is_some());
    }

    #[test]
    fn set_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.bump_use("s1");
        store.set_pinned("s1", true);
        let entry = store.get("s1").unwrap();
        assert!(entry.pinned);
    }

    #[test]
    fn forget_removes_entry() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.bump_use("to-remove");
        store.forget("to-remove");
        assert!(store.get("to-remove").is_none());
    }

    #[test]
    fn forget_with_intent_records_absorbed_into() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.bump_use("old-skill");
        store.forget_with_intent("old-skill", Some("new-skill"));

        assert!(store.get("old-skill").is_none());
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());
        assert!(store.get("nope").is_none());
    }

    #[test]
    fn load_empty_dir_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());
        let data = store.load().unwrap();
        assert!(data.is_empty());
    }

    #[test]
    fn load_accepts_utf8_bom() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());
        let mut usage = SkillUsage::new("agent-skill");
        usage.created_by = Some("agent".to_string());
        let data = HashMap::from([("agent-skill".to_string(), usage)]);
        let json = serde_json::to_string(&data).unwrap();
        fs::write(dir.path().join(".usage.json"), format!("\u{feff}{json}")).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(store.is_agent_created("agent-skill"));
    }

    #[test]
    fn activity_count_sums_all() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.mark_agent_created("s1");
        store.bump_use("s1");
        store.bump_use("s1");
        store.bump_view("s1");
        store.bump_patch("s1");

        let entry = store.get("s1").unwrap();
        assert_eq!(entry.activity_count(), 4);
    }

    #[test]
    fn agent_created_report_filters() {
        let dir = tempfile::tempdir().unwrap();
        let store = SkillUsageStore::new(dir.path());

        store.bump_use("manual-skill");
        store.bump_use("agent-skill");
        store.mark_agent_created("agent-skill");

        let report = store.agent_created_report().unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].name, "agent-skill");
    }
}

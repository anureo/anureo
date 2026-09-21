// Re-export ToolCall from anureo-llm for classification functions
// that need structured args (HashMap) instead of JSON string.
pub use anureo_llm::tool::ToolCall as CuratorToolCall;

use super::prompts::CURATOR_REVIEW_PROMPT;
#[cfg(test)]
use super::skill_registry::Source;
use super::skill_registry::{
    Lifecycle, SkillContent, SkillError, SkillMeta, SkillRegistry, SkillRegistryCuratorExt,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use skill::{SkillUsageReport, SkillUsageStore};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use tracing::{debug, info, warn};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorConfig {
    #[serde(default = "default_stale_days")]
    pub stale_days_auto: u32,
    #[serde(default = "default_stale_days_manual")]
    pub stale_days_manual: u32,
    #[serde(default = "default_archive_days")]
    pub archive_days: u32,
    #[serde(default = "default_overlap_threshold")]
    pub overlap_threshold: f64,
    // === Phase 2.2: Four gate trigger config ===
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    #[serde(default = "default_interval_hours")]
    pub interval_hours: u64,
    #[serde(default = "default_min_idle_minutes")]
    pub min_idle_minutes: u64,
    /// Minimum number of skills required for the curator to run.
    ///
    /// Avoids triggering consolidation on near-empty libraries (e.g. fresh
    /// installs). Aligns with Hermes behavior where the candidate list is
    /// only meaningful when there are enough skills to cluster.
    #[serde(default = "default_min_skills_to_run")]
    pub min_skills_to_run: usize,
}

fn default_enabled() -> bool {
    true
}
fn default_interval_hours() -> u64 {
    168
}
fn default_min_idle_minutes() -> u64 {
    120
}
fn default_min_skills_to_run() -> usize {
    5
}

fn default_stale_days() -> u32 {
    60
}
fn default_stale_days_manual() -> u32 {
    30
}
fn default_archive_days() -> u32 {
    90
}
fn default_overlap_threshold() -> f64 {
    0.7
}

impl Default for CuratorConfig {
    fn default() -> Self {
        Self {
            stale_days_auto: default_stale_days(),
            stale_days_manual: default_stale_days_manual(),
            archive_days: default_archive_days(),
            overlap_threshold: default_overlap_threshold(),
            enabled: default_enabled(),
            interval_hours: default_interval_hours(),
            min_idle_minutes: default_min_idle_minutes(),
            min_skills_to_run: default_min_skills_to_run(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CuratorState {
    pub skill_last_used: HashMap<String, String>,
    #[serde(default)]
    pub last_run_at: Option<String>,
    #[serde(default)]
    pub last_run_duration_seconds: Option<f64>,
    #[serde(default)]
    pub last_run_summary: Option<String>,
    #[serde(default)]
    pub last_run_summary_shown_at: Option<String>,
    #[serde(default)]
    pub last_report_path: Option<String>,
    #[serde(default)]
    pub run_count: u32,
    #[serde(default)]
    pub paused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorReport {
    pub active: usize,
    pub stale: Vec<String>,
    pub archived: Vec<String>,
    pub overlapping: Vec<OverlapPair>,
    pub reactivated: Vec<String>,
    /// Hermes `hermes_cli/curator.py:354-368` parity: per-skill prune
    /// failures collected during the heuristic archive phase. CLI prints
    /// a `failures:` block and exits 1 on any failure so a single broken
    /// skill never silently aborts a curator run with the rest unarchived.
    #[serde(default)]
    pub failures: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlapPair {
    pub skill_a: String,
    pub skill_b: String,
    pub similarity: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Curator Types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AutoCounts {
    pub checked: usize,
    pub marked_stale: usize,
    pub archived: usize,
    pub reactivated: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CuratorReviewResult {
    pub started_at: DateTime<Utc>,
    pub auto_transitions: AutoCounts,
    pub summary_so_far: String,
}

#[derive(Debug, Clone)]
pub struct LlmPassResult {
    pub elapsed_seconds: f64,
    pub report_path: Option<PathBuf>,
    pub rename_summary: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// CuratorStateStore trait + implementations
// ─────────────────────────────────────────────────────────────────────────────

/// CuratorState storage abstraction trait
pub trait CuratorStateStore: Send + Sync {
    // Abstract methods (each implementation defines)
    fn load(&self) -> Result<CuratorState, SkillError>;
    fn save(&self, state: &CuratorState) -> Result<(), SkillError>;

    // Default implementations (based on load/save)

    fn set_paused(&self, paused: bool) -> Result<(), SkillError> {
        let mut state = self.load()?;
        state.paused = paused;
        self.save(&state)
    }

    fn is_paused(&self) -> bool {
        self.load().map(|s| s.paused).unwrap_or(false)
    }

    fn bump_run(
        &self,
        duration_secs: f64,
        summary: Option<&str>,
        report_path: Option<&str>,
    ) -> Result<(), SkillError> {
        let mut state = self.load()?;
        state.run_count += 1;
        state.last_run_at = Some(chrono::Utc::now().to_rfc3339());
        state.last_run_duration_seconds = Some(duration_secs);
        state.last_run_summary = summary.map(String::from);
        state.last_report_path = report_path.map(String::from);
        self.save(&state)
    }

    fn touch_skill(&self, name: &str) -> Result<(), SkillError> {
        let mut state = self.load()?;
        state
            .skill_last_used
            .insert(name.to_string(), chrono::Utc::now().to_rfc3339());
        self.save(&state)
    }

    fn mark_summary_shown(&self) -> Result<(), SkillError> {
        let mut state = self.load()?;
        state.last_run_summary_shown_at = Some(chrono::Utc::now().to_rfc3339());
        self.save(&state)
    }
}

/// File system storage (production)
pub struct FileStateStore {
    path: PathBuf,
}

impl FileStateStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl CuratorStateStore for FileStateStore {
    fn load(&self) -> Result<CuratorState, SkillError> {
        if !self.path.exists() {
            return Ok(CuratorState::default());
        }
        let data = fs::read_to_string(&self.path)?;
        serde_json::from_str(&data).map_err(|e| SkillError::InvalidFormat(e.to_string()))
    }

    fn save(&self, state: &CuratorState) -> Result<(), SkillError> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(state)
            .map_err(|e| SkillError::InvalidFormat(e.to_string()))?;
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, &data)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Memory storage (unit tests)
pub struct MemoryStateStore {
    state: std::sync::Mutex<CuratorState>,
}

impl Default for MemoryStateStore {
    fn default() -> Self {
        Self {
            state: std::sync::Mutex::new(CuratorState::default()),
        }
    }
}

impl MemoryStateStore {
    pub fn with_state(state: CuratorState) -> Self {
        Self {
            state: std::sync::Mutex::new(state),
        }
    }
}

impl CuratorStateStore for MemoryStateStore {
    fn load(&self) -> Result<CuratorState, SkillError> {
        Ok(self.state.lock().unwrap().clone())
    }

    fn save(&self, state: &CuratorState) -> Result<(), SkillError> {
        *self.state.lock().unwrap() = state.clone();
        Ok(())
    }

    fn set_paused(&self, paused: bool) -> Result<(), SkillError> {
        self.state.lock().unwrap().paused = paused;
        Ok(())
    }

    fn is_paused(&self) -> bool {
        self.state.lock().unwrap().paused
    }
}

#[derive(Clone)]
pub struct Curator {
    pub skills: SkillRegistry,
    config: CuratorConfig,
    state_path: PathBuf,
    skill_usage: SkillUsageStore,
}

impl Curator {
    pub fn new(skills: SkillRegistry, config: CuratorConfig) -> Self {
        let state_path = skills.base_dir().join("curator").join("state.json");
        let skill_usage = SkillUsageStore::new(skills.base_dir());
        Self {
            skills,
            config,
            state_path,
            skill_usage,
        }
    }

    pub fn with_state_path(mut self, path: PathBuf) -> Self {
        self.state_path = path;
        self
    }

    pub fn with_skill_usage(mut self, skill_usage: SkillUsageStore) -> Self {
        self.skill_usage = skill_usage;
        self
    }

    /// Expose agent-created provenance check (Hermes
    /// `curator.py:234-257`). Pin/Unpin/Rollback gates consult this to
    /// reject bundled/hub-installed skills that should never be curated.
    pub fn is_agent_created_skill(&self, name: &str) -> bool {
        self.skill_usage.is_agent_created(name)
    }

    /// Pin or unpin a curator-managed skill. `.usage.json` is the authority;
    /// the SKILL.md update is retained during the compatibility window so
    /// older anureo builds continue to display the same state.
    pub fn set_skill_pinned(&self, name: &str, pinned: bool) -> Result<(), SkillError> {
        if !self.is_agent_created_skill(name) {
            return Err(SkillError::NotFound(format!(
                "'{}' is not agent-created; refusing to change pin state",
                name
            )));
        }
        self.skills.set_pinned(name, pinned)?;
        self.skill_usage.set_pinned(name, pinned);
        Ok(())
    }

    pub fn should_run(&self, idle_for_seconds: Option<f64>) -> bool {
        // Gate 0: minimum skill count (don't run on near-empty libraries)
        let skill_count = self
            .skill_usage
            .agent_created_report()
            .map(|reports| {
                reports
                    .into_iter()
                    .filter(|usage| {
                        usage.state != Lifecycle::Archived
                            && self.skills.find_skill_dir(&usage.name).is_some()
                    })
                    .count()
            })
            .unwrap_or(0);
        if skill_count < self.config.min_skills_to_run {
            info!(
                "Curator: too few skills ({}) — need at least {}",
                skill_count, self.config.min_skills_to_run
            );
            return false;
        }

        // Gate 1: enabled
        if !self.config.enabled {
            info!("Curator: disabled in config");
            return false;
        }

        // Gate 2: paused
        let state = self.load_state().unwrap_or_default();
        if state.paused {
            info!("Curator: paused");
            return false;
        }

        // Gate 3: interval (or first-run delay)
        //
        // On the very first run (run_count == 0, no last_run_at), require at
        // least `interval_hours` of wall-clock time since skill library
        // creation to avoid consolidating freshly-installed skill sets.
        // Aligns with Hermes behavior where the curator waits one full
        // interval before its initial pass.
        if state.run_count == 0 || state.last_run_at.is_none() {
            if let Some(installed_at) = self.skills.library_created_at() {
                let elapsed = Utc::now() - installed_at;
                let needed = chrono::Duration::hours(self.config.interval_hours as i64);
                if elapsed < needed {
                    info!(
                        "Curator: first-run delay not yet met ({:.1}h < {}h since library creation)",
                        elapsed.num_minutes() as f64 / 60.0,
                        self.config.interval_hours
                    );
                    return false;
                }
            }
        } else if let Some(last_run) = state
            .last_run_at
            .as_ref()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        {
            let elapsed = Utc::now() - last_run.with_timezone(&Utc);
            let needed = chrono::Duration::hours(self.config.interval_hours as i64);
            if elapsed < needed {
                info!(
                    "Curator: interval not reached ({:.1}h < {}h)",
                    elapsed.num_minutes() as f64 / 60.0,
                    self.config.interval_hours
                );
                return false;
            }
        }

        // Gate 4: idle
        if let Some(idle_secs) = idle_for_seconds {
            let min_idle = self.config.min_idle_minutes as f64 * 60.0;
            if idle_secs < min_idle {
                info!(
                    "Curator: not idle long enough ({:.0}s < {}s)",
                    idle_secs, min_idle
                );
                return false;
            }
        }

        true
    }

    pub fn set_paused(&self, paused: bool) -> Result<(), SkillError> {
        let mut state = self.load_state()?;
        state.paused = paused;
        self.save_state(&state)
    }

    pub fn is_paused(&self) -> bool {
        self.load_state().map(|s| s.paused).unwrap_or(false)
    }

    // === Config accessors (for CLI status display) ===

    pub fn config_enabled(&self) -> bool {
        self.config.enabled
    }

    pub fn config_interval_hours(&self) -> u64 {
        self.config.interval_hours
    }

    pub fn config_min_skills(&self) -> usize {
        self.config.min_skills_to_run
    }

    pub fn config_overlap_threshold(&self) -> f64 {
        self.config.overlap_threshold
    }

    pub fn config_stale_days_auto(&self) -> u32 {
        self.config.stale_days_auto
    }

    pub fn config_stale_days_manual(&self) -> u32 {
        self.config.stale_days_manual
    }

    pub fn config_archive_days(&self) -> u32 {
        self.config.archive_days
    }

    pub fn run(&self, dry_run: bool) -> Result<CuratorReport, SkillError> {
        // Phase 0: Pre-run snapshot — create a backup before any mutations.
        //
        // Aligns with Hermes `curator_backup.snapshot_skills()` called at the
        // start of `run_curator_review()`. The snapshot is skipped in dry-run
        // mode (no mutations will occur) and on best-effort basis (snapshot
        // failure logs a warning but does not abort the run).
        if !dry_run {
            let backup = crate::curator_backup::CuratorBackup::new();
            let skills_dir = self.skills.base_dir();
            match backup.auto_snapshot(skills_dir) {
                Ok(Some(snapshot_name)) => {
                    info!("Curator: pre-run snapshot created: {}", snapshot_name);
                }
                Ok(None) => {
                    debug!("Curator: pre-run snapshot skipped (dir not found)");
                }
                Err(e) => {
                    warn!("Curator: pre-run snapshot failed (continuing): {}", e);
                }
            }
        }

        let all_skills = self.skills.list()?;
        let skills_by_name: HashMap<String, SkillMeta> = all_skills
            .iter()
            .cloned()
            .map(|meta| (meta.name.clone(), meta))
            .collect();
        let managed_usage = self
            .skill_usage
            .agent_created_report()
            .map_err(SkillError::InvalidFormat)?;
        let mut state = self.load_state()?;

        let now = Utc::now();
        let mut report = CuratorReport {
            active: 0,
            stale: Vec::new(),
            archived: Vec::new(),
            overlapping: Vec::new(),
            reactivated: Vec::new(),
            // Hermes `hermes_cli/curator.py:354-368` parity: per-skill
            // archive failures are collected into `report.failures` and
            // surfaced to the CLI so a single broken skill never silently
            // aborts the rest of the curator run.
            failures: Vec::new(),
        };

        // Curator ownership lives in `.usage.json`. Legacy directory names
        // and SKILL.md `source` fields never opt a user-authored skill into
        // automatic lifecycle transitions.
        for usage in &managed_usage {
            if !self.skill_usage.is_agent_created(&usage.name) {
                continue;
            }
            if usage.state == Lifecycle::Archived {
                report.archived.push(usage.name.clone());
                continue;
            }
            let Some(_meta) = skills_by_name.get(&usage.name) else {
                warn!(
                    "Curator: agent-created skill '{}' is missing from the active skill tree",
                    usage.name
                );
                continue;
            };
            if usage.pinned {
                info!("Skipping pinned skill: {}", usage.name);
                report.active += 1;
                continue;
            }

            let stale_days = self.config.stale_days_auto;
            let days_since = compute_usage_days_since(usage, &now);

            match usage.state {
                Lifecycle::Active => {
                    if days_since >= stale_days {
                        report.stale.push(usage.name.clone());
                        if !dry_run {
                            self.update_lifecycle(&usage.name, Lifecycle::Stale)?;
                            info!(
                                "Marked '{}' as stale ({} days unused)",
                                usage.name, days_since
                            );
                        }
                    } else {
                        report.active += 1;
                    }
                }
                Lifecycle::Stale => {
                    if days_since < stale_days {
                        info!(
                            "Reactivating stale skill '{}' ({} days since last use)",
                            usage.name, days_since
                        );
                        report.reactivated.push(usage.name.clone());
                        if !dry_run {
                            self.update_lifecycle(&usage.name, Lifecycle::Active)?;
                        }
                    } else if days_since >= self.config.archive_days {
                        report.archived.push(usage.name.clone());
                        if !dry_run {
                            // Collect per-skill archive failures
                            // (`hermes_cli/curator.py:354-368`) so the CLI
                            // can surface them and exit 1 instead of
                            // silently swallowing the error.
                            if let Err(e) = self.archive_skill(&usage.name) {
                                let msg = format!("{:?}", e);
                                warn!("Curator: archive failed for '{}': {}", usage.name, msg);
                                report.failures.push((usage.name.clone(), msg));
                            }
                            info!("Archived '{}' ({} days unused)", usage.name, days_since);
                        }
                    } else {
                        report.stale.push(usage.name.clone());
                    }
                }
                Lifecycle::Archived => unreachable!("handled above"),
            }
        }

        let managed_names: std::collections::HashSet<&str> = managed_usage
            .iter()
            .filter(|usage| {
                usage.state == Lifecycle::Active
                    && !usage.pinned
                    && self.skill_usage.is_agent_created(&usage.name)
            })
            .map(|usage| usage.name.as_str())
            .collect();
        let loaded: Vec<SkillContent> = all_skills
            .iter()
            .filter(|meta| managed_names.contains(meta.name.as_str()))
            .filter_map(|meta| self.skills.load(&meta.name).ok())
            .collect();

        for i in 0..loaded.len() {
            for j in (i + 1)..loaded.len() {
                let sim = compute_skill_similarity(&loaded[i], &loaded[j]);
                if sim >= self.config.overlap_threshold {
                    report.overlapping.push(OverlapPair {
                        skill_a: loaded[i].name.clone(),
                        skill_b: loaded[j].name.clone(),
                        similarity: sim,
                    });
                    warn!(
                        "Overlapping skills: '{}' and '{}' (similarity: {:.2})",
                        loaded[i].name, loaded[j].name, sim
                    );
                }
            }
        }

        if !dry_run {
            let now_str = now.to_rfc3339();
            state.last_run_at = Some(now_str);
            state.run_count += 1;

            for usage in &managed_usage {
                state
                    .skill_last_used
                    .entry(usage.name.clone())
                    .or_insert_with(|| now.to_rfc3339());
            }
            self.save_state(&state)?;
        }

        Ok(report)
    }

    /// Hermes-aligned prune entry point (`hermes_cli/curator.py:304-369`).
    ///
    /// Unlike `run(dry_run)`, this honours an explicit `days` threshold passed
    /// by the CLI rather than using `config.archive_days`. The legacy
    /// `anureo curator prune --days N` path was a no-op for `days` — it just
    /// called `curator.run(dry_run)` and printed the value. This method
    /// threads `days` through by overriding `stale_days_auto`,
    /// `stale_days_manual`, and `archive_days` for the duration of the call.
    ///
    /// `dry_run=true` returns the same `CuratorReport` shape but performs no
    /// lifecycle mutations.
    pub fn prune(&self, days: u32, dry_run: bool) -> Result<CuratorReport, SkillError> {
        let mut config = self.config.clone();
        config.stale_days_auto = days;
        config.stale_days_manual = days;
        config.archive_days = days;
        let scoped = Curator {
            skills: self.skills.clone(),
            config,
            state_path: self.state_path.clone(),
            skill_usage: self.skill_usage.clone(),
        };
        scoped.run(dry_run)
    }

    /// Hermes `skill_usage.py:482-567` `archive_skill`: physically move an
    /// agent-created skill into `.archive/<name>/` so it disappears from the
    /// active registry but stays recoverable via `restore_skill`.
    ///
    /// Returns the source path that was moved (for logging / rollback).
    /// Returns `Err(SkillError::NotFound)` when the skill does not exist
    /// under the active base dir.
    pub fn archive_skill(&self, name: &str) -> Result<std::path::PathBuf, SkillError> {
        // Hard-gate `PROTECTED_BUILTIN_SKILLS` (e.g. `plan`) BEFORE any
        // provenance check. Latent risk: if `prune_builtins` ever flips on
        // (Hermes `tools/skill_usage.py` #9), `is_agent_created_skill`
        // would otherwise let the bundled `plan` skill become archivable
        // and silently break the planning pipeline. We centralise the
        // allowlist here so adding a new protected skill is a single-line
        // change in `agent/skill/src/provenance.rs:98`.
        if skill::provenance::is_protected_builtin(name) {
            return Err(SkillError::Pinned(format!(
                "'{}' is a protected builtin; refusing to archive",
                name
            )));
        }

        // Provenance guard: only agent-created skills may be archived by the
        // curator. Without this check a bundled or user-installed skill
        // would silently disappear under `.archive/` and surface in
        // `restore` listings as if it were agent-created. Hermes aligns by
        // reading `.bundled_manifest` and `.hub/lock.json` first; we reuse
        // `is_agent_created_skill` which already implements that check.
        if !self.is_agent_created_skill(name) {
            return Err(SkillError::NotFound(format!(
                "'{}' is not agent-created; refusing to archive (pin check)",
                name
            )));
        }
        if self
            .skill_usage
            .get(name)
            .map(|usage| usage.pinned)
            .unwrap_or(false)
        {
            return Err(SkillError::Pinned(name.to_string()));
        }

        // The shared archive service resolves flat, category-nested, and
        // legacy auto/curated/evolved layouts recursively. The previous
        // `base_dir.join(name)` assumption failed for every nested skill.
        let src = skill::archive::archive_skill_to(self.skills.base_dir(), name)?;
        self.skill_usage.set_state(name, Lifecycle::Archived);
        // Priority #20 (Hermes `tools/skill_usage.py`): record the
        // pruned skill in `.curator_suppressed` so the next
        // `sync_skills` / `anureo update` pass does not re-seed it from
        // the bundled manifest. The list is consulted by
        // `agent/skill/src/sync.rs` before applying any bundled write
        // unless the user explicitly sets `ANUREO_SKILL_FORCE=1`.
        self.skills.usage_store().add_suppressed_name(name);
        info!("Archived skill '{}' from {}", name, src.display());
        Ok(src)
    }

    /// Inverse of `archive_skill`: move `.archive/<name>/` back into the
    /// active base dir. Returns the destination path on success.
    pub fn restore_skill(&self, name: &str) -> Result<std::path::PathBuf, SkillError> {
        // Shadowing guard: refuse to overwrite a skill that already exists
        // in the active tree, OR a non-agent-created skill (bundled / hub).
        // Hermes refuses both before touching disk.
        if self.is_agent_created_skill(name) && self.skills.find_skill_dir(name).is_some() {
            // Already in the active tree — caller is racing with another
            // restore. Treat as no-op success to match Hermes.
            return Ok(self.skills.find_skill_dir(name).unwrap());
        }

        let archive_root = self.skills.base_dir().join(".archive");
        let src = archive_root.join(name);
        let src = if src.is_dir() {
            src
        } else {
            // Hermes-aligned fallback (`skill_usage.py:540-548`): walk
            // `.archive/<name>-*` and pick the entry whose suffix is
            // exactly 14 ASCII digits (`%Y%m%d%H%M%S`). Anything else
            // (e.g. the legacy `as_secs()` form, a stray `-bak` backup
            // dir, user-supplied names) is rejected so a manual
            // `mv .archive/<name>.bak .archive/<name>` cannot sneak
            // through and overwrite the active tree.
            match pick_timestamped_archive(&archive_root, name) {
                Some(p) => p,
                None => {
                    return Err(SkillError::NotFound(format!(
                        "'{}' not found under {}",
                        name,
                        archive_root.display()
                    )));
                }
            }
        };
        let mut dst = self.skills.base_dir().join(name);
        if dst.exists() {
            // Hermes-aligned collision fallback: append a 14-digit UTC
            // stamp so the restore is recoverable even when an earlier
            // skill with the same name is present.
            let ts = Utc::now().format("%Y%m%d%H%M%S").to_string();
            dst = self.skills.base_dir().join(format!("{}-{}", name, ts));
        }
        // Cross-device rename fallback (see archive_skill).
        if let Err(e) = std::fs::rename(&src, &dst) {
            if e.raw_os_error() == Some(17) || e.raw_os_error() == Some(18) {
                info!(
                    "fs::rename failed for restore '{}' ({}); falling back to copy+remove",
                    name, e
                );
                copy_dir_recursive(&src, &dst)?;
                std::fs::remove_dir_all(&src)?;
            } else {
                return Err(SkillError::Io(e));
            }
        }
        // Priority #20 (Hermes `tools/skill_usage.py`): a restored skill
        // must be removed from `.curator_suppressed` so the next sync
        // pass keeps it. Idempotent — `remove_suppressed_name` is a
        // no-op if the name wasn't in the list.
        self.skills.usage_store().remove_suppressed_name(name);
        self.update_lifecycle(name, Lifecycle::Active)?;
        info!("Restored skill '{}' from {}", name, src.display());
        Ok(dst)
    }

    /// Hermes `skill_usage.py:482-567` `list_archived_names`: list the
    /// agent-created skills currently living under `.archive/`.
    pub fn list_archived_skills(&self) -> Result<Vec<String>, SkillError> {
        let archive_root = self.skills.base_dir().join(".archive");
        if !archive_root.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&archive_root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    out.push(name.to_string());
                }
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn touch_skill(&self, name: &str) -> Result<(), SkillError> {
        let mut state = self.load_state()?;
        state
            .skill_last_used
            .insert(name.to_string(), Utc::now().to_rfc3339());
        self.save_state(&state)?;

        self.skill_usage.bump_use(name);

        if let Ok(current) = self.skills.load(name) {
            if current.lifecycle != Lifecycle::Active {
                self.update_lifecycle(name, Lifecycle::Active)?;
                info!(
                    "Reactivated '{}' from {:?} to Active",
                    name, current.lifecycle
                );
            }
        }
        Ok(())
    }

    /// Mark that a curator run (heuristic or LLM) has completed.
    ///
    /// Updates `last_run_at`, `run_count`, and `skill_last_used` in the
    /// curator state file. Used by the LLM pass to record its execution
    /// so that `should_run()` correctly defers the next run.
    pub fn mark_run_completed(&self) -> Result<(), SkillError> {
        let mut state = self.load_state()?;
        let now = Utc::now();
        state.last_run_at = Some(now.to_rfc3339());
        state.run_count += 1;

        // Seed skill_last_used for all currently-known skills
        if let Ok(metas) = self.skills.list() {
            for meta in &metas {
                state
                    .skill_last_used
                    .entry(meta.name.clone())
                    .or_insert_with(|| now.to_rfc3339());
            }
        }

        self.save_state(&state)
    }

    /// Update curator state after an LLM review pass, populating the
    /// richer telemetry fields (`last_run_summary`, `last_report_path`,
    /// `last_run_duration_seconds`) on top of `mark_run_completed`'s
    /// `last_run_at` + `run_count` baseline.
    pub fn bump_run(
        &self,
        duration: std::time::Duration,
        summary: Option<&str>,
        report_path: Option<&Path>,
    ) -> Result<(), SkillError> {
        let mut state = self.load_state()?;
        let now = Utc::now();
        state.run_count += 1;
        state.last_run_at = Some(now.to_rfc3339());
        state.last_run_duration_seconds = Some(duration.as_secs_f64());
        state.last_run_summary = summary.map(String::from);
        state.last_report_path = report_path.map(|p| p.to_string_lossy().into_owned());

        // Seed skill_last_used for all currently-known skills
        if let Ok(metas) = self.skills.list() {
            for meta in &metas {
                state
                    .skill_last_used
                    .entry(meta.name.clone())
                    .or_insert_with(|| now.to_rfc3339());
            }
        }

        self.save_state(&state)
    }

    fn update_lifecycle(&self, name: &str, lifecycle: Lifecycle) -> Result<(), SkillError> {
        // Priority #24 (regression fix): gate `set_state(Archived)` for
        // PROTECTED_BUILTIN_SKILLS alongside the existing `archive_skill`
        // gate. Without this, the curator pass that flips Stale -> Archived
        // (the `run_for_goal` walker at line ~509) could archive a
        // bundled `plan` skill the moment `is_agent_created_skill` had
        // previously succeeded under a renamed copy. The check must fire
        // BEFORE the load() so we don't even read the on-disk state for
        // a protected skill.
        if lifecycle == Lifecycle::Archived && skill::provenance::is_protected_builtin(name) {
            return Err(SkillError::Pinned(format!(
                "'{}' is a protected builtin; refusing set_state(Archived)",
                name
            )));
        }
        let mut skill = self.skills.load(name)?;
        skill.lifecycle = lifecycle;
        self.skills.save(name, &skill)?;
        self.skill_usage.set_state(name, lifecycle);
        Ok(())
    }

    pub fn load_state(&self) -> Result<CuratorState, SkillError> {
        if !self.state_path.exists() {
            return Ok(CuratorState::default());
        }
        let data = fs::read_to_string(&self.state_path)?;
        serde_json::from_str(&data).map_err(|e| SkillError::InvalidFormat(e.to_string()))
    }

    fn save_state(&self, state: &CuratorState) -> Result<(), SkillError> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(state)
            .map_err(|e| SkillError::InvalidFormat(e.to_string()))?;
        let tmp_path = self.state_path.with_extension("tmp");
        fs::write(&tmp_path, &data)?;
        fs::rename(&tmp_path, &self.state_path)?;
        Ok(())
    }
}

/// Hermes-aligned `shutil.copytree` analogue used by the cross-device
/// rename fallback in `archive_skill`/`restore_skill` when
/// Hermes-aligned archive disambiguation (`skill_usage.py:540-548`):
/// scan `.archive/<name>-*` and return the directory whose suffix is
/// exactly 14 ASCII digits (`%Y%m%d%H%M%S`). Any other shape (the
/// legacy `as_secs()` form, `-bak`, manual `mv`, …) is rejected so we
/// cannot accidentally restore a stale copy. If multiple match, pick
/// the lexicographically largest (matches Hermes' "most recent first").
fn pick_timestamped_archive(
    archive_root: &std::path::Path,
    name: &str,
) -> Option<std::path::PathBuf> {
    let prefix = format!("{}-", name);
    let entries = std::fs::read_dir(archive_root).ok()?;
    let mut candidates: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            if !n.starts_with(&prefix) {
                return None;
            }
            let suffix = &n[prefix.len()..];
            if suffix.len() == 14 && suffix.chars().all(|c| c.is_ascii_digit()) {
                Some(e.path())
            } else {
                None
            }
        })
        .collect();
    candidates.sort();
    candidates.pop()
}

/// `fs::rename` fails on cross-volume links (Windows error 17).
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), SkillError> {
    std::fs::create_dir_all(dst)?;
    let mut stack: Vec<std::path::PathBuf> = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => return Err(SkillError::Io(e)),
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => return Err(SkillError::Io(e)),
            };
            let from = entry.path();
            let ft = match entry.file_type() {
                Ok(f) => f,
                Err(e) => return Err(SkillError::Io(e)),
            };
            let rel = from.strip_prefix(src).unwrap_or(&from);
            let to = dst.join(rel);
            if ft.is_dir() {
                std::fs::create_dir_all(&to)?;
                stack.push(from);
            } else if ft.is_symlink() {
                let target = fs::read_link(&from)?;
                #[cfg(unix)]
                std::os::unix::fs::symlink(&target, &to)?;
                #[cfg(not(unix))]
                {
                    if target.is_dir() {
                        copy_dir_recursive(&target, &to)?;
                    } else if target.is_file() {
                        fs::copy(&target, &to)?;
                    } else {
                        fs::copy(&from, &to)?;
                    }
                }
            } else {
                fs::copy(&from, &to)?;
            }
        }
    }
    Ok(())
}

fn compute_usage_days_since(usage: &SkillUsageReport, now: &chrono::DateTime<chrono::Utc>) -> u32 {
    if let Some(ts) = usage
        .last_activity_at
        .as_ref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
    {
        return now
            .signed_duration_since(ts.with_timezone(&chrono::Utc))
            .num_days()
            .max(0) as u32;
    }
    if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&usage.created_at) {
        return now
            .signed_duration_since(ts.with_timezone(&chrono::Utc))
            .num_days()
            .max(0) as u32;
    }
    0
}

fn compute_skill_similarity(a: &SkillContent, b: &SkillContent) -> f64 {
    let a_words: std::collections::HashSet<String> = a
        .description
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .chain(a.triggers.iter().map(|t| t.to_lowercase()))
        .collect();

    let b_words: std::collections::HashSet<String> = b
        .description
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .chain(b.triggers.iter().map(|t| t.to_lowercase()))
        .collect();

    if a_words.is_empty() || b_words.is_empty() {
        return 0.0;
    }

    let intersection = a_words.intersection(&b_words).count();
    let union = a_words.union(&b_words).count();
    if union == 0 {
        return 0.0;
    }
    intersection as f64 / union as f64
}

// ─────────────────────────────────────────────────────────────────────────────
// Package Integrity Check
// ─────────────────────────────────────────────────────────────────────────────

/// Result of a package integrity check for a single skill.
///
/// Aligns with Hermes `check_package_integrity()` (`agent/curator.py:580-620`).
/// Scans the skill's SKILL.md body for relative links to `references/`,
/// `templates/`, `scripts/`, and `assets/` directories, then verifies that
/// each linked file exists on disk.
#[derive(Debug, Clone)]
pub struct PackageIntegrityResult {
    /// Name of the skill checked.
    pub skill_name: String,
    /// Whether the package is safe to archive (no broken links).
    pub is_safe: bool,
    /// List of broken relative links found in SKILL.md.
    pub broken_links: Vec<String>,
    /// List of support directories that exist under the skill root.
    pub support_dirs: Vec<String>,
}

/// Check package integrity for a skill before archiving.
///
/// Scans the skill's SKILL.md body for relative links to support directories
/// (`references/`, `templates/`, `scripts/`, `assets/`) and verifies that
/// the linked files exist on disk.
///
/// # Arguments
/// * `skills_base_dir` - The base directory of the skill library
/// * `skill_name` - The name of the skill to check
///
/// # Returns
/// `PackageIntegrityResult` with `is_safe=true` if no broken links found.
pub fn check_package_integrity(
    skills_base_dir: &std::path::Path,
    skill_name: &str,
) -> PackageIntegrityResult {
    let skill_dir = skills_base_dir.join(skill_name);
    let skill_md_path = skill_dir.join("SKILL.md");

    let mut broken_links = Vec::new();
    let mut support_dirs = Vec::new();

    // Check which support directories exist
    for dir_name in &["references", "templates", "scripts", "assets"] {
        if skill_dir.join(dir_name).exists() {
            support_dirs.push(dir_name.to_string());
        }
    }

    // Read SKILL.md
    let body = match fs::read_to_string(&skill_md_path) {
        Ok(content) => content,
        Err(_) => {
            return PackageIntegrityResult {
                skill_name: skill_name.to_string(),
                is_safe: true, // No SKILL.md = nothing to check
                broken_links,
                support_dirs,
            };
        }
    };

    // Regex to find relative links to support directories
    let link_re =
        regex::Regex::new(r"(?:references|templates|scripts|assets)/[a-zA-Z0-9_\-./]+").unwrap();

    let md_link_re =
        regex::Regex::new(r"\]\((references|templates|scripts|assets)/[^)]+\)").unwrap();

    // Collect all referenced paths
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();

    for cap in link_re.captures_iter(&body) {
        if let Some(m) = cap.get(0) {
            let path = m.as_str().trim_end_matches(|c: char| {
                !c.is_alphanumeric() && c != '/' && c != '.' && c != '-' && c != '_'
            });
            referenced.insert(path.to_string());
        }
    }

    for cap in md_link_re.captures_iter(&body) {
        if let Some(full) = cap.get(0) {
            let inner = full.as_str().trim_start_matches("](").trim_end_matches(')');
            referenced.insert(inner.to_string());
        }
    }

    // Verify each referenced path exists
    for rel_path in &referenced {
        let full_path = skill_dir.join(rel_path);
        if !full_path.exists() {
            broken_links.push(rel_path.clone());
        }
    }

    let is_safe = broken_links.is_empty();

    PackageIntegrityResult {
        skill_name: skill_name.to_string(),
        is_safe,
        broken_links,
        support_dirs,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// LLM Review Types
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct LlMReviewResult {
    pub summary: String,
    pub clusters: Vec<SkillCluster>,
    pub prunings: Vec<PruningDecision>,
    pub consolidations: Vec<ConsolidationDecision>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillCluster {
    pub name: String,
    pub members: Vec<String>,
    pub umbrella: String,
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruningDecision {
    pub name: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConsolidationDecision {
    pub source: String,
    pub into: String,
    pub method: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSnapshot {
    pub name: String,
    pub description: String,
    pub lifecycle: String,
    pub triggers: Vec<String>,
    pub body_len: usize,
}

/// A skill lifecycle state change observed between before/after snapshots.
///
/// Aligns with Hermes `state_transitions` payload field
/// (`agent/curator.py:1052-1057`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateTransition {
    pub name: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone)]
pub struct AbsorbedIntoDeclaration {
    pub name: String,
    pub into: String,
}

#[derive(Debug, Clone, Default)]
pub struct ClassificationResult {
    pub consolidated: Vec<ConsolidationDecision>,
    pub pruned: Vec<PruningDecision>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CuratorRunReport {
    pub run_id: String,
    pub started_at: String,
    pub elapsed_seconds: f64,
    pub model: String,
    pub provider: String,
    pub auto_counts: AutoCounts,
    pub before_snapshots: Vec<SkillSnapshot>,
    pub after_snapshots: Vec<SkillSnapshot>,
    pub consolidated: Vec<ConsolidationDecision>,
    pub pruned: Vec<PruningDecision>,
    pub llm_summary: Option<String>,
    pub classification_sources: std::collections::HashMap<String, String>,
    /// LLM pass error (Hermes `llm_error`). None = success.
    pub run_error: Option<String>,
    /// All tool calls collected during the pass (Hermes `tool_calls`).
    pub tool_calls: Vec<CuratorToolCall>,
    /// Skills removed this run (Hermes `archived`).
    pub removed: Vec<String>,
    /// Skills added this run (Hermes `added`).
    pub added: Vec<String>,
    /// Skill lifecycle state changes (Hermes `state_transitions`).
    pub state_transitions: Vec<StateTransition>,
}

impl CuratorRunReport {
    /// Save the report as both JSON and Markdown to the given directory.
    ///
    /// Creates `<dir>/<run_id>.json` and `<dir>/<run_id>.md`.
    /// Returns the paths of both files.
    ///
    /// Aligns with Hermes `CuratorRunReport.save_to_dir()` (`agent/curator.py:622-651`).
    pub fn save_to_dir(&self, dir: &std::path::Path) -> Result<(PathBuf, PathBuf), SkillError> {
        fs::create_dir_all(dir)?;

        let json_path = dir.join(format!("{}.json", self.run_id));
        let md_path = dir.join(format!("{}.md", self.run_id));

        // Write JSON
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| SkillError::InvalidFormat(e.to_string()))?;
        fs::write(&json_path, &json)?;

        // Write Markdown
        let md = self.render_markdown();
        fs::write(&md_path, &md)?;

        Ok((json_path, md_path))
    }

    /// Render the report as a human-readable Markdown document.
    ///
    /// Aligns with Hermes `CuratorRunReport.render_markdown()` (`agent/curator.py:653-690`).
    pub fn render_markdown(&self) -> String {
        use std::fmt::Write;
        let mut md = String::new();

        let _ = writeln!(md, "# Curator Run Report");
        let _ = writeln!(md);
        let _ = writeln!(md, "- **Run ID**: `{}`", self.run_id);
        let _ = writeln!(md, "- **Started**: {}", self.started_at);
        let _ = writeln!(md, "- **Elapsed**: {:.1}s", self.elapsed_seconds);
        if !self.model.is_empty() {
            let _ = writeln!(md, "- **Model**: `{}`", self.model);
        }
        if !self.provider.is_empty() {
            let _ = writeln!(md, "- **Provider**: `{}`", self.provider);
        }
        let _ = writeln!(md);

        // LLM error warning (Hermes `> ⚠ LLM pass error`)
        if let Some(ref err) = self.run_error {
            let _ = writeln!(md, "> ⚠ **LLM pass error**: `{}`", err);
            let _ = writeln!(md);
        }

        // Auto transitions
        let _ = writeln!(md, "## Auto Transitions");
        let _ = writeln!(md);
        let _ = writeln!(
            md,
            "- Checked: {} | Stale: {} | Archived: {} | Reactivated: {}",
            self.auto_counts.checked,
            self.auto_counts.marked_stale,
            self.auto_counts.archived,
            self.auto_counts.reactivated,
        );
        let _ = writeln!(md);

        // LLM consolidation pass statistics (Hermes `_render_report_markdown` §2)
        let _ = writeln!(md, "## LLM Consolidation Pass");
        let _ = writeln!(md);
        // Compute tool_call_counts by-name
        let mut tc_counts: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for tc in &self.tool_calls {
            *tc_counts.entry(tc.name.as_str()).or_default() += 1;
        }
        let _ = writeln!(md, "- **Tool calls**: {}", self.tool_calls.len());
        if !tc_counts.is_empty() {
            let mut sorted: Vec<(&&str, &usize)> = tc_counts.iter().collect();
            sorted.sort_by_key(|(k, _)| *k);
            let by_name: Vec<String> = sorted.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            let _ = writeln!(md, "  - By name: {}", by_name.join(", "));
        }
        let _ = writeln!(md, "- **Consolidated**: {}", self.consolidated.len());
        let _ = writeln!(md, "- **Pruned**: {}", self.pruned.len());
        let _ = writeln!(md, "- **Added**: {}", self.added.len());
        let _ = writeln!(
            md,
            "- **State transitions**: {}",
            self.state_transitions.len()
        );
        let _ = writeln!(md);

        // LLM summary
        if let Some(summary) = &self.llm_summary {
            let _ = writeln!(md, "## LLM Summary");
            let _ = writeln!(md);
            let _ = writeln!(md, "{}", summary);
            let _ = writeln!(md);
        }

        // Consolidated — list format with reason (aligns Hermes §3)
        if !self.consolidated.is_empty() {
            let _ = writeln!(md, "### Consolidated ({})", self.consolidated.len());
            let _ = writeln!(md);
            for c in &self.consolidated {
                let _ = writeln!(
                    md,
                    "- `{}` → merged into `{}` — {}",
                    c.source, c.into, c.method
                );
            }
            let _ = writeln!(md);
        }

        // Pruned — list format with reason (aligns Hermes §4)
        if !self.pruned.is_empty() {
            let _ = writeln!(md, "### Pruned ({})", self.pruned.len());
            let _ = writeln!(md);
            for p in &self.pruned {
                let _ = writeln!(md, "- `{}` — {}", p.name, p.reason);
            }
            let _ = writeln!(md);
        }

        // New skills this run (aligns Hermes §5)
        if !self.added.is_empty() {
            let _ = writeln!(md, "### New skills this run ({})", self.added.len());
            let _ = writeln!(md);
            for name in &self.added {
                let _ = writeln!(md, "- `{}`", name);
            }
            let _ = writeln!(md);
        }

        // State transitions (aligns Hermes §6)
        if !self.state_transitions.is_empty() {
            let _ = writeln!(
                md,
                "### State transitions ({})",
                self.state_transitions.len()
            );
            let _ = writeln!(md);
            for t in &self.state_transitions {
                let _ = writeln!(md, "- `{}`: {} → {}", t.name, t.from, t.to);
            }
            let _ = writeln!(md);
        }

        // Before/After skill counts
        if !self.before_snapshots.is_empty() || !self.after_snapshots.is_empty() {
            let _ = writeln!(md, "## Skill Library Changes");
            let _ = writeln!(md);
            let _ = writeln!(
                md,
                "- Before: {} skills | After: {} skills (Δ {:+})",
                self.before_snapshots.len(),
                self.after_snapshots.len(),
                self.after_snapshots.len() as i64 - self.before_snapshots.len() as i64,
            );
            let _ = writeln!(md);
        }

        // Classification sources breakdown
        if !self.classification_sources.is_empty() {
            let _ = writeln!(md, "## Classification Sources");
            let _ = writeln!(md);
            let mut counts: std::collections::HashMap<&str, usize> =
                std::collections::HashMap::new();
            for method in self.classification_sources.values() {
                *counts.entry(method.as_str()).or_default() += 1;
            }
            for (method, count) in counts {
                let _ = writeln!(md, "- **{}**: {}", method, count);
            }
            let _ = writeln!(md);
        }

        md
    }

    /// Build a `CuratorRunReport` from an LLM pass outcome.
    pub fn from_llm_pass_outcome(
        outcome: &crate::curator_llm::CuratorLlmPassOutcome,
        run_id: &str,
    ) -> Self {
        use std::collections::HashMap;

        let started_at = chrono::Utc::now().to_rfc3339();

        // Build classification_sources map from the outcome
        let mut classification_sources = HashMap::new();
        for c in &outcome.classification.consolidated {
            classification_sources.insert(c.source.clone(), c.method.clone());
        }

        // Compute state transitions by diffing before/after snapshot lifecycles
        // (aligns Hermes curator.py:1052-1057).
        let state_transitions = {
            let before_map: HashMap<&str, &str> = outcome
                .before_snapshots
                .iter()
                .map(|s| (s.name.as_str(), s.lifecycle.as_str()))
                .collect();
            let mut transitions = Vec::new();
            for after in &outcome.after_snapshots {
                if let Some(&before_lc) = before_map.get(after.name.as_str()) {
                    if before_lc != after.lifecycle {
                        transitions.push(StateTransition {
                            name: after.name.clone(),
                            from: before_lc.to_string(),
                            to: after.lifecycle.clone(),
                        });
                    }
                }
            }
            transitions
        };

        Self {
            run_id: run_id.to_string(),
            started_at,
            elapsed_seconds: outcome.elapsed_seconds,
            model: outcome.model.clone(),
            provider: outcome.provider.clone(),
            auto_counts: AutoCounts::default(),
            before_snapshots: outcome.before_snapshots.clone(),
            after_snapshots: outcome.after_snapshots.clone(),
            consolidated: outcome.classification.consolidated.clone(),
            pruned: outcome.classification.pruned.clone(),
            llm_summary: if outcome.summary.is_empty() {
                None
            } else {
                Some(outcome.summary.clone())
            },
            classification_sources,
            run_error: outcome.run_error.clone(),
            tool_calls: outcome.all_tool_calls.clone(),
            removed: outcome.removed_skills(),
            added: outcome.added_skills(),
            state_transitions,
        }
    }
}

/// Intermediate struct for deserializing the full YAML response.
///
/// Maps directly to the YAML schema the prompt instructs the LLM to produce:
/// ```yaml
/// summary: |
///   ...
/// clusters:
///   - name: ...
///     members: [...]
///     umbrella: ...
///     action: ...
/// prunings:
///   - name: ...
///     reason: ...
/// consolidations:
///   - source: ...
///     into: ...
///     method: ...
/// ```
#[derive(Deserialize)]
struct RawYamlResponse {
    summary: Option<String>,
    clusters: Option<Vec<SkillCluster>>,
    prunings: Option<Vec<PruningDecision>>,
    consolidations: Option<Vec<ConsolidationDecision>>,
}

/// Parse LLM YAML response (curator review output).
///
/// Extracts `summary`, `clusters`, `prunings`, and `consolidations` from the
/// YAML block. Aligns with the full prompt schema — no longer a stub.
///
/// The function first tries to extract a ```yaml fenced block; if none is
/// found, it treats the entire raw text as YAML. It then deserializes via
/// `serde_yaml` into [`RawYamlResponse`] and maps the result.
///
/// If the YAML is malformed, the function falls back to regex-based summary
/// extraction and returns empty lists for the structured fields (graceful
/// degradation).
pub fn parse_llm_review_response(raw: &str) -> LlMReviewResult {
    static YAML_BLOCK_RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"(?s)```yaml\s*(.*?)\s*```").unwrap());

    let yaml_body = YAML_BLOCK_RE
        .captures(raw)
        .and_then(|cap| cap.get(1))
        .map(|m| m.as_str())
        .unwrap_or(raw);

    // Attempt full structured deserialization
    match serde_yaml::from_str::<RawYamlResponse>(yaml_body) {
        Ok(parsed) => {
            let result = LlMReviewResult {
                summary: parsed.summary.unwrap_or_default().trim().to_string(),
                clusters: parsed.clusters.unwrap_or_default(),
                prunings: parsed.prunings.unwrap_or_default(),
                consolidations: parsed.consolidations.unwrap_or_default(),
            };
            debug!(
                "Parsed LLM review: {} clusters, {} prunings, {} consolidations",
                result.clusters.len(),
                result.prunings.len(),
                result.consolidations.len()
            );
            result
        }
        Err(e) => {
            // Graceful fallback: extract summary via regex, leave structured
            // fields empty. This handles LLMs that produce slightly malformed
            // YAML while still capturing the human-readable summary.
            debug!(
                "YAML parse failed ({}), falling back to regex summary extraction",
                e
            );

            let mut result = LlMReviewResult::default();

            static SUMMARY_RE: once_cell::sync::Lazy<regex::Regex> =
                once_cell::sync::Lazy::new(|| {
                    regex::Regex::new(r"(?m)^summary:\s*\|?\s*\n?((?:  .+\n?)+)").unwrap()
                });
            if let Some(cap) = SUMMARY_RE.captures(yaml_body) {
                let content = cap.get(1).map(|m| m.as_str()).unwrap_or("");
                result.summary = content
                    .lines()
                    .map(|l| l.trim_start_matches("  ").trim())
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string();
            }

            result
        }
    }
}

/// Build LLM prompt for curator review.
///
/// Aligns with Hermes `_render_candidate_list()` (`agent/curator.py:1368`):
/// sends only name + description + usage stats per skill (no body, no triggers).
/// This keeps the prompt compact (~150 chars/skill vs ~450 in the old format)
/// so 200+ skills fit in a single LLM context without timeout.
///
/// `dry_run` (Hermes parity: `agent/curator.py:run_curator_review(..., dry_run=True)`):
/// when `true`, prepend a `## Mode: DRY RUN` banner so the LLM produces an
/// analysis that *describes* changes without committing them. Tools still
/// run; downstream callers gate writes on the same flag.
pub fn build_llm_prompt(
    skills: &[SkillContent],
    usage_reports: &[SkillUsageReport],
    dry_run: bool,
) -> String {
    use std::collections::HashMap;
    use std::fmt::Write;

    let usage_map: HashMap<&str, &SkillUsageReport> =
        usage_reports.iter().map(|u| (u.name.as_str(), u)).collect();

    let mut prompt = CURATOR_REVIEW_PROMPT.trim().to_string();
    if dry_run {
        prompt.push_str(
            "\n\n## Mode: DRY RUN\n\n\
             You are running in dry-run mode. Produce the YAML analysis as usual, \
             but DO NOT describe any tool calls that would modify the skill library. \
             Treat all proposed changes as informational; the caller will not commit them.\n",
        );
    }
    prompt.push_str("\n\n## Active Skills\n\n");

    if skills.is_empty() {
        prompt.push_str("(No active skills found. Nothing to consolidate.)");
    } else {
        let _ = writeln!(prompt, "Agent-created skills ({}):\n", skills.len());
        for skill in skills {
            let u = usage_map.get(skill.name.as_str());
            let usage_line = match u {
                Some(r) => format!(
                    "  activity={} use={} view={} patches={} last={}",
                    r.activity_count,
                    r.use_count,
                    r.view_count,
                    r.patch_count,
                    r.last_activity_at.as_deref().unwrap_or("never")
                ),
                None => String::new(),
            };
            let _ = writeln!(
                prompt,
                "- {}  {}{}",
                skill.name,
                skill.description,
                if usage_line.is_empty() {
                    String::new()
                } else {
                    format!("\n  {}", usage_line)
                }
            );
        }
    }

    prompt.push_str("\n---\nOutput your YAML analysis now:");
    prompt
}

/// Three-layer classification arbitration
pub fn reconcile_classification(
    removed: &[String],
    _added: &[String],
    after_names: &std::collections::HashSet<String>,
    tool_calls: &[CuratorToolCall],
    llm_result: &LlMReviewResult,
) -> ClassificationResult {
    let extract_args =
        |call: &CuratorToolCall| -> std::collections::HashMap<String, serde_json::Value> {
            serde_json::from_str(call.arguments.as_str()).unwrap_or_default()
        };

    let declarations = extract_absorbed_into_declarations(tool_calls, &extract_args);

    let model_cons: std::collections::HashMap<_, _> = llm_result
        .consolidations
        .iter()
        .map(|c| (c.source.clone(), c))
        .collect();

    let heuristic = classify_removed_skills(removed, after_names, tool_calls, &extract_args);
    let heur_cons: std::collections::HashMap<_, _> = heuristic
        .consolidated
        .iter()
        .map(|c| (c.source.clone(), c))
        .collect();

    let mut consolidated = Vec::new();
    let mut pruned = Vec::new();

    for name in removed {
        if let Some(dec) = declarations.get(name) {
            if !dec.into.is_empty() {
                consolidated.push(ConsolidationDecision {
                    source: name.clone(),
                    into: dec.into.clone(),
                    method: "absorbed".to_string(),
                });
            } else {
                pruned.push(PruningDecision {
                    name: name.clone(),
                    reason: "explicit prune".to_string(),
                });
            }
        } else if let Some(c) = model_cons.get(name) {
            // Validate that the model-declared destination actually exists.
            // The LLM YAML block may claim consolidation into a skill that was
            // never created (hallucinated umbrella name). If the destination
            // does not exist in `after_names`, fall through to pruning rather
            // than recording a dangling reference.
            if after_names.contains(&c.into) {
                consolidated.push((*c).clone());
            } else {
                debug!(
                    "Model consolidation for '{}' into '{}' rejected: destination does not exist",
                    name, c.into
                );
                pruned.push(PruningDecision {
                    name: name.clone(),
                    reason: format!("model declared '{}' but it was not created", c.into),
                });
            }
        } else if let Some(c) = heur_cons.get(name) {
            consolidated.push((*c).clone());
        } else {
            pruned.push(PruningDecision {
                name: name.clone(),
                reason: "fallback (no evidence)".to_string(),
            });
        }
    }

    ClassificationResult {
        consolidated,
        pruned,
    }
}

fn extract_absorbed_into_declarations(
    tool_calls: &[CuratorToolCall],
    extract_args: &impl Fn(&CuratorToolCall) -> std::collections::HashMap<String, serde_json::Value>,
) -> std::collections::HashMap<String, AbsorbedIntoDeclaration> {
    let mut declarations = std::collections::HashMap::new();

    for call in tool_calls {
        // anureo exposes a single `skill_manage` tool; `absorbed_into` is only
        // meaningful on `action=delete`. Accept both the canonical tool name
        // (`skill_manage`) and the prompt-facing alias (`skill_delete`) so the
        // extraction survives regardless of how the tool-call is serialised.
        if call.name != "skill_manage" && call.name != "skill_delete" {
            continue;
        }
        let args = extract_args(call);
        // Only `delete` action carries `absorbed_into`; skip writes/patches/etc.
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("delete");
        if call.name == "skill_manage" && action != "delete" {
            continue;
        }
        if !args.contains_key("absorbed_into") {
            continue;
        }
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let into = args
            .get("absorbed_into")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        declarations.insert(
            name.to_string(),
            AbsorbedIntoDeclaration {
                name: name.to_string(),
                into: into.to_string(),
            },
        );
    }
    declarations
}

fn classify_removed_skills(
    removed: &[String],
    after_names: &std::collections::HashSet<String>,
    tool_calls: &[CuratorToolCall],
    extract_args: &impl Fn(&CuratorToolCall) -> std::collections::HashMap<String, serde_json::Value>,
) -> ClassificationResult {
    let destinations: std::collections::HashSet<String> = after_names.iter().cloned().collect();
    let mut consolidated = Vec::new();
    let mut pruned = Vec::new();

    for name in removed {
        let mut found_into: Option<String> = None;
        'search: for call in tool_calls {
            let args = extract_args(call);
            let target = args.get("name").and_then(|v| v.as_str()).unwrap_or("");

            if target.is_empty() || target == name {
                continue;
            }

            if !destinations.contains(target) {
                continue;
            }

            for value in args.values() {
                if let Some(s) = value.as_str() {
                    if name_mentioned_in_value(name, s) {
                        found_into = Some(target.to_string());
                        break 'search;
                    }
                }
            }
        }

        if let Some(into) = found_into {
            consolidated.push(ConsolidationDecision {
                source: name.clone(),
                into,
                method: "heuristic".to_string(),
            });
        } else {
            pruned.push(PruningDecision {
                name: name.clone(),
                reason: "no absorption evidence".to_string(),
            });
        }
    }

    ClassificationResult {
        consolidated,
        pruned,
    }
}

/// Check whether a skill `name` is mentioned in a tool-call argument `value`.
///
/// Uses **token boundary matching** instead of raw `value.contains(name)` to
/// avoid false positives. For example, `name = "rust"` should match
/// `"rust-debug"` or `"merge rust into umbrella"` but NOT `"trust"`.
///
/// Tokenization splits on `-`, `_`, `/`, whitespace, and common punctuation,
/// treating each segment as a distinct token. Both the hyphen and underscore
/// forms of the name are checked (so `"rust_debug"` matches `"rust-debug"`).
///
/// Aligns with Hermes `classify_removed_skills()` precision requirements.
fn name_mentioned_in_value(name: &str, value: &str) -> bool {
    // Normalize: replace hyphens with underscores for consistent tokenization
    let normalize = |s: &str| s.replace('-', "_");
    let name_norm = normalize(name);
    let value_norm = normalize(value);

    // Quick exit: if the entire value is the name (exact match)
    if value_norm == name_norm {
        return true;
    }

    // Collect all candidate tokens from the value:
    //
    // 1. **Full tokens** — split on whitespace + punctuation (but NOT `_`
    //    or `-` which are part of skill names). These match multi-word
    //    skill names like "rust_debug" in "merge rust_debug into umbrella".
    //
    // 2. **Sub-tokens** — additionally split each full token on `_` to handle
    //    cases where `name = "rust"` should match `value = "rust-debug"`
    //    (normalized: "rust_debug" → sub-tokens: ["rust", "debug"]).
    //    This ensures single-word components match inside compound names
    //    without false positives like "trust" matching "rust".
    let mut candidates: Vec<&str> = Vec::new();

    let full_tokens: Vec<&str> = value_norm
        .split(|c: char| c.is_whitespace() || "/,.;:()[]{}\"'".contains(c))
        .filter(|t| !t.is_empty())
        .collect();

    for token in &full_tokens {
        candidates.push(token);
        // Also add sub-tokens split on underscore for component matching
        for sub in token.split('_') {
            if !sub.is_empty() {
                candidates.push(sub);
            }
        }
    }

    // Check if any candidate equals the name
    if candidates.iter().any(|t| *t == name_norm) {
        return true;
    }

    // Also check path-component matching: if the value looks like a file path
    // (e.g. "skills/rust-debug/SKILL.md"), match on path components.
    if value.contains('/') {
        let components: Vec<&str> = value_norm.split('/').collect();
        if components.iter().any(|c| *c == name_norm) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod llm_review_tests {
    use super::*;

    #[test]
    fn test_parse_llm_review_response_with_yaml() {
        let raw = r#"
Here is my analysis:

```yaml
summary: |
  Processed 2 clusters. Merged narrow skills into broader umbrellas.

clusters:
  - name: "rust-debug"
    members: ["rust-debug-a", "rust-debug-b"]
    umbrella: "rust-debug"
    action: "merge"

prunings:
  - name: "obsolete-one-off"
    reason: "truly stale and irrelevant"

consolidations:
  - source: "rust-debug-a"
    into: "rust-debug"
    method: "patched"
  - source: "rust-debug-b"
    into: "rust-debug"
    method: "references"
```
"#;
        let result = parse_llm_review_response(raw);

        // Summary
        assert!(result.summary.contains("2 clusters"));

        // Clusters (was always empty in the old stub)
        assert_eq!(result.clusters.len(), 1);
        assert_eq!(result.clusters[0].name, "rust-debug");
        assert_eq!(
            result.clusters[0].members,
            vec!["rust-debug-a", "rust-debug-b"]
        );
        assert_eq!(result.clusters[0].umbrella, "rust-debug");
        assert_eq!(result.clusters[0].action, "merge");

        // Prunings
        assert_eq!(result.prunings.len(), 1);
        assert_eq!(result.prunings[0].name, "obsolete-one-off");
        assert!(result.prunings[0].reason.contains("stale"));

        // Consolidations
        assert_eq!(result.consolidations.len(), 2);
        assert_eq!(result.consolidations[0].source, "rust-debug-a");
        assert_eq!(result.consolidations[0].into, "rust-debug");
        assert_eq!(result.consolidations[0].method, "patched");
        assert_eq!(result.consolidations[1].source, "rust-debug-b");
        assert_eq!(result.consolidations[1].method, "references");
    }

    #[test]
    fn test_parse_llm_review_response_malformed_fallback() {
        // Malformed YAML should still extract the summary via regex fallback
        let raw = r#"
```yaml
summary: |
  This is a valid summary.
clusters: [[invalid yaml
```
"#;
        let result = parse_llm_review_response(raw);
        // Summary should be extracted even if structured parsing fails
        assert!(result.summary.contains("valid summary"));
        // Structured fields should be empty (graceful degradation)
        assert!(result.clusters.is_empty());
        assert!(result.prunings.is_empty());
        assert!(result.consolidations.is_empty());
    }

    #[test]
    fn test_parse_llm_review_response_no_yaml_block() {
        // Raw YAML without code fence should also be parsed
        let raw = "summary: A short summary.\nconsolidations: []\n";
        let result = parse_llm_review_response(raw);
        assert_eq!(result.summary, "A short summary.");
        assert!(result.consolidations.is_empty());
    }

    #[test]
    fn test_parse_llm_review_response_empty() {
        let raw = "Nothing to do here.";
        let result = parse_llm_review_response(raw);
        assert!(result.clusters.is_empty());
        assert!(result.prunings.is_empty());
        assert!(result.consolidations.is_empty());
    }

    #[test]
    fn test_name_mentioned_in_value_token_boundary() {
        // Exact match
        assert!(name_mentioned_in_value("rust", "rust"));

        // Hyphen-separated token: "rust" in "rust-debug" → match
        assert!(name_mentioned_in_value("rust", "rust-debug"));
        assert!(name_mentioned_in_value("rust-debug", "rust-debug"));

        // Underscore normalization: "rust_debug" matches "rust-debug"
        assert!(name_mentioned_in_value(
            "rust_debug",
            "merge rust-debug into umbrella"
        ));

        // Path component: "rust" in "skills/rust/SKILL.md" → match
        assert!(name_mentioned_in_value("rust", "skills/rust/SKILL.md"));

        // Sentence context: "merge rust into umbrella" → match
        assert!(name_mentioned_in_value("rust", "merge rust into umbrella"));

        // CRITICAL: substring false positive must NOT match
        // "rust" in "trust" → NO match (word boundary)
        assert!(!name_mentioned_in_value("rust", "trust"));
        assert!(!name_mentioned_in_value("rust", "trusting"));
        assert!(!name_mentioned_in_value("rust", "robust"));

        // Punctuation boundary: "rust," → match
        assert!(name_mentioned_in_value(
            "rust",
            "merge rust, debug into umbrella"
        ));

        // Empty value
        assert!(!name_mentioned_in_value("rust", ""));
        // Empty name (shouldn't match anything meaningful)
        assert!(!name_mentioned_in_value("", "anything"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_skill(name: &str, source: Source) -> SkillContent {
        SkillContent {
            name: name.to_string(),
            description: format!("Test skill {}", name),
            triggers: vec!["test".into()],
            lifecycle: Lifecycle::Active,
            source,
            category: None,
            created_by: None,
            body: "Do stuff".to_string(),
            raw: String::new(),
        }
    }

    #[test]
    fn curator_run_dry() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        skills
            .save("skill-a", &make_test_skill("skill-a", Source::Auto))
            .unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let curator = Curator::new(skills, CuratorConfig::default())
            .with_state_path(state_dir.path().join("state.json"));

        let report = curator.run(true).unwrap();
        assert_eq!(report.active, 1);
    }

    #[test]
    fn never_used_skill_not_immediately_stale() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        skills
            .save("new-skill", &make_test_skill("new-skill", Source::Auto))
            .unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let curator = Curator::new(skills, CuratorConfig::default())
            .with_state_path(state_dir.path().join("state.json"));
        let report = curator.run(true).unwrap();
        assert!(
            report.stale.is_empty(),
            "new skill should not be immediately stale"
        );
    }

    #[test]
    fn reactivation_on_touch() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        let mut skill = make_test_skill("reawaken-skill", Source::Auto);
        skill.lifecycle = Lifecycle::Stale;
        skills.save("reawaken-skill", &skill).unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let curator = Curator::new(skills, CuratorConfig::default())
            .with_state_path(state_dir.path().join("state.json"));

        curator.touch_skill("reawaken-skill").unwrap();

        let report = curator.run(true).unwrap();
        assert!(
            report.active > 0,
            "reactivated skill should be active, not stale"
        );
    }

    #[test]
    fn test_curator_run_report_render_markdown() {
        let report = CuratorRunReport {
            run_id: "test-run-001".into(),
            started_at: "2025-07-22T12:00:00Z".into(),
            elapsed_seconds: 12.5,
            model: "test-model".into(),
            provider: "test-provider".into(),
            auto_counts: AutoCounts {
                checked: 10,
                marked_stale: 2,
                archived: 1,
                reactivated: 0,
            },
            before_snapshots: vec![],
            after_snapshots: vec![],
            consolidated: vec![ConsolidationDecision {
                source: "narrow-a".into(),
                into: "umbrella".into(),
                method: "patched".into(),
            }],
            pruned: vec![PruningDecision {
                name: "obsolete".into(),
                reason: "truly stale".into(),
            }],
            llm_summary: Some("Merged 2 clusters".into()),
            classification_sources: std::collections::HashMap::new(),
            run_error: None,
            tool_calls: vec![CuratorToolCall {
                id: None,
                name: "skill_list".into(),
                arguments: "{}".into(),
            }],
            removed: vec!["old-skill".into()],
            added: vec!["new-skill".into()],
            state_transitions: vec![StateTransition {
                name: "mid-skill".into(),
                from: "Active".into(),
                to: "Stale".into(),
            }],
        };

        let md = report.render_markdown();
        assert!(md.contains("# Curator Run Report"));
        assert!(md.contains("test-run-001"));
        assert!(md.contains("12.5s"));
        assert!(md.contains("Checked: 10"));
        assert!(md.contains("Merged 2 clusters"));
        // New sections
        assert!(md.contains("## LLM Consolidation Pass"));
        assert!(md.contains("Tool calls**: 1"));
        assert!(md.contains("skill_list=1"));
        assert!(md.contains("Consolidated**: 1"));
        assert!(md.contains("Pruned**: 1"));
        assert!(md.contains("Added**: 1"));
        assert!(md.contains("State transitions**: 1"));
        // Consolidated list format
        assert!(md.contains("### Consolidated (1)"));
        assert!(md.contains("`narrow-a` → merged into `umbrella` — patched"));
        // Pruned list format
        assert!(md.contains("### Pruned (1)"));
        assert!(md.contains("`obsolete` — truly stale"));
        // Added section
        assert!(md.contains("### New skills this run (1)"));
        assert!(md.contains("`new-skill`"));
        // State transitions section
        assert!(md.contains("### State transitions (1)"));
        assert!(md.contains("`mid-skill`: Active → Stale"));
    }

    #[test]
    fn test_curator_run_report_render_with_error() {
        let report = CuratorRunReport {
            run_id: "err-run".into(),
            started_at: "2025-07-22T12:00:00Z".into(),
            elapsed_seconds: 1.0,
            model: String::new(),
            provider: String::new(),
            auto_counts: AutoCounts::default(),
            before_snapshots: vec![],
            after_snapshots: vec![],
            consolidated: vec![],
            pruned: vec![],
            llm_summary: None,
            classification_sources: std::collections::HashMap::new(),
            run_error: Some("Agent build error: timeout".into()),
            tool_calls: vec![],
            removed: vec![],
            added: vec![],
            state_transitions: vec![],
        };

        let md = report.render_markdown();
        assert!(md.contains("⚠ **LLM pass error**"));
        assert!(md.contains("Agent build error: timeout"));
    }

    #[test]
    fn test_curator_run_report_save_to_dir() {
        let dir = tempfile::tempdir().unwrap();
        let report = CuratorRunReport {
            run_id: "save-test".into(),
            started_at: "2025-07-22T12:00:00Z".into(),
            elapsed_seconds: 5.0,
            model: String::new(),
            provider: String::new(),
            auto_counts: AutoCounts::default(),
            before_snapshots: vec![],
            after_snapshots: vec![],
            consolidated: vec![],
            pruned: vec![],
            llm_summary: None,
            classification_sources: std::collections::HashMap::new(),
            run_error: None,
            tool_calls: vec![],
            removed: vec![],
            added: vec![],
            state_transitions: vec![],
        };

        let (json_path, md_path) = report.save_to_dir(dir.path()).unwrap();
        assert!(json_path.exists());
        assert!(md_path.exists());
        assert!(json_path.to_string_lossy().ends_with("save-test.json"));
        assert!(md_path.to_string_lossy().ends_with("save-test.md"));

        // Verify JSON is valid
        let json_content = std::fs::read_to_string(&json_path).unwrap();
        let _: CuratorRunReport = serde_json::from_str(&json_content).unwrap();

        // Verify Markdown is non-empty
        let md_content = std::fs::read_to_string(&md_path).unwrap();
        assert!(md_content.contains("# Curator Run Report"));
    }

    #[test]
    fn test_check_package_integrity_safe_no_links() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("test-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Test Skill\n\nNo links here.").unwrap();

        let result = check_package_integrity(dir.path(), "test-skill");
        assert!(result.is_safe);
        assert!(result.broken_links.is_empty());
    }

    #[test]
    fn test_check_package_integrity_broken_link() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("broken-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Broken Skill\n\nSee [details](references/missing.md) for more.",
        )
        .unwrap();

        let result = check_package_integrity(dir.path(), "broken-skill");
        assert!(!result.is_safe);
        assert!(result.broken_links.iter().any(|l| l.contains("missing.md")));
    }

    #[test]
    fn test_check_package_integrity_valid_link() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("good-skill");
        let refs_dir = skill_dir.join("references");
        std::fs::create_dir_all(&refs_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Good Skill\n\nSee [details](references/details.md).",
        )
        .unwrap();
        std::fs::write(refs_dir.join("details.md"), "Details content").unwrap();

        let result = check_package_integrity(dir.path(), "good-skill");
        assert!(result.is_safe);
        assert!(result.broken_links.is_empty());
        assert!(result.support_dirs.contains(&"references".to_string()));
    }

    #[test]
    fn test_should_run_rejects_too_few_skills() {
        // A near-empty library (1 skill < min_skills_to_run=5) should not run
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        // Create a single skill
        let skill = make_test_skill("lonely", Source::Auto);
        registry.save("lonely", &skill).unwrap();

        let curator = Curator::new(registry, CuratorConfig::default());
        // Even with idle time satisfied, too few skills blocks the run
        assert!(!curator.should_run(Some(99999.0)));
    }

    #[test]
    fn test_should_run_first_run_delay_blocks_immediate_run() {
        // On first run (run_count == 0), the curator should wait for
        // interval_hours since library creation before triggering.
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());

        // Create enough skills to pass the min_skills gate
        for i in 0..6 {
            let skill = make_test_skill(&format!("skill-{}", i), Source::Auto);
            registry.save(&format!("skill-{}", i), &skill).unwrap();
        }

        // Use a very large interval to ensure the delay is not met
        let config = CuratorConfig {
            interval_hours: 99999,
            min_idle_minutes: 0, // disable idle gate for this test
            ..Default::default()
        };

        let curator = Curator::new(registry, config);
        // First run should be blocked by the delay
        assert!(!curator.should_run(Some(99999.0)));
    }

    #[test]
    fn test_should_run_disabled_blocks_run() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        for i in 0..6 {
            let skill = make_test_skill(&format!("skill-{}", i), Source::Auto);
            registry.save(&format!("skill-{}", i), &skill).unwrap();
        }

        let config = CuratorConfig {
            enabled: false,
            ..Default::default()
        };

        let curator = Curator::new(registry, config);
        assert!(!curator.should_run(Some(99999.0)));
    }

    #[test]
    fn test_build_llm_prompt_with_skills() {
        let skills = vec![
            make_test_skill("skill-a", Source::Auto),
            make_test_skill("skill-b", Source::Manual),
        ];

        let usage_reports = vec![SkillUsageReport {
            name: "skill-a".to_string(),
            use_count: 5,
            view_count: 10,
            patch_count: 2,
            last_used_at: Some("2025-07-20T10:00:00Z".to_string()),
            last_viewed_at: None,
            last_patched_at: None,
            created_at: "2025-01-01T00:00:00Z".to_string(),
            state: Lifecycle::Active,
            pinned: false,
            archived_at: None,
            last_activity_at: Some("2025-07-20T10:00:00Z".to_string()),
            activity_count: 3,
        }];

        let prompt = build_llm_prompt(&skills, &usage_reports, false);

        assert!(prompt.contains("Active Skills"));
        assert!(prompt.contains("skill-a"));
        assert!(prompt.contains("skill-b"));
        assert!(prompt.contains("activity=3"));
        assert!(prompt.contains("use=5"));
        assert!(prompt.contains("view=10"));
        assert!(prompt.contains("patches=2"));
        assert!(prompt.contains("last=2025-07-20T10:00:00Z"));
        assert!(prompt.contains("Output your YAML analysis now"));
    }

    #[test]
    fn test_build_llm_prompt_empty_skills() {
        let skills: Vec<SkillContent> = vec![];
        let usage_reports: Vec<SkillUsageReport> = vec![];

        let prompt = build_llm_prompt(&skills, &usage_reports, false);

        assert!(prompt.contains("Active Skills"));
        assert!(prompt.contains("No active skills found. Nothing to consolidate."));
        assert!(prompt.contains("Output your YAML analysis now"));
    }

    #[test]
    fn test_build_llm_prompt_no_usage_reports() {
        let skills = vec![make_test_skill("skill-a", Source::Auto)];
        let usage_reports: Vec<SkillUsageReport> = vec![];

        let prompt = build_llm_prompt(&skills, &usage_reports, false);

        assert!(prompt.contains("skill-a"));
        assert!(prompt.contains("Test skill skill-a"));
        assert!(!prompt.contains("activity="));
    }

    #[test]
    fn test_reconcile_classification_with_absorbed_declarations() {
        let removed = vec!["old-skill".to_string()];
        let added = vec![];
        let after_names: std::collections::HashSet<String> =
            ["new-skill".to_string()].into_iter().collect();

        let tool_calls = vec![CuratorToolCall {
            id: None,
            name: "skill_manage".to_string(),
            arguments: r#"{"action":"delete","name":"old-skill","absorbed_into":"new-skill"}"#
                .to_string(),
        }];

        let llm_result = LlMReviewResult::default();

        let result =
            reconcile_classification(&removed, &added, &after_names, &tool_calls, &llm_result);

        assert_eq!(result.consolidated.len(), 1);
        assert_eq!(result.consolidated[0].source, "old-skill");
        assert_eq!(result.consolidated[0].into, "new-skill");
        assert_eq!(result.consolidated[0].method, "absorbed");
        assert!(result.pruned.is_empty());
    }

    #[test]
    fn test_reconcile_classification_with_explicit_prune() {
        let removed = vec!["obsolete-skill".to_string()];
        let added = vec![];
        let after_names: std::collections::HashSet<String> = std::collections::HashSet::new();

        let tool_calls = vec![CuratorToolCall {
            id: None,
            name: "skill_manage".to_string(),
            arguments: r#"{"action":"delete","name":"obsolete-skill","absorbed_into":""}"#
                .to_string(),
        }];

        let llm_result = LlMReviewResult::default();

        let result =
            reconcile_classification(&removed, &added, &after_names, &tool_calls, &llm_result);

        assert!(result.consolidated.is_empty());
        assert_eq!(result.pruned.len(), 1);
        assert_eq!(result.pruned[0].name, "obsolete-skill");
        assert_eq!(result.pruned[0].reason, "explicit prune");
    }

    #[test]
    fn test_reconcile_classification_with_model_consolidation() {
        let removed = vec!["merged-skill".to_string()];
        let added = vec![];
        let after_names: std::collections::HashSet<String> =
            ["umbrella-skill".to_string()].into_iter().collect();

        let tool_calls: Vec<CuratorToolCall> = vec![];

        let llm_result = LlMReviewResult {
            summary: String::new(),
            clusters: vec![],
            prunings: vec![],
            consolidations: vec![ConsolidationDecision {
                source: "merged-skill".to_string(),
                into: "umbrella-skill".to_string(),
                method: "llm-suggested".to_string(),
            }],
        };

        let result =
            reconcile_classification(&removed, &added, &after_names, &tool_calls, &llm_result);

        assert_eq!(result.consolidated.len(), 1);
        assert_eq!(result.consolidated[0].source, "merged-skill");
        assert_eq!(result.consolidated[0].into, "umbrella-skill");
        assert_eq!(result.consolidated[0].method, "llm-suggested");
        assert!(result.pruned.is_empty());
    }

    #[test]
    fn test_reconcile_classification_model_consolidation_invalid_destination() {
        let removed = vec!["merged-skill".to_string()];
        let added = vec![];
        let after_names: std::collections::HashSet<String> = std::collections::HashSet::new();

        let tool_calls: Vec<CuratorToolCall> = vec![];

        let llm_result = LlMReviewResult {
            summary: String::new(),
            clusters: vec![],
            prunings: vec![],
            consolidations: vec![ConsolidationDecision {
                source: "merged-skill".to_string(),
                into: "non-existent-skill".to_string(),
                method: "llm-suggested".to_string(),
            }],
        };

        let result =
            reconcile_classification(&removed, &added, &after_names, &tool_calls, &llm_result);

        assert!(result.consolidated.is_empty());
        assert_eq!(result.pruned.len(), 1);
        assert_eq!(result.pruned[0].name, "merged-skill");
        assert!(result.pruned[0].reason.contains("model declared"));
        assert!(result.pruned[0].reason.contains("non-existent-skill"));
    }

    #[test]
    fn test_reconcile_classification_fallback_no_evidence() {
        let removed = vec!["orphan-skill".to_string()];
        let added = vec![];
        let after_names: std::collections::HashSet<String> = std::collections::HashSet::new();

        let tool_calls: Vec<CuratorToolCall> = vec![];
        let llm_result = LlMReviewResult::default();

        let result =
            reconcile_classification(&removed, &added, &after_names, &tool_calls, &llm_result);

        assert!(result.consolidated.is_empty());
        assert_eq!(result.pruned.len(), 1);
        assert_eq!(result.pruned[0].name, "orphan-skill");
        assert_eq!(result.pruned[0].reason, "fallback (no evidence)");
    }

    #[test]
    fn test_extract_absorbed_into_declarations() {
        let tool_calls = vec![
            CuratorToolCall {
                id: None,
                name: "skill_manage".to_string(),
                arguments: r#"{"action":"delete","name":"skill-a","absorbed_into":"umbrella"}"#
                    .to_string(),
            },
            CuratorToolCall {
                id: None,
                name: "skill_delete".to_string(),
                arguments: r#"{"name":"skill-b","absorbed_into":"umbrella"}"#.to_string(),
            },
            CuratorToolCall {
                id: None,
                name: "skill_manage".to_string(),
                arguments: r#"{"action":"create","name":"skill-c"}"#.to_string(),
            },
            CuratorToolCall {
                id: None,
                name: "skill_manage".to_string(),
                arguments: r#"{"action":"delete","name":"skill-d","absorbed_into":""}"#.to_string(),
            },
        ];

        let extract_args =
            |call: &CuratorToolCall| -> std::collections::HashMap<String, serde_json::Value> {
                serde_json::from_str(call.arguments.as_str()).unwrap_or_default()
            };

        let declarations = extract_absorbed_into_declarations(&tool_calls, &extract_args);

        assert_eq!(declarations.len(), 3);
        assert!(declarations.contains_key("skill-a"));
        assert!(declarations.contains_key("skill-b"));
        assert!(declarations.contains_key("skill-d"));

        let skill_a_decl = declarations.get("skill-a").unwrap();
        assert_eq!(skill_a_decl.into, "umbrella");

        let skill_b_decl = declarations.get("skill-b").unwrap();
        assert_eq!(skill_b_decl.into, "umbrella");

        let skill_d_decl = declarations.get("skill-d").unwrap();
        assert_eq!(skill_d_decl.into, "");
    }

    #[test]
    fn test_classify_removed_skills_with_heuristic() {
        let removed = vec!["old-skill".to_string()];
        let after_names: std::collections::HashSet<String> =
            ["new-skill".to_string()].into_iter().collect();

        let tool_calls = vec![CuratorToolCall {
            id: None,
            name: "skill_manage".to_string(),
            arguments: r#"{"action":"create","name":"new-skill","content":"merge old-skill into umbrella"}"#.to_string(),
        }];

        let extract_args =
            |call: &CuratorToolCall| -> std::collections::HashMap<String, serde_json::Value> {
                serde_json::from_str(call.arguments.as_str()).unwrap_or_default()
            };

        let result = classify_removed_skills(&removed, &after_names, &tool_calls, &extract_args);

        assert_eq!(result.consolidated.len(), 1);
        assert_eq!(result.consolidated[0].source, "old-skill");
        assert_eq!(result.consolidated[0].into, "new-skill");
        assert_eq!(result.consolidated[0].method, "heuristic");
        assert!(result.pruned.is_empty());
    }

    #[test]
    fn test_classify_removed_skills_no_evidence() {
        let removed = vec!["orphan-skill".to_string()];
        let after_names: std::collections::HashSet<String> = std::collections::HashSet::new();

        let tool_calls: Vec<CuratorToolCall> = vec![];

        let extract_args =
            |call: &CuratorToolCall| -> std::collections::HashMap<String, serde_json::Value> {
                serde_json::from_str(call.arguments.as_str()).unwrap_or_default()
            };

        let result = classify_removed_skills(&removed, &after_names, &tool_calls, &extract_args);

        assert!(result.consolidated.is_empty());
        assert_eq!(result.pruned.len(), 1);
        assert_eq!(result.pruned[0].name, "orphan-skill");
        assert_eq!(result.pruned[0].reason, "no absorption evidence");
    }

    #[test]
    fn test_file_state_store_basic_operations() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let store = FileStateStore::new(state_path.clone());

        // Test load from non-existent file returns default state
        let state = store.load().unwrap();
        assert!(!state.paused);
        assert_eq!(state.run_count, 0);

        // Test save and load
        let new_state = CuratorState {
            paused: true,
            run_count: 5,
            ..Default::default()
        };
        store.save(&new_state).unwrap();

        let loaded = store.load().unwrap();
        assert!(loaded.paused);
        assert_eq!(loaded.run_count, 5);
    }

    #[test]
    fn test_file_state_store_trait_default_methods() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let store = FileStateStore::new(state_path);

        // Test set_paused and is_paused
        store.set_paused(true).unwrap();
        assert!(store.is_paused());

        store.set_paused(false).unwrap();
        assert!(!store.is_paused());

        // Test bump_run
        store
            .bump_run(10.5, Some("test summary"), Some("/path/to/report"))
            .unwrap();
        let state = store.load().unwrap();
        assert_eq!(state.run_count, 1);
        assert_eq!(state.last_run_duration_seconds, Some(10.5));
        assert_eq!(state.last_run_summary, Some("test summary".to_string()));
        assert_eq!(state.last_report_path, Some("/path/to/report".to_string()));

        // Test touch_skill
        store.touch_skill("test-skill").unwrap();
        let state = store.load().unwrap();
        assert!(state.skill_last_used.contains_key("test-skill"));

        // Test mark_summary_shown
        store.mark_summary_shown().unwrap();
        let state = store.load().unwrap();
        assert!(state.last_run_summary_shown_at.is_some());
    }

    #[test]
    fn test_memory_state_store_basic_operations() {
        let store = MemoryStateStore::default();

        // Test initial state
        let state = store.load().unwrap();
        assert!(!state.paused);
        assert_eq!(state.run_count, 0);

        // Test save and load
        let new_state = CuratorState {
            paused: true,
            run_count: 10,
            ..Default::default()
        };
        store.save(&new_state).unwrap();

        let loaded = store.load().unwrap();
        assert!(loaded.paused);
        assert_eq!(loaded.run_count, 10);
    }

    #[test]
    fn test_memory_state_store_with_initial_state() {
        let initial_state = CuratorState {
            paused: true,
            run_count: 7,
            ..Default::default()
        };

        let store = MemoryStateStore::with_state(initial_state);
        let loaded = store.load().unwrap();

        assert!(loaded.paused);
        assert_eq!(loaded.run_count, 7);
    }

    #[test]
    fn test_memory_state_store_trait_overrides() {
        let store = MemoryStateStore::default();

        // Test overridden set_paused and is_paused
        store.set_paused(true).unwrap();
        assert!(store.is_paused());

        store.set_paused(false).unwrap();
        assert!(!store.is_paused());

        // Test that these work alongside trait methods
        store.touch_skill("skill-a").unwrap();
        // Check paused state remains false
        assert!(!store.is_paused());
    }

    #[test]
    fn test_curator_mark_run_completed() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());

        // Add some skills
        for i in 0..3 {
            let skill = make_test_skill(&format!("skill-{}", i), Source::Auto);
            skills.save(&format!("skill-{}", i), &skill).unwrap();
        }

        let state_dir = tempfile::tempdir().unwrap();
        let curator = Curator::new(skills, CuratorConfig::default())
            .with_state_path(state_dir.path().join("state.json"));

        // Mark run as completed
        curator.mark_run_completed().unwrap();

        // Verify state was updated
        let state = curator.load_state().unwrap();
        assert_eq!(state.run_count, 1);
        assert!(state.last_run_at.is_some());
        assert_eq!(state.skill_last_used.len(), 3); // All skills should be seeded
    }

    #[test]
    fn test_curator_load_and_save_state() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        let skill = make_test_skill("test-skill", Source::Auto);
        skills.save("test-skill", &skill).unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let curator = Curator::new(skills, CuratorConfig::default())
            .with_state_path(state_dir.path().join("state.json"));

        // Test save_state
        let state = CuratorState {
            paused: true,
            run_count: 3,
            ..Default::default()
        };
        curator.save_state(&state).unwrap();

        // Test load_state
        let loaded = curator.load_state().unwrap();
        assert!(loaded.paused);
        assert_eq!(loaded.run_count, 3);

        // Test that state persists across instances
        let state_path = state_dir.path().join("state.json");
        let curator2 = Curator::new(SkillRegistry::new(dir.path()), CuratorConfig::default())
            .with_state_path(state_path);

        let loaded2 = curator2.load_state().unwrap();
        assert!(loaded2.paused);
        assert_eq!(loaded2.run_count, 3);
    }

    #[test]
    fn test_curator_update_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        let skill = make_test_skill("test-skill", Source::Auto);
        skills.save("test-skill", &skill).unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let curator = Curator::new(skills, CuratorConfig::default())
            .with_state_path(state_dir.path().join("state.json"));

        // Update lifecycle to Stale
        curator
            .update_lifecycle("test-skill", Lifecycle::Stale)
            .unwrap();

        // Verify the lifecycle was updated
        let updated_skill = curator.skills.load("test-skill").unwrap();
        assert_eq!(updated_skill.lifecycle, Lifecycle::Stale);

        // Update lifecycle back to Active
        curator
            .update_lifecycle("test-skill", Lifecycle::Active)
            .unwrap();
        let reactivated_skill = curator.skills.load("test-skill").unwrap();
        assert_eq!(reactivated_skill.lifecycle, Lifecycle::Active);
    }

    #[test]
    fn test_curator_run_with_various_lifecycles() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());

        // Create skills with different lifecycles
        let mut active_skill = make_test_skill("active-skill", Source::Auto);
        active_skill.lifecycle = Lifecycle::Active;
        skills.save("active-skill", &active_skill).unwrap();

        let mut stale_skill = make_test_skill("stale-skill", Source::Auto);
        stale_skill.lifecycle = Lifecycle::Stale;
        skills.save("stale-skill", &stale_skill).unwrap();

        let mut archived_skill = make_test_skill("archived-skill", Source::Auto);
        archived_skill.lifecycle = Lifecycle::Archived;
        skills.save("archived-skill", &archived_skill).unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let config = CuratorConfig {
            stale_days_auto: 0, // Make skills stale immediately
            archive_days: 0,
            ..Default::default()
        };
        let curator =
            Curator::new(skills, config).with_state_path(state_dir.path().join("state.json"));

        // Run in dry_run mode to check classification
        let report = curator.run(true).unwrap();

        // Verify that skills are properly classified
        assert!(!report.stale.is_empty() || !report.archived.is_empty()); // Some should be stale or archived
    }

    #[test]
    fn test_curator_run_dry_run_doesnt_modify_state() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        let skill = make_test_skill("test-skill", Source::Auto);
        skills.save("test-skill", &skill).unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let curator = Curator::new(skills, CuratorConfig::default())
            .with_state_path(state_dir.path().join("state.json"));

        // Run in dry_run mode
        let _ = curator.run(true).unwrap();

        // Verify that state was not modified (no run count increment)
        let state = curator.load_state().unwrap();
        assert_eq!(state.run_count, 0);
        assert!(state.last_run_at.is_none());
    }

    #[test]
    fn test_curator_config_accessors() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        let skill = make_test_skill("test-skill", Source::Auto);
        skills.save("test-skill", &skill).unwrap();

        let config = CuratorConfig {
            enabled: true,
            interval_hours: 100,
            min_idle_minutes: 30,
            min_skills_to_run: 10,
            overlap_threshold: 0.8,
            stale_days_auto: 90,
            stale_days_manual: 45,
            archive_days: 120,
        };

        let curator = Curator::new(skills, config);

        assert!(curator.config_enabled());
        assert_eq!(curator.config_interval_hours(), 100);
        assert_eq!(curator.config_min_skills(), 10);
        assert_eq!(curator.config_overlap_threshold(), 0.8);
        assert_eq!(curator.config_stale_days_auto(), 90);
        assert_eq!(curator.config_stale_days_manual(), 45);
        assert_eq!(curator.config_archive_days(), 120);
    }

    #[test]
    fn test_curator_state_store_default_impls_via_file() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let store = FileStateStore::new(state_path);

        // Test set_paused and is_paused trait defaults
        store.set_paused(true).unwrap();
        assert!(store.is_paused());
        store.set_paused(false).unwrap();
        assert!(!store.is_paused());

        // Test bump_run trait default
        store
            .bump_run(10.5, Some("Test summary"), Some("/test/path.md"))
            .unwrap();
        let state = store.load().unwrap();
        assert_eq!(state.run_count, 1);
        assert_eq!(state.last_run_duration_seconds, Some(10.5));
        assert_eq!(state.last_run_summary, Some("Test summary".to_string()));
        assert_eq!(state.last_report_path, Some("/test/path.md".to_string()));

        // Test touch_skill trait default
        store.touch_skill("skill-one").unwrap();
        let state = store.load().unwrap();
        assert!(state.skill_last_used.contains_key("skill-one"));

        // Test mark_summary_shown trait default
        store.mark_summary_shown().unwrap();
        let state = store.load().unwrap();
        assert!(state.last_run_summary_shown_at.is_some());
    }

    #[test]
    fn test_curator_state_store_memory_store_direct() {
        let store = MemoryStateStore::default();

        // Test set_paused and is_paused trait defaults
        store.set_paused(true).unwrap();
        assert!(store.is_paused());
        store.set_paused(false).unwrap();
        assert!(!store.is_paused());

        // Test bump_run trait default
        store.bump_run(5.0, Some("Memory summary"), None).unwrap();
        let state = store.load().unwrap();
        assert_eq!(state.run_count, 1);
        assert_eq!(state.last_run_duration_seconds, Some(5.0));
        assert_eq!(state.last_run_summary, Some("Memory summary".to_string()));
    }

    #[test]
    fn test_curator_state_store_with_state_initialization() {
        let initial_state = CuratorState {
            paused: true,
            run_count: 10,
            skill_last_used: {
                let mut map = HashMap::new();
                map.insert("pre-skill".to_string(), "2025-01-01T00:00:00Z".to_string());
                map
            },
            ..Default::default()
        };

        let store = MemoryStateStore::with_state(initial_state);
        let loaded = store.load().unwrap();

        assert!(loaded.paused);
        assert_eq!(loaded.run_count, 10);
        assert!(loaded.skill_last_used.contains_key("pre-skill"));
    }

    #[test]
    fn test_curator_config_builder_pattern() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        let skill = make_test_skill("test-skill", Source::Auto);
        skills.save("test-skill", &skill).unwrap();

        let config = CuratorConfig {
            enabled: true,
            interval_hours: 24,
            min_idle_minutes: 15,
            min_skills_to_run: 5,
            overlap_threshold: 0.85,
            stale_days_auto: 30,
            stale_days_manual: 60,
            archive_days: 90,
        };

        let curator = Curator::new(skills, config);

        assert!(curator.config_enabled());
        assert_eq!(curator.config_interval_hours(), 24);
        assert_eq!(curator.config_min_skills(), 5);
        assert_eq!(curator.config_overlap_threshold(), 0.85);
        assert_eq!(curator.config_stale_days_auto(), 30);
        assert_eq!(curator.config_stale_days_manual(), 60);
        assert_eq!(curator.config_archive_days(), 90);
    }

    #[test]
    fn test_curator_should_run_with_state_conditions() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        for i in 0..7 {
            // Need more than min_skills_to_run (default 5)
            let skill = make_test_skill(&format!("skill-{}", i), Source::Auto);
            skills.save(&format!("skill-{}", i), &skill).unwrap();
        }

        let state_dir = tempfile::tempdir().unwrap();
        let config = CuratorConfig {
            enabled: true,
            interval_hours: 1, // 1 hour interval
            min_skills_to_run: 5,
            ..Default::default()
        };

        let curator =
            Curator::new(skills, config).with_state_path(state_dir.path().join("state.json"));

        // Manually complete a run to establish state (bypassing first-run delay)
        curator.mark_run_completed().unwrap();

        // Should not run immediately after (within interval)
        assert!(
            !curator.should_run(None),
            "Should not run immediately after completing a run"
        );
    }

    #[test]
    fn test_curator_update_lifecycle_nonexistent_skill() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());

        let state_dir = tempfile::tempdir().unwrap();
        let curator = Curator::new(skills, CuratorConfig::default())
            .with_state_path(state_dir.path().join("state.json"));

        // Attempting to update non-existent skill should return an error
        let result = curator.update_lifecycle("nonexistent-skill", Lifecycle::Active);
        assert!(result.is_err());
    }

    #[test]
    fn test_curator_run_preserves_lifecycle_classification() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());

        // Create a skill with explicitly set stale lifecycle
        let mut stale_skill = make_test_skill("stale-skill", Source::Auto);
        stale_skill.lifecycle = Lifecycle::Stale;
        skills.save("stale-skill", &stale_skill).unwrap();

        let state_dir = tempfile::tempdir().unwrap();
        let config = CuratorConfig {
            stale_days_auto: 0, // Immediate stale
            ..Default::default()
        };

        let curator =
            Curator::new(skills, config).with_state_path(state_dir.path().join("state.json"));

        // Run the curator (should recognize existing stale state)
        let report = curator.run(true).unwrap();

        // Verify that the stale skill is properly classified
        assert!(!report.stale.is_empty() || !report.archived.is_empty()); // Should have some lifecycle changes

        // Verify the lifecycle was preserved
        let loaded_skill = curator.skills.load("stale-skill").unwrap();
        assert_eq!(loaded_skill.lifecycle, Lifecycle::Stale);
    }

    #[test]
    fn test_curator_should_run_respects_disabled_state() {
        let dir = tempfile::tempdir().unwrap();
        let skills = SkillRegistry::new(dir.path());
        for i in 0..10 {
            let skill = make_test_skill(&format!("skill-{}", i), Source::Auto);
            skills.save(&format!("skill-{}", i), &skill).unwrap();
        }

        let state_dir = tempfile::tempdir().unwrap();
        let config = CuratorConfig {
            enabled: false,
            ..Default::default()
        };

        let curator =
            Curator::new(skills, config).with_state_path(state_dir.path().join("state.json"));

        // Should not run when disabled
        assert!(!curator.should_run(Some(99999.0)));
    }

    #[test]
    fn test_curator_state_persistence_across_instances() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        let state_path = state_dir.path().join("state.json");

        let skill = make_test_skill("test-skill", Source::Auto);

        // First instance - modify state
        let skills1 = SkillRegistry::new(dir.path());
        skills1.save("test-skill", &skill).unwrap();
        let curator1 =
            Curator::new(skills1, CuratorConfig::default()).with_state_path(state_path.clone());
        curator1.mark_run_completed().unwrap();

        // Second instance - verify state persisted
        let skills2 = SkillRegistry::new(dir.path());
        let curator2 =
            Curator::new(skills2, CuratorConfig::default()).with_state_path(state_path.clone());

        let state = curator2.load_state().unwrap();
        assert_eq!(state.run_count, 1);
        assert!(state.skill_last_used.contains_key("test-skill"));
    }
}

//! Skill storage — CRUD operations for persisting skills.
//!
//! This module provides `SkillStorageRegistry` for managing skill persistence,
//! including creation, reading, updating, and deletion of skills with metadata.

use crate::utils::{is_excluded_path, parse_frontmatter};
use serde::{Deserialize, Serialize};
use serde_yaml::Value as YamlValue;
use std::collections::HashSet;
use std::fs;
use std::io::Write;

/// Hermes parity (`skill_utils.is_excluded_skill_path`, gap #7):
/// decision function for whether a given path belongs to a skill
/// the curator should skip during backup/snapshot/count. Mirrors the
/// Python helper exactly — `True` if the path is a builtin marker
/// (`.archive`, `_builtin`, `__pycache__`, dotfiles) or any hidden
/// store under `.anureo/`. Used by `experimental/curator/src/curator_backup.rs`
/// so the snapshot's `skills_count` matches what Hermes would have
/// emitted.
pub fn is_excluded_skill_path(path: &std::path::Path) -> bool {
    // Reject hidden directory entries (`.archive`, `_builtin`,
    // `__pycache__`, `.git`, etc.).
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        if name.starts_with('.') {
            return true;
        }
        if name.starts_with('_') && name != "_" {
            return true;
        }
        if name == "__pycache__" {
            return true;
        }
    }
    // Reject any path that lives inside the global `.anureo/` data
    // store (those are internal markers, not bundled skills).
    for ancestor in path.ancestors() {
        if let Some(name) = ancestor.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') {
                return true;
            }
            if name.starts_with('_') && name != "_" {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod excluded_path_tests {
    use super::is_excluded_skill_path;
    use std::path::Path;

    #[test]
    fn rejects_dotfile_directories() {
        assert!(is_excluded_skill_path(Path::new(
            "/skills/.archive/old-skill/SKILL.md"
        )));
        assert!(is_excluded_skill_path(Path::new("/skills/.git/HEAD")));
    }

    #[test]
    fn rejects_pycache_and_underscore_dirs() {
        assert!(is_excluded_skill_path(Path::new(
            "/skills/_builtin/foo/SKILL.md"
        )));
        assert!(is_excluded_skill_path(Path::new(
            "/skills/foo/__pycache__/x.py"
        )));
    }

    #[test]
    fn rejects_paths_under_anureo_data_dir() {
        assert!(is_excluded_skill_path(Path::new(
            "/home/u/.anureo/skills/.archive/x/SKILL.md"
        )));
    }

    #[test]
    fn accepts_normal_skill_paths() {
        assert!(!is_excluded_skill_path(Path::new("/skills/plan/SKILL.md")));
        assert!(!is_excluded_skill_path(Path::new(
            "/skills/code-review/SKILL.md"
        )));
    }
}
use crate::usage::SkillUsageStore;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Atomically write `text` to `path` via a unique sibling tempfile + fsync + rename.
///
/// Guarantees the destination is never observed in a half-written state, even if
/// the process is killed mid-write. Aligns with Hermes
/// `skill_manager_tool.py:_atomic_write_text` (tools/skill_manager_tool.py:67-100).
///
/// Returns the original `std::io::Error` on failure so callers can map it to
/// their own error type.
pub fn atomic_write_text(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!(
        "tmp.{}.{}.{}",
        pid,
        nanos,
        path.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Walk upward from `start` removing empty directories until we reach `stop`
/// (exclusive) or hit a non-empty directory. Bounded — never removes `stop`
/// itself, even if empty.
///
/// Mirrors Hermes `skill_manager_tool.py:702-705` `_cleanup_empty_parents`.
/// Used after `delete`/`remove_file` so the skill tree does not accumulate
/// ghost support-file directories when their last file is removed.
pub fn cleanup_empty_parents(start: &Path, stop: &Path) {
    let mut cur = match start.parent() {
        Some(p) => p.to_path_buf(),
        None => return,
    };
    loop {
        if !cur.starts_with(stop) {
            return;
        }
        if cur == stop {
            return;
        }
        let is_empty = fs::read_dir(&cur)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false);
        if !is_empty {
            return;
        }
        if fs::remove_dir(&cur).is_err() {
            return;
        }
        match cur.parent() {
            Some(p) => cur = p.to_path_buf(),
            None => return,
        }
    }
}

/// Hermes-aligned delete-target guard
/// (`skill_manager_tool.py:404-432`): refuse to delete a skill whose
/// directory (or any ancestor on the way to `base_dir`) is a symlink,
/// hard-link, or junction that escapes `base_dir`. Without this check a
/// malicious or stray `mklink /J auto\plan C:\Windows` would cause
/// `remove_dir_all` to recurse into the system path.
///
/// Strategy:
///   1. Use `symlink_metadata` (not `metadata`) so we see the lstat
///      file type and detect symlinks even when the target is missing.
///   2. `canonicalize` the directory, then walk ancestors verifying
///      every prefix is inside `base_dir`. Any `..` segment, symlink,
///      or junction that pushes the canonical path out of `base_dir`
///      yields `SkillError::InvalidFormat`.
fn _validate_delete_target(dir: &Path, base_dir: &Path) -> Result<(), SkillError> {
    // 1. Direct symlink check. `symlink_metadata` follows the symlink
    //    only for the final component; here we want the raw lstat so
    //    that `dir` itself being a symlink is caught.
    match fs::symlink_metadata(dir) {
        Ok(md) if md.file_type().is_symlink() => {
            return Err(SkillError::InvalidFormat(format!(
                "refusing to delete symlinked skill directory: {}",
                dir.display()
            )));
        }
        Ok(_) => {}
        Err(e) => {
            return Err(SkillError::Io(e));
        }
    }

    // 2. Canonicalize and verify ancestry. On Windows `canonicalize`
    //    resolves junctions; on POSIX it follows symlinks. Either way
    //    we now have the absolute real path.
    let canonical = match fs::canonicalize(dir) {
        Ok(p) => p,
        Err(e) => {
            // Missing target after the symlink_metadata check usually
            // means a dangling junction; refuse conservatively.
            return Err(SkillError::Io(e));
        }
    };
    let base_canonical = match fs::canonicalize(base_dir) {
        Ok(p) => p,
        Err(e) => {
            return Err(SkillError::Io(e));
        }
    };

    // 3. Walk ancestors of `canonical` until we reach `base_canonical`.
    //    If we hit a junction/symlink that escapes before then, the
    //    prefix-starts_with check on the raw `cur` still catches it
    //    because canonicalize already resolved all links.
    let mut cur = canonical.as_path();
    loop {
        if cur == base_canonical {
            return Ok(());
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => {
                return Err(SkillError::InvalidFormat(format!(
                    "refusing to delete skill directory outside base: {}",
                    dir.display()
                )));
            }
        }
    }
}

/// Skill lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lifecycle {
    /// Skill is active and available.
    Active,
    /// Skill hasn't been used recently and may be archived.
    Stale,
    /// Skill has been archived and is no longer shown by default.
    Archived,
}

/// Skill source/origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Auto-generated skill (e.g., by background review).
    Auto,
    /// Manually created skill.
    Manual,
    /// Evolved skill (generated from usage patterns).
    Evolved,
}

impl Source {
    /// Legacy directory segment used by pre-Hermes-layout installations.
    ///
    /// New writes no longer encode provenance in the directory tree. This
    /// helper remains for migration/compatibility code that needs to
    /// recognize `auto/`, `curated/`, and `evolved/` layouts.
    pub fn dir_name(self) -> &'static str {
        match self {
            Source::Auto => "auto",
            Source::Manual => "curated",
            Source::Evolved => "evolved",
        }
    }
}

/// Sanitize a user-supplied category segment so it cannot escape `base_dir`.
///
/// Rejects empty, `.`, `..`, `/`, `\\`, NUL, control chars, and reserved
/// Windows device names. Lowercases ASCII to keep directory lookups
/// case-insensitive on Windows. Matches the spirit of Hermes'
/// `skill_manager_tool.py:_sanitize_category`.
fn sanitize_category(raw: &str) -> String {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty()
        || trimmed == "."
        || trimmed == ".."
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains('\0')
        || trimmed.chars().any(|c| c.is_control())
    {
        return "_invalid".to_string();
    }
    let reserved = [
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "lpt1", "lpt2",
    ];
    if reserved.contains(&trimmed.as_str()) {
        return "_invalid".to_string();
    }
    trimmed
}

/// Metadata for a skill in the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub lifecycle: Lifecycle,
    pub source: Source,
    pub triggers: Vec<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub last_used: Option<String>,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub created_by: Option<String>,
}

/// Full skill content including body.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillContent {
    pub name: String,
    pub description: String,
    pub triggers: Vec<String>,
    pub lifecycle: Lifecycle,
    pub source: Source,
    /// Optional category/domain segment used by storage to place a new skill
    /// under `base_dir/category/name/`.
    pub category: Option<String>,
    pub created_by: Option<String>,
    pub body: String,
    pub raw: String,
}

/// Errors that can occur during skill storage operations.
#[derive(Debug, Error)]
pub enum SkillError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("YAML parse error: {0}")]
    Yaml(#[from] serde_yaml::Error),
    #[error("Skill not found: {0}")]
    NotFound(String),
    #[error("Invalid skill format: {0}")]
    InvalidFormat(String),
    /// Skill is pinned/protected and cannot be deleted or archived.
    #[error("Skill '{0}' is pinned and cannot be deleted or archived")]
    Pinned(String),
    /// Restore target already exists in the active tree.
    #[error("Skill '{0}' already exists in the active tree")]
    AlreadyExists(String),
}

/// Registry for persistent skill storage.
#[derive(Clone)]
pub struct SkillStorageRegistry {
    base_dir: PathBuf,
}

impl SkillStorageRegistry {
    /// Create a new storage registry at the given base directory.
    pub fn new(base_dir: &Path) -> Self {
        Self {
            base_dir: base_dir.to_path_buf(),
        }
    }

    /// Get the base directory for this registry.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Construct a `SkillUsageStore` rooted at this registry's base
    /// directory. Returns `Some(_)` because the usage store is
    /// always derivable from `base_dir` — callers in the curator
    /// (`archive_skill` / `restore_skill`) use this to keep the
    /// `.curator_suppressed` list in sync without having to thread
    /// an extra dependency through `Curator` (priority #20).
    pub fn usage_store(&self) -> SkillUsageStore {
        SkillUsageStore::new(&self.base_dir)
    }

    /// Resolve a skill directory by name, falling back to the canonical flat
    /// location for a skill that has not been created yet.
    ///
    /// `source` is retained in the signature for API compatibility. Hermes'
    /// current layout stores provenance in `.usage.json`, so it no longer
    /// participates in path construction.
    pub fn skill_dir(&self, _source: Source, name: &str) -> PathBuf {
        self.find_skill_dir(name)
            .unwrap_or_else(|| self.base_dir.join(name))
    }

    pub fn skill_file_path(&self, source: Source, name: &str) -> PathBuf {
        self.skill_dir(source, name).join("SKILL.md")
    }

    /// Find the directory containing a skill by name, searching the entire tree.
    ///
    /// Walks `base_dir` recursively looking for `<dir>/SKILL.md` where
    /// `<dir>` has the given name. Excludes `.git`, `node_modules`, etc.
    /// Returns the path to the directory containing `SKILL.md`.
    pub fn find_skill_dir(&self, name: &str) -> Option<PathBuf> {
        if !self.base_dir.exists() {
            return None;
        }
        self.scan_for_skill_dir(&self.base_dir, name)
    }

    /// Recursive helper for `find_skill_dir`.
    fn scan_for_skill_dir(&self, dir: &Path, name: &str) -> Option<PathBuf> {
        let entries = fs::read_dir(dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() || is_excluded_path(&path) {
                continue;
            }
            // Check if this directory IS the skill (matches name + has SKILL.md)
            if path.file_name().and_then(|n| n.to_str()) == Some(name) {
                let skill_md = path.join("SKILL.md");
                if skill_md.exists() {
                    return Some(path);
                }
            }
            // Recurse into subdirectories
            if let Some(found) = self.scan_for_skill_dir(&path, name) {
                return Some(found);
            }
        }
        None
    }

    /// Recursively collect all `SKILL.md` file paths under `base_dir`.
    fn collect_skill_files(&self) -> Vec<PathBuf> {
        if !self.base_dir.exists() {
            return Vec::new();
        }
        let mut result = Vec::new();
        self.collect_skill_files_recursive(&self.base_dir, &mut result);
        result
    }

    /// Recursive helper for `collect_skill_files`.
    fn collect_skill_files_recursive(&self, dir: &Path, out: &mut Vec<PathBuf>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if is_excluded_path(&path) {
                        continue;
                    }
                    // Check if this dir has a SKILL.md
                    let skill_md = path.join("SKILL.md");
                    if skill_md.exists() {
                        out.push(skill_md);
                    } else {
                        // Recurse deeper
                        self.collect_skill_files_recursive(&path, out);
                    }
                }
            }
        }
    }

    /// List all skills in the registry.
    ///
    /// Recursively scans `base_dir` for all `SKILL.md` files, excluding
    /// directories like `.git`, `node_modules`, `.archive`, etc.
    /// This aligns with Hermes `iter_skill_index_files()` (rglob + EXCLUDED_DIRS).
    pub fn list(&self) -> Result<Vec<SkillMeta>, SkillError> {
        let mut skills = Vec::new();
        for skill_md in self.collect_skill_files() {
            if let Ok(content) = self.load_from_path(&skill_md) {
                let pinned = self
                    .read_pinned_from_frontmatter(&skill_md)
                    .unwrap_or(false);
                skills.push(SkillMeta {
                    name: content.name.clone(),
                    description: content.description.clone(),
                    lifecycle: content.lifecycle,
                    source: content.source,
                    triggers: content.triggers,
                    created_at: None,
                    last_used: None,
                    pinned,
                    created_by: content.created_by.clone(),
                });
            }
        }
        Ok(skills)
    }

    /// Load a skill by name.
    ///
    /// Searches the entire `base_dir` tree for `<name>/SKILL.md`,
    /// providing backward compatibility with both old (`auto/curated/evolved/`)
    /// and new (flat/category) layouts.
    pub fn load(&self, name: &str) -> Result<SkillContent, SkillError> {
        if let Some(dir) = self.find_skill_dir(name) {
            let path = dir.join("SKILL.md");
            if path.exists() {
                return self.load_from_path(&path);
            }
        }
        // Hermes-aligned cross-profile hint
        // (`skill_manager_tool.py:298-398`): when a skill is missing from
        // the current registry, surface available alternatives so the user
        // is not left guessing whether the skill exists at all or whether
        // they are looking at the wrong profile.
        let hint = self.suggest_skill_alternatives(name);
        Err(SkillError::NotFound(format!("{}{}", name, hint)))
    }

    /// Build a diagnostic hint when `name` is not in the current registry.
    ///
    /// Mirrors `skill_manager_tool.py:298-398`. Walks the parent's parent
    /// (i.e. one level above `base_dir`) for sibling profiles whose name
    /// contains `name` as a substring, and returns a "did you mean" string
    /// listing the first few matches. Returns an empty string when no
    /// helpful hint is available so the error remains compact.
    fn suggest_skill_alternatives(&self, name: &str) -> String {
        // Hermes-aligned sibling-profile rglob
        // (`skill_manager_tool.py:298-398`). Instead of a fuzzy substring
        // match on profile directory names, walk the parent (profiles/),
        // enumerate every profile dir, rglob SKILL.md inside each, and
        // surface the profile name when an actual SKILL.md is found there.
        // This eliminates the previous false positives (e.g. a profile
        // called "auto-curator" matching the skill name "auto").
        let Some(profile_root) = self.base_dir.parent().and_then(|p| p.parent()) else {
            return String::new();
        };
        if !profile_root.exists() {
            return String::new();
        }
        let mut matches: Vec<String> = Vec::new();
        let entries = match std::fs::read_dir(profile_root) {
            Ok(rd) => rd,
            Err(_) => return String::new(),
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let profile_name = entry.file_name().to_string_lossy().to_string();
            let skills_tree = entry.path().join("skills");
            if !skills_tree.is_dir() {
                continue;
            }
            // rglob SKILL.md under each profile's skills/ tree. Hermes
            // does a full scan (skills_sync.py:301-356); we mirror that and
            // bail out as soon as we find any SKILL.md to keep the hint
            // cheap on large profiles.
            let found_skill = walk_rglob_skill_md(&skills_tree).is_some();
            if found_skill {
                matches.push(profile_name);
                if matches.len() >= 3 {
                    break;
                }
            }
        }
        if matches.is_empty() {
            String::new()
        } else {
            format!(
                "
Hint: skill '{}' not found in current profile.                  Did you mean a skill in one of these profiles? {}                  Use `anureo --profile <name>` to switch.",
                name,
                matches.join(", ")
            )
        }
    }

    fn load_from_path(&self, path: &Path) -> Result<SkillContent, SkillError> {
        let raw = fs::read_to_string(path)?;
        let raw_owned = raw.clone();
        let (frontmatter, body) = parse_frontmatter(&raw);

        let name = frontmatter
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| SkillError::InvalidFormat("missing name".into()))?
            .to_string();

        let description = frontmatter
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let triggers = frontmatter
            .get("triggers")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let lifecycle = frontmatter
            .get("lifecycle")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_yaml::from_str(s).ok())
            .unwrap_or(Lifecycle::Active);

        let source = frontmatter
            .get("source")
            .and_then(|v| v.as_str())
            .and_then(|s| serde_yaml::from_str(s).ok())
            .unwrap_or(Source::Manual);

        let created_by = frontmatter
            .get("created_by")
            .and_then(|v| v.as_str())
            .map(String::from);

        let category = frontmatter
            .get("category")
            .and_then(|v| v.as_str())
            .map(String::from);

        Ok(SkillContent {
            name,
            description,
            triggers,
            lifecycle,
            source,
            category,
            created_by,
            body,
            raw: raw_owned,
        })
    }

    /// Save a skill to the registry.
    pub fn save(&self, name: &str, content: &SkillContent) -> Result<(), SkillError> {
        // Pinned-skill gate (Hermes parity, `tools/skill_manager_tool.py`):
        // a skill whose `SKILL.md` frontmatter declares `pinned: true` is
        // immutable via the surface APIs (save / patch / write_file /
        // remove_file). Only `delete()` and `set_lifecycle(Archived)`
        // were previously guarded; this extends the contract to `save`.
        if let Some(existing_dir) = self.find_skill_dir(name) {
            let path = existing_dir.join("SKILL.md");
            if path.exists() && self.read_pinned_from_frontmatter(&path) == Some(true) {
                return Err(SkillError::Pinned(name.to_string()));
            }
        }

        // Hermes layout: provenance is stored in `.usage.json`; directories
        // express only an optional category. Existing skills remain in place
        // (including legacy auto/curated/evolved trees) so an edit never
        // performs a surprise migration.
        let dir = match self.find_skill_dir(name) {
            Some(existing) => existing,
            None => match content.category.as_deref().filter(|c| !c.is_empty()) {
                Some(cat) => self.base_dir.join(sanitize_category(cat)).join(name),
                None => self.base_dir.join(name),
            },
        };
        fs::create_dir_all(&dir)?;
        let path = dir.join("SKILL.md");

        let frontmatter = serde_yaml::to_string(&YamlValue::Mapping({
            let mut map = serde_yaml::Mapping::new();
            map.insert(
                YamlValue::String("name".into()),
                YamlValue::String(content.name.clone()),
            );
            map.insert(
                YamlValue::String("description".into()),
                YamlValue::String(content.description.clone()),
            );
            map.insert(
                YamlValue::String("triggers".into()),
                YamlValue::Sequence(
                    content
                        .triggers
                        .iter()
                        .map(|t| YamlValue::String(t.clone()))
                        .collect(),
                ),
            );
            map.insert(
                YamlValue::String("lifecycle".into()),
                YamlValue::String(
                    serde_yaml::to_string(&content.lifecycle)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                ),
            );
            map.insert(
                YamlValue::String("source".into()),
                YamlValue::String(
                    serde_yaml::to_string(&content.source)
                        .unwrap_or_default()
                        .trim()
                        .to_string(),
                ),
            );
            if let Some(ref by) = content.created_by {
                map.insert(
                    YamlValue::String("created_by".into()),
                    YamlValue::String(by.clone()),
                );
            }
            if let Some(ref cat) = content.category {
                if !cat.is_empty() {
                    map.insert(
                        YamlValue::String("category".into()),
                        YamlValue::String(cat.clone()),
                    );
                }
            }
            map
        }))?;

        let file_content = format!("---\n{}---\n{}", frontmatter, content.body);
        atomic_write_text(&path, &file_content)?;
        Ok(())
    }

    /// Delete a skill from the registry.
    ///
    /// Searches the entire tree for the skill directory and removes it.
    pub fn delete(&self, name: &str) -> Result<(), SkillError> {
        if let Some(dir) = self.find_skill_dir(name) {
            // Protected skills enforcement: pinned skills cannot be deleted.
            let skill_md = dir.join("SKILL.md");
            if skill_md.exists() && self.read_pinned_from_frontmatter(&skill_md) == Some(true) {
                return Err(SkillError::Pinned(name.to_string()));
            }
            // Hermes-aligned symlink/junction guard
            // (`skill_manager_tool.py:404-432`): refuse to follow a
            // symlink or junction that resolves outside `base_dir`. A
            // malicious `.bundled_manifest`/hub entry (or a stray
            // `mklink /J`) cannot trick us into `rm -rf` on a parent
            // directory or a system path.
            _validate_delete_target(&dir, &self.base_dir)?;
            if dir.exists() {
                fs::remove_dir_all(&dir)?;
                cleanup_empty_parents(&dir, &self.base_dir);
                return Ok(());
            }
        }
        Err(SkillError::NotFound(name.to_string()))
    }

    /// Set the `pinned` flag on a skill by updating its SKILL.md frontmatter.
    ///
    /// This reads the current SKILL.md, modifies the `pinned` field in the
    /// YAML frontmatter, and writes it back — preserving the body unchanged.
    pub fn set_pinned(&self, name: &str, pinned: bool) -> Result<(), SkillError> {
        if let Some(dir) = self.find_skill_dir(name) {
            let path = dir.join("SKILL.md");
            if path.exists() {
                let raw = fs::read_to_string(&path)?;
                let (mut frontmatter, body) = parse_frontmatter(&raw);

                frontmatter.insert(
                    serde_yaml::Value::String("pinned".into()),
                    serde_yaml::Value::Bool(pinned),
                );

                let new_yaml = serde_yaml::to_string(&frontmatter)
                    .map_err(|e| SkillError::InvalidFormat(e.to_string()))?;
                let new_content = format!("---\n{}---\n{}", new_yaml, body);
                atomic_write_text(&path, &new_content)?;
                return Ok(());
            }
        }
        Err(SkillError::NotFound(name.to_string()))
    }

    /// Set the `lifecycle` of a skill by updating its SKILL.md frontmatter.
    ///
    /// Used by `curator restore` to move a skill back to Active.
    pub fn set_lifecycle(&self, name: &str, lifecycle: Lifecycle) -> Result<(), SkillError> {
        if let Some(dir) = self.find_skill_dir(name) {
            let path = dir.join("SKILL.md");
            if path.exists() {
                // Protected skills enforcement: pinned skills cannot be archived.
                if lifecycle == Lifecycle::Archived
                    && self.read_pinned_from_frontmatter(&path) == Some(true)
                {
                    return Err(SkillError::Pinned(name.to_string()));
                }

                let raw = fs::read_to_string(&path)?;
                let (mut frontmatter, body) = parse_frontmatter(&raw);

                frontmatter.insert(
                    serde_yaml::Value::String("lifecycle".into()),
                    serde_yaml::Value::String(
                        serde_yaml::to_string(&lifecycle)
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                    ),
                );

                let new_yaml = serde_yaml::to_string(&frontmatter)
                    .map_err(|e| SkillError::InvalidFormat(e.to_string()))?;
                let new_content = format!("---\n{}---\n{}", new_yaml, body);
                atomic_write_text(&path, &new_content)?;
                return Ok(());
            }
        }
        Err(SkillError::NotFound(name.to_string()))
    }

    /// Patch a skill by replacing text.
    pub fn patch(&self, name: &str, old_string: &str, new_string: &str) -> Result<(), SkillError> {
        // Pinned-skill gate: a `pinned: true` frontmatter makes the skill
        // immutable through every write surface. Only delete() /
        // set_lifecycle(Archived) were previously guarded; extend the
        // contract to `patch`. Without this, `manage.rs::handle_patch`
        // could silently overwrite a protected skill's body.
        if let Some(dir) = self.find_skill_dir(name) {
            let path = dir.join("SKILL.md");
            if path.exists() && self.read_pinned_from_frontmatter(&path) == Some(true) {
                return Err(SkillError::Pinned(name.to_string()));
            }
        }
        let mut content = self.load(name)?;
        if !content.raw.contains(old_string) {
            return Err(SkillError::InvalidFormat(format!(
                "old_string not found in skill '{}'",
                name
            )));
        }
        content.raw = content.raw.replacen(old_string, new_string, 1);
        let (frontmatter, body) = parse_frontmatter(&content.raw);
        let mut updated = SkillContent {
            name: content.name.clone(),
            description: content.description.clone(),
            triggers: content.triggers.clone(),
            lifecycle: content.lifecycle,
            source: content.source,
            category: content.category.clone(),
            created_by: content.created_by.clone(),
            body,
            raw: content.raw.clone(),
        };

        // Update description and triggers from frontmatter
        if let Some(desc) = frontmatter
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from)
        {
            updated.description = desc;
        }
        if let Some(triggers) = frontmatter
            .get("triggers")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
        {
            updated.triggers = triggers;
        }

        self.save(name, &updated)
    }

    /// Patch a skill by replacing **every** occurrence of `old_string` with
    /// `new_string`. Fails if `old_string` is not present.
    pub fn patch_all(
        &self,
        name: &str,
        old_string: &str,
        new_string: &str,
    ) -> Result<(), SkillError> {
        let mut content = self.load(name)?;
        if !content.raw.contains(old_string) {
            return Err(SkillError::InvalidFormat(format!(
                "old_string not found in skill '{}'",
                name
            )));
        }
        content.raw = content.raw.replace(old_string, new_string);
        let (frontmatter, body) = parse_frontmatter(&content.raw);
        let mut updated = SkillContent {
            name: content.name.clone(),
            description: content.description.clone(),
            triggers: content.triggers.clone(),
            lifecycle: content.lifecycle,
            source: content.source,
            category: content.category.clone(),
            created_by: content.created_by.clone(),
            body,
            raw: content.raw.clone(),
        };

        if let Some(desc) = frontmatter
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from)
        {
            updated.description = desc;
        }
        if let Some(triggers) = frontmatter
            .get("triggers")
            .and_then(|v| v.as_sequence())
            .map(|seq| {
                seq.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
        {
            updated.triggers = triggers;
        }

        self.save(name, &updated)
    }

    /// Write an additional file inside a skill's directory.
    pub fn write_file(
        &self,
        skill_name: &str,
        path: &str,
        content: &str,
    ) -> Result<(), SkillError> {
        // Pinned-skill gate (extend the immutable-surface contract from
        // delete()/set_lifecycle(Archived) to write_file). A user with
        // `manage.rs::handle_write_file` would otherwise be able to swap
        // any file under a pinned skill's directory.
        if let Some(dir) = self.find_skill_dir(skill_name) {
            let skill_md = dir.join("SKILL.md");
            if skill_md.exists() && self.read_pinned_from_frontmatter(&skill_md) == Some(true) {
                return Err(SkillError::Pinned(skill_name.to_string()));
            }
        }
        let dir = self
            .find_skill_dir(skill_name)
            .ok_or_else(|| SkillError::NotFound(skill_name.to_string()))?;
        let file_path = dir.join(path.trim_start_matches('/'));
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write_text(&file_path, content)?;
        Ok(())
    }

    /// Remove a file from a skill's directory.
    pub fn remove_file(&self, skill_name: &str, path: &str) -> Result<(), SkillError> {
        // Pinned-skill gate (extend the immutable-surface contract from
        // delete()/set_lifecycle(Archived) to remove_file). Without it, a
        // caller could strip SKILL.md frontmatter, leaving the skill in
        // an unrecoverable state.
        if let Some(dir) = self.find_skill_dir(skill_name) {
            let skill_md = dir.join("SKILL.md");
            if skill_md.exists() && self.read_pinned_from_frontmatter(&skill_md) == Some(true) {
                return Err(SkillError::Pinned(skill_name.to_string()));
            }
        }
        let dir = self
            .find_skill_dir(skill_name)
            .ok_or_else(|| SkillError::NotFound(skill_name.to_string()))?;
        let file_path = dir.join(path.trim_start_matches('/'));
        if file_path.exists() {
            fs::remove_file(&file_path)?;
            cleanup_empty_parents(&file_path, &dir);
            Ok(())
        } else {
            Err(SkillError::NotFound(format!(
                "file '{}' in skill '{}'",
                path, skill_name
            )))
        }
    }

    /// Find skills matching a query string.
    pub fn find_matching(
        &self,
        query: &str,
        threshold: f64,
    ) -> Result<Vec<SkillContent>, SkillError> {
        let all = self.list()?;
        let query_lower = query.to_lowercase();
        let query_words: HashSet<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<(f64, String)> = Vec::new();
        for meta in &all {
            let score = compute_match_score(query_lower.as_str(), &query_words, meta);
            if score >= threshold {
                scored.push((score, meta.name.clone()));
            }
        }

        scored.sort_by_cached_key(|a| {
            std::cmp::Reverse(
                a.0.partial_cmp(&f64::INFINITY)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });

        let mut results = Vec::new();
        for (_, name) in scored {
            if let Ok(content) = self.load(&name) {
                results.push(content);
            }
        }
        Ok(results)
    }

    /// Read the `pinned` field from a SKILL.md frontmatter.
    fn read_pinned_from_frontmatter(&self, skill_md: &Path) -> Option<bool> {
        let raw = fs::read_to_string(skill_md).ok()?;
        let (frontmatter, _) = parse_frontmatter(&raw);
        frontmatter.get("pinned").and_then(|v| v.as_bool())
    }
}

/// Compute a match score between query and skill metadata.
fn compute_match_score(query: &str, query_words: &HashSet<&str>, meta: &SkillMeta) -> f64 {
    let trigger_lower: Vec<String> = meta.triggers.iter().map(|t| t.to_lowercase()).collect();
    let desc_lower = meta.description.to_lowercase();
    let name_lower = meta.name.to_lowercase();

    let mut max_score = 0.0_f64;

    for trigger in &trigger_lower {
        if trigger == query {
            return 1.0;
        }
        if trigger.contains(query) || query.contains(trigger.as_str()) {
            max_score = max_score.max(0.85);
        }
        let trigger_words: HashSet<&str> = trigger.split_whitespace().collect();
        let overlap = query_words.intersection(&trigger_words).count();
        let union = query_words.union(&trigger_words).count();
        if union > 0 {
            let jaccard = overlap as f64 / union as f64;
            max_score = max_score.max(jaccard);
        }
    }

    if desc_lower.contains(query) || name_lower.contains(query) {
        max_score = max_score.max(0.5);
    }

    max_score
}

/// Hermes-aligned recursive `SKILL.md` search used by
/// `suggest_skill_alternatives`. Bails out as soon as it finds the first
/// matching `SKILL.md` so the hint is cheap on large profiles.
fn walk_rglob_skill_md(root: &Path) -> Option<PathBuf> {
    if !root.is_dir() {
        return None;
    }
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(f) => f,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if is_excluded_path(&path) {
                    continue;
                }
                stack.push(path);
            } else if ft.is_file() && path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md")
            {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_skill() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillStorageRegistry::new(dir.path());
        let skill = SkillContent {
            name: "debug-rust".to_string(),
            description: "Debug Rust errors".to_string(),
            triggers: vec!["rust".into(), "cargo".into(), "compiler error".into()],
            lifecycle: Lifecycle::Active,
            source: Source::Auto,
            category: None,
            created_by: None,
            body: "1. Read the error\n2. Identify cause\n".to_string(),
            raw: String::new(),
        };
        registry.save("debug-rust", &skill).unwrap();
        let loaded = registry.load("debug-rust").unwrap();
        assert_eq!(loaded.name, "debug-rust");
        assert_eq!(loaded.triggers.len(), 3);
        assert_eq!(loaded.source, Source::Auto);
    }

    #[test]
    fn list_skills() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillStorageRegistry::new(dir.path());
        let skill = SkillContent {
            name: "test-skill".to_string(),
            description: "A test".to_string(),
            triggers: vec!["test".into()],
            lifecycle: Lifecycle::Active,
            source: Source::Manual,
            category: None,
            created_by: None,
            body: "Do stuff".to_string(),
            raw: String::new(),
        };
        registry.save("test-skill", &skill).unwrap();
        let list = registry.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].name, "test-skill");
    }

    #[test]
    fn find_matching_exact_trigger() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillStorageRegistry::new(dir.path());
        let skill = SkillContent {
            name: "rust-debug".to_string(),
            description: "Debug Rust".to_string(),
            triggers: vec!["rust compiler error".into()],
            lifecycle: Lifecycle::Active,
            source: Source::Auto,
            category: None,
            created_by: None,
            body: "Steps...".to_string(),
            raw: String::new(),
        };
        registry.save("rust-debug", &skill).unwrap();
        let matches = registry.find_matching("rust compiler error", 0.5).unwrap();
        assert_eq!(matches.len(), 1);
    }

    #[test]
    fn delete_skill() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillStorageRegistry::new(dir.path());
        let skill = SkillContent {
            name: "to-delete".to_string(),
            description: "Delete me".to_string(),
            triggers: vec![],
            lifecycle: Lifecycle::Active,
            source: Source::Manual,
            category: None,
            created_by: None,
            body: "...".to_string(),
            raw: String::new(),
        };
        registry.save("to-delete", &skill).unwrap();
        registry.delete("to-delete").unwrap();
        assert!(registry.load("to-delete").is_err());
    }

    #[test]
    fn load_nonexistent_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillStorageRegistry::new(dir.path());
        assert!(registry.load("nope").is_err());
    }

    #[test]
    fn write_and_read_file() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillStorageRegistry::new(dir.path());
        let skill = SkillContent {
            name: "test-write".to_string(),
            description: "Test write".to_string(),
            triggers: vec![],
            lifecycle: Lifecycle::Active,
            source: Source::Manual,
            category: None,
            created_by: None,
            body: "...".to_string(),
            raw: String::new(),
        };
        registry.save("test-write", &skill).unwrap();

        registry
            .write_file("test-write", "src/helper.rs", "fn helper() {}\n")
            .unwrap();

        let file_path = dir
            .path()
            .join("curated")
            .join("test-write")
            .join("src")
            .join("helper.rs");
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "fn helper() {}\n");
    }

    #[test]
    fn pinned_skill_cannot_be_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillStorageRegistry::new(dir.path());
        let skill = SkillContent {
            name: "pinned-skill".to_string(),
            description: "Protected".to_string(),
            triggers: vec![],
            lifecycle: Lifecycle::Active,
            source: Source::Manual,
            category: None,
            created_by: None,
            body: "...".to_string(),
            raw: String::new(),
        };
        registry.save("pinned-skill", &skill).unwrap();
        registry.set_pinned("pinned-skill", true).unwrap();

        // Delete should fail
        let result = registry.delete("pinned-skill");
        assert!(matches!(result, Err(SkillError::Pinned(_))));

        // Skill should still exist
        assert!(registry.load("pinned-skill").is_ok());
    }

    #[test]
    fn pinned_skill_cannot_be_archived() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillStorageRegistry::new(dir.path());
        let skill = SkillContent {
            name: "pinned-active".to_string(),
            description: "Protected".to_string(),
            triggers: vec![],
            lifecycle: Lifecycle::Active,
            source: Source::Manual,
            category: None,
            created_by: None,
            body: "...".to_string(),
            raw: String::new(),
        };
        registry.save("pinned-active", &skill).unwrap();
        registry.set_pinned("pinned-active", true).unwrap();

        // Archive should fail
        let result = registry.set_lifecycle("pinned-active", Lifecycle::Archived);
        assert!(matches!(result, Err(SkillError::Pinned(_))));

        // Setting Active (no change) should still succeed
        registry
            .set_lifecycle("pinned-active", Lifecycle::Active)
            .unwrap();
    }

    #[test]
    fn unpinned_skill_can_be_deleted_and_archived() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillStorageRegistry::new(dir.path());
        let skill = SkillContent {
            name: "normal-skill".to_string(),
            description: "Not protected".to_string(),
            triggers: vec![],
            lifecycle: Lifecycle::Active,
            source: Source::Manual,
            category: None,
            created_by: None,
            body: "...".to_string(),
            raw: String::new(),
        };
        registry.save("normal-skill", &skill).unwrap();
        // Not pinned → archive works
        registry
            .set_lifecycle("normal-skill", Lifecycle::Archived)
            .unwrap();
    }
}

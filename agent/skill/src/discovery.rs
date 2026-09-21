//! Skill discovery — scanning and loading skills from various locations.

use crate::utils::{parse_skill_frontmatter, SkillMetadata};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use thiserror::Error;

const SKILLS_SUBDIR: &str = ".anureo/skills";
const SKILL_MD: &str = "SKILL.md";
const DESCRIPTION_MD: &str = "DESCRIPTION.md";
const SKILL_EXTENSIONS: &[&str] = &["md", "txt", "markdown"];

#[derive(Debug, Error)]
pub enum SkillDiscoveryError {
    #[error("skill not found: {0}")]
    NotFound(String),
    #[error("read skill {path}: {source}")]
    ReadFailed {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("parse skill {path}: {reason}")]
    ParseFailed { path: PathBuf, reason: String },
}

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub metadata: SkillMetadata,
    pub base_path: PathBuf,
    pub skill_file: PathBuf,
    pub source: SkillSource,
    /// For `Source::Builtin` entries: the SKILL.md content embedded in the
    /// binary via `include_str!`. When `Some`, [`SkillRegistry::load_skill_with_dir`]
    /// uses it directly instead of reading from `skill_file`.
    pub embedded_content: Option<String>,
    /// For `Source::Builtin` entries: reference files bundled alongside the
    /// SKILL.md (mirrors `references/*.md` for filesystem skills). When `Some`,
    /// [`SkillRegistry::load_skill_with_dir`] lists them under
    /// `## Additional resources` so the agent can `read` them on demand.
    /// Each entry is `(name, content)` - same shape as `tool_core::EmbeddedRef`,
    /// kept as a plain tuple to avoid a `skill -> tool-core` crate dependency.
    pub embedded_files: Option<Vec<(String, String)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillSource {
    Project,
    ProfileDir,
    User,
    Agent,
    Data,
    /// Skill bundled inside a anureo crate (e.g. via `include_str!`). Added by
    /// [`SkillRegistry::add_builtin`] after filesystem discovery completes.
    /// Lower priority than filesystem-discovered entries with the same name.
    Builtin,
}

impl SkillSource {
    /// Short human label for display in CLI banner / log output.
    pub fn label(&self) -> &'static str {
        match self {
            SkillSource::Project => "Project",
            SkillSource::ProfileDir => "Profile",
            SkillSource::User => "User",
            SkillSource::Agent => "Agent",
            SkillSource::Data => "Data",
            SkillSource::Builtin => "Builtin",
        }
    }
}

pub struct SkillRegistry {
    pub skills: Vec<SkillEntry>,
    cache: std::sync::Mutex<crate::cache::SkillCache>,
}

impl Clone for SkillRegistry {
    fn clone(&self) -> Self {
        let cache = self.cache.lock().map(|c| c.clone()).unwrap_or_default();
        Self {
            skills: self.skills.clone(),
            cache: std::sync::Mutex::new(cache),
        }
    }
}

impl SkillRegistry {
    pub fn empty() -> Self {
        Self {
            skills: Vec::new(),
            cache: std::sync::Mutex::new(crate::cache::SkillCache::new()),
        }
    }

    pub fn discover(
        working_folder: &Path,
        extra_dirs: &[PathBuf],
    ) -> Result<Self, SkillDiscoveryError> {
        let mut skills = Vec::new();

        // 1. Project skills
        let project_dir = working_folder.join(SKILLS_SUBDIR);
        if project_dir.is_dir() {
            skills.extend(scan_skills_dir_recursive(
                &project_dir,
                SkillSource::Project,
            ));
        }

        // 2. Extra/profile directories
        for dir in extra_dirs {
            if dir.is_dir() {
                skills.extend(scan_skills_dir_recursive(dir, SkillSource::ProfileDir));
            }
        }

        // 3. User skills
        let user_dir = env_config::home::anureo_home().join("skills");
        if user_dir.is_dir() {
            skills.extend(scan_skills_dir_recursive(&user_dir, SkillSource::User));
        }

        // 4. Data skills (recursive)
        let data_dir = env_config::home::anureo_home().join("data").join("skills");
        if data_dir.is_dir() {
            skills.extend(scan_skills_dir_recursive(&data_dir, SkillSource::Data));
        }

        // Deduplicate by name (first source wins)
        let mut seen = HashSet::new();
        skills.retain(|e| seen.insert(e.metadata.name.clone()));

        Ok(Self {
            skills,
            cache: std::sync::Mutex::new(crate::cache::SkillCache::new()),
        })
    }

    pub fn add_agent_skills(&mut self, agent_skills_dir: &Path) -> Result<(), SkillDiscoveryError> {
        if !agent_skills_dir.is_dir() {
            return Ok(());
        }
        let new_skills = scan_skills_dir_recursive(agent_skills_dir, SkillSource::Agent);
        for entry in new_skills {
            if !self
                .skills
                .iter()
                .any(|e| e.metadata.name == entry.metadata.name)
            {
                self.skills.push(entry);
            }
        }
        Ok(())
    }

    /// Register a builtin skill bundled inside a anureo crate.
    ///
    /// If a skill with the same name already exists (from any filesystem
    /// source), the call is a no-op — user/project skills take precedence
    /// over builtins.
    ///
    /// `requires_tools` is exposed via the SKILL.md `metadata.conditions.requires_tools`
    /// field so [`Self::apply_toolset_filters`] can hide the skill when its
    /// dependent tool is not registered.
    pub fn add_builtin(
        &mut self,
        name: &str,
        description: &str,
        content: &str,
        triggers: Vec<String>,
        requires_tools: Vec<String>,
        references: Vec<(String, String)>,
    ) {
        if self.skills.iter().any(|e| e.metadata.name == name) {
            return;
        }
        let metadata = SkillMetadata {
            name: name.to_string(),
            description: description.to_string(),
            triggers,
            metadata: if requires_tools.is_empty() {
                None
            } else {
                Some(crate::utils::SkillMetadataBlock {
                    conditions: crate::utils::SkillConditions {
                        requires_tools,
                        ..Default::default()
                    },
                    ..Default::default()
                })
            },
            ..Default::default()
        };
        self.skills.push(SkillEntry {
            metadata,
            base_path: PathBuf::new(),
            skill_file: PathBuf::new(),
            source: SkillSource::Builtin,
            embedded_content: Some(content.to_string()),
            embedded_files: if references.is_empty() {
                None
            } else {
                Some(references)
            },
        });
    }

    pub fn list(&self) -> &[SkillEntry] {
        &self.skills
    }

    pub fn load_skill(&self, name: &str) -> Result<String, SkillDiscoveryError> {
        let (content, _) = self.load_skill_with_dir(name)?;
        Ok(content)
    }

    pub fn load_skill_with_dir(
        &self,
        name: &str,
    ) -> Result<(String, PathBuf), SkillDiscoveryError> {
        let entry = self
            .skills
            .iter()
            .find(|e| e.metadata.name == name)
            .ok_or_else(|| SkillDiscoveryError::NotFound(name.to_string()))?;
        let base_path = entry.base_path.clone();

        let content = match &entry.embedded_content {
            Some(c) => c.clone(),
            None => std::fs::read_to_string(&entry.skill_file).map_err(|source| {
                SkillDiscoveryError::ReadFailed {
                    path: entry.skill_file.clone(),
                    source,
                }
            })?,
        };
        let (_, body) = parse_skill_frontmatter(&content);
        let mut out = body;

        // Builtin path: surface the references that were embedded via
        // `include_str!` so the agent can `read` them on demand.
        if let Some(refs) = &entry.embedded_files {
            if !refs.is_empty() {
                out.push_str("\n\n## Additional resources\nThis skill includes these reference files (use `read` tool if needed):\n");
                for (name, _content) in refs {
                    out.push_str("- ");
                    out.push_str(name);
                    out.push('\n');
                }
            }
        }

        // Filesystem path: scan the skill's directory for sibling files
        // (mirrors the behavior of disk-discovered skills).
        if entry.embedded_content.is_none()
            && entry
                .skill_file
                .file_name()
                .map(|f| f == SKILL_MD)
                .unwrap_or(false)
        {
            if let Ok(rd) = std::fs::read_dir(&entry.base_path) {
                let others: Vec<String> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_file())
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .filter(|n| n != SKILL_MD)
                    .collect();
                if !others.is_empty() {
                    // Only append the header if we didn't already write one
                    // for the builtin path above.
                    if !out.contains("## Additional resources") {
                        out.push_str("\n\n## Additional resources\nThis skill includes these reference files (use `read` tool if needed):\n");
                    }
                    for o in others {
                        out.push_str("- ");
                        out.push_str(&o);
                        out.push('\n');
                    }
                }
            }
        }
        Ok((out, base_path))
    }

    pub fn apply_filters(
        &mut self,
        enabled: Option<&[String]>,
        disabled: Option<&[String]>,
        current_platform: Option<&str>,
        platform_disabled: Option<&[String]>,
    ) {
        if let Some(en) = enabled {
            if !en.is_empty() {
                let set: HashSet<_> = en.iter().cloned().collect();
                self.skills.retain(|e| set.contains(&e.metadata.name));
            }
        }
        let mut disabled_set: HashSet<String> = disabled
            .and_then(|d| {
                if d.is_empty() {
                    None
                } else {
                    Some(d.iter().cloned().collect::<HashSet<_>>())
                }
            })
            .unwrap_or_default();
        if let Some(pd) = platform_disabled {
            disabled_set.extend(pd.iter().cloned());
        }
        if !disabled_set.is_empty() {
            self.skills
                .retain(|e| !disabled_set.contains(&e.metadata.name));
        }
        if let Some(platform) = current_platform {
            self.skills
                .retain(|e| e.metadata.matches_platform(platform));
        }
    }

    pub fn apply_toolset_filters(
        &mut self,
        available_tools: Option<&HashSet<String>>,
        available_toolsets: Option<&HashSet<String>>,
    ) {
        if available_tools.is_none() && available_toolsets.is_none() {
            return;
        }
        let tools = available_tools.cloned().unwrap_or_default();
        let toolsets = available_toolsets.cloned().unwrap_or_default();

        self.skills.retain(|entry| {
            let c = entry.metadata.conditions();
            for ts in &c.fallback_for_toolsets {
                if toolsets.contains(ts) {
                    return false;
                }
            }
            for t in &c.fallback_for_tools {
                if tools.contains(t) {
                    return false;
                }
            }
            for ts in &c.requires_toolsets {
                if !toolsets.contains(ts) {
                    return false;
                }
            }
            for t in &c.requires_tools {
                if !tools.contains(t) {
                    return false;
                }
            }
            true
        });
    }

    pub fn available_skills_prompt(&self) -> String {
        if self.skills.is_empty() {
            return String::new();
        }

        let mut by_category: BTreeMap<String, Vec<&SkillEntry>> = BTreeMap::new();
        for entry in &self.skills {
            let cat = entry.metadata.category.as_deref().unwrap_or("general");
            by_category.entry(cat.to_string()).or_default().push(entry);
        }

        let mut lines = vec![
            "<available_skills>".to_string(),
            "When the user's task matches a known skill, use the `skill` tool to load its full instructions before proceeding.".to_string(),
            String::new(),
        ];

        for (cat, skills) in &by_category {
            let cat_desc = skills
                .iter()
                .filter_map(|s| s.metadata.category_desc.as_deref())
                .next();
            if let Some(desc) = cat_desc {
                lines.push(format!("  {}: {}", cat, desc));
            } else {
                lines.push(format!("  {}:", cat));
            }
            for s in skills {
                let desc = if s.metadata.description.is_empty() {
                    String::new()
                } else {
                    format!(" {}", s.metadata.description.trim())
                };
                lines.push(format!("    - {}:{}", s.metadata.name, desc));
            }
        }

        lines.push("</available_skills>".to_string());
        lines.join("\n")
    }

    pub fn invalidate_cache(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            cache.invalidate_all();
        }
    }
}

fn read_category_description(dir: &Path) -> Option<String> {
    let desc_file = dir.join(DESCRIPTION_MD);
    if !desc_file.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&desc_file).ok()?;
    let (mapping, _) = crate::utils::parse_frontmatter(&content);
    mapping
        .get(serde_yaml::Value::String("description".into()))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn derive_category(skill_path: &Path, scan_root: &Path) -> Option<String> {
    let rel = skill_path.strip_prefix(scan_root).ok()?;
    let parent = rel.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.to_string_lossy().to_string())
}

pub fn scan_skills_dir(dir: &Path, source: SkillSource) -> Vec<SkillEntry> {
    let mut entries = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return entries;
    };

    for e in read_dir.flatten() {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let skill_file = path.join(SKILL_MD);
        if !skill_file.is_file() {
            continue;
        }
        let content = match std::fs::read_to_string(&skill_file) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let (meta_opt, _) = parse_skill_frontmatter(&content);
        let mut metadata = meta_opt.unwrap_or_else(|| SkillMetadata {
            name: path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default(),
            ..Default::default()
        });

        if metadata.name.is_empty() {
            metadata.name = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
        }

        if metadata.category.is_none() {
            metadata.category = derive_category(&skill_file, dir);
        }
        if metadata.category_desc.is_none() {
            metadata.category_desc = metadata
                .category
                .as_ref()
                .and_then(|cat| read_category_description(&dir.join(cat)));
        }

        entries.push(SkillEntry {
            metadata,
            base_path: path,
            skill_file,
            source,
            embedded_content: None,
            embedded_files: None,
        });
    }

    let Ok(read_dir2) = std::fs::read_dir(dir) else {
        return entries;
    };
    for e in read_dir2.flatten() {
        let path = e.path();
        if !path.is_file() {
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !SKILL_EXTENSIONS.contains(&ext) {
            continue;
        }
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        if name.eq_ignore_ascii_case("SKILL") || name.eq_ignore_ascii_case("DESCRIPTION") {
            continue;
        }
        if entries.iter().any(|e| e.metadata.name == name) {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let (meta_opt, _) = parse_skill_frontmatter(&content);
        let metadata = meta_opt.unwrap_or_else(|| SkillMetadata {
            name: name.clone(),
            ..Default::default()
        });

        entries.push(SkillEntry {
            metadata,
            base_path: path.parent().unwrap_or(dir).to_path_buf(),
            skill_file: path,
            source,
            embedded_content: None,
            embedded_files: None,
        });
    }

    entries
}

pub fn scan_skills_dir_recursive(dir: &Path, source: SkillSource) -> Vec<SkillEntry> {
    let mut entries = scan_skills_dir(dir, source);
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return entries;
    };

    for e in read_dir.flatten() {
        let path = e.path();
        if !path.is_dir() || crate::utils::is_excluded_path(&path) {
            continue;
        }
        // Skip directories that are themselves skill directories
        if path.join(SKILL_MD).exists() {
            continue;
        }
        entries.extend(scan_skills_dir_recursive(&path, source));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! registry_from_skills {
        ($entries:expr) => {
            SkillRegistry {
                skills: $entries,
                cache: std::sync::Mutex::new(crate::cache::SkillCache::new()),
            }
        };
    }

    fn make_test_entry(name: &str, source: SkillSource) -> SkillEntry {
        SkillEntry {
            metadata: SkillMetadata {
                name: name.to_string(),
                description: format!("Test skill {}", name),
                ..Default::default()
            },
            base_path: PathBuf::from(format!("/test/{}", name)),
            skill_file: PathBuf::from(format!("/test/{}/SKILL.md", name)),
            source,
            embedded_content: None,
            embedded_files: None,
        }
    }

    #[test]
    fn empty_registry_returns_empty_prompt() {
        let registry = SkillRegistry::empty();
        assert!(registry.available_skills_prompt().is_empty());
    }

    #[test]
    fn available_skills_prompt_with_categories() {
        let mut entries = vec![
            make_test_entry("code-review", SkillSource::Project),
            make_test_entry("test-driven", SkillSource::User),
        ];
        entries[0].metadata.category = Some("coding".to_string());
        entries[1].metadata.category = Some("testing".to_string());
        let registry = registry_from_skills!(entries);
        let prompt = registry.available_skills_prompt();
        assert!(prompt.contains("<available_skills>"));
        assert!(prompt.contains("</available_skills>"));
        assert!(prompt.contains("  coding:"));
        assert!(prompt.contains("    - code-review:"));
        assert!(prompt.contains("  testing:"));
        assert!(prompt.contains("    - test-driven:"));
    }

    #[test]
    fn available_skills_prompt_uncategorized_as_general() {
        let entries = vec![make_test_entry("standalone", SkillSource::Project)];
        let registry = registry_from_skills!(entries);
        let prompt = registry.available_skills_prompt();
        assert!(prompt.contains("  general:"));
        assert!(prompt.contains("    - standalone:"));
    }

    #[test]
    fn apply_toolset_filters_fallback_hidden() {
        let mut entries = vec![make_test_entry("duckduckgo", SkillSource::Project)];
        entries[0].metadata.metadata = Some(crate::utils::SkillMetadataBlock {
            conditions: crate::utils::SkillConditions {
                fallback_for_toolsets: vec!["web".to_string()],
                ..Default::default()
            },
            ..Default::default()
        });
        let mut registry = registry_from_skills!(entries);

        let mut toolsets = HashSet::new();
        toolsets.insert("web".to_string());
        registry.apply_toolset_filters(None, Some(&toolsets));
        assert!(registry.list().is_empty());

        let mut registry2 =
            registry_from_skills!(vec![make_test_entry("duckduckgo", SkillSource::Project)]);
        registry2.skills[0].metadata.metadata = Some(crate::utils::SkillMetadataBlock {
            conditions: crate::utils::SkillConditions {
                fallback_for_toolsets: vec!["web".to_string()],
                ..Default::default()
            },
            ..Default::default()
        });
        registry2.apply_toolset_filters(None, Some(&HashSet::new()));
        assert_eq!(registry2.list().len(), 1);
    }

    #[test]
    fn apply_toolset_filters_requires_missing() {
        let mut entries = vec![make_test_entry("terminal-only", SkillSource::Project)];
        entries[0].metadata.metadata = Some(crate::utils::SkillMetadataBlock {
            conditions: crate::utils::SkillConditions {
                requires_toolsets: vec!["terminal".to_string()],
                ..Default::default()
            },
            ..Default::default()
        });
        let mut registry = registry_from_skills!(entries);
        registry.apply_toolset_filters(None, Some(&HashSet::new()));
        assert!(registry.list().is_empty());
    }

    #[test]
    fn apply_toolset_filters_none_skips() {
        let entries = vec![make_test_entry("any", SkillSource::Project)];
        let mut registry = registry_from_skills!(entries);
        registry.apply_toolset_filters(None, None);
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn apply_filters_enabled_whitelist() {
        let entries = vec![
            make_test_entry("a", SkillSource::Project),
            make_test_entry("b", SkillSource::Project),
        ];
        let mut registry = registry_from_skills!(entries);
        registry.apply_filters(Some(&["a".to_string()]), None, None, None);
        let names: Vec<_> = registry
            .list()
            .iter()
            .map(|e| e.metadata.name.as_str())
            .collect();
        assert_eq!(names, vec!["a"]);
    }

    #[test]
    fn add_builtin_inserts_skill() {
        let mut registry = SkillRegistry::empty();
        registry.add_builtin(
            "luft-workflow-dsl",
            "Lua DSL reference",
            "---\nname: luft-workflow-dsl\n---\n\n# Body",
            vec!["luft".to_string()],
            vec!["luft".to_string()],
            vec![],
        );
        assert_eq!(registry.list().len(), 1);
        let entry = &registry.list()[0];
        assert_eq!(entry.metadata.name, "luft-workflow-dsl");
        assert_eq!(entry.source, SkillSource::Builtin);
        assert!(entry.embedded_content.is_some());
    }

    #[test]
    fn add_builtin_skips_when_name_exists() {
        let mut registry = SkillRegistry::empty();
        // Inject a disk skill with the same name first.
        registry
            .skills
            .push(make_test_entry("luft-workflow-dsl", SkillSource::Project));

        // No-op: builtin skipped because name already taken.
        registry.add_builtin(
            "luft-workflow-dsl",
            "v1",
            "v1 content",
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.list()[0].source, SkillSource::Project);
    }

    #[test]
    fn load_skill_with_dir_uses_embedded_content() {
        let mut registry = SkillRegistry::empty();
        registry.add_builtin(
            "luft-workflow-dsl",
            "DSL",
            "---\nname: luft-workflow-dsl\n---\n\n# Embedded Body",
            vec![],
            vec!["luft".to_string()],
            vec![],
        );
        let (content, _) = registry.load_skill_with_dir("luft-workflow-dsl").unwrap();
        assert!(content.contains("Embedded Body"));
    }

    #[test]
    fn load_skill_with_dir_lists_embedded_references() {
        let mut registry = SkillRegistry::empty();
        registry.add_builtin(
            "luft-workflow-dsl",
            "DSL",
            "---\nname: luft-workflow-dsl\n---\n\n# Embedded Body",
            vec![],
            vec!["luft".to_string()],
            vec![
                ("references/foo.md".to_string(), "# Foo\nbody".to_string()),
                ("references/bar.md".to_string(), "# Bar\nbody".to_string()),
            ],
        );
        let (content, _) = registry.load_skill_with_dir("luft-workflow-dsl").unwrap();
        assert!(content.contains("## Additional resources"));
        assert!(content.contains("- references/foo.md"));
        assert!(content.contains("- references/bar.md"));
    }

    #[test]
    fn builtin_skill_requires_tools_round_trip() {
        let mut registry = SkillRegistry::empty();
        registry.add_builtin(
            "luft-workflow-dsl",
            "DSL",
            "body",
            vec![],
            vec!["luft".to_string()],
            vec![],
        );
        let entry = &registry.list()[0];
        let block = entry.metadata.metadata.as_ref().unwrap();
        assert_eq!(block.conditions.requires_tools, vec!["luft".to_string()]);
    }

    #[test]
    fn apply_filters_disabled_blacklist() {
        let entries = vec![
            make_test_entry("a", SkillSource::Project),
            make_test_entry("b", SkillSource::Project),
        ];
        let mut registry = registry_from_skills!(entries);
        registry.apply_filters(None, Some(&["a".to_string()]), None, None);
        let names: Vec<&str> = registry
            .list()
            .iter()
            .map(|e| e.metadata.name.as_str())
            .collect();
        assert!(!names.contains(&"a"));
        assert!(names.contains(&"b"));
    }

    #[test]
    fn apply_filters_platform_excludes() {
        let mut entries = vec![make_test_entry("mac-only", SkillSource::Project)];
        entries[0].metadata.platforms = vec!["macos".to_string()];
        let mut registry = registry_from_skills!(entries);

        registry.apply_filters(None, None, Some("windows"), None);
        assert!(registry.list().is_empty());

        let mut entries2 = vec![make_test_entry("mac-only", SkillSource::Project)];
        entries2[0].metadata.platforms = vec!["macos".to_string()];
        let mut registry2 = registry_from_skills!(entries2);
        registry2.apply_filters(None, None, Some("darwin"), None);
        assert_eq!(registry2.list().len(), 1);
    }

    #[test]
    fn apply_filters_platform_disabled() {
        let entries = vec![
            make_test_entry("a", SkillSource::Project),
            make_test_entry("b", SkillSource::Project),
        ];
        let mut registry = registry_from_skills!(entries);
        registry.apply_filters(None, None, None, Some(&["a".to_string()]));
        let names: Vec<&str> = registry
            .list()
            .iter()
            .map(|e| e.metadata.name.as_str())
            .collect();
        assert!(!names.contains(&"a"));
        assert!(names.contains(&"b"));
    }
}

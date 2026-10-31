//! Curator LLM Pass — agent-based consolidation using the ReAct pattern.
//!
//! Previously a manual multi-turn loop (NOT a full ReAct graph), now refactored
//! to use the same Agent/ReAct architecture as the background review (see
//! `review.rs`). The curator spawns a forked agent with a restricted tool set
//! (`skill_list`, `skill_view`, `skill_manage`) and a curator-specific prompt,
//! then collects tool calls for classification.
//!
//! ## Architecture
//!
//! ```text
//! build prompt ──► Agent::from_config(config)
//!                        │
//!                        ▼
//!                 agent.run_with_config(prompt, fork_config, on_event)
//!                        │
//!                 ┌──────┴──────┐
//!                 ▼             ▼
//!           ReAct loop      event callback
//!           (think→act      collects ToolCallStart
//!            →observe)       for classification
//!                 │
//!           final reply (YAML)
//! ```
//!
//! ## Classification
//!
//! After the agent run completes, collected tool calls are passed to
//! `reconcile_classification()` to determine which skills were consolidated
//! (absorbed into an umbrella) vs pruned (archived with no absorption).

use crate::curator::{
    self, parse_llm_review_response, reconcile_classification, ClassificationResult,
    CuratorToolCall, LlMReviewResult, SkillSnapshot,
};
use crate::prompts::CURATOR_SYSTEM_PROMPT;
use crate::review::uuid_v4;
use crate::review_tool_gate::ReviewToolGate;
use crate::skill_registry::{Lifecycle, SkillContent, SkillRegistry};

use agent::agent::{Agent, AgentEvent};
use agent::ReactBuildConfig;
use checkpoint::RunnableConfig;
use skill::SkillUsageStore;
use tool_basic::skill::{make_skill_tools_with_skills_dir, SkillManagerTool};
use tool_core::Tool;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

/// Result of a single curator LLM pass.
///
/// Aligns with Hermes `result_meta` (`agent/curator.py:1676-1810`):
/// `final_reply` ← `final`, `summary` ← `summary`, `model` ← `model`,
/// `provider` ← `provider`, `run_error` ← `error`.
#[derive(Debug, Clone)]
pub struct CuratorLlmPassOutcome {
    /// Full (untruncated) final response from the reviewer (Hermes `final`).
    pub final_reply: String,
    /// Short summary suitable for state file, ≤240 chars (Hermes `summary`).
    pub summary: String,
    /// Model the forked agent actually ran on (Hermes `model`).
    pub model: String,
    /// Provider the forked agent ran on (Hermes `provider`).
    pub provider: String,
    /// Set if the pass failed mid-run; final_reply/summary may still be
    /// empty (Hermes `error`). Never panics — callers get structured failure.
    pub run_error: Option<String>,
    /// All tool calls collected across all turns (for classification).
    pub all_tool_calls: Vec<CuratorToolCall>,
    /// Parsed YAML result from the final LLM response.
    pub llm_result: LlMReviewResult,
    /// Final classification: consolidated vs pruned.
    pub classification: ClassificationResult,
    /// Skill names before the pass.
    pub before_names: Vec<String>,
    /// Skill names after the pass.
    pub after_names: Vec<String>,
    /// Snapshots of skills before the pass.
    pub before_snapshots: Vec<SkillSnapshot>,
    /// Snapshots of skills after the pass.
    pub after_snapshots: Vec<SkillSnapshot>,
    /// Elapsed time in seconds.
    pub elapsed_seconds: f64,
    /// Number of tool calls executed (approximation of LLM turns).
    pub turns: u32,
}

impl CuratorLlmPassOutcome {
    /// Returns the names of skills that were removed (existed before, not after).
    pub fn removed_skills(&self) -> Vec<String> {
        let after: HashSet<&str> = self.after_names.iter().map(|s| s.as_str()).collect();
        self.before_names
            .iter()
            .filter(|name| !after.contains(name.as_str()))
            .cloned()
            .collect()
    }

    /// Returns the names of skills that were added (exist after, not before).
    pub fn added_skills(&self) -> Vec<String> {
        let before: HashSet<&str> = self.before_names.iter().map(|s| s.as_str()).collect();
        self.after_names
            .iter()
            .filter(|name| !before.contains(name.as_str()))
            .cloned()
            .collect()
    }
}

/// Snapshot all active agent-created skills as `SkillContent` for prompt building.
///
/// Only includes skills where `usage_store.is_agent_created(name)` is true,
/// aligning with Hermes `list_agent_created_skill_names()`.
fn snapshot_active_skills(
    registry: &SkillRegistry,
    usage_store: &SkillUsageStore,
) -> Result<Vec<SkillContent>, String> {
    let metas = registry.list().map_err(|e| format!("{:?}", e))?;
    let mut skills = Vec::new();
    for meta in &metas {
        let Some(usage) = usage_store.get(&meta.name) else {
            continue;
        };
        if usage.created_by.as_deref() != Some("agent")
            || usage.state != Lifecycle::Active
            || usage.pinned
            || !usage_store.is_agent_created(&meta.name)
        {
            continue;
        }
        if let Ok(content) = registry.load(&meta.name) {
            skills.push(content);
        }
    }
    Ok(skills)
}

/// Snapshot skill names for before/after comparison.
fn snapshot_skill_names(registry: &SkillRegistry) -> Result<Vec<String>, String> {
    let metas = registry.list().map_err(|e| format!("{:?}", e))?;
    Ok(metas.into_iter().map(|m| m.name).collect())
}

/// Snapshot skill metadata for report generation.
fn snapshot_skill_metas(registry: &SkillRegistry) -> Result<Vec<SkillSnapshot>, String> {
    let metas = registry.list().map_err(|e| format!("{:?}", e))?;
    let mut snapshots = Vec::new();
    for meta in &metas {
        let body_len = registry.load(&meta.name).map(|c| c.body.len()).unwrap_or(0);
        snapshots.push(SkillSnapshot {
            name: meta.name.clone(),
            description: meta.description.clone(),
            lifecycle: format!("{:?}", meta.lifecycle),
            triggers: meta.triggers.clone(),
            body_len,
        });
    }
    Ok(snapshots)
}

/// Build the tool executors for the curator agent.
///
/// The LLM gets the same tool set as the background review agent (minus
/// `memory`). Tools are injected via `ReactBuildConfig.extra_tools` so the
/// agent's `build_react_runner` picks them up alongside (or instead of) the
/// builtin tool set.
///
/// Tools:
///   - `skill_list` — list all skills with names/descriptions
///   - `skill_view` — load a skill's full content by name
///   - `skill_manage` — create/patch/edit/delete/write_file/remove_file
fn build_curator_tools(skills_base_dir: &std::path::Path) -> Vec<Arc<dyn Tool>> {
    let registry = Arc::new(SkillRegistry::new(skills_base_dir));
    let usage_store = Arc::new(SkillUsageStore::new(skills_base_dir));

    // Use make_skill_tools_with_skills_dir (not _with_folder) — the base_dir
    // is already the skills storage root (e.g. ~/.anureo/data/skills/), not a
    // working folder. Using _with_folder would join ".anureo/skills" again,
    // producing a non-existent double path like ~/.anureo/data/skills/.anureo/skills.
    let view_usage = SkillUsageStore::new(skills_base_dir);
    let (list_tool, view_tool) =
        make_skill_tools_with_skills_dir(skills_base_dir, Some(view_usage));

    let manage = Arc::new(SkillManagerTool::for_background_review(
        registry,
        Some(usage_store),
    ));

    vec![Arc::new(list_tool), Arc::new(view_tool), manage]
}

/// Run the curator LLM pass using a full ReAct agent (same architecture as
/// the background review).
///
/// This is the main entry point for the LLM-driven consolidation pass. It
/// builds the prompt from active skills, forks a ReAct agent with a
/// restricted tool set, collects all tool calls via event callback, and
/// returns the classification.
///
/// # Arguments
/// * `base_config` - Base agent config (has LLM credentials, working folder, etc.)
/// * `registry` - The skill registry to read/modify (for before/after snapshots)
/// * `usage_store` - Usage metadata store (for filtering agent-created skills)
/// * `usage_reports` - Usage metadata for prompt building
///
/// # Returns
/// `CuratorLlmPassOutcome` containing all collected data, or an error string.
pub async fn run_curator_llm_pass(
    base_config: ReactBuildConfig,
    registry: &SkillRegistry,
    usage_store: &SkillUsageStore,
    usage_reports: &[skill::SkillUsageReport],
    dry_run: bool,
) -> Result<CuratorLlmPassOutcome, String> {
    let start = std::time::Instant::now();

    // Capture model/provider before base_config is moved into config (Hermes
    // curator.py:1740-1741 — resolve_runtime_provider feeds result_meta).
    let model = base_config.model.clone().unwrap_or_default();
    let provider = base_config
        .llm_provider
        .clone()
        .or_else(|| base_config.llm_provider_name.clone())
        .unwrap_or_default();

    // Snapshot before-state (pre-condition errors still propagate as Err)
    let before_names = snapshot_skill_names(registry)?;
    let before_snapshots = snapshot_skill_metas(registry)?;
    let active_skills = snapshot_active_skills(registry, usage_store)?;

    info!(
        "Curator LLM pass: starting with {} active skills (agent mode)",
        active_skills.len()
    );

    if active_skills.is_empty() {
        return Ok(CuratorLlmPassOutcome {
            final_reply: String::new(),
            summary: "skipped (no agent-created candidates)".to_string(),
            model,
            provider,
            run_error: None,
            all_tool_calls: vec![],
            llm_result: LlMReviewResult::default(),
            classification: ClassificationResult::default(),
            before_names: before_names.clone(),
            after_names: before_names,
            before_snapshots: before_snapshots.clone(),
            after_snapshots: before_snapshots,
            elapsed_seconds: start.elapsed().as_secs_f64(),
            turns: 0,
        });
    }

    // Build prompt (includes CURATOR_REVIEW_PROMPT + skill list).
    // `dry_run` (Hermes parity: `agent/curator.py:run_curator_review(..., dry_run=True)`)
    // prepends a "## Mode: DRY RUN" banner so the LLM produces an analysis
    // that *describes* changes without committing them. The downstream tools
    // still run; the caller is responsible for gating writes.
    let prompt = curator::build_llm_prompt(&active_skills, usage_reports, dry_run);

    // Build curator tools — injected via extra_tools so the agent uses the
    // storage-based SkillRegistry (auto/curated/evolved), not the discovery-based one.
    let curator_tools = build_curator_tools(registry.base_dir());

    // Configure agent for curator mode — mirrors review.rs isolation pattern.
    let gate = ReviewToolGate::with_allowed(vec!["skill_list", "skill_view", "skill_manage"]);
    let mut config = base_config;

    // Hermes parity: thread ANUREO_CURATOR_* env vars through to this pass.
    // `resolve_curator_overrides()` reads the env once and returns an
    // overrides bag; we apply it BEFORE the aux-model override below so the
    // two never fight (explicit env wins; aux is only used when nothing is
    // explicitly set).
    let overrides = config.resolve_curator_overrides();
    if let Some(m) = overrides.model {
        config.model = Some(m);
    } else if let Some(ref aux) = config.aux_model.clone() {
        // If an aux model is configured, use it for the curator agent instead of the
        // main session model. Aligns Hermes `aux_model` / `dev_model` configuration.
        config.model = Some(aux.clone());
    }
    if let Some(p) = overrides.provider {
        config.llm_provider_name = Some(p);
    }
    if let Some(k) = overrides.api_key {
        config.openai_api_key = Some(k);
    }
    if let Some(u) = overrides.base_url {
        config.openai_base_url = Some(u);
    }

    config.is_background_review = true;
    config.memory_enabled = false;
    config.user_profile_enabled = false;
    config.memory_nudge_interval = 0;
    config.skill_nudge_interval = 0;
    // Restrict visible tools to only the 3 skill tools (affects list_tools + call_tool)
    config.builtin_tool_filter = Some(gate.as_builtin_filter());
    // Execution-only safety net (same whitelist)
    config.call_tool_filter = Some(gate.as_builtin_filter());
    // Inject curator-specific tools built from SkillStorageRegistry
    config.extra_tools = Some(Arc::new(curator_tools));
    // Use curator system prompt instead of default ReAct prompt
    config.system_prompt = Some(CURATOR_SYSTEM_PROMPT.to_string());

    // Build agent — on failure return degraded outcome (aligns Hermes
    // "Never raises" curator.py:1698-1703).
    let agent = match Agent::from_config(config).await {
        Ok(a) => a,
        Err(e) => {
            let elapsed = start.elapsed().as_secs_f64();
            let err_msg = format!("Agent build error: {}", e);
            warn!("Curator LLM pass failed: {}", err_msg);
            return Ok(CuratorLlmPassOutcome {
                final_reply: String::new(),
                summary: err_msg.clone(),
                model,
                provider,
                run_error: Some(err_msg),
                all_tool_calls: vec![],
                llm_result: LlMReviewResult::default(),
                classification: ClassificationResult::default(),
                before_names: before_names.clone(),
                after_names: before_names,
                before_snapshots: before_snapshots.clone(),
                after_snapshots: before_snapshots,
                elapsed_seconds: elapsed,
                turns: 0,
            });
        }
    };

    // Fork config with isolated thread/checkpoint (same pattern as review.rs:369-376)
    let fork_thread_id = format!("curator-{}", uuid_v4());
    let fork_thread_id_log = fork_thread_id.clone();
    let fork_config = RunnableConfig {
        thread_id: Some(fork_thread_id),
        checkpoint_ns: "curator".to_string(),
        ..Default::default()
    };

    info!(
        "Curator agent running (thread_id: {})...",
        fork_thread_id_log
    );

    // Collect tool calls via event callback
    let all_tool_calls: Arc<Mutex<Vec<CuratorToolCall>>> = Arc::new(Mutex::new(Vec::new()));
    let turn_counter: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
    let tc_clone = all_tool_calls.clone();
    let turn_clone = turn_counter.clone();

    // Run agent — on failure preserve collected tool_calls and return
    // degraded outcome (aligns Hermes curator.py:1801-1803).
    let result = match agent
        .run_with_config(&prompt, Some(fork_config), move |ev| {
            match ev {
                AgentEvent::ToolCallStart { name, arguments, .. } => {
                    debug!(tool = %name, "Curator tool call");
                    // Truncate arguments to 400 chars for report readability
                    // (aligns Hermes curator.py:1797-1798).
                    let truncated = truncate_args(&arguments, 400);
                    tc_clone.lock().unwrap().push(CuratorToolCall {
                        id: None,
                        name: name.clone(),
                        arguments: truncated,
                    });
                    *turn_clone.lock().unwrap() += 1;
                }
                AgentEvent::ToolEnd {
                    name,
                    result,
                    is_error,
                    ..
                } => {
                    if is_error {
                        warn!(
                            tool = %name,
                            preview = %anureo_util::text::truncate::truncate(&result, 200),
                            "Curator tool '{}' failed",
                            name
                        );
                    } else {
                        debug!(tool = %name, "Curator tool '{}' ok", name);
                    }
                }
                _ => {}
            }
        })
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let elapsed = start.elapsed().as_secs_f64();
            let collected = all_tool_calls.lock().unwrap().clone();
            let turns = *turn_counter.lock().unwrap();
            let err_msg = format!("Agent run error: {}", e);
            warn!("Curator LLM pass failed: {}", err_msg);
            let after_names =
                snapshot_skill_names(registry).unwrap_or_else(|_| before_names.clone());
            let after_snapshots =
                snapshot_skill_metas(registry).unwrap_or_else(|_| before_snapshots.clone());
            return Ok(CuratorLlmPassOutcome {
                final_reply: String::new(),
                summary: err_msg.clone(),
                model,
                provider,
                run_error: Some(err_msg),
                all_tool_calls: collected,
                llm_result: LlMReviewResult::default(),
                classification: ClassificationResult::default(),
                before_names,
                after_names,
                before_snapshots,
                after_snapshots,
                elapsed_seconds: elapsed,
                turns,
            });
        }
    };

    let all_tool_calls = all_tool_calls.lock().unwrap().clone();
    let turns = *turn_counter.lock().unwrap();

    // Parse final YAML response from agent reply
    let llm_result = parse_llm_review_response(&result.reply);

    // Build summary (aligns Hermes curator.py:1780 — ≤240 chars, Unicode-safe)
    let summary = summarize_review(&llm_result, &result.reply);

    // Snapshot after-state
    let after_names = snapshot_skill_names(registry)?;
    let after_snapshots = snapshot_skill_metas(registry)?;

    let removed = {
        let after_set: HashSet<&str> = after_names.iter().map(|s| s.as_str()).collect();
        before_names
            .iter()
            .filter(|n| !after_set.contains(n.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    };
    let added = {
        let before_set: HashSet<&str> = before_names.iter().map(|s| s.as_str()).collect();
        after_names
            .iter()
            .filter(|n| !before_set.contains(n.as_str()))
            .cloned()
            .collect::<Vec<_>>()
    };

    let after_set: HashSet<String> = after_names.iter().cloned().collect();

    let classification =
        reconcile_classification(&removed, &added, &after_set, &all_tool_calls, &llm_result);

    let elapsed = start.elapsed().as_secs_f64();
    info!(
        "Curator LLM pass: completed in {:.1}s, {} tool calls, {} consolidated, {} pruned",
        elapsed,
        all_tool_calls.len(),
        classification.consolidated.len(),
        classification.pruned.len(),
    );

    Ok(CuratorLlmPassOutcome {
        final_reply: result.reply,
        summary,
        model,
        provider,
        run_error: None,
        all_tool_calls,
        llm_result,
        classification,
        before_names,
        after_names,
        before_snapshots,
        after_snapshots,
        elapsed_seconds: elapsed,
        turns,
    })
}

/// Truncate a string to `max_chars` Unicode characters, appending `"."` if
/// truncation occurred. Unicode-safe (aligns Hermes curator.py:1797-1798 for
/// tool_call arguments and curator.py:1780 for summary).
fn truncate_args(s: &str, max_chars: usize) -> String {
    let char_count = s.chars().count();
    if char_count > max_chars {
        let truncated: String = s.chars().take(max_chars).collect();
        format!("{}.", truncated)
    } else {
        s.to_string()
    }
}

/// Build a ≤240-char summary from the LLM final reply.
///
/// Aligns with Hermes curator.py:1780:
///   `(final[:240] + ".") if len(final) > 240 else (final or "no change")`
///
/// Uses `chars()` iteration for Unicode safety.
fn truncate_summary(final_reply: &str) -> String {
    if final_reply.is_empty() {
        "no change".to_string()
    } else {
        truncate_args(final_reply, 240)
    }
}

fn summarize_review(review: &LlMReviewResult, final_reply: &str) -> String {
    if review.summary.trim().is_empty() {
        return truncate_summary(final_reply);
    }

    let summary = review
        .summary
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    truncate_summary(&summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outcome_removed_and_added() {
        let outcome = CuratorLlmPassOutcome {
            final_reply: String::new(),
            summary: "no change".into(),
            model: "test-model".into(),
            provider: "test-provider".into(),
            run_error: None,
            all_tool_calls: vec![],
            llm_result: LlMReviewResult::default(),
            classification: ClassificationResult {
                consolidated: vec![],
                pruned: vec![],
            },
            before_names: vec!["a".into(), "b".into(), "c".into()],
            after_names: vec!["a".into(), "d".into()],
            before_snapshots: vec![],
            after_snapshots: vec![],
            elapsed_seconds: 0.0,
            turns: 0,
        };

        assert_eq!(
            outcome.removed_skills(),
            vec!["b".to_string(), "c".to_string()]
        );
        assert_eq!(outcome.added_skills(), vec!["d".to_string()]);
    }

    #[tokio::test]
    async fn test_build_curator_tools_produces_all_tools() {
        let dir = tempfile::tempdir().unwrap();
        let tools = build_curator_tools(dir.path());
        assert_eq!(tools.len(), 3);
        let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
        assert!(names.contains(&"skill_list"));
        assert!(names.contains(&"skill_view"));
        assert!(names.contains(&"skill_manage"));
    }

    #[test]
    fn test_truncate_summary_short() {
        assert_eq!(truncate_summary("hello"), "hello");
    }

    #[test]
    fn test_truncate_summary_empty() {
        assert_eq!(truncate_summary(""), "no change");
    }

    #[test]
    fn test_truncate_summary_long() {
        let long = "a".repeat(300);
        let summary = truncate_summary(&long);
        assert_eq!(summary.chars().count(), 241); // 240 + "."
        assert!(summary.ends_with('.'));
    }

    #[test]
    fn test_truncate_summary_unicode_safe() {
        let cjk: String = "你".repeat(121); // 121 chars = 121 * 3 bytes
        let summary = truncate_summary(&cjk);
        assert_eq!(summary.chars().count(), 121); // not truncated (< 240)
    }

    #[test]
    fn test_summarize_review_prefers_structured_summary() {
        let review = LlMReviewResult {
            summary: "One candidate exists.\nNo consolidation needed.".to_string(),
            ..LlMReviewResult::default()
        };
        let raw = "```yaml\nsummary: One candidate exists.\n```";
        assert_eq!(
            summarize_review(&review, raw),
            "One candidate exists. No consolidation needed."
        );
    }

    #[test]
    fn test_truncate_args_short() {
        assert_eq!(truncate_args("short", 400), "short");
    }

    #[test]
    fn test_truncate_args_long() {
        let long = "x".repeat(500);
        let truncated = truncate_args(&long, 400);
        assert_eq!(truncated.chars().count(), 401); // 400 + "."
        assert!(truncated.ends_with('.'));
    }

    fn make_test_skill_content(name: &str) -> SkillContent {
        SkillContent {
            name: name.to_string(),
            description: format!("Test skill {}", name),
            triggers: vec!["test".into()],
            lifecycle: Lifecycle::Active,
            source: crate::Source::Auto,
            category: None,
            created_by: None,
            body: "Do stuff".to_string(),
            raw: String::new(),
        }
    }

    #[test]
    fn snapshot_skill_names_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        let names = snapshot_skill_names(&registry).unwrap();
        assert!(names.is_empty());
    }

    #[test]
    fn snapshot_skill_names_returns_all() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        registry.save("a", &make_test_skill_content("a")).unwrap();
        registry.save("b", &make_test_skill_content("b")).unwrap();
        let names = snapshot_skill_names(&registry).unwrap();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn snapshot_skill_metas_includes_body_len() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        registry.save("x", &make_test_skill_content("x")).unwrap();
        let metas = snapshot_skill_metas(&registry).unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].name, "x");
        assert!(metas[0].body_len > 0);
    }

    #[test]
    fn snapshot_skill_metas_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        let metas = snapshot_skill_metas(&registry).unwrap();
        assert!(metas.is_empty());
    }

    #[test]
    fn snapshot_active_skills_empty_registry() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        let usage = SkillUsageStore::new(dir.path());
        let skills = snapshot_active_skills(&registry, &usage).unwrap();
        assert!(skills.is_empty());
    }

    #[test]
    fn snapshot_active_skills_filters_by_agent_created() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        let usage = SkillUsageStore::new(dir.path());

        registry
            .save("agent-skill", &make_test_skill_content("agent-skill"))
            .unwrap();
        usage.mark_agent_created("agent-skill");

        registry
            .save("user-skill", &make_test_skill_content("user-skill"))
            .unwrap();

        let skills = snapshot_active_skills(&registry, &usage).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "agent-skill");
    }

    #[test]
    fn snapshot_active_skills_excludes_pinned() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        let usage = SkillUsageStore::new(dir.path());

        registry
            .save("pinned", &make_test_skill_content("pinned"))
            .unwrap();
        usage.mark_agent_created("pinned");
        registry.set_pinned("pinned", true).unwrap();

        let skills = snapshot_active_skills(&registry, &usage).unwrap();
        assert!(skills.is_empty(), "pinned skills should be excluded");
    }

    #[test]
    fn snapshot_active_skills_excludes_non_active() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        let usage = SkillUsageStore::new(dir.path());

        let mut skill = make_test_skill_content("stale");
        skill.lifecycle = Lifecycle::Stale;
        registry.save("stale", &skill).unwrap();
        usage.mark_agent_created("stale");

        let skills = snapshot_active_skills(&registry, &usage).unwrap();
        assert!(skills.is_empty(), "non-active skills should be excluded");
    }

    #[tokio::test]
    async fn run_curator_llm_pass_skips_agent_build_without_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let registry = SkillRegistry::new(dir.path());
        let usage = SkillUsageStore::new(dir.path());

        let outcome =
            run_curator_llm_pass(ReactBuildConfig::default(), &registry, &usage, &[], false)
                .await
                .unwrap();

        assert_eq!(outcome.summary, "skipped (no agent-created candidates)");
        assert!(outcome.run_error.is_none());
        assert_eq!(outcome.turns, 0);
    }
}

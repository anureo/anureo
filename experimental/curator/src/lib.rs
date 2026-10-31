//! Background review system for anureo.
//!
//! This crate provides the background review functionality that analyzes
//! completed conversations and updates memory/skills. It includes:
//!
//! - **Review**: ReAct-based agent fork that processes conversations using LLM tools
//! - **Tool Gate**: Whitelist filter for review-time tool access
//! - **Curator**: Skill lifecycle management and consolidation
//! - **Prompts**: Detailed review prompts aligned with Hermes
//! - **History / Observability**: Review record persistence and metrics
//! - **Workflow**: Global task registry and Curator auto-run

pub mod backfill_triggers;
pub mod curator;
pub mod curator_backup;
pub mod curator_llm;
pub mod history;
pub mod observability;
pub mod prompts;
pub mod review;
pub mod review_tool_gate;
pub mod security;
pub mod skill_registry;
pub mod workflow;

// Re-export key types for convenience
pub use review::{
    bounded_review_unified_diff, run_review, run_review_with_cancellation, spawn_background_review, spawn_review_after_session,
    ReviewActionSummary, ReviewConfig, ReviewOutcome, TokenUsageSummary, REVIEW_INSTRUCTION,
};
pub use review_tool_gate::{ReviewToolGate, REVIEW_ALLOWED_TOOLS};

pub use curator::{
    build_llm_prompt, parse_llm_review_response, reconcile_classification, AbsorbedIntoDeclaration,
    AutoCounts, ClassificationResult, ConsolidationDecision, Curator, CuratorConfig, CuratorReport,
    CuratorReviewResult, CuratorRunReport, CuratorState, CuratorStateStore, CuratorToolCall,
    FileStateStore, LlMReviewResult, LlmPassResult, MemoryStateStore, PruningDecision,
    SkillCluster, SkillSnapshot, StateTransition,
};
pub use curator_backup::{BackupError, CuratorBackup, SnapshotMeta};
pub use history::{
    ReviewDetailsPage, ReviewHistory, ReviewHistoryPage, ReviewRecord, ReviewRunStatus,
    ReviewStatus, ReviewStatusRecord,
};
pub use observability::ObservabilityStore;
pub use skill::sync::{
    compute_content_hash, compute_dir_hash, compute_hash, sync_skills, BundledManifest, SyncResult,
};
pub use skill::{SkillUsage, SkillUsageStore};
pub use skill_registry::{Lifecycle, SkillContent, SkillError, SkillMeta, SkillRegistry, Source};

pub use backfill_triggers::{run_backfill_triggers, BackfillTriggersOutcome, DEFAULT_BATCH_SIZE};
pub use workflow::{
    run_curator_if_needed, run_curator_llm_if_needed, skills_default_path_public,
    wait_for_pending_reviews,
};

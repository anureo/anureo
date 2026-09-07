pub mod bash;
pub mod batch;
pub mod date;
pub mod exa;
pub mod file;
pub mod http_retry;
pub mod mcp;
pub mod mcp_adapter;
pub mod powershell;
pub mod shared;
pub mod skill;
pub mod todo;
pub mod web;

// Re-export tools
pub use bash::{BashTool, CommandExecutor, LocalCommandExecutor, TOOL_BASH};
pub use batch::{BatchTool, TOOL_BATCH};
pub use date::{DateTool, TOOL_DATE};
pub use file::*;
pub use mcp::{McpSession, McpSessionError, McpToolSource};
pub use mcp_adapter::{register_mcp_tools, register_mcp_tools_with_specs, McpToolAdapter};
pub use powershell::{
    LocalPowerShellExecutor, PowerShellExecutor, PowerShellTool, TOOL_POWERSHELL,
};
pub use shared::{canceller::*, shell_output::*};
pub use skill::{SkillListTool, SkillManagerTool, SkillViewTool, TOOL_SKILL_MANAGE};
pub use todo::*;
pub use web::WebFetcherTool;

use std::path::Path;
use std::sync::Arc;

use tool_core::{ToolRegistryLocked, ToolSourceError};

pub async fn register_bash_tools(registry: &ToolRegistryLocked) {
    registry.register_async(Box::new(BashTool::new())).await;
}

pub fn register_file_tools(
    aggregate: &ToolRegistryLocked,
    working_folder: impl AsRef<Path>,
    allow_outside: bool,
    _skill_registry: Option<Arc<::skill::SkillRegistry>>,
    _skill_usage: Option<::skill::SkillUsageStore>,
    is_background_review: bool,
) -> Result<(), ToolSourceError> {
    let path = working_folder.as_ref();
    let canonical = path.canonicalize().map_err(|e| {
        ToolSourceError::InvalidInput(format!(
            "working folder not found or not a directory: {} (path: {})",
            e,
            path.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(ToolSourceError::InvalidInput(
            "working folder is not a directory".to_string(),
        ));
    }
    let working_folder = Arc::new(canonical);
    aggregate.register_sync(Box::new(file::LsTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::ReadFileTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::WriteFileTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::EditFileTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::MultieditTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::ApplyPatchTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::MoveFileTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::DeleteFileTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::CreateDirTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::GlobTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(file::GrepTool::new(
        working_folder.clone(),
        allow_outside,
    )));
    aggregate.register_sync(Box::new(todo::TodoWriteTool::new(working_folder.clone())));
    aggregate.register_sync(Box::new(todo::TodoReadTool::new(working_folder.clone())));
    aggregate.register_sync(Box::new(date::DateTool::new()));

    if let Some(registry) = _skill_registry {
        let (list_tool, view_tool) = skill::make_skill_tools_with_registry(registry, _skill_usage);
        aggregate.register_sync(Box::new(list_tool));
        aggregate.register_sync(Box::new(view_tool));
    } else if let Some(usage) = _skill_usage {
        let (list_tool, view_tool) =
            skill::make_skill_tools_with_folder(working_folder.clone(), Some(usage));
        aggregate.register_sync(Box::new(list_tool));
        aggregate.register_sync(Box::new(view_tool));
    }

    let skills_dir = path.join(".anureo/skills");
    let storage = Arc::new(::skill::storage::SkillStorageRegistry::new(&skills_dir));
    let usage = Arc::new(::skill::SkillUsageStore::new(&skills_dir));
    // 011-02: route SkillManagerTool factory based on write origin.
    // For background-review agents, register the BackgroundReview factory so created skills
    // are marked `agent-created` (see `SkillManagerTool::handle_create` at manage.rs:219).
    let manager = if is_background_review {
        skill::SkillManagerTool::for_background_review(storage, Some(usage))
    } else {
        skill::SkillManagerTool::for_foreground(storage, Some(usage))
    };
    aggregate.register_sync(Box::new(manager));

    Ok(())
}

pub async fn register_web_tools(registry: &ToolRegistryLocked) {
    registry
        .register_async(Box::new(WebFetcherTool::new()))
        .await;
}

#[cfg(test)]
static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn env_test_lock() -> &'static std::sync::Mutex<()> {
    &ENV_TEST_LOCK
}

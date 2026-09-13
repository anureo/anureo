//! End-to-end test: prove `build_react_config` registers the workflow tool
//! AND its `workflow` builtin skill before returning the SkillRegistry.
//!
//! This is the path the CLI, ACP, and telegram-bot front-ends actually use.
//! It catches the bug where `register_extra_tools` was called AFTER
//! `build_react_config` returned, silently dropping the workflow tool's
//! `workflow` builtin skill from the agent's SkillRegistry.

use std::path::PathBuf;
use std::sync::Arc;

use agent::build_react_run_context;
use agent::run::build_react_config;
use agent::run::RunOptions;
use anureo_llm::message::UserContent;
use tool_core::{MockTool, Tool};
use tool_workflow::default_workflow_tool_provider;

fn make_run_options(
    working: PathBuf,
    provider: Option<agent::run::ExtraToolsProvider>,
) -> RunOptions {
    RunOptions {
        message: UserContent::Text(String::new()),
        working_folder: Some(working),
        session_id: None,
        cancellation: None,
        thread_id: Some("test-thread".to_string()),
        agent: None,
        verbose: false,
        verbose_level: 0,
        got_adaptive: false,
        display_max_len: 4096,
        output_json: false,
        model: None,
        mcp_config_path: None,
        output_timestamp: false,
        dry_run: false,
        debug_llm: false,
        provider: None,
        base_url: None,
        api_key: None,
        provider_type: None,
        any_stream_event_sender: None,
        bash_executor: None,
        extra_tools: None,
        default_extra_tools_provider: provider,
        acp_session_id: None,
        force_compact: false,
        chat_id: None,
        worktree: false,
        goal_mode: false,
        acp_mcp_servers: None,

        acp_mcp_sources: None,
        effort: None,
        tier: None,
    }
}

#[test]
fn build_react_config_with_provider_registers_workflow() {
    let dir = tempfile::tempdir().expect("tempdir");
    let opts = make_run_options(
        dir.path().to_path_buf(),
        Some(default_workflow_tool_provider()),
    );

    let (_config, _resolved, skill_registry) = build_react_config(&opts);

    let registry = skill_registry
        .as_ref()
        .expect("skill_registry should be Some when provider is set");
    let names: Vec<String> = registry
        .list()
        .iter()
        .map(|e| e.metadata.name.clone())
        .collect();
    assert!(
        names.contains(&"workflow".to_string()),
        "registry should contain workflow builtin skill when provider is set, got: {:?}",
        names
    );
}

#[test]
fn build_react_config_provider_pushes_six_workflow_tools_into_config() {
    let dir = tempfile::tempdir().expect("tempdir");
    let opts = make_run_options(
        dir.path().to_path_buf(),
        Some(default_workflow_tool_provider()),
    );

    let (config, _resolved, _registry) = build_react_config(&opts);

    let extra: &Arc<Vec<Arc<dyn Tool>>> = config
        .extra_tools
        .as_ref()
        .expect("config.extra_tools should be populated by default_extra_tools_provider");
    let names: Vec<&str> = extra.iter().map(|t| t.name()).collect();
    for tool in [
        "workflow_start",
        "workflow_status",
        "workflow_list",
        "workflow_events",
        "workflow_source",
        "workflow_files",
    ] {
        assert!(
            names.contains(&tool),
            "config.extra_tools should contain {tool}, got: {:?}",
            names
        );
    }
}

#[test]
fn build_react_config_preserves_caller_tools_when_appending_defaults() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut opts = make_run_options(
        dir.path().to_path_buf(),
        Some(default_workflow_tool_provider()),
    );
    opts.extra_tools = Some(Arc::new(vec![Arc::new(*MockTool::new(
        "update_goal",
        "goal lifecycle control",
        String::new(),
    )) as Arc<dyn Tool>]));

    let (config, _resolved, _registry) = build_react_config(&opts);

    let extra = config
        .extra_tools
        .as_ref()
        .expect("config.extra_tools should preserve caller tools and defaults");
    let names: Vec<&str> = extra.iter().map(|tool| tool.name()).collect();
    assert!(
        names.contains(&"update_goal"),
        "caller-provided ACP tools must survive config construction, got: {:?}",
        names
    );
    assert!(
        names.contains(&"workflow_start"),
        "default tools should still be appended, got: {:?}",
        names
    );
}

#[tokio::test(flavor = "current_thread")]
async fn caller_tools_reach_the_final_model_tool_source() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut opts = make_run_options(dir.path().to_path_buf(), None);
    opts.extra_tools = Some(Arc::new(vec![Arc::new(*MockTool::new(
        "update_goal",
        "goal lifecycle control",
        String::new(),
    )) as Arc<dyn Tool>]));

    let (mut config, _resolved, _registry) = build_react_config(&opts);
    // The final registration path does not require filesystem-backed tools;
    // keeping them disabled avoids leaving their background watchers alive in
    // this focused integration test.
    config.working_folder = None;
    let ctx = build_react_run_context(&config)
        .await
        .expect("build final React run context");
    let names: Vec<String> = ctx
        .tool_source
        .list_tools()
        .await
        .into_iter()
        .map(|tool| tool.name)
        .collect();

    assert!(
        names.iter().any(|name| name == "update_goal"),
        "caller-provided tool must reach the final model tool source, got: {names:?}"
    );
}

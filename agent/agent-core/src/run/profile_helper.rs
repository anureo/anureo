//! Agent Profile — helpers that depend on `RunOptions`.
//
//! The core profile types and functions live in `crate::profile`.
//! Consumers should import directly from that crate.

use crate::profile::{load_profile_by_name, AgentProfile, ProfileSource};

use super::RunOptions;

/// Load profile from RunOptions when `--agent` / `-P` is set: built-in agents (compile-time) or
/// resolve_named_profile. When `agent` is unset, returns [`None`] (no implicit default profile).
/// Returns the loaded profile together with its source (BuiltIn / Project / User).
pub fn load_profile_from_options(opts: &RunOptions) -> Option<(AgentProfile, ProfileSource)> {
    let name = opts.agent.as_ref()?;
    load_profile_by_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use checkpoint_sqlite_store::env_test_lock;
    use anureo_llm::message::UserContent;

    #[test]
    fn builtin_dev_agent_loaded_with_embedded_instructions() {
        let opts = RunOptions {
            message: UserContent::Text(String::new()),
            working_folder: None,
            session_id: None,
            cancellation: None,
            thread_id: None,
            agent: Some("dev".to_string()),
            verbose: false,
            verbose_level: 0,
            got_adaptive: false,
            display_max_len: 200,
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
            default_extra_tools_provider: None,
            acp_session_id: None,
            async_agent_registry: None,
            subagent_sink: None,
            force_compact: false,
            chat_id: None,
            worktree: false,
            goal_mode: false,
            acp_mcp_servers: None,
            acp_mcp_sources: None,
            effort: None,
            tier: None,
        };
        let (profile, source) = load_profile_from_options(&opts).expect("built-in dev profile");
        assert_eq!(profile.name, "dev");
        assert_eq!(source, ProfileSource::BuiltIn);
        let role = profile.role.as_ref().unwrap();
        assert!(role
            .content
            .as_ref()
            .unwrap()
            .contains("Editing constraints"));
        assert!(role.content.as_ref().unwrap().contains("agent"));
    }

    #[test]
    fn builtin_agent_builder_loaded_with_embedded_instructions() {
        let opts = RunOptions {
            message: UserContent::Text(String::new()),
            working_folder: None,
            session_id: None,
            cancellation: None,
            thread_id: None,
            agent: Some("agent-builder".to_string()),
            verbose: false,
            verbose_level: 0,
            got_adaptive: false,
            display_max_len: 200,
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
            default_extra_tools_provider: None,
            acp_session_id: None,
            async_agent_registry: None,
            subagent_sink: None,
            force_compact: false,
            chat_id: None,
            worktree: false,
            goal_mode: false,
            acp_mcp_servers: None,
            acp_mcp_sources: None,
            effort: None,
            tier: None,
        };
        let (profile, source) =
            load_profile_from_options(&opts).expect("built-in agent-builder profile");
        assert_eq!(profile.name, "agent-builder");
        assert_eq!(source, ProfileSource::BuiltIn);
        let role = profile.role.as_ref().unwrap();
        let content = role.content.as_ref().unwrap();
        assert!(content.contains("anureo Agent Builder"));
        assert!(content.contains("Follow the AgentProfile schema"));
    }

    #[test]
    fn load_profile_from_options_no_agent_no_default() {
        let _lock = env_test_lock().lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let prev_dir = std::env::current_dir().ok();
        let _ = std::env::set_current_dir(dir.path());
        let prev_anureo = env_config::home::override_path();
        env_config::home::set_override(Some(dir.path().to_path_buf()));

        let opts = RunOptions {
            message: UserContent::Text(String::new()),
            working_folder: None,
            session_id: None,
            cancellation: None,
            thread_id: None,
            agent: None,
            verbose: false,
            verbose_level: 0,
            got_adaptive: false,
            display_max_len: 200,
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
            default_extra_tools_provider: None,
            acp_session_id: None,
            async_agent_registry: None,
            subagent_sink: None,
            force_compact: false,
            chat_id: None,
            worktree: false,
            goal_mode: false,
            acp_mcp_servers: None,
            acp_mcp_sources: None,
            effort: None,
            tier: None,
        };
        let result = load_profile_from_options(&opts);

        if let Some(d) = prev_dir {
            let _ = std::env::set_current_dir(d);
        }
        env_config::home::set_override(prev_anureo);

        assert!(result.is_none());
    }

    #[test]
    fn load_profile_from_options_unknown_agent() {
        let _lock = env_test_lock().lock().unwrap();
        let prev_anureo = env_config::home::override_path();
        let dir = tempfile::tempdir().unwrap();
        env_config::home::set_override(Some(dir.path().to_path_buf()));

        let opts = RunOptions {
            message: UserContent::Text(String::new()),
            working_folder: None,
            session_id: None,
            cancellation: None,
            thread_id: None,
            agent: Some("nonexistent-agent-xyz".to_string()),
            verbose: false,
            verbose_level: 0,
            got_adaptive: false,
            display_max_len: 200,
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
            default_extra_tools_provider: None,
            acp_session_id: None,
            async_agent_registry: None,
            subagent_sink: None,
            force_compact: false,
            chat_id: None,
            worktree: false,
            goal_mode: false,
            acp_mcp_servers: None,
            acp_mcp_sources: None,
            effort: None,
            tier: None,
        };
        let result = load_profile_from_options(&opts);
        env_config::home::set_override(prev_anureo);
        assert!(result.is_none());
    }

    #[test]
    fn resolve_named_profile_user_level() {
        let _lock = env_test_lock().lock().unwrap();
        let anureo_home = tempfile::tempdir().unwrap();
        let agents_dir = anureo_home.path().join("agents");
        let agent_dir = agents_dir.join("custom-agent");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(
            agent_dir.join("config.yaml"),
            "name: custom-agent\ndescription: Custom\n",
        )
        .unwrap();

        let prev_anureo = env_config::home::override_path();
        env_config::home::set_override(Some(anureo_home.path().to_path_buf()));

        let prev_dir = std::env::current_dir().ok();
        let empty = tempfile::tempdir().unwrap();
        let _ = std::env::set_current_dir(empty.path());

        let result = crate::profile::resolve_named_profile("custom-agent");

        if let Some(d) = prev_dir {
            let _ = std::env::set_current_dir(d);
        }
        env_config::home::set_override(prev_anureo);

        assert!(result.is_some());
    }
}

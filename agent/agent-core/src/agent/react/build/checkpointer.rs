use std::sync::Arc;

use crate::state::ReActState;
use checkpoint::{Checkpointer, JsonSerializer, RunnableConfig};
use checkpoint_sqlite_store::SqliteSaver;
use anureo_graph_core::GraphError;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::agent::react::config::ReactBuildConfig;

pub(super) fn to_agent_error(e: impl std::fmt::Display) -> GraphError {
    GraphError::ExecutionFailed(e.to_string())
}

pub fn resolve_memory_db_path(config: &ReactBuildConfig) -> String {
    config.db_path.clone().unwrap_or_else(|| {
        checkpoint_sqlite_store::default_memory_db_path()
            .to_string_lossy()
            .into_owned()
    })
}

pub fn build_checkpointer_for_state<S>(
    config: &ReactBuildConfig,
    db_path: &str,
) -> Result<Option<Arc<dyn Checkpointer<S>>>, GraphError>
where
    S: Clone + Send + Sync + 'static + Serialize + DeserializeOwned,
{
    if config.thread_id.is_none() {
        return Ok(None);
    }
    let serializer = Arc::new(JsonSerializer);
    let saver = SqliteSaver::new(db_path, serializer).map_err(to_agent_error)?;
    Ok(Some(Arc::new(saver) as Arc<dyn Checkpointer<S>>))
}

pub(super) fn build_checkpointer(
    config: &ReactBuildConfig,
    db_path: &str,
) -> Result<Option<Arc<dyn Checkpointer<ReActState>>>, GraphError> {
    build_checkpointer_for_state::<ReActState>(config, db_path)
}

pub(super) fn build_runnable_config(config: &ReactBuildConfig) -> Option<RunnableConfig> {
    if config.thread_id.is_none() && config.user_id.is_none() {
        return None;
    }
    Some(RunnableConfig {
        thread_id: config.thread_id.clone(),
        checkpoint_id: None,
        checkpoint_ns: String::new(),
        user_id: config.user_id.clone(),
        resume_from_node_id: None,
        // GAP-06 fix: stop dropping depth / acp_session_id at this seam.
        // The act executor reads `depth` into ToolCallContext, which is what
        // the `agent` tool's max-depth guard checks — hardcoding None here
        // kept the guard dead for every real session.
        depth: config.depth,
        acp_session_id: config.acp_session_id.clone(),
        resume_value: None,
        resume_values_by_namespace: Default::default(),
        resume_values_by_interrupt_id: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GAP-06: depth/acp_session_id must survive the ReactBuildConfig →
    /// RunnableConfig seam — the `agent` tool's max-depth guard reads it.
    #[test]
    fn build_runnable_config_propagates_depth_and_session() {
        let mut config = ReactBuildConfig::default();
        config.thread_id = Some("t1".into());
        config.depth = Some(2);
        config.acp_session_id = Some("sess-9".into());

        let rc = build_runnable_config(&config).expect("thread_id present");
        assert_eq!(rc.depth, Some(2));
        assert_eq!(rc.acp_session_id.as_deref(), Some("sess-9"));
    }

    #[test]
    fn build_runnable_config_defaults_stay_none() {
        let mut config = ReactBuildConfig::default();
        config.thread_id = Some("t1".into());

        let rc = build_runnable_config(&config).expect("thread_id present");
        assert_eq!(rc.depth, None);
        assert_eq!(rc.acp_session_id, None);
    }
}

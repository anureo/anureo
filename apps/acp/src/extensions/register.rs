use std::sync::Arc;

use crate::connection_registry::ConnectionRegistry;
use crate::global_events::GlobalEventBus;

use super::session_history::SessionHistoryHandler;
use super::session_list::SessionListHandler;
use super::ExtensionRegistry;

/// Handlers produced by [`register_default_extensions`] that need
/// post-construction wiring (e.g. binding to the agent after the registry
/// has been injected into it).
pub struct ExtensionRegistryHandles {
    pub session_history: Arc<SessionHistoryHandler>,
    pub session_list: Arc<SessionListHandler>,
    pub question: Arc<super::question::QuestionHandler>,
    /// P5b：goal 扩展句柄，需在 agent `Arc` 包装后 `bind`（thread 解析 +
    /// runtime 钩子 + goal/updated 广播）。
    pub goal: Arc<super::goal::GoalHandler>,
}

/// Register every extension domain implemented in this crate.
pub fn register_default_extensions(
    registry: &mut ExtensionRegistry,
    global_bus: Arc<GlobalEventBus>,
    connections: Option<Arc<ConnectionRegistry>>,
) -> ExtensionRegistryHandles {
    registry.register("auth", Arc::new(super::auth::AuthHandler));
    registry.register("files", Arc::new(super::files::FilesHandler));
    registry.register(
        "git",
        Arc::new(super::git::GitHandler::new().with_global_bus(global_bus.clone())),
    );
    registry.register("worktree", Arc::new(super::worktree::WorktreeHandler));
    registry.register("mcp", Arc::new(super::mcp::McpHandler));
    registry.register(
        "config_entity",
        Arc::new(super::config_entity::ConfigEntityHandler::new()),
    );
    registry.register("model", Arc::new(super::model::ModelHandler::new()));
    // P5b：goal 后端 = thread_goals；构造后经 handles.goal.bind(&agent) 接线。
    let goal_handler = match connections.as_ref() {
        Some(connections) => {
            super::goal::GoalHandler::new().with_connections(connections.clone())
        }
        None => super::goal::GoalHandler::new(),
    };
    let goal_handler = Arc::new(goal_handler);
    registry.register("goal", goal_handler.clone());
    // P8：中立 goal 控制方法（alignment 附录 C）——`_session/goal` 路由到
    // goal 域 session_control；能力广告走 initialize `_meta.goal`（registry
    // 别名不进能力快照）。
    registry.register_alias("_session/goal", "_anureo.dev/goal/session_control");
    registry.register(
        "scheduled-task",
        Arc::new(super::scheduled_task::ScheduledTaskHandler),
    );
    registry.register("connection", Arc::new(super::connection::ConnectionHandler));
    registry.register("relay", Arc::new(super::relay::RelayHandler));
    registry.register(
        "pairing",
        Arc::new(super::pairing::PairingHandler::default()),
    );
    registry.register(
        "client-auth",
        Arc::new(super::client_auth::ClientAuthHandler::default()),
    );
    let question_handler = match connections {
        Some(connections) => super::question::QuestionHandler::with_connections(connections),
        None => super::question::QuestionHandler::default(),
    };
    let question_handler = Arc::new(question_handler);
    registry.register("question", question_handler.clone());
    registry.register("github", Arc::new(super::github::GithubHandler::default()));
    registry.register(
        "notification",
        Arc::new(super::notification::NotificationHandler::default()),
    );
    registry.register("skills", Arc::new(super::skills::SkillsHandler::default()));
    registry.register(
        "session-folder",
        Arc::new(super::session_folder::SessionFolderHandler::default()),
    );
    registry.register(
        "snippet",
        Arc::new(super::snippet::SnippetHandler::default()),
    );
    registry.register(
        "command",
        Arc::new(super::command::CommandHandler::default()),
    );
    registry.register("plugin", Arc::new(super::plugin::PluginHandler::default()));
    registry.register(
        "agent",
        Arc::new(super::agent_profile::AgentProfileHandler::default()),
    );
    registry.register(
        "diagnostics",
        Arc::new(super::diagnostics::DiagnosticsHandler::default()),
    );
    registry.register(
        "project",
        Arc::new(super::project::ProjectHandler::persistent()),
    );
    registry.register("provider", Arc::new(super::provider::ProviderHandler));
    registry.register("tunnel", Arc::new(super::tunnel::TunnelHandler::default()));
    registry.register(
        "multi-run",
        Arc::new(super::multi_run::MultiRunHandler::default()),
    );
    registry.register(
        "settings",
        Arc::new(
            super::settings::SettingsHandler::persistent().with_global_bus(global_bus.clone()),
        ),
    );
    registry.register(
        "session-assist",
        Arc::new(super::session_assist::SessionAssistHandler),
    );
    registry.register(
        "session-auth",
        Arc::new(super::session_auth::SessionAuthHandler::default()),
    );
    registry.register(
        "small-model",
        Arc::new(super::small_model::SmallModelHandler::default()),
    );
    registry.register(
        "auto-review",
        Arc::new(super::auto_review::AutoReviewHandler::default()),
    );
    registry.register(
        "preview",
        Arc::new(super::preview::PreviewHandler::default()),
    );
    let terminal_manager = Arc::new(crate::terminal::TerminalManager::default());
    {
        let mgr = terminal_manager.clone();
        let bus = global_bus.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                mgr.set_bus(bus).await;
            });
        }
    }
    registry.register(
        "terminal-ext",
        Arc::new(super::terminal_ext::TerminalExtHandler::new(
            terminal_manager,
        )),
    );
    registry.register("tts", Arc::new(super::tts::TtsHandler::default()));
    registry.register(
        "dictation",
        Arc::new(super::dictation::DictationHandler::default()),
    );
    let session_list = Arc::new(SessionListHandler::new().with_global_bus(global_bus.clone()));
    registry.register("session", session_list.clone());
    registry.register(
        "session-metrics",
        Arc::new(super::session_metrics::SessionMetricsHandler::new(
            session_list.clone(),
        )),
    );

    registry.register(
        "global",
        Arc::new(super::global::GlobalHandler::new(global_bus)),
    );

    let session_history = Arc::new(SessionHistoryHandler::new());
    registry.register("session-history", session_history.clone());
    ExtensionRegistryHandles {
        session_history,
        session_list,
        question: question_handler,
        goal: goal_handler,
    }
}

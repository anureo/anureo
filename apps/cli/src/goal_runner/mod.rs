//! **goal-codex-alignment P6 冻结**：旧 detached goal runner 的 CLI 侧实现
//! （AnureoTool/ShellTool/GoalRunner）。已不在关键路径（新路径 = `agent/goal`
//! crate + `/goal` 六子命令），保留供 `anureo goal` legacy CLI 与迁移审计，
//! 移除见 P7。
pub mod runner;
pub mod tool;

pub use runner::{resume, write_mcp_config, GoalRunner};
pub use tool::{shell_tool_args, CodingTool, AnureoTool, ShellTool};

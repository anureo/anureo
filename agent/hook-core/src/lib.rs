//! hook-core：anureo 的 hook 运行时（plugin-hook-system.md 设计、
//! plugin-hook-code-plan.md §1 的实现）。
//!
//! 职责边界：
//! - 事件类型与 stdin/stdout 协议（`event` / `outcome`）
//! - 配置解析与层合并（`definition` / `registry`）
//! - argv 进程执行（`executor`，不经 shell；Windows 下 `.cmd` shim 不可直接 spawn）
//! - turn 边界排空队列（`drain`）
//! - 门面 `HookDispatcher`（`emit` 异步 + `gate_tool` 同步 gate）
//!
//! 本 crate 不依赖 agent-core / llm / tool-core，保证可单测、可独立复用。

pub mod definition;
pub mod dispatcher;
pub mod drain;
pub mod event;
pub mod executor;
pub mod outcome;
pub mod registry;

pub use definition::{parse_hooks, HookAction, HookDefinition, HookIssue, HookScope, OnFailure};
pub use dispatcher::{HookDispatcher, ToolGateVerdict};
pub use drain::{DrainItem, DrainKind, DrainQueue};
pub use event::{
    build_stdin_payload, HookContext, HookEventKind, IdleCause, ToolPayload, TurnCause,
    TurnEndCause,
};
pub use outcome::{Decision, HookOutcome, RawExit};
pub use registry::HookRegistry;

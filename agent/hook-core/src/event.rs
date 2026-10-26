//! Hook 事件类型与载荷（设计文档 §4.1 / §4.5）。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// P0 事件面全集。`as_str` 为配置与载荷中使用的规范名（slash 形式）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventKind {
    SessionStart,
    TurnStart,
    TurnEnd,
    ToolPre,
    ToolPost,
    CompactBefore,
    CompactAfter,
    ThreadIdle,
}

impl HookEventKind {
    /// 规范事件名（配置 `events` 数组与 stdin 载荷的 `event` 字段）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SessionStart => "session/start",
            Self::TurnStart => "turn/start",
            Self::TurnEnd => "turn/end",
            Self::ToolPre => "tool/pre",
            Self::ToolPost => "tool/post",
            Self::CompactBefore => "compact/before",
            Self::CompactAfter => "compact/after",
            Self::ThreadIdle => "thread/idle",
        }
    }

    /// 解析配置中的事件名。同时接受 slash 与 snake 两种拼写（`tool/pre`
    /// 与 `tool_pre`），未知名称返回 `None`（由解析层记 issue 后跳过）。
    pub fn parse(s: &str) -> Option<Self> {
        let normalized = s.trim().replace('/', "_");
        match normalized.as_str() {
            "session_start" => Some(Self::SessionStart),
            "turn_start" => Some(Self::TurnStart),
            "turn_end" => Some(Self::TurnEnd),
            "tool_pre" => Some(Self::ToolPre),
            "tool_post" => Some(Self::ToolPost),
            "compact_before" => Some(Self::CompactBefore),
            "compact_after" => Some(Self::CompactAfter),
            "thread_idle" => Some(Self::ThreadIdle),
            _ => None,
        }
    }

    /// 全部事件（文档与测试用）。
    pub const ALL: &'static [HookEventKind] = &[
        HookEventKind::SessionStart,
        HookEventKind::TurnStart,
        HookEventKind::TurnEnd,
        HookEventKind::ToolPre,
        HookEventKind::ToolPost,
        HookEventKind::CompactBefore,
        HookEventKind::CompactAfter,
        HookEventKind::ThreadIdle,
    ];
}

/// turn 的发起方（区分后台 turn，设计文档 §4.1「后台 turn 策略」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TurnCause {
    #[default]
    User,
    GoalContinuation,
    AutoReview,
    Curator,
}

impl TurnCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::GoalContinuation => "goal-continuation",
            Self::AutoReview => "auto-review",
            Self::Curator => "curator",
        }
    }
}

/// turn 结束原因。loom 自有枚举（非 codex 的 ThreadIdleCause）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndCause {
    Completed,
    Error,
    Interrupted,
}

impl TurnEndCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Error => "error",
            Self::Interrupted => "interrupted",
        }
    }
}

/// 会话转 idle 的原因（loom 自有）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleCause {
    TurnEnd,
    Interrupt,
    Startup,
}

impl IdleCause {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::TurnEnd => "turn_end",
            Self::Interrupt => "interrupt",
            Self::Startup => "startup",
        }
    }
}

/// 一次发射的完整上下文。发射点填充；不携带任何凭据（设计 §6.2）。
#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub turn_cause: TurnCause,
    /// turn/end 事件携带；其他事件为 `None`。
    pub end_cause: Option<TurnEndCause>,
    /// thread/idle 事件携带；其他事件为 `None`。
    pub idle_cause: Option<IdleCause>,
    /// >0 = 子 agent（D8 深度过滤：非 `include_subagents` 的 hook 跳过）。
    pub depth: u32,
}

impl HookContext {
    /// drain 队列键：thread 优先，缺失回退 session。
    pub fn drain_key(&self) -> Option<&str> {
        self.thread_id.as_deref().or(self.session_id.as_deref())
    }
}

/// tool/pre、tool/post 事件的工具载荷。
#[derive(Debug, Clone)]
pub struct ToolPayload {
    pub name: String,
    pub params: serde_json::Value,
    pub call_id: Option<String>,
}

/// 构造 hook 进程 stdin 的单行 JSON 载荷（设计 §4.5）。
pub fn build_stdin_payload(
    kind: HookEventKind,
    ctx: &HookContext,
    tool: Option<&ToolPayload>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "event": kind.as_str(),
        "depth": ctx.depth,
        "turnCause": ctx.turn_cause.as_str(),
    });
    let obj = payload.as_object_mut().expect("payload is object");
    if let Some(id) = &ctx.session_id {
        obj.insert("sessionId".into(), serde_json::json!(id));
    }
    if let Some(id) = &ctx.thread_id {
        obj.insert("threadId".into(), serde_json::json!(id));
    }
    if let Some(cwd) = &ctx.cwd {
        obj.insert("cwd".into(), serde_json::json!(cwd.display().to_string()));
    }
    if let Some(cause) = ctx.end_cause {
        obj.insert("turnEndCause".into(), serde_json::json!(cause.as_str()));
    }
    if let Some(cause) = ctx.idle_cause {
        obj.insert("idleCause".into(), serde_json::json!(cause.as_str()));
    }
    if let Some(tool) = tool {
        let mut tool_obj = serde_json::json!({
            "name": tool.name,
            "params": tool.params,
        });
        if let Some(call_id) = &tool.call_id {
            tool_obj
                .as_object_mut()
                .expect("tool is object")
                .insert("callId".into(), serde_json::json!(call_id));
        }
        obj.insert("tool".into(), tool_obj);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_parse_accepts_slash_and_snake() {
        assert_eq!(
            HookEventKind::parse("tool/pre"),
            Some(HookEventKind::ToolPre)
        );
        assert_eq!(
            HookEventKind::parse("session_start"),
            Some(HookEventKind::SessionStart)
        );
        assert_eq!(
            HookEventKind::parse("turn/start "),
            Some(HookEventKind::TurnStart)
        );
        assert_eq!(HookEventKind::parse("unknown/event"), None);
    }

    #[test]
    fn stdin_payload_shape() {
        let ctx = HookContext {
            session_id: Some("s1".into()),
            thread_id: Some("t1".into()),
            cwd: Some(PathBuf::from("/w")),
            end_cause: Some(TurnEndCause::Interrupted),
            idle_cause: None,
            ..Default::default()
        };
        let payload = build_stdin_payload(
            HookEventKind::TurnEnd,
            &ctx,
            Some(&ToolPayload {
                name: "write_file".into(),
                params: serde_json::json!({"path": "a.rs"}),
                call_id: Some("c1".into()),
            }),
        );
        assert_eq!(payload["event"], "turn/end");
        assert_eq!(payload["sessionId"], "s1");
        assert_eq!(payload["threadId"], "t1");
        assert_eq!(payload["cwd"], "/w");
        assert_eq!(payload["turnEndCause"], "interrupted");
        assert_eq!(payload["tool"]["name"], "write_file");
        assert_eq!(payload["tool"]["params"]["path"], "a.rs");
        assert_eq!(payload["tool"]["callId"], "c1");
        assert_eq!(payload["depth"], 0);
    }

    #[test]
    fn stdin_payload_omits_absent_fields() {
        let payload =
            build_stdin_payload(HookEventKind::SessionStart, &HookContext::default(), None);
        assert!(payload.get("threadId").is_none());
        assert!(payload.get("tool").is_none());
        assert_eq!(payload["event"], "session/start");
    }
}

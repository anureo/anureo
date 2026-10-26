//! Hook stdout 协议（设计文档 §4.5）。

use serde::Deserialize;

/// `tool/pre` 的裁决。异步事件上的 `block` 会被忽略并 warn。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Block,
}

/// 解析后的 hook 输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookOutcome {
    pub decision: Decision,
    pub reason: Option<String>,
    pub additional_context: Option<String>,
    pub user_message: Option<String>,
}

/// 进程执行裸结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawExit {
    Code(i32),
    Timeout,
    SpawnError(String),
}

impl RawExit {
    /// 是否为失败（非 0 退出码 / 超时 / 启动失败）。
    pub fn is_failure(&self) -> bool {
        !matches!(self, Self::Code(0))
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Code(c) => format!("exit code {c}"),
            Self::Timeout => "timed out".into(),
            Self::SpawnError(e) => format!("spawn failed: {e}"),
        }
    }
}

/// stdout 反序列化形态。`decision` 缺省 allow；未知 decision 值按 allow。
#[derive(Debug, Deserialize)]
struct RawOutcome {
    #[serde(default)]
    decision: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    additional_context: Option<String>,
    #[serde(default)]
    user_message: Option<String>,
}

/// 解析 hook stdout。空输出或非 JSON → `None`（= allow，仅记录）。
/// 未知字段忽略；`decision` 仅识别 `block`，其余一律按 allow。
pub fn parse_stdout(stdout: &str) -> Option<HookOutcome> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    let raw: RawOutcome = serde_json::from_str(trimmed).ok()?;
    Some(HookOutcome {
        decision: if raw.decision.as_deref() == Some("block") {
            Decision::Block
        } else {
            Decision::Allow
        },
        reason: raw.reason.filter(|s| !s.is_empty()),
        additional_context: raw.additional_context.filter(|s| !s.is_empty()),
        user_message: raw.user_message.filter(|s| !s.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_stdout_is_allow() {
        assert!(parse_stdout("").is_none());
        assert!(parse_stdout("  \r\n").is_none());
    }

    #[test]
    fn block_reason_parsed() {
        let out = parse_stdout(r#"{"decision":"block","reason":"no writes to src/legacy"}"#)
            .expect("valid json parses");
        assert_eq!(out.decision, Decision::Block);
        assert_eq!(out.reason.as_deref(), Some("no writes to src/legacy"));
        assert!(out.additional_context.is_none());
    }

    #[test]
    fn extra_fields_ignored() {
        let out = parse_stdout(
            r#"{"decision":"allow","additional_context":"fmt done","future_field":123}"#,
        )
        .expect("valid json parses");
        assert_eq!(out.decision, Decision::Allow);
        assert_eq!(out.additional_context.as_deref(), Some("fmt done"));
    }

    #[test]
    fn invalid_json_is_none() {
        assert!(parse_stdout("not json at all").is_none());
        assert!(parse_stdout("{trailing").is_none());
    }

    #[test]
    fn unknown_decision_treated_as_allow() {
        let out = parse_stdout(r#"{"decision":"deny"}"#).expect("parses");
        assert_eq!(out.decision, Decision::Allow);
    }

    #[test]
    fn raw_exit_failure_semantics() {
        assert!(!RawExit::Code(0).is_failure());
        assert!(RawExit::Code(1).is_failure());
        assert!(RawExit::Timeout.is_failure());
        assert!(RawExit::SpawnError("x".into()).is_failure());
    }
}

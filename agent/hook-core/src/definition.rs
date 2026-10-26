//! HookDefinition 与配置解析（设计文档 §4.2）。
//!
//! 配置形态（config_store JSON 层的 `hook` 数组，或 plugin.json 的
//! `hooks` 数组）：
//!
//! ```json
//! [{
//!   "name": "format-after-edit",
//!   "events": ["tool/post"],
//!   "match": { "tool": "^(write_file|edit_file)$" },
//!   "command": ["bun", "~/scripts/format.js"],
//!   "timeout_ms": 8000,
//!   "on_failure": "warn",
//!   "include_subagents": false
//! }]
//! ```
//!
//! 解析语义：单条失败记 `HookIssue` 后跳过，不 fail-fast。

use crate::event::HookEventKind;
use crate::outcome::HookOutcome;
use std::sync::Arc;
use std::time::Duration;

/// 进程内订阅者（P0 占位通道；goal/auto_review 迁移是后续演进）。
pub type BuiltinHookFn =
    Arc<dyn Fn(HookEventKind, &crate::event::HookContext) -> Option<HookOutcome> + Send + Sync>;

#[derive(Clone)]
pub enum HookAction {
    /// argv 直 spawn（不经 shell）。`{pluginDir}`/`~` 占位由 executor 展开。
    Command(Vec<String>),
    /// 进程内订阅者（同步、零开销）。
    Builtin(BuiltinHookFn),
}

impl std::fmt::Debug for HookAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Command(argv) => f.debug_tuple("Command").field(argv).finish(),
            Self::Builtin(_) => f.debug_tuple("Builtin").finish_non_exhaustive(),
        }
    }
}

/// 失败策略。`Block` 仅对 `tool/pre` 有意义（fail-open 默认，见设计 §4.3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnFailure {
    #[default]
    Warn,
    Ignore,
    Block,
}

impl OnFailure {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "ignore" => Some(Self::Ignore),
            "warn" => Some(Self::Warn),
            "block" => Some(Self::Block),
            _ => None,
        }
    }
}

/// hook 所属层（由所在配置层/manifest 决定，配置中不手写）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookScope {
    User,
    Project,
    Plugin(String),
}

impl HookScope {
    /// 展示标签。
    pub fn label(&self) -> String {
        match self {
            Self::User => "user".into(),
            Self::Project => "project".into(),
            Self::Plugin(id) => format!("plugin:{id}"),
        }
    }
}

/// 一条 hook 定义。
#[derive(Debug, Clone)]
pub struct HookDefinition {
    pub name: String,
    pub events: Vec<HookEventKind>,
    /// 工具名过滤（已锚定 `^(?:...)$`；仅 tool/* 事件生效）。
    pub match_tool: Option<regex::Regex>,
    pub action: HookAction,
    pub timeout: Duration,
    pub on_failure: OnFailure,
    pub include_subagents: bool,
    pub scope: HookScope,
}

impl HookDefinition {
    pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;
}

/// 单条解析问题（跳过 + 上层 warn 展示）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookIssue {
    pub index: usize,
    pub name: Option<String>,
    pub reason: String,
}

/// hook 名校验：非空、不含路径分隔符、不藏 `..`、不以 `.` 开头；
/// 允许字母数字/下划线/连字符（对齐 config_store 实体名语义）。
fn is_valid_hook_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('.') {
        return false;
    }
    if name.contains("..") || name.contains('/') || name.contains('\\') {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn issue(index: usize, name: Option<String>, reason: impl Into<String>) -> HookIssue {
    HookIssue {
        index,
        name,
        reason: reason.into(),
    }
}

/// 解析 `hook` 数组。`Value::Null` 或非数组 → 空结果（无 issue，缺省即无声）。
pub fn parse_hooks(
    value: &serde_json::Value,
    scope: HookScope,
) -> (Vec<HookDefinition>, Vec<HookIssue>) {
    let Some(arr) = value.as_array() else {
        return (Vec::new(), Vec::new());
    };

    let mut defs = Vec::new();
    let mut issues = Vec::new();
    for (index, entry) in arr.iter().enumerate() {
        match parse_one(entry, index, &scope) {
            Ok(def) => defs.push(def),
            Err(i) => issues.push(i),
        }
    }
    (defs, issues)
}

fn parse_one(
    entry: &serde_json::Value,
    index: usize,
    scope: &HookScope,
) -> Result<HookDefinition, HookIssue> {
    let obj = entry
        .as_object()
        .ok_or_else(|| issue(index, None, "entry must be an object"))?;

    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| issue(index, None, "missing or non-string \"name\""))?
        .to_string();
    if !is_valid_hook_name(&name) {
        return Err(issue(
            index,
            Some(name),
            "invalid hook name (allowed: alnum/_/-, no path separators, no leading dot)",
        ));
    }

    let events_val = obj
        .get("events")
        .and_then(|v| v.as_array())
        .ok_or_else(|| issue(index, Some(name.clone()), "missing or non-array \"events\""))?;
    let mut events = Vec::new();
    let mut unknown = Vec::new();
    for ev in events_val {
        match ev.as_str().map(HookEventKind::parse) {
            Some(Some(kind)) => events.push(kind),
            Some(None) => unknown.push(ev.to_string()),
            None => {
                return Err(issue(
                    index,
                    Some(name.clone()),
                    "event entry must be a string",
                ))
            }
        }
    }
    if !unknown.is_empty() {
        return Err(issue(
            index,
            Some(name.clone()),
            format!("unknown events: {}", unknown.join(", ")),
        ));
    }
    if events.is_empty() {
        return Err(issue(
            index,
            Some(name.clone()),
            "\"events\" must not be empty",
        ));
    }

    let command = obj
        .get("command")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            let argv: Option<Vec<_>> = arr.iter().map(|v| v.as_str().map(str::to_string)).collect();
            argv.filter(|argv| !argv.is_empty() && argv.iter().all(|a| !a.trim().is_empty()))
        })
        .ok_or_else(|| {
            issue(
                index,
                Some(name.clone()),
                "\"command\" must be a non-empty array of non-empty strings",
            )
        })?;

    let match_tool = match obj
        .get("match")
        .and_then(|m| m.get("tool"))
        .and_then(|t| t.as_str())
    {
        None => None,
        Some(pattern) => {
            let anchored = format!("^(?:{pattern})$");
            Some(regex::Regex::new(&anchored).map_err(|e| {
                issue(
                    index,
                    Some(name.clone()),
                    format!("invalid match.tool regex: {e}"),
                )
            })?)
        }
    };

    let timeout_ms = match obj.get("timeout_ms") {
        None => HookDefinition::DEFAULT_TIMEOUT_MS,
        Some(v) => v.as_u64().filter(|ms| *ms > 0).ok_or_else(|| {
            issue(
                index,
                Some(name.clone()),
                "\"timeout_ms\" must be a positive integer",
            )
        })?,
    };

    let on_failure = match obj.get("on_failure") {
        None => OnFailure::default(),
        Some(v) => v.as_str().and_then(OnFailure::parse).ok_or_else(|| {
            issue(
                index,
                Some(name.clone()),
                "\"on_failure\" must be one of ignore|warn|block",
            )
        })?,
    };

    let include_subagents = match obj.get("include_subagents") {
        None => false,
        Some(v) => v.as_bool().ok_or_else(|| {
            issue(
                index,
                Some(name.clone()),
                "\"include_subagents\" must be a boolean",
            )
        })?,
    };

    Ok(HookDefinition {
        name,
        events,
        match_tool,
        action: HookAction::Command(command),
        timeout: Duration::from_millis(timeout_ms),
        on_failure,
        include_subagents,
        scope: scope.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(value: serde_json::Value) -> (Vec<HookDefinition>, Vec<HookIssue>) {
        parse_hooks(&value, HookScope::User)
    }

    #[test]
    fn defaults_applied() {
        let (defs, issues) = parse(json!([{
            "name": "fmt",
            "events": ["tool/post"],
            "command": ["bun", "fmt.js"]
        }]));
        assert!(issues.is_empty());
        let def = &defs[0];
        assert_eq!(def.timeout, Duration::from_millis(10_000));
        assert_eq!(def.on_failure, OnFailure::Warn);
        assert!(!def.include_subagents);
        assert!(def.match_tool.is_none());
    }

    #[test]
    fn unknown_event_skipped_with_issue() {
        let (defs, issues) = parse(json!([{
            "name": "x",
            "events": ["tool/nope"],
            "command": ["echo"]
        }]));
        assert!(defs.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(issues[0].reason.contains("tool/nope"));
    }

    #[test]
    fn bad_regex_skipped() {
        let (defs, issues) = parse(json!([{
            "name": "x",
            "events": ["tool/pre"],
            "match": { "tool": "(unclosed" },
            "command": ["echo"]
        }]));
        assert!(defs.is_empty());
        assert!(issues[0].reason.contains("regex"));
    }

    #[test]
    fn anchor_wrapped_match_tool() {
        let (defs, _) = parse(json!([{
            "name": "x",
            "events": ["tool/pre"],
            "match": { "tool": "write_file|edit_file" },
            "command": ["echo"]
        }]));
        let re = defs[0].match_tool.as_ref().expect("regex present");
        assert!(re.is_match("write_file"));
        assert!(re.is_match("edit_file"));
        assert!(!re.is_match("write_file2"));
        assert!(!re.is_match("awrite_file"));
    }

    #[test]
    fn invalid_names_rejected() {
        for bad in ["", ".hidden", "a/b", "a\\b", "a..b"] {
            let (defs, issues) = parse(json!([{
                "name": bad,
                "events": ["turn/start"],
                "command": ["echo"]
            }]));
            assert!(defs.is_empty(), "name {bad:?} should be rejected");
            assert_eq!(issues.len(), 1);
        }
    }

    #[test]
    fn empty_command_rejected() {
        let (defs, issues) = parse(json!([{
            "name": "x",
            "events": ["turn/start"],
            "command": []
        }]));
        assert!(defs.is_empty());
        assert!(issues[0].reason.contains("command"));
    }

    #[test]
    fn null_value_is_empty_silent() {
        let (defs, issues) = parse(serde_json::Value::Null);
        assert!(defs.is_empty());
        assert!(issues.is_empty());
    }

    #[test]
    fn mixed_valid_and_invalid_entries() {
        let (defs, issues) = parse(json!([
            { "name": "good", "events": ["tool/pre"], "command": ["echo"] },
            42
        ]));
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "good");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].index, 1);
    }
}

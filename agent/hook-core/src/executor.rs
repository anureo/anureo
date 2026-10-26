//! Hook 进程执行（设计文档 §4.3）。
//!
//! - argv 直 spawn，**不经 shell**（D1：消除注入面）。
//! - Windows：argv[0] 必须是真实可执行（`node.exe`/`bun.exe`/`cmd.exe` 可解析；
//!   npm `.cmd` shim 不可直接 spawn）。
//! - stdin 写单行 JSON 载荷后关闭；stdout 上限 64KB。
//! - 并发闸：全局 8（`run_command` 内），单事件批次 4（`run_batch` 外层）。

use crate::definition::{HookAction, HookDefinition};
use crate::event::{build_stdin_payload, HookContext, HookEventKind, ToolPayload};
use crate::outcome::{parse_stdout, HookOutcome, RawExit};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Semaphore;

/// stdout 读取上限。
pub const STDOUT_CAP_BYTES: usize = 64 * 1024;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 单个 hook 的执行结果。
#[derive(Debug, Clone)]
pub struct ExecutedOutcome {
    pub def_name: String,
    pub raw: RawExit,
    /// stdout 协议解析结果（None = 无输出或非 JSON）。
    pub outcome: Option<HookOutcome>,
    pub elapsed: Duration,
    pub stdout_truncated: bool,
}

pub struct HookExecutor {
    /// 单事件批次并发上限（4）。
    per_event: Arc<Semaphore>,
    /// 全局并发上限（8）。
    global: Arc<Semaphore>,
}

impl Default for HookExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl HookExecutor {
    pub fn new() -> Self {
        Self {
            per_event: Arc::new(Semaphore::new(4)),
            global: Arc::new(Semaphore::new(8)),
        }
    }

    /// 同一事件的批次执行（`gate_tool` 用）：整批共享单事件闸，条目并发。
    /// `plugin_dirs`：hook 名 → plugin 目录（按 def 各自展开 `{pluginDir}`）。
    pub async fn run_batch(
        &self,
        defs: &[Arc<HookDefinition>],
        kind: HookEventKind,
        ctx: &HookContext,
        tool: Option<&ToolPayload>,
        plugin_dirs: &std::collections::HashMap<String, PathBuf>,
    ) -> Vec<ExecutedOutcome> {
        let _permit = self.per_event.acquire().await;
        let payload = build_stdin_payload(kind, ctx, tool);
        let futs = defs.iter().map(|def| {
            let plugin_dir = plugin_dirs.get(&def.name).map(|p| p.as_path());
            self.run_command(def, kind, ctx, &payload, plugin_dir)
        });
        futures::future::join_all(futs).await
    }

    /// 执行单个 hook：Builtin 同步调用；Command spawn 子进程。
    pub async fn run_command(
        &self,
        def: &HookDefinition,
        kind: HookEventKind,
        ctx: &HookContext,
        payload: &serde_json::Value,
        plugin_dir: Option<&Path>,
    ) -> ExecutedOutcome {
        let started = Instant::now();
        let _permit = self.global.acquire().await;

        match &def.action {
            HookAction::Builtin(f) => {
                let outcome = f(kind, ctx);
                ExecutedOutcome {
                    def_name: def.name.clone(),
                    raw: RawExit::Code(0),
                    outcome,
                    elapsed: started.elapsed(),
                    stdout_truncated: false,
                }
            }
            HookAction::Command(argv) => {
                self.run_process(def, argv, kind, ctx, payload, plugin_dir, started)
                    .await
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_process(
        &self,
        def: &HookDefinition,
        argv: &[String],
        _kind: HookEventKind,
        ctx: &HookContext,
        payload: &serde_json::Value,
        plugin_dir: Option<&Path>,
        started: Instant,
    ) -> ExecutedOutcome {
        let expanded = expand_placeholders(argv, plugin_dir);
        let mut cmd = tokio::process::Command::new(&expanded[0]);
        cmd.args(&expanded[1..])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        if let Some(cwd) = &ctx.cwd {
            cmd.current_dir(cwd);
        }
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let spawned = cmd.spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(hook = %def.name, error = %e, "hook spawn failed");
                return ExecutedOutcome {
                    def_name: def.name.clone(),
                    raw: RawExit::SpawnError(e.to_string()),
                    outcome: None,
                    elapsed: started.elapsed(),
                    stdout_truncated: false,
                };
            }
        };

        let mut payload_line = payload.to_string();
        payload_line.push('\n');

        let waited = tokio::time::timeout(def.timeout, async {
            // stdin：写载荷后立刻关闭——读 stdin 到 EOF 的进程才能退出
            // （勿把 ChildStdin 句柄留到 wait 之后，否则死锁）。
            if let Some(si) = child.stdin.take().as_mut() {
                let _ = si.write_all(payload_line.as_bytes()).await;
                let _ = si.shutdown().await;
            }
            drop(child.stdin.take());
            // stdout：读尽（上限内）后关闭再 wait
            let mut buf = Vec::new();
            if let Some(so) = child.stdout.as_mut() {
                let mut limited = so.take(STDOUT_CAP_BYTES as u64 + 1);
                let _ = limited.read_to_end(&mut buf).await;
            }
            drop(child.stdout.take());
            let status = child.wait().await;
            (buf, status)
        })
        .await;

        match waited {
            Ok((buf, Ok(status))) => {
                let (text, truncated) = decode_stdout(&buf);
                let outcome = parse_stdout(&text);
                ExecutedOutcome {
                    def_name: def.name.clone(),
                    raw: RawExit::Code(status.code().unwrap_or(-1)),
                    outcome,
                    elapsed: started.elapsed(),
                    stdout_truncated: truncated,
                }
            }
            Ok((_, Err(e))) => ExecutedOutcome {
                def_name: def.name.clone(),
                raw: RawExit::SpawnError(e.to_string()),
                outcome: None,
                elapsed: started.elapsed(),
                stdout_truncated: false,
            },
            Err(_) => {
                let _ = child.kill().await;
                tracing::warn!(hook = %def.name, timeout_ms = def.timeout.as_millis() as u64, "hook timed out; killed");
                ExecutedOutcome {
                    def_name: def.name.clone(),
                    raw: RawExit::Timeout,
                    outcome: None,
                    elapsed: started.elapsed(),
                    stdout_truncated: false,
                }
            }
        }
    }
}

/// stdout 解码（lossy）+ 截断标记。
fn decode_stdout(buf: &[u8]) -> (String, bool) {
    let truncated = buf.len() > STDOUT_CAP_BYTES;
    (String::from_utf8_lossy(buf).into_owned(), truncated)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

/// argv 占位展开：`~` / `~/...` → home；`{pluginDir}` / `{pluginDir}/...`
/// → plugin 目录。**仅整段（前缀）替换，不做子串替换**，避免注入拼接。
/// 无法展开（home/plugin_dir 缺失）时保留原 token。
pub fn expand_placeholders(argv: &[String], plugin_dir: Option<&Path>) -> Vec<String> {
    argv.iter()
        .map(|token| {
            if token == "~" {
                return home_dir()
                    .map(|h| h.to_string_lossy().into_owned())
                    .unwrap_or_else(|| token.clone());
            }
            if let Some(rest) = token
                .strip_prefix("~/")
                .or_else(|| token.strip_prefix("~\\"))
            {
                if let Some(home) = home_dir() {
                    return home.join(rest).to_string_lossy().into_owned();
                }
            }
            if token == "{pluginDir}" {
                if let Some(dir) = plugin_dir {
                    return dir.to_string_lossy().into_owned();
                }
            }
            if let Some(rest) = token
                .strip_prefix("{pluginDir}/")
                .or_else(|| token.strip_prefix("{pluginDir}\\"))
            {
                if let Some(dir) = plugin_dir {
                    return dir.join(rest).to_string_lossy().into_owned();
                }
            }
            token.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{HookAction, HookScope, OnFailure};
    use crate::outcome::Decision;

    fn command_def(name: &str, argv: &[String], timeout_ms: u64) -> HookDefinition {
        HookDefinition {
            name: name.into(),
            events: vec![HookEventKind::ToolPre],
            match_tool: None,
            action: HookAction::Command(argv.iter().map(|s| s.to_string()).collect()),
            timeout: Duration::from_millis(timeout_ms),
            on_failure: OnFailure::Warn,
            include_subagents: false,
            scope: HookScope::User,
        }
    }

    /// 输出固定 JSON：写入临时文件后用 type/cat 打印——避开 shell 引号转义
    /// （Windows 下 `cmd /C echo` 的引号转义会破坏 JSON 字面量）。
    fn echo_json_argv(json: &str) -> Vec<String> {
        use std::io::Write as _;
        let path = std::env::temp_dir().join(format!("hook-core-echo-{}.json", std::process::id()));
        let mut f = std::fs::File::create(&path).expect("create temp file");
        f.write_all(json.as_bytes()).expect("write temp file");
        drop(f);
        if cfg!(windows) {
            vec![
                "cmd".into(),
                "/C".into(),
                "type".into(),
                path.to_string_lossy().into_owned(),
            ]
        } else {
            vec!["cat".into(), path.to_string_lossy().into_owned()]
        }
    }

    fn sleep_argv() -> Vec<String> {
        if cfg!(windows) {
            vec!["ping".into(), "-n".into(), "10".into(), "127.0.0.1".into()]
        } else {
            vec!["sleep".into(), "10".into()]
        }
    }

    fn echo_stdin_argv() -> Vec<String> {
        if cfg!(windows) {
            // sort 读 stdin 至 EOF（more 在管道下行为不可靠）
            vec!["sort".into()]
        } else {
            vec!["cat".into()]
        }
    }

    fn exit_code_argv(code: i32) -> Vec<String> {
        if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), format!("exit {code}")]
        } else {
            vec!["sh".into(), "-c".into(), format!("exit {code}")]
        }
    }

    async fn run(def: &HookDefinition, plugin_dir: Option<&Path>) -> ExecutedOutcome {
        HookExecutor::new()
            .run_command(
                def,
                HookEventKind::ToolPre,
                &HookContext::default(),
                &serde_json::json!({"event": "tool/pre"}),
                plugin_dir,
            )
            .await
    }

    #[tokio::test]
    async fn block_decision_parsed() {
        let argv = echo_json_argv(r#"{"decision":"block","reason":"blocked-by-test"}"#);
        let def = command_def("b", &argv, 10_000);
        let out = run(&def, None).await;
        assert_eq!(out.raw, RawExit::Code(0));
        let outcome = out.outcome.expect("json stdout parses");
        assert_eq!(outcome.decision, Decision::Block);
        assert_eq!(outcome.reason.as_deref(), Some("blocked-by-test"));
    }

    #[tokio::test]
    async fn invalid_json_falls_back_to_allow() {
        let argv = echo_json_argv("plain-text-not-json");
        let def = command_def("b", &argv, 10_000);
        let out = run(&def, None).await;
        assert_eq!(out.raw, RawExit::Code(0));
        assert!(out.outcome.is_none());
    }

    #[tokio::test]
    async fn timeout_kills_process() {
        let argv = sleep_argv();
        let def = command_def("slow", &argv, 300);
        let out = run(&def, None).await;
        assert_eq!(out.raw, RawExit::Timeout);
        assert!(out.outcome.is_none());
        assert!(out.elapsed < Duration::from_secs(8));
    }

    #[tokio::test]
    async fn spawn_error_recorded() {
        let argv = vec!["definitely-not-a-real-exe-xyz-42".to_string()];
        let def = command_def("missing", &argv, 5_000);
        let out = run(&def, None).await;
        assert!(matches!(out.raw, RawExit::SpawnError(_)));
    }

    #[tokio::test]
    async fn exit_code_propagated() {
        let argv = exit_code_argv(3);
        let def = command_def("fail", &argv, 10_000);
        let out = run(&def, None).await;
        assert_eq!(out.raw, RawExit::Code(3));
        assert!(out.outcome.is_none());
    }

    #[tokio::test]
    async fn stdin_roundtrip() {
        let argv = echo_stdin_argv();
        let def = command_def("echo", &argv, 10_000);
        // 走 run_batch 验证 payload 构造链路
        let executor = HookExecutor::new();
        let ctx = HookContext {
            thread_id: Some("t".into()),
            ..Default::default()
        };
        let results = executor
            .run_batch(
                &[Arc::new(def)],
                HookEventKind::TurnStart,
                &ctx,
                None,
                &std::collections::HashMap::new(),
            )
            .await;
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].raw,
            RawExit::Code(0),
            "echo process should succeed"
        );
    }

    #[tokio::test]
    async fn builtin_action_invoked_inline() {
        let def = HookDefinition {
            name: "bi".into(),
            events: vec![HookEventKind::TurnEnd],
            match_tool: None,
            action: HookAction::Builtin(Arc::new(|_, _| {
                Some(HookOutcome {
                    decision: Decision::Allow,
                    reason: None,
                    additional_context: Some("from-builtin".into()),
                    user_message: None,
                })
            })),
            timeout: Duration::from_secs(1),
            on_failure: OnFailure::Warn,
            include_subagents: false,
            scope: HookScope::User,
        };
        let out = run(&def, None).await;
        assert_eq!(out.raw, RawExit::Code(0));
        assert_eq!(
            out.outcome
                .as_ref()
                .and_then(|o| o.additional_context.as_deref()),
            Some("from-builtin")
        );
    }

    #[test]
    fn placeholder_plugin_dir_expansion() {
        let dir = Path::new("/plugins/demo");
        let argv = vec![
            "node".to_string(),
            "{pluginDir}".to_string(),
            "{pluginDir}/scripts/check.js".to_string(),
            "{pluginDir}\\win-path.js".to_string(),
            "keep-{pluginDir}-literal".to_string(),
        ];
        let expanded = expand_placeholders(&argv, Some(dir));
        assert_eq!(expanded[0], "node");
        // 用与实现一致的 Path::join 期望值，平台无关断言
        let base = dir.to_string_lossy().into_owned();
        assert!(expanded[1] == base || expanded[1] == base.replace('/', "\\"));
        assert!(expanded[2].starts_with(&expanded[1]));
        assert!(expanded[2].ends_with("check.js"));
        assert!(expanded[3].ends_with("win-path.js"));
        // 子串占位不展开（防注入拼接）
        assert_eq!(expanded[4], "keep-{pluginDir}-literal");
    }

    #[test]
    fn placeholder_home_expansion() {
        let argv = vec![
            "~".to_string(),
            "~/scripts/fmt.js".to_string(),
            "relative".to_string(),
        ];
        let expanded = expand_placeholders(&argv, None);
        // home 在测试环境必然存在（USERPROFILE/HOME）
        assert_ne!(expanded[0], "~");
        assert!(expanded[1] != "~/scripts/fmt.js");
        assert!(
            expanded[1].ends_with("scripts/fmt.js") || expanded[1].ends_with("scripts\\fmt.js")
        );
        assert_eq!(expanded[2], "relative");
    }

    #[test]
    fn placeholder_missing_plugin_dir_keeps_token() {
        let argv = vec!["{pluginDir}/x.js".to_string()];
        let expanded = expand_placeholders(&argv, None);
        assert_eq!(expanded[0], "{pluginDir}/x.js");
    }

    #[test]
    fn decode_stdout_truncation_marker() {
        let buf = vec![b'x'; STDOUT_CAP_BYTES + 10];
        let (_, truncated) = decode_stdout(&buf);
        assert!(truncated);
        let (text, truncated) = decode_stdout(b"ok");
        assert_eq!(text, "ok");
        assert!(!truncated);
    }
}

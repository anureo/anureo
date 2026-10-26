//! HookDispatcher：发射门面（设计文档 §1.8）。
//!
//! - `emit`：异步事件（除 `tool/pre` 外全部）——fire-and-forget，
//!   产出写入 per-thread drain 队列，在 turn 边界排空。
//! - `gate_tool`：唯一同步 gate——并发执行全部匹配 `tool/pre` hook，
//!   任一 block 即短路；失败按 `on_failure`（默认 fail-open）。
//! - D8 深度过滤：`ctx.depth > 0` 且 hook 未声明 `include_subagents` 时跳过。

use crate::definition::{HookDefinition, OnFailure};
use crate::drain::{DrainItem, DrainQueue};
use crate::event::{build_stdin_payload, HookContext, HookEventKind, ToolPayload};
use crate::executor::HookExecutor;
use crate::outcome::Decision;
use crate::registry::HookRegistry;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

/// `tool/pre` 的 gate 裁决。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolGateVerdict {
    Allow,
    Block { hook: String, reason: String },
}

struct Inner {
    registry: RwLock<HookRegistry>,
    executor: HookExecutor,
    drains: tokio::sync::Mutex<HashMap<String, DrainQueue>>,
    /// hook 名 → plugin 目录（`{pluginDir}` 展开用）。
    plugin_dirs: RwLock<HashMap<String, PathBuf>>,
}

/// 轻量句柄（Clone，内部 Arc）。全部发射点共享同一实例。
#[derive(Clone)]
pub struct HookDispatcher {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for HookDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookDispatcher")
            .field("is_noop", &self.is_noop())
            .finish()
    }
}

impl HookDispatcher {
    pub fn from_layers(
        user: Vec<HookDefinition>,
        project: Vec<HookDefinition>,
        plugin: Vec<HookDefinition>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                registry: RwLock::new(HookRegistry::from_layers(user, project, plugin)),
                executor: HookExecutor::new(),
                drains: tokio::sync::Mutex::new(HashMap::new()),
                plugin_dirs: RwLock::new(HashMap::new()),
            }),
        }
    }

    /// 空注册表（配置缺失时的零开销占位）。
    pub fn noop() -> Self {
        Self::from_layers(Vec::new(), Vec::new(), Vec::new())
    }

    pub fn is_noop(&self) -> bool {
        self.inner
            .registry
            .read()
            .expect("registry lock poisoned")
            .is_empty()
    }

    /// P1：plugin install/enable/disable/uninstall 后整体换注册表。
    pub fn rebuild(
        &self,
        user: Vec<HookDefinition>,
        project: Vec<HookDefinition>,
        plugin: Vec<HookDefinition>,
    ) {
        *self.inner.registry.write().expect("registry lock poisoned") =
            HookRegistry::from_layers(user, project, plugin);
    }

    /// 更新 hook 名 → plugin 目录映射（`{pluginDir}` 展开用）。
    pub fn set_plugin_dirs(&self, map: HashMap<String, PathBuf>) {
        *self
            .inner
            .plugin_dirs
            .write()
            .expect("plugin_dirs lock poisoned") = map;
    }

    fn plugin_dir_for(&self, hook: &str) -> Option<PathBuf> {
        self.inner
            .plugin_dirs
            .read()
            .expect("plugin_dirs lock poisoned")
            .get(hook)
            .cloned()
    }

    /// 深度（D8）+ match.tool 过滤后的订阅者快照。
    fn matched_subscribers(
        &self,
        kind: HookEventKind,
        ctx: &HookContext,
        tool: Option<&ToolPayload>,
    ) -> Vec<Arc<HookDefinition>> {
        let registry = self.inner.registry.read().expect("registry lock poisoned");
        registry
            .subscribers(kind)
            .iter()
            .filter(|def| {
                if ctx.depth > 0 && !def.include_subagents {
                    return false;
                }
                match (&def.match_tool, tool) {
                    (Some(re), Some(tool)) => re.is_match(&tool.name),
                    (Some(_), None) => false,
                    (None, _) => true,
                }
            })
            .cloned()
            .collect()
    }

    /// 异步发射（fire-and-forget）。无订阅者时零开销（不 spawn）。
    /// `tool/post` 事件传 `Some(tool)`，其余传 `None`。
    pub fn emit(&self, kind: HookEventKind, ctx: HookContext, tool: Option<ToolPayload>) {
        let defs = self.matched_subscribers(kind, &ctx, tool.as_ref());
        if defs.is_empty() {
            return;
        }
        let payload = build_stdin_payload(kind, &ctx, tool.as_ref());
        let drain_key = ctx.drain_key().map(str::to_string);

        // 必须在 tokio runtime 内调用；无 runtime 时降级为 warn 丢弃
        // （发射点均在 async 上下文，此处是防御）。
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                event = kind.as_str(),
                "hook emit outside tokio runtime; dropped"
            );
            return;
        };

        for def in defs {
            let dispatcher = self.clone();
            let ctx = ctx.clone();
            let payload = payload.clone();
            let drain_key = drain_key.clone();
            handle.spawn(async move {
                let plugin_dir = dispatcher.plugin_dir_for(&def.name);
                let result = dispatcher
                    .inner
                    .executor
                    .run_command(&def, kind, &ctx, &payload, plugin_dir.as_deref())
                    .await;
                dispatcher
                    .record_async_result(kind, &def, result, drain_key.as_deref())
                    .await;
            });
        }
    }

    /// 异步执行结果的 drain/日志归档。
    async fn record_async_result(
        &self,
        kind: HookEventKind,
        def: &Arc<HookDefinition>,
        result: crate::executor::ExecutedOutcome,
        drain_key: Option<&str>,
    ) {
        tracing::debug!(
            hook = %def.name,
            event = kind.as_str(),
            raw = %result.raw.describe(),
            elapsed_ms = result.elapsed.as_millis() as u64,
            produced_context = result.outcome.is_some(),
            "hook executed"
        );

        if let Some(outcome) = &result.outcome {
            if outcome.decision == Decision::Block && kind != HookEventKind::ToolPre {
                // block 仅 tool/pre 有意义（设计 §4.5）
                tracing::warn!(
                    hook = %def.name,
                    event = kind.as_str(),
                    "hook returned decision=block on non-gate event; ignored"
                );
            }
            if let (Some(item), Some(key)) =
                (DrainItem::from_outcome(&def.name, outcome), drain_key)
            {
                self.push_drain(key, item).await;
            }
            return;
        }

        // 无协议输出：按失败策略归档（Block 策略在异步事件上等同 Warn）
        if result.raw.is_failure() && def.on_failure != OnFailure::Ignore {
            if let Some(key) = drain_key {
                let item = DrainItem::from_failure(
                    &def.name,
                    format!(
                        "{} after {}ms",
                        result.raw.describe(),
                        result.elapsed.as_millis()
                    ),
                );
                self.push_drain(key, item).await;
            }
        }
    }

    async fn push_drain(&self, key: &str, item: DrainItem) {
        let mut drains = self.inner.drains.lock().await;
        drains.entry(key.to_string()).or_default().push(item);
    }

    /// `tool/pre` 同步 gate。并发执行全部匹配 hook，任一 block 短路；
    /// 失败/超时按 `on_failure`（未显式声明 block 时 fail-open）。
    pub async fn gate_tool(&self, ctx: &HookContext, tool: &ToolPayload) -> ToolGateVerdict {
        let defs = self.matched_subscribers(HookEventKind::ToolPre, ctx, Some(tool));
        if defs.is_empty() {
            return ToolGateVerdict::Allow;
        }
        let plugin_dirs = self
            .inner
            .plugin_dirs
            .read()
            .expect("plugin_dirs lock poisoned")
            .clone();
        let results = self
            .inner
            .executor
            .run_batch(&defs, HookEventKind::ToolPre, ctx, Some(tool), &plugin_dirs)
            .await;

        // 订阅者序（user → project → plugin）取第一个 block
        for (def, result) in defs.iter().zip(results.iter()) {
            if let Some(outcome) = &result.outcome {
                if outcome.decision == Decision::Block {
                    return ToolGateVerdict::Block {
                        hook: def.name.clone(),
                        reason: outcome
                            .reason
                            .clone()
                            .unwrap_or_else(|| "blocked by hook".into()),
                    };
                }
                continue;
            }
            if result.raw.is_failure() && def.on_failure == OnFailure::Block {
                return ToolGateVerdict::Block {
                    hook: def.name.clone(),
                    reason: format!(
                        "hook failed ({}) with on_failure=block",
                        result.raw.describe()
                    ),
                };
            }
        }
        ToolGateVerdict::Allow
    }

    /// 排空并渲染某 thread 的注入块（消费即清空；空队列返回 None）。
    pub async fn take_drain(&self, thread_key: &str) -> Option<String> {
        let mut drains = self.inner.drains.lock().await;
        let queue = drains.get_mut(thread_key)?;
        DrainQueue::render(queue.take_all())
    }

    /// 诊断用：某 thread 当前积压条数。
    pub async fn pending_drain(&self, thread_key: &str) -> usize {
        self.inner
            .drains
            .lock()
            .await
            .get(thread_key)
            .map_or(0, DrainQueue::len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{HookAction, HookScope};
    use crate::outcome::HookOutcome;
    use std::time::Duration;

    fn builtin_def(
        name: &str,
        events: &[HookEventKind],
        f: impl Fn(HookEventKind, &HookContext) -> Option<HookOutcome> + Send + Sync + 'static,
    ) -> HookDefinition {
        HookDefinition {
            name: name.into(),
            events: events.to_vec(),
            match_tool: None,
            action: HookAction::Builtin(Arc::new(f)),
            timeout: Duration::from_secs(1),
            on_failure: OnFailure::Warn,
            include_subagents: false,
            scope: HookScope::User,
        }
    }

    fn ctx(depth: u32) -> HookContext {
        HookContext {
            thread_id: Some("t1".into()),
            depth,
            ..Default::default()
        }
    }

    fn tool(name: &str) -> ToolPayload {
        ToolPayload {
            name: name.into(),
            params: serde_json::json!({}),
            call_id: None,
        }
    }

    #[tokio::test]
    async fn noop_dispatcher_allows_everything() {
        let d = HookDispatcher::noop();
        assert!(d.is_noop());
        assert_eq!(
            d.gate_tool(&ctx(0), &tool("write_file")).await,
            ToolGateVerdict::Allow
        );
        assert!(d.take_drain("t1").await.is_none());
        // 无订阅者 emit 不 panic
        d.emit(HookEventKind::TurnEnd, ctx(0), None);
    }

    #[tokio::test]
    async fn gate_tool_block_from_builtin() {
        let d = HookDispatcher::from_layers(
            vec![builtin_def("guard", &[HookEventKind::ToolPre], |_, _| {
                Some(HookOutcome {
                    decision: Decision::Block,
                    reason: Some("no writes to src/legacy".into()),
                    additional_context: None,
                    user_message: None,
                })
            })],
            vec![],
            vec![],
        );
        assert_eq!(
            d.gate_tool(&ctx(0), &tool("write_file")).await,
            ToolGateVerdict::Block {
                hook: "guard".into(),
                reason: "no writes to src/legacy".into()
            }
        );
    }

    #[tokio::test]
    async fn gate_tool_first_block_wins_in_subscriber_order() {
        // 先 allow 后 block：应返回 block
        let d = HookDispatcher::from_layers(
            vec![
                builtin_def("a-allow", &[HookEventKind::ToolPre], |_, _| None),
                builtin_def("z-blocker", &[HookEventKind::ToolPre], |_, _| {
                    Some(HookOutcome {
                        decision: Decision::Block,
                        reason: Some("second".into()),
                        additional_context: None,
                        user_message: None,
                    })
                }),
            ],
            vec![],
            vec![],
        );
        assert_eq!(
            d.gate_tool(&ctx(0), &tool("x")).await,
            ToolGateVerdict::Block {
                hook: "z-blocker".into(),
                reason: "second".into()
            }
        );
    }

    #[tokio::test]
    async fn gate_tool_fail_open_by_default() {
        // exit 3 的进程 hook（无协议输出）+ 默认 on_failure=warn → Allow
        let argv: Vec<String> = if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "exit 3".into()]
        } else {
            vec!["sh".into(), "-c".into(), "exit 3".into()]
        };
        let def = HookDefinition {
            name: "failing".into(),
            events: vec![HookEventKind::ToolPre],
            match_tool: None,
            action: HookAction::Command(argv),
            timeout: Duration::from_secs(5),
            on_failure: OnFailure::Warn,
            include_subagents: false,
            scope: HookScope::User,
        };
        let d = HookDispatcher::from_layers(vec![def], vec![], vec![]);
        assert_eq!(
            d.gate_tool(&ctx(0), &tool("x")).await,
            ToolGateVerdict::Allow
        );
    }

    #[tokio::test]
    async fn gate_tool_fail_closed_with_block_policy() {
        let argv: Vec<String> = if cfg!(windows) {
            vec!["cmd".into(), "/C".into(), "exit 3".into()]
        } else {
            vec!["sh".into(), "-c".into(), "exit 3".into()]
        };
        let mut def = HookDefinition {
            name: "strict".into(),
            events: vec![HookEventKind::ToolPre],
            match_tool: None,
            action: HookAction::Command(argv),
            timeout: Duration::from_secs(5),
            on_failure: OnFailure::Warn,
            include_subagents: false,
            scope: HookScope::User,
        };
        def.on_failure = OnFailure::Block;
        let d = HookDispatcher::from_layers(vec![def], vec![], vec![]);
        match d.gate_tool(&ctx(0), &tool("x")).await {
            ToolGateVerdict::Block { hook, reason } => {
                assert_eq!(hook, "strict");
                assert!(reason.contains("exit code 3"));
            }
            v => panic!("expected block, got {v:?}"),
        }
    }

    #[tokio::test]
    async fn depth_skips_unless_include_subagents() {
        let d = HookDispatcher::from_layers(
            vec![builtin_def(
                "sub-guard",
                &[HookEventKind::ToolPre],
                |_, _| {
                    Some(HookOutcome {
                        decision: Decision::Block,
                        reason: Some("subagents blocked".into()),
                        additional_context: None,
                        user_message: None,
                    })
                },
            )],
            vec![],
            vec![],
        );
        // depth=1 且 include_subagents=false → 不触发
        assert_eq!(
            d.gate_tool(&ctx(1), &tool("x")).await,
            ToolGateVerdict::Allow
        );

        let mut inclusive = builtin_def("incl", &[HookEventKind::ToolPre], |_, _| {
            Some(HookOutcome {
                decision: Decision::Block,
                reason: Some("depth allowed".into()),
                additional_context: None,
                user_message: None,
            })
        });
        inclusive.include_subagents = true;
        let d2 = HookDispatcher::from_layers(vec![inclusive], vec![], vec![]);
        assert!(matches!(
            d2.gate_tool(&ctx(1), &tool("x")).await,
            ToolGateVerdict::Block { .. }
        ));
    }

    #[tokio::test]
    async fn match_tool_filters_subscribers() {
        let mut def = builtin_def("editor-only", &[HookEventKind::ToolPre], |_, _| {
            Some(HookOutcome {
                decision: Decision::Block,
                reason: Some("editors blocked".into()),
                additional_context: None,
                user_message: None,
            })
        });
        def.match_tool = Some(regex::Regex::new("^(?:write_file|edit_file)$").unwrap());
        let d = HookDispatcher::from_layers(vec![def], vec![], vec![]);
        assert_eq!(
            d.gate_tool(&ctx(0), &tool("read_file")).await,
            ToolGateVerdict::Allow
        );
        assert!(matches!(
            d.gate_tool(&ctx(0), &tool("edit_file")).await,
            ToolGateVerdict::Block { .. }
        ));
    }

    #[tokio::test]
    async fn emit_drains_context_to_thread_queue() {
        let d = HookDispatcher::from_layers(
            vec![builtin_def(
                "reporter",
                &[HookEventKind::TurnEnd],
                |_, _| {
                    Some(HookOutcome {
                        decision: Decision::Allow,
                        reason: None,
                        additional_context: Some("formatted 3 files".into()),
                        user_message: None,
                    })
                },
            )],
            vec![],
            vec![],
        );
        d.emit(HookEventKind::TurnEnd, ctx(0), None);
        // fire-and-forget：轮询等待后台任务落队
        for _ in 0..100 {
            if let Some(block) = d.take_drain("t1").await {
                assert!(block.contains("formatted 3 files"));
                assert!(block.contains("[hook reporter"));
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("drain never received hook output");
    }

    #[tokio::test]
    async fn block_decision_on_async_event_is_ignored_but_context_kept() {
        let d = HookDispatcher::from_layers(
            vec![builtin_def(
                "confused",
                &[HookEventKind::ToolPost],
                |_, _| {
                    Some(HookOutcome {
                        decision: Decision::Block,
                        reason: Some("ignored".into()),
                        additional_context: Some("ctx kept".into()),
                        user_message: None,
                    })
                },
            )],
            vec![],
            vec![],
        );
        d.emit(HookEventKind::ToolPost, ctx(0), Some(tool("x")));
        for _ in 0..100 {
            if let Some(block) = d.take_drain("t1").await {
                assert!(block.contains("ctx kept"));
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("drain never received");
    }

    #[tokio::test]
    async fn take_drain_consumes_queue() {
        let d = HookDispatcher::from_layers(vec![], vec![], vec![]);
        d.push_drain("t1", DrainItem::from_failure("h", "boom".into()))
            .await;
        assert_eq!(d.pending_drain("t1").await, 1);
        assert!(d.take_drain("t1").await.is_some());
        assert_eq!(d.pending_drain("t1").await, 0);
        assert!(d.take_drain("t1").await.is_none());
    }

    #[tokio::test]
    async fn rebuild_replaces_registry() {
        let d = HookDispatcher::noop();
        assert!(d.is_noop());
        d.rebuild(
            vec![builtin_def("late", &[HookEventKind::ToolPre], |_, _| {
                Some(HookOutcome {
                    decision: Decision::Block,
                    reason: Some("added later".into()),
                    additional_context: None,
                    user_message: None,
                })
            })],
            vec![],
            vec![],
        );
        assert!(!d.is_noop());
        assert!(matches!(
            d.gate_tool(&ctx(0), &tool("x")).await,
            ToolGateVerdict::Block { .. }
        ));
    }

    #[tokio::test]
    async fn layer_override_via_rebuild() {
        // user 层同名 hook 覆盖 plugin 层
        let plugin_hook = builtin_def("shared", &[HookEventKind::ToolPre], |_, _| {
            Some(HookOutcome {
                decision: Decision::Block,
                reason: Some("plugin says block".into()),
                additional_context: None,
                user_message: None,
            })
        });
        let d = HookDispatcher::from_layers(vec![], vec![], vec![plugin_hook]);
        match d.gate_tool(&ctx(0), &tool("x")).await {
            ToolGateVerdict::Block { reason, .. } => assert_eq!(reason, "plugin says block"),
            v => panic!("expected block, got {v:?}"),
        }

        let user_hook = builtin_def("shared", &[HookEventKind::ToolPre], |_, _| None);
        d.rebuild(vec![user_hook], vec![], vec![]);
        assert_eq!(
            d.gate_tool(&ctx(0), &tool("x")).await,
            ToolGateVerdict::Allow
        );
    }
}

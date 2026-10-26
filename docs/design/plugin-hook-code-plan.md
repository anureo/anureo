# Plugin / Hook 系统代码实施方案（P0 + P1）

> **状态**: 代码级方案（配套 [plugin-hook-system.md](plugin-hook-system.md) v2；本文细化到类型签名与插入点。行号基于 2026-12 main 快照，实现时漂移以结构为准）
> **范围**: PR-1～PR-4（P0 hook 运行时 + P1 plugin 本地装载）；P2 npm、P3 见设计文档，不在本文
> **验证过的锚点**: `run/types.rs:41-148`（RunOptions 手写 Clone）、`run/config_builder.rs:56-260`、`react/act_executor.rs:65-197`、`react/config/prompt_assembly.rs:20-38`、`skill/discovery.rs:46-140`、`tools/agent/build_config.rs:27-32`、`agent.rs:1359-1470/2220-2231`、`compress/compact_node.rs:29-60`、`extensions/mod.rs:167-189`、根 `Cargo.toml:5-77`

---

## 0. 变更总览

### 0.1 新增

| 路径 | 内容 |
|---|---|
| `agent/hook-core/` | 新 crate（§1）：event / definition / outcome / registry / executor / drain / dispatcher |
| `apps/acp/src/hooks/mod.rs` | `HookConfigLoader` + dispatcher 构建与注入（§2.10） |
| `apps/acp/src/plugin_host/{mod,manifest,scan}.rs` | P1：manifest 模型、校验、目录扫描（§3.1） |
| `apps/acp/tests/hook_e2e.rs` | P0 集成测试（§4.2） |
| `apps/acp/tests/fixtures/plugins/demo/` | demo plugin（§4.3） |

### 0.2 修改

| 文件 | 改动 |
|---|---|
| 根 `Cargo.toml:5` members | + `"agent/hook-core"` |
| `agent/agent-core/Cargo.toml` | + `hook-core`（path 依赖） |
| `agent/agent-core/src/run/types.rs` | RunOptions +3 字段，Clone 手写补 3 行 |
| `agent/agent-core/src/run/config_builder.rs` | dispatcher/plugin 资源传递、MCP 第三来源合并 |
| `agent/agent-core/src/agent/react/act_executor.rs` | `ToolCallExecutor` + hooks 字段，`execute_one` 两处插入 |
| `agent/agent-core/src/compress/{config,compact_node}.rs` | CompactionConfig + dispatcher，压缩前后 emit |
| `agent/skill/src/discovery.rs` | `SkillSource::Plugin` + `add_plugin_skills` |
| `apps/acp/src/agent.rs` | turn/session/idle 发射点 + drain 注入 |
| `apps/acp/src/runtime.rs` | dispatcher 构建、PluginHandler 注入 |
| `apps/acp/src/extensions/plugin.rs` | install/uninstall/enable/disable + 门禁 + 通知 |

依赖方向：`hook-core` → 仅 tokio/serde/serde_json/thiserror/tracing/regex；被 `agent-core` 与 `apps/acp` 依赖；**不依赖** llm/tool-core/agent-core（保证可单测、可复用）。

---

## 1. `agent/hook-core` crate

### 1.1 Cargo.toml

```toml
[package]
name = "hook-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
tokio = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
thiserror = { workspace = true }
tracing = { workspace = true }
regex = "1"

[dev-dependencies]
tokio = { workspace = true, features = ["full", "test-util"] }
```

根 `Cargo.toml` 的 `[workspace.dependencies]` 加 `regex = "1"`。

### 1.2 `event.rs` — 事件与载荷

```rust
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// P0 事件面（设计文档 §4.1）。字符串形式用于配置/载荷。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEventKind {
    SessionStart,   // "session_start"（载荷名 session/start，见 as_str）
    TurnStart,
    TurnEnd,
    ToolPre,
    ToolPost,
    CompactBefore,
    CompactAfter,
    ThreadIdle,
}

impl HookEventKind {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TurnCause { #[default] User, GoalContinuation, AutoReview, Curator }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndCause { Completed, Error, Interrupted }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleCause { TurnEnd, Interrupt, Startup } // loom 自有，非 codex ThreadIdleCause

/// 一次发射的完整上下文（发射点填充；不携带任何凭据）。
#[derive(Debug, Clone, Default)]
pub struct HookContext {
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub cwd: Option<PathBuf>,
    pub turn_cause: TurnCause,
    pub depth: u32, // >0 = 子 agent（D8 判定用）
}

#[derive(Debug, Clone)]
pub struct ToolPayload {
    pub name: String,
    pub params: serde_json::Value,
    pub call_id: Option<String>,
}

/// stdin 单行 JSON 载荷的构造（设计文档 §4.5）。
pub fn build_stdin_payload(
    kind: HookEventKind,
    ctx: &HookContext,
    tool: Option<&ToolPayload>,
    extra: Option<&serde_json::Value>,
) -> serde_json::Value;
```

### 1.3 `definition.rs` — 定义与解析

```rust
pub type BuiltinHookFn =
    Arc<dyn Fn(HookEventKind, &HookContext) -> Option<HookOutcome> + Send + Sync>;

#[derive(Clone)]
pub enum HookAction {
    /// argv 直 spawn（不经 shell）。`{pluginDir}`/`~` 占位展开。
    Command(Vec<String>),
    /// 进程内订阅者（P0 仅占位通道，goal/auto_review 迁移是后续演进）。
    Builtin(BuiltinHookFn),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnFailure { #[default] Warn, Ignore, Block } // Block 仅 ToolPre 有意义

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookScope { User, Project, Plugin(String) }

pub struct HookDefinition {
    pub name: String,
    pub events: Vec<HookEventKind>,
    pub match_tool: Option<regex::Regex>, // ^...$ 由调用方拼接锚点
    pub action: HookAction,
    pub timeout: std::time::Duration,     // 默认 10s
    pub on_failure: OnFailure,
    pub include_subagents: bool,          // 默认 false（D8）
    pub scope: HookScope,                 // 由所在层决定，配置不手写
}

/// 解析 config 的 `hook` 数组（或 plugin.json 的 `hooks`）。
/// 返回 (有效定义, 问题列表)；单条失败跳过 + HookIssue（设计 §4.2 校验失败行为）。
pub fn parse_hooks(value: &serde_json::Value, scope: HookScope)
    -> (Vec<HookDefinition>, Vec<HookIssue>);

pub struct HookIssue { pub index: usize, pub name: Option<String>, pub reason: String }
```

解析规则（逐条，错误即 `HookIssue` 后 continue）：

1. 非对象 / 缺 `name` / `name` 含 `/`、`\`、`..`、前导 `.`（复用 config_store 实体名校验语义）；
2. `events` 缺失或含未知事件名（对照 `HookEventKind::as_str`）；
3. `command` 非非空字符串数组（`Builtin` 形态只能代码构造，JSON 只有 Command）；
4. `match.tool` 非合法 regex；
5. `timeout_ms` / `on_failure` / `include_subagents` 非法值。

### 1.4 `outcome.rs` — stdout 协议

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision { Allow, Block }

#[derive(Debug, Clone)]
pub struct HookOutcome {
    pub decision: Decision,
    pub reason: Option<String>,
    pub additional_context: Option<String>,
    pub user_message: Option<String>,
}

#[derive(Debug)]
pub enum RawExit { Code(i32), Timeout, SpawnError(String) }

/// stdout 解析（容错）：空/非 JSON → None（= Allow + 仅记录）；
/// decision:block 仅在 ToolPre 语义下由调用方采纳。
pub fn parse_stdout(stdout: &str) -> Option<HookOutcome>;
```

### 1.5 `registry.rs` — 注册表与层合并

```rust
pub struct HookRegistry {
    defs: Vec<Arc<HookDefinition>>,
    // 事件 → 定义索引（构建时一次分桶）
    index: HashMap<HookEventKind, Vec<Arc<HookDefinition>>>,
}

impl HookRegistry {
    /// 层优先级（低→高）：User < Project < Plugin（设计 §4.2/§5.3）。
    /// 同层同名取先声明并 warn；跨层同名高优先级覆盖。
    pub fn from_layers(
        user: Vec<HookDefinition>,
        project: Vec<HookDefinition>,
        plugin: Vec<HookDefinition>,
    ) -> Self;

    pub fn subscribers(&self, kind: HookEventKind) -> &[Arc<HookDefinition>];
    pub fn all(&self) -> &[Arc<HookDefinition>];
    pub fn is_empty(&self) -> bool;
}
```

### 1.6 `executor.rs` — 进程执行

```rust
pub struct HookExecutor {
    per_event: Arc<tokio::sync::Semaphore>,  // 4
    global: Arc<tokio::sync::Semaphore>,     // 8
}

pub struct ExecutedOutcome {
    pub def_name: String,
    pub raw: RawExit,
    pub outcome: Option<HookOutcome>, // parse_stdout 结果
    pub elapsed: std::time::Duration,
}

impl HookExecutor {
    /// 运行一个 Command hook：spawn argv → 写 stdin 载荷 → 关 stdin →
    /// 读 stdout（上限 64KB，超限截断标记）→ timeout kill。
    pub async fn run_command(
        &self,
        def: &HookDefinition,
        kind: HookEventKind,
        ctx: &HookContext,
        payload: &serde_json::Value,
        plugin_dir: Option<&std::path::Path>,
    ) -> ExecutedOutcome;
}

/// argv 占位展开：`~`/`~/...` → home；`{pluginDir}` → plugin_dir。
/// 仅替换整段 token（不子串替换，避免注入拼接）。
pub fn expand_placeholders(argv: &[String], plugin_dir: Option<&Path>) -> Vec<String>;
```

实现要点：

- `tokio::process::Command::new(argv[0]).args(&argv[1..]).stdin(piped).stdout(piped).stderr(null)`；**不经 shell**（D1）。
- Windows 注释明示：argv[0] 必须是真实可执行（`node.exe`/`bun.exe`/`cmd.exe` 可解析；npm `.cmd` shim 不可 spawn）。
- 并发闸：`run_command` 内先 `global.acquire()`，`gate_tool` 聚合处再控每事件 4；`timeout` 用 `tokio::time::timeout` + `child.kill().await`。

### 1.7 `drain.rs` — turn 边界排空队列

```rust
pub enum DrainKind {
    Context(String),      // additional_context（单条截断 4KB）
    UserMessage(String),  // user_message
    Failed(String),       // 执行失败摘要（仅 Warn/Block 级）
}

pub struct DrainItem { pub hook: String, pub kind: DrainKind }

pub struct DrainQueue { items: std::collections::VecDeque<DrainItem>, cap: usize /*32*/ }

impl DrainQueue {
    pub fn push(&mut self, item: DrainItem); // 超容丢最旧 + tracing::warn
    pub fn take_all(&mut self) -> Vec<DrainItem>;
    /// 渲染为注入块（总长上限 16KB，超出从头截断保留最新）：
    /// "--- hook context (from async hooks) ---\n[hook <name>]\n..."
    pub fn render(items: Vec<DrainItem>) -> Option<String>;
}
```

### 1.8 `dispatcher.rs` — 门面

```rust
pub struct HookDispatcher {
    inner: std::sync::RwLock<HookRegistry>,          // rebuild 用
    executor: std::sync::Arc<HookExecutor>,
    drains: tokio::sync::Mutex<HashMap<String, DrainQueue>>, // key=thread_id（fallback session_id）
    plugin_dirs: std::sync::RwLock<HashMap<String, PathBuf>>, // hook name -> pluginDir
}

pub enum ToolGateVerdict { Allow, Block { hook: String, reason: String } }

impl HookDispatcher {
    pub fn from_layers(user, project, plugin: Vec<HookDefinition>) -> Arc<Self>;
    pub fn noop() -> Arc<Self>;                     // 空注册表（配置缺失时）
    pub fn is_noop(&self) -> bool;

    /// 异步事件（除 ToolPre 外全部）：spawn 任务并发执行匹配 hook；
    /// 结果按 DrainKind push 到 thread 队列（无订阅者时零开销）。
    /// D8：ctx.depth > 0 且 !def.include_subagents → 跳过。
    pub fn emit(&self, kind: HookEventKind, ctx: HookContext, tool: Option<ToolPayload>);

    /// 唯一同步 gate：并发执行全部匹配 ToolPre hook（总等待上限 =
    /// min(sum, max(def.timeout))），任一 Block 即短路返回。
    /// 失败/超时按 def.on_failure（Block 需显式声明，否则 fail-open）。
    pub async fn gate_tool(&self, ctx: &HookContext, tool: &ToolPayload) -> ToolGateVerdict;

    /// turn/start 时排空并渲染注入块（消费即清空）。
    pub async fn take_drain(&self, thread_key: &str) -> Option<String>;

    /// P1：plugin enable/disable/install/uninstall 后整体换注册表。
    pub fn rebuild(&self, user, project, plugin: Vec<HookDefinition>);
}
```

`emit` 内部：`tokio::spawn`，任务只借用 `Arc<Self>` 与 owned 数据（**不得**引用 PromptGuard 等短生命周期对象）；执行结果写 drain，`tracing::debug!` 记录耗时/退出码（不记载荷全文）。

### 1.9 crate 单测清单（文件内 `#[cfg(test)]`）

| 文件 | 用例 |
|---|---|
| executor.rs | `timeout_kills_process`、`block_decision_parsed`、`invalid_json_falls_back_to_allow`、`placeholder_home_expansion`、`placeholder_plugin_dir_expansion`、`stdout_cap_truncates`、`spawn_error_recorded` |
| drain.rs | `fifo_order_preserved`、`cap_evicts_oldest`、`render_truncates_total`、`render_empty_is_none` |
| registry.rs | `layer_priority_user_lt_project_lt_plugin`、`same_layer_dup_first_wins`、`index_buckets_by_event` |
| definition.rs | `unknown_event_skipped_with_issue`、`bad_regex_skipped`、`anchor_wrapped_match_tool`、`defaults_applied` |
| outcome.rs | `empty_stdout_is_allow`、`block_reason_parsed`、`extra_fields_ignored` |

---

## 2. P0 接线：发射点与配置装载

### 2.1 workspace 注册

根 `Cargo.toml` members 追加 `"agent/hook-core"`（列表无 glob，逐项显式）；`[workspace.dependencies]` 追加 `regex = "1"`。`agent/agent-core/Cargo.toml`：

```toml
hook-core = { path = "../hook-core" }
```

### 2.2 `RunOptions`（`run/types.rs:41`）

新增三字段（P0 用第一个，P1 用后两个，一次加齐避免二次动 Clone）：

```rust
/// Hook dispatcher（None = 全部发射点零开销早退）。子 agent 经
/// build_config_from_profile 的 clone 继承；深度判定在发射处（D8）。
pub hook_dispatcher: Option<std::sync::Arc<hook_core::HookDispatcher>>,
/// 已启用 plugin 的 skills 目录（P1，§3.2）。
pub plugin_skill_dirs: Option<Vec<std::path::PathBuf>>,
/// plugin 声明的 MCP servers（P1，§3.3；优先级低于 mcp.json）。
pub plugin_mcp_servers: Option<Vec<env_config::McpServerDef>>,
```

⚠️ `RunOptions` 的 `Clone` 是**手写 impl**（`types.rs:108-148`）——三个新字段各补一行 `.clone()`，漏写是编译错（字段驱动，问题不大但要注意 review）。

### 2.3 `ReactBuildConfig`（`agent/react/mod.rs` 定义处）

+ `pub hook_dispatcher: Option<Arc<hook_core::HookDispatcher>>`（`Default = None`）。`tools/agent/build_config.rs:32` 的 `parent_config.clone()` 自然继承给子 agent——**不做剔除**，D8 由发射点 depth 判定（`ToolCallExecutor` 已有 `depth: u32` 字段，来源 `run_ctx.config.depth`）。

### 2.4 `config_builder.rs`

在 Phase 2 seams 区（`:80-87`）追加：

```rust
base.hook_dispatcher = effective_opts.hook_dispatcher.clone();
```

skill registry 构建处（`build_react_config` 返回前）追加：

```rust
if let Some(dirs) = effective_opts.plugin_skill_dirs.as_ref() {
    if let Some(reg) = skill_registry.as_mut() {
        reg.add_plugin_skills(dirs); // §3.2
    }
}
```

MCP 第三来源（`:236` mcp.json 载入之后、`:243` ACP 覆盖之前）：

```rust
// plugin MCP：仅新增，不覆盖 mcp.json 同名（优先级 ACP > mcp.json > plugin）。
if let Some(plugin_servers) = effective_opts.plugin_mcp_servers.clone() {
    let mut merged = base.mcp_servers.unwrap_or_default();
    let existing: HashSet<_> = merged.iter().map(|s| s.name().to_string()).collect();
    for srv in plugin_servers {
        if existing.contains(srv.name()) {
            tracing::warn!(name = srv.name(), "plugin mcp server shadowed by higher-priority config; skipped");
        } else {
            merged.push(srv);
        }
    }
    base.mcp_servers = Some(merged);
}
```

### 2.5 `act_executor.rs` — tool/pre 与 tool/post

`ToolCallExecutor` struct（`:34-55`）+ `new()`（`:67-100`）：

```rust
hooks: Option<std::sync::Arc<hook_core::HookDispatcher>>,
// new() 内：run_ctx.config.hook_dispatcher.clone()（与 thread_id/depth 同源）
```

`execute_one`（`:133-197`）两处插入：

**tool/pre** —— ②（`:158-165` 空名检查）与 ③（`:168 run_tool`）之间，与参数解析失败分支（`:142-155`）完全同构：

```rust
// ②.5 hook gate (tool/pre)：同步、可 block。
if let Some(d) = self.hooks.as_ref() {
    let verdict = d.gate_tool(&self.hook_ctx(), &ToolPayload {
        name: tc.name.clone(),
        params: args.clone(),
        call_id: tc.id.clone(),
    }).await;
    if let ToolGateVerdict::Block { hook, reason } = verdict {
        let text = format!(
            "blocked by hook [{hook}]: {reason}. Fix the issue or adjust the hook.");
        let outcome = self.normalize(tc, &args, &text, true);
        self.emit_end(tc, &outcome, true);
        return Ok(outcome);
    }
}
```

**tool/post** —— ④ Ok 分支 `emit_end` 之后：

```rust
if let Some(d) = self.hooks.as_ref() {
    d.emit(HookEventKind::ToolPost, self.hook_ctx(), Some(ToolPayload {
        name: tc.name.clone(), params: args.clone(), call_id: tc.id.clone(),
    }));
}
```

`hook_ctx()` 私有辅助：由 `self.thread_id / acp_session_id / depth` 组装 `HookContext`（cwd 从 config.working_folder 取）。`emit`/`gate_tool` 内部做 D8 深度过滤。

### 2.6 compact 事件（`compress/`）

`CompactionConfig` + `pub hook_dispatcher: Option<Arc<HookDispatcher>>`（ReactBuildConfig 构建处随配置传入）。`CompactNode::run`（`:59` 的 `force_compact || overflow` 分支内，实际 summarize 调用前后）：

```rust
if let Some(d) = self.config.hook_dispatcher.as_ref() {
    d.emit(HookEventKind::CompactBefore, ctx_no_tool(), None);
}
let compacted = compaction::summarize(...).await; // 现有调用
if let Some(d) = self.config.hook_dispatcher.as_ref() {
    d.emit(HookEventKind::CompactAfter, ctx_no_tool(), None);
}
```

（ctx 的 thread_id 从 state/运行上下文取；拿不到的字段置 None，载荷宁缺勿假。）

### 2.7 ACP turn 咽喉点（`agent.rs:1359 prompt_with_capabilities`）

goal 的 per-turn 钩子（`:1396-1420 on_turn_start_for/exempt`）就是同位置先例，hook 三处插入：

**turn/start + drain 注入** —— `user_content` 构建（`:1422-1425`）之后：

```rust
let mut user_content = user_content;
if let Some(d) = self.hooks.as_ref() {
    // ① 排空上一轮积压（turn 边界注入，goal 消息注入先例同走消息侧）
    if let Some(block) = d.take_drain(&entry.thread_id).await {
        user_content = hook_core::prepend_context(user_content, &block);
    }
    // ② 发射 turn/start（cause：goal 续跑的标记 P0 先 User，§6 TODO）
    d.emit(HookEventKind::TurnStart, HookContext {
        session_id: Some(args.session_id.to_string()),
        thread_id: Some(entry.thread_id.clone()),
        cwd: entry.cwd.clone(),
        turn_cause: TurnCause::User,
        depth: 0,
    }, None);
}
```

`hook_core::prepend_context(UserContent, &str) -> UserContent`：Text 形态直接在头部拼接 `"{block}\n\n---\n{原文本}"`；非 Text 形态前置一个 text block（保持原 blocks）。**为什么注入消息侧而非 system prompt**：AgentCache 按 `(agent_id, model, working_folder)` 复用 handle（`config_builder.rs:163-170` 注释），system prompt 组装时机不保证每 turn；消息侧注入与 goal 续跑注入同构、逐 turn 必达。

**turn/end** —— runner 完成返回处。`prompt_with_capabilities` 尾部有多个 return 路径，方案：在主执行路径外包一层：

```rust
let started = std::time::Instant::now();
let result = self.run_prompt_core(/* 现有主体 */).await;
if let Some(d) = self.hooks.as_ref() {
    let cause = match &result { Ok(_) if was_interrupted(&cancellation) => TurnEndCause::Interrupted,
                                Ok(_) => TurnEndCause::Completed,
                                Err(_) => TurnEndCause::Error };
    d.emit(HookEventKind::TurnEnd, ctx_with(cause), None);
}
result
```

（把 `:1422` 之后到返回的主体提为 `run_prompt_core` 私有方法，纯代码移动不改语义；`was_interrupted` 依据 cancellation generation 状态判定。）

### 2.8 session/start 与 thread/idle（`agent.rs`）

- **session/start**：`session/new` 处理器在 thread 建好、返回响应**之前** `emit`（fire-and-forget spawn，不阻塞响应；`:902-937` curator 机会运行区附近即插入位）。
- **thread/idle**：两处 `set_lifecycle("idle")`（`:2226` session/load 的 `finish_restore`、`:2404` prompt 尾 idle）旁各加一行 `emit(ThreadIdle, cause)`（前者 `Startup`，后者 `TurnEnd`；`Interrupt` 走 cancel 路径，P0 先覆盖前两值）。

### 2.9 dispatcher 装载（新 `apps/acp/src/hooks/mod.rs`）

```rust
pub struct HookConfigLoader;

impl HookConfigLoader {
    /// 读 config 层 `hook` 段（user + project；custom 层同样参与，按
    /// User < Project < Custom 合并语义归入 project 级）→ dispatcher。
    /// 解析 issue 全部 tracing::warn（不 fail-fast）。
    pub fn load(working_dir: Option<&Path>) -> std::sync::Arc<HookDispatcher> {
        let Ok(layers) = crate::extensions::config_store::read_config_layers(working_dir) else {
            return HookDispatcher::noop();
        };
        let (user, i1) = parse_hooks(layers.user_config.get("hook"), HookScope::User);
        let (project, i2) = parse_hooks(layers.project_config.get("hook"), HookScope::Project);
        let (custom, i3) = parse_hooks(layers.custom_config.get("hook"), HookScope::Project);
        for issue in i1.iter().chain(&i2).chain(&i3) {
            tracing::warn!(?issue, "hook config entry skipped");
        }
        HookDispatcher::from_layers(user, project.into_iter().chain(custom).collect(), vec![])
    }
}
```

接线点：`runtime.rs` `AcpRuntime` 构造处（`register_default_extensions` 调用旁）：`let hooks = HookConfigLoader::load(working_dir);` 存入 agent（新字段 `pub hooks: Arc<HookDispatcher>`，嵌入测试用 `noop()`）。session 工厂构建 RunOptions 时：`opts.hook_dispatcher = Some(agent.hooks.clone())`。**CLI 路径（apps/cli 单跑）P0 不接**，留 P1 后按需。

---

## 3. P1：PluginHost 与装配

### 3.1 `apps/acp/src/plugin_host/`

**manifest.rs**：

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub id: String,            // ^[a-z0-9][a-z0-9-]*$，即 config entry 名
    pub name: String,
    pub version: String,       // semver，宽松校验（非空）
    pub description: Option<String>,
    pub author: Option<String>,
    #[serde(default)] pub mcp_servers: Vec<PluginMcpServer>,
    #[serde(default)] pub hooks: Vec<serde_json::Value>, // 复用 parse_hooks（scope=Plugin(id)）
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginMcpServer {
    pub name: String,
    pub command: Vec<String>,  // {pluginDir} 展开
    #[serde(default)] pub env: std::collections::BTreeMap<String, String>,
}

pub enum ManifestError { MissingFile, InvalidJson(String), InvalidId, DuplicateMcpName(String), HookIssue(Vec<HookIssue>) }

/// 校验 + 反序列化；hooks 的 command 引用若含 {pluginDir} 之外的路径
/// 不强拦（argv 本就自由），但 script 建议放 pluginDir（文档约定）。
pub fn validate_manifest(dir: &Path) -> Result<PluginManifest, ManifestError>;
```

**scan.rs / mod.rs**：

```rust
pub enum PluginState { Active, Disabled, Error(String) }

pub struct InstalledPlugin {
    pub manifest: PluginManifest,
    pub dir: PathBuf,          // plugins/<id>/
    pub state: PluginState,
    pub capabilities: Vec<&'static str>, // skill/command/hook/mcp/agent-profile 探测回填
}

pub struct HostSnapshot {
    pub plugins: Vec<InstalledPlugin>,
    pub skill_dirs: Vec<PathBuf>,              // Active 的 <dir>/skills
    pub mcp_servers: Vec<env_config::McpServerDef>, // {pluginDir} 已展开
    pub command_dirs: Vec<PathBuf>,            // <dir>/commands
    pub agent_dirs: Vec<PathBuf>,              // <dir>/agents
    pub hook_defs: Vec<hook_core::HookDefinition>, // scope=Plugin(id)
}

impl PluginHost {
    /// 扫描两个 plugins 目录（§D6 路径）× config entries：
    /// - 目录有 manifest 且 entry 启用 → Active；
    /// - entry 为 {"enabled": false} → Disabled；
    /// - 目录无 manifest / manifest 坏 → Error（entry 残留即 stale 提示）。
    pub fn scan(working_dir: Option<&Path>) -> HostSnapshot;
}
```

### 3.2 skills 装配（`agent/skill/src/discovery.rs`）

```rust
// SkillSource 枚举（:46）加 variant：
Plugin,   // label "Plugin"

impl SkillRegistry {
    /// 对齐 add_agent_skills（:140）先例；在 discover() 结果之后调用，
    /// "first wins" dedup（:130）→ 用户/项目 skill 同名天然优先。
    pub fn add_plugin_skills(&mut self, plugin_skill_dirs: &[PathBuf]) {
        for dir in plugin_skill_dirs {
            if dir.is_dir() {
                let found = scan_skills_dir_recursive(dir, SkillSource::Plugin);
                let known: HashSet<_> = self.skills.iter()
                    .map(|s| s.metadata.name.clone()).collect();
                for s in found {
                    if known.contains(&s.metadata.name) {
                        tracing::warn!(skill = %s.metadata.name,
                            "plugin skill shadowed by user/project skill; skipped");
                    } else { self.skills.push(s); }
                }
            }
        }
    }
}
```

（若 `add_agent_skills` 实为独立 trait/结构方法，实现时对齐其真实挂点——语义不变。）

### 3.3 MCP / commands / agents / hooks 装配

| 资源 | 接线 |
|---|---|
| MCP | `PluginHost.scan` → session 工厂填 `RunOptions.plugin_mcp_servers` → `config_builder` §2.4 合并（ACP > mcp.json > plugin） |
| commands | `command.rs` 的发现函数（user/project 命令目录清单处）追加 `snapshot.command_dirs`；触发 `available_commands_update` |
| agent profiles | agent_registry 的 `.anureo/agents` 发现处追加 `snapshot.agent_dirs`（同名冲突同 skills 规则） |
| hooks | `dispatcher.rebuild(user, project, snapshot.hook_defs)` —— install/enable/disable/uninstall 后统一走 `refresh_plugins()`（下） |

`apps/acp` 新增 `refresh_plugins()`：scan → 更新 agent 持有的 plugin 资源集 → `dispatcher.rebuild` → `GlobalEventBus` 发 `plugin/changed` → `CapabilityManager.update` + `capability_changed`（发射点在 global_events.rs 广播设施上核实补齐）→ 涉及 commands 时 `available_commands_update`。

### 3.4 plugin.rs RPC 扩展

**门禁**（设计 §6.1，P1 硬验收）：

```rust
pub struct PluginAuthPolicy {
    allowed_principals: Option<HashSet<String>>, // None = 本地单用户，全放行
}
impl PluginAuthPolicy {
    pub fn local() -> Self { Self { allowed_principals: None } }
    pub fn restricted(list: HashSet<String>) -> Self { Self { allowed_principals: Some(list) } }
    pub fn check(&self, ctx: &ExtensionContext) -> Result<(), ExtensionError> {
        match &self.allowed_principals {
            None => Ok(()),
            Some(set) if set.contains(&ctx.principal) => Ok(()),
            _ => Err(ExtensionError::forbidden("plugin write requires admin principal")),
        }
    }
}
```

`PluginHandler::new(policy)`；`runtime.rs` 构造：stdio/本地默认 `local()`；cowork 嵌入按宿主注入 `restricted(...)`（来源：server 启动参数/环境变量 `ANUREO_ADMIN_PRINCIPALS`，实现时对齐 server principal 体系）。**门禁未配置 `restricted` 前，多宿主部署文档明示不得暴露 plugin 写方法**。

**方法**（对齐 spec 23，保留现有 `entry_*`/`file_*`/`registry`）：

```
plugin/install   {source: {type:"path", path}} → {id, name, version, state:"active",
                   capabilities, installedAt, autoEnable 默认 false}
                   流程：路径域校验（复用现有校验）→ 复制进 plugins/<id>/ →
                   validate_manifest → entry 写入（复用 entry_create 内部函数）→
                   refresh_plugins() → 返回。任一步失败回滚（删半装目录）。
plugin/uninstall {id, cleanup:bool} → {cleanup: {hooks:"ok"|"failed", mcpServers:…, commands:…}}
                   流程：refresh 前置快照 → 移除 entry → refresh_plugins() →
                   删目录；失败项收集进 cleanup 报告（spec 23 语义）。
plugin/enable    {id} / plugin/disable {id} → entry 更新 + refresh_plugins()
plugin/list      合并 entries + 目录扫描 → PluginItem[]（spec 23 形状）
```

所有写方法首行 `self.policy.check(ctx)?`。

---

## 4. 测试

### 4.1 crate 单测

§1.9 清单（`cargo nextest run -p hook-core`）。

### 4.2 P0 集成（`apps/acp/tests/hook_e2e.rs`，模式对齐 `e2e_goal_neutral.rs` 真 ACP 链路）

hook 命令用 `["cmd", "/c", "echo {json}"]`（Windows：cmd.exe 可直接 spawn；跨平台分支用 `["printf"]`/`["echo"]`）。用例：

| 用例 | 断言 |
|---|---|
| `tool_pre_block_visible_to_model` | hook 输出 `{"decision":"block","reason":"no writes to src/legacy"}` → write_file legacy 路径被拒，模型收到 blocked 文本（session/update tool_call 块） |
| `tool_post_context_injected_next_turn` | tool/post hook 输出 additional_context → 下一轮 prompt 的用户消息头部含 hook context 块 |
| `drain_fifo_and_cap` | 两个 hook 顺序产出 → 注入顺序 FIFO |
| `no_config_zero_diff` | 无 hook 配置 → 既有全量测试行为不变（回归基线） |
| `hook_injection_not_activity` | drain 注入后 idle 时钟不被重置（curator gate 断言） |
| `subagent_depth_skipped` | depth>0 的 agent 工具子调用不触发 hook（include_subagents=false） |
| `timeout_fail_open` | hook 超时（sleep 30, timeout 200ms）→ tool 照常执行，drain 有 Failed 项 |

### 4.3 P1 集成（`apps/acp/tests/plugin_install_lifecycle.rs` + fixtures）

fixtures：

```
tests/fixtures/plugins/demo/
  plugin.json        # 1 hook(tool/pre block demo-legacy) + 1 mcpServer(echo)
  skills/demo-skill/SKILL.md
  commands/demo.md
  agents/demo.yaml
  scripts/check.js
```

用例：`install_activates_all_capabilities` / `disable_emits_capability_changed` / `uninstall_cleanup_report` / `non_admin_write_rejected`（restricted policy 下）/ `stale_entry_reported_as_error`。

### 4.4 回归门禁

```powershell
cargo nextest run --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

---

## 5. PR 拆分

| PR | 内容 | 验收 |
|---|---|---|
| **PR-1** | `hook-core` crate 全量 + 单测（纯新增，零接线） | `cargo nextest run -p hook-core` 全绿；clippy 零警告 |
| **PR-2** | §2 全部发射点 + `HookConfigLoader` + runtime 接线 + `hook_e2e.rs` | P0 验收清单（设计文档 §7 P0）+ 全 workspace 回归绿 |
| **PR-3** | `plugin_host` + skill crate 改动 + 四个装配点 + `refresh_plugins`（无新 RPC） | 装配快照测试 + 手测 demo plugin 目录 |
| **PR-4** | plugin.rs 生命周期 RPC + `PluginAuthPolicy` + notifications + spec 23 修订 + fixtures | `plugin_install_lifecycle.rs` 全绿 + spec PR 过通信审查清单 |

顺序依赖：PR-2 依赖 PR-1；PR-3 依赖 PR-2；PR-4 依赖 PR-3。PR-1/PR-2 合入即 P0 完结、可独立交付价值。

---

## 6. 实现注意事项（踩坑清单）

1. **RunOptions Clone 手写**（types.rs:108）：新字段漏 clone 是编译错——照补即可，别改成 derive（会破坏 `message` 的特化 clone 语义？现有手写保留原因见 impl）。
2. **子 agent 继承靠 clone、过滤靠 depth**（build_config.rs:32）：不要在 config_builder 里剔除 dispatcher——`depth` 判定一处逻辑胜过两处传播控制。
3. **emit 任务生命周期**：fire-and-forget 任务只持 `Arc<HookDispatcher>` + owned 载荷；禁止借用 PromptGuard/session entry。
4. **Windows spawn**：集成测试与示例 hook 一律 `cmd.exe`/`node.exe` 直 argv；npm `.cmd` shim 不可 spawn（D1 注记写入 hooks 文档页）。
5. **drain 注入走消息侧**（§2.7）：AgentCache 复用 handle，system prompt 不可靠；goal 续跑消息注入是先例。
6. **turn cause 标记**：goal 续跑/AutoReview turn 的 cause 区分需要 AcpTurnDriver → prompt 路径带标（RunOptions 侧注或 session map）；P0 全部记 `User`，TODO 在 `prompt_with_capabilities` 注释标注。
7. **hook 配置不进 config.toml**（foundation/config 体系），只进 config_store JSON 层——两套配置体系勿混（设计 §4.2）。
8. **executor stdout 上限 64KB / 单条 context 4KB / drain 总 16KB**：防 hook 输出打爆上下文。
9. **门禁先行**：PR-4 中 `restricted` 模式未验证前，entries 不参与运行时装配（PR-3 的 scan 结果对多宿主模式可先用空集开关顶住——实现时在 `refresh_plugins` 加 policy 判定）。
10. **`scope_of_entry` 启发式不动**：现有 entry_* 的显示语义保持；运行时 scope 以 manifest/层为准。

---

## 7. 与设计文档的对应

| 设计（plugin-hook-system.md v2） | 本文 |
|---|---|
| D1/D3/D8 执行模型 | §1.6/§1.8/§2.5 |
| §4.1 事件面 | §1.2、§2.5-2.8 |
| §4.2 配置格式与校验 | §1.3、§2.9 |
| §4.3 drain 注入 prompt_assembly | **修订**：改消息侧注入（§2.7，AgentCache 原因）——回写设计文档 |
| §5.1-5.4 plugin 装配与 RPC | §3 |
| §6.1 授权 | §3.4 PluginAuthPolicy |
| §8 测试 | §4 |

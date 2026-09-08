# anureo Tool、MCP 与 Skill 开发

> **状态**：基于当前源码的贡献者说明
> **相关代码**：`agent/tool/tool-core`、`agent/tool/tool-basic`、`agent/skill`、`foundation/config/src/mcp_config.rs`
> **安全参考**：[security-and-privacy.md](../user-guide/07-security-and-privacy.md)

本文面向 anureo 贡献者，说明当前 Tool、MCP 和 Skill 的实现边界、调用流程、扩展点与测试方法。结论只依据本文列出的当前源码和测试；计划列表中的 `agent/tool/tool-basic/src/mod.rs` 在当前仓库不存在，因此不把它当作模块或 API 依据。未在这些源码中出现的 CLI、配置项、传输能力和行为不视为已实现。

## 1. 结论先行

- `tool-core` 只定义通用 `Tool`、`ToolSpec`、`ToolCallContext`、`ToolRegistry` 和过滤/调用机制；具体文件、shell、MCP、Skill 工具属于 `tool-basic`，不能把 MCP 协议细节下沉到 registry。
- MCP 的当前适配路径是 `McpToolSource` → `McpToolAdapter` → `ToolRegistryLocked`：先通过 `tools/list` 缓存每个工具的 `ToolSpec`，调用时再通过共享 source 转发 `tools/call`。工具名没有自动加 server 前缀，多个 server 的同名工具可能在 registry 中互相覆盖。
- 当前 MCP 支持 stdio 和 Streamable HTTP。stdio 会 spawn 子进程并由 `rmcp` 完成 initialize；HTTP 使用 `StreamableHttpClientTransport`。配置模型可以读取 `oauth`，但指定的 session 实现没有 OAuth 流程，不能把该字段描述为已生效认证 API。
- Skill 分为 discovery 的文件系统条目和 `Tool::builtin_skill()` 提供的 binary-embedded 条目。`skill_view` 负责按需加载，`skill_manage` 负责写入；写入前的 frontmatter/content/path validation 与可选的 post-write security scan 是两个不同层次。
- 文件工具默认把路径限制在 canonical working folder；`register_file_tools` 统一 canonicalize 工作目录并传入 `allow_outside`。设置 `allow_outside` 会跳过 containment check，属于高风险开关，应与项目安全策略和用户确认一起设计。
- `ToolRegistry` 的 `list` filter、`call_filter`、`dry_run` 是独立控制面；YAML spec 可以完整替换 `list` 输出中同名工具的 `ToolSpec`，但不创建 `Tool`、不改变 `call` 路由，当前 `TOOL_YAML_FILES` 为空，不能依赖它为工具补齐规格。

## 2. Tool 核心边界

### 2.1 `Tool` 是最小扩展接口

`agent/tool/tool-core/src/tool.rs` 的 `Tool` trait 只有四个相关入口：

```rust
fn name(&self) -> &str;
fn spec(&self) -> ToolSpec;
async fn call(
    &self,
    args: serde_json::Value,
    ctx: Option<&ToolCallContext>,
) -> Result<ToolCallContent, ToolSourceError>;
fn builtin_skill(&self) -> Option<BuiltinSkill>;
```

新增本地工具通常只需要实现该 trait，并在 owning crate 提供注册函数。`ToolSpec` 的 `name` 应与 `name()` 一致；输入参数由 `input_schema` 描述，调用实现仍必须自行校验必填字段和类型。错误应使用 `ToolSourceError` 的相应变体，而不是把错误 JSON 当成成功结果返回，除非该工具的协议明确要求结构化错误响应。

`BuiltinSkill` 用于随工具一起编译进 binary 的 `SKILL.md`，内容包括 `name`、`description`、`triggers`、可选的 `requires_tools` 和 embedded reference files。它不是另一种 Tool 调用协议：registry/Agent 在启动时把它加入 Skill registry，Agent 再通过 `skill` 工具按需读取。

### 2.2 Registry 的注册、列举和调用

`agent/tool/tool-core/src/registry.rs` 中，`ToolRegistry` 用 `HashMap<String, Box<dyn Tool>>` 按工具名保存实现；重复 `register` 会用新值替换旧值。`ToolRegistryLocked` 在外层用 `Arc<tokio::sync::RwLock<ToolRegistry>>`，提供 `register_async`、`list_tools` 和 `call_tool`。

调用链如下：

```text
register(_async) → ToolRegistry.tools[name]
                     │
list_tools ──────────┴→ tool.spec → filter → YAML override
call_tool(name,args,ctx)
  → filter
  → call_filter
  → dry_run（若启用则不执行工具）
  → tool.call(args, effective_ctx)
```

`filter` 控制工具能否出现在 `list`，也会参与 `call`；`call_filter` 只额外限制调用。`BuiltinToolFilter` 的 whitelist 与 blacklist 同时存在时，两者都必须通过，blacklist 优先否决。`dry_run` 返回 `(dry run: <name> was not executed)` 文本，不会进入具体工具。

如果 `call_tool` 收到显式 `ctx`，registry 会把它保存为最近一次 context；后续没有显式 context 的调用可能复用该值。这个 context 不是 per-call 隔离机制。`ToolCallContext` 可携带 recent messages、thread/user/ACP session id、depth、cancellation 和 typed event 的 JSON adapter；工具是否使用这些字段由具体实现决定。

`register_sync` 是同步阻塞桥接：它另起线程，在该线程创建 current-thread Tokio runtime，完成注册后由调用线程 `join`。因此它不会在当前 Tokio runtime 内嵌套启动 runtime，但在 async 上下文调用仍会阻塞执行器，而且每次注册都会创建线程和 runtime；异步代码应优先使用 `register_async`。

### 2.3 YAML spec 的当前限制

`agent/tool/tool-core/src/yaml_specs.rs` 的 `TOOL_YAML_FILES` 是空切片，并保留了“迁移后恢复 YAML 文件”的 TODO。`load_yaml_specs()` 会把 YAML 完整反序列化为 `ToolSpec`，再按 `name` 建表；`apply_yaml_overrides` 对 `list()` 中的同名项执行整个 `ToolSpec` 替换，而不是只覆盖 `description`。所以 YAML 可以改变 `name`、`input_schema`、`output_hint` 等 spec 字段，但不会创建真实 `Tool`，也不会改变 `call()` 路由。若产品只允许覆盖 `description`，应在源码增加字段级合并并补对应测试。新增工具应直接实现 `spec()`，不要依赖当前空的 YAML 层。

## 3. 基础工具与安全边界

### 3.1 文件工具

`agent/tool/tool-basic/src/lib.rs` 的 `register_file_tools` 是文件工具的组合入口。它先 canonicalize `working_folder`，拒绝不存在或非目录的路径，然后把同一个 `Arc<PathBuf>` 和 `allow_outside` 传给 `LsTool`、`ReadFileTool`、`WriteFileTool`、`EditFileTool`、`MultieditTool`、`ApplyPatchTool`、`MoveFileTool`、`DeleteFileTool`、`CreateDirTool`、`GlobTool`、`GrepTool`，以及 Todo/Date 工具。

具体路径检查集中在 `agent/tool/tool-basic/src/file/path.rs` 的 `resolve_path`：默认模式要求规范化路径和已解析的父目录位于 working folder 内，并处理 symlink/新文件父目录；`allow_outside = true` 时绝对路径可直接使用，relative path 仍以 working folder 为基准，但不再做 containment check。新增文件工具必须复用这个 helper，不要自行拼接路径。

`ReadFileTool` 的当前契约是：`path` 必填，`offset` 为 0-based line index，`limit` 默认 2000，长行按 2000 的长度上限截断，并以 `cat -n` 风格输出。`encoding` 出现在 schema 中，但实现使用 `std::fs::read_to_string`，当前只实际读取 UTF-8；不要据此承诺任意编码支持。

`ApplyPatchTool` 接受一个 `patchText`，解析 `*** Begin Patch`/`*** End Patch` 之间的 Add、Update、Delete，Update 还可 Move。每个路径仍经过 `resolve_path`；Delete 对目录使用 `remove_dir_all`。因此它的影响面不仅是文本替换，调用前要审查 patch 中的删除和移动目标。

### 3.2 Bash executor

`agent/tool/tool-basic/src/bash/executor.rs` 以 `CommandExecutor` trait 抽象执行器，`LocalCommandExecutor` 在 Unix 使用 `sh -c`，Windows 使用 `powershell -NoProfile -Command`。`timeout_ms` 为 `None` 或 `0` 时采用 120000 ms；命令的 stdout/stderr 先重定向到 working directory 下的 shell output 文件。正常结束后读取并删除临时输出文件；超时则保留输出文件、detach 子进程并返回 `timed_out` 与相对文件路径；取消会 kill 子进程并返回 `ToolSourceError::Transport("command cancelled")`。

这个实现的 timeout 行为不是强制终止：超时分支明确 detach 进程。新增 command executor 或修改输出策略时必须同时测试 cancellation、超时、输出文件清理和泄露的进程风险。

## 4. MCP：配置、session 与 adapter

### 4.1 配置文件与解析

`foundation/config/src/mcp_config.rs` 定义 Cursor/Claude 兼容的 JSON 根对象：`mcpServers` 映射 server name 到 `McpServerEntry`。entry 当前字段是 `command`、`args`、`env`、`disabled`、`url`、`headers`、`oauth`。

配置路径优先级由 `discover_mcp_config_path` 实现：存在的显式 `override_path` → `working_dir/.anureo/mcp.json` → `anureo_home()/mcp.json`；都不存在时返回 `None`。`parse_mcp_config` 跳过 `disabled: true`；若同时有 `url` 和 `command`，url 优先；url 必须以 `http://` 或 `https://` 开头；否则必须有非空 command。配置保存是“写固定 sibling `json.tmp` 临时文件后替换目标文件”的 best-effort replace，upsert/remove 也复用该路径；源码没有 `sync_all` 或并发写协调，不能把它承诺为具备 durability 或并发安全的完整原子保存契约。若要承诺这些性质，需要锁、唯一临时名、`sync_all`、rename 失败清理和并发测试。

配置解析得到的 `McpServerDef::Stdio` 或 `McpServerDef::Http` 只是 foundation 层的数据模型。本文指定源码没有展示从该 enum 到 Agent registry 的完整 wiring；贡献者修改入口时应继续追踪实际调用方，不要把 `parse_mcp_config` 当成已经建立 MCP session 的 API。

### 4.2 两种 transport

`agent/tool/tool-basic/src/mcp/session.rs` 的 `McpSession::new` 使用 `TokioChildProcess` spawn server，传入 command/args/env，并通过 `rmcp::ServiceExt::serve` 完成 initialize。Windows 下 `npx`、`npm`、`yarn`、`pnpm` 以及 `.cmd`/`.bat` 会包成 `cmd /C`；其他平台不包裹。

`agent/tool/tool-basic/src/mcp/session_http.rs` 的 `McpHttpSession::new` 使用 `StreamableHttpClientTransportConfig`。传入的 headers 会转换为 reqwest header，并由 transport 加到请求中；实现不是 SSE-only 的旧接口，而是当前 rmcp 的 Streamable HTTP client。两种 session 都只暴露 `list_tools` 和 `call_tool`，错误统一映射为 `ToolSourceError::Transport`。

`McpServerEntry.oauth` 和 `OAuthConfig` 能被 config parser 保存、读取和传入 `McpServerDef::Http`，但 `McpHttpSession::new` 的参数只有 URL 和 headers，指定源码没有 token 获取、refresh、DCR 或 OAuth handshake。OAuth 在当前文档中应标为未在该运行时路径实现的实验性/未完成配置面，而不是可用能力。

### 4.3 `McpToolSource` 与 `McpToolAdapter`

`McpToolSource` 在 `mcp/mod.rs` 中用 enum 持有 stdio 或 HTTP session：

```text
McpSession / McpHttpSession
        └─ list_tools / call_tool
              ↓
McpToolSource::list_tools_async
        → Vec<ToolSpec>（input_schema 转 serde_json::Value）
        → output_hint = FileRefWithExcerpt
              ↓
register_mcp_tools
        → 每个 ToolSpec 一个 McpToolAdapter
        → ToolRegistryLocked
```

`McpToolAdapter` 保存工具名、从 `tools/list` 得到的 spec 和共享 `Arc<McpToolSource>`。它实现通用 `Tool`，`call` 忽略 `ToolCallContext`，把参数转发给共享 source；但 MCP call 的顶层参数必须是 JSON object。当前 stdio `session.rs` 和 HTTP `session_http.rs` 都调用 `arguments.as_object().cloned()`，非 object 值会被静默丢弃并降为 `None`，随后发送给 MCP。贡献者应把这视为参数类型边界：若产品契约要求 object，应在 adapter/source 边界返回 `ToolSourceError::InvalidInput`，并为 stdio、HTTP 各补一条测试；若保留降级语义，也必须在文档和测试中明确“非 object 等同于无 arguments”。这样 MCP 工具与 local tools 可以进入同一 registry，但 adapter 不提供 server namespace、per-call context 或独立 session。

`call_tool_async` 对 `tools/call` 的当前处理有明确的信息损失：若 `is_error` 为 true，提取 text content 并作为 `Transport` 错误；成功时只提取所有 text block，trim 后为空则报 `no text in tools/call response`；image、resource、structured content 等非 text block 不会被保留。扩展 MCP 结果时必须先决定是否要扩展 `ToolCallContent`/normalization 边界，不能只改 adapter 的字符串拼接。

由于 registry 以裸工具名为 key，连续注册不同 MCP server 的同名工具会发生覆盖；当前 adapter 没有自动改名或 server 前缀。若要支持多 server 同名工具，应在配置/adapter/registry/model-facing schema 三处同时设计命名策略，并补 collision 测试。

## 5. Skill 生命周期与模块边界

### 5.1 Discovery 与 builtin skill

`agent/skill/src/discovery.rs` 的 `SkillRegistry::discover` 按当前顺序扫描：project 的 `<working_folder>/.anureo/skills`、extra/profile dirs、`anureo_home()/skills`、`anureo_home()/data/skills`（后者 recursive），同名 skill 由先出现的 source 保留。`add_agent_skills` 追加 Agent source；`add_builtin` 只在没有同名文件系统条目时加入 Builtin source，因此 project/user skill 优先于 builtin。

`SkillEntry` 保存 metadata、base_path、SKILL.md 路径、source，以及 builtin 的 embedded content/reference files。`load_skill_with_dir` 去掉 frontmatter，只返回 body；filesystem skill 会列出同目录 support files，builtin skill 会列出编译时 embedded references。`apply_filters` 处理 enabled/disabled/platform，`apply_toolset_filters` 处理 `requires_tools`、`requires_toolsets` 和 fallback 条件。

`agent/tool/tool-basic/src/skill/mod.rs` 的 `make_skill_tools_with_registry`、`make_skill_tools_with_folder` 和 `make_skill_tools_with_skills_dir` 构造 `skill_list`/`skill_view`。`SkillListTool` 在有 registry 时使用 registry，否则从 skills directory recursive scan；`SkillViewTool` 支持 `file_path` 读取 support file，并对 filesystem path 做 canonical containment check，拒绝 path traversal。Builtin references 只能读取被 embedded 的文件。

### 5.2 持久化与 `skill_manage`

`agent/skill/src/storage.rs` 的 `SkillStorageRegistry` 以传入的 `base_dir` 为根：`Source::Auto` → `auto`，`Source::Manual` → `curated`，`Source::Evolved` → `evolved`；有 category 时保存到 `base_dir/source/category/name/SKILL.md`。`save` 使用 `atomic_write_text`（临时 sibling file、`sync_all`、rename），`list` recursive 查找 SKILL.md，并排除 hidden/underscore/`.anureo` 内部路径。

`agent/tool/tool-basic/src/skill/manage.rs` 的 `SkillManagerTool` 暴露六个 action：`create`、`patch`、`edit`、`delete`、`write_file`、`remove_file`；`action` 和 `name` 是 schema required。create/edit 接收完整 SKILL.md，patch 默认只替换一个 occurrence，`replace_all=true` 替换全部，patch 可用 `file_path` 修改 support file。write/remove file 用 `file_path`，每个 support file 上限为 1 MiB。

写入流程可概括为：

```text
skill_manage.call
  → with_write_origin(Foreground/BackgroundReview)
  → action handler
  → validate frontmatter/name/content/path
  → SkillStorageRegistry atomic write
  → optional security_scan_skill
  → failure rollback（适用时）/ usage update / discovery cache invalidation
```

`create` 要求 frontmatter 中的 name 与参数一致，并以 `Source::Auto`、`created_by: agent` 构造内容；BackgroundReview 成功后才调用 `SkillUsageStore::mark_agent_created`。Foreground 不会因传入 usage store 自动标记 agent-created。`edit`/`patch` 在成功写入后 bump patch counter；write/remove support file 也会 bump patch。

delete 在配置了 usage store 时只允许 `is_agent_created` 的 skill；`absorbed_into` 为空字符串表示 archive/prune 意图，非空值要求目标 skill 已存在且不能等于自身。BackgroundReview 或提供 `absorbed_into` 时走 archive service 路径；普通 foreground delete 且未提供该字段时走 `storage.delete`。pinned skill 由 storage 层保护，不能删除或 archive；当前 storage 也把 save/patch/write_file/remove_file 视为 pinned 的不可变面，新增写入口必须保持一致。

### 5.3 Validation 与 security scan 不是一回事

`agent/skill/src/validation.rs` 是内存内容验证：frontmatter 必须存在且关闭，必须有 string 类型的 `name`/`description` 和非空 body；name 最长 64 字符、只允许 lowercase ASCII alphanumeric、`-`、`_`、`.`，且首字符必须为 alphanumeric。body 超过 100 KiB 是 Critical；危险 shell/code pattern 是 Critical；prompt-injection pattern 和 script/javascript URI 是 Warning。

`validate_skill_path` 拒绝 `..`、absolute path、backslash，并只允许 `skills`、`references`、`templates`、`scripts`、`assets` 这些顶层目录；单段 executable extension（例如 `.sh`、`.exe`、`.ps1`、`.py`）也拒绝。新增 support-file action 必须调用它。

`agent/skill/src/security.rs` 是写入后的目录级 guard wrapper。默认 `guard_agent_created` 为 false；启用后调用 `guard::scan_skill` 和 `should_allow_install`，危险结果返回错误。遗留环境变量 `SKILLS_GUARD_AGENT_CREATED` 可覆盖传入值，但源码明确记录它已 deprecated，应优先使用 `config.toml [skills] guard_agent_created`。scan 失败时 wrapper 会记录 warning 并返回 Ok，不能把“scan 未完成”误写成“已通过安全审查”。

## 6. 扩展点与实现建议

### 6.1 第一次小修改：为 MCP 参数增加 object 校验

适合首次贡献者的最小闭环，是只在 `agent/tool/tool-basic/src/mcp/mod.rs` 的 `McpToolSource::call_tool_async` 边界增加一个纯参数校验分支：接受 JSON object，遇到 string、array、number、boolean 或 null 返回 `ToolSourceError::InvalidInput`。这个改动不启用 `allow_outside`，不运行外部 MCP server，也不执行 shell、delete 或 archive。

建议把校验提取为同文件的纯函数，并在该文件现有 `#[cfg(test)] mod tests` 增加非 object/ object 两个 unit test；随后再在 `session.rs` 和 `session_http.rs` 各补一条调用边界测试，确认两种 transport 都不会把错误值静默转换为 `None`。最小验证命令（从 anureo workspace 根目录运行）是：

```powershell
cd C:\Users\<user>\dev\anureo
cargo fmt --check
cargo test -p tool-basic mcp
cargo test -p tool-basic --lib
cargo check -p tool-basic
git diff --check
git diff -- agent/tool/tool-basic/src/mcp/mod.rs agent/tool/tool-basic/src/mcp/session.rs agent/tool/tool-basic/src/mcp/session_http.rs
```

预期结果是 formatter、精确的 MCP unit tests、`tool-basic` library tests 和 crate check 均成功，且 `git diff` 只包含参数校验及其测试。若 Cargo 因 `target` 目录无写权限失败，应修复目录权限，或先设置一个可写的 `CARGO_TARGET_DIR` 后重跑同一组命令；不要用权限问题掩盖尚未编译的测试失败。完成后检查 `git status --short`，确认没有生成或纳入无关文件。

### 新增一个 local Tool

1. 在拥有该副作用的 `tool-basic` 子模块实现 `Tool`，完整提供 `name`、schema、参数验证、错误映射和必要的 `ToolCallContext` 使用。
2. 在 `agent/tool/tool-basic/src/lib.rs` re-export 或提供 `register_*_tools`，使用 `register_async`；只有同步边界才使用 `register_sync`。
3. 如果工具需要 prompt guidance，返回 `BuiltinSkill`，并声明 `requires_tools`，使 discovery 的 `apply_toolset_filters` 能在依赖工具不存在时隐藏它。
4. 为成功、参数错误、底层 transport 错误、取消/超时和 output normalization 补测试。

### 接入 MCP server/tool

先在 `mcp_config.rs` 增加或调整纯配置模型和解析测试，再在 `tool-basic` 选择 `McpToolSource::new`/`new_with_env`（stdio）或 `new_http`（HTTP），最后调用 `register_mcp_tools`。不要在 `McpToolAdapter` 中重复实现 transport；它的职责只是把一个 MCP tool 转成通用 `Tool`。

如果需要 headers、认证、namespace、非文本结果或 reconnect，应先扩展 session/source 的数据模型和生命周期，再更新 adapter 与 registry 测试。仅把字段加入 `McpServerEntry` 不会让运行时自动生效。

### 新增 Skill 能力

读取/发现能力放在 `agent/skill/src/discovery.rs` 和 `tool-basic/src/skill/list.rs`/`view.rs`；持久化放在 `storage.rs`；输入规则放在 `validation.rs`；外部/agent-created 扫描放在 `guard.rs`/`security.rs`；usage/lifecycle 放在 `usage.rs`。`skill_manage` 只做 tool-facing orchestration，不应把 storage CRUD、路径安全和 frontmatter 解析重新复制一份。

新增 write action 时至少保证：路径 validation、1 MiB support-file 上限（如适用）、atomic write、pinned gate、security scan/rollback、usage bump 和 discovery cache invalidation 都有明确策略。若 action 需要区分 foreground/background，使用现有 `for_foreground`/`for_background_review` 与 `with_write_origin` 边界，不要用隐式全局变量判断。

## 7. 测试与验证

先满足这些前置条件：从 `C:\Users\<user>\dev\anureo`（或 Unix 等价的 anureo workspace 根目录）运行；安装可用的 Rust/Cargo toolchain；确保 workspace 和 `target` 目录可写。若 target 无写权限，修复目录权限或选择可写的 `CARGO_TARGET_DIR` 后再重跑。然后按改动范围运行以下检查，并把 `cargo fmt --check` 作为基础检查；package 名和 test target 应以当前 workspace manifest 为准：

```powershell
cargo fmt --check
cargo test -p tool-basic
cargo test -p tool-basic --test register_file_tools_origin
cargo test -p skill
cargo test -p config
cargo check --workspace
```

重点测试面：

| 改动 | 必须覆盖的事实 |
| --- | --- |
| Tool/registry | register 覆盖同名工具、list/call filter 分离、dry-run 不执行、显式/隐式 context、ToolSourceError 映射 |
| 文件工具 | working folder canonicalize、`..`/symlink containment、`allow_outside`、UTF-8 长行、patch Add/Update/Delete/Move、取消/超时 |
| MCP config/session | disabled、override/project/global 路径优先级、url 优先 command、非法 URL/空 command、stdio spawn/initialize、HTTP headers、非 text 或 `is_error` response |
| MCP adapter | `tools/list` spec 映射、共享 source、同名工具 collision、call 参数与错误转发 |
| Skill discovery | project/profile/user/data 顺序、同名 first-wins、builtin precedence、toolset/platform filters、references 和 path traversal |
| Skill manage | frontmatter/name mismatch、category、patch unique/replace-all、rollback、pinned、delete authorization、support-file path/size、cache invalidation、usage origin |
| Security | `guard_agent_created` 默认关闭、config/env override、Critical/High block、clean skill、scan panic/failure 行为 |

用户指定测试已经覆盖若干关键契约：`register_file_tools_origin.rs` 验证 BackgroundReview 会标记 agent-created、Foreground 不会标记，并验证 `skill_manage` 被注册；`manage/tests/frontmatter.rs` 验证 frontmatter 基本成功和缺失/未闭合错误；`manage/tests/coverage.rs` 覆盖 category、edit/patch rollback、path traversal、support-file、usage、security scan 和 response preview。修改这些契约时应先扩展对应测试，再运行 crate 与 workspace 检查。

## 8. 常见坑与当前未完成面

- 把 `agent/tool/tool-basic/src/mod.rs` 当成现有入口：当前没有该文件；入口是 `tool-basic/src/lib.rs` 及其公开子模块。
- 以为 MCP config 的 `oauth` 已可用：当前 parser 能保留字段，但 `McpHttpSession` 只消费 URL/headers，没有 OAuth 流程。
- 以为 MCP 保留完整 `CallToolResult`：当前 source 只保留 text block，image/resource/structured content 会丢失，空 text 会变成 transport error。
- 以为每个 MCP server 有独立命名空间：adapter 使用裸工具名，registry 的 HashMap 会覆盖同名工具。
- 以为 `tools/list` 会自动刷新：adapter 保存注册时的 spec；当前指定源码没有 refresh/invalidation 协议。
- 把 `ToolRegistry` 最近一次 context 当成隔离上下文；无显式 context 的调用可能读取共享 context。
- 在已有 Tokio runtime 中调用 `register_sync`；它会在线程内创建 current-thread runtime，但调用线程仍会同步 `join`，会阻塞 async worker。应使用 `register_async`，避免额外线程/runtime 及 join 行为。
- 把 `allow_outside` 当成普通便利选项；它会跳过文件 containment check，必须显式审查。
- 把 Bash timeout 当成 kill：当前超时会 detach 子进程并保留输出文件。
- 认为 `encoding` schema 让 `read` 支持任意编码；实现使用 UTF-8 `read_to_string`。
- 只做 validation 就认为 skill 安全；content validation 与可选目录 guard 是不同阶段，且 guard 默认关闭。
- 认为 `skill_manage` 的所有错误都会以 `Err` 返回；action handler 的业务失败通常是 `ToolCallContent` 中的 `{success:false,error:...}` JSON，缺少字段或未知 action 才是 `ToolSourceError::InvalidInput`。
- 直接写 support file 而跳过 `validate_skill_path`、pinned、scan rollback 或 cache invalidation；这会破坏 Skill 的安全和发现契约。
- 把当前 YAML tool specs 当成只覆盖 description 的配置面；实现会用同名 YAML `ToolSpec` 整体替换 `list` 输出，`TOOL_YAML_FILES` 为空，现阶段没有内置 YAML spec。
- 把 `SkillUsageStore` 的 agent-created 标记当成所有来源的 provenance；其实现会排除 bundled/hub-owned 记录，并且 Foreground `skill_manage` 不自动标记。
- 忽略项目安全指南：MCP 的 command、URL、headers、env 都是需要审查的外部影响面；file write、shell、network、delete 都应在明确工作目录和授权范围后执行。

## 9. 最小贡献流程

1. 从 `tool-core/src/lib.rs`、`tool-basic/src/lib.rs` 或 `skill/src/lib.rs` 确认 owning boundary 和公开 re-export。
2. 沿 `config → session/source → adapter → registry → Tool::call`，或 `skill_manage → validation → storage → security/usage/cache` 追踪一条 happy path 和至少一条失败 path。
3. 先在最靠近实现的 crate 补 unit/integration test，再接入 CLI/Agent wiring；不要只验证 schema 能生成。
4. 对 MCP 明确 transport、命名、lifecycle 和 result fidelity；对 Skill 明确 provenance、pinned、rollback 和 cache invalidation。
5. 运行相关 `cargo test`，再运行 `cargo check --workspace`；涉及跨 crate 注册时补 `register_file_tools_origin` 类 integration test。
6. 修改后重新核对文档中的路径、命令、配置项和实验性标签，尤其是 OAuth、非文本 MCP result、YAML specs 与 guard 默认值。

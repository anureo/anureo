# Plugin 与 Hook 系统实现方案

> **状态**: 提案（未立项，待评审）；v2 = 自查修订版（修正 5 处事实错误，补多宿主安全/子代理传播/后台 turn 策略，见 §10 修订记录）。代码级展开见 [plugin-hook-code-plan.md](plugin-hook-code-plan.md)（其中 drain 注入点已修订为消息侧注入，以代码方案为准）
> **范围**: anureo agent 侧运行时（`agent/hook-core`、`agent/agent-core`、`apps/acp`）；OpenChamber/cowork 前端 UI 不在本方案内（另立前端分册）
> **交叉参考**: [codex 架构分析 §5/§12](../analysis/codex-architecture-20260906.md)、[acp-spec 23-plugin](../acp-spec/extensions/23-plugin.md)、[acp-spec 20-skills](../acp-spec/extensions/20-skills.md)、[ACP 通信审查](../dev/acp/04-communication-reasonableness.md)

---

## 1. 背景与目标

codex 的用户级扩展面 = hooks（用户可配置生命周期处理器）+ plugins（技能/模板/MCP bundle）+ skills（已有）+ connectors/memories。anureo 目前只有 skills；hooks 无对应物；plugin 只有 RPC 管理面的一半（配置 CRUD），没有运行时。

**目标**：

1. **Hook**：统一的生命周期事件发射点 + 用户可配置处理器（命令执行），解决 codex 分析指出的「生命周期钩子无统一发射点」差距；
2. **Plugin**：manifest 声明的资源包（skills / MCP servers / commands / agent profiles / hooks），一键安装启用，由各子系统装配；
3. **对齐 spec 23**：把 plugin RPC 面从草案推进到实现，并修正 spec 与现有 `plugin.rs` 的漂移。

**非目标**（见 §9）：进程内 JS 执行引擎、connectors、memories、npm 之外的 registry、前端 UI、hook 专属 RPC 域。

---

## 2. 现状盘点

| 组件 | 现状 | 差距 |
|---|---|---|
| `docs/acp-spec/extensions/23-plugin.md` | install/uninstall/enable/disable + `plugin/changed` + progress 草案 | 标注「未实现」；与实际 `plugin.rs` 方法集漂移（spec 无 `entry_*`/`file_*`/`registry`） |
| `apps/acp/src/extensions/plugin.rs` | 已实现：`entry_*`（config 层 `plugin` 键 CRUD）、`file_*`（`~/.config/anureo/plugins/*.js|ts` 与 `<wd>/.anureo/plugins/` 文件 CRUD）、`registry`（npmjs.org 元数据查询，1h 缓存） | **无运行时消费方**——插件 JS 文件全仓无人加载执行；npm 条目无人解析安装 |
| Hook 系统 | 无 | goal runner（`agent/goal/src/runtime.rs` 的 `on_session_idle→continue_if_idle`）、auto_review 是硬编码特判；用户不可配置任何 hook |
| Turn 咽喉点 | 所有 turn（含 goal 续跑，经 `AcpTurnDriver::start_turn_if_idle`）都汇入 `AnureoAcpAgent::prompt_with_capabilities` + `begin_prompt`/`PromptGuard`（`apps/acp/src/agent.rs`；`prompt_executor.rs` 仅为黑盒测试注入边界） | 单一咽喉点 = `turn/start|end` 只需埋一处 |
| Session lifecycle | `agent.rs` 会话 lifecycle 状态机（`set_lifecycle("idle")` 两处）+ curator 以 idle_for_seconds 机会性运行 | 通用 idle 转换点存在，但无通用 idle **事件广播**（loom 无 codex 的 `ThreadIdleCause` 概念） |
| `GoalHandler::bind` + `goal_runtime_for` | per-thread runtime attach 先例（`apps/acp/src/extensions/goal.rs:372`） | 可作为 hook dispatcher 的接线模板 |
| `RunOptions.any_stream_event_sender` | 事件桥先例（`agent/agent-core/src/run/types.rs:65`），且已铺进 `act_executor.rs`（`ToolCallExecutor` 字段） | hook dispatcher 沿用同一注入模式，工具层布线成本极低 |
| `CapabilityManager`（`apps/acp/src/extensions/capability.rs`） | snapshot 容器 + `capability_changed` 支持声明 | 具体通知发射点需在 P1 接线时核实/补齐 |
| skills 安装 | `skills/install` 有回滚语义（spec 20） | plugin install 复用同一语义 |

---

## 3. 总体设计

### 3.1 概念模型

```
用户配置 / Plugin manifest
        │
        ▼
┌──────────────────┐   订阅者两类：
│  HookDispatcher  │──► Command 订阅者（argv 进程，用户/plugin 声明）
│ (agent/hook-core) │──► Builtin 订阅者（Rust fn，goal/auto_review 渐进迁入）
└────────┬─────────┘
         │ 事件发射（apps/acp + agent-core 埋点）
   ┌─────┴──────────────────────────────┐
   │ session/start │ turn/start │ turn/end │ tool/pre │ tool/post │ compact/* │ thread/idle
   └────────────────────────────────────┘
```

**关键决策**：

| # | 决策 | 理由 |
|---|---|---|
| D1 | hook 处理器 = **argv 数组直接 spawn（不经 shell）** | 消除注入面。注：Windows 下 `CreateProcess` 只解析 `.exe`，`node`/`bun.exe` 可用，npm `.cmd` shim **不可**直接 spawn——文档须明示 |
| D2 | 订阅者双形态：`Command`（用户/plugin）+ `Builtin`（Rust） | dispatcher 即 contributor 模式的最小形态；内置扩展未来走同一通道而非散落特判；**不抄** codex 12 个细粒度 trait 全集 |
| D3 | 结果在 **turn 边界排空**（drain）为主；`tool/pre` 是唯一同步 gate | 对齐 codex「hook 结果在 turn 边界排空」，不阻塞采样循环；pre-gate 是 block 场景刚需，需同步等待（带超时） |
| D4 | Plugin = **纯声明式资源包**，无任意代码执行（P0–P2） | anureo 无 JS runtime；plugin 引用的脚本只作为 hook 的 Command 动作由用户 runtime（node/bun）执行 |
| D5 | plugin 启用状态复用 config 层 `plugin` entries（`plugin.rs` 已写入），不另设 `installed.json` | 单一事实源，entry CRUD 管理面直接复用。**代价**：entries 获得运行时含义后，其 RPC 写操作必须加门禁（见 §6.1） |
| D6 | 安装目录沿 `plugin.rs` 现有布局：user = `~/.config/anureo/plugins/<id>/`，project = `<wd>/.anureo/plugins/<id>/` | 与 `file_*` 方法一致，零迁移 |
| D7 | hook **不开新 RPC 域**：配置走既有 config 面，执行可见性走 diagnostics + session/update | P0 零新协议面；未来确需 hook/test 再立 spec 39 |
| D8 | 默认**仅顶层会话**发射；子 agent 不传播（opt-in `include_subagents`） | 子 agent 数量放大 hook 触发；`config_builder` 的 config_template 会向子代传播 mcp 等资源，dispatcher 必须**显式排除**出传播集 |

### 3.2 与 codex 机制对照

| codex | 本方案 | 说明 |
|---|---|---|
| hooks（config 可配置，turn 边界排空） | P0 同构 | 事件面子集起步 |
| plugins（外部技能/模板/MCP bundle + 推荐） | P1/P2 | 推荐（`RecommendedPluginCandidates`）不做 |
| extension-api 12 contributor traits | D2 的 Builtin 订阅者 | 最小通道，按需扩 trait |
| McpServerContributor | plugin manifest 声明 MCP + `config_builder` 合并 | 不做运行时动态贡献 |

---

## 4. Hook 系统设计

### 4.1 事件面（P0 全集）

| 事件 | 发射点 | 时机 | 同步/异步 |
|---|---|---|---|
| `session/start` | `agent.rs` 会话创建后（thread 建好、首个 prompt 前） | 不得阻塞 session/new 响应 | 异步 |
| `turn/start` | `prompt_with_capabilities` 内、`begin_prompt` 之后、runner 启动前 | 所有 turn 单一咽喉点（用户 prompt、goal 续跑、multi_run 均经此） | 异步 + drain 排空 |
| `turn/end` | react runner run 结束（completed / error / interrupted） | 含 cause | 异步，结果 drain 进下一轮 |
| `tool/pre` | `agent/agent-core/src/agent/react/act_executor.rs` 调工具前（`ToolCallExecutor` 已持有 `any_stream_event_sender` 同款布线位） | **可 deny** | **同步**（超时内等待） |
| `tool/post` | `ToolCallExecutor` 工具完成后 | 可注入 additional_context | 异步 |
| `compact/before` / `compact/after` | `agent/agent-core/src/compress/compact_node.rs` | 压缩前后 | 异步 |
| `thread/idle` | `agent.rs` 会话 lifecycle 置 `idle` 处（`set_lifecycle("idle")` 两处） | loom 无 codex `ThreadIdleCause`；载荷自带轻量 cause（`turn_end` / `interrupt` / `startup`） | 异步 |

**后台 turn 策略**：goal 续跑 / auto-review / curator 自动 spawn 的会话同样经过 turn 咽喉点，默认**照常发射**；载荷带 `turn.cause`（`user` / `goal-continuation` / `auto-review` / `curator`），hook 作者按需过滤。**防环**：hook 注入的上下文不计入会话活动（不重置 idle 时钟、不触发 curator gate——`idle_for_seconds` 语义不变）。

后续候选（不在 P0）：`turn/input`（steering/排队输入注入，需先确认 prompt queue 语义）、`session/end`。

### 4.2 HookDefinition 与配置格式

配置放 **config_store 的 JSON/JSONC 层**（与 `plugin` entries 同体系；注意与 `{anureo_home}/config.toml`（foundation/config，providers 专用）是两套，hooks **不进** config.toml）。层与路径（`read_config_layers` 实际候选）：

- user：`~/.config/anureo/config.json` | `anureo.json` | `anureo.jsonc`
- project：`<wd>/anureo.json(c)` 或 `<wd>/.anureo/anureo.json(c)`
- custom：`ANUREO_CONFIG` 环境变量指向的文件

`config.json` 示例：

```json
{
  "hook": [
    {
      "name": "format-after-edit",
      "events": ["tool/post"],
      "match": { "tool": "^(write_file|edit_file|multiedit)$" },
      "command": ["bun", "~/scripts/format.js"],
      "timeout_ms": 8000,
      "on_failure": "warn",
      "include_subagents": false
    }
  ]
}
```

| 字段 | 必填 | 说明 |
|---|---|---|
| `name` | 是 | 层内唯一；合并优先级 user < project（同 project 内同名取先声明，warn） |
| `events` | 是 | §4.1 事件名数组 |
| `match` | 否 | regex 过滤（`tool` 键匹配工具名） |
| `command` | 是（Command 形态） | argv 数组，`~`/`{pluginDir}` 占位展开 |
| `timeout_ms` | 否 | 默认 10000 |
| `on_failure` | 否 | `ignore` / `warn`（默认）/ `block`（仅 tool/pre 有意义） |
| `include_subagents` | 否 | 默认 false（D8） |

**校验失败行为**：单条 hook 解析/校验失败（未知事件、坏 regex、空 command）→ **跳过该条 + `tracing::warn` + 计入 diagnostics**，不 fail-fast——误配不能拖死 agent（与 `config_builder.rs:238` MCP 配置加载失败的 warn-and-continue 语义一致）。

scope 不手写：由所在配置层决定（运行时标注 `user` / `project` / `plugin:<id>`，修正现有 `scope_of_entry` 启发式）。

Plugin 声明的 hook 同构，**并入 `plugin.json` 的 `hooks` 数组**（不设独立 hooks.toml，单一 manifest）。

### 4.3 执行模型

- **同步 gate（仅 `tool/pre`）**：dispatch → 并发执行所有匹配 hook（上限 4），任一返回 `block` 即拒绝工具调用，其余结果丢弃；全部超时/失败按 `on_failure`。拒绝走**现有工具执行错误路径**（`ToolResult` + `act_utils::DEFAULT_EXECUTION_ERROR_TEMPLATE`），`reason` 透传给模型——不发明新的错误类型。
- **异步 + drain（其余事件）**：fire-and-forget 发射，执行结果（stdout 解析产物/超时/失败标记）入 per-thread `HookDrainQueue`；在 `turn/start` 排空注入。注入点为**消息侧**（`prompt_with_capabilities` 构建用户消息时头部拼接，`hook_core::prepend_context`），不用 system prompt——AgentCache 按 `(agent_id, model, working_folder)` 复用 handle，system prompt 组装时机不保证逐 turn；消息侧注入与 goal 续跑注入同构、逐 turn 必达。顺序按发射序。
- **护栏**：每事件并发上限（默认 4）、全局并发上限（默认 8）、超时默认 10s；`tracing` 记录每次执行的耗时/退出码/是否产出上下文，不记载荷全文（防 secret 落盘）。

### 4.4 crate 与接线

新 crate `agent/hook-core`（依赖最小化：tokio、serde_json、regex；不依赖 agent-core/llm）：

```
hook-core/src/
  event.rs        // HookEvent 枚举 + 载荷类型（含 turn.cause）
  definition.rs   // HookDefinition（Command | Builtin）、match 规则、解析校验
  registry.rs     // 事件 → 订阅者索引；user/project/plugin 层合并
  executor.rs     // argv spawn（tokio::process）、超时、并发闸、~/{pluginDir} 展开
  outcome.rs      // HookOutcome（allow/block/additional_context/user_message）、stdout JSON 协议
  drain.rs        // per-thread HookDrainQueue
```

接线（沿 `any_stream_event_sender` 先例）：

1. `RunOptions` 增 `hook_dispatcher: Option<Arc<HookDispatcher>>`；
2. `apps/acp` 启动时：`HookConfigLoader` 经 `read_config_layers` 读 `hook` 段 → 构建 dispatcher → 注入 RunOptions 与 session 工厂；
3. 发射点只增不改现有语义：dispatcher 为 `None` 时零开销（`if let Some` 早退），无配置时行为完全不变；
4. **子 agent 排除**（D8）：`config_builder` 构建子 agent RunOptions 时不复制 dispatcher（除非 `include_subagents`）；
5. goal/auto_review **暂不迁移**（避免回归），Builtin 订阅者通道先空跑验证。

### 4.5 载荷与结果协议

stdin（单行 JSON）：

```json
{"event":"tool/pre","sessionId":"...","threadId":"...","cwd":"...",
 "turn":{"cause":"user"},
 "tool":{"name":"write_file","params":{"path":"..."}}}
```

stdout 协议（空输出 = allow，仅记录）：

```json
{"decision":"block","reason":"path outside allowlist"}
{"additional_context":"formatted 3 files","user_message":"hook: format-after-edit ran"}
```

非 JSON / 非 0 退出 → 按 `on_failure`；`decision:"block"` 仅 `tool/pre` 有效（其他事件忽略并 warn）。

---

## 5. Plugin 系统设计

### 5.1 manifest 与目录布局

```
~/.config/anureo/plugins/<id>/          # 或 <wd>/.anureo/plugins/<id>/
  plugin.json          # manifest（必须，含 hooks 声明）
  skills/<name>/...    # 可选：SKILL.md 包
  commands/<name>.md   # 可选：斜杠命令/snippet
  agents/<name>.yaml   # 可选：agent profile
  scripts/...          # 可选：hook 引用的脚本（argv 以 {pluginDir} 引用）
```

`plugin.json`：

```json
{
  "id": "git-enhanced",
  "name": "Git Enhanced",
  "version": "1.2.0",
  "description": "PR 模板与同步命令",
  "author": "anureo",
  "mcpServers": [{ "name": "git-enhanced-server", "command": ["node", "{pluginDir}/server.js"] }],
  "hooks": [ { "name": "pre-commit-check", "events": ["tool/pre"], "command": ["node", "{pluginDir}/scripts/check.js"] } ]
}
```

`capabilities` 由 server 校验 manifest 后回填（同 spec 23 `items[].capabilities` 语义，枚举扩展为 `skill` / `command` / `hook` / `mcp` / `agent-profile`）。

### 5.2 生命周期与状态

- **启用状态** = config 层 `plugin` entries（D5）：出现即启用；entry 为 `{"enabled": false}` 对象 = 已安装未启用（`entry_update` 扩展支持）。
- **install(source)**：`path`（校验在两个 plugins 目录域内）= 复制进 plugins 目录（Windows 无符号链接特权问题，v1 只做复制）+ 校验 manifest + 写 entry；`npm`（P2）= tarball 下载 → integrity 校验 → 解压 → 同 path 流程。失败回滚（对齐 `skills/install`）。
- **uninstall(cleanup=true)**：移除 entry → 停用各域资源 → 删目录；partial failure 按 spec 23 报告 `cleanup.*.failed`（`hooks`/`mcpServers`/`commands` 三类，skills 随目录删除天然清理）。
- **enable/disable**：只改 entry + 重建 dispatcher/注册表，不动物理目录。

### 5.3 装配点（各子系统消费）

| 资源 | 装配点 | 冲突规则 |
|---|---|---|
| skills | `agent/skill/src/discovery.rs::SkillDiscovery` 增 `SkillSource::Plugin` 源（沿 `add_agent_skills()` 追加目录先例）扫描已启用 plugin 的 `skills/` | 用户/内置 skill 同名优先；plugin 间同名取后启用者并 warn |
| MCP servers | `agent/agent-core/src/run/config_builder.rs` 合并序列（现为 `discover_mcp_config_path` 的 mcp.json → `base.mcp_servers`，再 ACP 注入覆盖同名）插入第三来源：plugin 声明以**低于 mcp.json** 的优先级并入 | 显式 ACP > mcp.json > plugin；两 plugin 同名 → plugin 置 error 状态 |
| commands | `apps/acp/src/extensions/command.rs` 发现路径追加（`COMMAND_DIR_NAME` 体系） | 同 skills |
| agent profiles | `.anureo/agents` 发现路径追加（`AGENT_DIR_NAME` 体系） | 同 skills |
| hooks | manifest `hooks` 并入 HookRegistry（§4.4），scope = `plugin:<id>` | 同名 hook：user > project > plugin |

每次 install/enable/disable/uninstall 后：发 `plugin/changed` + `capability_changed`（`CapabilityManager` 快照更新 + 通知发射点 P1 核实补齐）；涉及 command 时触发 `available_commands_update`（spec 23 已定）。

### 5.4 RPC 面（spec 23 修订）

`plugin.rs` 保留现有 `entry_*`/`file_*`/`registry`（配置与本地开发面），新增生命周期方法对齐 spec：

| 方法 | 期 | 说明 |
|---|---|---|
| `plugin/list` | P1 | 重载现有 list：合并 config entries + plugins 目录扫描，返回 spec 23 `PluginItem` 形状 |
| `plugin/install` / `uninstall` | P1(path) / P2(npm) | §5.2；写操作过 §6.1 门禁 |
| `plugin/enable` / `disable` | P1 | §5.2；同上门禁 |
| `plugin/progress` notification | P2 | npm 安装进度 |

**spec 23 同步修订**（独立小 PR，须过 `docs/dev/acp/04-communication-reasonableness.md` 审查清单）：实现状态推进；`entry_*`/`file_*`/`registry` 补录；`autoEnable` 默认值从 `true` 改 `false`；`capabilities` 枚举补 `skill`/`agent-profile`；补 §6.1 的授权要求为 normative。

**Hook 无独立 RPC 域**（D7）：配置管理走既有 config 面；hook 执行可见性（最近执行/失败）走 diagnostics 域与 session/update 流。

---

## 6. 安全模型

### 6.1 多宿主授权（cowork 嵌入场景，P1 硬性要求）

现状风险：`plugin/entry_create` 等 RPC **现在就能写 config 的 `plugin` 段**；P1 起 entries 具备运行时含义（hook command = 任意 argv 执行），未加门禁即构成**远程代码执行面**。

1. hook/plugin 的全部写操作（`entry_*` 写路径、`install`/`uninstall`/`enable`/`disable`、config 面 `hook` 段写入）须过 **server-side 授权**：本地 stdio 单用户模式放行；多宿主（cowork）模式限 admin principal（对照 `goal.rs resolve_thread_key` 的 `owner_principal` 校验先例，具体门禁形态实现时对齐 server 的 principal 体系）。
2. **信任边界**：hook 配置只来自 server 本机文件系统的 config 层与 plugins 目录；不接受客户端经 RPC 直接提交 hook 定义。
3. 本地单用户语义不变：config 本来就是用户自己写的，hook 与「用户自己配 mcp command」同级权限。

### 6.2 执行与资源

1. **Hook 命令 = 用户同等权限**：不提权、不降权；argv 直 spawn 不经 shell（D1）；Windows 注意 `.cmd` shim 不可 spawn（用 `node.exe`/`bun.exe` 或脚本绝对路径作 argv[0]，文档明示）；`~`/`{pluginDir}` 仅展开为路径占位。
2. **`tool/pre` block 是本地可编程 gate，不替代 ACP `requestPermission` 人审**：hook 先跑（快、可编程），permission 后问（人审），叠加生效。
3. **Plugin 安装是供应链信任决策**：`autoEnable` 默认 `false`；install 响应回显 capabilities；npm 包 P2 起 integrity 校验；path source 限制在两个 plugins 目录域内。
4. **载荷最小化**：stdin 不含 API key/token；诊断日志不落载荷全文。
5. **资源护栏**：§4.3 并发与超时上限；hook 总耗时计入 turn 指标（`stats/collector`），超阈值 warn；hook 注入不重置 idle/不触发 curator（§4.1 防环）。

---

## 7. 分期实施计划

### P0 — Hook 运行时（约 5–8 人日）

1. `agent/hook-core` crate：类型/registry/executor/drain + 单测（超时、失败策略、stdout 解析、`~`/`{pluginDir}` 展开、并发闸、Windows spawn）；
2. `RunOptions.hook_dispatcher` seam + 发射点：`turn/start|end`（`prompt_with_capabilities` 咽喉点 + react runner）、`tool/pre|post`（`ToolCallExecutor`）、`compact/*`、`session/start`、`thread/idle`（`set_lifecycle("idle")` 处）；
3. config 层 `hook` 段加载（JSON 三层合并、单条校验失败跳过 + warn）；
4. 子 agent 排除逻辑（D8）+ 后台 turn cause 载荷；
5. 集成测试：drain 顺序、tool/pre block 走现有错误模板、无配置零变化（回归基线）、hook 注入不影响 idle/curator 语义；
6. 本方案 §4 即事件面契约（D7 不立新 spec）；如引入 RPC 面再过通信审查清单。

**验收**：声明 `tool/pre` block hook 后，命中 match 的工具调用被拒且模型可见 reason；`tool/post` 的 `additional_context` 在下一 turn 经 prompt_assembly 注入；hook 超时不阻塞 turn；无 hooks 配置时全量既有测试绿。

### P1 — Plugin 本地装载（约 1 周）

1. manifest schema + 校验器（含错误定位）；
2. `PluginHost`：扫描两个 plugins 目录 + config entries → 有效资源集；
3. §5.3 五个装配点接线 + 冲突规则；
4. RPC：`install(path)` / `uninstall` / `enable` / `disable` / `list` 重载 + `plugin/changed` / `capability_changed`（发射点核实补齐）+ **§6.1 授权门禁**；
5. `file_*` 保持纯文件管理语义（插件素材/脚本，被 hooks 声明引用后生效）；「散置 JS 文件自动注册 hook」**不做**（解释器探测是范围蔓延，v2 修订降级）；
6. 测试：装配快照测试、partial failure、uninstall cleanup 报告、未授权写操作被拒。

**验收**：demo plugin（1 skill + 1 command + 1 hook + 1 MCP server）install → list 显示 capabilities 与 state=active → 四类资源会话可用；disable → `capability_changed` 且资源消失；uninstall → cleanup 报告完整；非 admin principal 调写操作被拒（多宿主模式）。

### P2 — npm 分发（约 3–4 人日）

`registry` 元数据已有 → tarball 下载/integrity 校验/解压/版本选择/`plugin/progress`；同 id 重装 = 升级（conflict 语义按 spec 23）。

### P3 — 可选，明确暂不启动

进程内 JS 插件 API（deno_core/boa 评估）或 cowork 宿主桥；`turn/input` steering hook；散置文件隐式 hook；plugin 推荐机制。

---

## 8. 测试策略

- **hook-core 单测**：executor 行为矩阵（exit code × stdout 形态 × on_failure）；drain 顺序与容量上限；Windows/Unix spawn 差异。
- **集成**（`cargo nextest`，沿 `.nextest.toml`）：acp 层 fake session 驱动 turn，断言 session/update 流中 hook 产出的注入项与拒绝项；goal 续跑 turn 的 cause 载荷；idle/curator 语义不受 hook 注入扰动。
- **装配快照**：PluginHost 对固定 fixtures 目录的有效资源集快照（可引入 insta，见 codex 分析 §13 借鉴清单）。
- **回归基线**：`hook_dispatcher = None` 全路径行为不变的既有测试全绿。

---

## 9. 风险与明确不做

| 项 | 处置 |
|---|---|
| hook 误配拖慢 turn | 默认超时 + on_failure=warn + turn 指标告警；`tool/pre` 失败默认放行（fail-open），`block` 需显式声明 |
| **entries 获运行时含义后的 RCE 面** | §6.1 授权门禁为 P1 硬性验收项；门禁未落地前 entries 不参与运行时装配 |
| hook 与续跑/curator 成环 | `turn.cause` 载荷 + 注入不重置 idle + 集成测试锚定（§8） |
| 子 agent 放大触发 | D8 默认不传播，`include_subagents` opt-in |
| Windows spawn 差异 | D1 注记 + 单测 + 文档指引（.cmd shim 不可用） |
| dispatcher 发射点遗漏/顺序错 | 事件面契约即本方案 §4 + 集成测试锚定；发射点集中在 6 处，改动需过 review 清单 |
| plugin 与用户资源同名冲突 | §5.3 统一规则（显式配置优先），冲突即 warn/error 状态可见 |
| 明确不做 | 进程内 JS runtime；connectors/memories（各自另案）；codex 式 12 contributor trait 全集；hook 专属 RPC 域（D7）；npm 之外 registry |

---

## 10. 修订记录

- **v2（自查修订）**：修正 v1 五处事实错误——①config 层是 JSON/JSONC 非 TOML（user=`~/.config/anureo/config.json` 系、project=`anureo.json(c)` 系）；②loom 无 `ThreadIdleCause`（codex-rs 概念），idle 发射点改挂 `set_lifecycle("idle")`；③turn 咽喉点是 `prompt_with_capabilities`（`prompt_executor.rs` 是测试边界）；④MCP 装配点在 `config_builder.rs` 合并序列而非 apps/acp 配置装载；⑤工具拒绝走 `ToolResult`/现有错误模板而非虚构的 `ToolCallError`。新增：§6.1 多宿主授权（RCE 面）、D8 子 agent 排除、§4.1 后台 turn cause 与防环、D7 不开 hook RPC 域、drain 注入落点 prompt_assembly、P0 工作量上调 5–8 人日、`file_*` 隐式 hook 降级不做。

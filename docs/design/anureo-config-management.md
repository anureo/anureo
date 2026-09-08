# anureo 配置管理面 ACP 化方案

> **状态**: Proposal-v4（待评审，尚未开发）
> **范围**: anureo `apps/acp` 配置管理域补持久层；消灭 anureo Desk 前端剩余管理面 REST 调用（anureo Desk `docs/acp/13-rest-to-acp-api-migration.md` §6）
> **日期**: 2026-08-19
> **相关引用**: anureo `docs/acp-spec/extensions/`（wire 契约）、anureo Desk 13 号文档

---

## 1. 现状

wire 层已完整：agent/command/skill/snippet/plugin/mcp 六域的 CRUD、scope（Global/Project）、`changed` 通知、clientRequestId 幂等、generation 游标、`check_server_policy` 鉴权均已实现（`apps/acp/src/extensions/{agent_profile,command,snippet,skills,plugin,mcp}.rs`）。

持久层已有先例（JSON 落盘，project 优先、`~/.anureo/` 兜底）：

```
<wd>/.anureo/scheduled-tasks.json   scheduled_task.rs:99
<wd>/.anureo/goals.json             goal.rs:158（已由 thread_goals 表取代，见注记）
<wd>/.anureo/identity-profiles.json git/identity.rs:27
<wd>/.anureo/mcp.json               mcp.rs:141
```

> 注记（2026-09-07，goal-codex-alignment P6）：goal 持久化已迁至
> `<anureo_home>/tasks/tasks.db` 的 `thread_goals` 表；goals.json 仅由冻结的
> legacy detached runner（`anureo goal`，P7 移除）读写，新路径
> （`/goal` 六子命令与 `_anureo.dev/goal/*`）不再接触该文件。旧数据用
> `anureo goal --migrate` 一次性导入（源文件保留不删）。

**缺口**：agent/command/snippet/skills/plugin 五个 handler 为内存 HashMap（进程重启即丢）；skills install 的 Git/Local source 仅登记元数据；model 域仅 list（providers 无写）。

## 2. 方案

### P1 持久层（anureo，~2-3 天）

五个 handler 的 store 换成既有落盘模式：

```
<wd>/.anureo/agents.json      ~/.anureo/agents.json
<wd>/.anureo/commands.json    ~/.anureo/commands.json
<wd>/.anureo/snippets.json    ~/.anureo/snippets.json
<wd>/.anureo/skills.json      ~/.anureo/skills.json
<wd>/.anureo/plugins.json     ~/.anureo/plugins.json
<wd>/.anureo/providers.json   ~/.anureo/providers.json（model 域补读写）
```

- serde 序列化现有 struct，scope 语义照旧（command 的 Project 记录 canonical working directory，加载时按 ctx 过滤）
- 原子写（temp + rename）；读时缓存 + mtime 失效
- 内置项（builtin agents / builtin skills）不落盘，运行时合成 + override 持久化（沿用 agent_profile 的 `builtin_overrides` 结构）
- skills install：Git source 执行 clone 到 `<ANUREO_HOME>/skills/<id>`、Local source 复制目录，目录存在性校验已有（`local_path`）

### P2 补齐项（anureo，~2-3 天）

- mcp/configure 补 headers/oauth/timeout/env 字段 + `delete` 方法（mcp.json schema 扩展）
- model 域补 provider create/update/delete（写 providers.json）
- scheduled-task 已补 create/update/delete（已有落盘）；真实定时触发与 session/prompt 执行仍由 Phase 5 scheduler 负责
- `project/icon_get {theme}`（主题着色 SVG dataUrl）+ icon_set（响应带 `settings` 字段）
- TTS：`TtsSynthesizeParams` + optional `providerId/modelId`（消费 anureo 已配置 provider，不再透传凭据）
- `diagnostics/probe_url`、`diagnostics/free_port`
- 登录限流（auth/login 暴力破解防护，dictation.rs 的 `RATE_WINDOW` 流控模式可参考）

### P3 流式 + 协议内认证（anureo，~3-4 天）

**流式（同 WS）**：terminal/dictation/tts 不开新端点。

- terminal：PTY 由 anureo 承载（portable-pty）。`terminal/create|resize|close` request、`terminal/input` notification、`terminal/output` notification（4KB 聚合 + 50ms flush）；既有 `_anureo.dev/terminal/restart|force_kill` 不变。Express node-pty 退役
- dictation：复用 `dictation.rs` 既有二进制帧协议（693 行，LINEAR16/OPUS + 流控），同一 `/acp` WS 按 text/binary opcode 分流（JSON-RPC 走 text，音频帧走 binary）——零重写、保二进制效率
- tts：`tts/chunk {requestId, audio(base64), mime, final}` notification 分片下行，替代现状大音频走 `substream_url` HTTP 拉取
- 帧预算：WS 上限 1 MiB（`apps/server/src/handlers/acp.rs` `MAX_MESSAGE_BYTES`）；terminal 4KB、dictation 单帧 ≤256KB（`MAX_AUDIO_BYTES`）均安全

**协议内认证（JWT）**：WS 握手免认证，登录在协议内，凭据 JWT（前端 localStorage + 短 TTL + `auth/refresh`）。

```
[open] ──auth/login {password}──>   [authed] → {token, expiresIn}
[open] ──auth/authorize {token}──>  [authed]（重连/刷新免密）
[open] ──业务方法──> auth_required (-3200x)
```

- 未认证仅放行 `auth/login|authorize` + `initialize`；origin 检查留在握手层
- auth 从 HTTP 中间件移到 ACP 连接级状态 + 方法分发门；`ANUREO_JWT_SECRET` 语义平移
- 过渡：cookie 与 token 双栈一个版本，前端全切后删 cookie 路径；Express ui-auth + `/auth/*` 路由删除

### P4 前端迁移（anureo Desk，~2-3 天）

- 管理面 CRUD 调用点 → 既有域方法（`acpApi.agent.create` 等，wire 契约不变，无新契约）
- terminal/dictation/tts 切 ACP 流；SessionAuthGate 改协议内 JWT
- `config/reload` → client adapter no-op（anureo 同进程热生效，`requiresReload: false` 恒定）
- probe 出网归零；Express opencode-manager、ui-auth、node-pty 模块删除

## 3. opencode 配置处置

anureo 不读、不写、不迁移 `.opencode/` 与 `~/.config/opencode/`。存量配置用户自行迁移（或后续 anureo Desk 提供纯前端导入辅助，不进后端契约）。

## 4. 验收

1. 登录态 probe 出网 = 0 业务请求（仅静态/relay）
2. 增删改 agent/command/skill 后重启 anureo 进程，配置仍在；新开 session 立即可用
3. Express 全关时：管理面 + 终端 + 语音 + 登录全功能可用（worktree validate/bootstrap/preview 除外，OC 业务保留）
4. terminal 刷屏无卡顿；dictation 延迟不高于现 Express 实现
5. anureo `cargo clippy -D warnings` + nextest 全绿；anureo Desk `tsc` + `bun test --parallel` 全绿

## 5. 风险

| 风险 | 缓解 |
|---|---|
| JWT localStorage XSS 窃取 | 短 TTL + refresh + CSP；无 CSRF 面（已拍板接受） |
| 未认证 WS 蹭资源 | origin 检查 + 30s 未认证断开 + 登录限流 |
| 双进程并发写（过渡期） | `.anureo/` anureo 独占；Express 写 `.opencode/`，互不冲突 |
| cookie/token 双栈分叉 | 过渡仅一个版本期，之后删 cookie 路径 |

## 6. 不做

- worktree validate/bootstrap/preview、`git/integrate`（OC 业务）
- opencode 生态双向同步
- provider OAuth 浏览器流（`provider/*/auth` 保留 HTTP 302，后期评估）
- 大文件/视频二进制传输（无需求；未来走独立 binary 端点，前端 adapter 隔离）

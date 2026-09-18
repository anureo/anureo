# 子代理契约 Phase 1 代码方案（2026-09-18 晚执行版）

> **状态**: Plan（今晚执行）。**注意**：全量（Phase 1→4）详细实现方案已沉淀至[子代理契约实现详细方案](./subagent-impl-plan.md)，其 §2 吸收并扩展了本文内容（含 tool_call_id 传播与 cancellation 继承的细化）；本文保留为当晚提交清单快照，冲突时以详细方案为准。对已冻结的[ACP 子代理契约](./acp-subagent-contract.md)做**增量实现**，不重设计；范围严格限定为[差距审计](./subagent-interaction-gap.md)的 **Phase 1（agent-core 身份与上下文）**，外加 lifecycle 接缝预留。2026-09-18 已对全部落点做源码复核，行号为当日快照。
> **今晚不做**: Phase 2（ACP child session 注册）、Phase 3（metadata/Desk）、Phase 4（取消/统计/后台终态推送）。这些需要跨 crate 与前端联动，另行排期。
> **交叉参考**: [ACP 子代理契约](./acp-subagent-contract.md)（wire source of truth）、[差距审计](./subagent-interaction-gap.md)（Phase 划分）、[codex 子代理架构分析](../analysis/codex-subagent-architecture-20260918.md)（设计借鉴来源）

---

## 1. 现状核对（2026-09-18 源码证据）

审计结论至今仍然成立，逐条复核如下：

| 审计项 | 当日证据 | 结论 |
| --- | --- | --- |
| GAP-02 身份拼接 | `agent/agent-core/src/tools/agent/mod.rs:70-74` `prepare_agent_id` 生成 `sub-{thread}-{agent}-{depth}-{seq}`；`runner.rs:119-121` checkpoint thread 另拼 `sub-{thread}-{agent}-{depth}`（无 seq） | 未修 |
| GAP-06 depth 传播中断 | `foundation/checkpoint/src/config.rs:27` `RunnableConfig.depth` 字段存在；`act_executor.rs:90` 会读取；但 `react/build/checkpointer.rs:56-57` 硬编码 `depth: None, acp_session_id: None`；`ReactBuildConfig` 无 `depth` 字段（`react_build_config.rs`） | 未修 |
| 深度限制失效（新发现） | 因上条，嵌套 `agent` 调用经 `act_executor` 看到的 `ctx.depth` 恒为 0，`mod.rs:237` 的 `current_depth >= self.max_depth` 守卫在真实链路中是死代码 | 需今晚修复 |
| GAP-03 无 lifecycle 桥 | 全仓无 `SubagentIdentity` / `SubagentLifecycleSink` 符号 | 未做，今晚定义接缝 |
| registry 每 runner 新建 | `react/build/tool_source.rs:354` `AsyncAgentRegistry::new()` 在 build 路径内创建 | 今晚加注入接缝 |
| `eprintln!` 诊断 | `runner.rs:184` 仍存在 | 今晚替换 |

注：`ToolCallContext`（`agent/tool/tool-core/src/context.rs`）已有 `depth`、`acp_session_id` 字段；`build_config_from_profile`（`build_config.rs:32`）以 `parent_config.clone()` 起步，故 `acp_session_id` 会随 clone 自动继承——传播断裂点只在 `checkpointer.rs` 与 `ReactBuildConfig.depth` 缺失两处。

## 2. 今晚交付：4 个独立可测的提交

### Commit 1 — `feat(agent-core): add SubagentIdentity and unify invocation/thread IDs`

新增 `agent/agent-core/src/tools/agent/identity.rs`：

```rust
/// 一次 agent tool invocation 的完整身份。入口生成一次，之后只传递、不重算
/// （契约 §3.2 不变量 2）。
pub(crate) struct SubagentIdentity {
    pub invocation_id: String,      // 工具面 agent_id（原 prepare_agent_id 产物）
    pub parent_thread_id: String,   // 父 checkpoint thread
    pub child_thread_id: String,    // 子 checkpoint thread（含 seq，消除碰撞）
    pub agent_name: String,
    pub depth: u32,                 // child 的深度 = parent_depth + 1
}

impl SubagentIdentity {
    pub(crate) fn generate(parent_thread_id: &str, agent_name: &str, parent_depth: u32) -> Self;
}
```

- 生成规则：沿用现有 seq 计数器（`ASYNC_SEQ`），`child_thread_id` 必须含 seq（对齐契约 §3.2 不变量 1/3）；ID 字符串格式仍是实现细节，消费方按 opaque string 对待。
- 改 `mod.rs::call`：入口 `generate` 一次，删除 `prepare_agent_id`；`spawn_agent_task` 改收 `SubagentIdentity`。
- 改 `runner.rs:119-121`：`sub_config.thread_id = Some(identity.child_thread_id)`，删除现场拼接；`worktree.rs` 链路透传 identity。
- registry `register()` 的 agent_id 改用 `identity.invocation_id`（值格式不变，来源统一）。

**验收**：并发 100 个同名同 depth 的 `generate()` 全部 ID 唯一；`cargo nextest run -p agent identity` 过。

### Commit 2 — `fix(agent-core): propagate depth through RunnableConfig (max-depth guard)`

1. `react_build_config.rs`：`ReactBuildConfig` 增加 `pub depth: Option<u32>`（Default 为 None，clone 继承语义与 `acp_session_id` 一致）。
2. `checkpointer.rs:56`：`build_runnable_config` 改为 `depth: config.depth, acp_session_id: config.acp_session_id.clone()`——不再丢字段。
3. `runner.rs`：现有 `child_depth`（`runner.rs:151`，目前只用于显示）落进配置：`sub_config.depth = Some(child_depth)`。
4. ACP 顶层无感：`apps/acp/src/agent.rs:1543` 已设父 `acp_session_id`，`depth` 缺省 None → 顶层 ctx.depth=0 → 子=1 → 嵌套=2，`mod.rs` 的 max-depth 守卫从此生效。

**验收**：单测断言 `build_runnable_config(config with depth=Some(2))` 产出 `RunnableConfig.depth == Some(2)`；嵌套两层 agent 的集成路径 depth 递增（可用现有 react 测试设施）。

### Commit 3 — `feat(agent-core): introduce SubagentLifecycleSink with exactly-once terminal`

新增 `agent/agent-core/src/tools/agent/lifecycle.rs`（契约 §4 的 trait，今晚只立接缝，不实现 ACP 侧）：

```rust
#[async_trait]
pub trait SubagentLifecycleSink: Send + Sync {
    async fn created(&self, identity: &SubagentIdentity, agent_name: &str) -> Result<(), String>;
    async fn running(&self, identity: &SubagentIdentity) -> Result<(), String>;
    async fn terminal(&self, identity: &SubagentIdentity, outcome: SubagentOutcome) -> Result<(), String>;
}

pub enum SubagentOutcome { Completed { stats: AgentCompletionStats }, Failed { error: String }, Cancelled }
```

设计决策（借鉴 codex 分析 §6「RAII/恰一次收敛」）：

- **sink 挂在 `AsyncAgentRegistry` 内**而非 AgentTool 调用点：registry 的 `complete/fail/cancel` 已有"终态不可覆盖"守卫（`registry.rs:254`、`registry.rs:276`），"恰好一个 terminal"不变量天然收敛在此；且 `AgentCancelTool` 走同一 registry，取消路径的 terminal 免费获得。
- registry 构造改 `with_sink(Option<Arc<dyn SubagentLifecycleSink>>)`，缺省 noop；`AgentTool::new` 签名不变（registry 已是注入参数）。
- `created` 在 `spawn_agent_task` 里、`tokio::spawn` **之前**调用（契约 §4：失败则 invocation 不启动）；`running` 在任务起跑后；Phase 2 的 ACP 实现届时替换 noop。
- **不**向 sink 传 prompt/模型输出/secret（契约 §4 语义约束）。

**验收**：fake sink 记录调用序列，断言 `created → running → terminal` 顺序、重复 `complete` 不产生第二个 terminal、cancel 路径产出 `Cancelled`。

### Commit 4 — `chore(agent-core): registry injectable + tracing diagnostics`

1. `ReactBuildConfig` 增加 `pub async_agent_registry: Option<AsyncAgentRegistry>`；`tool_source.rs:354` 改为 `config.async_agent_registry.clone().unwrap_or_default()`——今晚保持"每 runner 新建"的默认行为不变，仅为 Phase 2 的 ACP 共享 registry 留注入点。
2. `runner.rs:184` 的 `eprintln!("{}", formatted)` 改 `tracing::info!(target: "subagent_display", ...)`；`subagent_display::format_subagent_event` 返回值改为结构化字段或直接在函数内打点（二选一，倾向后者）。

**验收**：`grep -r "eprintln!" agent/agent-core/src/tools/` 零命中；clippy 零警告。

### 附加提交 1b — `fix(tools): slim agent tool family interface`

工具面独立小提交（~40 行，可在 Commit 1 同 PR 或紧随其后；详细清单见[实现详细方案 §2.8](./subagent-impl-plan.md)）：

- **T1** agent_get 列表模式去掉 result 全文 + `to_string_pretty`→`to_string`（上下文炸弹，收益最大的一项）
- **T2** `estimated_paths` 补进 input_schema（现状 worktree.rs 解析但 schema 未声明）
- **T3** agent_cancel 对已终态代理改幂等成功返回
- **T4** 错误文案可执行化（depth 超限/resolve 失败列可用名/not found 区分驱逐与不存在）
- **T5** 同步返回附 stats（mod.rs:267 不再以 `_` 丢弃）
- **T6** worktree 模式返回 path/branch/preserved
- **T7** description 补并行用法一句
- **T10** agent_get/thread_get 描述互引 + status 过滤；清 tool_source.rs:375 死注释
- （T8 timeout 语义/max_runtime → 演进 A2；T9 agent_get wait → 演进 E1-5，不在今晚）

**验收**：`cargo nextest run -p agent tools`（测试 11）。

## 3. 测试与门禁（今晚完成定义）

```powershell
cargo nextest run -p agent                 # package 名对照：agent-core → agent
cargo nextest run -p anureo-acp stream_bridge   # 确认未破坏现有流桥回归
cargo clippy --workspace --all-targets -- -D warnings
```

新增测试清单（Commit 1-3 各自携带）：

| 测试 | 验证点（对齐契约 §10.1） |
| --- | --- |
| identity uniqueness | 并发同名/depth 全 ID 唯一；child_thread_id 含 seq |
| lifecycle ordering | created 先于 spawn；每路径恰一个 terminal；重复终态幂等 |
| depth propagation | RunnableConfig.depth 透传；嵌套 depth 递增；超限拒绝 |
| registry injectable | 注入共享实例时 tool_source 复用；缺省行为不变 |

## 4. 明确不做（今晚）

- ACP child session 注册 / `session/cancel` 映射（Phase 2，需 `apps/acp` SessionStore/Repository 联动）。
- `_meta["anureo.dev"].subagent` 版本化 metadata 与 stream_bridge 投影（GAP-04 / Phase 3）。
- Desk 前端改动（`isSubagentTool`、`parentSessionId → parentID` 适配）。
- registry 跨会话共享的真实注入（今晚只留字段接缝）。
- codex 式信箱/驻留/角色叠加：按[演进方案](./subagent-evolution-plan.md)分档吸收（A 档已并入本 MVP，E1/E2 待 MVP 后），与冻结契约不捆绑。

## 5. 提交后的文档动作

- 在[差距审计](./subagent-interaction-gap.md) §5 Phase 1 条目下追加"2026-09-18 已实现"标记与 commit hash。
- 若 Phase 1 验收全绿，契约 §12 完成定义中 1/5 两项部分达成，在契约状态头追加进度注记（不改状态）。

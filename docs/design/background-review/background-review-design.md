# Background Review 架构设计

> **状态**：Draft，待评审
> **日期**：2026-08-20
> **范围**：anureo ACP background review（Memory / Skill 知识整理）的运行时、生命周期与持久化边界
> **相关代码**：`apps/acp/src/review_runner.rs`、`apps/acp/src/agent.rs`、`apps/acp/src/extensions/auto_review.rs`
> **协议方案**：[Background Review ACP 协议设计](./background-review-protocol.md)
> **交互方案**：anureo Desk `anureo-feat-dev/docs/design/background-review-interaction.md`
> **相关文档**：[10-memory-review-experimental.md](../../user-guide/10-memory-review-experimental.md)、[acp-subagent-contract.md](../acp-subagent-contract.md)

---

## 1. 背景与边界

当前系统存在两套 review：

1. **Background review**：从对话中提取 Memory 和 Skill，写入长期上下文。
2. **Auto review**：检查代码变更，通过独立 review session 与实现 Agent 循环协作。

本文只设计第一类。两者可以共享 ACP notification、权限校验和 anureo Desk 的活动中心，但不共享 session、状态机和结果 schema。

## 2. 当前运行链路

```text
prompt 完成
  → apps/acp/src/agent.rs
  → review_runner::spawn_inprocess_review
  → extract_session_text
  → anureo_curator::run_review
  → Memory / Skill 写入
  → ReviewHistory 持久化
  → agent_message_chunk + session_info_update
```

当前实现：

| 能力 | 现状 | 证据 |
|---|---|---|
| 异步执行 | 独立 OS thread 和 Tokio runtime | `apps/acp/src/review_runner.rs:101-170` |
| 每 session 去重 | `global_registry().try_acquire` | `apps/acp/src/review_runner.rs:128-140` |
| 自动触发 | 每次 `RunCompletion::Finished` 后启动 | `apps/acp/src/agent.rs:1272-1288` |
| 手动触发 | `/review-skill` 命令 | `apps/acp/src/agent.rs:1045-1060` |
| 历史记录 | `ReviewHistory` append | `apps/acp/src/review_runner.rs:172-220` |
| session 状态 | `session/list` 返回 `_meta.review` | `apps/acp/src/agent.rs:2059-2180` |
| 完成通知 | assistant 摘要和 session metadata | `apps/acp/src/review_runner.rs:242-285` |

## 3. 设计决策

| 维度 | 决定 | 说明 |
|---|---|---|
| 任务模型 | 单 session 后台 job | 不创建隐藏 review session |
| 生命周期 | 主 prompt 完成后异步启动 | 不阻塞主 Agent |
| 状态来源 | 持久化 history + ACP realtime notification | 刷新或重连后可以恢复 |
| 结果粒度 | session summary + action details | UI 不解析自然语言摘要作为数据源 |
| 取消语义 | 只取消 Background review | 不取消主 Agent prompt |
| Auto review 边界 | 继续使用独立 `auto-review` 域 | 防止两类 review 状态混淆 |

## 4. 安全与可逆性

Background review 会修改未来 Agent 的 context，因此这些结果应被视为 AI 生成的持久化变更。

### Phase 0

- 保持现有直接写入行为；
- UI 展示来源 session、写入数量和 action 摘要；
- 明确标注“AI 整理结果”。

### Phase 1

- review 输出候选变更；
- 用户确认后写入 Memory / Skill；
- 支持逐条拒绝、整次撤销和 snapshot rollback。

## 5. 实施顺序

### P0：状态可见化

1. 扩展 `SessionReviewMeta`；
2. 增加 running / completed / skipped / failed 状态；
3. 保证 session/list 和 realtime update 字段一致；
4. 增加 runner 状态转换单测。

### P1：结构化结果

1. 持久化 `review_id` 和 action details；
2. 实现 `review/status`、`review/history`、`review/details`；
3. 增加 `review/start` 和 `review/cancel`；
4. 接入统一权限和 progress notification。

### P2：确认和撤销

1. 引入 candidate changes；
2. 提供 Memory / Skill diff；
3. 支持确认、拒绝和 rollback。

## 6. 改动文件清单

| 文件 | 改动 | 说明 |
|---|---|---|
| `apps/acp/src/review_runner.rs` | 修改 | 状态转换、review id、结构化结果通知 |
| `apps/acp/src/agent.rs` | 修改 | 扩展 `SessionReviewMeta` 和 session list 映射 |
| `apps/acp/src/extensions/review.rs` | 新增 | review extension，详见协议文档 |
| `apps/acp/src/extensions/mod.rs` | 修改 | 注册 review extension 和 capability |

## 7. 测试计划

| 测试 | 验证点 |
|---|---|
| runner 单测 | start、completed、skipped、failed 状态正确 |
| runner 单测 | 状态、去重和生命周期正确 |
| persistence 测试 | 重启后可恢复最新状态和历史 |
| E2E | 对话完成后后台 review 不阻塞主 Agent |

## 8. 风险与开放问题

| 风险 | 处理 |
|---|---|
| best-effort notification 丢失 | UI 必须以持久化查询作为最终一致性来源 |
| 自动写入影响未来 context | Phase 1 增加 candidate / confirm |
| Background review 与 Auto review 混名 | 产品文案和协议域明确区分 |

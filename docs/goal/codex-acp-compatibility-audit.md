# codex-acp Goal 扩展兼容性审计

> **状态**：核心 wire 与生命周期兼容（2026-09-11）
> **上游基线**：`agentclientprotocol/codex-acp` main `effb0fe670a49dfbb5071764b5f8a2e3c09e2393`
> **权威参考**：[goal-extension.md](https://github.com/agentclientprotocol/codex-acp/blob/effb0fe670a49dfbb5071764b5f8a2e3c09e2393/docs/goal-extension.md)、`src/GoalExtension.ts`、`src/ThreadGoalSnapshot.ts`、`src/CodexAcpServer.ts`

---

## 结论

anureo 的 provider-neutral goal 面可供按 codex-acp Goal Extension v1 编写的客户端使用。权威入口、动作集合、状态投影、快照位置、时间单位、清除通知、legacy alias 和 set 立即启动语义均已对齐。

anureo 仍保留少量向后兼容扩展字段与响应字段；它们是 additive，不改变 codex-acp 客户端所依赖的字段。客户端必须以 `session_info_update._meta.goal` 为权威状态，不应依赖 mutation result 中的额外快照。

## 对照矩阵

| 维度 | codex-acp 基线 | anureo | 结论 |
|---|---|---|---|
| capability | initialize 顶层 `_meta.goal`，version 1 | 同形；另在 `agentCapabilities._meta.goal` 重复提供 | 兼容；后者为 additive |
| 控制方法 | `_session/goal` | `_session/goal` | 一致 |
| 动作 | set/pause/resume/clear | 同四项 | 一致 |
| legacy alias | `_codex/session/goal_control` 可用但不广播 | 同样注册且不广播 | 一致 |
| set 参数 | sessionId/action/objective；允许未知扩展字段 | 同；额外解释可选 `tokenBudget` | 兼容扩展 |
| set 生命周期 | 落 goal 后立即尝试启动 turn | 同 | 一致 |
| 快照位置 | `session_info_update._meta.goal` | 同 | 一致 |
| objective | 必填完整字符串 | 文件化存储发布前还原全文 | 一致 |
| 状态 | active/paused/blocked/limited/complete | 内部六态投影到同五态 | 一致 |
| 时间 | Unix 毫秒 | Unix 毫秒 | 一致 |
| clear | 发布 `goal: null` | 同 | 一致 |
| mutation result | 当前实现返回 `{}` | 返回额外 goal/cleared 信息 | wire 可兼容；客户端不得依赖扩展结果 |
| 恢复 | goal 属于 session；通知为推送状态 | DB 持久化，`session/load` 重发权威快照 | 兼容且恢复语义更明确 |

## 本轮修复

1. 注册 `_codex/session/goal_control`，并让 stdio/WS 共用扩展分发。
2. `_session/goal set` 成功发布 active 快照后立即调用 idle-safe turn driver。
3. 文件化 objective 发布前还原，快照始终携带上游必填 `objective`。
4. 生产路径拒绝未知 session 和跨 principal session，不再退化为以任意 sessionId 创建 thread goal。
5. E2E 覆盖主方法、legacy alias、set 立即启动、跨 turn 记账、limited 投影和清除。

## 非阻塞差异

- `tokenBudget`、`statusReason` 是 anureo 扩展。codex-acp 的参数 parser 为 passthrough，客户端应把这些字段视为可选。
- anureo mutation result 带额外快照；codex-acp 当前返回空对象。JSON-RPC 调用仍兼容，但跨实现客户端必须监听快照通知。
- `agentCapabilities._meta.goal` 是历史客户端兼容投影；标准发现位置仍是 initialize 响应顶层 `_meta.goal`。

## 回归入口

```powershell
cargo nextest run -p anureo-acp --test e2e_goal_neutral
cargo nextest run -p anureo-acp --test e2e_goal_recovery
cargo clippy -p goal -p anureo-acp --all-targets -- -D warnings
```


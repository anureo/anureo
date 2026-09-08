# Goal 系统文档

> 状态：目录索引
> 创建：2026-09-06
> 说明：goal 相关设计、分析与参考资料统一收纳于本目录；协议规范与用户指南因系列编号保留原地（见下）。

| 文档 | 状态 | 一句话定位 |
|---|---|---|
| [goal-codex-alignment.md](./goal-codex-alignment.md) | **已实施**（P0-P8 落地；中立 goal 面 = `agentCapabilities._meta.goal` 协商 + `_session/goal` 控制 + 快照发布，规范见附录 C 与 spec 14） | 全量对齐 Codex 架构：session-integrated + `thread_goals` 单一事实源 + 3 个模型工具；取代 goal-system-workflow.md |
| [goal-codex-alignment-todo.md](./goal-codex-alignment-todo.md) | P0-P8 已收尾；遗留：旧 runner 移除（独立版本）、全链路续跑 e2e（后续项）、FE 跨仓手动验收 | 上文的开发执行清单：Phase 0-8 可勾选任务 + 每 Phase DoD + 风险检查点（R1-R5），随开发滚动更新 |
| [goal-improvement-plan.md](./goal-improvement-plan.md) | **已搁置**（被 alignment 路线取代，2026-09-07） | 差距修补路线：保持 server-owned 架构，只借记账/审计/steering 机制（P0-P4，明确不采用 per-thread goal 与 idle 续跑） |
| [goal-system-workflow.md](./goal-system-workflow.md) | 已被取代 | 多 Agent + Lua 双头编排（历史参考，勿据此实现） |
| [session-goal-integration.md](./session-goal-integration.md) | 历史草案 | JS runtime 移植路线；Phase 1-2 基础设施被后续方案复用 |
| [codex-goal-analysis.md](./codex-goal-analysis.md) | 参考资料 | Codex goal 源码事实记录（上游快照 `e3e5ad28`），语义基线 |

**已决**（2026-09-07）：评审定夺 alignment（全量对齐）路线胜出并已实施；goal-improvement-plan（差距修补）搁置。

**其他位置的 goal 文档**（系列编号保留原地）：

- [../acp-spec/extensions/14-goal-scheduled-task.md](../acp-spec/extensions/14-goal-scheduled-task.md) — ACP 扩展协议规范
- [../user-guide/09-goal-task-experimental.md](../user-guide/09-goal-task-experimental.md) — 用户指南

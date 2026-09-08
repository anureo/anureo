# ACP WebSocket 剩余性能优化方案

> **状态**: Draft（仅记录未完成项，2026-08-26）
> **审计对象**: OpenChamber `http://localhost:5180/session/session-3b04a31b-9e70-4e5f-a140-96f1e8446de3` → anureo dev ACP WebSocket
> **当前证据**: `.anureo-home/ws-trace-session-3b04a31b-git-check-20260826.json`
> **交叉参考**: [ACP WebSocket 剩余性能审计](../analysis/acp-websocket-startup-interaction-audit.md)、[ACP 通信合理性审查](../dev/acp/04-communication-reasonableness.md)

---

本文只维护当前仍需修复的内容。已达到验收门禁的启动调度、session index、topic union、可选配置读取和 target critical-path 隔离不再列入。

## 1. 当前未达标指标

| 指标 | 最新实测 | 目标 |
| --- | ---: | ---: |
| target `session/load` RPC | 1.50 s（历史 warm 采样为 470 ms～1.88 s） | `p95 <= 500 ms` |
| target replay response | 47,682 B | 可分页或压缩 |

当前剩余问题不再是前端请求排队或 Git payload，而是大 session 回放的服务端耗时波动。

## 2. P1：降低 `session/load` 长尾

### 问题

同一 session、同一 warm dev 环境下，完整回放耗时在 470 ms 到 1.88 s 之间波动，最新样本为 1.50 s。响应体稳定约 47.7 KiB，说明需要把服务端读取、事件重放、序列化和 WebSocket 写出分别计时，不能继续把长尾归因于前端调度。

### 方案

- 在 `session/load` 服务端链路增加分段 tracing：索引定位、checkpoint 读取、event log 读取、materialize、JSON 序列化、socket send；
- 记录 `session_id`、event 数、checkpoint 命中、原始字节数和输出字节数，但不记录消息正文；
- 优先复用最近 materialized snapshot，并以 session revision 校验失效；
- 对大 replay 提供 cursor/page 或 snapshot + delta，首屏只返回可见尾部和恢复游标；
- 若双方能力协商支持，评估 WebSocket per-message deflate；未协商时不得擅自改变 transport；
- reconnect、分页重试和 route 切换必须保持 generation guard 与幂等。

### 验收

- 固定 session 连续 warm 运行 20 次，`session/load` p95 `<= 500 ms`、p99 `<= 1 s`；
- 首屏首批消息可见时间 `<= 1 s`；
- 分页/增量恢复与完整恢复 materialize 结果一致；
- 不丢失 tool call、plan、usage、terminal 和 session metadata 更新。

## 3. 测试与发布门禁

每次优化使用同一 URL 做 cold 1 次、warm 20 次采样，并输出：

- `session/load` 分段耗时、p50/p95/p99 和 response bytes；
- target replay 与 non-replay bytes；
- error frame、重复 request、断线恢复和 route race 结果。

不能用单次快速样本替代分位数；需要以 20 次 warm 采样确认 `session/load` 长尾是否关闭。

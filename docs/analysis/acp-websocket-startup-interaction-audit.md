# OpenChamber × anureo ACP WebSocket 剩余性能审计

> **状态**: 已复测；仅保留未关闭问题（2026-08-26）
> **审计对象**: OpenChamber `http://localhost:5180/session/session-3b04a31b-9e70-4e5f-a140-96f1e8446de3` → anureo dev server `ws://localhost:5180/acp`
> **证据文件**: `.anureo-home/ws-trace-session-3b04a31b-git-check-20260826.json`
> **修复方案**: [ACP WebSocket 剩余性能优化方案](../design/acp-bootstrap-performance-optimization.md)

---

本文只记录最终 trace 中仍未达到门禁的性能问题，不保留已修复项及其历史方案。

## 1. 剩余结论

目前只剩一项：`session/load` 完整回放存在明显耗时波动，最新样本为 1.50 s，历史 warm 样本范围为 470 ms～1.88 s。

页面已能正确恢复目标 session，最终 URL 不变，浏览器 console 无 warning/error，ACP error frame 为 0。因此以下问题是服务端长尾和后台 payload 问题，不是连接、协议正确性或页面可用性问题。

## 2. 最新 wire 数据

采样命令：

```powershell
node scripts/web-audit/ws-trace-session.mjs `
  --url "http://localhost:5180/session/session-3b04a31b-9e70-4e5f-a140-96f1e8446de3?startupTrace=1" `
  --wait 8000 `
  --max-frame 1200 `
  --out ".anureo-home/ws-trace-session-3b04a31b-git-check-20260826.json"
```

| 指标 | 实测 |
| --- | ---: |
| ACP OUT / IN frames | 16 / 19 |
| ACP outbound / inbound | 2,232 B / 59,405 B |
| target replay | 47,682 B |
| non-replay inbound | 11,723 B |
| error frames | 0 |
| target `session/load` | 1 次 |
| session index | 1 次 |
| initial subscribe | 1 次 |
| 首屏 `git/check` | 1 次；request 107 B / response 59 B |
| 首屏 full `git/status` | 0 次 |

## 3. `session/load` 长尾

最新关键时序：

| 时间 | 事件 |
| ---: | --- |
| 3.28 s | `initialize` response |
| 3.34 s | target `session/load` request |
| 4.84 s | target `session/load` response |

request 在 initialize 后约 60 ms 发出，前端调度不再是瓶颈；1.50 s 几乎全部位于 RPC 内部。历史相同环境采样范围为 470 ms～1.88 s，说明服务端链路存在长尾或缓存/IO 差异。

下一步必须对 checkpoint、event log、materialize、serialize 和 socket send 分段计时，并以 20 次 warm 样本计算 p95/p99。单次 trace 不能证明该问题已关闭。

## 4. 剩余门禁

| 指标 | 当前 | 门禁 |
| --- | ---: | ---: |
| `session/load` warm p95 | 尚未测得；单次 470 ms～1.88 s | `<= 500 ms` |
| 首批消息可见 | 最新样本受 1.50 s RPC 限制 | `<= 1 s` |

后续复测必须报告延迟分位数；只看最终页面或单次总耗时不足以关闭问题。

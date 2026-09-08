# ACP 多会话并发可靠性加固

> **状态**: 已实现（2026-09-05）
> **日期**: 2026-09-05
> **相关代码**: `apps/acp/src/runtime.rs`、`apps/acp/src/notification_router.rs`、`apps/acp/src/stream_bridge.rs`、`foundation/checkpoint-sqlite-store/src/sqlite_util.rs`、`foundation/checkpoint-sqlite-store/src/sqlite_saver.rs`
> **交叉参考**: [标准 ACP 单 Server 多 Session 实现方案](./acp-single-server-multi-session.md)、[会话 IndexedDB 缓存与断线增量恢复](./session-incremental-recovery.md)、[运行中会话的多端加载](./session-load-running-multi-device.md)

---

## 1. 背景与问题

ACP runtime 允许不同 session 并行执行，并通过全局 semaphore 将并行 prompt 数限制为 4。内存中的 cwd、配置、取消 token 与连接绑定均按 session 隔离，但实际运行还共享两类进程级资源：

1. 所有 thread 的 checkpoint 写入同一个 SQLite 数据库；
2. 所有 session 的实时 `session/update` 先进入同一个 runtime ingress，再路由到各 connection。

原实现存在两个跨 session 干扰窗口：

- `SqliteSaver::put` 与 `put_writes` 没有使用已有的 `SQLITE_BUSY` / `SQLITE_LOCKED` retry helper。WAL 允许并发读取，但同一时刻仍只有一个 writer；两个 session 同时 checkpoint 时，其中一个 prompt 可能因瞬时锁竞争失败。
- Agent stream callback 是同步接口，却向容量 256 的共享 channel 执行 `try_send`。队列满时事件被直接丢弃；同时 runtime 的单 consumer 会等待 connection outbound queue，一个慢连接可能阻塞其他 session 的记录与投递。

现有 multi-session E2E 只验证不同 session 产生不同 ID 且可被列出，没有执行两个并行 prompt，也没有覆盖 checkpoint writer contention 或慢连接背压。

## 2. 设计决策

| 维度 | 决定 | 说明 |
| --- | --- | --- |
| SQLite writer | `BEGIN IMMEDIATE` + bounded retry | 在 transaction 开始处竞争 writer lease，避免执行到中途才失败 |
| retry 范围 | `put` 与 `put_writes` 整个原子写单元 | retry 不得只包裹单条 statement，否则多行 pending writes 可能部分提交 |
| SQLite 等待 | connection busy timeout + jitter retry | busy timeout 吸收短锁，显式 retry 处理事务边界竞争 |
| stream ingress | `UnboundedSender` | 同步 stream callback 无法 await；进入 runtime 前不得静默丢 canonical event |
| live connection 投递 | bounded queue `try_send` | 慢连接只丢失自身 live delivery，不阻塞 event log 与其他 connection |
| 恢复语义 | event log 为事实源，client 按 seq 检测 gap | live 投递失败后通过既有 session recovery 补齐 |
| history/load | 保持 await + flush barrier | replay 必须先于 `session/load` response，不适用 live 非阻塞策略 |

## 3. 数据流

```text
session A stream callback ─┐
                           ├─ unbounded ordered ingress ─ event log append ─┬─ try_send connection A
session B stream callback ─┘                                               └─ try_send connection B

session/load replay ─ send_and_flush ─ bounded connection queue ─ barrier ─ response
```

关键不变量：

1. runtime 接受的 canonical live event 不因共享 ingress 满而丢失；
2. event log append 发生在 live route 之前；
3. connection A 的背压不得阻塞 connection B；
4. 同一 connection 的 live queue 满时返回显式 `QueueFull`，由现有 seq/gap recovery 恢复；
5. checkpoint 的 thread 隔离键不替代 writer contention 控制。

## 4. 实现细节

### 4.1 SQLite checkpoint

`execute_write` 使用 `TransactionBehavior::Immediate`，并在 `DatabaseBusy` / `DatabaseLocked` 时进行带 jitter 的有限重试。`SqliteSaver::put` 和 `put_writes` 将完整写单元交给该 helper。

`open_sqlite_with_wal` 同时设置有限 busy timeout，让 schema 初始化与其他未进入 helper 的短写操作也能等待当前 writer 释放，而不是立即失败。

### 4.2 实时事件 ingress

`SessionNotifier` 由同步 callback 调用，因此 runtime ingress 使用 unbounded channel。该 channel 只解除 callback 与 async consumer 之间不兼容的背压边界；持久化后仍由 session event log 的 event/byte retention 控制长期空间。

### 4.3 慢连接隔离

普通 live `NotificationRouter::send` 对 connection outbound queue 使用 `try_send`。若某个 connection 已满：

- event 已先写入 session update log；
- router 记录 route failure 并继续消费其他 session；
- client 后续通过 cursor gap/session recovery 补齐。

`send_and_flush`、`send_history_batch` 与 `flush_session` 继续 await，因为这些路径参与 request/response 顺序协议，不能降级为 best-effort live delivery。

## 5. 改动文件清单

| 文件 | 改动类型 | 说明 |
| --- | --- | --- |
| `foundation/checkpoint-sqlite-store/src/sqlite_util.rs` | 修改 | immediate transaction、busy timeout |
| `foundation/checkpoint-sqlite-store/src/sqlite_saver.rs` | 修改 | checkpoint 与 pending writes 使用统一 retry |
| `apps/acp/src/runtime.rs` | 修改 | canonical update ingress 改为有序 unbounded channel |
| `apps/acp/src/stream_bridge.rs` | 修改 | notifier sender 类型与发送语义同步调整 |
| `apps/acp/src/agent.rs` | 修改 | session update sender 类型同步调整 |
| `apps/acp/src/review_runner.rs` | 修改 | background review sender 类型同步调整 |
| `apps/acp/src/notification_router.rs` | 修改 | live route 非阻塞，增加慢连接隔离测试 |
| `foundation/checkpoint-sqlite-store/src/sqlite_saver.rs` | 测试 | 两个 thread 并发 checkpoint contention 回归 |

## 6. 测试计划

| 测试 | 验证点 |
| --- | --- |
| held writer + `put` | 外部 writer 短暂持锁时 checkpoint 最终成功 |
| held writer + `put_writes` | pending writes 作为一个 transaction 重试并完整提交 |
| slow connection isolation | connection A queue 满时，connection B 的 live update 仍立即入队 |
| parallel prompt execution | 两个不同 session 的 prompt 实际同时进入 executor |
| existing sqlite suite | roundtrip、幂等、thread/ns/checkpoint 隔离不回归 |
| ACP unit suite | session update、load/recovery、background review sender 改型不回归 |
| clippy | workspace 相关 package 零 warning |

## 7. 验证结果

- `cargo test --locked -p checkpoint-sqlite-store --lib`：41 passed；
- 慢连接隔离与双 session 并行 prompt 两个定向回归测试：2 passed；
- `cargo clippy --locked -p checkpoint-sqlite-store --lib -- -D warnings`：通过；
- `cargo clippy --locked -p anureo-acp --lib -- -D warnings`：通过；
- `cargo test --locked -p anureo-acp --lib`：607 passed，13 failed。失败用例均在构造默认 runtime 时无法打开沙箱外的默认 session config 数据库；新增并发用例和本次涉及的路由用例均通过。

## 8. 向后兼容与限制

- ACP wire schema、session ID、cursor 与 error code 不变。
- 全局 prompt semaphore 默认值仍为 4。
- live queue 满会使该 connection 暂时缺帧，但不会污染其他 session；正确性依赖已实现的 seq/gap recovery。
- unbounded ingress 消除了确定性的 queue-full 丢事件，但极端持续磁盘停顿仍可能造成短期内存增长；后续可将 event-log append 拆为按 session 分片的 worker，并增加 ingress depth metric。

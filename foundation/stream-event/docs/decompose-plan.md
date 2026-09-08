# 拆除 stream-event crate 方案

> **状态**: 设计提案
> **目标**: 将 `stream-event` crate (~3,650 行, 13 文件) 拆解为职责清晰的独立单元

---

## 目录

1. [现状：为什么想拆](#1-现状为什么想拆)
2. [模块→消费者映射](#2-模块消费者映射)
3. [三个独立关注点](#3-三个独立关注点)
4. [拆分方案](#4-拆分方案)
5. [推荐方案：A（保守拆分）](#5-推荐方案a保守拆分)
6. [迁移步骤](#6-迁移步骤)
7. [风险评估](#7-风险评估)

---

## 1. 现状：为什么想拆

`stream-event` 当前混合了三个不相关的关注点：

```
stream-event/ (~3,650 行)
├── 核心事件词汇     ← foundation/*, agent-core/*, apps/* 全依赖
├── 线路协议格式     ← 只有 runner.rs + CLI agent.rs 用
└── Codex 协议类型   ← 只有 codex_event_builder + experimental/codex 用
```

**问题**：
- 改 `convert.rs` 一行 → 重编译 foundation/llm（它只需要 `MessageChunk`）
- 改 `codex.rs` → 重编译 pregel（它根本不用 codex）
- `MessageChunk` 和 `ProtocolEvent` 毫无关系，却在同一个 crate

---

## 2. 模块→消费者映射

对每个模块，追踪谁在用它：

### 2.1 核心事件词汇（生产者 + 消费者都用）

| 模块 | 核心导出 | 消费者 |
|------|---------|--------|
| `message.rs` | `MessageChunk`, `MessageChunkKind`, `StreamSink` | foundation/llm, agent-core/think_node, apps/cli/display, apps/acp |
| `stream_event.rs` | `StreamEvent<S>` (22 变体) | 几乎所有人 |
| `stream_mode.rs` | `StreamMode` (8 变体) | foundation/graph-core, pregel, agent-core |
| `metadata.rs` | `StreamMetadata`, `CheckpointEvent<S>` | foundation/graph-core, agent-core |
| `sender.rs` | `StreamEventSink` | agent-core/think_node |
| `writers/stream_writer.rs` | `StreamWriter<S>` | foundation/graph-core, pregel |

### 2.2 线路协议格式（只有桥接层用）

| 模块 | 核心导出 | 消费者 |
|------|---------|--------|
| `event.rs` | `ProtocolEvent` (22 变体 wire enum) | agent-core/runner, apps/cli/agent.rs |
| `envelope.rs` | `Envelope`, `EnvelopeState`, `to_json` | agent-core/runner, apps/cli/agent.rs |
| `convert.rs` | `stream_event_to_protocol_event`, `stream_event_to_format_a`, `ProtocolEventEnvelope`, `stream_event_to_protocol_envelope` | agent-core/runner |

### 2.3 Codex 协议（完全独立）

| 模块 | 核心导出 | 消费者 |
|------|---------|--------|
| `codex.rs` | `CodexEvent`, `CodexUsage`, `CodexErrorInfo`, helper functions | apps/cli/codex_event_builder, experimental/codex/agent + event_bridge + thread_log |

### 2.4 消费者依赖矩阵

```
                         核心    协议    Codex
                        ─────  ─────  ──────
foundation/llm            ✅      ❌      ❌
foundation/graph-core     ✅      ❌      ❌
foundation/pregel         ✅      ❌      ❌
agent-core/think_node     ✅      ❌      ❌
agent-core/act_executor   ✅      ❌      ❌
agent-core/subagent_disp  ✅      ❌      ❌
agent-core/runner         ❌      ✅      ❌
apps/cli/agent.rs         ✅      ✅      ❌
apps/cli/display          ✅      ❌      ❌
apps/cli/codex_builder    ❌      ❌      ✅
apps/acp                  ✅      ❌      ❌
experimental/codex        ✅      ❌      ✅
```

**关键发现**：协议层（event/envelope/convert）只有 **2 个消费者**（runner + CLI agent.rs），核心层有 **11 个消费者**，Codex 只有 **2 个消费者**。

---

## 3. 三个独立关注点

```
┌─────────────────────────────────────────────────┐
│ 关注点 A: 核心事件词汇                            │
│                                                 │
│   message.rs      MessageChunk, StreamSink       │
│   stream_event.rs StreamEvent<S>                 │
│   stream_mode.rs  StreamMode                     │
│   metadata.rs     StreamMetadata, CheckpointEvent│
│   sender.rs       StreamEventSink               │
│   writers/        StreamWriter                   │
│                                                 │
│   行数: ~1,170                                   │
│   消费者: 11 个 crate                            │
│   依赖: serde, serde_json, tokio (mpsc)          │
└─────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────┐
│ 关注点 B: 线路协议格式                            │
│                                                 │
│   event.rs        ProtocolEvent (wire enum)      │
│   envelope.rs     Envelope, EnvelopeState        │
│   convert.rs      转换函数 + ProtocolEventEnvelope│
│                                                 │
│   行数: ~1,940                                   │
│   消费者: 2 个 crate (runner + CLI agent.rs)     │
│   依赖: serde, serde_json, 关注点 A              │
└─────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────┐
│ 关注点 C: Codex 协议                              │
│                                                 │
│   codex.rs        CodexEvent, CodexUsage, ...    │
│                                                 │
│   行数: ~426                                     │
│   消费者: 2 个 crate (codex_builder + codex daemon)│
│   依赖: serde, serde_json (无内部依赖)            │
└─────────────────────────────────────────────────┘
```

---

## 4. 拆分方案

### 方案 A：保守拆分（推荐）

拆成 3 个 crate，保持现有 crate 边界尽可能不变：

```
stream-event/ (slimmed, ~1,170 行)       ← 保留 crate 名
├── message.rs
├── stream_event.rs
├── stream_mode.rs
├── metadata.rs
├── sender.rs
└── writers/

anureo-protocol/ (new, ~1,940 行)          ← 新 crate (或复用已废弃的旧名)
├── event.rs
├── envelope.rs
└── convert.rs
  └── depends on: stream-event

codex-protocol/ (new, ~426 行)           ← 新 crate
└── codex.rs
    └── depends on: (无内部依赖)
```

**优点**：
- 11 个消费者不需要改 Cargo.toml（仍依赖 `stream-event`）
- 只有 runner.rs + CLI agent.rs 需要加 `anureo-protocol` 依赖
- codex 消费者改为依赖 `codex-protocol`
- 编译隔离：改协议层不再重编译 foundation/*

**缺点**：
- `stream-event` 名字不够准确（剩下的是事件类型，不是 "stream event" 协议）
- 仍有 3 个 crate 要维护

### 方案 B：激进拆分（彻底拆除）

完全消灭 `stream-event` crate，将代码分散到归属的 crate：

```
foundation/llm/
└── src/stream.rs (新)
    ├── MessageChunk, MessageChunkKind     ← 从 message.rs 移入
    └── StreamSink trait                   ← 从 message.rs 移入

foundation/graph-core/
└── src/event.rs (新)
    ├── StreamEvent<S>                     ← 从 stream_event.rs 移入
    ├── StreamMode                         ← 从 stream_mode.rs 移入
    ├── StreamMetadata, CheckpointEvent    ← 从 metadata.rs 移入
    └── StreamWriter                       ← 从 writers/stream_writer.rs 移入

agent-core/
└── src/run/runner.rs (已有)
    ├── Envelope, EnvelopeState            ← 从 envelope.rs 移入
    ├── ProtocolEvent                      ← 从 event.rs 移入
    ├── stream_event_to_protocol_envelope  ← 从 convert.rs 移入
    └── StreamEventSink                    ← 从 sender.rs 移入

apps/cli/
└── src/codex/ (新模块)
    └── codex.rs                           ← 从 codex.rs 移入
```

**优点**：
- 完全消灭 `stream-event` crate
- 类型归属于它们的领域所有者
- 无需新 crate

**缺点**：
- **大量 Cargo.toml 改动**：11 个 crate 要调整依赖路径
- **循环依赖风险**：`StreamEvent` 包含 `MessageChunk`，如果 MessageChunk 在 llm，StreamEvent 在 graph-core，则 graph-core → llm（当前不存在）
- `foundation/llm` 当前 re-export `MessageChunk`，变成定义后 graph-core 需要依赖 llm
- `StreamEventSink` 同时实现 `StreamSink`(llm) 和产出 `StreamEvent`(graph-core)，必须放在依赖两者的 crate

**循环依赖问题图解**：

```
当前:                           方案 B 后:
                               
foundation/llm                  foundation/llm
  └── depends on stream-event     └── defines MessageChunk, StreamSink
                                         ↑
foundation/graph-core            foundation/graph-core
  └── depends on stream-event     └── depends on llm (NEW! 循环风险)
      └── defines StreamEvent            └── defines StreamEvent

agent-core                      agent-core
  └── depends on both              └── depends on both (不变)
```

graph-core 当前不依赖 llm，方案 B 会创建这个新依赖。

### 方案 C：折中（2 crate）

保留 `stream-event`（核心）+ 提取 `codex-protocol`，协议层留在 stream-event 中：

```
stream-event/ (~3,200 行)                ← 保留大部分
├── message.rs
├── stream_event.rs
├── stream_mode.rs
├── metadata.rs
├── sender.rs
├── writers/
├── event.rs                              ← 保留
├── envelope.rs                           ← 保留
└── convert.rs                            ← 保留

codex-protocol/ (~426 行)                 ← 仅提取 codex
└── codex.rs
```

**优点**：
- 最小改动（只有 codex 消费者改依赖）
- codex 确实是独立的协议，不该在这个 crate

**缺点**：
- 没有解决核心问题（改 convert.rs 仍重编译 foundation/*）
- 只是"切了一块边角料"

---

## 5. 推荐方案：A（保守拆分）

### 理由

| 维度 | 方案 A | 方案 B | 方案 C |
|------|:------:|:------:|:------:|
| 解决编译隔离 | ✅ | ✅ | ❌ |
| 消费者改动量 | 小 (2 crate) | 大 (11 crate) | 极小 (2 crate) |
| 循环依赖风险 | 无 | **有** | 无 |
| 新 crate 数 | 2 | 0 | 1 |
| 类型归属清晰 | 中 | 高 | 低 |
| 迁移风险 | 低 | 高 | 极低 |

方案 A 在收益和风险之间取得了最佳平衡。

### 目标结构

```
stream-event/ (slimmed)
├── Cargo.toml
├── src/
│   ├── lib.rs               # re-exports
│   ├── message.rs           # MessageChunk, MessageChunkKind, StreamSink
│   ├── stream_event.rs      # StreamEvent<S>
│   ├── stream_mode.rs       # StreamMode
│   ├── metadata.rs          # StreamMetadata, CheckpointEvent
│   ├── sender.rs            # StreamEventSink
│   └── writers/
│       ├── mod.rs
│       └── stream_writer.rs # StreamWriter
└── tests/
    └── stream_event.rs      # 保留核心测试

anureo-protocol/ (new)
├── Cargo.toml               # depends on: stream-event
├── src/
│   ├── lib.rs
│   ├── event.rs             # ProtocolEvent
│   ├── envelope.rs          # Envelope, EnvelopeState, to_json
│   └── convert.rs           # stream_event_to_protocol_*, ProtocolEventEnvelope
└── tests/
    └── protocol.rs          # 保留协议测试

codex-protocol/ (new)
├── Cargo.toml               # depends on: serde, serde_json
├── src/
│   └── lib.rs               # codex.rs 内容
└── tests/
    └── codex.rs             # 保留 codex 测试
```

### Cargo.toml 依赖关系

```
codex-protocol     → (无内部依赖)

stream-event       → serde, serde_json, tokio (mpsc)

anureo-protocol      → stream-event, serde, serde_json

foundation/llm     → stream-event           (不变)
foundation/graph-core → stream-event        (不变)
foundation/pregel  → stream-event           (不变)
agent-core         → stream-event + anureo-protocol  (新增 anureo-protocol)
apps/cli           → stream-event + anureo-protocol + codex-protocol  (新增 2 个)
apps/acp           → stream-event           (不变)
experimental/codex → stream-event + codex-protocol  (新增 codex-protocol)
```

### 消费者改动清单

| 消费者 | Cargo.toml 改动 | 代码改动 |
|--------|----------------|---------|
| foundation/llm | **无** | **无** |
| foundation/graph-core | **无** | **无** |
| foundation/pregel | **无** | **无** |
| agent-core/think_node | **无** | **无** |
| agent-core/act_executor | **无** | **无** |
| agent-core/subagent_display | **无** | **无** |
| agent-core/runner | 加 `anureo-protocol` | `use stream_event::envelope::EnvelopeState` → `use anureo_protocol::EnvelopeState`; `use stream_event::convert::...` → `use anureo_protocol::...` |
| apps/cli/agent.rs | 加 `anureo-protocol` | `use stream_event::EnvelopeState` → `use anureo_protocol::EnvelopeState` |
| apps/cli/display | **无** | **无** |
| apps/cli/codex_event_builder | 加 `codex-protocol` | `use stream_event::codex::` → `use codex_protocol::` |
| apps/acp | **无** | **无** |
| experimental/codex | 加 `codex-protocol` | `use stream_event::codex::` → `use codex_protocol::` |

**只需改 4 个文件**，其余 7 个消费者完全不受影响。

---

## 6. 迁移步骤

### Step 1: 提取 codex-protocol（零风险，先做）

```
1. 创建 codex-protocol/ crate
2. 移动 stream-event/src/codex.rs → codex-protocol/src/lib.rs
3. 移动 stream-event/tests/codex_test.rs → codex-protocol/tests/codex.rs
4. 从 stream-event/src/lib.rs 删除 `pub mod codex` 和 `pub use codex::CodexEvent`
5. 从 stream-event/Cargo.toml 删除 codex 相关内容
6. 更新 workspace Cargo.toml 添加 codex-protocol
7. 更新 2 个消费者:
   - apps/cli: 加 codex-protocol 依赖, 改 import
   - experimental/codex: 加 codex-protocol 依赖, 改 import
8. cargo test -p stream-event (确认 codex 测试已移走)
9. cargo test -p codex-protocol
10. cargo test -p apps-cli
11. cargo test -p codex
```

**验证点**: `stream-event` 不再包含 codex 模块; `codex-protocol` 独立编译通过

### Step 2: 提取 anureo-protocol（核心步骤）

```
1. 创建 anureo-protocol/ crate (depends on stream-event)
2. 移动 3 个文件:
   - stream-event/src/event.rs     → anureo-protocol/src/event.rs
   - stream-event/src/envelope.rs  → anureo-protocol/src/envelope.rs
   - stream-event/src/convert.rs   → anureo-protocol/src/convert.rs
3. 移动 stream-event/tests/stream_event.rs → anureo-protocol/tests/protocol.rs
4. 更新 anureo-protocol/src/lib.rs:
   pub use event::ProtocolEvent;
   pub use envelope::{to_json, Envelope, EnvelopeState};
   pub use convert::{stream_event_to_protocol_envelope, stream_event_to_format_a, ProtocolEventEnvelope};
5. 从 stream-event/src/lib.rs 删除:
   - pub mod convert / pub mod envelope / pub mod event
   - 对应的 pub use 行
6. 更新 workspace Cargo.toml 添加 anureo-protocol
7. 更新 2 个消费者:
   - agent-core: 加 anureo-protocol 依赖, 改 import
   - apps/cli: 加 anureo-protocol 依赖, 改 import
8. cargo build --workspace
9. cargo test -p stream-event
10. cargo test -p anureo-protocol
11. cargo test -p agent-core
12. cargo test -p apps-cli
```

**验证点**: `stream-event` 不再包含 protocol 模块; `anureo-protocol` 依赖 `stream-event` 并独立编译

### Step 3: 清理

```
1. 删除 stream-event/Cargo.toml 中的 tracing 依赖 (未使用)
2. 修复 convert.rs:360 的编译错误 (use stream_event → use crate)
3. 更新 stream-event/docs/ 文档
4. cargo clippy --workspace --all-targets
5. cargo test --workspace
```

---

## 7. 风险评估

| 风险 | 概率 | 影响 | 缓解 |
|------|:----:|:----:|------|
| Import 路径遗漏 | 中 | 低 | 编译器强制检查 (use 路径不存在 → 编译错误) |
| convert.rs 测试编译错误 | 高 | 低 | Step 3 修复已知 bug (`use stream_event::` → `use crate::`) |
| anureo-protocol 名字冲突 | 低 | 低 | 搜索 workspace 确认无同名 crate |
| 消费者遗漏 | 低 | 中 | `cargo build --workspace` 验证全量编译 |
| git 历史丢失 | 低 | 低 | 使用 `git mv` 保留文件历史 |

---

## 附录: 逐模块归属决策

| 模块 | 行数 | 归属 | 决策理由 |
|------|------|------|----------|
| `message.rs` | 86 | stream-event | MessageChunk 是所有事件的基础类型，不是 LLM 独有 |
| `stream_event.rs` | 306 | stream-event | 核心事件 enum，所有人依赖 |
| `stream_mode.rs` | 57 | stream-event | 配置 enum，与 StreamEvent 绑定 |
| `metadata.rs` | 87 | stream-event | StreamMetadata + CheckpointEvent，与 StreamEvent 绑定 |
| `sender.rs` | 223 | stream-event | StreamEventSink 桥接 MessageChunk → StreamEvent，属于事件层 |
| `writers/stream_writer.rs` | 430 | stream-event | StreamWriter 发送 StreamEvent，属于事件层 |
| `event.rs` | 528 | **anureo-protocol** | ProtocolEvent 是线路格式，不是领域事件 |
| `envelope.rs` | 377 | **anureo-protocol** | EnvelopeState 是线路注入器，只有 runner 用 |
| `convert.rs` | 1,038 | **anureo-protocol** | 转换函数桥接领域→线路，只有 runner 用 |
| `codex.rs` | 426 | **codex-protocol** | 完全独立的 Codex 协议，与 anureo 事件无关 |

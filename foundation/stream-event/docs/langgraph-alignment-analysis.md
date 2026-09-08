# stream-event: LangGraph 对齐分析

> **状态**: 设计提案
> **范围**: `stream-event` crate 及其消费者
> **基准**: LangGraph `main` 分支 (2025-08), `langgraph.types` + `langgraph.stream._types`

---

## 目录

1. [现状分析](#1-现状分析)
2. [LangGraph 架构参考](#2-langgraph-架构参考)
3. [逐维度对比](#3-逐维度对比)
4. [差距清单](#4-差距清单)
5. [消费者影响分析](#5-消费者影响分析)
6. [迁移方案](#6-迁移方案)
7. [风险评估](#7-风险评估)
8. [不在范围内的内容](#8-不在范围内的内容)

---

## 1. 现状分析

### 1.1 Crate 结构

```
stream-event/
├── Cargo.toml                      # 依赖: serde, serde_json, tokio, tracing(未使用)
├── src/
│   ├── lib.rs                      # 模块声明 + re-exports (27 行)
│   ├── stream_event.rs             # StreamEvent<S> 泛型 enum (22 变体, 306 行)
│   ├── event.rs                    # ProtocolEvent wire enum (22 变体, 528 行)
│   ├── convert.rs                  # StreamEvent→ProtocolEvent→Format A 转换 (1038 行)
│   ├── envelope.rs                 # EnvelopeState 信封注入 (377 行)
│   ├── metadata.rs                 # StreamMetadata + CheckpointEvent (87 行)
│   ├── message.rs                  # MessageChunk + StreamSink trait (86 行)
│   ├── sender.rs                   # StreamEventSink 适配器 (223 行)
│   ├── stream_mode.rs              # StreamMode enum (8 变体, 57 行)
│   ├── codex.rs                    # Codex 协议类型 (独立, 426 行)
│   └── writers/
│       ├── mod.rs                  # re-export (2 行)
│       └── stream_writer.rs        # StreamWriter API (430 行)
├── tests/
│   ├── stream_event.rs             # EnvelopeState 集成测试 (163 行)
│   └── codex_test.rs               # Codex 类型集成测试 (273 行)
└── docs/
    └── langgraph-alignment-analysis.md  # 本文档
```

**规模统计**:

| 类别 | 文件数 | 行数 |
|------|--------|------|
| 生产代码 | 13 | ~3,652 |
| 单元测试 (内联) | 13 | ~2,121 |
| 集成测试 | 2 | ~436 |
| **总计** | **15** | **~6,209** |

### 1.2 当前数据流

```
                    anureo (agent-core / pregel)
                             │
                    StreamEvent<S> (泛型, 内部类型)
                             │
                    ┌────────┴────────┐
                    │ convert.rs      │
                    │ (1:1 静态转换)   │
                    └────────┬────────┘
                             │
                    ProtocolEvent (wire enum)
                             │
                    ┌────────┴────────┐
                    │ envelope.rs     │
                    │ to_json()       │
                    │ (注入 session_id,│
                    │  node_id,       │
                    │  event_id)      │
                    └────────┬────────┘
                             │
                    JSON 帧 (在线路上)
                             │
                ┌────────────┼────────────┐
                │            │            │
         CLI event_handler  ACP      Codex bridge
         (giant match)   stream_bridge  event_bridge
```

**特点**:
- 单向管道：`StreamEvent → ProtocolEvent → JSON`，无中间件层
- 两个巨型 match (convert.rs:53-189 和 convert.rs:218-349) 完全 1:1 镜像
- 消费端必须在单个循环中 `match` 所有事件变体

### 1.3 核心类型

#### StreamMode (8 变体)

```rust
pub enum StreamMode {
    Values,      // 节点完成后发送完整状态
    Updates,     // 发送增量更新 + node id
    Messages,    // LLM token 流
    Custom,      // 自定义 JSON
    Checkpoints, // checkpoint 事件
    Tasks,       // 任务开始/结束
    Tools,       // 工具生命周期
    Debug,       // = Checkpoints + Tasks
}
```

#### StreamEvent\<S\> (22 变体)

分为 5 组：

| 组 | 变体 | 说明 |
|----|------|------|
| **状态** | `Values(S)`, `Updates{node_id, state, namespace}` | 图状态快照与增量 |
| **消息** | `Messages{chunk, metadata}` | LLM token 流 |
| **任务** | `TaskStart{node_id, namespace}`, `TaskEnd{node_id, result, namespace}` | 节点执行生命周期 |
| **工具** | `ToolCall`, `ToolStart`, `ToolOutput`, `ToolEnd` | 工具调用全生命周期 |
| **推理框架** | `TotExpand`, `TotEvaluate`, `TotBacktrack`, `GotPlan`, `GotNodeStart/Complete/Failed`, `GotExpand` | ToT/GoT 特有 |
| **元数据** | `Custom(Value)`, `Checkpoint(CheckpointEvent<S>)`, `Usage{...}` | 辅助事件 |

#### ProtocolEvent (22 变体, wire 格式)

与 `StreamEvent` 1:1 对应，但：
- 去掉泛型：所有 `S` 变为 `serde_json::Value`
- 去掉 `namespace`: 不在事件级别携带 (通过 `EnvelopeState.node_id` 间接跟踪)
- 字段重命名: `TaskStart.node_id` → `NodeEnter.id` (payload 字段名不同)
- 合并: `Messages` 的 `kind` 拆分为 `MessageChunk` 和 `ThoughtChunk` 两个变体

#### EnvelopeState (信封注入器)

```rust
pub struct EnvelopeState {
    pub session_id: String,       // 会话 ID (常量)
    pub current_node_id: String,  // 当前节点 span (node_enter 时更新)
    pub node_run_seq: u64,        // 节点运行序号
    pub next_event_id: u64,       // 单调递增事件 ID
}
```

对应 LangGraph 的 `ProtocolEvent.seq` + `ProtocolEvent.params.namespace`。

#### StreamWriter\<S\> (写入器)

```rust
pub struct StreamWriter<S> {
    tx: Option<mpsc::Sender<StreamEvent<S>>>,  // None = no-op
    modes: Arc<HashSet<StreamMode>>,            // 启用的模式
}
```

提供 `emit_*` 方法（async + try_），每个方法内部检查 `modes.contains(...)` 后发送。

#### Namespace 当前表示

```rust
// StreamEvent 各变体
namespace: Option<String>

// StreamMetadata
namespace: Option<String>

// CheckpointEvent
checkpoint_ns: Option<String>
```

---

## 2. LangGraph 架构参考

### 2.1 三层架构

LangGraph 的流式系统分为三层，每层有不同的职责：

```
┌─────────────────────────────────────────────────────────────┐
│ Layer 1: Streaming (Pregel 引擎)                             │
│                                                             │
│   graph.stream(input, stream_mode=[...])                    │
│   ↓                                                         │
│   StreamMode: values | updates | messages | custom |        │
│               checkpoints | tasks | debug                    │
│   ↓                                                         │
│   输出: StreamPart TypedDict {type, ns, data}               │
│                                                             │
│   特点: 原始引擎事件，无加工，单消费者迭代器                    │
└──────────────────────┬──────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────────┐
│ Layer 2: Event Router / StreamMux (协议层)                   │
│                                                             │
│   graph.stream_events(input, version="v3")                  │
│   ↓                                                         │
│   1. 将 Pregel 事件包装为 ProtocolEvent 信封                   │
│   2. 注入 seq (单调序列号，非时间戳)                           │
│   3. 通过 StreamTransformer 管道处理                          │
│   4. 写入主事件日志 + 分派到各投影                              │
│                                                             │
│   ProtocolEvent {                                           │
│       type: "event",                                        │
│       event_id: str,        # 可选                           │
│       seq: int,             # 单调递增                       │
│       method: str,          # StreamMode 值                  │
│       params: {                                             │
│           namespace: list[str],  # 子图路径 tuple             │
│           timestamp: int,        # wall-clock ms              │
│           data: Any,             # 事件 payload               │
│           interrupts: tuple      # HITL 中断 (可选)           │
│       }                                                     │
│   }                                                         │
│                                                             │
│   特点: 可插入 transformer (PII 过滤/metrics/mutation),       │
│         transformer 可 mutate/suppress/schedule             │
└──────────────────────┬──────────────────────────────────────┘
                       │
                       ▼
┌─────────────────────────────────────────────────────────────┐
│ Layer 3: Event Streaming v3 (类型化投影)                      │
│                                                             │
│   stream = graph.stream_events(input, version="v3")         │
│                                                             │
│   投影 (独立迭代器):                                           │
│   ┌─────────────────────┬───────────────────────────────┐   │
│   │ stream              │ 遍历所有协议事件                 │   │
│   │ stream.messages     │ chat model token deltas        │   │
│   │ stream.values       │ 状态快照 + 最终值               │   │
│   │ stream.output       │ 最终输出 (Promise)             │   │
│   │ stream.subgraphs    │ 嵌套子图执行                    │   │
│   │ stream.interrupts   │ HITL 中断 payload              │   │
│   │ stream.interrupted  │ 是否暂停等待人工输入            │   │
│   │ stream.extensions   │ 自定义 transformer 投影         │   │
│   └─────────────────────┴───────────────────────────────┘   │
│                                                             │
│   特点: 多消费者并发、独立迭代器、类型安全投影                   │
└─────────────────────────────────────────────────────────────┘
```

### 2.2 LangGraph 核心类型 (源码摘录)

#### StreamMode

```python
# langgraph/types.py
StreamMode = Literal[
    "values",      # 每步后发送完整状态
    "updates",     # 每步后发送增量更新
    "checkpoints", # checkpoint 创建时发送事件
    "tasks",       # 任务开始/结束时发送事件
    "debug",       # = checkpoints + tasks
    "messages",    # LLM token + metadata 二元组
    "custom",      # 自定义数据
]
```

对比: anureo 有额外的 `Tools` 模式，LangGraph 将其包含在 `tasks` 中。

#### StreamPart v2 (判别联合)

```python
# langgraph/types.py — 每个 StreamPart 是一个 TypedDict
class ValuesStreamPart(TypedDict):
    type: Literal["values"]
    ns: tuple[str, ...]      # namespace tuple (子图路径)
    data: dict[str, Any]     # 状态快照
    interrupts: tuple[Any, ...]  # HITL 中断

class UpdatesStreamPart(TypedDict):
    type: Literal["updates"]
    ns: tuple[str, ...]
    data: dict[str, Any]     # {node_id: update}

class MessagesStreamPart(TypedDict):
    type: Literal["messages"]
    ns: tuple[str, ...]
    data: tuple[AnyMessage, dict[str, Any]]  # (message_chunk, metadata)

class CustomStreamPart(TypedDict):
    type: Literal["custom"]
    ns: tuple[str, ...]
    data: Any

# 联合类型 (类型窄化用)
StreamPart = ValuesStreamPart | UpdatesStreamPart | MessagesStreamPart | ...
```

#### ProtocolEvent (Layer 2 信封)

```python
# langgraph/stream/_types.py
class _ProtocolEventParams(TypedDict):
    namespace: list[str]           # 子图路径
    timestamp: int                 # wall-clock ms (不可靠，非单调)
    data: Any
    interrupts: NotRequired[tuple[Any, ...]]

class ProtocolEvent(TypedDict):
    type: Literal["event"]
    event_id: NotRequired[str]     # 线路字段
    seq: NotRequired[int]          # 单调序列号 (排序用这个)
    method: str                    # StreamMode 值
    params: _ProtocolEventParams
```

#### StreamTransformer (中间件抽象)

```python
# langgraph/stream/_types.py
class StreamTransformer(ABC):
    """扩展点: 观察流经 StreamMux 的协议事件，构建类型化投影."""

    requires_async: ClassVar[bool] = False
    supports_sync: ClassVar[bool] = False
    required_stream_modes: ClassVar[tuple[str, ...]] = ()
    before_builtins: ClassVar[bool] = False  # 是否在内置 transformer 之前运行

    def __init__(self, scope: tuple[str, ...] = ()) -> None:
        self.scope = scope  # 操作的 namespace

    @abstractmethod
    def init(self) -> dict[str, Any]:
        """返回投影字典，键成为 run.extensions."""
        ...

    def process(self, event: ProtocolEvent) -> bool:
        """处理事件 (同步). 返回 True 保留, False 抑制."""
        raise NotImplementedError

    async def aprocess(self, event: ProtocolEvent) -> bool:
        """处理事件 (异步). 默认委托给 process."""
        return self.process(event)

    def finalize(self) -> None: ...     # 正常结束时调用
    def fail(self, err) -> None: ...    # 异常结束时调用

    def schedule(self, coro, *, on_error="log") -> Task:
        """调度异步任务，生命周期由 StreamMux 管理."""
        ...
```

#### 内置 Transformer 示例

| Transformer | 输入 | 输出投影 |
|-------------|------|----------|
| `ValuesTransformer` | values 事件 | `stream.values` (状态快照迭代器 + 最终值 Promise) |
| `MessagesTransformer` | messages 事件 | `stream.messages` (token delta 迭代器) |
| `OutputTransformer` | values 事件 | `stream.output` (最终输出 Promise) |
| `SubgraphTransformer` | tasks + namespace 事件 | `stream.subgraphs` (嵌套 run stream) |
| `LifecycleTransformer` | tasks 事件 | `stream.interrupted` + `stream.interrupts` |
| `ToolCallTransformer` | messages 事件 | tool call 投影 (从消息中推导工具调用) |

### 2.3 LangGraph 的版本演进

| 版本 | API | 返回类型 | 特点 |
|------|-----|---------|------|
| v1 | `stream(stream_mode=...)` | `Iterator[dict]` | 原始事件字典，无类型 |
| v2 | `stream(stream_mode=..., version="v2")` | `Iterator[StreamPart]` | TypedDict 判别联合 |
| v3 | `stream_events(version="v3")` | `GraphRunStream` | 类型化投影 + transformer 管道 |

---

## 3. 逐维度对比

### 3.1 StreamMode

| 维度 | LangGraph | anureo | 差异 |
|------|-----------|------|------|
| 变体数 | 7 | 8 | anureo 多 `Tools` |
| `Tools` 模式 | 无（合并到 tasks/custom） | 有 | **anureo 更细粒度** |
| `debug` 语义 | = checkpoints + tasks | = checkpoints + tasks + tools | **anureo 隐含 tools** |
| 序列化 | `Literal` 字符串 | `enum` + serde derive | **anureo 编译期安全** |
| 多模式同时 | `stream_mode=[...]` → `(mode, data)` 元组 | `HashSet<StreamMode>` + `is_mode_enabled()` | **概念对齐** |
| 序列化测试 | 无（Python） | 8 变体 roundtrip | anureo 有 ✅ |

**结论**: `StreamMode` 设计已对齐。`Tools` 额外变体是 anureo 的改进（工具事件是一等公民，不需要从 messages 中推导）。

### 3.2 事件信封 (Envelope)

| 维度 | LangGraph | anureo | 差异 |
|------|-----------|------|------|
| 信封类型 | `ProtocolEvent` (TypedDict) | `EnvelopeState` + `Envelope` | **概念对应** |
| 序列号 | `seq: int` (单调，明确不用时间戳) | `event_id: u64` (单调递增) | **设计一致** ✅ |
| 排序可靠性 | 文档明确: `seq` > `timestamp` | 隐含: `event_id` 递增 | LangGraph 显式 |
| 会话 ID | 无独立信封字段 | `session_id: String` | **anureo 更完整** |
| 节点追踪 | `params.namespace: list[str]` | `node_run_seq` → `run-{id}-{seq}` | LangGraph 用路径，anureo 用序号 |
| 注入策略 | StreamMux 统一注入 | `EnvelopeState::inject_into` (`or_insert_with` 不覆盖) | **设计一致** ✅ |
| 不覆盖已有 | 默认行为 | 显式 `or_insert_with` + 测试 | anureo 有测试 ✅ |

**结论**: 信封设计核心一致。差异在 namespace 表示方式（tuple vs 序号）。

### 3.3 Namespace (子图路由)

| 维度 | LangGraph | anureo | 差距 |
|------|-----------|------|------|
| 类型 | `tuple[str, ...]` / `list[str]` | `Option<String>` | **重大差距** |
| 表达能力 | 完整子图路径: `("parent:task-1", "child:task-2")` | 单层字符串或 None | 无法表达嵌套 |
| 空值语义 | `()` = 根图 | `None` = 根图 | 概念对应 |
| 序列化 | JSON array: `["parent:t1", "child:t2"]` | JSON string: `"sub"` | 不兼容 |
| 子图路由 | 每个 subgraph 事件自动携带路径前缀 | 手动传递 `Option<String>` | LangGraph 自动化 |
| checkpoint_ns | `params.namespace` 覆盖 | 独立字段 `checkpoint_ns: Option<String>` | anureo 分离 |

**结论**: namespace 是最大的结构性差距。LangGraph 的 tuple 表示能表达任意深度子图嵌套，anureo 的 `Option<String>` 只能表达单层。

### 3.4 事件转换

| 维度 | LangGraph | anureo | 差距 |
|------|-----------|------|------|
| 转换方式 | StreamTransformer 管道 (可插拔) | `convert.rs` 硬编码 1:1 match | **重大差距** |
| 可扩展性 | 运行时注册 transformer | 编译期固定 | **重大差距** |
| 事件 mutation | transformer 可 mutate 事件 | 无 | **缺失** |
| 事件 suppress | `process()` 返回 False | 无 | **缺失** |
| 异步处理 | `aprocess()` + `schedule()` | 无 | **缺失** |
| transformer 顺序 | `before_builtins` 控制 | N/A | — |
| 事件投影 | 内置 + 自定义 transformer | 无 | **缺失** |

**结论**: 转换层是最大的功能差距。LangGraph 有完整的中间件管道，anureo 只有静态转换。

### 3.5 消费端模型

| 维度 | LangGraph | anureo | 差距 |
|------|-----------|------|------|
| 消费方式 | 类型化投影 (`stream.messages` 等) | 单循环 giant match | **重大差距** |
| 多消费者 | broadcast channel (各投影独立迭代器) | 单消费者 mpsc | **重大差距** |
| 类型安全 | TypedDict + 类型窄化 | `match` 手动分派 | anureo 也安全 ✅ |
| 并发消费 | 多个 `async for` 并行 | 串行处理 | **差距** |
| 子流发现 | `stream.subgraphs` 自动发现嵌套图 | 无 | **缺失** |

### 3.6 消息流 (Messages)

| 维度 | LangGraph | anureo | 差异 |
|------|-----------|------|------|
| 数据结构 | `(message_chunk, metadata)` 二元组 | `MessageChunk { content, kind }` + `StreamMetadata` | anureo 更结构化 ✅ |
| Thinking 区分 | 通过 message type (AIMessageChunk) | `MessageChunkKind::Thinking` 显式变体 | **anureo 更好** ✅ |
| metadata 内容 | `langgraph_node`, `langgraph_step` 等 | `anureo_node`, `namespace` | 对应 |
| Sink 抽象 | `get_stream_writer()` 从 config 获取 | `StreamSink` trait + `StreamEventSink` | 对应 |
| 热路径优化 | 无特殊处理 | `try_send` (非阻塞) + 零中间 channel | **anureo 更优** ✅ |

### 3.7 工具事件

| 维度 | LangGraph | anureo | 差异 |
|------|-----------|------|------|
| 模式 | 无独立 `Tools` 模式 | `StreamMode::Tools` 独立模式 | **anureo 更好** ✅ |
| 生命周期事件 | 从 messages 推导 (ToolCallTransformer) | `ToolCall/ToolStart/ToolOutput/ToolEnd` 原生变体 | **anureo 更好** ✅ |
| raw_result | 无 | `ToolEnd.raw_result: Option<String>` | **anureo 独有** ✅ |

### 3.8 推理框架事件

| 维度 | LangGraph | anureo | 差异 |
|------|-----------|------|------|
| ToT 事件 | 无内置（需用 custom mode） | `TotExpand/Evaluate/Backtrack` 原生 | **anureo 独有** ✅ |
| GoT 事件 | 无内置 | `GotPlan/NodeStart/Complete/Failed/Expand` 原生 | **anureo 独有** ✅ |
| 用法 | `custom` 模式 + 自定义 data | 编译期类型安全 | **anureo 更好** ✅ |

### 3.9 Checkpoint 事件

| 维度 | LangGraph | anureo | 差异 |
|------|-----------|------|------|
| 格式 | `StateSnapshot` (同 `get_state()` 返回) | `CheckpointEvent<S>` 自定义 | 设计不同 |
| 触发 | `stream_mode="checkpoints"` | `StreamMode::Checkpoints` | 一致 ✅ |
| checkpoint_ns | `params.namespace` 统一 | 独立字段 `checkpoint_ns` | anureo 分离 |

---

## 4. 差距清单

按影响程度排序：

### 4.1 结构性差距

| ID | 差距 | 影响 | LangGraph 对应 |
|----|------|------|---------------|
| **G1** | namespace 用 `Option<String>` 而非 `Vec<String>` | 无法表达子图嵌套路径 | `namespace: list[str]` |
| **G2** | 无 StreamTransformer 中间件层 | 无法在运行时插入事件处理 (PII/metrics/mutation) | `StreamTransformer` ABC |
| **G3** | 无类型化投影 | 消费端必须 giant match，无法并发消费 | `stream.messages` / `stream.values` 等 |
| **G4** | 无事件 suppress 能力 | 所有事件必须发送到消费者 | `process()` 返回 False |

### 4.2 功能性差距

| ID | 差距 | 影响 | LangGraph 对应 |
|----|------|------|---------------|
| **G5** | 无 HITL interrupt 事件 | 无法支持 human-in-the-loop | `stream.interrupts` / `stream.interrupted` |
| **G6** | 无 transformer 异步调度 | 无法在事件管道中做异步操作 (外部 API 调用等) | `schedule()` |
| **G7** | 无子图发现机制 | 消费端无法自动发现嵌套图执行 | `stream.subgraphs` |
| **G8** | 无版本化 API | 破坏性变更无迁移路径 | `version="v1"/"v2"/"v3"` |

### 4.3 非差距项 (anureo 已对齐或更优)

- ✅ `StreamMode` 8 变体 (比 LangGraph 多 `Tools`)
- ✅ `event_id` / `seq` 单调递增
- ✅ `or_insert_with` 不覆盖已有字段
- ✅ `reply_envelope` 不推进状态
- ✅ `try_send` 热路径优化 (比 LangGraph 更优)
- ✅ `MessageChunkKind::Thinking` 显式区分 (比 LangGraph 更好)
- ✅ `ToolCall/Start/Output/End` 完整工具生命周期 (比 LangGraph 更好)
- ✅ ToT/GoT 原生事件变体 (LangGraph 无)
- ✅ 泛型 `StreamEvent<S>` (编译期类型安全, LangGraph 只有 `Any`)

---

## 5. 消费者影响分析

### 5.1 消费者全景

基于代码搜索，stream-event crate 的外部消费者分布如下：

```
                    StreamEvent<S>
                         │
        ┌────────────────┼────────────────────┐
        │                │                    │
   agent-core       foundation           apps
        │                │                    │
   ┌────┴──────┐   ┌─────┴─────┐      ┌──────┴──────┐
   │ think_node│   │ pregel/   │      │ cli/        │
   │ act_exec  │   │ runtime   │      │ event_handler│
   │ plan_node │   │ graph-core│      │ acp/        │
   │ runner    │   │ run_ctx   │      │ stream_bridge│
   └───────────┘   └───────────┘      │ codex/      │
                                      │ event_builder│
                                      └─────────────┘

                    ProtocolEvent
                         │
        ┌────────────────┼────────────────┐
        │                │                │
   agent-core        apps/cli         apps/acp
   runner.rs         agent.rs         stream_bridge.rs

                    StreamWriter
                         │
              ┌──────────┴──────────┐
              │                     │
        pregel/node.rs        graph-core/
        (创建)                 run_context.rs
                              (from_context)
```

### 5.2 各消费者详情

#### agent-core (核心生产者 + 转换者)

| 文件 | 依赖类型 | 用法 | 改动敏感度 |
|------|---------|------|-----------|
| `agent/react/think_node.rs` | `StreamEvent::Messages`, `ToolCall`, `Usage`; `StreamMode::Messages/Tools/Debug` | 通过 `StreamEventSink` 发送事件; 检查模式 | **中** |
| `agent/react/act_executor.rs` | `StreamEvent::ToolStart/End/Custom`; `StreamMode::Tools/Debug/Custom` | 通过 `StreamWriter` 发送工具事件 | **中** |
| `agent/react/plan_node.rs` | `StreamMode::Custom` | 检查 Custom 模式 | 低 |
| `agent/react/execute_engine.rs` | `StreamMode::Custom` | 检查 Custom 模式 | 低 |
| `run/runner.rs` | `stream_event_to_protocol_envelope`, `stream_event_to_format_a`, `EnvelopeState` | 核心转换入口; 管理 envelope 状态 | **高** |
| `subagent_display.rs` | `StreamEvent` 全变体 | 格式化显示 | **中** |

#### foundation (基础设施)

| 文件 | 依赖类型 | 用法 | 改动敏感度 |
|------|---------|------|-----------|
| `pregel/runtime.rs` | `StreamMode::Values/Updates/Checkpoints` | 图执行循环中发送状态事件 | 中 |
| `pregel/node.rs` | `StreamWriter`, `StreamMode` | 从 channel 创建 writer | 中 |
| `graph-core/run_context.rs` | `StreamWriter`, `StreamMode` | `from_context()` 创建 writer; 便捷 emit 方法 | 中 |

#### apps (消费端)

| 文件 | 依赖类型 | 用法 | 改动敏感度 |
|------|---------|------|-----------|
| `cli/display/event_handler.rs` | `StreamEvent` 全 22 变体 | giant match 分派显示 | **高** (P3 目标) |
| `cli/run/agent.rs` | `EnvelopeState`, `to_json` | 创建/管理 envelope; 测试转换 | 中 |
| `cli/envelope.rs` | `EnvelopeState` | 封套管理辅助 | 低 |
| `acp/stream_bridge.rs` | `TypedAnyStreamEvent` → `StreamUpdate` | ACP 协议桥接 | 中 |
| `cli/codex_event_builder.rs` | `codex.rs` 全部类型 | 构建 Codex 事件 | 低 (独立) |

### 5.3 namespace 使用现状

搜索结果显示：

| 位置 | namespace 值 | 说明 |
|------|-------------|------|
| `think_node.rs:90` | `namespace: None` | 硬编码 None |
| `act_executor.rs` | 未设置 | 不使用 namespace |
| `sender.rs:81` | `self.namespace.clone()` | 从 StreamEventSink 传入 |
| `convert.rs:226-241` | 序列化到 JSON | 保留字段 |
| **实际使用场景** | **几乎全部为 None** | **子图路由功能未实际使用** |

**结论**: namespace 字段当前几乎全部为 `None`，子图路由功能在设计层面存在但未实际使用。这降低了 Phase 1 (namespace 升级) 的紧迫性。

### 5.4 改动影响矩阵

下表展示每个迁移阶段对各消费者的影响：

| 消费者 | Phase 1 (namespace) | Phase 2 (transformer) | Phase 3 (projection) |
|--------|:-------------------:|:---------------------:|:--------------------:|
| think_node.rs | **改** (None → root()) | 不改 | 不改 |
| act_executor.rs | **改** (None → root()) | 不改 | 不改 |
| runner.rs | **改** (EnvelopeState) | **改** (插入 pipeline) | 可选改 |
| event_handler.rs | **改** (match namespace) | 不改 | **可选迁移** |
| stream_bridge.rs | 不改 | 不改 | **可选迁移** |
| run_context.rs | **改** (StreamWriter 构造) | 不改 | 不改 |
| pregel/runtime.rs | 不改 | 不改 | 不改 |
| pregel/node.rs | 不改 | 不改 | 不改 |

---

## 6. 迁移方案

### 6.1 总体策略

```
Phase 2 (transformer) ── 零破坏，立即可用
    │
    ├──> Phase 1 (namespace) ── 类型变更，机械替换
    │
    └──> Phase 3 (projection) ── 消费端可选迁移
```

**推荐执行顺序**: P2 → P1 → P3

理由：
- P2 纯增量，不改变任何现有类型，可立即提供价值
- P1 是破坏性变更但当前 namespace 全部为 None，影响面可控
- P3 是消费端改进，不强制迁移，按需进行

### 6.2 Phase 2: StreamTransformer 中间件层

**目标**: 引入事件处理管道，支持运行时插入 transformer

#### 新增文件

`stream-event/src/transform.rs` (~150 行)

#### 核心 API

```rust
/// 处理后的事件决策
pub enum TransformResult {
    /// 保留事件，继续管道
    Keep,
    /// 丢弃事件（不发送给消费者）
    Suppress,
    /// 替换为另一个事件
    Replace(ProtocolEvent),
}

/// 流事件变换器，对应 LangGraph StreamTransformer.
pub trait StreamTransformer: Send + Sync {
    /// 该 transformer 的 namespace scope
    fn scope(&self) -> &Namespace;

    /// 处理同步事件
    fn process(&mut self, event: &mut ProtocolEvent) -> TransformResult;

    /// 该 transformer 需要哪些 StreamMode
    fn required_stream_modes(&self) -> &[StreamMode] { &[] }

    /// 运行结束时清理
    fn finalize(&mut self) {}
}

/// 事件变换管道
pub struct TransformPipeline {
    transformers: Vec<Box<dyn StreamTransformer>>,
}
```

#### 集成点

在 `runner.rs` 的 `stream_event_to_protocol_envelope` 之后插入 pipeline 调用：

```rust
// runner.rs — 当前
let protocol_ev = stream_event_to_protocol_event(&ev)?;
let envelope = stream_event_to_json(&protocol_ev, &mut state)?;

// runner.rs — 之后
let protocol_ev = stream_event_to_protocol_event(&ev)?;
let mut protocol_ev = pipeline.process(&mut protocol_ev);  // 新增
let envelope = match protocol_ev {
    TransformResult::Keep | TransformResult::Replace(_) => 
        stream_event_to_json(&protocol_ev, &mut state)?,
    TransformResult::Suppress => continue,  // 跳过此事件
};
```

#### 内置 Transformer 示例

```rust
/// PII 过滤 transformer (示例)
pub struct PiiFilter { scope: Namespace }
impl StreamTransformer for PiiFilter {
    fn scope(&self) -> &Namespace { &self.scope }
    fn process(&mut self, event: &mut ProtocolEvent) -> TransformResult {
        if let ProtocolEvent::MessageChunk { content, .. } = event {
            *content = redact_pii(content);
        }
        TransformResult::Keep
    }
}

/// Metrics 收集 transformer (示例)
pub struct MetricsCollector { 
    scope: Namespace,
    event_count: AtomicU64,
}
impl StreamTransformer for MetricsCollector {
    fn scope(&self) -> &Namespace { &self.scope }
    fn process(&mut self, _event: &mut ProtocolEvent) -> TransformResult {
        self.event_count.fetch_add(1, Ordering::Relaxed);
        TransformResult::Keep
    }
}
```

#### 文件改动

| 文件 | 改动 |
|------|------|
| `stream-event/src/transform.rs` | 新增 (~150 行) |
| `stream-event/src/lib.rs` | 新增 `pub mod transform;` + re-exports |
| `stream-event/Cargo.toml` | 删除 `tracing` (未使用) |
| `agent-core/src/run/runner.rs` | 插入 pipeline 调用 (~5 行) |

#### 测试计划

- `TransformPipeline` 空 pipeline 透传
- 单 transformer Keep / Suppress / Replace 路径
- 多 transformer 链式处理 (A Keep → B Suppress)
- `finalize()` 调用验证
- trait object 动态分发 (`Box<dyn StreamTransformer>`)

#### 风险

- **低**: 纯增量，不改变现有类型
- 需确认 `runner.rs` 中 pipeline 的所有权模型 (可变引用 vs Clone)

### 6.3 Phase 1: Namespace 升级

**目标**: 将 `Option<String>` 升级为 `Namespace` (Vec\<String\>)

#### 新增类型

`stream-event/src/namespace.rs` (~80 行)

```rust
/// 子图路径，对应 LangGraph 的 namespace tuple.
/// 空 = 根图; ["parent:task-1"] = 一级子图;
/// ["parent:task-1", "child:task-2"] = 二级嵌套.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Namespace(pub Vec<String>);

impl Namespace {
    /// 根图 namespace (空路径)
    pub fn root() -> Self { Self(Vec::new()) }

    /// 是否为根图
    pub fn is_root(&self) -> bool { self.0.is_empty() }

    /// 追加一个路径段 (mutable)
    pub fn push(&mut self, segment: impl Into<String>) {
        self.0.push(segment.into());
    }

    /// 创建子 namespace (immutable)
    pub fn child(&self, segment: impl Into<String>) -> Self {
        let mut v = self.0.clone();
        v.push(segment.into());
        Namespace(v)
    }

    /// 路径切片
    pub fn as_slice(&self) -> &[String] { &self.0 }

    /// 序列化为 JSON array (兼容 LangGraph 格式)
    /// `[]` = 根图, `["parent:t1"]` = 一级子图
    pub fn to_json_array(&self) -> serde_json::Value {
        serde_json::to_value(&self.0).unwrap_or(serde_json::Value::Array(vec![]))
    }
}

impl From<Option<String>> for Namespace {
    fn from(opt: Option<String>) -> Self {
        match opt {
            Some(s) => Self(vec![s]),
            None => Self::root(),
        }
    }
}

impl From<Namespace> for Option<String> {
    fn from(ns: Namespace) -> Self {
        ns.0.into_iter().next()
    }
}
```

#### 改动点

| 文件 | 变更 | 说明 |
|------|------|------|
| `stream_event.rs` | `namespace: Option<String>` → `Namespace` (3 处: Updates, TaskStart, TaskEnd) | 类型变更 |
| `metadata.rs` | `StreamMetadata.namespace` → `Namespace`; `CheckpointEvent.checkpoint_ns` → `Namespace` | 类型变更 |
| `sender.rs` | `namespace: Option<String>` → `Namespace` | 类型变更 |
| `writers/stream_writer.rs` | `emit_*` 方法签名变更 | namespace 参数 |
| `convert.rs` | 序列化 namespace 为 JSON array | 格式变更 |
| `envelope.rs` | `EnvelopeState` 内部用 Namespace | 可选 |

#### 消费者改动

| 消费者 | 当前代码 | 改为 |
|--------|---------|------|
| `think_node.rs:90` | `namespace: None` | `namespace: Namespace::root()` |
| `act_executor.rs` | 未设置 namespace | 不变 (默认 root) |
| `run_context.rs` | `StreamWriter::new(tx, modes)` | 增加 namespace 参数 |
| `convert.rs` format_a | `"namespace": namespace` (string/null) | `"namespace": namespace.to_json_array()` |

#### 序列化格式变更

```json
// 之前 (Option<String>)
{"TaskStart": {"node_id": "think", "namespace": null}}

// 之后 (Namespace)
{"TaskStart": {"node_id": "think", "namespace": []}}
```

#### 测试计划

- `Namespace::root()` 序列化为 `[]`
- `Namespace::root().is_root()` == true
- `child()` 不可变追加
- `From<Option<String>>` 转换 (None → root, Some("x") → ["x"])
- roundtrip serde: `Namespace` → JSON array → `Namespace`

#### 风险

- **中**: 类型变更影响 ~8 个文件，但改动是机械性的
- 序列化格式变更 (null → []) 可能破坏已有 JSON 消费者
- 需要同步更新所有 `namespace: None` 为 `Namespace::root()`

#### 回退方案

`Namespace` 实现 `From<Option<String>>`，可在迁移期间保持兼容。但序列化格式变更 (null → []) 不可回退。

### 6.4 Phase 3: 类型化投影

**目标**: 为消费端提供独立的事件流迭代器

#### 新增类型

`stream-event/src/projection.rs` (~120 行)

```rust
use tokio::sync::broadcast;
use crate::ProtocolEvent;

/// 事件投影，对应 LangGraph v3 的 GraphRunStream.
///
/// 消费端可以订阅特定类型的事件流，无需在单个循环中 match.
pub struct EventProjection {
    tx: broadcast::Sender<ProtocolEvent>,
}

impl EventProjection {
    pub fn new(buffer: usize) -> Self {
        let (tx, _) = broadcast::channel(buffer);
        Self { tx }
    }

    /// 发送事件到所有订阅者
    pub fn emit(&self, event: ProtocolEvent) {
        let _ = self.tx.send(event);
    }

    /// 订阅消息流 (MessageChunk + ThoughtChunk)
    pub fn messages(&self) -> EventStream {
        EventStream::filtered(self.tx.subscribe(), |ev| {
            matches!(ev,
                ProtocolEvent::MessageChunk { .. } |
                ProtocolEvent::ThoughtChunk { .. }
            )
        })
    }

    /// 订阅工具流 (ToolCall + ToolStart + ToolOutput + ToolEnd)
    pub fn tools(&self) -> EventStream { ... }

    /// 订阅状态流 (Values + Updates)
    pub fn state(&self) -> EventStream { ... }

    /// 订阅任务流 (NodeEnter + NodeExit)
    pub fn tasks(&self) -> EventStream { ... }

    /// 订阅推理流 (ToT + GoT 全部)
    pub fn reasoning(&self) -> EventStream { ... }

    /// 订阅全部事件
    pub fn all(&self) -> EventStream {
        EventStream::raw(self.tx.subscribe())
    }
}

/// 类型化事件流
pub struct EventStream {
    rx: broadcast::Receiver<ProtocolEvent>,
    filter: Box<dyn Fn(&ProtocolEvent) -> bool + Send + Sync>,
}

impl EventStream {
    pub async fn next(&mut self) -> Option<ProtocolEvent> {
        loop {
            match self.rx.recv().await {
                Ok(ev) if (self.filter)(&ev) => return Some(ev),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }
}
```

#### 消费端改造示例

```rust
// apps/cli/run/agent.rs — 之前
while let Some(ev) = stream.recv().await {
    match ev {
        ProtocolEvent::MessageChunk { content, .. } => print!("{}", content),
        ProtocolEvent::ToolEnd { name, result, .. } => eprintln!("[{}] {}", name, result),
        // ... 22 个变体
    }
}

// apps/cli/run/agent.rs — 之后
let projection = EventProjection::new(256);
// pipeline → projection.emit(protocol_ev);

let mut msg_stream = projection.messages();
let mut tool_stream = projection.tools();

tokio::select! {
    ev = msg_stream.next() => {
        if let Some(ProtocolEvent::MessageChunk { content, .. }) = ev {
            print!("{}", content);
        }
    }
    ev = tool_stream.next() => {
        if let Some(ProtocolEvent::ToolEnd { name, result, .. }) = ev {
            eprintln!("[{}] {}", name, result);
        }
    }
}
```

#### 设计决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| Channel 类型 | `broadcast` | 支持多消费者，对应 LangGraph 的多投影 |
| Buffer 大小 | 可配置 (默认 256) | 避免慢消费者阻塞快消费者 |
| 慢消费者策略 | `recv()` 返回 `Err(Lagged)` | 对应 LangGraph 的 StreamChannel 行为 |
| 过滤方式 | closure predicate | 灵活，可组合自定义投影 |
| 是否强制迁移 | 否 | 现有 giant match 模式继续工作 |

#### 风险

- **低**: 纯新增，不影响现有代码
- `broadcast` channel 的 Lagged 问题：慢消费者可能丢失事件
- 增加了 `tokio::sync::broadcast` 依赖（tokio 已是依赖）

### 6.5 各阶段汇总

| 阶段 | 破坏性 | 新增代码 | 改动文件 | 新增依赖 | 主要收益 |
|------|:------:|---------:|:--------:|----------|----------|
| **P2** transformer | 无 | ~150 行 | 2 (新增 + runner) | 无 | 中间件能力 (PII/metrics) |
| **P1** namespace | **高** | ~80 行 | ~10 | 无 | 子图路由就绪 |
| **P3** projection | 无 | ~120 行 | 消费端按需 | 无 | 消费端解耦 |

---

## 7. 风险评估

### 7.1 技术风险

| 风险 | 概率 | 影响 | 缓解措施 |
|------|:----:|:----:|----------|
| P1 序列化格式变更破坏消费者 | 中 | 高 | 版本化 JSON; 文档变更; 渐进迁移 |
| P2 pipeline 增加延迟 | 低 | 低 | pipeline 仅在需要时创建; 空 pipeline 透传 |
| P3 broadcast Lagged 丢事件 | 中 | 中 | 配置足够大的 buffer; 消费者处理 Lagged |
| 大量 `None → root()` 遗漏 | 中 | 低 | 编译器强制 (Namespace 不是 Option) |

### 7.2 设计风险

| 风险 | 概率 | 影响 | 缓解措施 |
|------|:----:|:----:|----------|
| Transformer trait 设计不适合实际场景 | 低 | 中 | 先实现 2-3 个内置 transformer 验证 |
| Namespace 类型过度工程化 | 低 | 低 | 当前虽未使用，但 LangGraph 已验证此模式 |
| Projection 引入不必要的复杂度 | 中 | 中 | 设为可选，不强制迁移 |

### 7.3 迁移风险

| 风险 | 概率 | 影响 | 缓解措施 |
|------|:----:|:----:|----------|
| P1 影响范围超出预期 | 中 | 中 | 充分的 grep + 编译器验证 |
| 测试覆盖不足导致回归 | 中 | 高 | 每阶段补充对应测试 |
| 消费者迁移不完整导致行为不一致 | 低 | 低 | P3 不强制迁移，新旧模式共存 |

---

## 8. 不在范围内的内容

以下 LangGraph 特性**不在本次对齐范围内**，原因如下：

| 特性 | 原因 |
|------|------|
| HITL interrupt 事件 (G5) | 需要图执行引擎层面的 interrupt/resume 支持，超出 stream-event crate 范围 |
| Transformer 异步调度 (G6) | `schedule()` 需要 async runtime 绑定，且当前无使用场景 |
| 子图自动发现 (G7) | 需要 Pregel 引擎配合，超出 stream-event crate 范围 |
| 版本化 API (G8) | LangGraph 的 v1/v2/v3 是历史演进产物，anureo 可以直接用正确的版本 |
| `langgraph_sdk` 的 `messages-tuple` 模式 | SDK 特有，不需要 |
| `stream_transformers` 编译时注册 | LangGraph 通过 `compile(stream_transformers=...)` 实现，需要图引擎配合 |

---

## 附录 A: LangGraph 源码参考

| 文件 | 内容 | URL |
|------|------|-----|
| `langgraph/types.py` | StreamMode, StreamPart TypedDicts | [GitHub](https://github.com/langchain-ai/langgraph/blob/main/libs/langgraph/langgraph/types.py) |
| `langgraph/stream/_types.py` | ProtocolEvent, StreamTransformer | [GitHub](https://github.com/langchain-ai/langgraph/blob/main/libs/langgraph/langgraph/stream/_types.py) |
| `langgraph/stream/run_stream.py` | GraphRunStream (v3) | [GitHub](https://github.com/langchain-ai/langgraph/blob/main/libs/langgraph/langgraph/stream/run_stream.py) |
| `langgraph/pregel/protocol.py` | Pregel.stream/astream overloads | [GitHub](https://github.com/langchain-ai/langgraph/blob/main/libs/langgraph/langgraph/pregel/protocol.py) |
| `docs/docs/concepts/streaming.md` | 流式概念文档 | [GitHub](https://github.com/langchain-ai/langgraph/blob/main/docs/docs/concepts/streaming.md) |
| `tests/test_stream_v2.py` | v2 StreamPart 测试 | [GitHub](https://github.com/langchain-ai/langgraph/blob/main/libs/langgraph/tests/test_stream_v2.py) |

## 附录 B: 词汇表

| 术语 | LangGraph | anureo | 说明 |
|------|-----------|------|------|
| 流模式 | `StreamMode` (Literal) | `StreamMode` (enum) | 控制发送哪些类型的事件 |
| 流事件 | `StreamPart` (TypedDict) | `StreamEvent<S>` (泛型 enum) | Pregel 引擎发出的原始事件 |
| 协议事件 | `ProtocolEvent` (TypedDict) | `ProtocolEvent` (enum) | Layer 2 信封包装后的事件 |
| 信封状态 | `StreamMux` 内部状态 | `EnvelopeState` (struct) | 跟踪 session/node/event 序号 |
| 事件变换器 | `StreamTransformer` (ABC) | (待实现) `StreamTransformer` (trait) | 中间件抽象 |
| 事件投影 | `GraphRunStream` | (待实现) `EventProjection` | 类型化消费端迭代器 |
| 命名空间 | `namespace: tuple[str, ...]` | `namespace: Option<String>` → `Namespace` | 子图路径 |
| 流写入器 | `StreamWriter` (config callable) | `StreamWriter<S>` (struct) | 节点内发送事件的 API |

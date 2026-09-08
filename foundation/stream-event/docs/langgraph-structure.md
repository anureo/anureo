# LangGraph 模块与文件结构

> **基准**: LangGraph v1.2.7 (`libs/langgraph/`, 2025-08 main 分支)
> **来源**: GitHub 源码直接抓取
> **用途**: 为 anureo stream-event 对齐提供结构参考

---

## 目录

1. [顶层结构](#1-顶层结构)
2. [核心包内部结构](#2-核心包内部结构)
3. [关键模块详解](#3-关键模块详解)
4. [stream/ 子包深度分析](#4-stream-子包深度分析)
5. [分层职责总结](#5-分层职责总结)
6. [设计特征](#6-设计特征)
7. [与 anureo stream-event 结构对比](#7-与-anureo-stream-event-结构对比)

---

## 1. 顶层结构

LangGraph 采用 **monorepo + 多包** 架构：

```
langgraph/                              # GitHub monorepo
├── libs/
│   ├── langgraph/                      # ★ 核心包 (pip install langgraph, v1.2.7)
│   ├── langgraph-checkpoint/           # 独立包: checkpoint 持久化抽象 + 内存实现
│   ├── langgraph-sdk/                  # 独立包: 远程部署 SDK (LangGraph Cloud)
│   ├── langgraph-prebuilt/             # 独立包: 预构建 agent 组件 (create_react_agent 等)
│   └── langchain-protocol/             # 独立包: 线路协议类型 (ProtocolEvent wire shape)
├── docs/
│   └── docs/
│       ├── concepts/
│       │   └── streaming.md            # 流式概念文档
│       └── ...
└── tests/
    └── libs/langgraph/
        ├── test_stream_v2.py           # v2 StreamPart 测试
        └── ...
```

### 核心包依赖关系

```toml
# libs/langgraph/pyproject.toml
[project]
name = "langgraph"
version = "1.2.7"
dependencies = [
    "langchain-core>=1.4.7,<2",         # 底层 Runnable 接口
    "langgraph-checkpoint>=4.1.0,<5.0.0", # BaseCheckpointSaver
    "langgraph-sdk>=0.4.2,<0.5.0",
    "langgraph-prebuilt>=1.1.0,<1.2.0",
    "xxhash>=3.5.0",                    # 确定性 ID 生成 (xxh3_128_hexdigest)
    "pydantic>=2.7.4",                  # Schema 验证
]
# langchain-protocol 通过 langchain-core 间接依赖
```

### 多包发布边界

| 包 | 职责 | 是否可独立使用 |
|----|------|:---:|
| `langgraph` | 图构建 + 执行引擎 + 流式基础设施 | 是 |
| `langgraph-checkpoint` | checkpoint 保存/恢复抽象 + 内存实现 | 是 |
| `langgraph-sdk` | 远程调用 LangGraph Cloud | 是 |
| `langgraph-prebuilt` | 预构建 agent (ReAct, tool-calling) | 是 |
| `langchain-protocol` | 线路协议 (ProtocolEvent wire format) | 是 |

---

## 2. 核心包内部结构

```
libs/langgraph/langgraph/
│
├── __init__.py                         # 顶层 re-exports
├── types.py                            # ★ 核心类型中心 (StreamMode, StreamPart, Command, Interrupt)
├── constants.py                        # START, END, TAG_NOSTREAM, TAG_HIDDEN
├── errors.py                           # GraphInterrupt, GraphRecursionError, ErrorCode
├── warnings.py                         # LangGraphDeprecatedSinceV10/V11
├── utils.py                            # 工具函数
│
├── graph/                              # ── 图构建 API (用户直接使用) ──
│   ├── __init__.py                     # re-export: StateGraph, MessageGraph, MessagesState
│   ├── state.py                        # ★ StateGraph (~75K chars, 主图构建器)
│   └── message.py                      # MessageGraph, add_messages reducer
│
├── pregel/                             # ── Pregel 执行引擎 ──
│   ├── __init__.py                     # re-export: Pregel, NodeBuilder
│   ├── main.py                         # ★ Pregel (~176K chars) + NodeBuilder
│   ├── protocol.py                     # PregelProtocol / StreamProtocol (抽象接口)
│   ├── retry.py                        # RetryPolicy
│   ├── call.py                         # call() 上下文 (prebuilt 用)
│   └── utility.py                      # RunnableOp 子类
│
├── stream/                             # ── 流式基础设施 (v3 核心) ──
│   ├── __init__.py                     # re-export 全部公共 API
│   ├── _types.py                       # ★ ProtocolEvent, StreamTransformer ABC
│   ├── _convert.py                     # convert_to_protocol_event (StreamPart → ProtocolEvent)
│   ├── _mux.py                         # ★ StreamMux (中央事件分派器)
│   ├── run_stream.py                   # ★ GraphRunStream (消费端入口)
│   ├── stream_channel.py               # StreamChannel (单消费者 drainable queue)
│   └── transformers.py                 # ★ 9 个内置 Transformer
│
├── checkpoint/                         # ── Checkpoint re-export (from langgraph-checkpoint 包) ──
│   └── __init__.py
│
├── _internal/                          # ── 私有实现 (下划线前缀, 不对外暴露) ──
│   ├── _constants.py                   # CONF, TASKS, INTERRUPT, NS_SEP, NS_END
│   ├── _fields.py                      # get_cached_annotated_keys, get_update_as_tuples
│   ├── _retry.py                       # default_retry_on
│   ├── _typing.py                      # MISSING, DeprecatedKwargs
│   ├── _cache.py                       # default_cache_key
│   └── _serde.py                       # 序列化工具
│
├── api/                                # 外部 API 层 (langgraph-api 用)
└── config/                             # 配置访问
    └── __init__.py                     # get_stream_writer(), get_stream_writer_async()
```

### 文件规模分布

| 文件/目录 | 大约行数/字符数 | 职责 |
|-----------|----------------|------|
| `types.py` | ~33K chars (~900 行) | 所有公共类型定义 |
| `pregel/main.py` | ~176K chars (~5000+ 行) | Pregel 引擎核心 |
| `graph/state.py` | ~75K chars (~2000+ 行) | StateGraph 构建 |
| `stream/transformers.py` | ~40K chars (~1100+ 行) | 9 个内置 Transformer |
| `stream/_mux.py` | ~21K chars (~600 行) | StreamMux |
| `stream/run_stream.py` | ~24K chars (~650 行) | GraphRunStream |
| `stream/_types.py` | ~13K chars (~330 行) | ProtocolEvent + StreamTransformer |
| `stream/stream_channel.py` | ~12K chars (~350 行) | StreamChannel |
| `stream/_convert.py` | ~0.8K chars (~30 行) | StreamPart → ProtocolEvent |
| `errors.py` | ~7.5K chars (~200 行) | 异常类型 |
| `constants.py` | ~1.5K chars (~50 行) | 公共常量 |
| `_internal/` | ~15K chars 总计 | 私有工具 |

---

## 3. 关键模块详解

### 3.1 `types.py` — 唯一类型中心

所有公共流式类型定义集中在一个文件：

```python
# langgraph/types.py

# ── Stream Mode ──
StreamMode = Literal[
    "values", "updates", "messages", "custom",
    "checkpoints", "tasks", "debug"
]

# ── StreamPart v2 判别联合 (每个是一个 TypedDict) ──
class ValuesStreamPart(TypedDict):
    type: Literal["values"]
    ns: tuple[str, ...]
    data: dict[str, Any]
    interrupts: tuple[Any, ...]

class UpdatesStreamPart(TypedDict):
    type: Literal["updates"]
    ns: tuple[str, ...]
    data: dict[str, Any]

class MessagesStreamPart(TypedDict):
    type: Literal["messages"]
    ns: tuple[str, ...]
    data: tuple[AnyMessage, dict[str, Any]]  # (chunk, metadata)

class CustomStreamPart(TypedDict): ...
class CheckpointStreamPart(TypedDict): ...
class TasksStreamPart(TypedDict): ...
class TaskResultStreamPart(TypedDict): ...
class DebugStreamPart(TypedDict): ...

# 联合类型 (类型窄化用)
StreamPart = ValuesStreamPart | UpdatesStreamPart | MessagesStreamPart | ...
```

其他公共类型：

| 类型 | 说明 |
|------|------|
| `Command` | 图控制指令 (goto, update, resume) |
| `Interrupt` | HITL 中断 payload |
| `StateSnapshot` | 状态快照 (同 `get_state()` 返回) |
| `GraphOutput` | 泛型输出类型 |
| `StreamWriter` | 节点内写入器类型别名 (`Callable[[Any], None]`) |

### 3.2 `pregel/protocol.py` — 执行协议接口

通过 `@overload` 提供三个版本的 stream/astream：

```python
# pregel/protocol.py

class PregelProtocol(Runnable, Generic[StateT, ContextT, InputT, OutputT]):
    """编译后的图必须实现的协议."""

    # v1: 原始字典流 (向后兼容)
    @overload
    def stream(self, ..., version: Literal["v1"] = ...) -> Iterator[dict[str, Any] | Any]: ...

    # v2: 类型化 StreamPart 流
    @overload
    def stream(self, ..., version: Literal["v2"]) -> Iterator[StreamPart[StateT, OutputT]]: ...

    # v3: 类型化投影 + transformer 管道
    def stream_events(self, ..., version: Literal["v3"]) -> GraphRunStream: ...
```

### 3.3 `pregel/main.py` — Pregel 引擎

```python
# pregel/main.py

class NodeBuilder:
    """节点构建器 DSL."""
    def subscribe_only(self, *channels): ...
    def subscribe_to(self, *channels): ...
    def read_from(self, *channels): ...
    def do(self, func): ...
    def write_to(self, *channels): ...
    def meta(self, *tags, **metadata): ...
    def add_retry_policies(self, *policies): ...
    def add_cache_policy(self, policy): ...
    def set_timeout(self, timeout): ...
    def build(self) -> PregelNode: ...

class Pregel(Runnable, Generic[...]):
    """编译后的图运行器."""
    # ── 图结构查询 ──
    def get_graph(self, config=None, *, xray=False) -> DrawableGraph: ...
    def get_subgraphs(self, ...): ...
    def stream_channels_list(self) -> Sequence[str]: ...
    def stream_channels_asis(self) -> str | Sequence[str]: ...

    # ── 状态访问 ──
    def get_state(self, config) -> StateSnapshot: ...
    def get_state_history(self, config): ...
    def update_state(self, config, values, as_node=...): ...

    # ── 执行 ──
    def stream(self, input, config, *, stream_mode=None, version="v1", ...): ...
    def astream(self, ...): ...
    def stream_events(self, input, config, *, version="v3", ...): ...

    # ── v3 内部入口 ──
    def _pregel_stream_v3(self, ...): ...
```

### 3.4 `graph/state.py` — StateGraph 构建器

```python
# graph/state.py

class StateGraph(Generic[StateT]):
    """用户构建图的主要 API."""
    def add_node(self, node: str, action: Callable): ...
    def add_edge(self, start_key: str, end_key: str): ...
    def add_conditional_edges(self, start_key, condition_func): ...
    def add_serializers(self, serializers): ...
    def compile(
        self,
        checkpointer: BaseCheckpointSaver | None = None,
        *,
        interrupt_before=None,
        interrupt_after=None,
        transformers: list[TransformerFactory] | None = None,
    ) -> CompiledStateGraph: ...
```

注意 `compile(transformers=[...])` — transformer 在编译期注册。

### 3.5 `config/__init__.py` — 运行时配置访问

```python
# config/__init__.py

def get_stream_writer() -> StreamWriter:
    """从 RunnableConfig 中获取 StreamWriter.
    在节点函数内部调用."""
    ...

def get_stream_writer_async() -> StreamWriter: ...
```

---

## 4. stream/ 子包深度分析

`stream/` 是 LangGraph v1.0+ 重构的核心，6 个文件职责严格分离：

### 4.1 分层架构

```
        Layer 1: Pregel 引擎原始事件
               (StreamPart)
                     │
                     ▼
    ┌──────────────────────────────────────┐
    │ _convert.py                          │
    │ convert_to_protocol_event()          │
    │ StreamPart → ProtocolEvent           │
    │ (注入 timestamp, namespace)           │
    └───────────────┬──────────────────────┘
                    │
                    ▼
    ┌──────────────────────────────────────┐
    │ _mux.py — StreamMux                  │
    │ 1. 分配 seq (单调递增)                │
    │ 2. 遍历 transformer pipeline         │
    │ 3. 写入 main event log               │
    │ 4. 自动注入到 StreamChannel          │
    └───────────────┬──────────────────────┘
                    │
        ┌───────────┼───────────────┐
        │           │               │
        ▼           ▼               ▼
   ValuesTransformer  MessagesTransformer  ...
        │           │               │
        ▼           ▼               ▼
   StreamChannel   StreamChannel   StreamChannel
   (values)        (messages)      (custom)
        │           │               │
        ▼           ▼               ▼
    ┌──────────────────────────────────────┐
    │ run_stream.py — GraphRunStream       │
    │ 消费端统一入口                        │
    │ .values  .messages  .output          │
    │ .interrupted  .interrupts            │
    │ .subgraphs  .extensions              │
    └──────────────────────────────────────┘
```

### 4.2 文件职责矩阵

| 文件 | 行数 | 职责 | 核心导出 |
|------|------|------|---------|
| `_types.py` | ~330 | 协议定义 + transformer 抽象 | `ProtocolEvent`, `StreamTransformer` |
| `_convert.py` | ~30 | StreamPart → ProtocolEvent 转换 | `convert_to_protocol_event` |
| `_mux.py` | ~600 | 中央事件分派器 + transformer 管道 | `StreamMux` |
| `stream_channel.py` | ~350 | 单消费者 drainable queue | `StreamChannel` |
| `transformers.py` | ~1100 | 9 个内置投影 transformer | `ValuesTransformer` 等 |
| `run_stream.py` | ~650 | 消费端入口 (caller-driven pump) | `GraphRunStream` |

### 4.3 `_types.py` — 协议层类型

```python
# stream/_types.py

class _ProtocolEventParams(TypedDict):
    namespace: list[str]           # 子图路径
    timestamp: int                 # wall-clock ms (不可靠，非单调)
    data: Any
    interrupts: NotRequired[tuple[Any, ...]]

class ProtocolEvent(TypedDict):
    type: Literal["event"]
    event_id: NotRequired[str]     # 线路字段 (snake_case)
    seq: NotRequired[int]          # 单调序列号 (排序用这个, 不用 timestamp)
    method: str                    # StreamMode 值 ("values", "messages", ...)
    params: _ProtocolEventParams


class StreamTransformer(ABC):
    """扩展点: 观察流经 StreamMux 的协议事件, 构建类型化投影."""

    # ── 类属性 (声明能力) ──
    requires_async: ClassVar[bool] = False
    supports_sync: ClassVar[bool] = False
    required_stream_modes: ClassVar[tuple[str, ...]] = ()
    before_builtins: ClassVar[bool] = False

    def __init__(self, scope: tuple[str, ...] = ()) -> None:
        self.scope = scope

    # ── 投影初始化 ──
    @abstractmethod
    def init(self) -> dict[str, Any]:
        """返回投影字典. 键成为 run.extensions.
        StreamChannel 实例自动被 mux wiring."""

    # ── 事件处理 (sync + async 双通道) ──
    def process(self, event: ProtocolEvent) -> bool:
        """返回 True 保留, False 抑制."""
        raise NotImplementedError

    async def aprocess(self, event: ProtocolEvent) -> bool:
        """默认委托给 process."""
        return self.process(event)

    # ── 生命周期 ──
    def finalize(self) -> None: ...
    async def afinalize(self) -> None: self.finalize()
    def fail(self, err: BaseException) -> None: ...
    async def afail(self, err: BaseException) -> None: self.fail(err)

    # ── 异步任务调度 ──
    def schedule(self, coro, *, on_error="log") -> asyncio.Task:
        """调度异步任务. 生命周期由 mux 管理."""


def transformer_requires_async(t: StreamTransformer) -> bool:
    """检测 transformer 是否需要 async runtime."""
    ...
```

### 4.4 `_convert.py` — 层 1→2 桥接

```python
# stream/_convert.py (~30 行)

def convert_to_protocol_event(part: StreamPart) -> ProtocolEvent:
    """将 v2 StreamPart 转换为 ProtocolEvent."""
    part_dict = cast(dict[str, Any], part)
    params: _ProtocolEventParams = {
        "namespace": list(part_dict["ns"]),      # ns tuple → namespace list
        "timestamp": int(time.time() * 1000),     # wall-clock ms
        "data": part_dict["data"],
    }
    if "interrupts" in part_dict:
        params["interrupts"] = part_dict["interrupts"]
    return {
        "type": "event",
        "method": part_dict["type"],              # StreamMode 值
        "params": params,
    }
```

### 4.5 `_mux.py` — StreamMux 中央调度器

```python
# stream/_mux.py

TransformerFactory = Callable[["tuple[str, ...]"], StreamTransformer]
"""工厂函数: 接收 scope (namespace tuple), 返回 transformer 实例."""


class StreamMux:
    """中央事件分派器.

    Owns the main event log and routes events through a transformer pipeline.
    StreamChannels with a name discovered in transformer projections are
    auto-wired so that every push() also injects a ProtocolEvent into the
    main log.
    """

    def __init__(
        self,
        scope: tuple[str, ...],
        transformer_factories: list[TransformerFactory],
        *,
        is_async: bool,
    ): ...

    def push(self, part: StreamPart) -> None:
        """Layer 1 → Layer 2 入口."""
        # 1. convert_to_protocol_event(part) → ProtocolEvent
        event = convert_to_protocol_event(part)
        # 2. 分配 seq (单调递增)
        event["seq"] = self._next_seq()
        # 3. 遍历 transformer pipeline
        for transformer in self._transformers:
            if not transformer.process(event):
                return  # suppressed
        # 4. 写入 main event log
        self._log.append(event)
        # 5. 自动注入到有 name 的 StreamChannel

    def _make_child(self, scope: tuple[str, ...]) -> "StreamMux":
        """为子图创建 child mux (继承 transformer pipeline)."""

    def bind_pump(self, pump: Callable[[], bool]) -> None:
        """绑定同步 pump 回调 (caller-driven pulling)."""
```

### 4.6 `stream_channel.py` — 单消费者队列

```python
# stream/stream_channel.py

class StreamChannel(Generic[T]):
    """Single-consumer drainable queue for streaming events.

    - 构造时传入 name → push() 自动注入 ProtocolEvent 到 main log
    - 构造时不传 name → local-only, items 仅对 in-process 消费者可见
    - 单消费者: 第二次 __iter__/__aiter__ 调用会 raise
    - 用 tee(n)/atee(n) 实现 fan-out
    - Starts unbound — 需要 mux 调用 _bind(is_async) 后才能迭代
    """

    def __init__(
        self,
        name: str | None = None,     # None = local-only
        *, max_size: int = 0,        # 0 = unbounded
    ): ...

    def push(self, item: T) -> None: ...
    def close(self) -> None: ...
    def fail(self, err: BaseException) -> None: ...

    # sync 迭代
    def __iter__(self) -> Iterator[T]: ...
    # async 迭代
    def __aiter__(self) -> AsyncIterator[T]: ...

    # fan-out
    def tee(self, n: int) -> tuple["StreamChannel[T]", ...]: ...
    async def atee(self, n: int) -> tuple["StreamChannel[T]", ...]: ...
```

### 4.7 `transformers.py` — 9 个内置 Transformer

```python
# stream/transformers.py (~1100 行)

class ValuesTransformer(StreamTransformer):
    """Capture values events as a drainable stream of state snapshots.
    Provides the run.values projection.
    run.output / run.interrupted / run.interrupts 由 run stream 直接跟踪."""

class UpdatesTransformer(StreamTransformer):
    """Capture updates events. Provides the updates stream."""

class MessagesTransformer(StreamTransformer):
    """Capture messages events, projecting them as chat model token deltas.
    Provides the run.messages projection.
    Uses ChatModelStream from langchain-core."""

class CustomTransformer(StreamTransformer):
    """Capture custom events. Provides the custom stream."""

class TasksTransformer(StreamTransformer):
    """Capture task start/end events. Provides the tasks stream."""

class CheckpointsTransformer(StreamTransformer):
    """Capture checkpoint events. Provides the checkpoints stream."""

class DebugTransformer(StreamTransformer):
    """Merge checkpoints + tasks events into a combined debug stream."""

class LifecycleTransformer(StreamTransformer):
    """Track graph lifecycle (interrupted, interrupts).
    Provides run.interrupted and run.interrupts."""

class SubgraphTransformer(StreamTransformer):
    """Manage nested subgraph execution streams.
    Provides run.subgraphs projection.
    Creates child StreamMux instances for each subgraph."""
```

### 4.8 `run_stream.py` — GraphRunStream

```python
# stream/run_stream.py

class GraphRunStream:
    """Sync run stream with caller-driven pumping.

    The caller's iteration on any projection (values, messages,
    raw events, or output) drives the graph forward. No background
    thread is used — the caller's for loop is the pump.

    Projections are single-consumer — iterating a projection
    drives the single shared pump. Use tee/atee for fan-out.
    """

    # ── 延迟初始化的投影属性 ──
    # 由 transformer 在 init() 中创建

    @property
    def values(self) -> StreamChannel[dict]:
        """状态快照流 (from ValuesTransformer)."""

    @property
    def messages(self) -> StreamChannel:
        """Token delta 流 (from MessagesTransformer)."""

    @property
    def output(self) -> Promise:
        """最终输出 (from ValuesTransformer, resolves on completion)."""

    @property
    def interrupted(self) -> bool:
        """是否暂停等待人工输入 (from LifecycleTransformer)."""

    @property
    def interrupts(self) -> list[Interrupt]:
        """HITL 中断列表 (from LifecycleTransformer)."""

    @property
    def subgraphs(self) -> StreamChannel:
        """嵌套子图流 (from SubgraphTransformer)."""

    @property
    def extensions(self) -> Mapping[str, Any]:
        """自定义 transformer 投影."""

    # ── 迭代 ──
    def __iter__(self) -> Iterator[ProtocolEvent]:
        """遍历所有协议事件 (drives the pump)."""

    def __enter__(self) -> Self: ...
    def __exit__(self, *exc) -> None: ...


class AsyncGraphRunStream:
    """Async 版本. API 对称."""

class SubgraphRunStream:
    """子图 sync 版本 (pump 由父 GraphRunStream 驱动)."""

class AsyncSubgraphRunStream:
    """子图 async 版本."""
```

---

## 5. 分层职责总结

```
┌─────────────────────────────────────────────────────────────────┐
│ graph/state.py — StateGraph                                     │
│   用户构建图的 API: add_node(), add_edge(), compile()            │
│   compile(transformers=[...]) 在编译期注册 transformer           │
│   输出: CompiledStateGraph (Pregel 子类)                        │
└──────────────────────────┬──────────────────────────────────────┘
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│ pregel/main.py — Pregel                                         │
│   图执行引擎                                                     │
│   .stream(version="v1")  → Iterator[dict]                      │
│   .stream(version="v2")  → Iterator[StreamPart]                │
│   .stream_events(version="v3") → GraphRunStream                │
│                                                                 │
│   _pregel_stream_v3():                                          │
│     1. 创建 StreamMux (注册 transformer factories)              │
│     2. 创建 GraphRunStream (绑定 pump)                          │
│     3. 执行 Pregel loop → 产出 StreamPart                       │
│     4. mux.push(part) 驱动 transformer pipeline                 │
└──────────────────────────┬──────────────────────────────────────┘
                           ▼
┌─────────────────────────────────────────────────────────────────┐
│ stream/ — 流式基础设施                                          │
│                                                                 │
│   _convert.py     StreamPart → ProtocolEvent                    │
│   _mux.py         StreamMux: transformer pipeline + event log   │
│   transformers    9 个内置投影 (Values/Messages/Custom/...)     │
│   stream_channel  StreamChannel: 单消费者 drainable queue       │
│   run_stream      GraphRunStream: 消费端入口                    │
│                                                                 │
│   数据流:                                                       │
│   StreamPart → convert → ProtocolEvent → mux.push()             │
│     → transformer.process() → StreamChannel.push()              │
│     → GraphRunStream.messages.next()                            │
└─────────────────────────────────────────────────────────────────┘
```

---

## 6. 设计特征

1. **`types.py` 是唯一的类型中心** — 所有公共类型集中在一个文件，不像 anureo 分散在 `stream_event.rs` + `event.rs` + `metadata.rs` + `message.rs` 四个文件

2. **`stream/` 是独立子包** — 6 个文件职责严格分离：
   - 协议定义 (`_types.py`) / 转换 (`_convert.py`) / 调度 (`_mux.py`)
   - 通道 (`stream_channel.py`) / 投影 (`transformers.py`) / 消费端 (`run_stream.py`)

3. **`_internal/` 隔离私有实现** — 所有内部常量和工具函数用下划线前缀目录，公共 API 中不暴露

4. **`pregel/` 和 `stream/` 解耦** — Pregel 引擎只产出 `StreamPart`（Layer 1），`stream/` 层负责包装、变换和投影（Layer 2-3）

5. **checkpoint 独立为包** — `langgraph-checkpoint` 可单独发布和复用，anureo 的 checkpoint 目前耦合在 foundation 中

6. **Transformer 在编译期注册** — `graph.compile(transformers=[...])` 而非运行时动态添加

7. **Caller-driven pump** — GraphRunStream 不使用后台线程，消费端的 `for` 循环就是 pump，驱动画执行前进

8. **单消费者 StreamChannel + tee/atee fan-out** — 每个投影是单消费者队列，需要 fan-out 时显式调用 `tee(n)`

9. **`_convert.py` 极简 (~30 行)** — StreamPart → ProtocolEvent 的转换逻辑极简，因为 `StreamPart` 已经是结构化的 TypedDict

10. **版本化协议通过 `@overload`** — `stream()` 的 v1/v2 和 `stream_events()` 的 v3 通过 Python `@overload` 在类型层面区分

---

## 7. 与 anureo stream-event 结构对比

### 7.1 目录结构对比

```
LangGraph (Python)                        anureo (Rust)
─────────────────                         ───────────
langgraph/                                stream-event/
├── types.py              (类型中心)      ├── stream_event.rs    (StreamEvent<S>)
├── constants.py                          ├── event.rs           (ProtocolEvent)
├── errors.py                             ├── stream_mode.rs     (StreamMode)
│                                         ├── metadata.rs        (StreamMetadata)
├── graph/                                ├── message.rs         (MessageChunk)
│   ├── state.py         (StateGraph)     ├── envelope.rs        (EnvelopeState)
│   └── message.py                        ├── convert.rs         (转换函数, ~1038 行)
├── pregel/                               ├── sender.rs          (StreamEventSink)
│   ├── main.py          (Pregel)         ├── codex.rs           (Codex 协议)
│   └── protocol.py                       └── writers/
├── stream/               (流式基础设施)       └── stream_writer.rs (StreamWriter)
│   ├── _types.py        (ProtocolEvent)
│   ├── _convert.py      (转换, ~30 行)
│   ├── _mux.py          (StreamMux)
│   ├── run_stream.py    (GraphRunStream)
│   ├── stream_channel.py
│   └── transformers.py  (9 个 transformer)
└── _internal/            (私有实现)
```

### 7.2 职责分布对比

| 职责 | LangGraph | anureo | 差异 |
|------|-----------|------|------|
| 类型定义 | `types.py` 单文件集中 | 分散在 4 个文件 | **LangGraph 更集中** |
| 事件转换 | `_convert.py` (~30 行) | `convert.rs` (~1038 行) | **anureo 更臃肿** |
| 中间件层 | `_mux.py` + `transformers.py` | 无 | **anureo 缺失** |
| 消费端模型 | `run_stream.py` (多投影) | 无 (giant match) | **anureo 缺失** |
| 事件通道 | `stream_channel.py` (单消费者 queue) | `mpsc::Sender` | 不同设计 |
| 私有实现 | `_internal/` 子包 | 无 (全部 public) | **LangGraph 更严格** |
| Checkpoint | 独立包 `langgraph-checkpoint` | 内嵌在 `metadata.rs` | **LangGraph 可复用** |

### 7.3 类型集中度对比

LangGraph 的 `types.py` 集中了**所有**公共类型（~900 行）：

```
StreamMode, StreamPart (8 个 TypedDict), ProtocolEvent,
Command, Interrupt, StateSnapshot, GraphOutput, StreamWriter
```

anureo 的类型分散在 5 个文件中：

```
stream_event.rs   → StreamEvent<S> (22 变体)
event.rs          → ProtocolEvent (22 变体)
metadata.rs       → StreamMetadata, CheckpointEvent<S>
message.rs        → MessageChunk, MessageChunkKind, StreamSink
stream_mode.rs    → StreamMode
envelope.rs       → EnvelopeState, Envelope
```

### 7.4 convert 层复杂度对比

LangGraph 的 `_convert.py` 只有 ~30 行，因为 `StreamPart` 已经是 TypedDict，转换只需字段重命名：

```python
def convert_to_protocol_event(part: StreamPart) -> ProtocolEvent:
    params = {
        "namespace": list(part_dict["ns"]),
        "timestamp": int(time.time() * 1000),
        "data": part_dict["data"],
    }
    return {"type": "event", "method": part_dict["type"], "params": params}
```

anureo 的 `convert.rs` 有 ~1038 行，因为需要：
1. `StreamEvent<S>` → `ProtocolEvent` (泛型擦除 + 字段重命名, ~190 行 match)
2. `StreamEvent<S>` → Format A JSON (Debug 格式化 + 序列化, ~137 行 match)
3. `ProtocolEventEnvelope` 类型定义
4. `stream_event_to_protocol_envelope` 函数

### 7.5 结构化建议

基于 LangGraph 的结构，anureo stream-event 可考虑：

| 改进方向 | 当前 | 参考 LangGraph | 优先级 |
|---------|------|---------------|--------|
| 类型集中 | 分散 4 个文件 | `types.py` 单文件 | 低 (Rust 习惯分文件) |
| convert 拆分 | 单文件 1038 行 | 拆分 convert/ + protocol_envelope/ | 中 |
| 中间件层 | 无 | `_mux.py` + `transformers.py` | **高** (已在迁移方案 P2) |
| 消费端投影 | 无 | `run_stream.py` | **高** (已在迁移方案 P3) |
| 私有隔离 | 无 | `_internal/` 子目录 | 低 |
| Checkpoint 独立 | 内嵌 | 独立包 | 低 (workspace 已有拆分) |

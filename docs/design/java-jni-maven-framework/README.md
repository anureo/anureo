# anureo Java JNI Framework 与 Maven 发布方案

> **状态**: 提案
> **日期**: 2026-08-21
> **范围**: 为 anureo 增加可嵌入 JVM 的 Java SDK、JNI runtime 与 Maven 发布链路
> **相关代码**: `agent/agent-core/src/run/runner.rs`、`apps/cli`、`apps/server`、`apps/acp`
> **交叉参考**: [Tauri 桌面应用集成方案](../tauri-integration.md)、[开发环境](../../dev/dev-environment.md)

---

## 1. 背景与目标

anureo 目前以 CLI、HTTP server 和 ACP agent 的形式提供能力。Java 生态中的调用方若要使用 anureo，只能管理外部进程或自行对接 HTTP/ACP；无法把 agent runtime 作为 Maven dependency 嵌入自身 JVM。

本方案增加一套 Java framework，使调用方可通过 Maven 引入 anureo，并在进程内调用 Rust runtime：

```xml
<dependency>
  <groupId>dev.anureo</groupId>
  <artifactId>anureo-java-api</artifactId>
  <version>0.5.0</version>
</dependency>
```

目标：

1. 为 Java 17+ 提供稳定、异步、可取消的 anureo agent API。
2. 将 Rust runtime 作为按平台发布的 native library 自动加载。
3. 复用现有 agent/tool/skill/config/llm 能力，而不将 CLI、Server 或 ACP 的应用层行为带入嵌入式模式。
4. 建立 Maven Local、Nexus Central 等仓库的可重复发布流程。
5. 以最小权限作为 embedded runtime 的默认安全策略。

非目标：

- 不把 Rust 内部 struct、trait、Tokio future 逐一映射到 Java。
- 不承诺 `agent-core` 的内部事件、runner 或配置结构是 Java ABI。
- 首版不支持 Android，也不将所有实验性 agent 模式作为稳定 API 发布。
- 首版不替代已有 HTTP/ACP 集成方式。

## 2. 现状与设计决策

现有 `agent::run::run_agent_from_config` 已统一构建和运行 React、DUP、ToT、GoT runner，并可以输出 `TypedAnyStreamEvent` 与接收 `RunCancellation`。它适合作为嵌入式执行的内部接线点；CLI 的参数解析/终端输出和 Server 的 HTTP/WebSocket 生命周期不适合成为 JNI API。

| 维度 | 决定 | 说明 |
|---|---|---|
| Java baseline | Java 17 | 使用 records、sealed types 与 `AutoCloseable`，兼顾 LTS 覆盖率 |
| native bridge | JNI | 由 Rust `jni` crate 导出最小方法集合 |
| Rust 稳定层 | 新增 `anureo-sdk-core` | 隔离 Java SDK 与 `agent-core` 内部实现 |
| 事件传输 | JSON envelope | JNI 只传字符串，Java 层转换为强类型 event |
| 并发模型 | 进程级单例 Tokio runtime | 禁止每个调用创建 runtime 或嵌套 `block_on` |
| native 分发 | 一个 natives JAR 内嵌支持平台库 | 一行 Maven dependency；首版优先可靠性 |
| 默认 agent | React | 其他模式以后续 experimental feature 形式开放 |
| 默认权限 | 最小权限 | 禁止 shell、写入、MCP，需 `ToolPolicy` 显式开启 |

## 3. 总体架构

```text
Java application
  │ Maven dependency
  ▼
anureo-java-api.jar
  ├─ anureoClient / anureoRequest / anureoRun / anureoEvent
  ├─ CompletableFuture、事件回调、取消与异常模型
  └─ NativeLoader：选择、验证、解压、加载动态库
  │ JNI
  ▼
anureo_jni.dll / libanureo_jni.so / libanureo_jni.dylib
  ├─ JVM 边界、句柄表、callback 线程附着
  └─ 进程内 Tokio runtime
  │
  ▼
anureo-sdk-core
  ├─ 稳定请求/结果/事件/错误 façade
  └─ 基于 agent::run::run_agent_from_config 执行
  │
  ▼
agent / tool / skill / config / llm / checkpoint
```

新增目录结构：

```text
foundation/anureo-sdk-core/          # Rust 语言无关 embedding façade
apps/jni/                          # cdylib 与 JNI 边界
java/
├── anureo-java-api/                 # Java 公共 API 与 NativeLoader
├── anureo-java-natives/             # 动态库资源与打包规则
├── anureo-java-bom/                 # 可选的版本约束 BOM
├── build.gradle.kts
└── settings.gradle.kts
```

`anureo-sdk-core` 与 `apps/jni` 均加入根 `Cargo.toml` workspace；`anureo-sdk-core` 不得依赖 `apps/cli`、`apps/server` 或 `apps/acp`。

## 4. Rust Embedding API

`foundation/anureo-sdk-core` 负责把易变的内部运行模型收敛为稳定接口：

```rust
pub struct anureoRequest {
    pub message: String,
    pub working_directory: PathBuf,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub api_key: Option<SecretString>,
    pub agent_mode: AgentMode,
    pub tools: ToolPolicy,
    pub session_id: Option<String>,
}

pub enum anureoEvent {
    RunStarted { run_id: String },
    TextDelta { text: String },
    ToolCallStarted { name: String, arguments: Value },
    ToolCallFinished { name: String, result: Value },
    Warning { code: String, message: String },
    Completed { reply: String, reasoning: Option<String> },
    Cancelled,
    Failed { code: anureoErrorCode, message: String },
}

pub struct anureoEngine;

impl anureoEngine {
    pub fn new(config: anureoEngineConfig) -> Result<Self, anureoError>;

    pub async fn run(
        &self,
        request: anureoRequest,
        cancellation: CancellationToken,
        on_event: impl FnMut(anureoEvent) + Send + 'static,
    ) -> Result<anureoResult, anureoError>;
}
```

该 crate 的实现职责：

1. 把 SDK 请求转换为现有 `ReactBuildConfig`、`RunCmd` 和 `RunParams`。
2. 将 `TypedAnyStreamEvent` 转换成稳定的 `anureoEvent`。
3. 将 `CancellationToken` 接到既有 `RunCancellation`。
4. 归一化 build、run、config、LLM/network 等错误，避免 Rust 错误字符串成为 Java API。
5. 第一版仅提供 React；DUP、ToT、GoT 不进入默认公开 API。

## 5. JNI 边界与生命周期

Java 不保存 Rust 指针。native 层以不透明 `long` handle 管理所有对象：

```text
Java anureoClient       ↔ clientHandle: long
Java anureoRun          ↔ runHandle: long
Java listener         ↔ JNI GlobalRef
Rust ClientRegistry   ↔ handle → Arc<anureoEngine>
Rust RunRegistry      ↔ handle → CancellationToken / task state
```

建议 JNI 导出最小集合：

```java
final class NativeBindings {
  static native long createClient(String configJson);
  static native long startRun(long clientHandle, String requestJson,
                              anureoEventListener listener);
  static native String awaitRun(long runHandle);
  static native void cancelRun(long runHandle);
  static native void closeRun(long runHandle);
  static native void closeClient(long clientHandle);
  static native String nativeVersion();
}
```

JNI 方法是 Rust `cdylib` 的唯一公开 ABI。每个导出函数须在 FFI 边界使用 `catch_unwind`，将 panic 转换为 Java `anureoNativeException`；任何 Rust panic、Java exception 或错误对象均不得跨越 JNI 边界。

### 5.1 线程与 callback

native runtime 在 JVM 进程内通过 `OnceLock` 创建一个 Tokio multi-thread runtime。禁止在每次 run 创建 runtime，禁止在 JNI 回调中嵌套 `block_on`。

Rust worker 向 Java listener 发送事件时：

1. 通过 `AttachCurrentThread` 获取 `JNIEnv`。
2. 使用 `GlobalRef` 保存 listener，绝不保存 local reference。
3. 调用 `onEvent(String eventJson)`。
4. 检查并清除 pending Java exception。
5. listener 抛异常时记录 `tracing`，将 run 转换为失败或取消。

回调通过有界队列和 dispatcher 线程派发，防止慢 Java listener 长时间阻塞 agent。`text_delta` 可合并；工具事件和终态事件不可丢弃。

### 5.2 取消与资源回收

`anureoRun.cancel()` 调用 native `cancelRun`，触发协作式 `CancellationToken`：等待模型请求和工具执行抵达可中断点。它不是 hard kill，Java 文档必须说明外部进程/网络调用不会保证立即结束。

`anureoClient`、`anureoRun` 均实现 `AutoCloseable`，使用者通过 `try-with-resources` 关闭。可使用 `Cleaner` 做泄漏兜底，但不得依赖 finalizer/Cleaner 作为确定性的资源释放机制。

## 6. Java API

Java 公开模块：

```text
dev.anureo.api
├── anureoClient / anureoClientBuilder
├── anureoRequest / anureoRun / anureoResult
├── anureoEvent / anureoEventListener
├── ToolPolicy
├── anureoException
├── anureoConfigurationException
├── anureoNativeLoadException
└── anureoCancelledException
```

示例：

```java
try (anureoClient anureo = anureoClient.builder()
    .apiKey(System.getenv("OPENAI_API_KEY"))
    .baseUrl(System.getenv("OPENAI_BASE_URL"))
    .workingDirectory(Path.of("C:/work/demo"))
    .model("gpt-5.2")
    .build()) {

  anureoRun run = anureo.run(
      anureoRequest.of("分析这个项目并给出重构建议"),
      event -> System.out.println(event.type() + ": " + event.data())
  );

  anureoResult result = run.await();
  System.out.println(result.reply());
}
```

异步入口提供 `CompletableFuture<anureoResult>`。事件跨 JNI 时使用版本化 JSON 信封，Java 层再转为 sealed event 类型：

```json
{
  "schemaVersion": 1,
  "type": "text_delta",
  "runId": "run_xxx",
  "sequence": 42,
  "payload": { "text": "正在分析项目结构……" }
}
```

稳定事件仅包含 `run_started`、`text_delta`、`tool_call_started`、`tool_call_finished`、`warning`、`completed`、`cancelled`、`failed`。Rust 内部 stream event 的完整 JSON 不属于公共契约。

## 7. 配置、状态与权限

配置优先级：

```text
anureoClientBuilder 显式参数
  > Java system properties（anureo.*）
  > 环境变量（OPENAI_API_KEY / OPENAI_BASE_URL）
  > 项目 anureo 配置
```

推荐公开 `anureo.apiKey`、`anureo.baseUrl`、`anureo.model`、`anureo.home`、`anureo.log.level`。API key 仅短暂复制到 native 层，错误、事件和 tracing 中必须脱敏。

SDK 默认 state home 与 CLI/Server 开发 home 隔离：Windows 为 `%LOCALAPPDATA%\\anureo`，macOS 为 `~/Library/Application Support/anureo`，Linux 为 `~/.local/share/anureo`。调用方可显式 `builder.home(...)`。

嵌入式运行不能继承用户机器的全部 agent 权限：

| 能力 | 默认 | 说明 |
|---|---:|---|
| 工作目录内读取 | 开启 | 项目分析的基础能力 |
| 工作目录内写入 | 关闭 | `ToolPolicy` 显式授权 |
| Shell 命令 | 关闭 | 需显式 allowlist |
| MCP server | 关闭 | 防止隐式启动外部进程 |
| 工作目录外访问 | 关闭 | canonical path 校验 |
| 工具网络访问 | 关闭 | LLM endpoint 独立于工具权限 |

## 8. Maven Artifact 与 native 加载

建议首发坐标：

```text
dev.anureo:anureo-java-api:0.5.0
dev.anureo:anureo-java-natives:0.5.0
dev.anureo:anureo-java-bom:0.5.0
```

`anureo-java-natives` 中将各平台动态库作为 JAR resource：

```text
META-INF/anureo/natives/
├── windows-x86_64/anureo_jni.dll
├── windows-aarch64/anureo_jni.dll
├── linux-x86_64/libanureo_jni.so
├── linux-aarch64/libanureo_jni.so
├── macos-aarch64/libanureo_jni.dylib
└── macos-x86_64/libanureo_jni.dylib
```

`NativeLoader` 根据 `os.name`、`os.arch` 选择文件，校验 SHA-256，解压至 `${java.io.tmpdir}/anureo/<version>/<hash>/` 后以绝对路径调用 `System.load()`。Windows 使用 version/hash 子目录，以免 DLL 文件锁阻断升级。

Maven classifier 可用于分平台 artifact，但 Maven 在构建依赖图阶段无法可靠地按运行时平台自动选择 classifier。故第一版使用全平台 natives JAR，以保障一行 dependency 可用；native 包过大后，再为企业用户提供 classifier + Maven/Gradle OS detector 的可选发行方式。

发布链路：

```text
本地验证：./gradlew publishToMavenLocal
正式发布：Nexus Central staging → 验证 → release
```

正式 Maven Central artifact 必须包含正确 POM metadata、sources JAR、Javadoc JAR、GPG 签名、license、SCM 与开发者信息。API JAR 与 natives JAR 需要严格同版本；加载时调用 `nativeVersion()` 校验版本，不匹配立即失败。

## 9. 跨平台构建与测试

| 平台 | Rust target | JDK | 优先级 |
|---|---|---:|---:|
| Windows x86_64 | `x86_64-pc-windows-msvc` | 17、21 | P0 |
| Linux x86_64 | `x86_64-unknown-linux-gnu` | 17、21 | P0 |
| macOS Apple Silicon | `aarch64-apple-darwin` | 17、21 | P1 |
| macOS Intel | `x86_64-apple-darwin` | 17、21 | P1 |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | 17、21 | P2 |

CI 流程：

```text
Rust fmt / nextest / clippy
  ↓
各原生 runner 编译 cdylib
  ↓
复制 native library 到 Java resources
  ↓
Gradle unit test
  ↓
OS × JDK JNI integration test
  ↓
生成 sources / javadoc / signed Maven artifacts
  ↓
Nexus Central staging 与 release
```

优先在原生 GitHub Actions runner 上构建，而非只依赖交叉编译。anureo 所依赖的 SQLite、PTY、TLS 和系统能力可能具有 target-specific 行为，必须在产物运行平台验证。

## 10. 实施计划

### Phase 0：JNI 可行性 Spike

1. 创建 `apps/jni` 并产出 `cdylib`。
2. Java 调用 `nativeVersion()`。
3. native 启动 Tokio task 并回调一条测试事件。
4. 验证 JDK 17/21、Windows x86_64、异常清理和重复关闭。

**验收**：`mvn test` 或 Gradle test 可调用 native library；重复创建/关闭 client 不崩溃、不泄漏。

### Phase 1：Rust Embedding Core

1. 新增 `foundation/anureo-sdk-core`。
2. 包装 `run_agent_from_config`，完成请求、结果、事件和错误模型。
3. 接通 `RunCancellation`。
4. 默认 React agent，并添加 mock LLM 的纯 Rust 测试。

**验收**：无需 JVM 即可通过 SDK façade 执行一次 mock agent run。

### Phase 2：Java SDK MVP

1. 完成 `anureoClient`、`anureoRun`、`anureoRequest` 和 event API。
2. 实现异步结果、回调、取消与 `AutoCloseable`。
3. 实现 native 解压、哈希、锁和版本校验。
4. 首先发布 Windows x86_64。

**验收**：Java 示例可完成真实模型调用；取消、callback 异常、错误配置均有确定结果。

### Phase 3：安全与 Maven 发布

1. 实现 `ToolPolicy` 和最小权限默认值。
2. 完成密钥脱敏与 SDK state home 隔离。
3. 配置 sources/Javadoc/signing 与 `publishToMavenLocal`。
4. Nexus Central staging 发布。

**验收**：独立 Maven 项目只凭坐标即可运行，无需 Cargo 或 Rust 环境。

### Phase 4：跨平台与扩展

1. 增加 Linux x86_64 和 macOS ARM64。
2. 扩展事件模型、会话恢复和可控 MCP 接入。
3. 增加远程 HTTP/ACP transport，使 Java 用户可在 embedded 和 remote runtime 间切换。

## 11. 风险与缓解

| 风险 | 缓解措施 |
|---|---|
| 慢 Java callback 阻塞 agent | 有界队列与 dispatcher；终态事件不可丢 |
| Java exception 穿越 FFI | 每次 callback 检查并清除 exception，转换为受控失败 |
| Tokio runtime 嵌套 | 进程级单例 runtime，禁止 JNI 内嵌 `block_on` |
| Windows DLL 锁定 | 按版本/hash 解压目录加载 |
| CLI 与 SDK 数据冲突 | SDK 专用 home，调用方可显式配置 |
| Shell/MCP 的安全风险 | 默认关闭，`ToolPolicy` 显式授权 |
| 内部 Rust 重构破坏 Java | `anureo-sdk-core` 作为唯一稳定适配层 |
| native JAR 体积增加 | 初期优先可靠性，后续可添加 classifier 发行 |

## 12. 向后兼容性

Java public API 遵循 SemVer；JNI native 方法只增不改。事件 JSON 使用 `schemaVersion`，允许新增字段但不改变已有字段语义。Rust 内部 crate 无 Java 兼容性承诺，由 `anureo-sdk-core` 隔离。

Java API、native bridge 与其依赖的 anureo runtime 使用相同发布版本。加载时 native 与 Java 版本不一致必须失败，而不是在未知 ABI 下继续执行。

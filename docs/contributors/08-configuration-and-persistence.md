# 配置与持久化

> **状态**：基于当前源码的贡献者说明
> **相关代码**：`foundation/config`、`foundation/checkpoint`、`foundation/checkpoint-sqlite-store`、`apps/cli`、`apps/acp`、`apps/server`、`experimental/task/task-core/migrations`

本文面向 anureo 贡献者，说明当前源码中的配置来源、运行时状态、checkpoint、长期 Store 及应用层持久化边界。所有路径、字段、命令和行为均以列出的当前源码为准；没有源码和测试证据的 API 不在本文中承诺。

前置知识建议先阅读[架构与依赖边界](./02-architecture-and-dependency-boundaries.md)，其中说明配置、checkpoint 与 Store 的 owning boundary；涉及 ACP session 或 backend storage 时，再阅读[ACP 与 backend 集成](./06-acp-and-backend-integration.md)。本文在首次涉及这些边界时仍保留对应源码路径，便于直接回到实现。

## 1. 先区分四类“状态”

anureo 目前不是由一个统一数据库承载全部状态，而是按生命周期分开：

| 状态 | owning module | 当前实现 | 生命周期 |
| --- | --- | --- | --- |
| 进程配置 | `foundation/config` | `.env`、`config.toml` 解析后写入 process environment | 当前进程 |
| graph checkpoint | `foundation/checkpoint`、`checkpoint-sqlite-store` | `Checkpointer`、`MemorySaver`、`SqliteSaver` | thread/run，可 resume、replay、branch |
| 跨 session 数据 | `foundation/checkpoint`、`checkpoint-sqlite-store` | `Store`、`InMemoryStore`、`SqliteStore` | namespace/key，长期 memory/preferences 等 |
| 应用记录 | `apps/cli`、`apps/acp`、`apps/server`、`experimental/task` | JSON session 文件、session_config SQLite 表、server Store、task SQLite schema | 各入口自有；不能与 checkpoint 互相替代 |

最容易误判的是：ACP `SessionStore` 和 server `AppState` 的 map 是运行时内存表；它们本身不会在进程退出后恢复。checkpoint 保存 graph 状态，也不是 CLI 的 session 列表；Store 保存跨 session key/value，也不是某次 run 的执行快照。

## 2. 配置加载链

### 2.1 anureo home 与文件路径

`foundation/config/src/home.rs` 是用户级路径的单一边界。`anureo_home()` 先读 `ANUREO_HOME`；未设置时 Unix 读 `$HOME`，Windows 读 `%USERPROFILE%`，再拼接 `.anureo`；相应 home 变量缺失时最终回退到当前目录 `.`。路径函数只返回 `PathBuf`，不会自动创建目录。

当前源码定义的路径包括：

```text
{ANUREO_HOME}/config.toml
{ANUREO_HOME}/mcp.json
{ANUREO_HOME}/thread/{session_id}/
{ANUREO_HOME}/acp/
{ANUREO_HOME}/logs/
{ANUREO_HOME}/logs/cli/
{ANUREO_HOME}/logs/acp/
{ANUREO_HOME}/logs/acp/anureo-acp.log
{ANUREO_HOME}/logs/llm/
```

`foundation/checkpoint-sqlite-store` 的 `default_memory_db_path()` 使用 `{ANUREO_HOME}/memory.db`，创建父目录；home 不可用时回退到当前目录的 `memory.db`。CLI 的 task 数据库另在 `{ANUREO_HOME}/tasks/tasks.db`，由 `apps/cli/src/task_db.rs::ensure_task_db` 创建父目录。CLI file session store 默认使用 `{ANUREO_HOME}/data/sessions`。

### 2.2 `config.toml`、`.env` 与 process environment

`load_and_apply_with_report("anureo", override_dir)` 的调用流程是：

```text
ANUREO_HOME/config.toml
  -> xdg_toml::load_full_config
  -> [env]、[default].provider、[[providers]]

override_dir/.env（若 override_dir 为 None，则 current_dir/.env）
  -> dotenv::load_env_map

两者合并
  -> 已存在的 process environment 不覆盖
  -> std::env::set_var
  -> ConfigLoadReport（secret masked）
```

同一个 key 的优先级从高到低是：

```text
已有 process environment > 项目 .env > [default].provider 选中的 [[providers]] > config.toml [env]
```

这里的 provider 只在 `[default].provider` 指向现有 `[[providers]]` 时生效；provider 名称匹配大小写不敏感。没有选择 provider、provider 名称不存在或只有未被选中的 provider 时，不会自动应用 provider 环境变量。缺失的 `config.toml` 和 `.env` 都按空输入处理；TOML 读取/解析和 `.env` 读取失败会通过 `LoadError` 返回。

`dotenv.rs` 是一个有意受限的 parser：支持 `KEY=VALUE`、空值、行首 `#` 注释、双引号中的 `\"` 和去除单引号；不支持 multiline 或 line continuation，值内部的 `#` 不会被当作注释。贡献者不要把它当作完整 dotenv 规范实现。

### 2.3 `[[providers]]` 与默认 model

`ProviderDef` 当前可配置：

```toml
[default]
provider = "openai"

[[providers]]
name = "openai"
api_key = "sk-..."
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
type = "openai_compat"
temperature = 0.7
fetch_models = true
cache_ttl = 300
enable_tier_resolution = true

[[providers.models]]
id = "custom-model"
context_limit = 100000
output_limit = 8192
reasoning_efforts = ["low", "medium", "high"]
```

`ProviderDef::to_env_map()` 当前映射 `api_key → OPENAI_API_KEY`、`base_url → OPENAI_BASE_URL`、`model → MODEL`、有限的 `temperature → OPENAI_TEMPERATURE`。`provider_config.rs` 则把 provider 转为 `model-spec-core::registry::ProviderConfig`，其中 `fetch_models` 默认 `false`、`enable_tier_resolution` 默认 `true`，并把手工声明的 model id 传入 `declared_models`。

没有 `base_url` 时，配置加载器会先检查 `ANUREO_MODELS_DEV_API_JSON` 内联 JSON；否则请求 `MODELS_DEV_URL`，默认是 `https://models.dev/api.json`，从 provider 名称提取 API 地址。这是当前源码的 fallback，不应被描述成稳定的 provider discovery API；测试应优先使用内联 JSON，避免网络依赖。

`default_model()` 的顺序是 `MODEL` 环境变量、`[default].provider` 对应 provider 的 `model`、名称含 `coding-plan` 的第一个 provider、任意第一个带 model 的 provider，最后是 `gpt-4o-mini`。`default_provider_name()` 的顺序是显式 default、名称含 `coding-plan` 的第一个 provider、首个 provider。

配置报告的 `ConfigLoadReport` 记录来源和路径；`is_secret_key` 会识别 key/token/secret/password/credential/auth 等命名，`mask_value` 只保留前后两个字符，过短值显示为 `***`。日志必须使用 `value_masked` 或 `summary()`，不要输出原始 credential。

### 2.4 MCP 与 LSP 是独立配置面

MCP 配置不是 `config.toml [env]` 的一部分。`mcp_config.rs` 读取 Cursor/Claude 兼容的 JSON 根对象 `mcpServers`，每个 entry 必须有 `command` 或 `url`；`url` 存在时优先，并且只接受 `http://` 或 `https://`。`disabled = true` 的 entry 被跳过。stdio 结果为 `McpServerDef::Stdio`，远程结果为 `McpServerDef::Http`，HTTP entry 可携带 `headers` 与 `oauth`。

发现顺序是：显式 `override_path`（文件存在）→ `{working_dir}/.anureo/mcp.json` → `{ANUREO_HOME}/mcp.json`。`save_mcp_config` 先写同目录 `.json.tmp` 再 rename；`upsert_mcp_server`、`remove_mcp_server` 是文件级修改接口，`get_or_create_mcp_config_path` 会创建全局空配置。

LSP 配置也独立于 anureo home：先找 `$XDG_CONFIG_HOME/anureo/lsp.toml`，再找 `~/.config/anureo/lsp.toml`；没有文件时返回内置默认。当前默认 server 覆盖 Rust、TypeScript、JavaScript、Python、Go、Java。`LspServerConfig` 的扩展字段包括 command/args、file patterns、initialization options、root URI、env、startup timeout 和 auto-install；默认并发数为 10，启动超时通常为 10 秒（Java 为 30 秒）。源码只提供加载/发现模型，不在本文扩展未读出的 LSP runtime API。

## 3. Checkpoint 与 Store 的模块边界

### 3.1 `Checkpointer`：执行快照

`foundation/checkpoint` 导出 `Checkpoint<S>`、`CheckpointTuple<S>`、`CheckpointMetadata`（当前别名为 `KernelMetadata`）、`RunnableConfig` 和 `Checkpointer<S>`。`RunnableConfig` 的关键寻址字段是 `thread_id`、可选 `checkpoint_ns`、可选 `checkpoint_id`；Store 使用 `user_id` 做多租户隔离。

一个 checkpoint 包含 version（当前 `CHECKPOINT_VERSION = 2`）、UUID6 id、时间戳、`channel_values`、channel versions、`versions_seen`、updated channels、pending sends/writes/interrupts，以及 kernel/user metadata。`KernelMetadata` 描述 source（Input/Loop/Update/Fork）、step、created_at、parents、children 和可选 summary；`CheckpointUserMeta` 的持久化结果必须是可供 SQLite `json_extract()` 查询的 JSON object。

`Checkpointer` 的最小契约是：

- `put(config, checkpoint)` 写入当前 thread/namespace；
- `get_tuple(config)` 在指定 `checkpoint_id` 时读取该点，否则读取该 lineage 最新点；缺少 `thread_id` 应返回 `ThreadIdRequired`；
- `list(config, limit, before, after)` 返回历史 metadata；
- `put_writes`/`get_writes` 保存并按 `(task_id, idx)` 去重、排序 pending writes，默认实现为空操作以兼容旧 saver。

`Serializer<S>` 只负责 state 与 bytes；`JsonSerializer` 使用 JSON。`TypedSerializer` 目前支持 `null`、`bytes`、`json` 三种 tag；`bytes` 还原成 UTF-8 lossy JSON string，而不是通用 binary object。新增持久化 backend 应明确选择哪个 serializer 契约。

### 3.2 `Store`：跨 session key/value

`Store` 使用 `Namespace = Vec<String>` 和 key 定位 `Item`，支持 put/get/get_item/delete/list、带 query/filter/limit/offset 的 search、namespace listing 及 batch。它适合 preferences、facts、memory 等跨 run 数据，不承载 graph frontier 或 pending interrupt。

`InMemoryStore` 是 process-local 实现：以 namespace 与 key 组成内部 map key，更新时保留 `created_at`、刷新 `updated_at`；搜索是 key/value 文本过滤，不是向量语义搜索。`SqliteStore` 使用 SQLite `store_kv(ns, key, value, created_at, updated_at)`，value 为 JSON text，异步 trait 方法通过 `spawn_blocking` 操作 SQLite，并使用 WAL helper。它是当前源码中的 file-backed、single-node Store；搜索仍是简化的 key/value 过滤，不是 semantic index。

### 3.3 SQLite checkpoint

`foundation/checkpoint-sqlite-store` 的公开实现是 `SqliteSaver`（`Checkpointer`）和 `SqliteStore`（`Store`），默认 memory DB 路径是 `{ANUREO_HOME}/memory.db`。`SqliteSaver` 注入 `Arc<dyn Serializer<S>>`，因此 state 类型必须与 serializer 的序列化能力匹配；backend 还保存 checkpoint lineage、pending writes 和 metadata。

这是一个有破坏性后果的“重新建空库”操作，不是旧业务数据恢复。执行 repair 前必须停止所有 anureo 进程，确认要操作的绝对数据库路径，并先复制原文件备份；同时确认没有其他进程会继续打开该路径。操作后核对原文件名是否变为 `.corrupt-*`、新库是否存在 `state_meta(key='schema_repair', ...)`，并确认应用自己的 schema 初始化已成功。旧数据仍在 `.corrupt-*` 文件中，必须通过明确的导入/恢复流程处理，不能把新库中的 schema marker 当作数据已恢复的证明。

SQLite 文件损坏时，`repair_state_db_schema` 只对 SQLite 报告的 `file is not a database`、`database disk image is malformed` 或 `not a database` 做修复：把原文件重命名为 `<name>.corrupt-<unix_ts>`，打开原路径建立新连接，并写入 `state_meta(schema_repair, timestamp)` 标记。原损坏文件会保留，便于恢复；locked、out-of-memory 等其他错误会继续向上返回。修复函数不能替代业务 schema 初始化，调用方仍需运行自己的 init schema。

## 4. 各应用的实际持久化方式

### 4.1 CLI file session 与 task DB

`apps/cli/src/run/session_store.rs` 的 `FileSessionStore` 把一个 `StoredSession` 写成 `{base_dir}/{id}.json`。字段是 id、title、content、created_at、updated_at、tags；`list(limit)` 按 updated_at 倒序，`search(query, limit)` 在 content/title/tags 上做 lowercase substring 匹配。`store_session_from_conversation` 对已有 session 追加 User/Assistant 文本并合并去重 tags，不是 checkpoint resume。

`apps/cli/src/task_db.rs` 目前只负责确保 `{ANUREO_HOME}/tasks/tasks.db` 的目录和路径；它没有在该源码文件中实现 task repository 或 schema。不要从函数名推断更高层 task API。

### 4.2 ACP session 与 session config

`apps/acp/src/session.rs` 的 `SessionStore` 是进程内 `RwLock<HashMap<SessionId, SessionEntry>>`。`session/new` 对应 `create`，session id 形如 `session-{UUID}`；entry 保存 thread_id、working_directory、cancelled、SessionConfig、MCP server definitions 和 connection。session 的 thread_id 默认使用 session id。

一次 prompt 先 `begin_prompt`：同一 session 已有 active turn 时返回 `None`，否则产生 generation 和 `RunCancellation`；`cancel_current_generation` 同时设置 cancelled 并 cancel token；完成后按 generation `finish_prompt` 清理 current turn。SessionConfig 当前包含 model、current_agent、effort。这个 session table 在进程退出后丢失。

`SessionConfigStore` 是另一条、可跨重启的 SQLite 边界，与 checkpoint DB 共用传入的 database path，但只管理 `session_config(session_id, key, value, updated_at)` 及索引。它提供 set/get/get_all/delete_session/copy_config；key/value 是开放字符串，源码没有声明一组更大的稳定配置 key 枚举。不要把 ACP 内存 `SessionStore` 的更新自动当成已写入 SQLite，除非调用方显式接入 `SessionConfigStore`。

### 4.3 Server Store seam

`apps/server/src/storage.rs` 定义 server 自己的同步 `Store` trait，服务 sessions、messages、parts、global events 与 v2 session events。写侧是 save/delete/replace/append，读侧是 load-on-startup；event ring capacity 为 512，超出时淘汰最旧事件。默认 `InMemoryStore` 使用 `parking_lot::RwLock`，每个接口都会真实读写 map，不是 no-op。

该 trait 是 server 的持久化 seam：源码注释说明 `AppState::store` 在测试中可为 `None`，通过 `new_server_state_with_store` 构造时可启用 write-through，并由 `load_from_store` 在启动时回填内存 map。当前指定源码没有提供 SQLite/JSON file server backend；文档只把它标为可替换扩展点，不能声称已经有实现。

### 4.4 Experimental task schema

`experimental/task/task-core/migrations/20250101000000_initial.sql` 创建 `tasks` 表：`id`、`name`、`description`、`assignee`、`start_time`、`created_at`、`status`，status 约束为 `pending/in_progress/completed/cancelled`。`20250102000000_goal_fields.sql` 追加 `metadata TEXT NOT NULL DEFAULT '{}'`。

这一目录属于实验性 task 能力；migration 是当前可见 schema 证据，不等于已经存在稳定 CLI/API、迁移 runner 或跨模块集成。贡献者修改它时必须显式标注实验性并检查真实 migration 执行入口。

## 5. 从配置到持久化的调用流程

```text
CLI / ACP / server 入口
        │
        ├─ foundation/config::load_and_apply_with_report
        │     ├─ ANUREO_HOME/config.toml
        │     ├─ project .env
        │     ├─ active provider -> process env
        │     └─ default_model / ProviderConfig consumer
        │
        ├─ graph runtime
        │     ├─ RunnableConfig.thread_id + checkpoint_ns/id
        │     └─ Checkpointer::put/get_tuple/list/put_writes
        │             └─ MemorySaver（仅内存）或 SqliteSaver（SQLite）
        │
        ├─ cross-session memory
        │     └─ Store::put/get/search/list
        │             └─ InMemoryStore 或 SqliteStore
        │
        └─ app-specific records
              ├─ CLI JSON sessions / task DB path
              ├─ ACP SessionStore + optional SessionConfigStore
              └─ server Store write-through/load-on-startup
```

扩展时应沿 owning boundary 接入：新配置字段进入 `foundation/config` 的 model/parser，再由 consumer 读取；新 graph resume 语义进入 `Checkpointer` backend；新长期数据进入 `Store`；CLI/ACP/server 的展示、协议和应用记录留在各自 app。不要在 app 中直接 `set_var`、复制 checkpoint schema 或把内存 session map 伪装成持久化。

## 6. 测试与验证

### 6.1 配置测试

基础单元测试覆盖 home 路径、dotenv parser、TOML 缺失/解析错误、provider 字段、model 默认值、MCP JSON 校验和 LSP 默认配置。注意：`foundation/config/src/home.rs` 中的 `CONFIG_TEST_LOCK` 位于 `#[cfg(test)] pub(crate)`，集成测试不可访问；而且 `providers_e2e.rs` 与 `mcp_config_e2e.rs` 各自有私有 mutex，不能跨测试目标协调。凡会修改 process-global environment 的集成测试，都必须在自己的测试文件中使用串行锁和 RAII guard；guard 至少覆盖 `ANUREO_HOME`、所有待写入的 provider/config key、`ANUREO_MODELS_DEV_API_JSON` 和 `MODELS_DEV_URL`，并在正常返回、断言失败或 panic 展开时恢复原值。

provider 与 MCP 的 e2e 文件使用真实临时目录、写入 `config.toml`/`mcp.json`、调用公开 load/discover 函数并断言 env/report 或 server definition；但当前测试函数均带 `#[ignore]`。运行时应明确选择被忽略测试，例如：

```powershell
cargo test -p config --test providers_e2e -- --ignored
cargo test -p config --test mcp_config_e2e -- --ignored
```

provider fallback 测试优先设置 `ANUREO_MODELS_DEV_API_JSON`，不要让测试依赖外网 `https://models.dev/api.json`。

### 6.2 checkpoint/store/SQLite 测试

建议按实现层验证：

```powershell
cargo test -p checkpoint
cargo test -p checkpoint-sqlite-store
cargo test -p anureo-cli
cargo test -p acp
cargo test -p anureo-server
```

重点断言包括：缺少 `thread_id` 的错误、latest 与指定 checkpoint 读取、history paging、pending writes 的顺序与 `(task_id, idx)` 幂等性、JSON serializer round-trip、Store namespace/filter/search 的 limit/offset、SQLite 重启后的 round-trip，以及损坏 DB 被保留为 `.corrupt-*` 后重新初始化。涉及 `apps/server` 时还要分别验证 store 为 `None` 的旧测试路径和启用 store 的 load/write-through 路径。

### 6.3 文档与 schema 验证

修改 migration 前应使用该实验性 task crate 的真实 Cargo target/runner（如果当前 workspace 中存在），不要仅凭文件名构造命令。修改 CLI session path、ACP session config 或 server Store 后，先 `cargo check` 对应 package，再运行该 package 的 tests；不要把实验性 task migration 当作 workspace 全量稳定测试已覆盖的证据。

## 7. 扩展点与常见坑

| 需求 | 应修改的位置 | 关键约束 |
| --- | --- | --- |
| 新配置来源/字段 | `foundation/config` | 明确优先级、错误类型、report masking，并补 env 隔离测试 |
| 新 provider 映射 | `xdg_toml.rs`、`provider_config.rs`、加载测试 | 不覆盖已有 env；不要把 models.dev fallback 扩成未声明 API |
| 新 MCP JSON 字段 | `mcp_config.rs` | 保持 `mcpServers` 兼容格式，补 disabled/invalid/override path 测试 |
| 新 checkpoint backend | 实现 `Checkpointer<S>`，注入 `Serializer<S>` | 保持 thread/namespace/id 寻址和 pending writes 契约 |
| 新长期存储 | 实现 `checkpoint::Store` | 明确 namespace、JSON value、search 语义和并发/事务边界 |
| 新 CLI session 数据 | `FileSessionStore` 或其 owning CLI 模块 | 不把 JSON conversation archive 当 graph checkpoint |
| ACP 配置持久化 | 显式调用 `SessionConfigStore` | 内存 `SessionStore` 不会自动跨进程保存 |
| server durable backend | 实现 `apps/server::storage::Store` | 当前源码只给出 seam，SQLite/JSON backend 仍是未实现扩展 |
| task goal/schema | `experimental/task/task-core/migrations` | 实验性；先确认 migration runner 与兼容策略 |

常见坑：

- 把 `ANUREO_HOME` 当成会自动建目录的 API；多数 home 路径函数只拼路径，只有具体 store/文件 writer 才负责创建目录。
- 认为 `[default].provider` 会选择第一个 provider；配置加载器只应用显式选中的 provider，默认 model 的 fallback 是另一套逻辑。
- 把 `.env` 当完整 dotenv parser，或以为 `.env` 能覆盖 shell 中已经存在的变量；两者都不成立。
- 把 `MemorySaver`/`InMemoryStore` 当持久化；它们都会在进程结束时丢失。
- 把 `checkpoint_id`、`checkpoint_ns` 和 `thread_id` 漏掉其中之一；backend 的 lineage 隔离依赖这些字段的组合。
- 误以为 SQLite repair 会恢复旧数据；它保留损坏文件并创建空数据库，贡献者仍需设计恢复/导入流程。
- 直接记录 API key、MCP headers 或 session config value；当前 masking 只覆盖 config report，其他日志不会自动替你脱敏。
- 把 ACP `SessionStore` 的 `SessionConfig` 更新当成 `SessionConfigStore::set`；两者没有在指定源码中自动同步。
- 把 server 的 `InMemoryStore` 和 foundation 的 `checkpoint::Store` 当同一个 trait；它们方法签名和 owning domain 都不同。
- 把 task migration 当作稳定功能；该目录明确属于 experimental，schema 证据不能推出未读出的 API。

## 8. 最小贡献流程

1. 先确定数据的生命周期：进程配置、单 run checkpoint、跨 session Store，还是 app-specific record。
2. 从 owning module 的 trait/struct 开始追踪到实际 consumer，确认 key、路径、错误和线程/进程边界。
3. 修改配置时同时补优先级、masking、缺失文件和 process-global env 恢复测试；修改 backend 时补 round-trip、隔离、错误和重启语义测试。
4. 对 SQLite 操作检查 schema 初始化、WAL/阻塞调用、事务/幂等性和损坏文件行为；对 JSON 文件操作检查临时文件、排序、limit 和坏文件处理。
5. 运行对应 package 的 `cargo test`，对 `#[ignore]` 的 e2e 显式使用 `-- --ignored`；最后用 `cargo check` 检查跨 crate wiring。
6. 更新文档时只描述当前源码已有的 API；任何 experimental 路径、未提供 backend 或未实现 runner 都显式标注。

### 8.1 最小安全修改：新增一个 provider 映射

下面是一次可复制、范围较小的 `foundation/config` 修改路径。假设要给 `ProviderDef` 增加一个已有字段到 process environment 的映射：先在 `foundation/config/src/xdg_toml.rs:82` 定位 `ProviderDef`，在 `:111-115` 修改 `ProviderDef::to_env_map()`，再检查 `foundation/config/src/provider_config.rs:21` 是否也需要把该字段传给 `ProviderConfig`；不要只改 consumer 侧的 `set_var`。如果字段影响默认选择或 fallback，同时检查 `default_model()`/`default_provider_name()` 的优先级。

测试放在 `foundation/config/tests/providers_e2e.rs`：复制现有 `e2e_default_provider_sets_env_vars` 的结构，在 `tempfile::tempdir()` 下写入 `config.toml`（`write_config(dir.path(), ...)`），用本测试目标自己的 `LOCK` 和 RAII `EnvGuard`，设置 `ANUREO_HOME`，并清理本测试会触及的 provider key（例如 `OPENAI_API_KEY`、`OPENAI_BASE_URL`、`MODEL`、`LLM_PROVIDER`、`OPENAI_TEMPERATURE`）、`ANUREO_MODELS_DEV_API_JSON` 与 `MODELS_DEV_URL`。调用 `load_and_apply_with_report("anureo", None::<&std::path::Path>)`，断言新增环境变量的值（`ProviderDef::to_env_map()` 当前会映射 `api_key`、`base_url`、`model`、有限的 `temperature`；`provider_type` 则由 provider 配置转换/consumer 使用，不是该方法的 env 映射）以及 `ConfigLoadReport` 中对应 entry 的 `source`；若测试 provider 没有 `base_url`，用内联 `ANUREO_MODELS_DEV_API_JSON`，不要访问网络。测试函数保持 `#[ignore]` 时，从仓库根目录运行：

```powershell
cargo test -p config --test providers_e2e -- --ignored e2e_default_provider_sets_env_vars
cargo check -p config
```

预期结果是第一条命令报告该指定测试通过（`1 passed`，其余 ignored 不执行），第二条命令以状态码 0 完成。若修改的是现有测试函数或名称，应把命令末尾替换为实际 test name；若环境变量原本存在，测试结束后必须恢复原值，不能仅 `remove_var`。

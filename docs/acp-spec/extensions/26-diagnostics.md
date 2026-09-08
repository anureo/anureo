# Diagnostics 诊断

> **命名空间**: `_anureo.dev/diagnostics/*`
> **Capability key**: `diagnostics`
> **实现状态**: ❌ 未实现

---

## Capability

```json
{
  "diagnostics": {
    "logs": true,
    "export": true
  }
}
```

- Client 必须在 `initialize` 时声明 `agentCapabilities._meta["anureo.dev"].diagnostics` 的 method 粒度。
- **安全约束**: 诊断导出不得包含 token、secret、完整 API key 或用户文件内容。日志路径必须由 server 解析，不接受 client 传入的绝对路径。

---

## Methods

### `_anureo.dev/diagnostics/logs`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `diagnostics.logs` |
| 权限 | 无（读取操作） |
| 分页 | 支持（`08-cross-cutting-patterns.md` §1） |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "_anureo.dev/diagnostics/logs",
  "params": {
    "level": "info",
    "component": "acp",
    "since": "2025-08-19T00:00:00Z",
    "until": "2025-08-19T23:59:59Z",
    "search": "error",
    "cursor": null,
    "limit": 100
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `level` | string | 否 | 日志级别过滤：`trace` / `debug` / `info` / `warn` / `error`（返回该级别及以上） |
| `component` | string | 否 | 按组件过滤：`acp` / `server` / `session` / `mcp` / `llm` / `tool` |
| `since` | string (ISO 8601) | 否 | 起始时间 |
| `until` | string (ISO 8601) | 否 | 结束时间 |
| `search` | string | 否 | 全文搜索关键词 |
| `cursor` | string \| null | 否 | 分页游标 |
| `limit` | int | 否 | 每页数量，默认 100，最大 500 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "items": [
      {
        "timestamp": "2025-08-19T14:30:00.123Z",
        "level": "error",
        "component": "acp",
        "message": "Failed to initialize MCP server 'git-enhanced'",
        "details": {
          "serverName": "git-enhanced",
          "errorCode": "connection_refused",
          "retryCount": 3
        },
        "sessionId": "sess_abc123",
        "threadId": null
      },
      {
        "timestamp": "2025-08-19T14:31:00.456Z",
        "level": "warn",
        "component": "llm",
        "message": "Rate limit approaching for provider 'openai'",
        "details": {
          "provider": "openai",
          "remainingRequests": 15,
          "resetAt": "2025-08-19T14:35:00Z"
        },
        "sessionId": null,
        "threadId": null
      }
    ],
    "nextCursor": "eyJ0IjoiMjAyNS0wOC0xOVQxNDozMTo1OS40NTZaIn0=",
    "hasMore": true
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `items[].timestamp` | string (ISO 8601) | 日志时间戳（毫秒精度） |
| `items[].level` | string | 日志级别 |
| `items[].component` | string | 来源组件 |
| `items[].message` | string | 日志消息（已脱敏，不含 token/secret） |
| `items[].details` | object | 结构化附加信息（已脱敏） |
| `items[].sessionId` | string \| null | 关联的 session ID（如有） |
| `items[].threadId` | string \| null | 关联的 thread ID（如有） |
| `nextCursor` | string \| null | 下一页游标 |
| `hasMore` | bool | 是否还有更多数据 |

#### 逻辑说明

1. **日志路径由 server 解析**: Client 不能传入日志文件路径或目录。Server 从自身的日志存储中检索（SQLite、文件、或内存环形缓冲区）。
2. **脱敏处理**: Server 在返回日志前必须执行脱敏——token、secret、完整 API key、用户文件路径中的敏感部分必须被替换为 `****`。
3. 日志按 `timestamp` 降序排列（最新优先）。
4. `level` 过滤为包含性过滤：`info` 返回 `info` + `warn` + `error`。
5. `search` 为子串匹配（不区分大小写），server 可限制搜索范围以控制性能。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsLogsRequest {
    pub level: Option<LogLevel>,
    pub component: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub search: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub timestamp: DateTime<Utc>,
    pub level: LogLevel,
    pub component: String,
    pub message: String,
    pub details: Option<serde_json::Value>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsLogsResponse {
    pub items: Vec<LogEntry>,
    pub next_cursor: Option<String>,
    pub has_more: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `diagnostics.logs` 未声明 |
| `Invalid Params (-32602)` | `since` > `until`、cursor 格式非法、`limit` 超出范围 |

---

### `_anureo.dev/diagnostics/export`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | `diagnostics.export` |
| 权限 | Server-side authorization（写操作——因为导出包含系统信息） |
| 进度 | 支持 progress notification（`08-cross-cutting-patterns.md` §3） |
| Timeout | 建议 60 秒 |

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "_anureo.dev/diagnostics/export",
  "params": {
    "includeLogs": true,
    "includeConfig": true,
    "includeSessionMetadata": true,
    "includeSystemInfo": true,
    "logLevel": "info",
    "since": "2025-08-19T00:00:00Z",
    "format": "json"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `includeLogs` | bool | 否 | 是否包含日志（默认 `true`） |
| `includeConfig` | bool | 否 | 是否包含配置信息（默认 `true`，脱敏后） |
| `includeSessionMetadata` | bool | 否 | 是否包含 session metadata（默认 `true`） |
| `includeSystemInfo` | bool | 否 | 是否包含系统信息（默认 `true`） |
| `logLevel` | string | 否 | 日志级别过滤（同 `diagnostics/logs`） |
| `since` | string (ISO 8601) | 否 | 日志起始时间 |
| `format` | string | 否 | 导出格式：`json`（默认）/ `text` |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "exportId": "diag_export_001",
    "format": "json",
    "downloadUrl": "/api/diagnostics/download/diag_export_001",
    "expiresAt": "2025-08-19T15:30:00Z",
    "size": 256000,
    "contents": {
      "logs": {
        "entryCount": 342,
        "timeRange": "2025-08-19T00:00:00Z to 2025-08-19T14:30:00Z"
      },
      "config": {
        "version": "0.4.4",
        "features": ["unstable_boolean_config", "unstable_session_fork"]
      },
      "sessionMetadata": {
        "sessionCount": 5
      },
      "systemInfo": {
        "os": "linux",
        "arch": "x86_64",
        "rustVersion": "1.85.0"
      }
    },
    "redacted": ["api_key", "auth_token", "provider_secret"]
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `result.exportId` | string | 导出包唯一标识 |
| `result.format` | string | 导出格式 |
| `result.downloadUrl` | string | 下载 URL（相对路径，需添加 server base URL） |
| `result.expiresAt` | string (ISO 8601) | 下载链接过期时间 |
| `result.size` | int | 导出包大小（字节） |
| `result.contents` | object | 导出内容概要 |
| `result.contents.logs.entryCount` | int | 包含的日志条目数 |
| `result.contents.logs.timeRange` | string | 日志时间范围 |
| `result.contents.config.version` | string | anureo 版本 |
| `result.contents.config.features` | string[] | 启用的 feature flags |
| `result.contents.sessionMetadata.sessionCount` | int | 包含的 session 数 |
| `result.contents.systemInfo` | object | 系统信息摘要 |
| `result.redacted` | string[] | 已脱敏的字段类型列表 |

#### 逻辑说明

1. **安全约束（强制）**: 诊断导出不得包含以下内容：
   - **Token**: 任何认证 token（session token、bearer token、pairing secret）
   - **Secret**: provider secret、webhook secret、加密密钥
   - **完整 API key**: API key 必须脱敏为 `prefix-****-suffix` 格式
   - **用户文件内容**: 不得包含任何用户源代码、配置文件正文或 prompt/response 内容
2. Server 在导出前执行自动脱敏扫描，`result.redacted` 列出被脱敏的字段类型。
3. 导出包通过临时 URL 提供下载，有过期时间（默认 30 分钟）。
4. 导出过程中的进度通过 `_anureo.dev/diagnostics/progress` notification 上报。
5. `downloadUrl` 为 server 相对路径，client 拼接 base URL 后通过 HTTP GET 下载。下载需要与 ACP connection 相同的 Bearer token 认证。
6. 大型导出（如包含大量日志）可能需要较长时间，server 应考虑异步生成。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsExportRequest {
    pub include_logs: Option<bool>,
    pub include_config: Option<bool>,
    pub include_session_metadata: Option<bool>,
    pub include_system_info: Option<bool>,
    pub log_level: Option<LogLevel>,
    pub since: Option<DateTime<Utc>>,
    pub format: Option<ExportFormat>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ExportFormat {
    Json,
    Text,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportContents {
    pub logs: Option<ExportLogSummary>,
    pub config: Option<ExportConfigSummary>,
    pub session_metadata: Option<ExportSessionSummary>,
    pub system_info: Option<ExportSystemInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportLogSummary {
    pub entry_count: u64,
    pub time_range: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportConfigSummary {
    pub version: String,
    pub features: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSessionSummary {
    pub session_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportSystemInfo {
    pub os: String,
    pub arch: String,
    pub rust_version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticsExportResponse {
    pub export_id: String,
    pub format: ExportFormat,
    pub download_url: String,
    pub expires_at: DateTime<Utc>,
    pub size: u64,
    pub contents: ExportContents,
    pub redacted: Vec<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `capability_not_supported (-32001)` | `diagnostics.export` 未声明 |
| `forbidden (-32002)` | Server-side authorization 拒绝 |
| `Internal Error (-32603)` | 导出生成失败（磁盘空间不足、日志存储不可用等） |

---

## Notifications

### `_anureo.dev/diagnostics/progress`

导出过程中的进度 notification（`08-cross-cutting-patterns.md` §3 长时操作进度）：

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/diagnostics/progress",
  "params": {
    "operationId": "diag_export_001",
    "progress": 65,
    "phase": "collecting_logs",
    "message": "Collecting 342 log entries...",
    "cancelable": true
  }
}
```

| 阶段 (`phase`) | 说明 |
|---|---|
| `collecting_logs` | 收集和过滤日志 |
| `collecting_config` | 收集配置信息 |
| `collecting_metadata` | 收集 session metadata |
| `redacting` | 执行脱敏扫描 |
| `packaging` | 打包导出文件 |

取消通过 JSON-RPC `notifications/cancelled`（ID 为 `operationId`）。

---

## Reconnect Resync 映射

本扩展域无状态变更 notification，无需 resync 映射。日志和导出为按需查询操作，不维护客户端状态。

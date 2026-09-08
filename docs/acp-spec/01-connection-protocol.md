# Connection 协议

> **命名空间**: 标准 ACP v1
> **实现状态**: ✅ 已实现
> **源码**: `apps/acp/src/agent.rs`、`apps/acp/src/protocol.rs`

---

## 1. `initialize`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | 无（连接建立后的第一个 ACP request） |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "method": "initialize",
  "params": {
    "protocolVersion": "2025-07-01",
    "clientCapabilities": {
      "fs": {
        "readTextFile": true,
        "writeTextFile": true
      },
      "terminal": true,
      "mcp": {
        "http": true
      },
      "prompt": {
        "image": true,
        "audio": true
      }
    },
    "clientInfo": {
      "name": "anureo",
      "version": "1.0.0"
    }
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `protocolVersion` | string | 是 | 请求的协议版本 |
| `clientCapabilities` | object | 是 | 客户端能力声明 |
| `clientCapabilities.fs.readTextFile` | bool | 否 | 支持 `fs/read_text_file` reverse-RPC |
| `clientCapabilities.fs.writeTextFile` | bool | 否 | 支持 `fs/write_text_file` reverse-RPC |
| `clientCapabilities.terminal` | bool | 否 | 支持 `terminal/*` reverse-RPC |
| `clientCapabilities.mcp.http` | bool | 否 | 支持 HTTP MCP server |
| `clientCapabilities.prompt.image` | bool | 否 | 支持 image content block |
| `clientCapabilities.prompt.audio` | bool | 否 | 支持 audio content block |
| `clientInfo` | object | 是 | 客户端标识 |
| `clientInfo.name` | string | 是 | 客户端名称 |
| `clientInfo.version` | string | 是 | 客户端版本 |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": "2025-07-01",
    "agentCapabilities": {
      "loadSession": true,
      "promptCapabilities": {
        "image": true,
        "audio": true,
        "embeddedContext": true
      },
      "mcpCapabilities": {
        "http": true,
        "sse": false
      },
      "sessionCapabilities": {
        "list": {},
        "delete": {},
        "resume": {},
        "close": {}
        // 注意: fork 未在此声明（代码 bug，见 02-session-lifecycle.md §4）
      },
      "_meta": {
        "anureo.dev": {
          // ExtensionRegistry capability 快照（31 个域），见 agent.rs::initialize()
        }
      }
    },
    "agentInfo": {
      "name": "anureo",
      "version": "<anureo_version>"
    },
    "authMethods": []
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `protocolVersion` | string | 协商后的协议版本（`ProtocolVersion::V1`） |
| `agentCapabilities.loadSession` | bool | anureo 返回 `true` |
| `agentCapabilities.promptCapabilities` | object | image/audio/embeddedContext |
| `agentCapabilities.mcpCapabilities` | object | HTTP MCP 支持 |
| `agentCapabilities.sessionCapabilities` | object | list/delete/resume/close 已声明；fork handler 已实现但 capability 未声明（代码 bug） |
| `agentCapabilities._meta` | object | `["anureo.dev"]` 为扩展域能力快照，随 `ExtensionRegistry` 注册自动生成 |
| `agentInfo.name` | string | 固定 `"anureo"` |
| `agentInfo.version` | string | anureo 版本号 |
| `authMethods` | array | 当前为空数组 |

### 逻辑说明

1. **单次约束**: 每条 connection 只能成功 `initialize` 一次；重复调用返回 `AlreadyInitialized` 错误
2. **第一请求**: `initialize` 必须是连接建立后的第一个业务 request；在此之前不得调用 session 或扩展 method
3. **Capability snapshot**: Client 必须根据本次 response 重建 capability snapshot，不能使用历史缓存推断
4. **Client 能力解析**: anureo 通过 `ClientCapabilitiesInfo` 解析客户端能力，用于决定是否发起 reverse-RPC
5. **扩展能力**: 当 `_anureo.dev` 扩展实现后，能力声明放在 `agentCapabilities._meta["anureo.dev"]`

### Rust 类型

```rust
// agent.rs
async fn initialize(
    &self,
    args: InitializeRequest
) -> agent_client_protocol::Result<InitializeResponse>

// 返回的 agentCapabilities 构造（protocol.rs）
AgentCapabilities {
    load_session: true,
    mcp_capabilities: McpCapabilities { http: true, sse: false },
    prompt_capabilities: PromptCapabilities {
        image: true, audio: true, embedded_context: true
    },
    session_capabilities: SessionCapabilities {
        list: SessionListCapabilities {},
        delete: SessionDeleteCapabilities {},
        resume: SessionResumeCapabilities {},
        close: SessionCloseCapabilities {}
    },
}
```

### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Request (-32600)` | JSON-RPC 结构非法 |
| `Invalid Params (-32602)` | 缺少 `protocolVersion` 或 `clientInfo` |

---

## 2. `authenticate`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 触发条件 | Agent 在 `initialize` 中声明 `authMethods` |
| anureo 状态 | ✅ Handler 已存在；当前 `authMethods` 为空 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "method": "authenticate",
  "params": {
    "method": "<auth-method>",
    "challenge": "<challenge-data>"
  }
}
```

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 2,
  "result": {
    "authenticated": true
  }
}
```

### 逻辑说明

1. 当前 `authMethods` 为空数组，所以 Client 不应发起此请求
2. WebSocket transport 的 Bearer auth 属于 Transport/Server 层（`apps/server/src/handlers/acp.rs`），不等同于 ACP `authenticate`
3. Server 层 auth: 从 `AUTHORIZATION` 头提取 Bearer token，与 `ANUREO_AUTH_TOKEN` 环境变量比对；匹配后生成 principal（格式 `token-{hash}`）
4. 未设置 `ANUREO_AUTH_TOKEN` 或 token 不匹配时，principal 为 `"local-anonymous"`

### Rust 类型

```rust
async fn authenticate(
    &self,
    args: AuthenticateRequest
) -> agent_client_protocol::Result<AuthenticateResponse>
```

### Error

| Error code | 触发条件 |
|---|---|
| `Method Not Found (-32601)` | `authMethods` 为空时不应调用 |

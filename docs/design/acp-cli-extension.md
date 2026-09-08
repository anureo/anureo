# anureo ACP CLI 扩展协议设计

> **状态**：Draft，待评审
> **日期**：2026-08-08
> **范围**：让 `anureo --acp` 通过 ACP WebSocket 访问完整 anureo CLI 能力
> **相关代码**：`apps/cli/src/args.rs`、`apps/cli/src/main.rs`、`apps/acp/src/ws_bridge.rs`、`apps/server/src/handlers/acp.rs`、`apps/server/src/acp_hub.rs`
> **相关文档**：[acp-websocket.md](./acp-websocket.md)、[acp-websocket-todo.md](./acp-websocket-todo.md)
> **官方依据**：[ACP Extensibility](https://agentclientprotocol.com/protocol/v1/extensibility)、[ACP Slash Commands](https://agentclientprotocol.com/protocol/v1/slash-commands)、[ACP Session Config Options](https://agentclientprotocol.com/protocol/v1/session-config-options)、[ACP Transports](https://agentclientprotocol.com/protocol/v1/transports)

## 1. 背景与问题

anureo 当前同时存在两类入口：

1. `anureo` CLI：直接执行 agent、管理 session、model、MCP、skills、memory、task、goal、review 等功能。
2. `anureo acp`：面向 IDE 的 ACP stdio bridge，将 ACP JSON-RPC 透传到 `anureo-server` 的 `/acp` WebSocket。

当前 `anureo-server` 已经具备 ACP WebSocket、`AcpHub`、session 持久化、断线 replay 和 Bearer 鉴权。现有 ACP agent 主要覆盖对话生命周期：

```text
initialize
session/new
session/load
session/prompt
session/update
session/cancel
```

但 CLI 的命令面明显大于 ACP 标准方法。当前 `apps/cli/src/args.rs` 中至少包含以下非 ACP 标准能力：

| CLI 类别 | 代表命令 | ACP 标准中是否直接对应 |
|---|---|---|
| Agent mode | `react`、`dup`、`tot`、`got` | 部分对应 `session/prompt`，但 mode 语义不完整 |
| Model | `models list`、`models show` | 无直接对应 |
| Tool | `tool list`、`tool show` | 无直接对应 |
| MCP | `mcp list/add/edit/delete/enable/disable` | 无直接对应 |
| Agent profile | `agent list/export` | 无直接对应 |
| Skills | `skills list/show/create/edit/delete/sync` | 无直接对应 |
| Memory | `memory list/show/edit/search` | 无直接对应 |
| Task | `task create/list/show/continue` | 无直接对应 |
| Goal | `goal run/resume` | 无直接对应 |
| Review | `review run/batch/show/history` | 无直接对应 |
| Curator/Evolve | `curator ...`、`evolve` | 无直接对应 |

如果把这些命令都编码成自然语言 prompt，会产生几个问题：

- 参数含义不稳定，无法可靠映射到现有 CLI 语义。
- GUI 或远程 client 无法通过 schema 生成表单。
- 读操作、修改操作和高风险执行操作无法区分。
- 长任务无法可靠取消、恢复和查询。
- JSON 输出与终端文本输出难以保持一致。

因此需要一个 anureo-specific ACP extension protocol。

## 2. 目标与非目标

### 2.1 目标

- 支持 `anureo --acp` 作为 ACP Client，通过 `/acp` WebSocket 执行 CLI 命令。
- 保持标准 ACP session/prompt/tool 生命周期兼容。
- 让 client 可以发现 anureo 支持的命令、参数 schema、风险和执行模式。
- 为短命令提供 request/response 语义，为长任务提供 job 语义。
- 复用现有 CLI service，而不是在 ACP handler 中复制业务逻辑。
- 让权限、confirmation、working directory、session owner 和 auth 具有明确边界。
- 对未知 anureo 扩展保持标准 JSON-RPC `-32601 Method not found` 行为。

### 2.2 非目标

- 不修改 ACP 标准方法的既有语义。
- 不把 anureo 所有 CLI 命令提升为 ACP 标准协议。
- 不引入 `raw shell` 或任意 `argv` 远程执行接口。
- 不在第一阶段实现跨多个 ACP server 的 command federation。
- 不依赖 ACP v2 draft；本设计以当前 ACP v1 SDK 和 JSON-RPC 语义为基线。
- 不把每个 CLI 子命令都设计成独立 JSON-RPC method。

## 3. 设计决策

| 维度 | 决定 | 说明 |
|---|---|---|
| 扩展命名 | `_anureo/...` | ACP 要求自定义 method 以 `_` 开头，避免与未来标准方法冲突 |
| 能力声明 | `initialize.agentCapabilities._meta["anureo.dev"]` | 不向标准 capability 对象添加未定义的 root-level 字段 |
| 命令发现 | `_anureo/cli/describe` | 返回结构化 command catalog 和 JSON Schema |
| 命令执行 | `_anureo/cli/execute` | 使用 `command + args`，不传 raw argv |
| 短命令 | JSON-RPC request/response | 适用于 list/show/validate 等有限时长操作 |
| 长命令 | Job | 立即返回 `jobId`，通过 `_anureo/job/update` 推送状态 |
| session 对齐 | 优先使用 ACP 标准方法 | `session/list`、`session/close`、`session/delete`、`session/set_config_option` 不重复实现 |
| 用户快捷命令 | ACP slash commands | `/models`、`/review` 等仍通过 `session/prompt` 执行 |
| 风险控制 | command metadata + server enforcement | client 展示 confirmation，server 不能只依赖 client 自律 |
| 业务复用 | CommandRegistry | CLI 本地入口和 ACP 入口调用同一个 service/registry |
| 版本策略 | capability version + schema version | 扩展独立于 ACP `protocolVersion` 演进 |

ACP 官方扩展规范允许通过 `_meta` 附加自定义数据、使用 `_` 开头的 custom request/notification，并建议在 capability `_meta` 中提前声明扩展能力；本设计遵循这些约束。[ACP Extensibility](https://agentclientprotocol.com/protocol/v1/extensibility)

## 4. 协议分层

```text
┌──────────────────────────────────────────────────────────────┐
│ ACP standard                                                 │
│ initialize / session/* / prompt / update / cancel / tools   │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│ anureo ACP extension                                            │
│ _anureo/cli/describe                                            │
│ _anureo/cli/execute                                             │
│ _anureo/job/list / get / cancel / update                        │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│ anureo command registry                                         │
│ models / tools / mcp / skills / memory / task / goal / review │
└──────────────────────────────┬───────────────────────────────┘
                               │
┌──────────────────────────────▼───────────────────────────────┐
│ Existing anureo services and stores                             │
│ config / SessionStore / AcpHub / MCP / workflow / task store │
└──────────────────────────────────────────────────────────────┘
```

### 4.1 两种调用入口

```text
用户对话 / IDE 快捷命令
    └── session/prompt

精确 CLI 控制面
    └── _anureo/cli/execute
```

`session/prompt` 适合自然语言和交互式 agent turn；`_anureo/cli/execute` 适合必须精确表达的查询、配置修改和长任务控制。

## 5. 初始化与能力协商

### 5.1 Agent capability

anureo 在标准 `initialize` response 中声明：

```json
{
  "jsonrpc": "2.0",
  "id": 1,
  "result": {
    "protocolVersion": 1,
    "agentInfo": {
      "name": "anureo",
      "version": "0.1.0"
    },
    "agentCapabilities": {
      "loadSession": true,
      "sessionCapabilities": {
        "list": {},
        "close": {},
        "delete": {}
      },
      "_meta": {
        "anureo.dev": {
          "extension": "anureo-cli",
          "version": "1",
          "protocolVersion": "2026-08-01",
          "describeMethod": "_anureo/cli/describe",
          "executeMethod": "_anureo/cli/execute",
          "jobMethods": [
            "_anureo/job/list",
            "_anureo/job/get",
            "_anureo/job/cancel"
          ],
          "features": {
            "commandCatalog": true,
            "structuredExecution": true,
            "longRunningJobs": true,
            "resumeJobs": true
          }
        }
      }
    }
  }
}
```

标准 ACP 的 session list/close/delete 等能力应直接使用对应标准字段和方法。anureo 私有能力只放在 `_meta["anureo.dev"]` 下。

### 5.2 Client capability

anureo CLI Client 在 `initialize` request 中声明自己的扩展能力：

```json
{
  "clientInfo": {
    "name": "anureo-cli",
    "version": "0.1.0"
  },
  "clientCapabilities": {
    "_meta": {
      "anureo.dev": {
        "cliClient": true,
        "interactiveTerminal": true,
        "jsonOutput": true,
        "jobUpdates": true
      }
    }
  }
}
```

server 不应仅因为 client 声明了 capability 就授予权限。capability 用于协商展示和传输能力，授权仍由 server 侧的 auth、owner、policy 和 command risk 决定。

## 6. Command Catalog

### 6.1 `_anureo/cli/describe`

请求：

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "method": "_anureo/cli/describe",
  "params": {
    "includeHidden": false,
    "includeSchemas": true,
    "category": "mcp"
  }
}
```

参数：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `includeHidden` | boolean | 否 | 是否包含内部 command，默认 false |
| `includeSchemas` | boolean | 否 | 是否返回完整 `inputSchema`，默认 true |
| `category` | string | 否 | 按类别过滤 |
| `cursor` | string | 否 | 大型 catalog 的分页 cursor，视为 opaque |

响应：

```json
{
  "jsonrpc": "2.0",
  "id": 10,
  "result": {
    "extension": "anureo-cli",
    "version": "1",
    "schemaVersion": "2026-08-01",
    "commands": [
      {
        "name": "models.list",
        "category": "models",
        "description": "List available models",
        "kind": "query",
        "longRunning": false,
        "requiresSession": false,
        "risk": "read",
        "inputSchema": {
          "type": "object",
          "properties": {
            "provider": {
              "type": "string"
            }
          },
          "additionalProperties": false
        }
      },
      {
        "name": "mcp.add",
        "category": "mcp",
        "description": "Add an MCP server",
        "kind": "mutation",
        "longRunning": false,
        "requiresSession": false,
        "risk": "config_write",
        "requiresConfirmation": true,
        "inputSchema": {
          "type": "object",
          "required": ["name", "transport"],
          "properties": {
            "name": { "type": "string", "minLength": 1 },
            "transport": {
              "type": "string",
              "enum": ["stdio", "http", "sse"]
            },
            "command": { "type": "string" },
            "args": {
              "type": "array",
              "items": { "type": "string" }
            },
            "url": { "type": "string", "format": "uri" },
            "env": {
              "type": "object",
              "additionalProperties": { "type": "string" }
            }
          },
          "additionalProperties": false
        }
      }
    ],
    "nextCursor": null
  }
}
```

### 6.2 Command metadata

每个 command 至少包含：

| 字段 | 说明 |
|---|---|
| `name` | 稳定的 dotted name，例如 `mcp.add` |
| `category` | `models`、`mcp`、`skills`、`goal` 等 |
| `description` | 面向 client 的说明 |
| `kind` | `query`、`mutation`、`execution`、`job` |
| `longRunning` | 是否返回 job |
| `requiresSession` | 是否必须绑定 ACP session |
| `risk` | `read`、`config_write`、`workspace_write`、`execute`、`destructive` |
| `requiresConfirmation` | client 是否应该显示确认 UI |
| `inputSchema` | JSON Schema 风格的结构化参数 |
| `outputSchema` | 成功结果的数据结构，可选 |
| `supportsDryRun` | 是否支持 dry-run |
| `supportsIdempotency` | 是否支持 `idempotencyKey` |

`inputSchema` 是 command 的输入契约，不等于 Clap 的完整 help 文本。Clap 仍然是本地 CLI 的用户界面定义，ACP catalog 是跨客户端的稳定 API 定义。

## 7. Command Execution

### 7.1 `_anureo/cli/execute`

请求：

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "method": "_anureo/cli/execute",
  "params": {
    "command": "models.list",
    "args": {
      "provider": "openai"
    },
    "context": {
      "cwd": "C:/work/project",
      "sessionId": null
    },
    "options": {
      "output": "json",
      "dryRun": false,
      "idempotencyKey": "models-list-001"
    },
    "_meta": {
      "traceparent": "00-80e1af08d2-7a0858536d2-01"
    }
  }
}
```

参数定义：

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `command` | string | 是 | 必须存在于 command registry |
| `args` | object | 是 | 通过该 command 的 input schema 校验 |
| `context.cwd` | absolute path | 否 | 默认使用 ACP session cwd 或 server 配置 cwd |
| `context.sessionId` | string | 否 | 绑定 session 的 command 必填 |
| `options.output` | `json`/`text` | 否 | server 内部以结构化数据为准，text 仅作为兼容字段 |
| `options.dryRun` | boolean | 否 | 仅对支持 dry-run 的 command 生效 |
| `options.idempotencyKey` | string | 否 | 防止 mutation 重复提交 |
| `_meta` | object | 否 | trace context 等扩展 metadata |

禁止出现以下参数：

```json
{
  "argv": ["任意", "字符串"],
  "shell": "任意命令",
  "commandLine": "任意命令行"
}
```

### 7.2 短命令响应

查询命令直接返回：

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "result": {
    "command": "models.list",
    "status": "completed",
    "data": {
      "models": [
        {
          "provider": "openai",
          "id": "gpt-4.1",
          "tier": "strong"
        }
      ]
    },
    "warnings": []
  }
}
```

标准输出文本不应成为协议的 source of truth。CLI 的 `--json`、ACP client UI 和 Web UI 都应消费 `data`；`text` 可作为人类阅读的兼容展示字段。

### 7.3 标准 JSON-RPC 错误

未知 command：

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "error": {
    "code": -32601,
    "message": "Method not found",
    "data": {
      "command": "unknown.command"
    }
  }
}
```

已知 command 但输入不合法：

```json
{
  "jsonrpc": "2.0",
  "id": 20,
  "error": {
    "code": -32602,
    "message": "Invalid params",
    "data": {
      "kind": "schema_validation_error",
      "command": "mcp.add",
      "fields": [
        {
          "path": "transport",
          "message": "must be one of: stdio, http, sse"
        }
      ],
      "retryable": false
    }
  }
}
```

业务错误使用 anureo 保留的 server error range：

| Code | 含义 |
|---:|---|
| `-32020` | command validation failed |
| `-32021` | permission denied |
| `-32022` | confirmation required |
| `-32023` | session not found/owner mismatch |
| `-32024` | job not found |
| `-32025` | command already running |
| `-32026` | conflict/idempotency mismatch |
| `-32027` | workspace boundary violation |
| `-32028` | command execution failed |

错误 `data` 必须包含：

```json
{
  "kind": "permission_denied",
  "command": "mcp.delete",
  "retryable": false,
  "userAction": "confirm_or_change_policy"
}
```

## 8. Job 协议

### 8.1 适用范围

下列 command 默认采用 job 模式：

```text
goal.run
goal.resume
review.run
review.batch
curator.run
evolve.run
task.create
task.continue
```

长任务不能让单个 JSON-RPC request 长时间占用连接；服务端应先返回 job accepted。

### 8.2 创建 Job

请求仍使用 `_anureo/cli/execute`：

```json
{
  "jsonrpc": "2.0",
  "id": 30,
  "method": "_anureo/cli/execute",
  "params": {
    "command": "goal.run",
    "args": {
      "goal": "修复当前项目所有测试失败",
      "verify": "cargo test"
    },
    "context": {
      "cwd": "C:/work/project"
    },
    "options": {
      "output": "json"
    }
  }
}
```

立即响应：

```json
{
  "jsonrpc": "2.0",
  "id": 30,
  "result": {
    "status": "accepted",
    "jobId": "job_01JACPCLI0001",
    "command": "goal.run",
    "createdAt": "2026-08-08T12:00:00Z"
  }
}
```

### 8.3 Job 更新

使用自定义 notification：

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo/job/update",
  "params": {
    "jobId": "job_01JACPCLI0001",
    "command": "goal.run",
    "status": "running",
    "phase": "verification",
    "message": "Running cargo test",
    "progress": {
      "current": 2,
      "total": 5
    },
    "sessionId": "sess_abc"
  }
}
```

允许的状态：

```text
accepted → queued → running → completed
                         ├── failed
                         ├── cancelled
                         └── expired
```

完成事件：

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo/job/update",
  "params": {
    "jobId": "job_01JACPCLI0001",
    "status": "completed",
    "exitCode": 0,
    "result": {
      "taskId": "task_123",
      "changedFiles": 4
    }
  }
}
```

### 8.4 Job 查询和取消

```text
_anureo/job/list
_anureo/job/get
_anureo/job/cancel
```

取消请求：

```json
{
  "jsonrpc": "2.0",
  "id": 31,
  "method": "_anureo/job/cancel",
  "params": {
    "jobId": "job_01JACPCLI0001",
    "reason": "user_requested"
  }
}
```

`_anureo/job/cancel` 只取消 CLI job。session 内普通 prompt 继续使用 ACP 标准的 `session/cancel`。

Job 必须持久化最小元数据：

```text
job_id
owner
command
session_id
cwd
status
created_at
updated_at
result_summary
error_summary
```

WebSocket 断开后 job 不能因为连接关闭而自动重复执行。client 重连后使用 `_anureo/job/get` 或 `_anureo/job/list` 恢复状态。

## 9. CLI 命令映射

### 9.1 标准 ACP 方法优先

| anureo CLI 语义 | 协议入口 |
|---|---|
| 对话 | `session/prompt` |
| 取消当前 prompt | `session/cancel` |
| session list | `session/list` |
| session load | `session/load` |
| session resume | `session/resume`，若 server 支持 |
| session close | `session/close` |
| session delete | `session/delete` |
| model/mode/effort | `session/set_config_option` |

ACP 的 session config options 已被设计为可扩展的 model、mode、reasoning 等 session-level selector，应优先用它表达 `--model`、`--tier`、`--effort`，而不是增加 `_anureo/model/set`。[Session Config Options](https://agentclientprotocol.com/protocol/v1/session-config-options)

### 9.2 anureo extension command registry

第一阶段建议注册：

| Command | 类型 | 是否长任务 | 是否修改 |
|---|---|---:|---:|
| `models.list` | query | 否 | 否 |
| `models.show` | query | 否 | 否 |
| `tools.list` | query | 否 | 否 |
| `tools.show` | query | 否 | 否 |
| `mcp.list` | query | 否 | 否 |
| `mcp.show` | query | 否 | 否 |
| `agents.list` | query | 否 | 否 |
| `skills.list` | query | 否 | 否 |
| `skills.show` | query | 否 | 否 |
| `memory.list` | query | 否 | 否 |
| `memory.search` | query | 否 | 否 |
| `mcp.add` | mutation | 否 | 是 |
| `mcp.edit` | mutation | 否 | 是 |
| `mcp.delete` | destructive | 否 | 是 |
| `mcp.enable` | mutation | 否 | 是 |
| `mcp.disable` | mutation | 否 | 是 |
| `skills.create` | mutation | 否 | 是 |
| `skills.edit` | mutation | 否 | 是 |
| `skills.delete` | destructive | 否 | 是 |
| `memory.edit` | mutation | 否 | 是 |
| `task.create` | job | 是 | 是 |
| `task.continue` | job | 是 | 是 |
| `goal.run` | job | 是 | 是 |
| `goal.resume` | job | 是 | 是 |
| `review.run` | job | 是 | 否/可能写入 |
| `review.batch` | job | 是 | 否/可能写入 |
| `curator.run` | job | 是 | 是 |
| `evolve.run` | job | 是 | 是 |

### 9.3 Slash Commands

ACP slash commands 适合作为用户体验层。Agent 可以通过 `available_commands_update` 发布 `/models`、`/review`、`/plan` 等命令，client 将其作为普通 `session/prompt` 发送。[ACP Slash Commands](https://agentclientprotocol.com/protocol/v1/slash-commands)

建议支持：

```text
/models
/tools
/review <target>
/goal <description>
/task <description>
```

不建议将以下命令实现为 slash command：

```text
/mcp delete
/skills delete
/memory edit
```

它们需要结构化参数、权限校验和确认，不应依赖自然语言或字符串解析。

## 10. 权限与安全

### 10.1 风险等级

```text
read
config_write
workspace_write
execute
destructive
```

server 必须根据 registry 中的风险等级执行 policy，不能只把 `requiresConfirmation` 当成 client 展示提示。

### 10.2 Confirmation

高风险 command 在执行前返回：

```json
{
  "jsonrpc": "2.0",
  "id": 40,
  "error": {
    "code": -32022,
    "message": "Confirmation required",
    "data": {
      "command": "mcp.delete",
      "risk": "destructive",
      "scope": "config",
      "target": "github",
      "confirmationToken": "confirm_01J...",
      "expiresAt": "2026-08-08T12:01:00Z"
    }
  }
}
```

第一版可以由 client 重新发送带有短期 token 的 execute request：

```json
{
  "command": "mcp.delete",
  "args": {
    "name": "github"
  },
  "options": {
    "confirmationToken": "confirm_01J..."
  }
}
```

token 必须绑定 owner、command、args 摘要和有效期，不能只绑定 command 名称。

### 10.3 Working directory

`cwd` 必须是 absolute path，并经过：

1. session owner 校验。
2. server workspace policy 校验。
3. command 所需 scope 校验。
4. 路径 canonicalization 和 workspace boundary 校验。

禁止 client 通过 `cwd` 绕过项目边界访问任意目录。

### 10.4 Auth 和 WebSocket

继续复用现有 `/acp` 的：

- `ANUREO_AUTH_TOKEN` Bearer token。
- `SessionOwner`。
- `AcpHub` owner isolation。
- WebSocket 最大消息/帧大小限制。
- browser `Origin` 校验。

`anureo --acp` 的 WebSocket Client 应与现有 `apps/acp/src/ws_bridge.rs` 复用 auth header 和连接逻辑。

## 11. 并发、幂等和断线

### 11.1 Query

查询 command 可以并发执行，但必须使用同一 owner 的 read scope。

### 11.2 Mutation

修改 command 默认按 `command + target` 做冲突检测。支持 `idempotencyKey` 的 mutation 在重复提交时必须返回原结果，而不是再次修改。

### 11.3 Job

Job 创建后与 WebSocket 生命周期解耦：

```text
WebSocket disconnect
    ↓
Job continues
    ↓
Client reconnect
    ↓
_anureo/job/get(jobId)
```

不得因为 client 重连而重新执行 `goal.run`、`review.run` 或 `task.create`。

### 11.4 ACP session prompt

ACP session 的普通 prompt 继续使用现有 session-level concurrency guard。CLI extension command 和 ACP prompt 不应共用一个无区分的锁；至少需要区分：

```text
session prompt lock
command mutation lock
job identity lock
```

## 12. 代码架构

### 12.1 新增模块

```text
apps/acp/src/extensions/
  mod.rs
  capabilities.rs
  cli.rs
  jobs.rs
  schema.rs
  errors.rs
```

职责：

| 文件 | 职责 |
|---|---|
| `mod.rs` | 注册 extension handlers |
| `capabilities.rs` | 生成 `_meta["anureo.dev"]` capability |
| `cli.rs` | 实现 describe/execute JSON-RPC handler |
| `jobs.rs` | Job store、状态机和 update notification |
| `schema.rs` | command metadata 和 input/output schema |
| `errors.rs` | JSON-RPC error 与 anureo error mapping |

### 12.2 Command Registry

建议新增可被 CLI 和 ACP 共用的 registry：

```rust
pub trait anureoCommand: Send + Sync {
    fn name(&self) -> &'static str;
    fn describe(&self) -> CommandDescription;

    async fn execute(
        &self,
        ctx: CommandContext,
        args: serde_json::Value,
    ) -> Result<CommandResult, CommandError>;
}

pub struct CommandContext {
    pub owner: SessionOwner,
    pub cwd: PathBuf,
    pub session_id: Option<String>,
    pub dry_run: bool,
    pub cancellation: CancellationToken,
}
```

推荐的调用关系：

```text
Clap CLI handler ────────┐
                         ├── CommandRegistry ── service/store
ACP extension handler ───┘
```

不要让 ACP handler 调用 `std::process::Command` 重新启动 `anureo` 自身，也不要从 ACP request 拼接成 shell command。

### 12.3 Job Store

第一阶段可以在 `AcpHub` 之外增加独立的 `AcpJobStore`：

```rust
pub struct AcpJob {
    pub id: JobId,
    pub owner: SessionOwner,
    pub command: String,
    pub session_id: Option<String>,
    pub cwd: PathBuf,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub result: Option<Value>,
    pub error: Option<JobError>,
}
```

第一阶段允许内存 store + process lifetime；如果目标是 server restart 后恢复，则需要把 job metadata 持久化到 anureo home，并为运行中的 process 增加 recovery 状态。该能力应单独作为后续阶段，不应假定仅凭 ACP reconnect 就能恢复 OS process。

## 13. `anureo --acp` Client 设计

CLI 侧建议新增：

```text
apps/acp/src/client.rs
apps/acp/src/ws_connection.rs
```

入口示例：

```bash
anureo --acp "检查当前项目"
anureo --acp --acp-url ws://127.0.0.1:3030/acp "列出模型"
anureo --acp -s sess_123 "继续上次任务"
anureo --acp --json "执行 models.list"
```

CLI ACP Client 的连接流程：

```text
connect WebSocket
    ↓
initialize
    ↓
如果有 --session-id：session/load
否则：session/new
    ↓
普通自然语言：session/prompt
精确命令：_anureo/cli/execute
    ↓
消费 session/update 或 _anureo/job/update
    ↓
输出 text / json
```

`apps/acp/src/ws_bridge.rs` 当前是 stdio↔WebSocket 原样 relay，应抽取其中的：

- URL 解析。
- health probe。
- server 自动拉起。
- Bearer header。
- reconnect backoff。

`client.rs` 不应直接复用 relay loop，因为它需要按 JSON-RPC `id` 路由 response，并识别 session update/job update。

## 14. 实施阶段

### Phase 1：只读 command extension

- 新增 `_meta["anureo.dev"]` capability。
- 新增 `_anureo/cli/describe`。
- 新增 `_anureo/cli/execute`。
- 实现：
  - `models.list`
  - `models.show`
  - `tools.list`
  - `tools.show`
  - `mcp.list`
  - `skills.list`
  - `memory.list`
- 增加 JSON-RPC schema/error 测试。

### Phase 2：标准 session 能力对齐

- 将 `session/list`、`session/close`、`session/delete` 接入 CLI Client。
- 将 `--model`、`--tier`、`--effort` 映射到 `session/set_config_option`。
- 发布 ACP `available_commands_update`。
- 实现 `/models`、`/review`、`/goal` 等 slash command。

### Phase 3：配置和资源修改

- 实现 `mcp.add/edit/delete/enable/disable`。
- 实现 `skills.create/edit/delete`。
- 实现 `memory.edit`。
- 加入 confirmation token、risk、scope 和 workspace boundary。

### Phase 4：Job 系统

- 新增 `AcpJobStore`。
- 实现 `_anureo/job/update`。
- 实现 `_anureo/job/get/list/cancel`。
- 接入 `goal`、`review`、`task`、`curator`。
- 测试断线后 job 不重复执行，重连后可查询状态。

### Phase 5：业务层收敛

- 把 CLI handler 中的业务逻辑下沉为 service。
- CLI 和 ACP 都通过 `CommandRegistry` 调用 service。
- 清理 ACP handler 中重复的配置、MCP、session 逻辑。
- 增加 extension version compatibility test。

## 15. 改动文件清单

| 文件 | 改动类型 | 说明 |
|---|---|---|
| `apps/cli/src/args.rs` | 修改 | 增加 `--acp`、`--acp-url`，校验与 subcommand 的组合 |
| `apps/cli/src/main.rs` | 修改 | 分流到 ACP Client，保留现有 `anureo acp` bridge |
| `apps/acp/src/client.rs` | 新增 | ACP Client request/response 和 prompt/job 消费 |
| `apps/acp/src/ws_connection.rs` | 新增/抽取 | WS 连接、探活、auth、server 拉起、重连 |
| `apps/acp/src/extensions/mod.rs` | 新增 | extension handler 注册 |
| `apps/acp/src/extensions/capabilities.rs` | 新增 | capability metadata |
| `apps/acp/src/extensions/cli.rs` | 新增 | describe/execute |
| `apps/acp/src/extensions/jobs.rs` | 新增 | job 生命周期 |
| `apps/acp/src/extensions/schema.rs` | 新增 | command descriptions 和 schemas |
| `apps/acp/src/extensions/errors.rs` | 新增 | extension error mapping |
| `apps/acp/src/stdio_loop.rs` | 修改 | 注册 extension request handlers |
| `apps/server/src/acp_hub.rs` | 修改 | 按 owner/session 维护 job 关联和通知路由，视实现阶段决定 |
| `apps/server/src/handlers/acp.rs` | 修改 | 接入 extension dispatch 和权限上下文 |
| `apps/cli/src/command_registry.rs` | 新增或重构 | CLI/ACP 共用 command registry |
| `apps/cli/src/handlers/*` | 重构 | 将业务逻辑下沉为 service，保留 Clap adapter |
| `apps/server/tests/acp_ws_e2e.rs` | 修改 | 增加 extension over WebSocket 测试 |
| `apps/acp/tests/*` | 新增 | client、schema、error、job 单元测试 |
| `docs/design/acp-cli-extension.md` | 新增 | 本设计文档 |

## 16. 测试计划

| 测试 | 验证点 |
|---|---|
| `initialize_advertises_anureo_extension` | `_meta["anureo.dev"]` 存在且版本正确 |
| `describe_returns_command_catalog` | catalog、schema、风险字段完整 |
| `describe_filters_category` | category 过滤和 cursor 行为 |
| `execute_models_list` | 查询命令返回结构化 data |
| `execute_rejects_unknown_command` | 未知 command 返回 `-32601` |
| `execute_validates_schema` | 非法参数返回 `-32602` |
| `execute_requires_confirmation` | 高风险 mutation 返回 `-32022` |
| `confirmation_token_is_bound` | token 不能跨 command/args/owner 复用 |
| `cwd_boundary_is_enforced` | 越界路径返回 `-32027` |
| `idempotent_mutation_returns_original_result` | 重复 idempotency key 不重复写入 |
| `job_execute_returns_immediately` | 长任务立即返回 jobId |
| `job_updates_are_ordered` | 状态转换不会倒退 |
| `job_cancel_stops_work` | cancel 后状态为 cancelled |
| `job_reconnect_does_not_duplicate` | 断线重连不重新执行 job |
| `session_methods_use_standard_acp` | 不重复实现标准 session API |
| `slash_commands_are_advertised` | `available_commands_update` 正确 |
| `cli_and_acp_share_registry` | 同一 command 的 CLI/ACP 结果一致 |
| `acp_ws_auth_is_required` | Bearer token 和 owner isolation 生效 |
| `oversized_extension_message_is_rejected` | 复用现有 1 MiB 限制 |
| `unknown_extension_notification_is_ignored` | 遵循 ACP notification 兼容语义 |

验收命令：

```powershell
cargo test -p acp
cargo test -p anureo-server --test acp_ws_e2e
cargo test -p anureo-cli
cargo test --workspace
cargo run -p anureo-cli -- --acp "检查当前项目"
```

### 16.1 Node.js BDD E2E

Node.js 黑盒测试放在 `e2e/`，不依赖 Rust 内部类型。测试直接启动已构建的 `anureo` binary，通过 `anureo acp` 的 stdin/stdout 与 ACP server 的 `/acp` WebSocket 间接验证真实链路。

```text
Node test runner
    │ stdin/stdout
    ▼
anureo acp
    │ WebSocket
    ▼
anureo server /acp
```

当前落地的 living specification：

```text
e2e/features/acp/anureo-acp-stdio.feature
e2e/features/acp/cli-acp-client.feature
```

当前可执行的 Node BDD tests：

```text
e2e/tests/acp-bdd/anureo-acp-stdio.test.mjs
e2e/tests/acp-bdd/cli-acp-client.test.mjs
```

运行：

```powershell
cargo build -p anureo-cli
npm --prefix e2e run test:bdd:acp
```

`anureo acp` 的 initialize、bridge 重启和 session/load 场景必须通过；`anureo --acp` 的 session 创建、恢复、JSON 输出和 prompt streaming 场景也必须通过。prompt 场景使用 deterministic Node ACP WebSocket fixture，验证 CLI Client 的 ACP 协议行为而不依赖真实模型 provider。

## 17. 向后兼容性

### 17.1 对普通 ACP Client

- 普通 ACP Client 仍可只使用 `initialize`、`session/new`、`session/prompt`。
- 不认识 `_anureo/...` 的 client 不受影响。
- 不支持扩展时，server 继续对标准 ACP 请求提供服务。
- 未识别的 custom notification 应忽略；未知 custom request 返回标准 method-not-found。

### 17.2 对现有 `anureo acp`

保持：

```bash
anureo acp
anureo acp ws://127.0.0.1:3030/acp
```

它继续作为 IDE stdio bridge，不改为 CLI command executor。

新增：

```bash
anureo --acp "message"
anureo --acp --acp-url ws://host:port/acp "message"
```

### 17.3 扩展版本

扩展版本不改变 ACP `protocolVersion`。client 应根据：

```text
agentCapabilities._meta["anureo.dev"].version
agentCapabilities._meta["anureo.dev"].protocolVersion
```

决定是否调用某个 command。command catalog 是运行时权威来源，client 不应仅根据 anureo 二进制版本推断 command 存在。

## 18. 未决问题

| 问题 | 当前建议 |
|---|---|
| `CommandRegistry` 放在 `apps/cli` 还是新 crate | 第一阶段放 `apps/cli`，若 server 直接复用困难再下沉到 `foundation/cli-command` |
| job 是否跨 server restart 恢复 | 第一阶段不承诺；先保证 WebSocket reconnect 不重复执行 |
| confirmation 是否纳入 ACP 标准 permission | 第一阶段使用 anureo extension error/token；后续评估 ACP permission request |
| 是否暴露完整 skill 内容 | 默认只返回 metadata，内容通过显式 `skills.show` 查询 |
| MCP 配置是否允许远程修改 | 默认关闭；需要显式 server policy 和高风险 confirmation |
| `output: text` 是否保留 | 保留为展示兼容字段，但结构化 `data` 是正式契约 |
| 多 client 同时订阅 job | 以 owner 为边界，后续增加 job subscriber/cursor |
| 是否支持 ACP v2 | 等 v2 稳定后单独建立 migration 文档，不在本设计中混用 v1/v2 schema |

## 19. 验收标准

本设计完成的最低判断标准：

1. `initialize` 能发现 `anureo.dev` extension。
2. client 能通过 `_anureo/cli/describe` 获取命令及参数 schema。
3. `models.list`、`tools.list`、`mcp.list` 至少三个 query command 可通过 WebSocket 执行。
4. 非法 command、非法参数、权限拒绝均返回结构化 JSON-RPC error。
5. 至少一个 mutation command 支持 confirmation 和 workspace boundary 校验。
6. 至少一个长任务返回 job，并支持 update、get、cancel。
7. CLI 本地入口和 ACP 入口使用相同 command service，不复制业务逻辑。
8. 不认识 anureo extension 的 ACP Client 仍能正常执行标准 ACP 对话。
9. WebSocket 断线重连不会重复执行已接受的 job。
10. `cargo test -p acp`、`cargo test -p anureo-server --test acp_ws_e2e` 和 `cargo test -p anureo-cli` 通过。

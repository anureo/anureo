# Session 配置

> **命名空间**: 标准 ACP v1
> **实现状态**: ✅ 已实现
> **源码**: `apps/acp/src/agent.rs`、`apps/acp/src/session_config_store.rs`

---

## 1. `session/set_config_option`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | 无额外要求 |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 11,
  "method": "session/set_config_option",
  "params": {
    "sessionId": "thread-abc123",
    "category": "model",
    "configId": "model",
    "value": "glm-4.6",
    "booleanValue": null
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sessionId` | string | 是 | 目标 session |
| `category` | string | 是 | 配置类别 |
| `configId` | string | 是 | 配置项 ID |
| `value` | string | 否 | 字符串值（与 `booleanValue` 二选一） |
| `booleanValue` | bool | 否 | 布尔值（需要 `unstable_boolean_config` feature） |

### Config Category

| Category | configId | value 类型 | 说明 |
|---|---|---|---|
| `model` | `model` | string | 主模型 ID |
| `model` | `effort` | string | 推理努力程度 |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 11,
  "result": {}
}
```

### 逻辑说明

1. 验证 session 存在且属于当前 connection
2. 验证 `category` + `configId` 组合合法
3. 更新 `SessionConfig`（内存中的 `SessionEntry.session_config`）
4. 持久化到 `SessionConfigStore`（SQLite）
5. 如果 session 有 active generation，**config 变更不应用于当前 generation**——只在下次 prompt 生效
6. 成功后通过 `session/update` notification 发送 `config_option_update`

### Rust 类型

```rust
async fn set_session_config_option(
    &self, args: SetSessionConfigOptionRequest
) -> agent_client_protocol::Result<SetSessionConfigOptionResponse>

// SessionConfig 字段映射
pub struct SessionConfig {
    pub current_agent: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}
```

### Error

| Error code | 触发条件 |
|---|---|
| `session_not_found` | session 不存在 |
| `Invalid Params (-32602)` | 未知 category/configId 或 value 类型不匹配 |

---

## 2. `session/set_mode`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Agent request |
| 能力 | 无额外要求 |
| anureo 状态 | ✅ 已实现 |

### Request

```json
{
  "jsonrpc": "2.0",
  "id": 12,
  "method": "session/set_mode",
  "params": {
    "sessionId": "thread-abc123",
    "mode": "dev"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `sessionId` | string | 是 | 目标 session |
| `mode` | string | 是 | Agent profile ID |

### Response

```json
{
  "jsonrpc": "2.0",
  "id": 12,
  "result": {}
}
```

### 逻辑说明

1. 验证 session 存在
2. 验证 `mode` 是已注册的 agent profile（`AgentRegistry`）
3. 更新 `SessionConfig.current_agent`
4. 持久化到 `SessionConfigStore`
5. 成功后通过 `session/update` notification 发送 `current_mode_update`

### Agent Profile

anureo 的 agent profile 通过 `AgentRegistry` 管理。每个 profile 定义：
- 系统提示词模板
- 可用工具集
- 行为参数

常见 profile: `default`、`ask`（只回答不修改）、`dev`（全功能开发）

### Rust 类型

```rust
async fn set_session_mode(
    &self, args: SetSessionModeRequest
) -> agent_client_protocol::Result<SetSessionModeResponse>
```

### Error

| Error code | 触发条件 |
|---|---|
| `session_not_found` | session 不存在 |
| `Invalid Params (-32602)` | 未知的 mode/profile ID |

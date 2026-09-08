# Question 扩展

> 命名空间: `_anureo.dev/question/*`
> Capability key: `question`
> 实现状态: ❌ 未实现

---

## Capability

```json
{
  "question": {
    "request": true,
    "reply": true,
    "cancel": true
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `question.request` | bool | Server 可发起 question request |
| `question.reply` | bool | Client 可回复 question |
| `question.cancel` | bool | 可取消 pending question |

### 启用条件

本扩展**仅在 ACP 标准 elicitation（`session/request_permission`）无法表达 Anureo question 语义时启用**。anureo question 支持以下 elicitation 不具备的能力：

- 多选项 + 自由文本输入混合（elicitation 仅支持单一 input）
- 选项级 metadata（描述、图标、disabled 状态）
- 超时自动取消
- 多轮 question（基于前一个 reply 追问）

Client 在 `initialize` 时通过 `agentCapabilities._meta["anureo.dev"].question` 判断是否启用。若未声明，question 语义回退到标准 elicitation。

---

## Methods

### `_anureo.dev/question/request`

| 项目 | 内容 |
|---|---|
| 方向 | Server → Client（reverse-RPC request） |
| Capability | `question.request` |
| 权限 | 无额外 server-side authorization（由 Agent 内部逻辑发起） |
| Timeout | 默认 120s；Agent 可在 `timeoutMs` 中覆盖 |

Server（Agent）请求 Client 展示一个 Anureo question，等待用户选择或输入。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": "ext-42",
  "method": "_anureo.dev/question/request",
  "params": {
    "questionId": "q-2025-001",
    "title": "选择部署目标",
    "prompt": "当前分支已通过 CI，请选择部署环境：",
    "choices": [
      {
        "value": "staging",
        "label": "Staging",
        "description": "预发布环境，用于集成测试",
        "disabled": false
      },
      {
        "value": "production",
        "label": "Production",
        "description": "生产环境，需要管理员审批",
        "disabled": true
      }
    ],
    "allowFreeText": true,
    "freeTextPlaceholder": "输入自定义环境名称...",
    "defaultChoice": "staging",
    "timeoutMs": 60000,
    "sessionId": "session-abc123"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `questionId` | string | 是 | 唯一标识，用于 reply/cancel 关联 |
| `title` | string | 否 | UI 标题（简短） |
| `prompt` | string | 是 | 问题正文 |
| `choices` | Choice[] | 否 | 选项列表；为空表示纯自由文本输入 |
| `choices[].value` | string | 是 | 选项值（reply 中返回） |
| `choices[].label` | string | 是 | 选项显示标签 |
| `choices[].description` | string | 否 | 选项描述 |
| `choices[].disabled` | bool | 否 | 是否禁用，默认 `false` |
| `allowFreeText` | bool | 否 | 是否允许自由文本输入，默认 `false` |
| `freeTextPlaceholder` | string | 否 | 自由文本输入框 placeholder |
| `defaultChoice` | string | 否 | 默认选中的 choice value |
| `timeoutMs` | number | 否 | 超时毫秒数；超时后 Agent 视为取消 |
| `sessionId` | string | 否 | 关联的 session ID（多 session 环境下路由用） |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": "ext-42",
  "result": {
    "questionId": "q-2025-001",
    "status": "answered",
    "choice": "staging",
    "freeText": null
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `questionId` | string | 回显 questionId |
| `status` | `"answered"` \| `"cancelled"` \| `"timeout"` | 回复状态 |
| `choice` | string\|null | 用户选择的 choice value（`answered` 时必填） |
| `freeText` | string\|null | 自由文本输入内容（`allowFreeText` 为 true 且用户填写时） |

#### 逻辑说明

1. **唯一性**: `questionId` 在同一 Agent 运行时内全局唯一。Client 同一时刻可能收到多个 pending question。
2. **超时**: Server 端维护 `timeoutMs` 计时器；超时后 Server 不等待 Client response，继续以默认行为执行。Client 超时后返回的 response 被 Server 丢弃。
3. **默认选择**: `defaultChoice` 用于超时或用户跳过时的默认行为；Agent 逻辑必须能处理默认值。
4. **方向**: 本 method 是 reverse-RPC（Server → Client），与标准 ACP `session/request_permission` 方向一致但语义不同。Client 收到后渲染 UI 并等待用户操作。
5. **回退**: 若 Client 未声明 `question` capability，Agent 不应发起此 request，回退到标准 elicitation 或默认决策。
6. **多轮**: Agent 可在前一个 question 的 reply 后立即发起新 question（追问），Client 按 `questionId` 区分。

#### Rust 类型

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionChoice {
    pub value: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub question_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<QuestionChoice>,
    #[serde(default)]
    pub allow_free_text: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_text_placeholder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QuestionStatus {
    Answered,
    Cancelled,
    Timeout,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionReply {
    pub question_id: String,
    pub status: QuestionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Capability Not Supported (-32001)` | Client 未声明 `question` capability |
| `Invalid Params (-32602)` | `prompt` 为空或 `questionId` 缺失 |
| `Internal Error (-32603)` | Agent 内部状态异常（如 question 引擎未初始化） |

---

### `_anureo.dev/question/reply`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `question.reply` |
| 权限 | 无额外 authorization（回复自己收到的 question） |

Client 返回用户对 question 的选择或输入。也可作为 Client 主动取消的途径。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 101,
  "method": "_anureo.dev/question/reply",
  "params": {
    "questionId": "q-2025-001",
    "status": "answered",
    "choice": "staging",
    "freeText": null
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `questionId` | string | 是 | 关联的 question ID |
| `status` | `"answered"` \| `"cancelled"` | 是 | 用户操作类型 |
| `choice` | string | 条件必填 | `status` 为 `answered` 且 question 有 choices 时必填 |
| `freeText` | string | 否 | 自由文本输入内容 |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 101,
  "result": {
    "accepted": true
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `accepted` | bool | Server 是否接受了此 reply |

#### 逻辑说明

1. **幂等**: 同一 `questionId` 的第二次 reply 返回 `accepted: false`，不报错（前一个 reply 已被处理）。
2. **竞态**: 若 Server 端已超时并继续执行，后到的 reply 被丢弃，返回 `accepted: false`。
3. **校验**: `choice` 必须在 question 的 choices 列表中（除非 `allowFreeText` 为 true）。无效 choice 返回 `Invalid Params`。
4. **取消语义**: `status: "cancelled"` 表示用户主动关闭了 question UI；Agent 收到后可决定后续行为（如使用默认值或中止操作）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionReplyRequest {
    pub question_id: String,
    pub status: QuestionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionReplyResponse {
    pub accepted: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `questionId` 不存在或 `choice` 不在 choices 中 |
| `Method Not Found (-32601)` | 扩展未声明 |
| `Internal Error (-32603)` | Agent question 引擎内部错误 |

---

### `_anureo.dev/question/cancel`

| 项目 | 内容 |
|---|---|
| 方向 | Client → Server request |
| Capability | `question.cancel` |
| 权限 | 无额外 authorization |

Client 主动取消一个 pending question。与 `question/reply` 的 `status: "cancelled"` 语义相同，但用于无需等待 UI 交互的场景（如 Client 切换 session、关闭窗口）。

#### Request

```json
{
  "jsonrpc": "2.0",
  "id": 102,
  "method": "_anureo.dev/question/cancel",
  "params": {
    "questionId": "q-2025-001",
    "reason": "user_navigation"
  }
}
```

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `questionId` | string | 是 | 要取消的 question ID |
| `reason` | string | 否 | 取消原因（日志用，不影响逻辑） |

#### Response

```json
{
  "jsonrpc": "2.0",
  "id": 102,
  "result": {
    "cancelled": true
  }
}
```

| 字段 | 类型 | 说明 |
|---|---|---|
| `cancelled` | bool | 是否成功取消（`false` 表示 question 已被 reply 或已超时） |

#### 逻辑说明

1. **幂等**: 取消已完成的 question 不报错，返回 `cancelled: false`。
2. **Agent 行为**: 收到 cancel 后 Agent 决定是否使用默认值或中止当前操作链。
3. **批量取消**: Client 断开连接时，Server 自动 cancel 所有 pending question（无需 Client 逐个调用）。

#### Rust 类型

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionCancelRequest {
    pub question_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuestionCancelResponse {
    pub cancelled: bool,
}
```

#### Error

| Error code | 触发条件 |
|---|---|
| `Invalid Params (-32602)` | `questionId` 缺失 |
| `Method Not Found (-32601)` | 扩展未声明 |

---

## Notifications

本扩展无 notification。Question 生命周期通过 request/response 完成。

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| （无） | （无） | Question 是 transient 交互，不持久化；重连后 pending question 自动取消 |

> Client 重连后无需 resync question 状态。Server 在连接断开时自动 cancel 所有 pending question。Agent 通过默认值或中止操作处理断开期间的 question。

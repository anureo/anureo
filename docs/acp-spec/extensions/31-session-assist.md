# Session Assist（会话辅助）

> 命名空间: `_anureo.dev/session-assist/*`
> Capability key: `session-assist`

## Capability

```json
{
  "session-assist": {
    "recap": true
  }
}
```

- 声明 `session-assist` capability 后，client 将接收 server 自动生成的 recap notification。
- Session Assist 是 **server-side event-driven watcher**，完全由 server SSE 事件驱动。
- Extension 只推送结果，**不触发生成**——client 没有对应的 request method 来主动请求 recap。

### 生成流程

```
Session idle ≥ 60s (no active generation, no new prompt)
  → Server-side watcher detects idle
  → Invoke small model (see 32-small-model.md)
  → Generate recap of last assistant turn + suggested follow-up
  → PATCH to session metadata: metadata.anureo.assist
  → Emit _anureo.dev/session-assist/recap notification
  → Client receives notification OR detects via session/update (metadata change)
```

- "Idle" 定义：session 的最后一个 generation 完成后，60 秒内没有新的 `session/prompt`。
- 如果 session 没有任何 generation 历史（空 session），不触发 recap。
- Recap 只针对最近的 assistant 回复，不总结整个 session 历史。
- 同一个 assistant turn 只生成一次 recap——重复 idle 不重新生成。

---

## Methods

Session Assist 域**没有 request method**。所有交互通过 notification 和 session metadata 完成。

---

## Notifications

### `_anureo.dev/session-assist/recap`

| 项目 | 内容 |
|---|---|
| 方向 | Server → Client notification |
| 触发 | Server-side watcher 检测到 session idle 60s 后，small model 生成了 recap |
| Capability | `session-assist.recap` |

```json
{
  "jsonrpc": "2.0",
  "method": "_anureo.dev/session-assist/recap",
  "params": {
    "sessionId": "sess_abc123",
    "recap": "将 src/auth.rs 的密码验证从明文比较改为 bcrypt::verify，移除了 2 个 unsafe 块，新增了 3 个单元测试。",
    "suggestions": [
      "为新的 bcrypt 验证添加集成测试",
      "检查其他模块是否还有明文密码比较",
      "更新 CHANGELOG.md 记录此安全改进"
    ],
    "generatedAt": "2025-08-19T10:01:00Z",
    "modelUsed": "glm-4-flash",
    "turnIndex": 3
  }
}
```

**params 字段:**

| 字段 | 类型 | 说明 |
|---|---|---|
| `sessionId` | string | 关联的 session ID |
| `recap` | string | 上一轮 assistant 回复的摘要（自然语言） |
| `suggestions` | string[] | 建议的 follow-up prompt 列表（0-5 条） |
| `generatedAt` | string | Recap 生成时间 |
| `modelUsed` | string | 使用的 small model 名称 |
| `turnIndex` | number | 被总结的 assistant turn 索引 |

**Rust 类型:**

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize)]
pub struct SessionAssistRecapParams {
    pub session_id: String,
    pub recap: String,
    pub suggestions: Vec<String>,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub model_used: String,
    pub turn_index: u32,
}

/// Written to session metadata at `metadata.anureo.assist`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistMetadata {
    pub recap: String,
    pub suggestions: Vec<String>,
    pub generated_at: chrono::DateTime<chrono::Utc>,
    pub model_used: String,
    pub turn_index: u32,
}
```

**逻辑说明:**

1. Server-side watcher 监控所有活跃 session 的 generation 状态。
2. Session idle 60 秒后，watcher 调用 small model（见 `32-small-model.md`）生成 recap。
3. Small model 优先使用 session 当前的 provider/model；`restrictToPreferredProvider` 为 true 时禁止全局 fallback。
4. 如果 small model 调用失败（provider 不可用、rate limit 等），**静默跳过**——不发送 notification，不修改 metadata。
5. Recap 结果 PATCH 到 session metadata `metadata.anureo.assist`，通过标准 `session/update`（`session_info_update._meta`）传输。
6. Extension notification 和 session metadata update 可能同时到达 client——client 应以 notification 为准（它是 metadata 变化的语义信号）。
7. `suggestions` 为 0-5 条建议，每条是可直接作为 prompt 发送的自然语言文本。
8. `turnIndex` 标识被总结的 assistant turn，client 可以据此定位 session 历史中的对应位置。
9. **此扩展不产生 ACP `session/update` 的 message 流变化**——recap 只写入 metadata，不添加 assistant message。

### 静默失败行为

| 场景 | 行为 |
|---|---|
| Small model provider 不可用 | 静默跳过，无 notification |
| Small model 返回错误 | 静默跳过，无 notification |
| Session 在 60s 内收到新 prompt | 取消 pending recap 生成 |
| Session 被关闭/删除 | 清理 watcher，无 notification |
| Client 未声明 `session-assist` capability | Server 不生成 recap（节省资源） |

---

## Reconnect Resync 映射

| Notification | Authoritative method | 快照保证 |
|---|---|---|
| `session-assist/recap` | `session/load`（通过 `session/update`） | 重建 session metadata（含 `metadata.anureo.assist`） |

- Session Assist 的 recap 结果持久化在 session metadata 中（`metadata.anureo.assist`），不是临时状态。
- Client 重连后通过标准 `session/load` 恢复完整 session 状态，metadata 中的 recap 数据随之恢复。
- Recap notification 本身不重放——client 重连后从 session metadata 中读取最新的 recap 数据。
- 如果 `session/load` 失败，client 保留旧的 metadata（显示 stale 指示）。

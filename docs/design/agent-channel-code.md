# Agent Channel：代码实现方案

**创建时间**：2025-08-19  
**状态**：实现方案  
**关联设计**：[agent-channel.md](./agent-channel.md)

---

## 1. 总览

基于现有架构，Channel 实现涉及以下改动：

| 改动类型 | 位置 | 内容 |
|---------|------|------|
| 新增 crate | `agent/channel/` | Channel 基础设施 |
| 新增 crate | `agent/patterns/` | 应用模式层（Design Review） |
| 新增 crate | `agent/tool/tool-channel/` | Channel tools |
| 修改 | `agent/agent-core/src/state.rs` | ReActState 增加 channel inbox 字段 |
| 修改 | `agent/agent-core/src/agent/react/observe_node.rs` | 注入 channel 消息 |
| 修改 | `agent/agent-core/src/agent/react/runner.rs` | 构建 graph 时注入 ChannelManager |
| 修改 | `agent/tool/tool-core/src/context.rs` | ToolCallContext 增加 channel 句柄 |
| 修改 | `Cargo.toml` | workspace members |

---

## 2. Crate 结构

### 2.1 `agent/channel/` — Channel 基础设施

```
agent/channel/
├── Cargo.toml
└── src/
    ├── lib.rs              # mod 声明 + re-export
    ├── types.rs            # Channel, Message, ChannelMode, ChannelLifetime
    ├── inbox.rs            # ChannelInbox — per-agent 未读消息队列
    ├── registry.rs         # ChannelRegistry — agent↔channel 注册表
    ├── manager.rs          # ChannelManager — channel CRUD + 消息分发
    └── dispatcher.rs       # WakeupDispatcher — 唤醒逻辑
```

### 2.2 `agent/tool/tool-channel/` — Channel Tools

```
agent/tool/tool-channel/
├── Cargo.toml
└── src/
    ├── lib.rs              # mod 声明 + re-export + ToolSpec 定义
    ├── create.rs           # channel_create tool
    ├── send.rs             # channel_send tool
    ├── join.rs             # channel_join tool
    ├── leave.rs            # channel_leave tool
    ├── close.rs            # channel_close tool
    └── list.rs             # channel_list tool
```

### 2.3 `agent/patterns/` — 应用模式层

```
agent/patterns/
├── Cargo.toml
└── src/
    ├── lib.rs
    └── design_review/
        ├── mod.rs
        ├── types.rs        # Issue, Severity, IssueStatus, ConvergenceCheck
        ├── tracker.rs      # IssueTracker — issue 追踪 + 收敛判断
        └── judge.rs        # Judge 仲裁逻辑
```

---

## 3. `agent/channel/` 详细设计

### 3.1 `types.rs` — 核心类型

```rust
//! Channel 核心类型定义

use std::time::{Duration, Instant};
use serde::{Deserialize, Serialize};

/// Channel 唯一标识
pub type ChannelId = String;

/// Agent 唯一标识（复用现有 thread_id 体系）
pub type AgentId = String;

/// 一个多 agent 对话通道
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Channel {
    pub id: ChannelId,
    pub creator: AgentId,
    pub topic: String,
    pub mode: ChannelMode,
    pub moderator: Option<AgentId>,
    pub participants: Vec<ChannelParticipant>,
    #[serde(skip)]
    pub messages: Vec<ChannelMessage>,
    pub lifetime: ChannelLifetime,
    pub created_at: String,
    pub last_active: String,
    pub closed: bool,
}

/// Channel 参与者（携带 agent profile 信息，用于自动派生）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelParticipant {
    pub id: AgentId,
    /// agent profile 名称（如 "system-architect"）
    pub profile: String,
    /// 是否已派生（运行时状态）
    pub spawned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ChannelMode {
    Chat,
    Meeting,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelLifetime {
    Task(String),
    Manual,
    Ttl(u64),  // seconds
}

/// Channel 中的一条消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMessage {
    pub id: String,
    pub channel_id: ChannelId,
    pub from: AgentId,
    pub content: String,
    pub mentions: Vec<AgentId>,
    pub timestamp: String,
    pub metadata: Option<ChannelMessageMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelMessageMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub file_refs: Vec<String>,
    /// 自由扩展字段，供 Pattern 层使用
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Agent 被唤醒的原因
#[derive(Debug, Clone)]
pub enum WakeReason {
    NewChannelMessage { channel_id: ChannelId, messages: Vec<ChannelMessage> },
    Mentioned { channel_id: ChannelId, messages: Vec<ChannelMessage> },
    GrantedFloor { channel_id: ChannelId, message: ChannelMessage },
    ChannelClosed { channel_id: ChannelId },
}
```

### 3.2 `inbox.rs` — Per-Agent 消息队列

```rust
//! ChannelInbox: 每个 agent 的未读消息队列
//!
//! 基于 tokio::sync::Notify 实现 push 投递。
//! Notify 的 permit 会累积，park 时立即消费，不会丢失通知。

use std::sync::Arc;
use parking_lot::Mutex;
use tokio::sync::Notify;
use crate::types::{ChannelMessage, WakeReason};

/// 每个 agent 持有一个 ChannelInbox
#[derive(Clone)]
pub struct ChannelInbox {
    /// 未读 channel 事件（按到达顺序排列）
    pending: Arc<Mutex<Vec<WakeReason>>>,
    /// 唤醒信号
    notify: Arc<Notify>,
}

impl ChannelInbox {
    pub fn new() -> Self {
        Self {
            pending: Arc::new(Mutex::new(Vec::new())),
            notify: Arc::new(Notify::new()),
        }
    }

    /// 投递唤醒事件：先入队，再 notify
    pub fn deliver(&self, reason: WakeReason) {
        self.pending.lock().push(reason);
        self.notify.notify_one();
    }

    /// 取出所有待处理的唤醒事件（非阻塞）
    pub fn drain(&self) -> Vec<WakeReason> {
        let mut guard = self.pending.lock();
        std::mem::take(&mut *guard)
    }

    /// 是否有待处理的事件
    pub fn has_pending(&self) -> bool {
        !self.pending.lock().is_empty()
    }

    /// park 直到有新事件
    pub async fn park_until_wakeup(&self) {
        if !self.has_pending() {
            self.notify.notified().await;
        }
    }

    /// 将 channel 消息格式化为可注入 context 的文本
    pub fn format_for_context(reasons: &[WakeReason]) -> Option<String> {
        let mut lines = Vec::new();
        for reason in reasons {
            match reason {
                WakeReason::NewChannelMessage { channel_id, messages }
                | WakeReason::Mentioned { channel_id, messages } => {
                    let tag = matches!(reason, WakeReason::Mentioned { .. })
                        .then_some("@you").unwrap_or("");
                    for msg in messages {
                        lines.push(format!(
                            "[Channel: {}{}] {} ({}): {}",
                            channel_id, tag, msg.from, msg.timestamp, msg.content
                        ));
                    }
                }
                WakeReason::GrantedFloor { channel_id, message } => {
                    lines.push(format!(
                        "[Channel: {} @you (floor granted)] {}: {}",
                        channel_id, message.from, message.content
                    ));
                }
                WakeReason::ChannelClosed { channel_id } => {
                    lines.push(format!("[Channel: {} has been closed]", channel_id));
                }
            }
        }
        if lines.is_empty() { None } else { Some(lines.join("\n")) }
    }
}
```

### 3.3 `registry.rs` — 注册表

```rust
//! ChannelRegistry: 管理 agent ↔ channel 的订阅关系

use std::collections::{HashMap, HashSet};
use parking_lot::RwLock;
use std::sync::Arc;
use crate::inbox::ChannelInbox;
use crate::types::*;

/// 注册表（线程安全，可 clone 共享）
#[derive(Clone)]
pub struct ChannelRegistry {
    inner: Arc<RwLock<RegistryInner>>,
}

struct RegistryInner {
    /// channel_id → channel 实例
    channels: HashMap<ChannelId, Channel>,
    /// agent_id → inbox
    inboxes: HashMap<AgentId, ChannelInbox>,
    /// agent_id → 订阅的 channel_id 集合
    subscriptions: HashMap<AgentId, HashSet<ChannelId>>,
}

impl ChannelRegistry {
    pub fn new() -> Self {
        Self { inner: Arc::new(RwLock::new(RegistryInner {
            channels: HashMap::new(),
            inboxes: HashMap::new(),
            subscriptions: HashMap::new(),
        }))}
    }

    /// 获取或创建 agent 的 inbox
    pub fn get_or_create_inbox(&self, agent_id: &str) -> ChannelInbox {
        let mut inner = self.inner.write();
        inner.inboxes
            .entry(agent_id.to_string())
            .or_insert_with(ChannelInbox::new)
            .clone()
    }

    /// 注册 agent 到 channel
    pub fn subscribe(&self, channel_id: &str, agent_id: &str) {
        let mut inner = self.inner.write();
        inner.subscriptions
            .entry(agent_id.to_string())
            .or_default()
            .insert(channel_id.to_string());
        if let Some(ch) = inner.channels.get_mut(channel_id) {
            if !ch.participants.contains(&agent_id.to_string()) {
                ch.participants.push(agent_id.to_string());
            }
        }
    }

    /// 取消注册
    pub fn unsubscribe(&self, channel_id: &str, agent_id: &str) {
        let mut inner = self.inner.write();
        if let Some(subs) = inner.subscriptions.get_mut(agent_id) {
            subs.remove(channel_id);
        }
        if let Some(ch) = inner.channels.get_mut(channel_id) {
            ch.participants.retain(|p| p != agent_id);
        }
    }

    /// 获取 agent 参与的所有 channel
    pub fn get_agent_channels(&self, agent_id: &str) -> Vec<ChannelId> {
        self.inner.read()
            .subscriptions.get(agent_id)
            .map(|s| s.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// 获取 channel
    pub fn get_channel(&self, channel_id: &str) -> Option<Channel> {
        self.inner.read().channels.get(channel_id).cloned()
    }

    /// 获取 agent 的 inbox（只读查询）
    pub fn get_inbox(&self, agent_id: &str) -> Option<ChannelInbox> {
        self.inner.read().inboxes.get(agent_id).cloned()
    }

    /// 插入 / 更新 channel
    pub fn upsert_channel(&self, channel: Channel) {
        self.inner.write().channels.insert(channel.id.clone(), channel);
    }

    /// 获取 channel 的所有 participant inbox
    pub fn get_participant_inboxes(&self, channel_id: &str) -> Vec<(AgentId, ChannelInbox)> {
        let inner = self.inner.read();
        match inner.channels.get(channel_id) {
            Some(ch) => ch.participants.iter()
                .filter_map(|p| inner.inboxes.get(&p.id).map(|ibx| (p.id.clone(), ibx.clone())))
                .collect(),
            None => vec![],
        }
    }

    /// 标记 participant 为已派生
    pub fn mark_spawned(&self, channel_id: &str, agent_id: &str) {
        if let Some(ch) = self.inner.write().channels.get_mut(channel_id) {
            if let Some(p) = ch.participants.iter_mut().find(|p| p.id == agent_id) {
                p.spawned = true;
            }
        }
    }

    /// 标记 channel 关闭
    pub fn mark_closed(&self, channel_id: &str) {
        if let Some(ch) = self.inner.write().channels.get_mut(channel_id) {
            ch.closed = true;
        }
    }
}
```

### 3.4 `manager.rs` — Channel CRUD + 消息存储

```rust
//! ChannelManager: channel 的创建、查询、消息发送
//! 持有 AgentFactory，创建 channel 时自动派生 participant agent

use std::sync::Arc;
use parking_lot::RwLock;
use crate::registry::ChannelRegistry;
use crate::dispatcher::WakeupDispatcher;
use crate::types::*;
use anureo_llm::support::uuid6::uuid6;
use async_trait::async_trait;

/// Agent 工厂接口（由 ReactRunner 或 app 层实现）
/// ChannelManager 创建 channel 时调用，自动派生 participant agent
#[async_trait]
pub trait AgentFactory: Send + Sync {
    /// 派生一个 agent，注入共享的 ChannelManager
    /// agent 启动后自动进入 ReAct loop，park 等待 channel 消息
    async fn spawn(
        &self,
        agent_id: AgentId,
        profile: &str,
        channel_manager: ChannelManager,
        initial_prompt: Option<String>,
    );
}

/// Channel 管理器
#[derive(Clone)]
pub struct ChannelManager {
    registry: ChannelRegistry,
    dispatcher: WakeupDispatcher,
    /// Agent 工厂（Option：ChannelManager 可在没有 factory 的情况下工作，
    /// 此时创建 channel 不会自动派生 agent，适用于测试）
    agent_factory: Option<Arc<dyn AgentFactory>>,
}

/// 任务状态查询接口（由上层实现）
pub trait TaskStatusLookup: Send + Sync {
    fn is_completed(&self, task_id: &str) -> bool;
}

impl ChannelManager {
    pub fn new() -> Self {
        let registry = ChannelRegistry::new();
        let dispatcher = WakeupDispatcher::new(registry.clone());
        Self { registry, dispatcher, agent_factory: None }
    }

    /// 注入 AgentFactory
    pub fn with_factory(mut self, factory: Arc<dyn AgentFactory>) -> Self {
        self.agent_factory = Some(factory);
        self
    }

    pub fn registry(&self) -> &ChannelRegistry {
        &self.registry
    }

    /// 创建 channel
    ///
    /// 创建 channel 后，自动为每个 participant 调用 AgentFactory::spawn()。
    /// creator 自身不自动派生——它已经是运行中的 agent。
    pub async fn create_channel(
        &self,
        creator: AgentId,
        topic: String,
        mode: ChannelMode,
        moderator: Option<AgentId>,
        participants: Vec<ChannelParticipant>,
        lifetime: ChannelLifetime,
    ) -> Channel {
        let channel_id = format!("ch_{}", uuid6());

        // 确保 creator 也在 participants 中（但不自动派生）
        let mut all_participants = participants.clone();
        let creator_exists = all_participants.iter().any(|p| p.id == creator);
        if !creator_exists {
            all_participants.push(ChannelParticipant {
                id: creator.clone(),
                profile: String::new(),  // creator 不需要 profile
                spawned: true,           // creator 已在运行
            });
        }

        let channel = Channel {
            id: channel_id.clone(),
            creator: creator.clone(),
            topic,
            mode,
            moderator,
            participants: all_participants.clone(),
            messages: Vec::new(),
            lifetime,
            created_at: now_iso(),
            last_active: now_iso(),
            closed: false,
        };

        // 为每个 participant 注册 inbox + 订阅
        for p in &all_participants {
            self.registry.get_or_create_inbox(&p.id);
            self.registry.subscribe(&channel_id, &p.id);
        }

        self.registry.upsert_channel(channel.clone());

        // 自动派生未 spawn 的 participant agent
        if let Some(ref factory) = self.agent_factory {
            for p in &all_participants {
                if !p.spawned && p.id != creator {
                    factory.spawn(
                        p.id.clone(),
                        &p.profile,
                        self.clone(),
                        None,  // initial_prompt
                    ).await;
                    // 标记为已派生
                    self.registry.mark_spawned(&channel_id, &p.id);
                }
            }
        }

        channel
    }

    /// 发送消息
    pub fn send_message(
        &self,
        channel_id: &str,
        from: AgentId,
        content: String,
        mentions: Vec<AgentId>,
        metadata: Option<ChannelMessageMetadata>,
    ) -> Result<ChannelMessage, String> {
        let channel = self.registry.get_channel(channel_id)
            .ok_or("channel not found")?;
        if channel.closed {
            return Err("channel is closed".into());
        }
        if !channel.participants.iter().any(|p| p.id == from) {
            return Err("sender is not a participant".into());
        }

        let msg = ChannelMessage {
            id: format!("msg_{}", uuid6()),
            channel_id: channel_id.to_string(),
            from: from.clone(),
            content: content.clone(),
            mentions: mentions.clone(),
            timestamp: now_iso(),
            metadata,
        };

        // 消息存入 channel（运行时为 RwLock<Vec<Message>>）
        self.registry.upsert_channel(Channel {
            messages: { /* append message to channel */ vec![] },
            last_active: now_iso(),
            ..channel
        });

        // 唤醒分发
        self.dispatcher.dispatch_message(&channel_id, &from, &mentions);

        Ok(msg)
    }

    /// 关闭 channel
    pub fn close_channel(&self, channel_id: &str, by: &str) -> Result<(), String> {
        let channel = self.registry.get_channel(channel_id)
            .ok_or("channel not found")?;

        let is_creator = channel.creator == by;
        let is_moderator = channel.moderator.as_deref() == Some(by);
        if !is_creator && !is_moderator {
            return Err("only creator or moderator can close".into());
        }

        self.registry.mark_closed(channel_id);
        self.dispatcher.dispatch_close(channel_id);
        Ok(())
    }

    /// Agent 加入 channel
    pub fn join(&self, channel_id: &str, agent_id: AgentId) -> Result<(), String> {
        let channel = self.registry.get_channel(channel_id)
            .ok_or("channel not found")?;
        if channel.closed {
            return Err("channel is closed".into());
        }
        self.registry.get_or_create_inbox(&agent_id);
        self.registry.subscribe(channel_id, &agent_id);
        Ok(())
    }

    /// Agent 退出 channel
    pub fn leave(&self, channel_id: &str, agent_id: &str) -> Result<(), String> {
        self.registry.unsubscribe(channel_id, agent_id);
        Ok(())
    }

    /// 检查 channel 是否应该关闭（TTL / Task）
    pub fn check_lifetime(&self, _task_lookup: &dyn TaskStatusLookup) {
        // 遍历所有 channel，检查 lifetime 条件
        // 如果需要关闭，调用 close_channel
    }
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}
```

### 3.5 `dispatcher.rs` — 唤醒分发

```rust
//! WakeupDispatcher: 根据 channel 模式分发唤醒事件

use crate::registry::ChannelRegistry;
use crate::inbox::ChannelInbox;
use crate::types::*;

#[derive(Clone)]
pub struct WakeupDispatcher {
    registry: ChannelRegistry,
}

impl WakeupDispatcher {
    pub fn new(registry: ChannelRegistry) -> Self {
        Self { registry }
    }

    /// 分发一条新消息
    pub fn dispatch_message(
        &self,
        channel_id: &str,
        from: &str,
        mentions: &[AgentId],
    ) {
        let channel = match self.registry.get_channel(channel_id) {
            Some(ch) => ch,
            None => return,
        };

        // 获取消息（最新一条）
        let latest_msg = match channel.messages.last() {
            Some(m) => m.clone(),
            None => return,
        };

        match channel.mode {
            ChannelMode::Chat => {
                self.dispatch_chat(channel_id, from, mentions, latest_msg, &channel);
            }
            ChannelMode::Meeting => {
                self.dispatch_meeting(channel_id, from, mentions, latest_msg, &channel);
            }
        }
    }

    fn dispatch_chat(
        &self,
        channel_id: &str,
        from: &str,
        mentions: &[AgentId],
        msg: ChannelMessage,
        channel: &Channel,
    ) {
        let inboxes = self.registry.get_participant_inboxes(channel_id);
        for (agent_id, inbox) in inboxes {
            if agent_id == from {
                continue;  // 不唤醒发送者
            }
            if mentions.contains(&agent_id) {
                inbox.deliver(WakeReason::Mentioned {
                    channel_id: channel_id.to_string(),
                    messages: vec![msg.clone()],
                });
            } else {
                inbox.deliver(WakeReason::NewChannelMessage {
                    channel_id: channel_id.to_string(),
                    messages: vec![msg.clone()],
                });
            }
        }
    }

    fn dispatch_meeting(
        &self,
        channel_id: &str,
        from: &str,
        mentions: &[AgentId],
        msg: ChannelMessage,
        channel: &Channel,
    ) {
        // Case 1: moderator 发消息 → 直接触发
        if let Some(ref moderator) = channel.moderator {
            if from == moderator {
                // moderator 的消息直接投递
                let inboxes = self.registry.get_participant_inboxes(channel_id);
                for (agent_id, inbox) in inboxes {
                    if agent_id == *from {
                        continue;
                    }
                    if mentions.contains(&agent_id) {
                        inbox.deliver(WakeReason::GrantedFloor {
                            channel_id: channel_id.to_string(),
                            message: msg.clone(),
                        });
                    } else {
                        inbox.deliver(WakeReason::NewChannelMessage {
                            channel_id: channel_id.to_string(),
                            messages: vec![msg.clone()],
                        });
                    }
                }
                return;
            }
        }

        // Case 2: 普通参与者发消息 → 只唤醒 moderator
        if let Some(ref moderator) = channel.moderator {
            if let Some(inbox) = self.registry.get_inbox(moderator) {
                if mentions.contains(moderator) {
                    inbox.deliver(WakeReason::Mentioned {
                        channel_id: channel_id.to_string(),
                        messages: vec![msg.clone()],
                    });
                } else {
                    inbox.deliver(WakeReason::NewChannelMessage {
                        channel_id: channel_id.to_string(),
                        messages: vec![msg.clone()],
                    });
                }
            }
        }
    }

    /// 分发 channel 关闭通知
    pub fn dispatch_close(&self, channel_id: &str) {
        let inboxes = self.registry.get_participant_inboxes(channel_id);
        for (_, inbox) in inboxes {
            inbox.deliver(WakeReason::ChannelClosed {
                channel_id: channel_id.to_string(),
            });
        }
    }
}
```

---

## 4. ReAct Loop 集成

### 4.1 `ReActState` 扩展

在 `agent/agent-core/src/state.rs` 中增加两个字段：

```rust
// Channel inbox（不可序列化，含 Notify）
#[serde(default, skip)]
pub channel_inbox: Option<anureo_channel::ChannelInbox>,

// 一次性的 channel prompt（下次 think 循环消费，用完即清）
// ObserveNode 写入 → ThinkNode 读取并注入 LLM → 清空
#[serde(default, skip)]
pub channel_prompt: Option<String>,
```

> `skip` 因为 inbox（含 Notify）和 channel_prompt（运行时瞬态）都不需要持久化到 checkpoint。

### 4.2 Channel 消息注入：Prompt 模式

**核心思路**：channel 消息不进入 `state.messages` 对话历史，而是作为**一次性 prompt**，在下次 ThinkNode 调用 LLM 时注入，用完即清。

为什么不用 `Message::User`：
- 避免污染对话历史——channel 消息是上下文通知，不是用户指令
- 避免 token 膨胀——历史消息会随轮次累积，prompt 只出现一次
- Agent 可以自然地在 think 中决定是否通过 `channel_send` 回复，而不需要在对话中产生额外的消息

**ObserveNode**（`observe_node.rs`）—— drain inbox，写入 `channel_prompt`：

```rust
// 在现有 observe 逻辑之后追加：

// 1. 检查 channel inbox
if let Some(ref inbox) = state.channel_inbox {
    let reasons = inbox.drain();
    if !reasons.is_empty() {
        // 写入 channel_prompt，ThinkNode 会在下次循环消费
        state.channel_prompt = anureo_channel::ChannelInbox::format_for_context(&reasons);
    }
}

// 2. 如果没有工具调用、没有工具结果、没有 channel 消息 → park
if state.tool_calls.is_empty()
    && state.tool_results.is_empty()
    && state.channel_prompt.is_none()
    && state.channel_inbox.as_ref().map_or(true, |ibx| !ibx.has_pending())
{
    // park 直到 channel 有新消息
    if let Some(ref inbox) = state.channel_inbox {
        inbox.park_until_wakeup().await;
    }
}
```

**ThinkNode**（`think_node.rs`）—— 读取 `channel_prompt`，注入到 LLM 调用，然后清空：

```rust
// 在调用 LLM 之前：

// 读取并清空 channel_prompt
let channel_prompt = state.channel_prompt.take();

// 构建发送给 LLM 的 messages
let mut llm_messages = state.messages.clone();

if let Some(ref prompt) = channel_prompt {
    // 作为 system 消息追加（不修改 state.messages）
    // 放在消息列表末尾，LLM 最后看到，优先级最高
    llm_messages.push(Message::System(format!(
        "[Channel Update]\nYou have received new messages in channels you participate in.\n\
         Review them and decide if you need to respond using the channel_send tool.\n\n{}",
        prompt
    )));
}

// 用 llm_messages 调用 LLM（而非 state.messages）
let response = client.chat(&llm_messages, ...).await?;

// channel_prompt 已被 take() 清空，不会在下一轮重复
```

**数据流**：

```
ObserveNode                ThinkNode                 LLM
    │                          │                       │
    │ drain inbox              │                       │
    │ → channel_prompt = Some  │                       │
    │                          │                       │
    ├──────────────────────────►│                       │
    │                          │ take channel_prompt   │
    │                          │ = Some                │
    │                          │ 构建 llm_messages     │
    │                          │ + System(channel)     │
    │                          │                       │
    │                          │ ──────────────────────►│
    │                          │                       │ think
    │                          │ ◄──────────────────────│
    │                          │ response              │
    │                          │                       │
    │                          │ channel_prompt = None │
    │                          │ (已 take，自动清空)    │
```

### 4.3 `ReactRunner` 构建

在 `ReactRunner::new` 中，接收 `ChannelManager` 并传递到 state：

```rust
pub fn new(
    // ... 现有参数 ...
    channel_manager: Option<anureo_channel::ChannelManager>,  // 新增
) -> Result<Self, CompilationError> {
    // ... 现有逻辑 ...

    // 初始化 state 时注入 channel inbox
    // 在 build_react_initial_state 中：
    //   if let Some(ref cm) = channel_manager {
    //       state.channel_inbox = Some(cm.registry().get_or_create_inbox(&thread_id));
    //   }
}
```

### 4.4 `ToolCallContext` 扩展

在 `agent/tool/tool-core/src/context.rs` 中增加：

```rust
pub struct ToolCallContext {
    // ... 现有字段 ...

    /// Channel 管理器句柄（Channel tools 使用）
    pub channel_manager: Option<anureo_channel::ChannelManager>,
}
```

---

## 5. `agent/tool/tool-channel/` — Tool 实现

### 5.1 Tool 注册

每个 tool 实现 `Tool` trait，以 `channel_create` 为例：

```rust
//! channel_create tool

use async_trait::async_trait;
use serde_json::Value;
use tool_core::{Tool, ToolCallContent, ToolCallContext, ToolSourceError, ToolSpec};

pub struct ChannelCreateTool;

#[async_trait]
impl Tool for ChannelCreateTool {
    fn name(&self) -> &str { "channel_create" }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "channel_create".into(),
            description: "Create a new communication channel for multi-agent collaboration. \
                         Returns the channel_id.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "description": "Topic/purpose of the channel"
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["chat", "meeting"],
                        "default": "chat"
                    },
                    "moderator": {
                        "type": "string",
                        "description": "Required for meeting mode"
                    },
                    "participants": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "profile": { "type": "string", "description": "Agent profile name (e.g. \"coder\", \"system-architect\")" }
                            },
                            "required": ["id", "profile"]
                        }
                    },
                    "lifetime": {
                        "type": "string",
                        "default": "manual"
                    }
                },
                "required": ["topic"]
            }),
            // .. 其他 ToolSpec 字段
        }
    }

    async fn call(
        &self,
        args: Value,
        ctx: Option<&ToolCallContext>,
    ) -> Result<ToolCallContent, ToolSourceError> {
        let ctx = ctx.ok_or(ToolSourceError::ContextRequired)?;
        let cm = ctx.channel_manager.as_ref()
            .ok_or(ToolSourceError::Internal("channel manager not available".into()))?;

        let topic = args["topic"].as_str().unwrap_or("");
        let mode = match args["mode"].as_str().unwrap_or("chat") {
            "meeting" => anureo_channel::ChannelMode::Meeting,
            _ => anureo_channel::ChannelMode::Chat,
        };
        let moderator = args.get("moderator").and_then(|v| v.as_str()).map(String::from);
        let participants: Vec<anureo_channel::ChannelParticipant> = args["participants"]
            .as_array()
            .map(|arr| arr.iter().filter_map(|v| {
                let id = v.get("id")?.as_str()?.to_string();
                let profile = v.get("profile")?.as_str()?.to_string();
                Some(anureo_channel::ChannelParticipant { id, profile, spawned: false })
            }).collect())
            .unwrap_or_default();
        let lifetime = parse_lifetime(args.get("lifetime"));

        let creator = ctx.thread_id.clone().unwrap_or_else(|| "root".into());
        let channel = cm.create_channel(
            creator, topic.into(), mode, moderator,
            participants, lifetime,
        ).await;

        Ok(ToolCallContent::text(format!(
            "Channel created: id={}, topic='{}'",
            channel.id, channel.topic
        )))
    }
}
```

其余 5 个 tools（`send`, `join`, `leave`, `close`, `list`）结构类似，实现 `Tool` trait，调用 `ChannelManager` 对应方法。

### 5.2 在 app 层注册

在 `apps/cli` / `apps/acp` 的 tool 注册处，创建 ChannelManager 并注入 AgentFactory：

```rust
// 实现 AgentFactory
struct AppAgentFactory {
    // 持有 ReactRunner 构建所需的依赖（LLM client、tool registry 等）
}

#[async_trait]
impl AgentFactory for AppAgentFactory {
    async fn spawn(
        &self,
        agent_id: AgentId,
        profile: &str,
        channel_manager: ChannelManager,
        initial_prompt: Option<String>,
    ) {
        // 1. 加载 profile（如 .anureo/agents/{profile}/）
        // 2. 构建 ReactRunner，注入共享的 ChannelManager
        // 3. tokio::spawn agent 的 ReAct loop
        tokio::spawn(async move {
            let runner = ReactRunner::builder()
                .thread_id(agent_id)
                .profile(profile)
                .channel_manager(channel_manager.clone())
                .build();
            runner.run().await;
        });
    }
}

// 注册 channel tools
let channel_manager = anureo_channel::ChannelManager::new()
    .with_factory(Arc::new(AppAgentFactory { /* ... */ }));

let extra_tools: Vec<Arc<dyn Tool>> = vec![
    Arc::new(tool_channel::ChannelCreateTool),
    Arc::new(tool_channel::ChannelSendTool),
    Arc::new(tool_channel::ChannelJoinTool),
    Arc::new(tool_channel::ChannelLeaveTool),
    Arc::new(tool_channel::ChannelCloseTool),
    Arc::new(tool_channel::ChannelListTool),
];
```

---

## 6. 依赖关系

```toml
# agent/channel/Cargo.toml
[dependencies]
tokio = { workspace = true }
parking-lot = "0.12"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = "0.4"
anureo-llm = { path = "../../foundation/llm" }  # uuid6 复用

# agent/tool/tool-channel/Cargo.toml
[dependencies]
async-trait = { workspace = true }
serde_json = { workspace = true }
tool-core = { path = "../tool-core" }
anureo-channel = { path = "../../channel" }

# agent/patterns/Cargo.toml
[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
anureo-channel = { path = "../channel" }
anureo-llm = { path = "../../foundation/llm" }
```

---

## 7. 关键流程时序

### 7.1 消息发送与唤醒

```
Agent A 调用 channel_send tool
  │
  ├── Tool::call → ChannelManager::send_message
  │     ├── 消息存入 channel.messages
  │     ├── 更新 channel.last_active
  │     └── WakeupDispatcher::dispatch_message
  │           ├── Chat 模式: 遍历所有 participant inbox → deliver
  │           └── Meeting 模式:
  │               ├── 如果 from == moderator → mentions 的人 GrantedFloor
  │               └── 否则 → 只通知 moderator
  │
  ├── Agent A 的 ObserveNode 返回结果（tool result）
  │
  ├── Agent A 的 ThinkNode 发现无 tool_calls + inbox 为空
  │   → ObserveNode 中 park_until_wakeup()
  │
  └── Agent B 的 ObserveNode 下一次迭代
        ├── inbox.drain() → 取出 WakeReason::Mentioned
        ├── format_for_context → 注入为 Message::User
        └── 继续到 ThinkNode → LLM 处理消息
```

### 7.2 Agent Park → Wakeup 循环

```
ObserveNode::run()
  │
  ├── 1. 合并 tool_results 到 messages（现有逻辑）
  │
  ├── 2. drain channel inbox
  │     ├── 有消息 → format_for_context → 写入 state.channel_prompt
  │     └── 无消息 → 跳过
  │
  ├── 3. 判断是否需要 park
  │     ├── tool_calls 非空 → 不 park（继续到 act）
  │     ├── tool_results 非空 → 不 park（继续到 think）
  │     ├── channel_prompt 非空 → 不 park（有 channel 消息要处理）
  │     └── 全部为空 → park_until_wakeup().await
  │           ├── Notify permit 已累积 → 立即返回
  │           └── 无 permit → 挂起，等待下次 deliver
  │
  └── 4. 返回 (state, Next::goto("think"))

ThinkNode::run()
  │
  ├── 1. state.channel_prompt.take() → 读取并清空
  │
  ├── 2. 构建 llm_messages = state.messages.clone()
  │     如果 channel_prompt 非空 → 追加 Message::System(channel_prompt)
  │
  ├── 3. 调用 LLM（用 llm_messages，不修改 state.messages）
  │
  └── 4. 写入 assistant response → state.messages
         channel_prompt 已清空，不会在下一轮重复
```

---

## 8. Workspace 修改

### `Cargo.toml`

```toml
# 在 members 中添加：
"agent/channel",
"agent/patterns",
"agent/tool/tool-channel",
```

---

## 9. 实施顺序

按设计文档的 Phase 1-4，映射到代码任务：

| 顺序 | 任务 | 涉及文件 | 验收 |
|------|------|---------|------|
| 1 | crate 骨架 + types.rs | `agent/channel/` | `cargo check` 通过 |
| 2 | ChannelInbox | `inbox.rs` | 单元测试：deliver → drain 正确 |
| 3 | ChannelRegistry | `registry.rs` | 单元测试：subscribe/unsubscribe 正确 |
| 4 | ChannelManager + Dispatcher | `manager.rs`, `dispatcher.rs` | 单元测试：create→send→deliver 闭环 |
| 5 | ReActState 扩展 | `state.rs` | 编译通过，serde skip 正确 |
| 6 | ObserveNode 集成 | `observe_node.rs` | 集成测试：消息注入到 messages |
| 7 | ReactRunner 构建 | `runner.rs` | 编译通过 |
| 8 | ToolCallContext 扩展 | `context.rs` | 编译通过 |
| 9 | tool-channel 6 个 tools | `tool-channel/` | 每个工具调用测试 |
| 10 | 注册到 app 层 | `apps/cli/` | 端到端：CLI 中 channel_create → send |
| 11 | Meeting 模式 + ForwardingRule | `dispatcher.rs` | 集成测试：moderator 转发 |
| 12 | Lifecycle + 持久化 | `manager.rs` | TTL/Task 关闭测试 |
| 13 | Design Review Pattern | `patterns/` | 端到端：architect + challenger 场景 |

---

## 10. 待解决的技术问题

| # | 问题 | 影响 | 倾向 |
|---|------|------|------|
| 1 | channel.messages 的并发写入 | dispatcher 读 channel 时可能拿到旧快照 | Channel 运行时用 `RwLock<Vec<ChannelMessage>>`，registry 持有 Arc |
| 2 | Agent park 时 ObserveNode 阻塞 | graph-core 的 Node::run 是 async，park 会阻塞该节点的 task | 可接受——每个 agent 是独立的 tokio task |
| 3 | channel manager 如何传递给 agent tool 派生的子 agent | 子 agent 需要共享同一个 ChannelManager | ChannelManager clone 成本低（内部全是 Arc），通过 ReactBuildConfig 传递 |
| 4 | ReActState 的 serde skip 对 checkpoint 的影响 | park 中的 agent 重启后丢失 inbox | Phase 3 通过持久化 channel 消息 + 重启时重建 inbox 解决 |
| 5 | Agent 的 thread_id 与 ChannelAgentId 的映射 | channel 用 AgentId 标识参与者，需要与 thread_id 对齐 | 直接用 thread_id 作为 AgentId |

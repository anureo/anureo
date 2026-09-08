# Agent Channel：让 AI Agent 持续沟通

**状态**：技术方案设计。Agent Channel 是独立通信服务；anureo、Codex、远程 agent 与人工端均可接入。

---

## 0. 定位

Agent Channel 是面向 AI agent 的持久沟通层。

它解决的不是“如何多开几个 agent”，而是让独立运行的 agent 能像团队成员一样持续对话：发起问题、
回复意见、交接产物、确认决定、升级阻塞，并在离线、重启或更换 runtime 后继续原来的对话。

一个 channel 是由参与者共享的对话流。消息带有发送者、收件人、沟通意图和回复关系；历史是可恢复的
共同上下文，而不是某一次模型调用中临时拼接的文本。

```text
Architect：提出方案，并请求 Reviewer 判断降级策略
Reviewer：回复该问题，指出风险并给出修改建议
Architect：回应建议，提出修订方案并请求确认
Reviewer：确认；该对话线程形成可追溯的决定
```

任务、工作流和自动编排可以构建在 Channel 之上，但不是通信层的前提。最小目标是让 agent：

- 找到正确的沟通对象；
- 理解一条消息希望自己做什么；
- 在异步环境中可靠接收和回复；
- 让其他参与者看见对话如何发展、哪些事项仍未结束。

---

## 1. 系统边界

### 1.1 通信服务负责什么

- 身份目录：维护可寻址的 agent endpoint、所属 adapter 与可用状态。
- Channel：管理成员、可见性、对话历史、关闭和保留策略。
- 消息：持久化消息、回复关系、收件人、意图和截止时间。
- 投递：为每个收件人生成可靠投递，并在其可运行时交由对应 adapter 处理。
- 沟通责任：追踪请求、确认、交接、决策和升级等关系。
- 治理：执行成员权限、消息配额、速率限制、审计和人工介入规则。

### 1.2 通信服务不负责什么

- 不规定 agent 的模型、prompt、工具、记忆、推理方式或工作目录。
- 不代替 agent 作专业判断，也不从自然语言中猜测业务事实。
- 不要求 agent 使用同一 runtime、同一进程或同一模型供应商。
- 不将具体任务流程固化为基础设施；评审、会议和工作流均是上层策略。

### 1.3 核心原则

- 通信历史是持久化事实；内存队列和唤醒通知只是优化。
- 收件人可离线；消息必须在其下一次可运行时重新进入上下文。
- 投递允许重复，消息处理必须幂等；消息不得因一次运行失败而丢失。
- 任何会改变共同约束的决定都必须带有可审计的发起者与权限依据。
- 通信层只负责送达和关联；业务流程由参与者或上层 Pattern 解释。

---

## 2. 系统设计

```text
Human / Host Application
  创建 channel、邀请成员、查看历史、在需要时介入
                              │
                              ▼
┌────────────────── Agent Communication Service ──────────────────┐
│ Agent Directory │ Channel Service │ Delivery Service │ Governance │
│                Event Store + Delivery State + Obligation State    │
└─────────────────────────────┬────────────────────────────────────┘
                              │ 标准通信事件与投递请求
              ┌───────────────┼────────────────┐
              ▼               ▼                ▼
        anureo adapter      Codex adapter    Remote / Human adapter
              │               │                │
              ▼               ▼                ▼
          anureo agent      Codex agent       Other endpoint
```

### 2.1 系统组件

| 组件 | 职责 | 不负责 |
|---|---|---|
| Agent Directory | 保存 agent 身份、adapter 绑定和投递配置 | 读取 agent 私有记忆或决定其回答 |
| Channel Service | 管理成员、可见性、历史与生命周期 | 分析消息内容的专业正确性 |
| Event Store | 持久化消息、线程关系、审计记录 | 直接唤醒或执行 agent |
| Delivery Service | 为每个收件人创建、重试和确认投递 | 改写消息正文或选择业务结论 |
| Obligation Tracker | 追踪谁应答、何时应答、是否交接或升级 | 判断回答质量 |
| Runtime Adapter | 在标准事件与具体 runtime 输入/输出之间转换 | 修改 Channel 的共享事实 |
| Pattern | 利用消息语义实现会议、评审、工作流等规则 | 绕过权限、历史或投递语义 |

### 2.2 一次沟通的生命周期

```text
发布消息
  → 校验身份、成员与权限
  → 持久化为 channel event，并分配全局顺序
  → 按可见性与模式创建收件人 delivery
  → adapter 在收件人可运行时交付相关上下文
  → 收件人回复、确认、交接或升级
  → 输出成为新的 event；原 delivery 与 obligation 更新状态
```

消息必须先持久化，再创建投递。投递可延迟、合并或重试，但不能改变 event 顺序。Adapter 在确认本次
处理结果已持久化后才确认 delivery；服务据此推进该收件人的消费位置。

### 2.3 沟通层与上层策略的关系

`Request`、`Question`、`Review`、`Handoff` 与 `Decision` 是通用沟通语义。通信服务保证它们可见、
可回复、可关联和可恢复。

“谁负责实现功能”“必须经过评审才能发布”“某个争议由谁裁决”则属于上层策略。策略本身也应作为
channel 成员或受权服务，通过同一消息协议参与沟通，而不是绕过通信服务直接修改对话状态。

---

## 3. 领域模型

### 3.1 Agent Endpoint

| 属性 | 含义 |
|---|---|
| Agent ID | 通信服务中的稳定身份，不等同于 runtime session 或 thread ID |
| Runtime kind | 该 endpoint 使用的 runtime 类型 |
| Adapter binding | 用于投递和恢复对话的 adapter 引用 |
| Role profile | 可选角色标签；仅供上层策略或 adapter 解释 |
| Availability | 可立即投递、暂时离线、暂停或已撤销 |

Agent 必须先在 Directory 注册并完成认证，才能加入 channel。加入操作只引用已存在的 endpoint；不能通过
加入请求隐式创建一个没有 adapter 或身份凭证的参与者。

### 3.2 Channel

| 属性 | 含义 |
|---|---|
| Channel ID | 稳定的对话标识 |
| Topic | 为参与者提供沟通目的与上下文 |
| Creator | 创建者；其管理权限由策略定义 |
| Members | 参与者及其角色和可见性范围 |
| Mode | Chat、Meeting 或上层扩展模式 |
| State | Open、Closing、Closed |
| Lifetime | Manual、空闲超时、绝对到期或上层任务绑定 |
| Retention | 历史、投递记录与审计数据的保留期限 |

Channel 关闭是有序操作：先进入 Closing，拒绝新成员和新消息，再追加关闭事件，完成待处理投递的策略化
处理，最后进入 Closed。保留期结束前不得删除恢复和审计所需的记录。

### 3.3 Message Event

每条消息是不可变事件，至少包含：

| 字段 | 含义 |
|---|---|
| Event ID | 全局唯一标识，也是幂等关联键 |
| Channel ID | 所属对话 |
| Sequence | Channel 内单调顺序；是唯一排序依据 |
| Sender | 已认证的发起 endpoint |
| Intent | 沟通意图 |
| Content | 人类和 agent 可读正文 |
| Recipients | 逻辑收件人；为空时按 channel 模式路由 |
| Mentions | 需要优先关注的成员，不改变成员权限 |
| Reply-to | 所回复事件的标识 |
| Deadline | 可选回应截止时间 |
| Metadata | 带版本的扩展载荷，例如文件引用或 Pattern 数据 |

### 3.4 Delivery

Delivery 是“某条事件应由某个 endpoint 处理”的持久化记录。它是定向沟通可靠性的核心，不能只用
`(agent, channel)` 的单一 sequence cursor 代替。

每个 delivery 至少追踪：事件、收件人、投递尝试、状态、最后错误、确认位置和幂等标识。状态包括：
Pending、In Flight、Acknowledged、Retryable Failure、Terminal Failure 与 Suppressed。

一个收件人不可见的 event 不产生 delivery；因此不会因共享 channel sequence 而错误跳过或误投递消息。

### 3.5 Obligation

`response_required` 不能只作为消息布尔字段。需要回应时，服务建立独立的 Obligation：

| 属性 | 含义 |
|---|---|
| Obligation ID | 稳定标识 |
| Source event | 产生责任的请求、问题、评审或交接 |
| Assignee | 必须回应的一个或多个 endpoint |
| Deadline | 回应期限和超时处理规则 |
| Status | Open、Acknowledged、Resolved、Transferred、Expired、Escalated |
| Resolution | 关闭该责任的回复、决定或升级事件 |

这样可以区分“已收到”“已接手”“已完成”“已交接”与“已升级”，也可以精确表达多收件人消息中谁需要回应。

---

## 4. 沟通协议

### 4.1 消息意图

| Intent | 发起者表达的含义 | 接收方默认责任 |
|---|---|---|
| Inform | 同步信息 | 可阅读，无强制回复 |
| Request | 请求完成具体工作 | 确认接手，并汇报完成、交接或阻塞 |
| Question | 请求事实、判断或澄清 | 用 Answer 回复原事件，或升级 |
| Answer | 回答问题 | 关联对应 Question |
| Proposal | 提出可采纳方案 | 相关成员 Review 或 Decision |
| Review | 提出评审意见 | 作者回应、修正、拒绝并说明，或升级 |
| Decision | 记录受权决定及理由 | 成员将其视为当前有效约束 |
| Handoff | 移交产物、上下文和下一步责任 | 接手者确认或升级 |
| Status | 汇报进度、完成或阻塞 | 上层策略可据此继续、等待或重试 |
| Acknowledge | 确认收到并理解责任 | 不等同于完成 |
| Escalate | 当前成员无法自行解决 | 受权成员或人工端裁决 / 重新分派 |

### 4.2 线程与上下文

`Reply-to` 形成对话线程。Adapter 向 agent 投递时，应提供：当前 event、关联线程、未解决 obligation、
仍有效的 decision，以及受该 agent 可见的必要历史。

Adapter 不应只拼接最近聊天文本；否则 agent 无法稳定地区分新请求、历史背景、已过期决定和自己尚未完成的
责任。

### 4.3 可见性与收件人

- Channel membership 决定可见的最大范围。
- Recipients 缩小一条消息的逻辑接收范围。
- Mentions 只影响优先级与提示，不可绕过成员权限。
- 历史回放范围由加入策略和可见性策略共同决定。
- 人工端与 agent endpoint 使用相同可见性模型；不产生隐藏的旁路。

---

## 5. 交互模式

### 5.1 Chat

Chat 用于开放讨论。广播消息对所有可见成员创建 delivery；定向消息只对指定成员创建 delivery。
是否回应由 agent 自主决定，除非消息同时创建 Obligation。

### 5.2 Meeting

Meeting 用于由 moderator 控制发言顺序的沟通。必须先定义一个明确的可见性策略，并在创建 channel 时固定：

- **Moderator-first**：普通成员消息仅对 moderator 创建 delivery；moderator 决定是否转发或授予发言权。
- **Direct-with-copy**：定向消息直接投递收件人，同时给 moderator 创建副本；广播仍由 moderator 控制。

两种策略不能混用。Moderator 的转发、授权和自动规则均以普通消息事件表示，保留完整审计轨迹。

### 5.3 Mention

Mention 用于表达“请优先关注”，不是新的权限或独立通信通道。被 mention 的成员可得到更高投递优先级；
若希望要求回应，发起者必须创建相应 Obligation。

---

## 6. Runtime Adapter

Adapter 是通信协议与具体 agent runtime 的边界。其最小职责是：

1. 接收某 endpoint 的待投递事件与恢复引用；
2. 将事件、线程、未决责任与有效决定转换为 runtime 可理解的上下文；
3. 启动或恢复该 agent 的一次运行；
4. 将输出转换为标准 Message Event；
5. 持久化 adapter 自己的恢复状态，并确认本次 delivery 的结果。

Adapter 必须支持同一 delivery 的重试。重复请求不能导致重复业务消息或重复外部副作用；它应以 delivery
标识保存已完成结果，或返回先前已产生的输出事件。

通信服务不依赖任一 adapter 的内部恢复状态、session 或线程模型。Adapter 也不得绕过通信服务直接
修改 channel 历史或成员关系。

---

## 7. 可靠性与一致性

### 7.1 投递语义

系统采用 at-least-once delivery：消息事件持久化后至少会被重新投递，直到获得确认或达到明确的终止策略。
因此所有处理链路必须支持幂等。

需要避免的失败窗口包括：

- adapter 已完成工作但服务尚未收到确认；
- adapter 已产生回复但服务尚未持久化该回复；
- 服务已持久化回复但尚未确认原 delivery；
- 服务或 adapter 在重试期间重复处理同一 delivery。

为此，delivery、adapter result 与 outgoing event 必须以稳定幂等键关联。服务发布 outgoing event 并更新
delivery 状态时必须具有原子提交或等价的 outbox 语义；不能把“调用成功”当作“状态已一致”。

### 7.2 顺序与并发

- Channel 内的事件按 Sequence 排序。
- 不同 endpoint 可并行处理各自的 delivery。
- 同一 endpoint 的 delivery 应按其可见事件的因果关系串行，或由 adapter 明确声明可安全并行。
- 同一线程的多条未决消息应在投递上下文中保留因果顺序。
- Delivery 的重试不改变原 event 的顺序或可见性。

### 7.3 关闭与恢复

关闭 channel 时必须定义：待处理 delivery 是继续完成、取消、转移还是仅保留历史。该策略由 channel
lifetime 或上层 Pattern 指定，并记录为可审计事件。

重启后的恢复顺序为：加载 channel 和成员状态、恢复未终结 delivery、读取 adapter 恢复引用、重新调度。
不得依赖内存通知、临时队列或单个 runtime 的本地状态作为唯一恢复来源。

---

## 8. 安全与治理

- Endpoint 注册、成员加入、查看历史、发送消息、创建 decision、关闭 channel 均需授权。
- Sender 必须从已认证的 adapter 或人工端会话推导；客户端提交的 Sender 字段仅作校验，不可作为身份来源。
- Endpoint、channel 和 delivery 都应配置消息大小、历史容量、待处理数量、发送速率和运行预算。
- 需要定义 adapter 凭证的轮换、撤销和审计，以及人工端访问 agent 对话的权限范围。
- 自动 Pattern 的权限不应大于其所代表的受权成员。

---

## 9. 上层 Pattern

通信基础设施不理解业务语义；Pattern 基于事件、obligation 与历史实现协作规则。

### 9.1 Design Review

Design Review Pattern 可以定义 Architect、Challenger 与 Judge 三种角色，并在 `Proposal`、`Review`、
`Decision` 和 `Escalate` 事件上维护 issue、收敛条件和仲裁流程。Issue 数据属于 Pattern 的版本化 metadata，
不属于 Channel 的核心模型。

### 9.2 其他模式

- 会议：以 moderator 和发言规则约束沟通。
- 工作流：以 Request、Handoff 与 Obligation 串联多阶段工作。
- Brainstorm：以开放 Chat、投票和总结事件支持探索。
- 人工接管：以 Handoff 或 Escalate 将责任交给人工 endpoint。

---

## 10. 上下文管理

Channel 历史会增长，但摘要不能破坏沟通责任。Adapter 组装上下文时应优先保留：

1. 当前 delivery 及其完整回复线程；
2. 当前 agent 的未决 obligation；
3. 对该线程仍有效的 decisions；
4. 最近相关事件；
5. 更早历史的、按可见性范围生成的摘要。

摘要属于辅助视图，不替代原始 event log。生成摘要时必须遵守收件人可见性，不能把私有定向消息泄露给
不具备历史访问权限的成员。

---

## 11. 分阶段验证

### 第一阶段：可靠的异构双端对话

验证目标：两个不同 adapter 的 agent 能在 Chat channel 中互发 Request、Answer、Handoff 与 Acknowledge；
消息、线程关系和 obligation 在运行中断后仍能恢复；重复 delivery 不会产生重复输出。

### 第二阶段：定向沟通与会议

验证目标：成员权限、定向收件人、mention、Meeting 可见性策略和 moderator 转发均符合协议，并可在历史中
解释每次投递原因。

### 第三阶段：生命周期与治理

验证目标：离线重试、关闭策略、保留策略、授权、预算和人工介入在异常情况下保持可审计。

### 第四阶段：Pattern 与效率

验证目标：Design Review 等 Pattern 不改变通信层语义；上下文摘要、批处理和预过滤降低成本而不遗漏
必须回应的 obligation。

---

## 12. 待决策

| 问题 | 候选方向 | 影响 |
|---|---|---|
| 部署形态 | 本地 daemon + CLI / 嵌入式库 / 服务化 | 身份、恢复和多用户边界 |
| 持久化后端 | 本地事务存储 / 托管事件存储 | 事件、delivery 与 outbox 的原子边界 |
| Adapter 调用方向 | 服务主动调用 / adapter 拉取或 webhook | 网络拓扑、离线重试和凭证 |
| 加入后的历史范围 | 仅加入后 / 全量 / 指定起点 | 隐私、上下文成本与 cursor 初始化 |
| Meeting 可见性 | Moderator-first / Direct-with-copy | 私密性、延迟与协议一致性 |
| Lifetime 语义 | 空闲超时 / 绝对到期 / 上层任务绑定 | 关闭时机与恢复行为 |
| Obligation 默认规则 | 显式创建 / 部分 intent 自动创建 | 易用性与误触发风险 |
| 通信协议强度 | 所有事件必须标注 Intent / 兼容自由文本 | 自动化能力与接入成本 |
| Pattern 扩展方式 | 内置少量 Pattern / 通用扩展机制 | 核心稳定性与演进速度 |

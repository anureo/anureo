# Background Review Prompt: anureo vs Hermes 对齐分析

## 涉及文件

| 角色 | Hermes (Python) | anureo (Rust) |
|------|-----------------|-------------|
| Prompt 定义 | `agent/background_review.py:160-360` | `experimental/curator/src/prompts.rs:7-226` |
| Runtime guard | `agent/background_review.py:786-790` | `experimental/curator/src/review.rs:23-29` |
| User message 拼接 | `agent/background_review.py:786-791` | `experimental/curator/src/review.rs:278-289` |

---

## 1. 三段 Review Prompt 对比

### 1.1 MEMORY_REVIEW_PROMPT

**Hermes** (`background_review.py:160-173`):
```python
_MEMORY_REVIEW_PROMPT = (
    "Review the conversation above and consider saving to memory if appropriate.\n\n"
    "Focus on:\n"
    "1. Has the user revealed things about themselves - their persona, desires, "
    "preferences, or personal details worth remembering?\n"
    "2. Has the user expressed expectations about how you should behave, their work "
    "style, or ways they want you to operate?\n\n"
    "If something stands out, save it using the memory tool. "
    "If nothing is worth saving, just say 'Nothing to save.' and stop."
)
```

**anureo** (`prompts.rs:7-17`):
```rust
pub const MEMORY_REVIEW_PROMPT: &str = "\
Review the conversation above and consider saving to memory if appropriate.\n\
\n\
Focus on:\n\
1. Has the user revealed things about themselves — their persona, desires, \
preferences, or personal details worth remembering?\n\
2. Has the user expressed expectations about how you should behave, their work \
style, or ways they want you to operate?\n\
\n\
If something stands out, save it using the memory tool.\n\
If nothing is worth saving, just say 'Nothing to save.' and stop.";
```

**差异**:
- `-` → `—` (em dash)：Hermes 用 ASCII 连字符 `-`，anureo 用 Unicode em dash `—`。全文统一。
- 句末 `.` vs `.`：语义相同，句号位置一致。
- **语义：完全对齐。**

---

### 1.2 SKILL_REVIEW_PROMPT

**差异逐项**:

| # | 位置 | Hermes | anureo | 类型 |
|---|------|--------|------|------|
| 1 | 分隔符 | `-` (ASCII) | `—` (em dash) | 标点 |
| 2 | `skill_list` vs `skills_list` | `UPDATE AN EXISTING UMBRELLA (via skills_list + skill_view)` | `UPDATE AN EXISTING UMBRELLA (via skill_list + skill_view)` | 工具名 |
| 3 | Protected skills 示例 | `e.g. 'hermes-agent'` | `e.g. 'hermes-agent'` | 应改为 anureo 等价物 |
| 4 | Hub-installed | `'hermes skills install'` | `'hermes skills install'` | 应改为 anureo 等价物 |
| 5 | Pin 命令 | `'hermes curator pin'` | `'hermes curator pin'` | 应改为 anureo 等价物 |

- 差异 1：全文 `—` vs `-`，与 MEMORY_REVIEW_PROMPT 相同的模式。
- 差异 2：Hermes 用 `skills_list`（复数），anureo 用 `skill_list`（单数）。**anureo 是正确的**——anureo 注册的工具名就叫 `skill_list`（无 s）。Hermes 这里可能是笔误。
- 差异 3-5：Hermes 专有示例（`'hermes-agent'` skill, `'hermes skills install'`, `'hermes curator pin'`）原样保留在 anureo 中，应改为 anureo 对应物。
- **语义：对齐，仅有标点和项目名差异。**

---

### 1.3 COMBINED_REVIEW_PROMPT

与 SKILL_REVIEW_PROMPT 相同的差异模式（`-` vs `—`、`skills_list` vs `skill_list`、Hermes 项目名）。不赘述。

---

## 2. Runtime Guard 文本对比

### Hermes (`background_review.py:786-790`)

```python
user_message=(
    prompt
    + "\n\nYou can only call memory and skill "
    "management tools. Other tools will be denied "
    "at runtime - do not attempt them."
),
```

特点：
- 直接拼在 prompt 后面（同一段纯文本，无标签）
- 用 `-` (ASCII)
- 没有包装在 `<background_review>` 标签中

### anureo (`review.rs:23-29` + `review.rs:287-289`)

```rust
pub const REVIEW_INSTRUCTION: &str = "<background_review>
Review the conversation above and extract durable knowledge.
- Use memory tools to save user preferences and project facts.
- Use skill tools to save reusable task patterns.
- Only use memory and skill tools. Other tools will be denied at runtime.
- If nothing is worth saving, respond with \"Nothing to save.\"
</background_review>";
```

拼接方式：
```rust
format!(
    "Here is the conversation to review:\n\n---\n{}\n---\n\n{}\n\n{}",
    truncated, prompt, REVIEW_INSTRUCTION
)
```

特点：
- 用 `<background_review>` XML 标签包装
- 额外包含了 "extract durable knowledge"、memory/skill 用法提示等 Hermes 没有的内容
- 拼在 prompt 后面，但中间多一层 `\n\n`

### 差异汇总

| 维度 | Hermes | anureo | 影响 |
|------|--------|------|------|
| 包装方式 | 无标签，纯文本 | `<background_review>` XML 标签 | 结构差异 |
| 额外内容 | 无 | "extract durable knowledge"、memory/skill 用法提示 | 语义差异 |
| Guard 措辞 | "You can only call memory and skill management tools" | "Only use memory and skill tools" | 措辞差异 |
| Deny 措辞 | "will be denied at runtime - do not attempt them" | "will be denied at runtime" | 缺少 "do not attempt" |
| "Nothing to save" | 在 prompt 里已有 | 在 REVIEW_INSTRUCTION 里重复出现 | 冗余 |

---

## 3. User Message 拼接方式对比

### Hermes

```python
# background_review.py:786-791
review_agent.run_conversation(
    user_message=(
        prompt                              # MEMORY/SKILL/COMBINED
        + "\n\nYou can only call memory and skill "
        "management tools. Other tools will be denied "
        "at runtime - do not attempt them."
    ),
    conversation_history=_review_history,   # 完整对话快照
)
```

结构：`{prompt}\n\n{guard_text}`

### anureo

```rust
// review.rs:284-289
let prompt = select_review_prompt(review_memory, review_skills)?;
let truncated = truncate_unicode(session_content, max_chars);
Some(format!(
    "Here is the conversation to review:\n\n---\n{}\n---\n\n{}\n\n{}",
    truncated, prompt, REVIEW_INSTRUCTION
))
```

结构：`Here is the conversation to review:\n\n---\n{session}\n---\n\n{prompt}\n\n{REVIEW_INSTRUCTION}`

### 差异

| 维度 | Hermes | anureo |
|------|--------|------|
| 对话内容传递 | `conversation_history` 参数（API 级 history） | 拼在 user message 里（文本级 inline） |
| Guard 位置 | prompt 之后 `\n\n` 直接拼接 | prompt 之后 `\n\n` + `REVIEW_INSTRUCTION`（包装在 XML 标签中） |
| 对话分隔 | 无（history 是 API 参数） | `---` 分隔符 |

---

## 4. 需要对齐的改动清单

### 4.1 REVIEW_INSTRUCTION（`review.rs:23-29`）

**现状**：6 行，包含 XML 标签 + 额外指令 + guard。
**Hermes 对齐目标**：仅需一行 guard 文本。

```rust
// 改前
pub const REVIEW_INSTRUCTION: &str = "<background_review>
Review the conversation above and extract durable knowledge.
- Use memory tools to save user preferences and project facts.
- Use skill tools to save reusable task patterns.
- Only use memory and skill tools. Other tools will be denied at runtime.
- If nothing is worth saving, respond with \"Nothing to save.\"
</background_review>";

// 改后（对齐 Hermes）
pub const REVIEW_INSTRUCTION: &str = "\
You can only call memory and skill management tools. Other tools will be denied at runtime — do not attempt them.";
```

### 4.2 Em dash → ASCII dash（`prompts.rs` 全文）

所有 `—` (U+2014) → `-` (U+002D)。影响三个常量：`MEMORY_REVIEW_PROMPT`、`SKILL_REVIEW_PROMPT`、`COMBINED_REVIEW_PROMPT`。

### 4.3 项目名替换（`prompts.rs`）

| Hermes 原文 | anureo 替换 |
|-------------|-----------|
| `e.g. 'hermes-agent'` | `e.g. 'anureo-development'` 或删除示例 |
| `'hermes skills install'` | `'anureo skills install'` 或删除 |
| `'hermes curator pin'` | `'anureo curator pin'` |

### 4.4 测试更新

`review.rs` 中的测试（`review.rs:869-939`）引用了 `REVIEW_INSTRUCTION` 的旧内容，需要同步更新断言。

---

## 5. 不需要改动的部分

- `select_review_prompt()` 逻辑 — 已对齐
- `build_review_user_message()` 结构 — anureo 的 `---` 分隔 + inline session 方式是 Rust 侧架构决定（没有 API-level conversation_history），与 prompt 内容无关
- `ReviewToolGate` — runtime 级工具白名单，与 prompt 无关
- `CURATOR_REVIEW_PROMPT` — curator LLM pass 用，不是 background review

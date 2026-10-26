//! turn 边界排空队列（设计文档 §4.3「异步 + drain」）。
//!
//! 异步 hook 的产出（additional_context / user_message / 失败摘要）先入
//! per-thread 队列，在下一个 `turn/start` 排空渲染为注入块。上限：
//! 单条 context 4KB、队列 32 条、渲染总长 16KB（超限丢最旧）。

use crate::outcome::HookOutcome;

/// 单条 context 截断上限。
pub const CONTEXT_TRUNC_BYTES: usize = 4 * 1024;
/// 队列容量（超出丢最旧）。
pub const QUEUE_CAP: usize = 32;
/// 渲染块总长上限。
pub const RENDER_CAP_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainKind {
    /// `additional_context`（截断至 4KB）。
    Context(String),
    /// `user_message`（截断至 4KB）。
    UserMessage(String),
    /// 执行失败摘要（仅 Warn 级以上记录）。
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrainItem {
    pub hook: String,
    pub kind: DrainKind,
}

impl DrainItem {
    fn text(&self) -> &str {
        match &self.kind {
            DrainKind::Context(s) | DrainKind::UserMessage(s) | DrainKind::Failed(s) => s,
        }
    }

    fn kind_label(&self) -> &'static str {
        match &self.kind {
            DrainKind::Context(_) => "context",
            DrainKind::UserMessage(_) => "message",
            DrainKind::Failed(_) => "failed",
        }
    }

    fn truncate(s: &str) -> String {
        truncate_bytes(s, CONTEXT_TRUNC_BYTES)
    }

    /// hook 输出 → drain 条目。additional_context 优先于 user_message；
    /// 两者皆无 → `None`（不产生注入）。
    pub fn from_outcome(hook: &str, outcome: &HookOutcome) -> Option<Self> {
        if let Some(ctx) = &outcome.additional_context {
            return Some(Self {
                hook: hook.into(),
                kind: DrainKind::Context(Self::truncate(ctx)),
            });
        }
        if let Some(msg) = &outcome.user_message {
            return Some(Self {
                hook: hook.into(),
                kind: DrainKind::UserMessage(Self::truncate(msg)),
            });
        }
        None
    }

    /// 失败摘要条目。
    pub fn from_failure(hook: &str, description: String) -> Self {
        Self {
            hook: hook.into(),
            kind: DrainKind::Failed(Self::truncate(&description)),
        }
    }
}

/// 按字节截断（不切 UTF-8 边界）。
fn truncate_bytes(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

#[derive(Debug, Default)]
pub struct DrainQueue {
    items: std::collections::VecDeque<DrainItem>,
    cap: usize,
}

impl DrainQueue {
    pub fn new() -> Self {
        Self {
            items: std::collections::VecDeque::new(),
            cap: QUEUE_CAP,
        }
    }

    pub fn push(&mut self, item: DrainItem) {
        if self.items.len() >= self.cap {
            if let Some(evicted) = self.items.pop_front() {
                tracing::warn!(
                    hook = %evicted.hook,
                    "hook drain queue full; evicted oldest item"
                );
            }
        }
        self.items.push_back(item);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn take_all(&mut self) -> Vec<DrainItem> {
        self.items.drain(..).collect()
    }

    /// 渲染为注入块。总长超 16KB 时从头丢弃整条（保留最新）。
    pub fn render(items: Vec<DrainItem>) -> Option<String> {
        if items.is_empty() {
            return None;
        }
        let mut total: usize = 0;
        let mut keep_from = items.len();
        for (i, item) in items.iter().enumerate().rev() {
            total += item.text().len() + item.hook.len() + 32;
            if total > RENDER_CAP_BYTES {
                break;
            }
            keep_from = i;
        }
        let kept = &items[keep_from.min(items.len() - 1)..];
        if kept.is_empty() {
            return None;
        }
        let mut out = String::from("--- hook context (from async hooks) ---\n");
        for item in kept {
            out.push_str(&format!(
                "[hook {} ({})]\n{}\n",
                item.hook,
                item.kind_label(),
                item.text()
            ));
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::Decision;

    fn ctx_item(hook: &str, text: &str) -> DrainItem {
        DrainItem {
            hook: hook.into(),
            kind: DrainKind::Context(text.into()),
        }
    }

    #[test]
    fn fifo_order_preserved() {
        let mut q = DrainQueue::new();
        q.push(ctx_item("a", "1"));
        q.push(ctx_item("b", "2"));
        let items = q.take_all();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].hook, "a");
        assert_eq!(items[1].hook, "b");
        assert!(q.is_empty());
    }

    #[test]
    fn cap_evicts_oldest() {
        let mut q = DrainQueue::new();
        for i in 0..QUEUE_CAP + 5 {
            q.push(ctx_item(&format!("h{i}"), "x"));
        }
        assert_eq!(q.len(), QUEUE_CAP);
        let items = q.take_all();
        assert_eq!(items[0].hook, format!("h{}", 5));
    }

    #[test]
    fn render_truncates_total_keeping_newest() {
        let big = "y".repeat(CONTEXT_TRUNC_BYTES);
        let items = vec![
            ctx_item("old1", &big),
            ctx_item("old2", &big),
            ctx_item("old3", &big),
            ctx_item("old4", &big),
            ctx_item("newest", "keep me"),
        ];
        let rendered = DrainQueue::render(items).expect("non-empty renders");
        assert!(rendered.len() <= RENDER_CAP_BYTES + 512);
        assert!(rendered.contains("newest"));
        assert!(!rendered.contains("old1"));
    }

    #[test]
    fn render_empty_is_none() {
        assert!(DrainQueue::render(Vec::new()).is_none());
    }

    #[test]
    fn from_outcome_prefers_context() {
        let out = HookOutcome {
            decision: Decision::Allow,
            reason: None,
            additional_context: Some("ctx".into()),
            user_message: Some("msg".into()),
        };
        let item = DrainItem::from_outcome("h", &out).expect("produces item");
        assert_eq!(item.kind, DrainKind::Context("ctx".into()));
    }

    #[test]
    fn from_outcome_none_when_no_payload() {
        let out = HookOutcome {
            decision: Decision::Allow,
            reason: Some("r".into()),
            additional_context: None,
            user_message: None,
        };
        assert!(DrainItem::from_outcome("h", &out).is_none());
    }

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "日本語テキスト".repeat(500);
        let t = DrainItem::truncate(&s);
        assert!(t.len() <= CONTEXT_TRUNC_BYTES + 32);
        assert!(t.ends_with("[truncated]"));
    }
}

//! goal 可观测性指标（alignment Phase 7 可选项，补齐弱项 10「events/metrics」）。
//!
//! 两层：
//! - **events**（恒开）：每个生命周期转折发一条 `tracing` 语义事件
//!   （`target = "goal_metrics"`，稳定字段 `event` / `thread_id`），与
//!   Codex `ThreadGoalUpdated` 式事件流对位；
//! - **metrics**（`otel` feature）：进程内原子计数器恒可用
//!   （[`global()`] 快照 / 测试断言）；启用 `otel` feature 后经
//!   `opentelemetry` Observable Counter 暴露——宿主装了全局
//!   MeterProvider 即导出，未装则为 no-op（OTel 标准模式），不强制
//!   workspace 引入 SDK/导出器依赖。
//!
//! 计数为进程生命周期累计值（单调），重启归零——goal 的持久事实在
//! `thread_goals` 表，指标只做观测。

use std::sync::atomic::{AtomicU64, Ordering};

/// 指标计数集（语义命名，与 tracing `event` 字段同名）。
#[derive(Debug, Default)]
pub struct GoalMetrics {
    /// 用户/系统 set（含替换）。
    pub goals_set: AtomicU64,
    /// set 覆盖了既有 goal（快照替换）。
    pub goals_replaced: AtomicU64,
    pub goals_paused: AtomicU64,
    pub goals_resumed: AtomicU64,
    pub goals_cleared: AtomicU64,
    pub goals_edited: AtomicU64,
    pub goals_completed: AtomicU64,
    pub goals_blocked: AtomicU64,
    pub goals_usage_limited: AtomicU64,
    pub goals_budget_limited: AtomicU64,
    /// idle 续跑实际启动（`start_turn_if_idle` 返回 true）。
    pub continuations_started: AtomicU64,
    /// 续跑被 deferral 推迟（§6.6）。
    pub continuations_deferred: AtomicU64,
}

static GLOBAL: std::sync::OnceLock<GoalMetrics> = std::sync::OnceLock::new();

/// 进程级单例。
pub fn global() -> &'static GoalMetrics {
    GLOBAL.get_or_init(GoalMetrics::default)
}

impl GoalMetrics {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    fn emit(event: &str, thread_id: &str) {
        tracing::info!(
            target: "goal_metrics",
            event,
            thread_id = %thread_id,
            "goal lifecycle event"
        );
    }

    pub fn record_set(&self, thread_id: &str, replaced: bool) {
        Self::bump(&self.goals_set);
        if replaced {
            Self::bump(&self.goals_replaced);
        }
        Self::emit("goals_set", thread_id);
    }

    pub fn record_pause(&self, thread_id: &str) {
        Self::bump(&self.goals_paused);
        Self::emit("goals_paused", thread_id);
    }

    pub fn record_resume(&self, thread_id: &str) {
        Self::bump(&self.goals_resumed);
        Self::emit("goals_resumed", thread_id);
    }

    pub fn record_clear(&self, thread_id: &str) {
        Self::bump(&self.goals_cleared);
        Self::emit("goals_cleared", thread_id);
    }

    pub fn record_edit(&self, thread_id: &str) {
        Self::bump(&self.goals_edited);
        Self::emit("goals_edited", thread_id);
    }

    pub fn record_complete(&self, thread_id: &str) {
        Self::bump(&self.goals_completed);
        Self::emit("goals_completed", thread_id);
    }

    pub fn record_blocked(&self, thread_id: &str) {
        Self::bump(&self.goals_blocked);
        Self::emit("goals_blocked", thread_id);
    }

    pub fn record_usage_limited(&self, thread_id: &str) {
        Self::bump(&self.goals_usage_limited);
        Self::emit("goals_usage_limited", thread_id);
    }

    pub fn record_budget_limited(&self, thread_id: &str) {
        Self::bump(&self.goals_budget_limited);
        Self::emit("goals_budget_limited", thread_id);
    }

    pub fn record_continuation_started(&self, thread_id: &str) {
        Self::bump(&self.continuations_started);
        Self::emit("continuations_started", thread_id);
    }

    pub fn record_continuation_deferred(&self, thread_id: &str) {
        Self::bump(&self.continuations_deferred);
        Self::emit("continuations_deferred", thread_id);
    }

    /// 测试/诊断用快照。
    pub fn snapshot(&self) -> GoalMetricsSnapshot {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        GoalMetricsSnapshot {
            goals_set: g(&self.goals_set),
            goals_replaced: g(&self.goals_replaced),
            goals_paused: g(&self.goals_paused),
            goals_resumed: g(&self.goals_resumed),
            goals_cleared: g(&self.goals_cleared),
            goals_edited: g(&self.goals_edited),
            goals_completed: g(&self.goals_completed),
            goals_blocked: g(&self.goals_blocked),
            goals_usage_limited: g(&self.goals_usage_limited),
            goals_budget_limited: g(&self.goals_budget_limited),
            continuations_started: g(&self.continuations_started),
            continuations_deferred: g(&self.continuations_deferred),
        }
    }
}

/// [`GoalMetrics::snapshot`] 的纯数据视图。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GoalMetricsSnapshot {
    pub goals_set: u64,
    pub goals_replaced: u64,
    pub goals_paused: u64,
    pub goals_resumed: u64,
    pub goals_cleared: u64,
    pub goals_edited: u64,
    pub goals_completed: u64,
    pub goals_blocked: u64,
    pub goals_usage_limited: u64,
    pub goals_budget_limited: u64,
    pub continuations_started: u64,
    pub continuations_deferred: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    // 单进程共享 global——并发测试下只做单调性断言（≥），不做精确相等。
    #[test]
    fn global_counters_monotonic() {
        let m = global();
        let before = m.snapshot();
        m.record_set("t-metrics", true);
        m.record_continuation_started("t-metrics");
        let after = m.snapshot();
        assert!(after.goals_set > before.goals_set);
        assert!(after.goals_replaced > before.goals_replaced);
        assert!(after.continuations_started > before.continuations_started);
    }
}

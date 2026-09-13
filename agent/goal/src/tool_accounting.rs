//! B3（gap-remediation G7）：per-tool 结果记账与 exec 连续失败判据。
//!
//! Codex `ext/goal/src/accounting.rs::record_tool_outcome` +
//! `execution_failure_goal` 的等价物：同一 goal 连续
//! [`MAX_CONSECUTIVE_EXECUTION_FAILURES`] 个 goal turn 出现「失败 exec 且
//! 无任何成功工具」→ ExecutionUnavailable blocked，防止 shell/执行环境坏掉
//! 时自动续跑循环无限烧 token。这是 blocked 的第二个系统触发源（第一个是
//! turn 不可恢复错误，见 `GoalRuntimeHandle::on_turn_error`）。
//!
//! 纯内存、零 IO：`record_tool_outcome` 在流事件热路径上调用（host 的
//! `on_event` 闭包），只做 Mutex 内的布尔记账。

use std::sync::Mutex;

/// 计入 exec 失败判定的工具注册名。codex 的 `exec` ↔ loom 侧 tool-basic
/// 的 shell 工具；容错收录历史/别名注册名，落地以 create_acp_tools 注册表
/// 实际名称为准。
pub const EXEC_TOOL_NAMES: &[&str] = &["bash", "exec", "shell"];

/// 连续失败 goal turn 阈值（与 codex 一致）。
pub const MAX_CONSECUTIVE_EXECUTION_FAILURES: u8 = 3;

#[derive(Debug, Default, Clone, Copy)]
struct TurnToolStats {
    /// 本 turn 出现过失败的 exec 类工具。
    failed_execution: bool,
    /// 本 turn 出现过任一成功工具（任意工具成功即豁免本 turn）。
    successful_tool: bool,
}

#[derive(Debug, Default)]
struct Inner {
    current: TurnToolStats,
    /// 连击归属的 goal_id（goal 更换即重新起算）。
    execution_failure_goal_id: Option<String>,
    consecutive_execution_failure_turns: u8,
}

/// per-thread 工具结果记账（挂在 `GoalRuntimeHandle` 上）。
#[derive(Debug, Default)]
pub struct ToolAccounting {
    inner: Mutex<Inner>,
}

impl ToolAccounting {
    pub fn new() -> Self {
        Self::default()
    }

    /// 流事件热路径：记录一次工具结果。
    /// - 失败：仅 exec 类工具名计入 `failed_execution`（codex 只认
    ///   default-namespace exec 的 handler 级失败；无名的 `ToolError` 不计）。
    /// - 成功：任一工具成功 → 本 turn 豁免，且清零跨 turn 连击。
    pub fn record_tool_outcome(&self, tool: &str, failed: bool) {
        let mut inner = self.inner.lock().expect("tool accounting poisoned");
        if failed {
            if EXEC_TOOL_NAMES.contains(&tool) {
                inner.current.failed_execution = true;
            }
        } else {
            inner.current.successful_tool = true;
            inner.execution_failure_goal_id = None;
            inner.consecutive_execution_failure_turns = 0;
        }
    }

    /// turn 开始：重置本 turn 统计（跨 turn 连击保留）。
    pub fn begin_turn(&self) {
        self.inner.lock().expect("tool accounting poisoned").current = TurnToolStats::default();
    }

    /// turn 结束判定。`bound_goal_id` 为本 turn 绑定的 goal（无绑定/豁免
    /// turn 传 `None`，不累计）。返回 `Some(goal_id)` 表示连续失败达标，
    /// 调用方应将该 goal 置为 blocked（计数随即重新起算——resume 后
    /// fresh audit，对齐 codex spec 的恢复语义）。
    pub fn execution_failure_goal(&self, bound_goal_id: Option<&str>) -> Option<String> {
        let mut inner = self.inner.lock().expect("tool accounting poisoned");
        let goal_id = bound_goal_id?.to_string();
        if inner.execution_failure_goal_id.as_deref() != Some(goal_id.as_str()) {
            // goal 更换：重新起算
            inner.execution_failure_goal_id = Some(goal_id.clone());
            inner.consecutive_execution_failure_turns = 0;
        }
        if inner.current.successful_tool || !inner.current.failed_execution {
            return None;
        }
        inner.consecutive_execution_failure_turns += 1;
        if inner.consecutive_execution_failure_turns >= MAX_CONSECUTIVE_EXECUTION_FAILURES {
            inner.consecutive_execution_failure_turns = 0;
            return Some(goal_id);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_consecutive_failed_exec_turns_trigger() {
        let ta = ToolAccounting::new();
        for _ in 0..2 {
            ta.begin_turn();
            ta.record_tool_outcome("bash", true);
            assert!(ta.execution_failure_goal(Some("g1")).is_none());
        }
        ta.begin_turn();
        ta.record_tool_outcome("bash", true);
        assert_eq!(
            ta.execution_failure_goal(Some("g1")).as_deref(),
            Some("g1"),
            "第三个失败 turn 应触发 blocked"
        );
    }

    #[test]
    fn any_successful_tool_resets_streak_and_exempts_turn() {
        let ta = ToolAccounting::new();
        for _ in 0..2 {
            ta.begin_turn();
            ta.record_tool_outcome("bash", true);
            assert!(ta.execution_failure_goal(Some("g1")).is_none());
        }
        // 第三个 turn 出现成功工具 → 豁免且清零
        ta.begin_turn();
        ta.record_tool_outcome("read", false);
        ta.record_tool_outcome("bash", true);
        assert!(ta.execution_failure_goal(Some("g1")).is_none());
        // 重新计满 3 个才再触发
        for _ in 0..2 {
            ta.begin_turn();
            ta.record_tool_outcome("bash", true);
            assert!(ta.execution_failure_goal(Some("g1")).is_none());
        }
        ta.begin_turn();
        ta.record_tool_outcome("bash", true);
        assert!(ta.execution_failure_goal(Some("g1")).is_some());
    }

    #[test]
    fn goal_change_restarts_audit() {
        let ta = ToolAccounting::new();
        for _ in 0..2 {
            ta.begin_turn();
            ta.record_tool_outcome("bash", true);
            assert!(ta.execution_failure_goal(Some("g1")).is_none());
        }
        // 用户替换 goal → 连击重新起算
        ta.begin_turn();
        ta.record_tool_outcome("bash", true);
        assert!(ta.execution_failure_goal(Some("g2")).is_none());
    }

    #[test]
    fn non_exec_failures_and_unnamed_tool_errors_are_ignored() {
        let ta = ToolAccounting::new();
        for _ in 0..MAX_CONSECUTIVE_EXECUTION_FAILURES + 1 {
            ta.begin_turn();
            ta.record_tool_outcome("read", true); // 非 exec 失败不计
            ta.record_tool_outcome("", true); // 无名 ToolError 不计
            assert!(ta.execution_failure_goal(Some("g1")).is_none());
        }
    }

    #[test]
    fn unbound_turns_do_not_accumulate() {
        let ta = ToolAccounting::new();
        for _ in 0..MAX_CONSECUTIVE_EXECUTION_FAILURES + 1 {
            ta.begin_turn();
            ta.record_tool_outcome("bash", true);
            assert!(ta.execution_failure_goal(None).is_none());
        }
    }

    #[test]
    fn trigger_restarts_count_for_post_resume_audit() {
        let ta = ToolAccounting::new();
        // 前两个失败 turn：未达阈值
        for _ in 0..MAX_CONSECUTIVE_EXECUTION_FAILURES - 1 {
            ta.begin_turn();
            ta.record_tool_outcome("bash", true);
            assert!(ta.execution_failure_goal(Some("g1")).is_none());
        }
        // 第三个 → 触发（计数随即重新起算）
        ta.begin_turn();
        ta.record_tool_outcome("bash", true);
        assert!(ta.execution_failure_goal(Some("g1")).is_some());
        // blocked → resume 后 fresh audit：再计满 3 个才再次触发
        for _ in 0..MAX_CONSECUTIVE_EXECUTION_FAILURES - 1 {
            ta.begin_turn();
            ta.record_tool_outcome("bash", true);
            assert!(ta.execution_failure_goal(Some("g1")).is_none());
        }
        ta.begin_turn();
        ta.record_tool_outcome("bash", true);
        assert!(ta.execution_failure_goal(Some("g1")).is_some());
    }
}

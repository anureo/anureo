//! Goal 类型定义（goal-codex-alignment §5 / §6.1 / §6.3）。
//!
//! 语义基线从 Codex ext/goal 原样搬运：6 态状态机 + `is_terminal` +
//! AccountingMode 四档。anureo 扩展仅两处：`verify_command` 载体（§6.9）、
//! `status_reason` 归因（§7.2 迁移映射），两者均为 P0 审计新增的表列。

use serde::{Deserialize, Serialize};

/// Goal 生命周期 6 态（alignment §6.1）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    Active,
    Paused,
    Blocked,
    /// provider/账户用量限制（系统置位），**不是** runner 失败（§6.1）。
    UsageLimited,
    BudgetLimited,
    Complete,
}

impl GoalStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::UsageLimited => "usage_limited",
            Self::BudgetLimited => "budget_limited",
            Self::Complete => "complete",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "usage_limited" => Self::UsageLimited,
            "budget_limited" => Self::BudgetLimited,
            "complete" => Self::Complete,
            _ => return None,
        })
    }

    /// 终态 = `{budget_limited, complete}`（alignment §6.1）。
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::BudgetLimited | Self::Complete)
    }

    /// 用户可恢复（resume/set → active）：paused / blocked / usage_limited /
    /// budget_limited（B2：软终态可提额后 resume，对齐 codex 外部
    /// `set(status=Active)`；complete 仍不可恢复）。
    pub const fn user_resumable(self) -> bool {
        matches!(
            self,
            Self::Paused | Self::Blocked | Self::UsageLimited | Self::BudgetLimited
        )
    }

    pub const ALL: [GoalStatus; 6] = [
        Self::Active,
        Self::Paused,
        Self::Blocked,
        Self::UsageLimited,
        Self::BudgetLimited,
        Self::Complete,
    ];
}

/// 终止性 turn 错误的 goal 语义分类（gap-remediation A1）。
///
/// Codex 用类型化 `CodexErrorInfo::UsageLimitExceeded` 区分（ext/goal
/// extension.rs `on_turn_error`）：配额/额度耗尽 → usage_limited（用户可
/// 操作、可恢复），其余不可恢复错误 → blocked（阻止续跑循环烧 token）。
/// 本枚举由 host 侧从 `RunError` 归一（goal crate 不依赖 agent-core /
/// model-spec-core，保持依赖方向约束）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnErrorClass {
    /// 配额/额度耗尽：active → usage_limited（系统置位）。
    UsageLimited,
    /// 其余不可恢复错误：active → blocked。
    TurnError,
}

/// 记账允许冲账的状态集（alignment §6.3 AccountingMode）。
/// 变体名与 alignment §6.3 文档词汇一一对应，前缀非本意，勿改名。
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountingMode {
    /// 正常进度：仅 active。
    ActiveStatusOnly,
    /// tool/turn 结束补记越界前后最后一段：active, budget_limited。
    ActiveOnly,
    /// 模型完成时补齐最后使用量：active, budget_limited, complete。
    ActiveOrComplete,
    /// 错误/停止路径补账：active, paused, blocked, usage_limited, budget_limited。
    ActiveOrStopped,
}

impl AccountingMode {
    pub const fn allows(self, status: GoalStatus) -> bool {
        use GoalStatus::*;
        match self {
            Self::ActiveStatusOnly => matches!(status, Active),
            Self::ActiveOnly => matches!(status, Active | BudgetLimited),
            Self::ActiveOrComplete => matches!(status, Active | BudgetLimited | Complete),
            // ActiveOrStopped = 除 complete 外全部（§6.3 表）
            Self::ActiveOrStopped => !matches!(status, Complete),
        }
    }

    /// 生成 SQL `status IN (...)` 片段（与 [`Self::allows`] 必须保持一致；
    /// `test_allowed_sql_matches_allows` 保证两者同步）。
    pub const fn allowed_statuses_sql(self) -> &'static str {
        match self {
            Self::ActiveStatusOnly => "'active'",
            Self::ActiveOnly => "'active','budget_limited'",
            Self::ActiveOrComplete => "'active','budget_limited','complete'",
            Self::ActiveOrStopped => "'active','paused','blocked','usage_limited','budget_limited'",
        }
    }
}

/// Goal 行（`thread_goals` 表投影）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    pub thread_id: String,
    pub goal_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<i64>,
    pub tokens_used: i64,
    pub time_used_seconds: i64,
    /// anureo 扩展（§6.9）：verify 完成门命令。
    pub verify_command: Option<String>,
    /// anureo 扩展（§7.2）：blocked / usage_limited 归因。
    pub status_reason: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    /// C2（gap-remediation G8）：用户 edit 目标 +1；create/set/replace 归 0。
    /// runtime 以「DB revision > last_seen」判定 mid-turn 目标变更。
    pub objective_revision: i64,
    /// C1（gap-remediation G9）：goal turn 迭代计数（goal 驱动 turn +1；
    /// create/set/replace 归 0），对应 codex turn_trigger 元数据面。
    pub iteration_count: i64,
    /// P7 objective 文件化：DB `objective` 列存 `@file:<name>` 标记，
    /// 文本在 `<goals_dir>/<name>`（非持久标志，由 goal_from_row 按前缀派生；
    /// serde skip——投影/序列化面用 [`crate::objective_file] 判定）。文本消费方
    /// （steering/REPL/legacy get）需经 `resolve_objective` 取全文。
    #[serde(skip)]
    pub objective_file: bool,
}

impl Goal {
    /// `remaining_tokens = max(budget − used, 0)`；无预算 → `None`（§6.8 get_goal）。
    pub fn remaining_tokens(&self) -> Option<i64> {
        self.token_budget.map(|b| (b - self.tokens_used).max(0))
    }
}

/// objective 内联长度上限（字节，与 Codex 对齐）；超过走 P7 objective 文件化
/// （DB 存 `@file:` 标记，文本落 `<goals_dir>/<thread>.md`）。
pub const MAX_OBJECTIVE_LEN: usize = 4000;

/// `max_goal_token_budget` 护栏（盲审 A1：全仓此前不存在）。
/// 默认 5,000,000；`ANUREO_MAX_GOAL_TOKEN_BUDGET` 可覆盖（非法值回退默认）。
pub fn max_goal_token_budget() -> i64 {
    std::env::var("ANUREO_MAX_GOAL_TOKEN_BUDGET")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(5_000_000)
}

/// store 层 create 请求（模型/系统路径，§6.8：不能覆盖 unfinished goal；
/// 用户 set 的替换入口用 [`GoalStore::replace`]，无此限制）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateGoalRequest {
    pub thread_id: String,
    pub objective: String,
    pub token_budget: Option<i64>,
    pub verify_command: Option<String>,
}

impl CreateGoalRequest {
    pub fn validate(&self) -> Result<(), GoalValidationError> {
        let objective = self.objective.trim();
        if objective.is_empty() {
            return Err(GoalValidationError::EmptyObjective);
        }
        if objective.len() > MAX_OBJECTIVE_LEN {
            return Err(GoalValidationError::ObjectiveTooLong);
        }
        if let Some(b) = self.token_budget {
            if b <= 0 {
                return Err(GoalValidationError::BudgetMustBePositive);
            }
            let cap = max_goal_token_budget();
            if b > cap {
                return Err(GoalValidationError::BudgetAboveCap(cap));
            }
        }
        Ok(())
    }

    /// trim 后的 objective（store 落库用）。
    pub fn trimmed_objective(&self) -> &str {
        self.objective.trim()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GoalValidationError {
    #[error("objective is empty after trim")]
    EmptyObjective,
    #[error("objective exceeds {MAX_OBJECTIVE_LEN} chars")]
    ObjectiveTooLong,
    #[error("token budget must be positive")]
    BudgetMustBePositive,
    #[error("token budget exceeds max_goal_token_budget cap ({0})")]
    BudgetAboveCap(i64),
}

/// `account_thread_goal_usage` 结果（§6.2：`Unchanged` → delta 丢弃、
/// 内存基线**不**推进，防止旧 turn 写到新 goal）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountingOutcome {
    Updated {
        tokens_used: i64,
        status: GoalStatus,
    },
    Unchanged,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_are_budget_limited_and_complete() {
        assert!(GoalStatus::BudgetLimited.is_terminal());
        assert!(GoalStatus::Complete.is_terminal());
        for s in [
            GoalStatus::Active,
            GoalStatus::Paused,
            GoalStatus::Blocked,
            GoalStatus::UsageLimited,
        ] {
            assert!(!s.is_terminal(), "{s:?} 不应是终态");
        }
    }

    #[test]
    fn status_roundtrip_via_str() {
        for s in GoalStatus::ALL {
            assert_eq!(GoalStatus::parse(s.as_str()), Some(s));
        }
        assert_eq!(GoalStatus::parse("cancelled"), None);
    }

    /// AccountingMode 状态矩阵逐档断言（§6.3 表，TODO P1 单测项）。
    #[test]
    fn accounting_mode_matrix() {
        use AccountingMode::*;
        use GoalStatus::*;
        let cases: [(AccountingMode, &[GoalStatus]); 4] = [
            (ActiveStatusOnly, &[Active]),
            (ActiveOnly, &[Active, BudgetLimited]),
            (ActiveOrComplete, &[Active, BudgetLimited, Complete]),
            (
                ActiveOrStopped,
                &[Active, Paused, Blocked, UsageLimited, BudgetLimited],
            ),
        ];
        for (mode, allowed) in cases {
            for status in GoalStatus::ALL {
                assert_eq!(
                    mode.allows(status),
                    allowed.contains(&status),
                    "{mode:?} × {status:?} 不符 §6.3"
                );
            }
        }
    }

    #[test]
    fn allowed_sql_matches_allows() {
        // SQL 片段与 allows 必须同步漂移
        for mode in [
            AccountingMode::ActiveStatusOnly,
            AccountingMode::ActiveOnly,
            AccountingMode::ActiveOrComplete,
            AccountingMode::ActiveOrStopped,
        ] {
            for status in GoalStatus::ALL {
                let in_sql = mode.allowed_statuses_sql().contains(status.as_str());
                assert_eq!(in_sql, mode.allows(status), "{mode:?} × {status:?}");
            }
        }
    }

    #[test]
    fn create_request_validation() {
        let ok = CreateGoalRequest {
            thread_id: "t".into(),
            objective: "  do it  ".into(),
            token_budget: Some(100),
            verify_command: None,
        };
        assert!(ok.validate().is_ok());
        assert_eq!(ok.trimmed_objective(), "do it");

        let empty = CreateGoalRequest {
            objective: "   ".into(),
            ..ok.clone()
        };
        assert_eq!(empty.validate(), Err(GoalValidationError::EmptyObjective));

        let nonpos = CreateGoalRequest {
            token_budget: Some(0),
            ..ok.clone()
        };
        assert_eq!(
            nonpos.validate(),
            Err(GoalValidationError::BudgetMustBePositive)
        );

        let huge = CreateGoalRequest {
            token_budget: Some(i64::MAX),
            ..ok.clone()
        };
        assert!(matches!(
            huge.validate(),
            Err(GoalValidationError::BudgetAboveCap(_))
        ));
    }

    #[test]
    fn remaining_tokens_saturates_at_zero() {
        let mut g = sample_goal();
        g.token_budget = Some(100);
        g.tokens_used = 150;
        assert_eq!(g.remaining_tokens(), Some(0));
        g.tokens_used = 40;
        assert_eq!(g.remaining_tokens(), Some(60));
        g.token_budget = None;
        assert_eq!(g.remaining_tokens(), None);
    }

    fn sample_goal() -> Goal {
        Goal {
            thread_id: "t".into(),
            goal_id: "g".into(),
            objective: "obj".into(),
            status: GoalStatus::Active,
            token_budget: None,
            tokens_used: 0,
            time_used_seconds: 0,
            verify_command: None,
            status_reason: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            objective_revision: 0,
            iteration_count: 0,
            objective_file: false,
        }
    }
}

//! OTel 指标导出（`feature = "otel"`，alignment Phase 7 可选项）。
//!
//! 把 [`crate::metrics::global`] 的进程内计数器暴露为 OTel Observable
//! Counter（`anureo.goal.*` 命名空间）。宿主通过 `set_meter_provider`
//! 安装了全局 MeterProvider 即随采集周期导出；未安装则为 OTel 标准
//! no-op（API 层不落地）。SDK/导出器依赖由宿主自选——本 crate 只依赖
//! `opentelemetry` API 门面。
//!
//! 用法（宿主初始化时调用一次）：
//!
//! ```ignore
//! // 安装 SDK MeterProvider 后：
//! goal::otel::install();
//! ```

use std::sync::atomic::Ordering;

use opentelemetry::global;

use crate::metrics::global;

/// 注册 `anureo.goal.*` Observable Counter（幂等安全：重复调用只会创建
/// 重复 instrument，宿主应只在初始化路径调用一次）。
pub fn install() {
    let meter = global::meter("anureo.goal");
    let m = global();

    macro_rules! observable_counter {
        ($name:literal, $desc:literal, $field:ident) => {
            let metrics = m;
            let _ = meter
                .u64_observable_counter($name)
                .with_description($desc)
                .with_callback(move |inst| {
                    inst.observe(metrics.$field.load(Ordering::Relaxed), &[]);
                })
                .build();
        };
    }

    observable_counter!(
        "anureo.goal.set.total",
        "Goals set via user/system set (includes replacements).",
        goals_set
    );
    observable_counter!(
        "anureo.goal.replaced.total",
        "Set operations that replaced an existing goal (snapshot replacement).",
        goals_replaced
    );
    observable_counter!(
        "anureo.goal.paused.total",
        "Goals paused by user.",
        goals_paused
    );
    observable_counter!(
        "anureo.goal.resumed.total",
        "Goals resumed by user.",
        goals_resumed
    );
    observable_counter!(
        "anureo.goal.cleared.total",
        "Goals cleared by user.",
        goals_cleared
    );
    observable_counter!(
        "anureo.goal.edited.total",
        "Goal objectives edited.",
        goals_edited
    );
    observable_counter!(
        "anureo.goal.completed.total",
        "Goals reached complete (model declaration / verify gate).",
        goals_completed
    );
    observable_counter!(
        "anureo.goal.blocked.total",
        "Goals blocked (turn error / model declaration).",
        goals_blocked
    );
    observable_counter!(
        "anureo.goal.usage_limited.total",
        "Goals moved to usage_limited (provider quota exhausted).",
        goals_usage_limited
    );
    observable_counter!(
        "anureo.goal.budget_limited.total",
        "Goals moved to budget_limited (token budget exceeded).",
        goals_budget_limited
    );
    observable_counter!(
        "anureo.goal.continuations.started.total",
        "Idle continuations actually started (start_turn_if_idle accepted).",
        continuations_started
    );
    observable_counter!(
        "anureo.goal.continuations.deferred.total",
        "Idle continuations deferred (deferral marker present).",
        continuations_deferred
    );
}

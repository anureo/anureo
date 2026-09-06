-- thread_goals：goal 状态单一事实源（goal-codex-alignment §7.1，Codex ext/goal 同构）。
--
-- 相对 §7.1 的 anureo 扩展列（偏离已在 docs/goal/goal-codex-alignment-todo.md
-- 「追加发现项」登记，并回写 alignment 附录 B.7）：
--   verify_command  —— 完成门命令载体（§6.9；现状 GoalMeta 字段，P0 发现 §7.1 缺载体）
--   status_reason   —— blocked / usage_limited 归因（§7.2 迁移映射要求 lifecycle_reason）

CREATE TABLE thread_goals (
    thread_id TEXT PRIMARY KEY NOT NULL,
    goal_id TEXT NOT NULL,
    objective TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN (
        'active','paused','blocked','usage_limited','budget_limited','complete'
    )),
    token_budget INTEGER,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    time_used_seconds INTEGER NOT NULL DEFAULT 0,
    verify_command TEXT,
    status_reason TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

-- 续跑推迟标记（§6.6）：FK 级联——goal 行被删除（clear/替换）时自动清除。
-- 前置条件：TaskDb 连接必须 foreign_keys(true)（db.rs 前置修复，否则 CASCADE 失效）。
CREATE TABLE thread_goal_continuation_deferrals (
    thread_id TEXT PRIMARY KEY NOT NULL
        REFERENCES thread_goals(thread_id) ON DELETE CASCADE
);

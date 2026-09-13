-- C1/C2（goal-codex-gap-remediation-plan §7）：续跑链路元数据列。
--   objective_revision —— 用户 edit 目标即 +1；runtime 依此在下一 goal turn
--                         边界渲染 objective_updated steering（G8）。
--   iteration_count    —— goal turn 迭代计数（create/set/replace 归 0；
--                         每次 goal 驱动 turn +1），对应 codex turn_trigger
--                         元数据面（G9）。
ALTER TABLE thread_goals ADD COLUMN objective_revision INTEGER NOT NULL DEFAULT 0;
ALTER TABLE thread_goals ADD COLUMN iteration_count INTEGER NOT NULL DEFAULT 0;

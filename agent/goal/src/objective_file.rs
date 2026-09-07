//! objective 文件化（alignment Phase 7 可选项）。
//!
//! objective 超过内联上限（[`needs_file_backing`]：chars > 4000 或字节超出
//! DB 内联校验）时，**文本落盘** `<anureo_home>/goals/<thread_id>.md`，
//! DB `objective` 列改存 [`INLINE_MARKER_PREFIX`] 标记（`@file:<file_name>`），
//! `Goal.objective_file` 标志置位。投影面（中立快照 `_meta.goal` /
//! `session.metadata.anureo.goal`）以 `objectiveFile: true` 代替全文，避免
//! 大 payload 撑爆通知；文本消费方（steering 注入、REPL show、legacy get）
//! 经 `GoalStore::resolve_objective` 取全文。
//!
//! 目录来源：[`GoalStore`] 的 `goals_dir`（`from_task_db` 从
//! `<home>/tasks/tasks.db` 推导 `<home>/goals`；无推导结果时文件化
//! no-op——超长 objective 落回 DB 内联校验路径，照常拒绝）。

use std::path::{Path, PathBuf};

/// objective 内联上限（chars）；超出走文件 + `objectiveFile: true` 投影。
pub const OBJECTIVE_INLINE_LIMIT_CHARS: usize = 4000;

/// objective 是否超出内联上限（决定文件化与投影形状）。
pub fn objective_exceeds_limit(objective: &str) -> bool {
    objective.chars().count() > OBJECTIVE_INLINE_LIMIT_CHARS
}

/// thread_id → 文件名（session id 可能含 `:`/`/` 等，白名单转义）。
fn file_name_for(thread_id: &str) -> String {
    let safe: String = thread_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.md")
}

/// DB `objective` 列的文件化标记前缀（`@file:<file_name>`，相对 goals_dir）。
pub const INLINE_MARKER_PREFIX: &str = "@file:";

/// objective 是否需要文件化：超过内联上限（chars），或超出 DB 内联字节上限
/// （`MAX_OBJECTIVE_LEN`——CJK 长文本 chars 未超但字节超，同样不能内联）。
pub fn needs_file_backing(objective: &str) -> bool {
    objective_exceeds_limit(objective) || objective.len() > crate::types::MAX_OBJECTIVE_LEN
}

/// 文件化 objective 的绝对上限（chars，防摘式滥用；超此拒收）。
pub const MAX_FILE_OBJECTIVE_CHARS: usize = 1_000_000;

/// DB 标记值：`@file:<file_name>`。
pub fn marker_for(thread_id: &str) -> String {
    format!("{INLINE_MARKER_PREFIX}{}", file_name_for(thread_id))
}

/// objective 是否为文件化标记。
pub fn is_file_backed(objective: &str) -> bool {
    objective.starts_with(INLINE_MARKER_PREFIX)
}

/// 标记 → 文件名（非标记返回 `None`）。
pub fn marker_file_name(objective: &str) -> Option<&str> {
    objective.strip_prefix(INLINE_MARKER_PREFIX)
}

/// objective 文件路径：`<goals_dir>/<thread_id>.md`。
pub fn file_path(dir: &Path, thread_id: &str) -> PathBuf {
    dir.join(file_name_for(thread_id))
}

/// 写 objective 文件（覆盖式；目录不存在则创建）。失败由调用方记日志降级
/// （DB 全文仍是事实源，文件只是 FE 投影通道）。
pub async fn write(dir: &Path, thread_id: &str, objective: &str) -> std::io::Result<PathBuf> {
    let path = file_path(dir, thread_id);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, objective).await?;
    Ok(path)
}

/// 删除 objective 文件（不存在则忽略）。
pub async fn remove(dir: &Path, thread_id: &str) {
    let _ = tokio::fs::remove_file(file_path(dir, thread_id)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_boundary() {
        assert!(!objective_exceeds_limit(&"a".repeat(4000)));
        assert!(objective_exceeds_limit(&"a".repeat(4001)));
        // 多字节字符按 chars 计数
        let wide = "目".repeat(4000);
        assert!(!objective_exceeds_limit(&wide));
        assert!(objective_exceeds_limit(&format!("{wide}!")));
    }

    #[test]
    fn file_name_sanitized() {
        assert_eq!(file_name_for("s-abc_123"), "s-abc_123.md");
        assert_eq!(file_name_for("a/b:c"), "a_b_c.md");
    }

    #[tokio::test]
    async fn write_and_remove_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = write(dir.path(), "t1", "long objective").await.expect("write");
        assert_eq!(p, file_path(dir.path(), "t1"));
        assert_eq!(tokio::fs::read_to_string(&p).await.expect("read"), "long objective");
        remove(dir.path(), "t1").await;
        assert!(!p.exists());
        // remove 幂等（不存在不报错）
        remove(dir.path(), "t1").await;
    }
}

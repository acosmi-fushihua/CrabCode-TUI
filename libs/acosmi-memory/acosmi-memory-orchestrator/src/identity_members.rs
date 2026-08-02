//! W-MEMORY-SELF-EVOLVE-DGM G2 (2026-07-16) — 项目身份成员关系（worktree
//! 归一的 orchestrator 侧读取面）。
//!
//! 背景：TS 侧 `findCanonicalGitRoot` 早已把同仓所有 worktree 归一到主检出
//! 的记忆目录，而 Rust dispatcher 此前用 `--show-toplevel`（每个 worktree
//! 各一个 slug）—— TS↔Rust 漂移造成 per-checkout 记忆裂脑。G2 对齐后，
//! dispatcher（`crates/acosmi-app-server/src/dispatcher/memory.rs`）在解析
//! 到 worktree cwd 时写两份**身份标记**（JSON 文件形状即跨 workspace 契约，
//! 两侧测试各自钉死）：
//!
//! - canonical 项目：`.memory-rust-derived/identity-members.json`
//!   `{"members": ["<member-slug>", …]}`（cap 64）；
//! - member 项目：`.memory-rust-derived/identity-canonical.json`
//!   `{"canonical_slug": "<canonical-slug>"}`。
//!
//! orchestrator 三个消费点：
//! 1. 语料装配（`dream_corpus`）——canonical 项目做梦时并读成员项目的转写
//!    （worktree 会话的经验汇入同一份记忆）；
//! 2. gate 会话计数（`dream_gate::list_sessions_touched_since`）——与语料
//!    同口径，否则 gate 说 0 会话而语料有料；
//! 3. 周期轮转（`rotation_candidates`）——跳过 member 项目（其转写已由
//!    canonical 侧消费，双整理 = 同素材烧双份配额）。

use std::path::{Path, PathBuf};

pub const IDENTITY_MEMBERS_FILENAME: &str = "identity-members.json";
pub const IDENTITY_CANONICAL_FILENAME: &str = "identity-canonical.json";
pub const MEMBERS_CAP: usize = 64;

/// canonical 项目登记的成员 slug 清单（缺失/损坏 → 空）。
#[must_use]
pub fn member_slugs(project_state_dir: &Path) -> Vec<String> {
    let path =
        crate::daily_log::rust_derived_root(project_state_dir).join(IDENTITY_MEMBERS_FILENAME);
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| {
            value
                .get("members")
                .and_then(serde_json::Value::as_array)
                .map(|members| {
                    members
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .take(MEMBERS_CAP)
                        .collect()
                })
        })
        .unwrap_or_default()
}

/// canonical 项目的成员 project_state_dir 列表（兄弟目录布局
/// `<base>/projects/<slug>`；只返回真实存在的目录，排除自身）。
#[must_use]
pub fn member_project_state_dirs(project_state_dir: &Path) -> Vec<PathBuf> {
    let Some(projects_dir) = project_state_dir.parent() else {
        return Vec::new();
    };
    let self_name = project_state_dir.file_name();
    member_slugs(project_state_dir)
        .into_iter()
        .filter(|slug| Some(std::ffi::OsStr::new(slug.as_str())) != self_name)
        .map(|slug| projects_dir.join(slug))
        .filter(|dir| dir.is_dir())
        .collect()
}

/// member 项目指回的 canonical slug（缺失/损坏/空 → None）。
#[must_use]
pub fn canonical_redirect_of(project_state_dir: &Path) -> Option<String> {
    let path =
        crate::daily_log::rust_derived_root(project_state_dir).join(IDENTITY_CANONICAL_FILENAME);
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let slug = value.get("canonical_slug")?.as_str()?.trim().to_string();
    if slug.is_empty() {
        return None;
    }
    // 自指 = 无重定向（防御畸形标记）。
    if project_state_dir.file_name() == Some(std::ffi::OsStr::new(slug.as_str())) {
        return None;
    }
    Some(slug)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write_derived(dir: &Path, filename: &str, body: &str) {
        let derived = crate::daily_log::rust_derived_root(dir);
        std::fs::create_dir_all(&derived).unwrap();
        std::fs::write(derived.join(filename), body).unwrap();
    }

    #[test]
    fn members_resolve_to_existing_sibling_dirs_only() {
        let base = TempDir::new().unwrap();
        let projects = base.path().join("projects");
        let canonical = projects.join("D--crabcode");
        let member = projects.join("D--crabcode-wt-dgm");
        std::fs::create_dir_all(&canonical).unwrap();
        std::fs::create_dir_all(&member).unwrap();
        write_derived(
            &canonical,
            IDENTITY_MEMBERS_FILENAME,
            r#"{"members":["D--crabcode-wt-dgm","D--ghost-worktree","D--crabcode"]}"#,
        );

        let dirs = member_project_state_dirs(&canonical);
        assert_eq!(dirs, vec![member], "不存在的目录与自身都被排除");
    }

    #[test]
    fn canonical_redirect_parses_and_rejects_self_reference() {
        let base = TempDir::new().unwrap();
        let member = base.path().join("projects").join("D--crabcode-wt-x");
        std::fs::create_dir_all(&member).unwrap();
        write_derived(
            &member,
            IDENTITY_CANONICAL_FILENAME,
            r#"{"canonical_slug":"D--crabcode"}"#,
        );
        assert_eq!(
            canonical_redirect_of(&member).as_deref(),
            Some("D--crabcode")
        );

        // 自指标记 = 无重定向。
        write_derived(
            &member,
            IDENTITY_CANONICAL_FILENAME,
            r#"{"canonical_slug":"D--crabcode-wt-x"}"#,
        );
        assert_eq!(canonical_redirect_of(&member), None);
    }

    #[test]
    fn missing_or_corrupt_markers_fail_soft() {
        let base = TempDir::new().unwrap();
        let dir = base.path().join("projects").join("p");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(member_slugs(&dir).is_empty());
        assert!(member_project_state_dirs(&dir).is_empty());
        assert_eq!(canonical_redirect_of(&dir), None);

        write_derived(&dir, IDENTITY_MEMBERS_FILENAME, "{corrupt");
        write_derived(&dir, IDENTITY_CANONICAL_FILENAME, "{corrupt");
        assert!(member_slugs(&dir).is_empty());
        assert_eq!(canonical_redirect_of(&dir), None);
    }
}

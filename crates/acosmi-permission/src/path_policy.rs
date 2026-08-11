//! 路径验证策略
//!
//! 等价于 TS pathValidation.ts (485 LOC)
//! 实现 7 层路径权限检查和 validatePath 入口函数。
//!
//! 7 层安全模型:
//!
//! - 1. Deny 规则匹配
//! - 2.5. 安全检查 (dangerous files/directories, Acosmi config) — 必须先于 Layer 2
//!   运行 (A17 / P2-2 #16)，否则 .crabcode 放行会绕过危险文件拦截
//! - 2. 内部可编辑路径 (.crabcode/，锚定到解析后的 config-home)
//! - 3. 工作目录检查 (acceptEdits 门控写入)
//! - 3.5. 内部可读路径
//! - 3.7. 沙箱写允许列表
//! - 4. Allow 规则匹配
//! - 5. 默认拒绝

use crate::constants;
use crate::types::{FileOperationType, PathCheckResult, PermissionDecisionReason};
use std::ffi::OsString;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

// ─── Path Validation Entry ───

/// validatePath 入口（等价于 TS validatePath）
///
/// 处理: 引号剥离, tilde 展开, UNC 阻断, tilde 变体拒绝, shell 展开阻断, glob 验证
#[must_use]
pub fn validate_path(raw_path: &str, cwd: &str, op: FileOperationType) -> PathCheckResult {
    // 1. 引号剥离
    let stripped = strip_surrounding_quotes(raw_path);

    // 2. Tilde 展开
    let expanded = expand_tilde(&stripped);

    // 3. UNC 路径阻断
    if is_unc_path(&expanded) {
        return PathCheckResult {
            allowed: false,
            reason: Some(PermissionDecisionReason::SafetyCheck {
                reason: format!("UNC path blocked (credential leak risk): {raw_path}"),
                classifier_approvable: false,
            }),
        };
    }

    // 4. Tilde 变体拒绝 (~user, ~+, ~-, ~N)
    if has_unexpanded_tilde_variant(&stripped) {
        return PathCheckResult {
            allowed: false,
            reason: Some(PermissionDecisionReason::SafetyCheck {
                reason: format!("Unexpanded tilde variant rejected: {raw_path}"),
                classifier_approvable: false,
            }),
        };
    }

    // 5. Shell 展开阻断
    if has_shell_expansion(&expanded) {
        return PathCheckResult {
            allowed: false,
            reason: Some(PermissionDecisionReason::SafetyCheck {
                reason: format!("Path contains shell expansion characters: {raw_path}"),
                classifier_approvable: false,
            }),
        };
    }

    // 6. Glob 验证 (写操作拒绝 glob，读操作验证基础目录)
    if has_glob_chars(&expanded) {
        if matches!(op, FileOperationType::Write | FileOperationType::Create) {
            return PathCheckResult {
                allowed: false,
                reason: Some(PermissionDecisionReason::SafetyCheck {
                    reason: format!("Glob patterns not allowed for write operations: {raw_path}"),
                    classifier_approvable: false,
                }),
            };
        }
        // 读操作: 验证 glob 基础目录
        let base_dir = get_glob_base_directory(&expanded);
        return is_path_allowed(&base_dir, cwd, op, &[], &[]);
    }

    // 7. 词法路径解析和权限检查
    let resolved = resolve_path(&expanded, cwd);
    let lexical_result = is_path_allowed(&resolved, cwd, op, &[], &[]);
    if !lexical_result.allowed
        || !matches!(op, FileOperationType::Write | FileOperationType::Create)
    {
        return lexical_result;
    }

    // 8. 文件系统身份检查。词法 containment 不能发现
    // `<cwd>/link -> /outside` 或指向 `.crabcode` 的链接。对目标与 cwd
    // 都解析现有祖先；最终目标不存在时，从最近的已存在父级
    // realpath 后追加未存在后缀。任何解析错误都 fail closed。
    validate_real_filesystem_target(&resolved, cwd, op)
}

/// Validate the filesystem identity reached through symlinks.
///
/// This closes policy bypasses through symlinks that already exist at check
/// time, including when the final file itself does not exist. There remains an
/// unavoidable TOCTOU window between this check and a later process opening
/// the path. Eliminating that race requires execution-time `openat`/dirfd
/// confinement or an OS sandbox; permission policy alone cannot provide it.
fn validate_real_filesystem_target(
    resolved_path: &str,
    cwd: &str,
    op: FileOperationType,
) -> PathCheckResult {
    let canonical_cwd = match canonicalize_nearest_existing(cwd) {
        Ok(path) => path,
        Err(reason) => return symlink_resolution_failure(cwd, &reason),
    };
    let canonical_target = match canonicalize_nearest_existing(resolved_path) {
        Ok(path) => path,
        Err(reason) => return symlink_resolution_failure(resolved_path, &reason),
    };

    is_path_allowed(&canonical_target, &canonical_cwd, op, &[], &[])
}

/// Resolve every existing path component, then append a suffix whose
/// components do not exist yet. `std::fs::canonicalize` resolves the complete
/// symlink chain in the existing prefix.
fn canonicalize_nearest_existing(path: &str) -> Result<String, String> {
    let input = Path::new(path);
    let absolute = if input.is_absolute() {
        input.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("cannot resolve current directory: {error}"))?
            .join(input)
    };

    let mut probe = absolute;
    let mut missing_suffix: Vec<OsString> = Vec::new();
    loop {
        match std::fs::symlink_metadata(&probe) {
            Ok(_) => break,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let Some(name) = probe.file_name().map(OsString::from) else {
                    return Err(format!(
                        "no existing ancestor could be resolved for {}",
                        probe.display()
                    ));
                };
                missing_suffix.push(name);
                if !probe.pop() {
                    return Err(format!(
                        "no existing ancestor could be resolved for {}",
                        path
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "cannot inspect path component {}: {error}",
                    probe.display()
                ));
            }
        }
    }

    let mut canonical: PathBuf = std::fs::canonicalize(&probe)
        .map_err(|error| format!("cannot resolve {}: {error}", probe.display()))?;
    for component in missing_suffix.iter().rev() {
        canonical.push(component);
    }
    let canonical = canonical
        .to_str()
        .ok_or_else(|| "resolved path is not valid UTF-8".to_string())?;
    Ok(normalize_path(canonical))
}

fn symlink_resolution_failure(path: &str, reason: &str) -> PathCheckResult {
    PathCheckResult {
        allowed: false,
        reason: Some(PermissionDecisionReason::SafetyCheck {
            reason: format!("Unable to verify symlink-safe path '{path}': {reason}"),
            classifier_approvable: false,
        }),
    }
}

// ─── 7-Layer Path Permission Check ───

/// 7 层路径权限检查（等价于 TS isPathAllowed）
#[must_use]
pub fn is_path_allowed(
    path: &str,
    cwd: &str,
    op: FileOperationType,
    deny_rules: &[&str],
    allow_rules: &[&str],
) -> PathCheckResult {
    let normalized = normalize_path(path);

    // Layer 1: Deny 规则匹配
    for rule in deny_rules {
        if path_matches_rule(&normalized, rule) {
            return PathCheckResult {
                allowed: false,
                reason: Some(PermissionDecisionReason::SafetyCheck {
                    reason: format!("Path denied by rule: {rule}"),
                    classifier_approvable: false,
                }),
            };
        }
    }

    // Layer 2.5: 安全检查 (dangerous files/dirs, config protection)
    //
    // SECURITY (A17 / P2-2 #16): 此安全检查必须 **先于** 下方 Layer 2 的 .crabcode
    // 内部可编辑路径放行运行。否则 `is_internal_editable_path` 的匹配（即便已锚定到
    // 解析后的 config-home）会在危险文件被拦截前先放行 —— 例如 config-home 内的
    // `.bashrc` / `.mcp.json` / `.crabcode.json`。dangerous_files 黑名单的拦截
    // 必须无法被 .crabcode 放行绕过。
    //
    // check_path_safety 内部对 `.crabcode` 这一危险**目录**条目做精确豁免：仅当
    // 整条路径是锚定的内部可编辑路径时跳过该目录段（镜像 TS 端 `.crabcode/worktrees`
    // 结构性 carve-out），从而让合法 `<config-home>/plans/foo.md` 通过、同时
    // `.crabcode` 路径下的危险**文件**仍被 dangerous_files 拦截。
    if matches!(op, FileOperationType::Write | FileOperationType::Create)
        && let Some(reason) = check_path_safety(&normalized)
    {
        return PathCheckResult {
            allowed: false,
            reason: Some(reason),
        };
    }

    // Layer 2: 内部可编辑路径 (.crabcode/，锚定到解析后的 config-home)
    if matches!(op, FileOperationType::Write | FileOperationType::Create)
        && is_internal_editable_path(&normalized)
    {
        return PathCheckResult {
            allowed: true,
            reason: None,
        };
    }

    // Layer 3: 工作目录检查
    if is_path_in_working_directory(&normalized, cwd) && matches!(op, FileOperationType::Read) {
        return PathCheckResult {
            allowed: true,
            reason: None,
        };
    }
    // 写/创建: 需要 acceptEdits 模式（由调用方检查）
    // 这里仅检查路径在工作目录内

    // Layer 3.5: 内部可读路径
    if matches!(op, FileOperationType::Read) && is_internal_readable_path(&normalized) {
        return PathCheckResult {
            allowed: true,
            reason: None,
        };
    }

    // Layer 3.7: 沙箱写允许列表
    // (由调用方提供 sandbox 配置，此处占位)

    // Layer 4: Allow 规则匹配
    for rule in allow_rules {
        if path_matches_rule(&normalized, rule) {
            return PathCheckResult {
                allowed: true,
                reason: None,
            };
        }
    }

    // Layer 5: 默认 — 读操作宽松，写操作严格
    match op {
        FileOperationType::Read => PathCheckResult {
            allowed: true,
            reason: None,
        },
        FileOperationType::Write | FileOperationType::Create => {
            // 写操作默认检查工作目录
            if is_path_in_working_directory(&normalized, cwd) {
                return PathCheckResult {
                    allowed: true,
                    reason: None,
                };
            }
            PathCheckResult {
                allowed: false,
                reason: Some(PermissionDecisionReason::SafetyCheck {
                    reason: format!("Path '{path}' is outside working directory '{cwd}'"),
                    classifier_approvable: true,
                }),
            }
        }
    }
}

/// 基本路径验证（向后兼容）
#[must_use]
pub fn validate_path_basic(path: &str, cwd: &str, op: FileOperationType) -> PathCheckResult {
    validate_path(path, cwd, op)
}

// ─── Safety Checks ───

/// 路径安全检查（Layer 2.5）
fn check_path_safety(normalized_path: &str) -> Option<PermissionDecisionReason> {
    let filename = normalized_path.rsplit('/').next().unwrap_or("");
    let filename_lower = filename.to_ascii_lowercase();

    // CrabCode configuration is executable policy, not ordinary state. Keep
    // these paths behind an explicit approval even when they live below the
    // resolved config-home (which otherwise contains auto-editable plans and
    // scratchpad data). Match case-insensitively and after lexical
    // normalization so case changes and `./` / `..` segments cannot bypass
    // the check.
    if is_sensitive_crabcode_config_path(normalized_path) {
        return Some(PermissionDecisionReason::SafetyCheck {
            reason: format!("CrabCode configuration requires explicit approval: {normalized_path}"),
            classifier_approvable: false,
        });
    }

    // 危险文件
    let dangerous_files = constants::dangerous_files();
    if dangerous_files.contains(filename_lower.as_str()) {
        return Some(PermissionDecisionReason::SafetyCheck {
            reason: format!("Dangerous file: {filename}"),
            classifier_approvable: false,
        });
    }

    // 危险目录
    //
    // `.crabcode` 同时在 dangerous_directories 黑名单里，但合法的内部可编辑路径
    // （计划文件 / scratchpad / settings.json）就住在解析后的 config-home 的
    // `.crabcode` 目录下。对 **这一个** 危险目录条目做精确豁免：当整条路径是锚定到
    // config-home 的内部可编辑路径时，跳过 `.crabcode` 段（镜像 TS 端
    // `.crabcode/worktrees` 结构性 carve-out）。其余危险目录（.git/.vscode/.idea）
    // 与下方危险**文件**检查不受影响 —— 故 `<config-home>/.crabcode/.bashrc`
    // 仍被危险文件检查拦截，`/tmp/x/.crabcode/.bashrc`（未锚定）则被危险目录拦截。
    let dangerous_dirs = constants::dangerous_directories();
    let is_anchored_internal = is_internal_editable_path(normalized_path);
    let lower_path = normalized_path.to_ascii_lowercase();
    let parts: Vec<&str> = lower_path.split('/').collect();
    for part in &parts {
        if dangerous_dirs.contains(*part) {
            if *part == ".crabcode" && is_anchored_internal {
                continue;
            }
            return Some(PermissionDecisionReason::SafetyCheck {
                reason: format!("Path contains dangerous directory: {part}"),
                classifier_approvable: false,
            });
        }
    }

    // 危险删除路径
    if is_dangerous_removal_path(normalized_path) {
        return Some(PermissionDecisionReason::SafetyCheck {
            reason: format!("Dangerous path operation: {normalized_path}"),
            classifier_approvable: false,
        });
    }

    None
}

/// Whether a config-home path must remain behind explicit approval.
///
/// The state directory contains executable/reloadable policy and credentials,
/// so it is deny-by-default for automatic writes. Only the small positive
/// allowlist in `is_safe_internal_editable_relative_path` is exempt.
fn is_sensitive_crabcode_config_path(normalized_path: &str) -> bool {
    let lower = normalized_path.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('/').filter(|part| !part.is_empty()).collect();

    // Protect by identity relative to the resolved state-directory anchor,
    // not merely by a literal `.crabcode` path segment. `CRABCODE_CONFIG_DIR`
    // is a full override and may be named anything. Include both the resolver
    // anchor and its canonical filesystem identity so a symlink cannot hide
    // policy-bearing entries.
    if let Some(relative) = resolved_config_home_anchors()
        .iter()
        .find_map(|anchor| relative_path_under_anchor(normalized_path, anchor))
    {
        return is_sensitive_config_relative_path(relative);
    }

    // An unanchored `.crabcode` tree gets no internal-write carve-out. This
    // includes project-local lookalikes and aliases whose filesystem identity
    // could not be proven against the resolver-selected anchor.
    parts.contains(&".crabcode")
}

fn is_sensitive_config_relative_path(relative: &str) -> bool {
    !is_safe_internal_editable_relative_path(relative)
}

/// Internal config-home paths proven safe for automatic writes.
///
/// This is intentionally a positive allowlist. Plugins, hooks, output styles,
/// workflows, templates, credentials, marketplaces, caches, and future state
/// directories may be loaded or executed and therefore must not inherit a
/// blanket write carve-out merely because they live under config-home.
fn is_internal_editable_path(path: &str) -> bool {
    resolved_config_home_anchors().iter().any(|anchor| {
        relative_path_under_anchor(path, anchor)
            .is_some_and(is_safe_internal_editable_relative_path)
    })
}

fn is_safe_internal_editable_relative_path(relative: &str) -> bool {
    let first = relative.split('/').next().unwrap_or_default();
    matches!(first, "plans" | "scratchpad")
}

/// Check whether one argv token, interpreted as a path, names protected
/// config-home state. Unlike `validate_path`, this does not reject arbitrary
/// outside-workspace paths: it is used as a command-agnostic sensitive-target
/// floor where an argv token may also be a URL or literal.
pub(crate) fn is_sensitive_config_write_target(raw_path: &str, cwd: &str) -> bool {
    let stripped = strip_surrounding_quotes(raw_path);
    let expanded = expand_tilde(&stripped);
    if expanded.is_empty()
        || is_unc_path(&expanded)
        || has_unexpanded_tilde_variant(&stripped)
        || has_shell_expansion(&expanded)
        || has_glob_chars(&expanded)
    {
        return false;
    }

    let resolved = resolve_path(&expanded, cwd);
    if is_sensitive_crabcode_config_path(&resolved) {
        return true;
    }

    canonicalize_nearest_existing(&resolved)
        .is_ok_and(|canonical| is_sensitive_crabcode_config_path(&canonical))
}

/// 内部可读路径
fn is_internal_readable_path(path: &str) -> bool {
    // Preserve the existing broad read behavior independently from the much
    // narrower write allowlist above.
    is_under_resolved_config_home(path)
}

/// 路径是否位于解析后的 config-home（`.crabcode` 状态目录）之内。
///
/// 锚点来自 §硬约束 #4 解析器 `acosmi_config::paths::resolve_state_dir`，其返回值
/// 即 `.crabcode` 目录本身（已包含 `.crabcode` 末段，遵循
/// `CRABCODE_CONFIG_DIR` → `CRABCODE_HOME` → `~/.crabcode` 优先级）。
///
/// 路径在比较前经 `normalize_path` 折叠 `.` / `..`（防 `.crabcode/../.bashrc`
/// 这类遍历绕过）。匹配条件：path == anchor 或 path 以 `anchor/` 为前缀。
fn is_under_resolved_config_home(normalized_path: &str) -> bool {
    resolved_config_home_anchors()
        .iter()
        .any(|anchor| relative_path_under_anchor(normalized_path, anchor).is_some())
}

/// Return both the resolver-selected state-directory anchor and its nearest-
/// existing canonical filesystem identity. The latter matters for a relative
/// or not-yet-created override and keeps internal plans/scratchpad paths usable
/// after the write-path validator canonicalizes a target.
fn resolved_config_home_anchors() -> Vec<String> {
    let anchor_raw = acosmi_config::paths::resolve_state_dir();
    let Some(anchor_str) = anchor_raw.to_str() else {
        return Vec::new();
    };
    let anchor = normalize_path(anchor_str);
    if anchor.is_empty() {
        return Vec::new();
    }

    let mut anchors = vec![anchor];
    if let Ok(canonical) = canonicalize_nearest_existing(anchor_str)
        && !anchors.contains(&canonical)
    {
        anchors.push(canonical);
    }
    anchors
}

/// Return the path relative to an anchor, with exact component-boundary
/// matching. An empty string means the anchor directory itself.
fn relative_path_under_anchor<'a>(path: &'a str, anchor: &str) -> Option<&'a str> {
    if path == anchor {
        return Some("");
    }
    if anchor == "/" {
        return path.strip_prefix('/');
    }
    path.strip_prefix(anchor)?.strip_prefix('/')
}

/// 路径是否匹配规则（简单前缀匹配）
fn path_matches_rule(path: &str, rule: &str) -> bool {
    let normalized_rule = normalize_path(rule);
    path == normalized_rule || path.starts_with(&format!("{normalized_rule}/"))
}

// ─── Path Helpers ───

/// 展开 ~ 为用户 home 目录
#[must_use]
pub fn expand_tilde(path: &str) -> String {
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{}{}", home, &path[1..]);
        }
        if let Ok(profile) = std::env::var("USERPROFILE") {
            return format!("{}{}", profile, &path[1..]);
        }
    }
    path.to_string()
}

/// 检查路径是否为危险删除路径
#[must_use]
pub fn is_dangerous_removal_path(path: &str) -> bool {
    let expanded = expand_tilde(path);
    let trimmed = expanded.trim();

    if trimmed.is_empty() {
        return true;
    }

    if trimmed == "/" {
        return true;
    }

    if trimmed == "*" || trimmed.ends_with("/*") {
        return true;
    }

    if path.trim() == "~" {
        return true;
    }

    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    if !home.is_empty() && trimmed == home {
        return true;
    }

    // Windows 驱动器根目录
    if trimmed.len() <= 3 {
        let bytes = trimmed.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            if bytes.len() == 2 {
                return true;
            }
            if bytes.len() == 3 && (bytes[2] == b'/' || bytes[2] == b'\\') {
                return true;
            }
        }
    }

    let normalized = collapse_slashes(&trimmed.replace('\\', "/"));

    // 根目录直接子项
    if let Some(without_root) = normalized.strip_prefix('/') {
        let without_trailing = without_root.trim_end_matches('/');
        if !without_trailing.is_empty() && !without_trailing.contains('/') {
            return true;
        }
    }

    // Windows 驱动器直接子项
    if normalized.len() >= 4 {
        let bytes = normalized.as_bytes();
        if bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
            let after_drive = &normalized[3..];
            let without_trailing = after_drive.trim_end_matches('/');
            if !without_trailing.is_empty() && !without_trailing.contains('/') {
                return true;
            }
        }
    }

    false
}

/// 合并连续斜杠
fn collapse_slashes(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_slash = false;
    for c in s.chars() {
        if c == '/' {
            if !prev_slash {
                result.push('/');
            }
            prev_slash = true;
        } else {
            result.push(c);
            prev_slash = false;
        }
    }
    result
}

/// 检查路径是否在工作目录内
#[must_use]
pub fn is_path_in_working_directory(path: &str, cwd: &str) -> bool {
    let expanded = expand_tilde(path);
    let normalized_path = normalize_path(&expanded);
    let normalized_cwd = normalize_path(cwd);

    if normalized_path.is_empty() || normalized_cwd.is_empty() {
        return false;
    }

    normalized_path == normalized_cwd || normalized_path.starts_with(&format!("{normalized_cwd}/"))
}

/// 检查路径是否包含 shell 展开字符
#[must_use]
pub fn has_shell_expansion(path: &str) -> bool {
    path.contains('$') || path.contains('%') || path.starts_with('=')
}

/// 检查是否为 UNC 路径
#[must_use]
pub fn is_unc_path(path: &str) -> bool {
    path.starts_with("\\\\") || path.starts_with("//")
}

/// 检查未展开的 tilde 变体
fn has_unexpanded_tilde_variant(path: &str) -> bool {
    if !path.starts_with('~') {
        return false;
    }
    // ~ 和 ~/ 和 ~\ 是合法的
    if path == "~" || path.starts_with("~/") || path.starts_with("~\\") {
        return false;
    }
    // ~user, ~+, ~-, ~N 是不允许的
    true
}

/// 检查是否包含 glob 字符
fn has_glob_chars(path: &str) -> bool {
    path.contains('*') || path.contains('?') || path.contains('[')
}

/// 获取 glob 基础目录
fn get_glob_base_directory(path: &str) -> String {
    let glob_chars = ['*', '?', '[', '{'];
    let mut first_glob = path.len();
    for gc in &glob_chars {
        if let Some(idx) = path.find(*gc)
            && idx < first_glob
        {
            first_glob = idx;
        }
    }

    if first_glob == 0 {
        return ".".to_string();
    }

    let before_glob = &path[..first_glob];
    // 找最后一个目录分隔符
    if let Some(last_sep) = before_glob.rfind(['/', '\\']) {
        before_glob[..last_sep].to_string()
    } else {
        ".".to_string()
    }
}

/// 剥离引号
fn strip_surrounding_quotes(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.len() >= 2 {
        let first = trimmed.as_bytes()[0];
        let last = trimmed.as_bytes()[trimmed.len() - 1];
        if (first == b'\'' && last == b'\'') || (first == b'"' && last == b'"') {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

/// 解析路径（相对 → 绝对）
fn resolve_path(path: &str, cwd: &str) -> String {
    if path.starts_with('/') || (path.len() >= 2 && path.as_bytes()[1] == b':') {
        // 已是绝对路径
        normalize_path(path)
    } else {
        // 相对路径
        let base = normalize_path(cwd);
        normalize_path(&format!("{base}/{path}"))
    }
}

/// 规范化路径（折叠 `.` 和 `..` 段，防止路径遍历绕过）
fn normalize_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let is_absolute = normalized.starts_with('/');

    let mut components: Vec<&str> = Vec::new();
    for segment in normalized.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                if is_absolute {
                    // 绝对路径：直接弹出，不允许越过根
                    components.pop();
                } else if components.last().is_none_or(|s| *s == "..") {
                    components.push("..");
                } else {
                    components.pop();
                }
            }
            other => components.push(other),
        }
    }

    if is_absolute {
        let joined = components.join("/");
        if joined.is_empty() {
            "/".to_string()
        } else {
            format!("/{joined}")
        }
    } else {
        let joined = components.join("/");
        if joined.is_empty() {
            ".".to_string()
        } else {
            joined
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_tilde() {
        assert_eq!(expand_tilde("/home/user"), "/home/user");
        assert_eq!(expand_tilde("relative/path"), "relative/path");
    }

    #[test]
    fn test_is_dangerous_removal_path() {
        assert!(is_dangerous_removal_path("/"));
        assert!(is_dangerous_removal_path("/*"));
        assert!(is_dangerous_removal_path("~"));
        assert!(is_dangerous_removal_path("/usr"));
        assert!(is_dangerous_removal_path("/etc"));
        assert!(is_dangerous_removal_path("/home"));
        assert!(is_dangerous_removal_path("C:"));
        assert!(is_dangerous_removal_path("C:\\"));
        assert!(is_dangerous_removal_path("C:/"));
        assert!(!is_dangerous_removal_path("/home/user/project"));
        assert!(!is_dangerous_removal_path("/usr/local/bin"));
        assert!(!is_dangerous_removal_path("C:\\Users\\test"));
    }

    #[test]
    fn test_is_path_in_working_directory() {
        assert!(is_path_in_working_directory(
            "/home/user/project",
            "/home/user/project"
        ));
        assert!(is_path_in_working_directory(
            "/home/user/project/src",
            "/home/user/project"
        ));
        assert!(!is_path_in_working_directory(
            "/home/user/other",
            "/home/user/project"
        ));
        assert!(!is_path_in_working_directory(
            "/etc/passwd",
            "/home/user/project"
        ));
    }

    #[test]
    fn test_has_shell_expansion() {
        assert!(has_shell_expansion("$HOME/file"));
        assert!(has_shell_expansion("%USERPROFILE%\\file"));
        assert!(has_shell_expansion("=ls"));
        assert!(!has_shell_expansion("/home/user/file"));
    }

    #[test]
    fn test_is_unc_path() {
        assert!(is_unc_path("\\\\server\\share"));
        assert!(is_unc_path("//server/share"));
        assert!(!is_unc_path("/home/user"));
    }

    #[test]
    fn test_validate_path_basic_read() {
        let result = validate_path_basic("/etc/passwd", "/home/user", FileOperationType::Read);
        assert!(result.allowed);
    }

    #[test]
    fn test_validate_path_basic_write_outside_cwd() {
        let result = validate_path_basic(
            "/etc/passwd",
            "/home/user/project",
            FileOperationType::Write,
        );
        assert!(!result.allowed);
    }

    #[test]
    fn test_validate_path_basic_write_inside_cwd() {
        let result = validate_path_basic(
            "/home/user/project/file.txt",
            "/home/user/project",
            FileOperationType::Write,
        );
        assert!(result.allowed);
    }

    #[test]
    fn test_validate_path_basic_shell_expansion() {
        let result = validate_path_basic("$HOME/file", "/home/user", FileOperationType::Read);
        assert!(!result.allowed);
    }

    #[test]
    fn test_normalize_path() {
        assert_eq!(normalize_path("C:\\Users\\test\\"), "C:/Users/test");
        assert_eq!(normalize_path("/home/user/"), "/home/user");
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn test_strip_surrounding_quotes() {
        assert_eq!(strip_surrounding_quotes("'path'"), "path");
        assert_eq!(strip_surrounding_quotes("\"path\""), "path");
        assert_eq!(strip_surrounding_quotes("path"), "path");
    }

    #[test]
    fn test_unc_path_blocked() {
        let result = validate_path("\\\\evil\\share", "/home/user", FileOperationType::Read);
        assert!(!result.allowed);
    }

    #[test]
    fn test_tilde_variant_blocked() {
        let result = validate_path("~user/file", "/home/user", FileOperationType::Read);
        assert!(!result.allowed);

        let result = validate_path("~+/file", "/home/user", FileOperationType::Read);
        assert!(!result.allowed);
    }

    #[test]
    fn test_glob_write_blocked() {
        let result = validate_path("/tmp/*.txt", "/tmp", FileOperationType::Write);
        assert!(!result.allowed);
    }

    #[test]
    fn test_glob_read_allowed() {
        let result = validate_path(
            "/home/user/project/*.rs",
            "/home/user/project",
            FileOperationType::Read,
        );
        assert!(result.allowed);
    }

    #[test]
    fn test_dangerous_file_write_blocked() {
        let result = is_path_allowed(
            "/.gitconfig",
            "/home/user",
            FileOperationType::Write,
            &[],
            &[],
        );
        assert!(!result.allowed);
    }

    #[test]
    fn test_get_glob_base_directory() {
        assert_eq!(get_glob_base_directory("/home/user/*.rs"), "/home/user");
        assert_eq!(get_glob_base_directory("*.rs"), ".");
        assert_eq!(get_glob_base_directory("/home/*/file"), "/home");
    }

    #[cfg(unix)]
    #[test]
    fn write_through_workspace_symlink_to_outside_is_blocked() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, workspace.join("escape")).unwrap();

        // The final file does not exist: the nearest existing parent must
        // still be resolved through the symlink to the outside directory.
        let result = validate_path(
            "escape/new/deep/file.txt",
            workspace.to_str().unwrap(),
            FileOperationType::Create,
        );
        assert!(
            !result.allowed,
            "outside symlink target must require approval"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_through_workspace_symlink_to_sensitive_config_is_blocked() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let config = tmp.path().join(".crabcode");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        symlink(&config, workspace.join("policy-link")).unwrap();

        let result = validate_path(
            "policy-link/settings.json",
            workspace.to_str().unwrap(),
            FileOperationType::Write,
        );
        assert!(
            !result.allowed,
            "symlink alias must not hide protected CrabCode configuration"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_resolution_allows_inside_target_and_fails_closed_on_dangling_link() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let inside = workspace.join("generated");
        std::fs::create_dir_all(&inside).unwrap();
        symlink(&inside, workspace.join("inside-link")).unwrap();
        symlink(
            tmp.path().join("missing-target"),
            workspace.join("dangling"),
        )
        .unwrap();

        let safe = validate_path(
            "inside-link/new.txt",
            workspace.to_str().unwrap(),
            FileOperationType::Create,
        );
        assert!(
            safe.allowed,
            "symlink that remains inside cwd should be allowed"
        );

        let dangling = validate_path(
            "dangling/new.txt",
            workspace.to_str().unwrap(),
            FileOperationType::Create,
        );
        assert!(
            !dangling.allowed,
            "unresolvable symlink chain must fail closed"
        );
    }

    // ─── A17 / P2-2 #16: anchored .crabcode + safety-before-allow regression ───

    /// Save+restore an env var across a test body (so #[serial] tests don't leak).
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, val: &str) -> Self {
            let prev = std::env::var(key).ok();
            #[allow(unsafe_code)]
            unsafe {
                std::env::set_var(key, val);
            }
            Self { key, prev }
        }
        fn clear(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            #[allow(unsafe_code)]
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            #[allow(unsafe_code)]
            unsafe {
                match &self.prev {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    /// Pin the resolved config-home to a tempdir via CRABCODE_HOME and clear the
    /// higher-priority overrides, so `resolve_state_dir()` == `<tmp>/.crabcode`.
    /// Returns (tempdir, guards) — keep guards alive for the test body.
    fn pin_config_home(tmp: &tempfile::TempDir) -> Vec<EnvGuard> {
        let home = tmp.path().to_str().unwrap().to_string();
        vec![
            EnvGuard::clear("CRABCODE_CONFIG_DIR"),
            EnvGuard::clear("CRABCODE_STATE_DIR"),
            EnvGuard::clear("CRABCODE_CONFIG_PATH"),
            EnvGuard::clear("CRABCODE_PROFILE"),
            EnvGuard::set("CRABCODE_HOME", &home),
        ]
    }

    /// Pin the full state-directory override to a name that deliberately does
    /// not contain a `.crabcode` component.
    fn pin_custom_config_dir(path: &std::path::Path) -> Vec<EnvGuard> {
        let state_dir = path.to_str().unwrap().to_string();
        vec![
            EnvGuard::clear("CRABCODE_STATE_DIR"),
            EnvGuard::clear("CRABCODE_CONFIG_PATH"),
            EnvGuard::clear("CRABCODE_PROFILE"),
            EnvGuard::clear("CRABCODE_HOME"),
            EnvGuard::set("CRABCODE_CONFIG_DIR", &state_dir),
        ]
    }

    fn pin_custom_state_dir(path: &std::path::Path) -> Vec<EnvGuard> {
        let state_dir = path.to_str().unwrap().to_string();
        vec![
            EnvGuard::clear("CRABCODE_CONFIG_DIR"),
            EnvGuard::clear("CRABCODE_HOME"),
            EnvGuard::clear("CRABCODE_PROFILE"),
            EnvGuard::set("CRABCODE_STATE_DIR", &state_dir),
        ]
    }

    #[test]
    #[serial_test::serial]
    fn test_a17_legit_internal_plan_file_still_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let _guards = pin_config_home(&tmp);

        // <config-home>/.crabcode/plans/foo.md must still be auto-allowed for write.
        let path = format!("{}/.crabcode/plans/foo.md", tmp.path().display());
        let result = is_path_allowed(&path, "/", FileOperationType::Write, &[], &[]);
        assert!(
            result.allowed,
            "legit internal plan file under resolved config-home must be allowed: {result:?}"
        );

        // Everything else under config-home is approval-only because it may
        // be loaded, executed, or contain credentials.
        let config_root = format!("{}/.crabcode", tmp.path().display());
        for relative in [
            "",
            "settings.json",
            "settings.local.json",
            "commands/release.md",
            "agents/reviewer.md",
            "skills/reviewer/SKILL.md",
            "plugins/demo/hooks/hooks.json",
            "plugins/demo/.codex-plugin/plugin.json",
            "output-styles/reviewer.md",
            "workflows/release.md",
            "templates/report.md",
            "plugins/known_marketplaces.json",
            "known_marketplaces.json",
            ".credentials.json",
            "cache/artifact.bin",
        ] {
            let path = if relative.is_empty() {
                config_root.clone()
            } else {
                format!("{config_root}/{relative}")
            };
            assert!(
                !is_path_allowed(&path, &config_root, FileOperationType::Write, &[], &[]).allowed,
                "non-allowlisted config-home write must require approval: {path}"
            );
        }
        let scratch = format!("{}/.crabcode/scratchpad/notes.txt", tmp.path().display());
        assert!(is_path_allowed(&scratch, "/", FileOperationType::Write, &[], &[]).allowed);
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn symlink_to_resolved_config_root_is_blocked_after_canonicalization() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let _guards = pin_config_home(&tmp);
        let workspace = tmp.path().join("workspace");
        let config_root = tmp.path().join(".crabcode");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&config_root).unwrap();
        symlink(&config_root, workspace.join("hidden-policy")).unwrap();

        let result = validate_path(
            "hidden-policy",
            workspace.to_str().unwrap(),
            FileOperationType::Write,
        );
        assert!(
            !result.allowed,
            "a workspace alias must not make the complete configuration root auto-editable"
        );
    }

    #[test]
    #[serial_test::serial]
    fn custom_config_dir_protects_relative_policy_identity_without_crabcode_segment() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("custom-state-store");
        std::fs::create_dir_all(&state_dir).unwrap();
        let _guards = pin_custom_config_dir(&state_dir);
        let resolved_state_dir = acosmi_config::paths::resolve_state_dir();
        assert_eq!(
            resolved_state_dir,
            std::fs::canonicalize(&state_dir).unwrap()
        );
        assert!(is_under_resolved_config_home(
            resolved_state_dir.join("plans/plan.md").to_str().unwrap()
        ));

        for relative in [
            "",
            "settings.json",
            "settings.local.json",
            "settings.team.json",
            "commands/release.md",
            "agents/reviewer.md",
            "skills/reviewer/SKILL.md",
            "plugins/demo/hooks/hooks.json",
            "plugins/demo/.codex-plugin/plugin.json",
            "output-styles/reviewer.md",
            "workflows/release.md",
            "templates/report.md",
            "plugins/known_marketplaces.json",
            "known_marketplaces.json",
            ".credentials.json",
            "cache/artifact.bin",
        ] {
            let path = if relative.is_empty() {
                resolved_state_dir.clone()
            } else {
                resolved_state_dir.join(relative)
            };
            let result = is_path_allowed(
                path.to_str().unwrap(),
                resolved_state_dir.to_str().unwrap(),
                FileOperationType::Write,
                &[],
                &[],
            );
            assert!(
                !result.allowed,
                "custom config policy identity must require approval: {} => {result:?}",
                path.display()
            );
        }

        for relative in ["plans/plan.md", "scratchpad/notes.txt"] {
            let path = resolved_state_dir.join(relative);
            let result = is_path_allowed(
                path.to_str().unwrap(),
                resolved_state_dir.to_str().unwrap(),
                FileOperationType::Write,
                &[],
                &[],
            );
            assert!(
                result.allowed,
                "ordinary internal state must remain auto-editable: {} => {result:?}",
                path.display()
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn custom_state_dir_uses_same_identity_protection_and_preserves_plans() {
        let tmp = tempfile::tempdir().unwrap();
        let state_dir = tmp.path().join("state-override-without-brand-name");
        std::fs::create_dir_all(&state_dir).unwrap();
        let _guards = pin_custom_state_dir(&state_dir);
        let resolved = acosmi_config::paths::resolve_state_dir();

        for path in [resolved.clone(), resolved.join("settings.json")] {
            let result = is_path_allowed(
                path.to_str().unwrap(),
                resolved.to_str().unwrap(),
                FileOperationType::Write,
                &[],
                &[],
            );
            assert!(
                !result.allowed,
                "CRABCODE_STATE_DIR policy identity must require approval: {} => {result:?}",
                path.display()
            );
        }

        let plan = resolved.join("plans/plan.md");
        let result = is_path_allowed(
            plan.to_str().unwrap(),
            resolved.to_str().unwrap(),
            FileOperationType::Write,
            &[],
            &[],
        );
        assert!(
            result.allowed,
            "CRABCODE_STATE_DIR plans must remain auto-editable: {} => {result:?}",
            plan.display()
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn custom_config_dir_symlink_uses_canonical_anchor_and_preserves_state_carveouts() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let real_state_dir = tmp.path().join("opaque-policy-store");
        let configured_alias = tmp.path().join("configured-state-alias");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&real_state_dir).unwrap();
        symlink(&real_state_dir, &configured_alias).unwrap();
        symlink(&real_state_dir, workspace.join("hidden-config")).unwrap();
        let _guards = pin_custom_config_dir(&configured_alias);

        for relative in [
            "hidden-config",
            "hidden-config/settings.preview.json",
            "hidden-config/commands/release.md",
            "hidden-config/agents/reviewer.md",
            "hidden-config/skills/reviewer/SKILL.md",
            "hidden-config/plugins/demo/hooks/hooks.json",
            "hidden-config/plugins/demo/.codex-plugin/plugin.json",
            "hidden-config/output-styles/reviewer.md",
            "hidden-config/workflows/release.md",
            "hidden-config/templates/report.md",
            "hidden-config/plugins/known_marketplaces.json",
            "hidden-config/.credentials.json",
            "hidden-config/cache/artifact.bin",
        ] {
            let result = validate_path(
                relative,
                workspace.to_str().unwrap(),
                FileOperationType::Write,
            );
            assert!(
                !result.allowed,
                "canonical config identity must survive a workspace symlink: {relative} => {result:?}"
            );
        }

        for relative in [
            "hidden-config/plans/new.md",
            "hidden-config/scratchpad/notes.txt",
        ] {
            let result = validate_path(
                relative,
                workspace.to_str().unwrap(),
                FileOperationType::Create,
            );
            assert!(
                result.allowed,
                "non-policy internal state must retain its carve-out through canonicalization: {relative} => {result:?}"
            );
        }
    }

    #[test]
    fn test_crabcode_config_protection_is_case_and_traversal_resistant() {
        for path in [
            "/work/.CrAbCoDe/Settings.JSON",
            "/work/tmp/../.crabcode/settings.local.json",
            "/work/.crabcode/commands/release.md",
            "/work/.crabcode/skills/reviewer/SKILL.md",
            "/work/.GIT/config",
            "/work/.CrAbCoDe.JSON",
        ] {
            let result = validate_path(path, "/work", FileOperationType::Write);
            assert!(!result.allowed, "protected config path must ask: {path}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_a17_dangerous_file_in_unanchored_crabcode_now_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let _guards = pin_config_home(&tmp);

        // /tmp/x/.crabcode/.bashrc — .crabcode segment but NOT under config-home.
        // Previously auto-allowed by the unanchored substring + pre-safety Layer 2.
        let result = is_path_allowed(
            "/tmp/x/.crabcode/.bashrc",
            "/",
            FileOperationType::Write,
            &[],
            &[],
        );
        assert!(
            !result.allowed,
            "dangerous file under any unanchored .crabcode path must be denied: {result:?}"
        );

        // Traversal form must not slip through normalization either:
        // /etc/evil/.crabcode/../.bashrc normalizes to /etc/evil/.bashrc → dangerous file.
        let traversal = is_path_allowed(
            "/etc/evil/.crabcode/../.bashrc",
            "/",
            FileOperationType::Write,
            &[],
            &[],
        );
        assert!(
            !traversal.allowed,
            "traversal-via-.crabcode must be denied: {traversal:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_a17_dangerous_file_inside_real_config_home_now_denied() {
        let tmp = tempfile::tempdir().unwrap();
        let _guards = pin_config_home(&tmp);

        // Even INSIDE the real config-home, a dangerous *file* must be blocked
        // (safety check runs before the .crabcode allow). .mcp.json / .crabcode.json
        // are in dangerous_files and are not written through the fs/* validator by
        // any internal code path (config writes use the dedicated config writer).
        for fname in [".bashrc", ".mcp.json", ".crabcode.json", ".zshrc"] {
            let path = format!("{}/.crabcode/{fname}", tmp.path().display());
            let result = is_path_allowed(&path, "/", FileOperationType::Write, &[], &[]);
            assert!(
                !result.allowed,
                "dangerous file {fname} inside real config-home must be denied: {result:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_a17_unanchored_crabcode_nondangerous_file_not_auto_allowed() {
        // A non-dangerous file under an unanchored .crabcode path must NOT be
        // auto-allowed by Layer 2 (it falls through to the default write rules,
        // which deny outside cwd). This proves the anchor — not just the dangerous
        // filter — is doing the work.
        let tmp = tempfile::tempdir().unwrap();
        let _guards = pin_config_home(&tmp);

        let result = is_path_allowed(
            "/tmp/x/.crabcode/notes.md",
            "/home/user/project",
            FileOperationType::Write,
            &[],
            &[],
        );
        assert!(
            !result.allowed,
            "non-dangerous file under unanchored .crabcode must not be auto-allowed: {result:?}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_a17_other_dangerous_dirs_still_blocked_inside_config_home() {
        // The .crabcode carve-out is precise: other dangerous dirs (.git/.vscode/
        // .idea) nested under config-home are still blocked.
        let tmp = tempfile::tempdir().unwrap();
        let _guards = pin_config_home(&tmp);

        let path = format!("{}/.crabcode/.git/config", tmp.path().display());
        let result = is_path_allowed(&path, "/", FileOperationType::Write, &[], &[]);
        assert!(
            !result.allowed,
            ".git under config-home must still be blocked: {result:?}"
        );
    }
}

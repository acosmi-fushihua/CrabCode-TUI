//! fs 模式表 → 内核能吃的路径规则（W-SANDBOX-ENFORCED-DEADCODE PR-2）。
//!
//! [`crate::exec_config`] 把 TS 派生的四表**逐字**带到了 Rust；本模块回答下一个
//! 问题：**这些模式串在内核层面到底能不能被强制？**
//!
//! ## 为什么归一化是平台无关的纯函数
//!
//! 内核施加层三平台各不相同（landlock ruleset / SBPL profile / ACL+token），
//! 但「`.` 是哪个目录」「`~/x` 展开成什么」「`/foo/**` 的强制目标是哪棵子树」
//! 这三件事**与平台无关**。把它们放进各自的 cfg 分支意味着同一份语义写三遍、
//! 且只有一份能在本机被测到。这里做成纯函数 ⇒ 三平台共用一份判据，且
//! **Windows 本机也能把 Unix 的归一化逻辑跑绿**。
//!
//! ## 铁律：不可表达的规则**必须留下名字**
//!
//! 一条模式串若无法翻译成内核规则，本模块把它放进
//! [`ResolvedFsRules::unresolvable`]，**绝不**降级近似：
//!
//! - **allow 侧近似 = 提权**。`/foo/*.pem` 若近似成「放行 /foo 整棵树」，用户
//!   要的是一个文件、拿到的是一个目录。
//! - **deny 侧近似 = 假强制**。近似成更小的范围就等于答应了却没做到，而这正是
//!   本立项要消灭的形态（SoT §2.3「绝不假装强制」）。
//!
//! 唯一被认可的「近似」是尾部 `/**` 与 `/*`：它们的语义**就是**子树，而
//! `subpath` / `PathBeneath` 表达的也**就是**子树，两者逐字等价，不是近似。
//!
//! ## deny 的可强制性因平台而异（本模块只给判据，不下结论）
//!
//! [`shadowing_grant`] 回答的是一个纯几何问题：**这条 deny 与某条 allow 是否
//! 重叠**。两个平台对同一个答案的处置完全不同，故判据在这里、处置在各自的
//! 施加层：
//!
//! - **macOS**：SBPL 支持 deny 规则，重叠的 deny 靠「后写的规则赢」兑现 ⇒
//!   施加层把重叠项拿去**运行期实证**（`sandbox_check`），证不出来就失败。
//! - **Linux**：Landlock 是**纯 allow-grant 模型**，无法在已放行的子树里挖洞
//!   （证据见 `linux::landlock` 的 `UNENFORCEABLE(linux)` 锚点）⇒ 施加层把
//!   重叠项如实报成不可强制。不重叠的 deny 由 Landlock 的默认拒绝天然兑现。

use std::path::{Component, Path, PathBuf};

use crate::exec_config::FsRules;

/// 四表的表名。诊断文本里逐字出现，与 wire 字段名对齐。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsRuleKind {
    AllowRead,
    AllowWrite,
    DenyRead,
    DenyWrite,
}

impl FsRuleKind {
    /// wire 字段名（`filesystem.<name>`）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::AllowRead => "filesystem.allowRead",
            Self::AllowWrite => "filesystem.allowWrite",
            Self::DenyRead => "filesystem.denyRead",
            Self::DenyWrite => "filesystem.denyWrite",
        }
    }
}

/// 一条已翻译成绝对路径子树的规则。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFsRule {
    pub kind: FsRuleKind,
    /// 强制目标：这棵子树的根（绝对、无 `.`/`..`）。
    pub path: PathBuf,
    /// 原始模式串。诊断必须能指回用户写的那一行。
    pub source: String,
}

/// 一条翻译不了的规则。**它存在的全部意义就是被报出来。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvableFsRule {
    pub kind: FsRuleKind,
    pub source: String,
    pub why: &'static str,
}

/// 模式串含 `subpath` 表达不了的 glob 元字符。
pub const WHY_GLOB_NOT_EXPRESSIBLE: &str = "glob pattern is not expressible as a path subtree";
/// 模式串把整棵文件系统当成目标（裸 `*` / `**` / 展开后是根）。
pub const WHY_WHOLE_FILESYSTEM: &str = "pattern targets the entire filesystem";
/// 模式串含 `..`。
pub const WHY_PARENT_TRAVERSAL: &str = "pattern contains a `..` component";
/// 模式串以 `~` 开头但本进程解析不出 home 目录。
pub const WHY_NO_HOME: &str = "pattern starts with `~` but no home directory could be resolved";

/// 四表的翻译结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedFsRules {
    pub allow_read: Vec<ResolvedFsRule>,
    pub allow_write: Vec<ResolvedFsRule>,
    pub deny_read: Vec<ResolvedFsRule>,
    pub deny_write: Vec<ResolvedFsRule>,
    /// 翻译不了的条目。施加层**必须**把它们报出去。
    pub unresolvable: Vec<UnresolvableFsRule>,
}

impl ResolvedFsRules {
    /// 全部 allow 侧目标（读 + 写）。deny 的重叠判定以它为基准。
    #[must_use]
    pub fn allow_roots(&self) -> Vec<PathBuf> {
        self.allow_read
            .iter()
            .chain(self.allow_write.iter())
            .map(|r| r.path.clone())
            .collect()
    }
}

/// 翻译四表。`cwd` 解析 `.` 与相对路径，`home` 解析 `~`。
///
/// 顺序保持输入顺序——规则顺序对 SBPL 是语义（后写的赢），对诊断是可读性。
#[must_use]
pub fn resolve_fs_rules(rules: &FsRules, cwd: &Path, home: Option<&Path>) -> ResolvedFsRules {
    let mut out = ResolvedFsRules::default();
    for (kind, patterns) in [
        (FsRuleKind::AllowRead, &rules.allow_read),
        (FsRuleKind::AllowWrite, &rules.allow_write),
        (FsRuleKind::DenyRead, &rules.deny_read),
        (FsRuleKind::DenyWrite, &rules.deny_write),
    ] {
        for pattern in patterns {
            match normalize_pattern(pattern, cwd, home) {
                Ok(path) => {
                    let rule = ResolvedFsRule {
                        kind,
                        path,
                        source: pattern.clone(),
                    };
                    match kind {
                        FsRuleKind::AllowRead => out.allow_read.push(rule),
                        FsRuleKind::AllowWrite => out.allow_write.push(rule),
                        FsRuleKind::DenyRead => out.deny_read.push(rule),
                        FsRuleKind::DenyWrite => out.deny_write.push(rule),
                    }
                }
                Err(why) => out.unresolvable.push(UnresolvableFsRule {
                    kind,
                    source: pattern.clone(),
                    why,
                }),
            }
        }
    }
    out
}

/// 把一条模式串翻成绝对子树根。
///
/// 步骤（顺序有意义）：尾部 glob 剥离 → `~` 展开 → 相对路径按 `cwd` 锚定 →
/// 词法归一（吃掉 `.`，拒绝 `..`）→ 残留 glob 元字符即判不可表达。
///
/// **词法归一而非 `canonicalize`** 是刻意的：规则里的路径经常还不存在
/// （典型：为一个尚未创建的 `settings.json` 预先 denyWrite），`canonicalize`
/// 会因 ENOENT 把这条规则整个弄丢——而丢掉一条 deny 就是把用户的防线悄悄
/// 拆掉。符号链接的解析交给内核层（landlock 的 `PathFd` / SBPL 的
/// `canonicalize`，各自按自己的语义处理）。
pub fn normalize_pattern(
    pattern: &str,
    cwd: &Path,
    home: Option<&Path>,
) -> Result<PathBuf, &'static str> {
    let stripped = strip_trailing_glob(pattern);
    if stripped.is_empty() {
        // 裸 `*` / `**` —— 剥完什么都不剩，目标是整棵文件系统。
        return Err(WHY_WHOLE_FILESYSTEM);
    }

    let expanded: PathBuf = if stripped == "~" {
        home.ok_or(WHY_NO_HOME)?.to_path_buf()
    } else if let Some(rest) = strip_home_prefix(stripped) {
        home.ok_or(WHY_NO_HOME)?.join(rest)
    } else if is_absolute_pattern(stripped) {
        PathBuf::from(stripped)
    } else {
        cwd.join(stripped)
    };

    let normalized = lexically_normalize(&expanded)?;

    // glob 元字符必须在**展开之后**检查：`~/.aws/**` 剥完尾巴是 `~/.aws`，
    // 干净；而 `/foo/*.pem` 剥不掉（`*.pem` 不是尾部 glob 后缀），残留即拒绝。
    if normalized
        .as_os_str()
        .to_string_lossy()
        .contains(['*', '?', '['])
    {
        return Err(WHY_GLOB_NOT_EXPRESSIBLE);
    }

    if normalized.parent().is_none() {
        // 展开后就是根（`/` 或 `C:\`）——放行它等于放行一切，拒绝它等于禁止一切。
        return Err(WHY_WHOLE_FILESYSTEM);
    }

    Ok(normalized)
}

/// 剥掉尾部的子树 glob 后缀。`/a/**` `/a/*` `/a/**/*` 的语义都是「/a 这棵子树」，
/// 与 `subpath` / `PathBeneath` 逐字等价，不是近似。
fn strip_trailing_glob(pattern: &str) -> &str {
    let mut s = pattern;
    loop {
        let trimmed = s
            .strip_suffix("/**")
            .or_else(|| s.strip_suffix("/*"))
            .or_else(|| s.strip_suffix("\\**"))
            .or_else(|| s.strip_suffix("\\*"))
            .unwrap_or(s);
        if trimmed == s {
            // 裸 `*` / `**`（没有前导分隔符）单独兜住。
            if s == "*" || s == "**" {
                return "";
            }
            return s;
        }
        s = trimmed;
    }
}

/// `~/x` → `Some("x")`；`~x`（如 `~alice/y`）不是我们支持的形态，返回 `None`
/// 让它按普通相对路径走（随后大概率因 `~` 不是合法目录名而失败得很明显）。
fn strip_home_prefix(pattern: &str) -> Option<&str> {
    pattern
        .strip_prefix("~/")
        .or_else(|| pattern.strip_prefix("~\\"))
}

/// POSIX 绝对（前导 `/`）或平台绝对（Windows 盘符）。
///
/// 显式认前导 `/` 而不是只问 `Path::is_absolute()`：本模块要在 Windows 上被
/// 单测，而 Windows 的 `is_absolute()` 对 `/tmp/x` 返 false。规则串来自 TS，
/// Unix 上恒是 POSIX 写法。
fn is_absolute_pattern(pattern: &str) -> bool {
    pattern.starts_with('/') || Path::new(pattern).is_absolute()
}

/// 词法归一：吃掉 `.`，拒绝 `..`，保留前缀与根。
fn lexically_normalize(path: &Path) -> Result<PathBuf, &'static str> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => return Err(WHY_PARENT_TRAVERSAL),
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(WHY_WHOLE_FILESYSTEM);
    }
    Ok(out)
}

/// 从 env 解析 home 目录，用于展开 `~`。
///
/// 刻意读 env 而不是 `getpwuid`：helper 继承的正是宿主进程的 env，而 TS 侧
/// 派生规则时用的也是同一套（`expandPath` 走 `os.homedir()` → `$HOME`）。
/// 两边取值口径必须一致，否则同一条 `~/x` 在两侧指向两个目录。
#[must_use]
pub fn home_dir_from_env() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// 这条 deny 是否与某条 allow 重叠？重叠即「内核默认拒绝兑现不了它」。
///
/// 两种重叠都算，因为两种都让 deny 至少有一部分落在放行区里：
/// - grant 是 deny 的祖先或同一路径（deny 整个在放行区内）
/// - deny 是 grant 的祖先（deny 的一部分被 grant 挖了洞）
///
/// 返回第一条重叠的 grant，用于诊断文本点名。
#[must_use]
pub fn shadowing_grant<'a>(deny: &Path, grants: &'a [PathBuf]) -> Option<&'a Path> {
    grants
        .iter()
        .find(|grant| is_ancestor_or_same(grant, deny) || is_ancestor_or_same(deny, grant))
        .map(PathBuf::as_path)
}

/// `ancestor` 是否是 `descendant` 的祖先或同一路径（逐 component 比较，
/// **不做字符串前缀比较**——`/a/bc` 不是 `/a/b` 的后代）。
fn is_ancestor_or_same(ancestor: &Path, descendant: &Path) -> bool {
    let mut a = ancestor.components();
    let mut d = descendant.components();
    loop {
        match (a.next(), d.next()) {
            (None, _) => return true,
            (Some(_), None) => return false,
            (Some(x), Some(y)) if x == y => {}
            (Some(_), Some(_)) => return false,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cwd() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from("C:\\ws")
        } else {
            PathBuf::from("/ws")
        }
    }

    fn home() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from("C:\\Users\\u")
        } else {
            PathBuf::from("/home/u")
        }
    }

    fn norm(pattern: &str) -> Result<PathBuf, &'static str> {
        normalize_pattern(pattern, &cwd(), Some(&home()))
    }

    #[test]
    fn dot_anchors_to_the_command_cwd() {
        // `.` 是 allowWrite 的第一条（sandbox-adapter.ts::convertToSandboxRuntimeConfig），
        // 也就是整个沙箱最承重的一条规则。锚错了 = 命令连自己的工作目录都写不了。
        assert_eq!(norm(".").unwrap(), cwd());
        assert_eq!(norm("./sub").unwrap(), cwd().join("sub"));
        assert_eq!(norm("sub/deep").unwrap(), cwd().join("sub").join("deep"));
    }

    #[test]
    fn tilde_expands_to_home() {
        assert_eq!(norm("~").unwrap(), home());
        assert_eq!(norm("~/.aws").unwrap(), home().join(".aws"));
    }

    #[test]
    fn tilde_without_a_home_is_reported_not_guessed() {
        assert_eq!(
            normalize_pattern("~/.aws", &cwd(), None).unwrap_err(),
            WHY_NO_HOME
        );
    }

    #[test]
    fn trailing_subtree_globs_are_exact_not_approximate() {
        // `/a/**` 的语义就是「/a 这棵子树」，与 subpath/PathBeneath 逐字等价。
        for pattern in ["/a/b/**", "/a/b/*", "/a/b/**/*"] {
            assert_eq!(
                norm(pattern).unwrap(),
                PathBuf::from("/a/b"),
                "pattern {pattern}"
            );
        }
        assert_eq!(norm("~/.aws/**").unwrap(), home().join(".aws"));
    }

    #[test]
    fn interior_globs_are_reported_never_widened() {
        // 近似成 `/a` 就是把一个文件的授权扩成一整棵树。
        for pattern in ["/a/*.pem", "/a/*/b", "/a/?.txt", "/a/[abc]"] {
            assert_eq!(
                norm(pattern).unwrap_err(),
                WHY_GLOB_NOT_EXPRESSIBLE,
                "pattern {pattern}"
            );
        }
    }

    #[test]
    fn whole_filesystem_targets_are_refused() {
        for pattern in ["*", "**", "/**", "/*", "/"] {
            assert!(norm(pattern).is_err(), "pattern {pattern} must be refused");
        }
    }

    #[test]
    fn parent_traversal_is_refused() {
        assert_eq!(norm("/a/../b").unwrap_err(), WHY_PARENT_TRAVERSAL);
        assert_eq!(norm("../escape").unwrap_err(), WHY_PARENT_TRAVERSAL);
    }

    #[test]
    fn absolute_posix_patterns_survive_on_every_host() {
        // 规则串来自 TS，Unix 上恒是 POSIX 写法；Windows 本机跑这条单测时
        // `Path::is_absolute()` 会说 false —— 所以判据里显式认前导 `/`。
        assert_eq!(norm("/etc/hosts").unwrap(), PathBuf::from("/etc/hosts"));
    }

    fn rules(
        allow_read: &[&str],
        allow_write: &[&str],
        deny_read: &[&str],
        deny_write: &[&str],
    ) -> FsRules {
        let own = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        FsRules {
            allow_read: own(allow_read),
            allow_write: own(allow_write),
            deny_read: own(deny_read),
            deny_write: own(deny_write),
        }
    }

    #[test]
    fn resolve_keeps_tables_apart_and_preserves_order() {
        let resolved = resolve_fs_rules(
            &rules(
                &["/usr/lib"],
                &[".", "/tmp/x"],
                &["~/.aws/**"],
                &["/etc/hosts"],
            ),
            &cwd(),
            Some(&home()),
        );
        assert_eq!(resolved.allow_read.len(), 1);
        assert_eq!(resolved.allow_write.len(), 2);
        assert_eq!(resolved.allow_write[0].path, cwd());
        assert_eq!(resolved.allow_write[1].path, PathBuf::from("/tmp/x"));
        assert_eq!(resolved.deny_read[0].path, home().join(".aws"));
        assert_eq!(resolved.deny_write[0].path, PathBuf::from("/etc/hosts"));
        assert!(resolved.unresolvable.is_empty());
    }

    #[test]
    fn unresolvable_entries_keep_their_name_and_table() {
        let resolved =
            resolve_fs_rules(&rules(&[], &["/a/*.pem"], &[], &[]), &cwd(), Some(&home()));
        assert!(resolved.allow_write.is_empty());
        assert_eq!(resolved.unresolvable.len(), 1);
        assert_eq!(resolved.unresolvable[0].kind, FsRuleKind::AllowWrite);
        assert_eq!(resolved.unresolvable[0].source, "/a/*.pem");
        assert_eq!(resolved.unresolvable[0].why, WHY_GLOB_NOT_EXPRESSIBLE);
        assert_eq!(
            FsRuleKind::AllowWrite.wire_name(),
            "filesystem.allowWrite",
            "诊断文本必须能指回 wire 字段名"
        );
    }

    #[test]
    fn deny_inside_an_allow_is_detected_as_overlapping() {
        // 这正是生产里最常见的形态：allowWrite `.` + denyWrite `<cwd>/.crabcode/settings.json`
        // （configAdapters.ts 把 denyWrite 直接叫 `denyWithinAllow`）。
        let grants = vec![cwd()];
        let deny = cwd().join(".crabcode").join("settings.json");
        assert_eq!(shadowing_grant(&deny, &grants), Some(cwd().as_path()));
    }

    #[test]
    fn deny_outside_every_allow_is_not_overlapping() {
        let grants = vec![cwd(), PathBuf::from("/tmp/x")];
        assert!(shadowing_grant(&home().join(".aws"), &grants).is_none());
    }

    #[test]
    fn a_grant_nested_inside_a_deny_also_counts_as_overlapping() {
        // deny 覆盖 /a，但 /a/b 被放行 ⇒ deny 有一部分兑现不了，必须报出来。
        let grants = vec![PathBuf::from("/a/b")];
        assert_eq!(
            shadowing_grant(Path::new("/a"), &grants),
            Some(Path::new("/a/b"))
        );
    }

    #[test]
    fn sibling_prefixes_are_not_ancestors() {
        // 字符串前缀比较会把 /a/bc 判成 /a/b 的后代 —— 那会让一条无关的 deny
        // 被误报成重叠，进而在 macOS 上被拿去做运行期实证并失败。
        let grants = vec![PathBuf::from("/a/b")];
        assert!(shadowing_grant(Path::new("/a/bc"), &grants).is_none());
    }

    #[test]
    fn allow_roots_merges_both_read_and_write_tables() {
        let resolved = resolve_fs_rules(
            &rules(&["/usr/lib"], &["/w"], &[], &[]),
            &cwd(),
            Some(&home()),
        );
        let roots = resolved.allow_roots();
        assert!(roots.contains(&PathBuf::from("/usr/lib")));
        assert!(roots.contains(&PathBuf::from("/w")));
    }
}

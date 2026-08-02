//! F1 daemon/产物代际握手 (2026-06-11) — build-id 比对的共享真源。
//!
//! 背景：cron 等常驻 daemon 一经拉起会跨越 CLI/TUI 进程生命周期；若 launcher
//! 只按 PID 存活复用，本机升级后可能继续运行旧二进制。本模块给 cron daemon、
//! native launcher 与 browser 的诊断字段提供同一 build-id 真源。
//!
//! ## 契约（主控已定，跨组件，不许漂移）
//!
//! - **格式**：`${version}+${git short sha(12)}`；构建时 env `CRABCODE_BUILD_ID`
//!   已设则逐字使用；git 不可用 → `${version}+unknown`。**不带 dirty 后缀**。
//! - **非权威语义**：以 `+unknown` 结尾的 id 视为「总是匹配」——比对任一侧为
//!   unknown → 一致（不换代不拒绝）。保护只在两侧都有权威 id 时生效。
//! - **落盘**：`<run_dir>/<name>.build-id`（紧邻 `<name>.pid`，同生命周期）。
//! - **总开关**：env `CRABCODE_SKIP_BUILD_HANDSHAKE=1` 关闭一切代际握手行为
//!   （回滚阀），所有判定点统一尊重。

use std::path::Path;
use std::path::PathBuf;

use crate::paths;

/// 总开关 env 名（TS 侧同名）。
pub const SKIP_BUILD_HANDSHAKE_ENV: &str = "CRABCODE_SKIP_BUILD_HANDSHAKE";

/// 非权威 build-id 的后缀标记。
pub const UNKNOWN_SUFFIX: &str = "+unknown";

/// 本二进制编译期注入的 build-id（`build.rs` 产出；env 优先 → git → unknown）。
///
/// 本 crate 是 path-dep 共享库：值在**链入它的二进制**编译时确定。workspace
/// 统一 `[workspace.package].version`，故与各二进制自己的
/// `CARGO_PKG_VERSION` 拼出的字符串逐字一致。
#[must_use]
pub fn self_build_id() -> &'static str {
    env!("CRABCODE_BUILD_ID")
}

/// 代际握手是否开启（`CRABCODE_SKIP_BUILD_HANDSHAKE=1` → 关）。
#[must_use]
pub fn handshake_enabled() -> bool {
    !skip_flag_set(std::env::var(SKIP_BUILD_HANDSHAKE_ENV).ok().as_deref())
}

/// 纯函数形态的开关判定（测试不依赖进程全局 env）。
/// 契约：值 == `"1"` 才算显式关闭；空串 / 缺失 / 其他值均视为开启。
#[must_use]
pub fn skip_flag_set(value: Option<&str>) -> bool {
    value == Some("1")
}

/// id 是否权威（非空且不以 `+unknown` 结尾）。
#[must_use]
pub fn is_authoritative(id: &str) -> bool {
    !id.is_empty() && !id.ends_with(UNKNOWN_SUFFIX)
}

/// 比对结果（`evaluate_handshake` 的输出）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandshakeVerdict {
    /// 握手关闭 / 任一侧非权威 / 不等但**自身不比对端新**（只升不降，
    /// 防换代乒乓）→ 不换代，复用现有 daemon。
    Skip,
    /// 双方权威且相等。
    Match,
    /// 自身代际**严格更新**（版本号更高），**或对端记录缺失**（老代
    /// daemon 从未写过 build-id 文件 → 换代一次即自愈，期望行为）。
    Mismatch,
}

/// 核心判定（纯函数，TS 侧 `evaluateBuildHandshake` 逐字镜像同一真值表）：
///
/// | 条件 | 结果 |
/// |---|---|
/// | skip flag 设 1 | `Skip` |
/// | 自身 id 非权威（`+unknown`） | `Skip` |
/// | 对端记录缺失（`None`） | `Mismatch`（老代 daemon，换代自愈） |
/// | 对端记录非权威 | `Skip` |
/// | 相等 | `Match` |
/// | 不等且自身版本更高 | `Mismatch`（升级换代） |
/// | 不等且自身版本不更高 | `Skip`（**只升不降**，见下） |
///
/// ## 只升不降（2026-06-12 真机取证修订）
///
/// 旧真值表「不等 → Mismatch」是**对称**的：升级后旧版 TUI 窗口仍开着时，
/// 新窗换代到新版 daemon → 旧窗下一次 ensure 又把 daemon **降级**回旧版 →
/// 新窗再升 → 乒乓循环。修订为只有自身
/// 版本**严格更高**才换代；版本相同 sha 不同（force-move tag 重建等）视为
/// 不可排序，不换代（代价：同版本重建不自动换代，重启 daemon 手动消解；
/// 收益：根除乒乓）。版本段解析失败同样不换代（fail-safe 宁可旧不可无）。
#[must_use]
pub fn evaluate_handshake(self_id: &str, recorded: Option<&str>, skip: bool) -> HandshakeVerdict {
    if skip {
        return HandshakeVerdict::Skip;
    }
    if !is_authoritative(self_id) {
        return HandshakeVerdict::Skip;
    }
    match recorded {
        None => HandshakeVerdict::Mismatch,
        Some(recorded) if !is_authoritative(recorded) => HandshakeVerdict::Skip,
        Some(recorded) if recorded == self_id => HandshakeVerdict::Match,
        Some(recorded) => {
            if build_id_version_newer(self_id, recorded) {
                HandshakeVerdict::Mismatch
            } else {
                HandshakeVerdict::Skip
            }
        }
    }
}

/// `self_id` 的版本段是否**严格高于** `recorded` 的版本段（数值逐段比较，
/// 如 `1.3.44 > 1.3.43`、`1.10.0 > 1.9.9`；段数不齐按字典序，等前缀更长者
/// 视为更高）。任一侧版本段解析失败 → `false`（不可排序 → 调用方不换代，
/// fail-safe）。
#[must_use]
pub fn build_id_version_newer(self_id: &str, recorded: &str) -> bool {
    match (
        parse_build_id_version(self_id),
        parse_build_id_version(recorded),
    ) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

/// 取 build-id `${version}+${sha}` 的版本段并解析为数值段列表；
/// 无 `+` 分隔或任一段非纯数字 → `None`。
fn parse_build_id_version(id: &str) -> Option<Vec<u64>> {
    let version = id.split('+').next()?;
    if version.is_empty() {
        return None;
    }
    version
        .split('.')
        .map(|seg| seg.parse::<u64>().ok())
        .collect()
}

/// `<run_dir>/<name>.build-id`。
#[must_use]
pub fn build_id_file(name: &str) -> PathBuf {
    build_id_file_for_generation(name, None)
}

/// Resolve the build-id sidecar for an optional daemon generation.
#[must_use]
pub fn build_id_file_for_generation(name: &str, generation: Option<&str>) -> PathBuf {
    paths::run_dir().join(format!(
        "{}.build-id",
        paths::name_with_generation(name, generation)
    ))
}

/// 读 daemon 落盘的 build-id（trim；空文件视为缺失）。
#[must_use]
pub fn read_recorded_build_id(path: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// daemon 自报身份：把 `id` 写到 `<run_dir>/<name>.build-id`。
///
/// 写失败只 warn 不挡启动（缺文件 = 下一轮 ensure 判 Mismatch 换代一次，
/// fail-open 语义自洽）。
pub fn write_build_id_file(name: &str, id: &str) {
    write_build_id_file_for_generation(name, None, id);
}

/// Persist a daemon build id for the exact endpoint generation. Empty
/// generations retain the canonical sidecar path and fail-soft behavior.
pub fn write_build_id_file_for_generation(name: &str, generation: Option<&str>, id: &str) {
    let path = build_id_file_for_generation(name, generation);
    if let Err(err) = paths::ensure_run_dir() {
        tracing::warn!(daemon = name, error = %err, "build-id 写入前创建 run 目录失败（不挡启动）");
        return;
    }
    if let Err(err) = std::fs::write(&path, id) {
        tracing::warn!(daemon = name, file = %path.display(), error = %err, "build-id 文件写入失败（不挡启动）");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_build_id_is_nonempty_and_has_plus_separator() {
        let id = self_build_id();
        assert!(!id.is_empty());
        assert!(id.contains('+'), "build-id 必须是 version+sha 形态: {id}");
    }

    #[test]
    fn authoritative_semantics() {
        assert!(is_authoritative("1.3.41+abc123def456"));
        assert!(!is_authoritative("1.3.41+unknown"));
        assert!(!is_authoritative(""));
    }

    #[test]
    fn skip_flag_only_honors_literal_one() {
        assert!(skip_flag_set(Some("1")));
        assert!(!skip_flag_set(Some("0")));
        assert!(!skip_flag_set(Some("")));
        assert!(!skip_flag_set(Some("true")));
        assert!(!skip_flag_set(None));
    }

    #[test]
    fn evaluate_skip_when_flag_set() {
        assert_eq!(
            evaluate_handshake("1.0.0+aaaaaaaaaaaa", Some("1.0.0+bbbbbbbbbbbb"), true),
            HandshakeVerdict::Skip
        );
    }

    #[test]
    fn evaluate_skip_when_self_unknown() {
        assert_eq!(
            evaluate_handshake("1.0.0+unknown", Some("1.0.0+bbbbbbbbbbbb"), false),
            HandshakeVerdict::Skip
        );
    }

    #[test]
    fn evaluate_skip_when_recorded_unknown() {
        assert_eq!(
            evaluate_handshake("1.0.0+aaaaaaaaaaaa", Some("9.9.9+unknown"), false),
            HandshakeVerdict::Skip
        );
    }

    #[test]
    fn evaluate_mismatch_when_recorded_missing() {
        // 老代 daemon 从未写过 build-id 文件 → 换代一次即自愈
        assert_eq!(
            evaluate_handshake("1.0.0+aaaaaaaaaaaa", None, false),
            HandshakeVerdict::Mismatch
        );
    }

    #[test]
    fn evaluate_match_and_mismatch_on_authoritative_pair() {
        assert_eq!(
            evaluate_handshake("1.0.0+aaaaaaaaaaaa", Some("1.0.0+aaaaaaaaaaaa"), false),
            HandshakeVerdict::Match
        );
        // 2026-06-12 只升不降修订：同版本不同 sha = 不可排序 → 不换代
        // （旧契约 Mismatch 会与对端互相换代乒乓）。
        assert_eq!(
            evaluate_handshake("1.0.0+aaaaaaaaaaaa", Some("1.0.0+bbbbbbbbbbbb"), false),
            HandshakeVerdict::Skip
        );
    }

    #[test]
    fn evaluate_upgrade_only_never_downgrade() {
        // 自身更新 → 换代（升级）
        assert_eq!(
            evaluate_handshake("1.3.44+aaaaaaaaaaaa", Some("1.3.43+bbbbbbbbbbbb"), false),
            HandshakeVerdict::Mismatch
        );
        // 自身更旧 → 不换代（防降级乒乓：真机 1.3.43 旧窗曾把 1.3.44 daemon 换回旧代）
        assert_eq!(
            evaluate_handshake("1.3.43+bbbbbbbbbbbb", Some("1.3.44+aaaaaaaaaaaa"), false),
            HandshakeVerdict::Skip
        );
        // 数值比较而非字符串比较：1.10.0 > 1.9.9
        assert_eq!(
            evaluate_handshake("1.10.0+aaaaaaaaaaaa", Some("1.9.9+bbbbbbbbbbbb"), false),
            HandshakeVerdict::Mismatch
        );
        // 版本段解析失败 → 不可排序 → 不换代（fail-safe）
        assert_eq!(
            evaluate_handshake("1.0.0-beta+aaaaaaaaaaaa", Some("1.0.0+bbbbbbbbbbbb"), false),
            HandshakeVerdict::Skip
        );
    }

    #[test]
    fn build_id_version_newer_ordering() {
        assert!(build_id_version_newer(
            "1.3.44+aaaaaaaaaaaa",
            "1.3.43+bbbbbbbbbbbb"
        ));
        assert!(!build_id_version_newer(
            "1.3.43+aaaaaaaaaaaa",
            "1.3.44+bbbbbbbbbbbb"
        ));
        assert!(!build_id_version_newer(
            "1.3.44+aaaaaaaaaaaa",
            "1.3.44+bbbbbbbbbbbb"
        ));
        assert!(build_id_version_newer(
            "2.0.0+aaaaaaaaaaaa",
            "1.99.99+bbbbbbbbbbbb"
        ));
    }

    #[test]
    fn build_id_file_uses_the_daemon_name() {
        assert!(
            build_id_file("cron")
                .to_string_lossy()
                .ends_with("cron.build-id")
        );
    }

    #[test]
    fn read_recorded_build_id_trims_and_rejects_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.build-id");
        std::fs::write(&p, "  1.0.0+abc123def456\n").unwrap();
        assert_eq!(
            read_recorded_build_id(&p).as_deref(),
            Some("1.0.0+abc123def456")
        );
        std::fs::write(&p, "   \n").unwrap();
        assert!(read_recorded_build_id(&p).is_none());
        assert!(read_recorded_build_id(&dir.path().join("absent")).is_none());
    }
}

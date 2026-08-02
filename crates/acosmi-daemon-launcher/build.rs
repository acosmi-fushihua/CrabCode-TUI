//! F1 daemon/产物代际握手 (2026-06-11) — 编译期注入 `CRABCODE_BUILD_ID`。
//!
//! 格式契约（与 TS `scripts/build-ts.ts` / `scripts/build-worker.ts` 的
//! `MACRO.BUILD_ID`、`scripts/release.sh` 的 bundle `build-id` 文件逐字对齐）：
//!
//! - env `CRABCODE_BUILD_ID` 已设 → **逐字**使用（release 链权威路径，
//!   `scripts/release.sh` 统一计算后 export，cargo/bun 两轨继承同一字符串）；
//! - 否则 `${CARGO_PKG_VERSION}+$(git rev-parse --short=12 HEAD)`；
//! - git 不可用（release tarball 内构建 / 无 .git）→ `${CARGO_PKG_VERSION}+unknown`
//!   （`+unknown` 后缀 = 非权威 id，运行时比对永远视为一致，见
//!   `src/build_id.rs`）。
//!
//! 注意只声明 `rerun-if-env-changed=CRABCODE_BUILD_ID`，**不** watch git 文件 ——
//! 否则每次 commit 都重编译整条依赖链。接受「同 checkout 不重编译 → dev 下
//! sha 可能滞后」的弱一致；release 走 env 注入是权威路径。
//!
//! 本 crate 是 cron launcher 与 browser 诊断字段共享的 build-id 真源。
//! `crabcode-cron` 和 `acosmi-cmd-browser` 通过
//! `acosmi_daemon_launcher::build_id::self_build_id()` 取值。workspace 统一
//! `[workspace.package].version`，故各二进制取到的 `${version}+${sha}` 与
//! 各自 `CARGO_PKG_VERSION` 拼出的结果逐字相同。

fn main() {
    println!("cargo:rerun-if-env-changed=CRABCODE_BUILD_ID");

    let build_id = std::env::var("CRABCODE_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(derive_build_id_from_git);

    println!("cargo:rustc-env=CRABCODE_BUILD_ID={build_id}");
}

fn derive_build_id_from_git() -> String {
    let version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
    let sha = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|raw| raw.trim().to_string())
        .filter(|sha| !sha.is_empty() && sha.chars().all(|c| c.is_ascii_hexdigit()));

    match sha {
        Some(sha) => format!("{version}+{sha}"),
        None => format!("{version}+unknown"),
    }
}

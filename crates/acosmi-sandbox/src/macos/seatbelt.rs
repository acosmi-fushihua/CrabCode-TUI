//! SBPL (Sandbox Profile Language) profile generator.
//!
//! Generates Seatbelt profiles dynamically from [`SandboxConfig`], following
//! the Chromium model of parameterized per-use profiles.
//!
//! # Profile structure
//!
//! ```scheme
//! (version 1)
//! (deny default)
//! (import "bsd.sb")        ; minimal baseline (dynamic linker, locale, /dev/urandom)
//! ;; workspace, network, process, system paths, devices — generated per config
//! ```
//!
//! # Key design decisions
//!
//! - Base import is `bsd.sb` (NOT `system.sb` — too permissive)
//! - Workspace path is injected via `(param "WORKSPACE_DIR")` for safe escaping
//! - Process execution is broadly allowed (the sandbox restricts resources, not which
//!   binaries can run — that's the CLI layer's responsibility)
//! - Network deny rules take precedence over allow rules in SBPL

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::config::{MountMode, NetworkPolicy, SandboxConfig, SecurityLevel};
use crate::error::SandboxError;
use crate::exec_config::SandboxExecPlan;
use crate::exec_rules::{ResolvedFsRule, ResolvedFsRules};

/// A generated sandbox profile with its parameter bindings.
pub struct SandboxProfile {
    /// The SBPL source string.
    pub sbpl: String,
    /// Key-value parameters referenced in the profile via `(param "KEY")`.
    pub params: Vec<(String, String)>,
}

/// Generate a Seatbelt SBPL profile from the given sandbox configuration.
///
/// The workspace directory is passed as a parameter (`WORKSPACE_DIR`) to avoid
/// SBPL injection via path names containing special characters.
pub fn generate_profile(config: &SandboxConfig) -> Result<SandboxProfile, SandboxError> {
    generate_profile_inner(config, NetworkRelaxations::default())
}

/// 用户显式要求的网络放宽 + PR-8 的出口锁端口。默认全零 ⇒ 与本结构出现前
/// 逐字同构。
#[derive(Debug, Clone, Copy, Default)]
pub struct NetworkRelaxations {
    /// `network.allowLocalBinding`：`Restricted` 档下不再拒绝 localhost 出站。
    pub allow_local_binding: bool,
    /// 过滤代理监听的 loopback 端口（`network.httpProxyPort`）。
    ///
    /// `0` = 没有代理 ⇒ 网络规则与加锁前**逐字同构**（这是本字段的默认值，
    /// 也是「没有代理时行为完全不变」这条契约的落点）。非零 ⇒ 出口锁生效：
    /// 唯一可达的 TCP 目的地是 `localhost:<port>`，见 [`emit_network_rules`]。
    ///
    /// 注意这不是「放宽」而是「收紧」——它和上面那个字段方向相反，放在同一个
    /// 结构里是因为两者都只影响 `emit_network_rules` 的 `Restricted` 分支。
    pub proxy_port: u16,
}

fn generate_profile_inner(
    config: &SandboxConfig,
    relax: NetworkRelaxations,
) -> Result<SandboxProfile, SandboxError> {
    let mut sbpl = String::with_capacity(2048);
    let mut params: Vec<(String, String)> = Vec::new();

    // ── Header ─────────────────────────────────────────────────────────────
    sbpl.push_str("(version 1)\n");
    sbpl.push_str("(deny default)\n");
    sbpl.push_str("(import \"bsd.sb\")\n\n");

    // ── Workspace access ───────────────────────────────────────────────────
    // Canonicalize workspace path — on macOS, /var → /private/var is a symlink
    // and Seatbelt operates on resolved paths.
    let canonical_workspace =
        config
            .workspace
            .canonicalize()
            .map_err(|e| SandboxError::PathError {
                path: config.workspace.clone(),
                reason: format!("failed to canonicalize workspace: {e}"),
            })?;
    let workspace_str = canonical_workspace
        .to_str()
        .ok_or_else(|| SandboxError::PathError {
            path: config.workspace.clone(),
            reason: "workspace path is not valid UTF-8".into(),
        })?;
    params.push(("WORKSPACE_DIR".into(), workspace_str.into()));

    // TMPDIR parameter — used for per-user temp directory access.
    // Must canonicalize for the same symlink reason as workspace.
    let tmpdir_raw = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let tmpdir = std::path::Path::new(&tmpdir_raw)
        .canonicalize()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(tmpdir_raw);
    params.push(("TMPDIR".into(), tmpdir));

    sbpl.push_str("; Workspace access\n");
    sbpl.push_str("(define workspace-dir (param \"WORKSPACE_DIR\"))\n");

    match config.security_level {
        SecurityLevel::L0Deny => {
            sbpl.push_str("(allow file-read* (subpath workspace-dir))\n");
        }
        SecurityLevel::L1Allowlist | SecurityLevel::L2Sandboxed => {
            sbpl.push_str("(allow file-read* file-write* (subpath workspace-dir))\n");
        }
    }
    sbpl.push('\n');

    // ── Additional mounts ──────────────────────────────────────────────────
    // F-04 fix: Use SBPL parameters for mount paths (consistent with workspace)
    // to prevent SBPL injection via specially crafted path names.
    if !config.mounts.is_empty() {
        sbpl.push_str("; Additional mounts\n");
        for (i, mount) in config.mounts.iter().enumerate() {
            let canonical_mount =
                mount
                    .host_path
                    .canonicalize()
                    .map_err(|e| SandboxError::PathError {
                        path: mount.host_path.clone(),
                        reason: format!("failed to canonicalize mount path: {e}"),
                    })?;
            let path = canonical_mount
                .to_str()
                .ok_or_else(|| SandboxError::PathError {
                    path: mount.host_path.clone(),
                    reason: "mount path is not valid UTF-8".into(),
                })?;
            let param_name = format!("MOUNT_{i}");
            params.push((param_name.clone(), path.into()));
            let _ = writeln!(sbpl, "(define mount-{i} (param \"{param_name}\"))");
            match mount.mode {
                MountMode::ReadOnly => {
                    let _ = writeln!(sbpl, "(allow file-read* (subpath mount-{i}))");
                }
                MountMode::ReadWrite => {
                    let _ = writeln!(sbpl, "(allow file-read* file-write* (subpath mount-{i}))");
                }
            }
        }
        sbpl.push('\n');
    }

    // ── Network policy ─────────────────────────────────────────────────────
    emit_network_rules(&mut sbpl, config.effective_network_policy(), relax);

    // ── Process execution ──────────────────────────────────────────────────
    emit_process_rules(&mut sbpl);

    // ── System paths ───────────────────────────────────────────────────────
    emit_system_paths(&mut sbpl);

    // ── Temp directories ───────────────────────────────────────────────────
    emit_temp_dirs(&mut sbpl);

    // ── Device access ──────────────────────────────────────────────────────
    emit_device_access(&mut sbpl);

    // ── Mach services ──────────────────────────────────────────────────────
    emit_mach_services(&mut sbpl);

    // ── Sysctl access ──────────────────────────────────────────────────────
    emit_sysctl_access(&mut sbpl);

    Ok(SandboxProfile { sbpl, params })
}

/// 按执行计划生成 profile —— fs 四表在这里第一次真的进入内核
/// （W-SANDBOX-ENFORCED-DEADCODE PR-2）。
///
/// 事故前 `handle_spawn_managed_sandboxed` 写死 `mounts: vec![]`，四表一条都
/// 到不了这里（SoT §1.5）。现在 allow 段与 deny 段都追加在 profile **末尾**，
/// 顺序是语义的一部分：
///
/// 1. 基础 profile（workspace / 系统路径 / temp / 设备）先写；
/// 2. 计划的 allow 段其次 —— 它只会**加宽**，与前面的规则不冲突；
/// 3. 计划的 deny 段**最后** —— SBPL 后写的规则压过先写的，deny 要赢就必须
///    排在所有 allow 之后。
///
/// 第 3 条的优先级假设没有编译期证据，所以施加层**不信它**：施加完成后由
/// `platform::apply_exec_plan_to_self` 拿 `sandbox_check` 逐条实测重叠的 deny，
/// 证不出来就整条命令失败。假设归假设，出厂前得有实测。
///
/// `TMPDIR` 由调用方在**调用本函数之前**设进本进程 env（helper 的执行顺序），
/// 基础 profile 的 `TMPDIR` 参数因此自动取到计划里的 `tmp_dir`——不需要第二条
/// 传递路径，也就没有两条路径不一致的可能。
pub fn generate_profile_for_plan(
    plan: &SandboxExecPlan,
    resolved: &ResolvedFsRules,
) -> Result<SandboxProfile, SandboxError> {
    let mut profile = generate_profile_inner(
        &plan.base,
        NetworkRelaxations {
            allow_local_binding: plan.network.allow_local_binding,
            proxy_port: plan.network.http_proxy_port,
        },
    )?;

    let mut sbpl = profile.sbpl;
    let mut params = profile.params;

    // ── weaker.networkIsolation（W-SANDBOX-ENFORCED-DEADCODE PR-8）─────────
    //
    // 这是该开关在 macOS 上**唯一**的实现（schema 也把它标为 macOS-only）：
    // Go 写的工具自己做 TLS 证书链校验，走的是 `trustd` 这个 Mach 服务；
    // 基础 profile 的 Mach 白名单里没有它，于是它们在沙箱里连不上任何 HTTPS。
    // 放行它是**更弱**方向，所以必须由用户显式打开，默认关。
    //
    // 位置刻意在 fs 四表**之前**：本块只放行 mach-lookup，与 fs 规则不重叠，
    // 而下面那条「deny 段必须排在所有 allow 之后」的排序契约不能被打破。
    if plan.weaker.network_isolation {
        sbpl.push_str("; weaker.networkIsolation — TLS trust evaluation (macOS-only)\n");
        sbpl.push_str("(allow mach-lookup (global-name \"com.apple.trustd.agent\"))\n\n");
    }

    if !resolved.allow_read.is_empty() || !resolved.allow_write.is_empty() {
        sbpl.push_str("; Plan filesystem allow rules (fs four tables)\n");
        for (i, rule) in resolved.allow_read.iter().enumerate() {
            emit_plan_path(&mut sbpl, &mut params, &format!("FS_ALLOW_R_{i}"), rule);
            let _ = writeln!(sbpl, "(allow file-read* (subpath fs-allow-r-{i}))");
        }
        for (i, rule) in resolved.allow_write.iter().enumerate() {
            emit_plan_path(&mut sbpl, &mut params, &format!("FS_ALLOW_W_{i}"), rule);
            let _ = writeln!(
                sbpl,
                "(allow file-read* file-write* (subpath fs-allow-w-{i}))"
            );
        }
        sbpl.push('\n');
    }

    if !resolved.deny_read.is_empty() || !resolved.deny_write.is_empty() {
        sbpl.push_str("; Plan filesystem deny rules — MUST stay last (later rules win)\n");
        for (i, rule) in resolved.deny_read.iter().enumerate() {
            emit_plan_path(&mut sbpl, &mut params, &format!("FS_DENY_R_{i}"), rule);
            // subpath + literal 都写：目标可能是目录也可能是**尚不存在**的文件
            // （典型：为一个还没被创建的 settings.json 预先 denyWrite），
            // 而我们无法在这里可靠区分。`literal` ⊆ `subpath`，多写一条不会
            // 扩大拒绝范围，只是把「目标其实是个文件」这种情况也焊死。
            let _ = writeln!(sbpl, "(deny file-read* (subpath fs-deny-r-{i}))");
            let _ = writeln!(sbpl, "(deny file-read* (literal fs-deny-r-{i}))");
        }
        for (i, rule) in resolved.deny_write.iter().enumerate() {
            emit_plan_path(&mut sbpl, &mut params, &format!("FS_DENY_W_{i}"), rule);
            let _ = writeln!(sbpl, "(deny file-write* (subpath fs-deny-w-{i}))");
            let _ = writeln!(sbpl, "(deny file-write* (literal fs-deny-w-{i}))");
        }
        sbpl.push('\n');
    }

    profile = SandboxProfile { sbpl, params };
    Ok(profile)
}

/// 声明一个路径参数并绑定成 SBPL 变量。
///
/// 路径**永远走 `(param …)`**，不直接拼进 profile 文本——这是本文件既有的
/// 反注入契约（F-04）：规则里的路径来自用户 settings，含引号或括号的路径
/// 直接拼进去就是一次 SBPL 注入。
fn emit_plan_path(
    sbpl: &mut String,
    params: &mut Vec<(String, String)>,
    param_name: &str,
    rule: &ResolvedFsRule,
) {
    let path = canonicalize_best_effort(&rule.path);
    params.push((param_name.to_string(), path.to_string_lossy().into_owned()));
    let var = param_name.to_lowercase().replace('_', "-");
    let _ = writeln!(sbpl, "(define {var} (param \"{param_name}\"))");
}

/// 尽力 canonicalize：路径不存在时，canonicalize 它**最深的存在祖先**再把剩下
/// 的部分接回去。
///
/// 为什么不能只用 `canonicalize`：规则里的路径经常还不存在（为一个尚未创建的
/// 文件预先 denyWrite 是最常见的形态），`canonicalize` 会 ENOENT，那条规则就
/// 没了。为什么不能干脆不 canonicalize：macOS 上 `/tmp` → `/private/tmp`、
/// `/var` → `/private/var` 是符号链接，而 Seatbelt 按**解析后**的路径匹配——
/// 拿未解析的写法去 deny，内核那边根本对不上，等于没写。
///
/// 两种做法各自会丢一半规则，所以必须是「尽力」这一种。
pub(crate) fn canonicalize_best_effort(path: &Path) -> PathBuf {
    if let Ok(resolved) = path.canonicalize() {
        return resolved;
    }
    let mut suffix: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        let Some(name) = cursor.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        if let Ok(resolved) = parent.canonicalize() {
            let mut out = resolved;
            for part in suffix.iter().rev() {
                out.push(part);
            }
            return out;
        }
        cursor = parent;
    }
    path.to_path_buf()
}

/// Escape a string for embedding in SBPL quoted strings.
/// Handles backslashes and double quotes.
#[cfg(test)]
fn escape_sbpl_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Emit network rules based on the effective network policy.
fn emit_network_rules(sbpl: &mut String, policy: NetworkPolicy, relax: NetworkRelaxations) {
    sbpl.push_str("; Network policy\n");
    match policy {
        NetworkPolicy::None => {
            // (deny default) already blocks everything; explicit for clarity
            sbpl.push_str("(deny network*)\n");
        }
        NetworkPolicy::Restricted if relax.proxy_port > 0 => {
            // ── PR-8 出口锁：过滤代理是**唯一**可达的 TCP 目的地 ─────────────
            //
            // 这一档的全部力量来自**没写的那一行**：profile 头部是
            // `(deny default)`，所以只要不发那条通吃的
            // `(allow network-outbound (remote tcp))`，外部主机、LAN、
            // 以及 loopback 上代理之外的每一个端口就都到不了——不需要（也无法）
            // 逐个去 deny。SBPL 的 host token 只认 `*` 与 `localhost`
            // （不认 IP 字面量，见下面 `Restricted` 常规档的注释），而
            // `localhost` 匹配的正是代理监听的回环地址。
            //
            // 这条规则把「代理只是建议」变成「代理是唯一出口」：命令想联网
            // 就只能经过过滤代理，域名白/黑名单才第一次真的有强制力。
            //
            // 假设归假设——「不发通吃规则 ⇒ 其余 TCP 真的不可达」这件事没有
            // 编译期证据，所以施加完由 `platform::verify_egress_locked_to_proxy`
            // 当场向内核实证；证不出来整条命令失败（125），绝不放一个
            // 「以为在过滤」的沙箱出去。
            let _ = writeln!(
                sbpl,
                "(allow network-outbound (remote tcp \"localhost:{}\"))",
                relax.proxy_port
            );
            // DNS 保留：拿到域名解析并不能绕开出口锁（解析出的 IP 依然连不上），
            // 而拿掉它会让「先 getaddrinfo 再连代理」的常规客户端直接挂掉。
            sbpl.push_str("(allow network-outbound (remote udp \"*:53\"))\n");
            // Unix 域套接字照旧全拦——否则它就是绕开代理的第二条路。
            sbpl.push_str("(deny network* (local unix-socket))\n");
            //
            // `network.allowLocalBinding` 在这一档**没有单独的分支**：本档只
            // 放行 `localhost:<proxy>` 一个目的地，其余 loopback 端口由
            // `(deny default)` 兜住——比常规档那条 `(deny … "localhost:*")`
            // 更严，方向安全，所以不需要（也不能）为它开洞：开了就等于给了
            // 一条不经代理的出口。
        }
        NetworkPolicy::Restricted => {
            // Allow outbound TCP to public internet
            sbpl.push_str("(allow network-outbound (remote tcp))\n");
            // DNS resolution (UDP port 53)
            sbpl.push_str("(allow network-outbound (remote udp \"*:53\"))\n");
            // Deny localhost — SBPL only supports `*` or `localhost` as host
            // (CIDR ranges like 127.0.0.0/8 are NOT supported by Seatbelt)
            //
            // `network.allowLocalBinding` 打开时保留 localhost：用户显式声明
            // 沙箱内要跑本地服务并连它（dev server / 本地数据库）。
            if !relax.allow_local_binding {
                sbpl.push_str("(deny network-outbound (remote tcp \"localhost:*\"))\n");
            }
            // Block Unix domain sockets to prevent proxy bypass
            sbpl.push_str("(deny network* (local unix-socket))\n");
            // NOTE: LAN addresses (10.x, 172.16.x, 192.168.x) cannot be blocked
            // via SBPL alone — Seatbelt network filters operate on hostname/port,
            // not on IP ranges. Full LAN blocking requires Network Extension or
            // a proxy-based approach (Phase 6 enhancement).
        }
        NetworkPolicy::Host => {
            sbpl.push_str("(allow network*)\n");
        }
    }
    sbpl.push('\n');
}

/// Emit process execution rules.
///
/// We allow broad process execution because the sandbox's job is resource isolation,
/// not binary whitelisting. The CLI layer controls which commands are allowed.
fn emit_process_rules(sbpl: &mut String) {
    sbpl.push_str("; Process execution\n");
    sbpl.push_str("(allow process-exec)\n");
    sbpl.push_str("(allow process-fork)\n");
    sbpl.push_str("(allow signal (target self))\n\n");
}

/// Emit system path read access for dynamic linking and interpreter support.
///
/// `bsd.sb` covers basic system libraries, but interpreters (Python, Node, Ruby)
/// and Homebrew tools need additional paths.
fn emit_system_paths(sbpl: &mut String) {
    sbpl.push_str("; System paths for dynamic linking and interpreter support\n");
    let read_paths = [
        "/usr/lib",
        "/usr/share",
        "/usr/local",
        "/opt/homebrew",
        "/System/Library",
        "/Library/Frameworks",
        "/Library/Apple",
        "/private/var/db", // dyld shared cache metadata
    ];
    for path in &read_paths {
        let _ = writeln!(sbpl, "(allow file-read* (subpath \"{path}\"))");
    }

    // Executable search paths
    let exec_paths = ["/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"];
    for path in &exec_paths {
        let _ = writeln!(sbpl, "(allow file-read* (subpath \"{path}\"))");
    }
    sbpl.push('\n');
}

/// Emit temp directory access.
fn emit_temp_dirs(sbpl: &mut String) {
    sbpl.push_str("; Temporary directories\n");
    sbpl.push_str("(allow file-read* file-write* (subpath \"/tmp\"))\n");
    sbpl.push_str("(allow file-read* file-write* (subpath \"/private/tmp\"))\n");
    // macOS per-user temp dir
    sbpl.push_str("(allow file-read* file-write* (subpath (param \"TMPDIR\")))\n");
    sbpl.push('\n');
}

/// Emit device file access.
fn emit_device_access(sbpl: &mut String) {
    sbpl.push_str("; Device access\n");
    let devices = [
        "/dev/null",
        "/dev/zero",
        "/dev/urandom",
        "/dev/random",
        "/dev/stdin",
        "/dev/stdout",
        "/dev/stderr",
        "/dev/fd",
        "/dev/tty",
        "/dev/dtracehelper",
    ];
    for dev in &devices {
        let _ = writeln!(sbpl, "(allow file-read* file-write* (literal \"{dev}\"))");
    }
    // /dev/fd/* needs subpath access for file descriptor operations
    sbpl.push_str("(allow file-read* file-write* (subpath \"/dev/fd\"))\n\n");
}

/// Emit Mach service lookups required by many macOS programs.
fn emit_mach_services(sbpl: &mut String) {
    sbpl.push_str("; Required Mach services\n");
    let services = [
        "com.apple.system.logger",
        "com.apple.system.notification_center",
        "com.apple.CoreServices.coreservicesd",
        "com.apple.SecurityServer",
        "com.apple.system.opendirectoryd.libinfo",
    ];
    for svc in &services {
        let _ = writeln!(sbpl, "(allow mach-lookup (global-name \"{svc}\"))");
    }
    sbpl.push('\n');
}

/// Emit sysctl read access needed by runtime introspection.
fn emit_sysctl_access(sbpl: &mut String) {
    sbpl.push_str("; Sysctl access (runtime introspection)\n");
    sbpl.push_str("(allow sysctl-read)\n\n");
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::{BackendPreference, OutputFormat, ResourceLimits};

    /// Create a test config using a real temp directory (required for canonicalization).
    fn test_config(
        security_level: SecurityLevel,
        network: Option<NetworkPolicy>,
    ) -> (SandboxConfig, tempfile::TempDir) {
        #[allow(clippy::expect_used)]
        let tmpdir = tempfile::tempdir().expect("failed to create temp dir");
        let config = SandboxConfig {
            security_level,
            command: "/usr/bin/echo".into(),
            args: vec!["hello".into()],
            workspace: tmpdir.path().to_path_buf(),
            mounts: vec![],
            resource_limits: ResourceLimits::default(),
            network_policy: network,
            env_vars: std::collections::HashMap::new(),
            format: OutputFormat::Json,
            backend: BackendPreference::Native,
        };
        (config, tmpdir)
    }

    #[test]
    fn profile_l0_deny_has_readonly_workspace() {
        let (config, _td) = test_config(SecurityLevel::L0Deny, None);
        let profile = generate_profile(&config).unwrap();
        // Workspace is read-only (no file-write* on workspace-dir)
        assert!(
            profile
                .sbpl
                .contains("(allow file-read* (subpath workspace-dir))")
        );
        assert!(
            !profile
                .sbpl
                .contains("(allow file-read* file-write* (subpath workspace-dir))")
        );
        // L0 default network is None
        assert!(profile.sbpl.contains("(deny network*)"));
    }

    #[test]
    fn profile_l1_sandbox_has_readwrite_workspace() {
        let (config, _td) = test_config(SecurityLevel::L1Allowlist, None);
        let profile = generate_profile(&config).unwrap();
        assert!(profile.sbpl.contains("file-write*"));
        // L1 default network is Restricted
        assert!(
            profile
                .sbpl
                .contains("(allow network-outbound (remote tcp))")
        );
        assert!(
            profile
                .sbpl
                .contains("(deny network-outbound (remote tcp \"localhost:*\"))")
        );
    }

    #[test]
    fn profile_host_network_allows_all() {
        let (config, _td) = test_config(SecurityLevel::L2Sandboxed, Some(NetworkPolicy::Host));
        let profile = generate_profile(&config).unwrap();
        assert!(profile.sbpl.contains("(allow network*)"));
    }

    #[test]
    fn profile_has_required_header() {
        let (config, _td) = test_config(SecurityLevel::L1Allowlist, None);
        let profile = generate_profile(&config).unwrap();
        assert!(
            profile
                .sbpl
                .starts_with("(version 1)\n(deny default)\n(import \"bsd.sb\")\n")
        );
    }

    #[test]
    fn profile_params_contain_workspace() {
        let (config, td) = test_config(SecurityLevel::L1Allowlist, None);
        let profile = generate_profile(&config).unwrap();
        // Workspace param should be the canonicalized path
        let canonical = td.path().canonicalize().unwrap();
        let canonical_str = canonical.to_str().unwrap();
        assert!(
            profile
                .params
                .iter()
                .any(|(k, v)| k == "WORKSPACE_DIR" && v == canonical_str)
        );
    }

    #[test]
    fn escape_handles_special_chars() {
        assert_eq!(
            escape_sbpl_string(r#"path\with"quotes"#),
            r#"path\\with\"quotes"#
        );
    }

    // ── PR-8 出口锁 ────────────────────────────────────────────────────────

    /// 通吃规则的**字面量**。出口锁的全部力量来自「不发这一行」，所以它必须
    /// 是一个具名常量而不是散落在断言里的字符串——改了发射侧却忘了改断言，
    /// 断言就会变成一句永真的空话。
    const BLANKET_TCP_RULE: &str = "(allow network-outbound (remote tcp))";

    fn test_plan(
        proxy_port: u16,
        weaker_network_isolation: bool,
    ) -> (SandboxExecPlan, tempfile::TempDir) {
        let (base, tmpdir) =
            test_config(SecurityLevel::L2Sandboxed, Some(NetworkPolicy::Restricted));
        let plan = SandboxExecPlan {
            base,
            filesystem: crate::exec_config::FsRules {
                allow_read: vec![],
                allow_write: vec![],
                deny_read: vec![],
                deny_write: vec![],
            },
            network: crate::exec_config::NetworkRules {
                policy: NetworkPolicy::Restricted,
                allowed_domains: vec![],
                denied_domains: vec![],
                allow_unix_sockets: vec![],
                allow_all_unix_sockets: false,
                allow_local_binding: false,
                http_proxy_port: proxy_port,
                socks_proxy_port: 0,
            },
            weaker: crate::exec_config::WeakerFlags {
                nested_sandbox: false,
                network_isolation: weaker_network_isolation,
            },
            tmp_dir: tmpdir.path().to_path_buf(),
            fidelity: crate::exec_config::FidelityReport {
                level: crate::exec_config::FidelityLevel::Full,
                unenforced: vec![],
            },
        };
        (plan, tmpdir)
    }

    #[test]
    fn proxy_lock_makes_the_proxy_the_only_reachable_tcp_destination() {
        let (plan, _td) = test_plan(31337, false);
        let profile = generate_profile_for_plan(&plan, &ResolvedFsRules::default()).unwrap();

        // 唯一放行的 TCP 目的地。
        assert!(
            profile
                .sbpl
                .contains("(allow network-outbound (remote tcp \"localhost:31337\"))"),
            "proxy port must be the one allowed TCP destination:\n{}",
            profile.sbpl
        );
        // …而这条不发，才是「其余全部不可达」的来源。
        assert!(
            !profile.sbpl.contains(BLANKET_TCP_RULE),
            "the blanket TCP allow must NOT be emitted under the egress lock:\n{}",
            profile.sbpl
        );
        // 常规档那条 localhost deny 会把代理自己也拦掉，出口锁档必须不发它。
        assert!(
            !profile
                .sbpl
                .contains("(deny network-outbound (remote tcp \"localhost:*\"))")
        );
        // DNS 与 unix-socket 拦截照旧。
        assert!(
            profile
                .sbpl
                .contains("(allow network-outbound (remote udp \"*:53\"))")
        );
        assert!(profile.sbpl.contains("(deny network* (local unix-socket))"));
    }

    #[test]
    fn zero_proxy_port_leaves_the_network_rules_byte_identical() {
        let (plan, _td) = test_plan(0, false);
        let locked = generate_profile_for_plan(&plan, &ResolvedFsRules::default()).unwrap();

        // 与「本字段出现之前」的 Restricted 档逐字同构。
        assert!(locked.sbpl.contains(BLANKET_TCP_RULE));
        assert!(
            locked
                .sbpl
                .contains("(deny network-outbound (remote tcp \"localhost:*\"))")
        );
        assert!(!locked.sbpl.contains("localhost:0"));

        // 与不带 plan 的基础生成器同一段网络规则（同一个 workspace，逐字比对）。
        let baseline = generate_profile_inner(&plan.base, NetworkRelaxations::default()).unwrap();
        assert_eq!(
            network_section(&locked.sbpl),
            network_section(&baseline.sbpl),
            "http_proxy_port == 0 must leave the network section untouched"
        );
    }

    /// 抠出 `; Network policy` 到下一个空行为止的那一段。
    fn network_section(sbpl: &str) -> String {
        sbpl.split("; Network policy\n")
            .nth(1)
            .unwrap_or_default()
            .split("\n\n")
            .next()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn trustd_is_allowed_only_when_the_weaker_flag_is_set() {
        const TRUSTD: &str = "(allow mach-lookup (global-name \"com.apple.trustd.agent\"))";

        let (off, _td_off) = test_plan(31337, false);
        let profile_off = generate_profile_for_plan(&off, &ResolvedFsRules::default()).unwrap();
        assert!(!profile_off.sbpl.contains(TRUSTD));

        let (on, _td_on) = test_plan(31337, true);
        let profile_on = generate_profile_for_plan(&on, &ResolvedFsRules::default()).unwrap();
        assert!(profile_on.sbpl.contains(TRUSTD));

        // 开关与代理端口正交：没有代理时打开它同样只加这一行。
        let (on_no_proxy, _td2) = test_plan(0, true);
        let profile_no_proxy =
            generate_profile_for_plan(&on_no_proxy, &ResolvedFsRules::default()).unwrap();
        assert!(profile_no_proxy.sbpl.contains(TRUSTD));
        assert!(profile_no_proxy.sbpl.contains(BLANKET_TCP_RULE));
    }
}

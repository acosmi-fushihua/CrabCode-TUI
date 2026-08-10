//! `crabcode sandbox-exec` 真二进制烟测
//! （W-SANDBOX-ENFORCED-DEADCODE PR-1 配置管道 + PR-2 Unix 执行 + PR-3 Windows 执行）。
//!
//! 单测能证明解析器与映射层是对的，但证明不了**这条 argv 真的走到了那段代码**：
//! 快路径拦截排在 `Cli::try_parse` 之前，而 `Commands` 枚举里刻意没有
//! `SandboxExec` 变体——一旦拦截失效，clap 会把它当未知子命令处理，用户拿到的
//! 是一段帮助文本和退出码 2，而不是 125。
//!
//! ## 三层用例，分层理由不同
//!
//! 1. **协议层**（全平台）：退出码 / stderr 首行 / stdout 纯净 / 一次性 stdin 配置。
//! 2. **执行层**（需要后端真可用；Unix `execvp` 后进程就是命令本身，Windows 是
//!    helper 中继子进程）：退出码透传、stdio 直通、TMPDIR 生效。
//! 3. **隔离层**（`cfg(unix)` + 后端可用）：**反假阳**是这一层的全部意义。断言
//!    「沙箱里写不了 X」单独看毫无价值：命令拼错了、路径不存在、shell 没起来，
//!    都会让它变绿。所以每条隔离断言都先跑一次**不带沙箱**的同一条命令并要求它
//!    **成功**，证明这个探针确实能分辨两种情况，然后才断言带沙箱那次失败。
//!    Windows 不参与这一层：那个后端按令牌与作业对象隔离，**没有路径过滤**
//!    （helper 会把每条 fs 规则报成 `UNENFORCEABLE(windows)`），所以"写不了 X"
//!    在那里本来就不成立，断言它只会是一条撒谎的绿。
//!
//! 后端不可用的机器（无 Landlock 的内核 / 越过性能门的 Windows）跳过 2、3 层
//! ——**不是恒绿，是没料可测**，跳过原因会打进测试输出。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const INIT_FAIL_PREFIX: &str = "__CRABCODE_SANDBOX_INIT_FAIL__:";
const INIT_FAIL_EXIT_CODE: i32 = 125;

fn crabcode_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_crabcode-pure-tui-launcher"))
}

/// 四表可定制的 v1 配置。JSON **手写字面量**而非结构体序列化：wire 名字漂了
/// 就得在这里当场红，而不是两边一起漂走。
fn config_json(
    cwd: &Path,
    tmp_dir: &Path,
    allow_write: &[String],
    deny_write: &[String],
) -> String {
    let json_list = |v: &[String]| {
        let items: Vec<String> = v.iter().map(|s| format!("{s:?}")).collect();
        format!("[{}]", items.join(", "))
    };
    format!(
        r#"{{
  "configVersion": 1,
  "fidelity": {{ "level": "full", "unenforced": [] }},
  "securityLevel": "allowlist",
  "cwd": {cwd:?},
  "tmpDir": {tmp:?},
  "filesystem": {{
    "allowRead": [],
    "allowWrite": {allow},
    "denyRead": [],
    "denyWrite": {deny}
  }},
  "network": {{
    "policy": "restricted",
    "allowedDomains": [],
    "deniedDomains": [],
    "allowUnixSockets": [],
    "allowAllUnixSockets": false,
    "allowLocalBinding": false,
    "httpProxyPort": 0,
    "socksProxyPort": 0
  }},
  "weaker": {{ "nestedSandbox": false, "networkIsolation": false }}
}}"#,
        cwd = cwd.to_string_lossy(),
        tmp = tmp_dir.to_string_lossy(),
        allow = json_list(allow_write),
        deny = json_list(deny_write),
    )
}

/// 一份**本平台**合法的 v1 配置（cwd 必须是绝对路径，`validate` 会查）。
fn valid_config_json(cwd: &Path, tmp_dir: &Path) -> String {
    config_json(cwd, tmp_dir, &[".".to_string()], &[])
}

fn run(args: &[&str]) -> Output {
    Command::new(crabcode_bin())
        .args(args)
        .output()
        .expect("crabcode binary runs")
}

fn run_with_config(config: &str, command_argv: &[String], cwd: &Path, verbose: bool) -> Output {
    let mut command = Command::new(crabcode_bin());
    command
        .args(["sandbox-exec", "--config-stdin", "--"])
        .args(command_argv)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if verbose {
        command.env("CRABCODE_SANDBOX_EXEC_VERBOSE", "1");
    }

    let mut child = command.spawn().expect("crabcode binary runs");
    child
        .stdin
        .take()
        .expect("sandbox helper stdin is piped")
        .write_all(config.as_bytes())
        .expect("write sandbox config to helper stdin");
    child.wait_with_output().expect("crabcode binary exits")
}

fn first_stderr_line(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// 本机后端可用吗？问的是**真二进制的真探测**，不是平台名。
///
/// 拿它跳过执行/隔离层用例是刻意的：在没有 Landlock 的内核上断言隔离生效，
/// 得到的只会是一条无从解释的红。
fn backend_available() -> bool {
    let out = run(&["sandbox-probe", "--json"]);
    if !out.status.success() {
        return false;
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(v) => v,
        Err(_) => return false,
    };
    parsed["backends"]["bash"]["available"] == serde_json::Value::Bool(true)
}

#[test]
fn sandbox_probe_fast_route_is_single_line_json() {
    let out = run(&["sandbox-probe", "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "stderr was: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stderr.is_empty());
    let stdout = String::from_utf8(out.stdout).expect("probe stdout is UTF-8");
    assert_eq!(stdout.lines().count(), 1);
    let payload: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("probe stdout is JSON");
    assert_eq!(payload["version"], serde_json::json!(1));
}

#[test]
fn malformed_probe_route_never_becomes_a_tui_prompt() {
    let out = run(&["sandbox-probe", "--unexpected"]);
    assert_eq!(out.status.code(), Some(64));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("CRABCODE_PURE_TUI_UNSUPPORTED:"));
}

/// 一条在本平台**一定存在**、打印 `hi` 到 stdout 的命令。
///
/// Windows 上 `echo` 是 cmd.exe 的内建，不是可执行文件 —— 直接把它交给
/// `CreateProcessAsUserW` 只会得到 ERROR_FILE_NOT_FOUND，或者更糟：命中某个
/// 恰好在 PATH 上的 `echo.exe`（本机就有一个来自 Git-Bash 的），于是测试的绿
/// 取决于开发机装了什么。用 `%COMSPEC% /C echo hi` 把这件事钉死。
fn echo_hi_argv() -> Vec<String> {
    if cfg!(windows) {
        let comspec = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string());
        vec![comspec, "/C".to_string(), "echo hi".to_string()]
    } else {
        vec!["echo".to_string(), "hi".to_string()]
    }
}

#[test]
fn config_from_private_stdin_reaches_the_execution_stage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config = valid_config_json(dir.path(), &std::env::temp_dir());
    let out = run_with_config(&config, &echo_hi_argv(), dir.path(), false);

    if backend_available() {
        // 执行侧接线后命令真的跑起来：Unix 上 helper 已经 `execvp` 成了它，
        // Windows 上 helper 还在，但 stdout 是子进程直写宿主句柄的。
        assert_eq!(
            out.status.code(),
            Some(0),
            "stderr was: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    } else {
        // 后端不可用（无 Landlock 的内核 / 越过性能门的 Windows）：必须走 125
        // 协议诚实失败，**绝不**悄悄裸跑一遍。
        assert_eq!(
            out.status.code(),
            Some(INIT_FAIL_EXIT_CODE),
            "stderr was: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let first = first_stderr_line(&out);
        assert!(
            first.starts_with(INIT_FAIL_PREFIX),
            "stderr first line must carry the failure diagnostic, got: {first}"
        );
        // stdout 属于被执行的命令 —— 命令没跑起来，helper 自己一个字节都不许写。
        assert!(
            out.stdout.is_empty(),
            "helper must never write to stdout, got: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
}

#[test]
fn malformed_argv_fails_by_the_same_protocol() {
    // 缺 `--` 分隔符。TS 拼错 argv 与沙箱起不来在用户那里是同一件事：
    // 命令没跑，且必须能被同一条识别逻辑认出来。
    let out = run(&["sandbox-exec", "--config-stdin", "echo"]);
    assert_eq!(out.status.code(), Some(INIT_FAIL_EXIT_CODE));
    assert_eq!(
        first_stderr_line(&out),
        format!("{INIT_FAIL_PREFIX}invalid-argv")
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn empty_config_stdin_fails_closed() {
    let out = run(&["sandbox-exec", "--config-stdin", "--", "echo"]);
    assert_eq!(out.status.code(), Some(INIT_FAIL_EXIT_CODE));
    assert_eq!(
        first_stderr_line(&out),
        format!("{INIT_FAIL_PREFIX}config-unreadable")
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn legacy_path_config_mode_is_rejected() {
    let out = run(&[
        "sandbox-exec",
        "--config",
        "/tmp/attacker-controlled.json",
        "--",
        "echo",
    ]);
    assert_eq!(out.status.code(), Some(INIT_FAIL_EXIT_CODE));
    assert_eq!(
        first_stderr_line(&out),
        format!("{INIT_FAIL_PREFIX}invalid-argv")
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn config_missing_a_security_field_is_rejected() {
    // E1 根治面的端到端证据：少一条规则 ⇒ 拒绝启动，而不是「当它是空的」。
    let dir = tempfile::tempdir().expect("tempdir");
    let full = valid_config_json(dir.path(), &std::env::temp_dir());
    let mut doc: serde_json::Value = serde_json::from_str(&full).expect("valid json");
    doc.get_mut("filesystem")
        .and_then(serde_json::Value::as_object_mut)
        .expect("filesystem object")
        .remove("denyWrite")
        .expect("denyWrite present");
    let out = run_with_config(
        &serde_json::to_string(&doc).expect("serialize"),
        &["echo".to_string()],
        dir.path(),
        false,
    );
    assert_eq!(out.status.code(), Some(INIT_FAIL_EXIT_CODE));
    assert_eq!(
        first_stderr_line(&out),
        format!("{INIT_FAIL_PREFIX}config-invalid")
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// 执行层 + 隔离层（Unix，PR-2）
//
// `execvp` 之后这个进程**就是**用户的命令。下面每一条都在验证那句话的一个推论。
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(unix)]
mod unix_execution {
    use super::*;
    use std::time::{Duration, Instant};

    /// 跑一条经沙箱的 `/bin/sh -c <script>`。返回 `None` = 本机没料可测。
    fn run_sandboxed(
        workspace: &Path,
        allow_write: &[String],
        deny_write: &[String],
        script: &str,
    ) -> Option<Output> {
        if !backend_available() {
            return None;
        }
        let config = config_json(workspace, &std::env::temp_dir(), allow_write, deny_write);
        Some(run_with_config(
            &config,
            &["/bin/sh".to_string(), "-c".to_string(), script.to_string()],
            workspace,
            false,
        ))
    }

    /// 同一条脚本**不经**沙箱直跑。反假阳的对照组。
    fn run_bare(workspace: &Path, script: &str) -> Output {
        Command::new("/bin/sh")
            .args(["-c", script])
            .current_dir(workspace)
            .output()
            .expect("/bin/sh runs")
    }

    fn skip(reason: &str) {
        eprintln!("SKIP: {reason}");
    }

    /// 单引号包裹 + 内部单引号转义。测试脚本自己拼 shell 也得拼对。
    fn shell_quote(path: &Path) -> String {
        let escaped = path.to_string_lossy().replace('\'', "'\\''");
        format!("'{escaped}'")
    }

    fn p95(samples: &mut [Duration]) -> Duration {
        samples.sort_unstable();
        let idx = (samples.len() * 95).div_ceil(100).saturating_sub(1);
        samples[idx.min(samples.len() - 1)]
    }

    #[test]
    fn exit_code_is_the_commands_own() {
        // helper `exec` 掉了自己，所以退出码不是"传播"的 —— 它就是命令的。
        // 7 刻意避开 125/126/127 这些有含义的码。
        let dir = tempfile::tempdir().expect("tempdir");
        let Some(out) = run_sandboxed(dir.path(), &[".".into()], &[], "exit 7") else {
            skip("sandbox backend unavailable on this host");
            return;
        };
        assert_eq!(
            out.status.code(),
            Some(7),
            "stderr was: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn stdout_and_stderr_reach_the_host_unchanged() {
        // 宿主给的三个 fd 原样是命令的。任何中继实现都会在这里露馅
        // （多一层缓冲、丢一段尾巴、把 stderr 并进 stdout）。
        let dir = tempfile::tempdir().expect("tempdir");
        let Some(out) = run_sandboxed(
            dir.path(),
            &[".".into()],
            &[],
            "printf 'to-out'; printf 'to-err' >&2",
        ) else {
            skip("sandbox backend unavailable on this host");
            return;
        };
        assert_eq!(String::from_utf8_lossy(&out.stdout), "to-out");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("to-err"),
            "stderr must carry the command's own bytes, got: {stderr}"
        );
        // 命令跑起来了 ⇒ 失败协议一个字节都不该出现。
        assert!(
            !stderr.contains(INIT_FAIL_PREFIX),
            "a successful run must not emit the init-fail protocol"
        );
    }

    #[test]
    fn tmpdir_inside_the_sandbox_is_the_one_the_config_declared() {
        // `tmpDir` 一旦写进配置就是一句承诺：提示词告诉模型 `$TMPDIR` 可写。
        // 承诺一个沙箱外的目录 = 模型按提示写文件，然后被拒。
        let dir = tempfile::tempdir().expect("tempdir");
        let Some(out) = run_sandboxed(dir.path(), &[".".into()], &[], "printf '%s' \"$TMPDIR\"")
        else {
            skip("sandbox backend unavailable on this host");
            return;
        };
        let seen = String::from_utf8_lossy(&out.stdout).to_string();
        assert_eq!(
            Path::new(&seen),
            std::env::temp_dir().as_path(),
            "TMPDIR inside the sandbox must be the declared tmpDir"
        );
    }

    #[test]
    fn writing_inside_the_workspace_still_works() {
        // 隔离的另一半：该放行的必须真放行。只测"写不了"会让一个把所有东西都
        // 拒掉的沙箱看起来完美。
        let dir = tempfile::tempdir().expect("tempdir");
        let Some(out) = run_sandboxed(
            dir.path(),
            &[".".into()],
            &[],
            "printf 'ok' > inside.txt && cat inside.txt",
        ) else {
            skip("sandbox backend unavailable on this host");
            return;
        };
        assert_eq!(
            out.status.code(),
            Some(0),
            "workspace must stay writable; stderr was: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), "ok");
    }

    #[test]
    fn writing_outside_every_allowed_path_is_refused() {
        // **反假阳**：先证明这条探针能分辨两种情况（不带沙箱时它必须成功），
        // 再断言带沙箱时它失败。少了对照组，这条用例在"命令根本没跑起来"时
        // 也会变绿 —— 那正是本立项要消灭的那种"看起来有沙箱"。
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            skip("no HOME on this host");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let target = home.join(format!(
            ".crabcode-sandbox-smoke-{}.tmp",
            std::process::id()
        ));
        let script = format!("printf 'x' > {}", shell_quote(&target));

        let bare = run_bare(dir.path(), &script);
        let bare_ok = bare.status.success();
        let _ = std::fs::remove_file(&target);
        if !bare_ok {
            skip("HOME is not writable — the probe cannot tell the two cases apart");
            return;
        }

        let Some(out) = run_sandboxed(dir.path(), &[".".into()], &[], &script) else {
            skip("sandbox backend unavailable on this host");
            return;
        };
        let created = target.exists();
        let _ = std::fs::remove_file(&target);
        assert!(
            !out.status.success() && !created,
            "a path outside every allowWrite entry must not be writable inside the sandbox \
             (exit={:?}, created={created}, stderr={})",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// denyWrite 落在 allowWrite 内部 —— 生产里最常见的形态
    /// （allowWrite `.` + denyWrite `<cwd>/.crabcode/settings.json`）。
    ///
    /// **两个平台的正确行为不同，本用例分别钉住各自的那个**：
    /// - macOS：SBPL 支持 deny，施加层还会在运行期用 `sandbox_check` 实证 ⇒
    ///   必须真的写不了。
    /// - Linux：Landlock 是纯 allow-grant 模型，在放行的子树里挖不了洞 ⇒
    ///   写得进去，但**必须报出来**（`CRABCODE_SANDBOX_EXEC_VERBOSE=1` 时可见）。
    ///
    /// 把两边写成同一个期望值只能靠削平其中一边的真相，那才是 drift。
    #[test]
    fn deny_write_inside_an_allowed_subtree_is_either_enforced_or_reported() {
        if !backend_available() {
            skip("sandbox backend unavailable on this host");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let denied = dir.path().join("secret.txt");
        let script = format!("printf 'x' > {}", shell_quote(&denied));

        let config = config_json(
            dir.path(),
            &std::env::temp_dir(),
            &[".".to_string()],
            &[denied.to_string_lossy().into_owned()],
        );
        let out = run_with_config(
            &config,
            &["/bin/sh".to_string(), "-c".to_string(), script.clone()],
            dir.path(),
            true,
        );
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();

        if cfg!(target_os = "macos") {
            assert!(
                !out.status.success() && !denied.exists(),
                "macOS Seatbelt must enforce denyWrite inside an allowed subtree \
                 (exit={:?}, stderr={stderr})",
                out.status.code()
            );
        } else {
            assert!(
                stderr.contains("UNENFORCEABLE(linux)"),
                "Linux cannot carve a deny hole inside a granted subtree — it must say so \
                 instead of pretending. stderr was: {stderr}"
            );
        }
    }

    /// helper 的额外开销必须小到没人会为了省它而关掉沙箱。
    ///
    /// 门是 **p95 ≤ 150ms**（SoT §4-E4）。E4 的教训就是这条：Windows 上
    /// 46s/次 spawn 之所以能潜伏，正因为只有一个用例断言过耗时。
    #[test]
    fn helper_overhead_stays_under_the_budget() {
        const RUNS: usize = 20;
        const BUDGET_MS: u128 = 150;

        if !backend_available() {
            skip("sandbox backend unavailable on this host");
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");

        let mut baseline: Vec<Duration> = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let t = Instant::now();
            let _ = run_bare(dir.path(), "exit 0");
            baseline.push(t.elapsed());
        }

        let mut sandboxed: Vec<Duration> = Vec::with_capacity(RUNS);
        for _ in 0..RUNS {
            let t = Instant::now();
            let out = run_sandboxed(dir.path(), &[".".into()], &[], "exit 0")
                .expect("backend was available a moment ago");
            assert_eq!(out.status.code(), Some(0));
            sandboxed.push(t.elapsed());
        }

        let bare_p95 = p95(&mut baseline);
        let sandboxed_p95 = p95(&mut sandboxed);
        let overhead = sandboxed_p95.saturating_sub(bare_p95);
        assert!(
            overhead.as_millis() <= BUDGET_MS,
            "sandbox helper p95 overhead {}ms exceeds the {BUDGET_MS}ms budget \
             (bare p95 {}ms, sandboxed p95 {}ms)",
            overhead.as_millis(),
            bare_p95.as_millis(),
            sandboxed_p95.as_millis()
        );
    }
}

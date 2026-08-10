//! `crabcode sandbox-exec --config-stdin -- <program> [args…]`
//! —— per-command 沙箱助手（W-SANDBOX-ENFORCED-DEADCODE PR-1 配置管道 /
//! PR-2 Unix 执行 / PR-3 Windows 执行）。
//!
//! 形态（SoT §2.1）：`Shell.ts::exec` 把 argv 前缀化成
//! `crabcode sandbox-exec --config-stdin -- <binShell> <args…>`；宿主把配置写进
//! helper 的一次性 stdin → 施加隔离 → `execvp`（Unix，exec 后进程就是命令本身，进程树与
//! 今日完全同构）/ spawn-child + Job（Windows，没有 `exec` 可用，见
//! [`run_windows`]）。
//!
//! ## stdout 是命令的，不是我们的
//!
//! `execvp` 之后这个进程**就是**用户的命令，它的 stdout 属于命令；Windows 上
//! 子进程直接拿到宿主那三个句柄，同样如此。所以 helper 自身**永远不往 stdout
//! 写一个字节**，诊断一律 stderr。一行"友好提示"在这里就是混进命令输出里的
//! 脏字节，而出问题时没人会想到来这里找。
//!
//! ## 运行期失败协议（与 TS 的合同，SoT §2.3）
//!
//! - 退出码 [`SANDBOX_INIT_FAIL_EXIT_CODE`] = 125
//! - stderr 标记行 = [`SANDBOX_INIT_FAIL_PREFIX`] + 原因 slug；它**之前**只允许出现以
//!   [`WARNING_LINE_PREFIX`] / [`NOTICE_LINE_PREFIX`] 开头的 helper 自诊断行
//!
//! 该协议只为当前命令提供可读诊断，绝不能改变宿主会话的安全状态：退出码和
//! stderr 最终都可由不可信命令伪造。宿主**绝不静默重跑无沙箱版**；人类可读的
//! 细节写在标记后的独立行。

use std::io::Read;
use std::path::Path;

use acosmi_sandbox::exec_config::SandboxExecConfigV1;

/// 沙箱初始化失败的退出码。与 TS 侧识别逻辑是同一份合同。
///
/// 选 125 是因为它落在 shell 保留区（126=不可执行 / 127=找不到命令）之外、
/// 又不与常见业务码冲突；GNU `env` 用它表示"env 自己失败了"，语义同构。
pub const SANDBOX_INIT_FAIL_EXIT_CODE: i32 = 125;

/// stderr 首行前缀。只用于当前命令的人类可读诊断；宿主不得据此改变安全状态。
pub const SANDBOX_INIT_FAIL_PREFIX: &str = "__CRABCODE_SANDBOX_INIT_FAIL__:";

/// helper 自诊断行的两个前缀 —— **跨语言合同的一部分**，不是排版糖。
///
/// Unix 执行路径必须在 `execvp` 之前把话说完（exec 之后这个进程就不是我们了），
/// Windows 同样要在子进程接管 stderr 前输出，所以 warning / notice 可能排在失败
/// 标记之前。它们不参与宿主侧鉴权或状态迁移。
pub const WARNING_LINE_PREFIX: &str = "crabcode sandbox-exec: warning: ";
/// 见 [`WARNING_LINE_PREFIX`]。
pub const NOTICE_LINE_PREFIX: &str = "crabcode sandbox-exec: notice: ";

/// 执行侧在本平台没有实现（Unix / Windows 之外的目标）。
///
/// 与探测侧的 `platform::PROBE_REASON_PLATFORM_UNSUPPORTED` 刻意不是同一个
/// 字符串：那个回答"这个后端能用吗"，这个回答"这次执行为什么失败"。两个问题
/// 共用一个 slug 会让日志里再也分不清是探测漏了还是执行崩了。
#[cfg_attr(any(unix, windows), allow(dead_code))]
pub const REASON_EXEC_NOT_IMPLEMENTED: &str = "exec-not-implemented";

// 下面三个里有两个只在 Unix 执行路径上有使用点，但**不能因此只在 unix 下定义**：
// `failure_protocol_constants_*` 单测要在每个平台上钉住这份合同的形状，而一个
// 只在半数平台存在的合同没法被钉。
/// 隔离施加失败（landlock / seccomp / seatbelt 任一环报错；Windows 上是令牌 /
/// 作业对象 / 子进程创建任一环报错）。
pub const REASON_SANDBOX_APPLY_FAILED: &str = "sandbox-apply-failed";
#[cfg_attr(not(unix), allow(dead_code))]
/// 隔离施加"成功"但 canary 证不出它真的生效 —— 静默零隔离的唯一防线。
///
/// Windows 上同名的失败走 [`REASON_SANDBOX_APPLY_FAILED`]：那边的 canary
/// （`IsProcessInJob`）在子进程还挂起时就问完了，证不出来就没有"施加成功"这个
/// 中间状态可言 —— 整件事一起失败，分成两个 slug 只会假装出一个不存在的阶段。
pub const REASON_SANDBOX_VERIFY_FAILED: &str = "sandbox-verify-failed";
#[cfg_attr(not(unix), allow(dead_code))]
/// `execvp` 返回了 —— 它只在失败时返回，所以命令**从未运行过**。
pub const REASON_EXEC_FAILED: &str = "exec-failed";

/// argv 形态不合法（缺 `--config-stdin` / 缺 `--` / `--` 后没有程序）。
pub const REASON_INVALID_ARGV: &str = "invalid-argv";
/// stdin 配置读不出来（空输入 / I/O / 超限 / 非 UTF-8）。
pub const REASON_CONFIG_UNREADABLE: &str = "config-unreadable";
/// 配置文件读到了但不是一份合法 v1 文档（缺字段 / 未知字段 / 版本不符）。
pub const REASON_CONFIG_INVALID: &str = "config-invalid";

/// 解析好的一次调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxExecInvocation {
    pub program: String,
    pub args: Vec<String>,
}

/// 解析 `sandbox-exec` 之后的 token。
///
/// **只认一种形态**：`--config-stdin -- <program> [args…]`。不接受路径模式、
/// 不接受 flag 换序、不接受 `--` 之前出现其它 token。
/// 唯一的调用方是 TS（`Shell.ts` 按固定模板拼 argv），宽容度在这里只会变成
/// 歧义面：把用户命令里的某个 token 误读成 helper 的 flag，后果是隔离参数被
/// 篡改。多一种写法 = 多一条能被误读的路径。
pub fn parse_sandbox_exec_argv<I>(tokens: I) -> Result<SandboxExecInvocation, String>
where
    I: IntoIterator<Item = String>,
{
    let mut it = tokens.into_iter();

    match it.next().as_deref() {
        Some("--config-stdin") => {}
        Some(other) => {
            return Err(format!(
                "expected `--config-stdin` as the first argument, got `{other}`"
            ));
        }
        None => return Err("expected `--config-stdin -- <program> [args…]`".to_string()),
    }

    match it.next().as_deref() {
        Some("--") => {}
        Some(other) => {
            return Err(format!(
                "expected `--` after `--config-stdin`, got `{other}`"
            ));
        }
        None => return Err("expected `--` followed by the program to run".to_string()),
    }

    let program = it
        .next()
        .ok_or_else(|| "expected a program after `--`".to_string())?;
    if program.is_empty() {
        return Err("the program after `--` must not be empty".to_string());
    }

    Ok(SandboxExecInvocation {
        program,
        args: it.collect(),
    })
}

/// 打印失败协议：**首行恒为** `前缀+slug`，然后是攒下的 warning 与细节。
///
/// 本函数攒下的 warning 排在标记行**之后**，不一产生就打——这是本函数形状的
/// 由来（那次实测红：清理失败的 warning 曾把标记挤到第二行）。
///
/// 但这条纪律**只管得住本函数**：[`run_unix`] 必须在 `execvp` 之前把 warning /
/// notice 说完（exec 之后这个进程就不是我们了，再没有第二次机会），
/// [`run_windows`] 同理要在子进程接管 stderr 之前说完。所以「标记恒为首行」在
/// 全局并不成立，真正的合同是**双侧共同承担**的更弱形态：
///
/// 标记与诊断只描述当前 helper 失败，不是可信的跨进程控制协议。
fn fail_with(reason: &str, detail: &str, warnings: &[String]) -> ! {
    eprintln!("{SANDBOX_INIT_FAIL_PREFIX}{reason}");
    for warning in warnings {
        eprintln!("{WARNING_LINE_PREFIX}{warning}");
    }
    eprintln!("crabcode sandbox-exec: {detail}");
    std::process::exit(SANDBOX_INIT_FAIL_EXIT_CODE)
}

/// 无 warning 可攒时的简写。
fn fail(reason: &str, detail: &str) -> ! {
    fail_with(reason, detail, &[])
}

/// Bound the private control pipe so a malformed direct invocation cannot make
/// the helper allocate without limit. The ordinary config is only a few KiB.
const MAX_CONFIG_BYTES: u64 = 8 * 1024 * 1024;

fn read_config_from_stdin() -> std::io::Result<String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .lock()
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "sandbox-exec config stdin is empty",
        ));
    }
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "sandbox-exec config exceeds the 8 MiB limit",
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Prove that the directory used to resolve the plan's filesystem rules is
/// also the directory in which the command will execute.
///
/// The config and the process spawn are two channels. Comparing their
/// canonical identities prevents a drifted caller from declaring one `cwd`
/// while inheriting another. This check happens before applying any
/// irreversible sandbox restriction.
fn validate_inherited_working_directory(declared: &Path) -> Result<(), String> {
    let inherited = std::env::current_dir()
        .map_err(|error| format!("could not resolve the helper working directory: {error}"))?;
    let inherited = inherited.canonicalize().map_err(|error| {
        format!(
            "could not canonicalize the helper working directory {}: {error}",
            inherited.display()
        )
    })?;
    let declared = declared.canonicalize().map_err(|error| {
        format!(
            "could not canonicalize the declared cwd {}: {error}",
            declared.display()
        )
    })?;

    let matches = if cfg!(windows) {
        inherited
            .to_string_lossy()
            .eq_ignore_ascii_case(&declared.to_string_lossy())
    } else {
        inherited == declared
    };
    if !matches {
        return Err(format!(
            "declared cwd {} does not match the helper working directory {}",
            declared.display(),
            inherited.display()
        ));
    }
    Ok(())
}

/// 执行一次 `sandbox-exec`。永不返回。
pub fn run_sandbox_exec(invocation: &SandboxExecInvocation) -> ! {
    let warnings: Vec<String> = Vec::new();

    let raw = match read_config_from_stdin() {
        Ok(raw) => raw,
        Err(e) => fail_with(
            REASON_CONFIG_UNREADABLE,
            &format!("could not read config from stdin: {e}"),
            &warnings,
        ),
    };

    let config = match SandboxExecConfigV1::parse(&raw) {
        Ok(config) => config,
        Err(e) => fail_with(REASON_CONFIG_INVALID, &format!("{e}"), &warnings),
    };
    let plan = match config.into_plan(invocation.program.clone(), invocation.args.clone()) {
        Ok(plan) => plan,
        Err(e) => fail_with(REASON_CONFIG_INVALID, &format!("{e}"), &warnings),
    };

    if let Err(e) = plan.validate() {
        fail_with(REASON_CONFIG_INVALID, &format!("{e}"), &warnings);
    }
    if let Err(detail) = validate_inherited_working_directory(&plan.base.workspace) {
        fail_with(REASON_CONFIG_INVALID, &detail, &warnings);
    }

    // ── 到这里为止，隔离规则已经完整抵达并通过校验 ────────────────────────
    #[cfg(unix)]
    {
        run_unix(&plan, warnings)
    }

    #[cfg(windows)]
    {
        run_windows(&plan, warnings)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = &plan;
        fail_with(
            REASON_EXEC_NOT_IMPLEMENTED,
            "the sandbox execution backend is not wired on this platform",
            &warnings,
        )
    }
}

/// Unix 执行路径：准备 → 施加隔离 → canary → `execvp`。永不正常返回。
///
/// ## 为什么是 `execvp` 而不是 spawn + wait
///
/// `exec` 之后**这个进程就是用户的命令**：pid 不变、stdio 就是宿主给的那三个
/// fd、进程组与 detached 语义原样保留。于是宿主侧的 tree-kill / abort /
/// timeout / TaskOutput 文件落盘全部**按构造继承**，一行都不用改（SoT §3）。
/// 换成 spawn + wait 就得自己重实现转发、信号中继与退出码传播，每一处都是新的
/// 出错面 —— 而 Windows 之所以必须走那条路（PR-3），恰恰是因为它没有 `exec`。
///
/// ## 顺序不可换
///
/// 1. **argv 先建**：`CString` 分配可能失败，而失败在施加隔离**之后**发生就
///    只能死在一个已经没有退路的进程里。
/// 2. **TMPDIR 再设**：seatbelt profile 与 landlock 规则都从 env 读 `TMPDIR`，
///    设晚了就是给沙箱开了一个它自己都不认识的临时目录。
/// 3. **施加 → canary → exec**：canary 是"施加是 no-op"的唯一防线
///    （`verify_sandbox_active` 的 crate 自带纪律）。
#[cfg(unix)]
fn run_unix(plan: &acosmi_sandbox::exec_config::SandboxExecPlan, mut warnings: Vec<String>) -> ! {
    use std::ffi::CString;

    // 1. argv —— 在任何不可逆操作之前构造完毕。
    let mut argv_owned: Vec<CString> = Vec::with_capacity(plan.base.args.len() + 1);
    for token in std::iter::once(&plan.base.command).chain(plan.base.args.iter()) {
        match CString::new(token.as_str()) {
            Ok(c) => argv_owned.push(c),
            Err(_) => fail_with(
                REASON_INVALID_ARGV,
                &format!("argument contains a null byte: {token:?}"),
                &warnings,
            ),
        }
    }

    // 2. TMPDIR —— 沙箱内的临时目录由隔离规则决定，不由宿主环境决定。
    // SAFETY: 单线程上下文（helper 在此之前没有起过任何线程），`set_var` 的
    // 数据竞争前提不成立。
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("TMPDIR", &plan.tmp_dir);
    }

    // 3. 施加隔离。失败 = 这条命令不能跑，**不是**"那就裸跑吧"。
    let report = match acosmi_sandbox::apply_exec_plan_to_self(plan) {
        Ok(report) => report,
        Err(e) => fail_with(
            REASON_SANDBOX_APPLY_FAILED,
            &format!("could not apply the sandbox: {e}"),
            &warnings,
        ),
    };
    warnings.extend(report.warnings);

    // 4. canary：施加"成功"不等于隔离生效。
    if let Err(e) = acosmi_sandbox::verify_sandbox_active(plan.base.security_level) {
        fail_with(
            REASON_SANDBOX_VERIFY_FAILED,
            &format!("the sandbox reported success but the canary disagrees: {e}"),
            &warnings,
        );
    }

    // 5. 攒下的话在 exec **之前**说完 —— exec 之后这个进程就不是我们了。
    for warning in &warnings {
        eprintln!("{WARNING_LINE_PREFIX}{warning}");
    }
    if !report.notices.is_empty()
        && std::env::var_os(acosmi_sandbox::platform::SANDBOX_EXEC_VERBOSE_ENV).is_some()
    {
        for notice in &report.notices {
            eprintln!("{NOTICE_LINE_PREFIX}{notice}");
        }
    }

    // 6. execvp —— PATH 解析，成功即不返回。
    let mut argv_ptrs: Vec<*const libc::c_char> = argv_owned.iter().map(|c| c.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    // SAFETY: `argv_owned` 在整个调用期间存活并拥有全部 C 串；`argv_ptrs` 以
    // NULL 结尾，形态即 `execvp` 要求的 `char *const argv[]`。
    #[allow(unsafe_code)]
    unsafe {
        libc::execvp(argv_ptrs[0], argv_ptrs.as_ptr());
    }

    // execvp 只在失败时返回 —— 命令**从未运行过**，所以这不是命令的失败，
    // 是我们的失败，必须走 125 协议而不是伪造一个退出码。
    let errno = std::io::Error::last_os_error();
    fail_with(
        REASON_EXEC_FAILED,
        &format!("could not exec `{}`: {errno}", plan.base.command),
        &[],
    )
}

/// Windows 执行路径：起一个受沙箱约束的子进程，中继它的退出码。永不正常返回。
///
/// ## 为什么这里不是 `execvp`
///
/// Windows 没有 `exec`，也不允许进程在启动后收紧自己的令牌 —— 沙箱只能施加在
/// **创建时刻**。于是 helper 没法变成命令本身，只能活着当中继。代价是宿主看到
/// 的 pid 是 helper 的，好处是这件事**只在这里**需要被知道：
///
/// - stdio 直接把宿主的三个句柄交给子进程，不经中转，所以 `TaskOutput` 落盘 /
///   截断 / 流式全部照旧；
/// - 退出码原样传播；
/// - helper 的寿命 = Job 的寿命（`KILL_ON_JOB_CLOSE`），所以宿主杀 helper 就是
///   杀整棵树 —— 不需要转发任何信号，也就没有一条会转发错的路径。
///
/// ## 顺序不可换
///
/// 1. **notices 先攒完**：施加成功与否都要留下"哪些规则没兑现"的名字，而配置
///    文件此刻已经被删了，这是最后一次能说清楚的机会。
/// 2. **话在 spawn 之前说完**：子进程一起来，stderr 就是它的了；我们的诊断再
///    往里写就是混进命令输出的脏字节。
/// 3. **exit 用子进程的码**：不是"我们成功了"的 0。
#[cfg(windows)]
fn run_windows(plan: &acosmi_sandbox::exec_config::SandboxExecPlan, warnings: Vec<String>) -> ! {
    // 攒下的话在起子进程**之前**说完。
    for warning in &warnings {
        eprintln!("{WARNING_LINE_PREFIX}{warning}");
    }

    let outcome = match acosmi_sandbox::run_exec_plan_as_child(plan) {
        Ok(outcome) => outcome,
        Err(e) => fail_with(
            REASON_SANDBOX_APPLY_FAILED,
            &format!("could not run the command under the sandbox: {e}"),
            &[],
        ),
    };

    // notices 只在显式开关下打印 —— 它们对同一个会话里每条命令都一样，默认进
    // stderr 就等于把同样几行注入模型每一次的工具结果。见 `ExecPlanReport`。
    if !outcome.report.notices.is_empty()
        && std::env::var_os(acosmi_sandbox::platform::SANDBOX_EXEC_VERBOSE_ENV).is_some()
    {
        for notice in &outcome.report.notices {
            eprintln!("{NOTICE_LINE_PREFIX}{notice}");
        }
    }
    for warning in &outcome.report.warnings {
        eprintln!("{WARNING_LINE_PREFIX}{warning}");
    }

    std::process::exit(outcome.exit_code)
}

/// 快路径入口：解析 argv 尾巴，失败即按失败协议退出。永不返回。
pub fn dispatch(tokens: Vec<String>) -> ! {
    match parse_sandbox_exec_argv(tokens) {
        Ok(invocation) => run_sandbox_exec(&invocation),
        Err(detail) => fail(REASON_INVALID_ARGV, &detail),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn argv(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn canonical_form_parses() {
        let parsed = parse_sandbox_exec_argv(argv(&[
            "--config-stdin",
            "--",
            "/bin/bash",
            "-c",
            "echo hi",
        ]))
        .unwrap();
        assert_eq!(parsed.program, "/bin/bash");
        assert_eq!(parsed.args, vec!["-c".to_string(), "echo hi".to_string()]);
    }

    #[test]
    fn program_without_args_parses() {
        let parsed = parse_sandbox_exec_argv(argv(&["--config-stdin", "--", "true"])).unwrap();
        assert_eq!(parsed.program, "true");
        assert!(parsed.args.is_empty());
    }

    #[test]
    fn tokens_after_the_separator_are_never_reinterpreted_as_our_flags() {
        // 命令自己带 `--config` / `--` 时必须原样落进 args——否则用户的一个
        // 普通参数就能改写隔离配置的来源。
        let parsed = parse_sandbox_exec_argv(argv(&[
            "--config-stdin",
            "--",
            "git",
            "log",
            "--",
            "--config",
            "x",
        ]))
        .unwrap();
        assert_eq!(parsed.program, "git");
        assert_eq!(
            parsed.args,
            vec![
                "log".to_string(),
                "--".to_string(),
                "--config".to_string(),
                "x".to_string()
            ]
        );
    }

    #[test]
    fn missing_config_stdin_flag_is_rejected() {
        assert!(parse_sandbox_exec_argv(argv(&["--", "/bin/bash"])).is_err());
        assert!(parse_sandbox_exec_argv(argv(&[])).is_err());
    }

    #[test]
    fn legacy_path_config_forms_are_rejected() {
        assert!(parse_sandbox_exec_argv(argv(&["--config"])).is_err());
        assert!(
            parse_sandbox_exec_argv(argv(&["--config", "/tmp/cfg.json", "--", "true"])).is_err()
        );
        assert!(parse_sandbox_exec_argv(argv(&["--config=/tmp/c.json", "--", "true"])).is_err());
    }

    #[test]
    fn extra_token_before_separator_is_rejected() {
        assert!(parse_sandbox_exec_argv(argv(&["--config-stdin", "extra", "--", "true"])).is_err());
    }

    #[test]
    fn missing_separator_is_rejected() {
        assert!(parse_sandbox_exec_argv(argv(&["--config-stdin"])).is_err());
        assert!(parse_sandbox_exec_argv(argv(&["--config-stdin", "/bin/bash"])).is_err());
    }

    #[test]
    fn empty_program_is_rejected() {
        assert!(parse_sandbox_exec_argv(argv(&["--config-stdin", "--"])).is_err());
        assert!(parse_sandbox_exec_argv(argv(&["--config-stdin", "--", ""])).is_err());
    }

    #[test]
    fn declared_cwd_must_match_the_inherited_working_directory() {
        let inherited = std::env::current_dir().unwrap();
        validate_inherited_working_directory(&inherited).unwrap();

        let other = tempfile::tempdir().unwrap();
        assert!(validate_inherited_working_directory(other.path()).is_err());
    }

    #[test]
    fn helper_diagnostic_line_prefixes_are_unambiguous() {
        // 这些前缀只为当前命令提供可读诊断，宿主不得据此改变会话安全状态。
        assert_eq!(WARNING_LINE_PREFIX, "crabcode sandbox-exec: warning: ");
        assert_eq!(NOTICE_LINE_PREFIX, "crabcode sandbox-exec: notice: ");
        // 标记前缀不能是自诊断前缀的前缀（反之亦然）—— 否则跳过循环会把标记行
        // 本身当成一条 warning 跳掉。
        assert!(!WARNING_LINE_PREFIX.starts_with(SANDBOX_INIT_FAIL_PREFIX));
        assert!(!NOTICE_LINE_PREFIX.starts_with(SANDBOX_INIT_FAIL_PREFIX));
        assert!(!SANDBOX_INIT_FAIL_PREFIX.starts_with(WARNING_LINE_PREFIX));
        assert!(!SANDBOX_INIT_FAIL_PREFIX.starts_with(NOTICE_LINE_PREFIX));
        // 两个自诊断前缀必须互不为前缀，否则 notice 会被当成 warning 记账。
        assert!(!WARNING_LINE_PREFIX.starts_with(NOTICE_LINE_PREFIX));
        assert!(!NOTICE_LINE_PREFIX.starts_with(WARNING_LINE_PREFIX));
    }

    #[test]
    fn failure_protocol_constants_are_the_contract_ts_reads() {
        // 这两个值改了就是改合同 —— TS 侧识别逻辑必须同 PR 跟着改。
        assert_eq!(SANDBOX_INIT_FAIL_EXIT_CODE, 125);
        assert_eq!(SANDBOX_INIT_FAIL_PREFIX, "__CRABCODE_SANDBOX_INIT_FAIL__:");
        // 首行必须是「前缀 + slug」，中间不许有空格/冒号之外的装饰。
        let all = [
            REASON_EXEC_NOT_IMPLEMENTED,
            REASON_INVALID_ARGV,
            REASON_CONFIG_UNREADABLE,
            REASON_CONFIG_INVALID,
            REASON_SANDBOX_APPLY_FAILED,
            REASON_SANDBOX_VERIFY_FAILED,
            REASON_EXEC_FAILED,
        ];
        for reason in all {
            assert!(!reason.is_empty());
            assert!(
                !reason.contains(char::is_whitespace),
                "reason slug `{reason}` must be a single token"
            );
        }
        // slug 必须两两不同 —— 两个不同的失败共用一个名字，就等于日志里再也
        // 分不清是配置坏了还是内核拒了。
        let mut sorted = all;
        sorted.sort_unstable();
        let unique = {
            let mut v = sorted.to_vec();
            v.dedup();
            v.len()
        };
        assert_eq!(unique, all.len(), "reason slugs must be pairwise distinct");
    }
}

//! Acosmi Daemon Launcher — 通用 daemon 化封装
//!
//! 由 `acosmi-cron-launcher` (Phase 5 P1-A, 2026-05-06) 泛化而来；当前产品
//! 消费者是 `crabcode-cron`。所有运行期文件按 daemon `name` 派生：
//!
//! - `~/.crabcode/run/<name>.lock` flock(LOCK_EX|LOCK_NB) 互斥（多
//!   supervisor / 多 CLI 并发拉起仅一胜出）
//! - double-fork + setsid + close 继承 fd → 让 init/launchd 收养
//! - `~/.crabcode/run/<name>.pid` 由 daemon 自己写入；launcher 通过
//!   `kill -0` + 内容校验判活
//!
//! `ensure_running(name, socket_path, binary)` 是唯一公开入口。

#![allow(
    unsafe_code,
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::similar_names,
    clippy::manual_c_str_literals,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::doc_markdown,
    clippy::implicit_hasher,
    clippy::ref_as_ptr,
    clippy::borrow_as_ptr,
    clippy::map_unwrap_or
)]

/// Step 2 Phase D.8 — local mirror of `acosmi_supervisor::silent_drop!`.
///
/// The macro is defined here rather than imported from `acosmi-supervisor`
/// to keep `acosmi-daemon-launcher`'s dependency graph minimal (this crate
/// is a tiny standalone crate; pulling in the full supervisor — tokio with
/// `full` features, watchdog, executor, sandbox — just for one macro would
/// inflate build time and binary size for no behavioural reason). Behaviour
/// is byte-identical to the supervisor version.
///
/// Use to declare an *intentional* discard of a value with a one-line reason
/// (typically a `Result` from a fire-and-forget operation that the caller
/// has decided silence is correct for). Reviewers can read the reason at the
/// call site without re-deriving why the silent discard is OK.
#[macro_export]
macro_rules! silent_drop {
    ($expr:expr, $reason:literal) => {{
        // The reason is at the call site for reviewers.
        let _ = $expr;
    }};
}

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

// pub：spawn 互斥的并发语义由 tests/socket_lock_concurrent.rs 跨平台集成
// 验证（P0-2）。launcher 之外的生产代码不应直接用它。
pub mod socket_lock;
#[cfg(unix)]
mod spawn_unix;
#[cfg(windows)]
mod spawn_windows;
#[cfg(windows)]
pub mod windows_event;
#[cfg(windows)]
pub mod windows_pipe;

pub mod build_id;
pub mod cron;
pub mod paths;
pub mod state_identity;

#[derive(Debug, Error)]
pub enum LauncherError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("daemon binary not found: {0}")]
    BinaryMissing(PathBuf),
    #[error("daemon failed to write PID file within {0:?}")]
    PidFileTimeout(Duration),
    #[error("lock 被持有但 PID 文件未出现：另一 launcher spawn 失败？")]
    LockOrphaned,
    #[error("spawn failed: {0}")]
    SpawnFailed(String),
    #[error("nix: {0}")]
    #[cfg(unix)]
    Nix(#[from] nix::Error),
}

pub type Result<T> = std::result::Result<T, LauncherError>;

/// Daemon 化后的进程句柄。
#[derive(Debug, Clone)]
pub struct DaemonHandle {
    pub pid: u32,
    pub sock_path: String,
    pub pid_file: PathBuf,
}

/// Fully structured detached-process request.
///
/// This is the low-level primitive for daemons whose transport requires exact
/// environment and logging ownership. It never invokes a shell and inherits
/// the caller environment except for explicit replacements.
#[derive(Debug, Clone)]
pub struct DetachedCommand {
    pub binary: PathBuf,
    pub args: Vec<OsString>,
    /// Exact environment entries that replace inherited values for the
    /// detached child.
    pub env_overrides: Vec<(OsString, OsString)>,
    /// When present, stdout and stderr append to this file. Otherwise both
    /// streams are redirected to the platform null device.
    pub log_file: Option<PathBuf>,
}

/// Spawn a detached daemon from exact argv elements.
///
/// Unix uses the existing double-fork + `setsid` path; Windows requires
/// `CREATE_BREAKAWAY_FROM_JOB` and fails closed if the host job forbids it.
/// A successful return only proves launcher handoff. Callers must still wait
/// for their transport endpoint and report the log tail on failure.
pub fn spawn_detached_command(command: &DetachedCommand) -> Result<()> {
    if !command.binary.exists() {
        return Err(LauncherError::BinaryMissing(command.binary.clone()));
    }
    #[cfg(unix)]
    {
        spawn_unix::daemonize_command(
            &command.binary,
            &command.args,
            &command.env_overrides,
            command.log_file.as_deref(),
        )
    }
    #[cfg(windows)]
    {
        spawn_windows::spawn_command(
            &command.binary,
            &command.args,
            &command.env_overrides,
            command.log_file.as_deref(),
        )
    }
}

/// 等待 PID 文件出现 + 内容可读 + `kill -0` 通过的最大时长。
const PID_FILE_WAIT: Duration = Duration::from_secs(5);
/// PID 文件轮询间隔。
const PID_FILE_POLL: Duration = Duration::from_millis(50);
/// F1 代际换代：优雅 stop 旧代 daemon 的默认超时（复用既有 `stop()` 的
/// SIGTERM+poll / Windows soft-stop+TerminateProcess 兜底链路）。
const GENERATION_SWAP_STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// 确保 daemon `name` 在 `socket_path` 上跑。
///
/// - 已活且代际一致（见下）：返回现有 handle；
/// - 已活但 build-id 代际不一致（F1 握手）：优雅 stop 旧代 → spawn 新代；
///   换代失败 fail-open（log warn + 返回旧 handle，宁可旧不可无）；
/// - 不在跑：抢锁 → daemon spawn → 等 PID 文件 → 返回 handle；
/// - 抢锁失败：另一进程在拉起，等 PID 文件出现即可。
///
/// `name`（当前为 `"cron"`）决定 PID / lock / socket 文件名。
/// `socket_path` 仅写入返回的 handle 用于诊断；daemon 二进制自己通过
/// `paths::socket_file(name)` resolve UDS 路径，调用方只要传一致的值即可
/// （推荐 `paths::socket_file(name).to_string_lossy()`）。
///
/// 代际判定语义（`build_id::evaluate_handshake` 真值表）：
/// `CRABCODE_SKIP_BUILD_HANDSHAKE=1` 或任一侧 `+unknown` → 跳过；
/// `<name>.build-id` 缺失 → 视为不一致（老代 daemon 从未写过该文件，
/// 换代一次即自愈）；双方权威且不等 → **只升不降**（自身版本严格更高才
/// 换代；同版本异 sha / 自身更旧 / 版本不可解析 → 复用，防新旧窗口并存
/// 时换代乒乓，2026-06-12 修订）。
pub fn ensure_running(name: &str, socket_path: &str, binary: &Path) -> Result<DaemonHandle> {
    ensure_running_with(
        name,
        socket_path,
        binary,
        build_id::self_build_id(),
        GENERATION_SWAP_STOP_TIMEOUT,
        &PidIdentityPolicy::Expect(binary.to_path_buf()),
    )
}

/// `ensure_running` 的注入点形态（测试用：自定 self build-id、stop 超时与
/// PID 身份策略）。生产调用一律走 [`ensure_running`]（强制 `Expect(binary)`）。
#[doc(hidden)]
pub fn ensure_running_with(
    name: &str,
    socket_path: &str,
    binary: &Path,
    self_build_id: &str,
    swap_stop_timeout: Duration,
    identity: &PidIdentityPolicy,
) -> Result<DaemonHandle> {
    let pid_file = paths::pid_file(name);
    let lock_file = paths::lock_file(name);

    paths::ensure_run_dir()?;

    // Fast path：PID 文件记录的进程活着**且身份核实是我们的 daemon**（P0-3，
    // 防 PID 复用假活）→ 先过 F1 代际握手，仅一致才直接复用。
    if let Some(pid) = read_alive_daemon_pid(&pid_file, identity) {
        if !needs_generation_swap(name, self_build_id) {
            tracing::debug!(pid, daemon = name, sock = %socket_path, "daemon already running, skip spawn");
            return Ok(DaemonHandle {
                pid,
                sock_path: socket_path.to_string(),
                pid_file,
            });
        }
        tracing::info!(
            pid,
            daemon = name,
            self_build_id,
            recorded = build_id::read_recorded_build_id(&build_id::build_id_file(name)).as_deref(),
            "daemon build-id generation mismatch; attempting graceful swap"
        );
        // 不一致 → 落到下方持锁路径执行换代（socket_lock 两端串行化，
        // 保证并发 ensure 只换代一次）。
    }

    // P0-2（W-CRON-RELEASE-REOPEN 2026-07-16）：spawn 临界区两端统一持锁
    // （Unix flock / Windows 独占共享句柄）。此前 Windows 无任何 spawn 互斥，
    // 并发 ensure 双 spawn → 输家撞 daemon 业务锁结构化退出，首启迁移 >5s 时
    // 双方 PidTimeout。
    match socket_lock::acquire(&lock_file)? {
        socket_lock::LockOutcome::Acquired(_guard) => {
            // 二次确认：抢锁后再读一次 pid 文件（peer 可能已完成换代）
            if let Some(pid) = read_alive_daemon_pid(&pid_file, identity) {
                if !needs_generation_swap(name, self_build_id) {
                    return Ok(DaemonHandle {
                        pid,
                        sock_path: socket_path.to_string(),
                        pid_file,
                    });
                }
                // 仍不一致 → 持锁优雅换代；stop 失败 fail-open 返回旧 handle。
                if !swap_stop_old_generation(name, pid, swap_stop_timeout, identity) {
                    return Ok(DaemonHandle {
                        pid,
                        sock_path: socket_path.to_string(),
                        pid_file,
                    });
                }
                // stop 成功 → 走下方既有 spawn 路径拉新代。
            }

            if !binary.exists() {
                return Err(LauncherError::BinaryMissing(binary.to_path_buf()));
            }

            tracing::info!(
                daemon = name,
                binary = %binary.display(),
                sock = %socket_path,
                "spawning daemon"
            );
            spawn_daemon_detached(binary)?;
            wait_for_pid_file(&pid_file, identity).map(|pid| DaemonHandle {
                pid,
                sock_path: socket_path.to_string(),
                pid_file: pid_file.clone(),
            })
        }
        socket_lock::LockOutcome::Contended => {
            tracing::debug!(daemon = name, "lock contended; waiting for peer launcher");
            wait_for_pid_file(&pid_file, identity).map(|pid| DaemonHandle {
                pid,
                sock_path: socket_path.to_string(),
                pid_file: pid_file.clone(),
            })
        }
    }
}

/// 平台分派的 detached daemon spawn（Unix double-fork / Windows
/// `DETACHED_PROCESS + CREATE_BREAKAWAY_FROM_JOB`）。
fn spawn_daemon_detached(binary: &Path) -> Result<()> {
    spawn_detached_command(&DetachedCommand {
        binary: binary.to_path_buf(),
        args: vec![OsString::from("serve")],
        env_overrides: Vec::new(),
        log_file: None,
    })
}

/// F1：当前是否需要代际换代（握手开 + 自身权威 + 对端记录缺失或权威不等）。
fn needs_generation_swap(name: &str, self_build_id: &str) -> bool {
    let recorded = build_id::read_recorded_build_id(&build_id::build_id_file(name));
    build_id::evaluate_handshake(
        self_build_id,
        recorded.as_deref(),
        !build_id::handshake_enabled(),
    ) == build_id::HandshakeVerdict::Mismatch
}

/// F1：优雅 stop 旧代 daemon。返回 `true` = 旧代已退出（可安全 spawn 新代）；
/// `false` = stop 失败/超时 → 调用方 fail-open 复用旧 handle（宁可旧不可无）。
fn swap_stop_old_generation(
    name: &str,
    pid: u32,
    timeout: Duration,
    identity: &PidIdentityPolicy,
) -> bool {
    match stop_with_identity(name, timeout, identity) {
        Ok(true) => {
            tracing::info!(
                pid,
                daemon = name,
                "old-generation daemon stopped; spawning new generation"
            );
            true
        }
        Ok(false) => {
            tracing::warn!(
                pid,
                daemon = name,
                timeout_ms = timeout.as_millis() as u64,
                "old-generation daemon did not exit in time; fail-open reusing old daemon"
            );
            false
        }
        Err(err) => {
            tracing::warn!(
                pid,
                daemon = name,
                error = %err,
                "old-generation daemon stop failed; fail-open reusing old daemon"
            );
            false
        }
    }
}

/// PID 身份策略（P0-3，W-CRON-RELEASE-REOPEN 2026-07-16）。
///
/// pid 文件里的 PID 可能已被无关进程复用（daemon 被 taskkill/SIGKILL 硬杀后
/// pid 文件残留）。仅凭 [`pid_alive`] 判活会把复用进程当成「daemon 还在跑」：
/// ensure 返回死 handle（守护假活、永不重拉），stop 则按 PID 误杀无辜进程。
/// 身份层用进程 exe / 进程名与预期 daemon 二进制比对，核实通过才认领。
#[derive(Debug, Clone)]
pub enum PidIdentityPolicy {
    /// 生产语义：PID 必须通过 [`pid_identity`] 身份核实（进程 exe / 名称与
    /// 预期二进制的文件名匹配）。
    Expect(PathBuf),
    /// 测试注入：跳过身份核实，沿用纯 [`pid_alive`] 判活（`sleep`/`sh` 假扮
    /// daemon 的集成测试用；生产路径禁止——[`ensure_running`] / [`stop`]
    /// 永远构造 `Expect`）。
    TrustPidFile,
}

/// [`pid_identity`] 的三值判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidIdentity {
    /// 进程 exe / 名称与预期 daemon 二进制匹配。
    Confirmed,
    /// 进程不存在，或存在但明确是别的二进制（PID 已被复用）。
    Mismatch,
    /// 进程存在但取不到 exe 与名称，无法核实。调用方按保守方向处理：
    /// spawn 决策当"不是我们的 daemon"（真冲突由 daemon 业务锁仲裁），
    /// kill 决策当"不可杀"（绝不按未核实身份的 PID 硬杀）。
    Unknown,
}

/// 核实 `pid` 对应进程是否是预期的 daemon 二进制。
///
/// 判据（依次）：进程 exe 路径的文件名；取不到 exe 时退回进程名（Linux
/// `/proc/<pid>/comm` 截断到 15 字符 → 按前缀比对）。比对不区分大小写，
/// `.exe` 后缀两侧互相容忍（Windows/Unix 交叉约定）。
pub fn pid_identity(pid: u32, expected_binary: &Path) -> PidIdentity {
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind, System, UpdateKind};

    let Some(expected_name) = expected_binary
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
    else {
        return PidIdentity::Unknown;
    };
    let expected_stem = expected_name
        .strip_suffix(".exe")
        .unwrap_or(&expected_name)
        .to_string();

    // 只刷新目标 PID + exe 字段（name 是 refresh 时无条件填充的基础字段），
    // 避免全量进程扫描（与 acosmi-scheduler lock.rs::process_start_time 同范式）。
    let refresh = ProcessRefreshKind::nothing().with_exe(UpdateKind::Always);
    let mut sys = System::new_with_specifics(RefreshKind::nothing().with_processes(refresh));
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        false,
        refresh,
    );
    let Some(process) = sys.process(Pid::from_u32(pid)) else {
        // 进程枚举不到 = 不存在（或刚退出）→ 不是活着的我们。
        return PidIdentity::Mismatch;
    };

    let matches_expected = |candidate: &str| -> bool {
        let stem = candidate.strip_suffix(".exe").unwrap_or(candidate);
        stem == expected_stem
    };

    if let Some(name) = process.exe().and_then(|exe| exe.file_name()) {
        let name = name.to_string_lossy().to_lowercase();
        return if matches_expected(&name) {
            PidIdentity::Confirmed
        } else {
            PidIdentity::Mismatch
        };
    }

    let name = process.name().to_string_lossy().to_lowercase();
    if name.is_empty() {
        return PidIdentity::Unknown;
    }
    // Linux /proc comm 截断到 15 字符：截断形态按前缀比对。
    if matches_expected(&name) || (name.len() == 15 && expected_stem.starts_with(name.as_str())) {
        PidIdentity::Confirmed
    } else {
        PidIdentity::Mismatch
    }
}

/// 解析 PID 文件内容（非零 u32），不做任何存活/身份判定。
fn read_pid_file(pid_file: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(pid_file).ok()?;
    let pid: u32 = raw.trim().parse().ok()?;
    if pid == 0 { None } else { Some(pid) }
}

/// 读 PID 文件并通过 `kill -0` 校验进程仍存活。
///
/// 注意：**纯存活判定，无身份核实**——PID 被复用时会假阳性。daemon 生命周期
/// 决策（ensure / stop）一律走 [`read_alive_daemon_pid`] / [`stop`] 的身份层；
/// 本函数保留给"仅需存活性"的诊断类调用与既有测试。
pub fn read_alive_pid(pid_file: &Path) -> Option<u32> {
    let pid = read_pid_file(pid_file)?;
    if pid_alive(pid) { Some(pid) } else { None }
}

/// 身份感知读（P0-3）：存活 **且** 按 `identity` 策略核实是我们的 daemon。
///
/// `Mismatch`（PID 复用）与 `Unknown`（无法核实）都按「我们的 daemon 不在跑」
/// 返回 `None`：spawn 侧继续拉起（真冲突由 daemon 业务锁仲裁，输家结构化
/// 退出且不覆写胜者的 pid 文件）；杀进程侧另有 [`stop`] 的保守分支。
fn read_alive_daemon_pid(pid_file: &Path, identity: &PidIdentityPolicy) -> Option<u32> {
    let pid = read_alive_pid(pid_file)?;
    match identity {
        PidIdentityPolicy::TrustPidFile => Some(pid),
        PidIdentityPolicy::Expect(binary) => match pid_identity(pid, binary) {
            PidIdentity::Confirmed => Some(pid),
            PidIdentity::Mismatch | PidIdentity::Unknown => None,
        },
    }
}

/// PID 存活判定（**僵尸感知基准**）。
///
/// W-CRON-LIVENESS-PARITY (2026-07-20) 判活四实现同源契约（本处 = A，基准）——改一处
/// 必核对四处：
///   A `acosmi-daemon-launcher::pid_alive`（**本函数**，P0-3 已僵尸感知）
///   B `acosmi-scheduler::lock::is_pid_alive`（src/lock.rs）
///   C `acosmi-exec::tree_kill::is_process_alive`（src/tree_kill.rs）
///   D `acosmi-app-server::parent_death::parent_pid_is_alive`（src/parent_death.rs）
/// Windows「已退出但句柄被钉住的僵尸」OpenProcess 仍成功；必须再 GetExitCodeProcess
/// 判 STILL_ACTIVE(259) 才算活。Unix 经 kill(pid,0)+ESRCH（reap 后正确判死）。
#[cfg(unix)]
pub fn pid_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    matches!(
        kill(Pid::from_raw(pid as i32), None),
        Ok(()) | Err(nix::errno::Errno::EPERM)
    )
}

#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER};
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => {
                let mut code: u32 = 0;
                let alive = GetExitCodeProcess(h, &mut code).is_ok() && code == 259; // STILL_ACTIVE
                let _ = CloseHandle(h);
                alive
            }
            // P0-3 真值表统一（对齐 acosmi-scheduler lock.rs::is_pid_alive，
            // 2026-07-16）：OpenProcess 失败仅 ERROR_INVALID_PARAMETER(87)
            // 证明「进程不存在」；ACCESS_DENIED 等 = 进程存在但无权限（PID 被
            // 系统/提权进程复用的常态）→ 按「存在」处理。此前这里一律判死，
            // 与业务锁判活语义相反：同一 PID 两层各执一词互相楔死。是否
            // 「我们的 daemon」由身份层 pid_identity 判定，不再靠可打开性猜。
            Err(e) => e.code() != ERROR_INVALID_PARAMETER.to_hresult(),
        }
    }
}

fn wait_for_pid_file(pid_file: &Path, identity: &PidIdentityPolicy) -> Result<u32> {
    let deadline = std::time::Instant::now() + PID_FILE_WAIT;
    loop {
        // 身份感知（P0-3）：等待期间 pid 文件可能仍是上一代残留内容（PID 已被
        // 无关进程复用）——bare 存活判定会把 Contended 等待方立即"喂"一个外来
        // 进程的假 handle。身份不符继续轮询，直到新 daemon 写出自己的 PID。
        if let Some(pid) = read_alive_daemon_pid(pid_file, identity) {
            return Ok(pid);
        }
        if std::time::Instant::now() > deadline {
            return Err(LauncherError::PidFileTimeout(PID_FILE_WAIT));
        }
        std::thread::sleep(PID_FILE_POLL);
    }
}

/// `crabcode <daemon> stop` / restart 用：发 SIGTERM (unix) / signal 软停 event
/// (windows)，等 PID 文件被清。
///
/// **Windows R6-T1 §A.2 软停**：在 `TerminateProcess` 之前先 signal named
/// event 让 daemon 走 graceful 关闭路径；event 不存在或 daemon 没在
/// `softstop_grace`（这里固定 80% timeout）内退出时，再 `TerminateProcess`
/// 兜底。daemon 端若编译时跑在新版（含 `windows_event::create_event`）会接到
/// 软停 → 6 步关闭；若 daemon 是旧二进制（event 还没创建），`signal_event_by_name`
/// 返 `NotFound`，逻辑直接进硬杀分支，行为与旧版等价。
pub fn stop(name: &str, timeout: Duration) -> Result<bool> {
    // 约定：daemon 二进制名 = `crabcode-<name>`（cron → crabcode-cron）。
    // 外部 stop 调用方没有 binary 路径，
    // 按约定推导预期身份；身份核实不过的进程宁可不杀（保守方向，见
    // `stop_with_identity`）。新 daemon 若违反此命名约定，必须在这里扩展映射。
    stop_with_identity(
        name,
        timeout,
        &PidIdentityPolicy::Expect(PathBuf::from(format!("crabcode-{name}"))),
    )
}

/// [`stop`] 的身份注入形态（swap 换代路径复用 ensure 的策略；测试可注入
/// `TrustPidFile` 复现旧 bare-PID 语义）。
#[doc(hidden)]
pub fn stop_with_identity(
    name: &str,
    timeout: Duration,
    identity: &PidIdentityPolicy,
) -> Result<bool> {
    let pid_file = paths::pid_file(name);
    let Some(pid) = read_pid_file(&pid_file) else {
        return Ok(false);
    };
    if !pid_alive(pid) {
        // 进程已死但 PID 文件残留 → best effort 清理，视作"无 daemon 可停"。
        let _ = std::fs::remove_file(&pid_file);
        return Ok(false);
    }
    // P0-3（2026-07-16）：杀前身份校验。此前 stop 按 PID 硬杀无任何身份核实，
    // 陈旧 pid 文件 + PID 复用 ⇒ TerminateProcess 误杀无辜进程（审计次级#5）。
    if let PidIdentityPolicy::Expect(binary) = identity {
        match pid_identity(pid, binary) {
            PidIdentity::Confirmed => {}
            PidIdentity::Mismatch => {
                tracing::warn!(
                    pid,
                    daemon = name,
                    "pid 文件指向的进程不是本 daemon（PID 已被复用）；清理陈旧 pid 文件且不杀该进程"
                );
                let _ = std::fs::remove_file(&pid_file);
                return Ok(false);
            }
            PidIdentity::Unknown => {
                tracing::warn!(
                    pid,
                    daemon = name,
                    "无法核实 PID 身份；拒绝按未核实身份硬杀（fail-open）"
                );
                return Ok(false);
            }
        }
    }

    #[cfg(unix)]
    {
        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        if let Err(e) = kill(Pid::from_raw(pid as i32), Some(Signal::SIGTERM)) {
            tracing::warn!(pid, error = %e, "SIGTERM failed");
        }
    }
    #[cfg(windows)]
    {
        // R6-T1 §A.2：先 signal soft-stop event；daemon 在 R6-T1 6 步关闭路径
        // 内退出后会自删 pid_file。下面的 poll loop 等到 80% timeout 还没退出
        // 再走硬杀。`NotFound` 表示 daemon 没创建 event（旧二进制），直接硬杀。
        match windows_event::signal_event_by_name(name) {
            Ok(()) => {
                tracing::debug!(pid, daemon = name, "soft-stop event signaled");
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(
                    pid,
                    daemon = name,
                    "soft-stop event not found (legacy daemon?); going straight to TerminateProcess"
                );
                terminate_process_windows(pid);
                return wait_pid_file_clear(&pid_file, pid, timeout);
            }
            Err(err) => {
                tracing::warn!(pid, daemon = name, error = %err, "soft-stop signal failed");
            }
        }
    }

    // unix 路径直接进 poll；windows 软停后给 80% timeout，再硬杀兜底。
    #[cfg(unix)]
    {
        wait_pid_file_clear(&pid_file, pid, timeout)
    }
    #[cfg(windows)]
    {
        let soft_grace = Duration::from_millis(((timeout.as_millis() * 8) / 10) as u64);
        match wait_pid_file_clear(&pid_file, pid, soft_grace) {
            Ok(true) => Ok(true),
            Ok(false) | Err(_) => {
                tracing::warn!(
                    pid,
                    daemon = name,
                    "soft-stop did not exit in {:?}; falling back to TerminateProcess",
                    soft_grace
                );
                terminate_process_windows(pid);
                let remaining = timeout.saturating_sub(soft_grace);
                wait_pid_file_clear(&pid_file, pid, remaining)
            }
        }
    }
}

/// 共用 poll 循环：等 `pid_file` 被清，或 `pid` 进程不再活，或 `timeout` 用完。
fn wait_pid_file_clear(pid_file: &Path, pid: u32, timeout: Duration) -> Result<bool> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !pid_file.exists() {
            return Ok(true);
        }
        if !pid_alive(pid) {
            // 进程已死但 PID 文件没清 → best effort 清理
            let _ = std::fs::remove_file(pid_file);
            return Ok(true);
        }
        if std::time::Instant::now() > deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(windows)]
fn terminate_process_windows(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};
    // SAFETY: 标准 Win32 调用；handle 仅本地使用，立即 close。
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
            let _ = TerminateProcess(h, 1);
            let _ = CloseHandle(h);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pid_file_timeout_error_displays() {
        let e = LauncherError::PidFileTimeout(Duration::from_secs(3));
        assert!(format!("{e}").contains("3"));
    }

    #[test]
    fn read_alive_pid_zero_in_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("zero.pid");
        std::fs::write(&p, "0").unwrap();
        assert!(read_alive_pid(&p).is_none());
    }

    #[test]
    fn pid_alive_for_self_is_true() {
        // 自己的 PID 必活；防 pid_alive 链路退化
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_for_high_pid_is_false() {
        // 4194303 = Linux 默认 PID 上限，常态下不会被分配出去。
        // 选这个值断 pid_alive 在"明显不存在的 PID"上返回 false，确保
        // probe / liveness 链路对死 PID 不会假阳性误判存活。
        // PID 0 不直接断（kill(0, 0) 在 Linux 是发给当前进程组，语义有歧义）；
        // 0 的拒绝由 read_alive_pid 上层做（见 read_alive_pid_zero_in_file_returns_none）。
        assert!(!pid_alive(4_194_303));
    }

    // ── P0-3 身份层（W-CRON-RELEASE-REOPEN 2026-07-16）──────────────────────

    #[test]
    fn pid_identity_confirms_own_process_against_own_exe() {
        let exe = std::env::current_exe().expect("current_exe");
        assert_eq!(
            pid_identity(std::process::id(), &exe),
            PidIdentity::Confirmed,
            "自身 PID 对自身 exe 必须 Confirmed"
        );
    }

    #[test]
    fn pid_identity_mismatch_for_foreign_expected_binary() {
        // 自身 PID 对一个明显不是自己的预期二进制 → Mismatch（这正是
        // 「pid 文件残留 + PID 被别的进程复用」的判定形态）。
        assert_eq!(
            pid_identity(
                std::process::id(),
                Path::new("/definitely/not/crabcode-cron")
            ),
            PidIdentity::Mismatch
        );
    }

    #[test]
    fn pid_identity_dead_pid_is_mismatch() {
        assert_eq!(
            pid_identity(4_194_303, Path::new("crabcode-cron")),
            PidIdentity::Mismatch
        );
    }

    #[test]
    fn pid_identity_tolerates_exe_suffix_cross_platform() {
        // `.exe` 后缀两侧互相容忍：预期名带 .exe、进程名不带（或反之）都算同一
        // stem。用自身进程验证（Windows 上自身 exe 带 .exe，Unix 不带）。
        let exe = std::env::current_exe().expect("current_exe");
        let stem = exe.file_stem().expect("stem").to_string_lossy().to_string();
        let with_exe = exe.with_file_name(format!("{stem}.exe"));
        let without_exe = exe.with_file_name(stem);
        assert_eq!(
            pid_identity(std::process::id(), &with_exe),
            PidIdentity::Confirmed
        );
        assert_eq!(
            pid_identity(std::process::id(), &without_exe),
            PidIdentity::Confirmed
        );
    }

    #[test]
    fn read_alive_daemon_pid_rejects_reused_pid() {
        // 陈旧 pid 文件指向一个活着但"不是我们 daemon"的进程（这里用测试进程
        // 自身扮演复用者）：bare read_alive_pid 判活，身份感知读必须拒认。
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("cron.pid");
        std::fs::write(&pid_file, std::process::id().to_string()).unwrap();

        assert!(read_alive_pid(&pid_file).is_some(), "bare 判活应 Some");
        assert!(
            read_alive_daemon_pid(
                &pid_file,
                &PidIdentityPolicy::Expect(PathBuf::from("crabcode-cron"))
            )
            .is_none(),
            "身份不符（PID 复用）必须判『我们的 daemon 不在跑』"
        );
        // TrustPidFile（测试注入）保持旧 bare 语义。
        assert_eq!(
            read_alive_daemon_pid(&pid_file, &PidIdentityPolicy::TrustPidFile),
            Some(std::process::id())
        );
    }

    #[test]
    fn read_alive_daemon_pid_accepts_own_identity() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("self.pid");
        std::fs::write(&pid_file, std::process::id().to_string()).unwrap();
        let exe = std::env::current_exe().expect("current_exe");
        assert_eq!(
            read_alive_daemon_pid(&pid_file, &PidIdentityPolicy::Expect(exe)),
            Some(std::process::id())
        );
    }

    /// P0-3 stop 杀前身份校验：陈旧 pid 文件 + PID 复用（用测试进程自身扮演
    /// 复用者）→ 清理 pid 文件、**绝不杀进程**、返回 Ok(false)。
    /// 若守卫失效误走 kill 路径，被 TERM 的是本测试进程自身——测试即哨兵。
    #[test]
    #[serial_test::serial]
    fn stop_with_identity_mismatch_cleans_stale_pid_and_does_not_kill() {
        let dir = tempfile::tempdir().unwrap();
        let prev_cfg = std::env::var_os("CRABCODE_CONFIG_DIR");
        // SAFETY: serial_test 串行化，无并发 env 读写
        unsafe { std::env::set_var("CRABCODE_CONFIG_DIR", dir.path()) };
        paths::ensure_run_dir().unwrap();

        let name = "cron";
        std::fs::write(paths::pid_file(name), std::process::id().to_string()).unwrap();

        let stopped = stop_with_identity(
            name,
            Duration::from_millis(300),
            &PidIdentityPolicy::Expect(PathBuf::from("crabcode-cron")),
        )
        .expect("stop_with_identity");
        assert!(!stopped, "身份不符不得视作『成功停止 daemon』");
        assert!(
            !paths::pid_file(name).exists(),
            "PID 复用形态的陈旧 pid 文件应被清理"
        );

        unsafe {
            match prev_cfg {
                Some(v) => std::env::set_var("CRABCODE_CONFIG_DIR", v),
                None => std::env::remove_var("CRABCODE_CONFIG_DIR"),
            }
        }
    }
}

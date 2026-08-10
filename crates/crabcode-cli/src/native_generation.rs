//! Stable-launcher handoff for complete pure-TUI native-installer generations.
//!
//! A Windows file symlink requires privilege and a running `.exe` cannot be
//! replaced in-process. The stable launcher is therefore an ordinary copy of
//! `crabcode.exe`; it reads the atomically replaced `versions/.current` marker
//! and forwards the complete argv/environment/console to that generation's
//! launcher. POSIX flat installs use the same handoff, while version-directory
//! launches and development binaries never redirect.
//!
//! Windows cannot replace the current process image. Its stable launcher owns
//! exactly one foreground generation process through a kill-on-close Job. The
//! wrapper itself remains outside that Job; `PROC_THREAD_ATTRIBUTE_JOB_LIST`
//! associates the suspended generation atomically during `CreateProcessW`.
//! Before the exact returned primary-thread handle is resumed, the Job switches
//! to silent-breakaway: descendants remain the generation's supervisor/lifecycle
//! responsibility, and deliberately detached shared daemons are not
//! accidentally owned by one launcher window.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
#[cfg(any(not(any(unix, windows)), all(test, unix)))]
use std::process::ExitStatus;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

pub(crate) use acosmi_generation_lease::GenerationLease;

pub(crate) const CURRENT_GENERATION_MARKER: &str = ".current";
pub(crate) const STABLE_LAUNCHER_PROTOCOL_MARKER: &str = ".launcher-v1";
pub(crate) const STABLE_LAUNCHER_PENDING_MARKER: &str = ".launcher-v1.pending";
const STABLE_LAUNCHER_ENV: &str = "CRABCODE_STABLE_LAUNCHER_PATH";
const MAX_MARKER_BYTES: u64 = 32 * 1024;
#[cfg(any(windows, test))]
const WINDOWS_ACTIVATION_COMMIT_SUBCOMMAND: &str = "__native-update-activate-commit";
#[cfg(any(windows, test))]
const WINDOWS_PROCESS_START_IDENTITY_PREFIX: &str = "win32-process-start:";
#[cfg(windows)]
pub(crate) const WINDOWS_ACTIVATION_RESTART_REQUIRED_EXIT_CODE: i32 = 75;

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WindowsActivationBrokerOutcome {
    Scheduled,
    RestartRequired,
}

#[cfg(any(windows, test))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StableLauncherJobPhase {
    BeforeGeneration,
    GenerationRunning,
}

#[cfg(any(windows, test))]
const fn stable_launcher_job_limits(
    phase: StableLauncherJobPhase,
    kill_on_close: u32,
    silent_breakaway: u32,
) -> u32 {
    match phase {
        // The wrapper intentionally stays outside this Job and silent breakaway
        // must still be disabled when CreateProcess runs. The suspended
        // generation therefore joins the Job atomically at creation.
        StableLauncherJobPhase::BeforeGeneration => kill_on_close,
        // Once the direct generation is known to be a member, future children
        // belong to its own supervisor or detached-daemon lifecycle.
        StableLauncherJobPhase::GenerationRunning => kill_on_close | silent_breakaway,
    }
}

#[cfg(any(windows, test))]
const fn windows_job_denies_explicit_breakaway(
    in_job: bool,
    limit_flags: u32,
    breakaway_ok: u32,
    silent_breakaway_ok: u32,
) -> bool {
    in_job && limit_flags & (breakaway_ok | silent_breakaway_ok) == 0
}

#[cfg(any(windows, test))]
const fn stable_launcher_consumes_ctrl_event(
    ctrl_type: u32,
    ctrl_c_event: u32,
    ctrl_break_event: u32,
) -> bool {
    ctrl_type == ctrl_c_event || ctrl_type == ctrl_break_event
}

fn non_empty_env(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|value| !value.is_empty())
}

fn installer_home_dir() -> Option<PathBuf> {
    let home = non_empty_env("HOME").map(PathBuf::from);
    // `getXDGDataHome()` / `getXDGStateHome()` fall back to Node/Bun's
    // `os.homedir()`. On shipped Windows runtimes that API gives
    // `%USERPROFILE%` before querying the profile known folder, so keep the
    // native stable launcher on the exact same directory root.
    #[cfg(windows)]
    let home = home.or_else(|| non_empty_env("USERPROFILE").map(PathBuf::from));
    home.or_else(dirs::home_dir)
}

pub(crate) fn native_installer_versions_dir() -> Result<PathBuf> {
    let data_home = non_empty_env("XDG_DATA_HOME").map_or_else(
        || installer_home_dir().map(|home| home.join(".local").join("share")),
        |value| Some(PathBuf::from(value)),
    );
    let data_home = data_home.context("cannot resolve native-installer data home")?;
    if !data_home.is_absolute() {
        bail!("native-installer data home must be absolute");
    }
    Ok(data_home.join("crabcode").join("versions"))
}

fn native_installer_locks_dir() -> Result<PathBuf> {
    let state_home = non_empty_env("XDG_STATE_HOME").map_or_else(
        || installer_home_dir().map(|home| home.join(".local").join("state")),
        |value| Some(PathBuf::from(value)),
    );
    let state_home = state_home.context("cannot resolve native-installer state home")?;
    if !state_home.is_absolute() {
        bail!("native-installer state home must be absolute");
    }
    Ok(state_home.join("crabcode").join("locks"))
}

fn launcher_file_name() -> &'static str {
    if cfg!(windows) {
        "crabcode.exe"
    } else {
        "crabcode"
    }
}

fn tui_file_name() -> &'static str {
    if cfg!(windows) {
        "crabcode-tui.exe"
    } else {
        "crabcode-tui"
    }
}

fn memory_file_name() -> &'static str {
    if cfg!(windows) {
        "acosmi-memory-orchestrator.exe"
    } else {
        "acosmi-memory-orchestrator"
    }
}

fn stable_launcher_candidates(home: &Path) -> Vec<PathBuf> {
    let mut candidates = vec![
        home.join(".local").join("bin").join(launcher_file_name()),
        home.join(".crabcode")
            .join("bin")
            .join(launcher_file_name()),
    ];
    if let Some(custom_home) = non_empty_env("CRABCODE_HOME") {
        candidates.push(
            PathBuf::from(custom_home)
                .join("bin")
                .join(launcher_file_name()),
        );
    }
    candidates
}

fn path_identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn same_path(left: &Path, right: &Path) -> bool {
    let left = path_identity(left);
    let right = path_identity(right);
    if cfg!(windows) {
        left.to_string_lossy()
            .eq_ignore_ascii_case(&right.to_string_lossy())
    } else {
        left == right
    }
}

fn is_known_stable_launcher(path: &Path, versions_dir: &Path) -> Result<bool> {
    if let Some(explicit) = non_empty_env(STABLE_LAUNCHER_ENV) {
        let explicit = PathBuf::from(explicit);
        if explicit.is_absolute() && same_path(path, &explicit) {
            return Ok(true);
        }
    }
    if installer_home_dir().is_some_and(|home| {
        stable_launcher_candidates(&home)
            .iter()
            .any(|candidate| same_path(path, candidate))
    }) {
        return Ok(true);
    }
    Ok(protocol_stable_launchers(versions_dir)?
        .iter()
        .any(|candidate| same_path(path, candidate)))
}

fn is_release_version_segment(value: &str) -> bool {
    let (core, prerelease) = value
        .split_once('-')
        .map_or((value, None), |(core, suffix)| (core, Some(suffix)));
    let mut parts = core.split('.');
    let valid_core = (0..3).all(|_| {
        parts.next().is_some_and(|part| {
            !part.is_empty()
                && part.bytes().all(|b| b.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'))
        })
    }) && parts.next().is_none();
    valid_core
        && prerelease.is_none_or(|suffix| {
            suffix.split('.').all(|part| {
                !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        })
}

fn metadata_is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn read_canonical_marker(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot inspect path marker {}", path.display()))?;
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) || metadata.len() == 0 {
        bail!("path marker is not a real regular non-empty file");
    }
    if metadata.len() > MAX_MARKER_BYTES {
        bail!("path marker is too large");
    }
    let raw = fs::read_to_string(path)
        .with_context(|| format!("cannot read path marker {}", path.display()))?;
    let value = raw.strip_suffix('\n').unwrap_or(&raw);
    if value.is_empty()
        || (raw != value && raw != format!("{value}\n"))
        || value.chars().any(char::is_control)
    {
        bail!("path marker is not one canonical path line");
    }
    let target = PathBuf::from(value);
    if !target.is_absolute() {
        bail!("path marker target must be absolute");
    }
    Ok(target)
}

fn protocol_stable_launchers(versions_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut launchers = Vec::new();
    for name in [
        STABLE_LAUNCHER_PROTOCOL_MARKER,
        STABLE_LAUNCHER_PENDING_MARKER,
    ] {
        let marker = versions_dir.join(name);
        match fs::symlink_metadata(&marker) {
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("cannot inspect launcher marker {}", marker.display())
                });
            }
        }
        let launcher = read_canonical_marker(&marker)?;
        if launcher.file_name().and_then(|value| value.to_str()) != Some(launcher_file_name()) {
            bail!("stable-launcher marker names an unexpected executable");
        }
        let metadata = fs::symlink_metadata(&launcher).with_context(|| {
            format!(
                "stable-launcher marker target is missing: {}",
                launcher.display()
            )
        })?;
        if !metadata.is_file() || metadata_is_reparse_point(&metadata) || metadata.len() == 0 {
            bail!("stable-launcher marker target is not a real regular file");
        }
        launchers.push(launcher);
    }
    Ok(launchers)
}

fn assert_real_regular_file(root: &Path, relative: &str, label: &str) -> Result<()> {
    let mut path = root.to_path_buf();
    let segments: Vec<&str> = relative.split('/').collect();
    for (index, segment) in segments.iter().enumerate() {
        path.push(segment);
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("missing {label}: {}", path.display()))?;
        if metadata_is_reparse_point(&metadata) {
            bail!("{label} traverses a symlink or reparse point");
        }
        if index + 1 == segments.len() {
            if !metadata.is_file() || metadata.len() == 0 {
                bail!("{label} must be a real regular non-empty file");
            }
        } else if !metadata.is_dir() {
            bail!("{label} parent must be a real directory");
        }
    }
    let canonical = fs::canonicalize(&path)
        .with_context(|| format!("cannot canonicalize {label}: {}", path.display()))?;
    if !canonical.starts_with(root) {
        bail!("{label} escapes the selected generation");
    }
    Ok(())
}

fn validate_generation_directory(versions_dir: &Path, generation_path: &Path) -> Result<PathBuf> {
    let versions = fs::canonicalize(versions_dir)
        .with_context(|| format!("cannot canonicalize {}", versions_dir.display()))?;
    let target_metadata = fs::symlink_metadata(generation_path).with_context(|| {
        format!(
            "cannot inspect generation root {}",
            generation_path.display()
        )
    })?;
    if !target_metadata.is_dir() || metadata_is_reparse_point(&target_metadata) {
        bail!("generation root must be a real directory, not a reparse point");
    }
    let target = fs::canonicalize(generation_path)
        .with_context(|| format!("cannot canonicalize {}", generation_path.display()))?;
    if target.parent() != Some(versions.as_path()) {
        bail!("generation marker escapes the native-installer versions directory");
    }
    let version = target
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| is_release_version_segment(value))
        .context("generation marker does not name one canonical release version")?;
    if version.starts_with('.') {
        bail!("generation marker cannot select a hidden transaction directory");
    }

    // Keep this list restricted to the files the pure launcher must execute
    // before the signed TS installer can participate again. Full manifest and
    // signature verification remains owned by bundleContract.ts; duplicating
    // its signed-file policy here would create a second verifier.
    for (relative, label) in [
        (launcher_file_name(), "generation launcher"),
        (tui_file_name(), "generation native TUI"),
        (memory_file_name(), "generation memory orchestrator"),
        (
            if cfg!(windows) { "bun.exe" } else { "bun" },
            "generation Bun runtime",
        ),
        (
            "dist/tui-runtime/index.js",
            "generation direct TypeScript runtime",
        ),
        ("build-id", "generation build id"),
        ("release-manifest.json", "generation signed manifest"),
        ("release-manifest.digest.json", "generation manifest digest"),
    ] {
        assert_real_regular_file(&target, relative, label)?;
    }
    Ok(target)
}

fn validate_generation_root(versions_dir: &Path, marker: &Path) -> Result<PathBuf> {
    let marker_target = read_canonical_marker(marker)?;
    validate_generation_directory(versions_dir, &marker_target)
}

/// Prove that `executable` belongs to the generation selected by the native
/// installer's durable `.current` pointer.
///
/// This is intentionally stronger than merely recognizing a launcher under
/// `versions/`: an already-running old generation keeps its lifetime lease,
/// but loses update authority as soon as the installer selects a successor.
/// Memory uses this proof only for the otherwise-unordered case where two
/// authoritative build ids have the same numeric version and different
/// commit metadata.
pub(crate) fn executable_is_selected_generation(
    executable: &Path,
    memory_binary: &Path,
    expected_build_id: &str,
) -> Result<bool> {
    let versions = native_installer_versions_dir()?;
    executable_is_selected_generation_in(executable, memory_binary, expected_build_id, &versions)
}

fn executable_is_selected_generation_in(
    executable: &Path,
    memory_binary: &Path,
    expected_build_id: &str,
    versions_dir: &Path,
) -> Result<bool> {
    if !is_generation_launcher(executable, versions_dir) {
        return Ok(false);
    }
    let marker = versions_dir.join(CURRENT_GENERATION_MARKER);
    let selected = match fs::symlink_metadata(&marker) {
        Ok(_) => validate_generation_root(versions_dir, &marker)?,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect generation marker {}", marker.display()));
        }
    };
    if !same_path(executable, &selected.join(launcher_file_name()))
        || !same_path(memory_binary, &selected.join(memory_file_name()))
    {
        return Ok(false);
    }
    let build_id_path = selected.join("build-id");
    let metadata = fs::symlink_metadata(&build_id_path)
        .with_context(|| format!("cannot inspect {}", build_id_path.display()))?;
    if !metadata.is_file()
        || metadata_is_reparse_point(&metadata)
        || metadata.len() == 0
        || metadata.len() > 256
    {
        bail!("selected generation build-id is not a bounded real regular file");
    }
    let raw = fs::read_to_string(&build_id_path)
        .with_context(|| format!("cannot read {}", build_id_path.display()))?;
    let build_id = raw.strip_suffix('\n').unwrap_or(&raw);
    if build_id.is_empty()
        || (raw != build_id && raw != format!("{build_id}\n"))
        || build_id.chars().any(char::is_control)
    {
        bail!("selected generation build-id is not one canonical value line");
    }
    Ok(build_id == expected_build_id)
}

fn is_generation_launcher(current_exe: &Path, versions_dir: &Path) -> bool {
    let (Ok(current), Ok(versions)) = (
        fs::canonicalize(current_exe),
        fs::canonicalize(versions_dir),
    ) else {
        return false;
    };
    if current.file_name().and_then(|value| value.to_str()) != Some(launcher_file_name()) {
        return false;
    }
    let Some(generation) = current.parent() else {
        return false;
    };
    generation.parent() == Some(versions.as_path())
        && generation
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(is_release_version_segment)
}

fn handoff_target(current_exe: &Path, versions_dir: &Path) -> Result<Option<PathBuf>> {
    // A selected generation launcher must never consult the stable pointer;
    // this is the recursion guard for ordinary launches and the migration
    // helper alike.
    if is_generation_launcher(current_exe, versions_dir) {
        return Ok(None);
    }
    if !is_known_stable_launcher(current_exe, versions_dir)? {
        return Ok(None);
    }

    let marker = versions_dir.join(CURRENT_GENERATION_MARKER);
    match fs::symlink_metadata(&marker) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if protocol_stable_launchers(versions_dir)?
                .iter()
                .any(|launcher| same_path(current_exe, launcher))
            {
                bail!("stable launcher protocol is active but .current is missing");
            }
            return Ok(None);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("cannot inspect generation marker {}", marker.display()));
        }
    }
    let generation = validate_generation_root(versions_dir, &marker)?;
    let target = generation.join(launcher_file_name());
    if same_path(current_exe, &target) {
        return Ok(None);
    }
    Ok(Some(target))
}

#[cfg(any(not(any(unix, windows)), all(test, unix)))]
fn child_exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;

        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

/// If this executable is the stable launcher, run the selected immutable
/// generation and return its exact process exit code. Invalid markers fail
/// closed rather than silently executing stale flat companions.
pub(crate) fn handoff_to_current_generation() -> Result<Option<i32>> {
    let current_exe = std::env::current_exe().context("cannot resolve current launcher")?;
    let versions = native_installer_versions_dir()?;
    let Some(target) = handoff_target(&current_exe, &versions)? else {
        return Ok(None);
    };
    let generation = target
        .parent()
        .context("selected generation launcher has no parent directory")?
        .to_path_buf();
    let locks = native_installer_locks_dir()?;
    // Register a native lifetime record while holding the exact per-version
    // transaction used by TS GC. This closes the `.current` read ->
    // generation spawn/exec window: GC either wins first (and validation below
    // fails closed) or observes this live record before deletion.
    let generation_lease = GenerationLease::acquire_fresh(&generation, &locks, || {
        let validated = validate_generation_directory(&versions, &generation)?;
        if !same_path(&validated, &generation) {
            bail!("selected generation changed while acquiring its native lease");
        }
        Ok(())
    })?;
    let mut command = Command::new(&target);
    command
        .args(std::env::args_os().skip(1))
        .env(STABLE_LAUNCHER_ENV, &current_exe)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    generation_lease.inject_command_environment(&mut command)?;

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // A POSIX stable launcher is a path indirection, not an extra process
        // supervisor. exec preserves the PID/process group and therefore gives
        // Ctrl-C, SIGTERM and parent death exactly the generation's semantics.
        let error = command.exec();
        Err(error).with_context(|| format!("cannot exec generation {}", target.display()))
    }

    #[cfg(windows)]
    {
        let exit_code = wait_for_windows_generation(&command, &target)?;
        Ok(Some(exit_code))
    }

    #[cfg(not(any(unix, windows)))]
    {
        let status = command
            .status()
            .with_context(|| format!("cannot launch generation {}", target.display()))?;
        Ok(Some(child_exit_code(status)))
    }
}

/// Acquire (or, after POSIX stable-launcher `exec`, adopt) the native lease for
/// a directly running immutable generation. Development/flat binaries return
/// `None`. The returned guard must remain alive until the Rust supervisor has
/// stopped; it also supplies the capability-bound environment handed to Bun.
pub(crate) fn acquire_current_generation_lease() -> Result<Option<GenerationLease>> {
    let current_exe = std::env::current_exe().context("cannot resolve current launcher")?;
    let versions = native_installer_versions_dir()?;
    if !is_generation_launcher(&current_exe, &versions) {
        return Ok(None);
    }
    let current = fs::canonicalize(&current_exe)
        .with_context(|| format!("cannot canonicalize {}", current_exe.display()))?;
    let generation = current
        .parent()
        .context("generation launcher has no parent directory")?
        .to_path_buf();
    let generation = validate_generation_directory(&versions, &generation)?;
    let locks = native_installer_locks_dir()?;
    let lease = GenerationLease::acquire_or_adopt(&generation, &locks, || {
        let validated = validate_generation_directory(&versions, &generation)?;
        if !same_path(&validated, &generation) {
            bail!("running generation changed while acquiring its native lease");
        }
        Ok(())
    })?;
    Ok(Some(lease))
}

#[cfg(windows)]
fn wait_for_windows_generation(command: &Command, target: &Path) -> Result<i32> {
    // A custom handler (not the inherited NULL-ignore flag) keeps only the
    // wrapper alive for Ctrl-C/Ctrl-Break. The generation shares the console,
    // receives the event independently, and runs the supervisor's one graceful
    // shutdown coordinator for either event.
    let _console_handler = StableLauncherConsoleHandler::install()?;
    let mut job = StableLauncherJob::create()?;
    let mut child = create_suspended_windows_generation(command, target, &job)
        .with_context(|| format!("cannot launch generation {}", target.display()))?;

    let setup = (|| -> Result<()> {
        job.assert_contains(&child.process)?;
        job.configure(StableLauncherJobPhase::GenerationRunning)?;
        child.resume_primary_thread()?;
        Ok(())
    })();
    if let Err(error) = setup {
        child.terminate_and_reap(&job);
        return Err(error);
    }

    let exit_code = match child.wait() {
        Ok(code) => code,
        Err(error) => {
            child.terminate_and_reap(&job);
            return Err(error)
                .with_context(|| format!("cannot wait for generation {}", target.display()));
        }
    };
    // The direct generation has exited. Closing KILL_ON_JOB_CLOSE also reaps
    // any accidental non-breakaway residue; intended shared daemons already
    // used the silent-breakaway policy enabled before the primary thread ran.
    job.close()
        .context("cannot release completed generation job")?;
    Ok(exit_code)
}

#[cfg(windows)]
struct OwnedWindowsHandle(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl OwnedWindowsHandle {
    fn from_raw(handle: windows_sys::Win32::Foundation::HANDLE, label: &str) -> Result<Self> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;

        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            bail!("{label} returned an invalid Windows handle");
        }
        Ok(Self(handle))
    }

    fn close(&mut self) -> Result<()> {
        use windows_sys::Win32::Foundation::CloseHandle;

        if self.0.is_null() {
            return Ok(());
        }
        // SAFETY: this guard owns the handle and clears it after the single
        // successful close, preventing reuse or a second close in Drop.
        #[allow(unsafe_code)]
        if unsafe { CloseHandle(self.0) } == 0 {
            return Err(std::io::Error::last_os_error()).context("CloseHandle failed");
        }
        self.0 = std::ptr::null_mut();
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for OwnedWindowsHandle {
    fn drop(&mut self) {
        if self.0.is_null() {
            return;
        }
        // SAFETY: best-effort close of the still-owned handle.
        #[allow(unsafe_code)]
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
        self.0 = std::ptr::null_mut();
    }
}

#[cfg(windows)]
struct OwnedWindowsAttributeList<'values> {
    _storage: Vec<usize>,
    list: windows_sys::Win32::System::Threading::LPPROC_THREAD_ATTRIBUTE_LIST,
    _values: std::marker::PhantomData<&'values [windows_sys::Win32::Foundation::HANDLE]>,
}

#[cfg(windows)]
impl Drop for OwnedWindowsAttributeList<'_> {
    fn drop(&mut self) {
        // SAFETY: `list` was initialized in `_storage`, which remains live and
        // unmoved for the guard's lifetime.
        #[allow(unsafe_code)]
        unsafe {
            windows_sys::Win32::System::Threading::DeleteProcThreadAttributeList(self.list);
        }
    }
}

#[cfg(windows)]
fn windows_process_attributes<'values>(
    handles: &'values [windows_sys::Win32::Foundation::HANDLE],
    jobs: &'values [windows_sys::Win32::Foundation::HANDLE],
) -> Result<OwnedWindowsAttributeList<'values>> {
    use windows_sys::Win32::System::Threading::{
        InitializeProcThreadAttributeList, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
        PROC_THREAD_ATTRIBUTE_JOB_LIST, UpdateProcThreadAttribute,
    };

    if handles.is_empty() || jobs.is_empty() {
        bail!("process creation handle and Job lists must be non-empty");
    }
    let mut bytes = 0usize;
    // The documented sizing call fails while returning the required size.
    #[allow(unsafe_code)]
    unsafe {
        let _ = InitializeProcThreadAttributeList(std::ptr::null_mut(), 2, 0, &raw mut bytes);
    }
    if bytes == 0 {
        bail!("InitializeProcThreadAttributeList returned an empty size");
    }
    let words = bytes
        .checked_add(std::mem::size_of::<usize>() - 1)
        .context("process attribute list size overflow")?
        / std::mem::size_of::<usize>();
    let mut storage = vec![0usize; words];
    let list = storage.as_mut_ptr().cast();
    #[allow(unsafe_code)]
    if unsafe { InitializeProcThreadAttributeList(list, 2, 0, &raw mut bytes) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("InitializeProcThreadAttributeList failed");
    }
    let owned = OwnedWindowsAttributeList {
        _storage: storage,
        list,
        _values: std::marker::PhantomData,
    };
    #[allow(unsafe_code)]
    unsafe {
        if UpdateProcThreadAttribute(
            owned.list,
            0,
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            std::mem::size_of_val(handles),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("UpdateProcThreadAttribute(handle list) failed");
        }
        if UpdateProcThreadAttribute(
            owned.list,
            0,
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            jobs.as_ptr().cast(),
            std::mem::size_of_val(jobs),
            std::ptr::null_mut(),
            std::ptr::null(),
        ) == 0
        {
            return Err(std::io::Error::last_os_error()).context(
                "UpdateProcThreadAttribute(Job list) failed; atomic Job assignment requires Windows 10 / Windows Server 2016 or newer",
            );
        }
    }
    Ok(owned)
}

#[cfg(windows)]
fn duplicate_inheritable_standard_handle(
    kind: u32,
    null_access: u32,
) -> Result<OwnedWindowsHandle> {
    use windows_sys::Win32::Foundation::{
        DUPLICATE_SAME_ACCESS, DuplicateHandle, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: GetStdHandle reads the current process standard-handle table.
    #[allow(unsafe_code)]
    let source = unsafe { GetStdHandle(kind) };
    if source.is_null() || source == INVALID_HANDLE_VALUE {
        let nul: Vec<u16> = "NUL\0".encode_utf16().collect();
        let security = SECURITY_ATTRIBUTES {
            nLength: u32::try_from(std::mem::size_of::<SECURITY_ATTRIBUTES>())?,
            lpSecurityDescriptor: std::ptr::null_mut(),
            bInheritHandle: 1,
        };
        // SAFETY: NUL is a fixed NUL-terminated device path and `security`
        // remains live for the synchronous call.
        #[allow(unsafe_code)]
        let handle = unsafe {
            CreateFileW(
                nul.as_ptr(),
                null_access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                &raw const security,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        return OwnedWindowsHandle::from_raw(handle, "CreateFileW(NUL)");
    }

    let mut duplicate = std::ptr::null_mut();
    // SAFETY: source is a live process standard handle; the current-process
    // pseudo handle is valid for both source and target. The duplicate is a
    // new owned, inheritable handle restricted by the HANDLE_LIST attribute.
    #[allow(unsafe_code)]
    if unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            source,
            GetCurrentProcess(),
            &raw mut duplicate,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("DuplicateHandle(stdio) failed");
    }
    OwnedWindowsHandle::from_raw(duplicate, "DuplicateHandle(stdio)")
}

#[cfg(windows)]
fn append_windows_quoted_arg(output: &mut Vec<u16>, arg: &std::ffi::OsStr) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let units = arg.encode_wide().collect::<Vec<_>>();
    if units.contains(&0) {
        bail!("generation argv contains NUL");
    }
    let needs_quotes =
        units.is_empty() || units.iter().any(|unit| matches!(*unit, 0x20 | 0x09 | 0x22));
    if !needs_quotes {
        output.extend(units);
        return Ok(());
    }
    output.push(u16::from(b'"'));
    let mut slashes = 0usize;
    for unit in units {
        if unit == u16::from(b'\\') {
            slashes += 1;
            continue;
        }
        if unit == u16::from(b'"') {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2 + 1));
            output.push(unit);
        } else {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
            output.push(unit);
        }
        slashes = 0;
    }
    output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2));
    output.push(u16::from(b'"'));
    Ok(())
}

#[cfg(windows)]
fn windows_command_line(command: &Command) -> Result<Vec<u16>> {
    let mut output = Vec::new();
    append_windows_quoted_arg(&mut output, command.get_program())?;
    for arg in command.get_args() {
        output.push(u16::from(b' '));
        append_windows_quoted_arg(&mut output, arg)?;
    }
    output.push(0);
    if output.len() > 32_767 {
        bail!("generation command line exceeds CreateProcessW's UTF-16 limit");
    }
    Ok(output)
}

#[cfg(windows)]
fn windows_env_key_identity(key: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;

    key.encode_wide()
        .map(|unit| match unit {
            0x41..=0x5a => unit + 0x20,
            _ => unit,
        })
        .collect()
}

#[cfg(windows)]
fn windows_environment_block(command: &Command) -> Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let mut entries = std::env::vars_os().collect::<Vec<_>>();
    for (key, value) in command.get_envs() {
        let identity = windows_env_key_identity(key);
        entries.retain(|(existing, _)| windows_env_key_identity(existing) != identity);
        if let Some(value) = value {
            entries.push((key.to_os_string(), value.to_os_string()));
        }
    }
    entries.sort_by(|(left, _), (right, _)| {
        windows_env_key_identity(left).cmp(&windows_env_key_identity(right))
    });

    let mut output = Vec::new();
    for (key, value) in entries {
        let key = key.encode_wide().collect::<Vec<_>>();
        let value = value.encode_wide().collect::<Vec<_>>();
        if key.is_empty() || key.contains(&0) || value.contains(&0) {
            bail!("generation environment contains an invalid key or NUL");
        }
        if key[0] != u16::from(b'=') && key.contains(&u16::from(b'=')) {
            bail!("generation environment key contains '='");
        }
        output.extend(key);
        output.push(u16::from(b'='));
        output.extend(value);
        output.push(0);
    }
    // Environment blocks end with an additional NUL; an empty block needs two.
    output.push(0);
    if output.len() == 1 {
        output.push(0);
    }
    if output.len() > 32_767 {
        bail!("generation environment exceeds CreateProcessW's UTF-16 limit");
    }
    Ok(output)
}

#[cfg(windows)]
struct SuspendedWindowsGeneration {
    process: OwnedWindowsHandle,
    primary_thread: Option<OwnedWindowsHandle>,
}

#[cfg(windows)]
impl SuspendedWindowsGeneration {
    fn resume_primary_thread(&mut self) -> Result<()> {
        use windows_sys::Win32::System::Threading::ResumeThread;

        let thread = self
            .primary_thread
            .as_mut()
            .context("generation primary thread handle was already consumed")?;
        // SAFETY: this is the exact hThread returned alongside hProcess by the
        // one CREATE_SUSPENDED call; it has never been resumed or rediscovered.
        #[allow(unsafe_code)]
        let previous_suspend_count = unsafe { ResumeThread(thread.0) };
        if previous_suspend_count == u32::MAX {
            return Err(std::io::Error::last_os_error())
                .context("cannot resume generation primary thread");
        }
        if previous_suspend_count != 1 {
            bail!(
                "generation primary thread had unexpected suspend count {previous_suspend_count}"
            );
        }
        thread.close()?;
        self.primary_thread = None;
        Ok(())
    }

    fn wait(&self) -> Result<i32> {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};

        // SAFETY: the exact process handle remains owned throughout the wait.
        #[allow(unsafe_code)]
        if unsafe { WaitForSingleObject(self.process.0, u32::MAX) } != WAIT_OBJECT_0 {
            return Err(std::io::Error::last_os_error()).context("cannot wait for generation");
        }
        let mut exit_code = 1u32;
        #[allow(unsafe_code)]
        if unsafe { GetExitCodeProcess(self.process.0, &raw mut exit_code) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot read generation exit code");
        }
        Ok(i32::from_ne_bytes(exit_code.to_ne_bytes()))
    }

    fn terminate_and_reap(&mut self, job: &StableLauncherJob) {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        use windows_sys::Win32::System::Threading::{TerminateProcess, WaitForSingleObject};

        // SAFETY: both handles are exact owned handles. Job termination covers
        // any already-created descendants; direct termination is defense in
        // depth, and the unbounded wait prevents a suspended orphan.
        #[allow(unsafe_code)]
        unsafe {
            let _ = TerminateJobObject(job.handle.0, 1);
            let _ = TerminateProcess(self.process.0, 1);
            let _ = WaitForSingleObject(self.process.0, u32::MAX);
        }
    }
}

#[cfg(windows)]
fn create_suspended_windows_generation(
    command: &Command,
    target: &Path,
    job: &StableLauncherJob,
) -> Result<SuspendedWindowsGeneration> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows_sys::Win32::System::Console::{
        STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, EXTENDED_STARTUPINFO_PRESENT,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOEXW,
    };

    let mut command_line = windows_command_line(command)?;
    let environment = windows_environment_block(command)?;
    let application = target
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let current_directory = command.get_current_dir().map(|path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    });

    let standard_handles = [
        duplicate_inheritable_standard_handle(STD_INPUT_HANDLE, GENERIC_READ)?,
        duplicate_inheritable_standard_handle(STD_OUTPUT_HANDLE, GENERIC_WRITE)?,
        duplicate_inheritable_standard_handle(STD_ERROR_HANDLE, GENERIC_WRITE)?,
    ];
    let inherited_handles = [
        standard_handles[0].0,
        standard_handles[1].0,
        standard_handles[2].0,
    ];
    let jobs = [job.handle.0];
    let attributes = windows_process_attributes(&inherited_handles, &jobs)?;
    // SAFETY: zero is the documented initial state for both Win32 structures.
    #[allow(unsafe_code)]
    let mut startup: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    startup.StartupInfo.cb = u32::try_from(std::mem::size_of::<STARTUPINFOEXW>())?;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = inherited_handles[0];
    startup.StartupInfo.hStdOutput = inherited_handles[1];
    startup.StartupInfo.hStdError = inherited_handles[2];
    startup.lpAttributeList = attributes.list;
    #[allow(unsafe_code)]
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    let current_directory_ptr = current_directory
        .as_ref()
        .map_or(std::ptr::null(), |value| value.as_ptr());

    // SAFETY: application/current-directory/environment are NUL-terminated;
    // command_line is writable as required by CreateProcessW; STARTUPINFOEX
    // and its attribute values remain alive for the synchronous call. JOB_LIST
    // gives the child atomic containment, while PROCESS_INFORMATION returns the
    // only process/primary-thread handles lifecycle code ever uses.
    #[allow(unsafe_code)]
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT,
            environment.as_ptr().cast(),
            current_directory_ptr,
            &raw const startup.StartupInfo,
            &raw mut process,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error()).context("CreateProcessW failed");
    }

    let process_handle = OwnedWindowsHandle::from_raw(process.hProcess, "CreateProcessW hProcess")?;
    let thread_handle =
        match OwnedWindowsHandle::from_raw(process.hThread, "CreateProcessW hThread") {
            Ok(handle) => handle,
            Err(error) => {
                // hProcess is already guarded; closing the Job at unwind kills any
                // process created with a malformed PROCESS_INFORMATION result.
                drop(process_handle);
                return Err(error);
            }
        };
    Ok(SuspendedWindowsGeneration {
        process: process_handle,
        primary_thread: Some(thread_handle),
    })
}

#[cfg(windows)]
struct StableLauncherConsoleHandler;

#[cfg(windows)]
impl StableLauncherConsoleHandler {
    fn install() -> Result<Self> {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

        // SAFETY: the callback is a process-lifetime function with the exact
        // PHANDLER_ROUTINE ABI and touches no non-signal-safe Rust state.
        #[allow(unsafe_code)]
        let installed = unsafe { SetConsoleCtrlHandler(Some(stable_launcher_console_handler), 1) };
        if installed == 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot install stable-launcher console handler");
        }
        Ok(Self)
    }
}

#[cfg(windows)]
impl Drop for StableLauncherConsoleHandler {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;

        // SAFETY: removes the same static callback installed above. Failure at
        // process teardown cannot make the completed generation unsafe.
        #[allow(unsafe_code)]
        unsafe {
            let _ = SetConsoleCtrlHandler(Some(stable_launcher_console_handler), 0);
        }
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
unsafe extern "system" fn stable_launcher_console_handler(ctrl_type: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};

    i32::from(stable_launcher_consumes_ctrl_event(
        ctrl_type,
        CTRL_C_EVENT,
        CTRL_BREAK_EVENT,
    ))
}

#[cfg(windows)]
struct StableLauncherJob {
    handle: OwnedWindowsHandle,
}

#[cfg(windows)]
impl StableLauncherJob {
    fn create() -> Result<Self> {
        use windows_sys::Win32::System::JobObjects::CreateJobObjectW;

        // SAFETY: unnamed Job with default security attributes; ownership is
        // transferred immediately to the RAII handle.
        #[allow(unsafe_code)]
        let handle = OwnedWindowsHandle::from_raw(
            unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) },
            "CreateJobObjectW",
        )?;
        configure_stable_launcher_job(handle.0, StableLauncherJobPhase::BeforeGeneration)?;
        // The wrapper deliberately does not join this Job. The child is added
        // atomically via PROC_THREAD_ATTRIBUTE_JOB_LIST, so closing the last
        // handle on panic kills the generation without killing the wrapper in
        // the middle of Rust unwinding or diagnostics.
        Ok(Self { handle })
    }

    fn configure(&self, phase: StableLauncherJobPhase) -> Result<()> {
        configure_stable_launcher_job(self.handle.0, phase)
    }

    fn assert_contains(&self, process: &OwnedWindowsHandle) -> Result<()> {
        use windows_sys::Win32::System::JobObjects::IsProcessInJob;

        let mut contained = 0;
        // SAFETY: both handles are live for the duration of this synchronous
        // query and `contained` points to writable BOOL storage.
        #[allow(unsafe_code)]
        let queried = unsafe { IsProcessInJob(process.0, self.handle.0, &raw mut contained) };
        if queried == 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot verify suspended generation job membership");
        }
        if contained == 0 {
            bail!("suspended generation did not atomically inherit launcher job");
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.handle.close()
    }
}

#[cfg(windows)]
fn configure_stable_launcher_job(
    handle: windows_sys::Win32::Foundation::HANDLE,
    phase: StableLauncherJobPhase,
) -> Result<()> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::System::JobObjects::{
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    #[allow(unsafe_code)]
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = stable_launcher_job_limits(
        phase,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    );
    // SAFETY: `limits` has the exact structure/size required by the information
    // class and remains live for the synchronous call.
    #[allow(unsafe_code)]
    let configured = unsafe {
        SetInformationJobObject(
            handle,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of!(limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot configure generation job phase {phase:?}"));
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn assert_pending_launcher_binding(versions: &Path, stable_launcher: &Path) -> Result<()> {
    if !stable_launcher.is_absolute()
        || stable_launcher.file_name().and_then(|value| value.to_str())
            != Some(launcher_file_name())
    {
        bail!("stable launcher path has an invalid shape");
    }
    let pending = read_canonical_marker(&versions.join(STABLE_LAUNCHER_PENDING_MARKER))
        .context("durable stable-launcher migration marker is missing or invalid")?;
    if pending != stable_launcher {
        bail!("stable launcher path does not exactly match its durable pending marker");
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn valid_windows_process_start_identity(value: &str) -> bool {
    value
        .strip_prefix(WINDOWS_PROCESS_START_IDENTITY_PREFIX)
        .is_some_and(|ticks| {
            !ticks.is_empty()
                && !ticks.starts_with('0')
                && ticks.bytes().all(|byte| byte.is_ascii_digit())
                && ticks.parse::<u64>().is_ok()
        })
}

#[cfg(any(windows, test))]
fn windows_activation_commit_args(
    parent_pid: u32,
    parent_start_identity: &str,
    stable_launcher: &Path,
) -> Vec<OsString> {
    vec![
        OsString::from(WINDOWS_ACTIVATION_COMMIT_SUBCOMMAND),
        OsString::from("--parent-pid"),
        OsString::from(parent_pid.to_string()),
        OsString::from("--parent-start-identity"),
        OsString::from(parent_start_identity),
        OsString::from("--stable-launcher"),
        stable_launcher.as_os_str().to_os_string(),
    ]
}

#[cfg(windows)]
fn assert_windows_activation_request(parent_pid: u32, stable_launcher: &Path) -> Result<PathBuf> {
    if parent_pid <= 1 {
        bail!("Windows stable-launcher activation parent PID must be greater than one");
    }
    let versions = native_installer_versions_dir()?;
    assert_pending_launcher_binding(&versions, stable_launcher)?;
    let generation =
        validate_generation_root(&versions, &versions.join(CURRENT_GENERATION_MARKER))?;
    let current_exe = std::env::current_exe().context("cannot resolve migration helper")?;
    if !same_path(&current_exe, &generation.join("crabcode.exe")) {
        bail!("migration helper is not the selected signed generation launcher");
    }
    Ok(current_exe)
}

#[cfg(windows)]
fn windows_process_start_identity(
    process: windows_sys::Win32::Foundation::HANDLE,
) -> Result<String> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetProcessTimes;

    // Difference between 0001-01-01 (`DateTime.Ticks`) and the Win32 FILETIME
    // epoch 1601-01-01. This deliberately matches the TS/PowerShell identity
    // used by pidLock.ts.
    const DOTNET_FILETIME_OFFSET_TICKS: u64 = 504_911_232_000_000_000;

    // SAFETY: `process` is a live retained process handle with query rights;
    // all four FILETIME outputs are writable for the synchronous call.
    #[allow(unsafe_code)]
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    #[allow(unsafe_code)]
    if unsafe {
        GetProcessTimes(
            process,
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("cannot read Windows activation parent birth identity");
    }
    let filetime = (u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime);
    let ticks = filetime
        .checked_add(DOTNET_FILETIME_OFFSET_TICKS)
        .context("Windows activation parent birth identity overflow")?;
    let identity = format!("{WINDOWS_PROCESS_START_IDENTITY_PREFIX}{ticks}");
    if !valid_windows_process_start_identity(&identity) {
        bail!("Windows activation parent birth identity is not canonical");
    }
    Ok(identity)
}

#[cfg(windows)]
fn open_windows_activation_parent(parent_pid: u32) -> Result<Option<OwnedWindowsHandle>> {
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    if parent_pid <= 1 {
        bail!("Windows stable-launcher activation parent PID must be greater than one");
    }
    // SAFETY: OpenProcess returns a new owned handle or null. Query + wait are
    // the only rights needed; the RAII guard closes every successful handle.
    #[allow(unsafe_code)]
    let raw = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
            0,
            parent_pid,
        )
    };
    if raw.is_null() {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error().map(|value| value as u32) == Some(ERROR_INVALID_PARAMETER) {
            // The exact parent is already gone. The caller's durable pending
            // marker remains the authority; replacement still retries any
            // transient image sharing violation from the outer launcher.
            return Ok(None);
        }
        return Err(error).with_context(|| {
            format!("cannot open Windows activation parent process {parent_pid}")
        });
    }
    OwnedWindowsHandle::from_raw(raw, "OpenProcess(activation parent)").map(Some)
}

#[cfg(windows)]
fn observe_live_windows_activation_parent(parent_pid: u32) -> Result<String> {
    let parent = open_windows_activation_parent(parent_pid)?
        .context("Windows activation parent exited before its native broker observed it")?;
    windows_process_start_identity(parent.0)
}

#[cfg(windows)]
fn wait_for_windows_activation_parent(
    parent_pid: u32,
    expected_start_identity: &str,
) -> Result<()> {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;

    if !valid_windows_process_start_identity(expected_start_identity) {
        bail!("Windows activation parent birth identity is not canonical");
    }
    let Some(parent) = open_windows_activation_parent(parent_pid)? else {
        return Ok(());
    };
    let observed = windows_process_start_identity(parent.0)?;
    if observed != expected_start_identity {
        bail!("Windows activation parent PID was reused before the commit helper could wait");
    }
    // SAFETY: this is the exact retained handle whose birth identity matched
    // the broker-observed parent. No PID lookup participates after this point.
    #[allow(unsafe_code)]
    if unsafe { WaitForSingleObject(parent.0, u32::MAX) } != WAIT_OBJECT_0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot wait for Windows activation parent exit");
    }
    Ok(())
}

#[cfg(windows)]
fn current_windows_job_denies_explicit_breakaway() -> Result<bool> {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::System::JobObjects::{
        IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        QueryInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut in_job = 0;
    // SAFETY: the current-process pseudo-handle is always valid; a null Job
    // queries membership in any Job associated with it.
    #[allow(unsafe_code)]
    if unsafe { IsProcessInJob(GetCurrentProcess(), std::ptr::null_mut(), &raw mut in_job) } == 0 {
        return Err(std::io::Error::last_os_error())
            .context("cannot determine whether activation broker is inside a Windows Job");
    }
    if in_job == 0 {
        return Ok(false);
    }

    // Querying a null Job handle is the documented way to inspect the calling
    // process's immediate Job. This distinguishes the released 1.0.13 policy
    // (KILL_ON_CLOSE only) from an unrelated ACCESS_DENIED inside a Job that
    // already permits explicit or silent breakaway.
    #[allow(unsafe_code)]
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    #[allow(unsafe_code)]
    if unsafe {
        QueryInformationJobObject(
            std::ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of_mut!(limits).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .context("cannot inspect activation broker Windows Job limits");
    }
    Ok(windows_job_denies_explicit_breakaway(
        true,
        limits.BasicLimitInformation.LimitFlags,
        JOB_OBJECT_LIMIT_BREAKAWAY_OK,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
    ))
}

/// First stage of a Windows migration. Bun may itself be inside a
/// kill-on-close supervisor Job, so it launches this ordinary broker and waits
/// for a zero exit. The broker validates the signed selected generation,
/// captures the exact parent birth identity, then uses a native, structured
/// CreateProcess request with explicit breakaway to start the real commit
/// helper. There is no shell or fallback without breakaway.
#[cfg(windows)]
pub(crate) fn launch_prepared_windows_activation_helper(
    parent_pid: u32,
    stable_launcher: &Path,
) -> Result<WindowsActivationBrokerOutcome> {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
    };

    let current_exe = assert_windows_activation_request(parent_pid, stable_launcher)?;
    let parent_start_identity = observe_live_windows_activation_parent(parent_pid)?;
    let args = windows_activation_commit_args(parent_pid, &parent_start_identity, stable_launcher);
    let mut commit = Command::new(&current_exe);
    commit
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    let child = match commit.spawn() {
        Ok(child) => child,
        Err(error) if error.raw_os_error().map(|code| code as u32) == Some(ERROR_ACCESS_DENIED) => {
            if current_windows_job_denies_explicit_breakaway()? {
                // Released 1.0.13 binds its supervisor to a KILL_ON_CLOSE Job
                // without BREAKAWAY_OK. Do not corrupt or clear the durable
                // `.current`/`.pending` handoff and do not pretend activation
                // completed. The updater maps this dedicated status to
                // activationPending/restart-required; the first fresh 1.0.15
                // supervisor (whose Job explicitly permits breakaway) retries
                // the same signed local migration before any remote policy.
                return Ok(WindowsActivationBrokerOutcome::RestartRequired);
            }
            return Err(error)
                .context("cannot create the detached Windows activation commit helper");
        }
        Err(error) => {
            return Err(error)
                .context("cannot create the detached Windows activation commit helper");
        }
    };
    // Dropping std::process::Child closes only the broker's process handle on
    // Windows; it does not terminate the explicitly breakaway child.
    drop(child);
    Ok(WindowsActivationBrokerOutcome::Scheduled)
}

#[cfg(windows)]
fn move_replace_write_through(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both pointers are valid NUL-terminated UTF-16 buffers for the
    // duration of the call. MoveFileExW owns neither buffer.
    #[allow(unsafe_code)]
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn write_launcher_protocol_marker(versions: &Path, stable_launcher: &Path) -> Result<()> {
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    let marker = versions.join(STABLE_LAUNCHER_PROTOCOL_MARKER);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp = versions.join(format!(
        "{STABLE_LAUNCHER_PROTOCOL_MARKER}.tmp.{}.{nonce}",
        std::process::id(),
    ));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temp)
        .with_context(|| format!("cannot create {}", temp.display()))?;
    writeln!(file, "{}", stable_launcher.display())?;
    file.sync_all()?;
    move_replace_write_through(&temp, &marker)
        .with_context(|| format!("cannot publish {}", marker.display()))?;
    Ok(())
}

/// One-time Windows migration. It runs from the new immutable generation,
/// waits for the old stable launcher process to exit, atomically replaces that
/// single launcher, and only then records launcher protocol v1. No symlink,
/// Developer Mode or split companion copy is used.
#[cfg(windows)]
pub(crate) fn activate_prepared_windows_launcher(
    parent_pid: u32,
    parent_start_identity: &str,
    stable_launcher: &Path,
) -> Result<()> {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    let current_exe = assert_windows_activation_request(parent_pid, stable_launcher)?;
    wait_for_windows_activation_parent(parent_pid, parent_start_identity)?;
    // The wait can be arbitrarily long. Re-read every durable binding after it
    // completes so a newer activation cannot be overwritten by an older
    // helper that validated `.current` only before sleeping.
    let current_after_wait = assert_windows_activation_request(parent_pid, stable_launcher)?;
    if !same_path(&current_exe, &current_after_wait) {
        bail!("selected Windows activation generation changed while waiting for its parent");
    }
    let versions = native_installer_versions_dir()?;
    let parent = stable_launcher
        .parent()
        .context("stable launcher has no parent")?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("cannot inspect stable launcher parent {}", parent.display()))?;
    if !parent_metadata.is_dir() || metadata_is_reparse_point(&parent_metadata) {
        bail!("stable launcher parent must be a real directory");
    }
    match fs::symlink_metadata(stable_launcher) {
        Ok(metadata)
            if !metadata.is_file()
                || metadata_is_reparse_point(&metadata)
                || metadata.len() == 0 =>
        {
            bail!("existing stable launcher must be a real regular non-empty file");
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot inspect existing stable launcher"),
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let prepared = parent.join(format!(
        ".crabcode.exe.launcher-v1.{}.{nonce}.tmp",
        std::process::id(),
    ));
    let mut source = fs::File::open(&current_exe)?;
    let mut destination = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&prepared)
        .with_context(|| format!("cannot prepare {}", prepared.display()))?;
    std::io::copy(&mut source, &mut destination)?;
    destination.sync_all()?;
    drop(destination);

    let mut last_error = None;
    for _ in 0..1200 {
        match move_replace_write_through(&prepared, stable_launcher) {
            Ok(()) => {
                write_launcher_protocol_marker(&versions, stable_launcher)?;
                let _ = fs::remove_file(versions.join(STABLE_LAUNCHER_PENDING_MARKER));
                return Ok(());
            }
            Err(error) => {
                last_error = Some(error);
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    let _ = fs::remove_file(&prepared);
    bail!(
        "timed out replacing the Windows stable launcher: {}",
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_file(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"fixture").unwrap();
    }

    fn write_generation_fixture(generation: &Path) -> PathBuf {
        let launcher = generation.join(launcher_file_name());
        for relative in [
            launcher_file_name(),
            tui_file_name(),
            memory_file_name(),
            if cfg!(windows) { "bun.exe" } else { "bun" },
            "dist/tui-runtime/index.js",
            "build-id",
            "release-manifest.json",
            "release-manifest.digest.json",
        ] {
            write_file(&generation.join(relative));
        }
        launcher
    }

    fn generation_fixture() -> (TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = TempDir::new().unwrap();
        let versions = temp.path().join("versions");
        let generation = versions.join("1.2.3");
        let launcher = write_generation_fixture(&generation);
        fs::write(
            versions.join(CURRENT_GENERATION_MARKER),
            format!("{}\n", generation.display()),
        )
        .unwrap();
        (temp, versions, generation, launcher)
    }

    #[test]
    fn only_the_exact_current_generation_has_same_version_handoff_authority() {
        let (_temp, versions, generation, launcher) = generation_fixture();
        let memory = generation.join(memory_file_name());
        assert!(
            executable_is_selected_generation_in(&launcher, &memory, "fixture", &versions).unwrap()
        );

        let stale_generation = versions.join("1.2.2");
        let stale_launcher = write_generation_fixture(&stale_generation);
        let stale_memory = stale_generation.join(memory_file_name());
        assert!(is_generation_launcher(&stale_launcher, &versions));
        assert!(
            !executable_is_selected_generation_in(
                &stale_launcher,
                &stale_memory,
                "fixture",
                &versions,
            )
            .unwrap(),
            "a valid leased generation that is no longer selected must not promote an unordered peer"
        );

        assert!(
            !executable_is_selected_generation_in(&launcher, &stale_memory, "fixture", &versions,)
                .unwrap(),
            "an orchestrator override outside the selected generation cannot inherit launcher authority"
        );
        assert!(
            !executable_is_selected_generation_in(
                &launcher,
                &memory,
                "other-build+aaaaaaaaaaaa",
                &versions,
            )
            .unwrap(),
            "a selected path cannot authorize a different process build"
        );

        fs::write(
            versions.join(CURRENT_GENERATION_MARKER),
            format!("{}\n", stale_generation.display()),
        )
        .unwrap();
        assert!(
            !executable_is_selected_generation_in(&launcher, &memory, "fixture", &versions)
                .unwrap()
        );
        assert!(
            executable_is_selected_generation_in(
                &stale_launcher,
                &stale_memory,
                "fixture",
                &versions,
            )
            .unwrap()
        );

        fs::write(
            versions.join(CURRENT_GENERATION_MARKER),
            format!("{}\n", generation.parent().unwrap().display()),
        )
        .unwrap();
        assert!(
            executable_is_selected_generation_in(&launcher, &memory, "fixture", &versions).is_err()
        );
    }

    #[test]
    fn selected_generation_authority_fails_closed_for_missing_or_malformed_files() {
        let (_temp, versions, generation, launcher) = generation_fixture();
        let memory = generation.join(memory_file_name());
        let marker = versions.join(CURRENT_GENERATION_MARKER);

        fs::remove_file(&marker).unwrap();
        assert!(
            !executable_is_selected_generation_in(&launcher, &memory, "fixture", &versions)
                .unwrap()
        );

        fs::write(&marker, "x".repeat((MAX_MARKER_BYTES + 1) as usize)).unwrap();
        assert!(
            executable_is_selected_generation_in(&launcher, &memory, "fixture", &versions).is_err()
        );

        fs::write(&marker, format!("{}\n", generation.display())).unwrap();
        fs::write(generation.join("build-id"), b"fixture\nsecond\n").unwrap();
        assert!(
            executable_is_selected_generation_in(&launcher, &memory, "fixture", &versions).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn selected_generation_authority_rejects_symlinks_and_non_utf8_build_id() {
        use std::os::unix::fs::symlink;

        {
            let (temp, versions, generation, launcher) = generation_fixture();
            let memory = generation.join(memory_file_name());
            let marker = versions.join(CURRENT_GENERATION_MARKER);
            let real_marker = temp.path().join("real-current");
            fs::write(&real_marker, format!("{}\n", generation.display())).unwrap();
            fs::remove_file(&marker).unwrap();
            symlink(&real_marker, &marker).unwrap();
            assert!(
                executable_is_selected_generation_in(&launcher, &memory, "fixture", &versions)
                    .is_err()
            );
        }

        {
            let (temp, versions, generation, launcher) = generation_fixture();
            let memory = generation.join(memory_file_name());
            let build_id = generation.join("build-id");
            let real_build_id = temp.path().join("real-build-id");
            fs::write(&real_build_id, b"fixture\n").unwrap();
            fs::remove_file(&build_id).unwrap();
            symlink(&real_build_id, &build_id).unwrap();
            assert!(
                executable_is_selected_generation_in(&launcher, &memory, "fixture", &versions)
                    .is_err()
            );
        }

        {
            let (_temp, versions, generation, launcher) = generation_fixture();
            let memory = generation.join(memory_file_name());
            fs::write(generation.join("build-id"), [0xff, 0xfe]).unwrap();
            assert!(
                executable_is_selected_generation_in(&launcher, &memory, "fixture", &versions)
                    .is_err()
            );
        }
    }

    #[test]
    fn marker_accepts_exact_one_generation_and_rejects_escape_or_reparse() {
        let (_temp, versions, generation, _launcher) = generation_fixture();
        assert_eq!(
            validate_generation_root(&versions, &versions.join(CURRENT_GENERATION_MARKER)).unwrap(),
            fs::canonicalize(&generation).unwrap()
        );

        fs::write(
            versions.join(CURRENT_GENERATION_MARKER),
            format!("{}\n", versions.parent().unwrap().display()),
        )
        .unwrap();
        assert!(
            validate_generation_root(&versions, &versions.join(CURRENT_GENERATION_MARKER))
                .unwrap_err()
                .to_string()
                .contains("escapes")
        );
    }

    #[test]
    fn pure_tui_generation_contract_requires_every_pre_runtime_component() {
        let (_temp, versions, generation, _launcher) = generation_fixture();
        for relative in [
            tui_file_name(),
            memory_file_name(),
            if cfg!(windows) { "bun.exe" } else { "bun" },
            "dist/tui-runtime/index.js",
            "build-id",
            "release-manifest.json",
            "release-manifest.digest.json",
        ] {
            let path = generation.join(relative);
            let contents = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            assert!(
                validate_generation_root(&versions, &versions.join(CURRENT_GENERATION_MARKER))
                    .unwrap_err()
                    .to_string()
                    .contains("missing"),
                "{relative}"
            );
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(path, contents).unwrap();
        }
    }

    #[test]
    fn release_version_segment_is_strict() {
        for valid in ["1.2.3", "1.2.3-rc.1", "0.0.0-dev.abc"] {
            assert!(is_release_version_segment(valid), "{valid}");
        }
        for invalid in [
            "v1.2.3",
            "01.2.3",
            "1.2",
            "1.2.3/escape",
            ".candidate",
            "1.2.3-",
            "1.2.3-rc..1",
        ] {
            assert!(!is_release_version_segment(invalid), "{invalid}");
        }
    }

    #[test]
    fn final_or_pending_protocol_marker_binds_a_custom_flat_launcher() {
        let (temp, versions, _generation, launcher) = generation_fixture();
        let stable = temp.path().join("custom-flat").join(launcher_file_name());
        write_file(&stable);

        for marker_name in [
            STABLE_LAUNCHER_PROTOCOL_MARKER,
            STABLE_LAUNCHER_PENDING_MARKER,
        ] {
            for stale in [
                STABLE_LAUNCHER_PROTOCOL_MARKER,
                STABLE_LAUNCHER_PENDING_MARKER,
            ] {
                let _ = fs::remove_file(versions.join(stale));
            }
            fs::write(
                versions.join(marker_name),
                format!("{}\n", stable.display()),
            )
            .unwrap();
            let selected = handoff_target(&stable, &versions).unwrap().unwrap();
            assert!(same_path(&selected, &launcher));
        }

        fs::remove_file(versions.join(CURRENT_GENERATION_MARKER)).unwrap();
        assert!(
            handoff_target(&stable, &versions)
                .unwrap_err()
                .to_string()
                .contains(".current is missing")
        );
    }

    #[test]
    fn migration_mutation_requires_the_exact_durable_pending_binding() {
        let (temp, versions, _generation, _launcher) = generation_fixture();
        let stable = temp.path().join("custom-flat").join(launcher_file_name());
        fs::create_dir_all(stable.parent().unwrap()).unwrap();

        assert!(assert_pending_launcher_binding(&versions, &stable).is_err());
        fs::write(
            versions.join(STABLE_LAUNCHER_PENDING_MARKER),
            format!(
                "{}\n",
                temp.path()
                    .join("other")
                    .join(launcher_file_name())
                    .display()
            ),
        )
        .unwrap();
        assert!(assert_pending_launcher_binding(&versions, &stable).is_err());
        fs::write(
            versions.join(STABLE_LAUNCHER_PENDING_MARKER),
            format!("{}\n", stable.display()),
        )
        .unwrap();
        assert!(assert_pending_launcher_binding(&versions, &stable).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn signal_terminated_generation_preserves_shell_exit_semantics() {
        let status = Command::new("sh")
            .args(["-c", "kill -TERM $$"])
            .status()
            .unwrap();
        assert_eq!(child_exit_code(status), 128 + 15);
    }

    #[test]
    fn windows_job_phase_policy_has_atomic_direct_child_ownership() {
        const KILL: u32 = 0b0001;
        const SILENT: u32 = 0b0100;

        assert_eq!(
            stable_launcher_job_limits(StableLauncherJobPhase::BeforeGeneration, KILL, SILENT,),
            KILL,
            "silent breakaway must be disabled while the suspended child inherits the job",
        );
        assert_eq!(
            stable_launcher_job_limits(StableLauncherJobPhase::GenerationRunning, KILL, SILENT,),
            KILL | SILENT,
            "the direct generation stays contained while descendants remain separately owned",
        );
    }

    #[test]
    fn deferred_activation_requires_a_job_that_actually_denies_breakaway() {
        const BREAKAWAY: u32 = 0b0010;
        const SILENT: u32 = 0b0100;

        assert!(!windows_job_denies_explicit_breakaway(
            false, 0, BREAKAWAY, SILENT,
        ));
        assert!(windows_job_denies_explicit_breakaway(
            true, 0, BREAKAWAY, SILENT,
        ));
        assert!(!windows_job_denies_explicit_breakaway(
            true, BREAKAWAY, BREAKAWAY, SILENT,
        ));
        assert!(!windows_job_denies_explicit_breakaway(
            true, SILENT, BREAKAWAY, SILENT,
        ));
    }

    #[test]
    fn windows_wrapper_consumes_only_interrupt_console_events() {
        const CTRL_C: u32 = 0;
        const CTRL_BREAK: u32 = 1;

        assert!(stable_launcher_consumes_ctrl_event(
            CTRL_C, CTRL_C, CTRL_BREAK
        ));
        assert!(stable_launcher_consumes_ctrl_event(
            CTRL_BREAK, CTRL_C, CTRL_BREAK,
        ));
        for close_or_shutdown in [2, 5, 6] {
            assert!(!stable_launcher_consumes_ctrl_event(
                close_or_shutdown,
                CTRL_C,
                CTRL_BREAK,
            ));
        }
    }

    #[test]
    fn windows_activation_parent_identity_is_canonical_and_strict() {
        assert!(valid_windows_process_start_identity(
            "win32-process-start:638881234567890123"
        ));
        for invalid in [
            "",
            "win32-process-start:",
            "win32-process-start:0",
            "win32-process-start:01",
            "win32-process-start:-1",
            "win32-process-start:1.0",
            "linux-proc-startticks:123",
        ] {
            assert!(!valid_windows_process_start_identity(invalid), "{invalid}");
        }
    }

    #[test]
    fn windows_activation_commit_argv_is_structured_and_lossless() {
        let stable = PathBuf::from(r"C:\Program Files\Crab Code\crabcode.exe");
        let args =
            windows_activation_commit_args(4242, "win32-process-start:638881234567890123", &stable);
        assert_eq!(
            args,
            [
                OsString::from("__native-update-activate-commit"),
                OsString::from("--parent-pid"),
                OsString::from("4242"),
                OsString::from("--parent-start-identity"),
                OsString::from("win32-process-start:638881234567890123"),
                OsString::from("--stable-launcher"),
                stable.into_os_string(),
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn generation_contract_rejects_a_symlinked_runtime_parent() {
        use std::os::unix::fs::symlink;

        let (temp, versions, generation, _launcher) = generation_fixture();
        fs::remove_dir_all(generation.join("dist")).unwrap();
        let outside = temp.path().join("outside-dist");
        write_file(&outside.join("tui-runtime").join("index.js"));
        symlink(&outside, generation.join("dist")).unwrap();

        assert!(
            validate_generation_root(&versions, &versions.join(CURRENT_GENERATION_MARKER))
                .unwrap_err()
                .to_string()
                .contains("reparse point")
        );
    }

    #[test]
    fn a_generation_launcher_never_recurses_to_itself() {
        let (_temp, versions, _generation, launcher) = generation_fixture();
        assert!(is_generation_launcher(&launcher, &versions));
        assert_eq!(handoff_target(&launcher, &versions).unwrap(), None);
    }
}

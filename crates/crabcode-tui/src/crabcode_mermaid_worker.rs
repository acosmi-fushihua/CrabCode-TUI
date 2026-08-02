//! Lazy, out-of-process Mermaid PNG rendering for the native CrabCode TUI.
//!
//! Mermaid fences are rendered as terminal box art by the Markdown path.  PNG
//! generation is a separate, user-invoked affordance: the TUI renders only
//! after an Open Image or Copy Image Path action, keeps the expensive renderer
//! off the draw thread, and never displays the PNG inline.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use crabcode_mermaid::{
    MermaidTheme, RenderLimits, RenderParams, RenderedDiagram, SubprocessError, default_engine,
    render_checked, run_with_timeout,
};
use indexmap::IndexMap;

pub(crate) const MERMAID_RENDER_SUBCOMMAND: &str = "__crabcode-mermaid-render";

const SESSION_DISK_CAP_BYTES: u64 = 200 * 1024 * 1024;
const APPROX_CELL_W_PX: u32 = 8;
const RENDER_SCALE: u32 = 2;
const MIN_TARGET_WIDTH_PX: u32 = 320;
const MAX_TARGET_WIDTH_PX: u32 = 1600;
const MAX_TARGET_HEIGHT_PX: u32 = 2400;
const OPEN_MIN_WIDTH_PX: u32 = 2560;
const OPEN_MAX_HEIGHT_PX: u32 = 8192;
const RENDER_TIMEOUT: Duration = Duration::from_millis(3000);
const SWEEP_EVERY_N_WRITES: u32 = 8;
const WIDTH_BUCKET_COLS: u16 = 8;
const OPEN_WIDTH_BUCKET: u16 = u16::MAX;
const RENDER_REVISION: u8 = 3;
const CHILD_WATCHDOG_SLACK: Duration = Duration::from_secs(3);
const CHILD_WATCHDOG_EXIT_CODE: i32 = 2;
static NEXT_ATOMIC_WRITE_ID: AtomicU64 = AtomicU64::new(0);

#[cfg(target_os = "linux")]
const CHILD_ADDRESS_SPACE_CAP_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    static TEST_CACHE_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_test_cache_root<R>(root: &Path, run: impl FnOnce() -> R) -> R {
    struct Reset(Option<PathBuf>);
    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_CACHE_ROOT.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }

    let previous = TEST_CACHE_ROOT.with(|slot| slot.replace(Some(root.to_path_buf())));
    let _reset = Reset(previous);
    run()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub(crate) enum MermaidRenderQuality {
    #[default]
    Terminal,
    Open,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct MermaidCacheKey {
    source_hash: [u8; 32],
    /// Concrete renderer theme tag.  The default CrabCode dark theme uses the
    /// pinned renderer's dark-default tag `0`; tag `1` is the light surface.
    theme_tag: u8,
    width_bucket: u16,
    quality: MermaidRenderQuality,
}

impl MermaidCacheKey {
    pub(crate) fn derive(
        source: &str,
        theme_tag: u8,
        target_width_cols: u16,
        quality: MermaidRenderQuality,
    ) -> Self {
        let width_bucket = match quality {
            MermaidRenderQuality::Terminal => target_width_cols / WIDTH_BUCKET_COLS,
            MermaidRenderQuality::Open => OPEN_WIDTH_BUCKET,
        };
        Self {
            source_hash: *blake3::hash(source.as_bytes()).as_bytes(),
            theme_tag,
            width_bucket,
            quality,
        }
    }

    pub(crate) fn cache_filename(&self) -> String {
        let mut name = String::with_capacity(88);
        for byte in self.source_hash {
            let _ = write!(name, "{byte:02x}");
        }
        let quality_tag = match self.quality {
            MermaidRenderQuality::Terminal => "t",
            MermaidRenderQuality::Open => "o",
        };
        let _ = write!(
            name,
            "-{}-{}-{}-r{RENDER_REVISION}.png",
            self.theme_tag, self.width_bucket, quality_tag
        );
        name
    }
}

pub(crate) struct MermaidJob {
    pub(crate) key: MermaidCacheKey,
    pub(crate) source: String,
    pub(crate) out_path: PathBuf,
    pub(crate) theme_dark: bool,
    pub(crate) target_width_px: u32,
    pub(crate) quality: MermaidRenderQuality,
}

#[derive(Debug)]
pub(crate) enum MermaidOutcome {
    Ready { path: PathBuf },
    Failed { reason: &'static str },
}

#[derive(Debug)]
pub(crate) struct MermaidResult {
    pub(crate) key: MermaidCacheKey,
    pub(crate) outcome: MermaidOutcome,
}

#[derive(Debug)]
struct PendingMermaidAction {
    key: MermaidCacheKey,
    action: crate::tui_links::MermaidAffordanceAction,
}

pub(crate) struct MermaidRuntime {
    tx: Option<Sender<MermaidJob>>,
    rx: Option<Receiver<MermaidResult>>,
    pending: Vec<PendingMermaidAction>,
}

#[derive(Debug)]
pub(crate) enum MermaidRequest {
    Immediate {
        action: crate::tui_links::MermaidAffordanceAction,
        path: PathBuf,
    },
    Queued,
    AlreadyPending,
    NotReady,
    WorkerUnavailable,
}

#[derive(Debug)]
pub(crate) struct MermaidCompletion {
    pub(crate) action: crate::tui_links::MermaidAffordanceAction,
    pub(crate) outcome: MermaidOutcome,
}

impl MermaidRuntime {
    pub(crate) fn new() -> Self {
        Self {
            tx: None,
            rx: None,
            pending: Vec::new(),
        }
    }

    fn ensure_worker(&mut self) {
        if self.tx.is_none() {
            let (tx, rx) = spawn_worker();
            self.tx = Some(tx);
            self.rx = Some(rx);
        }
    }

    pub(crate) fn is_rendering(&self, source: &str) -> bool {
        if self.pending.is_empty() {
            return false;
        }
        let source_hash = *blake3::hash(source.as_bytes()).as_bytes();
        self.pending
            .iter()
            .any(|pending| pending.key.source_hash == source_hash)
    }

    pub(crate) fn has_pending_work(&self) -> bool {
        !self.pending.is_empty()
    }

    fn has_pending(
        &self,
        key: &MermaidCacheKey,
        action: crate::tui_links::MermaidAffordanceAction,
    ) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.key == *key && pending.action == action)
    }

    pub(crate) fn request(
        &mut self,
        source: String,
        action: crate::tui_links::MermaidAffordanceAction,
        session_id: Option<&str>,
        theme_tag: u8,
        theme_dark: bool,
        content_cols: u16,
    ) -> MermaidRequest {
        let Some(directory) = session_id.and_then(default_session_cache_dir) else {
            return MermaidRequest::NotReady;
        };
        let quality = MermaidRenderQuality::Open;
        let key = MermaidCacheKey::derive(&source, theme_tag, content_cols, quality);
        let out_path = directory.join(key.cache_filename());
        if self.has_pending(&key, action) {
            return MermaidRequest::AlreadyPending;
        }
        if read_cached_png(&out_path) {
            return MermaidRequest::Immediate {
                action,
                path: out_path,
            };
        }
        let job = MermaidJob {
            key: key.clone(),
            source,
            out_path,
            theme_dark,
            target_width_px: target_width_px(content_cols),
            quality,
        };
        self.ensure_worker();
        if self.tx.as_ref().is_none_or(|tx| tx.send(job).is_err()) {
            return MermaidRequest::WorkerUnavailable;
        }
        self.pending.push(PendingMermaidAction { key, action });
        MermaidRequest::Queued
    }

    pub(crate) fn poll(&mut self) -> Vec<MermaidCompletion> {
        let mut completed = Vec::new();
        let Some(rx) = self.rx.as_ref() else {
            return completed;
        };
        while let Ok(result) = rx.try_recv() {
            let actions = take_pending_for(&mut self.pending, &result.key);
            for action in actions {
                let outcome = match &result.outcome {
                    MermaidOutcome::Ready { path } => MermaidOutcome::Ready { path: path.clone() },
                    MermaidOutcome::Failed { reason } => MermaidOutcome::Failed { reason },
                };
                completed.push(MermaidCompletion { action, outcome });
            }
        }
        completed
    }
}

fn take_pending_for(
    pending: &mut Vec<PendingMermaidAction>,
    key: &MermaidCacheKey,
) -> Vec<crate::tui_links::MermaidAffordanceAction> {
    let mut actions = Vec::new();
    pending.retain(|entry| {
        if entry.key == *key {
            actions.push(entry.action);
            false
        } else {
            true
        }
    });
    actions
}

type RenderFn = dyn Fn(&MermaidJob, Duration) -> Result<(), &'static str> + Send + Sync;

fn default_render_fn() -> Arc<RenderFn> {
    #[cfg(test)]
    {
        Arc::new(render_in_process_for_tests)
    }
    #[cfg(not(test))]
    {
        match std::env::current_exe() {
            Ok(executable) => Arc::new(move |job: &MermaidJob, timeout: Duration| {
                render_via_subprocess(
                    &executable,
                    &job.source,
                    job.theme_dark,
                    job.target_width_px,
                    job.quality,
                    &job.out_path,
                    timeout,
                )
            }),
            Err(_) => Arc::new(|_: &MermaidJob, _: Duration| Err("no_executable")),
        }
    }
}

pub(crate) fn spawn_worker() -> (Sender<MermaidJob>, Receiver<MermaidResult>) {
    spawn_worker_with(default_render_fn())
}

fn spawn_worker_with(render: Arc<RenderFn>) -> (Sender<MermaidJob>, Receiver<MermaidResult>) {
    let (job_tx, job_rx) = std::sync::mpsc::channel::<MermaidJob>();
    let (result_tx, result_rx) = std::sync::mpsc::channel::<MermaidResult>();
    std::thread::Builder::new()
        .name("crabcode-mermaid-render".to_string())
        .spawn(move || {
            let mut writes_since_sweep = 0_u32;
            let mut swept_directories = std::collections::HashSet::new();
            while let Ok(first) = job_rx.recv() {
                for (_, job) in drain_coalesced(first, &job_rx) {
                    if let Some(directory) = job.out_path.parent()
                        && swept_directories.insert(directory.to_path_buf())
                    {
                        // This can decode and delete up to the cache budget, so
                        // it belongs on the worker rather than the TUI event
                        // thread. Running it before this directory's first
                        // render also prevents a sweep/write race.
                        sweep_session_cache(directory, SESSION_DISK_CAP_BYTES);
                    }
                    let (outcome, wrote) = render_job(render.as_ref(), &job, RENDER_TIMEOUT);
                    if wrote {
                        writes_since_sweep = writes_since_sweep.saturating_add(1);
                        if writes_since_sweep >= SWEEP_EVERY_N_WRITES {
                            writes_since_sweep = 0;
                            if let Some(directory) = job.out_path.parent() {
                                sweep_session_cache(directory, SESSION_DISK_CAP_BYTES);
                            }
                        }
                    }
                    if result_tx
                        .send(MermaidResult {
                            key: job.key,
                            outcome,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            }
        })
        .expect("spawn CrabCode Mermaid worker");
    (job_tx, result_rx)
}

fn drain_coalesced(
    first: MermaidJob,
    receiver: &Receiver<MermaidJob>,
) -> IndexMap<MermaidCacheKey, MermaidJob> {
    let mut pending = IndexMap::new();
    pending.insert(first.key.clone(), first);
    while let Ok(job) = receiver.try_recv() {
        pending.insert(job.key.clone(), job);
    }
    pending
}

pub(crate) fn representative_content_cols(terminal_width: u16) -> u16 {
    const TRANSCRIPT_CHROME: u16 = 2;
    const TIMESTAMP_RESERVE: u16 = 10;
    terminal_width
        .saturating_sub(TRANSCRIPT_CHROME)
        .saturating_sub(TIMESTAMP_RESERVE)
        .max(20)
}

pub(crate) fn target_width_px(content_cols: u16) -> u32 {
    (u32::from(content_cols) * APPROX_CELL_W_PX * RENDER_SCALE)
        .clamp(MIN_TARGET_WIDTH_PX, MAX_TARGET_WIDTH_PX)
}

pub(crate) fn default_session_cache_dir(session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty()
        || session_id.len() > 128
        || !session_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }
    #[cfg(test)]
    if let Some(root) = TEST_CACHE_ROOT.with(|slot| slot.borrow().clone()) {
        return Some(root.join(session_id));
    }
    Some(
        crabcode_config_root()?
            .join("cache")
            .join("native-tui")
            .join("mermaid")
            .join(session_id),
    )
}

fn crabcode_config_root() -> Option<PathBuf> {
    crabcode_config_root_from(
        std::env::var_os("CRABCODE_CONFIG_DIR"),
        std::env::var_os("CRABCODE_HOME"),
        dirs::home_dir(),
    )
}

fn crabcode_config_root_from(
    config_dir: Option<OsString>,
    home_override: Option<OsString>,
    system_home: Option<PathBuf>,
) -> Option<PathBuf> {
    if let Some(path) = config_dir.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = home_override.filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path).join(".crabcode"));
    }
    system_home.map(|home| home.join(".crabcode"))
}

pub(crate) fn read_cached_png(path: &Path) -> bool {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return false;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    bytes.starts_with(b"\x89PNG\r\n\x1a\n") && image::load_from_memory(&bytes).is_ok()
}

#[cfg_attr(test, allow(dead_code))]
pub(crate) fn render_via_subprocess(
    executable: &Path,
    source: &str,
    theme_dark: bool,
    target_width_px: u32,
    quality: MermaidRenderQuality,
    out_path: &Path,
    timeout: Duration,
) -> Result<(), &'static str> {
    let mut command = Command::new(executable);
    command
        .arg(MERMAID_RENDER_SUBCOMMAND)
        .arg("--out")
        .arg(out_path)
        .arg("--theme")
        .arg(if theme_dark { "dark" } else { "light" })
        .arg("--quality")
        .arg(match quality {
            MermaidRenderQuality::Terminal => "terminal",
            MermaidRenderQuality::Open => "open",
        })
        .arg("--width")
        .arg(target_width_px.to_string())
        .arg("--deadline-ms")
        .arg(timeout.as_millis().to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(target_os = "linux")]
    command
        .env("_RJEM_MALLOC_CONF", "narenas:1")
        .env("MALLOC_CONF", "narenas:1");
    detach_command(&mut command);
    run_render_command(command, source.as_bytes(), out_path, timeout)
}

fn run_render_command(
    command: Command,
    source: &[u8],
    out_path: &Path,
    timeout: Duration,
) -> Result<(), &'static str> {
    map_run_result(run_with_timeout(command, Some(source), timeout), out_path)
}

#[cfg_attr(test, allow(dead_code))]
fn detach_command(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0200);
    }
}

#[cfg_attr(test, allow(dead_code))]
fn map_run_result(
    result: Result<(), SubprocessError>,
    out_path: &Path,
) -> Result<(), &'static str> {
    match result {
        Ok(()) if read_cached_png(out_path) => Ok(()),
        Ok(()) => Err("no_output"),
        Err(SubprocessError::Timeout) => Err("timeout"),
        Err(SubprocessError::NonZeroExit(status)) => match status.code() {
            None => Err("child_crashed"),
            Some(CHILD_WATCHDOG_EXIT_CODE) => Err("child_watchdog"),
            Some(_) => Err("child_error"),
        },
        Err(SubprocessError::Spawn(_)) => Err("spawn"),
        Err(SubprocessError::Wait(_)) => Err("wait"),
    }
}

pub(crate) fn maybe_run_render_subprocess(argv: impl IntoIterator<Item = OsString>) -> Option<i32> {
    let argv = argv.into_iter().collect::<Vec<_>>();
    if !is_render_subcommand(&argv) {
        return None;
    }
    Some(match render_child(argv.into_iter().skip(2)) {
        Ok(()) => 0,
        Err(_) => 1,
    })
}

fn is_render_subcommand(argv: &[OsString]) -> bool {
    argv.get(1).and_then(|argument| argument.to_str()) == Some(MERMAID_RENDER_SUBCOMMAND)
}

struct RenderArgs {
    out: PathBuf,
    theme_dark: bool,
    width: u32,
    quality: MermaidRenderQuality,
    deadline: Duration,
}

fn parse_render_args(mut args: impl Iterator<Item = OsString>) -> Result<RenderArgs, &'static str> {
    let mut out = None;
    let mut theme_dark = false;
    let mut width = 0_u32;
    let mut quality = MermaidRenderQuality::Terminal;
    let mut deadline = RENDER_TIMEOUT;
    while let Some(flag) = args.next() {
        let flag = flag.to_str().ok_or("non_utf8_flag")?;
        let value = args.next().ok_or("missing_value")?;
        match flag {
            "--out" => out = Some(PathBuf::from(value)),
            "--theme" => {
                theme_dark = match value.to_str() {
                    Some("dark") => true,
                    Some("light") => false,
                    _ => return Err("invalid_theme"),
                }
            }
            "--width" => width = parse_u32(value).ok_or("invalid_width")?,
            "--max-height" => {
                let _ = parse_u32(value).ok_or("invalid_height")?;
            }
            "--quality" => {
                quality = match value.to_str() {
                    Some("terminal" | "t") => MermaidRenderQuality::Terminal,
                    Some("open" | "o") => MermaidRenderQuality::Open,
                    _ => return Err("invalid_quality"),
                }
            }
            "--deadline-ms" => {
                deadline = Duration::from_millis(parse_u64(value).ok_or("invalid_deadline")?);
            }
            _ => return Err("unknown_flag"),
        }
    }
    Ok(RenderArgs {
        out: out.ok_or("missing_out")?,
        theme_dark,
        width,
        quality,
        deadline,
    })
}

fn parse_u32(value: OsString) -> Option<u32> {
    value.to_str()?.parse().ok()
}

fn parse_u64(value: OsString) -> Option<u64> {
    value.to_str()?.parse().ok()
}

fn render_child(args: impl Iterator<Item = OsString>) -> Result<(), &'static str> {
    let parsed = parse_render_args(args)?;
    install_child_backstops(parsed.deadline);
    let source = read_stdin_capped(RenderLimits::default().max_source_bytes)?;
    render_and_write(&parsed, &source)
}

fn render_and_write(parsed: &RenderArgs, source: &str) -> Result<(), &'static str> {
    let diagram = render_source_to_png(source, parsed.theme_dark, parsed.width, parsed.quality)
        .map_err(|_| "render")?;
    write_png_atomic(&parsed.out, &diagram.png).map_err(|_| "write")
}

fn read_stdin_capped(max: usize) -> Result<String, &'static str> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "stdin")?;
    String::from_utf8(bytes).map_err(|_| "utf8")
}

fn install_child_backstops(forwarded_budget: Duration) {
    #[cfg(target_os = "linux")]
    {
        cap_child_address_space();
        install_parent_death_signal();
    }
    let deadline = child_watchdog_deadline(forwarded_budget);
    let _ = std::thread::Builder::new()
        .name("crabcode-mermaid-watchdog".to_string())
        .spawn(move || {
            std::thread::sleep(deadline);
            std::process::exit(CHILD_WATCHDOG_EXIT_CODE);
        });
}

fn child_watchdog_deadline(forwarded_budget: Duration) -> Duration {
    forwarded_budget.saturating_add(CHILD_WATCHDOG_SLACK)
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn cap_child_address_space() {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: both libc calls only read or write the local `rlimit` value.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_AS, &mut limit) != 0 {
            return;
        }
        let cap = if limit.rlim_max == libc::RLIM_INFINITY {
            CHILD_ADDRESS_SPACE_CAP_BYTES
        } else {
            CHILD_ADDRESS_SPACE_CAP_BYTES.min(limit.rlim_max)
        };
        if limit.rlim_cur == libc::RLIM_INFINITY || limit.rlim_cur > cap {
            limit.rlim_cur = cap;
            let _ = libc::setrlimit(libc::RLIMIT_AS, &limit);
        }
    }
}

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn install_parent_death_signal() {
    // SAFETY: PR_SET_PDEATHSIG changes only this child process's kernel state.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong);
    }
}

fn render_params_for(
    theme_dark: bool,
    target_width_px: u32,
    quality: MermaidRenderQuality,
) -> RenderParams {
    let theme = if theme_dark {
        MermaidTheme::Dark
    } else {
        MermaidTheme::Light
    };
    match quality {
        MermaidRenderQuality::Open => {
            RenderParams::for_os_viewer(theme, OPEN_MIN_WIDTH_PX, OPEN_MAX_HEIGHT_PX)
        }
        MermaidRenderQuality::Terminal => RenderParams {
            theme,
            target_width_px,
            max_height_px: MAX_TARGET_HEIGHT_PX,
            scale: 1.0,
            min_width_px: 0,
            background: Some(theme.surface_background()),
        },
    }
}

fn render_source_to_png(
    source: &str,
    theme_dark: bool,
    target_width_px: u32,
    quality: MermaidRenderQuality,
) -> Result<RenderedDiagram, crabcode_mermaid::MermaidError> {
    render_checked(
        default_engine().as_ref(),
        source,
        &render_params_for(theme_dark, target_width_px, quality),
        &RenderLimits::default(),
    )
}

#[cfg(test)]
fn render_in_process_for_tests(job: &MermaidJob, _timeout: Duration) -> Result<(), &'static str> {
    let diagram = render_source_to_png(
        &job.source,
        job.theme_dark,
        job.target_width_px,
        job.quality,
    )
    .map_err(|_| "render")?;
    write_png_atomic(&job.out_path, &diagram.png).map_err(|_| "write")
}

fn render_job(render: &RenderFn, job: &MermaidJob, timeout: Duration) -> (MermaidOutcome, bool) {
    let started = Instant::now();
    if read_cached_png(&job.out_path) {
        return (
            MermaidOutcome::Ready {
                path: job.out_path.clone(),
            },
            false,
        );
    }
    if job.source.len() > RenderLimits::default().max_source_bytes {
        return (MermaidOutcome::Failed { reason: "oversize" }, false);
    }
    match render(job, timeout) {
        Ok(()) => {
            tracing::debug!(
                target: "crabcode_mermaid",
                elapsed_ms = started.elapsed().as_millis() as u64,
                "diagram render completed"
            );
            (
                MermaidOutcome::Ready {
                    path: job.out_path.clone(),
                },
                true,
            )
        }
        Err(reason) => {
            tracing::warn!(
                target: "crabcode_mermaid",
                elapsed_ms = started.elapsed().as_millis() as u64,
                reason,
                "diagram render failed"
            );
            (MermaidOutcome::Failed { reason }, false)
        }
    }
}

fn write_png_atomic(path: &Path, png: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut last_collision = None;
    for _ in 0..16 {
        let id = NEXT_ATOMIC_WRITE_ID.fetch_add(1, Ordering::Relaxed);
        let temporary = path.with_extension(format!("png-{}-{id}.png.tmp", std::process::id()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                last_collision = Some(error);
                continue;
            }
            Err(error) => return Err(error),
        };
        let result = (|| {
            use std::io::Write as _;
            file.write_all(png)?;
            file.sync_all()?;
            drop(file);
            atomic_replace_file(&temporary, path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        return result;
    }
    Err(last_collision.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate an exclusive Mermaid cache temp file",
        )
    }))
}

#[cfg(not(windows))]
fn atomic_replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(source, destination)
}

#[cfg(windows)]
#[allow(unsafe_code)] // MoveFileExW is the Windows atomic replace primitive.
fn atomic_replace_file(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: both vectors are NUL-terminated and live for the duration of the
    // call; MoveFileExW neither retains nor owns their pointers.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

pub(crate) fn sweep_session_cache(directory: &Path, max_bytes: u64) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut files = Vec::new();
    let mut total = 0_u64;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".png.tmp"))
        {
            let _ = std::fs::remove_file(path);
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("png") {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() || !read_cached_png(&path) {
            let _ = std::fs::remove_file(path);
            continue;
        }
        let length = metadata.len();
        total = total.saturating_add(length);
        files.push((
            metadata.modified().unwrap_or(std::time::UNIX_EPOCH),
            length,
            path,
        ));
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    for (_, length, path) in files {
        if total <= max_bytes {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(length);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(source: &str, out_path: PathBuf) -> MermaidJob {
        MermaidJob {
            key: MermaidCacheKey::derive(source, 0, 80, MermaidRenderQuality::Open),
            source: source.to_string(),
            out_path,
            theme_dark: true,
            target_width_px: target_width_px(80),
            quality: MermaidRenderQuality::Open,
        }
    }

    #[test]
    fn hidden_subcommand_is_exactly_position_one() {
        assert_eq!(
            maybe_run_render_subprocess(
                [OsString::from("crabcode-tui"), OsString::from("--help"),]
            ),
            None
        );
        assert_eq!(
            maybe_run_render_subprocess([
                OsString::from("crabcode-tui"),
                OsString::from("--help"),
                OsString::from(MERMAID_RENDER_SUBCOMMAND),
            ]),
            None
        );
    }

    #[test]
    fn malformed_child_arguments_fail_closed() {
        assert!(parse_render_args(Vec::<OsString>::new().into_iter()).is_err());
        assert!(
            parse_render_args([OsString::from("--out"), OsString::from("x")].into_iter()).is_ok()
        );
        assert!(
            parse_render_args(
                [
                    OsString::from("--out"),
                    OsString::from("x"),
                    OsString::from("--theme"),
                    OsString::from("unknown"),
                ]
                .into_iter()
            )
            .is_err()
        );
    }

    #[test]
    fn cache_key_is_theme_width_quality_and_source_sensitive() {
        let source = "flowchart LR\nA-->B";
        let key = MermaidCacheKey::derive(source, 0, 80, MermaidRenderQuality::Terminal);
        assert_eq!(
            key,
            MermaidCacheKey::derive(source, 0, 87, MermaidRenderQuality::Terminal)
        );
        assert_ne!(
            key,
            MermaidCacheKey::derive(source, 0, 88, MermaidRenderQuality::Terminal)
        );
        assert_ne!(
            key,
            MermaidCacheKey::derive(source, 1, 80, MermaidRenderQuality::Terminal)
        );
        assert_ne!(
            key,
            MermaidCacheKey::derive(source, 0, 80, MermaidRenderQuality::Open)
        );
        let open = MermaidCacheKey::derive(source, 0, 80, MermaidRenderQuality::Open);
        assert_eq!(
            open,
            MermaidCacheKey::derive(source, 0, 500, MermaidRenderQuality::Open)
        );
        assert!(open.cache_filename().ends_with("-0-65535-o-r3.png"));
    }

    #[test]
    fn cache_root_follows_the_documented_crabcode_environment_precedence() {
        assert_eq!(
            crabcode_config_root_from(
                Some(OsString::from("/tmp/crabcode-config")),
                Some(OsString::from("/tmp/crabcode-home")),
                Some(PathBuf::from("/tmp/system-home")),
            ),
            Some(PathBuf::from("/tmp/crabcode-config")),
        );
        assert_eq!(
            crabcode_config_root_from(
                None,
                Some(OsString::from("/tmp/crabcode-home")),
                Some(PathBuf::from("/tmp/system-home")),
            ),
            Some(PathBuf::from("/tmp/crabcode-home/.crabcode")),
        );
        assert_eq!(
            crabcode_config_root_from(None, None, Some(PathBuf::from("/tmp/system-home"))),
            Some(PathBuf::from("/tmp/system-home/.crabcode")),
        );
        assert!(default_session_cache_dir("../escape").is_none());
    }

    #[test]
    fn worker_renders_valid_png_and_cache_hit_reuses_it() {
        let directory = tempfile::tempdir().expect("tempdir");
        let out_path = directory.path().join("diagram.png");
        let (sender, receiver) = spawn_worker();
        sender
            .send(job("flowchart LR\nA-->B", out_path.clone()))
            .expect("send first render");
        let first = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("first render");
        assert!(matches!(first.outcome, MermaidOutcome::Ready { .. }));
        assert!(read_cached_png(&out_path));

        sender
            .send(job("flowchart LR\nA-->B", out_path.clone()))
            .expect("send cached render");
        let second = receiver
            .recv_timeout(Duration::from_secs(10))
            .expect("cached render");
        assert!(matches!(second.outcome, MermaidOutcome::Ready { .. }));
    }

    #[test]
    fn oversized_source_is_rejected_before_render_function() {
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let called_for_render = Arc::clone(&called);
        let render: Arc<RenderFn> = Arc::new(move |_, _| {
            called_for_render.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        });
        let directory = tempfile::tempdir().expect("tempdir");
        let mut oversized = job("x", directory.path().join("oversized.png"));
        oversized.source = "x".repeat(RenderLimits::default().max_source_bytes + 1);
        let (outcome, wrote) = render_job(render.as_ref(), &oversized, RENDER_TIMEOUT);
        assert!(matches!(
            outcome,
            MermaidOutcome::Failed { reason: "oversize" }
        ));
        assert!(!wrote);
        assert!(!called.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn corrupt_and_symlink_cache_entries_are_never_hits() {
        let directory = tempfile::tempdir().expect("tempdir");
        let corrupt = directory.path().join("corrupt.png");
        std::fs::write(&corrupt, b"\x89PNG\r\n\x1a\ntruncated").expect("write corrupt");
        assert!(!read_cached_png(&corrupt));
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = directory.path().join("target.png");
            let diagram = render_source_to_png(
                "flowchart LR\nA-->B",
                true,
                320,
                MermaidRenderQuality::Terminal,
            )
            .expect("render");
            std::fs::write(&target, diagram.png).expect("write target");
            let link = directory.path().join("link.png");
            symlink(&target, &link).expect("symlink");
            assert!(!read_cached_png(&link));
        }
    }

    #[test]
    fn cache_sweep_removes_temps_corrupt_entries_and_oldest_over_budget() {
        let directory = tempfile::tempdir().expect("tempdir");
        let temporary = directory.path().join("orphan.png.tmp");
        let corrupt = directory.path().join("corrupt.png");
        std::fs::write(&temporary, b"temp").expect("temp");
        std::fs::write(&corrupt, b"bad").expect("corrupt");
        sweep_session_cache(directory.path(), 0);
        assert!(!temporary.exists());
        assert!(!corrupt.exists());
    }

    #[test]
    fn drain_coalesced_keeps_latest_per_key() {
        let (sender, receiver) = std::sync::mpsc::channel::<MermaidJob>();
        sender
            .send(job("same", PathBuf::from("/tmp/old.png")))
            .expect("old same");
        sender
            .send(job("other", PathBuf::from("/tmp/other.png")))
            .expect("other");
        sender
            .send(job("same", PathBuf::from("/tmp/new.png")))
            .expect("new same");
        let first = receiver.recv().expect("first");
        let pending = drain_coalesced(first, &receiver);
        assert_eq!(pending.len(), 2);
        let same = MermaidCacheKey::derive("same", 0, 80, MermaidRenderQuality::Open);
        let other = MermaidCacheKey::derive("other", 0, 80, MermaidRenderQuality::Open);
        assert_eq!(pending[&same].out_path, PathBuf::from("/tmp/new.png"));
        assert_eq!(pending[&other].out_path, PathBuf::from("/tmp/other.png"));
    }

    #[test]
    fn take_pending_for_takes_all_matching_and_leaves_the_rest() {
        use crate::tui_links::MermaidAffordanceAction::{CopyPath, Open};

        let one = MermaidCacheKey::derive("one", 0, 80, MermaidRenderQuality::Open);
        let two = MermaidCacheKey::derive("two", 0, 80, MermaidRenderQuality::Open);
        let mut pending = vec![
            PendingMermaidAction {
                key: one.clone(),
                action: Open,
            },
            PendingMermaidAction {
                key: two.clone(),
                action: Open,
            },
            PendingMermaidAction {
                key: one.clone(),
                action: CopyPath,
            },
        ];
        assert_eq!(take_pending_for(&mut pending, &one), vec![Open, CopyPath]);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].key, two);
        assert!(take_pending_for(&mut pending, &one).is_empty());
    }

    #[test]
    fn runtime_has_pending_dedupes_by_key_and_action() {
        use crate::tui_links::MermaidAffordanceAction::{CopyPath, Open};

        let mut runtime = MermaidRuntime::new();
        let key = MermaidCacheKey::derive("d", 0, 80, MermaidRenderQuality::Open);
        assert!(!runtime.has_pending(&key, Open));
        runtime.pending.push(PendingMermaidAction {
            key: key.clone(),
            action: Open,
        });
        assert!(runtime.has_pending(&key, Open));
        assert!(!runtime.has_pending(&key, CopyPath));
        let other = MermaidCacheKey::derive("other", 0, 80, MermaidRenderQuality::Open);
        assert!(!runtime.has_pending(&other, Open));
    }

    #[test]
    fn target_width_px_is_clamped() {
        assert_eq!(target_width_px(1), MIN_TARGET_WIDTH_PX);
        assert_eq!(target_width_px(10_000), MAX_TARGET_WIDTH_PX);
        let middle = target_width_px(60);
        assert!(middle > MIN_TARGET_WIDTH_PX && middle < MAX_TARGET_WIDTH_PX);
    }

    #[test]
    fn representative_cols_subtracts_chrome_and_timestamp() {
        assert_eq!(representative_content_cols(100), 88);
        assert_eq!(representative_content_cols(0), 20);
        assert_eq!(representative_content_cols(5), 20);
    }

    #[test]
    fn sweep_enforces_cap_and_keeps_non_png() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = directory.path();
        for index in 0..3 {
            let path = root.join(format!("p{index}.png"));
            image::RgbaImage::from_pixel(2, 2, image::Rgba([index, 2, 3, 255]))
                .save(path)
                .expect("PNG");
        }
        let other = root.join("keep.txt");
        std::fs::write(&other, b"not cache").expect("text");
        sweep_session_cache(root, 0);
        assert!(other.exists());
        assert!(
            std::fs::read_dir(root)
                .expect("read cache")
                .flatten()
                .all(
                    |entry| entry.path().extension().and_then(|value| value.to_str())
                        != Some("png")
                )
        );
    }

    #[test]
    fn sweep_under_cap_keeps_valid_png() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("a.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save(&path)
            .expect("PNG");
        sweep_session_cache(directory.path(), 1024 * 1024);
        assert!(path.exists());
    }

    #[test]
    fn sweep_missing_dir_is_noop() {
        let directory = tempfile::tempdir().expect("tempdir");
        let missing = directory.path().join("missing");
        sweep_session_cache(&missing, 100);
        assert!(!missing.exists());
    }

    #[test]
    fn sweep_reclaims_orphaned_png_tmp() {
        let directory = tempfile::tempdir().expect("tempdir");
        let orphan = directory.path().join("d.png.tmp");
        std::fs::write(&orphan, b"orphan").expect("orphan");
        sweep_session_cache(directory.path(), 1024);
        assert!(!orphan.exists());
    }

    #[test]
    fn worker_autonomously_produces_ready() {
        let directory = tempfile::tempdir().expect("tempdir");
        let out = directory.path().join("autonomous.png");
        let (sender, receiver) = spawn_worker();
        sender
            .send(job("flowchart LR\nA-->B", out.clone()))
            .expect("send");
        let result = receiver
            .recv_timeout(Duration::from_secs(20))
            .expect("autonomous result");
        assert!(matches!(result.outcome, MermaidOutcome::Ready { .. }));
        assert!(read_cached_png(&out));
    }

    #[test]
    fn worker_oversized_source_fails_without_writing() {
        let directory = tempfile::tempdir().expect("tempdir");
        let out = directory.path().join("oversized-worker.png");
        let source = "x".repeat(RenderLimits::default().max_source_bytes + 1);
        let (sender, receiver) = spawn_worker();
        sender.send(job(&source, out.clone())).expect("send");
        let result = receiver
            .recv_timeout(Duration::from_secs(20))
            .expect("oversized result");
        assert!(matches!(
            result.outcome,
            MermaidOutcome::Failed { reason: "oversize" }
        ));
        assert!(!out.exists());
    }

    #[test]
    fn render_job_render_failure_is_failed() {
        let directory = tempfile::tempdir().expect("tempdir");
        let render = |_: &MermaidJob, _: Duration| Err("timeout");
        let (outcome, wrote) = render_job(
            &render,
            &job("flowchart LR\nA-->B", directory.path().join("fail.png")),
            RENDER_TIMEOUT,
        );
        assert!(matches!(
            outcome,
            MermaidOutcome::Failed { reason: "timeout" }
        ));
        assert!(!wrote);
    }

    #[test]
    fn render_job_success_reports_ready_and_wrote() {
        let directory = tempfile::tempdir().expect("tempdir");
        let out = directory.path().join("ok.png");
        let render = |job: &MermaidJob, _: Duration| {
            image::RgbaImage::from_pixel(3, 3, image::Rgba([5, 6, 7, 255]))
                .save(&job.out_path)
                .map_err(|_| "write")
        };
        let (outcome, wrote) = render_job(
            &render,
            &job("flowchart LR\nA-->B", out.clone()),
            RENDER_TIMEOUT,
        );
        assert!(matches!(outcome, MermaidOutcome::Ready { .. }));
        assert!(wrote);
        assert!(read_cached_png(&out));
    }

    #[test]
    fn render_job_disk_hit_skips_render() {
        let directory = tempfile::tempdir().expect("tempdir");
        let out = directory.path().join("hit.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save(&out)
            .expect("PNG");
        let render = |_: &MermaidJob, _: Duration| -> Result<(), &'static str> {
            panic!("disk hit must not render")
        };
        let (outcome, wrote) =
            render_job(&render, &job("flowchart LR\nA-->B", out), RENDER_TIMEOUT);
        assert!(matches!(outcome, MermaidOutcome::Ready { .. }));
        assert!(!wrote);
    }

    #[test]
    fn render_job_write_failure_is_fatal_without_a_png() {
        let directory = tempfile::tempdir().expect("tempdir");
        let blocker = directory.path().join("blocker");
        std::fs::write(&blocker, b"x").expect("blocker");
        let out = blocker.join("sub").join("d.png");
        let render = default_render_fn();
        let (outcome, wrote) = render_job(
            render.as_ref(),
            &job("flowchart LR\nA-->B", out.clone()),
            RENDER_TIMEOUT,
        );
        assert!(matches!(outcome, MermaidOutcome::Failed { .. }));
        assert!(!wrote);
        assert!(!out.exists());
    }

    #[test]
    fn render_source_to_png_handles_cyclic_login_flow() {
        let source = "flowchart TD\n\
            Start([User visits login page]) --> Enter[Enter username & password]\n\
            Enter --> Submit[Submit credentials]\n\
            Submit --> Validate{Credentials valid?}\n\
            Validate -->|No| Fail[Show error message]\n\
            Fail --> Attempts{Too many failed attempts?}\n\
            Attempts -->|Yes| Lock[Lock account]\n\
            Attempts -->|No| Enter\n\
            Validate -->|Yes| Session[Create session]";
        let diagram = render_source_to_png(source, false, 1024, MermaidRenderQuality::Terminal)
            .expect("cyclic flow renders");
        assert!(diagram.width_px > 0 && diagram.height_px > 0);
        image::load_from_memory(&diagram.png).expect("PNG");
    }

    fn os_args(arguments: &[&str]) -> std::vec::IntoIter<OsString> {
        arguments
            .iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parse_render_args_accepts_valid_and_rejects_malformed() {
        let parsed = parse_render_args(os_args(&[
            "--out",
            "/tmp/x.png",
            "--theme",
            "dark",
            "--quality",
            "open",
            "--width",
            "640",
            "--max-height",
            "900",
        ]))
        .expect("valid");
        assert_eq!(parsed.out, PathBuf::from("/tmp/x.png"));
        assert!(parsed.theme_dark);
        assert_eq!(parsed.width, 640);
        assert_eq!(parsed.quality, MermaidRenderQuality::Open);

        for malformed in [
            vec!["--theme", "light"],
            vec!["--out", "/tmp/x.png", "--bogus"],
            vec!["--out", "/tmp/x.png", "--width", "wide"],
            vec!["--out", "/tmp/x.png", "--theme", "purple"],
            vec!["--out", "/tmp/x.png", "--theme"],
            vec!["--out"],
        ] {
            assert!(
                parse_render_args(os_args(&malformed)).is_err(),
                "{malformed:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn parse_render_args_rejects_non_utf8_flag() {
        use std::os::unix::ffi::OsStringExt as _;
        let invalid = OsString::from_vec(vec![0xff, 0xfe]);
        assert!(parse_render_args(vec![invalid].into_iter()).is_err());
    }

    #[test]
    fn render_and_write_produces_decodable_png() {
        let directory = tempfile::tempdir().expect("tempdir");
        let out = directory.path().join("child.png");
        let parsed = RenderArgs {
            out: out.clone(),
            theme_dark: true,
            width: 640,
            quality: MermaidRenderQuality::Terminal,
            deadline: RENDER_TIMEOUT,
        };
        render_and_write(&parsed, "flowchart LR\nA-->B").expect("render");
        assert!(read_cached_png(&out));
    }

    #[test]
    fn render_and_write_empty_source_fails_without_png() {
        let directory = tempfile::tempdir().expect("tempdir");
        let out = directory.path().join("empty.png");
        let parsed = RenderArgs {
            out: out.clone(),
            theme_dark: false,
            width: 320,
            quality: MermaidRenderQuality::Terminal,
            deadline: RENDER_TIMEOUT,
        };
        assert!(render_and_write(&parsed, "").is_err());
        assert!(!out.exists());
    }

    #[test]
    fn is_render_subcommand_matches_only_argv1() {
        let argv = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(is_render_subcommand(&argv(&[
            "crabcode-tui",
            MERMAID_RENDER_SUBCOMMAND
        ])));
        assert!(!is_render_subcommand(&argv(&["crabcode-tui"])));
        assert!(!is_render_subcommand(&argv(&["crabcode-tui", "chat"])));
        assert!(!is_render_subcommand(&argv(&[
            "crabcode-tui",
            "chat",
            MERMAID_RENDER_SUBCOMMAND
        ])));
    }

    #[test]
    fn map_run_result_covers_every_outcome() {
        let directory = tempfile::tempdir().expect("tempdir");
        let good = directory.path().join("good.png");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save(&good)
            .expect("PNG");
        assert_eq!(map_run_result(Ok(()), &good), Ok(()));
        assert_eq!(
            map_run_result(Ok(()), &directory.path().join("missing.png")),
            Err("no_output")
        );
        assert_eq!(
            map_run_result(Err(SubprocessError::Timeout), &good),
            Err("timeout")
        );
        assert_eq!(
            map_run_result(
                Err(SubprocessError::Spawn(std::io::Error::other("x"))),
                &good
            ),
            Err("spawn")
        );
        assert_eq!(
            map_run_result(
                Err(SubprocessError::Wait(std::io::Error::other("x"))),
                &good
            ),
            Err("wait")
        );
    }

    #[cfg(unix)]
    #[test]
    fn map_run_result_distinguishes_crash_watchdog_from_render_error() {
        let out = Path::new("/nonexistent/out.png");
        let crashed = Command::new("sh")
            .args(["-c", "kill -ABRT $$"])
            .status()
            .expect("abort stub");
        assert_eq!(
            map_run_result(Err(SubprocessError::NonZeroExit(crashed)), out),
            Err("child_crashed")
        );
        let watchdog = Command::new("sh")
            .arg("-c")
            .arg(format!("exit {CHILD_WATCHDOG_EXIT_CODE}"))
            .status()
            .expect("watchdog stub");
        assert_eq!(
            map_run_result(Err(SubprocessError::NonZeroExit(watchdog)), out),
            Err("child_watchdog")
        );
        let failed = Command::new("sh")
            .args(["-c", "exit 1"])
            .status()
            .expect("failure stub");
        assert_eq!(
            map_run_result(Err(SubprocessError::NonZeroExit(failed)), out),
            Err("child_error")
        );
    }

    #[test]
    fn child_watchdog_deadline_trails_the_forwarded_budget() {
        assert_eq!(
            child_watchdog_deadline(Duration::from_secs(30)),
            Duration::from_secs(33)
        );
        for millis in [1_u64, 100, 1_500, 30_000] {
            let budget = Duration::from_millis(millis);
            assert!(child_watchdog_deadline(budget) > budget);
        }
    }

    #[test]
    fn parse_render_args_handles_deadline_ms() {
        let parsed = parse_render_args(os_args(&["--out", "/tmp/x.png", "--deadline-ms", "30000"]))
            .expect("deadline");
        assert_eq!(parsed.deadline, Duration::from_millis(30_000));
        assert_eq!(
            parse_render_args(os_args(&["--out", "/tmp/x.png"]))
                .expect("default")
                .deadline,
            RENDER_TIMEOUT
        );
        assert!(parse_render_args(os_args(&["--out", "/tmp/x.png", "--deadline-ms"])).is_err());
        assert!(
            parse_render_args(os_args(&["--out", "/tmp/x.png", "--deadline-ms", "soon"])).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_render_command_maps_stub_child_outcomes() {
        fn stub(script: &str) -> Command {
            let mut command = Command::new("sh");
            command
                .arg("-c")
                .arg(script)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            detach_command(&mut command);
            command
        }

        let directory = tempfile::tempdir().expect("tempdir");
        let source_png = directory.path().join("source.png");
        image::RgbaImage::from_pixel(3, 3, image::Rgba([9, 8, 7, 255]))
            .save(&source_png)
            .expect("PNG");
        let png = std::fs::read(source_png).expect("read");
        let output = directory.path().join("output.png");
        assert!(
            run_render_command(
                stub(&format!("cat > '{}'", output.display())),
                &png,
                &output,
                Duration::from_secs(10)
            )
            .is_ok()
        );
        assert_eq!(
            run_render_command(
                stub("cat >/dev/null"),
                b"x",
                &directory.path().join("none.png"),
                Duration::from_secs(10)
            ),
            Err("no_output")
        );
        assert_eq!(
            run_render_command(
                stub("cat >/dev/null; exit 7"),
                b"x",
                &directory.path().join("error.png"),
                Duration::from_secs(10)
            ),
            Err("child_error")
        );
        let started = Instant::now();
        assert_eq!(
            run_render_command(
                stub("sleep 30"),
                b"x",
                &directory.path().join("slow.png"),
                Duration::from_millis(100)
            ),
            Err("timeout")
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let mut missing = Command::new("definitely-not-a-real-binary-9f8a7b6c5d4e");
        missing.stdin(Stdio::piped());
        assert_eq!(
            run_render_command(
                missing,
                b"x",
                &directory.path().join("spawn.png"),
                Duration::from_secs(5)
            ),
            Err("spawn")
        );
    }

    #[test]
    fn parse_u32_arg_validates() {
        assert_eq!(parse_u32(OsString::from("800")), Some(800));
        assert_eq!(parse_u32(OsString::from("-1")), None);
        assert_eq!(parse_u32(OsString::from("x")), None);
    }

    #[test]
    fn per_theme_renders_land_in_distinct_files() {
        let directory = tempfile::tempdir().expect("tempdir");
        let source = "flowchart LR\nA-->B";
        let dark = MermaidCacheKey::derive(source, 0, 80, MermaidRenderQuality::Terminal);
        let light = MermaidCacheKey::derive(source, 1, 80, MermaidRenderQuality::Terminal);
        assert_ne!(dark.cache_filename(), light.cache_filename());
        let dark_out = directory.path().join(dark.cache_filename());
        let light_out = directory.path().join(light.cache_filename());
        let (sender, receiver) = spawn_worker();
        sender
            .send(MermaidJob {
                key: dark,
                source: source.to_string(),
                out_path: dark_out.clone(),
                theme_dark: true,
                target_width_px: 512,
                quality: MermaidRenderQuality::Terminal,
            })
            .expect("dark");
        sender
            .send(MermaidJob {
                key: light,
                source: source.to_string(),
                out_path: light_out.clone(),
                theme_dark: false,
                target_width_px: 512,
                quality: MermaidRenderQuality::Terminal,
            })
            .expect("light");
        for _ in 0..2 {
            assert!(matches!(
                receiver
                    .recv_timeout(Duration::from_secs(20))
                    .expect("render")
                    .outcome,
                MermaidOutcome::Ready { .. }
            ));
        }
        assert!(read_cached_png(&dark_out));
        assert!(read_cached_png(&light_out));
    }

    #[test]
    fn mermaid_view_disk_hit_runs_action_without_dispatch() {
        use crate::tui_links::MermaidAffordanceAction;

        let directory = tempfile::tempdir().expect("tempdir");
        with_test_cache_root(directory.path(), || {
            let source = "flowchart LR\nA-->B";
            let key = MermaidCacheKey::derive(source, 0, 80, MermaidRenderQuality::Open);
            let cache = default_session_cache_dir("session")
                .expect("cache")
                .join(key.cache_filename());
            std::fs::create_dir_all(cache.parent().expect("parent")).expect("cache dir");
            image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
                .save(&cache)
                .expect("PNG");
            let mut runtime = MermaidRuntime::new();
            assert!(matches!(
                runtime.request(
                    source.to_string(),
                    MermaidAffordanceAction::Open,
                    Some("session"),
                    0,
                    true,
                    80
                ),
                MermaidRequest::Immediate { .. }
            ));
            assert!(runtime.tx.is_none());
            assert!(runtime.rx.is_none());
        });
    }

    #[test]
    fn mermaid_view_pending_dedup_wins_over_disk_hit_race() {
        use crate::tui_links::MermaidAffordanceAction;

        let directory = tempfile::tempdir().expect("tempdir");
        with_test_cache_root(directory.path(), || {
            let source = "flowchart LR\nA-->B";
            let key = MermaidCacheKey::derive(source, 0, 80, MermaidRenderQuality::Open);
            let cache = default_session_cache_dir("session")
                .expect("cache")
                .join(key.cache_filename());
            std::fs::create_dir_all(cache.parent().expect("parent")).expect("cache dir");
            image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
                .save(&cache)
                .expect("PNG");

            let mut runtime = MermaidRuntime::new();
            runtime.pending.push(PendingMermaidAction {
                key,
                action: MermaidAffordanceAction::CopyPath,
            });
            assert!(matches!(
                runtime.request(
                    source.to_string(),
                    MermaidAffordanceAction::CopyPath,
                    Some("session"),
                    0,
                    true,
                    80
                ),
                MermaidRequest::AlreadyPending
            ));
            assert_eq!(runtime.pending.len(), 1);
            assert!(
                runtime.tx.is_none(),
                "the disk-hit race must not dispatch a duplicate worker job"
            );
        });
    }

    #[test]
    fn mermaid_is_rendering_matches_by_source_across_theme_width_change() {
        use crate::tui_links::MermaidAffordanceAction;

        let source = "flowchart LR\nA-->B";
        let click_key = MermaidCacheKey::derive(source, 0, 80, MermaidRenderQuality::Open);
        let live_key = MermaidCacheKey::derive(source, 1, 240, MermaidRenderQuality::Open);
        assert_ne!(click_key, live_key);
        assert_eq!(click_key.source_hash, live_key.source_hash);
        let mut runtime = MermaidRuntime::new();
        runtime.pending.push(PendingMermaidAction {
            key: click_key,
            action: MermaidAffordanceAction::Open,
        });
        assert!(runtime.is_rendering(source));
        assert!(!runtime.is_rendering("flowchart LR\nC-->D"));
    }

    #[test]
    fn atomic_write_replaces_a_corrupt_cache_entry() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("replace.png");
        std::fs::write(&path, b"corrupt").expect("corrupt");
        let diagram = render_source_to_png(
            "flowchart LR\nA-->B",
            true,
            320,
            MermaidRenderQuality::Terminal,
        )
        .expect("render");
        write_png_atomic(&path, &diagram.png).expect("replace");
        assert!(read_cached_png(&path));
        assert!(
            std::fs::read_dir(directory.path())
                .expect("read")
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".png.tmp")))
        );
    }
}

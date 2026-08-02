//! Fixed-upstream background writer for CrabCode terminal frames.
//!
//! Ratatui renders and diffs frames on the UI thread, but a terminal emulator
//! may temporarily stop consuming stdout. Writing the resulting byte stream on
//! a dedicated thread keeps backend events, permission requests, and terminal
//! input responsive during that interval.
//!
//! The fixed upstream uses an ordered, unbounded frame channel: once a frame
//! sequence is reserved, it is never dropped, replaced, or converted into a
//! renderer-visible timeout merely because the terminal is slow. Terminal
//! ownership drops the producer and joins this thread before ordinary
//! teardown. Panic and repeated-signal paths publish a one-way emergency
//! latch and only *try* to fence the output gate: they must never wait behind
//! a terminal write whose consumer may have stopped reading.

use std::cell::{Cell, RefCell};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError, Weak, mpsc};
use std::time::{Duration, Instant};

use crossterm::QueueableCommand as _;
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};

use crate::crabcode_image_overlay::CrabCodeImageEscapes;

thread_local! {
    /// A renderer-produced terminal image command for the synchronized frame
    /// currently being composed on this UI thread.
    static STAGED_IMAGE_POST_FLUSH: RefCell<Option<CrabCodeImageEscapes>> =
        const { RefCell::new(None) };
    /// True only while the current thread owns the process-wide terminal
    /// output gate. A panic on the writer thread must not try to re-lock that
    /// same non-reentrant mutex from the panic hook.
    static TERMINAL_WRITE_ACTIVE: Cell<bool> = const { Cell::new(false) };
    /// True while this thread owns the process-wide terminal output gate
    /// outside the writer loop (normal teardown, panic, or forced signal).
    ///
    /// Teardown can be entered from the panic hook while the writer thread is
    /// already inside the gate. Tracking both forms of ownership lets the
    /// restoration transaction reuse the same non-reentrant mutex without
    /// deadlocking.
    static TERMINAL_GATE_HELD: Cell<bool> = const { Cell::new(false) };
}

static TERMINAL_WRITE_GATE: OnceLock<Mutex<()>> = OnceLock::new();
static TERMINAL_WRITER_EMERGENCY_STOP: AtomicBool = AtomicBool::new(false);
static NEXT_TERMINAL_CONTROL_GENERATION: AtomicU64 = AtomicU64::new(1);

fn terminal_write_gate() -> &'static Mutex<()> {
    TERMINAL_WRITE_GATE.get_or_init(|| Mutex::new(()))
}

struct TerminalWriteMarker;

impl TerminalWriteMarker {
    fn enter() -> Self {
        TERMINAL_WRITE_ACTIVE.with(|active| active.set(true));
        Self
    }
}

impl Drop for TerminalWriteMarker {
    fn drop(&mut self) {
        TERMINAL_WRITE_ACTIVE.with(|active| active.set(false));
    }
}

/// Exclusive direct-output ownership for one terminal restoration
/// transaction.
///
/// The `None` branch is reachable when restoration is re-entered on a thread
/// that already owns the non-reentrant gate, including a writer-thread panic.
/// In that case the caller is already serialized and must not lock again.
pub(crate) struct TerminalOutputGuard {
    _gate: Option<MutexGuard<'static, ()>>,
    marked_here: bool,
}

impl Drop for TerminalOutputGuard {
    fn drop(&mut self) {
        if self.marked_here {
            // Release the mutex before publishing that this thread no longer
            // owns it. This preserves the invariant even if a later Drop
            // implementation on the same thread re-enters terminal cleanup.
            drop(self._gate.take());
            TERMINAL_GATE_HELD.with(|held| held.set(false));
        }
    }
}

pub(crate) fn lock_terminal_output_for_restore() -> TerminalOutputGuard {
    let already_owned = TERMINAL_WRITE_ACTIVE.with(Cell::get) || TERMINAL_GATE_HELD.with(Cell::get);
    if already_owned {
        return TerminalOutputGuard {
            _gate: None,
            marked_here: false,
        };
    }
    let gate = terminal_write_gate()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    TERMINAL_GATE_HELD.with(|held| held.set(true));
    TerminalOutputGuard {
        _gate: Some(gate),
        marked_here: true,
    }
}

fn lock_terminal_output_for_active_write_with(
    emergency_stopped: &AtomicBool,
) -> io::Result<TerminalOutputGuard> {
    let guard = lock_terminal_output_for_restore();
    if emergency_stopped.load(Ordering::Acquire) {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "terminal output is permanently stopped after emergency restoration",
        ));
    }
    Ok(guard)
}

/// Serialize a live renderer's direct terminal mutation with restoration.
///
/// The emergency latch is checked only after taking the shared gate. An
/// already-running write may finish before cleanup, but no new direct write
/// can start after panic/signal restoration has acquired and released it.
pub(crate) fn lock_terminal_output_for_active_write() -> io::Result<TerminalOutputGuard> {
    lock_terminal_output_for_active_write_with(&TERMINAL_WRITER_EMERGENCY_STOP)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EmergencyOutputFence {
    /// No other thread can currently be inside a gated terminal write.
    Quiesced,
    /// Another thread owns the gate and may be blocked in kernel output.
    Contended,
}

/// Non-blocking emergency observation of the terminal output owner.
///
/// A contended result deliberately owns no mutex guard. Its caller may perform
/// only kernel-state restoration and bounded best-effort output before
/// terminating the process; normal direct teardown is forbidden.
pub(crate) struct EmergencyTerminalWriterGuard {
    _output: Option<TerminalOutputGuard>,
    fence: EmergencyOutputFence,
}

impl EmergencyTerminalWriterGuard {
    pub(crate) const fn output_fence(&self) -> EmergencyOutputFence {
        self.fence
    }
}

fn try_emergency_gate(gate: &Mutex<()>) -> Option<MutexGuard<'_, ()>> {
    match gate.try_lock() {
        Ok(guard) => Some(guard),
        Err(TryLockError::Poisoned(error)) => Some(error.into_inner()),
        Err(TryLockError::WouldBlock) => None,
    }
}

pub(crate) fn emergency_stop_terminal_writer() -> EmergencyTerminalWriterGuard {
    // This latch participates in the crossed-publication handshake with the
    // TUI's raw/console acquisition state. SeqCst prevents both sides from
    // observing the other's store as absent and then mutating/restoring the
    // kernel terminal state in the wrong order.
    TERMINAL_WRITER_EMERGENCY_STOP.store(true, Ordering::SeqCst);
    let already_owned = TERMINAL_WRITE_ACTIVE.with(Cell::get) || TERMINAL_GATE_HELD.with(Cell::get);
    if already_owned {
        return EmergencyTerminalWriterGuard {
            _output: Some(TerminalOutputGuard {
                _gate: None,
                marked_here: false,
            }),
            fence: EmergencyOutputFence::Quiesced,
        };
    }
    // Emergency cleanup must never become the initializer of this OnceLock:
    // another thread may be inside `get_or_init`, and waiting for it would
    // defeat the fatal path's boundedness. Without a published gate there can
    // be no published writer owner, so the output is quiesced.
    let Some(gate) = TERMINAL_WRITE_GATE.get() else {
        return EmergencyTerminalWriterGuard {
            _output: None,
            fence: EmergencyOutputFence::Quiesced,
        };
    };
    match try_emergency_gate(gate) {
        Some(gate) => {
            TERMINAL_GATE_HELD.with(|held| held.set(true));
            EmergencyTerminalWriterGuard {
                _output: Some(TerminalOutputGuard {
                    _gate: Some(gate),
                    marked_here: true,
                }),
                fence: EmergencyOutputFence::Quiesced,
            }
        }
        None => EmergencyTerminalWriterGuard {
            _output: None,
            fence: EmergencyOutputFence::Contended,
        },
    }
}

pub(crate) fn terminal_writer_emergency_stopped() -> bool {
    TERMINAL_WRITER_EMERGENCY_STOP.load(Ordering::SeqCst)
}

pub(crate) fn stage_image_post_flush(escapes: Option<CrabCodeImageEscapes>) {
    STAGED_IMAGE_POST_FLUSH.with(|staged| *staged.borrow_mut() = escapes);
}

fn clear_staged_image_post_flush() {
    STAGED_IMAGE_POST_FLUSH.with(|staged| {
        staged.borrow_mut().take();
    });
}

fn take_staged_image_post_flush() -> Option<CrabCodeImageEscapes> {
    STAGED_IMAGE_POST_FLUSH.with(|staged| staged.borrow_mut().take())
}

#[derive(Debug)]
pub enum WriterEvent {
    Written(u64),
    Failed(std::io::Error),
}
/// Outcome of a bounded writer drain attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriterDrain {
    Drained,
    TimedOut,
}
/// Tracks submitted and successfully flushed presentation sequences.
///
/// During a child handoff, input is parked before this state is drained. Since
/// a sequence is reserved before its payload is sent, an accepted frame blocks
/// the drain before it is visible to the writer; no queued frame can land after
/// the child takes the tty.
#[derive(Clone, Debug)]
pub struct WriterSync {
    queued: Arc<AtomicU64>,
    written: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    writer_active: Arc<AtomicBool>,
    enqueue_gate: Arc<Mutex<()>>,
    event_tx: Option<tokio::sync::mpsc::UnboundedSender<WriterEvent>>,
    emergency_stopped: &'static AtomicBool,
}
impl Default for WriterSync {
    fn default() -> Self {
        Self::new()
    }
}
impl WriterSync {
    pub fn new() -> Self {
        Self {
            queued: Arc::new(AtomicU64::new(0)),
            written: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicBool::new(false)),
            writer_active: Arc::new(AtomicBool::new(false)),
            enqueue_gate: Arc::new(Mutex::new(())),
            event_tx: None,
            emergency_stopped: &TERMINAL_WRITER_EMERGENCY_STOP,
        }
    }
    fn with_event_sender(event_tx: tokio::sync::mpsc::UnboundedSender<WriterEvent>) -> Self {
        Self {
            queued: Arc::new(AtomicU64::new(0)),
            written: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicBool::new(false)),
            writer_active: Arc::new(AtomicBool::new(false)),
            enqueue_gate: Arc::new(Mutex::new(())),
            event_tx: Some(event_tx),
            emergency_stopped: &TERMINAL_WRITER_EMERGENCY_STOP,
        }
    }
    #[cfg(test)]
    fn new_with_emergency_latch(emergency_stopped: &'static AtomicBool) -> Self {
        Self {
            emergency_stopped,
            ..Self::new()
        }
    }
    #[cfg(test)]
    fn new_for_test() -> (Self, tokio::sync::mpsc::UnboundedReceiver<WriterEvent>) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        (Self::with_event_sender(event_tx), event_rx)
    }
    fn reserve_sequence(&self) -> u64 {
        self.queued.fetch_add(1, Ordering::Release) + 1
    }
    fn enqueue_payload(&self, tx: &WriterSender, data: Vec<u8>) -> io::Result<u64> {
        let _enqueue = self
            .enqueue_gate
            .lock()
            .map_err(|_| io::Error::other("terminal writer enqueue gate was poisoned"))?;
        self.enqueue_payload_locked(tx, data)
    }
    fn enqueue_payload_locked(&self, tx: &WriterSender, data: Vec<u8>) -> io::Result<u64> {
        if self.emergency_stopped() {
            let error = terminal_writer_emergency_error();
            self.mark_failed(io::Error::new(error.kind(), error.to_string()));
            return Err(error);
        }
        if self.failed() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal writer thread already failed",
            ));
        }
        let sequence = self.reserve_sequence();
        if tx.send(WriterPayload { sequence, data }).is_err() {
            let error = io::Error::new(io::ErrorKind::BrokenPipe, "terminal writer thread exited");
            self.mark_failed(io::Error::new(error.kind(), error.to_string()));
            return Err(error);
        }
        Ok(sequence)
    }
    fn emergency_stopped(&self) -> bool {
        self.emergency_stopped.load(Ordering::Acquire)
    }
    fn mark_written(&self, sequence: u64) {
        self.written.store(sequence, Ordering::Release);
        if let Some(event_tx) = &self.event_tx {
            let _ = event_tx.send(WriterEvent::Written(sequence));
        }
    }
    fn mark_failed(&self, error: std::io::Error) {
        if self
            .failed
            .compare_exchange(false, true, Ordering::Release, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        if let Some(event_tx) = &self.event_tx {
            let _ = event_tx.send(WriterEvent::Failed(error));
        }
    }
    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Acquire)
    }
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }
    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }
    fn is_drained(&self) -> bool {
        !self.failed() && self.written() >= self.queued()
    }
    /// Block until the writer flushes every accepted payload, output fails, or
    /// the deadline passes.
    pub fn wait_drained(&self, timeout: Duration) -> std::io::Result<WriterDrain> {
        let deadline = Instant::now() + timeout;
        while !self.is_drained() {
            if self.failed() {
                return Err(std::io::Error::other("terminal output failed"));
            }
            if Instant::now() >= deadline {
                return Ok(WriterDrain::TimedOut);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(WriterDrain::Drained)
    }
}
/// A writer that buffers frame output and sends it to a background thread
/// for non-blocking terminal I/O.
///
/// All escape sequences produced during a frame are collected in an internal
/// `Vec<u8>`. When [`flush()`](Write::flush) is called, the accumulated bytes
/// are sent through a channel to a dedicated writer thread that performs the
/// actual (potentially blocking) `write()` to stderr / the pty fd.
///
/// This decouples the tokio event loop from pty back-pressure: if the
/// terminal emulator is slow to read (e.g. Ghostty busy with another pane),
/// only the writer thread stalls — the event loop keeps processing timers,
/// events, and ACP messages.
pub struct WriterPayload {
    pub(crate) sequence: u64,
    pub(crate) data: Vec<u8>,
}
pub type WriterSender = mpsc::Sender<WriterPayload>;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterAlreadyActive;
impl std::fmt::Display for WriterAlreadyActive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriterSync already owns a live TermWriter")
    }
}
impl std::error::Error for WriterAlreadyActive {}
pub struct TermWriter {
    buf: Vec<u8>,
    tx: WriterSender,
    sync: WriterSync,
}
impl TermWriter {
    pub fn new(tx: WriterSender, sync: WriterSync) -> Result<Self, WriterAlreadyActive> {
        sync.writer_active
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| WriterAlreadyActive)?;
        Ok(Self {
            buf: Vec::with_capacity(32 * 1024),
            tx,
            sync,
        })
    }
    /// Drop the current frame's buffered bytes without sending them.
    pub fn discard(&mut self) {
        self.buf.clear();
    }
    /// Shared writer progress used by the suspend path to
    /// [`WriterSync::wait_drained`] before a child takes the tty.
    pub fn writer_sync(&self) -> &WriterSync {
        &self.sync
    }

    fn flush_and_close_synchronized_frame(
        &mut self,
        synchronized_frame_open: &AtomicBool,
    ) -> io::Result<()> {
        let _enqueue = self
            .sync
            .enqueue_gate
            .lock()
            .map_err(|_| io::Error::other("terminal writer enqueue gate was poisoned"))?;
        if self.buf.is_empty() {
            synchronized_frame_open.store(false, Ordering::Release);
            return Ok(());
        }
        let data = std::mem::take(&mut self.buf);
        self.sync.enqueue_payload_locked(&self.tx, data)?;
        // Publish frame closure before releasing the same gate acquired by
        // standalone control senders. A control racing the frame boundary is
        // therefore deterministically ordered after it, never spuriously
        // rejected in the send -> flag-clear gap.
        synchronized_frame_open.store(false, Ordering::Release);
        Ok(())
    }
}
impl Write for TermWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.buf);
        self.sync.enqueue_payload(&self.tx, data)?;
        Ok(())
    }
}
impl Drop for TermWriter {
    fn drop(&mut self) {
        let _ = self.flush();
        self.sync.writer_active.store(false, Ordering::Release);
    }
}
/// Handle for the background writer thread.
///
/// Joining ensures all queued frames have been written to the terminal
/// before proceeding with teardown (e.g. `LeaveAlternateScreen`).
pub struct WriterThread {
    handle: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    sync: WriterSync,
}
impl WriterThread {
    /// Block until the writer thread has processed all pending frames and
    /// exited. The [`mpsc::Sender`] must be dropped *before* calling this,
    /// otherwise the thread will never see the channel close.
    pub fn join(mut self) -> std::io::Result<()> {
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        match handle.join() {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::other("terminal writer thread panicked")),
        }
    }
    pub fn writer_sync(&self) -> &WriterSync {
        &self.sync
    }
}
impl Drop for WriterThread {
    fn drop(&mut self) {
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
fn write_payload(
    writer: &mut impl Write,
    payload: &WriterPayload,
    sync: &WriterSync,
) -> std::io::Result<()> {
    match writer
        .write_all(&payload.data)
        .and_then(|()| writer.flush())
    {
        Ok(()) => {
            sync.mark_written(payload.sequence);
            Ok(())
        }
        Err(error) => {
            sync.mark_failed(std::io::Error::new(error.kind(), error.to_string()));
            Err(error)
        }
    }
}

#[derive(Debug)]
struct TerminalControlChannel {
    generation: u64,
    sender: WriterSender,
    sync: WriterSync,
    synchronized_frame_open: AtomicBool,
}

fn terminal_writer_emergency_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        "terminal output stopped during emergency restoration",
    )
}

fn fail_terminal_writer_for_emergency(sync: &WriterSync) -> io::Error {
    let error = terminal_writer_emergency_error();
    sync.mark_failed(io::Error::new(error.kind(), error.to_string()));
    error
}

/// Weak, generation-bound route into the active ordered terminal writer.
///
/// The handle deliberately does not keep the writer channel alive. Dropping
/// the frame writer closes this generation even if a stale registry entry or
/// caller still holds a handle.
#[derive(Clone, Debug)]
pub(crate) struct TerminalControlSender {
    generation: u64,
    channel: Weak<TerminalControlChannel>,
}

impl TerminalControlSender {
    fn is_live(&self) -> bool {
        self.channel
            .upgrade()
            .is_some_and(|channel| !channel.sync.failed())
    }

    fn enqueue(&self, bytes: &[u8]) -> io::Result<()> {
        let channel = self.channel.upgrade().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "terminal control sender belongs to a retired writer generation",
            )
        })?;
        let _enqueue = channel
            .sync
            .enqueue_gate
            .lock()
            .map_err(|_| io::Error::other("terminal writer enqueue gate was poisoned"))?;
        if channel.synchronized_frame_open.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "terminal control bytes cannot split an open synchronized frame",
            ));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        channel
            .sync
            .enqueue_payload_locked(&channel.sender, bytes.to_vec())
            .map(|_| ())
    }
}

#[derive(Debug, Default)]
struct TerminalControlRegistry {
    active: Option<TerminalControlSender>,
}

impl TerminalControlRegistry {
    fn install(&mut self, sender: TerminalControlSender) -> io::Result<u64> {
        if self.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a terminal control writer generation is already registered",
            ));
        }
        if !sender.is_live() {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "cannot register a retired or failed terminal writer generation",
            ));
        }
        let generation = sender.generation;
        self.active = Some(sender);
        Ok(generation)
    }

    fn release(&mut self, generation: u64) -> io::Result<()> {
        let Some(active) = self.active.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no terminal control writer generation is registered",
            ));
        };
        if active.generation != generation {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "stale terminal control lease {generation} cannot release active generation {}",
                    active.generation
                ),
            ));
        }
        self.active = None;
        Ok(())
    }

    fn enqueue(&self, bytes: &[u8]) -> io::Result<()> {
        let sender = self.active.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "no terminal control writer generation is registered",
            )
        })?;
        sender.enqueue(bytes)
    }
}

fn terminal_control_registry() -> &'static Mutex<TerminalControlRegistry> {
    static REGISTRY: OnceLock<Mutex<TerminalControlRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(TerminalControlRegistry::default()))
}

/// Exact ownership token for one installed terminal-control generation.
///
/// It is intentionally non-cloneable. A stale lease can neither remove nor
/// enqueue through a later suspend/resume generation.
#[derive(Debug)]
pub(crate) struct TerminalControlLease {
    generation: u64,
    armed: bool,
}

impl TerminalControlLease {
    pub(crate) fn release(mut self) -> io::Result<()> {
        let result = terminal_control_registry()
            .lock()
            .map_err(|_| io::Error::other("terminal control registry was poisoned"))?
            .release(self.generation);
        self.armed = false;
        result
    }
}

impl Drop for TerminalControlLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut registry) = terminal_control_registry().lock() {
            let _ = registry.release(self.generation);
        }
        self.armed = false;
    }
}

pub(crate) fn install_terminal_control_sender(
    sender: TerminalControlSender,
) -> io::Result<TerminalControlLease> {
    let generation = terminal_control_registry()
        .lock()
        .map_err(|_| io::Error::other("terminal control registry was poisoned"))?
        .install(sender)?;
    Ok(TerminalControlLease {
        generation,
        armed: true,
    })
}

/// Enqueue standalone OSC/BEL bytes into the current frame writer generation.
///
/// Missing, stale, closed, and failed generations are errors. Callers must
/// never fall back to writing terminal bytes directly to stdout.
pub(crate) fn enqueue_registered_terminal_control(bytes: &[u8]) -> io::Result<()> {
    terminal_control_registry()
        .lock()
        .map_err(|_| io::Error::other("terminal control registry was poisoned"))?
        .enqueue(bytes)
}

fn next_terminal_control_generation() -> io::Result<u64> {
    NEXT_TERMINAL_CONTROL_GENERATION
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| io::Error::other("terminal control generation space was exhausted"))
}

/// `Write` implementation consumed by `CrosstermBackend`.
///
/// Crossterm can issue many small writes for one frame. They are accumulated
/// until its final `flush`, then enqueued as one ordered payload.
pub struct CrabCodeFrameWriter {
    buffer: Vec<u8>,
    synchronized_frame_content_start: Option<usize>,
    pending_image_commit: Option<CrabCodeImageEscapes>,
    ordered_writer: TermWriter,
    control_channel: Arc<TerminalControlChannel>,
}

impl CrabCodeFrameWriter {
    fn new(sender: WriterSender, sync: WriterSync) -> io::Result<Self> {
        let generation = next_terminal_control_generation()?;
        let control_channel = Arc::new(TerminalControlChannel {
            generation,
            sender: sender.clone(),
            sync: sync.clone(),
            synchronized_frame_open: AtomicBool::new(false),
        });
        let ordered_writer = TermWriter::new(sender, sync).map_err(|error| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("CrabCode terminal writer state is already active: {error}"),
            )
        })?;
        Ok(Self {
            buffer: Vec::with_capacity(32 * 1024),
            synchronized_frame_content_start: None,
            pending_image_commit: None,
            ordered_writer,
            control_channel,
        })
    }

    pub(crate) fn terminal_control_sender(&self) -> TerminalControlSender {
        TerminalControlSender {
            generation: self.control_channel.generation,
            channel: Arc::downgrade(&self.control_channel),
        }
    }

    /// Start one atomic terminal frame.
    ///
    /// Crossterm's backend calls `flush` from cursor and clear commands. While
    /// this transaction is open those incidental flushes remain buffered.
    /// The complete transaction remains one ordered payload, matching the
    /// fixed upstream writer contract.
    pub(crate) fn begin_synchronized_frame(&mut self) -> io::Result<()> {
        clear_staged_image_post_flush();
        self.pending_image_commit = None;
        if self.synchronized_frame_content_start.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CrabCode terminal frame transaction is already open",
            ));
        }

        // Serialize the closed -> open transition with every standalone
        // control enqueue. A control accepted before this guard is ordered
        // before the frame; once the flag is published, no control can enter
        // until the complete frame payload is accepted.
        let enqueue_gate = Arc::clone(&self.control_channel.sync.enqueue_gate);
        let _enqueue = enqueue_gate
            .lock()
            .map_err(|_| io::Error::other("terminal writer enqueue gate was poisoned"))?;
        self.control_channel
            .synchronized_frame_open
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "terminal control channel already marks a synchronized frame open",
                )
            })?;

        // Enclose any command buffered by a preceding Ratatui operation
        // instead of emitting it outside the synchronized update.
        let mut framed = Vec::with_capacity(self.buffer.len().saturating_add(16));
        if let Err(error) = framed.queue(BeginSynchronizedUpdate) {
            self.control_channel
                .synchronized_frame_open
                .store(false, Ordering::Release);
            return Err(error);
        }
        let content_start = framed.len();
        framed.append(&mut self.buffer);
        self.buffer = framed;
        self.synchronized_frame_content_start = Some(content_start);
        Ok(())
    }

    /// Whether the open frame contains anything besides its begin delimiter.
    pub(crate) fn synchronized_frame_has_content(&mut self) -> io::Result<bool> {
        self.append_staged_image_post_flush()?;
        let content_start = self.synchronized_frame_content_start.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "CrabCode terminal frame transaction is not open",
            )
        })?;
        Ok(self.buffer.len() > content_start)
    }

    /// Discard an incomplete or provably idle synchronized frame.
    pub(crate) fn abort_synchronized_frame(&mut self) {
        clear_staged_image_post_flush();
        self.pending_image_commit = None;
        self.buffer.clear();
        self.ordered_writer.discard();
        self.synchronized_frame_content_start = None;
        self.control_channel
            .synchronized_frame_open
            .store(false, Ordering::Release);
    }

    /// Close and enqueue one complete synchronized frame.
    pub(crate) fn finish_synchronized_frame(&mut self) -> io::Result<()> {
        if self.synchronized_frame_content_start.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "CrabCode terminal frame transaction is not open",
            ));
        }
        self.append_staged_image_post_flush()?;
        self.queue(EndSynchronizedUpdate)?;
        let payload = std::mem::take(&mut self.buffer);
        self.ordered_writer.write_all(&payload)?;
        self.ordered_writer
            .flush_and_close_synchronized_frame(&self.control_channel.synchronized_frame_open)?;
        self.synchronized_frame_content_start = None;
        if let Some(escapes) = self.pending_image_commit.take() {
            escapes.commit();
        }
        Ok(())
    }

    fn append_staged_image_post_flush(&mut self) -> io::Result<()> {
        if self.pending_image_commit.is_some() {
            return Ok(());
        }
        let Some(escapes) = take_staged_image_post_flush() else {
            return Ok(());
        };
        if escapes.has_bytes() {
            // `frame_transaction::draw_frame` calls this immediately after Ratatui's
            // differential flush and before applying the logical cursor. The
            // terminal therefore paints cells, then pixels, then restores the
            // cursor, all inside one synchronized update.
            self.write_all(escapes.as_bytes())?;
        }
        self.pending_image_commit = Some(escapes);
        Ok(())
    }

    fn enqueue_buffer(&mut self) -> io::Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let payload = std::mem::take(&mut self.buffer);
        self.ordered_writer.write_all(&payload)?;
        self.ordered_writer.flush()
    }

    /// Enqueue renderer-owned terminal control bytes in the same monotonic
    /// payload stream as Ratatui frames.
    ///
    /// Notifications, title changes, and similar OSC/BEL controls must never
    /// bypass the sole writer generation. A control payload is standalone:
    /// any preceding non-transactional bytes are committed first, while an
    /// open synchronized frame is rejected so the control cannot split it.
    pub(crate) fn enqueue_control_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.synchronized_frame_content_start.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal control bytes cannot split a synchronized frame",
            ));
        }
        self.enqueue_buffer()?;
        if bytes.is_empty() {
            return Ok(());
        }
        self.ordered_writer.write_all(bytes)?;
        self.ordered_writer.flush()
    }
}

impl Write for CrabCodeFrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.len().checked_add(bytes.len()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::OutOfMemory,
                "CrabCode terminal frame size overflow",
            )
        })?;
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.synchronized_frame_content_start.is_some() {
            return Ok(());
        }
        self.enqueue_buffer()
    }
}

impl Drop for CrabCodeFrameWriter {
    fn drop(&mut self) {
        if self.synchronized_frame_content_start.is_some() {
            // An unmatched BeginSynchronizedUpdate can leave a terminal
            // suppressing all later output. Never submit a partial frame.
            self.abort_synchronized_frame();
        } else {
            let _ = self.flush();
        }
    }
}

/// CrabCode-facing neutral alias for the fixed-upstream writer-thread owner.
pub type CrabCodeWriterThread = WriterThread;

/// One event stream and one monotonic sequence space survive terminal
/// suspend/resume generations. Reusing the fixed-upstream `WriterSync`
/// prevents an ACK from an old generation matching a new generation's target.
pub(crate) fn writer_event_channel() -> (
    WriterSync,
    tokio::sync::mpsc::UnboundedReceiver<WriterEvent>,
) {
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    (WriterSync::with_event_sender(event_tx), event_rx)
}

/// Start one fixed-upstream-style ordered stdout writer for a terminal
/// ownership interval.
pub fn spawn_terminal_writer(
    sync: WriterSync,
) -> io::Result<(CrabCodeFrameWriter, CrabCodeWriterThread)> {
    // Historical CrabCode interactive rendering owns stdout (including its
    // supported pipeline route), while the fixed upstream owns stderr. Keep
    // that proven product difference and preserve the upstream buffered
    // writer-thread lifecycle.
    spawn_terminal_writer_with(io::BufWriter::with_capacity(64 * 1024, io::stdout()), sync)
}

fn write_payload_under_gate(
    gate: &Mutex<()>,
    output: &mut impl Write,
    payload: &WriterPayload,
    sync: &WriterSync,
) -> io::Result<()> {
    let _gate = gate
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if sync.emergency_stopped() {
        return Err(fail_terminal_writer_for_emergency(sync));
    }
    // The deterministic PTY barrier must sit inside the same gate and
    // immediately before real output. That proves the repeated-signal path
    // never waits on an accepted frame whose terminal consumer is stalled.
    #[cfg(feature = "terminal-lifecycle-tests")]
    if let Err(error) = hold_test_only_accepted_frame(payload) {
        sync.mark_failed(io::Error::new(error.kind(), error.to_string()));
        return Err(error);
    }
    let _marker = TerminalWriteMarker::enter();
    let result = write_payload(output, payload, sync);
    if result.is_ok() && sync.emergency_stopped() {
        return Err(fail_terminal_writer_for_emergency(sync));
    }
    result
}

fn spawn_terminal_writer_with(
    mut output: impl Write + Send + 'static,
    sync: WriterSync,
) -> io::Result<(CrabCodeFrameWriter, CrabCodeWriterThread)> {
    // The emergency stop is a process-lifetime one-way latch. Its callers
    // either terminate the process or run from the panic hook; a later writer
    // must never re-arm output after forced restoration has started.
    if sync.emergency_stopped() {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "terminal output is permanently stopped after emergency restoration",
        ));
    }
    let (sender, receiver) = mpsc::channel::<WriterPayload>();
    let frame_writer = CrabCodeFrameWriter::new(sender, sync)?;
    let thread_sync = frame_writer.ordered_writer.writer_sync().clone();
    let writer_thread_sync = thread_sync.clone();
    let handle = std::thread::Builder::new()
        .name("crabcode-terminal-writer".to_string())
        .spawn(move || {
            while let Ok(payload) = receiver.recv() {
                if thread_sync.emergency_stopped() {
                    return Err(fail_terminal_writer_for_emergency(&thread_sync));
                }
                write_payload_under_gate(
                    terminal_write_gate(),
                    &mut output,
                    &payload,
                    &thread_sync,
                )?;
            }
            if thread_sync.failed() {
                Err(std::io::Error::other("terminal output failed"))
            } else {
                Ok(())
            }
        })?;
    Ok((
        frame_writer,
        WriterThread {
            handle: Some(handle),
            sync: writer_thread_sync,
        },
    ))
}

#[cfg(feature = "terminal-lifecycle-tests")]
fn hold_test_only_accepted_frame(payload: &WriterPayload) -> io::Result<()> {
    const ARM_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_WRITER_FRAME_ARM_FILE";
    const READY_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_WRITER_FRAME_READY_FILE";
    const RELEASE_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_WRITER_FRAME_RELEASE_FILE";
    const BEGIN_FRAME: &[u8] = b"\x1b[?2026h";
    const END_FRAME: &[u8] = b"\x1b[?2026l";

    let Some(arm_path) = std::env::var_os(ARM_FILE_ENV) else {
        return Ok(());
    };
    if !std::path::Path::new(&arm_path).is_file()
        || !payload
            .data
            .windows(BEGIN_FRAME.len())
            .any(|window| window == BEGIN_FRAME)
        || !payload
            .data
            .windows(END_FRAME.len())
            .any(|window| window == END_FRAME)
    {
        return Ok(());
    }
    let ready_path = std::env::var_os(READY_FILE_ENV).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{READY_FILE_ENV} is required when {ARM_FILE_ENV} is armed"),
        )
    })?;
    if std::path::Path::new(&ready_path).is_file() {
        return Ok(());
    }
    let release_path = std::env::var_os(RELEASE_FILE_ENV).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{RELEASE_FILE_ENV} is required when {ARM_FILE_ENV} is armed"),
        )
    })?;
    std::fs::write(&ready_path, b"accepted")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new(&release_path).is_file() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "test-only accepted writer frame was not released at {}",
                    std::path::Path::new(&release_path).display()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

#[cfg(test)]
pub(crate) struct InMemoryFrameReceiver(mpsc::Receiver<WriterPayload>);

#[cfg(test)]
impl InMemoryFrameReceiver {
    pub(crate) fn recv(&self) -> Result<Vec<u8>, mpsc::RecvError> {
        self.0.recv().map(|payload| payload.data)
    }

    pub(crate) fn try_recv(&self) -> Result<Vec<u8>, mpsc::TryRecvError> {
        self.0.try_recv().map(|payload| payload.data)
    }

    pub(crate) fn try_iter(&self) -> impl Iterator<Item = Vec<u8>> + '_ {
        self.0.try_iter().map(|payload| payload.data)
    }
}

#[cfg(test)]
pub(crate) fn in_memory_frame_writer() -> (CrabCodeFrameWriter, InMemoryFrameReceiver) {
    let (sender, receiver) = mpsc::channel();
    (
        CrabCodeFrameWriter::new(sender, WriterSync::new())
            .expect("in-memory terminal writer state is new"),
        InMemoryFrameReceiver(receiver),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};

    #[derive(Clone, Default)]
    struct SharedOutput(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct GatedFlushOutput {
        bytes: Arc<Mutex<Vec<u8>>>,
        flush_started: mpsc::SyncSender<()>,
        release_flush: mpsc::Receiver<()>,
    }

    impl Write for GatedFlushOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flush_started
                .send(())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "test gate closed"))?;
            self.release_flush
                .recv()
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "test gate closed"))
        }
    }

    struct FlushFailOutput;

    impl Write for FlushFailOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("injected terminal flush failure"))
        }
    }

    #[derive(Clone, Copy)]
    enum BlockingPoint {
        Write,
        Flush,
    }

    struct BlockingOutput {
        point: BlockingPoint,
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
        bytes: Arc<Mutex<Vec<u8>>>,
        blocked: bool,
    }

    impl BlockingOutput {
        fn block_once(&mut self) -> io::Result<()> {
            if self.blocked {
                return Ok(());
            }
            self.blocked = true;
            self.entered
                .send(())
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "test observer closed"))?;
            self.release
                .recv()
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "test release closed"))
        }
    }

    impl Write for BlockingOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if matches!(self.point, BlockingPoint::Write) {
                self.block_once()?;
            }
            self.bytes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if matches!(self.point, BlockingPoint::Flush) {
                self.block_once()?;
            }
            Ok(())
        }
    }

    #[test]
    fn buffers_one_frame_and_preserves_payload_order() {
        let output = SharedOutput::default();
        let readable = output.clone();
        let (mut writer, thread) = spawn_terminal_writer_with(output, WriterSync::new())
            .expect("test writer should start");

        writer.write_all(b"first").expect("buffer first fragment");
        writer.write_all(b"-frame").expect("buffer second fragment");
        assert!(
            readable
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "a partial frame must not reach the terminal before flush"
        );
        writer.flush().expect("enqueue first frame");
        writer.write_all(b"second").expect("buffer second frame");
        writer.flush().expect("enqueue second frame");
        drop(writer);
        thread.join().expect("writer should drain before join");

        assert_eq!(
            readable
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            b"first-framesecond"
        );
    }

    #[test]
    fn writer_success_is_acknowledged_after_flush() {
        let (sync, mut events) = WriterSync::new_for_test();
        let sequence = sync.reserve_sequence();
        let payload = WriterPayload {
            sequence,
            data: b"frame bytes".to_vec(),
        };
        let mut sink = Vec::new();
        write_payload(&mut sink, &payload, &sync).expect("write payload");
        assert_eq!(sink, b"frame bytes");
        assert_eq!(sync.written(), sequence);
        assert!(matches!(
            events.try_recv(),
            Ok(WriterEvent::Written(written)) if written == sequence
        ));
        assert_eq!(
            sync.wait_drained(Duration::from_secs(1)).unwrap(),
            WriterDrain::Drained
        );
    }

    #[test]
    fn writer_success_is_acknowledged_only_after_terminal_flush() {
        let (flush_started, flush_observed) = mpsc::sync_channel(1);
        let (release_flush, flush_release) = mpsc::sync_channel(1);
        let output = GatedFlushOutput {
            bytes: Arc::new(Mutex::new(Vec::new())),
            flush_started,
            release_flush: flush_release,
        };
        let sync = WriterSync::new();
        let (mut writer, thread) =
            spawn_terminal_writer_with(output, sync.clone()).expect("test writer should start");

        writer.write_all(b"frame").expect("buffer frame");
        writer.flush().expect("enqueue frame");
        flush_observed
            .recv_timeout(Duration::from_secs(1))
            .expect("writer reached terminal flush");

        assert_eq!(sync.queued(), 1);
        assert_eq!(
            sync.written(),
            0,
            "write_all without a successful terminal flush is not an acknowledgement"
        );
        assert_eq!(
            sync.wait_drained(Duration::from_millis(5))
                .expect("writer remains healthy"),
            WriterDrain::TimedOut
        );

        release_flush.send(()).expect("release terminal flush");
        assert_eq!(
            sync.wait_drained(Duration::from_secs(1))
                .expect("writer should drain"),
            WriterDrain::Drained
        );
        assert_eq!(sync.written(), 1);
        drop(writer);
        thread.join().expect("writer should exit cleanly");
    }

    #[test]
    fn writer_flush_failure_is_not_acknowledged() {
        let sync = WriterSync::new();
        let (mut writer, thread) = spawn_terminal_writer_with(FlushFailOutput, sync.clone())
            .expect("test writer should start");
        writer.write_all(b"frame").expect("buffer frame");
        writer.flush().expect("enqueue frame");
        drop(writer);

        let error = thread
            .join()
            .expect_err("terminal flush failure must fail the writer");
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(sync.queued(), 1);
        assert_eq!(sync.written(), 0);
        assert!(
            sync.wait_drained(Duration::ZERO).is_err(),
            "a failed payload must never become drained by being acknowledged"
        );
    }

    #[test]
    fn writer_drain_timeout_is_bounded_and_retryable() {
        let sync = WriterSync::new();
        let sequence = sync.reserve_sequence();
        assert_eq!(
            sync.wait_drained(Duration::ZERO)
                .expect("a healthy undrained writer returns a retryable state"),
            WriterDrain::TimedOut
        );
        sync.mark_written(sequence);
        assert_eq!(
            sync.wait_drained(Duration::ZERO)
                .expect("the same writer can be checked again after progress"),
            WriterDrain::Drained
        );
    }

    #[test]
    fn terminal_restore_gate_serializes_direct_teardown_transactions() {
        let source = include_str!("terminal_writer.rs");
        let spawn_start = source
            .find("fn spawn_terminal_writer_with(")
            .expect("writer spawn function");
        let spawn_end = source[spawn_start..]
            .find(
                "\n#[cfg(feature = \"terminal-lifecycle-tests\")]\nfn \
                 hold_test_only_accepted_frame(",
            )
            .map(|offset| spawn_start + offset)
            .expect("writer spawn boundary");
        let spawn = &source[spawn_start..spawn_end];
        assert!(
            spawn.contains("if sync.emergency_stopped()"),
            "writer spawn must fail closed after emergency restoration"
        );
        assert!(
            !spawn.contains("TERMINAL_WRITER_EMERGENCY_STOP.store(false"),
            "the process-lifetime emergency latch must never be re-armed by writer construction"
        );

        let first = lock_terminal_output_for_restore();
        // Re-entry on the owning thread must not deadlock. Panic restoration
        // uses this exact shape when the emergency guard already owns the
        // output gate.
        let nested = lock_terminal_output_for_restore();
        drop(nested);

        let (attempting_tx, attempting_rx) = mpsc::sync_channel(1);
        let (acquired_tx, acquired_rx) = mpsc::sync_channel(1);
        let contender = std::thread::spawn(move || {
            attempting_tx.send(()).expect("announce gate attempt");
            let _guard = lock_terminal_output_for_restore();
            acquired_tx.send(()).expect("announce gate acquisition");
        });
        attempting_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("contender reached output gate");
        assert!(
            acquired_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "a second teardown must not overtake the active restoration transaction"
        );
        drop(first);
        acquired_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("contender acquires after restoration releases the gate");
        contender.join().expect("join output-gate contender");
    }

    #[test]
    fn active_terminal_writes_fail_closed_after_emergency_restoration() {
        let emergency_stopped = Arc::new(AtomicBool::new(false));
        let active = lock_terminal_output_for_active_write_with(&emergency_stopped)
            .expect("active write starts before emergency restoration");

        let (marked_tx, marked_rx) = mpsc::sync_channel(1);
        let (restored_tx, restored_rx) = mpsc::sync_channel(1);
        let thread_stopped = Arc::clone(&emergency_stopped);
        let restorer = std::thread::spawn(move || {
            // Emergency publication precedes waiting for the output gate,
            // exactly as emergency_stop_terminal_writer does in production.
            thread_stopped.store(true, Ordering::Release);
            marked_tx.send(()).expect("publish emergency marker");
            let _restore_guard = lock_terminal_output_for_restore();
            restored_tx
                .send(())
                .expect("publish restoration completion");
        });
        marked_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("emergency stop is published");
        assert!(
            restored_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "restoration must wait for the in-flight active write"
        );
        drop(active);
        restored_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("restoration runs after the in-flight write releases");
        restorer.join().expect("join simulated emergency restorer");

        let rejected = lock_terminal_output_for_active_write_with(&emergency_stopped);
        assert!(
            rejected
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::BrokenPipe),
            "a later active write must fail closed after restoration"
        );
        // Restoration deliberately uses the unchecked re-entrant primitive
        // and therefore remains available after the latch is set.
        let restore = lock_terminal_output_for_restore();
        let nested_restore = lock_terminal_output_for_restore();
        drop(nested_restore);
        drop(restore);
    }

    #[test]
    fn emergency_gate_is_nonblocking_inside_real_write_and_flush_calls() {
        for point in [BlockingPoint::Write, BlockingPoint::Flush] {
            let gate = Arc::new(Mutex::new(()));
            let thread_gate = Arc::clone(&gate);
            let emergency_stopped = Box::leak(Box::new(AtomicBool::new(false)));
            let sync = WriterSync::new_with_emergency_latch(emergency_stopped);
            let sequence = sync.reserve_sequence();
            let payload = WriterPayload {
                sequence,
                data: b"blocked frame".to_vec(),
            };
            let (entered_tx, entered_rx) = mpsc::sync_channel(1);
            let (release_tx, release_rx) = mpsc::sync_channel(1);
            let bytes = Arc::new(Mutex::new(Vec::new()));
            let output = BlockingOutput {
                point,
                entered: entered_tx,
                release: release_rx,
                bytes,
                blocked: false,
            };
            let thread_sync = sync.clone();
            let writer = std::thread::spawn(move || {
                let mut output = output;
                write_payload_under_gate(&thread_gate, &mut output, &payload, &thread_sync)
            });
            entered_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("writer entered the injected blocking syscall seam");

            emergency_stopped.store(true, Ordering::Release);
            let started = Instant::now();
            assert!(
                try_emergency_gate(&gate).is_none(),
                "the exact production gate must report contention instead of waiting"
            );
            assert!(
                started.elapsed() < Duration::from_millis(50),
                "emergency gate observation must remain bounded inside write and flush"
            );

            release_tx.send(()).expect("release injected writer");
            let error = writer
                .join()
                .expect("join injected writer")
                .expect_err("a writer released after emergency publication must fail");
            assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
            assert!(sync.failed());
        }
    }

    #[test]
    fn emergency_stop_fails_every_accepted_unwritten_payload_loudly() {
        let emergency_stopped = Box::leak(Box::new(AtomicBool::new(false)));
        let sync = WriterSync::new_with_emergency_latch(emergency_stopped);
        let output = SharedOutput::default();
        let readable = output.clone();
        let gate = lock_terminal_output_for_restore();
        let (mut writer, thread) = spawn_terminal_writer_with(output, sync.clone())
            .expect("start writer with isolated emergency latch");

        writer
            .write_all(b"accepted-before-emergency")
            .expect("buffer emergency fixture");
        writer.flush().expect("accept emergency fixture");
        assert_eq!(sync.queued(), 1);
        emergency_stopped.store(true, Ordering::Release);
        drop(gate);
        drop(writer);

        let error = thread
            .join()
            .expect_err("emergency-stopped writer must fail loudly");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(sync.queued(), 1);
        assert_eq!(sync.written(), 0);
        assert!(sync.failed());
        assert!(
            sync.wait_drained(Duration::ZERO).is_err(),
            "an emergency-discarded accepted payload must never appear drained"
        );
        assert!(
            readable
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "an accepted payload must not escape after emergency restoration"
        );
    }

    #[test]
    fn writer_state_rejects_multiple_live_frame_producers() {
        let (sender, _receiver) = mpsc::channel::<WriterPayload>();
        let sync = WriterSync::new();
        let first = CrabCodeFrameWriter::new(sender.clone(), sync.clone())
            .expect("first producer owns the state");
        let second = match CrabCodeFrameWriter::new(sender.clone(), sync.clone()) {
            Ok(_) => panic!("a second live producer would break ordering"),
            Err(error) => error,
        };
        assert_eq!(second.kind(), io::ErrorKind::AlreadyExists);
        drop(first);
        let replacement = CrabCodeFrameWriter::new(sender, sync)
            .expect("ownership is reusable only after the first producer drops");
        drop(replacement);
    }

    #[test]
    fn writer_thread_preserves_multibyte_utf8() {
        let output = SharedOutput::default();
        let readable = output.clone();
        let expected = "CrabCode 中文 🦀 café".as_bytes().to_vec();
        let (mut writer, thread) = spawn_terminal_writer_with(output, WriterSync::new())
            .expect("test writer should start");
        writer
            .write_all(&expected)
            .expect("buffer multibyte UTF-8 payload");
        writer.flush().expect("enqueue UTF-8 payload");
        drop(writer);
        thread.join().expect("writer should drain before join");
        assert_eq!(
            readable
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            expected.as_slice()
        );
    }

    #[test]
    fn synchronized_transaction_submits_one_complete_payload() {
        let (mut writer, receiver) = in_memory_frame_writer();
        writer
            .begin_synchronized_frame()
            .expect("begin synchronized frame");
        writer.write_all(b"cell-diff").expect("buffer cell diff");
        writer
            .flush()
            .expect("intermediate backend flush remains buffered");
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        writer
            .finish_synchronized_frame()
            .expect("finish synchronized frame");

        let payload = receiver.recv().expect("one complete frame");
        let mut expected = Vec::new();
        expected
            .queue(BeginSynchronizedUpdate)
            .expect("begin bytes");
        expected.extend_from_slice(b"cell-diff");
        expected.queue(EndSynchronizedUpdate).expect("end bytes");
        assert_eq!(payload, expected);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn image_escape_is_appended_after_ratatui_flush_and_before_cursor_and_sync_end() {
        let (mut writer, receiver) = in_memory_frame_writer();
        writer
            .begin_synchronized_frame()
            .expect("begin synchronized frame");
        writer
            .write_all(b"ratatui-cell-diff")
            .expect("buffer Ratatui cell diff");
        writer
            .flush()
            .expect("Ratatui backend flush remains inside the frame");
        stage_image_post_flush(Some(CrabCodeImageEscapes::plain_for_test(
            "terminal-pixel-escape",
        )));
        assert!(
            writer
                .synchronized_frame_has_content()
                .expect("inspect post-flush frame")
        );
        writer
            .write_all(b"logical-cursor-action")
            .expect("buffer cursor restoration after pixels");
        writer
            .finish_synchronized_frame()
            .expect("finish synchronized frame");

        let payload = receiver.recv().expect("one complete frame");
        let cell_offset = payload
            .windows(b"ratatui-cell-diff".len())
            .position(|window| window == b"ratatui-cell-diff")
            .expect("cell diff marker");
        let pixel_offset = payload
            .windows(b"terminal-pixel-escape".len())
            .position(|window| window == b"terminal-pixel-escape")
            .expect("pixel marker");
        let cursor_offset = payload
            .windows(b"logical-cursor-action".len())
            .position(|window| window == b"logical-cursor-action")
            .expect("cursor marker");
        let mut end = Vec::new();
        end.queue(EndSynchronizedUpdate).expect("end bytes");
        let end_offset = payload
            .windows(end.len())
            .position(|window| window == end.as_slice())
            .expect("synchronized end marker");
        assert!(
            cell_offset < pixel_offset
                && pixel_offset < cursor_offset
                && cursor_offset < end_offset,
            "required protocol order is Ratatui flush -> pixels -> cursor -> synchronized end"
        );
    }

    #[test]
    fn aborted_transaction_never_emits_an_unmatched_begin_marker() {
        let (mut writer, receiver) = in_memory_frame_writer();
        writer
            .begin_synchronized_frame()
            .expect("begin synchronized frame");
        writer.write_all(b"partial").expect("buffer partial frame");
        writer.abort_synchronized_frame();
        drop(writer);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn dropping_an_open_unsubmitted_transaction_emits_nothing() {
        let (mut writer, receiver) = in_memory_frame_writer();
        writer
            .begin_synchronized_frame()
            .expect("begin synchronized frame");
        writer.write_all(b"partial").expect("buffer partial frame");
        drop(writer);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn standalone_control_bytes_preserve_frame_order_without_splitting_transactions() {
        let (mut writer, receiver) = in_memory_frame_writer();

        writer
            .begin_synchronized_frame()
            .expect("begin first synchronized frame");
        writer.write_all(b"frame-a").expect("write first frame");
        assert!(
            writer.enqueue_control_bytes(b"forbidden").is_err(),
            "control bytes must never split an open frame transaction"
        );
        writer
            .finish_synchronized_frame()
            .expect("finish first frame");
        writer
            .enqueue_control_bytes(b"\x07")
            .expect("enqueue standalone notification control");
        writer
            .begin_synchronized_frame()
            .expect("begin second synchronized frame");
        writer.write_all(b"frame-b").expect("write second frame");
        writer
            .finish_synchronized_frame()
            .expect("finish second frame");

        let first = receiver.recv().expect("first frame payload");
        let control = receiver.recv().expect("control payload");
        let second = receiver.recv().expect("second frame payload");
        assert!(first.windows(7).any(|window| window == b"frame-a"));
        assert_eq!(control, b"\x07");
        assert!(second.windows(7).any(|window| window == b"frame-b"));
    }

    #[test]
    fn generation_bound_controls_share_frame_order_and_cannot_split_a_transaction() {
        let (mut writer, receiver) = in_memory_frame_writer();
        let sender = writer.terminal_control_sender();
        let mut registry = TerminalControlRegistry::default();
        let generation = registry
            .install(sender)
            .expect("install the in-memory writer generation");

        writer
            .begin_synchronized_frame()
            .expect("begin first synchronized frame");
        writer.write_all(b"frame-a").expect("write first frame");
        let error = registry
            .enqueue(b"control-inside-frame")
            .expect_err("a standalone control must not split an open frame");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        writer
            .finish_synchronized_frame()
            .expect("finish first frame");
        registry
            .enqueue(b"ordered-control")
            .expect("enqueue control after the frame");
        writer
            .begin_synchronized_frame()
            .expect("begin second synchronized frame");
        writer.write_all(b"frame-b").expect("write second frame");
        writer
            .finish_synchronized_frame()
            .expect("finish second frame");

        let first = receiver.recv().expect("first frame payload");
        let control = receiver.recv().expect("control payload");
        let second = receiver.recv().expect("second frame payload");
        assert!(first.windows(7).any(|window| window == b"frame-a"));
        assert_eq!(control, b"ordered-control");
        assert!(second.windows(7).any(|window| window == b"frame-b"));
        registry
            .release(generation)
            .expect("release exact writer generation");
    }

    #[test]
    fn registry_rejects_replacement_and_stale_generation_release() {
        let (first, first_receiver) = in_memory_frame_writer();
        let (second, second_receiver) = in_memory_frame_writer();
        let first_sender = first.terminal_control_sender();
        let second_sender = second.terminal_control_sender();
        let mut registry = TerminalControlRegistry::default();

        let first_generation = registry
            .install(first_sender)
            .expect("install first generation");
        let replacement_error = registry
            .install(second_sender.clone())
            .expect_err("a second live generation must fail closed");
        assert_eq!(replacement_error.kind(), io::ErrorKind::AlreadyExists);
        registry
            .release(first_generation)
            .expect("release first generation");
        let second_generation = registry
            .install(second_sender)
            .expect("install second generation after release");
        assert_ne!(first_generation, second_generation);
        let stale_error = registry
            .release(first_generation)
            .expect_err("a stale lease cannot release the active generation");
        assert_eq!(stale_error.kind(), io::ErrorKind::PermissionDenied);
        registry
            .enqueue(b"second-generation")
            .expect("active second generation remains installed");
        assert_eq!(
            second_receiver.recv().expect("second writer payload"),
            b"second-generation"
        );
        assert!(matches!(
            first_receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        registry
            .release(second_generation)
            .expect("release second generation");
        drop(first);
        drop(second);
    }

    #[test]
    fn weak_control_handle_does_not_keep_a_retired_writer_channel_alive() {
        let (writer, receiver) = in_memory_frame_writer();
        let sender = writer.terminal_control_sender();
        drop(writer);

        let error = sender
            .enqueue(b"retired")
            .expect_err("a retired generation must reject terminal bytes");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
        let mut registry = TerminalControlRegistry::default();
        let error = registry
            .install(sender)
            .expect_err("a retired weak handle cannot be installed");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn control_route_closes_after_background_writer_failure() {
        let sync = WriterSync::new();
        let (writer, thread) = spawn_terminal_writer_with(FlushFailOutput, sync.clone())
            .expect("start failing test writer");
        let sender = writer.terminal_control_sender();
        let mut registry = TerminalControlRegistry::default();
        let generation = registry
            .install(sender)
            .expect("install live writer generation");
        registry
            .enqueue(b"payload-that-fails-flush")
            .expect("the live writer accepts the first payload");

        let deadline = Instant::now() + Duration::from_secs(1);
        while !sync.failed() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(sync.failed(), "the injected flush failure must be observed");
        let error = registry
            .enqueue(b"must-not-follow-failure")
            .expect_err("a failed writer generation must reject later controls");
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        registry
            .release(generation)
            .expect("release failed generation by exact token");
        drop(writer);
        assert!(
            thread.join().is_err(),
            "the background writer exposes its terminal flush failure"
        );
    }

    #[test]
    fn parallel_local_registries_never_cross_route_controls() {
        std::thread::scope(|scope| {
            let left = scope.spawn(|| {
                let (writer, receiver) = in_memory_frame_writer();
                let mut registry = TerminalControlRegistry::default();
                let generation = registry
                    .install(writer.terminal_control_sender())
                    .expect("install left generation");
                registry.enqueue(b"left").expect("enqueue left control");
                assert_eq!(receiver.recv().expect("left payload"), b"left");
                registry.release(generation).expect("release left");
            });
            let right = scope.spawn(|| {
                let (writer, receiver) = in_memory_frame_writer();
                let mut registry = TerminalControlRegistry::default();
                let generation = registry
                    .install(writer.terminal_control_sender())
                    .expect("install right generation");
                registry.enqueue(b"right").expect("enqueue right control");
                assert_eq!(receiver.recv().expect("right payload"), b"right");
                registry.release(generation).expect("release right");
            });
            left.join().expect("left registry thread");
            right.join().expect("right registry thread");
        });
    }
}

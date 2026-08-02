//! Terminal input isolation for the native CrabCode TUI.
//!
//! The reader follows the fixed Rust renderer lifecycle: each event is stamped
//! at the reader, ordinary polling is bounded, transient parser errors are
//! skipped up to a consecutive-error threshold, and a terminal handoff can
//! park the sole crossterm owner before another process inherits the TTY.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event};
use tokio::sync::mpsc;

const READER_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PARK_WAIT_INTERVAL: Duration = Duration::from_millis(5);
const MAX_CONSECUTIVE_EVENT_ERRORS: u32 = 50;

/// A terminal event paired with the instant at which the reader accepted it.
///
/// Capturing this before queueing keeps scroll acceleration, reversal, and
/// paste grouping independent of UI draw or backend scheduling delays.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TimedInputEvent {
    pub(crate) event: Event,
    pub(crate) arrived_at: Instant,
}

impl TimedInputEvent {
    fn now(event: Event) -> Self {
        Self {
            event,
            arrived_at: Instant::now(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct InputState {
    failure: Arc<Mutex<Option<(io::ErrorKind, String)>>>,
}

impl InputState {
    fn mark_failed(&self, error: &io::Error) {
        let mut stored = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if stored.is_none() {
            *stored = Some((error.kind(), error.to_string()));
        }
    }

    fn current_failure(&self) -> Option<io::Error> {
        let stored = self
            .failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stored
            .as_ref()
            .map(|(kind, message)| io::Error::new(*kind, message.clone()))
    }
}

#[derive(Clone, Debug, Default)]
struct ReaderParking {
    pause_requested: Arc<AtomicBool>,
    parked: Arc<AtomicBool>,
}

impl ReaderParking {
    /// Request a park and wait for a fresh reader acknowledgement.
    ///
    /// The pause remains asserted on both success and timeout. This mirrors
    /// the fixed lifecycle contract: only the handoff owner may unpark after
    /// either completing the child handoff or abandoning a timed-out attempt.
    fn park(&self, timeout: Duration) -> bool {
        // Clear the acknowledgement before publishing the new request. A
        // previously parked generation must never satisfy this handoff.
        self.parked.store(false, Ordering::Release);
        self.pause_requested.store(true, Ordering::Release);
        #[cfg(feature = "terminal-lifecycle-tests")]
        publish_test_only_park_requested();

        let started_at = Instant::now();
        while !self.parked.load(Ordering::Acquire) && started_at.elapsed() < timeout {
            let remaining = timeout.saturating_sub(started_at.elapsed());
            std::thread::sleep(PARK_WAIT_INTERVAL.min(remaining));
        }
        self.parked.load(Ordering::Acquire)
    }

    fn unpark(&self) {
        self.pause_requested.store(false, Ordering::Release);
    }

    fn pause_requested(&self) -> bool {
        self.pause_requested.load(Ordering::Acquire)
    }

    fn acknowledge_parked(&self) {
        self.parked.store(true, Ordering::Release);
    }

    fn mark_active(&self) {
        self.parked.store(false, Ordering::Release);
    }
}

#[derive(Debug, Default)]
struct ConsecutiveEventErrors {
    count: u32,
}

impl ConsecutiveEventErrors {
    fn record_success(&mut self) {
        self.count = 0;
    }

    fn record_error(&mut self) -> bool {
        self.count = self.count.saturating_add(1);
        self.count >= MAX_CONSECUTIVE_EVENT_ERRORS
    }
}

fn record_reader_error(
    errors: &mut ConsecutiveEventErrors,
    state: &InputState,
    error: &io::Error,
) -> bool {
    let fatal = errors.record_error();
    if fatal {
        state.mark_failed(error);
    }
    fatal
}

/// The sole crossterm event source for one terminal-ownership generation.
///
/// The producer never competes with a cancellable async crossterm future. It
/// owns crossterm on a dedicated thread and forwards timestamped events over an
/// unbounded channel, matching the fixed renderer lifecycle. Shutdown joins
/// that reader before terminal ownership can change.
pub struct TerminalEventSource {
    receiver: Option<mpsc::UnboundedReceiver<TimedInputEvent>>,
    state: InputState,
    shutdown: Arc<AtomicBool>,
    parking: ReaderParking,
    handle: Option<JoinHandle<()>>,
}

impl TerminalEventSource {
    pub fn start() -> io::Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let shutdown = Arc::new(AtomicBool::new(false));
        let thread_shutdown = shutdown.clone();
        let state = InputState::default();
        let thread_state = state.clone();
        let parking = ReaderParking::default();
        let thread_parking = parking.clone();
        let handle = std::thread::Builder::new()
            .name("crabcode-terminal-input".to_string())
            .spawn(move || {
                let mut consecutive_errors = ConsecutiveEventErrors::default();
                while !thread_shutdown.load(Ordering::Acquire) {
                    if thread_parking.pause_requested() {
                        // This acknowledgement is published only after the
                        // reader has stopped calling crossterm.
                        thread_parking.acknowledge_parked();
                        std::thread::sleep(READER_POLL_INTERVAL);
                        continue;
                    }
                    thread_parking.mark_active();

                    let terminal_event = match event::poll(READER_POLL_INTERVAL) {
                        Ok(false) => continue,
                        Ok(true) => event::read(),
                        Err(error) => Err(error),
                    };
                    match terminal_event {
                        Ok(terminal_event) => {
                            #[cfg(feature = "terminal-lifecycle-tests")]
                            if let Err(error) = append_test_only_input_event(&terminal_event)
                                .and_then(|()| {
                                    hold_test_only_reader_with_buffered_crossterm_state()
                                })
                            {
                                thread_state.mark_failed(&error);
                                tracing::error!(
                                    error = %error,
                                    "test-only crossterm generation barrier failed"
                                );
                                break;
                            }
                            consecutive_errors.record_success();
                            let timed_event = TimedInputEvent::now(terminal_event);
                            if sender.send(timed_event).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            if record_reader_error(
                                &mut consecutive_errors,
                                &thread_state,
                                &error,
                            ) {
                                tracing::error!(
                                    consecutive_event_errors = consecutive_errors.count,
                                    error = %error,
                                    "crossterm input reader exceeded its consecutive-error threshold"
                                );
                                break;
                            }
                            tracing::warn!(
                                consecutive_event_errors = consecutive_errors.count,
                                error = %error,
                                "skipping transient crossterm input error"
                            );
                        }
                    }
                }
            })?;
        Ok(Self {
            receiver: Some(receiver),
            state,
            shutdown,
            parking,
            handle: Some(handle),
        })
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.receiver
            .as_ref()
            .is_some_and(|receiver| !receiver.is_empty())
    }

    pub(crate) fn receiver_mut(
        &mut self,
    ) -> io::Result<&mut mpsc::UnboundedReceiver<TimedInputEvent>> {
        self.receiver.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "CrabCode terminal input receiver is closed",
            )
        })
    }

    pub(crate) fn failure(&self) -> Option<io::Error> {
        self.state.current_failure()
    }

    /// Pause the reader before handing the TTY to another process.
    ///
    /// A `false` result is a timeout, not an implicit cancellation: the pause
    /// remains asserted and the caller must invoke [`Self::unpark_reader`].
    pub(crate) fn park_reader(&self, timeout: Duration) -> bool {
        self.parking.park(timeout)
    }

    /// Return crossterm ownership to the reader after a handoff or timeout.
    pub(crate) fn unpark_reader(&self) {
        self.parking.unpark();
    }

    /// Discard events accepted before a terminal ownership generation changed.
    ///
    /// The reader must be parked first. Only the acknowledged pre-park race
    /// can be present in this channel.
    pub(crate) fn discard_pending(&mut self) -> io::Result<usize> {
        let receiver = self.receiver_mut()?;
        let mut discarded = 0_usize;
        while receiver.try_recv().is_ok() {
            discarded = discarded.saturating_add(1);
        }
        Ok(discarded)
    }

    /// Retire crossterm's process-global complete-event queues and partial
    /// decoder state while this source's sole reader is parked.
    ///
    /// This does not flush the operating-system tty queue. The caller owns
    /// that separate linearization point immediately before unpark.
    pub(crate) fn retire_crossterm_generation(&self) {
        crossterm::event::reset_reader_generation();
    }

    /// Discard bytes still buffered by the Unix tty while the reader is
    /// parked.
    ///
    /// A process-wide SIGSTOP can leave input in the kernel rather than this
    /// source's channel. Clearing only the channel would let those bytes enter
    /// the resumed renderer generation after the reader is unparked.
    #[cfg(unix)]
    pub(crate) fn discard_pending_tty_input(&self) -> io::Result<()> {
        nix::sys::termios::tcflush(std::io::stdin(), nix::sys::termios::FlushArg::TCIFLUSH)
            .map_err(io::Error::other)
    }

    /// Non-Unix consoles have no Unix tty input queue to flush. Native
    /// process suspension is unsupported there, but the shared resume branch
    /// still needs an explicit cross-platform implementation.
    #[cfg(not(unix))]
    pub(crate) const fn discard_pending_tty_input(&self) -> io::Result<()> {
        Ok(())
    }

    /// Stop and join the sole terminal reader.
    ///
    /// This is called explicitly before every terminal suspend/restart so the
    /// following owner cannot race a detached reader. `Drop` invokes the same
    /// path as a last-resort cleanup.
    pub fn stop(mut self) -> io::Result<()> {
        self.stop_inner()
    }

    fn stop_inner(&mut self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::Release);
        // A parked reader sleeps without touching crossterm. Clearing the
        // pause ensures shutdown is observed on the next bounded iteration.
        self.parking.unpark();
        self.receiver.take();
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        match handle.join() {
            Ok(()) => Ok(()),
            Err(_) => Err(io::Error::other(
                "CrabCode terminal input reader thread panicked",
            )),
        }
    }
}

#[cfg(feature = "terminal-lifecycle-tests")]
fn publish_test_only_park_requested() {
    const READY_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_INPUT_PARK_REQUESTED_FILE";
    let Some(path) = std::env::var_os(READY_FILE_ENV) else {
        return;
    };
    std::fs::write(&path, b"requested").unwrap_or_else(|error| {
        panic!(
            "failed to publish test-only terminal-input park request at {}: {error}",
            std::path::Path::new(&path).display()
        )
    });
}

#[cfg(feature = "terminal-lifecycle-tests")]
fn append_test_only_input_event(event: &Event) -> io::Result<()> {
    use std::io::Write as _;

    const EVENT_LOG_ENV: &str = "CRABCODE_TUI_TEST_ONLY_INPUT_EVENT_LOG_FILE";
    let Some(path) = std::env::var_os(EVENT_LOG_ENV) else {
        return Ok(());
    };
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{event:?}")
}

#[cfg(feature = "terminal-lifecycle-tests")]
fn hold_test_only_reader_with_buffered_crossterm_state() -> io::Result<()> {
    const ARM_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_INPUT_BUFFER_ARM_FILE";
    const READY_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_INPUT_BUFFER_READY_FILE";
    const RELEASE_FILE_ENV: &str = "CRABCODE_TUI_TEST_ONLY_INPUT_BUFFER_RELEASE_FILE";

    let Some(arm_path) = std::env::var_os(ARM_FILE_ENV) else {
        return Ok(());
    };
    if !std::path::Path::new(&arm_path).is_file() {
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

    // `read()` returned one complete event. A zero-timeout poll gives
    // crossterm's global reader/source one opportunity to accept additional
    // currently-ready tty bytes without consuming a second complete event.
    // `true` leaves an event in InternalEventReader; `false` can leave an
    // incomplete CSI/UTF-8/paste prefix in the Unix parser. Either accepted
    // state predates the barrier below; the final tty flush separately retires
    // bytes that remain in the kernel.
    let _has_complete_event = event::poll(Duration::ZERO)?;
    std::fs::write(&ready_path, b"buffered")?;

    let deadline = Instant::now() + Duration::from_secs(5);
    while !std::path::Path::new(&release_path).is_file() {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "test-only buffered crossterm reader was not released at {}",
                    std::path::Path::new(&release_path).display()
                ),
            ));
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}

impl Drop for TerminalEventSource {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(character: char) -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE))
    }

    #[test]
    fn input_error_round_trips_without_losing_the_error_kind() {
        let state = InputState::default();
        state.mark_failed(&io::Error::new(
            io::ErrorKind::NotConnected,
            "terminal detached",
        ));
        let error = state
            .current_failure()
            .expect("the first terminal input failure must remain observable");
        assert_eq!(error.kind(), io::ErrorKind::NotConnected);
        assert_eq!(error.to_string(), "terminal detached");
        state.mark_failed(&io::Error::other("later failure"));
        assert_eq!(
            state
                .current_failure()
                .expect("the first failure remains authoritative")
                .kind(),
            io::ErrorKind::NotConnected
        );
    }

    #[test]
    fn timed_input_burst_preserves_reader_arrival_instants() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let state = InputState::default();
        let first_arrival = Instant::now();
        let second_arrival = first_arrival + Duration::from_millis(17);
        for timed_event in [
            TimedInputEvent {
                event: press('a'),
                arrived_at: first_arrival,
            },
            TimedInputEvent {
                event: press('b'),
                arrived_at: second_arrival,
            },
        ] {
            sender
                .send(timed_event)
                .expect("the test receiver remains connected");
        }
        let mut source = TerminalEventSource {
            receiver: Some(receiver),
            state,
            shutdown: Arc::new(AtomicBool::new(false)),
            parking: ReaderParking::default(),
            handle: None,
        };

        let receiver = source
            .receiver_mut()
            .expect("the timestamped receiver should be readable");
        let events = [receiver.try_recv(), receiver.try_recv()]
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("the timestamped burst should be readable");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].arrived_at, first_arrival);
        assert_eq!(events[1].arrived_at, second_arrival);
        assert_eq!(events[0].event, press('a'));
        assert_eq!(events[1].event, press('b'));
    }

    #[test]
    fn timed_input_event_is_stamped_before_it_can_be_queued() {
        let before = Instant::now();
        let event = TimedInputEvent::now(press('x'));
        let after = Instant::now();

        assert!(event.arrived_at >= before);
        assert!(event.arrived_at <= after);
    }

    #[test]
    fn generation_change_discards_only_the_already_accepted_channel_prefix() {
        let (sender, receiver) = mpsc::unbounded_channel();
        sender
            .send(TimedInputEvent::now(press('a')))
            .expect("queue pre-generation input");
        sender
            .send(TimedInputEvent::now(press('b')))
            .expect("queue second pre-generation input");
        let mut source = TerminalEventSource {
            receiver: Some(receiver),
            state: InputState::default(),
            shutdown: Arc::new(AtomicBool::new(false)),
            parking: ReaderParking::default(),
            handle: None,
        };

        assert_eq!(source.discard_pending().expect("discard pending input"), 2);
        assert!(
            source
                .receiver_mut()
                .expect("receiver remains live")
                .try_recv()
                .is_err()
        );
        sender
            .send(TimedInputEvent::now(press('c')))
            .expect("queue post-generation input");
        assert_eq!(
            source
                .receiver_mut()
                .expect("receiver remains live")
                .try_recv()
                .expect("post-generation input remains")
                .event,
            press('c')
        );
    }

    #[test]
    fn accepted_events_are_drained_before_the_fatal_reader_error() {
        let (sender, receiver) = mpsc::unbounded_channel();
        let state = InputState::default();
        sender
            .send(TimedInputEvent::now(press('q')))
            .expect("the test receiver remains connected");
        state.mark_failed(&io::Error::new(
            io::ErrorKind::InvalidData,
            "consecutive parser failures",
        ));
        drop(sender);
        let mut source = TerminalEventSource {
            receiver: Some(receiver),
            state: state.clone(),
            shutdown: Arc::new(AtomicBool::new(false)),
            parking: ReaderParking::default(),
            handle: None,
        };

        let accepted = source
            .receiver_mut()
            .expect("accepted input must remain readable")
            .try_recv()
            .expect("accepted input must precede the terminal failure");
        assert_eq!(accepted.event, press('q'));
        assert!(
            source
                .receiver_mut()
                .expect("the receiver remains inspectable")
                .try_recv()
                .is_err(),
            "the accepted prefix must be exhausted before surfacing failure"
        );

        let failure = source
            .failure()
            .expect("the fatal reader error follows the accepted prefix");
        assert_eq!(failure.kind(), io::ErrorKind::InvalidData);
        assert_eq!(failure.to_string(), "consecutive parser failures");
    }

    #[test]
    fn transient_errors_fail_only_at_fixed_threshold() {
        let mut errors = ConsecutiveEventErrors::default();
        let state = InputState::default();
        let transient = io::Error::new(io::ErrorKind::InvalidData, "malformed terminal bytes");

        for observed in 1..MAX_CONSECUTIVE_EVENT_ERRORS {
            assert!(
                !record_reader_error(&mut errors, &state, &transient),
                "error {observed} must remain transient"
            );
            assert_eq!(errors.count, observed);
            assert!(state.current_failure().is_none());
        }
        assert!(record_reader_error(&mut errors, &state, &transient));
        assert_eq!(errors.count, MAX_CONSECUTIVE_EVENT_ERRORS);
        let failure = state
            .current_failure()
            .expect("the threshold error must become observable");
        assert_eq!(failure.kind(), io::ErrorKind::InvalidData);
        assert_eq!(failure.to_string(), "malformed terminal bytes");
    }

    #[test]
    fn successful_event_resets_consecutive_error_threshold() {
        let mut errors = ConsecutiveEventErrors::default();
        for _ in 0..(MAX_CONSECUTIVE_EVENT_ERRORS - 1) {
            assert!(!errors.record_error());
        }

        errors.record_success();

        assert_eq!(errors.count, 0);
        assert!(!errors.record_error());
        assert_eq!(errors.count, 1);
    }

    #[test]
    fn park_input_reader_timeout_clears_stale_acknowledgement() {
        let parking = ReaderParking::default();
        parking.parked.store(true, Ordering::Release);

        let acknowledged = parking.park(Duration::ZERO);

        assert!(!acknowledged);
        assert!(!parking.parked.load(Ordering::Acquire));
        assert!(parking.pause_requested.load(Ordering::Acquire));
        parking.unpark();
    }

    #[test]
    fn parked_reader_remains_paused_until_handoff_owner_unparks() {
        let parking = ReaderParking::default();
        let reader_parking = parking.clone();
        let reader = std::thread::spawn(move || {
            while !reader_parking.pause_requested() {
                std::thread::yield_now();
            }
            reader_parking.acknowledge_parked();
            while reader_parking.pause_requested() {
                std::thread::yield_now();
            }
        });

        assert!(parking.park(Duration::from_secs(1)));
        assert!(parking.pause_requested());
        assert!(parking.parked.load(Ordering::Acquire));
        parking.unpark();
        reader.join().expect("the mock reader should unpark");
        assert!(!parking.pause_requested());
    }
}

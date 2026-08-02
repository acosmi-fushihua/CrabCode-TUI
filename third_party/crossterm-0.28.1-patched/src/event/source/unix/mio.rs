use std::{collections::VecDeque, io, time::Duration};

use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use signal_hook_mio::v1_0::Signals;

#[cfg(feature = "bracketed-paste")]
use crate::event::source::unix::bounded_paste::{BoundedPaste, BRACKETED_PASTE_START};
#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{
    source::EventSource, sys::unix::parse::parse_event, timeout::PollTimeout, Event, InternalEvent,
};
use crate::terminal::sys::file_descriptor::{terminal_input_fd, FileDesc};

// Tokens to identify file descriptor
const TTY_TOKEN: Token = Token(0);
const SIGNAL_TOKEN: Token = Token(1);
#[cfg(feature = "event-stream")]
const WAKE_TOKEN: Token = Token(2);

// I (@zrzka) wasn't able to read more than 1_022 bytes when testing
// reading on macOS/Linux -> we don't need bigger buffer and 1k of bytes
// is enough.
const TTY_BUFFER_SIZE: usize = 1_024;

#[derive(Clone, Copy)]
struct ReadyEvent {
    token: Token,
}

struct TerminalDrain {
    closed: bool,
    error: Option<io::Error>,
}

pub(crate) struct UnixInternalEventSource {
    poll: Poll,
    events: Events,
    pending_events: VecDeque<InternalEvent>,
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty_fd: FileDesc<'static>,
    tty_closed: bool,
    tty_error: Option<io::Error>,
    signals: Signals,
    #[cfg(feature = "event-stream")]
    waker: Waker,
}

/// Drain one edge-triggered TTY readiness notification to `WouldBlock` or EOF.
///
/// Mio guarantees only readable/writable readiness across all selectors:
/// close/error bits can be absent, and any readiness can be spurious. The
/// production event descriptor is therefore an independently opened,
/// permanently nonblocking terminal input. Every readable edge is resolved by
/// an actual read: `WouldBlock` means a spurious/exhausted edge and `read(0)`
/// alone proves EOF. A one-chunk lookahead keeps parser `more` semantics exact
/// even when the input length is a multiple of `TTY_BUFFER_SIZE`.
fn drain_tty_readiness(
    tty_fd: &FileDesc<'_>,
    tty_buffer: &mut [u8; TTY_BUFFER_SIZE],
    parser: &mut Parser,
    pending_events: &mut VecDeque<InternalEvent>,
) -> TerminalDrain {
    drain_tty_readiness_with(
        |buffer| tty_fd.read(buffer),
        tty_buffer,
        parser,
        pending_events,
    )
}

fn drain_tty_readiness_with(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    tty_buffer: &mut [u8; TTY_BUFFER_SIZE],
    parser: &mut Parser,
    pending_events: &mut VecDeque<InternalEvent>,
) -> TerminalDrain {
    let mut previous_buffer = [0u8; TTY_BUFFER_SIZE];
    let mut previous_len = 0;
    loop {
        match read(tty_buffer) {
            Ok(0) => {
                if previous_len > 0 {
                    parser.advance(&previous_buffer[..previous_len], false);
                    pending_events.extend(parser.by_ref());
                }
                return TerminalDrain {
                    closed: true,
                    error: None,
                };
            }
            Ok(read_count) => {
                if previous_len > 0 {
                    parser.advance(&previous_buffer[..previous_len], true);
                    pending_events.extend(parser.by_ref());
                }
                previous_buffer[..read_count].copy_from_slice(&tty_buffer[..read_count]);
                previous_len = read_count;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if previous_len > 0 {
                    parser.advance(&previous_buffer[..previous_len], false);
                    pending_events.extend(parser.by_ref());
                }
                return TerminalDrain {
                    closed: false,
                    error: None,
                };
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if previous_len > 0 {
                    parser.advance(&previous_buffer[..previous_len], false);
                    pending_events.extend(parser.by_ref());
                }
                return TerminalDrain {
                    closed: false,
                    error: Some(error),
                };
            }
        }
    }
}

fn accept_ready_batch(
    ready_events: impl IntoIterator<Item = ReadyEvent>,
    tty_fd: &FileDesc<'_>,
    tty_buffer: &mut [u8; TTY_BUFFER_SIZE],
    parser: &mut Parser,
    pending_events: &mut VecDeque<InternalEvent>,
    tty_closed: &mut bool,
    tty_error: &mut Option<io::Error>,
    mut take_resize_event: impl FnMut() -> io::Result<Option<InternalEvent>>,
) -> io::Result<bool> {
    #[cfg(feature = "event-stream")]
    let mut woke = false;
    #[cfg(not(feature = "event-stream"))]
    let woke = false;
    for ready in ready_events {
        match ready.token {
            TTY_TOKEN => {
                let drain = drain_tty_readiness(tty_fd, tty_buffer, parser, pending_events);
                *tty_closed |= drain.closed;
                if tty_error.is_none() {
                    *tty_error = drain.error;
                }
            }
            SIGNAL_TOKEN => {
                if let Some(event) = take_resize_event()? {
                    pending_events.push_back(event);
                }
            }
            #[cfg(feature = "event-stream")]
            WAKE_TOKEN => {
                // Do not return until every token in this edge-triggered batch
                // has been accepted. The caller gives the wake priority after
                // the batch; queued events remain available to the next poll.
                woke = true;
            }
            _ => unreachable!("Synchronize Evented handle registration & token handling"),
        }
    }
    Ok(woke)
}

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        UnixInternalEventSource::from_file_descriptor(terminal_input_fd()?)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc<'static>) -> io::Result<Self> {
        let input_flags = rustix::fs::fcntl_getfl(&input_fd)?;
        if !input_flags.contains(rustix::fs::OFlags::NONBLOCK) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "terminal event input descriptor must already be nonblocking",
            ));
        }
        let poll = Poll::new()?;
        let registry = poll.registry();

        let tty_raw_fd = input_fd.raw_fd();
        let mut tty_ev = SourceFd(&tty_raw_fd);
        registry.register(&mut tty_ev, TTY_TOKEN, Interest::READABLE)?;

        let mut signals = Signals::new([signal_hook::consts::SIGWINCH])?;
        registry.register(&mut signals, SIGNAL_TOKEN, Interest::READABLE)?;

        #[cfg(feature = "event-stream")]
        let waker = Waker::new(registry, WAKE_TOKEN)?;

        Ok(UnixInternalEventSource {
            poll,
            events: Events::with_capacity(3),
            pending_events: VecDeque::with_capacity(3),
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty_fd: input_fd,
            tty_closed: false,
            tty_error: None,
            signals,
            #[cfg(feature = "event-stream")]
            waker,
        })
    }
}

impl EventSource for UnixInternalEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        if let Some(event) = self.pending_events.pop_front() {
            return Ok(Some(event));
        }
        if let Some(event) = self.parser.next() {
            return Ok(Some(event));
        }
        if let Some(error) = self.tty_error.take() {
            return Err(error);
        }
        if self.tty_closed {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "terminal input closed",
            ));
        }

        let timeout = PollTimeout::new(timeout);

        loop {
            if let Err(e) = self.poll.poll(&mut self.events, timeout.leftover()) {
                // Mio will throw an interrupted error in case of cursor position retrieval. We need to retry until it succeeds.
                // Previous versions of Mio (< 0.7) would automatically retry the poll call if it was interrupted (if EINTR was returned).
                // https://docs.rs/mio/0.7.0/mio/struct.Poll.html#notes
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                } else {
                    return Err(e);
                }
            };

            if self.events.is_empty() {
                // No readiness events = timeout
                return Ok(None);
            }

            // Drain the whole readiness batch before returning one event. On
            // edge-triggered pollers (notably macOS kqueue), returning for a
            // SIGWINCH token before a simultaneously-ready TTY token would
            // discard that TTY readiness edge while its bytes remained
            // unread. Preserve every accepted event in `pending_events`.
            let ready_events = self
                .events
                .iter()
                .map(|event| ReadyEvent {
                    token: event.token(),
                })
                .collect::<Vec<_>>();
            let woke = {
                let Self {
                    pending_events,
                    parser,
                    tty_buffer,
                    tty_fd,
                    tty_closed,
                    tty_error,
                    signals,
                    ..
                } = self;
                accept_ready_batch(
                    ready_events,
                    tty_fd,
                    tty_buffer,
                    parser,
                    pending_events,
                    tty_closed,
                    tty_error,
                    || {
                        if signals.pending().next() == Some(signal_hook::consts::SIGWINCH) {
                            // TODO Should we remove tput?
                            //
                            // This can take a really long time, because terminal::size can
                            // launch new process (tput) and then it parses its output. It's
                            // not a really long time from the absolute time point of view, but
                            // it's a really long time from the mio, async-std/tokio executor, ...
                            // point of view.
                            let new_size = crate::terminal::size()?;
                            Ok(Some(InternalEvent::Event(Event::Resize(
                                new_size.0, new_size.1,
                            ))))
                        } else {
                            Ok(None)
                        }
                    },
                )?
            };
            if woke {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Poll operation was woken up by `Waker::wake`",
                ));
            }

            if let Some(event) = self.pending_events.pop_front() {
                return Ok(Some(event));
            }
            if let Some(error) = self.tty_error.take() {
                return Err(error);
            }
            if self.tty_closed {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "terminal input closed",
                ));
            }

            // Processing above can take some time, check if timeout expired
            if timeout.elapsed() {
                return Ok(None);
            }
        }
    }

    fn reset_generation(&mut self) {
        self.parser.reset_generation();
        self.pending_events.clear();
        self.events.clear();
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.waker.clone()
    }
}

//
// Following `Parser` structure exists for two reasons:
//
//  * mimic anes Parser interface
//  * move the advancing, parsing, ... stuff out of the `try_read` method
//
#[derive(Debug)]
struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
    #[cfg(feature = "bracketed-paste")]
    bounded_paste: Option<BoundedPaste>,
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            // This buffer is used for -> 1 <- ANSI escape sequence. Are we
            // aware of any ANSI escape sequence that is bigger? Can we make
            // it smaller?
            //
            // Probably not worth spending more time on this as "there's a plan"
            // to use the anes crate parser.
            buffer: Vec::with_capacity(256),
            // TTY_BUFFER_SIZE is 1_024 bytes. How many ANSI escape sequences can
            // fit? What is an average sequence length? Let's guess here
            // and say that the average ANSI escape sequence length is 8 bytes. Thus
            // the buffer size should be 1024/8=128 to avoid additional allocations
            // when processing large amounts of data.
            //
            // There's no need to make it bigger: `drain_tty_readiness` moves
            // every complete event into the source-level pending queue before
            // the next TTY_BUFFER is processed.
            internal_events: VecDeque::with_capacity(128),
            #[cfg(feature = "bracketed-paste")]
            bounded_paste: None,
        }
    }
}

impl Parser {
    fn reset_generation(&mut self) {
        *self = Self::default();
    }

    fn advance(&mut self, buffer: &[u8], more: bool) {
        for (idx, byte) in buffer.iter().enumerate() {
            let more = idx + 1 < buffer.len() || more;

            #[cfg(feature = "bracketed-paste")]
            if let Some(paste) = self.bounded_paste.as_mut() {
                if let Some(text) = paste.advance(*byte) {
                    self.internal_events
                        .push_back(InternalEvent::Event(Event::Paste(text)));
                    self.bounded_paste = None;
                }
                continue;
            }

            self.buffer.push(*byte);

            #[cfg(feature = "bracketed-paste")]
            if self.buffer == BRACKETED_PASTE_START {
                self.buffer.clear();
                self.bounded_paste = Some(BoundedPaste::default());
                continue;
            }

            match parse_event(&self.buffer, more) {
                Ok(Some(ie)) => {
                    self.internal_events.push_back(ie);
                    self.buffer.clear();
                }
                Ok(None) => {
                    // Event can't be parsed, because we don't have enough bytes for
                    // the current sequence. Keep the buffer and process next bytes.
                }
                Err(_) => {
                    // Event can't be parsed (not enough parameters, parameter is not a number, ...).
                    // Clear the buffer and continue with another sequence.
                    self.buffer.clear();
                }
            }
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal_events.pop_front()
    }
}

#[cfg(test)]
mod readiness_tests {
    use std::io::Write as _;
    #[cfg(feature = "libc")]
    use std::os::fd::IntoRawFd as _;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::event::KeyCode;

    fn ready(token: Token) -> ReadyEvent {
        ReadyEvent { token }
    }

    fn owned_descriptor(reader: UnixStream) -> FileDesc<'static> {
        #[cfg(feature = "libc")]
        {
            FileDesc::new(reader.into_raw_fd(), true)
        }
        #[cfg(not(feature = "libc"))]
        {
            FileDesc::Owned(reader.into())
        }
    }

    fn descriptor(reader: UnixStream) -> FileDesc<'static> {
        reader
            .set_nonblocking(true)
            .expect("make readiness fixture nonblocking");
        owned_descriptor(reader)
    }

    #[test]
    fn constructor_rejects_blocking_descriptor_without_mutating_its_open_file_description() {
        let (reader, _writer) = UnixStream::pair().expect("create blocking descriptor fixture");
        let retained = reader
            .try_clone()
            .expect("retain alias for descriptor flag inspection");
        let original_flags =
            rustix::fs::fcntl_getfl(&retained).expect("read original descriptor flags");
        assert!(!original_flags.contains(rustix::fs::OFlags::NONBLOCK));

        let error = match UnixInternalEventSource::from_file_descriptor(owned_descriptor(reader)) {
            Ok(_) => panic!("blocking event input must be rejected"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            rustix::fs::fcntl_getfl(&retained).expect("read flags after rejected constructor"),
            original_flags,
            "constructor must never mutate a caller's open-file description"
        );
    }

    #[test]
    fn tty_readiness_is_drained_past_one_buffer_and_one_event() {
        let (reader, mut writer) = UnixStream::pair().expect("create readiness socket pair");
        let input_fd = descriptor(reader);

        let mut source =
            UnixInternalEventSource::from_file_descriptor(input_fd).expect("create event source");
        let payload = vec![b'x'; TTY_BUFFER_SIZE * 3 + 17];
        writer
            .write_all(&payload)
            .expect("write more than one TTY buffer");

        let drain = drain_tty_readiness(
            &source.tty_fd,
            &mut source.tty_buffer,
            &mut source.parser,
            &mut source.pending_events,
        );

        assert!(!drain.closed);
        assert!(drain.error.is_none());
        assert_eq!(source.pending_events.len(), payload.len());
        for event in source.pending_events.drain(..) {
            match event {
                InternalEvent::Event(Event::Key(key)) => {
                    assert_eq!(key.code, KeyCode::Char('x'));
                }
                other => panic!("unexpected event from readiness payload: {other:?}"),
            }
        }
    }

    #[test]
    fn exact_buffer_multiples_finalize_a_trailing_escape_key() {
        for (payload_len, peer_closed) in [
            (TTY_BUFFER_SIZE, false),
            (TTY_BUFFER_SIZE, true),
            (TTY_BUFFER_SIZE * 2, false),
            (TTY_BUFFER_SIZE * 2, true),
        ] {
            let (reader, writer) = UnixStream::pair().expect("create readiness socket pair");
            let input_fd = descriptor(reader);

            let mut source = UnixInternalEventSource::from_file_descriptor(input_fd)
                .expect("create event source");
            let mut payload = vec![b'x'; payload_len];
            payload[payload_len - 1] = b'\x1b';
            let mut writer = Some(writer);
            writer
                .as_mut()
                .expect("writer remains available")
                .write_all(&payload)
                .expect("write an exact TTY-buffer multiple");
            if peer_closed {
                drop(writer.take());
            }

            let drain = drain_tty_readiness(
                &source.tty_fd,
                &mut source.tty_buffer,
                &mut source.parser,
                &mut source.pending_events,
            );

            assert_eq!(drain.closed, peer_closed);
            assert!(drain.error.is_none());
            assert_eq!(source.pending_events.len(), payload_len);
            assert!(matches!(
                source.pending_events.back(),
                Some(InternalEvent::Event(Event::Key(key))) if key.code == KeyCode::Esc
            ));
        }
    }

    #[test]
    fn read_error_after_bytes_is_deferred_until_accepted_events_are_delivered() {
        let (reader, _writer) = UnixStream::pair().expect("create event source fixture");
        let input_fd = descriptor(reader);
        let mut source =
            UnixInternalEventSource::from_file_descriptor(input_fd).expect("create event source");
        let mut reads = 0;
        let drain = drain_tty_readiness_with(
            |buffer| {
                reads += 1;
                if reads == 1 {
                    buffer[0] = b'x';
                    Ok(1)
                } else {
                    Err(io::Error::new(
                        io::ErrorKind::Other,
                        "injected terminal read failure",
                    ))
                }
            },
            &mut source.tty_buffer,
            &mut source.parser,
            &mut source.pending_events,
        );
        source.tty_error = drain.error;

        assert!(!drain.closed);
        assert!(matches!(
            source.try_read(Some(Duration::ZERO)),
            Ok(Some(InternalEvent::Event(Event::Key(key))))
                if key.code == KeyCode::Char('x')
        ));
        let error = source
            .try_read(Some(Duration::ZERO))
            .expect_err("deferred terminal read error follows accepted input");
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn spurious_tty_readiness_never_blocks_on_an_empty_nonblocking_descriptor() {
        let (reader, writer) = UnixStream::pair().expect("create readiness socket pair");
        reader
            .set_nonblocking(true)
            .expect("make the owned event input nonblocking");
        let (result_sender, result_receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let input_fd = descriptor(reader);

            let mut parser = Parser::default();
            let mut buffer = [0u8; TTY_BUFFER_SIZE];
            let mut pending = VecDeque::new();
            let drain = drain_tty_readiness(&input_fd, &mut buffer, &mut parser, &mut pending);
            let result = (
                drain.closed,
                drain.error.map(|error| error.kind()),
                pending.len(),
            );
            let _ = result_sender.send(result);
        });

        let result = match result_receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(result) => result,
            Err(error) => {
                drop(writer);
                let _ = worker.join();
                panic!("empty synthetic readiness blocked a terminal read: {error}");
            }
        };
        drop(writer);
        worker.join().expect("join empty-readiness worker");
        assert_eq!(result, (false, None, 0));
    }

    #[test]
    fn closed_tty_readiness_is_reported_after_queued_events_are_exhausted() {
        let (reader, mut writer) = UnixStream::pair().expect("create readiness socket pair");
        let input_fd = descriptor(reader);
        let mut source =
            UnixInternalEventSource::from_file_descriptor(input_fd).expect("create event source");
        writer.write_all(b"x").expect("queue one terminal event");
        drop(writer);

        let woke = accept_ready_batch(
            [ready(TTY_TOKEN)],
            &source.tty_fd,
            &mut source.tty_buffer,
            &mut source.parser,
            &mut source.pending_events,
            &mut source.tty_closed,
            &mut source.tty_error,
            || Ok(None),
        )
        .expect("accept readable-only terminal EOF");

        assert!(!woke);
        assert!(source.tty_closed);
        assert!(matches!(
            source.try_read(Some(Duration::ZERO)),
            Ok(Some(InternalEvent::Event(Event::Key(key))))
                if key.code == KeyCode::Char('x')
        ));
        let error = source
            .try_read(Some(Duration::ZERO))
            .expect_err("terminal closure follows its queued input");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn spurious_readable_edge_does_not_poison_future_terminal_input_or_flags() {
        let (reader, mut writer) = UnixStream::pair().expect("create readiness socket pair");
        let input_fd = descriptor(reader);
        let mut source =
            UnixInternalEventSource::from_file_descriptor(input_fd).expect("create event source");
        let original_flags =
            rustix::fs::fcntl_getfl(&source.tty_fd).expect("read original descriptor flags");

        accept_ready_batch(
            [ready(TTY_TOKEN)],
            &source.tty_fd,
            &mut source.tty_buffer,
            &mut source.parser,
            &mut source.pending_events,
            &mut source.tty_closed,
            &mut source.tty_error,
            || Ok(None),
        )
        .expect("accept a synthetic spurious readable edge");

        assert!(!source.tty_closed);
        assert_eq!(
            rustix::fs::fcntl_getfl(&source.tty_fd)
                .expect("read descriptor flags after spurious readable edge"),
            original_flags,
            "readiness draining must not mutate the event descriptor flags"
        );

        writer
            .write_all(b"x")
            .expect("write terminal input after the spurious readable edge");
        accept_ready_batch(
            [ready(TTY_TOKEN)],
            &source.tty_fd,
            &mut source.tty_buffer,
            &mut source.parser,
            &mut source.pending_events,
            &mut source.tty_closed,
            &mut source.tty_error,
            || Ok(None),
        )
        .expect("accept terminal input after the spurious readable edge");
        assert!(matches!(
            source.pending_events.front(),
            Some(InternalEvent::Event(Event::Key(key)))
                if key.code == KeyCode::Char('x')
        ));
    }

    #[test]
    fn resize_and_tty_tokens_are_both_retained_in_either_batch_order() {
        for ready_events in [
            [ready(SIGNAL_TOKEN), ready(TTY_TOKEN)],
            [ready(TTY_TOKEN), ready(SIGNAL_TOKEN)],
        ] {
            let (reader, mut writer) = UnixStream::pair().expect("create readiness socket pair");
            let input_fd = descriptor(reader);
            let mut source = UnixInternalEventSource::from_file_descriptor(input_fd)
                .expect("create event source");
            writer.write_all(b"x").expect("make the TTY token readable");

            let woke = accept_ready_batch(
                ready_events,
                &source.tty_fd,
                &mut source.tty_buffer,
                &mut source.parser,
                &mut source.pending_events,
                &mut source.tty_closed,
                &mut source.tty_error,
                || Ok(Some(InternalEvent::Event(Event::Resize(80, 24)))),
            )
            .expect("accept synthetic resize and real TTY readiness");

            assert!(!woke);
            assert_eq!(source.pending_events.len(), 2);
            assert!(source.pending_events.iter().any(|event| {
                matches!(
                    event,
                    InternalEvent::Event(Event::Key(key))
                        if key.code == KeyCode::Char('x')
                )
            }));
            assert!(source
                .pending_events
                .iter()
                .any(|event| { matches!(event, InternalEvent::Event(Event::Resize(80, 24))) }));
        }
    }

    #[cfg(feature = "event-stream")]
    #[test]
    fn wake_token_does_not_abandon_a_later_tty_token_in_the_same_batch() {
        let (reader, mut writer) = UnixStream::pair().expect("create readiness socket pair");
        let input_fd = descriptor(reader);
        let mut source =
            UnixInternalEventSource::from_file_descriptor(input_fd).expect("create event source");
        writer.write_all(b"x").expect("make the TTY token readable");

        let woke = accept_ready_batch(
            [ready(WAKE_TOKEN), ready(TTY_TOKEN)],
            &source.tty_fd,
            &mut source.tty_buffer,
            &mut source.parser,
            &mut source.pending_events,
            &mut source.tty_closed,
            &mut source.tty_error,
            || Ok(None),
        )
        .expect("accept wake and TTY readiness");

        assert!(woke);
        assert!(matches!(
            source.pending_events.front(),
            Some(InternalEvent::Event(Event::Key(key)))
                if key.code == KeyCode::Char('x')
        ));
    }
}

#[cfg(all(test, feature = "bracketed-paste"))]
mod bounded_paste_integration_tests {
    use super::*;
    use crate::event::{Event, KeyCode};

    #[test]
    fn bracketed_payload_is_one_event_and_following_key_is_independent() {
        let mut parser = Parser::default();
        parser.advance(BRACKETED_PASTE_START, true);
        parser.advance(b"body\x1b[2Dstill-body", true);
        parser.advance(b"\x1b[20", true);
        parser.advance(b"1~x", false);

        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Paste(
                "body\x1b[2Dstill-body".to_string()
            )))
        );
        assert_eq!(
            parser.next(),
            Some(InternalEvent::Event(Event::Key(KeyCode::Char('x').into())))
        );
        assert_eq!(parser.next(), None);
    }

    fn key_chars(parser: &mut Parser) -> String {
        let mut chars = String::new();
        while let Some(event) = parser.next() {
            match event {
                InternalEvent::Event(Event::Key(key)) => match key.code {
                    KeyCode::Char(character) => chars.push(character),
                    other => panic!("unexpected key after generation reset: {other:?}"),
                },
                other => panic!("unexpected event after generation reset: {other:?}"),
            }
        }
        chars
    }

    #[test]
    fn generation_reset_discards_complete_and_partial_csi_state() {
        let mut parser = Parser::default();
        parser.advance(b"old\x1b[", false);
        assert!(parser.next().is_some(), "one old complete event is queued");

        parser.reset_generation();
        parser.advance(b"fresh", false);

        assert_eq!(key_chars(&mut parser), "fresh");
    }

    #[test]
    fn generation_reset_discards_split_utf8_prefix() {
        let mut parser = Parser::default();
        parser.advance(&[b'x', 0xe7, 0x95], false);
        assert!(parser.next().is_some(), "one old complete event is queued");

        parser.reset_generation();
        parser.advance(b"fresh", false);

        assert_eq!(key_chars(&mut parser), "fresh");
    }

    #[test]
    fn generation_reset_discards_open_bracketed_paste() {
        let mut parser = Parser::default();
        parser.advance(b"x\x1b[200~stale-open-paste", false);
        assert!(parser.next().is_some(), "one old complete event is queued");

        parser.reset_generation();
        parser.advance(b"fresh", false);

        assert_eq!(key_chars(&mut parser), "fresh");
    }
}

#[cfg(feature = "libc")]
use std::os::unix::prelude::AsRawFd;
use std::{collections::VecDeque, io, os::unix::net::UnixStream, time::Duration};

#[cfg(not(feature = "libc"))]
use rustix::fd::{AsFd, AsRawFd};

use signal_hook::low_level::pipe;

use crate::event::timeout::PollTimeout;
use crate::event::Event;
use filedescriptor::{poll, pollfd, POLLERR, POLLHUP, POLLIN};

#[cfg(feature = "bracketed-paste")]
use crate::event::source::unix::bounded_paste::{BoundedPaste, BRACKETED_PASTE_START};
#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{source::EventSource, sys::unix::parse::parse_event, InternalEvent};
use crate::terminal::sys::file_descriptor::{terminal_input_fd, FileDesc};

/// Holds a prototypical Waker and a receiver we can wait on when doing select().
#[cfg(feature = "event-stream")]
struct WakePipe {
    receiver: UnixStream,
    waker: Waker,
}

#[cfg(feature = "event-stream")]
impl WakePipe {
    fn new() -> io::Result<Self> {
        let (receiver, sender) = nonblocking_unix_pair()?;
        Ok(WakePipe {
            receiver,
            waker: Waker::new(sender),
        })
    }
}

// I (@zrzka) wasn't able to read more than 1_022 bytes when testing
// reading on macOS/Linux -> we don't need bigger buffer and 1k of bytes
// is enough.
const TTY_BUFFER_SIZE: usize = 1_024;

pub(crate) struct UnixInternalEventSource {
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty: FileDesc<'static>,
    tty_closed: bool,
    tty_error: Option<io::Error>,
    winch_signal_receiver: UnixStream,
    #[cfg(feature = "event-stream")]
    wake_pipe: WakePipe,
}

fn nonblocking_unix_pair() -> io::Result<(UnixStream, UnixStream)> {
    let (receiver, sender) = UnixStream::pair()?;
    receiver.set_nonblocking(true)?;
    sender.set_nonblocking(true)?;
    Ok((receiver, sender))
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
        Ok(UnixInternalEventSource {
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty: input_fd,
            tty_closed: false,
            tty_error: None,
            winch_signal_receiver: {
                let (receiver, sender) = nonblocking_unix_pair()?;
                // Unregistering is unnecessary because EventSource is a singleton
                #[cfg(feature = "libc")]
                pipe::register(libc::SIGWINCH, sender)?;
                #[cfg(not(feature = "libc"))]
                pipe::register(rustix::process::Signal::Winch as i32, sender)?;
                receiver
            },
            #[cfg(feature = "event-stream")]
            wake_pipe: WakePipe::new()?,
        })
    }
}

/// read_complete reads from a non-blocking file descriptor
/// until the buffer is full or it would block.
///
/// Similar to `std::io::Read::read_to_end`, except this function
/// only fills the given buffer and does not read beyond that.
fn read_complete(fd: &FileDesc, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        match fd.read(buf) {
            Ok(x) => return Ok(x),
            Err(e) => match e.kind() {
                io::ErrorKind::WouldBlock => return Ok(0),
                io::ErrorKind::Interrupted => continue,
                _ => return Err(e),
            },
        }
    }
}

fn poll_retrying_interrupted<T>(
    mut poll_once: impl FnMut() -> Result<T, filedescriptor::Error>,
) -> Result<T, filedescriptor::Error> {
    loop {
        match poll_once() {
            Err(filedescriptor::Error::Poll(error)) | Err(filedescriptor::Error::Io(error))
                if error.kind() == io::ErrorKind::Interrupted =>
            {
                continue;
            }
            result => return result,
        }
    }
}

/// Drain nonblocking terminal input to `WouldBlock` or EOF.
///
/// `poll` can report a close as readable-only, HUP-only, or error readiness.
/// Actual reads are authoritative. One-chunk lookahead finalizes exact buffer
/// multiples without leaving a trailing escape sequence buffered forever.
struct TerminalDrain {
    closed: bool,
    error: Option<io::Error>,
}

fn drain_terminal(
    fd: &FileDesc,
    buf: &mut [u8; TTY_BUFFER_SIZE],
    parser: &mut Parser,
) -> TerminalDrain {
    drain_terminal_with(|buffer| fd.read(buffer), buf, parser)
}

fn drain_terminal_with(
    mut read: impl FnMut(&mut [u8]) -> io::Result<usize>,
    buf: &mut [u8; TTY_BUFFER_SIZE],
    parser: &mut Parser,
) -> TerminalDrain {
    let mut previous_buffer = [0u8; TTY_BUFFER_SIZE];
    let mut previous_len = 0;
    loop {
        match read(buf) {
            Ok(0) => {
                if previous_len > 0 {
                    parser.advance(&previous_buffer[..previous_len], false);
                }
                return TerminalDrain {
                    closed: true,
                    error: None,
                };
            }
            Ok(read_count) => {
                if previous_len > 0 {
                    parser.advance(&previous_buffer[..previous_len], true);
                }
                previous_buffer[..read_count].copy_from_slice(&buf[..read_count]);
                previous_len = read_count;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if previous_len > 0 {
                    parser.advance(&previous_buffer[..previous_len], false);
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
                }
                return TerminalDrain {
                    closed: false,
                    error: Some(error),
                };
            }
        }
    }
}

impl EventSource for UnixInternalEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
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

        fn make_pollfd<F: AsRawFd>(fd: &F) -> pollfd {
            pollfd {
                fd: fd.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            }
        }

        #[cfg(not(feature = "event-stream"))]
        let mut fds = [
            make_pollfd(&self.tty),
            make_pollfd(&self.winch_signal_receiver),
        ];

        #[cfg(feature = "event-stream")]
        let mut fds = [
            make_pollfd(&self.tty),
            make_pollfd(&self.winch_signal_receiver),
            make_pollfd(&self.wake_pipe.receiver),
        ];

        loop {
            // check if there are buffered events from the last read
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
            let poll_result = poll_retrying_interrupted(|| poll(&mut fds, timeout.leftover()));
            match poll_result {
                Err(filedescriptor::Error::Poll(e)) | Err(filedescriptor::Error::Io(e)) => {
                    return Err(e);
                }
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("got unexpected error while polling: {:?}", e),
                    ))
                }
                Ok(_) => (),
            };
            if fds[0].revents & (POLLIN | POLLHUP | POLLERR) != 0 {
                let drain = drain_terminal(&self.tty, &mut self.tty_buffer, &mut self.parser);
                self.tty_closed |= drain.closed;
                if self.tty_error.is_none() {
                    self.tty_error = drain.error;
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
            }
            if fds[1].revents & POLLIN != 0 {
                #[cfg(feature = "libc")]
                let fd = FileDesc::new(self.winch_signal_receiver.as_raw_fd(), false);
                #[cfg(not(feature = "libc"))]
                let fd = FileDesc::Borrowed(self.winch_signal_receiver.as_fd());
                // drain the pipe
                while read_complete(&fd, &mut [0; 1024])? != 0 {}
                // TODO Should we remove tput?
                //
                // This can take a really long time, because terminal::size can
                // launch new process (tput) and then it parses its output. It's
                // not a really long time from the absolute time point of view, but
                // it's a really long time from the mio, async-std/tokio executor, ...
                // point of view.
                let new_size = crate::terminal::size()?;
                return Ok(Some(InternalEvent::Event(Event::Resize(
                    new_size.0, new_size.1,
                ))));
            }

            #[cfg(feature = "event-stream")]
            if fds[2].revents & POLLIN != 0 {
                #[cfg(feature = "libc")]
                let fd = FileDesc::new(self.wake_pipe.receiver.as_raw_fd(), false);
                #[cfg(not(feature = "libc"))]
                let fd = FileDesc::Borrowed(self.wake_pipe.receiver.as_fd());
                // drain the pipe
                while read_complete(&fd, &mut [0; 1024])? != 0 {}

                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "Poll operation was woken up by `Waker::wake`",
                ));
            }
            if timeout.elapsed() {
                return Ok(None);
            }
        }
    }

    fn reset_generation(&mut self) {
        self.parser.reset_generation();
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.wake_pipe.waker.clone()
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
            // There's no need to make it bigger, because when you look at the `try_read`
            // method implementation, all events are consumed before the next TTY_BUFFER
            // is processed -> events pushed.
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
mod parser_generation_tests {
    use super::*;

    #[test]
    fn reset_discards_complete_and_partial_state() {
        let mut parser = Parser::default();
        parser.advance(b"x\x1b[", false);
        assert!(parser.next().is_some(), "one old complete event is queued");

        parser.reset_generation();
        parser.advance(b"fresh", false);

        let mut chars = String::new();
        while let Some(event) = parser.next() {
            match event {
                InternalEvent::Event(Event::Key(key)) => match key.code {
                    crate::event::KeyCode::Char(character) => chars.push(character),
                    other => panic!("unexpected key after generation reset: {other:?}"),
                },
                other => panic!("unexpected event after generation reset: {other:?}"),
            }
        }
        assert_eq!(chars, "fresh");
    }
}

#[cfg(test)]
mod terminal_read_tests {
    use super::*;
    use crate::event::KeyCode;
    use std::io::Write as _;
    #[cfg(feature = "libc")]
    use std::os::fd::IntoRawFd as _;

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
            .expect("make terminal read fixture nonblocking");
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
    fn interrupted_zero_timeout_poll_is_retried_before_returning() {
        let mut attempts = 0;
        let result = poll_retrying_interrupted(|| {
            attempts += 1;
            if attempts == 1 {
                Err(filedescriptor::Error::Io(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "injected EINTR",
                )))
            } else {
                Ok(0)
            }
        })
        .expect("retry interrupted poll");

        assert_eq!(result, 0);
        assert_eq!(attempts, 2);
    }

    #[test]
    fn closed_terminal_descriptor_is_not_reported_as_would_block() {
        let (reader, writer) = UnixStream::pair().expect("create terminal read fixture");
        drop(writer);
        let descriptor = descriptor(reader);
        let mut buffer = [0; TTY_BUFFER_SIZE];
        let mut parser = Parser::default();
        let drain = drain_terminal(&descriptor, &mut buffer, &mut parser);
        assert!(drain.closed);
        assert!(drain.error.is_none());
    }

    #[test]
    fn exact_buffer_multiples_finalize_before_would_block_and_eof() {
        for (payload_len, peer_closed) in [
            (TTY_BUFFER_SIZE, false),
            (TTY_BUFFER_SIZE, true),
            (TTY_BUFFER_SIZE * 2, false),
            (TTY_BUFFER_SIZE * 2, true),
        ] {
            let (reader, mut writer) = UnixStream::pair().expect("create terminal read fixture");
            let descriptor = descriptor(reader);
            let mut payload = vec![b'x'; payload_len];
            payload[payload_len - 1] = b'\x1b';
            writer
                .write_all(&payload)
                .expect("write exact terminal-buffer multiple");
            if peer_closed {
                drop(writer);
            }

            let mut buffer = [0; TTY_BUFFER_SIZE];
            let mut parser = Parser::default();
            let drain = drain_terminal(&descriptor, &mut buffer, &mut parser);
            assert_eq!(drain.closed, peer_closed);
            assert!(drain.error.is_none());
            assert_eq!(parser.internal_events.len(), payload_len);
            assert!(matches!(
                parser.internal_events.back(),
                Some(InternalEvent::Event(Event::Key(key))) if key.code == KeyCode::Esc
            ));
        }
    }

    #[test]
    fn read_error_after_bytes_is_deferred_until_accepted_events_are_delivered() {
        let (reader, _writer) = UnixStream::pair().expect("create event source fixture");
        let descriptor = descriptor(reader);
        let mut source = UnixInternalEventSource::from_file_descriptor(descriptor)
            .expect("create terminal event source");
        let mut reads = 0;
        let drain = drain_terminal_with(
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
    fn zero_timeout_drains_buffered_events_before_known_eof() {
        let (reader, mut writer) = UnixStream::pair().expect("create terminal read fixture");
        let descriptor = descriptor(reader);
        let mut source = UnixInternalEventSource::from_file_descriptor(descriptor)
            .expect("create terminal event source");
        writer.write_all(b"xy").expect("write two terminal events");
        drop(writer);

        for expected in ['x', 'y'] {
            assert!(matches!(
                source.try_read(Some(Duration::ZERO)),
                Ok(Some(InternalEvent::Event(Event::Key(key))))
                    if key.code == KeyCode::Char(expected)
            ));
        }
        let error = source
            .try_read(Some(Duration::ZERO))
            .expect_err("known EOF follows buffered events");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }
}

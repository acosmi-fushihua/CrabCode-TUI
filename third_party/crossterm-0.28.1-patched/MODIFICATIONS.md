# CrabCode modifications to Crossterm 0.28.1

Upstream project: <https://github.com/crossterm-rs/crossterm>

Upstream version: `0.28.1`

License: MIT (see the adjacent `LICENSE` file)

This directory starts from the crates.io Crossterm 0.28.1 source and is patched
for CrabCode's native terminal client. The material changes are:

- add a Unix bracketed-paste parser with an 8 MiB retained-payload ceiling;
- recognize an end marker split across arbitrary terminal reads and prevent
  partial marker bytes or oversized discarded bytes from escaping as key
  events;
- preserve a truncation signal while returning valid UTF-8 event text;
- integrate that parser into both Unix event-source implementations;
- add a sole-reader terminal-generation reset that clears queued events,
  skipped events, complete decoded events, partial CSI/UTF-8 prefixes, open
  bracketed-paste state, and Windows surrogate/mouse decoder state without
  replacing file descriptors, pollers, signal registrations, or wakers;
- add a raw-mode reassertion that reapplies raw termios from Crossterm's saved
  pre-raw baseline without replacing that baseline, so a later disable still
  restores the exact original shell mode;
- open the event reader as an independently owned, permanently nonblocking
  descriptor for the same kernel-reported terminal device, so `forkpty`/
  `login_tty` stdin/stdout/stderr aliases never inherit event-reader flag
  changes, and use `O_NOCTTY` so reopening the device cannot acquire a
  controlling terminal as a side effect; the input OFD is `O_CLOEXEC` and
  cannot leak into external editor/link/clipboard children; internal
  constructors reject a blocking descriptor without mutating its open-file
  description;
- retain every event accepted from one Unix Mio readiness batch before
  returning, drain each nonblocking edge across multiple buffers to
  `WouldBlock`/EOF, use one-chunk lookahead rather than buffer fullness to
  finalize exact-boundary escape sequences, and defer event-stream wake
  completion until the batch is safe, so spurious readiness, SIGWINCH, or wake
  tokens cannot block or discard a simultaneously-ready TTY edge;
- treat Mio close/error bits only as optional hints and use actual reads for
  every readable edge, so readable-only EOF is observed as `read(0)`, spurious
  readiness remains `WouldBlock`, and accepted bytes are delivered before a
  following read error or EOF; apply the same EOF and exact-boundary rules to
  the `use-dev-tty` poll backend, including HUP/error-only readiness, one real
  zero-time poll, and EINTR retry;
- stop the `EventStream` background poll on terminal errors, retain the exact
  error for the foreground stream poll, and wake the executor once, preventing
  a closed terminal from becoming an unwoken hot loop;
- add parser, split-marker, overflow, UTF-8, generation-reset, multi-buffer,
  exact-buffer-boundary before `WouldBlock` and EOF, spurious/readable-only
  readiness, deferred read-error ordering, descriptor-flag stability,
  zero-time/EINTR polling, event-stream error wake, and raw-reassertion unit
  tests; a direct Unix PTY subprocess test proves the event reader uses a
  distinct open-file description for the same terminal, preserves all three
  standard-stream status flags, sets `O_NONBLOCK`/`FD_CLOEXEC`, does not acquire
  a controlling terminal, and closes on drop in both libc and non-libc lanes;
  product-level PTY tests separately exercise terminal shutdown;
- remove stale platform compile-error cfg checks and apply warning/style-only
  compatibility adjustments required by the current Rust toolchain;
- align the standalone patched crate's declared minimum Rust version with the
  product workspace's explicit Rust 1.92 baseline; the product's locked
  dependency graph is compiled by Rust 1.92 CI instead of retaining Crossterm
  0.28.1's unverified upstream 1.63 declaration;
- add local workspace metadata so the vendored crate can be tested
  independently inside this repository.

The upstream copyright and MIT license are unchanged. CrabCode's changes are
not represented as an upstream Crossterm release.

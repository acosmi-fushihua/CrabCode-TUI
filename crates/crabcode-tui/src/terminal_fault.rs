//! Fatal process-fault restoration for the process-owned terminal.
//!
//! This module intentionally does one job:
//!
//! - Unix: restore the terminal after `SIGBUS` or `SIGSEGV`, restore the
//!   default disposition, and re-raise the same signal.
//! - Windows: restore terminal protocol bytes for the fixed set of fatal SEH
//!   exceptions and return `EXCEPTION_CONTINUE_SEARCH`.
//!
//! It does not collect diagnostics, walk stacks, write crash files, or consult
//! runtime/backend configuration.

// Fatal signal/SEH restoration cannot be expressed through safe Rust: the
// handler must use async-signal-safe libc calls and process-lifetime storage.
// Keep the exception at this dedicated fault-restoration boundary; individual
// unsafe sites below document their exact invariants.
#![allow(unsafe_code)]

#[cfg(unix)]
mod imp {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

    const ALT_STACK_SIZE: usize = 16 * 1024;
    const FATAL_MEMORY_SIGNALS: [libc::c_int; 2] = [libc::SIGBUS, libc::SIGSEGV];

    static ALT_STACK_INSTALLED: AtomicBool = AtomicBool::new(false);
    static BASELINE_READY: AtomicBool = AtomicBool::new(false);
    static BASELINE_FD: AtomicI32 = AtomicI32::new(-1);
    static OUTPUT_FD: AtomicI32 = AtomicI32::new(libc::STDOUT_FILENO);

    // Written once before the handlers are registered and never mutated
    // afterwards. The ready publication is the only way the handler can read
    // this storage.
    static mut ORIGINAL_TERMIOS: libc::termios = unsafe { std::mem::zeroed() };

    pub(super) fn install() {
        capture_terminal_baseline();
        setup_alt_stack();
        unsafe {
            register_fatal_memory_signals();
        }
    }

    fn capture_terminal_baseline() {
        let mut termios = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: termios is writable storage and fd 0 has already been routed
        // to the interactive tty before this process-lifetime installation.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, termios.as_mut_ptr()) } == 0 {
            // Keep a stable descriptor for the same terminal even if inherited
            // fd 0 is later replaced by process-local code.
            let baseline_fd = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
            if baseline_fd >= 0 {
                // SAFETY: tcgetattr initialized termios and no handler is
                // registered yet, so publication cannot race this one write.
                unsafe {
                    std::ptr::addr_of_mut!(ORIGINAL_TERMIOS).write(termios.assume_init());
                }
                BASELINE_FD.store(baseline_fd, Ordering::Relaxed);
                BASELINE_READY.store(true, Ordering::Release);
            }
        }

        // CrabCode's historical direct TUI owns stdout. Preserve that product
        // route while keeping a descriptor immune to later fd replacement.
        let output_fd = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 0) };
        if output_fd >= 0 {
            OUTPUT_FD.store(output_fd, Ordering::Relaxed);
        }
    }

    fn setup_alt_stack() {
        if ALT_STACK_INSTALLED.swap(true, Ordering::AcqRel) {
            return;
        }
        // SAFETY: this process-lifetime mapping is intentionally retained until
        // exit so a stack-overflow SIGSEGV can run on independent storage.
        unsafe {
            let stack = libc::mmap(
                std::ptr::null_mut(),
                ALT_STACK_SIZE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if stack != libc::MAP_FAILED {
                let signal_stack = libc::stack_t {
                    ss_sp: stack,
                    ss_flags: 0,
                    ss_size: ALT_STACK_SIZE,
                };
                let _ = libc::sigaltstack(&signal_stack, std::ptr::null_mut());
            }
        }
    }

    unsafe fn register_fatal_memory_signals() {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = fatal_memory_signal_handler as *const () as usize;
            action.sa_flags = handler_flags();
            libc::sigemptyset(&mut action.sa_mask);
            for signal in FATAL_MEMORY_SIGNALS {
                let _ = libc::sigaction(signal, &action, std::ptr::null_mut());
            }
        }
    }

    const fn handler_flags() -> libc::c_int {
        libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESETHAND
    }

    unsafe extern "C" fn fatal_memory_signal_handler(
        signal: libc::c_int,
        _info: *mut libc::siginfo_t,
        _context: *mut libc::c_void,
    ) {
        unsafe {
            // The active/inactive seam is atomic-only. No Rust lock, allocator,
            // formatter, writer, runtime traversal, or destructor is reachable
            // from this signal context.
            if crate::terminal::fatal_fault_protocol_restore_required() {
                let output_fd = OUTPUT_FD.load(Ordering::Relaxed);
                let bytes = crate::terminal::FATAL_FAULT_TERMINAL_RESTORE;
                let _ = libc::write(output_fd, bytes.as_ptr().cast(), bytes.len());
            }

            if BASELINE_READY.load(Ordering::Acquire) {
                let baseline_fd = BASELINE_FD.load(Ordering::Relaxed);
                let _ = libc::tcsetattr(
                    baseline_fd,
                    libc::TCSANOW,
                    std::ptr::addr_of!(ORIGINAL_TERMIOS),
                );
            }

            // Preserve the operating-system crash status and core-dump policy.
            let mut default_action: libc::sigaction = std::mem::zeroed();
            default_action.sa_sigaction = libc::SIG_DFL;
            default_action.sa_flags = 0;
            libc::sigemptyset(&mut default_action.sa_mask);
            let _ = libc::sigaction(signal, &default_action, std::ptr::null_mut());
            let _ = libc::raise(signal);
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn fatal_signal_set_and_handler_flags_match_the_fixed_terminal_contract() {
            assert_eq!(FATAL_MEMORY_SIGNALS, [libc::SIGBUS, libc::SIGSEGV]);
            let flags = handler_flags();
            assert_ne!(flags & libc::SA_SIGINFO, 0);
            assert_ne!(flags & libc::SA_ONSTACK, 0);
            assert_ne!(flags & libc::SA_RESETHAND, 0);
        }

        #[test]
        fn signal_handler_has_no_diagnostic_or_backend_surface() {
            let source = include_str!("terminal_fault.rs")
                .split("    #[cfg(test)]")
                .next()
                .expect("production Unix fatal-fault source");
            for forbidden in [
                "CrashHandlerConfig",
                "check_previous_crash",
                "last-crash.bin",
                "symbolicate",
                "backtrace::",
                "appServer",
            ] {
                assert!(
                    !source.contains(forbidden),
                    "terminal-only fatal restoration must not contain {forbidden}"
                );
            }
        }

        #[test]
        fn fatal_restore_precedes_termios_and_same_signal_reraise() {
            let source = include_str!("terminal_fault.rs");
            let handler = source
                .split("unsafe extern \"C\" fn fatal_memory_signal_handler")
                .nth(1)
                .expect("fatal handler source");
            let write = handler.find("libc::write").expect("terminal restore write");
            let termios = handler.find("libc::tcsetattr").expect("termios restore");
            let default = handler
                .find("libc::sigaction(signal")
                .expect("default disposition restore");
            let reraise = handler
                .find("libc::raise(signal)")
                .expect("same-signal raise");
            assert!(write < termios && termios < default && default < reraise);
        }

        #[test]
        fn installation_precedes_runtime_and_direct_backend_spawn() {
            let source = include_str!("lib.rs");
            let stdin = source
                .find("terminal::prepare_interactive_stdin(initial_prompt)")
                .expect("interactive stdin route");
            let install = source
                .find("terminal_fault::install()")
                .expect("fatal restore install");
            let runtime = source
                .find("tokio::runtime::Builder::new_current_thread()")
                .expect("event runtime construction");
            let backend = source
                .find("RuntimeHost::spawn_uninitialized_in")
                .expect("direct backend spawn");
            assert!(stdin < install && install < runtime && runtime < backend);
        }
    }
}

#[cfg(any(windows, test))]
const EXCEPTION_ACCESS_VIOLATION: i32 = 0xC000_0005_u32 as i32;
#[cfg(any(windows, test))]
const EXCEPTION_STACK_OVERFLOW: i32 = 0xC000_00FD_u32 as i32;
#[cfg(any(windows, test))]
const EXCEPTION_IN_PAGE_ERROR: i32 = 0xC000_0006_u32 as i32;
#[cfg(any(windows, test))]
const EXCEPTION_ILLEGAL_INSTRUCTION: i32 = 0xC000_001D_u32 as i32;
#[cfg(any(windows, test))]
const EXCEPTION_ARRAY_BOUNDS_EXCEEDED: i32 = 0xC000_008C_u32 as i32;

#[cfg(any(windows, test))]
const WINDOWS_FATAL_EXCEPTION_CODES: [i32; 5] = [
    EXCEPTION_ACCESS_VIOLATION,
    EXCEPTION_STACK_OVERFLOW,
    EXCEPTION_IN_PAGE_ERROR,
    EXCEPTION_ILLEGAL_INSTRUCTION,
    EXCEPTION_ARRAY_BOUNDS_EXCEEDED,
];

#[cfg(any(windows, test))]
fn is_windows_fatal_exception(code: i32) -> bool {
    WINDOWS_FATAL_EXCEPTION_CODES.contains(&code)
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::System::Diagnostics::Debug::{
        EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
    };

    const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

    pub(super) fn install() {
        set_filter(terminal_restore_filter_basic);
    }

    pub(super) fn set_protocol_restore_enabled(enabled: bool) {
        set_filter(if enabled {
            terminal_restore_filter
        } else {
            terminal_restore_filter_basic
        });
    }

    fn set_filter(filter: unsafe extern "system" fn(*const EXCEPTION_POINTERS) -> i32) {
        // SAFETY: both filters have the exact Win32 top-level exception-filter
        // ABI and live for the process lifetime.
        unsafe {
            SetUnhandledExceptionFilter(Some(filter));
        }
    }

    /// Inactive TUI filter: preserve the process's ordinary SEH search.
    unsafe extern "system" fn terminal_restore_filter_basic(
        _info: *const EXCEPTION_POINTERS,
    ) -> i32 {
        EXCEPTION_CONTINUE_SEARCH
    }

    /// Active TUI filter: restore only for the fixed fatal exception set and
    /// preserve Windows' ordinary unhandled-exception search/termination.
    unsafe extern "system" fn terminal_restore_filter(info: *const EXCEPTION_POINTERS) -> i32 {
        unsafe {
            if info.is_null() {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            let exception_record = (*info).ExceptionRecord;
            if exception_record.is_null() {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            if !super::is_windows_fatal_exception((*exception_record).ExceptionCode) {
                return EXCEPTION_CONTINUE_SEARCH;
            }
            crate::terminal::write_fatal_fault_terminal_restore();
            EXCEPTION_CONTINUE_SEARCH
        }
    }
}

/// Install the terminal-only fatal-fault lifecycle before any runtime or
/// worker thread exists.
pub(crate) fn install() {
    #[cfg(unix)]
    imp::install();
    #[cfg(windows)]
    windows::install();
}

/// Switch the Windows top-level exception filter at the same published
/// boundary that enables or disables TUI terminal protocol ownership.
#[cfg(windows)]
pub(crate) fn set_protocol_restore_enabled(enabled: bool) {
    windows::set_protocol_restore_enabled(enabled);
}

#[cfg(test)]
mod contract_tests {
    fn position_in_fatal_restore(needle: &[u8]) -> usize {
        crate::terminal::FATAL_FAULT_TERMINAL_RESTORE
            .windows(needle.len())
            .position(|window| window == needle)
            .unwrap_or_else(|| {
                panic!(
                    "fatal restore must contain {:?}",
                    String::from_utf8_lossy(needle)
                )
            })
    }

    fn windows_production_source() -> &'static str {
        include_str!("terminal_fault.rs")
            .split("#[cfg(windows)]\nmod windows {")
            .nth(1)
            .expect("Windows fatal-fault module")
            .split("#[cfg(test)]\nmod contract_tests")
            .next()
            .expect("production Windows fatal-fault source")
    }

    #[test]
    fn windows_exception_set_is_exact_and_search_continues() {
        assert_eq!(
            super::WINDOWS_FATAL_EXCEPTION_CODES,
            [
                0xC000_0005_u32 as i32,
                0xC000_00FD_u32 as i32,
                0xC000_0006_u32 as i32,
                0xC000_001D_u32 as i32,
                0xC000_008C_u32 as i32,
            ]
        );
        for code in super::WINDOWS_FATAL_EXCEPTION_CODES {
            assert!(super::is_windows_fatal_exception(code));
        }
        assert!(!super::is_windows_fatal_exception(0xE06D_7363_u32 as i32));

        let production = windows_production_source();
        let filter = production
            .split("unsafe extern \"system\" fn terminal_restore_filter(")
            .nth(1)
            .expect("active Windows exception filter");
        assert!(filter.contains("crate::terminal::write_fatal_fault_terminal_restore()"));
        assert!(filter.contains("EXCEPTION_CONTINUE_SEARCH"));
        assert!(production.contains("SetUnhandledExceptionFilter"));
        assert!(production.contains("Some(filter)"));
    }

    #[test]
    fn fatal_restore_sequence_covers_every_fixed_terminal_protocol_mode() {
        for needle in [
            b"\x1b[?2026l".as_slice(),
            b"\x1b[?25h".as_slice(),
            b"\x1b[?1000l".as_slice(),
            b"\x1b[?1002l".as_slice(),
            b"\x1b[?1003l".as_slice(),
            b"\x1b[?1015l".as_slice(),
            b"\x1b[?1006l".as_slice(),
            b"\x1b[?2004l".as_slice(),
            b"\x1b[?1004l".as_slice(),
            b"\x1b[<u".as_slice(),
            b"\x1b[?1049l".as_slice(),
        ] {
            position_in_fatal_restore(needle);
        }
    }

    #[test]
    fn fatal_restore_sequence_ends_synchronized_update_first() {
        assert!(crate::terminal::FATAL_FAULT_TERMINAL_RESTORE.starts_with(b"\x1b[?2026l"));
    }

    #[test]
    fn fatal_restore_sequence_pops_keyboard_before_leaving_alternate_screen() {
        assert!(position_in_fatal_restore(b"\x1b[<u") < position_in_fatal_restore(b"\x1b[?1049l"));
    }

    #[test]
    fn production_fault_module_has_no_diagnostic_or_backend_surface() {
        let source = include_str!("terminal_fault.rs");
        let unix_production = source
            .split("    #[cfg(test)]")
            .next()
            .expect("production Unix fatal-fault source");
        let windows_production = windows_production_source();
        for forbidden in [
            "CrashHandlerConfig",
            "check_previous_crash",
            "last-crash.bin",
            "symbolicate",
            "backtrace::",
            "appServer",
        ] {
            assert!(
                !unix_production.contains(forbidden) && !windows_production.contains(forbidden),
                "terminal-only fatal restoration must not contain {forbidden}"
            );
        }
    }

    #[test]
    fn windows_filter_switches_with_the_existing_terminal_protocol_phase() {
        let terminal = include_str!("terminal.rs");
        let publish = terminal
            .split("    fn publish(self) {")
            .nth(1)
            .expect("terminal phase publication")
            .split("\n    }\n")
            .next()
            .expect("terminal phase publication body");
        let switch = publish
            .find("crate::terminal_fault::set_protocol_restore_enabled")
            .expect("Windows active/basic filter switch");
        let active = publish
            .find("self == EmergencyTerminalPhase::ProtocolActive")
            .expect("protocol-active discriminator");
        let publication = publish
            .find("EMERGENCY_TERMINAL_PHASE.store")
            .expect("atomic terminal phase publication");
        assert!(switch < active && active < publication);
    }
}

#![allow(unsafe_code)]

//! Unix daemon spawn: double-fork, `setsid`, close inherited descriptors, and
//! `execve` with an exact argv vector.
//!
//! The caller must not invoke this from a multi-threaded Tokio runtime. The
//! child side deliberately performs only async-signal-safe libc calls; all
//! strings and pointer arrays are prepared before `fork`.

use std::collections::BTreeMap;
use std::ffi::{CString, OsString};
use std::fs::OpenOptions;
use std::os::fd::IntoRawFd as _;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

use crate::{LauncherError, Result};

pub fn daemonize_command(
    binary: &Path,
    args: &[OsString],
    env_overrides: &[(OsString, OsString)],
    log_file: Option<&Path>,
) -> Result<()> {
    let bin_c = CString::new(binary.as_os_str().as_bytes()).map_err(|error| {
        LauncherError::SpawnFailed(format!("binary path contains NUL: {error}"))
    })?;
    let argv0 = CString::new(binary.file_name().unwrap_or(binary.as_os_str()).as_bytes())
        .map_err(|error| LauncherError::SpawnFailed(format!("argv0 contains NUL: {error}")))?;

    let mut argv_cstrings = Vec::with_capacity(args.len() + 1);
    argv_cstrings.push(argv0);
    for arg in args {
        argv_cstrings.push(CString::new(arg.as_os_str().as_bytes()).map_err(|error| {
            LauncherError::SpawnFailed(format!("argument contains NUL: {error}"))
        })?);
    }
    let argv: Vec<*const libc::c_char> = argv_cstrings
        .iter()
        .map(|arg| arg.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    let mut environment = std::env::vars_os().collect::<BTreeMap<_, _>>();
    for (key, value) in env_overrides {
        environment.insert(key.clone(), value.clone());
    }
    let env_cstrings: std::result::Result<Vec<CString>, _> = environment
        .into_iter()
        .map(|(key, value)| {
            let mut entry = key;
            entry.push("=");
            entry.push(value);
            CString::new(entry.as_os_str().as_bytes())
        })
        .collect();
    let env_cstrings = env_cstrings.map_err(|error| {
        LauncherError::SpawnFailed(format!("environment contains NUL: {error}"))
    })?;
    let envp: Vec<*const libc::c_char> = env_cstrings
        .iter()
        .map(|entry| entry.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    // Resolve the descriptor ceiling before fork. The child may only call
    // async-signal-safe libc operations, and closing a fixed `3..256` range
    // would leak any higher-numbered client descriptors into the daemon.
    let max_fd = open_file_limit();
    let null_fd = unsafe { libc::open(b"/dev/null\0".as_ptr().cast(), libc::O_RDWR) };
    if null_fd < 0 {
        return Err(LauncherError::Io(std::io::Error::last_os_error()));
    }

    let log_fd = if let Some(log_file) = log_file {
        if let Some(parent) = log_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(log_file)
        {
            Ok(file) => {
                use std::os::unix::fs::PermissionsExt as _;

                std::fs::set_permissions(log_file, std::fs::Permissions::from_mode(0o600))?;
                file.into_raw_fd()
            }
            Err(error) => {
                unsafe { libc::close(null_fd) };
                return Err(LauncherError::Io(error));
            }
        }
    } else {
        null_fd
    };

    match unsafe { libc::fork() } {
        -1 => {
            close_parent_fds(null_fd, log_fd);
            Err(LauncherError::Io(std::io::Error::last_os_error()))
        }
        0 => unsafe {
            if libc::setsid() < 0 {
                libc::_exit(127);
            }
            match libc::fork() {
                -1 => libc::_exit(127),
                0 => {
                    libc::dup2(null_fd, libc::STDIN_FILENO);
                    libc::dup2(log_fd, libc::STDOUT_FILENO);
                    libc::dup2(log_fd, libc::STDERR_FILENO);
                    if null_fd > libc::STDERR_FILENO {
                        libc::close(null_fd);
                    }
                    if log_fd > libc::STDERR_FILENO && log_fd != null_fd {
                        libc::close(log_fd);
                    }
                    close_inherited_fds(max_fd);
                    crate::silent_drop!(
                        libc::chdir(b"/\0".as_ptr().cast()),
                        "post-fork cwd is diagnostic only; execve remains authoritative"
                    );
                    libc::execve(bin_c.as_ptr(), argv.as_ptr(), envp.as_ptr());
                    libc::_exit(127);
                }
                _ => libc::_exit(0),
            }
        },
        child_pid => {
            close_parent_fds(null_fd, log_fd);
            let mut status = 0;
            // W-CRON-LIVENESS-PARITY: this waitpid must reap the intermediate
            // child. The grandchild daemon is adopted by init; retaining this
            // reap is what prevents a scheduler-lock-owning Unix zombie.
            let rc = unsafe { libc::waitpid(child_pid, &mut status, 0) };
            if rc < 0 {
                tracing::warn!(
                    error = %std::io::Error::last_os_error(),
                    "waitpid for detached daemon intermediate child failed"
                );
            }
            Ok(())
        }
    }
}

fn open_file_limit() -> libc::c_int {
    let mut limit = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    let rc = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, limit.as_mut_ptr()) };
    if rc != 0 {
        return 1024;
    }
    let limit = unsafe { limit.assume_init() }.rlim_cur;
    if limit == libc::RLIM_INFINITY {
        return 65_536;
    }
    libc::c_int::try_from(limit).unwrap_or(libc::c_int::MAX)
}

/// Close every non-stdio descriptor.
///
/// Linux uses `close_range`; macOS and other targets use the portable
/// `close(2)` loop. All paths remain async-signal-safe after `fork`.
unsafe fn close_inherited_fds(max_fd: libc::c_int) {
    #[cfg(target_os = "macos")]
    {
        for fd in 3..max_fd {
            unsafe { libc::close(fd) };
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let rc = unsafe { libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32) };
        if rc == 0 {
            return;
        }
        for fd in 3..max_fd {
            unsafe { libc::close(fd) };
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
    {
        for fd in 3..max_fd {
            unsafe { libc::close(fd) };
        }
    }
}

fn close_parent_fds(null_fd: libc::c_int, log_fd: libc::c_int) {
    unsafe {
        libc::close(null_fd);
        if log_fd != null_fd {
            libc::close(log_fd);
        }
    }
}

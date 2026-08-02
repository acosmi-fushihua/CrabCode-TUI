use std::{
    ffi::OsStr,
    fs::File,
    io,
    os::unix::{ffi::OsStrExt as _, fs::OpenOptionsExt as _},
    path::{Path, PathBuf},
};

#[cfg(feature = "libc")]
use libc::size_t;
#[cfg(not(feature = "libc"))]
use rustix::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
#[cfg(feature = "libc")]
use rustix::fd::{AsFd, BorrowedFd};
#[cfg(feature = "libc")]
use std::{
    fs,
    marker::PhantomData,
    os::unix::{
        io::{IntoRawFd, RawFd},
        prelude::AsRawFd,
    },
};

/// A file descriptor wrapper.
///
/// It allows to retrieve raw file descriptor, write to the file descriptor and
/// mainly it closes the file descriptor once dropped.
#[derive(Debug)]
#[cfg(feature = "libc")]
pub struct FileDesc<'a> {
    fd: RawFd,
    close_on_drop: bool,
    phantom: PhantomData<&'a ()>,
}

#[cfg(not(feature = "libc"))]
pub enum FileDesc<'a> {
    Owned(OwnedFd),
    Borrowed(BorrowedFd<'a>),
}

#[cfg(feature = "libc")]
impl FileDesc<'_> {
    /// Constructs a new `FileDesc` with the given `RawFd`.
    ///
    /// # Arguments
    ///
    /// * `fd` - raw file descriptor
    /// * `close_on_drop` - specify if the raw file descriptor should be closed once the `FileDesc` is dropped
    pub fn new(fd: RawFd, close_on_drop: bool) -> FileDesc<'static> {
        FileDesc {
            fd,
            close_on_drop,
            phantom: PhantomData,
        }
    }

    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let result = unsafe {
            libc::read(
                self.fd,
                buffer.as_mut_ptr() as *mut libc::c_void,
                buffer.len() as size_t,
            )
        };

        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result as usize)
        }
    }

    /// Returns the underlying file descriptor.
    pub fn raw_fd(&self) -> RawFd {
        self.fd
    }
}

#[cfg(not(feature = "libc"))]
impl FileDesc<'_> {
    pub fn read(&self, buffer: &mut [u8]) -> io::Result<usize> {
        let fd = match self {
            FileDesc::Owned(fd) => fd.as_fd(),
            FileDesc::Borrowed(fd) => fd.as_fd(),
        };
        let result = rustix::io::read(fd, buffer)?;
        Ok(result)
    }

    pub fn raw_fd(&self) -> RawFd {
        match self {
            FileDesc::Owned(fd) => fd.as_raw_fd(),
            FileDesc::Borrowed(fd) => fd.as_raw_fd(),
        }
    }
}

#[cfg(feature = "libc")]
impl Drop for FileDesc<'_> {
    fn drop(&mut self) {
        if self.close_on_drop {
            // Note that errors are ignored when closing a file descriptor. The
            // reason for this is that if an error occurs we don't actually know if
            // the file descriptor was closed or not, and if we retried (for
            // something like EINTR), we might close another valid file descriptor
            // opened after we closed ours.
            let _ = unsafe { libc::close(self.fd) };
        }
    }
}

impl AsRawFd for FileDesc<'_> {
    fn as_raw_fd(&self) -> RawFd {
        self.raw_fd()
    }
}

#[cfg(not(feature = "libc"))]
impl AsFd for FileDesc<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        match self {
            FileDesc::Owned(fd) => fd.as_fd(),
            FileDesc::Borrowed(fd) => fd.as_fd(),
        }
    }
}

#[cfg(feature = "libc")]
impl AsFd for FileDesc<'_> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: `FileDesc` owns or borrows `fd` for at least `self`'s
        // lifetime, and this does not transfer or close the descriptor.
        unsafe { BorrowedFd::borrow_raw(self.fd) }
    }
}

#[cfg(feature = "libc")]
/// Creates a file descriptor pointing to the standard input or `/dev/tty`.
pub fn tty_fd() -> io::Result<FileDesc<'static>> {
    let (fd, close_on_drop) = if unsafe { libc::isatty(libc::STDIN_FILENO) == 1 } {
        (libc::STDIN_FILENO, false)
    } else {
        (
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")?
                .into_raw_fd(),
            true,
        )
    };

    Ok(FileDesc::new(fd, close_on_drop))
}

#[cfg(not(feature = "libc"))]
/// Creates a file descriptor pointing to the standard input or `/dev/tty`.
pub fn tty_fd() -> io::Result<FileDesc<'static>> {
    let stdin = rustix::stdio::stdin();
    let fd = if rustix::termios::isatty(stdin) {
        FileDesc::Borrowed(stdin)
    } else {
        let dev_tty = File::options().read(true).write(true).open("/dev/tty")?;
        FileDesc::Owned(dev_tty.into())
    };
    Ok(fd)
}

fn open_nonblocking_terminal(path: &Path) -> io::Result<File> {
    let file = File::options()
        .read(true)
        .custom_flags(
            (rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::NOCTTY)
                .bits() as i32,
        )
        .open(path)?;
    if !rustix::termios::isatty(&file) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "terminal event input path is not a terminal: {}",
                path.display()
            ),
        ));
    }
    Ok(file)
}

/// Open a terminal input descriptor with its own nonblocking open-file
/// description.
///
/// `login_tty`/`forkpty` commonly make stdin, stdout, and stderr aliases of one
/// open-file description. Mutating `O_NONBLOCK` on borrowed stdin would then
/// affect concurrent terminal output. Resolve stdin's kernel-reported terminal
/// path and open that device again so event reads can remain nonblocking
/// without changing any standard-stream flags. If stdin is redirected, use the
/// controlling terminal as Crossterm did previously.
pub fn terminal_input_fd() -> io::Result<FileDesc<'static>> {
    let stdin = rustix::stdio::stdin();
    let stdin_is_tty = rustix::termios::isatty(stdin);
    let tty_name_result = if stdin_is_tty {
        rustix::termios::ttyname(stdin, Vec::new())
            .map(|name| PathBuf::from(OsStr::from_bytes(name.to_bytes())))
    } else {
        Ok(PathBuf::from("/dev/tty"))
    };

    let file = match tty_name_result {
        Ok(path) => match open_nonblocking_terminal(&path) {
            Ok(file) => file,
            Err(primary_error) if path != Path::new("/dev/tty") => {
                open_nonblocking_terminal(Path::new("/dev/tty")).map_err(
                    |fallback_error| {
                        io::Error::new(
                            fallback_error.kind(),
                            format!(
                                "cannot open independent terminal input at {} ({primary_error}) or /dev/tty ({fallback_error})",
                                path.display()
                            ),
                        )
                    },
                )?
            }
            Err(error) => return Err(error),
        },
        Err(name_error) => open_nonblocking_terminal(Path::new("/dev/tty")).map_err(
            |fallback_error| {
                io::Error::new(
                    fallback_error.kind(),
                    format!(
                        "cannot resolve stdin terminal path ({name_error}) or open /dev/tty ({fallback_error})"
                    ),
                )
            },
        )?,
    };

    if stdin_is_tty {
        let stdin_stat = rustix::fs::fstat(stdin)?;
        let input_stat = rustix::fs::fstat(&file)?;
        if stdin_stat.st_rdev != input_stat.st_rdev {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "independent terminal input does not identify the stdin terminal device",
            ));
        }
    }

    #[cfg(feature = "libc")]
    {
        Ok(FileDesc::new(file.into_raw_fd(), true))
    }
    #[cfg(not(feature = "libc"))]
    {
        Ok(FileDesc::Owned(file.into()))
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use super::*;
    use std::{
        env,
        io::Read as _,
        os::unix::process::CommandExt as _,
        process::{Command, Stdio},
    };

    use rustix::{
        fd::BorrowedFd,
        fs::{fcntl_getfl, fcntl_setfl, OFlags},
        io::{fcntl_getfd, Errno, FdFlags},
        process::{getpgrp, getpid, getsid, setsid},
        pty::{grantpt, openpt, ptsname, unlockpt, OpenptFlags},
        stdio::{stderr, stdin, stdout},
        termios::{isatty, tcgetsid},
    };

    const TEST_STAGE_ENV: &str = "CROSSTERM_TERMINAL_INPUT_FD_TEST_STAGE";
    const LAUNCHER_STAGE: &str = "session-launcher";
    const ASSERTION_STAGE: &str = "descriptor-assertions";
    const TEST_NAME: &str =
        "terminal::sys::file_descriptor::tests::terminal_input_fd_has_an_independent_nonblocking_close_on_exec_ofd";

    fn assert_no_controlling_terminal(context: &str) {
        let error = tcgetsid(stdin()).expect_err(context);
        assert_eq!(
            error,
            Errno::NOTTY,
            "{context}: the PTY unexpectedly became this session's controlling terminal"
        );
        assert!(
            File::options()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .is_err(),
            "{context}: /dev/tty unexpectedly resolved a controlling terminal"
        );
    }

    fn run_descriptor_assertions() {
        assert_eq!(
            getsid(None).expect("read child session id"),
            getpid(),
            "descriptor assertions must run as a session leader"
        );
        assert!(isatty(stdin()));
        assert!(isatty(stdout()));
        assert!(isatty(stderr()));
        assert_no_controlling_terminal("before terminal_input_fd");

        let stdin_flags_before = fcntl_getfl(stdin()).expect("read stdin status flags");
        let stdout_flags_before = fcntl_getfl(stdout()).expect("read stdout status flags");
        let stderr_flags_before = fcntl_getfl(stderr()).expect("read stderr status flags");
        assert!(
            !stdin_flags_before.contains(OFlags::NONBLOCK)
                && !stdout_flags_before.contains(OFlags::NONBLOCK)
                && !stderr_flags_before.contains(OFlags::NONBLOCK),
            "PTY standard streams must begin in blocking mode"
        );

        let input = terminal_input_fd().expect("open independent terminal input descriptor");
        let input_raw_fd = input.raw_fd();
        assert!(
            input_raw_fd > stderr().as_raw_fd(),
            "terminal input must own a descriptor distinct from stdin/stdout/stderr"
        );

        let input_flags = fcntl_getfl(&input).expect("read terminal input status flags");
        assert!(
            input_flags.contains(OFlags::NONBLOCK),
            "terminal input descriptor must be nonblocking"
        );
        assert!(
            fcntl_getfd(&input)
                .expect("read terminal input descriptor flags")
                .contains(FdFlags::CLOEXEC),
            "terminal input descriptor must be close-on-exec"
        );
        assert_eq!(
            rustix::fs::fstat(stdin())
                .expect("stat stdin terminal")
                .st_rdev,
            rustix::fs::fstat(&input)
                .expect("stat independent terminal input")
                .st_rdev,
            "independent terminal input must identify the stdin terminal device"
        );

        assert_eq!(
            fcntl_getfl(stdin()).expect("read stdin flags after terminal input open"),
            stdin_flags_before,
            "opening terminal input must not mutate stdin's open-file description"
        );
        assert_eq!(
            fcntl_getfl(stdout()).expect("read stdout flags after terminal input open"),
            stdout_flags_before,
            "opening terminal input must not mutate stdout's open-file description"
        );
        assert_eq!(
            fcntl_getfl(stderr()).expect("read stderr flags after terminal input open"),
            stderr_flags_before,
            "opening terminal input must not mutate stderr's open-file description"
        );
        assert_no_controlling_terminal("after terminal_input_fd");

        fcntl_setfl(&input, input_flags - OFlags::NONBLOCK)
            .expect("clear nonblocking on independent terminal input");
        assert!(!fcntl_getfl(&input)
            .expect("read terminal input flags after clearing nonblocking")
            .contains(OFlags::NONBLOCK));
        assert_eq!(
            fcntl_getfl(stdin()).expect("read stdin flags while terminal input flags differ"),
            stdin_flags_before,
            "changing terminal input status flags must not mutate stdin"
        );
        assert_eq!(
            fcntl_getfl(stdout()).expect("read stdout flags while terminal input flags differ"),
            stdout_flags_before,
            "changing terminal input status flags must not mutate stdout"
        );
        assert_eq!(
            fcntl_getfl(stderr()).expect("read stderr flags while terminal input flags differ"),
            stderr_flags_before,
            "changing terminal input status flags must not mutate stderr"
        );
        fcntl_setfl(&input, input_flags).expect("restore nonblocking terminal input flags");
        assert!(fcntl_getfl(&input)
            .expect("read restored terminal input flags")
            .contains(OFlags::NONBLOCK));

        drop(input);
        let closed = unsafe { BorrowedFd::borrow_raw(input_raw_fd) };
        assert_eq!(
            fcntl_getfd(closed).expect_err("dropped terminal input descriptor must be closed"),
            Errno::BADF
        );
        assert_no_controlling_terminal("after terminal input drop");
    }

    fn run_session_launcher() -> ! {
        assert_ne!(
            getpid(),
            getpgrp(),
            "spawned launcher must not already be a process-group leader"
        );
        let session_id = setsid().expect("create isolated test session");
        assert_eq!(session_id, getpid());

        let executable = env::current_exe().expect("resolve current test executable");
        let error = Command::new(executable)
            .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
            .env(TEST_STAGE_ENV, ASSERTION_STAGE)
            .exec();
        panic!("exec descriptor assertion stage: {error}");
    }

    fn open_pty() -> (File, File) {
        let master =
            openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open pseudoterminal master");
        grantpt(&master).expect("grant pseudoterminal slave");
        unlockpt(&master).expect("unlock pseudoterminal slave");
        let slave_name = ptsname(&master, Vec::new()).expect("resolve pseudoterminal slave path");
        let slave_path = Path::new(OsStr::from_bytes(slave_name.to_bytes()));
        let slave = File::options()
            .read(true)
            .write(true)
            .custom_flags(OFlags::NOCTTY.bits() as i32)
            .open(slave_path)
            .expect("open pseudoterminal slave");
        (File::from(master), slave)
    }

    #[test]
    fn terminal_input_fd_has_an_independent_nonblocking_close_on_exec_ofd() {
        match env::var(TEST_STAGE_ENV).as_deref() {
            Ok(LAUNCHER_STAGE) => run_session_launcher(),
            Ok(ASSERTION_STAGE) => {
                run_descriptor_assertions();
                return;
            }
            Ok(other) => panic!("unexpected terminal input test stage: {other}"),
            Err(env::VarError::NotUnicode(_)) => panic!("terminal input test stage is not UTF-8"),
            Err(env::VarError::NotPresent) => {}
        }

        let (mut master, slave) = open_pty();
        let child_stdin = slave.try_clone().expect("clone PTY slave for child stdin");
        let child_stdout = slave.try_clone().expect("clone PTY slave for child stdout");
        let executable = env::current_exe().expect("resolve current test executable");
        let mut child = Command::new(executable)
            .args(["--exact", TEST_NAME, "--nocapture", "--test-threads=1"])
            .env(TEST_STAGE_ENV, LAUNCHER_STAGE)
            .stdin(Stdio::from(child_stdin))
            .stdout(Stdio::from(child_stdout))
            .stderr(Stdio::from(slave))
            .spawn()
            .expect("spawn terminal input session launcher");

        let mut output = Vec::new();
        loop {
            let mut buffer = [0; 1024];
            match master.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => output.extend_from_slice(&buffer[..read]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                // Linux PTY masters commonly report EIO after the final slave
                // closes; the child status remains the authoritative result.
                Err(_) => break,
            }
        }
        let status = child.wait().expect("wait for terminal input child");
        assert!(
            status.success(),
            "terminal input PTY child failed with {status}:\n{}",
            String::from_utf8_lossy(&output)
        );
    }
}

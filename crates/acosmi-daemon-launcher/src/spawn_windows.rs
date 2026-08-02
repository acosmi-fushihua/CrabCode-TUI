//! Windows daemon spawn with explicit `CREATE_BREAKAWAY_FROM_JOB`.
//!
//! There is no shell and no fallback without breakaway: silently inheriting a
//! client Job would make a shared daemon die with that client.

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::path::Path;

use crate::{LauncherError, Result};

pub fn spawn_command(
    binary: &Path,
    args: &[OsString],
    env_overrides: &[(OsString, OsString)],
    log_file: Option<&Path>,
) -> Result<()> {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

    let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB;
    spawn_with_flags(binary, args, env_overrides, log_file, flags).map_err(|error| {
        LauncherError::SpawnFailed(format!(
            "CreateProcess with required CREATE_BREAKAWAY_FROM_JOB: {error}"
        ))
    })
}

fn spawn_with_flags(
    binary: &Path,
    args: &[OsString],
    env_overrides: &[(OsString, OsString)],
    log_file: Option<&Path>,
    flags: u32,
) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt as _;
    use std::process::{Command, Stdio};

    let mut command = Command::new(binary);
    command
        .args(args)
        .envs(env_overrides.iter().cloned())
        .stdin(Stdio::null())
        .creation_flags(flags);

    if let Some(log_file) = log_file {
        if let Some(parent) = log_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file)?;
        let stderr = stdout.try_clone()?;
        command
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr));
    } else {
        command.stdout(Stdio::null()).stderr(Stdio::null());
    }

    // Do not clear the environment: state-root and runtime overrides are part
    // of the daemon launch contract.
    // Drop Child immediately after spawn. On Windows that closes the process
    // handle, so an exited detached daemon cannot be pinned as a zombie and
    // wedge the scheduler liveness/lock contract.
    command.spawn().map(|_child| ())
}

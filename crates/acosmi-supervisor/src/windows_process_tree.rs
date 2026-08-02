//! Race-free, surface-neutral Windows process-tree launcher.
//!
//! The public launcher privately dispatches `process-tree-exec -- <command...>`
//! to this module. On Windows the real child is created suspended and
//! atomically associated with a kill-on-close Job Object through
//! `PROC_THREAD_ATTRIBUTE_JOB_LIST`. A successful `CreateProcessW` therefore
//! returns an already-contained child; unsupported or incompatible Job policy
//! fails before child code can run.

use std::ffi::{OsStr, OsString};

#[cfg(any(windows, test))]
use std::path::Path;

pub const PROCESS_TREE_HELPER_ENV: &str = "CRABCODE_PROCESS_TREE_EXECUTABLE";
pub const PROCESS_TREE_EXEC_SUBCOMMAND: &str = "process-tree-exec";

/// Parse the launcher's exact private process-tree route.
///
/// Only a first argument equal to `process-tree-exec` selects the route. Once
/// selected, the delimiter and child command are mandatory; malformed private
/// invocations fail instead of falling through to the public TUI parser.
pub fn parse_process_tree_exec_args(args: &[OsString]) -> Result<Option<Vec<OsString>>, String> {
    if args.first().map(OsString::as_os_str) != Some(OsStr::new(PROCESS_TREE_EXEC_SUBCOMMAND)) {
        return Ok(None);
    }
    if args.get(1).map(OsString::as_os_str) != Some(OsStr::new("--")) {
        return Err("process-tree-exec requires the exact `--` delimiter".to_owned());
    }
    if args.len() == 2 {
        return Err("process-tree-exec requires a child command".to_owned());
    }
    Ok(Some(args[2..].to_vec()))
}

#[cfg(any(windows, test))]
fn is_windows_batch_extension(extension: &str) -> bool {
    extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
}

#[cfg(any(windows, test))]
fn is_node_modules_cmd_shim(path: &Path) -> bool {
    let normalized = path
        .to_string_lossy()
        .replace('\\', "/")
        .to_ascii_lowercase();
    normalized.ends_with(".cmd") && normalized.contains("/node_modules/.bin/")
}

#[cfg(any(windows, test))]
fn is_cmd_meta_character(unit: u16) -> bool {
    matches!(
        unit,
        0x28 | 0x29
            | 0x5b
            | 0x5d
            | 0x25
            | 0x21
            | 0x5e
            | 0x22
            | 0x60
            | 0x3c
            | 0x3e
            | 0x26
            | 0x7c
            | 0x3b
            | 0x2c
            | 0x20
            | 0x2a
            | 0x3f
    )
}

#[cfg(any(windows, test))]
fn validate_cmd_token(units: &[u16]) -> Result<(), String> {
    if units.contains(&0) {
        return Err("batch command or argument contains NUL".to_owned());
    }
    if units.iter().any(|unit| matches!(*unit, 0x0a | 0x0d)) {
        return Err("batch command or argument contains a line break".to_owned());
    }
    Ok(())
}

/// Escape a resolved batch program for cmd.exe's command position.
#[cfg(any(windows, test))]
fn append_cmd_escaped_command(output: &mut Vec<u16>, units: &[u16]) -> Result<(), String> {
    validate_cmd_token(units)?;
    for unit in units {
        if is_cmd_meta_character(*unit) {
            output.push(u16::from(b'^'));
        }
        output.push(*unit);
    }
    Ok(())
}

/// Escape one batch argument for a direct cmd.exe `/c` command.
#[cfg(any(windows, test))]
fn append_cmd_escaped_argument(
    output: &mut Vec<u16>,
    units: &[u16],
    double_escape_meta: bool,
) -> Result<(), String> {
    validate_cmd_token(units)?;
    let mut quoted = vec![u16::from(b'"')];
    let mut slashes = 0usize;
    for unit in units {
        if *unit == u16::from(b'\\') {
            slashes += 1;
            continue;
        }
        if *unit == u16::from(b'"') {
            quoted.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2 + 1));
            quoted.push(*unit);
        } else {
            quoted.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
            quoted.push(*unit);
        }
        slashes = 0;
    }
    // cmd.exe removes these outer quotes itself; trailing slashes must remain
    // literal rather than being doubled for a second CRT argv decoder.
    quoted.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
    quoted.push(u16::from(b'"'));

    let mut escaped = Vec::with_capacity(quoted.len());
    for unit in quoted {
        if is_cmd_meta_character(unit) {
            escaped.push(u16::from(b'^'));
        }
        escaped.push(unit);
    }
    // node_modules command shims invoke a second cmd.exe parse. Match the
    // existing cross-spawn-compatible narrow double-escape rule.
    if double_escape_meta {
        for unit in escaped {
            if is_cmd_meta_character(unit) {
                output.push(u16::from(b'^'));
            }
            output.push(unit);
        }
    } else {
        output.extend(escaped);
    }
    Ok(())
}

/// Append one argv token using the documented Windows CRT quoting grammar.
#[cfg(any(windows, test))]
fn append_windows_quoted_units(output: &mut Vec<u16>, units: &[u16]) -> Result<(), String> {
    if units.contains(&0) {
        return Err("process-tree child argv contains NUL".to_owned());
    }
    let needs_quotes =
        units.is_empty() || units.iter().any(|unit| matches!(*unit, 0x20 | 0x09 | 0x22));
    if !needs_quotes {
        output.extend_from_slice(units);
        return Ok(());
    }
    output.push(u16::from(b'"'));
    let mut slashes = 0usize;
    for unit in units {
        if *unit == u16::from(b'\\') {
            slashes += 1;
            continue;
        }
        if *unit == u16::from(b'"') {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2 + 1));
            output.push(*unit);
        } else {
            output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes));
            output.push(*unit);
        }
        slashes = 0;
    }
    output.extend(std::iter::repeat_n(u16::from(b'\\'), slashes * 2));
    output.push(u16::from(b'"'));
    Ok(())
}

#[cfg(any(windows, test))]
const fn ordinary_process_tree_job_limit_flags(kill_on_close: u32) -> u32 {
    // Ordinary direct-backend children must remain strictly contained. No
    // BREAKAWAY_OK or SILENT_BREAKAWAY_OK flag is admitted by this helper.
    kill_on_close
}

#[cfg(not(windows))]
pub fn run_process_tree_exec(_command: Vec<OsString>) -> Result<i32, String> {
    Err("process-tree-exec is only available on Windows".to_owned())
}

#[cfg(windows)]
pub fn run_process_tree_exec(command: Vec<OsString>) -> Result<i32, String> {
    windows::run(command)
}

#[cfg(windows)]
mod windows {
    #![allow(unsafe_code)]

    use std::ffi::{OsStr, OsString};
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};

    use windows::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0,
    };
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows::Win32::System::JobObjects::{
        CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject,
    };
    use windows::Win32::System::Threading::{
        CREATE_SUSPENDED, CreateProcessW, DeleteProcThreadAttributeList,
        EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess,
        InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
        ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
        UpdateProcThreadAttribute, WaitForSingleObject,
    };
    use windows::core::{PCWSTR, PWSTR};

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: this guard owns the handle and closes it once.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    struct OwnedAttributeList<'values> {
        _storage: Vec<usize>,
        list: LPPROC_THREAD_ATTRIBUTE_LIST,
        _values: std::marker::PhantomData<&'values [HANDLE]>,
    }

    impl Drop for OwnedAttributeList<'_> {
        fn drop(&mut self) {
            if !self.list.is_invalid() {
                // SAFETY: the initialized list lives inside `_storage`.
                unsafe {
                    DeleteProcThreadAttributeList(self.list);
                }
            }
        }
    }

    fn append_quoted_arg(output: &mut Vec<u16>, arg: &OsStr) -> Result<(), String> {
        super::append_windows_quoted_units(output, &arg.encode_wide().collect::<Vec<_>>())
    }

    fn command_line(command: &[OsString]) -> Result<Vec<u16>, String> {
        let mut output = Vec::new();
        for (index, arg) in command.iter().enumerate() {
            if index > 0 {
                output.push(u16::from(b' '));
            }
            append_quoted_arg(&mut output, arg)?;
        }
        output.push(0);
        if output.len() > 32_767 {
            return Err(
                "process-tree command line exceeds CreateProcessW's UTF-16 limit".to_owned(),
            );
        }
        Ok(output)
    }

    struct PreparedCommand {
        application: Vec<u16>,
        command_line: Vec<u16>,
    }

    fn executable_candidates(program: &Path) -> Vec<PathBuf> {
        if program.extension().is_some() {
            return vec![program.to_path_buf()];
        }
        let pathext =
            std::env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
        pathext
            .to_string_lossy()
            .split(';')
            .filter_map(|extension| {
                let extension = extension.trim();
                if extension.is_empty()
                    || !extension.starts_with('.')
                    || extension.contains(['/', '\\'])
                {
                    return None;
                }
                let mut candidate = program.as_os_str().to_os_string();
                candidate.push(extension);
                Some(PathBuf::from(candidate))
            })
            .collect()
    }

    fn resolve_program(program: &OsStr) -> Result<PathBuf, String> {
        if program.is_empty() {
            return Err("process-tree child program is empty".to_owned());
        }
        let program_path = PathBuf::from(program);
        let has_path_component = program_path.is_absolute()
            || program_path.components().count() > 1
            || program.to_string_lossy().contains(['/', '\\']);
        let roots = if has_path_component {
            vec![PathBuf::new()]
        } else {
            std::env::var_os("PATH")
                .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        for candidate in executable_candidates(&program_path) {
            for root in &roots {
                let path = if root.as_os_str().is_empty() {
                    candidate.clone()
                } else {
                    root.join(&candidate)
                };
                let Ok(metadata) = std::fs::metadata(&path) else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                return path
                    .canonicalize()
                    .map_err(|err| format!("cannot canonicalize child program: {err}"));
            }
        }
        Err(format!(
            "process-tree child program was not found through its explicit path or PATH/PATHEXT: {}",
            program.to_string_lossy()
        ))
    }

    fn trusted_comspec() -> Result<PathBuf, String> {
        use windows_sys::Win32::System::SystemInformation::GetSystemDirectoryW;

        let mut buffer = vec![0u16; 32_768];
        // SAFETY: buffer is writable for its reported capacity.
        let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
        if length == 0 || length as usize >= buffer.len() {
            return Err(format!(
                "GetSystemDirectoryW failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        buffer.truncate(length as usize);
        let system_directory = PathBuf::from(OsString::from_wide(&buffer));
        let comspec = system_directory.join("cmd.exe");
        let metadata = std::fs::metadata(&comspec)
            .map_err(|err| format!("trusted system cmd.exe is unavailable: {err}"))?;
        if !metadata.is_file() {
            return Err("trusted system cmd.exe is not a regular file".to_owned());
        }
        comspec
            .canonicalize()
            .map_err(|err| format!("cannot canonicalize trusted system cmd.exe: {err}"))
    }

    fn prepare_command(command: &[OsString]) -> Result<PreparedCommand, String> {
        let target = resolve_program(&command[0])?;
        let is_batch = target
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(super::is_windows_batch_extension);
        if is_batch {
            // canonicalize() yields a verbatim path; cmd.exe needs an ordinary
            // absolute DOS/UNC spelling for batch execution.
            let target =
                acosmi_util_absolute_path::AbsolutePathBuf::from_absolute_path_checked(&target)
                    .map_err(|err| format!("cannot normalize resolved batch program: {err}"))?
                    .into_path_buf();
            let comspec = trusted_comspec()?;
            let double_escape_meta = super::is_node_modules_cmd_shim(&target);
            let mut shell_command = Vec::new();
            super::append_cmd_escaped_command(
                &mut shell_command,
                &target.as_os_str().encode_wide().collect::<Vec<_>>(),
            )?;
            for argument in &command[1..] {
                shell_command.push(u16::from(b' '));
                super::append_cmd_escaped_argument(
                    &mut shell_command,
                    &argument.encode_wide().collect::<Vec<_>>(),
                    double_escape_meta,
                )?;
            }
            let mut command_line = Vec::new();
            append_quoted_arg(&mut command_line, comspec.as_os_str())?;
            command_line.extend(" /d /s /v:off /c \"".encode_utf16());
            command_line.extend(shell_command);
            command_line.push(u16::from(b'"'));
            command_line.push(0);
            if command_line.len() > 32_767 {
                return Err(
                    "batch process-tree command line exceeds CreateProcessW's UTF-16 limit"
                        .to_owned(),
                );
            }
            return Ok(PreparedCommand {
                application: comspec.as_os_str().encode_wide().chain(Some(0)).collect(),
                command_line,
            });
        }

        let mut resolved = command.to_vec();
        resolved[0] = target.as_os_str().to_os_string();
        Ok(PreparedCommand {
            application: target.as_os_str().encode_wide().chain(Some(0)).collect(),
            command_line: command_line(&resolved)?,
        })
    }

    fn duplicate_inheritable_standard_handle(
        kind: windows::Win32::System::Console::STD_HANDLE,
    ) -> Result<OwnedHandle, String> {
        // SAFETY: reads the current process standard-handle table.
        let source =
            unsafe { GetStdHandle(kind) }.map_err(|err| format!("GetStdHandle failed: {err}"))?;
        if source.is_invalid() {
            return Err("process-tree wrapper has an invalid standard handle".to_owned());
        }
        let mut duplicate = HANDLE::default();
        // SAFETY: the returned inheritable duplicate is newly owned and the
        // wrapper's original handle flags remain untouched.
        unsafe {
            DuplicateHandle(
                GetCurrentProcess(),
                source,
                GetCurrentProcess(),
                &raw mut duplicate,
                0,
                true,
                DUPLICATE_SAME_ACCESS,
            )
            .map_err(|err| format!("DuplicateHandle(standard handle) failed: {err}"))?;
        }
        if duplicate.is_invalid() {
            return Err("DuplicateHandle returned an invalid standard handle".to_owned());
        }
        Ok(OwnedHandle(duplicate))
    }

    fn process_creation_attribute_list<'values>(
        handles: &'values [HANDLE],
        jobs: &'values [HANDLE],
    ) -> Result<OwnedAttributeList<'values>, String> {
        if handles.is_empty() || jobs.is_empty() {
            return Err("process creation handle and Job lists must be non-empty".to_owned());
        }
        let mut bytes = 0usize;
        // The sizing call intentionally reports the required allocation size.
        unsafe {
            let _ = InitializeProcThreadAttributeList(None, 2, None, &raw mut bytes);
        }
        if bytes == 0 {
            return Err("InitializeProcThreadAttributeList returned an empty size".to_owned());
        }
        let words = bytes
            .checked_add(size_of::<usize>() - 1)
            .ok_or_else(|| "process attribute list size overflow".to_owned())?
            / size_of::<usize>();
        let mut storage = vec![0usize; words];
        let list = LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
        unsafe {
            InitializeProcThreadAttributeList(Some(list), 2, None, &raw mut bytes)
                .map_err(|err| format!("InitializeProcThreadAttributeList failed: {err}"))?;
        }
        let owned = OwnedAttributeList {
            _storage: storage,
            list,
            _values: std::marker::PhantomData,
        };
        unsafe {
            UpdateProcThreadAttribute(
                owned.list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(handles.as_ptr().cast()),
                std::mem::size_of_val(handles),
                None,
                None,
            )
            .map_err(|err| format!("UpdateProcThreadAttribute(handle list) failed: {err}"))?;
            UpdateProcThreadAttribute(
                owned.list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(jobs.as_ptr().cast()),
                std::mem::size_of_val(jobs),
                None,
                None,
            )
            .map_err(|err| {
                format!(
                    "UpdateProcThreadAttribute(Job list) failed; atomic Job assignment requires \
                     Windows 10 / Windows Server 2016 or newer: {err}"
                )
            })?;
        }
        Ok(owned)
    }

    fn create_suspended_child_in_job(
        prepared: &mut PreparedCommand,
        job: &OwnedHandle,
    ) -> Result<(OwnedHandle, OwnedHandle), String> {
        let standard_handles = [
            duplicate_inheritable_standard_handle(STD_INPUT_HANDLE)?,
            duplicate_inheritable_standard_handle(STD_OUTPUT_HANDLE)?,
            duplicate_inheritable_standard_handle(STD_ERROR_HANDLE)?,
        ];
        let inherited_handles = [
            standard_handles[0].0,
            standard_handles[1].0,
            standard_handles[2].0,
        ];
        let jobs = [job.0];
        let attribute_list = process_creation_attribute_list(&inherited_handles, &jobs)?;
        let mut startup = STARTUPINFOEXW::default();
        startup.StartupInfo.cb = u32::try_from(size_of::<STARTUPINFOEXW>())
            .map_err(|_| "STARTUPINFOEX size overflow".to_owned())?;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = inherited_handles[0];
        startup.StartupInfo.hStdOutput = inherited_handles[1];
        startup.StartupInfo.hStdError = inherited_handles[2];
        startup.lpAttributeList = attribute_list.list;
        let mut process = PROCESS_INFORMATION::default();
        // SAFETY: all strings are NUL-terminated, command_line is writable,
        // handles remain alive, and JOB_LIST makes containment creation-time.
        unsafe {
            CreateProcessW(
                PCWSTR::from_raw(prepared.application.as_ptr()),
                Some(PWSTR::from_raw(prepared.command_line.as_mut_ptr())),
                None,
                None,
                true,
                CREATE_SUSPENDED | EXTENDED_STARTUPINFO_PRESENT,
                None,
                None,
                &raw const startup.StartupInfo,
                &raw mut process,
            )
            .map_err(|err| {
                format!(
                    "CreateProcessW atomic Job-list child failed; incompatible parent Job nesting \
                     or UI limits fail closed without starting the child: {err}"
                )
            })?;
        }
        Ok((OwnedHandle(process.hProcess), OwnedHandle(process.hThread)))
    }

    fn assert_child_in_job(job: &OwnedHandle, process: &OwnedHandle) -> Result<(), String> {
        let mut contained = windows::core::BOOL(0);
        // SAFETY: both guards own valid Win32 handles.
        unsafe { IsProcessInJob(process.0, Some(job.0), &raw mut contained) }
            .map_err(|err| format!("IsProcessInJob failed: {err}"))?;
        if contained.0 == 0 {
            return Err(
                "CreateProcessW returned a child outside the requested atomic Job".to_owned(),
            );
        }
        Ok(())
    }

    fn terminate_and_reap_suspended_child(job: &OwnedHandle, process: &OwnedHandle) {
        // SAFETY: both handles are live; the wait ensures no suspended child
        // survives a failed membership/resume assertion.
        unsafe {
            let _ = TerminateJobObject(job.0, 1);
            let _ = TerminateProcess(process.0, 1);
            let _ = WaitForSingleObject(process.0, u32::MAX);
        }
    }

    pub(super) fn run(command: Vec<OsString>) -> Result<i32, String> {
        if command.is_empty() {
            return Err("process-tree-exec requires a child command".to_owned());
        }
        let mut prepared = prepare_command(&command)?;
        let info_len = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .map_err(|_| "Job Object information size overflow".to_owned())?;
        // SAFETY: returns a new owned Job handle.
        let job = unsafe { CreateJobObjectW(None, None) }
            .map(OwnedHandle)
            .map_err(|err| format!("CreateJobObjectW failed: {err}"))?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT(
            super::ordinary_process_tree_job_limit_flags(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.0),
        );
        // SAFETY: `limits` matches the requested information class.
        unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&limits).cast(),
                info_len,
            )
            .map_err(|err| format!("SetInformationJobObject failed: {err}"))?;
        }

        let (process_handle, thread_handle) = create_suspended_child_in_job(&mut prepared, &job)?;
        if let Err(error) = assert_child_in_job(&job, &process_handle) {
            terminate_and_reap_suspended_child(&job, &process_handle);
            return Err(error);
        }
        // SAFETY: this is the untouched primary thread from CREATE_SUSPENDED.
        let previous_suspend_count = unsafe { ResumeThread(thread_handle.0) };
        if previous_suspend_count != 1 {
            let detail = if previous_suspend_count == u32::MAX {
                windows::core::Error::from_thread().to_string()
            } else {
                format!("unexpected previous suspend count {previous_suspend_count}")
            };
            terminate_and_reap_suspended_child(&job, &process_handle);
            return Err(format!("ResumeThread failed closed: {detail}"));
        }
        drop(thread_handle);
        if unsafe { WaitForSingleObject(process_handle.0, u32::MAX) } != WAIT_OBJECT_0 {
            terminate_and_reap_suspended_child(&job, &process_handle);
            return Err("WaitForSingleObject failed".to_owned());
        }
        let mut exit_code = 1u32;
        // SAFETY: the direct child is signaled and its process handle is live.
        unsafe {
            GetExitCodeProcess(process_handle.0, &raw mut exit_code)
                .map_err(|err| format!("GetExitCodeProcess failed: {err}"))?;
            // Reap any descendants that outlived the direct child before the
            // wrapper exits, guaranteeing inherited output handles reach EOF.
            let _ = TerminateJobObject(job.0, 1);
        }
        Ok(i32::from_ne_bytes(exit_code.to_ne_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os_args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn escaped(value: &str, append: fn(&mut Vec<u16>, &[u16]) -> Result<(), String>) -> String {
        let mut output = Vec::new();
        append(&mut output, &value.encode_utf16().collect::<Vec<_>>()).unwrap();
        String::from_utf16(&output).unwrap()
    }

    fn quoted_command_line(values: &[&str]) -> Result<String, String> {
        let mut output = Vec::new();
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                output.push(u16::from(b' '));
            }
            append_windows_quoted_units(&mut output, &value.encode_utf16().collect::<Vec<_>>())?;
        }
        String::from_utf16(&output).map_err(|error| error.to_string())
    }

    #[test]
    fn private_route_parser_is_exact_and_lossless() {
        assert_eq!(
            parse_process_tree_exec_args(&os_args(&[
                "process-tree-exec",
                "--",
                r"C:\Program Files\bun.exe",
                "space value",
                "--flag"
            ]))
            .unwrap(),
            Some(os_args(&[
                r"C:\Program Files\bun.exe",
                "space value",
                "--flag"
            ]))
        );
        assert!(
            parse_process_tree_exec_args(&os_args(&["--", "cmd.exe"]))
                .unwrap()
                .is_none()
        );
        assert!(parse_process_tree_exec_args(&os_args(&["process-tree-exec", "cmd.exe"])).is_err());
        assert!(parse_process_tree_exec_args(&os_args(&["process-tree-exec", "--"])).is_err());
    }

    #[test]
    fn helper_environment_name_is_stable() {
        assert_eq!(PROCESS_TREE_HELPER_ENV, "CRABCODE_PROCESS_TREE_EXECUTABLE");
    }

    #[test]
    fn ordinary_job_limit_is_strict_kill_on_close_only() {
        const KILL_ON_CLOSE: u32 = 0x2000;
        assert_eq!(
            ordinary_process_tree_job_limit_flags(KILL_ON_CLOSE),
            KILL_ON_CLOSE
        );
        assert_eq!(
            ordinary_process_tree_job_limit_flags(KILL_ON_CLOSE) & 0x0c00,
            0,
            "ordinary helper must not admit breakaway flags"
        );
    }

    #[test]
    fn batch_extensions_and_node_shim_rule_match_the_authority() {
        for extension in ["bat", "BAT", "bAt", "cmd", "CMD", "CmD"] {
            assert!(is_windows_batch_extension(extension));
        }
        for extension in ["exe", "com", "ps1", "cmd.exe", ""] {
            assert!(!is_windows_batch_extension(extension));
        }
        assert!(is_node_modules_cmd_shim(Path::new(
            r"C:\repo\node_modules\.bin\tool.CMD"
        )));
        assert!(!is_node_modules_cmd_shim(Path::new(
            r"C:\repo\scripts\tool.cmd"
        )));
    }

    #[test]
    fn batch_cmd_escaping_matches_the_existing_cross_spawn_grammar() {
        assert_eq!(
            escaped(
                r"C:\Program Files\Demo & Tool.CMD",
                append_cmd_escaped_command,
            ),
            r"C:\Program^ Files\Demo^ ^&^ Tool.CMD",
        );
        assert_eq!(
            escaped("A&B|C^D( E )%F!G", |output, units| {
                append_cmd_escaped_argument(output, units, false)
            }),
            r#"^"A^&B^|C^^D^(^ E^ ^)^%F^!G^""#,
        );
        assert_eq!(
            escaped(r"trail\", |output, units| {
                append_cmd_escaped_argument(output, units, false)
            }),
            r#"^"trail\^""#,
        );
        assert_eq!(
            escaped("A&B", |output, units| {
                append_cmd_escaped_argument(output, units, true)
            }),
            r#"^^^"A^^^&B^^^""#,
        );
        assert!(
            append_cmd_escaped_argument(&mut Vec::new(), &[u16::from(b'X'), 0], false).is_err()
        );
        assert!(
            append_cmd_escaped_argument(
                &mut Vec::new(),
                &"line\nbreak".encode_utf16().collect::<Vec<_>>(),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn native_command_line_quoting_covers_spaces_quotes_and_trailing_slashes() {
        assert_eq!(
            quoted_command_line(&["plain.exe", "space value", r#"quote"value"#, r"trail\"])
                .unwrap(),
            r#"plain.exe "space value" "quote\"value" trail\"#
        );
        assert_eq!(
            quoted_command_line(&["plain.exe", ""]).unwrap(),
            r#"plain.exe """#
        );
        assert!(append_windows_quoted_units(&mut Vec::new(), &[0]).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn process_tree_execution_fails_closed_off_windows() {
        let error = run_process_tree_exec(os_args(&["child"]))
            .expect_err("non-Windows must never emulate Job Object containment");
        assert_eq!(error, "process-tree-exec is only available on Windows");
    }

    #[cfg(windows)]
    #[serial_test::serial]
    #[test]
    #[allow(unsafe_code)]
    fn batch_path_pathext_and_metacharacters_round_trip() {
        struct RestoreEnvironment {
            path: Option<OsString>,
            pathext: Option<OsString>,
        }

        impl Drop for RestoreEnvironment {
            fn drop(&mut self) {
                // SAFETY: serialized test restores both variables on unwind.
                unsafe {
                    match self.path.take() {
                        Some(value) => std::env::set_var("PATH", value),
                        None => std::env::remove_var("PATH"),
                    }
                    match self.pathext.take() {
                        Some(value) => std::env::set_var("PATHEXT", value),
                        None => std::env::remove_var("PATHEXT"),
                    }
                }
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("MiXeD Runner.CmD");
        let observed = temp.path().join("observed.txt");
        std::fs::write(
            &script,
            "@echo off\r\n\
             setlocal DisableDelayedExpansion\r\n\
             set \"CRAB_ARG0=%~1\"\r\n\
             set \"CRAB_ARG1=%~2\"\r\n\
             set \"CRAB_ARG2=%~3\"\r\n\
             set \"CRAB_OUT=%~dp0observed.txt\"\r\n\
             powershell.exe -NoLogo -NoProfile -NonInteractive -Command \"[IO.File]::WriteAllLines($env:CRAB_OUT,@($env:CRAB_ARG0,$env:CRAB_ARG1,$env:CRAB_ARG2))\"\r\n\
             exit /b 37\r\n",
        )
        .unwrap();

        let old_path = std::env::var_os("PATH");
        let old_pathext = std::env::var_os("PATHEXT");
        let mut path_entries = vec![temp.path().to_path_buf()];
        if let Some(path) = &old_path {
            path_entries.extend(std::env::split_paths(path));
        }
        let test_path = std::env::join_paths(path_entries).unwrap();
        let _restore = RestoreEnvironment {
            path: old_path,
            pathext: old_pathext,
        };
        // SAFETY: guarded by serial_test and restored by `_restore`.
        unsafe {
            std::env::set_var("PATH", test_path);
            std::env::set_var("PATHEXT", ".CmD;.EXE;.BAT;.CMD");
        }

        let exit = run_process_tree_exec(os_args(&[
            "MiXeD Runner",
            "space value",
            "A&B|C^D( E )%F!G",
            r"trail\",
        ]))
        .unwrap();
        assert_eq!(exit, 37);
        let values = std::fs::read_to_string(observed)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(values, ["space value", "A&B|C^D( E )%F!G", r"trail\"]);
    }
}

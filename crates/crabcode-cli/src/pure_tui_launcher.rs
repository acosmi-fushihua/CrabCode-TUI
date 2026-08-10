//! Compile-time pure CrabCode TUI launcher.
//!
//! The release packager renames this binary to the public launcher name. Its
//! linked closure contains only the native TUI + memory supervision route; it
//! cannot discover or retry a second product surface.

mod memory_runtime;
mod native_generation;
mod native_tui_bootstrap;
mod sandbox_exec;
mod sandbox_probe;

#[cfg(any(windows, test))]
use std::ffi::{OsStr, OsString};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use acosmi_supervisor::topology::ProcessId;
use anyhow::{Context as _, Result, bail};

const UNSUPPORTED_PREFIX: &str = "CRABCODE_PURE_TUI_UNSUPPORTED:";
const LAUNCH_FAILED_PREFIX: &str = "CRABCODE_PURE_TUI_LAUNCH_FAILED:";
const UNSUPPORTED_EXIT_CODE: i32 = 64;
const LAUNCH_FAILED_EXIT_CODE: i32 = 70;
const RELEASE_PACKAGE_SMOKE_ARG: &str = "__release-package-smoke";
const RELEASE_PACKAGE_SMOKE_ENV: &str = "CRABCODE_RELEASE_PACKAGE_SMOKE";

enum PureLaunchError {
    Unsupported(String),
    Failed(anyhow::Error),
}

fn main() {
    match run() {
        Ok(0) => {}
        Ok(code) => std::process::exit(code),
        Err(PureLaunchError::Unsupported(reason)) => {
            eprintln!("{UNSUPPORTED_PREFIX} {reason}");
            std::process::exit(UNSUPPORTED_EXIT_CODE);
        }
        Err(PureLaunchError::Failed(error)) => {
            eprintln!("{LAUNCH_FAILED_PREFIX} {error:#}");
            std::process::exit(LAUNCH_FAILED_EXIT_CODE);
        }
    }
}

fn run() -> std::result::Result<i32, PureLaunchError> {
    // Stable launchers must consult the signed generation pointer before any
    // terminal policy, public argument parsing, or sibling discovery. A
    // selected generation short-circuits this path and cannot recurse.
    if let Some(exit_code) =
        native_generation::handoff_to_current_generation().map_err(PureLaunchError::Failed)?
    {
        return Ok(exit_code);
    }

    // Internal sandbox helpers run in the selected immutable generation, but
    // before acquiring a generation lease or applying any terminal/public-TUI
    // policy. The probe owns stdout (one JSON line); sandbox-exec owns all
    // three stdio streams and becomes the requested command on Unix.
    if try_dispatch_sandbox_probe_before_public_parse()? {
        return Ok(0);
    }
    try_dispatch_sandbox_exec_before_public_parse();

    // A directly running immutable generation owns a validation-bound lease
    // before any private or public route can execute. Unlike the generic
    // executable-location lease, this revalidates the complete pure-TUI
    // pre-runtime closure under the shared per-version transaction.
    let generation_lease = native_generation::acquire_current_generation_lease()
        .context("cannot acquire validated native generation lease")
        .map_err(PureLaunchError::Failed)?;

    let raw_os_args = std::env::args_os().skip(1).collect::<Vec<_>>();
    #[cfg(windows)]
    if let Some(command) =
        parse_private_native_generation_command(&raw_os_args).map_err(PureLaunchError::Failed)?
    {
        let exit_code =
            dispatch_private_native_generation_command(command).map_err(PureLaunchError::Failed)?;
        drop(generation_lease);
        return Ok(exit_code);
    }

    // This exact private route must remain usable from piped Bun/MCP children,
    // so dispatch it before any terminal or public-surface policy check.
    if let Some(command) =
        acosmi_supervisor::windows_process_tree::parse_process_tree_exec_args(&raw_os_args)
            .map_err(|error| PureLaunchError::Failed(anyhow::Error::msg(error)))?
    {
        return acosmi_supervisor::windows_process_tree::run_process_tree_exec(command)
            .map_err(|error| PureLaunchError::Failed(anyhow::Error::msg(error)));
    }
    let raw_args = raw_os_args
        .into_iter()
        .map(|argument| {
            argument.into_string().map_err(|argument| {
                PureLaunchError::Unsupported(format!(
                    "public TUI argument is not valid Unicode: {argument:?}"
                ))
            })
        })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let release_package_smoke = raw_args == [RELEASE_PACKAGE_SMOKE_ARG]
        && std::env::var(RELEASE_PACKAGE_SMOKE_ENV).as_deref() == Ok("1");
    if !release_package_smoke
        && let Some(reason) = unsupported_reason(
            &raw_args,
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        )
    {
        return Err(PureLaunchError::Unsupported(reason));
    }

    let tui_args = normalize_native_args(raw_args).map_err(PureLaunchError::Unsupported)?;
    let cwd = std::env::current_dir()
        .context("cannot resolve launch working directory")
        .map_err(PureLaunchError::Failed)?;
    let tui_binary = required_tui_sibling().map_err(PureLaunchError::Failed)?;
    let mut config = native_tui_bootstrap::build_native_tui_supervisor(&cwd, &tui_binary, tui_args)
        .map_err(PureLaunchError::Failed)?;

    if let Some(lease) = generation_lease.as_ref() {
        let foreground = config
            .processes
            .iter_mut()
            .find(|process| process.id == ProcessId::native_tui())
            .context("native supervisor topology omitted its foreground TUI")
            .map_err(PureLaunchError::Failed)?;
        for (name, value) in lease.environment().map_err(PureLaunchError::Failed)? {
            let _ = foreground.env.insert(name.to_owned(), value);
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot build native supervisor runtime")
        .map_err(PureLaunchError::Failed)?;
    let exit_code = runtime
        .block_on(acosmi_supervisor::run_with_config_foreground_exit_code(
            config, None,
        ))
        .map_err(PureLaunchError::Failed)?
        .unwrap_or(0);
    drop(generation_lease);
    Ok(exit_code)
}

/// Dispatch the exact internal probe shape before any launcher side effect.
///
/// A malformed `sandbox-probe` route is rejected here instead of falling
/// through as a public TUI prompt. `--json` is accepted as the version-skew
/// guard used by the TypeScript caller; the payload is JSON in both forms.
fn try_dispatch_sandbox_probe_before_public_parse() -> std::result::Result<bool, PureLaunchError> {
    let mut args = std::env::args();
    let Some(_program) = args.next() else {
        return Ok(false);
    };
    if args.next().as_deref() != Some("sandbox-probe") {
        return Ok(false);
    }
    match args.next().as_deref() {
        None => {}
        Some("--json") if args.next().is_none() => {}
        _ => {
            return Err(PureLaunchError::Unsupported(
                "sandbox-probe is an internal route and only accepts `--json`".into(),
            ));
        }
    }
    sandbox_probe::run_sandbox_probe().map_err(PureLaunchError::Failed)?;
    Ok(true)
}

/// Dispatch the internal per-command sandbox helper.
///
/// Once the first token matches, the helper owns the invocation. Its parser
/// intentionally accepts only `--config-stdin -- PROGRAM [ARGS...]`; malformed
/// forms fail through the stable exit-125 protocol and can never become a TUI
/// prompt.
fn try_dispatch_sandbox_exec_before_public_parse() {
    let mut args = std::env::args();
    let Some(_program) = args.next() else {
        return;
    };
    if args.next().as_deref() != Some("sandbox-exec") {
        return;
    }
    sandbox_exec::dispatch(args.collect());
}

#[cfg(any(windows, test))]
#[derive(Debug, PartialEq, Eq)]
enum PrivateNativeGenerationCommand {
    Activate {
        parent_pid: u32,
        stable_launcher: PathBuf,
    },
    ActivateCommit {
        parent_pid: u32,
        parent_start_identity: String,
        stable_launcher: PathBuf,
    },
}

#[cfg(any(windows, test))]
fn parse_private_native_generation_command(
    args: &[OsString],
) -> Result<Option<PrivateNativeGenerationCommand>> {
    let Some(command) = args.first() else {
        return Ok(None);
    };
    if command == OsStr::new("__native-update-activate") {
        if args.len() != 5
            || args[1] != OsStr::new("--parent-pid")
            || args[3] != OsStr::new("--stable-launcher")
        {
            bail!("invalid private native activation command shape");
        }
        let parent_pid = parse_private_parent_pid(&args[2])?;
        let stable_launcher = PathBuf::from(&args[4]);
        return Ok(Some(PrivateNativeGenerationCommand::Activate {
            parent_pid,
            stable_launcher,
        }));
    }
    if command == OsStr::new("__native-update-activate-commit") {
        if args.len() != 7
            || args[1] != OsStr::new("--parent-pid")
            || args[3] != OsStr::new("--parent-start-identity")
            || args[5] != OsStr::new("--stable-launcher")
        {
            bail!("invalid private native activation commit command shape");
        }
        let parent_pid = parse_private_parent_pid(&args[2])?;
        let parent_start_identity = args[4]
            .clone()
            .into_string()
            .map_err(|_| anyhow::anyhow!("private activation birth identity is not Unicode"))?;
        if parent_start_identity.is_empty() {
            bail!("private activation birth identity is empty");
        }
        let stable_launcher = PathBuf::from(&args[6]);
        return Ok(Some(PrivateNativeGenerationCommand::ActivateCommit {
            parent_pid,
            parent_start_identity,
            stable_launcher,
        }));
    }
    Ok(None)
}

#[cfg(any(windows, test))]
fn parse_private_parent_pid(value: &OsStr) -> Result<u32> {
    let value = value
        .to_str()
        .context("private native activation parent PID is not Unicode")?;
    let parent_pid = value
        .parse::<u32>()
        .context("private native activation parent PID is invalid")?;
    if parent_pid <= 1 {
        bail!("private native activation parent PID must be greater than one");
    }
    Ok(parent_pid)
}

#[cfg(windows)]
fn dispatch_private_native_generation_command(
    command: PrivateNativeGenerationCommand,
) -> Result<i32> {
    match command {
        PrivateNativeGenerationCommand::Activate {
            parent_pid,
            stable_launcher,
        } => {
            let outcome = native_generation::launch_prepared_windows_activation_helper(
                parent_pid,
                &stable_launcher,
            )?;
            Ok(
                if outcome == native_generation::WindowsActivationBrokerOutcome::RestartRequired {
                    native_generation::WINDOWS_ACTIVATION_RESTART_REQUIRED_EXIT_CODE
                } else {
                    0
                },
            )
        }
        PrivateNativeGenerationCommand::ActivateCommit {
            parent_pid,
            parent_start_identity,
            stable_launcher,
        } => {
            native_generation::activate_prepared_windows_launcher(
                parent_pid,
                &parent_start_identity,
                &stable_launcher,
            )?;
            Ok(0)
        }
    }
}

fn unsupported_reason(
    args: &[String],
    stdin_attached: bool,
    stdout_attached: bool,
) -> Option<String> {
    if !stdout_attached {
        return Some("stdout must be attached to a terminal".into());
    }
    if !stdin_attached && !cfg!(unix) {
        return Some("stdin must be attached to a terminal on this platform".into());
    }
    if args.iter().any(|arg| arg == "-p" || arg == "--print") {
        return Some("print/headless routing is outside the pure TUI artifact".into());
    }
    if args.iter().any(|arg| {
        arg.strip_prefix("--").is_some_and(|name| {
            name == "chrome" || name == "no-chrome" || name.starts_with("chrome-")
        })
    }) {
        return Some("browser routing is outside the pure TUI artifact".into());
    }

    let first = args.first().map(String::as_str);
    if matches!(
        first,
        Some(
            "serve"
                | "doctor"
                | "version"
                | "browser"
                | "cron"
                | "__native-update-activate"
                | "__native-update-activate-commit"
        )
    ) {
        return Some(format!(
            "legacy command route `{}` is outside the pure TUI artifact",
            first.unwrap_or_default()
        ));
    }
    None
}

fn normalize_native_args(args: Vec<String>) -> std::result::Result<Vec<String>, String> {
    let mut normalized = Vec::with_capacity(args.len());
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            // The pure launcher is compile-time native, so this full-launcher
            // opt-in flag is consumed rather than leaked to the TUI parser.
            "--experimental-rust-tui" => {}
            // Logging format belongs to a shared supervisor CLI. Accept and
            // consume its value so existing invocation wrappers remain usable.
            "--log-format" => {
                let Some(value) = iter.next() else {
                    return Err("--log-format requires a value".into());
                };
                if value != "text" && value != "json" {
                    return Err("--log-format accepts only text or json".into());
                }
            }
            value if value.starts_with("--log-format=") => {
                let value = value.trim_start_matches("--log-format=");
                if value != "text" && value != "json" {
                    return Err("--log-format accepts only text or json".into());
                }
            }
            _ => normalized.push(arg),
        }
    }
    Ok(normalized)
}

fn required_tui_sibling() -> Result<PathBuf> {
    let executable = std::env::current_exe().context("cannot resolve pure TUI launcher path")?;
    let executable = std::fs::canonicalize(&executable).unwrap_or(executable);
    let directory = executable
        .parent()
        .context("pure TUI launcher has no parent directory")?;
    let name = if cfg!(windows) {
        "crabcode-tui.exe"
    } else {
        "crabcode-tui"
    };
    verified_executable(&directory.join(name))
}

fn verified_executable(path: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("required native TUI sibling is missing: {}", path.display()))?;
    let metadata = std::fs::metadata(&canonical).with_context(|| {
        format!(
            "cannot inspect required native TUI sibling: {}",
            canonical.display()
        )
    })?;
    if !metadata.is_file() {
        bail!(
            "required native TUI sibling is not a regular file: {}",
            canonical.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            bail!(
                "required native TUI sibling is not executable: {}",
                canonical.display()
            );
        }
    }
    if canonical == std::fs::canonicalize(std::env::current_exe()?)? {
        bail!("native TUI sibling resolves to the launcher itself");
    }
    Ok(canonical)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn out_of_domain_routes_fail_before_spawn() {
        assert!(unsupported_reason(&args(&[]), true, false).is_some());
        assert_eq!(
            unsupported_reason(&args(&[]), false, true).is_none(),
            cfg!(unix),
            "only Unix has the fixed historical piped-stdin /dev/tty route"
        );
        for values in [
            &["-p"][..],
            &["--print"],
            &["--chrome"],
            &["--no-chrome"],
            &["--chrome-automation-mcp"],
            &["--chrome-native-host"],
            &["serve"],
            &["doctor"],
            &["version"],
            &["browser"],
            &["cron"],
        ] {
            assert!(
                unsupported_reason(&args(values), true, true).is_some(),
                "{values:?}"
            );
        }
    }

    #[test]
    fn ordinary_native_arguments_are_lossless() {
        let input = args(&[
            "--experimental-rust-tui",
            "--screen-mode",
            "inline",
            "--model",
            "best",
            "explain",
        ]);
        assert_eq!(
            normalize_native_args(input).unwrap(),
            args(&["--screen-mode", "inline", "--model", "best", "explain"])
        );
    }

    #[test]
    fn shared_log_format_is_consumed_and_validated() {
        assert_eq!(
            normalize_native_args(args(&["--log-format=json", "--model", "best"])).unwrap(),
            args(&["--model", "best"])
        );
        assert!(normalize_native_args(args(&["--log-format"])).is_err());
        assert!(normalize_native_args(args(&["--log-format=xml"])).is_err());
    }

    #[test]
    fn private_native_generation_commands_are_exact_and_not_public_options() {
        let activate = [
            "__native-update-activate",
            "--parent-pid",
            "4242",
            "--stable-launcher",
            "/tmp/crabcode",
        ]
        .map(OsString::from);
        assert_eq!(
            parse_private_native_generation_command(&activate).unwrap(),
            Some(PrivateNativeGenerationCommand::Activate {
                parent_pid: 4242,
                stable_launcher: PathBuf::from("/tmp/crabcode"),
            })
        );

        let commit = [
            "__native-update-activate-commit",
            "--parent-pid",
            "4242",
            "--parent-start-identity",
            "win32-process-start:638881234567890123",
            "--stable-launcher",
            r"C:\Program Files\Crab Code\crabcode.exe",
        ]
        .map(OsString::from);
        assert_eq!(
            parse_private_native_generation_command(&commit).unwrap(),
            Some(PrivateNativeGenerationCommand::ActivateCommit {
                parent_pid: 4242,
                parent_start_identity: "win32-process-start:638881234567890123".to_string(),
                stable_launcher: PathBuf::from(r"C:\Program Files\Crab Code\crabcode.exe"),
            })
        );

        for malformed in [
            &activate[..4],
            &[
                OsString::from("__native-update-activate"),
                OsString::from("--parent-pid"),
                OsString::from("1"),
                OsString::from("--stable-launcher"),
                OsString::from("/tmp/crabcode"),
            ],
            &[
                OsString::from("__native-update-activate"),
                OsString::from("--stable-launcher"),
                OsString::from("/tmp/crabcode"),
                OsString::from("--parent-pid"),
                OsString::from("4242"),
            ],
        ] {
            assert!(parse_private_native_generation_command(malformed).is_err());
        }
        assert_eq!(
            parse_private_native_generation_command(&[OsString::from("--model")]).unwrap(),
            None
        );
    }
}

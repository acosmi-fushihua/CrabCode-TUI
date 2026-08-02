use std::path::{Path, PathBuf};
#[cfg(any(unix, windows))]
use std::process::Stdio;
use std::time::{Duration, Instant};

use acosmi_memory_orchestrator::ping_endpoint;
use anyhow::{bail, Context, Result};
#[cfg(any(unix, windows))]
use tokio::io::AsyncWriteExt;
#[cfg(any(unix, windows))]
use tokio::process::Command;

fn repo_root() -> Result<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .context("resolve repo root from CARGO_MANIFEST_DIR")
}

fn source_between<'a>(source: &'a str, start: &str, end: &str) -> Result<&'a str> {
    let start_idx = source
        .find(start)
        .with_context(|| format!("missing start marker: {start}"))?;
    let rest = &source[start_idx..];
    let end_idx = rest
        .find(end)
        .with_context(|| format!("missing end marker after {start}: {end}"))?;
    Ok(&rest[..end_idx])
}

#[test]
fn pure_tui_bootstrap_ensures_stable_memory_without_owning_its_transport() -> Result<()> {
    let bootstrap = std::fs::read_to_string(
        repo_root()?.join("crates/crabcode-cli/src/native_tui_bootstrap.rs"),
    )?;
    let ensure_section = source_between(
        &bootstrap,
        "pub(crate) fn build_native_tui_supervisor(",
        "fn build_native_tui_supervisor_with_inputs(",
    )?;
    assert!(ensure_section.contains("MemoryRuntimeCoordinator::resolve("));
    assert!(ensure_section.contains("memory.ensure()"));

    let topology_section = source_between(
        &bootstrap,
        "fn build_native_tui_supervisor_with_inputs(",
        "#[cfg(windows)]",
    )?;
    assert!(topology_section.contains("lifecycle_env.insert_unicode(MEMORY_IPC_ENDPOINT_ENV"));
    assert!(topology_section.contains("id: ProcessId::native_tui()"));
    assert!(topology_section.contains("stdio_policy: StdioPolicy::InheritTerminal"));
    assert!(topology_section.contains("ipc_type: IpcType::None"));
    assert!(topology_section.contains("process_group: ProcessGroupPolicy::Foreground"));
    assert!(topology_section.contains("processes: vec![native_config]"));
    assert!(!topology_section.contains("ProcessId::ts_session()"));
    assert!(!topology_section.contains("ProcessId::memory_orchestrator()"));

    let orchestrator = std::fs::read_to_string(
        repo_root()?.join("libs/acosmi-memory/acosmi-memory-orchestrator/src/lib.rs"),
    )?;
    assert!(orchestrator.contains("tokio::net::windows::named_pipe::ServerOptions"));
    assert!(orchestrator.contains("tokio::net::windows::named_pipe::ClientOptions"));
    assert!(!orchestrator.contains("named pipe memory endpoint is reserved"));
    assert!(!orchestrator.contains("named pipe memory ping is reserved"));

    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn memory_ping_uses_dedicated_unix_socket_not_stdio() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let socket_path = dir.path().join("crabcode-memory-e12.sock");
    let endpoint = format!("unix:{}", socket_path.display());
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");
    let journal_path = dir.path().join("memory-journal.sqlite3");

    let mut child = Command::new(binary)
        .env("CRABCODE_MEMORY_IPC_ENDPOINT", &endpoint)
        .env("CRABCODE_MEMORY_JOURNAL_PATH", &journal_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(br#"{"method":"memory.ping","transport":"stdio"}"#)
            .await?;
    }

    let result = wait_for_ping(&mut child, &endpoint).await;
    child.start_kill()?;
    let output = child.wait_with_output().await?;

    let pong = result.with_context(|| {
        format!(
            "memory orchestrator stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    })?;
    assert_eq!(pong["ok"], true);
    assert_eq!(pong["service"], "acosmi-memory-orchestrator");
    assert!(
        output.stdout.is_empty(),
        "memory.ping must not respond over stdout/stdin IPC; stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );

    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn memory_ping_uses_dedicated_windows_named_pipe_not_stdio() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let pipe_name = format!(r"\\.\pipe\crabcode-memory-e12-{}", std::process::id());
    let endpoint = format!("npipe:{pipe_name}");
    let binary = env!("CARGO_BIN_EXE_acosmi-memory-orchestrator");

    let mut child = Command::new(binary)
        .env("CRABCODE_MEMORY_IPC_ENDPOINT", &endpoint)
        .env(
            "CRABCODE_MEMORY_JOURNAL_PATH",
            dir.path().join("memory-journal.sqlite3"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(br#"{"method":"memory.ping","transport":"stdio"}"#)
            .await?;
    }

    let result = wait_for_ping(&mut child, &endpoint).await;
    child.start_kill()?;
    let output = child.wait_with_output().await?;

    let pong = result?;
    assert_eq!(pong["ok"], true);
    assert_eq!(pong["service"], "acosmi-memory-orchestrator");
    assert!(
        output.stdout.is_empty(),
        "memory.ping must not respond over stdout/stdin IPC; stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );

    Ok(())
}

#[cfg(any(unix, windows))]
async fn wait_for_ping(
    child: &mut tokio::process::Child,
    endpoint: &str,
) -> Result<serde_json::Value> {
    // Match the production coordinator's measured startup contract. A 148 MB
    // unoptimized macOS test binary took 6.90 s to expose its socket on this
    // host; the release binary stayed below 1.5 s.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_error = None;

    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            bail!("memory-orchestrator exited before ping succeeded: {status}");
        }

        match ping_endpoint(endpoint).await {
            Ok(pong) => return Ok(pong),
            Err(e) => last_error = Some(e),
        }

        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    match last_error {
        Some(e) => Err(e).context("memory.ping did not succeed before timeout"),
        None => bail!("memory.ping did not run before timeout"),
    }
}

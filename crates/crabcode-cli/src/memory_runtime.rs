//! Stable per-state-root Memory runtime discovery and cold start.
//!
//! The Memory orchestrator is deliberately generation-independent. Concurrent
//! direct-runtime callers must resolve the same local endpoint and exactly one
//! restart supervisor. Two OS locks split the lifecycle:
//!
//! - `memory-runtime.start.lock` serializes cold-start observation/spawn;
//! - `memory-runtime.owner.lock` is held for the complete lifetime of the
//!   orchestrator supervisor process.
//!
//! This module performs only pre-runtime cold start. On Unix its detached
//! spawn uses `fork`, so callers must invoke [`MemoryRuntimeCoordinator::ensure`]
//! before constructing a multi-threaded runtime.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use acosmi_daemon_launcher::socket_lock::{self, LockOutcome};
use acosmi_daemon_launcher::{DetachedCommand, LauncherError};
use thiserror::Error;

pub const MEMORY_IPC_ENDPOINT_ENV: &str = "CRABCODE_MEMORY_IPC_ENDPOINT";
pub const MEMORY_JOURNAL_PATH_ENV: &str = "CRABCODE_MEMORY_JOURNAL_PATH";
pub const MEMORY_ORCHESTRATOR_BIN_ENV: &str = "CRABCODE_MEMORY_ORCHESTRATOR_BIN";
pub const MEMORY_COORDINATOR_ENV: &str = "CRABCODE_MEMORY_COORDINATOR";
pub const MEMORY_COORDINATOR_CHILD_ENV: &str = "CRABCODE_MEMORY_COORDINATOR_CHILD";
pub const MEMORY_COORDINATOR_LOCK_ENV: &str = "CRABCODE_MEMORY_COORDINATOR_LOCK";

const START_TIMEOUT: Duration = Duration::from_secs(10);
const RETRY_INTERVAL: Duration = Duration::from_millis(50);
const PROBE_TIMEOUT: Duration = Duration::from_millis(750);
const LOG_TAIL_BYTES: u64 = 4 * 1024;
const MAX_PROBE_RESPONSE_BYTES: usize = 4 * 1024;
const MEMORY_PROTOCOL_VERSION: u64 = 1;
const MEMORY_SCHEMA_ID: &str = "crabcode-memory-ipc-v1-20260725";
const MEMORY_SERVICE_IDENTITY: &str = "acosmi-memory-orchestrator";
const MEMORY_REQUIRED_CAPABILITIES: &[&str] =
    &["coordinator-promote-v1", "events-v1", "runner-journal-v1"];
const MEMORY_PING_FRAME: &[u8] = b"{\"method\":\"memory.ping\"}\n";

#[derive(Debug, Error)]
pub enum MemoryRuntimeError {
    #[error("cannot canonicalize managed Memory state root {path}: {source}")]
    StateRootCanonicalization {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot resolve Memory orchestrator beside caller executable: {0}")]
    CallerExecutable(PathBuf),
    #[error("Memory orchestrator binary must be an absolute regular non-symlink file: {0}")]
    InvalidBinary(PathBuf),
    #[error("cannot prepare private Memory runtime directory: {0}")]
    RuntimeIo(#[source] std::io::Error),
    #[error("cannot acquire Memory runtime lifecycle lock: {0}")]
    LifecycleLock(#[source] LauncherError),
    #[error("cannot spawn Memory runtime coordinator: {0}")]
    Spawn(#[source] LauncherError),
    #[error("Memory runtime at {endpoint} is incompatible: {reason}")]
    IncompatibleEndpoint { endpoint: String, reason: String },
    #[error("Memory runtime at {endpoint} rejected successor promotion: {reason}")]
    PromotionRejected { endpoint: String, reason: String },
    #[error(
        "Memory runtime did not open {endpoint} within {timeout:?}; log: {log_path}{tail_suffix}"
    )]
    StartTimeout {
        endpoint: String,
        timeout: Duration,
        log_path: PathBuf,
        tail_suffix: String,
    },
}

/// Resolved, immutable startup contract for one canonical state root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRuntimeCoordinator {
    state_root: PathBuf,
    endpoint: String,
    binary: PathBuf,
    start_lock: PathBuf,
    owner_lock: PathBuf,
    journal_path: PathBuf,
    log_path: PathBuf,
}

impl MemoryRuntimeCoordinator {
    /// Resolve the stable endpoint, sibling binary, locks, and log from one
    /// state-root snapshot and one executable snapshot.
    pub fn resolve(
        state_root: &Path,
        caller_executable: &Path,
    ) -> Result<Self, MemoryRuntimeError> {
        let state_root =
            acosmi_daemon_launcher::state_identity::canonicalize_state_root_path(state_root)
                .map_err(|source| MemoryRuntimeError::StateRootCanonicalization {
                    path: state_root.to_path_buf(),
                    source,
                })?;
        let binary = resolve_binary(caller_executable)?;
        let username = std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string());
        let endpoint = acosmi_daemon_launcher::paths::memory_ipc_endpoint_for_state_root(
            &state_root,
            &username,
        );
        let run_dir = state_root.join("run");
        let log_dir = state_root.join("logs");
        let journal_path = state_root.join("memory").join("memory-journal.sqlite3");
        Ok(Self {
            state_root,
            endpoint,
            binary,
            start_lock: run_dir.join("memory-runtime.start.lock"),
            owner_lock: run_dir.join("memory-runtime.owner.lock"),
            journal_path,
            log_path: log_dir.join("memory-orchestrator.log"),
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub fn state_root(&self) -> &Path {
        &self.state_root
    }

    #[must_use]
    pub fn binary(&self) -> &Path {
        &self.binary
    }

    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    /// Ensure the stable supervisor is serving the endpoint.
    ///
    /// The startup lock remains held until readiness, preventing a second
    /// launcher from observing the short window between detached spawn and
    /// the supervisor acquiring its lifetime owner lock.
    pub fn ensure(&self) -> Result<(), MemoryRuntimeError> {
        self.prepare_runtime_paths()?;
        let probe = EndpointProbe::new()?;
        match probe.observe(&self.endpoint) {
            EndpointObservation::Reusable => return Ok(()),
            EndpointObservation::Incompatible(reason) => {
                return Err(self.incompatible_endpoint(reason));
            }
            EndpointObservation::Unavailable | EndpointObservation::PromotionRequired(_) => {}
        }

        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            match probe.observe(&self.endpoint) {
                EndpointObservation::Reusable => return Ok(()),
                EndpointObservation::Incompatible(reason) => {
                    return Err(self.incompatible_endpoint(reason));
                }
                EndpointObservation::Unavailable | EndpointObservation::PromotionRequired(_) => {}
            }
            if Instant::now() >= deadline {
                return Err(self.timeout_error());
            }

            match socket_lock::acquire(&self.start_lock)
                .map_err(MemoryRuntimeError::LifecycleLock)?
            {
                LockOutcome::Contended => std::thread::sleep(RETRY_INTERVAL),
                LockOutcome::Acquired(start_guard) => {
                    // Keep the guard alive through endpoint readiness. This is
                    // the handoff barrier between launcher and supervisor.
                    let _start_guard = start_guard;
                    let mut spawned = false;
                    let mut promotion_requested_for = None::<String>;
                    while Instant::now() < deadline {
                        match probe.observe(&self.endpoint) {
                            EndpointObservation::Reusable => return Ok(()),
                            EndpointObservation::Incompatible(reason) => {
                                return Err(self.incompatible_endpoint(reason));
                            }
                            EndpointObservation::PromotionRequired(identity) => {
                                if promotion_requested_for.as_deref()
                                    != Some(identity.build_id.as_str())
                                {
                                    probe.request_promotion(&self.endpoint, &identity).map_err(
                                        |reason| MemoryRuntimeError::PromotionRejected {
                                            endpoint: self.endpoint.clone(),
                                            reason,
                                        },
                                    )?;
                                    promotion_requested_for = Some(identity.build_id);
                                }
                            }
                            EndpointObservation::Unavailable => {
                                if !spawned {
                                    match socket_lock::acquire(&self.owner_lock)
                                        .map_err(MemoryRuntimeError::LifecycleLock)?
                                    {
                                        // Acquiring the lifetime lock is the
                                        // proof that an acknowledged old
                                        // owner has exited. Drop this probe
                                        // before spawning the successor so
                                        // the new coordinator can own it.
                                        LockOutcome::Acquired(owner_probe) => {
                                            drop(owner_probe);
                                            self.spawn_supervisor()?;
                                            spawned = true;
                                        }
                                        // A live supervisor is either
                                        // draining after promotion or in a
                                        // child restart/backoff window.
                                        LockOutcome::Contended => {}
                                    }
                                }
                            }
                        }
                        std::thread::sleep(RETRY_INTERVAL);
                    }
                    return Err(self.timeout_error());
                }
            }
        }
    }

    fn prepare_runtime_paths(&self) -> Result<(), MemoryRuntimeError> {
        prepare_private_directory(
            self.start_lock
                .parent()
                .expect("start lock always has run-dir parent"),
        )?;
        prepare_private_directory(
            self.log_path
                .parent()
                .expect("log path always has log-dir parent"),
        )?;
        prepare_private_directory(
            self.journal_path
                .parent()
                .expect("journal path always has memory-dir parent"),
        )?;
        if let Some(socket_path) = unix_socket_path(&self.endpoint)
            && let Some(parent) = socket_path.parent()
        {
            prepare_private_directory(parent)?;
        }
        Ok(())
    }

    fn spawn_supervisor(&self) -> Result<(), MemoryRuntimeError> {
        tracing::info!(
            endpoint = %self.endpoint,
            binary = %self.binary().display(),
            state_root = %self.state_root().display(),
            "starting stable Memory runtime coordinator"
        );
        let command = self.supervisor_command();
        acosmi_daemon_launcher::spawn_detached_command(&command).map_err(MemoryRuntimeError::Spawn)
    }

    fn supervisor_command(&self) -> DetachedCommand {
        DetachedCommand {
            binary: self.binary.clone(),
            args: Vec::new(),
            env_overrides: vec![
                (
                    OsString::from(MEMORY_IPC_ENDPOINT_ENV),
                    OsString::from(&self.endpoint),
                ),
                (OsString::from(MEMORY_COORDINATOR_ENV), OsString::from("1")),
                (
                    OsString::from(MEMORY_COORDINATOR_CHILD_ENV),
                    OsString::from("0"),
                ),
                (
                    OsString::from(MEMORY_COORDINATOR_LOCK_ENV),
                    self.owner_lock.as_os_str().to_os_string(),
                ),
                (
                    OsString::from(MEMORY_JOURNAL_PATH_ENV),
                    self.journal_path().as_os_str().to_os_string(),
                ),
                (
                    OsString::from("CRABCODE_CONFIG_DIR"),
                    self.state_root().as_os_str().to_os_string(),
                ),
            ],
            log_file: Some(self.log_path.clone()),
        }
    }

    fn timeout_error(&self) -> MemoryRuntimeError {
        MemoryRuntimeError::StartTimeout {
            endpoint: self.endpoint.clone(),
            timeout: START_TIMEOUT,
            log_path: self.log_path.clone(),
            tail_suffix: format_log_tail(&self.log_path),
        }
    }

    fn incompatible_endpoint(&self, reason: String) -> MemoryRuntimeError {
        MemoryRuntimeError::IncompatibleEndpoint {
            endpoint: self.endpoint.clone(),
            reason,
        }
    }
}

fn resolve_binary(caller_executable: &Path) -> Result<PathBuf, MemoryRuntimeError> {
    let candidate = std::env::var_os(MEMORY_ORCHESTRATOR_BIN_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let caller = fs::canonicalize(caller_executable)
                .unwrap_or_else(|_| caller_executable.to_path_buf());
            caller
                .parent()
                .map_or_else(PathBuf::new, |parent| parent.join(binary_name()))
        });
    if candidate.as_os_str().is_empty() {
        return Err(MemoryRuntimeError::CallerExecutable(
            caller_executable.to_path_buf(),
        ));
    }
    if !candidate.is_absolute() {
        return Err(MemoryRuntimeError::InvalidBinary(candidate));
    }
    let metadata = fs::symlink_metadata(&candidate)
        .map_err(|_| MemoryRuntimeError::InvalidBinary(candidate.clone()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(MemoryRuntimeError::InvalidBinary(candidate));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(MemoryRuntimeError::InvalidBinary(candidate));
        }
    }
    fs::canonicalize(&candidate).map_err(|_| MemoryRuntimeError::InvalidBinary(candidate))
}

fn binary_name() -> &'static OsStr {
    if cfg!(windows) {
        OsStr::new("acosmi-memory-orchestrator.exe")
    } else {
        OsStr::new("acosmi-memory-orchestrator")
    }
}

fn prepare_private_directory(path: &Path) -> Result<(), MemoryRuntimeError> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path).map_err(MemoryRuntimeError::RuntimeIo)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(MemoryRuntimeError::RuntimeIo(std::io::Error::other(
                format!("runtime path must be a real directory: {}", path.display()),
            )));
        }
    } else {
        fs::create_dir_all(path).map_err(MemoryRuntimeError::RuntimeIo)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(MemoryRuntimeError::RuntimeIo)?;
    }
    Ok(())
}

fn unix_socket_path(endpoint: &str) -> Option<PathBuf> {
    endpoint.strip_prefix("unix:").map(PathBuf::from)
}

struct EndpointProbe {
    runtime: tokio::runtime::Runtime,
    local_build_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoryEndpointIdentity {
    protocol_version: u64,
    schema_id: String,
    build_id: String,
    capabilities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum EndpointObservation {
    /// No process accepted the connection. A launcher may cold-start only
    /// after separately proving that the lifetime owner lock is free.
    Unavailable,
    /// The endpoint implements the complete contract and is the same or a
    /// safely newer/non-authoritative generation.
    Reusable,
    /// The endpoint implements the complete contract but is an older,
    /// authoritative generation. It must explicitly acknowledge handoff.
    PromotionRequired(MemoryEndpointIdentity),
    /// A process answered, but its identity or control contract is unsafe to
    /// reuse or replace automatically.
    Incompatible(String),
}

#[derive(Debug)]
enum EndpointRequestError {
    Unavailable,
    Incompatible(String),
}

impl EndpointProbe {
    fn new() -> Result<Self, MemoryRuntimeError> {
        Self::with_build_id(acosmi_daemon_launcher::build_id::self_build_id())
    }

    fn with_build_id(local_build_id: &str) -> Result<Self, MemoryRuntimeError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
            .map_err(MemoryRuntimeError::RuntimeIo)?;
        Ok(Self {
            runtime,
            local_build_id: local_build_id.to_string(),
        })
    }

    fn observe(&self, endpoint: &str) -> EndpointObservation {
        self.runtime.block_on(async {
            match tokio::time::timeout(
                PROBE_TIMEOUT,
                request_memory_endpoint(endpoint, MEMORY_PING_FRAME),
            )
            .await
            {
                Err(_) => EndpointObservation::Incompatible(
                    "identity probe timed out after accepting or attempting the endpoint"
                        .to_string(),
                ),
                Ok(Err(EndpointRequestError::Unavailable)) => EndpointObservation::Unavailable,
                Ok(Err(EndpointRequestError::Incompatible(reason))) => {
                    EndpointObservation::Incompatible(reason)
                }
                Ok(Ok(response)) => classify_memory_identity(&self.local_build_id, &response),
            }
        })
    }

    fn request_promotion(
        &self,
        endpoint: &str,
        current: &MemoryEndpointIdentity,
    ) -> Result<(), String> {
        // Promotion is never inferred from connection loss. The old owner
        // must acknowledge this exact successor generation and the complete
        // protocol/schema contract before the launcher waits for lock release.
        let mut frame = serde_json::to_vec(&serde_json::json!({
            "method": "memory.coordinator.promote",
            "payload": {
                "successor_build_id": self.local_build_id,
                "protocol_version": MEMORY_PROTOCOL_VERSION,
                "schema_id": MEMORY_SCHEMA_ID,
            }
        }))
        .map_err(|error| format!("cannot encode promotion request: {error}"))?;
        frame.push(b'\n');

        let response = self.runtime.block_on(async {
            tokio::time::timeout(
                PROBE_TIMEOUT,
                request_memory_endpoint(endpoint, frame.as_slice()),
            )
            .await
        });
        let response = match response {
            Err(_) => return Err("promotion acknowledgement timed out".to_string()),
            Ok(Err(EndpointRequestError::Unavailable)) => {
                return Err("endpoint disappeared before promotion acknowledgement".to_string());
            }
            Ok(Err(EndpointRequestError::Incompatible(reason))) => return Err(reason),
            Ok(Ok(response)) => response,
        };

        let ok = response.get("ok").and_then(serde_json::Value::as_bool);
        let promote = response.get("promote").and_then(serde_json::Value::as_bool);
        let current_build_id = response
            .get("current_build_id")
            .and_then(serde_json::Value::as_str);
        let successor_build_id = response
            .get("successor_build_id")
            .and_then(serde_json::Value::as_str);
        if ok == Some(true)
            && promote == Some(true)
            && current_build_id == Some(current.build_id.as_str())
            && successor_build_id == Some(self.local_build_id.as_str())
        {
            Ok(())
        } else {
            let remote_error = response
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("malformed or generation-mismatched promotion acknowledgement");
            Err(remote_error.to_string())
        }
    }
}

fn classify_memory_identity(
    local_build_id: &str,
    response: &serde_json::Value,
) -> EndpointObservation {
    let identity = match parse_memory_identity(response) {
        Ok(identity) => identity,
        Err(reason) => return EndpointObservation::Incompatible(reason),
    };

    if identity.protocol_version != MEMORY_PROTOCOL_VERSION {
        return EndpointObservation::Incompatible(format!(
            "protocol mismatch: expected {MEMORY_PROTOCOL_VERSION}, got {}",
            identity.protocol_version
        ));
    }
    if identity.schema_id != MEMORY_SCHEMA_ID {
        return EndpointObservation::Incompatible(format!(
            "schema mismatch: expected {MEMORY_SCHEMA_ID}, got {}",
            identity.schema_id
        ));
    }
    let missing_capabilities = MEMORY_REQUIRED_CAPABILITIES
        .iter()
        .copied()
        .filter(|required| {
            !identity
                .capabilities
                .iter()
                .any(|actual| actual == required)
        })
        .collect::<Vec<_>>();
    if !missing_capabilities.is_empty() {
        return EndpointObservation::Incompatible(format!(
            "missing required capabilities: {}",
            missing_capabilities.join(", ")
        ));
    }

    let build_id = &identity.build_id;
    let local_is_authoritative = acosmi_daemon_launcher::build_id::is_authoritative(local_build_id);
    let remote_is_authoritative = acosmi_daemon_launcher::build_id::is_authoritative(build_id);
    if local_build_id == build_id || !local_is_authoritative || !remote_is_authoritative {
        return EndpointObservation::Reusable;
    }
    if acosmi_daemon_launcher::build_id::build_id_version_newer(build_id, local_build_id) {
        // Never let an older launcher downgrade a newer stable owner.
        return EndpointObservation::Reusable;
    }
    if acosmi_daemon_launcher::build_id::build_id_version_newer(local_build_id, build_id) {
        return EndpointObservation::PromotionRequired(identity);
    }

    EndpointObservation::Incompatible(format!(
        "authoritative build ids differ but are not safely ordered: local={local_build_id}, remote={build_id}"
    ))
}

fn parse_memory_identity(response: &serde_json::Value) -> Result<MemoryEndpointIdentity, String> {
    if response.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(response
            .get("error")
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || "identity response did not affirm ok=true".to_string(),
                |error| format!("identity probe rejected: {error}"),
            ));
    }
    if response.get("service").and_then(serde_json::Value::as_str) != Some(MEMORY_SERVICE_IDENTITY)
    {
        return Err("service identity mismatch".to_string());
    }
    let protocol_version = response
        .get("protocol_version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "identity is missing numeric protocol_version".to_string())?;
    let schema_id = response
        .get("schema_id")
        .and_then(serde_json::Value::as_str)
        .filter(|schema_id| !schema_id.is_empty())
        .ok_or_else(|| "identity is missing non-empty schema_id".to_string())?
        .to_string();
    let build_id = response
        .get("build_id")
        .and_then(serde_json::Value::as_str)
        .filter(|build_id| !build_id.is_empty())
        .ok_or_else(|| "identity is missing non-empty build_id".to_string())?
        .to_string();
    let capabilities = response
        .get("capabilities")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "identity is missing capabilities array".to_string())?
        .iter()
        .map(|capability| {
            capability
                .as_str()
                .filter(|capability| !capability.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    "identity capabilities must contain only non-empty strings".to_string()
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(MemoryEndpointIdentity {
        protocol_version,
        schema_id,
        build_id,
        capabilities,
    })
}

async fn request_memory_endpoint(
    endpoint: &str,
    request_frame: &[u8],
) -> Result<serde_json::Value, EndpointRequestError> {
    use tokio::io::AsyncWriteExt as _;

    if let Some(path) = endpoint.strip_prefix("unix:") {
        #[cfg(unix)]
        {
            let mut stream = tokio::net::UnixStream::connect(path)
                .await
                .map_err(|_| EndpointRequestError::Unavailable)?;
            stream
                .write_all(request_frame)
                .await
                .map_err(|_| EndpointRequestError::Unavailable)?;
            stream
                .flush()
                .await
                .map_err(|_| EndpointRequestError::Unavailable)?;
            return read_probe_response(&mut stream)
                .await
                .map_err(classify_response_io_error);
        }
        #[cfg(not(unix))]
        {
            let _ = path;
        }
    }
    if let Some(pipe) = endpoint.strip_prefix("npipe:") {
        #[cfg(windows)]
        {
            let mut stream = tokio::net::windows::named_pipe::ClientOptions::new()
                .open(pipe)
                .map_err(|_| EndpointRequestError::Unavailable)?;
            stream
                .write_all(request_frame)
                .await
                .map_err(|_| EndpointRequestError::Unavailable)?;
            stream
                .flush()
                .await
                .map_err(|_| EndpointRequestError::Unavailable)?;
            return read_probe_response(&mut stream)
                .await
                .map_err(classify_response_io_error);
        }
        #[cfg(not(windows))]
        {
            let _ = pipe;
        }
    }
    Err(EndpointRequestError::Incompatible(
        "unsupported Memory endpoint scheme".to_string(),
    ))
}

fn classify_response_io_error(error: std::io::Error) -> EndpointRequestError {
    match error.kind() {
        std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::BrokenPipe => EndpointRequestError::Unavailable,
        _ => {
            EndpointRequestError::Incompatible(format!("invalid Memory control response: {error}"))
        }
    }
}

async fn read_probe_response<R>(reader: &mut R) -> std::io::Result<serde_json::Value>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt as _;

    let mut frame = Vec::with_capacity(256);
    let mut chunk = [0_u8; 256];
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        let slice = &chunk[..read];
        if let Some(newline) = slice.iter().position(|byte| *byte == b'\n') {
            frame.extend_from_slice(&slice[..newline]);
            break;
        }
        frame.extend_from_slice(slice);
        if frame.len() > MAX_PROBE_RESPONSE_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Memory control response exceeded size limit",
            ));
        }
    }
    if frame.is_empty() || frame.len() > MAX_PROBE_RESPONSE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "Memory control response missing or oversized",
        ));
    }
    serde_json::from_slice(&frame)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

fn format_log_tail(path: &Path) -> String {
    let tail = read_log_tail(path);
    if tail.is_empty() {
        String::new()
    } else {
        format!("\n--- memory-orchestrator.log tail ---\n{tail}")
    }
}

fn read_log_tail(path: &Path) -> String {
    let Ok(mut file) = fs::File::open(path) else {
        return String::new();
    };
    let Ok(length) = file
        .metadata()
        .map(|metadata| metadata.len().min(LOG_TAIL_BYTES))
    else {
        return String::new();
    };
    if file.seek(SeekFrom::End(-(length as i64))).is_err() {
        return String::new();
    }
    let mut bytes = vec![0; length as usize];
    if file.read_exact(&mut bytes).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&bytes).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    #[cfg(unix)]
    fn resolve_binds_all_artifacts_to_one_canonical_root() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempdir().expect("tempdir");
        let bin_dir = temp.path().join("bin");
        fs::create_dir(&bin_dir).expect("bin dir");
        let caller = bin_dir.join("crabcode-launcher");
        let memory = bin_dir.join("acosmi-memory-orchestrator");
        fs::write(&caller, b"caller").expect("caller");
        fs::write(&memory, b"memory").expect("memory");
        fs::set_permissions(&caller, fs::Permissions::from_mode(0o700)).expect("caller mode");
        fs::set_permissions(&memory, fs::Permissions::from_mode(0o700)).expect("memory mode");

        let raw_root = temp.path().join("state/../state");
        let runtime = MemoryRuntimeCoordinator::resolve(&raw_root, &caller).expect("resolve");
        assert!(runtime.state_root().is_absolute());
        assert_eq!(
            runtime.binary(),
            memory.canonicalize().expect("canonical memory")
        );
        assert!(
            runtime.endpoint().contains("memory-orchestrator.sock"),
            "{}",
            runtime.endpoint()
        );
        assert!(runtime.start_lock.starts_with(runtime.state_root()));
        assert!(runtime.owner_lock.starts_with(runtime.state_root()));
        assert!(runtime.log_path.starts_with(runtime.state_root()));
        assert_eq!(
            runtime.journal_path(),
            runtime
                .state_root()
                .join("memory")
                .join("memory-journal.sqlite3")
        );

        let next_session =
            MemoryRuntimeCoordinator::resolve(runtime.state_root(), &caller).expect("resolve next");
        assert_eq!(next_session.endpoint, runtime.endpoint);
        assert_eq!(next_session.start_lock, runtime.start_lock);
        assert_eq!(next_session.owner_lock, runtime.owner_lock);
        assert_eq!(next_session.journal_path, runtime.journal_path);
        assert_eq!(next_session.log_path, runtime.log_path);

        let command = runtime.supervisor_command();
        assert!(command.args.is_empty());
        assert_eq!(command.binary, runtime.binary);
        assert_eq!(
            command.log_file.as_deref(),
            Some(runtime.log_path.as_path())
        );
        let env = command
            .env_overrides
            .iter()
            .cloned()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(
            env.get(OsStr::new(MEMORY_IPC_ENDPOINT_ENV)),
            Some(&OsString::from(runtime.endpoint()))
        );
        assert_eq!(
            env.get(OsStr::new(MEMORY_JOURNAL_PATH_ENV)),
            Some(&runtime.journal_path.as_os_str().to_os_string())
        );
        assert_eq!(
            env.get(OsStr::new(MEMORY_COORDINATOR_ENV)),
            Some(&OsString::from("1"))
        );
        assert_eq!(
            env.get(OsStr::new(MEMORY_COORDINATOR_CHILD_ENV)),
            Some(&OsString::from("0"))
        );
        assert_eq!(
            env.get(OsStr::new(MEMORY_COORDINATOR_LOCK_ENV)),
            Some(&runtime.owner_lock.as_os_str().to_os_string())
        );
        assert_eq!(
            env.get(OsStr::new("CRABCODE_CONFIG_DIR")),
            Some(&runtime.state_root.as_os_str().to_os_string())
        );
    }

    #[test]
    fn coordinator_has_no_removed_surface_launcher_dependency() {
        let removed_dependency = ["acosmi-", "app", "-server-launcher"].concat();
        assert!(!include_str!("../Cargo.toml").contains(&removed_dependency));
    }

    #[test]
    fn unknown_endpoint_scheme_is_never_treated_as_compatible() {
        let probe = EndpointProbe::new().expect("probe runtime");
        assert!(matches!(
            probe.observe("tcp:127.0.0.1:7"),
            EndpointObservation::Incompatible(_)
        ));
        assert!(matches!(
            probe.observe(""),
            EndpointObservation::Incompatible(_)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn endpoint_probe_accepts_complete_matching_identity() {
        use std::io::Read as _;
        use std::io::Write as _;

        let temp = tempdir().expect("tempdir");
        let socket = temp.path().join("memory.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0_u8; MEMORY_PING_FRAME.len()];
            stream.read_exact(&mut request).expect("read ping");
            assert_eq!(request, MEMORY_PING_FRAME);
            let mut response = serde_json::to_vec(&identity_response(
                acosmi_daemon_launcher::build_id::self_build_id(),
            ))
            .expect("serialize identity");
            response.push(b'\n');
            stream.write_all(&response).expect("write response");
        });
        let probe = EndpointProbe::new().expect("probe runtime");
        assert_eq!(
            probe.observe(&format!("unix:{}", socket.display())),
            EndpointObservation::Reusable
        );
        server.join().expect("server joins");
    }

    #[test]
    #[cfg(unix)]
    fn endpoint_probe_rejects_connectable_impostor() {
        use std::io::Read as _;
        use std::io::Write as _;

        let temp = tempdir().expect("tempdir");
        let socket = temp.path().join("impostor.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0_u8; MEMORY_PING_FRAME.len()];
            stream.read_exact(&mut request).expect("read ping");
            stream
                .write_all(b"{\"ok\":true,\"service\":\"other\",\"protocol_version\":1}\n")
                .expect("write response");
        });
        let probe = EndpointProbe::new().expect("probe runtime");
        assert!(matches!(
            probe.observe(&format!("unix:{}", socket.display())),
            EndpointObservation::Incompatible(_)
        ));
        server.join().expect("server joins");
    }

    #[test]
    fn identity_reuse_requires_complete_protocol_schema_and_capabilities() {
        let exact = identity_response("1.2.3+aaaaaaaaaaaa");
        assert_eq!(
            classify_memory_identity("1.2.3+aaaaaaaaaaaa", &exact),
            EndpointObservation::Reusable
        );

        let mut missing_schema = exact.clone();
        missing_schema
            .as_object_mut()
            .expect("identity object")
            .remove("schema_id");
        assert_incompatible_contains(
            classify_memory_identity("1.2.3+aaaaaaaaaaaa", &missing_schema),
            "schema_id",
        );

        let mut wrong_schema = exact.clone();
        wrong_schema["schema_id"] = serde_json::json!("memory-v2");
        assert_incompatible_contains(
            classify_memory_identity("1.2.3+aaaaaaaaaaaa", &wrong_schema),
            "schema mismatch",
        );

        let mut missing_capability = exact.clone();
        missing_capability["capabilities"] = serde_json::json!(["coordinator-promote-v1"]);
        assert_incompatible_contains(
            classify_memory_identity("1.2.3+aaaaaaaaaaaa", &missing_capability),
            "events-v1",
        );

        let mut missing_promotion_capability = exact.clone();
        missing_promotion_capability["capabilities"] =
            serde_json::json!(["events-v1", "runner-journal-v1"]);
        assert_incompatible_contains(
            classify_memory_identity("1.2.4+bbbbbbbbbbbb", &missing_promotion_capability),
            "coordinator-promote-v1",
        );

        let mut missing_journal_capability = exact.clone();
        missing_journal_capability["capabilities"] =
            serde_json::json!(["coordinator-promote-v1", "events-v1"]);
        assert_incompatible_contains(
            classify_memory_identity("1.2.3+aaaaaaaaaaaa", &missing_journal_capability),
            "runner-journal-v1",
        );

        let mut wrong_protocol = exact;
        wrong_protocol["protocol_version"] = serde_json::json!(2);
        assert_incompatible_contains(
            classify_memory_identity("1.2.3+aaaaaaaaaaaa", &wrong_protocol),
            "protocol mismatch",
        );
    }

    #[test]
    fn build_order_is_monotonic_and_same_version_different_sha_fails_closed() {
        let older = identity_response("1.2.2+aaaaaaaaaaaa");
        let observation = classify_memory_identity("1.2.3+bbbbbbbbbbbb", &older);
        let EndpointObservation::PromotionRequired(identity) = observation else {
            panic!("strictly older authoritative owner must require promotion");
        };
        assert_eq!(identity.build_id, "1.2.2+aaaaaaaaaaaa");

        let newer = identity_response("1.2.4+cccccccccccc");
        assert_eq!(
            classify_memory_identity("1.2.3+bbbbbbbbbbbb", &newer),
            EndpointObservation::Reusable,
            "an older caller must never downgrade the stable owner"
        );

        let same_version_other_sha = identity_response("1.2.3+cccccccccccc");
        assert_incompatible_contains(
            classify_memory_identity("1.2.3+bbbbbbbbbbbb", &same_version_other_sha),
            "not safely ordered",
        );
    }

    #[test]
    fn non_authoritative_builds_never_trigger_promotion() {
        let authoritative_remote = identity_response("1.2.2+aaaaaaaaaaaa");
        assert_eq!(
            classify_memory_identity("1.2.3+unknown", &authoritative_remote),
            EndpointObservation::Reusable
        );

        let unknown_remote = identity_response("1.2.2+unknown");
        assert_eq!(
            classify_memory_identity("1.2.3+bbbbbbbbbbbb", &unknown_remote),
            EndpointObservation::Reusable
        );
    }

    #[test]
    #[cfg(unix)]
    fn promotion_request_binds_ack_to_observed_and_successor_builds() {
        use std::io::BufRead as _;
        use std::io::BufReader;
        use std::io::Write as _;

        let temp = tempdir().expect("tempdir");
        let socket = temp.path().join("promotion.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut ping_stream, _) = listener.accept().expect("accept ping");
            let mut ping = String::new();
            BufReader::new(&mut ping_stream)
                .read_line(&mut ping)
                .expect("read ping");
            assert_eq!(ping.as_bytes(), MEMORY_PING_FRAME);
            let mut identity = serde_json::to_vec(&identity_response("1.2.2+aaaaaaaaaaaa"))
                .expect("serialize identity");
            identity.push(b'\n');
            ping_stream.write_all(&identity).expect("write identity");
            drop(ping_stream);

            let (mut promote_stream, _) = listener.accept().expect("accept promotion");
            let mut promote = String::new();
            BufReader::new(&mut promote_stream)
                .read_line(&mut promote)
                .expect("read promotion");
            let request: serde_json::Value =
                serde_json::from_str(&promote).expect("parse promotion");
            assert_eq!(request["method"], "memory.coordinator.promote");
            assert_eq!(
                request["payload"]["successor_build_id"],
                "1.2.3+bbbbbbbbbbbb"
            );
            assert_eq!(
                request["payload"]["protocol_version"],
                MEMORY_PROTOCOL_VERSION
            );
            assert_eq!(request["payload"]["schema_id"], MEMORY_SCHEMA_ID);
            promote_stream
                .write_all(
                    b"{\"ok\":true,\"promote\":true,\"current_build_id\":\"1.2.2+aaaaaaaaaaaa\",\"successor_build_id\":\"1.2.3+bbbbbbbbbbbb\"}\n",
                )
                .expect("write promotion ack");
        });

        let probe = EndpointProbe::with_build_id("1.2.3+bbbbbbbbbbbb").expect("probe runtime");
        let endpoint = format!("unix:{}", socket.display());
        let EndpointObservation::PromotionRequired(identity) = probe.observe(&endpoint) else {
            panic!("older endpoint should require promotion");
        };
        probe
            .request_promotion(&endpoint, &identity)
            .expect("valid promotion acknowledgement");
        server.join().expect("server joins");
    }

    #[test]
    #[cfg(unix)]
    fn promotion_ack_with_wrong_current_generation_fails_closed() {
        use std::io::BufRead as _;
        use std::io::BufReader;
        use std::io::Write as _;

        let temp = tempdir().expect("tempdir");
        let socket = temp.path().join("promotion-wrong-ack.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept promotion");
            let mut request = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut request)
                .expect("read promotion");
            stream
                .write_all(
                    b"{\"ok\":true,\"promote\":true,\"current_build_id\":\"1.2.1+wrongwrong12\",\"successor_build_id\":\"1.2.3+bbbbbbbbbbbb\"}\n",
                )
                .expect("write wrong ack");
        });

        let probe = EndpointProbe::with_build_id("1.2.3+bbbbbbbbbbbb").expect("probe runtime");
        let identity =
            parse_memory_identity(&identity_response("1.2.2+aaaaaaaaaaaa")).expect("identity");
        let error = probe
            .request_promotion(&format!("unix:{}", socket.display()), &identity)
            .expect_err("mismatched ack must fail closed");
        assert!(error.contains("generation-mismatched"), "{error}");
        server.join().expect("server joins");
    }

    #[test]
    #[cfg(unix)]
    fn private_directory_rejects_symlink() {
        use std::os::unix::fs::symlink;

        let temp = tempdir().expect("tempdir");
        let real = temp.path().join("real");
        fs::create_dir(&real).expect("real dir");
        let alias = temp.path().join("alias");
        symlink(&real, &alias).expect("symlink");
        assert!(prepare_private_directory(&alias).is_err());
    }

    fn identity_response(build_id: &str) -> serde_json::Value {
        serde_json::json!({
            "ok": true,
            "service": MEMORY_SERVICE_IDENTITY,
            "protocol_version": MEMORY_PROTOCOL_VERSION,
            "schema_id": MEMORY_SCHEMA_ID,
            "build_id": build_id,
            "capabilities": MEMORY_REQUIRED_CAPABILITIES,
            "pid": 42,
        })
    }

    fn assert_incompatible_contains(observation: EndpointObservation, expected: &str) {
        let EndpointObservation::Incompatible(reason) = observation else {
            panic!("expected incompatible observation, got {observation:?}");
        };
        assert!(
            reason.contains(expected),
            "{reason:?} did not contain {expected:?}"
        );
    }
}

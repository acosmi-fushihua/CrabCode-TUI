//! Process-local consumption of the native installer's pending-relaunch marker.
//!
//! This module owns no backend request and no daemon query. It reads the exact
//! marker written by `src/utils/nativeInstaller/pendingRelaunch.ts`, admits a
//! relaunch only from already-projected direct TUI state, and carries an
//! explicit phase fence so a replacement process cannot start before both the
//! terminal and the StructuredIO child have been released.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde::Deserialize;

const PENDING_RELAUNCH_MARKER: &str = "crabcode-tui.pending-relaunch";
const MAX_STATE_ROOT_SYMLINK_HOPS: usize = 40;
pub(crate) const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, thiserror::Error)]
pub(crate) enum PendingRelaunchError {
    #[error("{name} is not valid Unicode")]
    NonUnicodeEnvironment { name: &'static str },
    #[error("{name} must be absolute: {value}")]
    RelativeStateRoot { name: &'static str, value: String },
    #[error("cannot resolve the CrabCode home directory")]
    MissingHome,
    #[error("runtime state root has too many symlink hops: {0}")]
    TooManySymlinks(PathBuf),
    #[error("cannot inspect pending-relaunch marker {}: {source}", path.display())]
    MarkerMetadata { path: PathBuf, source: io::Error },
    #[error("pending-relaunch marker is not a regular file: {}", .0.display())]
    MarkerNotRegular(PathBuf),
    #[error("cannot read pending-relaunch marker {}: {source}", path.display())]
    MarkerRead { path: PathBuf, source: io::Error },
    #[error("pending-relaunch marker JSON is invalid: {0}")]
    MarkerJson(serde_json::Error),
    #[error("pending-relaunch marker targetVersion is invalid")]
    InvalidTargetVersion,
    #[error("pending-relaunch marker relaunchBinary must be an absolute path")]
    InvalidRelaunchBinary,
    #[error("pending-relaunch marker timestamps and process identity must be positive")]
    InvalidWriterIdentity,
    #[error("pending-relaunch lifecycle transition is out of order")]
    InvalidLifecycleTransition,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PendingRelaunchPreflightError {
    #[error("cannot inspect stable relaunch binary {}: {source}", path.display())]
    BinaryMetadata { path: PathBuf, source: io::Error },
    #[error("stable relaunch binary is not a regular file: {}", .0.display())]
    BinaryNotRegular(PathBuf),
    #[cfg(unix)]
    #[error("stable relaunch binary is not executable: {}", .0.display())]
    BinaryNotExecutable(PathBuf),
    #[error("cannot inspect relaunch working directory {}: {source}", path.display())]
    WorkingDirectoryMetadata { path: PathBuf, source: io::Error },
    #[error("relaunch working directory is not a directory: {}", .0.display())]
    WorkingDirectoryNotDirectory(PathBuf),
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingRelaunchMarker {
    target_version: String,
    relaunch_binary: String,
    written_at: u64,
    writer_pid: u32,
}

impl PendingRelaunchMarker {
    fn validate(self) -> Result<Self, PendingRelaunchError> {
        if self.target_version.is_empty() || self.target_version.trim() != self.target_version {
            return Err(PendingRelaunchError::InvalidTargetVersion);
        }
        if self.relaunch_binary.contains('\0') || !Path::new(&self.relaunch_binary).is_absolute() {
            return Err(PendingRelaunchError::InvalidRelaunchBinary);
        }
        if self.written_at == 0 || self.writer_pid == 0 {
            return Err(PendingRelaunchError::InvalidWriterIdentity);
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DirectIdleFacts<'a> {
    pub(crate) busy: bool,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) session_state: Option<&'a str>,
    pub(crate) active_task_count: usize,
    pub(crate) processing_background_task: bool,
    pub(crate) fatal: bool,
}

impl DirectIdleFacts<'_> {
    fn exact_idle_session_id(self) -> Option<String> {
        if self.fatal
            || self.busy
            || self.session_state != Some("idle")
            || self.active_task_count != 0
            || self.processing_background_task
        {
            return None;
        }
        let session_id = self.session_id?;
        is_canonical_session_id(session_id).then(|| session_id.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RelaunchPhase {
    Interactive,
    TerminalRestored,
    RuntimeStopped,
}

#[derive(Debug)]
pub(crate) struct PendingRelaunch {
    target_version: String,
    relaunch_binary: PathBuf,
    session_id: String,
    draft: Option<String>,
    cwd: PathBuf,
    phase: RelaunchPhase,
}

impl PendingRelaunch {
    fn new(marker: PendingRelaunchMarker, session_id: String, draft: &str, cwd: &Path) -> Self {
        let draft = crate::text_safety::trim_ecmascript_whitespace(draft);
        Self {
            target_version: marker.target_version,
            relaunch_binary: PathBuf::from(marker.relaunch_binary),
            session_id,
            draft: (!draft.is_empty()).then(|| draft.to_string()),
            cwd: cwd.to_path_buf(),
            phase: RelaunchPhase::Interactive,
        }
    }

    pub(crate) fn target_version(&self) -> &str {
        &self.target_version
    }

    /// Reject deterministic spawn failures while the current terminal and
    /// direct runtime are still fully owned by this process.
    ///
    /// This follows the stable symlink instead of canonicalizing it, so the
    /// installer's atomic launcher path remains the eventual spawn target.
    /// A successful preflight cannot eliminate a later TOCTOU or operating
    /// system resource failure; the terminal/runtime phase fence still applies
    /// to the actual spawn.
    pub(crate) fn preflight(&self) -> Result<(), PendingRelaunchPreflightError> {
        let binary_metadata = fs::metadata(&self.relaunch_binary).map_err(|source| {
            PendingRelaunchPreflightError::BinaryMetadata {
                path: self.relaunch_binary.clone(),
                source,
            }
        })?;
        if !binary_metadata.is_file() {
            return Err(PendingRelaunchPreflightError::BinaryNotRegular(
                self.relaunch_binary.clone(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            if binary_metadata.permissions().mode() & 0o111 == 0 {
                return Err(PendingRelaunchPreflightError::BinaryNotExecutable(
                    self.relaunch_binary.clone(),
                ));
            }
        }

        let cwd_metadata = fs::metadata(&self.cwd).map_err(|source| {
            PendingRelaunchPreflightError::WorkingDirectoryMetadata {
                path: self.cwd.clone(),
                source,
            }
        })?;
        if !cwd_metadata.is_dir() {
            return Err(PendingRelaunchPreflightError::WorkingDirectoryNotDirectory(
                self.cwd.clone(),
            ));
        }
        Ok(())
    }

    pub(crate) fn mark_terminal_restored(&mut self) -> Result<(), PendingRelaunchError> {
        if self.phase != RelaunchPhase::Interactive {
            return Err(PendingRelaunchError::InvalidLifecycleTransition);
        }
        self.phase = RelaunchPhase::TerminalRestored;
        Ok(())
    }

    pub(crate) fn mark_runtime_stopped(&mut self) -> Result<(), PendingRelaunchError> {
        if self.phase != RelaunchPhase::TerminalRestored {
            return Err(PendingRelaunchError::InvalidLifecycleTransition);
        }
        self.phase = RelaunchPhase::RuntimeStopped;
        Ok(())
    }

    fn command_spec(&self) -> Result<RelaunchCommand, PendingRelaunchError> {
        if self.phase != RelaunchPhase::RuntimeStopped {
            return Err(PendingRelaunchError::InvalidLifecycleTransition);
        }
        let mut args = vec![OsString::from("--resume"), OsString::from(&self.session_id)];
        if let Some(draft) = &self.draft {
            args.push(OsString::from("--prefill"));
            args.push(OsString::from(draft));
        }
        Ok(RelaunchCommand {
            binary: self.relaunch_binary.clone(),
            args,
            cwd: self.cwd.clone(),
        })
    }

    pub(crate) fn spawn(self) -> Result<Child, PendingRelaunchSpawnError> {
        let target_version = self.target_version.clone();
        let spec = self
            .command_spec()
            .map_err(PendingRelaunchSpawnError::Lifecycle)?;
        spec.spawn()
            .map_err(|source| PendingRelaunchSpawnError::Spawn {
                target_version,
                binary: spec.binary,
                source,
            })
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PendingRelaunchSpawnError {
    #[error(transparent)]
    Lifecycle(#[from] PendingRelaunchError),
    #[error(
        "could not start CrabCode {target_version} from {}: {source}",
        binary.display()
    )]
    Spawn {
        target_version: String,
        binary: PathBuf,
        source: io::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelaunchCommand {
    binary: PathBuf,
    args: Vec<OsString>,
    cwd: PathBuf,
}

impl RelaunchCommand {
    fn spawn(&self) -> io::Result<Child> {
        Command::new(&self.binary)
            .args(&self.args)
            .current_dir(&self.cwd)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
    }
}

#[derive(Debug)]
pub(crate) struct PendingRelaunchMonitor {
    marker_path_override: Option<PathBuf>,
    next_check: Instant,
}

impl PendingRelaunchMonitor {
    pub(crate) fn from_process(now: Instant) -> Self {
        Self {
            // Resolve on every poll, matching the TypeScript reader. If an
            // existing state-root symlink is repointed while this process is
            // alive, the reader and installer still converge on one path.
            marker_path_override: None,
            next_check: now,
        }
    }

    pub(crate) const fn deadline(&self) -> Option<Instant> {
        Some(self.next_check)
    }

    pub(crate) fn poll(
        &mut self,
        now: Instant,
        current_version: &str,
        facts: DirectIdleFacts<'_>,
        draft: &str,
        cwd: &Path,
    ) -> Option<PendingRelaunch> {
        if now < self.next_check {
            return None;
        }
        self.next_check = now + POLL_INTERVAL;
        let marker_path = self
            .marker_path_override
            .clone()
            .map_or_else(pending_relaunch_marker_path, Ok)
            .ok()?;
        let marker = read_pending_relaunch_marker(&marker_path).ok()??;
        if marker.target_version == current_version {
            return None;
        }
        let session_id = facts.exact_idle_session_id()?;
        Some(PendingRelaunch::new(marker, session_id, draft, cwd))
    }
}

fn pending_relaunch_marker_path() -> Result<PathBuf, PendingRelaunchError> {
    let root = if let Some(config_dir) = unicode_environment("CRABCODE_CONFIG_DIR")? {
        canonical_runtime_state_root("CRABCODE_CONFIG_DIR", PathBuf::from(config_dir))?
    } else if let Some(crabcode_home) = unicode_environment("CRABCODE_HOME")? {
        canonical_runtime_state_root(
            "CRABCODE_HOME/fallback home",
            PathBuf::from(crabcode_home).join(".crabcode"),
        )?
    } else {
        let fallback_home = dirs::home_dir().ok_or(PendingRelaunchError::MissingHome)?;
        canonical_runtime_state_root(
            "CRABCODE_HOME/fallback home",
            fallback_home.join(".crabcode"),
        )?
    };
    Ok(root.join("run").join(PENDING_RELAUNCH_MARKER))
}

fn unicode_environment(name: &'static str) -> Result<Option<String>, PendingRelaunchError> {
    match env::var(name) {
        Ok(value) if value.is_empty() => Ok(None),
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => {
            Err(PendingRelaunchError::NonUnicodeEnvironment { name })
        }
    }
}

#[cfg(test)]
fn runtime_state_root(
    config_dir: Option<&str>,
    crabcode_home: Option<&str>,
    fallback_home: &Path,
) -> Result<PathBuf, PendingRelaunchError> {
    let (name, candidate) = if let Some(config_dir) = config_dir.filter(|value| !value.is_empty()) {
        ("CRABCODE_CONFIG_DIR", PathBuf::from(config_dir))
    } else {
        let home = crabcode_home
            .filter(|value| !value.is_empty())
            .map_or_else(|| fallback_home.to_path_buf(), PathBuf::from);
        ("CRABCODE_HOME/fallback home", home.join(".crabcode"))
    };
    canonical_runtime_state_root(name, candidate)
}

fn canonical_runtime_state_root(
    name: &'static str,
    candidate: PathBuf,
) -> Result<PathBuf, PendingRelaunchError> {
    if !candidate.is_absolute() {
        return Err(PendingRelaunchError::RelativeStateRoot {
            name,
            value: candidate.display().to_string(),
        });
    }
    canonicalize_runtime_state_root(&candidate)
}

fn canonicalize_runtime_state_root(path: &Path) -> Result<PathBuf, PendingRelaunchError> {
    let mut lexical = normalize_absolute(path);
    for _ in 0..MAX_STATE_ROOT_SYMLINK_HOPS {
        let components = lexical
            .components()
            .map(|component| component.as_os_str().to_os_string())
            .collect::<Vec<_>>();
        let mut prefix = PathBuf::new();
        let mut followed_link = false;

        for (index, component) in components.iter().enumerate() {
            prefix.push(component);
            if !matches!(lexical.components().nth(index), Some(Component::Normal(_))) {
                continue;
            }
            let metadata = match fs::symlink_metadata(&prefix) {
                Ok(metadata) => metadata,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                    ) =>
                {
                    return Ok(lexical);
                }
                Err(error) => {
                    return Err(PendingRelaunchError::MarkerMetadata {
                        path: prefix,
                        source: error,
                    });
                }
            };
            if metadata.file_type().is_symlink() {
                let target = fs::read_link(&prefix).map_err(|source| {
                    PendingRelaunchError::MarkerMetadata {
                        path: prefix.clone(),
                        source,
                    }
                })?;
                let resolved_target = if target.is_absolute() {
                    target
                } else {
                    prefix
                        .parent()
                        .unwrap_or_else(|| Path::new(std::path::MAIN_SEPARATOR_STR))
                        .join(target)
                };
                let mut replacement = resolved_target;
                for remaining in &components[index + 1..] {
                    replacement.push(remaining);
                }
                lexical = normalize_absolute(&replacement);
                followed_link = true;
                break;
            }
        }
        if !followed_link {
            return Ok(lexical);
        }
    }
    Err(PendingRelaunchError::TooManySymlinks(path.to_path_buf()))
}

fn normalize_absolute(path: &Path) -> PathBuf {
    debug_assert!(path.is_absolute());
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(std::path::MAIN_SEPARATOR_STR),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn read_pending_relaunch_marker(
    path: &Path,
) -> Result<Option<PendingRelaunchMarker>, PendingRelaunchError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(PendingRelaunchError::MarkerMetadata {
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Err(PendingRelaunchError::MarkerNotRegular(path.to_path_buf()));
    }
    let bytes = fs::read(path).map_err(|source| PendingRelaunchError::MarkerRead {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice::<PendingRelaunchMarker>(&bytes)
        .map_err(PendingRelaunchError::MarkerJson)?
        .validate()
        .map(Some)
}

fn is_canonical_session_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 36
        && [8, 13, 18, 23]
            .into_iter()
            .all(|index| bytes[index] == b'-')
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| [8, 13, 18, 23].contains(&index) || byte.is_ascii_hexdigit())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    const SESSION_ID: &str = "10000000-0000-4000-8000-000000000001";

    fn marker(binary: &str, target_version: &str) -> PendingRelaunchMarker {
        PendingRelaunchMarker {
            target_version: target_version.to_string(),
            relaunch_binary: binary.to_string(),
            written_at: 1,
            writer_pid: 1,
        }
    }

    fn idle_facts() -> DirectIdleFacts<'static> {
        DirectIdleFacts {
            busy: false,
            session_id: Some(SESSION_ID),
            session_state: Some("idle"),
            active_task_count: 0,
            processing_background_task: false,
            fatal: false,
        }
    }

    fn write_marker(path: &Path, value: serde_json::Value) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }

    fn write_executable_fixture(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn runtime_state_root_matches_typescript_precedence_and_cold_suffix() {
        let temp = tempfile::tempdir().unwrap();
        let canonical_temp = fs::canonicalize(temp.path()).unwrap();
        let config = temp.path().join("config").join("missing");
        let home = temp.path().join("home");
        assert_eq!(
            runtime_state_root(
                Some(config.to_str().unwrap()),
                Some("relative-lower-priority-home-must-not-win"),
                Path::new("relative-lower-priority-fallback-must-not-win"),
            )
            .unwrap(),
            canonical_temp.join("config").join("missing")
        );
        assert_eq!(
            runtime_state_root(None, Some(home.to_str().unwrap()), temp.path()).unwrap(),
            canonical_temp.join("home").join(".crabcode")
        );
        assert!(runtime_state_root(Some("relative"), None, temp.path()).is_err());
        assert!(runtime_state_root(None, Some("relative"), temp.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_state_root_follows_each_symlink_and_keeps_missing_suffix() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        fs::create_dir(&real).unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        symlink(&real, &first).unwrap();
        symlink(&first, &second).unwrap();
        let source = second.join("cold").join("suffix");
        assert_eq!(
            canonicalize_runtime_state_root(&source).unwrap(),
            fs::canonicalize(&real).unwrap().join("cold").join("suffix")
        );
    }

    #[cfg(unix)]
    #[test]
    fn runtime_state_root_rejects_a_symlink_cycle_at_the_exact_hop_bound() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        symlink(&second, &first).unwrap();
        symlink(&first, &second).unwrap();
        assert!(matches!(
            canonicalize_runtime_state_root(&first),
            Err(PendingRelaunchError::TooManySymlinks(path)) if path == first
        ));
    }

    #[test]
    fn marker_parser_is_exact_and_accepts_a_stable_absolute_symlink_path() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(PENDING_RELAUNCH_MARKER);
        let stable_binary = temp.path().join("bin").join("crabcode");
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            fs::create_dir_all(stable_binary.parent().unwrap()).unwrap();
            let versioned_binary = temp.path().join("versions").join("2.0.0");
            fs::create_dir_all(versioned_binary.parent().unwrap()).unwrap();
            fs::write(&versioned_binary, b"fixture").unwrap();
            symlink(&versioned_binary, &stable_binary).unwrap();
        }
        write_marker(
            &path,
            serde_json::json!({
                "targetVersion": "2.0.0",
                "relaunchBinary": stable_binary,
                "writtenAt": SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64,
                "writerPid": 7
            }),
        );
        let parsed = read_pending_relaunch_marker(&path)
            .unwrap()
            .expect("marker");
        assert_eq!(parsed.target_version, "2.0.0");
        assert_eq!(
            parsed.relaunch_binary,
            stable_binary.to_str().unwrap(),
            "the stable launcher path is retained exactly and is not canonicalized"
        );

        for invalid in [
            serde_json::json!({
                "targetVersion": "2.0.0",
                "relaunchBinary": "relative/crabcode",
                "writtenAt": 1,
                "writerPid": 1
            }),
            serde_json::json!({
                "targetVersion": " 2.0.0 ",
                "relaunchBinary": stable_binary,
                "writtenAt": 1,
                "writerPid": 1
            }),
            serde_json::json!({
                "targetVersion": "2.0.0",
                "relaunchBinary": stable_binary,
                "writtenAt": 0,
                "writerPid": 1
            }),
            serde_json::json!({
                "targetVersion": "2.0.0",
                "relaunchBinary": stable_binary,
                "writtenAt": 1,
                "writerPid": 1,
                "unexpected": true
            }),
        ] {
            write_marker(&path, invalid);
            assert!(read_pending_relaunch_marker(&path).is_err());
        }
    }

    #[test]
    fn marker_targeting_current_version_is_deferred_and_retained() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(PENDING_RELAUNCH_MARKER);
        write_marker(
            &path,
            serde_json::json!({
                "targetVersion": "1.0.20",
                "relaunchBinary": temp.path().join("crabcode"),
                "writtenAt": 1,
                "writerPid": 1
            }),
        );
        let now = Instant::now();
        let mut monitor = PendingRelaunchMonitor {
            marker_path_override: Some(path.clone()),
            next_check: now,
        };
        assert!(
            monitor
                .poll(now, "1.0.20", idle_facts(), "", temp.path())
                .is_none()
        );
        assert!(
            path.exists(),
            "multi-window loop guard must not delete marker"
        );
    }

    #[test]
    fn spawn_failure_reports_the_target_and_preserves_the_stable_binary_path() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("missing").join("crabcode");
        let mut request = PendingRelaunch::new(
            marker(binary.to_str().unwrap(), "2.0.0"),
            SESSION_ID.to_string(),
            "",
            temp.path(),
        );
        request.mark_terminal_restored().unwrap();
        request.mark_runtime_stopped().unwrap();
        let error = request.spawn().unwrap_err().to_string();
        assert!(error.contains("2.0.0"));
        assert!(error.contains(binary.to_str().unwrap()));
    }

    #[test]
    fn preflight_rejects_deterministic_binary_and_working_directory_failures() {
        let temp = tempfile::tempdir().unwrap();
        let missing_binary = temp.path().join("missing").join("crabcode");
        let request = PendingRelaunch::new(
            marker(missing_binary.to_str().unwrap(), "2.0.0"),
            SESSION_ID.to_string(),
            "",
            temp.path(),
        );
        assert!(matches!(
            request.preflight(),
            Err(PendingRelaunchPreflightError::BinaryMetadata { path, .. })
                if path == missing_binary
        ));

        let binary_directory = temp.path().join("binary-directory");
        fs::create_dir(&binary_directory).unwrap();
        let request = PendingRelaunch::new(
            marker(binary_directory.to_str().unwrap(), "2.0.0"),
            SESSION_ID.to_string(),
            "",
            temp.path(),
        );
        assert!(matches!(
            request.preflight(),
            Err(PendingRelaunchPreflightError::BinaryNotRegular(path))
                if path == binary_directory
        ));

        let executable = temp.path().join("stable").join("crabcode");
        write_executable_fixture(&executable);
        let cwd_file = temp.path().join("not-a-directory");
        fs::write(&cwd_file, b"fixture").unwrap();
        let request = PendingRelaunch::new(
            marker(executable.to_str().unwrap(), "2.0.0"),
            SESSION_ID.to_string(),
            "",
            &cwd_file,
        );
        assert!(matches!(
            request.preflight(),
            Err(PendingRelaunchPreflightError::WorkingDirectoryNotDirectory(path))
                if path == cwd_file
        ));

        let missing_cwd = temp.path().join("missing-cwd");
        let request = PendingRelaunch::new(
            marker(executable.to_str().unwrap(), "2.0.0"),
            SESSION_ID.to_string(),
            "",
            &missing_cwd,
        );
        assert!(matches!(
            request.preflight(),
            Err(PendingRelaunchPreflightError::WorkingDirectoryMetadata { path, .. })
                if path == missing_cwd
        ));
    }

    #[cfg(unix)]
    #[test]
    fn preflight_follows_but_does_not_replace_the_stable_launcher_symlink() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let temp = tempfile::tempdir().unwrap();
        let versioned_binary = temp.path().join("versions").join("2.0.0");
        write_executable_fixture(&versioned_binary);
        let stable_binary = temp.path().join("bin").join("crabcode");
        fs::create_dir_all(stable_binary.parent().unwrap()).unwrap();
        symlink(&versioned_binary, &stable_binary).unwrap();

        let mut request = PendingRelaunch::new(
            marker(stable_binary.to_str().unwrap(), "2.0.0"),
            SESSION_ID.to_string(),
            "",
            temp.path(),
        );
        request.preflight().unwrap();
        request.mark_terminal_restored().unwrap();
        request.mark_runtime_stopped().unwrap();
        assert_eq!(
            request.command_spec().unwrap().binary,
            stable_binary,
            "preflight must retain the stable launcher path as the spawn target"
        );

        fs::set_permissions(&versioned_binary, fs::Permissions::from_mode(0o644)).unwrap();
        let request = PendingRelaunch::new(
            marker(stable_binary.to_str().unwrap(), "2.0.0"),
            SESSION_ID.to_string(),
            "",
            temp.path(),
        );
        assert!(matches!(
            request.preflight(),
            Err(PendingRelaunchPreflightError::BinaryNotExecutable(path))
                if path == stable_binary
        ));
    }

    #[test]
    fn every_unknown_or_active_idle_fact_defers_relaunch() {
        let base = idle_facts();
        let cases = [
            DirectIdleFacts { busy: true, ..base },
            DirectIdleFacts {
                session_id: None,
                ..base
            },
            DirectIdleFacts {
                session_id: Some("not-a-canonical-session"),
                ..base
            },
            DirectIdleFacts {
                session_state: None,
                ..base
            },
            DirectIdleFacts {
                session_state: Some("future-unknown-state"),
                ..base
            },
            DirectIdleFacts {
                active_task_count: 1,
                ..base
            },
            DirectIdleFacts {
                processing_background_task: true,
                ..base
            },
            DirectIdleFacts {
                fatal: true,
                ..base
            },
        ];
        for facts in cases {
            assert!(facts.exact_idle_session_id().is_none());
        }
        assert_eq!(base.exact_idle_session_id().as_deref(), Some(SESSION_ID));
    }

    #[test]
    fn spawn_argv_is_exact_and_phase_fenced_after_terminal_and_runtime_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("stable").join("crabcode");
        let mut request = PendingRelaunch::new(
            marker(binary.to_str().unwrap(), "2.0.0"),
            SESSION_ID.to_string(),
            "  exact draft  ",
            temp.path(),
        );
        assert!(request.command_spec().is_err());
        request.mark_terminal_restored().unwrap();
        assert!(request.command_spec().is_err());
        request.mark_runtime_stopped().unwrap();
        let command = request.command_spec().unwrap();
        assert_eq!(command.binary, binary);
        assert_eq!(
            command.args,
            [
                OsString::from("--resume"),
                OsString::from(SESSION_ID),
                OsString::from("--prefill"),
                OsString::from("exact draft"),
            ]
        );
        assert_eq!(command.cwd, temp.path());
    }

    #[test]
    fn empty_draft_does_not_add_prefill() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = PendingRelaunch::new(
            marker("/absolute/crabcode", "2.0.0"),
            SESSION_ID.to_string(),
            " \n\t ",
            temp.path(),
        );
        request.mark_terminal_restored().unwrap();
        request.mark_runtime_stopped().unwrap();
        assert_eq!(
            request.command_spec().unwrap().args,
            [OsString::from("--resume"), OsString::from(SESSION_ID),]
        );
    }

    #[test]
    fn spawn_argv_trims_bom_but_preserves_next_line_like_ecmascript() {
        let temp = tempfile::tempdir().unwrap();
        let mut request = PendingRelaunch::new(
            marker("/absolute/crabcode", "2.0.0"),
            SESSION_ID.to_string(),
            "\u{feff} \u{0085}exact draft\u{0085} \u{feff}",
            temp.path(),
        );
        request.mark_terminal_restored().unwrap();
        request.mark_runtime_stopped().unwrap();
        assert_eq!(
            request.command_spec().unwrap().args,
            [
                OsString::from("--resume"),
                OsString::from(SESSION_ID),
                OsString::from("--prefill"),
                OsString::from("\u{0085}exact draft\u{0085}"),
            ]
        );

        let mut bom_only = PendingRelaunch::new(
            marker("/absolute/crabcode", "2.0.0"),
            SESSION_ID.to_string(),
            "\u{feff}\t\u{feff}",
            temp.path(),
        );
        bom_only.mark_terminal_restored().unwrap();
        bom_only.mark_runtime_stopped().unwrap();
        assert_eq!(
            bom_only.command_spec().unwrap().args,
            [OsString::from("--resume"), OsString::from(SESSION_ID),]
        );
    }
}

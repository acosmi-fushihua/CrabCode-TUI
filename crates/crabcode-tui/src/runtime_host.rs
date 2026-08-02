//! Process-owned adapter to CrabCode's existing StructuredIO/QueryEngine path.
//!
//! The child is a private, piped SDK process. There is no daemon discovery,
//! socket, browser surface, second renderer, or alternate backend in this
//! module.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Notify;

use crate::sdk_runtime::{
    OutboundCompletion, OutboundDeliveryId, OutboundSubmitError, RuntimeConfig, RuntimeEvent,
    SdkRuntime, SendError, ShutdownError, SpawnError, TransportLimits,
};

#[cfg(feature = "terminal-lifecycle-tests")]
const RUNTIME_SCRIPT_ENV: &str = "CRABCODE_TUI_RUNTIME_SCRIPT";
#[cfg(feature = "terminal-lifecycle-tests")]
const BUN_BIN_ENV: &str = "CRABCODE_TUI_BUN";
const TEAMMATE_COMMAND_ENV: &str = "CRABCODE_TEAMMATE_COMMAND";
const DESKTOP_AUTOMATION_ENV: &str = "CRABCODE_DESKTOP_AUTOMATION";
const DESKTOP_AUTOMATION_WRITES_ENV: &str = "CRABCODE_DESKTOP_AUTOMATION_WRITES";
const DESKTOP_CAPTURE_ENV: &str = "CRABCODE_DESKTOP_CAPTURE";
const DESKTOP_VISUAL_SIDECAR_ENV: &str = "CRABCODE_DESKTOP_VISUAL_SIDECAR";
const BROWSER_DEFAULT_SURFACE_ENV: &str = "CRABCODE_BROWSER_DEFAULT_SURFACE";
const BROWSER_EMBED_ATTACH_ENV: &str = "CRABCODE_BROWSER_EMBED_ATTACH";
const EMBED_BROWSER_CDP_ENV: &str = "CRABCODE_EMBED_BROWSER_CDP";
const RUNTIME_DIST_DIR: &str = "dist";
const RUNTIME_BUNDLE_DIR: &str = "tui-runtime";
const RUNTIME_ENTRY_FILE: &str = "index.js";
pub const INITIALIZE_REQUEST_ID: &str = "crabcode-tui-initialize";
const CRABCODE_TUI_SETUP_SUBTYPE: &str = "crabcode_tui_setup";

#[derive(Debug, Error)]
pub enum HostStartError {
    #[error("failed to resolve runtime workspace: {0}")]
    Workspace(#[source] std::io::Error),
    #[error("failed to resolve the native CrabCode TUI executable: {0}")]
    Executable(#[source] std::io::Error),
    #[error("runtime workspace is not a directory: {0}")]
    InvalidWorkspace(PathBuf),
    #[error("CrabCode runtime bundle is unavailable: {0}")]
    RuntimeBundle(String),
    #[error(transparent)]
    Spawn(#[from] SpawnError),
}

pub struct RuntimeHost {
    runtime: SdkRuntime,
    next_request_id: AtomicU64,
}

impl RuntimeHost {
    /// Spawn the private direct runtime without consuming any stdout setup
    /// request or sending the SDK initialize request.
    ///
    /// The product uses this split point to take terminal ownership only
    /// after executable/bundle/process validation has succeeded, while still
    /// rendering every pre-initialize setup interaction through the native
    /// lifecycle. No backend request is synthesized by this constructor.
    pub(crate) fn spawn_uninitialized_in(
        runtime_args: Vec<OsString>,
        cwd: PathBuf,
    ) -> Result<(Self, PathBuf), HostStartError> {
        let cwd = std::fs::canonicalize(cwd).map_err(HostStartError::Workspace)?;
        if !cwd.is_dir() {
            return Err(HostStartError::InvalidWorkspace(cwd));
        }
        let teammate_command = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .map_err(HostStartError::Executable)?;
        let script = resolve_runtime_script()?;
        let program = resolve_bun_program(&script)?;
        let runtime_args = normalize_runtime_args(runtime_args);
        let (removed_environment, environment) =
            direct_runtime_environment(teammate_command.into_os_string());
        let runtime = SdkRuntime::spawn(RuntimeConfig {
            program,
            script,
            cwd: cwd.clone(),
            runtime_args,
            removed_environment,
            environment,
            limits: TransportLimits::default(),
        })?;
        Ok((
            Self {
                runtime,
                next_request_id: AtomicU64::new(1),
            },
            cwd,
        ))
    }

    pub fn try_recv_event(&self) -> Result<RuntimeEvent, std::sync::mpsc::TryRecvError> {
        self.runtime.try_recv_event()
    }

    pub(crate) fn event_notifier(&self) -> Arc<Notify> {
        self.runtime.event_notifier()
    }

    pub fn try_recv_stderr(
        &self,
    ) -> Result<crate::sdk_runtime::StderrFrame, std::sync::mpsc::TryRecvError> {
        self.runtime.try_recv_stderr()
    }

    pub(crate) fn stderr_notifier(&self) -> Arc<Notify> {
        self.runtime.stderr_notifier()
    }

    pub fn outbound_notifier(&self) -> Arc<Notify> {
        self.runtime.outbound_notifier()
    }

    pub fn next_outbound_deadline(&self) -> Option<Instant> {
        self.runtime.next_outbound_deadline()
    }

    pub fn has_nonblocking_outbound_work(&self) -> bool {
        self.runtime.has_nonblocking_outbound_work()
    }

    pub fn progress_nonblocking_outbound(&self) -> bool {
        self.runtime.progress_nonblocking_outbound()
    }

    pub fn try_recv_outbound_completion(&self) -> Option<OutboundCompletion> {
        self.runtime.try_recv_outbound_completion()
    }

    pub fn abort_nonblocking(&self, reason: String) {
        self.runtime.abort_nonblocking(reason);
    }

    pub fn submit_user_content(
        &self,
        content: Value,
        priority: Option<&str>,
    ) -> Result<OutboundDeliveryId, OutboundSubmitError> {
        let mut envelope = serde_json::Map::new();
        envelope.insert("type".to_string(), Value::String("user".to_string()));
        envelope.insert(
            "message".to_string(),
            json!({"role": "user", "content": content}),
        );
        envelope.insert("parent_tool_use_id".to_string(), Value::Null);
        if let Some(priority) = priority {
            envelope.insert("priority".to_string(), Value::String(priority.to_string()));
        }
        self.runtime.submit_user_message(Value::Object(envelope))
    }

    pub fn submit_initialize(&self) -> Result<OutboundDeliveryId, OutboundSubmitError> {
        self.runtime
            .submit_control_request(INITIALIZE_REQUEST_ID, initialize_request_payload())
    }

    pub fn submit_control(
        &self,
        request: Value,
    ) -> Result<(String, OutboundDeliveryId), OutboundSubmitError> {
        let request_id = self.next_control_id();
        let delivery_id = self
            .runtime
            .submit_control_request(request_id.clone(), request)?;
        Ok((request_id, delivery_id))
    }

    pub fn submit_private_runtime_action(
        &self,
        request_id: &str,
        action: Value,
    ) -> Result<OutboundDeliveryId, OutboundSubmitError> {
        self.runtime
            .submit_private_runtime_action(request_id.to_string(), action)
    }

    pub fn submit_interrupt(&self) -> Result<(String, OutboundDeliveryId), OutboundSubmitError> {
        let request_id = self.next_control_id();
        let delivery_id = self.runtime.submit_interrupt(request_id.clone())?;
        Ok((request_id, delivery_id))
    }

    pub fn submit_permission_response(
        &self,
        request_id: &str,
        response: Value,
    ) -> Result<OutboundDeliveryId, OutboundSubmitError> {
        self.runtime
            .submit_permission_response(request_id, response)
    }

    pub fn submit_elicitation_response(
        &self,
        request_id: &str,
        response: Value,
    ) -> Result<OutboundDeliveryId, OutboundSubmitError> {
        self.runtime
            .submit_elicitation_response(request_id, response)
    }

    pub fn submit_startup_interaction_response(
        &self,
        request_id: &str,
        subtype: &str,
        response: Value,
    ) -> Result<OutboundDeliveryId, OutboundSubmitError> {
        if subtype != CRABCODE_TUI_SETUP_SUBTYPE {
            return Err(SendError::InvalidEnvelope(format!(
                "unsupported native TUI startup interaction subtype `{subtype}`"
            ))
            .into());
        }
        self.runtime
            .submit_control_success(request_id, subtype, response)
    }

    pub fn shutdown(&mut self, reason: Option<&str>) -> Result<(), ShutdownError> {
        let request_id = self.next_control_id();
        self.runtime.shutdown(request_id, reason)
    }

    /// Reap the private child before its setup router has handed stdin to
    /// StructuredIO. No backend envelope is valid at this boundary.
    pub(crate) fn shutdown_before_runtime_handoff(&mut self) -> Result<(), ShutdownError> {
        self.runtime.shutdown_before_runtime_handoff()
    }

    fn next_control_id(&self) -> String {
        let sequence = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        format!("crabcode-tui-{sequence}")
    }
}

fn direct_runtime_environment(
    teammate_command: OsString,
) -> (Vec<OsString>, Vec<(OsString, OsString)>) {
    (
        vec![
            OsString::from(TEAMMATE_COMMAND_ENV),
            OsString::from(DESKTOP_AUTOMATION_WRITES_ENV),
            OsString::from(DESKTOP_CAPTURE_ENV),
            OsString::from(DESKTOP_VISUAL_SIDECAR_ENV),
            OsString::from(BROWSER_DEFAULT_SURFACE_ENV),
            OsString::from(BROWSER_EMBED_ATTACH_ENV),
            OsString::from(EMBED_BROWSER_CDP_ENV),
        ],
        vec![
            (OsString::from(TEAMMATE_COMMAND_ENV), teammate_command),
            (OsString::from(DESKTOP_AUTOMATION_ENV), OsString::from("0")),
            (
                OsString::from(BROWSER_EMBED_ATTACH_ENV),
                OsString::from("0"),
            ),
            (OsString::from(EMBED_BROWSER_CDP_ENV), OsString::from("0")),
        ],
    )
}

fn initialize_request_payload() -> Value {
    json!({
        "subtype": "initialize",
        "promptSuggestions": true,
        "agentProgressSummaries": true
    })
}

fn resolve_runtime_script() -> Result<PathBuf, HostStartError> {
    #[cfg(feature = "terminal-lifecycle-tests")]
    if let Some(explicit) = std::env::var_os(RUNTIME_SCRIPT_ENV) {
        return validate_runtime_script(PathBuf::from(explicit), None);
    }
    let executable = std::env::current_exe()
        .map_err(HostStartError::Executable)
        .and_then(|executable| {
            std::fs::canonicalize(&executable).map_err(HostStartError::Executable)
        })?;
    let expected_root = executable.parent().ok_or_else(|| {
        HostStartError::RuntimeBundle(format!("{} has no install root", executable.display()))
    })?;
    let Some(found) = runtime_script_candidate(&executable).filter(|candidate| candidate.is_file())
    else {
        return Err(HostStartError::RuntimeBundle(format!(
            "install {RUNTIME_DIST_DIR}/{RUNTIME_BUNDLE_DIR}/{RUNTIME_ENTRY_FILE} in the verified CrabCode executable tree"
        )));
    };
    validate_runtime_script(found, Some(expected_root))
}

fn runtime_script_candidate(executable: &Path) -> Option<PathBuf> {
    Some(
        executable
            .parent()?
            .join(RUNTIME_DIST_DIR)
            .join(RUNTIME_BUNDLE_DIR)
            .join(RUNTIME_ENTRY_FILE),
    )
}

fn validate_runtime_script(
    path: PathBuf,
    expected_root: Option<&Path>,
) -> Result<PathBuf, HostStartError> {
    if !path.is_file() {
        return Err(HostStartError::RuntimeBundle(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let canonical = std::fs::canonicalize(&path).map_err(|error| {
        HostStartError::RuntimeBundle(format!("cannot resolve {}: {error}", path.display()))
    })?;
    let actual_root = runtime_install_root(&canonical).ok_or_else(|| {
        HostStartError::RuntimeBundle(format!(
            "{} is outside the required {RUNTIME_DIST_DIR}/{RUNTIME_BUNDLE_DIR}/{RUNTIME_ENTRY_FILE} layout",
            canonical.display()
        ))
    })?;
    if expected_root.is_some_and(|expected| actual_root != expected) {
        return Err(HostStartError::RuntimeBundle(format!(
            "{} resolves outside executable install root {}",
            path.display(),
            expected_root.expect("checked expected root").display()
        )));
    }
    Ok(canonical)
}

fn runtime_install_root(script: &Path) -> Option<&Path> {
    if script.file_name()? != RUNTIME_ENTRY_FILE {
        return None;
    }
    let runtime_dir = script.parent()?;
    if runtime_dir.file_name()? != RUNTIME_BUNDLE_DIR {
        return None;
    }
    let dist_dir = runtime_dir.parent()?;
    if dist_dir.file_name()? != RUNTIME_DIST_DIR {
        return None;
    }
    dist_dir.parent()
}

fn resolve_bun_program(runtime_script: &Path) -> Result<PathBuf, HostStartError> {
    #[cfg(feature = "terminal-lifecycle-tests")]
    let explicit = std::env::var_os(BUN_BIN_ENV);
    #[cfg(not(feature = "terminal-lifecycle-tests"))]
    let explicit = None;
    resolve_bun_program_from(runtime_script, explicit)
}

fn resolve_bun_program_from(
    runtime_script: &Path,
    explicit: Option<OsString>,
) -> Result<PathBuf, HostStartError> {
    if let Some(explicit) = explicit {
        let explicit = PathBuf::from(explicit);
        if !explicit.is_file() {
            return Err(HostStartError::RuntimeBundle(format!(
                "test runtime executable {} is not a regular file",
                explicit.display()
            )));
        }
        return std::fs::canonicalize(&explicit).map_err(|error| {
            HostStartError::RuntimeBundle(format!(
                "cannot resolve test runtime executable {}: {error}",
                explicit.display()
            ))
        });
    }
    let binary = if cfg!(windows) { "bun.exe" } else { "bun" };
    let root = runtime_install_root(runtime_script).ok_or_else(|| {
        HostStartError::RuntimeBundle(format!(
            "{} has no verified install root",
            runtime_script.display()
        ))
    })?;
    let canonical_root = std::fs::canonicalize(root).map_err(|error| {
        HostStartError::RuntimeBundle(format!(
            "cannot resolve verified CrabCode TUI install root {}: {error}",
            root.display()
        ))
    })?;
    let candidate = root.join(binary);
    if !candidate.is_file() {
        return Err(HostStartError::RuntimeBundle(format!(
            "verified CrabCode TUI runtime requires sibling {}",
            candidate.display()
        )));
    }
    let canonical = std::fs::canonicalize(&candidate).map_err(|error| {
        HostStartError::RuntimeBundle(format!(
            "cannot resolve bundled runtime executable {}: {error}",
            candidate.display()
        ))
    })?;
    if canonical.parent() != Some(canonical_root.as_path()) {
        return Err(HostStartError::RuntimeBundle(format!(
            "bundled runtime executable {} resolves outside {}",
            candidate.display(),
            root.display()
        )));
    }
    Ok(canonical)
}

fn normalize_runtime_args(args: impl IntoIterator<Item = impl Into<OsString>>) -> Vec<OsString> {
    args.into_iter()
        .map(Into::into)
        // Verbose SDK output is a fixed part of the lossless transport.
        .filter(|argument| argument.to_str() != Some("--verbose"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_payload_is_the_existing_sdk_request() {
        assert_eq!(
            initialize_request_payload(),
            json!({
                "subtype": "initialize",
                "promptSuggestions": true,
                "agentProgressSummaries": true
            })
        );
    }

    #[test]
    fn redundant_verbose_is_removed_but_backend_flags_are_byte_preserved() {
        let args = normalize_runtime_args([
            OsString::from("--verbose"),
            OsString::from("--model"),
            OsString::from("account:best"),
            OsString::from("--mcp-config"),
            OsString::from("a.json"),
        ]);
        assert_eq!(
            args,
            [
                OsString::from("--model"),
                OsString::from("account:best"),
                OsString::from("--mcp-config"),
                OsString::from("a.json"),
            ]
        );
    }

    #[test]
    fn direct_runtime_denies_gui_owned_automation_without_widening_the_wire() {
        let teammate = OsString::from("/opt/crabcode/crabcode-tui");
        let (removed, injected) = direct_runtime_environment(teammate.clone());

        assert_eq!(
            removed,
            [
                OsString::from(TEAMMATE_COMMAND_ENV),
                OsString::from(DESKTOP_AUTOMATION_WRITES_ENV),
                OsString::from(DESKTOP_CAPTURE_ENV),
                OsString::from(DESKTOP_VISUAL_SIDECAR_ENV),
                OsString::from(BROWSER_DEFAULT_SURFACE_ENV),
                OsString::from(BROWSER_EMBED_ATTACH_ENV),
                OsString::from(EMBED_BROWSER_CDP_ENV),
            ]
        );
        assert_eq!(
            injected,
            [
                (OsString::from(TEAMMATE_COMMAND_ENV), teammate),
                (OsString::from(DESKTOP_AUTOMATION_ENV), OsString::from("0"),),
                (
                    OsString::from(BROWSER_EMBED_ATTACH_ENV),
                    OsString::from("0"),
                ),
                (OsString::from(EMBED_BROWSER_CDP_ENV), OsString::from("0"),),
            ]
        );
    }

    #[test]
    fn runtime_candidates_are_anchored_to_the_executable_not_the_workspace() {
        let executable = Path::new("/opt/crabcode/crabcode-tui");
        assert_eq!(
            runtime_script_candidate(executable),
            Some(PathBuf::from("/opt/crabcode/dist/tui-runtime/index.js"))
        );
        assert_eq!(
            runtime_script_candidate(Path::new("/opt/crabcode/missing-generation/crabcode-tui")),
            Some(PathBuf::from(
                "/opt/crabcode/missing-generation/dist/tui-runtime/index.js"
            )),
            "a missing local bundle must never fall through to /opt/crabcode/dist or /opt/dist"
        );
    }

    #[test]
    fn dedicated_runtime_layout_resolves_the_install_root_and_bun_sibling() {
        let install = tempfile::tempdir().expect("temporary install root");
        let script = install
            .path()
            .join("dist")
            .join("tui-runtime")
            .join("index.js");
        let bundled_bun = install
            .path()
            .join(if cfg!(windows) { "bun.exe" } else { "bun" });
        std::fs::create_dir_all(script.parent().expect("runtime directory"))
            .expect("create runtime directory");
        std::fs::write(&script, b"runtime").expect("write runtime entry");
        std::fs::write(&bundled_bun, b"bun").expect("write bundled Bun");
        assert_eq!(runtime_install_root(&script), Some(install.path()));
        assert_eq!(
            resolve_bun_program_from(&script, None).expect("bundled Bun"),
            std::fs::canonicalize(&bundled_bun).expect("canonical bundled Bun"),
            "a dedicated runtime must resolve the packaged Bun without PATH"
        );
        let explicit_bun = install.path().join("fixture-bun");
        std::fs::write(&explicit_bun, b"fixture").expect("write explicit test runtime");
        assert_eq!(
            resolve_bun_program_from(&script, Some(explicit_bun.clone().into_os_string()))
                .expect("explicit test runtime"),
            std::fs::canonicalize(explicit_bun).expect("canonical explicit runtime")
        );
        std::fs::remove_file(&bundled_bun).expect("remove bundled Bun");
        assert!(
            resolve_bun_program_from(&script, None).is_err(),
            "a missing sibling Bun must not fall through to PATH"
        );
        assert!(runtime_install_root(Path::new("/opt/crabcode/dist/index.js")).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_and_bun_symlinks_cannot_escape_the_executable_generation() {
        use std::os::unix::fs::symlink;

        let expected = tempfile::tempdir().expect("expected install root");
        let foreign = tempfile::tempdir().expect("foreign install root");
        let expected_runtime_dir = expected.path().join("dist/tui-runtime");
        let foreign_runtime_dir = foreign.path().join("dist/tui-runtime");
        std::fs::create_dir_all(&expected_runtime_dir).expect("expected runtime directory");
        std::fs::create_dir_all(&foreign_runtime_dir).expect("foreign runtime directory");
        let foreign_script = foreign_runtime_dir.join("index.js");
        std::fs::write(&foreign_script, b"foreign").expect("foreign runtime");
        let linked_script = expected_runtime_dir.join("index.js");
        symlink(&foreign_script, &linked_script).expect("runtime symlink");
        assert!(
            validate_runtime_script(linked_script, Some(expected.path())).is_err(),
            "an install-tree symlink must not select another generation's shaped runtime"
        );

        std::fs::remove_file(expected_runtime_dir.join("index.js")).expect("remove runtime link");
        let expected_script = expected_runtime_dir.join("index.js");
        std::fs::write(&expected_script, b"expected").expect("expected runtime");
        let foreign_bun = foreign.path().join("bun");
        std::fs::write(&foreign_bun, b"foreign bun").expect("foreign Bun");
        symlink(&foreign_bun, expected.path().join("bun")).expect("Bun symlink");
        assert!(
            resolve_bun_program_from(&expected_script, None).is_err(),
            "the sibling Bun must resolve inside the same canonical generation"
        );
    }
}

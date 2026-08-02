//! Tmux Integration
//!
//! 等价于 TS tmuxSocket.ts
//!
//! Provides:
//! - Tmux availability detection
//! - Isolated tmux socket management (prevents `CrabCode` from affecting user sessions)
//! - Tmux session/pane management trait + basic implementation
//!
//! On Windows, tmux runs inside WSL; commands are routed through `wsl -e tmux ...`.
//! The `TmuxManager` trait is cfg-gated: on non-Unix platforms, only the trait
//! definition and a no-op stub are compiled.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

// ─── Tmux Session/Pane Types ───

/// Tmux session information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxSession {
    /// Session name
    pub name: String,
    /// Number of windows
    pub windows: u32,
    /// Whether this session is attached
    pub attached: bool,
}

/// Tmux pane information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TmuxPane {
    /// Pane ID (e.g., "%0")
    pub id: String,
    /// Pane index within the window
    pub index: u32,
    /// Whether this pane is active
    pub active: bool,
    /// Pane width in columns
    pub width: u32,
    /// Pane height in rows
    pub height: u32,
}

/// Split direction for pane creation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

// ─── TmuxManager Trait ───

/// Trait for tmux session management.
///
/// Implementations handle the actual tmux command execution. The trait
/// is designed to support both real tmux interaction (via subprocess)
/// and mock implementations for testing.
#[async_trait::async_trait]
pub trait TmuxManager: Send + Sync {
    /// Check if tmux is available on this system
    async fn is_available(&self) -> bool;

    /// Get the socket name for Acosmi's isolated tmux session
    fn socket_name(&self) -> &str;

    /// Mark that the Tmux tool has been used at least once.
    /// Called by `TungstenTool` before initialization.
    fn mark_tool_used(&self);

    /// Check if the Tmux tool has been used at least once.
    /// Used by shell providers to decide whether to initialize the socket.
    fn has_tool_been_used(&self) -> bool;

    /// Check if the isolated socket has been initialized
    fn is_initialized(&self) -> bool;

    /// Initialize the isolated tmux socket (creates a session named "base")
    async fn initialize(&self) -> Result<(), TmuxError>;

    /// Get the TMUX environment variable value for the isolated socket.
    /// Returns None if not yet initialized.
    /// Format: "`socket_path,server_pid,pane_index`"
    fn get_tmux_env(&self) -> Option<String>;

    /// Create a new tmux session
    async fn create_session(&self, name: &str) -> Result<(), TmuxError>;

    /// Check if a session exists
    async fn has_session(&self, name: &str) -> Result<bool, TmuxError>;

    /// Kill (destroy) a tmux session
    async fn kill_session(&self, name: &str) -> Result<(), TmuxError>;

    /// List all sessions on the isolated socket
    async fn list_sessions(&self) -> Result<Vec<TmuxSession>, TmuxError>;

    /// Split a pane
    async fn split_pane(
        &self,
        target: &str,
        direction: SplitDirection,
    ) -> Result<String, TmuxError>;

    /// Send keys to a pane
    async fn send_keys(&self, target: &str, keys: &str) -> Result<(), TmuxError>;

    /// Kill the tmux server for cleanup
    async fn kill_server(&self) -> Result<(), TmuxError>;
}

/// Tmux-specific errors
#[derive(Debug, thiserror::Error)]
pub enum TmuxError {
    #[error("tmux is not available on this system")]
    NotAvailable,

    #[error("tmux socket not initialized")]
    NotInitialized,

    #[error("tmux command failed (exit {code}): {stderr}")]
    CommandFailed { code: i32, stderr: String },

    #[error("failed to parse tmux output: {0}")]
    ParseError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

// ─── Socket State ───

/// Internal state for the tmux socket
struct SocketState {
    socket_path: Option<String>,
    server_pid: Option<u32>,
}

/// Acosmi's isolated tmux socket manager.
///
/// Creates and manages a tmux socket named `acosmi-<PID>` that is
/// isolated from the user's tmux sessions. All tmux commands run
/// through `CrabCode` use this socket via the `-L` flag.
pub struct AcosmiTmuxManager {
    /// Socket name (e.g., "acosmi-12345")
    socket_name: String,
    /// Cached availability result (initialized once, no TOCTOU race)
    availability: tokio::sync::OnceCell<bool>,
    /// Whether the Tmux tool has been used at least once
    tool_used: AtomicBool,
    /// Socket state (path + server PID)
    state: Mutex<SocketState>,
}

impl AcosmiTmuxManager {
    /// Create a new tmux manager with a socket name based on the current process ID
    #[must_use]
    pub fn new(pid: u32) -> Self {
        Self {
            socket_name: format!("acosmi-{pid}"),
            availability: tokio::sync::OnceCell::const_new(),
            tool_used: AtomicBool::new(false),
            state: Mutex::new(SocketState {
                socket_path: None,
                server_pid: None,
            }),
        }
    }

    /// Create with a custom socket name (for testing)
    pub fn with_socket_name(name: impl Into<String>) -> Self {
        Self {
            socket_name: name.into(),
            availability: tokio::sync::OnceCell::const_new(),
            tool_used: AtomicBool::new(false),
            state: Mutex::new(SocketState {
                socket_path: None,
                server_pid: None,
            }),
        }
    }

    /// Execute a tmux command via subprocess.
    ///
    /// On Windows, routes through `wsl -e tmux ...`.
    /// Returns (stdout, stderr, exit_code).
    #[cfg(unix)]
    async fn exec_tmux(&self, args: &[&str]) -> Result<(String, String, i32), std::io::Error> {
        let mut all_args = vec!["-L", &self.socket_name];
        all_args.extend_from_slice(args);

        let output = tokio::process::Command::new("tmux")
            .args(&all_args)
            .output()
            .await?;

        Ok((
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
            output.status.code().unwrap_or(1),
        ))
    }

    /// Execute a tmux command via WSL on Windows.
    #[cfg(windows)]
    async fn exec_tmux(&self, args: &[&str]) -> Result<(String, String, i32), std::io::Error> {
        let mut wsl_args = vec!["-e", "tmux", "-L", &self.socket_name];
        wsl_args.extend_from_slice(args);

        let output = tokio::process::Command::new("wsl")
            .args(&wsl_args)
            .env("WSL_UTF8", "1")
            .output()
            .await?;

        Ok((
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
            output.status.code().unwrap_or(1),
        ))
    }

    /// Set socket info after initialization
    fn set_socket_info(&self, path: String, pid: u32) {
        let mut state = self
            .state
            .lock()
            .expect("TmuxProvider state mutex poisoned");
        state.socket_path = Some(path);
        state.server_pid = Some(pid);
    }
}

#[async_trait::async_trait]
impl TmuxManager for AcosmiTmuxManager {
    async fn is_available(&self) -> bool {
        *self
            .availability
            .get_or_init(|| async {
                if cfg!(windows) {
                    match tokio::process::Command::new("wsl")
                        .args(["-e", "tmux", "-V"])
                        .env("WSL_UTF8", "1")
                        .output()
                        .await
                    {
                        Ok(output) => output.status.success(),
                        Err(_) => false,
                    }
                } else {
                    match tokio::process::Command::new("which")
                        .arg("tmux")
                        .output()
                        .await
                    {
                        Ok(output) => output.status.success(),
                        Err(_) => false,
                    }
                }
            })
            .await
    }

    fn socket_name(&self) -> &str {
        &self.socket_name
    }

    fn mark_tool_used(&self) {
        self.tool_used.store(true, Ordering::SeqCst);
    }

    fn has_tool_been_used(&self) -> bool {
        self.tool_used.load(Ordering::SeqCst)
    }

    fn is_initialized(&self) -> bool {
        let state = self
            .state
            .lock()
            .expect("TmuxProvider state mutex poisoned");
        state.socket_path.is_some() && state.server_pid.is_some()
    }

    async fn initialize(&self) -> Result<(), TmuxError> {
        if self.is_initialized() {
            return Ok(());
        }

        if !self.is_available().await {
            return Err(TmuxError::NotAvailable);
        }

        // Create a new session
        let (_, stderr, code) = self
            .exec_tmux(&[
                "new-session",
                "-d",
                "-s",
                "base",
                "-e",
                "CRABCODE_SKIP_PROMPT_HISTORY=true",
            ])
            .await
            .map_err(TmuxError::Io)?;

        if code != 0 {
            // Check if session already exists
            let (_, _, check_code) = self
                .exec_tmux(&["has-session", "-t", "base"])
                .await
                .map_err(TmuxError::Io)?;

            if check_code != 0 {
                return Err(TmuxError::CommandFailed { code, stderr });
            }
        }

        // Set global env var
        let _ = self
            .exec_tmux(&[
                "set-environment",
                "-g",
                "CRABCODE_SKIP_PROMPT_HISTORY",
                "true",
            ])
            .await;

        // Get socket path and server PID
        let (stdout, _, info_code) = self
            .exec_tmux(&["display-message", "-p", "#{socket_path},#{pid}"])
            .await
            .map_err(TmuxError::Io)?;

        if info_code == 0 {
            let parts: Vec<&str> = stdout.trim().split(',').collect();
            if parts.len() == 2
                && let Ok(pid) = parts[1].parse::<u32>()
            {
                self.set_socket_info(parts[0].to_string(), pid);
                return Ok(());
            }
        }

        // Fallback: construct socket path from standard tmux location.
        // tmux sockets live at $TMPDIR/tmux-<UID>/<socket_name> (or /tmp/tmux-<UID>/).
        // We get UID via `id -u` to avoid a libc dependency.
        let uid = get_current_uid().await;
        let tmp_dir = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
        let fallback_path = format!("{}/tmux-{}/{}", tmp_dir, uid, self.socket_name);

        // Try to get PID separately
        let (pid_stdout, _, pid_code) = self
            .exec_tmux(&["display-message", "-p", "#{pid}"])
            .await
            .map_err(TmuxError::Io)?;

        if pid_code == 0
            && let Ok(pid) = pid_stdout.trim().parse::<u32>()
        {
            self.set_socket_info(fallback_path, pid);
            return Ok(());
        }

        Err(TmuxError::ParseError(
            "Failed to determine socket path and server PID".to_string(),
        ))
    }

    fn get_tmux_env(&self) -> Option<String> {
        let state = self
            .state
            .lock()
            .expect("TmuxProvider state mutex poisoned");
        match (&state.socket_path, &state.server_pid) {
            (Some(path), Some(pid)) => Some(format!("{path},{pid},0")),
            _ => None,
        }
    }

    async fn create_session(&self, name: &str) -> Result<(), TmuxError> {
        if !self.is_initialized() {
            return Err(TmuxError::NotInitialized);
        }

        let (_, stderr, code) = self
            .exec_tmux(&["new-session", "-d", "-s", name])
            .await
            .map_err(TmuxError::Io)?;

        if code != 0 {
            return Err(TmuxError::CommandFailed { code, stderr });
        }
        Ok(())
    }

    async fn has_session(&self, name: &str) -> Result<bool, TmuxError> {
        if !self.is_initialized() {
            return Err(TmuxError::NotInitialized);
        }

        let (_, _, code) = self
            .exec_tmux(&["has-session", "-t", name])
            .await
            .map_err(TmuxError::Io)?;

        Ok(code == 0)
    }

    async fn kill_session(&self, name: &str) -> Result<(), TmuxError> {
        if !self.is_initialized() {
            return Err(TmuxError::NotInitialized);
        }

        let (_, stderr, code) = self
            .exec_tmux(&["kill-session", "-t", name])
            .await
            .map_err(TmuxError::Io)?;

        if code != 0 {
            return Err(TmuxError::CommandFailed { code, stderr });
        }
        Ok(())
    }

    async fn list_sessions(&self) -> Result<Vec<TmuxSession>, TmuxError> {
        if !self.is_initialized() {
            return Err(TmuxError::NotInitialized);
        }

        let (stdout, stderr, code) = self
            .exec_tmux(&[
                "list-sessions",
                "-F",
                "#{session_name},#{session_windows},#{session_attached}",
            ])
            .await
            .map_err(TmuxError::Io)?;

        if code != 0 {
            return Err(TmuxError::CommandFailed { code, stderr });
        }

        let mut sessions = Vec::new();
        for line in stdout.trim().lines() {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() == 3 {
                sessions.push(TmuxSession {
                    name: parts[0].to_string(),
                    windows: parts[1].parse().unwrap_or(0),
                    attached: parts[2] == "1",
                });
            }
        }

        Ok(sessions)
    }

    async fn split_pane(
        &self,
        target: &str,
        direction: SplitDirection,
    ) -> Result<String, TmuxError> {
        if !self.is_initialized() {
            return Err(TmuxError::NotInitialized);
        }

        let dir_flag = match direction {
            SplitDirection::Horizontal => "-h",
            SplitDirection::Vertical => "-v",
        };

        let (stdout, stderr, code) = self
            .exec_tmux(&[
                "split-window",
                dir_flag,
                "-t",
                target,
                "-P",
                "-F",
                "#{pane_id}",
            ])
            .await
            .map_err(TmuxError::Io)?;

        if code != 0 {
            return Err(TmuxError::CommandFailed { code, stderr });
        }

        Ok(stdout.trim().to_string())
    }

    async fn send_keys(&self, target: &str, keys: &str) -> Result<(), TmuxError> {
        if !self.is_initialized() {
            return Err(TmuxError::NotInitialized);
        }

        let (_, stderr, code) = self
            .exec_tmux(&["send-keys", "-t", target, keys, "Enter"])
            .await
            .map_err(TmuxError::Io)?;

        if code != 0 {
            return Err(TmuxError::CommandFailed { code, stderr });
        }
        Ok(())
    }

    async fn kill_server(&self) -> Result<(), TmuxError> {
        let (_, stderr, code) = self
            .exec_tmux(&["kill-server"])
            .await
            .map_err(TmuxError::Io)?;

        if code != 0 {
            // Server may already be dead, which is fine
            tracing::debug!(
                "Failed to kill tmux server (exit {}): {}",
                code,
                stderr.trim()
            );
        }
        Ok(())
    }
}

/// Get the current user's UID.
///
/// On Unix, runs `id -u` to get the UID without depending on libc.
/// On Windows (WSL), defaults to 0 (root, which is the CI default).
async fn get_current_uid() -> u32 {
    #[cfg(unix)]
    {
        if let Ok(output) = tokio::process::Command::new("id").arg("-u").output().await
            && output.status.success()
            && let Ok(uid) = String::from_utf8_lossy(&output.stdout)
                .trim()
                .parse::<u32>()
        {
            return uid;
        }
        0
    }

    #[cfg(not(unix))]
    {
        // On Windows, tmux runs inside WSL. Query real UID via wsl -e id -u.
        if let Ok(output) = tokio::process::Command::new("wsl")
            .args(["-e", "id", "-u"])
            .env("WSL_UTF8", "1")
            .output()
            .await
        {
            if output.status.success() {
                if let Ok(uid) = String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .parse::<u32>()
                {
                    return uid;
                }
            }
        }
        // Fallback to 0 only if WSL query fails
        0
    }
}

/// Detect if the current process is running inside a tmux session.
///
/// Checks the TMUX environment variable which tmux sets automatically
/// for processes running inside a tmux pane.
#[must_use]
pub fn is_inside_tmux() -> bool {
    std::env::var("TMUX").is_ok()
}

/// Parse the TMUX environment variable.
///
/// Format: "`socket_path,server_pid,pane_index`"
/// Example: "/tmp/tmux-501/default,12345,0"
#[must_use]
pub fn parse_tmux_env(tmux_env: &str) -> Option<(String, u32, u32)> {
    let parts: Vec<&str> = tmux_env.split(',').collect();
    if parts.len() == 3 {
        let pid = parts[1].parse::<u32>().ok()?;
        let pane = parts[2].parse::<u32>().ok()?;
        Some((parts[0].to_string(), pid, pane))
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_socket_name_format() {
        let mgr = AcosmiTmuxManager::new(12345);
        assert_eq!(mgr.socket_name(), "acosmi-12345");
    }

    #[test]
    fn test_custom_socket_name() {
        let mgr = AcosmiTmuxManager::with_socket_name("test-socket");
        assert_eq!(mgr.socket_name(), "test-socket");
    }

    #[test]
    fn test_not_initialized_by_default() {
        let mgr = AcosmiTmuxManager::new(1);
        assert!(!mgr.is_initialized());
        assert!(mgr.get_tmux_env().is_none());
    }

    #[test]
    fn test_set_socket_info() {
        let mgr = AcosmiTmuxManager::new(1);
        mgr.set_socket_info("/tmp/tmux-501/acosmi-1".to_string(), 54321);
        assert!(mgr.is_initialized());
        assert_eq!(
            mgr.get_tmux_env(),
            Some("/tmp/tmux-501/acosmi-1,54321,0".to_string())
        );
    }

    #[test]
    fn test_tool_used_tracking() {
        let mgr = AcosmiTmuxManager::new(1);
        assert!(!mgr.has_tool_been_used());
        mgr.mark_tool_used();
        assert!(mgr.has_tool_been_used());
    }

    #[test]
    fn test_parse_tmux_env_valid() {
        let result = parse_tmux_env("/tmp/tmux-501/default,12345,0");
        assert_eq!(
            result,
            Some(("/tmp/tmux-501/default".to_string(), 12345, 0))
        );
    }

    #[test]
    fn test_parse_tmux_env_invalid() {
        assert_eq!(parse_tmux_env(""), None);
        assert_eq!(parse_tmux_env("just-a-path"), None);
        assert_eq!(parse_tmux_env("/path,notanumber,0"), None);
        assert_eq!(parse_tmux_env("/path,123,notanumber"), None);
    }

    #[test]
    fn test_tmux_session_serialization() {
        let session = TmuxSession {
            name: "base".to_string(),
            windows: 1,
            attached: false,
        };
        let json = serde_json::to_string(&session).unwrap();
        let restored: TmuxSession = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.name, "base");
        assert_eq!(restored.windows, 1);
        assert!(!restored.attached);
    }

    #[test]
    fn test_tmux_pane_serialization() {
        let pane = TmuxPane {
            id: "%0".to_string(),
            index: 0,
            active: true,
            width: 80,
            height: 24,
        };
        let json = serde_json::to_string(&pane).unwrap();
        let restored: TmuxPane = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.id, "%0");
        assert!(restored.active);
    }

    #[test]
    fn test_split_direction() {
        assert_ne!(SplitDirection::Horizontal, SplitDirection::Vertical);
    }
}

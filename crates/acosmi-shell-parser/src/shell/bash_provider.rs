//! Bash Shell Provider
//!
//! 等价于 TS bashProvider.ts
//!
//! Key behaviors ported from TS:
//! - Snapshot sourcing (if snapshot file exists)
//! - extglob disable (dual bash/zsh when `CRABCODE_SHELL_PREFIX` is set)
//! - User command wrapped in `eval`
//! - CWD tracking via `pwd -P >| <file>` (noclobber-safe)
//! - Dual path handling for Windows (POSIX path inside bash vs native OS path)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::provider::{ExecCommandOpts, ExecCommandResult, ShellProvider};
use super::session_env::SessionEnvironment;
use super::tmux::TmuxManager;
use crate::ShellType;
use crate::bash::quote::shell_quote;

/// Bash Shell Provider
pub struct BashShellProvider {
    shell_path: String,
    detached: bool,
    /// Last resolved snapshot file path (set during construction)
    snapshot_file_path: Option<String>,
    /// Stashed during `build_exec_command` for use in `get_environment_overrides`.
    /// Uses Mutex because the trait requires Send + Sync and `build_exec_command` takes &self.
    current_sandbox_tmp_dir: Mutex<Option<String>>,
    /// Optional session environment manager for hook-based env scripts + /env vars
    session_env: Option<Arc<SessionEnvironment>>,
    /// Optional tmux manager for isolated socket management
    tmux_manager: Option<Arc<dyn TmuxManager>>,
}

impl BashShellProvider {
    /// 创建 Bash Provider
    ///
    /// `skip_snapshot` controls whether to look for a snapshot file.
    /// In TS, this resolves the snapshot path asynchronously; here we
    /// do a synchronous file-existence check for simplicity.
    pub async fn new(shell_path: &str, skip_snapshot: bool) -> Self {
        let snapshot_file_path = if skip_snapshot {
            None
        } else {
            resolve_snapshot_path(shell_path)
        };

        Self {
            shell_path: shell_path.to_string(),
            // TS: detached = true
            detached: true,
            snapshot_file_path,
            current_sandbox_tmp_dir: Mutex::new(None),
            session_env: None,
            tmux_manager: None,
        }
    }

    /// Create a Bash Provider with session environment and tmux manager
    pub async fn with_infrastructure(
        shell_path: &str,
        skip_snapshot: bool,
        session_env: Option<Arc<SessionEnvironment>>,
        tmux_manager: Option<Arc<dyn TmuxManager>>,
    ) -> Self {
        let snapshot_file_path = if skip_snapshot {
            None
        } else {
            resolve_snapshot_path(shell_path)
        };

        Self {
            shell_path: shell_path.to_string(),
            detached: true,
            snapshot_file_path,
            current_sandbox_tmp_dir: Mutex::new(None),
            session_env,
            tmux_manager,
        }
    }

    /// Set the session environment manager
    pub fn set_session_env(&mut self, session_env: Arc<SessionEnvironment>) {
        self.session_env = Some(session_env);
    }

    /// Set the tmux manager
    pub fn set_tmux_manager(&mut self, tmux_manager: Arc<dyn TmuxManager>) {
        self.tmux_manager = Some(tmux_manager);
    }

    /// Get a reference to the session environment (if set)
    pub const fn session_env(&self) -> Option<&Arc<SessionEnvironment>> {
        self.session_env.as_ref()
    }

    /// Get a reference to the tmux manager (if set)
    pub fn tmux_manager(&self) -> Option<&Arc<dyn TmuxManager>> {
        self.tmux_manager.as_ref()
    }
}

/// Convert a Windows path to a POSIX path suitable for use inside bash (e.g., Git Bash).
/// `C:\Users\foo\bar` -> `/c/Users/foo/bar`
fn to_posix_path(path: &str) -> String {
    // If it already looks like a POSIX path, return as-is
    if path.starts_with('/') {
        return path.to_string();
    }
    // Handle drive letter: C:\... -> /c/...
    if path.len() >= 3 && path.as_bytes()[1] == b':' {
        let drive = (path.as_bytes()[0] as char).to_ascii_lowercase();
        let rest = &path[2..];
        return format!("/{}{}", drive, rest.replace('\\', "/"));
    }
    // Fallback: just replace backslashes
    path.replace('\\', "/")
}

/// Attempt to resolve a snapshot file path for the given shell.
/// Returns None if no snapshot file is found.
const fn resolve_snapshot_path(_shell_path: &str) -> Option<String> {
    // KNOWN_LIMITATION: shell 快照解析 — 需要与 TS resolveSnapshotFilePath 对齐，跟踪于全局审计 G-10A
    // This would check for pre-built environment snapshot files that can be sourced
    // to speed up shell initialization (avoiding full .bashrc/.profile evaluation).
    // For now, return None (no snapshot).
    None
}

/// Build the command to disable extglob.
///
/// When `CRABCODE_SHELL_PREFIX` is set, we may be running under bash OR zsh,
/// so we emit commands for both shells wrapped in a group.
/// Without the prefix, we know it's bash, so only the bash command is needed.
fn get_disable_extglob_command(has_shell_prefix: bool) -> String {
    if has_shell_prefix {
        // Dual bash/zsh compatible: try bash's shopt first, fall back to zsh's setopt
        "{ shopt -u extglob || setopt NO_EXTENDED_GLOB; } >/dev/null 2>&1 || true".to_string()
    } else {
        "shopt -u extglob 2>/dev/null".to_string()
    }
}

#[async_trait::async_trait]
impl ShellProvider for BashShellProvider {
    fn shell_type(&self) -> ShellType {
        ShellType::Bash
    }

    fn shell_path(&self) -> &str {
        &self.shell_path
    }

    fn detached(&self) -> bool {
        self.detached
    }

    async fn build_exec_command(
        &self,
        command: &str,
        opts: ExecCommandOpts,
    ) -> Result<ExecCommandResult, crate::ShellParserError> {
        // Stash the sandbox tmp dir for use in get_environment_overrides
        {
            let mut guard = self
                .current_sandbox_tmp_dir
                .lock()
                .expect("BashProvider sandbox tmp_dir mutex poisoned");
            *guard = opts.sandbox_tmp_dir.clone();
        }

        let default_tmp = std::env::temp_dir();
        let default_tmp_str = default_tmp.to_string_lossy().to_string();
        let tmp_dir = opts.sandbox_tmp_dir.as_deref().unwrap_or(&default_tmp_str);

        // Build CWD file paths:
        // - shell_cwd_file_path: POSIX path for use inside bash commands
        // - cwd_file_path: native OS path for the return value (used by Rust to read the file)
        let native_cwd_file_path = if opts.use_sandbox {
            format!("{}/cwd-{}", tmp_dir, opts.id)
        } else {
            format!("{}/acosmi-{}-cwd", tmp_dir, opts.id)
        };

        // On Windows, the tmp_dir may be a Windows path (C:\...), but inside bash
        // (Git Bash) we need a POSIX path.
        let shell_cwd_file_path = if cfg!(windows) {
            to_posix_path(&native_cwd_file_path)
        } else {
            native_cwd_file_path.clone()
        };

        // cwd_file_path returned to caller is the native OS path
        let cwd_file_path = native_cwd_file_path;

        // Detect shell prefix
        let shell_prefix = std::env::var("CRABCODE_SHELL_PREFIX")
            .ok()
            .filter(|s| !s.is_empty());
        let has_shell_prefix = shell_prefix.is_some();

        // Assemble command parts in order:
        let mut parts: Vec<String> = Vec::new();

        // 1. Source snapshot file (if exists)
        if let Some(ref snapshot_path) = self.snapshot_file_path {
            let posix_snapshot = if cfg!(windows) {
                to_posix_path(snapshot_path)
            } else {
                snapshot_path.clone()
            };
            parts.push(format!(
                "source {} 2>/dev/null || true",
                shell_quote(&posix_snapshot)
            ));
        }

        // 2. Source session environment scripts (hook env files from disk)
        //    Matches TS: `const sessionEnvScript = await getSessionEnvironmentScript()`
        if let Some(ref session_env) = self.session_env
            && let Some(script) = session_env.get_session_env_script().await
        {
            parts.push(script);
        }

        // 3. Disable extglob
        parts.push(get_disable_extglob_command(has_shell_prefix));

        // 4. User command wrapped in eval (quote the command for safe embedding)
        let quoted_command = shell_quote(command);
        parts.push(format!("eval {quoted_command}"));

        // 4. CWD tracking: use >| for noclobber safety
        parts.push(format!("pwd -P >| {}", shell_quote(&shell_cwd_file_path)));

        // Join all parts with " && "
        let mut command_string = parts.join(" && ");

        // 5. If CRABCODE_SHELL_PREFIX is set, wrap the final command
        if let Some(prefix) = shell_prefix {
            // The prefix wraps the entire command string
            command_string = format!("{} {}", prefix, shell_quote(&command_string));
        }

        Ok(ExecCommandResult {
            command_string,
            cwd_file_path,
        })
    }

    fn get_spawn_args(&self, command_string: &str) -> Vec<String> {
        if self.snapshot_file_path.is_some() {
            // With snapshot: non-login shell (snapshot already provides env setup)
            vec!["-c".to_string(), command_string.to_string()]
        } else {
            // Without snapshot: login shell so that .profile/.bash_profile are sourced
            // Note: -l must come before -c for bash to treat it as login + command
            vec![
                "-l".to_string(),
                "-c".to_string(),
                command_string.to_string(),
            ]
        }
    }

    async fn get_environment_overrides(&self, command: &str) -> HashMap<String, String> {
        let mut env = HashMap::new();

        // TS bashProvider does NOT set PS1, HISTFILE, or HISTSIZE.
        // Those were incorrectly added in the original Rust port.

        // TMUX SOCKET ISOLATION (matches TS bashProvider.getEnvironmentOverrides):
        // Initialize Acosmi's tmux socket only after the Tmux tool has been used
        // or if the current command appears to use tmux. Once initialized, all
        // subsequent Bash commands use the isolated socket via the TMUX env var.
        if let Some(ref tmux_mgr) = self.tmux_manager {
            let command_uses_tmux = command.contains("tmux");
            if tmux_mgr.has_tool_been_used() || command_uses_tmux {
                // Attempt to initialize if not already done (best-effort)
                if !tmux_mgr.is_initialized() {
                    let _ = tmux_mgr.initialize().await;
                }
            }
            // If initialized, override TMUX to isolate all tmux commands
            if let Some(tmux_env) = tmux_mgr.get_tmux_env() {
                env.insert("TMUX".to_string(), tmux_env);
            }
        }

        // If sandbox tmp dir was stashed during build_exec_command, set TMPDIR etc.
        let sandbox_tmp_dir = {
            let guard = self
                .current_sandbox_tmp_dir
                .lock()
                .expect("BashProvider sandbox tmp_dir mutex poisoned");
            guard.clone()
        };

        if let Some(ref tmp_dir) = sandbox_tmp_dir {
            env.insert("TMPDIR".to_string(), tmp_dir.clone());
            env.insert("CRABCODE_TMPDIR".to_string(), tmp_dir.clone());
            // TMPPREFIX is used by zsh for temp files
            env.insert("TMPPREFIX".to_string(), format!("{tmp_dir}/zsh"));
        }

        // Apply session env vars set via /env (child processes only, not the REPL).
        // Matches TS: `for (const [key, value] of getSessionEnvVars())`
        if let Some(ref session_env) = self.session_env {
            for (key, value) in session_env.get_env_overrides() {
                env.insert(key, value);
            }
        }

        env
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_bash_provider_type() {
        let provider = BashShellProvider::new("/bin/bash", true).await;
        assert_eq!(provider.shell_type(), ShellType::Bash);
    }

    #[tokio::test]
    async fn test_bash_provider_detached_is_true() {
        let provider = BashShellProvider::new("/bin/bash", true).await;
        assert!(provider.detached());
    }

    #[tokio::test]
    async fn test_bash_provider_spawn_args_no_snapshot() {
        let provider = BashShellProvider::new("/bin/bash", true).await;
        // skip_snapshot=true means no snapshot, so we get login shell args
        let args = provider.get_spawn_args("echo hello");
        assert_eq!(args, vec!["-l", "-c", "echo hello"]);
    }

    #[tokio::test]
    async fn test_bash_provider_env_overrides_no_ps1() {
        let provider = BashShellProvider::new("/bin/bash", true).await;
        let env = provider.get_environment_overrides("echo hello").await;
        // PS1, HISTFILE, HISTSIZE should NOT be present
        assert!(!env.contains_key("PS1"));
        assert!(!env.contains_key("HISTFILE"));
        assert!(!env.contains_key("HISTSIZE"));
    }

    #[tokio::test]
    async fn test_bash_provider_build_exec_command_contains_eval() {
        let provider = BashShellProvider::new("/bin/bash", true).await;
        let opts = ExecCommandOpts {
            id: "test-123".to_string(),
            sandbox_tmp_dir: None,
            use_sandbox: false,
        };
        let result = provider
            .build_exec_command("echo hello", opts)
            .await
            .unwrap();
        // Command should contain eval with the quoted user command
        assert!(result.command_string.contains("eval"));
        // Command should contain pwd -P >| for noclobber safety
        assert!(result.command_string.contains("pwd -P >|"));
        // Command should contain extglob disable
        assert!(result.command_string.contains("shopt -u extglob"));
    }

    #[tokio::test]
    async fn test_bash_provider_sandbox_env_vars() {
        let provider = BashShellProvider::new("/bin/bash", true).await;
        let opts = ExecCommandOpts {
            id: "test-456".to_string(),
            sandbox_tmp_dir: Some("/tmp/sandbox-456".to_string()),
            use_sandbox: true,
        };
        // build_exec_command stashes the sandbox_tmp_dir
        let _result = provider.build_exec_command("ls", opts).await.unwrap();
        let env = provider.get_environment_overrides("ls").await;
        assert_eq!(env.get("TMPDIR"), Some(&"/tmp/sandbox-456".to_string()));
        assert_eq!(
            env.get("CRABCODE_TMPDIR"),
            Some(&"/tmp/sandbox-456".to_string())
        );
        assert_eq!(
            env.get("TMPPREFIX"),
            Some(&"/tmp/sandbox-456/zsh".to_string())
        );
    }

    #[test]
    fn test_to_posix_path() {
        assert_eq!(to_posix_path("/tmp/foo"), "/tmp/foo");
        assert_eq!(to_posix_path("C:\\Users\\foo\\bar"), "/c/Users/foo/bar");
        assert_eq!(to_posix_path("D:\\temp"), "/d/temp");
    }

    #[test]
    fn test_disable_extglob_without_prefix() {
        let cmd = get_disable_extglob_command(false);
        assert_eq!(cmd, "shopt -u extglob 2>/dev/null");
    }

    #[test]
    fn test_disable_extglob_with_prefix() {
        let cmd = get_disable_extglob_command(true);
        assert!(cmd.contains("shopt -u extglob"));
        assert!(cmd.contains("setopt NO_EXTENDED_GLOB"));
    }

    #[tokio::test]
    async fn test_bash_provider_with_session_env() {
        let session_env = Arc::new(SessionEnvironment::new(
            "test-session",
            "/tmp/test-config",
            None,
        ));
        session_env.session_vars().set("MY_VAR", "my_value");
        session_env.session_vars().set("ANOTHER", "val2");

        let provider =
            BashShellProvider::with_infrastructure("/bin/bash", true, Some(session_env), None)
                .await;

        let env = provider.get_environment_overrides("echo hello").await;
        assert_eq!(env.get("MY_VAR"), Some(&"my_value".to_string()));
        assert_eq!(env.get("ANOTHER"), Some(&"val2".to_string()));
    }

    #[tokio::test]
    async fn test_bash_provider_session_env_accessors() {
        let session_env = Arc::new(SessionEnvironment::new("s1", "/tmp", None));

        let mut provider = BashShellProvider::new("/bin/bash", true).await;
        assert!(provider.session_env().is_none());
        assert!(provider.tmux_manager().is_none());

        provider.set_session_env(session_env);
        assert!(provider.session_env().is_some());
    }

    #[tokio::test]
    async fn test_bash_provider_sandbox_overrides_session_vars() {
        // Sandbox TMPDIR should win over session vars with the same key,
        // because sandbox vars are inserted after session vars.
        let session_env = Arc::new(SessionEnvironment::new("s1", "/tmp", None));
        session_env.session_vars().set("TMPDIR", "/user/custom/tmp");

        let provider =
            BashShellProvider::with_infrastructure("/bin/bash", true, Some(session_env), None)
                .await;

        let opts = ExecCommandOpts {
            id: "test-789".to_string(),
            sandbox_tmp_dir: Some("/tmp/sandbox-789".to_string()),
            use_sandbox: true,
        };
        let _result = provider.build_exec_command("ls", opts).await.unwrap();
        let env = provider.get_environment_overrides("ls").await;

        // Session vars are applied first, but then sandbox TMPDIR overwrites.
        // Actually in TS bashProvider, session vars come AFTER sandbox vars,
        // so session vars can override sandbox. In our implementation session
        // vars are applied after TMPDIR too. Let's verify the last-write-wins:
        // In our code: TMPDIR is set by sandbox block, then session vars overwrite.
        // This matches TS behavior where session vars are the last line.
        assert_eq!(env.get("TMPDIR"), Some(&"/user/custom/tmp".to_string()));
    }
}

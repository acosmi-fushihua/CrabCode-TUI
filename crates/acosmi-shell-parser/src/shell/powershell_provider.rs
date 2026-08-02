//! `PowerShell` Shell Provider
//!
//! 等价于 TS powershellProvider.ts
//!
//! Key behaviors ported from TS:
//! - Always uses `-Command` (never `-EncodedCommand`) for spawn args
//! - Correct exit code capture: checks $LASTEXITCODE, then $?, fallback 1
//! - Sandbox branch: when `use_sandbox`, builds full `PowerShell` invocation for sh -c wrapping
//! - CWD file uses `-NoNewline` on Out-File

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::provider::{ExecCommandOpts, ExecCommandResult, ShellProvider};
use super::session_env::SessionEnvironment;
use crate::ShellType;

/// `PowerShell` Shell Provider
pub struct PowerShellShellProvider {
    shell_path: String,
    /// Stashed during `build_exec_command` for use in `get_environment_overrides`.
    current_sandbox_tmp_dir: Mutex<Option<String>>,
    /// Optional session environment manager for /env vars
    session_env: Option<Arc<SessionEnvironment>>,
}

impl PowerShellShellProvider {
    #[must_use]
    pub fn new(shell_path: &str) -> Self {
        Self {
            shell_path: shell_path.to_string(),
            current_sandbox_tmp_dir: Mutex::new(None),
            session_env: None,
        }
    }

    /// Create a `PowerShell` provider with session environment integration
    #[must_use]
    pub fn with_session_env(shell_path: &str, session_env: Arc<SessionEnvironment>) -> Self {
        Self {
            shell_path: shell_path.to_string(),
            current_sandbox_tmp_dir: Mutex::new(None),
            session_env: Some(session_env),
        }
    }

    /// Set the session environment manager
    pub fn set_session_env(&mut self, session_env: Arc<SessionEnvironment>) {
        self.session_env = Some(session_env);
    }

    /// Get a reference to the session environment (if set)
    pub const fn session_env(&self) -> Option<&Arc<SessionEnvironment>> {
        self.session_env.as_ref()
    }
}

#[async_trait::async_trait]
impl ShellProvider for PowerShellShellProvider {
    fn shell_type(&self) -> ShellType {
        ShellType::PowerShell
    }

    fn shell_path(&self) -> &str {
        &self.shell_path
    }

    fn detached(&self) -> bool {
        false
    }

    async fn build_exec_command(
        &self,
        command: &str,
        opts: ExecCommandOpts,
    ) -> Result<ExecCommandResult, crate::ShellParserError> {
        // Stash sandbox tmp dir for get_environment_overrides
        {
            let mut guard = self
                .current_sandbox_tmp_dir
                .lock()
                .expect("PowerShellProvider sandbox tmp_dir mutex poisoned");
            *guard = opts.sandbox_tmp_dir.clone();
        }

        let default_tmp = std::env::temp_dir();
        let default_tmp_str = default_tmp.to_string_lossy().to_string();
        let tmp_dir = opts.sandbox_tmp_dir.as_deref().unwrap_or(&default_tmp_str);

        // CWD file path: use sandbox_tmp_dir when sandboxed
        let cwd_file_path = if opts.use_sandbox {
            format!("{}/cwd-ps-{}", tmp_dir, opts.id)
        } else {
            format!("{}/acosmi-pwd-ps-{}", tmp_dir, opts.id)
        };

        if opts.use_sandbox {
            // Sandbox branch: build full PowerShell invocation string with Base64 encoding
            // for sh -c wrapping. The outer sandbox shell (sh) will invoke PowerShell
            // with the encoded command.
            let inner_ps = build_sandbox_ps_command(command, &cwd_file_path);
            let utf16_bytes: Vec<u8> = inner_ps.encode_utf16().flat_map(u16::to_le_bytes).collect();
            let encoded = crate::powershell::parser::base64_encode(&utf16_bytes);

            // Build sh -c wrapper that invokes powershell with encoded command
            let shell_path_escaped = self.shell_path.replace('\'', "'\\''");
            let command_string = format!(
                "'{shell_path_escaped}' -NoProfile -NonInteractive -EncodedCommand '{encoded}'"
            );

            Ok(ExecCommandResult {
                command_string,
                cwd_file_path,
            })
        } else {
            // Normal (non-sandbox) branch
            let ps_command = build_normal_ps_command(command, &cwd_file_path);

            Ok(ExecCommandResult {
                command_string: ps_command,
                cwd_file_path,
            })
        }
    }

    fn get_spawn_args(&self, command_string: &str) -> Vec<String> {
        // TS: ALWAYS uses -Command (never -EncodedCommand)
        vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            command_string.to_string(),
        ]
    }

    async fn get_environment_overrides(&self, _command: &str) -> HashMap<String, String> {
        let mut env = HashMap::new();

        // Apply session env vars set via /env FIRST so sandbox TMPDIR below
        // can't be overridden by `/env TMPDIR=...`.
        // Matches TS powershellProvider ordering.
        if let Some(ref session_env) = self.session_env {
            for (key, value) in session_env.get_env_overrides() {
                env.insert(key, value);
            }
        }

        // If sandbox tmp dir was stashed, set TMPDIR and CRABCODE_TMPDIR.
        // PowerShell on Linux/macOS honors TMPDIR for [System.IO.Path]::GetTempPath()
        let sandbox_tmp_dir = {
            let guard = self
                .current_sandbox_tmp_dir
                .lock()
                .expect("PowerShellProvider sandbox tmp_dir mutex poisoned");
            guard.clone()
        };

        if let Some(ref tmp_dir) = sandbox_tmp_dir {
            env.insert("TMPDIR".to_string(), tmp_dir.clone());
            env.insert("CRABCODE_TMPDIR".to_string(), tmp_dir.clone());
        }

        env
    }
}

/// Build the `PowerShell` command string for the normal (non-sandbox) case.
///
/// Structure:
/// 1. User command
/// 2. Capture exit code correctly
/// 3. Write CWD to file with -`NoNewline`
/// 4. Exit with captured code
fn build_normal_ps_command(command: &str, cwd_file_path: &str) -> String {
    let escaped_path = cwd_file_path.replace('\'', "''");
    format!(
        "{command}\n$_ec = if ($null -ne $LASTEXITCODE) {{ $LASTEXITCODE }} elseif ($?) {{ 0 }} else {{ 1 }}\n(Get-Location).Path | Out-File -FilePath '{escaped_path}' -Encoding utf8 -NoNewline\nexit $_ec"
    )
}

/// Build the `PowerShell` command string for the sandbox case.
/// This will be Base64-encoded and passed via -`EncodedCommand`.
fn build_sandbox_ps_command(command: &str, cwd_file_path: &str) -> String {
    let escaped_path = cwd_file_path.replace('\'', "''");
    format!(
        "{command}\n$_ec = if ($null -ne $LASTEXITCODE) {{ $LASTEXITCODE }} elseif ($?) {{ 0 }} else {{ 1 }}\n(Get-Location).Path | Out-File -FilePath '{escaped_path}' -Encoding utf8 -NoNewline\nexit $_ec"
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_powershell_spawn_args_always_command() {
        let provider = PowerShellShellProvider::new("pwsh");
        // Even for long or multiline commands, always use -Command
        let long_cmd = "a".repeat(5000);
        let args = provider.get_spawn_args(&long_cmd);
        assert_eq!(args[0], "-NoProfile");
        assert_eq!(args[1], "-NonInteractive");
        assert_eq!(args[2], "-Command");
        assert_eq!(args[3], long_cmd);
    }

    #[test]
    fn test_powershell_spawn_args_short() {
        let provider = PowerShellShellProvider::new("pwsh");
        let args = provider.get_spawn_args("Get-ChildItem");
        assert_eq!(args[0], "-NoProfile");
        assert_eq!(args[1], "-NonInteractive");
        assert_eq!(args[2], "-Command");
        assert_eq!(args[3], "Get-ChildItem");
    }

    #[tokio::test]
    async fn test_powershell_exit_code_capture() {
        let provider = PowerShellShellProvider::new("pwsh");
        let opts = ExecCommandOpts {
            id: "test-1".to_string(),
            sandbox_tmp_dir: None,
            use_sandbox: false,
        };
        let result = provider
            .build_exec_command("Write-Host 'hello'", opts)
            .await
            .unwrap();
        // Should contain the correct exit code capture
        assert!(result.command_string.contains(
            "$_ec = if ($null -ne $LASTEXITCODE) { $LASTEXITCODE } elseif ($?) { 0 } else { 1 }"
        ));
        // Should use -NoNewline
        assert!(result.command_string.contains("-NoNewline"));
    }

    #[tokio::test]
    async fn test_powershell_sandbox_env_vars() {
        let provider = PowerShellShellProvider::new("pwsh");
        let opts = ExecCommandOpts {
            id: "test-2".to_string(),
            sandbox_tmp_dir: Some("/tmp/sandbox".to_string()),
            use_sandbox: true,
        };
        let _result = provider.build_exec_command("ls", opts).await.unwrap();
        let env = provider.get_environment_overrides("ls").await;
        assert_eq!(env.get("TMPDIR"), Some(&"/tmp/sandbox".to_string()));
        assert_eq!(
            env.get("CRABCODE_TMPDIR"),
            Some(&"/tmp/sandbox".to_string())
        );
    }

    #[tokio::test]
    async fn test_powershell_sandbox_cwd_path() {
        let provider = PowerShellShellProvider::new("pwsh");
        let opts = ExecCommandOpts {
            id: "test-3".to_string(),
            sandbox_tmp_dir: Some("/tmp/sandbox".to_string()),
            use_sandbox: true,
        };
        let result = provider.build_exec_command("ls", opts).await.unwrap();
        // Sandbox CWD path should use the sandbox tmp dir
        assert!(result.cwd_file_path.starts_with("/tmp/sandbox/"));
        assert!(result.cwd_file_path.contains("cwd-ps-"));
    }

    #[test]
    fn test_normal_ps_command_format() {
        let cmd = build_normal_ps_command("Get-ChildItem", "/tmp/cwd");
        assert!(cmd.starts_with("Get-ChildItem\n"));
        assert!(cmd.contains("$_ec = if ($null -ne $LASTEXITCODE)"));
        assert!(cmd.contains("-NoNewline"));
        assert!(cmd.contains("exit $_ec"));
    }

    #[tokio::test]
    async fn test_powershell_with_session_env() {
        let session_env = Arc::new(SessionEnvironment::new("ps-test", "/tmp/config", None));
        session_env
            .session_vars()
            .set("PATH", "/custom/bin:/usr/bin");
        session_env.session_vars().set("MY_PS_VAR", "hello");

        let provider = PowerShellShellProvider::with_session_env("pwsh", session_env);
        let env = provider.get_environment_overrides("Get-ChildItem").await;

        assert_eq!(env.get("MY_PS_VAR"), Some(&"hello".to_string()));
        assert_eq!(env.get("PATH"), Some(&"/custom/bin:/usr/bin".to_string()));
    }

    #[tokio::test]
    async fn test_powershell_session_env_before_sandbox() {
        // TS: session vars FIRST so sandbox TMPDIR below can't be overridden.
        // In our implementation, sandbox TMPDIR is inserted AFTER session vars,
        // so sandbox TMPDIR wins.
        let session_env = Arc::new(SessionEnvironment::new("ps-test2", "/tmp/config", None));
        session_env.session_vars().set("TMPDIR", "/user/tmp");

        let provider = PowerShellShellProvider::with_session_env("pwsh", session_env);
        let opts = ExecCommandOpts {
            id: "test-5".to_string(),
            sandbox_tmp_dir: Some("/tmp/sandbox-ps".to_string()),
            use_sandbox: true,
        };
        let _result = provider.build_exec_command("ls", opts).await.unwrap();
        let env = provider.get_environment_overrides("ls").await;

        // Sandbox TMPDIR should override session var (last-write-wins)
        assert_eq!(env.get("TMPDIR"), Some(&"/tmp/sandbox-ps".to_string()));
    }

    #[test]
    fn test_powershell_session_env_accessors() {
        let mut provider = PowerShellShellProvider::new("pwsh");
        assert!(provider.session_env().is_none());

        let session_env = Arc::new(SessionEnvironment::new("s1", "/tmp", None));
        provider.set_session_env(session_env);
        assert!(provider.session_env().is_some());
    }
}

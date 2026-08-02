//! Session Environment Management
//!
//! 等价于 TS sessionEnvironment.ts + sessionEnvVars.ts
//!
//! Manages shell session environment variables:
//! - Session-scoped env vars set via /env (applied to child processes only)
//! - Hook-based env scripts loaded from disk (Setup, `SessionStart`, `CwdChanged`, `FileChanged`)
//! - Cache invalidation when CWD changes

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Hook event types that can produce environment files
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookEvent {
    Setup,
    SessionStart,
    CwdChanged,
    FileChanged,
}

impl HookEvent {
    /// Returns the lowercase prefix used in hook env filenames
    #[must_use]
    pub const fn file_prefix(&self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::SessionStart => "sessionstart",
            Self::CwdChanged => "cwdchanged",
            Self::FileChanged => "filechanged",
        }
    }

    /// Sort priority (lower = earlier in sourcing order)
    #[must_use]
    pub const fn priority(&self) -> u8 {
        match self {
            Self::Setup => 0,
            Self::SessionStart => 1,
            Self::CwdChanged => 2,
            Self::FileChanged => 3,
        }
    }
}

/// Session-scoped environment variables set via /env command.
///
/// Applied only to spawned child processes (via provider env overrides),
/// not to the REPL process itself.
#[derive(Debug, Clone)]
pub struct SessionEnvVars {
    vars: Arc<Mutex<HashMap<String, String>>>,
}

impl Default for SessionEnvVars {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionEnvVars {
    /// Create a new empty session env var store
    #[must_use]
    pub fn new() -> Self {
        Self {
            vars: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Set a session environment variable
    pub fn set(&self, name: impl Into<String>, value: impl Into<String>) {
        let mut guard = self.vars.lock().expect("SessionEnvVars mutex poisoned");
        guard.insert(name.into(), value.into());
    }

    /// Delete a session environment variable
    pub fn delete(&self, name: &str) {
        let mut guard = self.vars.lock().expect("SessionEnvVars mutex poisoned");
        guard.remove(name);
    }

    /// Clear all session environment variables
    pub fn clear(&self) {
        let mut guard = self.vars.lock().expect("SessionEnvVars mutex poisoned");
        guard.clear();
    }

    /// Get a snapshot of all session environment variables
    #[must_use]
    pub fn get_all(&self) -> HashMap<String, String> {
        let guard = self.vars.lock().expect("SessionEnvVars mutex poisoned");
        guard.clone()
    }

    /// Get a specific session environment variable
    #[must_use]
    pub fn get(&self, name: &str) -> Option<String> {
        let guard = self.vars.lock().expect("SessionEnvVars mutex poisoned");
        guard.get(name).cloned()
    }

    /// Return the number of session env vars
    #[must_use]
    pub fn len(&self) -> usize {
        let guard = self.vars.lock().expect("SessionEnvVars mutex poisoned");
        guard.len()
    }

    /// Check if there are no session env vars
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Cache states for session environment scripts
#[derive(Debug, Clone)]
enum EnvScriptCache {
    /// Not yet loaded (need to check disk)
    NotLoaded,
    /// Checked disk, no files exist (don't check again)
    Empty,
    /// Loaded and cached
    Loaded(String),
}

/// Session environment manager that handles hook-based env scripts
/// and session-scoped env vars.
///
/// Corresponds to TS `sessionEnvironment.ts` + `sessionEnvVars.ts`.
#[derive(Debug, Clone)]
pub struct SessionEnvironment {
    /// Session ID for directory isolation
    session_id: String,
    /// Base config directory (e.g., ~/.config/acosmi)
    config_home: PathBuf,
    /// Session-scoped env vars (set via /env)
    session_vars: SessionEnvVars,
    /// Cached session env script
    script_cache: Arc<Mutex<EnvScriptCache>>,
    /// Optional path to `CRABCODE_ENV_FILE` from parent process
    env_file_path: Option<String>,
}

impl SessionEnvironment {
    /// Create a new session environment manager
    pub fn new(
        session_id: impl Into<String>,
        config_home: impl Into<PathBuf>,
        env_file_path: Option<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            config_home: config_home.into(),
            session_vars: SessionEnvVars::new(),
            script_cache: Arc::new(Mutex::new(EnvScriptCache::NotLoaded)),
            env_file_path,
        }
    }

    /// Get the session env directory path
    #[must_use]
    pub fn session_env_dir(&self) -> PathBuf {
        self.config_home.join("session-env").join(&self.session_id)
    }

    /// Get the file path for a specific hook's env file
    #[must_use]
    pub fn hook_env_file_path(&self, hook_event: HookEvent, hook_index: usize) -> PathBuf {
        let prefix = hook_event.file_prefix();
        self.session_env_dir()
            .join(format!("{prefix}-hook-{hook_index}.sh"))
    }

    /// Access the session env vars store
    #[must_use]
    pub const fn session_vars(&self) -> &SessionEnvVars {
        &self.session_vars
    }

    /// Invalidate the cached session environment script.
    /// Called when CWD changes or hook files are updated.
    pub fn invalidate_cache(&self) {
        let mut guard = self
            .script_cache
            .lock()
            .expect("SessionEnvironment script_cache mutex poisoned");
        *guard = EnvScriptCache::NotLoaded;
    }

    /// Load the session environment script from disk.
    ///
    /// Returns a shell script string that should be sourced before each command,
    /// or None if no session environment is configured.
    ///
    /// On Windows, session environment is not supported (matches TS behavior).
    pub async fn get_session_env_script(&self) -> Option<String> {
        // Windows: not supported
        if cfg!(windows) {
            return None;
        }

        // Check cache first
        {
            let guard = self
                .script_cache
                .lock()
                .expect("SessionEnvironment script_cache mutex poisoned");
            match &*guard {
                EnvScriptCache::NotLoaded => {
                    // Fall through to load from disk
                }
                EnvScriptCache::Empty => return None,
                EnvScriptCache::Loaded(script) => return Some(script.clone()),
            }
        }

        let mut scripts: Vec<String> = Vec::new();

        // 1. Check for CRABCODE_ENV_FILE from parent process
        if let Some(ref env_file) = self.env_file_path
            && let Ok(content) = tokio::fs::read_to_string(env_file).await
        {
            let trimmed = content.trim().to_string();
            if !trimmed.is_empty() {
                scripts.push(trimmed);
            }
        }

        // 2. Load hook environment files from session directory
        let session_dir = self.session_env_dir();
        if let Ok(mut entries) = tokio::fs::read_dir(&session_dir).await {
            let mut hook_files: Vec<String> = Vec::new();
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str()
                    && is_hook_env_file(name)
                {
                    hook_files.push(name.to_string());
                }
            }
            // Sort hook files by priority then index
            hook_files.sort_by(|a, b| compare_hook_env_files(a, b));

            for file_name in &hook_files {
                let file_path = session_dir.join(file_name);
                if let Ok(content) = tokio::fs::read_to_string(&file_path).await {
                    let trimmed = content.trim().to_string();
                    if !trimmed.is_empty() {
                        scripts.push(trimmed);
                    }
                }
            }
        }

        // Update cache

        if scripts.is_empty() {
            let mut guard = self
                .script_cache
                .lock()
                .expect("SessionEnvironment script_cache mutex poisoned");
            *guard = EnvScriptCache::Empty;
            None
        } else {
            let combined = scripts.join("\n");
            let mut guard = self
                .script_cache
                .lock()
                .expect("SessionEnvironment script_cache mutex poisoned");
            *guard = EnvScriptCache::Loaded(combined.clone());
            Some(combined)
        }
    }

    /// Clear CWD-related env files (filechanged, cwdchanged hooks).
    /// Called when the working directory changes.
    pub async fn clear_cwd_env_files(&self) {
        let session_dir = self.session_env_dir();
        if let Ok(mut entries) = tokio::fs::read_dir(&session_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str()
                    && is_hook_env_file(name)
                    && (name.starts_with("filechanged-hook-")
                        || name.starts_with("cwdchanged-hook-"))
                {
                    // Truncate the file (write empty content)
                    let _ = tokio::fs::write(entry.path(), "").await;
                }
            }
        }
    }

    /// Ensure the session env directory exists
    pub async fn ensure_session_dir(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(self.session_env_dir()).await
    }

    /// Get all environment overrides for a child process.
    /// Combines session vars with any other env settings.
    #[must_use]
    pub fn get_env_overrides(&self) -> HashMap<String, String> {
        self.session_vars.get_all()
    }
}

/// Check if a filename matches the hook env file pattern:
/// `(setup|sessionstart|cwdchanged|filechanged)-hook-<N>.sh`
fn is_hook_env_file(name: &str) -> bool {
    let prefixes = [
        "setup-hook-",
        "sessionstart-hook-",
        "cwdchanged-hook-",
        "filechanged-hook-",
    ];
    for prefix in &prefixes {
        if let Some(rest) = name.strip_prefix(prefix)
            && let Some(num_str) = rest.strip_suffix(".sh")
            && num_str.parse::<u32>().is_ok()
        {
            return true;
        }
    }
    false
}

/// Parse hook env file components for sorting
fn parse_hook_env_file(name: &str) -> Option<(u8, u32)> {
    let events = [
        (HookEvent::Setup, "setup-hook-"),
        (HookEvent::SessionStart, "sessionstart-hook-"),
        (HookEvent::CwdChanged, "cwdchanged-hook-"),
        (HookEvent::FileChanged, "filechanged-hook-"),
    ];
    for (event, prefix) in &events {
        if let Some(rest) = name.strip_prefix(prefix)
            && let Some(num_str) = rest.strip_suffix(".sh")
            && let Ok(index) = num_str.parse::<u32>()
        {
            return Some((event.priority(), index));
        }
    }
    None
}

/// Compare two hook env filenames for sorting (by event priority, then index)
fn compare_hook_env_files(a: &str, b: &str) -> std::cmp::Ordering {
    let a_parts = parse_hook_env_file(a).unwrap_or((99, 0));
    let b_parts = parse_hook_env_file(b).unwrap_or((99, 0));
    a_parts.cmp(&b_parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_env_vars_set_get() {
        let vars = SessionEnvVars::new();
        assert!(vars.is_empty());

        vars.set("FOO", "bar");
        assert_eq!(vars.get("FOO"), Some("bar".to_string()));
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn test_session_env_vars_delete() {
        let vars = SessionEnvVars::new();
        vars.set("FOO", "bar");
        vars.set("BAZ", "qux");
        assert_eq!(vars.len(), 2);

        vars.delete("FOO");
        assert_eq!(vars.get("FOO"), None);
        assert_eq!(vars.get("BAZ"), Some("qux".to_string()));
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn test_session_env_vars_clear() {
        let vars = SessionEnvVars::new();
        vars.set("A", "1");
        vars.set("B", "2");
        vars.clear();
        assert!(vars.is_empty());
    }

    #[test]
    fn test_session_env_vars_get_all() {
        let vars = SessionEnvVars::new();
        vars.set("X", "10");
        vars.set("Y", "20");
        let all = vars.get_all();
        assert_eq!(all.get("X"), Some(&"10".to_string()));
        assert_eq!(all.get("Y"), Some(&"20".to_string()));
    }

    #[test]
    fn test_session_env_vars_overwrite() {
        let vars = SessionEnvVars::new();
        vars.set("KEY", "old");
        vars.set("KEY", "new");
        assert_eq!(vars.get("KEY"), Some("new".to_string()));
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn test_is_hook_env_file() {
        assert!(is_hook_env_file("setup-hook-0.sh"));
        assert!(is_hook_env_file("sessionstart-hook-1.sh"));
        assert!(is_hook_env_file("cwdchanged-hook-2.sh"));
        assert!(is_hook_env_file("filechanged-hook-10.sh"));

        assert!(!is_hook_env_file("setup-hook-.sh"));
        assert!(!is_hook_env_file("random-hook-0.sh"));
        assert!(!is_hook_env_file("setup-hook-0.txt"));
        assert!(!is_hook_env_file("setup-hook-abc.sh"));
        assert!(!is_hook_env_file(""));
    }

    #[test]
    fn test_hook_env_file_sorting() {
        let mut files = vec![
            "filechanged-hook-0.sh".to_string(),
            "setup-hook-1.sh".to_string(),
            "sessionstart-hook-0.sh".to_string(),
            "setup-hook-0.sh".to_string(),
            "cwdchanged-hook-0.sh".to_string(),
        ];
        files.sort_by(|a, b| compare_hook_env_files(a, b));

        assert_eq!(
            files,
            vec![
                "setup-hook-0.sh",
                "setup-hook-1.sh",
                "sessionstart-hook-0.sh",
                "cwdchanged-hook-0.sh",
                "filechanged-hook-0.sh",
            ]
        );
    }

    #[test]
    fn test_hook_event_priority() {
        assert!(HookEvent::Setup.priority() < HookEvent::SessionStart.priority());
        assert!(HookEvent::SessionStart.priority() < HookEvent::CwdChanged.priority());
        assert!(HookEvent::CwdChanged.priority() < HookEvent::FileChanged.priority());
    }

    #[test]
    fn test_session_environment_paths() {
        let env = SessionEnvironment::new("test-session-123", "/home/user/.config/acosmi", None);
        let dir = env.session_env_dir();
        assert!(dir.ends_with("session-env/test-session-123"));

        let hook_path = env.hook_env_file_path(HookEvent::Setup, 0);
        assert!(hook_path.ends_with("setup-hook-0.sh"));
    }

    #[test]
    fn test_session_environment_env_overrides() {
        let env = SessionEnvironment::new("s1", "/tmp/config", None);
        env.session_vars().set("PATH", "/usr/bin:/bin");
        env.session_vars().set("CUSTOM", "value");

        let overrides = env.get_env_overrides();
        assert_eq!(overrides.get("PATH"), Some(&"/usr/bin:/bin".to_string()));
        assert_eq!(overrides.get("CUSTOM"), Some(&"value".to_string()));
    }

    #[test]
    fn test_cache_invalidation() {
        let env = SessionEnvironment::new("s2", "/tmp/config", None);
        // Should not panic on repeated invalidation
        env.invalidate_cache();
        env.invalidate_cache();
    }

    #[tokio::test]
    async fn test_session_env_script_empty() {
        let env = SessionEnvironment::new(
            "nonexistent-session",
            "/tmp/nonexistent-config-dir-12345",
            None,
        );
        // With no files on disk, should return None
        let script = env.get_session_env_script().await;
        if cfg!(windows) {
            assert!(script.is_none(), "Windows should always return None");
        } else {
            assert!(script.is_none(), "No hook files should mean None");
        }
    }
}

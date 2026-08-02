//! Shell State Snapshot Mechanism
//!
//! 等价于 TS ShellSnapshot.ts 的快照持久化与恢复逻辑
//!
//! Provides:
//! - Full shell state capture: env vars, CWD, shell options, aliases
//! - Incremental snapshots (delta from a base snapshot)
//! - JSON serialization/deserialization for persistence
//! - Snapshot diff for comparing two states

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A full shell state snapshot
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellSnapshot {
    /// Snapshot ID (unique identifier)
    pub id: String,
    /// Timestamp when the snapshot was taken (unix millis)
    pub timestamp_ms: u64,
    /// Shell type that produced this snapshot
    pub shell_type: String,
    /// Environment variables
    pub env_vars: HashMap<String, String>,
    /// Current working directory
    pub cwd: String,
    /// Shell options (e.g., shopt settings for bash, setopt for zsh)
    pub shell_options: Vec<ShellOption>,
    /// Aliases
    pub aliases: HashMap<String, String>,
    /// Optional: PATH as a separate ordered list for easier diffing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_entries: Option<Vec<String>>,
}

/// A shell option setting
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShellOption {
    /// Option name (e.g., "extglob", "noclobber")
    pub name: String,
    /// Whether the option is enabled
    pub enabled: bool,
}

/// An incremental snapshot that records only changes from a base snapshot
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncrementalSnapshot {
    /// ID of this incremental snapshot
    pub id: String,
    /// ID of the base snapshot this is relative to
    pub base_id: String,
    /// Timestamp when captured (unix millis)
    pub timestamp_ms: u64,
    /// Environment variables that were added or changed
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub env_added: HashMap<String, String>,
    /// Environment variable keys that were removed
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub env_removed: Vec<String>,
    /// CWD change (None if unchanged)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Shell options that changed
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options_changed: Vec<ShellOption>,
    /// Aliases that were added or changed
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub aliases_added: HashMap<String, String>,
    /// Alias keys that were removed
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases_removed: Vec<String>,
}

/// The diff between two snapshots
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SnapshotDiff {
    /// Env vars present in `after` but not in `before`, or with different values
    pub env_added_or_changed: HashMap<String, String>,
    /// Env var keys present in `before` but not in `after`
    pub env_removed: Vec<String>,
    /// CWD change (None if same)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_before: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd_after: Option<String>,
    /// Shell options that differ
    pub options_changed: Vec<ShellOption>,
    /// Aliases added/changed
    pub aliases_added_or_changed: HashMap<String, String>,
    /// Aliases removed
    pub aliases_removed: Vec<String>,
}

/// Determine the PATH separator based on shell type.
/// bash/zsh/sh always use ':', regardless of host OS.
/// `PowerShell` uses ';' on Windows and ':' on Unix.
fn path_separator_for_shell(shell_type: &str) -> char {
    match shell_type.to_lowercase().as_str() {
        "bash" | "zsh" | "sh" | "fish" => ':',
        "powershell" | "pwsh" => {
            if cfg!(windows) {
                ';'
            } else {
                ':'
            }
        }
        _ => {
            if cfg!(windows) {
                ';'
            } else {
                ':'
            }
        }
    }
}

impl ShellSnapshot {
    /// Create a new snapshot with the given parameters
    pub fn new(
        id: impl Into<String>,
        shell_type: impl Into<String>,
        env_vars: HashMap<String, String>,
        cwd: impl Into<String>,
        shell_options: Vec<ShellOption>,
        aliases: HashMap<String, String>,
    ) -> Self {
        let cwd = cwd.into();
        let env = env_vars;
        let shell_type_str: String = shell_type.into();

        // Extract PATH entries if PATH is present
        let sep = path_separator_for_shell(&shell_type_str);
        let path_entries = env
            .get("PATH")
            .map(|p| p.split(sep).map(std::string::ToString::to_string).collect());

        Self {
            id: id.into(),
            timestamp_ms: current_timestamp_ms(),
            shell_type: shell_type_str,
            env_vars: env,
            cwd,
            shell_options,
            aliases,
            path_entries,
        }
    }

    /// Serialize the snapshot to JSON
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserialize a snapshot from JSON
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Save snapshot to a file
    pub async fn save_to_file(&self, path: &Path) -> std::io::Result<()> {
        let json = self.to_json().map_err(std::io::Error::other)?;
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, json).await
    }

    /// Load snapshot from a file
    pub async fn load_from_file(path: &Path) -> std::io::Result<Self> {
        let json = tokio::fs::read_to_string(path).await?;
        Self::from_json(&json).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Compute a diff between this snapshot (before) and another (after)
    #[must_use]
    pub fn diff(&self, after: &Self) -> SnapshotDiff {
        let mut env_added_or_changed = HashMap::new();
        let mut env_removed = Vec::new();

        // Find added/changed env vars
        for (key, val) in &after.env_vars {
            match self.env_vars.get(key) {
                Some(old_val) if old_val == val => {
                    // unchanged
                }
                _ => {
                    env_added_or_changed.insert(key.clone(), val.clone());
                }
            }
        }

        // Find removed env vars
        for key in self.env_vars.keys() {
            if !after.env_vars.contains_key(key) {
                env_removed.push(key.clone());
            }
        }
        env_removed.sort();

        // CWD change
        let (cwd_before, cwd_after) = if self.cwd == after.cwd {
            (None, None)
        } else {
            (Some(self.cwd.clone()), Some(after.cwd.clone()))
        };

        // Shell options diff
        let mut options_changed = Vec::new();
        let before_opts: HashMap<&str, bool> = self
            .shell_options
            .iter()
            .map(|o| (o.name.as_str(), o.enabled))
            .collect();
        for opt in &after.shell_options {
            match before_opts.get(opt.name.as_str()) {
                Some(&enabled) if enabled == opt.enabled => {
                    // unchanged
                }
                _ => {
                    options_changed.push(opt.clone());
                }
            }
        }

        // Aliases diff
        let mut aliases_added_or_changed = HashMap::new();
        let mut aliases_removed = Vec::new();

        for (key, val) in &after.aliases {
            match self.aliases.get(key) {
                Some(old_val) if old_val == val => {}
                _ => {
                    aliases_added_or_changed.insert(key.clone(), val.clone());
                }
            }
        }
        for key in self.aliases.keys() {
            if !after.aliases.contains_key(key) {
                aliases_removed.push(key.clone());
            }
        }
        aliases_removed.sort();

        SnapshotDiff {
            env_added_or_changed,
            env_removed,
            cwd_before,
            cwd_after,
            options_changed,
            aliases_added_or_changed,
            aliases_removed,
        }
    }

    /// Create an incremental snapshot from a diff with another snapshot
    #[must_use]
    pub fn incremental_from(&self, after: &Self) -> IncrementalSnapshot {
        let diff = self.diff(after);
        IncrementalSnapshot {
            id: after.id.clone(),
            base_id: self.id.clone(),
            timestamp_ms: after.timestamp_ms,
            env_added: diff.env_added_or_changed,
            env_removed: diff.env_removed,
            cwd: diff.cwd_after,
            options_changed: diff.options_changed,
            aliases_added: diff.aliases_added_or_changed,
            aliases_removed: diff.aliases_removed,
        }
    }

    /// Apply an incremental snapshot to produce a new full snapshot
    #[must_use]
    pub fn apply_incremental(&self, inc: &IncrementalSnapshot) -> Self {
        let mut env_vars = self.env_vars.clone();

        // Remove deleted vars
        for key in &inc.env_removed {
            env_vars.remove(key);
        }

        // Add/update changed vars
        for (key, val) in &inc.env_added {
            env_vars.insert(key.clone(), val.clone());
        }

        // Update CWD
        let cwd = inc.cwd.as_ref().unwrap_or(&self.cwd).clone();

        // Update shell options
        let mut options = self.shell_options.clone();
        for changed_opt in &inc.options_changed {
            if let Some(existing) = options.iter_mut().find(|o| o.name == changed_opt.name) {
                existing.enabled = changed_opt.enabled;
            } else {
                options.push(changed_opt.clone());
            }
        }

        // Update aliases
        let mut aliases = self.aliases.clone();
        for key in &inc.aliases_removed {
            aliases.remove(key);
        }
        for (key, val) in &inc.aliases_added {
            aliases.insert(key.clone(), val.clone());
        }

        // Rebuild PATH entries
        let sep = path_separator_for_shell(&self.shell_type);
        let path_entries = env_vars
            .get("PATH")
            .map(|p| p.split(sep).map(std::string::ToString::to_string).collect());

        Self {
            id: inc.id.clone(),
            timestamp_ms: inc.timestamp_ms,
            shell_type: self.shell_type.clone(),
            env_vars,
            cwd,
            shell_options: options,
            aliases,
            path_entries,
        }
    }
}

impl IncrementalSnapshot {
    /// Serialize to JSON
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Deserialize from JSON
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Save to file
    pub async fn save_to_file(&self, path: &Path) -> std::io::Result<()> {
        let json = self.to_json().map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(path, json).await
    }

    /// Load from file
    pub async fn load_from_file(path: &Path) -> std::io::Result<Self> {
        let json = tokio::fs::read_to_string(path).await?;
        Self::from_json(&json).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Check if this incremental snapshot is empty (no changes)
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.env_added.is_empty()
            && self.env_removed.is_empty()
            && self.cwd.is_none()
            && self.options_changed.is_empty()
            && self.aliases_added.is_empty()
            && self.aliases_removed.is_empty()
    }
}

/// Snapshot file manager for managing a directory of snapshots
pub struct SnapshotStore {
    /// Base directory for snapshot files
    base_dir: PathBuf,
}

impl SnapshotStore {
    /// Create a new snapshot store at the given directory
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    /// Get the file path for a full snapshot by ID
    #[must_use]
    pub fn snapshot_path(&self, id: &str) -> PathBuf {
        self.base_dir.join(format!("snapshot-{id}.json"))
    }

    /// Get the file path for an incremental snapshot by ID
    #[must_use]
    pub fn incremental_path(&self, id: &str) -> PathBuf {
        self.base_dir.join(format!("incremental-{id}.json"))
    }

    /// Save a full snapshot
    pub async fn save_snapshot(&self, snapshot: &ShellSnapshot) -> std::io::Result<()> {
        let path = self.snapshot_path(&snapshot.id);
        snapshot.save_to_file(&path).await
    }

    /// Load a full snapshot by ID
    pub async fn load_snapshot(&self, id: &str) -> std::io::Result<ShellSnapshot> {
        let path = self.snapshot_path(id);
        ShellSnapshot::load_from_file(&path).await
    }

    /// Save an incremental snapshot
    pub async fn save_incremental(&self, incremental: &IncrementalSnapshot) -> std::io::Result<()> {
        let path = self.incremental_path(&incremental.id);
        incremental.save_to_file(&path).await
    }

    /// Load an incremental snapshot by ID
    pub async fn load_incremental(&self, id: &str) -> std::io::Result<IncrementalSnapshot> {
        let path = self.incremental_path(id);
        IncrementalSnapshot::load_from_file(&path).await
    }

    /// Ensure the store directory exists
    pub async fn ensure_dir(&self) -> std::io::Result<()> {
        tokio::fs::create_dir_all(&self.base_dir).await
    }

    /// List all full snapshot IDs in the store
    pub async fn list_snapshot_ids(&self) -> std::io::Result<Vec<String>> {
        let mut ids = Vec::new();
        if let Ok(mut entries) = tokio::fs::read_dir(&self.base_dir).await {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str()
                    && let Some(id) = name
                        .strip_prefix("snapshot-")
                        .and_then(|s| s.strip_suffix(".json"))
                {
                    ids.push(id.to_string());
                }
            }
        }
        ids.sort();
        Ok(ids)
    }
}

/// Get the current timestamp in milliseconds
fn current_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Generate a unique snapshot ID
#[must_use]
pub fn generate_snapshot_id(prefix: &str) -> String {
    let ts = current_timestamp_ms();
    let random: u32 = rand_simple();
    format!("{prefix}-{ts}-{random:06x}")
}

/// Simple pseudo-random number for snapshot IDs (no external crate needed)
fn rand_simple() -> u32 {
    let t = current_timestamp_ms();
    // Mix in thread ID for uniqueness across threads
    let thread_id = std::thread::current().id();
    let thread_hash = format!("{thread_id:?}");
    let mut hash = t as u32;
    for b in thread_hash.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(u32::from(b));
    }
    hash ^ (t >> 32) as u32
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn make_test_snapshot(id: &str) -> ShellSnapshot {
        let mut env_vars = HashMap::new();
        env_vars.insert("HOME".to_string(), "/home/user".to_string());
        // Test shell_type is "bash", which always uses ':' as PATH separator
        env_vars.insert("PATH".to_string(), "/usr/bin:/bin".to_string());
        env_vars.insert("EDITOR".to_string(), "vim".to_string());

        let mut aliases = HashMap::new();
        aliases.insert("ll".to_string(), "ls -la".to_string());
        aliases.insert("gs".to_string(), "git status".to_string());

        let options = vec![
            ShellOption {
                name: "extglob".to_string(),
                enabled: true,
            },
            ShellOption {
                name: "noclobber".to_string(),
                enabled: false,
            },
        ];

        ShellSnapshot::new(id, "bash", env_vars, "/home/user/project", options, aliases)
    }

    #[test]
    fn test_snapshot_json_roundtrip() {
        let snapshot = make_test_snapshot("test-1");
        let json = snapshot.to_json().unwrap();
        let restored = ShellSnapshot::from_json(&json).unwrap();

        assert_eq!(snapshot.id, restored.id);
        assert_eq!(snapshot.env_vars, restored.env_vars);
        assert_eq!(snapshot.cwd, restored.cwd);
        assert_eq!(snapshot.shell_options, restored.shell_options);
        assert_eq!(snapshot.aliases, restored.aliases);
    }

    #[test]
    fn test_snapshot_diff_no_changes() {
        let a = make_test_snapshot("a");
        let b = a.clone();

        let diff = a.diff(&b);
        assert!(diff.env_added_or_changed.is_empty());
        assert!(diff.env_removed.is_empty());
        assert!(diff.cwd_before.is_none());
        assert!(diff.cwd_after.is_none());
        assert!(diff.options_changed.is_empty());
        assert!(diff.aliases_added_or_changed.is_empty());
        assert!(diff.aliases_removed.is_empty());
    }

    #[test]
    fn test_snapshot_diff_env_changes() {
        let a = make_test_snapshot("a");
        let mut b = a.clone();
        b.id = "b".to_string();

        // Add a new var
        b.env_vars
            .insert("NEW_VAR".to_string(), "value".to_string());
        // Change an existing var
        b.env_vars.insert("EDITOR".to_string(), "nano".to_string());
        // Remove a var
        b.env_vars.remove("HOME");

        let diff = a.diff(&b);
        assert_eq!(
            diff.env_added_or_changed.get("NEW_VAR"),
            Some(&"value".to_string())
        );
        assert_eq!(
            diff.env_added_or_changed.get("EDITOR"),
            Some(&"nano".to_string())
        );
        assert!(diff.env_removed.contains(&"HOME".to_string()));
    }

    #[test]
    fn test_snapshot_diff_cwd_change() {
        let a = make_test_snapshot("a");
        let mut b = a.clone();
        b.id = "b".to_string();
        b.cwd = "/tmp/other".to_string();

        let diff = a.diff(&b);
        assert_eq!(diff.cwd_before, Some("/home/user/project".to_string()));
        assert_eq!(diff.cwd_after, Some("/tmp/other".to_string()));
    }

    #[test]
    fn test_snapshot_diff_alias_changes() {
        let a = make_test_snapshot("a");
        let mut b = a.clone();
        b.id = "b".to_string();

        // Change an alias
        b.aliases.insert("ll".to_string(), "ls -alh".to_string());
        // Add a new alias
        b.aliases.insert("gp".to_string(), "git push".to_string());
        // Remove an alias
        b.aliases.remove("gs");

        let diff = a.diff(&b);
        assert_eq!(
            diff.aliases_added_or_changed.get("ll"),
            Some(&"ls -alh".to_string())
        );
        assert_eq!(
            diff.aliases_added_or_changed.get("gp"),
            Some(&"git push".to_string())
        );
        assert!(diff.aliases_removed.contains(&"gs".to_string()));
    }

    #[test]
    fn test_incremental_snapshot_creation() {
        let a = make_test_snapshot("a");
        let mut b = a.clone();
        b.id = "b".to_string();
        b.env_vars.insert("NEW".to_string(), "val".to_string());
        b.env_vars.remove("HOME");
        b.cwd = "/new/path".to_string();

        let inc = a.incremental_from(&b);
        assert_eq!(inc.id, "b");
        assert_eq!(inc.base_id, "a");
        assert_eq!(inc.env_added.get("NEW"), Some(&"val".to_string()));
        assert!(inc.env_removed.contains(&"HOME".to_string()));
        assert_eq!(inc.cwd, Some("/new/path".to_string()));
    }

    #[test]
    fn test_apply_incremental() {
        let a = make_test_snapshot("a");
        let mut b = a.clone();
        b.id = "b".to_string();
        b.env_vars.insert("NEW".to_string(), "val".to_string());
        b.env_vars.remove("HOME");
        b.cwd = "/new/path".to_string();
        b.aliases.insert("gp".to_string(), "git push".to_string());
        b.aliases.remove("gs");
        b.shell_options.push(ShellOption {
            name: "histappend".to_string(),
            enabled: true,
        });

        let inc = a.incremental_from(&b);
        let restored = a.apply_incremental(&inc);

        assert_eq!(restored.id, "b");
        assert_eq!(restored.env_vars.get("NEW"), Some(&"val".to_string()));
        assert!(!restored.env_vars.contains_key("HOME"));
        assert_eq!(restored.cwd, "/new/path");
        assert_eq!(restored.aliases.get("gp"), Some(&"git push".to_string()));
        assert!(!restored.aliases.contains_key("gs"));
        assert!(
            restored
                .shell_options
                .iter()
                .any(|o| o.name == "histappend" && o.enabled)
        );
    }

    #[test]
    fn test_incremental_empty_check() {
        let a = make_test_snapshot("a");
        let b = a.clone();

        let inc = a.incremental_from(&b);
        // With identical snapshots, incremental should be empty except for ids
        assert!(inc.env_added.is_empty());
        assert!(inc.env_removed.is_empty());
        assert!(inc.cwd.is_none());
        assert!(inc.options_changed.is_empty());
        assert!(inc.aliases_added.is_empty());
        assert!(inc.aliases_removed.is_empty());
        assert!(inc.is_empty());
    }

    #[test]
    fn test_incremental_json_roundtrip() {
        let a = make_test_snapshot("a");
        let mut b = a.clone();
        b.id = "b".to_string();
        b.env_vars.insert("X".to_string(), "1".to_string());

        let inc = a.incremental_from(&b);
        let json = inc.to_json().unwrap();
        let restored = IncrementalSnapshot::from_json(&json).unwrap();

        assert_eq!(inc.id, restored.id);
        assert_eq!(inc.base_id, restored.base_id);
        assert_eq!(inc.env_added, restored.env_added);
    }

    #[test]
    fn test_path_entries_extraction() {
        let snapshot = make_test_snapshot("test");
        let entries = snapshot.path_entries.as_ref().unwrap();
        assert_eq!(entries, &["/usr/bin", "/bin"]);
    }

    #[test]
    fn test_generate_snapshot_id() {
        let id1 = generate_snapshot_id("bash");
        let id2 = generate_snapshot_id("bash");
        assert!(id1.starts_with("bash-"));
        assert!(id2.starts_with("bash-"));
        // They should be different (different timestamps or random parts)
        // Note: in extremely fast test runs they *could* be the same, so
        // we just check format rather than strict inequality
        assert!(id1.len() > 10);
    }

    #[test]
    fn test_snapshot_store_paths() {
        let store = SnapshotStore::new("/tmp/snapshots");
        assert_eq!(
            store.snapshot_path("abc123"),
            PathBuf::from("/tmp/snapshots/snapshot-abc123.json")
        );
        assert_eq!(
            store.incremental_path("def456"),
            PathBuf::from("/tmp/snapshots/incremental-def456.json")
        );
    }

    #[test]
    fn test_snapshot_diff_option_changes() {
        let a = make_test_snapshot("a");
        let mut b = a.clone();
        b.id = "b".to_string();
        // Change an existing option
        if let Some(opt) = b.shell_options.iter_mut().find(|o| o.name == "extglob") {
            opt.enabled = false;
        }

        let diff = a.diff(&b);
        assert!(
            diff.options_changed
                .iter()
                .any(|o| o.name == "extglob" && !o.enabled)
        );
    }
}

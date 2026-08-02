//! Stable identity for the cron daemon's mutable state domain.
//!
//! A cron daemon is a singleton in one transport namespace, but its business
//! state is selected independently by `CRABCODE_CONFIG_DIR`,
//! `CRABCODE_STATE_DIR`, `CRABCODE_HOME`, and `CRABCODE_PROFILE`. The identity
//! binds both dimensions so a client cannot silently mutate a daemon that was
//! started for another state/profile/config root.
//!
//! The wire value is deliberately opaque and contains no credential material:
//! `cron-state-v1:<sha256>`. The hash input is the canonical state path plus
//! the canonical transport namespace, framed with byte lengths. This is an
//! identity checksum, not an authentication secret.

use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

const DOMAIN: &[u8] = b"crabcode-cron-state-identity-v1";
pub const CRON_STATE_IDENTITY_PREFIX: &str = "cron-state-v1:";

/// Resolve the identity expected by the current process for the canonical
/// cron endpoint and the mutable state selected by `acosmi-config`.
pub fn resolve_cron_state_identity() -> io::Result<String> {
    let state_dir = acosmi_config::paths::resolve_state_dir();
    let transport = crate::paths::socket_file("cron");
    cron_state_identity_for(&state_dir, &transport)
}

/// Derive a stable identity for an explicit state directory and transport.
/// Production callers normally use [`resolve_cron_state_identity`]; the
/// explicit form is useful for launch/readiness checks and platform fixtures.
pub fn cron_state_identity_for(state_dir: &Path, transport: &Path) -> io::Result<String> {
    let state = normalize_filesystem_path(state_dir)?;
    let transport = normalize_transport_namespace(transport)?;
    Ok(state_identity_from_normalized(&state, &transport))
}

/// Hash two already-normalized UTF-8 components using the v1 framing contract.
/// This is public so the shared Rust/TypeScript fixture can pin serialization
/// without depending on a host filesystem layout.
#[must_use]
pub fn state_identity_from_normalized(state_path: &str, transport_namespace: &str) -> String {
    let state = state_path.as_bytes();
    let transport = transport_namespace.as_bytes();
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN);
    hasher.update([0]);
    hasher.update((state.len() as u64).to_be_bytes());
    hasher.update(state);
    hasher.update((transport.len() as u64).to_be_bytes());
    hasher.update(transport);
    format!("{CRON_STATE_IDENTITY_PREFIX}{:x}", hasher.finalize())
}

/// Canonicalize an existing path (including symlinks). For a not-yet-created
/// path, canonicalize its deepest existing ancestor and append the normalized
/// missing suffix. That keeps daemon/client identities equal during cold start
/// while still collapsing symlink aliases whenever the filesystem can prove
/// they are aliases.
fn normalize_filesystem_path(path: &Path) -> io::Result<String> {
    let normalized = canonicalize_state_root_path(path)?;
    let text = normalized.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "cron state identity path is not valid UTF-8",
        )
    })?;
    Ok(platform_normalize_text(text))
}

/// Canonicalize a daemon state root before deriving any endpoint, lock,
/// identity, log, or mutable-store path from it.
///
/// Existing symlinks are collapsed. For a cold-start path whose final
/// components do not exist yet, the deepest existing ancestor is
/// canonicalized and the lexically-normalized suffix is appended. Errors
/// other than a genuinely missing component fail closed: treating
/// permission denial or a symlink loop as "missing" would let two textual
/// aliases become independent daemon authorities over one physical store.
pub fn canonicalize_state_root_path(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let canonical = match std::fs::canonicalize(&absolute) {
        Ok(path) => path,
        Err(_) => canonicalize_deepest_existing(&lexically_normalize(&absolute))?,
    };
    Ok(lexically_normalize(&canonical))
}

fn canonicalize_deepest_existing(path: &Path) -> io::Result<PathBuf> {
    let mut probe = path.to_path_buf();
    let mut missing = Vec::<OsString>::new();

    loop {
        match std::fs::canonicalize(&probe) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error),
        }

        if let Some(name) = probe.file_name() {
            missing.push(name.to_os_string());
        }
        if !probe.pop() {
            // A nonexistent Windows drive/UNC root has no canonical ancestor.
            // Keep the absolute lexical path; both sides apply the same v1
            // textual normalization and therefore fail closed consistently.
            return Ok(path.to_path_buf());
        }
    }
}

fn lexically_normalize(path: &Path) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = output.pop();
            }
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                output.push(component.as_os_str());
            }
        }
    }
    output
}

fn normalize_transport_namespace(transport: &Path) -> io::Result<String> {
    #[cfg(windows)]
    {
        let text = transport.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "cron transport identity is not valid UTF-8",
            )
        })?;
        Ok(normalize_windows_text(text))
    }
    #[cfg(not(windows))]
    {
        normalize_filesystem_path(transport)
    }
}

fn platform_normalize_text(text: &str) -> String {
    #[cfg(windows)]
    {
        normalize_windows_text(text)
    }
    #[cfg(not(windows))]
    {
        text.to_string()
    }
}

/// Windows paths and named-pipe names are case-insensitive. The wire contract
/// uses lower-case forward slashes so config roots that differ only by case or
/// slash spelling cannot produce a false mismatch.
#[must_use]
pub fn normalize_windows_text(text: &str) -> String {
    let mut normalized = text.replace('\\', "/").to_lowercase();
    if let Some(rest) = normalized.strip_prefix("//?/unc/") {
        normalized = format!("//{rest}");
    } else if let Some(rest) = normalized.strip_prefix("//?/") {
        normalized = rest.to_string();
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Fixture {
        state_path: String,
        transport_namespace: String,
        identity: String,
    }

    #[test]
    fn shared_v1_serialization_fixture() {
        let fixtures: Vec<Fixture> = serde_json::from_str(include_str!(
            "../../../tests/fixtures/cron-state-identity-v1.json"
        ))
        .expect("fixture JSON");
        for fixture in fixtures {
            assert_eq!(
                state_identity_from_normalized(&fixture.state_path, &fixture.transport_namespace),
                fixture.identity
            );
        }
    }

    #[test]
    fn existing_symlink_and_real_path_share_identity() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let temp = tempfile::tempdir().expect("tempdir");
            let real_state = temp.path().join("real-state");
            let real_runtime = temp.path().join("real-runtime");
            std::fs::create_dir_all(&real_state).expect("real state");
            std::fs::create_dir_all(&real_runtime).expect("real runtime");
            let state_link = temp.path().join("state-link");
            let runtime_link = temp.path().join("runtime-link");
            symlink(&real_state, &state_link).expect("state symlink");
            symlink(&real_runtime, &runtime_link).expect("runtime symlink");

            let real = cron_state_identity_for(&real_state, &real_runtime.join("run/cron.sock"))
                .expect("real identity");
            let alias = cron_state_identity_for(&state_link, &runtime_link.join("run/cron.sock"))
                .expect("alias identity");
            assert_eq!(real, alias);
        }
    }

    #[test]
    fn nonexistent_suffix_under_symlink_is_canonicalized() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            let temp = tempfile::tempdir().expect("tempdir");
            let real = temp.path().join("real");
            std::fs::create_dir_all(&real).expect("real dir");
            let alias = temp.path().join("alias");
            symlink(&real, &alias).expect("symlink");
            let real_path =
                normalize_filesystem_path(&real.join("missing/child")).expect("real normalized");
            let alias_path =
                normalize_filesystem_path(&alias.join("missing/child")).expect("alias normalized");
            assert_eq!(real_path, alias_path);
        }
    }

    #[test]
    fn relative_paths_match_their_absolute_spelling() {
        let cwd = std::env::current_dir().expect("current dir");
        let unique = format!(".c3-state-identity-relative-{}", std::process::id());
        let relative_state = PathBuf::from(&unique).join("state/../state");
        let relative_transport = if cfg!(windows) {
            PathBuf::from(r"\\.\pipe\CrabCode-relative-state-test-Cron")
        } else {
            PathBuf::from(&unique).join("run/cron.sock")
        };
        let relative = cron_state_identity_for(&relative_state, &relative_transport)
            .expect("relative identity");
        let absolute_transport = if cfg!(windows) {
            relative_transport
        } else {
            cwd.join(&relative_transport)
        };
        let absolute = cron_state_identity_for(&cwd.join(&relative_state), &absolute_transport)
            .expect("absolute identity");
        assert_eq!(relative, absolute);
    }

    #[test]
    fn windows_case_and_separator_spelling_collapse() {
        assert_eq!(
            normalize_windows_text(r"C:\Users\Alice\State"),
            "c:/users/alice/state"
        );
        assert_eq!(
            normalize_windows_text(r"\\.\pipe\CrabCode-Alice-Cron"),
            "//./pipe/crabcode-alice-cron"
        );
    }

    #[test]
    fn windows_global_pipe_split_config_roots_have_distinct_identities() {
        let pipe = normalize_windows_text(r"\\.\pipe\CrabCode-Alice-Cron");
        let config_a = normalize_windows_text(r"C:\Users\Alice\Config-A");
        let config_b = normalize_windows_text(r"C:\Users\Alice\Config-B");
        let identity_a = state_identity_from_normalized(&config_a, &pipe);
        let identity_b = state_identity_from_normalized(&config_b, &pipe);
        assert_ne!(identity_a, identity_b);
        assert_eq!(
            identity_a,
            state_identity_from_normalized(
                &normalize_windows_text(r"c:/users/alice/config-a"),
                &pipe
            )
        );
    }
}

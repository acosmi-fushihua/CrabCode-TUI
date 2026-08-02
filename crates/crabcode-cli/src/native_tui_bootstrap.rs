//! 原生 CrabCode TUI 的最小监督拓扑。
//!
//! 这里刻意不读取任何全局/项目 settings，也不构造 TypeScript legacy
//! bootstrap 信封。workspace trust 由原生 TUI 与其私有 StructuredIO runtime
//! 在加载项目能力前完成；启动器只负责稳定 memory runtime 的 ensure、
//! 终端前台、进程树和 generation lease 的生命周期。

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

use acosmi_supervisor::topology::{
    IpcType, ProcessConfig, ProcessGroupPolicy, ProcessId, RestartPolicy, StdioPolicy,
    SupervisorConfig,
};
use anyhow::{Context, Result, bail};

use crate::memory_runtime::{MEMORY_IPC_ENDPOINT_ENV, MemoryRuntimeCoordinator};

#[derive(Clone, Debug, Default)]
struct NativeEnvironment {
    unicode: HashMap<String, String>,
    native: HashMap<OsString, OsString>,
}

impl NativeEnvironment {
    fn insert_unicode(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        self.native.remove(OsStr::new(&name));
        let _ = self.unicode.insert(name, value.into());
    }

    fn remove(&mut self, name: &str) {
        self.unicode.remove(name);
        self.native.remove(OsStr::new(name));
    }
}

/// 构造生产原生 TUI 拓扑。
///
/// memory orchestrator 是后端能力的一部分。它按 canonical state root
/// 独立于单次 TUI 生命周期复用；TUI supervisor 只持有稳定 endpoint。
pub(crate) fn build_native_tui_supervisor(
    launch_cwd: &Path,
    tui_binary: &Path,
    tui_args: Vec<String>,
) -> Result<SupervisorConfig> {
    let caller_executable =
        std::env::current_exe().context("cannot resolve CrabCode launcher path")?;
    let state_root = acosmi_daemon_launcher::paths::home_dir();
    let memory =
        MemoryRuntimeCoordinator::resolve(&state_root, &caller_executable).with_context(|| {
            format!(
                "cannot resolve stable memory runtime for {}",
                state_root.display()
            )
        })?;
    memory.ensure().with_context(|| {
        format!(
            "cannot ensure stable memory runtime at {}",
            memory.endpoint()
        )
    })?;
    let mut lifecycle_env = sanitized_parent_environment();
    configure_process_tree_helper_environment(
        &mut lifecycle_env,
        current_process_tree_helper()?.as_deref(),
    )?;
    Ok(build_native_tui_supervisor_with_inputs(
        launch_cwd,
        tui_binary,
        tui_args,
        Some(memory.endpoint().to_owned()),
        lifecycle_env,
    ))
}

fn build_native_tui_supervisor_with_inputs(
    launch_cwd: &Path,
    tui_binary: &Path,
    tui_args: Vec<String>,
    memory_ipc_endpoint: Option<String>,
    mut lifecycle_env: NativeEnvironment,
) -> SupervisorConfig {
    if let Some(endpoint) = memory_ipc_endpoint {
        lifecycle_env.insert_unicode(MEMORY_IPC_ENDPOINT_ENV, endpoint);
    } else {
        lifecycle_env.remove(MEMORY_IPC_ENDPOINT_ENV);
    }

    let shutdown_timeout = lifecycle_env
        .unicode
        .get("CRABCODE_SHUTDOWN_TIMEOUT_MS")
        .and_then(|value| value.parse::<u64>().ok())
        .map_or(Duration::from_secs(10), Duration::from_millis);
    let max_restart_count = lifecycle_env
        .unicode
        .get("CRABCODE_MAX_RESTART_COUNT")
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(5);

    let native_config = ProcessConfig {
        id: ProcessId::native_tui(),
        binary: tui_binary.to_str().unwrap_or("crabcode-tui").to_owned(),
        binary_os: Some(tui_binary.as_os_str().to_owned()),
        args: tui_args,
        env: lifecycle_env.unicode,
        env_os: lifecycle_env.native,
        inherit_parent_env: false,
        // The launch cwd is authoritative until the TUI completes its own
        // trust/worktree handshake. No pre-trust project discovery occurs here.
        cwd: Some(launch_cwd.to_path_buf()),
        restart_policy: RestartPolicy::Never,
        health_check_interval: Duration::ZERO,
        stdio_policy: StdioPolicy::InheritTerminal,
        // The outer supervisor owns no IPC transport. The native TUI owns its
        // private StructuredIO child; the stable memory endpoint is only a
        // preserved backend environment input, not the TUI transport.
        ipc_type: IpcType::None,
        process_group: ProcessGroupPolicy::Foreground,
        depends_on: vec![],
    };

    SupervisorConfig {
        processes: vec![native_config],
        shutdown_timeout,
        max_restart_count,
        restart_window: Duration::from_secs(60),
    }
}

#[cfg(windows)]
fn current_process_tree_helper() -> Result<Option<PathBuf>> {
    std::env::current_exe()
        .context("cannot resolve Windows process-tree launcher path")
        .map(Some)
}

#[cfg(not(windows))]
fn current_process_tree_helper() -> Result<Option<PathBuf>> {
    Ok(None)
}

fn configure_process_tree_helper_environment(
    environment: &mut NativeEnvironment,
    helper: Option<&Path>,
) -> Result<()> {
    const NAME: &str = acosmi_supervisor::windows_process_tree::PROCESS_TREE_HELPER_ENV;
    environment.remove(NAME);
    let Some(helper) = helper else {
        return Ok(());
    };
    if !helper.is_absolute() || !helper.is_file() {
        bail!(
            "Windows process-tree helper must be an absolute regular file: {}",
            helper.display()
        );
    }
    let canonical = std::fs::canonicalize(helper).with_context(|| {
        format!(
            "cannot canonicalize Windows process-tree helper: {}",
            helper.display()
        )
    })?;
    let canonical = canonical.to_str().with_context(|| {
        format!(
            "Windows process-tree helper path is not valid Unicode: {}",
            canonical.display()
        )
    })?;
    environment.insert_unicode(NAME, canonical);
    Ok(())
}

fn sanitized_parent_environment() -> NativeEnvironment {
    sanitized_environment_from(std::env::vars_os())
}

fn sanitized_environment_from<K, V>(values: impl IntoIterator<Item = (K, V)>) -> NativeEnvironment
where
    K: Into<OsString>,
    V: Into<OsString>,
{
    let mut environment = NativeEnvironment::default();
    for (name, value) in values {
        let name = name.into();
        let value = value.into();
        if name.to_str().is_some_and(is_pretrust_owned_environment) {
            continue;
        }
        match (name.to_str(), value.to_str()) {
            (Some(name), Some(value)) => {
                let _ = environment
                    .unicode
                    .insert(name.to_owned(), value.to_owned());
            }
            _ => {
                let _ = environment.native.insert(name, value);
            }
        }
    }
    environment
}

fn is_pretrust_owned_environment(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CRABCODE_BOOTSTRAP_CONFIG"
            | "CRABCODE_INTERNAL_STRUCTURED_CUSTOM_BRIDGE_KEYS"
            | "CRABCODE_MEMORY_IPC_ENDPOINT"
            | "CRABCODE_PROCESS_TREE_EXECUTABLE"
            | "CRABCODE_SUPERVISOR_UDS"
            | "CRABCODE_SUPERVISOR_SECRET"
    ) || upper.starts_with("CRABCODE_NATIVE_GENERATION_LEASE_")
        || upper.starts_with("CRABCODE_NATIVE_TUI_TRUST_")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    fn find<'a>(config: &'a SupervisorConfig, id: &ProcessId) -> &'a ProcessConfig {
        config
            .processes
            .iter()
            .find(|process| &process.id == id)
            .expect("process exists")
    }

    #[test]
    fn native_topology_owns_only_foreground_and_injects_stable_memory_endpoint() {
        let endpoint = "unix:/state/run/memory-orchestrator.sock";
        let config = build_native_tui_supervisor_with_inputs(
            Path::new("/workspace"),
            Path::new("/release/crabcode-tui"),
            vec!["--model".into(), "best".into()],
            Some(endpoint.into()),
            sanitized_environment_from([("PATH", "/bin")]),
        );

        assert_eq!(config.start_order(), vec![ProcessId::native_tui()]);
        assert_eq!(
            config
                .shutdown_order()
                .into_iter()
                .map(|process| process.id.clone())
                .collect::<Vec<_>>(),
            vec![ProcessId::native_tui()]
        );
        assert_eq!(config.processes.len(), 1);
        assert!(
            !config
                .processes
                .iter()
                .any(|process| process.id == ProcessId::ts_session()
                    || process.id == ProcessId::memory_orchestrator())
        );

        let native = find(&config, &ProcessId::native_tui());
        assert_eq!(native.process_group, ProcessGroupPolicy::Foreground);
        assert_eq!(native.restart_policy, RestartPolicy::Never);
        assert_eq!(native.stdio_policy, StdioPolicy::InheritTerminal);
        assert_eq!(native.ipc_type, IpcType::None);
        assert_eq!(native.binary, "/release/crabcode-tui");
        assert_eq!(
            native.binary_os.as_deref(),
            Some(OsStr::new("/release/crabcode-tui"))
        );
        assert_eq!(native.args, ["--model", "best"]);
        assert_eq!(native.cwd.as_deref(), Some(Path::new("/workspace")));
        assert!(!native.inherit_parent_env);
        assert_eq!(
            native.env.get(MEMORY_IPC_ENDPOINT_ENV).map(String::as_str),
            Some(endpoint)
        );
    }

    #[cfg(unix)]
    #[test]
    fn native_topology_preserves_non_utf8_executable_path_bytes() {
        use std::os::unix::ffi::OsStringExt as _;

        let tui = PathBuf::from(OsString::from_vec(b"/release/crabcode-\xff-tui".to_vec()));
        let config = build_native_tui_supervisor_with_inputs(
            Path::new("/workspace"),
            &tui,
            vec![],
            Some("unix:/tmp/crabcode-memory-test.sock".into()),
            sanitized_environment_from([("PATH", "/bin")]),
        );

        let native_config = find(&config, &ProcessId::native_tui());
        assert_eq!(native_config.binary_os.as_deref(), Some(tui.as_os_str()));
        assert_eq!(native_config.binary, "crabcode-tui");
        assert!(!native_config.binary.contains('\u{fffd}'));
    }

    #[test]
    fn native_environment_preserves_identity_and_shell_but_strips_pretrust_owners() {
        let env = sanitized_environment_from([
            ("PATH", "/bin"),
            ("HOME", "/caller/home"),
            ("ANTHROPIC_API_KEY", "secret"),
            ("CRABCODE_CONFIG_DIR", "/trusted-config"),
            ("CRABCODE_HOME", "/caller/crabcode-home"),
            ("CRABCODE_BOOTSTRAP_CONFIG", "forged"),
            ("crabcode_internal_structured_custom_bridge_keys", "forged"),
            ("CRABCODE_MEMORY_IPC_ENDPOINT", "stale"),
            ("CRABCODE_PROCESS_TREE_EXECUTABLE", "/stale/helper"),
            ("CRABCODE_SUPERVISOR_SECRET", "stale"),
            ("CRABCODE_NATIVE_GENERATION_LEASE_PATH", "stale"),
            ("CRABCODE_NATIVE_TUI_TRUST_NONCE", "stale"),
        ]);

        assert_eq!(env.unicode.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(
            env.unicode.get("HOME").map(String::as_str),
            Some("/caller/home")
        );
        assert_eq!(
            env.unicode.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("secret")
        );
        assert_eq!(
            env.unicode.get("CRABCODE_CONFIG_DIR").map(String::as_str),
            Some("/trusted-config")
        );
        assert_eq!(
            env.unicode.get("CRABCODE_HOME").map(String::as_str),
            Some("/caller/crabcode-home")
        );
        assert!(!env.unicode.keys().any(|name| {
            let upper = name.to_ascii_uppercase();
            upper.contains("BOOTSTRAP_CONFIG")
                || upper.contains("SUPERVISOR_SECRET")
                || upper.contains("GENERATION_LEASE")
                || upper.contains("TRUST_NONCE")
                || upper.contains("PROCESS_TREE_EXECUTABLE")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn native_environment_preserves_non_utf8_bytes_without_lossy_policy_matching() {
        use std::os::unix::ffi::OsStringExt as _;

        let non_unicode_name = OsString::from_vec(b"CRABCODE_\xff_CUSTOM_MODE".to_vec());
        let non_unicode_value = OsString::from_vec(b"value-\xfe".to_vec());
        let ordinary_non_unicode_value = OsString::from_vec(b"/bin:/private/\xff".to_vec());
        let environment = sanitized_environment_from([
            (non_unicode_name.clone(), non_unicode_value.clone()),
            (OsString::from("PATH"), ordinary_non_unicode_value.clone()),
            (
                OsString::from("CRABCODE_CUSTOM_MODE"),
                OsString::from_vec(vec![b'1', 0xff]),
            ),
        ]);

        assert_eq!(
            environment.native.get(&non_unicode_name),
            Some(&non_unicode_value)
        );
        assert_eq!(
            environment.native.get(OsStr::new("PATH")),
            Some(&ordinary_non_unicode_value)
        );
        assert!(
            environment
                .native
                .contains_key(OsStr::new("CRABCODE_CUSTOM_MODE")),
            "an ordinary non-UTF-8 value must be preserved without lossy policy matching"
        );
        assert!(!environment.unicode.contains_key("CRABCODE_CUSTOM_MODE"));
    }

    #[test]
    fn canonical_same_generation_process_tree_helper_reaches_every_native_child() {
        let temp = tempfile::tempdir().expect("tempdir");
        let helper = temp.path().join("crabcode-pure-tui-launcher.exe");
        std::fs::write(&helper, b"launcher").expect("helper fixture");
        let canonical = std::fs::canonicalize(&helper).expect("canonical helper");
        let mut environment = sanitized_environment_from([(
            acosmi_supervisor::windows_process_tree::PROCESS_TREE_HELPER_ENV,
            "/stale/helper",
        )]);
        configure_process_tree_helper_environment(&mut environment, Some(&helper))
            .expect("configure helper");

        let config = build_native_tui_supervisor_with_inputs(
            Path::new("/workspace"),
            Path::new("/release/crabcode-tui"),
            vec![],
            Some("unix:/tmp/memory.sock".into()),
            environment,
        );
        let expected = canonical.to_str().expect("Unicode fixture");
        for process in &config.processes {
            assert_eq!(
                process
                    .env
                    .get(acosmi_supervisor::windows_process_tree::PROCESS_TREE_HELPER_ENV)
                    .map(String::as_str),
                Some(expected),
                "{}",
                process.id
            );
        }
    }

    #[test]
    fn platforms_without_the_helper_remove_inherited_stale_paths() {
        let mut environment = sanitized_environment_from([(
            acosmi_supervisor::windows_process_tree::PROCESS_TREE_HELPER_ENV,
            "/stale/helper",
        )]);
        configure_process_tree_helper_environment(&mut environment, None)
            .expect("remove stale helper");
        assert!(
            !environment
                .unicode
                .contains_key(acosmi_supervisor::windows_process_tree::PROCESS_TREE_HELPER_ENV)
        );
    }

    #[test]
    fn absent_memory_endpoint_removes_stale_environment_in_test_builder() {
        let config = build_native_tui_supervisor_with_inputs(
            Path::new("/workspace"),
            Path::new("/release/crabcode-tui"),
            vec![],
            None,
            sanitized_environment_from([(MEMORY_IPC_ENDPOINT_ENV, "stale")]),
        );
        assert_eq!(config.processes.len(), 1);
        let native = find(&config, &ProcessId::native_tui());
        assert_eq!(native.ipc_type, IpcType::None);
        assert!(!native.env.contains_key(MEMORY_IPC_ENDPOINT_ENV));
    }

    #[test]
    fn lifecycle_limits_come_only_from_filtered_environment() {
        let config = build_native_tui_supervisor_with_inputs(
            Path::new("/workspace"),
            Path::new("/release/crabcode-tui"),
            vec![],
            Some("unix:/tmp/memory.sock".into()),
            sanitized_environment_from([
                ("CRABCODE_SHUTDOWN_TIMEOUT_MS", "3456"),
                ("CRABCODE_MAX_RESTART_COUNT", "9"),
            ]),
        );
        assert_eq!(config.shutdown_timeout, Duration::from_millis(3456));
        assert_eq!(config.max_restart_count, 9);
    }
}

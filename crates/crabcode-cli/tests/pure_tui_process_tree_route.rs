#[cfg(not(windows))]
#[test]
fn private_process_tree_route_precedes_terminal_policy_and_fails_closed() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_crabcode-pure-tui-launcher"))
        .args(["process-tree-exec", "--", "child"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("run private process-tree route");

    assert_eq!(output.status.code(), Some(70));
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
    assert!(
        stderr.contains("process-tree-exec is only available on Windows"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("CRABCODE_PURE_TUI_UNSUPPORTED:"),
        "the private route must dispatch before the no-TTY public policy: {stderr}"
    );
}

#[cfg(unix)]
#[test]
fn non_utf8_environment_reaches_launcher_without_abort_or_lossy_matching() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    for (name, value) in [
        (
            OsString::from_vec(b"CRABCODE_\xff_CUSTOM".to_vec()),
            OsString::from("1"),
        ),
        (
            OsString::from("CRABCODE_CUSTOM"),
            OsString::from_vec(vec![b'1', 0xff]),
        ),
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_crabcode-pure-tui-launcher"))
            .env_clear()
            .env(name, value)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .expect("run launcher with non-UTF-8 environment");

        assert_eq!(output.status.code(), Some(64), "{output:?}");
        let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
        assert_eq!(
            stderr,
            "CRABCODE_PURE_TUI_UNSUPPORTED: stdout must be attached to a terminal\n"
        );
        assert!(!stderr.contains("panicked at"), "{stderr}");
    }
}

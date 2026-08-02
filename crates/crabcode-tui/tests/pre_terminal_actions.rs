use std::io::Write as _;
use std::process::{Command, Stdio};

const CYCLIC_LOGIN_FLOW: &str = "flowchart TD\n\
    Start([User visits login page]) --> Enter[Enter username & password]\n\
    Enter --> Submit[Submit credentials]\n\
    Submit --> Validate{Credentials valid?}\n\
    Validate -->|No| Fail[Show error message]\n\
    Fail --> Attempts{Too many failed attempts?}\n\
    Attempts -->|Yes| Lock[Lock account]\n\
    Attempts -->|No| Enter\n\
    Validate -->|Yes| Session[Create session]";

fn run_hidden_mermaid_child(
    source: &[u8],
    output_path: &std::path::Path,
    quality: &str,
    deadline_ms: u64,
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_crabcode-tui"))
        .arg("__crabcode-mermaid-render")
        .arg("--out")
        .arg(output_path)
        .arg("--theme")
        .arg("dark")
        .arg("--quality")
        .arg(quality)
        .arg("--width")
        .arg("640")
        .arg("--deadline-ms")
        .arg(deadline_ms.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn isolated renderer");
    let mut stdin = child.stdin.take().expect("piped stdin");
    // Oversized-input containment can close the pipe after max+1 bytes; the
    // child exit status, not a late BrokenPipe from this harness, is the
    // authoritative outcome.
    let _ = stdin.write_all(source);
    drop(stdin);
    child.wait_with_output().expect("wait for renderer")
}

#[test]
fn hidden_cron_route_reaches_the_launcher_before_tty_validation() {
    let output = Command::new(env!("CARGO_BIN_EXE_crabcode-tui"))
        .arg("--ensure-cron-daemon")
        .env("CRABCODE_DISABLE_CRON", "1")
        .output()
        .expect("run the CrabCode TUI lifecycle route");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("CRABCODE_DISABLE_CRON"),
        "the daemon launcher must own this failure: {stderr}"
    );
    assert!(
        !stderr.contains("requires interactive stdin and stdout"),
        "the hidden lifecycle route must run before terminal validation: {stderr}"
    );
}

#[test]
fn hidden_mermaid_child_renders_png_before_runtime_or_tty_initialization() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output_path = directory.path().join("diagram.png");
    let output = run_hidden_mermaid_child(
        b"flowchart LR\nA[CrabCode]-->B[TUI]\n",
        &output_path,
        "open",
        3_000,
    );
    assert!(
        output.status.success(),
        "isolated renderer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "hidden renderer must not write terminal output"
    );
    let png = std::fs::read(&output_path).expect("rendered PNG");
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    let image = image::load_from_memory(&png).expect("decodable PNG");
    assert!(image.width() > 0 && image.height() > 0);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("direct CrabCode runtime"));
    assert!(!stderr.contains("requires interactive stdin and stdout"));
}

#[test]
fn hidden_mermaid_child_renders_cyclic_login_flow() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output_path = directory.path().join("login.png");
    let output =
        run_hidden_mermaid_child(CYCLIC_LOGIN_FLOW.as_bytes(), &output_path, "open", 30_000);
    assert!(
        output.status.success(),
        "cyclic flow failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let png = std::fs::read(&output_path).expect("PNG");
    let image = image::load_from_memory(&png).expect("decodable PNG");
    assert!(image.width() > 0 && image.height() > 0);
}

#[test]
fn hidden_mermaid_child_contains_oversized_source_without_png() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output_path = directory.path().join("oversized.png");
    let oversized = format!("flowchart TD\n{}", "A-->B\n".repeat(100_000));
    let output = run_hidden_mermaid_child(oversized.as_bytes(), &output_path, "terminal", 30_000);
    assert!(!output.status.success());
    assert!(!output_path.exists());
}

#[test]
fn hidden_mermaid_child_contains_invalid_diagram_without_png() {
    let directory = tempfile::tempdir().expect("tempdir");
    let output_path = directory.path().join("invalid.png");
    let output = run_hidden_mermaid_child(
        b"this is not a mermaid diagram at all",
        &output_path,
        "terminal",
        30_000,
    );
    assert!(!output.status.success());
    assert!(!output_path.exists());
}

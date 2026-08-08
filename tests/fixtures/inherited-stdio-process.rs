use std::env;
use std::fs;
use std::io::{self, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

const DESCENDANT_LIFETIME: Duration = Duration::from_secs(60);
const LATE_OUTPUT_DELAY: Duration = Duration::from_millis(250);

fn write_stdout(value: &[u8]) {
    let mut stdout = io::stdout().lock();
    stdout.write_all(value).expect("write fixture stdout");
    stdout.flush().expect("flush fixture stdout");
}

fn write_stderr(value: &[u8]) {
    let mut stderr = io::stderr().lock();
    stderr.write_all(value).expect("write fixture stderr");
    stderr.flush().expect("flush fixture stderr");
}

fn run_descendant(mode: &str) {
    if mode == "late" {
        thread::sleep(LATE_OUTPUT_DELAY);
        write_stdout(b"late-output\n");
        return;
    }
    thread::sleep(DESCENDANT_LIFETIME);
}

fn main() {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("descendant") {
        let mode = arguments.get(1).map(String::as_str).unwrap_or("sleep");
        run_descendant(mode);
        return;
    }

    let Some(mode) = arguments.first().map(String::as_str) else {
        panic!("usage: inherited-stdio-process <closed|exit|hang|partial|stderr|late> <pid-file>");
    };
    let Some(pid_file) = arguments.get(1) else {
        panic!("usage: inherited-stdio-process <closed|exit|hang|partial|stderr|late> <pid-file>");
    };
    assert!(
        matches!(
            mode,
            "closed" | "exit" | "hang" | "partial" | "stderr" | "late"
        ),
        "unsupported fixture mode: {mode}",
    );

    if mode == "closed" {
        write_stdout(b"{\"closed\":true}\n");
        return;
    }

    let executable = env::current_exe().expect("resolve fixture executable");
    let descendant = Command::new(executable)
        .args(["descendant", if mode == "late" { "late" } else { "sleep" }])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn fixture descendant");
    let descendant_pid = descendant.id();
    fs::write(pid_file, descendant_pid.to_string()).expect("write descendant pid");

    let suffix = if mode == "partial" { "" } else { "\n" };
    write_stdout(format!("{{\"descendantPid\":{descendant_pid}}}{suffix}").as_bytes());
    if mode == "stderr" {
        write_stderr(b"fixture stderr\n");
    }
    if mode == "hang" {
        thread::sleep(DESCENDANT_LIFETIME);
    }
}

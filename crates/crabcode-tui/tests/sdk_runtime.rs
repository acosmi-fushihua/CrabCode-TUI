#![cfg(unix)]

#[path = "../src/generated_renderer_contract.rs"]
mod generated_renderer_contract;

#[path = "../src/sdk_runtime.rs"]
mod sdk_runtime;

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use sdk_runtime::{
    EnvelopeClass, OutboundCompletion, OutboundSubmitError, RequestCorrelation, RuntimeConfig,
    RuntimeEvent, SdkRuntime, SendError, SendTimeoutStage, ShutdownError, ShutdownOutcome,
    SpawnError, SystemSubtype, TransportLimits,
};

static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

const FIXTURE: &str = r#"#!/bin/sh
set -eu

mode="${SDK_RUNTIME_FIXTURE_MODE}"
args_file="${SDK_RUNTIME_FIXTURE_ARGS}"
stdin_file="${SDK_RUNTIME_FIXTURE_STDIN}"
: > "$args_file"
: > "$stdin_file"
for argument in "$@"; do
    printf '%s\n' "$argument" >> "$args_file"
done

respond_to_control_request() {
    line="$1"
    request_id=$(printf '%s\n' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
    case "$line" in
        *'"type":"crabcode_tui_runtime_action"'*'"kind":"health_snapshot"'*)
            printf '{"type":"crabcode_tui_runtime_result","protocol_version":1,"request_id":"%s","result":{"kind":"health_snapshot","status":"ready"}}\n' "$request_id"
            ;;
        *'"subtype":"interrupt"'*)
            printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"interrupted":true}}}\n' "$request_id"
            ;;
        *'"subtype":"end_session"'*)
            printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"ended":true}}}\n' "$request_id"
            exit 0
            ;;
        *)
            ;;
    esac
}

read_stdin() {
    while IFS= read -r line; do
        printf '%s\n' "$line" >> "$stdin_file"
        respond_to_control_request "$line"
    done
}

case "$mode" in
    fields)
        printf 'stderr-isolated\n' >&2
        printf '{"type":"system","subtype":"init","session_id":"session-完整","model":"model-x","tools":["Read","Bash"],"mcp_servers":[{"name":"mcp-a","status":"connected"}],"nested":{"zero":0,"false":false,"null":null,"array":[1,"二",{"deep":"🦀"}]}}\n'
        printf '{"type":"user","uuid":"user-1","message":{"role":"user","content":[{"type":"text","text":"你好"}]},"parent_tool_use_id":null,"tool_use_result":{"stdout":"完整"}}\n'
        printf '{"type":"assistant","uuid":"assistant-1","session_id":"session-完整","parent_tool_use_id":null,"message":{"id":"api-1","role":"assistant","content":[{"type":"text","text":"完成 🦀"}],"usage":{"input_tokens":7,"output_tokens":3}},"extension":{"preserve":true}}\n'
        read_stdin
        ;;
    partial)
        printf '%s' '{"type":"system","subtype":"status","status":"处理中","extra":"'
        sleep 0.05
        printf '\360\237'
        sleep 0.05
        printf '\246\200"}\n'
        read_stdin
        ;;
    backpressure)
        index=0
        while [ "$index" -lt 12 ]; do
            printf '{"type":"user","message":{"role":"user","content":"%s-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}}\n' "$index"
            index=$((index + 1))
        done
        read_stdin
        ;;
    long-stream)
        # Emit a sustained task stream on the data lane while the parent keeps
        # servicing health, interrupt, and shutdown requests on stdin. Every
        # line stays below PIPE_BUF so the two writers cannot splice JSON.
        (
            index=0
            while [ "$index" -lt 4096 ]; do
                printf '{"type":"keep_alive","task":"long-stream","index":%s,"payload":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}\n' "$index"
                index=$((index + 1))
                if [ $((index % 64)) -eq 0 ]; then
                    sleep 0.005
                fi
            done
        ) &
        read_stdin
        ;;
    unknown-top)
        printf '{"type":"future_event","all":{"fields":["remain",1,true,null]}}\n'
        read_stdin
        ;;
    unknown-system)
        printf '{"type":"system","subtype":"future_system","payload":{"keep":"raw"}}\n'
        read_stdin
        ;;
    unknown-control)
        printf '{"type":"control_request","request_id":"future-control","request":{"subtype":"future_control","payload":{"keep":"raw"}}}\n'
        read_stdin
        ;;
    unknown-immediate)
        printf '{"type":"future_immediate","payload":{"keep":"raw"}}\n'
        exit 0
        ;;
    unknown-with-descendant)
        sleep 30 &
        printf '{"type":"future_descendant","payload":{"keep":"raw"}}\n'
        read_stdin
        ;;
    bad-json)
        printf '{"type":"assistant","broken":\n'
        read_stdin
        ;;
    oversized)
        printf '%s' '{"type":"assistant","blob":"'
        index=0
        while [ "$index" -lt 600 ]; do
            printf x
            index=$((index + 1))
        done
        printf '"}\n'
        read_stdin
        ;;
    reverse)
        printf '{"type":"control_request","request_id":"permission-1","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-1","input":{"command":"pwd"},"permission_suggestions":[{"type":"addRules","rules":[{"toolName":"Bash","ruleContent":"pwd"}],"behavior":"allow"}],"description":"run pwd"}}\n'
        printf '{"type":"control_request","request_id":"elicitation-1","request":{"subtype":"elicitation","mcp_server_name":"forms","message":"Choose","mode":"form","elicitation_id":"elicit-7","requested_schema":{"type":"object","properties":{"answer":{"type":"string"}}}}}\n'
        read_stdin
        ;;
    reverse-cancel-before-admission)
        printf '{"type":"control_request","request_id":"permission-cancelled","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"tool-cancelled","input":{"command":"pwd"}}}\n'
        IFS= read -r line
        printf '%s\n' "$line" >> "$stdin_file"
        printf '{"type":"control_cancel_request","request_id":"permission-cancelled"}\n'
        read_stdin
        ;;
    interrupt)
        read_stdin
        ;;
    crash)
        printf 'crash-stderr\n' >&2
        exit 42
        ;;
    shutdown-no-response)
        while IFS= read -r line; do
            printf '%s\n' "$line" >> "$stdin_file"
            case "$line" in
                *'"subtype":"end_session"'*) exit 0 ;;
                *) ;;
            esac
        done
        ;;
    shutdown-error)
        while IFS= read -r line; do
            printf '%s\n' "$line" >> "$stdin_file"
            request_id=$(printf '%s\n' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
            case "$line" in
                *'"subtype":"end_session"'*)
                    printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"refused"}}\n' "$request_id"
                    exit 0
                    ;;
                *) ;;
            esac
        done
        ;;
    shutdown-crash)
        while IFS= read -r line; do
            printf '%s\n' "$line" >> "$stdin_file"
            request_id=$(printf '%s\n' "$line" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
            case "$line" in
                *'"subtype":"end_session"'*)
                    printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$request_id"
                    exit 42
                    ;;
                *) ;;
            esac
        done
        ;;
    shutdown-backlog)
        index=0
        while [ "$index" -lt 4 ]; do
            printf '{"type":"keep_alive","index":%s}\n' "$index"
            printf 'shutdown-stderr-%s\n' "$index" >&2
            index=$((index + 1))
        done
        read_stdin
        ;;
    stdin-stall)
        # Deliberately stay alive without reading stdin. A frame larger than
        # the kernel pipe must hit the transport's end-to-end writer deadline.
        while :; do
            sleep 1
        done
        ;;
    stdin-stall-backlogs)
        index=0
        while [ "$index" -lt 20 ]; do
            printf '{"type":"keep_alive","index":%s,"payload":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}\n' "$index"
            printf 'stall-stderr-%s-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n' "$index" >&2
            index=$((index + 1))
        done
        while :; do
            sleep 1
        done
        ;;
    stdin-closed)
        printf 'ready\n' >> "$stdin_file"
        exec 0<&-
        while :; do
            sleep 1
        done
        ;;
    *)
        printf 'unknown fixture mode\n' >&2
        exit 90
        ;;
esac
"#;

struct Fixture {
    directory: PathBuf,
    script: PathBuf,
    args: PathBuf,
    stdin: PathBuf,
}

impl Fixture {
    fn create() -> Self {
        let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from("/tmp").join(format!(
            "crabcode-sdk-runtime-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).expect("create fixture directory");
        let script = directory.join("fixture.sh");
        std::fs::write(&script, FIXTURE).expect("write fixture");
        let mut permissions = std::fs::metadata(&script)
            .expect("fixture metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).expect("make fixture executable");
        Self {
            args: directory.join("args.log"),
            stdin: directory.join("stdin.log"),
            directory,
            script,
        }
    }

    fn config(&self, mode: &str) -> RuntimeConfig {
        RuntimeConfig {
            program: PathBuf::from("/bin/sh"),
            script: self.script.clone(),
            cwd: std::env::current_dir().expect("test cwd"),
            runtime_args: vec![OsString::from("--model"), OsString::from("best")],
            removed_environment: Vec::new(),
            environment: vec![
                (
                    OsString::from("SDK_RUNTIME_FIXTURE_MODE"),
                    OsString::from(mode),
                ),
                (
                    OsString::from("SDK_RUNTIME_FIXTURE_ARGS"),
                    self.args.clone().into_os_string(),
                ),
                (
                    OsString::from("SDK_RUNTIME_FIXTURE_STDIN"),
                    self.stdin.clone().into_os_string(),
                ),
            ],
            limits: TransportLimits::default(),
        }
    }

    fn read_lines(path: &PathBuf) -> Vec<String> {
        wait_until(Duration::from_secs(3), || path.is_file());
        std::fs::read_to_string(path)
            .expect("read fixture log")
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn wait_until(timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for fixture");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn recv(runtime: &SdkRuntime) -> RuntimeEvent {
    runtime
        .recv_event_timeout(Duration::from_secs(4))
        .expect("runtime event")
}

fn envelope(event: RuntimeEvent) -> sdk_runtime::RawEnvelope {
    match event {
        RuntimeEvent::Envelope(envelope) => envelope,
        other => panic!("expected raw envelope, got {other:?}"),
    }
}

fn recv_outbound_completions(
    runtime: &SdkRuntime,
    count: usize,
    timeout: Duration,
) -> Vec<OutboundCompletion> {
    let deadline = Instant::now() + timeout;
    let mut completed = Vec::new();
    while completed.len() < count {
        while let Some(completion) = runtime.try_recv_outbound_completion() {
            completed.push(completion);
            if completed.len() == count {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {count} non-blocking outbound completions"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    completed
}

#[test]
fn readiness_notifiers_wake_for_stdout_and_stderr_without_polling() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("fields")).expect("spawn runtime");
    let event_ready = runtime.event_notifier();
    let stderr_ready = runtime.stderr_notifier();
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test async runtime");

    async_runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), event_ready.notified())
            .await
            .expect("stdout readiness notification");
        tokio::time::timeout(Duration::from_secs(2), stderr_ready.notified())
            .await
            .expect("stderr readiness notification");
    });

    assert!(matches!(recv(&runtime), RuntimeEvent::Envelope(_)));
    runtime
        .recv_stderr_timeout(Duration::from_secs(2))
        .expect("stderr frame after readiness");
    runtime
        .shutdown("end-readiness", Some("test"))
        .expect("graceful shutdown");
}

#[test]
fn nonblocking_outbound_lane_is_bounded_fifo_and_ack_driven() {
    let fixture = Fixture::create();
    let mut config = fixture.config("interrupt");
    config.limits.writer_capacity = 2;
    let mut runtime = SdkRuntime::spawn(config).expect("spawn runtime");
    let ready = runtime.outbound_notifier();
    let async_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("test async runtime");

    let first = json!({
        "type": "user",
        "message": {"role": "user", "content": "first"}
    });
    let second_request = json!({"subtype": "get_context_usage"});
    let first_id = runtime
        .submit_user_message(first.clone())
        .expect("queue first frame");
    let second_id = runtime
        .submit_control_request("nonblocking-list", second_request.clone())
        .expect("queue second frame");
    assert!(matches!(
        runtime.submit_user_message(json!({
            "type": "user",
            "message": {"role": "user", "content": "must wait for capacity"}
        })),
        Err(OutboundSubmitError::QueueFull {
            frame_capacity: 2,
            queued_frames: 2,
            ..
        })
    ));

    async_runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), ready.notified())
            .await
            .expect("submission readiness notification");
    });
    assert!(runtime.progress_nonblocking_outbound());
    async_runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), ready.notified())
            .await
            .expect("writer ACK readiness notification");
    });

    let completions = recv_outbound_completions(&runtime, 2, Duration::from_secs(2));
    assert_eq!(
        completions.iter().map(|item| item.id).collect::<Vec<_>>(),
        [first_id, second_id]
    );
    let receipts = completions
        .into_iter()
        .map(|completion| completion.result.expect("writer ACK"))
        .collect::<Vec<_>>();
    assert_eq!(
        receipts
            .iter()
            .map(|receipt| receipt.sequence)
            .collect::<Vec<_>>(),
        [0, 1]
    );
    assert_eq!(receipts[0].value, first);
    let second = json!({
        "type": "control_request",
        "request_id": "nonblocking-list",
        "request": second_request
    });
    assert_eq!(receipts[1].value, second);

    let interrupt_id = runtime
        .submit_interrupt("nonblocking-interrupt")
        .expect("queue typed interrupt");
    let interrupt = recv_outbound_completions(&runtime, 1, Duration::from_secs(2))
        .pop()
        .expect("interrupt writer completion");
    assert_eq!(interrupt.id, interrupt_id);
    assert_eq!(
        interrupt.result.expect("interrupt writer ACK").value,
        json!({
            "type": "control_request",
            "request_id": "nonblocking-interrupt",
            "request": {"subtype": "interrupt"}
        })
    );
    let interrupt_response = envelope(recv(&runtime));
    assert!(matches!(
        interrupt_response.correlation,
        Some(RequestCorrelation::OutboundResponseMatched {
            ref request_id,
            ref request_subtype
        }) if request_id == "nonblocking-interrupt" && request_subtype == "interrupt"
    ));

    wait_until(Duration::from_secs(2), || {
        std::fs::read_to_string(&fixture.stdin).is_ok_and(|contents| contents.lines().count() >= 3)
    });
    let sent = Fixture::read_lines(&fixture.stdin)
        .into_iter()
        .take(3)
        .map(|line| serde_json::from_str::<Value>(&line).expect("captured JSON"))
        .collect::<Vec<_>>();
    assert_eq!(sent[0], first);
    assert_eq!(sent[1], second);
    assert_eq!(sent[2]["request"]["subtype"], "interrupt");
    assert!(!runtime.has_nonblocking_outbound_work());

    runtime
        .shutdown("end-nonblocking-fifo", Some("test"))
        .expect("graceful shutdown");
}

#[test]
fn private_runtime_health_roundtrip_preserves_exact_id_and_correlation() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("interrupt")).expect("spawn runtime");
    let request_id = "private-health-精确-1";
    let action = json!({"kind":"health_snapshot"});

    let delivery_id = runtime
        .submit_private_runtime_action(request_id, action.clone())
        .expect("queue private runtime action");
    let completion = recv_outbound_completions(&runtime, 1, Duration::from_secs(2))
        .pop()
        .expect("private action writer completion");
    assert_eq!(completion.id, delivery_id);
    assert_eq!(
        completion.result.expect("private action writer ACK").value,
        json!({
            "type":"crabcode_tui_runtime_action",
            "protocol_version":1,
            "request_id":request_id,
            "action":action
        })
    );

    let result = envelope(recv(&runtime));
    assert_eq!(
        result.classification,
        EnvelopeClass::PrivateRuntimeResult {
            request_id: Some(request_id.to_string()),
            result_kind: Some("health_snapshot".to_string()),
            validation_error: None,
        }
    );
    assert_eq!(
        result.correlation,
        Some(RequestCorrelation::PrivateRuntimeResultMatched {
            request_id: request_id.to_string(),
            action_kind: "health_snapshot".to_string(),
        })
    );
    assert_eq!(result.value["request_id"], request_id);

    let sent = Fixture::read_lines(&fixture.stdin)
        .into_iter()
        .map(|line| serde_json::from_str::<Value>(&line).expect("captured JSON"))
        .find(|value| value["type"] == "crabcode_tui_runtime_action")
        .expect("captured private action");
    assert_eq!(sent["request_id"], request_id);
    assert_eq!(sent["action"], json!({"kind":"health_snapshot"}));

    runtime
        .shutdown("end-private-health", Some("test"))
        .expect("graceful shutdown");
}

#[test]
fn nonblocking_writer_timeout_is_indeterminate_aborts_and_never_replays() {
    let fixture = Fixture::create();
    let mut config = fixture.config("stdin-stall");
    config.limits.outbound_send_timeout = Duration::from_millis(150);
    config.limits.shutdown_timeout = Duration::from_secs(1);
    let runtime = SdkRuntime::spawn(config).expect("spawn stalled runtime");
    let message = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": "x".repeat(8 * 1024 * 1024)
        },
        "uuid": "nonblocking-stalled-write"
    });

    let id = runtime
        .submit_user_message(message)
        .expect("submission must not wait for writer ACK");
    assert!(runtime.next_outbound_deadline().is_some());
    assert!(runtime.progress_nonblocking_outbound());
    let completion = recv_outbound_completions(&runtime, 1, Duration::from_secs(3))
        .pop()
        .expect("timeout completion");
    assert_eq!(completion.id, id);
    assert!(matches!(
        completion.result,
        Err(SendError::TimedOut {
            stage: SendTimeoutStage::WriterAcknowledgement,
            ..
        })
    ));
    assert!(matches!(
        runtime.submit_user_message(json!({
            "type": "user",
            "message": {"role": "user", "content": "must-not-replay"}
        })),
        Err(OutboundSubmitError::Send(SendError::Closed))
    ));

    let terminal_deadline = Instant::now() + Duration::from_secs(4);
    let mut saw_fatal = false;
    let mut saw_exit = false;
    while !saw_exit {
        assert!(
            Instant::now() < terminal_deadline,
            "indeterminate non-blocking timeout did not terminate the runtime"
        );
        match runtime.recv_event_timeout(Duration::from_millis(250)) {
            Ok(RuntimeEvent::Envelope(_)) => {}
            Ok(RuntimeEvent::Fatal(fatal)) => {
                assert!(fatal.reason.contains("delivery is indeterminate"));
                assert!(fatal.reason.contains("must not be retried"));
                saw_fatal = true;
            }
            Ok(RuntimeEvent::ChildExited(exit)) => {
                assert!(!exit.expected && !exit.success);
                saw_exit = true;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("runtime disconnected before terminal lifecycle events")
            }
        }
    }
    assert!(saw_fatal);
    assert!(
        Fixture::read_lines(&fixture.stdin)
            .iter()
            .all(|line| !line.contains("must-not-replay")),
        "an indeterminate delivery must never be replayed"
    );
}

#[test]
fn nonblocking_reverse_response_reserves_then_commits_on_writer_ack() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("reverse")).expect("spawn runtime");
    let _permission = envelope(recv(&runtime));
    let _elicitation = envelope(recv(&runtime));
    let response = json!({"behavior": "deny", "message": "not permitted"});
    let id = runtime
        .submit_permission_response("permission-1", response.clone())
        .expect("reserve reverse response");
    assert!(matches!(
        runtime.submit_permission_response("permission-1", response.clone()),
        Err(OutboundSubmitError::Send(SendError::Correlation(ref reason)))
            if reason.contains("already has a response awaiting delivery")
    ));

    let completion = recv_outbound_completions(&runtime, 1, Duration::from_secs(2))
        .pop()
        .expect("reverse response completion");
    assert_eq!(completion.id, id);
    completion.result.expect("reverse response writer ACK");
    assert!(matches!(
        runtime.submit_permission_response("permission-1", response),
        Err(OutboundSubmitError::Send(SendError::Correlation(ref reason)))
            if reason.contains("no pending reverse request")
    ));

    let elicitation_id = runtime
        .submit_elicitation_response("elicitation-1", json!({"action": "decline"}))
        .expect("unrelated reverse request remains pending");
    let elicitation = recv_outbound_completions(&runtime, 1, Duration::from_secs(2))
        .pop()
        .expect("elicitation response completion");
    assert_eq!(elicitation.id, elicitation_id);
    elicitation.result.expect("elicitation writer ACK");
    runtime
        .shutdown("end-nonblocking-reverse", Some("test"))
        .expect("graceful shutdown");
}

#[test]
fn cancelled_reverse_response_is_rolled_back_before_writer_admission() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("reverse-cancel-before-admission"))
        .expect("spawn runtime");
    let request = envelope(recv(&runtime));
    assert!(matches!(
        request.correlation,
        Some(RequestCorrelation::ReverseRequestRegistered {
            ref request_id,
            ref subtype
        }) if request_id == "permission-cancelled" && subtype == "can_use_tool"
    ));
    let id = runtime
        .submit_permission_response("permission-cancelled", json!({"behavior": "deny"}))
        .expect("reserve response before cancellation");

    // The fixture uses this unrelated, blocking compatibility frame only as a
    // deterministic gate. The reserved non-blocking response has not entered
    // the writer queue, so the backend cancellation must invalidate it.
    runtime
        .send_keep_alive()
        .expect("release cancellation gate");
    let cancellation = envelope(recv(&runtime));
    assert!(matches!(
        cancellation.correlation,
        Some(RequestCorrelation::ReverseRequestCancelled {
            ref request_id,
            request_subtype: Some(ref subtype)
        }) if request_id == "permission-cancelled" && subtype == "can_use_tool"
    ));

    let completion = recv_outbound_completions(&runtime, 1, Duration::from_secs(2))
        .pop()
        .expect("cancelled response completion");
    assert_eq!(completion.id, id);
    assert!(matches!(
        completion.result,
        Err(SendError::Correlation(ref reason))
            if reason.contains("cancelled before response delivery")
    ));
    assert!(
        Fixture::read_lines(&fixture.stdin)
            .iter()
            .all(|line| !line.contains("\"type\":\"control_response\"")),
        "a response cancelled before writer admission must never reach stdin"
    );

    runtime
        .shutdown("end-cancel-before-admission", Some("test"))
        .expect("graceful shutdown");
}

#[test]
fn spawn_uses_exact_fixed_protocol_arguments_and_preserves_every_field_in_order() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("fields")).expect("spawn runtime");
    let expected = [
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--model",
        "best",
    ];
    wait_until(Duration::from_secs(3), || {
        std::fs::read_to_string(&fixture.args)
            .is_ok_and(|contents| contents.lines().count() == expected.len())
    });
    assert_eq!(Fixture::read_lines(&fixture.args), expected);

    let first = envelope(recv(&runtime));
    let second = envelope(recv(&runtime));
    let third = envelope(recv(&runtime));
    assert_eq!([first.sequence, second.sequence, third.sequence], [0, 1, 2]);
    assert_eq!(
        first.classification,
        EnvelopeClass::System(SystemSubtype::Init)
    );
    assert_eq!(second.classification, EnvelopeClass::User);
    assert_eq!(third.classification, EnvelopeClass::Assistant);
    let expected_values = [
        json!({
            "type": "system",
            "subtype": "init",
            "session_id": "session-完整",
            "model": "model-x",
            "tools": ["Read", "Bash"],
            "mcp_servers": [{"name": "mcp-a", "status": "connected"}],
            "nested": {
                "zero": 0,
                "false": false,
                "null": null,
                "array": [1, "二", {"deep": "🦀"}]
            }
        }),
        json!({
            "type": "user",
            "uuid": "user-1",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": "你好"}]
            },
            "parent_tool_use_id": null,
            "tool_use_result": {"stdout": "完整"}
        }),
        json!({
            "type": "assistant",
            "uuid": "assistant-1",
            "session_id": "session-完整",
            "parent_tool_use_id": null,
            "message": {
                "id": "api-1",
                "role": "assistant",
                "content": [{"type": "text", "text": "完成 🦀"}],
                "usage": {"input_tokens": 7, "output_tokens": 3}
            },
            "extension": {"preserve": true}
        }),
    ];
    for (raw, expected) in [first, second, third].iter().zip(expected_values.iter()) {
        assert_eq!(&raw.value, expected);
        assert_eq!(
            raw.encoded_len,
            serde_json::to_vec(expected).expect("encode expected").len()
        );
    }

    let stderr = runtime
        .recv_stderr_timeout(Duration::from_secs(2))
        .expect("isolated stderr");
    assert_eq!(stderr.bytes, b"stderr-isolated");
    assert!(
        !matches!(
            runtime.try_recv_event(),
            Ok(RuntimeEvent::Envelope(ref raw)) if raw.value == json!("stderr-isolated")
        ),
        "stderr must never enter the stdout protocol queue"
    );
    runtime
        .shutdown("end-fields", Some("test"))
        .expect("graceful shutdown");
}

#[test]
fn split_packets_and_split_multibyte_codepoints_form_one_lossless_envelope() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("partial")).expect("spawn runtime");
    let raw = envelope(recv(&runtime));
    assert_eq!(
        raw.classification,
        EnvelopeClass::System(SystemSubtype::Status)
    );
    assert_eq!(raw.value["status"], "处理中");
    assert_eq!(raw.value["extra"], "🦀");
    runtime
        .shutdown("end-partial", None)
        .expect("graceful shutdown");
}

#[test]
fn bounded_queue_applies_backpressure_without_loss_or_reordering() {
    let fixture = Fixture::create();
    let mut config = fixture.config("backpressure");
    config.limits.stdout_event_capacity = 1;
    config.limits.stdout_queue_bytes = 160;
    config.limits.max_stdout_frame_bytes = 150;
    let runtime = SdkRuntime::spawn(config).expect("spawn runtime");
    std::thread::sleep(Duration::from_millis(100));

    for index in 0..12 {
        let raw = envelope(recv(&runtime));
        assert_eq!(raw.sequence, index);
        assert_eq!(
            raw.value["message"]["content"],
            format!("{index}-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
        );
    }
    drop(runtime);
}

#[test]
fn sustained_long_task_stream_stays_ordered_bounded_and_control_responsive() {
    let fixture = Fixture::create();
    let mut config = fixture.config("long-stream");
    config.limits.max_stdout_frame_bytes = 512;
    config.limits.stdout_event_capacity = 8;
    config.limits.stdout_queue_bytes = 2 * 1024;
    config.limits.writer_capacity = 2;
    let mut runtime = SdkRuntime::spawn(config).expect("spawn long-stream runtime");

    let mut stream_count = 0_u64;
    let mut next_health_probe = 512_u64;
    let mut matched_health_probes = 0_u64;
    while stream_count < 4096 || matched_health_probes < 8 {
        let raw = envelope(recv(&runtime));
        match raw.value.get("type").and_then(Value::as_str) {
            Some("keep_alive") => {
                assert_eq!(raw.value["task"], "long-stream");
                assert_eq!(raw.value["index"].as_u64(), Some(stream_count));
                stream_count += 1;
                if stream_count == next_health_probe {
                    let request_id = format!("long-stream-health-{stream_count}");
                    let delivery_id = runtime
                        .submit_private_runtime_action(
                            request_id,
                            json!({"kind":"health_snapshot"}),
                        )
                        .expect("health probe must enter the bounded control lane");
                    let completion = recv_outbound_completions(&runtime, 1, Duration::from_secs(2))
                        .pop()
                        .expect("health probe writer completion");
                    assert_eq!(completion.id, delivery_id);
                    completion.result.expect("health probe writer ACK");
                    next_health_probe += 512;
                }
            }
            Some("crabcode_tui_runtime_result") => {
                assert!(matches!(
                    raw.classification,
                    EnvelopeClass::PrivateRuntimeResult {
                        ref result_kind,
                        validation_error: None,
                        ..
                    } if result_kind.as_deref() == Some("health_snapshot")
                ));
                assert!(matches!(
                    raw.correlation,
                    Some(RequestCorrelation::PrivateRuntimeResultMatched {
                        ref action_kind,
                        ..
                    }) if action_kind == "health_snapshot"
                ));
                matched_health_probes += 1;
            }
            observed => panic!("unexpected long-stream event type: {observed:?}"),
        }
    }

    assert_eq!(stream_count, 4096);
    assert_eq!(matched_health_probes, 8);
    let interrupt = runtime
        .interrupt("long-stream-interrupt")
        .expect("interrupt remains writable after sustained output");
    assert_eq!(interrupt.value["request"]["subtype"], "interrupt");
    let interrupt_response = envelope(recv(&runtime));
    assert!(matches!(
        interrupt_response.correlation,
        Some(RequestCorrelation::OutboundResponseMatched {
            ref request_id,
            ref request_subtype,
        }) if request_id == "long-stream-interrupt" && request_subtype == "interrupt"
    ));

    runtime
        .shutdown("end-long-stream", Some("soak-complete"))
        .expect("long-stream graceful shutdown");
}

#[test]
fn dropping_with_a_full_bounded_queue_terminates_without_deadlock() {
    let fixture = Fixture::create();
    let mut config = fixture.config("backpressure");
    config.limits.stdout_event_capacity = 1;
    config.limits.stdout_queue_bytes = 160;
    config.limits.max_stdout_frame_bytes = 150;
    let runtime = SdkRuntime::spawn(config).expect("spawn runtime");
    std::thread::sleep(Duration::from_millis(100));

    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        drop(runtime);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("drop blocked behind a full event queue");
}

#[test]
fn additive_unknown_presentation_events_are_raw_and_runtime_remains_live() {
    for (mode, expected_type, expected_subtype) in [
        ("unknown-top", Some("future_event"), None),
        ("unknown-system", Some("system"), Some("future_system")),
        ("unknown-with-descendant", Some("future_descendant"), None),
    ] {
        let fixture = Fixture::create();
        let mut runtime = SdkRuntime::spawn(fixture.config(mode)).expect("spawn runtime");
        let raw = envelope(recv(&runtime));
        assert!(matches!(
            raw.classification,
            EnvelopeClass::Unclassified {
                ref observed_type,
                ref observed_system_subtype
            } if observed_type.as_deref() == expected_type
                && observed_system_subtype.as_deref() == expected_subtype
        ));
        let expected_raw = match mode {
            "unknown-top" => {
                json!({"type": "future_event", "all": {"fields": ["remain", 1, true, null]}})
            }
            "unknown-system" => {
                json!({"type": "system", "subtype": "future_system", "payload": {"keep": "raw"}})
            }
            "unknown-with-descendant" => {
                json!({"type": "future_descendant", "payload": {"keep": "raw"}})
            }
            _ => unreachable!(),
        };
        assert_eq!(raw.value, expected_raw);
        assert_eq!(
            raw.encoded_len,
            serde_json::to_vec(&expected_raw)
                .expect("encode expected unknown")
                .len()
        );
        assert!(matches!(
            runtime.recv_event_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        runtime
            .shutdown(format!("end-{mode}"), Some("additive-compatible"))
            .expect("additive presentation event must not terminate the runtime");
    }
}

#[test]
fn unknown_control_subtype_is_raw_then_fatal_and_terminated() {
    let fixture = Fixture::create();
    let runtime = SdkRuntime::spawn(fixture.config("unknown-control")).expect("spawn runtime");
    let raw = envelope(recv(&runtime));
    assert!(matches!(
        raw.classification,
        EnvelopeClass::Unclassified {
            ref observed_type,
            ref observed_system_subtype
        } if observed_type.as_deref() == Some("control_request")
            && observed_system_subtype.is_none()
    ));
    assert_eq!(
        raw.value,
        json!({
            "type": "control_request",
            "request_id": "future-control",
            "request": {
                "subtype": "future_control",
                "payload": {"keep": "raw"}
            }
        })
    );
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::Fatal(ref fatal)
            if fatal.reason.contains("unknown control request subtype `future_control`")
    ));
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::ChildExited(ref exited) if !exited.expected
    ));
}

#[test]
fn additive_unknown_event_is_retained_before_unexpected_child_exit() {
    let fixture = Fixture::create();
    let runtime = SdkRuntime::spawn(fixture.config("unknown-immediate")).expect("spawn runtime");
    let raw = envelope(recv(&runtime));
    assert!(matches!(
        raw.classification,
        EnvelopeClass::Unclassified {
            ref observed_type,
            ref observed_system_subtype
        } if observed_type.as_deref() == Some("future_immediate")
            && observed_system_subtype.is_none()
    ));
    assert_eq!(
        raw.value,
        json!({"type": "future_immediate", "payload": {"keep": "raw"}})
    );
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::Fatal(ref fatal)
            if fatal.reason.contains("exited unexpectedly")
    ));
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::ChildExited(ref exited)
            if !exited.expected && exited.success && exited.code == Some(0)
    ));
}

#[test]
fn malformed_json_and_oversized_stdout_fail_closed() {
    for (mode, expected) in [
        ("bad-json", "invalid stdout NDJSON"),
        ("oversized", "exceeded 128 bytes"),
    ] {
        let fixture = Fixture::create();
        let mut config = fixture.config(mode);
        if mode == "oversized" {
            config.limits.max_stdout_frame_bytes = 128;
        }
        let runtime = SdkRuntime::spawn(config).expect("spawn runtime");
        let fatal = recv(&runtime);
        assert!(matches!(
            fatal,
            RuntimeEvent::Fatal(ref fatal) if fatal.reason.contains(expected)
        ));
        assert!(matches!(recv(&runtime), RuntimeEvent::ChildExited(_)));
    }
}

#[test]
fn permission_and_elicitation_responses_are_exact_and_request_correlated() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("reverse")).expect("spawn runtime");
    let permission = envelope(recv(&runtime));
    let elicitation = envelope(recv(&runtime));
    assert!(matches!(
        permission.correlation,
        Some(RequestCorrelation::ReverseRequestRegistered {
            ref request_id,
            ref subtype
        }) if request_id == "permission-1" && subtype == "can_use_tool"
    ));
    assert!(matches!(
        elicitation.correlation,
        Some(RequestCorrelation::ReverseRequestRegistered {
            ref request_id,
            ref subtype
        }) if request_id == "elicitation-1" && subtype == "elicitation"
    ));

    let permission_body = json!({
        "behavior": "allow",
        "updatedInput": {"command": "pwd"},
        "updatedPermissions": [{
            "type": "addRules",
            "rules": [{"toolName": "Bash", "ruleContent": "pwd"}],
            "behavior": "allow"
        }],
        "decisionClassification": "user_permanent"
    });
    let elicitation_body = json!({
        "action": "accept",
        "content": {"answer": "完整"}
    });
    let permission_receipt = runtime
        .respond_permission("permission-1", permission_body.clone())
        .expect("permission response");
    let elicitation_receipt = runtime
        .respond_elicitation("elicitation-1", elicitation_body.clone())
        .expect("elicitation response");
    let expected_permission_wire = json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": "permission-1",
            "response": permission_body
        }
    });
    let expected_elicitation_wire = json!({
        "type": "control_response",
        "response": {
            "subtype": "success",
            "request_id": "elicitation-1",
            "response": elicitation_body
        }
    });
    assert_eq!(permission_receipt.value, expected_permission_wire);
    assert_eq!(elicitation_receipt.value, expected_elicitation_wire);

    wait_until(Duration::from_secs(3), || {
        std::fs::read_to_string(&fixture.stdin).is_ok_and(|contents| contents.lines().count() >= 2)
    });
    let sent = Fixture::read_lines(&fixture.stdin)
        .into_iter()
        .take(2)
        .map(|line| serde_json::from_str::<Value>(&line).expect("captured JSON"))
        .collect::<Vec<_>>();
    assert_eq!(sent, [expected_permission_wire, expected_elicitation_wire]);

    runtime
        .shutdown("end-reverse", Some("test"))
        .expect("graceful shutdown");
}

#[test]
fn reverse_control_error_is_exact_and_closes_only_the_matching_request() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("reverse")).expect("spawn runtime");
    let _permission = envelope(recv(&runtime));
    let _elicitation = envelope(recv(&runtime));
    let pending = json!({
        "type":"control_request",
        "request_id":"still-pending",
        "request":{
            "subtype":"can_use_tool",
            "tool_name":"Read",
            "input":{"file_path":"a"}
        }
    });
    let receipt = runtime
        .respond_control_error(
            "permission-1",
            "can_use_tool",
            "policy changed",
            Some(vec![pending.clone()]),
        )
        .expect("typed error response");
    let expected = json!({
        "type":"control_response",
        "response":{
            "subtype":"error",
            "request_id":"permission-1",
            "error":"policy changed",
            "pending_permission_requests":[pending]
        }
    });
    assert_eq!(receipt.value, expected);
    assert!(
        runtime
            .respond_permission("permission-1", json!({"behavior":"deny"}))
            .expect_err("the resolved request must not remain pending")
            .to_string()
            .contains("no pending reverse request")
    );
    runtime
        .respond_elicitation("elicitation-1", json!({"action":"decline"}))
        .expect("the unrelated reverse request remains pending");
    runtime
        .shutdown("end-reverse-error", Some("test"))
        .expect("graceful shutdown");
    assert_eq!(
        serde_json::from_str::<Value>(&Fixture::read_lines(&fixture.stdin)[0])
            .expect("captured error response"),
        expected
    );
}

#[test]
fn keep_alive_and_environment_update_have_exact_typed_stdin_wires() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("interrupt")).expect("spawn runtime");
    let keep_alive = runtime.send_keep_alive().expect("keep-alive");
    assert_eq!(keep_alive.value, json!({"type":"keep_alive"}));

    let variables = HashMap::from([
        (
            "CRABCODE_SESSION_ACCESS_TOKEN".to_string(),
            "fresh".to_string(),
        ),
        ("UNICODE_VALUE".to_string(), "完整".to_string()),
    ]);
    let update = runtime
        .send_environment_update(&variables)
        .expect("environment update");
    assert_eq!(
        update.value,
        json!({
            "type":"update_environment_variables",
            "variables":{
                "CRABCODE_SESSION_ACCESS_TOKEN":"fresh",
                "UNICODE_VALUE":"完整"
            }
        })
    );
    runtime
        .shutdown("end-typed-stdin", Some("test"))
        .expect("graceful shutdown");

    let sent = Fixture::read_lines(&fixture.stdin)
        .into_iter()
        .map(|line| serde_json::from_str::<Value>(&line).expect("captured JSON"))
        .collect::<Vec<_>>();
    assert_eq!(sent[0], keep_alive.value);
    assert_eq!(sent[1], update.value);
    assert_eq!(sent[2]["request"]["subtype"], "end_session");
}

#[test]
fn interrupt_and_shutdown_use_correlated_control_requests() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("interrupt")).expect("spawn runtime");
    let user_message = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": "send exactly"}]
        },
        "parent_tool_use_id": null,
        "session_id": "caller-owned"
    });
    let user_receipt = runtime
        .send_user_message(user_message.clone())
        .expect("user message write");
    assert_eq!(user_receipt.value, user_message);

    let receipt = runtime.interrupt("interrupt-1").expect("interrupt write");
    assert_eq!(
        receipt.value,
        json!({
            "type": "control_request",
            "request_id": "interrupt-1",
            "request": {"subtype": "interrupt"}
        })
    );
    let response = envelope(recv(&runtime));
    assert!(matches!(
        response.correlation,
        Some(RequestCorrelation::OutboundResponseMatched {
            ref request_id,
            ref request_subtype
        }) if request_id == "interrupt-1" && request_subtype == "interrupt"
    ));
    assert_eq!(
        runtime
            .shutdown("end-interrupt", Some("test_complete"))
            .expect("graceful shutdown"),
        ShutdownOutcome::Graceful
    );
    let end_response = envelope(recv(&runtime));
    assert_eq!(
        end_response.value,
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "end-interrupt",
                "response": {"ended": true}
            }
        })
    );
    assert!(matches!(
        end_response.correlation,
        Some(RequestCorrelation::OutboundResponseMatched {
            ref request_id,
            ref request_subtype
        }) if request_id == "end-interrupt" && request_subtype == "end_session"
    ));
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::ChildExited(ref exit) if exit.expected && exit.success
    ));
    assert_eq!(
        runtime
            .shutdown("end-interrupt-repeat", Some("test_complete"))
            .expect("repeated shutdown is idempotent"),
        ShutdownOutcome::AlreadyStopped
    );

    let sent = Fixture::read_lines(&fixture.stdin)
        .into_iter()
        .map(|line| serde_json::from_str::<Value>(&line).expect("captured JSON"))
        .collect::<Vec<_>>();
    assert_eq!(
        sent,
        vec![
            user_receipt.value,
            json!({
                "type": "control_request",
                "request_id": "interrupt-1",
                "request": {"subtype": "interrupt"}
            }),
            json!({
                "type": "control_request",
                "request_id": "end-interrupt",
                "request": {
                    "subtype": "end_session",
                    "reason": "test_complete"
                }
            })
        ]
    );
}

#[test]
fn shutdown_after_protocol_abort_is_idempotent_and_sends_no_end_session() {
    let fixture = Fixture::create();
    let mut runtime = SdkRuntime::spawn(fixture.config("interrupt")).expect("spawn runtime");
    runtime.abort_nonblocking("test protocol root failure".to_string());
    assert_eq!(
        runtime
            .shutdown("unused-after-abort", Some("test"))
            .expect("aborting shutdown is already terminal"),
        ShutdownOutcome::AlreadyStopped
    );
    assert_eq!(
        runtime
            .shutdown("unused-after-stop", Some("test"))
            .expect("stopped shutdown remains idempotent"),
        ShutdownOutcome::AlreadyStopped
    );
    let captured_stdin = std::fs::read_to_string(&fixture.stdin).unwrap_or_default();
    assert!(
        captured_stdin.is_empty(),
        "an aborting runtime must not receive a second end_session command: {captured_stdin}"
    );
}

#[test]
fn graceful_shutdown_cannot_deadlock_behind_its_own_bounded_response() {
    let fixture = Fixture::create();
    let mut config = fixture.config("interrupt");
    config.limits.stdout_event_capacity = 1;
    let mut runtime = SdkRuntime::spawn(config).expect("spawn runtime");
    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = runtime.shutdown("bounded-end", None);
        let _ = done_tx.send((runtime, result));
    });
    let (runtime, result) = done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("shutdown blocked behind its own control response");
    result.expect("bounded graceful shutdown");
    let response = envelope(recv(&runtime));
    assert_eq!(
        response.value,
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "bounded-end",
                "response": {"ended": true}
            }
        })
    );
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::ChildExited(ref exit) if exit.expected && exit.success
    ));
}

#[test]
fn graceful_shutdown_pumps_and_preserves_full_stdout_and_stderr_backlogs() {
    let fixture = Fixture::create();
    let mut config = fixture.config("shutdown-backlog");
    config.limits.stdout_event_capacity = 1;
    config.limits.stderr_event_capacity = 1;
    let mut runtime = SdkRuntime::spawn(config).expect("spawn runtime");
    std::thread::sleep(Duration::from_millis(50));

    let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = runtime.shutdown("backlog-end", Some("drain"));
        let _ = done_tx.send((runtime, result));
    });
    let (runtime, result) = done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("shutdown deadlocked behind a full data queue");
    result.expect("backlog shutdown");

    for index in 0..4 {
        let raw = envelope(recv(&runtime));
        assert_eq!(raw.sequence, index);
        assert_eq!(raw.value, json!({"type": "keep_alive", "index": index}));
    }
    let response = envelope(recv(&runtime));
    assert_eq!(response.sequence, 4);
    assert_eq!(
        response.value,
        json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": "backlog-end",
                "response": {"ended": true}
            }
        })
    );
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::ChildExited(ref exit) if exit.expected && exit.success
    ));
    for index in 0..4 {
        let stderr = runtime
            .recv_stderr_timeout(Duration::from_secs(1))
            .expect("preserved stderr backlog");
        assert_eq!(stderr.sequence, index);
        assert_eq!(stderr.bytes, format!("shutdown-stderr-{index}").as_bytes());
    }
}

#[test]
fn shutdown_uses_one_wall_clock_deadline_even_with_full_output_queues() {
    let fixture = Fixture::create();
    let mut config = fixture.config("stdin-stall-backlogs");
    config.limits.shutdown_timeout = Duration::from_millis(150);
    config.limits.outbound_send_timeout = Duration::from_secs(2);
    config.limits.max_stdout_frame_bytes = 512;
    config.limits.stdout_event_capacity = 1;
    config.limits.stdout_queue_bytes = 512;
    config.limits.max_stderr_frame_bytes = 128;
    config.limits.stderr_event_capacity = 1;
    config.limits.stderr_queue_bytes = 128;
    let mut runtime = SdkRuntime::spawn(config).expect("spawn shutdown-deadline fixture");

    let started = Instant::now();
    let error = runtime
        .shutdown("deadline-end", None)
        .expect_err("child that ignores end_session must time out");
    assert!(
        matches!(error, ShutdownError::TimedOut),
        "unexpected shutdown deadline error: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "shutdown consumed more than its one configured wall-clock deadline"
    );
}

#[test]
fn stalled_stdin_with_full_output_queues_fail_closes_without_retry_or_join_deadlock() {
    let fixture = Fixture::create();
    let mut config = fixture.config("stdin-stall-backlogs");
    config.limits.outbound_send_timeout = Duration::from_millis(150);
    config.limits.shutdown_timeout = Duration::from_secs(1);
    config.limits.max_stdout_frame_bytes = 512;
    config.limits.stdout_event_capacity = 1;
    config.limits.stdout_queue_bytes = 512;
    config.limits.max_stderr_frame_bytes = 128;
    config.limits.stderr_event_capacity = 1;
    config.limits.stderr_queue_bytes = 128;
    let runtime = SdkRuntime::spawn(config).expect("spawn stalled-stdin fixture");
    let message = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": "x".repeat(8 * 1024 * 1024)
        },
        "uuid": "stalled-write"
    });

    let started = Instant::now();
    let error = runtime
        .send_user_message(message)
        .expect_err("writer must not wait forever for a child that does not read stdin");
    assert!(
        matches!(
            error,
            SendError::TimedOut {
                stage: SendTimeoutStage::WriterAcknowledgement,
                ..
            }
        ),
        "unexpected timeout error: {error}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "writer deadline did not bound the synchronous send"
    );

    assert!(matches!(
        runtime.send_user_message(json!({
            "type": "user",
            "message": {"role": "user", "content": "must-not-retry"}
        })),
        Err(SendError::Closed)
    ));

    let terminal_deadline = Instant::now() + Duration::from_secs(4);
    let mut saw_fatal = false;
    let mut saw_exit = false;
    while !saw_exit {
        assert!(
            Instant::now() < terminal_deadline,
            "timeout abort deadlocked while joining full stdout/stderr readers"
        );
        match runtime.recv_event_timeout(Duration::from_millis(250)) {
            Ok(RuntimeEvent::Envelope(_)) => {}
            Ok(RuntimeEvent::Fatal(fatal)) => {
                assert!(fatal.reason.contains("delivery is indeterminate"));
                assert!(fatal.reason.contains("must not be retried"));
                saw_fatal = true;
            }
            Ok(RuntimeEvent::ChildExited(exited)) => {
                assert!(!exited.expected && !exited.success);
                saw_exit = true;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("transport disconnected before terminal lifecycle events")
            }
        }
    }
    assert!(saw_fatal, "writer timeout must publish a fatal event");
    assert!(
        runtime.try_recv_stderr().is_ok(),
        "stderr queued before fail-close must remain observable"
    );
    assert!(
        Fixture::read_lines(&fixture.stdin).is_empty(),
        "the stalled child must not observe a complete retried frame"
    );
}

#[test]
fn broken_stdin_write_is_terminal_and_kills_a_still_running_child() {
    let fixture = Fixture::create();
    let mut config = fixture.config("stdin-closed");
    config.limits.outbound_send_timeout = Duration::from_secs(1);
    let runtime = SdkRuntime::spawn(config).expect("spawn closed-stdin fixture");
    wait_until(Duration::from_secs(2), || {
        std::fs::read_to_string(&fixture.stdin).is_ok_and(|contents| contents == "ready\n")
    });

    let error = runtime
        .send_user_message(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "x".repeat(1024 * 1024)
            }
        }))
        .expect_err("closed child stdin must reject the write");
    assert!(
        matches!(error, SendError::Write(_)),
        "unexpected closed-stdin error: {error}"
    );
    assert!(matches!(
        runtime.send_user_message(json!({
            "type": "user",
            "message": {"role": "user", "content": "must-not-send"}
        })),
        Err(SendError::Closed)
    ));
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::Fatal(ref fatal) if fatal.reason.contains("SDK stdin writer failed")
    ));
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::ChildExited(ref exited) if !exited.expected && !exited.success
    ));
}

#[test]
fn graceful_shutdown_requires_matching_success_response_and_successful_exit() {
    for (mode, expected) in [
        ("shutdown-no-response", "missing"),
        ("shutdown-error", "rejected"),
        ("shutdown-crash", "exit"),
    ] {
        let fixture = Fixture::create();
        let mut runtime = SdkRuntime::spawn(fixture.config(mode)).expect("spawn runtime");
        let error = runtime
            .shutdown(format!("end-{mode}"), None)
            .expect_err("invalid shutdown must not report success");
        match expected {
            "missing" => assert!(matches!(error, ShutdownError::MissingEndSessionResponse(_))),
            "rejected" => assert!(matches!(
                error,
                ShutdownError::EndSessionRejected { ref outcome, .. } if outcome == "error"
            )),
            "exit" => assert!(matches!(
                error,
                ShutdownError::UnsuccessfulExit { code: Some(42) }
            )),
            _ => unreachable!(),
        }
    }
}

#[test]
fn setup_phase_shutdown_reaps_without_writing_a_runtime_control_envelope() {
    let fixture = Fixture::create();
    let mut runtime =
        SdkRuntime::spawn(fixture.config("interrupt")).expect("spawn setup-phase fixture");
    wait_until(Duration::from_secs(3), || fixture.stdin.is_file());

    runtime
        .shutdown_before_runtime_handoff()
        .expect("force-reap setup-phase runtime");

    assert!(
        Fixture::read_lines(&fixture.stdin).is_empty(),
        "the setup router cannot accept end_session before StructuredIO handoff"
    );
}

#[test]
fn pending_control_request_state_is_bounded_in_both_directions() {
    let fixture = Fixture::create();
    let mut outbound_config = fixture.config("interrupt");
    outbound_config
        .limits
        .max_pending_control_requests_per_direction = 1;
    let outbound = SdkRuntime::spawn(outbound_config).expect("spawn outbound fixture");
    let session_receipt = outbound
        .send_control_request(
            "pending-1",
            json!({"subtype": "set_model", "model": "model-a"}),
        )
        .expect("first pending request");
    assert_eq!(
        session_receipt.value,
        json!({
            "type": "control_request",
            "request_id": "pending-1",
            "request": {
                "subtype": "set_model",
                "model": "model-a"
            }
        })
    );
    let error = outbound
        .send_control_request("pending-2", json!({"subtype": "set_model", "model": "b"}))
        .expect_err("second pending request must be rejected");
    assert!(error.to_string().contains("limit (1) exceeded"));
    drop(outbound);

    let fixture = Fixture::create();
    let mut reverse_config = fixture.config("reverse");
    reverse_config
        .limits
        .max_pending_control_requests_per_direction = 1;
    let reverse = SdkRuntime::spawn(reverse_config).expect("spawn reverse fixture");
    let first = envelope(recv(&reverse));
    assert!(matches!(
        first.correlation,
        Some(RequestCorrelation::ReverseRequestRegistered { .. })
    ));
    let second = envelope(recv(&reverse));
    assert_eq!(
        second.value["request"]["subtype"],
        Value::String("elicitation".to_string())
    );
    assert!(matches!(
        recv(&reverse),
        RuntimeEvent::Fatal(ref fatal) if fatal.reason.contains("limit (1) exceeded")
    ));
    assert!(matches!(recv(&reverse), RuntimeEvent::ChildExited(_)));
}

#[test]
fn terminal_shutdown_reserves_its_receipt_slot_when_pending_map_is_full() {
    let fixture = Fixture::create();
    let mut config = fixture.config("interrupt");
    config.limits.max_pending_control_requests_per_direction = 1;
    let mut runtime = SdkRuntime::spawn(config).expect("spawn saturated-correlation fixture");
    runtime
        .send_control_request("ordinary-pending", json!({"subtype": "get_context_usage"}))
        .expect("fill the ordinary outbound correlation capacity");

    runtime
        .shutdown("terminal-end", Some("test"))
        .expect("terminal shutdown must retain room for its required receipt");
    let sent = Fixture::read_lines(&fixture.stdin);
    assert_eq!(
        sent.iter()
            .filter(|line| line.contains("\"subtype\":\"end_session\""))
            .count(),
        1,
        "shutdown must enqueue exactly one terminal request"
    );
}

#[test]
fn spawn_failure_and_unexpected_child_exit_are_explicit() {
    // This integration target includes sdk_runtime.rs as a standalone module,
    // without terminal.rs. Keep the production emergency-cleanup entry point
    // type-checked here; terminal.rs owns the actual signal/panic invocation.
    let _terminal_cleanup_entrypoint: fn() = sdk_runtime::try_force_kill_active_runtimes;

    let fixture = Fixture::create();
    let mut missing = fixture.config("fields");
    missing.program = PathBuf::from("/tmp/crabcode-sdk-runtime-missing-program");
    assert!(matches!(
        SdkRuntime::spawn(missing),
        Err(SpawnError::Spawn(_))
    ));

    let runtime = SdkRuntime::spawn(fixture.config("crash")).expect("spawn crash fixture");
    let stderr = runtime
        .recv_stderr_timeout(Duration::from_secs(2))
        .expect("crash stderr");
    assert_eq!(stderr.bytes, b"crash-stderr");
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::Fatal(ref fatal) if fatal.reason.contains("exited unexpectedly")
    ));
    assert!(matches!(
        recv(&runtime),
        RuntimeEvent::ChildExited(ref exited)
            if !exited.expected && !exited.success && exited.code == Some(42)
    ));
}

#[test]
fn reserved_protocol_arguments_are_rejected_before_spawn() {
    let fixture = Fixture::create();
    for argument in [
        "--print",
        "--print=false",
        "--no-print",
        "--input-format=json",
        "--output-format",
        "--verbose",
        "--verbose=false",
        "--no-verbose",
        "--include-partial-messages",
        "--include-partial-messages=false",
        "--no-include-partial-messages",
        "--include-hook-events",
        "--include-hook-events=false",
        "--no-include-hook-events",
    ] {
        let mut config = fixture.config("fields");
        config.runtime_args = vec![OsString::from(argument)];
        assert!(matches!(
            SdkRuntime::spawn(config),
            Err(SpawnError::ReservedArgument(_))
        ));
    }
}

/// Executes the complete process-boundary transport contract as one immutable
/// evidence target. The individual tests remain the readable source of each
/// assertion; this aggregate exists so the backend-invariance gate can bind
/// one successful execution to the exact transport source and test revision
/// without treating source hashes as runtime proof.
#[test]
fn complete_backend_adapter_transport_contract_is_lossless() {
    spawn_uses_exact_fixed_protocol_arguments_and_preserves_every_field_in_order();
    split_packets_and_split_multibyte_codepoints_form_one_lossless_envelope();
    bounded_queue_applies_backpressure_without_loss_or_reordering();
    nonblocking_outbound_lane_is_bounded_fifo_and_ack_driven();
    nonblocking_writer_timeout_is_indeterminate_aborts_and_never_replays();
    nonblocking_reverse_response_reserves_then_commits_on_writer_ack();
    cancelled_reverse_response_is_rolled_back_before_writer_admission();
    permission_and_elicitation_responses_are_exact_and_request_correlated();
    reverse_control_error_is_exact_and_closes_only_the_matching_request();
    keep_alive_and_environment_update_have_exact_typed_stdin_wires();
    interrupt_and_shutdown_use_correlated_control_requests();
    graceful_shutdown_pumps_and_preserves_full_stdout_and_stderr_backlogs();
    pending_control_request_state_is_bounded_in_both_directions();
    additive_unknown_presentation_events_are_raw_and_runtime_remains_live();
    unknown_control_subtype_is_raw_then_fatal_and_terminated();
    additive_unknown_event_is_retained_before_unexpected_child_exit();
    malformed_json_and_oversized_stdout_fail_closed();
    spawn_failure_and_unexpected_child_exit_are_explicit();
}

/// Crosses the transport/projection seam for every non-product critical
/// adapter contract. The independent local-QueryEngine route remains outside
/// this test because proving that route requires a real initialized bundled
/// TypeScript child; this test must not replace it with a mock or a source
/// assertion.
#[test]
fn complete_backend_adapter_nonroute_critical_contracts_are_lossless() {
    use crabcode_tui::sdk_projection::{Projection, ProjectionEffect};
    use crabcode_tui::sdk_runtime::{
        EnvelopeClass as CrateEnvelopeClass, RawEnvelope as CrateRawEnvelope,
        classify_envelope as classify_crate_envelope,
    };

    complete_backend_adapter_transport_contract_is_lossless();

    let raw = |sequence: u64, value: Value| {
        let classification =
            classify_crate_envelope(&value).unwrap_or_else(|_| CrateEnvelopeClass::Unclassified {
                observed_type: value
                    .get("type")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                observed_system_subtype: value
                    .get("subtype")
                    .and_then(Value::as_str)
                    .map(str::to_string),
            });
        CrateRawEnvelope {
            sequence,
            encoded_len: serde_json::to_vec(&value)
                .expect("encode critical-contract fixture")
                .len(),
            value,
            classification,
            correlation: None,
        }
    };

    let mut projection = Projection::default();
    for (sequence, value) in [
        json!({
            "type":"system",
            "subtype":"init",
            "apiKeySource":"none",
            "crab_code_version":"1.0.0",
            "cwd":"/workspace",
            "tools":[],
            "mcp_servers":[],
            "model":"model",
            "permissionMode":"default",
            "slash_commands":[],
            "output_style":"default",
            "skills":[],
            "plugins":[],
            "session_id":"session-critical",
            "uuid":"init-critical"
        }),
        json!({
            "type":"system",
            "subtype":"status",
            "status":"compacting",
            "session_id":"session-critical",
            "uuid":"status-critical"
        }),
        json!({
            "type":"system",
            "subtype":"session_state_changed",
            "state":"idle",
            "session_id":"session-critical",
            "uuid":"state-critical"
        }),
        json!({
            "type":"stream_event",
            "uuid":"stream-critical",
            "session_id":"session-critical",
            "parent_tool_use_id":null,
            "event":{"type":"message_start","message":{"id":"message-critical"}}
        }),
        json!({
            "type":"stream_event",
            "uuid":"stream-critical",
            "session_id":"session-critical",
            "parent_tool_use_id":null,
            "event":{
                "type":"content_block_start",
                "index":0,
                "content_block":{"type":"text","text":""}
            }
        }),
        json!({
            "type":"stream_event",
            "uuid":"stream-critical",
            "session_id":"session-critical",
            "parent_tool_use_id":null,
            "event":{
                "type":"content_block_delta",
                "index":0,
                "delta":{"type":"text_delta","text":"lossless"}
            }
        }),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(
            !matches!(
                projection.ingest(raw(sequence as u64, value.clone())),
                ProjectionEffect::FailClosed { .. }
            ),
            "declared critical-contract fixture must remain accepted: {value}"
        );
        assert_eq!(
            projection.raw_envelopes().last().map(|entry| &entry.value),
            Some(&value)
        );
    }
    assert_eq!(projection.session_id(), Some("session-critical"));
    assert_eq!(projection.session_state(), Some("idle"));
    assert!(
        projection
            .items()
            .iter()
            .any(|item| item.text == "lossless")
    );

    for (sequence, subtype) in [
        "success",
        "error_during_execution",
        "error_max_budget_usd",
        "error_max_structured_output_retries",
        "error_max_turns",
    ]
    .into_iter()
    .enumerate()
    {
        let is_success = subtype == "success";
        let mut value = json!({
            "type":"result",
            "subtype":subtype,
            "duration_ms":1,
            "duration_api_ms":1,
            "is_error":!is_success,
            "num_turns":1,
            "stop_reason":null,
            "total_cost_usd":0,
            "usage":{},
            "modelUsage":{},
            "permission_denials":[],
            "session_id":"session-critical",
            "uuid":format!("result-critical-{sequence}")
        });
        if is_success {
            value
                .as_object_mut()
                .expect("result object")
                .insert("result".to_string(), Value::String("done".to_string()));
        } else {
            value.as_object_mut().expect("result object").insert(
                "errors".to_string(),
                Value::Array(vec![Value::String("source error".to_string())]),
            );
        }
        let effect = projection.ingest(raw((100 + sequence) as u64, value.clone()));
        assert!(matches!(
            effect,
            ProjectionEffect::TurnCompleted {
                ref subtype,
                raw_sequence,
                ..
            } if subtype == value["subtype"].as_str().expect("result subtype")
                && raw_sequence == (100 + sequence) as u64
        ));
        assert_eq!(
            projection.raw_envelopes().last().map(|entry| &entry.value),
            Some(&value)
        );
    }

    let projected_item_count_before_future_event = projection.items().len();
    let future_event = json!({
        "type":"stream_event",
        "uuid":"future-event",
        "session_id":"session-critical",
        "parent_tool_use_id":null,
        "event":{"type":"future_stream_event","opaque":{"retained":true}}
    });
    assert_eq!(
        projection.ingest(raw(200, future_event.clone())),
        ProjectionEffect::CompatibilityFault {
            sequence: 200,
            event_type: "future_stream_event".to_string(),
            code: "unknown_stream_event".to_string(),
        }
    );
    assert_eq!(
        projection.raw_envelopes().last().map(|entry| &entry.value),
        Some(&future_event)
    );
    assert_eq!(
        projection.items().len(),
        projected_item_count_before_future_event,
        "unknown presentation events are diagnosed by the typed effect without inventing transcript content"
    );

    let future_delta = json!({
        "type":"stream_event",
        "uuid":"future-delta",
        "session_id":"session-critical",
        "parent_tool_use_id":null,
        "event":{
            "type":"content_block_delta",
            "index":0,
            "delta":{"type":"future_delta","opaque":{"retained":true}}
        }
    });
    assert_eq!(
        projection.ingest(raw(201, future_delta.clone())),
        ProjectionEffect::None
    );
    assert_eq!(
        projection.raw_envelopes().last().map(|entry| &entry.value),
        Some(&future_delta)
    );
    assert!(projection.items().iter().any(|item| {
        item.title == "Renderer compatibility" && item.text.contains("future_delta")
    }));
}

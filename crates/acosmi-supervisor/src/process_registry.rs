//! Managed process registry for the Go↔Rust capability.spawn pathway.
//!
//! `CapabilityFamily::SpawnManaged` is implemented here, with stdout /
//! stderr / exit 以 `MessageEnvelope{msg_type=event, payload=EventPayload}`
//! 的形式回推到 Go UDS 连接，让 broker 的 `DeliverStdout` pipeline 真正
//! 通路。
//!
//! # 设计
//!
//! - 每个 spawn 得到一个 [`ProcessSlot`]，由 UUID 生成四个独立 handle ID：
//!   `stdin_write_id / stdout_read_id / stderr_read_id / kill_id`。
//! - 注册表按**各 ID** 建索引，使 `write_stdin(id, data)` 与 `kill(id)` 直接
//!   按 Go 传入的 ID 定位；无需 Go 端额外存"主 ID"。
//! - spawn 后立即 `tokio::spawn` 一个 supervisor task 作为 slot 的**唯一**
//!   child owner：读 stdout / stderr 推 event；等 `wait()` 或 `kill_token`
//!   触发 `kill_tree()`；最终发 `process.exit` event + 从注册表摘除所有 ID。
//! - 注册表生命周期与单个 Go UDS 连接绑定：连接断开时调 [`ProcessRegistry::shutdown_all`]
//!   kill 所有进程——避免 Go 进程退出后 MCP server 孤儿进程。

use std::collections::HashMap;
use std::sync::Arc;

use acosmi_exec::process::ProcessBuilder;
use acosmi_types::protocol::ManagedProcessHandle;
use base64::Engine;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

/// 单个托管进程的运行时状态。`Arc` 共享给 registry 索引 + supervisor task。
struct ProcessSlot {
    pid: u32,
    stdin_write_id: String,
    stdout_read_id: String,
    stderr_read_id: String,
    kill_id: String,
    /// 写入 stdin 的句柄；supervisor task 不读这个字段——只有外部 `write_stdin`
    /// 调用会短暂 lock 并写入。`Option::None` 表示子进程已 drop stdin。
    stdin: Mutex<Option<ChildStdin>>,
    /// 触发 supervisor task 提前 `kill_tree` 的信号。
    kill_token: CancellationToken,
}

/// 进程注册表——单个 Go UDS 连接的 scope。
pub struct ProcessRegistry {
    /// 以 `stdin_write_id` 定位 slot
    by_stdin: Mutex<HashMap<String, Arc<ProcessSlot>>>,
    /// 以 `kill_id` 定位 slot
    by_kill: Mutex<HashMap<String, Arc<ProcessSlot>>>,
    /// 所有 slot 的引用汇总，用于 `shutdown_all`
    all: Mutex<Vec<Arc<ProcessSlot>>>,
}

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            by_stdin: Mutex::new(HashMap::new()),
            by_kill: Mutex::new(HashMap::new()),
            all: Mutex::new(Vec::new()),
        }
    }

    /// 启动一个托管子进程。
    ///
    /// 成功返回 schema 对齐的 `ManagedProcessHandle`。`outbox` 是 Rust→Go
    /// 的通用消息发送端；supervisor task 把 stdout / stderr / exit 包成
    /// `MessageEnvelope{msg_type=event}` 发回。
    pub async fn spawn(
        self: &Arc<Self>,
        program: String,
        args: Vec<String>,
        outbox: mpsc::Sender<serde_json::Value>,
    ) -> anyhow::Result<ManagedProcessHandle> {
        let mut proc = ProcessBuilder::new(&program)
            .args(&args)
            .stdin(acosmi_exec::StdinConfig::Piped)
            .stdout(acosmi_exec::OutputConfig::Piped)
            .stderr(acosmi_exec::OutputConfig::Piped)
            .spawn()
            .await
            .map_err(|e| anyhow::anyhow!("spawn {program} failed: {e}"))?;

        let pid = proc.pid;
        let stdin = proc.take_stdin();
        let stdout = proc.take_stdout();
        let stderr = proc.take_stderr();

        let stdin_write_id = format!("stdin-{}", uuid::Uuid::new_v4());
        let stdout_read_id = format!("stdout-{}", uuid::Uuid::new_v4());
        let stderr_read_id = format!("stderr-{}", uuid::Uuid::new_v4());
        let kill_id = format!("kill-{}", uuid::Uuid::new_v4());

        let slot = Arc::new(ProcessSlot {
            pid,
            stdin_write_id: stdin_write_id.clone(),
            stdout_read_id: stdout_read_id.clone(),
            stderr_read_id: stderr_read_id.clone(),
            kill_id: kill_id.clone(),
            stdin: Mutex::new(stdin),
            kill_token: CancellationToken::new(),
        });

        {
            let mut idx_stdin = self.by_stdin.lock().await;
            let mut idx_kill = self.by_kill.lock().await;
            let mut all = self.all.lock().await;
            idx_stdin.insert(stdin_write_id.clone(), Arc::clone(&slot));
            idx_kill.insert(kill_id.clone(), Arc::clone(&slot));
            all.push(Arc::clone(&slot));
        }

        let registry = Arc::clone(self);
        let slot_task = Arc::clone(&slot);
        // JoinHandle held by caller — slot supervisor task is owned by
        // the registry's own slot via the `slot_task` Arc and reaped
        // through `Self::supervise`'s natural completion path. Bare
        // `tokio::spawn` is acceptable here because slot termination
        // is handled by the registry's own state machine, not by
        // dropping a JoinHandle. Audited Step 2 Phase D.1.
        #[allow(clippy::disallowed_methods)]
        tokio::spawn(async move {
            Self::supervise(registry, slot_task, proc, stdout, stderr, outbox).await;
        });

        tracing::info!(
            pid,
            program = %program,
            stdin_write_id = %stdin_write_id,
            stdout_read_id = %stdout_read_id,
            "capability.spawn: process spawned"
        );

        Ok(ManagedProcessHandle {
            pid,
            stdin_write_id,
            stdout_read_id,
            stderr_read_id: Some(stderr_read_id),
            kill_id: Some(kill_id),
            shell_type: Some("none".to_string()),
        })
    }

    /// 向子进程 stdin 写入原始字节（调用方应自行附加换行）。
    pub async fn write_stdin(&self, stdin_write_id: &str, data: &[u8]) -> anyhow::Result<()> {
        let slot = {
            let idx = self.by_stdin.lock().await;
            idx.get(stdin_write_id).cloned()
        };
        let slot =
            slot.ok_or_else(|| anyhow::anyhow!("unknown stdin_write_id: {stdin_write_id}"))?;

        let mut guard = slot.stdin.lock().await;
        let sink = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("stdin already closed for {stdin_write_id}"))?;
        sink.write_all(data)
            .await
            .map_err(|e| anyhow::anyhow!("stdin write failed: {e}"))?;
        sink.flush()
            .await
            .map_err(|e| anyhow::anyhow!("stdin flush failed: {e}"))?;
        Ok(())
    }

    /// 终止进程（先 SIGTERM 再 SIGKILL 由 `kill_tree` 实现处理，此处仅触发 token）。
    /// 返回 `true` 表示触发成功；`false` 表示 `kill_id` 未知。
    pub async fn kill(&self, kill_id: &str) -> bool {
        let idx = self.by_kill.lock().await;
        match idx.get(kill_id) {
            Some(slot) => {
                slot.kill_token.cancel();
                true
            }
            None => false,
        }
    }

    /// 连接断开时调用，触发所有 slot 的 kill，避免孤儿进程。
    pub async fn shutdown_all(&self) {
        let slots: Vec<_> = {
            let all = self.all.lock().await;
            all.iter().cloned().collect()
        };
        for slot in slots {
            slot.kill_token.cancel();
        }
    }

    /// 从所有索引摘除一个已退出的 slot。
    async fn drop_slot(&self, slot: &ProcessSlot) {
        let mut idx_stdin = self.by_stdin.lock().await;
        let mut idx_kill = self.by_kill.lock().await;
        let mut all = self.all.lock().await;
        idx_stdin.remove(&slot.stdin_write_id);
        idx_kill.remove(&slot.kill_id);
        all.retain(|s| s.pid != slot.pid);
    }

    /// Supervisor task — slot 的唯一 child owner。负责：
    /// - 逐行读 stdout → `process.stdout` event
    /// - 逐行读 stderr → `process.stderr` event
    /// - 等待 wait 完成 or `kill_token` 触发 → `kill_tree`
    /// - 最终发 `process.exit` + 清理注册表
    async fn supervise(
        registry: Arc<Self>,
        slot: Arc<ProcessSlot>,
        mut proc: acosmi_exec::process::ManagedProcess,
        stdout: Option<tokio::process::ChildStdout>,
        stderr: Option<tokio::process::ChildStderr>,
        outbox: mpsc::Sender<serde_json::Value>,
    ) {
        let stdout_id = slot.stdout_read_id.clone();
        let stderr_id = slot.stderr_read_id.clone();
        let outbox_stdout = outbox.clone();
        let outbox_stderr = outbox.clone();

        let stdout_task = stdout.map(|s| {
            let id = stdout_id.clone();
            // JoinHandle returned to caller via Option<JoinHandle> — caller
            // awaits or aborts via the surrounding `tokio::select!` race
            // against `wait()` and `kill_token`. Bare-discard regression
            // audited Step 2 Phase D.1.
            #[allow(clippy::disallowed_methods)]
            tokio::spawn(async move {
                relay_stream(s, &id, "process.stdout", outbox_stdout).await;
            })
        });
        let stderr_task = stderr.map(|s| {
            let id = stderr_id.clone();
            // JoinHandle returned to caller — same pattern as stdout above.
            #[allow(clippy::disallowed_methods)]
            tokio::spawn(async move {
                relay_stream(s, &id, "process.stderr", outbox_stderr).await;
            })
        });

        // Race wait vs kill_token.
        let exit_code: i32;
        tokio::select! {
            wait_result = proc.wait() => {
                exit_code = wait_result.unwrap_or(-1);
            }
            () = slot.kill_token.cancelled() => {
                tracing::info!(pid = slot.pid, "capability.spawn: kill_token triggered, killing process tree");
                let _ = proc.kill_tree().await;
                exit_code = proc.wait().await.unwrap_or(-1);
            }
        }

        // Let the reader tasks drain remaining output before we emit exit.
        if let Some(t) = stdout_task {
            let _ = t.await;
        }
        if let Some(t) = stderr_task {
            let _ = t.await;
        }

        // Emit process.exit.
        let exit_env = build_event_envelope(
            "process.exit",
            serde_json::json!({
                "stdout_read_id": &slot.stdout_read_id,
                "exit_code": exit_code,
            }),
        );
        let _ = outbox.send(exit_env).await;

        registry.drop_slot(&slot).await;
        tracing::info!(
            pid = slot.pid,
            exit_code,
            "capability.spawn: supervisor task done"
        );
    }
}

/// Read chunks from an async byte stream and emit each line as a
/// `process.stdout` / `process.stderr` event. base64-encodes the raw bytes
/// so the Go side can reproduce them verbatim regardless of UTF-8 validity.
async fn relay_stream<R>(
    reader: R,
    stdout_read_id: &str,
    event_name: &str,
    outbox: mpsc::Sender<serde_json::Value>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = BufReader::with_capacity(8 * 1024, reader);
    let mut line = Vec::with_capacity(256);
    loop {
        line.clear();
        match buf.read_until(b'\n', &mut line).await {
            Ok(0) => return, // EOF
            Ok(_) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(&line);
                let env = build_event_envelope(
                    event_name,
                    serde_json::json!({
                        "stdout_read_id": stdout_read_id,
                        "chunk_base64": b64,
                    }),
                );
                if outbox.send(env).await.is_err() {
                    // downstream closed — give up; supervisor still handles kill/wait.
                    return;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "relay_stream read error");
                return;
            }
        }
    }
}

/// Build a `MessageEnvelope{msg_type=event, payload=EventPayload{event_name, data}}`
/// conformant to `contracts/schemas/message-envelope.schema.json`. Go side
/// dispatches on `event_name` via `Bridge.OnEvent`.
fn build_event_envelope(event_name: &str, data: serde_json::Value) -> serde_json::Value {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    serde_json::json!({
        "header": {
            "version": 1,
            "msg_id": format!("rust-evt-{now_ms}-{}", uuid::Uuid::new_v4()),
            "timestamp_ms": now_ms,
            "source_component": "rust",
        },
        "msg_type": "event",
        "channel": "rust_go",
        "payload": {
            "event_name": event_name,
            "data": data,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn echo_cmd_args(line: &str) -> (String, Vec<String>) {
        #[cfg(windows)]
        {
            (
                "cmd.exe".to_string(),
                vec!["/C".to_string(), format!("echo {line}")],
            )
        }
        #[cfg(not(windows))]
        {
            ("echo".to_string(), vec![line.to_string()])
        }
    }

    #[tokio::test]
    async fn spawn_echo_emits_stdout_and_exit_events() {
        let reg = Arc::new(ProcessRegistry::new());
        let (tx, mut rx) = mpsc::channel::<serde_json::Value>(32);
        let (program, args) = echo_cmd_args("hello-proc-registry");

        let handle = reg
            .spawn(program, args, tx)
            .await
            .expect("spawn should succeed");

        assert!(handle.pid > 0);
        assert!(!handle.stdin_write_id.is_empty());
        assert!(!handle.stdout_read_id.is_empty());
        assert!(handle.kill_id.is_some());

        // Collect events until we see a process.exit.
        let mut saw_stdout = false;
        let mut saw_exit = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline && !saw_exit {
            let recv_fut = rx.recv();
            let ev = match tokio::time::timeout(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
                recv_fut,
            )
            .await
            {
                Ok(Some(v)) => v,
                _ => break,
            };
            let event_name = ev
                .get("payload")
                .and_then(|p| p.get("event_name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let data = ev.get("payload").and_then(|p| p.get("data"));
            match event_name {
                "process.stdout" => {
                    if let Some(b64) = data
                        .and_then(|d| d.get("chunk_base64"))
                        .and_then(|v| v.as_str())
                    {
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .expect("chunk_base64 decodable");
                        let line = String::from_utf8_lossy(&bytes);
                        if line.contains("hello-proc-registry") {
                            saw_stdout = true;
                        }
                    }
                }
                "process.exit" => {
                    saw_exit = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_stdout,
            "did not observe stdout event with expected chunk"
        );
        assert!(saw_exit, "did not observe process.exit event");
    }

    #[tokio::test]
    async fn kill_triggers_exit_event_and_removes_slot() {
        let reg = Arc::new(ProcessRegistry::new());
        let (tx, mut rx) = mpsc::channel::<serde_json::Value>(32);

        // A long-lived process: use a platform-appropriate sleeper.
        #[cfg(windows)]
        let (program, args) = (
            "cmd.exe".to_string(),
            vec!["/C".to_string(), "pause >nul".to_string()],
        );
        #[cfg(not(windows))]
        let (program, args) = (
            "sh".to_string(),
            vec!["-c".to_string(), "sleep 30".to_string()],
        );

        let handle = reg
            .spawn(program, args, tx)
            .await
            .expect("spawn long-lived should succeed");

        let kill_id = handle.kill_id.clone().expect("kill_id present");
        // Give the spawn a beat to wire up pipes before we kill.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(reg.kill(&kill_id).await, "kill should find the slot");

        // Exit event should arrive within a few seconds.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut saw_exit = false;
        while tokio::time::Instant::now() < deadline && !saw_exit {
            let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remain, rx.recv()).await {
                Ok(Some(ev)) => {
                    let name = ev
                        .get("payload")
                        .and_then(|p| p.get("event_name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    if name == "process.exit" {
                        saw_exit = true;
                    }
                }
                _ => break,
            }
        }
        assert!(saw_exit, "kill did not produce process.exit event");

        // Slot should be gone from the registry.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let still_knows = reg.kill(&kill_id).await;
        assert!(!still_knows, "slot should be removed after exit");
    }

    #[tokio::test]
    async fn write_stdin_rejects_unknown_id() {
        let reg = ProcessRegistry::new();
        let err = reg.write_stdin("does-not-exist", b"hello\n").await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn kill_unknown_id_returns_false() {
        let reg = ProcessRegistry::new();
        assert!(!reg.kill("ghost-kill-id").await);
    }
}

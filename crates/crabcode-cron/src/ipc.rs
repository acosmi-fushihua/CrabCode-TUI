//! crabcode-cron IPC 服务器（双平台）
//!
//! - Unix：监听 `~/.crabcode/run/cron.sock`（UnixListener），accept→spawn handle_conn
//! - Windows：监听 `\\.\pipe\crabcode-<USER>-cron`（NamedPipeServer），
//!   `connect().await`→spawn handle_conn→立刻 create 下一个 instance 接续 accept
//!
//! 两端都接 newline-delimited JSON 请求，业务侧 `handle_request_line` /
//! `sub_*` 完全平台无关；只有 listener bind / accept 和 stream 类型分平台。
//!
//! ## 协议（WT3 契约）
//!
//! 每条消息 = 一行 JSON（`\n` 终止）。请求格式：
//! ```text
//! {"id": "<任意字符串>", "method": "cron.<sub>", "params": {...}}
//! ```
//! `cron.add/update/remove/run` additionally require
//! `params.expectedStateIdentity`. The daemon compares it with the immutable
//! identity captured at startup before it sends any scheduler command.
//!
//! `<sub>` ∈ `{list, add, remove, update, status, poll_events, run, runs,
//! outbox_read, claim, accept, report_failure, retry, cancel,
//! occurrence_history, delivery_begin, delivery_accept, delivery_abandon}`。
//! `list..runs` 与 `acosmi-supervisor::ipc::handle_cron_method`
//! 子命令集 1:1 对齐（立宪 3 阶段 3-D 补全 `run` / `runs`）。
//! `claim`（PR5-W2-3）+ `accept` / `report_failure` / `retry` / `cancel` /
//! `occurrence_history`（PR5-W3-3）是 SQLite occurrence 账本的 claim/lease 与
//! result/lifecycle 面：前五者是 mutation（要 `expectedStateIdentity`，但
//! occurrence-keyed，不带 `expectedLedgerEpoch`）；`occurrence_history` 是只读
//! 审计（不设身份闸门）。
//! `outbox_read`（W-CRON-AUTOMATION-E2E P7）是 cursor 式持久 outbox 读取 ——
//! 幂等（非 drain），供 REPL 在 daemon 重启 / 离线后恢复未消费的 fire 事件。
//! `cron.subscribe` 是另设的长连推送 method（见下方
//! `parse_subscribe_request`）。
//!
//! 响应格式：
//! ```text
//! {"id": "<同请求 id>", "result": <method-specific JSON>}
//! ```
//!
//! `result` 字段值与 supervisor `handle_cron_method` 返回的 JSON 完全一致：
//! - 成功：`{"status": "ok", ...}`
//! - 业务失败：`{"status": "error", "error": "..."}`
//!
//! 协议级错误（method 缺失 / json 解析失败 / 未知 method）走顶层 `error` 字段：
//! ```text
//! {"id": "<id-or-null>", "error": {"code": <int>, "message": "..."}}
//! ```
//!
//! ## 与 supervisor IPC 的差异
//!
//! - supervisor IPC 走 acosmi-types::protocol 的 envelope+trace 帧；这里直接
//!   走 newline-delimited JSON，因为 cron daemon 与 supervisor 是不同子系统、
//!   不同 binary，无理由强行复用 envelope。
//! - WT3 cron CLI 客户端按本协议直连 cron.sock；WT2 supervisor 改造时如需
//!   桥接 supervisor IPC → cron.sock，自行做协议转换。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use acosmi_scheduler::CronCommand;
use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use acosmi_cron_ledger::{
    CancelOutcome, ClaimOutcome, ConsumerClaim, ConsumerKind, EnvelopeTarget, ReportedFailureKind,
    RetryOutcome, WritebackOutcome,
};

use crate::CronLedgerWriter;
use crate::delivery::{
    DeliveryAbandonOutcome, DeliveryAcceptOutcome, DeliveryBeginOutcome, DeliveryConsumer,
    DeliveryError,
};

#[cfg(unix)]
use tokio::net::UnixListener;
// Windows pipe creation uses the shared daemon-launcher helper with a per-user
// DACL and reject_remote_clients, matching Unix socket mode 0600.
#[cfg(windows)]
use acosmi_daemon_launcher::windows_pipe::create_pipe_server;

/// daemon CRUD 的 reply oneshot 超时上限。daemon 应在下一个 tick（~1s）完成，
/// 2s 足够覆盖 check() 长尾。超时返错让调用方感知 daemon 卡死。
const CMD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub type EventQueue = Arc<Mutex<Vec<serde_json::Value>>>;

/// fire 事件广播 channel（Push 模型）。
///
/// `cron.subscribe` IPC 方法把 client connection 切为 long-running，连接的
/// write_half 持续 yield daemon 主循环 `broadcast_tx.send()` 推过来的 fire 事件。
/// channel 容量 64：tick 1s + Mac fast loops 同 tick 多 fire 事件下不会撞 lag。
/// lagging receiver 单事件丢失不致命 —— `cron.poll_events` 保留作 fallback +
/// reconnect 后从最近 lastFiredAt 不丢。
pub type EventBroadcaster = broadcast::Sender<serde_json::Value>;

/// 启动 IPC 服务器。`shutdown` 触发后干净退出。
///
/// - Unix：UnixListener 监听 `sock_path`，accept→spawn handle_conn
/// - Windows：`sock_path` 是 `\\.\pipe\...`，每轮 `ServerOptions::new().create()`
///   建一个 NamedPipeServer instance，`connect().await` 等客户端，spawn
///   handle_conn 并 immediately create 下一个 instance（保持持续 accept）。
// C2 ready/shutdown and C3 immutable state identity are deliberately explicit
// lifecycle capabilities. Bundling them into a catch-all context would hide
// which values are cloned into connection tasks and weaken reviewability.
#[allow(clippy::too_many_arguments)]
pub async fn serve(
    sock_path: PathBuf,
    cmd_tx: mpsc::Sender<CronCommand>,
    events: EventQueue,
    broadcaster: EventBroadcaster,
    cron_dir: PathBuf,
    ledger_writer: CronLedgerWriter,
    state_identity: String,
    ledger_epoch: String,
    shutdown: CancellationToken,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    #[cfg(unix)]
    {
        serve_unix(
            sock_path,
            cmd_tx,
            events,
            broadcaster,
            cron_dir,
            ledger_writer,
            state_identity,
            ledger_epoch,
            shutdown,
            ready,
        )
        .await
    }
    #[cfg(windows)]
    {
        serve_windows(
            sock_path,
            cmd_tx,
            events,
            broadcaster,
            cron_dir,
            ledger_writer,
            state_identity,
            ledger_epoch,
            shutdown,
            ready,
        )
        .await
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
async fn serve_unix(
    sock_path: PathBuf,
    cmd_tx: mpsc::Sender<CronCommand>,
    events: EventQueue,
    broadcaster: EventBroadcaster,
    cron_dir: PathBuf,
    ledger_writer: CronLedgerWriter,
    state_identity: String,
    ledger_epoch: String,
    shutdown: CancellationToken,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let listener = bind_listener(&sock_path)
        .with_context(|| format!("UnixListener::bind({})", sock_path.display()))?;
    // `cron.pid` is the launcher's ready marker. Main waits for this signal
    // before publishing it, so a live PID can no longer race ahead of bind.
    let _ = ready.send(());

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                tracing::info!("IPC accept loop 收到 shutdown");
                break;
            }
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let cmd_tx = cmd_tx.clone();
                        let events = Arc::clone(&events);
                        let broadcaster = broadcaster.clone();
                        let cron_dir = cron_dir.clone();
                        let ledger_writer = ledger_writer.clone();
                        let state_identity = state_identity.clone();
                        let ledger_epoch = ledger_epoch.clone();
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_conn(stream, cmd_tx, events, broadcaster, cron_dir, ledger_writer, state_identity, ledger_epoch, shutdown).await {
                                tracing::warn!(error = %e, "cron IPC connection 错误");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "accept 失败");
                        // 不立即 break：临时 EMFILE 等错误不该挂掉整个 daemon
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    // 关停：尝试 unlink socket 文件（best-effort；main 也会做一次幂等）
    if sock_path.exists() {
        let _ = std::fs::remove_file(&sock_path);
    }
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn serve_windows(
    pipe_path: PathBuf,
    cmd_tx: mpsc::Sender<CronCommand>,
    events: EventQueue,
    broadcaster: EventBroadcaster,
    cron_dir: PathBuf,
    ledger_writer: CronLedgerWriter,
    state_identity: String,
    ledger_epoch: String,
    shutdown: CancellationToken,
    ready: oneshot::Sender<()>,
) -> Result<()> {
    let pipe_name = pipe_path.to_string_lossy().to_string();
    // 第一个 instance 必须用 first_pipe_instance(true) 防被其他进程抢占；
    // create_pipe_server 同时带 per-user DACL + reject_remote_clients（PR-4）。
    let mut server = create_pipe_server(&pipe_name, true)
        .with_context(|| format!("NamedPipe create({pipe_name})"))?;
    // The first secure pipe instance is now owned by this process. Only this
    // point may satisfy the main-process startup barrier.
    let _ = ready.send(());

    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                tracing::info!("IPC accept loop 收到 shutdown（Windows pipe）");
                break;
            }
            connect_result = server.connect() => {
                match connect_result {
                    Ok(()) => {
                        // 当前 server 已连上 client，把它移交 worker；同时立刻
                        // 创建下一个 instance 维持后续 connect 接续。
                        let connected = server;
                        server = match create_pipe_server(&pipe_name, false) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!(error = %e, "重建下一个 pipe instance 失败，退出 accept loop");
                                // 把已连上的 worker 跑完再回收
                                let cmd_tx = cmd_tx.clone();
                                let events = Arc::clone(&events);
                                let broadcaster = broadcaster.clone();
                                let cron_dir = cron_dir.clone();
                                let ledger_writer = ledger_writer.clone();
                                let state_identity = state_identity.clone();
                                let ledger_epoch = ledger_epoch.clone();
                                let shutdown = shutdown.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = handle_conn(connected, cmd_tx, events, broadcaster, cron_dir, ledger_writer, state_identity, ledger_epoch, shutdown).await {
                                        tracing::warn!(error = %e, "cron IPC connection 错误");
                                    }
                                });
                                break;
                            }
                        };
                        let cmd_tx = cmd_tx.clone();
                        let events = Arc::clone(&events);
                        let broadcaster = broadcaster.clone();
                        let cron_dir = cron_dir.clone();
                        let ledger_writer = ledger_writer.clone();
                        let state_identity = state_identity.clone();
                        let ledger_epoch = ledger_epoch.clone();
                        let shutdown = shutdown.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_conn(connected, cmd_tx, events, broadcaster, cron_dir, ledger_writer, state_identity, ledger_epoch, shutdown).await {
                                tracing::warn!(error = %e, "cron IPC connection 错误");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "NamedPipe connect 失败");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        }
    }

    // Windows Named Pipe 无残留文件需清理；instance 随 daemon 退出由 OS 回收。
    Ok(())
}

#[cfg(unix)]
fn bind_listener(sock_path: &Path) -> std::io::Result<UnixListener> {
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // 上层 main.rs 已 unlink；这里再做一次防御性 unlink，避免 unit test 顺序
    // 或上次崩溃残留导致 EADDRINUSE。
    if sock_path.exists() {
        let _ = std::fs::remove_file(sock_path);
    }
    UnixListener::bind(sock_path)
}

#[allow(clippy::too_many_arguments)]
async fn handle_conn<S>(
    stream: S,
    cmd_tx: mpsc::Sender<CronCommand>,
    events: EventQueue,
    broadcaster: EventBroadcaster,
    cron_dir: PathBuf,
    ledger_writer: CronLedgerWriter,
    state_identity: String,
    ledger_epoch: String,
    shutdown: CancellationToken,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        let read = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                tracing::debug!("conn 主动关闭（shutdown）");
                return Ok(());
            }
            r = reader.read_line(&mut line) => r,
        };

        match read {
            Ok(0) => return Ok(()), // EOF
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }

        // 特判 cron.subscribe：把 conn 切到 long-running push 模式。
        // 解析失败 / method 不是 cron.subscribe → 走 handle_request_line 标准路径。
        if let Some(request) = parse_subscribe_request(&line) {
            let result = match identity_mismatch(
                request.expected_state_identity.as_deref(),
                &state_identity,
                false,
            ) {
                Some(error) => error,
                None => with_identity_metadata(
                    serde_json::json!({"status": "ok", "subscribed": true}),
                    request.expected_state_identity.as_deref(),
                    &state_identity,
                ),
            };
            let is_ok = result.get("status").and_then(serde_json::Value::as_str) == Some("ok");
            let ack = serde_json::json!({"id": request.id, "result": result});
            let mut ack_bytes = serde_json::to_vec(&ack).unwrap_or_default();
            ack_bytes.push(b'\n');
            write_half.write_all(&ack_bytes).await?;
            if !is_ok {
                return Ok(());
            }
            return run_subscribe_loop(reader, write_half, broadcaster, shutdown).await;
        }

        let resp = handle_request_line_for_state_with_ledger(
            &line,
            &cmd_tx,
            &events,
            &cron_dir,
            Some(&ledger_writer),
            &state_identity,
            &ledger_epoch,
        )
        .await;
        let mut bytes = serde_json::to_vec(&resp).unwrap_or_else(|e| {
            // 序列化失败走最小 error 帧
            format!(
                r#"{{"id":null,"error":{{"code":-32603,"message":"response serialize failed: {e}"}}}}"#
            )
            .into_bytes()
        });
        bytes.push(b'\n');
        write_half.write_all(&bytes).await?;
    }
}

/// 检测一条已读 line 是否是 `cron.subscribe` 请求。
///
/// 是 → 返回请求 id（用作 ack 的 id 字段）。否则 None，走标准 `handle_request_line` 路径。
/// 解析失败也返 None —— 让标准路径返协议错误，subscribe 路径不打补丁。
struct SubscribeRequest {
    id: serde_json::Value,
    expected_state_identity: Option<String>,
}

fn parse_subscribe_request(line: &str) -> Option<SubscribeRequest> {
    let req: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    let method = req.get("method")?.as_str()?;
    if method == "cron.subscribe" {
        Some(SubscribeRequest {
            id: req.get("id").cloned().unwrap_or(serde_json::Value::Null),
            expected_state_identity: req
                .get("params")
                .and_then(|params| params.get("expectedStateIdentity"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        })
    } else {
        None
    }
}

/// `cron.subscribe` 长连接推送循环。
///
/// 三个事件源并行 select：
/// 1. shutdown.cancelled() → 干净退出
/// 2. broadcaster.recv() → 写 fire 事件 JSON 给 client
/// 3. reader.read_line() → client EOF 即退出（subscribe 后 client 不再发请求；
///    后续 line 静默丢弃，仅作为活性探针）
///
/// `broadcast::Lagged` 不致命：log warn + 继续 listen。client 端用 `cron.poll_events`
/// fallback 或 reconnect 时按 lastFiredAt 补 missed 事件。
async fn run_subscribe_loop<R, W>(
    mut reader: BufReader<R>,
    mut write_half: W,
    broadcaster: EventBroadcaster,
    shutdown: CancellationToken,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut rx = broadcaster.subscribe();
    let mut sink = String::new();
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => return Ok(()),
            ev = rx.recv() => match ev {
                Ok(event) => {
                    let mut bytes = match serde_json::to_vec(&event) {
                        Ok(b) => b,
                        Err(_) => continue,
                    };
                    bytes.push(b'\n');
                    if write_half.write_all(&bytes).await.is_err() {
                        return Ok(()); // client gone
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(lagged = n, "cron subscriber lagged，丢失 {n} 事件；client 应靠 poll_events 补");
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            line_read = reader.read_line(&mut sink) => match line_read {
                Ok(0) => return Ok(()), // EOF — client disconnected
                Ok(_) => sink.clear(), // 忽略后续行（subscribe 是单向 push，client 不该发请求）
                Err(_) => return Ok(()),
            }
        }
    }
}

/// 处理一行请求，返回响应 Value。
#[cfg(test)]
async fn handle_request_line_for_state(
    line: &str,
    cmd_tx: &mpsc::Sender<CronCommand>,
    events: &EventQueue,
    cron_dir: &Path,
    state_identity: &str,
    ledger_epoch: &str,
) -> serde_json::Value {
    handle_request_line_for_state_with_ledger(
        line,
        cmd_tx,
        events,
        cron_dir,
        None,
        state_identity,
        ledger_epoch,
    )
    .await
}

async fn handle_request_line_for_state_with_ledger(
    line: &str,
    cmd_tx: &mpsc::Sender<CronCommand>,
    events: &EventQueue,
    cron_dir: &Path,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
    ledger_epoch: &str,
) -> serde_json::Value {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        // 空行不算错；返回空响应（id=null）
        return serde_json::json!({"id": null, "error": {"code": -32600, "message": "empty request"}});
    }

    let req: serde_json::Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(e) => {
            return serde_json::json!({
                "id": null,
                "error": {"code": -32700, "message": format!("parse error: {e}")}
            });
        }
    };

    let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let Some(method) = req.get("method").and_then(|v| v.as_str()) else {
        return serde_json::json!({
            "id": id,
            "error": {"code": -32600, "message": "missing 'method' field"}
        });
    };

    let Some(sub) = method.strip_prefix("cron.") else {
        return serde_json::json!({
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("method must be 'cron.<sub>', got {method:?}")
            }
        });
    };

    let params = req.get("params");
    let requires_identity = is_mutating_subcommand(sub);
    let expected_identity = params
        .and_then(|value| value.get("expectedStateIdentity"))
        .and_then(serde_json::Value::as_str);
    if let Some(result) = identity_mismatch(expected_identity, state_identity, requires_identity) {
        return serde_json::json!({"id": id, "result": result});
    }

    let expected_ledger_epoch = params
        .and_then(|value| value.get("expectedLedgerEpoch"))
        .and_then(serde_json::Value::as_str);
    if is_delivery_subcommand(sub)
        && let Some(result) = ledger_epoch_mismatch(expected_ledger_epoch, ledger_epoch)
    {
        return serde_json::json!({"id": id, "result": result});
    }

    let result = handle_cron_method(
        sub,
        params,
        cmd_tx,
        events,
        cron_dir,
        ledger_writer,
        state_identity,
        ledger_epoch,
    )
    .await;
    let result = with_identity_metadata(result, expected_identity, state_identity);
    let result = if is_delivery_subcommand(sub) {
        with_ledger_metadata(result, expected_ledger_epoch, ledger_epoch)
    } else {
        result
    };
    serde_json::json!({"id": id, "result": result})
}

fn is_mutating_subcommand(sub: &str) -> bool {
    matches!(
        sub,
        "add"
            | "update"
            | "remove"
            | "run"
            // `claim` mutates occurrence status + inserts an attempt row, so it
            // requires `expectedStateIdentity` like the other mutations. It is
            // NOT an `is_delivery_subcommand`: it is occurrence-keyed, not tied
            // to the outbox-journal `ledgerEpoch` namespace, so no
            // `expectedLedgerEpoch` fence and no `with_ledger_metadata`.
            | "claim"
            // W3-3 result/lifecycle writebacks. Each mutates occurrence /
            // attempt / result rows, so it requires `expectedStateIdentity`.
            // Like `claim`, all four are occurrence-keyed (NOT
            // `is_delivery_subcommand`): no `expectedLedgerEpoch` fence.
            // `occurrence_history` is deliberately absent — it is a read.
            | "accept"
            | "report_failure"
            | "retry"
            | "cancel"
            | "delivery_begin"
            | "delivery_accept"
            | "delivery_abandon"
    )
}

fn is_delivery_subcommand(sub: &str) -> bool {
    matches!(
        sub,
        "delivery_begin" | "delivery_accept" | "delivery_abandon"
    )
}

fn identity_mismatch(
    expected: Option<&str>,
    actual: &str,
    required: bool,
) -> Option<serde_json::Value> {
    let Some(expected) = expected.filter(|identity| !identity.is_empty()) else {
        return required.then(|| {
            serde_json::json!({
                "status": "error",
                "code": "state_identity_required",
                "error": "expectedStateIdentity is required for cron mutations",
                "stateIdentity": actual,
                "expectedStateIdentity": serde_json::Value::Null,
                "stateIdentityMatches": false,
            })
        });
    };
    if expected == actual {
        return None;
    }
    Some(serde_json::json!({
        "status": "error",
        "code": "state_identity_mismatch",
        "error": "cron state identity mismatch; request was not executed",
        "stateIdentity": actual,
        "expectedStateIdentity": expected,
        "stateIdentityMatches": false,
    }))
}

fn ledger_epoch_mismatch(expected: Option<&str>, actual: &str) -> Option<serde_json::Value> {
    let Some(expected) = expected.filter(|epoch| !epoch.is_empty()) else {
        return Some(serde_json::json!({
            "status": "error",
            "code": "ledger_epoch_required",
            "error": "expectedLedgerEpoch is required for cron delivery mutations",
            "ledgerEpoch": actual,
            "expectedLedgerEpoch": serde_json::Value::Null,
            "ledgerEpochMatches": false,
        }));
    };
    if expected == actual {
        return None;
    }
    Some(serde_json::json!({
        "status": "error",
        "code": "ledger_epoch_mismatch",
        "error": "cron delivery ledger epoch mismatch; request was not executed",
        "ledgerEpoch": actual,
        "expectedLedgerEpoch": expected,
        "ledgerEpochMatches": false,
    }))
}

fn with_identity_metadata(
    mut result: serde_json::Value,
    expected: Option<&str>,
    actual: &str,
) -> serde_json::Value {
    if let Some(object) = result.as_object_mut() {
        object.insert(
            "stateIdentity".to_string(),
            serde_json::Value::String(actual.to_string()),
        );
        object.insert(
            "expectedStateIdentity".to_string(),
            expected
                .map(|value| serde_json::Value::String(value.to_string()))
                .unwrap_or(serde_json::Value::Null),
        );
        object.insert(
            "stateIdentityMatches".to_string(),
            expected
                .map(|value| serde_json::Value::Bool(value == actual))
                .unwrap_or(serde_json::Value::Null),
        );
    }
    result
}

fn with_ledger_metadata(
    mut result: serde_json::Value,
    expected: Option<&str>,
    actual: &str,
) -> serde_json::Value {
    if let Some(object) = result.as_object_mut() {
        object.insert(
            "ledgerEpoch".to_string(),
            serde_json::Value::String(actual.to_string()),
        );
        object.insert(
            "expectedLedgerEpoch".to_string(),
            expected
                .map(|value| serde_json::Value::String(value.to_string()))
                .unwrap_or(serde_json::Value::Null),
        );
        object.insert(
            "ledgerEpochMatches".to_string(),
            expected
                .map(|value| serde_json::Value::Bool(value == actual))
                .unwrap_or(serde_json::Value::Null),
        );
    }
    result
}

#[cfg(test)]
const TEST_LEDGER_EPOCH: &str = "00000000-0000-4000-8000-000000000001";

#[cfg(test)]
async fn handle_request_line(
    line: &str,
    cmd_tx: &mpsc::Sender<CronCommand>,
    events: &EventQueue,
    cron_dir: &Path,
) -> serde_json::Value {
    const TEST_IDENTITY: &str = "cron-state-v1:test";
    let mut request = match serde_json::from_str::<serde_json::Value>(line.trim()) {
        Ok(value) => value,
        Err(_) => {
            return handle_request_line_for_state(
                line,
                cmd_tx,
                events,
                cron_dir,
                TEST_IDENTITY,
                TEST_LEDGER_EPOCH,
            )
            .await;
        }
    };
    let mutating = request
        .get("method")
        .and_then(serde_json::Value::as_str)
        .and_then(|method| method.strip_prefix("cron."))
        .is_some_and(is_mutating_subcommand);
    let delivery_mutation = request
        .get("method")
        .and_then(serde_json::Value::as_str)
        .and_then(|method| method.strip_prefix("cron."))
        .is_some_and(is_delivery_subcommand);
    if mutating {
        if !request
            .get("params")
            .is_some_and(serde_json::Value::is_object)
        {
            request["params"] = serde_json::json!({});
        }
        if let Some(params) = request
            .get_mut("params")
            .and_then(serde_json::Value::as_object_mut)
        {
            params
                .entry("expectedStateIdentity")
                .or_insert_with(|| serde_json::Value::String(TEST_IDENTITY.to_string()));
            if delivery_mutation {
                params
                    .entry("expectedLedgerEpoch")
                    .or_insert_with(|| serde_json::Value::String(TEST_LEDGER_EPOCH.to_string()));
            }
        }
    }
    handle_request_line_for_state(
        &request.to_string(),
        cmd_tx,
        events,
        cron_dir,
        TEST_IDENTITY,
        TEST_LEDGER_EPOCH,
    )
    .await
}

/// 与 `acosmi-supervisor::ipc::handle_cron_method`（ipc.rs:1204-1374）1:1
/// 等价的子命令集，确保 WT3 切换 socket 时业务语义零差异。
///
/// 路由层只负责 sub 解析和分发；具体业务对 daemon 的命令投递走各 sub 的
/// 独立 helper。原 supervisor 把所有分支集中在单函数里（170 行），这里拆分
/// 是为了通过 clippy::too_many_lines 而不破坏对外契约。
#[allow(clippy::too_many_arguments)]
async fn handle_cron_method(
    sub: &str,
    params: Option<&serde_json::Value>,
    cmd_tx: &mpsc::Sender<CronCommand>,
    events: &EventQueue,
    cron_dir: &Path,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
    ledger_epoch: &str,
) -> serde_json::Value {
    match sub {
        "poll_events" => sub_poll_events(events).await,
        "list" => sub_list(params, cmd_tx).await,
        "add" => sub_add(params, cmd_tx).await,
        "remove" => sub_remove(params, cmd_tx).await,
        "update" => sub_update(params, cmd_tx).await,
        "status" => sub_status(cmd_tx).await,
        "run" => sub_run(params, cmd_tx).await,
        "runs" => sub_runs(params, cmd_tx).await,
        "outbox_read" => sub_outbox_read(params, cron_dir, ledger_writer, ledger_epoch).await,
        "claim" => sub_claim(params, ledger_writer, state_identity).await,
        "accept" => sub_accept(params, ledger_writer, state_identity).await,
        "report_failure" => sub_report_failure(params, ledger_writer, state_identity).await,
        "retry" => sub_retry(params, ledger_writer, state_identity).await,
        "cancel" => sub_cancel(params, ledger_writer, state_identity).await,
        "occurrence_history" => sub_occurrence_history(params, ledger_writer).await,
        "delivery_begin" => {
            sub_delivery_begin(params, ledger_writer, state_identity, ledger_epoch).await
        }
        "delivery_accept" => {
            sub_delivery_accept(params, ledger_writer, state_identity, ledger_epoch).await
        }
        "delivery_abandon" => {
            sub_delivery_abandon(params, ledger_writer, state_identity, ledger_epoch).await
        }
        other => {
            serde_json::json!({
                "status": "error",
                "error": format!("未知 cron 子命令: {other}")
            })
        }
    }
}

async fn sub_poll_events(events: &EventQueue) -> serde_json::Value {
    let drained: Vec<serde_json::Value> = {
        let mut queue = events.lock().await;
        queue.drain(..).collect()
    };
    serde_json::json!({"status": "ok", "events": drained})
}

async fn sub_list(
    params: Option<&serde_json::Value>,
    cmd_tx: &mpsc::Sender<CronCommand>,
) -> serde_json::Value {
    let include_disabled = params
        .and_then(|p| p.get("includeDisabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(e) = cmd_tx
        .send(CronCommand::List {
            include_disabled,
            reply: reply_tx,
        })
        .await
    {
        return serde_json::json!({"status": "error", "error": format!("send list cmd: {e}")});
    }
    match tokio::time::timeout(CMD_TIMEOUT, reply_rx).await {
        Ok(Ok(jobs)) => serde_json::json!({"status": "ok", "jobs": jobs}),
        Ok(Err(e)) => {
            serde_json::json!({"status": "error", "error": format!("reply dropped: {e}")})
        }
        Err(_) => {
            serde_json::json!({"status": "error", "error": "scheduler unresponsive (timeout)"})
        }
    }
}

fn cron_payload_has_non_blank_prompt(payload: &acosmi_scheduler::CronPayload) -> bool {
    payload
        .message
        .as_deref()
        .is_some_and(|message| !message.trim().is_empty())
        || payload
            .text
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
}

async fn sub_add(
    params: Option<&serde_json::Value>,
    cmd_tx: &mpsc::Sender<CronCommand>,
) -> serde_json::Value {
    let Some(input) = params
        .and_then(|p| serde_json::from_value::<acosmi_scheduler::CronJobCreate>(p.clone()).ok())
    else {
        return serde_json::json!({
            "status": "error",
            "error": "无效的 CronJobCreate 参数"
        });
    };
    if input.name.trim().is_empty() {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_cron_name",
            "error": "cron name must not be empty or whitespace",
        });
    }
    if !cron_payload_has_non_blank_prompt(&input.payload) {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_cron_payload",
            "error": "cron payload requires a non-empty message or text",
        });
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(e) = cmd_tx
        .send(CronCommand::Add {
            input,
            reply: reply_tx,
        })
        .await
    {
        return serde_json::json!({"status": "error", "error": format!("send add cmd: {e}")});
    }
    match tokio::time::timeout(CMD_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(result))) => serde_json::json!({"status": "ok", "id": result.id}),
        Ok(Ok(Err(e))) => serde_json::json!({"status": "error", "error": format!("{e}")}),
        Ok(Err(e)) => {
            serde_json::json!({"status": "error", "error": format!("reply dropped: {e}")})
        }
        Err(_) => {
            serde_json::json!({"status": "error", "error": "scheduler unresponsive (timeout)"})
        }
    }
}

async fn sub_remove(
    params: Option<&serde_json::Value>,
    cmd_tx: &mpsc::Sender<CronCommand>,
) -> serde_json::Value {
    let Some(id) = params
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return serde_json::json!({"status": "error", "error": "缺少 id 参数"});
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(e) = cmd_tx
        .send(CronCommand::Remove {
            id,
            reply: reply_tx,
        })
        .await
    {
        return serde_json::json!({"status": "error", "error": format!("send remove cmd: {e}")});
    }
    match tokio::time::timeout(CMD_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(()))) => serde_json::json!({"status": "ok"}),
        Ok(Ok(Err(e))) => serde_json::json!({"status": "error", "error": format!("{e}")}),
        Ok(Err(e)) => {
            serde_json::json!({"status": "error", "error": format!("reply dropped: {e}")})
        }
        Err(_) => {
            serde_json::json!({"status": "error", "error": "scheduler unresponsive (timeout)"})
        }
    }
}

async fn sub_update(
    params: Option<&serde_json::Value>,
    cmd_tx: &mpsc::Sender<CronCommand>,
) -> serde_json::Value {
    let Some(id) = params
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return serde_json::json!({"status": "error", "error": "缺少 id 参数"});
    };
    let Some(patch) = params
        .and_then(|p| p.get("patch"))
        .and_then(|v| serde_json::from_value::<acosmi_scheduler::CronJobPatch>(v.clone()).ok())
    else {
        return serde_json::json!({
            "status": "error",
            "error": "无效的 CronJobPatch 参数"
        });
    };
    if patch
        .name
        .as_deref()
        .is_some_and(|name| name.trim().is_empty())
    {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_cron_name",
            "error": "cron name must not be empty or whitespace",
        });
    }
    if patch
        .payload
        .as_ref()
        .is_some_and(|payload| !cron_payload_has_non_blank_prompt(payload))
    {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_cron_payload",
            "error": "cron payload requires a non-empty message or text",
        });
    }
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(e) = cmd_tx
        .send(CronCommand::Update {
            id,
            patch,
            reply: reply_tx,
        })
        .await
    {
        return serde_json::json!({"status": "error", "error": format!("send update cmd: {e}")});
    }
    match tokio::time::timeout(CMD_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(()))) => serde_json::json!({"status": "ok"}),
        Ok(Ok(Err(e))) => serde_json::json!({"status": "error", "error": format!("{e}")}),
        Ok(Err(e)) => {
            serde_json::json!({"status": "error", "error": format!("reply dropped: {e}")})
        }
        Err(_) => {
            serde_json::json!({"status": "error", "error": "scheduler unresponsive (timeout)"})
        }
    }
}

async fn sub_status(cmd_tx: &mpsc::Sender<CronCommand>) -> serde_json::Value {
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(e) = cmd_tx.send(CronCommand::Status { reply: reply_tx }).await {
        return serde_json::json!({"status": "error", "error": format!("send status cmd: {e}")});
    }
    match tokio::time::timeout(CMD_TIMEOUT, reply_rx).await {
        Ok(Ok(s)) => serde_json::json!({
            "status": "ok",
            "version": s.version,
            "job_count": s.job_count,
            "enabled_count": s.enabled_count,
        }),
        Ok(Err(e)) => {
            serde_json::json!({"status": "error", "error": format!("reply dropped: {e}")})
        }
        Err(_) => {
            serde_json::json!({"status": "error", "error": "scheduler unresponsive (timeout)"})
        }
    }
}

async fn sub_run(
    params: Option<&serde_json::Value>,
    cmd_tx: &mpsc::Sender<CronCommand>,
) -> serde_json::Value {
    let Some(id) = params
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return serde_json::json!({"status": "error", "error": "缺少 id 参数"});
    };
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(e) = cmd_tx
        .send(CronCommand::Run {
            id,
            reply: reply_tx,
        })
        .await
    {
        return serde_json::json!({"status": "error", "error": format!("send run cmd: {e}")});
    }
    match tokio::time::timeout(CMD_TIMEOUT, reply_rx).await {
        Ok(Ok(Ok(()))) => serde_json::json!({"status": "ok", "triggered": true}),
        Ok(Ok(Err(e))) => serde_json::json!({"status": "error", "error": format!("{e}")}),
        Ok(Err(e)) => {
            serde_json::json!({"status": "error", "error": format!("reply dropped: {e}")})
        }
        Err(_) => {
            serde_json::json!({"status": "error", "error": "scheduler unresponsive (timeout)"})
        }
    }
}

/// `cron.runs` 默认返回末尾 50 条 fire_log（与 TS 上游 ListJobRuns 默认一致）。
const RUNS_DEFAULT_LIMIT: usize = 50;

async fn sub_runs(
    params: Option<&serde_json::Value>,
    cmd_tx: &mpsc::Sender<CronCommand>,
) -> serde_json::Value {
    let Some(id) = params
        .and_then(|p| p.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
    else {
        return serde_json::json!({"status": "error", "error": "缺少 id 参数"});
    };
    let limit = params
        .and_then(|p| p.get("limit"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(RUNS_DEFAULT_LIMIT);
    let (reply_tx, reply_rx) = oneshot::channel();
    if let Err(e) = cmd_tx
        .send(CronCommand::Runs {
            id,
            limit,
            reply: reply_tx,
        })
        .await
    {
        return serde_json::json!({"status": "error", "error": format!("send runs cmd: {e}")});
    }
    match tokio::time::timeout(CMD_TIMEOUT, reply_rx).await {
        Ok(Ok(entries)) => serde_json::json!({"status": "ok", "entries": entries}),
        Ok(Err(e)) => {
            serde_json::json!({"status": "error", "error": format!("reply dropped: {e}")})
        }
        Err(_) => {
            serde_json::json!({"status": "error", "error": "scheduler unresponsive (timeout)"})
        }
    }
}

/// `cron.outbox_read` 默认返回的最大事件数（一次恢复读取的上限）。
const OUTBOX_READ_DEFAULT_LIMIT: usize = 500;

/// `cron.outbox_read`（W-CRON-AUTOMATION-E2E P7）——cursor 式持久 outbox 读取。
///
/// 参数 `{ afterEventId?: u64, limit?: u64 }`，返回
/// `{ status:"ok", events:[OutboxEvent...], lastEventId: u64 }`：
/// - `events` = outbox 中 `event_id > afterEventId` 的事件，升序，至多 limit 条
/// - `lastEventId` = 返回事件的最大 id；为空时回 `afterEventId`（cursor 不前进）
///
/// 幂等：不 drain、不写盘，重复调用同 cursor 得同结果。consumer（REPL）据此
/// 在 daemon 重启 / 离线后恢复未消费的 fire / 补投 / skipped 事件。
async fn sub_outbox_read(
    params: Option<&serde_json::Value>,
    cron_dir: &Path,
    ledger_writer: Option<&CronLedgerWriter>,
    ledger_epoch: &str,
) -> serde_json::Value {
    let after_event_id = params
        .and_then(|p| p.get("afterEventId"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let limit = params
        .and_then(|p| p.get("limit"))
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .filter(|&n| n > 0)
        .unwrap_or(OUTBOX_READ_DEFAULT_LIMIT);

    let page_result = if let Some(writer) = ledger_writer {
        // Production reads share the daemon's single actor with append and
        // rotation, so the strict retained scan cannot observe rename gaps.
        writer.outbox_read(after_event_id, limit).await
    } else {
        // Unit-only fallback for request-parser tests that do not spawn the
        // actor. It is still strict and never mutates delivery state.
        acosmi_scheduler::outbox::read_after_with_bounds(cron_dir, after_event_id, limit).await
    };
    let page = match page_result {
        Ok(page) => page,
        Err(error) => {
            return serde_json::json!({
                "status": "error",
                "code": "outbox_state_unavailable",
                "error": format!("cron outbox retained scan failed: {error}"),
                "ledgerEpoch": ledger_epoch,
            });
        }
    };
    let last_event_id = page
        .events
        .iter()
        .map(|e| e.event_id)
        .max()
        .unwrap_or(after_event_id);
    serde_json::json!({
        "status": "ok",
        "ledgerEpoch": ledger_epoch,
        "oldestEventId": page.oldest_event_id,
        "newestEventId": page.newest_event_id,
        "events": page.events,
        "lastEventId": last_event_id,
    })
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeliveryBeginParams {
    expected_state_identity: String,
    expected_ledger_epoch: String,
    event: acosmi_scheduler::OutboxEvent,
    consumer: DeliveryConsumer,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeliveryFenceParams {
    expected_state_identity: String,
    expected_ledger_epoch: String,
    event_id: u64,
    generation: String,
    lease_token: String,
}

async fn sub_delivery_begin(
    params: Option<&serde_json::Value>,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
    ledger_epoch: &str,
) -> serde_json::Value {
    let parsed = params
        .cloned()
        .and_then(|value| serde_json::from_value::<DeliveryBeginParams>(value).ok());
    let Some(parsed) = parsed else {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_delivery_request",
            "error": "invalid cron.delivery_begin parameters",
        });
    };
    let Some(writer) = ledger_writer else {
        return delivery_error_result(&DeliveryError::ActorUnavailable);
    };
    match writer
        .delivery_begin(
            parsed.expected_state_identity,
            parsed.expected_ledger_epoch,
            parsed.event,
            parsed.consumer,
        )
        .await
    {
        Ok(DeliveryBeginOutcome::Acquired(lease)) => serde_json::json!({
            "status": "ok",
            "outcome": "acquired",
            "eventId": lease.event_id,
            "generation": lease.generation.to_string(),
            "leaseToken": lease.lease_token,
            "expiresAtMs": lease.expires_at_ms,
        }),
        Ok(DeliveryBeginOutcome::Busy {
            event_id,
            retry_at_ms,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "busy",
            "eventId": event_id,
            "retryAtMs": retry_at_ms,
        }),
        Ok(DeliveryBeginOutcome::Accepted {
            event_id,
            accepted_at_ms,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "accepted",
            "eventId": event_id,
            "acceptedAtMs": accepted_at_ms,
        }),
        Err(error) => {
            tracing::warn!(
                code = error.code(),
                event_id = parsed_event_id_hint(params),
                state_identity,
                ledger_epoch,
                "cron delivery begin failed"
            );
            delivery_error_result(&error)
        }
    }
}

async fn sub_delivery_accept(
    params: Option<&serde_json::Value>,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
    ledger_epoch: &str,
) -> serde_json::Value {
    let parsed = params
        .cloned()
        .and_then(|value| serde_json::from_value::<DeliveryFenceParams>(value).ok());
    let Some(parsed) = parsed else {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_delivery_request",
            "error": "invalid cron.delivery_accept parameters",
        });
    };
    let Some(writer) = ledger_writer else {
        return delivery_error_result(&DeliveryError::ActorUnavailable);
    };
    let generation = match parsed.generation.parse::<u64>() {
        Ok(generation) => generation,
        Err(error) => {
            return delivery_error_result(&DeliveryError::InvalidRequest(format!(
                "generation must be a decimal u64 string: {error}"
            )));
        }
    };
    match writer
        .delivery_accept(
            parsed.expected_state_identity,
            parsed.expected_ledger_epoch,
            parsed.event_id,
            generation,
            parsed.lease_token,
        )
        .await
    {
        Ok(DeliveryAcceptOutcome::Accepted {
            event_id,
            generation,
            accepted_at_ms,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "accepted",
            "eventId": event_id,
            "generation": generation.to_string(),
            "acceptedAtMs": accepted_at_ms,
        }),
        Ok(DeliveryAcceptOutcome::Fenced {
            event_id,
            generation,
            reason,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "fenced",
            "eventId": event_id,
            "generation": generation.to_string(),
            "reason": reason,
        }),
        Err(error) => {
            tracing::warn!(
                code = error.code(),
                event_id = parsed_event_id_hint(params),
                state_identity,
                ledger_epoch,
                "cron delivery accept failed"
            );
            delivery_error_result(&error)
        }
    }
}

async fn sub_delivery_abandon(
    params: Option<&serde_json::Value>,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
    ledger_epoch: &str,
) -> serde_json::Value {
    let parsed = params
        .cloned()
        .and_then(|value| serde_json::from_value::<DeliveryFenceParams>(value).ok());
    let Some(parsed) = parsed else {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_delivery_request",
            "error": "invalid cron.delivery_abandon parameters",
        });
    };
    let Some(writer) = ledger_writer else {
        return delivery_error_result(&DeliveryError::ActorUnavailable);
    };
    let generation = match parsed.generation.parse::<u64>() {
        Ok(generation) => generation,
        Err(error) => {
            return delivery_error_result(&DeliveryError::InvalidRequest(format!(
                "generation must be a decimal u64 string: {error}"
            )));
        }
    };
    match writer
        .delivery_abandon(
            parsed.expected_state_identity,
            parsed.expected_ledger_epoch,
            parsed.event_id,
            generation,
            parsed.lease_token,
        )
        .await
    {
        Ok(DeliveryAbandonOutcome::Abandoned {
            event_id,
            generation,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "abandoned",
            "eventId": event_id,
            "generation": generation.to_string(),
        }),
        Ok(DeliveryAbandonOutcome::Fenced {
            event_id,
            generation,
            reason,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "fenced",
            "eventId": event_id,
            "generation": generation.to_string(),
            "reason": reason,
        }),
        Ok(DeliveryAbandonOutcome::Absent {
            event_id,
            generation,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "absent",
            "eventId": event_id,
            "generation": generation.to_string(),
        }),
        Err(error) => {
            tracing::warn!(
                code = error.code(),
                event_id = parsed_event_id_hint(params),
                state_identity,
                ledger_epoch,
                "cron delivery abandon failed"
            );
            delivery_error_result(&error)
        }
    }
}

/// Params for `cron.claim`.
///
/// `expectedStateIdentity` is validated by the mutation gate *before* dispatch;
/// it is carried here only so `deny_unknown_fields` accepts the full params
/// object (claim is `is_mutating_subcommand`). The claim reads none of it — the
/// actor mints the clock and lease token.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClaimParams {
    #[allow(dead_code)]
    expected_state_identity: String,
    consumer_kind: String,
    consumer_id: String,
    target: String,
}

/// Map raw `cron.claim` params into a typed [`ConsumerClaim`], rejecting an
/// unknown `consumerKind` or `target` with a clear, field-named message.
fn parse_consumer_claim(parsed: &ClaimParams) -> std::result::Result<ConsumerClaim, String> {
    let kind = match parsed.consumer_kind.as_str() {
        "tui" => ConsumerKind::Tui,
        other => {
            return Err(format!("unknown consumerKind {other:?} (expected \"tui\")"));
        }
    };
    let target = match parsed.target.as_str() {
        "main" => EnvelopeTarget::Main,
        "isolated" => EnvelopeTarget::Isolated,
        "continuation" => EnvelopeTarget::Continuation,
        other => {
            return Err(format!(
                "unknown target {other:?} (expected \"main\", \"isolated\", or \"continuation\")"
            ));
        }
    };
    Ok(ConsumerClaim {
        kind,
        id: parsed.consumer_id.clone(),
        target,
    })
}

/// `cron.claim` — occurrence-keyed durable claim/lease over the SQLite ledger.
///
/// Parses `{ consumerKind, consumerId, target }` (+ the gate-required
/// `expectedStateIdentity`) into a [`ConsumerClaim`], then asks the sole ledger
/// writer actor to claim the next runnable occurrence for that target. The actor
/// mints the clock + lease token — this method supplies neither.
///
/// Wire-number discipline (matches how `delivery_*` sends `generation`): the i64
/// fields `occurrenceId` / `attemptId` / `fence` / `scheduledAtMs` /
/// `leaseExpiresAtMs` exceed JavaScript's safe-integer range, so they cross the
/// wire as DECIMAL STRINGS. `envelope` is serde-serializable → embedded as an
/// object. `NoWork` → `{ "noWork": true }`. Actor absent → the same fallback
/// shape the `delivery_*` methods use.
async fn sub_claim(
    params: Option<&serde_json::Value>,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
) -> serde_json::Value {
    let Some(parsed) = params
        .cloned()
        .and_then(|value| serde_json::from_value::<ClaimParams>(value).ok())
    else {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_claim_request",
            "error": "invalid cron.claim parameters",
        });
    };
    let consumer = match parse_consumer_claim(&parsed) {
        Ok(consumer) => consumer,
        Err(message) => {
            return serde_json::json!({
                "status": "error",
                "code": "invalid_claim_request",
                "error": message,
            });
        }
    };
    let Some(writer) = ledger_writer else {
        // Actor unavailable: reuse the exact fallback shape the delivery methods
        // emit for a missing writer.
        return delivery_error_result(&DeliveryError::ActorUnavailable);
    };
    match writer.claim_next(consumer).await {
        Ok(ClaimOutcome::Claimed(claimed)) => serde_json::json!({
            "status": "ok",
            "claimed": true,
            "occurrenceId": claimed.occurrence_id.to_string(),
            "attemptId": claimed.attempt_id.to_string(),
            "jobId": claimed.job_id,
            "scheduledAtMs": claimed.scheduled_at_ms.to_string(),
            "fence": claimed.fence.to_string(),
            "leaseToken": claimed.lease_token,
            "leaseExpiresAtMs": claimed.lease_expires_at_ms.to_string(),
            "envelope": claimed.envelope,
        }),
        Ok(ClaimOutcome::NoWork) => serde_json::json!({
            "status": "ok",
            "noWork": true,
        }),
        Err(error) => {
            tracing::warn!(state_identity, error = %error, "cron claim failed");
            serde_json::json!({
                "status": "error",
                "code": "claim_failed",
                "error": error,
            })
        }
    }
}

/// Params shared by `cron.accept`. The gate-required `expectedStateIdentity` is
/// validated BEFORE dispatch and carried here only so `deny_unknown_fields`
/// accepts the full object (accept is `is_mutating_subcommand`). The i64
/// identifiers `occurrenceId` / `attemptId` / `fence` (+ optional `durationMs`)
/// cross the wire as DECIMAL STRINGS — they exceed JavaScript's safe-integer
/// range — and are parsed in the sub_ fn, mirroring `delivery_*`'s `generation`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AcceptParams {
    #[allow(dead_code)]
    expected_state_identity: String,
    occurrence_id: String,
    attempt_id: String,
    fence: String,
    #[serde(default)]
    duration_ms: Option<String>,
}

/// `cron.report_failure` params: [`AcceptParams`] plus the failure
/// classification (`errorKind` = `failure` | `error`) and an optional
/// human-readable `errorMessage`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReportFailureParams {
    #[allow(dead_code)]
    expected_state_identity: String,
    occurrence_id: String,
    attempt_id: String,
    fence: String,
    #[serde(default)]
    duration_ms: Option<String>,
    error_kind: String,
    #[serde(default)]
    error_message: Option<String>,
}

/// `cron.retry` / `cron.cancel` params: the occurrence id (decimal string) plus
/// the gate-required `expectedStateIdentity`.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OccurrenceLifecycleParams {
    #[allow(dead_code)]
    expected_state_identity: String,
    occurrence_id: String,
}

/// `cron.occurrence_history` params. A READ: no `expectedStateIdentity` (not
/// identity-gated), just the job id.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OccurrenceHistoryParams {
    job_id: String,
}

/// Parse a wire i64 that crosses as a decimal string (the W3-3 echo-back
/// discipline — see [`AcceptParams`]). Field-named error on failure.
fn parse_wire_i64(field: &str, raw: &str) -> std::result::Result<i64, String> {
    raw.parse::<i64>()
        .map_err(|error| format!("{field} must be a decimal i64 string: {error}"))
}

/// Parse the shared `accept` / `report_failure` echo-back identifiers.
fn parse_writeback_ids(
    occurrence_id: &str,
    attempt_id: &str,
    fence: &str,
    duration_ms: Option<&str>,
) -> std::result::Result<(i64, i64, i64, Option<i64>), String> {
    let occurrence_id = parse_wire_i64("occurrenceId", occurrence_id)?;
    let attempt_id = parse_wire_i64("attemptId", attempt_id)?;
    let fence = parse_wire_i64("fence", fence)?;
    let duration_ms = duration_ms
        .map(|raw| parse_wire_i64("durationMs", raw))
        .transpose()?;
    Ok((occurrence_id, attempt_id, fence, duration_ms))
}

/// Render a [`WritebackOutcome`] as the shared `cron.accept` /
/// `cron.report_failure` response — every i64 a decimal string, `actualFence`
/// string-or-null.
fn writeback_outcome_json(outcome: WritebackOutcome) -> serde_json::Value {
    match outcome {
        WritebackOutcome::Recorded {
            occurrence_id,
            fence,
        } => serde_json::json!({
            "status": "ok",
            "outcome": "recorded",
            "occurrenceId": occurrence_id.to_string(),
            "fence": fence.to_string(),
        }),
        WritebackOutcome::Fenced {
            occurrence_id,
            expected_fence,
            actual_fence,
        } => serde_json::json!({
            "status": "ok",
            "outcome": "fenced",
            "occurrenceId": occurrence_id.to_string(),
            "expectedFence": expected_fence.to_string(),
            "actualFence": actual_fence.map(|fence| fence.to_string()),
        }),
    }
}

/// `cron.accept` — the W3-1 success writeback over the SQLite occurrence ledger.
///
/// Parses the echo-back identifiers (`occurrenceId` / `attemptId` / `fence`, +
/// optional `durationMs`) as decimal strings, then asks the sole ledger writer
/// actor to record the run (the actor mints the clock). `Recorded` / `Fenced`
/// map to the shared [`writeback_outcome_json`] shape; an absent actor reuses
/// the delivery methods' actor-unavailable fallback.
async fn sub_accept(
    params: Option<&serde_json::Value>,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
) -> serde_json::Value {
    let Some(parsed) = params
        .cloned()
        .and_then(|value| serde_json::from_value::<AcceptParams>(value).ok())
    else {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_accept_request",
            "error": "invalid cron.accept parameters",
        });
    };
    let (occurrence_id, attempt_id, fence, duration_ms) = match parse_writeback_ids(
        &parsed.occurrence_id,
        &parsed.attempt_id,
        &parsed.fence,
        parsed.duration_ms.as_deref(),
    ) {
        Ok(values) => values,
        Err(message) => {
            return serde_json::json!({
                "status": "error",
                "code": "invalid_accept_request",
                "error": message,
            });
        }
    };
    let Some(writer) = ledger_writer else {
        return delivery_error_result(&DeliveryError::ActorUnavailable);
    };
    match writer
        .accept_occurrence(occurrence_id, attempt_id, fence, duration_ms)
        .await
    {
        Ok(outcome) => writeback_outcome_json(outcome),
        Err(error) => {
            tracing::warn!(state_identity, error = %error, "cron accept failed");
            serde_json::json!({
                "status": "error",
                "code": "accept_failed",
                "error": error,
            })
        }
    }
}

/// `cron.report_failure` — the W3-1 failure writeback. Same shape as
/// [`sub_accept`] plus `errorKind` (`failure` | `error`) and an optional
/// `errorMessage`.
async fn sub_report_failure(
    params: Option<&serde_json::Value>,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
) -> serde_json::Value {
    let Some(parsed) = params
        .cloned()
        .and_then(|value| serde_json::from_value::<ReportFailureParams>(value).ok())
    else {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_report_failure_request",
            "error": "invalid cron.report_failure parameters",
        });
    };
    let (occurrence_id, attempt_id, fence, duration_ms) = match parse_writeback_ids(
        &parsed.occurrence_id,
        &parsed.attempt_id,
        &parsed.fence,
        parsed.duration_ms.as_deref(),
    ) {
        Ok(values) => values,
        Err(message) => {
            return serde_json::json!({
                "status": "error",
                "code": "invalid_report_failure_request",
                "error": message,
            });
        }
    };
    let kind = match parsed.error_kind.as_str() {
        "failure" => ReportedFailureKind::Failure,
        "error" => ReportedFailureKind::Error,
        other => {
            return serde_json::json!({
                "status": "error",
                "code": "invalid_report_failure_request",
                "error": format!(
                    "unknown errorKind {other:?} (expected \"failure\" or \"error\")"
                ),
            });
        }
    };
    let Some(writer) = ledger_writer else {
        return delivery_error_result(&DeliveryError::ActorUnavailable);
    };
    match writer
        .report_failure_occurrence(
            occurrence_id,
            attempt_id,
            fence,
            kind,
            parsed.error_message,
            duration_ms,
        )
        .await
    {
        Ok(outcome) => writeback_outcome_json(outcome),
        Err(error) => {
            tracing::warn!(state_identity, error = %error, "cron report_failure failed");
            serde_json::json!({
                "status": "error",
                "code": "report_failure_failed",
                "error": error,
            })
        }
    }
}

/// `cron.retry` — the W3-2 lifecycle reset of a terminal `failed` occurrence
/// back to `pending`. `Retried` carries the post-bump fence (decimal string);
/// `NotRetryable` carries the occurrence's current status under the
/// non-colliding key `status_` (the top-level `status` is the ok/error frame).
async fn sub_retry(
    params: Option<&serde_json::Value>,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
) -> serde_json::Value {
    let Some(parsed) = params
        .cloned()
        .and_then(|value| serde_json::from_value::<OccurrenceLifecycleParams>(value).ok())
    else {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_retry_request",
            "error": "invalid cron.retry parameters",
        });
    };
    let occurrence_id = match parse_wire_i64("occurrenceId", &parsed.occurrence_id) {
        Ok(value) => value,
        Err(message) => {
            return serde_json::json!({
                "status": "error",
                "code": "invalid_retry_request",
                "error": message,
            });
        }
    };
    let Some(writer) = ledger_writer else {
        return delivery_error_result(&DeliveryError::ActorUnavailable);
    };
    match writer.retry_occurrence(occurrence_id).await {
        Ok(RetryOutcome::Retried {
            occurrence_id,
            fence,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "retried",
            "occurrenceId": occurrence_id.to_string(),
            "fence": fence.to_string(),
        }),
        Ok(RetryOutcome::NotRetryable {
            occurrence_id,
            status,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "not_retryable",
            "occurrenceId": occurrence_id.to_string(),
            "status_": status,
        }),
        Err(error) => {
            tracing::warn!(state_identity, error = %error, "cron retry failed");
            serde_json::json!({
                "status": "error",
                "code": "retry_failed",
                "error": error,
            })
        }
    }
}

/// `cron.cancel` — the W3-2 lifecycle termination of a `pending` / `claimed`
/// occurrence as `skipped`. `Cancelled` / `NotCancellable` mirror
/// [`sub_retry`]'s response shape (`status_` for the occurrence's current
/// status).
async fn sub_cancel(
    params: Option<&serde_json::Value>,
    ledger_writer: Option<&CronLedgerWriter>,
    state_identity: &str,
) -> serde_json::Value {
    let Some(parsed) = params
        .cloned()
        .and_then(|value| serde_json::from_value::<OccurrenceLifecycleParams>(value).ok())
    else {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_cancel_request",
            "error": "invalid cron.cancel parameters",
        });
    };
    let occurrence_id = match parse_wire_i64("occurrenceId", &parsed.occurrence_id) {
        Ok(value) => value,
        Err(message) => {
            return serde_json::json!({
                "status": "error",
                "code": "invalid_cancel_request",
                "error": message,
            });
        }
    };
    let Some(writer) = ledger_writer else {
        return delivery_error_result(&DeliveryError::ActorUnavailable);
    };
    match writer.cancel_occurrence(occurrence_id).await {
        Ok(CancelOutcome::Cancelled {
            occurrence_id,
            fence,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "cancelled",
            "occurrenceId": occurrence_id.to_string(),
            "fence": fence.to_string(),
        }),
        Ok(CancelOutcome::NotCancellable {
            occurrence_id,
            status,
        }) => serde_json::json!({
            "status": "ok",
            "outcome": "not_cancellable",
            "occurrenceId": occurrence_id.to_string(),
            "status_": status,
        }),
        Err(error) => {
            tracing::warn!(state_identity, error = %error, "cron cancel failed");
            serde_json::json!({
                "status": "error",
                "code": "cancel_failed",
                "error": error,
            })
        }
    }
}

/// `cron.occurrence_history` — read-only audit history (occurrence ⋈ attempts ⋈
/// results) for a job. NOT identity-gated (see `is_mutating_subcommand`). Every
/// i64 crosses as a decimal string; absent join columns are `null`.
async fn sub_occurrence_history(
    params: Option<&serde_json::Value>,
    ledger_writer: Option<&CronLedgerWriter>,
) -> serde_json::Value {
    let Some(parsed) = params
        .cloned()
        .and_then(|value| serde_json::from_value::<OccurrenceHistoryParams>(value).ok())
    else {
        return serde_json::json!({
            "status": "error",
            "code": "invalid_occurrence_history_request",
            "error": "invalid cron.occurrence_history parameters",
        });
    };
    let Some(writer) = ledger_writer else {
        return delivery_error_result(&DeliveryError::ActorUnavailable);
    };
    match writer.occurrence_history(parsed.job_id).await {
        Ok(rows) => {
            let history: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|row| {
                    serde_json::json!({
                        "occurrenceId": row.occurrence_id.to_string(),
                        "scheduledAtMs": row.scheduled_at_ms.to_string(),
                        "status": row.status,
                        "fence": row.fence.to_string(),
                        "target": row.target,
                        "attemptId": row.attempt_id.map(|id| id.to_string()),
                        "outcome": row.outcome,
                        "resultStatus": row.result_status,
                        "errorMessage": row.error_message,
                        "durationMs": row.duration_ms.map(|ms| ms.to_string()),
                        "recordedAtMs": row.recorded_at_ms.map(|ms| ms.to_string()),
                    })
                })
                .collect();
            serde_json::json!({
                "status": "ok",
                "history": history,
            })
        }
        Err(error) => serde_json::json!({
            "status": "error",
            "code": "occurrence_history_failed",
            "error": error,
        }),
    }
}

fn delivery_error_result(error: &DeliveryError) -> serde_json::Value {
    serde_json::json!({
        "status": "error",
        "code": error.code(),
        "error": error.to_string(),
    })
}

fn parsed_event_id_hint(params: Option<&serde_json::Value>) -> u64 {
    params
        .and_then(|value| value.get("eventId"))
        .and_then(serde_json::Value::as_u64)
        .or_else(|| {
            params
                .and_then(|value| value.get("event"))
                .and_then(|value| value.get("eventId"))
                .and_then(serde_json::Value::as_u64)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用临时 cron_dir —— 大多数 IPC 测试不碰 outbox，但 handle_request_line
    /// 现在需要一个 dir 参数（W-CRON-AUTOMATION-E2E P7）。每个测试持有自己的
    /// TempDir，drop 时 rm -rf。
    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tmpdir")
    }

    fn delivery_event(event_id: u64) -> serde_json::Value {
        serde_json::json!({
            "eventId": event_id,
            "jobId": format!("job-{event_id}"),
            "eventType": "fired",
            "scheduledAtMs": 1_000,
            "emittedAtMs": 1_001,
            "payloadMessage": format!("run-{event_id}"),
        })
    }

    async fn seed_delivery_event(dir: &Path, event_id: u64) {
        let event: acosmi_scheduler::OutboxEvent =
            serde_json::from_value(delivery_event(event_id)).expect("delivery event shape");
        acosmi_scheduler::outbox::append_durable(dir, &event)
            .await
            .expect("seed durable outbox event");
    }

    fn valid_add_params() -> serde_json::Value {
        serde_json::json!({
            "name": "valid",
            "schedule": {"kind": "at", "at": "2030-01-01T00:00:00Z"},
            "payload": {"kind": "systemEvent", "text": "run"},
        })
    }

    #[tokio::test]
    async fn add_and_update_reject_blank_domain_fields_before_enqueue() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(8);

        let mut blank_name = valid_add_params();
        blank_name["name"] = serde_json::json!("  ");
        let result = sub_add(Some(&blank_name), &cmd_tx).await;
        assert_eq!(result["code"], "invalid_cron_name");

        for payload in [
            serde_json::json!({"kind": "systemEvent"}),
            serde_json::json!({"kind": "systemEvent", "text": " \t", "message": ""}),
        ] {
            let mut params = valid_add_params();
            params["payload"] = payload;
            let result = sub_add(Some(&params), &cmd_tx).await;
            assert_eq!(result["code"], "invalid_cron_payload");
        }

        for patch in [
            serde_json::json!({"name": "\t"}),
            serde_json::json!({
                "payload": {"kind": "agentTurn", "message": "  ", "text": null}
            }),
        ] {
            let params = serde_json::json!({"id": "job", "patch": patch});
            let result = sub_update(Some(&params), &cmd_tx).await;
            assert!(matches!(
                result["code"].as_str(),
                Some("invalid_cron_name" | "invalid_cron_payload")
            ));
        }
        assert!(
            cmd_rx.try_recv().is_err(),
            "invalid add/update must never reach scheduler mutation"
        );
    }

    #[tokio::test]
    async fn add_accepts_valid_text_fallback_and_enqueues_trim_safe_payload() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(1);
        let mut params = valid_add_params();
        params["payload"] = serde_json::json!({
            "kind": "agentTurn",
            "message": "   ",
            "text": "fallback",
        });
        let add_task = tokio::spawn(async move { sub_add(Some(&params), &cmd_tx).await });
        let command = cmd_rx.recv().await.expect("add command");
        let CronCommand::Add { input, reply } = command else {
            panic!("expected add command")
        };
        assert_eq!(input.payload.text.as_deref(), Some("fallback"));
        reply
            .send(Ok(acosmi_scheduler::CronAddResult {
                id: "accepted".to_string(),
            }))
            .expect("reply add");
        let result = add_task.await.expect("add task");
        assert_eq!(result["status"], "ok");
        assert_eq!(result["id"], "accepted");
    }

    #[tokio::test]
    async fn all_mutations_require_expected_identity_before_enqueue() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(8);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let cases = [
            (
                "cron.add",
                serde_json::json!({
                    "name": "blocked",
                    "schedule": {"kind":"cron","expr":"0 0 1 1 *"},
                    "payload": {"kind":"systemEvent","text":"blocked"}
                }),
            ),
            (
                "cron.update",
                serde_json::json!({"id":"job","patch":{"enabled":false}}),
            ),
            ("cron.remove", serde_json::json!({"id":"job"})),
            ("cron.run", serde_json::json!({"id":"job"})),
            (
                "cron.claim",
                serde_json::json!({
                    "consumerKind": "tui",
                    "consumerId": "guard",
                    "target": "main",
                }),
            ),
            (
                "cron.accept",
                serde_json::json!({
                    "occurrenceId": "1",
                    "attemptId": "1",
                    "fence": "1",
                }),
            ),
            (
                "cron.report_failure",
                serde_json::json!({
                    "occurrenceId": "1",
                    "attemptId": "1",
                    "fence": "1",
                    "errorKind": "failure",
                }),
            ),
            ("cron.retry", serde_json::json!({"occurrenceId": "1"})),
            ("cron.cancel", serde_json::json!({"occurrenceId": "1"})),
            (
                "cron.delivery_begin",
                serde_json::json!({
                    "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                    "event": delivery_event(1),
                    "consumer": {"kind":"tui","id":"guard"},
                }),
            ),
            (
                "cron.delivery_accept",
                serde_json::json!({
                    "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                    "eventId": 1,
                    "generation": "1",
                    "leaseToken": "00000000-0000-4000-8000-000000000001",
                }),
            ),
            (
                "cron.delivery_abandon",
                serde_json::json!({
                    "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                    "eventId": 1,
                    "generation": "1",
                    "leaseToken": "00000000-0000-4000-8000-000000000001",
                }),
            ),
        ];

        for (method, params) in cases {
            let request = serde_json::json!({"id":"guard","method":method,"params":params});
            let response = handle_request_line_for_state(
                &request.to_string(),
                &cmd_tx,
                &events,
                td.path(),
                "cron-state-v1:actual",
                TEST_LEDGER_EPOCH,
            )
            .await;
            assert_eq!(
                response["result"]["code"], "state_identity_required",
                "{method}: {response}"
            );
            assert!(
                cmd_rx.try_recv().is_err(),
                "{method} must not enqueue without identity"
            );
        }
    }

    #[tokio::test]
    async fn all_mutations_reject_mismatch_before_enqueue() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(8);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let cases = [
            (
                "cron.add",
                serde_json::json!({
                    "name": "blocked",
                    "schedule": {"kind":"cron","expr":"0 0 1 1 *"},
                    "payload": {"kind":"systemEvent","text":"blocked"}
                }),
            ),
            (
                "cron.update",
                serde_json::json!({"id":"job","patch":{"enabled":false}}),
            ),
            ("cron.remove", serde_json::json!({"id":"job"})),
            ("cron.run", serde_json::json!({"id":"job"})),
            (
                "cron.claim",
                serde_json::json!({
                    "consumerKind": "tui",
                    "consumerId": "guard",
                    "target": "isolated",
                }),
            ),
            (
                "cron.accept",
                serde_json::json!({
                    "occurrenceId": "2",
                    "attemptId": "2",
                    "fence": "1",
                }),
            ),
            (
                "cron.report_failure",
                serde_json::json!({
                    "occurrenceId": "2",
                    "attemptId": "2",
                    "fence": "1",
                    "errorKind": "error",
                }),
            ),
            ("cron.retry", serde_json::json!({"occurrenceId": "2"})),
            ("cron.cancel", serde_json::json!({"occurrenceId": "2"})),
            (
                "cron.delivery_begin",
                serde_json::json!({
                    "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                    "event": delivery_event(2),
                    "consumer": {"kind":"tui","id":"guard"},
                }),
            ),
            (
                "cron.delivery_accept",
                serde_json::json!({
                    "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                    "eventId": 2,
                    "generation": "1",
                    "leaseToken": "00000000-0000-4000-8000-000000000001",
                }),
            ),
            (
                "cron.delivery_abandon",
                serde_json::json!({
                    "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                    "eventId": 2,
                    "generation": "1",
                    "leaseToken": "00000000-0000-4000-8000-000000000001",
                }),
            ),
        ];

        for (method, mut params) in cases {
            params["expectedStateIdentity"] = serde_json::json!("cron-state-v1:other");
            let request = serde_json::json!({"id":"guard","method":method,"params":params});
            let response = handle_request_line_for_state(
                &request.to_string(),
                &cmd_tx,
                &events,
                td.path(),
                "cron-state-v1:actual",
                TEST_LEDGER_EPOCH,
            )
            .await;
            assert_eq!(
                response["result"]["code"], "state_identity_mismatch",
                "{method}: {response}"
            );
            assert_eq!(response["result"]["stateIdentity"], "cron-state-v1:actual");
            assert_eq!(
                response["result"]["expectedStateIdentity"],
                "cron-state-v1:other"
            );
            assert!(
                cmd_rx.try_recv().is_err(),
                "{method} mismatch must not enqueue"
            );
        }
    }

    #[tokio::test]
    async fn mismatched_read_reports_actual_identity_without_foreign_payload() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(8);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let request = serde_json::json!({
            "id": "guard-read",
            "method": "cron.list",
            "params": {"expectedStateIdentity":"cron-state-v1:other"}
        });
        let response = handle_request_line_for_state(
            &request.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            "cron-state-v1:actual",
            TEST_LEDGER_EPOCH,
        )
        .await;

        assert_eq!(response["result"]["code"], "state_identity_mismatch");
        assert_eq!(response["result"]["stateIdentity"], "cron-state-v1:actual");
        assert_eq!(response["result"]["stateIdentityMatches"], false);
        assert!(response["result"].get("jobs").is_none());
        assert!(
            cmd_rx.try_recv().is_err(),
            "mismatched read must not enqueue"
        );
    }

    #[tokio::test]
    async fn mismatched_subscription_is_rejected_in_ack_before_streaming() {
        let (server, client) = tokio::io::duplex(4096);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(8);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let (broadcaster, _) = broadcast::channel(8);
        let td = tmp_dir();
        let ledger = crate::open_test_ledger(td.path());
        let (ledger_writer, ledger_task) = crate::spawn_cron_ledger_writer(
            td.path().to_path_buf(),
            1,
            "cron-state-v1:actual".to_string(),
            TEST_LEDGER_EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();
        let shutdown = CancellationToken::new();
        let handler = tokio::spawn(handle_conn(
            server,
            cmd_tx,
            events,
            broadcaster,
            td.path().to_path_buf(),
            ledger_writer.clone(),
            "cron-state-v1:actual".to_string(),
            TEST_LEDGER_EPOCH.to_string(),
            shutdown,
        ));

        let (read_half, mut write_half) = tokio::io::split(client);
        write_half
            .write_all(
                br#"{"id":"sub","method":"cron.subscribe","params":{"expectedStateIdentity":"cron-state-v1:other"}}
"#,
            )
            .await
            .expect("write subscription");
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read ack");
        let response: serde_json::Value = serde_json::from_str(line.trim()).expect("ack JSON");

        assert_eq!(response["result"]["code"], "state_identity_mismatch");
        assert_eq!(response["result"]["stateIdentity"], "cron-state-v1:actual");
        assert_eq!(response["result"]["stateIdentityMatches"], false);
        assert!(cmd_rx.try_recv().is_err());
        handler
            .await
            .expect("handler join")
            .expect("handler result");
        drop(ledger_writer);
        ledger_task.await.expect("ledger task");
    }

    #[tokio::test]
    async fn delivery_epoch_mismatch_is_rejected_before_actor_mutation() {
        const STATE: &str = "cron-state-v1:delivery-ipc";
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        seed_delivery_event(td.path(), 51).await;
        let ledger = crate::open_test_ledger(td.path());
        let (ledger_writer, ledger_task) = crate::spawn_cron_ledger_writer(
            td.path().to_path_buf(),
            52,
            STATE.to_string(),
            TEST_LEDGER_EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();

        let missing = serde_json::json!({
            "id": "epoch-required",
            "method": "cron.delivery_begin",
            "params": {
                "expectedStateIdentity": STATE,
                "event": delivery_event(51),
                "consumer": {"kind":"tui","id":"guard"},
            }
        });
        let missing_result = handle_request_line_for_state_with_ledger(
            &missing.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(missing_result["result"]["code"], "ledger_epoch_required");

        let mismatched = serde_json::json!({
            "id": "epoch-guard",
            "method": "cron.delivery_begin",
            "params": {
                "expectedStateIdentity": STATE,
                "expectedLedgerEpoch": "00000000-0000-4000-8000-000000000099",
                "event": delivery_event(51),
                "consumer": {"kind":"tui","id":"guard"},
            }
        });
        let rejected = handle_request_line_for_state_with_ledger(
            &mismatched.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(rejected["result"]["code"], "ledger_epoch_mismatch");
        assert_eq!(rejected["result"]["ledgerEpochMatches"], false);

        let valid = serde_json::json!({
            "id": "epoch-valid",
            "method": "cron.delivery_begin",
            "params": {
                "expectedStateIdentity": STATE,
                "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                "event": delivery_event(51),
                "consumer": {"kind":"tui","id":"valid"},
            }
        });
        let acquired = handle_request_line_for_state_with_ledger(
            &valid.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(acquired["result"]["outcome"], "acquired");
        assert_eq!(acquired["result"]["generation"], "1");

        drop(ledger_writer);
        ledger_task.await.expect("ledger task");
    }

    #[tokio::test]
    async fn delivery_ipc_roundtrip_is_fenced_and_accept_is_observable() {
        const STATE: &str = "cron-state-v1:delivery-roundtrip";
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        seed_delivery_event(td.path(), 61).await;
        let ledger = crate::open_test_ledger(td.path());
        let (ledger_writer, ledger_task) = crate::spawn_cron_ledger_writer(
            td.path().to_path_buf(),
            62,
            STATE.to_string(),
            TEST_LEDGER_EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();
        let begin_request = |consumer_id: &str| {
            serde_json::json!({
                "id": consumer_id,
                "method": "cron.delivery_begin",
                "params": {
                    "expectedStateIdentity": STATE,
                    "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                    "event": delivery_event(61),
                    "consumer": {"kind":"tui","id":consumer_id},
                }
            })
        };
        let first = handle_request_line_for_state_with_ledger(
            &begin_request("first").to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(first["result"]["outcome"], "acquired");
        assert_eq!(first["result"]["generation"], "1");
        assert_eq!(first["result"]["stateIdentity"], STATE);
        assert_eq!(first["result"]["ledgerEpoch"], TEST_LEDGER_EPOCH);
        let token = first["result"]["leaseToken"].as_str().unwrap();

        let busy = handle_request_line_for_state_with_ledger(
            &begin_request("second").to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(busy["result"]["outcome"], "busy");

        let stale_accept = serde_json::json!({
            "id": "stale",
            "method": "cron.delivery_accept",
            "params": {
                "expectedStateIdentity": STATE,
                "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                "eventId": 61,
                "generation": "1",
                "leaseToken": "00000000-0000-4000-8000-000000000099",
            }
        });
        let fenced = handle_request_line_for_state_with_ledger(
            &stale_accept.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(fenced["result"]["outcome"], "fenced");

        let accept = serde_json::json!({
            "id": "accept",
            "method": "cron.delivery_accept",
            "params": {
                "expectedStateIdentity": STATE,
                "expectedLedgerEpoch": TEST_LEDGER_EPOCH,
                "eventId": 61,
                "generation": "1",
                "leaseToken": token,
            }
        });
        let accepted = handle_request_line_for_state_with_ledger(
            &accept.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(accepted["result"]["outcome"], "accepted");

        let observed = handle_request_line_for_state_with_ledger(
            &begin_request("observer").to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(observed["result"]["outcome"], "accepted");

        drop(ledger_writer);
        ledger_task.await.expect("ledger task");
    }

    /// 解析非 JSON → 协议级 parse error
    #[tokio::test]
    async fn invalid_json_returns_parse_error() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let resp = handle_request_line("not json\n", &cmd_tx, &events, td.path()).await;
        assert_eq!(resp["id"], serde_json::Value::Null);
        let code = resp["error"]["code"].as_i64().unwrap();
        assert_eq!(code, -32700);
    }

    /// 缺 method 字段 → 协议级 invalid request
    #[tokio::test]
    async fn missing_method_returns_invalid_request() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let resp = handle_request_line(r#"{"id":"1"}"#, &cmd_tx, &events, td.path()).await;
        assert_eq!(resp["error"]["code"].as_i64().unwrap(), -32600);
    }

    /// method 不带 cron. 前缀 → method not found
    #[tokio::test]
    async fn method_without_prefix_returns_method_not_found() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let resp =
            handle_request_line(r#"{"id":"1","method":"list"}"#, &cmd_tx, &events, td.path()).await;
        assert_eq!(resp["error"]["code"].as_i64().unwrap(), -32601);
    }

    /// poll_events 直接返回 events 队列（不需要 cmd_tx）
    #[tokio::test]
    async fn poll_events_drains_queue() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(vec![
            serde_json::json!({"type":"cron.fired","job":{"id":"a"}}),
            serde_json::json!({"type":"cron.fired","job":{"id":"b"}}),
        ]));
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"X","method":"cron.poll_events"}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        let evs = resp["result"]["events"].as_array().unwrap();
        assert_eq!(evs.len(), 2);
        // drain 后再次 poll 应为空
        let resp2 = handle_request_line(
            r#"{"id":"Y","method":"cron.poll_events"}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert!(resp2["result"]["events"].as_array().unwrap().is_empty());
    }

    /// 未知 cron 子命令 → 业务级 error（status:"error"），非协议错误
    #[tokio::test]
    async fn unknown_sub_returns_business_error() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.bogus"}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "error");
        assert!(resp["result"]["error"].as_str().unwrap().contains("bogus"));
    }

    /// status 命令把请求投递到 cmd channel，然后通过 oneshot 拿回
    #[tokio::test]
    async fn status_method_roundtrips_cmd_channel() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        // 模拟 daemon 响应 Status 请求
        let stub_daemon = tokio::spawn(async move {
            if let Some(CronCommand::Status { reply }) = cmd_rx.recv().await {
                let _ = reply.send(acosmi_scheduler::CronStoreStatus {
                    version: acosmi_scheduler::STORE_VERSION,
                    job_count: 7,
                    enabled_count: 3,
                });
            }
        });
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.status"}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "ok");
        assert_eq!(resp["result"]["job_count"], 7);
        assert_eq!(resp["result"]["enabled_count"], 3);
        assert_eq!(resp["result"]["stateIdentity"], "cron-state-v1:test");
        assert!(resp["result"]["stateIdentityMatches"].is_null());
        let _ = stub_daemon.await;
    }

    /// cmd_rx drop（daemon 退出）→ send 失败 → 业务级 error，不 panic
    #[tokio::test]
    async fn cmd_send_failure_returns_error() {
        let (cmd_tx, cmd_rx) = mpsc::channel::<CronCommand>(1);
        drop(cmd_rx); // daemon "退出"
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.status"}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "error");
        let msg = resp["result"]["error"].as_str().unwrap();
        assert!(msg.contains("send status cmd"), "{msg}");
    }

    /// daemon 卡死（不回复）→ CMD_TIMEOUT 后返 unresponsive。
    ///
    /// 注：CMD_TIMEOUT = 2s，这条测试真跑要 2s。靠 tokio::time::pause 可以
    /// 用虚拟时钟瞬间推进，但要 tokio "test-util" feature；为省一个 feature
    /// 依赖，这里改成 reply oneshot 立刻 drop（让 reply_rx 收到 RecvError），
    /// 走 `Ok(Err(_))` 分支断 "reply dropped" 错误。timeout 分支由 sub_*
    /// 内显式调用 `tokio::time::timeout` 做兜底，函数结构本身已被覆盖。
    #[tokio::test]
    async fn reply_dropped_returns_business_error() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        // 接到命令立刻 drop reply（CronCommand 内部的 oneshot::Sender）
        let drainer = tokio::spawn(async move {
            if let Some(cmd) = cmd_rx.recv().await {
                drop(cmd); // 包含 reply_tx，drop 后 reply_rx.await -> Err
            }
        });
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.status"}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "error");
        let msg = resp["result"]["error"].as_str().unwrap();
        assert!(msg.contains("reply dropped"), "{msg}");
        let _ = drainer.await;
    }

    /// cron.run：缺 id 返业务错误
    #[tokio::test]
    async fn run_without_id_returns_business_error() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.run"}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "error");
        assert!(
            resp["result"]["error"]
                .as_str()
                .unwrap()
                .contains("缺少 id"),
        );
    }

    /// cron.run 命中 daemon -> Ok(()) 路径
    #[tokio::test]
    async fn run_method_roundtrips_ok() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let stub = tokio::spawn(async move {
            if let Some(CronCommand::Run { id, reply }) = cmd_rx.recv().await {
                assert_eq!(id, "abc");
                let _ = reply.send(Ok(()));
            }
        });
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.run","params":{"id":"abc"}}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "ok");
        assert_eq!(resp["result"]["triggered"], true);
        let _ = stub.await;
    }

    /// cron.run 命中 NotFound 业务错误
    #[tokio::test]
    async fn run_not_found_returns_business_error() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let stub = tokio::spawn(async move {
            if let Some(CronCommand::Run { reply, .. }) = cmd_rx.recv().await {
                let _ = reply.send(Err(acosmi_scheduler::CronError::NotFound("ghost".into())));
            }
        });
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.run","params":{"id":"ghost"}}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "error");
        assert!(resp["result"]["error"].as_str().unwrap().contains("ghost"));
        let _ = stub.await;
    }

    /// cron.runs：缺 id 返业务错误
    #[tokio::test]
    async fn runs_without_id_returns_business_error() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.runs"}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "error");
    }

    /// cron.runs：命中 daemon → 返回 entries 数组（含默认 limit）
    #[tokio::test]
    async fn runs_method_roundtrips_entries() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let stub = tokio::spawn(async move {
            if let Some(CronCommand::Runs { id, limit, reply }) = cmd_rx.recv().await {
                assert_eq!(id, "j-1");
                assert_eq!(limit, RUNS_DEFAULT_LIMIT);
                let entry = acosmi_scheduler::FireLogEntry {
                    job_id: "j-1".into(),
                    fire_ts_ms: 100,
                    session_key: "s".into(),
                    channel_id: None,
                    continuation_kind: "chat".into(),
                    message: "hi".into(),
                    title: None,
                };
                let _ = reply.send(vec![entry]);
            }
        });
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.runs","params":{"id":"j-1"}}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "ok");
        let entries = resp["result"]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["jobId"], "j-1");
        let _ = stub.await;
    }

    /// cron.runs：自定义 limit 透传给 daemon
    #[tokio::test]
    async fn runs_custom_limit_passed_through() {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let stub = tokio::spawn(async move {
            if let Some(CronCommand::Runs { limit, reply, .. }) = cmd_rx.recv().await {
                assert_eq!(limit, 7);
                let _ = reply.send(vec![]);
            }
        });
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.runs","params":{"id":"j-1","limit":7}}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "ok");
        let _ = stub.await;
    }

    // ════════════════════════════════════════════════════════════════════
    // W-CRON-AUTOMATION-E2E P7：cron.outbox_read
    // ════════════════════════════════════════════════════════════════════

    /// cron.outbox_read：空 outbox → events 空、lastEventId 回 afterEventId。
    #[tokio::test]
    async fn outbox_read_empty_returns_cursor_unchanged() {
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.outbox_read","params":{"afterEventId":7}}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "ok");
        assert!(resp["result"]["events"].as_array().unwrap().is_empty());
        assert!(resp["result"]["oldestEventId"].is_null());
        assert!(resp["result"]["newestEventId"].is_null());
        let epoch = resp["result"]["ledgerEpoch"]
            .as_str()
            .expect("outbox read must publish its durable ledger epoch");
        assert_eq!(epoch.len(), 36);
        assert_eq!(epoch.matches('-').count(), 4);
        // 空读 → cursor 不前进，回 afterEventId
        assert_eq!(resp["result"]["lastEventId"], 7);
    }

    /// cron.outbox_read：cursor 过滤 + lastEventId = 返回事件的最大 id。
    #[tokio::test]
    async fn outbox_read_filters_by_cursor() {
        use acosmi_scheduler::task_store::{
            CronJobState, CronPayload, CronSchedule, PayloadKind, ScheduleKind, SessionTarget,
            WakeMode,
        };
        use acosmi_scheduler::{OutboxEvent, OutboxEventType};
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();

        // 直接写 3 条 outbox 事件
        let mk_job = |id: &str| acosmi_scheduler::CronJob {
            id: id.into(),
            agent_id: None,
            name: format!("job-{id}"),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: Some(true),
            created_at_ms: 1000,
            updated_at_ms: 1000,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2030-01-01T00:00:00+00:00".into()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: None,
                message: Some("m".into()),
                model: None,
                thinking: None,
                timeout_seconds: None,
                allow_unsafe_external_content: None,
                deliver: None,
                channel: None,
                to: None,
                best_effort_deliver: None,
            },
            delivery: None,
            state: CronJobState::default(),
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };
        for i in 1..=3_u64 {
            let job = mk_job(&format!("j{i}"));
            let ev = OutboxEvent::from_job(&job, i, OutboxEventType::Fired, 1000);
            acosmi_scheduler::outbox::append(td.path(), &ev).await;
        }

        // afterEventId=1 → 拿 id 2,3；lastEventId=3
        let resp = handle_request_line(
            r#"{"id":"1","method":"cron.outbox_read","params":{"afterEventId":1}}"#,
            &cmd_tx,
            &events,
            td.path(),
        )
        .await;
        assert_eq!(resp["result"]["status"], "ok");
        let evs = resp["result"]["events"].as_array().unwrap();
        assert_eq!(evs.len(), 2);
        assert_eq!(evs[0]["eventId"], 2);
        assert_eq!(evs[0]["eventType"], "fired");
        assert_eq!(resp["result"]["oldestEventId"], 1);
        assert_eq!(resp["result"]["newestEventId"], 3);
        assert_eq!(resp["result"]["lastEventId"], 3);
    }

    /// `cron.claim` parser (no actor, mirroring the delivery `ledger_writer:
    /// None` guards): valid params with no actor fall back to the SAME
    /// actor-unavailable shape the delivery methods use; an unknown consumerKind
    /// or target is rejected `invalid_claim_request` BEFORE the actor check; a
    /// missing required field fails the parse the same way.
    #[tokio::test]
    async fn claim_parser_rejects_unknown_kind_target_and_falls_back_without_actor() {
        // Valid params, no actor → delivery-style actor-unavailable fallback.
        let valid = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x",
            "consumerKind": "tui",
            "consumerId": "tui-1",
            "target": "isolated",
        });
        let unavailable = sub_claim(Some(&valid), None, "cron-state-v1:x").await;
        assert_eq!(unavailable["status"], "error");
        assert_eq!(unavailable["code"], "delivery_actor_unavailable");

        // Unknown consumerKind → invalid_claim_request (before the actor check).
        let bad_kind = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x",
            "consumerKind": "robot",
            "consumerId": "x",
            "target": "main",
        });
        let kind_err = sub_claim(Some(&bad_kind), None, "cron-state-v1:x").await;
        assert_eq!(kind_err["code"], "invalid_claim_request");
        assert!(
            kind_err["error"].as_str().unwrap().contains("consumerKind"),
            "{kind_err}"
        );

        // Unknown target → invalid_claim_request.
        let bad_target = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x",
            "consumerKind": "tui",
            "consumerId": "x",
            "target": "sideways",
        });
        let target_err = sub_claim(Some(&bad_target), None, "cron-state-v1:x").await;
        assert_eq!(target_err["code"], "invalid_claim_request");
        assert!(
            target_err["error"].as_str().unwrap().contains("target"),
            "{target_err}"
        );

        // Missing required field (consumerId) → parse failure → invalid_claim_request.
        let missing = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x",
            "consumerKind": "tui",
            "target": "main",
        });
        let missing_err = sub_claim(Some(&missing), None, "cron-state-v1:x").await;
        assert_eq!(missing_err["code"], "invalid_claim_request");
    }

    /// `cron.claim` end to end over the real actor: fire one Main-target
    /// occurrence, then the IPC claim returns it with every i64 field as a
    /// DECIMAL STRING (wire-number discipline) + the envelope embedded as an
    /// object + identity metadata (claim is a mutation) but NO ledgerEpoch (claim
    /// is occurrence-keyed, not a delivery subcommand). A second claim → noWork.
    #[tokio::test]
    async fn claim_ipc_emits_big_numbers_as_strings_then_reports_no_work() {
        use acosmi_scheduler::{
            CronJob, CronJobState, CronPayload, CronSchedule, OutboxEventType, PayloadKind,
            ScheduleKind, SessionTarget, WakeMode,
        };
        const STATE: &str = "cron-state-v1:claim-ipc";
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let ledger = crate::open_test_ledger(td.path());
        let (ledger_writer, ledger_task) = crate::spawn_cron_ledger_writer(
            td.path().to_path_buf(),
            1,
            STATE.to_string(),
            TEST_LEDGER_EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();

        // Fire one Main-target occurrence (with its projected envelope) through
        // the actor, exactly as production does on a cron fire.
        let job = CronJob {
            id: "claim-ipc-job".to_string(),
            agent_id: None,
            name: "claim-ipc".to_string(),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: None,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2030-01-01T00:00:00+00:00".to_string()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: None,
                message: Some("run-ipc".to_string()),
                model: None,
                thinking: None,
                timeout_seconds: None,
                allow_unsafe_external_content: None,
                deliver: None,
                channel: None,
                to: None,
                best_effort_deliver: None,
            },
            delivery: None,
            state: CronJobState::default(),
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        };
        ledger_writer
            .append_job(
                &job,
                OutboxEventType::Fired,
                1_800_000_077_000_i64,
                acosmi_cron_ledger::OccurrenceKind::Scheduled,
                acosmi_cron_ledger::FireAdvance::NoAdvance,
            )
            .await
            .expect("fire must record a claimable occurrence");

        let request = serde_json::json!({
            "id": "claim",
            "method": "cron.claim",
            "params": {
                "expectedStateIdentity": STATE,
                "consumerKind": "tui",
                "consumerId": "tui-ipc",
                "target": "main",
            }
        });
        let claimed = handle_request_line_for_state_with_ledger(
            &request.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        let result = &claimed["result"];
        assert_eq!(result["status"], "ok");
        assert_eq!(result["claimed"], true);
        assert_eq!(result["jobId"], "claim-ipc-job");
        // Wire-number discipline: i64 fields cross as decimal STRINGS.
        assert!(result["occurrenceId"].is_string(), "{result}");
        assert!(result["attemptId"].is_string(), "{result}");
        assert!(result["fence"].is_string(), "{result}");
        assert!(result["scheduledAtMs"].is_string(), "{result}");
        assert!(result["leaseExpiresAtMs"].is_string(), "{result}");
        assert_eq!(result["fence"], "1");
        assert_eq!(result["scheduledAtMs"], "1800000077000");
        assert!(
            result["leaseToken"]
                .as_str()
                .is_some_and(|token| !token.is_empty()),
            "the actor must mint a non-empty lease token"
        );
        // Envelope embedded as a JSON object.
        assert_eq!(result["envelope"]["target"], "main");
        assert_eq!(result["envelope"]["message"], "run-ipc");
        // Identity metadata present (claim is a mutation); ledgerEpoch absent
        // (claim is occurrence-keyed, not a delivery subcommand).
        assert_eq!(result["stateIdentity"], STATE);
        assert_eq!(result["stateIdentityMatches"], true);
        assert!(result.get("ledgerEpoch").is_none(), "{result}");

        // The single occurrence is now leased → a second claim finds no work.
        let again = handle_request_line_for_state_with_ledger(
            &request.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(again["result"]["status"], "ok");
        assert_eq!(again["result"]["noWork"], true);

        drop(ledger_writer);
        ledger_task.await.expect("ledger task");
    }

    /// A Main-target `CronJob` for the W3-3 IPC round-trips (systemEvent payload,
    /// so a fire records a claimable, envelope-bearing occurrence).
    fn ipc_test_job(id: &str, message: &str) -> acosmi_scheduler::CronJob {
        use acosmi_scheduler::{
            CronJob, CronJobState, CronPayload, CronSchedule, PayloadKind, ScheduleKind,
            SessionTarget, WakeMode,
        };
        CronJob {
            id: id.to_string(),
            agent_id: None,
            name: format!("job-{id}"),
            description: None,
            owner: None,
            enabled: true,
            delete_after_run: None,
            created_at_ms: 1_000,
            updated_at_ms: 1_000,
            schedule: CronSchedule {
                kind: ScheduleKind::At,
                at: Some("2030-01-01T00:00:00+00:00".to_string()),
                every_ms: None,
                anchor_ms: None,
                expr: None,
                tz: None,
            },
            session_target: SessionTarget::Main,
            wake_mode: WakeMode::NextHeartbeat,
            payload: CronPayload {
                kind: PayloadKind::SystemEvent,
                text: None,
                message: Some(message.to_string()),
                model: None,
                thinking: None,
                timeout_seconds: None,
                allow_unsafe_external_content: None,
                deliver: None,
                channel: None,
                to: None,
                best_effort_deliver: None,
            },
            delivery: None,
            state: CronJobState::default(),
            permanent: None,
            session_key: None,
            channel_id: None,
            continuation_kind: None,
        }
    }

    /// PR5-W3-3 parsers (no actor, mirroring `sub_claim`'s `ledger_writer: None`
    /// guards): valid params with no actor fall back to the SAME actor-unavailable
    /// shape the delivery methods use; malformed input is rejected with the
    /// method's `invalid_*_request` code BEFORE the actor check.
    #[tokio::test]
    async fn writeback_lifecycle_parsers_reject_bad_input_and_fall_back_without_actor() {
        // accept: valid → actor-unavailable; non-decimal occurrenceId → invalid.
        let valid_accept = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x",
            "occurrenceId": "10", "attemptId": "20", "fence": "1", "durationMs": "42",
        });
        assert_eq!(
            sub_accept(Some(&valid_accept), None, "cron-state-v1:x").await["code"],
            "delivery_actor_unavailable"
        );
        let bad_accept = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x",
            "occurrenceId": "not-a-number", "attemptId": "20", "fence": "1",
        });
        let bad_accept_res = sub_accept(Some(&bad_accept), None, "cron-state-v1:x").await;
        assert_eq!(bad_accept_res["code"], "invalid_accept_request");
        assert!(
            bad_accept_res["error"]
                .as_str()
                .unwrap()
                .contains("occurrenceId"),
            "{bad_accept_res}"
        );

        // report_failure: valid → actor-unavailable; unknown errorKind → invalid.
        let valid_rf = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x",
            "occurrenceId": "10", "attemptId": "20", "fence": "1",
            "errorKind": "failure", "errorMessage": "oops",
        });
        assert_eq!(
            sub_report_failure(Some(&valid_rf), None, "cron-state-v1:x").await["code"],
            "delivery_actor_unavailable"
        );
        let bad_kind = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x",
            "occurrenceId": "10", "attemptId": "20", "fence": "1", "errorKind": "explode",
        });
        let bad_kind_res = sub_report_failure(Some(&bad_kind), None, "cron-state-v1:x").await;
        assert_eq!(bad_kind_res["code"], "invalid_report_failure_request");
        assert!(
            bad_kind_res["error"]
                .as_str()
                .unwrap()
                .contains("errorKind"),
            "{bad_kind_res}"
        );

        // retry: valid → actor-unavailable; non-decimal occurrenceId → invalid.
        let valid_retry = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x", "occurrenceId": "10",
        });
        assert_eq!(
            sub_retry(Some(&valid_retry), None, "cron-state-v1:x").await["code"],
            "delivery_actor_unavailable"
        );
        let bad_retry = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x", "occurrenceId": "NaN",
        });
        assert_eq!(
            sub_retry(Some(&bad_retry), None, "cron-state-v1:x").await["code"],
            "invalid_retry_request"
        );

        // cancel: valid → actor-unavailable; empty occurrenceId → invalid.
        let valid_cancel = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x", "occurrenceId": "10",
        });
        assert_eq!(
            sub_cancel(Some(&valid_cancel), None, "cron-state-v1:x").await["code"],
            "delivery_actor_unavailable"
        );
        let bad_cancel = serde_json::json!({
            "expectedStateIdentity": "cron-state-v1:x", "occurrenceId": "",
        });
        assert_eq!(
            sub_cancel(Some(&bad_cancel), None, "cron-state-v1:x").await["code"],
            "invalid_cancel_request"
        );

        // occurrence_history (READ — 2-arg): valid → actor-unavailable; missing
        // jobId → invalid.
        let valid_hist = serde_json::json!({"jobId": "job-1"});
        assert_eq!(
            sub_occurrence_history(Some(&valid_hist), None).await["code"],
            "delivery_actor_unavailable"
        );
        let bad_hist = serde_json::json!({"nope": "x"});
        assert_eq!(
            sub_occurrence_history(Some(&bad_hist), None).await["code"],
            "invalid_occurrence_history_request"
        );
    }

    /// PR5-W3-3 `cron.accept` end to end over the real actor: fire → claim →
    /// accept records the run, every echoed i64 a DECIMAL STRING, identity
    /// metadata present (accept is a mutation) but NO ledgerEpoch (occurrence-
    /// keyed). Then `cron.occurrence_history` with NO expectedStateIdentity still
    /// succeeds — the read is not identity-gated — returning the joined row with
    /// decimal-string i64s.
    #[tokio::test]
    async fn accept_ipc_records_decimal_strings_identity_no_epoch_history_ungated() {
        use acosmi_cron_ledger::{
            ClaimOutcome, ConsumerClaim, ConsumerKind, EnvelopeTarget, FireAdvance, OccurrenceKind,
        };
        use acosmi_scheduler::OutboxEventType;
        const STATE: &str = "cron-state-v1:accept-ipc";
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let ledger = crate::open_test_ledger(td.path());
        let (ledger_writer, ledger_task) = crate::spawn_cron_ledger_writer(
            td.path().to_path_buf(),
            1,
            STATE.to_string(),
            TEST_LEDGER_EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();

        ledger_writer
            .append_job(
                &ipc_test_job("acc-ipc-job", "run-acc"),
                OutboxEventType::Fired,
                1_800_000_500_000,
                OccurrenceKind::Scheduled,
                FireAdvance::NoAdvance,
            )
            .await
            .expect("fire");
        let claimed = match ledger_writer
            .claim_next(ConsumerClaim {
                kind: ConsumerKind::Tui,
                id: "tui".to_string(),
                target: EnvelopeTarget::Main,
            })
            .await
            .expect("claim")
        {
            ClaimOutcome::Claimed(c) => c,
            ClaimOutcome::NoWork => panic!("expected a claimed occurrence"),
        };

        let accept = serde_json::json!({
            "id": "accept",
            "method": "cron.accept",
            "params": {
                "expectedStateIdentity": STATE,
                "occurrenceId": claimed.occurrence_id.to_string(),
                "attemptId": claimed.attempt_id.to_string(),
                "fence": claimed.fence.to_string(),
                "durationMs": "88",
            }
        });
        let recorded = handle_request_line_for_state_with_ledger(
            &accept.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        let result = &recorded["result"];
        assert_eq!(result["status"], "ok");
        assert_eq!(result["outcome"], "recorded");
        assert!(result["occurrenceId"].is_string(), "{result}");
        assert!(result["fence"].is_string(), "{result}");
        assert_eq!(result["occurrenceId"], claimed.occurrence_id.to_string());
        assert_eq!(result["fence"], claimed.fence.to_string());
        // Identity metadata present (mutation); ledgerEpoch absent (not delivery).
        assert_eq!(result["stateIdentity"], STATE);
        assert_eq!(result["stateIdentityMatches"], true);
        assert!(result.get("ledgerEpoch").is_none(), "{result}");

        // occurrence_history WITHOUT expectedStateIdentity → still ok (not gated).
        let history_req = serde_json::json!({
            "id": "h",
            "method": "cron.occurrence_history",
            "params": {"jobId": "acc-ipc-job"}
        });
        let hist = handle_request_line_for_state_with_ledger(
            &history_req.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        let hres = &hist["result"];
        assert_eq!(
            hres["status"], "ok",
            "occurrence_history must not be identity-gated: {hres}"
        );
        let rows = hres["history"].as_array().expect("history array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"], "completed");
        assert!(rows[0]["occurrenceId"].is_string());
        assert!(rows[0]["scheduledAtMs"].is_string());
        assert!(rows[0]["attemptId"].is_string());
        assert_eq!(rows[0]["resultStatus"], "success");
        assert_eq!(rows[0]["durationMs"], "88");
        assert_eq!(rows[0]["target"], "main");

        drop(ledger_writer);
        ledger_task.await.expect("ledger task");
    }

    /// PR5-W3-3 `cron.report_failure` + `cron.retry` over the real actor: a failed
    /// writeback records `failed`, retry resets it (post-bump fence as a decimal
    /// string), and a late `cron.accept` at the pre-retry fence is `fenced`.
    #[tokio::test]
    async fn report_failure_then_retry_ipc_roundtrip_and_stale_accept_is_fenced() {
        use acosmi_cron_ledger::{
            ClaimOutcome, ConsumerClaim, ConsumerKind, EnvelopeTarget, FireAdvance, OccurrenceKind,
        };
        use acosmi_scheduler::OutboxEventType;
        const STATE: &str = "cron-state-v1:rf-ipc";
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let ledger = crate::open_test_ledger(td.path());
        let (ledger_writer, ledger_task) = crate::spawn_cron_ledger_writer(
            td.path().to_path_buf(),
            1,
            STATE.to_string(),
            TEST_LEDGER_EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();

        ledger_writer
            .append_job(
                &ipc_test_job("rf-ipc-job", "run-rf"),
                OutboxEventType::Fired,
                1_800_000_600_000,
                OccurrenceKind::Scheduled,
                FireAdvance::NoAdvance,
            )
            .await
            .expect("fire");
        let claimed = match ledger_writer
            .claim_next(ConsumerClaim {
                kind: ConsumerKind::Tui,
                id: "tui".to_string(),
                target: EnvelopeTarget::Main,
            })
            .await
            .expect("claim")
        {
            ClaimOutcome::Claimed(c) => c,
            ClaimOutcome::NoWork => panic!("expected a claimed occurrence"),
        };

        let rf = serde_json::json!({
            "id": "rf",
            "method": "cron.report_failure",
            "params": {
                "expectedStateIdentity": STATE,
                "occurrenceId": claimed.occurrence_id.to_string(),
                "attemptId": claimed.attempt_id.to_string(),
                "fence": claimed.fence.to_string(),
                "errorKind": "failure",
                "errorMessage": "bad run",
                "durationMs": "9",
            }
        });
        let rf_res = handle_request_line_for_state_with_ledger(
            &rf.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(rf_res["result"]["outcome"], "recorded");
        assert!(rf_res["result"]["occurrenceId"].is_string());
        assert_eq!(rf_res["result"]["fence"], claimed.fence.to_string());
        assert_eq!(rf_res["result"]["stateIdentityMatches"], true);
        assert!(rf_res["result"].get("ledgerEpoch").is_none());

        let retry = serde_json::json!({
            "id": "retry",
            "method": "cron.retry",
            "params": {
                "expectedStateIdentity": STATE,
                "occurrenceId": claimed.occurrence_id.to_string(),
            }
        });
        let retry_res = handle_request_line_for_state_with_ledger(
            &retry.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(retry_res["result"]["outcome"], "retried");
        assert!(retry_res["result"]["fence"].is_string());
        assert_eq!(
            retry_res["result"]["fence"],
            (claimed.fence + 1).to_string()
        );

        // The pre-retry claimer's late accept is fenced: the occurrence advanced
        // past `claimed.fence` (retry bumped it), so nothing is written.
        let stale = serde_json::json!({
            "id": "stale",
            "method": "cron.accept",
            "params": {
                "expectedStateIdentity": STATE,
                "occurrenceId": claimed.occurrence_id.to_string(),
                "attemptId": claimed.attempt_id.to_string(),
                "fence": claimed.fence.to_string(),
            }
        });
        let stale_res = handle_request_line_for_state_with_ledger(
            &stale.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(stale_res["result"]["outcome"], "fenced");
        assert_eq!(
            stale_res["result"]["expectedFence"],
            claimed.fence.to_string()
        );
        assert_eq!(
            stale_res["result"]["actualFence"],
            (claimed.fence + 1).to_string()
        );

        drop(ledger_writer);
        ledger_task.await.expect("ledger task");
    }

    /// PR5-W3-3 `cron.cancel` over the real actor: cancel terminates the claimed
    /// occurrence (`cancelled`, post-bump fence); a second cancel is
    /// `not_cancellable` and a retry of the now-`skipped` occurrence is
    /// `not_retryable` — both surfacing the occurrence's current status under the
    /// non-colliding `status_` key.
    #[tokio::test]
    async fn cancel_ipc_roundtrip_then_not_cancellable_and_not_retryable() {
        use acosmi_cron_ledger::{
            ClaimOutcome, ConsumerClaim, ConsumerKind, EnvelopeTarget, FireAdvance, OccurrenceKind,
        };
        use acosmi_scheduler::OutboxEventType;
        const STATE: &str = "cron-state-v1:cancel-ipc";
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let ledger = crate::open_test_ledger(td.path());
        let (ledger_writer, ledger_task) = crate::spawn_cron_ledger_writer(
            td.path().to_path_buf(),
            1,
            STATE.to_string(),
            TEST_LEDGER_EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();

        ledger_writer
            .append_job(
                &ipc_test_job("cx-ipc-job", "run-cx"),
                OutboxEventType::Fired,
                1_800_000_700_000,
                OccurrenceKind::Scheduled,
                FireAdvance::NoAdvance,
            )
            .await
            .expect("fire");
        let claimed = match ledger_writer
            .claim_next(ConsumerClaim {
                kind: ConsumerKind::Tui,
                id: "tui".to_string(),
                target: EnvelopeTarget::Main,
            })
            .await
            .expect("claim")
        {
            ClaimOutcome::Claimed(c) => c,
            ClaimOutcome::NoWork => panic!("expected a claimed occurrence"),
        };

        let cancel = serde_json::json!({
            "id": "cancel",
            "method": "cron.cancel",
            "params": {"expectedStateIdentity": STATE, "occurrenceId": claimed.occurrence_id.to_string()}
        });
        let cancel_res = handle_request_line_for_state_with_ledger(
            &cancel.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(cancel_res["result"]["outcome"], "cancelled");
        assert!(cancel_res["result"]["occurrenceId"].is_string());
        assert_eq!(
            cancel_res["result"]["fence"],
            (claimed.fence + 1).to_string()
        );
        assert!(cancel_res["result"].get("ledgerEpoch").is_none());

        // A second cancel: already terminal → not_cancellable, status_ = skipped.
        let again = serde_json::json!({
            "id": "cancel2",
            "method": "cron.cancel",
            "params": {"expectedStateIdentity": STATE, "occurrenceId": claimed.occurrence_id.to_string()}
        });
        let again_res = handle_request_line_for_state_with_ledger(
            &again.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(again_res["result"]["outcome"], "not_cancellable");
        assert_eq!(again_res["result"]["status_"], "skipped");

        // Retry of a skipped (non-failed) occurrence → not_retryable, status_ = skipped.
        let retry = serde_json::json!({
            "id": "retry",
            "method": "cron.retry",
            "params": {"expectedStateIdentity": STATE, "occurrenceId": claimed.occurrence_id.to_string()}
        });
        let retry_res = handle_request_line_for_state_with_ledger(
            &retry.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(retry_res["result"]["outcome"], "not_retryable");
        assert_eq!(retry_res["result"]["status_"], "skipped");

        drop(ledger_writer);
        ledger_task.await.expect("ledger task");
    }

    /// PR5-W3-3: `cron.occurrence_history` is NOT identity-gated — unlike a
    /// mutation, a call with NO `expectedStateIdentity` against an actor bound to
    /// a specific identity still succeeds (here an absent job → empty history),
    /// and carries null identity metadata like any read.
    #[tokio::test]
    async fn occurrence_history_ipc_is_not_identity_gated() {
        const STATE: &str = "cron-state-v1:hist-gate";
        let (cmd_tx, _cmd_rx) = mpsc::channel::<CronCommand>(1);
        let events: EventQueue = Arc::new(Mutex::new(Vec::new()));
        let td = tmp_dir();
        let ledger = crate::open_test_ledger(td.path());
        let (ledger_writer, ledger_task) = crate::spawn_cron_ledger_writer(
            td.path().to_path_buf(),
            1,
            STATE.to_string(),
            TEST_LEDGER_EPOCH.to_string(),
            ledger,
        )
        .await
        .unwrap();

        let req = serde_json::json!({
            "id": "h",
            "method": "cron.occurrence_history",
            "params": {"jobId": "absent-job"}
        });
        let resp = handle_request_line_for_state_with_ledger(
            &req.to_string(),
            &cmd_tx,
            &events,
            td.path(),
            Some(&ledger_writer),
            STATE,
            TEST_LEDGER_EPOCH,
        )
        .await;
        assert_eq!(
            resp["result"]["status"], "ok",
            "a read must not require identity: {resp}"
        );
        assert!(
            resp["result"].get("code").is_none(),
            "no state_identity_required for a read: {resp}"
        );
        assert!(
            resp["result"]["history"].as_array().unwrap().is_empty(),
            "an absent job has no history"
        );
        // A read gets null identity metadata (like cron.list), never a match gate.
        assert!(resp["result"]["stateIdentityMatches"].is_null());

        drop(ledger_writer);
        ledger_task.await.expect("ledger task");
    }
}

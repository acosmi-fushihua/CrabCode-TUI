pub mod access_counts;
pub mod atomic_write;
// W-MEMORY-EVOLUTION PR-3 (2026-05-29) — 真 BroadcastEmitter：Tier policy 的
// 反向 IPC LLM / Embedding 请求经 `EventSink` push 给 TUI client pump 翻成
// `ServerNotification` 广播。替换 PR-3 之前 5 个 Tier processor 的
// `RecordingEmitter`（内存空转）。
pub mod broadcast_emitter;
pub mod daily_log;
pub mod dedup_hash;
pub mod derived_gc;
pub mod dream_config;
// W-MEMORY-DATA-COMPLETION Phase 0 (2026-06-20) — assemble the real dream
// corpus (recent-session transcripts + memdir manifest) the Tier-3 dream
// Phase-1 "Orient" prompt consumes. Replaces the `String::new()` placeholders
// both dream call sites previously passed (P0: dream burned LLM calls but saw
// only "(no recent sessions)"/"(memdir empty)" → produced zero insights).
pub mod dream_corpus;
pub mod dream_gate;
// W-MEMORY-EVOLUTION PR-1 (2026-05-29) — events 长连传输地基。`EventSink`
// 持有订阅者写半端，让 orchestrator 能主动 push 通知帧给 TUI client（闭合 B1
// 「UDS 一问一答无法 push」阻塞）。本 PR 只跑 heartbeat 保活帧；真
// ServerNotification emit 留 PR-3。
pub mod event_sink;
pub mod evolution;
pub mod extract_archive;
pub mod extract_cursor;
pub mod frontmatter_repair;
pub mod identity_members;
pub mod importance_pressure;
pub mod ipc_handler;
pub mod leader_lock;
pub mod lock;
pub mod memory_md_analyze;
pub mod output_language;
pub mod result_listener;
pub mod scheduler;
pub mod search_policy;
pub mod search_stats;
// W-MEMORY-DREAM-REBUILD v7 P4.1 (2026-05-25) — Phase 4 起手 PR: acosmi-se
// 搜索引擎接通骨架。SearchEngineIntegration owns the SE handle + reverse-IPC
// embedding emitter + pending oneshot map. Tier processor 写盘后通过
// `upsert_file(path)` API 把文件交给 SE indexer 真实索引。具体 Tier hook
// 留 follow-up（stub-then-wire 模式）。
pub mod se_integration;
// W-MEMORY-DREAM-REBUILD v7 P4.3 (2026-05-25) — Long-running fs-event-driven
// index sync daemon. Complements P4.1 emit-driven path (Tier policy explicit
// upsert) by catching external-trigger writes (user-edited markdown / non-
// orchestrator fs writes). Process-internal background tokio task; no
// TUI client method exposed (WHITELIST + AllowAnyOrigin counts unchanged).
pub mod index_daemon;
pub mod stale_detector;
pub mod status;
// W-MEMORY-DREAM-REBUILD v7 P3.1 (2026-05-25) — Tier1/2/3 policy 共用基础
// 设施 + 反向 IPC LLM 调用契约。具体 Tier policy 实施留 P3.2-P3.5。
pub mod tier;
pub mod transcript_index;
pub mod turn_evaluator;
pub mod watch_config;

// W-MEMORY-DREAM-REBUILD v7 P2.x (2026-05-25): orchestrator 通过 path dep
// 接入 sibling crate（详 CLAUDE.md §硬约束 #15）。
// P2.1 叶子层：core (基础数据结构) + parse (markdown 解析入口)。
// P2.2 加 adapter (TS markdown → Rust struct，跨 workspace 引入 acosmi-segment)。
// P2.3 加 transaction (path 事务锁)。
//
// 2026-07-27 更正：原注释写着"P3.x Tier policy 实施时切换到
// transaction::path_lock"——**这个切换从 2026-05-25 至今没有发生**，
// `lock.rs` 仍是自己的简单文件锁。留着这句会给下一个读代码的人一条已过期
// 的行动指引（诱导按一个并不存在的计划去重构）。当前实况见下方 pub use
// 段的说明。
// 后续 P2.4 接入 session / queue / vfs / se。
pub use acosmi_memory_adapter as adapter;
pub use acosmi_memory_core as core;
pub use acosmi_memory_parse as parse;
pub use acosmi_memory_transaction as transaction;
// P2.4 业务核心 + 搜索引擎入口（最后一波 path dep）。
//
// 2026-07-27 可达性实况（`rg -c "acosmi_memory_<x>"` 在本 crate src/ 下逐个
// 实测）：**transaction / queue / vfs 各只命中 1 次，且就是下面这几行
// `pub use` 本身**——它们在生产链路上没有任何调用方，当前仅作再导出。
// session 只有 trait 面在产（`acosmi-memory-se/src/vector_store_adapter.rs`
// 取用其中七个符号），compressor / extractor / retriever 等实现零调用方。
// 此处**只做如实标注，不做删除** —— CLAUDE.md §15-4 明示未来删除须单独
// 审计外部消费者、ABI、构建与分发合同。
//
// session: Compressor + Extractor + Deduplicator（per-LLM-sample 记忆抽取）。
// queue: 异步嵌入队列 + 语义处理（embedding pipeline）。
// vfs: FS + VectorStore + Embedder 组合（写盘隔离 + 抽象 store backends）。
// se: 进程内搜索引擎封装（HNSW + 倒排 + 多语言 + 量化；
//     跨 workspace path dep → ../../acosmi-se/acosmi-segment etc.）
pub use acosmi_memory_queue as queue;
pub use acosmi_memory_se as se;
pub use acosmi_memory_session as session;
pub use acosmi_memory_vfs as vfs;

use anyhow::{anyhow, Result};

pub const MEMORY_IPC_ENDPOINT_ENV: &str = "CRABCODE_MEMORY_IPC_ENDPOINT";
pub const MEMORY_PROTOCOL_VERSION: u64 = 1;
pub const MEMORY_SCHEMA_ID: &str = "crabcode-memory-ipc-v1-20260725";
pub const MEMORY_SERVICE_IDENTITY: &str = "acosmi-memory-orchestrator";
// Keep the v1 discovery token so older launchers identify this as the same
// coordinator surface. The v2 token is an explicit payload upgrade: this
// owner intentionally rejects an unbound legacy promotion request rather than
// letting an older caller replace a newer live process.
pub const MEMORY_CAPABILITIES: &[&str] = &[
    "coordinator-promote-v1",
    "coordinator-promote-owner-bind-v2",
    "events-v1",
    "runner-journal-v1",
];
pub const MEMORY_PROMOTION_EXIT_CODE: i32 = 75;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServeExit {
    Promoted,
}

// W-MEMORY-EVOLUTION PR-1 (2026-05-29) — events 长连保活心跳。
/// IPC method that turns a one-shot request connection into a persistent
/// events subscriber. The connection is kept open and registered with the
/// `EventSink`; the orchestrator pushes notification frames over it.
pub const MEMORY_EVENTS_SUBSCRIBE_METHOD: &str = "memory.events.subscribe";

/// Default heartbeat interval (ms) for the events long-connection keepalive
/// frame. Overridable via `CRABCODE_MEMORY_HEARTBEAT_MS` so tests can use a
/// short interval instead of waiting 30s.
const DEFAULT_HEARTBEAT_MS: u64 = 30_000;
const HEARTBEAT_MS_ENV: &str = "CRABCODE_MEMORY_HEARTBEAT_MS";

fn heartbeat_interval_ms() -> u64 {
    match std::env::var(HEARTBEAT_MS_ENV) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => ms,
            _ => DEFAULT_HEARTBEAT_MS,
        },
        Err(_) => DEFAULT_HEARTBEAT_MS,
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse the IPC `method` field from a request line without fully decoding
/// the payload (the events-subscribe branch only needs the method).
fn parse_request_method(buf: &[u8]) -> Option<String> {
    serde_json::from_slice::<serde_json::Value>(buf)
        .ok()
        .and_then(|v| {
            v.get("method")
                .and_then(|m| m.as_str())
                .map(|s| s.to_string())
        })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryEndpoint {
    Unix(String),
    WindowsNamedPipe(String),
}

pub fn parse_endpoint(endpoint: &str) -> Result<MemoryEndpoint> {
    if let Some(path) = endpoint.strip_prefix("unix:") {
        if path.is_empty() {
            return Err(anyhow!("memory IPC unix endpoint path is empty"));
        }
        return Ok(MemoryEndpoint::Unix(path.to_string()));
    }

    if let Some(pipe) = endpoint.strip_prefix("npipe:") {
        if pipe.is_empty() {
            return Err(anyhow!("memory IPC named pipe endpoint is empty"));
        }
        return Ok(MemoryEndpoint::WindowsNamedPipe(pipe.to_string()));
    }

    Err(anyhow!("unsupported memory IPC endpoint: {endpoint}"))
}

pub async fn serve_endpoint(endpoint: &str) -> Result<ServeExit> {
    let journal = std::sync::Arc::new(acosmi_memory_journal::Journal::open_from_env()?);
    serve_endpoint_with_journal(endpoint, journal).await
}

async fn serve_endpoint_with_journal(
    endpoint: &str,
    journal: std::sync::Arc<acosmi_memory_journal::Journal>,
) -> Result<ServeExit> {
    match parse_endpoint(endpoint)? {
        MemoryEndpoint::Unix(path) => serve_unix_endpoint(&path, journal).await,
        MemoryEndpoint::WindowsNamedPipe(pipe) => {
            serve_windows_named_pipe_endpoint(&pipe, journal).await
        }
    }
}

pub async fn ping_endpoint(endpoint: &str) -> Result<serde_json::Value> {
    match parse_endpoint(endpoint)? {
        MemoryEndpoint::Unix(path) => ping_unix_endpoint(&path).await,
        MemoryEndpoint::WindowsNamedPipe(pipe) => ping_windows_named_pipe_endpoint(&pipe).await,
    }
}

// W-MEMORY-EVOLUTION PR-0 (2026-05-29) — D3 去全局 Mutex 根治 B2 死锁。
// `IpcHandler` 现用内部可变（`std::sync::Mutex` 字段 + `&self` 方法），故
// 这里只需 `Arc<IpcHandler>` 共享、无需外层锁。此前的外层
// `Arc<Mutex<IpcHandler>>` 跨整个 `handle_value` await 持有，导致 tier
// process（await LLM 往返）阻塞 llm_call_result delivery 抢同一锁 → 死锁。
async fn handle_ipc_bytes(
    buf: &[u8],
    handler: &std::sync::Arc<crate::ipc_handler::IpcHandler>,
) -> serde_json::Value {
    let request = serde_json::from_slice(buf).unwrap_or_else(|e| {
        serde_json::json!({
            "method": "__invalid__",
            "payload": { "error": e.to_string() }
        })
    });
    handler.handle_value(request).await
}

async fn read_ipc_request<R>(reader: &mut R) -> std::io::Result<Vec<u8>>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut buf = Vec::new();
    let mut reader = BufReader::new(reader);
    reader.read_until(b'\n', &mut buf).await?;
    Ok(buf)
}

/// Encode a one-shot IPC response as a single NDJSON frame (trailing `\n`).
///
/// K1 (W-MEMORY-LIFECYCLE 2026-07-09) — requests are newline-framed
/// (`read_ipc_request` reads until `\n`) but one-shot responses used to go out
/// as bare `serde_json::to_vec` bytes with NO trailing newline. The TS client
/// (`src/bridge/adapters/memoryBridgeIpc.ts::SocketMemoryBridgeDriver.send`)
/// parses responses as newline-delimited frames: without the `\n` a frame
/// never completes on `'data'`. Unix survived via the clean-EOF fallback
/// (`shutdown()` half-close → `'end'` → final-frame parse), but Windows named
/// pipes have no half-close — the server's `disconnect()` surfaces as an
/// `'error' EPIPE` on the client and the fully-buffered response is rejected
/// wholesale. The Rust app-server client fixed the same pathology long ago
/// (`crates/acosmi-app-server/src/dispatcher/memory.rs::
/// try_decode_memory_ipc_response`); this makes the orchestrator side NDJSON-
/// symmetric so every response is parseable the moment its bytes arrive.
/// The events-subscribe ack and `event_sink` push frames already carry `\n`
/// and are not routed through here.
fn encode_one_shot_response(response: &serde_json::Value) -> Vec<u8> {
    // `serde_json::Value` serialization is effectively infallible (object keys
    // are always strings; non-finite floats cannot be constructed in a
    // `Value`) — the fallback frame is purely defensive so the write path can
    // never panic.
    let mut bytes = serde_json::to_vec(response)
        .unwrap_or_else(|_| br#"{"ok":false,"error":"response encode failed"}"#.to_vec());
    bytes.push(b'\n');
    bytes
}

/// K1 request-side mirror (W-MEMORY-LIFECYCLE 2026-07-09) — the ping request
/// is newline-framed because the serve loops frame requests with
/// `read_ipc_request` (`read_until(b'\n')`). Without the trailing `\n` a
/// Windows named-pipe server waits forever for the frame end — the client's
/// `shutdown()` is a NO-OP on named pipes (no half-close, so no EOF ever
/// arrives) — while the client waits for the response: a deterministic
/// deadlock (`e2e_ipc_topology_memory_socket::
/// memory_ping_uses_dedicated_windows_named_pipe_not_stdio` hung exactly
/// here). Unix escaped only because `shutdown()` delivers a real EOF there.
const PING_REQUEST_FRAME: &[u8] = b"{\"method\":\"memory.ping\"}\n";

/// K1 request-side mirror — read one newline-framed one-shot response,
/// parse-on-arrival: return as soon as the frame's `\n` lands instead of
/// waiting for an EOF that Windows named pipes never deliver cleanly. When
/// the transport errors first (server `disconnect()` surfaces as
/// broken-pipe with the response typically already buffered), fall back to
/// parsing whatever WAS buffered and only propagate the error when the
/// buffer holds no parseable frame — mirrors the app-server client's
/// `try_decode_memory_ipc_response` precedent
/// (`crates/acosmi-app-server/src/dispatcher/memory.rs`).
async fn read_one_shot_response<R>(reader: R) -> Result<serde_json::Value>
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    match reader.read_until(b'\n', &mut buf).await {
        Ok(_) => Ok(serde_json::from_slice(&buf)?),
        Err(e) => {
            if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&buf) {
                return Ok(value);
            }
            Err(e.into())
        }
    }
}

#[cfg(unix)]
/// A12 (P1-12) — classify an `accept()` error as a per-process resource
/// exhaustion (too many open files: EMFILE process-level / ENFILE system-level)
/// vs a transient per-connection error (ECONNABORTED / EINTR / EPROTO). On
/// exhaustion the serve loop backs off briefly so it doesn't spin a 100% CPU
/// tight error loop while the fd table drains; transient kinds retry
/// immediately. EMFILE/ENFILE lack stable `io::ErrorKind` variants, so match
/// the raw OS errno (24 = EMFILE, 23 = ENFILE on Linux/macOS).
fn is_resource_exhaustion(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(libc::EMFILE) | Some(libc::ENFILE))
}

/// Removes only the exact Unix socket inode created by this server. A
/// same-user process that unlinks and replaces the pathname cannot make this
/// owner delete the replacement during shutdown.
#[cfg(unix)]
struct BoundUnixSocketPath {
    path: std::path::PathBuf,
    device: u64,
    inode: u64,
    short_parent: Option<OwnedShortSocketParent>,
}

#[cfg(unix)]
struct OwnedShortSocketParent {
    path: std::path::PathBuf,
    device: u64,
    inode: u64,
}

#[cfg(unix)]
impl BoundUnixSocketPath {
    fn capture(path: &std::path::Path) -> Result<Self> {
        use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};

        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.file_type().is_socket() {
            return Err(anyhow!(
                "bound memory endpoint is not a Unix socket: {}",
                path.display()
            ));
        }
        Ok(Self {
            path: path.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
            short_parent: OwnedShortSocketParent::capture(path)?,
        })
    }

    fn remove_owned_short_parent(&self) {
        use std::os::unix::fs::MetadataExt as _;

        let Some(parent) = self.short_parent.as_ref() else {
            return;
        };
        let Ok(metadata) = std::fs::symlink_metadata(&parent.path) else {
            return;
        };
        if metadata.dev() != parent.device || metadata.ino() != parent.inode {
            log::warn!(
                "memory short-socket directory changed ownership before cleanup; preserving replacement: {}",
                parent.path.display()
            );
            return;
        }
        match std::fs::remove_dir(&parent.path) {
            Ok(()) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    || error
                        .raw_os_error()
                        .is_some_and(|code| code == libc::ENOTEMPTY || code == libc::EEXIST) => {}
            Err(error) => log::warn!(
                "failed to remove owned memory short-socket directory {}: {error}",
                parent.path.display()
            ),
        }
    }
}

#[cfg(unix)]
impl OwnedShortSocketParent {
    fn capture(socket_path: &std::path::Path) -> Result<Option<Self>> {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

        if socket_path.file_name() != Some(std::ffi::OsStr::new("memory-orchestrator.sock")) {
            return Ok(None);
        }
        let Some(parent) = socket_path.parent() else {
            return Ok(None);
        };
        if parent.parent() != Some(std::path::Path::new("/tmp")) {
            return Ok(None);
        }
        let Some(namespace) = parent
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .and_then(|name| name.strip_prefix("crabcode-memory-"))
        else {
            return Ok(None);
        };
        if namespace.len() != 32 || !namespace.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Ok(None);
        }

        let metadata = std::fs::symlink_metadata(parent)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.permissions().mode() & 0o777 != 0o700
        {
            return Err(anyhow!(
                "memory short-socket parent is not a private 0700 directory: {}",
                parent.display()
            ));
        }
        Ok(Some(Self {
            path: parent.to_path_buf(),
            device: metadata.dev(),
            inode: metadata.ino(),
        }))
    }
}

#[cfg(unix)]
impl Drop for BoundUnixSocketPath {
    fn drop(&mut self) {
        use std::os::unix::fs::MetadataExt as _;

        let Ok(metadata) = std::fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.dev() != self.device || metadata.ino() != self.inode {
            log::warn!(
                "memory endpoint path changed ownership before cleanup; preserving replacement: {}",
                self.path.display()
            );
            return;
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => self.remove_owned_short_parent(),
            Err(error) => {
                log::warn!(
                    "failed to remove owned memory endpoint {}: {error}",
                    self.path.display()
                );
            }
        }
    }
}

#[cfg(unix)]
async fn serve_unix_endpoint(
    socket_path: &str,
    journal: std::sync::Arc<acosmi_memory_journal::Journal>,
) -> Result<ServeExit> {
    use std::io;
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    use crate::ipc_handler::IpcHandler;

    // Bind without deleting first. The old remove-before-bind sequence could
    // unlink a live listener and let a second process publish the same
    // pathname, splitting Memory authority. Only an address that is both
    // occupied and not connectable is treated as a stale crash artifact.
    let listener = match UnixListener::bind(socket_path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            if tokio::net::UnixStream::connect(socket_path).await.is_ok() {
                return Err(anyhow!(
                    "memory orchestrator endpoint already has a live owner: {socket_path}"
                ));
            }
            match tokio::fs::remove_file(socket_path).await {
                Ok(()) => {}
                Err(remove_error) if remove_error.kind() == io::ErrorKind::NotFound => {}
                Err(remove_error) => return Err(remove_error.into()),
            }
            UnixListener::bind(socket_path)?
        }
        Err(error) => return Err(error.into()),
    };
    let _bound_socket_path = BoundUnixSocketPath::capture(std::path::Path::new(socket_path))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600)).await?;
    }

    // W-MEMORY-EVOLUTION PR-1 — events 长连传输地基。`EventSink` 持订阅者写
    // 半端；heartbeat task 周期 push 保活帧。
    let event_sink = crate::event_sink::EventSink::new();
    spawn_heartbeat_task(Arc::clone(&event_sink));

    // W-MEMORY-EVOLUTION PR-3 — real BroadcastEmitter wiring. The handler's
    // 5 Tier processors now emit reverse-IPC LLM call requests through the
    // events long-connection (`UdsBroadcastEmitter` backed by `event_sink`)
    // instead of the in-memory `RecordingEmitter`. TUI client's
    // `memory_events_pump` reads these frames, maps them to
    // `ServerNotification`, and broadcasts to the TS business side.
    //
    // W-MEMORY-EVOLUTION PR-0 — no outer Mutex (interior mutability in handler).
    let handler = Arc::new(IpcHandler::with_event_sink(
        Arc::clone(&event_sink),
        journal,
    ));
    let recovery = handler
        .recover_runner_settlements()
        .await
        .map_err(|error| anyhow!("recover durable runner settlements: {error}"))?;
    if recovery.candidates > 0 {
        log::info!("[runner-settlement-recovery] startup report: {recovery:?}");
    }
    spawn_durable_imagination_task(Arc::clone(&handler), Arc::clone(&event_sink));
    let (promotion_tx, mut promotion_rx) = tokio::sync::mpsc::channel::<()>(1);

    // W-MEMORY-EVOLUTION PR-5 — periodic / idle auto-dream task. Every
    // `scan_interval_ms` it considers running a Tier-3 dream consolidation
    // against the most-recently-active project (idle + dream_config.enabled +
    // AutoDreamGate all gating it). This is the "self-evolve while the user is
    // away" loop TS can't provide. Runs detached for the life of the process;
    // each tick is fail-soft (errors logged, next tick retries).
    spawn_periodic_dream_task(Arc::clone(&handler));

    loop {
        // A12 fix (P1-12, 2026-06-05) — fail-soft accept. Previously
        // `accept().await?` propagated ANY accept error (ECONNABORTED / EMFILE
        // / ENFILE / EINTR) out of the serve loop, killing the orchestrator for
        // ALL projects. Mirror the per-connection fail-soft below: log + keep
        // serving. For resource-exhaustion kinds (too many open files) back off
        // briefly so we don't spin a 100% CPU tight error loop while the fd
        // table drains.
        let accepted = tokio::select! {
            promoted = promotion_rx.recv() => {
                if promoted.is_some() {
                    return Ok(ServeExit::Promoted);
                }
                continue;
            }
            accepted = listener.accept() => accepted,
        };
        let (mut stream, _) = match accepted {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("orchestrator accept failed (fail-soft, continuing): {e}");
                if is_resource_exhaustion(&e) {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                continue;
            }
        };
        let handler = Arc::clone(&handler);
        let event_sink = Arc::clone(&event_sink);
        let promotion_tx = promotion_tx.clone();
        tokio::spawn(async move {
            let buf = match read_ipc_request(&mut stream).await {
                Ok(buf) => buf,
                Err(e) => {
                    let response = serde_json::json!({ "ok": false, "error": e.to_string() });
                    // K1: NDJSON framing — trailing `\n` (see encode_one_shot_response).
                    let _ = stream.write_all(&encode_one_shot_response(&response)).await;
                    let _ = stream.shutdown().await;
                    return;
                }
            };

            // W-MEMORY-EVOLUTION PR-1: events subscribe branch — keep the
            // connection open as a long-lived events subscriber.
            if parse_request_method(&buf).as_deref() == Some(MEMORY_EVENTS_SUBSCRIBE_METHOD) {
                let (read_half, mut write_half) = stream.into_split();
                // Ack on the write half before handing it to the sink.
                let ack = b"{\"ok\":true,\"subscribed\":true}\n";
                if write_half.write_all(ack).await.is_ok() {
                    let _ = write_half.flush().await;
                    event_sink.register(write_half).await;
                }
                // We only push; the read half can be dropped. Dropping it
                // does not close the write half (split halves own separate
                // refs), so the subscriber stays alive for pushes.
                drop(read_half);
                return;
            }

            // Default: one-shot request → response → shutdown. K1: the frame
            // carries a trailing `\n` (NDJSON symmetry with the request side).
            let response = handle_ipc_bytes(&buf, &handler).await;
            let promote =
                response.get("promote").and_then(serde_json::Value::as_bool) == Some(true);
            let written = stream
                .write_all(&encode_one_shot_response(&response))
                .await
                .is_ok();
            let flushed = stream.flush().await.is_ok();
            let _ = stream.shutdown().await;
            if promote && written && flushed {
                let _ = promotion_tx.send(()).await;
            }
        });
    }
}

// W-MEMORY-EVOLUTION PR-1 — events 长连保活心跳 task。每 `heartbeat_interval_ms`
// push 一个保活帧给所有订阅者。这是链路保活帧（**不是** ServerNotification），
// TUI client 侧 `memory_events_pump` 消费但不广播。无订阅者时 `push_frame` no-op。
#[cfg(unix)]
fn spawn_heartbeat_task(event_sink: std::sync::Arc<crate::event_sink::EventSink>) {
    let interval_ms = heartbeat_interval_ms();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate first tick so the first heartbeat lands
        // after one interval (avoids a t=0 burst before any subscriber).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let frame = serde_json::json!({
                "notification": "memory/events/heartbeat",
                "ts_ms": now_ms(),
            });
            event_sink.push_frame(&frame).await;
        }
    });
}

// W-MEMORY-EVOLUTION PR-5 (2026-05-29) — periodic / idle auto-dream task.
// Drives `ipc_handler::run_dream_tick` on a fixed interval. The per-tick
// decision (idle / enabled / gate) lives in `run_dream_tick` (unit-tested);
// this wrapper just owns the `tokio::time::interval` cadence + fail-soft
// logging. Runs for the life of the process (the serve loop is infinite); no
// explicit shutdown signal needed — the task dies with the runtime when the
// process exits.
#[cfg(unix)]
fn spawn_periodic_dream_task(handler: std::sync::Arc<crate::ipc_handler::IpcHandler>) {
    use crate::ipc_handler::{run_dream_tick, DreamTickConfig, DreamTickOutcome};

    let config = DreamTickConfig::from_env();
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_millis(config.scan_interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate first tick so we don't dream at t=0 before any
        // session has even run a turn (mirrors the heartbeat task pattern).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let outcome = run_dream_tick(&handler, now_ms(), config).await;
            match &outcome {
                DreamTickOutcome::Dreamed { theme_count } => {
                    log::info!("periodic dream task: consolidated ({theme_count} themes)");
                }
                DreamTickOutcome::Errored { error } => {
                    log::warn!("periodic dream task: tick errored (fail-soft): {error}");
                }
                other => {
                    log::debug!("periodic dream task: skipped ({other:?})");
                }
            }
        }
    });
}

/// Drain journaled after-dream imagination work only while the reverse-IPC
/// events channel has a live subscriber. Each claimed row remains durable
/// across process exit; the loop itself does not own correctness.
#[cfg(any(unix, windows))]
fn spawn_durable_imagination_task(
    handler: std::sync::Arc<crate::ipc_handler::IpcHandler>,
    event_sink: std::sync::Arc<crate::event_sink::EventSink>,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if event_sink.subscriber_count().await == 0 {
                continue;
            }
            loop {
                match handler.drain_one_durable_imagination_followup().await {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error) => {
                        log::warn!(
                            "[durable-imagination] drain failed; journal row remains recoverable: {error}"
                        );
                        break;
                    }
                }
            }
        }
    });
}

// W-MEMORY-EVOLUTION W11 PR-5 (2026-05-29) — Windows siblings of the events
// heartbeat + periodic dream tasks. Bodies are identical to the `#[cfg(unix)]`
// versions above (both reference only platform-agnostic `tokio::time`,
// `EventSink::push_frame`, and `run_dream_tick`); kept as separate
// `#[cfg(windows)]` items so the Unix versions stay verbatim.
#[cfg(windows)]
fn spawn_heartbeat_task(event_sink: std::sync::Arc<crate::event_sink::EventSink>) {
    let interval_ms = heartbeat_interval_ms();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate first tick so the first heartbeat lands
        // after one interval (avoids a t=0 burst before any subscriber).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let frame = serde_json::json!({
                "notification": "memory/events/heartbeat",
                "ts_ms": now_ms(),
            });
            event_sink.push_frame(&frame).await;
        }
    });
}

#[cfg(windows)]
fn spawn_periodic_dream_task(handler: std::sync::Arc<crate::ipc_handler::IpcHandler>) {
    use crate::ipc_handler::{run_dream_tick, DreamTickConfig, DreamTickOutcome};

    let config = DreamTickConfig::from_env();
    tokio::spawn(async move {
        let mut ticker =
            tokio::time::interval(std::time::Duration::from_millis(config.scan_interval_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Consume the immediate first tick so we don't dream at t=0 before any
        // session has even run a turn (mirrors the heartbeat task pattern).
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let outcome = run_dream_tick(&handler, now_ms(), config).await;
            match &outcome {
                DreamTickOutcome::Dreamed { theme_count } => {
                    log::info!("periodic dream task: consolidated ({theme_count} themes)");
                }
                DreamTickOutcome::Errored { error } => {
                    log::warn!("periodic dream task: tick errored (fail-soft): {error}");
                }
                other => {
                    log::debug!("periodic dream task: skipped ({other:?})");
                }
            }
        }
    });
}

#[cfg(not(unix))]
async fn serve_unix_endpoint(
    _socket_path: &str,
    _journal: std::sync::Arc<acosmi_memory_journal::Journal>,
) -> Result<ServeExit> {
    Err(anyhow!(
        "memory orchestrator POC endpoint server is Unix-only"
    ))
}

#[cfg(unix)]
async fn ping_unix_endpoint(socket_path: &str) -> Result<serde_json::Value> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixStream;

    let mut stream = UnixStream::connect(socket_path).await?;
    // K1 request-side mirror: newline-framed request; `shutdown()` stays as a
    // belt (real half-close EOF on unix also delimits for older servers).
    stream.write_all(PING_REQUEST_FRAME).await?;
    stream.shutdown().await?;
    read_one_shot_response(stream).await
}

#[cfg(not(unix))]
async fn ping_unix_endpoint(_socket_path: &str) -> Result<serde_json::Value> {
    Err(anyhow!("memory orchestrator POC ping is Unix-only"))
}

/// Drain a named-pipe **server-end** handle before `disconnect()`.
///
/// Windows named pipes have no half-close, and `DisconnectNamedPipe`
/// (`NamedPipeServer::disconnect`) is destructive — it discards any response
/// bytes the client has not yet read. On a one-shot `write_all` + `flush` +
/// `shutdown` + `disconnect` sequence the client (a separate process) often
/// has not drained the pipe yet, so the discard surfaces as
/// `EPIPE: broken pipe` on the client. Unix sockets avoid this for free via the
/// kernel buffer + `shutdown()` half-close; Windows needs an explicit drain.
///
/// `FlushFileBuffers` on a server-end pipe handle blocks until the client has
/// read all buffered data (MSDN), which is exactly the drain we need. The
/// orchestrator runs on a multi-thread runtime and this fires inside the
/// per-connection task, so the brief block does not stall the accept loop; the
/// client is a trusted local process that reads immediately, so it returns in
/// microseconds. Best-effort — the BOOL result is ignored.
#[cfg(windows)]
fn drain_named_pipe_to_client(stream: &tokio::net::windows::named_pipe::NamedPipeServer) {
    use std::os::windows::io::AsRawHandle;
    // SAFETY: the raw handle is the live server-end pipe handle owned by
    // `stream`, valid for the duration of this borrow. FlushFileBuffers only
    // reads/blocks on the handle and does not take ownership of it.
    unsafe {
        windows_sys::Win32::Storage::FileSystem::FlushFileBuffers(
            stream.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
        );
    }
}

#[cfg(windows)]
async fn serve_windows_named_pipe_endpoint(
    pipe: &str,
    journal: std::sync::Arc<acosmi_memory_journal::Journal>,
) -> Result<ServeExit> {
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ServerOptions;

    use crate::ipc_handler::IpcHandler;

    // W-MEMORY-EVOLUTION W11 PR-5 (2026-05-29) — Windows Named Pipe events push
    // chain (previously a stub). This now mirrors `serve_unix_endpoint`: the
    // `EventSink` holds long-connection subscriber write-halves, a heartbeat
    // task keeps them alive, the periodic dream task self-evolves while idle,
    // and the handler is wired with the real `UdsBroadcastEmitter` (Tier
    // processors push reverse-IPC frames). A `memory.events.subscribe` request
    // splits the connection and registers the write-half with the `EventSink`
    // (matching the Unix subscribe branch) instead of falling through to a
    // one-shot response.
    //
    // W-MEMORY-EVOLUTION PR-0 — no outer Mutex (interior mutability in handler).
    let event_sink = crate::event_sink::EventSink::new();
    spawn_heartbeat_task(Arc::clone(&event_sink));

    let handler = Arc::new(IpcHandler::with_event_sink(
        Arc::clone(&event_sink),
        journal,
    ));
    let recovery = handler
        .recover_runner_settlements()
        .await
        .map_err(|error| anyhow!("recover durable runner settlements: {error}"))?;
    if recovery.candidates > 0 {
        log::info!("[runner-settlement-recovery] startup report: {recovery:?}");
    }
    spawn_durable_imagination_task(Arc::clone(&handler), Arc::clone(&event_sink));
    spawn_periodic_dream_task(Arc::clone(&handler));
    let (promotion_tx, mut promotion_rx) = tokio::sync::mpsc::channel::<()>(1);

    let mut server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(pipe)?;
    loop {
        tokio::select! {
            promoted = promotion_rx.recv() => {
                if promoted.is_some() {
                    return Ok(ServeExit::Promoted);
                }
                continue;
            }
            connected = server.connect() => connected?,
        }
        // Move the connected instance to the worker; immediately create the
        // next instance to keep accepting (cron `serve_windows` pattern).
        let connected = server;
        server = ServerOptions::new().create(pipe)?;
        let handler = Arc::clone(&handler);
        let event_sink = Arc::clone(&event_sink);
        let promotion_tx = promotion_tx.clone();
        tokio::spawn(async move {
            let mut stream = connected;
            let buf = match read_ipc_request(&mut stream).await {
                Ok(buf) => buf,
                Err(e) => {
                    let response = serde_json::json!({ "ok": false, "error": e.to_string() });
                    // K1: NDJSON framing — trailing `\n` (see encode_one_shot_response).
                    let _ = stream.write_all(&encode_one_shot_response(&response)).await;
                    let _ = stream.flush().await;
                    drain_named_pipe_to_client(&stream);
                    let _ = stream.shutdown().await;
                    let _ = stream.disconnect();
                    return;
                }
            };

            // W-MEMORY-EVOLUTION W11 PR-5: events subscribe branch — keep the
            // connection open as a long-lived events subscriber (mirrors the
            // Unix subscribe branch in `serve_unix_endpoint`).
            if parse_request_method(&buf).as_deref() == Some(MEMORY_EVENTS_SUBSCRIBE_METHOD) {
                let (read_half, mut write_half) = tokio::io::split(stream);
                let ack = b"{\"ok\":true,\"subscribed\":true}\n";
                if write_half.write_all(ack).await.is_ok() {
                    let _ = write_half.flush().await;
                    event_sink.register(write_half).await;
                }
                // We only push; the read half can be dropped. The split halves
                // own separate refs, so dropping the read half does not close
                // the write half — the subscriber stays alive for pushes.
                drop(read_half);
                return;
            }

            // Default: one-shot request → response → shutdown. K1: the frame
            // carries a trailing `\n` (NDJSON symmetry with the request side)
            // so the TS client can parse it on arrival instead of relying on
            // an EOF that Windows named pipes never deliver cleanly.
            let response = handle_ipc_bytes(&buf, &handler).await;
            let promote =
                response.get("promote").and_then(serde_json::Value::as_bool) == Some(true);
            let written = stream
                .write_all(&encode_one_shot_response(&response))
                .await
                .is_ok();
            let flushed = stream.flush().await.is_ok();
            drain_named_pipe_to_client(&stream);
            let _ = stream.shutdown().await;
            let _ = stream.disconnect();
            if promote && written && flushed {
                let _ = promotion_tx.send(()).await;
            }
        });
    }
}

#[cfg(not(windows))]
async fn serve_windows_named_pipe_endpoint(
    _pipe: &str,
    _journal: std::sync::Arc<acosmi_memory_journal::Journal>,
) -> Result<ServeExit> {
    Err(anyhow!(
        "memory orchestrator named pipe server is only available on Windows"
    ))
}

#[cfg(windows)]
async fn ping_windows_named_pipe_endpoint(pipe: &str) -> Result<serde_json::Value> {
    use tokio::io::AsyncWriteExt;
    use tokio::net::windows::named_pipe::ClientOptions;

    let mut stream = ClientOptions::new().open(pipe)?;
    // K1 request-side mirror: the trailing `\n` IS the frame end — named
    // pipes have no half-close (`shutdown()` is a no-op), so without it the
    // server's `read_until(b'\n')` and this client's response wait deadlock
    // deterministically. Response reading is parse-on-arrival for the same
    // reason (no clean EOF exists on this transport).
    stream.write_all(PING_REQUEST_FRAME).await?;
    stream.flush().await?;
    read_one_shot_response(stream).await
}

#[cfg(not(windows))]
async fn ping_windows_named_pipe_endpoint(_pipe: &str) -> Result<serde_json::Value> {
    Err(anyhow!(
        "memory orchestrator named pipe ping is only available on Windows"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_unix_endpoint() {
        assert_eq!(
            parse_endpoint("unix:/tmp/crabcode-memory-7.sock").expect("parse"),
            MemoryEndpoint::Unix("/tmp/crabcode-memory-7.sock".to_string())
        );
    }

    #[test]
    fn parses_reserved_windows_named_pipe_endpoint() {
        assert_eq!(
            parse_endpoint(r"npipe:\\.\pipe\crabcode-memory-7").expect("parse"),
            MemoryEndpoint::WindowsNamedPipe(r"\\.\pipe\crabcode-memory-7".to_string())
        );
    }

    // K1 (W-MEMORY-LIFECYCLE): one-shot responses are NDJSON frames — the
    // encoded bytes MUST end with exactly one `\n` so the TS client's
    // newline-delimited parser completes the frame on arrival (Windows named
    // pipes deliver no clean EOF; without the `\n` the buffered response is
    // discarded together with the disconnect EPIPE).
    #[test]
    fn one_shot_response_is_a_single_newline_terminated_ndjson_frame() {
        let response = serde_json::json!({ "ok": true, "value": 7 });
        let bytes = encode_one_shot_response(&response);

        assert_eq!(bytes.last(), Some(&b'\n'), "frame must end with \\n");
        assert!(
            !bytes[..bytes.len() - 1].contains(&b'\n'),
            "exactly one trailing newline — the JSON body itself is single-line"
        );

        // The frame round-trips: strip the newline, parse, compare.
        let parsed: serde_json::Value =
            serde_json::from_slice(&bytes[..bytes.len() - 1]).expect("frame parses");
        assert_eq!(parsed, response);
    }

    #[test]
    fn one_shot_error_response_is_newline_terminated_too() {
        // Mirrors the read-error path in both serve loops.
        let response = serde_json::json!({ "ok": false, "error": "read failed" });
        let bytes = encode_one_shot_response(&response);
        assert!(bytes.ends_with(b"\n"));
        let parsed: serde_json::Value =
            serde_json::from_slice(&bytes).expect("serde tolerates trailing whitespace");
        assert_eq!(parsed["ok"], false);
    }

    // K1 request-side mirror: the ping request is newline-framed so
    // `read_ipc_request` (`read_until(b'\n')`) completes the frame without a
    // half-close EOF (which Windows named pipes never deliver).
    #[test]
    fn ping_request_frame_is_newline_terminated_json() {
        assert!(PING_REQUEST_FRAME.ends_with(b"\n"));
        let parsed: serde_json::Value =
            serde_json::from_slice(PING_REQUEST_FRAME).expect("frame parses");
        assert_eq!(parsed["method"], "memory.ping");
    }

    /// Mock transport: yields `data` on the first read, then either EOF or a
    /// broken-pipe error — models a Windows named-pipe client whose server has
    /// already `disconnect()`ed with the response buffered.
    struct DataThen {
        data: Option<Vec<u8>>,
        then_broken_pipe: bool,
    }

    impl tokio::io::AsyncRead for DataThen {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if let Some(data) = self.data.take() {
                buf.put_slice(&data);
                return std::task::Poll::Ready(Ok(()));
            }
            if self.then_broken_pipe {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "EPIPE: broken pipe",
                )));
            }
            std::task::Poll::Ready(Ok(())) // EOF
        }
    }

    #[tokio::test]
    async fn one_shot_response_parses_on_arrival_of_newline_frame() {
        // The complete NDJSON frame resolves without any EOF at all.
        let transport = DataThen {
            data: Some(b"{\"ok\":true,\"pong\":1}\n".to_vec()),
            then_broken_pipe: true, // even a pending EPIPE never gets hit
        };
        let value = read_one_shot_response(transport).await.expect("parses");
        assert_eq!(value["pong"], 1);
    }

    #[tokio::test]
    async fn one_shot_response_tolerates_epipe_with_buffered_data() {
        // Windows pathology: no trailing newline from an older server + the
        // disconnect EPIPE — the buffered response must still parse (mirrors
        // app-server's try_decode_memory_ipc_response).
        let transport = DataThen {
            data: Some(b"{\"ok\":true,\"pong\":2}".to_vec()),
            then_broken_pipe: true,
        };
        let value = read_one_shot_response(transport)
            .await
            .expect("buffered response survives the EPIPE");
        assert_eq!(value["pong"], 2);

        // But a garbage buffer + EPIPE still surfaces the transport error.
        let transport = DataThen {
            data: Some(b"not json".to_vec()),
            then_broken_pipe: true,
        };
        assert!(read_one_shot_response(transport).await.is_err());
    }

    #[tokio::test]
    async fn one_shot_response_parses_unterminated_frame_on_clean_eof() {
        // Unix belt: an older server without the trailing `\n` still works via
        // the shutdown()-driven EOF.
        let transport = DataThen {
            data: Some(b"{\"ok\":true,\"pong\":3}".to_vec()),
            then_broken_pipe: false,
        };
        let value = read_one_shot_response(transport)
            .await
            .expect("EOF path parses");
        assert_eq!(value["pong"], 3);
    }

    #[cfg(unix)]
    fn private_short_socket_path() -> std::path::PathBuf {
        use std::os::unix::fs::DirBuilderExt as _;

        let parent = std::path::Path::new("/tmp")
            .join(format!("crabcode-memory-{}", uuid::Uuid::new_v4().simple()));
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(&parent).expect("private short parent");
        parent.join("memory-orchestrator.sock")
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_cleanup_removes_its_empty_private_short_parent() {
        let socket_path = private_short_socket_path();
        let parent = socket_path.parent().expect("socket parent").to_path_buf();
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind private socket");
        let owner = BoundUnixSocketPath::capture(&socket_path).expect("capture socket identity");

        drop(owner);

        assert!(!socket_path.exists(), "owned socket must be removed");
        assert!(!parent.exists(), "empty owned short parent must be removed");
        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_cleanup_preserves_a_nonempty_private_short_parent() {
        let socket_path = private_short_socket_path();
        let parent = socket_path.parent().expect("socket parent").to_path_buf();
        let sentinel = parent.join("successor-owned");
        std::fs::write(&sentinel, b"keep").expect("sentinel");
        let listener =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind private socket");
        let owner = BoundUnixSocketPath::capture(&socket_path).expect("capture socket identity");

        drop(owner);

        assert!(!socket_path.exists(), "owned socket must be removed");
        assert!(sentinel.exists(), "unrelated parent content must survive");
        drop(listener);
        std::fs::remove_file(sentinel).expect("remove sentinel");
        std::fs::remove_dir(parent).expect("remove private parent");
    }

    #[cfg(unix)]
    #[test]
    fn unix_socket_cleanup_preserves_a_replacement_inode() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let socket_path = temp_dir.path().join("memory.sock");
        let original = std::os::unix::net::UnixListener::bind(&socket_path).expect("bind original");
        let owner = BoundUnixSocketPath::capture(&socket_path).expect("capture original identity");

        std::fs::remove_file(&socket_path).expect("unlink original pathname");
        let replacement =
            std::os::unix::net::UnixListener::bind(&socket_path).expect("bind replacement");
        drop(owner);

        assert!(
            socket_path.exists(),
            "the old owner must not delete a replacement socket"
        );
        drop(replacement);
        drop(original);
        std::fs::remove_file(&socket_path).expect("cleanup replacement pathname");
    }

    #[cfg(unix)]
    async fn send_unix_request(
        socket_path: &std::path::Path,
        request: &serde_json::Value,
    ) -> serde_json::Value {
        use tokio::io::AsyncWriteExt as _;

        let mut stream = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match tokio::net::UnixStream::connect(socket_path).await {
                    Ok(stream) => break stream,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) =>
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(error) => panic!("connect to Memory endpoint: {error}"),
                }
            }
        })
        .await
        .expect("Memory endpoint must become connectable within 2s");

        let mut frame = serde_json::to_vec(request).expect("encode request");
        frame.push(b'\n');
        stream.write_all(&frame).await.expect("write request");
        stream.flush().await.expect("flush request");
        read_one_shot_response(stream)
            .await
            .expect("one-shot response")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_server_owner_bound_promotion_fails_closed_then_releases_after_exact_ack() {
        let temp_dir = tempfile::TempDir::new().expect("temp dir");
        let socket_path = temp_dir.path().join("memory.sock");
        let endpoint = format!("unix:{}", socket_path.display());
        let server_endpoint = endpoint.clone();
        let journal = std::sync::Arc::new(
            acosmi_memory_journal::Journal::open(temp_dir.path().join("journal.sqlite3"))
                .expect("journal"),
        );
        let server = tokio::spawn(async move {
            serve_endpoint_with_journal(&server_endpoint, journal)
                .await
                .expect("serve")
        });

        let ping = send_unix_request(
            &socket_path,
            &serde_json::json!({ "method": "memory.ping" }),
        )
        .await;
        assert_eq!(ping["ok"], true);
        assert!(ping["capabilities"]
            .as_array()
            .is_some_and(|capabilities| capabilities.iter().any(|capability| {
                capability.as_str() == Some("coordinator-promote-owner-bind-v2")
            })));
        let current_build_id = ping["build_id"].as_str().expect("ping build id");
        let current_pid = ping["pid"].as_u64().expect("ping pid");
        let current_major = current_build_id
            .split_once('+')
            .expect("build id includes authority suffix")
            .0
            .split('.')
            .next()
            .expect("build id includes major version")
            .parse::<u64>()
            .expect("major version is numeric");
        let successor_build_id = format!("{}.0.0+promotion-test", current_major + 1);
        let valid_payload = serde_json::json!({
                "expected_current_build_id": current_build_id,
                "expected_current_pid": current_pid,
                "successor_build_id": successor_build_id,
                "protocol_version": MEMORY_PROTOCOL_VERSION,
                "schema_id": MEMORY_SCHEMA_ID,
        });

        let wrong_pid = if current_pid == u64::MAX {
            current_pid - 1
        } else {
            current_pid + 1
        };
        let raw_same_version_build_id = format!(
            "{}+raw-same-version",
            current_build_id
                .split_once('+')
                .expect("build id has metadata")
                .0
        );
        let mut invalid_payloads = Vec::new();
        let mut wrong_build_payload = valid_payload.clone();
        wrong_build_payload["expected_current_build_id"] = serde_json::json!("0.0.0+wrong-owner");
        invalid_payloads.push(("wrong current build", wrong_build_payload));
        let mut wrong_pid_payload = valid_payload.clone();
        wrong_pid_payload["expected_current_pid"] = serde_json::json!(wrong_pid);
        invalid_payloads.push(("wrong current pid", wrong_pid_payload));
        let mut wrong_protocol_payload = valid_payload.clone();
        wrong_protocol_payload["protocol_version"] = serde_json::json!(MEMORY_PROTOCOL_VERSION + 1);
        invalid_payloads.push(("wrong protocol", wrong_protocol_payload));
        let mut wrong_schema_payload = valid_payload.clone();
        wrong_schema_payload["schema_id"] = serde_json::json!("wrong-memory-schema");
        invalid_payloads.push(("wrong schema", wrong_schema_payload));
        let mut same_version_payload = valid_payload.clone();
        same_version_payload["successor_build_id"] = serde_json::json!(raw_same_version_build_id);
        invalid_payloads.push(("raw same-version successor", same_version_payload));

        for (case, payload) in invalid_payloads {
            let response = send_unix_request(
                &socket_path,
                &serde_json::json!({
                    "method": "memory.coordinator.promote",
                    "payload": payload,
                }),
            )
            .await;
            assert_eq!(response["ok"], false, "{case}: {response}");
            assert_ne!(response["promote"], true, "{case}: {response}");
        }

        let response = send_unix_request(
            &socket_path,
            &serde_json::json!({
                "method": "memory.coordinator.promote",
                "payload": valid_payload,
            }),
        )
        .await;
        assert_eq!(response["ok"], true);
        assert_eq!(response["promote"], true);
        assert_eq!(response["current_build_id"], current_build_id);
        assert_eq!(response["current_pid"], current_pid);
        assert_eq!(response["successor_build_id"], successor_build_id);
        assert_eq!(response["protocol_version"], MEMORY_PROTOCOL_VERSION);
        assert_eq!(response["schema_id"], MEMORY_SCHEMA_ID);

        let exit = tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .expect("server releases endpoint after acknowledged promotion")
            .expect("server task joins");
        assert_eq!(exit, ServeExit::Promoted);
        assert!(
            !socket_path.exists(),
            "acknowledged promotion must remove the owned Unix socket pathname"
        );
    }
}

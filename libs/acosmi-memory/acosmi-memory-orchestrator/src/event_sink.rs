//! W-MEMORY-EVOLUTION PR-1 (2026-05-29) — events 长连传输地基。
//!
//! # Purpose
//!
//! Orchestrator 当前 UDS 服务是「收一条请求 → 答一条 → shutdown 关连接」
//! （`lib.rs serve_unix_endpoint`），物理上无法主动 push 通知给 TUI client。
//! 这是 B1 阻塞。`EventSink` 持有一组**长连订阅者**，让 orchestrator 能在任意
//! 时刻把通知帧 push 给所有订阅者（TUI client 侧的 `memory_events_pump` 收到后
//! 广播给 TS 客户端）。`EventSink` 作为 `Arc<Self>` 共享给每个 Tier processor +
//! SE，故 `push_frame` 会被多个并发任务调用。
//!
//! # Frame protocol
//!
//! 每帧是一行 NDJSON（`serde_json::to_vec(frame) + b'\n'`）。订阅者按行读。
//!
//! # 并发 / 慢订阅者隔离（W-MEMORY-EVOLUTION FIX #9, 2026-06-01）
//!
//! 历史实现把订阅者写半端放进 `Mutex<Vec<OwnedWriteHalf>>`，`push_frame` 持锁
//! **跨 socket `write_all/flush().await`** 逐个写。这有两个问题：
//!   1. 慢订阅者头阻塞——持锁期间任何 `push_frame` / `register` 全部排队；一个
//!      **卡死**（永不读）的订阅者会让 `write_all` 永远挂起，死锁所有 push。
//!   2. 「take 快照 → 释放锁 → 写 → 写回存活者」的变体仍有缺陷：并发 push 会看到
//!      空列表丢帧，且卡死订阅者会让快照永不写回（永久丢失写半端）。
//!
//! 本实现用**每订阅者一条 bounded channel + 专属写任务**（计划 §C1 #9 的稳健
//! 备选）：`register` 为订阅者起一个 task 持有 socket 写半端，从 channel 取帧落
//! 盘；`EventSink` 只持 `Vec<mpsc::Sender<Arc<[u8]>>>`。`push_frame` 对每个 sender
//! 做**非阻塞 `try_send`**——满（慢订阅者）即对该订阅者丢这一帧（背压），关闭
//! （写任务因落盘失败退出 = 死订阅者）即剪除。锁只在极短的 `try_send` 循环期间
//! 持有、**绝不跨 socket IO**，故慢/卡死订阅者只影响自己，绝不阻塞其他订阅者或
//! 其他 push。

#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
#[cfg(unix)]
use tokio::net::unix::OwnedWriteHalf;
#[cfg(unix)]
use tokio::sync::{mpsc, Mutex};

/// Per-subscriber outbound queue depth. Generous so transient bursts never drop
/// frames; if a subscriber is so slow it backs up this far, dropping its
/// further frames (backpressure) is correct — far better than blocking every
/// other subscriber and every push.
#[cfg(any(unix, windows))]
const SUBSCRIBER_CHANNEL_CAP: usize = 256;

/// 持有所有 events 长连订阅者的发送端。每个订阅者由一个专属写任务持有 socket
/// 写半端，从其 channel 取帧落盘。`Arc<EventSink>` 共享给 accept loop 各连接
/// （register）+ 各 Tier/SE 任务（push_frame）。
#[cfg(unix)]
pub struct EventSink {
    senders: Mutex<Vec<mpsc::Sender<Arc<[u8]>>>>,
}

#[cfg(unix)]
impl EventSink {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            senders: Mutex::new(Vec::new()),
        })
    }

    /// 把一个新订阅者的写半端接入：起一个专属写任务持有它，从 bounded channel
    /// 取帧逐行落盘；首次写失败（订阅者断开）即退出（丢弃 receiver，使对应
    /// sender `is_closed()` → 下次 `push_frame` 剪除）。
    pub async fn register(&self, mut writer: OwnedWriteHalf) {
        let (tx, mut rx) = mpsc::channel::<Arc<[u8]>>(SUBSCRIBER_CHANNEL_CAP);
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
        });
        self.senders.lock().await.push(tx);
    }

    /// 当前存活订阅者数量（测试 / 诊断用）。顺带剪除写任务已退出的死订阅者。
    pub async fn subscriber_count(&self) -> usize {
        let mut senders = self.senders.lock().await;
        senders.retain(|tx| !tx.is_closed());
        senders.len()
    }

    /// 把 `frame` 序列化为一行 NDJSON 非阻塞地分发给所有订阅者。锁只在 `try_send`
    /// 循环期间短暂持有，绝不跨 socket IO。
    pub async fn push_frame(&self, frame: &serde_json::Value) {
        let bytes = match serialize_frame(frame) {
            Some(b) => b,
            None => return,
        };
        let mut senders = self.senders.lock().await;
        if senders.is_empty() {
            return;
        }
        senders.retain(|tx| match tx.try_send(Arc::clone(&bytes)) {
            // Queued for the subscriber's writer task.
            Ok(()) => true,
            // Slow subscriber: drop THIS frame for it (backpressure), keep it.
            Err(mpsc::error::TrySendError::Full(_)) => true,
            // Writer task exited (subscriber gone): prune it.
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }
}

/// Serialize a frame to a single NDJSON line as a cheaply-cloneable `Arc<[u8]>`.
/// Returns `None` (logged) on serialization failure — fail-soft.
#[cfg(any(unix, windows))]
fn serialize_frame(frame: &serde_json::Value) -> Option<std::sync::Arc<[u8]>> {
    match serde_json::to_vec(frame) {
        Ok(mut b) => {
            b.push(b'\n');
            Some(std::sync::Arc::from(b.into_boxed_slice()))
        }
        Err(err) => {
            log::warn!("event_sink: failed to serialize frame; dropping: {err}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Windows: Named Pipe events push 真实现（W-MEMORY-EVOLUTION W11 PR-5）。
//
// 与 Unix 版**同形**：Unix 订阅者写任务持 `OwnedWriteHalf`（来自
// `UnixStream::into_split`），Windows 持 `WriteHalf<NamedPipeServer>`（来自
// `tokio::io::split(NamedPipeServer)`）。`register` / `subscriber_count` /
// `push_frame` 的 API 与 Unix 完全一致，故 `lib.rs` 的 events-subscribe 分支可
// 平台无关调用。每订阅者一条 bounded channel + 专属写任务的隔离语义逐字等价
// （仅 writer 具体类型分平台）。
//
// 注：本块全部 `#[cfg(windows)]`；去掉它们后上面的 Unix `EventSink` 逐字不变。
// ---------------------------------------------------------------------------

#[cfg(windows)]
use std::sync::Arc;
#[cfg(windows)]
use tokio::io::AsyncWriteExt as WindowsAsyncWriteExt;
#[cfg(windows)]
use tokio::io::WriteHalf;
#[cfg(windows)]
use tokio::net::windows::named_pipe::NamedPipeServer;
#[cfg(windows)]
use tokio::sync::{mpsc, Mutex as WindowsMutex};

/// 持有所有 events 长连订阅者的发送端（Windows Named Pipe）。语义与 Unix 版
/// 一致：每订阅者一个专属写任务 + bounded channel。
#[cfg(windows)]
pub struct EventSink {
    senders: WindowsMutex<Vec<mpsc::Sender<Arc<[u8]>>>>,
}

#[cfg(windows)]
impl EventSink {
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            senders: WindowsMutex::new(Vec::new()),
        })
    }

    /// 把一个新订阅者的写半端接入（与 Unix `register` 语义一致）。
    pub async fn register(&self, mut writer: WriteHalf<NamedPipeServer>) {
        let (tx, mut rx) = mpsc::channel::<Arc<[u8]>>(SUBSCRIBER_CHANNEL_CAP);
        tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
        });
        self.senders.lock().await.push(tx);
    }

    /// 当前存活订阅者数量（与 Unix `subscriber_count` 语义一致）。
    pub async fn subscriber_count(&self) -> usize {
        let mut senders = self.senders.lock().await;
        senders.retain(|tx| !tx.is_closed());
        senders.len()
    }

    /// 非阻塞分发一帧（与 Unix `push_frame` 语义一致）。
    pub async fn push_frame(&self, frame: &serde_json::Value) {
        let bytes = match serialize_frame(frame) {
            Some(b) => b,
            None => return,
        };
        let mut senders = self.senders.lock().await;
        if senders.is_empty() {
            return;
        }
        senders.retain(|tx| match tx.try_send(Arc::clone(&bytes)) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => true,
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        });
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::UnixStream;

    #[tokio::test]
    async fn push_frame_delivers_ndjson_to_subscriber() {
        let sink = EventSink::new();
        let (client, server) = UnixStream::pair().expect("pair");
        let (_server_read, server_write) = server.into_split();
        sink.register(server_write).await;
        assert_eq!(sink.subscriber_count().await, 1);

        sink.push_frame(&serde_json::json!({"notification": "x", "n": 1}))
            .await;

        // The per-subscriber writer task writes asynchronously; read_line awaits
        // the data, so this is deterministic.
        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read line");
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
        assert_eq!(parsed["notification"], "x");
        assert_eq!(parsed["n"], 1);
    }

    #[tokio::test]
    async fn dead_subscriber_is_pruned_on_push() {
        let sink = EventSink::new();
        let (client, server) = UnixStream::pair().expect("pair");
        let (_server_read, server_write) = server.into_split();
        sink.register(server_write).await;
        assert_eq!(sink.subscriber_count().await, 1);

        // Drop the client → the subscriber's writer task fails on its next write
        // (EPIPE) and exits, dropping its receiver. push_frame then sees the
        // sender closed and prunes it. Poll (bounded) until convergence — the
        // writer task fails promptly once a frame reaches it.
        drop(client);
        let mut pruned = false;
        for _ in 0..200 {
            sink.push_frame(&serde_json::json!({"notification": "y"}))
                .await;
            if sink.subscriber_count().await == 0 {
                pruned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            pruned,
            "dead subscriber must be pruned after its write fails"
        );
    }

    #[tokio::test]
    async fn slow_subscriber_does_not_block_other_pushes() {
        // A subscriber that never reads must not block push_frame. With a
        // bounded channel + non-blocking try_send, push_frame returns promptly
        // regardless; the slow subscriber's channel simply fills and drops.
        let sink = EventSink::new();
        let (_never_read_client, server) = UnixStream::pair().expect("pair");
        let (_sr, server_write) = server.into_split();
        sink.register(server_write).await;

        // Push far more frames than the channel cap; each push must return
        // quickly (no head-of-line block) even though the subscriber never drains.
        let pushes = async {
            for i in 0..(SUBSCRIBER_CHANNEL_CAP + 50) {
                sink.push_frame(&serde_json::json!({"notification": "z", "i": i}))
                    .await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), pushes)
            .await
            .expect("push_frame must never block on a slow subscriber");
    }
}

// W-MEMORY-EVOLUTION W11 PR-5 (2026-05-29) — Windows Named Pipe `EventSink`
// round-trip tests. Mirror the `#[cfg(all(test, unix))]` tests above but use a
// `NamedPipeServer` (split via `tokio::io::split`) as the registered writer and
// a `ClientOptions` client as the reader. Type-checked everywhere via
// `cargo check --target x86_64-pc-windows-msvc --tests`; only *run* on Windows
// CI (Named Pipes are Windows kernel objects with no host equivalent on this
// macOS / Linux dev box).
#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::windows::named_pipe::{ClientOptions, ServerOptions};

    fn unique_pipe_name(tag: &str) -> String {
        format!(
            r"\\.\pipe\crabcode-memory-eventsink-test-{tag}-{}",
            std::process::id()
        )
    }

    #[tokio::test]
    async fn push_frame_delivers_ndjson_to_subscriber() {
        let pipe_name = unique_pipe_name("deliver");
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .expect("create server");
        let client = ClientOptions::new().open(&pipe_name).expect("open client");
        server.connect().await.expect("connect");

        let sink = EventSink::new();
        let (_server_read, server_write) = tokio::io::split(server);
        sink.register(server_write).await;
        assert_eq!(sink.subscriber_count().await, 1);

        sink.push_frame(&serde_json::json!({"notification": "x", "n": 1}))
            .await;

        let mut reader = BufReader::new(client);
        let mut line = String::new();
        reader.read_line(&mut line).await.expect("read line");
        let parsed: serde_json::Value = serde_json::from_str(line.trim()).expect("parse");
        assert_eq!(parsed["notification"], "x");
        assert_eq!(parsed["n"], 1);
    }

    #[tokio::test]
    async fn dead_subscriber_is_pruned_on_push() {
        let pipe_name = unique_pipe_name("prune");
        let server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .expect("create server");
        let client = ClientOptions::new().open(&pipe_name).expect("open client");
        server.connect().await.expect("connect");

        let sink = EventSink::new();
        let (_server_read, server_write) = tokio::io::split(server);
        sink.register(server_write).await;
        assert_eq!(sink.subscriber_count().await, 1);

        // Drop the client → the subscriber's writer task fails and exits; poll
        // (bounded) until push_frame prunes the closed sender.
        drop(client);
        let mut pruned = false;
        for _ in 0..200 {
            sink.push_frame(&serde_json::json!({"notification": "y"}))
                .await;
            if sink.subscriber_count().await == 0 {
                pruned = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            pruned,
            "dead subscriber must be pruned after its write fails"
        );
    }
}

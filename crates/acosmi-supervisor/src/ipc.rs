//! IPC 层 — stdio JSON-RPC (Rust↔TS) + UDS/Named Pipes (Rust↔Go)
//!
//! 本模块实现 supervisor 与子进程之间的进程间通信：
//! - [`NdjsonCodec`]: 基于 `tokio_util::codec` 的 NDJSON 帧协议编解码器
//! - [`IpcServer`]: 监听来自 Go 的 UDS/Named Pipe 连接
//! - [`StdioBridge`]: 通过子进程 stdin/stdout 与 TS 通信
//! - [`handle_capability_request`]: Go → Rust 能力申请路由
//! - [`IpcSignal`]: IPC 通道向主循环传递的信号（心跳 + 健康状态）

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};
use tokio_util::sync::CancellationToken;

use acosmi_executor::CommandExecutor;
use acosmi_permission::{
    PermissionBehavior, PermissionContext, PermissionMode, PermissionResult, PermissionRule,
    PermissionRuleSource, PermissionRuleValue, check_bash_permission,
};
use acosmi_types::exec_types::{CommandFamily, ExecReq};
use acosmi_types::protocol::{CapabilityFamily, CapabilityReq, CapabilityResp, CapabilityStatus};

// ── IPC 信号类型 ───────────────────────────────────────────────────────

/// IPC 通道向主循环传递的信号
///
/// 覆盖心跳（Go/TS）、TS 降级进入/退出、TS 不响应四类事件。
///
/// Go 侧健康检查结果附带在 Go→Rust heartbeat 的 `ts_status` 字段
/// （alive/degraded/unresponsive/dead），由 `handle_connection` 转换为本 enum
/// 的信号上报主循环。
///
/// 根因修（2026-04-23）：原来 IPC 层仅支持 alive 字面值；现与 `LivenessState`
/// 四态（alive / degraded / unresponsive / dead）对齐，跨层语义一致。
/// `TsAlive` 专门用于让 Rust 在 TS 从 Degraded 恢复时 `clear_degraded`
/// （普通 Heartbeat(TS) 不会清除 Degraded 状态，必须显式退出路径）。
#[derive(Debug)]
pub enum IpcSignal {
    /// 子进程心跳（Go 通过 IPC 主动上报 / TS 通过 Go piggyback 中继）
    Heartbeat(acosmi_heartbeat::ProcessKind),
    /// TS 健康（来自 Go piggyback `ts_status="alive`"）
    /// `主循环收到后：record_heartbeat` + `clear_degraded（从` Degraded 恢复到 Alive 的唯一路径）
    TsAlive,
    /// Go 报告 TS 运行中但性能降级（内存压力 / event-loop 延迟），tracker 升级到 Degraded
    TsDegraded,
    /// Go 报告 TS 完全无响应（ping 超时或 event-loop 死锁），触发立即 kill-restart
    TsUnresponsive,
}

/// `derive_ts_signal` 解码后的目标信号（不直接发，由调用方转成 `IpcSignal`）。
///
/// 见 `ipc.rs` 内调用点的映射表注释 — 既兼容 pre-PerConnState 4 态枚举，
/// 也兼容 2026-04-28 多 conn 聚合 (`K/N_alive` / `no_ts_connected`)。
#[derive(Debug, PartialEq, Eq)]
enum TsStatusDerived {
    Alive,
    Degraded,
    Unresponsive,
    Dead,
    Unknown,
}

/// 把 Go 上报的 `ts_status` 字面量解码为 supervisor 关心的四态 + Unknown。
///
/// 兼容两套格式：
/// - 单 conn 4 态：`alive` / `degraded` / `unresponsive` / `dead`
/// - 多 conn 聚合：`no_ts_connected` / `K/N_alive`
///   - K==N (含 1/1) → Alive
/// - K==0 N>0 → Dead（即 IpcSignal::TsUnresponsive，立即 kill-restart）
///   - 0<K<N → Degraded
///   - `no_ts_connected` → Alive（boot 期/收尾期乐观处理，与 pre-reform
///     ChannelHealth 初始 LivenessAlive 默认一致；Rust 自身的启动超时检测
///     是 TS 进程根本没存活的兜底）
fn derive_ts_signal(ts_status: &str) -> TsStatusDerived {
    match ts_status {
        "alive" => TsStatusDerived::Alive,
        "degraded" => TsStatusDerived::Degraded,
        "unresponsive" => TsStatusDerived::Unresponsive,
        "dead" => TsStatusDerived::Dead,
        "no_ts_connected" => TsStatusDerived::Alive,
        s => parse_kn_alive(s).unwrap_or(TsStatusDerived::Unknown),
    }
}

/// 解析 `K/N_alive` 格式（如 `1/1_alive`、`2/3_alive`、`0/2_alive`）。
/// 不匹配返回 None。
fn parse_kn_alive(s: &str) -> Option<TsStatusDerived> {
    let stripped = s.strip_suffix("_alive")?;
    let (k_str, n_str) = stripped.split_once('/')?;
    let k: u32 = k_str.parse().ok()?;
    let n: u32 = n_str.parse().ok()?;
    if n == 0 {
        return None;
    }
    Some(if k == n {
        TsStatusDerived::Alive
    } else if k == 0 {
        TsStatusDerived::Dead
    } else {
        TsStatusDerived::Degraded
    })
}

#[cfg(test)]
mod ts_status_tests {
    use super::*;

    #[test]
    fn legacy_4_states() {
        assert_eq!(derive_ts_signal("alive"), TsStatusDerived::Alive);
        assert_eq!(derive_ts_signal("degraded"), TsStatusDerived::Degraded);
        assert_eq!(
            derive_ts_signal("unresponsive"),
            TsStatusDerived::Unresponsive
        );
        assert_eq!(derive_ts_signal("dead"), TsStatusDerived::Dead);
    }

    #[test]
    fn aggregate_no_ts_optimistic() {
        assert_eq!(derive_ts_signal("no_ts_connected"), TsStatusDerived::Alive);
    }

    #[test]
    fn aggregate_all_alive() {
        assert_eq!(derive_ts_signal("1/1_alive"), TsStatusDerived::Alive);
        assert_eq!(derive_ts_signal("3/3_alive"), TsStatusDerived::Alive);
    }

    #[test]
    fn aggregate_all_dead() {
        assert_eq!(derive_ts_signal("0/2_alive"), TsStatusDerived::Dead);
        assert_eq!(derive_ts_signal("0/1_alive"), TsStatusDerived::Dead);
    }

    #[test]
    fn aggregate_partial_degraded() {
        assert_eq!(derive_ts_signal("1/2_alive"), TsStatusDerived::Degraded);
        assert_eq!(derive_ts_signal("2/3_alive"), TsStatusDerived::Degraded);
    }

    #[test]
    fn aggregate_malformed_unknown() {
        assert_eq!(derive_ts_signal("0/0_alive"), TsStatusDerived::Unknown);
        assert_eq!(derive_ts_signal("garbage"), TsStatusDerived::Unknown);
        assert_eq!(derive_ts_signal("1/2_dead"), TsStatusDerived::Unknown);
        assert_eq!(derive_ts_signal(""), TsStatusDerived::Unknown);
    }
}

// ── 错误类型 ────────────────────────────────────────────────────────────

/// IPC 层错误类型
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// IO 错误
    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    /// JSON 序列化/反序列化错误
    #[error("JSON 序列化错误: {0}")]
    Json(#[from] serde_json::Error),

    /// 协议级错误（格式不合法、消息类型不认识等）
    #[error("协议错误: {0}")]
    Protocol(String),

    /// 版本不兼容
    #[error("版本不兼容: 本地={local}, 远程={remote}")]
    VersionMismatch { local: String, remote: String },

    /// 连接已关闭
    #[error("连接已关闭")]
    ConnectionClosed,

    /// P1-2: 握手认证失败（peer-uid 不匹配 / 密钥缺失或错误）。
    #[error("IPC 认证失败: {0}")]
    AuthDenied(String),
}

// ── NDJSON 编解码器 ─────────────────────────────────────────────────────

/// NDJSON 单帧最大字节数（16 MiB），超过此限制的消息将被拒绝以防止 OOM
const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

/// Newline-Delimited JSON 编解码器
///
/// 以 `'\n'` 作为帧分隔符，每行解析为一个 [`serde_json::Value`]。
/// 用于 `tokio_util::codec` 框架，可直接与 `Framed` 组合使用。
/// 单帧最大 [`MAX_FRAME_SIZE`] 字节，超过时返回错误。
pub struct NdjsonCodec;

impl Decoder for NdjsonCodec {
    type Item = serde_json::Value;
    type Error = IpcError;

    /// 从字节缓冲区中尝试解码一条 NDJSON 消息
    ///
    /// 查找第一个 `'\n'` 字符，将其之前的字节作为一个 JSON 消息解析。
    /// 空行被静默跳过（返回 `Ok(None)`）。
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // 查找换行符
        let newline_pos = src.iter().position(|&b| b == b'\n');
        let Some(pos) = newline_pos else {
            // 没有完整的一行；检查是否超过帧大小限制（OOM 防护）
            if src.len() > MAX_FRAME_SIZE {
                return Err(IpcError::Protocol(format!(
                    "NDJSON 帧超过最大限制（{} bytes > {MAX_FRAME_SIZE} bytes），缓冲区中未找到换行符",
                    src.len()
                )));
            }
            return Ok(None);
        };

        // 取出这一行（不含 '\n'）
        let line = src.split_to(pos);
        // 消费 '\n' 本身
        let _ = src.split_to(1);

        // 去除可能的 '\r'（Windows 换行兼容）
        let line_bytes = if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            &line[..]
        };

        // 跳过空行（NDJSON 规范允许空行作为 keep-alive）
        if line_bytes.is_empty() {
            tracing::trace!("NDJSON 解码器跳过空行");
            return Ok(None);
        }

        // 解析 JSON
        let value: serde_json::Value = serde_json::from_slice(line_bytes)?;
        Ok(Some(value))
    }
}

impl Encoder<serde_json::Value> for NdjsonCodec {
    type Error = IpcError;

    /// 将 JSON 值编码为 NDJSON 帧（JSON + `'\n'`）
    fn encode(&mut self, item: serde_json::Value, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let json_bytes = serde_json::to_vec(&item)?;
        dst.reserve(json_bytes.len() + 1);
        dst.extend_from_slice(&json_bytes);
        dst.extend_from_slice(b"\n");
        Ok(())
    }
}

// ── Framed 编解码器（rust_go 通道专用） ─────────────────────────────────

/// Framed 单帧最大字节数（16 MiB），与 Go 端 `FramedCodec` 对齐
const MAX_FRAMED_SIZE: u32 = 16 * 1024 * 1024;

/// 4 字节 Big-Endian 长度前缀 + JSON 编解码器
///
/// 与原 Go 端 `services/internal/ipc/codec.go` 的 `FramedCodec` 线格式完全一致
/// (M6 已归档至 archive/2026-05-02-go-services/internal/ipc/codec.go)：
///
/// ```text
/// +------------------+----------------------------+
/// |   length (4B)    |     JSON payload (N bytes) |
/// |  big-endian u32  |     UTF-8 encoded           |
/// +------------------+----------------------------+
/// ```
///
/// 用于 `tokio_util::codec::Framed`，适配 Rust↔Go UDS/Named Pipe 链路。
pub struct FramedJsonCodec;

impl Decoder for FramedJsonCodec {
    type Item = serde_json::Value;
    type Error = IpcError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // 需要至少 4 字节读取长度前缀
        if src.len() < 4 {
            return Ok(None);
        }

        // 读取长度前缀（不消费）
        let length = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);

        if length > MAX_FRAMED_SIZE {
            return Err(IpcError::Protocol(format!(
                "Framed 帧超过最大限制（{length} bytes > {MAX_FRAMED_SIZE} bytes）"
            )));
        }

        let total = 4 + length as usize;
        if src.len() < total {
            // 数据不完整，等待更多数据
            src.reserve(total - src.len());
            return Ok(None);
        }

        // 消费 4 字节长度前缀
        let _ = src.split_to(4);
        // 消费 payload
        let payload = src.split_to(length as usize);

        let value: serde_json::Value = serde_json::from_slice(&payload)?;
        Ok(Some(value))
    }
}

impl Encoder<serde_json::Value> for FramedJsonCodec {
    type Error = IpcError;

    fn encode(&mut self, item: serde_json::Value, dst: &mut BytesMut) -> Result<(), Self::Error> {
        let json_bytes = serde_json::to_vec(&item)?;
        let length = json_bytes.len() as u32;

        if length > MAX_FRAMED_SIZE {
            return Err(IpcError::Protocol(format!(
                "Framed 帧超过最大限制（{length} bytes > {MAX_FRAMED_SIZE} bytes）"
            )));
        }

        dst.reserve(4 + json_bytes.len());
        dst.extend_from_slice(&length.to_be_bytes());
        dst.extend_from_slice(&json_bytes);
        Ok(())
    }
}

// ── IPC 服务器配置 ──────────────────────────────────────────────────────

/// IPC 服务器配置
#[derive(Debug, Clone)]
pub struct IpcConfig {
    /// Unix Domain Socket 路径（仅 Unix 平台使用）
    pub uds_path: Option<PathBuf>,
    /// Windows Named Pipe 名称（仅 Windows 平台使用）
    pub pipe_name: Option<String>,
    /// 协议版本号（用于 Handshake 版本协商）
    pub protocol_version: String,
    /// 组件版本号（用于 Handshake 交换）
    pub component_version: String,
    /// P1-2 (2026-06-05): 进程级一次性握手密钥（高熵 CSPRNG）。
    ///
    /// 客户端 `version_handshake` 帧必须在 `params.auth_secret` 携带此值；
    /// 缺失或不匹配立即断连（无静默降级到未认证模式）。secret 经 env
    /// `CRABCODE_SUPERVISOR_SECRET` 与 `CRABCODE_SUPERVISOR_UDS` 同路径注入
    /// 受信子进程（见 `lib.rs` ts-session 注入点）。
    pub auth_secret: String,
}

/// P1-2: 生成进程级一次性握手密钥（≥240 bit CSPRNG 熵）。
///
/// 复用既有 `uuid` v4 依赖（getrandom CSPRNG，无需新增 crate）。两个 v4
/// UUID 各 122 bit 随机性，hex 拼接 = 64 字符 / 244 bit，足够作为一次性
/// loopback 认证 token（非长期凭据、非密码学签名密钥）。
#[must_use]
fn generate_ipc_auth_secret() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

impl Default for IpcConfig {
    fn default() -> Self {
        // P1-2: UDS 落 state_dir（honors CRABCODE_CONFIG_DIR > CRABCODE_STATE_DIR
        // > CRABCODE_HOME > homedir 的 §4 解析序），不再用 world-traversable
        // /tmp 与 pid-predictable 文件名。与 `acosmi-runtime::bootstrap::serve`
        // 的 `<state_dir>/supervisor.sock` 约定对齐（这里加 pid 子级避免同机
        // 多 supervisor 撞同一 socket）。
        let pid = std::process::id();
        let state_dir = acosmi_config::paths::resolve_state_dir();
        let uds_path = state_dir.join("supervisor").join(format!("{pid}.sock"));
        Self {
            uds_path: Some(uds_path),
            pipe_name: Some(format!(r"\\.\pipe\crabcode-{pid}")),
            protocol_version: "1.0".to_string(),
            component_version: env!("CARGO_PKG_VERSION").to_string(),
            auth_secret: generate_ipc_auth_secret(),
        }
    }
}

// ── IPC 服务器 ──────────────────────────────────────────────────────────

/// IPC 服务器：监听来自 Go 的连接
///
/// 在 Unix 上通过 UDS（Unix Domain Socket）监听连接，
/// 在 Windows 上通过 Named Pipe 监听连接。
/// 每个连接建立后先交换 Handshake，然后进入请求-响应循环。
/// 收到 Go 的心跳消息时通过 `signal_tx` 通知 supervisor 主循环。
pub struct IpcServer {
    /// 服务器配置
    config: IpcConfig,
    /// 命令执行器（共享引用）
    executor: Arc<CommandExecutor>,
    /// 关闭令牌（用于优雅关闭）
    shutdown: CancellationToken,
    /// IPC 信号发送端（心跳 + 健康告警，通知 supervisor 主循环）
    signal_tx: tokio::sync::mpsc::Sender<IpcSignal>,
    /// Supervisor 状态快照（P1.1）：`supervisor.status` RPC 返回此对象的只读 clone。
    /// None 时 RPC 返回 `schema_version=0` 哨兵，表示当前未注入 provider。
    status_provider: Option<crate::status::SharedStatus>,
}

impl IpcServer {
    /// 创建新的 IPC 服务器实例
    #[must_use]
    pub fn new(
        config: IpcConfig,
        executor: Arc<CommandExecutor>,
        shutdown: CancellationToken,
        signal_tx: tokio::sync::mpsc::Sender<IpcSignal>,
    ) -> Self {
        Self {
            config,
            executor,
            shutdown,
            signal_tx,
            status_provider: None,
        }
    }

    /// 注入 supervisor 状态快照 provider（P1.1）。
    /// 未调用时 `supervisor.status` RPC 返回 `schema_version=0` 哨兵。
    pub fn set_status_provider(&mut self, provider: crate::status::SharedStatus) {
        self.status_provider = Some(provider);
    }

    /// 启动 IPC 服务器（后台运行）
    ///
    /// 返回一个 `JoinHandle`，可用于等待服务器退出。
    /// 服务器在收到 shutdown 信号后会停止接受新连接。
    pub async fn start(&self) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let config = self.config.clone();
        let executor = Arc::clone(&self.executor);
        let shutdown = self.shutdown.clone();
        let signal_tx = self.signal_tx.clone();
        let status_provider = self.status_provider.clone();

        // 根据平台选择监听方式
        #[cfg(unix)]
        {
            self.start_unix(config, executor, shutdown, signal_tx, status_provider)
                .await
        }

        #[cfg(windows)]
        {
            self.start_windows(config, executor, shutdown, signal_tx, status_provider)
                .await
        }

        #[cfg(not(any(unix, windows)))]
        {
            let _ = signal_tx;
            let _ = status_provider;
            anyhow::bail!("不支持的平台");
        }
    }

    /// Unix 平台：启动 UDS 监听
    #[cfg(unix)]
    async fn start_unix(
        &self,
        config: IpcConfig,
        executor: Arc<CommandExecutor>,
        shutdown: CancellationToken,
        signal_tx: tokio::sync::mpsc::Sender<IpcSignal>,
        status_provider: Option<crate::status::SharedStatus>,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        use tokio::net::UnixListener;

        let uds_path = config.uds_path.clone().unwrap_or_else(|| {
            // P1-2: fallback 也落 state_dir/supervisor，不再用 /tmp。
            acosmi_config::paths::resolve_state_dir()
                .join("supervisor")
                .join(format!("{}.sock", std::process::id()))
        });

        // P1-2: 父目录 0700（仅当前用户可遍历），create_dir_all 后强制权限。
        if let Some(parent) = uds_path.parent() {
            std::fs::create_dir_all(parent)?;
            use std::os::unix::fs::PermissionsExt;
            if let Err(e) = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))
            {
                tracing::warn!(error = %e, dir = %parent.display(), "设置 supervisor socket 目录 0700 失败");
            }
        }

        // 清理旧的 socket 文件（如果存在）
        if uds_path.exists() {
            tracing::warn!(path = %uds_path.display(), "清理旧的 UDS socket 文件");
            let _ = std::fs::remove_file(&uds_path);
        }

        let listener = UnixListener::bind(&uds_path)?;
        // P1-2: bind 后立刻把 socket chmod 0600（仅 owner 读写）。
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&uds_path, std::fs::Permissions::from_mode(0o600))?;
        }
        tracing::info!(path = %uds_path.display(), "IPC 服务器启动（Unix Domain Socket，0600）");

        // JoinHandle held by caller — `handle` is returned out of
        // `start_unix` and owned by the surrounding `IpcServer`. Bare
        // `tokio::spawn` is acceptable here because the IPC server
        // task lifecycle is tracked by `IpcServer::shutdown()`.
        // Audited Step 2 Phase D.1.
        #[allow(clippy::disallowed_methods)]
        let handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    // 监听 shutdown 信号
                    () = shutdown.cancelled() => {
                        tracing::info!("IPC 服务器收到关闭信号，停止接受新连接");
                        break;
                    }
                    // 接受新连接
                    accept_result = listener.accept() => {
                        match accept_result {
                            Ok((stream, _addr)) => {
                                // P1-2: peer-uid 校验 — 仅同 uid 进程可连。
                                // 在任何 handshake 处理前拒绝跨 uid 连接。
                                if let Err(reason) = verify_peer_uid(&stream) {
                                    tracing::warn!(reason = %reason, "拒绝跨 uid 的 UDS 连接（peer-uid 校验失败）");
                                    drop(stream);
                                    continue;
                                }
                                tracing::info!("接受新的 UDS 连接");
                                let executor = Arc::clone(&executor);
                                let config = config.clone();
                                let shutdown = shutdown.clone();
                                let hb_tx = signal_tx.clone();
                                let status = status_provider.clone();

                                // Step 2 Phase D.2: per-connection handler
                                // tracked through process-global registry —
                                // panics inside `handle_connection` are
                                // surfaced at error level. Closes Step 1 §六
                                // R1 ① for ipc.rs:478.
                                crate::task_registry::global().spawn(
                                    "supervisor.ipc.unix_per_conn_handler",
                                    async move {
                                        if let Err(e) = handle_connection(
                                            stream, &config, &executor, shutdown, hb_tx,
                                            status,
                                        ).await {
                                            tracing::error!(error = %e, "处理连接时发生错误");
                                        }
                                    },
                                );
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "接受连接失败");
                            }
                        }
                    }
                }
            }

            // 清理 socket 文件
            if uds_path.exists() {
                let _ = std::fs::remove_file(&uds_path);
                tracing::debug!(path = %uds_path.display(), "已清理 UDS socket 文件");
            }
        });

        Ok(handle)
    }

    /// Windows 平台：启动 Named Pipe 监听
    #[cfg(windows)]
    async fn start_windows(
        &self,
        config: IpcConfig,
        executor: Arc<CommandExecutor>,
        shutdown: CancellationToken,
        signal_tx: tokio::sync::mpsc::Sender<IpcSignal>,
        status_provider: Option<crate::status::SharedStatus>,
    ) -> anyhow::Result<tokio::task::JoinHandle<()>> {
        let pipe_name = config
            .pipe_name
            .clone()
            .unwrap_or_else(|| format!(r"\\.\pipe\crabcode-{}", std::process::id()));

        tracing::info!(pipe = %pipe_name, "IPC 服务器启动（Windows Named Pipe）");

        let pipe_name_clone = pipe_name;
        // JoinHandle held by caller — same pattern as `start_unix`.
        // Audited Step 2 Phase D.1.
        #[allow(clippy::disallowed_methods)]
        let handle = tokio::spawn(async move {
            loop {
                // 创建新的 pipe 实例等待客户端连接
                //
                // P1-2: 当前用户专属 DACL（owner + SYSTEM 全权，其它一律拒）。
                // 通过 SDDL `D:P(A;;GA;;;OW)(A;;GA;;;SY)` 构建 security descriptor，
                // 经 `create_with_security_attributes_raw` 应用，等价 Unix 0600。
                let server = match create_named_pipe_server_current_user(&pipe_name_clone) {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::error!(error = %e, "创建 Named Pipe 失败");
                        // 短暂等待后重试
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                };

                // 等待客户端连接或 shutdown 信号
                tokio::select! {
                    () = shutdown.cancelled() => {
                        tracing::info!("IPC 服务器收到关闭信号，停止接受新连接");
                        break;
                    }
                    connect_result = server.connect() => {
                        match connect_result {
                            Ok(()) => {
                                tracing::info!("接受新的 Named Pipe 连接");
                                let executor = Arc::clone(&executor);
                                let config = config.clone();
                                let shutdown = shutdown.clone();
                                let hb_tx = signal_tx.clone();
                                let status = status_provider.clone();

                                // Step 2 Phase D.2 supplemental (not in plan
                                // §2.5 strict 6 because cfg(windows) hides
                                // it from clippy on Linux dev hosts): same
                                // hazard as Unix sibling (line 489 after
                                // edits). Closes Step 1 §六 R1 ① for the
                                // Windows per-pipe handler.
                                crate::task_registry::global().spawn(
                                    "supervisor.ipc.windows_per_pipe_handler",
                                    async move {
                                        if let Err(e) = handle_connection(
                                            server, &config, &executor, shutdown, hb_tx,
                                            status,
                                        ).await {
                                            tracing::error!(error = %e, "处理连接时发生错误");
                                        }
                                });
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "Named Pipe 连接失败");
                            }
                        }
                    }
                }
            }
        });

        Ok(handle)
    }
}

// ── P1-2 认证辅助 ───────────────────────────────────────────────────────

/// 校验 UDS peer 凭据 uid == 当前进程 uid。
///
/// 跨 uid 连接直接拒绝（在任何 handshake 处理前）。即便 socket 权限 0600 +
/// 父目录 0700 已大幅收窄攻击面，peer-uid 校验是纵深防御的第二道（防同机
/// 其它用户 / 提权场景）。
#[cfg(unix)]
#[allow(unsafe_code)]
fn verify_peer_uid(stream: &tokio::net::UnixStream) -> Result<(), String> {
    let cred = stream
        .peer_cred()
        .map_err(|e| format!("无法读取 peer 凭据: {e}"))?;
    // SAFETY: getuid() 无前置条件、不会失败、不解引用任何指针。
    let self_uid = unsafe { nix::libc::getuid() };
    if cred.uid() != self_uid {
        return Err(format!(
            "peer uid={} 与本进程 uid={} 不匹配",
            cred.uid(),
            self_uid
        ));
    }
    Ok(())
}

/// P1-2: 用当前用户专属 DACL 创建 Named Pipe server 实例（Windows）。
///
/// 默认 `ServerOptions::create` 使用 default DACL（继承 token，可能放宽）。
/// 这里通过 SDDL `D:P(A;;GA;;;OW)(A;;GA;;;SY)` 构建一个仅 owner（OW）+
/// LocalSystem（SY）有 GENERIC_ALL、其它 principal 一律无权的 security
/// descriptor，等价 Unix 0600。`P` = protected（不继承父级 ACE）。
///
/// **未在 macOS/Linux 平台编译验证**（cfg(windows) 屏蔽）；逻辑按 win32 API
/// 契约书写，需在 Windows host 上 smoke 验证。
#[cfg(windows)]
#[allow(unsafe_code)]
fn create_named_pipe_server_current_user(
    pipe_name: &str,
) -> std::io::Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use std::os::windows::ffi::OsStrExt;
    use tokio::net::windows::named_pipe::ServerOptions;
    use windows_sys::Win32::Foundation::{HLOCAL, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};

    // SDDL: protected DACL granting GENERIC_ALL to the object owner (OW) and
    // LocalSystem (SY); nothing else gets access.
    let sddl: Vec<u16> = std::ffi::OsStr::new("D:P(A;;GA;;;OW)(A;;GA;;;SY)")
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let mut psd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: sddl is a valid NUL-terminated wide string; psd receives a
    // LocalAlloc'd descriptor we free below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            // windows-sys 0.59 defines SDDL_REVISION_1 as u32; avoid a
            // redundant cast that fails cross-platform clippy with -D warnings.
            SDDL_REVISION_1,
            &mut psd,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut sa = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: psd,
        bInheritHandle: 0,
    };

    // SAFETY: `sa` lives for the duration of the create call; psd is a valid
    // descriptor produced above.
    let result = unsafe {
        ServerOptions::new()
            .first_pipe_instance(false)
            .create_with_security_attributes_raw(
                pipe_name,
                std::ptr::addr_of_mut!(sa).cast::<core::ffi::c_void>(),
            )
    };

    // Free the descriptor regardless of create() outcome.
    // SAFETY: psd was allocated by ConvertStringSecurityDescriptor... via LocalAlloc.
    unsafe {
        LocalFree(psd as HLOCAL);
    }

    result
}

/// P1-2: 从握手 envelope 中提取并校验 `auth_secret`。
///
/// 缺失或不匹配立即返 `Err`（无静默降级）。使用长度优先 + 逐字节比较以避免
/// 早退分支（loopback 一次性 token，已 0600 收窄；常量时间是 belt-and-suspenders）。
fn verify_handshake_secret(
    params: Option<&serde_json::Value>,
    expected: &str,
) -> Result<(), IpcError> {
    let provided = params
        .and_then(|p| p.get("auth_secret"))
        .and_then(|v| v.as_str());
    let provided = match provided {
        Some(s) => s,
        None => {
            return Err(IpcError::AuthDenied(
                "握手缺少 auth_secret 字段".to_string(),
            ));
        }
    };
    let a = provided.as_bytes();
    let b = expected.as_bytes();
    let mut diff = a.len() ^ b.len();
    let n = a.len().min(b.len());
    for i in 0..n {
        diff |= (a[i] ^ b[i]) as usize;
    }
    if diff != 0 {
        return Err(IpcError::AuthDenied("auth_secret 不匹配".to_string()));
    }
    Ok(())
}

// ── 连接处理 ────────────────────────────────────────────────────────────

/// 处理单个 IPC 连接
///
/// 连接处理流程：
/// 1. 交换 Handshake 消息（验证协议版本）
/// 2. 进入消息循环：读取原始 JSON → 按 `msg_type` 分发
///    - `"heartbeat"` → 通知 supervisor 记录 Go 心跳
///    - 其他 → 反序列化为 CapabilityReq，路由到 executor
/// 3. 连接断开时清理
///
/// 此函数对 Unix UDS 和 Windows Named Pipe 通用（通过泛型约束）。
async fn handle_connection<T>(
    stream: T,
    config: &IpcConfig,
    executor: &CommandExecutor,
    shutdown: CancellationToken,
    signal_tx: tokio::sync::mpsc::Sender<IpcSignal>,
    status_provider: Option<crate::status::SharedStatus>,
) -> Result<(), IpcError>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    // 用于 Framed 解码的缓冲区
    let mut read_buf = BytesMut::with_capacity(4096);

    // ── 步骤 1: 接收 Go 发来的 envelope-wrapped handshake request ──
    //
    // 协议契约 (原与 services/internal/ipc/handshake.go 对齐, M6 已归档至
    // archive/2026-05-02-go-services/internal/ipc/handshake.go):
    //   Go 客户端首帧 = MessageEnvelope{
    //       msg_type: "request",
    //       payload: { method: "version_handshake", params: VersionHandshake{
    //           protocol_version, component, component_version, min_protocol_version,
    //           capabilities, build_info } } }
    //
    //   Rust 服务端响应 = MessageEnvelope{
    //       msg_type: "response",
    //       payload: { result: { accepted, negotiated_version, capabilities,
    //                            reason? } } }
    //
    // 历史 bug (2026-04-26 根因修复): 原实现 server-first 发送 bare Handshake,
    // 但 client 用 request envelope，serde 解 bare Handshake 必报 "missing field
    // protocol_version" — 因为 protocol_version 在 payload.params 里不在顶层。
    // 历史症状是心跳 35s 后被判 Dead → kill-restart 风暴。
    let req_value: serde_json::Value = match recv_framed(&mut reader, &mut read_buf).await? {
        Some(v) => v,
        None => return Err(IpcError::ConnectionClosed),
    };

    let msg_type = req_value
        .get("msg_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let method = req_value
        .get("payload")
        .and_then(|p| p.get("method"))
        .and_then(|m| m.as_str())
        .unwrap_or("");
    let req_msg_id = req_value
        .get("header")
        .and_then(|h| h.get("msg_id"))
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    let req_trace = TraceEcho::from_envelope(&req_value);

    if msg_type != "request" || method != "version_handshake" {
        tracing::warn!(
            msg_type = %msg_type,
            method = %method,
            "首帧不是 envelope handshake request — 拒绝连接"
        );
        return Err(IpcError::Protocol(format!(
            "expected request method=version_handshake, got msg_type={msg_type} method={method}"
        )));
    }

    let params = req_value.get("payload").and_then(|p| p.get("params"));

    // ── P1-2: 握手密钥校验（在版本协商与任何 capability 分发之前） ──
    // 缺失或错误密钥立即断连，无静默降级到未认证模式。受信子进程经
    // CRABCODE_SUPERVISOR_SECRET env 继承 secret（lib.rs ts-session 注入点）。
    if let Err(auth_err) = verify_handshake_secret(params, &config.auth_secret) {
        tracing::warn!(error = %auth_err, "握手认证失败 — 拒绝连接");
        return Err(auth_err);
    }

    let remote_version = params
        .and_then(|p| p.get("protocol_version"))
        .and_then(|v| v.as_str())
        .unwrap_or("0")
        .to_string();
    let remote_component = params
        .and_then(|p| p.get("component_version"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    tracing::info!(
        remote_version = %remote_version,
        remote_component = %remote_component,
        "收到远程 Handshake (envelope)"
    );

    // 验证协议版本兼容性（主版本号必须相同）
    let local_major = config.protocol_version.split('.').next().unwrap_or("0");
    let remote_major = remote_version.split('.').next().unwrap_or("0");
    let accepted = local_major == remote_major;

    let result = if accepted {
        serde_json::json!({
            "accepted": true,
            "negotiated_version": config.protocol_version,
            "component": "rust",
            "component_version": config.component_version,
            "capabilities": ["exec", "spawn_managed", "fs_read"],
        })
    } else {
        serde_json::json!({
            "accepted": false,
            "reason": format!(
                "major version mismatch: local={} remote={}",
                config.protocol_version, remote_version
            ),
        })
    };

    let resp = build_envelope_response_traced(&req_msg_id, result, &req_trace);
    send_framed(&mut writer, &resp).await?;

    if !accepted {
        return Err(IpcError::VersionMismatch {
            local: config.protocol_version.clone(),
            remote: remote_version,
        });
    }

    // ── 步骤 2.5: 每连接 scope 的 ProcessRegistry 与出站 event 通道 ──
    // 2026-04-23 根因补全: broker spawn 的 MCP 子进程生命与此连接绑定；
    // 出站通道给 supervise task 把 stdout / stderr / exit 以 IPC event 回推。
    // connection 断开时 shutdown_all 避免 Go 进程退出后留孤儿 MCP server。
    let registry = std::sync::Arc::new(crate::process_registry::ProcessRegistry::new());
    let (outbox_tx, mut outbox_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(256);

    // ── 步骤 3: 消息循环（按 msg_type + method 二级分发） ──
    tracing::info!("进入消息循环");
    loop {
        tokio::select! {
            () = shutdown.cancelled() => {
                tracing::info!("连接处理收到关闭信号");
                break;
            }
            // 后台 supervise task 推送的出站事件（process.stdout / stderr / exit）
            Some(event_env) = outbox_rx.recv() => {
                if let Err(e) = send_framed(&mut writer, &event_env).await {
                    tracing::warn!(error = %e, "发送 process event 失败，中断连接");
                    break;
                }
            }
            msg = recv_framed::<serde_json::Value, _>(&mut reader, &mut read_buf) => {
                match msg {
                    Ok(Some(value)) => {
                        let msg_type = value.get("msg_type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        match msg_type.as_str() {
                            // ── 心跳 ──
                            "heartbeat" => {
                                tracing::trace!("收到 Go 心跳");
                                if let Err(e) = signal_tx.try_send(IpcSignal::Heartbeat(acosmi_heartbeat::ProcessKind::Go)) {
                                    tracing::warn!(error = %e, "IPC 信号发送失败（通道满或已关闭）");
                                }
                                // Go 心跳携带 TS 状态。两种格式（向前/向后兼容）:
                                //
                                // 1. 4 态枚举（pre-PerConnState 单 conn 模型）:
                                //    alive / degraded / unresponsive / dead
                                //
                                // 2. 多 conn 聚合（2026-04-28 PerConnState 改造，arch §8）:
                                //    "no_ts_connected" — 当前 0 conn（boot 期或所有 TUI 优雅退出）
                                //    "K/N_alive"       — 共 N 条 conn，其中 K 条 LivenessAlive
                                //
                                // 映射策略:
                                //   K==N (含 1/1)  → TsAlive       记心跳，保持 Alive
                                //   K==0 N>0       → TsUnresponsive 全员失活，触发 kill-restart
                                //   0<K<N          → TsDegraded     部分失活，记心跳但标记降级
                                //   no_ts_connected→ TsAlive        boot 期/收尾期乐观处理，避免误杀
                                //                                   （与 pre-reform ChannelHealth 初始
                                //                                   LivenessAlive 默认一致）
                                //
                                // 此处为**唯一**的 TS 心跳源（替代已删除的 try_check_alive 假心跳）。
                                if let Some(ts_status) = value
                                    .get("payload")
                                    .and_then(|p| p.get("ts_status"))
                                    .and_then(|s| s.as_str())
                                {
                                    let derived = derive_ts_signal(ts_status);
                                    match derived {
                                        TsStatusDerived::Alive => {
                                            if let Err(e) = signal_tx.try_send(IpcSignal::TsAlive) {
                                                tracing::warn!(error = %e, "TsAlive 信号发送失败");
                                            }
                                        }
                                        TsStatusDerived::Degraded => {
                                            if let Err(e) = signal_tx.try_send(IpcSignal::Heartbeat(acosmi_heartbeat::ProcessKind::TypeScript)) {
                                                tracing::warn!(error = %e, "TS 心跳（degraded）信号发送失败");
                                            }
                                            if let Err(e) = signal_tx.try_send(IpcSignal::TsDegraded) {
                                                tracing::warn!(error = %e, "TsDegraded 信号发送失败");
                                            }
                                        }
                                        TsStatusDerived::Unresponsive => {
                                            // 不记心跳；让 tracker 到 max_miss 阈值自然触发 Dead
                                            tracing::debug!(status = %ts_status, "Go 报告 TS unresponsive，不注入心跳");
                                        }
                                        TsStatusDerived::Dead => {
                                            if let Err(e) = signal_tx.try_send(IpcSignal::TsUnresponsive) {
                                                tracing::warn!(error = %e, "TsUnresponsive 信号发送失败");
                                            }
                                        }
                                        TsStatusDerived::Unknown => {
                                            tracing::warn!(status = %ts_status, "未识别的 ts_status 字面量");
                                        }
                                    }
                                }
                            }
                            // ── MessageEnvelope request：按 method 路由 ──
                            "request" => {
                                let method = value.get("payload")
                                    .and_then(|p| p.get("method"))
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let req_msg_id = value.get("header")
                                    .and_then(|h| h.get("msg_id"))
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                // 提取 payload.params 供能力申请使用
                                let params_value = value.get("payload")
                                    .and_then(|p| p.get("params"))
                                    .cloned();
                                // W-TELE-P2 Phase B: extract incoming trace header once
                                // so every response path echoes consistent trace fields
                                // back to Go. None-variant means legacy sender without
                                // trace (pre-W-TELE-P2); echo is a no-op in that case.
                                let req_trace = TraceEcho::from_envelope(&value);
                                // Instrument the request handling with an ipc_handle span
                                // so Rust-side tracing::info!/warn! inside handlers carry
                                // trace_id = Go's trace_id. _guard drops at end of match.
                                //
                                // !! W-TELE-P2-FU-4 (2026-04-25 Wave 5) WARNING:
                                // `_entered` is a synchronous Span::enter() guard. It is
                                // VALID across `.await` points within THIS match block
                                // because all current handlers stay in the same task.
                                // Any future refactor that dispatches a handler to a
                                // background task via `tokio::spawn(handler)` MUST use
                                // `.instrument(span)` instead — the guard does NOT cross
                                // task boundaries and trace_id will silently disappear
                                // from Rust logs (no compile error). Pattern:
                                //   tokio::spawn(handler.instrument(_ipc_span.clone()));
                                // See contracts/schemas/observability-context.schema.json
                                // and the W-TELE-P2 architecture doc for context.
                                let _ipc_span = if let Some(tid) = req_trace.trace_id.as_deref() { tracing::info_span!(
                                    "ipc_handle",
                                    method = %method,
                                    trace_id = %tid,
                                    parent_span_id = req_trace.span_id.as_deref().unwrap_or("")
                                ) } else { tracing::info_span!("ipc_handle", method = %method) };
                                let _entered = _ipc_span.enter();

                                match method.as_str() {
                                    // 进程取消（Go CapabilityClient.Cancel）
                                    // 直接从原始 JSON 提取 abort_token，绕过 CapabilityReq
                                    // 反序列化（Go family="process" 与 Rust enum 不兼容）
                                    // 进程取消（Go CapabilityClient.Cancel）
                                    // 直接从原始 JSON 提取 abort_token，绕过 CapabilityReq
                                    // 反序列化（Go family="process" 与 Rust enum 不兼容）。
                                    // 返回 status="success" 匹配 Go parseCapabilityResponse 期望。
                                    "capability.cancel" => {
                                        let token = params_value.as_ref()
                                            .and_then(|p| p.get("args"))
                                            .and_then(|a| a.as_array())
                                            .and_then(|arr| arr.first())
                                            .and_then(|v| v.as_str());
                                        let result = if let Some(t) = token {
                                            let killed = executor.abort(t).await;
                                            tracing::info!(abort_token = t, killed, "capability.cancel 处理完成");
                                            serde_json::json!({"status": "success", "killed": killed})
                                        } else {
                                            tracing::warn!("capability.cancel 缺少 abort_token（args[0]）");
                                            serde_json::json!({
                                                "status": "denied",
                                                "denial_reason": "invalid_params",
                                                "denial_message": "missing abort_token in args[0]"
                                            })
                                        };
                                        let resp = build_envelope_response_traced(&req_msg_id, result, &req_trace);
                                        send_framed(&mut writer, &resp).await?;
                                    }
                                    // supervisor.status：返回主循环维护的快照（P1.1）
                                    "supervisor.status" => {
                                        let result = match status_provider.as_ref() {
                                            Some(shared) => {
                                                let guard = shared.read().await;
                                                serde_json::to_value(&*guard).unwrap_or_else(|e| {
                                                    serde_json::json!({
                                                        "schema_version": 0,
                                                        "error": format!("serialize failed: {e}"),
                                                    })
                                                })
                                            }
                                            None => serde_json::json!({
                                                "schema_version": 0,
                                                "error": "status provider not injected",
                                            }),
                                        };
                                        let resp = build_envelope_response_traced(&req_msg_id, result, &req_trace);
                                        send_framed(&mut writer, &resp).await?;
                                    }
                                    // 2026-04-28 阶段 3-B：cron.* 路由已移除（立宪 3：cron 跟随 hub）。
                                    // 调用方撞 `_` 分支，得到 method-not-found。
                                    // 阶段 3-C（WT3）已把 acosmi-cmd-cron 的 RPC 客户端从 gateway-rpc
                                    // 切到 cron daemon UDS（~/.crabcode/run/cron.sock）；旧的
                                    // 「fallback 到 scheduled_tasks.json 直读直写」也已经移除（daemon 是
                                    // scheduled_tasks.lock 的唯一持有者，绕过它会脱节）。
                                    //
                                    // 其他能力申请（method 以 capability. 开头）
                                    m if m.starts_with("capability.") => {
                                        if let Some(params) = params_value {
                                            match serde_json::from_value::<CapabilityReq>(params) {
                                                Ok(req) => {
                                                    tracing::debug!(family = ?req.family, command = ?req.command, "收到能力申请");
                                                    let cap_resp = handle_capability_request_full(
                                                        req,
                                                        executor,
                                                        &registry,
                                                        &outbox_tx,
                                                    ).await;
                                                    // Step 2 Phase D.6 / Step 1 §六 R1 ④:
                                                    // `cap_resp` is a `CapabilityResponse` —
                                                    // a derive(Serialize) struct with no
                                                    // unrepresentable variants (no `f64::NAN`
                                                    // fields, no foreign types). Serialization
                                                    // failure is therefore a programmer-introduced
                                                    // contract violation (e.g., someone added a
                                                    // weird field), not a runtime data condition.
                                                    // Surface it as a Json error response so the
                                                    // caller sees a clear failure rather than
                                                    // a silent `null` from `unwrap_or_default()`.
                                                    let cap_payload = match serde_json::to_value(&cap_resp) {
                                                        Ok(v) => v,
                                                        Err(e) => {
                                                            tracing::error!(
                                                                target: "supervisor.ipc.capability",
                                                                error = %e,
                                                                "BUG: CapabilityResponse failed to serialize — type contract violated",
                                                            );
                                                            serde_json::json!({
                                                                "error": "internal: capability response serialize failed",
                                                                "detail": e.to_string(),
                                                            })
                                                        }
                                                    };
                                                    let resp = build_envelope_response_traced(
                                                        &req_msg_id,
                                                        cap_payload,
                                                        &req_trace,
                                                    );
                                                    send_framed(&mut writer, &resp).await?;
                                                }
                                                Err(e) => {
                                                    tracing::warn!(error = %e, method = m, "能力申请参数解析失败");
                                                }
                                            }
                                        } else {
                                            tracing::warn!(method = m, "能力申请缺少 payload.params");
                                        }
                                    }
                                    _ => {
                                        tracing::warn!(method = %method, "未知 request method，忽略");
                                    }
                                }
                            }
                            // ── 兜底：尝试裸 CapabilityReq 格式（向后兼容） ──
                            _ => {
                                match serde_json::from_value::<CapabilityReq>(value) {
                                    Ok(req) => {
                                        tracing::debug!(family = ?req.family, command = ?req.command, "收到能力申请（legacy 格式）");
                                        let resp = handle_capability_request_full(
                                            req,
                                            executor,
                                            &registry,
                                            &outbox_tx,
                                        ).await;
                                        send_framed(&mut writer, &resp).await?;
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, msg_type = msg_type, "未知消息类型，忽略");
                                    }
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::info!("远程端已断开连接");
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "读取消息时发生错误");
                        break;
                    }
                }
            }
        }
    }

    // 连接断开：kill 所有此连接 spawn 的子进程，避免孤儿 MCP server。
    registry.shutdown_all().await;
    tracing::debug!("连接处理结束");
    Ok(())
}

/// W-TELE-P2 Phase B: trace fields extracted from an incoming envelope header.
///
/// Lightweight struct used to echo TraceID/SessionID/TaskID/ToolUseID onto the
/// response header, and to record `parent_span_id` pointing at the client's
/// request span. A response's own `span_id` is a fresh server-side id generated
/// per call — intentionally independent of client's, so Go sees cleanly
/// distinguishable spans on the same trace.
#[derive(Debug, Default, Clone)]
struct TraceEcho {
    trace_id: Option<String>,
    /// Client's span id — becomes `parent_span_id` on the response.
    span_id: Option<String>,
    session_id: Option<String>,
    task_id: Option<String>,
    tool_use_id: Option<String>,
}

impl TraceEcho {
    fn from_envelope(env: &serde_json::Value) -> Self {
        let h = match env.get("header") {
            Some(h) => h,
            None => return Self::default(),
        };
        let field =
            |k: &str| -> Option<String> { h.get(k).and_then(|v| v.as_str()).map(str::to_string) };
        Self {
            trace_id: field("trace_id"),
            span_id: field("span_id"),
            session_id: field("session_id"),
            task_id: field("task_id"),
            tool_use_id: field("tool_use_id"),
        }
    }

    /// True when trace context was actually present (not a legacy sender).
    const fn has_trace(&self) -> bool {
        self.trace_id.is_some()
    }
}

/// Build a `MessageEnvelope` response that echoes the incoming trace header.
/// Go side `dispatchResponse` matches via `header.correlation_id`.
///
/// When the request had no trace (legacy sender), the response is stamped
/// only with `msg_id/correlation_id` (i.e. no trace fields).
///
/// The response is a fresh server-side span under the same trace:
///   - `header.trace_id`       = `req.trace_id`
///   - `header.span_id`        = fresh 16-hex (generated here)
///   - `header.parent_span_id` = `req.span_id`
///   - `header.{session,task,tool_use`}_id = echoed from req
fn build_envelope_response_traced(
    correlation_id: &str,
    result: serde_json::Value,
    trace: &TraceEcho,
) -> serde_json::Value {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut header = serde_json::json!({
        "version": 1,
        "msg_id": format!("rust-{now_ms}"),
        "correlation_id": correlation_id,
        "timestamp_ms": now_ms,
        "source_component": "rust"
    });

    if trace.has_trace() {
        if let Some(tid) = &trace.trace_id {
            header["trace_id"] = serde_json::Value::String(tid.clone());
        }
        // Fresh span_id for the response work unit.
        // W-TELE-P2-FU-5 (2026-04-25 Wave 5): unified to acosmi-types::otel::generate_span_id
        // — was previously a local copy with slightly different XOR mask. now_ms argument is
        // no longer needed (the upstream generator uses its own SystemTime read).
        let _ = now_ms; // kept for call site parity until generate_resp_span_id is removed
        header["span_id"] = serde_json::Value::String(acosmi_types::otel::generate_span_id());
        if let Some(psid) = &trace.span_id {
            header["parent_span_id"] = serde_json::Value::String(psid.clone());
        }
        if let Some(s) = &trace.session_id {
            header["session_id"] = serde_json::Value::String(s.clone());
        }
        if let Some(t) = &trace.task_id {
            header["task_id"] = serde_json::Value::String(t.clone());
        }
        if let Some(tu) = &trace.tool_use_id {
            header["tool_use_id"] = serde_json::Value::String(tu.clone());
        }
    }

    serde_json::json!({
        "header": header,
        "msg_type": "response",
        "channel": "rust_go",
        "payload": {
            "result": result
        }
    })
}

// W-TELE-P2-FU-5 (2026-04-25 Wave 5): generate_resp_span_id 已删除；统一调用
// acosmi_types::otel::generate_span_id（pub）。两 crate 间不再各自维护 span_id 算法。

/// 通过异步写入器发送 Framed 消息（4-byte BE 长度前缀 + JSON）
///
/// 与 Go 端 FramedCodec.Encode 线格式一致。
async fn send_framed<W, M>(writer: &mut W, msg: &M) -> Result<(), IpcError>
where
    W: tokio::io::AsyncWrite + Unpin,
    M: Serialize,
{
    let value = serde_json::to_value(msg)?;
    let mut buf = BytesMut::new();
    FramedJsonCodec.encode(value, &mut buf)?;
    writer.write_all(&buf).await?;
    writer.flush().await?;
    Ok(())
}

/// 通过异步读取器接收并反序列化 Framed 消息
///
/// 从 `reader` 读取数据到 `read_buf`，然后通过 `FramedJsonCodec` 解码。
/// 返回 `Ok(None)` 表示连接已关闭（EOF）。
async fn recv_framed<M, R>(reader: &mut R, read_buf: &mut BytesMut) -> Result<Option<M>, IpcError>
where
    R: tokio::io::AsyncRead + Unpin,
    M: DeserializeOwned,
{
    let mut temp = [0u8; 4096];

    loop {
        // 先尝试从已有缓冲区解码
        if let Some(value) = FramedJsonCodec.decode(read_buf)? {
            let msg: M = serde_json::from_value(value)?;
            return Ok(Some(msg));
        }

        // 缓冲区中没有完整消息，从 reader 读取更多数据
        let n = reader.read(&mut temp).await?;
        if n == 0 {
            // EOF：连接已关闭
            return Ok(None);
        }
        read_buf.extend_from_slice(&temp[..n]);
    }
}

// ── 能力申请路由 ────────────────────────────────────────────────────────

/// 处理来自 Go 的能力申请请求
///
/// 根据请求的能力家族路由到对应的处理逻辑：
/// - `Exec`: 转换为 [`ExecReq`] 并通过 executor 执行
/// - `SpawnManaged`: Phase 2 实现（暂返回 Denied）
/// - 其他: 返回 Denied
///
/// 2026-04-28 阶段 3-B：`CapabilityFamily::Cron` 分支已删除（立宪 3：cron 不再
/// 经过 supervisor，迁到独立 `crabcode-cron` 直连 hub 路径）。
async fn handle_capability_request(
    req: CapabilityReq,
    executor: &CommandExecutor,
) -> CapabilityResp {
    match req.family {
        CapabilityFamily::Exec => {
            let command = req.command.unwrap_or_default();
            let args = req.args.unwrap_or_default();
            let policy = req.policy.unwrap_or(acosmi_types::protocol::ExecPolicy {
                sandbox: None,
                timeout_ms: None,
                cwd: None,
                max_output_bytes: None,
                stdin_data: None,
                allowed_path_prefixes: None,
                inherit_env: None,
            });
            // 将 CapabilityReq 转换为 ExecReq
            let exec_req = ExecReq {
                family: CommandFamily::Other(command.clone()),
                program: command,
                args,
                cwd: policy.cwd.filter(|s| !s.is_empty()),
                env: HashMap::new(),
                stdin: None,
                timeout_ms: policy.timeout_ms.unwrap_or(0),
                sandbox: policy.sandbox.unwrap_or(false),
                abort_token: None,
            };

            let result = executor.execute(exec_req).await;

            // 根据执行结果构建响应
            if result.error.is_some() {
                CapabilityResp {
                    status: CapabilityStatus::Error,
                    exec_result: Some(result),
                    ..Default::default()
                }
            } else {
                CapabilityResp {
                    status: CapabilityStatus::Ok,
                    exec_result: Some(result),
                    ..Default::default()
                }
            }
        }
        CapabilityFamily::SpawnManaged => {
            // The plain capability handler lacks the connection-bound process
            // registry required for lifecycle-safe managed spawn. Callers must
            // use the registered exec.spawnManaged transport instead.
            tracing::warn!(
                "capability.spawn 路径仍未实装 —— 调用方应迁移到 exec.spawnManaged stdio NDJSON bridge（Go MCP broker 独立立项）"
            );
            CapabilityResp {
                status: CapabilityStatus::Denied,
                denial_reason: Some("lifecycle_not_supported".to_string()),
                denial_message: Some(
                    "capability.spawn requires registry dispatch (unreachable via plain handler)"
                        .to_string(),
                ),
                error: Some(acosmi_types::CrabError::exec(
                    acosmi_types::error::codes::PROTOCOL_CAPABILITY_DENIED,
                    "capability.spawn needs registry-aware handler",
                    "route through handle_capability_request_with_registry",
                )),
                ..Default::default()
            }
        }
        _ => {
            // 不支持的能力家族
            tracing::warn!(family = ?req.family, "不支持的能力家族");
            CapabilityResp {
                status: CapabilityStatus::Denied,
                denial_reason: Some("unknown".to_string()),
                denial_message: Some(format!("不支持的能力家族: {:?}", req.family)),
                error: Some(acosmi_types::CrabError::exec(
                    acosmi_types::error::codes::PROTOCOL_CAPABILITY_DENIED,
                    format!("不支持的能力家族: {:?}", req.family),
                    "请求的能力不被支持",
                )),
                ..Default::default()
            }
        }
    }
}

/// Registry-aware capability handler.
///
/// 2026-04-23 根因补全：在 `handle_capability_request` 上加壳，专门处理
/// 两类需要 Go UDS event relay / process handle table 的请求：
///
/// 1. `CapabilityFamily::SpawnManaged` — 真实 spawn 子进程、注册到 registry、
///    返回 `ManagedProcessHandle` 给 Go；后台 task 把 stdout / stderr / exit
///    包成 `MessageEnvelope{msg_type=event}` 经 `outbox` 回发。
/// 2. `CapabilityFamily::Exec` with `command == "process_write_stdin"` 或
///    `"process_kill"` — 不走 `executor.execute`（那会当作 shell 命令找不到），
///    而是对 registry 做 stdin write / kill 操作。Go 侧 `broker.transport_stdio.Send`
///    与 `broker.createTransport` 的 onClose 分别走这两条路径。
///
/// 其他 family 透传给原 `handle_capability_request`。
async fn handle_capability_request_full(
    req: CapabilityReq,
    executor: &CommandExecutor,
    registry: &std::sync::Arc<crate::process_registry::ProcessRegistry>,
    outbox: &tokio::sync::mpsc::Sender<serde_json::Value>,
) -> CapabilityResp {
    let request_id = req.request_id.clone();
    let trace_id = if req.trace_id.is_empty() {
        None
    } else {
        Some(req.trace_id.clone())
    };

    match &req.family {
        CapabilityFamily::SpawnManaged => {
            let command = req.command.clone().unwrap_or_default();
            let args = req.args.clone().unwrap_or_default();
            if command.is_empty() {
                return CapabilityResp {
                    request_id,
                    trace_id,
                    status: CapabilityStatus::Error,
                    error: Some(acosmi_types::CrabError::exec(
                        acosmi_types::error::codes::PROTOCOL_INVALID_REQUEST,
                        "capability.spawn missing command",
                        "command field is required",
                    )),
                    ..Default::default()
                };
            }
            match registry.spawn(command, args, outbox.clone()).await {
                Ok(handle) => CapabilityResp {
                    request_id,
                    trace_id,
                    status: CapabilityStatus::Ok,
                    process_handle: Some(handle),
                    ..Default::default()
                },
                Err(e) => CapabilityResp {
                    request_id,
                    trace_id,
                    status: CapabilityStatus::Error,
                    error: Some(acosmi_types::CrabError::exec(
                        acosmi_types::error::codes::INFRA_PROCESS_SPAWN_FAILED,
                        format!("spawn failed: {e}"),
                        "check command path and permissions",
                    )),
                    ..Default::default()
                },
            }
        }
        CapabilityFamily::Exec => {
            // Go broker uses capability.exec with these reserved command names
            // to address the long-running process table.
            let command = req.command.clone().unwrap_or_default();
            let args = req.args.clone().unwrap_or_default();
            match command.as_str() {
                "process_write_stdin" => {
                    if args.len() < 2 {
                        return CapabilityResp {
                            request_id,
                            trace_id,
                            status: CapabilityStatus::Error,
                            error: Some(acosmi_types::CrabError::exec(
                                acosmi_types::error::codes::PROTOCOL_INVALID_REQUEST,
                                "process_write_stdin requires [stdin_write_id, data]",
                                "provide 2 args",
                            )),
                            ..Default::default()
                        };
                    }
                    let stdin_id = &args[0];
                    let data = args[1].as_bytes();
                    match registry.write_stdin(stdin_id, data).await {
                        Ok(()) => CapabilityResp {
                            request_id,
                            trace_id,
                            status: CapabilityStatus::Ok,
                            exec_result: Some(acosmi_types::exec_types::ExecResult {
                                stdout: String::new(),
                                stderr: String::new(),
                                code: 0,
                                timed_out: false,
                                killed: false,
                                duration_ms: 0,
                                error: None,
                            }),
                            ..Default::default()
                        },
                        Err(e) => CapabilityResp {
                            request_id,
                            trace_id,
                            status: CapabilityStatus::Error,
                            error: Some(acosmi_types::CrabError::exec(
                                acosmi_types::error::codes::INFRA_IPC_PROTOCOL_ERROR,
                                format!("stdin write failed: {e}"),
                                "handle may be invalid or child exited",
                            )),
                            ..Default::default()
                        },
                    }
                }
                "process_kill" => {
                    if args.is_empty() {
                        return CapabilityResp {
                            request_id,
                            trace_id,
                            status: CapabilityStatus::Error,
                            error: Some(acosmi_types::CrabError::exec(
                                acosmi_types::error::codes::PROTOCOL_INVALID_REQUEST,
                                "process_kill requires [kill_id]",
                                "provide 1 arg",
                            )),
                            ..Default::default()
                        };
                    }
                    let killed = registry.kill(&args[0]).await;
                    CapabilityResp {
                        request_id,
                        trace_id,
                        status: CapabilityStatus::Ok,
                        exec_result: Some(acosmi_types::exec_types::ExecResult {
                            stdout: String::new(),
                            stderr: if killed {
                                String::new()
                            } else {
                                "kill_id unknown".to_string()
                            },
                            code: i32::from(!killed),
                            timed_out: false,
                            killed: false,
                            duration_ms: 0,
                            error: None,
                        }),
                        ..Default::default()
                    }
                }
                _ => {
                    // Non-reserved exec — delegate to plain handler (which
                    // invokes executor.execute). Still merge request_id back.
                    let mut resp = handle_capability_request(req, executor).await;
                    resp.request_id = request_id;
                    resp.trace_id = trace_id;
                    resp
                }
            }
        }
        _ => {
            let mut resp = handle_capability_request(req, executor).await;
            resp.request_id = request_id;
            resp.trace_id = trace_id;
            resp
        }
    }
}

// ── StdioBridge — TS 桥连 ───────────────────────────────────────────────

/// TS 进程 stdio 桥连
///
/// 通过子进程的 stdin/stdout 管道实现 NDJSON 通信。
/// 用于 Rust supervisor 与 TypeScript 会话层之间的消息交换。
pub struct StdioBridge {
    /// 从 TS stdout 读取的带缓冲读取器
    reader: BufReader<ChildStdout>,
    /// 写入 TS stdin 的管道
    writer: ChildStdin,
}

impl StdioBridge {
    /// 创建新的 stdio 桥连
    ///
    /// # 参数
    /// - `stdout`: 子进程的 stdout（用于读取 TS 发来的消息）
    /// - `stdin`: 子进程的 stdin（用于向 TS 发送消息）
    #[must_use]
    pub fn new(stdout: ChildStdout, stdin: ChildStdin) -> Self {
        Self {
            reader: BufReader::new(stdout),
            writer: stdin,
        }
    }

    /// 发送 JSON 消息到 TS 进程
    ///
    /// 将消息序列化为 JSON，追加换行符，然后 flush 写入 stdin 管道。
    pub async fn send(&mut self, msg: &impl Serialize) -> Result<(), IpcError> {
        let json = serde_json::to_string(msg)?;
        self.writer.write_all(json.as_bytes()).await?;
        self.writer.write_all(b"\n").await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// 从 TS 进程读取一行 NDJSON 消息
    ///
    /// 读取一行文本，解析为指定类型 `T`。
    /// 返回 `Ok(None)` 表示 EOF（子进程已关闭 stdout）。
    pub async fn recv<T: DeserializeOwned>(&mut self) -> Result<Option<T>, IpcError> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line).await?;
        if n == 0 {
            // EOF：子进程已关闭 stdout
            return Ok(None);
        }

        // 去除尾部换行
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            // 空行，用循环读取下一行（避免 async 递归）
            loop {
                line.clear();
                let n2 = self.reader.read_line(&mut line).await?;
                if n2 == 0 {
                    return Ok(None);
                }
                let trimmed2 = line.trim_end();
                if !trimmed2.is_empty() {
                    let msg: T = serde_json::from_str(trimmed2)?;
                    return Ok(Some(msg));
                }
            }
        }

        let msg: T = serde_json::from_str(trimmed)?;
        Ok(Some(msg))
    }

    /// 发送 keepalive 探测消息
    ///
    /// 向 TS 进程发送 `{"type":"keepalive"}` 消息。
    /// 如果写入失败（broken pipe），说明 TS 进程已退出。
    pub async fn send_keepalive(&mut self) -> Result<(), IpcError> {
        let keepalive = serde_json::json!({"type": "keepalive"});
        self.send(&keepalive).await
    }
}

// ── ExecBridge stdio 会话处理 ──────────────────────────────────────────

/// `ExecBridge` JSON-RPC 2.0 请求
///
/// TS 通过 stdio NDJSON 发送此格式的请求：
/// ```json
/// {"jsonrpc":"2.0","method":"exec.getGitContext","params":{...},"id":"uuid"}
/// ```
#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    /// JSON-RPC 版本，固定 "2.0"
    #[allow(dead_code)]
    jsonrpc: String,
    /// 方法名：exec.getGitContext / exec.execGitCommand / exec.execGitCommandBatch / exec.execCommand / exec.spawnManaged
    method: String,
    /// 方法参数
    params: serde_json::Value,
    /// 请求 ID（用于关联响应）
    id: serde_json::Value,
}

/// 构造 JSON-RPC 2.0 成功响应
fn jsonrpc_ok(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "result": result,
        "id": id
    })
}

/// 构造 JSON-RPC 2.0 错误响应
fn jsonrpc_error(id: &serde_json::Value, code: i32, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "error": {
            "code": code,
            "message": message
        },
        "id": id
    })
}

/// JSON-RPC 错误码
const JSONRPC_METHOD_NOT_FOUND: i32 = -32601;
const JSONRPC_INVALID_PARAMS: i32 = -32602;
const JSONRPC_INTERNAL_ERROR: i32 = -32603;
/// 权限被拒绝（deny 规则或 `DontAsk` 模式）
const JSONRPC_PERMISSION_DENIED: i32 = -32001;
/// 需要用户审批（Ask 状态，TS 侧应展示确认对话框）
const JSONRPC_PERMISSION_ASK: i32 = -32002;

/// `ExecBridge` getGitContext 请求参数
#[derive(Debug, Deserialize)]
struct GetGitContextParams {
    cwd: String,
}

/// `ExecBridge` execGitCommand 请求参数
#[derive(Debug, Deserialize)]
struct ExecGitCommandParams {
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    input: Option<String>,
    /// 权限模式（可选）。None = 跳过检查（向后兼容）。
    /// 取值: "default" | "bypassPermissions" | "dontAsk" | "plan" | "acceptEdits"
    #[serde(default)]
    permission_mode: Option<String>,
}

/// `ExecBridge` execCommand 请求参数
#[derive(Debug, Deserialize)]
struct ExecCommandParams {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    input: Option<String>,
    /// 权限模式（可选）。None = 跳过检查（向后兼容）。
    /// 取值: "default" | "bypassPermissions" | "dontAsk" | "plan" | "acceptEdits"
    #[serde(default)]
    permission_mode: Option<String>,
    /// 是否启用沙箱（默认 false 向后兼容）。
    ///
    /// **诚实边界（P2-3 2026-06-05）**：经由此 `CommandExecutor` 路径时，`true` 当前**仅
    /// 触发 shell 包装**（`build_shell_command`），**不**做 Landlock/Seatbelt/Job-Object
    /// 真实隔离。真实隔离是 supervisor `spawnManaged` 的 `sandbox_mode=enforced` 路径
    /// （依赖平台后端就绪 E-4 Linux / E-5 macOS / E-6 Windows），与此字段不同。
    /// 切勿据此字段名假定本路径已隔离。
    #[serde(default)]
    sandbox: bool,
}

/// `ExecBridge` spawnManaged 请求参数
///
/// `shell` selects the concrete shell binary. `sandbox_mode` opts into
/// `AsyncSandboxRunner::spawn()`; `None` preserves direct managed execution.
#[derive(Debug, Deserialize)]
struct SpawnManagedParams {
    command: String,
    /// Shell 类型（"bash" | "zsh" | "sh" | "powershell" | "pwsh" | "cmd"）。
    /// Sprint 7 阶段 1 前：被静默吞（实际走 `/bin/sh -c` 或 `cmd.exe /C`）。
    /// 阶段 1 后：按值分派到对应 shell binary；未知值报 `JSONRPC_INVALID_PARAMS`。
    shell: String,
    /// 沙箱 bypass 标记。
    /// Sprint 7 阶段 4 前：字段读取但被 log 记录，实际路径无条件 direct spawn（无沙箱）。
    /// 阶段 4 后：与 `sandbox_mode` 联合决定；true → 强制 direct spawn；
    /// false + `sandbox_mode=Enforced` → 走 `AsyncSandboxRunner` 真沙箱。
    #[serde(default, rename = "dangerouslyDisableSandbox")]
    dangerously_disable_sandbox: bool,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout: Option<u64>,
    #[serde(default)]
    env: Option<HashMap<String, String>>,
    /// 权限模式（可选）。None = 跳过检查（向后兼容）。
    #[serde(default)]
    permission_mode: Option<String>,
    /// Sprint 7 阶段 4 新增（2026-04-23）：沙箱模式 opt-in。
    ///
    /// 合法值：
    /// - `None` / `Some("none")` / 未设置 → **默认** → 走 direct `ProcessBuilder` spawn（无沙箱），
    ///   行为跟阶段 4 前完全一致，保证既有调用方（TS `BashTool` 默认路径、Go MCP
    ///   broker stdio transport）不破坏
    /// - `Some("enforced")` → 走 `AsyncSandboxRunner::spawn()` 真沙箱路径
    ///   （Linux Landlock+Seccomp / macOS Seatbelt；Windows 因 DLL init 根因
    ///   未修暂 `PlatformNotSupported` 降级）；若 `dangerously_disable_sandbox=true`
    ///   则此值被忽略，强制 direct spawn
    ///
    /// 默认 `None` 是 "无回归无关联影响" 的关键：用户/调用方显式启用才进入沙箱，
    /// 防止 Linux/macOS 上默认启用后 `BashTool` 命令（写 `/etc/`、访问 LAN 等）被
    /// Landlock/Seatbelt 拒绝导致现网破坏。
    ///
    /// 升级策略（Sprint 8+）：发布稳定版后，TS 侧在 `shouldUseSandbox()` 里根据
    /// `areUnsandboxedCommandsAllowed()` 门控决定传 `"enforced"` 还是 `"none"`，
    /// 默认行为逐步从 None 切换到 Enforced。
    #[serde(default, rename = "sandboxMode")]
    sandbox_mode: Option<String>,
}

/// Sprint 7 阶段 1（2026-04-23）：shell 字段到真实可执行程序的分派。
///
/// 此前 `handle_spawn_managed` 硬编码走 `/bin/sh -c` (Unix) 或 `cmd.exe /C` (Windows)，
/// `parsed.shell` 字段被静默吞（`#[allow(dead_code)]`）。例如 TS 传
/// `shell: "powershell"` 到 Windows 实际跑的却是 cmd.exe。
///
/// # 兼容策略（保证无回归）
///
/// 未知/空 shell 值保持历史默认行为；已知值查 PATH 真分派，**若查不到退回历史默认**，
/// 避免 Windows 无 bash.exe 的机器突然 error。同时 log warn 让异常可观测。
///
/// | `shell` 值 | Unix | Windows |
/// |---|---|---|
/// | `""` / `"sh"` | `/bin/sh -c` | `cmd.exe /C`（查不到 sh.exe） |
/// | `"bash"` | `<bash> -c`；查不到退回 `/bin/sh -c` | `<bash.exe> -c`；查不到退回 `cmd.exe /C`（兼容历史） |
/// | `"zsh"` | `<zsh> -c`；查不到 error | 查不到 error |
/// | `"powershell"` / `"pwsh"` | `<pwsh> -NoProfile -Command`；查不到 error | 先 pwsh.exe → fallback powershell.exe → 最终 `cmd.exe /C`（兼容历史） |
/// | `"cmd"` | error | `cmd.exe /C` |
/// | 其他 | `JSONRPC_INVALID_PARAMS` | 同 Unix |
///
/// # Returns
///
/// `Ok((program, args))` 用于 `ProcessBuilder::new(program).args(&args)`，
/// `args` 末尾已经包含待执行的 `command` 字符串。
/// `Err(msg)` 调用方应返回 `JSONRPC_INVALID_PARAMS`。
/// Phase 2 R4'：从 sandboxed stdout 末尾提取 cwd marker（`__CRABCODE_CWD_BEGIN__<cwd>__CRABCODE_CWD_END__`），
/// 返回 (剥离 marker 后的 stdout, Option<final_cwd>)。
///
/// marker 由 `handle_spawn_managed_sandboxed` 在 enforced 路径下用 `trap EXIT`
/// 注入（2026-05-06 复核审计 FIX-1 修订，原 `;printf` 拼接致 heredoc 挂起 +
/// exit code 丢失，详 doc 附录 D.11.1）：
/// `trap '__rc=$?; printf "\n__CRABCODE_CWD_BEGIN__%s__CRABCODE_CWD_END__" "$(pwd -P)"; exit $__rc' EXIT\n${cmd}`
/// TS 端从 SpawnManagedResult.finalCwd 同步 setCwdState，实现 `cd` 命令的 cwd 传播。
///
/// 设计：用 `rfind` 取最末次匹配（防多次出现取最末），删除 marker 行（含前导 `\n`）。
fn extract_cwd_marker(stdout: &str) -> (String, Option<String>) {
    const BEGIN: &str = "__CRABCODE_CWD_BEGIN__";
    const END: &str = "__CRABCODE_CWD_END__";
    let Some(begin_idx) = stdout.rfind(BEGIN) else {
        return (stdout.to_string(), None);
    };
    let cwd_start = begin_idx + BEGIN.len();
    let Some(rel_end_idx) = stdout[cwd_start..].find(END) else {
        return (stdout.to_string(), None);
    };
    let cwd_end = cwd_start + rel_end_idx;
    let final_cwd = stdout[cwd_start..cwd_end].to_string();
    let after_marker = cwd_end + END.len();
    // 删除从 marker 行开头（最近的 `\n`）到 marker 末尾的整行
    let marker_line_start = stdout[..begin_idx].rfind('\n').unwrap_or(0);
    let mut clean = stdout[..marker_line_start].to_string();
    if after_marker < stdout.len() {
        clean.push_str(&stdout[after_marker..]);
    }
    (clean, Some(final_cwd))
}

fn resolve_shell(shell: &str, command: &str) -> Result<(String, Vec<String>), String> {
    let s = shell.trim().to_ascii_lowercase();
    let cmd = command.to_string();

    // 历史默认路径（用于空值 + 各种 Windows fallback，保证无回归）
    let default_pair = || -> (String, Vec<String>) {
        if cfg!(windows) {
            ("cmd.exe".to_string(), vec!["/C".to_string(), cmd.clone()])
        } else {
            ("/bin/sh".to_string(), vec!["-c".to_string(), cmd.clone()])
        }
    };

    match s.as_str() {
        "" => Ok(default_pair()),
        "sh" => {
            #[cfg(unix)]
            {
                Ok(("/bin/sh".to_string(), vec!["-c".to_string(), cmd]))
            }
            #[cfg(windows)]
            {
                if let Some(path) = which_in_path("sh") {
                    Ok((path, vec!["-c".to_string(), cmd]))
                } else {
                    tracing::warn!(
                        "shell='sh' 但 Windows PATH 未找到 sh.exe，回退 cmd.exe /C（Sprint 7 阶段 1 兼容策略）"
                    );
                    Ok(default_pair())
                }
            }
        }
        "bash" => {
            #[cfg(unix)]
            {
                if let Some(path) = which_in_path("bash") {
                    return Ok((path, vec!["-c".to_string(), cmd]));
                }
                tracing::warn!("shell='bash' 但 PATH 未找到 bash，回退 /bin/sh -c");
                Ok(("/bin/sh".to_string(), vec!["-c".to_string(), cmd]))
            }
            #[cfg(windows)]
            {
                // Sprint 7 阶段 1 兼容策略（2026-04-23）：Windows 下 `shell: "bash"`
                // **不查 PATH**，一律保留阶段 1 前的 `cmd.exe /C` 行为。
                //
                // 根因：Windows PATH 里 bash.exe 常见来源有二，两者都会引入回归：
                //   1. `C:\Windows\System32\bash.exe`：WSL 子系统 wrapper，启动需
                //      WSL distro；若 distro 未装或 WSL 服务未启动则挂起或报错
                //      （本机 bench 中 test_spawn_managed_returns_process_id 即被
                //      此路径挂在 5s progress 超时上）
                //   2. `C:\Program Files\Git\bin\bash.exe`：MSYS/MINGW bash，
                //      对 Windows 路径 (`C:\...`) 的语义跟 POSIX bash 不同
                //
                // TS `processBashCommand.tsx` 在 Windows 非 PowerShell 模式固定传
                // `shell: "bash"`，但阶段 1 前该路径实际走 cmd.exe；UI 展示和
                // 模型提示都已按 cmd 行为校准。切换到真 bash 会引入大量间接 regress。
                //
                // 未来若确有需要显式用 Git bash 或 WSL bash，应引入新 shell 值
                // （如 `"git-bash"` / `"wsl-bash"`）而非重定义 `"bash"` 语义。
                tracing::debug!(
                    "shell='bash' 在 Windows 下走 cmd.exe /C（Sprint 7 阶段 1 兼容策略，避免 WSL/MSYS bash 歧义）"
                );
                Ok(default_pair())
            }
        }
        "zsh" => {
            #[cfg(unix)]
            {
                match which_in_path("zsh") {
                    Some(path) => Ok((path, vec!["-c".to_string(), cmd])),
                    None => Err("shell='zsh' 但 PATH 未找到 zsh 可执行文件".to_string()),
                }
            }
            #[cfg(windows)]
            {
                // zsh on Windows 极其罕见且同样面临 MSYS/WSL 歧义。保守回退 cmd.exe
                // 加 warn，跟 "bash" 策略一致（防止 regression）。
                tracing::warn!(
                    "shell='zsh' 在 Windows 下走 cmd.exe /C（Sprint 7 阶段 1 兼容策略；Windows 原生 zsh 不常见）"
                );
                Ok(default_pair())
            }
        }
        "powershell" | "pwsh" => {
            // 先试 PowerShell 7+（跨平台 pwsh）
            if let Some(path) = which_in_path("pwsh") {
                return Ok((
                    path,
                    vec![
                        "-NoProfile".to_string(),
                        "-Command".to_string(),
                        cmd.clone(),
                    ],
                ));
            }
            #[cfg(windows)]
            {
                // 次试 Windows PowerShell 5.1
                if let Some(path) = which_in_path("powershell") {
                    return Ok((
                        path,
                        vec!["-NoProfile".to_string(), "-Command".to_string(), cmd],
                    ));
                }
                tracing::warn!(
                    "shell='{shell}' 但 Windows PATH 未找到 pwsh.exe / powershell.exe，回退 cmd.exe /C（Sprint 7 阶段 1 兼容策略：保持历史行为）"
                );
                Ok(default_pair())
            }
            #[cfg(unix)]
            {
                let _ = cmd;
                Err(format!(
                    "shell='{shell}' 但 PATH 未找到 pwsh（Unix 下 PowerShell 需要单独安装）"
                ))
            }
        }
        "cmd" => {
            #[cfg(windows)]
            {
                Ok(("cmd.exe".to_string(), vec!["/C".to_string(), cmd]))
            }
            #[cfg(unix)]
            {
                let _ = cmd;
                Err("shell='cmd' 不适用于 Unix 平台".to_string())
            }
        }
        other => Err(format!(
            "不支持的 shell 值: {other:?}（支持 bash/zsh/sh/powershell/pwsh/cmd，或留空使用平台默认）"
        )),
    }
}

/// 在 PATH 中查找可执行文件（Sprint 7 阶段 1 helper）。
///
/// 平台差异：
/// - Unix：分隔符 `:`，文件名无扩展名
/// - Windows：分隔符 `;`，按 `PATHEXT` 的常见扩展名（`.exe`, `.cmd`, `.bat`）+ 无扩展名依次尝试
///
/// 返回第一个命中的绝对路径。所有 shell 名都是简单标识符（无空格），
/// 不存在路径注入或引号转义风险。
fn which_in_path(bin: &str) -> Option<String> {
    let path_var = std::env::var("PATH").ok()?;
    let sep = if cfg!(windows) { ';' } else { ':' };
    #[cfg(windows)]
    let exts: &[&str] = &[".exe", ".cmd", ".bat", ""];
    #[cfg(unix)]
    let exts: &[&str] = &[""];

    for dir in path_var.split(sep) {
        if dir.is_empty() {
            continue;
        }
        for ext in exts {
            let candidate = std::path::Path::new(dir).join(format!("{bin}{ext}"));
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    None
}

/// Git 可执行文件路径查找
///
/// `按优先级依次检查：GIT_PATH` 环境变量 → PATH 中的 git → 常见安装路径
fn find_git_executable() -> String {
    // 1. 环境变量覆盖
    if let Ok(git_path) = std::env::var("GIT_PATH")
        && !git_path.is_empty()
    {
        return git_path;
    }

    // 2. 尝试 which/where 查找
    let which_cmd = if cfg!(windows) { "where" } else { "which" };
    if let Ok(output) = std::process::Command::new(which_cmd).arg("git").output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            // which 可能返回多行，取第一行
            return path.lines().next().unwrap_or("git").to_string();
        }
    }

    // 3. 常见路径兜底
    #[cfg(unix)]
    for path in &[
        "/usr/bin/git",
        "/usr/local/bin/git",
        "/opt/homebrew/bin/git",
    ] {
        if std::path::Path::new(path).exists() {
            return path.to_string();
        }
    }

    // 4. 最终兜底
    "git".to_string()
}

/// 处理 `ExecBridge` stdio 会话
///
/// 从 `StdioBridge` 读取 JSON-RPC 2.0 请求，按 method 路由到对应处理器，
/// 通过 `StdioBridge` 发送 JSON-RPC 2.0 响应。
///
/// 这是 TS↔Rust stdio 通道的 `ExecBridge` 消息分发循环，
/// 功能等价于 Go↔Rust UDS 通道的 `handle_connection` 但走 NDJSON。
///
/// 支持进度通知：spawnManaged 等长运行命令可通过 `progress_tx` 发送
/// JSON-RPC notification（无 id），由本循环统一经 bridge 发出。
///
/// # 生命周期
///
/// 循环直到：
/// - `StdioBridge` 返回 EOF（TS 进程退出）
/// - 收到 shutdown 信号
pub async fn handle_stdio_session(
    bridge: &mut StdioBridge,
    executor: Arc<CommandExecutor>,
    shutdown: CancellationToken,
) {
    // 进度事件通道：后台 spawn 任务通过 tx 发送通知，本循环通过 rx 接收并写入 bridge
    let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<serde_json::Value>(64);

    tracing::info!("ExecBridge stdio 会话开始");
    loop {
        tokio::select! {
            biased;  // 保证分支按声明顺序优先匹配（shutdown > progress > recv）
            // 优先级 1：关闭信号
            () = shutdown.cancelled() => {
                tracing::info!("ExecBridge stdio 会话收到关闭信号");
                break;
            }
            // 优先级 2：后台任务的进度/完成通知
            Some(event) = progress_rx.recv() => {
                if let Err(e) = bridge.send(&event).await {
                    tracing::error!(error = %e, "ExecBridge 进度事件发送失败");
                    break;
                }
            }
            // 优先级 3：来自 TS 的 JSON-RPC 请求
            msg = bridge.recv::<serde_json::Value>() => {
                match msg {
                    Ok(Some(value)) => {
                        // 解析 JSON-RPC 请求
                        match serde_json::from_value::<JsonRpcRequest>(value.clone()) {
                            Ok(req) => {
                                let resp = dispatch_exec_bridge(
                                    &req,
                                    Arc::clone(&executor),
                                    progress_tx.clone(),
                                ).await;
                                if let Err(e) = bridge.send(&resp).await {
                                    tracing::error!(error = %e, "ExecBridge 发送响应失败");
                                    break;
                                }
                            }
                            Err(e) => {
                                // 非 JSON-RPC 消息（可能是 keepalive 等），跳过
                                let msg_type = value.get("type")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                if msg_type == "keepalive" {
                                    tracing::trace!("收到 keepalive，忽略");
                                } else {
                                    tracing::warn!(
                                        error = %e,
                                        msg_type = msg_type,
                                        "ExecBridge 收到非 JSON-RPC 消息，忽略"
                                    );
                                }
                            }
                        }
                    }
                    Ok(None) => {
                        tracing::info!("ExecBridge stdio EOF（TS 进程已退出）");
                        break;
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "ExecBridge stdio 读取错误");
                        break;
                    }
                }
            }
        }
    }
    tracing::info!("ExecBridge stdio 会话结束");
}

// ── ExecBridge 权限检查 ───────────────────────────────────────────────

/// 权限检查结果
enum PermCheckOutcome {
    /// 允许执行
    Allowed,
    /// 被拒绝，附带 JSON-RPC 错误响应
    Blocked(serde_json::Value),
}

/// 解析请求参数中的 `permission_mode` 字符串为 `PermissionMode` 枚举。
/// 返回 None 表示跳过权限检查（向后兼容：请求不含此字段时）。
fn parse_permission_mode(mode_str: Option<&str>) -> Option<PermissionMode> {
    mode_str.map(|s| match s {
        "bypassPermissions" => PermissionMode::BypassPermissions,
        "dontAsk" => PermissionMode::DontAsk,
        "plan" => PermissionMode::Plan,
        "acceptEdits" => PermissionMode::AcceptEdits,
        "auto" => PermissionMode::Auto,
        _ => PermissionMode::Default,
    })
}

/// 将程序名和参数拼接为完整命令字符串，供 `check_bash_permission` 判定。
fn format_full_command(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{} {}", program, args.join(" "))
    }
}

/// 构建 git 宽松权限规则（隐式允许所有 git 命令）。
/// 只有用户显式 deny 规则才能阻断 git 操作。
fn git_lenient_rules() -> Vec<PermissionRule> {
    vec![PermissionRule {
        source: PermissionRuleSource::PolicySettings,
        behavior: PermissionBehavior::Allow,
        value: PermissionRuleValue {
            tool_name: "Bash".to_string(),
            rule_content: Some("git *".to_string()),
        },
    }]
}

/// 对一条完整命令执行权限检查。
///
/// - `full_command`: 完整命令字符串（如 "npm install" 或 "git push"）
/// - `cwd`: 当前工作目录
/// - `mode`: 权限模式
/// - `extra_rules`: 额外注入的规则（如 git 宽松规则）
/// - `id`: JSON-RPC 请求 ID（用于构造错误响应）
fn check_exec_permission(
    full_command: &str,
    cwd: Option<&str>,
    mode: PermissionMode,
    extra_rules: Vec<PermissionRule>,
    id: &serde_json::Value,
) -> PermCheckOutcome {
    let ctx = PermissionContext {
        cwd: cwd.unwrap_or(".").to_string(),
        original_cwd: None,
        mode,
        rules: extra_rules,
        tool_name: "Bash".to_string(),
    };

    match check_bash_permission(full_command, &ctx) {
        PermissionResult::Allow { .. } | PermissionResult::Passthrough => PermCheckOutcome::Allowed,
        PermissionResult::Deny { message, .. } => {
            tracing::warn!(command = full_command, reason = %message, "ExecBridge 权限被拒绝");
            PermCheckOutcome::Blocked(jsonrpc_error(id, JSONRPC_PERMISSION_DENIED, &message))
        }
        PermissionResult::Ask { message, .. } => {
            tracing::info!(command = full_command, reason = %message, "ExecBridge 权限需审批");
            PermCheckOutcome::Blocked(serde_json::json!({
                "jsonrpc": "2.0",
                "error": {
                    "code": JSONRPC_PERMISSION_ASK,
                    "message": message,
                    "data": {
                        "type": "permission_ask",
                        "command": full_command
                    }
                },
                "id": id
            }))
        }
    }
}

/// `ExecBridge` 方法路由分发
///
/// 按 JSON-RPC method 字段路由到对应处理器：
/// - `exec.getGitContext` → 3 个 git 命令并行
/// - `exec.execGitCommand` → 单 git 命令
/// - `exec.execGitCommandBatch` → N 个 git 命令并行
/// - `exec.execCommand` → 通用命令
/// - `exec.spawnManaged` → 受管进程启动（异步，通过 `progress_tx` 发送完成通知）
/// - `exec.abort` → 终止受管进程
async fn dispatch_exec_bridge(
    req: &JsonRpcRequest,
    executor: Arc<CommandExecutor>,
    progress_tx: tokio::sync::mpsc::Sender<serde_json::Value>,
) -> serde_json::Value {
    match req.method.as_str() {
        "exec.getGitContext" => handle_get_git_context(&req.params, &req.id, &executor).await,
        "exec.execGitCommand" => handle_exec_git_command(&req.params, &req.id, &executor).await,
        "exec.execGitCommandBatch" => {
            handle_exec_git_command_batch(&req.params, &req.id, executor).await
        }
        "exec.execCommand" => handle_exec_command(&req.params, &req.id, &executor).await,
        "exec.spawnManaged" => {
            handle_spawn_managed(&req.params, &req.id, executor, progress_tx).await
        }
        "exec.abort" => handle_exec_abort(&req.params, &req.id, &executor).await,
        _ => {
            tracing::warn!(method = %req.method, "未知 ExecBridge 方法");
            jsonrpc_error(
                &req.id,
                JSONRPC_METHOD_NOT_FOUND,
                &format!("未知方法: {}", req.method),
            )
        }
    }
}

/// 执行 `ExecReq` 并转换为 JSON 结果格式
///
/// 统一的执行路径：构造 `ExecReq` → `executor.execute()` → 转换结果
// 9 args mirror the wire-level RPC payload one-to-one; bundling into a struct
// would just rename fields without simplifying the call sites.
#[allow(clippy::too_many_arguments)]
async fn execute_and_format(
    program: &str,
    args: Vec<String>,
    family: CommandFamily,
    cwd: Option<String>,
    env: HashMap<String, String>,
    stdin: Option<String>,
    timeout_ms: u64,
    sandbox: bool,
    executor: &CommandExecutor,
) -> serde_json::Value {
    let exec_req = ExecReq {
        family,
        program: program.to_string(),
        args,
        cwd: cwd.filter(|s| !s.is_empty()),
        env,
        stdin,
        timeout_ms,
        sandbox,
        abort_token: None,
    };

    let result = executor.execute(exec_req).await;

    serde_json::json!({
        "stdout": result.stdout,
        "stderr": result.stderr,
        "code": result.code,
        "error": result.error
    })
}

/// 处理 exec.getGitContext — 批量获取 git 上下文
///
/// 并行执行 3 个 git 命令：status --short, log --oneline -n 5, config user.name
/// 结果拼装为 { status, log, userName }
async fn handle_get_git_context(
    params: &serde_json::Value,
    id: &serde_json::Value,
    executor: &CommandExecutor,
) -> serde_json::Value {
    let parsed: GetGitContextParams = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => return jsonrpc_error(id, JSONRPC_INVALID_PARAMS, &format!("参数解析失败: {e}")),
    };

    let git = find_git_executable();
    let cwd = parsed.cwd;

    // 构造 3 个并行 git 命令
    let status_req = ExecReq {
        family: CommandFamily::Git,
        program: git.clone(),
        args: vec![
            "--no-optional-locks".into(),
            "status".into(),
            "--short".into(),
        ],
        cwd: Some(cwd.clone()),
        env: HashMap::new(),
        stdin: None,
        timeout_ms: 10_000,
        sandbox: false,
        abort_token: None,
    };

    let log_req = ExecReq {
        family: CommandFamily::Git,
        program: git.clone(),
        args: vec![
            "--no-optional-locks".into(),
            "log".into(),
            "--oneline".into(),
            "-n".into(),
            "5".into(),
        ],
        cwd: Some(cwd.clone()),
        env: HashMap::new(),
        stdin: None,
        timeout_ms: 10_000,
        sandbox: false,
        abort_token: None,
    };

    let username_req = ExecReq {
        family: CommandFamily::Git,
        program: git,
        args: vec!["config".into(), "user.name".into()],
        cwd: Some(cwd),
        env: HashMap::new(),
        stdin: None,
        timeout_ms: 10_000,
        sandbox: false,
        abort_token: None,
    };

    // 并行执行
    let (status_result, log_result, username_result) = tokio::join!(
        executor.execute(status_req),
        executor.execute(log_req),
        executor.execute(username_req),
    );

    jsonrpc_ok(
        id,
        serde_json::json!({
            "status": status_result.stdout.trim_end(),
            "log": log_result.stdout.trim_end(),
            "userName": username_result.stdout.trim_end()
        }),
    )
}

/// 处理 exec.execGitCommand — 单个 git 命令
///
/// 当请求携带 `permission_mode` 时，使用宽松模式检查（隐式 allow `git *`）。
/// 只有用户显式 deny 规则才能阻断 git 操作。
async fn handle_exec_git_command(
    params: &serde_json::Value,
    id: &serde_json::Value,
    executor: &CommandExecutor,
) -> serde_json::Value {
    let parsed: ExecGitCommandParams = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => return jsonrpc_error(id, JSONRPC_INVALID_PARAMS, &format!("参数解析失败: {e}")),
    };

    // 权限检查（宽松模式：注入隐式 git allow 规则）
    if let Some(mode) = parse_permission_mode(parsed.permission_mode.as_deref()) {
        let full_cmd = format_full_command("git", &parsed.args);
        if let PermCheckOutcome::Blocked(resp) = check_exec_permission(
            &full_cmd,
            parsed.cwd.as_deref(),
            mode,
            git_lenient_rules(),
            id,
        ) {
            return resp;
        }
    }

    let git = find_git_executable();
    let result = execute_and_format(
        &git,
        parsed.args,
        CommandFamily::Git,
        parsed.cwd,
        parsed.env.unwrap_or_default(),
        parsed.input,
        parsed.timeout.unwrap_or(30_000),
        false,
        executor,
    )
    .await;

    jsonrpc_ok(id, result)
}

/// 处理 exec.execGitCommandBatch — 批量 git 命令并行
///
/// 接受 `Arc<CommandExecutor>` 以便 spawn 并行任务。
/// 当任一请求携带 `permission_mode` 时，逐条检查后再执行。
async fn handle_exec_git_command_batch(
    params: &serde_json::Value,
    id: &serde_json::Value,
    executor: Arc<CommandExecutor>,
) -> serde_json::Value {
    // params 是请求数组
    let requests: Vec<ExecGitCommandParams> = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => return jsonrpc_error(id, JSONRPC_INVALID_PARAMS, &format!("参数解析失败: {e}")),
    };

    // 逐条权限检查（宽松模式）—— 在 spawn 并行任务之前完成
    for req in &requests {
        if let Some(mode) = parse_permission_mode(req.permission_mode.as_deref()) {
            let full_cmd = format_full_command("git", &req.args);
            if let PermCheckOutcome::Blocked(resp) =
                check_exec_permission(&full_cmd, req.cwd.as_deref(), mode, git_lenient_rules(), id)
            {
                return resp;
            }
        }
    }

    let git = find_git_executable();
    let count = requests.len();

    // spawn 并行任务，每个任务返回 (index, result) 以保持顺序
    let mut join_handles = Vec::with_capacity(count);
    for (idx, req) in requests.into_iter().enumerate() {
        let exec_req = ExecReq {
            family: CommandFamily::Git,
            program: git.clone(),
            args: req.args,
            cwd: req.cwd.filter(|s| !s.is_empty()),
            env: req.env.unwrap_or_default(),
            stdin: req.input,
            timeout_ms: req.timeout.unwrap_or(30_000),
            sandbox: false,
            abort_token: None,
        };
        let executor = Arc::clone(&executor);
        // JoinHandle held by caller — pushed onto `join_handles` Vec
        // and awaited together below. Bare-discard regression audited
        // Step 2 Phase D.1.
        #[allow(clippy::disallowed_methods)]
        join_handles.push(tokio::spawn(async move {
            (idx, executor.execute(exec_req).await)
        }));
    }

    // 收集结果并按索引排序
    let mut indexed_results: Vec<(usize, acosmi_types::exec_types::ExecResult)> =
        Vec::with_capacity(count);
    for handle in join_handles {
        match handle.await {
            Ok((idx, result)) => indexed_results.push((idx, result)),
            Err(e) => {
                tracing::error!(error = %e, "batch 任务 panic");
                return jsonrpc_error(id, JSONRPC_INTERNAL_ERROR, &format!("batch 任务失败: {e}"));
            }
        }
    }
    indexed_results.sort_by_key(|(idx, _)| *idx);

    let json_results: Vec<serde_json::Value> = indexed_results
        .into_iter()
        .map(|(_, r)| {
            serde_json::json!({
                "stdout": r.stdout,
                "stderr": r.stderr,
                "code": r.code,
                "error": r.error
            })
        })
        .collect();

    jsonrpc_ok(id, serde_json::json!(json_results))
}

/// 处理 exec.execCommand — 通用外部命令
///
/// 当请求携带 `permission_mode` 时，执行前先做权限检查（严格模式，无隐式规则）。
async fn handle_exec_command(
    params: &serde_json::Value,
    id: &serde_json::Value,
    executor: &CommandExecutor,
) -> serde_json::Value {
    let parsed: ExecCommandParams = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => return jsonrpc_error(id, JSONRPC_INVALID_PARAMS, &format!("参数解析失败: {e}")),
    };

    // 权限检查（严格模式：无隐式 allow 规则）
    if let Some(mode) = parse_permission_mode(parsed.permission_mode.as_deref()) {
        let full_cmd = format_full_command(&parsed.command, &parsed.args);
        if let PermCheckOutcome::Blocked(resp) =
            check_exec_permission(&full_cmd, parsed.cwd.as_deref(), mode, vec![], id)
        {
            return resp;
        }
    }

    let result = execute_and_format(
        &parsed.command,
        parsed.args,
        CommandFamily::Other(parsed.command.clone()),
        parsed.cwd,
        parsed.env.unwrap_or_default(),
        parsed.input,
        parsed.timeout.unwrap_or(30_000),
        parsed.sandbox,
        executor,
    )
    .await;

    jsonrpc_ok(id, result)
}

/// 处理 exec.spawnManaged — 受管进程异步启动（支持逐行进度流）
///
/// 启动一个长运行受管进程，立即返回 `process_id` 和 `abort_token`。
/// stdout/stderr 逐行读取后通过 `progress_tx` 发送 `exec.progress` 通知。
/// 进程完成后通过 `progress_tx` 发送 `exec.completed` 通知。
/// 所有 `exec.progress` 保证在 `exec.completed` 之前发送。
/// 通过 `exec.abort` 请求可终止进行中的进程。
async fn handle_spawn_managed(
    params: &serde_json::Value,
    id: &serde_json::Value,
    executor: Arc<CommandExecutor>,
    progress_tx: tokio::sync::mpsc::Sender<serde_json::Value>,
) -> serde_json::Value {
    let parsed: SpawnManagedParams = match serde_json::from_value(params.clone()) {
        Ok(p) => p,
        Err(e) => return jsonrpc_error(id, JSONRPC_INVALID_PARAMS, &format!("参数解析失败: {e}")),
    };

    // Sprint 7 阶段 0（2026-04-23）：让 shell / dangerously_disable_sandbox 字段
    // 从 dead_code 变"至少被观察到"，便于定位阶段 1/4 改动前的真实调用分布。
    // 阶段 1 完成后 `shell` 字段会实际参与 shell 分派；阶段 4 完成后
    // `dangerously_disable_sandbox` 会与 sandbox_mode 联合决定是否走真沙箱。
    // 当前（阶段 0）两字段仍不改变实际行为 —— 下方代码硬编码 sh/cmd 并直接
    // ProcessBuilder spawn（无沙箱），维持既有语义，确保无回归。
    tracing::debug!(
        command = %parsed.command,
        shell = %parsed.shell,
        dangerously_disable_sandbox = parsed.dangerously_disable_sandbox,
        cwd = ?parsed.cwd,
        permission_mode = ?parsed.permission_mode,
        "spawnManaged 请求（Sprint 7 阶段 0：字段已纳入观察，行为未变）"
    );

    // 权限检查（严格模式）—— 只传真实命令名，不附加 shell 元信息
    if let Some(mode) = parse_permission_mode(parsed.permission_mode.as_deref())
        && let PermCheckOutcome::Blocked(resp) =
            check_exec_permission(&parsed.command, parsed.cwd.as_deref(), mode, vec![], id)
    {
        return resp;
    }

    // 生成唯一标识：原子计数器 + 纳秒时间戳，消除碰撞风险
    static SPAWN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SPAWN_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ts = {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    };
    let process_id = format!("managed-{ts:013x}-{seq:04x}");
    let abort_token = process_id.clone();

    let timeout_ms = parsed.timeout.unwrap_or(120_000);

    // Sprint 7 阶段 1（2026-04-23）：shell 字段分派，取代原硬编码 /bin/sh / cmd.exe。
    // 根因：`parsed.shell` 字段此前 `#[allow(dead_code)]`，Windows 上 `shell: "powershell"`
    // 被静默降级为 cmd.exe。现在按值分派真实 shell binary，查不到时按兼容策略回退
    // 历史默认，避免无 bash.exe / pwsh 的 Windows 机器突然 error（见 `resolve_shell`
    // 文档）。
    let (exec_program, exec_args) = match resolve_shell(&parsed.shell, &parsed.command) {
        Ok(pair) => pair,
        Err(msg) => return jsonrpc_error(id, JSONRPC_INVALID_PARAMS, &msg),
    };

    tracing::debug!(
        exec_program = %exec_program,
        shell_requested = %parsed.shell,
        "spawnManaged shell 分派完成（Sprint 7 阶段 1）"
    );

    // Sprint 7 阶段 4（2026-04-23）：sandbox_mode opt-in 分派。
    //
    // 合法取值（大小写不敏感）：
    //   - None / "" / "none" / 未设置 → **默认路径**，走下方 direct ProcessBuilder
    //     spawn（无任何沙箱隔离），行为跟阶段 4 前完全一致 —— 无回归
    //   - "enforced" + `!dangerously_disable_sandbox` → 走 AsyncSandboxRunner 真沙箱
    //     （Linux Landlock+Seccomp / macOS Seatbelt；Windows 暂 PlatformNotSupported
    //     因 STATUS_DLL_INIT_FAILED 根因未修，自动降级到 direct）
    //   - `dangerously_disable_sandbox=true` 不管 sandbox_mode 是什么都走 direct
    //     （对应 TS `!cmd` 用户主动 bypass 场景）
    let sandbox_mode_normalized = parsed
        .sandbox_mode
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let want_sandbox = sandbox_mode_normalized == "enforced" && !parsed.dangerously_disable_sandbox;

    if want_sandbox {
        tracing::info!(
            process_id = %process_id,
            command = %parsed.command,
            "Sprint 7 阶段 4：sandbox_mode=enforced，尝试 AsyncSandboxRunner"
        );
        return handle_spawn_managed_sandboxed(
            &exec_program,
            &exec_args,
            &parsed,
            process_id,
            abort_token,
            timeout_ms,
            id,
            executor,
            progress_tx,
        )
        .await;
    }

    let mut builder = acosmi_exec::ProcessBuilder::new(&exec_program)
        .args(&exec_args)
        .stdout(acosmi_exec::OutputConfig::Piped)
        .stderr(acosmi_exec::OutputConfig::Piped)
        .stdin(acosmi_exec::StdinConfig::Null)
        .new_process_group(true)
        .envs(&parsed.env.clone().unwrap_or_default());

    if let Some(ref cwd) = parsed.cwd
        && !cwd.is_empty()
    {
        builder = builder.cwd(cwd);
    }

    let mut proc = match builder.spawn().await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, command = %parsed.command, "spawnManaged 进程启动失败");
            return jsonrpc_error(id, JSONRPC_INTERNAL_ERROR, &format!("进程启动失败: {e}"));
        }
    };

    let pid = proc.pid;

    // 注册 abort_token 以支持 exec.abort 终止
    executor.register_abort(abort_token.clone(), pid).await;

    // 在后台执行：逐行读取 stdout/stderr → 发送 exec.progress → 最终发送 exec.completed
    let proc_id = process_id.clone();
    let abort_token_inner = abort_token.clone();
    // Step 2 Phase D.2: spawn-managed (non-sandbox) progress reporter
    // tracked through the process-global registry; panics surface at
    // error level. Closes Step 1 §六 R1 ① for ipc.rs:2497.
    crate::task_registry::global().spawn("supervisor.ipc.spawn_managed_progress", async move {
        let abort_token = abort_token_inner;
        let start = std::time::Instant::now();

        // 取出 stdout/stderr 句柄
        let stdout_handle = proc.take_stdout();
        let stderr_handle = proc.take_stderr();

        // stdout 逐行读取 → exec.progress
        let stdout_progress_tx = progress_tx.clone();
        let stdout_proc_id = proc_id.clone();
        // JoinHandle held by caller — `stdout_task` is awaited below.
        // Bare-discard regression audited Step 2 Phase D.1.
        #[allow(clippy::disallowed_methods)]
        let stdout_task = tokio::spawn(async move {
            let mut collected = String::new();
            let mut line_count: u64 = 0;
            if let Some(reader) = stdout_handle {
                let mut buf_reader = BufReader::new(reader);
                let mut line_buf = String::new();
                loop {
                    line_buf.clear();
                    match buf_reader.read_line(&mut line_buf).await {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            let line = line_buf.trim_end_matches('\n').trim_end_matches('\r');
                            line_count += 1;
                            collected.push_str(line);
                            collected.push('\n');
                            // 发送 exec.progress notification
                            let notification = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "exec.progress",
                                "params": {
                                    "process_id": stdout_proc_id,
                                    "stream": "stdout",
                                    "data": line,
                                    "line_number": line_count,
                                }
                            });
                            let _ = stdout_progress_tx.send(notification).await;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "spawnManaged stdout 读取错误");
                            break;
                        }
                    }
                }
            }
            collected
        });

        // stderr 逐行读取 → exec.progress
        let stderr_progress_tx = progress_tx.clone();
        let stderr_proc_id = proc_id.clone();
        // JoinHandle held by caller — `stderr_task` awaited below.
        // Bare-discard regression audited Step 2 Phase D.1.
        #[allow(clippy::disallowed_methods)]
        let stderr_task = tokio::spawn(async move {
            let mut collected = String::new();
            let mut line_count: u64 = 0;
            if let Some(reader) = stderr_handle {
                let mut buf_reader = BufReader::new(reader);
                let mut line_buf = String::new();
                loop {
                    line_buf.clear();
                    match buf_reader.read_line(&mut line_buf).await {
                        Ok(0) => break, // EOF
                        Ok(_) => {
                            let line = line_buf.trim_end_matches('\n').trim_end_matches('\r');
                            line_count += 1;
                            collected.push_str(line);
                            collected.push('\n');
                            let notification = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "exec.progress",
                                "params": {
                                    "process_id": stderr_proc_id,
                                    "stream": "stderr",
                                    "data": line,
                                    "line_number": line_count,
                                }
                            });
                            let _ = stderr_progress_tx.send(notification).await;
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "spawnManaged stderr 读取错误");
                            break;
                        }
                    }
                }
            }
            collected
        });

        // 等待进程退出（带超时）
        let wait_result = if timeout_ms > 0 {
            let duration = std::time::Duration::from_millis(timeout_ms);
            if let Ok(result) = tokio::time::timeout(duration, proc.wait()).await {
                Some(result)
            } else {
                // 超时 — 终止进程树
                let _ = proc.kill_tree().await;
                None
            }
        } else {
            Some(proc.wait().await)
        };

        // 等待 stdout/stderr 收集完成
        // Step 2 Phase D.6 / Step 1 §六 R1 ④: replaces
        // `task.await.unwrap_or_default()` which folded JoinError (task
        // panic / abort) into the same empty value as a successful but
        // empty collection. `await_or_log` distinguishes the three
        // variants and traces panic / abort while still returning a
        // usable Default for downstream framing.
        let stdout = crate::task_registry::await_or_log(
            stdout_task,
            "supervisor.ipc.spawn_managed_progress.stdout",
        )
        .await;
        let stderr = crate::task_registry::await_or_log(
            stderr_task,
            "supervisor.ipc.spawn_managed_progress.stderr",
        )
        .await;

        let duration_ms = start.elapsed().as_millis() as u64;

        let (code, timed_out, killed) = match wait_result {
            Some(Ok(code)) => (code, false, false),
            Some(Err(e)) => {
                tracing::warn!(error = %e, "spawnManaged 进程等待失败");
                (-1, false, false)
            }
            None => {
                // 超时终止
                (-1, true, true)
            }
        };

        // 清理 abort_token 注册
        executor.unregister_abort(&abort_token).await;

        tracing::info!(
            process_id = %proc_id,
            code,
            duration_ms,
            timed_out,
            "spawnManaged 进程完成"
        );

        // 发送 exec.completed 通知（JSON-RPC notification，无 id）
        // 保证在所有 exec.progress 之后发送
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exec.completed",
            "params": {
                "process_id": proc_id,
                "stdout": stdout,
                "stderr": stderr,
                "code": code,
                "timed_out": timed_out,
                "killed": killed,
                "duration_ms": duration_ms,
                // Sprint 7 阶段 4：让调用方知道**真实**沙箱后端。direct 分支
                // 明确返 "none"，避免以前 Sprint 6 "已动态" 虚报那种模糊状态。
                "sandbox_backend": "none",
            }
        });
        // 忽略发送失败（会话可能已关闭）
        let _ = progress_tx.send(notification).await;
    });

    tracing::info!(
        process_id = %process_id,
        command = %parsed.command,
        "spawnManaged 进程已后台启动"
    );

    jsonrpc_ok(
        id,
        serde_json::json!({
            "process_id": process_id,
            "abort_token": abort_token,
            "status": "spawned",
            // Sprint 7 阶段 4：immediate response 也带 sandbox_backend，
            // 让调用方在收到 progress notification 前就能知道路径选择
            "sandbox_backend": "none"
        }),
    )
}

/// Sprint 7 阶段 4（2026-04-23）：`handle_spawn_managed` 的 sandbox 分支。
///
/// 当 `sandbox_mode == "enforced"` 且 `!dangerously_disable_sandbox` 时进入。
/// 流程：
///   1. 构造 `SandboxConfig`（workspace + command/args + env + timeout 等）
///   2. `acosmi_sandbox::select_async_runner` 选可用 backend（Linux/macOS 有，
///      Windows/Docker 暂返 Err）
///   3. `runner.spawn(config).await` 拿 `SandboxedProcess`（pid + async stdout
///      /stderr + wait future + abort closure）
///   4. 后台 tokio task 逐行读 stdout/stderr → 发 `exec.progress`；wait 完成 →
///      发 `exec.completed`（含真实 `sandbox_backend` 名）
///   5. 返回 `{process_id, abort_token, status: "spawned", sandbox_backend}`
///
/// 失败路径（`select_async_runner` Err / spawn Err）：返 `JSONRPC_INTERNAL_ERROR`，
/// **不静默降级到 direct** —— 因为调用方明确要求 enforced，静默降级违反契约。
/// 调用方应根据错误决定是否带 `dangerouslyDisableSandbox:true` 重试。
#[allow(clippy::too_many_arguments)]
async fn handle_spawn_managed_sandboxed(
    exec_program: &str,
    exec_args: &[String],
    parsed: &SpawnManagedParams,
    process_id: String,
    abort_token: String,
    timeout_ms: u64,
    id: &serde_json::Value,
    executor: Arc<CommandExecutor>,
    progress_tx: tokio::sync::mpsc::Sender<serde_json::Value>,
) -> serde_json::Value {
    use acosmi_sandbox::config::{
        BackendPreference, OutputFormat, ResourceLimits, SandboxConfig, SecurityLevel,
    };

    // 构造 SandboxConfig
    let workspace = parsed
        .cwd
        .as_ref()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(std::env::temp_dir);

    let mut env_map = std::collections::HashMap::new();
    if let Some(ref env) = parsed.env {
        env_map.extend(env.iter().map(|(k, v)| (k.clone(), v.clone())));
    }

    // Phase 2 R4'：注入 cwd marker 让 TS 端在 enforced 路径下能同步 setCwdState。
    // 仅当 exec_args 形如 `["-c", cmd]`（POSIX shell, sh/bash/zsh，详 resolve_shell）
    // 时改造；非 POSIX 形态（cmd.exe /C 等）不注入 — enforced 路径目前只走
    // macOS Seatbelt + Linux Landlock，Windows enforced 已在外层 PlatformNotSupported err。
    //
    // 2026-05-06 复核审计修订（HIGH-1 + HIGH-2 + RC-LOW）：注入策略从 `${cmd};printf ...`
    // 改为 `trap '__rc=$?; printf ...; exit $__rc' EXIT\n${cmd}`。原 `;printf` 拼接两个致命缺陷：
    //   - HIGH-1：cmd 含 heredoc（如 `cat <<EOF\n...\nEOF`）时，`EOF;printf` 不独占
    //     一行，bash 不识别 heredoc 终止符 → 命令挂起
    //   - HIGH-2：整体 exit code = printf 的 0；原 ${cmd} 真退出码（如 cargo build 的 1）
    //     被吞 → BashTool 收到 success 假信号
    // trap EXIT 同步修复双 bug：trap 在 shell 退出前自动触发，heredoc 在 trap 行后正常
    // 解析；`__rc=$?` 捕获 ${cmd} 真退出码，`exit $__rc` 透传。详主真源 doc 附录 D.11。
    let exec_args_with_marker: Vec<String> = if exec_args.len() == 2 && exec_args[0] == "-c" {
        let wrapped_cmd = format!(
            "trap '__rc=$?; printf \"\\n__CRABCODE_CWD_BEGIN__%s__CRABCODE_CWD_END__\" \"$(pwd -P)\"; exit $__rc' EXIT\n{}",
            exec_args[1]
        );
        vec![exec_args[0].clone(), wrapped_cmd]
    } else {
        // RC-LOW（2026-05-06 审计）：非 -c 形态时 cwd marker 不注入。仅 cmd.exe /C
        // 等形态命中此分支；本期 enforced 不走 Windows，未来 resolve_shell 加新 shell
        // 形态时此 silent skip 应可观测。
        tracing::warn!(
            shell = %parsed.shell,
            exec_program = %exec_program,
            "exec_args 非 -c 形态，cwd marker 不注入（finalCwd 将为 None）"
        );
        exec_args.to_vec()
    };

    let sandbox_config = SandboxConfig {
        security_level: SecurityLevel::L1Allowlist,
        command: exec_program.to_string(),
        args: exec_args_with_marker,
        workspace,
        mounts: vec![],
        resource_limits: ResourceLimits {
            timeout_secs: if timeout_ms > 0 {
                Some(timeout_ms.div_ceil(1000))
            } else {
                None
            },
            ..ResourceLimits::default()
        },
        network_policy: None,
        env_vars: env_map,
        format: OutputFormat::Json,
        backend: BackendPreference::Auto,
    };

    // 选择 async runner
    let runner = match acosmi_sandbox::select_async_runner(&sandbox_config) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                error = %e,
                command = %parsed.command,
                "Sprint 7 阶段 4：select_async_runner 失败，拒绝降级（契约要求 enforced）"
            );
            return jsonrpc_error(
                id,
                JSONRPC_INTERNAL_ERROR,
                &format!(
                    "sandbox_mode=enforced 但当前平台无可用 async runner: {e}。\
                     若需 bypass 请改传 dangerouslyDisableSandbox: true。"
                ),
            );
        }
    };

    let backend_name = runner.name().to_string();
    tracing::info!(
        process_id = %process_id,
        backend = %backend_name,
        "Sprint 7 阶段 4：选中 async runner，spawn sandboxed process"
    );

    // spawn 沙箱进程
    let sandboxed = match runner.spawn(&sandbox_config).await {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                error = %e,
                command = %parsed.command,
                "Sprint 7 阶段 4：sandbox spawn 失败"
            );
            return jsonrpc_error(
                id,
                JSONRPC_INTERNAL_ERROR,
                &format!("sandboxed spawn failed: {e}"),
            );
        }
    };

    let pid = sandboxed.pid;
    executor.register_abort(abort_token.clone(), pid).await;

    // 后台任务：逐行读 stdout/stderr + wait → 最终 exec.completed
    let proc_id = process_id.clone();
    let abort_token_inner = abort_token.clone();
    let backend_inner = backend_name.clone();
    // Step 2 Phase D.2: spawn-managed (sandbox) progress reporter
    // tracked through process-global registry. Closes Step 1 §六 R1 ①
    // for ipc.rs:2782.
    crate::task_registry::global().spawn(
        "supervisor.ipc.spawn_managed_sandbox_progress",
        async move {
            let abort_token = abort_token_inner;
            let start = std::time::Instant::now();
            // Sprint 7 阶段 6 深度复核修（2026-04-23）：destructure 必须把 `_guards`
            // 也 move 进闭包。之前用 `..` 跳过 `_guards`，destructure 完成时
            // `SandboxedProcess` 其余字段（含 `_guards`）立即 drop —— Linux 下
            // cgroup Arc 释放最后一份引用后 cgroup 被清理，子进程失去资源限制。
            //
            // 绑定顺序 = 声明顺序；Rust 本地变量 drop 顺序为**反向声明顺序**。
            // 把 `_sandbox_guards` 放**最前**（最先绑定 → 闭包结束时最后 drop），
            // 保证 guards 生命周期 >= wait/child 进程生命周期。
            let acosmi_sandbox::SandboxedProcess {
                _guards: _sandbox_guards,
                stdout,
                stderr,
                wait,
                abort,
                ..
            } = sandboxed;

            // stdout 逐行 → exec.progress
            let stdout_progress_tx = progress_tx.clone();
            let stdout_proc_id = proc_id.clone();
            // JoinHandle held by caller — sandbox-path counterpart of line
            // 2508; awaited below. Bare-discard regression audited.
            #[allow(clippy::disallowed_methods)]
            let stdout_task = tokio::spawn(async move {
                let mut collected = String::new();
                let mut line_count: u64 = 0;
                if let Some(reader) = stdout {
                    let mut buf_reader = BufReader::new(reader);
                    let mut line_buf = String::new();
                    loop {
                        line_buf.clear();
                        match buf_reader.read_line(&mut line_buf).await {
                            Ok(0) => break,
                            Ok(_) => {
                                let line = line_buf.trim_end_matches('\n').trim_end_matches('\r');
                                line_count += 1;
                                collected.push_str(line);
                                collected.push('\n');
                                let notification = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "exec.progress",
                                    "params": {
                                        "process_id": stdout_proc_id,
                                        "stream": "stdout",
                                        "data": line,
                                        "line_number": line_count,
                                    }
                                });
                                let _ = stdout_progress_tx.send(notification).await;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "sandboxed stdout 读取错误");
                                break;
                            }
                        }
                    }
                }
                collected
            });

            // stderr 逐行 → exec.progress
            let stderr_progress_tx = progress_tx.clone();
            let stderr_proc_id = proc_id.clone();
            // JoinHandle held by caller — sandbox-path counterpart of line
            // 2549. Audited Step 2 Phase D.1.
            #[allow(clippy::disallowed_methods)]
            let stderr_task = tokio::spawn(async move {
                let mut collected = String::new();
                let mut line_count: u64 = 0;
                if let Some(reader) = stderr {
                    let mut buf_reader = BufReader::new(reader);
                    let mut line_buf = String::new();
                    loop {
                        line_buf.clear();
                        match buf_reader.read_line(&mut line_buf).await {
                            Ok(0) => break,
                            Ok(_) => {
                                let line = line_buf.trim_end_matches('\n').trim_end_matches('\r');
                                line_count += 1;
                                collected.push_str(line);
                                collected.push('\n');
                                let notification = serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "method": "exec.progress",
                                    "params": {
                                        "process_id": stderr_proc_id,
                                        "stream": "stderr",
                                        "data": line,
                                        "line_number": line_count,
                                    }
                                });
                                let _ = stderr_progress_tx.send(notification).await;
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "sandboxed stderr 读取错误");
                                break;
                            }
                        }
                    }
                }
                collected
            });

            // wait with timeout（用 tokio::time::timeout 包装 SandboxedProcess.wait）
            let wait_result = if timeout_ms > 0 {
                let duration = std::time::Duration::from_millis(timeout_ms);
                if let Ok(inner) = tokio::time::timeout(duration, wait).await {
                    Some(inner)
                } else {
                    // 超时 → abort
                    abort().await;
                    None
                }
            } else {
                Some(wait.await)
            };

            // Step 2 Phase D.6 / Step 1 §六 R1 ④: same as the non-sandbox
            // path above (line 2637/2638-equivalent). See `await_or_log` doc.
            let stdout_collected = crate::task_registry::await_or_log(
                stdout_task,
                "supervisor.ipc.spawn_managed_sandbox_progress.stdout",
            )
            .await;
            let stderr_collected = crate::task_registry::await_or_log(
                stderr_task,
                "supervisor.ipc.spawn_managed_sandbox_progress.stderr",
            )
            .await;
            let duration_ms = start.elapsed().as_millis() as u64;

            let (code, timed_out, killed) = match wait_result {
                Some(Ok(code)) => (code, false, false),
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "sandboxed wait 失败");
                    (-1, false, false)
                }
                None => (-1, true, true),
            };

            executor.unregister_abort(&abort_token).await;

            // Phase 2 R4'：解析 cwd marker 提取 final_cwd（若有），从 stdout 剥离 marker 行
            let (stdout_clean, final_cwd) = extract_cwd_marker(&stdout_collected);

            tracing::info!(
                process_id = %proc_id,
                backend = %backend_inner,
                code,
                duration_ms,
                timed_out,
                final_cwd = ?final_cwd,
                "Sprint 7 阶段 4：sandboxed 进程完成"
            );

            let notification = serde_json::json!({
                "jsonrpc": "2.0",
                "method": "exec.completed",
                "params": {
                    "process_id": proc_id,
                    "stdout": stdout_clean,
                    "stderr": stderr_collected,
                    "code": code,
                    "timed_out": timed_out,
                    "killed": killed,
                    "duration_ms": duration_ms,
                    "sandbox_backend": backend_inner,
                    "final_cwd": final_cwd,
                }
            });
            let _ = progress_tx.send(notification).await;
        },
    );

    jsonrpc_ok(
        id,
        serde_json::json!({
            "process_id": process_id,
            "abort_token": abort_token,
            "status": "spawned",
            "sandbox_backend": backend_name,
        }),
    )
}

/// 处理 exec.abort — 终止受管进程
///
/// 通过 `abort_token` 查找并终止正在运行的进程（包括进程树）。
async fn handle_exec_abort(
    params: &serde_json::Value,
    id: &serde_json::Value,
    executor: &CommandExecutor,
) -> serde_json::Value {
    let abort_token = match params.get("abort_token").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return jsonrpc_error(id, JSONRPC_INVALID_PARAMS, "缺少 abort_token 参数"),
    };

    let killed = executor.abort(abort_token).await;
    tracing::info!(abort_token, killed, "exec.abort 处理完成");
    jsonrpc_ok(id, serde_json::json!({ "ok": killed }))
}

// ── 单元测试 ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::bytes::BytesMut;

    // ── NdjsonCodec 测试 ────────────────────────────────────────────

    #[test]
    fn test_ndjson_decode_single_message() {
        let mut codec = NdjsonCodec;
        let mut buf = BytesMut::from(&b"{\"hello\":\"world\"}\n"[..]);

        let result = codec.decode(&mut buf).expect("解码不应失败");
        assert!(result.is_some());

        let value = result.expect("应该有值");
        assert_eq!(value["hello"], "world");
        assert!(buf.is_empty(), "缓冲区应该被消费完毕");
    }

    // ── Phase 2 R4' extract_cwd_marker 单测 ──────────────────────────────

    #[test]
    fn test_extract_cwd_marker_typical() {
        let stdout = "hello\nworld\n__CRABCODE_CWD_BEGIN__/tmp/foo__CRABCODE_CWD_END__";
        let (clean, cwd) = extract_cwd_marker(stdout);
        assert_eq!(clean, "hello\nworld");
        assert_eq!(cwd, Some("/tmp/foo".to_string()));
    }

    #[test]
    fn test_extract_cwd_marker_no_marker_returns_none() {
        let stdout = "hello\nworld\n";
        let (clean, cwd) = extract_cwd_marker(stdout);
        assert_eq!(clean, "hello\nworld\n");
        assert_eq!(cwd, None);
    }

    #[test]
    fn test_extract_cwd_marker_partial_marker_returns_none() {
        // 只有 BEGIN 没 END → 不能解析，原样返
        let stdout = "data\n__CRABCODE_CWD_BEGIN__/tmp/foo";
        let (clean, cwd) = extract_cwd_marker(stdout);
        assert_eq!(clean, stdout);
        assert_eq!(cwd, None);
    }

    #[test]
    fn test_extract_cwd_marker_uses_rightmost_match() {
        // 用户命令巧合输出 BEGIN/END 字面 + 真 marker 在后；rfind 取最末
        let stdout = "fake __CRABCODE_CWD_BEGIN__nope__CRABCODE_CWD_END__\nreal\n__CRABCODE_CWD_BEGIN__/real/cwd__CRABCODE_CWD_END__";
        let (_clean, cwd) = extract_cwd_marker(stdout);
        assert_eq!(cwd, Some("/real/cwd".to_string()));
    }

    #[test]
    fn test_extract_cwd_marker_empty_cwd() {
        // 极端：cwd 字符串为空（不应发生，但断言不 panic）
        let stdout = "data\n__CRABCODE_CWD_BEGIN____CRABCODE_CWD_END__";
        let (clean, cwd) = extract_cwd_marker(stdout);
        assert_eq!(clean, "data");
        assert_eq!(cwd, Some(String::new()));
    }

    #[test]
    fn test_extract_cwd_marker_with_path_containing_special_chars() {
        let stdout = "out\n__CRABCODE_CWD_BEGIN__/Users/foo bar/proj-1__CRABCODE_CWD_END__";
        let (clean, cwd) = extract_cwd_marker(stdout);
        assert_eq!(clean, "out");
        assert_eq!(cwd, Some("/Users/foo bar/proj-1".to_string()));
    }

    // ── Phase 2 复核审计 FIX-1（2026-05-06）：trap EXIT 注入字符串合法性 ─────

    /// 实证 handle_spawn_managed_sandboxed 的 trap 注入字符串模板：
    /// - 必须以 `trap '...' EXIT\n` 开头（heredoc 安全 + exit code 透传）
    /// - 必须含 `__rc=$?` 捕获原命令退出码
    /// - 必须含 `exit $__rc` 透传退出码（覆盖 trap 命令本身的 exit 0）
    /// - marker 字符串 BEGIN/END 与 extract_cwd_marker 解析端一致
    /// - 用户原命令 `${cmd}` 在换行后（heredoc 终止符独占行规则得以满足）
    #[test]
    fn test_trap_exit_injection_format_template() {
        // 复刻 handle_spawn_managed_sandboxed 的注入逻辑（保持与生产代码同步）
        let cmd = "echo hi";
        let wrapped = format!(
            "trap '__rc=$?; printf \"\\n__CRABCODE_CWD_BEGIN__%s__CRABCODE_CWD_END__\" \"$(pwd -P)\"; exit $__rc' EXIT\n{}",
            cmd
        );

        // 关键特征断言
        assert!(wrapped.starts_with("trap '"), "必须 trap 起头");
        assert!(wrapped.contains("__rc=$?"), "必须捕获原 exit code (HIGH-2)");
        assert!(
            wrapped.contains("exit $__rc"),
            "必须透传 exit code (HIGH-2)"
        );
        assert!(wrapped.contains("__CRABCODE_CWD_BEGIN__"), "marker BEGIN");
        assert!(wrapped.contains("__CRABCODE_CWD_END__"), "marker END");
        assert!(wrapped.contains("$(pwd -P)"), "pwd -P 子 shell 取真路径");
        assert!(
            wrapped.contains("' EXIT\n"),
            "trap 命令独占一行（HIGH-1 heredoc 安全）"
        );
        assert!(
            wrapped.ends_with(cmd),
            "用户原 cmd 在 trap 行之后（heredoc 终止符独占行规则）"
        );
    }

    /// 验证 heredoc 命令在 trap EXIT 注入后**保持完整**（HIGH-1 修复回归门）
    #[test]
    fn test_trap_exit_preserves_heredoc_command() {
        let heredoc_cmd = "cat <<EOF\nhello\nEOF";
        let wrapped = format!(
            "trap '__rc=$?; printf \"\\n__CRABCODE_CWD_BEGIN__%s__CRABCODE_CWD_END__\" \"$(pwd -P)\"; exit $__rc' EXIT\n{}",
            heredoc_cmd
        );

        // heredoc 终止符 `EOF` 在末尾独占行（注入后不破坏）
        assert!(
            wrapped.ends_with("\nEOF"),
            "heredoc 终止符 EOF 必须独占行末尾以保持 bash 语法合法"
        );
        // trap 命令在 heredoc 之前（不与 heredoc 内容混合）
        let trap_end = wrapped.find("' EXIT\n").expect("trap 必须有 EXIT 终止");
        let heredoc_start = wrapped.find("cat <<EOF").expect("heredoc 命令必须出现");
        assert!(trap_end < heredoc_start, "trap 必须在 heredoc 命令之前");
    }

    #[test]
    fn test_ndjson_decode_incomplete_message() {
        let mut codec = NdjsonCodec;
        let mut buf = BytesMut::from(&b"{\"hello\":\"wor"[..]);

        // 没有换行符，应返回 None（等待更多数据）
        let result = codec.decode(&mut buf).expect("解码不应失败");
        assert!(result.is_none());
    }

    #[test]
    fn test_ndjson_decode_multiple_messages() {
        let mut codec = NdjsonCodec;
        let mut buf = BytesMut::from(&b"{\"a\":1}\n{\"b\":2}\n"[..]);

        // 第一条消息
        let msg1 = codec
            .decode(&mut buf)
            .expect("解码不应失败")
            .expect("应该有第一条消息");
        assert_eq!(msg1["a"], 1);

        // 第二条消息
        let msg2 = codec
            .decode(&mut buf)
            .expect("解码不应失败")
            .expect("应该有第二条消息");
        assert_eq!(msg2["b"], 2);

        // 没有更多消息
        let msg3 = codec.decode(&mut buf).expect("解码不应失败");
        assert!(msg3.is_none());
    }

    #[test]
    fn test_ndjson_decode_empty_line() {
        let mut codec = NdjsonCodec;
        let mut buf = BytesMut::from(&b"\n{\"x\":1}\n"[..]);

        // 空行应该被跳过，返回 None
        let result1 = codec.decode(&mut buf).expect("解码不应失败");
        assert!(result1.is_none());

        // 下一次调用应该返回实际消息
        let result2 = codec
            .decode(&mut buf)
            .expect("解码不应失败")
            .expect("应该有消息");
        assert_eq!(result2["x"], 1);
    }

    #[test]
    fn test_ndjson_decode_windows_line_ending() {
        let mut codec = NdjsonCodec;
        let mut buf = BytesMut::from(&b"{\"cr\":true}\r\n"[..]);

        let result = codec
            .decode(&mut buf)
            .expect("解码不应失败")
            .expect("应该正确解析 CRLF 行");
        assert_eq!(result["cr"], true);
    }

    #[test]
    fn test_ndjson_decode_invalid_json() {
        let mut codec = NdjsonCodec;
        let mut buf = BytesMut::from(&b"not valid json\n"[..]);

        let result = codec.decode(&mut buf);
        assert!(result.is_err(), "无效 JSON 应返回错误");
    }

    #[test]
    fn test_ndjson_decode_oversized_frame() {
        let mut codec = NdjsonCodec;
        // 创建一个超过 MAX_FRAME_SIZE 的缓冲区（无换行符）
        let oversized = vec![b'x'; MAX_FRAME_SIZE + 1];
        let mut buf = BytesMut::from(oversized.as_slice());

        let result = codec.decode(&mut buf);
        assert!(result.is_err(), "超大帧应返回错误");

        let err = result.unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("最大限制"),
            "错误消息应包含'最大限制'，实际: {err_msg}"
        );
    }

    #[test]
    fn test_ndjson_encode() {
        let mut codec = NdjsonCodec;
        let mut buf = BytesMut::new();
        let value = serde_json::json!({"key": "value", "num": 42});

        codec.encode(value, &mut buf).expect("编码不应失败");

        // 编码结果应以 '\n' 结尾
        assert!(buf.ends_with(b"\n"));

        // 去掉换行后应该是有效的 JSON
        let json_part = &buf[..buf.len() - 1];
        let parsed: serde_json::Value =
            serde_json::from_slice(json_part).expect("编码结果应为有效 JSON");
        assert_eq!(parsed["key"], "value");
        assert_eq!(parsed["num"], 42);
    }

    #[test]
    fn test_ndjson_roundtrip() {
        let mut codec = NdjsonCodec;
        let original = serde_json::json!({
            "type": "handshake",
            "version": "1.0",
            "nested": {"a": [1, 2, 3]}
        });

        // 编码
        let mut buf = BytesMut::new();
        codec
            .encode(original.clone(), &mut buf)
            .expect("编码不应失败");

        // 解码
        let decoded = codec
            .decode(&mut buf)
            .expect("解码不应失败")
            .expect("应该有消息");

        assert_eq!(original, decoded);
    }

    // ── FramedJsonCodec 测试 ──────────────────────────────────────────

    #[test]
    fn test_framed_encode_decode_roundtrip() {
        let mut codec = FramedJsonCodec;
        let original = serde_json::json!({"type": "handshake", "version": "1.0"});

        let mut buf = BytesMut::new();
        codec
            .encode(original.clone(), &mut buf)
            .expect("编码不应失败");

        // 验证帧格式：前 4 字节是 BE u32 长度
        assert!(buf.len() > 4);
        let length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        assert_eq!(buf.len(), 4 + length);

        let decoded = codec
            .decode(&mut buf)
            .expect("解码不应失败")
            .expect("应该有消息");
        assert_eq!(original, decoded);
        assert!(buf.is_empty(), "缓冲区应该被消费完毕");
    }

    #[test]
    fn test_framed_decode_incomplete_length() {
        let mut codec = FramedJsonCodec;
        let mut buf = BytesMut::from(&[0u8, 0, 0][..]); // 只有 3 字节
        let result = codec.decode(&mut buf).expect("解码不应失败");
        assert!(result.is_none(), "不足 4 字节应返回 None");
    }

    #[test]
    fn test_framed_decode_incomplete_payload() {
        let mut codec = FramedJsonCodec;
        // 长度前缀声明 100 字节，但只给了 10 字节 payload
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&100u32.to_be_bytes());
        buf.extend_from_slice(&[b'x'; 10]);
        let result = codec.decode(&mut buf).expect("解码不应失败");
        assert!(result.is_none(), "payload 不完整应返回 None");
    }

    #[test]
    fn test_framed_decode_oversized_frame() {
        let mut codec = FramedJsonCodec;
        let mut buf = BytesMut::new();
        buf.extend_from_slice(&(MAX_FRAMED_SIZE + 1).to_be_bytes());
        let result = codec.decode(&mut buf);
        assert!(result.is_err(), "超大帧应返回错误");
    }

    #[test]
    fn test_framed_decode_multiple_messages() {
        let mut codec = FramedJsonCodec;
        let msg1 = serde_json::json!({"a": 1});
        let msg2 = serde_json::json!({"b": 2});

        let mut buf = BytesMut::new();
        codec.encode(msg1.clone(), &mut buf).unwrap();
        codec.encode(msg2.clone(), &mut buf).unwrap();

        let d1 = codec.decode(&mut buf).unwrap().expect("应有第一条");
        let d2 = codec.decode(&mut buf).unwrap().expect("应有第二条");
        assert_eq!(msg1, d1);
        assert_eq!(msg2, d2);
        assert!(codec.decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn test_framed_go_wire_compat() {
        // 手动构造一个 Go 端会发出的帧，验证 Rust 能正确解码
        let json_payload = b"{\"msg_type\":\"heartbeat\"}";
        let length = json_payload.len() as u32;

        let mut buf = BytesMut::new();
        buf.extend_from_slice(&length.to_be_bytes());
        buf.extend_from_slice(json_payload);

        let mut codec = FramedJsonCodec;
        let decoded = codec
            .decode(&mut buf)
            .expect("解码不应失败")
            .expect("应有消息");
        assert_eq!(decoded["msg_type"], "heartbeat");
    }

    // ── StdioBridge 测试 ────────────────────────────────────────────

    /// 通过启动一个 echo-like 子进程来测试 StdioBridge 的发送和接收
    #[tokio::test]
    #[cfg(not(windows))] // Windows 无可靠的 stdin→stdout 回显命令（findstr 管道不 flush）
    async fn test_stdio_bridge_send_recv() {
        // 启动一个简单的子进程：从 stdin 读取并原样输出到 stdout
        #[cfg(windows)]
        let mut child = tokio::process::Command::new("cmd.exe")
            .args(["/C", "findstr", ".*"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("启动子进程失败");

        #[cfg(not(windows))]
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("启动子进程失败");

        let stdin = child.stdin.take().expect("应该有 stdin");
        let stdout = child.stdout.take().expect("应该有 stdout");

        let mut bridge = StdioBridge::new(stdout, stdin);

        // 发送一个 JSON 消息
        let msg = serde_json::json!({"hello": "world"});
        bridge.send(&msg).await.expect("发送不应失败");

        // 读回来
        let received: serde_json::Value = bridge
            .recv()
            .await
            .expect("接收不应失败")
            .expect("应该收到消息");

        assert_eq!(received["hello"], "world");

        // 关闭 bridge（drop writer）导致子进程收到 EOF 并退出
        drop(bridge);
        let _ = child.wait().await;
    }

    #[tokio::test]
    #[cfg(not(windows))] // Windows 无可靠的 stdin→stdout 回显命令
    async fn test_stdio_bridge_keepalive() {
        // 启动回显子进程
        #[cfg(windows)]
        let mut child = tokio::process::Command::new("cmd.exe")
            .args(["/C", "findstr", ".*"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("启动子进程失败");

        #[cfg(not(windows))]
        let mut child = tokio::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("启动子进程失败");

        let stdin = child.stdin.take().expect("应该有 stdin");
        let stdout = child.stdout.take().expect("应该有 stdout");

        let mut bridge = StdioBridge::new(stdout, stdin);

        // 发送 keepalive
        bridge.send_keepalive().await.expect("keepalive 不应失败");

        // 读取回显，验证格式
        let received: serde_json::Value = bridge
            .recv()
            .await
            .expect("接收不应失败")
            .expect("应该收到 keepalive 回显");

        assert_eq!(received["type"], "keepalive");

        drop(bridge);
        let _ = child.wait().await;
    }

    #[tokio::test]
    #[cfg(not(windows))] // Windows cmd.exe echo 输出格式与 JSON 不兼容
    async fn test_stdio_bridge_recv_eof() {
        // 启动一个立即输出 JSON 并退出的子进程来测试 EOF
        #[cfg(windows)]
        let mut child = tokio::process::Command::new("cmd.exe")
            .args(["/C", r#"echo {"done":true}"#])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("启动子进程失败");

        #[cfg(not(windows))]
        let mut child = tokio::process::Command::new("echo")
            .arg(r#"{"done":true}"#)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("启动子进程失败");

        let stdin = child.stdin.take().expect("应该有 stdin");
        let stdout = child.stdout.take().expect("应该有 stdout");

        let mut bridge = StdioBridge::new(stdout, stdin);

        // 第一条消息
        let msg: serde_json::Value = bridge
            .recv()
            .await
            .expect("接收不应失败")
            .expect("应该收到消息");
        assert_eq!(msg["done"], true);

        // 子进程已退出，接下来应该收到 EOF (None)
        let _ = child.wait().await;
        let eof = bridge
            .recv::<serde_json::Value>()
            .await
            .expect("EOF 不应报错");
        assert!(eof.is_none(), "EOF 应返回 None");
    }

    // ── handle_capability_request 测试 ──────────────────────────────

    #[tokio::test]
    async fn test_handle_exec_request() {
        let executor = CommandExecutor::new();

        // 构建一个简单的 exec 能力请求
        #[cfg(windows)]
        let command = "cmd.exe".to_string();
        #[cfg(not(windows))]
        let command = "echo".to_string();

        #[cfg(windows)]
        let args = vec![
            "/C".to_string(),
            "echo".to_string(),
            "test_output".to_string(),
        ];
        #[cfg(not(windows))]
        let args = vec!["test_output".to_string()];

        let req = CapabilityReq {
            request_id: "test-exec-001".to_string(),
            trace_id: "00000000000000000000000000000001".to_string(),
            span_id: None,
            family: CapabilityFamily::Exec,
            command: Some(command),
            args: Some(args),
            env_overrides: None,
            policy: Some(acosmi_types::protocol::ExecPolicy {
                sandbox: Some(false),
                timeout_ms: Some(5000),
                cwd: Some(String::new()),
                max_output_bytes: None,
                stdin_data: None,
                allowed_path_prefixes: None,
                inherit_env: None,
            }),
            lifecycle: acosmi_types::protocol::Lifecycle::Oneshot,
            batch_items: None,
        };

        let resp = handle_capability_request(req, &executor).await;

        assert_eq!(resp.status, CapabilityStatus::Ok);
        assert!(resp.exec_result.is_some());
        let result = resp.exec_result.expect("应有执行结果");
        assert!(
            result.stdout.contains("test_output"),
            "stdout 应包含 test_output，实际: {}",
            result.stdout
        );
    }

    #[tokio::test]
    async fn test_handle_spawn_managed_denied() {
        let executor = CommandExecutor::new();

        let req = CapabilityReq {
            request_id: "test-spawn-001".to_string(),
            trace_id: "00000000000000000000000000000002".to_string(),
            span_id: None,
            family: CapabilityFamily::SpawnManaged,
            command: Some("some_process".to_string()),
            args: Some(vec![]),
            env_overrides: None,
            policy: Some(acosmi_types::protocol::ExecPolicy {
                sandbox: Some(false),
                timeout_ms: Some(5000),
                cwd: Some(String::new()),
                max_output_bytes: None,
                stdin_data: None,
                allowed_path_prefixes: None,
                inherit_env: None,
            }),
            lifecycle: acosmi_types::protocol::Lifecycle::LongRunning,
            batch_items: None,
        };

        let resp = handle_capability_request(req, &executor).await;

        // SpawnManaged 尚未实现，应返回 Denied
        assert_eq!(resp.status, CapabilityStatus::Denied);
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn test_handle_unsupported_family_denied() {
        let executor = CommandExecutor::new();

        let req = CapabilityReq {
            request_id: "test-fs-001".to_string(),
            trace_id: "00000000000000000000000000000003".to_string(),
            span_id: None,
            family: CapabilityFamily::FsRead,
            command: Some("read_file".to_string()),
            args: Some(vec!["/tmp/test.txt".to_string()]),
            env_overrides: None,
            policy: Some(acosmi_types::protocol::ExecPolicy {
                sandbox: Some(false),
                timeout_ms: Some(5000),
                cwd: Some(String::new()),
                max_output_bytes: None,
                stdin_data: None,
                allowed_path_prefixes: None,
                inherit_env: None,
            }),
            lifecycle: acosmi_types::protocol::Lifecycle::Oneshot,
            batch_items: None,
        };

        let resp = handle_capability_request(req, &executor).await;

        assert_eq!(resp.status, CapabilityStatus::Denied);
        assert!(resp.error.is_some());
    }

    // ── IpcConfig 测试 ──────────────────────────────────────────────

    #[test]
    fn test_ipc_config_default() {
        let config = IpcConfig::default();
        let pid = std::process::id();

        assert!(config.uds_path.is_some());
        let uds_path = config.uds_path.expect("应有 UDS 路径");
        // P1-2: UDS 现落 state_dir/supervisor/<pid>.sock，不再是 /tmp/...。
        let uds_str = uds_path.to_string_lossy();
        assert!(
            !uds_str.starts_with("/tmp/"),
            "UDS 路径不应再落 /tmp，得到 {uds_str}"
        );
        assert_eq!(
            uds_path.file_name(),
            Some(std::ffi::OsStr::new(&format!("{pid}.sock"))),
            "UDS 文件名应为 <pid>.sock，得到 {uds_str}"
        );
        assert_eq!(
            uds_path.parent().and_then(std::path::Path::file_name),
            Some(std::ffi::OsStr::new("supervisor")),
            "UDS 父目录应为 supervisor，得到 {uds_str}"
        );
        let state_dir = acosmi_config::paths::resolve_state_dir();
        assert!(
            uds_path.starts_with(&state_dir),
            "UDS 路径应落在 state_dir ({}) 下，得到 {uds_str}",
            state_dir.display()
        );

        assert!(config.pipe_name.is_some());
        let pipe_name = config.pipe_name.expect("应有 pipe 名称");
        assert!(pipe_name.contains(&pid.to_string()), "pipe 名称应包含 PID");
        assert!(
            pipe_name.starts_with(r"\\.\pipe\crabcode-"),
            "pipe 名称应以标准前缀开头"
        );

        assert_eq!(config.protocol_version, "1.0");

        // P1-2: auth_secret 是高熵 hex（两个 v4 UUID 拼接 = 64 hex 字符）。
        assert_eq!(config.auth_secret.len(), 64, "auth_secret 应为 64 hex 字符");
        assert!(
            config.auth_secret.chars().all(|c| c.is_ascii_hexdigit()),
            "auth_secret 应全为 hex"
        );
        // 两次 default 的 secret 不同（CSPRNG，非固定）。
        let other = IpcConfig::default();
        assert_ne!(
            config.auth_secret, other.auth_secret,
            "两次生成的 secret 应不同"
        );
    }

    // ── P1-2: 握手密钥校验单测 ──────────────────────────────────────

    #[test]
    fn test_handshake_secret_missing_rejected() {
        // params 无 auth_secret 字段 → AuthDenied
        let params = serde_json::json!({ "protocol_version": "1.0" });
        let err =
            verify_handshake_secret(Some(&params), "expected-secret").expect_err("缺密钥应被拒");
        assert!(
            matches!(err, IpcError::AuthDenied(_)),
            "应为 AuthDenied: {err:?}"
        );
    }

    #[test]
    fn test_handshake_secret_none_params_rejected() {
        let err = verify_handshake_secret(None, "expected-secret").expect_err("无 params 应被拒");
        assert!(
            matches!(err, IpcError::AuthDenied(_)),
            "应为 AuthDenied: {err:?}"
        );
    }

    #[test]
    fn test_handshake_secret_wrong_rejected() {
        let params = serde_json::json!({ "auth_secret": "wrong-secret" });
        let err =
            verify_handshake_secret(Some(&params), "expected-secret").expect_err("错误密钥应被拒");
        assert!(
            matches!(err, IpcError::AuthDenied(_)),
            "应为 AuthDenied: {err:?}"
        );
    }

    #[test]
    fn test_handshake_secret_correct_accepted() {
        let params = serde_json::json!({ "auth_secret": "expected-secret" });
        assert!(
            verify_handshake_secret(Some(&params), "expected-secret").is_ok(),
            "正确密钥应通过"
        );
    }

    #[test]
    fn test_handshake_secret_length_mismatch_rejected() {
        // 前缀匹配但长度不同 → 仍拒（防长度旁路）
        let params = serde_json::json!({ "auth_secret": "expected-secret-extra" });
        let err =
            verify_handshake_secret(Some(&params), "expected-secret").expect_err("长度不同应被拒");
        assert!(
            matches!(err, IpcError::AuthDenied(_)),
            "应为 AuthDenied: {err:?}"
        );
    }

    // ── P1-2: socket / 目录权限单测（Unix） ─────────────────────────

    /// 生成一个短的临时目录路径（落 /tmp，避开 macOS temp_dir 过长触发
    /// `path must be shorter than SUN_LEN` 的 UDS bind 失败）。
    #[cfg(unix)]
    fn short_test_dir(tag: &str) -> std::path::PathBuf {
        let short = &uuid::Uuid::new_v4().simple().to_string()[..8];
        std::path::PathBuf::from(format!("/tmp/{tag}-{short}"))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_unix_socket_perms_0600_and_dir_0700() {
        use std::os::unix::fs::PermissionsExt;

        // 用隔离短路径跑一次 start_unix，断言权限位。socket 路径必须短于
        // SUN_LEN（macOS ~104 字节），故落 /tmp 而非长的 temp_dir。
        let tmp = short_test_dir("ccperm");
        std::fs::create_dir_all(&tmp).expect("创建临时 state_dir");
        let sock = tmp.join("sup").join("t.sock");

        let cfg = IpcConfig {
            uds_path: Some(sock.clone()),
            ..IpcConfig::default()
        };

        let executor = Arc::new(CommandExecutor::new());
        let shutdown = CancellationToken::new();
        let (tx, _rx) = tokio::sync::mpsc::channel::<IpcSignal>(8);
        let server = IpcServer::new(cfg, executor, shutdown.clone(), tx);
        let handle = server.start().await.expect("启动 IPC server");

        // 等 socket 落盘。
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(sock.exists(), "socket 文件应已创建: {}", sock.display());

        let sock_mode = std::fs::metadata(&sock)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(sock_mode, 0o600, "socket 应为 0600，得到 {sock_mode:o}");

        let dir_mode = std::fs::metadata(sock.parent().unwrap())
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "socket 父目录应为 0700，得到 {dir_mode:o}");

        shutdown.cancel();
        let _ = handle.await;
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ── P1-2: 端到端握手认证（Unix UDS） ────────────────────────────

    #[cfg(unix)]
    #[tokio::test]
    async fn test_handshake_rejects_missing_secret_end_to_end() {
        let result = run_handshake_with_optional_secret(None).await;
        // 服务端在认证失败时不发 accepted 响应，直接断连 → 客户端读到 EOF。
        assert!(
            result.is_none(),
            "缺密钥时不应收到 accepted 响应，得到 {result:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_handshake_rejects_wrong_secret_end_to_end() {
        let result = run_handshake_with_optional_secret(Some("totally-wrong")).await;
        assert!(
            result.is_none(),
            "错误密钥时不应收到 accepted 响应，得到 {result:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_handshake_accepts_correct_secret_end_to_end() {
        // SENTINEL: special marker telling the harness to use the server's secret.
        let result = run_handshake_with_optional_secret(Some("__USE_SERVER_SECRET__")).await;
        match result {
            Some(v) => {
                assert_eq!(
                    v.get("accepted").and_then(|a| a.as_bool()),
                    Some(true),
                    "正确密钥应被接受: {v:?}"
                );
            }
            None => panic!("正确密钥应收到 accepted 响应，但连接被断开"),
        }
    }

    /// 端到端握手 harness：起真实 UDS server，连一个客户端，发 version_handshake
    /// 帧（按 `secret` 决定是否/如何携带 auth_secret），返回服务端的 result
    /// payload（被拒时返 None，因服务端断连客户端读到 EOF）。
    #[cfg(unix)]
    async fn run_handshake_with_optional_secret(secret: Option<&str>) -> Option<serde_json::Value> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // 短路径（SUN_LEN 限制，见 perms 测试注释）。
        let tmp = short_test_dir("cchs");
        let sock = tmp.join("sup").join("t.sock");

        let cfg = IpcConfig {
            uds_path: Some(sock.clone()),
            ..IpcConfig::default()
        };
        let server_secret = cfg.auth_secret.clone();

        let executor = Arc::new(CommandExecutor::new());
        let shutdown = CancellationToken::new();
        let (tx, _rx) = tokio::sync::mpsc::channel::<IpcSignal>(8);
        let server = IpcServer::new(cfg, executor, shutdown.clone(), tx);
        let handle = server.start().await.expect("启动 IPC server");

        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let mut client = tokio::net::UnixStream::connect(&sock)
            .await
            .expect("连接 server");

        // 构造 version_handshake envelope。
        let mut params = serde_json::json!({
            "protocol_version": "1.0",
            "component_version": "test",
        });
        if let Some(s) = secret {
            let effective = if s == "__USE_SERVER_SECRET__" {
                server_secret.clone()
            } else {
                s.to_string()
            };
            params["auth_secret"] = serde_json::Value::String(effective);
        }
        let env = serde_json::json!({
            "msg_type": "request",
            "header": { "msg_id": "hs-1" },
            "payload": { "method": "version_handshake", "params": params },
        });

        // Framed 编码：4 字节大端长度 + JSON。
        let body = serde_json::to_vec(&env).expect("序列化握手");
        let len = (body.len() as u32).to_be_bytes();
        client.write_all(&len).await.expect("写长度");
        client.write_all(&body).await.expect("写 body");
        client.flush().await.expect("flush");

        // 读响应：4 字节长度 + JSON。被拒时 server 断连 → read 到 EOF/0。
        let mut len_buf = [0u8; 4];
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            client.read_exact(&mut len_buf),
        )
        .await;

        let out = match read_result {
            Ok(Ok(_)) => {
                let resp_len = u32::from_be_bytes(len_buf) as usize;
                let mut resp = vec![0u8; resp_len];
                if client.read_exact(&mut resp).await.is_ok() {
                    let v: serde_json::Value = serde_json::from_slice(&resp).expect("解析响应");
                    // result 在 payload.result。
                    v.get("payload")
                        .and_then(|p| p.get("result"))
                        .cloned()
                        .or(Some(v))
                } else {
                    None
                }
            }
            // EOF / timeout / error → 被拒（无 accepted 响应）。
            _ => None,
        };

        shutdown.cancel();
        let _ = handle.await;
        let _ = std::fs::remove_dir_all(&tmp);
        out
    }

    // ── IpcError 测试 ───────────────────────────────────────────────

    #[test]
    fn test_ipc_error_display() {
        let err = IpcError::Protocol("消息格式不合法".to_string());
        assert!(err.to_string().contains("协议错误"));
        assert!(err.to_string().contains("消息格式不合法"));

        let err = IpcError::VersionMismatch {
            local: "1.0".to_string(),
            remote: "2.0".to_string(),
        };
        assert!(err.to_string().contains("1.0"));
        assert!(err.to_string().contains("2.0"));

        let err = IpcError::ConnectionClosed;
        assert!(err.to_string().contains("连接已关闭"));
    }

    #[test]
    fn test_ipc_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pipe broken");
        let ipc_err: IpcError = io_err.into();
        assert!(matches!(ipc_err, IpcError::Io(_)));
    }

    #[test]
    fn test_ipc_error_from_json() {
        let json_err = serde_json::from_str::<serde_json::Value>("invalid").unwrap_err();
        let ipc_err: IpcError = json_err.into();
        assert!(matches!(ipc_err, IpcError::Json(_)));
    }

    // ── ExecBridge JSON-RPC 测试 ────────────────────────────────────

    #[test]
    fn test_jsonrpc_ok_response() {
        let id = serde_json::json!("test-id-001");
        let result = serde_json::json!({"status": "ok"});
        let resp = jsonrpc_ok(&id, result.clone());

        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], "test-id-001");
        assert_eq!(resp["result"], result);
        assert!(resp.get("error").is_none());
    }

    #[test]
    fn test_jsonrpc_error_response() {
        let id = serde_json::json!("test-id-002");
        let resp = jsonrpc_error(&id, JSONRPC_METHOD_NOT_FOUND, "未知方法: foo.bar");

        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], "test-id-002");
        assert_eq!(resp["error"]["code"], JSONRPC_METHOD_NOT_FOUND);
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap()
                .contains("未知方法")
        );
        assert!(resp.get("result").is_none());
    }

    #[test]
    fn test_jsonrpc_request_deserialize() {
        let json = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "exec.getGitContext",
            "params": {"cwd": "/tmp/test"},
            "id": "uuid-123"
        });

        let req: JsonRpcRequest = serde_json::from_value(json).expect("反序列化不应失败");
        assert_eq!(req.method, "exec.getGitContext");
        assert_eq!(req.params["cwd"], "/tmp/test");
        assert_eq!(req.id, "uuid-123");
    }

    #[tokio::test]
    async fn test_dispatch_unknown_method() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "exec.unknown".to_string(),
            params: serde_json::json!({}),
            id: serde_json::json!("test-unknown"),
        };

        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let resp = dispatch_exec_bridge(&req, executor, tx).await;

        assert_eq!(resp["error"]["code"], JSONRPC_METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_dispatch_spawn_managed_executes() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "exec.spawnManaged".to_string(),
            params: serde_json::json!({
                "command": "echo",
                "shell": "bash",
                "dangerouslyDisableSandbox": true
            }),
            id: serde_json::json!("test-spawn"),
        };

        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let resp = dispatch_exec_bridge(&req, executor, tx).await;

        // spawnManaged 现在走 execute_and_format，应返回 result
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_dispatch_exec_git_command() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "exec.execGitCommand".to_string(),
            params: serde_json::json!({
                "args": ["--version"]
            }),
            id: serde_json::json!("test-git-version"),
        };

        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let resp = dispatch_exec_bridge(&req, executor, tx).await;

        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        let result = &resp["result"];
        assert_eq!(result["code"], 0, "git --version 应成功");
        assert!(
            result["stdout"]
                .as_str()
                .unwrap_or("")
                .contains("git version"),
            "stdout 应包含 git version"
        );
    }

    #[tokio::test]
    #[cfg(not(windows))]
    async fn test_dispatch_exec_command() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            method: "exec.execCommand".to_string(),
            params: serde_json::json!({
                "command": "echo",
                "args": ["hello_from_exec_bridge"]
            }),
            id: serde_json::json!("test-echo"),
        };

        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let resp = dispatch_exec_bridge(&req, executor, tx).await;

        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        let result = &resp["result"];
        assert_eq!(result["code"], 0);
        assert!(
            result["stdout"]
                .as_str()
                .unwrap_or("")
                .contains("hello_from_exec_bridge")
        );
    }

    #[test]
    fn test_find_git_executable() {
        let git = find_git_executable();
        assert!(!git.is_empty(), "git 路径不应为空");
        // 在 CI/开发环境中 git 应该可用
        assert!(git.contains("git"), "git 路径应包含 'git'，实际: {git}");
    }

    // ── 权限检查测试 ────────────────────────────────────────────────

    #[test]
    fn test_parse_permission_mode_none_skips_check() {
        assert!(parse_permission_mode(None).is_none());
    }

    #[test]
    fn test_parse_permission_mode_known_values() {
        assert_eq!(
            parse_permission_mode(Some("default")),
            Some(PermissionMode::Default)
        );
        assert_eq!(
            parse_permission_mode(Some("bypassPermissions")),
            Some(PermissionMode::BypassPermissions)
        );
        assert_eq!(
            parse_permission_mode(Some("dontAsk")),
            Some(PermissionMode::DontAsk)
        );
        assert_eq!(
            parse_permission_mode(Some("plan")),
            Some(PermissionMode::Plan)
        );
        assert_eq!(
            parse_permission_mode(Some("acceptEdits")),
            Some(PermissionMode::AcceptEdits)
        );
        assert_eq!(
            parse_permission_mode(Some("auto")),
            Some(PermissionMode::Auto)
        );
    }

    #[test]
    fn test_parse_permission_mode_unknown_defaults() {
        // 未知值应降级为 Default
        assert_eq!(
            parse_permission_mode(Some("something_weird")),
            Some(PermissionMode::Default)
        );
    }

    #[test]
    fn test_format_full_command_no_args() {
        assert_eq!(format_full_command("echo", &[]), "echo");
    }

    #[test]
    fn test_format_full_command_with_args() {
        let args = vec!["hello".to_string(), "world".to_string()];
        assert_eq!(format_full_command("echo", &args), "echo hello world");
    }

    #[test]
    fn test_check_exec_permission_bypass_always_allows() {
        let id = serde_json::json!("test-1");
        let result = check_exec_permission(
            "rm -rf /",
            None,
            PermissionMode::BypassPermissions,
            vec![],
            &id,
        );
        assert!(matches!(result, PermCheckOutcome::Allowed));
    }

    #[test]
    fn test_check_exec_permission_dontask_always_denies() {
        let id = serde_json::json!("test-2");
        let result =
            check_exec_permission("echo hello", None, PermissionMode::DontAsk, vec![], &id);
        assert!(matches!(result, PermCheckOutcome::Blocked(_)));
    }

    #[test]
    fn test_check_exec_permission_readonly_allowed() {
        let id = serde_json::json!("test-3");
        let result =
            check_exec_permission("cat file.txt", None, PermissionMode::Default, vec![], &id);
        assert!(matches!(result, PermCheckOutcome::Allowed));
    }

    #[test]
    fn test_check_exec_permission_git_lenient_allows() {
        let id = serde_json::json!("test-4");
        // git push 在无规则时会被 Ask，但加了 git lenient 规则后应 Allow
        let result = check_exec_permission(
            "git push origin main",
            None,
            PermissionMode::Default,
            git_lenient_rules(),
            &id,
        );
        assert!(matches!(result, PermCheckOutcome::Allowed));
    }

    #[test]
    fn test_check_exec_permission_dangerous_asks() {
        let id = serde_json::json!("test-5");
        // npm install 在 Default 模式下应返回 Ask（非只读、无规则）
        let result =
            check_exec_permission("npm install", None, PermissionMode::Default, vec![], &id);
        assert!(matches!(result, PermCheckOutcome::Blocked(_)));
    }

    #[test]
    fn test_check_exec_permission_deny_rule_blocks_git() {
        let id = serde_json::json!("test-6");
        let mut rules = git_lenient_rules();
        // 添加显式 deny 规则：禁止 git push --force
        rules.push(PermissionRule {
            source: PermissionRuleSource::Session,
            behavior: PermissionBehavior::Deny,
            value: PermissionRuleValue {
                tool_name: "Bash".to_string(),
                rule_content: Some("git push --force *".to_string()),
            },
        });
        let result = check_exec_permission(
            "git push --force origin main",
            None,
            PermissionMode::Default,
            rules,
            &id,
        );
        assert!(matches!(result, PermCheckOutcome::Blocked(_)));
    }

    #[test]
    fn test_git_lenient_rules_structure() {
        let rules = git_lenient_rules();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].behavior, PermissionBehavior::Allow);
        assert_eq!(rules[0].value.tool_name, "Bash");
        assert_eq!(rules[0].value.rule_content, Some("git *".to_string()));
    }

    // Sprint 7 阶段 6 (2026-04-23)：5 个 test_exec_command_* 用裸 "echo"，在
    // Windows 上 echo 是 cmd.exe builtin 非可执行文件，CommandExecutor 直接返 -1。
    // pre-existing Windows failures（非 Sprint 7 引入），跟 worker::tests 的
    // `/bin/echo` 硬编码同类 CI 盲区。未来应改用 cfg 分派 cmd /C echo (Windows)
    // / /bin/echo (Unix)，或 ignore 本组 test 直到 E-3 IPC 路径重写。
    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "pre-existing Windows failure: bare `echo` 不是 exe；Round 3 建议改用 cfg 分派或 ignore"
    )]
    async fn test_exec_command_with_permission_bypass() {
        let executor = Arc::new(CommandExecutor::new());
        let params = serde_json::json!({
            "command": "echo",
            "args": ["perm_test"],
            "permission_mode": "bypassPermissions"
        });
        let id = serde_json::json!("perm-exec-1");
        let resp = handle_exec_command(&params, &id, &executor).await;
        // bypassPermissions 应直接执行
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        assert_eq!(resp["result"]["code"], 0);
    }

    #[tokio::test]
    async fn test_exec_command_with_permission_dontask() {
        let executor = Arc::new(CommandExecutor::new());
        let params = serde_json::json!({
            "command": "npm",
            "args": ["install"],
            "permission_mode": "dontAsk"
        });
        let id = serde_json::json!("perm-exec-2");
        let resp = handle_exec_command(&params, &id, &executor).await;
        // dontAsk 应拒绝执行
        assert!(resp.get("error").is_some(), "应有 error: {resp}");
        let err_code = resp["error"]["code"].as_i64().unwrap();
        assert_eq!(err_code, JSONRPC_PERMISSION_DENIED as i64);
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "pre-existing Windows failure (同 test_exec_command_with_permission_bypass)"
    )]
    async fn test_exec_command_without_permission_mode() {
        let executor = Arc::new(CommandExecutor::new());
        let params = serde_json::json!({
            "command": "echo",
            "args": ["no_perm_check"]
        });
        let id = serde_json::json!("perm-exec-3");
        let resp = handle_exec_command(&params, &id, &executor).await;
        // 无 permission_mode 应直接执行（向后兼容）
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        assert_eq!(resp["result"]["code"], 0);
    }

    #[tokio::test]
    async fn test_exec_git_command_lenient_allows() {
        let executor = Arc::new(CommandExecutor::new());
        let params = serde_json::json!({
            "args": ["status", "--short"],
            "permission_mode": "default"
        });
        let id = serde_json::json!("perm-git-1");
        let resp = handle_exec_git_command(&params, &id, &executor).await;
        // git status 在 default 模式 + lenient 规则下应被允许
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
    }

    #[tokio::test]
    async fn test_spawn_managed_permission_dontask() {
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "bash",
            "shell": "bash",
            "permission_mode": "dontAsk"
        });
        let id = serde_json::json!("perm-spawn-1");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        // dontAsk 应拒绝
        assert!(resp.get("error").is_some(), "应有 error: {resp}");
    }

    #[tokio::test]
    async fn test_spawn_managed_returns_process_id() {
        let executor = Arc::new(CommandExecutor::new());
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let params = serde_json::json!({
            "command": "echo hello",
            "shell": "bash",
            "dangerouslyDisableSandbox": true
        });
        let id = serde_json::json!("spawn-test-1");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;

        // 应立即返回 process_id 和 abort_token
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        let result = &resp["result"];
        assert!(result["process_id"].is_string());
        assert!(result["abort_token"].is_string());
        assert_eq!(result["status"], "spawned");

        // 收集所有通知直到 exec.completed（之前可能有 exec.progress）
        let mut got_progress = false;
        let mut got_completed = false;
        let deadline = std::time::Duration::from_secs(10);
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            match tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await {
                Ok(Some(event)) => {
                    let method = event["method"].as_str().unwrap_or("");
                    match method {
                        "exec.progress" => {
                            got_progress = true;
                            // 验证 progress 消息格式
                            let p = &event["params"];
                            assert!(p["process_id"].is_string());
                            assert!(p["stream"].is_string());
                            assert!(p["data"].is_string());
                        }
                        "exec.completed" => {
                            got_completed = true;
                            let p = &event["params"];
                            assert!(p["process_id"].is_string());
                            break;
                        }
                        _ => panic!("未预期的通知方法: {method}"),
                    }
                }
                Ok(None) => panic!("通道不应关闭"),
                Err(_) => panic!("等待通知超时"),
            }
        }

        assert!(got_completed, "应收到 exec.completed 通知");
        // echo 命令应产生至少一行 stdout progress
        assert!(got_progress, "echo 命令应产生 exec.progress 通知");
    }

    #[tokio::test]
    async fn test_exec_abort_unknown_token() {
        let executor = Arc::new(CommandExecutor::new());
        let params = serde_json::json!({ "abort_token": "nonexistent-token" });
        let id = serde_json::json!("abort-test-1");
        let resp = handle_exec_abort(&params, &id, &executor).await;
        assert!(resp.get("result").is_some());
        assert_eq!(resp["result"]["ok"], false);
    }

    #[tokio::test]
    async fn test_exec_abort_missing_token() {
        let executor = Arc::new(CommandExecutor::new());
        let params = serde_json::json!({});
        let id = serde_json::json!("abort-test-2");
        let resp = handle_exec_abort(&params, &id, &executor).await;
        assert!(resp.get("error").is_some(), "应有 error: {resp}");
    }

    // ── Sandbox IPC 测试 ────────────────────────────────────────

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "pre-existing Windows failure (同 test_exec_command_with_permission_bypass)"
    )]
    async fn test_exec_command_sandbox_false_default() {
        // 不传 sandbox 字段时应默认 false（向后兼容）
        let executor = Arc::new(CommandExecutor::new());
        let params = serde_json::json!({
            "command": "echo",
            "args": ["sandbox_default_test"]
        });
        let id = serde_json::json!("sandbox-1");
        let resp = handle_exec_command(&params, &id, &executor).await;
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        assert_eq!(resp["result"]["code"], 0);
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "pre-existing Windows failure (同 test_exec_command_with_permission_bypass)"
    )]
    async fn test_exec_command_sandbox_true_propagates() {
        // sandbox: true 应通过 execute_and_format 传递到 CommandExecutor
        // Phase 1 后端仍是 stub（无实际隔离），但不应影响执行
        let executor = Arc::new(CommandExecutor::new());
        let params = serde_json::json!({
            "command": "echo",
            "args": ["sandbox_true_test"],
            "sandbox": true
        });
        let id = serde_json::json!("sandbox-2");
        let resp = handle_exec_command(&params, &id, &executor).await;
        // 即使 sandbox=true，Phase 1 stub 仍能执行（只是无隔离）
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        assert_eq!(resp["result"]["code"], 0);
    }

    #[tokio::test]
    #[cfg_attr(
        windows,
        ignore = "pre-existing Windows failure (同 test_exec_command_with_permission_bypass)"
    )]
    async fn test_exec_command_sandbox_false_explicit() {
        // 显式 sandbox: false
        let executor = Arc::new(CommandExecutor::new());
        let params = serde_json::json!({
            "command": "echo",
            "args": ["no_sandbox"],
            "sandbox": false
        });
        let id = serde_json::json!("sandbox-3");
        let resp = handle_exec_command(&params, &id, &executor).await;
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        assert_eq!(resp["result"]["code"], 0);
    }

    #[tokio::test]
    async fn test_spawn_managed_sandbox_disabled() {
        // dangerouslyDisableSandbox: true → sandbox: false
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "echo",
            "shell": "bash",
            "dangerouslyDisableSandbox": true
        });
        let id = serde_json::json!("sandbox-spawn-1");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        assert_eq!(resp["result"]["status"], "spawned");
    }

    #[tokio::test]
    async fn test_spawn_managed_sandbox_enabled_default() {
        // dangerouslyDisableSandbox 未设置 → sandbox: true（默认启用）
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "echo",
            "shell": "bash"
        });
        let id = serde_json::json!("sandbox-spawn-2");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        // Phase 1 stub 下仍可执行
        assert!(resp.get("result").is_some(), "应有 result: {resp}");
        assert_eq!(resp["result"]["status"], "spawned");
    }

    // ── Sprint 7 阶段 1 (2026-04-23): resolve_shell 分派测试 ────────────

    #[test]
    fn test_resolve_shell_empty_defaults_to_platform() {
        let (prog, args) = resolve_shell("", "echo hello").expect("empty shell 应返回平台默认");
        if cfg!(windows) {
            assert_eq!(prog, "cmd.exe");
            assert_eq!(args, vec!["/C".to_string(), "echo hello".to_string()]);
        } else {
            assert_eq!(prog, "/bin/sh");
            assert_eq!(args, vec!["-c".to_string(), "echo hello".to_string()]);
        }
    }

    #[test]
    fn test_resolve_shell_cmd_unix_rejects() {
        #[cfg(unix)]
        {
            let err = resolve_shell("cmd", "anything").unwrap_err();
            assert!(err.contains("不适用于 Unix"), "err msg: {err}");
        }
        #[cfg(windows)]
        {
            let (prog, args) = resolve_shell("cmd", "echo hi").expect("Windows cmd 应 OK");
            assert_eq!(prog, "cmd.exe");
            assert_eq!(args, vec!["/C".to_string(), "echo hi".to_string()]);
        }
    }

    #[test]
    fn test_resolve_shell_bash_with_fallback() {
        // bash 策略按平台分化（Sprint 7 阶段 1 无回归兼容）
        let (prog, args) = resolve_shell("bash", "echo $SHELL")
            .expect("bash 应该返回 Ok（Unix 走 bash/sh，Windows 固定 cmd.exe）");
        if cfg!(windows) {
            // Windows 下 `shell: "bash"` **保守**走 cmd.exe，避开 System32 WSL stub
            // 和 Git bash 语义歧义。见 resolve_shell `"bash"` 分支文档。
            assert_eq!(
                prog, "cmd.exe",
                "Windows bash 策略保留阶段 1 前 cmd.exe 行为：实为 {prog}"
            );
            assert_eq!(args, vec!["/C".to_string(), "echo $SHELL".to_string()]);
        } else {
            // Unix：要么命中真 bash，要么 /bin/sh fallback
            assert!(
                prog.ends_with("/bash") || prog == "/bin/sh",
                "Unix bash 分派后应为 bash 或 /bin/sh，实为 {prog}"
            );
            assert_eq!(args[0], "-c");
            assert_eq!(args[args.len() - 1], "echo $SHELL");
        }
    }

    #[test]
    fn test_resolve_shell_powershell_pwsh_alias() {
        // powershell 和 pwsh 走同一分派路径；两种值都应 Ok 或在 Unix 无 pwsh 时 Err
        let r1 = resolve_shell("powershell", "Write-Host hi");
        let r2 = resolve_shell("pwsh", "Write-Host hi");
        if cfg!(windows) {
            let (p1, _) = r1.expect("Windows powershell 应 Ok（即便 fallback cmd.exe）");
            let (p2, _) = r2.expect("Windows pwsh 应 Ok（即便 fallback cmd.exe）");
            let acceptable = |p: &str| {
                p.eq_ignore_ascii_case("cmd.exe")
                    || p.to_lowercase().ends_with("pwsh.exe")
                    || p.to_lowercase().ends_with("powershell.exe")
            };
            assert!(acceptable(&p1), "p1={p1}");
            assert!(acceptable(&p2), "p2={p2}");
        } else if which_in_path("pwsh").is_none() {
            // Unix 无 pwsh → Err
            assert!(r1.is_err(), "Unix 无 pwsh 应 Err，实际 Ok: {r1:?}");
            assert!(r2.is_err(), "Unix 无 pwsh 应 Err，实际 Ok: {r2:?}");
        } else {
            let (p1, args1) = r1.unwrap();
            assert!(p1.ends_with("pwsh"), "Unix pwsh 应命中 pwsh：{p1}");
            assert_eq!(args1[0], "-NoProfile");
            assert_eq!(args1[1], "-Command");
        }
    }

    #[test]
    fn test_resolve_shell_rejects_unknown() {
        let err = resolve_shell("fish_but_wrong_spelling", "ls").unwrap_err();
        assert!(err.contains("不支持的 shell 值"), "err msg: {err}");
    }

    #[test]
    fn test_resolve_shell_case_insensitive_and_trimmed() {
        // 大小写 / 首尾空白不敏感；"BaSh" 应与 "bash" 等价
        let (p1, _) = resolve_shell("  BaSh ", "echo 1").expect("大小写不敏感");
        if cfg!(windows) {
            assert_eq!(p1, "cmd.exe", "Windows 下 bash → cmd.exe，实为 {p1}");
        } else {
            assert!(p1.ends_with("/bash") || p1 == "/bin/sh");
        }
    }

    #[test]
    fn test_resolve_shell_zsh_platform_split() {
        // Unix：zsh 无 fallback；找不到 → Err
        // Windows：zsh 保守回退 cmd.exe（同 "bash" 兼容策略）
        #[cfg(unix)]
        {
            if which_in_path("zsh").is_none() {
                let err = resolve_shell("zsh", "echo 1").unwrap_err();
                assert!(err.contains("zsh"), "err msg: {err}");
            } else {
                let (prog, args) = resolve_shell("zsh", "echo 1").unwrap();
                assert!(prog.ends_with("zsh"));
                assert_eq!(args, vec!["-c".to_string(), "echo 1".to_string()]);
            }
        }
        #[cfg(windows)]
        {
            let (prog, args) =
                resolve_shell("zsh", "echo 1").expect("Windows 下 zsh 应 fallback cmd.exe");
            assert_eq!(prog, "cmd.exe");
            assert_eq!(args, vec!["/C".to_string(), "echo 1".to_string()]);
        }
    }

    #[tokio::test]
    async fn test_spawn_managed_rejects_unknown_shell() {
        // 非法 shell 值应返回 JSONRPC_INVALID_PARAMS，而非静默降级或 panic
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "echo hi",
            "shell": "nonexistent_shell_xyz"
        });
        let id = serde_json::json!("shell-reject-1");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        assert!(
            resp.get("error").is_some(),
            "非法 shell 应返回 error: {resp}"
        );
        let code = resp["error"]["code"].as_i64().unwrap_or(0);
        assert_eq!(
            code, JSONRPC_INVALID_PARAMS as i64,
            "错误码应为 INVALID_PARAMS"
        );
    }

    #[tokio::test]
    async fn test_spawn_managed_empty_shell_still_works() {
        // 空 shell 走平台默认，跟阶段 1 前行为一致（无回归）
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "echo hi",
            "shell": ""
        });
        let id = serde_json::json!("shell-empty-1");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        assert!(
            resp.get("result").is_some(),
            "空 shell 应走默认且 OK: {resp}"
        );
        assert_eq!(resp["result"]["status"], "spawned");
    }

    // ── Sprint 7 阶段 4 (2026-04-23): sandbox_mode opt-in 分派测试 ──────

    #[tokio::test]
    async fn test_spawn_managed_sandbox_mode_unset_goes_direct() {
        // 未设 sandbox_mode → 走 direct path（无回归），返回里 sandbox_backend=none
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "echo hi",
            "shell": "bash",
        });
        let id = serde_json::json!("sandbox-mode-unset");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        assert!(resp.get("result").is_some(), "should have result: {resp}");
        assert_eq!(resp["result"]["status"], "spawned");
        assert_eq!(
            resp["result"]["sandbox_backend"], "none",
            "direct path 应明确返 sandbox_backend=none (阶段 4 诚实标注)"
        );
    }

    #[tokio::test]
    async fn test_spawn_managed_sandbox_mode_none_equivalent_to_unset() {
        // 显式 "none" 跟未设等价
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "echo hi",
            "shell": "bash",
            "sandboxMode": "none",
        });
        let id = serde_json::json!("sandbox-mode-none");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        assert!(resp.get("result").is_some(), "should have result: {resp}");
        assert_eq!(resp["result"]["sandbox_backend"], "none");
    }

    #[tokio::test]
    async fn test_spawn_managed_dangerously_disable_overrides_mode() {
        // dangerouslyDisableSandbox: true 强制 direct，即便 sandbox_mode=enforced
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "echo hi",
            "shell": "bash",
            "sandboxMode": "enforced",
            "dangerouslyDisableSandbox": true,
        });
        let id = serde_json::json!("sandbox-bypass-wins");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        assert!(resp.get("result").is_some(), "bypass 应走 direct: {resp}");
        assert_eq!(resp["result"]["sandbox_backend"], "none");
    }

    #[tokio::test]
    #[cfg(windows)]
    async fn test_spawn_managed_sandbox_mode_enforced_windows_errors() {
        // Windows 下 enforced 因 AsyncSandboxRunner 返 PlatformNotSupported（DLL init
        // 根因未修，Round 3 独立立项）→ 调用应明确报错，**不静默降级**。
        // 调用方应理解后改传 dangerouslyDisableSandbox:true（UI `!cmd` 场景）。
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "echo hi",
            "shell": "cmd",
            "sandboxMode": "enforced",
        });
        let id = serde_json::json!("sandbox-windows-enforced");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        assert!(
            resp.get("error").is_some(),
            "Windows 下 enforced 应返 error（不静默降级）: {resp}"
        );
    }

    #[tokio::test]
    async fn test_spawn_managed_sandbox_mode_case_insensitive() {
        // "ENFORCED" / "Enforced" 等价于 "enforced"（大小写不敏感）
        // Windows 下应报同样的 enforced-unavailable error
        let executor = Arc::new(CommandExecutor::new());
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        let params = serde_json::json!({
            "command": "echo hi",
            "shell": "bash",
            "sandboxMode": "  Enforced  ",  // 带空白，大小写混合
            "dangerouslyDisableSandbox": false,
        });
        let id = serde_json::json!("sandbox-ci-1");
        let resp = handle_spawn_managed(&params, &id, executor, tx).await;
        // Windows 下 error；Linux/macOS 下 Ok（假设有 runner）
        #[cfg(windows)]
        {
            assert!(
                resp.get("error").is_some(),
                "Windows Enforced (case-insensitive) should error: {resp}"
            );
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            // 可能成功（有 runner）或失败（权限等），两种状态都接受；关键是不 panic
            let _ = resp;
        }
    }
}

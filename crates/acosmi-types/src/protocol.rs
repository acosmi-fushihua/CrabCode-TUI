//! Go → Rust 能力申请协议（CapabilityRequest）
//!
//! 定义 Go 调度中枢向 Rust 执行层申请能力的请求/响应格式。

use serde::{Deserialize, Serialize};

/// 能力家族
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityFamily {
    /// 执行外部命令（一次性）
    Exec,
    /// 启动托管进程
    SpawnManaged,
    /// 文件系统读取
    FsRead,
    /// 文件系统写入
    FsWrite,
    /// 文件系统状态查询
    FsStat,
    /// 平台工具链调用
    PlatformTool,
    /// 定时任务调度（Cron 统一）
    Cron,
}

/// 进程生命周期类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    /// 一次性执行
    Oneshot,
    /// 长时间运行
    LongRunning,
    /// 批量执行
    Batch,
}

/// 执行策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecPolicy {
    /// 是否启用沙箱
    #[serde(default)]
    pub sandbox: Option<bool>,
    /// 超时时间（毫秒）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// 工作目录
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// stdout+stderr 合计最大输出字节数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<u64>,
    /// 一次性写入子进程 stdin 的数据（Base64 编码）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_data: Option<String>,
    /// `fs_read/fs_write` 操作允许访问的路径前缀白名单
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_path_prefixes: Option<Vec<String>>,
    /// 是否继承 Rust 进程的环境变量
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inherit_env: Option<bool>,
}

/// 批量执行项
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchItem {
    /// 批次内单条命令
    pub command: String,
    /// 命令参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// 覆盖外层 `env_overrides` 的局部环境变量
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_overrides: Option<std::collections::HashMap<String, Option<String>>>,
    /// 覆盖外层 policy.cwd 的局部工作目录
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_override: Option<String>,
    /// 覆盖外层 `policy.stdin_data` 的局部 stdin 输入
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin_data: Option<String>,
    /// 覆盖外层 `policy.timeout_ms` 的局部超时
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms_override: Option<u64>,
    /// 批次项本地标识符
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    /// 此项失败时是否中止后续批次项
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_fast: Option<bool>,
}

/// 能力申请请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CapabilityReq {
    /// 请求 ID
    pub request_id: String,
    /// OpenTelemetry `trace_id`
    pub trace_id: String,
    /// OpenTelemetry `span_id`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,
    /// 能力家族
    pub family: CapabilityFamily,
    /// 命令名
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// 命令参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// 进程级环境变量覆盖
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_overrides: Option<std::collections::HashMap<String, Option<String>>>,
    /// 执行策略
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ExecPolicy>,
    /// 生命周期
    pub lifecycle: Lifecycle,
    /// 批量执行项
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_items: Option<Vec<BatchItem>>,
}

/// 能力申请响应状态
///
/// 序列化后的字符串值 MUST 与 `contracts/schemas/capability-response.schema.json`
/// 保持一致："ok" / "denied" / "timeout" / "error"。
/// 2026-04-23 根因补全: Go 侧 `capability.go` 旧值 "success" 为契约偏离，已随本轮改为 "ok"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum CapabilityStatus {
    Ok,
    Denied,
    Timeout,
    #[default]
    Error,
}

/// 托管进程句柄
///
/// 字段对齐 `capability-response.schema.json#/properties/process_handle`。
/// 2026-04-23 根因补全: 原先只有 `pid` + `handle_id` 两个字段，与 schema 规定的
/// `stdin_write_id / stdout_read_id / stderr_read_id / kill_id / shell_type` 5 字段严重不符。
/// 本版本扩展为 schema 完整字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedProcessHandle {
    /// 进程 ID
    pub pid: u32,
    /// Rust 侧分配的 stdin 写入通道 ID
    pub stdin_write_id: String,
    /// Rust 侧分配的 stdout 读取通道 ID（事件中携带此 ID 供 Go 路由）
    pub stdout_read_id: String,
    /// stderr 独立读取通道 ID（可选）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_read_id: Option<String>,
    /// kill 信号句柄 ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kill_id: Option<String>,
    /// 实际使用的 shell 类型（信息性）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell_type: Option<String>,
}

/// 能力申请响应
///
/// 字段名对齐 `capability-response.schema.json`。
/// 2026-04-23 根因补全: 原字段 `result` / `handle` 与 schema/Go contracts 的
/// `exec_result` / `process_handle` 不符，此轮重命名并补全 schema 必需字段。
///
/// 构造推荐：`CapabilityResp { status, ..Default::default() }`；只填 status +
/// 本次响应有值的字段，其余走 Default。
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CapabilityResp {
    /// 与对应 `CapabilityRequest` 的 `request_id` 一一对应
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub request_id: String,
    /// 透传用于日志追踪
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// 状态
    pub status: CapabilityStatus,
    /// 拒绝原因（status=denied 时机器可读代码）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial_reason: Option<String>,
    /// 拒绝消息（人类可读）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denial_message: Option<String>,
    /// 执行结果（oneshot/batch 时返回）
    #[serde(
        default,
        rename = "exec_result",
        skip_serializing_if = "Option::is_none"
    )]
    pub exec_result: Option<super::exec_types::ExecResult>,
    /// `托管进程句柄（spawn_managed` 时返回）
    #[serde(
        default,
        rename = "process_handle",
        skip_serializing_if = "Option::is_none"
    )]
    pub process_handle: Option<ManagedProcessHandle>,
    /// 批量结果（lifecycle=batch 时返回）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_results: Option<Vec<serde_json::Value>>,
    /// 错误信息
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<super::error::CrabError>,
}

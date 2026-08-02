//! 统一执行请求/结果类型

use serde::{Deserialize, Serialize};

/// 命令家族（用于路由和权限判断）
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandFamily {
    Git,
    Gh,
    Ssh,
    Tmux,
    Rg,
    Npm,
    Pnpm,
    Shell,
    PlatformTool,
    LspServer,
    McpServer,
    Other(String),
}

/// 统一执行请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecReq {
    /// 命令家族
    pub family: CommandFamily,
    /// 可执行文件路径或名称
    pub program: String,
    /// 命令参数
    pub args: Vec<String>,
    /// 工作目录
    #[serde(default)]
    pub cwd: Option<String>,
    /// 环境变量覆盖
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// 标准输入
    #[serde(default)]
    pub stdin: Option<String>,
    /// 超时时间（毫秒），0 表示无超时
    #[serde(default)]
    pub timeout_ms: u64,
    /// 是否启用沙箱
    #[serde(default)]
    pub sandbox: bool,
    /// 中止信号标识（用于关联 abort 请求）
    #[serde(default)]
    pub abort_token: Option<String>,
}

/// 统一执行结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    /// 标准输出
    pub stdout: String,
    /// 标准错误
    pub stderr: String,
    /// 退出码
    #[serde(rename = "exit_code")]
    pub code: i32,
    /// 是否超时
    #[serde(default)]
    pub timed_out: bool,
    /// 是否被信号终止
    #[serde(default)]
    pub killed: bool,
    /// 执行耗时（毫秒）
    #[serde(default)]
    pub duration_ms: u64,
    /// 错误消息（spawn 失败等）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 批量执行请求
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchExecReq {
    /// 批量命令列表
    pub commands: Vec<ExecReq>,
    /// 是否在首个失败时停止
    #[serde(default)]
    pub fail_fast: bool,
    /// 最大并发数（0 表示串行）
    #[serde(default)]
    pub max_concurrency: usize,
}

/// 批量执行结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchExecResult {
    /// 各命令结果
    pub results: Vec<ExecResult>,
    /// 是否全部成功
    pub all_success: bool,
}

/// 协议握手消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handshake {
    /// 协议版本
    pub protocol_version: String,
    /// 组件版本
    pub component_version: String,
    /// 支持的能力列表
    pub capabilities: Vec<String>,
}

impl ExecResult {
    /// 判断是否执行成功
    #[must_use]
    pub const fn success(&self) -> bool {
        self.code == 0 && !self.timed_out && !self.killed && self.error.is_none()
    }
}

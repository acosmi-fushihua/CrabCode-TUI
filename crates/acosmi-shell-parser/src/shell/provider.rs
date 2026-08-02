//! `ShellProvider` trait
//!
//! 等价于 TS shellProvider.ts

use crate::ShellType;
use std::collections::HashMap;

/// Shell Provider 接口
///
/// 提供构建执行命令、获取 spawn 参数和环境变量覆盖的抽象。
#[async_trait::async_trait]
pub trait ShellProvider: Send + Sync {
    /// Shell 类型
    fn shell_type(&self) -> ShellType;

    /// Shell 可执行文件路径
    fn shell_path(&self) -> &str;

    /// 是否以 detached 模式运行
    fn detached(&self) -> bool;

    /// 构建执行命令
    ///
    /// 返回 (命令字符串, CWD 文件路径)
    async fn build_exec_command(
        &self,
        command: &str,
        opts: ExecCommandOpts,
    ) -> Result<ExecCommandResult, crate::ShellParserError>;

    /// 获取 spawn 参数
    fn get_spawn_args(&self, command_string: &str) -> Vec<String>;

    /// 获取环境变量覆盖
    async fn get_environment_overrides(&self, command: &str) -> HashMap<String, String>;
}

/// 执行命令选项
#[derive(Debug, Clone)]
pub struct ExecCommandOpts {
    /// 命令 ID
    pub id: String,
    /// 沙箱临时目录（可选）
    pub sandbox_tmp_dir: Option<String>,
    /// 是否使用沙箱
    pub use_sandbox: bool,
}

/// 执行命令构建结果
#[derive(Debug, Clone)]
pub struct ExecCommandResult {
    /// 完整命令字符串
    pub command_string: String,
    /// CWD 跟踪文件路径
    pub cwd_file_path: String,
}

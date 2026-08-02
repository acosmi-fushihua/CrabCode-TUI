//! 跨平台进程启动与管理
//!
//! 提供 `ProcessBuilder`（建造者模式）和 `ManagedProcess`（进程句柄）两个核心类型。
//! 支持 Linux/macOS/Windows 三平台，自动处理进程组创建等平台差异。

use std::collections::HashMap;
use std::path::PathBuf;

use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tracing::debug;

use crate::tree_kill::{self, TreeKillResult};

/// 进程错误类型
#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    /// 进程启动失败
    #[error("进程启动失败: {0}")]
    SpawnFailed(#[source] std::io::Error),

    /// 等待进程退出失败
    #[error("等待进程退出失败: {0}")]
    WaitFailed(#[source] std::io::Error),

    /// 终止进程失败
    #[error("终止进程失败: {0}")]
    KillFailed(#[source] std::io::Error),

    /// 无法获取进程 PID
    #[error("无法获取进程 PID")]
    NoPid,
}

/// 标准输入配置
#[derive(Debug, Clone, Default)]
pub enum StdinConfig {
    /// 不接收标准输入（/dev/null）
    #[default]
    Null,
    /// 通过管道传输标准输入
    Piped,
    /// 继承父进程的标准输入
    Inherit,
}

/// 输出配置（stdout/stderr）
#[derive(Debug, Clone, Default)]
pub enum OutputConfig {
    /// 通过管道捕获输出
    #[default]
    Piped,
    /// 继承父进程的输出
    Inherit,
    /// 丢弃输出（/dev/null）
    Null,
}

/// 进程构建器（建造者模式）
///
/// # 示例
/// ```no_run
/// use acosmi_exec::process::{ProcessBuilder, OutputConfig, StdinConfig};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let mut proc = ProcessBuilder::new("echo")
///     .args(&["hello".to_string(), "world".to_string()])
///     .stdout(OutputConfig::Piped)
///     .stderr(OutputConfig::Piped)
///     .stdin(StdinConfig::Null)
///     .new_process_group(true)
///     .spawn()
///     .await?;
///
/// let code = proc.wait().await?;
/// println!("退出码: {code}");
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct ProcessBuilder {
    program: String,
    args: Vec<String>,
    envs: HashMap<String, String>,
    cwd: Option<PathBuf>,
    stdin_config: StdinConfig,
    stdout_config: OutputConfig,
    stderr_config: OutputConfig,
    new_process_group: bool,
}

impl ProcessBuilder {
    /// 创建新的进程构建器
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            envs: HashMap::new(),
            cwd: None,
            stdin_config: StdinConfig::default(),
            stdout_config: OutputConfig::default(),
            stderr_config: OutputConfig::default(),
            new_process_group: false,
        }
    }

    /// 设置命令参数
    #[must_use]
    pub fn args(mut self, args: &[String]) -> Self {
        self.args = args.to_vec();
        self
    }

    /// 添加单个环境变量
    #[must_use]
    pub fn env(mut self, key: &str, val: &str) -> Self {
        let _ = self.envs.insert(key.to_string(), val.to_string());
        self
    }

    /// 批量添加环境变量
    #[must_use]
    pub fn envs(mut self, envs: &HashMap<String, String>) -> Self {
        self.envs.extend(envs.clone());
        self
    }

    /// 设置工作目录
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// 设置标准输入配置
    #[must_use]
    pub const fn stdin(mut self, cfg: StdinConfig) -> Self {
        self.stdin_config = cfg;
        self
    }

    /// 设置标准输出配置
    #[must_use]
    pub const fn stdout(mut self, cfg: OutputConfig) -> Self {
        self.stdout_config = cfg;
        self
    }

    /// 设置标准错误配置
    #[must_use]
    pub const fn stderr(mut self, cfg: OutputConfig) -> Self {
        self.stderr_config = cfg;
        self
    }

    /// 是否创建新进程组
    ///
    /// - Unix: 通过 `setsid`/`setpgid(0, 0)` 创建新进程组
    /// - Windows: 通过 `CREATE_NEW_PROCESS_GROUP` 标志创建
    #[must_use]
    pub const fn new_process_group(mut self, yes: bool) -> Self {
        self.new_process_group = yes;
        self
    }

    /// 异步启动进程
    ///
    /// # Errors
    ///
    /// - `ProcessError::SpawnFailed`: 进程启动失败（命令不存在、权限不足等）
    /// - `ProcessError::NoPid`: 无法获取子进程 PID
    #[allow(clippy::unused_async)]
    pub async fn spawn(self) -> Result<ManagedProcess, ProcessError> {
        let mut cmd = tokio::process::Command::new(&self.program);

        // 设置参数
        cmd.args(&self.args);

        // 设置环境变量
        for (key, val) in &self.envs {
            cmd.env(key, val);
        }

        // 设置工作目录
        if let Some(ref cwd) = self.cwd {
            cmd.current_dir(cwd);
        }

        // 设置标准输入
        match self.stdin_config {
            StdinConfig::Null => {
                cmd.stdin(std::process::Stdio::null());
            }
            StdinConfig::Piped => {
                cmd.stdin(std::process::Stdio::piped());
            }
            StdinConfig::Inherit => {
                cmd.stdin(std::process::Stdio::inherit());
            }
        }

        // 设置标准输出
        match self.stdout_config {
            OutputConfig::Piped => {
                cmd.stdout(std::process::Stdio::piped());
            }
            OutputConfig::Inherit => {
                cmd.stdout(std::process::Stdio::inherit());
            }
            OutputConfig::Null => {
                cmd.stdout(std::process::Stdio::null());
            }
        }

        // 设置标准错误
        match self.stderr_config {
            OutputConfig::Piped => {
                cmd.stderr(std::process::Stdio::piped());
            }
            OutputConfig::Inherit => {
                cmd.stderr(std::process::Stdio::inherit());
            }
            OutputConfig::Null => {
                cmd.stderr(std::process::Stdio::null());
            }
        }

        // 平台特定：创建新进程组
        if self.new_process_group {
            Self::setup_process_group(&mut cmd);
        }

        debug!(
            program = %self.program,
            args = ?self.args,
            cwd = ?self.cwd,
            new_process_group = self.new_process_group,
            "启动进程"
        );

        let mut child = cmd.spawn().map_err(ProcessError::SpawnFailed)?;
        let pid = child.id().ok_or(ProcessError::NoPid)?;

        debug!(pid, "进程已启动");

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();

        Ok(ManagedProcess {
            pid,
            child,
            stdout,
            stderr,
            stdin,
        })
    }

    /// Unix 平台：通过 `pre_exec` 设置新进程组
    #[cfg(unix)]
    fn setup_process_group(cmd: &mut tokio::process::Command) {
        // SAFETY: setpgid(0, 0) 在 fork 后、exec 前调用是安全的，
        // 它将子进程放入以自身 PID 为组长的新进程组
        #[allow(unsafe_code)]
        unsafe {
            let _ = cmd.pre_exec(|| {
                let result = libc::setpgid(0, 0);
                if result == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }

    /// Windows 平台：设置 `CREATE_NEW_PROCESS_GROUP` 标志
    #[cfg(windows)]
    fn setup_process_group(cmd: &mut tokio::process::Command) {
        // `tokio::process::Command` 在 Windows 上已内置 `creation_flags` 方法
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP);
    }
}

/// 托管进程句柄
///
/// 封装 `tokio::process::Child`，提供统一的进程管理接口。
#[allow(clippy::missing_fields_in_debug)]
pub struct ManagedProcess {
    /// 进程 ID
    pub pid: u32,
    /// 底层 tokio 子进程（不含在 Debug 输出中）
    child: Child,
    /// 标准输出（可 take）
    pub stdout: Option<ChildStdout>,
    /// 标准错误（可 take）
    pub stderr: Option<ChildStderr>,
    /// 标准输入（可 take）
    stdin: Option<ChildStdin>,
}

impl std::fmt::Debug for ManagedProcess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ManagedProcess")
            .field("pid", &self.pid)
            .field("has_stdout", &self.stdout.is_some())
            .field("has_stderr", &self.stderr.is_some())
            .field("has_stdin", &self.stdin.is_some())
            .finish_non_exhaustive()
    }
}

impl ManagedProcess {
    /// 等待进程退出，返回退出码
    ///
    /// 在 Unix 上，如果进程被信号终止且无退出码，返回 -1。
    ///
    /// # Errors
    ///
    /// 返回 `ProcessError::WaitFailed` 如果等待系统调用失败。
    pub async fn wait(&mut self) -> Result<i32, ProcessError> {
        let status = self.child.wait().await.map_err(ProcessError::WaitFailed)?;
        let code = status.code().unwrap_or(-1);
        debug!(pid = self.pid, code, "进程已退出");
        Ok(code)
    }

    /// 终止单个进程（不终止子进程树）
    ///
    /// # Errors
    ///
    /// 返回 `ProcessError::KillFailed` 如果终止系统调用失败。
    pub async fn kill(&mut self) -> Result<(), ProcessError> {
        debug!(pid = self.pid, "终止单个进程");
        self.child.kill().await.map_err(ProcessError::KillFailed)
    }

    /// 终止进程树（包括所有子进程）
    pub async fn kill_tree(&mut self) -> TreeKillResult {
        debug!(pid = self.pid, "终止进程树");
        tree_kill::kill_process_tree_force(self.pid).await
    }

    /// 取出标准输入句柄（只能调用一次）
    pub const fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// 取出标准输出句柄（只能调用一次）
    pub const fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }

    /// 取出标准错误句柄（只能调用一次）
    pub const fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }
}

impl Drop for ManagedProcess {
    fn drop(&mut self) {
        // 尝试终止仍在运行的子进程，防止孤儿进程
        if matches!(self.child.try_wait(), Ok(None)) {
            tracing::warn!(
                pid = self.pid,
                "ManagedProcess dropped while child still running, killing"
            );
            // start_kill 是非异步的（仅发送信号），适合在 Drop 中使用
            let _ = self.child.start_kill();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_process_builder_default() {
        let builder = ProcessBuilder::new("test_program");
        assert_eq!(builder.program, "test_program");
        assert!(builder.args.is_empty());
        assert!(builder.envs.is_empty());
        assert!(builder.cwd.is_none());
        assert!(!builder.new_process_group);
    }

    #[test]
    fn test_process_builder_chain() {
        let mut envs = HashMap::new();
        let _ = envs.insert("KEY2".to_string(), "VAL2".to_string());

        let builder = ProcessBuilder::new("program")
            .args(&["arg1".to_string(), "arg2".to_string()])
            .env("KEY1", "VAL1")
            .envs(&envs)
            .cwd("/tmp")
            .stdin(StdinConfig::Piped)
            .stdout(OutputConfig::Null)
            .stderr(OutputConfig::Inherit)
            .new_process_group(true);

        assert_eq!(builder.program, "program");
        assert_eq!(builder.args, vec!["arg1", "arg2"]);
        assert_eq!(builder.envs.get("KEY1").map(String::as_str), Some("VAL1"));
        assert_eq!(builder.envs.get("KEY2").map(String::as_str), Some("VAL2"));
        assert_eq!(builder.cwd, Some(PathBuf::from("/tmp")));
        assert!(builder.new_process_group);
    }

    #[test]
    fn test_stdin_config_default() {
        let cfg = StdinConfig::default();
        assert!(matches!(cfg, StdinConfig::Null));
    }

    #[test]
    fn test_output_config_default() {
        let cfg = OutputConfig::default();
        assert!(matches!(cfg, OutputConfig::Piped));
    }

    #[tokio::test]
    async fn test_spawn_echo() {
        // 在 Windows 上使用 cmd /C echo，在 Unix 上使用 echo
        let mut proc = if cfg!(windows) {
            ProcessBuilder::new("cmd")
                .args(&["/C".to_string(), "echo".to_string(), "hello".to_string()])
                .stdout(OutputConfig::Piped)
                .stderr(OutputConfig::Piped)
                .stdin(StdinConfig::Null)
                .spawn()
                .await
                .expect("启动进程失败")
        } else {
            ProcessBuilder::new("echo")
                .args(&["hello".to_string()])
                .stdout(OutputConfig::Piped)
                .stderr(OutputConfig::Piped)
                .stdin(StdinConfig::Null)
                .spawn()
                .await
                .expect("启动进程失败")
        };

        assert!(proc.pid > 0);
        let code = proc.wait().await.expect("等待进程失败");
        assert_eq!(code, 0);
    }

    #[tokio::test]
    async fn test_spawn_nonexistent() {
        let result = ProcessBuilder::new("this_program_does_not_exist_12345")
            .spawn()
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ProcessError::SpawnFailed(_)));
    }

    #[tokio::test]
    async fn test_take_handles() {
        let mut proc = if cfg!(windows) {
            ProcessBuilder::new("cmd")
                .args(&["/C".to_string(), "echo".to_string(), "test".to_string()])
                .stdout(OutputConfig::Piped)
                .stderr(OutputConfig::Piped)
                .stdin(StdinConfig::Piped)
                .spawn()
                .await
                .expect("启动进程失败")
        } else {
            ProcessBuilder::new("echo")
                .args(&["test".to_string()])
                .stdout(OutputConfig::Piped)
                .stderr(OutputConfig::Piped)
                .stdin(StdinConfig::Piped)
                .spawn()
                .await
                .expect("启动进程失败")
        };

        // 第一次 take 应该成功
        assert!(proc.take_stdout().is_some());
        assert!(proc.take_stderr().is_some());
        assert!(proc.take_stdin().is_some());

        // 第二次 take 应该返回 None
        assert!(proc.take_stdout().is_none());
        assert!(proc.take_stderr().is_none());
        assert!(proc.take_stdin().is_none());

        let _ = proc.wait().await;
    }

    #[tokio::test]
    async fn test_spawn_with_env() {
        let mut proc = if cfg!(windows) {
            ProcessBuilder::new("cmd")
                .args(&[
                    "/C".to_string(),
                    "echo".to_string(),
                    "%TEST_VAR%".to_string(),
                ])
                .env("TEST_VAR", "hello_env")
                .stdout(OutputConfig::Piped)
                .stdin(StdinConfig::Null)
                .spawn()
                .await
                .expect("启动进程失败")
        } else {
            ProcessBuilder::new("sh")
                .args(&["-c".to_string(), "echo $TEST_VAR".to_string()])
                .env("TEST_VAR", "hello_env")
                .stdout(OutputConfig::Piped)
                .stdin(StdinConfig::Null)
                .spawn()
                .await
                .expect("启动进程失败")
        };

        let code = proc.wait().await.expect("等待进程失败");
        assert_eq!(code, 0);
    }

    #[test]
    fn test_process_error_display() {
        let err = ProcessError::NoPid;
        assert_eq!(format!("{err}"), "无法获取进程 PID");

        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "not found");
        let err = ProcessError::SpawnFailed(io_err);
        assert!(format!("{err}").contains("进程启动失败"));
    }

    #[test]
    fn test_managed_process_debug() {
        // 仅测试 Debug trait 不 panic
        let builder = ProcessBuilder::new("test");
        let debug_str = format!("{builder:?}");
        assert!(debug_str.contains("test"));
    }
}

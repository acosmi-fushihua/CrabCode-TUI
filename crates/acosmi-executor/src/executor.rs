//! 核心命令执行逻辑
//!
//! `CommandExecutor` 是统一命令执行器的核心结构体，负责：
//! - 根据 `ExecReq` 构建并启动子进程（复用 `acosmi-exec::ProcessBuilder`）
//! - 流式读取 stdout/stderr 并限制输出大小（复用 `acosmi-exec::OutputCollector`）
//! - 处理超时（通过 acosmi-exec 终止进程树）
//! - 通过 `abort_token` 支持外部中止
//! - 记录执行耗时

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;

use acosmi_exec::OutputCollector;
use acosmi_exec::process::{ManagedProcess, OutputConfig, ProcessBuilder, StdinConfig};
use acosmi_types::exec_types::{ExecReq, ExecResult};
use tracing::{debug, error, info, warn};

use crate::family::{get_family_config, resolve_program};

/// 中止句柄，持有子进程的 PID，用于从外部终止正在运行的进程
#[derive(Debug, Clone)]
pub struct AbortHandle {
    /// 子进程 PID
    pid: u32,
}

/// 统一命令执行器
///
/// 管理活跃进程的跟踪（用于 abort），并执行 `ExecReq` 返回 `ExecResult`。
pub struct CommandExecutor {
    /// `活跃进程跟踪表：abort_token` -> `AbortHandle`
    active_processes: Arc<Mutex<HashMap<String, AbortHandle>>>,
}

impl CommandExecutor {
    /// 创建新的命令执行器实例
    #[must_use]
    pub fn new() -> Self {
        Self {
            active_processes: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 核心执行方法：接收 `ExecReq` 并返回 `ExecResult`
    ///
    /// 执行流程：
    /// 1. 获取命令家族配置（FamilyConfig）
    /// 2. 解析可执行文件路径
    /// 3. 通过 `ProcessBuilder` 构建并启动子进程
    /// 4. 设置 cwd、env、stdin
    /// 5. 应用超时控制
    /// 6. 处理 `abort_token`
    /// 7. 收集结果并返回
    pub async fn execute(&self, req: ExecReq) -> ExecResult {
        let start = Instant::now();

        // 1. 获取家族配置
        let family_config = get_family_config(&req.family);

        // 2. require_sandbox 强制 → shell 包装（**非真实隔离**，P2-3 命名诚实化 2026-06-05）
        //
        // 诚实边界：本 `CommandExecutor` 路径对 `sandbox=true` / `require_sandbox` 的处理
        // 仅是「走 shell 包装」（下方 build_shell_command），**不**调用 acosmi-sandbox 的
        // Landlock/Seatbelt/Job-Object 真隔离。真实沙箱隔离在 supervisor 的 spawnManaged
        // `sandbox_mode=enforced` 路径（acosmi-supervisor ipc.rs），不是这里。故变量名为
        // `effective_shell_wrap`（曾误名 `effective_sandbox`，会让读者以为此处做了真隔离）。
        let effective_shell_wrap = if family_config.require_sandbox && !req.sandbox {
            warn!(
                family = ?req.family,
                "命令家族要求沙箱但请求未启用 → 已自动启用 shell 包装（注：此路径仅 shell 包装，非真实隔离）"
            );
            true
        } else {
            req.sandbox
        };

        // 3. 解析可执行文件路径
        let program_path = match resolve_program(&req.family, &req.program) {
            Ok(p) => p,
            Err(e) => {
                error!(program = %req.program, error = %e, "可执行文件解析失败");
                return ExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    code: -1,
                    timed_out: false,
                    killed: false,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("可执行文件解析失败: {e}")),
                };
            }
        };

        debug!(
            program = %program_path.display(),
            args = ?req.args,
            family = ?req.family,
            shell_wrap = effective_shell_wrap,
            "准备执行命令"
        );

        // 4. 构建 ProcessBuilder（考虑 shell 包装）
        let (exec_program, exec_args) = if family_config.shell_wrap || effective_shell_wrap {
            build_shell_command(&program_path, &req.args)
        } else {
            (program_path.to_string_lossy().to_string(), req.args.clone())
        };

        let stdin_cfg = if req.stdin.is_some() {
            StdinConfig::Piped
        } else {
            StdinConfig::Null
        };

        let mut builder = ProcessBuilder::new(&exec_program)
            .args(&exec_args)
            .stdout(OutputConfig::Piped)
            .stderr(OutputConfig::Piped)
            .stdin(stdin_cfg)
            .new_process_group(true)
            .envs(&req.env);

        if let Some(ref cwd) = req.cwd {
            builder = builder.cwd(cwd);
        }

        // 5. 启动进程
        let mut proc = match builder.spawn().await {
            Ok(p) => p,
            Err(e) => {
                error!(program = %exec_program, error = %e, "进程启动失败");
                return ExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    code: -1,
                    timed_out: false,
                    killed: false,
                    duration_ms: start.elapsed().as_millis() as u64,
                    error: Some(format!("进程启动失败: {e}")),
                };
            }
        };

        let pid = proc.pid;
        info!(pid, program = %exec_program, "进程已启动");

        // 6. 注册 abort_token
        let abort_token = req.abort_token.clone();
        if let Some(ref token) = abort_token {
            let mut map = self.active_processes.lock().await;
            if let Some(old) = map.insert(token.clone(), AbortHandle { pid }) {
                warn!(
                    token,
                    old_pid = old.pid,
                    new_pid = pid,
                    "abort_token 重复使用，前一个进程可能未被正确清理"
                );
            }
        }

        // 7. 写入 stdin（如果有的话）
        if let Some(ref stdin_data) = req.stdin
            && let Some(mut stdin_handle) = proc.take_stdin()
        {
            use tokio::io::AsyncWriteExt;
            // 写入 stdin 不应阻塞太久，给一个简短的超时
            let write_result = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                stdin_handle.write_all(stdin_data.as_bytes()),
            )
            .await;
            match write_result {
                Ok(Ok(())) => {
                    // 关闭 stdin，通知子进程输入结束
                    drop(stdin_handle);
                }
                Ok(Err(e)) => {
                    warn!(error = %e, "写入 stdin 失败");
                }
                Err(_) => {
                    warn!("写入 stdin 超时");
                }
            }
        }

        // 8. 流式读取 stdout/stderr + 超时控制
        let max_output = family_config.max_output_bytes;
        let timeout_ms = if req.timeout_ms > 0 {
            req.timeout_ms
        } else {
            family_config.default_timeout_ms
        };

        let result = execute_with_timeout(&mut proc, max_output, timeout_ms).await;

        // 9. 清理 abort_token 注册
        if let Some(ref token) = abort_token {
            let mut map = self.active_processes.lock().await;
            let _ = map.remove(token);
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            ExecuteOutcome::Completed {
                stdout,
                stderr,
                code,
            } => {
                info!(pid, code, duration_ms, "进程正常退出");
                ExecResult {
                    stdout,
                    stderr,
                    code,
                    timed_out: false,
                    killed: false,
                    duration_ms,
                    error: None,
                }
            }
            ExecuteOutcome::TimedOut { stdout, stderr } => {
                warn!(pid, timeout_ms, "进程超时，已终止进程树");
                // 进程树已在 execute_with_timeout 中被终止，不再重复 kill（#4 修复）
                ExecResult {
                    stdout,
                    stderr,
                    code: -1,
                    timed_out: true,
                    killed: true,
                    duration_ms,
                    error: Some(format!("执行超时 ({timeout_ms}ms)")),
                }
            }
            ExecuteOutcome::Error { message } => {
                error!(pid, error = %message, "进程执行出错");
                ExecResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    code: -1,
                    timed_out: false,
                    killed: false,
                    duration_ms,
                    error: Some(message),
                }
            }
        }
    }

    /// 注册外部管理的进程到 `abort_token` 跟踪表
    ///
    /// 用于 spawnManaged 等场景：调用方自行启动进程后，注册 PID 以支持 `abort()` 终止。
    /// 返回 true 表示成功注册，false 表示 token 已存在（旧值被覆盖）。
    pub async fn register_abort(&self, token: String, pid: u32) -> bool {
        let mut map = self.active_processes.lock().await;
        let replaced = map.insert(token, AbortHandle { pid });
        replaced.is_none()
    }

    /// 从 `abort_token` 跟踪表中移除指定 token
    ///
    /// 进程正常完成后调用，清理跟踪条目。
    pub async fn unregister_abort(&self, token: &str) {
        let mut map = self.active_processes.lock().await;
        let _ = map.remove(token);
    }

    /// 通过 `abort_token` 终止正在运行的进程
    ///
    /// 返回 true 表示成功找到并终止了对应进程，false 表示未找到对应的 token。
    pub async fn abort(&self, token: &str) -> bool {
        let handle = {
            let mut map = self.active_processes.lock().await;
            map.remove(token)
        };

        if let Some(h) = handle {
            info!(token, pid = h.pid, "通过 abort_token 终止进程");
            let _ = acosmi_exec::kill_process_tree_force(h.pid).await;
            true
        } else {
            warn!(token, "未找到对应的 abort_token");
            false
        }
    }
}

impl Default for CommandExecutor {
    fn default() -> Self {
        Self::new()
    }
}

/// 命令执行结果的内部枚举
enum ExecuteOutcome {
    /// 进程正常完成
    Completed {
        stdout: String,
        stderr: String,
        code: i32,
    },
    /// 进程超时（进程树已被终止）
    TimedOut { stdout: String, stderr: String },
    /// 执行过程中发生错误
    Error { message: String },
}

/// 带超时控制地执行进程，同时流式读取 stdout/stderr
///
/// 使用 acosmi-exec 的 `OutputCollector` 收集输出，超时时终止进程树。
async fn execute_with_timeout(
    proc: &mut ManagedProcess,
    max_output: usize,
    timeout_ms: u64,
) -> ExecuteOutcome {
    // 取出 stdout/stderr 句柄用于并行收集
    let stdout_handle = proc.take_stdout();
    let stderr_handle = proc.take_stderr();

    // 启动 stdout 收集任务（复用 acosmi-exec::OutputCollector）
    let stdout_task = tokio::spawn(async move {
        match stdout_handle {
            Some(reader) => {
                let mut collector = OutputCollector::new(max_output);
                collector.collect_from(reader).await
            }
            None => String::new(),
        }
    });

    // 启动 stderr 收集任务
    let stderr_task = tokio::spawn(async move {
        match stderr_handle {
            Some(reader) => {
                let mut collector = OutputCollector::new(max_output);
                collector.collect_from(reader).await
            }
            None => String::new(),
        }
    });

    // 等待进程退出（带超时）
    let wait_result = if timeout_ms > 0 {
        let duration = std::time::Duration::from_millis(timeout_ms);
        // Err = elapsed (timeout); Ok wraps the wait() result we want to retain.
        tokio::time::timeout(duration, proc.wait()).await.ok()
    } else {
        // 无超时限制
        Some(proc.wait().await)
    };

    // 超时时终止进程树（关闭管道，使输出收集任务能正常结束）
    if wait_result.is_none() {
        let _ = proc.kill_tree().await;
    }

    // 收集 stdout
    let stdout = match stdout_task.await {
        Ok(data) => data,
        Err(e) => {
            warn!(error = %e, "stdout 收集任务失败");
            String::new()
        }
    };

    // 收集 stderr
    let stderr = match stderr_task.await {
        Ok(data) => data,
        Err(e) => {
            warn!(error = %e, "stderr 收集任务失败");
            String::new()
        }
    };

    match wait_result {
        Some(Ok(code)) => {
            // 正常退出
            ExecuteOutcome::Completed {
                stdout,
                stderr,
                code,
            }
        }
        Some(Err(e)) => ExecuteOutcome::Error {
            message: format!("等待进程退出失败: {e}"),
        },
        None => {
            // 超时（进程树已在上方被终止）
            ExecuteOutcome::TimedOut { stdout, stderr }
        }
    }
}

/// 构建 shell 包装的命令参数
///
/// 返回 (program, args) 元组：
/// - Windows: ("cmd.exe", ["/C", "escaped command line"])
/// - Unix: ("/bin/sh", ["-c", "escaped command line"])
fn build_shell_command(program: &std::path::Path, args: &[String]) -> (String, Vec<String>) {
    // 将程序和参数拼接成安全转义的完整命令行字符串
    let mut full_command = shell_escape(program.to_string_lossy().as_ref());
    for arg in args {
        full_command.push(' ');
        full_command.push_str(&shell_escape(arg));
    }

    #[cfg(windows)]
    {
        ("cmd.exe".to_string(), vec!["/C".to_string(), full_command])
    }

    #[cfg(not(windows))]
    {
        ("/bin/sh".to_string(), vec!["-c".to_string(), full_command])
    }
}

/// 安全的 shell 参数转义
///
/// - Unix: 使用单引号包裹，防止 `$()`、`` ` ``、`!` 等 shell 展开
/// - Windows: 使用双引号包裹，`%` 替换为 `%%` 防止环境变量展开
fn shell_escape(s: &str) -> String {
    if s.is_empty() {
        #[cfg(not(windows))]
        return "''".to_string();
        #[cfg(windows)]
        return "\"\"".to_string();
    }

    #[cfg(not(windows))]
    {
        // Unix: 单引号防止所有 shell 展开（$(), ``, !, 变量替换等）
        // 单引号内唯一需要处理的是单引号本身：'arg'\''rest'
        let needs_quoting = s.contains(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '\'' | '"'
                        | '\\'
                        | '&'
                        | '|'
                        | ';'
                        | '<'
                        | '>'
                        | '('
                        | ')'
                        | '$'
                        | '`'
                        | '!'
                        | '{'
                        | '}'
                        | '*'
                        | '?'
                        | '#'
                        | '~'
                        | '['
                        | ']'
                )
        });
        if needs_quoting {
            let escaped = s.replace('\'', "'\\''");
            format!("'{escaped}'")
        } else {
            s.to_string()
        }
    }

    #[cfg(windows)]
    {
        // Windows cmd.exe: 双引号包裹，% 替换为 %% 防止变量展开
        let needs_quoting = s.contains(|c: char| {
            c.is_whitespace()
                || matches!(c, '"' | '&' | '|' | '<' | '>' | '(' | ')' | '^' | '%' | '!')
        });
        if needs_quoting {
            let escaped = s.replace('%', "%%").replace('"', "\"\"");
            format!("\"{escaped}\"")
        } else {
            s.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acosmi_types::exec_types::CommandFamily;

    #[test]
    fn test_shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "hello");

        #[cfg(not(windows))]
        {
            assert_eq!(shell_escape(""), "''");
            assert_eq!(shell_escape("hello world"), "'hello world'");
        }
        #[cfg(windows)]
        {
            assert_eq!(shell_escape(""), "\"\"");
            assert_eq!(shell_escape("hello world"), "\"hello world\"");
        }
    }

    #[test]
    fn test_shell_escape_special_chars() {
        #[cfg(not(windows))]
        {
            // Unix: 双引号在单引号内是安全的
            assert_eq!(shell_escape("foo\"bar"), "'foo\"bar'");
            // 单引号需要特殊处理
            assert_eq!(shell_escape("it's"), "'it'\\''s'");
        }
        #[cfg(windows)]
        {
            let result = shell_escape("foo\"bar");
            assert!(result.starts_with('"'));
            assert!(result.contains("\"\""));
        }
    }

    #[test]
    fn test_shell_escape_injection_prevention() {
        // $() 命令替换必须被阻止
        #[cfg(not(windows))]
        {
            assert_eq!(shell_escape("$(whoami)"), "'$(whoami)'");
            assert_eq!(shell_escape("`whoami`"), "'`whoami`'");
        }
        // Windows %var% 展开必须被阻止
        #[cfg(windows)]
        {
            let result = shell_escape("%PATH%");
            assert!(result.contains("%%PATH%%"));
        }
    }

    #[test]
    fn test_build_shell_command() {
        let (prog, args) = build_shell_command(
            std::path::Path::new("/usr/bin/test"),
            &["arg1".to_string(), "arg with space".to_string()],
        );

        #[cfg(not(windows))]
        {
            assert_eq!(prog, "/bin/sh");
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "-c");
            assert!(args[1].contains("/usr/bin/test"));
            assert!(args[1].contains("'arg with space'"));
        }

        #[cfg(windows)]
        {
            assert_eq!(prog, "cmd.exe");
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "/C");
        }
    }

    #[tokio::test]
    async fn test_executor_new() {
        let executor = CommandExecutor::new();
        // 检查初始状态下 active_processes 为空
        let map = executor.active_processes.lock().await;
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn test_abort_nonexistent_token() {
        let executor = CommandExecutor::new();
        let result = executor.abort("nonexistent_token").await;
        assert!(!result);
    }

    #[tokio::test]
    async fn test_execute_nonexistent_program() {
        let executor = CommandExecutor::new();
        let req = ExecReq {
            family: CommandFamily::Other("test".into()),
            program: "__nonexistent_program_xyz__".into(),
            args: vec![],
            cwd: None,
            env: std::collections::HashMap::new(),
            stdin: None,
            timeout_ms: 5000,
            sandbox: false,
            abort_token: None,
        };
        let result = executor.execute(req).await;
        assert_eq!(result.code, -1);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    #[cfg(any(target_os = "windows", target_os = "linux", target_os = "macos"))]
    async fn test_execute_simple_command() {
        let executor = CommandExecutor::new();

        // 使用 Git 家族（require_sandbox=false）进行直接执行测试
        #[cfg(windows)]
        let req = ExecReq {
            family: CommandFamily::Git,
            program: "cmd.exe".into(),
            args: vec!["/C".into(), "echo".into(), "hello".into()],
            cwd: None,
            env: std::collections::HashMap::new(),
            stdin: None,
            timeout_ms: 5000,
            sandbox: false,
            abort_token: None,
        };

        #[cfg(not(windows))]
        let req = ExecReq {
            family: CommandFamily::Git,
            program: "/bin/echo".into(),
            args: vec!["hello".into()],
            cwd: None,
            env: std::collections::HashMap::new(),
            stdin: None,
            timeout_ms: 5000,
            sandbox: false,
            abort_token: None,
        };

        let result = executor.execute(req).await;
        assert_eq!(result.code, 0);
        assert!(result.stdout.contains("hello"));
        assert!(!result.timed_out);
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn test_require_sandbox_enforcement() {
        // Other 家族要求沙箱，即使请求未启用也应自动启用
        let executor = CommandExecutor::new();

        // 使用 shell 命令测试 sandbox 自动启用
        #[cfg(not(windows))]
        let req = ExecReq {
            family: CommandFamily::Other("test".into()),
            program: "echo".into(),
            args: vec!["sandbox_test".into()],
            cwd: None,
            env: std::collections::HashMap::new(),
            stdin: None,
            timeout_ms: 5000,
            sandbox: false, // 未启用，但 Other 家族 require_sandbox=true
            abort_token: None,
        };

        #[cfg(windows)]
        let req = ExecReq {
            family: CommandFamily::Other("test".into()),
            program: "cmd.exe".into(),
            args: vec!["/C".into(), "echo".into(), "sandbox_test".into()],
            cwd: None,
            env: std::collections::HashMap::new(),
            stdin: None,
            timeout_ms: 5000,
            sandbox: false,
            abort_token: None,
        };

        let result = executor.execute(req).await;
        // 即使未显式启用 sandbox，require_sandbox 也会自动启用 shell 包装
        // 命令应正常执行
        assert_eq!(
            result.code, 0,
            "sandbox enforcement should not break execution: {:?}",
            result.error
        );
        assert!(result.stdout.contains("sandbox_test"));
    }
}

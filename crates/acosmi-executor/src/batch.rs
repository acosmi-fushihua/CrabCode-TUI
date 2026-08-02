//! 批量命令执行 API
//!
//! 解决高频 git 场景：一次请求多条命令，减少 IPC 往返。
//! 支持串行执行和基于 Semaphore 的并发控制，以及 `fail_fast` 快速失败模式。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use acosmi_types::exec_types::{BatchExecReq, BatchExecResult, ExecResult};
use tracing::{debug, info, warn};

use crate::executor::CommandExecutor;

/// 批量命令执行器
///
/// 封装 `CommandExecutor`，提供批量执行能力：
/// - `串行模式（max_concurrency` == 0）：逐个执行命令
/// - `并发模式（max_concurrency` > 0）：用 Semaphore 控制最大并发数
/// - `fail_fast` 模式：首个失败即停止后续命令
pub struct BatchExecutor {
    executor: Arc<CommandExecutor>,
}

impl BatchExecutor {
    /// 创建批量执行器
    ///
    /// # 参数
    /// - `executor`: 底层命令执行器的共享引用
    #[must_use]
    pub const fn new(executor: Arc<CommandExecutor>) -> Self {
        Self { executor }
    }

    /// 批量执行命令
    ///
    /// # 执行策略
    /// - `max_concurrency == 0`：串行执行，逐个调用 executor.execute
    /// - `max_concurrency > 0`：用 `tokio::sync::Semaphore` 限制并发数
    /// - `fail_fast == true`：首个命令失败（code != 0 或有 error）时停止后续命令
    ///
    /// # 返回
    /// `BatchExecResult`，其中 results 按原始命令顺序排列
    pub async fn execute_batch(&self, req: BatchExecReq) -> BatchExecResult {
        // 空命令列表，直接返回
        if req.commands.is_empty() {
            debug!("批量执行：命令列表为空，直接返回");
            return BatchExecResult {
                results: vec![],
                all_success: true,
            };
        }

        let total = req.commands.len();
        info!(
            total,
            max_concurrency = req.max_concurrency,
            fail_fast = req.fail_fast,
            "开始批量执行"
        );

        let results = if req.max_concurrency == 0 {
            // 串行模式
            self.execute_serial(req.commands, req.fail_fast).await
        } else {
            // 并发模式
            self.execute_concurrent(req.commands, req.max_concurrency, req.fail_fast)
                .await
        };

        let all_success = results.iter().all(|r| r.code == 0 && r.error.is_none());

        info!(total, executed = results.len(), all_success, "批量执行完成");

        BatchExecResult {
            results,
            all_success,
        }
    }

    /// 串行执行模式
    ///
    /// `逐个执行命令，fail_fast` 时遇到失败立即返回已有结果。
    async fn execute_serial(
        &self,
        commands: Vec<acosmi_types::exec_types::ExecReq>,
        fail_fast: bool,
    ) -> Vec<ExecResult> {
        let mut results = Vec::with_capacity(commands.len());

        for (index, cmd) in commands.into_iter().enumerate() {
            debug!(index, program = %cmd.program, "串行执行命令");
            let result = self.executor.execute(cmd).await;

            let failed = result.code != 0 || result.error.is_some();
            results.push(result);

            if fail_fast && failed {
                warn!(index, "fail_fast 模式：命令失败，停止后续执行");
                break;
            }
        }

        results
    }

    /// 并发执行模式
    ///
    /// 使用 `tokio::sync::Semaphore` 控制最大并发数，
    /// 使用 `AtomicBool` 在 `fail_fast` 模式下通知其他任务停止。
    /// 结果按原始命令顺序（index）返回。
    async fn execute_concurrent(
        &self,
        commands: Vec<acosmi_types::exec_types::ExecReq>,
        max_concurrency: usize,
        fail_fast: bool,
    ) -> Vec<ExecResult> {
        let total = commands.len();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(max_concurrency));
        let cancelled = Arc::new(AtomicBool::new(false));

        // 使用 JoinSet 管理并发任务，每个任务返回 (index, ExecResult)
        let mut join_set = tokio::task::JoinSet::new();

        for (index, cmd) in commands.into_iter().enumerate() {
            let executor = Arc::clone(&self.executor);
            let sem = Arc::clone(&semaphore);
            let cancel_flag = Arc::clone(&cancelled);

            let _ = join_set.spawn(async move {
                // 在获取信号量之前检查是否已取消
                if cancel_flag.load(Ordering::Relaxed) {
                    return (
                        index,
                        ExecResult {
                            stdout: String::new(),
                            stderr: String::new(),
                            code: -1,
                            timed_out: false,
                            killed: true,
                            duration_ms: 0,
                            error: Some("批量执行已取消（fail_fast）".to_string()),
                        },
                    );
                }

                // 获取信号量许可
                let _permit = sem.acquire().await.expect("信号量不应关闭");

                // 获取许可后再次检查取消标志
                if cancel_flag.load(Ordering::Relaxed) {
                    return (
                        index,
                        ExecResult {
                            stdout: String::new(),
                            stderr: String::new(),
                            code: -1,
                            timed_out: false,
                            killed: true,
                            duration_ms: 0,
                            error: Some("批量执行已取消（fail_fast）".to_string()),
                        },
                    );
                }

                debug!(index, program = %cmd.program, "并发执行命令");
                let result = executor.execute(cmd).await;

                // 执行失败且 fail_fast 时设置取消标志
                if fail_fast && (result.code != 0 || result.error.is_some()) {
                    warn!(index, "fail_fast 模式：命令失败，通知取消后续任务");
                    cancel_flag.store(true, Ordering::Relaxed);
                }

                (index, result)
            });
        }

        // 收集所有结果
        let mut indexed_results: Vec<(usize, ExecResult)> = Vec::with_capacity(total);
        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok(pair) => indexed_results.push(pair),
                Err(e) => {
                    // JoinError 只在任务 panic 时发生，记录错误但继续
                    warn!(error = %e, "并发任务 panic");
                }
            }
        }

        // 按原始命令顺序排序
        indexed_results.sort_by_key(|(idx, _)| *idx);
        indexed_results.into_iter().map(|(_, r)| r).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acosmi_types::exec_types::{BatchExecReq, CommandFamily, ExecReq};
    use std::collections::HashMap;

    /// 构造一个简单的测试 ExecReq
    ///
    /// Windows: echo 是 cmd 内置命令，必须通过 cmd.exe /C echo 执行
    /// Unix: 使用独立的 /bin/echo
    fn make_echo_req(msg: &str) -> ExecReq {
        #[cfg(windows)]
        {
            ExecReq {
                family: CommandFamily::Other("test".into()),
                program: "cmd.exe".into(),
                args: vec!["/C".into(), "echo".into(), msg.to_string()],
                cwd: None,
                env: HashMap::new(),
                stdin: None,
                timeout_ms: 5000,
                sandbox: false,
                abort_token: None,
            }
        }
        #[cfg(not(windows))]
        {
            ExecReq {
                family: CommandFamily::Other("test".into()),
                program: "/bin/echo".into(),
                args: vec![msg.to_string()],
                cwd: None,
                env: HashMap::new(),
                stdin: None,
                timeout_ms: 5000,
                sandbox: false,
                abort_token: None,
            }
        }
    }

    /// 构造一个必定失败的 ExecReq（不存在的程序）
    fn make_failing_req() -> ExecReq {
        ExecReq {
            family: CommandFamily::Other("test".into()),
            program: "__nonexistent_program_xyz__".into(),
            args: vec![],
            cwd: None,
            env: HashMap::new(),
            stdin: None,
            timeout_ms: 5000,
            sandbox: false,
            abort_token: None,
        }
    }

    // === 空命令列表 ===

    #[tokio::test]
    async fn test_empty_commands() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        let req = BatchExecReq {
            commands: vec![],
            fail_fast: false,
            max_concurrency: 0,
        };

        let result = batch.execute_batch(req).await;
        assert!(result.results.is_empty());
        assert!(result.all_success);
    }

    // === 串行模式测试 ===

    #[tokio::test]
    async fn test_serial_single_command() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        let req = BatchExecReq {
            commands: vec![make_echo_req("hello")],
            fail_fast: false,
            max_concurrency: 0,
        };

        let result = batch.execute_batch(req).await;
        assert_eq!(result.results.len(), 1);
        assert_eq!(result.results[0].code, 0);
        assert!(result.results[0].stdout.contains("hello"));
        assert!(result.all_success);
    }

    #[tokio::test]
    async fn test_serial_multiple_commands() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        let req = BatchExecReq {
            commands: vec![
                make_echo_req("first"),
                make_echo_req("second"),
                make_echo_req("third"),
            ],
            fail_fast: false,
            max_concurrency: 0,
        };

        let result = batch.execute_batch(req).await;
        assert_eq!(result.results.len(), 3);
        assert!(result.results[0].stdout.contains("first"));
        assert!(result.results[1].stdout.contains("second"));
        assert!(result.results[2].stdout.contains("third"));
        assert!(result.all_success);
    }

    #[tokio::test]
    async fn test_serial_fail_fast_stops_on_failure() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        let req = BatchExecReq {
            commands: vec![
                make_echo_req("ok"),
                make_failing_req(), // 这个会失败
                make_echo_req("should_not_run"),
            ],
            fail_fast: true,
            max_concurrency: 0,
        };

        let result = batch.execute_batch(req).await;
        // fail_fast 模式下，第二个命令失败后停止，第三个不会执行
        assert_eq!(result.results.len(), 2);
        assert_eq!(result.results[0].code, 0);
        assert_ne!(result.results[1].code, 0);
        assert!(!result.all_success);
    }

    #[tokio::test]
    async fn test_serial_no_fail_fast_continues_after_failure() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        let req = BatchExecReq {
            commands: vec![
                make_echo_req("ok"),
                make_failing_req(), // 这个会失败
                make_echo_req("still_runs"),
            ],
            fail_fast: false,
            max_concurrency: 0,
        };

        let result = batch.execute_batch(req).await;
        // 不启用 fail_fast，所有命令都会执行
        assert_eq!(result.results.len(), 3);
        assert_eq!(result.results[0].code, 0);
        assert_ne!(result.results[1].code, 0);
        assert_eq!(result.results[2].code, 0);
        assert!(!result.all_success);
    }

    // === 并发模式测试 ===

    #[tokio::test]
    async fn test_concurrent_multiple_commands() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        let req = BatchExecReq {
            commands: vec![
                make_echo_req("alpha"),
                make_echo_req("beta"),
                make_echo_req("gamma"),
            ],
            fail_fast: false,
            max_concurrency: 2,
        };

        let result = batch.execute_batch(req).await;
        // 结果按原始顺序返回
        assert_eq!(result.results.len(), 3);
        assert!(result.results[0].stdout.contains("alpha"));
        assert!(result.results[1].stdout.contains("beta"));
        assert!(result.results[2].stdout.contains("gamma"));
        assert!(result.all_success);
    }

    #[tokio::test]
    async fn test_concurrent_fail_fast() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        // 第一个命令失败，后续命令应被取消
        let req = BatchExecReq {
            commands: vec![
                make_failing_req(),
                make_echo_req("might_cancel"),
                make_echo_req("might_cancel_too"),
            ],
            fail_fast: true,
            max_concurrency: 1, // 并发为 1 保证顺序性以便测试
        };

        let result = batch.execute_batch(req).await;
        // 所有任务都会有结果（有些可能是取消结果）
        assert!(!result.all_success);
        // 第一个必定失败
        assert_ne!(result.results[0].code, 0);
    }

    #[tokio::test]
    async fn test_concurrent_preserves_order() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        // 发起多个命令，验证结果顺序与输入一致
        let req = BatchExecReq {
            commands: vec![
                make_echo_req("cmd0"),
                make_echo_req("cmd1"),
                make_echo_req("cmd2"),
                make_echo_req("cmd3"),
            ],
            fail_fast: false,
            max_concurrency: 4,
        };

        let result = batch.execute_batch(req).await;
        assert_eq!(result.results.len(), 4);
        for (i, r) in result.results.iter().enumerate() {
            assert!(
                r.stdout.contains(&format!("cmd{i}")),
                "结果 {} 应包含 cmd{}，实际 stdout: {}",
                i,
                i,
                r.stdout
            );
        }
        assert!(result.all_success);
    }

    // === all_success 字段测试 ===

    #[tokio::test]
    async fn test_all_success_true_when_all_ok() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        let req = BatchExecReq {
            commands: vec![make_echo_req("a"), make_echo_req("b")],
            fail_fast: false,
            max_concurrency: 0,
        };

        let result = batch.execute_batch(req).await;
        assert!(result.all_success);
    }

    #[tokio::test]
    async fn test_all_success_false_when_any_fails() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        let req = BatchExecReq {
            commands: vec![make_echo_req("a"), make_failing_req()],
            fail_fast: false,
            max_concurrency: 0,
        };

        let result = batch.execute_batch(req).await;
        assert!(!result.all_success);
    }

    // === 并发数为 1 相当于串行 ===

    #[tokio::test]
    async fn test_concurrency_one_acts_like_serial() {
        let executor = Arc::new(CommandExecutor::new());
        let batch = BatchExecutor::new(executor);

        let req = BatchExecReq {
            commands: vec![
                make_echo_req("seq1"),
                make_echo_req("seq2"),
                make_echo_req("seq3"),
            ],
            fail_fast: false,
            max_concurrency: 1,
        };

        let result = batch.execute_batch(req).await;
        assert_eq!(result.results.len(), 3);
        assert!(result.all_success);
    }
}

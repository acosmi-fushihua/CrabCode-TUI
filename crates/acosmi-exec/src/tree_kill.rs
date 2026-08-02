//! 跨平台进程树终止（替代 tree-kill npm 包）
//!
//! 使用 `kill_tree` crate 作为底层实现，支持 Linux/macOS/Windows 三平台。
//! 提供优雅退出（先 SIGTERM 再 SIGKILL）和强制终止两种模式。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// 进程树终止选项
#[derive(Debug, Clone)]
pub struct TreeKillOptions {
    /// 强制终止（不等待优雅退出）
    pub force: bool,
    /// 优雅退出等待时间（仅在 `force=false` 时生效）
    pub grace_period: Duration,
    /// Unix 信号编号（如 9=SIGKILL, 15=SIGTERM），Windows 上忽略
    pub signal: Option<i32>,
}

impl Default for TreeKillOptions {
    fn default() -> Self {
        Self {
            force: false,
            grace_period: Duration::from_secs(5),
            signal: None,
        }
    }
}

/// 终止结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeKillResult {
    /// 被成功终止的 PID 列表
    pub killed_pids: Vec<u32>,
    /// 终止失败的列表：(PID, 错误原因)
    pub failed: Vec<(u32, String)>,
}

impl TreeKillResult {
    /// 创建一个空结果
    const fn new() -> Self {
        Self {
            killed_pids: Vec::new(),
            failed: Vec::new(),
        }
    }

    /// 合并另一个结果到自身（去重：已在 `killed_pids` 中的 PID 不重复添加）
    fn merge(&mut self, other: Self) {
        for pid in other.killed_pids {
            if !self.killed_pids.contains(&pid) {
                self.killed_pids.push(pid);
            }
        }
        // 排除已成功终止的 PID 的失败记录
        for (pid, reason) in other.failed {
            if !self.killed_pids.contains(&pid) {
                self.failed.push((pid, reason));
            }
        }
    }

    /// 是否全部成功（无失败项）
    #[must_use]
    pub fn all_success(&self) -> bool {
        self.failed.is_empty()
    }
}

/// 将 `kill_tree` crate 的输出转换为我们的 `TreeKillResult`
fn convert_kill_tree_outputs(outputs: kill_tree::Outputs) -> TreeKillResult {
    let mut result = TreeKillResult::new();
    for output in outputs {
        match output {
            kill_tree::Output::Killed { process_id, .. } => {
                result.killed_pids.push(process_id);
            }
            kill_tree::Output::MaybeAlreadyTerminated { process_id, source } => {
                // 进程可能已经退出，记录为失败但不算严重错误
                result
                    .failed
                    .push((process_id, format!("可能已终止: {source}")));
            }
        }
    }
    result
}

/// 使用指定信号执行一次进程树终止
async fn do_kill_tree(pid: u32, signal: &str) -> TreeKillResult {
    let signal = signal.to_string();
    debug!(pid, signal, "执行进程树终止");

    let result = tokio::task::spawn_blocking(move || {
        let config = kill_tree::Config {
            signal,
            include_target: true,
        };
        kill_tree::blocking::kill_tree_with_config(pid, &config)
    })
    .await;

    match result {
        Ok(Ok(outputs)) => convert_kill_tree_outputs(outputs),
        Ok(Err(e)) => {
            warn!(pid, error = %e, "kill_tree 调用失败");
            let mut r = TreeKillResult::new();
            r.failed.push((pid, format!("kill_tree 错误: {e}")));
            r
        }
        Err(e) => {
            warn!(pid, error = %e, "spawn_blocking 任务 panic");
            let mut r = TreeKillResult::new();
            r.failed.push((pid, format!("任务 panic: {e}")));
            r
        }
    }
}

/// 异步终止进程树
///
/// 根据 `options` 决定终止策略：
/// - `force=true`：直接发送 SIGKILL（Unix）/ `TerminateProcess`（Windows）
/// - `force=false`：先发送 SIGTERM，等待 `grace_period`，若未退出再发 SIGKILL
pub async fn kill_process_tree(pid: u32, options: TreeKillOptions) -> TreeKillResult {
    debug!(pid, ?options, "开始终止进程树");

    if options.force {
        // 强制模式：直接 SIGKILL
        // Windows 上 kill_tree 始终用 TerminateProcess，信号名称被忽略
        return do_kill_tree(pid, "SIGKILL").await;
    }

    // 优雅模式：先 SIGTERM，等待，再 SIGKILL
    // 确定第一步使用的信号
    let first_signal = match options.signal {
        Some(9) => "SIGKILL".to_string(),
        Some(15) | None => "SIGTERM".to_string(),
        Some(2) => "SIGINT".to_string(),
        Some(1) => "SIGHUP".to_string(),
        Some(n) => format!("{n}"),
    };

    let mut result = do_kill_tree(pid, &first_signal).await;

    // 如果第一步已经是 SIGKILL，不需要第二步
    if first_signal == "SIGKILL" {
        return result;
    }

    // 轮询等待优雅退出（每 100ms 检查一次进程是否已退出，避免无条件等满 grace_period）
    let poll_interval = Duration::from_millis(100);
    let grace_deadline = tokio::time::Instant::now() + options.grace_period;
    let mut exited_early = false;

    while tokio::time::Instant::now() < grace_deadline {
        if !is_process_alive(pid).await {
            debug!(pid, "进程在优雅退出期内已退出，提前结束等待");
            exited_early = true;
            break;
        }
        let remaining = grace_deadline - tokio::time::Instant::now();
        tokio::time::sleep(poll_interval.min(remaining)).await;
    }

    // 如果进程已退出，无需强制终止
    if exited_early {
        return result;
    }

    // 进程仍存活，强制终止
    let force_result = do_kill_tree(pid, "SIGKILL").await;

    // 合并结果（第二轮可能部分进程已退出，那些会出现在 failed 中）
    result.merge(force_result);

    result
}

/// 检查进程是否仍然存活（跨平台）——**僵尸感知**。
///
/// W-CRON-LIVENESS-PARITY (2026-07-20) 判活四实现同源契约（本处 = C）——改一处必核对四处：
///   A `acosmi-daemon-launcher::pid_alive`（src/lib.rs，僵尸感知基准）
///   B `acosmi-scheduler::lock::is_pid_alive`（src/lock.rs，曾僵尸盲楔死守护，本轮根治）
///   C `acosmi-exec::tree_kill::is_process_alive`（**本函数**）
///   D `acosmi-app-server::parent_death::parent_pid_is_alive`（src/parent_death.rs）
/// Windows「已退出但句柄被钉住的僵尸」OpenProcess 仍成功；必须再 GetExitCodeProcess
/// 判 STILL_ACTIVE(259) 才算活，否则同一僵尸 PID 会让不同层各执一词。C 在 kill 路径，
/// 危害较低（僵尸判活至多对已死 PID 发无效 kill），但判活语义须与四实现一致。
async fn is_process_alive(pid: u32) -> bool {
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            // kill(pid, 0) 不发送信号，仅检查进程是否存在
            // 返回 0 表示进程存在，-1 + ESRCH 表示不存在
            // SAFETY: kill(pid, 0) 是标准 POSIX 查询操作，无副作用
            // Unix 僵尸（未 reap）同样 kill==0；本仓 kill 路径不产生能命中此处的 Unix
            // 僵尸源（守护经 launcher double-fork+reap，见契约 B 的 Unix 注释），且僵尸
            // 判活在 kill 路径仅致无效 kill，无害，故不为其向本基础层引入 sysinfo 依赖。
            #[allow(unsafe_code)]
            unsafe {
                libc::kill(pid as libc::pid_t, 0) == 0
            }
        }
        #[cfg(windows)]
        {
            // SAFETY: OpenProcess + GetExitCodeProcess + CloseHandle 是标准 Win32 API，
            // 不会造成内存安全问题
            #[allow(unsafe_code)]
            unsafe {
                let handle = windows_sys::Win32::System::Threading::OpenProcess(
                    windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
                    0, // bInheritHandle = false
                    pid,
                );
                if handle.is_null() {
                    false
                } else {
                    // 句柄可打开 ≠ 活着：已退出但句柄被钉住的僵尸同样能 Open 成功。只有
                    // GetExitCodeProcess == STILL_ACTIVE(259) 才是真活；kill 路径保守，
                    // GetExitCodeProcess 失败即判死（与本分支 null=>false 的保守方向一致）。
                    let mut code: u32 = 0;
                    let got = windows_sys::Win32::System::Threading::GetExitCodeProcess(
                        handle, &mut code,
                    );
                    let _ = windows_sys::Win32::Foundation::CloseHandle(handle);
                    const STILL_ACTIVE: u32 = 259;
                    got != 0 && code == STILL_ACTIVE
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            true // 不支持的平台，假设进程仍存活
        }
    })
    .await
    .unwrap_or(true) // spawn_blocking 失败时保守假设进程存活
}

/// 便捷方法：强制终止进程树
///
/// 等价于 `kill_process_tree(pid, TreeKillOptions { force: true, .. })`
pub async fn kill_process_tree_force(pid: u32) -> TreeKillResult {
    kill_process_tree(
        pid,
        TreeKillOptions {
            force: true,
            ..Default::default()
        },
    )
    .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_kill_options_default() {
        let opts = TreeKillOptions::default();
        assert!(!opts.force);
        assert_eq!(opts.grace_period, Duration::from_secs(5));
        assert!(opts.signal.is_none());
    }

    #[test]
    fn test_tree_kill_result_new() {
        let result = TreeKillResult::new();
        assert!(result.killed_pids.is_empty());
        assert!(result.failed.is_empty());
        assert!(result.all_success());
    }

    #[test]
    fn test_tree_kill_result_merge() {
        let mut r1 = TreeKillResult {
            killed_pids: vec![1, 2],
            failed: vec![],
        };
        let r2 = TreeKillResult {
            killed_pids: vec![3],
            failed: vec![(4, "error".to_string())],
        };
        r1.merge(r2);
        assert_eq!(r1.killed_pids, vec![1, 2, 3]);
        assert_eq!(r1.failed.len(), 1);
        assert!(!r1.all_success());
    }

    #[test]
    fn test_tree_kill_result_merge_dedup() {
        // 验证 merge 去重：已在 killed_pids 中的 PID 不重复添加
        let mut r1 = TreeKillResult {
            killed_pids: vec![1, 2],
            failed: vec![],
        };
        let r2 = TreeKillResult {
            killed_pids: vec![2, 3], // PID 2 重复
            failed: vec![
                (1, "already terminated".to_string()), // PID 1 已在 killed_pids 中
                (5, "error".to_string()),
            ],
        };
        r1.merge(r2);
        assert_eq!(r1.killed_pids, vec![1, 2, 3]); // PID 2 不重复
        assert_eq!(r1.failed.len(), 1); // PID 1 的失败记录被排除
        assert_eq!(r1.failed[0].0, 5);
    }

    #[test]
    fn test_tree_kill_result_serialization() {
        let result = TreeKillResult {
            killed_pids: vec![100, 200],
            failed: vec![(300, "进程不存在".to_string())],
        };
        let json = serde_json::to_string(&result).expect("序列化失败");
        let deserialized: TreeKillResult = serde_json::from_str(&json).expect("反序列化失败");
        assert_eq!(deserialized.killed_pids, vec![100, 200]);
        assert_eq!(deserialized.failed.len(), 1);
    }

    #[test]
    fn test_convert_kill_tree_outputs_killed() {
        let outputs = vec![kill_tree::Output::Killed {
            process_id: 42,
            parent_process_id: 1,
            name: "test".to_string(),
        }];
        let result = convert_kill_tree_outputs(outputs);
        assert_eq!(result.killed_pids, vec![42]);
        assert!(result.failed.is_empty());
    }

    #[tokio::test]
    async fn test_kill_nonexistent_process_force() {
        // 使用一个极大的 PID，几乎不可能存在
        let result = kill_process_tree_force(kill_tree::get_available_max_process_id()).await;
        // 不应该 panic，结果中应有失败记录或已终止记录
        assert!(
            !result.killed_pids.is_empty() || !result.failed.is_empty(),
            "应该有某种结果"
        );
    }

    #[tokio::test]
    async fn test_kill_nonexistent_process_graceful() {
        let opts = TreeKillOptions {
            force: false,
            grace_period: Duration::from_millis(100), // 短等待
            signal: None,
        };
        let result = kill_process_tree(kill_tree::get_available_max_process_id(), opts).await;
        // 不应该 panic
        assert!(
            !result.killed_pids.is_empty() || !result.failed.is_empty(),
            "应该有某种结果"
        );
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_zombie_is_judged_dead() {
        // W-CRON-LIVENESS-PARITY 判活四实现同源契约（C）：已退出但句柄被钉住的僵尸
        // OpenProcess 仍成功；补 GetExitCodeProcess 后必须判死，与 A/B/D 对齐。
        use std::process::{Command, Stdio};

        let mut child = Command::new("cmd")
            .args(["/C", "exit", "0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn zombie child");
        let zombie_pid = child.id();
        // 等它退出；Child 句柄仍被持有 → 进程对象钉住成僵尸（PID 不被复用）。
        child.wait().expect("wait zombie child");

        assert!(
            !is_process_alive(zombie_pid).await,
            "已退出的僵尸进程必须判死（GetExitCodeProcess ≠ STILL_ACTIVE）"
        );

        drop(child); // 保持句柄到断言之后
    }
}

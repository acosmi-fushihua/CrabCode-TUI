//! `CrabCode` 统一命令执行器（命执模块）
//!
//! 统一本地外部命令执行器，接收 `ExecReq` 返回 `ExecResult`。
//! 支持 batch execution API 解决高频 git 场景。
//!
//! 子模块：
//! - executor: 核心执行逻辑
//! - batch: 批量执行 API
//! - family: 命令家族路由

pub mod batch;
pub mod executor;
pub mod family;

// 重导出核心类型和便捷函数
pub use batch::BatchExecutor;
pub use executor::CommandExecutor;
pub use family::{FamilyConfig, get_family_config};

use std::sync::Arc;

use acosmi_types::exec_types::{BatchExecReq, BatchExecResult, CommandFamily, ExecReq, ExecResult};

/// 全局单例执行器
///
/// 使用 `OnceLock` 保证线程安全的一次性初始化，
/// 所有便捷方法共享同一个 `CommandExecutor` 实例。
static GLOBAL_EXECUTOR: std::sync::OnceLock<Arc<CommandExecutor>> = std::sync::OnceLock::new();

/// 获取或初始化全局执行器
fn global_executor() -> &'static Arc<CommandExecutor> {
    GLOBAL_EXECUTOR.get_or_init(|| Arc::new(CommandExecutor::new()))
}

/// 便捷方法：执行单条命令
///
/// 使用全局共享的 `CommandExecutor` 实例执行命令。
/// 适合无需自定义执行器配置的简单场景。
pub async fn exec(req: ExecReq) -> ExecResult {
    global_executor().execute(req).await
}

/// 便捷方法：批量执行命令
///
/// 创建一个 `BatchExecutor` 并基于全局执行器批量执行。
/// 支持串行/并发模式和 `fail_fast` 策略。
pub async fn exec_batch(req: BatchExecReq) -> BatchExecResult {
    let batch = BatchExecutor::new(Arc::clone(global_executor()));
    batch.execute_batch(req).await
}

/// 便捷方法：简单执行（程序名 + 参数）
///
/// 自动构建 `ExecReq`，使用 `Other("simple")` 家族和 30 秒超时。
///
/// # 示例
/// ```no_run
/// # async fn example() {
/// let result = acosmi_executor::exec_simple("git", &["status"]).await;
/// println!("exit code: {}", result.code);
/// # }
/// ```
pub async fn exec_simple(program: &str, args: &[&str]) -> ExecResult {
    let req = ExecReq {
        family: CommandFamily::Other("simple".into()),
        program: program.to_string(),
        args: args.iter().map(std::string::ToString::to_string).collect(),
        cwd: None,
        env: std::collections::HashMap::new(),
        stdin: None,
        timeout_ms: 30_000,
        sandbox: false,
        abort_token: None,
    };
    exec(req).await
}

/// 便捷方法：执行 git 命令
///
/// 自动使用 `CommandFamily::Git` 家族配置（30s 默认超时、10MB 输出限制）。
///
/// # 参数
/// - `args`: git 子命令和参数，如 `&["status", "--short"]`
/// - `cwd`: 可选的工作目录
///
/// # 示例
/// ```no_run
/// # async fn example() {
/// let result = acosmi_executor::exec_git(&["log", "--oneline", "-5"], Some("/path/to/repo")).await;
/// println!("{}", result.stdout);
/// # }
/// ```
pub async fn exec_git(args: &[&str], cwd: Option<&str>) -> ExecResult {
    let req = ExecReq {
        family: CommandFamily::Git,
        program: "git".to_string(),
        args: args.iter().map(std::string::ToString::to_string).collect(),
        cwd: cwd.map(std::string::ToString::to_string),
        env: std::collections::HashMap::new(),
        stdin: None,
        timeout_ms: 0, // 使用 family 默认超时
        sandbox: false,
        abort_token: None,
    };
    exec(req).await
}

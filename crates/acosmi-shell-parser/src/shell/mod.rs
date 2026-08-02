//! Shell 发现与 provider 选择
//!
//! 等价于 TS `src/utils/shell/*` 模块群。
//!
//! # 模块结构
//!
//! - [`provider`]: `ShellProvider` trait — 构建命令、获取 spawn 参数和环境覆盖
//! - [`bash_provider`]: Bash 实现（snapshot sourcing, extglob, eval wrap, CWD tracking）
//! - [`powershell_provider`]: `PowerShell` 实现（-Command, exit code capture, sandbox encoding）
//! - [`detection`]: Shell 检测与 `PowerShell` 发现
//! - [`session_env`]: 会话环境管理（session vars, hook env scripts）
//! - [`snapshot`]: Shell 状态快照（full/incremental, JSON 持久化, diff）
//! - [`tmux`]: Tmux 集成（isolated socket, session/pane management）

pub mod bash_provider;
pub mod detection;
pub mod powershell_provider;
pub mod provider;
pub mod session_env;
pub mod snapshot;
pub mod tmux;

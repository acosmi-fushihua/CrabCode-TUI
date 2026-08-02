//! `PowerShell` 命令解析
//!
//! 等价于 TS `src/utils/powershell/*` 模块群。
//! 通过调用 pwsh 子进程获取 AST JSON。

pub mod cmdlets;
pub mod parser;
pub mod prefix;
pub mod types;

//! Bash 词法分析、递归下降解析与 AST 安全分析
//!
//! 等价于 TS 的 `src/utils/bash/*` 模块群。

pub mod analysis;
pub mod ast;
pub mod commands;
pub mod heredoc;
pub mod lexer;
pub mod parser;
pub mod quote;
pub mod registry;
pub mod types;

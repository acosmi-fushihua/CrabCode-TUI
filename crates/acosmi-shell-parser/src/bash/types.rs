//! Bash 解析器内部类型
//!
//! 对应 TS bashParser.ts 中的 Lexer、Token、ParseState 等类型。

use std::collections::HashSet;
use std::time::Instant;

// ─── Token Types ───

/// Token 类型（42 种）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TokenType {
    Word,
    Number,
    Op,
    Newline,
    Comment,
    Eof,
    // 引号
    DQuote,       // "
    SQuote,       // '
    DollarDQuote, // $"
    DollarSQuote, // $'
    Backtick,     // `
    // Dollar 展开
    DollarParen,   // $(
    DollarDParen,  // $((
    DollarBrace,   // ${
    DollarSimple,  // $VAR
    DollarSpecial, // $?, $$, $!, etc.
    DollarAt,      // $@
    DollarStar,    // $*
    DollarBracket, // $[
    // 重定向
    RedirectOut,     // >
    RedirectAppend,  // >>
    RedirectIn,      // <
    RedirectHereDoc, // <<
    RedirectHereStr, // <<<
    RedirectDupOut,  // >&
    RedirectDupIn,   // <&
    RedirectClobber, // >|
    RedirectAll,     // &>
    RedirectAllApp,  // &>>
    // 管道和逻辑操作符
    Pipe,        // |
    PipeAnd,     // |&
    And,         // &&
    Or,          // ||
    Semi,        // ;
    DoubleSemi,  // ;;
    SemiAnd,     // ;&
    SemiSemiAnd, // ;;&
    Amp,         // &
    // 分组
    LParen, // (
    RParen, // )
    LBrace, // {
    RBrace, // }
    // 条件
    DblLBracket, // [[
    DblRBracket, // ]]
    LBracket,    // [
    RBracket,    // ]
    // 进程替换
    ProcSubIn,  // <(
    ProcSubOut, // >(
}

/// Token 结构
#[derive(Debug, Clone)]
pub struct Token {
    pub token_type: TokenType,
    pub value: String,
    /// UTF-8 字节偏移：起始
    pub start: usize,
    /// UTF-8 字节偏移：结束
    pub end: usize,
}

impl Token {
    #[must_use]
    pub fn new(token_type: TokenType, value: impl Into<String>, start: usize, end: usize) -> Self {
        Self {
            token_type,
            value: value.into(),
            start,
            end,
        }
    }

    #[must_use]
    pub const fn eof(pos: usize) -> Self {
        Self {
            token_type: TokenType::Eof,
            value: String::new(),
            start: pos,
            end: pos,
        }
    }
}

// ─── Pending Heredoc ───

/// 待处理的 heredoc
#[derive(Debug, Clone)]
pub struct HeredocPending {
    /// 分隔符文本
    pub delim: String,
    /// 是否为 <<-（去除前导 tab）
    pub strip_tabs: bool,
    /// 分隔符是否被引号包围
    pub quoted: bool,
    /// 正文起始字节偏移
    pub body_start: usize,
    /// 正文结束字节偏移
    pub body_end: usize,
    /// 关闭分隔符起始
    pub end_start: usize,
    /// 关闭分隔符结束
    pub end_end: usize,
}

// ─── Lexer State ───

/// 词法分析器状态
#[derive(Debug, Clone)]
pub struct Lexer {
    /// 源文本
    pub src: String,
    /// 字符串长度（字符数）
    pub len: usize,
    /// 当前字符索引
    pub i: usize,
    /// 当前 UTF-8 字节偏移
    pub b: usize,
    /// 待处理 heredoc 列表
    pub heredocs: Vec<HeredocPending>,
    /// 延迟构建的字节→字符索引表
    pub byte_table: Option<Vec<u32>>,
}

impl Lexer {
    #[must_use]
    pub fn new(src: String) -> Self {
        let len = src.chars().count();
        Self {
            src,
            len,
            i: 0,
            b: 0,
            heredocs: Vec::new(),
            byte_table: None,
        }
    }

    /// 当前字符
    #[must_use]
    pub fn current_char(&self) -> Option<char> {
        self.src[self.b..].chars().next()
    }

    /// 向前看 n 个字符
    #[must_use]
    pub fn peek_char(&self, n: usize) -> Option<char> {
        self.src[self.b..].chars().nth(n)
    }

    /// 检查从当前位置是否以给定字符串开始
    #[must_use]
    pub fn starts_with(&self, s: &str) -> bool {
        self.src[self.b..].starts_with(s)
    }

    /// 是否到达末尾
    #[must_use]
    pub const fn at_end(&self) -> bool {
        self.i >= self.len
    }

    /// 前进一个字符，更新字符索引和字节偏移
    pub fn advance(&mut self) {
        if let Some(ch) = self.current_char() {
            self.i += 1;
            self.b += ch.len_utf8();
        }
    }

    /// 前进 n 个字符
    pub fn advance_n(&mut self, n: usize) {
        for _ in 0..n {
            self.advance();
        }
    }

    /// 获取从 `start_byte` 到当前 byte 的切片
    #[must_use]
    pub fn slice_from(&self, start_byte: usize) -> &str {
        &self.src[start_byte..self.b]
    }
}

// ─── Parse State ───

/// 解析器状态
#[derive(Debug)]
pub struct ParseState {
    /// 词法分析器
    pub lexer: Lexer,
    /// 源文本（与 lexer.src 相同的引用）
    pub src: String,
    /// UTF-8 字节总数
    pub src_bytes: usize,
    /// 是否为纯 ASCII（快速路径：字节 == 字符索引）
    pub is_ascii: bool,
    /// 已创建节点数（预算追踪）
    pub node_count: usize,
    /// 超时截止时间
    pub deadline: Instant,
    /// 是否已中止
    pub aborted: bool,
    /// 反引号嵌套深度
    pub in_backtick: usize,
    /// 停止标记（用于 `[` 回溯）
    pub stop_token: Option<String>,
}

impl ParseState {
    /// 创建新的解析状态
    #[must_use]
    pub fn new(src: String, timeout_ms: u64) -> Self {
        let src_bytes = src.len();
        let is_ascii = src.is_ascii();
        let deadline = Instant::now() + std::time::Duration::from_millis(timeout_ms);
        let lexer = Lexer::new(src.clone());
        Self {
            lexer,
            src,
            src_bytes,
            is_ascii,
            node_count: 0,
            deadline,
            aborted: false,
            in_backtick: 0,
            stop_token: None,
        }
    }

    /// 检查是否超时或节点预算耗尽
    pub fn check_budget(&mut self) -> bool {
        // 每 128 个节点检查一次超时（与 TS 的 0x7f 掩码一致）
        if self.node_count & 0x7f == 0 && Instant::now() >= self.deadline {
            self.aborted = true;
            return true;
        }
        if self.node_count >= MAX_NODES {
            self.aborted = true;
            return true;
        }
        false
    }

    /// 创建新节点并增加计数
    pub fn make_node(
        &mut self,
        node_type: &str,
        text: &str,
        start: usize,
        end: usize,
    ) -> crate::TsNode {
        self.node_count += 1;
        crate::TsNode::new(node_type, text, start, end)
    }
}

// ─── Constants ───

/// 解析超时（毫秒）
pub const PARSE_TIMEOUT_MS: u64 = 50;

/// 最大节点数预算
pub const MAX_NODES: usize = 50_000;

/// 最大命令长度
pub const MAX_COMMAND_LENGTH: usize = 10_000;

/// 特殊变量名
#[must_use]
pub fn special_vars() -> HashSet<char> {
    ['?', '$', '@', '*', '#', '-', '!', '_']
        .into_iter()
        .collect()
}

/// 声明关键字
#[must_use]
pub fn decl_keywords() -> HashSet<&'static str> {
    ["export", "declare", "typeset", "readonly", "local"]
        .into_iter()
        .collect()
}

/// Shell 关键字
#[must_use]
pub fn shell_keywords() -> HashSet<&'static str> {
    [
        "if", "then", "elif", "else", "fi", "while", "until", "for", "in", "do", "done", "case",
        "esac", "function", "select",
    ]
    .into_iter()
    .collect()
}

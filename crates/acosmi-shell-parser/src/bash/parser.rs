//! Bash 递归下降解析器
//!
//! 等价于 TS bashParser.ts 的 `parseSource` 函数。
//! 纯算法实现，50ms 超时 + 50,000 节点预算。

use super::lexer;
use super::types::{MAX_COMMAND_LENGTH, PARSE_TIMEOUT_MS, ParseState, Token, TokenType};
use crate::{ParseResult, TsNode};

/// 解析 Bash 源文本为 AST
///
/// - `timeout_ms`: 超时毫秒数，0 表示使用默认值 (50ms)
/// - 返回 `ParseResult::Success(TsNode)` 或 `Aborted`/`Failed`
#[must_use]
pub fn parse_source(source: &str, timeout_ms: u64) -> ParseResult {
    let timeout = if timeout_ms == 0 {
        PARSE_TIMEOUT_MS
    } else {
        timeout_ms
    };

    if source.is_empty() {
        return ParseResult::Success(TsNode::new("program", "", 0, 0));
    }

    if source.len() > MAX_COMMAND_LENGTH {
        return ParseResult::Aborted;
    }

    let mut state = ParseState::new(source.to_string(), timeout);

    match parse_program(&mut state) {
        Some(node) if !state.aborted => ParseResult::Success(node),
        _ => {
            if state.aborted {
                ParseResult::Aborted
            } else {
                ParseResult::Failed
            }
        }
    }
}

/// 解析顶层 program 节点
fn parse_program(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let mut children = Vec::new();

    loop {
        skip_newlines_and_comments(state);

        if state.lexer.at_end() || state.aborted {
            break;
        }

        if state.check_budget() {
            return None;
        }

        if let Some(stmt) = parse_compound_list(state) {
            children.push(stmt);
        } else if state.aborted {
            return None;
        } else {
            // 尝试跳过一个 token 避免无限循环
            let _ = lexer::next_token(&mut state.lexer);
        }
    }

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("program", &text, start, end);
    node.children = children;
    Some(node)
}

/// 解析复合语句列表（分号/换行分隔）
fn parse_compound_list(state: &mut ParseState) -> Option<TsNode> {
    let first = parse_and_or(state)?;

    let start = first.start_index;
    let mut children = vec![first];

    loop {
        skip_newlines_and_comments(state);

        let tok = peek_token(state);
        match tok.token_type {
            TokenType::Semi | TokenType::Amp => {
                let _ = lexer::next_token(&mut state.lexer);
                skip_newlines_and_comments(state);

                if state.lexer.at_end() || state.aborted {
                    break;
                }

                // 检查是否还有后续命令
                let next_tok = peek_token(state);
                if is_statement_terminator(&next_tok) {
                    break;
                }

                if let Some(next) = parse_and_or(state) {
                    children.push(next);
                } else {
                    break;
                }
            }
            _ => break,
        }

        if state.aborted {
            return None;
        }
    }

    if children.len() == 1 {
        return children.into_iter().next();
    }

    let end = children.last().map_or(start, |n| n.end_index);
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("compound_statement", &text, start, end);
    node.children = children;
    Some(node)
}

/// 解析 && || 链（左结合）
fn parse_and_or(state: &mut ParseState) -> Option<TsNode> {
    let mut left = parse_pipeline(state)?;

    loop {
        skip_newlines_and_comments(state);

        let tok = peek_token(state);
        let op_type = match tok.token_type {
            TokenType::And => "&&",
            TokenType::Or => "||",
            _ => break,
        };

        let _ = lexer::next_token(&mut state.lexer); // consume operator
        skip_newlines_and_comments(state);

        if state.check_budget() {
            return None;
        }

        let right = parse_pipeline(state)?;
        let start = left.start_index;
        let end = right.end_index;
        let text = state.src[start..end].to_string();

        let op_node = state.make_node(op_type, op_type, left.end_index, right.start_index);
        let mut list_node = state.make_node("list", &text, start, end);
        list_node.children = vec![left, op_node, right];
        left = list_node;
    }

    Some(left)
}

/// 解析管道（| |&）
fn parse_pipeline(state: &mut ParseState) -> Option<TsNode> {
    // 检查 ! 否定
    let negated = {
        let tok = peek_token(state);
        if tok.token_type == TokenType::Word && tok.value == "!" {
            let _ = lexer::next_token(&mut state.lexer);
            true
        } else {
            false
        }
    };

    let mut left = parse_command(state)?;

    // 如果有否定但只有一个命令，包装在 negated_command 中
    if negated && peek_token(state).token_type != TokenType::Pipe {
        let start = left.start_index.saturating_sub(2); // '! ' 的大概位置
        let end = left.end_index;
        let text = state.src[start..end].to_string();
        let mut neg = state.make_node("negated_command", &text, start, end);
        neg.children = vec![left];
        return Some(neg);
    }

    loop {
        let tok = peek_token(state);
        match tok.token_type {
            TokenType::Pipe | TokenType::PipeAnd => {
                let _ = lexer::next_token(&mut state.lexer);
                skip_newlines_and_comments(state);

                if state.check_budget() {
                    return None;
                }

                let right = parse_command(state)?;
                let start = left.start_index;
                let end = right.end_index;
                let text = state.src[start..end].to_string();
                let mut pipe_node = state.make_node("pipeline", &text, start, end);
                pipe_node.children = vec![left, right];
                left = pipe_node;
            }
            _ => break,
        }
    }

    Some(left)
}

/// 解析单个命令（简单命令或复合命令）
fn parse_command(state: &mut ParseState) -> Option<TsNode> {
    if state.check_budget() {
        return None;
    }

    let tok = peek_token(state);

    match tok.token_type {
        // 复合命令
        TokenType::LParen => parse_subshell(state),
        TokenType::LBrace => parse_command_group(state),
        TokenType::DblLBracket => parse_test_command(state),

        // 关键字命令
        TokenType::Word => match tok.value.as_str() {
            "if" => parse_if(state),
            "while" | "until" => parse_while_until(state),
            "for" => parse_for(state),
            "case" => parse_case(state),
            "function" => parse_function(state),
            "select" => parse_select(state),
            "[" => parse_test_bracket(state),
            "time" => parse_time(state),
            "coproc" => parse_coproc(state),
            _ => parse_simple_command(state),
        },

        // $, 引号等开头也是简单命令
        _ if is_command_start(&tok) => parse_simple_command(state),

        _ => None,
    }
}

/// 解析简单命令
fn parse_simple_command(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let mut children = Vec::new();
    let mut has_command_name = false;

    loop {
        let tok = peek_token(state);

        // 检查是否为赋值（VAR=value）
        if !has_command_name
            && tok.token_type == TokenType::Word
            && is_assignment(&tok.value)
            && let Some(assign) = parse_assignment(state)
        {
            children.push(assign);
            continue;
        }

        // 检查重定向
        if is_redirect_token(&tok)
            && let Some(redir) = parse_redirect(state)
        {
            children.push(redir);
            continue;
        }

        // 命令名或参数
        if is_word_token(&tok) {
            let word = parse_word(state)?;
            if has_command_name {
                children.push(word);
            } else {
                let start = word.start_index;
                let end = word.end_index;
                let text = word.text.clone();
                let mut name_node = state.make_node("command_name", &text, start, end);
                name_node.children = vec![word];
                children.push(name_node);
                has_command_name = true;
            }
            continue;
        }

        break;
    }

    if children.is_empty() {
        return None;
    }

    let end = state.lexer.b;
    let text = state.src[start..end].trim_end().to_string();
    let actual_end = start + text.len();
    let mut node = state.make_node("command", &text, start, actual_end);
    node.children = children;
    Some(node)
}

/// 解析 word（处理引号、展开、连接）
fn parse_word(state: &mut ParseState) -> Option<TsNode> {
    let _start = state.lexer.b;
    let tok = peek_token(state);

    match tok.token_type {
        TokenType::Word | TokenType::Number => {
            let tok = lexer::next_token(&mut state.lexer);
            Some(state.make_node("word", &tok.value, tok.start, tok.end))
        }
        TokenType::SQuote => parse_single_quoted_string(state),
        TokenType::DQuote | TokenType::DollarDQuote => parse_double_quoted_string(state),
        TokenType::DollarSQuote => parse_ansi_c_string(state),
        TokenType::DollarParen => parse_command_substitution(state),
        TokenType::DollarDParen => parse_arithmetic_expansion(state),
        TokenType::DollarBrace => parse_parameter_expansion(state),
        TokenType::DollarSimple
        | TokenType::DollarSpecial
        | TokenType::DollarAt
        | TokenType::DollarStar => parse_simple_expansion(state),
        TokenType::Backtick => parse_backtick_substitution(state),
        TokenType::ProcSubIn | TokenType::ProcSubOut => parse_process_substitution(state),
        _ => None,
    }
}

/// 解析单引号字符串
fn parse_single_quoted_string(state: &mut ParseState) -> Option<TsNode> {
    let tok = lexer::next_token(&mut state.lexer); // consume ' (skips whitespace)
    let start = tok.start; // position of actual '
    let _content = lexer::read_single_quoted(&mut state.lexer);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    Some(state.make_node("raw_string", &text, start, end))
}

/// 解析双引号字符串
fn parse_double_quoted_string(state: &mut ParseState) -> Option<TsNode> {
    let tok = lexer::next_token(&mut state.lexer); // consume " or $" (skips whitespace)
    let start = tok.start;
    // 收集内容，处理嵌入展开
    let mut children = Vec::new();

    loop {
        if state.lexer.at_end() || state.aborted {
            break;
        }
        match state.lexer.current_char() {
            Some('"') => {
                state.lexer.advance();
                break;
            }
            Some('$') => {
                // 可能是嵌入展开
                let inner_start = state.lexer.b;
                let saved_i = state.lexer.i;
                let saved_b = state.lexer.b;
                let tok = lexer::next_token(&mut state.lexer);
                match tok.token_type {
                    TokenType::DollarParen
                    | TokenType::DollarBrace
                    | TokenType::DollarDParen
                    | TokenType::DollarSimple
                    | TokenType::DollarSpecial
                    | TokenType::DollarAt
                    | TokenType::DollarStar => {
                        // 回退并使用 parse_word 处理
                        state.lexer.i = saved_i;
                        state.lexer.b = saved_b;
                        if let Some(expansion) = parse_word(state) {
                            children.push(expansion);
                        } else {
                            // 无法解析，当作字面量
                            state.lexer.advance();
                        }
                    }
                    _ => {
                        // 裸 $ 当作字符串内容
                        let content_text = state.src[inner_start..state.lexer.b].to_string();
                        let end = state.lexer.b;
                        children.push(state.make_node(
                            "string_content",
                            &content_text,
                            inner_start,
                            end,
                        ));
                    }
                }
            }
            Some('`') => {
                if let Some(sub) = parse_backtick_substitution(state) {
                    children.push(sub);
                }
            }
            Some('\\') => {
                let esc_start = state.lexer.b;
                state.lexer.advance(); // skip '\'
                if !state.lexer.at_end() {
                    state.lexer.advance(); // skip escaped char
                }
                let esc_text = state.src[esc_start..state.lexer.b].to_string();
                let end = state.lexer.b;
                children.push(state.make_node("string_content", &esc_text, esc_start, end));
            }
            _ => {
                // 普通字符串内容
                let content_start = state.lexer.b;
                while !state.lexer.at_end() {
                    match state.lexer.current_char() {
                        Some('"' | '$' | '`' | '\\') => break,
                        _ => state.lexer.advance(),
                    }
                }
                let content_text = state.src[content_start..state.lexer.b].to_string();
                let end = state.lexer.b;
                if !content_text.is_empty() {
                    children.push(state.make_node(
                        "string_content",
                        &content_text,
                        content_start,
                        end,
                    ));
                }
            }
        }
    }

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("string", &text, start, end);
    node.children = children;
    Some(node)
}

/// 解析 $'...' ANSI-C 字符串
fn parse_ansi_c_string(state: &mut ParseState) -> Option<TsNode> {
    let tok = lexer::next_token(&mut state.lexer); // consume $'
    let start = tok.start;
    let _content = lexer::read_ansi_c_quoted(&mut state.lexer);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    Some(state.make_node("ansi_c_string", &text, start, end))
}

/// 解析命令替换 $(...)
fn parse_command_substitution(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume $(
    // 递归解析内部命令
    let inner = parse_compound_list(state);
    // expect )
    expect_token(state, TokenType::RParen);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("command_substitution", &text, start, end);
    if let Some(inner_node) = inner {
        node.children.push(inner_node);
    }
    Some(node)
}

/// 解析算术展开 $((...))
fn parse_arithmetic_expansion(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume $((
    // 读到 )) 为止
    let mut depth = 1;
    while !state.lexer.at_end() && depth > 0 {
        match state.lexer.current_char() {
            Some('(') => {
                if state.lexer.peek_char(1) == Some('(') {
                    depth += 1;
                    state.lexer.advance();
                }
                state.lexer.advance();
            }
            Some(')') => {
                if state.lexer.peek_char(1) == Some(')') {
                    depth -= 1;
                    state.lexer.advance();
                    state.lexer.advance();
                    if depth == 0 {
                        break;
                    }
                } else {
                    state.lexer.advance();
                }
            }
            _ => state.lexer.advance(),
        }
    }
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    Some(state.make_node("arithmetic_expansion", &text, start, end))
}

/// 解析参数展开 ${...}
fn parse_parameter_expansion(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume ${
    let mut depth = 1;
    while !state.lexer.at_end() && depth > 0 {
        match state.lexer.current_char() {
            Some('{') => {
                depth += 1;
                state.lexer.advance();
            }
            Some('}') => {
                depth -= 1;
                state.lexer.advance();
            }
            Some('\\') => {
                state.lexer.advance();
                state.lexer.advance();
            }
            Some('\'') => {
                state.lexer.advance();
                while !state.lexer.at_end() && state.lexer.current_char() != Some('\'') {
                    state.lexer.advance();
                }
                if !state.lexer.at_end() {
                    state.lexer.advance();
                }
            }
            Some('"') => {
                state.lexer.advance();
                while !state.lexer.at_end() && state.lexer.current_char() != Some('"') {
                    if state.lexer.current_char() == Some('\\') {
                        state.lexer.advance();
                    }
                    state.lexer.advance();
                }
                if !state.lexer.at_end() {
                    state.lexer.advance();
                }
            }
            _ => state.lexer.advance(),
        }
    }
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    Some(state.make_node("expansion", &text, start, end))
}

/// 解析简单展开 $VAR, $?, $$ 等
fn parse_simple_expansion(state: &mut ParseState) -> Option<TsNode> {
    let tok = lexer::next_token(&mut state.lexer);
    Some(state.make_node("simple_expansion", &tok.value, tok.start, tok.end))
}

/// 解析反引号命令替换 `...`
fn parse_backtick_substitution(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume `
    state.in_backtick += 1;

    // 读到匹配的 ` 为止
    while !state.lexer.at_end() {
        match state.lexer.current_char() {
            Some('`') => {
                state.lexer.advance();
                break;
            }
            Some('\\') => {
                state.lexer.advance();
                if !state.lexer.at_end() {
                    state.lexer.advance();
                }
            }
            _ => state.lexer.advance(),
        }
    }

    state.in_backtick = state.in_backtick.saturating_sub(1);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    Some(state.make_node("command_substitution", &text, start, end))
}

/// 解析进程替换 <(...) 或 >(...)
fn parse_process_substitution(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume <( or >(
    let inner = parse_compound_list(state);
    expect_token(state, TokenType::RParen);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("process_substitution", &text, start, end);
    if let Some(inner_node) = inner {
        node.children.push(inner_node);
    }
    Some(node)
}

/// 解析赋值 VAR=value
fn parse_assignment(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let tok = lexer::next_token(&mut state.lexer);
    let text = tok.value.clone();
    // 值部分可能包含引号等
    let eq_pos = text.find('=')?;
    let var_name = &text[..eq_pos];
    let mut node = state.make_node("variable_assignment", &text, tok.start, tok.end);
    let name_node = state.make_node(
        "variable_name",
        var_name,
        tok.start,
        tok.start + var_name.len(),
    );
    node.children.push(name_node);

    // 如果 = 后面还有连续的 word（引号等），需要继续解析
    if eq_pos + 1 < text.len() {
        let value_start = tok.start + eq_pos + 1;
        let value_text = &text[eq_pos + 1..];
        let value_node = state.make_node("word", value_text, value_start, tok.end);
        node.children.push(value_node);
    } else {
        // = 后面可能跟引号开始的 word
        let next = peek_token(state);
        if is_word_token(&next)
            && let Some(value) = parse_word(state)
        {
            let end = value.end_index;
            node.children.push(value);
            node.end_index = end;
            node.text = state.src[start..end].to_string();
        }
    }

    Some(node)
}

/// 解析重定向
fn parse_redirect(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let mut children = Vec::new();

    // 可能有 fd 数字前缀
    let tok = peek_token(state);
    if tok.token_type == TokenType::Number {
        let fd_tok = lexer::next_token(&mut state.lexer);
        children.push(state.make_node("file_descriptor", &fd_tok.value, fd_tok.start, fd_tok.end));
    }

    // 重定向操作符
    let op_tok = lexer::next_token(&mut state.lexer);

    // heredoc 特殊处理
    if op_tok.token_type == TokenType::RedirectHereDoc {
        return parse_heredoc_redirect(state, start, &op_tok, children);
    }

    // 目标 word
    let target = parse_word(state);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();

    let redir_type = match op_tok.token_type {
        TokenType::RedirectHereStr => "herestring_redirect",
        _ => "file_redirect",
    };

    let mut node = state.make_node(redir_type, &text, start, end);
    node.children.append(&mut children);
    if let Some(t) = target {
        node.children.push(t);
    }
    Some(node)
}

/// 解析 heredoc 重定向
fn parse_heredoc_redirect(
    state: &mut ParseState,
    start: usize,
    _op_tok: &Token,
    mut children: Vec<TsNode>,
) -> Option<TsNode> {
    // 读取分隔符
    lexer::skip_whitespace(&mut state.lexer);
    let delim_start = state.lexer.b;
    let mut delim = String::new();
    let mut quoted = false;
    let strip_tabs = _op_tok.value.contains('-');

    match state.lexer.current_char() {
        Some('\'') => {
            quoted = true;
            state.lexer.advance();
            while !state.lexer.at_end() && state.lexer.current_char() != Some('\'') {
                if let Some(c) = state.lexer.current_char() {
                    delim.push(c);
                }
                state.lexer.advance();
            }
            if !state.lexer.at_end() {
                state.lexer.advance();
            }
        }
        Some('"') => {
            quoted = true;
            state.lexer.advance();
            while !state.lexer.at_end() && state.lexer.current_char() != Some('"') {
                if let Some(c) = state.lexer.current_char() {
                    delim.push(c);
                }
                state.lexer.advance();
            }
            if !state.lexer.at_end() {
                state.lexer.advance();
            }
        }
        _ => {
            // 无引号分隔符
            while !state.lexer.at_end() {
                match state.lexer.current_char() {
                    Some(c)
                        if c.is_whitespace() || c == ';' || c == '&' || c == '|' || c == ')' =>
                    {
                        break;
                    }
                    Some('\\') => {
                        quoted = true; // 转义也算 quoted
                        state.lexer.advance();
                        if let Some(c) = state.lexer.current_char() {
                            delim.push(c);
                            state.lexer.advance();
                        }
                    }
                    Some(c) => {
                        delim.push(c);
                        state.lexer.advance();
                    }
                    None => break,
                }
            }
        }
    }

    // 注册 pending heredoc
    lexer::register_heredoc(&mut state.lexer, delim.clone(), strip_tabs, quoted);

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("heredoc_redirect", &text, start, end);
    let delim_node = state.make_node("heredoc_start", &delim, delim_start, end);
    children.push(delim_node);
    node.children.append(&mut children);
    Some(node)
}

// ─── Compound Commands ───

/// 解析子 shell (...)
fn parse_subshell(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume (
    skip_newlines_and_comments(state);
    let body = parse_compound_list(state);
    skip_newlines_and_comments(state);
    expect_token(state, TokenType::RParen);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("subshell", &text, start, end);
    if let Some(b) = body {
        node.children.push(b);
    }
    maybe_parse_redirects(state, &mut node);
    Some(node)
}

/// 解析命令组 { ... }
fn parse_command_group(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume {
    skip_newlines_and_comments(state);
    let body = parse_compound_list(state);
    skip_newlines_and_comments(state);
    expect_token(state, TokenType::RBrace);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("compound_statement", &text, start, end);
    if let Some(b) = body {
        node.children.push(b);
    }
    maybe_parse_redirects(state, &mut node);
    Some(node)
}

/// 解析 if 语句
fn parse_if(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume 'if'
    skip_newlines_and_comments(state);

    let mut children = Vec::new();

    // condition
    if let Some(cond) = parse_compound_list(state) {
        children.push(cond);
    }

    // 'then'
    skip_newlines_and_comments(state);
    expect_word(state, "then");
    skip_newlines_and_comments(state);

    if let Some(body) = parse_compound_list(state) {
        children.push(body);
    }

    // elif / else
    loop {
        skip_newlines_and_comments(state);
        let tok = peek_token(state);
        match tok.value.as_str() {
            "elif" => {
                let _ = lexer::next_token(&mut state.lexer);
                skip_newlines_and_comments(state);
                if let Some(cond) = parse_compound_list(state) {
                    children.push(cond);
                }
                skip_newlines_and_comments(state);
                expect_word(state, "then");
                skip_newlines_and_comments(state);
                if let Some(body) = parse_compound_list(state) {
                    children.push(body);
                }
            }
            "else" => {
                let _ = lexer::next_token(&mut state.lexer);
                skip_newlines_and_comments(state);
                if let Some(body) = parse_compound_list(state) {
                    children.push(body);
                }
                break;
            }
            "fi" => break,
            _ => break,
        }
    }

    skip_newlines_and_comments(state);
    expect_word(state, "fi");

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("if_statement", &text, start, end);
    node.children = children;
    maybe_parse_redirects(state, &mut node);
    Some(node)
}

/// 解析 while/until 语句
fn parse_while_until(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let tok = lexer::next_token(&mut state.lexer);
    let node_type = if tok.value == "while" {
        "while_statement"
    } else {
        "until_statement"
    };
    skip_newlines_and_comments(state);

    let mut children = Vec::new();
    if let Some(cond) = parse_compound_list(state) {
        children.push(cond);
    }

    skip_newlines_and_comments(state);
    expect_word(state, "do");
    skip_newlines_and_comments(state);

    if let Some(body) = parse_compound_list(state) {
        children.push(body);
    }

    skip_newlines_and_comments(state);
    expect_word(state, "done");

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node(node_type, &text, start, end);
    node.children = children;
    maybe_parse_redirects(state, &mut node);
    Some(node)
}

/// 解析 for 语句
fn parse_for(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume 'for'
    skip_newlines_and_comments(state);

    let mut children = Vec::new();

    // 变量名
    let var_tok = lexer::next_token(&mut state.lexer);
    children.push(state.make_node("variable_name", &var_tok.value, var_tok.start, var_tok.end));

    skip_newlines_and_comments(state);

    // 'in' 词列表（可选）
    let next = peek_token(state);
    if next.token_type == TokenType::Word && next.value == "in" {
        let _ = lexer::next_token(&mut state.lexer); // consume 'in'
        // 读取词列表直到 ; 或 newline 或 do
        loop {
            let tok = peek_token(state);
            if tok.token_type == TokenType::Semi || tok.token_type == TokenType::Newline {
                let _ = lexer::next_token(&mut state.lexer);
                break;
            }
            if tok.token_type == TokenType::Word && tok.value == "do" {
                break;
            }
            if tok.token_type == TokenType::Eof {
                break;
            }
            if let Some(w) = parse_word(state) {
                children.push(w);
            } else {
                break;
            }
        }
    }

    skip_newlines_and_comments(state);
    expect_word(state, "do");
    skip_newlines_and_comments(state);

    if let Some(body) = parse_compound_list(state) {
        children.push(body);
    }

    skip_newlines_and_comments(state);
    expect_word(state, "done");

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("for_statement", &text, start, end);
    node.children = children;
    maybe_parse_redirects(state, &mut node);
    Some(node)
}

/// 解析 case 语句
fn parse_case(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume 'case'
    skip_newlines_and_comments(state);

    let mut children = Vec::new();

    // case word
    if let Some(w) = parse_word(state) {
        children.push(w);
    }

    skip_newlines_and_comments(state);
    expect_word(state, "in");
    skip_newlines_and_comments(state);

    // case items
    loop {
        let tok = peek_token(state);
        if tok.token_type == TokenType::Word && tok.value == "esac" {
            break;
        }
        if tok.token_type == TokenType::Eof {
            break;
        }

        // 解析 pattern
        let item_start = state.lexer.b;

        // optional (
        if peek_token(state).token_type == TokenType::LParen {
            let _ = lexer::next_token(&mut state.lexer);
        }

        // 读取 pattern 直到 )
        let mut patterns = Vec::new();
        loop {
            if let Some(w) = parse_word(state) {
                patterns.push(w);
            }
            let tok = peek_token(state);
            if tok.token_type == TokenType::Pipe {
                let _ = lexer::next_token(&mut state.lexer);
            } else {
                break;
            }
        }

        expect_token(state, TokenType::RParen);
        skip_newlines_and_comments(state);

        // body
        let body = parse_compound_list(state);

        skip_newlines_and_comments(state);

        // ;; or ;& or ;;& or esac
        let tok = peek_token(state);
        if tok.token_type == TokenType::DoubleSemi
            || tok.token_type == TokenType::SemiAnd
            || tok.token_type == TokenType::SemiSemiAnd
        {
            let _ = lexer::next_token(&mut state.lexer);
        }

        skip_newlines_and_comments(state);

        let item_end = state.lexer.b;
        let item_text = state.src[item_start..item_end].to_string();
        let mut item = state.make_node("case_item", &item_text, item_start, item_end);
        item.children.extend(patterns);
        if let Some(b) = body {
            item.children.push(b);
        }
        children.push(item);
    }

    expect_word(state, "esac");

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("case_statement", &text, start, end);
    node.children = children;
    maybe_parse_redirects(state, &mut node);
    Some(node)
}

/// 解析函数定义
fn parse_function(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume 'function'
    skip_newlines_and_comments(state);

    let name_tok = lexer::next_token(&mut state.lexer);
    let name_node = state.make_node("word", &name_tok.value, name_tok.start, name_tok.end);

    skip_newlines_and_comments(state);

    // optional ()
    if peek_token(state).token_type == TokenType::LParen {
        let _ = lexer::next_token(&mut state.lexer);
        expect_token(state, TokenType::RParen);
        skip_newlines_and_comments(state);
    }

    let body = parse_command(state);

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("function_definition", &text, start, end);
    node.children.push(name_node);
    if let Some(b) = body {
        node.children.push(b);
    }
    Some(node)
}

/// 解析 select 语句
fn parse_select(state: &mut ParseState) -> Option<TsNode> {
    // select 与 for 结构相同
    parse_for(state).map(|mut n| {
        n.node_type = "select_statement".to_string();
        n
    })
}

/// 解析 [[ ... ]] 测试命令
fn parse_test_command(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume [[
    let mut children = Vec::new();

    loop {
        let tok = peek_token(state);
        if tok.token_type == TokenType::DblRBracket {
            let _ = lexer::next_token(&mut state.lexer);
            break;
        }
        if tok.token_type == TokenType::Eof {
            break;
        }
        if let Some(w) = parse_word(state) {
            children.push(w);
        } else {
            let _ = lexer::next_token(&mut state.lexer);
        }
    }

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("test_command", &text, start, end);
    node.children = children;
    maybe_parse_redirects(state, &mut node);
    Some(node)
}

/// 解析 [ ... ] 测试
fn parse_test_bracket(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume [
    let mut children = Vec::new();

    loop {
        let tok = peek_token(state);
        if tok.token_type == TokenType::RBracket {
            let _ = lexer::next_token(&mut state.lexer);
            break;
        }
        if tok.token_type == TokenType::Eof {
            break;
        }
        if let Some(w) = parse_word(state) {
            children.push(w);
        } else {
            let _ = lexer::next_token(&mut state.lexer);
        }
    }

    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("test_command", &text, start, end);
    node.children = children;
    maybe_parse_redirects(state, &mut node);
    Some(node)
}

/// 解析 time 前缀
fn parse_time(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume 'time'

    // 可选 -p 标志
    let next = peek_token(state);
    if next.token_type == TokenType::Word && next.value == "-p" {
        let _ = lexer::next_token(&mut state.lexer);
    }

    let cmd = parse_pipeline(state);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("command", &text, start, end);
    let name = state.make_node("command_name", "time", start, start + 4);
    node.children.push(name);
    if let Some(c) = cmd {
        node.children.push(c);
    }
    Some(node)
}

/// 解析 coproc
fn parse_coproc(state: &mut ParseState) -> Option<TsNode> {
    let start = state.lexer.b;
    let _ = lexer::next_token(&mut state.lexer); // consume 'coproc'

    let cmd = parse_command(state);
    let end = state.lexer.b;
    let text = state.src[start..end].to_string();
    let mut node = state.make_node("command", &text, start, end);
    let name = state.make_node("command_name", "coproc", start, start + 6);
    node.children.push(name);
    if let Some(c) = cmd {
        node.children.push(c);
    }
    Some(node)
}

// ─── Helper Functions ───

/// 预览下一个 token（不消费）
fn peek_token(state: &mut ParseState) -> Token {
    let saved_i = state.lexer.i;
    let saved_b = state.lexer.b;
    let saved_heredocs = state.lexer.heredocs.clone();
    let tok = lexer::next_token(&mut state.lexer);
    state.lexer.i = saved_i;
    state.lexer.b = saved_b;
    state.lexer.heredocs = saved_heredocs;
    tok
}

/// 跳过换行和注释
fn skip_newlines_and_comments(state: &mut ParseState) {
    loop {
        let tok = peek_token(state);
        match tok.token_type {
            TokenType::Newline | TokenType::Comment => {
                let _ = lexer::next_token(&mut state.lexer);
            }
            _ => break,
        }
    }
}

/// 期望特定 token 类型
fn expect_token(state: &mut ParseState, expected: TokenType) -> Option<Token> {
    let tok = lexer::next_token(&mut state.lexer);
    if tok.token_type == expected {
        Some(tok)
    } else {
        None // 不严格失败，允许错误恢复
    }
}

/// 期望特定关键字
fn expect_word(state: &mut ParseState, word: &str) -> Option<Token> {
    let tok = peek_token(state);
    if tok.token_type == TokenType::Word && tok.value == word {
        Some(lexer::next_token(&mut state.lexer))
    } else {
        None
    }
}

/// 检查是否为语句终止符
fn is_statement_terminator(tok: &Token) -> bool {
    matches!(
        tok.token_type,
        TokenType::Eof
            | TokenType::RParen
            | TokenType::RBrace
            | TokenType::DoubleSemi
            | TokenType::SemiAnd
            | TokenType::SemiSemiAnd
    ) || (tok.token_type == TokenType::Word
        && matches!(
            tok.value.as_str(),
            "fi" | "done" | "esac" | "then" | "elif" | "else" | "do" | "in"
        ))
}

/// 检查是否为重定向 token
fn is_redirect_token(tok: &Token) -> bool {
    matches!(
        tok.token_type,
        TokenType::RedirectOut
            | TokenType::RedirectAppend
            | TokenType::RedirectIn
            | TokenType::RedirectHereDoc
            | TokenType::RedirectHereStr
            | TokenType::RedirectDupOut
            | TokenType::RedirectDupIn
            | TokenType::RedirectClobber
            | TokenType::RedirectAll
            | TokenType::RedirectAllApp
    ) || (tok.token_type == TokenType::Number && {
        // 数字后跟重定向
        false // 简化处理
    })
}

/// 检查是否为 word 类型 token
const fn is_word_token(tok: &Token) -> bool {
    matches!(
        tok.token_type,
        TokenType::Word
            | TokenType::Number
            | TokenType::SQuote
            | TokenType::DQuote
            | TokenType::DollarDQuote
            | TokenType::DollarSQuote
            | TokenType::DollarParen
            | TokenType::DollarDParen
            | TokenType::DollarBrace
            | TokenType::DollarSimple
            | TokenType::DollarSpecial
            | TokenType::DollarAt
            | TokenType::DollarStar
            | TokenType::DollarBracket
            | TokenType::Backtick
            | TokenType::ProcSubIn
            | TokenType::ProcSubOut
    )
}

/// 检查是否为命令起始 token
const fn is_command_start(tok: &Token) -> bool {
    is_word_token(tok)
}

/// 检查字符串是否为赋值格式（VAR=...）
fn is_assignment(s: &str) -> bool {
    if let Some(eq_pos) = s.find('=') {
        if eq_pos == 0 {
            return false;
        }
        let name = &s[..eq_pos];
        // 变量名必须以字母或下划线开头
        let mut chars = name.chars();
        match chars.next() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return false,
        }
        chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
    } else {
        false
    }
}

/// 尝试解析命令后的重定向
fn maybe_parse_redirects(state: &mut ParseState, node: &mut TsNode) {
    loop {
        let tok = peek_token(state);
        if is_redirect_token(&tok) {
            if let Some(redir) = parse_redirect(state) {
                node.end_index = redir.end_index;
                node.text = state.src[node.start_index..node.end_index].to_string();
                node.children.push(redir);
            } else {
                break;
            }
        } else {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_command() {
        let result = parse_source("echo hello", 0);
        match result {
            ParseResult::Success(node) => {
                assert_eq!(node.node_type, "program");
                assert!(!node.children.is_empty());
                let cmd = &node.children[0];
                assert_eq!(cmd.node_type, "command");
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_pipeline() {
        let result = parse_source("ls | grep foo", 0);
        match result {
            ParseResult::Success(node) => {
                assert_eq!(node.node_type, "program");
                let pipeline = &node.children[0];
                assert_eq!(pipeline.node_type, "pipeline");
                assert_eq!(pipeline.children.len(), 2);
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_and_or() {
        let result = parse_source("cmd1 && cmd2 || cmd3", 0);
        match result {
            ParseResult::Success(node) => {
                assert_eq!(node.node_type, "program");
                let list = &node.children[0];
                assert_eq!(list.node_type, "list");
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_variable_assignment() {
        let result = parse_source("VAR=hello echo $VAR", 0);
        match result {
            ParseResult::Success(node) => {
                assert_eq!(node.node_type, "program");
                let cmd = &node.children[0];
                assert_eq!(cmd.node_type, "command");
                // Should have assignment + command name + arg
                assert!(cmd.children.len() >= 2);
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_if_statement() {
        let result = parse_source("if true; then echo yes; fi", 0);
        match result {
            ParseResult::Success(node) => {
                assert_eq!(node.node_type, "program");
                let if_stmt = &node.children[0];
                assert_eq!(if_stmt.node_type, "if_statement");
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_for_loop() {
        let result = parse_source("for i in 1 2 3; do echo $i; done", 0);
        match result {
            ParseResult::Success(node) => {
                assert_eq!(node.node_type, "program");
                let for_stmt = &node.children[0];
                assert_eq!(for_stmt.node_type, "for_statement");
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_subshell() {
        let result = parse_source("(echo hello)", 0);
        match result {
            ParseResult::Success(node) => {
                let sub = &node.children[0];
                assert_eq!(sub.node_type, "subshell");
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_redirect() {
        let result = parse_source("echo hello > output.txt", 0);
        match result {
            ParseResult::Success(node) => {
                let cmd = &node.children[0];
                assert_eq!(cmd.node_type, "command");
                // Should have redirect child
                assert!(cmd.children.iter().any(|c| c.node_type == "file_redirect"));
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_command_substitution() {
        let result = parse_source("echo $(date)", 0);
        match result {
            ParseResult::Success(node) => {
                assert_eq!(node.node_type, "program");
                let cmd = &node.children[0];
                assert!(cmd.find_descendant("command_substitution").is_some());
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_empty_input() {
        let result = parse_source("", 0);
        match result {
            ParseResult::Success(node) => {
                assert_eq!(node.node_type, "program");
                assert!(node.children.is_empty());
            }
            _ => panic!("Expected Success for empty input"),
        }
    }

    #[test]
    fn test_single_quoted_string() {
        let result = parse_source("echo 'hello world'", 0);
        match result {
            ParseResult::Success(node) => {
                let cmd = &node.children[0];
                assert!(cmd.find_descendant("raw_string").is_some());
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_double_quoted_string() {
        let result = parse_source("echo \"hello $USER\"", 0);
        match result {
            ParseResult::Success(node) => {
                let cmd = &node.children[0];
                assert!(cmd.find_descendant("string").is_some());
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_case_statement() {
        let result = parse_source("case $x in a) echo a;; b) echo b;; esac", 0);
        match result {
            ParseResult::Success(node) => {
                let case_stmt = &node.children[0];
                assert_eq!(case_stmt.node_type, "case_statement");
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_function_definition() {
        let result = parse_source("function foo { echo bar; }", 0);
        match result {
            ParseResult::Success(node) => {
                let func = &node.children[0];
                assert_eq!(func.node_type, "function_definition");
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_while_loop() {
        let result = parse_source("while true; do echo loop; done", 0);
        match result {
            ParseResult::Success(node) => {
                let while_stmt = &node.children[0];
                assert_eq!(while_stmt.node_type, "while_statement");
            }
            _ => panic!("Expected Success"),
        }
    }

    #[test]
    fn test_utf8_positions() {
        let result = parse_source("echo 你好", 0);
        match result {
            ParseResult::Success(node) => {
                // 'echo' = 4 bytes, space = 1 byte, '你好' = 6 bytes
                assert_eq!(node.start_index, 0);
                assert_eq!(node.end_index, 11);
            }
            _ => panic!("Expected Success"),
        }
    }
}

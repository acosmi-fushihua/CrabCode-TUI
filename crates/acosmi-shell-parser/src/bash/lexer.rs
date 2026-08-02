//! Bash 词法分析器（分词器）
//!
//! 等价于 TS bashParser.ts 中的 nextToken/advance 等函数。
//! 上下文敏感分词：处理引号、转义、heredoc、变量展开等。

use super::types::{HeredocPending, Lexer, Token, TokenType};

/// 跳过空白字符（不含换行）
pub fn skip_whitespace(lex: &mut Lexer) {
    while !lex.at_end() {
        match lex.current_char() {
            Some(' ' | '\t' | '\r') => lex.advance(),
            // 反斜杠续行
            Some('\\') if lex.peek_char(1) == Some('\n') => {
                lex.advance(); // skip '\'
                lex.advance(); // skip '\n'
            }
            _ => break,
        }
    }
}

/// 获取下一个 token
pub fn next_token(lex: &mut Lexer) -> Token {
    skip_whitespace(lex);

    if lex.at_end() {
        return Token::eof(lex.b);
    }

    let start_byte = lex.b;
    let ch = match lex.current_char() {
        Some(c) => c,
        None => return Token::eof(lex.b),
    };

    match ch {
        '\n' => {
            lex.advance();
            // 处理 pending heredocs
            process_pending_heredocs(lex);
            Token::new(TokenType::Newline, "\n", start_byte, lex.b)
        }

        '#' => {
            // 注释：读到行尾
            let start = lex.b;
            while !lex.at_end() && lex.current_char() != Some('\n') {
                lex.advance();
            }
            Token::new(
                TokenType::Comment,
                lex.src[start..lex.b].to_string(),
                start,
                lex.b,
            )
        }

        ';' => {
            lex.advance();
            if lex.current_char() == Some(';') {
                lex.advance();
                if lex.current_char() == Some('&') {
                    lex.advance();
                    Token::new(TokenType::SemiSemiAnd, ";;&", start_byte, lex.b)
                } else {
                    Token::new(TokenType::DoubleSemi, ";;", start_byte, lex.b)
                }
            } else if lex.current_char() == Some('&') {
                lex.advance();
                Token::new(TokenType::SemiAnd, ";&", start_byte, lex.b)
            } else {
                Token::new(TokenType::Semi, ";", start_byte, lex.b)
            }
        }

        '|' => {
            lex.advance();
            if lex.current_char() == Some('|') {
                lex.advance();
                Token::new(TokenType::Or, "||", start_byte, lex.b)
            } else if lex.current_char() == Some('&') {
                lex.advance();
                Token::new(TokenType::PipeAnd, "|&", start_byte, lex.b)
            } else {
                Token::new(TokenType::Pipe, "|", start_byte, lex.b)
            }
        }

        '&' => {
            lex.advance();
            if lex.current_char() == Some('&') {
                lex.advance();
                Token::new(TokenType::And, "&&", start_byte, lex.b)
            } else if lex.current_char() == Some('>') {
                lex.advance();
                if lex.current_char() == Some('>') {
                    lex.advance();
                    Token::new(TokenType::RedirectAllApp, "&>>", start_byte, lex.b)
                } else {
                    Token::new(TokenType::RedirectAll, "&>", start_byte, lex.b)
                }
            } else {
                Token::new(TokenType::Amp, "&", start_byte, lex.b)
            }
        }

        '>' => {
            lex.advance();
            if lex.current_char() == Some('>') {
                lex.advance();
                Token::new(TokenType::RedirectAppend, ">>", start_byte, lex.b)
            } else if lex.current_char() == Some('&') {
                lex.advance();
                Token::new(TokenType::RedirectDupOut, ">&", start_byte, lex.b)
            } else if lex.current_char() == Some('|') {
                lex.advance();
                Token::new(TokenType::RedirectClobber, ">|", start_byte, lex.b)
            } else if lex.current_char() == Some('(') {
                lex.advance();
                Token::new(TokenType::ProcSubOut, ">(", start_byte, lex.b)
            } else {
                Token::new(TokenType::RedirectOut, ">", start_byte, lex.b)
            }
        }

        '<' => {
            lex.advance();
            if lex.current_char() == Some('<') {
                lex.advance();
                if lex.current_char() == Some('<') {
                    lex.advance();
                    Token::new(TokenType::RedirectHereStr, "<<<", start_byte, lex.b)
                } else {
                    // << heredoc — 记录 pending
                    let strip_tabs = lex.current_char() == Some('-');
                    if strip_tabs {
                        lex.advance();
                    }
                    Token::new(
                        TokenType::RedirectHereDoc,
                        if strip_tabs { "<<-" } else { "<<" },
                        start_byte,
                        lex.b,
                    )
                }
            } else if lex.current_char() == Some('&') {
                lex.advance();
                Token::new(TokenType::RedirectDupIn, "<&", start_byte, lex.b)
            } else if lex.current_char() == Some('(') {
                lex.advance();
                Token::new(TokenType::ProcSubIn, "<(", start_byte, lex.b)
            } else {
                Token::new(TokenType::RedirectIn, "<", start_byte, lex.b)
            }
        }

        '(' => {
            lex.advance();
            Token::new(TokenType::LParen, "(", start_byte, lex.b)
        }
        ')' => {
            lex.advance();
            Token::new(TokenType::RParen, ")", start_byte, lex.b)
        }

        '{' => {
            lex.advance();
            Token::new(TokenType::LBrace, "{", start_byte, lex.b)
        }
        '}' => {
            lex.advance();
            Token::new(TokenType::RBrace, "}", start_byte, lex.b)
        }

        '[' => {
            lex.advance();
            if lex.current_char() == Some('[') {
                lex.advance();
                Token::new(TokenType::DblLBracket, "[[", start_byte, lex.b)
            } else {
                Token::new(TokenType::LBracket, "[", start_byte, lex.b)
            }
        }
        ']' => {
            lex.advance();
            if lex.current_char() == Some(']') {
                lex.advance();
                Token::new(TokenType::DblRBracket, "]]", start_byte, lex.b)
            } else {
                Token::new(TokenType::RBracket, "]", start_byte, lex.b)
            }
        }

        '$' => lex_dollar(lex, start_byte),

        '\'' => {
            lex.advance();
            Token::new(TokenType::SQuote, "'", start_byte, lex.b)
        }

        '"' => {
            lex.advance();
            Token::new(TokenType::DQuote, "\"", start_byte, lex.b)
        }

        '`' => {
            lex.advance();
            Token::new(TokenType::Backtick, "`", start_byte, lex.b)
        }

        // 数字后跟重定向操作符
        '0'..='9' if is_fd_redirect(lex) => {
            let num_start = lex.b;
            while !lex.at_end() && lex.current_char().is_some_and(|c| c.is_ascii_digit()) {
                lex.advance();
            }
            let num_text: String = lex.src[num_start..lex.b].to_string();
            Token::new(TokenType::Number, num_text, num_start, lex.b)
        }

        // 普通 word
        _ => lex_word(lex, start_byte),
    }
}

/// 解析 $ 开头的 token
fn lex_dollar(lex: &mut Lexer, start_byte: usize) -> Token {
    lex.advance(); // skip '$'

    match lex.current_char() {
        Some('(') => {
            lex.advance(); // skip '('
            if lex.current_char() == Some('(') {
                lex.advance(); // skip second '('
                Token::new(TokenType::DollarDParen, "$((", start_byte, lex.b)
            } else {
                Token::new(TokenType::DollarParen, "$(", start_byte, lex.b)
            }
        }
        Some('{') => {
            lex.advance();
            Token::new(TokenType::DollarBrace, "${", start_byte, lex.b)
        }
        Some('[') => {
            lex.advance();
            Token::new(TokenType::DollarBracket, "$[", start_byte, lex.b)
        }
        Some('\'') => {
            lex.advance();
            Token::new(TokenType::DollarSQuote, "$'", start_byte, lex.b)
        }
        Some('"') => {
            lex.advance();
            Token::new(TokenType::DollarDQuote, "$\"", start_byte, lex.b)
        }
        Some('@') => {
            lex.advance();
            Token::new(TokenType::DollarAt, "$@", start_byte, lex.b)
        }
        Some('*') => {
            lex.advance();
            Token::new(TokenType::DollarStar, "$*", start_byte, lex.b)
        }
        Some(c) if is_special_var(c) => {
            lex.advance();
            let text = format!("${c}");
            Token::new(TokenType::DollarSpecial, text, start_byte, lex.b)
        }
        Some(c) if c.is_alphanumeric() || c == '_' => {
            // $VAR_NAME
            let var_start = lex.b;
            while !lex.at_end() {
                match lex.current_char() {
                    Some(c) if c.is_alphanumeric() || c == '_' => lex.advance(),
                    _ => break,
                }
            }
            let text = format!("${}", &lex.src[var_start..lex.b]);
            Token::new(TokenType::DollarSimple, text, start_byte, lex.b)
        }
        _ => {
            // 裸 $ 当作 word
            Token::new(TokenType::Word, "$", start_byte, lex.b)
        }
    }
}

/// 解析普通 word token
fn lex_word(lex: &mut Lexer, start_byte: usize) -> Token {
    while !lex.at_end() {
        match lex.current_char() {
            Some(c) if is_word_break(c) => break,
            Some('\\') => {
                lex.advance(); // skip '\'
                if !lex.at_end() {
                    lex.advance(); // skip escaped char
                }
            }
            _ => lex.advance(),
        }
    }
    let text = lex.src[start_byte..lex.b].to_string();
    Token::new(TokenType::Word, text, start_byte, lex.b)
}

/// 判断字符是否为 word 边界
#[must_use]
const fn is_word_break(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t'
            | '\n'
            | '\r'
            | ';'
            | '|'
            | '&'
            | '>'
            | '<'
            | '('
            | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '\''
            | '"'
            | '`'
            | '$'
            | '#'
    )
}

/// 判断字符是否为特殊变量名
#[must_use]
const fn is_special_var(ch: char) -> bool {
    matches!(ch, '?' | '$' | '!' | '#' | '-' | '0' | '_')
}

/// 判断当前位置是否为 fd+redirect（如 2>）
fn is_fd_redirect(lex: &Lexer) -> bool {
    let remaining = &lex.src[lex.b..];
    let mut chars = remaining.chars();
    // 跳过数字
    let mut found_digit = false;
    loop {
        match chars.next() {
            Some(c) if c.is_ascii_digit() => {
                found_digit = true;
            }
            Some('>' | '<') if found_digit => return true,
            _ => return false,
        }
    }
}

/// 处理 pending heredocs（在遇到换行时调用）
fn process_pending_heredocs(lex: &mut Lexer) {
    if lex.heredocs.is_empty() {
        return;
    }

    let pending: Vec<HeredocPending> = lex.heredocs.drain(..).collect();
    for mut hd in pending {
        hd.body_start = lex.b;
        // 扫描 heredoc 正文：查找单独一行的分隔符
        let _body_start = lex.b;
        loop {
            if lex.at_end() {
                hd.body_end = lex.b;
                hd.end_start = lex.b;
                hd.end_end = lex.b;
                break;
            }
            // 记录行起始
            let line_start = lex.b;
            let line_char_start = lex.i;
            // 如果 strip_tabs，跳过前导 tab
            if hd.strip_tabs {
                while lex.current_char() == Some('\t') {
                    lex.advance();
                }
            }
            // 检查是否匹配分隔符
            let check_start = lex.b;
            let mut matched = true;
            for delim_ch in hd.delim.chars() {
                if lex.current_char() != Some(delim_ch) {
                    matched = false;
                    break;
                }
                lex.advance();
            }
            // 分隔符后必须是换行或 EOF
            if matched && (lex.at_end() || lex.current_char() == Some('\n')) {
                hd.body_end = line_start;
                hd.end_start = check_start;
                hd.end_end = lex.b;
                if lex.current_char() == Some('\n') {
                    lex.advance();
                }
                break;
            }
            // 不匹配，回退到行起始后继续读到行尾
            // 直接读到下一个换行
            // 先恢复到 line_start
            lex.b = line_start;
            lex.i = line_char_start;
            // 读完整行
            while !lex.at_end() && lex.current_char() != Some('\n') {
                lex.advance();
            }
            if lex.current_char() == Some('\n') {
                lex.advance();
            }
        }
        // heredoc 已处理完毕（信息保存在 hd 中，可供 AST 使用）
    }
}

/// 读取单引号字符串内容（直到匹配的 `'`）
pub fn read_single_quoted(lex: &mut Lexer) -> String {
    let start = lex.b;
    while !lex.at_end() {
        if lex.current_char() == Some('\'') {
            let text = lex.src[start..lex.b].to_string();
            lex.advance(); // skip closing '
            return text;
        }
        lex.advance();
    }
    // 未闭合
    lex.src[start..lex.b].to_string()
}

/// 读取双引号字符串内容（处理转义和嵌入展开）
pub fn read_double_quoted(lex: &mut Lexer) -> String {
    let _start = lex.b;
    let mut result = String::new();
    while !lex.at_end() {
        match lex.current_char() {
            Some('"') => {
                lex.advance(); // skip closing "
                return result;
            }
            Some('\\') => {
                lex.advance(); // skip '\'
                if let Some(escaped) = lex.current_char() {
                    match escaped {
                        '$' | '`' | '"' | '\\' | '\n' => {
                            if escaped != '\n' {
                                result.push(escaped);
                            }
                        }
                        _ => {
                            result.push('\\');
                            result.push(escaped);
                        }
                    }
                    lex.advance();
                }
            }
            Some('$' | '`') => {
                // 展开在解析器层处理，这里保留原文
                result.push(lex.current_char().unwrap_or_default());
                lex.advance();
            }
            Some(c) => {
                result.push(c);
                lex.advance();
            }
            None => break,
        }
    }
    result
}

/// 读取 ANSI-C 引用 $'...' 内容
pub fn read_ansi_c_quoted(lex: &mut Lexer) -> String {
    let mut result = String::new();
    while !lex.at_end() {
        match lex.current_char() {
            Some('\'') => {
                lex.advance();
                return result;
            }
            Some('\\') => {
                lex.advance();
                if let Some(esc) = lex.current_char() {
                    let ch = match esc {
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        'a' => '\x07',
                        'b' => '\x08',
                        'e' | 'E' => '\x1b',
                        'f' => '\x0c',
                        'v' => '\x0b',
                        '\\' => '\\',
                        '\'' => '\'',
                        '"' => '"',
                        '0' => '\0',
                        'x' => {
                            lex.advance();
                            let hex = read_hex_digits(lex, 2);
                            if let Ok(val) = u32::from_str_radix(&hex, 16) {
                                char::from_u32(val).unwrap_or('\u{FFFD}')
                            } else {
                                '\u{FFFD}'
                            }
                        }
                        'u' => {
                            lex.advance();
                            let hex = read_hex_digits(lex, 4);
                            if let Ok(val) = u32::from_str_radix(&hex, 16) {
                                char::from_u32(val).unwrap_or('\u{FFFD}')
                            } else {
                                '\u{FFFD}'
                            }
                        }
                        'U' => {
                            lex.advance();
                            let hex = read_hex_digits(lex, 8);
                            if let Ok(val) = u32::from_str_radix(&hex, 16) {
                                char::from_u32(val).unwrap_or('\u{FFFD}')
                            } else {
                                '\u{FFFD}'
                            }
                        }
                        _ => {
                            result.push('\\');
                            esc
                        }
                    };
                    // 对于 x/u/U 已在分支内处理
                    if matches!(esc, 'x' | 'u' | 'U') {
                        result.push(ch);
                    } else {
                        result.push(ch);
                        lex.advance();
                    }
                }
            }
            Some(c) => {
                result.push(c);
                lex.advance();
            }
            None => break,
        }
    }
    result
}

/// 读取最多 `max_digits` 个十六进制数字
fn read_hex_digits(lex: &mut Lexer, max_digits: usize) -> String {
    let mut hex = String::new();
    for _ in 0..max_digits {
        match lex.current_char() {
            Some(c) if c.is_ascii_hexdigit() => {
                hex.push(c);
                lex.advance();
            }
            _ => break,
        }
    }
    hex
}

/// 注册 pending heredoc
pub fn register_heredoc(lex: &mut Lexer, delim: String, strip_tabs: bool, quoted: bool) {
    lex.heredocs.push(HeredocPending {
        delim,
        strip_tabs,
        quoted,
        body_start: 0,
        body_end: 0,
        end_start: 0,
        end_end: 0,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_word() {
        let mut lex = Lexer::new("hello".to_string());
        let tok = next_token(&mut lex);
        assert_eq!(tok.token_type, TokenType::Word);
        assert_eq!(tok.value, "hello");
    }

    #[test]
    fn test_pipe_and_operators() {
        let mut lex = Lexer::new("a | b && c || d".to_string());
        let tok1 = next_token(&mut lex);
        assert_eq!(tok1.value, "a");
        let tok2 = next_token(&mut lex);
        assert_eq!(tok2.token_type, TokenType::Pipe);
        let tok3 = next_token(&mut lex);
        assert_eq!(tok3.value, "b");
        let tok4 = next_token(&mut lex);
        assert_eq!(tok4.token_type, TokenType::And);
        let tok5 = next_token(&mut lex);
        assert_eq!(tok5.value, "c");
        let tok6 = next_token(&mut lex);
        assert_eq!(tok6.token_type, TokenType::Or);
    }

    #[test]
    fn test_redirects() {
        let mut lex = Lexer::new("cmd > file >> append".to_string());
        let _ = next_token(&mut lex); // cmd
        let tok = next_token(&mut lex);
        assert_eq!(tok.token_type, TokenType::RedirectOut);
        let _ = next_token(&mut lex); // file
        let tok2 = next_token(&mut lex);
        assert_eq!(tok2.token_type, TokenType::RedirectAppend);
    }

    #[test]
    fn test_dollar_expansions() {
        let mut lex = Lexer::new("$HOME $(cmd) ${var}".to_string());
        let tok1 = next_token(&mut lex);
        assert_eq!(tok1.token_type, TokenType::DollarSimple);
        assert_eq!(tok1.value, "$HOME");
        let tok2 = next_token(&mut lex);
        assert_eq!(tok2.token_type, TokenType::DollarParen);
        let _ = next_token(&mut lex); // cmd
        let _ = next_token(&mut lex); // )
        let tok3 = next_token(&mut lex);
        assert_eq!(tok3.token_type, TokenType::DollarBrace);
    }

    #[test]
    fn test_semicolons() {
        let mut lex = Lexer::new("a;b;;c".to_string());
        let _ = next_token(&mut lex); // a
        let tok1 = next_token(&mut lex);
        assert_eq!(tok1.token_type, TokenType::Semi);
        let _ = next_token(&mut lex); // b
        let tok2 = next_token(&mut lex);
        assert_eq!(tok2.token_type, TokenType::DoubleSemi);
    }

    #[test]
    fn test_newline() {
        let mut lex = Lexer::new("a\nb".to_string());
        let _ = next_token(&mut lex); // a
        let tok = next_token(&mut lex);
        assert_eq!(tok.token_type, TokenType::Newline);
        let tok2 = next_token(&mut lex);
        assert_eq!(tok2.value, "b");
    }

    #[test]
    fn test_comment() {
        let mut lex = Lexer::new("# this is a comment\ncmd".to_string());
        let tok = next_token(&mut lex);
        assert_eq!(tok.token_type, TokenType::Comment);
        let tok2 = next_token(&mut lex);
        assert_eq!(tok2.token_type, TokenType::Newline);
        let tok3 = next_token(&mut lex);
        assert_eq!(tok3.value, "cmd");
    }

    #[test]
    fn test_escaped_char() {
        let mut lex = Lexer::new("hello\\ world".to_string());
        let tok = next_token(&mut lex);
        assert_eq!(tok.token_type, TokenType::Word);
        assert_eq!(tok.value, "hello\\ world");
    }

    #[test]
    fn test_utf8_handling() {
        let mut lex = Lexer::new("echo 你好".to_string());
        let tok1 = next_token(&mut lex);
        assert_eq!(tok1.value, "echo");
        assert_eq!(tok1.start, 0);
        assert_eq!(tok1.end, 4);
        let tok2 = next_token(&mut lex);
        assert_eq!(tok2.value, "你好");
        assert_eq!(tok2.start, 5); // 'echo' + space = 5 bytes
        assert_eq!(tok2.end, 11); // '你' = 3 bytes, '好' = 3 bytes
    }
}

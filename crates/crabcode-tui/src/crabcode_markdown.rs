//! Markdown presentation primitives used by the native CrabCode transcript.
//!
//! This module owns renderer state only. It does not inspect, change, or
//! synthesize backend protocol messages.

use std::collections::HashMap;
use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
use ratatui::style::Color;

const MAX_MATH_SOURCE_LEN: usize = 4096;

pub(crate) fn parser_options() -> Options {
    Options::ENABLE_GFM
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_MATH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_TABLES
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CodeBlockSourceSpan {
    pub(crate) info: String,
    pub(crate) body: String,
    pub(crate) source_byte_range: Range<usize>,
}

/// Discover only structurally closed fenced blocks. The source byte range
/// excludes both delimiter lines and selects the raw body bytes.
pub(crate) fn closed_fenced_code_blocks(source: &str) -> Vec<CodeBlockSourceSpan> {
    Parser::new_ext(source, parser_options())
        .into_offset_iter()
        .filter_map(|(event, block_range)| {
            let Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(info))) = event else {
                return None;
            };
            fenced_body_range(source, block_range).map(|source_byte_range| CodeBlockSourceSpan {
                info: info.into_string(),
                body: source[source_byte_range.clone()]
                    .replace("\r\n", "\n")
                    .replace('\r', "\n"),
                source_byte_range,
            })
        })
        .collect()
}

fn fenced_body_range(source: &str, block_range: Range<usize>) -> Option<Range<usize>> {
    let block = source.get(block_range.clone())?;
    let open_newline = block.find('\n')?;
    let opening = block[..open_newline].trim_start();
    let marker = opening.chars().next()?;
    let marker_length = opening
        .chars()
        .take_while(|character| *character == marker)
        .count();
    let body_start = block_range.start.checked_add(open_newline + 1)?;
    let mut line_start = body_start;
    let mut close_start = None;
    while line_start <= block_range.end {
        let remaining = source.get(line_start..block_range.end)?;
        let line_end = remaining
            .find('\n')
            .map_or(block_range.end, |offset| line_start + offset);
        let line = source.get(line_start..line_end)?.trim_end_matches('\r');
        if is_fence_close_line(line, marker, marker_length) {
            close_start = Some(line_start);
        }
        if line_end == block_range.end {
            break;
        }
        line_start = line_end + 1;
    }
    let body_end = close_start?;
    Some(body_start..body_end)
}

fn is_fence_close_line(line: &str, marker: char, marker_length: usize) -> bool {
    let trimmed = line.trim();
    matches!(marker, '`' | '~')
        && trimmed
            .chars()
            .take_while(|character| *character == marker)
            .count()
            >= marker_length
        && trimmed
            .chars()
            .skip_while(|character| *character == marker)
            .all(char::is_whitespace)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TerminalColorLevel {
    None,
    Basic,
    Ansi256,
    TrueColor,
}

pub(crate) fn detected_terminal_color_level() -> TerminalColorLevel {
    if std::env::var_os("NO_COLOR").is_some() {
        return TerminalColorLevel::None;
    }
    match supports_color::on(supports_color::Stream::Stdout) {
        Some(level) if level.has_16m => TerminalColorLevel::TrueColor,
        Some(level) if level.has_256 => TerminalColorLevel::Ansi256,
        Some(level) if level.has_basic => TerminalColorLevel::Basic,
        _ => TerminalColorLevel::TrueColor,
    }
}

pub(crate) fn adapt_color(color: Color, level: TerminalColorLevel) -> Option<Color> {
    match level {
        TerminalColorLevel::None => None,
        TerminalColorLevel::TrueColor => Some(color),
        TerminalColorLevel::Ansi256 => Some(match color {
            Color::Rgb(red, green, blue) => Color::Indexed(rgb_to_ansi256(red, green, blue)),
            other => other,
        }),
        TerminalColorLevel::Basic => Some(match color {
            Color::Rgb(red, green, blue) => Color::Indexed(rgb_to_ansi16(red, green, blue)),
            Color::Indexed(index) => {
                let (red, green, blue) = ansi256_rgb(index);
                Color::Indexed(rgb_to_ansi16(red, green, blue))
            }
            other => other,
        }),
    }
}

pub(crate) fn rgb_to_ansi256(red: u8, green: u8, blue: u8) -> u8 {
    (16_u16..=255)
        .map(|index| {
            let (candidate_red, candidate_green, candidate_blue) = ansi256_rgb(index as u8);
            let distance = squared_distance(
                (red, green, blue),
                (candidate_red, candidate_green, candidate_blue),
            );
            (distance, index as u8)
        })
        .min()
        .map_or(16, |(_, index)| index)
}

fn rgb_to_ansi16(red: u8, green: u8, blue: u8) -> u8 {
    (0_u8..=15)
        .map(|index| {
            let candidate = ansi256_rgb(index);
            (squared_distance((red, green, blue), candidate), index)
        })
        .min()
        .map_or(7, |(_, index)| index)
}

fn squared_distance(left: (u8, u8, u8), right: (u8, u8, u8)) -> u32 {
    let red = i32::from(left.0) - i32::from(right.0);
    let green = i32::from(left.1) - i32::from(right.1);
    let blue = i32::from(left.2) - i32::from(right.2);
    (red * red + green * green + blue * blue) as u32
}

fn ansi256_rgb(index: u8) -> (u8, u8, u8) {
    const ANSI16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match index {
        0..=15 => ANSI16[usize::from(index)],
        16..=231 => {
            let value = index - 16;
            let component = |part: u8| if part == 0 { 0 } else { 55 + 40 * part };
            (
                component(value / 36),
                component((value / 6) % 6),
                component(value % 6),
            )
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            (value, value, value)
        }
    }
}

/// Normalize model-emitted LaTeX delimiters before parsing. Code spans and
/// fenced blocks remain byte-for-byte unchanged.
pub(crate) fn normalize_latex_delimiters(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut index = 0;
    let mut at_line_start = true;
    let mut fence: Option<(u8, usize)> = None;
    while index < source.len() {
        if let Some((marker, length)) = fence {
            let line_end = source[index..]
                .find('\n')
                .map_or(source.len(), |offset| index + offset + 1);
            let line = source[index..line_end].trim_end_matches(['\r', '\n']);
            output.push_str(&source[index..line_end]);
            if fence_close(line, marker, length) {
                fence = None;
            }
            at_line_start = line_end < source.len();
            index = line_end;
            continue;
        }

        if at_line_start && let Some((marker, length)) = fence_open(source, index) {
            let line_end = source[index..]
                .find('\n')
                .map_or(source.len(), |offset| index + offset + 1);
            output.push_str(&source[index..line_end]);
            fence = Some((marker, length));
            at_line_start = line_end < source.len();
            index = line_end;
            continue;
        }

        if bytes[index] == b'\n' {
            output.push('\n');
            index += 1;
            at_line_start = true;
            continue;
        }
        at_line_start = false;

        if bytes[index] == b'`' {
            let length = ascii_run(bytes, index, b'`');
            let delimiter = &source[index..index + length];
            if let Some(relative) = source[index + length..].find(delimiter).filter(|relative| {
                !source[index + length..index + length + relative].contains('\n')
            }) {
                let end = index + length + relative + length;
                output.push_str(&source[index..end]);
                index = end;
            } else {
                output.push_str(delimiter);
                index += length;
            }
            continue;
        }

        if bytes[index] == b'\\' {
            if source[index..].starts_with("\\\\") {
                output.push_str("\\\\");
                index += 2;
                continue;
            }
            if source[index..].starts_with("\\(") {
                if let Some(close) = find_unescaped(&source[index + 2..], "\\)") {
                    let inner_start = index + 2;
                    let inner_end = inner_start + close;
                    let inner = source[inner_start..inner_end]
                        .trim_matches(|character: char| character.is_ascii_whitespace());
                    output.push('$');
                    push_joined_lines(&mut output, inner);
                    output.push('$');
                    index = inner_end + 2;
                } else {
                    output.push('$');
                    index += 2;
                }
                continue;
            }
            if source[index..].starts_with("\\[") {
                if let Some(close) = find_unescaped(&source[index + 2..], "\\]") {
                    let inner_start = index + 2;
                    let inner_end = inner_start + close;
                    push_display_math(&mut output, &source[inner_start..inner_end]);
                    index = inner_end + 2;
                } else {
                    output.push_str("$$");
                    index += 2;
                }
                continue;
            }
            if let Some((open, close)) = equation_environment_at(&source[index..]) {
                if let Some(relative) = source[index + open.len()..].find(close) {
                    let inner_start = index + open.len();
                    let inner_end = inner_start + relative;
                    push_display_math(&mut output, &source[inner_start..inner_end]);
                    index = inner_end + close.len();
                } else {
                    output.push_str("$$");
                    index += open.len();
                }
                continue;
            }
            let mut converted_close = false;
            for (delimiter, replacement) in [
                ("\\)", "$"),
                ("\\]", "$$"),
                ("\\end{equation}", "$$"),
                ("\\end{equation*}", "$$"),
            ] {
                if source[index..].starts_with(delimiter) {
                    output.push_str(replacement);
                    index += delimiter.len();
                    converted_close = true;
                    break;
                }
            }
            if converted_close {
                continue;
            }
            output.push('\\');
            index += 1;
            continue;
        }

        if source[index..].starts_with("$$")
            && let Some(close) = source[index + 2..].find("$$")
        {
            let inner_start = index + 2;
            let inner_end = inner_start + close;
            push_display_math(&mut output, &source[inner_start..inner_end]);
            index = inner_end + 2;
            continue;
        }

        let character = source[index..]
            .chars()
            .next()
            .expect("index remains on a UTF-8 boundary");
        output.push(character);
        index += character.len_utf8();
    }
    output
}

fn equation_environment_at(source: &str) -> Option<(&'static str, &'static str)> {
    [
        ("\\begin{equation*}", "\\end{equation*}"),
        ("\\begin{equation}", "\\end{equation}"),
    ]
    .into_iter()
    .find(|(open, _)| source.starts_with(open))
}

fn find_unescaped(source: &str, needle: &str) -> Option<usize> {
    let mut search = 0;
    while let Some(relative) = source[search..].find(needle) {
        let offset = search + relative;
        let preceding = source[..offset]
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'\\')
            .count();
        if preceding % 2 == 0 {
            return Some(offset);
        }
        search = offset + 1;
    }
    None
}

fn push_joined_lines(output: &mut String, source: &str) {
    let mut lines = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    if let Some(first) = lines.next() {
        output.push_str(first);
        for line in lines {
            output.push(' ');
            output.push_str(line);
        }
    }
}

fn push_display_math(output: &mut String, source: &str) {
    output.push_str("$$");
    push_joined_lines(output, source);
    output.push_str("$$");
}

fn fence_open(source: &str, index: usize) -> Option<(u8, usize)> {
    let line_end = source[index..]
        .find('\n')
        .map_or(source.len(), |offset| index + offset);
    let line = &source.as_bytes()[index..line_end];
    let spaces = line.iter().take_while(|byte| **byte == b' ').count();
    if spaces > 3 {
        return None;
    }
    let marker = *line.get(spaces)?;
    if !matches!(marker, b'`' | b'~') {
        return None;
    }
    let length = ascii_run(line, spaces, marker);
    (length >= 3).then_some((marker, length))
}

fn fence_close(line: &str, marker: u8, length: usize) -> bool {
    let bytes = line.as_bytes();
    let spaces = bytes.iter().take_while(|byte| **byte == b' ').count();
    if spaces > 3 || bytes.get(spaces) != Some(&marker) {
        return false;
    }
    let run = ascii_run(bytes, spaces, marker);
    run >= length
        && bytes[spaces + run..]
            .iter()
            .all(|byte| matches!(byte, b' ' | b'\t'))
}

fn ascii_run(bytes: &[u8], start: usize, value: u8) -> usize {
    bytes[start..]
        .iter()
        .take_while(|byte| **byte == value)
        .count()
}

pub(crate) fn latex_to_unicode_inline(source: &str) -> Option<String> {
    (source.len() <= MAX_MATH_SOURCE_LEN).then(|| render_latex_sequence(source))
}

pub(crate) fn latex_to_unicode_display(source: &str) -> Option<Vec<String>> {
    let rendered = latex_to_unicode_inline(source)?;
    let lines = rendered
        .split("\\\\")
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    Some(lines)
}

fn render_latex_sequence(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut index = 0;
    while index < source.len() {
        let remainder = &source[index..];
        if let Some(kind) = remainder
            .chars()
            .next()
            .filter(|character| matches!(character, '^' | '_'))
        {
            index += kind.len_utf8();
            let (atom, consumed) = latex_atom(&source[index..]);
            let rendered = render_latex_sequence(atom);
            let mapped = rendered
                .chars()
                .map(|character| {
                    if kind == '^' {
                        superscript(character)
                    } else {
                        subscript(character)
                    }
                })
                .collect::<Option<String>>();
            if let Some(mapped) = mapped {
                output.push_str(&mapped);
            } else {
                output.push(kind);
                if rendered.chars().count() > 1 {
                    output.push('(');
                    output.push_str(&rendered);
                    output.push(')');
                } else {
                    output.push_str(&rendered);
                }
            }
            index += consumed;
            continue;
        }
        if let Some(escaped) = remainder.strip_prefix('\\') {
            let name_length = escaped
                .chars()
                .take_while(char::is_ascii_alphabetic)
                .map(char::len_utf8)
                .sum::<usize>();
            if name_length > 0 {
                let name = &escaped[..name_length];
                output.push_str(latex_symbol(name).unwrap_or(name));
                index += 1 + name_length;
                continue;
            }
        }
        let character = remainder
            .chars()
            .next()
            .expect("index remains on a UTF-8 boundary");
        match character {
            '{' | '}' | '$' => {}
            '-' => output.push('−'),
            '\'' => output.push('′'),
            '~' => output.push(' '),
            _ => output.push(character),
        }
        index += character.len_utf8();
    }
    output
}

fn latex_atom(source: &str) -> (&str, usize) {
    if let Some(rest) = source.strip_prefix('{') {
        let mut depth = 1_usize;
        for (offset, character) in rest.char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        return (&rest[..offset], offset + 2);
                    }
                }
                _ => {}
            }
        }
        return (rest, source.len());
    }
    if let Some(rest) = source.strip_prefix('\\') {
        let length = rest
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .map(char::len_utf8)
            .sum::<usize>();
        if length > 0 {
            return (&source[..1 + length], 1 + length);
        }
    }
    source
        .char_indices()
        .nth(1)
        .map_or((source, source.len()), |(end, _)| (&source[..end], end))
}

fn superscript(character: char) -> Option<char> {
    Some(match character {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' | '−' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'x' => 'ˣ',
        'T' => 'ᵀ',
        'i' => 'ⁱ',
        'n' => 'ⁿ',
        't' => 'ᵗ',
        'h' => 'ʰ',
        _ => return None,
    })
}

fn subscript(character: char) -> Option<char> {
    Some(match character {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' | '−' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'r' => 'ᵣ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

fn latex_symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" | "varepsilon" => "ε",
        "theta" => "θ",
        "lambda" => "λ",
        "mu" => "μ",
        "pi" => "π",
        "rho" => "ρ",
        "sigma" => "σ",
        "phi" | "varphi" => "φ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Phi" => "Φ",
        "Omega" => "Ω",
        "times" => "×",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "to" | "rightarrow" => "→",
        "in" => "∈",
        "cup" => "∪",
        "sum" => "∑",
        _ => return None,
    })
}

pub(crate) type ReferenceDefinitions = HashMap<String, String>;

pub(crate) fn reference_definitions(source: &str) -> ReferenceDefinitions {
    source.lines().filter_map(reference_definition).collect()
}

pub(crate) fn is_reference_definition(line: &str) -> bool {
    reference_definition(line).is_some()
}

fn reference_definition(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    let label_end = trimmed.strip_prefix('[')?.find("]:")? + 1;
    let label = trimmed.get(1..label_end)?.trim().to_ascii_lowercase();
    let target = trimmed.get(label_end + 2..)?.trim();
    let target = target
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
        .unwrap_or_else(|| target.split_ascii_whitespace().next().unwrap_or_default());
    (!label.is_empty() && !target.is_empty()).then(|| (label, target.to_string()))
}

pub(crate) fn resolve_reference_links(input: &str, definitions: &ReferenceDefinitions) -> String {
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        let remainder = &input[index..];
        if remainder.starts_with('[')
            && let Some(label_end) = remainder[1..].find(']').map(|offset| offset + 1)
        {
            let label = &remainder[1..label_end];
            let suffix = &remainder[label_end + 1..];
            if let Some(reference_end) = suffix
                .strip_prefix('[')
                .and_then(|value| value.find(']').map(|offset| offset + 1))
            {
                let explicit = &suffix[1..reference_end];
                let key = if explicit.is_empty() { label } else { explicit };
                if let Some(target) = definitions.get(&key.trim().to_ascii_lowercase()) {
                    output.push('[');
                    output.push_str(label);
                    output.push_str("](");
                    output.push_str(target);
                    output.push(')');
                    index += label_end + 1 + reference_end + 1;
                    continue;
                }
            } else if let Some(target) = definitions.get(&label.trim().to_ascii_lowercase()) {
                output.push('[');
                output.push_str(label);
                output.push_str("](");
                output.push_str(target);
                output.push(')');
                index += label_end + 1;
                continue;
            }
        }
        let character = remainder
            .chars()
            .next()
            .expect("index remains on a UTF-8 boundary");
        output.push(character);
        index += character.len_utf8();
    }
    output
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodeTokenClass {
    Plain,
    Key,
    String,
    Number,
    Comment,
    Keyword,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HighlightedCodeSpan {
    pub(crate) class: CodeTokenClass,
    pub(crate) text: String,
}

pub(crate) type HighlightedCodeLine = Vec<HighlightedCodeSpan>;

#[derive(Debug, Clone, Default)]
pub(crate) struct CrabCodeOpenCodeHighlighter {
    fence_info: String,
    start_in_source: usize,
    committed_source: String,
    committed_len: usize,
    committed_lines: Vec<HighlightedCodeLine>,
}

impl CrabCodeOpenCodeHighlighter {
    pub(crate) fn highlight(
        &mut self,
        fence_info: &str,
        start_in_source: usize,
        text: &str,
    ) -> Vec<HighlightedCodeLine> {
        let rebuild = self.fence_info != fence_info
            || self.start_in_source != start_in_source
            || self.committed_len > text.len()
            || !text.starts_with(&self.committed_source);
        if rebuild {
            self.fence_info = fence_info.to_string();
            self.start_in_source = start_in_source;
            self.committed_source.clear();
            self.committed_len = 0;
            self.committed_lines.clear();
        }
        let tail = &text[self.committed_len..];
        let mut rendered = self.committed_lines.clone();
        rendered.extend(highlight_code_batch(fence_info, tail));
        if let Some(last_newline) = text.rfind('\n') {
            let new_committed_len = last_newline + 1;
            let committed_text = &text[..new_committed_len];
            self.committed_lines = highlight_code_batch(fence_info, committed_text);
            committed_text.clone_into(&mut self.committed_source);
            self.committed_len = new_committed_len;
        }
        rendered
    }
}

pub(crate) fn highlight_code_batch(fence_info: &str, text: &str) -> Vec<HighlightedCodeLine> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split_inclusive('\n')
        .map(|line| highlight_code_line(fence_info, line.trim_end_matches('\n')))
        .collect()
}

fn highlight_code_line(fence_info: &str, line: &str) -> HighlightedCodeLine {
    let language = fence_info
        .split_ascii_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(language.as_str(), "yaml" | "yml")
        && let Some((key, value)) = line.split_once(':')
    {
        let mut spans = vec![
            HighlightedCodeSpan {
                class: CodeTokenClass::Key,
                text: key.to_string(),
            },
            HighlightedCodeSpan {
                class: CodeTokenClass::Plain,
                text: ":".to_string(),
            },
        ];
        if !value.is_empty() {
            spans.push(HighlightedCodeSpan {
                class: classify_code_token(value.trim()),
                text: value.to_string(),
            });
        }
        return spans;
    }
    if line.trim_start().starts_with('#') || line.trim_start().starts_with("//") {
        return vec![HighlightedCodeSpan {
            class: CodeTokenClass::Comment,
            text: line.to_string(),
        }];
    }
    line.split_inclusive(char::is_whitespace)
        .map(|token| HighlightedCodeSpan {
            class: if matches!(
                token.trim(),
                "fn" | "let"
                    | "mut"
                    | "struct"
                    | "enum"
                    | "impl"
                    | "pub"
                    | "use"
                    | "const"
                    | "true"
                    | "false"
            ) {
                CodeTokenClass::Keyword
            } else {
                classify_code_token(token.trim())
            },
            text: token.to_string(),
        })
        .collect()
}

fn classify_code_token(token: &str) -> CodeTokenClass {
    if token.parse::<f64>().is_ok() {
        CodeTokenClass::Number
    } else if (token.starts_with('"') && token.ends_with('"'))
        || (token.starts_with('\'') && token.ends_with('\''))
    {
        CodeTokenClass::String
    } else {
        CodeTokenClass::Plain
    }
}

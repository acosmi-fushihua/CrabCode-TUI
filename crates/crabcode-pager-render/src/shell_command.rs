//! Renderer-owned shell-command display preparation.
//!
//! This module closes the fixed background-task rendering dependency tree:
//! tree-sitter-validated operator boundaries, heredoc payload protection,
//! quote-aware wrapping, and syntax highlighting. It parses only for display;
//! it owns no command execution, permission, policy, or backend authority.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use tree_sitter::{Node, Parser, Tree};
use tree_sitter_bash::LANGUAGE as BASH;
use unicode_width::UnicodeWidthStr;

use crate::syntax::get_syntect;
use crate::theme::Theme;

/// Parse the provided bash source using tree-sitter-bash, returning a Tree on
/// success or None if parsing failed.
fn try_parse_shell(src: &str) -> Option<Tree> {
    let lang = BASH.into();
    let mut parser = Parser::new();
    #[expect(clippy::expect_used)]
    parser.set_language(&lang).expect("load bash grammar");
    let old_tree: Option<&Tree> = None;
    parser.parse(src, old_tree)
}

/// Node kinds whose descendants are payload text, not shell control flow.
/// Operators that appear only as *characters* inside these are not real
/// soft-break points (e.g. `&&` in a heredoc body or double-quoted string).
const PAYLOAD_NODE_KINDS: &[&str] = &[
    // Heredoc body / content (not the redirect operator itself on the
    // command line — `cat <<EOF && true` still has a real list `&&`).
    "heredoc_body",
    "simple_heredoc_body",
    "heredoc_content",
    "heredoc_end",
    // Quoted / expansion payload
    "string",
    "raw_string",
    "string_content",
    "ansi_c_string",
    "translated_string",
    // Comments
    "comment",
];

/// True when `kind` is a shell list/pipeline operator we may soft-break after.
fn is_soft_break_operator_kind(kind: &str) -> bool {
    matches!(kind, "&&" | "||" | "|" | ";")
}

/// True when we must not descend into this node looking for operators.
fn is_payload_node_kind(kind: &str) -> bool {
    PAYLOAD_NODE_KINDS.contains(&kind)
}

/// Byte offsets into `script` **after** real shell list/pipeline operators
/// where a display soft-wrap is safe.
///
/// Uses tree-sitter-bash so `&&` / `||` / `|` / `;` that appear only inside
/// strings, heredoc bodies, or comments are **not** returned. The command-line
/// operator in `cat <<EOF && echo after` **is** returned (it is a real `list`
/// operator); the body's `foo && bar` is not.
///
/// Returns an empty vec when the script cannot be parsed at all (caller should
/// fall back to width-only word-wrap, not naive substring splits).
///
/// Offsets are sorted ascending and de-duplicated. Each offset is
/// `operator_node.end_byte()` — i.e. the split keeps the operator on the
/// preceding display row.
fn soft_break_offsets_after_operators(script: &str) -> Vec<usize> {
    let Some(tree) = try_parse_shell(script) else {
        return Vec::new();
    };

    let root = tree.root_node();
    // On a broken parse, tree-sitter can still expose `|` / `&&` / `;` nodes
    // that are *not* real shell control flow (e.g. fragments of unclosed
    // strings or half-parsed heredocs). Prefer no soft-breaks over wrong ones.
    if root.has_error() {
        return Vec::new();
    }

    let mut breaks: Vec<usize> = Vec::new();
    let mut stack: Vec<Node> = vec![root];

    while let Some(node) = stack.pop() {
        let kind = node.kind();

        // Do not walk into string / heredoc / comment payload — any operator
        // characters there are not shell syntax nodes we care about, and
        // skipping the whole subtree is cheaper and safer.
        if is_payload_node_kind(kind) {
            continue;
        }

        if is_soft_break_operator_kind(kind) {
            let end = node.end_byte();
            if end > 0 && end <= script.len() && script.is_char_boundary(end) {
                breaks.push(end);
            }
        }

        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    breaks.sort_unstable();
    breaks.dedup();
    breaks
}

/// Byte ranges of heredoc *payload* (body / content), not the `<<WORD` opener
/// on the command line.
///
/// Used by the command display so physical lines that are pure heredoc body
/// text are **not** soft-wrapped at spaces (they are free-form payload, not
/// shell syntax). Returns an empty vec when the script cannot be parsed or the
/// tree has errors.
fn heredoc_payload_byte_ranges(script: &str) -> Vec<(usize, usize)> {
    let Some(tree) = try_parse_shell(script) else {
        return Vec::new();
    };

    let root = tree.root_node();
    // Match soft-break policy: on a broken parse, tree-sitter error recovery can
    // invent or mis-bound `heredoc_body` nodes. Prefer no payload ranges (normal
    // soft-wrap / no false no-wrap) over wrong spans that overflow or skip wraps.
    if root.has_error() {
        return Vec::new();
    }

    const HEREDOC_PAYLOAD: &[&str] = &["heredoc_body", "simple_heredoc_body", "heredoc_content"];

    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut stack: Vec<Node> = vec![root];

    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if HEREDOC_PAYLOAD.contains(&kind) {
            let start = node.start_byte();
            let end = node.end_byte();
            if start < end && end <= script.len() {
                ranges.push((start, end));
            }
            // Do not walk into children — the outer body/content range is enough.
            continue;
        }
        let mut cursor = node.walk();
        let children: Vec<Node> = node.children(&mut cursor).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }

    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

/// True when the half-open byte range `[start, end)` lies entirely inside one
/// of the given (sorted) payload ranges.
fn range_fully_inside(start: usize, end: usize, ranges: &[(usize, usize)]) -> bool {
    if end < start {
        return false;
    }
    ranges.iter().any(|&(rs, re)| start >= rs && end <= re)
}

/// Split `line` (a physical line whose first byte is at `line_start` in the
/// full script that produced `breaks`) into contiguous slices at any soft
/// breaks that fall strictly inside the line.
///
/// When there are no applicable breaks, returns a single-element vec with
/// `line` unchanged.
fn split_physical_line_at_soft_breaks<'a>(
    line: &'a str,
    line_start: usize,
    breaks: &[usize],
) -> Vec<&'a str> {
    let line_end = line_start + line.len();
    let mut rel: Vec<usize> = breaks
        .iter()
        .copied()
        .filter(|&b| b > line_start && b < line_end)
        .map(|b| b - line_start)
        .filter(|&b| line.is_char_boundary(b))
        .collect();
    rel.dedup();
    if rel.is_empty() {
        return vec![line];
    }

    let mut chunks = Vec::with_capacity(rel.len() + 1);
    let mut start = 0usize;
    for b in rel {
        if b > start {
            chunks.push(&line[start..b]);
            start = b;
        }
    }
    if start < line.len() {
        chunks.push(&line[start..]);
    }
    if chunks.is_empty() {
        chunks.push(line);
    }
    chunks
}

/// Highlight a shell command string into styled spans.
///
/// Uses syntect with the best available grammar for the platform: tries
/// "powershell" first on Windows, falls back to "bash". Returns plain
/// `theme.command` color if no grammar matches.
pub(crate) fn highlight_bash_command(command: &str) -> Vec<Span<'static>> {
    let syntect = get_syntect();
    let grammar = if cfg!(windows) { "powershell" } else { "bash" };
    let Some(mut hl) = syntect
        .highlight_lines_for_token(grammar)
        .or_else(|| syntect.highlight_lines_for_token("bash"))
    else {
        let theme = Theme::current();
        return vec![Span::styled(
            command.to_string(),
            Style::default().fg(theme.command),
        )];
    };

    let line = format!("{command}\n");
    match hl.highlight_line(&line, &syntect.syntax_set) {
        Ok(ranges) => {
            let mut spans = Vec::new();
            for (style, segment) in ranges {
                let mut text = segment.to_owned();
                while text.ends_with('\n') || text.ends_with('\r') {
                    text.pop();
                }
                if text.is_empty() {
                    continue;
                }
                // Raw syntect RGB here used to bypass quantization and leak
                // polarity-tuned tmTheme colors into minimal.
                spans.push(Span::styled(
                    text,
                    crate::syntax::syntect_to_ratatui_fg(style),
                ));
            }
            if spans.is_empty() {
                let theme = Theme::current();
                vec![Span::styled(
                    command.to_string(),
                    Style::default().fg(theme.command),
                )]
            } else {
                spans
            }
        }
        Err(_) => {
            let theme = Theme::current();
            vec![Span::styled(
                command.to_string(),
                Style::default().fg(theme.command),
            )]
        }
    }
}

/// Wrap + syntax-highlight a bash command the same way the fixed command
/// display does: preserve source newlines / `\` continuations, keep heredoc
/// bodies intact, and use quote-aware width wrapping.
pub(crate) fn render_bash_command_display_lines(
    command: &str,
    content_width: usize,
) -> Vec<Line<'static>> {
    build_raw_bash_lines(command, content_width)
}

/// Normalize command text for display without destroying structure.
///
/// - Unifies line endings to `\n`
/// - Trims trailing whitespace per physical line (keeps indent)
/// - **Preserves** intentional newlines, including lines that end in `\`
///   (shell line continuations like `cmd \\\n  --flag`)
fn prepare_bash_display_text(command: &str) -> String {
    let normalized = command.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(normalized.len());
    for (i, line) in normalized.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    // Drop trailing blank lines (common when scripts end with `\n`)
    // but keep interior blank lines.
    while out.ends_with('\n') {
        let without = &out[..out.len() - 1];
        if without.ends_with('\\') {
            // Dangling `\` continuation at EOF: keep the backslash visible on
            // the last line but drop the now-useless trailing newline, which
            // would otherwise render as a stray empty row.
            out.pop();
            break;
        }
        if without.is_empty() || without.ends_with('\n') {
            out.pop();
            continue;
        }
        // Single trailing newline after content — drop it.
        out.pop();
        break;
    }
    out
}

/// Soft-wrap one physical line using tree-sitter-derived break offsets from
/// the full script. Breaks only at real list/pipeline operators (never `&&`
/// inside heredocs, quotes, or comments).
fn soft_wrap_physical_line(
    line: &str,
    line_start: usize,
    full_breaks: &[usize],
    heredoc_payload: &[(usize, usize)],
    content_width: usize,
) -> Vec<Line<'static>> {
    highlight_rows(soft_wrap_row_texts(
        line,
        line_start,
        full_breaks,
        heredoc_payload,
        content_width,
    ))
}

/// Compute the display row string slices for one physical `line`, applying the
/// same operator-aware + quote-aware wrapping as [`soft_wrap_physical_line`]
/// but without highlighting.
fn soft_wrap_row_texts<'a>(
    line: &'a str,
    line_start: usize,
    full_breaks: &[usize],
    heredoc_payload: &[(usize, usize)],
    content_width: usize,
) -> Vec<&'a str> {
    if content_width == 0 {
        return vec![line];
    }

    if UnicodeWidthStr::width(line) <= content_width {
        return vec![line];
    }

    // Heredoc body/content is free-form payload, not shell syntax — do not
    // soft-wrap at spaces. Keep the physical line intact even if it overflows.
    let line_end = line_start + line.len();
    if range_fully_inside(line_start, line_end, heredoc_payload) {
        return vec![line];
    }

    let chunks = split_physical_line_at_soft_breaks(line, line_start, full_breaks);
    // No real operators on this line (or parse found none) — quote-aware wrap.
    if chunks.len() <= 1 {
        return bash_quote_aware_wrap(line, content_width);
    }

    // Pack contiguous partitions of `line` into rows that fit the width.
    // Chunks are contiguous slices, so a packed row is just line[start..end].
    let mut chunk_starts: Vec<usize> = Vec::with_capacity(chunks.len());
    {
        let mut cursor = 0usize;
        for chunk in &chunks {
            debug_assert_eq!(&line[cursor..cursor + chunk.len()], *chunk);
            chunk_starts.push(cursor);
            cursor += chunk.len();
        }
    }

    // Row specs as (start, end) into `line`, then trim for display.
    let mut row_ranges: Vec<(usize, usize)> = Vec::new();
    let mut i = 0usize;
    while i < chunks.len() {
        // Skip leading whitespace when *starting* a continuation after a
        // previous row — that space belonged between operator and next cmd.
        let mut start = chunk_starts[i];
        if !row_ranges.is_empty() {
            while start < line.len() && line.as_bytes()[start].is_ascii_whitespace() {
                start += 1;
            }
            // Advance i if we skipped entire leading chunks of whitespace.
            while i < chunks.len() && chunk_starts[i] + chunks[i].len() <= start {
                i += 1;
            }
            if i >= chunks.len() {
                break;
            }
            // If we partially skipped into the current chunk, use `start`.
            if chunk_starts[i] < start {
                // start is inside chunks[i]
            } else {
                start = chunk_starts[i];
            }
        }

        let mut last_fit = i;
        let mut j = i;
        while j < chunks.len() {
            let end = chunk_starts[j] + chunks[j].len();
            // Display width from `start` (whitespace-trimmed for continuations).
            let slice = &line[start..end];
            if UnicodeWidthStr::width(slice) <= content_width {
                last_fit = j;
                j += 1;
            } else {
                break;
            }
        }
        if j == i {
            // Chunk alone exceeds width — emit rest of this chunk for quote-wrap.
            let end = chunk_starts[i] + chunks[i].len();
            row_ranges.push((start, end));
            i += 1;
        } else {
            let end = chunk_starts[last_fit] + chunks[last_fit].len();
            row_ranges.push((start, end));
            i = last_fit + 1;
        }
    }

    let mut out = Vec::new();
    for (start, end) in row_ranges {
        let row = line[start..end].trim_end();
        // Continuations already had leading ws skipped via `start`; first row
        // keeps any intentional indent.
        if UnicodeWidthStr::width(row) <= content_width {
            out.push(row);
        } else {
            out.extend(bash_quote_aware_wrap(row, content_width));
        }
    }
    out
}

fn highlight_rows<'a, I>(rows: I) -> Vec<Line<'static>>
where
    I: IntoIterator<Item = &'a str>,
{
    rows.into_iter()
        .map(|row| Line::from(highlight_bash_command(row)))
        .collect()
}

/// Word-wrap a bash fragment without breaking on whitespace that sits inside
/// single- or double-quoted strings.
fn bash_quote_aware_wrap(line: &str, width: usize) -> Vec<&str> {
    if width == 0 || UnicodeWidthStr::width(line) <= width {
        return vec![line];
    }

    let break_after = quote_aware_break_points(line);
    if break_after.is_empty() {
        // Nowhere safe to break (entire line is one quoted span, or no spaces).
        return vec![line];
    }

    let mut rows: Vec<&str> = Vec::new();
    let mut row_start = 0usize;
    let mut last_break = 0usize;

    let mut candidates = break_after;
    candidates.push(line.len());

    for &b in &candidates {
        if b <= row_start {
            continue;
        }
        let candidate = line[row_start..b].trim_end();
        if UnicodeWidthStr::width(candidate) <= width {
            last_break = b;
            continue;
        }
        if last_break > row_start {
            let row = line[row_start..last_break].trim_end();
            if !row.is_empty() {
                rows.push(row);
            }
            row_start = last_break;
            while row_start < line.len() && line.as_bytes()[row_start].is_ascii_whitespace() {
                row_start += 1;
            }
            last_break = row_start;
            if b > row_start {
                let candidate = line[row_start..b].trim_end();
                if UnicodeWidthStr::width(candidate) <= width {
                    last_break = b;
                } else {
                    let force_end = b;
                    let row = line[row_start..force_end].trim_end();
                    if !row.is_empty() {
                        rows.push(row);
                    }
                    row_start = force_end;
                    while row_start < line.len() && line.as_bytes()[row_start].is_ascii_whitespace()
                    {
                        row_start += 1;
                    }
                    last_break = row_start;
                }
            }
        } else {
            let row = line[row_start..b].trim_end();
            if !row.is_empty() {
                rows.push(row);
            }
            row_start = b;
            while row_start < line.len() && line.as_bytes()[row_start].is_ascii_whitespace() {
                row_start += 1;
            }
            last_break = row_start;
        }
    }
    if row_start < line.len() {
        let row = line[row_start..].trim_end();
        if !row.is_empty() {
            rows.push(row);
        }
    }
    if rows.is_empty() { vec![line] } else { rows }
}

/// Byte offsets at the start of whitespace runs that are safe soft-wrap
/// points (outside single/double quotes).
fn quote_aware_break_points(line: &str) -> Vec<usize> {
    let bytes = line.as_bytes();
    let mut breaks = Vec::new();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;

    while i < bytes.len() {
        let c = bytes[i];
        if in_single {
            if c == b'\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }
        if in_double {
            if c == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if c == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b if b.is_ascii_whitespace() => {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                    i += 1;
                }
                if start > 0 {
                    breaks.push(start);
                }
            }
            _ => i += 1,
        }
    }
    breaks.dedup();
    breaks
}

/// Build syntax-highlighted lines for a (possibly multi-line) bash command.
///
/// Preserves intentional newlines / `\` continuations. Soft-wraps overlong
/// physical lines at tree-sitter-validated shell operators, then uses
/// quote-aware width wrapping within each segment.
fn build_raw_bash_lines(command: &str, content_width: usize) -> Vec<Line<'static>> {
    let text = prepare_bash_display_text(command);
    if text.is_empty() {
        return Vec::new();
    }

    let full_breaks = soft_break_offsets_after_operators(&text);
    let heredoc_payload = heredoc_payload_byte_ranges(&text);

    let mut out = Vec::new();
    let mut offset = 0usize;
    for (idx, physical) in text.split('\n').enumerate() {
        if idx > 0 {
            offset += 1;
        }
        out.extend(soft_wrap_physical_line(
            physical,
            offset,
            &full_breaks,
            &heredoc_payload,
            content_width,
        ));
        offset += physical.len();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: Vec<Line<'static>>) -> Vec<String> {
        lines
            .into_iter()
            .map(|line| {
                line.spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn preserves_physical_lines_and_normalizes_line_endings() {
        assert_eq!(
            plain(render_bash_command_display_lines(
                "echo a  \r\n  echo b\r\n",
                80
            )),
            vec!["echo a", "  echo b"]
        );
    }

    #[test]
    fn soft_breaks_only_on_real_shell_operators() {
        let command = r#"printf "a && b" && echo c || echo d"#;
        assert_eq!(
            plain(render_bash_command_display_lines(command, 22)),
            vec![r#"printf "a && b" &&"#, "echo c || echo d"]
        );
    }

    #[test]
    fn quote_aware_wrap_keeps_jq_filter_together() {
        let command = "gh api repos --jq '.[] | select(.name == \"long value\")' tail";
        let rows = plain(render_bash_command_display_lines(command, 20));
        assert!(
            rows.iter()
                .any(|row| row.contains("'.[] | select(.name == \"long value\")'")),
            "quoted filter must remain intact: {rows:?}"
        );
    }

    #[test]
    fn heredoc_payload_line_is_not_width_wrapped() {
        let script = "cat <<EOF && echo after\nthis is a long heredoc payload with spaces\nEOF";
        let rows = plain(render_bash_command_display_lines(script, 12));
        assert!(
            rows.iter()
                .any(|row| row == "this is a long heredoc payload with spaces"),
            "heredoc body must remain one physical row: {rows:?}"
        );
    }

    #[test]
    fn syntax_highlight_preserves_exact_text_projection() {
        let command = "cargo test --locked";
        assert_eq!(
            plain(render_bash_command_display_lines(command, 80)),
            vec![command]
        );
    }
}

//! Shared, terminal-safe rendering for tool failures and plain-text results.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::render::line_utils::{byte_offset_at_width, floor_char_boundary, truncate_str};
use crate::scrollback::types::{BlockLine, DisplayMode};
use crate::text_safety::sanitize_bounded_terminal_text;

const FAILURE_TRUNCATED_LINES: usize = 3;
const FAILURE_EXPANDED_LINES: usize = 200;
const PLAIN_TEXT_TRUNCATED_LINES: usize = 12;
const PLAIN_TEXT_EXPANDED_LINES: usize = 200;
const EMPTY_FAILURE: &str = "(tool failed without details)";
const MAX_PROBE_SEGMENT_BYTES: usize = 4 * 1024;

/// Sanitize an untrusted display field, flatten hard line breaks, and cap its
/// terminal width. The caller retains the original field for copy/search.
pub(super) fn safe_single_line(raw: &str, max_width: usize) -> String {
    let sanitized = sanitize_bounded_terminal_text(raw);
    let flattened = sanitized
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    truncate_str(&flattened, max_width)
}

/// Sanitize and cap a multi-line dynamic field. Returns the visible rows and
/// the number of additional non-empty logical rows omitted.
pub(super) fn safe_field_preview(
    raw: &str,
    max_lines: usize,
    max_width: usize,
) -> (Vec<String>, usize) {
    let sanitized = sanitize_bounded_terminal_text(raw);
    let mut total = 0usize;
    let mut visible = Vec::with_capacity(max_lines);
    for line in sanitized
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        total = total.saturating_add(1);
        if visible.len() < max_lines {
            visible.push(truncate_str(line, max_width));
        }
    }
    let omitted = total.saturating_sub(visible.len());
    (visible, omitted)
}

/// Last-resort whole-card line budget. Typed result renderers also cap entry
/// counts; this guard covers wrapped headers and future per-entry additions.
pub(super) fn cap_block_lines(lines: &mut Vec<BlockLine>, max_lines: usize, style: Style) {
    if lines.len() <= max_lines || max_lines == 0 {
        return;
    }
    let omitted = lines.len().saturating_sub(max_lines.saturating_sub(1));
    lines.truncate(max_lines.saturating_sub(1));
    lines.push(BlockLine::styled(Line::from(Span::styled(
        format!("  … ({omitted} rendered line(s) omitted; full source retained)"),
        style,
    ))));
}

/// Render a failure in a compact preview or bounded expanded form.
///
/// Even `Collapsed` mode returns one detail row. This keeps the user's manual
/// fold compact while ensuring the failure reason never disappears behind the
/// header. All backend text crosses the terminal-safety boundary here.
pub(super) fn failure_lines(
    error: &str,
    mode: DisplayMode,
    width: usize,
    style: Style,
) -> Vec<BlockLine> {
    let limit = match mode {
        DisplayMode::Collapsed => 1,
        DisplayMode::Truncated => FAILURE_TRUNCATED_LINES,
        DisplayMode::Expanded => FAILURE_EXPANDED_LINES,
    };
    let omission_hint = match mode {
        DisplayMode::Collapsed => None,
        DisplayMode::Truncated => Some("expand to view"),
        DisplayMode::Expanded => Some("render limit reached; full value retained"),
    };
    bounded_text_lines(
        error,
        width,
        style,
        limit,
        omission_hint,
        Some(EMPTY_FAILURE),
    )
}

/// Render an unstructured string result without treating it as terminal
/// control input or allowing it to build an unbounded frame.
pub(super) fn plain_text_result_lines(
    output: &str,
    mode: DisplayMode,
    width: usize,
    style: Style,
) -> Vec<BlockLine> {
    let limit = match mode {
        DisplayMode::Collapsed => 1,
        DisplayMode::Truncated => PLAIN_TEXT_TRUNCATED_LINES,
        DisplayMode::Expanded => PLAIN_TEXT_EXPANDED_LINES,
    };
    let omission_hint = match mode {
        DisplayMode::Collapsed => None,
        DisplayMode::Truncated => Some("expand to view"),
        DisplayMode::Expanded => Some("render limit reached; full value retained"),
    };
    bounded_text_lines(output, width, style, limit, omission_hint, None)
}

/// Render an untrusted text preview with caller-selected row limits.
///
/// This is the shared paint boundary for typed blocks whose compact and
/// expanded previews intentionally use different budgets. The stored value is
/// never changed; only the rows built for the current frame are sanitized and
/// bounded.
pub(super) fn bounded_text_lines(
    raw: &str,
    width: usize,
    style: Style,
    limit: usize,
    omission_hint: Option<&str>,
    empty_fallback: Option<&str>,
) -> Vec<BlockLine> {
    if limit == 0 {
        return Vec::new();
    }
    let sanitized = sanitize_bounded_terminal_text(raw);
    let trimmed = sanitized.trim();
    let text = if trimmed.is_empty() {
        let Some(fallback) = empty_fallback else {
            return Vec::new();
        };
        fallback
    } else {
        trimmed
    };

    let content_width = width.saturating_sub(2).max(1);

    // Probe only one row beyond the display limit. Each logical source line is
    // first limited to the remaining probe width, so the wrapper can never
    // materialize the complete 256 KiB field at width=1 merely for a later
    // `take(limit)`. This keeps both the source segment and the Line/Span
    // intermediate proportional to the current frame budget.
    let probe_limit = limit.saturating_add(1);
    let mut wrapped = Vec::with_capacity(probe_limit.min(256));
    let mut source_lines = text.lines().peekable();
    let mut source_omitted = false;
    while let Some(source_line) = source_lines.next() {
        if source_line.is_empty() {
            wrapped.push(Line::default());
        } else {
            let mut remainder = source_line;
            while !remainder.is_empty() {
                if wrapped.len() == probe_limit {
                    source_omitted = true;
                    break;
                }
                let (segment, consumed) = next_probe_segment(remainder, content_width);
                wrapped.push(Line::from(Span::styled(segment.to_owned(), style)));
                remainder = &remainder[consumed..];
            }
        }

        if source_omitted {
            break;
        }
        if wrapped.len() == probe_limit && source_lines.peek().is_some() {
            source_omitted = true;
            break;
        }
    }
    debug_assert!(wrapped.len() <= probe_limit);

    let overflow = source_omitted || wrapped.len() > limit;
    let reserve_hint = omission_hint.is_some() && overflow && limit > 1;
    let visible = if reserve_hint { limit - 1 } else { limit };

    let mut lines = wrapped
        .into_iter()
        .take(visible)
        .map(|mut line| {
            line.spans.insert(0, Span::styled("  ", style));
            BlockLine::styled(line)
        })
        .collect::<Vec<_>>();

    if reserve_hint {
        let hint = omission_hint.expect("reserve_hint requires omission text");
        lines.push(BlockLine::styled(Line::from(Span::styled(
            format!("  … (more content omitted; {hint})"),
            style,
        ))));
    }

    lines
}

/// Return one word-aware, UTF-8-safe source segment for the bounded row probe.
///
/// A segment never exceeds one terminal row's display budget or 4 KiB. The
/// byte cap matters for pathological zero-width combining input, where a
/// display-width-only split could otherwise retain the complete 256 KiB field
/// in a single Span. `consumed` may include whitespace at the wrap boundary,
/// matching the existing wrapper's display behavior.
fn next_probe_segment(source: &str, width: usize) -> (&str, usize) {
    debug_assert!(!source.is_empty());
    let mut end = byte_offset_at_width(source, width.max(1));
    if end == 0 {
        end = source.chars().next().map_or(source.len(), char::len_utf8);
    }
    if end > MAX_PROBE_SEGMENT_BYTES {
        end = floor_char_boundary(source, MAX_PROBE_SEGMENT_BYTES);
    }
    if end >= source.len() {
        return (source, source.len());
    }

    let candidate = &source[..end];
    let word_boundary = candidate
        .char_indices()
        .rev()
        .find_map(|(index, character)| (index > 0 && character.is_whitespace()).then_some(index));
    let segment_end = word_boundary.unwrap_or(end);
    let mut consumed = segment_end;
    while let Some(character) = source[consumed..].chars().next()
        && character.is_whitespace()
    {
        consumed = consumed.saturating_add(character.len_utf8());
    }
    (&source[..segment_end], consumed.max(segment_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[BlockLine]) -> String {
        lines
            .iter()
            .map(|line| {
                line.content
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn collapsed_failure_is_one_safe_bounded_summary_row() {
        let lines = failure_lines(
            "\n\u{1b}[31mboom\u{1b}[0m\nsecond line",
            DisplayMode::Collapsed,
            30,
            Style::default(),
        );
        let rendered = text(&lines);
        assert_eq!(lines.len(), 1);
        assert!(rendered.contains("␛[31mboom"), "{rendered:?}");
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains("second line"));
    }

    #[test]
    fn expanded_plain_text_is_safe_and_frame_bounded() {
        let output = (0..250)
            .map(|index| format!("row {index} \u{1b}]52;c;payload\u{7}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = plain_text_result_lines(&output, DisplayMode::Expanded, 80, Style::default());
        let rendered = text(&lines);
        assert_eq!(lines.len(), PLAIN_TEXT_EXPANDED_LINES);
        assert!(
            rendered.contains("render-only truncation")
                || rendered.contains("render limit reached")
        );
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
    }

    #[test]
    fn maximum_field_at_width_one_materializes_only_the_row_probe() {
        const LIMIT: usize = PLAIN_TEXT_EXPANDED_LINES;
        let input = "x".repeat(crate::text_safety::MAX_RENDER_FIELD_BYTES);
        let lines = bounded_text_lines(
            &input,
            1,
            Style::default(),
            LIMIT,
            Some("render limit reached; full value retained"),
            None,
        );

        assert_eq!(lines.len(), LIMIT);
        let span_count = lines
            .iter()
            .map(|line| line.content.spans.len())
            .sum::<usize>();
        assert!(
            span_count <= LIMIT.saturating_mul(2),
            "row probe unexpectedly built {span_count} visible spans"
        );
        let visible_bytes = lines
            .iter()
            .flat_map(|line| &line.content.spans)
            .map(|span| span.content.len())
            .sum::<usize>();
        assert!(
            visible_bytes < 2_000,
            "width-one projection retained {visible_bytes} rendered bytes"
        );
        assert!(text(&lines).contains("more content omitted"));
        assert_eq!(input.len(), crate::text_safety::MAX_RENDER_FIELD_BYTES);
    }
}

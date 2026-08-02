//! CrabCode pager terminal presentation primitives.
//!
//! Provenance:
//! - commit: `a5727c5960452e7527a154b25cb5bf00cda0545e`
//! - monorepo source revision: `30192d2eef5d91a8fff0e53957de5bd05b43398c`
//!
//! This module intentionally contains no CrabCode backend type or agent/runtime
//! type. It is the presentation side of the adapter boundary:
//! complete CrabCode SDK envelopes are retained elsewhere, while this layer
//! only turns a read-only projection into terminal cells.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Write};
use std::ops::Range;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
#[cfg(test)]
use ratatui::style::Color;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthChar as _;
use unicode_width::UnicodeWidthStr as _;

pub use crate::audited_theme::CrabCodeTheme;

/// Ratatui buffer writes that tolerate a resize racing the current frame.
///
/// Upstream's documented contract is a missed cell for one frame instead of
/// an out-of-bounds panic.  Both axes and the requested width are clipped;
/// this also closes the upstream test gap where `x` was beyond the buffer.
pub trait SafeBuf {
    fn set_line_safe(&mut self, x: u16, y: u16, line: &Line<'_>, width: u16);
    fn set_span_safe(&mut self, x: u16, y: u16, span: &Span<'_>, width: u16);
    fn set_string_safe<S: AsRef<str>>(&mut self, x: u16, y: u16, string: S, style: Style);
}

impl SafeBuf for Buffer {
    fn set_line_safe(&mut self, x: u16, y: u16, line: &Line<'_>, width: u16) {
        if y >= self.area.y && y < self.area.bottom() && x >= self.area.x && x < self.area.right() {
            self.set_line(x, y, line, width.min(self.area.right().saturating_sub(x)));
        }
    }

    fn set_span_safe(&mut self, x: u16, y: u16, span: &Span<'_>, width: u16) {
        if y >= self.area.y && y < self.area.bottom() && x >= self.area.x && x < self.area.right() {
            self.set_span(x, y, span, width.min(self.area.right().saturating_sub(x)));
        }
    }

    fn set_string_safe<S: AsRef<str>>(&mut self, x: u16, y: u16, string: S, style: Style) {
        if y >= self.area.y && y < self.area.bottom() && x >= self.area.x && x < self.area.right() {
            self.set_string(x, y, string, style);
        }
    }
}

pub fn line_to_static(line: &Line<'_>) -> Line<'static> {
    Line {
        style: line.style,
        alignment: line.alignment,
        spans: line
            .spans
            .iter()
            .map(|span| Span::styled(span.content.to_string(), span.style))
            .collect(),
    }
}

pub fn floor_char_boundary(value: &str, index: usize) -> usize {
    let mut boundary = index.min(value.len());
    while boundary > 0 && !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    boundary
}

pub fn byte_offset_at_width(value: &str, max_width: usize) -> usize {
    let mut width: usize = 0;
    for (index, grapheme) in value.grapheme_indices(true) {
        let grapheme_width = grapheme.width();
        if width.saturating_add(grapheme_width) > max_width {
            return index;
        }
        width = width.saturating_add(grapheme_width);
    }
    value.len()
}

pub fn truncate_str(value: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if value.width() <= max_width {
        return value.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let body_end = byte_offset_at_width(value, max_width - 1);
    format!("{}…", &value[..body_end])
}

/// One visible fragment of a byte-range match after its source line wraps.
///
/// Byte offsets remain the authority at the search/projection boundary, while
/// terminal painting consumes display columns.  Keeping the conversion here
/// prevents UTF-8 byte counts from leaking into cell coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSegment {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
}

/// Project a byte range in `text` onto every wrapped row it intersects.
///
/// `wrap_ranges` must be expressed as byte ranges into the same `text`.
/// Invalid/non-boundary ranges are rejected by returning no segments rather
/// than slicing unchecked input in the terminal renderer.
pub fn byte_range_to_row_cols(
    text: &str,
    wrap_ranges: &[Range<usize>],
    match_range: Range<usize>,
) -> Vec<HighlightSegment> {
    if match_range.start > match_range.end
        || match_range.end > text.len()
        || !text.is_char_boundary(match_range.start)
        || !text.is_char_boundary(match_range.end)
        || wrap_ranges.iter().any(|range| {
            range.start > range.end
                || range.end > text.len()
                || !text.is_char_boundary(range.start)
                || !text.is_char_boundary(range.end)
        })
    {
        return Vec::new();
    }

    wrap_ranges
        .iter()
        .enumerate()
        .filter_map(|(row, wrapped)| {
            let start = match_range.start.max(wrapped.start);
            let end = match_range.end.min(wrapped.end);
            (start < end).then(|| {
                let row_text = &text[wrapped.clone()];
                HighlightSegment {
                    row,
                    col_start: byte_offset_to_display_col(
                        row_text,
                        start.saturating_sub(wrapped.start),
                    ),
                    col_end: byte_offset_to_display_col(
                        row_text,
                        end.saturating_sub(wrapped.start),
                    ),
                }
            })
        })
        .collect()
}

fn byte_offset_to_display_col(text: &str, byte_offset: usize) -> usize {
    text.char_indices()
        .take_while(|(index, _)| *index < byte_offset)
        .map(|(_, character)| character.width().unwrap_or(0))
        .sum()
}

/// Clip or pad a styled line to exactly `width` terminal columns.
///
/// Clipping is on grapheme boundaries, preserving combining sequences and
/// avoiding stale terminal cells after a wide glyph.
pub fn fit_line_to_width<'a>(line: Line<'a>, width: usize) -> Line<'a> {
    let total = line
        .spans
        .iter()
        .map(|span| span.content.width())
        .sum::<usize>();
    if total == width {
        return line;
    }
    let Line {
        style,
        alignment,
        mut spans,
    } = line;
    if total < width {
        spans.push(Span::raw(" ".repeat(width - total)));
        return Line {
            style,
            alignment,
            spans,
        };
    }

    let mut output = Vec::new();
    let mut used: usize = 0;
    for span in spans {
        let span_width = span.content.width();
        if used.saturating_add(span_width) <= width {
            used = used.saturating_add(span_width);
            output.push(span);
            if used == width {
                break;
            }
            continue;
        }
        let remaining = width.saturating_sub(used);
        let mut taken = String::new();
        let mut taken_width: usize = 0;
        for grapheme in span.content.graphemes(true) {
            let grapheme_width = grapheme.width();
            if taken_width.saturating_add(grapheme_width) > remaining {
                break;
            }
            taken_width = taken_width.saturating_add(grapheme_width);
            taken.push_str(grapheme);
        }
        if !taken.is_empty() {
            output.push(Span::styled(taken, span.style));
            used = used.saturating_add(taken_width);
        }
        if used < width {
            output.push(Span::raw(" ".repeat(width - used)));
        }
        break;
    }
    Line {
        style,
        alignment,
        spans: output,
    }
}

/// Style-preserving, Unicode-width-aware greedy wrapping.
///
/// Hard newlines are retained as separate rows.  Soft wrapping prefers the
/// last whitespace that fits; a single over-wide grapheme is emitted intact
/// so the iterator always makes progress.
pub fn wrap_line(line: &Line<'_>, width: usize) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line_to_static(line)];
    }

    let flat = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    if !flat.contains('\n') && is_table_line(&flat) {
        return vec![fit_line_to_width(line_to_static(line), width)];
    }

    #[derive(Clone)]
    struct StyledGrapheme {
        text: String,
        style: Style,
        width: usize,
    }

    let mut logical_rows: Vec<Vec<StyledGrapheme>> = vec![Vec::new()];
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            if grapheme == "\n" {
                logical_rows.push(Vec::new());
            } else {
                logical_rows
                    .last_mut()
                    .expect("logical row exists")
                    .push(StyledGrapheme {
                        text: grapheme.to_string(),
                        style: span.style.patch(line.style),
                        width: grapheme.width(),
                    });
            }
        }
    }

    let mut output = Vec::new();
    for logical in logical_rows {
        if logical.is_empty() {
            output.push(Line::default().style(line.style));
            continue;
        }
        let mut prefix_len = 0;
        while prefix_len + 1 < logical.len()
            && logical[prefix_len].text == "│"
            && logical[prefix_len + 1].text == " "
        {
            prefix_len += 2;
        }
        if prefix_len == logical.len() {
            prefix_len = 0;
        }
        let prefix_width = logical[..prefix_len]
            .iter()
            .map(|grapheme| grapheme.width)
            .sum::<usize>();

        let mut start = 0;
        while start < logical.len() {
            let mut end = start;
            let continuation = start > 0 && prefix_len > 0;
            let mut row_width = if continuation { prefix_width } else { 0 };
            let mut last_break = None;
            while end < logical.len() {
                let next = row_width.saturating_add(logical[end].width);
                if next > width && end > start {
                    break;
                }
                row_width = next;
                if logical[end].text.chars().all(char::is_whitespace) {
                    last_break = Some(end);
                }
                end += 1;
                if row_width >= width {
                    break;
                }
            }
            if end < logical.len()
                && let Some(break_at) = last_break
                && break_at >= start
            {
                end = break_at;
            }
            if end == start {
                end += 1;
            }

            let mut spans: Vec<Span<'static>> = Vec::new();
            let graphemes = logical[..prefix_len]
                .iter()
                .take(if continuation { prefix_len } else { 0 })
                .chain(logical[start..end].iter());
            for grapheme in graphemes {
                if let Some(last) = spans.last_mut()
                    && last.style == grapheme.style
                {
                    last.content.to_mut().push_str(&grapheme.text);
                    continue;
                }
                spans.push(Span::styled(grapheme.text.clone(), grapheme.style));
            }
            while spans
                .last()
                .is_some_and(|span| span.content.chars().all(char::is_whitespace))
            {
                spans.pop();
            }
            output.push(Line::from(spans).style(line.style));
            start = end;
            while start < logical.len() && logical[start].text.chars().all(char::is_whitespace) {
                start += 1;
            }
        }
    }
    output
}

/// Table rows and box-drawing borders own a fixed terminal row. Wrapping them
/// would destroy column alignment; the caller clips/pads them to the viewport.
fn is_table_line(text: &str) -> bool {
    let mut characters = text.chars();
    match characters.next() {
        Some(character)
            if ('\u{2500}'..='\u{257f}').contains(&character)
                && character != '\u{2502}'
                && character != '\u{2503}' =>
        {
            true
        }
        Some('\u{2502}') => {
            let mut in_prefix = true;
            for character in characters {
                if in_prefix && matches!(character, '\u{2502}' | ' ') {
                    continue;
                }
                in_prefix = false;
                if character == '\u{2502}' {
                    return true;
                }
            }
            false
        }
        Some('|') => true,
        _ => false,
    }
}

pub const SCROLLBAR_TOTAL_COLS: u16 = 2;

pub fn split_area_for_scrollbar(area: Rect) -> (Rect, Option<Rect>) {
    if area.width <= SCROLLBAR_TOTAL_COLS {
        return (area, None);
    }
    (
        Rect::new(
            area.x,
            area.y,
            area.width.saturating_sub(SCROLLBAR_TOTAL_COLS),
            area.height,
        ),
        Some(Rect::new(
            area.right().saturating_sub(1),
            area.y,
            1,
            area.height,
        )),
    )
}

pub const fn needs_scrollbar(total_lines: usize, viewport_lines: usize) -> bool {
    total_lines > viewport_lines
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrollbarClickResult {
    Top,
    Bottom,
    Offset(usize),
}

pub fn scrollbar_click_to_offset(
    cell_index: u16,
    track_cells: u16,
    total_lines: usize,
    viewport_lines: usize,
) -> ScrollbarClickResult {
    if track_cells == 0 || cell_index == 0 {
        return ScrollbarClickResult::Top;
    }
    if cell_index >= track_cells.saturating_sub(1) {
        return ScrollbarClickResult::Bottom;
    }
    let max_offset = total_lines.saturating_sub(viewport_lines);
    let denominator = usize::from(track_cells.saturating_sub(1)).max(1);
    ScrollbarClickResult::Offset(
        usize::from(cell_index)
            .saturating_mul(max_offset)
            .saturating_add(denominator / 2)
            / denominator,
    )
}

/// Scroll offset plus the explicit follow-live bit used by the TUI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollState {
    offset: usize,
    following: bool,
}

impl Default for ScrollState {
    fn default() -> Self {
        Self {
            offset: 0,
            following: true,
        }
    }
}

impl ScrollState {
    pub const fn offset(self) -> usize {
        self.offset
    }

    pub const fn is_following(self) -> bool {
        self.following
    }

    pub fn clamp(&mut self, total_lines: usize, viewport_lines: usize) {
        let bottom = total_lines.saturating_sub(viewport_lines);
        if self.following {
            self.offset = bottom;
        } else {
            self.offset = self.offset.min(bottom);
            if self.offset == bottom {
                self.following = true;
            }
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
        self.following = false;
    }

    pub fn scroll_down(&mut self, lines: usize, total_lines: usize, viewport_lines: usize) {
        let bottom = total_lines.saturating_sub(viewport_lines);
        let before = self.offset;
        self.offset = self.offset.saturating_add(lines).min(bottom);
        // Landing at the bottom remains a detached reading position. Follow
        // resumes only when a subsequent, non-zero scroll-down is already
        // fully clamped there: that zero-displacement overscroll is the
        // explicit user gesture used by the fixed renderer lifecycle.
        if lines > 0 && self.offset == before && self.offset >= bottom {
            self.following = true;
        }
    }

    pub fn follow(&mut self, total_lines: usize, viewport_lines: usize) {
        self.following = true;
        self.offset = total_lines.saturating_sub(viewport_lines);
    }

    pub fn set_offset(&mut self, offset: usize, total_lines: usize, viewport_lines: usize) {
        let bottom = total_lines.saturating_sub(viewport_lines);
        self.offset = offset.min(bottom);
        self.following = self.offset == bottom;
    }

    /// Snap a navigation target to the viewport top without enabling live
    /// follow when that target also happens to be the current bottom.
    ///
    /// Response-anchor navigation is a deliberate reading position.  It must
    /// remain detached from subsequent streaming output until the user
    /// explicitly returns to the bottom.
    pub fn snap_to_offset(&mut self, offset: usize, total_lines: usize, viewport_lines: usize) {
        let bottom = total_lines.saturating_sub(viewport_lines);
        self.offset = offset.min(bottom);
        self.following = false;
    }
}

/// A normalized key chord used consistently across CrabCode TUI contexts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyShortcut {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyShortcut {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        let mut shortcut = Self { code, modifiers };
        if let KeyCode::Char(character) = shortcut.code {
            if character.is_ascii_uppercase() {
                shortcut.modifiers.insert(KeyModifiers::SHIFT);
            } else if shortcut.modifiers.contains(KeyModifiers::SHIFT) {
                shortcut.code = KeyCode::Char(character.to_ascii_uppercase());
            }
        }
        shortcut
    }

    pub fn matches(self, event: &KeyEvent) -> bool {
        event.kind != KeyEventKind::Release && self == Self::new(event.code, event.modifiers)
    }

    pub fn display_pretty(self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::SUPER) {
            parts.push(if cfg!(target_os = "macos") {
                "Cmd".to_string()
            } else {
                "Super".to_string()
            });
        }
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl".to_string());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push(if cfg!(target_os = "macos") {
                "Opt".to_string()
            } else {
                "Alt".to_string()
            });
        }
        let has_shift = self.modifiers.contains(KeyModifiers::SHIFT);
        if has_shift || self.code == KeyCode::BackTab {
            parts.push("Shift".to_string());
        }
        parts.push(match self.code {
            KeyCode::Char(' ') => "Space".to_string(),
            KeyCode::Char(character) => character.to_ascii_lowercase().to_string(),
            KeyCode::Enter => "Enter".to_string(),
            KeyCode::Esc => "Esc".to_string(),
            KeyCode::Tab | KeyCode::BackTab => "Tab".to_string(),
            KeyCode::Backspace => "Backspace".to_string(),
            KeyCode::Delete => "Delete".to_string(),
            KeyCode::Up => "↑".to_string(),
            KeyCode::Down => "↓".to_string(),
            KeyCode::Left => "←".to_string(),
            KeyCode::Right => "→".to_string(),
            KeyCode::Home => "Home".to_string(),
            KeyCode::End => "End".to_string(),
            KeyCode::PageUp => "Page Up".to_string(),
            KeyCode::PageDown => "Page Down".to_string(),
            KeyCode::F(number) => format!("F{number}"),
            other => format!("{other:?}"),
        });
        parts.join("+")
    }
}

impl fmt::Display for KeyShortcut {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.display_pretty())
    }
}

pub fn is_shift_tab(event: &KeyEvent) -> bool {
    [
        KeyShortcut::new(KeyCode::BackTab, KeyModifiers::NONE),
        KeyShortcut::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        KeyShortcut::new(KeyCode::Tab, KeyModifiers::SHIFT),
    ]
    .iter()
    .any(|shortcut| shortcut.matches(event))
}

pub fn is_text_input_key(event: &KeyEvent) -> bool {
    matches!(event.code, KeyCode::Char(_))
        && (event.modifiers.is_empty()
            || event.modifiers == KeyModifiers::SHIFT
            || crate::tui_render::is_altgr(event.modifiers))
}

#[cfg(target_os = "windows")]
pub fn is_altgr(modifiers: KeyModifiers) -> bool {
    modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
}

#[cfg(not(target_os = "windows"))]
pub const fn is_altgr(_modifiers: KeyModifiers) -> bool {
    false
}

/// OSC 8 target after stripping every terminal control character.
pub fn sanitize_osc8_target(target: &str) -> Cow<'_, str> {
    if target
        .chars()
        .any(|character| character.is_control() || is_bidi_control(character))
    {
        Cow::Owned(
            target
                .chars()
                .filter(|character| !character.is_control() && !is_bidi_control(*character))
                .collect(),
        )
    } else {
        Cow::Borrowed(target)
    }
}

const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

pub fn write_osc8_open(writer: &mut impl Write, target: &str, id: Option<u32>) -> io::Result<()> {
    let target = sanitize_osc8_target(target);
    match id {
        Some(id) => write!(writer, "\x1b]8;id={id};{target}\x07"),
        None => write!(writer, "\x1b]8;;{target}\x07"),
    }
}

pub fn write_osc8_close(writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b"\x1b]8;;\x07")
}

/// Bounded FIFO used between state projection and terminal presentation.
///
/// It never silently discards an event. Producers receive the original value
/// back when either the item count or byte budget would be exceeded, allowing
/// runtime backpressure to propagate instead of corrupting display order.
#[derive(Debug)]
pub struct PresentationQueue<T> {
    entries: VecDeque<(usize, T)>,
    max_entries: usize,
    max_bytes: usize,
    bytes: usize,
}

impl<T> PresentationQueue<T> {
    pub fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            max_entries,
            max_bytes,
            bytes: 0,
        }
    }

    pub fn try_push(&mut self, value: T, encoded_bytes: usize) -> Result<(), T> {
        if self.entries.len() >= self.max_entries
            || encoded_bytes > self.max_bytes.saturating_sub(self.bytes)
        {
            return Err(value);
        }
        self.bytes = self.bytes.saturating_add(encoded_bytes);
        self.entries.push_back((encoded_bytes, value));
        Ok(())
    }

    pub fn pop(&mut self) -> Option<T> {
        self.entries.pop_front().map(|(encoded_bytes, value)| {
            self.bytes = self.bytes.saturating_sub(encoded_bytes);
            value
        })
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub const fn bytes(&self) -> usize {
        self.bytes
    }
}

/// Canonical style helpers used by the Crab projection.
pub mod styles {
    use super::*;

    pub fn assistant() -> Style {
        Style::default().fg(CrabCodeTheme::NIGHT.accent_assistant)
    }

    pub fn user() -> Style {
        Style::default()
            .fg(CrabCodeTheme::NIGHT.accent_user)
            .add_modifier(Modifier::BOLD)
    }

    pub fn thinking() -> Style {
        Style::default()
            .fg(CrabCodeTheme::NIGHT.accent_thinking)
            .add_modifier(Modifier::ITALIC)
    }

    pub fn tool() -> Style {
        Style::default().fg(CrabCodeTheme::NIGHT.accent_tool)
    }

    pub fn error() -> Style {
        Style::default()
            .fg(CrabCodeTheme::NIGHT.accent_error)
            .add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crabcode_night_palette_combines_direct_product_roles_with_fixed_mother_roles() {
        let theme = CrabCodeTheme::NIGHT;
        assert_eq!(theme.bg_base, Color::Rgb(20, 20, 20));
        assert_eq!(theme.bg_terminal, Color::Rgb(10, 10, 10));
        assert_eq!(theme.text_primary, Color::Rgb(255, 255, 255));
        assert_eq!(theme.accent_assistant, Color::Rgb(215, 119, 87));
        assert_eq!(theme.link, Color::Rgb(122, 166, 218));
    }

    #[test]
    fn safe_buffer_skips_both_out_of_bounds_axes() {
        let mut buffer = Buffer::empty(Rect::new(10, 20, 4, 2));
        buffer.set_string_safe(9, 20, "left", Style::default());
        buffer.set_string_safe(14, 20, "right", Style::default());
        buffer.set_string_safe(10, 19, "above", Style::default());
        buffer.set_string_safe(10, 22, "below", Style::default());
        assert!(buffer.content.iter().all(|cell| cell.symbol() == " "));
        buffer.set_string_safe(10, 20, "ok", Style::default());
        assert_eq!(buffer[(10, 20)].symbol(), "o");
    }

    #[test]
    fn width_clipping_never_splits_combining_or_wide_graphemes() {
        let line = Line::from(vec![
            Span::styled("a", Style::default().fg(Color::Red)),
            Span::styled("e\u{301}界z", Style::default().fg(Color::Blue)),
        ]);
        let fitted = fit_line_to_width(line, 3);
        assert_eq!(
            fitted
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>(),
            "ae\u{301} "
        );
        assert_eq!(fitted.width(), 3);
        assert_eq!(byte_offset_at_width("e\u{301}界x", 1), "e\u{301}".len());
        assert_eq!(truncate_str("e\u{301}界x", 2), "e\u{301}…");
        assert_eq!(truncate_str("界x", 3), "界x");
        assert_eq!(truncate_str("界xy", 3), "界…");
    }

    #[test]
    fn wrapping_preserves_styles_and_hard_breaks() {
        let red = Style::default().fg(Color::Red);
        let blue = Style::default().fg(Color::Blue);
        let line = Line::from(vec![
            Span::styled("hello ", red),
            Span::styled("world\n界界", blue),
        ]);
        let rows = wrap_line(&line, 6);
        let plain = rows
            .iter()
            .map(|row| {
                row.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert_eq!(plain, ["hello", "world", "界界"]);
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(rows[1].spans[0].style.fg, Some(Color::Blue));
    }

    #[test]
    fn simple_styled_wrap_preserves_styles() {
        let line = Line::from(vec![
            Span::styled("hello ", Style::default().fg(Color::Red)),
            Span::raw("world"),
        ]);
        let rows = wrap_line(&line, 6);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].spans[0].content.as_ref(), "hello");
        assert_eq!(rows[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(rows[1].spans[0].content.as_ref(), "world");
        assert_eq!(rows[1].spans[0].style.fg, None);
    }

    #[test]
    fn real_markdown_link_underline_does_not_leak_after_wrap() {
        let line = Line::from(vec![
            Span::raw("uses "),
            Span::styled(
                "Buildkite",
                Style::default().add_modifier(Modifier::UNDERLINED),
            ),
            Span::raw(" here"),
        ]);
        let rows = wrap_line(&line, 8);
        let underlined = rows
            .iter()
            .flat_map(|row| &row.spans)
            .filter(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(underlined, "Buildkite");
        for span in rows.iter().flat_map(|row| &row.spans) {
            if span.content.contains("uses") || span.content.contains("here") {
                assert!(!span.style.add_modifier.contains(Modifier::UNDERLINED));
            }
        }
    }

    #[test]
    fn wide_unicode_wraps_by_display_width() {
        let rows = wrap_line(&Line::from("😀😀😀"), 4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].spans[0].content.as_ref(), "😀😀");
        assert_eq!(rows[1].spans[0].content.as_ref(), "😀");
    }

    #[test]
    fn highlight_match_spans_three_rows() {
        let text = "abcdefghijklmnop";
        let ranges = vec![0..5, 5..10, 10..15, 15..16];
        assert_eq!(
            byte_range_to_row_cols(text, &ranges, 3..12),
            vec![
                HighlightSegment {
                    row: 0,
                    col_start: 3,
                    col_end: 5,
                },
                HighlightSegment {
                    row: 1,
                    col_start: 0,
                    col_end: 5,
                },
                HighlightSegment {
                    row: 2,
                    col_start: 0,
                    col_end: 2,
                },
            ],
        );
    }

    #[test]
    fn highlight_projection_uses_display_columns_and_rejects_invalid_ranges() {
        let text = "a界bc";
        assert_eq!(
            byte_range_to_row_cols(text, &[0.."a界".len(), "a界".len()..text.len()], 1..5),
            vec![
                HighlightSegment {
                    row: 0,
                    col_start: 1,
                    col_end: 3,
                },
                HighlightSegment {
                    row: 1,
                    col_start: 0,
                    col_end: 1,
                },
            ],
        );
        let invalid_line_boundary = 0..2;
        assert!(
            byte_range_to_row_cols(text, std::slice::from_ref(&invalid_line_boundary), 0..1,)
                .is_empty()
        );
        let full_line = 0..text.len();
        let reversed_selection = std::ops::Range { start: 4, end: 2 };
        assert!(
            byte_range_to_row_cols(text, std::slice::from_ref(&full_line), reversed_selection,)
                .is_empty()
        );
    }

    #[test]
    fn inline_segment_uses_terminal_columns_for_unicode_width() {
        let input = Line::from("hello 你好");
        assert_eq!(
            wrap_line(&input, 10).len(),
            1,
            "six ASCII columns plus two double-width CJK scalars must exactly fit ten columns"
        );
        assert_eq!(
            wrap_line(&input, 9).len(),
            2,
            "the same content must wrap when only nine terminal columns are available"
        );
    }

    #[test]
    fn table_line_box_drawing_not_wrapped() {
        let source = "│ Column A │ Column B │ Some very long content here │";
        let rows = wrap_line(&Line::from(source), 10);
        assert_eq!(rows.len(), 1);
        let rendered = rows[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(rendered.width(), 10);
        assert!(source.starts_with(rendered.trim_end()));
    }

    #[test]
    fn blockquote_line_wraps_with_prefix() {
        let rows = wrap_line(
            &Line::from("│ This is a blockquote that should wrap to multiple lines when narrow"),
            30,
        );
        assert!(rows.len() > 1);
        for row in rows {
            let text = row
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(text.starts_with("│ "), "{text:?}");
        }
    }

    #[test]
    fn scroll_follow_reengages_only_after_bottom_overscroll() {
        let mut scroll = ScrollState::default();
        scroll.clamp(100, 20);
        assert_eq!(scroll.offset(), 80);
        scroll.scroll_up(10);
        assert_eq!(scroll.offset(), 70);
        assert!(!scroll.is_following());
        scroll.scroll_down(9, 100, 20);
        assert_eq!(scroll.offset(), 79);
        assert!(!scroll.is_following());
        scroll.scroll_down(1, 100, 20);
        assert_eq!(scroll.offset(), 80);
        assert!(!scroll.is_following());
        scroll.scroll_down(1, 100, 20);
        assert_eq!(scroll.offset(), 80);
        assert!(scroll.is_following());
    }

    #[test]
    fn response_snap_stays_detached_even_when_target_clamps_to_bottom() {
        let mut scroll = ScrollState::default();
        scroll.snap_to_offset(90, 100, 20);
        assert_eq!(scroll.offset(), 80);
        assert!(!scroll.is_following());
    }

    #[test]
    fn shift_tab_and_case_normalization_match_all_encodings() {
        for event in [
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
        ] {
            assert!(is_shift_tab(&event));
        }
        let shortcut = KeyShortcut::new(KeyCode::Char('g'), KeyModifiers::SHIFT);
        assert!(shortcut.matches(&KeyEvent::new(KeyCode::Char('G'), KeyModifiers::NONE)));
    }

    #[test]
    fn osc8_strips_control_injection_and_balances_sequences() {
        let mut output = Vec::new();
        write_osc8_open(
            &mut output,
            "https://example.test/\u{1b}]8;;evil\u{7}",
            Some(7),
        )
        .expect("open");
        output.extend_from_slice(b"label");
        write_osc8_close(&mut output).expect("close");
        let output = String::from_utf8(output).expect("utf8");
        assert_eq!(
            output,
            "\u{1b}]8;id=7;https://example.test/]8;;evil\u{7}label\u{1b}]8;;\u{7}"
        );
        assert_eq!(
            sanitize_osc8_target("https://safe.test/\u{202e}gpj.exe"),
            "https://safe.test/gpj.exe"
        );
    }

    #[test]
    fn presentation_queue_applies_count_and_byte_backpressure_without_drop() {
        let mut queue = PresentationQueue::new(2, 5);
        assert_eq!(queue.try_push("ab", 2), Ok(()));
        assert_eq!(queue.try_push("cde", 3), Ok(()));
        assert_eq!(queue.try_push("retained", 1), Err("retained"));
        assert_eq!(queue.len(), 2);
        assert_eq!(queue.bytes(), 5);
        assert_eq!(queue.pop(), Some("ab"));
        assert_eq!(queue.try_push("f", 1), Ok(()));
        assert_eq!(queue.pop(), Some("cde"));
        assert_eq!(queue.pop(), Some("f"));
        assert!(queue.is_empty());
    }
}

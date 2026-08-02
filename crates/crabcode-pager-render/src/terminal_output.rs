//! CrabCode pager's bounded, line-oriented terminal-output emulator.
//!
//! This is a presentation primitive, not a shell/runtime. It accepts output
//! bytes already produced by CrabCode tools and resolves SGR, cursor movement,
//! erase operations, carriage returns and tabs into inert Ratatui lines.
//!
//! The implementation is pinned to audited upstream presentation behavior.
//! Color-capability quantization is deferred to CrabCode's terminal theme
//! boundary.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;
use vte::{Params, Parser, Perform};

const MAX_ROWS: usize = 50_000;
const MAX_COLS: usize = 8_192;

#[derive(Debug, Clone, PartialEq)]
pub struct RenderedTerminalLine {
    pub line: Line<'static>,
    pub plain: String,
}

pub fn render_terminal_lines(raw: &str, base: Style) -> Vec<RenderedTerminalLine> {
    if raw.is_empty() {
        return Vec::new();
    }
    let mut sink = TerminalSink::new(base);
    let mut parser = Parser::new();
    parser.advance(&mut sink, raw.as_bytes());
    sink.finish()
}

pub fn render_terminal_plain(raw: &str) -> String {
    render_terminal_lines(raw, Style::default())
        .into_iter()
        .map(|line| line.plain)
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone)]
struct Cell {
    text: String,
    style: Style,
    width: usize,
    continuation: bool,
}

impl Cell {
    fn blank(style: Style) -> Self {
        Self {
            text: " ".to_string(),
            style,
            width: 1,
            continuation: false,
        }
    }
}

struct TerminalSink {
    base: Style,
    current: Style,
    rows: Vec<Vec<Cell>>,
    row: usize,
    column: usize,
}

impl TerminalSink {
    fn new(base: Style) -> Self {
        Self {
            base,
            current: base,
            rows: vec![Vec::new()],
            row: 0,
            column: 0,
        }
    }

    fn ensure_row(&mut self) {
        if self.row >= MAX_ROWS {
            self.row = MAX_ROWS - 1;
        }
        while self.rows.len() <= self.row {
            self.rows.push(Vec::new());
        }
    }

    fn put(&mut self, character: char) {
        if self.column >= MAX_COLS || self.append_to_previous_grapheme(character) {
            return;
        }
        self.ensure_row();
        let width = character.to_string().width();
        if width == 0 || width > MAX_COLS.saturating_sub(self.column) {
            return;
        }
        let blank = Cell::blank(self.base);
        let row = &mut self.rows[self.row];
        let end = self.column.saturating_add(width);
        if end > row.len() {
            row.resize(end, blank.clone());
        }
        for column in self.column..end {
            clear_glyph_at(row, column, &blank);
        }
        if end > row.len() {
            row.resize(end, blank.clone());
        }
        row[self.column] = Cell {
            text: character.to_string(),
            style: self.current,
            width,
            continuation: false,
        };
        for cell in row.iter_mut().take(end).skip(self.column + 1) {
            *cell = Cell {
                text: String::new(),
                style: self.current,
                width: 0,
                continuation: true,
            };
        }
        self.column = end;
    }

    /// VTE delivers Unicode scalars, while a terminal cursor advances by
    /// extended grapheme clusters. Combining marks, variation selectors,
    /// emoji ZWJ sequences, and regional-indicator pairs must therefore amend
    /// the preceding cell instead of consuming another column.
    fn append_to_previous_grapheme(&mut self, character: char) -> bool {
        if self.column == 0 {
            return false;
        }
        self.ensure_row();
        let row = &mut self.rows[self.row];
        if row.is_empty() {
            return false;
        }
        let previous_column = self.column.saturating_sub(1).min(row.len() - 1);
        let Some(owner) = glyph_owner(row, previous_column) else {
            return false;
        };
        let mut combined = row[owner].text.clone();
        combined.push(character);
        if combined.graphemes(true).count() != 1 {
            return false;
        }

        let old_width = row[owner].width.max(1);
        let new_width = combined.width().max(1);
        let new_end = owner.saturating_add(new_width);
        if new_end > MAX_COLS {
            return true;
        }
        let blank = Cell::blank(self.base);
        if new_end > row.len() {
            row.resize(new_end, blank.clone());
        }
        if new_width > old_width {
            for column in owner + old_width..new_end {
                clear_glyph_at(row, column, &blank);
            }
        } else if new_width < old_width {
            for column in new_end..owner + old_width {
                if column < row.len() {
                    row[column] = blank.clone();
                }
            }
        }
        row[owner].text = combined;
        row[owner].style = self.current;
        row[owner].width = new_width;
        row[owner].continuation = false;
        for cell in row.iter_mut().take(new_end).skip(owner + 1) {
            *cell = Cell {
                text: String::new(),
                style: self.current,
                width: 0,
                continuation: true,
            };
        }
        self.column = new_end;
        true
    }

    fn newline(&mut self) {
        self.row = self.row.saturating_add(1);
        self.column = 0;
        self.ensure_row();
    }

    fn erase_line(&mut self, mode: u16) {
        self.ensure_row();
        let blank = Cell::blank(self.base);
        let row = &mut self.rows[self.row];
        match mode {
            0 => {
                let boundary = glyph_boundary_at_or_before(row, self.column.min(row.len()));
                row.truncate(boundary);
            }
            1 => {
                let mut end = self.column.saturating_add(1).min(row.len());
                if end > 0
                    && let Some(owner) = glyph_owner(row, end - 1)
                {
                    end = owner.saturating_add(row[owner].width.max(1)).min(row.len());
                }
                row[..end].fill(blank);
            }
            2 => row.clear(),
            _ => {}
        }
    }

    fn erase_display(&mut self, mode: u16) {
        match mode {
            0 => {
                self.ensure_row();
                let row = &mut self.rows[self.row];
                let boundary = glyph_boundary_at_or_before(row, self.column.min(row.len()));
                row.truncate(boundary);
                self.rows.truncate(self.row + 1);
            }
            1 => {
                self.ensure_row();
                for row in &mut self.rows[..self.row] {
                    row.clear();
                }
                let blank = Cell::blank(self.base);
                let row = &mut self.rows[self.row];
                let mut end = self.column.saturating_add(1).min(row.len());
                if end > 0
                    && let Some(owner) = glyph_owner(row, end - 1)
                {
                    end = owner.saturating_add(row[owner].width.max(1)).min(row.len());
                }
                row[..end].fill(blank);
            }
            2 | 3 => {
                self.rows.clear();
                self.rows.push(Vec::new());
                self.row = 0;
                self.column = 0;
            }
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.current = self.base;
            return;
        }
        let groups = params.iter().collect::<Vec<_>>();
        let mut index = 0;
        while index < groups.len() {
            let code = groups[index].first().copied().unwrap_or(0);
            match code {
                0 => self.current = self.base,
                1 => self.current = self.current.add_modifier(Modifier::BOLD),
                2 => self.current = self.current.add_modifier(Modifier::DIM),
                3 => self.current = self.current.add_modifier(Modifier::ITALIC),
                4 => self.current = self.current.add_modifier(Modifier::UNDERLINED),
                7 => self.current = self.current.add_modifier(Modifier::REVERSED),
                9 => self.current = self.current.add_modifier(Modifier::CROSSED_OUT),
                22 => {
                    self.current = self.current.remove_modifier(Modifier::BOLD | Modifier::DIM);
                }
                23 => self.current = self.current.remove_modifier(Modifier::ITALIC),
                24 => self.current = self.current.remove_modifier(Modifier::UNDERLINED),
                27 => self.current = self.current.remove_modifier(Modifier::REVERSED),
                29 => self.current = self.current.remove_modifier(Modifier::CROSSED_OUT),
                30..=37 => self.current.fg = Some(ansi16(code - 30)),
                39 => self.current.fg = self.base.fg,
                40..=47 => self.current.bg = Some(ansi16(code - 40)),
                49 => self.current.bg = self.base.bg,
                90..=97 => self.current.fg = Some(ansi16_bright(code - 90)),
                100..=107 => self.current.bg = Some(ansi16_bright(code - 100)),
                38 => {
                    if let Some(color) = extended_color(&groups, &mut index) {
                        self.current.fg = Some(color);
                    }
                }
                48 => {
                    if let Some(color) = extended_color(&groups, &mut index) {
                        self.current.bg = Some(color);
                    }
                }
                _ => {}
            }
            index += 1;
        }
    }

    fn finish(mut self) -> Vec<RenderedTerminalLine> {
        if self.rows.last().is_some_and(Vec::is_empty) {
            self.rows.pop();
        }
        let base = self.base;
        self.rows
            .into_iter()
            .map(|row| row_to_line(row, base))
            .collect()
    }
}

impl Perform for TerminalSink {
    fn print(&mut self, character: char) {
        self.put(character);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | 0x0b | 0x0c => self.newline(),
            b'\r' => self.column = 0,
            b'\t' => self.column = (self.column / 8 + 1).saturating_mul(8).min(MAX_COLS),
            0x08 => self.column = self.column.saturating_sub(1),
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        match action {
            'm' => self.apply_sgr(params),
            'K' => self.erase_line(first_param(params, 0)),
            'J' => self.erase_display(first_param(params, 0)),
            'A' => {
                self.row = self.row.saturating_sub(usize::from(first_param(params, 1)));
            }
            'B' => {
                self.row = self
                    .row
                    .saturating_add(usize::from(first_param(params, 1)))
                    .min(self.rows.len().saturating_sub(1));
            }
            'C' => {
                self.column = self
                    .column
                    .saturating_add(usize::from(first_param(params, 1)))
                    .min(MAX_COLS);
            }
            'D' => {
                self.column = self
                    .column
                    .saturating_sub(usize::from(first_param(params, 1)));
            }
            'G' => {
                self.column = usize::from(first_param(params, 1))
                    .saturating_sub(1)
                    .min(MAX_COLS);
            }
            _ => {}
        }
    }
}

fn first_param(params: &Params, default: u16) -> u16 {
    nth_param(params, 0, default)
}

fn nth_param(params: &Params, index: usize, default: u16) -> u16 {
    match params
        .iter()
        .nth(index)
        .and_then(|parameter| parameter.first().copied())
    {
        Some(0) | None => default,
        Some(value) => value,
    }
}

fn ansi16(index: u16) -> Color {
    match index {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        _ => Color::Gray,
    }
}

fn ansi16_bright(index: u16) -> Color {
    match index {
        0 => Color::DarkGray,
        1 => Color::LightRed,
        2 => Color::LightGreen,
        3 => Color::LightYellow,
        4 => Color::LightBlue,
        5 => Color::LightMagenta,
        6 => Color::LightCyan,
        _ => Color::White,
    }
}

fn extended_color(groups: &[&[u16]], index: &mut usize) -> Option<Color> {
    let current = groups[*index];
    if current.len() >= 2 {
        return parse_extended_color(&current[1..]);
    }
    match groups
        .get(*index + 1)
        .and_then(|parameter| parameter.first().copied())?
    {
        5 => {
            let palette = groups
                .get(*index + 2)
                .and_then(|parameter| parameter.first().copied())?;
            *index += 2;
            Some(Color::Indexed(u8::try_from(palette).ok()?))
        }
        2 => {
            let red = groups
                .get(*index + 2)
                .and_then(|parameter| parameter.first().copied())?;
            let green = groups
                .get(*index + 3)
                .and_then(|parameter| parameter.first().copied())?;
            let blue = groups
                .get(*index + 4)
                .and_then(|parameter| parameter.first().copied())?;
            *index += 4;
            Some(Color::Rgb(
                u8::try_from(red).ok()?,
                u8::try_from(green).ok()?,
                u8::try_from(blue).ok()?,
            ))
        }
        _ => None,
    }
}

fn parse_extended_color(parameters: &[u16]) -> Option<Color> {
    match parameters.first().copied()? {
        5 => parameters
            .get(1)
            .and_then(|palette| u8::try_from(*palette).ok())
            .map(Color::Indexed),
        2 => {
            let values = &parameters[1..];
            let (red, green, blue) = match values.len() {
                3 => (values[0], values[1], values[2]),
                length if length >= 4 => {
                    (values[length - 3], values[length - 2], values[length - 1])
                }
                _ => return None,
            };
            Some(Color::Rgb(
                u8::try_from(red).ok()?,
                u8::try_from(green).ok()?,
                u8::try_from(blue).ok()?,
            ))
        }
        _ => None,
    }
}

fn row_to_line(cells: Vec<Cell>, base: Style) -> RenderedTerminalLine {
    let mut end = cells.len();
    while end > 0
        && (cells[end - 1].continuation
            || (cells[end - 1].text == " " && cells[end - 1].style == base))
    {
        end -= 1;
    }
    let cells = &cells[..end];
    if cells.is_empty() {
        return RenderedTerminalLine {
            line: Line::default(),
            plain: String::new(),
        };
    }
    let plain = cells
        .iter()
        .filter(|cell| !cell.continuation)
        .map(|cell| cell.text.as_str())
        .collect::<String>();
    let mut spans = Vec::new();
    let mut text = String::new();
    let mut style = cells
        .iter()
        .find(|cell| !cell.continuation)
        .map_or(base, |cell| cell.style);
    for cell in cells.iter().filter(|cell| !cell.continuation) {
        if cell.style != style {
            spans.push(Span::styled(std::mem::take(&mut text), style));
            style = cell.style;
        }
        text.push_str(&cell.text);
    }
    spans.push(Span::styled(text, style));
    RenderedTerminalLine {
        line: Line::from(spans),
        plain,
    }
}

fn glyph_owner(row: &[Cell], column: usize) -> Option<usize> {
    if column >= row.len() {
        return None;
    }
    let mut owner = column;
    while owner > 0 && row[owner].continuation {
        owner -= 1;
    }
    (!row[owner].continuation).then_some(owner)
}

fn glyph_boundary_at_or_before(row: &[Cell], boundary: usize) -> usize {
    if boundary >= row.len() || boundary == 0 {
        return boundary.min(row.len());
    }
    if row[boundary].continuation {
        glyph_owner(row, boundary).unwrap_or(boundary)
    } else {
        boundary
    }
}

fn clear_glyph_at(row: &mut [Cell], column: usize, blank: &Cell) {
    let Some(owner) = glyph_owner(row, column) else {
        return;
    };
    let end = owner.saturating_add(row[owner].width.max(1)).min(row.len());
    row[owner..end].fill(blank.clone());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(raw: &str) -> String {
        render_terminal_plain(raw)
    }

    fn lines(raw: &str) -> Vec<String> {
        render_terminal_lines(raw, Style::default())
            .into_iter()
            .map(|line| line.plain)
            .collect()
    }

    #[test]
    fn strips_sgr_to_plain_text() {
        assert_eq!(plain("\x1b[1m\x1b[36mbazel\x1b[0m"), "bazel");
    }

    #[test]
    fn carriage_return_overwrites_in_place() {
        assert_eq!(plain("aaaa\rbb"), "bbaa");
    }

    #[test]
    fn progress_bar_collapses_to_final_state() {
        assert_eq!(plain("10%\r50%\r100%\n"), "100%");
    }

    #[test]
    fn newline_and_trailing_newline_match_terminal_rows() {
        assert_eq!(lines("a\nb"), vec!["a", "b"]);
        assert_eq!(lines("a\n"), vec!["a"]);
        assert_eq!(lines("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn windows_crlf_line_endings() {
        assert_eq!(lines("a\r\nb\r\nc\r\n"), vec!["a", "b", "c"]);
    }

    #[test]
    fn cursor_and_erase_sequences_resolve() {
        assert_eq!(lines("line1\nline2\x1b[A\rXX\x1b[K"), vec!["XX", "line2"]);
        assert_eq!(lines("loading 99%\x1b[2K\rdone\n"), vec!["done"]);
    }

    #[test]
    fn tab_advances_to_next_stop() {
        assert_eq!(plain("a\tb"), "a       b");
    }

    #[test]
    fn malformed_escape_does_not_panic() {
        assert_eq!(plain("\x1b[38;5mhi"), "hi");
        assert!(plain("\x1b[99999999999m\x1b[mok").contains("ok"));
    }

    #[test]
    fn ignores_dec_private_modes_and_osc() {
        assert_eq!(
            plain("\x1b[?25l\x1b]0;window title\x07hello\x1b[?1049h world\x1b[?25h"),
            "hello world"
        );
    }

    #[test]
    fn unsupported_cursor_and_conceal_sequences_are_inert() {
        assert_eq!(plain("\x1b7\x1b[10;5Hkept\x1b8"), "kept");
        let conceal = render_terminal_lines("\x1b[8mstill visible", Style::default());
        assert!(
            !conceal[0].line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::HIDDEN),
            "untrusted tool output must not conceal audit-relevant text"
        );
    }

    #[test]
    fn sgr_preserves_style_runs_and_plain_projection() {
        let rendered = render_terminal_lines("plain \x1b[31mred\x1b[0m", Style::default());
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].plain, "plain red");
        assert_eq!(rendered[0].line.spans.len(), 2);
        assert_eq!(rendered[0].line.spans[1].style.fg, Some(Color::Red));
    }

    #[test]
    fn extended_colors_support_indexed_and_truecolor_forms() {
        assert_eq!(parse_extended_color(&[5, 42]), Some(Color::Indexed(42)));
        assert_eq!(
            parse_extended_color(&[2, 10, 20, 30]),
            Some(Color::Rgb(10, 20, 30))
        );
        assert_eq!(
            parse_extended_color(&[2, 0, 10, 20, 30]),
            Some(Color::Rgb(10, 20, 30))
        );
        let rendered = render_terminal_lines(
            "\x1b[38;2;255;128;0mWARNING\x1b[0m: low disk\n",
            Style::default(),
        );
        assert_eq!(rendered[0].plain, "WARNING: low disk");
        assert_eq!(
            rendered[0].line.spans[0].style.fg,
            Some(Color::Rgb(255, 128, 0))
        );
        let invalid = render_terminal_lines("\x1b[38;2;256;0;0mplain\x1b[0m", Style::default());
        assert_eq!(invalid[0].line.spans[0].style.fg, None);
    }

    #[test]
    fn git_bash_empty_reset_restores_base_style() {
        let rendered =
            render_terminal_lines("\x1b[01;31m\x1b[Kfoo\x1b[m\x1b[Kbar\n", Style::default());
        assert_eq!(rendered[0].plain, "foobar");
        assert!(
            rendered[0].line.spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(rendered[0].line.spans[1].style, Style::default());
    }

    #[test]
    fn empty_input_yields_no_lines_and_repeated_render_is_deterministic() {
        assert!(render_terminal_lines("", Style::default()).is_empty());
        let raw = "a\nb\x1b[32mc\x1b[0m\rd\ne";
        assert_eq!(
            render_terminal_lines(raw, Style::default()),
            render_terminal_lines(raw, Style::default())
        );
    }

    #[test]
    fn wide_and_combining_graphemes_use_terminal_columns_for_overwrite() {
        assert_eq!(plain("界a\r x"), " xa");
        assert_eq!(plain("e\u{301}x\rZ"), "Zx");
        assert_eq!(plain("🙂x\x1b[2GZ"), " Zx");
    }

    #[test]
    fn emoji_sequences_remain_one_grapheme_and_two_terminal_columns() {
        let family = "👩‍👩‍👧‍👦";
        assert_eq!(plain(&format!("{family}x\rAB")), "ABx");
        assert_eq!(plain("🇨🇳x\rAB"), "ABx");
    }

    #[test]
    fn erase_never_leaves_half_of_a_wide_grapheme() {
        assert_eq!(plain("A界B\x1b[3G\x1b[K"), "A");
        assert_eq!(plain("A界B\x1b[3G\x1b[1K"), "   B");
    }

    #[test]
    fn newline_splits_lines() {
        assert_eq!(lines("a\nb"), vec!["a", "b"]);
    }

    #[test]
    fn trailing_newline_adds_no_blank_line() {
        assert_eq!(lines("a\n"), vec!["a"]);
        assert_eq!(lines("a\n\n"), vec!["a", ""]);
    }

    #[test]
    fn cursor_up_then_carriage_return_and_erase() {
        assert_eq!(lines("line1\nline2\x1b[A\rXX\x1b[K"), vec!["XX", "line2"]);
    }

    #[test]
    fn sgr_splits_into_styled_spans() {
        let rendered = render_terminal_lines("plain \x1b[31mred\x1b[0m", Style::default());
        assert_eq!(rendered.len(), 1);
        let spans = &rendered[0].line.spans;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.as_ref(), "plain ");
        assert_eq!(spans[1].content.as_ref(), "red");
        assert!(spans[1].style.fg.is_some());
        assert_eq!(rendered[0].plain, "plain red");
    }

    #[test]
    fn idempotent_line_count() {
        let raw = "a\nb\x1b[32mc\x1b[0m\rd\ne";
        let first = render_terminal_lines(raw, Style::default()).len();
        let second = render_terminal_lines(raw, Style::default()).len();
        assert_eq!(first, second);
    }

    #[test]
    fn empty_input_yields_no_lines() {
        assert!(render_terminal_lines("", Style::default()).is_empty());
    }

    #[test]
    fn ansi16_mapping() {
        assert_eq!(ansi16(1), Color::Red);
        assert_eq!(ansi16(6), Color::Cyan);
        assert_eq!(ansi16_bright(2), Color::LightGreen);
        assert_eq!(ansi16_bright(7), Color::White);
    }

    #[test]
    fn ext_color_subparam_forms() {
        assert_eq!(parse_extended_color(&[5, 42]), Some(Color::Indexed(42)));
        assert_eq!(
            parse_extended_color(&[2, 10, 20, 30]),
            Some(Color::Rgb(10, 20, 30)),
        );
        assert_eq!(
            parse_extended_color(&[2, 0, 10, 20, 30]),
            Some(Color::Rgb(10, 20, 30)),
        );
        assert_eq!(parse_extended_color(&[2, 1]), None);
    }

    #[test]
    fn ignores_cursor_save_restore_and_absolute_positioning() {
        assert_eq!(plain("\x1b7\x1b[10;5Hkept\x1b8"), "kept");
    }

    #[test]
    fn forced_sgr_over_crlf_renders_styled() {
        let rendered =
            render_terminal_lines("\x1b[01;31mmatch\x1b[0m\r\nplain\r\n", Style::default());
        assert_eq!(rendered.len(), 2);
        assert_eq!(rendered[0].plain, "match");
        assert_eq!(rendered[1].plain, "plain");
        assert!(
            rendered[0]
                .line
                .spans
                .iter()
                .any(|span| span.style.fg.is_some()),
        );
    }

    #[test]
    fn git_bash_gnu_grep_color() {
        let rendered =
            render_terminal_lines("\x1b[01;31m\x1b[Kfoo\x1b[m\x1b[Kbar\n", Style::default());
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].plain, "foobar");
        let spans = &rendered[0].line.spans;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.as_ref(), "foo");
        assert!(spans[0].style.fg.is_some());
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[1].content.as_ref(), "bar");
        assert_eq!(spans[1].style, Style::default());
    }

    #[test]
    fn powershell_truecolor_psstyle() {
        let rendered = render_terminal_lines(
            "\x1b[38;2;255;128;0mWARNING\x1b[0m: low disk\n",
            Style::default(),
        );
        assert_eq!(rendered.len(), 1);
        assert_eq!(rendered[0].plain, "WARNING: low disk");
        let spans = &rendered[0].line.spans;
        assert_eq!(spans[0].content.as_ref(), "WARNING");
        assert!(spans[0].style.fg.is_some());
        assert_eq!(spans[1].style, Style::default());
    }

    #[test]
    fn progress_erase_entire_line_collapses() {
        assert_eq!(lines("loading 99%\x1b[2K\rdone\n"), vec!["done"]);
    }
}

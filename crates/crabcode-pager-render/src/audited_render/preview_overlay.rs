//! Multiline preview-overlay widget.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Widget};

use super::line_utils::{truncate_line, truncate_str};
use super::safe_buf::SafeBuf;

#[derive(Debug, Clone, Copy)]
pub struct PreviewStyle {
    pub bg: Color,
    pub text_fg: Color,
    pub border_fg: Color,
}

impl PreviewStyle {
    pub fn new(bg: Color, text_fg: Color, border_fg: Color) -> Self {
        Self {
            bg,
            text_fg,
            border_fg,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PreviewConfig {
    pub preview_lines: usize,
    pub width_ratio: f32,
    pub bottom_gap: u16,
    pub min_width: u16,
    pub min_height: u16,
    pub hint: Option<Line<'static>>,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self {
            preview_lines: 3,
            width_ratio: 0.75,
            bottom_gap: 0,
            min_width: 20,
            min_height: 5,
            hint: None,
        }
    }
}

pub fn render_preview_overlay(
    buf: &mut Buffer,
    area: Rect,
    content: &str,
    style: PreviewStyle,
    config: PreviewConfig,
) -> Option<Rect> {
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 || area.height < config.min_height || area.width < config.min_width {
        return None;
    }

    let needs_dots = total > config.preview_lines * 2;
    let content_lines = if needs_dots {
        config.preview_lines * 2 + 1
    } else {
        total
    };
    let box_height = (content_lines as u16 + 2).min(area.height);
    let box_width = ((area.width as f32) * config.width_ratio) as u16;
    let anchor_bottom = area.y + area.height - config.bottom_gap;
    let box_area = Rect {
        x: area.x + area.width.saturating_sub(box_width) / 2,
        y: anchor_bottom.saturating_sub(box_height),
        width: box_width,
        height: box_height,
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(style.border_fg))
        .style(Style::default().bg(style.bg));
    let inner = block.inner(box_area);
    Clear.render(box_area, buf);
    buf.set_style(box_area, Style::default().bg(style.bg));
    block.render(box_area, buf);

    render_content_lines(
        buf,
        inner,
        &lines,
        needs_dots,
        config.preview_lines,
        Style::default().fg(style.text_fg).bg(style.bg),
        Style::default().fg(style.border_fg).bg(style.bg),
    );
    if let Some(hint) = &config.hint {
        render_border_hint(buf, box_area, hint, style.bg);
    }
    Some(box_area)
}

fn render_content_lines(
    buf: &mut Buffer,
    inner: Rect,
    lines: &[&str],
    needs_dots: bool,
    preview_lines: usize,
    text_style: Style,
    dots_style: Style,
) {
    let mut row = 0_u16;
    if needs_dots {
        for line in lines.iter().take(preview_lines) {
            if row >= inner.height {
                break;
            }
            render_line(buf, inner.x, inner.y + row, inner.width, line, text_style);
            row += 1;
        }
        if row < inner.height {
            let omitted = lines.len() - preview_lines * 2;
            buf.set_span_safe(
                inner.x,
                inner.y + row,
                &Span::styled(format!("⋮ ({omitted} more lines)"), dots_style),
                inner.width,
            );
            row += 1;
        }
        for line in lines.iter().skip(lines.len().saturating_sub(preview_lines)) {
            if row >= inner.height {
                break;
            }
            render_line(buf, inner.x, inner.y + row, inner.width, line, text_style);
            row += 1;
        }
    } else {
        for line in lines {
            if row >= inner.height {
                break;
            }
            render_line(buf, inner.x, inner.y + row, inner.width, line, text_style);
            row += 1;
        }
    }
}

#[inline]
fn render_line(buf: &mut Buffer, x: u16, y: u16, width: u16, line: &str, style: Style) {
    buf.set_span_safe(
        x,
        y,
        &Span::styled(truncate_str(line, width as usize), style),
        width,
    );
}

fn render_border_hint(buf: &mut Buffer, box_area: Rect, hint: &Line<'static>, bg: Color) {
    const CHROME: u16 = 6;
    const MIN_TEXT_WIDTH: u16 = 8;
    let text_width = box_area.width.saturating_sub(CHROME);
    if text_width < MIN_TEXT_WIDTH {
        return;
    }

    let mut line = truncate_line(hint.clone(), text_width as usize);
    for span in &mut line.spans {
        span.style = span.style.bg(bg);
    }
    let pad = Span::styled(" ", Style::default().bg(bg));
    let mut spans = vec![pad.clone()];
    spans.append(&mut line.spans);
    spans.push(pad);
    buf.set_line_safe(
        box_area.x + 2,
        box_area.y + box_area.height - 1,
        &Line::from(spans),
        box_area.width - 4,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_style() -> PreviewStyle {
        PreviewStyle::new(Color::Indexed(234), Color::Indexed(189), Color::Indexed(60))
    }

    #[test]
    fn test_empty_content_returns_none() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 20));
        assert!(
            render_preview_overlay(
                &mut buf,
                Rect::new(0, 0, 80, 20),
                "",
                test_style(),
                PreviewConfig::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn test_area_too_small_returns_none() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 3));
        assert!(
            render_preview_overlay(
                &mut buf,
                Rect::new(0, 0, 10, 3),
                "hello\nworld",
                test_style(),
                PreviewConfig::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn test_single_line_renders() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        let rect = render_preview_overlay(
            &mut buf,
            Rect::new(0, 0, 40, 10),
            "single line",
            test_style(),
            PreviewConfig::default(),
        )
        .unwrap();
        assert_eq!(rect.height, 3);
    }

    #[test]
    fn test_few_lines_no_dots() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 15));
        let rect = render_preview_overlay(
            &mut buf,
            Rect::new(0, 0, 40, 15),
            "line1\nline2\nline3\nline4",
            test_style(),
            PreviewConfig::default(),
        )
        .unwrap();
        assert_eq!(rect.height, 6);
        assert!(!buffer_to_string(&buf).contains('⋮'));
    }

    #[test]
    fn test_many_lines_shows_dots() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 15));
        let content = (1..=10)
            .map(|index| format!("line{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            render_preview_overlay(
                &mut buf,
                Rect::new(0, 0, 40, 15),
                &content,
                test_style(),
                PreviewConfig::default(),
            )
            .is_some()
        );
        let rendered = buffer_to_string(&buf);
        assert!(rendered.contains('⋮'));
        assert!(rendered.contains("4 more lines"));
    }

    #[test]
    fn test_custom_preview_lines() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 20));
        let content = (1..=20)
            .map(|index| format!("line{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let rect = render_preview_overlay(
            &mut buf,
            Rect::new(0, 0, 40, 20),
            &content,
            test_style(),
            PreviewConfig {
                preview_lines: 5,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rect.height, 13);
        assert!(buffer_to_string(&buf).contains("10 more lines"));
    }

    #[test]
    fn test_width_ratio() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 100, 10));
        let rect = render_preview_overlay(
            &mut buf,
            Rect::new(0, 0, 100, 10),
            "hello",
            test_style(),
            PreviewConfig {
                width_ratio: 0.5,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rect.width, 50);
    }

    #[test]
    fn test_long_line_truncated() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 10));
        assert!(
            render_preview_overlay(
                &mut buf,
                Rect::new(0, 0, 30, 10),
                &"a".repeat(100),
                test_style(),
                PreviewConfig::default(),
            )
            .is_some()
        );
        assert!(buffer_to_string(&buf).contains('…'));
    }

    fn test_hint() -> Line<'static> {
        Line::from(vec![Span::raw("enter"), Span::raw(" to expand")])
    }

    fn row_to_string(buf: &Buffer, y: u16) -> String {
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    fn assert_corners(buf: &Buffer, rect: Rect) {
        let y = rect.y + rect.height - 1;
        assert_eq!(buf[(rect.x, y)].symbol(), "╰");
        assert_eq!(buf[(rect.x + rect.width - 1, y)].symbol(), "╯");
    }

    #[test]
    fn test_hint_renders_in_bottom_border() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        let rect = render_preview_overlay(
            &mut buf,
            Rect::new(0, 0, 40, 10),
            "hello\nworld",
            test_style(),
            PreviewConfig {
                hint: Some(test_hint()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(rect.height, 4);
        assert!(row_to_string(&buf, rect.y + 1).contains("hello"));
        assert!(row_to_string(&buf, rect.y + 2).contains("world"));
        assert!(row_to_string(&buf, rect.y + 3).contains("enter to expand"));
        assert_corners(&buf, rect);
    }

    #[test]
    fn test_hint_costs_no_height() {
        let area = Rect::new(0, 0, 40, 10);
        let mut with_hint_buf = Buffer::empty(area);
        let with_hint = render_preview_overlay(
            &mut with_hint_buf,
            area,
            "l1\nl2\nl3",
            test_style(),
            PreviewConfig {
                hint: Some(test_hint()),
                ..Default::default()
            },
        );
        let mut plain_buf = Buffer::empty(area);
        let plain = render_preview_overlay(
            &mut plain_buf,
            area,
            "l1\nl2\nl3",
            test_style(),
            PreviewConfig::default(),
        );
        assert!(with_hint.is_some());
        assert_eq!(with_hint, plain);
    }

    #[test]
    fn test_hint_none_keeps_plain_border() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 10));
        let rect = render_preview_overlay(
            &mut buf,
            Rect::new(0, 0, 40, 10),
            "hello\nworld",
            test_style(),
            PreviewConfig::default(),
        )
        .unwrap();
        assert_corners(&buf, rect);
        let y = rect.y + rect.height - 1;
        for x in rect.x + 1..rect.x + rect.width - 1 {
            assert_eq!(buf[(x, y)].symbol(), "─", "col {x}");
        }
    }

    #[test]
    fn test_hint_truncated_at_narrow_width() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 30, 10));
        let rect = render_preview_overlay(
            &mut buf,
            Rect::new(0, 0, 30, 10),
            "hi",
            test_style(),
            PreviewConfig {
                hint: Some(Line::from("a very long hint that cannot possibly fit")),
                ..Default::default()
            },
        )
        .unwrap();
        let bottom = row_to_string(&buf, rect.y + rect.height - 1);
        assert!(bottom.contains('…'));
        assert!(!bottom.contains("possibly"));
        assert_corners(&buf, rect);
    }

    #[test]
    fn test_hint_skipped_when_ultra_narrow() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 16, 10));
        let rect = render_preview_overlay(
            &mut buf,
            Rect::new(0, 0, 16, 10),
            "hi",
            test_style(),
            PreviewConfig {
                hint: Some(test_hint()),
                min_width: 10,
                ..Default::default()
            },
        )
        .unwrap();
        let y = rect.y + rect.height - 1;
        for x in rect.x + 1..rect.x + rect.width - 1 {
            assert_eq!(buf[(x, y)].symbol(), "─", "col {x}");
        }
        assert_corners(&buf, rect);
    }

    fn buffer_to_string(buf: &Buffer) -> String {
        let mut output = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                output.push_str(buf[(x, y)].symbol());
            }
            output.push('\n');
        }
        output
    }
}

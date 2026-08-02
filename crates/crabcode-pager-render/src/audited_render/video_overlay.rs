//! Video playback overlay chrome for a native terminal surface.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use super::safe_buf::SafeBuf;

/// Renderer-owned video progress facts.
///
/// Decoding, filesystem access, and tool execution remain outside this crate.
#[derive(Debug, Clone)]
pub struct VideoViewerState {
    pub frame_count: usize,
    pub current_frame: usize,
    pub playing: bool,
    pub fps: f64,
    pub video_width: u32,
    pub video_height: u32,
    pub duration_secs: f64,
    pub title: Option<String>,
}

impl VideoViewerState {
    pub fn position_secs(&self) -> f64 {
        if self.fps <= 0.0 {
            0.0
        } else {
            self.current_frame as f64 / self.fps
        }
    }

    pub fn progress(&self) -> f64 {
        if self.frame_count <= 1 {
            0.0
        } else {
            self.current_frame as f64 / (self.frame_count - 1) as f64
        }
    }
}

/// Render the video viewer chrome and return its popup rectangle.
pub fn render_video_overlay(
    buf: &mut Buffer,
    area: Rect,
    viewer: &VideoViewerState,
    bg: Color,
    text_fg: Color,
    border_fg: Color,
) -> Option<Rect> {
    if area.height < 8 || area.width < 20 {
        return None;
    }

    super::color::dim_area(buf, area, bg, 0.5);
    let popup_width = ((u32::from(area.width) * 90) / 100)
        .max(28)
        .min(u32::from(area.width)) as u16;
    let popup_height = ((u32::from(area.height) * 90) / 100)
        .max(8)
        .min(u32::from(area.height)) as u16;
    let popup_rect = Rect::new(
        area.x + area.width.saturating_sub(popup_width) / 2,
        area.y + area.height.saturating_sub(popup_height) / 2,
        popup_width,
        popup_height,
    );

    ratatui::widgets::Clear.render(popup_rect, buf);
    buf.set_style(popup_rect, Style::default().fg(text_fg).bg(bg));
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_fg).bg(bg))
        .style(Style::default().bg(bg))
        .render(popup_rect, buf);

    let title = viewer.title.as_ref().map_or_else(
        || {
            format!(
                " Video ({}\u{00d7}{}) ",
                viewer.video_width, viewer.video_height
            )
        },
        |name| {
            format!(
                " {name} ({}\u{00d7}{}) ",
                viewer.video_width, viewer.video_height
            )
        },
    );
    let title_style = Style::default()
        .fg(text_fg)
        .bg(bg)
        .add_modifier(Modifier::BOLD);
    let title_width = u16::try_from(title.len()).unwrap_or(u16::MAX);
    let title_x = popup_rect.x + popup_rect.width.saturating_sub(title_width) / 2;
    buf.set_span_safe(
        title_x,
        popup_rect.y,
        &Span::styled(&title, title_style),
        title_width,
    );
    render_progress_bar(buf, popup_rect, viewer, text_fg, border_fg, bg);
    Some(popup_rect)
}

fn render_progress_bar(
    buf: &mut Buffer,
    popup_rect: Rect,
    viewer: &VideoViewerState,
    text_fg: Color,
    bar_dim: Color,
    bg: Color,
) {
    let inner_width = popup_rect.width.saturating_sub(2) as usize;
    if inner_width <= 10 {
        return;
    }
    let icon = if viewer.playing {
        "\u{25b6}"
    } else {
        "\u{23f8}"
    };
    let time_label = format!(
        "{icon} {}/{}  ",
        format_time(viewer.position_secs()),
        format_time(viewer.duration_secs),
    );
    let bar_width = inner_width.saturating_sub(time_label.len());
    if bar_width <= 4 {
        return;
    }
    let filled = ((viewer.progress() * bar_width as f64).round() as usize).min(bar_width);
    let line = Line::from(vec![
        Span::styled(time_label, Style::default().fg(text_fg).bg(bg)),
        Span::styled(
            "\u{2501}".repeat(filled),
            Style::default().fg(text_fg).bg(bg),
        ),
        Span::styled(
            "\u{2500}".repeat(bar_width.saturating_sub(filled)),
            Style::default().fg(bar_dim).bg(bg),
        ),
    ]);
    buf.set_line_safe(
        popup_rect.x + 1,
        popup_rect.y + popup_rect.height.saturating_sub(1),
        &line,
        inner_width as u16,
    );
}

fn format_time(secs: f64) -> String {
    let total = secs.round() as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_time_zero() {
        assert_eq!(format_time(0.0), "0:00");
    }

    #[test]
    fn format_time_short() {
        assert_eq!(format_time(5.4), "0:05");
    }

    #[test]
    fn format_time_minutes() {
        assert_eq!(format_time(90.0), "1:30");
    }
}

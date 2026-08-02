//! Rendered blockquote-bar detection for selection metadata.

use std::borrow::Cow;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::scrollback::types::Selectable;
use crate::theme::Theme;

#[derive(Clone, Copy)]
pub(crate) struct QuoteBarStrip {
    bar_style: Option<Style>,
}

impl QuoteBarStrip {
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            bar_style: enabled.then(quote_bar_style),
        }
    }

    pub(crate) fn selectable(&self, line: &mut Line<'static>) -> Selectable {
        match self.bar_style {
            Some(bar_style) => quote_prefix_selectable(line, bar_style),
            None => Selectable::All,
        }
    }
}

fn quote_bar_style() -> Style {
    let muted = Theme::current().markdown_muted;
    let style = Style::default().add_modifier(Modifier::DIM);
    if muted == Color::Reset {
        style
    } else {
        style.fg(muted)
    }
}

fn rendered_quote_prefix_len(line: &Line<'_>, bar_style: Style) -> Option<usize> {
    const BAR: char = '\u{2502}';
    const BAR_LEN: usize = '\u{2502}'.len_utf8();

    let mut characters = line
        .spans
        .iter()
        .flat_map(|span| {
            span.content
                .chars()
                .map(move |character| (character, span.style))
        })
        .peekable();
    let mut length = 0;
    loop {
        match characters.next() {
            Some((BAR, style)) if style == bar_style => length += BAR_LEN,
            _ => return None,
        }
        match characters.next() {
            None => return Some(length),
            Some((' ', _)) => {
                length += 1;
                match characters.peek() {
                    None => return Some(length),
                    Some((BAR, style)) if *style == bar_style => continue,
                    Some(_) => break,
                }
            }
            Some(_) => return None,
        }
    }
    if characters.any(|(character, _)| character == BAR) {
        return None;
    }
    Some(length)
}

fn split_spans_at(line: &mut Line<'static>, byte_offset: usize) -> usize {
    let mut accumulated = 0;
    for index in 0..line.spans.len() {
        let end = accumulated + line.spans[index].content.len();
        if end == byte_offset {
            return index + 1;
        }
        if end > byte_offset {
            let local = byte_offset - accumulated;
            let span = &mut line.spans[index];
            let tail = match &mut span.content {
                Cow::Borrowed(content) => {
                    let (head, tail) = content.split_at(local);
                    span.content = Cow::Borrowed(head);
                    Cow::Borrowed(tail)
                }
                Cow::Owned(content) => Cow::Owned(content.split_off(local)),
            };
            let style = span.style;
            line.spans.insert(
                index + 1,
                Span {
                    content: tail,
                    style,
                },
            );
            return index + 1;
        }
        accumulated = end;
    }
    line.spans.len()
}

fn quote_prefix_selectable(line: &mut Line<'static>, bar_style: Style) -> Selectable {
    let genuine = line
        .spans
        .first()
        .is_some_and(|span| span.content.as_ref() == "\u{2502}" && span.style == bar_style);
    if !genuine {
        return Selectable::All;
    }
    let Some(prefix_length) = rendered_quote_prefix_len(line, bar_style) else {
        return Selectable::All;
    };
    let prefix_spans = split_spans_at(line, prefix_length);
    Selectable::Spans(prefix_spans..line.spans.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_generated_prefix_is_excluded_after_exact_span_split() {
        let style = quote_bar_style();
        let mut line = Line::from(vec![
            Span::styled("│", style),
            Span::raw(" "),
            Span::styled("│", style),
            Span::raw(" deep"),
        ]);
        assert_eq!(rendered_quote_prefix_len(&line, style), Some(8));
        assert_eq!(
            quote_prefix_selectable(&mut line, style),
            Selectable::Spans(4..5),
        );
        assert_eq!(line.spans[4].content.as_ref(), "deep");
    }

    #[test]
    fn literal_or_interior_bars_fail_closed_to_full_selection() {
        let style = quote_bar_style();
        let mut literal = Line::raw("│ literal");
        assert_eq!(
            quote_prefix_selectable(&mut literal, style),
            Selectable::All,
        );

        let mut interior = Line::from(vec![Span::styled("│", style), Span::raw(" text │ content")]);
        assert_eq!(
            quote_prefix_selectable(&mut interior, style),
            Selectable::All,
        );
    }

    #[test]
    fn raw_mode_disables_quote_marker_stripping() {
        let style = quote_bar_style();
        let mut line = Line::from(vec![Span::styled("│", style), Span::raw(" text")]);
        assert_eq!(
            QuoteBarStrip::new(false).selectable(&mut line),
            Selectable::All,
        );
    }
}

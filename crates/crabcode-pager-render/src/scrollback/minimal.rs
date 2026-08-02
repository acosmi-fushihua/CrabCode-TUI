//! Native-scrollback commit and live-tail mechanics for one `ScrollbackState`.
//!
//! This is a dependency-closed adaptation of the pinned renderer source:
//! - repository commit `a5727c5960452e7527a154b25cb5bf00cda0545e`
//! - source revision `30192d2eef5d91a8fff0e53957de5bd05b43398c`
//! - the minimal commit module (`is_committable`,
//!   `minimal_commit_display_mode`, `scan_frontier`, `commit_leading_run`,
//!   `committed_appearance`, committed renderer)
//! - the minimal live module (`live_tail_renderer`, `draw_tail`,
//!   `tail_height`)
//!
//! Terminal insertion remains an injected callback at the TUI layer. This
//! module owns only renderer state and cells; it has no process or protocol
//! dependency.

use std::path::Path;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

use super::block::RenderBlock;
use super::blocks::ToolCallBlock;
use super::entry::ScrollbackEntry;
use super::state::ScrollbackState;
use super::types::DisplayMode;
use super::wrappers::EntryRenderer;
use crate::appearance::{AppearanceConfig, RendererLanguage};
use crate::render::Renderable;
use crate::theme::Theme;

/// Blank rows after each committed/live block. The fixed source currently
/// uses zero; keeping the shared constant prevents frontier height drift.
pub const MINIMAL_BLOCK_GAP: u16 = 0;

/// Whether one frontier entry is stable enough for print-once insertion.
pub fn is_committable(entry: &ScrollbackEntry, turn_running: bool, is_last: bool) -> bool {
    // Some CrabCode projection rows are deliberately retractable even after a
    // turn becomes idle. In particular, the historical direct TUI keeps the
    // last API retry dynamic and removes it when any later non-error message
    // arrives. Native scrollback is immutable, so committing such a row would
    // permanently leak an error beside a later successful response.
    if entry.block.holds_native_scrollback_frontier() {
        return false;
    }
    if entry.is_pending_user_input {
        return false;
    }
    if !turn_running {
        return true;
    }
    if !entry.is_running {
        return true;
    }
    matches!(entry.block, RenderBlock::BgTask(_))
        || (!is_last && matches!(entry.block, RenderBlock::AgentMessage(_)))
}

/// Print-once display policy from the fixed minimal lifecycle.
pub fn minimal_commit_display_mode(
    block: &RenderBlock,
    appearance: &AppearanceConfig,
) -> DisplayMode {
    match block {
        RenderBlock::ToolCall(ToolCallBlock::Edit(_)) => DisplayMode::Expanded,
        RenderBlock::ToolCall(
            tool @ (ToolCallBlock::Search(_)
            | ToolCallBlock::Read(_)
            | ToolCallBlock::ListDir(_)
            | ToolCallBlock::MemorySearch(_)
            | ToolCallBlock::IntegrationSearch(_)),
        ) if tool.is_success() => DisplayMode::Collapsed,
        RenderBlock::ToolCall(_) => DisplayMode::Truncated,
        RenderBlock::Thinking(_) if appearance.minimal_collapse_thinking => DisplayMode::Collapsed,
        RenderBlock::Thinking(_) => DisplayMode::Expanded,
        _ => DisplayMode::Expanded,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrontierScan {
    pub tail_start: usize,
    pub will_commit: bool,
}

enum Step {
    Commit,
    Skip,
    Stop,
}

fn classify(state: &ScrollbackState, index: usize, turn_running: bool) -> Step {
    let is_last = index.saturating_add(1) >= state.len();
    match state.get(index) {
        None => Step::Stop,
        Some(entry) if state.is_committed(entry.id) => Step::Skip,
        Some(entry) if !is_committable(entry, turn_running, is_last) => Step::Stop,
        Some(_) => Step::Commit,
    }
}

/// Read-only view of the exact frontier the next commit pass will consume.
pub fn scan_frontier(state: &ScrollbackState, turn_running: bool) -> FrontierScan {
    let mut index = state.commit_scan_cursor();
    let mut will_commit = false;
    loop {
        match classify(state, index, turn_running) {
            Step::Stop => break,
            Step::Skip => index = index.saturating_add(1),
            Step::Commit => {
                will_commit = true;
                index = index.saturating_add(1);
            }
        }
    }
    FrontierScan {
        tail_start: index,
        will_commit,
    }
}

/// The single mutating print-once frontier walk.
///
/// `on_commit` must return `true` only after terminal insertion succeeds.
/// Failed insertion leaves the entry uncommitted and retryable.
pub fn commit_leading_run(
    state: &mut ScrollbackState,
    turn_running: bool,
    mut on_commit: impl FnMut(&mut ScrollbackState, usize) -> bool,
) -> usize {
    let mut index = state.commit_scan_cursor();
    let mut count = 0usize;
    loop {
        match classify(state, index, turn_running) {
            Step::Stop => break,
            Step::Skip => index = index.saturating_add(1),
            Step::Commit => {
                if !on_commit(state, index) {
                    break;
                }
                state.mark_committed(index);
                count = count.saturating_add(1);
                index = index.saturating_add(1);
            }
        }
    }
    state.set_commit_scan_cursor(index);
    count
}

/// Stamp the live tail with the same display policy it will use after commit.
pub fn stamp_live_tail(state: &mut ScrollbackState) {
    let appearance = state.appearance().clone();
    let mut index = state.commit_scan_cursor();
    while let Some(entry) = state.get_mut(index) {
        entry.set_display_mode(minimal_commit_display_mode(&entry.block, &appearance));
        index = index.saturating_add(1);
    }
}

/// Finalize and stamp one frontier entry immediately before print-once paint.
///
/// Idle turns may leave an entry's animation flag set even though its content
/// is stable. Finalizing it here keeps native scrollback static and prevents a
/// stale running form from being printed permanently.
pub fn prepare_commit_entry(state: &mut ScrollbackState, index: usize) {
    if let Some(id) = state
        .get(index)
        .filter(|entry| entry.is_running)
        .map(|entry| entry.id)
    {
        state.finish_running(id);
    }
    let appearance = state.appearance().clone();
    if let Some(entry) = state.get_mut(index) {
        entry.set_display_mode(minimal_commit_display_mode(&entry.block, &appearance));
    }
}

/// Preserve the fixed expand-after-commit queue for entries printed folded.
///
/// This must run only after terminal insertion succeeds.
pub fn record_folded_commit(state: &mut ScrollbackState, index: usize) {
    let Some((id, mode)) = state
        .get(index)
        .map(|entry| (entry.id, entry.display_mode()))
    else {
        return;
    };
    if matches!(mode, DisplayMode::Collapsed | DisplayMode::Truncated) {
        state.record_committed_for_expand(id);
    }
}

/// Flat, timestamp-free appearance shared by committed blocks and live tail.
pub fn committed_appearance(base: &AppearanceConfig) -> AppearanceConfig {
    let mut appearance = base.clone();
    appearance.show_timestamps = false;
    appearance.scrollback.layout.block_pad_left = 0;
    appearance.scrollback.layout.block_pad_right = 0;
    appearance.scrollback.blocks.thinking.body_dim_italic = true;
    appearance.scrollback.blocks.thinking.collapsed_expand_hint = true;
    appearance
}

pub fn committed_renderer<'a>(
    entry: &'a ScrollbackEntry,
    theme: &'a Theme,
    appearance: AppearanceConfig,
    cwd: Option<&'a Path>,
) -> EntryRenderer<'a> {
    let hide_accent = !matches!(entry.block, RenderBlock::Thinking(_))
        || entry.display_mode() == DisplayMode::Collapsed;
    EntryRenderer::new(entry, theme)
        .with_appearance(appearance)
        .with_cwd(cwd)
        .with_tick(0)
        .with_flat_background(true)
        .with_hide_accent(hide_accent)
        .with_dim_accent(true)
}

pub fn live_tail_renderer<'a>(
    entry: &'a ScrollbackEntry,
    theme: &'a Theme,
    appearance: &AppearanceConfig,
    cwd: Option<&'a Path>,
    tick: u64,
) -> EntryRenderer<'a> {
    let hide_accent = !matches!(entry.block, RenderBlock::Thinking(_))
        || entry.display_mode() == DisplayMode::Collapsed;
    EntryRenderer::new(entry, theme)
        .with_appearance(appearance.clone())
        .with_cwd(cwd)
        .with_tick(tick)
        .with_flat_background(true)
        .with_hide_accent(hide_accent)
        .with_dim_accent(true)
}

fn truncation_footer(language: RendererLanguage, hidden: u16) -> String {
    match language {
        RendererLanguage::ZhCn => format!("… 还有 {hidden} 行 — 使用 /transcript 查看"),
        RendererLanguage::EnUs => {
            format!("… {hidden} more lines — /transcript to view")
        }
    }
}

/// Paint a committed renderer into the terminal-owned insertion buffer.
pub fn paint_committed(
    buffer: &mut Buffer,
    renderer: EntryRenderer<'_>,
    width: u16,
    full_height: u16,
    footer_style: Style,
) {
    let commit_height = buffer.area.height;
    let language = renderer.language();
    renderer.render(
        Rect {
            x: buffer.area.x,
            y: buffer.area.y,
            width,
            height: full_height,
        },
        buffer,
    );
    if commit_height == 0 || commit_height >= full_height {
        return;
    }
    let hidden = full_height.saturating_sub(commit_height.saturating_sub(1));
    let y = buffer
        .area
        .y
        .saturating_add(commit_height)
        .saturating_sub(1);
    let row = Rect {
        x: buffer.area.x,
        y,
        width,
        height: 1,
    };
    let style = footer_style.bg(Color::Reset);
    buffer.set_style(row, style);
    buffer.set_span(
        buffer.area.x,
        y,
        &Span::styled(truncation_footer(language, hidden), style),
        width,
    );
}

/// Draw the exact uncommitted frontier tail, bottom anchored.
#[allow(clippy::too_many_arguments)]
pub fn draw_tail(
    buffer: &mut Buffer,
    area: Rect,
    state: &ScrollbackState,
    turn_running: bool,
    theme: &Theme,
    appearance: &AppearanceConfig,
    cwd: Option<&Path>,
    tick: u64,
) {
    if area.height == 0 {
        return;
    }
    let renderer = |entry| live_tail_renderer(entry, theme, appearance, cwd, tick);
    let mut entries = Vec::new();
    let mut index = scan_frontier(state, turn_running).tail_start;
    while let Some(entry) = state.get(index) {
        entries.push(entry);
        index = index.saturating_add(1);
    }
    if entries.is_empty() {
        return;
    }
    let heights = entries
        .iter()
        .map(|entry| renderer(entry).desired_height(area.width))
        .collect::<Vec<_>>();
    let total = heights.iter().fold(0u16, |sum, height| {
        sum.saturating_add(*height)
            .saturating_add(MINIMAL_BLOCK_GAP)
    });
    let mut skip_top = total.saturating_sub(area.height);
    let mut y = area.y;
    let bottom = area.y.saturating_add(area.height);
    for (entry, content_height) in entries.iter().zip(heights) {
        let slot_height = content_height.saturating_add(MINIMAL_BLOCK_GAP);
        if skip_top >= slot_height {
            skip_top = skip_top.saturating_sub(slot_height);
            continue;
        }
        let slot_skip = skip_top;
        skip_top = 0;
        let entry_skip = slot_skip.min(content_height);
        let visible_content = content_height.saturating_sub(entry_skip);
        if visible_content > 0 {
            let draw_height = visible_content.min(bottom.saturating_sub(y));
            if draw_height == 0 {
                break;
            }
            renderer(entry).with_skip_rows(entry_skip).render(
                Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height: draw_height,
                },
                buffer,
            );
            y = y.saturating_add(draw_height);
            if y >= bottom {
                break;
            }
        }
        let gap_skipped = slot_skip.saturating_sub(entry_skip);
        let gap_visible = MINIMAL_BLOCK_GAP
            .saturating_sub(gap_skipped)
            .min(bottom.saturating_sub(y));
        y = y.saturating_add(gap_visible);
        if y >= bottom {
            break;
        }
    }
}

pub fn tail_height(
    state: &ScrollbackState,
    turn_running: bool,
    width: u16,
    appearance: &AppearanceConfig,
    cwd: Option<&Path>,
) -> u16 {
    let theme = Theme::current();
    let mut index = scan_frontier(state, turn_running).tail_start;
    let mut total = 0u16;
    while let Some(entry) = state.get(index) {
        total = total
            .saturating_add(
                live_tail_renderer(entry, &theme, appearance, cwd, 0).desired_height(width),
            )
            .saturating_add(MINIMAL_BLOCK_GAP);
        index = index.saturating_add(1);
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn finalized(text: &str) -> ScrollbackEntry {
        ScrollbackEntry::new(RenderBlock::stub(text, Color::Blue))
    }

    fn running(text: &str) -> ScrollbackEntry {
        ScrollbackEntry::running(RenderBlock::stub(text, Color::Blue))
    }

    #[test]
    fn truncation_footer_localizes_chrome_and_preserves_count_and_command() {
        assert_eq!(
            truncation_footer(RendererLanguage::ZhCn, 37),
            "… 还有 37 行 — 使用 /transcript 查看"
        );
        assert_eq!(
            truncation_footer(RendererLanguage::EnUs, 37),
            "… 37 more lines — /transcript to view"
        );
    }

    #[test]
    fn commit_frontier_is_retryable_and_stops_at_live_entry() {
        let mut state = ScrollbackState::new();
        state.push(finalized("a"));
        state.push(finalized("b"));
        state.push(running("c"));
        state.push(finalized("d"));

        let mut emitted = Vec::new();
        let count = commit_leading_run(&mut state, true, |_, index| {
            emitted.push(index);
            index == 0
        });
        assert_eq!(count, 1);
        assert_eq!(emitted, vec![0, 1]);
        assert_eq!(state.commit_scan_cursor(), 1);

        emitted.clear();
        let count = commit_leading_run(&mut state, true, |_, index| {
            emitted.push(index);
            true
        });
        assert_eq!(count, 1);
        assert_eq!(emitted, vec![1]);
        assert_eq!(scan_frontier(&state, true).tail_start, 2);
    }

    #[test]
    fn commit_frontier_commits_leading_finalized_run_and_stops_at_running() {
        let mut state = ScrollbackState::new();
        state.push(finalized("a"));
        state.push(finalized("b"));
        let running_id = state.push(running("c"));
        state.push(finalized("d"));

        let mut emitted = Vec::new();
        assert_eq!(
            commit_leading_run(&mut state, true, |_, index| {
                emitted.push(index);
                true
            }),
            2
        );
        assert_eq!(emitted, vec![0, 1]);
        assert_eq!(scan_frontier(&state, true).tail_start, 2);

        state.finish_running(running_id);
        emitted.clear();
        assert_eq!(
            commit_leading_run(&mut state, true, |_, index| {
                emitted.push(index);
                true
            }),
            2
        );
        assert_eq!(emitted, vec![2, 3]);
        assert_eq!(scan_frontier(&state, true).tail_start, 4);
    }

    #[test]
    fn commit_write_failure_leaves_entry_uncommitted_for_retry() {
        let mut state = ScrollbackState::new();
        state.push(finalized("retry"));

        assert_eq!(commit_leading_run(&mut state, false, |_, _| false), 0);
        assert_eq!(state.commit_scan_cursor(), 0);
        assert!(scan_frontier(&state, false).will_commit);

        assert_eq!(commit_leading_run(&mut state, false, |_, _| true), 1);
        assert_eq!(state.commit_scan_cursor(), 1);
        assert!(!scan_frontier(&state, false).will_commit);
    }

    #[test]
    fn committed_thinking_renders_the_expanded_body() {
        let mut state = ScrollbackState::new();
        state.push(ScrollbackEntry::new(RenderBlock::thinking(
            "first reasoning line\n\nsecond reasoning line",
        )));
        prepare_commit_entry(&mut state, 0);
        let entry = state.get(0).expect("thinking entry");
        assert_eq!(entry.display_mode(), DisplayMode::Expanded);

        let theme = Theme::current();
        let renderer = committed_renderer(
            entry,
            &theme,
            committed_appearance(state.appearance()),
            None,
        );
        let width = 80;
        let height = renderer.desired_height(width);
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        paint_committed(&mut buffer, renderer, width, height, theme.dim());
        let painted = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(painted.contains("first reasoning line"));
        assert!(painted.contains("second reasoning line"));
    }

    #[test]
    fn collapse_thinking_is_opt_in_and_changes_only_reasoning() {
        let default = committed_appearance(&AppearanceConfig::default());
        let collapsed = committed_appearance(&AppearanceConfig {
            minimal_collapse_thinking: true,
            ..AppearanceConfig::default()
        });
        let thinking = RenderBlock::thinking("reasoning");
        assert_eq!(
            minimal_commit_display_mode(&thinking, &default),
            DisplayMode::Expanded
        );
        assert_eq!(
            minimal_commit_display_mode(&thinking, &collapsed),
            DisplayMode::Collapsed
        );

        for block in [
            RenderBlock::agent_message("answer"),
            RenderBlock::edit("file.rs", None),
            RenderBlock::execute("ls"),
            RenderBlock::read("src/lib.rs", None),
        ] {
            assert_eq!(
                minimal_commit_display_mode(&block, &collapsed),
                minimal_commit_display_mode(&block, &default),
                "minimal reasoning policy must not change other blocks: {block:?}"
            );
        }
    }

    #[test]
    fn committed_appearance_enables_minimal_reasoning_legibility_only_on_clone() {
        let base = AppearanceConfig::default();
        assert!(!base.scrollback.blocks.thinking.body_dim_italic);
        assert!(!base.scrollback.blocks.thinking.collapsed_expand_hint);

        let committed = committed_appearance(&base);
        assert!(committed.scrollback.blocks.thinking.body_dim_italic);
        assert!(committed.scrollback.blocks.thinking.collapsed_expand_hint);
        assert!(!base.scrollback.blocks.thinking.body_dim_italic);
        assert!(!base.scrollback.blocks.thinking.collapsed_expand_hint);
    }

    #[test]
    fn expanded_minimal_thinking_keeps_a_dim_accent_while_answer_does_not() {
        use ratatui::style::Modifier;

        let theme = Theme::current();
        let appearance = committed_appearance(&AppearanceConfig::default());
        let mut thinking = ScrollbackEntry::new(RenderBlock::thinking(
            "reasoning long enough to remain visibly separate from the answer",
        ));
        thinking.set_display_mode(DisplayMode::Expanded);
        let renderer = committed_renderer(&thinking, &theme, appearance.clone(), None);
        assert_eq!(renderer.chrome_width(), 1);
        let width = 60;
        let height = renderer.desired_height(width).max(1);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        renderer.render(Rect::new(0, 0, width, height), &mut buffer);
        let rail = crate::glyphs::accent_bar();
        for y in 0..height {
            let cell = buffer.cell((0, y)).expect("thinking accent cell");
            assert_eq!(cell.symbol(), rail);
            assert!(cell.modifier.contains(Modifier::DIM));
        }

        let answer = ScrollbackEntry::new(RenderBlock::agent_message("answer"));
        let renderer = committed_renderer(&answer, &theme, appearance, None);
        assert_eq!(renderer.chrome_width(), 0);
        let area = Rect::new(0, 0, width, renderer.desired_height(width).max(1));
        let mut buffer = Buffer::empty(area);
        renderer.render(area, &mut buffer);
        assert_ne!(buffer.cell((0, 0)).expect("answer cell").symbol(), rail);
    }

    #[test]
    fn committed_renderer_wraps_long_unicode_without_loss() {
        let text = "甲乙丙丁戊己庚辛壬癸";
        let mut state = ScrollbackState::new();
        state.push(ScrollbackEntry::new(RenderBlock::user_prompt(text)));
        prepare_commit_entry(&mut state, 0);
        let entry = state.get(0).expect("user prompt entry");
        let theme = Theme::current();
        let width = 8;
        let renderer = committed_renderer(
            entry,
            &theme,
            committed_appearance(state.appearance()),
            None,
        );
        let height = renderer.desired_height(width);
        assert!(
            height > 1,
            "narrow committed prompt must occupy multiple rows"
        );

        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        paint_committed(&mut buffer, renderer, width, height, theme.dim());
        let compact = buffer
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();

        assert!(
            compact.contains(text),
            "native-scrollback paint must preserve every wrapped Unicode scalar in order: {compact}"
        );
    }

    #[test]
    fn committed_execute_output_preserves_ansi_foreground_style() {
        let mut state = ScrollbackState::new();
        state.push(ScrollbackEntry::new(RenderBlock::execute_with_output(
            "printf",
            "\u{1b}[31mZ\u{1b}[0m",
            None::<String>,
        )));
        prepare_commit_entry(&mut state, 0);
        let entry = state.get(0).expect("completed execute entry");
        let theme = Theme::current();
        let width = 80;
        let renderer = committed_renderer(
            entry,
            &theme,
            committed_appearance(state.appearance()),
            None,
        );
        let height = renderer.desired_height(width);
        let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
        paint_committed(&mut buffer, renderer, width, height, theme.dim());

        let red_output_cell = buffer
            .content()
            .iter()
            .find(|cell| cell.symbol() == "Z")
            .expect("ANSI-styled execute output must reach the committed buffer");
        assert_eq!(red_output_cell.fg, Color::Red);
    }

    #[test]
    fn idle_turn_commits_stale_running_entries() {
        let state = {
            let mut state = ScrollbackState::new();
            state.push(running("stale"));
            state
        };
        assert!(scan_frontier(&state, false).will_commit);
    }
}

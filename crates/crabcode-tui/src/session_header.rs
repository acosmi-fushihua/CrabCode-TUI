//! Typed, fixed-height session header.
//!
//! The header derives activity exclusively from renderer lifecycle/projection
//! facts. Free-form `TuiApp::status` text is deliberately absent so copy or
//! language changes cannot alter state priority.

use std::time::{Duration, Instant};

use crabcode_pager_render::audited_glyphs::{ballot_x, check_mark, diamond_filled, record_dot};
use crabcode_pager_render::audited_theme::CrabCodeTheme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr as _;

use crate::text_safety::sanitize_bounded_terminal_text;
use crate::tui_app::{RequestDialog, TuiApp, UiLanguage, localized_permission_mode_label};
use crate::tui_render::fit_line_to_width;
use crate::turn_lifecycle::{AgentState, TurnActivity, TurnOutcome, WaitingReason};
use crate::welcome_surface::middle_ellipsis;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeaderActivity {
    Fatal,
    WaitingForUser { subject: Option<String> },
    Cancelling,
    Requesting,
    Thinking,
    Responding,
    ToolInput { tool_name: Option<String> },
    ToolUse { tool_name: Option<String> },
    WaitingForModel,
    WaitingForSubagents { count: usize },
    WaitingForTask { count: usize },
    Retrying,
    Initializing,
    Failed,
    Cancelled,
    Completed,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PermissionPresentation {
    label: Option<String>,
    dangerous: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextPresentation {
    total_tokens: u64,
    max_tokens: u64,
    percentage: u64,
    baseline: bool,
    pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SessionHeaderViewModel {
    language: UiLanguage,
    activity: HeaderActivity,
    elapsed: Option<Duration>,
    model: Option<String>,
    permission: PermissionPresentation,
    context: Option<ContextPresentation>,
}

impl SessionHeaderViewModel {
    pub(crate) fn from_app(app: &TuiApp, now: Instant) -> Self {
        let turn = app.turn_status();
        let typed_tool_name = || {
            let active_sequence = app.projection.direct_stream_activity().raw_sequence;
            app.projection.items().iter().rev().find_map(|item| {
                let tool = item.presentation.tool.as_ref()?;
                (item.streaming
                    || active_sequence
                        .is_some_and(|sequence| item.raw_sequences.contains(&sequence)))
                .then_some(tool.name.as_deref())
                .flatten()
                .map(clean_subject)
            })
        };
        let activity = if app.fatal.is_some() {
            HeaderActivity::Fatal
        } else if let Some(dialog) = app.dialog.as_ref() {
            HeaderActivity::WaitingForUser {
                subject: match dialog {
                    RequestDialog::Permission {
                        tool_name,
                        display_name,
                        ..
                    } => display_name
                        .as_deref()
                        .or(Some(tool_name.as_str()))
                        .map(clean_subject),
                    RequestDialog::Question(_) => Some(
                        app.ui_language()
                            .text("需要回答", "answer needed")
                            .to_string(),
                    ),
                    RequestDialog::Elicitation { server_name, .. } => {
                        Some(clean_subject(server_name))
                    }
                    RequestDialog::GroveTerms { .. }
                    | RequestDialog::Setup(_)
                    | RequestDialog::SetupInput(_) => None,
                },
            }
        } else if turn.state() == AgentState::Cancelling {
            HeaderActivity::Cancelling
        } else if let Some(activity) = turn.activity() {
            match activity {
                TurnActivity::Requesting => HeaderActivity::Requesting,
                TurnActivity::Thinking => HeaderActivity::Thinking,
                TurnActivity::Responding => HeaderActivity::Responding,
                TurnActivity::ToolInput => HeaderActivity::ToolInput {
                    tool_name: typed_tool_name(),
                },
                TurnActivity::ToolUse => HeaderActivity::ToolUse {
                    tool_name: typed_tool_name(),
                },
                TurnActivity::Waiting(WaitingReason::Model) => HeaderActivity::WaitingForModel,
                TurnActivity::Waiting(WaitingReason::User) => {
                    HeaderActivity::WaitingForUser { subject: None }
                }
                TurnActivity::Waiting(WaitingReason::Subagent) => {
                    HeaderActivity::WaitingForSubagents {
                        count: turn.watchers().subagents,
                    }
                }
                TurnActivity::Waiting(WaitingReason::Task) => HeaderActivity::WaitingForTask {
                    count: turn.watchers().total(),
                },
                TurnActivity::Retrying => HeaderActivity::Retrying,
            }
        } else if app.stream_requesting() {
            HeaderActivity::Requesting
        } else if turn.watchers().total() > 0 {
            if turn.watchers().subagents > 0 {
                HeaderActivity::WaitingForSubagents {
                    count: turn.watchers().subagents,
                }
            } else {
                HeaderActivity::WaitingForTask {
                    count: turn.watchers().total(),
                }
            }
        } else {
            match app.projection.session_state() {
                Some("initializing") => HeaderActivity::Initializing,
                Some("requires_action") => HeaderActivity::WaitingForUser { subject: None },
                Some("running") => HeaderActivity::Responding,
                _ => match turn.last_outcome() {
                    Some(TurnOutcome::Failed | TurnOutcome::RuntimeStopped) => {
                        HeaderActivity::Failed
                    }
                    Some(TurnOutcome::Cancelled) => HeaderActivity::Cancelled,
                    Some(TurnOutcome::Complete) => HeaderActivity::Completed,
                    None => HeaderActivity::Ready,
                },
            }
        };
        let elapsed = if turn.is_running() {
            turn.activity_started_at()
                .map(|started| now.saturating_duration_since(started))
                .or_else(|| turn.elapsed(now))
        } else if matches!(
            activity,
            HeaderActivity::Failed | HeaderActivity::Cancelled | HeaderActivity::Completed
        ) {
            turn.last_elapsed()
        } else {
            // Watchers and a newly initialized idle session are not a past
            // foreground turn. Showing last_elapsed here would attach stale
            // timing to unrelated background work or to Ready.
            None
        };
        let active_model = app.projection.model();
        let model = active_model.map(|active| {
            let display = app
                .models
                .iter()
                .find(|choice| choice.id == active)
                .map(|choice| choice.label.as_str())
                .filter(|label| !label.trim().is_empty())
                .unwrap_or(active);
            sanitize_bounded_terminal_text(display).into_owned()
        });
        let permission_mode = app.projection.permission_mode();
        let permission = PermissionPresentation {
            label: permission_mode.map(|mode| {
                localized_permission_mode_label(Some(mode), app.ui_language()).to_string()
            }),
            dangerous: permission_mode == Some("bypassPermissions"),
        };
        let context = app.live_context_usage().map(|usage| ContextPresentation {
            total_tokens: usage.total_tokens,
            max_tokens: usage.max_tokens,
            percentage: usage.percentage,
            baseline: app.context_usage_is_baseline(),
            pending: app.context_usage_refresh_pending(),
        });
        Self {
            language: app.ui_language(),
            activity,
            elapsed,
            model,
            permission,
            context,
        }
    }
}

pub(crate) fn render_session_header(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &TuiApp,
    theme: CrabCodeTheme,
) {
    let model = SessionHeaderViewModel::from_app(app, Instant::now());
    render_header_model(frame, area, &model, theme);
}

fn render_header_model(
    frame: &mut Frame<'_>,
    area: Rect,
    model: &SessionHeaderViewModel,
    theme: CrabCodeTheme,
) {
    if area.is_empty() {
        return;
    }
    let base = Style::default().bg(theme.bg_base);
    frame.render_widget(Block::default().style(base), area);
    let row = header_status_row(model, theme, usize::from(area.width));
    frame.render_widget(Paragraph::new(row).style(base), Rect { height: 1, ..area });
    if area.height > 1 {
        frame.render_widget(
            Paragraph::new(Line::styled(
                "─".repeat(usize::from(area.width)),
                Style::default().fg(theme.gray_dim).bg(theme.bg_base),
            )),
            Rect::new(area.x, area.y.saturating_add(1), area.width, 1),
        );
    }
}

fn header_status_row(
    model: &SessionHeaderViewModel,
    theme: CrabCodeTheme,
    width: usize,
) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    if model.permission.dangerous
        && width < 60
        && !matches!(
            model.activity,
            HeaderActivity::Fatal | HeaderActivity::WaitingForUser { .. }
        )
    {
        return fit_line_to_width(
            Line::from(vec![
                Span::styled(
                    " CrabCode  ",
                    Style::default()
                        .fg(theme.accent_assistant)
                        .bg(theme.bg_base)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "{} {}",
                        diamond_filled(),
                        model.language.text("高风险免审", "HIGH-RISK MODE")
                    ),
                    Style::default()
                        .fg(theme.accent_error)
                        .bg(theme.bg_base)
                        .add_modifier(Modifier::BOLD),
                ),
            ])
            .style(Style::default().bg(theme.bg_base)),
            width,
        );
    }
    let compact = width < 72;
    let mut left = vec![Span::styled(
        " CrabCode",
        Style::default()
            .fg(theme.accent_assistant)
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
    )];
    if width >= 100 {
        left.push(Span::styled(
            format!(" v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(theme.gray).bg(theme.bg_base),
        ));
    }
    let (symbol, label, color) = activity_presentation(model, theme, compact);
    left.push(Span::styled("  ", Style::default().bg(theme.bg_base)));
    left.push(Span::styled(
        format!("{symbol} "),
        Style::default().fg(color).bg(theme.bg_base),
    ));
    left.push(Span::styled(
        label,
        Style::default()
            .fg(
                if matches!(
                    model.activity,
                    HeaderActivity::Fatal | HeaderActivity::Failed
                ) {
                    theme.accent_error
                } else {
                    theme.text_primary
                },
            )
            .bg(theme.bg_base)
            .add_modifier(Modifier::BOLD),
    ));
    if let Some(elapsed) = model.elapsed
        && should_show_elapsed(&model.activity)
    {
        left.push(Span::styled(
            format!(" · {}", format_duration(elapsed, compact)),
            Style::default().fg(theme.gray).bg(theme.bg_base),
        ));
    }

    let right = metadata_spans(model, theme, width);
    compose_priority_row(left, right, width, theme)
}

fn activity_presentation(
    model: &SessionHeaderViewModel,
    theme: CrabCodeTheme,
    compact: bool,
) -> (&'static str, String, ratatui::style::Color) {
    let language = model.language;
    match &model.activity {
        HeaderActivity::Fatal => (
            ballot_x(),
            language.text("协议已停止", "Protocol stopped").to_string(),
            theme.accent_error,
        ),
        HeaderActivity::WaitingForUser { subject } => {
            let base = language.text("等待确认", "Action needed");
            let label = subject.as_ref().map_or_else(
                || base.to_string(),
                |subject| {
                    format!(
                        "{base} · {}",
                        middle_ellipsis(subject, if compact { 10 } else { 18 })
                    )
                },
            );
            (diamond_filled(), label, theme.warning)
        }
        HeaderActivity::Cancelling => (
            record_dot(false),
            language.text("正在取消", "Cancelling").to_string(),
            theme.text_secondary,
        ),
        HeaderActivity::Requesting => (
            record_dot(true),
            language
                .text(
                    if compact {
                        "请求中"
                    } else {
                        "正在请求模型"
                    },
                    if compact {
                        "Requesting"
                    } else {
                        "Requesting model"
                    },
                )
                .to_string(),
            theme.accent_running,
        ),
        HeaderActivity::Thinking => (
            record_dot(true),
            language
                .text(if compact { "思考中" } else { "正在思考" }, "Thinking")
                .to_string(),
            theme.accent_running,
        ),
        HeaderActivity::Responding => (
            record_dot(true),
            language.text("正在回复", "Responding").to_string(),
            theme.accent_running,
        ),
        HeaderActivity::ToolInput { tool_name } => (
            record_dot(true),
            activity_with_subject(
                language.text("正在准备工具", "Preparing tool"),
                tool_name.as_deref(),
                compact,
            ),
            theme.accent_running,
        ),
        HeaderActivity::ToolUse { tool_name } => (
            record_dot(true),
            activity_with_subject(
                language.text("正在运行工具", "Running tool"),
                tool_name.as_deref(),
                compact,
            ),
            theme.accent_running,
        ),
        HeaderActivity::WaitingForModel => (
            record_dot(true),
            language.text("等待模型", "Waiting for model").to_string(),
            theme.accent_running,
        ),
        HeaderActivity::WaitingForSubagents { count } => (
            record_dot(true),
            match (language, count) {
                (UiLanguage::ZhCn, 0) => "等待子代理".to_string(),
                (UiLanguage::ZhCn, count) => format!("等待子代理 · {count}"),
                (UiLanguage::EnUs, 0) => "Waiting for subagents".to_string(),
                (UiLanguage::EnUs, count) => format!("Waiting for subagents · {count}"),
            },
            theme.accent_running,
        ),
        HeaderActivity::WaitingForTask { count } => (
            record_dot(true),
            match (language, count) {
                (UiLanguage::ZhCn, 0) => "等待后台任务".to_string(),
                (UiLanguage::ZhCn, count) => format!("后台任务 · {count}"),
                (UiLanguage::EnUs, 0) => "Waiting for background work".to_string(),
                (UiLanguage::EnUs, count) => format!("Background work · {count}"),
            },
            theme.accent_running,
        ),
        HeaderActivity::Retrying => (
            record_dot(true),
            language.text("正在重试", "Retrying").to_string(),
            theme.accent_running,
        ),
        HeaderActivity::Initializing => (
            record_dot(true),
            language.text("初始化中", "Initializing").to_string(),
            theme.accent_running,
        ),
        HeaderActivity::Failed => (
            ballot_x(),
            language.text("执行失败", "Turn failed").to_string(),
            theme.accent_error,
        ),
        HeaderActivity::Cancelled => (
            record_dot(false),
            language.text("已取消", "Cancelled").to_string(),
            theme.text_secondary,
        ),
        HeaderActivity::Completed => (
            check_mark(),
            language.text("本轮完成", "Turn complete").to_string(),
            theme.accent_success,
        ),
        HeaderActivity::Ready => (
            "●",
            language.text("已就绪", "Ready").to_string(),
            theme.accent_success,
        ),
    }
}

fn activity_with_subject(base: &str, subject: Option<&str>, compact: bool) -> String {
    subject.map_or_else(
        || base.to_string(),
        |subject| {
            format!(
                "{base} · {}",
                middle_ellipsis(subject, if compact { 9 } else { 18 })
            )
        },
    )
}

fn metadata_spans(
    model: &SessionHeaderViewModel,
    theme: CrabCodeTheme,
    width: usize,
) -> Vec<Span<'static>> {
    let mut components: Vec<(String, Style)> = Vec::new();
    let normal = Style::default().fg(theme.text_secondary).bg(theme.bg_base);
    let dim = Style::default().fg(theme.gray).bg(theme.bg_base);
    let danger = Style::default()
        .fg(theme.bg_base)
        .bg(theme.accent_error)
        .add_modifier(Modifier::BOLD);
    if model.permission.dangerous {
        if width >= 100
            && let Some(active) = model.model.as_deref()
        {
            components.push((middle_ellipsis(active, 24), normal));
        }
        components.push((
            model
                .language
                .text(" 高风险免审 ", " HIGH-RISK AUTO-APPROVAL ")
                .to_string(),
            danger,
        ));
        if width >= 100
            && let Some(context) = model.context
        {
            components.push((context_label(context, model.language, true), dim));
        }
    } else if width >= 72
        && let Some(permission) = model.permission.label.as_deref()
    {
        let permission = if width >= 100 {
            permission.to_string()
        } else {
            compact_permission_label(permission, model.language).to_string()
        };
        components.push((permission, dim));
    }
    if !model.permission.dangerous {
        if let Some(active) = model.model.as_deref() {
            let max = if width >= 100 { 24 } else { 18 };
            components.insert(0, (middle_ellipsis(active, max), normal));
        }
        if let Some(context) = model.context {
            if width >= 100 {
                components.push((context_label(context, model.language, true), dim));
            } else if width >= 72 {
                components.push((format!("{}%", context.percentage), dim));
            }
        }
    }
    let mut spans = Vec::new();
    for (index, (text, style)) in components.into_iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled(
                " · ",
                Style::default().fg(theme.gray_dim).bg(theme.bg_base),
            ));
        }
        spans.push(Span::styled(text, style));
    }
    spans.push(Span::styled(" ", Style::default().bg(theme.bg_base)));
    spans
}

fn compose_priority_row(
    left: Vec<Span<'static>>,
    mut right: Vec<Span<'static>>,
    width: usize,
    theme: CrabCodeTheme,
) -> Line<'static> {
    let base = Style::default().bg(theme.bg_base);
    if right.is_empty() {
        return fit_line_to_width(Line::from(left).style(base), width);
    }
    let minimum_left = width.min(28);
    let max_right = width.saturating_sub(minimum_left);
    let right_line = Line::from(right).style(base);
    right = if right_line.width() > max_right {
        fit_line_to_width(right_line, max_right).spans
    } else {
        right_line.spans
    };
    let right_width = right.iter().map(|span| span.content.width()).sum::<usize>();
    let left_width = width.saturating_sub(right_width);
    let mut line = fit_line_to_width(Line::from(left).style(base), left_width);
    line.spans.extend(right);
    line.style = base;
    line
}

fn should_show_elapsed(activity: &HeaderActivity) -> bool {
    !matches!(
        activity,
        HeaderActivity::Fatal
            | HeaderActivity::WaitingForUser { .. }
            | HeaderActivity::Initializing
            | HeaderActivity::Ready
    )
}

fn context_label(context: ContextPresentation, language: UiLanguage, detailed: bool) -> String {
    if context.pending && context.max_tokens == 0 {
        return language
            .text("上下文计算中…", "context calculating…")
            .to_string();
    }
    let label = if context.baseline {
        language.text("基础上下文", "baseline context")
    } else {
        language.text("上下文", "context")
    };
    if detailed {
        format!(
            "{label} {}/{} · {}%",
            format_tokens(context.total_tokens),
            format_tokens(context.max_tokens),
            context.percentage
        )
    } else {
        format!("{label} {}%", context.percentage)
    }
}

fn compact_permission_label(label: &str, language: UiLanguage) -> &str {
    match language {
        UiLanguage::ZhCn if label == "标准审批" => "标准",
        UiLanguage::ZhCn if label == "自动接受编辑" => "自动编辑",
        UiLanguage::EnUs if label == "standard" => "standard",
        UiLanguage::EnUs if label == "accept edits" => "auto edits",
        _ => label,
    }
}

fn format_tokens(tokens: u64) -> String {
    let (value, suffix) = if tokens >= 1_000_000 {
        (tokens as f64 / 1_000_000.0, "m")
    } else if tokens >= 1_000 {
        (tokens as f64 / 1_000.0, "k")
    } else {
        return tokens.to_string();
    };
    let mut formatted = format!("{value:.1}");
    if formatted.ends_with(".0") {
        formatted.truncate(formatted.len().saturating_sub(2));
    }
    format!("{formatted}{suffix}")
}

fn format_duration(duration: Duration, compact: bool) -> String {
    let seconds = duration.as_secs();
    if compact {
        if seconds < 60 {
            return format!("{seconds}s");
        }
        return format!("{}m{:02}s", seconds / 60, seconds % 60);
    }
    format!("{:02}:{:02}", seconds / 60, seconds % 60)
}

fn clean_subject(raw: &str) -> String {
    sanitize_bounded_terminal_text(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use super::*;

    fn model(activity: HeaderActivity, language: UiLanguage) -> SessionHeaderViewModel {
        SessionHeaderViewModel {
            language,
            activity,
            elapsed: Some(Duration::from_secs(72)),
            model: Some("DeepSeek-v4-Flash-Long-Identity".to_string()),
            permission: PermissionPresentation {
                label: Some(language.text("标准审批", "standard").to_string()),
                dangerous: false,
            },
            context: Some(ContextPresentation {
                total_tokens: 12_345,
                max_tokens: 200_000,
                percentage: 8,
                baseline: false,
                pending: false,
            }),
        }
    }

    fn render_buffer(model: &SessionHeaderViewModel, width: u16, theme: CrabCodeTheme) -> Buffer {
        let backend = TestBackend::new(width, 2);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| render_header_model(frame, frame.area(), model, theme))
            .expect("header draw");
        terminal.backend().buffer().clone()
    }

    fn render_text(model: &SessionHeaderViewModel, width: u16) -> Vec<String> {
        let buffer = render_buffer(model, width, CrabCodeTheme::dark());
        (0..2)
            .map(|row| {
                (0..width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn semantic_header_palette_covers_every_concrete_theme() {
        for theme in [
            CrabCodeTheme::dark(),
            CrabCodeTheme::light(),
            CrabCodeTheme::dark_daltonized(),
            CrabCodeTheme::light_daltonized(),
            CrabCodeTheme::dark_ansi(),
            CrabCodeTheme::light_ansi(),
        ] {
            let buffer = render_buffer(&model(HeaderActivity::Ready, UiLanguage::EnUs), 80, theme);
            assert!(buffer.content().iter().all(|cell| cell.bg == theme.bg_base));
            assert!(
                buffer
                    .content()
                    .iter()
                    .any(|cell| cell.symbol() == "●" && cell.fg == theme.accent_success)
            );
        }
    }

    #[test]
    fn fixed_two_rows_keep_status_and_divider_at_all_supported_widths() {
        for width in [120, 80, 60] {
            let rows = render_text(&model(HeaderActivity::Thinking, UiLanguage::ZhCn), width);
            let status = rows[0].replace(' ', "");
            assert!(rows[0].contains("CrabCode"));
            assert!(status.contains("思考"), "{}", rows[0]);
            assert!(rows[1].chars().all(|character| character == '─'));
        }
    }

    #[test]
    fn dangerous_permission_survives_narrow_metadata_reduction() {
        let mut dangerous = model(HeaderActivity::Ready, UiLanguage::ZhCn);
        dangerous.permission.dangerous = true;
        let rows = render_text(&dangerous, 60);
        assert!(
            rows[0].replace(' ', "").contains("高风险免审"),
            "{:?}",
            rows[0]
        );
        assert!(!rows[0].contains("DeepSeek"));

        let very_narrow = render_text(&dangerous, 40);
        assert!(
            very_narrow[0].replace(' ', "").contains("高风险免审"),
            "{:?}",
            very_narrow[0]
        );
    }

    #[test]
    fn activity_labels_cover_typed_terminal_outcomes_without_status_copy() {
        for (activity, expected) in [
            (HeaderActivity::Failed, "执行失败"),
            (HeaderActivity::Cancelled, "已取消"),
            (HeaderActivity::Completed, "本轮完成"),
            (
                HeaderActivity::ToolUse {
                    tool_name: Some("Bash".to_string()),
                },
                "Bash",
            ),
        ] {
            let rows = render_text(&model(activity, UiLanguage::ZhCn), 120);
            assert!(rows[0].replace(' ', "").contains(expected), "{:?}", rows[0]);
        }
    }

    #[test]
    fn waiting_reason_without_a_watcher_count_does_not_invent_one() {
        for (activity, expected) in [
            (
                HeaderActivity::WaitingForSubagents { count: 0 },
                "等待子代理",
            ),
            (HeaderActivity::WaitingForTask { count: 0 }, "等待后台任务"),
        ] {
            let view_model = model(activity, UiLanguage::ZhCn);
            let (_, label, _) = activity_presentation(&view_model, CrabCodeTheme::dark(), false);
            assert_eq!(label, expected);
        }
    }

    #[test]
    fn typed_header_derivation_never_reads_free_form_status_copy() {
        let source = include_str!("session_header.rs");
        let start = source
            .find("pub(crate) fn from_app")
            .expect("typed view-model derivation");
        let end = source[start..]
            .find("pub(crate) fn render_session_header")
            .map(|offset| start + offset)
            .expect("end of derivation");
        assert!(!source[start..end].contains("app.status"));
        assert!(
            !source[start..end].contains("ProjectedKind::ToolUse"),
            "a completed historical tool must not be reused as the active tool subject"
        );
    }
}

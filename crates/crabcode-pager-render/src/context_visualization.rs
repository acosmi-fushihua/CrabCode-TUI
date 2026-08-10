//! Renderer-only presentation of the existing `get_context_usage` response.
//!
//! The terminal lifecycle, modal ownership, input routing, and repaint loop
//! remain those of the fixed Rust TUI mother implementation. This module ports
//! only the product presentation that the fixed historical CrabCode direct TUI
//! applied to its already-computed context data.
//!
//! Fixed product source:
//! - commit: `2358212c2df2018816058c8a03b1ac3d324e74e0`
//! - `src/components/ContextVisualization.tsx`
//! - `src/utils/contextSuggestions.ts`
//! - `src/entrypoints/sdk/controlSchemas.ts`
//!
//! No backend state or protocol authority lives here. In particular, parsing
//! consumes the unchanged `control_response.response` object and rejects
//! unknown historical color/source roles instead of inventing a rendering.

use std::cmp::Ordering;
use std::path::Path;

use crate::audited_theme::{
    CrabCodeTheme, CrabCodeThemeKind,
    color_support::{self, ColorLevel},
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::{Map, Value};

use crate::appearance::RendererLanguage;
use crate::text_safety::sanitize_bounded_terminal_text;

const FREE_SPACE: &str = "Free space";
const AUTOCOMPACT_BUFFER: &str = "Autocompact buffer";
const SOURCE_DISPLAY_ORDER: [ContextSource; 5] = [
    ContextSource::Project,
    ContextSource::User,
    ContextSource::Managed,
    ContextSource::Plugin,
    ContextSource::BuiltIn,
];

const LARGE_TOOL_RESULT_PERCENT: f64 = 15.0;
const LARGE_TOOL_RESULT_TOKENS: u64 = 10_000;
const READ_BLOAT_PERCENT: f64 = 5.0;
const NEAR_CAPACITY_PERCENT: u64 = 80;
const MEMORY_HIGH_PERCENT: f64 = 5.0;
const MEMORY_HIGH_TOKENS: u64 = 5_000;
const JAVASCRIPT_MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextColorRole {
    PromptBorder,
    Inactive,
    Cyan,
    Permission,
    CrabCode,
    Warning,
    Purple,
}

impl ContextColorRole {
    fn parse(value: &str, path: &str) -> Result<Self, String> {
        match value {
            "promptBorder" => Ok(Self::PromptBorder),
            "inactive" => Ok(Self::Inactive),
            "cyan_FOR_SUBAGENTS_ONLY" => Ok(Self::Cyan),
            "permission" => Ok(Self::Permission),
            "crabCode" => Ok(Self::CrabCode),
            "warning" => Ok(Self::Warning),
            "purple_FOR_SUBAGENTS_ONLY" => Ok(Self::Purple),
            unknown => Err(format!(
                "{path} has unknown historical theme role `{unknown}`"
            )),
        }
    }

    fn color(self, theme: CrabCodeTheme, kind: CrabCodeThemeKind, level: ColorLevel) -> Color {
        let exact_product_color = match self {
            Self::PromptBorder => return theme.prompt_border,
            Self::Inactive => return theme.text_secondary,
            Self::CrabCode => return theme.accent_assistant,
            Self::Warning => return theme.warning,
            Self::Permission => match kind {
                CrabCodeThemeKind::Light => Color::Rgb(87, 105, 247),
                CrabCodeThemeKind::Dark => Color::Rgb(177, 185, 249),
                CrabCodeThemeKind::LightDaltonized => Color::Rgb(51, 102, 255),
                CrabCodeThemeKind::DarkDaltonized => Color::Rgb(153, 204, 255),
                CrabCodeThemeKind::LightAnsi => Color::Blue,
                CrabCodeThemeKind::DarkAnsi => Color::LightBlue,
                CrabCodeThemeKind::Auto => {
                    // `Auto` is resolved before a frame is painted. Keep this
                    // branch deterministic if an invalid caller bypasses that
                    // lifecycle invariant.
                    Color::Rgb(177, 185, 249)
                }
            },
            Self::Cyan => match kind {
                CrabCodeThemeKind::Light | CrabCodeThemeKind::Dark => Color::Rgb(8, 145, 178),
                CrabCodeThemeKind::LightDaltonized => Color::Rgb(0, 178, 178),
                CrabCodeThemeKind::DarkDaltonized => Color::Rgb(102, 204, 204),
                CrabCodeThemeKind::LightAnsi => Color::Cyan,
                CrabCodeThemeKind::DarkAnsi => Color::LightCyan,
                CrabCodeThemeKind::Auto => Color::Rgb(8, 145, 178),
            },
            Self::Purple => match kind {
                CrabCodeThemeKind::Light | CrabCodeThemeKind::Dark => Color::Rgb(147, 51, 234),
                CrabCodeThemeKind::LightDaltonized => Color::Rgb(128, 0, 128),
                CrabCodeThemeKind::DarkDaltonized => Color::Rgb(178, 102, 255),
                CrabCodeThemeKind::LightAnsi => Color::Magenta,
                CrabCodeThemeKind::DarkAnsi => Color::LightMagenta,
                CrabCodeThemeKind::Auto => Color::Rgb(147, 51, 234),
            },
        };
        color_support::quantize_color(exact_product_color, level)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextSource {
    User,
    Project,
    Local,
    Flag,
    Managed,
    Plugin,
    BuiltIn,
}

impl ContextSource {
    fn parse(value: &str, path: &str) -> Result<Self, String> {
        match value {
            "userSettings" => Ok(Self::User),
            "projectSettings" => Ok(Self::Project),
            "localSettings" => Ok(Self::Local),
            "flagSettings" => Ok(Self::Flag),
            "policySettings" => Ok(Self::Managed),
            "plugin" => Ok(Self::Plugin),
            // The unchanged CrabCode direct runtime uses `bundled` for
            // first-party bundled skill commands and `built-in` for the
            // historical built-in source. Both are renderer-equivalent
            // first-party sources; accepting the runtime's existing value
            // here does not add or rewrite a protocol field.
            "built-in" | "bundled" => Ok(Self::BuiltIn),
            unknown => Err(format!("{path} has unknown setting source `{unknown}`")),
        }
    }

    const fn display_name(self, language: RendererLanguage) -> &'static str {
        match self {
            Self::User => language.text("用户", "User"),
            Self::Project => language.text("项目", "Project"),
            Self::Local => language.text("本地", "Local"),
            Self::Flag => language.text("标志", "Flag"),
            Self::Managed => language.text("托管", "Managed"),
            Self::Plugin => language.text("插件", "Plugin"),
            Self::BuiltIn => language.text("内置", "Built-in"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextCategory {
    name: String,
    tokens: u64,
    color: ContextColorRole,
    is_deferred: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextGridSquare {
    color: ContextColorRole,
    category_name: String,
    is_full: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MemoryFile {
    path: String,
    tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct McpTool {
    name: String,
    tokens: u64,
    is_loaded: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourcedUsage {
    name: String,
    source: ContextSource,
    tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SkillUsage {
    tokens: u64,
    entries: Vec<SourcedUsage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolUsage {
    name: String,
    call_tokens: u64,
    result_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageBreakdown {
    tools: Vec<ToolUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuggestionSeverity {
    Info,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ContextSuggestion {
    severity: SuggestionSeverity,
    title: String,
    detail: String,
    savings_tokens: Option<u64>,
}

/// Parsed renderer projection of the existing context-usage response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextVisualization {
    categories: Vec<ContextCategory>,
    total_tokens: u64,
    raw_max_tokens: u64,
    percentage: u64,
    grid_rows: Vec<Vec<ContextGridSquare>>,
    model: String,
    memory_files: Vec<MemoryFile>,
    mcp_tools: Vec<McpTool>,
    agents: Vec<SourcedUsage>,
    skills: Option<SkillUsage>,
    is_auto_compact_enabled: bool,
    message_breakdown: Option<MessageBreakdown>,
}

impl ContextVisualization {
    /// Parse only the existing successful control-response envelope.
    pub fn from_control_response(payload: &Value) -> Result<Self, String> {
        let payload = required_object(payload, "control_response")?;
        let response = required_object_field(payload, "response", "control_response")?;
        Self::from_response_object(response)
    }

    fn from_response_object(response: &Map<String, Value>) -> Result<Self, String> {
        let categories = required_array_field(response, "categories", "response")?
            .iter()
            .enumerate()
            .map(|(index, value)| parse_category(value, index))
            .collect::<Result<Vec<_>, _>>()?;
        let total_tokens = required_u64_field(response, "totalTokens", "response")?;
        let _max_tokens = required_u64_field(response, "maxTokens", "response")?;
        let raw_max_tokens = required_u64_field(response, "rawMaxTokens", "response")?;
        if raw_max_tokens == 0 {
            return Err("response.rawMaxTokens must be greater than zero".to_string());
        }
        let percentage = required_u64_field(response, "percentage", "response")?;
        let grid_rows = required_array_field(response, "gridRows", "response")?
            .iter()
            .enumerate()
            .map(|(row_index, row)| {
                required_array(row, &format!("response.gridRows[{row_index}]"))?
                    .iter()
                    .enumerate()
                    .map(|(column_index, square)| {
                        parse_grid_square(square, row_index, column_index)
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .collect::<Result<Vec<_>, _>>()?;
        let model = required_string_field(response, "model", "response")?.to_string();
        let memory_files = required_array_field(response, "memoryFiles", "response")?
            .iter()
            .enumerate()
            .map(|(index, value)| parse_memory_file(value, index))
            .collect::<Result<Vec<_>, _>>()?;
        let mcp_tools = required_array_field(response, "mcpTools", "response")?
            .iter()
            .enumerate()
            .map(|(index, value)| parse_mcp_tool(value, index))
            .collect::<Result<Vec<_>, _>>()?;
        validate_optional_named_token_list(response, "deferredBuiltinTools", true)?;
        validate_optional_named_token_list(response, "systemTools", false)?;
        validate_optional_named_token_list(response, "systemPromptSections", false)?;
        let agents = required_array_field(response, "agents", "response")?
            .iter()
            .enumerate()
            .map(|(index, value)| parse_sourced_usage(value, index, "agents", "agentType"))
            .collect::<Result<Vec<_>, _>>()?;
        validate_optional_slash_commands(response)?;
        let skills = parse_optional_skills(response)?;
        validate_optional_u64_field(response, "autoCompactThreshold", "response")?;
        let is_auto_compact_enabled =
            required_bool_field(response, "isAutoCompactEnabled", "response")?;
        let message_breakdown = parse_optional_message_breakdown(response)?;
        validate_api_usage(response)?;

        Ok(Self {
            categories,
            total_tokens,
            raw_max_tokens,
            percentage,
            grid_rows,
            model,
            memory_files,
            mcp_tools,
            agents,
            skills,
            is_auto_compact_enabled,
            message_breakdown,
        })
    }

    /// Build source-shaped, terminal-safe styled rows for the mother modal.
    pub fn styled_lines(
        &self,
        theme: CrabCodeTheme,
        theme_kind: CrabCodeThemeKind,
        color_level: ColorLevel,
        language: RendererLanguage,
    ) -> Vec<Line<'static>> {
        let mut legend = self.legend_lines(theme, theme_kind, color_level, language);
        let mut lines = Vec::new();
        let grid_width = self
            .grid_rows
            .iter()
            .map(|row| row.len().saturating_mul(2))
            .max()
            .unwrap_or(0);
        let paired_rows = self.grid_rows.len().max(legend.len());

        for row_index in 0..paired_rows {
            let mut spans = Vec::new();
            if let Some(row) = self.grid_rows.get(row_index) {
                for square in row {
                    let (glyph, color) = if square.category_name == FREE_SPACE {
                        ("⛶ ", theme.text_secondary)
                    } else if square.category_name == AUTOCOMPACT_BUFFER {
                        ("⛝ ", square.color.color(theme, theme_kind, color_level))
                    } else {
                        (
                            if square.is_full { "⛁ " } else { "⛀ " },
                            square.color.color(theme, theme_kind, color_level),
                        )
                    };
                    spans.push(Span::styled(glyph, Style::default().fg(color)));
                }
                let row_width = row.len().saturating_mul(2);
                if row_width < grid_width {
                    spans.push(Span::raw(" ".repeat(grid_width - row_width)));
                }
            } else if grid_width > 0 {
                spans.push(Span::raw(" ".repeat(grid_width)));
            }

            if let Some(legend_line) = legend.get_mut(row_index) {
                if grid_width > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.append(&mut legend_line.spans);
            }
            lines.push(Line::from(spans));
        }

        self.append_sections(&mut lines, theme, theme_kind, color_level, language);
        lines
    }

    pub fn line_count(&self) -> usize {
        self.styled_lines(
            CrabCodeTheme::NIGHT,
            CrabCodeThemeKind::Dark,
            ColorLevel::TrueColor,
            RendererLanguage::default(),
        )
        .len()
    }

    /// Active model reported by the existing direct-mode response.
    ///
    /// This is a searchable/rendering projection only; it does not create a
    /// second backend model or protocol field.
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Tokens currently occupying the active model context.
    pub const fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    /// Runtime-reported hard context-window size before presentation reserves.
    pub const fn raw_max_tokens(&self) -> u64 {
        self.raw_max_tokens
    }

    /// Runtime-computed percentage of the context window currently in use.
    pub const fn percentage(&self) -> u64 {
        self.percentage
    }

    fn legend_lines(
        &self,
        theme: CrabCodeTheme,
        theme_kind: CrabCodeThemeKind,
        color_level: ColorLevel,
        language: RendererLanguage,
    ) -> Vec<Line<'static>> {
        let primary = Style::default().fg(theme.text_primary);
        let secondary = Style::default().fg(theme.text_secondary);
        let mut lines = vec![
            Line::styled(
                match language {
                    RendererLanguage::ZhCn => format!(
                        "{} · {}/{} 词元（{}%）",
                        safe_inline(&self.model),
                        format_tokens(self.total_tokens),
                        format_tokens(self.raw_max_tokens),
                        self.percentage
                    ),
                    RendererLanguage::EnUs => format!(
                        "{} · {}/{} tokens ({}%)",
                        safe_inline(&self.model),
                        format_tokens(self.total_tokens),
                        format_tokens(self.raw_max_tokens),
                        self.percentage
                    ),
                },
                secondary,
            ),
            Line::default(),
            Line::styled(
                language.text("按类别估算的用量", "Estimated usage by category"),
                secondary.add_modifier(Modifier::ITALIC),
            ),
        ];

        for category in self.categories.iter().filter(|category| {
            category.tokens > 0
                && category.name != FREE_SPACE
                && category.name != AUTOCOMPACT_BUFFER
                && !category.is_deferred
        }) {
            lines.push(Line::from(vec![
                Span::styled(
                    "⛁",
                    Style::default().fg(category.color.color(theme, theme_kind, color_level)),
                ),
                Span::styled(format!(" {}: ", safe_inline(&category.name)), primary),
                Span::styled(
                    match language {
                        RendererLanguage::ZhCn => format!(
                            "{} 词元（{:.1}%）",
                            format_tokens(category.tokens),
                            percent(category.tokens, self.raw_max_tokens)
                        ),
                        RendererLanguage::EnUs => format!(
                            "{} tokens ({:.1}%)",
                            format_tokens(category.tokens),
                            percent(category.tokens, self.raw_max_tokens)
                        ),
                    },
                    secondary,
                ),
            ]));
        }

        if let Some(free_space) = self
            .categories
            .iter()
            .find(|category| category.name == FREE_SPACE && category.tokens > 0)
        {
            lines.push(Line::from(vec![
                Span::styled("⛶", secondary),
                Span::styled(language.text(" 可用空间：", " Free space: "), primary),
                Span::styled(
                    match language {
                        RendererLanguage::ZhCn => format!(
                            "{}（{:.1}%）",
                            format_tokens(free_space.tokens),
                            percent(free_space.tokens, self.raw_max_tokens)
                        ),
                        RendererLanguage::EnUs => format!(
                            "{} ({:.1}%)",
                            format_tokens(free_space.tokens),
                            percent(free_space.tokens, self.raw_max_tokens)
                        ),
                    },
                    secondary,
                ),
            ]));
        }

        if let Some(autocompact) = self
            .categories
            .iter()
            .find(|category| category.name == AUTOCOMPACT_BUFFER && category.tokens > 0)
        {
            lines.push(Line::from(vec![
                Span::styled(
                    "⛝",
                    Style::default().fg(autocompact.color.color(theme, theme_kind, color_level)),
                ),
                Span::styled(
                    match language {
                        RendererLanguage::ZhCn => format!(
                            " 自动压缩缓冲区：{} 词元（{:.1}%）",
                            format_tokens(autocompact.tokens),
                            percent(autocompact.tokens, self.raw_max_tokens)
                        ),
                        RendererLanguage::EnUs => format!(
                            " {}: {} tokens ({:.1}%)",
                            safe_inline(&autocompact.name),
                            format_tokens(autocompact.tokens),
                            percent(autocompact.tokens, self.raw_max_tokens)
                        ),
                    },
                    secondary,
                ),
            ]));
        }

        lines
    }

    fn append_sections(
        &self,
        lines: &mut Vec<Line<'static>>,
        theme: CrabCodeTheme,
        theme_kind: CrabCodeThemeKind,
        color_level: ColorLevel,
        language: RendererLanguage,
    ) {
        let primary = Style::default().fg(theme.text_primary);
        let secondary = Style::default().fg(theme.text_secondary);
        let bold = primary.add_modifier(Modifier::BOLD);
        let has_deferred_mcp_tools = self
            .categories
            .iter()
            .any(|category| category.is_deferred && category.name.contains("MCP"));

        if !self.mcp_tools.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled(language.text("MCP 工具", "MCP tools"), bold),
                Span::styled(
                    if has_deferred_mcp_tools {
                        language.text(" · /mcp 按需加载", " · /mcp loaded on demand")
                    } else {
                        " · /mcp"
                    },
                    secondary,
                ),
            ]));
            if has_deferred_mcp_tools {
                let loaded = self
                    .mcp_tools
                    .iter()
                    .filter(|tool| tool.is_loaded == Some(true))
                    .collect::<Vec<_>>();
                if !loaded.is_empty() {
                    lines.push(Line::default());
                    lines.push(Line::styled(language.text("已加载", "Loaded"), secondary));
                    for tool in loaded {
                        lines.push(token_usage_line(
                            &tool.name,
                            tool.tokens,
                            primary,
                            secondary,
                            language,
                        ));
                    }
                }
                let available = self
                    .mcp_tools
                    .iter()
                    .filter(|tool| tool.is_loaded != Some(true))
                    .collect::<Vec<_>>();
                if !available.is_empty() {
                    lines.push(Line::default());
                    lines.push(Line::styled(language.text("可用", "Available"), secondary));
                    for tool in available {
                        lines.push(Line::styled(
                            format!("└ {}", safe_inline(&tool.name)),
                            secondary,
                        ));
                    }
                }
            } else {
                for tool in &self.mcp_tools {
                    lines.push(token_usage_line(
                        &tool.name,
                        tool.tokens,
                        primary,
                        secondary,
                        language,
                    ));
                }
            }
        }

        if !self.agents.is_empty() {
            append_sourced_section(
                lines,
                language.text("自定义代理", "Custom agents"),
                " · /agents",
                &self.agents,
                primary,
                secondary,
                language,
            );
        }

        if !self.memory_files.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(vec![
                Span::styled(language.text("记忆文件", "Memory files"), bold),
                Span::styled(" · /memory", secondary),
            ]));
            for file in &self.memory_files {
                lines.push(token_usage_line(
                    &display_path(&file.path),
                    file.tokens,
                    primary,
                    secondary,
                    language,
                ));
            }
        }

        if let Some(skills) = self.skills.as_ref().filter(|skills| skills.tokens > 0) {
            append_sourced_section(
                lines,
                language.text("技能", "Skills"),
                " · /skills",
                &skills.entries,
                primary,
                secondary,
                language,
            );
        }

        let suggestions = self.suggestions(language);
        if !suggestions.is_empty() {
            lines.push(Line::default());
            lines.push(Line::styled(language.text("建议", "Suggestions"), bold));
            for (index, suggestion) in suggestions.into_iter().enumerate() {
                if index > 0 {
                    lines.push(Line::default());
                }
                let icon_style = match suggestion.severity {
                    SuggestionSeverity::Warning => Style::default().fg(theme.warning),
                    SuggestionSeverity::Info => Style::default()
                        .fg(ContextColorRole::Permission.color(theme, theme_kind, color_level)),
                };
                let mut title = vec![
                    Span::styled(
                        match suggestion.severity {
                            SuggestionSeverity::Warning => "⚠ ",
                            SuggestionSeverity::Info => "ℹ ",
                        },
                        icon_style,
                    ),
                    Span::styled(safe_inline(&suggestion.title), bold),
                ];
                if let Some(tokens) = suggestion.savings_tokens.filter(|tokens| *tokens > 0) {
                    title.push(Span::styled(
                        match language {
                            RendererLanguage::ZhCn => {
                                format!(" → 可节省约 {}", format_tokens(tokens))
                            }
                            RendererLanguage::EnUs => {
                                format!(" → save ~{}", format_tokens(tokens))
                            }
                        },
                        secondary,
                    ));
                }
                lines.push(Line::from(title));
                lines.push(Line::styled(
                    format!("  {}", safe_inline(&suggestion.detail)),
                    secondary,
                ));
            }
        }
    }

    fn suggestions(&self, language: RendererLanguage) -> Vec<ContextSuggestion> {
        let mut suggestions = Vec::new();
        if self.percentage >= NEAR_CAPACITY_PERCENT {
            suggestions.push(ContextSuggestion {
                severity: SuggestionSeverity::Warning,
                title: match language {
                    RendererLanguage::ZhCn => {
                        format!("上下文已使用 {}%", self.percentage)
                    }
                    RendererLanguage::EnUs => {
                        format!("Context is {}% full", self.percentage)
                    }
                },
                detail: if self.is_auto_compact_enabled {
                    language
                        .text(
                            "即将触发自动压缩，这会丢弃较早的消息。现在使用 /compact 可控制保留的内容。",
                            "Autocompact will trigger soon, which discards older messages. Use /compact now to control what gets kept.",
                        )
                        .to_string()
                } else {
                    language
                        .text(
                            "自动压缩已关闭。使用 /compact 释放空间，或在 /config 中启用自动压缩。",
                            "Autocompact is disabled. Use /compact to free space, or enable autocompact in /config.",
                        )
                        .to_string()
                },
                savings_tokens: None,
            });
        }

        if let Some(breakdown) = &self.message_breakdown {
            for tool in &breakdown.tools {
                let total = tool.call_tokens.saturating_add(tool.result_tokens);
                let usage_percent = percent(total, self.raw_max_tokens);
                if usage_percent >= LARGE_TOOL_RESULT_PERCENT
                    && total >= LARGE_TOOL_RESULT_TOKENS
                    && let Some(suggestion) =
                        large_tool_suggestion(&tool.name, total, usage_percent, language)
                {
                    suggestions.push(suggestion);
                }
            }

            if let Some(read) = breakdown.tools.iter().find(|tool| tool.name == "Read") {
                let total = read.call_tokens.saturating_add(read.result_tokens);
                let total_percent = percent(total, self.raw_max_tokens);
                let result_percent = percent(read.result_tokens, self.raw_max_tokens);
                let covered_by_large_result =
                    total_percent >= LARGE_TOOL_RESULT_PERCENT && total >= LARGE_TOOL_RESULT_TOKENS;
                if !covered_by_large_result
                    && result_percent >= READ_BLOAT_PERCENT
                    && read.result_tokens >= LARGE_TOOL_RESULT_TOKENS
                {
                    suggestions.push(ContextSuggestion {
                        severity: SuggestionSeverity::Info,
                        title: match language {
                            RendererLanguage::ZhCn => format!(
                                "文件读取结果占用 {} 词元（{}%）",
                                format_tokens(read.result_tokens),
                                fixed_zero(result_percent)
                            ),
                            RendererLanguage::EnUs => format!(
                                "File reads using {} tokens ({}%)",
                                format_tokens(read.result_tokens),
                                fixed_zero(result_percent)
                            ),
                        },
                        detail: language
                            .text(
                                "如果正在重复读取文件，请考虑引用先前的读取结果。读取大文件时使用 offset/limit。",
                                "If you are re-reading files, consider referencing earlier reads. Use offset/limit for large files.",
                            )
                            .to_string(),
                        savings_tokens: Some(read.result_tokens.saturating_mul(3) / 10),
                    });
                }
            }
        }

        let total_memory_tokens = self
            .memory_files
            .iter()
            .fold(0_u64, |sum, file| sum.saturating_add(file.tokens));
        let memory_percent = percent(total_memory_tokens, self.raw_max_tokens);
        if memory_percent >= MEMORY_HIGH_PERCENT && total_memory_tokens >= MEMORY_HIGH_TOKENS {
            let mut largest = self.memory_files.iter().collect::<Vec<_>>();
            largest.sort_by(|left, right| right.tokens.cmp(&left.tokens));
            let largest = largest
                .into_iter()
                .take(3)
                .map(|file| {
                    format!(
                        "{} ({})",
                        display_path(&file.path),
                        format_tokens(file.tokens)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            suggestions.push(ContextSuggestion {
                severity: SuggestionSeverity::Info,
                title: match language {
                    RendererLanguage::ZhCn => format!(
                        "记忆文件占用 {} 词元（{}%）",
                        format_tokens(total_memory_tokens),
                        fixed_zero(memory_percent)
                    ),
                    RendererLanguage::EnUs => format!(
                        "Memory files using {} tokens ({}%)",
                        format_tokens(total_memory_tokens),
                        fixed_zero(memory_percent)
                    ),
                },
                detail: match language {
                    RendererLanguage::ZhCn => {
                        format!("最大项：{largest}。使用 /memory 检查并清理过期条目。")
                    }
                    RendererLanguage::EnUs => {
                        format!(
                            "Largest: {largest}. Use /memory to review and prune stale entries."
                        )
                    }
                },
                savings_tokens: Some(total_memory_tokens.saturating_mul(3) / 10),
            });
        }

        if !self.is_auto_compact_enabled
            && self.percentage >= 50
            && self.percentage < NEAR_CAPACITY_PERCENT
        {
            suggestions.push(ContextSuggestion {
                severity: SuggestionSeverity::Info,
                title: language
                    .text("自动压缩已关闭", "Autocompact is disabled")
                    .to_string(),
                detail: language
                    .text(
                        "不启用自动压缩将触及上下文上限并丢失对话内容。请在 /config 中启用，或手动使用 /compact。",
                        "Without autocompact, you will hit context limits and lose the conversation. Enable it in /config or use /compact manually.",
                    )
                    .to_string(),
                savings_tokens: None,
            });
        }

        suggestions.sort_by(|left, right| match (left.severity, right.severity) {
            (SuggestionSeverity::Warning, SuggestionSeverity::Info) => Ordering::Less,
            (SuggestionSeverity::Info, SuggestionSeverity::Warning) => Ordering::Greater,
            _ => right
                .savings_tokens
                .unwrap_or(0)
                .cmp(&left.savings_tokens.unwrap_or(0)),
        });
        suggestions
    }
}

fn parse_category(value: &Value, index: usize) -> Result<ContextCategory, String> {
    let path = format!("response.categories[{index}]");
    let object = required_object(value, &path)?;
    Ok(ContextCategory {
        name: required_string_field(object, "name", &path)?.to_string(),
        tokens: required_u64_field(object, "tokens", &path)?,
        color: ContextColorRole::parse(
            required_string_field(object, "color", &path)?,
            &format!("{path}.color"),
        )?,
        is_deferred: optional_bool_field(object, "isDeferred", &path)?.unwrap_or(false),
    })
}

fn parse_grid_square(
    value: &Value,
    row_index: usize,
    column_index: usize,
) -> Result<ContextGridSquare, String> {
    let path = format!("response.gridRows[{row_index}][{column_index}]");
    let object = required_object(value, &path)?;
    let color = ContextColorRole::parse(
        required_string_field(object, "color", &path)?,
        &format!("{path}.color"),
    )?;
    let _is_filled = required_bool_field(object, "isFilled", &path)?;
    let category_name = required_string_field(object, "categoryName", &path)?.to_string();
    let _tokens = required_u64_field(object, "tokens", &path)?;
    let _percentage = required_finite_number_field(object, "percentage", &path)?;
    let square_fullness = required_finite_number_field(object, "squareFullness", &path)?;
    if !(0.0..=1.0).contains(&square_fullness) {
        return Err(format!(
            "{path}.squareFullness must be between zero and one"
        ));
    }
    Ok(ContextGridSquare {
        color,
        category_name,
        is_full: square_fullness >= 0.7,
    })
}

fn parse_memory_file(value: &Value, index: usize) -> Result<MemoryFile, String> {
    let path = format!("response.memoryFiles[{index}]");
    let object = required_object(value, &path)?;
    let path_value = required_string_field(object, "path", &path)?.to_string();
    let _kind = required_string_field(object, "type", &path)?;
    let tokens = required_u64_field(object, "tokens", &path)?;
    Ok(MemoryFile {
        path: path_value,
        tokens,
    })
}

fn parse_mcp_tool(value: &Value, index: usize) -> Result<McpTool, String> {
    let path = format!("response.mcpTools[{index}]");
    let object = required_object(value, &path)?;
    let name = required_string_field(object, "name", &path)?.to_string();
    let _server_name = required_string_field(object, "serverName", &path)?;
    let tokens = required_u64_field(object, "tokens", &path)?;
    let is_loaded = optional_bool_field(object, "isLoaded", &path)?;
    Ok(McpTool {
        name,
        tokens,
        is_loaded,
    })
}

fn parse_sourced_usage(
    value: &Value,
    index: usize,
    collection: &str,
    name_field: &str,
) -> Result<SourcedUsage, String> {
    let path = format!("response.{collection}[{index}]");
    let object = required_object(value, &path)?;
    let name = required_string_field(object, name_field, &path)?.to_string();
    let source = ContextSource::parse(
        required_string_field(object, "source", &path)?,
        &format!("{path}.source"),
    )?;
    let tokens = required_u64_field(object, "tokens", &path)?;
    Ok(SourcedUsage {
        name,
        source,
        tokens,
    })
}

fn parse_optional_skills(response: &Map<String, Value>) -> Result<Option<SkillUsage>, String> {
    let Some(value) = response.get("skills") else {
        return Ok(None);
    };
    let object = required_object(value, "response.skills")?;
    let _total_skills = required_u64_field(object, "totalSkills", "response.skills")?;
    let _included_skills = required_u64_field(object, "includedSkills", "response.skills")?;
    let tokens = required_u64_field(object, "tokens", "response.skills")?;
    let entries = required_array_field(object, "skillFrontmatter", "response.skills")?
        .iter()
        .enumerate()
        .map(|(index, value)| parse_sourced_usage(value, index, "skills.skillFrontmatter", "name"))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(SkillUsage { tokens, entries }))
}

fn parse_optional_message_breakdown(
    response: &Map<String, Value>,
) -> Result<Option<MessageBreakdown>, String> {
    let Some(value) = response.get("messageBreakdown") else {
        return Ok(None);
    };
    let path = "response.messageBreakdown";
    let object = required_object(value, path)?;
    for field in [
        "toolCallTokens",
        "toolResultTokens",
        "attachmentTokens",
        "assistantMessageTokens",
        "userMessageTokens",
    ] {
        required_u64_field(object, field, path)?;
    }
    let tools = required_array_field(object, "toolCallsByType", path)?
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let item_path = format!("{path}.toolCallsByType[{index}]");
            let item = required_object(value, &item_path)?;
            Ok(ToolUsage {
                name: required_string_field(item, "name", &item_path)?.to_string(),
                call_tokens: required_u64_field(item, "callTokens", &item_path)?,
                result_tokens: required_u64_field(item, "resultTokens", &item_path)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    for (index, value) in required_array_field(object, "attachmentsByType", path)?
        .iter()
        .enumerate()
    {
        let item_path = format!("{path}.attachmentsByType[{index}]");
        let item = required_object(value, &item_path)?;
        required_string_field(item, "name", &item_path)?;
        required_u64_field(item, "tokens", &item_path)?;
    }
    Ok(Some(MessageBreakdown { tools }))
}

fn validate_optional_named_token_list(
    response: &Map<String, Value>,
    field: &str,
    requires_loaded: bool,
) -> Result<(), String> {
    let Some(value) = response.get(field) else {
        return Ok(());
    };
    let path = format!("response.{field}");
    for (index, value) in required_array(value, &path)?.iter().enumerate() {
        let item_path = format!("{path}[{index}]");
        let object = required_object(value, &item_path)?;
        required_string_field(object, "name", &item_path)?;
        required_u64_field(object, "tokens", &item_path)?;
        if requires_loaded {
            required_bool_field(object, "isLoaded", &item_path)?;
        }
    }
    Ok(())
}

fn validate_optional_slash_commands(response: &Map<String, Value>) -> Result<(), String> {
    let Some(value) = response.get("slashCommands") else {
        return Ok(());
    };
    let path = "response.slashCommands";
    let object = required_object(value, path)?;
    for field in ["totalCommands", "includedCommands", "tokens"] {
        required_u64_field(object, field, path)?;
    }
    Ok(())
}

fn validate_api_usage(response: &Map<String, Value>) -> Result<(), String> {
    let Some(value) = response.get("apiUsage") else {
        return Err("response.apiUsage is missing".to_string());
    };
    if value.is_null() {
        return Ok(());
    }
    let path = "response.apiUsage";
    let object = required_object(value, path)?;
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_creation_input_tokens",
        "cache_read_input_tokens",
    ] {
        required_u64_field(object, field, path)?;
    }
    Ok(())
}

fn append_sourced_section(
    lines: &mut Vec<Line<'static>>,
    title: &str,
    command_hint: &str,
    entries: &[SourcedUsage],
    primary: Style,
    secondary: Style,
    language: RendererLanguage,
) {
    lines.push(Line::default());
    lines.push(Line::from(vec![
        Span::styled(title.to_string(), primary.add_modifier(Modifier::BOLD)),
        Span::styled(command_hint.to_string(), secondary),
    ]));
    for source in SOURCE_DISPLAY_ORDER {
        let mut group = entries
            .iter()
            .filter(|entry| entry.source == source)
            .collect::<Vec<_>>();
        group.sort_by(|left, right| right.tokens.cmp(&left.tokens));
        if group.is_empty() {
            continue;
        }
        lines.push(Line::default());
        lines.push(Line::styled(source.display_name(language), secondary));
        for entry in group {
            lines.push(token_usage_line(
                &entry.name,
                entry.tokens,
                primary,
                secondary,
                language,
            ));
        }
    }
}

fn token_usage_line(
    name: &str,
    tokens: u64,
    primary: Style,
    secondary: Style,
    language: RendererLanguage,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("└ {}: ", safe_inline(name)), primary),
        Span::styled(
            match language {
                RendererLanguage::ZhCn => format!("{} 词元", format_tokens(tokens)),
                RendererLanguage::EnUs => format!("{} tokens", format_tokens(tokens)),
            },
            secondary,
        ),
    ])
}

fn large_tool_suggestion(
    tool_name: &str,
    tokens: u64,
    usage_percent: f64,
    language: RendererLanguage,
) -> Option<ContextSuggestion> {
    let token_display = format_tokens(tokens);
    let percentage = fixed_zero(usage_percent);
    match tool_name {
        "Bash" => Some(ContextSuggestion {
            severity: SuggestionSeverity::Warning,
            title: match language {
                RendererLanguage::ZhCn => {
                    format!("Bash 结果占用 {token_display} 词元（{percentage}%）")
                }
                RendererLanguage::EnUs => {
                    format!("Bash results using {token_display} tokens ({percentage}%)")
                }
            },
            detail: language
                .text(
                    "通过 head、tail 或 grep 减少结果大小。避免对大文件使用 cat；改用带 offset/limit 的 Read。",
                    "Pipe output through head, tail, or grep to reduce result size. Avoid cat on large files — use Read with offset/limit instead.",
                )
                .to_string(),
            savings_tokens: Some(tokens / 2),
        }),
        "Read" => Some(ContextSuggestion {
            severity: SuggestionSeverity::Info,
            title: match language {
                RendererLanguage::ZhCn => {
                    format!("Read 结果占用 {token_display} 词元（{percentage}%）")
                }
                RendererLanguage::EnUs => {
                    format!("Read results using {token_display} tokens ({percentage}%)")
                }
            },
            detail: language
                .text(
                    "使用 offset 和 limit 参数只读取所需部分。仅需要几行时，不要重新读取整个文件。",
                    "Use offset and limit parameters to read only the sections you need. Avoid re-reading entire files when you only need a few lines.",
                )
                .to_string(),
            savings_tokens: Some(tokens.saturating_mul(3) / 10),
        }),
        "Grep" => Some(ContextSuggestion {
            severity: SuggestionSeverity::Info,
            title: match language {
                RendererLanguage::ZhCn => {
                    format!("Grep 结果占用 {token_display} 词元（{percentage}%）")
                }
                RendererLanguage::EnUs => {
                    format!("Grep results using {token_display} tokens ({percentage}%)")
                }
            },
            detail: language
                .text(
                    "添加更具体的模式，或使用 glob/type 参数缩小文件类型范围。查找文件时可考虑使用 Glob 而非 Grep。",
                    "Add more specific patterns or use the glob or type parameter to narrow file types. Consider Glob for file discovery instead of Grep.",
                )
                .to_string(),
            savings_tokens: Some(tokens.saturating_mul(3) / 10),
        }),
        "WebFetch" => Some(ContextSuggestion {
            severity: SuggestionSeverity::Info,
            title: match language {
                RendererLanguage::ZhCn => {
                    format!("WebFetch 结果占用 {token_display} 词元（{percentage}%）")
                }
                RendererLanguage::EnUs => {
                    format!("WebFetch results using {token_display} tokens ({percentage}%)")
                }
            },
            detail: language
                .text(
                    "网页内容可能很大，请考虑只提取需要的具体信息。",
                    "Web page content can be very large. Consider extracting only the specific information needed.",
                )
                .to_string(),
            savings_tokens: Some(tokens.saturating_mul(4) / 10),
        }),
        _ if usage_percent >= 20.0 => Some(ContextSuggestion {
            severity: SuggestionSeverity::Info,
            title: match language {
                RendererLanguage::ZhCn => format!(
                    "{} 占用 {token_display} 词元（{percentage}%）",
                    safe_inline(tool_name)
                ),
                RendererLanguage::EnUs => format!(
                    "{} using {token_display} tokens ({percentage}%)",
                    safe_inline(tool_name)
                ),
            },
            detail: language
                .text(
                    "此工具正在占用大量上下文。",
                    "This tool is consuming a significant portion of context.",
                )
                .to_string(),
            savings_tokens: Some(tokens.saturating_mul(2) / 10),
        }),
        _ => None,
    }
}

fn percent(tokens: u64, raw_max_tokens: u64) -> f64 {
    (tokens as f64 / raw_max_tokens as f64) * 100.0
}

fn fixed_zero(value: f64) -> u64 {
    value.round().max(0.0) as u64
}

fn format_tokens(value: u64) -> String {
    const UNITS: [(u64, &str); 4] = [
        (1_000_000_000_000, "t"),
        (1_000_000_000, "b"),
        (1_000_000, "m"),
        (1_000, "k"),
    ];
    for (index, (threshold, _suffix)) in UNITS.into_iter().enumerate() {
        if value >= threshold {
            let mut unit_index = index;
            let mut tenths = value.saturating_mul(10).saturating_add(threshold / 2) / threshold;
            if tenths >= 10_000 && unit_index > 0 {
                unit_index -= 1;
                let promoted_threshold = UNITS[unit_index].0;
                tenths = value
                    .saturating_mul(10)
                    .saturating_add(promoted_threshold / 2)
                    / promoted_threshold;
            }
            return if tenths.is_multiple_of(10) {
                format!("{}{}", tenths / 10, UNITS[unit_index].1)
            } else {
                format!("{}.{}{}", tenths / 10, tenths % 10, UNITS[unit_index].1)
            };
        }
    }
    value.to_string()
}

fn display_path(value: &str) -> String {
    let path = Path::new(value);
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(relative) = path.strip_prefix(&cwd)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_string_lossy().into_owned();
    }
    if let Some(home) = dirs::home_dir()
        && let Ok(relative) = path.strip_prefix(&home)
        && !relative.as_os_str().is_empty()
    {
        return format!("~/{}", relative.to_string_lossy());
    }
    value.to_string()
}

fn safe_inline(value: &str) -> String {
    sanitize_bounded_terminal_text(value).replace('\n', " ")
}

fn required_object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("{path} must be an object"))
}

fn required_array<'a>(value: &'a Value, path: &str) -> Result<&'a [Value], String> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| format!("{path} must be an array"))
}

fn required_object_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<&'a Map<String, Value>, String> {
    let field_path = format!("{path}.{field}");
    required_object(
        object
            .get(field)
            .ok_or_else(|| format!("{field_path} is missing"))?,
        &field_path,
    )
}

fn required_array_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<&'a [Value], String> {
    let field_path = format!("{path}.{field}");
    required_array(
        object
            .get(field)
            .ok_or_else(|| format!("{field_path} is missing"))?,
        &field_path,
    )
}

fn required_string_field<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<&'a str, String> {
    let field_path = format!("{path}.{field}");
    object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{field_path} must be a string"))
}

fn required_u64_field(object: &Map<String, Value>, field: &str, path: &str) -> Result<u64, String> {
    let field_path = format!("{path}.{field}");
    let value = object
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{field_path} must be a non-negative integer"))?;
    if value > JAVASCRIPT_MAX_SAFE_INTEGER {
        return Err(format!(
            "{field_path} exceeds JavaScript's maximum safe integer"
        ));
    }
    Ok(value)
}

fn validate_optional_u64_field(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<(), String> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    let Some(value) = value.as_u64() else {
        return Err(format!("{path}.{field} must be a non-negative integer"));
    };
    if value > JAVASCRIPT_MAX_SAFE_INTEGER {
        return Err(format!(
            "{path}.{field} exceeds JavaScript's maximum safe integer"
        ));
    }
    Ok(())
}

fn required_finite_number_field(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<f64, String> {
    let field_path = format!("{path}.{field}");
    object
        .get(field)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("{field_path} must be a finite number"))
}

fn required_bool_field(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<bool, String> {
    let field_path = format!("{path}.{field}");
    object
        .get(field)
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("{field_path} must be a boolean"))
}

fn optional_bool_field(
    object: &Map<String, Value>,
    field: &str,
    path: &str,
) -> Result<Option<bool>, String> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    value
        .as_bool()
        .map(Some)
        .ok_or_else(|| format!("{path}.{field} must be a boolean when present"))
}

#[cfg(any(test, feature = "test-support"))]
pub fn minimal_test_control_response() -> Value {
    serde_json::json!({
        "response": {
            "categories": [
                {"name":"MCP tools","tokens":20_000,"color":"cyan_FOR_SUBAGENTS_ONLY"},
                {"name":"Free space","tokens":80_000,"color":"promptBorder"}
            ],
            "totalTokens":20_000,
            "maxTokens":100_000,
            "rawMaxTokens":100_000,
            "percentage":20,
            "gridRows":[[
                {
                    "color":"cyan_FOR_SUBAGENTS_ONLY",
                    "isFilled":true,
                    "categoryName":"MCP tools",
                    "tokens":20_000,
                    "percentage":20,
                    "squareFullness":1
                },
                {
                    "color":"promptBorder",
                    "isFilled":true,
                    "categoryName":"Free space",
                    "tokens":80_000,
                    "percentage":80,
                    "squareFullness":1
                }
            ]],
            "model":"model-from-sdk",
            "memoryFiles":[],
            "mcpTools":[],
            "agents":[],
            "isAutoCompactEnabled":true,
            "apiUsage":null
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn response() -> Value {
        json!({
            "response": {
                "categories": [
                    {"name":"System prompt","tokens":10_000,"color":"promptBorder"},
                    {"name":"MCP tools","tokens":12_000,"color":"cyan_FOR_SUBAGENTS_ONLY"},
                    {"name":"MCP tools (deferred)","tokens":2_000,"color":"inactive","isDeferred":true},
                    {"name":"Custom agents","tokens":8_000,"color":"permission"},
                    {"name":"Memory files","tokens":6_000,"color":"crabCode"},
                    {"name":"Skills","tokens":4_000,"color":"warning"},
                    {"name":"Messages","tokens":30_000,"color":"purple_FOR_SUBAGENTS_ONLY"},
                    {"name":"Autocompact buffer","tokens":10_000,"color":"inactive"},
                    {"name":"Free space","tokens":20_000,"color":"promptBorder"}
                ],
                "totalTokens":70_000,
                "maxTokens":100_000,
                "rawMaxTokens":100_000,
                "percentage":70,
                "gridRows":[[
                    {"color":"promptBorder","isFilled":true,"categoryName":"Free space","tokens":20_000,"percentage":20,"squareFullness":1},
                    {"color":"inactive","isFilled":true,"categoryName":"Autocompact buffer","tokens":10_000,"percentage":10,"squareFullness":1},
                    {"color":"cyan_FOR_SUBAGENTS_ONLY","isFilled":true,"categoryName":"MCP tools","tokens":12_000,"percentage":12,"squareFullness":1},
                    {"color":"permission","isFilled":true,"categoryName":"Custom agents","tokens":8_000,"percentage":8,"squareFullness":0.6}
                ]],
                "model":"model-from-sdk",
                "memoryFiles":[{"path":"MEMORY.md","type":"project","tokens":6_000}],
                "mcpTools":[
                    {"name":"loaded-tool","serverName":"server","tokens":7_000,"isLoaded":true},
                    {"name":"available-tool","serverName":"server","tokens":5_000,"isLoaded":false}
                ],
                "deferredBuiltinTools":[],
                "systemTools":[],
                "systemPromptSections":[],
                "agents":[
                    {"agentType":"project-agent","source":"projectSettings","tokens":5_000},
                    {"agentType":"user-agent","source":"userSettings","tokens":2_000},
                    {"agentType":"local-agent","source":"localSettings","tokens":1_000}
                ],
                "slashCommands":{"totalCommands":1,"includedCommands":1,"tokens":10},
                "skills":{
                    "totalSkills":3,
                    "includedSkills":3,
                    "tokens":4_000,
                    "skillFrontmatter":[
                        {"name":"managed-skill","source":"policySettings","tokens":3_000},
                        {"name":"plugin-skill","source":"plugin","tokens":1_000}
                    ]
                },
                "autoCompactThreshold":90_000,
                "isAutoCompactEnabled":true,
                "messageBreakdown":{
                    "toolCallTokens":100,
                    "toolResultTokens":100,
                    "attachmentTokens":0,
                    "assistantMessageTokens":100,
                    "userMessageTokens":100,
                    "toolCallsByType":[],
                    "attachmentsByType":[]
                },
                "apiUsage":null
            }
        })
    }

    fn texts(lines: &[Line<'_>]) -> Vec<String> {
        lines.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn existing_response_renders_historical_grid_legend_and_sections() {
        let context =
            ContextVisualization::from_control_response(&response()).expect("valid response");
        let theme = CrabCodeTheme::dark();
        let lines = context.styled_lines(
            theme,
            CrabCodeThemeKind::Dark,
            ColorLevel::TrueColor,
            RendererLanguage::EnUs,
        );
        let text = texts(&lines).join("\n");

        assert!(text.contains("⛶ ⛝ ⛁ ⛀"));
        assert!(text.contains("model-from-sdk · 70k/100k tokens (70%)"));
        assert!(text.contains("Estimated usage by category"));
        assert!(text.contains("MCP tools · /mcp loaded on demand"));
        assert!(text.contains("Loaded\n└ loaded-tool: 7k tokens"));
        assert!(text.contains("Available\n└ available-tool"));
        assert!(text.contains("Custom agents · /agents"));
        assert!(text.contains("Project\n└ project-agent: 5k tokens"));
        assert!(text.contains("User\n└ user-agent: 2k tokens"));
        assert!(!text.contains("local-agent"));
        assert!(text.contains("Memory files · /memory"));
        assert!(text.contains("Skills · /skills"));
        assert!(text.contains("Managed\n└ managed-skill: 3k tokens"));

        let glyph_spans = &lines[0].spans;
        assert_eq!(glyph_spans[0].style.fg, Some(theme.text_secondary));
        assert_eq!(glyph_spans[1].style.fg, Some(theme.text_secondary));
        assert_eq!(glyph_spans[2].style.fg, Some(Color::Rgb(8, 145, 178)));
        assert_eq!(glyph_spans[3].style.fg, Some(Color::Rgb(177, 185, 249)));
    }

    #[test]
    fn chinese_context_copy_preserves_backend_dynamic_values_exactly() {
        let mut payload = response();
        payload["response"]["model"] = json!("model-原样-42");
        payload["response"]["memoryFiles"][0]["path"] = json!("PATH_SENTINEL.md");
        payload["response"]["mcpTools"][0]["name"] = json!("tool-原样-17");
        payload["response"]["agents"][0]["agentType"] = json!("agent-原样-23");
        payload["response"]["skills"]["skillFrontmatter"][0]["name"] = json!("skill-原样-31");

        let context =
            ContextVisualization::from_control_response(&payload).expect("valid response");
        let lines = context.styled_lines(
            CrabCodeTheme::dark(),
            CrabCodeThemeKind::Dark,
            ColorLevel::TrueColor,
            RendererLanguage::ZhCn,
        );
        let text = texts(&lines).join("\n");

        assert!(text.contains("model-原样-42 · 70k/100k 词元（70%）"));
        assert!(text.contains("按类别估算的用量"));
        assert!(text.contains("可用空间：20k（20.0%）"));
        assert!(text.contains("自动压缩缓冲区：10k 词元（10.0%）"));
        assert!(text.contains("MCP 工具 · /mcp 按需加载"));
        assert!(text.contains("已加载\n└ tool-原样-17: 7k 词元"));
        assert!(text.contains("可用\n└ available-tool"));
        assert!(text.contains("自定义代理 · /agents"));
        assert!(text.contains("项目\n└ agent-原样-23: 5k 词元"));
        assert!(text.contains("记忆文件 · /memory"));
        assert!(text.contains("└ PATH_SENTINEL.md: 6k 词元"));
        assert!(text.contains("技能 · /skills"));
        assert!(text.contains("托管\n└ skill-原样-31: 3k 词元"));
        assert!(text.contains("建议"));
        assert!(text.contains("最大项：PATH_SENTINEL.md (6k)"));
        assert!(!text.contains("Estimated usage by category"));
        assert!(!text.contains("Memory files using"));
    }

    #[test]
    fn bundled_skill_source_from_direct_runtime_uses_builtin_presentation() {
        let mut payload = response();
        payload["response"]["skills"]["skillFrontmatter"][0]["source"] = json!("bundled");

        let context =
            ContextVisualization::from_control_response(&payload).expect("valid response");
        let text = texts(&context.styled_lines(
            CrabCodeTheme::dark(),
            CrabCodeThemeKind::Dark,
            ColorLevel::TrueColor,
            RendererLanguage::ZhCn,
        ))
        .join("\n");

        assert!(text.contains("内置"));
        assert!(text.contains("managed-skill"));
    }

    #[test]
    fn parser_rejects_unknown_color_source_and_non_integral_tokens() {
        let mut unknown_color = response();
        unknown_color["response"]["categories"][0]["color"] = json!("futureColor");
        assert_eq!(
            ContextVisualization::from_control_response(&unknown_color),
            Err(
                "response.categories[0].color has unknown historical theme role `futureColor`"
                    .to_string()
            )
        );

        let mut unknown_source = response();
        unknown_source["response"]["agents"][0]["source"] = json!("futureSettings");
        assert_eq!(
            ContextVisualization::from_control_response(&unknown_source),
            Err(
                "response.agents[0].source has unknown setting source `futureSettings`".to_string()
            )
        );

        let mut fractional_tokens = response();
        fractional_tokens["response"]["totalTokens"] = json!(1.5);
        assert_eq!(
            ContextVisualization::from_control_response(&fractional_tokens),
            Err("response.totalTokens must be a non-negative integer".to_string())
        );
    }

    #[test]
    fn source_suggestions_keep_historical_thresholds_order_and_copy() {
        let mut payload = response();
        payload["response"]["percentage"] = json!(85);
        payload["response"]["isAutoCompactEnabled"] = json!(false);
        payload["response"]["messageBreakdown"]["toolCallsByType"] = json!([
            {"name":"Bash","callTokens":1_000,"resultTokens":19_000},
            {"name":"Other","callTokens":1_000,"resultTokens":19_000}
        ]);
        let context =
            ContextVisualization::from_control_response(&payload).expect("valid response");
        let suggestions = context.suggestions(RendererLanguage::EnUs);

        assert_eq!(suggestions[0].severity, SuggestionSeverity::Warning);
        assert_eq!(suggestions[0].title, "Bash results using 20k tokens (20%)");
        assert_eq!(suggestions[1].severity, SuggestionSeverity::Warning);
        assert_eq!(suggestions[1].title, "Context is 85% full");
        assert!(suggestions.iter().any(|suggestion| {
            suggestion.title == "Other using 20k tokens (20%)"
                && suggestion.savings_tokens == Some(4_000)
        }));
        assert!(suggestions.iter().any(|suggestion| {
            suggestion.title == "Memory files using 6k tokens (6%)"
                && suggestion.savings_tokens == Some(1_800)
        }));
    }

    #[test]
    fn token_format_matches_historical_compact_formatter_without_dot_zero() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_001), "1k");
        assert_eq!(format_tokens(1_050), "1.1k");
        assert_eq!(format_tokens(999_999), "1m");
        assert_eq!(format_tokens(8_388_608), "8.4m");
    }

    #[test]
    fn control_response_wrapper_and_required_schema_fields_fail_closed() {
        assert_eq!(
            ContextVisualization::from_control_response(&json!({})),
            Err("control_response.response is missing".to_string())
        );
        let mut missing = response();
        missing["response"]
            .as_object_mut()
            .expect("response object")
            .remove("apiUsage");
        assert_eq!(
            ContextVisualization::from_control_response(&missing),
            Err("response.apiUsage is missing".to_string())
        );
    }
}

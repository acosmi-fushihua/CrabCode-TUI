//! Typed CrabCode transcript rows hosted by the fixed scrollback lifecycle.
//!
//! The block lifecycle (`BlockContent`, fold/raw/selection, wrapping and
//! `RenderBlock` delegation) is fixed to the pinned upstream commit
//! `a5727c5960452e7527a154b25cb5bf00cda0545e`. The semantic rows in this file
//! are presentation-only translations of the historical direct TUI at
//! CrabCode commit `2358212c2df2018816058c8a03b1ac3d324e74e0`,
//! principally `AttachmentMessage.tsx`, `AdvisorMessage.tsx`,
//! `HookProgressMessage.tsx`, `ShellProgressMessage.tsx`,
//! `SystemTextMessage.tsx`, `Message.tsx`, and
//! `UserToolResultMessage.tsx`. Ordinary tool rows retain their typed fields
//! here, but the historical registry's per-tool render functions remain a
//! separately audited RED denominator; this block does not impersonate them.
//!
//! These types are deliberately renderer-private. They do not describe,
//! extend, or normalize a backend protocol. Open structured tool payloads are
//! accepted only after the CrabCode projection has independently classified
//! their enclosing content block; this renderer receives display strings and
//! never dispatches on JSON keys.

use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use rand::Rng as _;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Number;
use vte::{Params, Parser, Perform};

use crate::appearance::RendererLanguage;
use crate::render::wrapping::word_wrap_lines;
use crate::scrollback::block::{BlockContent, RenderBlock, join_searchable};
use crate::scrollback::types::{
    AccentStyle, BlockContext, BlockLine, BlockOutput, DisplayMode, Selectable,
    shift_selection_metadata_for_prefix,
};
use crate::text_safety::sanitize_bounded_terminal_text;
use crate::theme::Theme;
use crate::util::abbreviate_path;

const ADVISOR_REVIEWED_MESSAGE_EN: &str =
    "Advisor has reviewed the conversation and will apply the feedback";
const ADVISOR_REVIEWED_MESSAGE_ZH: &str = "顾问已审阅对话，将应用反馈";
const MCP_PROGRESS_BAR_CELLS: usize = 20;
const MCP_PROGRESS_BLOCKS: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];
const TURN_COMPLETION_VERBS: [&str; 8] = [
    "Baked",
    "Brewed",
    "Churned",
    "Cogitated",
    "Cooked",
    "Crunched",
    "Sautéed",
    "Worked",
];

#[derive(Debug, Clone)]
pub struct CrabCodeProjectionBlock {
    pub kind: CrabCodeProjectionKind,
}

impl CrabCodeProjectionBlock {
    pub fn new(kind: CrabCodeProjectionKind) -> Self {
        Self { kind }
    }

    /// Retain state created by the fixed component's `useState` initializer.
    ///
    /// A projection revision may replace the Rust block while preserving the
    /// same scrollback entry identity. React keeps the sampled completion verb
    /// across that update, so the replacement must inherit it rather than
    /// visibly changing on a redraw.
    pub fn inherit_turn_duration_component_state(&mut self, previous: &Self) {
        let (
            CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::TurnDuration {
                completion_verb,
                ..
            }),
            CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::TurnDuration {
                completion_verb: previous_verb,
                ..
            }),
        ) = (&mut self.kind, &previous.kind)
        else {
            return;
        };
        completion_verb.clone_from(previous_verb);
    }

    pub fn is_hidden(&self) -> bool {
        match &self.kind {
            CrabCodeProjectionKind::DirectSystem(system) => system.is_hidden(),
            CrabCodeProjectionKind::DirectProgress(progress) => progress.is_hidden(),
            CrabCodeProjectionKind::DirectAttachment(attachment) => attachment.is_hidden(),
            CrabCodeProjectionKind::SourceNull(_) => true,
            CrabCodeProjectionKind::Advisor(_)
            | CrabCodeProjectionKind::DirectNestedProgress(_)
            | CrabCodeProjectionKind::RedactedThinking
            | CrabCodeProjectionKind::UserImage { .. }
            | CrabCodeProjectionKind::SdkImage(_)
            | CrabCodeProjectionKind::Tool(_)
            | CrabCodeProjectionKind::SdkSystem(_) => false,
        }
    }

    /// Return the fixed component-local API retry timer window.
    ///
    /// The historical component starts at zero elapsed milliseconds, advances
    /// in exact one-second intervals, and parks only after the first tick at
    /// or beyond `retryInMs`. This renderer-only timing fact never crosses the
    /// backend/protocol boundary.
    pub fn api_retry_animation_window(&self) -> Option<(Instant, Instant)> {
        let CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::ApiError {
            retry_in_ms,
            retry_attempt,
            mounted_at,
            ..
        }) = &self.kind
        else {
            return None;
        };
        if retry_attempt.as_f64().is_none_or(|attempt| attempt < 4.0) {
            return None;
        }
        let retry_in_ms = retry_in_ms.as_f64()?;
        if !retry_in_ms.is_finite() || retry_in_ms <= 0.0 {
            return None;
        }
        let tick_count = (retry_in_ms / 1_000.0).ceil();
        if tick_count > u64::MAX as f64 {
            return None;
        }
        let stop_at = mounted_at.checked_add(Duration::from_secs(tick_count as u64))?;
        Some((*mounted_at, stop_at))
    }

    /// Whether this projected row must remain retractable instead of entering
    /// immutable native terminal scrollback.
    ///
    /// The historical direct TUI always renders an `api_error` dynamically and
    /// removes it as soon as a later non-error message appears. Minimal mode
    /// cannot retract a row after `insert_before`, so the retry row must hold
    /// the print-once frontier even while the turn is idle. Once the projection
    /// removes/replaces it, the normal frontier can advance.
    pub fn holds_native_scrollback_frontier(&self) -> bool {
        matches!(
            &self.kind,
            CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::ApiError { .. })
        )
    }

    pub fn copy_text(&self) -> Option<String> {
        self.searchable_text()
    }

    pub fn searchable_text(&self) -> Option<String> {
        if self.is_hidden() {
            return None;
        }
        match &self.kind {
            CrabCodeProjectionKind::Advisor(advisor) => match advisor {
                CrabCodeAdvisorBlock::Invocation { input, .. } => join_searchable([
                    Some("Advising".to_string()),
                    input.display_text().map(ToString::to_string),
                ]),
                CrabCodeAdvisorBlock::Feedback { text } => join_searchable([
                    Some(ADVISOR_REVIEWED_MESSAGE_EN.to_string()),
                    Some(text.clone()),
                ]),
                CrabCodeAdvisorBlock::Redacted => {
                    join_searchable([Some(ADVISOR_REVIEWED_MESSAGE_EN.to_string())])
                }
                CrabCodeAdvisorBlock::Error { error_code } => join_searchable([
                    Some("Advisor unavailable".to_string()),
                    Some(error_code.clone()),
                ]),
            },
            CrabCodeProjectionKind::UserImage { image_id } => {
                join_searchable([Some(image_label(*image_id, RendererLanguage::EnUs))])
            }
            CrabCodeProjectionKind::SdkImage(image) => image.searchable_text(),
            CrabCodeProjectionKind::Tool(tool) => tool.searchable_text(),
            CrabCodeProjectionKind::SdkSystem(system) => {
                join_searchable([Some(system.title.clone()), Some(system.text.clone())])
            }
            CrabCodeProjectionKind::DirectSystem(system) => system.searchable_text(),
            CrabCodeProjectionKind::DirectProgress(progress) => progress.searchable_text(),
            CrabCodeProjectionKind::DirectAttachment(attachment) => attachment.searchable_text(),
            CrabCodeProjectionKind::DirectNestedProgress(progress) => progress.searchable_text(),
            CrabCodeProjectionKind::RedactedThinking => {
                join_searchable([Some("Thinking".to_string())])
            }
            CrabCodeProjectionKind::SourceNull(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum CrabCodeProjectionKind {
    Advisor(CrabCodeAdvisorBlock),
    /// Fixed historical placeholder; redacted ciphertext never enters output.
    RedactedThinking,
    UserImage {
        image_id: Option<u64>,
    },
    SdkImage(CrabCodeSdkImageBlock),
    Tool(CrabCodeToolBlock),
    SdkSystem(CrabCodeSdkSystemBlock),
    DirectSystem(CrabCodeDirectSystemBlock),
    DirectProgress(CrabCodeDirectProgressBlock),
    DirectNestedProgress(CrabCodeDirectNestedProgressBlock),
    DirectAttachment(CrabCodeDirectAttachmentBlock),
    SourceNull(CrabCodeSourceNullBlock),
}

/// Fixed historical AgentTool/SkillTool nested-progress rows.
///
/// The inner message is already classified by CrabCode's read-only
/// projection. `AgentMessage` preserves one classified assistant block;
/// `SkillMessage` scans every classified block in one outer envelope and
/// applies the fixed one-row overflow clip. Neither variant inspects or
/// dispatches on an open JSON payload.
#[derive(Debug, Clone)]
pub enum CrabCodeDirectNestedProgressBlock {
    AgentPrompt {
        prompt: String,
    },
    AgentInitializing,
    AgentHiddenToolUses {
        count: usize,
    },
    AgentMessage {
        source_text: String,
        verbose: bool,
        inner: Box<RenderBlock>,
    },
    SkillHiddenMessages {
        count: usize,
    },
    SkillMessage {
        source_text: String,
        verbose: bool,
        inners: Vec<RenderBlock>,
    },
}

impl CrabCodeDirectNestedProgressBlock {
    fn searchable_text(&self) -> Option<String> {
        match self {
            Self::AgentPrompt { prompt } => {
                join_searchable([Some("Prompt:".to_string()), Some(prompt.clone())])
            }
            Self::AgentInitializing => join_searchable([Some("Initializing…".to_string())]),
            Self::AgentHiddenToolUses { count } => join_searchable([Some(hidden_tool_uses_text(
                *count,
                true,
                RendererLanguage::EnUs,
            ))]),
            Self::AgentMessage { source_text, .. } | Self::SkillMessage { source_text, .. } => {
                join_searchable([Some(source_text.clone())])
            }
            Self::SkillHiddenMessages { count } => join_searchable([Some(hidden_tool_uses_text(
                *count,
                false,
                RendererLanguage::EnUs,
            ))]),
        }
    }

    fn output(&self, ctx: &BlockContext, theme: &Theme, language: RendererLanguage) -> BlockOutput {
        match self {
            Self::AgentPrompt { prompt } => {
                nested_agent_prompt_output(prompt, ctx, theme, language)
            }
            Self::AgentInitializing => BlockOutput {
                lines: vec![BlockLine::styled(Line::from(Span::styled(
                    language.text("正在初始化…", "Initializing…"),
                    theme.muted(),
                )))],
            },
            Self::AgentHiddenToolUses { count } => BlockOutput {
                lines: vec![BlockLine::styled(Line::from(Span::styled(
                    hidden_tool_uses_text(*count, true, language),
                    theme.muted(),
                )))],
            },
            Self::AgentMessage { verbose, inner, .. } => nested_inner_output(inner, *verbose, ctx),
            Self::SkillHiddenMessages { count } => BlockOutput {
                lines: vec![BlockLine::styled(Line::from(Span::styled(
                    hidden_tool_uses_text(*count, false, language),
                    theme.muted(),
                )))],
            },
            Self::SkillMessage {
                verbose, inners, ..
            } => {
                for inner in inners {
                    let mut output = nested_inner_output(inner, *verbose, ctx);
                    if !output.lines.is_empty() {
                        output.lines.truncate(1);
                        return output;
                    }
                }
                // Fixed SkillTool wraps every outer progress message in a
                // height=1 Box. A wholly null Message child therefore occupies
                // one blank row instead of disappearing.
                BlockOutput {
                    lines: vec![BlockLine::separator(Line::default())],
                }
            }
        }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        match self {
            Self::AgentMessage { inner, .. } => inner.accent(ctx),
            Self::SkillMessage {
                verbose, inners, ..
            } => inners
                .iter()
                .find(|inner| !nested_inner_output(inner, *verbose, ctx).lines.is_empty())
                .and_then(|inner| inner.accent(ctx)),
            Self::AgentPrompt { .. }
            | Self::AgentInitializing
            | Self::AgentHiddenToolUses { .. }
            | Self::SkillHiddenMessages { .. } => None,
        }
    }

    fn is_selectable(&self) -> bool {
        match self {
            Self::AgentPrompt { .. } => true,
            Self::AgentMessage { inner, .. } => inner.is_selectable(),
            Self::SkillMessage { inners, .. } => inners.iter().any(RenderBlock::is_selectable),
            Self::AgentInitializing
            | Self::AgentHiddenToolUses { .. }
            | Self::SkillHiddenMessages { .. } => false,
        }
    }
}

fn nested_inner_output(inner: &RenderBlock, verbose: bool, ctx: &BlockContext) -> BlockOutput {
    let mut inner_ctx = ctx.clone();
    inner_ctx.mode = if verbose {
        DisplayMode::Expanded
    } else {
        inner.default_display_mode()
    };
    inner_ctx.max_lines = None;
    inner.output(&inner_ctx)
}

fn hidden_tool_uses_text(
    count: usize,
    include_expand_hint: bool,
    language: RendererLanguage,
) -> String {
    match language {
        RendererLanguage::ZhCn if include_expand_hint => {
            format!("+{count} 次工具调用（按 ctrl+o 展开）")
        }
        RendererLanguage::ZhCn => format!("+{count} 次工具调用"),
        RendererLanguage::EnUs => {
            let noun = if count == 1 { "use" } else { "uses" };
            if include_expand_hint {
                format!("+{count} more tool {noun} (ctrl+o to expand)")
            } else {
                format!("+{count} more tool {noun}")
            }
        }
    }
}

fn nested_agent_prompt_output(
    prompt: &str,
    ctx: &BlockContext,
    theme: &Theme,
    language: RendererLanguage,
) -> BlockOutput {
    let mut lines = vec![BlockLine::styled(Line::from(Span::styled(
        language.text("提示词：", "Prompt:"),
        theme.fg(theme.accent_success).add_modifier(Modifier::BOLD),
    )))];
    let body_width = ctx.width.saturating_sub(2).max(1);
    let mut body = super::markdown_content::MarkdownContent::new(prompt.to_string())
        .output(usize::from(body_width));
    for line in &mut body.lines {
        line.content.spans.insert(0, Span::raw("  "));
        shift_selection_metadata_for_prefix(line, 1);
    }
    lines.extend(body.lines);
    BlockOutput { lines }
}

#[derive(Debug, Clone)]
pub enum CrabCodeAdvisorBlock {
    Invocation {
        input: CrabCodeToolPayload,
        state: CrabCodeAdvisorInvocationState,
    },
    Feedback {
        text: String,
    },
    Redacted,
    Error {
        error_code: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeAdvisorInvocationState {
    InProgress,
    Succeeded,
    Failed,
}

/// Renderer-complete SDK image provenance.
///
/// The projection intentionally retains no base64 payload bytes and no local
/// path authority. Consequently this block is a textual transcript row, not
/// an inline-media loader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrabCodeSdkImageBlock {
    Base64 {
        media_type: CrabCodeSdkImageMediaType,
        encoded_len: usize,
    },
    Url {
        url: String,
    },
    File {
        file_id: String,
    },
}

impl CrabCodeSdkImageBlock {
    fn display_text(&self, language: RendererLanguage) -> String {
        match self {
            Self::Base64 {
                media_type,
                encoded_len,
            } => match language {
                RendererLanguage::ZhCn => format!(
                    "{} · 编码载荷 {encoded_len} 字节（已收到完整载荷；以 CrabCode 对话记录为准）",
                    media_type.mime_type()
                ),
                RendererLanguage::EnUs => format!(
                    "{} · encoded payload {encoded_len} bytes (complete payload received; CrabCode transcript authoritative)",
                    media_type.mime_type()
                ),
            },
            Self::Url { url } => match language {
                RendererLanguage::ZhCn => format!("图片 URL · {url}"),
                RendererLanguage::EnUs => format!("image URL · {url}"),
            },
            Self::File { file_id } => match language {
                RendererLanguage::ZhCn => format!("图片文件 · {file_id}"),
                RendererLanguage::EnUs => format!("image file · {file_id}"),
            },
        }
    }

    fn searchable_text(&self) -> Option<String> {
        join_searchable([Some(self.display_text(RendererLanguage::EnUs))])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeSdkImageMediaType {
    Jpeg,
    Png,
    Gif,
    Webp,
}

impl CrabCodeSdkImageMediaType {
    fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
}

/// Classified open payload rendered without interpreting any nested key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrabCodeToolPayload {
    /// Complete structured input/result serialized from the retained `Value`.
    Json(String),
    /// A producer string result. It is not quoted or reparsed as JSON.
    Text(String),
    /// Exact streamed `partial_json` bytes. Invalid/incomplete JSON is valid.
    PartialJson(String),
    /// The classified field was explicitly JSON `null`.
    Null,
    /// The classified field was absent.
    Missing,
}

impl CrabCodeToolPayload {
    fn display_text(&self) -> Option<&str> {
        match self {
            Self::Json(text) | Self::Text(text) | Self::PartialJson(text) if !text.is_empty() => {
                Some(text)
            }
            Self::Json(_) | Self::Text(_) | Self::PartialJson(_) | Self::Null | Self::Missing => {
                None
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeToolResultTone {
    Result,
    Terminal,
}

/// Renderer field carrier for already-classified ordinary tool rows.
///
/// This keeps the fixed `BlockContent` lifecycle usable while preserving the
/// name, input/result value, optional error flag, and projected row class
/// currently available to the renderer. Complete `serde_json::Value` payloads
/// are serialized into display strings, so this is neither original-JSON-byte
/// preservation nor a typed-schema closure. It is not a substitute for
/// CrabCode's historical registry, schema validation, user-facing names, or
/// per-tool UI functions, and therefore does not by itself close tool-specific
/// presentation parity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CrabCodeToolBlock {
    Invocation {
        name: String,
        input: CrabCodeToolPayload,
    },
    Result {
        name: String,
        result: CrabCodeToolPayload,
        is_error: Option<bool>,
        tone: CrabCodeToolResultTone,
    },
    Progress {
        name: String,
        detail: String,
    },
}

impl CrabCodeToolBlock {
    fn searchable_text(&self) -> Option<String> {
        match self {
            Self::Invocation { name, input } => join_searchable([
                Some(name.clone()),
                input.display_text().map(ToString::to_string),
            ]),
            Self::Result {
                name,
                result,
                is_error: _,
                tone: _,
            } => join_searchable([
                Some(name.clone()),
                result.display_text().map(ToString::to_string),
            ]),
            Self::Progress { name, detail } => {
                join_searchable([Some(name.clone()), Some(detail.clone())])
            }
        }
    }

    fn is_error(&self) -> bool {
        matches!(
            self,
            Self::Result {
                is_error: Some(true),
                ..
            }
        )
    }
}

/// Branches whose fixed historical `Message.tsx` consumer returns `null`.
///
/// Keeping the exact content-block discriminator as a unit variant prevents a
/// future caller from treating source-null behavior as a generic fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeSourceNullBlock {
    AssistantServerToolUse,
    AssistantMcpToolUse,
    AssistantWebSearchToolResult,
    AssistantWebFetchToolResult,
    AssistantCodeExecutionToolResult,
    AssistantBashCodeExecutionToolResult,
    AssistantTextEditorCodeExecutionToolResult,
    AssistantToolSearchToolResult,
    AssistantMcpToolResult,
    AssistantContainerUpload,
    AssistantCompaction,
    UncorrelatedUserToolResult,
    SdkUserToolUse,
    DirectUserDocument,
    DirectUserThinking,
    DirectUserRedactedThinking,
    DirectUserToolUse,
    DirectUserServerToolUse,
    DirectUserMcpToolUse,
    DirectUserWebSearchToolResult,
    DirectUserWebFetchToolResult,
    DirectUserCodeExecutionToolResult,
    DirectUserBashCodeExecutionToolResult,
    DirectUserTextEditorCodeExecutionToolResult,
    DirectUserToolSearchToolResult,
    DirectUserMcpToolResult,
    DirectUserContainerUpload,
    DirectUserConnectorText,
    DirectUserAdvisorToolResult,
    DirectUserCompaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeMessageLevel {
    Info,
    Warning,
    Error,
    Suggestion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeSdkSystemSubtype {
    CompactBoundary,
    Status,
    PostTurnSummary,
    ApiRetry,
    LocalCommandOutput,
    HookStarted,
    HookProgress,
    HookResponse,
    TaskNotification,
    TaskStarted,
    TaskProgress,
    SessionStateChanged,
    FilesPersisted,
    ElicitationComplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeSdkSystemTone {
    System,
    Terminal,
    Progress,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct CrabCodeSdkSystemBlock {
    pub subtype: CrabCodeSdkSystemSubtype,
    pub tone: CrabCodeSdkSystemTone,
    pub level: Option<CrabCodeMessageLevel>,
    pub title: String,
    pub text: String,
}

impl CrabCodeSdkSystemBlock {
    fn is_foldable(&self) -> bool {
        matches!(
            self.subtype,
            CrabCodeSdkSystemSubtype::LocalCommandOutput
                | CrabCodeSdkSystemSubtype::HookProgress
                | CrabCodeSdkSystemSubtype::HookResponse
                | CrabCodeSdkSystemSubtype::TaskNotification
                | CrabCodeSdkSystemSubtype::TaskStarted
                | CrabCodeSdkSystemSubtype::TaskProgress
        )
    }
}

#[derive(Debug, Clone)]
pub struct CrabCodeDirectApiError {
    pub message: Option<String>,
    pub status: Option<Number>,
    pub nested_message: Option<String>,
    pub deeply_nested_message: Option<String>,
    pub connection_code: Option<String>,
}

#[derive(Debug, Clone)]
pub enum CrabCodeDirectSystemBlock {
    Informational {
        content: String,
        level: Option<CrabCodeMessageLevel>,
    },
    PermissionRetry {
        commands: Vec<String>,
    },
    ScheduledTaskFire {
        content: String,
    },
    StopHookSummary {
        hook_count: Number,
        hook_info_count: usize,
        hook_errors: Vec<String>,
        prevented_continuation: bool,
        stop_reason: Option<String>,
        hook_label: Option<String>,
    },
    TurnDuration {
        duration_ms: Number,
        budget_tokens: Option<Number>,
        budget_limit: Option<Number>,
        budget_nudges: Option<Number>,
        show_turn_duration: bool,
        completion_verb: String,
    },
    ApiError {
        error: CrabCodeDirectApiError,
        retry_in_ms: Number,
        retry_attempt: Number,
        max_retries: Number,
        mounted_at: Instant,
    },
    AwaySummary {
        content: String,
    },
    MemorySaved {
        written_paths: Vec<String>,
        team_count: Option<Number>,
    },
    AgentsKilled,
    ApiMetrics,
    CompactBoundary,
    MicrocompactBoundary,
    CommandInput {
        content: String,
        level: Option<CrabCodeMessageLevel>,
    },
    Thinking,
    FileSnapshot {
        content: String,
        level: Option<CrabCodeMessageLevel>,
    },
}

impl CrabCodeDirectSystemBlock {
    /// Mount one fixed historical turn-duration component.
    ///
    /// The source uses `lodash/sample` inside a React `useState` initializer:
    /// choose one member now, then let the projection adapter retain that value
    /// for the component/entry lifetime.
    pub fn turn_duration(
        duration_ms: Number,
        budget_tokens: Option<Number>,
        budget_limit: Option<Number>,
        budget_nudges: Option<Number>,
        show_turn_duration: bool,
    ) -> Self {
        let completion_verb = TURN_COMPLETION_VERBS
            [rand::rng().random_range(0..TURN_COMPLETION_VERBS.len())]
        .to_string();
        Self::TurnDuration {
            duration_ms,
            budget_tokens,
            budget_limit,
            budget_nudges,
            show_turn_duration,
            completion_verb,
        }
    }

    fn is_hidden(&self) -> bool {
        match self {
            Self::ApiMetrics
            | Self::CompactBoundary
            | Self::MicrocompactBoundary
            | Self::Thinking => true,
            Self::StopHookSummary {
                hook_errors,
                prevented_continuation,
                hook_label,
                ..
            } => {
                hook_errors.is_empty()
                    && !prevented_continuation
                    && hook_label.as_deref().is_none_or(str::is_empty)
            }
            Self::ApiError { retry_attempt, .. } => {
                retry_attempt.as_f64().is_some_and(|attempt| attempt < 4.0)
            }
            Self::TurnDuration {
                budget_limit,
                show_turn_duration,
                ..
            } => !show_turn_duration && budget_limit.is_none(),
            Self::Informational { .. }
            | Self::PermissionRetry { .. }
            | Self::ScheduledTaskFire { .. }
            | Self::AwaySummary { .. }
            | Self::MemorySaved { .. }
            | Self::AgentsKilled
            | Self::CommandInput { .. }
            | Self::FileSnapshot { .. } => false,
        }
    }

    fn searchable_text(&self) -> Option<String> {
        match self {
            Self::Informational { content, .. }
            | Self::ScheduledTaskFire { content }
            | Self::AwaySummary { content }
            | Self::CommandInput { content, .. }
            | Self::FileSnapshot { content, .. } => join_searchable([Some(content.clone())]),
            Self::StopHookSummary {
                hook_count,
                hook_info_count,
                hook_errors,
                stop_reason,
                hook_label,
                ..
            } => join_searchable(
                [
                    Some(format!(
                        "Ran {} {} {}",
                        display_json_number(hook_count),
                        hook_label
                            .as_deref()
                            .filter(|label| !label.is_empty())
                            .unwrap_or("stop"),
                        if hook_count.as_f64() == Some(1.0) {
                            "hook"
                        } else {
                            "hooks"
                        }
                    )),
                    stop_reason.clone(),
                ]
                .into_iter()
                .chain((*hook_info_count > 0).then_some(Some("⎿".to_string())))
                .chain(hook_errors.iter().cloned().map(Some)),
            ),
            Self::TurnDuration {
                duration_ms,
                budget_tokens,
                budget_limit,
                budget_nudges,
                show_turn_duration,
                completion_verb,
            } => turn_duration_text(
                duration_ms,
                budget_tokens.as_ref(),
                budget_limit.as_ref(),
                budget_nudges.as_ref(),
                *show_turn_duration,
                completion_verb,
                RendererLanguage::EnUs,
            ),
            Self::ApiError {
                error,
                retry_in_ms,
                retry_attempt,
                max_retries,
                mounted_at: _,
            } => join_searchable([
                Some(format_direct_api_error(error)),
                Some(display_json_number(retry_in_ms)),
                Some(display_json_number(retry_attempt)),
                Some(display_json_number(max_retries)),
            ]),
            Self::MemorySaved {
                written_paths,
                team_count: _,
            } => join_searchable(
                std::iter::once(Some("Memories saved".to_string()))
                    .chain(written_paths.iter().cloned().map(Some)),
            ),
            Self::PermissionRetry { commands } => {
                join_searchable([Some("Allowed".to_string()), Some(commands.join(", "))])
            }
            Self::AgentsKilled => {
                join_searchable([Some("All background agents stopped".to_string())])
            }
            Self::ApiMetrics
            | Self::CompactBoundary
            | Self::MicrocompactBoundary
            | Self::Thinking => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum CrabCodeDirectProgressBlock {
    Shell {
        output: String,
        full_output: String,
        elapsed_time_seconds: Number,
        total_lines: Number,
        total_bytes: Option<Number>,
        timeout_ms: Option<Number>,
    },
    Mcp {
        progress: Option<Number>,
        total: Option<Number>,
        progress_message: Option<String>,
        percentage: Option<u8>,
        server_name: String,
        tool_name: String,
    },
    SearchQuery {
        query: String,
    },
    SearchResults {
        query: String,
        result_count: u64,
    },
    WaitingForTask {
        task_description: String,
        task_type: String,
    },
    Hook {
        hook_event: String,
        in_progress_count: usize,
        resolved_count: usize,
    },
}

impl CrabCodeDirectProgressBlock {
    fn is_hidden(&self) -> bool {
        matches!(
            self,
            Self::Hook {
                hook_event,
                in_progress_count,
                resolved_count,
            } if *in_progress_count == 0
                || matches!(hook_event.as_str(), "PreToolUse" | "PostToolUse")
                || resolved_count == in_progress_count
        )
    }

    fn searchable_text(&self) -> Option<String> {
        match self {
            Self::Shell {
                output,
                full_output,
                ..
            } => join_searchable([Some(output.clone()), Some(full_output.clone())]),
            Self::Mcp {
                progress_message,
                server_name,
                tool_name,
                ..
            } => join_searchable([
                Some(server_name.clone()),
                Some(tool_name.clone()),
                progress_message.clone(),
            ]),
            Self::SearchQuery { query } | Self::SearchResults { query, .. } => {
                join_searchable([Some(query.clone())])
            }
            Self::WaitingForTask {
                task_description,
                task_type,
            } => join_searchable([
                Some(task_description.clone()),
                Some(task_type.clone()),
                Some("Waiting for task".to_string()),
            ]),
            Self::Hook {
                hook_event,
                in_progress_count,
                ..
            } if *in_progress_count > 0 => join_searchable([Some(hook_event.clone())]),
            Self::Hook { .. } => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum CrabCodeDirectFileContent {
    Notebook { cell_count: usize },
    Unchanged,
    Text { line_count: u64, truncated: bool },
    Binary { original_size: Number },
}

#[derive(Debug, Clone)]
pub struct CrabCodeRelevantMemory {
    pub path: String,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeDiagnosticSeverity {
    Error,
    Warning,
    Info,
    Hint,
}

#[derive(Debug, Clone)]
pub struct CrabCodeDiagnostic {
    pub message: String,
    pub severity: CrabCodeDiagnosticSeverity,
    pub start_line: Number,
    pub start_character: Number,
    pub code: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CrabCodeDiagnosticFile {
    pub uri: String,
    pub diagnostics: Vec<CrabCodeDiagnostic>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeHookPermissionDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrabCodeTaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Killed,
}

#[derive(Debug, Clone)]
pub enum CrabCodeDirectAttachmentBlock {
    Directory {
        display_path: String,
    },
    File {
        display_path: String,
        content: CrabCodeDirectFileContent,
    },
    CompactFileReference {
        display_path: String,
    },
    PdfReference {
        display_path: String,
        page_count: u64,
    },
    SelectedLines {
        ide_name: String,
        line_start: u64,
        line_end: u64,
        display_path: String,
    },
    NestedMemory {
        display_path: String,
    },
    RelevantMemories {
        memories: Vec<CrabCodeRelevantMemory>,
    },
    DynamicSkill {
        skill_names: Vec<String>,
        display_path: String,
    },
    SkillListing {
        skill_count: u64,
        is_initial: bool,
    },
    AgentListingDelta {
        added_types: Vec<String>,
        is_initial: bool,
    },
    QueuedCommand {
        text: Box<RenderBlock>,
        text_is_hidden: bool,
        image_paste_ids: Vec<u64>,
    },
    PlanFileReference {
        plan_file_path: String,
    },
    InvokedSkills {
        skill_names: Vec<String>,
    },
    Diagnostics {
        files: Vec<CrabCodeDiagnosticFile>,
    },
    McpResource {
        name: String,
        server: String,
        uri: String,
    },
    HookBlockingError {
        hook_name: String,
        hook_event: String,
        blocking_error: String,
    },
    HookNonBlockingError {
        hook_name: String,
        hook_event: String,
    },
    HookErrorDuringExecution {
        hook_name: String,
        hook_event: String,
    },
    HookStoppedContinuation {
        hook_name: String,
        hook_event: String,
        message: String,
    },
    HookSystemMessage {
        hook_name: String,
        content: String,
    },
    HookPermissionDecision {
        hook_event: String,
        decision: CrabCodeHookPermissionDecision,
    },
    TaskStatus {
        status: CrabCodeTaskStatus,
        description: String,
    },
    TeammateShutdownBatch {
        count: u64,
    },
}

impl CrabCodeDirectAttachmentBlock {
    fn is_hidden(&self) -> bool {
        match self {
            Self::SkillListing { is_initial, .. } => *is_initial,
            Self::AgentListingDelta {
                added_types,
                is_initial,
            } => *is_initial || added_types.is_empty(),
            Self::InvokedSkills { skill_names } => skill_names.is_empty(),
            Self::QueuedCommand {
                text_is_hidden,
                image_paste_ids,
                ..
            } => *text_is_hidden && image_paste_ids.is_empty(),
            Self::Diagnostics { files } => files.is_empty(),
            Self::HookBlockingError { hook_event, .. }
            | Self::HookNonBlockingError { hook_event, .. }
            | Self::HookErrorDuringExecution { hook_event, .. }
            | Self::HookStoppedContinuation { hook_event, .. } => {
                matches!(hook_event.as_str(), "Stop" | "SubagentStop")
            }
            Self::Directory { .. }
            | Self::File { .. }
            | Self::CompactFileReference { .. }
            | Self::PdfReference { .. }
            | Self::SelectedLines { .. }
            | Self::NestedMemory { .. }
            | Self::RelevantMemories { .. }
            | Self::DynamicSkill { .. }
            | Self::PlanFileReference { .. }
            | Self::McpResource { .. }
            | Self::HookSystemMessage { .. }
            | Self::HookPermissionDecision { .. }
            | Self::TaskStatus { .. }
            | Self::TeammateShutdownBatch { .. } => false,
        }
    }

    fn searchable_text(&self) -> Option<String> {
        match self {
            Self::Directory { display_path }
            | Self::CompactFileReference { display_path }
            | Self::NestedMemory { display_path } => join_searchable([Some(display_path.clone())]),
            Self::File {
                display_path,
                content: _,
            } => join_searchable([Some(display_path.clone())]),
            Self::PdfReference {
                display_path,
                page_count: _,
            }
            | Self::SelectedLines {
                display_path,
                ide_name: _,
                line_start: _,
                line_end: _,
            } => join_searchable([Some(display_path.clone())]),
            Self::RelevantMemories { memories } => {
                join_searchable(memories.iter().map(|memory| Some(memory.path.clone())))
            }
            Self::DynamicSkill {
                skill_names,
                display_path,
            } => join_searchable(
                std::iter::once(Some(display_path.clone()))
                    .chain(skill_names.iter().cloned().map(Some)),
            ),
            Self::SkillListing {
                skill_count,
                is_initial: false,
            } => join_searchable([Some(format!("{skill_count} skills available"))]),
            Self::AgentListingDelta {
                added_types,
                is_initial: false,
            } if !added_types.is_empty() => join_searchable(added_types.iter().cloned().map(Some)),
            Self::QueuedCommand {
                text,
                text_is_hidden,
                image_paste_ids,
            } => join_searchable(
                std::iter::once((!text_is_hidden).then(|| text.searchable_text()).flatten()).chain(
                    image_paste_ids
                        .iter()
                        .map(|id| Some(image_label(Some(*id), RendererLanguage::EnUs))),
                ),
            ),
            Self::PlanFileReference { plan_file_path } => {
                join_searchable([Some(plan_file_path.clone())])
            }
            Self::InvokedSkills { skill_names } => {
                join_searchable(skill_names.iter().cloned().map(Some))
            }
            Self::Diagnostics { files } => join_searchable(files.iter().flat_map(|file| {
                std::iter::once(Some(file.uri.clone())).chain(
                    file.diagnostics
                        .iter()
                        .flat_map(|diagnostic| {
                            [
                                Some(diagnostic.message.clone()),
                                diagnostic.code.clone(),
                                diagnostic.source.clone(),
                            ]
                        })
                        .collect::<Vec<_>>(),
                )
            })),
            Self::McpResource { name, server, uri } => {
                join_searchable([Some(name.clone()), Some(server.clone()), Some(uri.clone())])
            }
            Self::HookBlockingError {
                hook_name,
                blocking_error,
                ..
            } => join_searchable([Some(hook_name.clone()), Some(blocking_error.clone())]),
            Self::HookNonBlockingError { hook_name, .. }
            | Self::HookErrorDuringExecution { hook_name, .. } => {
                join_searchable([Some(hook_name.clone())])
            }
            Self::HookStoppedContinuation {
                hook_name, message, ..
            }
            | Self::HookSystemMessage {
                hook_name,
                content: message,
            } => join_searchable([Some(hook_name.clone()), Some(message.clone())]),
            Self::HookPermissionDecision { hook_event, .. } => {
                join_searchable([Some(hook_event.clone())])
            }
            Self::TaskStatus {
                description,
                status: _,
            } => join_searchable([Some(description.clone())]),
            Self::TeammateShutdownBatch { count } => {
                join_searchable([Some(format!("{count} teammates shut down gracefully"))])
            }
            Self::SkillListing {
                is_initial: true, ..
            }
            | Self::AgentListingDelta { .. } => None,
        }
    }
}

impl BlockContent for CrabCodeProjectionBlock {
    fn output(&self, ctx: &BlockContext) -> BlockOutput {
        if self.is_hidden() {
            return BlockOutput::new();
        }

        let theme = Theme::current();
        let language = ctx.appearance.language;
        if let CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::MemorySaved {
            written_paths,
            team_count,
        }) = &self.kind
        {
            return memory_saved_output(written_paths, team_count.as_ref(), ctx, &theme, language);
        }
        if let CrabCodeProjectionKind::DirectNestedProgress(progress) = &self.kind {
            return progress.output(ctx, &theme, language);
        }
        let lines = match &self.kind {
            CrabCodeProjectionKind::Advisor(advisor) => {
                advisor_lines(advisor, ctx, &theme, language)
            }
            CrabCodeProjectionKind::RedactedThinking => vec![Line::from(Span::styled(
                language.text("✻ 思考中…", "✻ Thinking…"),
                theme.muted().add_modifier(Modifier::ITALIC),
            ))],
            CrabCodeProjectionKind::UserImage { image_id } => {
                vec![styled_safe_line(
                    image_label(*image_id, language),
                    theme.primary(),
                )]
            }
            CrabCodeProjectionKind::SdkImage(image) => {
                vec![styled_safe_line(
                    image.display_text(language),
                    theme.primary(),
                )]
            }
            CrabCodeProjectionKind::Tool(tool) => tool_lines(tool, ctx, &theme, language),
            CrabCodeProjectionKind::SdkSystem(system) => sdk_system_lines(system, ctx, &theme),
            CrabCodeProjectionKind::DirectSystem(system) => {
                direct_system_lines(system, ctx, &theme, language)
            }
            CrabCodeProjectionKind::DirectProgress(progress) => {
                direct_progress_lines(progress, ctx, &theme, language)
            }
            CrabCodeProjectionKind::DirectNestedProgress(_) => {
                unreachable!("nested progress returns before generic line wrapping")
            }
            CrabCodeProjectionKind::DirectAttachment(attachment) => {
                direct_attachment_lines(attachment, ctx, &theme, language)
            }
            CrabCodeProjectionKind::SourceNull(_) => Vec::new(),
        };
        let wrapped = wrap_lines(lines, ctx.width);
        BlockOutput {
            lines: truncate_lines(wrapped, ctx.max_lines, theme.muted()),
        }
    }

    fn accent(&self, ctx: &BlockContext) -> Option<AccentStyle> {
        let theme = Theme::current();
        match &self.kind {
            CrabCodeProjectionKind::Advisor(CrabCodeAdvisorBlock::Invocation {
                state: CrabCodeAdvisorInvocationState::Failed,
                ..
            }) => Some(AccentStyle::static_color(theme.accent_error)),
            CrabCodeProjectionKind::Advisor(CrabCodeAdvisorBlock::Invocation {
                state: CrabCodeAdvisorInvocationState::InProgress,
                ..
            }) => Some(AccentStyle::animated(theme.accent_running)),
            CrabCodeProjectionKind::Advisor(CrabCodeAdvisorBlock::Error { .. }) => {
                Some(AccentStyle::static_color(theme.accent_error))
            }
            CrabCodeProjectionKind::Tool(tool) if tool.is_error() => {
                Some(AccentStyle::static_color(theme.accent_error))
            }
            CrabCodeProjectionKind::Tool(_) if ctx.is_running => {
                Some(AccentStyle::animated(theme.accent_running))
            }
            CrabCodeProjectionKind::Tool(_) => Some(AccentStyle::static_color(theme.accent_tool)),
            CrabCodeProjectionKind::SdkSystem(system) => match system.tone {
                CrabCodeSdkSystemTone::Error => Some(AccentStyle::static_color(theme.accent_error)),
                CrabCodeSdkSystemTone::Warning => Some(AccentStyle::static_color(theme.warning)),
                CrabCodeSdkSystemTone::Progress if ctx.is_running => {
                    Some(AccentStyle::animated(theme.accent_running))
                }
                CrabCodeSdkSystemTone::System
                | CrabCodeSdkSystemTone::Terminal
                | CrabCodeSdkSystemTone::Progress => None,
            },
            CrabCodeProjectionKind::DirectProgress(_) if ctx.is_running => {
                Some(AccentStyle::animated(theme.accent_running))
            }
            CrabCodeProjectionKind::DirectNestedProgress(progress) => progress.accent(ctx),
            CrabCodeProjectionKind::DirectAttachment(
                CrabCodeDirectAttachmentBlock::HookBlockingError { .. }
                | CrabCodeDirectAttachmentBlock::HookNonBlockingError { .. },
            ) => Some(AccentStyle::static_color(theme.accent_error)),
            CrabCodeProjectionKind::DirectAttachment(
                CrabCodeDirectAttachmentBlock::HookErrorDuringExecution { .. }
                | CrabCodeDirectAttachmentBlock::HookStoppedContinuation { .. },
            ) => Some(AccentStyle::static_color(theme.warning)),
            CrabCodeProjectionKind::Advisor(
                CrabCodeAdvisorBlock::Invocation {
                    state: CrabCodeAdvisorInvocationState::Succeeded,
                    ..
                }
                | CrabCodeAdvisorBlock::Feedback { .. }
                | CrabCodeAdvisorBlock::Redacted,
            )
            | CrabCodeProjectionKind::RedactedThinking
            | CrabCodeProjectionKind::UserImage { .. }
            | CrabCodeProjectionKind::SdkImage(_)
            | CrabCodeProjectionKind::DirectSystem(_)
            | CrabCodeProjectionKind::DirectProgress(_)
            | CrabCodeProjectionKind::DirectAttachment(_)
            | CrabCodeProjectionKind::SourceNull(_) => None,
        }
    }

    fn has_vpad_for(&self, _appearance: &crate::appearance::AppearanceConfig) -> bool {
        false
    }

    fn has_raw_mode(&self) -> bool {
        false
    }

    fn is_foldable(&self) -> bool {
        match &self.kind {
            CrabCodeProjectionKind::Advisor(CrabCodeAdvisorBlock::Invocation { input, .. }) => {
                input.display_text().is_some()
            }
            CrabCodeProjectionKind::Advisor(CrabCodeAdvisorBlock::Feedback { .. }) => true,
            CrabCodeProjectionKind::Tool(tool) => match tool {
                CrabCodeToolBlock::Invocation { input, .. } => input.display_text().is_some(),
                CrabCodeToolBlock::Result {
                    result, is_error, ..
                } => !matches!(is_error, Some(true)) && result.display_text().is_some(),
                CrabCodeToolBlock::Progress { .. } => false,
            },
            CrabCodeProjectionKind::SdkSystem(system) => system.is_foldable(),
            CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::StopHookSummary {
                hook_info_count,
                ..
            }) => *hook_info_count > 0,
            CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::ApiError {
                error,
                ..
            }) => format_direct_api_error(error).encode_utf16().count() > 1_000,
            CrabCodeProjectionKind::DirectProgress(CrabCodeDirectProgressBlock::Shell {
                ..
            }) => true,
            CrabCodeProjectionKind::DirectAttachment(
                CrabCodeDirectAttachmentBlock::RelevantMemories { .. }
                | CrabCodeDirectAttachmentBlock::Diagnostics { .. },
            ) => true,
            CrabCodeProjectionKind::Advisor(
                CrabCodeAdvisorBlock::Redacted | CrabCodeAdvisorBlock::Error { .. },
            )
            | CrabCodeProjectionKind::DirectNestedProgress(_)
            | CrabCodeProjectionKind::RedactedThinking
            | CrabCodeProjectionKind::UserImage { .. }
            | CrabCodeProjectionKind::SdkImage(_)
            | CrabCodeProjectionKind::DirectSystem(_)
            | CrabCodeProjectionKind::DirectProgress(_)
            | CrabCodeProjectionKind::DirectAttachment(_)
            | CrabCodeProjectionKind::SourceNull(_) => false,
        }
    }

    fn default_display_mode(&self) -> DisplayMode {
        match &self.kind {
            CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::ApiError {
                ..
            }) => DisplayMode::Collapsed,
            CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::StopHookSummary {
                hook_info_count,
                ..
            }) => {
                if *hook_info_count == 0 {
                    DisplayMode::Expanded
                } else {
                    DisplayMode::Collapsed
                }
            }
            CrabCodeProjectionKind::Advisor(CrabCodeAdvisorBlock::Invocation { .. })
            | CrabCodeProjectionKind::Advisor(CrabCodeAdvisorBlock::Feedback { .. })
            | CrabCodeProjectionKind::Tool(CrabCodeToolBlock::Invocation { .. })
            | CrabCodeProjectionKind::DirectProgress(CrabCodeDirectProgressBlock::Shell {
                ..
            })
            | CrabCodeProjectionKind::DirectAttachment(
                CrabCodeDirectAttachmentBlock::RelevantMemories { .. }
                | CrabCodeDirectAttachmentBlock::Diagnostics { .. },
            ) => DisplayMode::Collapsed,
            CrabCodeProjectionKind::Tool(CrabCodeToolBlock::Result { is_error, tone, .. }) => {
                if matches!(is_error, Some(true)) || *tone == CrabCodeToolResultTone::Terminal {
                    DisplayMode::Expanded
                } else {
                    DisplayMode::Collapsed
                }
            }
            CrabCodeProjectionKind::Tool(CrabCodeToolBlock::Progress { .. }) => {
                DisplayMode::Expanded
            }
            CrabCodeProjectionKind::Advisor(
                CrabCodeAdvisorBlock::Redacted | CrabCodeAdvisorBlock::Error { .. },
            )
            | CrabCodeProjectionKind::DirectNestedProgress(_)
            | CrabCodeProjectionKind::RedactedThinking
            | CrabCodeProjectionKind::UserImage { .. }
            | CrabCodeProjectionKind::SdkImage(_)
            | CrabCodeProjectionKind::SdkSystem(_)
            | CrabCodeProjectionKind::DirectSystem(_)
            | CrabCodeProjectionKind::DirectProgress(_)
            | CrabCodeProjectionKind::DirectAttachment(_)
            | CrabCodeProjectionKind::SourceNull(_) => DisplayMode::Expanded,
        }
    }

    fn is_selectable(&self) -> bool {
        match &self.kind {
            CrabCodeProjectionKind::DirectNestedProgress(progress) => progress.is_selectable(),
            _ => !self.is_hidden(),
        }
    }

    fn is_groupable(&self) -> bool {
        !matches!(
            self.kind,
            CrabCodeProjectionKind::UserImage { .. }
                | CrabCodeProjectionKind::SdkImage(_)
                | CrabCodeProjectionKind::DirectAttachment(
                    CrabCodeDirectAttachmentBlock::QueuedCommand { .. }
                )
        )
    }
}

fn advisor_lines(
    advisor: &CrabCodeAdvisorBlock,
    ctx: &BlockContext,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    match advisor {
        CrabCodeAdvisorBlock::Invocation { input, state } => {
            let (glyph, style) = match state {
                CrabCodeAdvisorInvocationState::InProgress => ("● ", theme.fg(theme.running)),
                CrabCodeAdvisorInvocationState::Succeeded => ("✓ ", theme.fg(theme.accent_success)),
                CrabCodeAdvisorInvocationState::Failed => ("● ", theme.fg(theme.accent_error)),
            };
            let mut lines = vec![Line::from(vec![
                Span::styled(glyph, style),
                Span::styled(
                    language.text("顾问分析中", "Advising"),
                    style.add_modifier(Modifier::BOLD),
                ),
            ])];
            if ctx.mode == DisplayMode::Expanded
                && let Some(input) = input.display_text()
            {
                lines.extend(safe_text_lines(input, theme.muted()));
            }
            lines
        }
        CrabCodeAdvisorBlock::Feedback { text } if ctx.mode == DisplayMode::Expanded => {
            safe_text_lines(text, theme.muted())
        }
        CrabCodeAdvisorBlock::Feedback { .. } | CrabCodeAdvisorBlock::Redacted => {
            vec![Line::from(vec![
                Span::styled("✓ ", theme.fg(theme.accent_success)),
                safe_span(
                    language.text(ADVISOR_REVIEWED_MESSAGE_ZH, ADVISOR_REVIEWED_MESSAGE_EN),
                    theme.muted(),
                ),
            ])]
        }
        CrabCodeAdvisorBlock::Error { error_code } => {
            let text = match language {
                RendererLanguage::ZhCn => format!("顾问不可用（{error_code}）"),
                RendererLanguage::EnUs => format!("Advisor unavailable ({error_code})"),
            };
            vec![styled_safe_line(text, theme.fg(theme.accent_error))]
        }
    }
}

fn tool_lines(
    tool: &CrabCodeToolBlock,
    ctx: &BlockContext,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    let (name, suffix, payload, detail) = match tool {
        CrabCodeToolBlock::Invocation { name, input } => (name, "", Some(input), None),
        CrabCodeToolBlock::Result {
            name,
            result,
            is_error,
            tone,
        } => {
            let suffix = if matches!(is_error, Some(true)) {
                language.text(" 失败", " failed")
            } else if *tone == CrabCodeToolResultTone::Terminal {
                language.text(" 输出", " output")
            } else {
                language.text(" 结果", " result")
            };
            (name, suffix, Some(result), None)
        }
        CrabCodeToolBlock::Progress { name, detail } => (
            name,
            language.text(" 进行中", " in progress"),
            None,
            Some(detail.as_str()),
        ),
    };

    let header_style = if tool.is_error() {
        theme.fg(theme.accent_error)
    } else {
        theme.primary()
    };
    let mut header = vec![safe_span(name, header_style.add_modifier(Modifier::BOLD))];
    if !suffix.is_empty() {
        header.push(Span::styled(suffix, theme.muted()));
    }
    if ctx.is_running {
        header.push(Span::styled(" …", theme.fg(theme.accent_running)));
    }
    let mut lines = vec![Line::from(header)];

    if let Some(detail) = detail.filter(|detail| !detail.is_empty()) {
        lines.extend(safe_text_lines(detail, theme.muted()));
    } else if ctx.mode != DisplayMode::Collapsed
        && let Some(payload) = payload.and_then(CrabCodeToolPayload::display_text)
    {
        let style = if tool.is_error() {
            theme.fg(theme.accent_error)
        } else {
            theme.muted()
        };
        lines.extend(safe_text_lines(payload, style));
    }
    lines
}

fn sdk_system_lines(
    system: &CrabCodeSdkSystemBlock,
    ctx: &BlockContext,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let tone = sdk_tone_style(system, theme);
    let mut header = vec![safe_span(&system.title, tone.add_modifier(Modifier::BOLD))];
    if ctx.is_running {
        header.push(Span::styled(" …", theme.fg(theme.accent_running)));
    }
    let mut lines = vec![Line::from(header)];
    if (!system.is_foldable() || ctx.mode != DisplayMode::Collapsed) && !system.text.is_empty() {
        lines.extend(safe_text_lines(&system.text, tone));
    }
    lines
}

fn sdk_tone_style(system: &CrabCodeSdkSystemBlock, theme: &Theme) -> Style {
    match system.level {
        Some(CrabCodeMessageLevel::Error) => theme.fg(theme.accent_error),
        Some(CrabCodeMessageLevel::Warning) => theme.fg(theme.warning),
        Some(CrabCodeMessageLevel::Suggestion) => theme.fg(theme.accent_plan),
        Some(CrabCodeMessageLevel::Info) | None => match system.tone {
            CrabCodeSdkSystemTone::Error => theme.fg(theme.accent_error),
            CrabCodeSdkSystemTone::Warning => theme.fg(theme.warning),
            CrabCodeSdkSystemTone::Progress => theme.fg(theme.running),
            CrabCodeSdkSystemTone::System | CrabCodeSdkSystemTone::Terminal => theme.muted(),
        },
    }
}

fn direct_system_lines(
    system: &CrabCodeDirectSystemBlock,
    ctx: &BlockContext,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    match system {
        CrabCodeDirectSystemBlock::Informational { content, level }
        | CrabCodeDirectSystemBlock::CommandInput { content, level }
        | CrabCodeDirectSystemBlock::FileSnapshot { content, level } => {
            generic_direct_system_lines(content, *level, theme)
        }
        CrabCodeDirectSystemBlock::PermissionRetry { commands } => vec![Line::from(vec![
            Span::styled("✻ ", theme.muted()),
            Span::styled(language.text("已允许 ", "Allowed "), theme.primary()),
            safe_span(
                commands.join(", "),
                theme.primary().add_modifier(Modifier::BOLD),
            ),
        ])],
        CrabCodeDirectSystemBlock::ScheduledTaskFire { content } => vec![Line::from(vec![
            Span::styled("✻ ", theme.muted()),
            safe_span(content, theme.muted()),
        ])],
        CrabCodeDirectSystemBlock::StopHookSummary {
            hook_count,
            hook_info_count,
            hook_errors,
            prevented_continuation,
            stop_reason,
            hook_label,
        } => {
            if let Some(label) = hook_label.as_deref().filter(|label| !label.is_empty()) {
                let hook_word = match (language, hook_count.as_f64() == Some(1.0)) {
                    (RendererLanguage::ZhCn, true) => "1 个 hook",
                    (RendererLanguage::ZhCn, false) => "{count} 个 hook",
                    (RendererLanguage::EnUs, true) => "1 hook",
                    (RendererLanguage::EnUs, false) => "{count} hooks",
                };
                let text = match language {
                    RendererLanguage::ZhCn => format!(
                        "  ⏿  已运行 {} {label} {hook_word}",
                        display_json_number(hook_count),
                    ),
                    RendererLanguage::EnUs => format!(
                        "  ⏿  Ran {} {label} {hook_word}",
                        display_json_number(hook_count),
                    ),
                };
                let mut lines = vec![styled_safe_line(text, theme.muted())];
                if ctx.mode == DisplayMode::Expanded {
                    lines.extend(
                        (0..*hook_info_count).map(|_| styled_safe_line("     ⏿ ", theme.muted())),
                    );
                }
                return lines;
            }

            let hook_word = match (language, hook_count.as_f64() == Some(1.0)) {
                (RendererLanguage::ZhCn, true) => "1 个 hook",
                (RendererLanguage::ZhCn, false) => "{count} 个 hook",
                (RendererLanguage::EnUs, true) => "1 hook",
                (RendererLanguage::EnUs, false) => "{count} hooks",
            };
            let mut header = match language {
                RendererLanguage::ZhCn => {
                    format!(
                        "● 已运行 {} stop {hook_word}",
                        display_json_number(hook_count)
                    )
                }
                RendererLanguage::EnUs => {
                    format!("● Ran {} stop {hook_word}", display_json_number(hook_count),)
                }
            };
            if *hook_info_count > 0 && ctx.mode != DisplayMode::Expanded {
                header.push_str(language.text("（按 ctrl+o 展开）", " (ctrl+o to expand)"));
            }
            let mut lines = vec![styled_safe_line(header, theme.primary())];
            if ctx.mode == DisplayMode::Expanded {
                lines.extend((0..*hook_info_count).map(|_| styled_safe_line("⎿  ", theme.muted())));
            }
            if *prevented_continuation
                && let Some(reason) = stop_reason.as_deref().filter(|reason| !reason.is_empty())
            {
                lines.push(styled_safe_line(format!("⎿  {reason}"), theme.primary()));
            }
            lines.extend(hook_errors.iter().map(|error| {
                let text = match language {
                    RendererLanguage::ZhCn => format!("⎿  停止 hook Hook 错误: {error}"),
                    RendererLanguage::EnUs => {
                        format!("⎿  Stop hook Hook error: {error}")
                    }
                };
                styled_safe_line(text, theme.primary())
            }));
            lines
        }
        CrabCodeDirectSystemBlock::TurnDuration {
            duration_ms,
            budget_tokens,
            budget_limit,
            budget_nudges,
            show_turn_duration,
            completion_verb,
        } => turn_duration_text(
            duration_ms,
            budget_tokens.as_ref(),
            budget_limit.as_ref(),
            budget_nudges.as_ref(),
            *show_turn_duration,
            completion_verb,
            language,
        )
        .map(|text| {
            Line::from(vec![
                Span::styled("✻ ", theme.muted()),
                safe_span(text, theme.muted()),
            ])
        })
        .into_iter()
        .collect(),
        CrabCodeDirectSystemBlock::ApiError {
            error,
            retry_in_ms,
            retry_attempt,
            max_retries,
            mounted_at,
        } => direct_api_error_lines(
            DirectApiRetryView {
                error,
                retry_in_ms,
                retry_attempt,
                max_retries,
                mounted_at: *mounted_at,
            },
            ctx.mode == DisplayMode::Expanded,
            theme,
            language,
        ),
        CrabCodeDirectSystemBlock::AwaySummary { content } => vec![Line::from(vec![
            Span::styled("※ ", theme.muted()),
            safe_span(content, theme.muted()),
        ])],
        CrabCodeDirectSystemBlock::MemorySaved {
            written_paths,
            team_count,
        } => memory_saved_lines(written_paths, team_count.as_ref(), theme, language),
        CrabCodeDirectSystemBlock::AgentsKilled => vec![Line::from(vec![
            Span::styled("● ", theme.fg(theme.accent_error)),
            Span::styled(
                language.text("所有后台代理已停止", "All background agents stopped"),
                theme.muted(),
            ),
        ])],
        CrabCodeDirectSystemBlock::ApiMetrics
        | CrabCodeDirectSystemBlock::CompactBoundary
        | CrabCodeDirectSystemBlock::MicrocompactBoundary
        | CrabCodeDirectSystemBlock::Thinking => Vec::new(),
    }
}

fn turn_duration_text(
    duration_ms: &Number,
    budget_tokens: Option<&Number>,
    budget_limit: Option<&Number>,
    budget_nudges: Option<&Number>,
    show_turn_duration: bool,
    completion_verb: &str,
    language: RendererLanguage,
) -> Option<String> {
    if !show_turn_duration && budget_limit.is_none() {
        return None;
    }

    let mut text = String::new();
    if show_turn_duration {
        let duration = duration_ms.as_f64().map_or_else(
            || duration_ms.to_string(),
            |value| format_duration(value, false),
        );
        text.push_str(completion_verb);
        text.push(' ');
        text.push_str(language.text("轮次", "Turn for"));
        text.push(' ');
        text.push_str(&duration);
    }

    if let Some(limit) = budget_limit {
        if show_turn_duration {
            text.push_str(" · ");
        }
        let tokens_value = budget_tokens.and_then(Number::as_f64).unwrap_or(f64::NAN);
        let limit_value = limit.as_f64().unwrap_or(f64::NAN);
        if tokens_value >= limit_value {
            text.push_str(&format!(
                "{} used ({} min ✔)",
                format_turn_budget_number(budget_tokens),
                format_turn_budget_number(Some(limit)),
            ));
        } else {
            let percentage = js_numeric_text(js_math_round(tokens_value / limit_value * 100.0));
            text.push_str(&format!(
                "{} / {} ({percentage}%)",
                format_turn_budget_number(budget_tokens),
                format_turn_budget_number(Some(limit)),
            ));
        }

        if let Some(nudges) = budget_nudges
            && nudges.as_f64().is_some_and(|value| value > 0.0)
        {
            let label = match (language, nudges.as_f64() == Some(1.0)) {
                (RendererLanguage::ZhCn, true) => "1 个提示",
                (RendererLanguage::ZhCn, false) => "{count} 个提示",
                (RendererLanguage::EnUs, true) => "1 nudge",
                (RendererLanguage::EnUs, false) => "{count} nudges",
            };
            text.push_str(&format!(" · {} {label}", display_json_number(nudges)));
        }
    }

    Some(text)
}

fn format_turn_budget_number(number: Option<&Number>) -> String {
    let Some(value) = number.and_then(Number::as_f64) else {
        return "NaN".to_string();
    };
    if !value.is_finite() {
        return js_numeric_text(value);
    }

    // `formatNumber` selects the one-decimal formatter from the original
    // value, before compact-unit promotion. This intentionally leaves negative
    // compact values without a forced trailing `.0`.
    let retain_trailing_decimal = value >= 1_000.0;
    const COMPACT_UNITS: [(f64, &str); 5] = [
        (1.0, ""),
        (1_000.0, "k"),
        (1_000_000.0, "m"),
        (1_000_000_000.0, "b"),
        (1_000_000_000_000.0, "t"),
    ];
    let mut unit = if value.abs() >= COMPACT_UNITS[4].0 {
        4
    } else if value.abs() >= COMPACT_UNITS[3].0 {
        3
    } else if value.abs() >= COMPACT_UNITS[2].0 {
        2
    } else if value.abs() >= COMPACT_UNITS[1].0 {
        1
    } else {
        0
    };
    let rounded = loop {
        let scaled = value / COMPACT_UNITS[unit].0;
        let mut rounded = (scaled * 10.0).round() / 10.0;
        if rounded == 0.0 && value.is_sign_negative() {
            rounded = -0.0;
        }
        if rounded.abs() >= 1_000.0 && unit + 1 < COMPACT_UNITS.len() {
            unit += 1;
            continue;
        }
        break rounded;
    };
    let mut formatted = format_grouped_fixed_one(rounded);
    if !retain_trailing_decimal && formatted.ends_with(".0") {
        formatted.truncate(formatted.len() - 2);
    }
    formatted.push_str(COMPACT_UNITS[unit].1);
    formatted
}

fn format_grouped_fixed_one(value: f64) -> String {
    let fixed = format!("{value:.1}");
    let (integer, fraction) = fixed
        .split_once('.')
        .expect("one-decimal formatting always includes a decimal point");
    let (sign, digits) = integer
        .strip_prefix('-')
        .map_or(("", integer), |digits| ("-", digits));
    let mut grouped = String::with_capacity(fixed.len() + digits.len() / 3);
    grouped.push_str(sign);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped.push('.');
    grouped.push_str(fraction);
    grouped
}

fn js_math_round(value: f64) -> f64 {
    if value.is_finite() {
        (value + 0.5).floor()
    } else {
        value
    }
}

fn js_numeric_text(value: f64) -> String {
    if value.is_nan() {
        "NaN".to_string()
    } else if value == f64::INFINITY {
        "Infinity".to_string()
    } else if value == f64::NEG_INFINITY {
        "-Infinity".to_string()
    } else {
        js_number(value)
    }
}

struct DirectApiRetryView<'a> {
    error: &'a CrabCodeDirectApiError,
    retry_in_ms: &'a Number,
    retry_attempt: &'a Number,
    max_retries: &'a Number,
    mounted_at: Instant,
}

fn direct_api_error_lines(
    retry: DirectApiRetryView<'_>,
    expanded: bool,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    let formatted = format_direct_api_error(retry.error);
    let formatted_units = formatted.encode_utf16().count();
    let error_text = if !expanded && formatted_units > 1_000 {
        let prefix =
            String::from_utf16_lossy(&formatted.encode_utf16().take(1_000).collect::<Vec<_>>());
        format!("{prefix}…")
    } else {
        formatted
    };
    let elapsed_ms = Instant::now()
        .saturating_duration_since(retry.mounted_at)
        .as_millis() as f64;
    let retry_seconds = retry
        .retry_in_ms
        .as_f64()
        .map(|milliseconds| ((milliseconds - elapsed_ms) / 1_000.0).round().max(0.0))
        .map(js_number)
        .unwrap_or_else(|| display_json_number(retry.retry_in_ms));
    let attempt = display_json_number(retry.retry_attempt);
    let maximum = display_json_number(retry.max_retries);
    let mut retry = match language {
        RendererLanguage::ZhCn => {
            format!("{retry_seconds} 秒后重试…（第 {attempt}/{maximum} 次）")
        }
        RendererLanguage::EnUs => {
            let unit = if retry_seconds == "1" {
                "second"
            } else {
                "seconds"
            };
            format!("Retrying in {retry_seconds} {unit}… (attempt {attempt}/{maximum})")
        }
    };
    if let Some(timeout) = std::env::var_os("API_TIMEOUT_MS") {
        retry.push_str(&format!(
            " · API_TIMEOUT_MS={}ms, try increasing it",
            timeout.to_string_lossy()
        ));
    }
    vec![
        styled_safe_line(error_text, theme.fg(theme.accent_error)),
        styled_safe_line(retry, theme.muted()),
    ]
}

fn format_direct_api_error(error: &CrabCodeDirectApiError) -> String {
    if let Some(code) = error.connection_code.as_deref() {
        match code {
            "ETIMEDOUT" => {
                return "Request timed out. Check your internet connection and proxy settings"
                    .to_string();
            }
            "UNABLE_TO_VERIFY_LEAF_SIGNATURE"
            | "UNABLE_TO_GET_ISSUER_CERT"
            | "UNABLE_TO_GET_ISSUER_CERT_LOCALLY" => {
                return "Unable to connect to API: SSL certificate verification failed. Check your proxy or corporate SSL certificates".to_string();
            }
            "CERT_HAS_EXPIRED" => {
                return "Unable to connect to API: SSL certificate has expired".to_string();
            }
            "CERT_REVOKED" => {
                return "Unable to connect to API: SSL certificate has been revoked".to_string();
            }
            "DEPTH_ZERO_SELF_SIGNED_CERT" | "SELF_SIGNED_CERT_IN_CHAIN" => {
                return "Unable to connect to API: Self-signed certificate detected. Check your proxy or corporate SSL certificates".to_string();
            }
            "ERR_TLS_CERT_ALTNAME_INVALID" | "HOSTNAME_MISMATCH" => {
                return "Unable to connect to API: SSL certificate hostname mismatch".to_string();
            }
            "CERT_NOT_YET_VALID" => {
                return "Unable to connect to API: SSL certificate is not yet valid".to_string();
            }
            "CERT_SIGNATURE_FAILURE"
            | "CERT_REJECTED"
            | "CERT_UNTRUSTED"
            | "CERT_CHAIN_TOO_LONG"
            | "PATH_LENGTH_EXCEEDED"
            | "ERR_TLS_HANDSHAKE_TIMEOUT"
            | "ERR_SSL_WRONG_VERSION_NUMBER"
            | "ERR_SSL_DECRYPTION_FAILED_OR_BAD_RECORD_MAC" => {
                return format!("Unable to connect to API: SSL error ({code})");
            }
            _ => {}
        }
    }

    if error.message.as_deref() == Some("Connection error.") {
        return error.connection_code.as_deref().map_or_else(
            || "Unable to connect to API. Check your internet connection".to_string(),
            |code| format!("Unable to connect to API ({code})"),
        );
    }
    if let Some(message) = error.message.as_deref() {
        return sanitize_api_error_html(message).unwrap_or_else(|| message.to_string());
    }
    for nested in [
        error.deeply_nested_message.as_deref(),
        error.nested_message.as_deref(),
    ] {
        if let Some(message) = nested.filter(|message| !message.is_empty())
            && let Some(sanitized) = sanitize_api_error_html(message)
        {
            return sanitized;
        }
    }
    format!(
        "API error (status {})",
        error
            .status
            .as_ref()
            .map_or_else(|| "unknown".to_string(), display_json_number)
    )
}

fn sanitize_api_error_html(message: &str) -> Option<String> {
    if !message.contains("<!DOCTYPE html") && !message.contains("<html") {
        return Some(message.to_string());
    }
    let title_start = message.find("<title>")? + "<title>".len();
    let title_end = message[title_start..].find("</title>")? + title_start;
    let title = message[title_start..title_end].trim();
    (!title.is_empty()).then(|| title.to_string())
}

fn memory_saved_lines(
    written_paths: &[String],
    team_count: Option<&Number>,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    let team_count_value = team_count.and_then(Number::as_f64).unwrap_or(0.0);
    let private_count = written_paths.len() as f64 - team_count_value;
    let mut parts = Vec::new();
    if private_count > 0.0 {
        parts.push(match language {
            RendererLanguage::ZhCn => format!("{} 条记忆", js_number(private_count)),
            RendererLanguage::EnUs => format!(
                "{} {}",
                js_number(private_count),
                if private_count == 1.0 {
                    "memory"
                } else {
                    "memories"
                }
            ),
        });
    }
    if team_count_value != 0.0 {
        parts.push(match language {
            RendererLanguage::ZhCn => {
                format!("{} 条团队记忆", js_number(team_count_value))
            }
            RendererLanguage::EnUs => format!(
                "{} team {}",
                js_number(team_count_value),
                if team_count_value == 1.0 {
                    "memory"
                } else {
                    "memories"
                }
            ),
        });
    }

    let mut lines = vec![Line::from(vec![
        Span::styled("● ", theme.muted()),
        safe_span(
            match language {
                RendererLanguage::ZhCn => format!("已保存记忆 {}", parts.join(" · ")),
                RendererLanguage::EnUs => format!("Memory saved {}", parts.join(" · ")),
            },
            theme.primary(),
        ),
    ])];
    lines.extend(written_paths.iter().map(|path| {
        let name = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path);
        styled_safe_line(format!("  ⎿  {name}"), theme.muted())
    }));
    lines
}

fn memory_saved_output(
    written_paths: &[String],
    team_count: Option<&Number>,
    ctx: &BlockContext,
    theme: &Theme,
    language: RendererLanguage,
) -> BlockOutput {
    let mut source_lines =
        memory_saved_lines(written_paths, team_count, theme, language).into_iter();
    let mut lines = source_lines
        .next()
        .map(|header| wrap_lines(vec![header], ctx.width))
        .unwrap_or_default();

    for (path, source_line) in written_paths.iter().zip(source_lines) {
        let target = crate::render::osc8::tool_path_file_target(path, ctx.cwd.as_deref());
        let mut wrapped = wrap_lines(vec![source_line], ctx.width);
        for line in &mut wrapped {
            line.link_target = target.clone();
        }
        lines.extend(wrapped);
    }

    BlockOutput {
        lines: truncate_lines(lines, ctx.max_lines, theme.muted()),
    }
}

fn generic_direct_system_lines(
    content: &str,
    level: Option<CrabCodeMessageLevel>,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let content = content.trim();
    let style = match level {
        Some(CrabCodeMessageLevel::Warning) => theme.fg(theme.warning),
        Some(CrabCodeMessageLevel::Error) => theme.fg(theme.accent_error),
        Some(CrabCodeMessageLevel::Suggestion) => theme.fg(theme.accent_plan),
        Some(CrabCodeMessageLevel::Info) => theme.muted(),
        None => theme.primary(),
    };
    let mut spans = Vec::new();
    if level != Some(CrabCodeMessageLevel::Info) {
        spans.push(Span::styled("● ", style));
    }
    spans.push(safe_span(content, style));
    vec![Line::from(spans)]
}

fn direct_progress_lines(
    progress: &CrabCodeDirectProgressBlock,
    ctx: &BlockContext,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    match progress {
        CrabCodeDirectProgressBlock::Shell { .. } => {
            shell_progress_lines(progress, ctx.mode == DisplayMode::Expanded, theme, language)
        }
        CrabCodeDirectProgressBlock::Mcp {
            progress,
            total,
            progress_message,
            percentage,
            ..
        } => mcp_progress_lines(
            progress.as_ref(),
            total.as_ref(),
            progress_message.as_deref(),
            *percentage,
            theme,
            language,
        ),
        CrabCodeDirectProgressBlock::SearchQuery { query } => {
            let text = match language {
                RendererLanguage::ZhCn => format!("正在搜索：{query}"),
                RendererLanguage::EnUs => format!("Searching: {query}"),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectProgressBlock::SearchResults {
            query,
            result_count,
        } => {
            let text = match language {
                RendererLanguage::ZhCn => {
                    format!("找到 {result_count} 条与“{query}”相关的结果")
                }
                RendererLanguage::EnUs => {
                    format!("Found {result_count} results for \"{query}\"")
                }
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectProgressBlock::WaitingForTask {
            task_description, ..
        } => {
            let mut lines = Vec::new();
            if !task_description.is_empty() {
                lines.push(styled_safe_line(
                    format!("  {task_description}"),
                    theme.muted(),
                ));
            }
            lines.push(Line::from(vec![
                Span::raw("     "),
                Span::styled(
                    language.text("正在等待任务 ", "Waiting for task "),
                    theme.muted(),
                ),
                Span::styled(
                    language.text(
                        "（按 esc 提供补充说明）",
                        "(esc to give additional instructions)",
                    ),
                    theme.dim().add_modifier(Modifier::ITALIC),
                ),
            ]));
            lines
        }
        CrabCodeDirectProgressBlock::Hook {
            hook_event,
            in_progress_count,
            resolved_count,
        } if *in_progress_count > 0
            && !matches!(hook_event.as_str(), "PreToolUse" | "PostToolUse")
            && resolved_count != in_progress_count =>
        {
            vec![Line::from(vec![
                Span::styled(language.text("正在运行 ", "Running "), theme.muted()),
                safe_span(hook_event, theme.muted().add_modifier(Modifier::BOLD)),
                Span::styled(
                    match language {
                        RendererLanguage::ZhCn => " 钩子…",
                        RendererLanguage::EnUs if *in_progress_count == 1 => " hook…",
                        RendererLanguage::EnUs => " hooks…",
                    },
                    theme.muted(),
                ),
            ])]
        }
        CrabCodeDirectProgressBlock::Hook { .. } => Vec::new(),
    }
}

fn direct_attachment_lines(
    attachment: &CrabCodeDirectAttachmentBlock,
    ctx: &BlockContext,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    match attachment {
        CrabCodeDirectAttachmentBlock::Directory { display_path } => {
            let text = match language {
                RendererLanguage::ZhCn => {
                    format!("已列出目录 {display_path}{}", std::path::MAIN_SEPARATOR)
                }
                RendererLanguage::EnUs => {
                    format!(
                        "Listed directory {display_path}{}",
                        std::path::MAIN_SEPARATOR
                    )
                }
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::File {
            display_path,
            content,
        } => {
            let detail = match content {
                CrabCodeDirectFileContent::Notebook { cell_count } => match language {
                    RendererLanguage::ZhCn => format!("{cell_count} 个单元格"),
                    RendererLanguage::EnUs => format!("{cell_count} cells"),
                },
                CrabCodeDirectFileContent::Unchanged => {
                    language.text("未更改", "unchanged").to_string()
                }
                CrabCodeDirectFileContent::Text {
                    line_count,
                    truncated,
                } => match language {
                    RendererLanguage::ZhCn => {
                        format!("{line_count}{} 行", if *truncated { "+" } else { "" })
                    }
                    RendererLanguage::EnUs => {
                        format!("{line_count}{} lines", if *truncated { "+" } else { "" })
                    }
                },
                CrabCodeDirectFileContent::Binary { original_size } => {
                    format_file_size(original_size, language)
                }
            };
            let text = match language {
                RendererLanguage::ZhCn => format!("已读取 {display_path}（{detail}）"),
                RendererLanguage::EnUs => format!("Read {display_path} ({detail})"),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::CompactFileReference { display_path } => {
            let text = match language {
                RendererLanguage::ZhCn => format!("已引用文件 {display_path}"),
                RendererLanguage::EnUs => format!("Referenced file {display_path}"),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::PdfReference {
            display_path,
            page_count,
        } => {
            let text = match language {
                RendererLanguage::ZhCn => {
                    format!("已引用 PDF {display_path}（{page_count} 页）")
                }
                RendererLanguage::EnUs => {
                    format!("Referenced PDF {display_path} ({page_count} pages)")
                }
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::SelectedLines {
            ide_name,
            line_start,
            line_end,
            display_path,
        } => {
            let count = line_end.saturating_sub(*line_start).saturating_add(1);
            let text = match language {
                RendererLanguage::ZhCn => {
                    format!("⧉ 已从 {ide_name} 的 {display_path} 选择 {count} 行")
                }
                RendererLanguage::EnUs => {
                    format!("⧉ Selected {count} lines from {display_path} in {ide_name}")
                }
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::NestedMemory { display_path } => {
            let text = match language {
                RendererLanguage::ZhCn => format!("已加载 {display_path}"),
                RendererLanguage::EnUs => format!("Loaded {display_path}"),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::RelevantMemories { memories } => {
            let count = memories.len();
            let header = match language {
                RendererLanguage::ZhCn => format!("  已回忆 {count} 条记忆"),
                RendererLanguage::EnUs => format!(
                    "  Recalled {count} {}",
                    if count == 1 { "memory" } else { "memories" }
                ),
            };
            let mut lines = vec![styled_safe_line(header, theme.muted())];
            if ctx.mode == DisplayMode::Expanded {
                lines.extend(memories.iter().map(|memory| {
                    let name = Path::new(&memory.path)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or(&memory.path);
                    styled_safe_line(format!("  {name}"), theme.muted())
                }));
            }
            lines
        }
        CrabCodeDirectAttachmentBlock::DynamicSkill {
            skill_names,
            display_path,
        } => {
            let count = skill_names.len();
            let text = match language {
                RendererLanguage::ZhCn => {
                    format!("已从 {display_path} 加载 {count} 个技能")
                }
                RendererLanguage::EnUs => format!(
                    "Loaded {count} {} from {display_path}",
                    if count == 1 { "skill" } else { "skills" }
                ),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::SkillListing {
            skill_count,
            is_initial: false,
        } => {
            let text = match language {
                RendererLanguage::ZhCn => format!("{skill_count} 个技能可用"),
                RendererLanguage::EnUs => format!(
                    "{skill_count} {} available",
                    if *skill_count == 1 { "skill" } else { "skills" }
                ),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::AgentListingDelta {
            added_types,
            is_initial: false,
        } if !added_types.is_empty() => {
            let count = added_types.len();
            let text = match language {
                RendererLanguage::ZhCn => format!("{count} 种代理类型可用"),
                RendererLanguage::EnUs => format!(
                    "{count} agent {} available",
                    if count == 1 { "type" } else { "types" }
                ),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::QueuedCommand {
            text,
            text_is_hidden,
            image_paste_ids,
        } => {
            let mut lines = if *text_is_hidden {
                Vec::new()
            } else {
                let mut inner_ctx = ctx.clone();
                inner_ctx.max_lines = None;
                text.output(&inner_ctx)
                    .lines
                    .into_iter()
                    .map(|line| line.content)
                    .collect()
            };
            lines.extend(
                image_paste_ids
                    .iter()
                    .map(|id| styled_safe_line(image_label(Some(*id), language), theme.primary())),
            );
            lines
        }
        CrabCodeDirectAttachmentBlock::PlanFileReference { plan_file_path } => {
            let path = display_path(plan_file_path, ctx.cwd.as_deref());
            let text = match language {
                RendererLanguage::ZhCn => format!("已引用计划文件（{path}）"),
                RendererLanguage::EnUs => format!("Plan file referenced ({path})"),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::InvokedSkills { skill_names } if !skill_names.is_empty() => {
            let text = match language {
                RendererLanguage::ZhCn => {
                    format!("已恢复技能（{}）", skill_names.join(", "))
                }
                RendererLanguage::EnUs => {
                    format!("Skills restored ({})", skill_names.join(", "))
                }
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::Diagnostics { files } if !files.is_empty() => {
            diagnostic_lines(files, ctx, theme, language)
        }
        CrabCodeDirectAttachmentBlock::McpResource { name, server, .. } => {
            let text = match language {
                RendererLanguage::ZhCn => format!("已从 {server} 读取 MCP 资源 {name}"),
                RendererLanguage::EnUs => format!("Read MCP resource {name} from {server}"),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::HookBlockingError {
            hook_name,
            hook_event,
            blocking_error,
        } if !matches!(hook_event.as_str(), "Stop" | "SubagentStop") => {
            let header = match language {
                RendererLanguage::ZhCn => format!("{hook_name} 钩子返回阻塞错误"),
                RendererLanguage::EnUs => {
                    format!("{hook_name} hook returned blocking error")
                }
            };
            let mut lines = vec![styled_safe_line(header, theme.fg(theme.accent_error))];
            let stderr = blocking_error.trim();
            if !stderr.is_empty() {
                lines.extend(safe_text_lines(stderr, theme.fg(theme.accent_error)));
            }
            lines
        }
        CrabCodeDirectAttachmentBlock::HookNonBlockingError {
            hook_name,
            hook_event,
        } if !matches!(hook_event.as_str(), "Stop" | "SubagentStop") => {
            let text = match language {
                RendererLanguage::ZhCn => format!("{hook_name} 钩子错误"),
                RendererLanguage::EnUs => format!("{hook_name} hook error"),
            };
            vec![styled_safe_line(text, theme.fg(theme.accent_error))]
        }
        CrabCodeDirectAttachmentBlock::HookErrorDuringExecution {
            hook_name,
            hook_event,
        } if !matches!(hook_event.as_str(), "Stop" | "SubagentStop") => {
            let text = match language {
                RendererLanguage::ZhCn => format!("{hook_name} 钩子警告"),
                RendererLanguage::EnUs => format!("{hook_name} hook warning"),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::HookStoppedContinuation {
            hook_name,
            hook_event,
            message,
        } if !matches!(hook_event.as_str(), "Stop" | "SubagentStop") => {
            let text = match language {
                RendererLanguage::ZhCn => {
                    format!("{hook_name} 钩子已停止继续执行：{message}")
                }
                RendererLanguage::EnUs => {
                    format!("{hook_name} hook stopped continuation: {message}")
                }
            };
            vec![styled_safe_line(text, theme.fg(theme.warning))]
        }
        CrabCodeDirectAttachmentBlock::HookSystemMessage { hook_name, content } => {
            let text = match language {
                RendererLanguage::ZhCn => format!("{hook_name} 提示：{content}"),
                RendererLanguage::EnUs => format!("{hook_name} says: {content}"),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::HookPermissionDecision {
            hook_event,
            decision,
        } => {
            let text = match language {
                RendererLanguage::ZhCn => format!(
                    "由 {hook_event} 钩子{}",
                    match decision {
                        CrabCodeHookPermissionDecision::Allow => "允许",
                        CrabCodeHookPermissionDecision::Deny => "拒绝",
                    }
                ),
                RendererLanguage::EnUs => format!(
                    "{} by {hook_event} hook",
                    match decision {
                        CrabCodeHookPermissionDecision::Allow => "Allowed",
                        CrabCodeHookPermissionDecision::Deny => "Denied",
                    }
                ),
            };
            vec![styled_safe_line(text, theme.muted())]
        }
        CrabCodeDirectAttachmentBlock::TaskStatus {
            status,
            description,
        } => vec![Line::from(vec![
            Span::styled("● ", theme.muted()),
            safe_span(
                match language {
                    RendererLanguage::ZhCn => format!(
                        "任务“{description}”{}",
                        match status {
                            CrabCodeTaskStatus::Pending => "待处理",
                            CrabCodeTaskStatus::Running => "仍在后台运行",
                            CrabCodeTaskStatus::Completed => "已在后台完成",
                            CrabCodeTaskStatus::Failed => "失败",
                            CrabCodeTaskStatus::Killed => "已停止",
                        }
                    ),
                    RendererLanguage::EnUs => format!(
                        "Task \"{description}\" {}",
                        match status {
                            CrabCodeTaskStatus::Pending => "pending",
                            CrabCodeTaskStatus::Running => "still running in background",
                            CrabCodeTaskStatus::Completed => "completed in background",
                            CrabCodeTaskStatus::Failed => "failed",
                            CrabCodeTaskStatus::Killed => "stopped",
                        }
                    ),
                },
                theme.muted(),
            ),
        ])],
        CrabCodeDirectAttachmentBlock::TeammateShutdownBatch { count } => {
            let text = match language {
                RendererLanguage::ZhCn => format!("{count} 位队友已正常关闭"),
                RendererLanguage::EnUs => format!(
                    "{count} {} shut down gracefully",
                    if *count == 1 { "teammate" } else { "teammates" }
                ),
            };
            vec![Line::from(vec![
                Span::styled("● ", theme.muted()),
                Span::styled(text, theme.muted()),
            ])]
        }
        CrabCodeDirectAttachmentBlock::SkillListing {
            is_initial: true, ..
        }
        | CrabCodeDirectAttachmentBlock::AgentListingDelta { .. }
        | CrabCodeDirectAttachmentBlock::InvokedSkills { .. }
        | CrabCodeDirectAttachmentBlock::Diagnostics { .. }
        | CrabCodeDirectAttachmentBlock::HookBlockingError { .. }
        | CrabCodeDirectAttachmentBlock::HookNonBlockingError { .. }
        | CrabCodeDirectAttachmentBlock::HookErrorDuringExecution { .. }
        | CrabCodeDirectAttachmentBlock::HookStoppedContinuation { .. } => Vec::new(),
    }
}

fn diagnostic_lines(
    files: &[CrabCodeDiagnosticFile],
    ctx: &BlockContext,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    if ctx.mode != DisplayMode::Expanded {
        let issue_count = files
            .iter()
            .map(|file| file.diagnostics.len())
            .sum::<usize>();
        let text = match language {
            RendererLanguage::ZhCn => {
                format!(
                    "在 {} 个文件中发现 {issue_count} 个新的诊断问题",
                    files.len()
                )
            }
            RendererLanguage::EnUs => format!(
                "Found {issue_count} new diagnostic {} in {} {}",
                if issue_count == 1 { "issue" } else { "issues" },
                files.len(),
                if files.len() == 1 { "file" } else { "files" }
            ),
        };
        return vec![styled_safe_line(text, theme.muted())];
    }

    let mut lines = Vec::new();
    for file in files {
        let (path, protocol) = diagnostic_path_and_protocol(&file.uri, ctx.cwd.as_deref());
        lines.push(styled_safe_line(
            format!("{path} {protocol}:"),
            theme.muted().add_modifier(Modifier::BOLD),
        ));
        for diagnostic in &file.diagnostics {
            let symbol = match diagnostic.severity {
                CrabCodeDiagnosticSeverity::Error => "×",
                CrabCodeDiagnosticSeverity::Warning => "⚠",
                CrabCodeDiagnosticSeverity::Info => "ℹ",
                CrabCodeDiagnosticSeverity::Hint => "★",
            };
            let line = number_plus_one(&diagnostic.start_line);
            let character = number_plus_one(&diagnostic.start_character);
            let mut text = match language {
                RendererLanguage::ZhCn => {
                    format!(
                        "  {symbol} [第 {line} 行:{character}] {}",
                        diagnostic.message
                    )
                }
                RendererLanguage::EnUs => {
                    format!(
                        "  {symbol} [Line {line}:{character}] {}",
                        diagnostic.message
                    )
                }
            };
            if let Some(code) = diagnostic.code.as_deref().filter(|code| !code.is_empty()) {
                text.push_str(&format!(" [{code}]"));
            }
            if let Some(source) = diagnostic
                .source
                .as_deref()
                .filter(|source| !source.is_empty())
            {
                text.push_str(&format!(" ({source})"));
            }
            lines.push(styled_safe_line(text, theme.muted()));
        }
    }
    lines
}

fn shell_progress_lines(
    progress: &CrabCodeDirectProgressBlock,
    verbose: bool,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    let CrabCodeDirectProgressBlock::Shell {
        output,
        full_output,
        elapsed_time_seconds,
        total_lines,
        total_bytes,
        timeout_ms,
    } = progress
    else {
        unreachable!("the direct-progress dispatcher admitted only Shell")
    };
    let stripped_full_output = strip_ansi(full_output.trim());
    let stripped_output = strip_ansi(output.trim());
    let producer_lines = stripped_output
        .split('\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let time = shell_time_display(elapsed_time_seconds, timeout_ms.as_ref(), language);

    if producer_lines.is_empty() {
        let mut text = language.text("正在运行…", "Running…").to_string();
        if let Some(time) = time {
            text.push(' ');
            text.push_str(&time);
        }
        return vec![styled_safe_line(text, theme.muted())];
    }

    let display = if verbose {
        stripped_full_output
    } else {
        producer_lines
            .iter()
            .rev()
            .take(5)
            .rev()
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
    };
    let mut lines = safe_text_lines(&display, theme.muted());
    let mut status = Vec::new();
    if !verbose {
        let total_line_value = total_lines.as_f64().unwrap_or(0.0);
        let total_bytes_truthy = total_bytes
            .as_ref()
            .and_then(Number::as_f64)
            .is_some_and(|bytes| bytes != 0.0);
        if total_bytes_truthy && total_line_value != 0.0 {
            status.push(match language {
                RendererLanguage::ZhCn => format!("~{} 行", js_number(total_line_value)),
                RendererLanguage::EnUs => format!("~{} lines", js_number(total_line_value)),
            });
        } else {
            let extra_lines = (total_line_value - 5.0).max(0.0);
            if extra_lines > 0.0 {
                status.push(match language {
                    RendererLanguage::ZhCn => format!("+{} 行", js_number(extra_lines)),
                    RendererLanguage::EnUs => format!("+{} lines", js_number(extra_lines)),
                });
            }
        }
    }
    if let Some(time) = time {
        status.push(time);
    }
    if let Some(bytes) = total_bytes.as_ref()
        && bytes.as_f64().is_some_and(|bytes| bytes != 0.0)
    {
        status.push(format_file_size(bytes, language));
    }
    if !status.is_empty() {
        lines.push(styled_safe_line(status.join(" "), theme.muted()));
    }
    lines
}

fn mcp_progress_lines(
    progress: Option<&Number>,
    total: Option<&Number>,
    progress_message: Option<&str>,
    percentage: Option<u8>,
    theme: &Theme,
    language: RendererLanguage,
) -> Vec<Line<'static>> {
    let Some(progress) = progress else {
        return vec![Line::styled(
            language.text("正在运行…", "Running…"),
            theme.muted(),
        )];
    };
    let total_value = total.and_then(Number::as_f64);
    if let (Some(progress_value), Some(total_value), Some(percentage)) =
        (progress.as_f64(), total_value, percentage)
        && total_value > 0.0
    {
        let ratio = (progress_value / total_value).clamp(0.0, 1.0);
        let mut lines = Vec::new();
        if let Some(message) = progress_message.filter(|message| !message.is_empty()) {
            lines.extend(safe_text_lines(message, theme.muted()));
        }
        lines.push(Line::from(vec![
            Span::styled(mcp_progress_bar(ratio), theme.fg(theme.accent_running)),
            Span::styled(format!(" {percentage}%"), theme.muted()),
        ]));
        return lines;
    }

    vec![styled_safe_line(
        progress_message.map_or_else(
            || match language {
                RendererLanguage::ZhCn => format!("处理中… {progress}"),
                RendererLanguage::EnUs => format!("Processing… {progress}"),
            },
            ToString::to_string,
        ),
        theme.muted(),
    )]
}

fn image_label(image_id: Option<u64>, language: RendererLanguage) -> String {
    match (language, image_id) {
        (RendererLanguage::ZhCn, None) => "[图片]".to_string(),
        (RendererLanguage::ZhCn, Some(image_id)) => format!("[图片 #{image_id}]"),
        (RendererLanguage::EnUs, None) => "[Image]".to_string(),
        (RendererLanguage::EnUs, Some(image_id)) => format!("[Image #{image_id}]"),
    }
}

fn safe_span(text: impl AsRef<str>, style: Style) -> Span<'static> {
    Span::styled(
        sanitize_bounded_terminal_text(text.as_ref()).into_owned(),
        style,
    )
}

fn styled_safe_line(text: impl AsRef<str>, style: Style) -> Line<'static> {
    Line::from(safe_span(text, style))
}

fn safe_text_lines(text: &str, style: Style) -> Vec<Line<'static>> {
    let safe = sanitize_bounded_terminal_text(text);
    safe.split('\n')
        .map(|line| Line::from(Span::styled(line.to_string(), style)))
        .collect()
}

fn wrap_lines(lines: Vec<Line<'static>>, width: u16) -> Vec<BlockLine> {
    word_wrap_lines(lines, usize::from(width))
        .into_iter()
        .map(|line| BlockLine::styled(line).with_selection_range(Some(0)))
        .collect()
}

fn truncate_lines(
    all_lines: Vec<BlockLine>,
    max_lines: Option<u16>,
    ellipsis_style: Style,
) -> Vec<BlockLine> {
    let Some(max_lines) = max_lines.map(usize::from) else {
        return all_lines;
    };
    if all_lines.len() <= max_lines || max_lines == 0 {
        return all_lines;
    }
    let take_count = if max_lines > 1 { max_lines - 1 } else { 1 };
    let mut truncated = all_lines.into_iter().take(take_count).collect::<Vec<_>>();
    if let Some(last) = truncated.last_mut() {
        let content_end = last.content.spans.len();
        last.content.spans.push(Span::styled(" …", ellipsis_style));
        last.selectable = Selectable::Spans(0..content_end);
    }
    truncated
}

fn mcp_progress_bar(ratio: f64) -> String {
    let ratio = ratio.clamp(0.0, 1.0);
    let whole = (ratio * MCP_PROGRESS_BAR_CELLS as f64).floor() as usize;
    let mut bar = MCP_PROGRESS_BLOCKS[MCP_PROGRESS_BLOCKS.len() - 1].repeat(whole);
    if whole < MCP_PROGRESS_BAR_CELLS {
        let remainder = ratio * MCP_PROGRESS_BAR_CELLS as f64 - whole as f64;
        let partial = (remainder * MCP_PROGRESS_BLOCKS.len() as f64).floor() as usize;
        bar.push_str(MCP_PROGRESS_BLOCKS[partial.min(MCP_PROGRESS_BLOCKS.len().saturating_sub(1))]);
        bar.push_str(
            &MCP_PROGRESS_BLOCKS[0].repeat(MCP_PROGRESS_BAR_CELLS.saturating_sub(whole + 1)),
        );
    }
    bar
}

fn format_file_size(size: &Number, language: RendererLanguage) -> String {
    let Some(bytes) = size.as_f64() else {
        return size.to_string();
    };
    let kb = bytes / 1024.0;
    if kb < 1.0 {
        return match language {
            RendererLanguage::ZhCn => format!("{} 字节", js_number(bytes)),
            RendererLanguage::EnUs => format!("{} bytes", js_number(bytes)),
        };
    }
    if kb < 1024.0 {
        return format!("{}KB", one_decimal(kb));
    }
    let mb = kb / 1024.0;
    if mb < 1024.0 {
        return format!("{}MB", one_decimal(mb));
    }
    format!("{}GB", one_decimal(mb / 1024.0))
}

fn one_decimal(value: f64) -> String {
    let formatted = format!("{value:.1}");
    formatted
        .strip_suffix(".0")
        .map_or(formatted.clone(), ToString::to_string)
}

fn shell_time_display(
    elapsed_time_seconds: &Number,
    timeout_ms: Option<&Number>,
    language: RendererLanguage,
) -> Option<String> {
    let elapsed = elapsed_time_seconds
        .as_f64()
        .map(|seconds| format_duration(seconds * 1000.0, false))?;
    let timeout = timeout_ms
        .and_then(Number::as_f64)
        .filter(|timeout| *timeout != 0.0)
        .map(|timeout| format_duration(timeout, true));
    Some(timeout.map_or_else(
        || format!("({elapsed})"),
        |timeout| match language {
            RendererLanguage::ZhCn => format!("({elapsed} · 超时 {timeout})"),
            RendererLanguage::EnUs => format!("({elapsed} · timeout {timeout})"),
        },
    ))
}

fn format_duration(ms: f64, hide_trailing_zeros: bool) -> String {
    if ms < 60_000.0 {
        if ms == 0.0 {
            return "0s".to_string();
        }
        if ms < 1.0 {
            return format!("{:.1}s", ms / 1000.0);
        }
        return format!("{}s", js_number((ms / 1000.0).floor()));
    }

    let mut days = (ms / 86_400_000.0).floor();
    let mut hours = ((ms % 86_400_000.0) / 3_600_000.0).floor();
    let mut minutes = ((ms % 3_600_000.0) / 60_000.0).floor();
    let mut seconds = ((ms % 60_000.0) / 1000.0).round();
    if seconds == 60.0 {
        seconds = 0.0;
        minutes += 1.0;
    }
    if minutes == 60.0 {
        minutes = 0.0;
        hours += 1.0;
    }
    if hours == 24.0 {
        hours = 0.0;
        days += 1.0;
    }

    let days = js_number(days);
    let hours = js_number(hours);
    let minutes = js_number(minutes);
    let seconds = js_number(seconds);
    if days != "0" {
        if hide_trailing_zeros && hours == "0" && minutes == "0" {
            return format!("{days}d");
        }
        if hide_trailing_zeros && minutes == "0" {
            return format!("{days}d {hours}h");
        }
        return format!("{days}d {hours}h {minutes}m");
    }
    if hours != "0" {
        if hide_trailing_zeros && minutes == "0" && seconds == "0" {
            return format!("{hours}h");
        }
        if hide_trailing_zeros && seconds == "0" {
            return format!("{hours}h {minutes}m");
        }
        return format!("{hours}h {minutes}m {seconds}s");
    }
    if minutes != "0" {
        if hide_trailing_zeros && seconds == "0" {
            return format!("{minutes}m");
        }
        return format!("{minutes}m {seconds}s");
    }
    format!("{seconds}s")
}

fn js_number(value: f64) -> String {
    if value == 0.0 {
        "0".to_string()
    } else if value.fract() == 0.0 && value.abs() < 1.0e21 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

fn display_json_number(number: &Number) -> String {
    number
        .as_f64()
        .map_or_else(|| number.to_string(), js_number)
}

fn number_plus_one(number: &Number) -> String {
    number
        .as_f64()
        .map_or_else(|| number.to_string(), |value| js_number(value + 1.0))
}

fn display_path(path: &str, cwd: Option<&Path>) -> String {
    let path_ref = Path::new(path);
    if let Some(cwd) = cwd
        && let Ok(relative) = path_ref.strip_prefix(cwd)
        && !relative.as_os_str().is_empty()
    {
        return relative.to_string_lossy().into_owned();
    }
    abbreviate_path(path).into_owned()
}

fn diagnostic_path_and_protocol(uri: &str, cwd: Option<&Path>) -> (String, String) {
    let stripped = uri
        .strip_prefix("file://")
        .or_else(|| uri.strip_prefix("_crabcode_fs_right:"))
        .unwrap_or(uri);
    let path = cwd.map_or_else(
        || stripped.to_string(),
        |cwd| {
            lexical_relative(cwd, Path::new(stripped))
                .to_string_lossy()
                .into_owned()
        },
    );
    let protocol = if uri.starts_with("file://") {
        "(file://)".to_string()
    } else if uri.starts_with("_crabcode_fs_right:") {
        "(crabcode_fs_right)".to_string()
    } else {
        format!("({})", uri.split(':').next().unwrap_or(uri))
    };
    (path, protocol)
}

fn lexical_relative(from: &Path, to: &Path) -> PathBuf {
    let from_components = from.components().collect::<Vec<_>>();
    let to_components = to.components().collect::<Vec<_>>();
    if component_root(&from_components) != component_root(&to_components) {
        return to.to_path_buf();
    }
    let common = from_components
        .iter()
        .zip(&to_components)
        .take_while(|(left, right)| left == right)
        .count();
    let mut relative = PathBuf::new();
    for component in &from_components[common..] {
        if matches!(component, Component::Normal(_)) {
            relative.push("..");
        }
    }
    for component in &to_components[common..] {
        relative.push(component.as_os_str());
    }
    relative
}

fn component_root<'a>(components: &'a [Component<'a>]) -> Option<&'a Component<'a>> {
    components
        .first()
        .filter(|component| matches!(component, Component::Prefix(_) | Component::RootDir))
}

#[derive(Default)]
struct StripAnsi {
    output: String,
}

impl Perform for StripAnsi {
    fn print(&mut self, character: char) {
        self.output.push(character);
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\n' | b'\r' | b'\t') {
            self.output.push(char::from(byte));
        }
    }

    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}

    fn put(&mut self, _byte: u8) {}

    fn unhook(&mut self) {}

    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}

    fn csi_dispatch(
        &mut self,
        _params: &Params,
        _intermediates: &[u8],
        _ignore: bool,
        _action: char,
    ) {
    }

    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

fn strip_ansi(text: &str) -> String {
    let mut sink = StripAnsi::default();
    let mut parser = Parser::new();
    parser.advance(&mut sink, text.as_bytes());
    sink.output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audited_appearance::AppearanceConfig;

    fn context_in_language(mode: DisplayMode, language: RendererLanguage) -> BlockContext {
        let appearance = AppearanceConfig {
            language,
            ..AppearanceConfig::default()
        };
        BlockContext {
            mode,
            is_running: false,
            width: 120,
            raw: false,
            max_lines: None,
            appearance,
            is_selected: false,
            cwd: Some(PathBuf::from("/work/project")),
        }
    }

    fn context(mode: DisplayMode) -> BlockContext {
        context_in_language(mode, RendererLanguage::EnUs)
    }

    fn plain_in_language(
        block: &CrabCodeProjectionBlock,
        mode: DisplayMode,
        language: RendererLanguage,
    ) -> String {
        block
            .output(&context_in_language(mode, language))
            .lines
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

    fn plain(block: &CrabCodeProjectionBlock, mode: DisplayMode) -> String {
        plain_in_language(block, mode, RendererLanguage::EnUs)
    }

    #[test]
    fn advisor_feedback_uses_fixed_collapsed_and_expanded_payloads() {
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::Advisor(
            CrabCodeAdvisorBlock::Feedback {
                text: "apply exact feedback".to_string(),
            },
        ));

        assert_eq!(
            plain(&block, DisplayMode::Collapsed),
            format!("✓ {ADVISOR_REVIEWED_MESSAGE_EN}")
        );
        assert_eq!(plain(&block, DisplayMode::Expanded), "apply exact feedback");
        assert!(block.is_foldable());
        assert_eq!(block.default_display_mode(), DisplayMode::Collapsed);
    }

    #[test]
    fn renderer_defaults_to_chinese_and_advisor_chrome_tracks_the_language() {
        assert_eq!(AppearanceConfig::default().language, RendererLanguage::ZhCn);
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::Advisor(
            CrabCodeAdvisorBlock::Feedback {
                text: "BACKEND_FEEDBACK_原样".to_string(),
            },
        ));

        assert_eq!(
            plain_in_language(&block, DisplayMode::Collapsed, RendererLanguage::ZhCn),
            format!("✓ {ADVISOR_REVIEWED_MESSAGE_ZH}")
        );
        assert_eq!(
            plain_in_language(&block, DisplayMode::Collapsed, RendererLanguage::EnUs),
            format!("✓ {ADVISOR_REVIEWED_MESSAGE_EN}")
        );
        for language in [RendererLanguage::ZhCn, RendererLanguage::EnUs] {
            assert_eq!(
                plain_in_language(&block, DisplayMode::Expanded, language),
                "BACKEND_FEEDBACK_原样"
            );
        }
    }

    #[test]
    fn mcp_progress_uses_the_fixed_twenty_cell_fractional_bar() {
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectProgress(
            CrabCodeDirectProgressBlock::Mcp {
                progress: Some(Number::from(3)),
                total: Some(Number::from(4)),
                progress_message: Some("Reading".to_string()),
                percentage: Some(75),
                server_name: "filesystem".to_string(),
                tool_name: "read_file".to_string(),
            },
        ));

        assert_eq!(
            plain(&block, DisplayMode::Expanded),
            "Reading\n███████████████      75%"
        );
    }

    #[test]
    fn shell_progress_strips_ansi_and_switches_between_tail_and_full_output() {
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectProgress(
            CrabCodeDirectProgressBlock::Shell {
                output: "\u{1b}[31mfour\nfive\nsix\u{1b}[0m".to_string(),
                full_output: "one\ntwo\nthree\nfour\nfive\nsix".to_string(),
                elapsed_time_seconds: Number::from(2),
                total_lines: Number::from(6),
                total_bytes: Some(Number::from(1536)),
                timeout_ms: None,
            },
        ));

        assert_eq!(
            plain(&block, DisplayMode::Collapsed),
            "four\nfive\nsix\n~6 lines (2s) 1.5KB"
        );
        assert_eq!(
            plain(&block, DisplayMode::Expanded),
            "one\ntwo\nthree\nfour\nfive\nsix\n(2s) 1.5KB"
        );
    }

    #[test]
    fn typed_tool_invocation_is_collapsed_but_expands_exact_classified_json() {
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::Tool(
            CrabCodeToolBlock::Invocation {
                name: "RepoProbe".to_string(),
                input: CrabCodeToolPayload::Json("{\n  \"path\": \"src/lib.rs\"\n}".to_string()),
            },
        ));

        assert_eq!(plain(&block, DisplayMode::Collapsed), "RepoProbe");
        assert_eq!(
            plain(&block, DisplayMode::Expanded),
            "RepoProbe\n{\n  \"path\": \"src/lib.rs\"\n}"
        );
        assert!(block.is_foldable());
        assert_eq!(block.default_display_mode(), DisplayMode::Collapsed);
    }

    #[test]
    fn typed_failed_terminal_result_is_expanded_and_error_accented() {
        let block =
            CrabCodeProjectionBlock::new(CrabCodeProjectionKind::Tool(CrabCodeToolBlock::Result {
                name: "Bash".to_string(),
                result: CrabCodeToolPayload::Text("exit 7".to_string()),
                is_error: Some(true),
                tone: CrabCodeToolResultTone::Terminal,
            }));

        assert_eq!(plain(&block, DisplayMode::Expanded), "Bash failed\nexit 7");
        assert!(!block.is_foldable());
        assert_eq!(block.default_display_mode(), DisplayMode::Expanded);
        assert!(block.accent(&context(DisplayMode::Expanded)).is_some());
    }

    #[test]
    fn typed_tool_progress_keeps_classified_detail_visible() {
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::Tool(
            CrabCodeToolBlock::Progress {
                name: "RepoProbe".to_string(),
                detail: "2 · task-1".to_string(),
            },
        ));

        assert_eq!(
            plain(&block, DisplayMode::Collapsed),
            "RepoProbe in progress\n2 · task-1"
        );
        assert!(!block.is_foldable());
        assert_eq!(block.default_display_mode(), DisplayMode::Expanded);
    }

    #[test]
    fn sdk_image_provenance_has_exact_text_without_media_loading() {
        let base64 = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::SdkImage(
            CrabCodeSdkImageBlock::Base64 {
                media_type: CrabCodeSdkImageMediaType::Png,
                encoded_len: 12,
            },
        ));
        let url = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::SdkImage(
            CrabCodeSdkImageBlock::Url {
                url: "https://example.invalid/a.png".to_string(),
            },
        ));
        let file = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::SdkImage(
            CrabCodeSdkImageBlock::File {
                file_id: "file_123".to_string(),
            },
        ));

        assert_eq!(
            plain(&base64, DisplayMode::Expanded),
            "image/png · encoded payload 12 bytes (complete payload received; CrabCode transcript authoritative)"
        );
        assert_eq!(
            plain(&url, DisplayMode::Expanded),
            "image URL · https://example.invalid/a.png"
        );
        assert_eq!(plain(&file, DisplayMode::Expanded), "image file · file_123");
    }

    #[test]
    fn localized_projection_preserves_dynamic_tool_hook_and_url_values_exactly() {
        let tool_name = "RepoProbe_原样-Δ";
        let payload = "{\n  \"path\": \"KEEP/RAW/路径\",\n  \"token\": \"DoNotTranslate\"\n}";
        let tool =
            CrabCodeProjectionBlock::new(CrabCodeProjectionKind::Tool(CrabCodeToolBlock::Result {
                name: tool_name.to_string(),
                result: CrabCodeToolPayload::Json(payload.to_string()),
                is_error: Some(false),
                tone: CrabCodeToolResultTone::Result,
            }));
        assert_eq!(
            plain_in_language(&tool, DisplayMode::Expanded, RendererLanguage::ZhCn),
            format!("{tool_name} 结果\n{payload}")
        );
        assert_eq!(
            plain_in_language(&tool, DisplayMode::Expanded, RendererLanguage::EnUs),
            format!("{tool_name} result\n{payload}")
        );

        let hook_name = "Hook-原样-Δ";
        let hook_message = "ERR::DoNotTranslate_保持";
        let hook = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectAttachment(
            CrabCodeDirectAttachmentBlock::HookStoppedContinuation {
                hook_name: hook_name.to_string(),
                hook_event: "PreToolUse".to_string(),
                message: hook_message.to_string(),
            },
        ));
        assert_eq!(
            plain_in_language(&hook, DisplayMode::Expanded, RendererLanguage::ZhCn),
            format!("{hook_name} 钩子已停止继续执行：{hook_message}")
        );
        assert_eq!(
            plain_in_language(&hook, DisplayMode::Expanded, RendererLanguage::EnUs),
            format!("{hook_name} hook stopped continuation: {hook_message}")
        );

        let url = "https://example.invalid/KeepRAW?q=DoNotTranslate_%E4%BF%9D%E6%8C%81";
        let image = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::SdkImage(
            CrabCodeSdkImageBlock::Url {
                url: url.to_string(),
            },
        ));
        assert_eq!(
            plain_in_language(&image, DisplayMode::Expanded, RendererLanguage::ZhCn),
            format!("图片 URL · {url}")
        );
        assert_eq!(
            plain_in_language(&image, DisplayMode::Expanded, RendererLanguage::EnUs),
            format!("image URL · {url}")
        );
    }

    #[test]
    fn redacted_thinking_never_exposes_ciphertext() {
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::RedactedThinking);

        assert_eq!(plain(&block, DisplayMode::Expanded), "✻ Thinking…");
        assert_eq!(block.searchable_text().as_deref(), Some("Thinking"));
    }

    #[test]
    fn stop_hook_and_memory_saved_use_only_fixed_producer_fields() {
        let hidden_success = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::StopHookSummary {
                hook_count: Number::from(1),
                hook_info_count: 1,
                hook_errors: vec![],
                prevented_continuation: false,
                stop_reason: None,
                hook_label: None,
            },
        ));
        assert!(hidden_success.is_hidden());
        assert!(plain(&hidden_success, DisplayMode::Expanded).is_empty());

        let hidden_empty_label = CrabCodeProjectionBlock::new(
            CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::StopHookSummary {
                hook_count: serde_json::from_str("1.0").expect("valid JSON number"),
                hook_info_count: 1,
                hook_errors: vec![],
                prevented_continuation: false,
                stop_reason: None,
                hook_label: Some(String::new()),
            }),
        );
        assert!(hidden_empty_label.is_hidden());
        assert!(plain(&hidden_empty_label, DisplayMode::Expanded).is_empty());

        let labeled = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::StopHookSummary {
                hook_count: serde_json::from_str("2.0").expect("valid JSON number"),
                hook_info_count: 2,
                hook_errors: vec!["not rendered by the fixed labeled branch".to_string()],
                prevented_continuation: true,
                stop_reason: Some("not rendered by the fixed labeled branch".to_string()),
                hook_label: Some("SessionStart".to_string()),
            },
        ));
        assert_eq!(
            plain(&labeled, DisplayMode::Expanded),
            "  ⏿  Ran 2 SessionStart {count} hooks\n     ⏿\n     ⏿"
        );

        let blocked = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::StopHookSummary {
                hook_count: Number::from(1),
                hook_info_count: 1,
                hook_errors: vec!["failure".to_string()],
                prevented_continuation: true,
                stop_reason: Some("blocked".to_string()),
                hook_label: None,
            },
        ));
        assert_eq!(
            plain(&blocked, DisplayMode::Expanded),
            "● Ran 1 stop 1 hook\n⎿\n⎿  blocked\n⎿  Stop hook Hook error: failure"
        );
        assert_eq!(
            plain(&blocked, DisplayMode::Collapsed),
            "● Ran 1 stop 1 hook (ctrl+o to expand)\n⎿  blocked\n⎿  Stop hook Hook error: failure"
        );

        let memory = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::MemorySaved {
                written_paths: vec![
                    "/work/project/private.md".to_string(),
                    "/work/project/team.md".to_string(),
                ],
                team_count: Some(Number::from(1)),
            },
        ));
        assert_eq!(
            plain(&memory, DisplayMode::Expanded),
            "● Memory saved 1 memory · 1 team memory\n  ⎿  private.md\n  ⎿  team.md"
        );
        let output = memory.output(&context(DisplayMode::Expanded));
        for (line, expected) in output
            .lines
            .iter()
            .skip(1)
            .zip(["/work/project/private.md", "/work/project/team.md"])
        {
            assert!(matches!(
                line.link_target.as_ref(),
                Some(crate::render::osc8::LinkTarget::File(path))
                    if path.as_ref() == Path::new(expected)
            ));
        }
    }

    #[test]
    fn turn_duration_preserves_fixed_duration_budget_language_and_null_branches() {
        let duration = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::TurnDuration {
                duration_ms: Number::from(66_000),
                budget_tokens: Some(Number::from(1_250)),
                budget_limit: Some(Number::from(2_000)),
                budget_nudges: Some(Number::from(2)),
                show_turn_duration: true,
                completion_verb: "Worked".to_string(),
            },
        ));
        assert_eq!(
            plain_in_language(&duration, DisplayMode::Expanded, RendererLanguage::EnUs),
            "✻ Worked Turn for 1m 6s · 1.3k / 2.0k (63%) · 2 {count} nudges"
        );
        assert_eq!(
            plain_in_language(&duration, DisplayMode::Expanded, RendererLanguage::ZhCn),
            "✻ Worked 轮次 1m 6s · 1.3k / 2.0k (63%) · 2 {count} 个提示"
        );
        assert_eq!(
            duration.searchable_text().as_deref(),
            Some("Worked Turn for 1m 6s · 1.3k / 2.0k (63%) · 2 {count} nudges")
        );

        let budget_only = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::TurnDuration {
                duration_ms: Number::from(5_000),
                budget_tokens: Some(Number::from(1_250)),
                budget_limit: Some(Number::from(2_000)),
                budget_nudges: Some(Number::from(1)),
                show_turn_duration: false,
                completion_verb: "Cooked".to_string(),
            },
        ));
        assert_eq!(
            plain_in_language(&budget_only, DisplayMode::Expanded, RendererLanguage::EnUs),
            "✻ 1.3k / 2.0k (63%) · 1 1 nudge"
        );
        assert_eq!(
            plain_in_language(&budget_only, DisplayMode::Expanded, RendererLanguage::ZhCn),
            "✻ 1.3k / 2.0k (63%) · 1 1 个提示"
        );

        let hidden = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::TurnDuration {
                duration_ms: Number::from(5_000),
                budget_tokens: None,
                budget_limit: None,
                budget_nudges: None,
                show_turn_duration: false,
                completion_verb: "Baked".to_string(),
            },
        ));
        assert!(hidden.is_hidden());
        assert!(plain(&hidden, DisplayMode::Expanded).is_empty());
        assert_eq!(hidden.searchable_text(), None);

        let over_budget = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::TurnDuration {
                duration_ms: Number::from(0),
                budget_tokens: Some(Number::from(2_500)),
                budget_limit: Some(Number::from(2_000)),
                budget_nudges: None,
                show_turn_duration: true,
                completion_verb: "Brewed".to_string(),
            },
        ));
        assert_eq!(
            plain(&over_budget, DisplayMode::Expanded),
            "✻ Brewed Turn for 0s · 2.5k used (2.0k min ✔)"
        );
    }

    #[test]
    fn turn_duration_mount_samples_only_the_fixed_completion_verbs() {
        for _ in 0..64 {
            let CrabCodeDirectSystemBlock::TurnDuration {
                completion_verb, ..
            } = CrabCodeDirectSystemBlock::turn_duration(Number::from(1), None, None, None, true)
            else {
                unreachable!("turn-duration constructor returned another variant");
            };
            assert!(TURN_COMPLETION_VERBS.contains(&completion_verb.as_str()));
        }
    }

    #[test]
    fn turn_budget_compact_number_matches_fixed_intl_boundaries() {
        for (value, expected) in [
            (999.95, "1k"),
            (1_000.0, "1.0k"),
            (1_050.0, "1.1k"),
            (1_150.0, "1.2k"),
            (999_950.0, "1.0m"),
            (-1_000.0, "-1k"),
            (9_007_199_254_740_991.0, "9,007.2t"),
        ] {
            let number = Number::from_f64(value).expect("finite JSON number");
            assert_eq!(
                format_turn_budget_number(Some(&number)),
                expected,
                "{value}"
            );
        }
    }

    #[test]
    fn api_error_preserves_fixed_formatter_retry_and_verbose_truncation() {
        let long_message = "x".repeat(1_001);
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::ApiError {
                error: CrabCodeDirectApiError {
                    message: Some(long_message.clone()),
                    status: Some(Number::from(503)),
                    nested_message: None,
                    deeply_nested_message: None,
                    connection_code: None,
                },
                retry_in_ms: Number::from(5_000),
                retry_attempt: Number::from(4),
                max_retries: Number::from(6),
                mounted_at: Instant::now(),
            },
        ));
        let collapsed = plain(&block, DisplayMode::Collapsed);
        let collapsed_flat = collapsed.replace('\n', "");
        assert!(collapsed_flat.starts_with(&format!("{}…", "x".repeat(1_000))));
        assert!(collapsed.contains("Retrying in 5 seconds… (attempt 4/6)"));
        let expanded = plain(&block, DisplayMode::Expanded);
        let expanded_flat = expanded.replace('\n', "");
        assert!(expanded_flat.starts_with(&long_message));
        assert!(!expanded_flat.starts_with(&format!("{}…", "x".repeat(1_000))));
        assert!(block.is_foldable());
        assert_eq!(block.default_display_mode(), DisplayMode::Collapsed);

        let early = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::ApiError {
                error: CrabCodeDirectApiError {
                    message: Some("hidden".to_string()),
                    status: None,
                    nested_message: None,
                    deeply_nested_message: None,
                    connection_code: None,
                },
                retry_in_ms: Number::from(1_000),
                retry_attempt: Number::from(3),
                max_retries: Number::from(6),
                mounted_at: Instant::now(),
            },
        ));
        assert!(early.is_hidden());
        assert!(plain(&early, DisplayMode::Expanded).is_empty());
    }

    #[test]
    fn api_error_foldability_uses_javascript_utf16_length_for_astral_text() {
        let astral_message = "😀".repeat(501);
        assert_eq!(astral_message.chars().count(), 501);
        assert_eq!(astral_message.encode_utf16().count(), 1_002);
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectSystem(
            CrabCodeDirectSystemBlock::ApiError {
                error: CrabCodeDirectApiError {
                    message: Some(astral_message.clone()),
                    status: None,
                    nested_message: None,
                    deeply_nested_message: None,
                    connection_code: None,
                },
                retry_in_ms: Number::from(1_000),
                retry_attempt: Number::from(4),
                max_retries: Number::from(6),
                mounted_at: Instant::now(),
            },
        ));

        assert!(
            block.is_foldable(),
            "JS .length sees 1002 UTF-16 units, so Ctrl-O must be available"
        );
        let collapsed = plain(&block, DisplayMode::Collapsed).replace('\n', "");
        assert!(collapsed.starts_with(&format!("{}…", "😀".repeat(500))));
        let expanded = plain(&block, DisplayMode::Expanded).replace('\n', "");
        assert!(expanded.starts_with(&astral_message));
    }

    #[test]
    fn exact_source_null_assistant_block_has_no_layout_search_or_selection() {
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::SourceNull(
            CrabCodeSourceNullBlock::AssistantCompaction,
        ));

        assert!(block.output(&context(DisplayMode::Expanded)).is_empty());
        assert!(!block.is_selectable());
        assert_eq!(block.searchable_text(), None);
        assert_eq!(block.copy_text(), None);
    }

    #[test]
    fn diagnostics_use_context_cwd_and_never_emit_terminal_controls() {
        let block = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectAttachment(
            CrabCodeDirectAttachmentBlock::Diagnostics {
                files: vec![CrabCodeDiagnosticFile {
                    uri: "file:///work/project/src/main.rs".to_string(),
                    diagnostics: vec![CrabCodeDiagnostic {
                        message: "bad\u{1b}[2J".to_string(),
                        severity: CrabCodeDiagnosticSeverity::Error,
                        start_line: Number::from(1),
                        start_character: Number::from(2),
                        code: Some("E1".to_string()),
                        source: Some("rustc".to_string()),
                    }],
                }],
            },
        ));

        assert_eq!(
            plain(&block, DisplayMode::Collapsed),
            "Found 1 new diagnostic issue in 1 file"
        );
        assert_eq!(
            plain(&block, DisplayMode::Expanded),
            "src/main.rs (file://):\n  × [Line 2:3] bad␛[2J [E1] (rustc)"
        );
    }

    #[test]
    fn nested_agent_prompt_and_hidden_summaries_keep_fixed_text_and_style() {
        let prompt = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectNestedProgress(
            CrabCodeDirectNestedProgressBlock::AgentPrompt {
                prompt: "inspect **all** files".to_string(),
            },
        ));
        assert_eq!(
            plain(&prompt, DisplayMode::Expanded),
            "Prompt:\n  inspect all files"
        );
        let output = prompt.output(&context(DisplayMode::Expanded));
        assert!(
            output.lines[0].content.spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );

        for (block, expected) in [
            (
                CrabCodeDirectNestedProgressBlock::AgentHiddenToolUses { count: 1 },
                "+1 more tool use (ctrl+o to expand)",
            ),
            (
                CrabCodeDirectNestedProgressBlock::AgentHiddenToolUses { count: 2 },
                "+2 more tool uses (ctrl+o to expand)",
            ),
            (
                CrabCodeDirectNestedProgressBlock::SkillHiddenMessages { count: 2 },
                "+2 more tool uses",
            ),
        ] {
            let block =
                CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectNestedProgress(block));
            assert_eq!(plain(&block, DisplayMode::Expanded), expected);
        }
    }

    #[test]
    fn nested_skill_message_clips_whole_message_and_preserves_null_box_height() {
        let message = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectNestedProgress(
            CrabCodeDirectNestedProgressBlock::SkillMessage {
                source_text: "first\n\nsecond".to_string(),
                verbose: false,
                inners: vec![RenderBlock::agent_message("first\n\nsecond")],
            },
        ));
        let output = message.output(&context(DisplayMode::Expanded));
        assert_eq!(output.lines.len(), 1);
        assert_eq!(plain(&message, DisplayMode::Expanded), "first");

        let source_null =
            CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectNestedProgress(
                CrabCodeDirectNestedProgressBlock::SkillMessage {
                    source_text: String::new(),
                    verbose: false,
                    inners: vec![RenderBlock::CrabCodeProjection(
                        CrabCodeProjectionBlock::new(CrabCodeProjectionKind::SourceNull(
                            CrabCodeSourceNullBlock::UncorrelatedUserToolResult,
                        )),
                    )],
                },
            ));
        let output = source_null.output(&context(DisplayMode::Expanded));
        assert_eq!(output.lines.len(), 1);
        assert!(plain(&source_null, DisplayMode::Expanded).is_empty());

        let visible_after_null =
            CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectNestedProgress(
                CrabCodeDirectNestedProgressBlock::SkillMessage {
                    source_text: "visible".to_string(),
                    verbose: false,
                    inners: vec![
                        RenderBlock::CrabCodeProjection(CrabCodeProjectionBlock::new(
                            CrabCodeProjectionKind::SourceNull(
                                CrabCodeSourceNullBlock::UncorrelatedUserToolResult,
                            ),
                        )),
                        RenderBlock::agent_message("visible\n\nclipped"),
                    ],
                },
            ));
        assert_eq!(plain(&visible_after_null, DisplayMode::Expanded), "visible");
    }

    #[test]
    fn nested_agent_message_preserves_all_rows_without_skill_clipping() {
        let message = CrabCodeProjectionBlock::new(CrabCodeProjectionKind::DirectNestedProgress(
            CrabCodeDirectNestedProgressBlock::AgentMessage {
                source_text: "first\n\nsecond".to_string(),
                verbose: false,
                inner: Box::new(RenderBlock::agent_message("first\n\nsecond")),
            },
        ));
        assert_eq!(plain(&message, DisplayMode::Expanded), "first\n\nsecond");
        assert!(message.is_groupable());
    }

    #[test]
    fn source_null_rows_have_zero_layout_lines_and_are_not_selectable() {
        for kind in [
            CrabCodeProjectionKind::DirectSystem(CrabCodeDirectSystemBlock::Thinking),
            CrabCodeProjectionKind::DirectProgress(CrabCodeDirectProgressBlock::Hook {
                hook_event: "PreToolUse".to_string(),
                in_progress_count: 2,
                resolved_count: 0,
            }),
            CrabCodeProjectionKind::DirectAttachment(CrabCodeDirectAttachmentBlock::SkillListing {
                skill_count: 4,
                is_initial: true,
            }),
        ] {
            let block = CrabCodeProjectionBlock::new(kind);
            assert!(block.output(&context(DisplayMode::Expanded)).is_empty());
            assert!(!block.is_selectable());
            assert!(!block.has_vpad(&context(DisplayMode::Expanded)));
            assert_eq!(block.searchable_text(), None);
            assert_eq!(block.copy_text(), None);
        }
    }
}

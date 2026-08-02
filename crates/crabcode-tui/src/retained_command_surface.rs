//! Native presentation state for fixed-history retained commands.
//!
//! `color` and `rename` keep their existing TypeScript persistence owners.
//! This module owns only the Rust renderer state needed to show the committed
//! standalone identity immediately and after restart.  `vim` and `brief` are
//! deliberately not represented as completed renderer commands here: their
//! fixed lifecycle needs state that the current native renderer does not own.

use crabcode_pager_render::audited_theme::CrabCodeThemeKind;
use ratatui::style::Color;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::tui_app::UiLanguage;

const COLOR_NAMES: &str = "red, blue, green, yellow, purple, orange, pink, cyan, default";

/// Exact unresolved responsibilities from the fixed product action and UI.
/// These strings are audit evidence, not user-facing runtime copy.
#[cfg(test)]
pub(crate) const VIM_RENDERER_BLOCKER: &str = "missing full-session normal/insert editor state, persisted startup hydration, prompt and picker key routing, and Escape semantics";
#[cfg(test)]
pub(crate) const BRIEF_RENDERER_BLOCKER: &str = "missing authoritative brief/Kairos display filter inputs and the fixed streaming, queued-command, spinner, idle-status, notification, transcript, and teammate-view branches";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetainedCommandPurpose {
    IdentitySnapshot,
    ColorApply,
    RenameApply,
}

impl RetainedCommandPurpose {
    pub(crate) const fn expected_result_kind(self) -> &'static str {
        match self {
            Self::IdentitySnapshot => "retained.identity.snapshot",
            Self::ColorApply => "retained.color.updated",
            Self::RenameApply => "retained.rename.updated",
        }
    }

    const fn action_kind(self) -> RetainedActionKind {
        match self {
            Self::IdentitySnapshot => RetainedActionKind::IdentitySnapshot,
            Self::ColorApply => RetainedActionKind::ColorApply,
            Self::RenameApply => RetainedActionKind::RenameApply,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RetainedCommandEffect {
    pub(crate) action: Value,
    pub(crate) purpose: RetainedCommandPurpose,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AgentColor {
    Red,
    Blue,
    Green,
    Yellow,
    Purple,
    Orange,
    Pink,
    Cyan,
}

impl AgentColor {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Red => "red",
            Self::Blue => "blue",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Purple => "purple",
            Self::Orange => "orange",
            Self::Pink => "pink",
            Self::Cyan => "cyan",
        }
    }

    fn parse_argument(value: &str) -> Option<Self> {
        match value {
            "red" => Some(Self::Red),
            "blue" => Some(Self::Blue),
            "green" => Some(Self::Green),
            "yellow" => Some(Self::Yellow),
            "purple" => Some(Self::Purple),
            "orange" => Some(Self::Orange),
            "pink" => Some(Self::Pink),
            "cyan" => Some(Self::Cyan),
            _ => None,
        }
    }

    /// Exact historical `AGENT_COLOR_TO_THEME_COLOR` values for the six
    /// concrete product themes. `Auto` is resolved before paint; returning no
    /// color there prevents an unresolved setting from becoming an invented
    /// palette authority.
    pub(crate) const fn background(self, theme: CrabCodeThemeKind) -> Option<Color> {
        let color = match theme {
            CrabCodeThemeKind::Light => match self {
                Self::Red => Color::Rgb(220, 38, 38),
                Self::Blue => Color::Rgb(37, 99, 235),
                Self::Green => Color::Rgb(22, 163, 74),
                Self::Yellow => Color::Rgb(202, 138, 4),
                Self::Purple => Color::Rgb(147, 51, 234),
                Self::Orange => Color::Rgb(234, 88, 12),
                Self::Pink => Color::Rgb(219, 39, 119),
                Self::Cyan => Color::Rgb(8, 145, 178),
            },
            CrabCodeThemeKind::Dark => match self {
                Self::Red => Color::Rgb(220, 38, 38),
                Self::Blue => Color::Rgb(37, 99, 235),
                Self::Green => Color::Rgb(22, 163, 74),
                Self::Yellow => Color::Rgb(202, 138, 4),
                Self::Purple => Color::Rgb(147, 51, 234),
                Self::Orange => Color::Rgb(234, 88, 12),
                Self::Pink => Color::Rgb(219, 39, 119),
                Self::Cyan => Color::Rgb(8, 145, 178),
            },
            CrabCodeThemeKind::LightDaltonized => match self {
                Self::Red => Color::Rgb(204, 0, 0),
                Self::Blue => Color::Rgb(0, 102, 204),
                Self::Green => Color::Rgb(0, 204, 0),
                Self::Yellow => Color::Rgb(255, 204, 0),
                Self::Purple => Color::Rgb(128, 0, 128),
                Self::Orange => Color::Rgb(255, 128, 0),
                Self::Pink => Color::Rgb(255, 102, 178),
                Self::Cyan => Color::Rgb(0, 178, 178),
            },
            CrabCodeThemeKind::DarkDaltonized => match self {
                Self::Red => Color::Rgb(255, 102, 102),
                Self::Blue => Color::Rgb(102, 178, 255),
                Self::Green => Color::Rgb(102, 255, 102),
                Self::Yellow => Color::Rgb(255, 255, 102),
                Self::Purple => Color::Rgb(178, 102, 255),
                Self::Orange => Color::Rgb(255, 178, 102),
                Self::Pink => Color::Rgb(255, 153, 204),
                Self::Cyan => Color::Rgb(102, 204, 204),
            },
            CrabCodeThemeKind::LightAnsi => match self {
                Self::Red => Color::Red,
                Self::Blue => Color::Blue,
                Self::Green => Color::Green,
                Self::Yellow => Color::Yellow,
                Self::Purple => Color::Magenta,
                Self::Orange => Color::LightRed,
                Self::Pink => Color::LightMagenta,
                Self::Cyan => Color::Cyan,
            },
            CrabCodeThemeKind::DarkAnsi => match self {
                Self::Red | Self::Orange => Color::LightRed,
                Self::Blue => Color::LightBlue,
                Self::Green => Color::LightGreen,
                Self::Yellow => Color::LightYellow,
                Self::Purple | Self::Pink => Color::LightMagenta,
                Self::Cyan => Color::LightCyan,
            },
            CrabCodeThemeKind::Auto => return None,
        };
        Some(color)
    }
}

pub(crate) const fn historical_inverse_text(theme: CrabCodeThemeKind) -> Option<Color> {
    match theme {
        CrabCodeThemeKind::Light
        | CrabCodeThemeKind::LightDaltonized
        | CrabCodeThemeKind::LightAnsi => Some(Color::White),
        CrabCodeThemeKind::Dark
        | CrabCodeThemeKind::DarkDaltonized
        | CrabCodeThemeKind::DarkAnsi => Some(Color::Black),
        CrabCodeThemeKind::Auto => None,
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RetainedCommandState {
    name: Option<String>,
    color: Option<AgentColor>,
    name_revision: u64,
    color_revision: u64,
    identity_hydrated: bool,
    pending_snapshot: Option<IdentityRevision>,
    pending_mutation: Option<RetainedCommandPurpose>,
    notice: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IdentityRevision {
    name: u64,
    color: u64,
}

impl RetainedCommandState {
    pub(crate) fn identity_snapshot(&mut self) -> Option<RetainedCommandEffect> {
        self.start(
            RetainedCommandPurpose::IdentitySnapshot,
            json!({"kind":"retained.identity.snapshot"}),
        )
    }

    pub(crate) fn color(
        &mut self,
        argument: &str,
        language: UiLanguage,
    ) -> Option<RetainedCommandEffect> {
        let normalized = argument.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            self.notice = Some(match language {
                UiLanguage::ZhCn => format!("请提供颜色。可用颜色：{COLOR_NAMES}"),
                UiLanguage::EnUs => {
                    format!("Please provide a color. Available colors: {COLOR_NAMES}")
                }
            });
            return None;
        }
        let reset = matches!(
            normalized.as_str(),
            "default" | "reset" | "none" | "gray" | "grey"
        );
        if !reset && AgentColor::parse_argument(&normalized).is_none() {
            self.notice = Some(match language {
                UiLanguage::ZhCn => format!("无效颜色“{normalized}”。可用颜色：{COLOR_NAMES}"),
                UiLanguage::EnUs => {
                    format!("Invalid color \"{normalized}\". Available colors: {COLOR_NAMES}")
                }
            });
            return None;
        }
        self.start(
            RetainedCommandPurpose::ColorApply,
            json!({"kind":"retained.color.apply","argument":argument}),
        )
    }

    pub(crate) fn rename(&mut self, argument: &str) -> Option<RetainedCommandEffect> {
        self.start(
            RetainedCommandPurpose::RenameApply,
            json!({"kind":"retained.rename.apply","argument":argument}),
        )
    }

    fn start(
        &mut self,
        purpose: RetainedCommandPurpose,
        action: Value,
    ) -> Option<RetainedCommandEffect> {
        match purpose {
            RetainedCommandPurpose::IdentitySnapshot => {
                if self.pending_snapshot.is_some() {
                    return None;
                }
                self.pending_snapshot = Some(IdentityRevision {
                    name: self.name_revision,
                    color: self.color_revision,
                });
            }
            RetainedCommandPurpose::ColorApply | RetainedCommandPurpose::RenameApply => {
                if self.pending_mutation.is_some() {
                    return None;
                }
                self.pending_mutation = Some(purpose);
            }
        }
        self.notice = None;
        Some(RetainedCommandEffect { action, purpose })
    }

    pub(crate) fn apply_result(
        &mut self,
        purpose: RetainedCommandPurpose,
        result_kind: &str,
        envelope: &Value,
        language: UiLanguage,
    ) -> Result<(), String> {
        if !self.is_pending(purpose) {
            return Err("retained-result-without-matching-pending-operation".to_string());
        }
        if result_kind != purpose.expected_result_kind() {
            return Err(format!(
                "retained-result-kind-mismatch:{}:{result_kind}",
                purpose.expected_result_kind()
            ));
        }
        let result = envelope
            .get("result")
            .ok_or("retained-result-object-missing")?;
        match purpose {
            RetainedCommandPurpose::IdentitySnapshot => {
                let parsed: IdentityResult = strict_result(result)?;
                let revision = self
                    .pending_snapshot
                    .take()
                    .expect("matching snapshot pending state");
                self.identity_hydrated = true;
                if self.name_revision == revision.name {
                    self.name = parsed.name.filter(|name| !name.is_empty());
                }
                if self.color_revision == revision.color {
                    self.color = parsed.color;
                }
            }
            RetainedCommandPurpose::ColorApply => {
                let parsed: ColorResult = strict_result(result)?;
                self.color = parsed.color;
                self.color_revision = self.color_revision.saturating_add(1);
                self.pending_mutation = None;
                self.notice = Some(match (language, parsed.color) {
                    (UiLanguage::ZhCn, Some(color)) => {
                        format!("会话颜色已设置为：{}", color.as_str())
                    }
                    (UiLanguage::EnUs, Some(color)) => {
                        format!("Session color set to: {}", color.as_str())
                    }
                    (UiLanguage::ZhCn, None) => "会话颜色已恢复默认".to_string(),
                    (UiLanguage::EnUs, None) => "Session color reset to default".to_string(),
                });
            }
            RetainedCommandPurpose::RenameApply => {
                let parsed: RenameResult = strict_result(result)?;
                self.name = Some(parsed.name.clone());
                self.name_revision = self.name_revision.saturating_add(1);
                self.pending_mutation = None;
                self.notice = Some(match language {
                    UiLanguage::ZhCn => format!("会话已重命名为：{}", parsed.name),
                    UiLanguage::EnUs => format!("Session renamed to: {}", parsed.name),
                });
            }
        }
        Ok(())
    }

    pub(crate) fn apply_error(
        &mut self,
        purpose: RetainedCommandPurpose,
        language: UiLanguage,
        code: &str,
    ) {
        self.clear_pending(purpose);
        self.notice = Some(match language {
            UiLanguage::ZhCn => format!("命令未完成，可重试：{code}"),
            UiLanguage::EnUs => format!("Command did not complete; retry is available: {code}"),
        });
    }

    pub(crate) fn apply_command_error(
        &mut self,
        purpose: RetainedCommandPurpose,
        envelope: &Value,
        language: UiLanguage,
    ) -> Result<(), String> {
        if !self.is_pending(purpose) {
            return Err("retained-error-without-matching-pending-operation".to_string());
        }
        let result = envelope
            .get("result")
            .ok_or("retained-error-object-missing")?;
        let parsed: CommandErrorResult = strict_result(result)?;
        if parsed.action_kind != purpose.action_kind() {
            return Err("retained-error-action-kind-mismatch".to_string());
        }
        self.clear_pending(purpose);
        self.notice = Some(command_error_copy(language, parsed.code));
        Ok(())
    }

    fn is_pending(&self, purpose: RetainedCommandPurpose) -> bool {
        match purpose {
            RetainedCommandPurpose::IdentitySnapshot => self.pending_snapshot.is_some(),
            RetainedCommandPurpose::ColorApply | RetainedCommandPurpose::RenameApply => {
                self.pending_mutation == Some(purpose)
            }
        }
    }

    fn clear_pending(&mut self, purpose: RetainedCommandPurpose) {
        match purpose {
            RetainedCommandPurpose::IdentitySnapshot => self.pending_snapshot = None,
            RetainedCommandPurpose::ColorApply | RetainedCommandPurpose::RenameApply
                if self.pending_mutation == Some(purpose) =>
            {
                self.pending_mutation = None;
            }
            RetainedCommandPurpose::ColorApply | RetainedCommandPurpose::RenameApply => {}
        }
    }

    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub(crate) const fn color_value(&self) -> Option<AgentColor> {
        self.color
    }

    pub(crate) fn banner_visible(&self) -> bool {
        self.name.is_some() || self.color.is_some()
    }

    pub(crate) fn identity_snapshot_retry_needed(&self) -> bool {
        !self.identity_hydrated && self.pending_snapshot.is_none()
    }

    pub(crate) fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    #[cfg(test)]
    fn pending(&self, purpose: RetainedCommandPurpose) -> bool {
        self.is_pending(purpose)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IdentityResult {
    #[allow(dead_code)]
    kind: IdentityKind,
    name: Option<String>,
    color: Option<AgentColor>,
}

#[derive(Deserialize)]
enum IdentityKind {
    #[serde(rename = "retained.identity.snapshot")]
    Snapshot,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ColorResult {
    #[allow(dead_code)]
    kind: ColorKind,
    color: Option<AgentColor>,
}

#[derive(Deserialize)]
enum ColorKind {
    #[serde(rename = "retained.color.updated")]
    Updated,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameResult {
    #[allow(dead_code)]
    kind: RenameKind,
    name: String,
}

#[derive(Deserialize)]
enum RenameKind {
    #[serde(rename = "retained.rename.updated")]
    Updated,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
enum RetainedActionKind {
    #[serde(rename = "retained.identity.snapshot")]
    IdentitySnapshot,
    #[serde(rename = "retained.color.apply")]
    ColorApply,
    #[serde(rename = "retained.rename.apply")]
    RenameApply,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum CommandErrorCode {
    ArgumentRequired,
    InvalidArgument,
    TeammateRestricted,
    NameGenerationUnavailable,
    CommandUnavailable,
    NotEntitled,
    SurfaceUnavailable,
    AuthorityFailure,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandErrorResult {
    #[allow(dead_code)]
    kind: CommandErrorKind,
    action_kind: RetainedActionKind,
    code: CommandErrorCode,
}

#[derive(Deserialize)]
enum CommandErrorKind {
    #[serde(rename = "retained_command_error")]
    Error,
}

fn command_error_copy(language: UiLanguage, code: CommandErrorCode) -> String {
    let copy = match (language, code) {
        (UiLanguage::ZhCn, CommandErrorCode::ArgumentRequired) => "必须提供命令参数",
        (UiLanguage::EnUs, CommandErrorCode::ArgumentRequired) => "A command argument is required",
        (UiLanguage::ZhCn, CommandErrorCode::InvalidArgument) => "命令参数无效",
        (UiLanguage::EnUs, CommandErrorCode::InvalidArgument) => "The command argument is invalid",
        (UiLanguage::ZhCn, CommandErrorCode::TeammateRestricted) => {
            "队友会话的名称和颜色由团队负责人分配"
        }
        (UiLanguage::EnUs, CommandErrorCode::TeammateRestricted) => {
            "Teammate names and colors are assigned by the team leader"
        }
        (UiLanguage::ZhCn, CommandErrorCode::NameGenerationUnavailable) => {
            "当前还没有可用于生成会话名称的对话内容；请运行 /rename <名称>"
        }
        (UiLanguage::EnUs, CommandErrorCode::NameGenerationUnavailable) => {
            "No conversation context is available for name generation; run /rename <name>"
        }
        (UiLanguage::ZhCn, CommandErrorCode::CommandUnavailable) => "当前构建未启用该命令",
        (UiLanguage::EnUs, CommandErrorCode::CommandUnavailable) => {
            "This command is not enabled in the current build"
        }
        (UiLanguage::ZhCn, CommandErrorCode::NotEntitled) => "当前账户未获得该功能权限",
        (UiLanguage::EnUs, CommandErrorCode::NotEntitled) => {
            "This feature is not enabled for the current account"
        }
        (UiLanguage::ZhCn, CommandErrorCode::SurfaceUnavailable) => {
            "命令所需的直连 TUI 状态不可用，可重试"
        }
        (UiLanguage::EnUs, CommandErrorCode::SurfaceUnavailable) => {
            "The direct TUI state required by this command is unavailable; retry is available"
        }
        (UiLanguage::ZhCn, CommandErrorCode::AuthorityFailure) => "命令权威操作未完成，可重试",
        (UiLanguage::EnUs, CommandErrorCode::AuthorityFailure) => {
            "The authoritative command operation did not complete; retry is available"
        }
    };
    copy.to_string()
}

fn strict_result<T: for<'de> Deserialize<'de>>(value: &Value) -> Result<T, String> {
    serde_json::from_value(value.clone()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(result: Value) -> Value {
        json!({
            "type":"crabcode_tui_runtime_result",
            "protocol_version":1,
            "request_id":"retained-test",
            "result":result,
        })
    }

    #[test]
    fn restart_snapshot_restores_name_and_color_without_creating_backend_state() {
        let mut state = RetainedCommandState::default();
        let effect = state.identity_snapshot().expect("initial snapshot request");
        assert_eq!(effect.action, json!({"kind":"retained.identity.snapshot"}));
        state
            .apply_result(
                effect.purpose,
                "retained.identity.snapshot",
                &envelope(json!({
                    "kind":"retained.identity.snapshot",
                    "name":"会话甲",
                    "color":"purple"
                })),
                UiLanguage::ZhCn,
            )
            .expect("valid snapshot");
        assert_eq!(state.name(), Some("会话甲"));
        assert_eq!(state.color_value(), Some(AgentColor::Purple));
        assert!(state.banner_visible());
    }

    #[test]
    fn startup_snapshot_never_blocks_a_command_or_overwrites_a_newer_commit() {
        let mut state = RetainedCommandState::default();
        let snapshot = state.identity_snapshot().expect("startup snapshot");
        let color = state
            .color("blue", UiLanguage::ZhCn)
            .expect("a slow startup snapshot cannot swallow user input");
        state
            .apply_result(
                color.purpose,
                "retained.color.updated",
                &envelope(json!({"kind":"retained.color.updated","color":"blue"})),
                UiLanguage::ZhCn,
            )
            .expect("newer color commit");
        state
            .apply_result(
                snapshot.purpose,
                "retained.identity.snapshot",
                &envelope(json!({
                    "kind":"retained.identity.snapshot",
                    "name":"持久化名称",
                    "color":"red"
                })),
                UiLanguage::ZhCn,
            )
            .expect("late startup snapshot");
        assert_eq!(state.name(), Some("持久化名称"));
        assert_eq!(state.color_value(), Some(AgentColor::Blue));
    }

    #[test]
    fn startup_snapshot_failure_is_independently_retryable() {
        let mut state = RetainedCommandState::default();
        let snapshot = state.identity_snapshot().expect("startup snapshot");
        state.apply_error(snapshot.purpose, UiLanguage::ZhCn, "queue_full");
        assert!(state.identity_snapshot().is_some());
        assert!(state.rename("用户名称").is_some());
    }

    #[test]
    fn color_and_rename_apply_only_committed_authority_results() {
        let mut state = RetainedCommandState::default();
        let color = state.color(" BLUE ", UiLanguage::ZhCn).expect("action");
        assert_eq!(
            color.action,
            json!({"kind":"retained.color.apply","argument":" BLUE "})
        );
        state
            .apply_result(
                color.purpose,
                "retained.color.updated",
                &envelope(json!({"kind":"retained.color.updated","color":"blue"})),
                UiLanguage::ZhCn,
            )
            .expect("valid result");
        assert_eq!(state.color_value(), Some(AgentColor::Blue));

        let rename = state.rename("  committed name  ").expect("action");
        state
            .apply_result(
                rename.purpose,
                "retained.rename.updated",
                &envelope(json!({
                    "kind":"retained.rename.updated",
                    "name":"committed name"
                })),
                UiLanguage::EnUs,
            )
            .expect("valid result");
        assert_eq!(state.name(), Some("committed name"));
        assert_eq!(state.notice(), Some("Session renamed to: committed name"));
    }

    #[test]
    fn invalid_color_is_local_and_does_not_enter_pending_state() {
        let mut state = RetainedCommandState::default();
        assert!(state.color("", UiLanguage::ZhCn).is_none());
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("可用颜色"))
        );
        assert!(!state.pending(RetainedCommandPurpose::ColorApply));
        assert!(state.color("ultraviolet", UiLanguage::EnUs).is_none());
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("Invalid color"))
        );
        assert!(!state.pending(RetainedCommandPurpose::ColorApply));
    }

    #[test]
    fn request_local_failure_clears_pending_and_same_command_is_retryable() {
        let mut state = RetainedCommandState::default();
        assert!(state.rename("first").is_some());
        assert!(state.pending(RetainedCommandPurpose::RenameApply));
        state.apply_error(
            RetainedCommandPurpose::RenameApply,
            UiLanguage::ZhCn,
            "queue_full",
        );
        assert!(!state.pending(RetainedCommandPurpose::RenameApply));
        assert!(state.rename("second").is_some());
    }

    #[test]
    fn typed_owner_errors_clear_pending_without_raw_backend_details() {
        let mut state = RetainedCommandState::default();
        let effect = state.rename("").expect("rename request");
        state
            .apply_command_error(
                effect.purpose,
                &envelope(json!({
                    "kind":"retained_command_error",
                    "action_kind":"retained.rename.apply",
                    "code":"name_generation_unavailable"
                })),
                UiLanguage::ZhCn,
            )
            .expect("typed command error");
        assert!(!state.pending(RetainedCommandPurpose::RenameApply));
        assert!(
            state
                .notice()
                .is_some_and(|notice| notice.contains("/rename <名称>"))
        );
        assert!(state.rename("retry").is_some());
    }

    #[test]
    fn mismatched_and_malformed_results_do_not_mutate_visible_identity() {
        let mut state = RetainedCommandState::default();
        let effect = state.identity_snapshot().expect("action");
        let malformed = envelope(json!({
            "kind":"retained.identity.snapshot",
            "name":"unsafe",
            "color":"blue",
            "unknown":true
        }));
        assert!(
            state
                .apply_result(
                    effect.purpose,
                    "retained.identity.snapshot",
                    &malformed,
                    UiLanguage::ZhCn,
                )
                .is_err()
        );
        assert_eq!(state.name(), None);
        assert_eq!(state.color_value(), None);
    }

    #[test]
    fn historical_agent_palette_is_exact_for_all_concrete_themes() {
        assert_eq!(
            AgentColor::Red.background(CrabCodeThemeKind::Light),
            Some(Color::Rgb(220, 38, 38))
        );
        assert_eq!(
            AgentColor::Pink.background(CrabCodeThemeKind::LightDaltonized),
            Some(Color::Rgb(255, 102, 178))
        );
        assert_eq!(
            AgentColor::Cyan.background(CrabCodeThemeKind::DarkDaltonized),
            Some(Color::Rgb(102, 204, 204))
        );
        assert_eq!(
            AgentColor::Orange.background(CrabCodeThemeKind::LightAnsi),
            Some(Color::LightRed)
        );
        assert_eq!(
            AgentColor::Purple.background(CrabCodeThemeKind::DarkAnsi),
            Some(Color::LightMagenta)
        );
        assert_eq!(AgentColor::Blue.background(CrabCodeThemeKind::Auto), None);
        assert_eq!(
            historical_inverse_text(CrabCodeThemeKind::Light),
            Some(Color::White)
        );
        assert_eq!(
            historical_inverse_text(CrabCodeThemeKind::Dark),
            Some(Color::Black)
        );
    }

    #[test]
    fn vim_and_brief_are_not_claimed_complete_without_their_renderer_inputs() {
        assert!(VIM_RENDERER_BLOCKER.contains("normal/insert"));
        assert!(VIM_RENDERER_BLOCKER.contains("Escape"));
        assert!(BRIEF_RENDERER_BLOCKER.contains("spinner"));
        assert!(BRIEF_RENDERER_BLOCKER.contains("notification"));
    }
}

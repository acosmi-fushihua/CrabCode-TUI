//! Renderer-owned terminal notifications.
//!
//! The protocol bytes and channel routing are the fixed historical CrabCode
//! behavior, while delivery uses the fixed Rust TUI's sole ordered writer.
//! This module never creates backend messages and never executes notification
//! hooks; the direct runtime retains those process-local hook side effects.

use std::io;

use crate::terminal;
use crate::terminal_capabilities::{TerminalName, terminal_context};
use crate::tui_app::{RendererNotificationChannel, TuiApp};

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
const DEFAULT_TITLE: &str = "CrabCode";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TerminalNotificationRequest {
    pub(crate) message: String,
    pub(crate) title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationChannel {
    Auto,
    Iterm2,
    Iterm2WithBell,
    TerminalBell,
    Kitty,
    Ghostty,
    Disabled,
}

impl NotificationChannel {
    fn from_renderer(channel: RendererNotificationChannel) -> Self {
        match channel.as_str() {
            "auto" => Self::Auto,
            "iterm2" => Self::Iterm2,
            "iterm2_with_bell" => Self::Iterm2WithBell,
            "terminal_bell" => Self::TerminalBell,
            "kitty" => Self::Kitty,
            "ghostty" => Self::Ghostty,
            "notifications_disabled" => Self::Disabled,
            _ => unreachable!("RendererNotificationChannel is a closed enum"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NotificationConfig {
    channel: NotificationChannel,
    /// Retained from the authoritative renderer context. No idle notification
    /// is scheduled until existing runtime events expose every historical idle
    /// predicate; retaining configuration does not invent those facts.
    _message_idle_threshold_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MultiplexerRoute {
    tmux: bool,
    screen: bool,
}

impl MultiplexerRoute {
    fn from_process() -> Self {
        Self {
            // Historical `wrapForMultiplexer` checks these exact variables,
            // not a broader terminal/multiplexer capability classification.
            tmux: std::env::var_os("TMUX").is_some(),
            screen: std::env::var_os("STY").is_some(),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct TerminalNotificationService {
    config: Option<NotificationConfig>,
}

impl TerminalNotificationService {
    pub(crate) fn synchronize_renderer_config(&mut self, app: &TuiApp) {
        let (Some(channel), Some(message_idle_threshold_ms)) = (
            app.renderer_preferred_notification_channel(),
            app.renderer_message_idle_notification_threshold_ms(),
        ) else {
            return;
        };
        self.config = Some(NotificationConfig {
            channel: NotificationChannel::from_renderer(channel),
            _message_idle_threshold_ms: message_idle_threshold_ms,
        });
    }

    pub(crate) fn send(
        &self,
        terminal: &mut terminal::TerminalSession,
        request: &TerminalNotificationRequest,
    ) -> io::Result<bool> {
        let Some(config) = self.config else {
            return Ok(false);
        };
        let controls = notification_controls(
            config.channel,
            terminal_context().brand,
            MultiplexerRoute::from_process(),
            request,
            kitty_notification_id(),
        );
        let emitted = !controls.is_empty();
        for control in controls {
            terminal.enqueue_control_bytes(&control)?;
        }
        Ok(emitted)
    }
}

fn kitty_notification_id() -> u16 {
    (uuid::Uuid::new_v4().as_u128() % 10_000) as u16
}

fn notification_controls(
    channel: NotificationChannel,
    terminal: TerminalName,
    multiplexer: MultiplexerRoute,
    request: &TerminalNotificationRequest,
    kitty_id: u16,
) -> Vec<Vec<u8>> {
    match channel {
        NotificationChannel::Auto => match terminal {
            TerminalName::Iterm2 => vec![iterm2_notification(request, terminal, multiplexer)],
            TerminalName::Kitty => kitty_notification(request, terminal, multiplexer, kitty_id),
            TerminalName::Ghostty => vec![ghostty_notification(request, terminal, multiplexer)],
            // Historical Apple Terminal auto-routing depends on the active
            // profile's exact `Bell === false` plist value. That fact is not
            // projected to this renderer, so fail closed instead of guessing.
            TerminalName::AppleTerminal
            | TerminalName::WarpTerminal
            | TerminalName::VsCode
            | TerminalName::Cursor
            | TerminalName::Windsurf
            | TerminalName::Zed
            | TerminalName::WezTerm
            | TerminalName::Alacritty
            | TerminalName::Rio
            | TerminalName::Foot
            | TerminalName::JetBrains
            | TerminalName::Vte
            | TerminalName::Terminator
            | TerminalName::WindowsTerminal
            | TerminalName::Otty
            | TerminalName::CrabCodeDesktop
            | TerminalName::Unknown => Vec::new(),
        },
        NotificationChannel::Iterm2 => {
            vec![iterm2_notification(request, terminal, multiplexer)]
        }
        NotificationChannel::Iterm2WithBell => vec![
            iterm2_notification(request, terminal, multiplexer),
            vec![BEL],
        ],
        NotificationChannel::TerminalBell => vec![vec![BEL]],
        NotificationChannel::Kitty => kitty_notification(request, terminal, multiplexer, kitty_id),
        NotificationChannel::Ghostty => {
            vec![ghostty_notification(request, terminal, multiplexer)]
        }
        NotificationChannel::Disabled => Vec::new(),
    }
}

fn iterm2_notification(
    request: &TerminalNotificationRequest,
    terminal: TerminalName,
    multiplexer: MultiplexerRoute,
) -> Vec<u8> {
    let display = request.title.as_ref().map_or_else(
        || request.message.clone(),
        |title| format!("{title}:\n{}", request.message),
    );
    wrap_for_multiplexer(
        osc(terminal, &["9".to_string(), format!("\n\n{display}")]),
        multiplexer,
    )
}

fn kitty_notification(
    request: &TerminalNotificationRequest,
    terminal: TerminalName,
    multiplexer: MultiplexerRoute,
    id: u16,
) -> Vec<Vec<u8>> {
    let title = request.title.as_deref().unwrap_or(DEFAULT_TITLE);
    [
        vec![
            "99".to_string(),
            format!("i={id}:d=0:p=title"),
            title.to_string(),
        ],
        vec![
            "99".to_string(),
            format!("i={id}:p=body"),
            request.message.clone(),
        ],
        vec![
            "99".to_string(),
            format!("i={id}:d=1:a=focus"),
            String::new(),
        ],
    ]
    .into_iter()
    .map(|parts| wrap_for_multiplexer(osc(terminal, &parts), multiplexer))
    .collect()
}

fn ghostty_notification(
    request: &TerminalNotificationRequest,
    terminal: TerminalName,
    multiplexer: MultiplexerRoute,
) -> Vec<u8> {
    let title = request.title.as_deref().unwrap_or(DEFAULT_TITLE);
    wrap_for_multiplexer(
        osc(
            terminal,
            &[
                "777".to_string(),
                "notify".to_string(),
                title.to_string(),
                request.message.clone(),
            ],
        ),
        multiplexer,
    )
}

fn osc(terminal: TerminalName, parts: &[String]) -> Vec<u8> {
    let mut sequence = Vec::new();
    sequence.extend_from_slice(&[ESC, b']']);
    sequence.extend_from_slice(parts.join(";").as_bytes());
    if terminal == TerminalName::Kitty {
        sequence.extend_from_slice(&[ESC, b'\\']);
    } else {
        sequence.push(BEL);
    }
    sequence
}

fn wrap_for_multiplexer(sequence: Vec<u8>, route: MultiplexerRoute) -> Vec<u8> {
    if route.tmux {
        let mut wrapped = Vec::with_capacity(sequence.len().saturating_mul(2).saturating_add(9));
        wrapped.extend_from_slice(&[ESC, b'P']);
        wrapped.extend_from_slice(b"tmux;");
        for byte in sequence {
            if byte == ESC {
                wrapped.push(ESC);
            }
            wrapped.push(byte);
        }
        wrapped.extend_from_slice(&[ESC, b'\\']);
        return wrapped;
    }
    if route.screen {
        let mut wrapped = Vec::with_capacity(sequence.len().saturating_add(4));
        wrapped.extend_from_slice(&[ESC, b'P']);
        wrapped.extend_from_slice(&sequence);
        wrapped.extend_from_slice(&[ESC, b'\\']);
        return wrapped;
    }
    sequence
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(message: &str, title: Option<&str>) -> TerminalNotificationRequest {
        TerminalNotificationRequest {
            message: message.to_string(),
            title: title.map(str::to_string),
        }
    }

    const DIRECT: MultiplexerRoute = MultiplexerRoute {
        tmux: false,
        screen: false,
    };

    #[test]
    fn iterm2_uses_historical_optional_title_and_terminal_terminator() {
        assert_eq!(
            notification_controls(
                NotificationChannel::Iterm2,
                TerminalName::Iterm2,
                DIRECT,
                &request("authenticated", None),
                7,
            ),
            [b"\x1b]9;\n\nauthenticated\x07".to_vec()]
        );
        assert_eq!(
            notification_controls(
                NotificationChannel::Iterm2,
                TerminalName::Kitty,
                DIRECT,
                &request("body", Some("Title")),
                7,
            ),
            [b"\x1b]9;\n\nTitle:\nbody\x1b\\".to_vec()]
        );
    }

    #[test]
    fn kitty_emits_three_correlated_osc99_controls() {
        assert_eq!(
            notification_controls(
                NotificationChannel::Kitty,
                TerminalName::Kitty,
                DIRECT,
                &request("authenticated", None),
                42,
            ),
            [
                b"\x1b]99;i=42:d=0:p=title;CrabCode\x1b\\".to_vec(),
                b"\x1b]99;i=42:p=body;authenticated\x1b\\".to_vec(),
                b"\x1b]99;i=42:d=1:a=focus;\x1b\\".to_vec(),
            ]
        );
    }

    #[test]
    fn ghostty_and_bell_routes_preserve_historical_bytes() {
        assert_eq!(
            notification_controls(
                NotificationChannel::Ghostty,
                TerminalName::Ghostty,
                DIRECT,
                &request("done", Some("Run")),
                1,
            ),
            [b"\x1b]777;notify;Run;done\x07".to_vec()]
        );
        assert_eq!(
            notification_controls(
                NotificationChannel::Iterm2WithBell,
                TerminalName::Iterm2,
                DIRECT,
                &request("done", None),
                1,
            ),
            [b"\x1b]9;\n\ndone\x07".to_vec(), b"\x07".to_vec()]
        );
    }

    #[test]
    fn tmux_and_screen_wrap_osc_but_never_wrap_raw_bell() {
        assert_eq!(
            notification_controls(
                NotificationChannel::Iterm2WithBell,
                TerminalName::Iterm2,
                MultiplexerRoute {
                    tmux: true,
                    screen: false,
                },
                &request("done", None),
                1,
            ),
            [
                b"\x1bPtmux;\x1b\x1b]9;\n\ndone\x07\x1b\\".to_vec(),
                b"\x07".to_vec(),
            ]
        );
        assert_eq!(
            notification_controls(
                NotificationChannel::Ghostty,
                TerminalName::Ghostty,
                MultiplexerRoute {
                    tmux: false,
                    screen: true,
                },
                &request("done", None),
                1,
            ),
            [b"\x1bP\x1b]777;notify;CrabCode;done\x07\x1b\\".to_vec()]
        );
    }

    #[test]
    fn auto_is_closed_to_only_proven_historical_terminal_routes() {
        for terminal in [
            TerminalName::Iterm2,
            TerminalName::Kitty,
            TerminalName::Ghostty,
        ] {
            assert!(
                !notification_controls(
                    NotificationChannel::Auto,
                    terminal,
                    DIRECT,
                    &request("done", None),
                    1,
                )
                .is_empty()
            );
        }
        for terminal in [
            TerminalName::AppleTerminal,
            TerminalName::Unknown,
            TerminalName::WezTerm,
            TerminalName::WindowsTerminal,
        ] {
            assert!(
                notification_controls(
                    NotificationChannel::Auto,
                    terminal,
                    DIRECT,
                    &request("done", None),
                    1,
                )
                .is_empty()
            );
        }
    }
}

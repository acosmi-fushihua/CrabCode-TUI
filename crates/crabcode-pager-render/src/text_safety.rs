//! Terminal-text safety boundary.
//!
//! Backend content is data, never a terminal-control channel. Ratatui
//! ultimately writes text to a real TTY, so every server- or workspace-owned
//! string must pass through this module before it is rendered.  Keeping the
//! policy in one place prevents a newly added tool block from accidentally
//! re-introducing ANSI/OSC injection.

use std::borrow::Cow;

/// Maximum bytes a single rendered field may contribute to one frame.
///
/// The complete value remains in the reducer/session model.  This is a render
/// budget only, which bounds frame construction without corrupting replay or
/// export data.
pub const MAX_RENDER_FIELD_BYTES: usize = 256 * 1024;

/// Match JavaScript `String.prototype.trim`, including BOM and excluding
/// Unicode NEXT LINE. CrabCode's legacy CLI uses JavaScript trim for
/// renderer-owned prefill and session-title search values.
#[doc(hidden)]
pub fn trim_ecmascript_whitespace(value: &str) -> &str {
    value.trim_matches(is_ecmascript_whitespace)
}

/// Match the code-point set used by ECMAScript `\s`.
///
/// `String.prototype.trim` and the fixed `/mcp` command's `/\s+/` tokenizer
/// share this WhiteSpace + LineTerminator set. Rust's `char::is_whitespace`
/// differs at both U+FEFF (ECMAScript yes) and U+0085 (ECMAScript no).
#[doc(hidden)]
pub const fn is_ecmascript_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}'
            ..='\u{000D}'
                | '\u{0020}'
                | '\u{00A0}'
                | '\u{1680}'
                | '\u{2000}'
                ..='\u{200A}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202F}'
                    | '\u{205F}'
                    | '\u{3000}'
                    | '\u{FEFF}'
    )
}

/// Converts untrusted text into terminal-safe, directionally explicit text.
///
/// Newlines are retained. Tabs are normalized to four spaces so a terminal's
/// tab stops cannot change layout. C0/C1 controls, DEL, ANSI ESC, and Unicode
/// bidi control characters are rendered as visible tokens. The function does
/// not interpret or partially strip escape sequences: making ESC itself
/// visible is sufficient to turn the remainder into inert text.
pub fn sanitize_terminal_text(input: &str) -> Cow<'_, str> {
    if input.chars().all(is_directly_renderable) {
        return Cow::Borrowed(input);
    }

    let mut output = String::with_capacity(input.len());
    for character in input.chars() {
        match character {
            '\n' => output.push('\n'),
            '\t' => output.push_str("    "),
            '\r' => output.push('␍'),
            '\u{1b}' => output.push('␛'),
            '\u{7f}' => output.push('␡'),
            character if is_bidi_control(character) => {
                use std::fmt::Write as _;
                let _format_result = write!(output, "⟪U+{:04X}⟫", character as u32);
            }
            character if character.is_control() => {
                use std::fmt::Write as _;
                let _format_result = write!(output, "⟪U+{:04X}⟫", character as u32);
            }
            character => output.push(character),
        }
    }
    Cow::Owned(output)
}

/// Applies the terminal-safety policy and a UTF-8-safe per-field render
/// budget. The returned marker is intentionally plain text and cannot be
/// mistaken for content that was present in the source.
pub fn sanitize_bounded_terminal_text(input: &str) -> Cow<'_, str> {
    let sanitized = sanitize_terminal_text(input);
    if sanitized.len() <= MAX_RENDER_FIELD_BYTES {
        return sanitized;
    }

    let mut boundary = MAX_RENDER_FIELD_BYTES;
    while !sanitized.is_char_boundary(boundary) {
        boundary -= 1;
    }
    let omitted = sanitized.len() - boundary;
    Cow::Owned(format!(
        "{}\n⟪render-only truncation: {omitted} UTF-8 byte(s) omitted; full value retained⟫",
        &sanitized[..boundary]
    ))
}

fn is_directly_renderable(character: char) -> bool {
    (character == '\n' || (!character.is_control() && character != '\t'))
        && !is_bidi_control(character)
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_unicode_is_zero_copy() {
        let input = "CrabCode 中文 🦀 e\u{301}\n";
        let sanitized = sanitize_terminal_text(input);
        assert!(matches!(sanitized, Cow::Borrowed(_)));
        assert_eq!(sanitized, input);
    }

    #[test]
    fn ecmascript_trim_keeps_its_distinct_unicode_contract() {
        assert_eq!(trim_ecmascript_whitespace("\u{FEFF} x \u{FEFF}"), "x");
        assert_eq!(
            trim_ecmascript_whitespace("\u{0085}x\u{0085}"),
            "\u{0085}x\u{0085}"
        );
    }

    #[test]
    fn ansi_osc_and_c0_controls_become_inert_visible_text() {
        let input = "\u{1b}[31mred\u{1b}[0m\u{1b}]52;c;secret\u{7}\r\tend";
        let sanitized = sanitize_terminal_text(input);
        assert!(!sanitized.contains('\u{1b}'));
        assert!(!sanitized.contains('\u{7}'));
        assert_eq!(sanitized, "␛[31mred␛[0m␛]52;c;secret⟪U+0007⟫␍    end");
    }

    #[test]
    fn bidi_overrides_are_exposed_instead_of_reordering_paths() {
        let input = "safe/\u{202e}gpj.exe";
        assert_eq!(sanitize_terminal_text(input), "safe/⟪U+202E⟫gpj.exe");
    }

    #[test]
    fn render_budget_never_splits_utf8_and_preserves_full_model_contract() {
        let input = "界".repeat(MAX_RENDER_FIELD_BYTES);
        let bounded = sanitize_bounded_terminal_text(&input);
        assert!(bounded.contains("render-only truncation"));
        assert!(bounded.starts_with('界'));
        assert!(bounded.len() < input.len());
        assert_eq!(input.chars().count(), MAX_RENDER_FIELD_BYTES);
    }
}

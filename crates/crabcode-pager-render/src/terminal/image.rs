//! Terminal image-protocol adapter used by the fixed overlay state machine.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine as _;

use super::{TerminalName, terminal_context};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GraphicsProtocol {
    Kitty,
    ITerm2,
    #[default]
    None,
}

impl GraphicsProtocol {
    pub fn supports_images(self) -> bool {
        !matches!(self, Self::None)
    }
}

static GRAPHICS_PROTOCOL: OnceLock<GraphicsProtocol> = OnceLock::new();
static INLINE_OVERLAY_FORCE_OFF: AtomicBool = AtomicBool::new(false);

/// Force scrollback inline-media overlays off for static terminal commits.
pub fn set_inline_overlay_force_off(off: bool) {
    INLINE_OVERLAY_FORCE_OFF.store(off, Ordering::Relaxed);
}

/// Whether the direct TUI owner has selected a static, draw-loop-free commit.
#[must_use]
pub fn scrollback_inline_overlay_forced_off() -> bool {
    INLINE_OVERLAY_FORCE_OFF.load(Ordering::Relaxed)
}

/// Whether the current terminal can safely host scrollback inline-media
/// overlays.
///
/// This is narrower than generic image support: scrollback media needs Kitty
/// placement ids, z-index, clearing, and source cropping so images track the
/// text grid.
pub fn scrollback_inline_overlay_active() -> bool {
    if INLINE_OVERLAY_FORCE_OFF.load(Ordering::Relaxed) {
        return false;
    }
    let protocol = detect_graphics_protocol();
    if test_protocol_override_active() {
        return protocol == GraphicsProtocol::Kitty;
    }
    scrollback_inline_overlay_active_for_brand(protocol, terminal_context().brand)
}

#[cfg(any(test, feature = "test-support"))]
fn test_protocol_override_active() -> bool {
    TEST_PROTOCOL_OVERRIDE.with(|current| current.get().is_some())
}

#[cfg(not(any(test, feature = "test-support")))]
fn test_protocol_override_active() -> bool {
    false
}

fn scrollback_inline_overlay_active_for_brand(
    protocol: GraphicsProtocol,
    brand: TerminalName,
) -> bool {
    matches!(
        (protocol, brand),
        (
            GraphicsProtocol::Kitty,
            TerminalName::Kitty | TerminalName::Ghostty | TerminalName::WezTerm
        )
    )
}

#[cfg(any(test, feature = "test-support"))]
thread_local! {
    static TEST_PROTOCOL_OVERRIDE: std::cell::Cell<Option<GraphicsProtocol>> =
        const { std::cell::Cell::new(None) };
}

pub fn detect_graphics_protocol() -> GraphicsProtocol {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(protocol) = TEST_PROTOCOL_OVERRIDE.with(std::cell::Cell::get) {
        return protocol;
    }
    *GRAPHICS_PROTOCOL.get_or_init(|| {
        let context = terminal_context();
        if context.graphics_protocol_skip_reason().is_some() {
            return GraphicsProtocol::None;
        }
        protocol_for_brand(context.brand, cfg!(target_os = "windows"))
    })
}

pub fn protocol_for_brand(brand: TerminalName, is_windows: bool) -> GraphicsProtocol {
    if is_windows {
        return GraphicsProtocol::None;
    }
    match brand {
        TerminalName::Kitty
        | TerminalName::Ghostty
        | TerminalName::WezTerm
        | TerminalName::WarpTerminal => GraphicsProtocol::Kitty,
        TerminalName::Iterm2
        | TerminalName::AppleTerminal
        | TerminalName::VsCode
        | TerminalName::Cursor
        | TerminalName::Windsurf
        | TerminalName::Zed
        | TerminalName::Alacritty
        | TerminalName::Rio
        | TerminalName::Foot
        | TerminalName::JetBrains
        | TerminalName::CrabCodeDesktop
        | TerminalName::Vte
        | TerminalName::Terminator
        | TerminalName::WindowsTerminal
        | TerminalName::Otty
        | TerminalName::Unknown => GraphicsProtocol::None,
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn set_protocol_for_test(protocol: GraphicsProtocol) -> TestProtocolGuard {
    TEST_PROTOCOL_OVERRIDE.with(|current| current.set(Some(protocol)));
    TestProtocolGuard
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestProtocolGuard;

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestProtocolGuard {
    fn drop(&mut self) {
        TEST_PROTOCOL_OVERRIDE.with(|current| current.set(None));
    }
}

pub(super) const KITTY_PLACEMENT_ID: u32 = 1;

fn is_encoded_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'])
}

fn transmit_kitty_image(image_data: &[u8], image_id: u32) -> String {
    kitty_chunked_escape(image_data, &format!("a=t,f=100,t=d,q=2,i={image_id}"))
}

fn kitty_chunked_escape(image_data: &[u8], first_chunk_header: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(image_data);
    let chunks = encoded.as_bytes().chunks(4096).collect::<Vec<_>>();
    let mut output = String::new();
    for (index, chunk) in chunks.iter().enumerate() {
        let more = usize::from(index + 1 < chunks.len());
        let chunk = std::str::from_utf8(chunk).unwrap_or("");
        if index == 0 {
            output.push_str(&format!(
                "\x1b_G{first_chunk_header},m={more};{chunk}\x1b\\"
            ));
        } else {
            output.push_str(&format!("\x1b_Gq=2,m={more};{chunk}\x1b\\"));
        }
    }
    output
}

fn place_kitty_image(image_id: u32, cols: u16, rows: u16) -> String {
    format!("\x1b_Ga=p,i={image_id},p={image_id},c={cols},r={rows},z=1,C=1,q=2\x1b\\")
}

pub(super) fn clear_kitty_image(image_id: u32) -> String {
    format!("\x1b_Ga=d,d=i,i={image_id},q=2\x1b\\")
}

fn render_iterm2_image(image_data: &[u8], cols: u16, rows: u16) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(image_data);
    format!(
        "\x1b]1337;File=inline=1;width={cols}cells;height={rows}cells;preserveAspectRatio=1:{encoded}\x07"
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_overlay_image_escapes_for_protocol(
    protocol: GraphicsProtocol,
    image_data: &[u8],
    cols: u16,
    rows: u16,
    cell_x: u16,
    cell_y: u16,
    retransmit: bool,
) -> Option<String> {
    if protocol == GraphicsProtocol::None {
        return None;
    }
    let mut escapes = format!("\x1b[{};{}H", cell_y + 1, cell_x + 1);
    match protocol {
        GraphicsProtocol::Kitty => {
            if retransmit {
                if !is_encoded_png(image_data) {
                    return None;
                }
                escapes.push_str(&transmit_kitty_image(image_data, KITTY_PLACEMENT_ID));
            }
            escapes.push_str(&place_kitty_image(KITTY_PLACEMENT_ID, cols, rows));
        }
        GraphicsProtocol::ITerm2 => {
            if retransmit {
                escapes.push_str(&render_iterm2_image(image_data, cols, rows));
            }
        }
        GraphicsProtocol::None => unreachable!(),
    }
    Some(escapes)
}

pub fn fit_image_to_cells(
    image_width: u32,
    image_height: u32,
    max_cols: u16,
    max_rows: u16,
) -> (u16, u16) {
    if image_width == 0 || image_height == 0 || max_cols == 0 || max_rows == 0 {
        return (max_cols.max(1), max_rows.max(1));
    }
    let image_aspect = image_width as f64 / image_height as f64;
    let cols_per_row = image_aspect / 0.5_f64;
    let cols_by_width = max_cols;
    let rows_by_width = (cols_by_width as f64 / cols_per_row).round() as u16;
    let rows_by_height = max_rows;
    let cols_by_height = (rows_by_height as f64 * cols_per_row).round() as u16;
    if rows_by_width <= max_rows {
        (cols_by_width, rows_by_width.max(1))
    } else {
        (cols_by_height.min(max_cols).max(1), rows_by_height)
    }
}

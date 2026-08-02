//! Native CrabCode image-preview overlay and terminal graphics protocol.
//!
//! This module is presentation-only. It consumes image bytes already owned by
//! the Rust TUI and paths already admitted by [`crate::tui_ui`]'s
//! `ArtifactProvenance`; it never invents a path or changes backend payloads.

use std::cell::Cell;
use std::fs::File;
use std::io::{Cursor, Read as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use base64::Engine as _;
use image::ImageEncoder as _;
use image::imageops::FilterType;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthStr as _;

use crate::composer_image::{
    MAX_COMPOSER_IMAGE_ENCODED_BYTES, MAX_COMPOSER_IMAGE_RAW_BYTES,
    MAX_COMPOSER_IMAGE_SOURCE_DIMENSION,
};
use crate::terminal_capabilities::{
    MultiplexerKind, TerminalContext, TerminalName, terminal_context,
};
use crate::text_safety::sanitize_bounded_terminal_text;
use crate::tui_app::UiLanguage;

const MIN_BOX_WIDTH: u16 = 28;
const MIN_PIXEL_BOX_HEIGHT: u16 = 8;
const MIN_META_BOX_HEIGHT: u16 = 6;
const META_PREVIEW_WIDTH_RATIO: f32 = 0.75;
const META_CONTENT_LINES: u16 = 4;
const META_BOX_CHROME_ROWS: u16 = 2;
#[cfg(test)]
const KITTY_PLACEMENT_ID: u32 = 1;

static NEXT_IMAGE_IDENTITY: AtomicU64 = AtomicU64::new(1);

/// Terminal graphics protocol selected by the fixed capability matrix.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum GraphicsProtocol {
    Kitty,
    // Kept as the fixed low-level protocol/test seam while production
    // capability selection deliberately maps iTerm2 to metadata fallback.
    #[allow(dead_code)]
    ITerm2,
    #[default]
    None,
}

#[cfg(test)]
impl GraphicsProtocol {
    const fn supports_images(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CrabCodeImagePreview {
    pub(crate) identity: u64,
    pub(crate) display_number: usize,
    pub(crate) mime_type: String,
    pub(crate) dimensions: Option<(u32, u32)>,
    pub(crate) byte_len: usize,
    pub(crate) pixels: Option<Arc<[u8]>>,
    pub(crate) display_path: Option<PathBuf>,
    pub(crate) preview_failed: bool,
}

impl CrabCodeImagePreview {
    pub(crate) fn without_path(
        identity: u64,
        display_number: usize,
        mime_type: String,
        dimensions: Option<(u32, u32)>,
        byte_len: usize,
        pixels: Option<Arc<[u8]>>,
    ) -> Self {
        let preview_failed = pixels.is_none();
        Self {
            identity,
            display_number,
            mime_type,
            dimensions,
            byte_len,
            pixels,
            display_path: None,
            preview_failed,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImagePlacement {
    cols: u16,
    rows: u16,
    x: u16,
    y: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ImageOverlayGeometry {
    overlay_rect: Rect,
    image_placement: Option<ImagePlacement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelOwner {
    identity: u64,
    protocol: GraphicsProtocol,
}

thread_local! {
    static COMMITTED_OWNER: Cell<Option<PixelOwner>> = const { Cell::new(None) };
    static TERMINAL_STATE_KNOWN: Cell<bool> = const { Cell::new(false) };
    #[cfg(test)]
    static TEST_PROTOCOL: Cell<Option<GraphicsProtocol>> = const { Cell::new(None) };
}

/// Escape bytes staged for the current synchronized frame.
///
/// Ownership is committed only after the complete frame has been accepted by
/// the bounded writer. A discarded or failed frame therefore retransmits on
/// the next attempt instead of assuming pixels reached the terminal.
#[derive(Debug)]
pub(crate) struct CrabCodeImageEscapes {
    post_flush: crabcode_pager_render::terminal::overlay::PostFlush,
    next_owner: Option<PixelOwner>,
}

impl CrabCodeImageEscapes {
    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.post_flush.as_str().as_bytes()
    }

    pub(crate) fn has_bytes(&self) -> bool {
        !self.post_flush.as_str().is_empty()
    }

    pub(crate) fn commit(self) {
        // The frame has been accepted by CrabCode's ordered terminal writer.
        // Drive the fixed-upstream PostFlush transition on this UI thread so
        // its thread-local owner remains colocated with subsequent renders.
        self.post_flush
            .write_to(&mut std::io::sink())
            .expect("writing a committed image transition to the sink cannot fail");
        COMMITTED_OWNER.with(|owner| owner.set(self.next_owner));
        TERMINAL_STATE_KNOWN.with(|known| known.set(true));
    }

    #[cfg(test)]
    fn as_str(&self) -> &str {
        self.post_flush.as_str()
    }

    #[cfg(test)]
    pub(crate) fn plain_for_test(bytes: impl Into<String>) -> Self {
        Self {
            post_flush: crabcode_pager_render::terminal::overlay::PostFlush::plain(bytes.into()),
            next_owner: None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ImageOverlayRender {
    pub(crate) pixels_active: bool,
    pub(crate) escapes: Option<CrabCodeImageEscapes>,
    #[cfg(test)]
    image_placement: Option<ImagePlacement>,
}

pub(crate) fn next_image_identity() -> u64 {
    NEXT_IMAGE_IDENTITY.fetch_add(1, Ordering::Relaxed)
}

pub(crate) fn reset_terminal_image_owner() {
    crabcode_pager_render::terminal::overlay::reset_owner();
    COMMITTED_OWNER.with(|owner| owner.set(None));
    TERMINAL_STATE_KNOWN.with(|known| known.set(false));
}

/// Return a one-shot clear command when a previously committed Kitty image is
/// active or this process does not yet know the terminal's ID-1 state. `None`
/// means no terminal pixel state can or needs to be changed.
pub(crate) fn clear_committed_image() -> Option<CrabCodeImageEscapes> {
    let owner = COMMITTED_OWNER.with(Cell::get);
    let state_known = TERMINAL_STATE_KNOWN.with(Cell::get);
    let protocol = owner
        .map(|owner| owner.protocol)
        .unwrap_or_else(detect_graphics_protocol);
    match protocol {
        GraphicsProtocol::Kitty => Some(CrabCodeImageEscapes {
            post_flush: crabcode_pager_render::terminal::overlay::PostFlush::from(
                crabcode_pager_render::terminal::overlay::clear_kitty(),
            ),
            next_owner: None,
        })
        .filter(|_| owner.is_some() || !state_known),
        GraphicsProtocol::ITerm2 => None,
        GraphicsProtocol::None => None,
    }
}

/// Detect the protocol from the existing CrabCode terminal capability model.
pub(crate) fn detect_graphics_protocol() -> GraphicsProtocol {
    #[cfg(test)]
    if let Some(protocol) = TEST_PROTOCOL.with(Cell::get) {
        return protocol;
    }
    protocol_for_context(terminal_context(), cfg!(target_os = "windows"))
}

fn protocol_for_context(context: &TerminalContext, is_windows: bool) -> GraphicsProtocol {
    if is_windows || context.multiplexer == MultiplexerKind::Tmux {
        return GraphicsProtocol::None;
    }
    protocol_for_brand(context.brand, false)
}

fn protocol_for_brand(brand: TerminalName, is_windows: bool) -> GraphicsProtocol {
    if is_windows {
        return GraphicsProtocol::None;
    }
    match brand {
        TerminalName::Kitty
        | TerminalName::Ghostty
        | TerminalName::WezTerm
        | TerminalName::WarpTerminal => GraphicsProtocol::Kitty,
        // The inline-image protocol has no reliable overlay id, z-index,
        // source crop, or clear operation. Keep its encoder for protocol
        // parity tests, but fail closed to metadata in production.
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
        | TerminalName::Vte
        | TerminalName::Terminator
        | TerminalName::WindowsTerminal
        | TerminalName::Otty
        | TerminalName::CrabCodeDesktop
        | TerminalName::Unknown => GraphicsProtocol::None,
    }
}

/// Convert validated encoded image bytes into the PNG payload required by the
/// Kitty direct-data path. The result is bounded so it cannot exceed the
/// existing terminal-frame safety envelope after base64 expansion.
pub(crate) fn prepare_terminal_preview(bytes: &[u8]) -> Option<Arc<[u8]>> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_COMPOSER_IMAGE_RAW_BYTES {
        return None;
    }
    let format = image::guess_format(bytes).ok()?;
    let dimensions = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if dimensions.0 > MAX_COMPOSER_IMAGE_SOURCE_DIMENSION
        || dimensions.1 > MAX_COMPOSER_IMAGE_SOURCE_DIMENSION
        || u64::from(dimensions.0).saturating_mul(u64::from(dimensions.1))
            > crate::composer_image::MAX_COMPOSER_IMAGE_PIXELS
    {
        return None;
    }
    if format == image::ImageFormat::Png && bytes.len() <= MAX_COMPOSER_IMAGE_ENCODED_BYTES {
        return Some(Arc::from(bytes));
    }

    let decoded = image::load_from_memory(bytes).ok()?;
    let display = if decoded.width() > crate::composer_image::MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION
        || decoded.height() > crate::composer_image::MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION
    {
        decoded.resize(
            crate::composer_image::MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION,
            crate::composer_image::MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION,
            FilterType::Lanczos3,
        )
    } else {
        decoded
    };
    let rgba = display.to_rgba8();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            rgba.as_raw(),
            display.width(),
            display.height(),
            image::ColorType::Rgba8.into(),
        )
        .ok()?;
    (png.len() <= MAX_COMPOSER_IMAGE_ENCODED_BYTES).then(|| Arc::from(png))
}

/// Load a preview only for a path already admitted by ArtifactProvenance.
///
/// The caller owns that provenance check. This function deliberately has no
/// relative-path or workspace lookup fallback.
pub(crate) fn load_provenance_image(path: &Path, display_number: usize) -> CrabCodeImagePreview {
    debug_assert!(path.is_absolute());
    let identity = next_image_identity();
    let metadata = path.metadata().ok();
    let byte_len = metadata
        .as_ref()
        .and_then(|metadata| usize::try_from(metadata.len()).ok())
        .unwrap_or(0);
    let fallback_mime = mime_from_extension(path).to_string();
    let mut fallback = CrabCodeImagePreview {
        identity,
        display_number,
        mime_type: fallback_mime,
        dimensions: None,
        byte_len,
        pixels: None,
        display_path: Some(path.to_path_buf()),
        preview_failed: true,
    };
    let Some(metadata) = metadata.filter(|metadata| metadata.is_file()) else {
        return fallback;
    };
    if metadata.len() > MAX_COMPOSER_IMAGE_RAW_BYTES {
        return fallback;
    }
    let Ok(mut file) = File::open(path) else {
        return fallback;
    };
    let mut bytes = Vec::with_capacity(byte_len);
    if file
        .by_ref()
        .take(MAX_COMPOSER_IMAGE_RAW_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_COMPOSER_IMAGE_RAW_BYTES
        || bytes.is_empty()
    {
        return fallback;
    }
    let Ok(format) = image::guess_format(&bytes) else {
        return fallback;
    };
    fallback.mime_type = mime_for_format(format).to_string();
    fallback.dimensions = image::ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .ok()
        .and_then(|reader| reader.into_dimensions().ok());
    fallback.pixels = prepare_terminal_preview(&bytes);
    fallback.preview_failed = fallback.pixels.is_none();
    fallback
}

/// Render the complete pixels/path matrix and return any post-flush terminal
/// pixel update. Callers must stage the returned escape through
/// [`crate::terminal_writer`] rather than writing it during buffer painting.
pub(crate) fn render_image_overlay(
    buf: &mut Buffer,
    area: Rect,
    image: &CrabCodeImagePreview,
    bg: Color,
    text_fg: Color,
    border_fg: Color,
    language: UiLanguage,
) -> Option<ImageOverlayRender> {
    let fixed_image = fixed_renderer_image(image);
    let fixed_protocol = crabcode_pager_render::terminal::image::detect_graphics_protocol();
    let show_pixels =
        fixed_protocol.supports_images() && image.pixels.as_deref().is_some_and(is_encoded_png);
    let min_height = if show_pixels {
        MIN_PIXEL_BOX_HEIGHT
    } else {
        MIN_META_BOX_HEIGHT
    };
    if area.width < MIN_BOX_WIDTH || area.height < min_height {
        return None;
    }

    let escapes = crabcode_pager_render::render::image_overlay::render_image_overlay(
        buf,
        area,
        &fixed_image,
        bg,
        text_fg,
        border_fg,
    );
    if language == UiLanguage::ZhCn {
        let repainted = paint_image_overlay_chrome(
            buf,
            area,
            image,
            show_pixels,
            fixed_protocol.supports_images(),
            bg,
            text_fg,
            border_fg,
            language,
        );
        debug_assert!(
            repainted.is_some(),
            "the localized chrome must use the fixed renderer's accepted geometry"
        );
    }
    let pixels_active = escapes.is_some();
    let next_owner = pixels_active.then_some(PixelOwner {
        identity: image.identity,
        protocol: match fixed_protocol {
            crabcode_pager_render::terminal::image::GraphicsProtocol::Kitty => {
                GraphicsProtocol::Kitty
            }
            crabcode_pager_render::terminal::image::GraphicsProtocol::ITerm2 => {
                GraphicsProtocol::ITerm2
            }
            crabcode_pager_render::terminal::image::GraphicsProtocol::None => {
                GraphicsProtocol::None
            }
        },
    });
    Some(ImageOverlayRender {
        pixels_active,
        escapes: escapes.map(|escapes| CrabCodeImageEscapes {
            post_flush: escapes.into(),
            next_owner,
        }),
        #[cfg(test)]
        image_placement: None,
    })
}

fn fixed_renderer_image(
    image: &CrabCodeImagePreview,
) -> crabcode_pager_render::prompt_images::PastedImage {
    let dimensions = image.dimensions.unwrap_or((640, 480));
    let preview = match &image.pixels {
        Some(bytes) => crabcode_pager_render::prompt_images::PromptImagePreview::ready(
            image.identity,
            Arc::clone(bytes),
            dimensions,
        ),
        None if image.preview_failed => {
            crabcode_pager_render::prompt_images::PromptImagePreview::failed(image.identity)
        }
        None => crabcode_pager_render::prompt_images::PromptImagePreview::pending(image.identity),
    };
    let source_path = image.display_path.as_deref().map(|path| {
        let displayed_path = path.display().to_string();
        let safe = sanitize_bounded_terminal_text(&displayed_path);
        PathBuf::from(safe.as_ref())
    });
    crabcode_pager_render::prompt_images::PastedImage {
        element_id: crabcode_ratatui_textarea::ElementId::from_raw(image.identity),
        display_number: image.display_number,
        mime_type: sanitize_bounded_terminal_text(&image.mime_type).into_owned(),
        dimensions: image.dimensions,
        byte_len: image.byte_len,
        encoded_bytes: image.pixels.clone(),
        source_path,
        staged_temp_path: None,
        session_image_path: None,
        preview,
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_image_overlay_chrome(
    buf: &mut Buffer,
    area: Rect,
    image: &CrabCodeImagePreview,
    show_pixels: bool,
    protocol_supports_images: bool,
    bg: Color,
    text_fg: Color,
    border_fg: Color,
    language: UiLanguage,
) -> Option<ImageOverlayGeometry> {
    let geometry = overlay_geometry(
        area,
        show_pixels,
        image.display_path.is_some(),
        image.dimensions.unwrap_or((640, 480)),
    )?;
    let overlay_rect = geometry.overlay_rect;

    Clear.render(overlay_rect, buf);
    buf.set_style(overlay_rect, Style::default().fg(text_fg).bg(bg));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_fg).bg(bg))
        .style(Style::default().bg(bg));
    let inner = block.inner(overlay_rect);
    block.render(overlay_rect, buf);

    let title_text = match language {
        UiLanguage::ZhCn => format!(" 图片 #{} ", image.display_number),
        UiLanguage::EnUs => format!(" Image #{} ", image.display_number),
    };
    let meta = build_meta_line(image);
    let full_title = if meta.width() + title_text.width() + 6 < usize::from(overlay_rect.width) {
        format!("{title_text}\u{2500} {meta} ")
    } else {
        title_text
    };
    let title_width = u16::try_from(full_title.width())
        .unwrap_or(u16::MAX)
        .min(overlay_rect.width);
    let title_x = overlay_rect.x + overlay_rect.width.saturating_sub(title_width) / 2;
    buf.set_span(
        title_x,
        overlay_rect.y,
        &Span::styled(
            full_title,
            Style::default()
                .fg(text_fg)
                .bg(bg)
                .add_modifier(Modifier::BOLD),
        ),
        title_width,
    );

    if inner.width == 0 || inner.height == 0 {
        return Some(geometry);
    }

    let path_footer = image.display_path.as_deref().filter(|_| inner.height >= 2);
    let image_inner = if let Some(path) = path_footer {
        let footer_y = inner.y + inner.height - 1;
        paint_path_line(
            buf,
            inner.x,
            footer_y,
            inner.width,
            path,
            Style::default().fg(text_fg).bg(bg),
            language,
        );
        Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(1),
        )
    } else {
        inner
    };

    if !show_pixels {
        let mut lines = vec![Line::from(format!(
            "{}{}",
            language.text("格式：", "Format: "),
            format_mime(&image.mime_type)
        ))];
        if let Some((width, height)) = image.dimensions {
            lines.push(Line::from(format!(
                "{}{width} x {height}",
                language.text("尺寸：", "Dimensions: ")
            )));
        }
        let status = if image.preview_failed {
            Some(language.text("预览不可用", "Preview unavailable"))
        } else if image.pixels.is_none() && protocol_supports_images {
            Some(language.text("预览等待中", "Preview pending"))
        } else {
            None
        };
        lines.push(Line::from(status.map(str::to_owned).unwrap_or_else(|| {
            format!(
                "{}{}",
                language.text("大小：", "Size: "),
                format_bytes(image.byte_len)
            )
        })));
        if path_footer.is_none()
            && let Some(path) = image.display_path.as_deref()
        {
            let displayed_path = path.display().to_string();
            let safe = sanitize_bounded_terminal_text(&displayed_path);
            lines.push(Line::from(format!(
                "{}{}",
                language.text("路径：", "Path: "),
                truncate_path_for_overlay(&safe, inner.width.saturating_sub(6) as usize)
            )));
        }
        Paragraph::new(lines)
            .style(Style::default().fg(text_fg).bg(bg))
            .wrap(Wrap { trim: false })
            .render(
                if path_footer.is_some() {
                    image_inner
                } else {
                    inner
                },
                buf,
            );
        return Some(geometry);
    }

    if image_inner.width > 0 && image_inner.height > 0 {
        let loading = language.text("正在加载...", "Loading...");
        let loading_width = u16::try_from(loading.width())
            .unwrap_or(u16::MAX)
            .min(image_inner.width);
        let x = image_inner.x + image_inner.width.saturating_sub(loading_width) / 2;
        let y = image_inner.y + image_inner.height / 2;
        buf.set_span(
            x,
            y,
            &Span::styled(loading, Style::default().fg(text_fg).bg(bg)),
            loading_width,
        );
    }

    Some(geometry)
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn render_image_overlay_for_protocol(
    buf: &mut Buffer,
    area: Rect,
    image: &CrabCodeImagePreview,
    protocol: GraphicsProtocol,
    bg: Color,
    text_fg: Color,
    border_fg: Color,
    language: UiLanguage,
) -> Option<ImageOverlayRender> {
    let show_pixels =
        protocol.supports_images() && image.pixels.as_deref().is_some_and(is_encoded_png);
    dim_area(buf, area);
    let geometry = paint_image_overlay_chrome(
        buf,
        area,
        image,
        show_pixels,
        protocol.supports_images(),
        bg,
        text_fg,
        border_fg,
        language,
    )?;
    if !show_pixels {
        return Some(ImageOverlayRender {
            pixels_active: false,
            escapes: None,
            #[cfg(test)]
            image_placement: None,
        });
    }
    let placement = geometry.image_placement?;
    let escapes = static_image_escapes(
        protocol,
        image.pixels.as_deref()?,
        placement,
        image.identity,
    );
    Some(ImageOverlayRender {
        pixels_active: true,
        escapes,
        #[cfg(test)]
        image_placement: Some(placement),
    })
}

fn overlay_geometry(
    area: Rect,
    show_pixels: bool,
    has_path: bool,
    dimensions: (u32, u32),
) -> Option<ImageOverlayGeometry> {
    let min_height = if show_pixels {
        MIN_PIXEL_BOX_HEIGHT
    } else {
        MIN_META_BOX_HEIGHT
    };
    if area.width < MIN_BOX_WIDTH || area.height < min_height {
        return None;
    }
    if show_pixels {
        let footer_rows = u16::from(has_path);
        let max_cols = area.width.saturating_sub(2).max(4);
        let max_rows = area
            .height
            .saturating_sub(2)
            .saturating_sub(footer_rows)
            .max(2);
        let (cols, rows) = fit_image_to_cells(dimensions.0, dimensions.1, max_cols, max_rows);
        let width = cols.saturating_add(2).clamp(MIN_BOX_WIDTH, area.width);
        let height = rows
            .saturating_add(2)
            .saturating_add(footer_rows)
            .clamp(MIN_PIXEL_BOX_HEIGHT, area.height);
        let x = area.x + area.width.saturating_sub(width) / 2;
        let y = area.y + area.height.saturating_sub(height) / 2;
        let inner_width = width.saturating_sub(2);
        let inner_height = height.saturating_sub(2).saturating_sub(footer_rows);
        return Some(ImageOverlayGeometry {
            overlay_rect: Rect::new(x, y, width, height),
            image_placement: Some(ImagePlacement {
                cols,
                rows,
                x: x + 1 + inner_width.saturating_sub(cols) / 2,
                y: y + 1 + inner_height.saturating_sub(rows) / 2,
            }),
        });
    }
    let width = ((area.width as f32) * META_PREVIEW_WIDTH_RATIO) as u16;
    let width = width.clamp(MIN_BOX_WIDTH, area.width);
    let height = (META_CONTENT_LINES + META_BOX_CHROME_ROWS)
        .min(area.height)
        .max(MIN_META_BOX_HEIGHT)
        .min(area.height);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height);
    Some(ImageOverlayGeometry {
        overlay_rect: Rect::new(x, y, width, height),
        image_placement: None,
    })
}

fn fit_image_to_cells(
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

#[cfg(test)]
fn static_image_escapes(
    protocol: GraphicsProtocol,
    image_data: &[u8],
    placement: ImagePlacement,
    identity: u64,
) -> Option<CrabCodeImageEscapes> {
    if protocol == GraphicsProtocol::None {
        return None;
    }
    let next_owner = PixelOwner { identity, protocol };
    let previous = COMMITTED_OWNER.with(Cell::get);
    let retransmit =
        previous.is_none_or(|owner| owner.identity != identity || owner.protocol != protocol);
    let mut bytes = format!("\x1b[{};{}H", placement.y + 1, placement.x + 1);
    match protocol {
        GraphicsProtocol::Kitty => {
            if retransmit {
                bytes.push_str(&transmit_kitty_image(image_data, KITTY_PLACEMENT_ID));
            }
            bytes.push_str(&place_kitty_image(
                KITTY_PLACEMENT_ID,
                placement.cols,
                placement.rows,
            ));
        }
        GraphicsProtocol::ITerm2 => {
            bytes.push_str(&render_iterm2_image(
                image_data,
                placement.cols,
                placement.rows,
            ));
        }
        GraphicsProtocol::None => unreachable!(),
    }
    Some(CrabCodeImageEscapes {
        post_flush: crabcode_pager_render::terminal::overlay::PostFlush::plain(bytes),
        next_owner: Some(next_owner),
    })
}

#[cfg(test)]
fn transmit_kitty_image(image_data: &[u8], image_id: u32) -> String {
    kitty_chunked_escape(image_data, &format!("a=t,f=100,t=d,q=2,i={image_id}"))
}

#[cfg(test)]
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

#[cfg(test)]
fn place_kitty_image(image_id: u32, cols: u16, rows: u16) -> String {
    format!("\x1b_Ga=p,i={image_id},p={image_id},c={cols},r={rows},z=1,C=1,q=2\x1b\\")
}

#[cfg(test)]
fn render_iterm2_image(image_data: &[u8], cols: u16, rows: u16) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(image_data);
    format!(
        "\x1b]1337;File=inline=1;width={cols}cells;height={rows}cells;preserveAspectRatio=1:{encoded}\x07"
    )
}

fn is_encoded_png(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'])
}

fn build_meta_line(image: &CrabCodeImagePreview) -> String {
    let mut parts = vec![format_mime(&image.mime_type)];
    if let Some((width, height)) = image.dimensions {
        parts.push(format!("{width}x{height}"));
    }
    parts.push(format_bytes(image.byte_len));
    if let Some(name) = image
        .display_path
        .as_deref()
        .and_then(Path::file_name)
        .map(|name| sanitize_bounded_terminal_text(&name.to_string_lossy()).into_owned())
    {
        parts.push(name);
    }
    parts.join(" \u{00b7} ")
}

fn format_mime(mime: &str) -> String {
    match mime {
        "image/png" => "PNG".to_string(),
        "image/jpeg" => "JPEG".to_string(),
        "image/tiff" => "TIFF".to_string(),
        "image/gif" => "GIF".to_string(),
        "image/webp" => "WebP".to_string(),
        "image/bmp" => "BMP".to_string(),
        other => sanitize_bounded_terminal_text(other).into_owned(),
    }
}

fn format_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn paint_path_line(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    path: &Path,
    text_style: Style,
    language: UiLanguage,
) {
    let displayed_path = path.display().to_string();
    let safe = sanitize_bounded_terminal_text(&displayed_path);
    let label = format!(
        "{}{}",
        language.text("路径：", "Path: "),
        truncate_path_for_overlay(&safe, width.saturating_sub(6) as usize)
    );
    let clipped = label.chars().take(width as usize).collect::<String>();
    buf.set_span(x, y, &Span::styled(clipped, text_style), width);
}

fn truncate_path_for_overlay(path: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let chars = path.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return path.to_string();
    }
    if max_chars <= 3 {
        return chars.into_iter().take(max_chars).collect();
    }
    let head = max_chars.saturating_sub(3) / 2;
    let tail = max_chars.saturating_sub(3) - head;
    format!(
        "{}...{}",
        chars[..head].iter().collect::<String>(),
        chars[chars.len() - tail..].iter().collect::<String>()
    )
}

#[cfg(test)]
fn dim_area(buf: &mut Buffer, area: Rect) {
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_style(cell.style().add_modifier(Modifier::DIM));
            }
        }
    }
}

fn mime_for_format(format: image::ImageFormat) -> &'static str {
    match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::WebP => "image/webp",
        image::ImageFormat::Tiff => "image/tiff",
        image::ImageFormat::Bmp => "image/bmp",
        _ => "application/octet-stream",
    }
}

fn mime_from_extension(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("tif" | "tiff") => "image/tiff",
        Some("bmp") => "image/bmp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    struct ProtocolGuard;

    impl ProtocolGuard {
        fn set(protocol: GraphicsProtocol) -> Self {
            TEST_PROTOCOL.with(|current| current.set(Some(protocol)));
            Self
        }
    }

    impl Drop for ProtocolGuard {
        fn drop(&mut self) {
            TEST_PROTOCOL.with(|current| current.set(None));
            reset_terminal_image_owner();
        }
    }

    fn png() -> Arc<[u8]> {
        Arc::from(vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'])
    }

    fn preview(path: Option<&Path>, pixels: bool) -> CrabCodeImagePreview {
        CrabCodeImagePreview {
            identity: next_image_identity(),
            display_number: 1,
            mime_type: "image/png".to_string(),
            dimensions: Some((640, 480)),
            byte_len: 1536,
            pixels: pixels.then(png),
            display_path: path.map(Path::to_path_buf),
            preview_failed: !pixels,
        }
    }

    fn buffer_text(buffer: &Buffer, area: Rect) -> String {
        (area.y..area.bottom())
            .map(|y| {
                let mut line = String::new();
                let mut continuation_cells = 0usize;
                for x in area.x..area.right() {
                    if continuation_cells > 0 {
                        continuation_cells -= 1;
                        continue;
                    }
                    let Some(cell) = buffer.cell((x, y)) else {
                        continue;
                    };
                    line.push_str(cell.symbol());
                    continuation_cells = cell.symbol().width().saturating_sub(1);
                }
                line
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_to_string(
        image: &CrabCodeImagePreview,
        protocol: GraphicsProtocol,
    ) -> (ImageOverlayRender, String) {
        render_to_string_in_language(image, protocol, UiLanguage::EnUs)
    }

    fn render_to_string_in_language(
        image: &CrabCodeImagePreview,
        protocol: GraphicsProtocol,
        language: UiLanguage,
    ) -> (ImageOverlayRender, String) {
        let area = Rect::new(10, 5, 60, 20);
        let mut buffer = Buffer::empty(area);
        let render = render_image_overlay_for_protocol(
            &mut buffer,
            area,
            image,
            protocol,
            Color::Black,
            Color::White,
            Color::Gray,
            language,
        )
        .expect("overlay");
        let text = buffer_text(&buffer, area);
        (render, text)
    }

    #[test]
    fn pixels_and_path_matrix_has_footer_and_post_flush_escape() {
        let _guard = ProtocolGuard::set(GraphicsProtocol::Kitty);
        let image = preview(Some(Path::new("/tmp/logo.png")), true);
        let (render, text) = render_to_string(&image, GraphicsProtocol::Kitty);
        let placement = render.image_placement.expect("pixel placement");
        let escapes = render.escapes.expect("pixel escapes");
        assert!(render.pixels_active);
        assert!(text.contains("Path: /tmp/logo.png"));
        assert!(escapes.as_str().starts_with(&format!(
            "\x1b[{};{}H",
            placement.y + 1,
            placement.x + 1
        )));
        assert!(escapes.as_str().contains("a=t"));
        assert!(
            escapes.as_str().contains(";iVBORw0KGgo=\x1b\\"),
            "the fixed PNG bytes must appear as their exact base64 Kitty transmission payload"
        );
        assert!(
            escapes
                .as_str()
                .contains(&format!("c={},r={}", placement.cols, placement.rows))
        );
    }

    #[test]
    fn pixels_only_matrix_omits_path_footer() {
        let _guard = ProtocolGuard::set(GraphicsProtocol::Kitty);
        let (render, text) = render_to_string(&preview(None, true), GraphicsProtocol::Kitty);
        assert!(render.pixels_active);
        assert!(render.image_placement.is_some());
        assert!(!text.contains("Path:"));
    }

    #[test]
    fn metadata_and_path_matrix_shows_stable_fields() {
        let _guard = ProtocolGuard::set(GraphicsProtocol::None);
        let image = preview(Some(Path::new("/tmp/logo.png")), true);
        let (render, text) = render_to_string(&image, GraphicsProtocol::None);
        assert!(!render.pixels_active);
        assert!(render.image_placement.is_none());
        assert!(render.escapes.is_none());
        assert!(text.contains("Format: PNG"));
        assert!(text.contains("Dimensions: 640 x 480"));
        assert!(text.contains("Size: 1.5 KB"));
        assert!(text.contains("Path: /tmp/logo.png"));
    }

    #[test]
    fn metadata_only_matrix_has_no_path_and_failed_preview_is_stable() {
        let _guard = ProtocolGuard::set(GraphicsProtocol::None);
        let mut image = preview(None, false);
        image.mime_type = "image/jpeg".to_string();
        let (render, text) = render_to_string(&image, GraphicsProtocol::None);
        assert!(!render.pixels_active);
        assert!(text.contains("Format: JPEG"));
        assert!(text.contains("Preview unavailable"));
        assert!(!text.contains("Loading..."));
        assert!(!text.contains("Path:"));
    }

    #[test]
    fn chinese_chrome_preserves_protocol_values_and_authority_path() {
        let _guard = ProtocolGuard::set(GraphicsProtocol::None);
        let mut image = preview(Some(Path::new("/tmp/原始-photo.jpg")), false);
        image.display_number = 17;
        image.mime_type = "image/jpeg".to_string();
        image.dimensions = Some((123, 456));
        let (render, text) =
            render_to_string_in_language(&image, GraphicsProtocol::None, UiLanguage::ZhCn);

        assert!(!render.pixels_active);
        assert!(text.contains("图片 #17"), "localized title:\n{text}");
        assert!(text.contains("格式：JPEG"), "format value:\n{text}");
        assert!(text.contains("尺寸：123 x 456"), "dimension value:\n{text}");
        assert!(text.contains("预览不可用"), "preview status:\n{text}");
        assert!(
            text.contains("路径：/tmp/原始-photo.jpg"),
            "authority path:\n{text}"
        );
        assert!(!text.contains("Format:"));
        assert!(!text.contains("Dimensions:"));
        assert!(!text.contains("Preview unavailable"));

        let mut available = preview(None, true);
        available.display_number = 18;
        let (_, size_text) =
            render_to_string_in_language(&available, GraphicsProtocol::None, UiLanguage::ZhCn);
        assert!(
            size_text.contains("大小：1.5 KB"),
            "localized size:\n{size_text}"
        );

        let mut pending = preview(None, false);
        pending.preview_failed = false;
        let (_, pending_text) =
            render_to_string_in_language(&pending, GraphicsProtocol::Kitty, UiLanguage::ZhCn);
        assert!(
            pending_text.contains("预览等待中"),
            "localized pending status:\n{pending_text}"
        );

        let (_, pixel_text) =
            render_to_string_in_language(&available, GraphicsProtocol::Kitty, UiLanguage::ZhCn);
        assert!(
            pixel_text.contains("正在加载..."),
            "localized pixel placeholder:\n{pixel_text}"
        );
    }

    #[test]
    fn production_overlay_repaints_only_fixed_chrome_for_chinese() {
        let _fixed_protocol = crabcode_pager_render::terminal::image::set_protocol_for_test(
            crabcode_pager_render::terminal::image::GraphicsProtocol::None,
        );
        let mut image = preview(Some(Path::new("/tmp/AUTHORITY-RAW.jpg")), false);
        image.display_number = 9;
        image.mime_type = "image/jpeg".to_string();
        image.dimensions = Some((321, 654));
        let area = Rect::new(10, 5, 80, 30);
        let mut buffer = Buffer::empty(area);
        render_image_overlay(
            &mut buffer,
            area,
            &image,
            Color::Black,
            Color::White,
            Color::Gray,
            UiLanguage::ZhCn,
        )
        .expect("localized production overlay");
        let text = buffer_text(&buffer, area);

        assert!(text.contains("图片 #9"));
        assert!(text.contains("格式：JPEG"));
        assert!(text.contains("尺寸：321 x 654"));
        assert!(text.contains("预览不可用"));
        assert!(text.contains("路径：/tmp/AUTHORITY-RAW.jpg"));
        assert!(!text.contains("Preview unavailable"));
    }

    #[test]
    fn failed_preview_uses_stable_metadata_fallback_on_production_overlay_path() {
        let _guard = ProtocolGuard::set(GraphicsProtocol::Kitty);
        let mut image = preview(Some(Path::new("/tmp/photo.jpg")), false);
        image.mime_type = "image/jpeg".to_string();
        let area = Rect::new(10, 5, 80, 30);
        let mut buffer = Buffer::empty(area);

        let render = render_image_overlay(
            &mut buffer,
            area,
            &image,
            Color::Black,
            Color::White,
            Color::Gray,
            UiLanguage::EnUs,
        )
        .expect("metadata fallback overlay");
        let text = (area.y..area.bottom())
            .map(|y| {
                (area.x..area.right())
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_string()))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !render.pixels_active,
            "a failed preview must not attempt terminal pixel transmission"
        );
        assert!(render.image_placement.is_none());
        assert!(render.escapes.is_none());
        assert!(text.contains("Format: JPEG"));
        assert!(text.contains("Dimensions: 640 x 480"));
        assert!(text.contains("Preview unavailable"));
        assert!(text.contains("Path: /tmp/photo.jpg"));
        assert!(!text.contains("Loading..."));
        assert!(!text.contains("data:image"));
    }

    #[test]
    fn production_overlay_consumes_fixed_renderer_and_post_flush_owner() {
        let _fixed_protocol = crabcode_pager_render::terminal::image::set_protocol_for_test(
            crabcode_pager_render::terminal::image::GraphicsProtocol::Kitty,
        );
        reset_terminal_image_owner();
        let image = preview(Some(Path::new("/tmp/logo.png")), true);
        let area = Rect::new(10, 5, 60, 20);

        let mut first_buffer = Buffer::empty(area);
        let first = render_image_overlay(
            &mut first_buffer,
            area,
            &image,
            Color::Black,
            Color::White,
            Color::Gray,
            UiLanguage::EnUs,
        )
        .expect("fixed renderer overlay");
        assert!(first.pixels_active);
        let first = first.escapes.expect("fixed renderer post-flush");
        assert!(first.as_str().contains("a=t"));
        assert!(first.as_str().contains("a=p"));
        first.commit();

        let mut next_buffer = Buffer::empty(area);
        let next = render_image_overlay(
            &mut next_buffer,
            area,
            &image,
            Color::Black,
            Color::White,
            Color::Gray,
            UiLanguage::EnUs,
        )
        .expect("steady-state fixed renderer overlay")
        .escapes
        .expect("steady-state placement");
        assert!(next.as_str().contains("a=p"));
        assert!(
            !next.as_str().contains("a=t"),
            "the fixed PostFlush owner must suppress redundant retransmission"
        );
    }

    #[test]
    fn production_resize_recomputes_image_placement_without_retransmission() {
        let _fixed_protocol = crabcode_pager_render::terminal::image::set_protocol_for_test(
            crabcode_pager_render::terminal::image::GraphicsProtocol::Kitty,
        );
        reset_terminal_image_owner();
        let image = preview(None, true);

        let wide = Rect::new(0, 0, 80, 30);
        let mut wide_buffer = Buffer::empty(wide);
        let first = render_image_overlay(
            &mut wide_buffer,
            wide,
            &image,
            Color::Black,
            Color::White,
            Color::Gray,
            UiLanguage::EnUs,
        )
        .expect("wide production image overlay")
        .escapes
        .expect("wide image placement");
        let first_escape = first.as_str().to_owned();
        assert!(first_escape.contains("a=t"));
        first.commit();

        let narrow = Rect::new(0, 0, 40, 15);
        let mut narrow_buffer = Buffer::empty(narrow);
        let resized = render_image_overlay(
            &mut narrow_buffer,
            narrow,
            &image,
            Color::Black,
            Color::White,
            Color::Gray,
            UiLanguage::EnUs,
        )
        .expect("narrow production image overlay")
        .escapes
        .expect("narrow image placement");
        assert!(resized.as_str().contains("a=p"));
        assert!(
            !resized.as_str().contains("a=t"),
            "resize must place the committed image at fresh geometry without retransmitting bytes"
        );
        assert_ne!(
            resized.as_str(),
            first_escape,
            "the production fixed renderer must not reuse stale wide-area placement"
        );
    }

    #[test]
    fn production_frame_writer_commits_fixed_post_flush_after_frame_acceptance() {
        let _fixed_protocol = crabcode_pager_render::terminal::image::set_protocol_for_test(
            crabcode_pager_render::terminal::image::GraphicsProtocol::Kitty,
        );
        reset_terminal_image_owner();
        let image = preview(Some(Path::new("/tmp/logo.png")), true);
        let area = Rect::new(10, 5, 60, 20);
        let mut first_buffer = Buffer::empty(area);
        let first = render_image_overlay(
            &mut first_buffer,
            area,
            &image,
            Color::Black,
            Color::White,
            Color::Gray,
            UiLanguage::EnUs,
        )
        .expect("fixed renderer overlay")
        .escapes
        .expect("fixed renderer post-flush");
        assert!(first.as_str().contains("a=t"));

        let (mut writer, receiver) = crate::terminal_writer::in_memory_frame_writer();
        writer
            .begin_synchronized_frame()
            .expect("begin synchronized frame");
        writer.write_all(b"cell-diff").expect("stage cell diff");
        crate::terminal_writer::stage_image_post_flush(Some(first));
        assert!(
            writer
                .synchronized_frame_has_content()
                .expect("stage fixed PostFlush")
        );

        let mut before_accept_buffer = Buffer::empty(area);
        let before_accept = render_image_overlay(
            &mut before_accept_buffer,
            area,
            &image,
            Color::Black,
            Color::White,
            Color::Gray,
            UiLanguage::EnUs,
        )
        .expect("uncommitted retry overlay")
        .escapes
        .expect("uncommitted retry escape");
        assert!(
            before_accept.as_str().contains("a=t"),
            "staging alone must not claim terminal ownership"
        );

        writer
            .finish_synchronized_frame()
            .expect("accept synchronized frame");
        let accepted = receiver.recv().expect("accepted frame payload");
        assert!(
            accepted.windows(3).any(|window| window == b"a=t"),
            "the accepted frame must contain the fixed image transmission"
        );

        let mut after_accept_buffer = Buffer::empty(area);
        let after_accept = render_image_overlay(
            &mut after_accept_buffer,
            area,
            &image,
            Color::Black,
            Color::White,
            Color::Gray,
            UiLanguage::EnUs,
        )
        .expect("committed steady-state overlay")
        .escapes
        .expect("committed placement escape");
        assert!(after_accept.as_str().contains("a=p"));
        assert!(
            !after_accept.as_str().contains("a=t"),
            "the accepted fixed PostFlush must commit ownership on the production frame path"
        );
    }

    #[test]
    fn protocol_matrix_and_tmux_windows_gates_match_fixed_capabilities() {
        for (brand, expected) in [
            (TerminalName::Kitty, GraphicsProtocol::Kitty),
            (TerminalName::Ghostty, GraphicsProtocol::Kitty),
            (TerminalName::WezTerm, GraphicsProtocol::Kitty),
            (TerminalName::WarpTerminal, GraphicsProtocol::Kitty),
            (TerminalName::Iterm2, GraphicsProtocol::None),
            (TerminalName::Unknown, GraphicsProtocol::None),
        ] {
            assert_eq!(protocol_for_brand(brand, false), expected);
            assert_eq!(protocol_for_brand(brand, true), GraphicsProtocol::None);
        }
        let mut tmux = crate::terminal_capabilities::terminal_context_from_env_for_test(&[(
            "TERM_PROGRAM",
            "kitty",
        )]);
        tmux.multiplexer = MultiplexerKind::Tmux;
        assert_eq!(protocol_for_context(&tmux, false), GraphicsProtocol::None);
    }

    #[test]
    fn steady_state_and_changed_placement_are_place_only_without_retransmission() {
        let _guard = ProtocolGuard::set(GraphicsProtocol::Kitty);
        let image = preview(None, true);
        let placement = ImagePlacement {
            cols: 20,
            rows: 10,
            x: 3,
            y: 4,
        };
        let first = static_image_escapes(
            GraphicsProtocol::Kitty,
            image.pixels.as_deref().unwrap(),
            placement,
            image.identity,
        )
        .expect("first escape");
        assert!(first.as_str().contains("a=t"));
        first.commit();
        let steady = static_image_escapes(
            GraphicsProtocol::Kitty,
            image.pixels.as_deref().unwrap(),
            placement,
            image.identity,
        )
        .expect("steady placement");
        assert!(steady.as_str().contains("a=p"));
        assert!(!steady.as_str().contains("a=t"));
        let moved = static_image_escapes(
            GraphicsProtocol::Kitty,
            image.pixels.as_deref().unwrap(),
            ImagePlacement { x: 4, ..placement },
            image.identity,
        )
        .expect("moved placement");
        assert!(moved.as_str().contains("a=p"));
        assert!(!moved.as_str().contains("a=t"));
    }

    #[test]
    fn terminal_generation_reset_requires_a_cold_start_clear() {
        let _guard = ProtocolGuard::set(GraphicsProtocol::Kitty);
        reset_terminal_image_owner();
        let clear = clear_committed_image().expect("unknown Kitty state must be cleared");
        assert!(clear.as_str().contains("a=d"));
        clear.commit();
        assert!(
            clear_committed_image().is_none(),
            "a committed clear makes the terminal state known"
        );
    }

    #[test]
    fn iterm_encoder_retains_requested_geometry_but_is_not_auto_selected() {
        let escape = render_iterm2_image(&[0; 10], 30, 15);
        assert!(escape.starts_with("\x1b]1337;File="));
        assert!(escape.contains("width=30cells"));
        assert!(escape.contains("height=15cells"));
        assert!(escape.contains("preserveAspectRatio=1"));
        assert_eq!(
            protocol_for_brand(TerminalName::Iterm2, false),
            GraphicsProtocol::None
        );
        let first = static_image_escapes(
            GraphicsProtocol::ITerm2,
            &[0; 10],
            ImagePlacement {
                cols: 30,
                rows: 15,
                x: 2,
                y: 3,
            },
            next_image_identity(),
        )
        .expect("low-level iTerm2 protocol remains representable");
        assert!(first.as_str().contains("1337;File="));
    }
}

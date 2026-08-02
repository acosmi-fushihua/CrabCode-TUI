use std::fs::{File, OpenOptions};
use std::io::{Cursor, Read as _};
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::codecs::webp::WebPEncoder;
use image::imageops::FilterType;
use image::{ColorType, DynamicImage, ImageEncoder, ImageFormat, ImageReader};
use thiserror::Error;

pub(crate) const MAX_COMPOSER_IMAGE_RAW_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const MAX_COMPOSER_IMAGE_ENCODED_BYTES: usize = 3_932_160;
pub(crate) const MAX_COMPOSER_IMAGE_PIXELS: u64 = 25_000_000;
pub(crate) const MAX_COMPOSER_IMAGE_SOURCE_DIMENSION: u32 = 10_000;
pub(crate) const MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION: u32 = 2_000;
pub(crate) const MAX_COMPOSER_IMAGES_PER_PASTE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LoadedComposerImage {
    pub(crate) filename: String,
    pub(crate) data_url: String,
    pub(crate) mime: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) encoded_bytes: usize,
    pub(crate) preview_identity: u64,
    pub(crate) terminal_preview: Option<Arc<[u8]>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ComposerImagePastePlan {
    pub(crate) image_paths: Vec<String>,
    pub(crate) non_image_text: String,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub(crate) enum ComposerImageError {
    #[error("the pasted value is not a supported image path")]
    UnsupportedPath,
    #[error("the image path must be an explicit absolute path")]
    InvalidPath,
    #[error("a symbolic link or reparse point in the final path component is not accepted")]
    LinkedPath,
    #[error("the image path is not a regular file")]
    NotRegularFile,
    #[error("the source image exceeds the {MAX_COMPOSER_IMAGE_RAW_BYTES} byte read limit")]
    SourceTooLarge,
    #[error("the source image could not be read")]
    ReadFailed,
    #[error("the source image format or dimensions could not be decoded")]
    DecodeFailed,
    #[error(
        "the source image exceeds the {MAX_COMPOSER_IMAGE_SOURCE_DIMENSION}px / {MAX_COMPOSER_IMAGE_PIXELS} pixel decode limit"
    )]
    DimensionsTooLarge,
    #[error(
        "the processed image exceeds the {MAX_COMPOSER_IMAGE_ENCODED_BYTES} byte API payload limit"
    )]
    EncodedTooLarge,
}

/// Recognizes the same user action as the legacy terminal composer: pasting
/// one or more image file paths. Mixed ordinary text is deliberately left as
/// text; the Rust TUI never guesses that arbitrary prompt content is a file.
#[cfg(test)]
pub(crate) fn parse_image_path_paste(text: &str) -> Option<Vec<String>> {
    let plan = classify_image_path_paste(text)?;
    plan.non_image_text.is_empty().then_some(plan.image_paths)
}

/// Mirrors the legacy bracketed-paste classifier: supported image-path lines
/// are loaded as a batch, while lines that do not have a supported image
/// extension remain ordinary pasted text. A path that looks like an image but
/// later fails to load remains part of the image batch; the caller can restore
/// the complete original paste when no image in the batch succeeds.
pub(crate) fn classify_image_path_paste(text: &str) -> Option<ComposerImagePastePlan> {
    let candidates = split_path_candidates(text);
    if candidates.is_empty() {
        return None;
    }
    let mut image_paths = Vec::with_capacity(candidates.len());
    let mut non_image_lines = Vec::new();
    for candidate in candidates {
        if let Some(path) = clean_image_path(&candidate) {
            if image_paths.len() < MAX_COMPOSER_IMAGES_PER_PASTE {
                image_paths.push(path);
            } else {
                // Preserve overflow as ordinary composer text. The attachment
                // cap limits decode/API work without silently dropping any
                // portion of the user's paste.
                non_image_lines.push(candidate);
            }
        } else {
            non_image_lines.push(candidate);
        }
    }
    (!image_paths.is_empty()).then_some(ComposerImagePastePlan {
        image_paths,
        non_image_text: non_image_lines.join("\n"),
    })
}

pub(crate) fn load_composer_image(
    _workspace_cwd: &Path,
    pasted_path: &str,
) -> Result<LoadedComposerImage, ComposerImageError> {
    let cleaned = clean_image_path(pasted_path).ok_or(ComposerImageError::UnsupportedPath)?;
    let input_path = Path::new(&cleaned);
    if !input_path.is_absolute() {
        return Err(ComposerImageError::InvalidPath);
    }
    let resolved = input_path.to_path_buf();
    if resolved.as_os_str().is_empty() {
        return Err(ComposerImageError::InvalidPath);
    }

    let mut file = open_regular_file_without_links(&resolved)?;
    let metadata = file
        .metadata()
        .map_err(|_| ComposerImageError::ReadFailed)?;
    if !metadata.is_file() {
        return Err(ComposerImageError::NotRegularFile);
    }
    if metadata.len() > MAX_COMPOSER_IMAGE_RAW_BYTES {
        return Err(ComposerImageError::SourceTooLarge);
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_COMPOSER_IMAGE_RAW_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| ComposerImageError::ReadFailed)?;
    if bytes.len() as u64 > MAX_COMPOSER_IMAGE_RAW_BYTES {
        return Err(ComposerImageError::SourceTooLarge);
    }
    if bytes.is_empty() {
        return Err(ComposerImageError::DecodeFailed);
    }

    let reader = ImageReader::new(Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|_| ComposerImageError::DecodeFailed)?;
    let source_format = reader
        .format()
        .filter(|format| {
            matches!(
                format,
                ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::Gif | ImageFormat::WebP
            )
        })
        .ok_or(ComposerImageError::DecodeFailed)?;
    let (source_width, source_height) = reader
        .into_dimensions()
        .map_err(|_| ComposerImageError::DecodeFailed)?;
    let source_pixels = u64::from(source_width).saturating_mul(u64::from(source_height));
    if source_width > MAX_COMPOSER_IMAGE_SOURCE_DIMENSION
        || source_height > MAX_COMPOSER_IMAGE_SOURCE_DIMENSION
        || source_pixels > MAX_COMPOSER_IMAGE_PIXELS
    {
        return Err(ComposerImageError::DimensionsTooLarge);
    }

    let encoded = process_image_bytes(&bytes, source_format, source_width, source_height)?;
    if encoded.bytes.len() > MAX_COMPOSER_IMAGE_ENCODED_BYTES {
        return Err(ComposerImageError::EncodedTooLarge);
    }
    let filename = resolved
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("pasted-image")
        .to_string();
    let encoded_bytes = encoded.bytes.len();
    let terminal_preview = crate::crabcode_image_overlay::prepare_terminal_preview(&encoded.bytes);
    let mime = encoded.mime.to_string();
    let width = encoded.width;
    let height = encoded.height;
    Ok(LoadedComposerImage {
        filename,
        data_url: format!(
            "data:{};base64,{}",
            encoded.mime,
            BASE64_STANDARD.encode(encoded.bytes)
        ),
        mime,
        width,
        height,
        encoded_bytes,
        preview_identity: crate::crabcode_image_overlay::next_image_identity(),
        terminal_preview,
    })
}

struct ProcessedImage {
    bytes: Vec<u8>,
    mime: &'static str,
    width: u32,
    height: u32,
}

fn process_image_bytes(
    bytes: &[u8],
    source_format: ImageFormat,
    source_width: u32,
    source_height: u32,
) -> Result<ProcessedImage, ComposerImageError> {
    if bytes.len() <= MAX_COMPOSER_IMAGE_ENCODED_BYTES
        && source_width <= MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION
        && source_height <= MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION
    {
        return Ok(ProcessedImage {
            bytes: bytes.to_vec(),
            mime: format_mime(source_format),
            width: source_width,
            height: source_height,
        });
    }

    let decoded = image::load_from_memory(bytes).map_err(|_| ComposerImageError::DecodeFailed)?;
    let resized = if source_width > MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION
        || source_height > MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION
    {
        decoded.resize(
            MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION,
            MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION,
            FilterType::Lanczos3,
        )
    } else {
        decoded
    };

    if source_format == ImageFormat::Png {
        let png = encode_png(&resized)?;
        if png.len() <= MAX_COMPOSER_IMAGE_ENCODED_BYTES {
            return Ok(processed(png, "image/png", &resized));
        }
    } else if source_format == ImageFormat::WebP {
        let webp = encode_webp(&resized)?;
        if webp.len() <= MAX_COMPOSER_IMAGE_ENCODED_BYTES {
            return Ok(processed(webp, "image/webp", &resized));
        }
    }

    for quality in [80, 60, 40, 20] {
        let jpeg = encode_jpeg(&resized, quality)?;
        if jpeg.len() <= MAX_COMPOSER_IMAGE_ENCODED_BYTES {
            return Ok(processed(jpeg, "image/jpeg", &resized));
        }
    }

    // This mirrors the legacy final fallback: once format compression at the
    // 2000px display bound is insufficient, reduce the longest edge to 1000px
    // and use the lowest established JPEG quality. Continue halving only to
    // enforce the existing API byte limit rather than returning an invalid
    // request.
    let mut fallback = resized.resize(1_000, 1_000, FilterType::Lanczos3);
    loop {
        let jpeg = encode_jpeg(&fallback, 20)?;
        if jpeg.len() <= MAX_COMPOSER_IMAGE_ENCODED_BYTES {
            return Ok(processed(jpeg, "image/jpeg", &fallback));
        }
        if fallback.width() <= 64 && fallback.height() <= 64 {
            return Err(ComposerImageError::EncodedTooLarge);
        }
        fallback = fallback.resize(
            (fallback.width() / 2).max(1),
            (fallback.height() / 2).max(1),
            FilterType::Lanczos3,
        );
    }
}

fn processed(bytes: Vec<u8>, mime: &'static str, image: &DynamicImage) -> ProcessedImage {
    ProcessedImage {
        bytes,
        mime,
        width: image.width(),
        height: image.height(),
    }
}

fn encode_png(image: &DynamicImage) -> Result<Vec<u8>, ComposerImageError> {
    let rgba = image.to_rgba8();
    let mut bytes = Vec::new();
    PngEncoder::new(&mut bytes)
        .write_image(
            rgba.as_raw(),
            image.width(),
            image.height(),
            ColorType::Rgba8.into(),
        )
        .map_err(|_| ComposerImageError::DecodeFailed)?;
    Ok(bytes)
}

fn encode_webp(image: &DynamicImage) -> Result<Vec<u8>, ComposerImageError> {
    let rgba = image.to_rgba8();
    let mut bytes = Vec::new();
    WebPEncoder::new_lossless(&mut bytes)
        .write_image(
            rgba.as_raw(),
            image.width(),
            image.height(),
            ColorType::Rgba8.into(),
        )
        .map_err(|_| ComposerImageError::DecodeFailed)?;
    Ok(bytes)
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, ComposerImageError> {
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, quality)
        .encode_image(image)
        .map_err(|_| ComposerImageError::DecodeFailed)?;
    Ok(bytes)
}

fn format_mime(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Png => "image/png",
        _ => unreachable!("source format was validated before image processing"),
    }
}

fn split_path_candidates(text: &str) -> Vec<String> {
    text.lines()
        .flat_map(split_space_separated_absolute_paths)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .collect()
}

fn split_space_separated_absolute_paths(line: &str) -> Vec<&str> {
    let mut starts = vec![0usize];
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b' ' || index + 1 >= bytes.len() {
            continue;
        }
        let next = &bytes[index + 1..];
        let unix_absolute = next.first() == Some(&b'/');
        let windows_absolute = next.len() >= 3
            && next[0].is_ascii_alphabetic()
            && next[1] == b':'
            && matches!(next[2], b'\\' | b'/');
        if unix_absolute || windows_absolute {
            starts.push(index + 1);
        }
    }
    starts.push(line.len());
    starts
        .windows(2)
        .map(|window| {
            let start = window[0];
            let mut end = window[1];
            if end > start && line.as_bytes().get(end - 1) == Some(&b' ') {
                end -= 1;
            }
            &line[start..end]
        })
        .collect()
}

fn clean_image_path(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let without_quotes = if trimmed.len() >= 2
        && ((trimmed.starts_with('"') && trimmed.ends_with('"'))
            || (trimmed.starts_with('\'') && trimmed.ends_with('\'')))
    {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    };

    #[cfg(windows)]
    let cleaned = without_quotes.to_string();
    #[cfg(not(windows))]
    let cleaned = strip_shell_backslash_escapes(without_quotes);

    let extension = Path::new(&cleaned)
        .extension()
        .and_then(|extension| extension.to_str())?
        .to_ascii_lowercase();
    matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp").then_some(cleaned)
}

#[cfg(not(windows))]
fn strip_shell_backslash_escapes(value: &str) -> String {
    let mut cleaned = String::with_capacity(value.len());
    let mut characters = value.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            if let Some(escaped) = characters.next() {
                cleaned.push(escaped);
            } else {
                cleaned.push(character);
            }
        } else {
            cleaned.push(character);
        }
    }
    cleaned
}

#[cfg(unix)]
fn open_regular_file_without_links(path: &Path) -> Result<File, ComposerImageError> {
    use std::os::unix::fs::OpenOptionsExt as _;

    let metadata = std::fs::symlink_metadata(path).map_err(|_| ComposerImageError::ReadFailed)?;
    if metadata.file_type().is_symlink() {
        return Err(ComposerImageError::LinkedPath);
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| ComposerImageError::ReadFailed)?;
    let opened = file
        .metadata()
        .map_err(|_| ComposerImageError::ReadFailed)?;
    if !opened.is_file() {
        return Err(ComposerImageError::NotRegularFile);
    }
    Ok(file)
}

#[cfg(windows)]
fn open_regular_file_without_links(path: &Path) -> Result<File, ComposerImageError> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
        .map_err(|_| ComposerImageError::ReadFailed)?;
    let metadata = file
        .metadata()
        .map_err(|_| ComposerImageError::ReadFailed)?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(ComposerImageError::LinkedPath);
    }
    if !metadata.is_file() {
        return Err(ComposerImageError::NotRegularFile);
    }
    Ok(file)
}

#[cfg(not(any(unix, windows)))]
fn open_regular_file_without_links(path: &Path) -> Result<File, ComposerImageError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| ComposerImageError::ReadFailed)?;
    if metadata.file_type().is_symlink() {
        return Err(ComposerImageError::LinkedPath);
    }
    let file = File::open(path).map_err(|_| ComposerImageError::ReadFailed)?;
    if !file
        .metadata()
        .map_err(|_| ComposerImageError::ReadFailed)?
        .is_file()
    {
        return Err(ComposerImageError::NotRegularFile);
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    struct TempFixture {
        root: PathBuf,
    }

    impl TempFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "crabcode-tui-composer-image-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir_all(&root).expect("create image fixture directory");
            Self { root }
        }
    }

    impl Drop for TempFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn parser_accepts_only_an_entire_supported_path_paste() {
        assert_eq!(
            parse_image_path_paste(r#""/tmp/a file.PNG""#),
            Some(vec!["/tmp/a file.PNG".to_string()])
        );
        assert_eq!(
            parse_image_path_paste("/tmp/a.png /tmp/b.webp\n/tmp/c.jpeg"),
            Some(vec![
                "/tmp/a.png".to_string(),
                "/tmp/b.webp".to_string(),
                "/tmp/c.jpeg".to_string()
            ])
        );
        assert_eq!(
            parse_image_path_paste(r"/tmp/a\ file.png"),
            Some(vec!["/tmp/a file.png".to_string()])
        );
        assert_eq!(parse_image_path_paste("explain /tmp/a.png"), None);
        assert_eq!(parse_image_path_paste("/tmp/a.bmp"), None);
    }

    #[test]
    fn mixed_paste_keeps_non_image_lines_separate_from_the_image_batch() {
        assert_eq!(
            classify_image_path_paste("/tmp/a.png\nexplain this\n/tmp/b.webp"),
            Some(ComposerImagePastePlan {
                image_paths: vec!["/tmp/a.png".to_string(), "/tmp/b.webp".to_string()],
                non_image_text: "explain this".to_string(),
            })
        );
        assert_eq!(parse_image_path_paste("/tmp/a.png\nexplain this"), None);
    }

    #[test]
    fn attachment_batch_cap_preserves_every_overflow_path_as_text() {
        let pasted = (0..(MAX_COMPOSER_IMAGES_PER_PASTE + 2))
            .map(|index| format!("/tmp/image-{index}.png"))
            .collect::<Vec<_>>()
            .join("\n");
        let plan = classify_image_path_paste(&pasted).expect("image paste plan");
        assert_eq!(plan.image_paths.len(), MAX_COMPOSER_IMAGES_PER_PASTE);
        assert_eq!(
            plan.non_image_text,
            "/tmp/image-100.png\n/tmp/image-101.png"
        );
        assert_eq!(
            plan.image_paths.first().map(String::as_str),
            Some("/tmp/image-0.png")
        );
        assert_eq!(
            plan.image_paths.last().map(String::as_str),
            Some("/tmp/image-99.png")
        );
    }

    #[test]
    fn loader_reads_and_reencodes_a_real_regular_image() {
        let fixture = TempFixture::new();
        let image_path = fixture.root.join("one.png");
        RgbaImage::from_pixel(1, 1, Rgba([1, 2, 3, 255]))
            .save(&image_path)
            .expect("write image fixture");

        let loaded = load_composer_image(&fixture.root, &image_path.to_string_lossy())
            .expect("load regular image fixture");
        assert_eq!(loaded.filename, "one.png");
        assert_eq!(loaded.mime, "image/png");
        assert_eq!((loaded.width, loaded.height), (1, 1));
        assert!(loaded.data_url.starts_with("data:image/png;base64,"));
        assert!(loaded.encoded_bytes > 0);
    }

    #[test]
    fn loader_resizes_to_the_legacy_two_thousand_pixel_display_bound() {
        let fixture = TempFixture::new();
        let image_path = fixture.root.join("wide.png");
        RgbaImage::from_pixel(2_100, 100, Rgba([1, 2, 3, 255]))
            .save(&image_path)
            .expect("write wide image fixture");

        let loaded = load_composer_image(&fixture.root, &image_path.to_string_lossy())
            .expect("resize wide image fixture");
        assert_eq!(loaded.width, MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION);
        assert!(loaded.height <= MAX_COMPOSER_IMAGE_DISPLAY_DIMENSION);
        assert!(loaded.encoded_bytes <= MAX_COMPOSER_IMAGE_ENCODED_BYTES);
    }

    #[test]
    fn oversize_valid_image_is_compressed_instead_of_only_rejected() {
        let fixture = TempFixture::new();
        let image_path = fixture.root.join("noise.png");
        let image = RgbaImage::from_fn(1_100, 1_100, |x, y| {
            let mixed = x
                .wrapping_mul(1_664_525)
                .wrapping_add(y.wrapping_mul(1_013_904_223));
            Rgba([mixed as u8, (mixed >> 8) as u8, (mixed >> 16) as u8, 255])
        });
        image.save(&image_path).expect("write noisy PNG fixture");
        assert!(
            image_path.metadata().expect("noise metadata").len()
                > MAX_COMPOSER_IMAGE_ENCODED_BYTES as u64
        );

        let loaded = load_composer_image(&fixture.root, &image_path.to_string_lossy())
            .expect("compress noisy image fixture");
        assert!(loaded.encoded_bytes <= MAX_COMPOSER_IMAGE_ENCODED_BYTES);
        assert!(loaded.data_url.starts_with("data:image/jpeg;base64,"));
    }

    #[test]
    fn loader_rejects_a_source_over_the_explicit_read_limit() {
        let fixture = TempFixture::new();
        let image_path = fixture.root.join("oversized.png");
        let file = File::create(&image_path).expect("create sparse oversized fixture");
        file.set_len(MAX_COMPOSER_IMAGE_RAW_BYTES + 1)
            .expect("size sparse oversized fixture");
        assert_eq!(
            load_composer_image(&fixture.root, &image_path.to_string_lossy()),
            Err(ComposerImageError::SourceTooLarge)
        );
    }

    #[cfg(unix)]
    #[test]
    fn loader_never_follows_a_symbolic_link() {
        use std::os::unix::fs::symlink;

        let fixture = TempFixture::new();
        let target = fixture.root.join("target.png");
        RgbaImage::from_pixel(1, 1, Rgba([1, 2, 3, 255]))
            .save(&target)
            .expect("write image target");
        symlink(&target, fixture.root.join("linked.png")).expect("create image symlink");
        assert_eq!(
            load_composer_image(
                &fixture.root,
                &fixture.root.join("linked.png").to_string_lossy()
            ),
            Err(ComposerImageError::LinkedPath)
        );
    }

    #[test]
    fn relative_image_name_is_not_guessed_as_a_workspace_file() {
        let fixture = TempFixture::new();
        let image_path = fixture.root.join("clipboard-name.png");
        RgbaImage::from_pixel(1, 1, Rgba([1, 2, 3, 255]))
            .save(&image_path)
            .expect("write same-name workspace fixture");

        assert_eq!(
            load_composer_image(&fixture.root, "clipboard-name.png"),
            Err(ComposerImageError::InvalidPath)
        );
    }
}

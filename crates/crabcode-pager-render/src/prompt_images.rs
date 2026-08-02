//! Narrow CrabCode image-record adapter consumed by the fixed renderer.
//!
//! Fixed upstream whole-file SHA-256:
//! `4b21a018499bac42d0445cc03cc04d842b33f3fa33f0dcba1fda5e1979fc79f4`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use crabcode_ratatui_textarea::ElementId;

const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif"];
const VIDEO_EXTENSIONS: &[&str] = &["mp4", "webm", "mov", "avi", "mkv"];
const MARKDOWN_IMAGE_REF_PATTERN: &str = r"!\[([^\]]*)\]\(([^)\s]+)\)";

fn strip_verbatim_prefix(path: &std::path::Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

fn decode_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let image = image::load_from_memory(bytes).ok()?;
    Some((image.width(), image.height()))
}

/// Metadata for inline media rendering in scrollback.
///
/// Fixed-source lineage:
/// `crates/codegen/xai-grok-pager-render/src/prompt_images.rs` at commit
/// `a5727c5960452e7527a154b25cb5bf00cda0545e`.
/// This renderer-owned value type carries no backend authority.
#[derive(Debug, Clone)]
pub struct InlineMediaInfo {
    pub path: PathBuf,
    pub width: u32,
    pub height: u32,
    pub is_video: bool,
    pub alt_text: String,
}

/// Validated image reference carried by a scrollback block.
///
/// Construction performs renderer-local validation of a path already present
/// in transcript text: supported extension, regular-file existence, image
/// decode, and dimensions. It does not discover files, fetch remote content,
/// dispatch a request, or define a backend/protocol field.
#[derive(Debug, Clone)]
pub struct ScrollbackImageRef {
    pub path: PathBuf,
    pub dimensions: Option<(u32, u32)>,
    pub alt_text: String,
}

impl ScrollbackImageRef {
    /// Construct from a file path, returning `None` unless it is a supported,
    /// existing, decodable image.
    pub fn from_path(path: impl Into<PathBuf>) -> Option<Self> {
        Self::from_path_with_alt(path, String::new())
    }

    /// Construct a validated image reference with Markdown alt text.
    pub fn from_path_with_alt(path: impl Into<PathBuf>, alt_text: String) -> Option<Self> {
        let path = strip_verbatim_prefix(&path.into());
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        if !IMAGE_EXTENSIONS.contains(&extension.as_str()) || !path.is_file() {
            return None;
        }
        let bytes = std::fs::read(&path).ok()?;
        let dimensions = decode_image_dimensions(&bytes)?;
        Some(Self {
            path,
            dimensions: Some(dimensions),
            alt_text,
        })
    }
}

/// Whether text consists only of resolved Markdown media references.
#[must_use]
pub fn is_media_only_markdown(text: &str, resolved_ref_count: usize) -> bool {
    use std::sync::LazyLock;

    if resolved_ref_count == 0 {
        return false;
    }

    static ONLY_MARKDOWN_REFS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(&format!(r"^\s*(?:{MARKDOWN_IMAGE_REF_PATTERN}\s*)+$"))
            .expect("static media-only Markdown regex must compile")
    });
    static MARKDOWN_REF: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(MARKDOWN_IMAGE_REF_PATTERN)
            .expect("static Markdown media reference regex must compile")
    });

    if !ONLY_MARKDOWN_REFS.is_match(text) {
        return false;
    }

    let unique_ref_count = MARKDOWN_REF
        .captures_iter(text)
        .filter_map(|capture| capture.get(2).map(|value| value.as_str()))
        .collect::<std::collections::HashSet<_>>()
        .len();
    unique_ref_count == resolved_ref_count
}

/// Extract validated image references from Markdown and bare absolute paths.
#[must_use]
pub fn extract_image_refs(text: &str) -> Vec<ScrollbackImageRef> {
    use std::sync::LazyLock;

    static MARKDOWN_REF: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(MARKDOWN_IMAGE_REF_PATTERN)
            .expect("static Markdown image reference regex must compile")
    });
    static ABSOLUTE_PATH: LazyLock<regex::Regex> = LazyLock::new(|| {
        let extensions = IMAGE_EXTENSIONS.join("|");
        regex::Regex::new(&format!(
            r"(?:^|[\s,])((?:/|[A-Za-z]:[\\/]|\\\\)[^\s,]+\.(?:{extensions}))(?:[\s,.(]|$)"
        ))
        .expect("static absolute image path regex must compile")
    });

    let mut references = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for capture in MARKDOWN_REF.captures_iter(text) {
        if let Some(path_match) = capture.get(2) {
            let path = path_match.as_str();
            let alt_text = capture
                .get(1)
                .map(|alt| alt.as_str().to_owned())
                .unwrap_or_default();
            if seen.insert(path.to_owned())
                && let Some(reference) = ScrollbackImageRef::from_path_with_alt(path, alt_text)
            {
                references.push(reference);
            }
        }
    }

    for capture in ABSOLUTE_PATH.captures_iter(text) {
        if let Some(path_match) = capture.get(1) {
            let path = path_match.as_str();
            if seen.insert(path.to_owned())
                && let Some(reference) = ScrollbackImageRef::from_path(path)
            {
                references.push(reference);
            }
        }
    }

    references
}

/// Validated video reference carried by a scrollback block.
///
/// Like [`ScrollbackImageRef`], this is presentation data only.
#[derive(Debug, Clone)]
pub struct ScrollbackVideoRef {
    pub path: PathBuf,
    pub alt_text: String,
}

impl ScrollbackVideoRef {
    /// Construct a validated video reference from an existing file.
    pub fn from_path(path: impl Into<PathBuf>) -> Option<Self> {
        Self::from_path_with_alt(path, String::new())
    }

    /// Construct a validated video reference with Markdown alt text.
    pub fn from_path_with_alt(path: impl Into<PathBuf>, alt_text: String) -> Option<Self> {
        let path = strip_verbatim_prefix(&path.into());
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        if !VIDEO_EXTENSIONS.contains(&extension.as_str()) || !path.is_file() {
            return None;
        }
        Some(Self { path, alt_text })
    }
}

/// Extract validated video references from Markdown and bare absolute paths.
#[must_use]
pub fn extract_video_refs(text: &str) -> Vec<ScrollbackVideoRef> {
    use std::sync::LazyLock;

    static MARKDOWN_REF: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(MARKDOWN_IMAGE_REF_PATTERN)
            .expect("static Markdown video reference regex must compile")
    });
    static ABSOLUTE_PATH: LazyLock<regex::Regex> = LazyLock::new(|| {
        let extensions = VIDEO_EXTENSIONS.join("|");
        regex::Regex::new(&format!(
            r"(?:^|[\s,])((?:/|[A-Za-z]:[\\/]|\\\\)[^\s,]+\.(?:{extensions}))(?:[\s,.(]|$)"
        ))
        .expect("static absolute video path regex must compile")
    });

    let mut references = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for capture in MARKDOWN_REF.captures_iter(text) {
        if let Some(path_match) = capture.get(2) {
            let path = path_match.as_str();
            let alt_text = capture
                .get(1)
                .map(|alt| alt.as_str().to_owned())
                .unwrap_or_default();
            if seen.insert(path.to_owned())
                && let Some(reference) = ScrollbackVideoRef::from_path_with_alt(path, alt_text)
            {
                references.push(reference);
            }
        }
    }

    for capture in ABSOLUTE_PATH.captures_iter(text) {
        if let Some(path_match) = capture.get(1) {
            let path = path_match.as_str();
            if seen.insert(path.to_owned())
                && let Some(reference) = ScrollbackVideoRef::from_path(path)
            {
                references.push(reference);
            }
        }
    }

    references
}

/// Renderer-facing image record. Product loading and provenance remain owned
/// by `crabcode-tui`; this type contains only the fields read by the fixed
/// image-overlay source.
#[derive(Debug, Clone)]
pub struct PastedImage {
    pub element_id: ElementId,
    pub display_number: usize,
    pub mime_type: String,
    pub dimensions: Option<(u32, u32)>,
    pub byte_len: usize,
    pub encoded_bytes: Option<Arc<[u8]>>,
    pub source_path: Option<PathBuf>,
    pub staged_temp_path: Option<PathBuf>,
    pub session_image_path: Option<PathBuf>,
    pub preview: PromptImagePreview,
}

impl PastedImage {
    pub fn preview_dimensions(&self) -> Option<(u32, u32)> {
        self.dimensions.or_else(|| self.preview.dimensions())
    }
}

#[derive(Debug, Clone)]
pub struct PromptImagePreview {
    identity: u64,
    result: Arc<OnceLock<PromptImagePreviewResult>>,
}

#[derive(Debug)]
enum PromptImagePreviewResult {
    Ready {
        bytes: Arc<[u8]>,
        dimensions: (u32, u32),
    },
    Failed,
}

impl Default for PromptImagePreview {
    fn default() -> Self {
        Self::pending(crate::terminal::overlay::next_owner_id())
    }
}

impl PromptImagePreview {
    pub fn pending(identity: u64) -> Self {
        Self {
            identity,
            result: Arc::new(OnceLock::new()),
        }
    }

    pub fn ready(identity: u64, bytes: Arc<[u8]>, dimensions: (u32, u32)) -> Self {
        let preview = Self::pending(identity);
        let _ = preview
            .result
            .set(PromptImagePreviewResult::Ready { bytes, dimensions });
        preview
    }

    pub fn failed(identity: u64) -> Self {
        let preview = Self::pending(identity);
        preview.mark_failed();
        preview
    }

    #[cfg(test)]
    pub fn ready_for_test(bytes: Vec<u8>, dimensions: (u32, u32)) -> Self {
        Self::ready(
            crate::terminal::overlay::next_owner_id(),
            Arc::from(bytes),
            dimensions,
        )
    }

    pub fn identity(&self) -> u64 {
        self.identity
    }

    pub fn is_pending(&self) -> bool {
        self.result.get().is_none()
    }

    pub fn is_failed(&self) -> bool {
        matches!(self.result.get(), Some(PromptImagePreviewResult::Failed))
    }

    pub fn prepared(&self) -> Option<(&[u8], (u32, u32))> {
        match self.result.get()? {
            PromptImagePreviewResult::Ready { bytes, dimensions } => {
                Some((bytes.as_ref(), *dimensions))
            }
            PromptImagePreviewResult::Failed => None,
        }
    }

    pub fn dimensions(&self) -> Option<(u32, u32)> {
        match self.result.get()? {
            PromptImagePreviewResult::Ready { dimensions, .. } => Some(*dimensions),
            PromptImagePreviewResult::Failed => None,
        }
    }

    pub fn mark_failed(&self) {
        let _ = self.result.set(PromptImagePreviewResult::Failed);
    }
}

#[cfg(test)]
mod scrollback_reference_tests {
    use super::*;

    fn write_test_png(path: &std::path::Path, width: u32, height: u32) {
        image::DynamicImage::new_rgb8(width, height)
            .save(path)
            .expect("test PNG must be written");
    }

    #[test]
    fn image_reference_requires_a_decodable_supported_file() {
        let directory = tempfile::tempdir().expect("temp directory");
        let valid = directory.path().join("valid.png");
        let invalid = directory.path().join("invalid.png");
        write_test_png(&valid, 3, 2);
        std::fs::write(&invalid, b"not an image").expect("invalid fixture");

        let reference = ScrollbackImageRef::from_path_with_alt(&valid, "preview".to_owned())
            .expect("valid reference");
        assert_eq!(reference.path, valid);
        assert_eq!(reference.dimensions, Some((3, 2)));
        assert_eq!(reference.alt_text, "preview");
        assert!(ScrollbackImageRef::from_path(invalid).is_none());
        assert!(ScrollbackImageRef::from_path(directory.path().join("missing.png")).is_none());
    }

    #[test]
    fn image_extraction_preserves_alt_text_and_deduplicates_paths() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("diagram.png");
        write_test_png(&path, 1, 1);
        let text = format!("![diagram]({p}) and ![duplicate]({p})", p = path.display());

        let references = extract_image_refs(&text);
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].path, path);
        assert_eq!(references[0].alt_text, "diagram");
        assert!(is_media_only_markdown(
            &format!("![diagram]({})", path.display()),
            1,
        ));
        assert!(!is_media_only_markdown(&text, 1));
    }

    #[test]
    fn bare_absolute_image_path_is_detected() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("saved.webp");
        image::DynamicImage::new_rgb8(2, 2)
            .save(&path)
            .expect("test WebP must be written");
        let references = extract_image_refs(&format!("saved to {} (12 bytes)", path.display()));
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].path, path);
    }

    #[test]
    fn video_extraction_validates_extension_and_existing_file() {
        let directory = tempfile::tempdir().expect("temp directory");
        let path = directory.path().join("clip.mp4");
        std::fs::write(&path, b"fixture").expect("video fixture");
        let references = extract_video_refs(&format!("![result]({0}) and {0}", path.display()));
        assert_eq!(references.len(), 1);
        assert_eq!(references[0].path, path);
        assert_eq!(references[0].alt_text, "result");
        assert!(extract_video_refs("![missing](/definitely/not/present/clip.mp4)").is_empty());
    }
}

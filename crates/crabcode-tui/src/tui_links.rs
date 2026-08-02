//! Visible semantic-link discovery and navigation for the terminal transcript.
//!
//! Navigation is deliberately scoped to the lines that were actually painted
//! in the latest frame. A link elsewhere in a long backend transcript is not
//! actionable until the user scrolls it into view.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use linkify::{LinkFinder, LinkKind};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::tui_app::UiLanguage;
use crate::tui_render::{byte_range_to_row_cols, sanitize_osc8_target};

const MAX_LINK_TARGET_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MermaidAffordanceAction {
    Open,
    CopyPath,
    CopySource,
}

impl MermaidAffordanceAction {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Open => "Open Mermaid image",
            Self::CopyPath => "Copy Mermaid image path",
            Self::CopySource => "Copy Mermaid source",
        }
    }
}

/// Semantic destination of a terminal link.
///
/// Files remain path-native until process handoff. Converting them to a
/// display string would lose non-UTF-8 bytes and would also conflate a trusted
/// filesystem target with a `file://` URL, which the standard URL filter
/// rejects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    Url(Arc<str>),
    File(Arc<Path>),
    Mermaid {
        action: MermaidAffordanceAction,
        source: Arc<str>,
    },
}

/// Whether painted text independently identifies its filesystem target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LinkPresentation {
    #[default]
    Opaque,
    SelfResolvingPath,
}

impl LinkTarget {
    pub fn display_text(&self) -> Cow<'_, str> {
        match self {
            Self::Url(url) => Cow::Borrowed(url),
            Self::File(path) => path.to_string_lossy(),
            Self::Mermaid { action, .. } => Cow::Borrowed(action.label()),
        }
    }

    pub(crate) const fn kind_label(&self, language: UiLanguage) -> &'static str {
        match self {
            Self::Url(_) => "URL",
            Self::File(_) => language.text("文件", "file"),
            Self::Mermaid { .. } => language.text("Mermaid 操作", "Mermaid action"),
        }
    }
}

impl fmt::Display for LinkTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.display_text())
    }
}

/// OSC 8 output and app-owned activation derived from one semantic target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLinkTarget {
    /// Terminal-owned destination, when this target has a valid OSC 8 form.
    pub osc8_url: Option<Arc<str>>,
    /// Path-native or URL-native target for explicit app activation.
    pub open_target: Option<LinkTarget>,
}

/// Resolve a semantic target without collapsing files into URL strings.
///
/// A relative file cannot be encoded as an OSC 8 `file://` URL, but it still
/// remains an app-openable path. Standard URLs are restricted to the pinned
/// upstream scheme set: HTTP, HTTPS, and mailto.
pub fn resolve_link_target(target: &LinkTarget) -> Option<ResolvedLinkTarget> {
    resolve_link_target_with_presentation(target, LinkPresentation::Opaque)
}

pub fn resolve_link_target_with_presentation(
    target: &LinkTarget,
    presentation: LinkPresentation,
) -> Option<ResolvedLinkTarget> {
    resolve_link_target_for_context(target, presentation, official_vscode_remote())
}

/// Resolve output and activation ownership for an injected terminal context.
///
/// The boolean is true only for an SSH session whose askpass path contains an
/// exact official VS Code server directory component. In that one case,
/// independently resolvable painted paths remain owned by the terminal.
pub fn resolve_link_target_for_context(
    target: &LinkTarget,
    presentation: LinkPresentation,
    is_official_vscode_remote: bool,
) -> Option<ResolvedLinkTarget> {
    match target {
        LinkTarget::Url(url) => safe_standard_url_target(url).map(|_| ResolvedLinkTarget {
            osc8_url: Some(Arc::clone(url)),
            open_target: Some(LinkTarget::Url(Arc::clone(url))),
        }),
        LinkTarget::File(_)
            if is_official_vscode_remote && presentation == LinkPresentation::SelfResolvingPath =>
        {
            Some(ResolvedLinkTarget {
                osc8_url: None,
                open_target: None,
            })
        }
        LinkTarget::File(path) => Some(ResolvedLinkTarget {
            osc8_url: file_path_to_url(path),
            open_target: Some(LinkTarget::File(Arc::clone(path))),
        }),
        LinkTarget::Mermaid { .. } => Some(ResolvedLinkTarget {
            osc8_url: None,
            open_target: None,
        }),
    }
}

pub fn resolve_link_open_target(target: &LinkTarget) -> Option<LinkTarget> {
    resolve_link_target(target).and_then(|resolved| resolved.open_target)
}

fn official_vscode_remote() -> bool {
    static OFFICIAL_REMOTE: OnceLock<bool> = OnceLock::new();
    *OFFICIAL_REMOTE.get_or_init(|| official_vscode_remote_from_env(&unicode_environment()))
}

pub(crate) fn unicode_environment() -> HashMap<String, String> {
    unicode_environment_from_os(std::env::vars_os())
}

fn unicode_environment_from_os(
    pairs: impl IntoIterator<Item = (OsString, OsString)>,
) -> HashMap<String, String> {
    pairs
        .into_iter()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect()
}

fn official_vscode_remote_from_env(environment: &HashMap<String, String>) -> bool {
    let nonempty = |name: &str| {
        environment
            .get(name)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    };
    let is_ssh = ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"]
        .iter()
        .any(|name| nonempty(name).is_some());
    is_ssh
        && nonempty("VSCODE_GIT_ASKPASS_MAIN").is_some_and(|path| {
            Path::new(path).components().any(|component| {
                matches!(
                    component,
                    std::path::Component::Normal(name)
                        if name == ".vscode-server" || name == ".vscode-server-insiders"
                )
            })
        })
}

fn file_path_to_url(path: &Path) -> Option<Arc<str>> {
    url::Url::from_file_path(path)
        .ok()
        .map(|url| Arc::from(url.as_str()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleLink {
    pub target: LinkTarget,
    pub row: usize,
    pub start_column: usize,
    pub end_column: usize,
}

/// One semantic destination painted across one or more visual rows.
///
/// Soft wrapping can split one URL or file path into several disjoint cell
/// ranges. Navigation treats the group as one link while OSC 8 emission and
/// selection styling use every fragment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleLinkGroup {
    pub target: LinkTarget,
    pub fragments: Vec<VisibleLink>,
}

impl VisibleLinkGroup {
    pub fn primary(&self) -> Option<&VisibleLink> {
        self.fragments.first()
    }
}

/// How one visual row reconnects to the row immediately before it.
///
/// `HardBreak` starts a new source line. `MidWord` restores no bytes and
/// `Space` restores the whitespace collapsed by word wrapping.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SoftWrapJoiner {
    #[default]
    HardBreak,
    MidWord,
    Space,
}

impl SoftWrapJoiner {
    const fn text(self) -> Option<&'static str> {
        match self {
            Self::HardBreak => None,
            Self::MidWord => Some(""),
            Self::Space => Some(" "),
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LinkNavigator {
    links: Vec<VisibleLink>,
    highlighted: Option<usize>,
}

#[cfg(test)]
impl LinkNavigator {
    /// Replace the hit map with the links painted in the latest frame.
    ///
    /// A selection survives a repaint only when the same target is still
    /// visible. Exact coordinates win, then the nearest duplicate target is
    /// selected. Scrolling a target out of view clears it instead of leaving
    /// a hidden action armed.
    pub fn refresh_visible(&mut self, links: Vec<VisibleLink>) {
        let selected = self.current_hit().cloned();
        self.links = links;
        self.highlighted = selected.and_then(|selected| {
            self.links
                .iter()
                .position(|link| link == &selected)
                .or_else(|| {
                    self.links
                        .iter()
                        .enumerate()
                        .filter(|(_, link)| link.target == selected.target)
                        .min_by_key(|(_, link)| {
                            link.row
                                .abs_diff(selected.row)
                                .saturating_mul(usize::from(u16::MAX))
                                .saturating_add(link.start_column.abs_diff(selected.start_column))
                        })
                        .map(|(index, _)| index)
                })
        });
    }

    /// Cycle through visible transcript links. Forward and backward movement
    /// both wrap; an empty hit map always clears stale selection.
    pub fn cycle(&mut self, forward: bool) -> Option<&LinkTarget> {
        if self.links.is_empty() {
            self.highlighted = None;
            return None;
        }
        self.highlighted = Some(match self.highlighted {
            None if forward => 0,
            None => self.links.len() - 1,
            Some(index) if forward => (index + 1) % self.links.len(),
            Some(index) => (index + self.links.len() - 1) % self.links.len(),
        });
        self.current()
    }

    pub fn current(&self) -> Option<&LinkTarget> {
        self.current_hit().map(|link| &link.target)
    }

    pub fn current_hit(&self) -> Option<&VisibleLink> {
        self.highlighted.and_then(|index| self.links.get(index))
    }

    /// Take the selected hit and clear the highlight before an external
    /// action. A link cannot stay armed after its opener handoff begins.
    pub fn take_current(&mut self) -> Option<VisibleLink> {
        let selected = self.current_hit().cloned();
        self.highlighted = None;
        selected
    }

    #[cfg(test)]
    fn set_url_links(&mut self, links: impl IntoIterator<Item = impl Into<String>>) {
        self.links = links
            .into_iter()
            .enumerate()
            .map(|(row, target)| {
                let target = target.into();
                VisibleLink {
                    end_column: target.width(),
                    target: LinkTarget::Url(Arc::from(target)),
                    row,
                    start_column: 0,
                }
            })
            .collect();
        self.highlighted = None;
    }
}

fn link_finder() -> &'static LinkFinder {
    static FINDER: OnceLock<LinkFinder> = OnceLock::new();
    FINDER.get_or_init(|| {
        let mut finder = LinkFinder::new();
        finder.kinds(&[LinkKind::Url]);
        finder
    })
}

/// One path segment without spaces.
const PATH_SEGMENT: &str = r"[a-zA-Z0-9_@.%][a-zA-Z0-9._+@%\-]*";

/// A final path segment with internal spaces must still have an extension.
const PATH_SEGMENT_SPACED: &str =
    r"[a-zA-Z0-9_@.%][a-zA-Z0-9._+@%\-]*(?: [a-zA-Z0-9._+@%\-]+)+\.[a-zA-Z0-9][a-zA-Z0-9._+@%\-]*";

fn file_path_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        let pattern = format!(r"~?/(?:{PATH_SEGMENT}/)+(?:{PATH_SEGMENT_SPACED}|{PATH_SEGMENT})");
        regex::Regex::new(&pattern).expect("file path regex")
    })
}

fn relative_file_path_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        let pattern = format!(r"(?:{PATH_SEGMENT}/)+[a-zA-Z0-9_@%+\-]+(?:\.[a-zA-Z0-9_@%+\-]+)+");
        regex::Regex::new(&pattern).expect("relative file path regex")
    })
}

/// Quoted paths may contain spaces in every segment. The closing quote is
/// verified separately because Rust's regex crate has no backreferences.
fn quoted_file_path_regex() -> &'static regex::Regex {
    static REGEX: OnceLock<regex::Regex> = OnceLock::new();
    REGEX.get_or_init(|| {
        let segment = r#"[^/"']+"#;
        let pattern = format!(r#"(["'])(~?/(?:{segment}/)+{segment})"#);
        regex::Regex::new(&pattern).expect("quoted file path regex")
    })
}

fn home_dir() -> Option<&'static Path> {
    static HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    HOME.get_or_init(dirs::home_dir).as_deref()
}

fn expand_tilde_with_home(path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    use std::path::Component;

    let mut components = path.components();
    let Some(Component::Normal(first)) = components.next() else {
        return Some(path.to_path_buf());
    };
    if first != "~" {
        return Some(path.to_path_buf());
    }

    let mut expanded = home?.to_path_buf();
    for component in components {
        match component {
            Component::Prefix(_) | Component::RootDir => {}
            _ => expanded.push(component.as_os_str()),
        }
    }
    Some(expanded)
}

fn tool_path_file_target_with_home(
    path: &str,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<LinkTarget> {
    use std::path::Component;

    let target = expand_tilde_with_home(Path::new(path), home)?;
    let target = if target.is_absolute()
        || matches!(target.components().next(), Some(Component::Prefix(_)))
    {
        target
    } else {
        match cwd {
            Some(cwd) => cwd.join(target),
            None => target,
        }
    };
    Some(LinkTarget::File(Arc::from(target)))
}

/// Turn a painted path into the exact filesystem target the OS should receive.
/// Ordinary relative paths require a caller-provided cwd; `.`/`..` and symlink
/// semantics are intentionally preserved for the OS.
pub fn tool_path_file_target(path: &str, cwd: Option<&Path>) -> Option<LinkTarget> {
    tool_path_file_target_with_home(path, cwd, home_dir())
}

fn path_to_file_target_with_home(path: &str, home: Option<&Path>) -> Option<LinkTarget> {
    tool_path_file_target_with_home(path, None, home)
}

fn local_link_to_file_target_with_home(
    destination: &str,
    media_paths: &[PathBuf],
    home: Option<&Path>,
) -> Option<LinkTarget> {
    use std::path::Component;

    let destination = destination.trim();
    if destination.is_empty()
        || destination.starts_with('#')
        || destination.contains("://")
        || destination.to_ascii_lowercase().starts_with("mailto:")
        || destination.to_ascii_lowercase().starts_with("tel:")
    {
        return None;
    }
    let painted_path = Path::new(destination);
    if painted_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    let target = expand_tilde_with_home(painted_path, home)?;
    let resolved = if target.is_absolute()
        || matches!(target.components().next(), Some(Component::Prefix(_)))
    {
        target
    } else {
        let mut matches = media_paths
            .iter()
            .filter(|path| path.is_absolute() && path.is_file() && path.ends_with(&target));
        let resolved = matches.next()?.clone();
        if matches.next().is_some() {
            return None;
        }
        resolved
    };
    resolved
        .is_file()
        .then(|| LinkTarget::File(Arc::from(resolved)))
}

/// Resolve a Markdown/local relative destination only through the exact
/// validated media files carried by the producing SDK tool result.
///
/// A suffix must identify one existing file. Ambiguous, absent, traversing,
/// URL-like, and anchor destinations stay non-actionable.
pub fn local_link_to_file_target(destination: &str, media_paths: &[PathBuf]) -> Option<LinkTarget> {
    local_link_to_file_target_with_home(destination, media_paths, home_dir())
}

/// Build a display-cell hit map from the exact lines passed to Ratatui.
///
/// This scanner has no media-provenance input, so it deliberately does not
/// infer ordinary relative paths. It discovers standard URLs and the pinned
/// upstream's independently resolvable absolute/`~/` path forms.
pub fn visible_links_from_lines(lines: &[Line<'_>]) -> Vec<VisibleLink> {
    visible_links_from_lines_with_context(lines, home_dir(), official_vscode_remote())
}

#[cfg(test)]
fn visible_links_from_lines_with_home(lines: &[Line<'_>], home: Option<&Path>) -> Vec<VisibleLink> {
    visible_links_from_lines_with_context(lines, home, false)
}

fn visible_links_from_lines_with_context(
    lines: &[Line<'_>],
    home: Option<&Path>,
    is_official_vscode_remote: bool,
) -> Vec<VisibleLink> {
    let joiners = vec![SoftWrapJoiner::HardBreak; lines.len()];
    visible_link_groups_from_soft_wrapped_lines_with_context(
        lines,
        &joiners,
        &[],
        Vec::new(),
        home,
        is_official_vscode_remote,
    )
    .into_iter()
    .flat_map(|group| group.fragments)
    .collect()
}

#[derive(Debug)]
struct RowSegment {
    row: usize,
    start: usize,
    end: usize,
}

/// Scan visual rows using their proven hard/soft-wrap relationship.
///
/// Existing semantic groups (for example a Markdown `[label](target)`) win
/// over plain-text discovery only on overlapping cells. The disjoint
/// pretty-rendered `(target)` suffix is still discovered as its own group,
/// matching the pinned upstream; an overlapping scanner hit is not duplicated.
pub fn visible_link_groups_from_soft_wrapped_lines(
    lines: &[Line<'_>],
    joiners: &[SoftWrapJoiner],
    media_paths: &[PathBuf],
    existing: Vec<VisibleLinkGroup>,
) -> Vec<VisibleLinkGroup> {
    visible_link_groups_from_soft_wrapped_lines_with_context(
        lines,
        joiners,
        media_paths,
        existing,
        home_dir(),
        official_vscode_remote(),
    )
}

fn visible_link_groups_from_soft_wrapped_lines_with_context(
    lines: &[Line<'_>],
    joiners: &[SoftWrapJoiner],
    media_paths: &[PathBuf],
    mut groups: Vec<VisibleLinkGroup>,
    home: Option<&Path>,
    is_official_vscode_remote: bool,
) -> Vec<VisibleLinkGroup> {
    let joiners_match = joiners.len() == lines.len();
    let mut logical_text = String::new();
    let mut rows = Vec::new();

    for (row, line) in lines.iter().enumerate() {
        let joiner = if joiners_match {
            joiners[row]
        } else {
            SoftWrapJoiner::HardBreak
        };
        match joiner.text() {
            Some(joiner) if !rows.is_empty() => logical_text.push_str(joiner),
            _ => {
                scan_logical_line(
                    &logical_text,
                    &rows,
                    media_paths,
                    home,
                    is_official_vscode_remote,
                    &mut groups,
                );
                logical_text.clear();
                rows.clear();
            }
        }
        let start = logical_text.len();
        for span in &line.spans {
            logical_text.push_str(span.content.as_ref());
        }
        rows.push(RowSegment {
            row,
            start,
            end: logical_text.len(),
        });
    }
    scan_logical_line(
        &logical_text,
        &rows,
        media_paths,
        home,
        is_official_vscode_remote,
        &mut groups,
    );
    groups
}

fn scan_logical_line(
    text: &str,
    rows: &[RowSegment],
    media_paths: &[PathBuf],
    home: Option<&Path>,
    is_official_vscode_remote: bool,
    groups: &mut Vec<VisibleLinkGroup>,
) {
    if text.is_empty() || rows.is_empty() {
        return;
    }
    extract_semantic_links(
        text,
        home,
        media_paths,
        |target, presentation, byte_start, byte_end| {
            if resolve_link_target_for_context(&target, presentation, is_official_vscode_remote)
                .and_then(|resolved| resolved.open_target)
                .is_none()
            {
                return;
            }
            let wrap_ranges = rows
                .iter()
                .map(|row| row.start..row.end)
                .collect::<Vec<_>>();
            let segments = byte_range_to_row_cols(text, &wrap_ranges, byte_start..byte_end);
            if segments.iter().any(|segment| {
                let Some(row) = rows.get(segment.row) else {
                    return true;
                };
                segment.col_start >= segment.col_end
                    || groups.iter().any(|group| {
                        group.fragments.iter().any(|fragment| {
                            fragment.row == row.row
                                && fragment.start_column < segment.col_end
                                && segment.col_start < fragment.end_column
                        })
                    })
            }) {
                return;
            }
            let fragments = segments
                .into_iter()
                .filter_map(|segment| {
                    let row = rows.get(segment.row)?;
                    Some(VisibleLink {
                        target: target.clone(),
                        row: row.row,
                        start_column: segment.col_start,
                        end_column: segment.col_end,
                    })
                })
                .collect::<Vec<_>>();
            if !fragments.is_empty() {
                groups.push(VisibleLinkGroup { target, fragments });
            }
        },
    );
}

fn extract_semantic_links(
    text: &str,
    home: Option<&Path>,
    media_paths: &[PathBuf],
    mut found: impl FnMut(LinkTarget, LinkPresentation, usize, usize),
) {
    let mut url_ranges = Vec::new();
    for link in link_finder().links(text) {
        let target = link.as_str();
        if safe_standard_url_target(target).is_none() {
            continue;
        }
        url_ranges.push(link.start()..link.end());
        found(
            LinkTarget::Url(Arc::from(target)),
            LinkPresentation::Opaque,
            link.start(),
            link.end(),
        );
    }

    let overlaps_url = |start: usize, end: usize| {
        url_ranges
            .iter()
            .any(|range| start < range.end && range.start < end)
    };
    let mut path_ranges = Vec::new();

    for captures in quoted_file_path_regex().captures_iter(text) {
        let open_quote = captures.get(1).expect("opening quote");
        let path_match = captures.get(2).expect("path capture");
        if text.as_bytes().get(path_match.end()) != Some(&open_quote.as_str().as_bytes()[0])
            || overlaps_url(path_match.start(), path_match.end())
        {
            continue;
        }
        let Some(target) = path_to_file_target_with_home(path_match.as_str(), home) else {
            continue;
        };
        found(
            target,
            LinkPresentation::SelfResolvingPath,
            path_match.start(),
            path_match.end(),
        );
        path_ranges.push(path_match.start()..path_match.end());
    }

    for path_match in file_path_regex().find_iter(text) {
        if overlaps_url(path_match.start(), path_match.end())
            || path_ranges
                .iter()
                .any(|range| path_match.start() < range.end && range.start < path_match.end())
        {
            continue;
        }
        if path_match.start() > 0 {
            let previous = text.as_bytes()[path_match.start() - 1];
            if previous.is_ascii_alphanumeric()
                || matches!(
                    previous,
                    b'_' | b'.' | b'+' | b'@' | b'-' | b':' | b'/' | b'~'
                )
            {
                continue;
            }
        }
        let path = path_match
            .as_str()
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        if path.is_empty() {
            continue;
        }
        let Some(target) = path_to_file_target_with_home(path, home) else {
            continue;
        };
        found(
            target,
            LinkPresentation::SelfResolvingPath,
            path_match.start(),
            path_match.start() + path.len(),
        );
        path_ranges.push(path_match.start()..path_match.start() + path.len());
    }

    if media_paths.is_empty() {
        return;
    }
    for path_match in relative_file_path_regex().find_iter(text) {
        if overlaps_url(path_match.start(), path_match.end())
            || path_ranges
                .iter()
                .any(|range| path_match.start() < range.end && range.start < path_match.end())
        {
            continue;
        }
        if path_match.start() > 0 {
            let previous = text.as_bytes()[path_match.start() - 1];
            if previous.is_ascii_alphanumeric()
                || matches!(
                    previous,
                    b'_' | b'.' | b'+' | b'@' | b'-' | b':' | b'/' | b'~' | b'%'
                )
            {
                continue;
            }
        }
        let path = path_match
            .as_str()
            .trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        let Some(target) = local_link_to_file_target_with_home(path, media_paths, home) else {
            continue;
        };
        found(
            target,
            LinkPresentation::Opaque,
            path_match.start(),
            path_match.start() + path.len(),
        );
    }
}

/// Apply selection styling to one visible hit without disturbing surrounding
/// markdown styles or splitting a Unicode grapheme.
pub fn highlight_visible_link(lines: &mut [Line<'static>], selected: &VisibleLink, style: Style) {
    let Some(line) = lines.get_mut(selected.row) else {
        return;
    };
    let mut column = 0_usize;
    let mut output: Vec<Span<'static>> = Vec::new();
    for span in std::mem::take(&mut line.spans) {
        for grapheme in span.content.graphemes(true) {
            let width = grapheme.width();
            let end = column.saturating_add(width);
            let selected_cell = if width == 0 {
                column >= selected.start_column && column < selected.end_column
            } else {
                column < selected.end_column && end > selected.start_column
            };
            let grapheme_style = if selected_cell {
                span.style.patch(style)
            } else {
                span.style
            };
            if let Some(previous) = output.last_mut()
                && previous.style == grapheme_style
            {
                previous.content.to_mut().push_str(grapheme);
            } else {
                output.push(Span::styled(grapheme.to_string(), grapheme_style));
            }
            column = end;
        }
    }
    line.spans = output;
}

/// Return only a bounded standard URL whose exact bytes are safe to hand to
/// OSC 8 and an OS handler. Existing target hardening intentionally rejects
/// whitespace and terminal/bidi controls rather than normalizing them.
pub fn safe_standard_url_target(target: &str) -> Option<Cow<'_, str>> {
    if target.is_empty()
        || target.len() > MAX_LINK_TARGET_BYTES
        || target.chars().any(char::is_whitespace)
    {
        return None;
    }
    let parsed = url::Url::parse(target).ok()?;
    match parsed.scheme() {
        "http" | "https" => {
            let scheme_end = target.find("://")?;
            let authority = target[scheme_end + 3..]
                .split(['/', '?', '#'])
                .next()
                .unwrap_or_default();
            if authority.is_empty() || parsed.host_str().is_none() {
                return None;
            }
        }
        "mailto" => {}
        _ => return None,
    }
    match sanitize_osc8_target(target) {
        Cow::Borrowed(target) => Some(Cow::Borrowed(target)),
        Cow::Owned(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    fn url(value: &str) -> LinkTarget {
        LinkTarget::Url(Arc::from(value))
    }

    fn file(value: &str) -> LinkTarget {
        LinkTarget::File(Arc::from(Path::new(value)))
    }

    #[test]
    fn semantic_link_kind_labels_follow_ui_language() {
        let mermaid = LinkTarget::Mermaid {
            action: MermaidAffordanceAction::Open,
            source: Arc::from("graph TD; A-->B"),
        };
        assert_eq!(
            url("https://example.test").kind_label(UiLanguage::ZhCn),
            "URL"
        );
        assert_eq!(
            url("https://example.test").kind_label(UiLanguage::EnUs),
            "URL"
        );
        assert_eq!(file("/tmp/a").kind_label(UiLanguage::ZhCn), "文件");
        assert_eq!(file("/tmp/a").kind_label(UiLanguage::EnUs), "file");
        assert_eq!(mermaid.kind_label(UiLanguage::ZhCn), "Mermaid 操作");
        assert_eq!(mermaid.kind_label(UiLanguage::EnUs), "Mermaid action");
    }

    #[test]
    fn interaction_links_cycle_wraps() {
        let mut navigator = LinkNavigator::default();
        navigator.set_url_links(["https://a.test", "https://b.test", "https://c.test"]);
        assert_eq!(navigator.cycle(true), Some(&url("https://a.test")));
        assert_eq!(navigator.cycle(true), Some(&url("https://b.test")));
        assert_eq!(navigator.cycle(true), Some(&url("https://c.test")));
        assert_eq!(navigator.cycle(true), Some(&url("https://a.test")));
        assert_eq!(navigator.cycle(false), Some(&url("https://c.test")));
    }

    #[test]
    fn visible_refresh_clears_hidden_selection_and_preserves_repaint() {
        let first = VisibleLink {
            target: url("https://a.test"),
            row: 3,
            start_column: 4,
            end_column: 18,
        };
        let mut navigator = LinkNavigator::default();
        navigator.refresh_visible(vec![first.clone()]);
        assert_eq!(navigator.cycle(true), Some(&url("https://a.test")));
        navigator.refresh_visible(vec![first]);
        assert_eq!(navigator.current(), Some(&url("https://a.test")));
        navigator.refresh_visible(Vec::new());
        assert_eq!(navigator.current(), None);
        assert_eq!(navigator.cycle(true), None);
    }

    #[test]
    fn visible_hit_map_uses_unicode_display_columns() {
        let lines = vec![Line::from(vec![
            Span::raw("界e\u{301} "),
            Span::styled("https://one.test/a", Style::default().fg(Color::Blue)),
        ])];
        let links = visible_links_from_lines(&lines);
        assert_eq!(
            links,
            vec![VisibleLink {
                target: url("https://one.test/a"),
                row: 0,
                start_column: 4,
                end_column: 22,
            }]
        );
    }

    #[test]
    fn highlight_changes_only_selected_graphemes() {
        let mut lines = vec![Line::from(vec![
            Span::styled("界 ", Style::default().fg(Color::White)),
            Span::styled(
                "https://one.test/a trailing",
                Style::default().fg(Color::Blue),
            ),
        ])];
        let selected = visible_links_from_lines(&lines)
            .into_iter()
            .next()
            .expect("link");
        highlight_visible_link(
            &mut lines,
            &selected,
            Style::default()
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
        let selected_text = lines[0]
            .spans
            .iter()
            .filter(|span| span.style.bg == Some(Color::Yellow))
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(selected_text, "https://one.test/a");
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content.contains("trailing") && span.style.bg.is_none())
        );
    }

    #[test]
    fn extraction_matches_the_fixed_upstream_url_kind_scanner() {
        let lines = vec![Line::raw(
            "See [one](https://one.test/a), <http://two.test/b>, \
             mailto:user@example.com and https://three.test/c.",
        )];
        let links = visible_links_from_lines(&lines)
            .into_iter()
            .map(|link| link.target.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            links,
            vec![
                "https://one.test/a",
                "http://two.test/b",
                "https://three.test/c"
            ]
        );
    }

    #[test]
    fn semantic_file_scan_preserves_path_target_and_avoids_url_overlap() {
        let lines = vec![Line::raw(
            "Open \"/tmp/Crab Code/report 1.pdf\", /tmp/project/src/main.rs, \
             and https://host.test/tmp/not-a-file.rs.",
        )];
        let links = visible_links_from_lines_with_home(&lines, Some(Path::new("/home/test")));
        assert_eq!(links.len(), 3);
        assert_eq!(links[0].target, url("https://host.test/tmp/not-a-file.rs"));
        assert_eq!(links[1].target, file("/tmp/Crab Code/report 1.pdf"));
        assert_eq!(links[2].target, file("/tmp/project/src/main.rs"));
        assert_eq!(
            &lines[0].spans[0].content[links[1].start_column..links[1].end_column],
            "/tmp/Crab Code/report 1.pdf"
        );
    }

    #[test]
    fn tilde_path_requires_a_real_home_and_relative_path_is_not_inferred() {
        let line = Line::raw("See ~/Desktop/report.pdf and images/1.png");
        let lines = [line.clone()];
        let links = visible_links_from_lines_with_home(&lines, Some(Path::new("/home/test")));
        assert_eq!(links.len(), 1);
        assert_eq!(
            links[0].target,
            LinkTarget::File(Arc::from(Path::new("/home/test/Desktop/report.pdf")))
        );
        assert!(visible_links_from_lines_with_home(&[line], None).is_empty());
    }

    #[test]
    fn soft_wrapped_url_is_one_group_with_one_fragment_per_row() {
        let lines = [
            Line::raw("See https://example.test/a/lo"),
            Line::raw("ng/path?key=value for details"),
        ];
        let groups = visible_link_groups_from_soft_wrapped_lines_with_context(
            &lines,
            &[SoftWrapJoiner::HardBreak, SoftWrapJoiner::MidWord],
            &[],
            Vec::new(),
            None,
            false,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].target,
            url("https://example.test/a/long/path?key=value")
        );
        assert_eq!(
            groups[0].fragments,
            vec![
                VisibleLink {
                    target: groups[0].target.clone(),
                    row: 0,
                    start_column: 4,
                    end_column: 29,
                },
                VisibleLink {
                    target: groups[0].target.clone(),
                    row: 1,
                    start_column: 0,
                    end_column: 17,
                },
            ]
        );
    }

    #[test]
    fn soft_wrapped_url_match_projects_across_three_rows_on_the_production_path() {
        let lines = [
            Line::raw("https://example.test/abc"),
            Line::raw("defgh"),
            Line::raw("ijklmnop"),
        ];
        let groups = visible_link_groups_from_soft_wrapped_lines_with_context(
            &lines,
            &[
                SoftWrapJoiner::HardBreak,
                SoftWrapJoiner::MidWord,
                SoftWrapJoiner::MidWord,
            ],
            &[],
            Vec::new(),
            None,
            false,
        );

        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].target,
            url("https://example.test/abcdefghijklmnop")
        );
        assert_eq!(
            groups[0]
                .fragments
                .iter()
                .map(|fragment| (fragment.row, fragment.start_column, fragment.end_column,))
                .collect::<Vec<_>>(),
            vec![(0, 0, 24), (1, 0, 5), (2, 0, 8)]
        );
    }

    #[test]
    fn hard_break_does_not_invent_a_cross_row_url() {
        let lines = [
            Line::raw("See https://example.test/a/lo"),
            Line::raw("ng/path?key=value for details"),
        ];
        let groups = visible_link_groups_from_soft_wrapped_lines_with_context(
            &lines,
            &[SoftWrapJoiner::HardBreak, SoftWrapJoiner::HardBreak],
            &[],
            Vec::new(),
            None,
            false,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].target, url("https://example.test/a/lo"));
        assert_eq!(groups[0].fragments.len(), 1);
    }

    #[test]
    fn space_joiner_restores_a_wrapped_spaced_file_name() {
        let lines = [
            Line::raw("open /tmp/release/Demo"),
            Line::raw("App.app now"),
        ];
        let groups = visible_link_groups_from_soft_wrapped_lines_with_context(
            &lines,
            &[SoftWrapJoiner::HardBreak, SoftWrapJoiner::Space],
            &[],
            Vec::new(),
            None,
            false,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].target, file("/tmp/release/Demo App.app"));
        assert_eq!(groups[0].fragments.len(), 2);
        assert_eq!(groups[0].fragments[1].start_column, 0);
        assert_eq!(groups[0].fragments[1].end_column, "App.app".width());
    }

    #[test]
    fn relative_media_requires_one_existing_provenance_match() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = root.path().join("one/images/1.png");
        std::fs::create_dir_all(first.parent().expect("parent")).expect("mkdir");
        std::fs::write(&first, b"image").expect("write");
        assert_eq!(
            local_link_to_file_target("images/1.png", std::slice::from_ref(&first)),
            Some(LinkTarget::File(Arc::from(first.as_path())))
        );
        assert!(local_link_to_file_target("images/2.png", std::slice::from_ref(&first)).is_none());
        assert!(
            local_link_to_file_target("../images/1.png", std::slice::from_ref(&first)).is_none()
        );

        let second = root.path().join("two/images/1.png");
        std::fs::create_dir_all(second.parent().expect("parent")).expect("mkdir");
        std::fs::write(&second, b"image").expect("write");
        assert!(local_link_to_file_target("images/1.png", &[first, second]).is_none());
    }

    #[test]
    fn soft_wrapped_relative_media_uses_exact_provenance_target() {
        let root = tempfile::tempdir().expect("tempdir");
        let media = root.path().join("session/images/1.png");
        std::fs::create_dir_all(media.parent().expect("parent")).expect("mkdir");
        std::fs::write(&media, b"image").expect("write");
        let lines = [
            Line::raw("Saved to images/1.p"),
            Line::raw("ng in the workspace"),
        ];
        let groups = visible_link_groups_from_soft_wrapped_lines_with_context(
            &lines,
            &[SoftWrapJoiner::HardBreak, SoftWrapJoiner::MidWord],
            std::slice::from_ref(&media),
            Vec::new(),
            None,
            false,
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].target,
            LinkTarget::File(Arc::from(media.as_path()))
        );
        assert_eq!(groups[0].fragments.len(), 2);
    }

    #[test]
    fn official_vscode_remote_detection_requires_ssh_and_an_exact_server_component() {
        let environment = |askpass: &str, ssh: bool| {
            let mut environment =
                HashMap::from([("VSCODE_GIT_ASKPASS_MAIN".to_string(), askpass.to_string())]);
            if ssh {
                environment.insert(
                    "SSH_CONNECTION".to_string(),
                    "192.0.2.1 50000 192.0.2.2 22".to_string(),
                );
            }
            environment
        };
        for server in [".vscode-server", ".vscode-server-insiders"] {
            assert!(official_vscode_remote_from_env(&environment(
                &format!("/home/user/{server}/bin/askpass"),
                true,
            )));
        }
        for askpass in [
            "/home/user/.vscode-server-oss/bin/askpass",
            "/home/user/.vscodium-server/bin/askpass",
            "/home/user/cache/.vscode-serverish/bin/askpass",
        ] {
            assert!(!official_vscode_remote_from_env(&environment(
                askpass, true
            )));
        }
        assert!(!official_vscode_remote_from_env(&environment(
            "/home/user/.vscode-server/bin/askpass",
            false,
        )));
    }

    #[cfg(unix)]
    #[test]
    fn unicode_environment_skips_non_utf8_keys_and_values_without_panicking() {
        use std::os::unix::ffi::OsStringExt;

        let environment = unicode_environment_from_os([
            (OsString::from("DISPLAY"), OsString::from(":0")),
            (
                OsString::from_vec(b"bad-\xff".to_vec()),
                OsString::from("value"),
            ),
            (
                OsString::from("BAD_VALUE"),
                OsString::from_vec(b"value-\xff".to_vec()),
            ),
        ]);
        assert_eq!(
            environment,
            HashMap::from([("DISPLAY".to_string(), ":0".to_string())])
        );
    }

    #[test]
    fn official_vscode_remote_delegates_only_self_resolving_file_text() {
        let file_target = file("/worktree/src/main.rs");
        assert_eq!(
            resolve_link_target_for_context(
                &file_target,
                LinkPresentation::SelfResolvingPath,
                true,
            ),
            Some(ResolvedLinkTarget {
                osc8_url: None,
                open_target: None,
            })
        );
        assert!(
            resolve_link_target_for_context(&file_target, LinkPresentation::Opaque, true)
                .and_then(|resolved| resolved.open_target)
                .is_some()
        );
        let lines = [Line::raw("/worktree/src/main.rs https://example.com/docs")];
        let visible =
            visible_links_from_lines_with_context(&lines, Some(Path::new("/home/test")), true);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].target, url("https://example.com/docs"));
    }

    #[test]
    fn tool_path_resolution_preserves_relative_parent_and_nonexistent_paths() {
        assert_eq!(
            tool_path_file_target("src/../main.rs", Some(Path::new("/worktree"))),
            Some(file("/worktree/src/../main.rs"))
        );
        let absolute = tool_path_file_target("/tmp/does-not-exist/file.rs", None).expect("target");
        assert_eq!(absolute, file("/tmp/does-not-exist/file.rs"));
    }

    #[test]
    fn resolved_target_keeps_standard_scheme_filter_and_path_native_open() {
        let web = url("https://example.com/a");
        assert_eq!(
            resolve_link_target(&web),
            Some(ResolvedLinkTarget {
                osc8_url: Some(Arc::from("https://example.com/a")),
                open_target: Some(web.clone()),
            })
        );
        let unsafe_url = url("javascript:alert(1)");
        assert!(resolve_link_target(&unsafe_url).is_none());
        let mail = url("mailto:user@example.com");
        assert_eq!(
            resolve_link_target(&mail),
            Some(ResolvedLinkTarget {
                osc8_url: Some(Arc::from("mailto:user@example.com")),
                open_target: Some(mail),
            })
        );

        let file_with_space = file("/tmp/a b.rs");
        assert_eq!(
            resolve_link_target(&file_with_space),
            Some(ResolvedLinkTarget {
                osc8_url: Some(Arc::from("file:///tmp/a%20b.rs")),
                open_target: Some(file_with_space.clone()),
            })
        );
        let relative_file = file("relative.rs");
        assert_eq!(
            resolve_link_target(&relative_file),
            Some(ResolvedLinkTarget {
                osc8_url: None,
                open_target: Some(relative_file),
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_target_preserves_non_utf8_bytes_while_osc8_percent_encodes_them() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let path = PathBuf::from(OsString::from_vec(b"/tmp/non-utf8-\x80/main.rs".to_vec()));
        let target = LinkTarget::File(Arc::from(path));
        let LinkTarget::File(open_path) = resolve_link_open_target(&target).expect("open target")
        else {
            panic!("expected file");
        };
        assert_eq!(
            open_path.as_os_str().as_bytes(),
            b"/tmp/non-utf8-\x80/main.rs"
        );
        let osc8 = resolve_link_target(&target)
            .and_then(|resolved| resolved.osc8_url)
            .expect("OSC 8 URL");
        assert!(osc8.contains("/tmp/non-utf8-%80/main.rs"), "got {osc8}");
        assert!(!osc8.contains("%EF%BF%BD"), "lossy replacement leaked");
    }

    #[test]
    fn unsafe_or_ambiguous_targets_are_not_actionable() {
        assert!(safe_standard_url_target("https://safe.test/a").is_some());
        assert!(safe_standard_url_target("HTTP://EXAMPLE.TEST/a").is_some());
        assert!(safe_standard_url_target("mailto:user@example.com").is_some());
        assert!(safe_standard_url_target("file:///tmp/a").is_none());
        assert!(safe_standard_url_target("https:///missing-host").is_none());
        assert!(safe_standard_url_target("https://?missing-host").is_none());
        assert!(safe_standard_url_target("https://#missing-host").is_none());
        assert!(safe_standard_url_target(" https://safe.test/a").is_none());
        assert!(safe_standard_url_target("https://safe.test/\u{1b}]8;;evil").is_none());
        assert!(safe_standard_url_target("https://safe.test/\u{202e}gpj.exe").is_none());
        assert!(
            safe_standard_url_target(&format!("https://a.test/{}", "x".repeat(8192))).is_none()
        );
    }

    #[test]
    fn automatic_link_scan_skips_unsafe_schemes() {
        let lines = [Line::raw(
            "javascript:alert(1) data:text/plain,secret https://safe.test/path",
        )];
        let groups = visible_link_groups_from_soft_wrapped_lines(
            &lines,
            &[SoftWrapJoiner::HardBreak],
            &[],
            Vec::new(),
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].target, url("https://safe.test/path"));
    }

    #[test]
    fn taking_a_link_clears_the_armed_selection() {
        let mut navigator = LinkNavigator::default();
        navigator.set_url_links(["https://a.test"]);
        assert_eq!(navigator.cycle(true), Some(&url("https://a.test")));
        assert_eq!(
            navigator.take_current().map(|link| link.target),
            Some(url("https://a.test"))
        );
        assert_eq!(navigator.current(), None);
    }
}

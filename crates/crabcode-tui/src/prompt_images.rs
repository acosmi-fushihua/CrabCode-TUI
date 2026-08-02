/// Strip a single pair of matching ASCII single or double quotes that
/// wrap `s`. Otherwise return `s` unchanged.
fn strip_matching_quotes(s: &str) -> &str {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &s[1..s.len() - 1];
        }
    }
    s
}

/// Whether `s` begins with a drop-style path anchor: `/`, `~/`, a
/// Windows drive (`X:\` or `X:/`), or a Windows UNC (`\\`). ASCII-only
/// so it never inspects a partial UTF-8 codepoint.
fn starts_with_path_anchor(s: &str) -> bool {
    let b = s.as_bytes();
    matches!(b.first(), Some(b'/'))
        || b.starts_with(b"~/")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && (b[2] == b'\\' || b[2] == b'/'))
        || b.starts_with(b"\\\\")
}

/// Whether `s` begins with something the space-splitter should treat as
/// a path token boundary: a bare path anchor, a `file://` URL, or a
/// quoted form of either (quotes are stripped before re-checking).
pub fn starts_with_drop_anchor(s: &str) -> bool {
    if starts_with_path_anchor(s) || s.starts_with("file://") {
        return true;
    }
    let unq = strip_matching_quotes(s);
    !std::ptr::eq(unq, s) && (starts_with_path_anchor(unq) || unq.starts_with("file://"))
}

// Renderer-private module boundary.

#[cfg(test)]
mod tests {
    use super::starts_with_drop_anchor;

    #[test]
    fn fixed_drop_anchor_accepts_every_supported_shape() {
        for input in [
            r"C:\foo.png",
            "C:/foo.png",
            r"\\srv\share\a.png",
            "/Users/a/b.png",
            "~/images/a.png",
            "file:///tmp/x.png",
            "\"C:\\My Pics\\a.png\"",
            "'/tmp/my image.png'",
        ] {
            assert!(starts_with_drop_anchor(input), "{input:?}");
        }
    }

    #[test]
    fn fixed_drop_anchor_rejects_relative_prose_and_unmatched_quotes() {
        for input in [
            "hello world",
            "relative/image.png",
            "\"relative/image.png\"",
            "\"/tmp/image.png",
        ] {
            assert!(!starts_with_drop_anchor(input), "{input:?}");
        }
    }
}

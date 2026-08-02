/// Truncate body to an overview using UTF-8 character boundaries.
pub fn truncate_overview(body: &str, max_chars: usize) -> String {
    body.chars().take(max_chars).collect()
}

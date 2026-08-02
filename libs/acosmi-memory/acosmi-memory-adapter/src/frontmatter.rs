use serde_yaml::{Mapping, Value};

use crate::AdapterError;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TsFrontmatter {
    pub name: Option<String>,
    pub description: Option<String>,
    pub type_: Option<String>,
    /// W-MEMORY-KB-UPLIFT P0 (2026-07-17) — knowledge-entry review-gate
    /// fields. `enabled: false` / `pending_review: true` mark agent drafts /
    /// evolution proposals that must NOT participate in retrieval until a
    /// human enables them; `injection` carries the knowledge injection mode
    /// (`always` / `auto` / `manual`). All absent on regular memory files.
    pub enabled: Option<bool>,
    pub pending_review: Option<bool>,
    pub injection: Option<String>,
}

/// Parse TS-compatible frontmatter from markdown.
///
/// This mirrors `src/utils/frontmatterParser.ts`: no frontmatter is allowed,
/// and YAML parse failures first retry after quoting simple unquoted values
/// that contain YAML-special characters.
pub fn frontmatter_ts_compat(
    markdown: &str,
) -> Result<(TsFrontmatter, String, bool), AdapterError> {
    let Some((frontmatter_text, body_start)) = extract_frontmatter(markdown) else {
        return Ok((TsFrontmatter::default(), markdown.to_owned(), false));
    };

    let body = markdown[body_start..].to_owned();
    let frontmatter = parse_frontmatter_text(frontmatter_text).or_else(|_| {
        let quoted = quote_problematic_values(frontmatter_text);
        parse_frontmatter_text(&quoted)
    })?;

    Ok((frontmatter, body, true))
}

fn extract_frontmatter(markdown: &str) -> Option<(&str, usize)> {
    if !markdown.starts_with("---") {
        return None;
    }

    let content_start = opening_content_start(markdown)?;
    let close_rel = markdown[content_start..].find("---")?;
    let close_start = content_start + close_rel;
    let body_start = consume_whitespace(markdown, close_start + 3);

    Some((&markdown[content_start..close_start], body_start))
}

fn opening_content_start(markdown: &str) -> Option<usize> {
    let mut last_newline_end = None;
    for (offset, ch) in markdown[3..].char_indices() {
        if !ch.is_whitespace() {
            break;
        }
        if ch == '\n' {
            last_newline_end = Some(3 + offset + ch.len_utf8());
        }
    }
    last_newline_end
}

fn consume_whitespace(markdown: &str, start: usize) -> usize {
    let mut end = start;
    for (offset, ch) in markdown[start..].char_indices() {
        if !ch.is_whitespace() {
            break;
        }
        end = start + offset + ch.len_utf8();
    }
    end
}

fn parse_frontmatter_text(frontmatter_text: &str) -> Result<TsFrontmatter, AdapterError> {
    let parsed: Value = serde_yaml::from_str(frontmatter_text)?;
    let Some(mapping) = parsed.as_mapping() else {
        return Ok(TsFrontmatter::default());
    };

    Ok(TsFrontmatter {
        name: string_field(mapping, "name"),
        description: string_field(mapping, "description"),
        type_: string_field(mapping, "type"),
        enabled: bool_field(mapping, "enabled"),
        pending_review: bool_field(mapping, "pending_review"),
        injection: string_field(mapping, "injection"),
    })
}

fn string_field(mapping: &Mapping, key: &str) -> Option<String> {
    mapping
        .get(Value::String(key.to_owned()))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn bool_field(mapping: &Mapping, key: &str) -> Option<bool> {
    mapping
        .get(Value::String(key.to_owned()))
        .and_then(Value::as_bool)
}

fn quote_problematic_values(frontmatter_text: &str) -> String {
    frontmatter_text
        .split('\n')
        .map(quote_problematic_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn quote_problematic_line(line: &str) -> String {
    let Some(colon_idx) = line.find(':') else {
        return line.to_owned();
    };
    let key = &line[..colon_idx];
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == '_' || c == '-')
    {
        return line.to_owned();
    }

    let after_colon = &line[colon_idx + 1..];
    if !after_colon
        .chars()
        .next()
        .is_some_and(|c| c.is_whitespace())
    {
        return line.to_owned();
    }

    let value = after_colon.trim_start();
    if value.is_empty()
        || ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
        || !has_yaml_special_chars(value)
    {
        return line.to_owned();
    }

    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{key}: \"{escaped}\"")
}

fn has_yaml_special_chars(value: &str) -> bool {
    value.contains(": ")
        || value.chars().any(|c| {
            matches!(
                c,
                '{' | '}' | '[' | ']' | '*' | '&' | '#' | '!' | '|' | '>' | '%' | '@' | '`'
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W-MEMORY-KB-UPLIFT P0 — the review-gate fields parse from knowledge
    /// frontmatter and stay `None` when absent (regular memory files).
    #[test]
    fn review_gate_fields_parse_and_default_to_none() {
        let (fm, body, has) = frontmatter_ts_compat(
            "---\ntype: knowledge\nname: n\nenabled: false\npending_review: true\ninjection: manual\n---\nbody\n",
        )
        .unwrap();
        assert!(has);
        assert_eq!(fm.enabled, Some(false));
        assert_eq!(fm.pending_review, Some(true));
        assert_eq!(fm.injection.as_deref(), Some("manual"));
        assert_eq!(body.trim(), "body");

        let (fm, _, _) = frontmatter_ts_compat("---\ntype: project\nname: m\n---\nbody\n").unwrap();
        assert_eq!(fm.enabled, None);
        assert_eq!(fm.pending_review, None);
        assert_eq!(fm.injection, None);
    }
}

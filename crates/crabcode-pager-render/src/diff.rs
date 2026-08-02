//! Renderer-local diff values and presentation algorithms.
//!
//! Backend decoding remains outside this crate. The direct adapter supplies
//! lossless [`DiffHunk`] values; this module only stitches overlapping display
//! hunks and projects them to a unified patch for copy/export.

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, PartialEq)]
pub struct DiffLine {
    pub text: String,
    pub lo: usize,
    pub ln: usize,
    pub tag: ChangeTag,
}

pub type DiffHunk = Vec<DiffLine>;

/// Build display hunks from complete before/after strings.
///
/// This is the context-free specialization of the fixed renderer's
/// `SearchReplaceEditDetail` algorithm. Keeping it renderer-local lets the
/// direct adapter preserve structured edit presentation without importing a
/// backend tool schema.
#[must_use]
pub fn diff_hunks_from_strings(old_text: &str, new_text: &str, start_line: usize) -> Vec<DiffHunk> {
    const MAX_CONTEXT: usize = 3;

    let mut lines = Vec::new();
    let (mut old_line, mut new_line) = (start_line, start_line);
    let diff = TextDiff::from_lines(old_text, new_text);
    for change in diff.iter_all_changes() {
        let tag = change.tag();
        lines.push(DiffLine {
            text: change.value().to_owned(),
            lo: old_line,
            ln: new_line,
            tag,
        });
        match tag {
            ChangeTag::Equal => {
                old_line = old_line.saturating_add(1);
                new_line = new_line.saturating_add(1);
            }
            ChangeTag::Delete => old_line = old_line.saturating_add(1),
            ChangeTag::Insert => new_line = new_line.saturating_add(1),
        }
    }

    let total_len = lines.len();
    let (mut start, mut end) = if lines.iter().all(|line| line.tag == ChangeTag::Equal) {
        (total_len, total_len)
    } else {
        let equal_before = lines
            .iter()
            .take_while(|line| line.tag == ChangeTag::Equal)
            .count();
        let equal_after = lines
            .iter()
            .rev()
            .take_while(|line| line.tag == ChangeTag::Equal)
            .count();
        (
            equal_before.saturating_sub(MAX_CONTEXT),
            total_len.saturating_sub(equal_after.saturating_sub(MAX_CONTEXT)),
        )
    };

    while start < end
        && lines[start].tag == ChangeTag::Equal
        && lines[start].text.trim_ascii().is_empty()
    {
        start += 1;
    }
    while start < end
        && lines[end - 1].tag == ChangeTag::Equal
        && lines[end - 1].text.trim_ascii().is_empty()
    {
        end -= 1;
    }

    if start < end {
        vec![lines[start..end].to_vec()]
    } else {
        Vec::new()
    }
}

/// Stitch overlapping or adjacent same-file display hunks when their
/// post-state coordinates and text agree exactly.
///
/// Ambiguous line-count-changing shapes and contradictory snapshots remain
/// separate rather than presenting a false merged diff.
#[must_use]
pub fn stitch_overlapping_hunks(hunks: Vec<DiffHunk>) -> Vec<DiffHunk> {
    let mut output: Vec<DiffHunk> = Vec::with_capacity(hunks.len());
    for hunk in hunks {
        if let Some(previous) = output.last_mut()
            && let Some(stitched) = stitch_hunk_pair(previous, &hunk)
        {
            *previous = stitched;
            continue;
        }
        output.push(hunk);
    }
    output
}

fn render_range(hunk: &DiffHunk) -> Option<(usize, usize)> {
    let mut range: Option<(usize, usize)> = None;
    for line in hunk {
        if line.tag == ChangeTag::Delete {
            continue;
        }
        range = Some(match range {
            None => (line.ln, line.ln),
            Some((minimum, maximum)) => (minimum.min(line.ln), maximum.max(line.ln)),
        });
    }
    range
}

fn render_position(hunk: &DiffHunk, line_number: usize) -> Option<usize> {
    hunk.iter()
        .position(|line| line.tag != ChangeTag::Delete && line.ln == line_number)
}

fn trimmed(text: &str) -> &str {
    text.trim_end_matches(['\r', '\n'])
}

fn stitch_hunk_pair(first: &DiffHunk, second: &DiffHunk) -> Option<DiffHunk> {
    let (first_minimum, first_maximum) = render_range(first)?;
    let (second_minimum, _) = render_range(second)?;
    if second_minimum < first_minimum || second_minimum > first_maximum + 1 {
        return None;
    }

    let mut output = first.clone();
    let mut maximum_line = first_maximum;
    let mut index = 0;
    while index < second.len() {
        let row = &second[index];
        if row.ln > maximum_line {
            for remaining in &second[index..] {
                if remaining.tag != ChangeTag::Delete {
                    if remaining.ln != maximum_line + 1 {
                        return None;
                    }
                    maximum_line = remaining.ln;
                }
                output.push(remaining.clone());
            }
            break;
        }

        match row.tag {
            ChangeTag::Equal => {
                let position = render_position(&output, row.ln)?;
                if trimmed(&output[position].text) != trimmed(&row.text) {
                    return None;
                }
                index += 1;
            }
            ChangeTag::Delete => {
                let replacement = second.get(index + 1)?;
                if replacement.tag != ChangeTag::Insert || replacement.ln != row.ln {
                    return None;
                }
                let position = render_position(&output, row.ln)?;
                if trimmed(&output[position].text) != trimmed(&row.text) {
                    return None;
                }
                match output[position].tag {
                    ChangeTag::Equal => {
                        output[position] = row.clone();
                        output.insert(position + 1, replacement.clone());
                    }
                    ChangeTag::Insert => {
                        output[position] = replacement.clone();
                    }
                    ChangeTag::Delete => unreachable!("render_position excludes deleted rows"),
                }
                index += 2;
            }
            ChangeTag::Insert => return None,
        }
    }
    Some(output)
}

/// Generate a unified patch string from display hunks.
#[must_use]
pub fn diff_hunks_to_patch(path: &str, hunks: &[DiffHunk]) -> String {
    if hunks.is_empty() {
        return String::new();
    }

    let mut output = String::new();
    output.push_str(&format!("--- a/{path}\n"));
    output.push_str(&format!("+++ b/{path}\n"));

    for hunk in hunks {
        if hunk.is_empty() {
            continue;
        }

        let old_start = hunk
            .iter()
            .filter(|line| line.tag != ChangeTag::Insert)
            .map(|line| line.lo)
            .next()
            .unwrap_or(1);
        let new_start = hunk
            .iter()
            .filter(|line| line.tag != ChangeTag::Delete)
            .map(|line| line.ln)
            .next()
            .unwrap_or(1);
        let old_count = hunk
            .iter()
            .filter(|line| line.tag != ChangeTag::Insert)
            .count();
        let new_count = hunk
            .iter()
            .filter(|line| line.tag != ChangeTag::Delete)
            .count();

        output.push_str(&format!(
            "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n",
        ));

        for line in hunk {
            let prefix = match line.tag {
                ChangeTag::Equal => ' ',
                ChangeTag::Insert => '+',
                ChangeTag::Delete => '-',
            };
            output.push(prefix);
            output.push_str(trimmed(&line.text));
            output.push('\n');
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str, old: usize, new: usize, tag: ChangeTag) -> DiffLine {
        DiffLine {
            text: text.to_owned(),
            lo: old,
            ln: new,
            tag,
        }
    }

    #[test]
    fn string_diff_matches_fixed_renderer_coordinates_and_content() {
        let hunks = diff_hunks_from_strings("hello\nworld\n", "hello\nearth\n", 7);
        assert_eq!(hunks.len(), 1);
        assert_eq!(
            hunks[0],
            vec![
                line("hello\n", 7, 7, ChangeTag::Equal),
                line("world\n", 8, 8, ChangeTag::Delete),
                line("earth\n", 9, 8, ChangeTag::Insert),
            ],
        );
        assert!(diff_hunks_from_strings("same\n", "same\n", 1).is_empty());
    }

    #[test]
    fn string_diff_preserves_new_file_insertion() {
        let hunks = diff_hunks_from_strings("", "new content\n", 1);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0][0].tag, ChangeTag::Insert);
        assert_eq!(hunks[0][0].text, "new content\n");
    }

    #[test]
    fn chained_replacement_keeps_original_delete_and_final_insert() {
        let first = vec![
            line("a\n", 1, 1, ChangeTag::Delete),
            line("b\n", 2, 1, ChangeTag::Insert),
            line("x\n", 2, 2, ChangeTag::Equal),
        ];
        let second = vec![
            line("b\n", 1, 1, ChangeTag::Delete),
            line("c\n", 2, 1, ChangeTag::Insert),
            line("x\n", 2, 2, ChangeTag::Equal),
        ];

        let stitched = stitch_overlapping_hunks(vec![first, second]);
        assert_eq!(
            stitched,
            vec![vec![
                line("a\n", 1, 1, ChangeTag::Delete),
                line("c\n", 2, 1, ChangeTag::Insert),
                line("x\n", 2, 2, ChangeTag::Equal),
            ]],
        );
    }

    #[test]
    fn context_disagreement_and_unpaired_insert_fail_closed() {
        let first = vec![
            line("alpha", 1, 1, ChangeTag::Equal),
            line("beta", 2, 2, ChangeTag::Equal),
        ];
        let disagreeing = vec![
            line("omega", 1, 1, ChangeTag::Equal),
            line("beta", 2, 2, ChangeTag::Equal),
        ];
        assert_eq!(
            stitch_overlapping_hunks(vec![first.clone(), disagreeing.clone()]),
            vec![first.clone(), disagreeing],
        );

        let growing = vec![
            line("alpha", 1, 1, ChangeTag::Equal),
            line("inserted", 2, 2, ChangeTag::Insert),
        ];
        assert_eq!(
            stitch_overlapping_hunks(vec![first.clone(), growing.clone()]),
            vec![first, growing],
        );
    }

    #[test]
    fn adjacent_truthful_tail_extends_previous_hunk() {
        let first = vec![
            line("one", 1, 1, ChangeTag::Equal),
            line("two", 2, 2, ChangeTag::Equal),
        ];
        let second = vec![
            line("two", 2, 2, ChangeTag::Equal),
            line("three", 3, 3, ChangeTag::Equal),
        ];
        assert_eq!(
            stitch_overlapping_hunks(vec![first, second]),
            vec![vec![
                line("one", 1, 1, ChangeTag::Equal),
                line("two", 2, 2, ChangeTag::Equal),
                line("three", 3, 3, ChangeTag::Equal),
            ]],
        );
    }

    #[test]
    fn patch_projection_preserves_counts_prefixes_and_normalizes_line_endings() {
        let hunks = vec![
            vec![
                line("context\r\n", 4, 4, ChangeTag::Equal),
                line("old\n", 5, 5, ChangeTag::Delete),
                line("new", 6, 5, ChangeTag::Insert),
            ],
            vec![
                line("tail old", 20, 20, ChangeTag::Delete),
                line("tail new\n", 21, 20, ChangeTag::Insert),
            ],
        ];
        assert_eq!(
            diff_hunks_to_patch("src/example.rs", &hunks),
            concat!(
                "--- a/src/example.rs\n",
                "+++ b/src/example.rs\n",
                "@@ -4,2 +4,2 @@\n",
                " context\n",
                "-old\n",
                "+new\n",
                "@@ -20,1 +20,1 @@\n",
                "-tail old\n",
                "+tail new\n",
            ),
        );
    }

    #[test]
    fn empty_hunk_set_has_no_partial_patch_header() {
        assert_eq!(diff_hunks_to_patch("src/example.rs", &[]), "");
    }
}

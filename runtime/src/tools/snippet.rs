//! Shared rendering for edit-tool confirmations: locate the region an edit
//! changed and render it with line numbers, so the model can verify the result
//! without re-reading the whole file.

/// Lines of context rendered on each side of a changed region.
const CONTEXT_LINES: usize = 3;
/// Hard cap on a rendered snippet; a truncated snippet ends with a marker.
const MAX_SNIPPET_BYTES: usize = 1500;
/// Widest changed span worth rendering inline. [`changed_range`] returns one
/// span, so a `replace_all` hitting lines 2 and 199 reports (2, 199) — a
/// "snippet" that is almost entirely untouched context and whose byte cap would
/// cut off the far end of the change anyway. Past this width callers report the
/// changed line numbers instead, which is what the model needs to re-anchor.
const MAX_SNIPPET_LINES: usize = 40;

/// 1-based inclusive line range that differs between `old` and `new`, computed
/// as the span between their common byte prefix and common byte suffix.
pub fn changed_range(old: &str, new: &str) -> (usize, usize) {
    let mut prefix = 0;
    while prefix < old.len().min(new.len()) && old.as_bytes()[prefix] == new.as_bytes()[prefix] {
        prefix += 1;
    }
    while prefix > 0 && (!old.is_char_boundary(prefix) || !new.is_char_boundary(prefix)) {
        prefix -= 1;
    }
    let mut suffix = 0;
    let max_suffix = old.len().min(new.len()) - prefix;
    while suffix < max_suffix
        && old.as_bytes()[old.len() - 1 - suffix] == new.as_bytes()[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    while suffix > 0
        && (!old.is_char_boundary(old.len() - suffix) || !new.is_char_boundary(new.len() - suffix))
    {
        suffix -= 1;
    }
    let start = new[..prefix].matches('\n').count() + 1;
    // When the change ends exactly at a line boundary, the trailing newline
    // belongs to the last changed line, not the one after it.
    let mut pos = new.len() - suffix;
    if pos > prefix && new.as_bytes()[pos - 1] == b'\n' {
        pos -= 1;
    }
    let end = new[..pos].matches('\n').count() + 1;
    (start, end.max(start))
}

/// Render `start_line..=end_line` (1-based, clamped to the file) with
/// [`CONTEXT_LINES`] of slack each side and `N→ ` prefixes. Output is capped at
/// [`MAX_SNIPPET_BYTES`]; a cut snippet ends with a truncation marker.
///
/// `None` when there is nothing useful to render — an empty file, or a span
/// wider than [`MAX_SNIPPET_LINES`] (see there for why a wide span is worse
/// than no snippet at all).
pub fn numbered_window(content: &str, start_line: usize, end_line: usize) -> Option<String> {
    if end_line.saturating_sub(start_line) + 1 > MAX_SNIPPET_LINES {
        return None;
    }
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return None;
    }
    let lo = start_line.saturating_sub(CONTEXT_LINES + 1); // 0-based
    let hi = (end_line + CONTEXT_LINES).min(lines.len()); // exclusive
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate().take(hi).skip(lo) {
        let row = format!("{}→ {}\n", i + 1, line);
        if out.len() + row.len() > MAX_SNIPPET_BYTES {
            out.push_str("… (snippet truncated)");
            break;
        }
        out.push_str(&row);
    }
    Some(out.trim_end().to_string())
}

/// Render a list of 1-based line numbers for a confirmation message, e.g.
/// "2, 87, 199". Capped so a 400-site rename doesn't paste 400 numbers.
pub fn line_list(lines: &[usize]) -> String {
    const MAX_LISTED: usize = 10;
    let shown = lines
        .iter()
        .take(MAX_LISTED)
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    if lines.len() > MAX_LISTED {
        format!("{shown}, … ({} more)", lines.len() - MAX_LISTED)
    } else {
        shown
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    #[test]
    fn changed_range_single_line_edit() {
        let old = "a\nb\nc\n";
        let new = "a\nX\nc\n";
        assert_eq!(changed_range(old, new), (2, 2));
    }

    #[test]
    fn changed_range_insertion_widens() {
        let old = "a\nd\n";
        let new = "a\nb\nc\nd\n";
        assert_eq!(changed_range(old, new), (2, 3));
    }

    #[test]
    fn numbered_window_has_line_numbers_and_context() {
        let content = (1..=20).map(|i| format!("line{i}\n")).collect::<String>();
        let out = numbered_window(&content, 10, 11).unwrap();
        assert!(out.contains("7→ line7"), "context above: {out}");
        assert!(out.contains("10→ line10"), "changed line: {out}");
        assert!(out.contains("14→ line14"), "context below: {out}");
        assert!(!out.contains("6→ line6"), "no extra context: {out}");
    }

    #[test]
    fn numbered_window_clamps_to_file_edges() {
        let out = numbered_window("only\n", 1, 1).unwrap();
        assert_eq!(out, "1→ only");
    }

    #[test]
    fn numbered_window_declines_a_span_wider_than_the_cap() {
        let content = (1..=300).map(|i| format!("line{i}\n")).collect::<String>();
        // What a scattered replace_all produces: a span covering most of the file.
        assert_eq!(numbered_window(&content, 2, 299), None);
        assert!(numbered_window(&content, 2, 41).is_some(), "at the cap");
    }

    #[test]
    fn line_list_caps_long_runs() {
        assert_eq!(line_list(&[2, 199]), "2, 199");
        let many: Vec<usize> = (1..=15).collect();
        assert_eq!(
            line_list(&many),
            "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, … (5 more)"
        );
    }
}

//! Renders the memory index that rides in the session system prompt: one line
//! per memory, grouped by space. Bodies are never inlined -- the agent pulls
//! the ones it wants with `memory_load`. Pure and synchronous so it is cheap to
//! test; the DB read happens in the caller.

use crate::memory::{MAX_INDEX_ENTRIES, MemoryRow};
use std::fmt::Write as _;

/// Build the `# Memories` prompt section for a session's selected `spaces`.
/// Returns an empty string when the session selected no spaces at all -- in
/// that case the memory tools are not exposed either, so the section would be
/// noise. When spaces are selected but hold nothing, the section still renders:
/// the agent needs to know the facility exists before it can use it.
pub fn render_index(rows: &[MemoryRow], spaces: &[String]) -> String {
    if spaces.is_empty() {
        return String::new();
    }
    let mut out = String::from(
        "# Memories\n\nSaved notes from earlier sessions. Each line is one memory: \
         its address, then a one-line summary. Load the full text of the ones you \
         need with the memory_load tool before relying on them.\n",
    );
    if rows.is_empty() {
        let _ = write!(
            out,
            "\nNo memories saved yet. Writable spaces: {}.\n",
            spaces.join(", ")
        );
        return out;
    }

    let shown = rows.len().min(MAX_INDEX_ENTRIES);
    let mut current: Option<&str> = None;
    for row in rows.iter().take(shown) {
        if current != Some(row.space.as_str()) {
            let _ = write!(out, "\n## {}\n\n", row.space);
            current = Some(row.space.as_str());
        }
        let _ = writeln!(
            out,
            "- {}/{} — {}",
            row.space,
            row.name,
            one_line(&row.description)
        );
    }
    if rows.len() > shown {
        let _ = write!(
            out,
            "\n({} more memories not listed — use memory_list to see the rest.)\n",
            rows.len() - shown
        );
    }
    out
}

/// Collapse any whitespace run to a single space so one description can never
/// occupy more than one index line.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::render_index;
    use crate::memory::MemoryRow;

    fn row(space: &str, name: &str, description: &str) -> MemoryRow {
        MemoryRow {
            id: 1,
            space: space.into(),
            name: name.into(),
            description: description.into(),
            content: "body".into(),
            created_at: "1".into(),
            updated_at: "1".into(),
        }
    }

    #[test]
    fn groups_by_space_and_uses_qualified_addresses() {
        let rows = vec![
            row("default", "alpha", "first fact"),
            row("default", "beta", "second fact"),
            row("ops", "gamma", "third fact"),
        ];
        let out = render_index(&rows, &["default".into(), "ops".into()]);
        assert!(out.starts_with("# Memories\n"));
        assert!(out.contains("## default\n"));
        assert!(out.contains("- default/alpha — first fact\n"));
        assert!(out.contains("- default/beta — second fact\n"));
        assert!(out.contains("## ops\n"));
        assert!(out.contains("- ops/gamma — third fact\n"));
        assert!(
            out.contains("memory_load"),
            "must tell the agent how to read one"
        );
    }

    #[test]
    fn empty_spaces_still_render_so_the_agent_knows_memory_exists() {
        let out = render_index(&[], &["default".into()]);
        assert!(out.contains("# Memories"));
        assert!(out.contains("No memories saved yet"));
        assert!(out.contains("default"), "must name the writable spaces");
    }

    #[test]
    fn no_selected_spaces_renders_nothing() {
        assert_eq!(render_index(&[], &[]), "");
    }

    #[test]
    fn truncation_is_announced_not_silent() {
        let rows: Vec<MemoryRow> = (0..250)
            .map(|i| row("default", &format!("m{i:03}"), "d"))
            .collect();
        let out = render_index(&rows, &["default".into()]);
        assert!(out.contains("50 more memories not listed"));
        assert_eq!(out.matches("- default/m").count(), 200);
    }

    #[test]
    fn newlines_in_a_description_cannot_break_the_index_layout() {
        let rows = vec![row("default", "alpha", "line one\nline two")];
        let out = render_index(&rows, &["default".into()]);
        assert!(out.contains("- default/alpha — line one line two\n"));
    }
}

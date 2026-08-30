use horsie_models::runtime::{ReplaceLinesInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ReplaceLinesInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let trailing_newline = content.ends_with('\n');
        // `lines()` strips the `\r` from CRLF, so rejoining with "\n" would
        // silently convert the whole file to LF. Rejoin with what it uses.
        let sep = if content.contains("\r\n") {
            "\r\n"
        } else {
            "\n"
        };
        let mut lines: Vec<&str> = content.lines().collect();
        let start = (input.start_line as usize)
            .saturating_sub(1)
            .min(lines.len());
        // Clamp the end to at least `start` so an inverted range degrades to an
        // insertion rather than panicking on a reversed splice range.
        let end = (input.end_line as usize).min(lines.len()).max(start);
        let replacement_lines: Vec<&str> = input.replacement.lines().collect();
        lines.splice(start..end, replacement_lines);
        let mut new_content = lines.join(sep);
        if trailing_newline {
            new_content.push_str(sep);
        }
        std::fs::write(&path, &new_content).map_err(|e| e.to_string())?;
        let (s, e) = super::snippet::changed_range(&content, &new_content);
        let header = format!(
            "Replaced lines {}-{} in '{}'.",
            input.start_line, input.end_line, input.path
        );
        // A block edit too large to window usefully keeps the bare confirmation
        // — its header already names the range the model needs.
        Ok::<String, String>(match super::snippet::numbered_window(&new_content, s, e) {
            Some(window) => format!("{header}\n\n{window}"),
            None => header,
        })
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
            artifacts: Vec::new(),
        }),
        Ok(Err(reason)) => ToolResult::Err(ToolError { reason }),
        Err(e) => ToolResult::Err(ToolError {
            reason: e.to_string(),
        }),
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
    use tempfile::TempDir;

    #[tokio::test]
    async fn success_includes_numbered_snippet() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\ne\nf\ng\n").unwrap();
        let result = exec(
            dir.path(),
            ReplaceLinesInput {
                path: "f.txt".into(),
                start_line: 3,
                end_line: 4,
                replacement: "X\nY".into(),
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("Replaced lines 3-4"), "{}", o.stdout);
                assert!(o.stdout.contains("3→ X"), "{}", o.stdout);
                assert!(o.stdout.contains("4→ Y"), "{}", o.stdout);
                assert!(o.stdout.contains("1→ a"), "context above: {}", o.stdout);
                assert!(o.stdout.contains("7→ g"), "context below: {}", o.stdout);
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    /// `lines()` drops the `\r`, so rejoining naively rewrites every line ending
    /// in the file and reports the whole file as changed.
    #[tokio::test]
    async fn crlf_line_endings_survive_the_edit() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\r\nb\r\nc\r\n").unwrap();
        let result = exec(
            dir.path(),
            ReplaceLinesInput {
                path: "f.txt".into(),
                start_line: 2,
                end_line: 2,
                replacement: "X".into(),
            },
        )
        .await;
        match result {
            ToolResult::Ok(_) => {
                let after = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
                assert_eq!(after, "a\r\nX\r\nc\r\n");
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    /// Replacing a large block spans more lines than a window can usefully show,
    /// so the confirmation stands alone rather than pasting a truncated dump.
    #[tokio::test]
    async fn wide_block_edit_skips_the_snippet() {
        let dir = TempDir::new().unwrap();
        let body: String = (1..=200).map(|i| format!("line{i}\n")).collect();
        std::fs::write(dir.path().join("f.txt"), body).unwrap();
        let replacement: String = (1..=100)
            .map(|i| format!("new{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = exec(
            dir.path(),
            ReplaceLinesInput {
                path: "f.txt".into(),
                start_line: 10,
                end_line: 150,
                replacement,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert_eq!(o.stdout, "Replaced lines 10-150 in 'f.txt'.");
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn replaces_line_range() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd").unwrap();
        let result = exec(
            dir.path(),
            ReplaceLinesInput {
                path: "f.txt".into(),
                start_line: 2,
                end_line: 3,
                replacement: "X\nY".into(),
            },
        )
        .await;
        match result {
            ToolResult::Ok(_) => {
                let after = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
                assert_eq!(after, "a\nX\nY\nd");
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}

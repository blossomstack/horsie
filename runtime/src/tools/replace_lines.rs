use models::runtime::{ReplaceLinesInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ReplaceLinesInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let mut lines: Vec<&str> = content.lines().collect();
        let start = (input.start_line as usize)
            .saturating_sub(1)
            .min(lines.len());
        // Clamp the end to at least `start` so an inverted range degrades to an
        // insertion rather than panicking on a reversed splice range.
        let end = (input.end_line as usize).min(lines.len()).max(start);
        let replacement_lines: Vec<&str> = input.replacement.lines().collect();
        lines.splice(start..end, replacement_lines);
        std::fs::write(&path, lines.join("\n")).map_err(|e| e.to_string())?;
        Ok::<String, String>(format!(
            "Replaced lines {}-{} in '{}'.",
            input.start_line, input.end_line, input.path
        ))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
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
                workspace: None,
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

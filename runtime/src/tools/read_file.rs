use models::runtime::{ReadFileInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ReadFileInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        let result = match (input.start_line, input.end_line) {
            (Some(s), Some(e)) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = (s as usize).saturating_sub(1).min(lines.len());
                let end = (e as usize).min(lines.len());
                lines[start..end].join("\n")
            }
            (Some(s), None) => {
                let lines: Vec<&str> = content.lines().collect();
                let start = (s as usize).saturating_sub(1).min(lines.len());
                lines[start..].join("\n")
            }
            _ => content,
        };
        Ok::<String, String>(result)
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
    async fn read_full_file() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "line1\nline2\nline3").unwrap();
        let result = exec(
            dir.path(),
            ReadFileInput {
                path: "f.txt".into(),
                start_line: None,
                end_line: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "line1\nline2\nline3"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn read_line_range() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd").unwrap();
        let result = exec(
            dir.path(),
            ReadFileInput {
                path: "f.txt".into(),
                start_line: Some(2),
                end_line: Some(3),
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "b\nc"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}

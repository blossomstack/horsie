use models::runtime::{EditFileInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: EditFileInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let content = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
        if !content.contains(&input.old_text) {
            return Err(format!("old_text not found in '{}'", input.path));
        }
        let new_content = content.replacen(&input.old_text, &input.new_text, 1);
        std::fs::write(&path, new_content).map_err(|e| e.to_string())?;
        Ok::<String, String>(format!("Edited '{}'.", input.path))
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
    async fn edit_replaces_text() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello world").unwrap();
        let result = exec(
            dir.path(),
            EditFileInput {
                path: "f.txt".into(),
                old_text: "world".into(),
                new_text: "rust".into(),
            },
        )
        .await;
        assert!(matches!(result, ToolResult::Ok(_)));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
            "hello rust"
        );
    }

    #[tokio::test]
    async fn edit_returns_error_when_not_found() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("f.txt"), "hello").unwrap();
        let result = exec(
            dir.path(),
            EditFileInput {
                path: "f.txt".into(),
                old_text: "missing".into(),
                new_text: "x".into(),
            },
        )
        .await;
        assert!(matches!(result, ToolResult::Err(_)));
    }
}

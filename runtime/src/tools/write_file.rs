use models::runtime::{ToolError, ToolOutput, ToolResult, WriteFileInput};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: WriteFileInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, &input.content).map_err(|e| e.to_string())
    })
    .await
    {
        Ok(Ok(())) => ToolResult::Ok(ToolOutput {
            stdout: "File written.".into(),
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
    async fn write_creates_file() {
        let dir = TempDir::new().unwrap();
        exec(
            dir.path(),
            WriteFileInput {
                path: "out.txt".into(),
                content: "hello".into(),
            },
        )
        .await;
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "hello"
        );
    }

    #[tokio::test]
    async fn write_creates_parent_dirs() {
        let dir = TempDir::new().unwrap();
        exec(
            dir.path(),
            WriteFileInput {
                path: "a/b/c.txt".into(),
                content: "x".into(),
            },
        )
        .await;
        assert!(dir.path().join("a/b/c.txt").exists());
    }
}

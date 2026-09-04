use horsie_models::runtime::{ToolError, ToolOutput, ToolResult, WriteFileInput};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: WriteFileInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let lines = input.content.lines().count();
        let bytes = input.content.len();
        std::fs::write(&path, &input.content).map_err(|e| e.to_string())?;
        Ok::<String, String>(format!(
            "Wrote {lines} lines ({bytes} bytes) to '{}'.",
            input.path
        ))
    })
    .await
    {
        Ok(Ok(stdout)) => ToolResult::Ok(ToolOutput {
            stdout,
            stderr: String::new(),
            exit_code: 0,
            artifacts: Vec::new(),
            original_output_bytes: 0,
            spilled_output_bytes: 0,
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
    async fn confirmation_reports_counts() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            WriteFileInput {
                path: "out.txt".into(),
                content: "a\nb\nc\n".into(),
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("3 lines"), "{}", o.stdout);
                assert!(o.stdout.contains("6 bytes"), "{}", o.stdout);
                assert!(o.stdout.contains("out.txt"), "{}", o.stdout);
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

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

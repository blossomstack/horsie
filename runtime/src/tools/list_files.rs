use models::runtime::{ListFilesInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: ListFilesInput) -> ToolResult {
    let path = working_dir.join(&input.path);
    match tokio::task::spawn_blocking(move || {
        let entries = std::fs::read_dir(&path).map_err(|e| e.to_string())?;
        let mut lines = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            let meta = entry.metadata().map_err(|e| e.to_string())?;
            let kind = if meta.is_dir() { "d" } else { "f" };
            let name = entry.file_name().to_string_lossy().into_owned();
            lines.push(format!("{kind} {name}"));
        }
        lines.sort();
        Ok::<String, String>(lines.join("\n"))
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
    async fn list_files_shows_entries() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let result = exec(dir.path(), ListFilesInput { path: ".".into() }).await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.contains("a.txt"));
                assert!(o.stdout.contains("sub"));
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}

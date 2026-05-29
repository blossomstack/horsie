use models::runtime::{BashInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub async fn exec(working_dir: &Path, input: BashInput) -> ToolResult {
    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&input.command)
        .current_dir(working_dir)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    match child {
        Ok(child) => match child.wait_with_output().await {
            Ok(output) => ToolResult::Ok(ToolOutput {
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                exit_code: output.status.code().unwrap_or(-1),
            }),
            Err(e) => ToolResult::Err(ToolError {
                reason: e.to_string(),
            }),
        },
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
    async fn bash_echo() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            BashInput {
                command: "echo hello".to_string(),
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout.trim(), "hello"),
            ToolResult::Err(e) => panic!("unexpected error: {}", e.reason),
        }
    }

    #[tokio::test]
    async fn bash_nonzero_exit() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            BashInput {
                command: "exit 42".to_string(),
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.exit_code, 42),
            ToolResult::Err(e) => panic!("unexpected error: {}", e.reason),
        }
    }

    #[tokio::test]
    async fn bash_uses_working_dir() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("sentinel.txt"), "found").unwrap();
        let result = exec(
            dir.path(),
            BashInput {
                command: "cat sentinel.txt".to_string(),
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout.trim(), "found"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}

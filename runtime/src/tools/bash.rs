use horsie_models::runtime::{BashInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;
use std::time::Duration;

/// Wall-clock limit applied when the caller does not specify one. Bounds runaway
/// or hung commands (e.g. waiting on stdin) so a single tool call cannot stall the
/// agent forever.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

pub async fn exec(working_dir: &Path, input: BashInput) -> ToolResult {
    let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    let child = tokio::process::Command::new("bash")
        .arg("-c")
        .arg(&input.command)
        .current_dir(working_dir)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let child = match child {
        Ok(child) => child,
        Err(e) => {
            return ToolResult::Err(ToolError {
                reason: e.to_string(),
            });
        }
    };

    // On timeout the `wait_with_output` future is dropped, which drops the child;
    // `kill_on_drop(true)` then sends SIGKILL so the process cannot linger.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => ToolResult::Ok(ToolOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.status.code().unwrap_or(-1),
        }),
        Ok(Err(e)) => ToolResult::Err(ToolError {
            reason: e.to_string(),
        }),
        Err(_elapsed) => ToolResult::Err(ToolError {
            reason: format!("command timed out after {}s", timeout.as_secs()),
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
                timeout_secs: None,
                workspace: None,
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
                timeout_secs: None,
                workspace: None,
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
                timeout_secs: None,
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout.trim(), "found"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn bash_times_out() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            BashInput {
                command: "sleep 5".to_string(),
                timeout_secs: Some(1),
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => panic!("expected timeout, got exit {}", o.exit_code),
            ToolResult::Err(e) => assert!(
                e.reason.contains("timed out"),
                "unexpected error: {}",
                e.reason
            ),
        }
    }
}

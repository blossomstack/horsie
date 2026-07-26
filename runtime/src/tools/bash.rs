use horsie_models::runtime::{BashInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Wall-clock limit applied when the caller does not specify one. Bounds runaway
/// or hung commands (e.g. waiting on stdin) so a single tool call cannot stall the
/// agent forever.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Cap on the partial output included in a timeout error. Tail-biased: the end
/// of the log is where a hang usually shows. Error strings are not truncated by
/// the dispatcher, so the cap must live here.
const MAX_PARTIAL_BYTES: usize = 10_000;

pub async fn exec(working_dir: &Path, input: BashInput) -> ToolResult {
    let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    // pipefail: a failing stage anywhere in a pipeline fails the command, so
    // `cargo test 2>&1 | tail` can't mask a test failure behind tail's exit 0.
    let child = tokio::process::Command::new("bash")
        .arg("-o")
        .arg("pipefail")
        .arg("-c")
        .arg(&input.command)
        .current_dir(working_dir)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            return ToolResult::Err(ToolError {
                reason: e.to_string(),
            });
        }
    };

    // Drain both streams as they arrive so a timeout can report what the
    // command produced before it was killed — the difference between
    // "still compiling" and "genuinely hung" for the agent.
    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    let drain = |buf: &Arc<Mutex<Vec<u8>>>| {
        String::from_utf8_lossy(&buf.lock().expect("buffer poisoned")).into_owned()
    };
    let mut readers = Vec::new();
    {
        let mut pipe = child.stdout.take().expect("stdout is piped");
        let buf = Arc::clone(&stdout_buf);
        readers.push(tokio::spawn(async move {
            drain_into(&mut pipe, &buf).await;
        }));
    }
    {
        let mut pipe = child.stderr.take().expect("stderr is piped");
        let buf = Arc::clone(&stderr_buf);
        readers.push(tokio::spawn(async move {
            drain_into(&mut pipe, &buf).await;
        }));
    }

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            for r in readers {
                let _ = r.await;
            }
            ToolResult::Ok(ToolOutput {
                stdout: drain(&stdout_buf),
                stderr: drain(&stderr_buf),
                exit_code: status.code().unwrap_or(-1),
            })
        }
        Ok(Err(e)) => ToolResult::Err(ToolError {
            reason: e.to_string(),
        }),
        Err(_elapsed) => {
            // Kill, reap, then collect whatever the readers captured.
            let _ = child.kill().await;
            let _ = child.wait().await;
            for r in readers {
                let _ = r.await;
            }
            let mut reason = format!("command timed out after {}s", timeout.as_secs());
            let captured = format!("{}{}", drain(&stdout_buf), drain(&stderr_buf));
            let tail = tail_str(&captured, MAX_PARTIAL_BYTES);
            if !tail.trim().is_empty() {
                reason.push_str(&format!("\n--- captured output before timeout ---\n{tail}"));
            }
            ToolResult::Err(ToolError { reason })
        }
    }
}

/// Copy a child output stream into `buf` until EOF.
async fn drain_into<R>(pipe: &mut R, buf: &Arc<Mutex<Vec<u8>>>)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0u8; 8192];
    while let Ok(n) = pipe.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        buf.lock().expect("buffer poisoned").extend_from_slice(&chunk[..n]);
    }
}

/// The last `max` bytes of `s`, nudged to a UTF-8 char boundary.
fn tail_str(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
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
    async fn timeout_returns_partial_output() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            BashInput {
                command: "echo before-timeout; sleep 5".to_string(),
                timeout_secs: Some(1),
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => panic!("expected timeout, got exit {}", o.exit_code),
            ToolResult::Err(e) => {
                assert!(e.reason.contains("timed out"), "{}", e.reason);
                assert!(e.reason.contains("before-timeout"), "{}", e.reason);
            }
        }
    }

    #[tokio::test]
    async fn pipefail_surfaces_mid_pipe_failure() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            BashInput {
                command: "false | true".to_string(),
                timeout_secs: None,
                workspace: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.exit_code, 1, "pipefail should fail the pipeline"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

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

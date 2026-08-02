use crate::state::EnvOverlay;
use horsie_models::runtime::{BashInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Wall-clock limit applied when the caller does not specify one. Bounds runaway
/// or hung commands (e.g. waiting on stdin) so a single tool call cannot stall the
/// agent forever.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Cap on the partial output included in a timeout error, per stream. Both
/// streams are reported, so a command that logs heavily to stderr can't push
/// stdout out of the window entirely. Tail-biased: the end of the log is where a
/// hang usually shows. Error strings are not truncated by the dispatcher, so the
/// cap must live here.
const MAX_PARTIAL_BYTES_PER_STREAM: usize = 5_000;

/// How long to keep waiting for the output pipes to reach EOF once the child is
/// gone. A backgrounded grandchild inherits those pipes and can hold them open
/// indefinitely (`npm run dev &`), so this wait must be bounded or `timeout_secs`
/// stops meaning anything.
const READER_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// A pipeline stage killed by SIGPIPE exits 128+13. Under `pipefail` that
/// becomes the pipeline's status, which would report the everyday `… | head` as
/// a failed command. An early-closing consumer is the point of `head`, not an
/// error, so it is normalized to success.
const SIGPIPE_EXIT: i32 = 141;

pub async fn exec(working_dir: &Path, env: &EnvOverlay, input: BashInput) -> ToolResult {
    let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
    // pipefail: a failing stage anywhere in a pipeline fails the command, so
    // `cargo test 2>&1 | tail` can't mask a test failure behind tail's exit 0.
    let mut command = tokio::process::Command::new("bash");
    command
        .arg("-o")
        .arg("pipefail")
        .arg("-c")
        .arg(&input.command)
        .current_dir(working_dir)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Give the command its own process group so a timeout can signal the whole
    // tree. Killing just bash leaves its children running and still holding the
    // output pipes open.
    #[cfg(unix)]
    command.process_group(0);
    env.apply_to(&mut command);
    let child = command.spawn();

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
    let pid = child.id();
    let stdout_buf = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf = Arc::new(Mutex::new(Vec::new()));
    let snapshot = |buf: &Arc<Mutex<Vec<u8>>>| {
        // A poisoned buffer still holds valid bytes; keep them.
        let guard = buf.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&guard).into_owned()
    };
    let mut readers = Vec::new();
    let (Some(out_pipe), Some(err_pipe)) = (child.stdout.take(), child.stderr.take()) else {
        return ToolResult::Err(ToolError {
            reason: "failed to capture child output pipes".into(),
        });
    };
    // ChildStdout/ChildStderr are distinct types; box them to treat uniformly.
    let pipes: [(Box<dyn tokio::io::AsyncRead + Unpin + Send>, _); 2] = [
        (Box::new(out_pipe), Arc::clone(&stdout_buf)),
        (Box::new(err_pipe), Arc::clone(&stderr_buf)),
    ];
    for (mut pipe, buf) in pipes {
        readers.push(tokio::spawn(async move {
            drain_into(&mut pipe, &buf).await;
        }));
    }

    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => {
            finish_readers(readers).await;
            ToolResult::Ok(ToolOutput {
                stdout: snapshot(&stdout_buf),
                stderr: snapshot(&stderr_buf),
                exit_code: match status.code() {
                    Some(SIGPIPE_EXIT) => 0,
                    Some(code) => code,
                    None => -1,
                },
            })
        }
        Ok(Err(e)) => {
            finish_readers(readers).await;
            ToolResult::Err(ToolError {
                reason: e.to_string(),
            })
        }
        Err(_elapsed) => {
            // Kill the whole group, reap, then collect what the readers caught.
            kill_group(pid);
            let _ = child.kill().await;
            let _ = child.wait().await;
            finish_readers(readers).await;
            let mut reason = format!("command timed out after {}s", timeout.as_secs());
            for (label, buf) in [("stdout", &stdout_buf), ("stderr", &stderr_buf)] {
                let captured = snapshot(buf);
                let tail = tail_str(&captured, MAX_PARTIAL_BYTES_PER_STREAM);
                if !tail.trim().is_empty() {
                    reason.push_str(&format!("\n--- {label} before timeout (tail) ---\n{tail}"));
                }
            }
            ToolResult::Err(ToolError { reason })
        }
    }
}

/// SIGKILL the child's process group, so children it backgrounded die with it
/// rather than lingering — still running, still holding the output pipes.
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: `pid` is our own child, spawned with `process_group(0)`, so the
        // group id equals its pid and contains only its descendants. A reaped
        // child yields ESRCH, which is ignored.
        unsafe {
            libc::killpg(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Wait — briefly — for the drain tasks to see EOF, then drop them.
///
/// EOF arrives only once every holder of the pipe's write end has closed it, and
/// a backgrounded grandchild inherits that end. Waiting unbounded here would let
/// `sleep 30 &` outlast a 3s `timeout_secs`, and a detached daemon would pin the
/// call (and grow the buffer) forever. Aborting also stops leaking a task per
/// timed-out command.
async fn finish_readers(mut readers: Vec<tokio::task::JoinHandle<()>>) {
    let _ = tokio::time::timeout(READER_DRAIN_GRACE, async {
        for r in readers.iter_mut() {
            let _ = r.await;
        }
    })
    .await;
    for r in &readers {
        r.abort();
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
        // A poisoned buffer still holds valid bytes; keep appending.
        buf.lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(&chunk[..n]);
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
            &EnvOverlay::default(),
            BashInput {
                command: "echo before-timeout; sleep 5".to_string(),
                timeout_secs: Some(1),
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

    /// A backgrounded child inherits stdout/stderr, so the pipes stay open long
    /// after bash itself exits. The drain wait must not follow them, or
    /// `timeout_secs` silently becomes "however long the grandchild lives".
    #[tokio::test]
    async fn backgrounded_child_does_not_outlive_the_call() {
        let dir = TempDir::new().unwrap();
        let started = std::time::Instant::now();
        let result = exec(
            dir.path(),
            &EnvOverlay::default(),
            BashInput {
                command: "sleep 30 & echo started".to_string(),
                timeout_secs: Some(1),
            },
        )
        .await;
        let elapsed = started.elapsed();
        assert!(elapsed.as_secs() < 10, "call blocked for {elapsed:?}");
        match result {
            ToolResult::Ok(o) => assert!(o.stdout.contains("started"), "{}", o.stdout),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    /// Same hazard on the timeout path: killing bash alone leaves the
    /// grandchild running and holding the pipes.
    #[tokio::test]
    async fn timeout_with_backgrounded_child_still_returns() {
        let dir = TempDir::new().unwrap();
        let started = std::time::Instant::now();
        let result = exec(
            dir.path(),
            &EnvOverlay::default(),
            BashInput {
                command: "sleep 60 & echo started; sleep 30".to_string(),
                timeout_secs: Some(1),
            },
        )
        .await;
        let elapsed = started.elapsed();
        assert!(elapsed.as_secs() < 10, "call blocked for {elapsed:?}");
        match result {
            ToolResult::Ok(o) => panic!("expected timeout, got exit {}", o.exit_code),
            ToolResult::Err(e) => assert!(e.reason.contains("timed out"), "{}", e.reason),
        }
    }

    /// Both streams get their own slice of the timeout report, so a chatty
    /// stderr can't evict stdout from the window entirely.
    #[tokio::test]
    async fn timeout_reports_both_streams() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            &EnvOverlay::default(),
            BashInput {
                command: "echo to-stdout; echo to-stderr >&2; sleep 5".to_string(),
                timeout_secs: Some(1),
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => panic!("expected timeout, got exit {}", o.exit_code),
            ToolResult::Err(e) => {
                assert!(e.reason.contains("to-stdout"), "{}", e.reason);
                assert!(e.reason.contains("to-stderr"), "{}", e.reason);
            }
        }
    }

    /// `… | head` kills the producer with SIGPIPE. Under pipefail that is the
    /// pipeline's status, and it must not read as a failed command.
    #[tokio::test]
    async fn sigpipe_from_head_is_not_a_failure() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            &EnvOverlay::default(),
            BashInput {
                command: "seq 1 200000 | head -3".to_string(),
                timeout_secs: Some(30),
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert_eq!(o.exit_code, 0, "SIGPIPE treated as failure: {}", o.stderr);
                assert_eq!(o.stdout, "1\n2\n3\n");
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn pipefail_surfaces_mid_pipe_failure() {
        let dir = TempDir::new().unwrap();
        let result = exec(
            dir.path(),
            &EnvOverlay::default(),
            BashInput {
                command: "false | true".to_string(),
                timeout_secs: None,
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
            &EnvOverlay::default(),
            BashInput {
                command: "echo hello".to_string(),
                timeout_secs: None,
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
            &EnvOverlay::default(),
            BashInput {
                command: "exit 42".to_string(),
                timeout_secs: None,
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
            &EnvOverlay::default(),
            BashInput {
                command: "cat sentinel.txt".to_string(),
                timeout_secs: None,
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
            &EnvOverlay::default(),
            BashInput {
                command: "sleep 5".to_string(),
                timeout_secs: Some(1),
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

    #[tokio::test]
    async fn env_overlay_reaches_the_child() {
        let dir = TempDir::new().unwrap();
        let overlay = EnvOverlay {
            sets: vec![("HORSIE_TEST_VAR".to_string(), "hello".to_string())],
            unsets: vec![],
        };
        let result = exec(
            dir.path(),
            &overlay,
            BashInput {
                command: "echo $HORSIE_TEST_VAR".to_string(),
                timeout_secs: None,
            },
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout.trim(), "hello"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}

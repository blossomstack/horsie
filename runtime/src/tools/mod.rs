pub mod bash;
pub mod find_and_replace;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod read_file;
pub mod replace_lines;
pub mod write_file;

use models::runtime::{ToolCall, ToolResult};
use std::path::Path;

/// Per-stream output budget. Tool output rides along in the agent's conversation
/// history and is re-sent to the model on every turn, so an unbounded `cat`, build
/// log, or test run would otherwise blow the context window and token budget. The
/// cap is enforced here, in the one place every tool result flows through, so it
/// holds regardless of which tool produced the output.
const MAX_STREAM_BYTES: usize = 50_000;

pub async fn dispatch(working_dir: &Path, call: ToolCall) -> ToolResult {
    let result = match call {
        ToolCall::Bash(input) => bash::exec(working_dir, input).await,
        ToolCall::ReadFile(input) => read_file::exec(working_dir, input).await,
        ToolCall::WriteFile(input) => write_file::exec(working_dir, input).await,
        ToolCall::FindAndReplace(input) => find_and_replace::exec(working_dir, input).await,
        ToolCall::ReplaceLines(input) => replace_lines::exec(working_dir, input).await,
        ToolCall::ListFiles(input) => list_files::exec(working_dir, input).await,
        ToolCall::Glob(input) => glob::exec(working_dir, input).await,
        ToolCall::Grep(input) => grep::exec(working_dir, input).await,
    };

    match result {
        ToolResult::Ok(mut output) => {
            output.stdout = truncate_stream(output.stdout);
            output.stderr = truncate_stream(output.stderr);
            ToolResult::Ok(output)
        }
        ToolResult::Err(e) => ToolResult::Err(e),
    }
}

/// Clamp a single output stream to [`MAX_STREAM_BYTES`], keeping the head and tail
/// (where the signal usually lives) and replacing the middle with a marker noting
/// how much was dropped. Slices are nudged to UTF-8 char boundaries.
fn truncate_stream(s: String) -> String {
    if s.len() <= MAX_STREAM_BYTES {
        return s;
    }
    let keep = MAX_STREAM_BYTES / 2;

    let mut head_end = keep.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len().saturating_sub(keep);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start.saturating_sub(head_end);

    format!(
        "{}\n[... {omitted} bytes truncated ...]\n{}",
        &s[..head_end],
        &s[tail_start..]
    )
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
    use models::runtime::BashInput;
    use tempfile::TempDir;

    #[test]
    fn short_output_is_unchanged() {
        let s = "hello world".to_string();
        assert_eq!(truncate_stream(s.clone()), s);
    }

    #[test]
    fn long_output_is_truncated_with_marker() {
        let s = "x".repeat(MAX_STREAM_BYTES * 2);
        let out = truncate_stream(s);
        assert!(out.len() < MAX_STREAM_BYTES + 100, "len was {}", out.len());
        assert!(out.contains("bytes truncated"));
        assert!(out.starts_with('x'));
        assert!(out.ends_with('x'));
    }

    #[tokio::test]
    async fn dispatch_truncates_large_bash_output() {
        let dir = TempDir::new().unwrap();
        // 80 KB of 'a' on stdout, well over the cap.
        let result = dispatch(
            dir.path(),
            ToolCall::Bash(BashInput {
                command: "head -c 80000 < /dev/zero | tr '\\0' a".to_string(),
                timeout_secs: None,
            }),
        )
        .await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.len() < MAX_STREAM_BYTES + 100, "not truncated");
                assert!(o.stdout.contains("bytes truncated"));
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}

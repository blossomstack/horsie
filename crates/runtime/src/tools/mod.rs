pub mod apply_patch;
pub mod bash;
pub mod find_and_replace;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod pull_request;
pub mod read_file;
pub mod read_image;
pub mod replace_lines;
pub mod set_env;
pub mod set_working_dir;
pub(crate) mod snippet;
pub(crate) mod spill;
pub mod write_file;

use crate::state::RuntimeState;
use crate::workspace::WorkspaceRegistry;
use horsie_models::runtime::{ToolCall, ToolError, ToolResult};

/// Per-stream output budget. Tool output rides along in the agent's conversation
/// history and is re-sent to the model on every turn, so an unbounded `cat`, build
/// log, or test run would otherwise blow the context window and token budget. The
/// cap is enforced here, in the one place every tool result flows through, so it
/// holds regardless of which tool produced the output.
const MAX_OUTPUT_BYTES: usize = 50_000;

/// Run a tool call, then clamp its output.
///
/// The two state-mutating tools act on the agent's own state. Every other tool
/// runs in the agent's working directory — its `set_working_dir` override if it
/// has one, else the first workspace. Relative paths join onto that; an absolute
/// path in the call replaces it outright (`Path::join` discards the base), which
/// is how a call reaches another workspace or the shared plugin library. That is
/// the only addressing mechanism, so there is no precedence to arbitrate.
///
/// Spelled out one arm per tool rather than routed through a shared helper: the
/// alternative needs either a wildcard arm (lint-denied) or a nested match with
/// an unreachable branch, and neither is worth the saved lines.
pub async fn dispatch(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    agent: &str,
    call: ToolCall,
) -> ToolResult {
    // Resolved once, for every tool that needs it: the state tools take no base
    // directory, so a session with no workspaces can still set an env var.
    let dir = registry
        .default_root()
        .map(|root| state.effective_dir(agent, &root));
    match call {
        ToolCall::SetWorkingDir(i) => set_working_dir::exec(registry, state, agent, i),
        ToolCall::SetEnv(i) => set_env::exec(state, agent, i),
        ToolCall::Bash(i) => match dir {
            Ok(d) => bash::exec(&d, &state.env_overlay(agent), i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::ReadFile(i) => match dir {
            Ok(d) => read_file::exec(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::ReadImage(i) => match dir {
            Ok(d) => read_image::exec(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::WriteFile(i) => match dir {
            Ok(d) => write_file::exec(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::ApplyPatch(i) => match dir {
            Ok(d) => apply_patch::exec(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::FindAndReplace(i) => match dir {
            Ok(d) => find_and_replace::exec(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::ReplaceLines(i) => match dir {
            Ok(d) => replace_lines::exec(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::ListFiles(i) => match dir {
            Ok(d) => list_files::exec(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::Glob(i) => match dir {
            Ok(d) => glob::exec(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::Grep(i) => match dir {
            Ok(d) => grep::exec(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::InspectPullRequest(i) => match dir {
            Ok(d) => pull_request::inspect(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
        ToolCall::InspectPullRequestDiff(i) => match dir {
            Ok(d) => pull_request::inspect_diff(&d, i).await,
            Err(reason) => ToolResult::Err(ToolError { reason }),
        },
    }
}

/// Bound the final result after hooks have had a chance to rewrite it.
pub(crate) async fn clamp_result(
    state: &RuntimeState,
    agent: &str,
    call_id: &str,
    result: ToolResult,
) -> ToolResult {
    match result {
        ToolResult::Ok(output) => ToolResult::Ok(clamp_output(state, agent, call_id, output).await),
        ToolResult::Err(error) => ToolResult::Err(error),
    }
}

/// Clamp a single output stream to [`MAX_STREAM_BYTES`], keeping the head and tail
/// (where the signal usually lives) and replacing the middle with a marker noting
/// how much was dropped. Slices are nudged to UTF-8 char boundaries.
async fn clamp_output(
    state: &RuntimeState,
    agent: &str,
    call_id: &str,
    mut output: horsie_models::runtime::ToolOutput,
) -> horsie_models::runtime::ToolOutput {
    let original_bytes = output.stdout.len().saturating_add(output.stderr.len());
    output.original_output_bytes = u64::try_from(original_bytes).unwrap_or(u64::MAX);
    if original_bytes <= MAX_OUTPUT_BYTES {
        return output;
    }
    let body = format!(
        "--- stdout ---\n{}\n--- stderr ---\n{}",
        output.stdout, output.stderr
    );
    let spill = match &state.spills {
        Some(store) => store.preserve(agent, call_id, body.as_bytes()).await,
        None => None,
    };
    output.spilled_output_bytes = spill.as_ref().map_or(0, |(_, bytes)| *bytes);
    let spill_path = spill.as_ref().map(|(path, _)| path.as_str());
    let (stdout_budget, stderr_budget) = stream_budgets(output.stdout.len(), output.stderr.len());
    output.stdout = truncate_stream(output.stdout, stdout_budget, spill_path);
    output.stderr = truncate_stream(output.stderr, stderr_budget, spill_path);
    output
}

fn safe_name(raw: &str) -> String {
    let value: String = raw
        .chars()
        .take(80)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if value.is_empty() {
        "output".to_string()
    } else {
        value
    }
}

fn stream_budgets(stdout: usize, stderr: usize) -> (usize, usize) {
    let mut stdout_budget = stdout.min(MAX_OUTPUT_BYTES / 2);
    let mut stderr_budget = stderr.min(MAX_OUTPUT_BYTES / 2);
    let remaining = MAX_OUTPUT_BYTES.saturating_sub(stdout_budget + stderr_budget);
    let stdout_extra = stdout.saturating_sub(stdout_budget).min(remaining);
    stdout_budget += stdout_extra;
    stderr_budget += stderr
        .saturating_sub(stderr_budget)
        .min(remaining.saturating_sub(stdout_extra));
    (stdout_budget, stderr_budget)
}

/// Clamp one stream, preserving the full command output in a temporary file.
fn truncate_stream(s: String, budget: usize, spill: Option<&str>) -> String {
    if s.len() <= budget {
        return s;
    }
    let keep = budget / 2;

    let mut head_end = keep.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = s.len().saturating_sub(keep);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let omitted = tail_start.saturating_sub(head_end);
    let location = spill
        .map(|path| format!("; full output: {path}"))
        .unwrap_or_else(|| "; full output could not be saved".to_string());

    format!(
        "{}\n[... {omitted} bytes truncated{location} ...]\n{}",
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
    use horsie_models::Workspace;
    use horsie_models::runtime::BashInput;
    use tempfile::TempDir;

    #[test]
    fn short_output_is_unchanged() {
        let s = "hello world".to_string();
        assert_eq!(truncate_stream(s.clone(), MAX_OUTPUT_BYTES, None), s);
    }

    #[test]
    fn two_busy_streams_share_one_output_budget() {
        let (stdout, stderr) = stream_budgets(MAX_OUTPUT_BYTES, MAX_OUTPUT_BYTES);
        assert_eq!(stdout + stderr, MAX_OUTPUT_BYTES);
        assert_eq!(stdout, stderr);
    }

    #[test]
    fn long_output_is_truncated_with_marker() {
        let s = "x".repeat(MAX_OUTPUT_BYTES * 2);
        let out = truncate_stream(s, MAX_OUTPUT_BYTES, None);
        assert!(out.len() < MAX_OUTPUT_BYTES + 100, "len was {}", out.len());
        assert!(out.contains("bytes truncated"));
        assert!(out.starts_with('x'));
        assert!(out.ends_with('x'));
    }

    #[tokio::test]
    async fn dispatch_truncates_large_bash_output() {
        let dir = TempDir::new().unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "ws".into(),
            path: dir.path().to_path_buf(),
        }]);
        // 80 KB of 'a' on stdout, well over the cap.
        let result = dispatch(
            &registry,
            &RuntimeState::new(),
            "a",
            ToolCall::Bash(BashInput {
                command: "head -c 80000 < /dev/zero | tr '\\0' a".to_string(),
                timeout_secs: None,
            }),
        )
        .await;
        let state = RuntimeState::new();
        let spill_root = state.spills.as_ref().unwrap().root.clone();
        let result = clamp_result(&state, "a", "call-1", result).await;
        match result {
            ToolResult::Ok(o) => {
                assert!(o.stdout.len() < MAX_OUTPUT_BYTES + 200, "not truncated");
                assert!(o.stdout.contains("bytes truncated"));
                assert!(o.stdout.contains("/a/call-1.txt"));
                assert_eq!(o.original_output_bytes, 80_000);
                assert!(o.spilled_output_bytes > 80_000);
                let saved = std::fs::read_to_string(spill_root.join("a/call-1.txt")).unwrap();
                assert!(saved.len() > 80_000);
                assert!(saved.contains("--- stdout ---"));
            }
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    /// Several workspaces is no longer ambiguous: the call lands in the first,
    /// where it used to be refused because the model had named none.
    #[tokio::test]
    async fn dispatch_defaults_to_the_first_workspace() {
        let first = TempDir::new().unwrap();
        let second = TempDir::new().unwrap();
        std::fs::write(first.path().join("marker.txt"), "first").unwrap();
        let registry = WorkspaceRegistry::new(vec![
            Workspace {
                name: "a".into(),
                path: first.path().to_path_buf(),
            },
            Workspace {
                name: "b".into(),
                path: second.path().to_path_buf(),
            },
        ]);
        let result = dispatch(
            &registry,
            &RuntimeState::new(),
            "agent-1",
            ToolCall::ReadFile(horsie_models::runtime::ReadFileInput {
                path: "marker.txt".into(),
                start_line: None,
                end_line: None,
            }),
        )
        .await;
        match result {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "first"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn dispatch_errors_with_no_workspaces() {
        let result = dispatch(
            &WorkspaceRegistry::new(vec![]),
            &RuntimeState::new(),
            "a",
            ToolCall::Bash(BashInput {
                command: "echo hi".to_string(),
                timeout_secs: None,
            }),
        )
        .await;
        assert!(matches!(result, ToolResult::Err(_)));
    }

    #[tokio::test]
    async fn file_tools_follow_the_session_cwd() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/f.txt"), "nested").unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "ws".into(),
            path: dir.path().to_path_buf(),
        }]);
        let state = RuntimeState::new();
        let r = dispatch(
            &registry,
            &state,
            "a",
            ToolCall::SetWorkingDir(horsie_models::runtime::SetWorkingDirInput {
                path: Some("sub".into()),
            }),
        )
        .await;
        assert!(matches!(r, ToolResult::Ok(_)));
        let r = dispatch(
            &registry,
            &state,
            "a",
            ToolCall::ReadFile(horsie_models::runtime::ReadFileInput {
                path: "f.txt".into(),
                start_line: None,
                end_line: None,
            }),
        )
        .await;
        match r {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "nested"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    /// An absolute path must win over the sticky cwd. This is the same scenario
    /// that once made an explicit `workspace` argument necessary: with a cwd set
    /// in workspace `a`, a call meant for `b` read — and `write_file` would have
    /// written — inside `a`, silently, with a plausible result and nothing in the
    /// output to reveal it. `Path::join` discarding the base is what makes the
    /// bug unrepresentable now.
    #[tokio::test]
    async fn an_absolute_path_is_not_hijacked_by_the_sticky_cwd() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        std::fs::create_dir(a.path().join("sub")).unwrap();
        // Same filename in both workspaces, different contents: reading the
        // wrong one succeeds, which is exactly what makes the bug silent.
        std::fs::write(a.path().join("sub/shared.txt"), "from a").unwrap();
        std::fs::write(b.path().join("shared.txt"), "from b").unwrap();
        let registry = WorkspaceRegistry::new(vec![
            Workspace {
                name: "a".into(),
                path: a.path().to_path_buf(),
            },
            Workspace {
                name: "b".into(),
                path: b.path().to_path_buf(),
            },
        ]);
        let state = RuntimeState::new();

        let r = dispatch(
            &registry,
            &state,
            "agent-1",
            ToolCall::SetWorkingDir(horsie_models::runtime::SetWorkingDirInput {
                path: Some("sub".into()),
            }),
        )
        .await;
        assert!(matches!(r, ToolResult::Ok(_)));

        let r = dispatch(
            &registry,
            &state,
            "agent-1",
            ToolCall::ReadFile(horsie_models::runtime::ReadFileInput {
                path: b.path().join("shared.txt").display().to_string(),
                start_line: None,
                end_line: None,
            }),
        )
        .await;
        match r {
            ToolResult::Ok(o) => assert_eq!(
                o.stdout, "from b",
                "an absolute path into 'b' must read from 'b', not from the cwd set in 'a'"
            ),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    #[tokio::test]
    async fn cwd_overrides_are_isolated_per_agent() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("root.txt"), "at root").unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "ws".into(),
            path: dir.path().to_path_buf(),
        }]);
        let state = RuntimeState::new();
        let r = dispatch(
            &registry,
            &state,
            "a",
            ToolCall::SetWorkingDir(horsie_models::runtime::SetWorkingDirInput {
                path: Some("sub".into()),
            }),
        )
        .await;
        assert!(matches!(r, ToolResult::Ok(_)));
        // Agent b still resolves relative paths against the workspace root: it
        // can read a file that agent a's cwd (sub/) does not contain.
        let r = dispatch(
            &registry,
            &state,
            "b",
            ToolCall::ReadFile(horsie_models::runtime::ReadFileInput {
                path: "root.txt".into(),
                start_line: None,
                end_line: None,
            }),
        )
        .await;
        match r {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "at root"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}

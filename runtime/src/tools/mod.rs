pub mod bash;
pub mod find_and_replace;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod read_file;
pub mod replace_lines;
pub mod set_env;
pub mod set_working_dir;
pub(crate) mod snippet;
pub mod write_file;

use crate::state::RuntimeState;
use crate::workspace::WorkspaceRegistry;
use horsie_models::runtime::{ToolCall, ToolError, ToolResult};
use std::path::PathBuf;

/// Per-stream output budget. Tool output rides along in the agent's conversation
/// history and is re-sent to the model on every turn, so an unbounded `cat`, build
/// log, or test run would otherwise blow the context window and token budget. The
/// cap is enforced here, in the one place every tool result flows through, so it
/// holds regardless of which tool produced the output.
const MAX_STREAM_BYTES: usize = 50_000;

/// Run a tool call, then clamp its output.
///
/// The two state-mutating tools act on the agent's own state. Every other tool
/// resolves a base directory first — the single name→path translation site —
/// and an unresolvable `workspace` (missing with several workspaces, or an
/// unknown name) comes back to the model as a `ToolError`.
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
    let result = match call {
        ToolCall::SetWorkingDir(i) => set_working_dir::exec(registry, state, agent, i),
        ToolCall::SetEnv(i) => set_env::exec(state, agent, i),
        ToolCall::Bash(i) => match base(registry, state, agent, &i.workspace) {
            Ok(dir) => bash::exec(&dir, &state.env_overlay(agent), i).await,
            Err(reason) => return ToolResult::Err(ToolError { reason }),
        },
        ToolCall::ReadFile(i) => match base(registry, state, agent, &i.workspace) {
            Ok(dir) => read_file::exec(&dir, i).await,
            Err(reason) => return ToolResult::Err(ToolError { reason }),
        },
        ToolCall::WriteFile(i) => match base(registry, state, agent, &i.workspace) {
            Ok(dir) => write_file::exec(&dir, i).await,
            Err(reason) => return ToolResult::Err(ToolError { reason }),
        },
        ToolCall::FindAndReplace(i) => match base(registry, state, agent, &i.workspace) {
            Ok(dir) => find_and_replace::exec(&dir, i).await,
            Err(reason) => return ToolResult::Err(ToolError { reason }),
        },
        ToolCall::ReplaceLines(i) => match base(registry, state, agent, &i.workspace) {
            Ok(dir) => replace_lines::exec(&dir, i).await,
            Err(reason) => return ToolResult::Err(ToolError { reason }),
        },
        ToolCall::ListFiles(i) => match base(registry, state, agent, &i.workspace) {
            Ok(dir) => list_files::exec(&dir, i).await,
            Err(reason) => return ToolResult::Err(ToolError { reason }),
        },
        ToolCall::Glob(i) => match base(registry, state, agent, &i.workspace) {
            Ok(dir) => glob::exec(&dir, i).await,
            Err(reason) => return ToolResult::Err(ToolError { reason }),
        },
        ToolCall::Grep(i) => match base(registry, state, agent, &i.workspace) {
            Ok(dir) => grep::exec(&dir, i).await,
            Err(reason) => return ToolResult::Err(ToolError { reason }),
        },
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

/// Resolve a call's base directory.
///
/// An explicit `workspace` names the base outright, the way an absolute path
/// does, and so wins over the agent's sticky working directory; only a call
/// that names no workspace inherits it. Letting the override win
/// unconditionally would silently redirect a call that asked for workspace B
/// into workspace A — a wrong-file read, or worse a wrong-file write, with
/// nothing in the output to show it happened.
fn base(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    agent: &str,
    workspace: &Option<String>,
) -> Result<PathBuf, String> {
    let root = registry.resolve(workspace)?;
    Ok(if workspace.is_none() {
        state.effective_dir(agent, &root)
    } else {
        root
    })
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
    use horsie_models::Workspace;
    use horsie_models::runtime::BashInput;
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
                workspace: None,
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

    #[tokio::test]
    async fn dispatch_errors_when_workspace_ambiguous() {
        let registry = WorkspaceRegistry::new(vec![
            Workspace {
                name: "a".into(),
                path: "/a".into(),
            },
            Workspace {
                name: "b".into(),
                path: "/b".into(),
            },
        ]);
        // No `workspace` with several workspaces → a ToolError, never silent.
        let result = dispatch(
            &registry,
            &RuntimeState::new(),
            "a",
            ToolCall::Bash(BashInput {
                command: "echo hi".to_string(),
                timeout_secs: None,
                workspace: None,
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
                workspace: None,
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
                workspace: None,
            }),
        )
        .await;
        match r {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "nested"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }

    /// An explicit `workspace` must win over the sticky cwd. Before the base
    /// was made workspace-aware, the override was applied unconditionally, so
    /// this call read (and `write_file` would have written) inside workspace
    /// `a` while the model had asked for `b` — silently, with a plausible
    /// result and nothing in the output to reveal it.
    #[tokio::test]
    async fn a_named_workspace_is_not_hijacked_by_the_sticky_cwd() {
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
                workspace: Some("a".into()),
            }),
        )
        .await;
        assert!(matches!(r, ToolResult::Ok(_)));

        let r = dispatch(
            &registry,
            &state,
            "agent-1",
            ToolCall::ReadFile(horsie_models::runtime::ReadFileInput {
                path: "shared.txt".into(),
                start_line: None,
                end_line: None,
                workspace: Some("b".into()),
            }),
        )
        .await;
        match r {
            ToolResult::Ok(o) => assert_eq!(
                o.stdout, "from b",
                "a call naming workspace 'b' must read from 'b', not from the cwd set in 'a'"
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
                workspace: None,
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
                workspace: None,
            }),
        )
        .await;
        match r {
            ToolResult::Ok(o) => assert_eq!(o.stdout, "at root"),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
    }
}

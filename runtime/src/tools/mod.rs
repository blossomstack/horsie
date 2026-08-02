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

/// Per-stream output budget. Tool output rides along in the agent's conversation
/// history and is re-sent to the model on every turn, so an unbounded `cat`, build
/// log, or test run would otherwise blow the context window and token budget. The
/// cap is enforced here, in the one place every tool result flows through, so it
/// holds regardless of which tool produced the output.
const MAX_STREAM_BYTES: usize = 50_000;

/// `SetEnv` carries no `workspace`; match exhaustiveness (wildcards are
/// lint-denied) still needs an arm, over this const.
const NONE: Option<String> = None;

/// The `workspace` field carried by every dir-based tool input.
fn workspace_of(call: &ToolCall) -> &Option<String> {
    match call {
        ToolCall::Bash(i) => &i.workspace,
        ToolCall::ReadFile(i) => &i.workspace,
        ToolCall::WriteFile(i) => &i.workspace,
        ToolCall::FindAndReplace(i) => &i.workspace,
        ToolCall::ReplaceLines(i) => &i.workspace,
        ToolCall::ListFiles(i) => &i.workspace,
        ToolCall::Glob(i) => &i.workspace,
        ToolCall::Grep(i) => &i.workspace,
        ToolCall::SetWorkingDir(i) => &i.workspace,
        ToolCall::SetEnv(_) => &NONE,
    }
}

/// Run a tool call, then clamp its output. The state-mutating variants run
/// against the registry + per-caller state; every other tool resolves its
/// target workspace to a root directory (the single translation site), then
/// applies the caller's cwd override if it has one. An unresolvable `workspace`
/// (missing with several workspaces, or an unknown name) is returned to the
/// model as a `ToolError`.
pub async fn dispatch(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    session: &Option<String>,
    call: ToolCall,
) -> ToolResult {
    let result = match call {
        ToolCall::SetWorkingDir(input) => set_working_dir::exec(registry, state, session, input),
        ToolCall::SetEnv(input) => set_env::exec(state, session, input),
        call @ (ToolCall::Bash(_)
        | ToolCall::ReadFile(_)
        | ToolCall::WriteFile(_)
        | ToolCall::FindAndReplace(_)
        | ToolCall::ReplaceLines(_)
        | ToolCall::ListFiles(_)
        | ToolCall::Glob(_)
        | ToolCall::Grep(_)) => {
            let dir = match registry.resolve(workspace_of(&call)) {
                Ok(d) => state.effective_dir(session, &d),
                Err(reason) => return ToolResult::Err(ToolError { reason }),
            };
            match call {
                ToolCall::Bash(input) => bash::exec(&dir, &state.env_overlay(session), input).await,
                ToolCall::ReadFile(input) => read_file::exec(&dir, input).await,
                ToolCall::WriteFile(input) => write_file::exec(&dir, input).await,
                ToolCall::FindAndReplace(input) => find_and_replace::exec(&dir, input).await,
                ToolCall::ReplaceLines(input) => replace_lines::exec(&dir, input).await,
                ToolCall::ListFiles(input) => list_files::exec(&dir, input).await,
                ToolCall::Glob(input) => glob::exec(&dir, input).await,
                ToolCall::Grep(input) => grep::exec(&dir, input).await,
                // Dead: the outer match routed both state variants before this arm.
                ToolCall::SetWorkingDir(_) | ToolCall::SetEnv(_) => ToolResult::Err(ToolError {
                    reason: "internal dispatch error".to_string(),
                }),
            }
        }
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
            &None,
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
            &None,
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
        let session = None;
        let r = dispatch(
            &registry,
            &state,
            &session,
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
            &session,
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

    #[tokio::test]
    async fn cwd_overrides_are_isolated_per_session() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("root.txt"), "at root").unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "ws".into(),
            path: dir.path().to_path_buf(),
        }]);
        let state = RuntimeState::new();
        let a = Some("a".to_string());
        let b = Some("b".to_string());
        let r = dispatch(
            &registry,
            &state,
            &a,
            ToolCall::SetWorkingDir(horsie_models::runtime::SetWorkingDirInput {
                path: Some("sub".into()),
                workspace: None,
            }),
        )
        .await;
        assert!(matches!(r, ToolResult::Ok(_)));
        // Session b still resolves relative paths against the workspace root:
        // it can read a file that session a's cwd (sub/) does not contain.
        let r = dispatch(
            &registry,
            &state,
            &b,
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

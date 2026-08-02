use crate::state::RuntimeState;
use crate::workspace::WorkspaceRegistry;
use horsie_models::runtime::{SetWorkingDirInput, ToolError, ToolOutput, ToolResult};
use std::path::Path;

pub fn exec(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    agent: &str,
    input: SetWorkingDirInput,
) -> ToolResult {
    match &input.path {
        Some(path) => set(registry, state, agent, &input.workspace, path),
        None => reset(registry, state, agent, &input.workspace),
    }
}

/// Point the agent's cwd at `path` — absolute, or relative to its current
/// effective cwd. A bad target is an error and changes nothing.
///
/// Naming a `workspace` restarts from that workspace's root instead of
/// chaining off the current cwd, matching how the other tools read an explicit
/// workspace: as the base, not a hint.
fn set(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    agent: &str,
    workspace: &Option<String>,
    path: &str,
) -> ToolResult {
    let base = match registry.resolve(workspace) {
        Ok(root) if workspace.is_none() => state.effective_dir(agent, &root),
        Ok(root) => root,
        Err(reason) => return ToolResult::Err(ToolError { reason }),
    };
    // Path::join discards the base when `path` is absolute — exactly cd semantics.
    let candidate = base.join(Path::new(path));
    let dir = match candidate.canonicalize() {
        Ok(d) => d,
        Err(e) => {
            return ToolResult::Err(ToolError {
                reason: format!("cannot set working directory to '{path}': {e}"),
            });
        }
    };
    if !dir.is_dir() {
        return ToolResult::Err(ToolError {
            reason: format!("not a directory: {}", dir.display()),
        });
    }
    state.set_cwd(agent, Some(dir.clone()));
    ok(dir.display().to_string())
}

/// Clear the agent's override, returning to per-call workspace resolution.
/// The target workspace is validated first so a typo doesn't silently drop
/// the override.
fn reset(
    registry: &WorkspaceRegistry,
    state: &RuntimeState,
    agent: &str,
    workspace: &Option<String>,
) -> ToolResult {
    let root = match registry.resolve(workspace) {
        Ok(r) => r,
        Err(reason) => return ToolResult::Err(ToolError { reason }),
    };
    state.set_cwd(agent, None);
    ok(root.display().to_string())
}

fn ok(stdout: String) -> ToolResult {
    ToolResult::Ok(ToolOutput {
        stdout,
        stderr: String::new(),
        exit_code: 0,
    })
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
    use tempfile::TempDir;

    fn fixture() -> (TempDir, WorkspaceRegistry, RuntimeState) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let registry = WorkspaceRegistry::new(vec![Workspace {
            name: "ws".into(),
            path: dir.path().to_path_buf(),
        }]);
        (dir, registry, RuntimeState::new())
    }

    fn input(path: Option<&str>, workspace: Option<&str>) -> SetWorkingDirInput {
        SetWorkingDirInput {
            path: path.map(str::to_string),
            workspace: workspace.map(str::to_string),
        }
    }

    #[test]
    fn relative_path_resolves_against_current_cwd_and_chains() {
        let (dir, registry, state) = fixture();
        std::fs::create_dir(dir.path().join("sub/deep")).unwrap();
        let r = exec(&registry, &state, "a", input(Some("sub"), None));
        match r {
            ToolResult::Ok(o) => assert_eq!(
                o.stdout,
                dir.path()
                    .join("sub")
                    .canonicalize()
                    .unwrap()
                    .display()
                    .to_string()
            ),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
        // A second relative set chains off the first.
        let r = exec(&registry, &state, "a", input(Some("deep"), None));
        assert!(matches!(r, ToolResult::Ok(_)));
        assert_eq!(
            state.effective_dir("a", dir.path()),
            dir.path().join("sub/deep").canonicalize().unwrap()
        );
    }

    #[test]
    fn absolute_path_is_used_as_is() {
        let (dir, registry, state) = fixture();
        let abs = dir.path().join("sub").display().to_string();
        let r = exec(&registry, &state, "a", input(Some(&abs), None));
        assert!(matches!(r, ToolResult::Ok(_)));
    }

    #[test]
    fn nonexistent_target_errors_and_preserves_state() {
        let (dir, registry, state) = fixture();
        let r = exec(&registry, &state, "a", input(Some("nope"), None));
        assert!(matches!(r, ToolResult::Err(_)));
        assert_eq!(state.effective_dir("a", dir.path()), dir.path());
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let (dir, registry, state) = fixture();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let r = exec(&registry, &state, "a", input(Some("f.txt"), None));
        match r {
            ToolResult::Err(e) => assert!(e.reason.contains("not a directory"), "{}", e.reason),
            ToolResult::Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn reset_clears_the_override() {
        let (dir, registry, state) = fixture();
        let _ = exec(&registry, &state, "a", input(Some("sub"), None));
        let r = exec(&registry, &state, "a", input(None, None));
        assert!(matches!(r, ToolResult::Ok(_)));
        assert_eq!(state.effective_dir("a", dir.path()), dir.path());
    }

    #[test]
    fn reset_with_unknown_workspace_errors_and_keeps_the_override() {
        let (dir, registry, state) = fixture();
        let _ = exec(&registry, &state, "a", input(Some("sub"), None));
        let r = exec(&registry, &state, "a", input(None, Some("zzz")));
        assert!(matches!(r, ToolResult::Err(_)));
        assert_eq!(
            state.effective_dir("a", dir.path()),
            dir.path().join("sub").canonicalize().unwrap()
        );
    }
}

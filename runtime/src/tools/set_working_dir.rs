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
        Some(path) => set(registry, state, agent, path),
        None => reset(registry, state, agent),
    }
}

/// Point the agent's cwd at `path` — absolute, or relative to its current
/// effective cwd. A bad target is an error and changes nothing.
fn set(registry: &WorkspaceRegistry, state: &RuntimeState, agent: &str, path: &str) -> ToolResult {
    let base = match registry.default_root() {
        Ok(root) => state.effective_dir(agent, &root),
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

/// Clear the agent's override, returning to the default working directory, and
/// report where that leaves it. Resolved before clearing so a runtime with no
/// workspaces errors instead of silently dropping the override.
fn reset(registry: &WorkspaceRegistry, state: &RuntimeState, agent: &str) -> ToolResult {
    let root = match registry.default_root() {
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

    fn input(path: Option<&str>) -> SetWorkingDirInput {
        SetWorkingDirInput {
            path: path.map(str::to_string),
        }
    }

    #[test]
    fn relative_path_resolves_against_current_cwd_and_chains() {
        let (dir, registry, state) = fixture();
        std::fs::create_dir(dir.path().join("sub/deep")).unwrap();
        let r = exec(&registry, &state, "a", input(Some("sub")));
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
        let r = exec(&registry, &state, "a", input(Some("deep")));
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
        let r = exec(&registry, &state, "a", input(Some(&abs)));
        assert!(matches!(r, ToolResult::Ok(_)));
    }

    #[test]
    fn nonexistent_target_errors_and_preserves_state() {
        let (dir, registry, state) = fixture();
        let r = exec(&registry, &state, "a", input(Some("nope")));
        assert!(matches!(r, ToolResult::Err(_)));
        assert_eq!(state.effective_dir("a", dir.path()), dir.path());
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let (dir, registry, state) = fixture();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        let r = exec(&registry, &state, "a", input(Some("f.txt")));
        match r {
            ToolResult::Err(e) => assert!(e.reason.contains("not a directory"), "{}", e.reason),
            ToolResult::Ok(_) => panic!("expected error"),
        }
    }

    #[test]
    fn reset_clears_the_override_and_reports_the_default_root() {
        let (dir, registry, state) = fixture();
        let _ = exec(&registry, &state, "a", input(Some("sub")));
        match exec(&registry, &state, "a", input(None)) {
            ToolResult::Ok(o) => assert_eq!(o.stdout, dir.path().display().to_string()),
            ToolResult::Err(e) => panic!("{}", e.reason),
        }
        assert_eq!(state.effective_dir("a", dir.path()), dir.path());
    }

    /// With nothing to fall back to, the override is left alone rather than
    /// dropped into a directory the runtime cannot name.
    #[test]
    fn reset_errors_with_no_workspaces_and_keeps_the_override() {
        let (dir, registry, state) = fixture();
        let _ = exec(&registry, &state, "a", input(Some("sub")));
        let empty = WorkspaceRegistry::new(vec![]);
        assert!(matches!(
            exec(&empty, &state, "a", input(None)),
            ToolResult::Err(_)
        ));
        assert_eq!(
            state.effective_dir("a", dir.path()),
            dir.path().join("sub").canonicalize().unwrap()
        );
    }
}

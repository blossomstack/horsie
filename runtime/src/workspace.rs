use models::Workspace;
use std::path::{Path, PathBuf};

/// Name → path registry the runtime resolves tool and scan `workspace` fields
/// against. Order-preserving. This is the single name→path translation site for the
/// runtime — both `tools::dispatch` and `scan::exec` go through it.
#[derive(Clone, Debug)]
pub struct WorkspaceRegistry {
    workspaces: Vec<Workspace>,
}

impl WorkspaceRegistry {
    pub fn new(workspaces: Vec<Workspace>) -> Self {
        Self { workspaces }
    }

    /// Parse a `name=path` CLI argument into a [`Workspace`].
    pub fn parse_arg(s: &str) -> Result<Workspace, String> {
        let (name, path) = s
            .split_once('=')
            .ok_or_else(|| format!("expected name=path, got '{s}'"))?;
        if name.is_empty() || path.is_empty() {
            return Err(format!("empty name or path in '{s}'"));
        }
        Ok(Workspace {
            name: name.to_string(),
            path: PathBuf::from(path),
        })
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// Resolve a tool's `workspace` field to a root path. `None` defaults to the sole
    /// workspace, or errors when there are several (the model must name one). An
    /// unknown name errors with the available list.
    pub fn resolve(&self, workspace: &Option<String>) -> Result<PathBuf, String> {
        match workspace {
            Some(name) => self
                .workspaces
                .iter()
                .find(|w| &w.name == name)
                .map(|w| w.path.clone())
                .ok_or_else(|| {
                    format!(
                        "unknown workspace '{name}'; available: {}",
                        self.names_csv()
                    )
                }),
            None => match self.workspaces.as_slice() {
                [only] => Ok(only.path.clone()),
                [] => Err("no workspaces configured".to_string()),
                _ => Err(format!(
                    "multiple workspaces; specify one of: {}",
                    self.names_csv()
                )),
            },
        }
    }

    /// Select workspaces to scan: `None` → all roots (registry order); `Some(name)` →
    /// just that one (empty if the name is unknown — scan stays best-effort).
    pub fn select(&self, workspace: &Option<String>) -> Vec<Workspace> {
        match workspace {
            None => self.workspaces.clone(),
            Some(name) => self
                .workspaces
                .iter()
                .filter(|w| &w.name == name)
                .cloned()
                .collect(),
        }
    }

    fn names_csv(&self) -> String {
        self.workspaces
            .iter()
            .map(|w| w.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// True if `path/.git` exists (a dir for a normal repo, a file for a submodule/worktree).
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
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

    fn reg() -> WorkspaceRegistry {
        WorkspaceRegistry::new(vec![
            Workspace {
                name: "api".into(),
                path: PathBuf::from("/ws/api"),
            },
            Workspace {
                name: "web".into(),
                path: PathBuf::from("/ws/web"),
            },
        ])
    }

    #[test]
    fn resolves_named() {
        assert_eq!(
            reg().resolve(&Some("web".into())).unwrap(),
            PathBuf::from("/ws/web")
        );
    }

    #[test]
    fn missing_with_multiple_errors() {
        assert!(reg().resolve(&None).is_err());
    }

    #[test]
    fn missing_with_single_defaults() {
        let r = WorkspaceRegistry::new(vec![Workspace {
            name: "only".into(),
            path: PathBuf::from("/x"),
        }]);
        assert_eq!(r.resolve(&None).unwrap(), PathBuf::from("/x"));
    }

    #[test]
    fn unknown_name_errors() {
        assert!(reg().resolve(&Some("nope".into())).is_err());
    }

    #[test]
    fn parse_arg_splits_name_path() {
        let w = WorkspaceRegistry::parse_arg("api=/ws/api").unwrap();
        assert_eq!(w.name, "api");
        assert_eq!(w.path, PathBuf::from("/ws/api"));
    }

    #[test]
    fn parse_arg_rejects_missing_eq() {
        assert!(WorkspaceRegistry::parse_arg("noeq").is_err());
    }

    #[test]
    fn select_all_and_one() {
        assert_eq!(reg().select(&None).len(), 2);
        assert_eq!(reg().select(&Some("api".into())).len(), 1);
        assert!(reg().select(&Some("zzz".into())).is_empty());
    }
}

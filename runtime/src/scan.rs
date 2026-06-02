use crate::workspace::{WorkspaceRegistry, is_git_repo};
use models::runtime::{ScanRequest, ScannedFile, WorkspaceScan};

/// Scan the selected workspaces. `req.workspace` filters which roots to scan (`None`
/// → all; `Some(name)` → just that one, or none if unknown). For each root, gather the
/// first existing instruction candidate (in order) and every file matching
/// `skills_glob`. Best-effort — a missing candidate yields `None`; an unreadable match
/// is skipped.
pub fn exec(registry: &WorkspaceRegistry, req: ScanRequest) -> Vec<WorkspaceScan> {
    registry
        .select(&req.workspace)
        .into_iter()
        .map(|ws| {
            let dir = ws.path.as_path();
            let instructions = req.instruction_candidates.iter().find_map(|name| {
                std::fs::read_to_string(dir.join(name))
                    .ok()
                    .map(|content| ScannedFile {
                        path: name.clone(),
                        content,
                    })
            });

            let pattern = format!("{}/{}", dir.display(), req.skills_glob);
            let mut skills = Vec::new();
            if let Ok(paths) = glob::glob(&pattern) {
                for entry in paths.flatten() {
                    if let Ok(content) = std::fs::read_to_string(&entry) {
                        skills.push(ScannedFile {
                            path: entry.to_string_lossy().into_owned(),
                            content,
                        });
                    }
                }
            }

            WorkspaceScan {
                name: ws.name.clone(),
                path: dir.display().to_string(),
                is_git_repo: is_git_repo(dir),
                instructions,
                skills,
            }
        })
        .collect()
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
    use models::Workspace;
    use std::path::Path;
    use tempfile::TempDir;

    fn reg(dirs: &[(&str, &Path)]) -> WorkspaceRegistry {
        WorkspaceRegistry::new(
            dirs.iter()
                .map(|(n, p)| Workspace {
                    name: (*n).into(),
                    path: p.to_path_buf(),
                })
                .collect(),
        )
    }

    fn req(workspace: Option<String>) -> ScanRequest {
        ScanRequest {
            call_id: "c".into(),
            workspace,
            instruction_candidates: vec!["AGENTS.md".into(), "AGENT.md".into(), "CLAUDE.md".into()],
            skills_glob: ".claude/skills/*/SKILL.md".into(),
        }
    }

    #[test]
    fn scans_all_workspaces() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        std::fs::write(a.path().join("AGENTS.md"), "a-rules").unwrap();
        let scans = exec(&reg(&[("a", a.path()), ("b", b.path())]), req(None));
        assert_eq!(scans.len(), 2);
        assert_eq!(scans[0].name, "a");
        assert_eq!(scans[0].instructions.as_ref().unwrap().content, "a-rules");
        assert!(scans[1].instructions.is_none());
    }

    #[test]
    fn filter_selects_one() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        let scans = exec(
            &reg(&[("a", a.path()), ("b", b.path())]),
            req(Some("b".into())),
        );
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].name, "b");
    }

    #[test]
    fn unknown_filter_is_empty() {
        let a = TempDir::new().unwrap();
        assert!(exec(&reg(&[("a", a.path())]), req(Some("zzz".into()))).is_empty());
    }

    #[test]
    fn detects_git_repo() {
        let a = TempDir::new().unwrap();
        std::fs::create_dir(a.path().join(".git")).unwrap();
        let scans = exec(&reg(&[("a", a.path())]), req(None));
        assert!(scans[0].is_git_repo);
    }

    #[test]
    fn no_git_is_false() {
        let a = TempDir::new().unwrap();
        let scans = exec(&reg(&[("a", a.path())]), req(None));
        assert!(!scans[0].is_git_repo);
    }

    #[test]
    fn instruction_precedence_first_match_wins() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENT.md"), "second").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "third").unwrap();
        let scans = exec(&reg(&[("w", dir.path())]), req(None));
        let f = scans[0].instructions.as_ref().unwrap();
        assert_eq!(f.path, "AGENT.md");
        assert_eq!(f.content, "second");
    }

    #[test]
    fn globs_skills_in_hidden_dir() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join(".claude/skills/git-bisect");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "body").unwrap();
        let scans = exec(&reg(&[("w", dir.path())]), req(None));
        assert_eq!(scans[0].skills.len(), 1);
        assert_eq!(scans[0].skills[0].content, "body");
    }

    #[test]
    fn missing_skills_dir_is_empty() {
        let dir = TempDir::new().unwrap();
        assert!(
            exec(&reg(&[("w", dir.path())]), req(None))[0]
                .skills
                .is_empty()
        );
    }
}

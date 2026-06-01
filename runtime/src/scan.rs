use models::runtime::{ScanRequest, ScannedFile, WorkspaceScan};
use std::path::Path;

/// Gather workspace context from `working_dir`: the first existing instruction
/// candidate (in order) and every file matching `skills_glob`. Best-effort — a
/// missing candidate yields `None`; an unreadable match is skipped.
pub fn exec(working_dir: &Path, req: ScanRequest) -> WorkspaceScan {
    let instructions = req.instruction_candidates.iter().find_map(|name| {
        let path = working_dir.join(name);
        std::fs::read_to_string(&path)
            .ok()
            .map(|content| ScannedFile {
                path: name.clone(),
                content,
            })
    });

    let pattern = format!("{}/{}", working_dir.display(), req.skills_glob);
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
        instructions,
        skills,
    }
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

    fn req() -> ScanRequest {
        ScanRequest {
            call_id: "c".into(),
            instruction_candidates: vec!["AGENTS.md".into(), "AGENT.md".into(), "CLAUDE.md".into()],
            skills_glob: ".claude/skills/*/SKILL.md".into(),
        }
    }

    #[test]
    fn instruction_precedence_first_match_wins() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENT.md"), "second").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "third").unwrap();
        let scan = exec(dir.path(), req());
        let f = scan.instructions.expect("instructions");
        assert_eq!(f.path, "AGENT.md");
        assert_eq!(f.content, "second");
    }

    #[test]
    fn no_instruction_file_is_none() {
        let dir = TempDir::new().unwrap();
        assert!(exec(dir.path(), req()).instructions.is_none());
    }

    #[test]
    fn globs_skills_in_hidden_dir() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join(".claude/skills/git-bisect");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "body").unwrap();
        let scan = exec(dir.path(), req());
        assert_eq!(scan.skills.len(), 1);
        assert_eq!(scan.skills[0].content, "body");
        assert!(
            scan.skills[0]
                .path
                .ends_with(".claude/skills/git-bisect/SKILL.md")
        );
    }

    #[test]
    fn missing_skills_dir_is_empty() {
        let dir = TempDir::new().unwrap();
        assert!(exec(dir.path(), req()).skills.is_empty());
    }
}

//! Locating a plugin's skills: the manifest `skills` field (string or array of
//! roots relative to the plugin root), else the conventional `skills/`. A skill
//! is a direct child directory of a root that contains a `SKILL.md`.

use super::PluginManifest;
use std::path::{Path, PathBuf};

/// Skill roots for a plugin: the manifest override when declared, else `skills/`.
pub fn skill_locations(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf> {
    match manifest.map(|m| m.skills.as_slice()) {
        Some(roots) if !roots.is_empty() => roots
            .iter()
            .map(|r| super::join_declared(plugin_root, r))
            .collect(),
        _ => vec![plugin_root.join("skills")],
    }
}

/// Every directory under a skill root that contains a `SKILL.md`, sorted for
/// stable ordering.
///
/// Paths are built by joining, never canonicalised, so callers can
/// `strip_prefix` a library root off them — the shared library installs plugins
/// as symlinks, and canonicalising here would leak the link target into the
/// relative skill ids the agent sees.
pub fn skill_dirs(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in skill_locations(plugin_root, manifest) {
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if dir.join("SKILL.md").is_file() {
                out.push(dir);
            }
        }
    }
    out.sort();
    out
}

/// A skill's `name` and `description`, read from its `SKILL.md` frontmatter.
///
/// `None` when either is missing: a skill a picker cannot label and a model
/// cannot choose between is not one. Deliberately the same two fields, read the
/// same way, as the runtime-side reader in `horsie_workflow` — the server and
/// the runtime must agree about what a skill is.
#[must_use]
pub fn parse(content: &str) -> Option<(String, String)> {
    let (front, _) = crate::frontmatter::split(content)?;
    let mut name = None;
    let mut description = None;
    for (key, value) in crate::frontmatter::pairs(front)? {
        match key {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            _ => {}
        }
    }
    Some((name?, description?))
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

    fn write_skill(root: &Path, rel: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: x\n---\nbody").unwrap();
    }

    #[test]
    fn defaults_to_skills_dir() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "skills/brainstorming");
        let dirs = skill_dirs(dir.path(), None);
        assert_eq!(dirs, vec![dir.path().join("skills/brainstorming")]);
    }

    #[test]
    fn manifest_override_replaces_the_default() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "custom/skills/x");
        write_skill(dir.path(), "skills/ignored");
        let m = PluginManifest {
            skills: vec!["custom/skills".into()],
            ..Default::default()
        };
        let dirs = skill_dirs(dir.path(), Some(&m));
        assert_eq!(dirs, vec![dir.path().join("custom/skills/x")]);
    }

    /// impeccable's shape: a `./`-prefixed, trailing-slash, dot-directory root.
    #[test]
    fn dot_prefixed_hidden_root_resolves() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), ".claude/skills/impeccable");
        let m = PluginManifest {
            skills: vec!["./.claude/skills/".into()],
            ..Default::default()
        };
        let dirs = skill_dirs(dir.path(), Some(&m));
        assert_eq!(dirs.len(), 1);
        assert!(dirs[0].ends_with("impeccable"));
        // The returned path must stay under the plugin root and must not be
        // canonicalised — callers strip_prefix it to build a relative id.
        assert_eq!(
            dirs[0].strip_prefix(dir.path()).unwrap(),
            Path::new(".claude/skills/impeccable"),
            "the declared `./` prefix must not survive into the path"
        );
    }

    #[test]
    fn array_roots_are_all_scanned_and_sorted() {
        let dir = TempDir::new().unwrap();
        write_skill(dir.path(), "b/skills/two");
        write_skill(dir.path(), "a/skills/one");
        let m = PluginManifest {
            skills: vec!["a/skills".into(), "b/skills".into()],
            ..Default::default()
        };
        let dirs = skill_dirs(dir.path(), Some(&m));
        assert_eq!(
            dirs,
            vec![
                dir.path().join("a/skills/one"),
                dir.path().join("b/skills/two"),
            ]
        );
    }

    #[test]
    fn missing_or_empty_roots_yield_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(skill_dirs(dir.path(), None).is_empty());
        std::fs::create_dir_all(dir.path().join("skills/notaskill")).unwrap();
        assert!(skill_dirs(dir.path(), None).is_empty());
    }
}

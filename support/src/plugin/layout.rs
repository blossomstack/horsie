//! Deciding whether a directory is an installable plugin, and describing it.

use super::{PluginManifest, skills};
use std::path::{Path, PathBuf};

/// An inspected plugin directory.
pub struct PluginRoot {
    pub dir: PathBuf,
    pub manifest: Option<PluginManifest>,
    pub skill_dirs: Vec<PathBuf>,
}

impl PluginRoot {
    /// Read the manifest (if any) and enumerate skills. `Err` only when a
    /// manifest is present but malformed — an absent manifest is normal.
    pub fn inspect(dir: &Path) -> Result<PluginRoot, String> {
        let manifest = PluginManifest::read(dir)?;
        let skill_dirs = skills::skill_dirs(dir, manifest.as_ref());
        Ok(PluginRoot {
            dir: dir.to_path_buf(),
            manifest,
            skill_dirs,
        })
    }

    /// Manifest `name`, else `fallback` (normally the repo basename).
    pub fn name(&self, fallback: &str) -> String {
        self.manifest
            .as_ref()
            .and_then(|m| m.name.as_deref())
            .unwrap_or(fallback)
            .to_string()
    }

    pub fn version(&self) -> Option<&str> {
        self.manifest.as_ref().and_then(|m| m.version.as_deref())
    }

    pub fn description(&self) -> Option<&str> {
        self.manifest.as_ref().and_then(|m| m.description.as_deref())
    }

    /// Today: a plugin is installable when it provides at least one skill.
    /// Widening this to hooks/agents/commands is Phase 1 of #105 — and this is
    /// the single place it changes.
    pub fn is_installable(&self) -> bool {
        !self.skill_dirs.is_empty()
    }

    /// Why `is_installable` is false, naming every location that was searched
    /// so the user can see what the tool expected.
    pub fn rejection(&self) -> String {
        let looked = skills::skill_locations(&self.dir, self.manifest.as_ref())
            .iter()
            .map(|p| p.strip_prefix(&self.dir).unwrap_or(p).display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        format!("no SKILL.md found: looked for */SKILL.md under {looked}")
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

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn plain_skills_dir_is_installable() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("skills/x/SKILL.md"), "---\nname: x\n---\n");
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert!(root.is_installable());
        assert_eq!(root.name("fallback"), "fallback");
    }

    #[test]
    fn manifest_name_wins_over_fallback() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join(".claude-plugin/plugin.json"),
            r#"{"name":"fancy","version":"2.0","description":"d"}"#,
        );
        write(&dir.path().join("skills/x/SKILL.md"), "---\nname: x\n---\n");
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert_eq!(root.name("fallback"), "fancy");
        assert_eq!(root.version(), Some("2.0"));
        assert_eq!(root.description(), Some("d"));
    }

    /// The impeccable case: manifest points skills outside the default location.
    #[test]
    fn manifest_skills_override_makes_it_installable() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join(".claude-plugin/plugin.json"),
            r#"{"name":"impeccable","skills":"./.claude/skills/"}"#,
        );
        write(
            &dir.path().join(".claude/skills/impeccable/SKILL.md"),
            "---\nname: impeccable\n---\n",
        );
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert!(
            root.is_installable(),
            "manifest-declared skills root must count"
        );
        assert_eq!(root.skill_dirs.len(), 1);
    }

    #[test]
    fn no_skills_is_not_installable_and_says_where_it_looked() {
        let dir = TempDir::new().unwrap();
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert!(!root.is_installable());
        let msg = root.rejection();
        assert!(
            msg.contains("skills"),
            "rejection should name the location: {msg}"
        );
    }

    #[test]
    fn malformed_manifest_propagates() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join(".claude-plugin/plugin.json"), "{oops");
        assert!(PluginRoot::inspect(dir.path()).is_err());
    }
}

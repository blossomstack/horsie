//! Deciding whether a directory is an installable plugin, and describing it.

use super::{ManifestDialect, PluginManifest, agents, commands, skills};
use std::path::{Path, PathBuf};

/// An inspected plugin directory.
pub struct PluginRoot {
    pub dir: PathBuf,
    pub manifest: Option<PluginManifest>,
    pub skill_dirs: Vec<PathBuf>,
    pub agent_files: Vec<PathBuf>,
    pub command_files: Vec<PathBuf>,
}

impl PluginRoot {
    /// Read the manifest (if any) and enumerate what the plugin provides. `Err`
    /// only when a manifest is present but malformed — an absent manifest is
    /// normal.
    pub fn inspect(dir: &Path) -> Result<PluginRoot, String> {
        let manifest = PluginManifest::read(dir)?;
        let skill_dirs = skills::skill_dirs(dir, manifest.as_ref());
        let agent_files = agents::agent_files(dir, manifest.as_ref());
        let command_files = commands::command_files(dir, manifest.as_ref());
        Ok(PluginRoot {
            dir: dir.to_path_buf(),
            manifest,
            skill_dirs,
            agent_files,
            command_files,
        })
    }

    /// Which packaging convention this directory follows.
    ///
    /// A directory with no manifest at all reads as
    /// [`ManifestDialect::Claude`]: it is being interpreted by horsie's own
    /// conventions, which are Claude's, and the portable dialect cannot be
    /// claimed by a plugin that never declared a `$schema`.
    pub fn dialect(&self) -> ManifestDialect {
        self.manifest
            .as_ref()
            .map_or(ManifestDialect::Claude, |m| m.dialect)
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
        self.manifest
            .as_ref()
            .and_then(|m| m.description.as_deref())
    }

    /// A plugin is installable when it declares itself one with a manifest, or
    /// provides something horsie runs: skills, hooks, agents, or commands.
    ///
    /// This was skills-only, which refused a hooks-only plugin at install even
    /// though horsie would have run its hooks perfectly well — the whole class
    /// of guard-only plugins, and MCP-only ones once Phase 4 lands.
    ///
    /// A manifest counts on its own because content is not always a property of
    /// the tree at the moment it is read. An authored plugin is created empty
    /// and filled in afterwards, so judging it by content alone made it
    /// unpublishable — and therefore unassignable — for the whole window in
    /// which it was being written. A manifest is the author saying this *is* a
    /// plugin, which is exactly the question being asked; the content check
    /// stays for the trees that never declared anything, which is where this
    /// guard earns its keep: a repo horsie was pointed at by mistake.
    pub fn is_installable(&self) -> bool {
        self.manifest.is_some()
            || !self.skill_dirs.is_empty()
            || !self.agent_files.is_empty()
            || !self.command_files.is_empty()
            || self.declares_hooks()
    }

    /// Whether `hooks/hooks.json` declares anything at all, runnable or not.
    ///
    /// Deliberately not "declares a hook horsie can run": a plugin whose only
    /// event is unwired is still a plugin, and the install already reports the
    /// events it cannot fire by name. Refusing it would be refusing software
    /// for a gap on horsie's side.
    fn declares_hooks(&self) -> bool {
        super::hooks::read(&self.dir)
            .is_ok_and(|h| !h.decls.is_empty() || !h.unsupported.is_empty())
    }

    /// Why `is_installable` is false, naming every location that was searched
    /// so the user can see what the tool expected.
    pub fn rejection(&self) -> String {
        let relative = |paths: Vec<PathBuf>| {
            paths
                .iter()
                .map(|p| p.strip_prefix(&self.dir).unwrap_or(p).display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        format!(
            "no manifest and nothing horsie can run: no {} or {}, no */SKILL.md \
             under {}, no *.md under {} or {}, no hooks/hooks.json",
            relative(vec![PluginManifest::claude_path(&self.dir)]),
            relative(vec![PluginManifest::agent_plugin_path(&self.dir)]),
            relative(skills::skill_locations(&self.dir, self.manifest.as_ref())),
            relative(agents::agent_locations(&self.dir, self.manifest.as_ref())),
            relative(commands::command_locations(
                &self.dir,
                self.manifest.as_ref()
            )),
        )
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
    fn an_empty_directory_is_not_installable_and_says_where_it_looked() {
        let dir = TempDir::new().unwrap();
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert!(!root.is_installable());
        let msg = root.rejection();
        for expected in ["skills", "agents", "hooks"] {
            assert!(
                msg.contains(expected),
                "rejection should name {expected}: {msg}"
            );
        }
    }

    /// A manifest and nothing else. This is what an authored plugin renders to
    /// between being created and having its first skill written, and refusing
    /// it there meant it never reached the library an agent picks from.
    #[test]
    fn a_manifest_alone_is_installable_in_either_dialect() {
        for (path, body) in [
            (
                ".claude-plugin/plugin.json",
                r#"{"name":"empty","version":"0.0.1"}"#,
            ),
            (
                "plugin.json",
                r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
                    "name":"empty","version":"0.0.1"}"#,
            ),
        ] {
            let dir = TempDir::new().unwrap();
            write(&dir.path().join(path), body);
            let root = PluginRoot::inspect(dir.path()).unwrap();
            assert!(root.is_installable(), "{path} alone must install");
            assert!(root.skill_dirs.is_empty());
        }
    }

    /// A plugin need not ship skills. This refused every guard-only plugin in
    /// the ecosystem — horsie ran their hooks, and would not let them install.
    #[test]
    fn hooks_alone_and_agents_alone_are_each_installable() {
        let hooks_only = TempDir::new().unwrap();
        write(
            &hooks_only.path().join("hooks/hooks.json"),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"guard"}]}]}}"#,
        );
        assert!(
            PluginRoot::inspect(hooks_only.path())
                .unwrap()
                .is_installable()
        );

        let agents_only = TempDir::new().unwrap();
        write(
            &agents_only.path().join("agents/reviewer.md"),
            "---\nname: reviewer\ndescription: reviews\n---\nbody",
        );
        let root = PluginRoot::inspect(agents_only.path()).unwrap();
        assert!(root.is_installable());
        assert_eq!(root.agent_files.len(), 1);
    }

    /// A plugin whose only declared event is one horsie cannot fire is still a
    /// plugin. The install reports the event by name; refusing the whole bundle
    /// would be refusing software for a gap on horsie's side.
    #[test]
    fn a_plugin_declaring_only_an_unwired_hook_still_installs() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("hooks/hooks.json"),
            r#"{"hooks":{"Notification":[{"hooks":[{"type":"command","command":"ping"}]}]}}"#,
        );
        assert!(PluginRoot::inspect(dir.path()).unwrap().is_installable());
    }

    #[test]
    fn malformed_manifest_propagates() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join(".claude-plugin/plugin.json"), "{oops");
        assert!(PluginRoot::inspect(dir.path()).is_err());
    }

    /// The whole portable core in one tree: a root manifest, a fixed `skills/`.
    /// Nothing about it is horsie-shaped, and it still installs.
    #[test]
    fn an_agent_plugins_tree_is_installable_and_names_its_dialect() {
        let dir = TempDir::new().unwrap();
        write(
            &dir.path().join("plugin.json"),
            r#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
                "name":"acme.tools","description":"d","version":"1.0.0"}"#,
        );
        write(
            &dir.path().join("skills/summarize/SKILL.md"),
            "---\nname: summarize\ndescription: d\n---\nbody",
        );
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert_eq!(root.dialect(), ManifestDialect::AgentPlugin);
        assert!(root.is_installable());
        assert_eq!(root.name("fallback"), "acme.tools");
        assert_eq!(root.skill_dirs.len(), 1);
    }

    /// A directory nobody labelled is being read by horsie's own conventions,
    /// which are Claude's.
    #[test]
    fn a_manifestless_tree_reads_as_the_claude_dialect() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("skills/x/SKILL.md"), "---\nname: x\n---\n");
        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert_eq!(root.dialect(), ManifestDialect::Claude);
    }
}

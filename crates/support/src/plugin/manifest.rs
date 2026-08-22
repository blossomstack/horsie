//! A plugin's manifest, in either of the two dialects horsie reads.
//!
//! Claude Code puts it at `.claude-plugin/plugin.json` and lets a plugin move
//! its own component directories. [Agent Plugins
//! 1.0](https://agent-plugins.org/specification) puts it at the root as
//! `plugin.json`, identifies itself with `$schema`, and fixes every component
//! location — "fixed component locations cannot be changed or configured
//! inline in `plugin.json`".
//!
//! Only the fields horsie uses are modelled, plus the metadata the settings
//! page shows.

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The `$schema` values that identify an Agent Plugins manifest. Matched by
/// prefix so a 1.1 manifest is still recognised as one — the spec pins the
/// version in the identifier, and refusing an unknown one would read as "this
/// is not a plugin" rather than "this is a newer plugin".
const AGENT_PLUGINS_SCHEMA_PREFIX: &str = "https://agent-plugins.org/schemas/";

/// Which packaging convention a plugin directory follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestDialect {
    /// `.claude-plugin/plugin.json`. Component locations are configurable, and
    /// horsie's hooks, agents and commands conventions apply.
    Claude,
    /// Root `plugin.json` carrying an Agent Plugins `$schema`. Skills live at
    /// `skills/`, MCP servers at `mcp.json`, and neither can be moved.
    AgentPlugin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest {
    pub dialect: ManifestDialect,
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Skill roots relative to the plugin root. Empty means "not declared" —
    /// callers fall back to the conventional `skills/`. Always empty under
    /// [`ManifestDialect::AgentPlugin`], where the location is fixed.
    pub skills: Vec<String>,
    /// Agent locations relative to the plugin root — a directory of `*.md`, or
    /// a single `.md` file. Empty means "not declared", so callers fall back to
    /// the conventional `agents/`.
    pub agents: Vec<String>,
    /// Command locations relative to the plugin root, same shape as `agents`.
    /// Empty falls back to the conventional `commands/`.
    pub commands: Vec<String>,
    /// Author's name, when declared.
    pub author: Option<String>,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    /// SPDX identifier, by convention.
    pub license: Option<String>,
    pub keywords: Vec<String>,
}

/// Raw wire shape, covering both dialects. `skills` is a string or an array of
/// strings; `repository` is a string or an object with a `url`.
#[derive(Deserialize)]
struct RawManifest {
    #[serde(rename = "$schema")]
    schema: Option<String>,
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    skills: Option<StringOrList>,
    agents: Option<StringOrList>,
    commands: Option<StringOrList>,
    author: Option<RawAuthor>,
    homepage: Option<String>,
    repository: Option<RawRepository>,
    license: Option<String>,
    keywords: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    fn into_vec(self) -> Vec<String> {
        match self {
            StringOrList::One(s) => vec![s],
            StringOrList::Many(v) => v,
        }
    }
}

/// Claude's manifests carry a bare string here, Agent Plugins an object.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawAuthor {
    Name(String),
    Object { name: Option<String> },
}

impl RawAuthor {
    fn into_name(self) -> Option<String> {
        match self {
            RawAuthor::Name(n) => Some(n),
            RawAuthor::Object { name } => name,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawRepository {
    Url(String),
    Object { url: Option<String> },
}

impl RawRepository {
    fn into_url(self) -> Option<String> {
        match self {
            RawRepository::Url(u) => Some(u),
            RawRepository::Object { url } => url,
        }
    }
}

/// The spec's plugin-name grammar: 1–64 characters of lowercase alphanumerics,
/// hyphens and periods, starting and ending alphanumeric, with no `--` or `..`.
///
/// Shared with authored plugins, whose names must satisfy the same rule — they
/// are rendered into a `plugin.json` that has to be readable by any conformant
/// client, not just this one.
pub fn validate_name(name: &str) -> Result<(), String> {
    let complaint = |why: &str| Err(format!("plugin name '{name}': {why}"));
    if name.is_empty() || name.chars().count() > 64 {
        return complaint("must be 1 to 64 characters");
    }
    if let Some(bad) = name
        .chars()
        .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-' || *c == '.'))
    {
        return complaint(&format!(
            "may only contain lowercase letters, digits, '-' and '.', but has '{bad}'"
        ));
    }
    let alnum = |c: Option<char>| c.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if !alnum(name.chars().next()) || !alnum(name.chars().next_back()) {
        return complaint("must start and end with a letter or digit");
    }
    if name.contains("--") || name.contains("..") {
        return complaint("may not contain '--' or '..'");
    }
    Ok(())
}

impl PluginManifest {
    /// An empty manifest in `dialect`, to be filled in by a caller building one
    /// rather than reading one. There is deliberately no `Default`: a manifest
    /// whose dialect nobody chose is a manifest whose component locations are
    /// wrong in one of the two directions, silently.
    #[must_use]
    pub fn empty(dialect: ManifestDialect) -> PluginManifest {
        PluginManifest {
            dialect,
            name: None,
            version: None,
            description: None,
            skills: Vec::new(),
            agents: Vec::new(),
            commands: Vec::new(),
            author: None,
            homepage: None,
            repository: None,
            license: None,
            keywords: Vec::new(),
        }
    }

    /// `<plugin_root>/.claude-plugin/plugin.json`.
    pub fn claude_path(plugin_root: &Path) -> PathBuf {
        plugin_root.join(".claude-plugin").join("plugin.json")
    }

    /// `<plugin_root>/plugin.json`.
    pub fn agent_plugin_path(plugin_root: &Path) -> PathBuf {
        plugin_root.join("plugin.json")
    }

    /// `Ok(None)` when neither dialect's manifest is present; `Err` when one is
    /// present but unreadable, malformed, or invalid for its dialect.
    ///
    /// The split matters: the runtime ignores errors (best-effort discovery
    /// must not let one bad plugin blank the library), while the CLI and server
    /// surface them (an install must fail loudly rather than silently drop the
    /// manifest and fall back to conventions).
    ///
    /// **Claude wins when a repo ships both.** It is the manifest aimed at a
    /// reader of this shape, and it is the only one that can express the hooks,
    /// agents and commands horsie runs — Agent Plugins assigns those no
    /// portable meaning. Repos carrying several vendors' manifests side by side
    /// are the norm rather than the exception: `obra/superpowers` ships nine.
    pub fn read(plugin_root: &Path) -> Result<Option<PluginManifest>, String> {
        let claude = Self::claude_path(plugin_root);
        if claude.is_file() {
            return Self::read_claude(&claude).map(Some);
        }
        let portable = Self::agent_plugin_path(plugin_root);
        if portable.is_file() {
            return Self::read_agent_plugin(&portable).map(Some);
        }
        Ok(None)
    }

    fn read_claude(path: &Path) -> Result<PluginManifest, String> {
        let mut raw = parse(path)?;
        let take = |v: Option<StringOrList>| v.map(StringOrList::into_vec).unwrap_or_default();
        let skills = take(raw.skills.take());
        let agents = take(raw.agents.take());
        let commands = take(raw.commands.take());
        Ok(PluginManifest {
            skills,
            agents,
            commands,
            ..common(raw, ManifestDialect::Claude)
        })
    }

    /// The portable dialect, which the spec makes stricter: `$schema` and
    /// `name` are both required, and a manifest missing either "MUST" be
    /// rejected outright rather than read for what it does have.
    ///
    /// The declared component locations are deliberately dropped. A conformant
    /// client reads `skills/` and nothing else, so honouring an override here
    /// would install a tree no other client can see.
    fn read_agent_plugin(path: &Path) -> Result<PluginManifest, String> {
        let raw = parse(path)?;
        match raw.schema.as_deref() {
            Some(s) if s.starts_with(AGENT_PLUGINS_SCHEMA_PREFIX) => {}
            Some(s) => {
                return Err(format!(
                    "plugin.json: '$schema' is '{s}', not an Agent Plugins schema \
                     ({AGENT_PLUGINS_SCHEMA_PREFIX}…)"
                ));
            }
            None => return Err("plugin.json: '$schema' is required".to_string()),
        }
        match raw.name.as_deref() {
            Some(name) => validate_name(name).map_err(|e| format!("plugin.json: {e}"))?,
            None => return Err("plugin.json: 'name' is required".to_string()),
        }
        Ok(common(raw, ManifestDialect::AgentPlugin))
    }
}

/// Everything both dialects share, with the component locations left empty for
/// the caller to fill in or leave.
fn common(raw: RawManifest, dialect: ManifestDialect) -> PluginManifest {
    PluginManifest {
        dialect,
        name: raw.name,
        version: raw.version,
        description: raw.description,
        skills: Vec::new(),
        agents: Vec::new(),
        commands: Vec::new(),
        author: raw.author.and_then(RawAuthor::into_name),
        homepage: raw.homepage,
        repository: raw.repository.and_then(RawRepository::into_url),
        license: raw.license,
        keywords: raw.keywords.unwrap_or_default(),
    }
}

fn parse(path: &Path) -> Result<RawManifest, String> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("plugin.json: {e}"))
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

    fn write_manifest(root: &Path, json: &str) {
        let dir = root.join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), json).unwrap();
    }

    fn write_portable(root: &Path, json: &str) {
        std::fs::write(root.join("plugin.json"), json).unwrap();
    }

    fn portable(name: &str) -> String {
        format!(
            r#"{{"$schema":"{AGENT_PLUGINS_SCHEMA_PREFIX}1.0.0/plugin.schema.json","name":"{name}"}}"#
        )
    }

    #[test]
    fn absent_manifest_is_ok_none() {
        let dir = TempDir::new().unwrap();
        assert_eq!(PluginManifest::read(dir.path()).unwrap(), None);
    }

    #[test]
    fn reads_scalar_fields() {
        let dir = TempDir::new().unwrap();
        write_manifest(
            dir.path(),
            r#"{"name":"impeccable","version":"4.0.4","description":"d"}"#,
        );
        let m = PluginManifest::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.dialect, ManifestDialect::Claude);
        assert_eq!(m.name.as_deref(), Some("impeccable"));
        assert_eq!(m.version.as_deref(), Some("4.0.4"));
        assert_eq!(m.description.as_deref(), Some("d"));
        assert!(m.skills.is_empty());
    }

    #[test]
    fn skills_accepts_string_or_array() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), r#"{"skills":"./.claude/skills/"}"#);
        assert_eq!(
            PluginManifest::read(dir.path()).unwrap().unwrap().skills,
            vec!["./.claude/skills/".to_string()]
        );

        let dir2 = TempDir::new().unwrap();
        write_manifest(dir2.path(), r#"{"skills":["a/skills","b/skills"]}"#);
        assert_eq!(
            PluginManifest::read(dir2.path()).unwrap().unwrap().skills,
            vec!["a/skills".to_string(), "b/skills".to_string()]
        );
    }

    #[test]
    fn malformed_manifest_is_err_not_none() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), "{not json");
        let err = PluginManifest::read(dir.path()).unwrap_err();
        assert!(
            err.contains("plugin.json"),
            "error should name the file: {err}"
        );
    }

    #[test]
    fn reads_the_agent_plugins_dialect() {
        let dir = TempDir::new().unwrap();
        write_portable(
            dir.path(),
            &format!(
                r#"{{"$schema":"{AGENT_PLUGINS_SCHEMA_PREFIX}1.0.0/plugin.schema.json",
                    "name":"acme.tools","version":"1.2.3","description":"d",
                    "author":{{"name":"A","email":"a@x"}},"homepage":"https://h",
                    "repository":"https://r","license":"MIT","keywords":["a","b"]}}"#
            ),
        );
        let m = PluginManifest::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.dialect, ManifestDialect::AgentPlugin);
        assert_eq!(m.name.as_deref(), Some("acme.tools"));
        assert_eq!(m.version.as_deref(), Some("1.2.3"));
        assert_eq!(m.author.as_deref(), Some("A"));
        assert_eq!(m.homepage.as_deref(), Some("https://h"));
        assert_eq!(m.repository.as_deref(), Some("https://r"));
        assert_eq!(m.license.as_deref(), Some("MIT"));
        assert_eq!(m.keywords, vec!["a".to_string(), "b".to_string()]);
    }

    /// The spec fixes `skills/`, so a declared override must not survive into
    /// the parsed manifest — honouring it would build a tree no other
    /// conformant client could read.
    #[test]
    fn agent_plugins_dialect_ignores_declared_component_locations() {
        let dir = TempDir::new().unwrap();
        write_portable(
            dir.path(),
            &format!(
                r#"{{"$schema":"{AGENT_PLUGINS_SCHEMA_PREFIX}1.0.0/plugin.schema.json",
                    "name":"x","skills":"elsewhere","agents":"a","commands":"c"}}"#
            ),
        );
        let m = PluginManifest::read(dir.path()).unwrap().unwrap();
        assert!(m.skills.is_empty());
        assert!(m.agents.is_empty());
        assert!(m.commands.is_empty());
    }

    #[test]
    fn root_manifest_without_schema_is_rejected() {
        let dir = TempDir::new().unwrap();
        write_portable(dir.path(), r#"{"name":"x"}"#);
        let err = PluginManifest::read(dir.path()).unwrap_err();
        assert!(err.contains("$schema"), "{err}");
    }

    #[test]
    fn root_manifest_with_a_foreign_schema_is_rejected() {
        let dir = TempDir::new().unwrap();
        write_portable(dir.path(), r#"{"$schema":"https://example.com/x","name":"x"}"#);
        let err = PluginManifest::read(dir.path()).unwrap_err();
        assert!(err.contains("not an Agent Plugins schema"), "{err}");
    }

    #[test]
    fn root_manifest_without_a_name_is_rejected() {
        let dir = TempDir::new().unwrap();
        write_portable(
            dir.path(),
            &format!(r#"{{"$schema":"{AGENT_PLUGINS_SCHEMA_PREFIX}1.0.0/plugin.schema.json"}}"#),
        );
        let err = PluginManifest::read(dir.path()).unwrap_err();
        assert!(err.contains("'name' is required"), "{err}");
    }

    /// `obra/superpowers` ships nine vendor trees, so this is the common case
    /// rather than a contrived one.
    #[test]
    fn claude_wins_when_a_repo_ships_both() {
        let dir = TempDir::new().unwrap();
        write_manifest(dir.path(), r#"{"name":"from-claude"}"#);
        write_portable(dir.path(), &portable("from-portable"));
        let m = PluginManifest::read(dir.path()).unwrap().unwrap();
        assert_eq!(m.dialect, ManifestDialect::Claude);
        assert_eq!(m.name.as_deref(), Some("from-claude"));
    }

    #[test]
    fn name_grammar_follows_the_spec() {
        for ok in ["my-plugin", "acme.tools", "lint3r", "a", "0"] {
            assert!(validate_name(ok).is_ok(), "{ok} should be valid");
        }
        for bad in [
            "",
            "-lead",
            "trail-",
            ".lead",
            "trail.",
            "Upper",
            "has space",
            "double--hyphen",
            "double..dot",
            "under_score",
        ] {
            assert!(validate_name(bad).is_err(), "{bad} should be invalid");
        }
        assert!(validate_name(&"a".repeat(64)).is_ok());
        assert!(validate_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn join_declared_drops_the_dot_prefix() {
        use std::path::PathBuf;
        let root = Path::new("/r");
        assert_eq!(
            super::super::join_declared(root, "./plugin"),
            PathBuf::from("/r/plugin")
        );
        assert_eq!(
            super::super::join_declared(root, "./.claude/skills/"),
            PathBuf::from("/r/.claude/skills/")
        );
        assert_eq!(
            super::super::join_declared(root, "plugin"),
            PathBuf::from("/r/plugin")
        );
        // A source that points at the repo root itself.
        assert_eq!(super::super::join_declared(root, "."), PathBuf::from("/r"));
        assert_eq!(super::super::join_declared(root, "./"), PathBuf::from("/r"));
    }

    #[test]
    fn source_key_is_stable_and_ref_sensitive() {
        let a = super::super::source_key("https://x/y.git", None);
        let b = super::super::source_key("https://x/y.git", None);
        let c = super::super::source_key("https://x/y.git", Some("v2"));
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 16);
    }
}

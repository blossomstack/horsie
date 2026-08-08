//! `.claude-plugin/plugin.json`.
//!
//! Only the fields horsie uses today are modelled. `mcpServers` is added here —
//! in one place — by #105's Phase 4.

use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PluginManifest {
    pub name: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    /// Skill roots relative to the plugin root. Empty means "not declared" —
    /// callers fall back to the conventional `skills/`.
    pub skills: Vec<String>,
    /// Agent locations relative to the plugin root — a directory of `*.md`, or
    /// a single `.md` file. Empty means "not declared", so callers fall back to
    /// the conventional `agents/`.
    pub agents: Vec<String>,
    /// Command locations relative to the plugin root, same shape as `agents`.
    /// Empty falls back to the conventional `commands/`.
    pub commands: Vec<String>,
}

/// Raw wire shape. `skills` is a string or an array of strings.
#[derive(Deserialize)]
struct RawManifest {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    skills: Option<StringOrList>,
    agents: Option<StringOrList>,
    commands: Option<StringOrList>,
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

impl PluginManifest {
    /// `<plugin_root>/.claude-plugin/plugin.json`.
    pub fn path(plugin_root: &Path) -> PathBuf {
        plugin_root.join(".claude-plugin").join("plugin.json")
    }

    /// `Ok(None)` when absent; `Err` when present but unreadable or malformed.
    ///
    /// The split matters: the runtime ignores errors (best-effort discovery
    /// must not let one bad plugin blank the library), while the CLI and server
    /// surface them (an install must fail loudly rather than silently drop the
    /// manifest and fall back to conventions).
    pub fn read(plugin_root: &Path) -> Result<Option<PluginManifest>, String> {
        let path = Self::path(plugin_root);
        if !path.is_file() {
            return Ok(None);
        }
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let raw: RawManifest =
            serde_json::from_str(&text).map_err(|e| format!("plugin.json: {e}"))?;
        Ok(Some(PluginManifest {
            name: raw.name,
            version: raw.version,
            description: raw.description,
            skills: raw.skills.map(StringOrList::into_vec).unwrap_or_default(),
            agents: raw.agents.map(StringOrList::into_vec).unwrap_or_default(),
            commands: raw.commands.map(StringOrList::into_vec).unwrap_or_default(),
        }))
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

    fn write_manifest(root: &Path, json: &str) {
        let dir = root.join(".claude-plugin");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), json).unwrap();
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

//! Locating a plugin's agents: the manifest `agents` field (string or array of
//! paths relative to the plugin root), else the conventional `agents/`.
//!
//! An entry may name a directory — every `*.md` directly inside it — or a single
//! `.md` file. Both shapes are in the spec, and the file shape is how a plugin
//! declares a subset of what its tree holds.
//!
//! Parsing the files is deliberately not here: the runtime finds them and ships
//! their bytes, the server reads their frontmatter. Same split as skills.

use super::PluginManifest;
use std::path::{Path, PathBuf};

/// Agent roots for a plugin: the manifest override when declared, else `agents/`.
pub fn agent_locations(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf> {
    match manifest.map(|m| m.agents.as_slice()) {
        Some(roots) if !roots.is_empty() => roots
            .iter()
            .map(|r| super::join_declared(plugin_root, r))
            .collect(),
        _ => vec![plugin_root.join("agents")],
    }
}

/// Every agent definition file for a plugin, sorted for stable ordering.
///
/// Paths are built by joining, never canonicalised, for the reason skills are:
/// the shared library installs plugins as symlinks, and canonicalising would
/// leak the link target into the relative ids the agent sees.
pub fn agent_files(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for location in agent_locations(plugin_root, manifest) {
        if location.is_file() {
            if is_markdown(&location) {
                out.push(location);
            }
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&location) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_markdown(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

fn is_markdown(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("md"))
}

/// One agent a plugin declares: what to call it, when to pick it, and how to
/// run it.
///
/// `tools` and `model` are kept **as the plugin wrote them**. Translating
/// Claude's tool names into horsie's, or resolving a model alias against a
/// catalogue this crate cannot see, are decisions for the consumer — and both
/// depend on state (the installed model cards) that a parser has no business
/// reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginAgentDef {
    pub name: String,
    /// What the agent is for. Required, because it is what a model selecting an
    /// agent actually reads — a catalogue of bare names is not a catalogue.
    pub description: String,
    /// Declared `model`, verbatim. Overwhelmingly an alias (`inherit`,
    /// `sonnet`, `opus`) rather than a model id.
    pub model: Option<String>,
    /// Declared `tools`, in Claude's vocabulary. Empty means "not declared",
    /// which is not the same as "no tools" — an explicit empty list is a
    /// declaration horsie has never seen in the wild and reads the same way.
    pub tools: Vec<String>,
    /// The body below the header: the agent's system prompt.
    pub prompt: String,
}

/// Parse one agent definition file.
///
/// `None` when the header is missing, malformed, or lacks `name` or
/// `description`. Both are required by every one of the definitions measured in
/// the wild, and an agent missing either cannot be offered to a model: one is
/// how it is named and the other is how it is chosen.
#[must_use]
pub fn parse(content: &str) -> Option<PluginAgentDef> {
    let (front, body) = crate::frontmatter::split(content)?;
    let mut def = PluginAgentDef {
        name: String::new(),
        description: String::new(),
        model: None,
        tools: Vec::new(),
        prompt: body.trim().to_string(),
    };
    for (key, value) in crate::frontmatter::pairs(front)? {
        match key.as_str() {
            "name" => def.name = value.to_string(),
            "description" => def.description = value.to_string(),
            "model" => def.model = Some(value.to_string()),
            "tools" => def.tools = crate::frontmatter::comma_list(&value),
            // `color`, `effort` and `initialPrompt` are read by nothing, so
            // they are not modelled. `effort` would need the definition when
            // the subagent's *actor* is built, and no library scan exists that
            // early. Anything else is a field of a future spec.
            _ => {}
        }
    }
    if def.name.is_empty() || def.description.is_empty() {
        return None;
    }
    Some(def)
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

    fn write(root: &Path, rel: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "---\nname: a\ndescription: d\n---\nbody").unwrap();
    }

    fn names(paths: &[PathBuf], root: &Path) -> Vec<String> {
        paths
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    /// The only shape measured in the wild: no manifest field, a plain
    /// `agents/` directory.
    #[test]
    fn defaults_to_the_agents_dir() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "agents/reviewer.md");
        write(dir.path(), "agents/explorer.md");
        // Not an agent: only `.md` files are definitions.
        write(dir.path(), "agents/README.txt");
        assert_eq!(
            names(&agent_files(dir.path(), None), dir.path()),
            vec!["agents/explorer.md", "agents/reviewer.md"]
        );
    }

    /// A declared entry may be a single file, which is how a plugin ships a
    /// subset of what its tree holds.
    #[test]
    fn a_manifest_may_name_files_a_directory_or_both() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "custom/one.md");
        write(dir.path(), "custom/two.md");
        write(dir.path(), "elsewhere/three.md");
        write(dir.path(), "agents/ignored.md");
        let manifest = PluginManifest {
            agents: vec!["./custom/one.md".into(), "elsewhere".into()],
            ..PluginManifest::empty(crate::plugin::ManifestDialect::Claude)
        };
        assert_eq!(
            names(&agent_files(dir.path(), Some(&manifest)), dir.path()),
            vec!["custom/one.md", "elsewhere/three.md"],
            "a declared set replaces the default, it does not add to it"
        );
    }

    /// `code-reviewer.md` from the official marketplace, header verbatim. It
    /// uses every key measured across the ecosystem's 31 definitions.
    #[test]
    fn parses_a_real_definition() {
        let def = parse(
            "---\n\
             name: code-reviewer\n\
             description: Reviews code for bugs, logic errors and security issues\n\
             tools: Glob, Grep, LS, Read, NotebookRead, WebFetch, TodoWrite\n\
             model: sonnet\n\
             color: red\n\
             effort: high\n\
             ---\n\n\
             You are an expert code reviewer.\n",
        )
        .unwrap();
        assert_eq!(def.name, "code-reviewer");
        assert!(def.description.starts_with("Reviews code"));
        assert_eq!(def.model.as_deref(), Some("sonnet"));
        assert_eq!(def.tools[0], "Glob");
        assert_eq!(def.tools.len(), 7);
        assert_eq!(def.prompt, "You are an expert code reviewer.");
    }

    /// Both are required: one is how the agent is named, the other is how a
    /// model chooses it. Half a definition is not a definition.
    #[test]
    fn a_definition_without_a_name_or_description_is_not_one() {
        assert!(parse("---\ndescription: d\n---\nbody").is_none());
        assert!(parse("---\nname: n\n---\nbody").is_none());
        assert!(parse("no frontmatter at all").is_none());
    }

    #[test]
    fn undeclared_fields_are_absent_not_empty_strings() {
        let def = parse("---\nname: n\ndescription: d\n---\nbody").unwrap();
        assert_eq!(def.model, None);
        assert!(def.tools.is_empty());
    }

    #[test]
    fn a_missing_or_empty_location_yields_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(agent_files(dir.path(), None).is_empty());
        let manifest = PluginManifest {
            agents: vec!["nowhere".into()],
            ..PluginManifest::empty(crate::plugin::ManifestDialect::Claude)
        };
        assert!(agent_files(dir.path(), Some(&manifest)).is_empty());
    }
}

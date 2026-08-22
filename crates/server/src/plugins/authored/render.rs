//! Rendering an authored plugin's rows into an Agent Plugins 1.0 tree.
//!
//! ```text
//! plugin.json                      ← $schema, name, version, description
//! skills/<skill>/SKILL.md          ← frontmatter + body
//! skills/<skill>/scripts/run.sh    ← whatever files the skill carries
//! ```
//!
//! The portable dialect rather than Claude's, for two reasons. It is the one
//! horsie is choosing from scratch, with no repo full of existing trees to stay
//! compatible with — and rendering what the reader in `horsie_support` parses
//! means the two halves of this feature check each other rather than drifting.

use serde::Serialize;
use std::path::Path;

/// The identifier every rendered manifest declares. Pinned, not derived: it
/// names the shape of the bytes, so a client reading it is entitled to assume
/// exactly this layout.
pub const SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";

/// The manifest, as a struct rather than a map.
///
/// Field order here is the byte order out, whatever `serde_json::Map` happens
/// to be — and it is not always the same thing. A workspace crate elsewhere in
/// the dependency graph enables `serde_json/preserve_order`, so under feature
/// unification a `Map` is insertion-ordered and otherwise it is sorted. Since
/// these bytes are what a bundle's digest is taken over, that would have made
/// the digest a function of how the server was *built* as well as of the rows
/// it was built from.
#[derive(Serialize)]
struct Manifest<'a> {
    #[serde(rename = "$schema")]
    schema: &'a str,
    name: &'a str,
    version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
}

/// One skill as it will be written out.
pub struct RenderedSkill {
    pub name: String,
    pub description: String,
    pub body: String,
    /// Relative to the skill's own directory: `scripts/run.sh`.
    pub files: Vec<(String, String)>,
}

/// Reject a path that would write outside the skill's own directory.
///
/// The contents come from an agent, so this is the one place a traversal could
/// enter — `render` joins these onto a real directory. Rejecting rather than
/// sanitising: a path that had to be rewritten to be safe is not the path
/// anyone meant, and silently relocating a file is worse than refusing it.
pub fn validate_file_path(path: &str) -> Result<(), String> {
    let complaint = |why: &str| Err(format!("file path '{path}': {why}"));
    if path.is_empty() {
        return complaint("must not be empty");
    }
    let p = Path::new(path);
    if p.is_absolute() || path.starts_with('/') || path.starts_with('\\') {
        return complaint("must be relative to the skill's directory");
    }
    for component in p.components() {
        match component {
            std::path::Component::Normal(_) => {}
            std::path::Component::Prefix(_)
            | std::path::Component::RootDir
            | std::path::Component::CurDir
            | std::path::Component::ParentDir => {
                return complaint("must not contain '.', '..' or a drive prefix");
            }
        }
    }
    Ok(())
}

/// Write the tree for one authored plugin into `dir`.
///
/// `generation` becomes the manifest `version` as `0.0.<generation>`. The spec
/// recommends semver and forbids a client from rejecting a version that is not
/// one, but a counter rendered as a patch level is both true and parseable, so
/// there is no reason to make a reader guess.
pub fn render(
    dir: &Path,
    name: &str,
    description: Option<&str>,
    generation: u64,
    skills: &[RenderedSkill],
) -> Result<(), String> {
    let manifest = Manifest {
        schema: SCHEMA,
        name,
        version: format!("0.0.{generation}"),
        description: description.map(str::trim).filter(|d| !d.is_empty()),
    };
    let json =
        serde_json::to_string_pretty(&manifest).map_err(|e| format!("render plugin.json: {e}"))?;
    write(&dir.join("plugin.json"), &json)?;

    for skill in skills {
        let root = dir.join("skills").join(&skill.name);
        // Through the shared renderer, not a format string. Frontmatter is
        // real YAML and these values come from an agent: a description holding
        // `: ` is not a scalar unless it is quoted, and one holding `---` would
        // otherwise close the header early and make the skill invisible to
        // every reader, including this server's own.
        let header = horsie_support::frontmatter::render(&[
            ("name", skill.name.as_str()),
            ("description", skill.description.as_str()),
        ]);
        write(
            &root.join("SKILL.md"),
            &format!("{header}\n{}\n", skill.body.trim_end()),
        )?;
        for (path, content) in &skill.files {
            validate_file_path(path)?;
            write(&root.join(path), content)?;
        }
    }
    Ok(())
}

fn write(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, content).map_err(|e| format!("write {}: {e}", path.display()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_support::plugin::{ManifestDialect, PluginRoot};

    fn skill(name: &str) -> RenderedSkill {
        RenderedSkill {
            name: name.to_string(),
            description: "what it does".to_string(),
            body: "Step 1.".to_string(),
            files: vec![("scripts/run.sh".to_string(), "echo hi".to_string())],
        }
    }

    /// The round trip that keeps the two halves of this feature honest: what
    /// the renderer writes, the reader in `horsie_support` reads back — and
    /// reads back as the portable dialect, not by falling through to
    /// convention.
    #[test]
    fn a_rendered_tree_reads_back_as_an_agent_plugin() {
        let dir = tempfile::tempdir().unwrap();
        render(
            dir.path(),
            "my-notes",
            Some("things I worked out"),
            7,
            &[skill("deploying"), skill("debugging")],
        )
        .unwrap();

        let root = PluginRoot::inspect(dir.path()).unwrap();
        assert_eq!(root.dialect(), ManifestDialect::AgentPlugin);
        assert!(root.is_installable());
        assert_eq!(root.name("fallback"), "my-notes");
        assert_eq!(root.version(), Some("0.0.7"));
        assert_eq!(root.description(), Some("things I worked out"));

        let catalog = horsie_support::plugin::catalog::build(&root);
        let names: Vec<&str> = catalog.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["debugging", "deploying"]);
        assert!(
            dir.path().join("skills/deploying/scripts/run.sh").is_file(),
            "a skill's own files render beside it"
        );
    }

    /// A description carrying `---` would close the header early, and one
    /// carrying `: ` is not a YAML scalar at all. Either way the skill becomes
    /// invisible to every reader horsie has.
    #[test]
    fn a_multiline_description_cannot_break_the_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        render(
            dir.path(),
            "p",
            None,
            1,
            &[RenderedSkill {
                name: "x".into(),
                description: "first\n---\nname: evil".into(),
                body: "b".into(),
                files: vec![],
            }],
        )
        .unwrap();
        let content = std::fs::read_to_string(dir.path().join("skills/x/SKILL.md")).unwrap();
        let (name, description) = horsie_support::plugin::skills::parse(&content)
            .expect("a rendered skill must be readable");
        assert_eq!(name, "x");
        assert_eq!(description, "first\n---\nname: evil");
    }

    #[test]
    fn a_traversing_file_path_is_refused() {
        for bad in ["../escape.sh", "/etc/passwd", "scripts/../../x", "./x", ""] {
            assert!(validate_file_path(bad).is_err(), "{bad} should be refused");
        }
        for ok in ["scripts/run.sh", "references/api.md", "a.txt"] {
            assert!(validate_file_path(ok).is_ok(), "{ok} should be allowed");
        }
    }

    /// The rendered bytes, pinned.
    ///
    /// A digest is recorded when a skill is saved and recomputed when it is
    /// provisioned, so a change to this output is a change every runtime will
    /// re-fetch for. That is fine and sometimes right — but it should be a
    /// decision, and a golden file is what makes it one rather than something
    /// noticed later as bundles churning in production.
    #[test]
    fn the_rendered_bytes_are_pinned() {
        let dir = tempfile::tempdir().unwrap();
        render(
            dir.path(),
            "my-notes",
            Some("things I worked out"),
            7,
            &[RenderedSkill {
                name: "deploying".into(),
                description: "what it does".into(),
                body: "Step 1.".into(),
                files: vec![],
            }],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.path().join("plugin.json")).unwrap(),
            "{\n  \"$schema\": \"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json\",\n  \
             \"name\": \"my-notes\",\n  \"version\": \"0.0.7\",\n  \
             \"description\": \"things I worked out\"\n}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("skills/deploying/SKILL.md")).unwrap(),
            "---\nname: deploying\ndescription: what it does\n---\n\nStep 1.\n"
        );
    }

    /// The same rows must produce the same bytes, or the digest recorded at
    /// save would disagree with the package rendered at fetch.
    #[test]
    fn rendering_is_deterministic() {
        let render_once = || {
            let dir = tempfile::tempdir().unwrap();
            render(dir.path(), "p", Some("d"), 3, &[skill("b"), skill("a")]).unwrap();
            std::fs::read_to_string(dir.path().join("plugin.json")).unwrap()
        };
        assert_eq!(render_once(), render_once());
    }
}

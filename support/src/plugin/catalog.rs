//! What a plugin bundle offers, in one list.
//!
//! A bundle's commands, skills and agents are three file conventions with three
//! parsers, but one question: *what can a user invoke?* This module answers it
//! once, at install time, so nothing downstream has to re-derive it — the
//! settings page, the composer's typeahead and the session seam all read the
//! same catalogue.
//!
//! Pure: [`build`] reads a directory that is already on disk and parses it.
//! No database, no network, no runtime.

use super::PluginRoot;
use serde::{Deserialize, Serialize};

/// Which kind of entry, and therefore how it is invoked and what expanding it
/// means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CatalogKind {
    /// `commands/*.md` — a prompt template, invoked `/name`.
    Command,
    /// `skills/*/SKILL.md` — invoked `/name`, expanding to an instruction that
    /// sends the agent to the skill tool.
    Skill,
    /// `agents/*.md` — invoked `@name`, expanding to a delegation instruction.
    Agent,
}

impl CatalogKind {
    /// The sigil this kind is invoked with. Commands and skills share `/`:
    /// both are "do this thing", and a user should not have to remember which
    /// of the two a bundle chose to ship.
    #[must_use]
    pub fn sigil(self) -> char {
        match self {
            CatalogKind::Command | CatalogKind::Skill => '/',
            CatalogKind::Agent => '@',
        }
    }

    /// The element name the expanded message is framed with.
    #[must_use]
    pub fn element(self) -> &'static str {
        match self {
            CatalogKind::Command => "command",
            CatalogKind::Skill => "skill",
            CatalogKind::Agent => "agent",
        }
    }
}

/// One thing a bundle offers.
///
/// `template` is `Some` only for a command — it is the body the server
/// substitutes. It is deliberately absent from the wire type clients receive:
/// the server expands, so no client needs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub kind: CatalogKind,
    pub name: String,
    pub description: String,
    /// `argument-hint`, shown beside the name in a picker. Commands only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
    /// The prompt template. Commands only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// Everything an inspected plugin offers, sorted by kind then name so a listing
/// is stable across installs.
///
/// Best-effort per file: one unparseable definition is skipped, never fatal.
/// A bundle is not a compiler, and refusing to install it over a malformed
/// `SKILL.md` would lose the twelve that are fine.
#[must_use]
pub fn build(root: &PluginRoot) -> Vec<CatalogEntry> {
    let mut out = Vec::new();

    for file in &root.command_files {
        let Some(name) = super::commands::name_of(file) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        if let Some(def) = super::commands::parse(&name, &content) {
            out.push(CatalogEntry {
                kind: CatalogKind::Command,
                name: def.name,
                description: def.description,
                argument_hint: def.argument_hint,
                template: Some(def.template),
            });
        }
    }

    for dir in &root.skill_dirs {
        let Ok(content) = std::fs::read_to_string(dir.join("SKILL.md")) else {
            continue;
        };
        if let Some((name, description)) = super::skills::parse(&content) {
            out.push(CatalogEntry {
                kind: CatalogKind::Skill,
                name,
                description,
                argument_hint: None,
                template: None,
            });
        }
    }

    for file in &root.agent_files {
        let Ok(content) = std::fs::read_to_string(file) else {
            continue;
        };
        if let Some(def) = super::agents::parse(&content) {
            out.push(CatalogEntry {
                kind: CatalogKind::Agent,
                name: def.name,
                description: def.description,
                argument_hint: None,
                template: None,
            });
        }
    }

    out.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.name.cmp(&b.name)));
    out
}

/// Frame an expanded invocation as the message the model reads.
///
/// The same shape hook-injected context uses, and for the same reason: a client
/// must be able to tell an invocation from something the person typed by hand,
/// and the model must read the body rather than the frame.
///
/// `name` and `args` are XML-escaped, and a newline in `args` becomes a space —
/// in the attribute only, since the body already carries the real arguments.
/// The attribute exists so a renderer can show `/review src/foo.rs` and keep
/// the body collapsed, and an argument containing a quote would otherwise break
/// the frame that renderer parses.
#[must_use]
pub fn frame(kind: CatalogKind, name: &str, args: &str, body: &str) -> String {
    let el = kind.element();
    if args.is_empty() {
        format!("<{el} name=\"{}\">\n{body}\n</{el}>", attr(name))
    } else {
        format!(
            "<{el} name=\"{}\" args=\"{}\">\n{body}\n</{el}>",
            attr(name),
            attr(args)
        )
    }
}

fn attr(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
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
    use std::path::Path;
    use tempfile::TempDir;

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn a_bundle_catalogues_all_three_kinds_sorted() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "commands/commit.md",
            "---\ndescription: Create a git commit\nargument-hint: <msg>\n---\nCommit $ARGUMENTS",
        );
        write(
            dir.path(),
            "skills/tdd/SKILL.md",
            "---\nname: tdd\ndescription: write tests first\n---\nbody",
        );
        write(
            dir.path(),
            "agents/reviewer.md",
            "---\nname: reviewer\ndescription: reviews a diff\n---\nbe a reviewer",
        );
        let root = PluginRoot::inspect(dir.path()).unwrap();
        let entries = build(&root);

        assert_eq!(
            entries
                .iter()
                .map(|e| (e.kind, e.name.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (CatalogKind::Command, "commit"),
                (CatalogKind::Skill, "tdd"),
                (CatalogKind::Agent, "reviewer"),
            ],
            "sorted by kind then name, so a listing is stable"
        );
        let commit = &entries[0];
        assert_eq!(commit.description, "Create a git commit");
        assert_eq!(commit.argument_hint.as_deref(), Some("<msg>"));
        assert_eq!(commit.template.as_deref(), Some("Commit $ARGUMENTS"));
        // Only a command has a body to expand.
        assert!(entries[1].template.is_none());
        assert!(entries[2].template.is_none());
    }

    /// An entry a picker cannot label cannot be offered, so it is skipped —
    /// and skipping it must not cost the bundle its other entries.
    #[test]
    fn an_entry_without_a_description_is_skipped_not_fatal() {
        let dir = TempDir::new().unwrap();
        write(
            dir.path(),
            "commands/bad.md",
            "---\nargument-hint: x\n---\nb",
        );
        write(
            dir.path(),
            "commands/good.md",
            "---\ndescription: fine\n---\nb",
        );
        write(
            dir.path(),
            "skills/nameless/SKILL.md",
            "---\nname: n\n---\nb",
        );
        write(dir.path(), "agents/bad.md", "---\nname: only\n---\nb");
        let root = PluginRoot::inspect(dir.path()).unwrap();
        let entries = build(&root);
        assert_eq!(
            entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            vec!["good"]
        );
    }

    #[test]
    fn a_frame_escapes_its_attributes() {
        let framed = frame(CatalogKind::Command, "review", "a \"b\" & <c>", "body");
        assert!(
            framed
                .starts_with("<command name=\"review\" args=\"a &quot;b&quot; &amp; &lt;c&gt;\">"),
            "{framed}"
        );
        // The body keeps the real arguments; only the attribute is normalised.
        let multiline = frame(CatalogKind::Skill, "tdd", "first\nsecond", "real\nbody");
        assert!(multiline.contains("args=\"first second\""), "{multiline}");
        assert!(multiline.contains("real\nbody"), "{multiline}");
        assert!(multiline.starts_with("<skill "), "{multiline}");
        // No arguments, no attribute.
        assert!(!frame(CatalogKind::Agent, "r", "", "b").contains("args="));
    }

    #[test]
    fn kinds_carry_their_sigil() {
        assert_eq!(CatalogKind::Command.sigil(), '/');
        assert_eq!(CatalogKind::Skill.sigil(), '/');
        assert_eq!(CatalogKind::Agent.sigil(), '@');
    }
}

//! Locating, parsing and expanding a plugin's slash commands.
//!
//! A command is a `commands/*.md` whose **filename is its name** — the format
//! has no `name` field — and whose body is a prompt template the user invokes as
//! `/name args`.
//!
//! Expansion is pure: `` !`cmd` `` outputs are handed in already run, so the
//! substitution engine does no I/O and is testable without a sandbox.

use super::PluginManifest;
use std::path::{Path, PathBuf};

/// Command roots for a plugin: the manifest override when declared, else
/// `commands/`.
pub fn command_locations(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf> {
    match manifest.map(|m| m.commands.as_slice()) {
        Some(roots) if !roots.is_empty() => roots
            .iter()
            .map(|r| super::join_declared(plugin_root, r))
            .collect(),
        _ => vec![plugin_root.join("commands")],
    }
}

/// Every command definition file, sorted. Directories only — no recursion,
/// because no published plugin nests them and a nested name has no spelling.
pub fn command_files(plugin_root: &Path, manifest: Option<&PluginManifest>) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for location in command_locations(plugin_root, manifest) {
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

/// The command name a file declares — its stem, since the format has no `name`.
#[must_use]
pub fn name_of(path: &Path) -> Option<String> {
    Some(path.file_stem()?.to_string_lossy().into_owned())
}

/// One slash command: how it is invoked, and what it expands to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginCommandDef {
    /// Invoked as `/name`. Taken from the filename.
    pub name: String,
    /// Required: it is what a picker lists and what tells a user what this does.
    pub description: String,
    /// `argument-hint`, shown beside the name in a picker.
    pub argument_hint: Option<String>,
    /// The template below the header.
    pub template: String,
}

/// Parse one command file. `name` comes from the caller because it is the
/// filename, which the content does not carry.
///
/// `None` when the header is missing, malformed, or declares no `description`.
#[must_use]
pub fn parse(name: &str, content: &str) -> Option<PluginCommandDef> {
    let (front, body) = crate::frontmatter::split(content)?;
    let mut def = PluginCommandDef {
        name: name.to_string(),
        description: String::new(),
        argument_hint: None,
        template: body.trim().to_string(),
    };
    for (key, value) in crate::frontmatter::pairs(front)? {
        match key {
            "description" => def.description = value.to_string(),
            "argument-hint" => def.argument_hint = Some(value.to_string()),
            // `allowed-tools` has no consumer: horsie does not run a template's
            // `` !`cmd` `` snippets, and narrowing a turn's toolbox from a
            // command is a separate decision. `disable-model-invocation` and
            // `hide-from-slash-command-tool` both describe a slash-command tool
            // offered to the *model*, which horsie does not have.
            _ => {}
        }
    }
    (!def.description.is_empty()).then_some(def)
}

/// A prompt that names an entry: `<sigil>name` optionally followed by
/// arguments. `/` names a command or a skill, `@` an agent.
///
/// `None` for anything else, including a bare sigil and a message that merely
/// contains one. Deliberately only the leading token — a paragraph mentioning
/// `/etc/hosts` is prose, not an invocation.
#[must_use]
pub fn parse_invocation(prompt: &str, sigil: char) -> Option<(&str, &str)> {
    let rest = prompt.strip_prefix(sigil)?;
    let name_end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let name = &rest[..name_end];
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    // Everything after the name is arguments, the rest of the first line and any
    // following lines alike — a command taking a paragraph is ordinary.
    Some((name, rest[name_end..].trim_start()))
}

/// Split an argument string the way a shell would, honouring `'` and `"`.
///
/// Not a shell: no escapes, no expansion, no globbing. Just enough to make
/// `$1` mean what a template author means by it when an argument has a space.
#[must_use]
pub fn split_args(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for ch in args.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => current.push(c),
            (None, c @ ('\'' | '"')) => {
                quote = Some(c);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started || !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (None, c) => current.push(c),
        }
    }
    if started || !current.is_empty() {
        out.push(current);
    }
    out
}

/// Substitute a command's template.
///
/// `$ARGUMENTS` is everything the user typed after the name; `$1`..`$9` are
/// positional. An unset position substitutes nothing, which is what a template
/// with an optional tail expects — not a literal `$3`.
///
/// `` !`cmd` `` is *not* run. Two of the 29 published commands interpolate a
/// shell, both gathering `git status`-shaped context; a template can simply ask
/// the agent to run those, and it has bash. The snippet is left as written so a
/// reader can see what the author meant.
#[must_use]
pub fn expand(template: &str, args: &str) -> String {
    let positional = split_args(args);
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(after) = tail.strip_prefix("$ARGUMENTS") {
            out.push_str(args);
            rest = after;
            continue;
        }
        if let Some(after) = tail.strip_prefix('$')
            && let Some(digit) = after.chars().next().and_then(|c| c.to_digit(10))
            && digit > 0
        {
            if let Some(value) = positional.get(digit as usize - 1) {
                out.push_str(value);
            }
            rest = &after[1..];
            continue;
        }
        // A `$` that starts nothing is literal text.
        out.push_str(&tail[..1]);
        rest = &tail[1..];
    }
    out.push_str(rest);
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
    use tempfile::TempDir;

    #[test]
    fn a_commands_name_is_its_filename() {
        let dir = TempDir::new().unwrap();
        let cmds = dir.path().join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("review.md"), "---\ndescription: d\n---\nbody").unwrap();
        std::fs::write(cmds.join("notes.txt"), "not a command").unwrap();
        let files = command_files(dir.path(), None);
        assert_eq!(files.len(), 1);
        assert_eq!(name_of(&files[0]).as_deref(), Some("review"));
    }

    /// Every one of the 29 published commands sets `description`; it is what a
    /// picker lists, so one without it cannot be offered.
    #[test]
    fn a_command_without_a_description_is_not_one() {
        assert!(parse("x", "---\nargument-hint: <file>\n---\nbody").is_none());
        assert!(parse("x", "no frontmatter").is_none());
        let def = parse(
            "review",
            "---\ndescription: reviews\nargument-hint: <path>\nallowed-tools: Bash, Read\n---\ncheck $1",
        )
        .unwrap();
        assert_eq!(def.name, "review");
        assert_eq!(def.argument_hint.as_deref(), Some("<path>"));
        assert_eq!(def.template, "check $1");
    }

    #[test]
    fn an_invocation_is_a_leading_sigil_name() {
        assert_eq!(
            parse_invocation("/review src/a.rs", '/'),
            Some(("review", "src/a.rs"))
        );
        assert_eq!(parse_invocation("/review", '/'), Some(("review", "")));
        assert_eq!(parse_invocation("/re-view_2", '/'), Some(("re-view_2", "")));
        // `@` names an agent, and the two sigils never answer for each other.
        assert_eq!(
            parse_invocation("@reviewer this", '@'),
            Some(("reviewer", "this"))
        );
        assert!(parse_invocation("@reviewer", '/').is_none());
        assert!(parse_invocation("/review", '@').is_none());
        // Not invocations: a bare sigil, a path, prose containing one.
        assert!(parse_invocation("/", '/').is_none());
        assert!(parse_invocation("/etc/hosts", '/').is_none());
        assert!(parse_invocation("see /review for details", '/').is_none());
        assert!(parse_invocation("hello", '/').is_none());
        // An email is not an agent, which is why `@` is leading-token only.
        assert!(parse_invocation("mail me at a@b.com", '@').is_none());
    }

    /// A command taking a paragraph is ordinary, so arguments run past the
    /// first line.
    #[test]
    fn arguments_span_the_whole_remainder() {
        let (name, args) = parse_invocation("/summarize the first\nand the second", '/').unwrap();
        assert_eq!(name, "summarize");
        assert_eq!(args, "the first\nand the second");
        let (_, only_tail) = parse_invocation("/summarize\nbody only", '/').unwrap();
        assert_eq!(only_tail, "body only");
    }

    #[test]
    fn args_split_like_a_shell_without_being_one() {
        assert_eq!(split_args("a b  c"), ["a", "b", "c"]);
        assert_eq!(split_args("\"two words\" x"), ["two words", "x"]);
        assert_eq!(
            split_args("'' x"),
            ["", "x"],
            "an empty quoted arg is an arg"
        );
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn substitutes_arguments_positionally_and_wholesale() {
        assert_eq!(expand("run $ARGUMENTS now", "a b"), "run a b now");
        assert_eq!(expand("$1 then $2", "a b"), "a then b");
        // An unset position leaves nothing, not a literal `$3`.
        assert_eq!(expand("[$1][$3]", "only"), "[only][]");
        // A `$` that starts nothing is text.
        assert_eq!(expand("costs $5.00 and $x", ""), "costs .00 and $x");
    }

    /// horsie does not run a template's shell snippets, so one survives
    /// substitution verbatim rather than vanishing.
    #[test]
    fn a_shell_snippet_is_left_as_written() {
        assert_eq!(expand("on !`git status` now", ""), "on !`git status` now");
        assert_eq!(expand("hi! there", ""), "hi! there");
    }
}

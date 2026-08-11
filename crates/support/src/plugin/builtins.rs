//! Slash commands horsie answers itself.
//!
//! A built-in is not a prompt. `/compact` does not expand into something the
//! model reads — it asks the server to do something, and the model never sees
//! the invocation at all. That is the whole reason this is a separate table
//! rather than a bundle that ships with the product: a bundle's command is a
//! template, and no template can compact a session.
//!
//! Two consequences fall out of that, and both are the point of the table:
//!
//! - **Built-ins are offered even when a session has no plugins.** The plugin
//!   catalogue is empty for a session with `use_plugins` false, and a built-in
//!   that vanished there would be missing from exactly the plainest session.
//! - **A bundle cannot shadow one.** Resolution consults this first, so
//!   installing a marketplace plugin that happens to define `/compact` cannot
//!   quietly take over a control the product owns.

/// One command the server answers itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Builtin {
    /// Typed after `/`.
    pub name: &'static str,
    /// Shown in the typeahead.
    pub description: &'static str,
    /// `argument-hint`, shown beside the name, when it takes arguments.
    pub argument_hint: Option<&'static str>,
}

/// Every built-in command, in the order a typeahead should offer them.
pub const BUILTINS: &[Builtin] = &[Builtin {
    name: "compact",
    description: "Summarise earlier history to free up context. The full \
                  transcript stays readable.",
    argument_hint: Some("[what to keep]"),
}];

/// The built-in `name` names, if it names one.
#[must_use]
pub fn builtin(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

/// The built-ins as catalogue entries, for the `/` typeahead.
///
/// Typed `command` so a client needs no new kind to render them: from the
/// composer's point of view a built-in behaves exactly like a bundle's command,
/// and only the server needs to know the difference.
#[must_use]
pub fn catalogue_entries() -> Vec<horsie_models::plugins::CatalogEntryView> {
    BUILTINS
        .iter()
        .map(|b| horsie_models::plugins::CatalogEntryView {
            kind: "command".to_string(),
            name: b.name.to_string(),
            description: b.description.to_string(),
            argument_hint: b.argument_hint.map(ToString::to_string),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_is_offered_to_the_typeahead() {
        let entries = catalogue_entries();
        assert_eq!(entries.len(), BUILTINS.len());
        for b in BUILTINS {
            assert!(
                entries
                    .iter()
                    .any(|e| e.name == b.name && e.kind == "command"),
                "{} is not offered",
                b.name
            );
        }
    }

    #[test]
    fn a_builtin_is_found_by_name_and_nothing_else_is() {
        assert_eq!(builtin("compact").map(|b| b.name), Some("compact"));
        assert!(builtin("Compact").is_none(), "names are exact");
        assert!(builtin("compact-all").is_none());
        assert!(builtin("").is_none());
    }

    /// Two built-ins with one name would make resolution order decide which
    /// one runs, which is not a thing anybody should have to know.
    #[test]
    fn builtin_names_are_unique() {
        let mut names: Vec<&str> = BUILTINS.iter().map(|b| b.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate builtin name");
    }
}

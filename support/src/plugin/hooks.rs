//! Reading and classifying `<plugin>/hooks/hooks.json`.
//!
//! Claude Code documents 31 hook events. horsie classifies every one of them and
//! runs only those it has a call site for — it must never claim to run a guard
//! it silently drops. The classification is exhaustive so an event added
//! upstream surfaces as `Unknown` rather than passing for something supported.

use std::path::Path;

/// A hook event horsie can actually run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    PreToolUse,
    PostToolUse,
    // A later change adds SessionEnd, UserPromptSubmit, Stop, SubagentStart and
    // SubagentStop alongside their call sites.
}

/// Why an event cannot run, which decides what the error tells the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// horsie has the seam; there is no call site for it yet.
    NotImplemented,
    /// horsie has no such concept.
    NoConcept,
    /// Not a documented Claude Code event at all.
    Unknown,
}

impl HookEvent {
    /// Classify a documented event name. `Err` carries why horsie cannot run it.
    pub fn parse(name: &str) -> Result<HookEvent, Unsupported> {
        match name {
            "SessionStart" => Ok(HookEvent::SessionStart),
            "PreToolUse" => Ok(HookEvent::PreToolUse),
            "PostToolUse" => Ok(HookEvent::PostToolUse),

            // Wired in a follow-up, when their call sites land. Classified as
            // deferred rather than supported so no hook can install believing
            // it works and then silently never fire.
            "SessionEnd" | "UserPromptSubmit" | "Stop" | "SubagentStart" | "SubagentStop"
            // horsie has the seam; nothing published uses these, so they are
            // deliberately not built. Promoting one is a small change.
            | "PostToolUseFailure" | "PostToolBatch" | "StopFailure" | "Notification"
            | "TaskCreated" | "TaskCompleted" | "CwdChanged" => Err(Unsupported::NotImplemented),

            // No horsie concept: no slash commands, no permission model, no
            // context compaction, no file watcher, no worktrees, no agent
            // teams, no MCP elicitation, no display layer.
            "UserPromptExpansion" | "PermissionRequest" | "PermissionDenied" | "PreCompact"
            | "PostCompact" | "FileChanged" | "ConfigChange" | "DirectoryAdded" | "Setup"
            | "MessageDisplay" | "TeammateIdle" | "WorktreeCreate" | "WorktreeRemove"
            | "Elicitation" | "ElicitationResult" | "InstructionsLoaded" => {
                Err(Unsupported::NoConcept)
            }

            _ => Err(Unsupported::Unknown),
        }
    }

    /// The documented name, so an outcome can be attributed on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
        }
    }
}

impl Unsupported {
    /// A sentence naming the event and what the user can do about it.
    pub fn explain(&self, event: &str) -> String {
        match self {
            Unsupported::NotImplemented => format!(
                "'{event}' is a Claude Code hook horsie has not implemented yet \
                 — open an issue if you need it"
            ),
            Unsupported::NoConcept => {
                format!("'{event}' has no equivalent in horsie, so its hook can never run")
            }
            Unsupported::Unknown => {
                format!("'{event}' is not a known Claude Code hook event")
            }
        }
    }
}

/// Claude Code tool names for a horsie tool.
///
/// Matchers published for Claude Code name Claude's tools — every matcher in
/// the official marketplace is one of `Bash` or
/// `Edit|Write|MultiEdit|NotebookEdit`. horsie's tools are snake_case and match
/// none of them, so without this table no published plugin's hook would ever
/// fire. Grok Build solves it the same way.
pub fn claude_aliases(horsie_tool: &str) -> &'static [&'static str] {
    match horsie_tool {
        "bash" => &["Bash"],
        "read_file" => &["Read"],
        "write_file" => &["Write"],
        // Both of horsie's in-place editors answer to Claude's edit tools.
        "find_and_replace" | "replace_lines" => &["Edit", "MultiEdit", "NotebookEdit"],
        "list_files" => &["LS"],
        "glob" => &["Glob"],
        "grep" => &["Grep"],
        // `set_env` and `set_working_dir` have no Claude equivalent.
        _ => &[],
    }
}

/// Whether a hook's `matcher` selects a tool call.
///
/// The matcher is a regex, tested unanchored against the horsie tool name and
/// each of its Claude aliases. An absent or empty matcher selects every tool.
/// A matcher that fails to compile selects nothing: a broken pattern must not
/// silently widen into "everything".
pub fn matcher_applies(matcher: Option<&str>, horsie_tool: &str) -> bool {
    let Some(pattern) = matcher.map(str::trim).filter(|m| !m.is_empty()) else {
        return true;
    };
    let Ok(re) = regex::Regex::new(pattern) else {
        return false;
    };
    re.is_match(horsie_tool)
        || claude_aliases(horsie_tool)
            .iter()
            .any(|alias| re.is_match(alias))
}

/// One runnable hook command, already classified and located.
#[derive(Debug, Clone)]
pub struct HookDecl {
    pub event: HookEvent,
    pub matcher: Option<String>,
    pub command: String,
    /// Per-hook budget in seconds; `None` means the caller's default.
    pub timeout: Option<u64>,
}

/// Everything a plugin declares, split into what horsie can run and what it
/// cannot.
#[derive(Debug, Clone, Default)]
pub struct PluginHooks {
    pub decls: Vec<HookDecl>,
    pub unsupported: Vec<(String, Unsupported)>,
}

/// Read `<plugin_root>/hooks/hooks.json`.
///
/// An absent file is an empty set. A present but malformed file is an error —
/// silently treating it as "no hooks" is the failure mode this whole feature
/// exists to remove.
pub fn read(plugin_root: &Path) -> Result<PluginHooks, String> {
    let path = plugin_root.join("hooks").join("hooks.json");
    if !path.is_file() {
        return Ok(PluginHooks::default());
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("hooks.json: {e}"))?;

    let Some(events) = json.get("hooks").and_then(serde_json::Value::as_object) else {
        return Ok(PluginHooks::default());
    };

    let mut out = PluginHooks::default();
    for (name, matchers) in events {
        let event = match HookEvent::parse(name) {
            Ok(e) => e,
            Err(reason) => {
                out.unsupported.push((name.clone(), reason));
                continue;
            }
        };
        let Some(matchers) = matchers.as_array() else {
            continue;
        };
        for entry in matchers {
            let matcher = entry
                .get("matcher")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let Some(hooks) = entry.get("hooks").and_then(serde_json::Value::as_array) else {
                continue;
            };
            for hook in hooks {
                if hook.get("type").and_then(serde_json::Value::as_str) != Some("command") {
                    continue;
                }
                let Some(command) = hook.get("command").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                out.decls.push(HookDecl {
                    event,
                    matcher: matcher.clone(),
                    command: command.to_string(),
                    timeout: hook.get("timeout").and_then(serde_json::Value::as_u64),
                });
            }
        }
    }
    Ok(out)
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

    /// Every documented Claude Code event, as of the 2026-08-02 docs.
    const ALL_31: [&str; 31] = [
        "SessionStart",
        "Setup",
        "UserPromptSubmit",
        "UserPromptExpansion",
        "PreToolUse",
        "PermissionRequest",
        "PermissionDenied",
        "PostToolUse",
        "PostToolUseFailure",
        "PostToolBatch",
        "Notification",
        "MessageDisplay",
        "SubagentStart",
        "SubagentStop",
        "TaskCreated",
        "TaskCompleted",
        "Stop",
        "StopFailure",
        "TeammateIdle",
        "InstructionsLoaded",
        "ConfigChange",
        "CwdChanged",
        "DirectoryAdded",
        "FileChanged",
        "WorktreeCreate",
        "WorktreeRemove",
        "PreCompact",
        "PostCompact",
        "Elicitation",
        "ElicitationResult",
        "SessionEnd",
    ];

    /// Pins the contract: adding a variant cannot silently change what horsie
    /// claims to support.
    #[test]
    fn every_documented_event_is_classified_with_the_expected_counts() {
        let mut supported = 0;
        let mut not_implemented = 0;
        let mut no_concept = 0;
        for name in ALL_31 {
            match HookEvent::parse(name) {
                Ok(_) => supported += 1,
                Err(Unsupported::NotImplemented) => not_implemented += 1,
                Err(Unsupported::NoConcept) => no_concept += 1,
                Err(Unsupported::Unknown) => {
                    panic!("{name} is documented but classified Unknown")
                }
            }
        }
        // Wiring the five turn/subagent events makes these 8 / 7 / 16.
        // Changing them is a deliberate act, not a side effect.
        assert_eq!(supported, 3, "supported set changed");
        assert_eq!(not_implemented, 12, "deferred set changed");
        assert_eq!(no_concept, 16, "absent set changed");
    }

    #[test]
    fn the_supported_three_are_exactly_these() {
        for name in ["SessionStart", "PreToolUse", "PostToolUse"] {
            assert!(HookEvent::parse(name).is_ok(), "{name} must be supported");
        }
    }

    /// The turn and subagent events must classify as deferred, not supported —
    /// horsie has no call site for them yet, and claiming them would mean a
    /// hook that installs and silently never fires.
    #[test]
    fn the_unwired_events_are_deferred_until_their_call_sites_exist() {
        for name in [
            "SessionEnd",
            "UserPromptSubmit",
            "Stop",
            "SubagentStart",
            "SubagentStop",
        ] {
            assert_eq!(HookEvent::parse(name), Err(Unsupported::NotImplemented));
        }
    }

    #[test]
    fn round_trips_through_as_str() {
        for name in ["PreToolUse", "PostToolUse", "SessionStart"] {
            assert_eq!(HookEvent::parse(name).unwrap().as_str(), name);
        }
    }

    #[test]
    fn an_undocumented_name_is_unknown_not_a_silent_pass() {
        assert_eq!(HookEvent::parse("PreFlarbulate"), Err(Unsupported::Unknown));
        assert_eq!(HookEvent::parse("pretooluse"), Err(Unsupported::Unknown));
    }

    #[test]
    fn each_reason_explains_itself_differently() {
        let deferred = Unsupported::NotImplemented.explain("PostToolBatch");
        assert!(deferred.contains("PostToolBatch"), "{deferred}");
        assert!(deferred.contains("not implemented"), "{deferred}");

        let absent = Unsupported::NoConcept.explain("WorktreeCreate");
        assert!(absent.contains("no equivalent"), "{absent}");

        let unknown = Unsupported::Unknown.explain("Nonsense");
        assert!(unknown.contains("not a known"), "{unknown}");
    }

    /// The three matchers that actually exist across the official marketplace.
    /// If aliasing regresses, no published plugin's hook fires at all.
    #[test]
    fn real_world_matchers_hit_the_right_horsie_tools() {
        let edits = Some("Edit|Write|MultiEdit|NotebookEdit");
        assert!(matcher_applies(edits, "write_file"));
        assert!(matcher_applies(edits, "find_and_replace"));
        assert!(matcher_applies(edits, "replace_lines"));
        assert!(!matcher_applies(edits, "bash"));
        assert!(!matcher_applies(edits, "read_file"));

        assert!(matcher_applies(Some("Bash"), "bash"));
        assert!(!matcher_applies(Some("Bash"), "write_file"));
    }

    #[test]
    fn an_absent_or_empty_matcher_matches_everything() {
        assert!(matcher_applies(None, "bash"));
        assert!(matcher_applies(Some(""), "read_file"));
    }

    #[test]
    fn a_matcher_may_name_the_horsie_tool_directly() {
        assert!(matcher_applies(Some("write_file"), "write_file"));
    }

    /// Anchors are used in the wild (`^claude-security:claude-security$`), so
    /// matchers are real regexes rather than split alternations.
    #[test]
    fn anchors_are_honoured() {
        assert!(matcher_applies(Some("^bash$"), "bash"));
        assert!(!matcher_applies(Some("^ash$"), "bash"));
    }

    /// A matcher that will not compile must not match everything by accident.
    #[test]
    fn an_invalid_regex_matches_nothing() {
        assert!(!matcher_applies(Some("(unclosed"), "bash"));
    }

    #[test]
    fn tools_without_a_claude_equivalent_alias_to_nothing() {
        assert!(claude_aliases("set_env").is_empty());
        assert_eq!(claude_aliases("bash"), ["Bash"]);
    }

    fn write_hooks(root: &Path, json: &str) {
        let dir = root.join("hooks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hooks.json"), json).unwrap();
    }

    #[test]
    fn absent_hooks_file_is_empty_not_an_error() {
        let dir = TempDir::new().unwrap();
        let h = read(dir.path()).unwrap();
        assert!(h.decls.is_empty());
        assert!(h.unsupported.is_empty());
    }

    /// impeccable's shape: matcher plus a per-hook timeout.
    #[test]
    fn reads_matcher_command_and_timeout() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{"PostToolUse":[{"matcher":"Edit|Write","hooks":[
                 {"type":"command","command":"node hook.mjs","timeout":5}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 1);
        assert_eq!(h.decls[0].event, HookEvent::PostToolUse);
        assert_eq!(h.decls[0].matcher.as_deref(), Some("Edit|Write"));
        assert_eq!(h.decls[0].command, "node hook.mjs");
        assert_eq!(h.decls[0].timeout, Some(5));
    }

    #[test]
    fn several_events_and_several_hooks_each_are_all_collected() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{
                 "PreToolUse":[{"hooks":[{"type":"command","command":"a"},
                                         {"type":"command","command":"b"}]}],
                 "SessionStart":[{"hooks":[{"type":"command","command":"c"}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 3);
        assert_eq!(
            h.decls
                .iter()
                .filter(|d| d.event == HookEvent::PreToolUse)
                .count(),
            2
        );
    }

    #[test]
    fn unsupported_events_are_collected_with_their_reason_not_dropped() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{
                 "PreToolUse":[{"hooks":[{"type":"command","command":"ok"}]}],
                 "PostToolBatch":[{"hooks":[{"type":"command","command":"x"}]}],
                 "WorktreeCreate":[{"hooks":[{"type":"command","command":"y"}]}],
                 "Bogus":[{"hooks":[{"type":"command","command":"z"}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 1, "only the supported event runs");
        let mut reasons = h.unsupported;
        reasons.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            reasons,
            vec![
                ("Bogus".to_string(), Unsupported::Unknown),
                ("PostToolBatch".to_string(), Unsupported::NotImplemented),
                ("WorktreeCreate".to_string(), Unsupported::NoConcept),
            ]
        );
    }

    /// Only `type: "command"` hooks are runnable; anything else is ignored
    /// rather than mistaken for one.
    #[test]
    fn non_command_hooks_are_skipped() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{"PreToolUse":[{"hooks":[
                 {"type":"http","url":"https://x"},
                 {"type":"command","command":"real"}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 1);
        assert_eq!(h.decls[0].command, "real");
    }

    #[test]
    /// impeccable's real `hooks/hooks.json`, verbatim. It is the plugin that
    /// motivated all of #105, so its exact shape is worth pinning: one event
    /// horsie runs today and one it defers, with a matcher that only selects
    /// the right tools because of the Claude alias table.
    #[test]
    fn impeccables_real_hooks_split_as_expected() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{
                 "PostToolUse":[{"matcher":"Edit|Write|MultiEdit","hooks":[
                   {"type":"command","command":"node hook.mjs","timeout":5}]}],
                 "Stop":[{"hooks":[
                   {"type":"command","command":"node hook.mjs","timeout":30}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();

        assert_eq!(h.decls.len(), 1);
        assert_eq!(h.decls[0].event, HookEvent::PostToolUse);
        assert_eq!(
            h.unsupported,
            vec![("Stop".to_string(), Unsupported::NotImplemented)]
        );

        // The matcher must reach horsie's editors and nothing else.
        let m = h.decls[0].matcher.as_deref();
        assert!(matcher_applies(m, "write_file"));
        assert!(matcher_applies(m, "find_and_replace"));
        assert!(!matcher_applies(m, "bash"));
        assert!(!matcher_applies(m, "read_file"));
    }

    #[test]
    fn a_malformed_hooks_file_is_an_error_not_an_empty_set() {
        let dir = TempDir::new().unwrap();
        write_hooks(dir.path(), "{not json");
        let err = read(dir.path()).unwrap_err();
        assert!(err.contains("hooks.json"), "{err}");
    }
}

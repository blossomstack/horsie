//! Every Claude Code hook event horsie has a seam for, described once.
//!
//! Three facts per event, each of which the rest of the system used to
//! re-derive: what its stdin payload looks like (see `invoke.rs`), what its
//! `matcher` selects on, and which output fields it may set. Claude Code
//! documents 31 events; the fifteen absent here need a horsie subsystem that
//! does not exist, so they are refused rather than modelled.

/// A hook event horsie can describe.
///
/// Being here is protocol knowledge, not a capability: [`HookEvent::is_wired`]
/// is what says horsie has a call site to fire it from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PostToolBatch,
    SessionStart,
    SessionEnd,
    UserPromptSubmit,
    UserPromptExpansion,
    Stop,
    StopFailure,
    SubagentStart,
    SubagentStop,
    TaskCreated,
    TaskCompleted,
    Notification,
    CwdChanged,
    PreCompact,
    PostCompact,
}

/// Why a declared hook cannot run, which decides what the error tells the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// Described by the library; horsie has no call site for it yet.
    NotImplemented,
    /// horsie has no such concept, and no plan for one.
    NoConcept,
    /// Not a documented Claude Code event at all.
    Unknown,
}

/// A field a hook may set on its JSON reply.
///
/// Named rather than free-form because the illegal state this library exists to
/// remove was a field recorded on an event that never offered it. A hook may
/// still *emit* anything; what it may *affect* is this table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputField {
    /// `systemMessage` — warning text shown to the user, never to the model.
    SystemMessage,
    /// Top-level `decision: "block"` with `reason`.
    Decision,
    /// `hookSpecificOutput.permissionDecision` — `PreToolUse` only.
    PermissionDecision,
    /// `hookSpecificOutput.additionalContext` — injected into the model.
    AdditionalContext,
    /// `hookSpecificOutput.updatedInput` — `PreToolUse` only.
    UpdatedInput,
    /// `hookSpecificOutput.updatedToolOutput` — `PostToolUse` only.
    UpdatedToolOutput,
    /// Top-level `continue: false` with `stopReason`. A common field: any event
    /// with JSON output at all may set it, and it outranks `Decision`.
    Halt,
}

use OutputField::{
    AdditionalContext, Decision, Halt, PermissionDecision, SystemMessage, UpdatedInput,
    UpdatedToolOutput,
};

impl HookEvent {
    /// Classify a documented event name.
    ///
    /// `Err` carries why horsie cannot describe it — never `NotImplemented`,
    /// which is [`super::read`]'s verdict about horsie's call sites rather than
    /// this table's about the protocol.
    pub fn parse(name: &str) -> Result<HookEvent, Unsupported> {
        match name {
            "PreToolUse" => Ok(HookEvent::PreToolUse),
            "PostToolUse" => Ok(HookEvent::PostToolUse),
            "PostToolUseFailure" => Ok(HookEvent::PostToolUseFailure),
            "PostToolBatch" => Ok(HookEvent::PostToolBatch),
            "SessionStart" => Ok(HookEvent::SessionStart),
            "SessionEnd" => Ok(HookEvent::SessionEnd),
            "UserPromptSubmit" => Ok(HookEvent::UserPromptSubmit),
            "UserPromptExpansion" => Ok(HookEvent::UserPromptExpansion),
            "Stop" => Ok(HookEvent::Stop),
            "StopFailure" => Ok(HookEvent::StopFailure),
            "SubagentStart" => Ok(HookEvent::SubagentStart),
            "SubagentStop" => Ok(HookEvent::SubagentStop),
            "TaskCreated" => Ok(HookEvent::TaskCreated),
            "TaskCompleted" => Ok(HookEvent::TaskCompleted),
            "Notification" => Ok(HookEvent::Notification),
            "CwdChanged" => Ok(HookEvent::CwdChanged),

            "PreCompact" => Ok(HookEvent::PreCompact),
            "PostCompact" => Ok(HookEvent::PostCompact),

            // No horsie concept: no permission model (horsie runs unattended by
            // design), no worktrees, no file watcher, no agent teams, no MCP
            // elicitation, no display layer. Each would need a subsystem, not a
            // call site.
            "PermissionRequest" | "PermissionDenied" | "FileChanged" | "ConfigChange"
            | "DirectoryAdded" | "Setup" | "MessageDisplay" | "TeammateIdle" | "WorktreeCreate"
            | "WorktreeRemove" | "Elicitation" | "ElicitationResult" | "InstructionsLoaded" => {
                Err(Unsupported::NoConcept)
            }

            _ => Err(Unsupported::Unknown),
        }
    }

    /// The documented name, so a record can be attributed on the wire.
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
            HookEvent::PostToolUseFailure => "PostToolUseFailure",
            HookEvent::PostToolBatch => "PostToolBatch",
            HookEvent::SessionStart => "SessionStart",
            HookEvent::SessionEnd => "SessionEnd",
            HookEvent::UserPromptSubmit => "UserPromptSubmit",
            HookEvent::UserPromptExpansion => "UserPromptExpansion",
            HookEvent::Stop => "Stop",
            HookEvent::StopFailure => "StopFailure",
            HookEvent::SubagentStart => "SubagentStart",
            HookEvent::SubagentStop => "SubagentStop",
            HookEvent::TaskCreated => "TaskCreated",
            HookEvent::TaskCompleted => "TaskCompleted",
            HookEvent::Notification => "Notification",
            HookEvent::CwdChanged => "CwdChanged",
            HookEvent::PreCompact => "PreCompact",
            HookEvent::PostCompact => "PostCompact",
        }
    }

    /// Whether horsie has a call site that fires this event.
    ///
    /// Exhaustive on purpose: promoting an event is a one-line change here plus
    /// its call site, and adding a variant without deciding fails to compile.
    pub fn is_wired(self) -> bool {
        match self {
            // `UserPromptSubmit` and `SubagentStart` joined this list when the
            // agent gained a pre-run seam: both need the turn's facts *and* a
            // runtime, before the run snapshots its history. Firing them from
            // `provide()` — the only place a runtime existed before — would have
            // been too late, leaving the first prompt of every session unhooked.
            HookEvent::PreToolUse
            | HookEvent::PostToolUse
            | HookEvent::SessionStart
            | HookEvent::SubagentStart
            | HookEvent::UserPromptSubmit
            // Wired with slash commands: it fires where the expansion happens,
            // which is the pre-run seam, immediately before `UserPromptSubmit`
            // sees the result.
            | HookEvent::UserPromptExpansion
            | HookEvent::Stop
            // A subagent's turn end used to fire `Stop`, because the sink that
            // fires it was not gated on the agent's kind — the same conflation
            // the start seam had before `SubagentStart` split off.
            | HookEvent::SubagentStop
            // Both fire from the compaction itself, which is the only place
            // that knows a boundary is being taken.
            | HookEvent::PreCompact
            | HookEvent::PostCompact => true,
            HookEvent::PostToolUseFailure
            | HookEvent::PostToolBatch
            | HookEvent::SessionEnd
            | HookEvent::StopFailure
            | HookEvent::TaskCreated
            | HookEvent::TaskCompleted
            | HookEvent::Notification
            | HookEvent::CwdChanged => false,
        }
    }

    /// Which output fields this event may set.
    ///
    /// `PreToolUse` keeps top-level `Decision` alongside `PermissionDecision`:
    /// the docs deprecate the former but still honour it, and published plugins
    /// use it. Refusing it would break them for no gain.
    pub fn permitted(self) -> &'static [OutputField] {
        match self {
            HookEvent::PreToolUse => &[
                SystemMessage,
                Decision,
                PermissionDecision,
                UpdatedInput,
                Halt,
            ],
            HookEvent::PostToolUse => &[
                SystemMessage,
                Decision,
                AdditionalContext,
                UpdatedToolOutput,
                Halt,
            ],
            HookEvent::PostToolUseFailure
            | HookEvent::PostToolBatch
            | HookEvent::UserPromptSubmit
            | HookEvent::UserPromptExpansion
            | HookEvent::Stop
            | HookEvent::SubagentStop => &[SystemMessage, Decision, AdditionalContext, Halt],
            // No `decision`: neither can refuse anything, because by the time
            // they run there is nothing left to refuse. They can still halt —
            // `continue` is not a refusal of the thing that just happened, it
            // is a request to go no further.
            HookEvent::SessionStart | HookEvent::SubagentStart => {
                &[SystemMessage, AdditionalContext, Halt]
            }
            HookEvent::TaskCreated | HookEvent::TaskCompleted => &[SystemMessage, Halt],
            // `PreCompact` can refuse: it runs before any history is rewritten,
            // so there is still something to refuse. It injects no context —
            // the compaction is not a turn, and there is no prompt to add to.
            HookEvent::PreCompact => &[SystemMessage, Decision, Halt],
            // Side-effect only: the docs give these no JSON output at all, not
            // even `systemMessage`, and exit 2 has no special meaning for them.
            HookEvent::SessionEnd
            | HookEvent::StopFailure
            | HookEvent::Notification
            // `PostCompact` reports; by the time it runs the boundary exists
            // and nothing it says could change it.
            | HookEvent::PostCompact
            | HookEvent::CwdChanged => &[],
        }
    }

    /// Whether this event may set `field`.
    pub fn permits(self, field: OutputField) -> bool {
        self.permitted().contains(&field)
    }

    /// Whether non-JSON stdout is injected context rather than debug output.
    ///
    /// For every other event a hook's bare stdout is something it printed, and
    /// recording it as context is how `PreToolUse` came to carry a field it
    /// never had.
    pub fn injects_bare_stdout(self) -> bool {
        matches!(self, HookEvent::SessionStart | HookEvent::UserPromptSubmit)
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

/// The horsie tools a Claude Code tool name stands for — [`claude_aliases`]
/// read backwards.
///
/// Needed because an agent's `tools` frontmatter is an allowlist written in
/// Claude's vocabulary, and horsie's allowlist filters on horsie's. Derived from
/// the same table rather than written out a second time, so the two directions
/// cannot drift; a test asserts they are inverses.
///
/// Empty for a name horsie has no tool for (`TodoWrite`, `WebFetch`, …). That is
/// the honest answer: an allowlist naming only those grants nothing, because
/// there is nothing to grant.
pub fn horsie_tools_for(claude_tool: &str) -> Vec<&'static str> {
    HORSIE_TOOLS
        .iter()
        .filter(|t| claude_aliases(t).contains(&claude_tool))
        .copied()
        .collect()
}

/// Every horsie tool the alias table knows. The one place both directions of
/// the mapping are enumerated.
const HORSIE_TOOLS: &[&str] = &[
    "bash",
    "read_file",
    "write_file",
    "find_and_replace",
    "replace_lines",
    "list_files",
    "glob",
    "grep",
];

/// Whether a hook's `matcher` selects an occurrence, given that occurrence's
/// matchable names.
///
/// The regex semantics are unchanged — unanchored, absent or empty selects
/// everything, and a pattern that fails to compile selects nothing so a broken
/// matcher cannot silently widen into "all". What generalises is the *subject*:
/// a tool event passes the tool name and its Claude aliases, `SessionStart`
/// passes its `source`, `Notification` its type. An event with no matcher
/// domain passes nothing, so only an absent matcher selects it.
pub fn matcher_selects(matcher: Option<&str>, subjects: &[&str]) -> bool {
    let Some(pattern) = matcher.map(str::trim).filter(|m| !m.is_empty()) else {
        return true;
    };
    let Ok(re) = regex::Regex::new(pattern) else {
        return false;
    };
    subjects.iter().any(|s| re.is_match(s))
}

/// [`matcher_selects`] for a tool event, whose subjects are the horsie tool
/// name plus every Claude name it answers to.
pub fn matcher_applies(matcher: Option<&str>, horsie_tool: &str) -> bool {
    let mut subjects = vec![horsie_tool];
    subjects.extend_from_slice(claude_aliases(horsie_tool));
    matcher_selects(matcher, &subjects)
}

/// Every documented Claude Code event, as of the 2026-08-02 docs.
#[cfg(test)]
pub(super) const ALL_31: [&str; 31] = [
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    /// The library describes every event horsie has a seam for; `read()` is what
    /// refuses the unwired ones. Pinning both halves separately is the point:
    /// widening what the library knows must not silently widen what horsie
    /// claims to run.
    #[test]
    fn all_eighteen_seam_events_are_described_and_the_other_thirteen_are_not() {
        let mut described = 0;
        let mut no_concept = 0;
        for name in ALL_31 {
            match HookEvent::parse(name) {
                Ok(_) => described += 1,
                Err(Unsupported::NoConcept) => no_concept += 1,
                Err(Unsupported::NotImplemented) => {
                    panic!("{name}: parse describes or refuses; NotImplemented is read()'s verdict")
                }
                Err(Unsupported::Unknown) => panic!("{name} is documented but classified Unknown"),
            }
        }
        assert_eq!(described, 18, "described set changed");
        assert_eq!(no_concept, 13, "absent set changed");
    }

    /// Wiring an event is a deliberate act. This is the list this change moves.
    #[test]
    fn exactly_ten_events_are_wired() {
        let wired: Vec<&str> = ALL_31
            .iter()
            .filter_map(|n| HookEvent::parse(n).ok())
            .filter(|e| e.is_wired())
            .map(HookEvent::name)
            .collect();
        assert_eq!(
            wired,
            vec![
                "SessionStart",
                "UserPromptSubmit",
                "UserPromptExpansion",
                "PreToolUse",
                "PostToolUse",
                "SubagentStart",
                "SubagentStop",
                "Stop",
                "PreCompact",
                "PostCompact",
            ],
            "wired set changed"
        );
    }

    /// The field table is the whole point of the library: a hook setting a
    /// field its event does not offer must be visibly ignored, never silently
    /// obeyed.
    #[test]
    fn only_pre_tool_use_may_rewrite_an_input() {
        assert!(HookEvent::PreToolUse.permits(OutputField::UpdatedInput));
        for e in [
            HookEvent::PostToolUse,
            HookEvent::SessionStart,
            HookEvent::Stop,
        ] {
            assert!(!e.permits(OutputField::UpdatedInput), "{}", e.name());
        }
    }

    #[test]
    fn pre_tool_use_offers_no_additional_context() {
        // The bug this replaces: records carried it, nothing consumed it, the
        // spec does not offer it, and there is no result yet to attach it to.
        assert!(!HookEvent::PreToolUse.permits(OutputField::AdditionalContext));
        assert!(HookEvent::PostToolUse.permits(OutputField::AdditionalContext));
    }

    #[test]
    fn side_effect_events_permit_no_output_at_all() {
        for e in [
            HookEvent::SessionEnd,
            HookEvent::StopFailure,
            HookEvent::Notification,
            HookEvent::CwdChanged,
        ] {
            assert!(e.permitted().is_empty(), "{} must permit nothing", e.name());
        }
    }

    #[test]
    fn session_start_cannot_block() {
        assert!(!HookEvent::SessionStart.permits(OutputField::Decision));
        assert!(!HookEvent::SessionStart.permits(OutputField::PermissionDecision));
    }

    /// Only these two read bare stdout as injected context. For every other
    /// event non-JSON stdout is debug output and is discarded.
    #[test]
    fn bare_stdout_is_context_for_exactly_two_events() {
        let injecting: Vec<&str> = ALL_31
            .iter()
            .filter_map(|n| HookEvent::parse(n).ok())
            .filter(|e| e.injects_bare_stdout())
            .map(HookEvent::name)
            .collect();
        assert_eq!(injecting, vec!["SessionStart", "UserPromptSubmit"]);
    }

    #[test]
    fn round_trips_through_name() {
        for name in ALL_31 {
            if let Ok(e) = HookEvent::parse(name) {
                assert_eq!(e.name(), name);
            }
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

    /// A matcher's subject is per-event, not always a tool name.
    #[test]
    fn a_matcher_selects_on_the_events_own_subject() {
        assert!(matcher_selects(Some("startup|resume"), &["startup"]));
        assert!(!matcher_selects(Some("startup|resume"), &["compact"]));
        // An event with no matcher domain: only an absent matcher selects it.
        assert!(matcher_selects(None, &[]));
        assert!(!matcher_selects(Some("anything"), &[]));
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

    /// The two directions are one table. Written out twice they would drift,
    /// and the drift would be silent: an agent's `tools` allowlist would grant
    /// a tool its hooks could not match, or the reverse.
    #[test]
    fn the_alias_table_reads_the_same_in_both_directions() {
        for horsie in HORSIE_TOOLS {
            for claude in claude_aliases(horsie) {
                assert!(
                    horsie_tools_for(claude).contains(horsie),
                    "{horsie} aliases to {claude}, which must map back"
                );
            }
        }
        // Claude's two in-place editors both reach horsie's two.
        assert_eq!(
            horsie_tools_for("Edit"),
            ["find_and_replace", "replace_lines"]
        );
        // A Claude tool horsie has no equivalent for grants nothing.
        assert!(horsie_tools_for("TodoWrite").is_empty());
    }
}

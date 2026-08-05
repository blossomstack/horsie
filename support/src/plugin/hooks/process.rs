//! Turning one hook process's reply into one typed outcome.
//!
//! Generic by construction: the event decides what a reply *may* say (via
//! [`HookEvent::permitted`]) and this decides what it *did* say. What that then
//! *means* belongs to the call site — `PreToolUse` fails closed, `Stop`
//! blocking continues a turn, `Notification` cannot block at all.

use super::events::{HookEvent, OutputField};
use serde_json::Value;

/// What a hook process produced.
///
/// `code` is `None` when it could not be run to completion — spawn failure or
/// timeout — which is an outage rather than anything the hook decided.
#[derive(Debug, Clone)]
pub struct HookReply {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// The hook's top-level answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Verdict {
    /// The hook ran and did not refuse.
    #[default]
    Proceed,
    /// The hook refused, via exit 2 or `decision: "block"`. What refusing means
    /// is per-event and decided elsewhere: for `PreToolUse` it denies a call,
    /// for `Stop` it continues a turn.
    Block { reason: Option<String> },
    /// The hook could not be run to completion, or exited non-zero in a way
    /// that is not a refusal. An outage, never a decision.
    Failed { reason: String },
}

/// `PreToolUse`'s permission vocabulary, carried separately from the verdict
/// because `ask` and `defer` are neither a refusal nor an outage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Permission {
    Deny { reason: Option<String> },
    Ask,
    Defer,
}

/// Everything one hook said, filtered to what its event may say.
#[derive(Debug, Clone, Default)]
pub struct HookOutput {
    pub verdict: Verdict,
    pub permission: Option<Permission>,
    pub system_message: Option<String>,
    pub additional_context: Option<String>,
    pub updated_input: Option<Value>,
    pub updated_tool_output: Option<String>,
    /// Fields the hook set that its event does not offer. Named rather than
    /// merely dropped, so a plugin author can be told why nothing happened.
    pub ignored: Vec<&'static str>,
}

/// The first non-empty line of `stderr`, or `fallback`. A failing hook often
/// dumps a stack trace; a reason is a sentence, not a log.
fn first_line(stderr: &str, fallback: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map_or_else(|| fallback.to_string(), str::to_string)
}

/// Interpret one hook's reply against its event's contract.
pub fn process(event: HookEvent, reply: &HookReply) -> HookOutput {
    let mut out = HookOutput::default();

    match reply.code {
        Some(0) => {}
        // A blocking error — but only for an event that can block. The same
        // exit code, a different outcome, decided by the table rather than by
        // whoever wrote the call site.
        Some(2) if event.permits(OutputField::Decision) => {
            let reason = reply.stderr.trim();
            out.verdict = Verdict::Block {
                reason: (!reason.is_empty()).then(|| reason.to_string()),
            };
            return out;
        }
        Some(code) => {
            out.verdict = Verdict::Failed {
                reason: first_line(&reply.stderr, &format!("the hook exited {code}")),
            };
            return out;
        }
        None => {
            out.verdict = Verdict::Failed {
                reason: first_line(&reply.stderr, "the hook could not be run"),
            };
            return out;
        }
    }

    let Ok(json) = serde_json::from_str::<Value>(&reply.stdout) else {
        // Not JSON. For the two events that inject context this *is* the
        // output; for every other it is debug noise the hook happened to print.
        if event.injects_bare_stdout() {
            let text = reply.stdout.trim();
            if !text.is_empty() {
                out.additional_context = Some(text.to_string());
            }
        }
        return out;
    };

    let str_at = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
    // Reads `field` only when the event offers it; otherwise names it as
    // ignored and yields nothing. `out` is a parameter rather than a capture so
    // the closure and the `&mut out` at each call site do not overlap.
    let take = |present: bool, field: OutputField, name: &'static str, out: &mut HookOutput| {
        if !present {
            return false;
        }
        if event.permits(field) {
            return true;
        }
        out.ignored.push(name);
        false
    };

    if take(
        json.get("systemMessage").is_some(),
        OutputField::SystemMessage,
        "systemMessage",
        &mut out,
    ) {
        out.system_message = str_at(json.get("systemMessage"));
    }

    if take(
        json.get("decision").and_then(Value::as_str) == Some("block"),
        OutputField::Decision,
        "decision",
        &mut out,
    ) {
        out.verdict = Verdict::Block {
            reason: str_at(json.get("reason")),
        };
    }

    let Some(hso) = json.get("hookSpecificOutput") else {
        return out;
    };

    if take(
        hso.get("additionalContext").is_some(),
        OutputField::AdditionalContext,
        "additionalContext",
        &mut out,
    ) {
        out.additional_context = str_at(hso.get("additionalContext"));
    }

    if take(
        hso.get("updatedInput").is_some(),
        OutputField::UpdatedInput,
        "updatedInput",
        &mut out,
    ) {
        out.updated_input = hso.get("updatedInput").cloned();
    }

    if take(
        hso.get("updatedToolOutput").is_some(),
        OutputField::UpdatedToolOutput,
        "updatedToolOutput",
        &mut out,
    ) {
        out.updated_tool_output = hso
            .get("updatedToolOutput")
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string));
    }

    if take(
        hso.get("permissionDecision").is_some(),
        OutputField::PermissionDecision,
        "permissionDecision",
        &mut out,
    ) {
        out.permission = match hso.get("permissionDecision").and_then(Value::as_str) {
            Some("deny") => Some(Permission::Deny {
                reason: str_at(hso.get("permissionDecisionReason")),
            }),
            Some("ask") => Some(Permission::Ask),
            Some("defer") => Some(Permission::Defer),
            _ => None,
        };
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

    fn ok(stdout: &str) -> HookReply {
        HookReply {
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
        }
    }

    #[test]
    fn exit_zero_with_no_output_just_proceeds() {
        let out = process(HookEvent::PostToolUse, &ok(""));
        assert_eq!(out.verdict, Verdict::Proceed);
        assert!(out.system_message.is_none());
        assert!(out.ignored.is_empty());
    }

    /// Exit 2 is a blocking error: stderr is the reason and stdout is ignored
    /// entirely, JSON or not. A hook cannot both refuse and rewrite.
    #[test]
    fn exit_two_blocks_with_stderr_and_discards_stdout() {
        let reply = HookReply {
            code: Some(2),
            stdout: r#"{"hookSpecificOutput":{"additionalContext":"ignored"}}"#.to_string(),
            stderr: "  writes are not allowed\n".to_string(),
        };
        let out = process(HookEvent::PostToolUse, &reply);
        match out.verdict {
            Verdict::Block { reason } => {
                assert_eq!(reason.as_deref(), Some("writes are not allowed"));
            }
            other => panic!("expected a block, got {other:?}"),
        }
        assert!(out.additional_context.is_none());
    }

    /// An event that cannot block treats exit 2 as a plain failure — the same
    /// process reply, a different outcome, decided by the table.
    #[test]
    fn exit_two_is_a_failure_for_an_event_that_cannot_block() {
        let reply = HookReply {
            code: Some(2),
            stdout: String::new(),
            stderr: "boom".into(),
        };
        let out = process(HookEvent::SessionStart, &reply);
        match out.verdict {
            Verdict::Failed { reason } => assert!(reason.contains("boom"), "{reason}"),
            other => panic!("SessionStart cannot block, got {other:?}"),
        }
    }

    #[test]
    fn any_other_exit_is_a_failure_naming_the_first_line_of_stderr() {
        let reply = HookReply {
            code: Some(1),
            stdout: String::new(),
            stderr: "cannot find node\nstack trace line\nanother".into(),
        };
        match process(HookEvent::PreToolUse, &reply).verdict {
            Verdict::Failed { reason } => {
                assert!(reason.contains("cannot find node"), "{reason}");
                assert!(
                    !reason.contains("stack trace"),
                    "one line, not a dump: {reason}"
                );
            }
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// `None` is spawn failure or timeout — an outage, never a decision.
    #[test]
    fn a_hook_that_never_ran_is_a_failure() {
        let reply = HookReply {
            code: None,
            stdout: String::new(),
            stderr: "timed out".into(),
        };
        assert!(matches!(
            process(HookEvent::PreToolUse, &reply).verdict,
            Verdict::Failed { .. }
        ));
    }

    #[test]
    fn bare_stdout_is_context_for_session_start() {
        let out = process(HookEvent::SessionStart, &ok("  project conventions  "));
        assert_eq!(
            out.additional_context.as_deref(),
            Some("project conventions")
        );
    }

    /// For every other event non-JSON stdout is debug output. Recording it as
    /// injected context is how `PreToolUse` came to carry a field it never had.
    #[test]
    fn bare_stdout_is_discarded_for_every_other_event() {
        for e in [
            HookEvent::PreToolUse,
            HookEvent::PostToolUse,
            HookEvent::Stop,
        ] {
            let out = process(e, &ok("debug noise"));
            assert!(out.additional_context.is_none(), "{}", e.name());
            assert_eq!(out.verdict, Verdict::Proceed);
        }
    }

    #[test]
    fn a_permitted_field_is_read() {
        let out = process(
            HookEvent::PostToolUse,
            &ok(
                r#"{"systemMessage":"heads up","hookSpecificOutput":{"additionalContext":"note","updatedToolOutput":"clean"}}"#,
            ),
        );
        assert_eq!(out.system_message.as_deref(), Some("heads up"));
        assert_eq!(out.additional_context.as_deref(), Some("note"));
        assert_eq!(out.updated_tool_output.as_deref(), Some("clean"));
        assert!(out.ignored.is_empty());
    }

    /// The library's reason for existing: a field the event does not offer is
    /// dropped *and named*, so the ignoring is visible rather than silent.
    #[test]
    fn a_field_the_event_does_not_permit_is_ignored_and_named() {
        let out = process(
            HookEvent::PreToolUse,
            &ok(
                r#"{"hookSpecificOutput":{"additionalContext":"nope","updatedInput":{"command":"ls"}}}"#,
            ),
        );
        assert!(
            out.additional_context.is_none(),
            "PreToolUse offers no context"
        );
        assert!(
            out.updated_input.is_some(),
            "but it does offer updatedInput"
        );
        assert_eq!(out.ignored, vec!["additionalContext"]);
    }

    #[test]
    fn side_effect_events_ignore_every_field_including_system_message() {
        let out = process(
            HookEvent::Notification,
            &ok(r#"{"systemMessage":"hi","decision":"block","reason":"no"}"#),
        );
        assert!(out.system_message.is_none());
        assert_eq!(out.verdict, Verdict::Proceed);
        assert_eq!(out.ignored, vec!["systemMessage", "decision"]);
    }

    #[test]
    fn decision_block_is_a_block_with_its_reason() {
        let out = process(
            HookEvent::Stop,
            &ok(r#"{"decision":"block","reason":"tests still failing"}"#),
        );
        match out.verdict {
            Verdict::Block { reason } => {
                assert_eq!(reason.as_deref(), Some("tests still failing"));
            }
            other => panic!("expected a block, got {other:?}"),
        }
    }

    #[test]
    fn permission_decisions_are_carried_separately_from_the_verdict() {
        let deny = process(
            HookEvent::PreToolUse,
            &ok(
                r#"{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"root"}}"#,
            ),
        );
        match deny.permission {
            Some(Permission::Deny { reason }) => assert_eq!(reason.as_deref(), Some("root")),
            other => panic!("expected a deny, got {other:?}"),
        }
        // `ask`/`defer` are the hook's word; what horsie does about them is the
        // call site's decision, not this parser's.
        let ask = process(
            HookEvent::PreToolUse,
            &ok(r#"{"hookSpecificOutput":{"permissionDecision":"ask"}}"#),
        );
        assert!(matches!(ask.permission, Some(Permission::Ask)));
        assert_eq!(ask.verdict, Verdict::Proceed);
    }

    /// Malformed JSON is not a hook failure: the process succeeded. It is
    /// stdout that happens not to parse, which for most events is noise.
    #[test]
    fn unparseable_json_on_exit_zero_is_not_a_failure() {
        let out = process(HookEvent::PostToolUse, &ok("{not json"));
        assert_eq!(out.verdict, Verdict::Proceed);
    }
}

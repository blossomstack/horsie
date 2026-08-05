//! What horsie is about to fire, with the facts that event needs.
//!
//! One arm per wired event, so a call site cannot fire `Stop` with a tool
//! payload or build a `SessionStartRecord` out of a tool call — the two shapes
//! never meet. Everything downstream is derived from the arm: the stdin
//! payload, the matcher's subject, and the record.
//!
//! Only wired events appear. Promoting one of the ten described-but-unwired
//! events means adding its arm here alongside its call site.

use super::events::{HookEvent, claude_aliases};
use super::process::{HookOutput, Permission, Verdict};
use horsie_models::hooks as rec;
use serde_json::{Value, json};

/// Cap on any before/after payload recorded for the UI. A hook that rewrites a
/// large file write must not bloat the journal.
const RECORD_CLAMP: usize = 8_000;

fn clamp(s: &str) -> String {
    s.chars().take(RECORD_CLAMP).collect()
}

#[derive(Debug, Clone, Copy)]
pub enum HookInvocation<'a> {
    PreToolUse {
        tool: &'a str,
        tool_call_id: &'a str,
        input: &'a Value,
    },
    PostToolUse {
        tool: &'a str,
        tool_call_id: &'a str,
        input: &'a Value,
        response: &'a str,
        is_error: bool,
    },
    SessionStart {
        source: &'a str,
    },
    /// A subagent's start. Not a `SessionStart` with a different subject: a
    /// subagent is not a session, and the two events carry different matcher
    /// domains — `source` for one, the agent's type for the other.
    SubagentStart {
        /// Which subagent. Every one reports the same `agent_type` until #105's
        /// Phase 2, so this is what tells two concurrent subagents apart.
        agent_id: &'a str,
        agent_type: &'a str,
    },
    UserPromptSubmit {
        prompt: &'a str,
    },
    /// A slash command about to be expanded into its template.
    UserPromptExpansion {
        prompt: &'a str,
        command: &'a str,
    },
    Stop {
        last_assistant_message: Option<&'a str>,
        /// True when horsie is only still running because a previous `Stop`
        /// hook blocked. A cooperative hook returns early rather than looping.
        stop_hook_active: bool,
    },
    /// A subagent's turn ending. Not a `Stop`: a subagent is not a session, and
    /// its matcher selects on the agent's type rather than on nothing at all.
    SubagentStop {
        agent_id: &'a str,
        agent_type: &'a str,
        last_assistant_message: Option<&'a str>,
        stop_hook_active: bool,
    },
}

impl HookInvocation<'_> {
    pub fn event(&self) -> HookEvent {
        match self {
            HookInvocation::PreToolUse { .. } => HookEvent::PreToolUse,
            HookInvocation::PostToolUse { .. } => HookEvent::PostToolUse,
            HookInvocation::SessionStart { .. } => HookEvent::SessionStart,
            HookInvocation::SubagentStart { .. } => HookEvent::SubagentStart,
            HookInvocation::UserPromptSubmit { .. } => HookEvent::UserPromptSubmit,
            HookInvocation::UserPromptExpansion { .. } => HookEvent::UserPromptExpansion,
            HookInvocation::Stop { .. } => HookEvent::Stop,
            HookInvocation::SubagentStop { .. } => HookEvent::SubagentStop,
        }
    }

    /// The names this occurrence's `matcher` is tested against.
    ///
    /// A tool event offers the horsie tool name and every Claude name it
    /// answers to; `SessionStart` offers its `source`. An event with no matcher
    /// domain offers nothing, so only an absent matcher selects it.
    pub fn matcher_subjects(&self) -> Vec<&str> {
        match self {
            HookInvocation::PreToolUse { tool, .. } | HookInvocation::PostToolUse { tool, .. } => {
                let mut v = vec![*tool];
                v.extend_from_slice(claude_aliases(tool));
                v
            }
            HookInvocation::SessionStart { source } => vec![*source],
            // `agent_type` alone: the id names one subagent, and a matcher
            // selecting a single run is not a thing the spec offers.
            HookInvocation::SubagentStart { agent_type, .. }
            | HookInvocation::SubagentStop { agent_type, .. } => vec![*agent_type],
            HookInvocation::UserPromptExpansion { command, .. } => vec![*command],
            HookInvocation::UserPromptSubmit { .. } | HookInvocation::Stop { .. } => Vec::new(),
        }
    }

    /// The JSON written to the hook's stdin.
    ///
    /// `session_id`, `transcript_path`, `cwd` and `permission_mode` are
    /// deliberately absent: horsie has no transcript file to name, no
    /// permission model, and the runtime's cwd is per-agent state a hook has no
    /// business acting on. Sending a placeholder would be worse than sending
    /// nothing — a hook that branched on it would branch on a lie.
    pub fn payload(&self) -> String {
        let v = match self {
            HookInvocation::PreToolUse {
                tool,
                tool_call_id,
                input,
            } => json!({
                "hook_event_name": "PreToolUse",
                "tool_name": tool,
                "tool_use_id": tool_call_id,
                "tool_input": input,
            }),
            HookInvocation::PostToolUse {
                tool,
                tool_call_id,
                input,
                response,
                is_error,
            } => json!({
                "hook_event_name": "PostToolUse",
                "tool_name": tool,
                "tool_use_id": tool_call_id,
                "tool_input": input,
                "tool_response": response,
                "is_error": is_error,
            }),
            HookInvocation::SessionStart { source } => json!({
                "hook_event_name": "SessionStart",
                "source": source,
            }),
            HookInvocation::SubagentStart {
                agent_id,
                agent_type,
            } => json!({
                "hook_event_name": "SubagentStart",
                "agent_id": agent_id,
                "agent_type": agent_type,
            }),
            HookInvocation::UserPromptSubmit { prompt } => json!({
                "hook_event_name": "UserPromptSubmit",
                "prompt": prompt,
            }),
            HookInvocation::UserPromptExpansion { prompt, command } => json!({
                "hook_event_name": "UserPromptExpansion",
                "prompt": prompt,
                "command": command,
            }),
            HookInvocation::Stop {
                last_assistant_message,
                stop_hook_active,
            } => json!({
                "hook_event_name": "Stop",
                "last_assistant_message": last_assistant_message,
                "stop_hook_active": stop_hook_active,
            }),
            HookInvocation::SubagentStop {
                agent_id,
                agent_type,
                last_assistant_message,
                stop_hook_active,
            } => json!({
                "hook_event_name": "SubagentStop",
                "agent_id": agent_id,
                "agent_type": agent_type,
                "last_assistant_message": last_assistant_message,
                "stop_hook_active": stop_hook_active,
            }),
        };
        v.to_string()
    }

    /// Fold one hook's output into this event's record.
    ///
    /// The one place a [`HookOutput`] becomes a `HookRecord`, so every event's
    /// mapping is checked against its own outcome union. A `Block` on an event
    /// whose union has no `Blocked` arm is not a judgement call here — the type
    /// leaves nowhere to put it.
    pub fn record(&self, plugin: &str, duration_ms: u64, out: &HookOutput) -> rec::HookRecord {
        rec::HookRecord {
            plugin: plugin.to_string(),
            duration_ms,
            // Straight through, on every event: `continue` is a common field,
            // so unlike the outcome it needs no per-event mapping at all.
            halt: out.halt.as_ref().map(|h| rec::HookHalt {
                reason: h.reason.clone(),
            }),
            action: self.action(out),
        }
    }

    fn action(&self, out: &HookOutput) -> rec::HookAction {
        let sys = out.system_message.clone();
        let ctx = || rec::ContextInjected {
            additional_context: out.additional_context.as_deref().map(clamp),
        };
        let failed = |reason: &str| rec::HookFailed {
            reason: reason.to_string(),
        };

        match self {
            HookInvocation::PreToolUse {
                tool,
                tool_call_id,
                input,
            } => {
                let call = rec::ToolScope {
                    tool: (*tool).to_string(),
                    tool_call_id: (*tool_call_id).to_string(),
                };
                let outcome = match (&out.verdict, &out.permission) {
                    (Verdict::Failed { reason }, _) => {
                        rec::PreToolUseOutcome::Failed(failed(reason))
                    }
                    (Verdict::Block { reason }, _) => {
                        rec::PreToolUseOutcome::Denied(rec::HookDenied {
                            reason: reason.clone(),
                        })
                    }
                    (Verdict::Proceed, Some(Permission::Deny { reason })) => {
                        rec::PreToolUseOutcome::Denied(rec::HookDenied {
                            reason: reason.clone(),
                        })
                    }
                    (Verdict::Proceed, Some(Permission::Ask)) => rec::PreToolUseOutcome::Ask,
                    (Verdict::Proceed, Some(Permission::Defer)) => rec::PreToolUseOutcome::Defer,
                    (Verdict::Proceed, None) => {
                        rec::PreToolUseOutcome::Allowed(rec::PreToolUseAllowed {
                            input: out.updated_input.as_ref().map(|after| rec::HookRewrite {
                                before: clamp(&input.to_string()),
                                after: clamp(&after.to_string()),
                            }),
                        })
                    }
                };
                rec::HookAction::PreToolUse(rec::PreToolUseRecord {
                    call,
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::PostToolUse {
                tool,
                tool_call_id,
                response,
                ..
            } => {
                let call = rec::ToolScope {
                    tool: (*tool).to_string(),
                    tool_call_id: (*tool_call_id).to_string(),
                };
                let outcome = match &out.verdict {
                    Verdict::Failed { reason } => rec::PostToolUseOutcome::Failed(failed(reason)),
                    Verdict::Block { reason } => {
                        rec::PostToolUseOutcome::Blocked(rec::HookBlocked {
                            reason: reason.clone(),
                        })
                    }
                    Verdict::Proceed => rec::PostToolUseOutcome::Ran(rec::PostToolUseRan {
                        output: out
                            .updated_tool_output
                            .as_deref()
                            .map(|after| rec::HookRewrite {
                                before: clamp(response),
                                after: clamp(after),
                            }),
                        additional_context: out.additional_context.as_deref().map(clamp),
                    }),
                };
                rec::HookAction::PostToolUse(rec::PostToolUseRecord {
                    call,
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::SessionStart { source } => {
                // Cannot block, so a block-shaped reply can only be a failure.
                // The union leaves nowhere else for it to go.
                let outcome = match &out.verdict {
                    Verdict::Proceed => rec::SessionStartOutcome::Ran(ctx()),
                    Verdict::Block { reason } => rec::SessionStartOutcome::Failed(failed(
                        reason
                            .as_deref()
                            .unwrap_or("the hook tried to block, which SessionStart cannot do"),
                    )),
                    Verdict::Failed { reason } => rec::SessionStartOutcome::Failed(failed(reason)),
                };
                rec::HookAction::SessionStart(rec::SessionStartRecord {
                    source: (*source).to_string(),
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::SubagentStart {
                agent_id,
                agent_type,
            } => {
                // Cannot block, exactly like `SessionStart`: by the time it runs
                // the subagent exists, so there is nothing left to refuse.
                let outcome = match &out.verdict {
                    Verdict::Proceed => rec::SubagentStartOutcome::Ran(ctx()),
                    Verdict::Block { reason } => rec::SubagentStartOutcome::Failed(failed(
                        reason
                            .as_deref()
                            .unwrap_or("the hook tried to block, which SubagentStart cannot do"),
                    )),
                    Verdict::Failed { reason } => rec::SubagentStartOutcome::Failed(failed(reason)),
                };
                rec::HookAction::SubagentStart(rec::SubagentStartRecord {
                    agent_id: (*agent_id).to_string(),
                    agent_type: (*agent_type).to_string(),
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::UserPromptSubmit { .. } => {
                let outcome = match &out.verdict {
                    Verdict::Proceed => rec::UserPromptSubmitOutcome::Ran(ctx()),
                    Verdict::Block { reason } => {
                        rec::UserPromptSubmitOutcome::Blocked(rec::HookBlocked {
                            reason: reason.clone(),
                        })
                    }
                    Verdict::Failed { reason } => {
                        rec::UserPromptSubmitOutcome::Failed(failed(reason))
                    }
                };
                rec::HookAction::UserPromptSubmit(rec::UserPromptSubmitRecord {
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::UserPromptExpansion { command, .. } => {
                let outcome = match &out.verdict {
                    Verdict::Proceed => rec::UserPromptExpansionOutcome::Ran(ctx()),
                    Verdict::Block { reason } => {
                        rec::UserPromptExpansionOutcome::Blocked(rec::HookBlocked {
                            reason: reason.clone(),
                        })
                    }
                    Verdict::Failed { reason } => {
                        rec::UserPromptExpansionOutcome::Failed(failed(reason))
                    }
                };
                rec::HookAction::UserPromptExpansion(rec::UserPromptExpansionRecord {
                    command: (*command).to_string(),
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::Stop { .. } => {
                // `CapReached` is never produced here: only the call site knows
                // it has run out of continuations, so it narrows the outcome.
                let outcome = match &out.verdict {
                    Verdict::Proceed => rec::StopOutcome::Ran(ctx()),
                    Verdict::Block { reason } => rec::StopOutcome::Blocked(rec::HookBlocked {
                        reason: reason.clone(),
                    }),
                    Verdict::Failed { reason } => rec::StopOutcome::Failed(failed(reason)),
                };
                rec::HookAction::Stop(rec::StopRecord {
                    system_message: sys,
                    outcome,
                })
            }
            HookInvocation::SubagentStop {
                agent_id,
                agent_type,
                ..
            } => {
                // Same three arms as `Stop`, and `CapReached` narrowed at the
                // same call site: a subagent is held in its loop by the same
                // budget the main agent is.
                let outcome = match &out.verdict {
                    Verdict::Proceed => rec::SubagentStopOutcome::Ran(ctx()),
                    Verdict::Block { reason } => {
                        rec::SubagentStopOutcome::Blocked(rec::HookBlocked {
                            reason: reason.clone(),
                        })
                    }
                    Verdict::Failed { reason } => rec::SubagentStopOutcome::Failed(failed(reason)),
                };
                rec::HookAction::SubagentStop(rec::SubagentStopRecord {
                    agent_id: (*agent_id).to_string(),
                    agent_type: (*agent_type).to_string(),
                    system_message: sys,
                    outcome,
                })
            }
        }
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
    use horsie_models::hooks::{HookAction, PreToolUseOutcome, SessionStartOutcome, StopOutcome};
    use serde_json::json;

    fn allowed() -> HookOutput {
        HookOutput::default()
    }

    #[test]
    fn an_invocation_knows_its_own_event() {
        let input = json!({"command": "ls"});
        let i = HookInvocation::PreToolUse {
            tool: "bash",
            tool_call_id: "tc1",
            input: &input,
        };
        assert_eq!(i.event(), HookEvent::PreToolUse);
        assert_eq!(
            HookInvocation::SessionStart { source: "startup" }.event(),
            HookEvent::SessionStart
        );
    }

    /// The payload used to be built by an inline `json!` at each call site, so
    /// its shape lived wherever someone typed it.
    #[test]
    fn the_payload_carries_the_documented_fields_for_its_event() {
        let input = json!({"command": "ls"});
        let i = HookInvocation::PreToolUse {
            tool: "bash",
            tool_call_id: "tc1",
            input: &input,
        };
        let p: Value = serde_json::from_str(&i.payload()).unwrap();
        assert_eq!(p["hook_event_name"], "PreToolUse");
        assert_eq!(p["tool_name"], "bash");
        assert_eq!(p["tool_use_id"], "tc1");
        assert_eq!(p["tool_input"]["command"], "ls");

        let s = HookInvocation::SessionStart { source: "resume" };
        let p: Value = serde_json::from_str(&s.payload()).unwrap();
        assert_eq!(p["hook_event_name"], "SessionStart");
        assert_eq!(p["source"], "resume");
    }

    /// The loop guard the design makes mandatory: a cooperative hook returns
    /// early when it sees this, which is the only reason to send it.
    #[test]
    fn stop_carries_stop_hook_active() {
        let i = HookInvocation::Stop {
            last_assistant_message: Some("done"),
            stop_hook_active: true,
        };
        let p: Value = serde_json::from_str(&i.payload()).unwrap();
        assert_eq!(p["stop_hook_active"], true);
        assert_eq!(p["last_assistant_message"], "done");
    }

    /// A subagent's stop is its own event, naming *which* subagent and of what
    /// type. Fired as `Stop` — which is what happened before this arm existed —
    /// it would carry neither. Only the type is a matcher subject: an id
    /// selects one run, which is not a thing the spec offers.
    #[test]
    fn subagent_stop_names_the_agent_on_the_payload_and_its_type_on_the_matcher() {
        let i = HookInvocation::SubagentStop {
            agent_id: "sub-1",
            agent_type: "reviewer",
            last_assistant_message: Some("looks fine"),
            stop_hook_active: false,
        };
        let p: Value = serde_json::from_str(&i.payload()).unwrap();
        assert_eq!(p["hook_event_name"], "SubagentStop");
        assert_eq!(p["agent_id"], "sub-1");
        assert_eq!(p["agent_type"], "reviewer");
        assert_eq!(p["last_assistant_message"], "looks fine");
        assert_eq!(i.matcher_subjects(), vec!["reviewer"]);
    }

    /// A blocking `SubagentStop` records a block, exactly as `Stop` does: it is
    /// blocked *from stopping*, and the call site continues the subagent.
    #[test]
    fn a_blocking_subagent_stop_records_a_block() {
        let out = HookOutput {
            verdict: Verdict::Block {
                reason: Some("no tests were run".into()),
            },
            ..Default::default()
        };
        let i = HookInvocation::SubagentStop {
            agent_id: "sub-1",
            agent_type: "reviewer",
            last_assistant_message: None,
            stop_hook_active: false,
        };
        match i.record("p", 1, &out).action {
            HookAction::SubagentStop(r) => {
                assert_eq!(r.agent_id, "sub-1");
                assert_eq!(r.agent_type, "reviewer");
                match r.outcome {
                    horsie_models::hooks::SubagentStopOutcome::Blocked(b) => {
                        assert_eq!(b.reason.as_deref(), Some("no tests were run"));
                    }
                    other => panic!("{other:?}"),
                }
            }
            other => panic!("{other:?}"),
        }
    }

    /// The halt rides the envelope, on every event, unmapped. A record that
    /// both allowed its call and halted the turn is the case this shape exists
    /// for.
    #[test]
    fn a_halt_is_recorded_beside_the_outcome_not_inside_it() {
        let out = HookOutput {
            halt: Some(crate::plugin::hooks::Halt {
                reason: Some("budget spent".into()),
            }),
            ..Default::default()
        };
        let input = json!({});
        let rec = HookInvocation::PreToolUse {
            tool: "bash",
            tool_call_id: "t",
            input: &input,
        }
        .record("p", 1, &out);
        assert_eq!(
            rec.halt.and_then(|h| h.reason).as_deref(),
            Some("budget spent")
        );
        assert!(matches!(
            rec.action,
            HookAction::PreToolUse(r) if matches!(r.outcome, PreToolUseOutcome::Allowed(_))
        ));
    }

    #[test]
    fn a_tool_invocations_matcher_subjects_include_the_claude_aliases() {
        let input = json!({});
        let i = HookInvocation::PreToolUse {
            tool: "write_file",
            tool_call_id: "t",
            input: &input,
        };
        assert_eq!(i.matcher_subjects(), vec!["write_file", "Write"]);
        assert_eq!(
            HookInvocation::SessionStart { source: "fork" }.matcher_subjects(),
            vec!["fork"]
        );
    }

    #[test]
    fn a_clean_pre_tool_use_records_as_allowed_with_its_scope() {
        let input = json!({});
        let i = HookInvocation::PreToolUse {
            tool: "bash",
            tool_call_id: "tc1",
            input: &input,
        };
        let record = i.record("guard", 4, &allowed());
        assert_eq!(record.plugin, "guard");
        assert_eq!(record.duration_ms, 4);
        match record.action {
            HookAction::PreToolUse(r) => {
                assert_eq!(r.call.tool, "bash");
                assert_eq!(r.call.tool_call_id, "tc1");
                assert!(matches!(r.outcome, PreToolUseOutcome::Allowed(_)));
            }
            other => panic!("expected a PreToolUse action, got {other:?}"),
        }
    }

    /// The contradiction this replaces: a hook that *failed* was recorded as
    /// `blocked`, against that field's own doc comment. Now they are different
    /// arms and cannot be confused.
    #[test]
    fn a_failure_and_a_denial_are_different_outcomes() {
        let input = json!({});
        let i = HookInvocation::PreToolUse {
            tool: "bash",
            tool_call_id: "tc1",
            input: &input,
        };

        let failed = HookOutput {
            verdict: Verdict::Failed {
                reason: "spawn".into(),
            },
            ..Default::default()
        };
        match i.record("g", 0, &failed).action {
            HookAction::PreToolUse(r) => assert!(matches!(r.outcome, PreToolUseOutcome::Failed(_))),
            other => panic!("{other:?}"),
        }

        let denied = HookOutput {
            permission: Some(Permission::Deny {
                reason: Some("root".into()),
            }),
            ..Default::default()
        };
        match i.record("g", 0, &denied).action {
            HookAction::PreToolUse(r) => match r.outcome {
                PreToolUseOutcome::Denied(d) => assert_eq!(d.reason.as_deref(), Some("root")),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// `SessionStart` cannot block, so a block-shaped output can only ever
    /// become a failure. The type makes the other reading unrepresentable.
    #[test]
    fn session_start_records_context_or_failure_and_nothing_else() {
        let i = HookInvocation::SessionStart { source: "startup" };
        let ran = HookOutput {
            additional_context: Some("conventions".into()),
            ..Default::default()
        };
        match i.record("g", 1, &ran).action {
            HookAction::SessionStart(r) => match r.outcome {
                SessionStartOutcome::Ran(c) => {
                    assert_eq!(c.additional_context.as_deref(), Some("conventions"));
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }

        let blocked = HookOutput {
            verdict: Verdict::Block { reason: None },
            ..Default::default()
        };
        match i.record("g", 1, &blocked).action {
            HookAction::SessionStart(r) => {
                assert!(matches!(r.outcome, SessionStartOutcome::Failed(_)));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn stop_records_a_block_as_a_block_not_a_failure() {
        let i = HookInvocation::Stop {
            last_assistant_message: None,
            stop_hook_active: false,
        };
        let blocked = HookOutput {
            verdict: Verdict::Block {
                reason: Some("tests fail".into()),
            },
            ..Default::default()
        };
        match i.record("g", 1, &blocked).action {
            HookAction::Stop(r) => match r.outcome {
                StopOutcome::Blocked(b) => assert_eq!(b.reason.as_deref(), Some("tests fail")),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    /// `systemMessage` reaches the record for every event that permits it — the
    /// field that has been parsed, stored and read by nobody since #140.
    #[test]
    fn a_system_message_is_carried_onto_the_record() {
        let i = HookInvocation::Stop {
            last_assistant_message: None,
            stop_hook_active: false,
        };
        let out = HookOutput {
            system_message: Some("heads up".into()),
            ..Default::default()
        };
        match i.record("g", 1, &out).action {
            HookAction::Stop(r) => assert_eq!(r.system_message.as_deref(), Some("heads up")),
            other => panic!("{other:?}"),
        }
    }
}

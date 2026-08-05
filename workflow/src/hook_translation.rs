//! Turning a hook record into the message it puts in front of the model.
//!
//! One rule decides every arm: **a hook record translates into the conversation
//! only when its effect has no other representation there.** A tool hook edits
//! the tool's own output, so the tool result already carries it. A `Stop` block
//! is a turn input and arrives as `AgentCommand::Resume { message }`. A
//! `system_message` is addressed to the user and never to the model. What is
//! left — context injected by a turn-scoped event — has nowhere else to live,
//! and becomes a message here.
//!
//! The match is exhaustive on purpose: wiring a new event cannot put text in
//! front of the model without someone deciding how.

use horsie_models::agent::{ContentPart, HookEntry, Message, Role, TextPart};
use horsie_models::hooks::{
    HookAction, HookRecord, PostToolBatchOutcome, SessionStartOutcome, StopOutcome,
    SubagentStartOutcome, SubagentStopOutcome, UserPromptSubmitOutcome,
};

/// The message a hook record contributes to the prompt, if any.
///
/// Derived at prompt assembly and never journaled, so the transcript keeps
/// showing a hook row where the model sees a message — which is what lets the
/// web client tell an intervention from something the user wrote.
#[must_use]
pub fn translate(entry: &HookEntry) -> Option<Message> {
    let (event, context) = match &entry.record.action {
        // --- Translated: turn-scoped context with nowhere else to live ---
        HookAction::SessionStart(r) => (
            "SessionStart",
            match &r.outcome {
                SessionStartOutcome::Ran(c) => c.additional_context.as_deref(),
                SessionStartOutcome::Failed(_) => None,
            },
        ),
        HookAction::UserPromptSubmit(r) => (
            "UserPromptSubmit",
            match &r.outcome {
                UserPromptSubmitOutcome::Ran(c) => c.additional_context.as_deref(),
                // A blocked prompt never starts a run, so there is no turn to
                // annotate; the seam abandons it. See [`prompt_blocked`].
                UserPromptSubmitOutcome::Blocked(_) | UserPromptSubmitOutcome::Failed(_) => None,
            },
        ),
        HookAction::Stop(r) => (
            "Stop",
            match &r.outcome {
                StopOutcome::Ran(c) => c.additional_context.as_deref(),
                // `Blocked` is a turn input, delivered as `Resume { message }`;
                // `CapReached` ends the turn; `Failed` is an outage.
                StopOutcome::Blocked(_) | StopOutcome::CapReached(_) | StopOutcome::Failed(_) => {
                    None
                }
            },
        ),
        HookAction::SubagentStart(r) => (
            "SubagentStart",
            match &r.outcome {
                SubagentStartOutcome::Ran(c) => c.additional_context.as_deref(),
                SubagentStartOutcome::Failed(_) => None,
            },
        ),
        HookAction::SubagentStop(r) => (
            "SubagentStop",
            match &r.outcome {
                SubagentStopOutcome::Ran(c) => c.additional_context.as_deref(),
                SubagentStopOutcome::Blocked(_) | SubagentStopOutcome::Failed(_) => None,
            },
        ),
        // The one tool-*named* event that translates: it spans N calls, so no
        // single tool result can carry it, and it fires at a turn boundary.
        HookAction::PostToolBatch(r) => (
            "PostToolBatch",
            match &r.outcome {
                PostToolBatchOutcome::Ran(c) => c.additional_context.as_deref(),
                PostToolBatchOutcome::Blocked(_) | PostToolBatchOutcome::Failed(_) => None,
            },
        ),
        // --- Never translated: already represented in the tool result ---
        //
        // A tool hook edits the tool's own output. `PreToolUse` has no
        // `additionalContext` by spec and its denial is a `ToolResult::Err`; the
        // other two have their context appended to the output by the runtime.
        HookAction::PreToolUse(_)
        | HookAction::PostToolUse(_)
        | HookAction::PostToolUseFailure(_)
        // --- Never translated: no model-visible content at all ---
        | HookAction::SessionEnd(_)
        | HookAction::StopFailure(_)
        | HookAction::Notification(_)
        | HookAction::CwdChanged(_)
        | HookAction::TaskCreated(_)
        | HookAction::TaskCompleted(_) => return None,
    };
    // `system_message` is deliberately never read here, on any arm: it is
    // addressed to the user, and pinned as such by
    // `a_system_message_is_recorded_and_never_injected`.
    let context = context?;
    Some(Message {
        id: format!("hook-context:{}", entry.id),
        role: Role::User,
        parts: vec![ContentPart::Text(TextPart {
            // Framed with its plugin, because a plugin is third-party and its
            // text must never read as horsie's own instruction.
            text: format!(
                "<hook-context plugin=\"{}\" event=\"{event}\">\n{context}\n</hook-context>",
                entry.record.plugin
            ),
        })],
        created_at_ms: entry.created_at_ms,
        started_at_ms: None,
    })
}

/// Why a `UserPromptSubmit` hook refused this prompt, if one did.
///
/// Not a translation: a blocked prompt is never journaled, so the run is
/// abandoned at the seam rather than filtered out of the fold.
#[must_use]
pub fn prompt_blocked(records: &[HookRecord]) -> Option<String> {
    records.iter().find_map(|r| match &r.action {
        HookAction::UserPromptSubmit(u) => match &u.outcome {
            UserPromptSubmitOutcome::Blocked(b) => Some(
                b.reason
                    .clone()
                    // A hook that refused without saying why still has to say
                    // something: this reaches the user as the turn's failure.
                    .unwrap_or_else(|| "a UserPromptSubmit hook blocked this prompt".to_string()),
            ),
            UserPromptSubmitOutcome::Ran(_) | UserPromptSubmitOutcome::Failed(_) => None,
        },
        // A single-event question, unlike `translate`: every other action is
        // simply not a `UserPromptSubmit` verdict.
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use horsie_models::hooks::{
        ContextInjected, CwdChangedRecord, HookBlocked, HookDenied, HookFailed, HookRecord,
        NotificationRecord, PostToolBatchRecord, PostToolUseFailureOutcome,
        PostToolUseFailureRecord, PostToolUseOutcome, PostToolUseRan, PostToolUseRecord,
        PreToolUseOutcome, PreToolUseRecord, SessionEndRecord, SessionStartRecord,
        SideEffectOutcome, StopFailureRecord, StopRecord, SubagentStartRecord, SubagentStopRecord,
        TaskCompletedRecord, TaskCreatedRecord, TaskOutcome, ToolScope, UserPromptSubmitRecord,
    };

    fn entry(action: HookAction) -> HookEntry {
        HookEntry {
            id: "hook:0".into(),
            created_at_ms: 1_000,
            record: HookRecord {
                plugin: "impeccable".into(),
                duration_ms: 3,
                action,
            },
        }
    }

    fn ctx(text: &str) -> ContextInjected {
        ContextInjected {
            additional_context: Some(text.into()),
        }
    }

    fn only_text(m: &Message) -> String {
        match m.parts.as_slice() {
            [ContentPart::Text(t)] => t.text.clone(),
            other => panic!("expected exactly one text part, got {other:?}"),
        }
    }

    #[test]
    fn session_start_context_becomes_a_framed_user_message() {
        let m = translate(&entry(HookAction::SessionStart(SessionStartRecord {
            source: "startup".into(),
            system_message: None,
            outcome: SessionStartOutcome::Ran(ctx("this repo pins node 22")),
        })))
        .expect("translated");
        assert!(matches!(m.role, Role::User));
        assert_eq!(m.id, "hook-context:hook:0");
        assert_eq!(m.created_at_ms, 1_000);
        let text = only_text(&m);
        assert!(text.starts_with("<hook-context plugin=\"impeccable\" event=\"SessionStart\">"));
        assert!(text.contains("this repo pins node 22"));
        assert!(text.ends_with("</hook-context>"));
    }

    #[test]
    fn stop_context_translates() {
        assert!(
            translate(&entry(HookAction::Stop(StopRecord {
                system_message: None,
                outcome: StopOutcome::Ran(ctx("remember to run the linter")),
            })))
            .is_some()
        );
    }

    #[test]
    fn user_prompt_submit_context_translates() {
        assert!(
            translate(&entry(HookAction::UserPromptSubmit(
                UserPromptSubmitRecord {
                    system_message: None,
                    outcome: UserPromptSubmitOutcome::Ran(ctx("current branch: main")),
                }
            )))
            .is_some()
        );
    }

    #[test]
    fn subagent_start_and_stop_context_translate() {
        assert!(
            translate(&entry(HookAction::SubagentStart(SubagentStartRecord {
                agent_type: "reviewer".into(),
                system_message: None,
                outcome: SubagentStartOutcome::Ran(ctx("house rules")),
            })))
            .is_some()
        );
        assert!(
            translate(&entry(HookAction::SubagentStop(SubagentStopRecord {
                agent_type: "reviewer".into(),
                system_message: None,
                outcome: SubagentStopOutcome::Ran(ctx("summarise findings")),
            })))
            .is_some()
        );
    }

    #[test]
    fn post_tool_batch_context_translates() {
        assert!(
            translate(&entry(HookAction::PostToolBatch(PostToolBatchRecord {
                calls: vec![],
                system_message: None,
                outcome: PostToolBatchOutcome::Ran(ctx("two files changed")),
            })))
            .is_some()
        );
    }

    /// The central division of the design: a tool hook edits the tool's own
    /// output, so the tool result already represents it and there is nothing
    /// left to translate.
    #[test]
    fn tool_scoped_records_never_translate() {
        let call = ToolScope {
            tool: "bash".into(),
            tool_call_id: "tc1".into(),
        };
        assert!(
            translate(&entry(HookAction::PreToolUse(PreToolUseRecord {
                call: call.clone(),
                system_message: Some("noisy".into()),
                outcome: PreToolUseOutcome::Denied(HookDenied {
                    reason: Some("no".into())
                }),
            })))
            .is_none()
        );
        assert!(
            translate(&entry(HookAction::PostToolUse(PostToolUseRecord {
                call: call.clone(),
                system_message: None,
                outcome: PostToolUseOutcome::Ran(PostToolUseRan {
                    output: None,
                    additional_context: Some("appended by the runtime".into()),
                }),
            })))
            .is_none()
        );
        assert!(
            translate(&entry(HookAction::PostToolUseFailure(
                PostToolUseFailureRecord {
                    call,
                    system_message: None,
                    outcome: PostToolUseFailureOutcome::Ran(ctx("also appended")),
                }
            )))
            .is_none()
        );
    }

    /// A block reason is a turn *input* (`Resume { message }`), not context; a
    /// cap-reached turn is over; a failure is an outage, not content.
    #[test]
    fn stop_block_cap_and_failure_never_translate() {
        for outcome in [
            StopOutcome::Blocked(HookBlocked {
                reason: Some("keep going".into()),
            }),
            StopOutcome::CapReached(HookBlocked { reason: None }),
            StopOutcome::Failed(HookFailed {
                reason: "timeout".into(),
            }),
        ] {
            assert!(
                translate(&entry(HookAction::Stop(StopRecord {
                    system_message: Some("shown to the user only".into()),
                    outcome,
                })))
                .is_none()
            );
        }
    }

    #[test]
    fn side_effect_and_task_events_never_translate() {
        for action in [
            HookAction::SessionEnd(SessionEndRecord {
                reason: "clear".into(),
                outcome: SideEffectOutcome::Ran,
            }),
            HookAction::StopFailure(StopFailureRecord {
                error: "rate_limit".into(),
                outcome: SideEffectOutcome::Ran,
            }),
            HookAction::Notification(NotificationRecord {
                message: "hi".into(),
                outcome: SideEffectOutcome::Ran,
            }),
            HookAction::CwdChanged(CwdChangedRecord {
                cwd: "/tmp".into(),
                outcome: SideEffectOutcome::Ran,
            }),
            HookAction::TaskCreated(TaskCreatedRecord {
                task_id: "t1".into(),
                system_message: Some("user-facing".into()),
                outcome: TaskOutcome::Ran,
            }),
            HookAction::TaskCompleted(TaskCompletedRecord {
                task_id: "t1".into(),
                system_message: None,
                outcome: TaskOutcome::Ran,
            }),
        ] {
            assert!(translate(&entry(action)).is_none());
        }
    }

    #[test]
    fn an_empty_additional_context_translates_to_nothing() {
        assert!(
            translate(&entry(HookAction::SessionStart(SessionStartRecord {
                source: "startup".into(),
                system_message: None,
                outcome: SessionStartOutcome::Ran(ContextInjected {
                    additional_context: None,
                }),
            })))
            .is_none()
        );
    }

    #[test]
    fn a_blocking_user_prompt_submit_record_reports_its_reason() {
        let records = vec![HookRecord {
            plugin: "guard".into(),
            duration_ms: 1,
            action: HookAction::UserPromptSubmit(UserPromptSubmitRecord {
                system_message: None,
                outcome: UserPromptSubmitOutcome::Blocked(HookBlocked {
                    reason: Some("secrets in the prompt".into()),
                }),
            }),
        }];
        assert_eq!(
            prompt_blocked(&records).as_deref(),
            Some("secrets in the prompt")
        );
        assert!(prompt_blocked(&[]).is_none());
    }
}

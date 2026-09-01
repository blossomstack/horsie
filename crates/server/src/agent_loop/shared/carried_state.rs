//! What a compaction must carry across verbatim.
//!
//! A summary is prose and may be wrong at the edges. These are facts with ids
//! in them, and an agent that reads a paraphrase of its own task list cannot
//! call `task_list` correctly afterwards. So they are rendered from state and
//! never shown to the summariser.
//!
//! The distinction this module exists for: **state surviving is not the same as
//! the model knowing it survived.** `task_list`, `set_timer` and `ask_user` all
//! keep durable state in [`AgentState`], and every one of them is invisible to
//! the model except through the tool calls in the history that a compaction
//! summarises away. Without this an agent wakes up holding three open tasks and
//! two armed timers, with no idea it has any.
//!
//! Deliberately *not* here: the working directory and environment overrides.
//! Those live in the runtime, keyed by agent id, so reading them would mean a
//! round-trip from inside the compaction path. The loss is small and
//! self-healing — the system prompt still names the workspace root every turn,
//! and an agent that is unsure can run `pwd` — where a lost task list is
//! neither.

use crate::agent_loop::AgentState;
use horsie_agentcore::{AgentLogBody, LifecycleEvent};
use std::collections::BTreeMap;

/// Every exact fact this agent must not forget, as one block.
///
/// Empty string when there is nothing to carry, so a session that never used a
/// task list or a timer gets no section of boilerplate saying so.
#[must_use]
pub fn render_carried_state(state: &AgentState) -> String {
    let mut sections: Vec<String> = Vec::new();

    if !state.task_list().tasks().is_empty() {
        sections.push(state.task_list().render());
    }

    if !state.timers().is_empty() {
        let mut block = String::from("Armed timers:");
        for t in state.timers() {
            block.push_str(&format!(
                "\n- {} ({}) fires at {}ms: {}",
                t.id,
                t.label,
                t.fire_at_unix_ms,
                if t.message.is_empty() {
                    "(no message)"
                } else {
                    t.message.as_str()
                }
            ));
        }
        sections.push(block);
    }

    if !state.asks().is_empty() {
        let mut block = String::from("Questions you are waiting on an answer to:");
        for a in state.asks() {
            block.push_str(&format!(
                "\n- [{}] {}",
                a.tool_call_id.as_deref().unwrap_or("unknown call"),
                a.question
            ));
        }
        sections.push(block);
    }

    let running = running_children(state);
    if !running.is_empty() {
        let mut block = String::from("Delegated work still running:");
        for (id, label) in running {
            block.push_str(&format!("\n- {id} ({label})"));
        }
        sections.push(block);
    }

    sections.join("\n\n")
}

/// Whether any work this agent delegated — a subagent it spawned, or a
/// workflow it invoked — is still running; that is, whether a report is still
/// owed to it.
///
/// The agent's own view, read off its own log: the session records every
/// spawn, every invocation and every ending on the *parent*, because the
/// parent is what a person has open while it waits. So this is not a second
/// copy of the session's forest — it is the agent actor's own state, which is
/// what makes it checkable before the agent is allowed to finish.
#[must_use]
pub fn has_outstanding_children(state: &AgentState) -> bool {
    !running_children(state).is_empty()
}

/// Delegated work that has not reported a terminal status. Invoked workflow
/// runs ride the same lifecycle vocabulary as subagents, so one read covers
/// both.
///
/// Read off the log's lifecycle entries rather than from a field, because that
/// is where the fact lives: the newest entry for an id is its current status.
fn running_children(state: &AgentState) -> Vec<(String, String)> {
    let mut latest: BTreeMap<String, (String, String)> = BTreeMap::new();
    for entry in state.log() {
        if let AgentLogBody::Lifecycle(LifecycleEvent::SubAgent(s)) = &entry.body {
            latest.insert(s.id.clone(), (s.title.clone(), s.status.clone()));
        }
    }
    latest
        .into_iter()
        .filter(|(_, (_, status))| status == "running")
        .map(|(id, (label, _))| (id, label))
        .collect()
}

/// Why a `PreCompact` hook refused, if one did.
///
/// A block *or* a halt: `{"decision":"block"}` says "not this compaction" and
/// `continue: false` says "stop entirely", and from here the answer is the same
/// — do not rewrite the history. The turn then runs uncompacted, which is worse
/// than compacting but better than compacting past a hook that was about to
/// save something.
#[must_use]
pub(crate) fn precompact_refusal(records: &[horsie_models::hooks::HookRecord]) -> Option<String> {
    use horsie_models::hooks::{HookAction, StopOutcome};
    records.iter().find_map(|r| {
        if let Some(halt) = &r.halt {
            return Some(
                halt.reason
                    .clone()
                    .unwrap_or_else(|| "a PreCompact hook set continue: false".to_string()),
            );
        }
        match &r.action {
            HookAction::PreCompact(p) => match &p.outcome {
                StopOutcome::Blocked(b) => Some(
                    b.reason
                        .clone()
                        .unwrap_or_else(|| "a PreCompact hook blocked this compaction".to_string()),
                ),
                // A hook that could not run cannot refuse: only `PreToolUse`
                // fails closed, and losing a compaction to a broken guard would
                // silently fill the context instead.
                StopOutcome::Ran(_) | StopOutcome::Failed(_) | StopOutcome::CapReached(_) => None,
            },
            // Only `PreCompact` decides a compaction. Every other record in the
            // batch is something else that happened to run.
            HookAction::PreToolUse(_)
            | HookAction::PostToolUse(_)
            | HookAction::PostToolUseFailure(_)
            | HookAction::PostToolBatch(_)
            | HookAction::PostCompact(_)
            | HookAction::SessionStart(_)
            | HookAction::SessionEnd(_)
            | HookAction::UserPromptSubmit(_)
            | HookAction::UserPromptExpansion(_)
            | HookAction::Stop(_)
            | HookAction::StopFailure(_)
            | HookAction::SubagentStart(_)
            | HookAction::SubagentStop(_)
            | HookAction::TaskCreated(_)
            | HookAction::TaskCompleted(_)
            | HookAction::Notification(_)
            | HookAction::CwdChanged(_) => None,
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::components::timers::domain::{TimerKind, TimerRecord};
    use horsie_agentcore::{AgentLogEntry, SubAgentLifecycle};

    /// Whether a report is still owed decides what a turn ending with plain
    /// text *means*: a park while the children work, or a step that stopped
    /// with nothing to wake it. Reading `has_active` off the session's tree
    /// instead would be a second copy of this fact; the parent's own log
    /// already carries every spawn and every ending.
    #[test]
    fn a_running_subagent_is_owed_and_a_finished_one_is_not() {
        let mut state = AgentState::default();
        assert!(!has_outstanding_children(&state), "nothing spawned yet");
        note_subagent(&mut state, "s1", "research", "running");
        assert!(has_outstanding_children(&state));
        note_subagent(&mut state, "s1", "research", "completed");
        assert!(
            !has_outstanding_children(&state),
            "the newest entry for an id is its status"
        );
    }

    /// A child that failed owes nothing either — the parent hears about it the
    /// same way. Missing this would leave a step parked for ever on a subagent
    /// that is never coming back.
    #[test]
    fn a_failed_subagent_is_not_still_owed() {
        let mut state = AgentState::default();
        note_subagent(&mut state, "s1", "research", "running");
        note_subagent(&mut state, "s1", "research", "failed");
        assert!(!has_outstanding_children(&state));
    }

    /// Append a subagent-progress entry the way the session would.
    fn note_subagent(state: &mut AgentState, id: &str, title: &str, status: &str) {
        state.log.push(AgentLogEntry {
            seq: state.next_seq,
            at_ms: 1,
            body: AgentLogBody::Lifecycle(LifecycleEvent::SubAgent(SubAgentLifecycle {
                id: id.into(),
                title: title.into(),
                status: status.into(),
            })),
        });
        state.next_seq += 1;
    }

    fn with_tasks(state: &mut AgentState, tasks: &[&str]) {
        state
            .task_list
            .apply(crate::agent_loop::components::task_list::domain::TaskListAction::Create {
                tasks: tasks.iter().map(|t| (*t).to_string()).collect(),
            })
            .expect("the fixture's task list is valid");
    }

    /// The test the whole module exists for. A summariser is given deliberately
    /// vague prose to produce; every id and label below must still appear
    /// literally, because they came from state and never went near it.
    #[test]
    fn carried_state_names_every_task_timer_and_ask_verbatim() {
        let mut state = AgentState::default();
        with_tasks(&mut state, &["migrate the journal", "delete the importer"]);
        state.timers.push(TimerRecord {
            id: crate::agent_loop::components::timers::domain::TimerId("timer-7".into()),
            label: "nightly".into(),
            message: "re-run the sweep".into(),
            kind: TimerKind::Recurring,
            interval_secs: 86_400,
            fire_at_unix_ms: 1_700_000_000_000,
            fire_count: 2,
        });
        state.asks.push(crate::agent_loop::AskedQuestion {
            tool_call_id: Some("tc-42".into()),
            question: "Which database should this point at?".into(),
            choices: Vec::new(),
            multiple: false,
        });

        let rendered = render_carried_state(&state);

        for needle in [
            "migrate the journal",
            "delete the importer",
            "nightly",
            "timer-7",
            "re-run the sweep",
            "1700000000000",
            "tc-42",
            "Which database should this point at?",
        ] {
            assert!(
                rendered.contains(needle),
                "carried state lost {needle:?}:\n{rendered}"
            );
        }
        // Task ids, which are what `task_list` is called with, must survive as
        // numbers rather than as positions in prose.
        assert!(rendered.contains("1. migrate the journal"));
        assert!(rendered.contains("2. delete the importer"));
    }

    // --- PreCompact refusal ------------------------------------------------

    use horsie_models::hooks::{
        HookAction, HookBlocked, HookHalt, HookRecord, PreCompactRecord, StopOutcome,
    };

    fn precompact(outcome: StopOutcome, halt: Option<HookHalt>) -> HookRecord {
        HookRecord {
            plugin: "guard".into(),
            duration_ms: 1,
            halt,
            action: HookAction::PreCompact(PreCompactRecord {
                trigger: "auto".into(),
                system_message: None,
                outcome,
            }),
        }
    }

    #[test]
    fn a_blocking_precompact_hook_refuses_with_its_reason() {
        let records = vec![precompact(
            StopOutcome::Blocked(HookBlocked {
                reason: Some("still writing notes".into()),
            }),
            None,
        )];
        assert_eq!(
            precompact_refusal(&records),
            Some("still writing notes".to_string())
        );
    }

    #[test]
    fn a_hook_that_refuses_without_a_reason_still_says_something() {
        let records = vec![precompact(
            StopOutcome::Blocked(HookBlocked { reason: None }),
            None,
        )];
        assert!(precompact_refusal(&records).is_some());
    }

    /// A halt and a block are different statements, and from here they have the
    /// same consequence — do not rewrite the history.
    #[test]
    fn a_halt_refuses_as_surely_as_a_block() {
        let records = vec![precompact(
            StopOutcome::Ran(horsie_models::hooks::ContextInjected {
                additional_context: None,
            }),
            Some(HookHalt {
                reason: Some("shutting down".into()),
            }),
        )];
        assert_eq!(precompact_refusal(&records), Some("shutting down".into()));
    }

    /// A guard that could not run must not be able to stop compaction: the
    /// context would silently fill instead. Only `PreToolUse` fails closed.
    #[test]
    fn a_failed_precompact_hook_does_not_refuse() {
        let records = vec![precompact(
            StopOutcome::Failed(horsie_models::hooks::HookFailed {
                reason: "exec format error".into(),
            }),
            None,
        )];
        assert_eq!(precompact_refusal(&records), None);
    }

    #[test]
    fn no_hooks_at_all_never_refuses() {
        assert_eq!(precompact_refusal(&[]), None);
    }

    #[test]
    fn a_session_with_nothing_to_carry_renders_nothing() {
        assert_eq!(render_carried_state(&AgentState::default()), "");
    }

    #[test]
    fn only_subagents_still_running_are_carried() {
        let mut state = AgentState::default();
        for (id, status) in [("a1", "running"), ("a2", "completed"), ("a1", "running")] {
            note_subagent(&mut state, id, &format!("{id}-label"), status);
        }
        let rendered = render_carried_state(&state);
        assert!(rendered.contains("a1"), "{rendered}");
        assert!(
            !rendered.contains("a2"),
            "a finished subagent is not something to wait for: {rendered}"
        );
    }

    /// The newest entry for an id wins, so a subagent that has since finished
    /// is not reported as still running.
    #[test]
    fn a_subagent_that_finished_after_starting_is_not_carried() {
        let mut state = AgentState::default();
        for status in ["running", "completed"] {
            note_subagent(&mut state, "a1", "reviewer", status);
        }
        assert!(!render_carried_state(&state).contains("reviewer"));
    }
}

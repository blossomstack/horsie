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

use super::agent_actor::AgentState;
use horsie_agentcore::{AgentLogBody, LifecycleEvent};
use std::collections::BTreeMap;

/// Every exact fact this agent must not forget, as one block.
///
/// Empty string when there is nothing to carry, so a session that never used a
/// task list or a timer gets no section of boilerplate saying so.
#[must_use]
pub fn render_carried_state(state: &AgentState) -> String {
    let mut sections: Vec<String> = Vec::new();

    if !state.task_list.tasks().is_empty() {
        sections.push(state.task_list.render());
    }

    if !state.timers.is_empty() {
        let mut block = String::from("Armed timers:");
        for t in &state.timers {
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

    if !state.asks.is_empty() {
        let mut block = String::from("Questions you are waiting on an answer to:");
        for a in &state.asks {
            block.push_str(&format!(
                "\n- [{}] {}",
                a.tool_call_id.as_deref().unwrap_or("unknown call"),
                a.question
            ));
        }
        sections.push(block);
    }

    let running = running_subagents(state);
    if !running.is_empty() {
        let mut block = String::from("Subagents still running:");
        for (id, label) in running {
            block.push_str(&format!("\n- {id} ({label})"));
        }
        sections.push(block);
    }

    sections.join("\n\n")
}

/// Subagents this agent spawned that have not reported a terminal status.
///
/// Read off the log's lifecycle entries rather than from a field, because that
/// is where the fact lives: the session records a subagent's progress on its
/// parent's log, and the newest entry for an id is its current status.
fn running_subagents(state: &AgentState) -> Vec<(String, String)> {
    let mut latest: BTreeMap<String, (String, String)> = BTreeMap::new();
    for entry in &state.log {
        if let AgentLogBody::Lifecycle(LifecycleEvent::SubAgent(s)) = &entry.body {
            latest.insert(s.id.clone(), (s.label.clone(), s.status.clone()));
        }
    }
    latest
        .into_iter()
        .filter(|(_, (_, status))| status == "running")
        .map(|(id, (label, _))| (id, label))
        .collect()
}

/// The owner's half of compaction, answered by the agent actor and its runtime.
///
/// Held by the *run's* task, not by the actor, for the same reason the hook
/// sinks are: a thirty-second hook must not be able to block a cancel.
pub struct ActorCompactionPolicy {
    actor: horsie_actor::ActorRef<super::AgentCommand>,
}

impl ActorCompactionPolicy {
    #[must_use]
    pub fn new(actor: horsie_actor::ActorRef<super::AgentCommand>) -> Self {
        Self { actor }
    }
}

#[async_trait::async_trait]
impl horsie_agentcore::CompactionPolicy for ActorCompactionPolicy {
    async fn carried_state(&self) -> String {
        // A failed ask means the actor is gone, which means this run is about
        // to be torn down anyway. An empty block is the safe answer: the
        // compaction still records what it summarised, and nothing invents
        // state that could not be read.
        self.actor
            .ask(|reply| super::AgentCommand::CarriedState { reply })
            .await
            .unwrap_or_default()
    }

    async fn before(
        &self,
        _plan: &horsie_agentcore::CompactionPlan,
    ) -> horsie_agentcore::PreCompactDecision {
        // `PreCompact` is wired in the next change; until then every compaction
        // proceeds, which is the behaviour a session with no plugins has anyway.
        horsie_agentcore::PreCompactDecision::Proceed
    }

    async fn after(&self, _result: &horsie_agentcore::CompactionResult) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::timers::{TimerKind, TimerRecord};
    use horsie_agentcore::{AgentLogEntry, SubAgentLifecycle};

    /// Append a subagent-progress entry the way the session would.
    fn note_subagent(state: &mut AgentState, id: &str, label: &str, status: &str) {
        state.log.push(AgentLogEntry {
            seq: state.next_seq,
            at_ms: 1,
            body: AgentLogBody::Lifecycle(LifecycleEvent::SubAgent(SubAgentLifecycle {
                id: id.into(),
                label: label.into(),
                status: status.into(),
            })),
        });
        state.next_seq += 1;
    }

    fn with_tasks(state: &mut AgentState, tasks: &[&str]) {
        state
            .task_list
            .apply(crate::agent_loop::task_list::TaskListAction::Create {
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
            id: crate::agent_loop::timers::TimerId("timer-7".into()),
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

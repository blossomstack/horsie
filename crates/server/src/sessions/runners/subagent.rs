//! One delegated worker: a task in, one report out.
//!
//! Most of this runner is a translation. An agent ends five ways; the agent
//! that asked for the work understands two — a report, or a failure — and
//! [`AgentLifecycle::on_agent_ended`] is the single place the five become the
//! two. The shape this replaces had nowhere to put that, so the *parent*
//! carried arms for `Asked` and `Parked` annotated "a subagent has no ask or
//! timer tools, so neither outcome should ever occur". Here they are not
//! defence but the answer: a worker that parked has ended in a way its asker
//! cannot read, and the only honest report is a failure. The parent never sees
//! a [`TurnEnd`] and so never learns how a worker is implemented.
//!
//! [`Runner::actions`] is the rest of it. It reads the folded state rather
//! than a flag, so "I have no agent, so start one" is the same code path
//! whether that state was written a millisecond ago or replayed from a journal
//! after a restart. A `run()` that fired once would need a second entry for
//! recovery, and the suppression that implies is what double-starts a worker.

use super::action::FirstInput;
use super::capabilities::{Capability, equip};
use super::message::{ChildOutcome, SubAgentOutcome};
use super::{Action, AgentId, AgentLifecycle, Emit, Runner, RunnerEvent, SessionView, TurnEnd};
use crate::agent_loop::UsageTotal;
use crate::sessions::spec::AgentSettings;
use serde::{Deserialize, Serialize};

/// How this worker ended, in its own words.
///
/// Two arms, because two is all the agent that asked can read. Everything else
/// an agent can do — ask, park, be interrupted — is translated into one of
/// these before it is recorded, so no reader of a finished worker has to know
/// the wider vocabulary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Outcome {
    Completed { report: String },
    Failed { error: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// The worker, once started. `None` is what [`Runner::actions`] reads as
    /// "start it".
    pub agent: Option<AgentId>,
    /// What the asking agent called this piece of work. Carried on every
    /// report, because the asker addresses its workers by label and not by id.
    pub label: String,
    /// The task, verbatim. It is also the worker's first input, so a restart
    /// before the agent existed re-sends exactly what was asked for.
    pub task: String,
    /// A plugin-declared agent type, or `None` for a worker that inherits its
    /// asker's instructions and tools.
    pub agent_type: Option<String>,
    /// What this worker runs under, fixed when it was created.
    pub settings: AgentSettings,
    /// This worker's own tokens. The session's aggregate is by model; the
    /// breakdown belongs to the runner that owns the agent.
    pub usage: UsageTotal,
    /// `Some` once it has ended, whichever way. The one field that says both
    /// "stop starting agents" and "there is a report to hand over".
    pub result: Option<Outcome>,
    pub capabilities: Vec<Capability>,
}

/// Not derived: [`AgentSettings`] has no `Default`. A live worker's settings
/// arrive with its args; this is the empty slice, and nothing else builds one.
#[cfg(test)]
impl Default for State {
    fn default() -> Self {
        Self {
            agent: None,
            label: String::new(),
            task: String::new(),
            agent_type: None,
            settings: super::empty_settings(),
            usage: UsageTotal::default(),
            result: None,
            capabilities: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Started { agent: AgentId },
    Concluded { output: String },
    Failed { error: String },
}

impl Runner for State {
    fn actions(&self, _view: &SessionView) -> Vec<Action> {
        if self.agent.is_some() || self.result.is_some() {
            return Vec::new();
        }
        vec![Action::StartAgent {
            // Minted here rather than in the fold: a decision may be
            // non-deterministic, a fold may not. The session journals the
            // `Started` that carries this id when it performs the action.
            agent: AgentId::new_v4(),
            spec: Box::new(equip(&self.capabilities, self.settings.clone())),
            first: FirstInput::Text(self.task.clone()),
        }]
    }

    fn outcome(&self) -> Option<ChildOutcome> {
        let result = self.result.as_ref()?;
        Some(ChildOutcome::SubAgent(match result {
            Outcome::Completed { report } => SubAgentOutcome::Completed {
                label: self.label.clone(),
                report: report.clone(),
            },
            Outcome::Failed { error } => SubAgentOutcome::Failed {
                label: self.label.clone(),
                error: error.clone(),
            },
        }))
    }

    fn busy(&self) -> bool {
        self.agent.is_some() && self.result.is_none()
    }

    fn capabilities(&self) -> &[Capability] {
        &self.capabilities
    }

    fn capabilities_mut(&mut self) -> &mut [Capability] {
        &mut self.capabilities
    }

    fn apply(&mut self, event: &RunnerEvent) {
        // Every other arm belongs to another runner, or is a capability's own
        // event that `RunnerState::apply` has already routed.
        let RunnerEvent::SubAgent(event) = event else {
            return;
        };
        match event {
            Event::Started { agent } => self.agent = Some(*agent),
            Event::Concluded { output } => {
                self.result = Some(Outcome::Completed {
                    report: output.clone(),
                });
            }
            Event::Failed { error } => {
                self.result = Some(Outcome::Failed {
                    error: error.clone(),
                });
            }
        }
    }
}

impl AgentLifecycle for State {
    /// Nothing: the `Started` the session journals when it performs the start
    /// already recorded the only fact there is. An event here would say it
    /// twice, and two writers of one field is how they come to disagree.
    fn on_agent_started(&self, _agent: AgentId) -> Emit {
        (Vec::new(), Vec::new())
    }

    fn on_agent_ended(&self, _agent: AgentId, end: &TurnEnd) -> Emit {
        let event = match end {
            TurnEnd::Concluded { output } => Event::Concluded {
                output: render(output),
            },
            // `terminal` is not read: a worker gets one run, so a failure it
            // could in principle retry still ends it, and the asker is owed an
            // answer either way.
            TurnEnd::Failed { error, .. } => Event::Failed {
                error: error.clone(),
            },
            // Real behaviour rather than defence. A worker is equipped with no
            // ask tool, so this is a worker that has stopped for an answer
            // nobody will ever give it; failing it is what stops the asker
            // waiting for ever.
            TurnEnd::Asked => Event::Failed {
                error: "a subagent has no ask tool, so it cannot stop for an answer".to_string(),
            },
            TurnEnd::Parked => Event::Failed {
                error: "a subagent has no timer tools, so it cannot park awaiting one".to_string(),
            },
            // Deliberately nothing. The session reconciles a child that was
            // interrupted when it loads, so failing it here would fail the same
            // worker twice — once from the report, once from the reconcile.
            TurnEnd::Interrupted => return (Vec::new(), Vec::new()),
        };
        (vec![RunnerEvent::SubAgent(event)], Vec::new())
    }

    fn on_agent_halted(&self, _agent: AgentId, reason: &str) -> Emit {
        (
            vec![RunnerEvent::SubAgent(Event::Failed {
                error: reason.to_string(),
            })],
            Vec::new(),
        )
    }
}

/// A worker's output as the text its asker will read.
///
/// A JSON string is its own contents — an agent that concluded with plain text
/// must not have its report delivered wrapped in quotes — and anything else is
/// its JSON, so a structured conclusion survives as something the asking model
/// can parse.
fn render(output: &serde_json::Value) -> String {
    if let serde_json::Value::String(text) = output {
        text.clone()
    } else {
        output.to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::runners::action::ToolLayer;
    use crate::sessions::runners::capabilities::title::TitleCapability;

    fn worker() -> State {
        State {
            label: "read the flake".into(),
            task: "look at the last three runs".into(),
            ..State::default()
        }
    }

    fn view() -> SessionView {
        SessionView {
            runtime_ready: true,
            depth: 0,
            active_agents: 0,
        }
    }

    fn ended(state: &State, end: &TurnEnd) -> Vec<RunnerEvent> {
        state.on_agent_ended(AgentId::new_v4(), end).0
    }

    fn only_event(events: Vec<RunnerEvent>) -> Event {
        assert_eq!(events.len(), 1, "expected one event, got {events:?}");
        let RunnerEvent::SubAgent(event) = &events[0] else {
            panic!("expected a subagent event, got {:?}", events[0]);
        };
        event.clone()
    }

    /// **The most important behaviour in this module.** Starting is a pure
    /// function of the folded state, which is what lets creation and recovery
    /// share one path: a replayed worker that never started its agent starts it
    /// now, and a worker that has one — or that has ended — asks for nothing.
    /// Lose the idempotence and every restart double-starts a worker, or needs
    /// a suppression flag that a restart is exactly the thing to get wrong.
    #[test]
    fn starting_is_idempotent() {
        let mut state = worker();
        assert_eq!(state.actions(&view()).len(), 1);
        // Called again on the same state — nothing has moved, so it is the
        // same single request, not a second one.
        assert_eq!(state.actions(&view()).len(), 1);

        state.apply(&RunnerEvent::SubAgent(Event::Started {
            agent: AgentId::new_v4(),
        }));
        assert!(state.actions(&view()).is_empty());

        state.apply(&RunnerEvent::SubAgent(Event::Concluded {
            output: "done".into(),
        }));
        assert!(state.actions(&view()).is_empty());

        // And a worker that failed before it ever had an agent stays stopped:
        // `agent.is_none()` alone would restart it for ever.
        let failed = State {
            result: Some(Outcome::Failed {
                error: "the create failed".into(),
            }),
            ..worker()
        };
        assert!(failed.actions(&view()).is_empty());
    }

    /// The task is the worker's first input. If it were dropped, the agent
    /// would start with an empty queue and sit there — a worker that exists,
    /// consumes a slot, and was never told what to do.
    #[test]
    fn the_task_is_the_first_input() {
        let state = worker();
        let actions = state.actions(&view());
        let Action::StartAgent { first, .. } = &actions[0] else {
            panic!("expected a start, got {:?}", actions[0]);
        };
        let FirstInput::Text(text) = first else {
            panic!("expected the task as text, got {first:?}");
        };
        assert_eq!(text, "look at the last three runs");
    }

    /// Equipment comes from folding this runner's capabilities, so what a
    /// worker can do is decided in one place rather than by a match on what
    /// kind of agent it is.
    #[test]
    fn the_agent_is_equipped_by_folding_the_capabilities() {
        let state = State {
            capabilities: vec![Capability::Title(TitleCapability::default())],
            ..worker()
        };
        let actions = state.actions(&view());
        let Action::StartAgent { spec, .. } = &actions[0] else {
            panic!("expected a start, got {:?}", actions[0]);
        };
        assert!(spec.has(&ToolLayer::SessionTitle));
        assert_eq!(
            spec.settings.as_ref().map(|s| s.model.clone()),
            Some(String::new())
        );
    }

    /// No outcome while it runs. A worker that reported before it finished
    /// would unblock the agent that asked with a report it does not have.
    #[test]
    fn there_is_no_outcome_until_it_ends() {
        let mut state = worker();
        assert!(state.outcome().is_none());
        state.apply(&RunnerEvent::SubAgent(Event::Started {
            agent: AgentId::new_v4(),
        }));
        assert!(state.outcome().is_none());
    }

    /// A conclusion is a completed report carrying the label the asker used.
    /// Drop the label and the asking model reads a report it cannot tie to the
    /// work it delegated.
    #[test]
    fn a_concluded_worker_reports_completed_with_its_label() {
        let mut state = worker();
        state.apply(&RunnerEvent::SubAgent(Event::Concluded {
            output: "three flakes, all in setup".into(),
        }));
        let Some(ChildOutcome::SubAgent(SubAgentOutcome::Completed { label, report })) =
            state.outcome()
        else {
            panic!("expected a completed report, got {:?}", state.outcome());
        };
        assert_eq!(label, "read the flake");
        assert_eq!(report, "three flakes, all in setup");
    }

    /// A failure is a report too, in the same vocabulary. An asker blocked on a
    /// worker that died and was never told would wait for ever.
    #[test]
    fn a_failed_worker_reports_failed_with_its_label() {
        let mut state = worker();
        state.apply(&RunnerEvent::SubAgent(Event::Failed {
            error: "the sandbox is gone".into(),
        }));
        let Some(ChildOutcome::SubAgent(SubAgentOutcome::Failed { label, error })) =
            state.outcome()
        else {
            panic!("expected a failed report, got {:?}", state.outcome());
        };
        assert_eq!(label, "read the flake");
        assert_eq!(error, "the sandbox is gone");
    }

    /// Busy is "an agent is out there working". A worker that read busy after
    /// it reported would hold the session resident for ever; one that read idle
    /// while its agent ran would let the session unload mid-turn.
    #[test]
    fn it_is_busy_only_between_starting_and_ending() {
        let mut state = worker();
        assert!(!state.busy());
        state.apply(&RunnerEvent::SubAgent(Event::Started {
            agent: AgentId::new_v4(),
        }));
        assert!(state.busy());
        state.apply(&RunnerEvent::SubAgent(Event::Concluded {
            output: "done".into(),
        }));
        assert!(!state.busy());
    }

    /// Only its own events touch its slice. Folding a neighbour's event here
    /// would let one runner's log rewrite another's state on replay.
    #[test]
    fn another_runners_event_is_a_no_op() {
        let mut state = worker();
        state.apply(&RunnerEvent::Conversation(
            crate::sessions::runners::conversation::Event::TurnBegan,
        ));
        assert!(state.agent.is_none());
        assert!(state.result.is_none());
    }

    /// A plain-text conclusion is delivered as its text and a structured one as
    /// its JSON. Render a string with `to_string` and every report the asker
    /// reads arrives wrapped in quotes and escaped.
    #[test]
    fn a_string_conclusion_keeps_its_text_and_anything_else_is_json() {
        let state = worker();
        let event = only_event(ended(
            &state,
            &TurnEnd::Concluded {
                output: serde_json::json!("found it"),
            },
        ));
        let Event::Concluded { output } = event else {
            panic!("expected a conclusion, got {event:?}");
        };
        assert_eq!(output, "found it");

        let event = only_event(ended(
            &state,
            &TurnEnd::Concluded {
                output: serde_json::json!({"files": 3}),
            },
        ));
        let Event::Concluded { output } = event else {
            panic!("expected a conclusion, got {event:?}");
        };
        assert_eq!(output, r#"{"files":3}"#);
    }

    /// A failing turn carries its error through verbatim, terminal or not: the
    /// asker gets one answer per worker and this is it.
    #[test]
    fn a_failing_turn_fails_the_worker_with_its_error() {
        let state = worker();
        for terminal in [true, false] {
            let event = only_event(ended(
                &state,
                &TurnEnd::Failed {
                    error: "the model refused".into(),
                    terminal,
                },
            ));
            let Event::Failed { error } = event else {
                panic!("expected a failure, got {event:?}");
            };
            assert_eq!(error, "the model refused");
        }
    }

    /// A worker has no ask tool, so an `Asked` is a worker stopped for an
    /// answer nobody will give. Translating it here is what keeps the parent
    /// free of the defensive arms it used to carry — and what stops the asking
    /// agent blocking on a worker that will never speak again.
    #[test]
    fn an_ask_fails_the_worker() {
        let event = only_event(ended(&worker(), &TurnEnd::Asked));
        let Event::Failed { error } = event else {
            panic!("expected a failure, got {event:?}");
        };
        assert!(error.contains("ask"));
    }

    /// The same for timers, for the same reason.
    #[test]
    fn a_park_fails_the_worker() {
        let event = only_event(ended(&worker(), &TurnEnd::Parked));
        let Event::Failed { error } = event else {
            panic!("expected a failure, got {event:?}");
        };
        assert!(error.contains("timer"));
    }

    /// An interruption emits nothing at all. The session reconciles an
    /// interrupted child when it loads; a failure written here would fail the
    /// same worker a second time and deliver two reports for one task.
    #[test]
    fn an_interruption_emits_nothing() {
        let (events, actions) = worker().on_agent_ended(AgentId::new_v4(), &TurnEnd::Interrupted);
        assert!(events.is_empty());
        assert!(actions.is_empty());
    }

    /// Starting journals nothing here: the session's own `Started` already
    /// recorded the agent, and a second writer of that field is how the log and
    /// the state come to disagree.
    #[test]
    fn starting_an_agent_records_nothing_further() {
        let (events, actions) = worker().on_agent_started(AgentId::new_v4());
        assert!(events.is_empty());
        assert!(actions.is_empty());
    }

    /// A halt is a failure with the halting reason as the report. Without it, a
    /// worker stopped by a hook would leave its asker waiting on a result that
    /// no turn will ever produce.
    #[test]
    fn a_halt_fails_the_worker_with_its_reason() {
        let (events, actions) = worker().on_agent_halted(AgentId::new_v4(), "blocked by a hook");
        assert!(actions.is_empty());
        let Event::Failed { error } = only_event(events) else {
            panic!("expected a failure");
        };
        assert_eq!(error, "blocked by a hook");
    }
}

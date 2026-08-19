//! One run of a workflow graph, and the step agents it owns over time.
//!
//! The graph lives here, in the runner's own slice, rather than on the
//! session's spec. That one move is what makes an ad-hoc run expressible — a
//! graph is journal data, so nothing requires it to have a name — and it is
//! what lets a session hold two runs at once. The shape this replaces kept a
//! single `Option<WorkflowRunState>` on the session and inferred a subagent's
//! owning tree from "which step is in flight", an inference with no answer the
//! moment a second run is live.
//!
//! There is deliberately no step runner. Steps are agents *of* this runner:
//! give each step its own runner and the graph state has nowhere to live, and
//! deciding the next step becomes a read of a sibling's slice.
//!
//! A step's ending and the run's ending are different facts, and keeping them
//! apart is the whole reason a run is a runner rather than a subagent.
//! [`AgentLifecycle::on_agent_ended`] concludes a *step*; [`Runner::outcome`]
//! answers only once the *run* is terminal, so an agent that invoked this run
//! is told once, at the end, rather than after every step.
//!
//! # One decision, one ender
//!
//! [`State::decide`] answers the whole question — the next step, the run's end,
//! its failure, or nothing — and both callers read it: [`Runner::actions`]
//! takes the step start, and [`AgentLifecycle::on_agent_ended`] takes the two
//! endings. Splitting "which step runs next" from "is the run over" would give
//! a run two things that could end it, and they would disagree the first time a
//! transition table changed.
//!
//! Ending is decided against the state the step's own conclusion *produces*,
//! folded locally one step early — the same fold the session applies when it
//! persists. Deciding against the state before it would leave a run whose last
//! step routed nowhere sitting at `Running` for ever, holding an output nothing
//! would ever deliver.

use super::action::{Action, FirstInput};
use super::ids::{AgentId, RunnerId};
use super::message::{ChildOutcome, WorkflowOutcome};
use super::{AgentLifecycle, Emit, Runner, RunnerEvent, SessionView, TurnEnd};
use crate::agent_loop::UsageTotal;
use crate::agent_loop::capabilities::ask_user::AskUserCapability;
use crate::agent_loop::capabilities::step_result::StepResultCapability;
use crate::agent_loop::capabilities::{Capabilities, Capability};
use crate::sessions::session_actor::{AgentEntry, AgentStatus};
use crate::sessions::spec::{AgentSettings, SessionStatus};
use crate::sessions::workflow::{
    DEFAULT_MAX_STEPS, OUTCOME_FIELD, StepRun, StepStatus, WorkflowRunSpec, WorkflowRunState,
    WorkflowRunStatus, compose_step_input, next_transition, output_as_input,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

/// The agent id of the execution at `index`.
///
/// Keyed on the **runner**, which is the one change from
/// [`WorkflowRunSpec::step_agent_id`]: that one keys off the session, and two
/// runs hosted by one session each have a step 0, so it hands both the same
/// agent id — one journal, two executions writing into it, and a recovery that
/// cannot tell them apart. The old function keeps its callers; the session
/// actor it serves has exactly one run.
///
/// Derived rather than minted, so deciding a step stays pure. The id is
/// journaled on [`Event::StepStarted`] all the same, so replay reads it back
/// rather than depending on this staying stable for ever.
#[must_use]
pub fn step_agent_id(runner: RunnerId, index: u32) -> AgentId {
    AgentId(Uuid::new_v5(
        &runner.as_uuid(),
        format!("step:{index}").as_bytes(),
    ))
}

/// One run: the graph it was started from, and what has executed so far.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    /// This runner's own id. Held because a step agent's id is derived from it
    /// and [`Runner::actions`] is handed no id — a runner that had to be told
    /// its own name at every boundary would be a field the session could get
    /// wrong.
    pub run: RunnerId,
    /// Snapshotted at creation: a definition or a preset may be edited or
    /// deleted while the run is under way, and step 4 must not change shape
    /// while step 2 is working.
    #[serde(with = "arc_graph")]
    pub graph: Arc<WorkflowRunSpec>,
    /// The append-only execution log. A step reached twice — by a loop or by a
    /// retry — has two entries, which is what keeps the fold pure and the graph
    /// endpoint's read of it lossless.
    pub steps: Vec<StepRun>,
    pub status: WorkflowRunStatus,
    /// The terminal output, once the run has ended.
    pub output: Option<Value>,
    pub error: Option<String>,
    /// This run's own total, across every step.
    pub usage: UsageTotal,
    /// The same tokens, split by the step agent that spent them.
    ///
    /// The one runner that keeps a per-agent breakdown, and it is not
    /// duplication: the graph endpoint renders a token count against each step,
    /// and this is its only source. A run's `usage` is a sum, and a sum cannot
    /// be taken apart again.
    ///
    /// Keyed on the agent rather than the step index because that is what the
    /// event carries and what [`State::index_of_agent`] turns back into an
    /// index — an index written here would be a second copy of a mapping the
    /// log already holds.
    pub step_usage: BTreeMap<AgentId, UsageTotal>,
    /// Whether this run's terminal output has been handed to the agent that
    /// invoked it, for the same reason [`super::subagent::State::reported`]
    /// exists: the invoking agent's own `outstanding` is the acknowledgement,
    /// and this is what stops the session offering an output it has already
    /// handed over at every boundary for ever. A root run — one nobody invoked
    /// — never has an offer to record, so this stays false and reads as one
    /// still owed to a parent that does not exist.
    pub reported: bool,
    pub capabilities: Capabilities,
}

/// `Arc<T>` only implements `Serialize`/`Deserialize` under serde's `rc`
/// feature, which is off workspace-wide — turning it on would change how every
/// `Arc` in the workspace persists, to buy one field a derive. The graph is
/// shared for cheap cloning, not for identity, so writing the inner value and
/// reading a fresh `Arc` back is exactly right.
mod arc_graph {
    use super::WorkflowRunSpec;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::sync::Arc;

    pub fn serialize<S: Serializer>(
        graph: &Arc<WorkflowRunSpec>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        WorkflowRunSpec::serialize(graph, serializer)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Arc<WorkflowRunSpec>, D::Error> {
        WorkflowRunSpec::deserialize(deserializer).map(Arc::new)
    }
}

/// What the graph wants next.
///
/// The shape `WorkflowOrchestrator`'s `AgentAction` had, narrowed to this
/// runner: only a step start is an [`Action`], because finishing and failing
/// are things this run records about itself rather than things it asks the
/// session to do.
enum Next {
    Start(StepStart),
    Finish {
        output: Value,
    },
    Fail {
        error: String,
    },
    /// Something is in flight, or a person has to move it.
    Wait,
}

impl Default for State {
    /// An empty graph, which starts nothing. [`super::RunnerState`] needs a
    /// default arm per kind; a run whose graph nobody filled in must sit still
    /// rather than guess at a start step.
    fn default() -> Self {
        Self {
            run: RunnerId::default(),
            graph: Arc::new(WorkflowRunSpec {
                workflow: String::new(),
                start: String::new(),
                steps: Vec::new(),
                input: String::new(),
                max_steps: DEFAULT_MAX_STEPS,
            }),
            steps: Vec::new(),
            status: WorkflowRunStatus::default(),
            output: None,
            error: None,
            usage: UsageTotal::default(),
            step_usage: BTreeMap::new(),
            reported: false,
            capabilities: Capabilities::default(),
        }
    }
}

/// What this run records about itself.
///
/// The session's step events minus their `at_ms`: a fold has no clock, and the
/// timestamp belongs to the journal entry the session stamps rather than to the
/// decision that produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// One execution of one step began. Appended, never replacing — a loop back
    /// onto a step and a retry of one are both new entries.
    StepStarted {
        index: u32,
        step: String,
        agent: AgentId,
        attempt: u32,
        /// The entry this came out of; `None` for the start step.
        from: Option<u32>,
        /// The transition condition that matched, if any.
        via: Option<String>,
        input: String,
    },
    StepConcluded {
        index: u32,
        output: Value,
    },
    StepFailed {
        index: u32,
        error: String,
    },
    /// A step was cancelled — by an interrupt, or by a retry taking its place.
    /// Suspends the run: a person decides between retrying and abandoning,
    /// because the step's effect on the shared workspace is unknown.
    StepCancelled {
        index: u32,
    },
    Finished {
        output: Value,
    },
    Failed {
        error: String,
    },
    /// This run's output has been offered to the agent that invoked it.
    /// Journaled after the offer — see [`super::subagent::Event::Reported`].
    Reported,
}

/// One step about to begin: everything both the action that starts it and the
/// event that records it need, decided once.
///
/// Two callers, one decision — [`Runner::actions`] asks the session to start
/// the agent and [`AgentLifecycle::on_agent_started`] journals it — so the id
/// the session starts and the id the log holds cannot drift.
#[derive(Debug, Clone)]
struct StepStart {
    index: u32,
    step: String,
    agent: AgentId,
    attempt: u32,
    from: Option<u32>,
    via: Option<String>,
    input: String,
}

impl State {
    /// The execution in flight, if any.
    #[must_use]
    pub fn current(&self) -> Option<u32> {
        self.steps
            .iter()
            .position(|s| s.status == StepStatus::Running)
            .map(|i| i as u32)
    }

    /// The execution one of this run's agents is. A step reached twice is two
    /// executions with two agents, so the agent — not the step's name — is what
    /// picks one out.
    #[must_use]
    fn execution(&self, agent: AgentId) -> Option<&StepRun> {
        self.steps.iter().find(|run| run.agent == agent.as_uuid())
    }

    /// The last execution, which is the one a routing decision is made from.
    #[must_use]
    pub fn last(&self) -> Option<(u32, &StepRun)> {
        self.steps
            .last()
            .map(|s| ((self.steps.len() - 1) as u32, s))
    }

    /// The execution an agent id belongs to. The lookup that turns an agent's
    /// ending into a step's ending; an agent this run never started has none.
    #[must_use]
    pub fn index_of_agent(&self, agent: AgentId) -> Option<u32> {
        self.steps
            .iter()
            .position(|s| s.agent == agent.as_uuid())
            .map(|i| i as u32)
    }

    /// How many times this step has already run, so a new execution numbers
    /// itself.
    #[must_use]
    pub fn attempts_of(&self, step: &str) -> u32 {
        self.steps.iter().filter(|s| s.step == step).count() as u32
    }

    /// What the graph wants started next, or `None` for every reason there is
    /// nothing to start: a step in flight, a run that is over, parked or
    /// suspended, a spent budget, a step that routes nowhere, and a graph that
    /// does not contain the step it names.
    ///
    /// The port of `WorkflowOrchestrator::step_actions`, minus its `Finish` and
    /// `Fail` arms, which have no [`Action`] to be requested through yet.
    fn next_step(&self) -> Option<StepStart> {
        match self.decide() {
            Next::Start(start) => Some(start),
            Next::Finish { .. } | Next::Fail { .. } | Next::Wait => None,
        }
    }

    /// What the graph wants next: a step, the run's end, its failure, or
    /// nothing.
    ///
    /// One decision function with one ender. Splitting "which step runs next"
    /// from "is the run over" would give a run two things that could end it,
    /// and they would disagree the first time a transition table changed.
    fn decide(&self) -> Next {
        // A step in flight, a park, a suspension and a terminal run all mean
        // the same thing here: nothing happens by itself. Only a retry moves a
        // suspended run, and only an answer moves a parked one.
        if self.status.is_terminal()
            || matches!(
                self.status,
                WorkflowRunStatus::Suspended | WorkflowRunStatus::AwaitingInput
            )
            || self.current().is_some()
        {
            return Next::Wait;
        }
        // A loop whose condition never flips would otherwise run for ever. The
        // budget is checked before starting, so the log holds exactly the
        // executions that ran.
        if self.steps.len() as u32 >= self.graph.max_steps {
            return Next::Fail {
                error: format!("step budget exhausted after {} steps", self.graph.max_steps),
            };
        }
        let Some((index, last)) = self.last() else {
            // Nothing has run: begin at the start step.
            if self.graph.step(&self.graph.start).is_none() {
                return Next::Fail {
                    error: format!("start step '{}' is not in this workflow", self.graph.start),
                };
            }
            return Next::Start(self.start(&self.graph.start, None, None, &self.graph.input, None));
        };
        // Only a concluded step decides anything. A failed one has already
        // ended the run's progress; a cancelled one waits for a retry.
        if last.status != StepStatus::Concluded {
            return Next::Wait;
        }
        let output = last.output.clone().unwrap_or(Value::Null);
        let Some(step) = self.graph.step(&last.step) else {
            return Next::Fail {
                error: format!("step '{}' is no longer in this workflow", last.step),
            };
        };
        let outcome = output
            .get(OUTCOME_FIELD)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // No transition matched: this step is terminal, and its result is the
        // run's — which is the run ending, not a step starting.
        let Some((to, via)) = next_transition(&step.transitions, &outcome) else {
            return Next::Finish { output };
        };
        if self.graph.step(&to).is_none() {
            return Next::Fail {
                error: format!(
                    "step '{}' transitions to '{to}', which is not in this workflow",
                    last.step
                ),
            };
        }
        Next::Start(self.start(
            &to,
            Some(index),
            via,
            &output_as_input(&output),
            Some(&last.step),
        ))
    }

    /// The execution that starts `step_name`, coming out of `from`.
    fn start(
        &self,
        step_name: &str,
        from: Option<u32>,
        via: Option<String>,
        incoming: &str,
        from_step: Option<&str>,
    ) -> StepStart {
        let index = self.steps.len() as u32;
        let prompt = self.graph.step(step_name).map_or("", |s| s.prompt.as_str());
        StepStart {
            index,
            step: step_name.to_string(),
            agent: step_agent_id(self.run, index),
            attempt: self.attempts_of(step_name) + 1,
            from,
            via,
            input: compose_step_input(prompt, from_step, incoming),
        }
    }
}

impl State {
    /// The action that starts one execution, equipped as that step declared.
    ///
    /// Shared by [`Runner::actions`] and [`State::retry`], because a retried
    /// step is an ordinary execution of the same step — equipped from the same
    /// declaration, on the same preset. Two copies of this would be two answers
    /// to "what is a step agent equipped with", and the retry's would be the one
    /// nobody edits.
    ///
    /// `None` when the step has gone from the graph, which is the same reason
    /// the run fails rather than starting an agent with no definition.
    fn start_action(&self, next: &StepStart) -> Option<Action> {
        // The step's own settings, not the run's: a graph resolves one preset
        // per step at creation, which is what lets step 1 run on a large model
        // and step 2 on a small one.
        let step = self.graph.step(&next.step)?;
        // A fresh copy for the step agent's task to equip itself from; the
        // folded one stays here.
        //
        // The per-*agent* capabilities join the copy rather than
        // `capabilities`: what a step promises to return, and whether it may
        // ask, are declared by that step, so step 1 can be interactive and step
        // 2 not. Neither carries state between steps — a submitted result is
        // this runner's own `StepConcluded` to fold.
        //
        let mut equipment = self.capabilities.clone();
        equipment.push(Capability::StepResult(StepResultCapability::new(
            step.outcomes.clone(),
            step.fields.clone(),
            step.interactive,
        )));
        // Equipped either way, and the flag is the whole difference: a step
        // that may not ask still needs somebody to answer for `ask_user`, or
        // the call falls through to the sandbox and the model is never told no.
        equipment.push(Capability::AskUser(match step.interactive {
            true => AskUserCapability::new(),
            false => AskUserCapability::not_interactive(),
        }));
        Some(Action::StartAgent {
            agent: next.agent,
            equipment,
            settings: Box::new(step.settings.clone()),
            first: FirstInput::Text(next.input.clone()),
        })
    }

    /// Re-run one execution from the log, because a person asked.
    ///
    /// **Not an [`AgentLifecycle`] method, and not reachable from one.** A retry
    /// comes from a person looking at a stalled run, never from an agent, so it
    /// is a plain function the session calls rather than a case every runner has
    /// to carry an arm for.
    ///
    /// Appends rather than truncating: earlier attempts stay readable, and the
    /// graph renders them stacked on their node. A run still in flight has its
    /// current step cancelled first — the run's workspace is shared, so two
    /// steps must never be writing to it at once. The workspace itself is *not*
    /// rolled back: a retried step re-runs against whatever the previous attempt
    /// left on disk, which is the honest behaviour and what the guide promises.
    ///
    /// The new execution sits on the *original's* edge — same `from`, same
    /// `via`, same input — so the graph draws it stacked on the node it retries
    /// rather than inventing a transition nothing took.
    ///
    /// `Err` is a request that names nothing, which is a person's mistake and
    /// belongs in the reply rather than in the log.
    pub fn retry(&self, index: u32) -> Result<Emit, String> {
        let target = self
            .steps
            .get(index as usize)
            .ok_or_else(|| format!("no step execution at index {index}"))?
            .clone();
        let mut emit = Emit::nothing();
        // Cancel whatever is in flight first, so the retry is the only writer of
        // the shared workspace. Both halves: the agent's run has to stop, and
        // the log entry has to say so — without the entry `current()` never
        // clears and the run starts nothing ever again.
        if let Some(current) = self.current() {
            if let Some(running) = self.steps.get(current as usize) {
                emit.actions.push(Action::Cancel {
                    agent: AgentId(running.agent),
                });
            }
            emit.events
                .push(RunnerEvent::Workflow(Event::StepCancelled {
                    index: current,
                }));
        }
        // Folded locally one step early — the same fold the session applies when
        // it persists — because the new execution's index and attempt are counts
        // over the log *including* the cancel above.
        let mut next = self.clone();
        for event in &emit.events {
            // Zero, and never persisted: this copy exists only to number the
            // retry, and the time the real fold stamps is the session's.
            next.apply(event, 0);
        }
        let start = StepStart {
            index: next.steps.len() as u32,
            step: target.step.clone(),
            agent: step_agent_id(self.run, next.steps.len() as u32),
            attempt: next.attempts_of(&target.step) + 1,
            from: target.from,
            via: target.via.clone(),
            input: target.input.clone(),
        };
        let action = self
            .start_action(&start)
            .ok_or_else(|| format!("step '{}' is no longer in this workflow", target.step))?;
        emit.events.push(RunnerEvent::Workflow(Event::StepStarted {
            index: start.index,
            step: start.step,
            agent: start.agent,
            attempt: start.attempt,
            from: start.from,
            via: start.via,
            input: start.input,
        }));
        emit.actions.push(action);
        Ok(emit)
    }
}

impl Runner for State {
    fn actions(&self, _view: &SessionView) -> Vec<Action> {
        let Some(next) = self.next_step() else {
            return Vec::new();
        };
        self.start_action(&next).into_iter().collect()
    }

    /// The **run's** ending, never a step's. A step concluding is an input to
    /// the graph; only the run owes anything to whoever invoked it.
    fn outcome(&self) -> Option<ChildOutcome> {
        if self.reported {
            return None;
        }
        match self.status {
            WorkflowRunStatus::Finished => {
                Some(ChildOutcome::Workflow(WorkflowOutcome::Finished {
                    output: self.output.clone().unwrap_or(Value::Null),
                }))
            }
            WorkflowRunStatus::Failed => Some(ChildOutcome::Workflow(WorkflowOutcome::Failed {
                error: self
                    .error
                    .clone()
                    .unwrap_or_else(|| "the run failed".to_string()),
            })),
            WorkflowRunStatus::Pending
            | WorkflowRunStatus::Running
            | WorkflowRunStatus::Suspended
            | WorkflowRunStatus::AwaitingInput => None,
        }
    }

    fn busy(&self) -> bool {
        self.current().is_some()
    }

    /// The **run's** status, and only when the run is over.
    ///
    /// `Suspended` is not an ending: a suspended run is one a person can still
    /// retry a step of, and marking it terminal would take the retry away. The
    /// two arms here are exactly [`WorkflowRunStatus::is_terminal`]'s, read
    /// through the same field [`Runner::outcome`] reads, so a run cannot be
    /// finished for the session and unfinished for the agent that invoked it.
    fn finished(&self) -> Option<super::RunnerStatus> {
        match self.status {
            WorkflowRunStatus::Finished => Some(super::RunnerStatus::Done),
            WorkflowRunStatus::Failed => Some(super::RunnerStatus::Failed),
            WorkflowRunStatus::Pending
            | WorkflowRunStatus::Running
            | WorkflowRunStatus::Suspended
            | WorkflowRunStatus::AwaitingInput => None,
        }
    }

    fn capabilities(&self) -> Option<&Capabilities> {
        Some(&self.capabilities)
    }

    /// One agent per *execution*, not one per step: a step reached twice — by a
    /// loop or by a retry — is two agents with two transcripts, and collapsing
    /// them would lose one.
    ///
    /// The one kind that stamps its own agents. A step comes and goes inside
    /// one runner's life, so the record's stamps are the run's and not the
    /// step's, and a step that left them to the read side would report the
    /// moment the run began.
    fn rows(&self) -> Vec<AgentEntry> {
        self.steps
            .iter()
            .map(|run| AgentEntry {
                id: AgentId(run.agent).to_string(),
                // Where a step sits is the session's fact about it, not the
                // run's.
                parent: None,
                depth: 0,
                label: Some(run.step.clone()),
                agent_type: None,
                status: match run.status {
                    StepStatus::Running => AgentStatus::Running,
                    StepStatus::Concluded => AgentStatus::Completed,
                    StepStatus::Failed => AgentStatus::Failed,
                    StepStatus::Cancelled => AgentStatus::Cancelled,
                },
                error: run.error.clone(),
                started_at_ms: run.started_at_ms,
                ended_at_ms: run.ended_at_ms.unwrap_or(0),
            })
            .collect()
    }

    fn standing(&self) -> Option<SessionStatus> {
        Some(match self.status {
            WorkflowRunStatus::Pending => SessionStatus::Idle,
            WorkflowRunStatus::Running => SessionStatus::Running,
            WorkflowRunStatus::AwaitingInput => SessionStatus::AwaitingInput,
            // A person decides between retrying and abandoning, so the run
            // rests. Not a failure: nothing broke, and a retry moves it.
            WorkflowRunStatus::Suspended => SessionStatus::Idle,
            // Not `Idle`: a run that ran to completion and one that stopped
            // part-way both rest, and telling them apart is the whole reason to
            // look at a list of past runs.
            WorkflowRunStatus::Finished => SessionStatus::Finished,
            WorkflowRunStatus::Failed => SessionStatus::Failed {
                reason: self
                    .error
                    .clone()
                    .unwrap_or_else(|| "the run failed".to_string()),
            },
        })
    }

    /// At most one step runs at a time and the definition chose it, so there is
    /// nothing else an unaddressed request on a run could mean. `None` between
    /// steps, which is when there is nothing to address.
    fn primary_agent(&self) -> Option<AgentId> {
        self.current()
            .and_then(|i| self.steps.get(i as usize))
            .map(|run| AgentId(run.agent))
    }

    fn run_log(&self) -> Option<WorkflowRunState> {
        Some(WorkflowRunState {
            status: self.status,
            steps: self.steps.clone(),
            output: self.output.clone(),
            error: self.error.clone(),
        })
    }

    fn run_graph(&self) -> Option<Arc<WorkflowRunSpec>> {
        Some(Arc::clone(&self.graph))
    }

    /// The step's own preset. A graph resolves one per step at creation, which
    /// is what lets step 1 run on a large model and step 2 on a small one — and
    /// reading the session's instead is what had every step's document report
    /// the first step's model.
    ///
    /// `None` only when a step's definition has gone from the graph it was
    /// snapshotted into, which nothing can produce.
    fn settings(&self, agent: AgentId) -> Option<&AgentSettings> {
        let run = self.execution(agent)?;
        self.graph.step(&run.step).map(|step| &step.settings)
    }

    /// A step's brief is its definition's, not something an agent handed it, so
    /// only the output half answers — rendered the way a worker's report is,
    /// because a reader wants the same thing from both.
    fn task_and_output(&self, agent: AgentId) -> (Option<String>, Option<String>) {
        let output = self
            .execution(agent)
            .and_then(|run| run.output.as_ref())
            .map(output_as_input);
        (None, output)
    }

    /// One total per step agent, which is what the graph endpoint renders. A
    /// step that has spent nothing is still listed: it is an agent of this run,
    /// and leaving it out would read as an agent the session does not have.
    fn usage(&self) -> Vec<(AgentId, UsageTotal)> {
        self.steps
            .iter()
            .map(|run| {
                let agent = AgentId(run.agent);
                (
                    agent,
                    self.step_usage.get(&agent).copied().unwrap_or_default(),
                )
            })
            .collect()
    }

    fn apply(&mut self, event: &RunnerEvent, at_ms: u64) {
        // Banked twice, against the run and against the step agent that spent
        // it. The run's total is what a session-level read wants; the split is
        // what the graph endpoint renders per step, and a sum cannot be taken
        // apart again.
        if let RunnerEvent::Usage { agent, spent, .. } = event {
            self.usage = self.usage.combine(spent);
            let entry = self.step_usage.entry(*agent).or_default();
            *entry = entry.combine(spent);
            return;
        }
        let RunnerEvent::Workflow(event) = event else {
            return;
        };
        match event {
            Event::StepStarted {
                // Where the entry lands is where the append puts it. The field
                // is what a later entry's `from` points at, and what a reader
                // of the log needs so it keeps no running count.
                index: _,
                step,
                agent,
                attempt,
                from,
                via,
                input,
            } => {
                self.steps.push(StepRun {
                    step: step.clone(),
                    agent: agent.as_uuid(),
                    attempt: *attempt,
                    from: *from,
                    via: via.clone(),
                    status: StepStatus::Running,
                    input: input.clone(),
                    output: None,
                    error: None,
                    // A fold has no clock, so the time arrives with the event:
                    // it is when the session journaled the entry, which is the
                    // one reading a replay lands on too.
                    started_at_ms: at_ms,
                    ended_at_ms: None,
                });
                self.status = WorkflowRunStatus::Running;
            }
            Event::StepConcluded { index, output } => {
                if let Some(s) = self.steps.get_mut(*index as usize) {
                    s.status = StepStatus::Concluded;
                    s.output = Some(output.clone());
                    s.ended_at_ms = Some(at_ms);
                }
                // Reasserted, because of the one path that leaves the status
                // something else: a step parked on a question set
                // `AwaitingInput`, and nothing else ever cleared it — so
                // answering resumed the step and then stalled the run at the
                // very step it had just finished.
                self.status = WorkflowRunStatus::Running;
            }
            Event::StepFailed { index, error } => {
                if let Some(s) = self.steps.get_mut(*index as usize) {
                    s.status = StepStatus::Failed;
                    s.error = Some(error.clone());
                    s.ended_at_ms = Some(at_ms);
                }
            }
            Event::StepCancelled { index } => {
                if let Some(s) = self.steps.get_mut(*index as usize) {
                    s.status = StepStatus::Cancelled;
                    s.ended_at_ms = Some(at_ms);
                }
                self.status = WorkflowRunStatus::Suspended;
            }
            Event::Finished { output } => {
                self.status = WorkflowRunStatus::Finished;
                self.output = Some(output.clone());
            }
            Event::Failed { error } => {
                self.status = WorkflowRunStatus::Failed;
                self.error = Some(error.clone());
            }
            Event::Reported => self.reported = true,
        }
    }
}

impl AgentLifecycle for State {
    /// The step's own record, written when its agent exists.
    ///
    /// Re-derives the same decision [`Runner::actions`] made rather than
    /// carrying it across: the derivation is pure, so the two agree by
    /// construction, and a `StepStarted` for an agent this run did not ask for
    /// cannot be written at all.
    fn on_agent_started(&self, agent: AgentId) -> Emit {
        let Some(next) = self.next_step() else {
            return Emit::nothing();
        };
        if next.agent != agent {
            return Emit::nothing();
        }
        Emit::record(vec![RunnerEvent::Workflow(Event::StepStarted {
            index: next.index,
            step: next.step,
            agent: next.agent,
            attempt: next.attempt,
            from: next.from,
            via: next.via,
            input: next.input,
        })])
    }

    /// A turn ending is a *step* ending, and only for the endings that are one.
    fn on_agent_ended(&self, agent: AgentId, end: &TurnEnd) -> Emit {
        // An agent this run never started belongs to somebody else.
        let Some(index) = self.index_of_agent(agent) else {
            return Emit::nothing();
        };
        let events = match end {
            TurnEnd::Concluded { output } => {
                let concluded = RunnerEvent::Workflow(Event::StepConcluded {
                    index,
                    output: output.clone(),
                });
                // The fold is local and one step early — the same fold the
                // session applies when it persists — because "is this run
                // over" is a question about the state this event *produces*,
                // not the one it was decided against. Without it a run whose
                // last step routed nowhere would sit at `Running` for ever,
                // with an output nothing ever delivered.
                let mut next = self.clone();
                // Zero, and it is never persisted: this copy exists only to ask
                // "is the run over", and the time the real fold stamps is the
                // one the session hands it when it writes the entry.
                next.apply(&concluded, 0);
                match next.decide() {
                    Next::Finish { output } => vec![
                        concluded,
                        RunnerEvent::Workflow(Event::Finished { output }),
                    ],
                    Next::Fail { error } => {
                        vec![concluded, RunnerEvent::Workflow(Event::Failed { error })]
                    }
                    Next::Start(_) | Next::Wait => vec![concluded],
                }
            }
            TurnEnd::Failed { error, terminal: _ } => {
                vec![RunnerEvent::Workflow(Event::StepFailed {
                    index,
                    error: error.clone(),
                })]
            }
            // The step is still running, parked on its question; the answer
            // comes back through the agent that asked and the turn resumes.
            TurnEnd::Asked
            // A step waiting on a timer or on subagents has not ended either.
            | TurnEnd::Parked
            // The session suspends an interrupted step at load. Recording it
            // here as well would append a second entry for one execution.
            | TurnEnd::Interrupted => Vec::new(),
        };
        Emit::record(events)
    }

    /// A halted step is cancelled, which suspends the run: what it did to the
    /// shared workspace before it stopped is unknown, so a person decides
    /// between retrying and abandoning.
    fn on_agent_halted(&self, agent: AgentId, _reason: &str) -> Emit {
        let Some(index) = self.index_of_agent(agent) else {
            return Emit::nothing();
        };
        Emit::record(vec![RunnerEvent::Workflow(Event::StepCancelled { index })])
    }

    /// Cancelling the step, which suspends the run.
    ///
    /// Cancelling the *agent* is not enough on a run: without a step event the
    /// log entry stays `Running` for ever, so [`State::current`] never clears
    /// and nothing starts again — the run wedged while its page read
    /// "Running". `StepCancelled` suspends it, which is the state a retry can
    /// move.
    ///
    /// Only while that agent's step is the one in flight. A stop arriving
    /// after the step already ended — or for a step some later execution has
    /// superseded — would cancel an entry the run has already routed past, and
    /// suspend a run that is happily working on the next step.
    fn on_agent_stopped(&self, agent: AgentId) -> Emit {
        let Some(index) = self.index_of_agent(agent) else {
            return Emit::nothing();
        };
        if self.current() != Some(index) {
            return Emit::nothing();
        }
        Emit::record(vec![RunnerEvent::Workflow(Event::StepCancelled { index })])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_loop::capabilities::testing::{advertised, facts, settings};
    use crate::sessions::workflow::{TransitionSpec, WorkflowStepSpec};
    use horsie_models::workflow::{OutcomeFilter, OutcomeIn};

    fn step(name: &str, transitions: Vec<TransitionSpec>) -> WorkflowStepSpec {
        WorkflowStepSpec {
            name: name.into(),
            agent: "a".into(),
            prompt: format!("Do {name}."),
            outcomes: crate::sessions::workflow::default_outcomes(),
            fields: Vec::new(),
            interactive: false,
            transitions,
            settings: settings(),
        }
    }

    /// A transition taken for any of `values`, or a catch-all when empty.
    fn to(target: &str, values: &[&str]) -> TransitionSpec {
        TransitionSpec {
            to: target.into(),
            when: (!values.is_empty()).then(|| {
                OutcomeFilter::In(OutcomeIn {
                    values: values.iter().map(|v| (*v).to_string()).collect(),
                })
            }),
        }
    }

    /// triage --p0--> fix, triage --else--> file.
    fn graph() -> WorkflowRunSpec {
        WorkflowRunSpec {
            workflow: "fix-bug".into(),
            start: "triage".into(),
            steps: vec![
                step("triage", vec![to("fix", &["p0"]), to("file", &[])]),
                step("fix", vec![]),
                step("file", vec![]),
            ],
            input: "the build is red".into(),
            max_steps: 100,
        }
    }

    fn run_of(graph: WorkflowRunSpec) -> State {
        State {
            run: RunnerId::new_v4(),
            graph: Arc::new(graph),
            ..State::default()
        }
    }

    fn run() -> State {
        run_of(graph())
    }

    fn view() -> SessionView {
        SessionView {
            runtime_ready: true,
            depth: 0,
            active_agents: 0,
        }
    }

    /// The agent the runner asks the session to start.
    fn started_agent(state: &State) -> AgentId {
        let actions = state.actions(&view());
        assert_eq!(actions.len(), 1, "expected one start, got {actions:?}");
        let Action::StartAgent { agent, .. } = &actions[0] else {
            panic!("expected a start, got {:?}", actions[0]);
        };
        *agent
    }

    /// Drive one step: start its agent, journal the start, then conclude it
    /// with `output`. The fold is the only writer, so a test moves the run
    /// exactly as the session does.
    fn advance(state: &mut State, output: Value) -> AgentId {
        let agent = started_agent(state);
        let Emit { events, .. } = state.on_agent_started(agent);
        for e in &events {
            state.apply(e, 0);
        }
        let Emit { events, .. } = state.on_agent_ended(agent, &TurnEnd::Concluded { output });
        for e in &events {
            state.apply(e, 0);
        }
        agent
    }

    /// A step's two timestamps come off the entries that recorded it, because a
    /// fold may not read a clock. Every ending stamps one: without it a
    /// cancelled or failed step reads as still running to anything that shows a
    /// duration.
    #[test]
    fn a_step_is_stamped_from_the_entries_that_started_and_ended_it() {
        for (ending, expected) in [
            (
                Event::StepConcluded {
                    index: 0,
                    output: Value::Null,
                },
                StepStatus::Concluded,
            ),
            (
                Event::StepFailed {
                    index: 0,
                    error: "provider 500".into(),
                },
                StepStatus::Failed,
            ),
            (Event::StepCancelled { index: 0 }, StepStatus::Cancelled),
        ] {
            let mut state = run();
            let agent = started_agent(&state);
            let Emit { events, .. } = state.on_agent_started(agent);
            for e in &events {
                state.apply(e, 100);
            }
            assert_eq!(state.steps[0].started_at_ms, 100);
            assert_eq!(state.steps[0].ended_at_ms, None, "it is still running");

            state.apply(&RunnerEvent::Workflow(ending), 250);
            assert_eq!(state.steps[0].status, expected);
            assert_eq!(state.steps[0].ended_at_ms, Some(250));
        }
    }

    /// A run begins at the start step with the run's own input, and the agent
    /// it asks for is the one derived for index 0. If the derivation and the
    /// action ever disagree, the session starts an agent whose journal the
    /// runner will never find again.
    #[test]
    fn a_fresh_run_starts_the_start_step_with_the_run_input() {
        let state = run();
        let actions = state.actions(&view());
        let [Action::StartAgent { agent, first, .. }] = actions.as_slice() else {
            panic!("expected one start, got {actions:?}");
        };
        assert_eq!(*agent, step_agent_id(state.run, 0));
        let FirstInput::Text(input) = first else {
            panic!("a step is always handed its composed input");
        };
        assert_eq!(input, "Do triage.\n\n## Input\nthe build is red");
    }

    /// The graph came from the runner's own state, built inline: no name, no
    /// definition row, no `SessionSpec`. This is the whole of what makes an
    /// ad-hoc workflow work, so it is a test rather than a claim in a doc.
    #[test]
    fn an_ad_hoc_graph_held_by_the_runner_starts_its_first_step() {
        let state = run_of(WorkflowRunSpec {
            workflow: String::new(),
            start: "only".into(),
            steps: vec![step("only", vec![])],
            input: "do the thing".into(),
            max_steps: 10,
        });
        let actions = state.actions(&view());
        let [Action::StartAgent { first, .. }] = actions.as_slice() else {
            panic!("expected one start, got {actions:?}");
        };
        let FirstInput::Text(input) = first else {
            panic!("a step is always handed its composed input");
        };
        assert_eq!(input, "Do only.\n\n## Input\ndo the thing");
    }

    /// Keyed on the runner, not the session. Two runs in one session both have
    /// a step 0; keyed on the session they would share an agent id, and two
    /// executions would write into one journal.
    #[test]
    fn step_agent_ids_are_keyed_on_the_runner() {
        let one = run();
        let two = run();
        assert_ne!(one.run, two.run);
        assert_ne!(started_agent(&one), started_agent(&two));
        // And stable for a given runner and index, which is what lets the id be
        // derived rather than minted.
        assert_eq!(step_agent_id(one.run, 0), step_agent_id(one.run, 0));
        assert_ne!(step_agent_id(one.run, 0), step_agent_id(one.run, 1));
    }

    /// Two steps never run at once. A second start while one is in flight would
    /// hand the shared workspace to two agents with no ordering between them.
    #[test]
    fn nothing_starts_while_a_step_is_in_flight() {
        let mut state = run();
        let agent = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(agent);
        for e in &events {
            state.apply(e, 0);
        }
        assert!(state.actions(&view()).is_empty());
        assert!(
            state.busy(),
            "a step in flight is work the session must keep"
        );
    }

    /// A suspended run waits for a person and a parked one waits for an answer:
    /// an interrupted step's effect on the workspace is unknown, so nothing
    /// resumes it by itself.
    #[test]
    fn a_terminal_suspended_or_parked_run_starts_nothing() {
        for status in [
            WorkflowRunStatus::Suspended,
            WorkflowRunStatus::AwaitingInput,
            WorkflowRunStatus::Finished,
            WorkflowRunStatus::Failed,
        ] {
            let state = State { status, ..run() };
            assert!(
                state.actions(&view()).is_empty(),
                "{status:?} must start nothing"
            );
        }
    }

    /// Nothing else bounds a graph with a loop.
    #[test]
    fn a_spent_step_budget_starts_nothing() {
        let mut state = run_of(WorkflowRunSpec {
            workflow: "w".into(),
            start: "a".into(),
            steps: vec![step("a", vec![to("a", &[])])],
            input: "x".into(),
            max_steps: 2,
        });
        advance(&mut state, serde_json::json!({}));
        advance(&mut state, serde_json::json!({}));
        assert_eq!(state.steps.len(), 2);
        assert!(
            state.actions(&view()).is_empty(),
            "the budget is checked before starting, so the log holds exactly \
             the executions that ran"
        );
    }

    /// The concluded step's `outcome` picks the branch, its output becomes the
    /// next step's input, and the entry records which condition matched — the
    /// three things that make the log a graph rather than a list.
    #[test]
    fn a_matching_condition_routes_with_the_previous_output_as_input() {
        let mut state = run();
        advance(&mut state, serde_json::json!({"outcome": "p0"}));
        let actions = state.actions(&view());
        let [Action::StartAgent { agent, first, .. }] = actions.as_slice() else {
            panic!("expected one start, got {actions:?}");
        };
        assert_eq!(*agent, step_agent_id(state.run, 1));
        let FirstInput::Text(input) = first else {
            panic!("a step is always handed its composed input");
        };
        assert!(
            input.starts_with("Do fix.\n\n## Input from step `triage`\n"),
            "{input}"
        );

        let Emit { events, .. } = state.on_agent_started(*agent);
        for e in &events {
            state.apply(e, 0);
        }
        assert_eq!(state.steps[1].step, "fix");
        assert_eq!(state.steps[1].from, Some(0));
        assert_eq!(state.steps[1].via.as_deref(), Some("outcome in [p0]"));
        assert_eq!(state.steps[1].attempt, 1);
    }

    /// A step concluding is not the run concluding. If `outcome` ever answered
    /// per step, the agent that invoked the run would be told it had finished
    /// after step 1 — which is the difference between a run and a subagent.
    #[test]
    fn outcome_is_none_per_step_and_the_runs_only_once_it_is_terminal() {
        let mut state = run();
        advance(&mut state, serde_json::json!({"outcome": "p0"}));
        assert!(
            state.outcome().is_none(),
            "a concluded step owes the invoker nothing"
        );

        state.apply(
            &RunnerEvent::Workflow(Event::Finished {
                output: serde_json::json!({"shipped": true}),
            }),
            0,
        );
        let Some(ChildOutcome::Workflow(WorkflowOutcome::Finished { output })) = state.outcome()
        else {
            panic!("a finished run reports its terminal output");
        };
        assert_eq!(output, serde_json::json!({"shipped": true}));
    }

    /// A run that died is still an answer. An agent blocked on one that failed
    /// and was never told would wait for ever.
    #[test]
    fn a_failed_run_reports_its_error() {
        let mut state = run();
        state.apply(
            &RunnerEvent::Workflow(Event::Failed {
                error: "step budget exhausted".into(),
            }),
            0,
        );
        let Some(ChildOutcome::Workflow(WorkflowOutcome::Failed { error })) = state.outcome()
        else {
            panic!("a failed run reports why");
        };
        assert_eq!(error, "step budget exhausted");
    }

    /// `busy` is what stops the session unloading mid-step. Between steps there
    /// is nothing to protect.
    #[test]
    fn busy_only_while_a_step_is_in_flight() {
        let mut state = run();
        assert!(!state.busy());
        let agent = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(agent);
        for e in &events {
            state.apply(e, 0);
        }
        assert!(state.busy());
        let Emit { events, .. } = state.on_agent_ended(
            agent,
            &TurnEnd::Concluded {
                output: serde_json::json!({}),
            },
        );
        for e in &events {
            state.apply(e, 0);
        }
        assert!(!state.busy());
    }

    /// Starting the agent writes the step's record, and it must name the agent
    /// the action asked for: those two ids are what tie a step to its journal.
    #[test]
    fn starting_the_agent_journals_the_step_it_was_started_for() {
        let mut state = run();
        let agent = started_agent(&state);
        let Emit { events, actions } = state.on_agent_started(agent);
        assert!(actions.is_empty(), "recording a start asks for nothing");
        let [
            RunnerEvent::Workflow(Event::StepStarted {
                index,
                step,
                agent: recorded,
                attempt,
                from,
                via,
                input,
            }),
        ] = events.as_slice()
        else {
            panic!("expected one StepStarted, got {events:?}");
        };
        assert_eq!(*index, 0);
        assert_eq!(step, "triage");
        assert_eq!(*recorded, agent);
        assert_eq!(*attempt, 1);
        assert_eq!(*from, None);
        assert!(via.is_none(), "the start step comes out of no condition");
        assert!(input.contains("the build is red"));

        for e in &events {
            state.apply(e, 0);
        }
        assert_eq!(state.status, WorkflowRunStatus::Running);
        assert_eq!(state.index_of_agent(agent), Some(0));
    }

    /// The ending of the *second* step's agent must conclude the second step.
    /// An index taken from anything but the agent id would silently overwrite
    /// step 1's result on every loop.
    #[test]
    fn a_concluded_agent_ends_its_own_step() {
        let mut state = run();
        advance(&mut state, serde_json::json!({"outcome": "p0"}));
        let second = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(second);
        for e in &events {
            state.apply(e, 0);
        }
        let Emit { events, .. } = state.on_agent_ended(
            second,
            &TurnEnd::Concluded {
                output: serde_json::json!({"outcome": "done"}),
            },
        );
        // Two events, not one: step 2 routes nowhere, so its conclusion is
        // also the run's. They arrive together because the ending is decided
        // against the state the conclusion produces.
        let [
            RunnerEvent::Workflow(Event::StepConcluded { index, output }),
            RunnerEvent::Workflow(Event::Finished { output: run_output }),
        ] = events.as_slice()
        else {
            panic!("expected a step conclusion and the run's end, got {events:?}");
        };
        assert_eq!(*index, 1);
        assert_eq!(*output, serde_json::json!({"outcome": "done"}));
        assert_eq!(run_output, output, "the run's output is the last step's");
    }

    /// A run whose last step routes nowhere must end, and end *here*. Before
    /// the ending had a producer it sat at `Running` for ever, holding an
    /// output nothing would ever deliver to the agent that invoked it.
    #[test]
    fn a_step_that_routes_nowhere_finishes_the_run() {
        let mut state = run();
        advance(&mut state, serde_json::json!({"outcome": "p0"}));
        let second = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(second);
        for e in &events {
            state.apply(e, 0);
        }
        let Emit { events, .. } = state.on_agent_ended(
            second,
            &TurnEnd::Concluded {
                output: serde_json::json!({"outcome": "done"}),
            },
        );
        for e in &events {
            state.apply(e, 0);
        }
        assert_eq!(state.status, WorkflowRunStatus::Finished);
        let Some(ChildOutcome::Workflow(WorkflowOutcome::Finished { .. })) = state.outcome() else {
            panic!(
                "a finished run owes its invoker an outcome, got {:?}",
                state.outcome()
            );
        };
    }

    /// A loop whose condition never flips fails the run rather than running for
    /// ever, and the failure names the budget so a reader knows it was a cap
    /// and not a crash.
    #[test]
    fn a_spent_step_budget_fails_the_run() {
        let mut state = run();
        state.graph = Arc::new(WorkflowRunSpec {
            max_steps: 1,
            ..(*state.graph).clone()
        });
        let agent = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(agent);
        for e in &events {
            state.apply(e, 0);
        }
        let Emit { events, .. } = state.on_agent_ended(
            agent,
            &TurnEnd::Concluded {
                output: serde_json::json!({"outcome": "p0"}),
            },
        );
        for e in &events {
            state.apply(e, 0);
        }
        assert_eq!(state.status, WorkflowRunStatus::Failed);
        assert!(
            state
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("budget"),
            "the failure has to name the cap: {:?}",
            state.error
        );
    }

    /// A failed turn fails its step, and the run then starts nothing: only a
    /// concluded step routes.
    #[test]
    fn a_failed_turn_fails_the_step_and_stops_the_run_advancing() {
        let mut state = run();
        let agent = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(agent);
        for e in &events {
            state.apply(e, 0);
        }
        let Emit { events, .. } = state.on_agent_ended(
            agent,
            &TurnEnd::Failed {
                error: "provider 500".into(),
                terminal: false,
            },
        );
        let [RunnerEvent::Workflow(Event::StepFailed { index, error })] = events.as_slice() else {
            panic!("expected one StepFailed, got {events:?}");
        };
        assert_eq!(*index, 0);
        assert_eq!(error, "provider 500");
        for e in &events {
            state.apply(e, 0);
        }
        assert!(state.actions(&view()).is_empty());
    }

    /// The three endings that are not a step ending. A park or an ask that
    /// wrote a step event would end an execution that is still going; an
    /// interrupt would append a second entry for one execution, because the
    /// session already suspends an interrupted step at load.
    #[test]
    fn an_ask_a_park_and_an_interrupt_end_no_step() {
        let mut state = run();
        let agent = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(agent);
        for e in &events {
            state.apply(e, 0);
        }
        for end in [TurnEnd::Asked, TurnEnd::Parked, TurnEnd::Interrupted] {
            let Emit { events, actions } = state.on_agent_ended(agent, &end);
            assert!(events.is_empty(), "{end:?} must record no step event");
            assert!(actions.is_empty(), "{end:?} must ask for nothing");
        }
    }

    /// An agent belonging to some other runner yields nothing. The match is on
    /// the id rather than on "whatever ran last", so a stray ending cannot
    /// conclude a step it has nothing to do with.
    #[test]
    fn an_agent_this_run_never_started_yields_no_events() {
        let mut state = run();
        let agent = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(agent);
        for e in &events {
            state.apply(e, 0);
        }
        let stranger = AgentId::new_v4();
        let Emit { events, .. } = state.on_agent_ended(
            stranger,
            &TurnEnd::Concluded {
                output: serde_json::json!({}),
            },
        );
        assert!(events.is_empty());
        let Emit { events, .. } = state.on_agent_halted(stranger, "stopped");
        assert!(events.is_empty());
    }

    /// Halting cancels the step and suspends the run, so nothing restarts by
    /// itself: what the step did to the shared workspace before it stopped is
    /// unknown.
    #[test]
    fn halting_a_step_cancels_it_and_suspends_the_run() {
        let mut state = run();
        let agent = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(agent);
        for e in &events {
            state.apply(e, 0);
        }
        let Emit { events, actions } = state.on_agent_halted(agent, "the person stopped it");
        assert!(actions.is_empty());
        let [RunnerEvent::Workflow(Event::StepCancelled { index })] = events.as_slice() else {
            panic!("expected one StepCancelled, got {events:?}");
        };
        assert_eq!(*index, 0);
        for e in &events {
            state.apply(e, 0);
        }
        assert_eq!(state.status, WorkflowRunStatus::Suspended);
        assert!(!state.busy());
        assert!(state.actions(&view()).is_empty());
    }

    /// Stopping cancels the step in flight and suspends the run, so nothing
    /// restarts by itself — and cancels *only* that step. A stop arriving after
    /// the step ended, or naming an execution a later one superseded, would
    /// cancel an entry the run has already routed past and suspend a run that
    /// is happily working on the next step.
    #[test]
    fn stopping_cancels_only_the_step_in_flight() {
        let mut state = run();
        let first = advance(&mut state, serde_json::json!({"outcome": "p0"}));
        // Step 0 has concluded and step 1 has not started: nothing is in
        // flight, so there is nothing a stop can cancel.
        assert!(state.on_agent_stopped(first).events.is_empty());
        assert!(
            state.on_agent_stopped(AgentId::new_v4()).events.is_empty(),
            "an agent this run never started is somebody else's"
        );

        let second = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(second);
        for e in &events {
            state.apply(e, 0);
        }
        // The superseded step is still not cancellable; only the live one is.
        assert!(state.on_agent_stopped(first).events.is_empty());
        let Emit { events, actions } = state.on_agent_stopped(second);
        assert!(actions.is_empty());
        let [RunnerEvent::Workflow(Event::StepCancelled { index })] = events.as_slice() else {
            panic!("expected one StepCancelled, got {events:?}");
        };
        assert_eq!(*index, 1);
        for e in &events {
            state.apply(e, 0);
        }
        assert_eq!(state.status, WorkflowRunStatus::Suspended);
    }

    /// **The graph endpoint renders a token count against each step, and this
    /// is its only source.** The run's total is a sum, and a sum cannot be
    /// taken apart again — so the split has to be folded as it arrives, keyed
    /// on the agent that spent it.
    #[test]
    fn per_step_tokens_bank_against_the_step_that_spent_them() {
        fn spent(input: u64) -> UsageTotal {
            UsageTotal {
                input_tokens: input,
                ..Default::default()
            }
        }
        let mut state = run();
        let first = advance(&mut state, serde_json::json!({"outcome": "p0"}));
        let second = started_agent(&state);
        let Emit { events, .. } = state.on_agent_started(second);
        for e in &events {
            state.apply(e, 0);
        }

        for (agent, tokens) in [(first, 10), (second, 3), (first, 5)] {
            state.apply(
                &RunnerEvent::Usage {
                    agent,
                    model: "sonnet".into(),
                    spent: spent(tokens),
                },
                0,
            );
        }

        assert_eq!(state.step_usage[&first].input_tokens, 15);
        assert_eq!(state.step_usage[&second].input_tokens, 3);
        // And the run's own total is the sum, so a session-level read does not
        // have to add the steps up itself.
        assert_eq!(state.usage.input_tokens, 18);
        // Keyed on the agent, which the log already maps back to a step.
        assert_eq!(state.index_of_agent(first), Some(0));
        assert_eq!(state.index_of_agent(second), Some(1));
    }

    /// A suspended run is not a finished one: a person can still retry a step
    /// of it, and marking it terminal takes the retry away.
    #[test]
    fn only_a_terminal_run_finishes() {
        for (status, want) in [
            (WorkflowRunStatus::Pending, None),
            (WorkflowRunStatus::Running, None),
            (WorkflowRunStatus::Suspended, None),
            (WorkflowRunStatus::AwaitingInput, None),
            (
                WorkflowRunStatus::Finished,
                Some(crate::sessions::runners::RunnerStatus::Done),
            ),
            (
                WorkflowRunStatus::Failed,
                Some(crate::sessions::runners::RunnerStatus::Failed),
            ),
        ] {
            let state = State { status, ..run() };
            assert_eq!(state.finished(), want, "{status:?}");
        }
    }

    /// A step's own capabilities are spliced into the copy the step agent is
    /// equipped from. Where they land no longer decides anything — a name
    /// belongs to one capability and the sandbox is underneath all of them —
    /// but forgetting them entirely would leave an interactive step with
    /// neither tool and nothing saying so.
    #[test]
    fn a_step_is_equipped_with_its_own_result_and_ask_tools() {
        let s = settings();
        let mut graph = graph();
        graph.steps[0].interactive = true;
        let mut state = run_of(graph);
        state.capabilities = crate::sessions::runners::assemble(
            crate::sessions::runners::RunnerKind::Workflow,
            &crate::sessions::runners::Assembly {
                settings: &s,
                agent: crate::sessions::runners::AgentId::new_v4(),
                depth: 0,
                unattended: false,
                fork: None,
                agent_type: None,
            },
        );
        let actions = state.actions(&view());
        let Action::StartAgent { equipment, .. } = &actions[0] else {
            panic!("expected a start, got {:?}", actions[0]);
        };
        let names: Vec<&str> = equipment.iter().map(|c| c.name()).collect();
        assert!(
            names.contains(&"step_result"),
            "a step cannot submit its result: {names:?}"
        );
        assert!(
            names.contains(&"ask_user"),
            "an interactive step cannot ask: {names:?}"
        );
    }

    /// And a step that did not declare itself interactive is not equipped to
    /// ask — the same runner, one flag apart, which is what lets step 1 stop
    /// for a person and step 2 not.
    ///
    /// It still *holds* the capability: something has to answer for `ask_user`,
    /// or a step that asks anyway falls through to the sandbox and is never
    /// told no.
    #[test]
    fn a_non_interactive_step_holds_ask_user_but_equips_no_tool() {
        let state = run();
        let actions = state.actions(&view());
        let Action::StartAgent { equipment, .. } = &actions[0] else {
            panic!("expected a start, got {:?}", actions[0]);
        };
        let names: Vec<&str> = equipment.iter().map(|c| c.name()).collect();
        assert!(names.contains(&"step_result"));
        assert!(names.contains(&"ask_user"), "{names:?}");

        let asks: Capabilities = equipment
            .iter()
            .filter(|c| c.name() == "ask_user")
            .cloned()
            .collect();
        assert!(
            advertised(&asks, &facts()).is_empty(),
            "a step that may not ask must be equipped with no ask tool"
        );
    }

    /// The slice is snapshotted, and the graph is the part with a hand-written
    /// codec — serde's `rc` feature is off, so an `Arc` that stopped
    /// round-tripping would lose a live run's whole definition.
    #[test]
    fn the_slice_round_trips_through_the_journal() {
        let mut state = run();
        advance(&mut state, serde_json::json!({"outcome": "p0"}));
        let json = serde_json::to_string(&state).unwrap();
        let back: State = serde_json::from_str(&json).unwrap();
        assert_eq!(*back.graph, *state.graph);
        assert_eq!(back.run, state.run);
        assert_eq!(back.steps, state.steps);
        assert_eq!(back.status, state.status);
    }

    /// Fold an `Emit`'s events, exactly as the session does when it persists.
    fn fold(state: &mut State, emit: &Emit) {
        for e in &emit.events {
            state.apply(e, 0);
        }
    }

    /// A retry appends an attempt on the *same edge*: same `from`, same `via`,
    /// same input. Truncating instead would lose the attempt a person is
    /// looking at, and re-routing would draw the retry on a transition nothing
    /// ever took.
    #[test]
    fn a_retry_appends_an_attempt_on_the_original_edge() {
        let mut state = run();
        advance(&mut state, serde_json::json!({"outcome": "p0"}));
        // triage concluded and routed to fix; run fix to completion too, so
        // nothing is in flight when the retry lands.
        advance(&mut state, serde_json::json!({"description": "fixed"}));
        assert_eq!(state.steps.len(), 2);

        let emit = state.retry(1).expect("index 1 is an execution");
        fold(&mut state, &emit);

        assert_eq!(state.steps.len(), 3, "the retry is appended");
        assert_eq!(state.steps[2].step, "fix");
        assert_eq!(state.steps[2].attempt, 2, "the retry numbers itself");
        assert_eq!(state.steps[2].from, state.steps[1].from);
        assert_eq!(state.steps[2].via, state.steps[1].via);
        assert_eq!(state.steps[2].input, state.steps[1].input);
        assert_eq!(
            state.steps[1].status,
            StepStatus::Concluded,
            "the first attempt is untouched"
        );
    }

    /// And it asks for the agent, because nothing else will: `actions()` routes
    /// out of the last *concluded* step, so a retry it did not start is a step
    /// that sits in the log as `Running` with no agent behind it, for ever.
    #[test]
    fn a_retry_starts_the_agent_it_journals() {
        let mut state = run();
        advance(&mut state, serde_json::json!({"outcome": "p0"}));
        advance(&mut state, serde_json::json!({"description": "fixed"}));

        let emit = state.retry(1).expect("index 1 is an execution");
        let Some(Action::StartAgent { agent, first, .. }) = emit.actions.last() else {
            panic!("expected a start, got {:?}", emit.actions);
        };
        let started = *agent;
        let FirstInput::Text(input) = first else {
            panic!("a step is started with its composed input, got {first:?}");
        };
        assert!(input.contains("Do fix."), "{input}");
        fold(&mut state, &emit);
        assert_eq!(
            AgentId(state.steps[2].agent),
            started,
            "the log names an agent the session was never asked to start"
        );
        // And the boundary starts nothing further: the retry is in flight, so a
        // second `actions()` would double-start it.
        assert!(state.actions(&view()).is_empty());
        // Nor does the agent's own start journal a second execution.
        assert!(state.on_agent_started(started).events.is_empty());
    }

    /// A run with a step in flight has it cancelled first — both halves. The
    /// agent's run has to stop, because the run's workspace is shared and two
    /// steps must never write to it at once; and the log entry has to say so,
    /// or `current()` never clears and the run starts nothing ever again.
    #[test]
    fn retrying_mid_step_cancels_the_one_in_flight() {
        let mut state = run();
        let running = advance_to_running(&mut state);

        let emit = state.retry(0).expect("index 0 is an execution");
        assert!(
            emit.actions
                .iter()
                .any(|a| matches!(a, Action::Cancel { agent } if *agent == running)),
            "the step in flight was left running: {:?}",
            emit.actions
        );
        fold(&mut state, &emit);
        assert_eq!(state.steps[0].status, StepStatus::Cancelled);
        assert_eq!(state.steps.len(), 2);
        assert_eq!(
            state.steps[1].attempt, 2,
            "the cancelled attempt still counts"
        );
        assert_eq!(state.current(), Some(1), "the retry is the one in flight");
    }

    /// Start the run's first step and leave it running.
    fn advance_to_running(state: &mut State) -> AgentId {
        let agent = started_agent(state);
        let emit = state.on_agent_started(agent);
        fold(state, &emit);
        agent
    }

    /// An index that names no execution is a person's mistake, so it is
    /// answered rather than journaled. A run that recorded it would hold a
    /// `StepStarted` for a step nobody asked for.
    #[test]
    fn retrying_an_index_that_names_nothing_is_refused() {
        let state = run();
        let err = state.retry(0).expect_err("nothing has run yet");
        assert!(err.contains('0'), "{err}");
        let mut state = run();
        advance(&mut state, serde_json::json!({"outcome": "p0"}));
        assert!(state.retry(7).is_err());
    }

    /// A retried step is equipped from the same declaration an ordinary
    /// execution is — one `start_action`, so the retry cannot drift into
    /// running with a different toolbox from the attempt it replaces.
    #[test]
    fn a_retry_is_equipped_like_any_other_execution() {
        let mut state = run();
        let first = state.actions(&view());
        let Some(Action::StartAgent { equipment, .. }) = first.first() else {
            panic!("expected a start, got {first:?}");
        };
        let ordinary: Vec<&str> = equipment.iter().map(|c| c.name()).collect();

        advance(&mut state, serde_json::json!({"outcome": "p0"}));
        advance(&mut state, serde_json::json!({"description": "fixed"}));
        let emit = state.retry(0).expect("index 0 is an execution");
        let Some(Action::StartAgent { equipment, .. }) = emit.actions.last() else {
            panic!("expected a start, got {:?}", emit.actions);
        };
        assert_eq!(
            equipment.iter().map(|c| c.name()).collect::<Vec<_>>(),
            ordinary
        );
    }
}

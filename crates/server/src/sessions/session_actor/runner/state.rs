//! The session's folded state: a tree of runners, and the session-scoped facts
//! that outrank all of them.
//!
//! Purely a function of the event log. The fold ([`SessionState::apply`]) is
//! one exhaustive match: session-scoped events land on the session's own
//! fields, agent-turn events are routed to the runner that owns the agent, and
//! runner-scoped events arrive already addressed. Nothing here reads a clock,
//! spawns anything, or answers a command — decisions live in the runners,
//! effects in the actor.
//!
//! The session's status is a *projection* ([`SessionState::status`]), never a
//! stored field. Three folds used to write one shared `status` and had to be
//! read together to know who wrote last; deriving it means they cannot
//! disagree.

use crate::agent_loop::UsageTotal;
use crate::sessions::forks::ForkMode;
use crate::sessions::spec::{AgentSettings, SessionSpec, SessionStatus};
use crate::sessions::workflow::{WorkflowRunSpec, WorkflowRunState, WorkflowRunStatus};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

use super::event::{RecordedEnd, RunnerArgs, RunnerEvent, SessionEvent};
use super::ids::{AgentId, RunnerId};
use super::{CANCELLED_ERROR, STOPPED_ERROR};

/// The session's persisted state — purely a function of the event log.
///
/// Snapshotted, so it is a durability contract — but one this redesign breaks
/// deliberately: journals written in the component vocabulary are truncated,
/// not migrated.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    /// What this session *is* — vendor, agent settings, workflow, name.
    ///
    /// In the session's own journal, not just the supervisor's, because a host
    /// that never saw the request creating this session has no parent to take
    /// it from. `None` for exactly as long as it takes a newly created session
    /// to journal it.
    pub spec: Option<SessionSpec>,
    /// The sandbox's lifecycle. Its own slice with a single writer — the
    /// provisioning events — where it used to be three components sharing one
    /// status field.
    pub provisioning: Provisioning,
    /// The terminal session-wide failure, if one happened. The one fact that
    /// outranks every runner: a session whose sandbox is gone for good cannot
    /// run anything, whatever its runners were doing.
    pub fatal: Option<String>,
    /// Every runner this session hosts — the one it *is* (`parent: None`) and
    /// every one nested under an agent. The only place structure is recorded.
    pub runners: BTreeMap<RunnerId, RunnerRecord>,
    /// Tokens banked per agent, keyed as the wire keys agents: `"main"`, or
    /// the agent's uuid. Banked at turn end, so a turn in flight is not in it
    /// and nothing has to be asked of an agent.
    pub agent_usage: BTreeMap<String, UsageTotal>,
}

/// The sandbox's lifecycle.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Provisioning {
    pub phase: ProvisionPhase,
    /// Which provision of this session's runtime is the current one — the
    /// `at_ms` of the `ProvisioningStarted` that began it. Every later
    /// acquisition addresses the sandbox this create produced, so the name has
    /// to outlive the create, and a reload.
    pub provisioned_at_ms: Option<u64>,
}

impl Provisioning {
    /// Whether the runtime is there to run on. The boundary's gate.
    #[must_use]
    pub fn ready(&self) -> bool {
        matches!(self.phase, ProvisionPhase::Ready)
    }
}

/// Where the sandbox's create has got to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum ProvisionPhase {
    /// Nothing journaled yet — a session that has not begun provisioning.
    #[default]
    NotStarted,
    /// The create is journaled and may be running at the vendor. Found at
    /// load, it is safe to re-attempt: no turn can have run under it.
    InFlight,
    Ready,
    /// The create failed retryably. A terminal failure is `fatal` instead —
    /// there is nothing left to retry.
    Failed {
        error: String,
    },
}

/// One runner.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunnerRecord {
    /// The agent that asked for this runner — the nesting edge, and the agent
    /// owed its result if its kind owes one. `None` only for the session's
    /// root.
    pub parent: Option<AgentId>,
    pub created_at_ms: u64,
    pub state: RunnerState,
}

/// A runner's own slice, by kind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RunnerState {
    Main(MainState),
    Sub(SubState),
    Fork(ForkState),
    Workflow(WorkflowState),
}

/// Where a conversation's current turn is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum TurnPhase {
    #[default]
    Idle,
    Running,
    /// Parked on one or more questions. Carries none of them: the questions
    /// belong to the agent that asked and are answered through it.
    AwaitingInput,
    /// The last turn failed. Sticky so a reader can see it; fully recoverable —
    /// the next turn moves it back to `Running`.
    Failed {
        error: String,
    },
}

/// The session's own conversation.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MainState {
    pub turn: TurnPhase,
}

/// One delegated task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubState {
    pub label: String,
    pub task: String,
    /// The plugin-declared agent type this subagent runs as, if any. `None` is
    /// the general-purpose subagent.
    pub agent_type: Option<String>,
    /// The caller's effective settings, snapshotted at spawn. A cold
    /// subagent's settings are in its own record — nothing is resolved through
    /// a recursive walk at read time.
    pub settings: AgentSettings,
    pub phase: SubPhase,
}

/// Where a delegated task is. `Done` is turn-terminal, not actor-terminal: a
/// node with children wakes again to consume their results and concludes a
/// second time, which is a fresh `Running` cycle.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubPhase {
    Running {
        /// When this cycle began — the spawn, or the wake that started it. The
        /// span restarts per cycle, so a parent is told about the work it is
        /// being reported, not the whole life of the node.
        since_ms: u64,
    },
    Done {
        /// `Ok` is the report; `Err` is what went wrong. Which side it is *is*
        /// the node's terminal status — a node cannot hold an output and count
        /// as failed, which the old record's separate `status`/`output`/`error`
        /// fields allowed.
        result: Result<String, String>,
        started_ms: u64,
        ended_ms: u64,
        /// Whether the parent was sent this result. Inside `Done` so a running
        /// node cannot be marked notified — every completion starts it false,
        /// every actual send re-marks it, and that pair is what makes delivery
        /// exactly-once across offloads.
        notified: bool,
    },
}

/// One fork of a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForkState {
    /// The source agent's log seq this fork was taken at — the branch point.
    pub source_seq: u64,
    pub mode: ForkMode,
    /// What the fork was created to do — the message typed after `/fork`.
    /// Durable so a fork abandoned mid-seed is re-seeded with it.
    pub message: String,
    /// What the fork has named itself, once it has.
    pub title: Option<String>,
    pub seed: SeedPhase,
    pub turn: TurnPhase,
    /// When this fork last did anything — the moment of its most recent turn
    /// event. A conversation has no *end*, but a reader looking at a session's
    /// shape still needs to know how far along a fork got.
    pub last_activity_ms: u64,
}

/// Where a fork's history seed is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SeedPhase {
    /// Nothing may run until the seed lands — the same state a session is in
    /// while its runtime is built, and the reason a fork found in it at load
    /// is safe to re-seed: no turn has run.
    Seeding,
    Seeded,
    /// The seed could not be produced. Carries the reason verbatim, because
    /// that string is what the user is shown.
    Failed {
        error: String,
    },
}

/// One workflow run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowState {
    /// The run's resolved graph, snapshotted into the `Created` event that
    /// made this runner. Replay reconstructs it without a store, and a
    /// definition edited mid-run cannot change a step that has not started.
    pub graph: Arc<WorkflowRunSpec>,
    /// The run log: which steps executed, in what order, and what became of
    /// each. The shape the graph projection and the retry vocabulary already
    /// speak.
    pub run: WorkflowRunState,
    /// Whether the agent that asked for this run was sent its terminal output.
    /// Meaningless for the session-root run, which nobody asked for.
    pub notified: bool,
}

impl ForkState {
    /// What a reader is told about this fork. Derived, never stored: the old
    /// roster journaled status *changes* and the writers could disagree.
    ///
    /// A turn that has moved outranks a pending seed — a fork drains its first
    /// message the moment the seed arrives, routinely before the session
    /// records that it landed, and reporting `Provisioning` through that first
    /// turn is the bug the old `apply_seeded` filter existed to avoid.
    #[must_use]
    pub fn agent_status(&self) -> super::super::AgentStatus {
        use super::super::AgentStatus;
        match (&self.seed, &self.turn) {
            (SeedPhase::Failed { .. }, _) => AgentStatus::Failed,
            (SeedPhase::Seeding, TurnPhase::Idle) => AgentStatus::Provisioning,
            (_, TurnPhase::Idle) => AgentStatus::Idle,
            (_, TurnPhase::Running) => AgentStatus::Running,
            (_, TurnPhase::AwaitingInput) => AgentStatus::AwaitingInput,
            (_, TurnPhase::Failed { .. }) => AgentStatus::Failed,
        }
    }
}

impl SessionState {
    // -- structure ---------------------------------------------------------

    /// The runner this session *is*: the one with no parent. Its phase is the
    /// session's status. `None` only before the root's `Created` has been
    /// journaled.
    #[must_use]
    pub fn root(&self) -> Option<(RunnerId, &RunnerRecord)> {
        self.runners
            .iter()
            .find(|(_, r)| r.parent.is_none())
            .map(|(id, r)| (*id, r))
    }

    #[must_use]
    pub fn record(&self, id: RunnerId) -> Option<&RunnerRecord> {
        self.runners.get(&id)
    }

    /// The runner that owns `agent`: the runner of the same id for a Main, Sub
    /// or Fork agent, else the workflow whose step log names it. One probe, no
    /// ordering — what an id is lives in the map, not in which registry
    /// answered first.
    #[must_use]
    pub fn owner_of(&self, agent: AgentId) -> Option<RunnerId> {
        let direct = RunnerId::of_agent(agent);
        if self.runners.contains_key(&direct) {
            return Some(direct);
        }
        self.runners.iter().find_map(|(id, r)| match &r.state {
            RunnerState::Workflow(w) if w.run.index_of_agent(agent.0).is_some() => Some(*id),
            RunnerState::Main(_)
            | RunnerState::Sub(_)
            | RunnerState::Fork(_)
            | RunnerState::Workflow(_) => None,
        })
    }

    /// How deep `id` sits in the runner tree: the root is 0, and every
    /// agent→child-runner edge costs 1 whether the child is a subagent, a fork
    /// or a nested run. The recursion budget's measure — one number across the
    /// combined tree, where the old forest counted only subagent edges.
    ///
    /// A walk, not a stored field: the old records carried `depth` and had to
    /// keep it in step with the structure it duplicated.
    #[must_use]
    pub fn depth_of(&self, id: RunnerId) -> u32 {
        let mut depth = 0;
        let mut current = id;
        // Bounded by the walk hitting the root or an unknown parent; a cycle
        // cannot be journaled because a child is always created after its
        // parent's agent.
        while let Some(parent) = self.runners.get(&current).and_then(|r| r.parent) {
            depth += 1;
            match self.owner_of(parent) {
                Some(owner) if owner != current => current = owner,
                _ => break,
            }
        }
        depth
    }

    // -- projections -------------------------------------------------------

    /// The session's status: a projection of the facts, never a stored field.
    ///
    /// Precedence is the point. A terminal failure outranks everything; a
    /// sandbox being built or broken outranks the runners (nothing can run
    /// without it); otherwise the session *is* its root runner, and the root's
    /// phase is the answer.
    #[must_use]
    pub fn status(&self) -> SessionStatus {
        if let Some(reason) = &self.fatal {
            return SessionStatus::Unrecoverable {
                reason: reason.clone(),
            };
        }
        match &self.provisioning.phase {
            ProvisionPhase::InFlight => return SessionStatus::Provisioning,
            ProvisionPhase::Failed { error } => {
                return SessionStatus::ProvisioningFailed {
                    reason: error.clone(),
                };
            }
            ProvisionPhase::NotStarted | ProvisionPhase::Ready => {}
        }
        match self.root().map(|(_, r)| &r.state) {
            Some(RunnerState::Main(m)) => match &m.turn {
                TurnPhase::Idle => SessionStatus::Idle,
                TurnPhase::Running => SessionStatus::Running,
                TurnPhase::AwaitingInput => SessionStatus::AwaitingInput,
                TurnPhase::Failed { error } => SessionStatus::Failed {
                    reason: error.clone(),
                },
            },
            Some(RunnerState::Workflow(w)) => match &w.run.status {
                // A run that has not started and one suspended between
                // attempts both rest; `Finished` is what tells a completed run
                // apart, and is why this arm is not `Idle` for it.
                WorkflowRunStatus::Pending | WorkflowRunStatus::Suspended => SessionStatus::Idle,
                WorkflowRunStatus::Running => SessionStatus::Running,
                WorkflowRunStatus::AwaitingInput => SessionStatus::AwaitingInput,
                WorkflowRunStatus::Finished => SessionStatus::Finished,
                WorkflowRunStatus::Failed => SessionStatus::Failed {
                    reason: w.run.error.clone().unwrap_or_default(),
                },
            },
            // A sub or fork can never be the root; a session with no root yet
            // has not recorded what it is.
            Some(RunnerState::Sub(_) | RunnerState::Fork(_)) | None => SessionStatus::Idle,
        }
    }

    /// The error a reader is shown beside the status, when the status carries
    /// one. Derived from [`Self::status`] so the two cannot disagree — the old
    /// stored `last_error` was cleared and set by three different folds.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        match self.status() {
            SessionStatus::Failed { reason }
            | SessionStatus::ProvisioningFailed { reason }
            | SessionStatus::Unrecoverable { reason } => Some(reason),
            SessionStatus::Provisioning
            | SessionStatus::Idle
            | SessionStatus::Running
            | SessionStatus::Finished
            | SessionStatus::AwaitingInput => None,
        }
    }

    /// Tokens banked across every agent this session hosts.
    #[must_use]
    pub fn session_usage_total(&self) -> UsageTotal {
        self.agent_usage
            .values()
            .fold(UsageTotal::default(), |acc, u| acc.combine(u))
    }

    // -- fold --------------------------------------------------------------

    /// Fold one event. Total: an event addressed to a runner that is not
    /// there, or of the wrong kind, changes nothing — journal corruption is
    /// not something a fold can repair, and a panic here takes recovery with
    /// it.
    pub fn apply(&mut self, event: &SessionEvent) {
        match event.clone() {
            SessionEvent::SpecRecorded { spec } => {
                self.spec = Some(*spec);
            }
            SessionEvent::Renamed { name } => {
                // Only the name moves. A rename must not resurrect a spec that
                // was never recorded.
                if let Some(spec) = self.spec.as_mut() {
                    spec.name = Some(name);
                }
            }
            SessionEvent::ProvisioningStarted { at_ms } => {
                self.provisioning.phase = ProvisionPhase::InFlight;
                self.provisioning.provisioned_at_ms = Some(at_ms);
            }
            SessionEvent::ProvisioningProgress { .. } => {}
            SessionEvent::ProvisioningSucceeded { .. } => {
                self.provisioning.phase = ProvisionPhase::Ready;
            }
            SessionEvent::ProvisioningFailed {
                error, terminal, ..
            } => {
                self.provisioning.phase = ProvisionPhase::Failed {
                    error: error.clone(),
                };
                if terminal {
                    self.fatal = Some(error);
                }
            }
            SessionEvent::SessionFailed { reason, .. } => {
                self.fatal = Some(reason);
            }
            SessionEvent::UsageRecorded {
                agent_id,
                usage_total,
                ..
            } => {
                self.agent_usage.insert(agent_id, usage_total);
            }
            SessionEvent::TurnBegan { at_ms, agent } => self.fold_turn_began(agent, at_ms),
            SessionEvent::TurnEnded { at_ms, agent, end } => {
                self.fold_turn_ended(agent, end, at_ms);
            }
            SessionEvent::Runner { id, at_ms, event } => self.fold_runner(id, event, at_ms),
        }
    }

    fn fold_turn_began(&mut self, agent: AgentId, at_ms: u64) {
        let Some(owner) = self.owner_of(agent) else {
            return;
        };
        let Some(record) = self.runners.get_mut(&owner) else {
            return;
        };
        match &mut record.state {
            RunnerState::Main(m) => m.turn = TurnPhase::Running,
            RunnerState::Fork(f) => {
                f.turn = TurnPhase::Running;
                f.last_activity_ms = at_ms;
            }
            // A wake: a terminal node starting another cycle to consume child
            // results. The span restarts with it.
            RunnerState::Sub(s) => s.phase = SubPhase::Running { since_ms: at_ms },
            // A step's beginning is its `StepStarted`; the agent starting the
            // turn that serves it adds nothing.
            RunnerState::Workflow(_) => {}
        }
    }

    fn fold_turn_ended(&mut self, agent: AgentId, end: RecordedEnd, at_ms: u64) {
        let Some(owner) = self.owner_of(agent) else {
            return;
        };
        let Some(record) = self.runners.get_mut(&owner) else {
            return;
        };
        match &mut record.state {
            RunnerState::Main(m) => m.turn = conversation_phase(end),
            RunnerState::Fork(f) => {
                f.turn = conversation_phase(end);
                f.last_activity_ms = at_ms;
            }
            RunnerState::Sub(s) => {
                let started_ms = match s.phase {
                    SubPhase::Running { since_ms } => since_ms,
                    SubPhase::Done { started_ms, .. } => started_ms,
                };
                let result = match end {
                    RecordedEnd::Concluded { output } => Ok(render_sub_output(&output)),
                    RecordedEnd::Failed { error } => Err(error),
                    RecordedEnd::Stopped => Err(STOPPED_ERROR.to_string()),
                    // Never journaled for a subagent: an ask is converted to a
                    // failure before journaling, and an interruption is
                    // repaired from this state at load instead.
                    RecordedEnd::Asked | RecordedEnd::Interrupted => return,
                };
                s.phase = SubPhase::Done {
                    result,
                    started_ms,
                    ended_ms: at_ms,
                    notified: false,
                };
            }
            RunnerState::Workflow(w) => {
                let Some(index) = w.run.index_of_agent(agent.0) else {
                    return;
                };
                match end {
                    RecordedEnd::Concluded { output } => {
                        w.run.apply_concluded(index, output, at_ms)
                    }
                    RecordedEnd::Failed { error } => {
                        w.run.apply_step_failed(index, error.clone(), at_ms);
                        w.run.apply_failed(error);
                    }
                    RecordedEnd::Asked => w.run.apply_awaiting(),
                    // A stopped step is a `StepCancelled`; an interrupted one
                    // is repaired at load.
                    RecordedEnd::Stopped | RecordedEnd::Interrupted => {}
                }
            }
        }
    }

    fn fold_runner(&mut self, id: RunnerId, event: RunnerEvent, at_ms: u64) {
        if let RunnerEvent::Created { parent, args } = event {
            self.runners.insert(
                id,
                RunnerRecord {
                    parent,
                    created_at_ms: at_ms,
                    state: born(*args, at_ms),
                },
            );
            return;
        }
        if matches!(event, RunnerEvent::ForkDeleted) {
            self.runners.remove(&id);
            return;
        }
        let Some(record) = self.runners.get_mut(&id) else {
            return;
        };
        match (&mut record.state, event) {
            (
                RunnerState::Workflow(w),
                RunnerEvent::StepStarted {
                    index: _,
                    step,
                    agent,
                    attempt,
                    from,
                    via,
                    input,
                },
            ) => {
                w.run
                    .apply_started(step, agent.0, attempt, from, via, input, at_ms);
            }
            (RunnerState::Workflow(w), RunnerEvent::StepCancelled { index }) => {
                w.run.apply_cancelled(index, at_ms);
            }
            (RunnerState::Workflow(w), RunnerEvent::RunFinished { output }) => {
                w.run.apply_finished(output);
            }
            (RunnerState::Workflow(w), RunnerEvent::RunFailed { error }) => {
                w.run.apply_failed(error);
            }
            (RunnerState::Workflow(w), RunnerEvent::Reported) => w.notified = true,
            (RunnerState::Workflow(w), RunnerEvent::Cancelled) => {
                if let Some(current) = w.run.current() {
                    w.run.apply_cancelled(current, at_ms);
                }
                w.run.apply_failed(CANCELLED_ERROR.to_string());
            }
            (RunnerState::Sub(s), RunnerEvent::Reported) => {
                if let SubPhase::Done { notified, .. } = &mut s.phase {
                    *notified = true;
                }
            }
            (RunnerState::Sub(s), RunnerEvent::Cancelled) => {
                if let SubPhase::Running { since_ms } = s.phase {
                    s.phase = SubPhase::Done {
                        result: Err(STOPPED_ERROR.to_string()),
                        started_ms: since_ms,
                        ended_ms: at_ms,
                        notified: false,
                    };
                }
            }
            (RunnerState::Fork(f), RunnerEvent::ForkSeeded) => {
                if matches!(f.seed, SeedPhase::Seeding) {
                    f.seed = SeedPhase::Seeded;
                }
            }
            (RunnerState::Fork(f), RunnerEvent::ForkSeedFailed { error }) => {
                f.seed = SeedPhase::Failed { error };
            }
            (RunnerState::Fork(f), RunnerEvent::ForkTitled { name }) => {
                f.title = Some(name);
            }
            // Addressed to a runner of the wrong kind: journal corruption the
            // fold cannot repair. Changing nothing beats guessing.
            (
                RunnerState::Main(_)
                | RunnerState::Sub(_)
                | RunnerState::Fork(_)
                | RunnerState::Workflow(_),
                _,
            ) => {}
        }
    }
}

/// What a conversation's phase becomes at a recorded end.
fn conversation_phase(end: RecordedEnd) -> TurnPhase {
    match end {
        // A stop and an interruption rest the conversation exactly as a
        // conclusion does; they differ in intent, which the transcript keeps.
        RecordedEnd::Concluded { .. } | RecordedEnd::Stopped | RecordedEnd::Interrupted => {
            TurnPhase::Idle
        }
        RecordedEnd::Asked => TurnPhase::AwaitingInput,
        RecordedEnd::Failed { error } => TurnPhase::Failed { error },
    }
}

/// A subagent's report, as its parent will read it: the final text, or the
/// structured output rendered as JSON.
fn render_sub_output(output: &serde_json::Value) -> String {
    output
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| output.to_string())
}

/// A newborn runner's slice, from its `Created` args.
pub(crate) fn born(args: RunnerArgs, at_ms: u64) -> RunnerState {
    match args {
        RunnerArgs::Main => RunnerState::Main(MainState::default()),
        RunnerArgs::Sub {
            label,
            task,
            agent_type,
            settings,
        } => RunnerState::Sub(SubState {
            label,
            task,
            agent_type,
            settings,
            phase: SubPhase::Running { since_ms: at_ms },
        }),
        RunnerArgs::Fork {
            source_seq,
            mode,
            message,
        } => RunnerState::Fork(ForkState {
            source_seq,
            mode,
            message,
            title: None,
            seed: SeedPhase::Seeding,
            turn: TurnPhase::default(),
            last_activity_ms: at_ms,
        }),
        RunnerArgs::Workflow { graph } => RunnerState::Workflow(WorkflowState {
            graph: Arc::new(graph),
            run: WorkflowRunState::default(),
            notified: false,
        }),
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
    //! The fold, tested as the behavioral spec it is: each test pins a rule
    //! the component-era folds enforced, in the runner vocabulary.

    use super::*;
    use crate::sessions::session_actor::AgentStatus;
    use crate::sessions::workflow::{StepStatus, WorkflowStepSpec};
    use uuid::Uuid;

    fn fold(events: &[SessionEvent]) -> SessionState {
        let mut state = SessionState::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    fn settings() -> AgentSettings {
        AgentSettings {
            model: "mock".into(),
            instructions: None,
            allowed_tools: None,
            use_plugins: None,
            max_iterations: None,
            max_retries: 0,
            mcp_servers: vec![],
            memory_spaces: vec![],
            thinking_effort: None,
            max_concurrent_subagents: None,
            auto_compact: None,
            control_plane: None,
            plugins: Vec::new(),
        }
    }

    fn graph(steps: &[&str]) -> WorkflowRunSpec {
        WorkflowRunSpec {
            workflow: "wf".into(),
            start: steps.first().unwrap().to_string(),
            steps: steps
                .iter()
                .map(|name| WorkflowStepSpec {
                    name: (*name).to_string(),
                    agent: "preset".into(),
                    prompt: "do it".into(),
                    outcomes: vec![],
                    fields: vec![],
                    interactive: false,
                    transitions: vec![],
                    settings: settings(),
                })
                .collect(),
            input: "go".into(),
            max_steps: 100,
        }
    }

    fn main_created(agent: AgentId) -> SessionEvent {
        SessionEvent::Runner {
            id: RunnerId::of_agent(agent),
            at_ms: 100,
            event: RunnerEvent::Created {
                parent: None,
                args: Box::new(RunnerArgs::Main),
            },
        }
    }

    fn sub_created(id: AgentId, parent: AgentId, at_ms: u64) -> SessionEvent {
        SessionEvent::Runner {
            id: RunnerId::of_agent(id),
            at_ms,
            event: RunnerEvent::Created {
                parent: Some(parent),
                args: Box::new(RunnerArgs::Sub {
                    label: "worker".into(),
                    task: "dig".into(),
                    agent_type: None,
                    settings: settings(),
                }),
            },
        }
    }

    fn ended(agent: AgentId, end: RecordedEnd, at_ms: u64) -> SessionEvent {
        SessionEvent::TurnEnded { at_ms, agent, end }
    }

    // -- provisioning ------------------------------------------------------

    #[test]
    fn a_create_moves_the_status_and_names_the_provision() {
        let s = fold(&[SessionEvent::ProvisioningStarted { at_ms: 7 }]);
        assert_eq!(s.status(), SessionStatus::Provisioning);
        assert_eq!(s.provisioning.provisioned_at_ms, Some(7));
        assert!(!s.provisioning.ready());
    }

    #[test]
    fn a_confirmed_runtime_rests_the_session() {
        let main = AgentId(Uuid::new_v4());
        let s = fold(&[
            main_created(main),
            SessionEvent::ProvisioningStarted { at_ms: 7 },
            SessionEvent::ProvisioningSucceeded { at_ms: 9 },
        ]);
        assert_eq!(s.status(), SessionStatus::Idle);
        assert!(s.provisioning.ready());
        assert_eq!(s.last_error(), None);
    }

    #[test]
    fn a_retryable_create_failure_is_not_terminal() {
        let s = fold(&[
            SessionEvent::ProvisioningStarted { at_ms: 7 },
            SessionEvent::ProvisioningFailed {
                at_ms: 8,
                error: "vendor offline".into(),
                terminal: false,
            },
        ]);
        assert_eq!(
            s.status(),
            SessionStatus::ProvisioningFailed {
                reason: "vendor offline".into()
            }
        );
        assert_eq!(s.last_error().as_deref(), Some("vendor offline"));
        // A re-attempt takes the same path a first create does.
        let mut s = s;
        s.apply(&SessionEvent::ProvisioningStarted { at_ms: 20 });
        assert_eq!(s.status(), SessionStatus::Provisioning);
        assert_eq!(s.provisioning.provisioned_at_ms, Some(20));
    }

    #[test]
    fn a_terminal_create_failure_ends_the_session() {
        let s = fold(&[
            SessionEvent::ProvisioningStarted { at_ms: 7 },
            SessionEvent::ProvisioningFailed {
                at_ms: 8,
                error: "refused".into(),
                terminal: true,
            },
        ]);
        assert_eq!(
            s.status(),
            SessionStatus::Unrecoverable {
                reason: "refused".into()
            }
        );
    }

    #[test]
    fn a_session_failure_outranks_everything() {
        let main = AgentId(Uuid::new_v4());
        let s = fold(&[
            main_created(main),
            SessionEvent::ProvisioningStarted { at_ms: 1 },
            SessionEvent::ProvisioningSucceeded { at_ms: 2 },
            SessionEvent::TurnBegan {
                at_ms: 3,
                agent: main,
            },
            SessionEvent::SessionFailed {
                at_ms: 4,
                reason: "sandbox gone".into(),
            },
        ]);
        assert_eq!(
            s.status(),
            SessionStatus::Unrecoverable {
                reason: "sandbox gone".into()
            }
        );
        assert_eq!(s.last_error().as_deref(), Some("sandbox gone"));
    }

    // -- the conversation --------------------------------------------------

    #[test]
    fn a_conversations_turn_cycle_moves_the_session() {
        let main = AgentId(Uuid::new_v4());
        let mut s = fold(&[main_created(main)]);
        assert_eq!(s.status(), SessionStatus::Idle);

        s.apply(&SessionEvent::TurnBegan {
            at_ms: 10,
            agent: main,
        });
        assert_eq!(s.status(), SessionStatus::Running);

        s.apply(&ended(main, RecordedEnd::Asked, 20));
        assert_eq!(s.status(), SessionStatus::AwaitingInput);

        s.apply(&SessionEvent::TurnBegan {
            at_ms: 30,
            agent: main,
        });
        s.apply(&ended(
            main,
            RecordedEnd::Concluded {
                output: serde_json::Value::Null,
            },
            40,
        ));
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    #[test]
    fn a_failed_turn_is_sticky_until_the_next_one_begins() {
        let main = AgentId(Uuid::new_v4());
        let mut s = fold(&[
            main_created(main),
            SessionEvent::TurnBegan {
                at_ms: 10,
                agent: main,
            },
            ended(
                main,
                RecordedEnd::Failed {
                    error: "boom".into(),
                },
                20,
            ),
        ]);
        assert_eq!(
            s.status(),
            SessionStatus::Failed {
                reason: "boom".into()
            }
        );
        assert_eq!(s.last_error().as_deref(), Some("boom"));
        // The failure is history once a new turn is under way.
        s.apply(&SessionEvent::TurnBegan {
            at_ms: 30,
            agent: main,
        });
        assert_eq!(s.status(), SessionStatus::Running);
        assert_eq!(s.last_error(), None);
    }

    #[test]
    fn a_stop_and_an_interruption_rest_the_conversation() {
        let main = AgentId(Uuid::new_v4());
        for end in [RecordedEnd::Stopped, RecordedEnd::Interrupted] {
            let s = fold(&[
                main_created(main),
                SessionEvent::TurnBegan {
                    at_ms: 10,
                    agent: main,
                },
                ended(main, end, 20),
            ]);
            assert_eq!(s.status(), SessionStatus::Idle);
        }
    }

    // -- delegated work ----------------------------------------------------

    #[test]
    fn a_spawned_sub_is_running_and_owned() {
        let main = AgentId(Uuid::new_v4());
        let sub = AgentId(Uuid::new_v4());
        let s = fold(&[main_created(main), sub_created(sub, main, 200)]);
        let id = RunnerId::of_agent(sub);
        assert_eq!(s.owner_of(sub), Some(id));
        assert_eq!(s.depth_of(id), 1);
        let RunnerState::Sub(node) = &s.record(id).unwrap().state else {
            panic!("expected a sub runner");
        };
        assert_eq!(node.phase, SubPhase::Running { since_ms: 200 });
        // A sub never moves the session's status.
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    #[test]
    fn a_subs_conclusion_owes_its_parent_and_reporting_settles_it() {
        let main = AgentId(Uuid::new_v4());
        let sub = AgentId(Uuid::new_v4());
        let id = RunnerId::of_agent(sub);
        let mut s = fold(&[
            main_created(main),
            sub_created(sub, main, 200),
            ended(
                sub,
                RecordedEnd::Concluded {
                    output: "found it".into(),
                },
                400,
            ),
        ]);
        let RunnerState::Sub(node) = &s.record(id).unwrap().state else {
            panic!("expected a sub runner");
        };
        assert_eq!(
            node.phase,
            SubPhase::Done {
                result: Ok("found it".into()),
                started_ms: 200,
                ended_ms: 400,
                notified: false,
            }
        );
        s.apply(&SessionEvent::Runner {
            id,
            at_ms: 401,
            event: RunnerEvent::Reported,
        });
        let RunnerState::Sub(node) = &s.record(id).unwrap().state else {
            panic!("expected a sub runner");
        };
        assert!(matches!(node.phase, SubPhase::Done { notified: true, .. }));
    }

    /// A woken node reports the cycle its parent is being told about, not the
    /// whole life of the node.
    #[test]
    fn a_second_cycle_reports_its_own_span_and_owes_again() {
        let main = AgentId(Uuid::new_v4());
        let sub = AgentId(Uuid::new_v4());
        let id = RunnerId::of_agent(sub);
        let s = fold(&[
            main_created(main),
            sub_created(sub, main, 200),
            ended(
                sub,
                RecordedEnd::Concluded {
                    output: "first".into(),
                },
                400,
            ),
            SessionEvent::Runner {
                id,
                at_ms: 401,
                event: RunnerEvent::Reported,
            },
            SessionEvent::TurnBegan {
                at_ms: 5_000,
                agent: sub,
            },
            ended(
                sub,
                RecordedEnd::Concluded {
                    output: "second".into(),
                },
                5_200,
            ),
        ]);
        let RunnerState::Sub(node) = &s.record(id).unwrap().state else {
            panic!("expected a sub runner");
        };
        assert_eq!(
            node.phase,
            SubPhase::Done {
                result: Ok("second".into()),
                started_ms: 5_000,
                ended_ms: 5_200,
                notified: false,
            }
        );
    }

    #[test]
    fn a_subs_structured_output_is_rendered_and_a_failure_is_kept_verbatim() {
        let main = AgentId(Uuid::new_v4());
        let ok = AgentId(Uuid::new_v4());
        let bad = AgentId(Uuid::new_v4());
        let s = fold(&[
            main_created(main),
            sub_created(ok, main, 200),
            sub_created(bad, main, 200),
            ended(
                ok,
                RecordedEnd::Concluded {
                    output: serde_json::json!({"n": 1}),
                },
                300,
            ),
            ended(
                bad,
                RecordedEnd::Failed {
                    error: "boom".into(),
                },
                300,
            ),
        ]);
        let RunnerState::Sub(node) = &s.record(RunnerId::of_agent(ok)).unwrap().state else {
            panic!("expected a sub runner");
        };
        assert!(
            matches!(&node.phase, SubPhase::Done { result: Ok(text), .. } if text == "{\"n\":1}")
        );
        let RunnerState::Sub(node) = &s.record(RunnerId::of_agent(bad)).unwrap().state else {
            panic!("expected a sub runner");
        };
        assert!(matches!(&node.phase, SubPhase::Done { result: Err(e), .. } if e == "boom"));
    }

    #[test]
    fn nesting_walks_the_parent_chain() {
        let main = AgentId(Uuid::new_v4());
        let lead = AgentId(Uuid::new_v4());
        let helper = AgentId(Uuid::new_v4());
        let s = fold(&[
            main_created(main),
            sub_created(lead, main, 200),
            sub_created(helper, lead, 300),
        ]);
        assert_eq!(s.depth_of(RunnerId::of_agent(main)), 0);
        assert_eq!(s.depth_of(RunnerId::of_agent(lead)), 1);
        assert_eq!(s.depth_of(RunnerId::of_agent(helper)), 2);
    }

    // -- forks -------------------------------------------------------------

    fn fork_created(id: AgentId, parent: AgentId, at_ms: u64) -> SessionEvent {
        SessionEvent::Runner {
            id: RunnerId::of_agent(id),
            at_ms,
            event: RunnerEvent::Created {
                parent: Some(parent),
                args: Box::new(RunnerArgs::Fork {
                    source_seq: 42,
                    mode: ForkMode::Copy,
                    message: "try another way".into(),
                }),
            },
        }
    }

    fn fork_state(s: &SessionState, id: AgentId) -> &ForkState {
        match &s.record(RunnerId::of_agent(id)).unwrap().state {
            RunnerState::Fork(f) => f,
            other => panic!("expected a fork runner, got {other:?}"),
        }
    }

    #[test]
    fn a_fork_provisions_until_its_seed_lands() {
        let main = AgentId(Uuid::new_v4());
        let fork = AgentId(Uuid::new_v4());
        let mut s = fold(&[main_created(main), fork_created(fork, main, 500)]);
        assert_eq!(
            fork_state(&s, fork).agent_status(),
            AgentStatus::Provisioning
        );
        s.apply(&SessionEvent::Runner {
            id: RunnerId::of_agent(fork),
            at_ms: 600,
            event: RunnerEvent::ForkSeeded,
        });
        assert_eq!(fork_state(&s, fork).agent_status(), AgentStatus::Idle);
    }

    /// A fork drains its first message the moment the seed arrives, routinely
    /// before the session records that it landed. The turn outranks the seed.
    #[test]
    fn a_forks_first_turn_outranks_a_seed_still_landing() {
        let main = AgentId(Uuid::new_v4());
        let fork = AgentId(Uuid::new_v4());
        let s = fold(&[
            main_created(main),
            fork_created(fork, main, 500),
            SessionEvent::TurnBegan {
                at_ms: 550,
                agent: fork,
            },
        ]);
        assert_eq!(fork_state(&s, fork).agent_status(), AgentStatus::Running);
    }

    #[test]
    fn a_forks_turns_never_move_the_session() {
        let main = AgentId(Uuid::new_v4());
        let fork = AgentId(Uuid::new_v4());
        let mut s = fold(&[
            main_created(main),
            fork_created(fork, main, 500),
            SessionEvent::Runner {
                id: RunnerId::of_agent(fork),
                at_ms: 600,
                event: RunnerEvent::ForkSeeded,
            },
            SessionEvent::TurnBegan {
                at_ms: 700,
                agent: fork,
            },
        ]);
        assert_eq!(s.status(), SessionStatus::Idle);
        assert_eq!(fork_state(&s, fork).agent_status(), AgentStatus::Running);
        s.apply(&ended(fork, RecordedEnd::Asked, 800));
        assert_eq!(
            fork_state(&s, fork).agent_status(),
            AgentStatus::AwaitingInput
        );
        assert_eq!(fork_state(&s, fork).last_activity_ms, 800);
        s.apply(&ended(fork, RecordedEnd::Failed { error: "x".into() }, 900));
        assert_eq!(fork_state(&s, fork).agent_status(), AgentStatus::Failed);
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    #[test]
    fn a_failed_seed_names_its_reason_and_a_delete_removes_the_fork() {
        let main = AgentId(Uuid::new_v4());
        let fork = AgentId(Uuid::new_v4());
        let mut s = fold(&[
            main_created(main),
            fork_created(fork, main, 500),
            SessionEvent::Runner {
                id: RunnerId::of_agent(fork),
                at_ms: 600,
                event: RunnerEvent::ForkSeedFailed {
                    error: "source gone".into(),
                },
            },
        ]);
        assert_eq!(fork_state(&s, fork).agent_status(), AgentStatus::Failed);
        assert_eq!(
            fork_state(&s, fork).seed,
            SeedPhase::Failed {
                error: "source gone".into()
            }
        );
        s.apply(&SessionEvent::Runner {
            id: RunnerId::of_agent(fork),
            at_ms: 700,
            event: RunnerEvent::ForkDeleted,
        });
        assert!(s.record(RunnerId::of_agent(fork)).is_none());
    }

    #[test]
    fn a_fork_titles_itself() {
        let main = AgentId(Uuid::new_v4());
        let fork = AgentId(Uuid::new_v4());
        let s = fold(&[
            main_created(main),
            fork_created(fork, main, 500),
            SessionEvent::Runner {
                id: RunnerId::of_agent(fork),
                at_ms: 600,
                event: RunnerEvent::ForkTitled {
                    name: "the detour".into(),
                },
            },
        ]);
        assert_eq!(fork_state(&s, fork).title.as_deref(), Some("the detour"));
    }

    // -- workflow runs -----------------------------------------------------

    fn run_created(id: RunnerId, parent: Option<AgentId>, steps: &[&str]) -> SessionEvent {
        SessionEvent::Runner {
            id,
            at_ms: 100,
            event: RunnerEvent::Created {
                parent,
                args: Box::new(RunnerArgs::Workflow {
                    graph: graph(steps),
                }),
            },
        }
    }

    fn step_started(
        id: RunnerId,
        index: u32,
        step: &str,
        agent: AgentId,
        at_ms: u64,
    ) -> SessionEvent {
        SessionEvent::Runner {
            id,
            at_ms,
            event: RunnerEvent::StepStarted {
                index,
                step: step.into(),
                agent,
                attempt: 1,
                from: None,
                via: None,
                input: "go".into(),
            },
        }
    }

    fn run_state(s: &SessionState, id: RunnerId) -> &WorkflowState {
        match &s.record(id).unwrap().state {
            RunnerState::Workflow(w) => w,
            other => panic!("expected a workflow runner, got {other:?}"),
        }
    }

    #[test]
    fn a_root_runs_steps_move_the_session() {
        let run = RunnerId(Uuid::new_v4());
        let step_agent = AgentId(Uuid::new_v4());
        let mut s = fold(&[run_created(run, None, &["review"])]);
        assert_eq!(s.status(), SessionStatus::Idle);

        s.apply(&step_started(run, 0, "review", step_agent, 200));
        assert_eq!(s.status(), SessionStatus::Running);
        assert_eq!(s.owner_of(step_agent), Some(run));

        s.apply(&ended(
            step_agent,
            RecordedEnd::Concluded {
                output: serde_json::json!({"outcome": "success"}),
            },
            300,
        ));
        let w = run_state(&s, run);
        assert_eq!(w.run.steps[0].status, StepStatus::Concluded);
        // Between steps the run is still running; the boundary decides next.
        assert_eq!(s.status(), SessionStatus::Running);

        s.apply(&SessionEvent::Runner {
            id: run,
            at_ms: 400,
            event: RunnerEvent::RunFinished {
                output: serde_json::json!({"outcome": "success"}),
            },
        });
        assert_eq!(s.status(), SessionStatus::Finished);
    }

    #[test]
    fn a_parked_step_parks_the_run() {
        let run = RunnerId(Uuid::new_v4());
        let step_agent = AgentId(Uuid::new_v4());
        let s = fold(&[
            run_created(run, None, &["review"]),
            step_started(run, 0, "review", step_agent, 200),
            ended(step_agent, RecordedEnd::Asked, 300),
        ]);
        assert_eq!(s.status(), SessionStatus::AwaitingInput);
        // The step entry is still running — an ask is a park, not a boundary.
        assert_eq!(run_state(&s, run).run.steps[0].status, StepStatus::Running);
    }

    #[test]
    fn a_failed_step_fails_the_run() {
        let run = RunnerId(Uuid::new_v4());
        let step_agent = AgentId(Uuid::new_v4());
        let s = fold(&[
            run_created(run, None, &["review"]),
            step_started(run, 0, "review", step_agent, 200),
            ended(
                step_agent,
                RecordedEnd::Failed {
                    error: "no result".into(),
                },
                300,
            ),
        ]);
        assert_eq!(
            s.status(),
            SessionStatus::Failed {
                reason: "no result".into()
            }
        );
        let w = run_state(&s, run);
        assert_eq!(w.run.steps[0].status, StepStatus::Failed);
        assert_eq!(w.run.status, WorkflowRunStatus::Failed);
    }

    #[test]
    fn a_cancelled_step_suspends_the_run() {
        let run = RunnerId(Uuid::new_v4());
        let step_agent = AgentId(Uuid::new_v4());
        let s = fold(&[
            run_created(run, None, &["review"]),
            step_started(run, 0, "review", step_agent, 200),
            SessionEvent::Runner {
                id: run,
                at_ms: 300,
                event: RunnerEvent::StepCancelled { index: 0 },
            },
        ]);
        assert_eq!(run_state(&s, run).run.status, WorkflowRunStatus::Suspended);
        // Suspended rests the session — a person decides between retry and
        // abandonment.
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    #[test]
    fn a_nested_run_never_moves_the_session_and_owes_its_parent() {
        let main = AgentId(Uuid::new_v4());
        let run = RunnerId(Uuid::new_v4());
        let step_agent = AgentId(Uuid::new_v4());
        let mut s = fold(&[
            main_created(main),
            run_created(run, Some(main), &["review"]),
            step_started(run, 0, "review", step_agent, 200),
        ]);
        assert_eq!(s.status(), SessionStatus::Idle);
        assert_eq!(s.depth_of(run), 1);

        s.apply(&SessionEvent::Runner {
            id: run,
            at_ms: 400,
            event: RunnerEvent::RunFinished {
                output: serde_json::json!({"outcome": "success"}),
            },
        });
        let w = run_state(&s, run);
        assert_eq!(w.run.status, WorkflowRunStatus::Finished);
        assert!(!w.notified);
        s.apply(&SessionEvent::Runner {
            id: run,
            at_ms: 500,
            event: RunnerEvent::Reported,
        });
        assert!(run_state(&s, run).notified);
    }

    #[test]
    fn a_step_agents_subagent_nests_under_the_run() {
        let run = RunnerId(Uuid::new_v4());
        let step_agent = AgentId(Uuid::new_v4());
        let sub = AgentId(Uuid::new_v4());
        let s = fold(&[
            run_created(run, None, &["review"]),
            step_started(run, 0, "review", step_agent, 200),
            sub_created(sub, step_agent, 300),
        ]);
        assert_eq!(s.owner_of(sub), Some(RunnerId::of_agent(sub)));
        assert_eq!(s.depth_of(RunnerId::of_agent(sub)), 1);
    }

    #[test]
    fn cancelling_a_run_cancels_its_step_and_fails_it() {
        let main = AgentId(Uuid::new_v4());
        let run = RunnerId(Uuid::new_v4());
        let step_agent = AgentId(Uuid::new_v4());
        let s = fold(&[
            main_created(main),
            run_created(run, Some(main), &["review"]),
            step_started(run, 0, "review", step_agent, 200),
            SessionEvent::Runner {
                id: run,
                at_ms: 300,
                event: RunnerEvent::Cancelled,
            },
        ]);
        let w = run_state(&s, run);
        assert_eq!(w.run.steps[0].status, StepStatus::Cancelled);
        assert_eq!(w.run.status, WorkflowRunStatus::Failed);
        assert_eq!(w.run.error.as_deref(), Some(CANCELLED_ERROR));
    }

    // -- the session's own facts ------------------------------------------

    #[test]
    fn usage_banks_per_agent_and_totals_across_them() {
        let one = UsageTotal {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        };
        let two = UsageTotal {
            input_tokens: 3,
            output_tokens: 1,
            ..Default::default()
        };
        let s = fold(&[
            SessionEvent::UsageRecorded {
                at_ms: 1,
                agent_id: "main".into(),
                usage_total: one,
            },
            SessionEvent::UsageRecorded {
                at_ms: 2,
                agent_id: "abc".into(),
                usage_total: two,
            },
        ]);
        let total = s.session_usage_total();
        assert_eq!((total.input_tokens, total.output_tokens), (13, 6));
    }

    #[test]
    fn a_rename_moves_only_the_name_and_never_invents_a_spec() {
        let mut s = fold(&[SessionEvent::Renamed {
            name: "ghost".into(),
        }]);
        assert!(s.spec.is_none());
        s.apply(&SessionEvent::SpecRecorded {
            spec: Box::new(crate::sessions::session_actor::testing::actor_spec_fixture()),
        });
        s.apply(&SessionEvent::Renamed {
            name: "real".into(),
        });
        assert_eq!(s.spec.unwrap().name.as_deref(), Some("real"));
    }

    // -- totality ----------------------------------------------------------

    #[test]
    fn events_for_unknown_or_mismatched_runners_change_nothing() {
        let main = AgentId(Uuid::new_v4());
        let baseline = fold(&[main_created(main)]);
        let mut s = baseline.clone();
        // A step event addressed to a conversation, an end from an agent
        // nobody owns, a report for a runner that is not there.
        s.apply(&SessionEvent::Runner {
            id: RunnerId::of_agent(main),
            at_ms: 1,
            event: RunnerEvent::StepCancelled { index: 0 },
        });
        s.apply(&ended(AgentId(Uuid::new_v4()), RecordedEnd::Stopped, 2));
        s.apply(&SessionEvent::Runner {
            id: RunnerId(Uuid::new_v4()),
            at_ms: 3,
            event: RunnerEvent::Reported,
        });
        assert_eq!(s, baseline);
    }

    #[test]
    fn the_root_is_the_runner_with_no_parent() {
        let main = AgentId(Uuid::new_v4());
        let sub = AgentId(Uuid::new_v4());
        let s = fold(&[main_created(main), sub_created(sub, main, 200)]);
        let (root, record) = s.root().unwrap();
        assert_eq!(root, RunnerId::of_agent(main));
        assert!(matches!(record.state, RunnerState::Main(_)));
    }
}

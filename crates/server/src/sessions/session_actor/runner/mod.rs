//! Runners: the units of work a session hosts, and the sub-state-machines
//! that run them.
//!
//! A session is one journal, one sandbox, and a tree of runners. A **runner**
//! wraps one unit of work — the session's own conversation
//! ([`MainAgentRunner`]), a fork of it ([`ForkAgentRunner`]), a delegated
//! task ([`SubAgentRunner`]), or a workflow run ([`WorkflowRunner`]) — owns
//! its slice of the folded state, and reacts to the lifecycle of the agents
//! that carry it out. Runners replace the component model: where a component
//! was a session-wide unit struct that could exist once, a runner is an entry
//! in [`state::SessionState::runners`], so a session holds as many of each
//! kind as its work created — which is what makes nesting (a subagent
//! spawning subagents, an agent invoking a workflow) structure instead of
//! special cases.
//!
//! Runners only ever **decide**. The fold in [`state`] is pure; the behavior
//! half here is pure too — a [`Runner`] is rebuilt from the state whenever
//! needed, holds identity and nothing folded, and returns events, actions and
//! repairs. The session actor is the only thing that acts: it performs what
//! runners ask for, journals what they decide, and folds what it journaled.

pub mod action;
pub mod deliver;
pub mod event;
mod fork_agent;
pub mod ids;
mod main_agent;
pub mod role;
pub mod state;
mod sub_agent;
mod workflow;

pub(crate) use fork_agent::ForkAgentRunner;
pub(crate) use main_agent::MainAgentRunner;
pub(crate) use sub_agent::SubAgentRunner;
pub(crate) use workflow::{WorkflowRunner, step_agent_id};

use crate::sessions::spec::SessionSpec;

use super::types::TurnEnd;
use action::{OutcomeDecision, Repair, RunnerAction};
use event::SessionEvent;
use ids::{AgentId, RunnerId};
use role::AgentRole;
use state::{ProvisionPhase, RunnerState, SessionState};

/// Deepest the combined runner tree may grow: every agent→child-runner edge
/// costs 1, whether the child is a subagent or a nested workflow run. One
/// budget, because the runaway it bounds — a machine creating workers in a
/// loop — does not care which kind of worker it creates. A node *at* this
/// depth cannot create.
pub const MAX_RUNNER_DEPTH: u32 = 4;

/// Cap on concurrently-live (non-terminal) workflow runs a session may hold,
/// so a loop of `invoke_workflow` calls fails fast instead of exhausting the
/// session's agents.
pub const MAX_LIVE_RUNS: usize = 8;

/// Cap on concurrently-active subagents when the caller's settings name none.
pub const DEFAULT_MAX_CONCURRENT_SUBAGENTS: u32 = 8;

/// Error recorded for work that was mid-run when the process died.
pub const INTERRUPTED_ERROR: &str = "interrupted by restart";

/// Error recorded for work someone stopped.
///
/// Its own wording rather than [`INTERRUPTED_ERROR`]'s, because this one
/// reaches a *model*: the parent reads it as the result of the child it is
/// waiting on, and "interrupted by restart" would have it reason about a
/// crash that never happened.
pub const STOPPED_ERROR: &str = "stopped before it finished";

/// Error a cancelled run reports to whoever asked for it.
pub const CANCELLED_ERROR: &str = "cancelled";

/// The uniform behavior every runner implements. Pure throughout: state and a
/// caller-stamped clock in, decisions out.
pub(crate) trait RunnerBehavior {
    /// What to journal for one of this runner's agents' turn ends, and whether
    /// a boundary follows. Unifies what used to be four parallel handlers —
    /// one per agent kind — each re-deciding the same five cases.
    fn on_outcome(
        &self,
        state: &SessionState,
        agent: AgentId,
        end: TurnEnd,
        now_ms: u64,
    ) -> OutcomeDecision;

    /// What this runner wants performed at a boundary: deliveries it owes
    /// upward, the next step it wants run, its own finish.
    fn actions(&self, state: &SessionState) -> Vec<RunnerAction>;

    /// The repairs this runner wants after a crash — discovered by asking
    /// every runner, so a kind added later cannot be forgotten the way a
    /// hand-maintained list forgot the fork's.
    fn repairs(&self, state: &SessionState) -> Vec<Repair>;

    /// Whether unloading now would lose work in flight.
    fn busy(&self, state: &SessionState) -> bool;

    /// What to journal for stopping this runner's agent `agent`, or `None` if
    /// it is not working — nothing to stop is not a failure.
    fn stop_event(&self, state: &SessionState, agent: AgentId, now_ms: u64)
    -> Option<SessionEvent>;

    /// Everything kind-specific about running agent `agent` under this
    /// runner. `None` when the role cannot be resolved — an agent this runner
    /// does not know, or a spec that cannot host it.
    fn role(&self, spec: &SessionSpec, state: &SessionState, agent: AgentId) -> Option<AgentRole>;
}

/// One runner, dispatchable. An enum rather than `Box<dyn>`: the set of kinds
/// is closed, and a new one added here walks the compiler through every seam.
pub(crate) enum Runner {
    Main(MainAgentRunner),
    Sub(SubAgentRunner),
    Fork(ForkAgentRunner),
    Workflow(WorkflowRunner),
}

impl Runner {
    /// The runner behind `id`, rebuilt from the state that knows what it is.
    /// `None` for an id the state does not hold.
    pub(crate) fn of(id: RunnerId, state: &SessionState) -> Option<Runner> {
        Some(match &state.record(id)?.state {
            RunnerState::Main(_) => Runner::Main(MainAgentRunner { id }),
            RunnerState::Sub(_) => Runner::Sub(SubAgentRunner { id }),
            RunnerState::Fork(_) => Runner::Fork(ForkAgentRunner { id }),
            RunnerState::Workflow(_) => Runner::Workflow(WorkflowRunner { id }),
        })
    }

    /// The runner that owns `agent`, if any.
    pub(crate) fn owner_of(agent: AgentId, state: &SessionState) -> Option<Runner> {
        Runner::of(state.owner_of(agent)?, state)
    }

    pub(crate) fn id(&self) -> RunnerId {
        match self {
            Runner::Main(r) => r.id,
            Runner::Sub(r) => r.id,
            Runner::Fork(r) => r.id,
            Runner::Workflow(r) => r.id,
        }
    }
}

impl RunnerBehavior for Runner {
    fn on_outcome(
        &self,
        state: &SessionState,
        agent: AgentId,
        end: TurnEnd,
        now_ms: u64,
    ) -> OutcomeDecision {
        match self {
            Runner::Main(r) => r.on_outcome(state, agent, end, now_ms),
            Runner::Sub(r) => r.on_outcome(state, agent, end, now_ms),
            Runner::Fork(r) => r.on_outcome(state, agent, end, now_ms),
            Runner::Workflow(r) => r.on_outcome(state, agent, end, now_ms),
        }
    }

    fn actions(&self, state: &SessionState) -> Vec<RunnerAction> {
        match self {
            Runner::Main(r) => r.actions(state),
            Runner::Sub(r) => r.actions(state),
            Runner::Fork(r) => r.actions(state),
            Runner::Workflow(r) => r.actions(state),
        }
    }

    fn repairs(&self, state: &SessionState) -> Vec<Repair> {
        match self {
            Runner::Main(r) => r.repairs(state),
            Runner::Sub(r) => r.repairs(state),
            Runner::Fork(r) => r.repairs(state),
            Runner::Workflow(r) => r.repairs(state),
        }
    }

    fn busy(&self, state: &SessionState) -> bool {
        match self {
            Runner::Main(r) => r.busy(state),
            Runner::Sub(r) => r.busy(state),
            Runner::Fork(r) => r.busy(state),
            Runner::Workflow(r) => r.busy(state),
        }
    }

    fn stop_event(
        &self,
        state: &SessionState,
        agent: AgentId,
        now_ms: u64,
    ) -> Option<SessionEvent> {
        match self {
            Runner::Main(r) => r.stop_event(state, agent, now_ms),
            Runner::Sub(r) => r.stop_event(state, agent, now_ms),
            Runner::Fork(r) => r.stop_event(state, agent, now_ms),
            Runner::Workflow(r) => r.stop_event(state, agent, now_ms),
        }
    }

    fn role(&self, spec: &SessionSpec, state: &SessionState, agent: AgentId) -> Option<AgentRole> {
        match self {
            Runner::Main(r) => r.role(spec, state, agent),
            Runner::Sub(r) => r.role(spec, state, agent),
            Runner::Fork(r) => r.role(spec, state, agent),
            Runner::Workflow(r) => r.role(spec, state, agent),
        }
    }
}

/// Every runner the state holds.
fn all(state: &SessionState) -> impl Iterator<Item = Runner> + '_ {
    state.runners.keys().filter_map(|id| Runner::of(*id, state))
}

/// Whether the boundary may start anything. Not while the sandbox is being
/// built or broken — the status those phases produce starts nothing, which is
/// what lets work queue behind a create instead of addressing a runtime that
/// does not exist — and never again after a terminal failure.
pub(crate) fn boundary_open(state: &SessionState) -> bool {
    state.fatal.is_none()
        && !matches!(
            state.provisioning.phase,
            ProvisionPhase::InFlight | ProvisionPhase::Failed { .. }
        )
}

/// Everything every runner wants performed at this boundary. Deliveries
/// first, across all runners, then step starts: a result owed to an agent
/// must be in its queue before anything decides to run it.
pub(crate) fn boundary_actions(state: &SessionState) -> Vec<RunnerAction> {
    if !boundary_open(state) {
        return Vec::new();
    }
    let mut deliveries = Vec::new();
    let mut starts = Vec::new();
    for runner in all(state) {
        for action in runner.actions(state) {
            match action {
                RunnerAction::Deliver { .. } => deliveries.push(action),
                RunnerAction::StartStep { .. }
                | RunnerAction::FinishRun { .. }
                | RunnerAction::FailRun { .. } => starts.push(action),
            }
        }
    }
    deliveries.extend(starts);
    deliveries
}

/// Whether any runner — or the sandbox create itself — has work in flight.
/// The OR-fold that gates an offload: asked of every runner, so a kind added
/// later makes itself heard.
pub(crate) fn session_busy(state: &SessionState) -> bool {
    matches!(state.provisioning.phase, ProvisionPhase::InFlight)
        || all(state).any(|r| r.busy(state))
}

/// Every repair the session wants after a load: the sandbox's own, then each
/// runner's, discovered by iteration.
pub(crate) fn load_repairs(state: &SessionState) -> Vec<Repair> {
    let mut repairs = Vec::new();
    if matches!(
        state.provisioning.phase,
        ProvisionPhase::InFlight | ProvisionPhase::Failed { .. }
    ) && state.fatal.is_none()
    {
        repairs.push(Repair::Provision);
    }
    for runner in all(state) {
        repairs.extend(runner.repairs(state));
    }
    repairs
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub(crate) mod testkit {
    //! Hand-built states for behavior tests: the same events the fold tests
    //! speak, packaged so a runner test reads as a scenario.

    use super::event::{RecordedEnd, RunnerArgs, RunnerEvent, SessionEvent};
    use super::ids::{AgentId, RunnerId};
    use super::state::SessionState;
    use crate::sessions::spec::AgentSettings;
    use crate::sessions::workflow::{TransitionSpec, WorkflowRunSpec, WorkflowStepSpec};
    use uuid::Uuid;

    pub(crate) fn fold(events: &[SessionEvent]) -> SessionState {
        let mut state = SessionState::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    pub(crate) fn settings() -> AgentSettings {
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

    pub(crate) fn step(name: &str, transitions: Vec<TransitionSpec>) -> WorkflowStepSpec {
        WorkflowStepSpec {
            name: name.into(),
            agent: "preset".into(),
            prompt: format!("Do {name}."),
            outcomes: crate::sessions::workflow::default_outcomes(),
            fields: Vec::new(),
            interactive: false,
            transitions,
            settings: settings(),
        }
    }

    /// A transition taken for any of `values`, or a catch-all when empty.
    pub(crate) fn to(target: &str, values: &[&str]) -> TransitionSpec {
        TransitionSpec {
            to: target.into(),
            when: (!values.is_empty()).then(|| {
                horsie_models::workflow::OutcomeFilter::In(horsie_models::workflow::OutcomeIn {
                    values: values.iter().map(|v| (*v).to_string()).collect(),
                })
            }),
        }
    }

    pub(crate) fn graph(start: &str, steps: Vec<WorkflowStepSpec>) -> WorkflowRunSpec {
        WorkflowRunSpec {
            workflow: "fix-bug".into(),
            start: start.into(),
            steps,
            input: "the build is red".into(),
            max_steps: 100,
        }
    }

    pub(crate) fn main_created(agent: AgentId) -> SessionEvent {
        SessionEvent::Runner {
            id: RunnerId::of_agent(agent),
            at_ms: 100,
            event: RunnerEvent::Created {
                parent: None,
                args: Box::new(RunnerArgs::Main),
            },
        }
    }

    pub(crate) fn sub_created(id: AgentId, parent: AgentId, at_ms: u64) -> SessionEvent {
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

    pub(crate) fn fork_created(id: AgentId, parent: AgentId, at_ms: u64) -> SessionEvent {
        SessionEvent::Runner {
            id: RunnerId::of_agent(id),
            at_ms,
            event: RunnerEvent::Created {
                parent: Some(parent),
                args: Box::new(RunnerArgs::Fork {
                    source_seq: 42,
                    mode: crate::sessions::forks::ForkMode::Copy,
                    message: "try another way".into(),
                }),
            },
        }
    }

    pub(crate) fn run_created(
        id: RunnerId,
        parent: Option<AgentId>,
        graph: WorkflowRunSpec,
    ) -> SessionEvent {
        SessionEvent::Runner {
            id,
            at_ms: 100,
            event: RunnerEvent::Created {
                parent,
                args: Box::new(RunnerArgs::Workflow { graph }),
            },
        }
    }

    pub(crate) fn step_started(
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

    pub(crate) fn ended(agent: AgentId, end: RecordedEnd, at_ms: u64) -> SessionEvent {
        SessionEvent::TurnEnded { at_ms, agent, end }
    }

    pub(crate) fn agent() -> AgentId {
        AgentId(Uuid::new_v4())
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
    use super::event::{RecordedEnd, RunnerEvent, SessionEvent};
    use super::testkit::*;
    use super::*;

    #[test]
    fn the_boundary_is_gated_by_the_sandbox_and_a_terminal_failure() {
        let run = RunnerId(uuid::Uuid::new_v4());
        let g = graph("triage", vec![step("triage", vec![])]);
        let mut state = fold(&[run_created(run, None, g)]);
        assert_eq!(
            boundary_actions(&state).len(),
            1,
            "a fresh run wants its first step"
        );

        state.apply(&SessionEvent::ProvisioningStarted { at_ms: 1 });
        assert!(
            boundary_actions(&state).is_empty(),
            "nothing starts mid-create"
        );
        state.apply(&SessionEvent::ProvisioningSucceeded { at_ms: 2 });
        assert_eq!(boundary_actions(&state).len(), 1);

        state.apply(&SessionEvent::SessionFailed {
            at_ms: 3,
            reason: "gone".into(),
        });
        assert!(
            boundary_actions(&state).is_empty(),
            "an unrecoverable session starts nothing"
        );
    }

    /// A result owed to an agent must be in its queue before anything decides
    /// to run it — deliveries come first, whatever map order says.
    #[test]
    fn deliveries_precede_step_starts_at_a_boundary() {
        let run = RunnerId(uuid::Uuid::new_v4());
        let step_agent = super::step_agent_id(run, 0);
        let sub = agent();
        let g = graph("triage", vec![step("triage", vec![to("triage", &[])])]);
        let state = fold(&[
            run_created(run, None, g),
            step_started(run, 0, "triage", step_agent, 200),
            sub_created(sub, step_agent, 250),
            ended(
                sub,
                RecordedEnd::Concluded {
                    output: "found".into(),
                },
                300,
            ),
            ended(
                step_agent,
                RecordedEnd::Concluded {
                    output: serde_json::json!({"outcome": "success"}),
                },
                400,
            ),
        ]);
        let actions = boundary_actions(&state);
        assert!(actions.len() >= 2, "{actions:?}");
        assert!(matches!(actions[0], RunnerAction::Deliver { .. }));
        assert!(matches!(actions[1], RunnerAction::StartStep { .. }));
    }

    #[test]
    fn busy_hears_every_runner_and_the_create() {
        let main = agent();
        let mut state = fold(&[main_created(main)]);
        assert!(!session_busy(&state));
        state.apply(&SessionEvent::ProvisioningStarted { at_ms: 1 });
        assert!(
            session_busy(&state),
            "a create in flight refuses an offload"
        );
        state.apply(&SessionEvent::ProvisioningSucceeded { at_ms: 2 });
        assert!(!session_busy(&state));
        let sub = agent();
        state.apply(&sub_created(sub, main, 100));
        assert!(
            session_busy(&state),
            "a running subagent refuses an offload"
        );
        state.apply(&ended(
            sub,
            RecordedEnd::Concluded {
                output: "done".into(),
            },
            200,
        ));
        assert!(!session_busy(&state));
    }

    /// Repairs are discovered by asking every runner — a kind added later
    /// cannot be forgotten the way a hand-maintained list forgot the fork's.
    #[test]
    fn load_repairs_cover_the_create_the_subs_the_forks_and_the_runs() {
        let main = agent();
        let sub = agent();
        let fork = agent();
        let run = RunnerId(uuid::Uuid::new_v4());
        let g = graph("triage", vec![step("triage", vec![])]);
        let state = fold(&[
            SessionEvent::ProvisioningStarted { at_ms: 1 },
            main_created(main),
            sub_created(sub, main, 100),
            fork_created(fork, main, 100),
            run_created(run, Some(main), g),
        ]);
        let repairs = load_repairs(&state);
        assert!(repairs.contains(&Repair::Provision));
        assert!(repairs.contains(&Repair::FailInterruptedSub {
            id: RunnerId::of_agent(sub)
        }));
        assert!(repairs.contains(&Repair::ReseedFork {
            id: RunnerId::of_agent(fork)
        }));
        assert!(repairs.contains(&Repair::AdvanceRun { id: run }));
    }

    #[test]
    fn a_settled_session_wants_no_repairs() {
        let main = agent();
        let fork = agent();
        let mut state = fold(&[
            SessionEvent::ProvisioningStarted { at_ms: 1 },
            SessionEvent::ProvisioningSucceeded { at_ms: 2 },
            main_created(main),
            fork_created(fork, main, 100),
        ]);
        state.apply(&SessionEvent::Runner {
            id: RunnerId::of_agent(fork),
            at_ms: 150,
            event: RunnerEvent::ForkSeeded,
        });
        assert!(load_repairs(&state).is_empty());
    }
}

/// Whether `runner` sits somewhere below `below` in the tree — its parent
/// chain of agents passes through that agent.
pub(crate) fn descends_through(state: &SessionState, runner: RunnerId, below: AgentId) -> bool {
    let mut current = runner;
    // Bounded like `depth_of`'s walk: a cycle cannot be journaled because a
    // child is always created after its parent's agent.
    for _ in 0..=state.runners.len() {
        let Some(parent) = state.runners.get(&current).and_then(|r| r.parent) else {
            return false;
        };
        if parent == below {
            return true;
        }
        match state.owner_of(parent) {
            Some(owner) if owner != current => current = owner,
            _ => return false,
        }
    }
    false
}

/// What cancelling everything below `below` means: the agents whose actors to
/// cancel, and the events that record the cancellation. Stopping an agent
/// stops the work it delegated — a run with no caller left to hear its result
/// and a subagent whose parent is gone would otherwise run on, unwatched.
pub(crate) fn cascade_below(
    state: &SessionState,
    below: AgentId,
    now_ms: u64,
) -> (Vec<AgentId>, Vec<SessionEvent>) {
    let mut cancel = Vec::new();
    let mut events = Vec::new();
    for (id, record) in &state.runners {
        if !descends_through(state, *id, below) {
            continue;
        }
        match &record.state {
            state::RunnerState::Workflow(w) if !w.run.status.is_terminal() => {
                if let Some(agent) = w.run.current_agent() {
                    cancel.push(AgentId(agent));
                }
                events.push(SessionEvent::Runner {
                    id: *id,
                    at_ms: now_ms,
                    event: event::RunnerEvent::Cancelled,
                });
            }
            state::RunnerState::Sub(s) if matches!(s.phase, state::SubPhase::Running { .. }) => {
                cancel.push(AgentId(id.0));
                events.push(SessionEvent::TurnEnded {
                    at_ms: now_ms,
                    agent: AgentId(id.0),
                    end: event::RecordedEnd::Failed {
                        error: STOPPED_ERROR.to_string(),
                    },
                });
            }
            state::RunnerState::Main(_)
            | state::RunnerState::Sub(_)
            | state::RunnerState::Fork(_)
            | state::RunnerState::Workflow(_) => {}
        }
    }
    (cancel, events)
}

/// Live (non-terminal) runs an agent invoked — the count [`MAX_LIVE_RUNS`]
/// bounds.
pub(crate) fn live_invoked_runs(state: &SessionState) -> usize {
    state
        .runners
        .values()
        .filter(|record| {
            record.parent.is_some()
                && matches!(
                    &record.state,
                    state::RunnerState::Workflow(w) if !w.run.status.is_terminal()
                )
        })
        .count()
}

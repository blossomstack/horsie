//! Which session event reaches which agent's log, and as what.
//!
//! The session actor still owns every one of these — it decides them, journals
//! them, and folds them into its own state. This module only answers "who needs
//! to see it", so that a viewer reading one agent's log sees everything that
//! happened to that agent without a second stream to reconcile against.
//!
//! A pure function with a table of cases, in its own file for the same reason
//! `agent_log` is: the interesting part is the table, and a table wants tests
//! that can enumerate it.
//!
//! # What the runner shape bought
//!
//! The table this replaces routed 20 variants of one flat session vocabulary,
//! and three of its rows were only there because that vocabulary had no way to
//! say "this fact belongs to *that* conversation". A fork's turn boundary needed
//! its own event, `ForkTurnEnded`, or it would have closed the main agent's
//! turn. Now a turn boundary arrives inside [`SessionEvent::Runner`], addressed
//! to the runner that recorded it, and the fork's own agent falls out of the id
//! — the case stops existing rather than being handled.
//!
//! The three state-derived helpers below tell the same story: each of them used
//! to probe several registries, in an order that was itself load-bearing, and
//! each is now one lookup in [`SessionState`].

use super::ids::{AgentId, RunnerId};
use super::state::{SessionEvent, SessionState};
use super::{RunnerEvent, RunnerState, conversation, runtime, subagent, workflow};
use horsie_agentcore::{
    EmptyOutcome, FailedOutcome, ForkLifecycle, LifecycleEvent, RuntimeLifecycle, RuntimeStatus,
    SessionFailedLifecycle, StepLifecycle, SubAgentLifecycle, TurnEndedLifecycle, TurnOutcome,
};

/// One entry: whose log it belongs in, and what it says there.
type Entry = (AgentId, LifecycleEvent);

/// Every log this event belongs in, and what it becomes in each.
///
/// A list rather than one destination, because a fact can matter to more than
/// one reader: a subagent's result is both its own last word and news to the
/// parent that is waiting on it. Bookkeeping returns an empty list, which is how
/// "nothing a viewer would render" is said.
///
/// Takes the state as it stands *after* the event, for the same reason the old
/// one did: a step execution knows only its index, and a child's ending knows
/// only its runner. Both find their agent in the runner's own slice.
///
/// **There is no catch-all arm anywhere below, at any level.** A new
/// [`RunnerEvent`] — or a new variant inside one — is a compile error here until
/// somebody says where it goes, because the failure this table exists to prevent
/// is silent: the session journals the fact, folds it, and no client ever sees
/// it.
#[must_use]
pub fn route(event: &SessionEvent, state: &SessionState) -> Vec<Entry> {
    match event {
        // What the session *is*, not something that happened in it. A client
        // reads the spec off the session document, and a transcript entry would
        // only repeat it.
        SessionEvent::SpecRecorded { .. } => Vec::new(),
        SessionEvent::RunnerCreated {
            parent,
            state: born,
            ..
        } => created(*parent, born),
        // Derived bookkeeping. A runner's status is read off its own slice, so
        // by the time this is journaled the event that actually ended it —
        // `SubAgent(Concluded)`, `Workflow(Finished)` — has already been routed.
        // Routing this too would render the same ending twice.
        SessionEvent::RunnerEnded { .. } => Vec::new(),
        // A conversation a person removed. There is no log left to record it
        // in — the runner and its agents are gone from the state by the time
        // this is routed — and the reader who asked for it is watching the
        // session list, where the row simply stops being there.
        SessionEvent::RunnerDeleted { .. } => Vec::new(),
        // A name is read off the session document, not out of a transcript.
        // The same reason `SpecRecorded` routes nowhere: it is what the session
        // *is*, and an entry would only repeat it.
        SessionEvent::Renamed { .. } => Vec::new(),
        // Recorded by the agent itself, in its own log: the agent journals its
        // own `TurnBegan`. Routing it from here as well would say it twice.
        SessionEvent::AgentStarted { .. } => Vec::new(),
        // A total is a number on the agent document, not a line in a transcript.
        SessionEvent::UsageBanked { .. } => Vec::new(),
        SessionEvent::Runner { id, event, .. } => from_runner(*id, event, state),
    }
}

/// A runner coming into being, on the log of the agent that asked for it.
///
/// Matched on the slice rather than on the `kind` beside it — the two always
/// agree, and only the slice carries what the entry needs to say: a worker's
/// label, a fork's branch mode, and in both cases the *agent* id, which is what
/// a client navigates to.
///
/// `parent` is `None` for the session's own conversation and for the sandbox.
/// Neither is news to anybody: they are what the session is made of, and there
/// is no earlier log for them to appear in.
fn created(parent: Option<AgentId>, born: &RunnerState) -> Vec<Entry> {
    match born {
        // On the parent, not the child: a worker appearing is something that
        // happens *in* the asking agent's trajectory, and the child's own log
        // starts with its own work.
        RunnerState::SubAgent(worker) => parent
            .map(|p| {
                (
                    p,
                    LifecycleEvent::SubAgent(SubAgentLifecycle {
                        id: worker.agent.to_string(),
                        label: worker.label.clone(),
                        status: "running".into(),
                    }),
                )
            })
            .into_iter()
            .collect(),
        RunnerState::Conversation(fork) => fork_created(parent, fork),
        // A run has no lifecycle entry of its own to be announced with — a
        // reader follows it through its steps, and the first of those lands the
        // moment it starts. The sandbox is not something an agent was told
        // about either; its readiness is, and that is `Runtime(Started)`.
        RunnerState::Workflow(_) | RunnerState::Runtime(_) => Vec::new(),
    }
}

/// A fork, on the conversation it branched from — where the branch actually
/// happened, which for a fork of a fork is that fork and not the session's root.
///
/// A conversation with no branch point is the session's own, and appears in
/// nobody's log; neither does one whose creator was not recorded.
///
/// It never reaches the model — `prompt_messages` drops every lifecycle body —
/// which is deliberate: a fork is for the person reading, and telling the source
/// about it would disturb its prompt cache for nothing.
fn fork_created(parent: Option<AgentId>, fork: &conversation::State) -> Vec<Entry> {
    let (Some(parent), Some(branch)) = (parent, fork.seed.as_ref()) else {
        return Vec::new();
    };
    vec![(
        parent,
        LifecycleEvent::Forked(ForkLifecycle {
            id: fork.agent.to_string(),
            // Nothing yet: a fork names itself later, and the session list is
            // where a client reads the current name.
            title: None,
            mode: branch.mode.as_str().to_string(),
        }),
    )]
}

/// One runner's own event, routed by which runner recorded it.
///
/// This is the whole of what `ForkTurnEnded` used to buy: the runner is named
/// on the entry, so a boundary lands on the conversation that reached it rather
/// than on whatever the session considers its main log.
fn from_runner(runner: RunnerId, event: &RunnerEvent, state: &SessionState) -> Vec<Entry> {
    match event {
        RunnerEvent::Runtime(event) => from_runtime(event, state),
        RunnerEvent::Conversation(event) => from_conversation(runner, event, state),
        RunnerEvent::SubAgent(event) => from_subagent(runner, event, state),
        RunnerEvent::Workflow(event) => from_workflow(runner, event, state),
        // Bookkeeping, like `SubAgentRunning` was: a token total is a number on
        // the agent document, and the session banks it against a model. Named
        // rather than left to a catch-all so that the next arm added here has to
        // answer the same question this one did.
        RunnerEvent::Usage { .. } => Vec::new(),
    }
}

/// The sandbox, in the log a person reads when they open the session.
///
/// A terminal failure is the one that reaches *every* agent instead: it takes
/// the runtime away for good, and a resident worker or fork that never heard
/// would go on believing it may still start a turn. That is the only difference
/// `terminal` makes here, and it is why the two arms answer with different
/// lifecycle events rather than with the same one carrying a flag.
fn from_runtime(event: &runtime::Event, state: &SessionState) -> Vec<Entry> {
    let payload = match event {
        runtime::Event::Started => RuntimeLifecycle {
            status: RuntimeStatus::Acquiring(EmptyOutcome {}),
            detail: None,
        },
        // Still acquiring, now with the vendor's own account of why it is taking
        // as long as it is. The status is unchanged on purpose: this is the same
        // fact as the entry above, said with more of what is known.
        runtime::Event::Progress { detail } => RuntimeLifecycle {
            status: RuntimeStatus::Acquiring(EmptyOutcome {}),
            detail: Some(detail.clone()),
        },
        runtime::Event::Succeeded { .. } => RuntimeLifecycle {
            // Nothing to add: the runtime is up, which is the whole message. A
            // detail here would be narration of a wait that is over.
            status: RuntimeStatus::Ready(EmptyOutcome {}),
            detail: None,
        },
        runtime::Event::Failed {
            error,
            terminal: true,
        } => {
            return every_agent(
                state,
                &LifecycleEvent::SessionFailed(SessionFailedLifecycle {
                    reason: error.clone(),
                }),
            );
        }
        runtime::Event::Failed {
            error,
            terminal: false,
        } => RuntimeLifecycle {
            status: RuntimeStatus::Failed(EmptyOutcome {}),
            detail: Some(error.clone()),
        },
        // Handing the sandbox back is the session being put down, not something
        // that happened to an agent — every agent it could be told about has
        // already stopped. The shape this replaces had no event for it at all.
        runtime::Event::Released => return Vec::new(),
    };
    on_session(state, LifecycleEvent::Runtime(payload))
}

/// A turn boundary, on the conversation that reached it.
///
/// Four ways to end collapse into one entry carrying an outcome, so a consumer
/// asking "is the turn over" does not have to enumerate them.
fn from_conversation(
    runner: RunnerId,
    event: &conversation::Event,
    state: &SessionState,
) -> Vec<Entry> {
    let outcome = match event {
        conversation::Event::TurnEnded => TurnOutcome::Ended(EmptyOutcome {}),
        conversation::Event::TurnFailed { error } => TurnOutcome::Failed(FailedOutcome {
            error: error.clone(),
        }),
        conversation::Event::TurnStopped => TurnOutcome::Stopped(EmptyOutcome {}),
        conversation::Event::TurnInterrupted => TurnOutcome::Interrupted(EmptyOutcome {}),
        // Nothing a reader sees. `TurnBegan` and `Asked` are recorded by the
        // agent that decided them, in its own log, so routing them from here
        // would render the same fact twice. A branch point landing, or failing
        // to, changes nothing in the source's transcript — the fork's row in the
        // session list is where a reader watches that.
        conversation::Event::Started
        | conversation::Event::Seeded
        | conversation::Event::SeedFailed { .. }
        | conversation::Event::TurnBegan
        | conversation::Event::Asked => return Vec::new(),
    };
    conversation_agent(runner, state)
        .map(|agent| {
            (
                agent,
                LifecycleEvent::TurnEnded(TurnEndedLifecycle { outcome }),
            )
        })
        .into_iter()
        .collect()
}

/// A finished worker, on its parent *and* on itself.
///
/// On the parent because that is what a person has open while it waits, and the
/// label comes off the worker's slice — a bare uuid is not something a reader
/// can place.
///
/// On the child because a worker has a page of its own, and a page folds
/// `TurnBegan` as `Running` until the matching end. Left out, a finished worker
/// reads `RUNNING` for ever on its own page while its parent's says `completed`.
fn from_subagent(runner: RunnerId, event: &subagent::Event, state: &SessionState) -> Vec<Entry> {
    let (status, outcome) = match event {
        // The session reconciling its own tree. A viewer already sees the spawn.
        subagent::Event::Started => return Vec::new(),
        subagent::Event::Concluded { .. } => ("completed", TurnOutcome::Ended(EmptyOutcome {})),
        subagent::Event::Failed { error } => (
            "failed",
            TurnOutcome::Failed(FailedOutcome {
                error: error.clone(),
            }),
        ),
    };
    let Some(worker) = worker_slice(runner, state) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    if let Some(parent) = parent_key(runner, state) {
        entries.push((
            parent,
            LifecycleEvent::SubAgent(SubAgentLifecycle {
                id: worker.agent.to_string(),
                label: worker.label.clone(),
                status: status.into(),
            }),
        ));
    }
    entries.push((
        worker.agent,
        LifecycleEvent::TurnEnded(TurnEndedLifecycle { outcome }),
    ));
    entries
}

/// A run's progress, on the log of the step it is about.
///
/// A run has no log of its own — there is no agent to hold one — so every entry
/// here names a step agent, including the two about the run's own ending, which
/// land on whichever step ran last.
fn from_workflow(runner: RunnerId, event: &workflow::Event, state: &SessionState) -> Vec<Entry> {
    let Some(run) = run_slice(runner, state) else {
        return Vec::new();
    };
    let (index, status) = match event {
        // The one event that names its own agent: a step's start decides it, so
        // it rides on the entry rather than being looked up in the state the
        // entry produced.
        workflow::Event::StepStarted {
            index, step, agent, ..
        } => {
            return vec![(
                *agent,
                LifecycleEvent::Step(StepLifecycle {
                    index: *index,
                    // The name, not just the index: an index identifies the
                    // execution, the name is what a person recognises.
                    name: step.clone(),
                    status: "started".into(),
                }),
            )];
        }
        workflow::Event::StepConcluded { index, .. } => (*index, "concluded"),
        workflow::Event::StepFailed { index, .. } => (*index, "failed"),
        workflow::Event::StepCancelled { index } => (*index, "cancelled"),
        // The run's own end, recorded on whichever step last ran — there is no
        // other log to put it in.
        workflow::Event::Finished { .. } => match last_index(run) {
            Some(index) => (index, "run_finished"),
            None => return Vec::new(),
        },
        workflow::Event::Failed { .. } => match last_index(run) {
            Some(index) => (index, "run_failed"),
            None => return Vec::new(),
        },
    };
    let Some(step) = run.steps.get(index as usize) else {
        return Vec::new();
    };
    vec![(
        AgentId(step.agent),
        LifecycleEvent::Step(StepLifecycle {
            index,
            name: step.step.clone(),
            status: status.into(),
        }),
    )]
}

/// The log a person reads when they open the session: the root runner's agent,
/// or — when the root is a run, which has no agent of its own — the step in
/// flight, whose log is the only one there is.
///
/// This was a three-registry probe whose *order* was load-bearing: it asked the
/// fork roster, then the subagent forest, then the run log, and reversing two of
/// them made a fork of a fork read as a fork of a subagent. It is now one lookup
/// in `runners`, because a runner's kind is a field rather than an inference.
fn session_wide(state: &SessionState) -> Option<AgentId> {
    match &state.record(state.root)?.state {
        RunnerState::Conversation(c) => Some(c.agent),
        RunnerState::SubAgent(w) => Some(w.agent),
        RunnerState::Workflow(run) => run
            .steps
            .get(run.current()? as usize)
            .map(|s| AgentId(s.agent)),
        RunnerState::Runtime(_) => None,
    }
}

/// Whose log a child's news belongs in: the agent that created it.
///
/// This was `SubAgentParent` plus a `TreeOwner` lookup — one enum saying whether
/// the parent was another worker or "the session's main", and a second registry
/// resolving what "main" meant for a run. Both are gone: a runner records the
/// agent that created it, and that agent is the answer whatever kind of runner
/// it belongs to.
fn parent_key(child: RunnerId, state: &SessionState) -> Option<AgentId> {
    state.record(child)?.parent
}

/// One entry on every agent this session hosts. For a fact that changes what an
/// agent may *do*, as opposed to one it merely renders.
///
/// This was the session-wide agent, plus every node of the subagent forest, plus
/// every fork in the roster — three sources that each had to remember to be
/// included, and a fork left out went on believing it could run on a runtime
/// that was gone. `agents` is now the one place an agent is registered, so
/// "every agent" is its keys.
fn every_agent(state: &SessionState, ev: &LifecycleEvent) -> Vec<Entry> {
    state
        .agents
        .keys()
        .map(|agent| (*agent, ev.clone()))
        .collect()
}

/// One entry in the session-wide log, or none when there is no log to put it in
/// — a run between steps has genuinely nowhere to record.
fn on_session(state: &SessionState, ev: LifecycleEvent) -> Vec<Entry> {
    session_wide(state)
        .map(|agent| (agent, ev))
        .into_iter()
        .collect()
}

fn conversation_agent(runner: RunnerId, state: &SessionState) -> Option<AgentId> {
    match &state.record(runner)?.state {
        RunnerState::Conversation(c) => Some(c.agent),
        RunnerState::SubAgent(_) | RunnerState::Workflow(_) | RunnerState::Runtime(_) => None,
    }
}

fn worker_slice(runner: RunnerId, state: &SessionState) -> Option<&subagent::State> {
    match &state.record(runner)?.state {
        RunnerState::SubAgent(w) => Some(w),
        RunnerState::Conversation(_) | RunnerState::Workflow(_) | RunnerState::Runtime(_) => None,
    }
}

fn run_slice(runner: RunnerId, state: &SessionState) -> Option<&workflow::State> {
    match &state.record(runner)?.state {
        RunnerState::Workflow(run) => Some(run.as_ref()),
        RunnerState::Conversation(_) | RunnerState::SubAgent(_) | RunnerState::Runtime(_) => None,
    }
}

/// The last execution's index, which is where a run's own ending is recorded.
fn last_index(run: &workflow::State) -> Option<u32> {
    run.steps
        .len()
        .checked_sub(1)
        .and_then(|i| u32::try_from(i).ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::sessions::forks::ForkMode;
    use crate::sessions::runners::action::Branch;
    use crate::sessions::runners::ids::{RunnerKind, RunnerStatus};
    use crate::sessions::spec::SessionSpec;

    fn fold(events: &[SessionEvent]) -> SessionState {
        let mut state = SessionState::default();
        for event in events {
            state.apply(event);
        }
        state
    }

    fn kind_of(state: &RunnerState) -> RunnerKind {
        match state {
            RunnerState::Conversation(_) => RunnerKind::Conversation,
            RunnerState::SubAgent(_) => RunnerKind::SubAgent,
            RunnerState::Workflow(_) => RunnerKind::Workflow,
            RunnerState::Runtime(_) => RunnerKind::Runtime,
        }
    }

    fn created_event(id: RunnerId, parent: Option<AgentId>, born: RunnerState) -> SessionEvent {
        SessionEvent::RunnerCreated {
            id,
            kind: kind_of(&born),
            parent,
            state: Box::new(born),
            at_ms: 1,
        }
    }

    fn conversation_of(agent: AgentId, seed: Option<Branch>) -> RunnerState {
        RunnerState::Conversation(conversation::State {
            agent,
            seed,
            ..conversation::State::default()
        })
    }

    fn branch(source: AgentId) -> Branch {
        Branch {
            source,
            source_seq: 0,
            mode: ForkMode::Copy,
        }
    }

    fn worker_of(agent: AgentId, label: &str) -> RunnerState {
        RunnerState::SubAgent(subagent::State {
            agent,
            label: label.into(),
            ..subagent::State::default()
        })
    }

    fn run_of(run: RunnerId) -> RunnerState {
        RunnerState::Workflow(Box::new(workflow::State {
            run,
            ..workflow::State::default()
        }))
    }

    fn on(runner: RunnerId, event: RunnerEvent) -> SessionEvent {
        SessionEvent::Runner {
            id: runner,
            event: Box::new(event),
            at_ms: 1,
        }
    }

    fn step_started(index: u32, step: &str, agent: AgentId) -> RunnerEvent {
        RunnerEvent::Workflow(workflow::Event::StepStarted {
            index,
            step: step.into(),
            agent,
            attempt: 1,
            from: None,
            via: None,
            input: "in".into(),
        })
    }

    /// A session whose root is a plain conversation, with its sandbox recorded.
    /// The shape almost every routing question is asked against.
    struct Conv {
        root: RunnerId,
        main: AgentId,
        runtime: RunnerId,
        log: Vec<SessionEvent>,
    }

    fn conversation_session() -> Conv {
        let root = RunnerId::new_v4();
        let main = AgentId::new_v4();
        let runtime = RunnerId::new_v4();
        let log = vec![
            created_event(root, None, conversation_of(main, None)),
            SessionEvent::AgentStarted {
                runner: root,
                agent: main,
            },
            created_event(
                runtime,
                None,
                RunnerState::Runtime(runtime::State::default()),
            ),
        ];
        Conv {
            root,
            main,
            runtime,
            log,
        }
    }

    /// A session whose root is a run, with one step in flight.
    struct Run {
        root: RunnerId,
        step: AgentId,
        runtime: RunnerId,
        log: Vec<SessionEvent>,
    }

    fn run_session() -> Run {
        let root = RunnerId::new_v4();
        let step = AgentId::new_v4();
        let runtime = RunnerId::new_v4();
        let log = vec![
            created_event(root, None, run_of(root)),
            on(root, step_started(0, "review", step)),
            SessionEvent::AgentStarted {
                runner: root,
                agent: step,
            },
            created_event(
                runtime,
                None,
                RunnerState::Runtime(runtime::State::default()),
            ),
        ];
        Run {
            root,
            step,
            runtime,
            log,
        }
    }

    /// Every variant, listed by hand, against a session that really contains
    /// each runner they name.
    ///
    /// The list is the runtime half of the guard: it asserts that a variant a
    /// reader should see actually reaches a log. The compile-time half is
    /// [`routes_nowhere`] below, which mirrors `route`'s table exhaustively —
    /// so a new `RunnerEvent` arm fails to build here as well as in `route`,
    /// and somebody has to decide twice.
    fn every_variant(world: &World) -> Vec<SessionEvent> {
        vec![
            SessionEvent::SpecRecorded {
                spec: Box::new(SessionSpec::for_vendor("local")),
            },
            created_event(
                RunnerId::new_v4(),
                None,
                conversation_of(AgentId::new_v4(), None),
            ),
            created_event(
                RunnerId::new_v4(),
                Some(world.main),
                conversation_of(AgentId::new_v4(), Some(branch(world.main))),
            ),
            created_event(
                RunnerId::new_v4(),
                Some(world.main),
                worker_of(AgentId::new_v4(), "helper"),
            ),
            created_event(RunnerId::new_v4(), Some(world.main), run_of(world.run)),
            created_event(
                RunnerId::new_v4(),
                None,
                RunnerState::Runtime(runtime::State::default()),
            ),
            SessionEvent::RunnerEnded {
                id: world.sub,
                status: RunnerStatus::Done,
                at_ms: 1,
            },
            SessionEvent::RunnerDeleted { id: world.fork },
            SessionEvent::Renamed {
                name: "the flake".into(),
            },
            SessionEvent::AgentStarted {
                runner: world.root,
                agent: world.main,
            },
            SessionEvent::UsageBanked {
                model: "sonnet".into(),
                spent: crate::agent_loop::UsageTotal::default(),
            },
            on(world.runtime, RunnerEvent::Runtime(runtime::Event::Started)),
            on(
                world.runtime,
                RunnerEvent::Runtime(runtime::Event::Progress {
                    detail: "the machine is booting".into(),
                }),
            ),
            on(
                world.runtime,
                RunnerEvent::Runtime(runtime::Event::Succeeded { at_ms: 1 }),
            ),
            on(
                world.runtime,
                RunnerEvent::Runtime(runtime::Event::Failed {
                    error: "no capacity".into(),
                    terminal: false,
                }),
            ),
            on(
                world.runtime,
                RunnerEvent::Runtime(runtime::Event::Failed {
                    error: "the sandbox is gone".into(),
                    terminal: true,
                }),
            ),
            on(
                world.runtime,
                RunnerEvent::Runtime(runtime::Event::Released),
            ),
            on(
                world.root,
                RunnerEvent::Conversation(conversation::Event::Started),
            ),
            on(
                world.root,
                RunnerEvent::Conversation(conversation::Event::Seeded),
            ),
            on(
                world.root,
                RunnerEvent::Conversation(conversation::Event::SeedFailed {
                    error: "the copy failed".into(),
                }),
            ),
            on(
                world.root,
                RunnerEvent::Conversation(conversation::Event::TurnBegan),
            ),
            on(
                world.root,
                RunnerEvent::Conversation(conversation::Event::Asked),
            ),
            on(
                world.root,
                RunnerEvent::Conversation(conversation::Event::TurnEnded),
            ),
            on(
                world.root,
                RunnerEvent::Conversation(conversation::Event::TurnFailed {
                    error: "boom".into(),
                }),
            ),
            on(
                world.root,
                RunnerEvent::Conversation(conversation::Event::TurnStopped),
            ),
            on(
                world.root,
                RunnerEvent::Conversation(conversation::Event::TurnInterrupted),
            ),
            on(world.sub, RunnerEvent::SubAgent(subagent::Event::Started)),
            on(
                world.sub,
                RunnerEvent::SubAgent(subagent::Event::Concluded {
                    output: "done".into(),
                }),
            ),
            on(
                world.sub,
                RunnerEvent::SubAgent(subagent::Event::Failed { error: "no".into() }),
            ),
            on(world.run, step_started(0, "review", world.step)),
            on(
                world.run,
                RunnerEvent::Workflow(workflow::Event::StepConcluded {
                    index: 0,
                    output: serde_json::Value::Null,
                }),
            ),
            on(
                world.run,
                RunnerEvent::Workflow(workflow::Event::StepFailed {
                    index: 0,
                    error: "no".into(),
                }),
            ),
            on(
                world.run,
                RunnerEvent::Workflow(workflow::Event::StepCancelled { index: 0 }),
            ),
            on(
                world.run,
                RunnerEvent::Workflow(workflow::Event::Finished {
                    output: serde_json::Value::Null,
                }),
            ),
            on(
                world.run,
                RunnerEvent::Workflow(workflow::Event::Failed { error: "no".into() }),
            ),
            on(
                world.root,
                RunnerEvent::Usage {
                    agent: world.main,
                    model: "sonnet".into(),
                    spent: crate::agent_loop::UsageTotal::default(),
                },
            ),
        ]
    }

    /// Whether this event is deliberately invisible.
    ///
    /// **Exhaustive, and that is the whole point.** A new arm anywhere in the
    /// `SessionEvent`/`RunnerEvent` tree stops this compiling, so nobody can add
    /// one without stating whether a reader should see it — the failure mode
    /// otherwise is silent, and no other test would catch it.
    fn routes_nowhere(event: &SessionEvent) -> bool {
        match event {
            SessionEvent::SpecRecorded { .. }
            | SessionEvent::RunnerEnded { .. }
            | SessionEvent::RunnerDeleted { .. }
            | SessionEvent::Renamed { .. }
            | SessionEvent::AgentStarted { .. }
            | SessionEvent::UsageBanked { .. } => true,
            SessionEvent::RunnerCreated { parent, state, .. } => match state.as_ref() {
                RunnerState::SubAgent(_) => parent.is_none(),
                RunnerState::Conversation(c) => parent.is_none() || c.seed.is_none(),
                RunnerState::Workflow(_) | RunnerState::Runtime(_) => true,
            },
            SessionEvent::Runner { event, .. } => match event.as_ref() {
                RunnerEvent::Usage { .. } => true,
                RunnerEvent::Runtime(e) => match e {
                    runtime::Event::Released => true,
                    runtime::Event::Started
                    | runtime::Event::Progress { .. }
                    | runtime::Event::Succeeded { .. }
                    | runtime::Event::Failed { .. } => false,
                },
                RunnerEvent::Conversation(e) => match e {
                    conversation::Event::Started
                    | conversation::Event::Seeded
                    | conversation::Event::SeedFailed { .. }
                    | conversation::Event::TurnBegan
                    | conversation::Event::Asked => true,
                    conversation::Event::TurnEnded
                    | conversation::Event::TurnFailed { .. }
                    | conversation::Event::TurnStopped
                    | conversation::Event::TurnInterrupted => false,
                },
                RunnerEvent::SubAgent(e) => match e {
                    subagent::Event::Started => true,
                    subagent::Event::Concluded { .. } | subagent::Event::Failed { .. } => false,
                },
                RunnerEvent::Workflow(e) => match e {
                    workflow::Event::StepStarted { .. }
                    | workflow::Event::StepConcluded { .. }
                    | workflow::Event::StepFailed { .. }
                    | workflow::Event::StepCancelled { .. }
                    | workflow::Event::Finished { .. }
                    | workflow::Event::Failed { .. } => false,
                },
            },
        }
    }

    /// One session holding every kind of runner, so each event can be asked
    /// against a state that really contains the runner it names.
    struct World {
        state: SessionState,
        root: RunnerId,
        main: AgentId,
        runtime: RunnerId,
        fork: RunnerId,
        sub: RunnerId,
        run: RunnerId,
        step: AgentId,
    }

    fn world() -> World {
        let base = conversation_session();
        let fork = RunnerId::new_v4();
        let fork_agent = AgentId::new_v4();
        let sub = RunnerId::new_v4();
        let sub_agent = AgentId::new_v4();
        let run = RunnerId::new_v4();
        let step = AgentId::new_v4();
        let mut log = base.log;
        log.extend([
            created_event(
                fork,
                Some(base.main),
                conversation_of(fork_agent, Some(branch(base.main))),
            ),
            SessionEvent::AgentStarted {
                runner: fork,
                agent: fork_agent,
            },
            created_event(sub, Some(base.main), worker_of(sub_agent, "helper")),
            SessionEvent::AgentStarted {
                runner: sub,
                agent: sub_agent,
            },
            created_event(run, Some(base.main), run_of(run)),
            on(run, step_started(0, "review", step)),
            SessionEvent::AgentStarted {
                runner: run,
                agent: step,
            },
        ]);
        World {
            state: fold(&log),
            root: base.root,
            main: base.main,
            runtime: base.runtime,
            fork,
            sub,
            run,
            step,
        }
    }

    /// Bookkeeping routes nowhere; everything else routes somewhere.
    ///
    /// Each event is asked against a session that really holds the runner it
    /// names, because most of the routings resolve their agent from the runner's
    /// slice rather than from the event.
    #[test]
    fn every_viewer_facing_event_has_a_destination() {
        let world = world();
        for event in every_variant(&world) {
            let entries = route(&event, &world.state);
            match routes_nowhere(&event) {
                true => assert!(entries.is_empty(), "{event:?} is bookkeeping"),
                false => assert!(!entries.is_empty(), "{event:?} has no destination"),
            }
        }
    }

    /// The vendor's own sentence reaches the log, which is the whole point of
    /// carrying one: "provisioning" for four minutes says nothing, while "the
    /// machine is resuming" is the answer to what a person is waiting for.
    ///
    /// Still `Acquiring`, and still no status change: narration describes the
    /// wait, it does not end it.
    #[test]
    fn a_vendors_words_reach_the_log_while_the_runtime_comes_up() {
        let conv = conversation_session();
        let state = fold(&conv.log);
        let entries = route(
            &on(
                conv.runtime,
                RunnerEvent::Runtime(runtime::Event::Progress {
                    detail: "the machine is booting".into(),
                }),
            ),
            &state,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, conv.main);
        let LifecycleEvent::Runtime(payload) = &entries[0].1 else {
            panic!("expected a Runtime entry, got {:?}", entries[0].1);
        };
        assert!(matches!(payload.status, RuntimeStatus::Acquiring(_)));
        assert_eq!(payload.detail.as_deref(), Some("the machine is booting"));
    }

    /// The two ends of a provisioning wait carry no detail, and that is not an
    /// oversight: "acquiring" is the start of a wait nothing is known about yet,
    /// and "ready" is the end of one, where the only news is that it is over.
    #[test]
    fn the_ends_of_a_provisioning_wait_have_nothing_to_say() {
        let conv = conversation_session();
        let state = fold(&conv.log);
        for event in [
            runtime::Event::Started,
            runtime::Event::Succeeded { at_ms: 2 },
        ] {
            let entries = route(
                &on(conv.runtime, RunnerEvent::Runtime(event.clone())),
                &state,
            );
            let Some((_, LifecycleEvent::Runtime(payload))) = entries.first() else {
                panic!("expected a Runtime entry for {event:?}");
            };
            assert_eq!(payload.detail, None, "{event:?}");
        }
    }

    /// A failure carries the vendor's reason, which is the one detail that was
    /// never dropped — and the one this variant must keep.
    #[test]
    fn a_failed_provision_reports_why() {
        let conv = conversation_session();
        let entries = route(
            &on(
                conv.runtime,
                RunnerEvent::Runtime(runtime::Event::Failed {
                    error: "no capacity in region".into(),
                    terminal: false,
                }),
            ),
            &fold(&conv.log),
        );
        let Some((_, LifecycleEvent::Runtime(payload))) = entries.first() else {
            panic!("expected a Runtime entry");
        };
        assert_eq!(payload.detail.as_deref(), Some("no capacity in region"));
    }

    /// A run that has not started its first step has no log at all yet, so a
    /// session-wide fact in that window is dropped rather than misfiled. This is
    /// the one case with genuinely nowhere to go: step agents are started per
    /// step, and a run has no conversation to fall back on.
    #[test]
    fn a_run_with_no_step_yet_has_nowhere_to_record() {
        let root = RunnerId::new_v4();
        let runtime_runner = RunnerId::new_v4();
        let state = fold(&[
            created_event(root, None, run_of(root)),
            created_event(
                runtime_runner,
                None,
                RunnerState::Runtime(runtime::State::default()),
            ),
        ]);
        assert!(
            route(
                &on(
                    runtime_runner,
                    RunnerEvent::Runtime(runtime::Event::Started)
                ),
                &state
            )
            .is_empty()
        );
    }

    /// A spawn and a terminal result both land on the *parent*: that is the log
    /// a person has open while a child is working. A nested child's parent is
    /// the worker above it, not the session's main agent — and that is now a
    /// field on the record rather than an enum plus a registry lookup.
    ///
    /// The terminal result *also* lands on the child — see
    /// [`a_finished_subagent_is_recorded_on_its_parent_and_on_itself`] — which
    /// is why only the parent's entry is asserted on here.
    #[test]
    fn a_subagents_news_is_recorded_on_its_parent() {
        let conv = conversation_session();
        let parent_runner = RunnerId::new_v4();
        let parent_agent = AgentId::new_v4();
        let child_runner = RunnerId::new_v4();
        let child_agent = AgentId::new_v4();
        let spawn = created_event(
            child_runner,
            Some(parent_agent),
            worker_of(child_agent, "child"),
        );
        let done = on(
            child_runner,
            RunnerEvent::SubAgent(subagent::Event::Concluded {
                output: "done".into(),
            }),
        );
        let mut log = conv.log;
        log.extend([
            created_event(
                parent_runner,
                Some(conv.main),
                worker_of(parent_agent, "parent"),
            ),
            SessionEvent::AgentStarted {
                runner: parent_runner,
                agent: parent_agent,
            },
            spawn.clone(),
            done.clone(),
        ]);
        let state = fold(&log);
        for event in [spawn, done] {
            let entries = route(&event, &state);
            assert_eq!(entries[0].0, parent_agent, "{event:?} on the parent");
            let LifecycleEvent::SubAgent(payload) = &entries[0].1 else {
                panic!("expected a SubAgent entry");
            };
            // The label comes off the worker's slice, so a terminal result names
            // the child rather than carrying a bare uuid.
            assert_eq!(payload.label, "child");
            assert_eq!(payload.id, child_agent.to_string());
        }
    }

    /// A run has no main agent, so its step entries go to the step's own log.
    /// They used to name `Main` and be dropped with a warning — every one of
    /// them, for the whole life of the run.
    #[test]
    fn a_runs_steps_are_recorded_on_the_step_that_ran() {
        let run = run_session();
        let state = fold(&run.log);
        let concluded = on(
            run.root,
            RunnerEvent::Workflow(workflow::Event::StepConcluded {
                index: 0,
                output: serde_json::Value::Null,
            }),
        );
        for event in [on(run.root, step_started(0, "review", run.step)), concluded] {
            let entries = route(&event, &state);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].0, run.step);
            let LifecycleEvent::Step(payload) = &entries[0].1 else {
                panic!("expected a Step entry");
            };
            // The name, not just the index: an index identifies the execution,
            // the name is what a person recognises.
            assert_eq!(payload.name, "review");
        }
    }

    /// A session-wide fact in a run has nowhere else to go either, so it lands
    /// on the step in flight rather than on a conversation that does not exist.
    #[test]
    fn a_session_wide_fact_in_a_run_lands_on_the_step_in_flight() {
        let run = run_session();
        let entries = route(
            &on(run.runtime, RunnerEvent::Runtime(runtime::Event::Started)),
            &fold(&run.log),
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, run.step);
    }

    /// The same fact in a conversation goes to the root conversation's agent.
    #[test]
    fn a_session_wide_fact_in_a_conversation_lands_on_main() {
        let conv = conversation_session();
        let entries = route(
            &on(conv.runtime, RunnerEvent::Runtime(runtime::Event::Started)),
            &fold(&conv.log),
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, conv.main);
    }

    /// A spawn under a run's step goes to that step, and it does so without any
    /// notion of "which tree owns this child": the step agent is the parent the
    /// record was created with.
    #[test]
    fn a_spawn_under_a_step_is_recorded_on_the_step() {
        let run = run_session();
        let child = RunnerId::new_v4();
        let spawn = created_event(
            child,
            Some(run.step),
            worker_of(AgentId::new_v4(), "helper"),
        );
        let mut log = run.log;
        log.push(spawn.clone());
        let entries = route(&spawn, &fold(&log));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, run.step);
    }

    /// Four ways a turn can end collapse into one lifecycle entry carrying an
    /// outcome, so a consumer asking "is the turn over" does not have to
    /// enumerate the ways it can be.
    #[test]
    fn every_way_a_turn_can_end_becomes_one_entry_kind() {
        let conv = conversation_session();
        let state = fold(&conv.log);
        let outcomes: Vec<TurnOutcome> = [
            conversation::Event::TurnEnded,
            conversation::Event::TurnFailed {
                error: "boom".into(),
            },
            conversation::Event::TurnStopped,
            conversation::Event::TurnInterrupted,
        ]
        .into_iter()
        .map(|e| {
            let routed = route(&on(conv.root, RunnerEvent::Conversation(e)), &state);
            match routed.into_iter().next().map(|(_, ev)| ev) {
                Some(LifecycleEvent::TurnEnded(t)) => t.outcome,
                other => panic!("expected TurnEnded, got {other:?}"),
            }
        })
        .collect();
        assert!(matches!(outcomes[0], TurnOutcome::Ended(_)));
        assert!(matches!(outcomes[1], TurnOutcome::Failed(_)));
        assert!(matches!(outcomes[2], TurnOutcome::Stopped(_)));
        assert!(matches!(outcomes[3], TurnOutcome::Interrupted(_)));
    }

    /// **A fork's boundary belongs to the fork, and now by construction.**
    ///
    /// The shape this replaces needed a whole separate event — `ForkTurnEnded` —
    /// because a turn boundary was a session-wide fact with no way to say whose
    /// turn it was; left out, a fork read `RUNNING` for ever, through reloads and
    /// restarts, because the status is derived from the journal. Here the entry
    /// names the runner, so there is no session-wide arm for it to fall into.
    #[test]
    fn a_forks_turn_boundary_lands_on_that_fork() {
        let world = world();
        let fork_agent = conversation_agent(world.fork, &world.state).expect("a fork");
        let entries = route(
            &on(
                world.fork,
                RunnerEvent::Conversation(conversation::Event::TurnEnded),
            ),
            &world.state,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, fork_agent);
        assert_ne!(entries[0].0, world.main, "it closed the root's turn");
        assert!(matches!(entries[0].1, LifecycleEvent::TurnEnded(_)));
    }

    /// A fork's turn can fail, and the reason has to survive the trip: its own
    /// page is the only place a reader will look for it.
    #[test]
    fn a_forks_failed_turn_carries_its_error() {
        let world = world();
        let entries = route(
            &on(
                world.fork,
                RunnerEvent::Conversation(conversation::Event::TurnFailed {
                    error: "the provider said no".into(),
                }),
            ),
            &world.state,
        );
        let LifecycleEvent::TurnEnded(ended) = &entries[0].1 else {
            panic!("expected a TurnEnded entry, got {:?}", entries[0].1);
        };
        let TurnOutcome::Failed(failed) = &ended.outcome else {
            panic!("expected a failed outcome, got {:?}", ended.outcome);
        };
        assert_eq!(failed.error, "the provider said no");
    }

    /// A terminal runtime failure takes every conversation in the session with
    /// it. A fork that never heard would go on believing it may start a turn, on
    /// a runtime that is gone for good.
    #[test]
    fn a_session_failure_reaches_the_forks_too() {
        let world = world();
        let fork_agent = conversation_agent(world.fork, &world.state).expect("a fork");
        let addressed: Vec<AgentId> = route(
            &on(
                world.runtime,
                RunnerEvent::Runtime(runtime::Event::Failed {
                    error: "the sandbox is gone".into(),
                    terminal: true,
                }),
            ),
            &world.state,
        )
        .into_iter()
        .map(|(agent, _)| agent)
        .collect();
        assert!(addressed.contains(&world.main), "{addressed:?}");
        assert!(addressed.contains(&fork_agent), "{addressed:?}");
        assert!(addressed.contains(&world.step), "{addressed:?}");
    }

    /// A finished subagent is news in two places: the parent that is waiting on
    /// it, and its own page, which reads `RUNNING` until its turn is closed.
    #[test]
    fn a_finished_subagent_is_recorded_on_its_parent_and_on_itself() {
        let world = world();
        let child = worker_slice(world.sub, &world.state)
            .expect("a worker")
            .agent;
        let entries = route(
            &on(
                world.sub,
                RunnerEvent::SubAgent(subagent::Event::Concluded {
                    output: "done".into(),
                }),
            ),
            &world.state,
        );
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert_eq!(entries[0].0, world.main);
        assert!(matches!(entries[0].1, LifecycleEvent::SubAgent(_)));
        assert_eq!(entries[1].0, child);
        assert!(matches!(entries[1].1, LifecycleEvent::TurnEnded(_)));
    }
}

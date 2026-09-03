//! The components an agent runs, and everything that is true of all of them.
//!
//! This file is the roster. It names every component exactly once, in four
//! places that are checked against each other at compile time: the struct that
//! holds them, the command routing, the event routing, and the list of durable
//! states. A component added later fails to build *here* — where it has to be
//! classified — rather than silently doing nothing.
//!
//! Nothing above this module names a component, and no component names
//! another. What happens next is decided by
//! [`AgentLoop::advance`](crate::agent_loop::boundary), which is the only
//! code that knows they all exist.
//!
//! One component, one module: [`provision`] the runtime and context setup they
//! all share, [`queue`] what this agent has accepted and how it becomes input,
//! [`turn`] one provider call and what an ending means, [`compaction`] folding
//! old history behind a summary boundary, [`timers`] and [`task_list`] the
//! tools whose state is the agent's own, [`seed`] branching and the
//! sub-session summary, [`usage`] what everything cost, [`reads`] the
//! questions that wake nothing, and [`log`] what others write into this
//! agent's transcript.

pub mod compaction;
pub mod log;
pub mod provision;
pub mod queue;
pub mod reads;
pub mod seed;
pub mod task_list;
pub mod timers;
pub mod turn;

use crate::agent_loop::prelude::*;
use horsie_actor::CommandEffect;
use serde::{Deserialize, Serialize};

pub(crate) use compaction::Compaction;
pub(crate) use log::LogWrites;
pub(crate) use provision::Provision;
pub(crate) use queue::Queue;
pub(crate) use reads::Reads;
pub(crate) use seed::Seeding;
pub(crate) use task_list::{TaskListPart, TaskLists};
pub(crate) use timers::{TimerState, Timers};
pub(crate) use turn::Turn;

/// One component's durable state, tagged by the component that owns it.
///
/// A list rather than a set of named fields on [`AgentState`]: adding a
/// component adds a variant here and a file, and touches nothing else. The
/// payload types are opaque — their fields are private to the file that owns
/// them, so nothing outside can read one without a method that file chose to
/// offer.
///
/// Serialized with an internal `kind` tag, because a snapshot outlives the
/// code that wrote it and positions in a list do not survive a component being
/// removed.
///
/// Not every component has one. Provisioning, compaction, seeding and the read
/// paths keep nothing durable of their own — a compaction boundary is a
/// transcript entry, not a field — and a component with no state simply has no
/// variant.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ComponentState {
    Timers(TimerState),
    TaskList(TaskListPart),
}

/// The `Part` implementations and the two polls, generated from one list so a
/// variant added above cannot be forgotten in any of them.
macro_rules! parts {
    ($($variant:ident($ty:ty)),+ $(,)?) => {
        impl ComponentState {
            /// This part as a sub session inherits it.
            pub(crate) fn carried(&self) -> Option<Self> {
                match self {
                    $(Self::$variant(part) => part.carried().map(Self::$variant),)+
                }
            }
        }

        /// One empty state per component, in registry order.
        pub(crate) fn default_parts() -> Vec<ComponentState> {
            vec![$(ComponentState::$variant(<$ty>::default()),)+]
        }

        $(impl Part for $ty {
            fn get(parts: &[ComponentState]) -> Option<&Self> {
                parts.iter().find_map(|p| match p {
                    ComponentState::$variant(part) => Some(part),
                    // `if let` in `find_map` shape: every other variant is
                    // some other component's, which is the whole point.
                    _other => None,
                })
            }

            fn get_mut(parts: &mut Vec<ComponentState>) -> Option<&mut Self> {
                if !parts.iter().any(|p| matches!(p, ComponentState::$variant(_))) {
                    parts.push(ComponentState::$variant(<$ty>::default()));
                }
                parts.iter_mut().find_map(|p| match p {
                    ComponentState::$variant(part) => Some(part),
                    _other => None,
                })
            }
        })+
    };
}

parts!(Timers(TimerState), TaskList(TaskListPart));

/// The component registry: every component an agent runs, held and named in
/// exactly one place.
///
/// The actor holds one of these and delegates wholesale — it never names a
/// component. Adding a component means editing this struct and its three
/// exhaustive routings below, all in this file, all checked at compile time:
/// a new command group or event variant that is not routed fails to build
/// *here*, where it has to be classified.
///
/// This is also the seam for building an agent's components from its spec
/// later: construction is centralized in [`AgentLoop::new`], so a spec-driven
/// variant changes this file and nothing above it.
pub(crate) struct AgentLoop {
    pub(crate) provision: Provision,
    pub(crate) timers: Timers,
    pub(crate) turn: Turn,
    pub(crate) queue: Queue,
    pub(crate) reads: Reads,
    pub(crate) log: LogWrites,
    pub(crate) seed: Seeding,
    pub(crate) task_lists: TaskLists,
    pub(crate) compaction: Compaction,
}

impl AgentLoop {
    pub fn new() -> Self {
        Self {
            provision: Provision,
            timers: Timers,
            turn: Turn::default(),
            queue: Queue::default(),
            reads: Reads,
            log: LogWrites,
            seed: Seeding,
            task_lists: TaskLists,
            compaction: Compaction,
        }
    }

    /// Route one command to the component that owns its group. Exhaustive:
    /// a command group added later fails to compile here.
    ///
    /// `Core` is deliberately absent — the agent's own decisions are the
    /// registry's, not any component's: see [`AgentLoop::advance`] and
    /// [`AgentLoop::cancel`] in [`super::boundary`].
    pub async fn handle(
        &mut self,
        cmd: AgentCommand,
        cx: &mut Cx<'_>,
    ) -> Option<CommandEffect<AgentDomainEvent>> {
        Some(match cmd {
            AgentCommand::Queue(c) => self.queue.handle(c, cx).await,
            AgentCommand::Run(RunCommand::StopHookDone { marker_seq, result }) => {
                self.stop_hook_returned(marker_seq, result, cx)
            }
            AgentCommand::Run(c) => self.turn.handle(c, cx).await,
            AgentCommand::Timer(c) => self.timers.handle(c, cx).await,
            AgentCommand::Read(c) => self.reads.handle(c, cx).await,
            AgentCommand::Log(c) => self.log.handle(c, cx).await,
            AgentCommand::Seed(c) => self.seed.handle(c, cx).await,
            AgentCommand::TaskList(c) => self.task_lists.handle(c, cx).await,
            AgentCommand::Provision(c) => self.provision.handle(c, cx).await,
            AgentCommand::Compaction(c) => self.compaction.handle(c, cx).await,
            AgentCommand::Core(CoreCommand::ToolReturned {
                marker_seq,
                tool_call_id,
                outcome,
            }) => {
                self.tool_returned(marker_seq, tool_call_id, outcome, cx)
                    .await
            }
            AgentCommand::Core(CoreCommand::Advance) => self.advance(cx).await,
            AgentCommand::Core(CoreCommand::Cancel { ack }) => self.cancel(ack, cx).await,
            AgentCommand::Core(CoreCommand::Shutdown) => return None,
        })
    }

    /// Toolboxes vended by the two genuine stateful components. Provisioning
    /// composes them ahead of runtime tools, so their names win collisions.
    pub(crate) fn toolboxes(
        &self,
        actor: horsie_actor::ActorRef<AgentCommand>,
    ) -> Vec<std::sync::Arc<dyn horsie_agentcore::Toolbox>> {
        [
            self.timers.toolbox(actor.clone()),
            self.task_lists.toolbox(actor),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    /// Ask each component, in registration order, to repair what a dead
    /// process left it holding. Nothing here decides what happens next: the
    /// actor advances once, afterwards, over the repaired state.
    pub async fn on_load(&mut self, cx: &mut Cx<'_>) {
        self.timers.on_load(cx).await;
    }
}

impl AgentLoop {
    /// The event-side twin of [`AgentLoop::handle`]: route each event to the
    /// component that owns it. Exhaustive the same way — an event added later
    /// fails to compile here, where it has to be classified.
    ///
    /// The one shared state-transition function: live handling, replay and
    /// every component's own fold-forward all go through here, so they cannot
    /// disagree. Associated rather than `&mut self` because a fold is pure and
    /// replay must not depend on which components an agent was instantiated
    /// with: any journal ever written stays readable, whatever a future spec
    /// chooses to run.
    pub fn apply(mut state: AgentState, event: AgentDomainEvent) -> AgentState {
        // `Seeded` carries a whole `AgentState`; storing that event inside the
        // state it installs would recursively duplicate the source snapshot.
        // The adopted history is already the durable account.
        let history_record =
            (!matches!(&event, AgentDomainEvent::Seeded { .. })).then(|| event.clone());
        match event {
            e @ (AgentDomainEvent::Seeded { .. } | AgentDomainEvent::SeedSummaryTaken { .. }) => {
                Seeding::apply(&mut state, e)
            }
            e @ (AgentDomainEvent::InputMessage { .. }
            | AgentDomainEvent::Received { .. }
            | AgentDomainEvent::Consumed { .. }
            | AgentDomainEvent::TurnBegan { .. }
            | AgentDomainEvent::AskRecorded { .. }
            | AgentDomainEvent::Parked { .. }) => Queue::apply(&mut state, e),
            e @ (AgentDomainEvent::MessageComplete { .. }
            | AgentDomainEvent::MessageAborted { .. }
            | AgentDomainEvent::ToolComplete { .. }
            | AgentDomainEvent::RunComplete { .. }
            | AgentDomainEvent::RunAborted { .. }
            | AgentDomainEvent::RunCancelled { .. }
            | AgentDomainEvent::Nudged { .. }) => Turn::apply(&mut state, e),
            e @ (AgentDomainEvent::HookRan { .. } | AgentDomainEvent::LifecycleRecorded { .. }) => {
                LogWrites::apply(&mut state, e)
            }
            e @ AgentDomainEvent::Compacted { .. } => Compaction::apply(&mut state, e),
            e @ (AgentDomainEvent::TimerArmed { .. }
            | AgentDomainEvent::TimerCancelled { .. }
            | AgentDomainEvent::TimerFired { .. }) => Timers::apply(&mut state, e),
            e @ AgentDomainEvent::TaskListChanged { .. } => TaskLists::apply(&mut state, e),
            AgentDomainEvent::SystemPromptRecorded { .. }
            | AgentDomainEvent::AgentInitialized { .. }
            | AgentDomainEvent::ConnectionCompleted
            | AgentDomainEvent::StepStarted { .. }
            | AgentDomainEvent::StepFailed { .. }
            | AgentDomainEvent::StopHookCompleted { .. }
            | AgentDomainEvent::RunEnded { .. } => {}
        }
        if let Some(history_record) = history_record {
            state.record_history(history_record);
        }
        state
    }

    /// Fold several events forward over a snapshot — what a handler does to
    /// see the state its own events leave behind before deciding what comes
    /// next.
    pub fn apply_all(state: &AgentState, events: &[AgentDomainEvent]) -> AgentState {
        events.iter().cloned().fold(state.clone(), Self::apply)
    }
}

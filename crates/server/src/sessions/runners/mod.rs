//! A session hosts runners; a runner owns one agent role and the capabilities
//! its agents are equipped with.
//!
//! Nothing in this module performs anything. A runner and a capability both
//! answer with `(events, actions)` — the events are what to journal, the
//! actions are what the session should do — which is what makes every decision
//! here testable against a hand-built state with no actor, no runtime and no
//! journal.
//!
//! That purity is also why there is no `run()` or `init()`. Starting work is
//! [`Runner::actions`], a pure function of the folded state, so creation and
//! recovery take the same path: a `Pending` runner with no agents starts its
//! first agent whether that state arrived a millisecond ago or from a journal
//! replayed after a restart. A `run()` that fired once would need a second
//! entry path for recovery, which either double-starts every agent or has to
//! be suppressed — and the suppression is where the bugs live.
//!
//! Three rules hold across everything below, and each replaces a way the
//! previous shape went wrong:
//!
//! 1. A runner writes only its own slice. Cross-runner facts arrive as calls
//!    ([`AgentLifecycle::on_agent_ended`], a child's outcome), never as reads.
//! 2. A runner returns events; the fold is the only writer. Nothing mutates
//!    state directly, so replay and live operation cannot diverge.
//! 3. A runner impl holds no fields. Everything it knows is in the state
//!    handed to it — a field would not survive a reload, and worse, would
//!    silently differ from what the log says.
#![allow(dead_code)] // Phase A: built and tested here, wired into the actor in Phase B.

pub mod action;
pub mod capabilities;
pub mod conversation;
pub mod ids;
pub mod lifecycle_routing;
pub mod loading;
pub mod message;
pub mod runtime;
pub mod state;
pub mod subagent;
pub mod workflow;

pub use ids::{AgentId, RunnerId, RunnerKind, RunnerStatus};
pub use state::{RunnerRecord, SessionState};

use action::Action;
use capabilities::Capabilities;
use message::ChildOutcome;
use serde::{Deserialize, Serialize};

/// What a runner decided: events for its own slice, actions for the session.
///
/// The same shape a capability's [`capabilities::Decision`] has, one level up —
/// deliberately, because "decide, never perform" is one idea and two shapes for
/// it would read as two.
#[derive(Debug, Default)]
pub struct Emit {
    pub events: Vec<RunnerEvent>,
    pub actions: Vec<Action>,
}

impl Emit {
    /// Nothing to journal, nothing to do. The common answer.
    #[must_use]
    pub fn nothing() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn record(events: Vec<RunnerEvent>) -> Self {
        Self {
            events,
            actions: Vec::new(),
        }
    }
}

/// How a turn ended, narrowed to the ways that mean something to a runner.
///
/// [`crate::agent_loop::AgentOutcome`] minus the reports that are not endings
/// at all — usage to bank, and a turn *beginning*. Those mean the same thing
/// to every runner, so the session answers them itself rather than routing
/// them, which is what keeps this enum small enough that each runner can match
/// it exhaustively instead of carrying arms for cases it can never see.
#[derive(Debug, Clone)]
pub enum TurnEnd {
    /// The agent produced its output — structured, or its final text.
    Concluded { output: serde_json::Value },
    /// The agent parked on one or more questions. Carries none of them: they
    /// belong to the agent that asked and are answered through it.
    Asked,
    /// `terminal` means the sandbox is gone and no later message brings it
    /// back; anything else is a turn that can be retried.
    Failed { error: String, terminal: bool },
    /// The agent parked awaiting its timers.
    Parked,
    /// The process died inside the turn, and the agent said so at recovery.
    Interrupted,
}

/// What a runner may know about the session around it.
///
/// Its own slice plus this, and nothing else. Handing a runner the whole
/// [`SessionState`] would let a workflow read a conversation's turn status,
/// which is the coupling this split exists to remove — so everything
/// cross-runner arrives as a call instead, and everything session-wide arrives
/// here as a number.
#[derive(Debug, Clone, Copy)]
pub struct SessionView {
    /// Nothing starts before the sandbox exists. One gate, checked once, for
    /// every runner.
    pub runtime_ready: bool,
    /// How deep this runner sits, for the one budget nesting needs.
    pub depth: u32,
    /// How many agents this session already has running, against the
    /// session-wide cap. A property of the sandbox, so it is the session's
    /// number rather than a per-runner one.
    pub active_agents: u32,
}

/// One unit of work, and the agents that carry it out.
///
/// Implemented by the *state* struct rather than by a separate unit type: the
/// state is all a runner has, so a second object would only be a place for a
/// field to hide that does not survive a reload.
pub trait Runner {
    /// What I want started, given the state as it now is.
    ///
    /// Pure and idempotent, called at every boundary — which is what lets
    /// creation and recovery take the same path. A `run()` that fired once
    /// would need a second entry for recovery, and the suppression that
    /// implies is where the bugs live.
    fn actions(&self, view: &SessionView) -> Vec<Action>;

    /// My ending, translated into the vocabulary of whoever created me.
    ///
    /// `None` while I am still going, and *always* `None` for a conversation:
    /// a conversation owes nobody a result, root or not, which is what lets
    /// `parent` mean provenance rather than debt.
    fn outcome(&self) -> Option<ChildOutcome> {
        None
    }

    /// Whether I have work in flight, so the session must not unload.
    fn busy(&self) -> bool;

    /// The status I have reached, if I have reached one.
    ///
    /// The session reads this at every boundary and journals a
    /// [`state::SessionEvent::RunnerEnded`] the first time it answers, so
    /// `RunnerRecord.status` is derived from the runner's own slice rather
    /// than written twice — the drift the previous shape had, where a fork's
    /// roster entry and the session's status were separate variables that
    /// disagreed.
    ///
    /// Defaulted to `None` because two of the four kinds never end: a
    /// conversation owes nobody a result, and the sandbox is not a unit of
    /// work at all. `Some` is a *terminal* status only; a runner that is
    /// merely idle is still going.
    fn finished(&self) -> Option<RunnerStatus> {
        None
    }

    /// What my agents are equipped with, or `None` when I own no agents.
    ///
    /// `Option` for the same reason [`RunnerState::lifecycle`] is one, and
    /// defaulted to the same answer: the runtime runner owns nothing that could
    /// hold a capability, so "it equips nothing" is a fact about the type
    /// rather than an empty field somebody has to keep empty.
    fn capabilities(&self) -> Option<&Capabilities> {
        None
    }

    /// The same, for folding a capability's own event into it. Separate from
    /// [`Runner::capabilities`] so the read-only path stays read-only.
    fn capabilities_mut(&mut self) -> Option<&mut Capabilities> {
        None
    }

    /// Fold one of my own events. Pure — no clock, no ids, no randomness.
    ///
    /// `at_ms` is when the session journaled the entry carrying this event, and
    /// it is how a fold that may not read a clock still stamps a time. It
    /// arrives *on the event* rather than being read here, so a replay lands
    /// exactly the timestamps the live run wrote — which is the whole recovery
    /// contract, and the reason reaching for `now_ms()` inside an `apply` is
    /// the one thing this signature exists to prevent.
    fn apply(&mut self, event: &RunnerEvent, at_ms: u64);
}

/// What a runner must answer for the agents it starts.
///
/// A separate trait, and [`RunnerState::lifecycle`] returns `None` for the one
/// runner that owns no agents — so "a runner with no agents cannot be handed
/// an agent event" is a fact about the type rather than an unreachable arm
/// somebody has to keep true.
///
/// No method switches on *which* agent, because one runner owns exactly one
/// agent role: a workflow owns several step agents over time, but they are all
/// step agents. A runner's state, its agent role and its outcome vocabulary
/// are one triple.
pub trait AgentLifecycle {
    fn on_agent_started(&self, agent: AgentId) -> Emit;

    /// The method that separates the runners: the same ending is a result for
    /// one, an input to a graph for another.
    fn on_agent_ended(&self, agent: AgentId, end: &TurnEnd) -> Emit;

    fn on_agent_halted(&self, agent: AgentId, reason: &str) -> Emit;

    /// A person stopped this agent.
    ///
    /// The one thing every impl must get right is the *gate*: stopping
    /// something that was not working is [`Emit::nothing`], never a failure.
    /// A boundary journaled over an agent that had already ended rewrites
    /// history — it moves an idle conversation backwards, or concludes a step
    /// the run has already routed past — and a stop is the easiest way to
    /// arrive twice, because a person can press it while the ending is in
    /// flight.
    ///
    /// Separate from [`Self::on_agent_halted`] because a halt comes from a
    /// hook and carries a reason the agent must be told; a stop is a person
    /// changing their mind and is not a failure of anything.
    fn on_agent_stopped(&self, agent: AgentId) -> Emit;
}

/// What a runner of this kind holds, in the order tool calls are offered
/// around them.
///
/// The order is the conflict resolution for a tool call, so it is written down
/// here rather than left to whoever constructs a runner: the fixed-name
/// capabilities come first and the open-namespace ones last, because
/// [`capabilities::runtime::RuntimeCapability`] answers for a namespace nobody
/// can enumerate — the sandbox toolbox plus whatever the plugin scan found.
/// Put it first and it silently shadows every named tool behind it.
#[must_use]
pub fn assemble(kind: RunnerKind, opts: &Assembly<'_>) -> Capabilities {
    use capabilities::{
        ask_user::AskUserCapability, control_plane::ControlPlaneCapability, fork::ForkCapability,
        mcp::McpCapability, memory::MemoryCapability, runtime::RuntimeCapability,
        sub_agent::SubAgentCapability, title::TitleCapability, workflow::WorkflowCapability,
    };
    let s = opts.settings;
    let mut caps = Capabilities::default();

    // A runner that owns no agents equips nothing, and has nothing to offer a
    // message to.
    if matches!(kind, RunnerKind::Runtime) {
        return caps;
    }

    // Delegation: every runner that owns an agent can delegate, which is what
    // makes nesting uniform rather than a privilege of the main agent.
    caps.push(SubAgentCapability::new(s.clone()));
    caps.push(WorkflowCapability::default());

    match kind {
        // A conversation can ask, name itself, and branch.
        RunnerKind::Conversation => {
            caps.push(match opts.unattended {
                true => AskUserCapability::unattended(),
                false => AskUserCapability::default(),
            });
            caps.push(match opts.fork {
                Some(fork) => TitleCapability::for_fork(fork),
                None => TitleCapability::default(),
            });
            caps.push(ForkCapability::new(s.clone()));
        }
        // A step's `submit_result` and its `ask_user` are declared per step, so
        // they are equipped when the step agent starts rather than held here.
        RunnerKind::Workflow => {}
        // A worker owes a report and cannot ask, name the session or branch.
        RunnerKind::SubAgent => {}
        RunnerKind::Runtime => {}
    }

    if s.control_plane == Some(true) && matches!(kind, RunnerKind::Conversation) {
        caps.push(ControlPlaneCapability);
    }
    if !s.memory_spaces.is_empty() {
        caps.push(MemoryCapability::new(s.memory_spaces.clone()));
    }
    // Last, and last on purpose.
    if !s.mcp_servers.is_empty() {
        caps.push(McpCapability::new(s.mcp_servers.clone()));
    }
    caps.push(RuntimeCapability::new(opts.agent_type.clone()));
    caps
}

/// What `assemble` needs beyond the kind.
pub struct Assembly<'a> {
    pub settings: &'a crate::sessions::spec::AgentSettings,
    /// Nobody is watching, so no `ask_user`: a question would park for ever.
    pub unattended: bool,
    /// Set when this conversation is a fork, so it names itself rather than
    /// the session it branched from.
    pub fork: Option<RunnerId>,
    /// The plugin-declared agent type a worker was spawned as, if it was.
    ///
    /// The *name* only. It travels to the runtime capability, which resolves
    /// the definition against the library on every load — so a worker whose
    /// plugin was uninstalled between spawn and wake fails rather than running
    /// a prompt nobody can point at.
    pub agent_type: Option<String>,
}

/// An `AgentSettings` with nothing set.
///
/// Test-only, and deliberately not a `Default` on `AgentSettings` itself: a
/// settings with an empty `model` names no provider, so production code must
/// not be able to build one by accident. The two runners that need a `Default`
/// state need it only so `RunnerState::empty_for` can build one, which is
/// itself test scaffolding.
#[cfg(test)]
#[must_use]
pub(crate) fn empty_settings() -> crate::sessions::spec::AgentSettings {
    crate::sessions::spec::AgentSettings {
        model: String::new(),
        allowed_tools: None,
        use_plugins: None,
        max_iterations: None,
        max_retries: 0,
        mcp_servers: Vec::new(),
        memory_spaces: Vec::new(),
        thinking_effort: None,
        max_concurrent_subagents: None,
        instructions: None,
        plugins: Vec::new(),
        auto_compact: None,
        control_plane: None,
    }
}

/// A runner's own slice.
///
/// One arm per kind, and the session never matches on it to make a decision:
/// it hands the whole value back to the impl that owns it. A closed enum
/// rather than an opaque blob so the journal stays typed and a shape change
/// fails to compile where it should.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerState {
    Conversation(conversation::State),
    SubAgent(subagent::State),
    Workflow(Box<workflow::State>),
    Runtime(runtime::State),
}

/// One runner's event, tagged with the runner that owns it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RunnerEvent {
    Conversation(conversation::Event),
    SubAgent(subagent::Event),
    Workflow(workflow::Event),
    Runtime(runtime::Event),
    /// A capability's own event, folded into whichever capability owns that
    /// arm. Here rather than on each runner because every runner holds
    /// capabilities and none of them needs to know which.
    Capability(capabilities::CapEvent),
    /// Tokens one of my agents spent, banked against me.
    ///
    /// One arm rather than one per runner, because banking is the same act for
    /// every kind: the session decides it — it means the same thing whoever
    /// spent them — and each runner only chooses where to add them up. A
    /// conversation and a worker keep one total; a workflow keeps one per step
    /// agent, because a run's graph is read per step and this is its only
    /// source for the number.
    ///
    /// `agent` is carried even where nobody reads it. The runner that owns the
    /// agent is the one being handed this, so the field is not routing; it is
    /// what lets a runner that owns several agents attribute a total to one,
    /// which is a fact about the log that a workflow-only field would hide.
    Usage {
        agent: AgentId,
        model: String,
        spent: crate::agent_loop::UsageTotal,
    },
}

macro_rules! dispatch {
    ($self:ident, $method:ident $(, $arg:expr)*) => {
        match $self {
            RunnerState::Conversation(s) => s.$method($($arg),*),
            RunnerState::SubAgent(s) => s.$method($($arg),*),
            RunnerState::Workflow(s) => s.$method($($arg),*),
            RunnerState::Runtime(s) => s.$method($($arg),*),
        }
    };
}

impl Runner for RunnerState {
    fn actions(&self, view: &SessionView) -> Vec<Action> {
        // One gate in front of everything: nothing starts before the sandbox
        // the work would run on exists.
        if !view.runtime_ready && !matches!(self, RunnerState::Runtime(_)) {
            return Vec::new();
        }
        dispatch!(self, actions, view)
    }

    fn outcome(&self) -> Option<ChildOutcome> {
        dispatch!(self, outcome)
    }

    fn busy(&self) -> bool {
        dispatch!(self, busy)
    }

    fn finished(&self) -> Option<RunnerStatus> {
        dispatch!(self, finished)
    }

    fn capabilities(&self) -> Option<&Capabilities> {
        dispatch!(self, capabilities)
    }

    fn capabilities_mut(&mut self) -> Option<&mut Capabilities> {
        dispatch!(self, capabilities_mut)
    }

    fn apply(&mut self, event: &RunnerEvent, at_ms: u64) {
        // A capability's event is folded into the capability that owns it,
        // whichever runner is holding it.
        if let RunnerEvent::Capability(e) = event {
            if let Some(caps) = dispatch!(self, capabilities_mut) {
                caps.apply(e);
            }
            return;
        }
        dispatch!(self, apply, event, at_ms);
    }
}

impl RunnerState {
    /// An empty slice of the right shape. Tests only — live code builds a
    /// runner's state from the args it was created with.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn empty_for(kind: RunnerKind) -> Self {
        match kind {
            RunnerKind::Conversation => Self::Conversation(conversation::State::default()),
            RunnerKind::SubAgent => Self::SubAgent(subagent::State::default()),
            RunnerKind::Workflow => Self::Workflow(Box::default()),
            RunnerKind::Runtime => Self::Runtime(runtime::State::default()),
        }
    }

    /// What this runner must answer for its agents, or `None` when it owns
    /// none. The runtime is the only `None`.
    #[must_use]
    pub fn lifecycle(&self) -> Option<&dyn AgentLifecycle> {
        match self {
            Self::Conversation(s) => Some(s),
            Self::SubAgent(s) => Some(s),
            Self::Workflow(s) => Some(s.as_ref()),
            Self::Runtime(_) => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use message::{Caller, Message, ToolCall};

    fn opts(settings: &crate::sessions::spec::AgentSettings) -> Assembly<'_> {
        Assembly {
            settings,
            unattended: false,
            fork: None,
            agent_type: None,
        }
    }

    fn call(name: &str) -> Message {
        Message::Tool(ToolCall {
            id: "t".into(),
            name: name.into(),
            input: serde_json::json!({}),
        })
    }

    /// The runtime capability answers for a namespace nobody can enumerate, so
    /// it must sort last. First, it silently shadows every named tool behind
    /// it — which is exactly the failure the written order exists to prevent.
    #[test]
    fn the_open_namespace_capability_sorts_last() {
        let s = empty_settings();
        for kind in [
            RunnerKind::Conversation,
            RunnerKind::SubAgent,
            RunnerKind::Workflow,
        ] {
            let caps = assemble(kind, &opts(&s));
            let last = caps.last().expect("every agent-owning runner equips one");
            assert_eq!(
                last.name(),
                "runtime",
                "{kind:?} put something after the runtime capability"
            );
        }
    }

    /// A runner that owns no agents equips nothing — there is no agent to
    /// equip, and nothing that could send it a tool call.
    #[test]
    fn the_runtime_runner_assembles_nothing() {
        let s = empty_settings();
        assert!(assemble(RunnerKind::Runtime, &opts(&s)).is_empty());
    }

    /// Every runner that owns an agent can delegate. That uniformity is the
    /// whole point: a subagent spawning a subagent, and a step invoking a
    /// workflow, are the same capability in a different holder.
    #[test]
    fn every_agent_owning_runner_can_delegate() {
        let s = empty_settings();
        let caller = Caller {
            agent: AgentId::new_v4(),
            depth: 0,
            active_agents: 0,
        };
        for kind in [
            RunnerKind::Conversation,
            RunnerKind::SubAgent,
            RunnerKind::Workflow,
        ] {
            let caps = assemble(kind, &opts(&s));
            assert!(caps.has("sub_agent"), "{kind:?} cannot spawn");
            assert!(caps.has("workflow"), "{kind:?} cannot invoke a workflow");
            // And the runtime does not swallow the named tool on the way past.
            let taken = caps
                .iter()
                .find_map(|c| c.handle(caller, &call("spawn_agent")).map(|_| c.name()));
            assert_eq!(
                taken,
                Some("sub_agent"),
                "{kind:?} left spawn_agent unclaimed or misrouted"
            );
        }
    }

    /// A worker owes a report; it cannot ask the user, name the session or
    /// branch a conversation. Equipping it with those would advertise tools
    /// whose answers nothing could route.
    #[test]
    fn a_worker_gets_none_of_a_conversations_arms() {
        let s = empty_settings();
        let caps = assemble(RunnerKind::SubAgent, &opts(&s));
        assert!(!caps.has("ask_user"));
        assert!(!caps.has("title"));
        assert!(!caps.has("fork"));
    }

    /// Nobody is watching a routine's run, so its conversation gets no
    /// `ask_user` layer: a question it asked would park the run for ever.
    #[tokio::test]
    async fn an_unattended_conversation_equips_no_ask_layer() {
        let s = empty_settings();
        let caps = assemble(
            RunnerKind::Conversation,
            &Assembly {
                settings: &s,
                unattended: true,
                fork: None,
                agent_type: None,
            },
        );
        // Without the runtime, which has no sandbox to acquire here: the list
        // is the same one either way, and what is asserted is what the
        // capabilities *before* it equipped.
        let attended: Capabilities = caps
            .iter()
            .filter(|c| c.name() != "runtime")
            .map(|c| c.save().into())
            .collect();
        let (spec, _) = attended
            .equip(&capabilities::testing::loading(), s.clone())
            .await
            .expect("nothing fatal");
        assert!(
            !capabilities::testing::equipped(spec)
                .contains(&capabilities::ask_user::TOOL.to_string())
        );
    }
}

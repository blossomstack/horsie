//! What an agent can do, held by the agent that does it.
//!
//! A capability is one thing an agent can do: `ask_user`, `submit_result`,
//! `spawn_agent`, `set_session_title`, the sandbox toolbox, the memory and MCP
//! layers. One mechanism rather than a special case per tool, which is what
//! lets a workflow whose first step is interactive and whose second is not
//! equip exactly the right tools without a second way of saying so.
//!
//! # Why these live here and not on the session
//!
//! They used to live on the session actor, and two of them proved that wrong.
//! `ask_user` and `submit_result` both end their turn by returning
//! [`ToolOutcome::StopRun`](horsie_agentcore::ToolOutcome) and telling the
//! session nothing at all — so the session-side handler for them was code no
//! message could reach. The fact each one needed was a `tool_call_id`: a
//! pointer into a transcript the session does not hold and cannot write.
//!
//! A capability belongs to the actor whose state it needs. That is the whole
//! rule, and it puts every one of them here — the agent owns the transcript,
//! the park, and the tool call, and it is the only thing that can journal
//! against them.
//!
//! What is left for the session is genuinely the session's: starting a child
//! runner, cancelling an agent, naming the session. Those are asked for with a
//! [`SessionRequest`] and answered with a [`SessionReply`], which is one
//! request and one answer rather than a share of the session's state.
//!
//! # Park and resume is why this exists
//!
//! A tool that returns a value never needed any of this: the toolbox could
//! answer it and the run would carry on. A tool that *parks* had no way to say
//! so — it could stop the run, but it could not record what it was waiting for,
//! because the place to record it was the agent's own journal.
//!
//! So the two verbs that matter are parking and resuming. Parking leaves a
//! dangling `tool_use` and ends the turn; resuming supplies the results that
//! pair with it and starts the next one. Both are the agent actor's to perform,
//! and a capability only says which one this call came to.
//!
//! # There is one dispatch mechanism, and it is the actor's
//!
//! What a person or a model asked a capability to do arrives as one of the
//! agent actor's own [`AgentCommand`](crate::agent_loop::AgentCommand) arms,
//! built by the toolbox layer that claimed the name. Nothing downstream of that
//! layer ever sees a tool name: which capability answers is decided by which
//! arm was constructed, so a name is resolved once, at compose time, rather
//! than by a scan at call time.
//!
//! Lifecycle — a turn boundary, a load, an answer, a child, a session reply, a
//! wake, a conclusion — is not routed at all. The actor reaches each of those
//! moments in its own code, and calls the capability that has something to say
//! about it by name. There used to be a `Msg` enum, an offer scan and a
//! broadcast in between; they bought a closed set the ability to pretend to be
//! open, and cost the actor the ability to see what any one capability decided.
//!
//! Every function a capability decides with returns a narrow type of its own,
//! never an [`AgentDomainEvent`]: the arm that called it is what journals, so
//! nothing can journal an event that is none of its business. Two conventions
//! run through those types — a variant named `Told` means the model gets a
//! plain successful tool result, and one named `Refused` means it gets a tool
//! *error*, which is what `is_error` reaching agentcore's loop detector and the
//! nudge budget depends on.
//!
//! # What an agent may do, and what it has done
//!
//! [`Capabilities`] is config: the list a runner equipped, and nothing else.
//! What each capability has accumulated is a field on
//! [`AgentState`](crate::agent_loop::AgentState) beside it, typed by a struct
//! whose fields are private to that capability's own file — so nothing outside
//! that file can reach a record except through an accessor the file chose to
//! write, and the journal reads flat.
//!
//! The split is the difference between a *depth* and the children spawned at
//! it, or an *attention mode* and the questions parked under it. Config is
//! answered when the agent is equipped and never again; state is folded from
//! this agent's own events. Which of the two an arm needs is answered by the
//! accessors below: [`Capabilities::ask_user`] and its siblings say whether a
//! capability is equipped at all, which is the one question a command arm has
//! to ask before doing anything.
//!
//! Order is still a written property of assembly — the open-namespace
//! capabilities, the sandbox above all, sort last because they answer for a
//! namespace nobody can enumerate. See [`Capabilities::push_front`].

pub mod ask_user;
pub mod budget;
pub mod control_plane;
pub mod fork;
pub mod mcp;
pub mod memory;
pub mod runtime;
pub mod step_result;
pub mod sub_agent;
pub mod task_list;
pub mod timers;
pub mod title;
pub mod workflow;

use crate::agent_loop::AgentCommand;
use crate::agent_loop::state::AgentState;
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::loading::{AgentFacts, AgentSpec, Loading};
use horsie_actor::ReplyTo;
use horsie_agentcore::{ToolCallError, ToolOutcome, Toolbox};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// How a tool call is answered: the outcome the run is waiting for, or the
/// error it is told instead.
pub type ToolReply = ReplyTo<Result<ToolOutcome, ToolCallError>>;

/// The run waiting for a command's answer.
///
/// Carried beside the command rather than inside it, because the shape of an
/// answer is the same for every capability — a tool outcome — and only *what*
/// was asked varies.
///
/// The channel is never used by a capability. A capability decides what the
/// answer is; the actor decides *when* it goes out, which is after the events
/// behind it are durable. A capability that could send would be able to report
/// success for work a crash loses, and no test could fail for it.
pub struct Answering {
    /// The provider's `tool_use` id. Still here beside the channel because a
    /// park outlives it: the call dangles, and the result arrives against this
    /// id on a process that has since rehydrated the session.
    pub call: String,
    pub reply: ToolReply,
}

/// The agent's own mailbox, in the shape a capability's layer can reach it.
///
/// Not an [`ActorRef`](horsie_actor::ActorRef): a capability is persisted state
/// and cannot hold an address, and passing this in is also what lets a test
/// compose a layer with no actor and no tokio runtime. Not a [`Toolbox`]
/// either, which is what it was until commands existed — a toolbox takes a
/// *name*, and the whole point of a command is that the name was resolved by
/// the layer that claimed it.
///
/// The command is built from the channel rather than handed over with one,
/// because the channel does not exist until the send does. That is
/// [`ActorRef::ask`](horsie_actor::ActorRef::ask)'s shape.
#[async_trait::async_trait]
pub trait Mailbox: Send + Sync {
    async fn send(
        &self,
        make: Box<dyn FnOnce(ToolReply) -> AgentCommand + Send>,
    ) -> Result<ToolOutcome, ToolCallError>;
}

/// What a capability can ask the session for.
///
/// Deliberately short. The session starts runners, forwards messages and tracks
/// the tree; anything longer than this list is a sign that a fact is being kept
/// on both sides.
#[derive(Debug, Clone)]
pub enum SessionRequest {
    /// Create a child runner — a subagent, a fork, a workflow run.
    ///
    /// `call` is the tool call that asked, and it is also the dedupe key: this
    /// request is journaled before it is sent, so a crash in between replays it,
    /// and the session must recognise the second copy as the same child rather
    /// than start two.
    StartRunner {
        call: String,
        id: RunnerId,
        kind: RunnerKind,
        args: Box<crate::sessions::runners::action::RunnerArgs>,
    },
    /// Stop an agent's run.
    Cancel { call: String, agent: AgentId },
    /// Name the session this agent belongs to.
    SetTitle { call: String, title: String },
}

impl SessionRequest {
    /// The tool call this request answers to.
    #[must_use]
    pub fn call(&self) -> &str {
        match self {
            Self::StartRunner { call, .. }
            | Self::Cancel { call, .. }
            | Self::SetTitle { call, .. } => call,
        }
    }
}

/// What the session said.
///
/// Two arms, and the refusal is one of them rather than an error type: a
/// capability that asked for a child and was told no has to answer the model,
/// and a refusal it cannot see is a tool call that never returns.
#[derive(Debug, Clone)]
pub enum SessionReply {
    Done { call: String },
    Refused { call: String, reason: String },
}

impl SessionReply {
    #[must_use]
    pub fn call(&self) -> &str {
        match self {
            Self::Done { call } | Self::Refused { call, .. } => call,
        }
    }
}

/// Why a capability could not equip the agent.
///
/// `fatal` is the capability's own call, and it is the whole answer to "does a
/// failed setup stop the turn?": the sandbox says yes, because an agent with no
/// runtime can do nothing; MCP says no, because a server that will not connect
/// costs the agent some tools and not its turn. Nothing above has to know which
/// is which.
#[derive(Debug)]
pub struct SetupError {
    pub capability: &'static str,
    pub reason: String,
    pub fatal: bool,
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} could not equip the agent: {}",
            self.capability, self.reason
        )
    }
}

/// One thing an agent can do.
///
/// A closed enum rather than a trait object, because the set was closed all
/// along: every capability there is lives in this module, and nothing outside
/// the crate contributes one — plugins bring hooks, MCP servers, agents and
/// skills, never a capability. What varies per agent is *which* of these arms a
/// runner equips, and that is a choice among known arms rather than an open set
/// of implementations. See [`crate::sessions::runners::assemble`].
///
/// Dispatch is the `match` in each method below. It is exhaustive, so a
/// fourteenth capability cannot be added without answering, in one place, every
/// question the actor asks of one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Capability {
    AskUser(ask_user::AskUserCapability),
    Title(title::TitleCapability),
    Fork(fork::ForkCapability),
    SubAgent(sub_agent::SubAgentCapability),
    Workflow(workflow::WorkflowCapability),
    StepResult(step_result::StepResultCapability),
    TaskList(task_list::TaskListCapability),
    Timers(timers::TimersCapability),
    TokenBudget(budget::TokenBudgetCapability),
    ControlPlane(control_plane::ControlPlaneCapability),
    Memory(memory::MemoryCapability),
    Mcp(mcp::McpCapability),
    Runtime(runtime::RuntimeCapability),
    /// A capability with no behaviour of its own, so the composition rules have
    /// something to be tested against that cannot break when a real capability
    /// changes.
    ///
    /// An arm rather than an injected stub, because a closed set cannot take
    /// one — the same shape the actor's own command enum carries for the same
    /// reason.
    #[cfg(test)]
    Fake(testing::FakeCapability),
}

impl Capability {
    /// Stable, and what a diagnostic names this capability by.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::AskUser(c) => c.name(),
            Self::Title(c) => c.name(),
            Self::Fork(c) => c.name(),
            Self::SubAgent(c) => c.name(),
            Self::Workflow(c) => c.name(),
            Self::StepResult(c) => c.name(),
            Self::TaskList(c) => c.name(),
            Self::Timers(c) => c.name(),
            Self::TokenBudget(c) => c.name(),
            Self::ControlPlane(c) => c.name(),
            Self::Memory(c) => c.name(),
            Self::Mcp(c) => c.name(),
            Self::Runtime(c) => c.name(),
            #[cfg(test)]
            Self::Fake(c) => c.name(),
        }
    }

    /// Equip the agent: acquire what this capability needs, then fill in the
    /// part of the spec it answers for.
    ///
    /// Async, and run on the agent's own task rather than on a mailbox —
    /// acquiring a sandbox, scanning a workspace and connecting an MCP server
    /// are all slow, and an actor that cannot answer a read while one agent
    /// starts is the shape this design exists to avoid.
    ///
    /// Reads config only, never folded state: the answer must not depend on how
    /// far this agent has got.
    pub async fn setup(&self, loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        match self {
            Self::AskUser(c) => c.setup(loading, spec).await,
            Self::Title(c) => c.setup(loading, spec).await,
            Self::StepResult(c) => c.setup(loading, spec).await,
            Self::ControlPlane(c) => c.setup(loading, spec).await,
            Self::Memory(c) => c.setup(loading, spec).await,
            Self::Mcp(c) => c.setup(loading, spec).await,
            Self::Runtime(c) => c.setup(loading, spec).await,
            // Nothing to acquire and nothing to say about the spec: these are
            // equipped entirely by the toolbox layer they contribute, which is
            // composed later and on the run's own task.
            Self::Fork(_)
            | Self::SubAgent(_)
            | Self::Workflow(_)
            | Self::TaskList(_)
            | Self::Timers(_)
            | Self::TokenBudget(_) => Ok(()),
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
        }
    }

    /// Release what [`Self::setup`] acquired. Runs when the agent is unloaded.
    ///
    /// Nothing holds a resource that needs releasing today: the two that
    /// acquire one — `mcp` and `runtime` — hand it to the spec, which outlives
    /// them. The match is exhaustive anyway, so the first capability that does
    /// hold one has to say so here rather than leak quietly.
    pub async fn teardown(&self) {
        match self {
            Self::AskUser(_)
            | Self::Title(_)
            | Self::Fork(_)
            | Self::SubAgent(_)
            | Self::Workflow(_)
            | Self::StepResult(_)
            | Self::TaskList(_)
            | Self::Timers(_)
            | Self::TokenBudget(_)
            | Self::ControlPlane(_)
            | Self::Memory(_)
            | Self::Mcp(_)
            | Self::Runtime(_) => {}
            #[cfg(test)]
            Self::Fake(_) => {}
        }
    }

    /// Wrap the agent's toolbox in this capability's own layer.
    ///
    /// A capability with no tools hands `inner` straight back, so it costs the
    /// model's calls no indirection at all.
    ///
    /// A layer answers the names it claims and passes everything else straight
    /// through — see [`crate::agent_loop::toolbox::claiming`], which is what
    /// every capability here builds its layer with. A claimed name becomes one
    /// of this capability's own commands and reaches [`Self::command`] on the
    /// actor's mailbox, so a tool can park, journal and ask the session;
    /// unclaimed ones never touch the mailbox.
    ///
    /// **Wrapping order is precedence.** [`Capabilities::layer`] applies the
    /// list back to front, so the *first* capability ends up outermost and wins
    /// a name against everything behind it. That is the whole of the ordering
    /// rule: there is nowhere else a name is resolved.
    ///
    /// [`AgentFacts`] rather than nothing, because an advertisement can depend
    /// on what the load found: `sub_agent` lists the installed agent types, and
    /// only the workspace scan knows them. Facts are why layers are composed on
    /// the run's own task after `provide` rather than on the mailbox before it —
    /// a capability that sorts ahead of `runtime`, as `sub_agent` must to win
    /// the `spawn_agent` name, has no scan of its own to read.
    ///
    /// `mailbox` is the agent's own. Passed in rather than held, because a
    /// capability is persisted state and an address is not; it is also what
    /// lets a test compose a layer with no actor running.
    #[must_use]
    pub fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        facts: &AgentFacts,
        mailbox: &Arc<dyn Mailbox>,
    ) -> Arc<dyn Toolbox> {
        match self {
            Self::AskUser(c) => c.layer(inner, facts, mailbox),
            Self::Title(c) => c.layer(inner, facts, mailbox),
            Self::SubAgent(c) => c.layer(inner, facts, mailbox),
            Self::Workflow(c) => c.layer(inner, facts, mailbox),
            Self::StepResult(c) => c.layer(inner, facts, mailbox),
            Self::TaskList(c) => c.layer(inner, facts, mailbox),
            Self::Timers(c) => c.layer(inner, facts, mailbox),
            // No layer at all rather than one that only forwards. `fork` is
            // typed by a person; the rest push their tools from `setup`, on the
            // agent's own task.
            Self::Fork(_)
            | Self::TokenBudget(_)
            | Self::ControlPlane(_)
            | Self::Memory(_)
            | Self::Mcp(_)
            | Self::Runtime(_) => inner,
            #[cfg(test)]
            Self::Fake(c) => c.layer(inner, facts, mailbox),
        }
    }

    /// A fact this capability would lose to a compaction unless it is carried
    /// across verbatim. `None` when there is nothing to carry.
    ///
    /// Durable state is not the same as the model knowing it is durable: a task
    /// list and an armed timer both survive a compaction, and every trace of
    /// either in the transcript is a tool call the summariser replaces with
    /// prose. So whatever has an id in it says so here, and is rendered into
    /// the boundary message untouched — see
    /// [`crate::agent_loop::carried_state`].
    ///
    /// Asked of the capability rather than read off the state, because whether
    /// a fact is worth carrying is a judgement about the *feature*: the state
    /// survives either way, and this is the part the model would otherwise stop
    /// knowing about.
    #[must_use]
    pub fn carried_state(&self, state: &AgentState) -> Option<String> {
        match self {
            Self::TaskList(_) => task_list::TaskListCapability::carried_state(&state.task_list),
            Self::Timers(_) => state.timers.carried_state(),
            Self::AskUser(_)
            | Self::Title(_)
            | Self::Fork(_)
            | Self::SubAgent(_)
            | Self::Workflow(_)
            | Self::StepResult(_)
            | Self::TokenBudget(_)
            | Self::ControlPlane(_)
            | Self::Memory(_)
            | Self::Mcp(_)
            | Self::Runtime(_) => None,
            #[cfg(test)]
            Self::Fake(_) => None,
        }
    }
}

/// What an agent is equipped with, in the order its layers wrap.
///
/// **Config, and only config.** What each capability has *done* is a field on
/// [`AgentState`] beside this list. Keeping the two apart is what makes this an
/// ordinary `Vec` again: the whole list can be cloned, serialized and compared
/// by derive, because there is nothing in it a reload could get wrong.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capabilities(Vec<Capability>);

impl Capabilities {
    #[must_use]
    pub fn new(caps: Vec<Capability>) -> Self {
        Self(caps)
    }

    /// Add a capability at the open-namespace end.
    ///
    /// Only assembly should reach the end: the last capability answers for a
    /// namespace nobody can enumerate, so anything pushed after it is shadowed.
    /// A capability with a fixed tool name wants [`Self::push_front`].
    pub fn push(&mut self, cap: Capability) {
        self.0.push(cap);
    }

    /// Equip a capability ahead of everything already here.
    ///
    /// Front rather than back because that is now the only way to win a name:
    /// first in the list is outermost in the toolbox and first in the offer
    /// scan, and the open-namespace sandbox sorts last precisely so that
    /// everything else is ahead of it.
    pub fn push_front(&mut self, cap: Capability) {
        self.0.insert(0, cap);
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Capability> {
        self.0.iter()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether a capability of this name is equipped.
    #[must_use]
    pub fn has(&self, name: &str) -> bool {
        self.0.iter().any(|c| c.name() == name)
    }

    /// The capability equipped for a feature, or `None` when this agent was not
    /// given it.
    ///
    /// The one question a command arm asks before doing anything: a command for
    /// a capability nobody is equipped with is a bug, and the model is told so
    /// rather than left waiting on a call that never returns. Also what a
    /// lifecycle point asks, so a capability that is not equipped is not
    /// consulted about a load or a turn boundary either.
    ///
    /// One accessor per feature rather than a generic lookup, because the
    /// answer's *type* is what the caller needs — the config a decision reads
    /// is that capability's own struct, and a `bool` would leave the arm
    /// reaching for it somewhere else.
    #[must_use]
    pub(crate) fn ask_user(&self) -> Option<&ask_user::AskUserCapability> {
        self.0.iter().find_map(|c| {
            let Capability::AskUser(c) = c else {
                return None;
            };
            Some(c)
        })
    }

    #[must_use]
    pub(crate) fn title(&self) -> Option<&title::TitleCapability> {
        self.0.iter().find_map(|c| {
            let Capability::Title(c) = c else {
                return None;
            };
            Some(c)
        })
    }

    #[must_use]
    pub(crate) fn fork(&self) -> Option<&fork::ForkCapability> {
        self.0.iter().find_map(|c| {
            let Capability::Fork(c) = c else {
                return None;
            };
            Some(c)
        })
    }

    #[must_use]
    pub(crate) fn sub_agent(&self) -> Option<&sub_agent::SubAgentCapability> {
        self.0.iter().find_map(|c| {
            let Capability::SubAgent(c) = c else {
                return None;
            };
            Some(c)
        })
    }

    #[must_use]
    pub(crate) fn workflow(&self) -> Option<&workflow::WorkflowCapability> {
        self.0.iter().find_map(|c| {
            let Capability::Workflow(c) = c else {
                return None;
            };
            Some(c)
        })
    }

    #[must_use]
    pub(crate) fn step_result(&self) -> Option<&step_result::StepResultCapability> {
        self.0.iter().find_map(|c| {
            let Capability::StepResult(c) = c else {
                return None;
            };
            Some(c)
        })
    }

    #[must_use]
    pub(crate) fn task_list(&self) -> Option<&task_list::TaskListCapability> {
        self.0.iter().find_map(|c| {
            let Capability::TaskList(c) = c else {
                return None;
            };
            Some(c)
        })
    }

    #[must_use]
    pub(crate) fn timers(&self) -> Option<&timers::TimersCapability> {
        self.0.iter().find_map(|c| {
            let Capability::Timers(c) = c else {
                return None;
            };
            Some(c)
        })
    }

    #[must_use]
    pub(crate) fn token_budget(&self) -> Option<&budget::TokenBudgetCapability> {
        self.0.iter().find_map(|c| {
            let Capability::TokenBudget(c) = c else {
                return None;
            };
            Some(c)
        })
    }

    /// The fake claiming `tool`, of however many are equipped.
    ///
    /// Named by tool rather than taken first, because two fakes claiming one
    /// name are only tellable apart by what they were built with — which is the
    /// composition rule under test.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn fake(&self, tool: &str) -> Option<&testing::FakeCapability> {
        self.0.iter().find_map(|c| {
            let Capability::Fake(c) = c else {
                return None;
            };
            (c.tool == tool).then_some(c)
        })
    }

    /// Wrap this run's toolbox in every capability's layer.
    ///
    /// Back to front, so the *first* capability ends up outermost. That is the
    /// whole ordering rule: whoever wins a name is first in the offer list and
    /// outermost in the toolbox, and now those are one statement rather than
    /// two lists to keep in step.
    ///
    /// `inner` is the sandbox the setup layers composed, so a capability that
    /// claims nothing leaves an ordinary `bash` call exactly as cheap as it was.
    #[must_use]
    pub fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        facts: &AgentFacts,
        mailbox: &Arc<dyn Mailbox>,
    ) -> Arc<dyn Toolbox> {
        self.0
            .iter()
            .rev()
            .fold(inner, |inner, cap| cap.layer(inner, facts, mailbox))
    }

    /// Everything this agent would lose to a compaction, in the order it was
    /// equipped in.
    ///
    /// The equipment list's order, so the boundary message's sections come out
    /// the same way twice — which is what makes a compaction's rendering
    /// reproducible rather than dependent on field order somewhere else.
    #[must_use]
    pub fn carried_state(&self, state: &AgentState) -> Vec<String> {
        self.0
            .iter()
            .filter_map(|c| c.carried_state(state))
            .collect()
    }

    /// Equip an agent by folding every capability over a fresh spec.
    ///
    /// One fold, one source, and no way to advertise a tool whose result
    /// nothing can process. Non-fatal failures are returned alongside the spec
    /// rather than swallowed: the agent starts, and the caller reports what it
    /// starts without.
    pub async fn equip(
        &self,
        loading: &Loading,
        settings: crate::sessions::spec::AgentSettings,
    ) -> Result<(AgentSpec, Vec<SetupError>), SetupError> {
        let mut spec = AgentSpec::new(settings);
        let mut degraded = Vec::new();
        for cap in &self.0 {
            if let Err(e) = cap.setup(loading, &mut spec).await {
                if e.fatal {
                    return Err(e);
                }
                degraded.push(e);
            }
        }
        Ok((spec, degraded))
    }

    /// Release everything `equip` acquired.
    pub async fn teardown(&self) {
        for cap in &self.0 {
            cap.teardown().await;
        }
    }
}

impl FromIterator<Capability> for Capabilities {
    fn from_iter<I: IntoIterator<Item = Capability>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

/// The layer a decorator wraps, or an empty one when it is the innermost.
///
/// Every server-owned toolbox layer here decorates: it answers for its own
/// tools and delegates the rest inward. `None` reaches whichever one ended up
/// innermost, which happens whenever a capability set has no runtime — a
/// prompt-only agent, or a test. Wrapping [`horsie_agentcore::EmptyToolbox`]
/// then is the honest answer: the decorator still advertises its own tools, and
/// a call for anything else is refused by the same code path that refuses an
/// unknown tool today.
#[must_use]
pub(crate) fn or_empty(
    inner: Option<std::sync::Arc<dyn horsie_agentcore::Toolbox>>,
) -> std::sync::Arc<dyn horsie_agentcore::Toolbox> {
    inner.unwrap_or_else(|| std::sync::Arc::new(horsie_agentcore::EmptyToolbox))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub mod testing {
    use super::*;
    use crate::agent_loop::AgentDomainEvent;
    use horsie_agentcore::ToolSpec;

    /// A tool call, as the layer that claimed the name would have built one.
    ///
    /// The channel goes nowhere: every capability test asserts on what its
    /// decision returned, and *when* an answer is sent is the actor's business
    /// rather than a capability's — so there is nothing here for a capability
    /// to send on.
    #[must_use]
    pub fn answering(call: &str) -> Answering {
        let (tx, rx) = tokio::sync::oneshot::channel();
        // Held nowhere: a capability never sends, so a dropped receiver is the
        // honest shape rather than a leak.
        drop(rx);
        Answering {
            call: call.to_string(),
            reply: horsie_actor::ReplyTo::from_sender(tx),
        }
    }

    /// A capability with a name and one tool.
    ///
    /// Enough to exercise every composition rule without a real capability
    /// existing yet: it claims its own tool name and nothing else, and what it
    /// was told is folded into [`FakeState`] like any other capability's.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct FakeCapability {
        pub tool: String,
        /// What this one's advertisement says, so two fakes claiming the same
        /// name are still tellable apart — which is the only way to see *which*
        /// of them the composed toolbox let through.
        pub description: String,
    }

    impl FakeCapability {
        pub fn new(tool: &str) -> Self {
            Self {
                tool: tool.to_string(),
                description: String::new(),
            }
        }

        /// One that claims `tool` and says so in its own words.
        pub fn describing(tool: &str, description: &str) -> Self {
            Self {
                description: description.to_string(),
                ..Self::new(tool)
            }
        }
    }

    /// What the fake has been told, folded from
    /// [`AgentDomainEvent::FakeSaw`].
    ///
    /// Its field is private for the same reason a real capability's state is:
    /// nothing outside this module decides what "seen" means.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct FakeState {
        seen: Vec<String>,
    }

    impl FakeState {
        /// What this fake has been told, in order.
        #[must_use]
        pub fn seen(&self) -> &[String] {
            &self.seen
        }

        /// Every fake shares one state, so a record names the tool it came
        /// from — which is how a test tells two fakes apart.
        pub(crate) fn saw(&mut self, tool: String, what: String) {
            self.seen.push(format!("{tool}:{what}"));
        }
    }

    /// The methods the [`Capability`] enum dispatches into, as every real
    /// capability has them.
    impl FakeCapability {
        pub fn name(&self) -> &'static str {
            "fake"
        }

        pub fn layer(
            &self,
            inner: Arc<dyn Toolbox>,
            _facts: &AgentFacts,
            mailbox: &Arc<dyn Mailbox>,
        ) -> Arc<dyn Toolbox> {
            let tool = self.tool.clone();
            crate::agent_loop::toolbox::claiming(
                inner,
                vec![crate::agent_loop::toolbox::ClaimedTool::new(
                    ToolSpec {
                        name: self.tool.clone(),
                        description: self.description.clone(),
                        input_schema: serde_json::json!({"type": "object"}),
                    },
                    move |_input, to| AgentCommand::FakeCall {
                        tool: tool.clone(),
                        answering: to,
                    },
                )],
                mailbox,
            )
        }

        /// What this fake records when its tool is called.
        ///
        /// The actor's arm journals it, the same as any other capability's:
        /// what the composition rules are tested against has to travel the
        /// path a real one does.
        #[must_use]
        pub(crate) fn saw(&self) -> AgentDomainEvent {
            AgentDomainEvent::FakeSaw {
                tool: self.tool.clone(),
                what: format!("tool:{}", self.tool),
            }
        }
    }

    /// The fake's own command, as its layer would build it.
    #[must_use]
    pub fn call(tool: &str) -> AgentCommand {
        AgentCommand::FakeCall {
            tool: tool.to_string(),
            answering: answering("t1"),
        }
    }

    /// What a load that found nothing leaves behind: no workspace scan, no
    /// plugin library, no runtime. The facts every capability but `sub_agent`
    /// advertises the same tools under.
    #[must_use]
    pub fn facts() -> AgentFacts {
        AgentFacts::default()
    }

    /// The shared empty settings, re-exported so a capability test does not
    /// have to know where it lives.
    #[must_use]
    pub fn settings() -> crate::sessions::spec::AgentSettings {
        crate::sessions::runners::empty_settings()
    }

    /// A fresh spec over those settings — what `equip` starts every agent from.
    #[must_use]
    pub fn spec() -> AgentSpec {
        AgentSpec::new(settings())
    }

    /// What a capability loads from, with nothing behind it.
    ///
    /// A session mailbox that answers nothing, a runtime provider that knows no
    /// vendor, and no MCP, memory, services or plugin library. Enough for every
    /// capability that composes a toolbox out of what it already holds; the two
    /// that reach outward — `mcp` and `runtime` — are what the `None`s are for,
    /// and their tests assert on how they degrade rather than pretending a
    /// sandbox is there.
    #[must_use]
    pub fn loading() -> Loading {
        use crate::sessions::addressing::SessionRef;
        use crate::sessions::session_actor::AgentKey;
        use horsie_actor::{ActorSystem, InMemoryJournal};
        use std::sync::{Arc, Mutex, RwLock};

        let session_id = uuid::Uuid::new_v4();
        let session = SessionRef::new(
            crate::testing::spawn_detached(
                &ActorSystem::new(Arc::new(InMemoryJournal::new())),
                Inert,
            ),
            crate::auth::UserId::bootstrap(),
            session_id,
            None,
        );
        let vendors: crate::sessions::spec::RuntimeVendorMap =
            Arc::new(RwLock::new(std::collections::HashMap::new()));
        let runtimes = crate::runtime_manager::test_runtime_manager(&vendors).provider(
            session_id.to_string(),
            "incarnation".to_string(),
            false,
            "none".to_string(),
            session_spec(),
        );
        Loading {
            session,
            session_id,
            key: AgentKey::Main,
            agent: AgentId::new_v4(),
            narrate: false,
            runtimes,
            registry: Arc::new(RwLock::new(std::collections::HashMap::new())),
            mcp: None,
            memory: None,
            services: None,
            plugin_library: None,
            last_client: Mutex::new(None),
        }
    }

    fn session_spec() -> crate::sessions::spec::SessionSpec {
        crate::sessions::spec::SessionSpec {
            name: Some("test".into()),
            kind: crate::sessions::spec::SessionKind::Agent {
                settings: settings(),
            },
            workspaces: vec![crate::sessions::spec::WorkspaceDef {
                name: "main".into(),
            }],
            provision: vec![],
            vendor: "none".into(),
            plugins: vec![],
            origin: crate::sessions::spec::SessionOrigin::User,
            environment: None,
            env_vars: vec![],
        }
    }

    /// A session mailbox that takes every command and does nothing with it.
    /// A capability's `setup` only ever needs the address, never an answer.
    struct Inert;

    #[async_trait::async_trait]
    impl horsie_actor::EventSourcedActor for Inert {
        type Command = crate::sessions::addressing::SessionInbox;
        type Event = ();
        type State = ();

        fn persistence_id(&self) -> horsie_actor::PersistenceId {
            horsie_actor::PersistenceId::new("capability-test", "inert")
        }

        fn initial_state() {}

        fn apply_event((): (), (): ()) {}

        async fn handle_command(
            &mut self,
            (): &(),
            _cmd: crate::sessions::addressing::SessionInbox,
            _ctx: &mut horsie_actor::ActorContext<crate::sessions::addressing::SessionInbox>,
        ) -> horsie_actor::CommandEffect<()> {
            horsie_actor::CommandEffect::none()
        }
    }

    /// A toolbox that answers nothing, standing in for whatever a real run
    /// would have wrapped or dispatched to.
    ///
    /// A prompt-only agent wraps no sandbox, so the inner end of a composition
    /// is this in a test.
    #[must_use]
    pub fn nothing() -> Arc<dyn horsie_agentcore::Toolbox> {
        Arc::new(horsie_agentcore::EmptyToolbox)
    }

    /// Which capability a call to `name` becomes a command for.
    ///
    /// Settled by the layer that claimed the name, so the answer is read off
    /// the command the *composed toolbox* produced — the same object the run
    /// hands the model — rather than by asking each capability whether a name
    /// is its.
    ///
    /// `None` when nothing claimed it: the call went straight through to
    /// whatever the layers wrap, which in a real run is the sandbox.
    pub async fn claimed_by(
        caps: &Capabilities,
        facts: &AgentFacts,
        name: &str,
    ) -> Option<&'static str> {
        let recorder = Arc::new(Recorder::default());
        let mailbox: Arc<dyn Mailbox> = Arc::clone(&recorder) as Arc<dyn Mailbox>;
        let _ = caps
            .layer(nothing(), facts, &mailbox)
            .execute(name, serde_json::json!({}), "t1")
            .await;
        *recorder.owner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A mailbox that remembers whose command reached it.
    #[derive(Default)]
    struct Recorder {
        owner: std::sync::Mutex<Option<&'static str>>,
    }

    #[async_trait::async_trait]
    impl Mailbox for Recorder {
        async fn send(
            &self,
            make: Box<dyn FnOnce(ToolReply) -> AgentCommand + Send>,
        ) -> Result<horsie_agentcore::ToolOutcome, horsie_agentcore::ToolCallError> {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let cmd = make(horsie_actor::ReplyTo::from_sender(tx));
            *self.owner.lock().unwrap_or_else(|e| e.into_inner()) = cmd.capability();
            // Nothing answers it: the command is the answer this is asking for.
            drop((cmd, rx));
            Ok(horsie_agentcore::ToolOutcome::Result(
                serde_json::Value::Null,
            ))
        }
    }

    /// A mailbox with no actor behind it.
    ///
    /// Advertising never sends anything, so composing a layer to ask what it
    /// claims needs no actor and no tokio runtime — which is the whole reason
    /// [`Capability::layer`] is handed a mailbox rather than holding one.
    #[must_use]
    pub fn nobody() -> Arc<dyn Mailbox> {
        Arc::new(Nobody)
    }

    struct Nobody;

    #[async_trait::async_trait]
    impl Mailbox for Nobody {
        async fn send(
            &self,
            _make: Box<dyn FnOnce(ToolReply) -> AgentCommand + Send>,
        ) -> Result<horsie_agentcore::ToolOutcome, horsie_agentcore::ToolCallError> {
            panic!("a test composed a layer and then called it with no actor behind it");
        }
    }

    /// The toolbox an equipped agent would run with, wrapping `inner`.
    ///
    /// What a run builds after `provide`, minus only the mailbox: the layers,
    /// in the order the agent actor applies them.
    #[must_use]
    pub fn composed(
        caps: &Capabilities,
        inner: Arc<dyn horsie_agentcore::Toolbox>,
        facts: &AgentFacts,
    ) -> Arc<dyn horsie_agentcore::Toolbox> {
        caps.layer(inner, facts, &nobody())
    }

    /// What one capability adds to the toolbox it wraps.
    ///
    /// Empty for a capability with no tools of its own, which is what "wraps
    /// nothing" looks like from outside: [`Capability::layer`] hands `inner`
    /// straight back, so there is no layer at all rather than one that only
    /// forwards.
    #[must_use]
    pub fn advertised_by(cap: &Capability, facts: &AgentFacts) -> Vec<String> {
        specs_of(cap, facts).into_iter().map(|s| s.name).collect()
    }

    /// The same, in full: what a capability shows the model, schemas and all.
    ///
    /// Read off its composed layer rather than a `specs()` of its own, because
    /// what a capability advertises and what a call to it becomes are now one
    /// declaration — so there is no list of specs to ask for separately.
    #[must_use]
    pub fn specs_of(cap: &Capability, facts: &AgentFacts) -> Vec<ToolSpec> {
        cap.layer(nothing(), facts, &nobody()).specs()
    }

    /// What an equipped agent advertises, outermost first.
    ///
    /// The question can no longer be put to the list itself: a capability
    /// contributes a toolbox layer rather than a list of specs, so the answer
    /// comes from the composed toolbox — the same object the run hands the
    /// model, which is what makes an assertion here an assertion about what the
    /// model was actually shown.
    #[must_use]
    pub fn advertised(caps: &Capabilities, facts: &AgentFacts) -> Vec<String> {
        composed(caps, nothing(), facts)
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    /// The names the composed toolbox advertises, innermost first.
    ///
    /// What a `setup` test asserts on now that a spec holds real toolboxes
    /// rather than a list of layer names: the question "is this tool equipped?"
    /// is answered by asking the thing the agent will actually run with.
    #[must_use]
    pub fn equipped(spec: AgentSpec) -> Vec<String> {
        spec.toolbox().map_or_else(Vec::new, |t| {
            t.specs().into_iter().map(|s| s.name).collect()
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::testing::*;
    use super::*;
    use horsie_agentcore::ToolSpec;

    fn caps(list: Vec<FakeCapability>) -> Capabilities {
        Capabilities::new(list.into_iter().map(Capability::Fake).collect())
    }

    /// An agent equipped with these and nothing folded yet.
    fn agent(list: Vec<FakeCapability>) -> AgentState {
        AgentState {
            capabilities: caps(list),
            ..AgentState::default()
        }
    }

    /// **A name resolves to one capability, and the layer that claimed it is
    /// what decided.** Two capabilities claiming one name is settled when their
    /// layers wrap, never by a scan at call time — so what a call becomes is
    /// read off the composed toolbox the run actually hands the model.
    #[tokio::test]
    async fn a_call_becomes_the_command_of_the_capability_that_claimed_it() {
        let caps = caps(vec![
            FakeCapability::new("first"),
            FakeCapability::new("second"),
        ]);
        assert_eq!(
            claimed_by(&caps, &facts(), "second").await,
            Some("fake"),
            "a claimed name did not become its capability's command"
        );
    }

    /// A name nobody claimed becomes no command at all: it goes to whatever the
    /// layers wrap, which in a real run is the sandbox.
    #[tokio::test]
    async fn a_name_nobody_claimed_becomes_no_command() {
        let caps = caps(vec![FakeCapability::new("only")]);
        assert!(claimed_by(&caps, &facts(), "nope").await.is_none());
    }

    /// Whose command an arm is, for the diagnostic when that capability is not
    /// equipped. A command that could not name its owner would reach the model
    /// as a call that never returns rather than as an error naming what is
    /// missing.
    #[test]
    fn a_command_names_the_capability_that_answers_it() {
        assert_eq!(call("only").capability(), Some("fake"));
    }

    /// **A clone cannot diverge from a reload.**
    ///
    /// The list is cloned every time an agent is equipped, and read back from
    /// the journal every time one loads, so the two have to agree. They used to
    /// agree because both went through a hand-written `save()`, and the risk
    /// worth a test was a `save()` that rebuilt from config and dropped what
    /// the agent had folded.
    ///
    /// Now the list holds no folded state at all, so the two paths are the
    /// derives and cannot differ. What is left to pin is that the *config*
    /// survives both — a capability whose config were rebuilt from a default
    /// would hand a routine's agent an `ask_user` it must not have. The other
    /// half of the old property, that what the agent folded survives a reload,
    /// moved to `AgentState` with the state; see
    /// `a_reload_keeps_what_the_agent_folded` there.
    #[test]
    fn a_round_trip_and_a_clone_both_carry_the_config() {
        let caps = Capabilities::new(vec![
            Capability::AskUser(ask_user::AskUserCapability::unattended()),
            Capability::SubAgent(sub_agent::SubAgentCapability::new(settings(), 3)),
        ]);

        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        for (what, list) in [("the reload", read), ("the clone", caps.clone())] {
            let [Capability::AskUser(ask), Capability::SubAgent(sub)] =
                list.iter().collect::<Vec<_>>()[..]
            else {
                panic!("{what} changed which capabilities these are");
            };
            assert_eq!(
                ask.mute,
                Some(ask_user::Mute::Unattended),
                "{what} un-muted an agent nobody is watching"
            );
            assert_eq!(
                sub.depth, 3,
                "{what} lost the depth its gate is answered from"
            );
        }
    }

    /// A per-agent capability is added at the fixed-name end. Appended instead,
    /// it would sit behind the capability that claims every call it is offered.
    #[test]
    fn push_front_puts_a_fixed_name_ahead_of_the_open_namespace() {
        let mut caps = caps(vec![FakeCapability::new("shared")]);
        caps.push_front(Capability::Fake(FakeCapability::new("shared")));

        // Both claim the same name; the front one answers.
        assert_eq!(caps.iter().count(), 2);
        assert_eq!(
            advertised(&caps, &facts()),
            vec!["shared"],
            "a name claimed twice is advertised once, by whoever will answer it"
        );
    }

    /// **Wrapping order is precedence.** The first capability in the list ends
    /// up outermost, so when two claim one name it is the first one's tool the
    /// model is shown — and the first one's layer that would answer a call.
    ///
    /// This is the ordering the layering exists to collapse: there used to be
    /// two, one for offering and one for wrapping, and they read opposite ways.
    /// Folding the list the other way round still compiles, still advertises
    /// one tool per name, and silently hands every contested name to the wrong
    /// capability.
    #[test]
    fn the_first_capability_in_the_list_wraps_outermost() {
        let caps = caps(vec![
            FakeCapability::describing("shared", "the first one's"),
            FakeCapability::describing("shared", "the second one's"),
        ]);
        let specs = caps.layer(nothing(), &facts(), &nobody()).specs();
        let [spec] = specs.as_slice() else {
            panic!("one name, advertised once, got {specs:?}");
        };
        assert_eq!(
            spec.description, "the first one's",
            "the outermost layer is not the first capability"
        );
    }

    /// A tool call for a name nobody claims is not the layers' business: it
    /// goes straight through to whatever the layers wrap, which in a real run
    /// is the sandbox. Every `bash` call in every session takes this path.
    #[tokio::test]
    async fn an_unclaimed_name_passes_straight_through_the_layers() {
        let caps = caps(vec![
            FakeCapability::new("first"),
            FakeCapability::new("second"),
        ]);
        let refuses: Arc<dyn Mailbox> = Arc::new(RefusingMailbox);
        let sandbox: Arc<dyn Toolbox> = Arc::new(Sandbox);
        let outcome = caps
            .layer(sandbox, &facts(), &refuses)
            .execute("bash", serde_json::Value::Null, "t1")
            .await
            .expect("the sandbox answers");
        assert_eq!(
            outcome,
            horsie_agentcore::ToolOutcome::Result(serde_json::Value::String(
                "sandbox ran bash".into()
            ))
        );
    }

    /// A toolbox that answers one name, standing in for the sandbox the layers
    /// wrap.
    #[derive(Debug)]
    struct Sandbox;

    #[async_trait::async_trait]
    impl Toolbox for Sandbox {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "bash".into(),
                description: String::new(),
                input_schema: serde_json::json!({"type": "object"}),
            }]
        }

        async fn execute(
            &self,
            name: &str,
            _input: serde_json::Value,
            _id: &str,
        ) -> Result<horsie_agentcore::ToolOutcome, horsie_agentcore::ToolCallError> {
            Ok(horsie_agentcore::ToolOutcome::Result(
                serde_json::Value::String(format!("sandbox ran {name}")),
            ))
        }
    }

    /// A mailbox that fails the test if a call reaches it.
    #[derive(Debug)]
    struct RefusingMailbox;

    #[async_trait::async_trait]
    impl Mailbox for RefusingMailbox {
        async fn send(
            &self,
            _make: Box<dyn FnOnce(ToolReply) -> AgentCommand + Send>,
        ) -> Result<horsie_agentcore::ToolOutcome, horsie_agentcore::ToolCallError> {
            panic!("a name nobody claims must never reach the mailbox");
        }
    }

    /// Nothing equipped is a real state — a capability set can be entirely
    /// prompt — so it is `None`/empty rather than an error.
    #[test]
    fn an_empty_set_claims_nothing() {
        let state = agent(Vec::new());
        assert!(state.capabilities.is_empty());
        assert!(advertised(&state.capabilities, &facts()).is_empty());
        assert!(state.capabilities.fake("x").is_none());
        assert!(state.capabilities.timers().is_none());
    }
}

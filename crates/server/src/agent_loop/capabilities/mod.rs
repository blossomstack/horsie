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
//! built by the claim that owns the name. Nothing downstream of
//! [`crate::agent_loop::toolbox::compose`] ever sees a tool name: which
//! capability answers is decided by which arm was constructed, so a name is
//! resolved once, at compose time, rather than by a scan at call time.
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
//! The enabled list's order is the order the model is shown these tools in, and
//! nothing else. It is not precedence: a name belongs to exactly one capability
//! or [`compose`](crate::agent_loop::toolbox::compose) refuses to build the
//! toolbox at all, so there is nothing left for an order to resolve. The one
//! fallthrough that remains is agent-owned names first, then the sandbox's open
//! namespace.

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

use crate::agent_loop::state::AgentState;
use crate::agent_loop::toolbox::ClaimedTool;
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::loading::{AgentFacts, AgentSpec, Loading};
use horsie_actor::ReplyTo;
use horsie_agentcore::{ToolCallError, ToolOutcome};
use serde::{Deserialize, Serialize};

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
            // equipped entirely by the tools they claim, which are composed
            // later and on the run's own task.
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

    /// The tools this capability answers for, each paired with the command a
    /// call to it becomes.
    ///
    /// Declaring the two together is what rules out a name that was claimed and
    /// cannot be mapped. A claimed name becomes one of this capability's own
    /// commands and reaches [`Self::command`] on the actor's mailbox, so a tool
    /// can park, journal and ask the session; an unclaimed one never touches
    /// the mailbox and goes straight to the sandbox.
    ///
    /// Empty for a capability with no tools of its own, and empty is the honest
    /// answer rather than a special case: it contributes nothing to
    /// [`compose`](crate::agent_loop::toolbox::compose), so it costs the
    /// model's calls nothing.
    ///
    /// [`AgentFacts`] rather than nothing, because an advertisement can depend
    /// on what the load found: `sub_agent` lists the installed agent types, and
    /// only the workspace scan knows them. Facts are why the toolbox is composed
    /// on the run's own task after `provide` rather than when the agent is
    /// equipped — `runtime`'s scan is what found them, and it runs later.
    #[must_use]
    pub(crate) fn claims(&self, facts: &AgentFacts) -> Vec<ClaimedTool> {
        match self {
            Self::AskUser(c) => c.claims(),
            Self::Title(c) => c.claims(),
            Self::SubAgent(c) => c.claims(facts),
            Self::Workflow(c) => c.claims(),
            Self::StepResult(c) => c.claims(),
            Self::TaskList(c) => c.claims(),
            Self::Timers(c) => c.claims(),
            // No tools of their own. `fork` is typed by a person; the rest push
            // their tools into the sandbox from `setup`, on the agent's own
            // task.
            Self::Fork(_)
            | Self::TokenBudget(_)
            | Self::ControlPlane(_)
            | Self::Memory(_)
            | Self::Mcp(_)
            | Self::Runtime(_) => Vec::new(),
            #[cfg(test)]
            Self::Fake(c) => c.claims(),
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

/// What an agent is equipped with, in the order its tools are advertised.
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

    /// Equip one more capability.
    ///
    /// The only way to add one, because position no longer decides anything: a
    /// name belongs to exactly one capability, and the sandbox is underneath all
    /// of them however this list is ordered. Where a capability lands only
    /// changes where its tools appear in what the model is shown.
    pub fn push(&mut self, cap: Capability) {
        self.0.push(cap);
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Capability> {
        self.0.iter()
    }

    /// The equipped list, for [`compose`](crate::agent_loop::toolbox::compose).
    #[must_use]
    pub(crate) fn as_slice(&self) -> &[Capability] {
        &self.0
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
    use crate::agent_loop::toolbox::{advertise, claims};
    use crate::agent_loop::{AgentCommand, AgentDomainEvent};
    use horsie_agentcore::{ToolSpec, Toolbox};
    use std::sync::Arc;

    /// A tool call, as the claim that owns the name would have built one.
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

        pub(crate) fn claims(&self) -> Vec<ClaimedTool> {
            let tool = self.tool.clone();
            vec![ClaimedTool::new(
                ToolSpec {
                    name: self.tool.clone(),
                    description: self.description.clone(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
                move |_input, to| AgentCommand::FakeCall {
                    tool: tool.clone(),
                    answering: to,
                },
            )]
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

    /// The fake's own command, as its claim would build it.
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
            role: crate::sessions::runners::loading::AgentRole::Root,
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

    /// Which capability a call to `name` becomes a command for.
    ///
    /// Read off the claim table [`compose`](crate::agent_loop::toolbox::compose)
    /// builds — the one place a name is resolved for this agent, and the same
    /// table the toolbox the run hands the model looks a call up in — rather
    /// than by asking each capability whether a name is its.
    ///
    /// `None` when nothing claimed it: the call goes straight to the sandbox.
    #[must_use]
    pub fn claimed_by(caps: &Capabilities, facts: &AgentFacts, name: &str) -> Option<&'static str> {
        claims(caps.as_slice(), facts)
            .expect("the capabilities under test claim one name each")
            .into_iter()
            .find(|t| t.spec().name == name)?
            .command(serde_json::json!({}), answering("t1"))
            .capability()
    }

    /// An actor reference nothing is listening on.
    ///
    /// Every helper here answers a question about *advertisement*, which never
    /// sends anything — so the reference is only needed to build the toolbox at
    /// all. A test that executes a claimed tool wants a real agent instead.
    #[must_use]
    pub fn nobody() -> horsie_actor::ActorRef<AgentCommand> {
        crate::testing::spawn_detached(
            &horsie_actor::ActorSystem::new(Arc::new(horsie_actor::InMemoryJournal::new())),
            Unreachable,
        )
    }

    /// Fails the test if a claimed tool is executed against it.
    pub struct Unreachable;

    #[async_trait::async_trait]
    impl horsie_actor::EventSourcedActor for Unreachable {
        type Command = AgentCommand;
        type Event = ();
        type State = ();

        fn persistence_id(&self) -> horsie_actor::PersistenceId {
            horsie_actor::PersistenceId::new("capability-test", "unreachable")
        }

        fn initial_state() {}

        fn apply_event((): (), (): ()) {}

        async fn handle_command(
            &mut self,
            (): &(),
            _cmd: AgentCommand,
            _ctx: &mut horsie_actor::ActorContext<AgentCommand>,
        ) -> horsie_actor::CommandEffect<()> {
            panic!("an unclaimed tool call must never reach the agent");
        }
    }

    /// The toolbox an equipped agent would run with over `inner`.
    ///
    /// What a run builds after `provide`, minus only the agent behind it.
    ///
    /// # Panics
    /// If two of these capabilities claim one tool name, which is the
    /// composition error a real run would refuse to start on.
    #[must_use]
    pub fn composed(
        caps: &Capabilities,
        inner: Arc<dyn Toolbox>,
        facts: &AgentFacts,
    ) -> Arc<dyn Toolbox> {
        crate::agent_loop::toolbox::compose(caps.as_slice(), facts, inner, nobody())
            .expect("the capabilities under test claim one name each")
    }

    /// What one capability contributes to the composed toolbox.
    ///
    /// Empty for a capability with no tools of its own, which is what "adds
    /// nothing" looks like from outside.
    #[must_use]
    pub fn advertised_by(cap: &Capability, facts: &AgentFacts) -> Vec<String> {
        specs_of(cap, facts).into_iter().map(|s| s.name).collect()
    }

    /// The same, in full: what a capability shows the model, schemas and all.
    ///
    /// Read off its claims rather than a `specs()` of its own, because what a
    /// capability advertises and what a call to it becomes are one declaration
    /// — so there is no list of specs to ask for separately.
    #[must_use]
    pub fn specs_of(cap: &Capability, facts: &AgentFacts) -> Vec<ToolSpec> {
        cap.claims(facts).iter().map(|t| t.spec().clone()).collect()
    }

    /// What an equipped agent advertises over an empty sandbox, in order.
    ///
    /// The same function the composed toolbox's `specs()` calls, so an
    /// assertion here is an assertion about what the model is actually shown —
    /// and it needs no agent behind it, because advertising never sends
    /// anything.
    ///
    /// # Panics
    /// If two of these capabilities claim one tool name.
    #[must_use]
    pub fn advertised(caps: &Capabilities, facts: &AgentFacts) -> Vec<String> {
        let claimed = claims(caps.as_slice(), facts)
            .expect("the capabilities under test claim one name each");
        advertise(&claimed, &horsie_agentcore::EmptyToolbox)
            .into_iter()
            .map(|s| s.name)
            .collect()
    }

    /// The names the sandbox a `setup` built advertises.
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
    use horsie_agentcore::{ToolSpec, Toolbox};
    use std::sync::Arc;

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

    /// **A name resolves to one capability, and the claim it came from is what
    /// decided.** Resolution happens once, when the toolbox is composed, never
    /// by a scan at call time — so what a call becomes is read off the claim
    /// table the run actually hands the model.
    #[test]
    fn a_call_becomes_the_command_of_the_capability_that_claimed_it() {
        let caps = caps(vec![
            FakeCapability::new("first"),
            FakeCapability::new("second"),
        ]);
        assert_eq!(
            claimed_by(&caps, &facts(), "second"),
            Some("fake"),
            "a claimed name did not become its capability's command"
        );
    }

    /// A name nobody claimed becomes no command at all: it goes to the sandbox.
    #[test]
    fn a_name_nobody_claimed_becomes_no_command() {
        let caps = caps(vec![FakeCapability::new("only")]);
        assert!(claimed_by(&caps, &facts(), "nope").is_none());
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

    /// **Two capabilities claiming one name is a construction error.** It used
    /// to be a silent precedence win — whichever sat earlier in the list
    /// answered, and the loser's tool simply vanished from what the model was
    /// shown, with nothing anywhere saying so.
    ///
    /// Nothing a person or a model can cause: the enabled list is assembled by
    /// first-party code. So it is caught where the list is turned into a
    /// toolbox, which is the last moment anything knows both claims exist.
    #[test]
    fn two_capabilities_claiming_one_name_is_a_composition_error() {
        let caps = caps(vec![
            FakeCapability::describing("shared", "the first one's"),
            FakeCapability::describing("shared", "the second one's"),
        ]);
        let conflict = crate::agent_loop::toolbox::claims(caps.as_slice(), &facts())
            .expect_err("a name claimed twice must not compose");
        assert_eq!(conflict.name, "shared");
        assert_eq!((conflict.held_by, conflict.claimed_by), ("fake", "fake"));
    }

    /// A capability that claims a sandbox name is advertised *once*, by the
    /// capability — it is what will answer the call, so a second copy of the
    /// name would show the model a tool nothing behind it will ever run.
    #[test]
    fn a_capability_claiming_a_sandbox_name_is_advertised_once() {
        let claims = crate::agent_loop::toolbox::claims(
            caps(vec![FakeCapability::describing("bash", "the agent's own")]).as_slice(),
            &facts(),
        )
        .expect("one claim");
        let specs = crate::agent_loop::toolbox::advertise(&claims, &Sandbox);
        let [spec] = specs.as_slice() else {
            panic!("one name, advertised once, got {specs:?}");
        };
        assert_eq!(spec.description, "the agent's own");
    }

    /// A tool call for a name nobody claims is not the agent's business: it
    /// goes straight to the sandbox. Every `bash` call in every session takes
    /// this path, and the agent behind this composition panics if it is
    /// reached.
    #[tokio::test]
    async fn an_unclaimed_name_passes_straight_through_to_the_sandbox() {
        let caps = caps(vec![
            FakeCapability::new("first"),
            FakeCapability::new("second"),
        ]);
        let sandbox: Arc<dyn Toolbox> = Arc::new(Sandbox);
        let outcome = composed(&caps, sandbox, &facts())
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

    /// A toolbox that answers one name, standing in for the sandbox the agent's
    /// own tools sit over.
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

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
//! runner, cancelling an agent, naming the session. A capability asks for those
//! with [`Act::Ask`] and gets a [`SessionReply`] back, which is one request and
//! one answer rather than a share of the session's state.
//!
//! # Park and resume is why this exists
//!
//! A tool that returns a value never needed any of this: the toolbox could
//! answer it and the run would carry on. A tool that *parks* had no way to say
//! so — it could stop the run, but it could not record what it was waiting for,
//! because the place to record it was the agent's own journal.
//!
//! So the two verbs that matter are [`Act::Park`] and [`Act::Resume`]. Parking
//! leaves a dangling `tool_use` and ends the turn; resuming supplies the results
//! that pair with it and starts the next one. Everything else a capability can
//! ask for is a convenience beside those two.
//!
//! # Offer and broadcast
//!
//! [`Capabilities::offer`] hands a message to each capability in order until one
//! takes it, and `None` from all of them is an error at the one place the scan
//! lives. [`Capabilities::broadcast`] hands it to every one of them.
//!
//! Which mode a message gets is carried by [`Msg::routing`] rather than by the
//! caller, so there is no table above this to keep in step. A tool call is
//! offered because exactly one capability owns a name. A turn boundary is
//! broadcast because a turn ending is news for all of them: the ask holds a
//! park open across it, the step result counts its nudges by it, and the
//! subagent list checks it for children still outstanding.
//!
//! Order is therefore the conflict resolution for tool calls, and it is a
//! written property of assembly: the open-namespace capabilities — the sandbox
//! above all — sort last, because they answer for a namespace nobody can
//! enumerate. See [`Capabilities::push_front`].

pub mod ask_user;
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

use crate::agent_loop::{AskAnswer, Incoming};
use crate::sessions::runners::ids::{AgentId, RunnerId, RunnerKind};
use crate::sessions::runners::loading::{AgentFacts, AgentSpec, Loading};
use crate::sessions::runners::message::{ChildMsg, Command, ToolCall};
use horsie_agentcore::Toolbox;
use horsie_models::agent::ToolResultInput;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Something reaching an agent's capabilities.
///
/// Borrowed rather than owned because the same message is handed to one
/// capability after another until it is claimed; whoever claims it clones what
/// it keeps.
#[derive(Debug)]
pub enum Msg<'a> {
    /// A tool the model called, and what the run that called it found.
    ///
    /// The facts ride with the call because they are the same ones the
    /// advertisement was built from — [`Capability::layer`] is composed on that
    /// run's task from this very value — so a refusal here names exactly the
    /// catalogue the model was shown. Nothing else carries them: facts exist for
    /// as long as a run does, and a tool call is the only message that is always
    /// inside one.
    Tool {
        call: &'a ToolCall,
        facts: &'a AgentFacts,
    },
    /// A `/builtin` the person typed, already parsed.
    Command(&'a Command),
    /// This agent's turn reached a boundary.
    Turn(TurnEvent),
    /// Every question this agent was parked on has been answered.
    ///
    /// All of them at once: a half-answered park cannot resume, because the
    /// next provider call would carry a `tool_use` with no result.
    Answer(&'a [AskAnswer]),
    /// A runner this agent's capability created moved.
    Child(&'a ChildMsg),
    /// The session answered something a capability asked it for.
    Reply(&'a SessionReply),
    /// A sleep a capability asked for with [`Act::Wake`] has elapsed.
    ///
    /// One capability owns each id, because the capability that asked is the
    /// one that minted it. A capability that does not recognise the id answers
    /// `None`, and that is how a sleep for a timer that has since been
    /// cancelled is dropped: the sleep itself cannot be called back.
    Woke { id: &'a str },
    /// This agent's work is finished and its result has been delivered.
    ///
    /// Not a turn boundary, and the difference is the whole reason it is its
    /// own arm: a turn ends many times before the work does, and what a
    /// capability should do at the two is opposite. A timer is armed *so that*
    /// a turn ending is not the end — and is moot the moment the agent says it
    /// is done, because nothing is left for it to wake.
    Concluded,
    /// This agent has finished folding its journal.
    ///
    /// The one message that is not news about something happening: it says the
    /// fold is complete, so whatever a capability is still holding now is
    /// everything the dead process left it.
    ///
    /// That is what closes the crash window. A request is journaled *before*
    /// it is sent, so a `Requested` with no answer folded after it may never
    /// have reached the session at all — and the model is parked on a call
    /// nobody will ever answer. A capability holding one re-emits
    /// [`Act::Ask`] here, with the ids it already recorded, which is what
    /// makes the second ask recognisable as a repeat rather than a new
    /// request.
    Loaded,
}

/// A turn boundary, as a capability sees it.
///
/// Four arms because a capability holding a park has to tell them apart: a turn
/// that *ended* may have abandoned the park, while one that failed or was
/// cancelled leaves it exactly where it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEvent {
    Began,
    Ended,
    Failed,
    Cancelled,
}

/// How a message finds its capabilities.
///
/// The variant decides, so the discipline lives in the type rather than in a
/// table above it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routing {
    /// Hand it to each in order until one takes it.
    Offer,
    /// Hand it to every one of them.
    Broadcast,
}

impl Msg<'_> {
    /// Whether this is offered around or broadcast.
    #[must_use]
    pub fn routing(&self) -> Routing {
        match self {
            // Exactly one capability owns a tool name, a slash command, a
            // child, or the request a reply answers.
            Self::Tool { .. } | Self::Command(_) | Self::Child(_) | Self::Reply(_) => {
                Routing::Offer
            }
            // And so is a wake: the capability that asked for the sleep minted
            // the id, so exactly one of them can answer for it.
            Self::Woke { .. } => Routing::Offer,
            // An answer set is offered too: the capability holding the park is
            // the one that recorded it, and no other can claim it.
            Self::Answer(_) => Routing::Offer,
            // A turn ending is news for all of them.
            Self::Turn(_) => Routing::Broadcast,
            // And so is a load: any of them may be holding a request the dead
            // process never got to send, so offering would stop at the first
            // and leave the rest parked for ever.
            Self::Loaded => Routing::Broadcast,
            // The agent finishing is news for every capability holding
            // something meant to wake it, not just the first.
            Self::Concluded => Routing::Broadcast,
        }
    }

    /// What this message is, for the diagnostic when nothing claims it.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Tool { call, .. } => format!("tool call `{}`", call.name),
            Self::Command(c) => format!("command `/{}`", c.name),
            Self::Turn(t) => format!("turn {t:?}"),
            Self::Answer(a) => format!("{} answer(s)", a.len()),
            Self::Child(c) => format!("child {}", c.child()),
            Self::Reply(r) => format!("session reply for call {}", r.call()),
            Self::Woke { id } => format!("wake for {id}"),
            Self::Concluded => "conclusion".to_string(),
            Self::Loaded => "load".to_string(),
        }
    }
}

/// What a capability decided: events for its own state, acts for the agent
/// actor to perform.
///
/// A struct rather than a tuple because both halves are lists, and a tuple of
/// two `Vec`s reads the same in either order — the one shape where getting it
/// backwards still compiles.
#[derive(Debug, Default)]
pub struct Decision {
    pub events: Vec<CapEvent>,
    pub acts: Vec<Act>,
}

impl Decision {
    /// Journal these, do nothing else.
    #[must_use]
    pub fn record(events: Vec<CapEvent>) -> Self {
        Self {
            events,
            acts: Vec::new(),
        }
    }

    /// Claim a message and do nothing at all — the honest answer for a
    /// broadcast a capability has no opinion about but does not want mistaken
    /// for "not mine".
    #[must_use]
    pub fn noop() -> Self {
        Self::default()
    }

    /// Answer the model, journal nothing.
    ///
    /// A refusal is not a fact about the agent, so it must not reach the log.
    #[must_use]
    pub fn reply(call: &str, text: impl Into<String>) -> Self {
        Self {
            events: Vec::new(),
            acts: vec![Act::Answer {
                call: call.to_string(),
                text: text.into(),
            }],
        }
    }

    /// Answer the model with an error, journalling nothing.
    #[must_use]
    pub fn refuse(call: &str, reason: impl Into<String>) -> Self {
        Self {
            events: Vec::new(),
            acts: vec![Act::Refuse {
                call: call.to_string(),
                reason: reason.into(),
            }],
        }
    }

    #[must_use]
    pub fn then(mut self, act: Act) -> Self {
        self.acts.push(act);
        self
    }
}

/// Something the agent actor should do.
///
/// A short list, and a capability never reaches past it. Growing it is
/// deliberate, in a commit that says why: [`Self::Wake`] is the last one, and
/// it was added because timers needed the one thing none of the others could
/// say — *let this much time pass*.
#[derive(Debug)]
pub enum Act {
    /// Answer a tool call with this text and let the run carry on.
    Answer { call: String, text: String },
    /// Answer nothing and end the turn, leaving `call` dangling.
    ///
    /// The parked agent *is* that dangling `tool_use`: the result arrives
    /// against it, possibly days later, on a process that has since rehydrated
    /// the session.
    ///
    /// `note` says what is being waited for, in words. The actor holds it —
    /// see `AgentState::parked` — because *being* parked governs things no
    /// capability can see: whether the queue may start a turn, and which
    /// dangling calls recovery must not repair. The capability keeps whatever
    /// it needs beyond that, which for `ask_user` is the question itself.
    Park { call: String, note: String },
    /// Supply results for calls left dangling by an earlier [`Self::Park`], and
    /// start a turn carrying them.
    Resume { results: Vec<ToolResultInput> },
    /// This agent's work is finished, and this is its result.
    ///
    /// Not a park, though both stop the run — which is exactly why the old code
    /// could treat `ask_user` and `submit_result` alike and sort them out
    /// afterwards by matching tool names. A park owes a result later; a
    /// conclusion owes nothing ever, and carries an output [`Self::Park`] has
    /// nowhere to put.
    Conclude { output: serde_json::Value },
    /// Do not treat this turn's end as the agent finishing: something this
    /// capability is holding will wake it.
    ///
    /// A verb rather than a claimed-but-empty [`Decision`], because a turn
    /// boundary is *broadcast* and [`Capabilities::broadcast`] merges what comes
    /// back — so "I claimed this" is invisible to the actor by construction,
    /// and only something in the merged result can carry it.
    ///
    /// This is invariant 6: a step whose subagent still owes it a report must
    /// not conclude, and must not be nudged either, because a nudge is for a
    /// turn that ended with *nothing* coming.
    Hold { note: String },
    /// Answer a tool call with an *error*, and let the run carry on.
    ///
    /// Distinct from [`Self::Answer`] because `is_error` is not decoration:
    /// agentcore's loop detector and the nudge budget both read the transcript,
    /// and a step submitting the same invalid outcome five times is exactly
    /// where the difference shows. Most refusals in the tree are plain results
    /// and always were — this is for the one that was not.
    Refuse { call: String, reason: String },
    /// Put something in this agent's own queue.
    Enqueue { item: Incoming },
    /// Record something in this agent's log, where a reader will see it.
    ///
    /// A capability's own events are folded but append nothing a client can
    /// read, which is the trap this exists for: `ask_user` journaling its park
    /// purely as a [`CapEvent`] would leave the question invisible in the UI —
    /// green tests, and only a browser would notice. So what a person should
    /// see is said explicitly, in the vocabulary the log already has.
    Record(Box<horsie_agentcore::LifecycleEvent>),
    /// Wake this agent in `after_secs`, naming the sleep `id`.
    ///
    /// The one thing a capability cannot do for itself: everything else here is
    /// a decision about state it holds, and this is a request for time to pass.
    /// The actor spawns the sleep and sends [`Msg::Woke`] back with the id.
    ///
    /// **Not journaled.** Like [`Self::Ask`], it is re-issued from the
    /// capability's own durable state on [`Msg::Loaded`] — which is what
    /// re-arms an armed timer after a restart, with its *remaining* delay. A
    /// wake in the log would be a second record of a fact the capability
    /// already holds, and the two could disagree.
    ///
    /// The id is minted by the capability, because the capability owns whatever
    /// the wake is for and has to recognise the id when it comes back.
    Wake { id: String, after_secs: u64 },
    /// Ask the session for something only it can do.
    ///
    /// The reply comes back as [`Msg::Reply`], which is why every request
    /// carries the tool call that prompted it: the capability that asked has to
    /// recognise the answer, and by then the turn that made the call may be
    /// long over.
    Ask(SessionRequest),
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
/// `dyn` rather than an enum because the set is composed at runtime — a
/// workflow step's capabilities are built from what that step declared — so it
/// is not knowable at the point a match arm would have to be written.
/// [`CapSlice`] carries persistence instead, which keeps the journal typed
/// without putting the enum back in the dispatch path.
#[async_trait::async_trait]
pub trait Capability: std::fmt::Debug + Send + Sync {
    /// Stable, and the key its events are routed by. An associated const would
    /// read better but makes the trait not dyn-compatible.
    fn name(&self) -> &'static str;

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
    async fn setup(&self, loading: &Loading, spec: &mut AgentSpec) -> Result<(), SetupError> {
        let _ = (loading, spec);
        Ok(())
    }

    /// Release what `setup` acquired. Runs when the agent is unloaded.
    async fn teardown(&self) {}

    /// Wrap the agent's toolbox in this capability's own layer.
    ///
    /// The default wraps nothing: a capability with no tools returns `inner`
    /// untouched, so it costs the model's calls no indirection at all.
    ///
    /// A layer answers the names it claims and passes everything else straight
    /// through — see [`crate::agent_loop::toolbox::claiming`], which is what
    /// every capability here builds its layer with. Claimed names reach
    /// [`Self::handle`] on the actor's mailbox, so a tool can park, journal and
    /// ask the session; unclaimed ones never touch the mailbox.
    ///
    /// **Wrapping order is precedence.** [`Capabilities::layer`] applies the
    /// list back to front, so the *first* capability ends up outermost and wins
    /// a name against everything behind it — the same answer the offer scan
    /// gives, which is why there is now one ordering rather than two that read
    /// opposite ways.
    ///
    /// [`AgentFacts`] rather than nothing, because an advertisement can depend
    /// on what the load found: `sub_agent` lists the installed agent types, and
    /// only the workspace scan knows them. Facts are why layers are composed on
    /// the run's own task after `provide` rather than on the mailbox before it —
    /// a capability that sorts ahead of `runtime`, as `sub_agent` must to win
    /// the `spawn_agent` name, has no scan of its own to read.
    ///
    /// `mailbox` is the agent's own, in toolbox form. Passed in rather than
    /// held, because a capability is persisted state and an address is not; it
    /// is also what lets a test compose a layer with no actor running.
    fn layer(
        &self,
        inner: Arc<dyn Toolbox>,
        facts: &AgentFacts,
        mailbox: &Arc<dyn Toolbox>,
    ) -> Arc<dyn Toolbox> {
        let _ = (facts, mailbox);
        inner
    }

    /// `None` means "not mine".
    ///
    /// One method rather than a `supports` predicate beside a handler, because
    /// a capability that answered yes and then could not cope, and a pair edited
    /// out of step, are states that cannot be written this way.
    fn handle(&self, msg: &Msg) -> Option<Decision>;

    /// Fold one of my own events.
    ///
    /// Pure: no clock, no randomness, no id generation — those belong in
    /// [`Self::handle`], which is a decision rather than a replay. Every
    /// capability is offered every event, so an arm that is not mine is a no-op
    /// rather than an error.
    fn apply(&mut self, event: &CapEvent) {
        let _ = event;
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
    fn carried_state(&self) -> Option<String> {
        None
    }

    /// What a client sees of this capability. `None` when it shows nothing.
    ///
    /// The second of the two named questions that cross the private-state
    /// boundary, and it is a *question* rather than a read: the caller says what
    /// it wants to know and gets a computed answer, never the capability's own
    /// shape. [`Capabilities::slices`] used to serve this by handing every
    /// capability's whole persisted state to anything inside `agent_loop`,
    /// which is a general bypass of the invariant that a capability's state is
    /// its own.
    ///
    /// [`CapView`] is deliberately not [`CapSlice`]. The journal shape is a
    /// durability contract and the view shape is an API contract; tying them
    /// together makes an API change force a journal migration.
    fn view(&self) -> Option<CapView> {
        None
    }

    /// Me, in the form the journal stores.
    fn save(&self) -> CapSlice;
}

/// One capability as it is persisted.
///
/// The whole capability rather than a durable-state extract, so a reload does
/// not depend on assembly reproducing the same config it produced when the
/// agent was first equipped. A capability's config is a fact about the agent,
/// and facts about the agent belong in its state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapSlice {
    AskUser(ask_user::AskUserCapability),
    ControlPlane(control_plane::ControlPlaneCapability),
    Fork(fork::ForkCapability),
    Mcp(mcp::McpCapability),
    Memory(memory::MemoryCapability),
    Runtime(runtime::RuntimeCapability),
    StepResult(step_result::StepResultCapability),
    SubAgent(sub_agent::SubAgentCapability),
    TaskList(task_list::TaskListCapability),
    Timers(timers::TimersCapability),
    Title(title::TitleCapability),
    Workflow(workflow::WorkflowCapability),
    /// A capability with no behaviour of its own, so the round-trip and
    /// folded-state rules have something to be tested against that cannot break
    /// when a real capability changes. Each migration adds its own arm beside
    /// these; nothing deletes this one.
    #[cfg(test)]
    Fake(testing::FakeCapability),
}

impl From<CapSlice> for Box<dyn Capability> {
    fn from(slice: CapSlice) -> Self {
        match slice {
            CapSlice::AskUser(c) => Box::new(c),
            CapSlice::ControlPlane(c) => Box::new(c),
            CapSlice::Fork(c) => Box::new(c),
            CapSlice::Mcp(c) => Box::new(c),
            CapSlice::Memory(c) => Box::new(c),
            CapSlice::Runtime(c) => Box::new(c),
            CapSlice::StepResult(c) => Box::new(c),
            CapSlice::SubAgent(c) => Box::new(c),
            CapSlice::TaskList(c) => Box::new(c),
            CapSlice::Timers(c) => Box::new(c),
            CapSlice::Title(c) => Box::new(c),
            CapSlice::Workflow(c) => Box::new(c),
            #[cfg(test)]
            CapSlice::Fake(c) => Box::new(c),
        }
    }
}

/// One capability's event, tagged with which capability owns it.
///
/// Typed rather than an opaque blob: the journal stays readable, and a shape
/// change fails to compile where it should.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CapEvent {
    AskUser(ask_user::Event),
    ControlPlane(control_plane::Event),
    Fork(fork::Event),
    Mcp(mcp::Event),
    Memory(memory::Event),
    Runtime(runtime::Event),
    StepResult(step_result::Event),
    SubAgent(sub_agent::Event),
    TaskList(task_list::Event),
    Timer(timers::Event),
    Title(title::Event),
    Workflow(workflow::Event),
    #[cfg(test)]
    Fake(testing::FakeEvent),
}

/// What a capability shows a client, in the client's vocabulary.
///
/// One arm per capability that has anything to show, and most have nothing —
/// which is why this is much shorter than [`CapSlice`] and why it must stay a
/// separate enum. `CapSlice` answers "what does the journal store?" and this
/// answers "what does `GET /api/sessions/:id/agents/:id` return?"; a capability
/// that reshapes its persisted state should not be able to change an HTTP
/// response by accident, nor the other way round.
///
/// Prefer widening this over adding a trait method when a third thing needs to
/// cross the boundary. A trait that grows a method per concern is the failure
/// mode this design exists to avoid.
#[derive(Debug, Clone)]
pub enum CapView {
    /// The plan the agent keeps for itself, in list order, as
    /// `AgentStateView.tasks` carries it.
    TaskList(Vec<crate::agent_loop::task_list::TaskRecord>),
}

/// What an agent is equipped with, in the order messages are offered around.
///
/// A newtype so the list round-trips through the journal as `Vec<CapSlice>`
/// with no hydration step: what comes back is what went in, including config.
#[derive(Debug, Default)]
pub struct Capabilities(Vec<Box<dyn Capability>>);

impl Capabilities {
    #[must_use]
    pub fn new(caps: Vec<Box<dyn Capability>>) -> Self {
        Self(caps)
    }

    /// Add a capability at the open-namespace end.
    ///
    /// Only assembly should reach the end: the last capability answers for a
    /// namespace nobody can enumerate, so anything pushed after it is shadowed.
    /// A capability with a fixed tool name wants [`Self::push_front`].
    pub fn push(&mut self, cap: impl Capability + 'static) {
        self.0.push(Box::new(cap));
    }

    /// Equip a capability ahead of everything already here.
    ///
    /// Front rather than back because that is now the only way to win a name:
    /// first in the list is outermost in the toolbox and first in the offer
    /// scan, and the open-namespace sandbox sorts last precisely so that
    /// everything else is ahead of it.
    pub fn push_front(&mut self, cap: impl Capability + 'static) {
        self.0.insert(0, Box::new(cap));
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Box<dyn Capability>> {
        self.0.iter()
    }

    /// Every capability in the form the journal stores it, in offer order.
    ///
    /// **Serialization only, and private for that reason.** This used to be the
    /// way anything inside `agent_loop` read a fact out of a capability, which
    /// made every capability's whole persisted state readable by anything that
    /// asked — the bypass [`Capability::view`] replaces. What a client sees is
    /// a question a capability answers; what the journal stores is nobody
    /// else's business.
    #[must_use]
    fn slices(&self) -> Vec<CapSlice> {
        self.0.iter().map(|c| c.save()).collect()
    }

    /// What every capability shows a client, in offer order.
    ///
    /// Skips the ones with nothing to show, so a caller matching an arm is
    /// asking whether *anything* here answers for it rather than which position
    /// it sits at.
    #[must_use]
    pub fn views(&self) -> Vec<CapView> {
        self.0.iter().filter_map(|c| c.view()).collect()
    }

    /// Everything this agent would lose to a compaction, in offer order.
    #[must_use]
    pub fn carried_state(&self) -> Vec<String> {
        self.0.iter().filter_map(|c| c.carried_state()).collect()
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
        mailbox: &Arc<dyn Toolbox>,
    ) -> Arc<dyn Toolbox> {
        self.0
            .iter()
            .rev()
            .fold(inner, |inner, cap| cap.layer(inner, facts, mailbox))
    }

    /// Hand a message to each capability until one takes it.
    ///
    /// `None` from all of them is an error at the one place this is called,
    /// never a silent drop.
    #[must_use]
    pub fn offer(&self, msg: &Msg) -> Option<Decision> {
        self.0.iter().find_map(|c| c.handle(msg))
    }

    /// Hand a message to every capability and merge what they decided.
    ///
    /// Order is preserved, so a broadcast that produces acts produces them in
    /// the same order the capabilities are offered tool calls in.
    #[must_use]
    pub fn broadcast(&self, msg: &Msg) -> Decision {
        self.0
            .iter()
            .filter_map(|c| c.handle(msg))
            .fold(Decision::default(), |mut all, d| {
                all.events.extend(d.events);
                all.acts.extend(d.acts);
                all
            })
    }

    /// Fold a capability's event into the capability that owns it.
    pub fn apply(&mut self, event: &CapEvent) {
        for cap in &mut self.0 {
            cap.apply(event);
        }
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

impl FromIterator<Box<dyn Capability>> for Capabilities {
    fn from_iter<I: IntoIterator<Item = Box<dyn Capability>>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Clone for Capabilities {
    /// Through the persisted form, so a clone cannot diverge from what a reload
    /// would produce.
    fn clone(&self) -> Self {
        Self(self.slices().into_iter().map(Into::into).collect())
    }
}

impl Serialize for Capabilities {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.slices().serialize(s)
    }
}

impl<'de> Deserialize<'de> for Capabilities {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Self(
            Vec::<CapSlice>::deserialize(d)?
                .into_iter()
                .map(Into::into)
                .collect(),
        ))
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
    use horsie_agentcore::ToolSpec;

    /// A capability with a name, one tool, and one piece of folded state.
    ///
    /// Enough to exercise every composition rule without a real capability
    /// existing yet: it claims its own tool name and nothing else, records what
    /// it was told, and answers `save()` with itself.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct FakeCapability {
        pub tool: String,
        /// What this one's advertisement says, so two fakes claiming the same
        /// name are still tellable apart — which is the only way to see *which*
        /// of them the composed toolbox let through.
        pub description: String,
        /// What broadcasts and events have done to it — the folded state a
        /// `save()` that rebuilt from config would silently drop.
        pub seen: Vec<String>,
        /// Whether this one claims turn boundaries.
        pub watches_turns: bool,
    }

    impl FakeCapability {
        pub fn new(tool: &str) -> Self {
            Self {
                tool: tool.to_string(),
                description: String::new(),
                seen: Vec::new(),
                watches_turns: false,
            }
        }

        /// One that claims `tool` and says so in its own words.
        pub fn describing(tool: &str, description: &str) -> Self {
            Self {
                description: description.to_string(),
                ..Self::new(tool)
            }
        }

        pub fn watching_turns(tool: &str) -> Self {
            Self {
                watches_turns: true,
                ..Self::new(tool)
            }
        }
    }

    /// The fake's only event: it saw something.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct FakeEvent {
        pub tool: String,
        pub what: String,
    }

    #[async_trait::async_trait]
    impl Capability for FakeCapability {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn layer(
            &self,
            inner: Arc<dyn Toolbox>,
            _facts: &AgentFacts,
            mailbox: &Arc<dyn Toolbox>,
        ) -> Arc<dyn Toolbox> {
            crate::agent_loop::toolbox::claiming(
                inner,
                vec![ToolSpec {
                    name: self.tool.clone(),
                    description: self.description.clone(),
                    input_schema: serde_json::json!({"type": "object"}),
                }],
                mailbox,
            )
        }

        fn handle(&self, msg: &Msg) -> Option<Decision> {
            match msg {
                Msg::Tool { call, .. } => (call.name == self.tool).then(|| {
                    Decision::record(vec![CapEvent::Fake(FakeEvent {
                        tool: self.tool.clone(),
                        what: format!("tool:{}", call.name),
                    })])
                }),
                Msg::Turn(t) => self.watches_turns.then(|| {
                    Decision::record(vec![CapEvent::Fake(FakeEvent {
                        tool: self.tool.clone(),
                        what: format!("turn:{t:?}"),
                    })])
                }),
                Msg::Command(_)
                | Msg::Answer(_)
                | Msg::Child(_)
                | Msg::Reply(_)
                | Msg::Woke { .. }
                | Msg::Concluded
                | Msg::Loaded => None,
            }
        }

        fn apply(&mut self, event: &CapEvent) {
            // Every capability is offered every event, so an arm that is not
            // mine is a no-op rather than an error. `let ... else` rather than
            // a match, so a tenth capability is not a change to all nine.
            let CapEvent::Fake(e) = event else { return };
            if e.tool == self.tool {
                self.seen.push(e.what.clone());
            }
        }

        fn save(&self) -> CapSlice {
            CapSlice::Fake(self.clone())
        }
    }

    #[must_use]
    pub fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: name.into(),
            input: serde_json::json!({}),
        }
    }

    /// What a load that found nothing leaves behind: no workspace scan, no
    /// plugin library, no runtime. The facts every capability but `sub_agent`
    /// advertises the same tools under.
    #[must_use]
    pub fn facts() -> AgentFacts {
        AgentFacts::default()
    }

    /// A tool call made under a load that found nothing.
    ///
    /// What every capability but one is tested against: the facts are the
    /// workspace scan, and only `sub_agent` reads them. A capability that wants
    /// facts of its own writes [`Msg::Tool`] out and supplies them.
    #[must_use]
    pub fn tool(call: &ToolCall) -> Msg<'_> {
        static NOTHING: std::sync::LazyLock<AgentFacts> =
            std::sync::LazyLock::new(AgentFacts::default);
        Msg::Tool {
            call,
            facts: &NOTHING,
        }
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

    /// A toolbox that answers nothing, standing in for whatever a real run
    /// would have wrapped or dispatched to.
    ///
    /// Advertising never touches the mailbox and a prompt-only agent wraps no
    /// sandbox, so both ends of a composition are this in a test — and a test
    /// composing a layer therefore needs no actor running.
    #[must_use]
    pub fn nothing() -> Arc<dyn horsie_agentcore::Toolbox> {
        Arc::new(horsie_agentcore::EmptyToolbox)
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
        caps.layer(inner, facts, &nothing())
    }

    /// What one capability adds to the toolbox it wraps.
    ///
    /// Empty for a capability with no tools of its own, which is what "wraps
    /// nothing" looks like from outside: [`Capability::layer`] hands `inner`
    /// straight back, so there is no layer at all rather than one that only
    /// forwards.
    #[must_use]
    pub fn advertised_by(cap: &dyn Capability, facts: &AgentFacts) -> Vec<String> {
        cap.layer(nothing(), facts, &nothing())
            .specs()
            .into_iter()
            .map(|s| s.name)
            .collect()
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
        Capabilities::new(
            list.into_iter()
                .map(|c| Box::new(c) as Box<dyn Capability>)
                .collect(),
        )
    }

    /// The first capability to claim a name wins, so order is the conflict
    /// resolution for tool calls rather than an accident of construction.
    #[test]
    fn the_first_capability_to_claim_a_tool_wins() {
        let caps = caps(vec![
            FakeCapability::new("first"),
            FakeCapability::new("second"),
        ]);
        let d = caps
            .offer(&tool(&call("second")))
            .expect("someone takes it");
        let Some(CapEvent::Fake(e)) = d.events.first() else {
            panic!("expected the fake's own event, got {:?}", d.events);
        };
        assert_eq!(e.tool, "second");
    }

    /// A call nobody claims is `None` at the one place the scan lives — loudly,
    /// so the actor can say which call went unclaimed rather than dropping it.
    #[test]
    fn a_call_nobody_claims_is_none() {
        let caps = caps(vec![FakeCapability::new("only")]);
        assert!(caps.offer(&tool(&call("nope"))).is_none());
        assert_eq!(
            tool(&call("nope")).describe(),
            "tool call `nope`",
            "the diagnostic has to name the call"
        );
    }

    /// A turn boundary reaches every capability, not the first — the ask holds
    /// a park open across it while the step result counts its nudges by it, and
    /// offering would give it to whichever sorted first.
    #[test]
    fn a_turn_boundary_reaches_every_capability() {
        let caps = caps(vec![
            FakeCapability::watching_turns("a"),
            FakeCapability::watching_turns("b"),
        ]);
        let msg = Msg::Turn(TurnEvent::Ended);
        assert_eq!(msg.routing(), Routing::Broadcast);

        let d = caps.broadcast(&msg);
        let tools: Vec<&str> = d
            .events
            .iter()
            .filter_map(|e| {
                let CapEvent::Fake(e) = e else { return None };
                Some(e.tool.as_str())
            })
            .collect();
        assert_eq!(
            tools,
            vec!["a", "b"],
            "a broadcast that stopped at the first"
        );
    }

    /// And offering the same boundary would have reached only one of them,
    /// which is the bug the routing rule exists to prevent.
    #[test]
    fn offering_a_turn_boundary_would_reach_only_the_first() {
        let caps = caps(vec![
            FakeCapability::watching_turns("a"),
            FakeCapability::watching_turns("b"),
        ]);
        let d = caps
            .offer(&Msg::Turn(TurnEvent::Ended))
            .expect("the first one claims it");
        assert_eq!(d.events.len(), 1);
    }

    /// A tool call is offered, never broadcast: two capabilities answering one
    /// call would produce two results for one `tool_use` id.
    #[test]
    fn a_tool_call_is_offered_and_a_turn_is_broadcast() {
        assert_eq!(tool(&call("x")).routing(), Routing::Offer);
        assert_eq!(Msg::Turn(TurnEvent::Began).routing(), Routing::Broadcast);
        assert_eq!(
            Msg::Answer(&[AskAnswer {
                tool_call_id: "t1".into(),
                text: "yes".into(),
            }])
            .routing(),
            Routing::Offer,
            "the capability holding the park is the one that recorded it"
        );
    }

    /// Cloning goes through `save()`, and the list is cloned every time an agent
    /// is equipped — so a `save()` that rebuilt itself from config instead of
    /// copying itself would silently drop what the agent had folded. Pinning
    /// names cannot catch that; only folded state can.
    #[test]
    fn a_round_trip_carries_the_folded_state_and_not_just_the_config() {
        let mut caps = caps(vec![FakeCapability::new("a")]);
        caps.apply(&CapEvent::Fake(FakeEvent {
            tool: "a".into(),
            what: "tool:a".into(),
        }));

        let written = serde_json::to_string(&caps).expect("write");
        let read: Capabilities = serde_json::from_str(&written).expect("read");
        let CapSlice::Fake(fake) = read.iter().next().expect("one").save() else {
            panic!("the journal changed which capability this is");
        };
        assert_eq!(
            fake.seen,
            vec!["tool:a"],
            "the reload was rebuilt from config and lost what the agent folded"
        );

        // And the in-memory clone takes the same path, so the two cannot drift.
        let CapSlice::Fake(cloned) = caps.clone().iter().next().expect("one").save() else {
            panic!("the clone changed which capability this is");
        };
        assert_eq!(cloned.seen, vec!["tool:a"]);
    }

    /// A per-agent capability is added at the fixed-name end. Appended instead,
    /// it would sit behind the capability that claims every call it is offered.
    #[test]
    fn push_front_puts_a_fixed_name_ahead_of_the_open_namespace() {
        let mut caps = caps(vec![FakeCapability::new("shared")]);
        caps.push_front(FakeCapability::new("shared"));

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
        let empty: Arc<dyn Toolbox> = Arc::new(horsie_agentcore::EmptyToolbox);
        let specs = caps.layer(Arc::clone(&empty), &facts(), &empty).specs();
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
        let refuses: Arc<dyn Toolbox> = Arc::new(RefusingMailbox);
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
    impl Toolbox for RefusingMailbox {
        fn specs(&self) -> Vec<ToolSpec> {
            Vec::new()
        }

        async fn execute(
            &self,
            name: &str,
            _input: serde_json::Value,
            _id: &str,
        ) -> Result<horsie_agentcore::ToolOutcome, horsie_agentcore::ToolCallError> {
            panic!("`{name}` is nobody's: it must never reach the mailbox");
        }
    }

    /// Nothing equipped is a real state — a capability set can be entirely
    /// prompt — so it is `None`/empty rather than an error.
    #[test]
    fn an_empty_set_claims_nothing() {
        let caps = Capabilities::default();
        assert!(caps.is_empty());
        assert!(advertised(&caps, &facts()).is_empty());
        assert!(caps.offer(&tool(&call("x"))).is_none());
        assert!(caps.broadcast(&Msg::Turn(TurnEvent::Ended)).acts.is_empty());
    }
}

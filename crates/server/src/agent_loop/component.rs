//! The contract every component of an agent shares — and everything they are
//! allowed to share.
//!
//! A component is an instantiated struct the actor holds. It owns its own
//! in-memory bookkeeping and the commands routed to it; the actor is a router
//! and keeps no domain logic. Three things are shared, and nothing else:
//!
//! - **[`AgentState`]** — the durable state, moved only by the fold.
//! - **The command/event vocabulary** — a component acts by returning events
//!   in a [`CommandEffect`], and reports its own progress by telling *its own*
//!   commands to the shared mailbox.
//! - **[`StepRun`]** — the transient half of the state: the few in-memory
//!   facts more than one component genuinely reads, deliberately unjournaled.
//!
//! **A component never names another component.** It cannot ask one for
//! anything and cannot tell one anything; the one thing it may say to the
//! world outside itself is [`Cx::advance`] — *something changed, reconsider* —
//! which names nobody. Deciding what happens next is
//! [`Components::advance`]'s job, in [`super::boundary`], and it is the only
//! code that knows what components exist.
//!
//! `apply` folds one component's events into state and must be pure — no I/O,
//! no clock, no step_run. `on_load` repairs what a dead process left behind.

use crate::agent_loop::prelude::*;
use async_trait::async_trait;
use horsie_actor::{ActorContext, CommandEffect};
use tokio_util::sync::CancellationToken;

/// What an agent can be doing off its own mailbox — one thing at a time.
///
/// The mailbox is never blocked, so anything that waits on the outside world
/// runs on a spawned task and reports back. This names which task that is;
/// `None` means the agent is between jobs, which is the only moment
/// [`Components::advance`] may start a new one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepPhase {
    /// Rehydrating the runtime, reconnecting MCP, composing the toolbox.
    Initialize,
    Connect,
    /// A turn's pre-start hooks.
    StartHooks,
    /// One provider call for the current Agent marker.
    CallingProvider,
    /// The current Agent marker's tool batch.
    RunningTools,
    /// The Stop/SubagentStop hook at a settled boundary.
    StopHook,
    /// A tool-less compaction provider call.
    Compaction,
    /// A tool-less seed-summary provider call.
    SeedSummary,
}

/// The transient half of an agent's state: in-memory facts more than one
/// component reads. Everything here is rebuilt or re-decided on recovery,
/// which is exactly why none of it is journaled.
pub(crate) struct StepRun {
    /// Whether this agent's session has a runtime to run on. Seeded at spawn
    /// and moved by the `Runtime` lifecycle records the owner already sends.
    pub ready: bool,
    /// The open history marker every callback must name.
    pub marker_seq: Option<u64>,
    /// The job running off the mailbox, if any. This is the busy gate.
    pub running: Option<StepPhase>,
    /// Cancels everything running for the current marker.
    pub cancel: CancellationToken,
    /// The contexts every kind of work runs against — the provider, the
    /// composed toolbox, the budget, the hooks — published by the provision
    /// component. Shared rather than owned by anyone because a turn, a
    /// compaction and a summary all read the same one.
    pub ctx: Option<std::sync::Arc<TurnCtx>>,
    /// Whether they must be rebuilt before the next work runs. Contexts are
    /// per turn: a rehydrated runtime, a reconnected MCP server or a changed
    /// prompt all arrive this way.
    pub ctx_stale: bool,
    /// The tool calls the actor has dispatched and not yet heard back from.
    /// In-memory only: a crash mid-batch recovers by repairing the journal,
    /// never by re-running a side effect.
    pub calls: Vec<DispatchedCall>,
    /// Calls that ended the run (`ask_user`, `submit_result`), collected as
    /// the batch settles; the turn interprets them once nothing is in flight.
    pub stopped: Vec<horsie_agentcore::StoppedCall>,
    /// What the next provider call should say about tool use. Taken when a
    /// turn starts, so it applies to exactly one turn. Set only when re-running
    /// a turn that ended without the result it owed.
    pub pending_tool_choice: Option<horsie_agentcore::ToolChoice>,
    /// Chunks of the message currently being written, since the newest log
    /// entry. Cleared whenever an entry lands, because the entry supersedes
    /// them. Written by the turn, read by the read path.
    pub deltas: Vec<String>,
}

impl StepRun {
    pub fn new(ready: bool) -> Self {
        Self {
            ready,
            marker_seq: None,
            running: None,
            cancel: CancellationToken::new(),
            ctx: None,
            ctx_stale: true,
            calls: Vec::new(),
            stopped: Vec::new(),
            pending_tool_choice: None,
            deltas: Vec::new(),
        }
    }

    /// Claim the foreground slot for the open history marker.
    pub fn begin(&mut self, kind: StepPhase, marker_seq: u64) -> CancellationToken {
        self.running = Some(kind);
        self.marker_seq = Some(marker_seq);
        self.cancel.clone()
    }

    pub fn live(&self, marker_seq: u64) -> bool {
        self.marker_seq == Some(marker_seq)
    }

    pub fn finished(&mut self, marker_seq: u64) -> bool {
        if !self.live(marker_seq) {
            return false;
        }
        self.running = None;
        true
    }

    /// Stop everything and make every callback for the old marker stale.
    pub fn stop(&mut self) {
        self.cancel.cancel();
        self.cancel = CancellationToken::new();
        self.marker_seq = None;
        self.running = None;
        // The batch died with its turn: the dying tasks' reports are fenced
        // out, and the conclusion repairs whatever dangles.
        self.calls.clear();
        self.stopped.clear();
        // Whatever the cancel interrupted may have been holding a runtime that
        // is going away; the next work builds its own.
        self.ctx_stale = true;
    }
}

/// Everything one turn's steps share, built once by the provision component
/// and published to the shared step_run for whoever needs it.
pub struct TurnCtx {
    pub provider: std::sync::Arc<dyn horsie_agentcore::LlmProvider>,
    /// The fully-composed, selection-filtered toolbox every call dispatches
    /// through — the components' own tools included, indistinguishable from
    /// the rest.
    pub toolbox: std::sync::Arc<dyn horsie_agentcore::Toolbox>,
    /// What the model is shown, already filtered.
    pub specs: Vec<horsie_agentcore::ToolSpec>,
    pub system_prompt: String,
    pub budget: Option<horsie_agentcore::CompactionBudget>,
    pub conversation_id: String,
    pub thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    /// For the compaction hooks a compact run fires.
    pub context_provider: std::sync::Arc<dyn crate::agent_loop::ContextProvider>,
}

/// One tool call on its way to the component that owns the tool.
///
/// Built by a vended [`ActorToolbox`] — the turn never
/// constructs one and never learns the tool was special. Carries the work
/// marker baked in at provisioning time, so a component never acts for a
/// turn that has since been cancelled: the stale call is refused, and the
/// cancel already repaired its dangling `tool_use`.
pub struct RoutedToolCall {
    pub tool_call_id: String,
    pub name: String,
    pub input: serde_json::Value,
    /// Answers the toolbox's `execute`, exactly as any remote tool answers.
    pub reply: horsie_actor::ReplyTo<Result<serde_json::Value, horsie_agentcore::ToolCallError>>,
}

/// One tool call the actor has dispatched, kept until it answers — the name
/// and input a stopper is reported with.
pub(crate) struct DispatchedCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// A toolbox whose tools run on the actor's own mailbox.
///
/// The mechanism behind every toolbox a component vends — the timer toolbox,
/// the task-list toolbox, whatever comes later. `execute` sends the call to
/// the actor as a command — where the owning component runs it over current
/// state and journals its own events — and waits for the answer. That makes
/// such a tool indistinguishable from a remote one at every layer above:
/// composed, filtered, dispatched, and answered on the same channel. The
/// extra mailbox round-trip is the price of having exactly one path.
pub(crate) struct ActorToolbox {
    specs: Vec<horsie_agentcore::ToolSpec>,
    /// Wraps the call in the owning component's command group.
    wrap: fn(RoutedToolCall) -> AgentCommand,
    actor: horsie_actor::ActorRef<AgentCommand>,
}

impl ActorToolbox {
    pub(crate) fn new(
        specs: Vec<horsie_agentcore::ToolSpec>,
        wrap: fn(RoutedToolCall) -> AgentCommand,
        actor: horsie_actor::ActorRef<AgentCommand>,
    ) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { specs, wrap, actor })
    }
}

#[async_trait]
impl horsie_agentcore::Toolbox for ActorToolbox {
    fn specs(&self) -> Vec<horsie_agentcore::ToolSpec> {
        self.specs.clone()
    }

    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        tool_call_id: &str,
    ) -> Result<horsie_agentcore::ToolOutcome, horsie_agentcore::ToolCallError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let call = RoutedToolCall {
            tool_call_id: tool_call_id.to_string(),
            name: name.to_string(),
            input,
            reply: horsie_actor::ReplyTo::from_sender(tx),
        };
        let _ = self.actor.tell((self.wrap)(call)).await;
        match rx.await {
            Ok(Ok(value)) => Ok(horsie_agentcore::ToolOutcome::Result(
                horsie_agentcore::ToolValue {
                    value,
                    artifacts: Vec::new(),
                },
            )),
            Ok(Err(e)) => Err(e),
            // The actor died or refused the marker: either way the turn
            // this call belongs to is over, and the fence drops the report.
            Err(_) => Err(horsie_agentcore::ToolCallError::ExecutionFailed(
                "the agent is no longer running this turn".to_string(),
            )),
        }
    }
}

/// What every component's state can be asked, by code that does not know which
/// one it is holding.
///
/// Both questions are polls over the whole list, and both are things a
/// component added later must be able to answer without anyone editing a
/// central rule. The defaults are the common case: most state neither blocks
/// anything nor survives a branch.
pub(crate) trait PartState: Sized {
    /// Why this part says the agent must not act yet. Vetoes commute — the
    /// order the parts are asked in cannot matter — which is what makes this a
    /// poll rather than another ordered decision.
    fn blocks(&self, _state: &AgentState) -> Option<Blocked> {
        None
    }

    /// What this part contributes to a sub session branched from here, if
    /// anything. Everything that is *about the session* carries; everything in
    /// flight, or that is a bill, does not.
    fn carried(&self) -> Option<Self> {
        None
    }
}

/// Reaching one component's state out of the list, typed.
///
/// Implemented centrally, once per variant, because the implementation only
/// names the variant — never a field — so it cannot become a way in.
pub(crate) trait Part: Sized {
    fn get(parts: &[ComponentState]) -> Option<&Self>;
    fn get_mut(parts: &mut Vec<ComponentState>) -> Option<&mut Self>;
}

/// What a component is handed with every command: the durable state as it
/// stands, the shared step_run, the agent's configuration, and the means to
/// spawn work and reach its own mailbox. Nothing here belongs to another
/// component.
pub(crate) struct Cx<'a> {
    pub state: &'a AgentState,
    pub step_run: &'a mut StepRun,
    pub runtime: &'a crate::agent_loop::context::AgentRuntimeContext,
    pub params: &'a AgentParams,
    pub actor: &'a ActorContext<AgentCommand>,
}

impl Cx<'_> {
    /// Announce that this agent has moved, waking every reader waiting on it.
    /// Announcing twice for one change is harmless.
    pub fn publish_revision(&self) {
        self.runtime.revision.send_modify(|r| *r += 1);
    }

    /// Put one of *this component's own* commands on the mailbox. It is
    /// handled after whatever the current handler persists is durable and
    /// folded.
    pub async fn tell(&self, cmd: AgentCommand) {
        let _ = self.actor.self_ref().tell(cmd).await;
    }

    /// Reconsider what this agent should be doing — the one thing a component
    /// may say to anything other than itself, and it names nobody.
    ///
    /// Rarely needed: the actor advances itself after every durable write, so
    /// this is for the changes that journal nothing at all.
    pub async fn advance(&self) {
        self.tell(AgentCommand::Core(CoreCommand::Advance)).await;
    }
}

/// A component's executor for its routed tool calls: the value the call
/// answers and the component's own events that record it. Pure over the state
/// (plus whatever task it spawns, like a timer's sleep).
pub(crate) type ToolExecutor =
    fn(
        &AgentState,
        &str,
        &serde_json::Value,
        horsie_actor::ActorRef<AgentCommand>,
    ) -> Result<(serde_json::Value, Vec<AgentDomainEvent>), horsie_agentcore::ToolCallError>;

/// Execute a routed tool call — the shared shape of every component-owned
/// tool.
///
/// The component runs the executor over current state, journals *its own*
/// events, and replies the value to the toolbox that asked. The `ToolComplete`
/// is not journaled here: the reply flows back through the toolbox to the same
/// `ToolReturned` path every remote call takes, so the turn records both kinds
/// identically and cannot tell them apart.
///
/// A call from a cancelled marker is refused without executing: the cancel
/// already repaired its dangling `tool_use`, and acting now would leak a side
/// effect. Dropping the reply is the refusal — the toolbox reads a dead
/// channel as "this turn is over".
pub(crate) async fn answer_tool_call(
    call: RoutedToolCall,
    cx: &mut Cx<'_>,
    execute: ToolExecutor,
) -> CommandEffect<AgentDomainEvent> {
    let call_is_open = cx
        .state
        .open_tool_calls()
        .iter()
        .any(|id| id == &call.tool_call_id)
        && cx
            .state
            .open_step()
            .is_some_and(|(_, kind)| *kind == StepKind::Agent);
    if !call_is_open {
        tracing::warn!(tool = call.name, "refusing a routed call for a closed step");
        return CommandEffect::none();
    }
    match execute(cx.state, &call.name, &call.input, cx.actor.self_ref()) {
        Ok((value, events)) => {
            let _ = call.reply.send(Ok(value));
            CommandEffect::persist(events)
        }
        Err(e) => {
            let _ = call.reply.send(Err(e));
            CommandEffect::none()
        }
    }
}

/// The shape every component shares. `handle` decides — state in, effect out;
/// the actor persists and folds. A component with no instance state still
/// implements this so the actor treats every field the same way.
#[async_trait]
pub(crate) trait Component {
    /// The command group routed to this component.
    type Command;

    /// Decide what `cmd` means. Reads whatever it likes through `cx`, writes
    /// only by returning events (durable) or touching step_run (transient), and
    /// reaches other components only by telling commands.
    async fn handle(
        &mut self,
        cmd: Self::Command,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent>;

    /// Fold one of this component's events into state.
    ///
    /// Must be pure — no I/O, no clock, no step_run. An associated function
    /// rather than a method because replay happens before any component
    /// instance exists.
    fn apply(_state: &mut AgentState, _event: AgentDomainEvent) {}

    /// Repair whatever a dead process left this component holding, once
    /// recovery has finished and before the first live command is handled.
    /// Nothing here persists; anything that needs to journal arrives as an
    /// ordinary command.
    async fn on_load(&mut self, _cx: &mut Cx<'_>) {}

    /// The toolbox this component vends, if it has tools to offer.
    ///
    /// The actor collects these at provisioning time and composes them ahead
    /// of the runtime's — the one place the whole tool surface is assembled.
    /// Most components vend nothing.
    fn toolbox(
        &self,
        _actor: horsie_actor::ActorRef<AgentCommand>,
    ) -> Option<std::sync::Arc<dyn horsie_agentcore::Toolbox>> {
        None
    }
}

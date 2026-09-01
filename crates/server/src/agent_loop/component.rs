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
//! - **[`Scratch`]** — the transient half of the state: the few in-memory
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
//! no clock, no scratch. `on_load` repairs what a dead process left behind.

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
pub(crate) enum WorkKind {
    /// Rehydrating the runtime, reconnecting MCP, composing the toolbox.
    Provisioning,
    /// A turn's pre-start hooks.
    Hooks,
    /// One provider call for the turn.
    Step,
    /// A summarising call, folded back into this agent's own history.
    Compaction,
    /// A summarising call taken for sub sessions branching from this one.
    Summary,
}

/// The transient half of an agent's state: in-memory facts more than one
/// component reads. Everything here is rebuilt or re-decided on recovery,
/// which is exactly why none of it is journaled.
pub(crate) struct Scratch {
    /// Whether this agent's session has a runtime to run on. Seeded at spawn
    /// and moved by the `Runtime` lifecycle records the owner already sends.
    pub ready: bool,
    /// The generation every off-mailbox report is fenced against. Bumped by a
    /// cancel, so a dying task's report names a generation that no longer
    /// exists and is dropped — which is what makes "nothing more will happen"
    /// true the moment a cancel is handled.
    pub work: u64,
    /// The job running off the mailbox, if any. This is the busy gate.
    pub running: Option<WorkKind>,
    /// Cancels everything running for the current generation.
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
    /// What the next provider call should say about tool use. Taken when a
    /// turn starts, so it applies to exactly one turn. Set only when re-running
    /// a turn that ended without the result it owed.
    pub pending_tool_choice: Option<horsie_agentcore::ToolChoice>,
    /// Chunks of the message currently being written, since the newest log
    /// entry. Cleared whenever an entry lands, because the entry supersedes
    /// them. Written by the turn, read by the read path.
    pub deltas: Vec<String>,
}

impl Scratch {
    pub fn new(ready: bool) -> Self {
        Self {
            ready,
            work: 0,
            running: None,
            cancel: CancellationToken::new(),
            ctx: None,
            ctx_stale: true,
            pending_tool_choice: None,
            deltas: Vec::new(),
        }
    }

    /// Claim the off-mailbox slot for `kind`. Answers the generation the
    /// report must carry and the token the task must die on.
    pub fn begin(&mut self, kind: WorkKind) -> (u64, CancellationToken) {
        self.running = Some(kind);
        (self.work, self.cancel.clone())
    }

    /// Whether `work` still names the live generation — the fence every
    /// off-mailbox report passes through.
    pub fn live(&self, work: u64) -> bool {
        self.work == work
    }

    /// Release the slot a live report belongs to. `false` for a report from a
    /// cancelled generation, which changes nothing.
    pub fn finished(&mut self, work: u64) -> bool {
        if !self.live(work) {
            return false;
        }
        self.running = None;
        true
    }

    /// Stop everything: kill what is running, and move the generation past
    /// anything it might still say.
    pub fn stop(&mut self) {
        self.cancel.cancel();
        self.cancel = CancellationToken::new();
        self.work = self.work.wrapping_add(1);
        self.running = None;
        // Whatever the cancel interrupted may have been holding a runtime that
        // is going away; the next work builds its own.
        self.ctx_stale = true;
    }
}

/// Everything one turn's steps share, built once by the provision component
/// and published to the shared scratch for whoever needs it.
pub struct TurnCtx {
    pub provider: std::sync::Arc<dyn horsie_agentcore::LlmProvider>,
    /// The fully-composed, selection-filtered toolbox remote calls dispatch
    /// through. Component tools never reach it — the turn routes them to
    /// their components first.
    pub toolbox: std::sync::Arc<dyn horsie_agentcore::Toolbox>,
    /// What the model is shown, already filtered.
    pub specs: Vec<horsie_agentcore::ToolSpec>,
    /// The component-claimed tool names that survived the filter.
    pub inline_names: std::collections::HashSet<String>,
    pub system_prompt: String,
    pub budget: Option<horsie_agentcore::CompactionBudget>,
    pub conversation_id: String,
    pub thinking_effort: Option<horsie_agentcore::ThinkingEffort>,
    /// For the compaction hooks a compact run fires.
    pub context_provider: std::sync::Arc<dyn crate::agent_loop::ContextProvider>,
}

/// One tool call the turn routed to a component instead of the toolbox.
///
/// Carries the work generation so a component never acts for a turn that has
/// since been cancelled or superseded — the stale call is dropped, and the
/// cancel already repaired its dangling `tool_use`.
pub struct ComponentToolCall {
    pub work: u64,
    pub tool_call_id: String,
    pub name: String,
    pub input: serde_json::Value,
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
/// stands, the shared scratch, the agent's configuration, and the means to
/// spawn work and reach its own mailbox. Nothing here belongs to another
/// component.
pub(crate) struct Cx<'a> {
    pub state: &'a AgentState,
    pub scratch: &'a mut Scratch,
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

/// Execute a routed tool call and answer it — the shared shape of every
/// component-claimed tool.
///
/// "Answering" is journaling the result, exactly as a remote tool call is
/// answered: the log is where an unanswered call is visible, so closing one
/// there closes it for everybody. Nothing is told to the turn, and the turn
/// cannot tell the two kinds of call apart.
///
/// A call from a cancelled generation is dropped: the cancel already repaired
/// its dangling `tool_use`, and acting now would leak a side effect.
pub(crate) async fn answer_tool_call(
    call: ComponentToolCall,
    cx: &mut Cx<'_>,
    execute: ToolExecutor,
) -> CommandEffect<AgentDomainEvent> {
    if !cx.scratch.live(call.work) {
        tracing::warn!(
            tool = call.name,
            "dropping a routed tool call from a cancelled turn"
        );
        return CommandEffect::none();
    }
    let (output, is_error, mut events) =
        match execute(cx.state, &call.name, &call.input, cx.actor.self_ref()) {
            // A string result is forwarded verbatim; re-encoding it as JSON
            // would wrap it in quotes and escape every newline.
            Ok((value, events)) => (
                value
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| value.to_string()),
                false,
                events,
            ),
            Err(e) => (e.to_string(), true, Vec::new()),
        };
    events.push(AgentDomainEvent::ToolComplete {
        tool_call_id: call.tool_call_id,
        output,
        is_error,
        artifacts: Vec::new(),
        at_ms: horsie_models::now_ms(),
    });
    CommandEffect::persist(events)
}

/// The shape every component shares. `handle` decides — state in, effect out;
/// the actor persists and folds. A component with no instance state still
/// implements this so the actor treats every field the same way.
#[async_trait]
pub(crate) trait Component {
    /// The command group routed to this component.
    type Command;

    /// Decide what `cmd` means. Reads whatever it likes through `cx`, writes
    /// only by returning events (durable) or touching scratch (transient), and
    /// reaches other components only by telling commands.
    async fn handle(
        &mut self,
        cmd: Self::Command,
        cx: &mut Cx<'_>,
    ) -> CommandEffect<AgentDomainEvent>;

    /// Fold one of this component's events into state.
    ///
    /// Must be pure — no I/O, no clock, no scratch. An associated function
    /// rather than a method because replay happens before any component
    /// instance exists.
    fn apply(_state: &mut AgentState, _event: AgentDomainEvent) {}

    /// Repair whatever a dead process left this component holding, once
    /// recovery has finished and before the first live command is handled.
    /// Nothing here persists; anything that needs to journal arrives as an
    /// ordinary command.
    async fn on_load(&mut self, _cx: &mut Cx<'_>) {}
}

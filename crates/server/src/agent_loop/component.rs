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

/// Non-tool foreground work. Tool execution has its own variant because only
/// that phase may carry dispatched and stopped calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StepPhase {
    Initialize,
    Connect,
    StartHooks,
    CallingProvider,
    StopHook,
    Compaction,
    SeedSummary,
}

/// One tool call dispatched by this process and not yet answered.
#[derive(Clone)]
pub(crate) struct DispatchedCall {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// The one foreground activity this process is running. `Idle` has no stale
/// marker or cancellation handle; only `Tools` can hold call bookkeeping.
enum ForegroundStep {
    Idle,
    Running {
        marker_seq: u64,
        phase: StepPhase,
        cancel: CancellationToken,
    },
    Tools {
        marker_seq: u64,
        cancel: CancellationToken,
        calls: Vec<DispatchedCall>,
        stopped: Vec<horsie_agentcore::StoppedCall>,
    },
}

/// The transient top of the durable history. It owns process-only execution
/// state; recovery rebuilds meaning from history and always starts here idle.
pub(crate) struct StepRun {
    pub ready: bool,
    foreground: ForegroundStep,
    pub ctx: Option<std::sync::Arc<TurnCtx>>,
    pub ctx_stale: bool,
    pub pending_tool_choice: Option<horsie_agentcore::ToolChoice>,
    /// Streamed text is kept outside `ForegroundStep` so cancellation can
    /// salvage it after moving the foreground back to `Idle`.
    pub deltas: Vec<String>,
}

impl StepRun {
    pub fn new(ready: bool) -> Self {
        Self {
            ready,
            foreground: ForegroundStep::Idle,
            ctx: None,
            ctx_stale: true,
            pending_tool_choice: None,
            deltas: Vec::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        !matches!(self.foreground, ForegroundStep::Idle)
    }

    /// Claim the foreground slot for non-tool work.
    pub fn begin(&mut self, phase: StepPhase, marker_seq: u64) -> CancellationToken {
        let cancel = CancellationToken::new();
        self.foreground = ForegroundStep::Running {
            marker_seq,
            phase,
            cancel: cancel.clone(),
        };
        cancel
    }

    /// Claim the foreground slot for a whole tool batch.
    pub fn begin_tools(
        &mut self,
        marker_seq: u64,
        calls: Vec<DispatchedCall>,
    ) -> CancellationToken {
        let cancel = CancellationToken::new();
        self.foreground = ForegroundStep::Tools {
            marker_seq,
            cancel: cancel.clone(),
            calls,
            stopped: Vec::new(),
        };
        cancel
    }

    pub fn push_delta(&mut self, marker_seq: u64, text: String) -> bool {
        let ForegroundStep::Running {
            marker_seq: open,
            phase: StepPhase::CallingProvider,
            ..
        } = &self.foreground
        else {
            return false;
        };
        if *open != marker_seq {
            return false;
        }
        self.deltas.push(text);
        true
    }

    pub fn live(&self, marker_seq: u64) -> bool {
        match &self.foreground {
            ForegroundStep::Idle => false,
            ForegroundStep::Running {
                marker_seq: open, ..
            }
            | ForegroundStep::Tools {
                marker_seq: open, ..
            } => *open == marker_seq,
        }
    }

    pub fn finished(&mut self, marker_seq: u64) -> bool {
        if !self.live(marker_seq) {
            return false;
        }
        self.foreground = ForegroundStep::Idle;
        true
    }

    pub fn take_tool(&mut self, marker_seq: u64, tool_call_id: &str) -> Option<DispatchedCall> {
        let ForegroundStep::Tools {
            marker_seq: open,
            calls,
            ..
        } = &mut self.foreground
        else {
            return None;
        };
        if *open != marker_seq {
            return None;
        }
        calls
            .iter()
            .position(|call| call.id == tool_call_id)
            .map(|position| calls.remove(position))
    }

    pub fn push_stopped(&mut self, stopped_call: horsie_agentcore::StoppedCall) {
        if let ForegroundStep::Tools { stopped, .. } = &mut self.foreground {
            stopped.push(stopped_call);
        }
    }

    /// Finish an empty tool batch and return every stopper it collected.
    /// `None` means other calls are still running or this is not a tool step.
    pub fn settle_tools(&mut self, marker_seq: u64) -> Option<Vec<horsie_agentcore::StoppedCall>> {
        let ForegroundStep::Tools {
            marker_seq: open,
            calls,
            ..
        } = &self.foreground
        else {
            return None;
        };
        if *open != marker_seq || !calls.is_empty() {
            return None;
        }
        let ForegroundStep::Tools { stopped, .. } =
            std::mem::replace(&mut self.foreground, ForegroundStep::Idle)
        else {
            return None;
        };
        Some(stopped)
    }

    /// Stop everything and make callbacks for the old marker stale.
    pub fn stop(&mut self) {
        match &self.foreground {
            ForegroundStep::Idle => {}
            ForegroundStep::Running { cancel, .. } | ForegroundStep::Tools { cancel, .. } => {
                cancel.cancel();
            }
        }
        self.foreground = ForegroundStep::Idle;
        self.ctx_stale = true;
    }
}

/// Everything one turn's steps share, built once by the provision component
/// and published to the shared step_run for whoever needs it.
pub struct TurnCtx {
    pub provider: std::sync::Arc<dyn horsie_agentcore::LlmProvider>,
    /// Stable initialization discovery. Persisted with `AgentInitialized` and
    /// reused only to rebuild live clients after reload.
    pub manifest: ContextManifest,
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

/// What each component state contributes when history is branched. The common
/// case contributes nothing; timers and task-list state opt in explicitly.
pub(crate) trait PartState: Sized {
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

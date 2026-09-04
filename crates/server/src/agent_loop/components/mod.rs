//! Stateful tools owned by the agent.
//!
//! Timers and task lists are components because each has its own commands,
//! events, durable state, recovery behavior, and toolbox. Run-loop phases do
//! not live here.

pub mod task_list;
pub mod timers;

use crate::agent_loop::prelude::*;
use async_trait::async_trait;
use horsie_actor::CommandEffect;
use serde::{Deserialize, Serialize};

pub(crate) use task_list::{TaskListPart, TaskLists};
pub(crate) use timers::{TimerState, Timers};

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
pub(crate) trait CarriedComponentState: Sized {
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
pub(crate) trait ComponentSlot: Sized {
    fn get(parts: &[ComponentState]) -> Option<&Self>;
    fn get_mut(parts: &mut Vec<ComponentState>) -> Option<&mut Self>;
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
    cx: &mut CommandContext<'_>,
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
            .is_some_and(|(_, kind)| *kind == StepKind::Provider);
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
        cx: &mut CommandContext<'_>,
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
    async fn on_load(&mut self, _cx: &mut CommandContext<'_>) {}

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
/// Only genuine components appear here. Run-loop phases record their state in
/// history instead of adding another durable part.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ComponentState {
    Timers(TimerState),
    TaskList(TaskListPart),
}

/// The `ComponentSlot` implementations and the two polls, generated from one list so a
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

        $(impl ComponentSlot for $ty {
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

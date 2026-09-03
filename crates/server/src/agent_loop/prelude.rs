//! What every component is handed.
//!
//! One `use` line at the top of each component, rather than a dozen: the
//! vocabulary ([`AgentCommand`], [`AgentDomainEvent`]), the state
//! ([`AgentState`] and the means to reach one's own part of it), the contract
//! ([`Component`], [`Cx`], [`StepRun`]), and the registry's own routing.
//!
//! Deliberately small. The component *states* are here because they are shared
//! vocabulary and opaque — a fold reaches another part through a method it
//! chose to offer, never a field. The component *structs* are not: a component
//! that needs to name another has reached for something that is not its
//! business.

#[cfg(test)]
pub(crate) use crate::agent_loop::AgentActor;
pub(crate) use crate::agent_loop::commands::{
    AbandonedStart, AgentCommand, CompactJob, CompactLanding, CompactOutcome, CompactedData,
    CompactionCommand, CoreCommand, LogCommand, PreparedStart, ProvidedOutcome, ProvisionCommand,
    QueueCommand, ReadCommand, RunCommand, SeedCommand, TaskListCommand, TimerCommand, ToolReturn,
};
pub(crate) use crate::agent_loop::component::{
    Component, Cx, Part, PartState, StepPhase, StepRun, TurnCtx, answer_tool_call,
};
pub(crate) use crate::agent_loop::components::{
    ComponentState, Components, TaskListPart, TimerState,
};
pub(crate) use crate::agent_loop::context::ContextManifest;
pub(crate) use crate::agent_loop::events::{
    AgentDomainEvent, RunEnd, StepFailure, StepKind, StopHookOutcome, SystemPromptSource,
};
pub(crate) use crate::agent_loop::params::AgentParams;
pub(crate) use crate::agent_loop::shared::repair::{
    missing_tool_results, parked_call_ids, repair_unanswered_tool_calls,
};
pub(crate) use crate::agent_loop::state::{AgentState, new_message_id};
#[cfg(test)]
pub(crate) use horsie_actor::EventSourcedActor;

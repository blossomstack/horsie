//! What every component is handed.
//!
//! One `use` line at the top of each component, rather than a dozen: the
//! vocabulary ([`AgentCommand`], [`AgentDomainEvent`]), the state
//! ([`AgentState`] and the means to reach one's own part of it), the contract
//! ([`Component`], [`Cx`], [`Scratch`]), and the registry's own routing.
//!
//! Deliberately small. The component *states* are here because they are shared
//! vocabulary and opaque — a fold reaches another part through a method it
//! chose to offer, never a field. The component *structs* are not: a component
//! that needs to name another has reached for something that is not its
//! business.

pub(crate) use crate::agent_loop::boundary::Blocked;
pub(crate) use crate::agent_loop::commands::{
    AgentCommand, CompactJob, CompactLanding, CompactOutcome, CompactedData, CompactionCommand,
    CoreCommand, LogCommand, PreparedStart, ProvidedOutcome, ProvisionCommand,
    QueueCommand, ReadCommand, RunCommand, SeedCommand, TaskListCommand, TimerCommand, ToolReturn,
    AbandonedStart,
};
pub(crate) use crate::agent_loop::component::{
    Component, ComponentToolCall, Cx, Part, PartState, Scratch, TurnCtx, WorkKind,
    answer_tool_call,
};
pub(crate) use crate::agent_loop::components::{
    ComponentState, Components, QueueState, TaskListPart, TimerState, TurnState, UsageState,
    component_tool_specs, is_component_tool, route_tool_call,
};
pub(crate) use crate::agent_loop::events::AgentDomainEvent;
pub(crate) use crate::agent_loop::params::AgentParams;
pub(crate) use crate::agent_loop::state::{AgentState, UsageTotal, new_message_id};
pub(crate) use crate::agent_loop::shared::repair::{
    missing_tool_results, parked_call_ids, repair_unanswered_tool_calls,
};

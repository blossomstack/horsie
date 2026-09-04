//! Shared internal vocabulary for run-loop handlers and stateful tools.
//!
//! This keeps command handlers focused on their decisions without hiding which
//! state is durable (`AgentState`) and which is live (`StepRun`).

#[cfg(test)]
pub(crate) use crate::agent_loop::AgentActor;
pub(crate) use crate::agent_loop::command_context::CommandContext;
pub(crate) use crate::agent_loop::commands::{
    AgentCommand, CompactJob, CompactLanding, CompactOutcome, CompactedData, CompactionCommand,
    ContextCommand, ContextReady, CoreCommand, HistoryCommand, IncomingCommand, PreparedInput,
    ProviderCommand, QueryCommand, RejectedInput, SeedCommand, TaskListCommand, TimerCommand,
    ToolReturn,
};
pub(crate) use crate::agent_loop::components::{
    CarriedComponentState, Component, ComponentSlot, ComponentState, TaskListPart, TimerState,
    answer_tool_call,
};
pub(crate) use crate::agent_loop::context::ContextManifest;
pub(crate) use crate::agent_loop::events::{
    AgentDomainEvent, RunEnd, StopHookOutcome, SystemPromptSource,
};
pub(crate) use crate::agent_loop::params::AgentParams;
pub(crate) use crate::agent_loop::run_loop::RunLoop;
pub(crate) use crate::agent_loop::shared::repair::{
    missing_tool_results, parked_call_ids, repair_unanswered_tool_calls,
};
pub(crate) use crate::agent_loop::state::{AgentState, new_message_id};
pub(crate) use crate::agent_loop::step::{ExecutionContext, StepFailure, StepKind, StepRun};
pub(crate) use crate::agent_loop::transcript::{Transcript, carry_transcript, project_transcript};
#[cfg(test)]
pub(crate) use horsie_actor::EventSourcedActor;

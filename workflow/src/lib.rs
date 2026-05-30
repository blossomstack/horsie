//! Multi-agent orchestration on top of the event-sourced `actor` runtime.
//!
//! A [`WorkflowActor`] drives a [`WorkflowDefinition`](models::workflow::WorkflowDefinition):
//! it spawns one [`AgentActor`] per agent session, routes handoff tools to the
//! next agent via the workflow's transitions, and owns the error and
//! interruption model — cancel, resume, ask/reply, fork, and crash recovery.
//! Both actors are event-sourced, so a restarted process recovers in-flight
//! workflows and conversations from the journal.

mod agent_actor;
mod context;
mod workflow_actor;

pub use agent_actor::{AgentActor, AgentCommand, AgentDomainEvent, AgentParams, AgentState};
pub use context::{
    AgentRuntimeContext, CONCLUDE_TOOL, DefaultToolboxFactory, ToolboxFactory,
    WorkflowRuntimeContext, conclude_tool_spec,
};
pub use workflow_actor::{
    WorkflowActor, WorkflowCommand, WorkflowDomainEvent, WorkflowState, WorkflowStatus,
};

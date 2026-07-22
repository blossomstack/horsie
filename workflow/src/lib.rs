//! Multi-agent orchestration on top of the event-sourced `actor` runtime.
//!
//! A [`WorkflowActor`] drives a [`WorkflowDefinition`](horsie_models::workflow::WorkflowDefinition):
//! it spawns one [`AgentActor`] per agent session, routes handoff tools to the
//! next agent via the workflow's transitions, and owns the error and
//! interruption model — cancel, resume, ask/reply, fork, and crash recovery.
//! Both actors are event-sourced, so a restarted process recovers in-flight
//! workflows and conversations from the journal.

mod agent_actor;
mod context;
mod mcp_toolbox;
mod task_list;
mod timers;
mod workflow_actor;
mod workspace;

pub use agent_actor::{
    AgentActor, AgentCommand, AgentDomainEvent, AgentParams, AgentState, UsageTotal,
};
pub use context::{
    AgentOutcome, AgentOutcomeSink, AgentRunDef, AgentRuntimeContext, CONCLUDE_TOOL,
    DefaultToolboxFactory, FixedRunResources, INSPECT_WORKSPACE_TOOL, PreparedRun, RunResources,
    SKILL_TOOL, ToolboxFactory, WorkflowRuntimeContext, conclude_tool_spec,
};
pub use mcp_toolbox::{CompositeToolbox, McpToolbox};
pub use task_list::{
    TASK_LIST_TOOL, TaskListAction, TaskListState, TaskRecord, TaskStatus, task_list_tool_spec,
};
pub use timers::{
    CancelSelector, TimerId, TimerKind, TimerRecord, TimerView, now_unix_ms, timer_tool_specs,
};
pub use workflow_actor::{
    WorkflowActor, WorkflowCommand, WorkflowDomainEvent, WorkflowNotification, WorkflowState,
    WorkflowStatus,
};
pub use workspace::{
    SharedContext, Skill, SkillSet, WorkspaceContext, compose_system_prompt, scan as scan_workspace,
};

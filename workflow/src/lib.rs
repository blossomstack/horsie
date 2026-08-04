//! The agent loop on top of the event-sourced `actor` runtime.
//!
//! An [`AgentActor`] runs one agent: it calls the provider, executes tools
//! through a [`Toolbox`](horsie_agentcore::Toolbox), and reports a terminal
//! [`AgentOutcome`] to whoever spawned it. It is event-sourced, so a restarted
//! process recovers an in-flight conversation from the journal.
//!
//! Sequencing several agents — an interactive session's main agent and its
//! subagents, or a workflow run's steps — belongs to the owner that spawns
//! them, not here.

mod agent_actor;
mod context;
mod mcp_toolbox;
mod task_list;
mod timers;
mod workspace;

pub use agent_actor::{
    AgentActor, AgentCommand, AgentDomainEvent, AgentHistoryPage, AgentObserver, AgentParams,
    AgentState, AgentStateView, AgentUsageSnapshot, HistoryQuery, UsageTotal, hook_entry,
    hook_entry_id,
};
pub use context::{
    AgentOutcome, AgentOutcomeSink, AgentRunDef, AgentRuntimeContext, CONCLUDE_TOOL, ContextError,
    ContextProvider, Contexts, DefaultToolboxFactory, FixedContextProvider, INSPECT_WORKSPACE_TOOL,
    SKILL_TOOL, ToolboxFactory, conclude_tool_spec,
};
pub use mcp_toolbox::{CompositeToolbox, McpToolbox};
pub use task_list::{
    TASK_LIST_TOOL, TaskListAction, TaskListState, TaskRecord, TaskStatus, task_list_tool_spec,
};
pub use timers::{CancelSelector, TimerId, TimerKind, TimerRecord, TimerView, timer_tool_specs};
pub use workspace::{
    SharedContext, SharedScan, Skill, SkillSet, WorkspaceContext, compose_system_prompt,
    scan as scan_workspace,
};

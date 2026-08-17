//! The agent loop, on top of the event-sourced `actor` runtime.
//!
//! An [`AgentActor`] runs one agent: it calls the provider, executes tools
//! through a [`Toolbox`](horsie_agentcore::Toolbox), and reports a terminal
//! [`AgentOutcome`] to whoever spawned it. It is event-sourced, so a restarted
//! process recovers an in-flight conversation from the journal.
//!
//! Sequencing several agents — an interactive session's main agent and its
//! subagents, or a workflow run's steps — belongs to the owner that spawns
//! them, not here. That owner is [`crate::sessions`]; the workflow *graph*
//! feature that schedules runs is [`crate::workflows`], which is a different
//! thing despite the adjacent name.

mod agent_actor;
mod agent_log;
pub mod capabilities;
pub mod carried_state;
mod context;
mod hook_translation;
mod inbox;
mod mcp_toolbox;
mod task_list;
mod timers;
mod workspace;

pub use agent_actor::{
    AgentActor, AgentCommand, AgentDomainEvent, AgentObserver, AgentParams, AgentState,
    AgentStateView, AgentUsageSnapshot, ReadOutcome, ReplayWindow, UsageTotal, hook_entry,
    hook_entry_id,
};
pub use agent_log::{Cursor, LogPage, REPLAY_CAP, page_after, page_before, replay_window};
pub use context::compaction_window;
pub use context::{
    AgentOutcome, AgentOutcomeSink, AgentRunDef, AgentRuntimeContext, AskedQuestion, ContextError,
    ContextProvider, Contexts, DefaultToolboxFactory, FixedContextProvider, INSPECT_WORKSPACE_TOOL,
    SKILL_TOOL, StartTurn, ToolboxFactory, TurnPreparation,
};
pub use hook_translation::{start_blocked, translate};
pub use inbox::{
    ABANDONED_ASK_RESULT, AnswerError, AskAnswer, Incoming, MERGE_SEPARATOR, Summarise, Turn,
    answered_turn, queued_turn, resumed_turn,
};
pub use mcp_toolbox::{
    CompositeToolbox, McpToolbox, McpToolboxes, McpUnavailable, PluginMcpToolbox,
};
pub use task_list::{
    TASK_LIST_TOOL, TaskListAction, TaskListState, TaskRecord, TaskStatus, task_list_tool_spec,
};
pub use timers::{CancelSelector, TimerId, TimerKind, TimerRecord, TimerView, timer_tool_specs};
pub use workspace::{
    AgentCatalog, CatalogAgent, SharedContext, SharedScan, Skill, SkillSet, WorkspaceContext,
    compose_system_prompt, scan as scan_workspace,
};

//! One agent: what it is, what it can be told, and what it decides.
//!
//! An [`AgentActor`] runs one agent. It is event-sourced, so a restarted
//! process recovers an in-flight session from the journal: everything durable
//! about it is [`AgentState`], everything it can be told is [`AgentCommand`],
//! and every change is an [`AgentDomainEvent`] journaled before it is believed.
//!
//! Read this module top down. The files beside this one are the architecture:
//!
//! - [`actor`] — the shell. It routes commands, persists what a component
//!   decided, and keeps the plumbing (the observer, the revision counter, the
//!   snapshot cadence). It decides nothing.
//! - [`boundary`] — what happens next, in one ordered decision, re-taken after
//!   every durable write. The only code that knows what components exist.
//! - [`component`] — the contract: what a component is, what it is handed, and
//!   everything components share.
//! - [`state`] — the transcript, plus one opaque part per component.
//! - [`commands`] and [`events`] — the vocabulary.
//! - [`params`] — how this incarnation was configured.
//! - [`context`] — the contract with whoever spawned this agent: what it is
//!   given (a provider, a toolbox, a workspace) and what it reports back.
//!
//! Below that, [`components`] holds the implementations — one module each,
//! pluggable, and secondary to the architecture above. [`shared`] holds what
//! more than one of them needs.
//!
//! Sequencing several agents — an interactive session's main agent and its
//! subagents, or a workflow run's steps — belongs to the owner that spawns
//! them, not here. That owner is [`crate::sessions`]; the workflow *graph*
//! feature that schedules runs is [`crate::workflows`], which is a different
//! thing despite the adjacent name.

mod actor;
mod boundary;
mod commands;
mod component;
pub mod components;
mod context;
mod events;
mod params;
mod prelude;
mod read_image_toolbox;
pub mod shared;
mod state;
#[cfg(test)]
pub(crate) mod testing;

pub use actor::{AgentActor, AgentObserver};
pub use commands::{
    AgentCommand, CompactJob, CompactOutcome, CompactedData, CompactionCommand, CoreCommand,
    LogCommand, ProvisionCommand, QueueCommand, ReadCommand, RunCommand, SeedCommand,
    TaskListCommand, TimerCommand, ToolReturn,
};
pub use component::TurnCtx;
pub use components::queue::inbox::{
    ABANDONED_ASK_RESULT, AnswerError, AskAnswer, Incoming, MERGE_SEPARATOR, Offer, Turn,
    answered_turn, queued_offer,
};
pub use components::reads::{ReadOutcome, ReplayWindow};
pub use components::task_list::domain::{
    TASK_LIST_TOOL, TaskListAction, TaskListState, TaskRecord, TaskStatus, task_list_tool_spec,
    wire_task,
};
pub use components::timers::domain::{
    CancelSelector, TimerId, TimerKind, TimerRecord, TimerView, timer_tool_specs,
};
pub use context::compaction_window;
pub use context::{
    AgentOutcome, AgentOutcomeSink, AgentRunDef, AgentRuntimeContext, AskedQuestion, ContextError,
    ContextManifest, ContextProvider, Contexts, DefaultToolboxFactory, FilteredToolbox,
    FixedContextProvider, FrozenPluginAgent, INSPECT_WORKSPACE_TOOL, SKILL_TOOL, StartTurn,
    StopHookRequest, StopHookResult, ToolboxFactory, TurnPreparation,
};
pub use events::{
    AgentDomainEvent, AgentHistoryEntry, RunEnd, StepFailure, StepKind, StopHookOutcome,
    SystemPromptSource,
};
pub use params::AgentParams;
pub use read_image_toolbox::{READ_IMAGE_TOOL, ReadImageToolbox};
pub use shared::agent_log::{
    Anchor, Cursor, LogFilter, LogPage, REPLAY_CAP, kind_of, page, replay_window, search,
    seq_of_id, since,
};
pub use shared::hook_translation::{start_blocked, translate};
pub use shared::mcp_toolbox::{
    ArtifactSink, CompositeToolbox, McpToolbox, McpToolboxes, McpUnavailable, PluginMcpToolbox,
};
pub use shared::workspace::{
    AgentCatalog, CatalogAgent, SharedContext, SharedScan, Skill, SkillSet, WorkspaceContext,
    compose_system_prompt, scan as scan_workspace,
};
pub use state::{
    AgentState, AgentStateView, AgentUsageSnapshot, UsageTotal, hook_entry, hook_entry_id,
};

//! One agent: an append-only history, one transient foreground step, and one
//! decision about what happens next.
//!
//! [`AgentActor`] owns a process-local `StepRun`; [`AgentState`] owns durable
//! history. A restart repairs the newest open marker without replaying provider,
//! tool, hook, compaction, seed-summary, or workspace-scan side effects.
//!
//! Read this module top down:
//!
//! - [`actor`] — persistence, recovery, observation, and the transient top step.
//! - [`boundary`] — the one ordered decision that drives normal and special steps.
//! - [`component`] — transient step state and the contract used only by genuine
//!   stateful tool components.
//! - [`state`] — chronological history plus timer and task-list component state.
//! - [`commands`] and [`events`] — the exhaustive vocabulary.
//! - [`context`] — one-time initialization and repeatable connection contracts.
//! - [`components`] — the actor-owned driver, its small handlers, and the timer
//!   and task-list components.
//! - [`shared`] — pure helpers used by more than one handler.
//!
//! Sequencing several agents remains the responsibility of the session or
//! workflow that spawned them.

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

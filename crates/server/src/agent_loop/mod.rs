//! One agent: an append-only history, one transient foreground step, and one
//! decision about what happens next.
//!
//! [`AgentActor`] owns process-local [`StepRun`]; [`AgentState`] owns durable
//! history. Recovery repairs the newest open marker without replaying provider,
//! tool, hook, compaction, seed-summary, or workspace-scan side effects.
//!
//! Read this module top down:
//!
//! - [`actor`] — persistence, recovery, and observation.
//! - [`run_loop`] — command routing and the one ordered next-step decision.
//! - [`step_run`] — all process-local foreground execution.
//! - [`state`] — chronological durable history and compact projections.
//! - [`transcript`] — the pure user-facing projection of history.
//! - [`commands`] and [`events`] — the exhaustive vocabulary.
//! - [`context`] — initialization and reconnection contracts.
//! - [`components`] — only timers and task lists, the two stateful tools.
//! - [`shared`] — pure helpers used in more than one place.
//!
//! Sequencing several agents remains the responsibility of the session or
//! workflow that spawned them.

mod actor;
mod command_context;
mod commands;
pub mod components;
mod context;
mod events;
mod params;
mod prelude;
mod read_image_toolbox;
mod run_loop;
pub mod shared;
mod state;
mod step_run;
#[cfg(test)]
pub(crate) mod testing;
mod transcript;
pub use actor::{AgentActor, AgentObserver};
pub use commands::{
    AgentCommand, CompactJob, CompactOutcome, CompactedData, CompactionCommand, ContextCommand,
    CoreCommand, HistoryCommand, IncomingCommand, ProviderCommand, QueryCommand, SeedCommand,
    TaskListCommand, TimerCommand, ToolReturn,
};
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
pub use run_loop::{AnswerError, AskAnswer, Incoming, ReadOutcome, ReplayWindow};
pub(crate) use run_loop::{PendingInput, TurnInput, next_input, validate_answers};
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
pub use state::{AgentState, AgentStateView, AgentUsageSnapshot, UsageTotal};
pub use step_run::ExecutionContext;
pub use transcript::{Transcript, hook_entry, hook_entry_id, project_transcript};

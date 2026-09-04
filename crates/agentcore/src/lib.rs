pub mod compaction;
mod error;
mod events;
mod provider;
mod secret;
mod step;
#[cfg(any(test, feature = "test-util"))]
pub mod testkit;
mod thinking;
mod tool;

pub use compaction::{
    CompactionBudget, approx_history_tokens, boundary_text, choose_cut, summary_prompt,
};
pub use error::{LlmError, ToolCallError};
pub use events::{EventSink, EventSinkError};
pub use provider::{
    ArtifactBytes, ArtifactSource, CompletionRequest, CompletionResponse, LlmProvider, StopReason,
    ToolChoice,
};
pub use secret::Secret;
pub use step::{
    StepError, StepRequest, StepResponse, artifact_ids, extract_text, extract_tool_calls, run_step,
    tool_fingerprint,
};
pub use thinking::{ThinkingDialect, ThinkingEffort};
pub use tool::{EmptyToolbox, Tool, ToolOutcome, ToolSpec, ToolValue, Toolbox, ToolboxImpl};

pub use horsie_models::agent::{
    AgentInput, AgentLogBody, AgentLogEntry, AgentOutput, AgentResult, AskLifecycle,
    CompactionEntry, CompactionTrigger, CompletedOutput, ContentPart, EmptyOutcome, FailedOutcome,
    HistoryEntry, HookEntry, LifecycleEvent, Message, PreparingLifecycle, QueuedLifecycle, Role,
    RuntimeLifecycle, RuntimeStatus, SessionFailedLifecycle, StepLifecycle, StoppedCall,
    StoppedOutput, SubAgentLifecycle, SubSessionLifecycle, TaskItem, TaskListLifecycle, TaskStatus,
    TextPart, ThinkingPart, ToolCallPart, ToolResultInput, ToolResultPart, TurnBeganLifecycle,
    TurnEndedLifecycle, TurnOutcome, Usage, UserMessageInput,
};
pub use horsie_models::events::{
    AgentEvent, ContentBlockStopEvent, InputMessageEvent, MessageCompleteEvent, MessageStartEvent,
    MessageStopEvent, RunCompleteEvent, TextBlockStartEvent, TextChunkEvent,
    ThinkingBlockStartEvent, ThinkingChunkEvent, ThinkingSignatureChunkEvent,
    ToolCallInputDeltaEvent, ToolCallStartEvent, ToolCompleteEvent, ToolExecutingEvent,
};

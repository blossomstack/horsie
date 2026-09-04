mod agent;
pub mod compaction;
mod error;
mod events;
mod provider;
mod secret;
#[cfg(any(test, feature = "test-util"))]
pub mod testkit;
mod thinking;
mod tool;

pub use agent::{Agent, AgentBuilder, AgentConfig};
pub use compaction::{
    CompactionBudget, CompactionPlan, CompactionPolicy, CompactionResult, PreCompactDecision,
};
pub use error::{
    AgentBuildError, AgentError, CommandDiagnostic, CommandFailure, LlmError, ToolCallError,
};
pub use events::{EventSink, EventSinkError};
pub use provider::{
    ArtifactBytes, ArtifactSource, CompletionRequest, CompletionResponse, LlmProvider, StopReason,
    ToolChoice,
};
pub use secret::Secret;
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

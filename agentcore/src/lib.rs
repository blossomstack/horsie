mod agent;
mod error;
mod events;
mod provider;
mod tool;

pub use agent::{Agent, AgentBuilder, AgentConfig};
pub use error::{AgentBuildError, AgentError, LlmError, ToolCallError};
pub use events::{EventSink, EventSinkError};
pub use provider::{CompletionRequest, CompletionResponse, LlmProvider, StopReason, ToolChoice};
pub use tool::{EmptyToolbox, Tool, ToolSpec, Toolbox, ToolboxImpl};

pub use models::agent::{
    AgentInput, AgentOutput, AgentResult, CompletedOutput, ContentPart, HandoffOutput, Message,
    Role, TextPart, ThinkingPart, ToolCallPart, ToolResultInput, ToolResultPart, Usage,
    UserMessageInput,
};
pub use models::events::{
    AgentEvent, ContentBlockStopEvent, InputMessageEvent, MessageCompleteEvent, MessageStartEvent,
    MessageStopEvent, RunCompleteEvent, TextBlockStartEvent, TextChunkEvent,
    ThinkingBlockStartEvent, ThinkingChunkEvent, ThinkingSignatureChunkEvent,
    ToolCallInputDeltaEvent, ToolCallStartEvent, ToolCompleteEvent, ToolExecutingEvent,
};

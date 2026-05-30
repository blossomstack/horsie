use agentcore::{AgentEvent, EventSink};
use std::io::Write;

/// Prints live `AgentEvent`s to stdout/stderr as the agent runs. Text streams to
/// stdout; tool/run lifecycle notes go to stderr so the final structured output on
/// stdout stays clean.
pub struct TerminalSink;

impl EventSink for TerminalSink {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::TextChunk(e) => {
                print!("{}", e.text);
                let _ = std::io::stdout().flush();
            }
            AgentEvent::ToolCallStart(e) => {
                eprintln!("\n· tool {} [{}]", e.name, e.tool_call_id);
            }
            AgentEvent::ToolComplete(e) => {
                eprintln!(
                    "· tool {} → {}",
                    e.tool_call_id,
                    if e.is_error { "error" } else { "ok" }
                );
            }
            AgentEvent::RunComplete(e) => {
                eprintln!(
                    "\n· run complete ({} iterations, {}/{} tokens)",
                    e.iterations, e.usage.input_tokens, e.usage.output_tokens
                );
            }
            // Streaming/structural events not surfaced on the terminal.
            AgentEvent::InputMessage(_)
            | AgentEvent::MessageStart(_)
            | AgentEvent::MessageStop(_)
            | AgentEvent::MessageComplete(_)
            | AgentEvent::ThinkingChunk(_)
            | AgentEvent::ToolCallInputDelta(_)
            | AgentEvent::ToolCallInputDone(_)
            | AgentEvent::ToolExecuting(_) => {}
        }
    }
}

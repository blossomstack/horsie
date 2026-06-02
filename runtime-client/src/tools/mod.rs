mod bash;
mod find_and_replace;
mod glob;
mod grep;
mod list_files;
mod read_file;
mod replace_lines;
mod write_file;

pub use bash::BashTool;
pub use find_and_replace::FindAndReplaceTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_files::ListFilesTool;
pub use read_file::ReadFileTool;
pub use replace_lines::ReplaceLinesTool;
pub use write_file::WriteFileTool;

use crate::client::RuntimeClient;
use agentcore::{ToolCallError, ToolboxImpl};
use models::runtime::ToolOutput;
use serde_json::Value;

/// Render a successful [`ToolOutput`] into the text the model sees.
///
/// The runtime returns `{stdout, stderr, exit_code}`, but historically only
/// `stdout` was forwarded — so a command that wrote its diagnostics to stderr or
/// exited non-zero looked like a clean success to the agent. This surfaces both:
/// stderr is appended to the visible output, and a non-zero exit code is reported
/// as a tool error so the agent loop marks the result `is_error` and the model
/// reliably notices the failure. File tools always exit 0 with empty stderr, so
/// for them this is a transparent passthrough of `stdout`.
pub(crate) fn render_output(o: ToolOutput) -> Result<Value, ToolCallError> {
    let mut text = o.stdout;
    if !o.stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&o.stderr);
    }
    if o.exit_code != 0 {
        return Err(ToolCallError::ExecutionFailed(format!(
            "command exited with status {}\n{text}",
            o.exit_code
        )));
    }
    Ok(Value::String(text))
}

/// Inject the standard optional `workspace` property into a tool's input schema. The
/// runtime resolves the name → root (defaulting to the sole workspace when omitted).
pub(crate) fn with_workspace(mut schema: Value) -> Value {
    if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
        props.insert(
            "workspace".to_string(),
            serde_json::json!({
                "type": "string",
                "description": "Which workspace to act in (see '# Workspaces'). Required when there is more than one workspace."
            }),
        );
    }
    schema
}

/// Extract the optional `workspace` argument from a tool-call input object.
pub(crate) fn workspace_arg(input: &Value) -> Option<String> {
    input
        .get("workspace")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Add all runtime-backed tools to an existing ToolboxImpl.
pub fn add_runtime_tools(toolbox: ToolboxImpl, client: RuntimeClient) -> ToolboxImpl {
    toolbox
        .add(BashTool::new(client.clone()))
        .add(ReadFileTool::new(client.clone()))
        .add(WriteFileTool::new(client.clone()))
        .add(FindAndReplaceTool::new(client.clone()))
        .add(ReplaceLinesTool::new(client.clone()))
        .add(ListFilesTool::new(client.clone()))
        .add(GlobTool::new(client.clone()))
        .add(GrepTool::new(client))
}

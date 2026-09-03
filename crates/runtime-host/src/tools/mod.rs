mod apply_patch;
mod bash;
mod find_and_replace;
mod glob;
mod grep;
mod list_files;
mod read_file;
mod replace_lines;
mod set_env;
mod set_working_dir;
mod write_file;

pub use apply_patch::ApplyPatchTool;
pub use bash::BashTool;
pub use find_and_replace::FindAndReplaceTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_files::ListFilesTool;
pub use read_file::ReadFileTool;
pub use replace_lines::ReplaceLinesTool;
pub use set_env::SetEnvTool;
pub use set_working_dir::SetWorkingDirTool;
pub use write_file::WriteFileTool;

use crate::client::RuntimeClient;
use horsie_agentcore::{ToolCallError, ToolboxImpl};
use horsie_models::runtime::ToolOutput;
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

/// Add all runtime-backed tools to an existing ToolboxImpl.
pub fn add_runtime_tools(toolbox: ToolboxImpl, client: RuntimeClient) -> ToolboxImpl {
    toolbox
        .add(BashTool::new(client.clone()))
        .add(ReadFileTool::new(client.clone()))
        .add(WriteFileTool::new(client.clone()))
        .add(ApplyPatchTool::new(client.clone()))
        .add(FindAndReplaceTool::new(client.clone()))
        .add(ReplaceLinesTool::new(client.clone()))
        .add(ListFilesTool::new(client.clone()))
        .add(GlobTool::new(client.clone()))
        .add(GrepTool::new(client.clone()))
        .add(SetWorkingDirTool::new(client.clone()))
        .add(SetEnvTool::new(client))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::testkit::MockTransport;
    use horsie_agentcore::Toolbox;

    /// The runtime resolves a tool's base directory itself — the caller's sticky
    /// working directory, else the first workspace — and an absolute path reaches
    /// anywhere else. A `workspace` property would be a second addressing scheme
    /// sent to the model on every request, so no runtime tool may grow one back.
    #[test]
    fn no_runtime_tool_advertises_a_workspace_property() {
        let client = RuntimeClient::detached(MockTransport::ok(""), "test-agent");
        let toolbox = add_runtime_tools(ToolboxImpl::default(), client);
        for spec in toolbox.specs() {
            let has_workspace = spec
                .input_schema
                .get("properties")
                .and_then(Value::as_object)
                .is_some_and(|p| p.contains_key("workspace"));
            assert!(
                !has_workspace,
                "tool '{}' still advertises a workspace property",
                spec.name
            );
        }
    }
}

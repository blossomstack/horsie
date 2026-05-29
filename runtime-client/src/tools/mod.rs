mod bash;
mod edit_file;
mod glob;
mod grep;
mod list_files;
mod read_file;
mod replace_in_file;
mod write_file;

pub use bash::BashTool;
pub use edit_file::EditFileTool;
pub use glob::GlobTool;
pub use grep::GrepTool;
pub use list_files::ListFilesTool;
pub use read_file::ReadFileTool;
pub use replace_in_file::ReplaceInFileTool;
pub use write_file::WriteFileTool;

use crate::client::RuntimeClient;
use agentcore::ToolboxImpl;

/// Add all runtime-backed tools to an existing ToolboxImpl.
pub fn add_runtime_tools(toolbox: ToolboxImpl, client: RuntimeClient) -> ToolboxImpl {
    toolbox
        .add(BashTool::new(client.clone()))
        .add(ReadFileTool::new(client.clone()))
        .add(WriteFileTool::new(client.clone()))
        .add(EditFileTool::new(client.clone()))
        .add(ReplaceInFileTool::new(client.clone()))
        .add(ListFilesTool::new(client.clone()))
        .add(GlobTool::new(client.clone()))
        .add(GrepTool::new(client))
}

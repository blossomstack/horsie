pub mod bash;
pub mod edit_file;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod read_file;
pub mod replace_in_file;
pub mod write_file;

use models::runtime::{ToolCall, ToolResult};
use std::path::Path;

pub async fn dispatch(working_dir: &Path, call: ToolCall) -> ToolResult {
    match call {
        ToolCall::Bash(input) => bash::exec(working_dir, input).await,
        ToolCall::ReadFile(input) => read_file::exec(working_dir, input).await,
        ToolCall::WriteFile(input) => write_file::exec(working_dir, input).await,
        ToolCall::EditFile(input) => edit_file::exec(working_dir, input).await,
        ToolCall::ReplaceInFile(input) => replace_in_file::exec(working_dir, input).await,
        ToolCall::ListFiles(input) => list_files::exec(working_dir, input).await,
        ToolCall::Glob(input) => glob::exec(working_dir, input).await,
        ToolCall::Grep(input) => grep::exec(working_dir, input).await,
    }
}

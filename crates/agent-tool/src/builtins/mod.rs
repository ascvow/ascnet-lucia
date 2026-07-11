//! 内置基础工具集。
//! Built-in basic tools.

mod list_dir;
mod read_file;
mod search;
mod shell;
mod write_file;

pub use list_dir::ListDirectoryTool;
pub use read_file::ReadFileTool;
pub use search::SearchFilesTool;
pub use shell::ShellTool;
pub use write_file::WriteFileTool;

use crate::ToolRegistry;
use anyhow::Result;

/// 使用默认配置将所有内置工具注册到 ToolRegistry。
/// Register all built-in tools with default settings.
pub fn register_builtins(registry: &mut ToolRegistry) -> Result<()> {
    registry.register(ReadFileTool)?;
    registry.register(WriteFileTool)?;
    registry.register(ListDirectoryTool)?;
    registry.register(ShellTool::default())?;
    registry.register(SearchFilesTool::default())?;
    Ok(())
}

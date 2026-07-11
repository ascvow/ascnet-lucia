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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;

    /// 内置工具描述必须给出明确选择场景和关键限制，帮助模型选择专用能力。
    #[test]
    fn builtin_specs_explain_selection_and_limits() {
        let specs = [
            ReadFileTool.spec(),
            WriteFileTool.spec(),
            ListDirectoryTool.spec(),
            ShellTool::default().spec(),
            SearchFilesTool::default().spec(),
        ];

        let description = |name: &str| {
            specs
                .iter()
                .find(|spec| spec.name == name)
                .map(|spec| spec.description.as_str())
                .expect("内置工具定义必须存在")
        };

        assert!(description("read_file").contains("locate it with search_files first"));
        assert!(description("write_file").contains("does not support partial edits or appends"));
        assert!(description("list_directory").contains("without recursing"));
        assert!(description("shell").contains("Prefer the dedicated tools"));
        assert!(description("search_files").contains("Regular expressions are not supported"));
    }
}

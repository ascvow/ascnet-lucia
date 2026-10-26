//! 内置基础工具集。
//! Built-in basic tools.

mod edit_file;
mod list_dir;
mod read_file;
mod search;
mod shell;
mod write_file;

pub use edit_file::EditFileTool;
pub use list_dir::ListDirectoryTool;
pub use read_file::ReadFileTool;
pub use search::SearchFilesTool;
pub use shell::ShellTool;
pub use write_file::WriteFileTool;

use crate::{ReadPathGrants, ToolRegistry, WorkspaceGuard};
use anyhow::Result;

/// 在指定工作区内注册全部内置工具。
///
/// `guard` 决定这些工具可以触碰的目录范围与文件能力，所有路径都会经它解析。
/// 调用方必须显式给出工作区：这是 Host 级的越权防线，不依赖任何插件参与。
/// `read_grants` 是用户在消息里显式引用的工作区外路径，只附加给读取与列目录工具；
/// 搜索、写入与创建按 `guard` 中工作区及应用额外目录的能力授权；Shell 工作目录仍限于工作区。
///
/// # Errors
///
/// 工具重名或名称非法时返回错误。
pub fn register_builtins(
    registry: &mut ToolRegistry,
    guard: WorkspaceGuard,
    read_grants: ReadPathGrants,
) -> Result<()> {
    let read_guard = guard.clone().with_read_grants(read_grants);
    registry.register(ReadFileTool::new(read_guard.clone()))?;
    registry.register(ListDirectoryTool::new(read_guard))?;
    registry.register(WriteFileTool::new(guard.clone()))?;
    registry.register(EditFileTool::new(guard.clone()))?;
    registry.register(ShellTool::new(guard.clone()))?;
    registry.register(SearchFilesTool::new(guard))?;
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
            ReadFileTool::default().spec(),
            WriteFileTool::default().spec(),
            EditFileTool::default().spec(),
            ListDirectoryTool::default().spec(),
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

        for spec in &specs {
            assert!(spec.description.is_ascii(), "{}", spec.name);
            assert!(
                spec.description.starts_with("<tool_usage>"),
                "{}",
                spec.name
            );
            assert!(spec.description.ends_with("</tool_usage>"), "{}", spec.name);
            assert!(spec.description.contains("<purpose>"), "{}", spec.name);
            assert!(spec.description.contains("<constraints>"), "{}", spec.name);
        }
        assert!(description("read_file").contains("8 MiB"));
        assert!(description("read_file").contains("256 KiB"));
        assert!(description("read_file").contains("1 MiB"));
        assert!(description("list_directory").contains("4096"));
        assert!(description("shell").contains("not a filesystem sandbox"));
        assert!(description("shell").contains("100 KiB"));
        assert!(description("read_file").contains("locate it with search_files first"));
        assert!(description("write_file").contains("does not support partial edits or appends"));
        assert!(description("edit_file").contains("Prefer this over write_file"));
        assert!(description("list_directory").contains("without recursing"));
        assert!(description("shell").contains("Prefer the dedicated tools"));
        assert!(description("search_files").contains("Regular expressions are not supported"));
    }
}

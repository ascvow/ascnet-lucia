//! 读取文件内容工具。
//! Read file content tool.

use crate::{FileCapability, Tool, ToolCall, ToolResult, ToolSpec, WorkspaceGuard};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// 读取文件内容，返回带行号的文本。
/// Read file content with line numbers.
///
/// 路径先经 [`WorkspaceGuard`] 解析，工作区之外的目标一律拒绝。
#[derive(Debug, Clone, Default)]
pub struct ReadFileTool {
    guard: WorkspaceGuard,
}

impl ReadFileTool {
    /// 以指定工作区守卫创建工具。
    pub fn new(guard: WorkspaceGuard) -> Self {
        Self { guard }
    }
}

#[derive(Deserialize)]
struct Args {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "read_file",
            "Read a known UTF-8 text file and return content with 1-based line numbers plus pagination metadata. Use this to inspect a file when its path is known. For large files, page with offset and limit; if the path is unknown, locate it with search_files first.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path to the UTF-8 text file to read"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "0-based line offset at which to start; 0 is the first line, default 0",
                        "minimum": 0
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of lines to return; default 2000, increase offset to continue",
                        "minimum": 1
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let args: Args = call.args_as()?;
        let offset = args.offset.unwrap_or(0);
        let limit = args.limit.unwrap_or(2000);

        let path = match self
            .guard
            .resolve_existing(&args.path, FileCapability::Read)
        {
            Ok(path) => path,
            Err(error) => return Ok(ToolResult::error(call.id, call.name, error.to_string())),
        };

        let content = match tokio::fs::read_to_string(&path).await {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::error(call.id, call.name, e.to_string())),
        };

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        if offset >= total {
            return Ok(ToolResult::success(
                call.id,
                call.name,
                json!({
                    "content": "",
                    "total_lines": total,
                    "offset": offset,
                    "lines_read": 0,
                }),
            ));
        }

        let end = (offset + limit).min(total);
        let numbered: Vec<String> = lines[offset..end]
            .iter()
            .enumerate()
            .map(|(i, line)| format!("{}\t{}", offset + i + 1, line))
            .collect();

        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "content": numbered.join("\n"),
                "total_lines": total,
                "offset": offset,
                "lines_read": end - offset,
            }),
        ))
    }
}

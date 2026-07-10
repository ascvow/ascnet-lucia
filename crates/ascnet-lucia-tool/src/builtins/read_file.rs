//! 读取文件内容工具。
//! Read file content tool.

use crate::{Tool, ToolCall, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// 读取文件内容，返回带行号的文本。
/// Read file content with line numbers.
pub struct ReadFileTool;

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
            "读取文件内容，返回带行号的文本。支持按行偏移和行数限制。",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径"
                    },
                    "offset": {
                        "type": "integer",
                        "description": "起始行偏移（0 表示第一行），默认 0",
                        "minimum": 0
                    },
                    "limit": {
                        "type": "integer",
                        "description": "最多读取行数，默认 2000",
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

        let content = match tokio::fs::read_to_string(&args.path).await {
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

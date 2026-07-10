//! 写入文件内容工具。
//! Write file content tool.

use crate::{Tool, ToolCall, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::path::Path;

/// 将内容写入文件，自动创建父目录。
/// Write content to a file, creating parent directories as needed.
pub struct WriteFileTool;

#[derive(Deserialize)]
struct Args {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "write_file",
            "将内容写入文件。如果文件已存在则覆盖，不存在则自动创建（含父目录）。",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "文件路径"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的内容"
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let args: Args = call.args_as()?;
        let path = Path::new(&args.path);

        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(e) = tokio::fs::create_dir_all(parent).await {
                    return Ok(ToolResult::error(
                        call.id,
                        call.name,
                        format!("无法创建父目录: {e}"),
                    ));
                }
            }
        }

        match tokio::fs::write(&args.path, &args.content).await {
            Ok(()) => {
                let bytes = args.content.len();
                Ok(ToolResult::success(
                    call.id,
                    call.name,
                    json!({
                        "path": args.path,
                        "bytes_written": bytes,
                    }),
                ))
            }
            Err(e) => Ok(ToolResult::error(call.id, call.name, e.to_string())),
        }
    }
}

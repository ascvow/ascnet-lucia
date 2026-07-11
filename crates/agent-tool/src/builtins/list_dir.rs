//! 列出目录内容工具。
//! List directory entries tool.

use crate::{Tool, ToolCall, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// 列出目录下的文件和子目录。
/// List files and subdirectories in a directory.
pub struct ListDirectoryTool;

#[derive(Deserialize)]
struct Args {
    path: String,
}

#[async_trait]
impl Tool for ListDirectoryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "list_directory",
            "List the direct children of a directory without recursing, returning each entry's name, type, and file size. Use this to inspect a known directory. To locate files recursively by content, use search_files instead.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path to inspect; only direct children are returned"
                    }
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let args: Args = call.args_as()?;

        let mut dir = match tokio::fs::read_dir(&args.path).await {
            Ok(d) => d,
            Err(e) => return Ok(ToolResult::error(call.id, call.name, e.to_string())),
        };

        let mut entries = Vec::new();
        while let Ok(Some(entry)) = dir.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let (kind, size) = match entry.metadata().await {
                Ok(meta) => {
                    let kind = if meta.is_dir() {
                        "directory"
                    } else if meta.is_symlink() {
                        "symlink"
                    } else {
                        "file"
                    };
                    (
                        kind,
                        if meta.is_file() {
                            Some(meta.len())
                        } else {
                            None
                        },
                    )
                }
                Err(_) => ("unknown", None),
            };
            entries.push(json!({
                "name": name,
                "type": kind,
                "size": size,
            }));
        }

        entries.sort_by(|a, b| {
            let a_type = a["type"].as_str().unwrap_or("");
            let b_type = b["type"].as_str().unwrap_or("");
            let a_name = a["name"].as_str().unwrap_or("");
            let b_name = b["name"].as_str().unwrap_or("");
            a_type.cmp(b_type).then(a_name.cmp(b_name))
        });

        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "path": args.path,
                "count": entries.len(),
                "entries": entries,
            }),
        ))
    }
}

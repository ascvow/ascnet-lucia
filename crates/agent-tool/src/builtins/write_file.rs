//! 写入文件内容工具。
//! Write file content tool.

use crate::{FileCapability, Tool, ToolCall, ToolResult, ToolSpec, WorkspaceGuard};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// 将内容写入文件，自动创建父目录。
/// Write content to a file, creating parent directories as needed.
///
/// 目标路径先经 [`WorkspaceGuard`] 解析：覆盖已有文件需要 `Write` 能力，
/// 新建文件需要 `Create` 能力，工作区之外的目标一律拒绝。
#[derive(Debug, Clone, Default)]
pub struct WriteFileTool {
    guard: WorkspaceGuard,
}

impl WriteFileTool {
    /// 以指定工作区守卫创建工具。
    pub fn new(guard: WorkspaceGuard) -> Self {
        Self { guard }
    }
}

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
            "Create or overwrite a file with complete content, creating parent directories when needed. Use this only for a new file or when full replacement is intended. It does not support partial edits or appends, and overwriting discards all existing content. Returns the path and number of bytes written.",
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Destination file path; an existing file will be completely overwritten"
                    },
                    "content": {
                        "type": "string",
                        "description": "Complete file content to write, not a patch or append fragment"
                    }
                },
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let args: Args = call.args_as()?;

        // 覆盖已有文件与新建文件是两种不同的能力，分别校验。
        let capability = if self.guard.exists(&args.path) {
            FileCapability::Write
        } else {
            FileCapability::Create
        };
        let path = match self.guard.resolve_new(&args.path, capability) {
            Ok(path) => path,
            Err(error) => return Ok(ToolResult::error(call.id, call.name, error.to_string())),
        };

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

        match tokio::fs::write(&path, &args.content).await {
            Ok(()) => {
                let bytes = args.content.len();
                Ok(ToolResult::success(
                    call.id,
                    call.name,
                    json!({
                        "path": path.to_string_lossy(),
                        "bytes_written": bytes,
                    }),
                ))
            }
            Err(e) => Ok(ToolResult::error(call.id, call.name, e.to_string())),
        }
    }
}

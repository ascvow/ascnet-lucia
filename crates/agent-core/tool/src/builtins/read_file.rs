//! 读取文件内容工具。
//! Read file content tool.

use crate::{
    FileCapability, Tool, ToolCall, ToolPermissionScope, ToolResult, ToolSpec, WorkspaceGuard,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncReadExt;

const MAX_READ_BYTES: usize = 8 * 1024 * 1024;
const MAX_LINE_BYTES: usize = 256 * 1024;
const MAX_PAGE_BYTES: usize = 1024 * 1024;

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
            r#"<tool_usage>
  <purpose>Read a known regular UTF-8 file and return numbered lines with exact pagination metadata.</purpose>
  <workflow>If the path is unknown, locate it with search_files first. Read only the relevant page; increase offset to continue. Displayed line numbers are not part of the file content.</workflow>
  <constraints>Files are limited to 8 MiB and lines to 256 KiB. Page JSON is limited to 1 MiB; reduce limit on overflow.</constraints>
</tool_usage>"#,
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
        .with_permission_scope(ToolPermissionScope::Read)
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
            Err(error) => {
                return Ok(ToolResult::error_with_kind(
                    call.id,
                    call.name,
                    error.tool_error_kind(),
                    error.to_string(),
                ))
            }
        };

        let content = match read_bounded_file(&path).await {
            Ok(content) => content,
            Err(error) => return Ok(ToolResult::error(call.id, call.name, error.to_string())),
        };
        match numbered_page(&content, offset, limit) {
            Ok(page) => Ok(ToolResult::success(call.id, call.name, page)),
            Err(error) => Ok(ToolResult::error(call.id, call.name, error.to_string())),
        }
    }
}

/// 预检只用于快速拒绝，读取本身限制为上限加一，防止文件在读取期间增长。
async fn read_bounded_file(path: &std::path::Path) -> Result<String> {
    let before = tokio::fs::symlink_metadata(path).await?;
    if !before.is_file() || before.file_type().is_symlink() {
        anyhow::bail!("read_file requires a regular file");
    }
    let owned = path.to_owned();
    let opened_file =
        tokio::task::spawn_blocking(move || crate::workspace::open_canonical_file_read(&owned))
            .await??;
    let file = tokio::fs::File::from_std(opened_file);
    let opened = file.metadata().await?;
    if !opened.is_file() {
        anyhow::bail!("read_file requires a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != opened.dev() || before.ino() != opened.ino() {
            anyhow::bail!("file changed while opening");
        }
    }
    if opened.len() > MAX_READ_BYTES as u64 {
        anyhow::bail!("file exceeds 8 MiB read limit");
    }
    let mut bytes = Vec::new();
    file.take(MAX_READ_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() > MAX_READ_BYTES {
        anyhow::bail!("file exceeds 8 MiB read limit");
    }
    Ok(String::from_utf8(bytes)?)
}

/// 不收集全部行引用；准确计数，任何超限返回错误而不是伪造分页总量。
fn numbered_page(content: &str, offset: usize, limit: usize) -> Result<serde_json::Value> {
    let end = offset.saturating_add(limit);
    let mut text = String::new();
    let mut total = 0usize;
    let mut count = 0usize;
    for (index, line) in content.lines().enumerate() {
        total += 1;
        if line.len() > MAX_LINE_BYTES {
            anyhow::bail!("line exceeds 256 KiB limit");
        }
        if index < offset || index >= end {
            continue;
        }
        let number = (index + 1).to_string();
        if text
            .len()
            .saturating_add(line.len())
            .saturating_add(number.len() + 2)
            > MAX_PAGE_BYTES
        {
            anyhow::bail!("page exceeds 1 MiB; reduce limit");
        }
        if count > 0 {
            text.push('\n');
        }
        text.push_str(&number);
        text.push('\t');
        text.push_str(line);
        count += 1;
    }
    let page = json!({"content":text,"total_lines":total,"offset":offset,"lines_read":count});
    if serde_json::to_vec(&page)?.len() > MAX_PAGE_BYTES {
        anyhow::bail!("page JSON exceeds 1 MiB; reduce limit");
    }
    Ok(page)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ReadPathGrants, Tool, ToolCall, WorkspaceGuard};

    /// 极端分页参数不可溢出，Unicode/CRLF 和总行数保持兼容。
    #[test]
    fn bounded_pages_preserve_pagination_and_reject_giant_lines() {
        let page = numbered_page("甲\r\n\n🙂\n末", 1, usize::MAX).unwrap();
        assert_eq!(
            page,
            json!({"content":"2\t\n3\t🙂\n4\t末","total_lines":4,"offset":1,"lines_read":3})
        );
        assert_eq!(
            numbered_page("a\nb", usize::MAX, usize::MAX).unwrap()["lines_read"],
            0
        );
        assert!(numbered_page(&"x".repeat(MAX_LINE_BYTES + 1), 0, 1).is_err());
        assert!(
            numbered_page(&format!("{}\n", "x".repeat(MAX_LINE_BYTES)).repeat(5), 0, 5).is_err()
        );
    }

    /// 大文件在读取前拒绝，目录也不能进入文本读取。
    #[tokio::test]
    async fn bounded_read_rejects_large_and_special_files() {
        let root = std::env::temp_dir().join(format!(
            "lucia-read-bounds-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("large");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_READ_BYTES as u64 + 1)
            .unwrap();
        let path = path.canonicalize().unwrap();
        assert!(read_bounded_file(&path)
            .await
            .unwrap_err()
            .to_string()
            .contains("8 MiB"));
        assert!(read_bounded_file(&root).await.is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// 用户显式授权的外部文本文件可以被只读工具读取。
    #[tokio::test]
    async fn reads_user_granted_external_file() {
        let base = std::env::temp_dir().join(format!(
            "lucia-read-grant-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let inside = base.join("inside");
        let outside = base.join("outside.txt");
        std::fs::create_dir_all(&inside).expect("应创建工作区");
        std::fs::write(&outside, "line one\nline two").expect("应写入外部文件");

        let grants = ReadPathGrants::new();
        grants.grant(&outside).expect("应登记外部路径");
        let guard = WorkspaceGuard::rooted(&inside)
            .expect("应创建守卫")
            .with_read_grants(grants);
        let tool = ReadFileTool::new(guard);
        let call = ToolCall::new(
            "read-granted",
            "read_file",
            json!({ "path": outside.to_str().expect("测试路径应为 UTF-8") }),
        );

        let result = tool.call(call).await.expect("工具应返回结果");
        assert!(!result.is_error, "授权路径不应报错：{:?}", result.content);
        assert!(
            result.content["content"]
                .as_str()
                .is_some_and(|content| content.contains("line two")),
            "应读取到外部文件内容：{:?}",
            result.content
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}

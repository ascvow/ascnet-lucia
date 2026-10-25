//! 按精确文本片段替换的文件编辑工具。
//! Exact-substring file editing tool.

use crate::{
    FileCapability, Tool, ToolCall, ToolErrorKind, ToolPermissionScope, ToolResult, ToolSpec,
    WorkspaceGuard,
};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;

/// 统一 diff 中每段变更前后保留的上下文行数。
const DIFF_CONTEXT_LINES: usize = 3;

/// 在已有文件中把一段精确文本替换为另一段。
/// Replace an exact text fragment inside an existing file.
///
/// 相对 [`super::WriteFileTool`] 的价值在于改一行不必重写整个文件：模型只需要给出待替换
/// 片段，输出规模与改动大小成正比而不是与文件大小成正比，也不会顺带覆盖无关内容。
///
/// 默认要求 `old_string` 在文件中唯一出现，匹配不到或匹配到多处都会失败并说明原因，避免
/// 改错位置。确实要改全部同名片段时由调用方显式传 `replace_all`。
#[derive(Debug, Clone, Default)]
pub struct EditFileTool {
    guard: WorkspaceGuard,
}

impl EditFileTool {
    /// 以指定工作区守卫创建工具。
    pub fn new(guard: WorkspaceGuard) -> Self {
        Self { guard }
    }
}

#[derive(Deserialize)]
struct Args {
    path: String,
    old_string: String,
    new_string: String,
    #[serde(default)]
    replace_all: bool,
}

#[async_trait]
impl Tool for EditFileTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "edit_file",
            r#"<tool_usage>
  <purpose>Replace an exact text fragment in an existing file, keeping all other bytes unchanged.</purpose>
  <workflow>Prefer this over write_file for existing files. Read the file first and copy old_string exactly, including whitespace but excluding displayed line numbers. Include enough context for a unique match. Inspect the returned diff and run relevant validation.</workflow>
  <constraints>old_string must be nonempty and differ from new_string. A missing or ambiguous match fails without changes; use replace_all only when every occurrence should change.</constraints>
  <result>A unified diff of the applied change, not proof that the change passes tests.</result>
</tool_usage>"#,
            json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Path of the file to edit; the file must already exist"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to replace, copied verbatim from the file including indentation. Include enough surrounding lines to make it unique"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text; use an empty string to delete the matched fragment"
                    },
                    "replace_all": {
                        "type": "boolean",
                        "description": "Replace every occurrence instead of requiring a unique match. Defaults to false"
                    }
                },
                "required": ["path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        )
        .with_permission_scope(ToolPermissionScope::Write)
    }

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let args: Args = call.args_as()?;

        // 编辑只针对已存在的文件；新建文件仍然走 write_file，两者能力不同。
        let path = match self.guard.resolve_new(&args.path, FileCapability::Write) {
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
        if !path.is_file() {
            return Ok(ToolResult::error_with_kind(
                call.id,
                call.name,
                ToolErrorKind::InvalidArguments,
                format!(
                    "文件不存在：{}。新建文件请使用 write_file。",
                    path.to_string_lossy()
                ),
            ));
        }

        let original = match tokio::fs::read_to_string(&path).await {
            Ok(text) => text,
            Err(error) => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("读取文件失败：{error}"),
                ))
            }
        };

        if args.old_string.is_empty() {
            return Ok(ToolResult::error_with_kind(
                call.id,
                call.name,
                ToolErrorKind::InvalidArguments,
                "old_string 不能为空；写入全新内容请使用 write_file。".to_string(),
            ));
        }
        if args.old_string == args.new_string {
            return Ok(ToolResult::error_with_kind(
                call.id,
                call.name,
                ToolErrorKind::InvalidArguments,
                "old_string 与 new_string 相同，该编辑不会产生任何变化。".to_string(),
            ));
        }

        let matches = original.matches(&args.old_string).count();
        match matches {
            0 => {
                return Ok(ToolResult::error_with_kind(
                    call.id,
                    call.name,
                    ToolErrorKind::InvalidArguments,
                    "old_string 在文件中不存在。请先读取文件并逐字复制待替换片段，注意缩进和空白。".to_string(),
                ))
            }
            // 多处匹配时不猜测目标位置：要么补足上下文，要么显式声明改全部。
            count if count > 1 && !args.replace_all => {
                return Ok(ToolResult::error_with_kind(
                    call.id,
                    call.name,
                    ToolErrorKind::InvalidArguments,
                    format!(
                        "old_string 在文件中出现 {count} 次，无法确定目标。请扩大片段使其唯一，或传 replace_all 替换全部。"
                    ),
                ))
            }
            _ => {}
        }

        let updated = if args.replace_all {
            original.replace(&args.old_string, &args.new_string)
        } else {
            original.replacen(&args.old_string, &args.new_string, 1)
        };

        if let Err(error) = tokio::fs::write(&path, &updated).await {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                format!("写入文件失败：{error}"),
            ));
        }

        let diff = unified_diff(&args.path, &original, &updated);
        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "path": path.to_string_lossy(),
                "replacements": if args.replace_all { matches } else { 1 },
                "diff": diff,
            }),
        ))
    }
}

/// 生成变更片段的统一 diff 文本。
///
/// 只输出发生变化的区块及其前后各 [`DIFF_CONTEXT_LINES`] 行，因此结果规模跟改动量成正比。
/// 界面按统一 diff 识别并着色，模型也能据此确认实际落到文件里的内容。
fn unified_diff(display_path: &str, original: &str, updated: &str) -> String {
    let old_lines: Vec<&str> = original.lines().collect();
    let new_lines: Vec<&str> = updated.lines().collect();
    // 逐字替换只改动中间一段，因此公共前后缀之外就是全部变更。
    let prefix = common_prefix_len(&old_lines, &new_lines);
    let suffix = common_suffix_len(&old_lines[prefix..], &new_lines[prefix..]);
    let old_end = old_lines.len() - suffix;
    let new_end = new_lines.len() - suffix;

    let context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let old_context_end = (old_end + DIFF_CONTEXT_LINES).min(old_lines.len());
    let new_context_end = (new_end + DIFF_CONTEXT_LINES).min(new_lines.len());

    let mut diff = String::new();
    diff.push_str(&format!("--- a/{display_path}\n"));
    diff.push_str(&format!("+++ b/{display_path}\n"));
    diff.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        context_start + 1,
        old_context_end - context_start,
        context_start + 1,
        new_context_end - context_start,
    ));
    for line in &old_lines[context_start..prefix] {
        diff.push_str(&format!(" {line}\n"));
    }
    for line in &old_lines[prefix..old_end] {
        diff.push_str(&format!("-{line}\n"));
    }
    for line in &new_lines[prefix..new_end] {
        diff.push_str(&format!("+{line}\n"));
    }
    for line in &new_lines[new_end..new_context_end] {
        diff.push_str(&format!(" {line}\n"));
    }
    diff
}

/// 返回两组行的公共前缀长度。
fn common_prefix_len(left: &[&str], right: &[&str]) -> usize {
    left.iter()
        .zip(right.iter())
        .take_while(|(a, b)| a == b)
        .count()
}

/// 返回两组行的公共后缀长度。
fn common_suffix_len(left: &[&str], right: &[&str]) -> usize {
    left.iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(a, b)| a == b)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// 在系统临时目录下建立唯一工作区，并写入一个待编辑文件。
    ///
    /// 与 `workspace` 模块的测试保持同一套隔离方式：按进程与线程命名，避免并发用例互相
    /// 干扰，也不引入额外依赖。
    fn fixture(tag: &str, content: &str) -> (std::path::PathBuf, WorkspaceGuard) {
        let base = std::env::temp_dir().join(format!(
            "lucia-edit-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("创建临时工作区");
        std::fs::write(base.join("sample.rs"), content).expect("写入夹具文件");
        let guard = WorkspaceGuard::rooted(&base).expect("创建工作区守卫");
        (base, guard)
    }

    /// 构造一次编辑调用。
    fn edit_call(path: &str, old: &str, new: &str, replace_all: bool) -> ToolCall {
        ToolCall::new(
            "edit-1",
            "edit_file",
            json!({
                "path": path,
                "old_string": old,
                "new_string": new,
                "replace_all": replace_all,
            }),
        )
    }

    /// 唯一匹配时只替换目标片段，其余内容逐字节保留，并返回统一 diff。
    #[tokio::test]
    async fn unique_match_replaces_only_the_target() {
        let (dir, guard) = fixture("unique", "fn a() {}\nfn target() {}\nfn b() {}\n");
        let tool = EditFileTool::new(guard);

        let result = tool
            .call(edit_call(
                "sample.rs",
                "fn target() {}",
                "fn renamed() {}",
                false,
            ))
            .await
            .expect("编辑应返回结果");

        assert!(!result.is_error, "{:?}", result.content);
        let updated = std::fs::read_to_string(dir.join("sample.rs")).expect("读取结果");
        assert_eq!(updated, "fn a() {}\nfn renamed() {}\nfn b() {}\n");
        let diff = result.content["diff"].as_str().expect("应返回 diff");
        assert!(diff.contains("-fn target() {}"), "{diff}");
        assert!(diff.contains("+fn renamed() {}"), "{diff}");
        // 未改动的行以上下文形式出现，不带增删前缀。
        assert!(diff.contains(" fn a() {}"), "{diff}");
    }

    /// 匹配到多处且未声明 replace_all 时必须失败，且不改动文件。
    #[tokio::test]
    async fn ambiguous_match_fails_without_touching_the_file() {
        let (dir, guard) = fixture("ambiguous", "let x = 1;\nlet x = 1;\n");
        let tool = EditFileTool::new(guard);

        let result = tool
            .call(edit_call("sample.rs", "let x = 1;", "let x = 2;", false))
            .await
            .expect("编辑应返回结果");

        assert!(result.is_error);
        assert!(
            result.content.to_string().contains("出现 2 次"),
            "{:?}",
            result.content
        );
        let unchanged = std::fs::read_to_string(dir.join("sample.rs")).expect("读取结果");
        assert_eq!(unchanged, "let x = 1;\nlet x = 1;\n");
    }

    /// 显式声明 replace_all 时替换全部匹配并报告次数。
    #[tokio::test]
    async fn replace_all_updates_every_occurrence() {
        let (dir, guard) = fixture("replace-all", "let x = 1;\nlet x = 1;\n");
        let tool = EditFileTool::new(guard);

        let result = tool
            .call(edit_call("sample.rs", "let x = 1;", "let x = 2;", true))
            .await
            .expect("编辑应返回结果");

        assert!(!result.is_error, "{:?}", result.content);
        assert_eq!(result.content["replacements"], Value::from(2));
        let updated = std::fs::read_to_string(dir.join("sample.rs")).expect("读取结果");
        assert_eq!(updated, "let x = 2;\nlet x = 2;\n");
    }

    /// 匹配不到片段时给出可操作的提示，而不是静默写入。
    #[tokio::test]
    async fn missing_match_explains_how_to_fix() {
        let (_dir, guard) = fixture("missing-match", "fn a() {}\n");
        let tool = EditFileTool::new(guard);

        let result = tool
            .call(edit_call(
                "sample.rs",
                "fn missing() {}",
                "fn b() {}",
                false,
            ))
            .await
            .expect("编辑应返回结果");

        assert!(result.is_error);
        assert!(
            result.content.to_string().contains("逐字复制"),
            "{:?}",
            result.content
        );
    }

    /// 目标文件不存在时提示改用 write_file，不创建新文件。
    #[tokio::test]
    async fn missing_file_points_to_write_file() {
        let (dir, guard) = fixture("missing-file", "fn a() {}\n");
        let tool = EditFileTool::new(guard);

        let result = tool
            .call(edit_call("absent.rs", "a", "b", false))
            .await
            .expect("编辑应返回结果");

        assert!(result.is_error);
        assert!(
            result.content.to_string().contains("write_file"),
            "{:?}",
            result.content
        );
        assert!(!dir.join("absent.rs").exists());
    }

    /// 工作区之外的路径必须被守卫拒绝。
    #[tokio::test]
    async fn path_outside_workspace_is_rejected() {
        let (_dir, guard) = fixture("escape", "fn a() {}\n");
        let tool = EditFileTool::new(guard);

        let result = tool
            .call(edit_call("../outside.rs", "a", "b", false))
            .await
            .expect("编辑应返回结果");

        assert!(result.is_error);
    }
}

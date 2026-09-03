//! 增量索引的事件适配与脏路径工具。
//!
//! 本模块只处理宿主事件到仓库相对路径的安全转换，不持有索引状态，也不解析具体工具的
//! 文件正文。索引重建由 [`crate::index::IndexBuild`] 负责；这样事件协议变化不会把业务
//! 判断扩散到结构索引或检索索引内部。

use serde_json::Value;
use std::collections::BTreeSet;

/// 单个插件实例最多保留的脏路径数量。
pub const MAX_DIRTY_PATHS: usize = 4_096;

/// 单个插件实例最多暂存的未完成写入调用数量。
pub const MAX_PENDING_WRITES: usize = 1_024;

/// 一次尚未收到终态结果的写入调用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingWrite {
    /// 可信运行 ID，用于在运行结束时清理残留调用。
    pub run_id: String,
    /// 事件中的工具名。
    pub tool_name: String,
    /// 从调用参数中提取出的仓库相对路径。
    pub paths: BTreeSet<String>,
}

/// 判断工具名是否表示可能修改仓库文件的调用。
///
/// 只接受已知写入工具，避免把任意工具结果中的字符串误当成路径。带命名空间的工具名
/// （例如 `core.write_file`）按最后一个 `.` 或 `::` 分段判断。
pub fn is_write_tool(name: &str) -> bool {
    let base = name
        .rsplit_once("::")
        .map(|(_, value)| value)
        .or_else(|| name.rsplit_once('.').map(|(_, value)| value))
        .unwrap_or(name)
        .trim()
        .to_ascii_lowercase();
    matches!(
        base.as_str(),
        "write_file"
            | "edit_file"
            | "create_file"
            | "delete_file"
            | "remove_file"
            | "rename_file"
            | "move_file"
            | "copy_file"
            | "apply_patch"
            | "patch_file"
            | "replace_file"
            | "str_replace"
            | "write"
            | "edit"
    )
}

/// 从写入调用参数中提取并规范化仓库相对路径。
///
/// 只遍历语义上表示路径的字段（`path`、`old_path`、`new_path`、`files` 等），不会扫描
/// 文件正文或 diff 中的任意单词。`apply_patch` 的标准文件头是唯一例外，会按补丁格式
/// 读取 `*** Update File:` 等行。
pub fn extract_paths(tool_name: &str, args: &Value) -> BTreeSet<String> {
    extract_paths_with_normalizer(tool_name, args, &normalize_path)
}

/// 从工具参数中提取路径，并把工作区内的绝对路径转换为仓库相对路径。
///
/// 原生写入工具的成功结果可能返回经过 Host 解析后的绝对路径；只有路径位于可信工作区
/// 根下时才允许转换，越界路径仍会被拒绝。
pub fn extract_paths_in_workspace(
    tool_name: &str,
    args: &Value,
    workspace_root: &str,
) -> BTreeSet<String> {
    let mut paths = extract_paths(tool_name, args);
    let normalize = |raw: &str| normalize_workspace_path(raw, workspace_root);
    paths.extend(extract_paths_with_normalizer(tool_name, args, &normalize));
    paths
}

/// 从工具结果的脱敏内容中提取路径，作为调用参数缺失时的安全兜底。
pub fn extract_result_paths(tool_name: &str, content: &Value) -> BTreeSet<String> {
    extract_paths(tool_name, content)
}

/// 从工具结果中提取路径，并支持 Host 返回的工作区绝对路径。
pub fn extract_result_paths_in_workspace(
    tool_name: &str,
    content: &Value,
    workspace_root: &str,
) -> BTreeSet<String> {
    let mut paths = extract_result_paths(tool_name, content);
    let normalize = |raw: &str| normalize_workspace_path(raw, workspace_root);
    paths.extend(extract_paths_with_normalizer(
        tool_name, content, &normalize,
    ));
    paths
}

/// 使用指定规范化器递归提取路径字段。
fn extract_paths_with_normalizer<F>(
    tool_name: &str,
    args: &Value,
    normalize: &F,
) -> BTreeSet<String>
where
    F: Fn(&str) -> Option<String>,
{
    let mut paths = BTreeSet::new();
    if tool_basename(tool_name).eq_ignore_ascii_case("apply_patch") {
        collect_patch_text(args, &mut paths, normalize);
    }
    collect_value(tool_name, args, None, &mut paths, normalize);
    paths
}

/// 把外部字符串校验为仓库相对路径。
///
/// 绝对路径、盘符路径和包含 `..` 的路径一律拒绝；这既避免把工作区之外的文件加入脏
/// 集合，也避免后续宿主读取调用收到未经验证的逃逸路径。返回值使用 `/` 分隔并去除多余
/// 的 `.` 段。
pub fn normalize_path(raw: &str) -> Option<String> {
    // 先统一分隔符，再判断绝对路径；否则 Windows 反斜杠形式可能绕过安全检查。
    let replaced = raw.trim().replace('\\', "/");
    if replaced.is_empty() || replaced.starts_with('/') || replaced.starts_with('~') {
        return None;
    }
    let bytes = replaced.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return None;
    }
    let mut parts = Vec::new();
    for part in replaced.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.chars().any(char::is_control) {
            return None;
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// 将工作区内的绝对路径转换为安全的仓库相对路径。
pub fn normalize_workspace_path(raw: &str, workspace_root: &str) -> Option<String> {
    let raw = raw.trim();
    if let Some(relative) = normalize_path(raw) {
        return Some(relative);
    }

    let normalized_raw = raw.replace('\\', "/");
    let normalized_root = workspace_root
        .trim()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    if normalized_root.is_empty() {
        return None;
    }
    let prefix = format!("{normalized_root}/");
    let relative = normalized_raw.strip_prefix(&prefix)?;
    normalize_path(relative)
}

/// 仅允许作为路径的字段名。
fn is_path_key(key: &str) -> bool {
    matches!(
        key.to_ascii_lowercase().as_str(),
        "path"
            | "file_path"
            | "filepath"
            | "filename"
            | "file"
            | "target"
            | "source"
            | "old_path"
            | "new_path"
            | "from"
            | "to"
            | "paths"
            | "files"
            | "file_paths"
            | "old"
            | "new"
    )
}

/// 取得工具名中不带命名空间的最后一段。
fn tool_basename(tool_name: &str) -> &str {
    tool_name
        .rsplit_once("::")
        .map(|(_, value)| value)
        .or_else(|| tool_name.rsplit_once('.').map(|(_, value)| value))
        .unwrap_or(tool_name)
        .trim()
}

/// 递归遍历带字段语义的 JSON 值。
fn collect_value(
    tool_name: &str,
    value: &Value,
    path_hint: Option<bool>,
    paths: &mut BTreeSet<String>,
    normalize: &impl Fn(&str) -> Option<String>,
) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let key_lower = key.to_ascii_lowercase();
                if is_path_key(key) {
                    collect_path_value(tool_name, child, paths, normalize);
                } else if tool_name
                    .rsplit_once("::")
                    .map(|(_, value)| value)
                    .or_else(|| tool_name.rsplit_once('.').map(|(_, value)| value))
                    .unwrap_or(tool_name)
                    .eq_ignore_ascii_case("apply_patch")
                    && (key_lower == "patch" || key_lower == "diff")
                {
                    collect_patch_text(child, paths, normalize);
                } else if matches!(child, Value::Object(_) | Value::Array(_)) {
                    collect_value(tool_name, child, path_hint, paths, normalize);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_value(tool_name, item, path_hint, paths, normalize);
            }
        }
        Value::String(text) if path_hint == Some(true) => {
            if let Some(path) = normalize(text) {
                paths.insert(path);
            }
        }
        _ => {}
    }
}

/// 读取路径字段的字符串、数组或嵌套编辑对象。
fn collect_path_value(
    tool_name: &str,
    value: &Value,
    paths: &mut BTreeSet<String>,
    normalize: &impl Fn(&str) -> Option<String>,
) {
    match value {
        Value::String(text) => {
            if let Some(path) = normalize(text) {
                paths.insert(path);
            }
            if tool_name
                .rsplit_once("::")
                .map(|(_, value)| value)
                .or_else(|| tool_name.rsplit_once('.').map(|(_, value)| value))
                .unwrap_or(tool_name)
                .eq_ignore_ascii_case("apply_patch")
            {
                collect_patch_text(value, paths, normalize);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_path_value(tool_name, item, paths, normalize);
            }
        }
        Value::Object(map) => {
            for (key, child) in map {
                if is_path_key(key) || matches!(child, Value::Object(_) | Value::Array(_)) {
                    collect_path_value(tool_name, child, paths, normalize);
                }
            }
        }
        _ => {}
    }
}

/// 从标准补丁头提取文件路径。
fn collect_patch_text(
    value: &Value,
    paths: &mut BTreeSet<String>,
    normalize: &impl Fn(&str) -> Option<String>,
) {
    let Some(text) = value.as_str() else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        let candidate = [
            "*** Update File: ",
            "*** Add File: ",
            "*** Delete File: ",
            "*** Move to: ",
            "+++ b/",
            "--- a/",
        ]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix));
        if let Some(candidate) = candidate.and_then(normalize) {
            paths.insert(candidate);
            continue;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let mut fields = rest.split_whitespace();
            for field in fields.by_ref().take(2) {
                let field = field
                    .strip_prefix("a/")
                    .or_else(|| field.strip_prefix("b/"));
                if let Some(path) = field.and_then(normalize) {
                    paths.insert(path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 路径规范化必须拒绝越界与绝对路径，并统一分隔符。
    #[test]
    fn normalizes_safe_relative_paths() {
        assert_eq!(normalize_path("./src\\lib.rs"), Some("src/lib.rs".into()));
        assert_eq!(normalize_path("../outside.rs"), None);
        assert_eq!(normalize_path("/tmp/outside.rs"), None);
        assert_eq!(normalize_path("\\absolute\\file.rs"), None);
        assert_eq!(normalize_path("\\\\server\\share\\file.rs"), None);
        assert_eq!(normalize_path("C:\\repo\\file.rs"), None);
        assert_eq!(
            normalize_workspace_path("/repo/src/lib.rs", "/repo"),
            Some("src/lib.rs".into())
        );
        assert_eq!(
            normalize_workspace_path("C:\\repo\\src\\lib.rs", "C:\\repo"),
            Some("src/lib.rs".into())
        );
        assert_eq!(normalize_workspace_path("/repo2/src/lib.rs", "/repo"), None);
    }

    /// 写入参数只应提取路径字段并自动去重。
    #[test]
    fn extracts_and_deduplicates_write_paths() {
        let paths = extract_paths(
            "edit_file",
            &json!({"path": "./src/a.rs", "edits": [{"path": "src/a.rs"}]}),
        );
        assert_eq!(paths, BTreeSet::from(["src/a.rs".to_string()]));
    }

    /// 重命名与补丁格式必须同时覆盖旧路径和新路径。
    #[test]
    fn extracts_rename_and_patch_paths() {
        let rename = extract_paths(
            "rename_file",
            &json!({"old_path": "src/old.rs", "new_path": "src/new.rs"}),
        );
        assert_eq!(
            rename,
            BTreeSet::from(["src/new.rs".to_string(), "src/old.rs".to_string()])
        );
        let patch = extract_paths(
            "apply_patch",
            &json!({"patch": "*** Update File: src/a.rs\n*** Add File: src/b.rs"}),
        );
        assert_eq!(
            patch,
            BTreeSet::from(["src/a.rs".to_string(), "src/b.rs".to_string()])
        );
    }

    /// 非写入工具不得被误判为文件变更工具。
    #[test]
    fn write_tool_classifier_is_conservative() {
        assert!(is_write_tool("write_file"));
        assert!(is_write_tool("plugin.edit_file"));
        assert!(!is_write_tool("read_file"));
        assert!(!is_write_tool("code_search"));
    }

    /// 补丁正文即使没有包在 `patch` 字段中也应能提取文件头路径。
    #[test]
    fn extracts_standalone_patch_text() {
        let paths = extract_paths(
            "apply_patch",
            &Value::String("*** Update File: src/a.rs\n".into()),
        );
        assert_eq!(paths, BTreeSet::from(["src/a.rs".to_string()]));
    }
}

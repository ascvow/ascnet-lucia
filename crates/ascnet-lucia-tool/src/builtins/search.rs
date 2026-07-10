//! 在文件中搜索文本工具。
//! Search text in files tool.

use crate::{Tool, ToolCall, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use std::fs;
use std::path::Path;

const DEFAULT_MAX_RESULTS: usize = 50;
const MAX_RESULTS_LIMIT: usize = 500;
const MAX_DEPTH: usize = 12;
/// 跳过大于此字节数的文件。
const MAX_FILE_SIZE: u64 = 1024 * 1024;

/// 跳过的目录名称。
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "__pycache__",
    ".venv",
    "venv",
    ".idea",
    ".vscode",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".cache",
];

/// 在目录树中递归搜索文本内容。
/// Recursively search for text content in a directory tree.
pub struct SearchFilesTool {
    /// 默认最大结果数。
    default_max_results: usize,
}

impl Default for SearchFilesTool {
    fn default() -> Self {
        Self {
            default_max_results: DEFAULT_MAX_RESULTS,
        }
    }
}

impl SearchFilesTool {
    /// 使用自定义默认最大结果数创建。
    pub fn with_max_results(max_results: usize) -> Self {
        Self {
            default_max_results: max_results.min(MAX_RESULTS_LIMIT),
        }
    }
}

#[derive(Deserialize)]
struct Args {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    include: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

#[derive(serde::Serialize)]
struct Match {
    file: String,
    line: usize,
    content: String,
}

/// 检查文件名是否匹配 include 过滤器（如 `*.rs`）。
fn matches_include(file_name: &str, include: &str) -> bool {
    if let Some(suffix) = include.strip_prefix('*') {
        file_name.ends_with(suffix)
    } else {
        file_name == include
    }
}

/// 检查是否为二进制文件（前 512 字节包含 null 字节）。
fn is_likely_binary(buf: &[u8]) -> bool {
    let check_len = buf.len().min(512);
    buf[..check_len].contains(&0)
}

/// 同步递归搜索。
fn search_recursive(
    dir: &Path,
    pattern: &str,
    include: Option<&str>,
    depth: usize,
    results: &mut Vec<Match>,
    max_results: usize,
) {
    if depth > MAX_DEPTH || results.len() >= max_results {
        return;
    }

    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut dirs = Vec::new();
    let mut files = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            dirs.push(path);
        } else {
            files.push(path);
        }
    }

    for file_path in files {
        if results.len() >= max_results {
            return;
        }

        let file_name = match file_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };

        if let Some(inc) = include {
            if !matches_include(&file_name, inc) {
                continue;
            }
        }

        let meta = match fs::metadata(&file_path) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if meta.len() > MAX_FILE_SIZE {
            continue;
        }

        let bytes = match fs::read(&file_path) {
            Ok(b) => b,
            Err(_) => continue,
        };

        if is_likely_binary(&bytes) {
            continue;
        }

        let content = match std::str::from_utf8(&bytes) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let path_str = file_path.to_string_lossy().to_string();
        for (i, line) in content.lines().enumerate() {
            if line.contains(pattern) {
                results.push(Match {
                    file: path_str.clone(),
                    line: i + 1,
                    content: truncate_line(line, 200),
                });
                if results.len() >= max_results {
                    return;
                }
            }
        }
    }

    for dir_path in dirs {
        if results.len() >= max_results {
            return;
        }
        let dir_name = match dir_path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => continue,
        };
        if SKIP_DIRS.contains(&dir_name) {
            continue;
        }
        search_recursive(&dir_path, pattern, include, depth + 1, results, max_results);
    }
}

/// 截断过长的行。
fn truncate_line(line: &str, max_chars: usize) -> String {
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(max_chars).collect();
        format!("{truncated}...")
    }
}

#[async_trait]
impl Tool for SearchFilesTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec::new(
            "search_files",
            "在目录中递归搜索包含指定文本的文件，返回匹配行及其位置。",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "要搜索的文本（大小写敏感的子串匹配）"
                    },
                    "path": {
                        "type": "string",
                        "description": "搜索起始目录，默认当前目录"
                    },
                    "include": {
                        "type": "string",
                        "description": "文件名过滤（如 *.rs），不指定则搜索所有文件"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "最大返回结果数，默认 50",
                        "minimum": 1,
                        "maximum": 500
                    }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        )
    }

    async fn call(&self, call: ToolCall) -> Result<ToolResult> {
        let args: Args = call.args_as()?;
        let max_results = args
            .max_results
            .unwrap_or(self.default_max_results)
            .min(MAX_RESULTS_LIMIT);
        let base_path = args.path.clone().unwrap_or_else(|| ".".to_string());
        let pattern = args.pattern.clone();
        let include = args.include.clone();

        let results = tokio::task::spawn_blocking(move || {
            let dir = Path::new(&base_path);
            if !dir.is_dir() {
                return Vec::new();
            }
            let mut matches = Vec::new();
            search_recursive(
                dir,
                &pattern,
                include.as_deref(),
                0,
                &mut matches,
                max_results,
            );
            matches
        })
        .await?;

        let count = results.len();
        let truncated = count >= max_results;

        Ok(ToolResult::success(
            call.id,
            call.name,
            json!({
                "matches": results,
                "count": count,
                "truncated": truncated,
            }),
        ))
    }
}

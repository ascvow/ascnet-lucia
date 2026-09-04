//! 当前仓库的可执行代码搜索入口。
//!
//! 该示例把 Lucia 仓库中全部受支持的文本与源码文件装入测试宿主，驱动
//! `CodeContextPlugin` 完成分片索引，再输出 `code_search` 返回的文件位置与代码片段。

use agent_plugin::{AgentPlugin, ToolCall};
use anyhow::{anyhow, Context, Result};
use code_context_plugin::{
    classify,
    testing::{activation_context, RepoHost},
    CodeContextPlugin, IndexStatus, PathClass,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// 单次命令允许推进索引的最大轮数，避免异常状态造成无限循环。
const MAX_INDEX_ROUNDS: usize = 1_000;

/// 构建本地搜索夹具时跳过的版本库元数据、依赖目录与构建产物。
const SKIPPED_DIRECTORIES: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".parcel-cache",
    "coverage",
    "vendor",
    "__pycache__",
    ".venv",
    "venv",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".gradle",
    ".idea",
    ".vscode",
    ".cache",
];

/// 命令行解析后的搜索选项。
#[derive(Debug)]
struct SearchOptions {
    /// 要索引和搜索的仓库根目录。
    root: PathBuf,
    /// 自然语言或代码标识符查询。
    query: String,
    /// 最多返回的结果数量。
    max_results: u64,
    /// 返回代码片段允许占用的 token 预算。
    token_budget: u64,
    /// 可选的仓库相对路径前缀。
    path_scope: Vec<String>,
}

/// 执行搜索命令；失败时打印包含上下文的错误并返回非零状态。
fn main() {
    if let Err(error) = run() {
        eprintln!("搜索失败：{error:#}");
        std::process::exit(1);
    }
}

/// 读取参数、建立全仓库索引并输出最终搜索结果。
///
/// # Errors
///
/// 参数非法、仓库读取失败、插件激活失败、索引未能完成或工具调用失败时返回错误。
fn run() -> Result<()> {
    let Some(options) = parse_options(std::env::args().skip(1))? else {
        print_help();
        return Ok(());
    };
    let root = options
        .root
        .canonicalize()
        .with_context(|| format!("无法访问仓库根目录 `{}`", options.root.display()))?;
    if !root.is_dir() {
        return Err(anyhow!("仓库根路径不是目录：{}", root.display()));
    }

    let files = collect_repository_files(&root)?;
    if files.is_empty() {
        return Err(anyhow!("仓库中没有可索引的受支持文件"));
    }
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&file_refs);
    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(&host, activation_context(&root.to_string_lossy()))
        .context("激活 Code Context 插件失败")?;

    eprintln!("正在索引 {} 个文件：{}", files.len(), root.display());
    let result = execute_search(&mut plugin, &host, &options)?;
    print_result(&plugin, &result);
    Ok(())
}

/// 解析搜索命令参数；请求帮助时返回 `Ok(None)`。
///
/// # Errors
///
/// 缺少选项值、数值格式错误、存在未知选项或查询为空时返回错误。
fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Option<SearchOptions>> {
    let mut root = default_repository_root();
    let mut max_results = 10;
    let mut token_budget = 8_000;
    let mut path_scope = Vec::new();
    let mut query_parts = Vec::new();
    let mut args = args.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => return Ok(None),
            "--root" => {
                root = PathBuf::from(next_value(&mut args, "--root")?);
            }
            "--max-results" => {
                max_results = parse_u64(next_value(&mut args, "--max-results")?, "--max-results")?;
            }
            "--token-budget" => {
                token_budget =
                    parse_u64(next_value(&mut args, "--token-budget")?, "--token-budget")?;
            }
            "--path-scope" => {
                path_scope.push(next_value(&mut args, "--path-scope")?);
            }
            unknown if unknown.starts_with('-') => {
                return Err(anyhow!("未知选项：{unknown}"));
            }
            query_part => query_parts.push(query_part.to_string()),
        }
    }

    let query = query_parts.join(" ").trim().to_string();
    if query.is_empty() {
        return Err(anyhow!("缺少搜索查询；使用 --help 查看用法"));
    }
    Ok(Some(SearchOptions {
        root,
        query,
        max_results,
        token_budget,
        path_scope,
    }))
}

/// 读取一个必须存在的选项值。
///
/// # Errors
///
/// 迭代器中没有后续参数时返回带选项名称的错误。
fn next_value(args: &mut impl Iterator<Item = String>, option: &str) -> Result<String> {
    args.next().ok_or_else(|| anyhow!("选项 {option} 缺少参数"))
}

/// 解析正整数选项。
///
/// # Errors
///
/// 值不是大于零的十进制整数时返回错误。
fn parse_u64(value: String, option: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .with_context(|| format!("选项 {option} 需要正整数，实际为 `{value}`"))?;
    if parsed == 0 {
        return Err(anyhow!("选项 {option} 必须大于零"));
    }
    Ok(parsed)
}

/// 从插件清单目录推导 Lucia 仓库根目录。
fn default_repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("插件目录必须位于 Lucia 仓库的 examples/plugins 下")
        .to_path_buf()
}

/// 递归读取仓库内全部受生产分类器支持的 UTF-8 文件。
///
/// 符号链接不会跟随，避免越过用户指定的仓库边界；依赖目录和构建产物在装入测试宿主前
/// 跳过，其余文件是否可索引由生产代码的 [`classify`] 统一决定。
///
/// # Errors
///
/// 任一需要访问的目录无法列举时返回错误；单个文件读取失败时打印警告并继续处理。
fn collect_repository_files(root: &Path) -> Result<Vec<(String, String)>> {
    let mut files = Vec::new();
    collect_recursive(root, root, 0, &mut files)?;
    files.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

/// 按稳定顺序递归收集单层目录。
///
/// `depth` 用于限制异常目录树；达到生产扫描器的 24 层边界后停止继续下探。
///
/// # Errors
///
/// 目录读取或目录项元数据读取失败时返回包含路径的错误。
fn collect_recursive(
    root: &Path,
    directory: &Path,
    depth: u32,
    output: &mut Vec<(String, String)>,
) -> Result<()> {
    if depth > 24 {
        return Ok(());
    }
    let mut entries = std::fs::read_dir(directory)
        .with_context(|| format!("无法列出目录 `{}`", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("读取目录项失败：{}", directory.display()))?;
    entries.sort_by_key(std::fs::DirEntry::file_name);

    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("读取文件类型失败：{}", entry.path().display()))?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if file_type.is_dir() {
            if SKIPPED_DIRECTORIES.contains(&name.as_ref()) {
                continue;
            }
            collect_recursive(root, &path, depth + 1, output)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .expect("递归目录项必须位于仓库根目录内")
            .to_string_lossy()
            .replace('\\', "/");
        if !matches!(classify(&relative), PathClass::Indexable(_)) {
            continue;
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => output.push((relative, content)),
            Err(error) => eprintln!("跳过无法读取的文件 {relative}：{error}"),
        }
    }
    Ok(())
}

/// 分片推进索引，完成后返回当前查询的最终工具结果。
///
/// # Errors
///
/// 工具调用失败、索引超过轮数上限或终态结果仍为错误时返回错误。
fn execute_search(
    plugin: &mut CodeContextPlugin,
    host: &RepoHost,
    options: &SearchOptions,
) -> Result<agent_plugin::ToolResult> {
    let arguments = json!({
        "query": options.query,
        "max_results": options.max_results,
        "token_budget": options.token_budget,
        "path_scope": options.path_scope,
    });

    for round in 1..=MAX_INDEX_ROUNDS {
        let result = plugin
            .call_tool_with_host(
                host,
                ToolCall::new(format!("search-{round}"), "code_search", arguments.clone()),
            )
            .context("调用 code_search 失败")?;
        if plugin.index_status() == IndexStatus::Indexing {
            if round == 1 || round % 10 == 0 {
                eprintln!("索引进行中：第 {round} 轮");
            }
            continue;
        }
        if result.is_error {
            return Err(anyhow!("code_search 返回错误：{}", result.content));
        }
        return Ok(result);
    }
    Err(anyhow!(
        "索引在 {MAX_INDEX_ROUNDS} 轮内未完成，当前状态为 {}",
        plugin.index_status().as_str()
    ))
}

/// 以适合终端阅读的格式输出索引统计和代码片段。
fn print_result(plugin: &CodeContextPlugin, result: &agent_plugin::ToolResult) {
    if let Some(stats) = plugin.index_stats() {
        println!(
            "索引完成：{} 个文件，{} 个代码块，{} 个符号",
            stats.files, stats.chunks, stats.symbols
        );
    }
    let Some(results) = result.content.get("results").and_then(Value::as_array) else {
        println!("未返回结构化搜索结果：{}", result.content);
        return;
    };
    if results.is_empty() {
        println!("未找到匹配结果");
        return;
    }

    for (index, item) in results.iter().enumerate() {
        let file = item
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or("未知文件");
        let lines = item
            .get("lines")
            .and_then(Value::as_array)
            .map(|range| {
                let start = range.first().and_then(Value::as_u64).unwrap_or(0);
                let end = range.get(1).and_then(Value::as_u64).unwrap_or(start);
                format!("{start}-{end}")
            })
            .unwrap_or_else(|| "未知行".to_string());
        let symbol = item
            .get("qualified_symbol")
            .or_else(|| item.get("symbol"))
            .and_then(Value::as_str)
            .map(|value| format!(" {value}"))
            .unwrap_or_default();
        println!("\n{}. {}:{}{}", index + 1, file, lines, symbol);
        if let Some(snippet) = item.get("snippet").and_then(Value::as_str) {
            println!("{snippet}");
        }
    }
}

/// 打印命令用法与可选参数。
fn print_help() {
    println!(
        "用法：\n  cargo run -p code-context-plugin --example search_repository --features test-support -- [选项] <查询>\n\n选项：\n  --root <路径>           仓库根目录，默认使用当前 Lucia 仓库\n  --max-results <数量>    最多返回结果数，默认 10\n  --token-budget <数量>   代码片段 token 预算，默认 8000\n  --path-scope <前缀>     限定仓库相对路径，可重复指定\n  -h, --help              显示帮助"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 查询词和全部选项必须按命令行语义解析。
    #[test]
    fn parses_query_and_options() {
        let options = parse_options(
            [
                "--root",
                "/repo",
                "--max-results",
                "5",
                "--token-budget",
                "2000",
                "--path-scope",
                "crates/agent-core/",
                "插件",
                "权限",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("参数应有效")
        .expect("不应进入帮助分支");

        assert_eq!(options.root, PathBuf::from("/repo"));
        assert_eq!(options.query, "插件 权限");
        assert_eq!(options.max_results, 5);
        assert_eq!(options.token_budget, 2_000);
        assert_eq!(options.path_scope, ["crates/agent-core/"]);
    }

    /// 缺少查询时必须返回明确错误，避免执行无意义的全仓库索引。
    #[test]
    fn rejects_missing_query() {
        let error = parse_options(std::iter::empty())
            .expect_err("缺少查询应失败")
            .to_string();
        assert!(error.contains("缺少搜索查询"));
    }
}

//! 当前仓库的可执行代码搜索入口。
//!
//! 该示例把 Lucia 仓库中全部受支持的文本与源码文件装入测试宿主，驱动
//! `CodeContextPlugin` 完成分片索引，再输出 `code_search` 返回的文件位置与代码片段。

use agent_plugin::{ActivationContext, AgentPlugin, HttpRequest, HttpResponse, ToolCall};
use anyhow::{anyhow, Context, Result};
use code_context_plugin::{
    classify, content_hash,
    testing::{activation_context, RepoHost},
    CodeContextPlugin, IndexStatus, PathClass,
};
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::HashMap;
use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::{self, IsTerminal, Write as IoWrite};
use std::path::{Path, PathBuf};
use url::Url;

/// 单次命令允许推进索引的最大轮数，避免异常状态造成无限循环。
const MAX_INDEX_ROUNDS: usize = 1_000;

/// 默认使用的 OpenAI 兼容嵌入端点。
const DEFAULT_EMBEDDING_BASE_URL: &str = "https://api.siliconflow.cn/v1/";

/// 默认读取 API 密钥的环境变量名。
const DEFAULT_EMBEDDING_API_KEY_ENV: &str = "SILICONFLOW_API_KEY";

/// 默认嵌入模型。
const DEFAULT_EMBEDDING_MODEL: &str = "BAAI/bge-m3";

/// 默认嵌入维度。
const DEFAULT_EMBEDDING_DIMENSION: u32 = 1_024;

/// 默认单批嵌入代码块数量。
const DEFAULT_EMBEDDING_BATCH_SIZE: usize = 32;

/// 默认缓存目录在用户主目录中的相对路径。
const DEFAULT_CACHE_SUBDIRECTORY: &str = ".lucia/code-context";

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
    /// 可选的首次查询；未提供时在索引完成后进入交互搜索。
    query: Option<String>,
    /// 最多返回的结果数量。
    max_results: u64,
    /// 返回代码片段允许占用的 token 预算。
    token_budget: u64,
    /// 可选的仓库相对路径前缀。
    path_scope: Vec<String>,
    /// 保存跨进程索引快照的目录。
    cache_dir: PathBuf,
    /// 是否启用磁盘缓存。
    cache_enabled: bool,
    /// 启动前是否删除当前仓库的旧缓存。
    rebuild_cache: bool,
    /// 语义嵌入配置；`None` 表示用户显式要求只运行符号与 BM25 检索。
    embedding: Option<EmbeddingOptions>,
}

/// 全仓库语义向量化使用的端点配置。
#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbeddingOptions {
    /// OpenAI 兼容端点的 base URL。
    base_url: String,
    /// 保存 API 密钥的环境变量名，密钥本身不进入参数或日志。
    api_key_env: String,
    /// 嵌入模型名称。
    model: String,
    /// 服务端返回的向量维度。
    dimension: u32,
    /// 每次 API 请求提交的代码块数量。
    batch_size: usize,
    /// 是否向端点发送 `dimensions` 字段。
    send_dimensions: bool,
}

impl Default for EmbeddingOptions {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_EMBEDDING_BASE_URL.to_string(),
            api_key_env: DEFAULT_EMBEDDING_API_KEY_ENV.to_string(),
            model: DEFAULT_EMBEDDING_MODEL.to_string(),
            dimension: DEFAULT_EMBEDDING_DIMENSION,
            batch_size: DEFAULT_EMBEDDING_BATCH_SIZE,
            send_dimensions: false,
        }
    }
}

/// 示例程序使用的受限嵌入 HTTP 客户端。
///
/// API 密钥只保存在该宿主侧对象中，不会写入激活元数据、插件请求或终端输出。
struct EmbeddingEndpoint {
    /// 已规范化且保留路径前缀的 base URL。
    base_url: Url,
    /// 由环境变量读取的 bearer 凭证。
    api_key: String,
    /// 复用连接池的同步 HTTP 客户端。
    agent: ureq::Agent,
    /// 已提交的嵌入批次数，用于输出长任务进度。
    batches: Cell<usize>,
    /// 已提交的文档或查询总数。
    inputs: Cell<usize>,
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
    let mut context = activation_context(&root.to_string_lossy());
    let cache_path = repository_cache_path(&options.cache_dir, &root);
    if options.rebuild_cache {
        match fs::remove_file(&cache_path) {
            Ok(()) => eprintln!("已删除旧索引缓存：{}", cache_path.display()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow!(
                    "删除旧索引缓存 `{}` 失败：{error}",
                    cache_path.display()
                ))
            }
        }
    }
    let host = if options.cache_enabled {
        let existed = cache_path.is_file();
        eprintln!(
            "索引缓存：{}（{}）",
            cache_path.display(),
            if existed {
                "将复用已有数据"
            } else {
                "首次创建"
            }
        );
        RepoHost::persisted(&file_refs, &cache_path).with_context(|| {
            format!(
                "打开索引缓存 `{}` 失败；可使用 --rebuild-cache 重建",
                cache_path.display()
            )
        })?
    } else {
        eprintln!("已禁用索引缓存，本次退出后不会复用语义向量");
        RepoHost::new(&file_refs)
    };
    let host = match options.embedding.as_ref() {
        Some(embedding) => {
            let endpoint = EmbeddingEndpoint::new(embedding)?;
            configure_semantic_index(&mut context, embedding, endpoint.endpoint_id());
            eprintln!(
                "将先为整个仓库生成语义向量：模型 {}，维度 {}，批大小 {}",
                embedding.model, embedding.dimension, embedding.batch_size
            );
            host.with_http_handler(move |request| endpoint.send(request))
        }
        None => {
            eprintln!("已启用纯词法模式：只建立符号索引与 BM25 索引");
            host
        }
    };
    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(&host, context)
        .context("激活 Code Context 插件失败")?;

    eprintln!("正在索引 {} 个文件：{}", files.len(), root.display());
    let bootstrap_query = options.query.as_deref().unwrap_or("初始化代码索引");
    let result = execute_search(&mut plugin, &host, &options, bootstrap_query, 0)?;
    if options.embedding.is_some() && plugin.index_status() != IndexStatus::Ready {
        let reason = result
            .content
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or("未返回降级原因");
        return Err(anyhow!("全仓库向量化未完成：{reason}"));
    }
    print_index_stats(&plugin);
    match options.query.as_deref() {
        Some(_) => print_search_result(&result),
        None => run_interactive_search(&mut plugin, &host, &options)?,
    }
    Ok(())
}

/// 解析搜索命令参数；请求帮助时返回 `Ok(None)`。
///
/// # Errors
///
/// 缺少选项值、数值格式错误或存在未知选项时返回错误。查询可以省略；省略后进入交互模式。
fn parse_options(args: impl IntoIterator<Item = String>) -> Result<Option<SearchOptions>> {
    let mut root = default_repository_root();
    let mut max_results = 10;
    let mut token_budget = 8_000;
    let mut path_scope = Vec::new();
    let mut embedding = EmbeddingOptions::default();
    let mut cache_dir = default_cache_dir();
    let mut cache_enabled = true;
    let mut rebuild_cache = false;
    let mut lexical_only = false;
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
            "--cache-dir" => {
                cache_dir = PathBuf::from(next_value(&mut args, "--cache-dir")?);
            }
            "--no-cache" => {
                cache_enabled = false;
            }
            "--rebuild-cache" => {
                rebuild_cache = true;
            }
            "--lexical-only" => {
                lexical_only = true;
            }
            "--embedding-base-url" => {
                embedding.base_url = next_value(&mut args, "--embedding-base-url")?;
            }
            "--embedding-api-key-env" => {
                embedding.api_key_env = next_value(&mut args, "--embedding-api-key-env")?;
            }
            "--embedding-model" => {
                embedding.model = next_value(&mut args, "--embedding-model")?;
            }
            "--embedding-dimension" => {
                embedding.dimension = parse_u32(
                    next_value(&mut args, "--embedding-dimension")?,
                    "--embedding-dimension",
                )?;
            }
            "--embedding-batch-size" => {
                embedding.batch_size = parse_usize(
                    next_value(&mut args, "--embedding-batch-size")?,
                    "--embedding-batch-size",
                )?;
            }
            "--embedding-send-dimensions" => {
                embedding.send_dimensions = parse_bool(
                    next_value(&mut args, "--embedding-send-dimensions")?,
                    "--embedding-send-dimensions",
                )?;
            }
            unknown if unknown.starts_with('-') => {
                return Err(anyhow!("未知选项：{unknown}"));
            }
            query_part => query_parts.push(query_part.to_string()),
        }
    }

    if rebuild_cache && !cache_enabled {
        return Err(anyhow!("--rebuild-cache 不能与 --no-cache 同时使用"));
    }

    let query = query_parts.join(" ").trim().to_string();
    Ok(Some(SearchOptions {
        root,
        query: (!query.is_empty()).then_some(query),
        max_results,
        token_budget,
        path_scope,
        cache_dir,
        cache_enabled,
        rebuild_cache,
        embedding: (!lexical_only).then_some(embedding),
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

/// 解析正 `u32` 选项。
///
/// # Errors
///
/// 值不是 `u32` 范围内的大于零十进制整数时返回错误。
fn parse_u32(value: String, option: &str) -> Result<u32> {
    let parsed = parse_u64(value, option)?;
    u32::try_from(parsed).with_context(|| format!("选项 {option} 超出 u32 范围：{parsed}"))
}

/// 解析正 `usize` 选项。
///
/// # Errors
///
/// 值不是当前平台 `usize` 范围内的大于零十进制整数时返回错误。
fn parse_usize(value: String, option: &str) -> Result<usize> {
    let parsed = parse_u64(value, option)?;
    usize::try_from(parsed).with_context(|| format!("选项 {option} 超出 usize 范围：{parsed}"))
}

/// 解析严格布尔选项。
///
/// # Errors
///
/// 值不是 `true` 或 `false` 时返回错误，避免拼写错误被静默纠正。
fn parse_bool(value: String, option: &str) -> Result<bool> {
    match value.as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(anyhow!(
            "选项 {option} 只能是 `true` 或 `false`，实际为 `{value}`"
        )),
    }
}

impl EmbeddingEndpoint {
    /// 从命令行配置创建受限端点客户端。
    ///
    /// # Errors
    ///
    /// base URL 非法、包含内嵌凭证，或密钥环境变量缺失时返回错误。密钥值不会进入错误
    /// 信息，避免终端记录泄漏凭证。
    fn new(options: &EmbeddingOptions) -> Result<Self> {
        let mut base_url = Url::parse(options.base_url.trim())
            .with_context(|| format!("嵌入 base URL 无效：{}", options.base_url))?;
        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(anyhow!(
                "嵌入 base URL 只支持 http/https，实际为 `{}`",
                base_url.scheme()
            ));
        }
        if base_url.host_str().is_none() {
            return Err(anyhow!("嵌入 base URL 缺少主机名"));
        }
        if !base_url.username().is_empty() || base_url.password().is_some() {
            return Err(anyhow!("嵌入 base URL 不得内嵌用户名或密码"));
        }
        if !base_url.path().ends_with('/') {
            base_url.set_path(&format!("{}/", base_url.path()));
        }

        let api_key_env = options.api_key_env.trim();
        if api_key_env.is_empty() {
            return Err(anyhow!("嵌入 API 密钥环境变量名不能为空"));
        }
        let api_key = std::env::var(api_key_env)
            .with_context(|| format!("未设置嵌入 API 密钥环境变量 `{api_key_env}`"))?;
        if api_key.trim().is_empty() {
            return Err(anyhow!("嵌入 API 密钥环境变量 `{api_key_env}` 的值为空"));
        }

        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .http_status_as_error(false)
                .max_redirects(0)
                .timeout_global(Some(std::time::Duration::from_secs(120)))
                .build(),
        );
        Ok(Self {
            base_url,
            api_key,
            agent,
            batches: Cell::new(0),
            inputs: Cell::new(0),
        })
    }

    /// 返回不暴露地址的稳定端点摘要，供插件判断嵌入空间兼容性。
    fn endpoint_id(&self) -> String {
        format!("search-example:{}", content_hash(self.base_url.as_str()))
    }

    /// 转发一条 Guest 嵌入请求并注入宿主侧凭证。
    ///
    /// # Errors
    ///
    /// 请求不是 POST、路径可能逃离 base URL、携带敏感 header、网络失败或响应超过限制时
    /// 返回错误。非 2xx 响应仍作为 [`HttpResponse`] 返回，由嵌入 provider 解释正文。
    fn send(&self, request: &HttpRequest) -> agent_plugin::Result<HttpResponse> {
        if !request.method.eq_ignore_ascii_case("POST") {
            return Err(anyhow!("嵌入示例只允许 POST，收到 {}", request.method));
        }
        validate_relative_path(&request.path)?;
        let url = self
            .base_url
            .join(request.path.trim_start_matches('/'))
            .map_err(|error| anyhow!("拼接嵌入请求路径失败：{error}"))?;

        let mut builder = self.agent.post(url.as_str());
        for (name, value) in &request.headers {
            if is_sensitive_header(name) {
                return Err(anyhow!("插件请求不得自行设置敏感 header `{name}`"));
            }
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder = builder.header("authorization", &format!("Bearer {}", self.api_key));

        let input_count = request
            .body
            .as_deref()
            .and_then(|body| serde_json::from_str::<Value>(body).ok())
            .and_then(|body| body.get("input").and_then(Value::as_array).map(Vec::len))
            .unwrap_or(0);
        let batch = self.batches.get() + 1;
        let inputs = self.inputs.get() + input_count;
        self.batches.set(batch);
        self.inputs.set(inputs);
        if batch == 1 || batch % 10 == 0 {
            eprintln!("正在生成语义向量：已提交 {batch} 批、{inputs} 条输入");
        }

        let response = match request.body.as_deref() {
            Some(body) => builder.send(body.as_bytes()),
            None => builder.send(&[] as &[u8]),
        }
        .map_err(|error| anyhow!("嵌入 HTTP 请求失败：{error}"))?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .into_with_config()
            .limit(10 * 1024 * 1024)
            .read_to_string()
            .map_err(|error| anyhow!("读取嵌入响应失败：{error}"))?;
        Ok(HttpResponse {
            status,
            headers: HashMap::new(),
            body,
        })
    }
}

/// 校验 Guest 路径不会覆盖宿主配置的目标地址与路径前缀。
///
/// # Errors
///
/// 绝对 URL、协议相对地址、父目录穿越、空路径或控制字符会被拒绝。
fn validate_relative_path(path: &str) -> Result<()> {
    let path = path.trim();
    if path.is_empty() {
        return Err(anyhow!("嵌入请求路径不能为空"));
    }
    if path.contains("://") || path.starts_with("//") {
        return Err(anyhow!("嵌入请求必须使用相对路径"));
    }
    if path.chars().any(char::is_whitespace) {
        return Err(anyhow!("嵌入请求路径不得包含空白或控制字符"));
    }
    let path_only = path.split('?').next().unwrap_or(path);
    if path_only.split('/').any(|segment| segment == "..") {
        return Err(anyhow!("嵌入请求路径不得包含父目录穿越"));
    }
    Ok(())
}

/// 判断插件请求是否试图自行提交宿主应掌握的敏感 header。
fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "host"
    )
}

/// 把端点与嵌入空间参数写入激活元数据。
///
/// `endpoint_id` 是 base URL 的摘要，不包含 API 密钥；插件用它判断缓存向量是否仍可比较。
fn configure_semantic_index(
    context: &mut ActivationContext,
    options: &EmbeddingOptions,
    endpoint_id: String,
) {
    context
        .metadata
        .insert("embedding_mode".to_string(), "api".to_string());
    context
        .metadata
        .insert("embedding_model".to_string(), options.model.clone());
    context.metadata.insert(
        "embedding_dimension".to_string(),
        options.dimension.to_string(),
    );
    context.metadata.insert(
        "embedding_batch_size".to_string(),
        options.batch_size.to_string(),
    );
    context.metadata.insert(
        "embedding_send_dimensions".to_string(),
        options.send_dimensions.to_string(),
    );
    context
        .metadata
        .insert("http_endpoint_id".to_string(), endpoint_id);
}

/// 从插件清单目录推导 Lucia 仓库根目录。
fn default_repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("插件目录必须位于 Lucia 仓库的 examples/plugins 下")
        .to_path_buf()
}

/// 返回跨进程索引默认缓存目录 `~/.lucia/code-context`。
///
/// 极简运行环境没有主目录信息时退回系统临时目录，避免仅因缓存不可寻址阻止搜索。
fn default_cache_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(DEFAULT_CACHE_SUBDIRECTORY)
}

/// 按规范化仓库根路径派生稳定且不泄露本地路径的缓存文件名。
fn repository_cache_path(cache_dir: &Path, root: &Path) -> PathBuf {
    let identity = content_hash(&format!(
        "code-context-search-repository\0{}",
        root.to_string_lossy()
    ));
    cache_dir.join(format!("{identity}.json"))
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
    query: &str,
    sequence: usize,
) -> Result<agent_plugin::ToolResult> {
    let arguments = json!({
        "query": query,
        "max_results": options.max_results,
        "token_budget": options.token_budget,
        "path_scope": options.path_scope,
    });

    for round in 1..=MAX_INDEX_ROUNDS {
        let result = plugin
            .call_tool_with_host(
                host,
                ToolCall::new(
                    format!("search-{sequence}-{round}"),
                    "code_search",
                    arguments.clone(),
                ),
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

/// 索引完成后进入终端交互循环，复用同一份全仓库向量索引连续搜索。
///
/// # Errors
///
/// 读取标准输入、刷新提示符或调用搜索工具失败时返回错误。
fn run_interactive_search(
    plugin: &mut CodeContextPlugin,
    host: &RepoHost,
    options: &SearchOptions,
) -> Result<()> {
    println!("输入要搜索的内容，输入 exit、quit 或 :q 结束。");
    let stdin = io::stdin();
    let mut sequence = 1;
    loop {
        print!("搜索> ");
        io::stdout().flush().context("刷新搜索提示符失败")?;

        let mut query = String::new();
        if stdin.read_line(&mut query).context("读取搜索内容失败")? == 0 {
            println!();
            return Ok(());
        }
        let query = query.trim();
        if query.is_empty() {
            continue;
        }
        if matches!(query, "exit" | "quit" | ":q") {
            return Ok(());
        }

        let result = execute_search(plugin, host, options, query, sequence)?;
        print_search_result(&result);
        sequence += 1;
    }
}

/// 输出已经建立的全仓库索引规模。
fn print_index_stats(plugin: &CodeContextPlugin) {
    if let Some(stats) = plugin.index_stats() {
        println!(
            "索引完成：{} 个文件，{} 个代码块，{} 个符号",
            stats.files, stats.chunks, stats.symbols
        );
    }
}

/// 以适合终端阅读的格式输出一次搜索返回的代码片段。
fn print_search_result(result: &agent_plugin::ToolResult) {
    print!(
        "{}",
        format_search_result(result, terminal_colors_enabled())
    );
}

/// 把结构化工具结果格式化为带分组、元数据和源码行号的终端文本。
///
/// `colors_enabled` 只控制 ANSI 样式，不改变可见文字，便于重定向输出和单元测试稳定比较。
fn format_search_result(result: &agent_plugin::ToolResult, colors_enabled: bool) -> String {
    let Some(results) = result.content.get("results").and_then(Value::as_array) else {
        return format!("搜索结果格式异常\n{}\n", result.content);
    };
    let mut output = String::new();
    let stats = result
        .details
        .as_ref()
        .and_then(|details| details.get("stats"));
    let mut summary = vec![format!("{} 条结果", results.len())];
    if let Some(candidates) = stats
        .and_then(|value| value.get("candidates"))
        .and_then(Value::as_u64)
    {
        summary.push(format!("{candidates} 个候选"));
    }
    if let Some(tokens) = stats
        .and_then(|value| value.get("estimated_tokens"))
        .and_then(Value::as_u64)
    {
        summary.push(format!("约 {tokens} tokens"));
    }
    let heading = paint(
        &format!("搜索结果  {}", summary.join(" · ")),
        "1;36",
        colors_enabled,
    );
    writeln!(output, "\n{heading}").expect("写入 String 不会失败");

    if results.is_empty() {
        writeln!(output, "未找到匹配结果").expect("写入 String 不会失败");
        return output;
    }
    if let Some(note) = result.content.get("note").and_then(Value::as_str) {
        writeln!(output, "{} {note}", paint("提示：", "1;33", colors_enabled))
            .expect("写入 String 不会失败");
    }

    for (index, item) in results.iter().enumerate() {
        let file = item
            .get("file")
            .and_then(Value::as_str)
            .unwrap_or("未知文件");
        let (start_line, end_line) = item
            .get("lines")
            .and_then(Value::as_array)
            .map(|range| {
                let start = range.first().and_then(Value::as_u64).unwrap_or(0);
                let end = range.get(1).and_then(Value::as_u64).unwrap_or(start);
                (start, end)
            })
            .unwrap_or((0, 0));
        let line_range = if start_line == 0 {
            "未知行".to_string()
        } else if start_line == end_line {
            start_line.to_string()
        } else {
            format!("{start_line}-{end_line}")
        };
        let symbol = item
            .get("qualified_symbol")
            .or_else(|| item.get("symbol"))
            .and_then(Value::as_str)
            .unwrap_or("文本片段");
        let mut metadata = Vec::new();
        if let Some(language) = item.get("language").and_then(Value::as_str) {
            metadata.push(language.to_string());
        }
        if let Some(kind) = item.get("kind").and_then(Value::as_str) {
            metadata.push(kind.to_string());
        }
        if item.get("relocated").and_then(Value::as_bool) == Some(true) {
            metadata.push("位置已重定位".to_string());
        }
        if item.get("truncated").and_then(Value::as_bool) == Some(true) {
            metadata.push("片段已截断".to_string());
        }

        let rank = paint(&format!("[{}]", index + 1), "1;32", colors_enabled);
        let symbol = paint(symbol, "1;33", colors_enabled);
        writeln!(
            output,
            "{} {rank} {symbol}",
            paint("╭─", "2", colors_enabled)
        )
        .expect("写入 String 不会失败");
        let location = paint(&format!("{file}:{line_range}"), "36", colors_enabled);
        if metadata.is_empty() {
            writeln!(output, "{} {location}", paint("│ ", "2", colors_enabled))
                .expect("写入 String 不会失败");
        } else {
            writeln!(
                output,
                "{} {location}  {}",
                paint("│ ", "2", colors_enabled),
                paint(&metadata.join(" · "), "2", colors_enabled)
            )
            .expect("写入 String 不会失败");
        }
        if let Some(signature) = item
            .get("signature")
            .and_then(Value::as_str)
            .and_then(|value| format_signature(value, item.get("language").and_then(Value::as_str)))
        {
            writeln!(
                output,
                "{} {} {signature}",
                paint("│ ", "2", colors_enabled),
                paint("签名", "1;35", colors_enabled)
            )
            .expect("写入 String 不会失败");
        }
        writeln!(
            output,
            "{}",
            paint(
                "├────────────────────────────────────────────────────────────────────────",
                "2",
                colors_enabled
            )
        )
        .expect("写入 String 不会失败");
        if let Some(snippet) = item.get("snippet").and_then(Value::as_str) {
            let number_width = end_line.max(start_line).to_string().len().max(1);
            for (offset, source) in snippet.lines().enumerate() {
                let source_line = start_line.saturating_add(offset as u64);
                let number = if start_line == 0 {
                    " ".repeat(number_width)
                } else {
                    format!("{source_line:>number_width$}")
                };
                writeln!(
                    output,
                    "{} {} {} {source}",
                    paint("│", "2", colors_enabled),
                    paint(&number, "2", colors_enabled),
                    paint("│", "2", colors_enabled)
                )
                .expect("写入 String 不会失败");
            }
        }
        writeln!(
            output,
            "{}\n",
            paint(
                "╰────────────────────────────────────────────────────────────────────────",
                "2",
                colors_enabled
            )
        )
        .expect("写入 String 不会失败");
    }

    output
}

/// 判断当前标准输出是否适合使用 ANSI 颜色。
///
/// 重定向到文件或设置 `NO_COLOR` 时返回 `false`，避免把控制字符写进日志和测试快照。
fn terminal_colors_enabled() -> bool {
    io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// 按需为一段终端文本添加 ANSI SGR 样式。
fn paint(text: &str, style: &str, enabled: bool) -> String {
    if enabled {
        format!("\x1b[{style}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// 清理并限制用于终端展示的声明签名。
///
/// Rust 解析结果可能把前导文档注释与属性折叠进签名；展示层只保留真正的声明，并把连续
/// 空白压成一个空格。超出 160 个字符时按字符边界截断，避免长泛型签名撑破终端布局。
fn format_signature(signature: &str, language: Option<&str>) -> Option<String> {
    let compact = signature.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return None;
    }
    let declaration = if language == Some("rust") {
        rust_declaration_start(&compact)
            .map(|start| compact[start..].trim())
            .unwrap_or(compact.as_str())
    } else {
        compact.as_str()
    };
    const MAX_VISIBLE_CHARS: usize = 160;
    if declaration.chars().count() <= MAX_VISIBLE_CHARS {
        return Some(declaration.to_string());
    }
    let mut shortened: String = declaration.chars().take(MAX_VISIBLE_CHARS - 1).collect();
    shortened.push('…');
    Some(shortened)
}

/// 定位折叠后 Rust 签名中的实际声明起点，跳过 Rustdoc 与 `#[...]` 属性。
fn rust_declaration_start(signature: &str) -> Option<usize> {
    const MARKERS: &[&str] = &[
        "pub(crate) ",
        "pub(super) ",
        "pub(self) ",
        "pub(in ",
        "pub ",
        "async ",
        "unsafe ",
        "extern ",
        "const ",
        "static ",
        "struct ",
        "enum ",
        "trait ",
        "impl ",
        "type ",
        "fn ",
        "mod ",
        "use ",
        "macro_rules! ",
    ];
    MARKERS
        .iter()
        .filter_map(|marker| signature.find(marker))
        .min()
}

/// 打印命令用法与可选参数。
fn print_help() {
    println!(
        "用法：\n  cargo run -p code-context-plugin --example search_repository --features test-support -- [选项] [查询]\n\n未提供查询时，索引完成后进入交互搜索，可连续输入多条查询。语义向量默认按仓库持久化，第二次运行只计算新增或变化的代码块。\n\n选项：\n  --root <路径>                         仓库根目录，默认使用当前 Lucia 仓库\n  --max-results <数量>                  最多返回结果数，默认 10\n  --token-budget <数量>                 代码片段 token 预算，默认 8000\n  --path-scope <前缀>                   限定仓库相对路径，可重复指定\n  --cache-dir <目录>                    索引缓存目录，默认 ~/.lucia/code-context\n  --no-cache                            不读取或写入跨进程索引缓存\n  --rebuild-cache                       删除当前仓库缓存后重新生成全部向量\n  --embedding-base-url <URL>            OpenAI 兼容端点，默认使用 SiliconFlow\n  --embedding-api-key-env <变量名>      API 密钥环境变量，默认 SILICONFLOW_API_KEY\n  --embedding-model <模型>              嵌入模型，默认 BAAI/bge-m3\n  --embedding-dimension <维度>          向量维度，默认 1024\n  --embedding-batch-size <数量>         单批代码块数，默认 32\n  --embedding-send-dimensions <布尔值>  是否发送 dimensions，默认 false\n  --lexical-only                        跳过向量化，只运行符号与 BM25 检索\n  -h, --help                            显示帮助"
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
                "--cache-dir",
                "/tmp/code-context-cache",
                "--rebuild-cache",
                "插件",
                "权限",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("参数应有效")
        .expect("不应进入帮助分支");

        assert_eq!(options.root, PathBuf::from("/repo"));
        assert_eq!(options.query.as_deref(), Some("插件 权限"));
        assert_eq!(options.max_results, 5);
        assert_eq!(options.token_budget, 2_000);
        assert_eq!(options.path_scope, ["crates/agent-core/"]);
        assert_eq!(options.cache_dir, PathBuf::from("/tmp/code-context-cache"));
        assert!(options.cache_enabled);
        assert!(options.rebuild_cache);
        assert_eq!(options.embedding, Some(EmbeddingOptions::default()));
    }

    /// 嵌入端点选项必须覆盖默认值，并保持密钥只通过环境变量引用。
    #[test]
    fn parses_embedding_options() {
        let options = parse_options(
            [
                "--embedding-base-url",
                "https://embed.example.com/v1/",
                "--embedding-api-key-env",
                "TEST_EMBEDDING_KEY",
                "--embedding-model",
                "embed-code",
                "--embedding-dimension",
                "768",
                "--embedding-batch-size",
                "16",
                "--embedding-send-dimensions",
                "true",
                "权限检查",
            ]
            .into_iter()
            .map(str::to_string),
        )
        .expect("参数应有效")
        .expect("不应进入帮助分支");

        assert_eq!(
            options.embedding,
            Some(EmbeddingOptions {
                base_url: "https://embed.example.com/v1/".to_string(),
                api_key_env: "TEST_EMBEDDING_KEY".to_string(),
                model: "embed-code".to_string(),
                dimension: 768,
                batch_size: 16,
                send_dimensions: true,
            })
        );
    }

    /// 纯词法开关必须显式关闭语义向量化，便于无网络环境单独验证基础检索。
    #[test]
    fn lexical_only_disables_embedding() {
        let options = parse_options(
            ["--lexical-only", "PluginHostApi"]
                .into_iter()
                .map(str::to_string),
        )
        .expect("参数应有效")
        .expect("不应进入帮助分支");

        assert!(options.embedding.is_none());
    }

    /// 相对路径校验必须阻止端点逃逸与父目录穿越。
    #[test]
    fn rejects_unsafe_embedding_paths() {
        assert!(validate_relative_path("embeddings").is_ok());
        assert!(validate_relative_path("https://evil.example/steal").is_err());
        assert!(validate_relative_path("../admin").is_err());
        assert!(validate_relative_path("embed\r\nheader").is_err());
    }

    /// 缺少查询时必须进入交互模式，让用户在索引完成后自行输入搜索内容。
    #[test]
    fn missing_query_enables_interactive_mode() {
        let options = parse_options(std::iter::empty())
            .expect("空参数应进入交互模式")
            .expect("不应进入帮助分支");

        assert!(options.query.is_none());
        assert_eq!(options.embedding, Some(EmbeddingOptions::default()));
        assert!(options.cache_enabled);
        assert!(!options.rebuild_cache);
    }

    /// 禁用缓存时不得同时请求删除缓存，避免选项语义互相矛盾。
    #[test]
    fn no_cache_rejects_rebuild_cache() {
        let error = parse_options(
            ["--no-cache", "--rebuild-cache"]
                .into_iter()
                .map(str::to_string),
        )
        .expect_err("冲突选项应失败");

        assert!(error.to_string().contains("不能与"));
    }

    /// 同一仓库必须得到稳定缓存文件，不同仓库不得共享缓存。
    #[test]
    fn repository_cache_path_is_stable_and_isolated() {
        let cache = Path::new("/cache");
        let first = repository_cache_path(cache, Path::new("/repo/a"));

        assert_eq!(first, repository_cache_path(cache, Path::new("/repo/a")));
        assert_ne!(first, repository_cache_path(cache, Path::new("/repo/b")));
        assert_eq!(first.parent(), Some(cache));
    }

    /// 搜索结果必须突出汇总、位置、符号、签名和带行号的源码，且纯文本模式不含 ANSI 控制符。
    #[test]
    fn search_result_format_is_grouped_and_readable() {
        let result = agent_plugin::ToolResult::success(
            "call-1",
            "code_search",
            json!({
                "status": "ready",
                "results": [{
                    "file": "crates/agent-tui/src/ui/plugin_render.rs",
                    "lines": [205, 206],
                    "language": "rust",
                    "symbol": "render_plugin_dialog",
                    "qualified_symbol": "render_plugin_dialog",
                    "kind": "function",
                    "signature": "pub(crate) fn render_plugin_dialog(...) ",
                    "snippet": "/// 渲染插件对话框。\npub(crate) fn render_plugin_dialog() {}",
                    "relocated": true,
                    "truncated": true
                }]
            }),
        )
        .with_details(json!({
            "stats": {
                "candidates": 24,
                "estimated_tokens": 128
            }
        }));

        let rendered = format_search_result(&result, false);

        assert!(rendered.contains("搜索结果  1 条结果 · 24 个候选 · 约 128 tokens"));
        assert!(rendered.contains("╭─ [1] render_plugin_dialog"));
        assert!(rendered
            .contains("plugin_render.rs:205-206  rust · function · 位置已重定位 · 片段已截断"));
        assert!(rendered.contains("签名 pub(crate) fn render_plugin_dialog(...)"));
        assert!(rendered.contains("│ 205 │ /// 渲染插件对话框。"));
        assert!(rendered.contains("│ 206 │ pub(crate) fn render_plugin_dialog() {}"));
        assert!(!rendered.contains('\x1b'));
    }

    /// 空结果也必须显示本次返回数量，不能只留下没有上下文的提示。
    #[test]
    fn empty_search_result_has_clear_summary() {
        let result = agent_plugin::ToolResult::success(
            "call-1",
            "code_search",
            json!({"status": "ready", "results": []}),
        );

        let rendered = format_search_result(&result, false);

        assert!(rendered.contains("搜索结果  0 条结果"));
        assert!(rendered.contains("未找到匹配结果"));
    }

    /// Rust 签名展示必须去掉重复的文档注释和测试属性，并限制超长文本。
    #[test]
    fn rust_signature_display_removes_prefixes_and_truncates() {
        let cleaned = format_signature(
            "/// 验证对话框。 #[test] #[cfg(feature = \"plugins\")] fn plugin_dialog_is_modal()",
            Some("rust"),
        );
        assert_eq!(cleaned.as_deref(), Some("fn plugin_dialog_is_modal()"));

        let long = format!("pub fn generated({})", "parameter: String, ".repeat(20));
        let shortened = format_signature(&long, Some("rust")).expect("长签名应仍可展示");
        assert_eq!(shortened.chars().count(), 160);
        assert!(shortened.ends_with('…'));
    }
}

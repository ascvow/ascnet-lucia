//! 代码检索试验场：拿你自己的代码试排序，并看清"为什么是这个排名"。
//!
//! 与 `search_repository` 的分工：那个示例面向"在真实仓库里找代码"，输出的是结果本身；
//! 这个示例默认展示 Agent 工具结果；`--diagnostics` 额外展开检索链路——查询意图、检索目标、
//! 实际执行的通道与配额、走的哪条延迟路径、是否触发了模型重排，以及每条结果的通道来源、
//! 代码角色与分数构成。
//!
//! # 用法
//!
//! ```sh
//! # 1) 索引整个 Lucia 仓库并进入交互检索
//! cargo run -p code-context-plugin --features test-support --example search_playground
//!
//! # 2) 换成别的项目
//! cargo run -p code-context-plugin --features test-support --example search_playground -- \
//!     --root ~/my-project
//!
//! # 3) 开启语义检索（需要 SILICONFLOW_API_KEY；不加则只跑符号 + 词法，完全离线）
//! cargo run -p code-context-plugin --features test-support --example search_playground -- \
//!     --semantic
//!
//! # 4) 一次性查询，不进入交互模式
//! cargo run -p code-context-plugin --features test-support --example search_playground -- \
//!     "权限检查在哪里实现"
//! ```
//!
//! 交互模式下输入 `exit`、`quit` 或 `:q` 退出。
//! 默认持久化缓存以复用 API 向量；源码是启动时的快照，修改源码后请重启示例。
//! 模型只通过 SiliconFlow API 调用，不加载本地模型。`--exclude` 可排除调试或第三方目录。

use agent_plugin::{AgentPlugin, HttpRequest, HttpResponse, ToolCall};
use anyhow::{anyhow, Context, Result};
use code_context_plugin::{
    testing::{activation_context, repository_root, RepoHost},
    CandidateSource, CodeContextPlugin, IndexStatus, SearchExecution,
};
use serde_json::json;
use std::collections::HashMap;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// 默认返回的结果条数。
const DEFAULT_LIMIT: usize = 8;

/// 每条结果展示的最大源码行数。
const PREVIEW_LINES: usize = 12;

/// 语义检索使用的端点与模型。
const EMBEDDING_BASE_URL: &str = "https://api.siliconflow.cn/v1/";
const EMBEDDING_MODEL: &str = "BAAI/bge-m3";
const EMBEDDING_DIMENSION: u32 = 1024;
const RERANK_MODEL: &str = "Qwen/Qwen3-Reranker-4B";
const API_KEY_ENV: &str = "SILICONFLOW_API_KEY";

/// 命令行选项。
struct Options {
    /// 被索引的目录。
    root: PathBuf,
    /// 可选的一次性查询；为空则进入交互模式。
    query: Option<String>,
    /// 返回条数。
    limit: usize,
    /// 是否启用语义通道。
    semantic: bool,
    /// 是否允许模型重排；仅在启用语义时有意义。
    rerank: bool,
    /// 跨进程元数据及向量缓存目录；为空时禁用持久缓存。
    cache_dir: Option<PathBuf>,
    /// Agent 实际输出的 token 预算。
    token_budget: u64,
    /// 显式开启额外检索诊断，可能多一次重排调用。
    diagnostics: bool,
    /// 不扫描的仓库相对路径前缀。
    excludes: Vec<String>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("试验场启动失败：{error:#}");
        std::process::exit(1);
    }
}

/// 解析参数、建立索引，然后进入查询循环。
///
/// # Errors
///
/// 参数非法、目录不可读、插件激活失败或索引未能完成时返回错误。
fn run() -> Result<()> {
    let options = parse_options()?;
    let root = options
        .root
        .canonicalize()
        .with_context(|| format!("无法访问目录 `{}`", options.root.display()))?;

    let files = collect_files(&root, &options.excludes)?;
    if files.is_empty() {
        return Err(anyhow!(
            "目录 `{}` 里没有可索引的文件；放几个源码文件进去再试",
            root.display()
        ));
    }
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();

    let cache_prefix = options.cache_dir.as_ref().map(|dir| cache_prefix_for(dir, &root));
    let host = if let Some(prefix) = &cache_prefix {
        std::fs::create_dir_all(prefix.parent().expect("缓存父目录"))?;
        migrate_legacy_cache(options.cache_dir.as_ref().expect("启用缓存"), prefix)?;
        eprintln!("向量缓存：{}（重启复用，未变化的代码不重新调用嵌入 API）", prefix.with_extension("sqlite3").display());
        RepoHost::persisted(&file_refs, prefix.with_extension("json"))?
    } else {
        RepoHost::new(&file_refs)
    };
    let mut context = activation_context(&root.to_string_lossy());
    let host = if options.semantic {
        let key = std::env::var(API_KEY_ENV)
            .map_err(|_| anyhow!("启用语义检索需要设置环境变量 {API_KEY_ENV}"))?;
        context
            .metadata
            .insert("embedding_mode".into(), "api".into());
        context
            .metadata
            .insert("embedding_model".into(), EMBEDDING_MODEL.into());
        context.metadata.insert(
            "embedding_dimension".into(),
            EMBEDDING_DIMENSION.to_string(),
        );
        context
            .metadata
            .insert("http_endpoint_id".into(), "search-playground".into());
        if options.rerank {
            context
                .metadata
                .insert("rerank_model".into(), RERANK_MODEL.into());
        }
        if key.trim().is_empty() {
            return Err(anyhow!("{API_KEY_ENV} 不能为空"));
        }
        host.with_http_handler(forward_to_endpoint(key))
    } else {
        host
    };

    let mut plugin = CodeContextPlugin::default();
    if let Some(prefix) = cache_prefix {
        plugin = plugin.with_sqlite_vector_database(prefix.with_extension("sqlite3"));
    }
    plugin
        .activate(&host, context)
        .context("激活 Code Context 插件失败")?;

    eprintln!("正在索引 {} 个文件：{}", files.len(), root.display());
    eprintln!("本轮使用启动时的源码快照；修改源码后重启，默认复用磁盘缓存。模型仅使用 API。");
    if options.semantic {
        eprintln!("语义检索已开启（{EMBEDDING_MODEL}），首次索引需要调用嵌入接口，请稍候");
    } else {
        eprintln!(
            "语义检索未开启：只跑符号 + 词法。中文自然语言查询会明显变差——词法无法区分\
             「对话」与「对话框」这类子串关系。设置 {API_KEY_ENV} 后默认即可开启。"
        );
    }
    let started = Instant::now();
    for round in 0..2_000 {
        let call = ToolCall::new(
            format!("playground-init-{round}"),
            "code_search",
            json!({"query": "__playground_index__"}),
        );
        plugin
            .call_tool_with_host(&host, call)
            .context("推进索引失败")?;
        if round % 10 == 0 {
            eprintln!(
                "索引进行中：第 {} 轮，状态 {}",
                round + 1,
                plugin.index_status().as_str()
            );
        }
        if plugin.index_status() != IndexStatus::Indexing {
            break;
        }
    }
    if plugin.index_status() == IndexStatus::Indexing {
        return Err(anyhow!("索引未能在限定轮次内完成"));
    }
    if !plugin.index_status().can_serve_search() {
        return Err(anyhow!("索引不可用：{}", plugin.index_status().as_str()));
    }
    if let Some(stats) = plugin.index_stats() {
        eprintln!(
            "索引完成：{} 个文件 / {} 个代码块 / {} 个符号，用时 {:.1}s，状态 {}",
            stats.files,
            stats.chunks,
            stats.symbols,
            started.elapsed().as_secs_f32(),
            plugin.index_status().as_str()
        );
    }
    if let Some(reason) = plugin.degraded_reason() {
        eprintln!("注意：{reason}");
    }

    match options.query.clone() {
        Some(query) => run_query(&mut plugin, &host, &root, &query, &options),
        None => interactive_loop(&mut plugin, &host, &root, &options),
    }
}

/// 交互式查询循环。
///
/// # Errors
///
/// 读取标准输入或刷新提示符失败时返回错误。
fn interactive_loop(
    plugin: &mut CodeContextPlugin,
    host: &RepoHost,
    root: &Path,
    options: &Options,
) -> Result<()> {
    println!("\n输入查询开始检索；exit / quit / :q 退出。\n");
    let stdin = io::stdin();
    loop {
        print!("检索> ");
        io::stdout().flush().context("刷新提示符失败")?;
        let mut line = String::new();
        if stdin.read_line(&mut line).context("读取输入失败")? == 0 {
            println!();
            return Ok(());
        }
        let query = line.trim();
        if query.is_empty() {
            continue;
        }
        if matches!(query, "exit" | "quit" | ":q") {
            return Ok(());
        }
        if let Err(error) = run_query(plugin, host, root, query, options) {
            eprintln!("查询失败：{error:#}");
        }
    }
}

/// 执行一次查询并打印结果与全部诊断。
fn run_query(
    plugin: &mut CodeContextPlugin,
    host: &RepoHost,
    root: &Path,
    query: &str,
    options: &Options,
) -> Result<()> {
    let started = Instant::now();
    let result = plugin.call_tool_with_host(
        host,
        ToolCall::new(
            "manual-search",
            "code_search",
            json!({
                "query": query, "max_results": options.limit, "token_budget": options.token_budget,
            }),
        ),
    )?;
    println!(
        "\nAgent 工具结果，用时 {:.1} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
    if result.is_error {
        return Err(anyhow!("{}", result.content));
    }
    if let Some(note) = result.content.get("note").and_then(|v| v.as_str()) {
        println!("提示：{note}");
    }
    let results = result.content["results"]
        .as_array()
        .ok_or_else(|| anyhow!("工具结果缺少 results"))?;
    if results.is_empty() {
        println!("没有命中结果。");
    }
    for (rank, item) in results.iter().enumerate() {
        let file = item["file"].as_str().unwrap_or("未知文件");
        let symbol = item
            .get("qualified_symbol")
            .or_else(|| item.get("symbol"))
            .and_then(|v| v.as_str())
            .unwrap_or("文本块");
        let first = item["lines"][0].as_u64().unwrap_or(0);
        let last = item["lines"][1].as_u64().unwrap_or(first);
        println!(
            "\n[{}] {symbol}\n{}:{first}-{last}",
            rank + 1,
            root.join(file).display()
        );
        if let Some(snippet) = item["snippet"].as_str() {
            for (offset, line) in snippet.lines().enumerate() {
                println!("{:>5} │ {line}", first + offset as u64);
            }
        }
    }
    if options.diagnostics {
        println!("\n额外诊断：以下是内部候选，不是 Agent 最终结果，可能额外调用 API。");
        let started = Instant::now();
        let execution = plugin.search_execution(host, query, None, options.limit);
        print_diagnostics(&execution, query, started.elapsed().as_secs_f64() * 1000.0);
        print_results(&execution, root);
    }
    Ok(())
}

/// 只读取允许的源码，跳过符号链接、依赖和显式排除路径，错误不静默吞掉。
fn cache_prefix_for(base: &Path, root: &Path) -> PathBuf {
    // --root 指向 src 等子目录时，向上找项目标识，避免所有项目都缓存到名为 src 的目录。
    let project = root.ancestors().find(|path| {
        path.join(".git").exists() || path.join("package.json").is_file()
            || path.join("Cargo.toml").is_file() || path.join("pom.xml").is_file()
    }).unwrap_or(root);
    let name = project.file_name().filter(|name| !name.is_empty()).unwrap_or_else(|| std::ffi::OsStr::new("workspace"));
    // 同名项目或同一项目不同扫描根仍隔离，目录可读，文件名保留稳定根摘要。
    base.join(name).join(code_context_plugin::content_hash(&format!("playground-v1:{}", root.display())))
}

/// 把旧平铺缓存迁入项目目录；SQLite 使用一致性快照，包含 WAL 中已提交的数据。
/// 保留旧文件，新缓存已存在时不覆盖；迁移失败会报错，不静默重新付费生成向量。
fn migrate_legacy_cache(base: &Path, destination: &Path) -> Result<()> {
    let name = destination.file_name().context("缓存文件名缺失")?;
    let json = destination.with_extension("json");
    let database = destination.with_extension("sqlite3");
    if json.exists() || database.exists() { return Ok(()); }
    for legacy in [base.join(name), base.join("playground").join(name)] {
        let old_json = legacy.with_extension("json");
        let old_database = legacy.with_extension("sqlite3");
        if !old_json.is_file() { continue; }
        let metadata = std::fs::read(&old_json).context("读取旧缓存元数据失败")?;
        serde_json::from_slice::<serde_json::Value>(&metadata).context("旧缓存元数据不是有效 JSON")?;
        if old_database.is_file() {
            let connection = rusqlite::Connection::open_with_flags(&old_database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .context("打开旧向量数据库失败")?;
            connection.execute("VACUUM INTO ?1", [database.to_string_lossy().as_ref()])
                .context("迁移旧向量数据库失败")?;
        }
        let temporary = json.with_extension("json.migrating");
        std::fs::write(&temporary, metadata).context("写入迁移元数据失败")?;
        std::fs::rename(&temporary, &json).context("落盘迁移元数据失败")?;
        eprintln!("已迁移旧缓存：{}；旧文件保留", legacy.display());
        break;
    }
    Ok(())
}

/// 只读取允许的源码，跳过符号链接、依赖和显式排除路径，错误不静默吞掉。
fn collect_files(root: &Path, excludes: &[String]) -> Result<Vec<(String, String)>> {
    fn walk(
        root: &Path,
        dir: &Path,
        excludes: &[String],
        depth: usize,
        files: &mut Vec<(String, String)>,
    ) -> Result<()> {
        if depth > 24 {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            if ty.is_symlink() {
                continue;
            }
            let path = entry.path();
            let relative = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            if excludes
                .iter()
                .any(|prefix| relative == *prefix || relative.starts_with(&format!("{prefix}/")))
            {
                continue;
            }
            if ty.is_dir() {
                if matches!(
                    entry.file_name().to_str(),
                    Some(
                        ".git"
                            | "node_modules"
                            | "target"
                            | "dist"
                            | "build"
                            | "coverage"
                            | "vendor"
                            | ".idea"
                            | ".vscode"
                            | ".cache"
                            | ".venv"
                    )
                ) {
                    continue;
                }
                walk(root, &path, excludes, depth + 1, files)?;
            } else if ty.is_file()
                && matches!(
                    code_context_plugin::classify(&relative),
                    code_context_plugin::PathClass::Indexable(_)
                )
            {
                files.push((
                    relative,
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("读取 {} 失败", path.display()))?,
                ));
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    walk(root, root, excludes, 0, &mut files)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(files)
}

/// 打印检索链路各层的判断依据。
///
/// `query` 为用户原始输入，用于对照实际送去召回的文本是否被剥离了开头指令词。
fn print_diagnostics(execution: &SearchExecution, query: &str, millis: f64) {
    let diagnostics = &execution.diagnostics;
    let plan = &diagnostics.plan;
    let confidence = &diagnostics.confidence;
    let stages = &diagnostics.stages;

    println!("\n── 检索链路 ──────────────────────────────────────");
    println!(
        "查询意图  {:?}   检索目标 {:?}   延迟路径 {}   耗时 {millis:.1} ms",
        diagnostics.analyzed_query.intent,
        diagnostics.analyzed_query.goal,
        diagnostics.path.as_str()
    );
    // 指令词是否被剥离，直接决定两路召回看到的是什么；只在与原查询不同时提示，避免刷屏。
    if diagnostics.analyzed_query.retrieval_text != query {
        println!(
            "检索文本  {}（已剥离开头指令词）",
            diagnostics.analyzed_query.retrieval_text
        );
    }
    println!(
        "检索计划  符号 {}（上限 {}）· 词法 {}（上限 {}）· 语义 {}（上限 {}）· 候选池 {}",
        on_off(plan.enable_symbol),
        plan.symbol_limit,
        on_off(plan.enable_lexical),
        plan.lexical_limit,
        on_off(plan.enable_semantic),
        plan.semantic_limit,
        plan.candidate_pool_limit,
    );
    println!(
        "候选规模  符号 {} · 词法 {} · 语义 {} → 并集 {} → 融合后 {} → 返回 {}",
        stages.symbol.len(),
        stages.lexical.len(),
        stages.semantic.len(),
        stages.union.len(),
        stages.post_fusion.len(),
        stages.final_results.len(),
    );
    println!(
        "置信度    {:?}（Top1/Top2 分差 {:.3}）· 符号词法一致 {} · 精确符号命中 {} · 含实现 {}",
        confidence.level,
        confidence.top_margin,
        yes_no(confidence.symbol_lexical_agreement),
        yes_no(confidence.exact_symbol_hit),
        yes_no(confidence.has_implementation),
    );
    println!(
        "模型重排  计划允许 {} · 实际发起 {} · 生效 {} — {}",
        yes_no(plan.allow_rerank),
        yes_no(diagnostics.rerank_activated),
        yes_no(diagnostics.rerank_applied),
        diagnostics.rerank_reason,
    );
}

/// 打印结果列表，并附带每条的角色、通道来源与源码预览。
fn print_results(execution: &SearchExecution, root: &Path) {
    if execution.results.is_empty() {
        println!("\n没有命中任何结果。\n");
        return;
    }
    println!("\n── 结果 ────────────────────────────────────────");
    for (rank, result) in execution.results.iter().enumerate() {
        let symbol = result
            .qualified_symbol
            .as_deref()
            .or(result.symbol_name.as_deref())
            .unwrap_or("（文本块）");
        println!("\n[{}] {}  {:.4}", rank + 1, symbol, result.score);
        println!(
            "    {}:{}-{}   角色 {:?}   语言 {}",
            result.file_path, result.start_line, result.end_line, result.role, result.language
        );
        println!("    来源 {}", channel_summary(result));
        println!("    构成 {}", result.debug.reason());
        if let Some(preview) = read_preview(
            root,
            result.file_path.as_str(),
            result.start_line,
            result.end_line,
        ) {
            for (offset, line) in preview.iter().enumerate() {
                println!("    {:>5} │ {line}", result.start_line as usize + offset);
            }
        }
    }
    println!();
}

/// 汇总该结果被哪些通道召回、各自排第几。
fn channel_summary(result: &code_context_plugin::SearchResult) -> String {
    if result.debug.ranks.is_empty() {
        return "（无通道记录）".to_string();
    }
    result
        .debug
        .ranks
        .iter()
        .map(|(source, rank)| {
            let name = match source {
                CandidateSource::Symbol => "符号",
                CandidateSource::Lexical => "词法",
                CandidateSource::Semantic => "语义",
            };
            format!("{name} 第 {rank} 名")
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// 读取结果起始行开始的若干行源码。
fn read_preview(
    root: &Path,
    relative: &str,
    start_line: u32,
    end_line: u32,
) -> Option<Vec<String>> {
    if start_line == 0 || end_line < start_line {
        return None;
    }
    let content = std::fs::read_to_string(root.join(relative)).ok()?;
    let start = start_line.saturating_sub(1) as usize;
    Some(
        content
            .lines()
            .skip(start)
            .take(PREVIEW_LINES.min((end_line - start_line + 1) as usize))
            .map(str::to_string)
            .collect(),
    )
}

/// 把 Guest 的相对路径请求转发到真实端点，并在宿主侧注入凭证。
///
/// 与生产宿主同一套约定：插件只提交相对路径，看不到地址也拿不到密钥。
fn forward_to_endpoint(api_key: String) -> impl Fn(&HttpRequest) -> Result<HttpResponse> {
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(180)))
            .build(),
    );
    move |request: &HttpRequest| {
        if !request.method.eq_ignore_ascii_case("POST")
            || !matches!(
                request.path.trim_start_matches('/'),
                "embeddings" | "rerank"
            )
        {
            return Err(anyhow!("试验场只允许嵌入和重排 POST 请求"));
        }
        let url = url::Url::parse(EMBEDDING_BASE_URL)
            .and_then(|base| base.join(request.path.trim_start_matches('/')))
            .map_err(|error| anyhow!("拼接端点路径失败：{error}"))?;
        let mut builder = agent.post(url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder = builder.header("authorization", &format!("Bearer {api_key}"));
        let response = match request.body.as_deref() {
            Some(body) => builder.send(body.as_bytes()),
            None => builder.send(&[] as &[u8]),
        }
        .map_err(|error| anyhow!("端点请求失败：{error}"))?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .into_with_config()
            .limit(32 * 1024 * 1024)
            .read_to_string()
            .map_err(|error| anyhow!("读取端点响应失败：{error}"))?;
        Ok(HttpResponse {
            status,
            headers: HashMap::new(),
            body,
        })
    }
}

/// 解析命令行参数。
///
/// # Errors
///
/// 选项缺少取值或数值非法时返回错误。
fn parse_options() -> Result<Options> {
    parse_args(std::env::args().skip(1), std::env::var(API_KEY_ENV).is_ok())
}

/// 解析可测试的参数序列；路径和数值错误在索引前报告。
fn parse_args(arguments: impl IntoIterator<Item = String>, has_key: bool) -> Result<Options> {
    // 默认索引整个 Lucia 仓库：试验场的意义就是在真实规模的代码上试检索。
    let mut root = repository_root();
    let mut limit = DEFAULT_LIMIT;
    // 默认跟随生产配置：只要给了密钥就开语义。中文自然语言查询几乎完全依赖语义通道
    // ——词法无法区分"对话"与"对话框"这类子串关系，默认关掉会给出误导性的糟糕结果。
    let mut semantic = has_key;
    // 自然语言默认源码重排；精确符号跳过，重复输入可能命中已校验的响应缓存。
    let mut rerank = semantic;
    let mut cache_dir = Some(
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(".lucia/code-context"),
    );
    let mut token_budget = 8000;
    let mut diagnostics = false;
    let mut excludes = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    let mut args = arguments.into_iter();

    while let Some(argument) = args.next() {
        match argument.as_str() {
            "-h" | "--help" => {
                print_help();
                std::process::exit(0);
            }
            "--root" => {
                root = PathBuf::from(args.next().ok_or_else(|| anyhow!("--root 缺少目录参数"))?);
            }
            "--limit" => {
                limit = args
                    .next()
                    .ok_or_else(|| anyhow!("--limit 缺少数值"))?
                    .parse()
                    .context("--limit 需要正整数")?;
            }
            "--semantic" => {
                semantic = true;
                rerank = true;
            }
            "--cache-dir" => {
                cache_dir = Some(PathBuf::from(
                    args.next().ok_or_else(|| anyhow!("--cache-dir 缺少路径"))?,
                ))
            }
            "--no-cache" => cache_dir = None,
            "--token-budget" => {
                token_budget = args
                    .next()
                    .ok_or_else(|| anyhow!("--token-budget 缺少数值"))?
                    .parse()
                    .context("token 预算须为整数")?
            }
            "--diagnostics" => diagnostics = true,
            "--exclude" => {
                let path = args
                    .next()
                    .ok_or_else(|| anyhow!("--exclude 缺少相对路径"))?
                    .replace('\\', "/");
                if path.starts_with('/')
                    || path.split('/').any(|p| p == "..")
                    || path.trim_matches('/').is_empty()
                {
                    return Err(anyhow!("--exclude 必须是非空仓库相对路径"));
                }
                excludes.push(path.trim_end_matches('/').to_string());
            }
            "--no-semantic" => {
                semantic = false;
                rerank = false;
            }
            "--no-rerank" => rerank = false,
            "--rerank" => {
                semantic = true;
                rerank = true;
            }
            unknown if unknown.starts_with("--") => {
                return Err(anyhow!("未知选项：{unknown}"));
            }
            text => parts.push(text.to_string()),
        }
    }

    let query = parts.join(" ").trim().to_string();
    if !(1..=50).contains(&limit) {
        return Err(anyhow!("--limit 范围为 1–50"));
    }
    if !(500..=50000).contains(&token_budget) {
        return Err(anyhow!("--token-budget 范围为 500–50000"));
    }
    Ok(Options {
        root,
        query: (!query.is_empty()).then_some(query),
        limit: limit.max(1),
        semantic,
        rerank,
        cache_dir,
        token_budget,
        diagnostics,
        excludes,
    })
}

/// 打印用法。
fn print_help() {
    println!(
        "代码检索试验场\n\n\
         用法：cargo run -p code-context-plugin --features test-support \\\n\
         \x20         --example search_playground -- [选项] [查询]\n\n\
         选项：\n\
         \x20 --root <目录>   被索引的目录，默认当前 Lucia 仓库根\n\
         \x20 --limit <数量>  返回条数，默认 {DEFAULT_LIMIT}\n\
         \x20 --semantic      开启语义检索；设置了 {API_KEY_ENV} 时默认即开启\n\
         \x20 --no-semantic   强制只跑符号 + 词法（中文自然语言查询会明显变差）\n\
         \x20 --rerank        在语义之上再开模型重排；设置了密钥时默认即开启\n\
         \x20 --no-rerank     关闭模型重排，只保留本地融合排序\n\
         \x20 --cache-dir <目录>  缓存基础目录，默认 ~/.lucia/code-context；下设 <项目名>/\n\
         \x20 --no-cache      禁用跨进程缓存\n\
         \x20 --token-budget <数量>  Agent 输出预算，默认 8000\n\
         \x20 --exclude <相对路径>  排除目录或文件，可重复\n\
         \x20 --diagnostics   额外运行内部排序诊断，可能多一次 API 请求\n\
         \x20 -h, --help      显示本帮助\n\n\
         不带查询时进入交互模式，可连续输入多条查询。"
    );
}

/// 布尔值的开关文案。
fn on_off(value: bool) -> &'static str {
    if value {
        "开"
    } else {
        "关"
    }
}

/// 布尔值的是否文案。
fn yes_no(value: bool) -> &'static str {
    if value {
        "是"
    } else {
        "否"
    }
}

/// 判断标准输出是否连接终端；保留给未来的着色输出使用。
#[allow(dead_code)]
fn is_tty() -> bool {
    io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试参数不读取当前机器的凭证状态。
    fn options(args: &[&str]) -> Result<Options> {
        parse_args(args.iter().map(|s| s.to_string()), false)
    }

    /// 显式启用语义时默认同步启用重排，所有模型都使用 API。
    #[test]
    fn semantic_flag_enables_rerank_and_manual_options() {
        let parsed = options(&[
            "--semantic",
            "--root",
            "/tmp/project",
            "--cache-dir",
            "/tmp/cache",
            "--token-budget",
            "12000",
            "--exclude",
            "src/debug/",
            "--diagnostics",
            "查找下单实现",
        ])
        .unwrap();
        assert!(parsed.semantic && parsed.rerank && parsed.diagnostics);
        assert_eq!(parsed.root, PathBuf::from("/tmp/project"));
        assert_eq!(parsed.cache_dir, Some(PathBuf::from("/tmp/cache")));
        assert_eq!(parsed.excludes, ["src/debug"]);
        assert_eq!(parsed.token_budget, 12000);
        assert_eq!(parsed.query.as_deref(), Some("查找下单实现"));
        let offline = options(&["--semantic", "--no-semantic", "--no-cache"]).unwrap();
        assert!(!offline.semantic && !offline.rerank && offline.cache_dir.is_none());
    }

    /// 非法边界和未知选项必须在索引前失败。
    #[test]
    fn invalid_options_fail_early() {
        for args in [
            &["--limit", "0"][..],
            &["--limit", "51"],
            &["--token-budget", "499"],
            &["--token-budget", "50001"],
            &["--exclude", "../secret"],
            &["--root"],
            &["--local-model"],
        ] {
            assert!(options(args).is_err(), "{args:?}");
        }
    }

    /// 小仓库实际工具调用、排除目录和预览边界可离线验证，不运行本地模型。
    #[test]
    fn fixture_scan_tool_result_and_preview_are_bounded() {
        let root = std::env::temp_dir().join(format!(
            "playground-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("debug")).unwrap();
        std::fs::write(
            root.join("main.ts"),
            "export function submitOrder() { return 1; }\nconst unrelated = 2;\n",
        )
        .unwrap();
        std::fs::write(root.join("debug/secret.ts"), "const hidden = 1;").unwrap();
        std::fs::write(root.join(".env"), "SECRET=not-a-real-secret").unwrap();
        let files = collect_files(&root, &["debug".into()]).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(read_preview(&root, "main.ts", 1, 1).unwrap().len(), 1);
        assert!(read_preview(&root, "main.ts", 0, 1).is_none());
        let refs: Vec<_> = files
            .iter()
            .map(|(p, c)| (p.as_str(), c.as_str()))
            .collect();
        let host = RepoHost::persisted(&refs, root.join("cache.json")).unwrap();
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(&host, activation_context(&root.to_string_lossy()))
            .unwrap();
        run_query(
            &mut plugin,
            &host,
            &root,
            "submitOrder",
            &options(&["--no-cache"]).unwrap(),
        )
        .unwrap();
        assert!(root.join("cache.json").is_file());
        std::fs::remove_dir_all(root).unwrap();
    }
}

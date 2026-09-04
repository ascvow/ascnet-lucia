//! 代码检索试验场：拿你自己的代码试排序，并看清"为什么是这个排名"。
//!
//! 与 `search_repository` 的分工：那个示例面向"在真实仓库里找代码"，输出的是结果本身；
//! 这个示例面向**调参与排查**，把检索链路的每一层判断都摊开——查询意图、检索目标、
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

use agent_plugin::{AgentPlugin, HttpRequest, HttpResponse, ToolCall};
use anyhow::{anyhow, Context, Result};
use code_context_plugin::{
    testing::{activation_context, collect_repository_files, repository_root, RepoHost},
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

    let files = collect_repository_files(&root);
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
        RepoHost::new(&file_refs).with_http_handler(forward_to_endpoint(key))
    } else {
        RepoHost::new(&file_refs)
    };

    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(&host, context)
        .context("激活 Code Context 插件失败")?;

    eprintln!("正在索引 {} 个文件：{}", files.len(), root.display());
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
            json!({"query": "初始化索引"}),
        );
        let _ = plugin.call_tool_with_host(&host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            break;
        }
    }
    if plugin.index_status() == IndexStatus::Indexing {
        return Err(anyhow!("索引未能在限定轮次内完成"));
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
        Some(query) => {
            run_query(&mut plugin, &host, &root, &query, &options);
            Ok(())
        }
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
        run_query(plugin, host, root, query, options);
    }
}

/// 执行一次查询并打印结果与全部诊断。
fn run_query(
    plugin: &mut CodeContextPlugin,
    host: &RepoHost,
    root: &Path,
    query: &str,
    options: &Options,
) {
    let started = Instant::now();
    let execution = plugin.search_execution(host, query, None, options.limit);
    let elapsed = started.elapsed();
    print_diagnostics(&execution, elapsed.as_secs_f64() * 1000.0);
    print_results(&execution, root);
}

/// 打印检索链路各层的判断依据。
fn print_diagnostics(execution: &SearchExecution, millis: f64) {
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
        if let Some(preview) = read_preview(root, result.file_path.as_str(), result.start_line) {
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
fn read_preview(root: &Path, relative: &str, start_line: u32) -> Option<Vec<String>> {
    let content = std::fs::read_to_string(root.join(relative)).ok()?;
    let start = start_line.saturating_sub(1) as usize;
    Some(
        content
            .lines()
            .skip(start)
            .take(PREVIEW_LINES)
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
    // 默认索引整个 Lucia 仓库：试验场的意义就是在真实规模的代码上试检索。
    let mut root = repository_root();
    let mut limit = DEFAULT_LIMIT;
    // 默认跟随生产配置：只要给了密钥就开语义。中文自然语言查询几乎完全依赖语义通道
    // ——词法无法区分"对话"与"对话框"这类子串关系，默认关掉会给出误导性的糟糕结果。
    let mut semantic = std::env::var(API_KEY_ENV).is_ok();
    // 重排同样默认开启：它是**条件触发**的，只在低置信度的自然语言查询上才真正发起请求，
    // 因此打开它不等于每次查询都付 1.7 秒。诊断里的"实际发起"一行会如实反映是否触发。
    let mut rerank = semantic;
    let mut parts: Vec<String> = Vec::new();
    let mut args = std::env::args().skip(1);

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
            "--semantic" => semantic = true,
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
    Ok(Options {
        root,
        query: (!query.is_empty()).then_some(query),
        limit: limit.max(1),
        semantic,
        rerank,
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

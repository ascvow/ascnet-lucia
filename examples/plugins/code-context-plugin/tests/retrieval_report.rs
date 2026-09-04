//! 检索质量评测报告。
//!
//! 与 `retrieval_benchmark.rs` 的分工：那里是**回归门禁**，用固定阈值卡住质量下滑；
//! 这里是**评测报告**，按查询类型分组给出逐条结果与消融对比，用来回答"改动到底改善了
//! 什么、代价是什么"。
//!
//! 生成报告：
//!
//! ```sh
//! cargo test -p code-context-plugin --features test-support \
//!     --test retrieval_report -- --nocapture
//! ```
//!
//! # 查询集怎么设计的
//!
//! 检索评测最容易自欺的地方是查询集：如果每条查询都把目标符号名原样写出来，那么任何
//! 词法检索都能拿满分，指标好看但与真实使用无关。这里按**查询与目标的词面距离**分成
//! 五类，其中后三类刻意不含目标符号名：
//!
//! | 类型 | 查询与目标的关系 | 考察什么 |
//! | --- | --- | --- |
//! | 精确标识符 | 查询就是符号名 | 符号通道的精确定位 |
//! | 限定名与成员 | 查询是 `Type::method` | 限定名解析与父子归属 |
//! | 混合查询 | 自然语言 + 标识符 | 定义能否压过使用点 |
//! | 中文行为描述 | 只有中文，无符号名 | 跨语言语义召回 |
//! | 概念查询 | 连关键词都不直接出现 | 真正的语义理解 |
//!
//! # 为什么同时看"文件命中"与"定义命中"
//!
//! 截图里那种"返回了各个使用点、函数本体没出现"的失败，在只看文件级 Recall 时完全
//! 看不出来——使用点和定义常常就在同一个文件里。因此每条查询同时记录目标**符号**是否
//! 出现在结果中，这才是"能不能替代人工翻代码"的真实判据。

use code_context_plugin::{
    testing::{activation_context, collect_repository_files, repository_root, RepoHost},
    CodeContextPlugin, IndexStatus, SearchResult,
};

use agent_plugin::{AgentPlugin, ToolCall};
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt::Write as _;

// ===========================================================================
// 查询集
// ===========================================================================

/// 查询类型，决定它考察检索链路的哪一段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum QueryKind {
    /// 查询本身就是一个符号名。
    ExactIdentifier,
    /// 查询是限定名或成员路径。
    QualifiedName,
    /// 自然语言中夹带标识符。
    Mixed,
    /// 纯中文行为描述，不含目标符号名。
    ChineseBehavior,
    /// 概念性描述，连关键词都不直接出现在目标里。
    Conceptual,
}

impl QueryKind {
    /// 返回报告中展示的中文名称。
    fn label(self) -> &'static str {
        match self {
            Self::ExactIdentifier => "精确标识符",
            Self::QualifiedName => "限定名与成员",
            Self::Mixed => "混合查询",
            Self::ChineseBehavior => "中文行为描述",
            Self::Conceptual => "概念查询",
        }
    }
}

/// 一条评测查询。
struct EvalQuery {
    /// 查询类型。
    kind: QueryKind,
    /// 提交给 `code_search` 的查询串。
    query: &'static str,
    /// 目标文件的路径子串；命中任一即算文件命中。
    ///
    /// 用子串而不是精确路径：仓库结构会随重构变动，路径子串对目录调整更稳定。
    expected_paths: &'static [&'static str],
    /// 目标符号短名；命中任一即算定义命中。空表示该查询不考察具体符号。
    expected_symbols: &'static [&'static str],
}

/// 评测查询集。
///
/// 全部目标都在本仓库内，因此报告可在任何检出上复现，不依赖外部语料。
fn eval_queries() -> Vec<EvalQuery> {
    vec![
        // --- 精确标识符：查询就是符号名 ---
        EvalQuery {
            kind: QueryKind::ExactIdentifier,
            query: "StructuralIndex",
            expected_paths: &["code-context-plugin/src/index/"],
            expected_symbols: &["StructuralIndex"],
        },
        EvalQuery {
            kind: QueryKind::ExactIdentifier,
            query: "SemanticIndex",
            expected_paths: &["code-context-plugin/src/semantic"],
            expected_symbols: &["SemanticIndex"],
        },
        EvalQuery {
            kind: QueryKind::ExactIdentifier,
            query: "CodeSearchRequest",
            expected_paths: &["code-context-plugin/src/tool.rs"],
            expected_symbols: &["CodeSearchRequest"],
        },
        EvalQuery {
            kind: QueryKind::ExactIdentifier,
            query: "PluginHostApi",
            expected_paths: &["agent-plugins/sdk/src/lib.rs"],
            expected_symbols: &["PluginHostApi"],
        },
        EvalQuery {
            kind: QueryKind::ExactIdentifier,
            query: "ToolResult",
            expected_paths: &["agent-core/tool/src/lib.rs"],
            expected_symbols: &["ToolResult"],
        },
        EvalQuery {
            kind: QueryKind::ExactIdentifier,
            query: "IgnoreRule",
            expected_paths: &["code-context-plugin/src/repository/ignore"],
            expected_symbols: &["IgnoreRule"],
        },
        EvalQuery {
            kind: QueryKind::ExactIdentifier,
            query: "FusionConfig",
            expected_paths: &["code-context-plugin/src/retrieval/fusion"],
            expected_symbols: &["FusionConfig"],
        },
        EvalQuery {
            kind: QueryKind::ExactIdentifier,
            query: "PluginVectorStore",
            expected_paths: &["code-context-plugin/src/sqlite_vector"],
            expected_symbols: &["SqliteVectorStore"],
        },
        // --- 限定名与成员 ---
        EvalQuery {
            kind: QueryKind::QualifiedName,
            query: "CodeSearchRequest::parse",
            expected_paths: &["code-context-plugin/src/tool.rs"],
            expected_symbols: &["parse"],
        },
        EvalQuery {
            kind: QueryKind::QualifiedName,
            query: "SemanticIndex::search",
            expected_paths: &["code-context-plugin/src/semantic"],
            expected_symbols: &["search"],
        },
        EvalQuery {
            kind: QueryKind::QualifiedName,
            query: "IndexBuild::advance",
            expected_paths: &["code-context-plugin/src/index/build"],
            expected_symbols: &["advance"],
        },
        EvalQuery {
            kind: QueryKind::QualifiedName,
            query: "RetrievalIndex::search_hybrid",
            expected_paths: &["code-context-plugin/src/retrieval/mod"],
            expected_symbols: &["search_hybrid"],
        },
        EvalQuery {
            kind: QueryKind::QualifiedName,
            query: "SqliteVectorStore::upsert_batch",
            expected_paths: &["code-context-plugin/src/sqlite_vector"],
            expected_symbols: &["upsert_batch"],
        },
        // --- 混合查询：自然语言里夹着标识符，考察定义能否压过使用点 ---
        EvalQuery {
            kind: QueryKind::Mixed,
            query: "parse_file 是在哪里定义的",
            expected_paths: &["code-context-plugin/src/parser/mod"],
            expected_symbols: &["parse_file"],
        },
        EvalQuery {
            kind: QueryKind::Mixed,
            query: "estimate_tokens 这个函数怎么算的",
            expected_paths: &["code-context-plugin/src/context/"],
            expected_symbols: &["estimate_tokens"],
        },
        EvalQuery {
            kind: QueryKind::Mixed,
            query: "content_hash 的实现在哪",
            expected_paths: &["code-context-plugin/src/repository/"],
            expected_symbols: &["content_hash"],
        },
        EvalQuery {
            kind: QueryKind::Mixed,
            query: "HttpRequest 结构体定义",
            expected_paths: &["agent-plugins/sdk/src/lib.rs"],
            expected_symbols: &["HttpRequest"],
        },
        EvalQuery {
            kind: QueryKind::Mixed,
            query: "tokenize_query 做了什么处理",
            expected_paths: &["code-context-plugin/src/retrieval/tokenize"],
            expected_symbols: &["tokenize_query"],
        },
        EvalQuery {
            kind: QueryKind::Mixed,
            query: "remove_chunks 怎么清理索引",
            expected_paths: &["code-context-plugin/src/retrieval/"],
            expected_symbols: &["remove_chunks"],
        },
        // --- 中文行为描述：不含任何目标符号名 ---
        EvalQuery {
            kind: QueryKind::ChineseBehavior,
            query: "怎么判断一个文件应不应该被索引",
            expected_paths: &["code-context-plugin/src/repository/"],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::ChineseBehavior,
            query: "代码块是怎么切分出来的",
            expected_paths: &["code-context-plugin/src/parser/"],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::ChineseBehavior,
            query: "多路检索结果如何合并排序",
            expected_paths: &["code-context-plugin/src/retrieval/fusion"],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::ChineseBehavior,
            query: "返回给模型的片段怎么控制长度",
            expected_paths: &["code-context-plugin/src/context/"],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::ChineseBehavior,
            query: "文件改动后索引如何增量更新",
            expected_paths: &[
                "code-context-plugin/src/incremental",
                "code-context-plugin/src/index/build",
            ],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::ChineseBehavior,
            query: "向量是怎么存到磁盘上的",
            expected_paths: &[
                "code-context-plugin/src/sqlite_vector",
                "code-context-plugin/src/semantic",
            ],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::ChineseBehavior,
            query: "插件怎么向宿主发起网络请求",
            expected_paths: &["code-context-plugin/src/embedding", "agent-plugins/"],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::ChineseBehavior,
            query: "忽略规则是怎么匹配路径的",
            expected_paths: &["code-context-plugin/src/repository/ignore"],
            expected_symbols: &[],
        },
        // --- 概念查询：关键词都不直接出现在目标里 ---
        EvalQuery {
            kind: QueryKind::Conceptual,
            query: "防止同一段代码在结果里出现两次",
            expected_paths: &["code-context-plugin/src/context/"],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::Conceptual,
            query: "一次做不完的任务怎么分批继续",
            expected_paths: &[
                "code-context-plugin/src/index/build",
                "code-context-plugin/src/repository/",
            ],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::Conceptual,
            query: "外部服务不可用时如何降级",
            expected_paths: &[
                "code-context-plugin/src/embedding",
                "code-context-plugin/src/rerank",
            ],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::Conceptual,
            query: "驼峰命名的标识符如何拆成可检索的词",
            expected_paths: &["code-context-plugin/src/retrieval/tokenize"],
            expected_symbols: &[],
        },
        EvalQuery {
            kind: QueryKind::Conceptual,
            query: "磁盘上的数据结构变了怎么兼容旧版本",
            expected_paths: &[
                "code-context-plugin/src/storage",
                "code-context-plugin/src/model",
            ],
            expected_symbols: &[],
        },
    ]
}

// ===========================================================================
// 指标
// ===========================================================================

/// 单条查询的评测结果。
struct QueryOutcome {
    /// 目标文件首次出现的名次，从 1 开始；未命中为 `None`。
    file_rank: Option<usize>,
    /// 目标符号首次出现的名次；不考察或未命中为 `None`。
    symbol_rank: Option<usize>,
    /// 该查询是否考察符号。
    checks_symbol: bool,
    /// 前三条结果的简述，用于逐题对比表。
    top: Vec<String>,
}

impl QueryOutcome {
    /// 目标文件是否出现在前 `k` 条中。
    fn hit_at(&self, k: usize) -> bool {
        self.file_rank.is_some_and(|rank| rank <= k)
    }

    /// 文件级 MRR 分量。
    fn reciprocal_rank(&self) -> f64 {
        self.file_rank.map_or(0.0, |rank| 1.0 / rank as f64)
    }

    /// 目标符号是否出现在结果中。
    fn symbol_hit(&self) -> bool {
        self.symbol_rank.is_some()
    }

    /// 返回报告中展示的判定符号。
    fn verdict(&self) -> &'static str {
        match (self.hit_at(1), self.hit_at(5), self.checks_symbol) {
            (true, _, true) if self.symbol_hit() => "命中",
            (true, _, false) => "命中",
            (true, _, true) => "文件命中/定义缺失",
            (false, true, true) if self.symbol_hit() => "前五命中",
            (false, true, _) => "前五命中",
            _ => "未命中",
        }
    }
}

/// 对一条查询求值。
fn evaluate(results: &[SearchResult], query: &EvalQuery) -> QueryOutcome {
    let file_rank = results.iter().position(|result| {
        query
            .expected_paths
            .iter()
            .any(|pattern| result.file_path.contains(pattern))
    });
    // 定义命中要求同时落在目标文件里：同名符号在别处出现不算找到定义。
    let symbol_rank = results.iter().position(|result| {
        let in_target = query
            .expected_paths
            .iter()
            .any(|pattern| result.file_path.contains(pattern));
        in_target
            && query.expected_symbols.iter().any(|symbol| {
                result.symbol_name.as_deref() == Some(*symbol)
                    || result.qualified_symbol.as_deref() == Some(*symbol)
            })
    });
    let top = results
        .iter()
        .take(3)
        .map(|result| {
            let file = result.file_path.rsplit('/').next().unwrap_or("?");
            match result.symbol_name.as_deref() {
                Some(symbol) => format!("{file}:{symbol}"),
                None => format!("{file}:{}-{}", result.start_line, result.end_line),
            }
        })
        .collect();

    QueryOutcome {
        file_rank: file_rank.map(|index| index + 1),
        symbol_rank: symbol_rank.map(|index| index + 1),
        checks_symbol: !query.expected_symbols.is_empty(),
        top,
    }
}

/// 一次配置下的汇总指标。
#[derive(Default, Clone, Copy)]
struct Metrics {
    /// 查询总数。
    queries: usize,
    /// Recall@1。
    recall_1: f64,
    /// Recall@5。
    recall_5: f64,
    /// Recall@10。
    recall_10: f64,
    /// 文件级 MRR。
    mrr: f64,
    /// 考察符号的查询数。
    symbol_queries: usize,
    /// 其中定义命中的数量。
    symbol_hits: usize,
}

impl Metrics {
    /// 从逐条结果汇总。
    fn from_outcomes(outcomes: &[QueryOutcome]) -> Self {
        if outcomes.is_empty() {
            return Self::default();
        }
        let total = outcomes.len() as f64;
        let symbol_queries = outcomes.iter().filter(|item| item.checks_symbol).count();
        Self {
            queries: outcomes.len(),
            recall_1: outcomes.iter().filter(|item| item.hit_at(1)).count() as f64 / total,
            recall_5: outcomes.iter().filter(|item| item.hit_at(5)).count() as f64 / total,
            recall_10: outcomes.iter().filter(|item| item.hit_at(10)).count() as f64 / total,
            mrr: outcomes
                .iter()
                .map(QueryOutcome::reciprocal_rank)
                .sum::<f64>()
                / total,
            symbol_queries,
            symbol_hits: outcomes
                .iter()
                .filter(|item| item.checks_symbol && item.symbol_hit())
                .count(),
        }
    }

    /// 返回定义命中率的展示文本；不考察符号时返回占位符。
    fn definition_rate(&self) -> String {
        if self.symbol_queries == 0 {
            return "—".to_string();
        }
        format!(
            "{}/{} ({:.0}%)",
            self.symbol_hits,
            self.symbol_queries,
            self.symbol_hits as f64 / self.symbol_queries as f64 * 100.0
        )
    }
}

// ===========================================================================
// 运行
// ===========================================================================

/// 在真实仓库上激活插件并推进索引至完成。
fn indexed_plugin(host: &RepoHost) -> CodeContextPlugin {
    let mut plugin = CodeContextPlugin::default();
    let root = repository_root();
    plugin
        .activate(host, activation_context(&root.to_string_lossy()))
        .expect("激活插件");
    for _ in 0..200 {
        let call = ToolCall::new("report", "code_search", json!({"query": "初始化索引"}));
        let _ = plugin.call_tool_with_host(host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            break;
        }
    }
    assert_ne!(
        plugin.index_status(),
        IndexStatus::Indexing,
        "索引应在有限轮次内完成"
    );
    plugin
}

/// 每条查询取回的结果数。
const RESULT_LIMIT: usize = 10;

/// 生成报告：逐类型汇总 + 逐题明细。
#[test]
fn retrieval_quality_report() {
    let root = repository_root();
    let files = collect_repository_files(&root);
    assert!(!files.is_empty(), "仓库中应有可索引文件");
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&file_refs);
    let mut plugin = indexed_plugin(&host);
    let stats = plugin.index_stats().expect("索引应已建成");

    let queries = eval_queries();
    let mut outcomes = Vec::with_capacity(queries.len());
    for query in &queries {
        let results = plugin.search_results(&host, query.query, None, RESULT_LIMIT);
        outcomes.push(evaluate(&results, query));
    }

    let mut report = String::new();
    writeln!(report, "\n# Code Context 检索质量报告\n").expect("写入报告");
    writeln!(
        report,
        "语料：本仓库 {} 个文件 / {} 个代码块 / {} 个符号\\\n\
         配置：符号 + 词法两路检索（无嵌入、无模型重排，可离线复现）\\\n\
         每条查询取回 {RESULT_LIMIT} 条结果\n",
        stats.files, stats.chunks, stats.symbols
    )
    .expect("写入报告");

    // --- 总览 ---
    let overall = Metrics::from_outcomes(&outcomes);
    writeln!(report, "## 总览\n").expect("写入报告");
    writeln!(report, "| 指标 | 数值 |").expect("写入报告");
    writeln!(report, "| --- | --- |").expect("写入报告");
    writeln!(report, "| 查询数 | {} |", overall.queries).expect("写入报告");
    writeln!(report, "| Recall@1 | {:.3} |", overall.recall_1).expect("写入报告");
    writeln!(report, "| Recall@5 | {:.3} |", overall.recall_5).expect("写入报告");
    writeln!(report, "| Recall@10 | {:.3} |", overall.recall_10).expect("写入报告");
    writeln!(report, "| MRR | {:.3} |", overall.mrr).expect("写入报告");
    writeln!(report, "| 定义命中率 | {} |", overall.definition_rate()).expect("写入报告");

    // --- 按查询类型 ---
    let mut grouped: BTreeMap<QueryKind, Vec<usize>> = BTreeMap::new();
    for (index, query) in queries.iter().enumerate() {
        grouped.entry(query.kind).or_default().push(index);
    }
    writeln!(report, "\n## 按查询类型\n").expect("写入报告");
    writeln!(
        report,
        "| 查询类型 | 条数 | Recall@1 | Recall@5 | MRR | 定义命中 |"
    )
    .expect("写入报告");
    writeln!(report, "| --- | --- | --- | --- | --- | --- |").expect("写入报告");
    for (kind, indexes) in &grouped {
        let subset: Vec<QueryOutcome> = indexes
            .iter()
            .map(|index| QueryOutcome {
                file_rank: outcomes[*index].file_rank,
                symbol_rank: outcomes[*index].symbol_rank,
                checks_symbol: outcomes[*index].checks_symbol,
                top: Vec::new(),
            })
            .collect();
        let metrics = Metrics::from_outcomes(&subset);
        writeln!(
            report,
            "| {} | {} | {:.2} | {:.2} | {:.2} | {} |",
            kind.label(),
            metrics.queries,
            metrics.recall_1,
            metrics.recall_5,
            metrics.mrr,
            metrics.definition_rate()
        )
        .expect("写入报告");
    }

    // --- 逐题明细 ---
    writeln!(report, "\n## 逐题明细\n").expect("写入报告");
    writeln!(report, "| 类型 | 查询 | 判定 | 名次 | 定义 | 前三结果 |").expect("写入报告");
    writeln!(report, "| --- | --- | --- | --- | --- | --- |").expect("写入报告");
    for (query, outcome) in queries.iter().zip(&outcomes) {
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} |",
            query.kind.label(),
            query.query,
            outcome.verdict(),
            outcome
                .file_rank
                .map_or("—".to_string(), |rank| rank.to_string()),
            if !outcome.checks_symbol {
                "—".to_string()
            } else {
                outcome
                    .symbol_rank
                    .map_or("未命中".to_string(), |rank| format!("第 {rank} 位"))
            },
            outcome.top.join("、")
        )
        .expect("写入报告");
    }

    println!("{report}");

    // 报告本身也是门禁：整体 Recall@5 跌破 0.7 说明检索链路出了结构性问题。
    assert!(
        overall.recall_5 >= 0.70,
        "Recall@5 {:.3} 低于门槛 0.70",
        overall.recall_5
    );
}

// ===========================================================================
// 消融对比：真实嵌入与真实重排
// ===========================================================================

/// 默认嵌入端点。
const EMBEDDING_BASE_URL: &str = "https://api.siliconflow.cn/v1/";
/// 默认嵌入模型与维度。
const EMBEDDING_MODEL: &str = "BAAI/bge-m3";
/// 嵌入维度。
const EMBEDDING_DIMENSION: u32 = 1024;
/// 默认重排模型。
const RERANK_MODEL: &str = "Qwen/Qwen3-Reranker-4B";
/// 读取 API 密钥的环境变量名。
const API_KEY_ENV: &str = "SILICONFLOW_API_KEY";

/// 一次消融配置。
struct Ablation {
    /// 报告中展示的名称。
    label: &'static str,
    /// 是否启用语义通道。
    semantic: bool,
    /// 是否启用模型重排。
    rerank: bool,
}

/// 构造带嵌入与重排配置的激活上下文。
fn ablation_context(root: &str, ablation: &Ablation) -> agent_plugin::ActivationContext {
    let mut context = activation_context(root);
    if ablation.semantic {
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
            .insert("embedding_batch_size".into(), "32".into());
        context
            .metadata
            .insert("http_endpoint_id".into(), "retrieval-report".into());
    }
    if ablation.rerank {
        context
            .metadata
            .insert("rerank_model".into(), RERANK_MODEL.into());
    }
    context
}

/// 把插件的相对路径请求转发到真实端点，并在宿主侧注入凭证。
///
/// 与生产宿主同一套约定：插件只提交相对路径，看不到地址也拿不到密钥。
fn forward_to_endpoint(
    api_key: String,
) -> impl Fn(&agent_plugin::HttpRequest) -> anyhow::Result<agent_plugin::HttpResponse> {
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(180)))
            .build(),
    );
    move |request: &agent_plugin::HttpRequest| {
        let url = url::Url::parse(EMBEDDING_BASE_URL)
            .and_then(|base| base.join(request.path.trim_start_matches('/')))
            .map_err(|error| anyhow::anyhow!("拼接端点路径失败：{error}"))?;
        let mut builder = agent.post(url.as_str());
        for (name, value) in &request.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        builder = builder.header("authorization", &format!("Bearer {api_key}"));
        let response = match request.body.as_deref() {
            Some(body) => builder.send(body.as_bytes()),
            None => builder.send(&[] as &[u8]),
        }
        .map_err(|error| anyhow::anyhow!("端点请求失败：{error}"))?;
        let status = response.status().as_u16();
        let body = response
            .into_body()
            .into_with_config()
            .limit(32 * 1024 * 1024)
            .read_to_string()
            .map_err(|error| anyhow::anyhow!("读取端点响应失败：{error}"))?;
        Ok(agent_plugin::HttpResponse {
            status,
            headers: std::collections::HashMap::new(),
            body,
        })
    }
}

/// 消融对比：逐通道开关，量化语义通道与模型重排各自的贡献。
///
/// 需要网络与 `SILICONFLOW_API_KEY`，因此默认忽略：
///
/// ```sh
/// SILICONFLOW_API_KEY=sk-... cargo test -p code-context-plugin --features test-support \
///     --test retrieval_report ablation -- --ignored --nocapture
/// ```
#[test]
#[ignore = "需要网络与嵌入 API 密钥"]
fn retrieval_ablation_report() {
    let Ok(api_key) = std::env::var(API_KEY_ENV) else {
        panic!("请设置 {API_KEY_ENV} 后再运行消融对比");
    };
    let root = repository_root();
    let files = collect_repository_files(&root);
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let queries = eval_queries();

    let ablations = [
        Ablation {
            label: "符号 + 词法",
            semantic: false,
            rerank: false,
        },
        Ablation {
            label: "+ 语义",
            semantic: true,
            rerank: false,
        },
        Ablation {
            label: "+ 语义 + 重排",
            semantic: true,
            rerank: true,
        },
    ];

    let mut rows: Vec<(&'static str, Metrics, BTreeMap<QueryKind, Metrics>)> = Vec::new();
    for ablation in &ablations {
        eprintln!("[消融] 正在运行配置：{}", ablation.label);
        let host =
            RepoHost::new(&file_refs).with_http_handler(forward_to_endpoint(api_key.clone()));
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(&host, ablation_context(&root.to_string_lossy(), ablation))
            .expect("激活插件");
        for _ in 0..400 {
            let call = ToolCall::new("ablation", "code_search", json!({"query": "初始化索引"}));
            let _ = plugin.call_tool_with_host(&host, call);
            if plugin.index_status() != IndexStatus::Indexing {
                break;
            }
        }
        assert_ne!(
            plugin.index_status(),
            IndexStatus::Indexing,
            "配置 `{}` 的索引未能完成",
            ablation.label
        );
        eprintln!(
            "[消融] {} → 状态 {} · 向量 {} 条 · 降级 {:?}",
            ablation.label,
            plugin.index_status().as_str(),
            plugin.semantic_len(),
            plugin.degraded_reason()
        );
        let mut outcomes = Vec::with_capacity(queries.len());
        for query in &queries {
            let results = plugin.search_results(&host, query.query, None, RESULT_LIMIT);
            outcomes.push(evaluate(&results, query));
        }
        let mut by_kind = BTreeMap::new();
        for kind in [
            QueryKind::ExactIdentifier,
            QueryKind::QualifiedName,
            QueryKind::Mixed,
            QueryKind::ChineseBehavior,
            QueryKind::Conceptual,
        ] {
            let subset: Vec<QueryOutcome> = queries
                .iter()
                .zip(&outcomes)
                .filter(|(query, _)| query.kind == kind)
                .map(|(_, outcome)| QueryOutcome {
                    file_rank: outcome.file_rank,
                    symbol_rank: outcome.symbol_rank,
                    checks_symbol: outcome.checks_symbol,
                    top: Vec::new(),
                })
                .collect();
            by_kind.insert(kind, Metrics::from_outcomes(&subset));
        }
        rows.push((ablation.label, Metrics::from_outcomes(&outcomes), by_kind));
    }

    let mut report = String::new();
    writeln!(report, "\n# 检索消融对比\n").expect("写入报告");
    writeln!(
        report,
        "嵌入 {EMBEDDING_MODEL}（{EMBEDDING_DIMENSION} 维）· 重排 {RERANK_MODEL}\n"
    )
    .expect("写入报告");
    writeln!(report, "## 总体\n").expect("写入报告");
    writeln!(
        report,
        "| 配置 | Recall@1 | Recall@5 | Recall@10 | MRR | 定义命中 |"
    )
    .expect("写入报告");
    writeln!(report, "| --- | --- | --- | --- | --- | --- |").expect("写入报告");
    for (label, metrics, _) in &rows {
        writeln!(
            report,
            "| {label} | {:.3} | {:.3} | {:.3} | {:.3} | {} |",
            metrics.recall_1,
            metrics.recall_5,
            metrics.recall_10,
            metrics.mrr,
            metrics.definition_rate()
        )
        .expect("写入报告");
    }

    writeln!(report, "\n## 按查询类型的 Recall@1\n").expect("写入报告");
    write!(report, "| 查询类型 |").expect("写入报告");
    for (label, _, _) in &rows {
        write!(report, " {label} |").expect("写入报告");
    }
    writeln!(report).expect("写入报告");
    writeln!(report, "| --- |{}", " --- |".repeat(rows.len())).expect("写入报告");
    for kind in [
        QueryKind::ExactIdentifier,
        QueryKind::QualifiedName,
        QueryKind::Mixed,
        QueryKind::ChineseBehavior,
        QueryKind::Conceptual,
    ] {
        write!(report, "| {} |", kind.label()).expect("写入报告");
        for (_, _, by_kind) in &rows {
            write!(report, " {:.2} |", by_kind[&kind].recall_1).expect("写入报告");
        }
        writeln!(report).expect("写入报告");
    }

    println!("{report}");
}

/// 诊断：语义通道到底有没有真正参与检索。
///
/// 消融对比里"+ 语义"与纯词法数字完全一致，这种一致性不可能来自权重差异，只能来自
/// 语义通道返回空。该测试直接把状态、降级原因、向量条数与分通道候选数打出来。
#[test]
#[ignore = "需要网络与嵌入 API 密钥"]
fn semantic_channel_diagnosis() {
    let api_key = std::env::var(API_KEY_ENV).expect("需要 API 密钥");
    let root = repository_root();
    let files = collect_repository_files(&root);
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&file_refs).with_http_handler(forward_to_endpoint(api_key));
    let ablation = Ablation {
        label: "语义",
        semantic: true,
        rerank: false,
    };
    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(&host, ablation_context(&root.to_string_lossy(), &ablation))
        .expect("激活插件");
    for round in 0..400 {
        let call = ToolCall::new("diag", "code_search", json!({"query": "初始化索引"}));
        let _ = plugin.call_tool_with_host(&host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            eprintln!("[诊断] 索引在第 {} 轮结束", round + 1);
            break;
        }
    }

    eprintln!("[诊断] 索引状态：{}", plugin.index_status().as_str());
    eprintln!("[诊断] 降级原因：{:?}", plugin.degraded_reason());
    eprintln!("[诊断] 索引规模：{:?}", plugin.index_stats());
    eprintln!("[诊断] 语义向量条数：{}", plugin.semantic_len());
    eprintln!("[诊断] HTTP 请求数：{}", host.http_requests().len());
    for request in host.http_requests() {
        let body = request.body.as_deref().unwrap_or("");
        eprintln!("[诊断] 请求路径：{} 正文字节：{}", request.path, body.len());
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
            if let Some(inputs) = value.get("input").and_then(|v| v.as_array()) {
                eprintln!("[诊断] input 条数：{}", inputs.len());
                let lens: Vec<usize> = inputs
                    .iter()
                    .map(|v| v.as_str().map(str::len).unwrap_or(0))
                    .collect();
                eprintln!(
                    "[诊断] input 长度：最小 {} 最大 {} 总计 {}",
                    lens.iter().min().copied().unwrap_or(0),
                    lens.iter().max().copied().unwrap_or(0),
                    lens.iter().sum::<usize>()
                );
                eprintln!(
                    "[诊断] 其他字段：{:?}",
                    value.as_object().map(|o| o.keys().collect::<Vec<_>>())
                );
            }
        }
    }

    let channels = plugin.search_candidates("代码块是怎么切分出来的", None, 20);
    for channel in &channels {
        let source = channel
            .first()
            .map(|candidate| candidate.source.as_str())
            .unwrap_or("空通道");
        eprintln!("[诊断] 通道 {source}：{} 条候选", channel.len());
    }
}

/// 诊断：代码块的规模分布。
///
/// 切块粒度直接决定语义向量的区分度——大量三五行的小块彼此高度相似，嵌入后挤在向量
/// 空间的同一片区域，检索时谁排第一近乎随机。先量出分布再决定怎么合并，避免凭感觉调参。
#[test]
fn chunk_size_distribution() {
    let root = repository_root();
    let files = collect_repository_files(&root);
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&file_refs);
    let plugin = indexed_plugin(&host);
    let index = plugin.structural_index().expect("索引应已建成");

    let mut lines: Vec<u32> = Vec::new();
    let mut by_kind: BTreeMap<String, (usize, u64)> = BTreeMap::new();
    let mut without_symbol = 0usize;
    let mut semantic_groups = 0usize;
    let mut grouped_members = 0usize;
    for chunk in index.chunks() {
        if chunk.semantic_only {
            semantic_groups += 1;
            continue;
        }
        if chunk.semantic_group_id.is_some() {
            grouped_members += 1;
        }
        let span = chunk.end_line.saturating_sub(chunk.start_line) + 1;
        lines.push(span);
        let kind = chunk
            .symbol_kind
            .map(|kind| format!("{kind:?}"))
            .unwrap_or_else(|| "文本块".to_string());
        let entry = by_kind.entry(kind).or_insert((0, 0));
        entry.0 += 1;
        entry.1 += u64::from(span);
        if chunk.symbol_name.is_none() {
            without_symbol += 1;
        }
    }
    lines.sort_unstable();

    let percentile = |p: f64| -> u32 {
        let index = ((lines.len() as f64 - 1.0) * p).round() as usize;
        lines[index.min(lines.len() - 1)]
    };
    let tiny = lines.iter().filter(|span| **span <= 3).count();
    let small = lines.iter().filter(|span| **span <= 6).count();
    let semantic_documents = lines.len() + semantic_groups - grouped_members;
    let saved_embeddings = grouped_members.saturating_sub(semantic_groups);

    println!("\n# 代码块规模分布\n");
    println!("| 指标 | 数值 |");
    println!("| --- | --- |");
    println!("| 原子代码块总数 | {} |", lines.len());
    println!("| 语义复合块 | {semantic_groups} |");
    println!("| 被合并原子块 | {grouped_members} |");
    println!("| 实际语义文档数 | {semantic_documents} |");
    println!(
        "| 减少嵌入请求 | {saved_embeddings}（{:.1}%） |",
        saved_embeddings as f64 / lines.len() as f64 * 100.0
    );
    println!("| 无符号名（文本兜底） | {without_symbol} |");
    println!("| 行数 P50 | {} |", percentile(0.50));
    println!("| 行数 P75 | {} |", percentile(0.75));
    println!("| 行数 P90 | {} |", percentile(0.90));
    println!("| 行数 P99 | {} |", percentile(0.99));
    println!("| 最大行数 | {} |", lines.last().copied().unwrap_or(0));
    println!(
        "| ≤3 行 | {tiny}（{:.1}%） |",
        tiny as f64 / lines.len() as f64 * 100.0
    );
    println!(
        "| ≤6 行 | {small}（{:.1}%） |",
        small as f64 / lines.len() as f64 * 100.0
    );

    println!("\n| 符号种类 | 数量 | 平均行数 |");
    println!("| --- | --- | --- |");
    for (kind, (count, total)) in &by_kind {
        println!(
            "| {kind} | {count} | {:.1} |",
            *total as f64 / *count as f64
        );
    }
}

/// 诊断：中文行为描述类查询的实际返回，逐条列出前五名。
///
/// 这类查询的 Recall@10 已达 0.938 而 Recall@1 仅 0.12——目标被召回却排不到第一。
/// 聚合指标看不出"第一名到底是什么"，必须逐条看排在目标前面的是哪些块，才能判断
/// 该改切块、改嵌入文档，还是改排序。
#[test]
#[ignore = "需要网络与嵌入 API 密钥"]
fn chinese_query_failure_analysis() {
    let api_key = std::env::var(API_KEY_ENV).expect("需要 API 密钥");
    let root = repository_root();
    let files = collect_repository_files(&root);
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&file_refs).with_http_handler(forward_to_endpoint(api_key));
    let ablation = Ablation {
        label: "语义",
        semantic: true,
        rerank: false,
    };
    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(&host, ablation_context(&root.to_string_lossy(), &ablation))
        .expect("激活插件");
    for _ in 0..400 {
        let call = ToolCall::new("diag", "code_search", json!({"query": "初始化索引"}));
        let _ = plugin.call_tool_with_host(&host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            break;
        }
    }
    assert_eq!(
        plugin.index_status(),
        IndexStatus::Ready,
        "语义索引必须就绪"
    );

    println!("\n# 中文查询失败分析\n");
    for query in eval_queries()
        .iter()
        .filter(|query| query.kind == QueryKind::ChineseBehavior)
    {
        let results = plugin.search_results(&host, query.query, None, 5);
        let outcome = evaluate(&results, query);
        println!(
            "## {}（目标 {:?}，名次 {:?}）\n",
            query.query, query.expected_paths, outcome.file_rank
        );
        for (rank, result) in results.iter().enumerate() {
            println!(
                "{}. `{}:{}-{}` {} — {}",
                rank + 1,
                result.file_path,
                result.start_line,
                result.end_line,
                result.symbol_name.as_deref().unwrap_or("（文本块）"),
                result.debug.reason()
            );
        }
        println!();
    }
}

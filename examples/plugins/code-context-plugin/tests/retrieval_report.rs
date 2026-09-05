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
    build_context,
    testing::{activation_context, collect_repository_files, repository_root, RepoHost},
    CodeContextPlugin, ContextBundle, ContextConfig, IndexStatus, RepositoryIdentity, SearchResult,
};

use agent_plugin::{AgentPlugin, ToolCall};
use serde_json::json;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

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

/// 判断文件是否属于评测答案或评测结果，必须从被测语料中排除。
fn is_evaluation_artifact(path: &str) -> bool {
    matches!(
        path,
        "examples/plugins/code-context-plugin/tests/retrieval_report.rs"
            | "examples/plugins/code-context-plugin/tests/retrieval_benchmark.rs"
            | "docs/code-context-engine/PROGRESS.md"
    ) || path.starts_with("docs/code-context-engine/retrieval-acceptance-")
}

/// 收集真实仓库语料，并排除包含查询、答案和指标的评测产物。
///
/// 不做这一步时，评测脚本中的查询文本与目标符号会被同一个索引读到，生成的报告也会反过来
/// 出现在 Top-10，导致指标随报告内容虚高而不是反映产品检索能力。
fn collect_evaluation_files(root: &std::path::Path) -> Vec<(String, String)> {
    collect_repository_files(root)
        .into_iter()
        .filter(|(path, _)| !is_evaluation_artifact(path))
        .collect()
}

/// 一条面向真实使用场景的业务式验收查询。
///
/// 查询刻意不包含目标符号名，模拟用户只描述“要找什么实现”时的输入。目标路径与符号共同
/// 约束实现本体，避免把同文件内的调用点或测试代码误算为成功。
struct AcceptanceQuery {
    /// 截图式报告中的业务类别。
    category: &'static str,
    /// 提交给检索链路的中文行为描述。
    query: &'static str,
    /// 目标实现所在文件的路径子串。
    expected_paths: &'static [&'static str],
    /// 目标实现符号；命中同名调用点不算成功。
    expected_symbols: &'static [&'static str],
}

/// 返回覆盖八类常见代码定位任务的 Lucia 业务式查询。
fn acceptance_queries() -> Vec<AcceptanceQuery> {
    vec![
        AcceptanceQuery {
            category: "实体类定义",
            query: "代码搜索请求参数实体在哪里定义",
            expected_paths: &["code-context-plugin/src/tool.rs"],
            expected_symbols: &["CodeSearchRequest"],
        },
        AcceptanceQuery {
            category: "页面组件",
            query: "插件管理页面怎么绘制",
            expected_paths: &["agent-tui/src/command/render.rs"],
            expected_symbols: &["render_native_plugin_dialog"],
        },
        AcceptanceQuery {
            category: "鉴权过滤器",
            query: "工具调用在什么情况下需要用户审批",
            expected_paths: &["permission-plugin/src/lib.rs"],
            expected_symbols: &["requires_approval"],
        },
        AcceptanceQuery {
            category: "状态下钻功能",
            query: "代码索引进度怎么发布到终端底栏",
            expected_paths: &["code-context-plugin/src/lib.rs"],
            expected_symbols: &["emit_index_progress"],
        },
        AcceptanceQuery {
            category: "选择器组件",
            query: "思考等级选择面板怎么渲染",
            expected_paths: &["agent-tui/src/command/render.rs"],
            expected_symbols: &["render_native_reasoning_panel"],
        },
        AcceptanceQuery {
            category: "工具函数",
            query: "驼峰命名的标识符如何拆成可检索的词",
            expected_paths: &["code-context-plugin/src/retrieval/tokenize.rs"],
            expected_symbols: &["split_camel_case"],
        },
        AcceptanceQuery {
            category: "控制器",
            query: "仓库代码搜索请求如何执行并打包代码片段",
            expected_paths: &["code-context-plugin/src/lib.rs"],
            expected_symbols: &["search"],
        },
        AcceptanceQuery {
            category: "刷新接口",
            query: "文件修改后如何增量刷新代码索引",
            expected_paths: &["code-context-plugin/src/lib.rs"],
            expected_symbols: &["advance_index"],
        },
    ]
}

// ===========================================================================
// 指标
// ===========================================================================

/// 单条查询的评测结果。
#[derive(Clone)]
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

/// 判断结果路径是否属于目标实现文件。
fn matches_target_path(file_path: &str, expected_paths: &[&str]) -> bool {
    expected_paths
        .iter()
        .any(|pattern| file_path.contains(pattern))
}

/// 判断结果符号是否为目标定义，而不是同文件中的调用点。
fn matches_target_symbol(
    symbol_name: Option<&str>,
    qualified_symbol: Option<&str>,
    expected_symbols: &[&str],
) -> bool {
    expected_symbols
        .iter()
        .any(|symbol| symbol_name == Some(*symbol) || qualified_symbol == Some(*symbol))
}

/// 按目标路径与符号对一组候选求值。
fn evaluate_target(
    results: &[SearchResult],
    expected_paths: &[&str],
    expected_symbols: &[&str],
) -> QueryOutcome {
    let file_rank = results
        .iter()
        .position(|result| matches_target_path(&result.file_path, expected_paths));
    // 定义命中要求同时落在目标文件里：同名符号在别处出现不算找到定义。
    let symbol_rank = results.iter().position(|result| {
        let in_target = matches_target_path(&result.file_path, expected_paths);
        in_target
            && matches_target_symbol(
                result.symbol_name.as_deref(),
                result.qualified_symbol.as_deref(),
                expected_symbols,
            )
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
        checks_symbol: !expected_symbols.is_empty(),
        top,
    }
}

/// 对一条标准评测查询求值。
fn evaluate(results: &[SearchResult], query: &EvalQuery) -> QueryOutcome {
    evaluate_target(results, query.expected_paths, query.expected_symbols)
}

/// 一次用户可见上下文构建的质量结果。
#[derive(Clone, Copy, Default)]
struct ContextOutcome {
    /// 是否返回了至少一段有效源码。
    has_snippet: bool,
    /// 目标定义是否带有效源码返回。
    implementation_hit: bool,
    /// 目标定义是否完整覆盖，且没有因行数上限被截断。
    complete_implementation: bool,
    /// 返回片段对目标定义行区间的覆盖率。
    target_coverage: f64,
    /// 最终返回的上下文条目数。
    returned: usize,
    /// 上下文构建器估算的 token 数。
    estimated_tokens: u32,
}

/// 计算两个闭区间的行覆盖率；目标区间非法时返回 0。
fn line_coverage(target_start: u32, target_end: u32, actual_start: u32, actual_end: u32) -> f64 {
    if target_start == 0 || target_end < target_start || actual_end < actual_start {
        return 0.0;
    }
    let overlap_start = target_start.max(actual_start);
    let overlap_end = target_end.min(actual_end);
    if overlap_end < overlap_start {
        return 0.0;
    }
    let overlap = overlap_end.saturating_sub(overlap_start).saturating_add(1);
    let target = target_end.saturating_sub(target_start).saturating_add(1);
    f64::from(overlap) / f64::from(target)
}

/// 评估用户可见上下文是否真正包含目标实现本体。
fn evaluate_context(
    results: &[SearchResult],
    bundle: &ContextBundle,
    expected_paths: &[&str],
    expected_symbols: &[&str],
) -> ContextOutcome {
    let target_result = results.iter().find(|result| {
        matches_target_path(&result.file_path, expected_paths)
            && matches_target_symbol(
                result.symbol_name.as_deref(),
                result.qualified_symbol.as_deref(),
                expected_symbols,
            )
    });
    let target_entry = bundle.entries.iter().find(|entry| {
        matches_target_path(&entry.file_path, expected_paths)
            && matches_target_symbol(
                entry.symbol_name.as_deref(),
                entry.qualified_symbol.as_deref(),
                expected_symbols,
            )
    });
    let implementation_hit = target_entry.is_some_and(|entry| {
        !entry.snippet.trim().is_empty()
            && entry.start_line > 0
            && entry.end_line >= entry.start_line
    });
    let target_coverage = match (target_result, target_entry) {
        (Some(result), Some(entry)) => line_coverage(
            result.start_line,
            result.end_line,
            entry.start_line,
            entry.end_line,
        ),
        _ => 0.0,
    };
    let complete_implementation = implementation_hit
        && target_entry.is_some_and(|entry| !entry.truncated)
        && target_coverage >= 1.0;

    ContextOutcome {
        has_snippet: bundle.entries.iter().any(|entry| {
            !entry.snippet.trim().is_empty()
                && entry.start_line > 0
                && entry.end_line >= entry.start_line
        }),
        implementation_hit,
        complete_implementation,
        target_coverage,
        returned: bundle.entries.len(),
        estimated_tokens: bundle.stats.estimated_tokens,
    }
}

/// 一次业务查询运行的排名、上下文与耗时记录。
struct AcceptanceSample {
    /// Top-10 排名质量。
    ranking: QueryOutcome,
    /// 用户可见代码片段质量。
    context: ContextOutcome,
    /// Top-10 chunk 身份序列，用于验证可复现性。
    order: Vec<String>,
    /// 检索与上下文打包的端到端耗时，单位毫秒。
    latency_ms: f64,
}

/// 同一业务查询的多轮运行记录。
struct AcceptanceMeasurement {
    /// 固定次数的重复样本。
    samples: Vec<AcceptanceSample>,
}

impl AcceptanceMeasurement {
    /// 返回第一轮样本；评测固定至少运行一次。
    fn first(&self) -> &AcceptanceSample {
        self.samples.first().expect("业务查询至少运行一次")
    }

    /// 判断各轮 Top-1 身份是否完全一致。
    fn top_1_stable(&self) -> bool {
        let expected = self.first().order.first();
        self.samples
            .iter()
            .all(|sample| sample.order.first() == expected)
    }

    /// 判断各轮完整 Top-10 身份与顺序是否完全一致。
    fn full_order_stable(&self) -> bool {
        let expected = &self.first().order;
        self.samples.iter().all(|sample| sample.order == *expected)
    }

    /// 返回该查询多轮运行的平均耗时。
    fn average_latency_ms(&self) -> f64 {
        mean(self.samples.iter().map(|sample| sample.latency_ms))
    }

    /// 返回该查询多轮运行的 P95 耗时。
    fn p95_latency_ms(&self) -> f64 {
        percentile(
            &self
                .samples
                .iter()
                .map(|sample| sample.latency_ms)
                .collect::<Vec<_>>(),
            0.95,
        )
    }

    /// 返回逐题表中的成功、部分成功或失败判定。
    fn verdict(&self) -> &'static str {
        let first = self.first();
        if first.ranking.hit_at(1) && first.context.complete_implementation {
            "命中"
        } else if first.ranking.hit_at(10) || first.context.implementation_hit {
            "部分命中"
        } else {
            "未命中"
        }
    }

    /// 返回可直接指导后续调优的判定原因。
    fn reason(&self) -> String {
        let first = self.first();
        if first.ranking.hit_at(1) && first.context.complete_implementation {
            return "目标定义位于第一名，正文完整".to_string();
        }
        if first.ranking.file_rank.is_none() {
            return "目标文件未进入 Top-10".to_string();
        }
        if first.ranking.symbol_rank.is_none() {
            return format!(
                "目标文件位于第 {} 名，但定义未返回",
                first.ranking.file_rank.expect("已确认文件命中")
            );
        }
        if !first.context.implementation_hit {
            return format!(
                "定义位于第 {} 名，但上下文未返回实现正文",
                first.ranking.symbol_rank.expect("已确认定义命中")
            );
        }
        if !first.context.complete_implementation {
            return format!(
                "实现正文覆盖 {:.0}%，存在截断或区间缺失",
                first.context.target_coverage * 100.0
            );
        }
        format!(
            "目标实现位于第 {} 名，尚未排到第一",
            first.ranking.symbol_rank.expect("已确认定义命中")
        )
    }
}

/// 业务验收的重复运行次数。
const ACCEPTANCE_REPEATS: usize = 3;

/// 对一组有限浮点数求平均值；空输入返回 0。
fn mean(values: impl Iterator<Item = f64>) -> f64 {
    let values = values.collect::<Vec<_>>();
    if values.is_empty() {
        return 0.0;
    }
    values.iter().sum::<f64>() / values.len() as f64
}

/// 使用 nearest-rank 规则计算分位数；`p` 会收敛到 0 到 1。
fn percentile(values: &[f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let p = p.clamp(0.0, 1.0);
    let rank = (p * sorted.len() as f64).ceil().max(1.0) as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// 重复运行八类业务查询，并测量实际检索与上下文打包链路。
///
/// `candidate_limit` 应与工具实际交给上下文层的候选数一致：启用远端重排时为 40，其他配置
/// 为 10。这样指标同时覆盖重排后的候选补位和最终返回上限。
fn measure_acceptance(
    plugin: &mut CodeContextPlugin,
    host: &RepoHost,
    queries: &[AcceptanceQuery],
    candidate_limit: usize,
) -> Vec<AcceptanceMeasurement> {
    queries
        .iter()
        .map(|query| {
            let samples = (0..ACCEPTANCE_REPEATS)
                .map(|_| {
                    let started = Instant::now();
                    let results = plugin.search_results(host, query.query, None, candidate_limit);
                    let ranking = evaluate_target(
                        &results[..results.len().min(RESULT_LIMIT)],
                        query.expected_paths,
                        query.expected_symbols,
                    );
                    let order = results
                        .iter()
                        .take(RESULT_LIMIT)
                        .map(|result| result.chunk_id.clone())
                        .collect();
                    let bundle = build_context(
                        host,
                        plugin.structural_index().expect("索引应已建成"),
                        results.clone(),
                        &ContextConfig {
                            max_results: RESULT_LIMIT,
                            ..ContextConfig::default()
                        },
                    );
                    let context = evaluate_context(
                        &results,
                        &bundle,
                        query.expected_paths,
                        query.expected_symbols,
                    );
                    AcceptanceSample {
                        ranking,
                        context,
                        order,
                        latency_ms: started.elapsed().as_secs_f64() * 1_000.0,
                    }
                })
                .collect();
            AcceptanceMeasurement { samples }
        })
        .collect()
}

/// 八类业务验收的汇总指标。
#[derive(Default)]
struct AcceptanceMetrics {
    /// 查询总数。
    queries: usize,
    /// 目标文件进入 Top-10 的数量。
    hits: usize,
    /// 目标定义进入 Top-10 的数量。
    definition_hits: usize,
    /// 用户可见结果包含目标实现正文的数量。
    implementation_hits: usize,
    /// 用户可见结果完整覆盖目标实现的数量。
    complete_implementations: usize,
    /// 至少返回一段源码的查询数量。
    snippet_queries: usize,
    /// Top-1 可复现的查询数量。
    stable_top_1: usize,
    /// 完整 Top-10 身份与顺序可复现的查询数量。
    stable_full_order: usize,
    /// 所有重复运行的平均耗时。
    average_latency_ms: f64,
    /// 所有重复运行的 P50 耗时。
    p50_latency_ms: f64,
    /// 所有重复运行的 P95 耗时。
    p95_latency_ms: f64,
    /// 第一轮平均返回上下文条目数。
    average_returned: f64,
    /// 第一轮目标实现平均行覆盖率。
    average_coverage: f64,
    /// 第一轮上下文平均估算 token 数。
    average_tokens: f64,
}

impl AcceptanceMetrics {
    /// 从逐题重复测量中汇总业务验收指标。
    fn from_measurements(measurements: &[AcceptanceMeasurement]) -> Self {
        if measurements.is_empty() {
            return Self::default();
        }
        let latencies = measurements
            .iter()
            .flat_map(|measurement| measurement.samples.iter().map(|sample| sample.latency_ms))
            .collect::<Vec<_>>();
        Self {
            queries: measurements.len(),
            hits: measurements
                .iter()
                .filter(|measurement| measurement.first().ranking.hit_at(10))
                .count(),
            definition_hits: measurements
                .iter()
                .filter(|measurement| measurement.first().ranking.symbol_hit())
                .count(),
            implementation_hits: measurements
                .iter()
                .filter(|measurement| measurement.first().context.implementation_hit)
                .count(),
            complete_implementations: measurements
                .iter()
                .filter(|measurement| measurement.first().context.complete_implementation)
                .count(),
            snippet_queries: measurements
                .iter()
                .filter(|measurement| measurement.first().context.has_snippet)
                .count(),
            stable_top_1: measurements
                .iter()
                .filter(|measurement| measurement.top_1_stable())
                .count(),
            stable_full_order: measurements
                .iter()
                .filter(|measurement| measurement.full_order_stable())
                .count(),
            average_latency_ms: mean(latencies.iter().copied()),
            p50_latency_ms: percentile(&latencies, 0.50),
            p95_latency_ms: percentile(&latencies, 0.95),
            average_returned: mean(
                measurements
                    .iter()
                    .map(|measurement| measurement.first().context.returned as f64),
            ),
            average_coverage: mean(
                measurements
                    .iter()
                    .map(|measurement| measurement.first().context.target_coverage),
            ),
            average_tokens: mean(
                measurements
                    .iter()
                    .map(|measurement| f64::from(measurement.first().context.estimated_tokens)),
            ),
        }
    }

    /// 返回计数指标的百分比展示文本。
    fn rate(&self, count: usize) -> String {
        if self.queries == 0 {
            return "—".to_string();
        }
        format!(
            "{count}/{} ({:.1}%)",
            self.queries,
            count as f64 / self.queries as f64 * 100.0
        )
    }
}

/// 写入八类业务查询的汇总指标。
fn write_acceptance_overview(report: &mut String, metrics: &AcceptanceMetrics) {
    writeln!(report, "## 八类业务式验收\n").expect("写入报告");
    writeln!(
        report,
        "每条查询重复运行 {ACCEPTANCE_REPEATS} 次，延迟包含检索、可选远端重排与上下文打包。\n"
    )
    .expect("写入报告");
    writeln!(report, "| 指标 | 数值 |").expect("写入报告");
    writeln!(report, "| --- | --- |").expect("写入报告");
    writeln!(report, "| 业务查询数 | {} |", metrics.queries).expect("写入报告");
    writeln!(
        report,
        "| 命中率（目标文件进入 Top-10） | {} |",
        metrics.rate(metrics.hits)
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 定义命中率 | {} |",
        metrics.rate(metrics.definition_hits)
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 实现本体命中率 | {} |",
        metrics.rate(metrics.implementation_hits)
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 完整实现覆盖率 | {} |",
        metrics.rate(metrics.complete_implementations)
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 返回有效代码片段 | {} |",
        metrics.rate(metrics.snippet_queries)
    )
    .expect("写入报告");
    writeln!(
        report,
        "| Top-1 完全一致率 | {} |",
        metrics.rate(metrics.stable_top_1)
    )
    .expect("写入报告");
    writeln!(
        report,
        "| Top-10 身份与顺序完全一致率 | {} |",
        metrics.rate(metrics.stable_full_order)
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 响应时间平均值 | {:.3} ms |",
        metrics.average_latency_ms
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 响应时间 P50 | {:.3} ms |",
        metrics.p50_latency_ms
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 响应时间 P95 | {:.3} ms |",
        metrics.p95_latency_ms
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 平均返回上下文条目 | {:.2} |",
        metrics.average_returned
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 目标实现平均行覆盖 | {:.1}% |",
        metrics.average_coverage * 100.0
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 平均上下文估算 token | {:.0} |",
        metrics.average_tokens
    )
    .expect("写入报告");
}

/// 写入八类业务查询的逐题结果。
fn write_acceptance_details(
    report: &mut String,
    heading: &str,
    queries: &[AcceptanceQuery],
    measurements: &[AcceptanceMeasurement],
) {
    writeln!(report, "\n### {heading}\n").expect("写入报告");
    writeln!(
        report,
        "| 类别 | 查询 | 判定 | 文件名次 | 定义名次 | 实现正文 | 行覆盖 | Top-1 稳定 | 全序稳定 | 平均/P95 | 前三结果 | 原因 |"
    )
    .expect("写入报告");
    writeln!(
        report,
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
    )
    .expect("写入报告");
    for (query, measurement) in queries.iter().zip(measurements) {
        let first = measurement.first();
        writeln!(
            report,
            "| {} | {} | {} | {} | {} | {} | {:.0}% | {} | {} | {:.3}/{:.3} ms | {} | {} |",
            query.category,
            query.query,
            measurement.verdict(),
            first
                .ranking
                .file_rank
                .map_or("—".to_string(), |rank| rank.to_string()),
            first
                .ranking
                .symbol_rank
                .map_or("—".to_string(), |rank| rank.to_string()),
            if first.context.implementation_hit {
                if first.context.complete_implementation {
                    "完整"
                } else {
                    "部分"
                }
            } else {
                "未返回"
            },
            first.context.target_coverage * 100.0,
            if measurement.top_1_stable() {
                "是"
            } else {
                "否"
            },
            if measurement.full_order_stable() {
                "是"
            } else {
                "否"
            },
            measurement.average_latency_ms(),
            measurement.p95_latency_ms(),
            first.ranking.top.join("、"),
            measurement.reason(),
        )
        .expect("写入报告");
    }
}

/// 写入当前实现能够提供与明确不支持的附带信息。
fn write_capability_boundary(report: &mut String) {
    writeln!(report, "\n## 附带信息能力边界\n").expect("写入报告");
    writeln!(report, "| 能力 | 当前状态 | 验收方式 |").expect("写入报告");
    writeln!(report, "| --- | --- | --- |").expect("写入报告");
    writeln!(report, "| 文件、行区间、符号、签名 | 支持 | 逐条结果字段 |").expect("写入报告");
    writeln!(
        report,
        "| 实际代码片段 | 支持 | 非空正文、合法行区间与截断标记 |"
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 实现本体而非使用点 | 支持并量化 | 目标路径 + 目标符号 + 正文同时命中 |"
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 调用链 | 不支持 | 当前没有符号引用图或 Call Hierarchy 索引 |"
    )
    .expect("写入报告");
    writeln!(
        report,
        "| 影响面 | 不支持 | 当前没有 references、依赖传播或代码图谱 |"
    )
    .expect("写入报告");
}

/// 分位数必须使用稳定的 nearest-rank 规则，并正确处理空样本。
#[test]
fn latency_percentile_uses_nearest_rank() {
    assert_eq!(percentile(&[], 0.95), 0.0);
    assert_eq!(percentile(&[4.0, 1.0, 3.0, 2.0], 0.50), 2.0);
    assert_eq!(percentile(&[4.0, 1.0, 3.0, 2.0], 0.95), 4.0);
}

/// Top-1 稳定与完整排序稳定必须分开统计。
#[test]
fn reproducibility_distinguishes_top_1_and_full_order() {
    let sample = |order: &[&str]| AcceptanceSample {
        ranking: QueryOutcome {
            file_rank: Some(1),
            symbol_rank: Some(1),
            checks_symbol: true,
            top: Vec::new(),
        },
        context: ContextOutcome::default(),
        order: order.iter().map(|value| (*value).to_string()).collect(),
        latency_ms: 1.0,
    };
    let same_top = AcceptanceMeasurement {
        samples: vec![sample(&["a", "b"]), sample(&["a", "c"])],
    };
    assert!(same_top.top_1_stable());
    assert!(!same_top.full_order_stable());

    let changed_top = AcceptanceMeasurement {
        samples: vec![sample(&["a", "b"]), sample(&["c", "b"])],
    };
    assert!(!changed_top.top_1_stable());
}

/// 行覆盖率必须按目标定义区间计算，不能把任意非空片段算成完整实现。
#[test]
fn implementation_line_coverage_tracks_partial_context() {
    assert_eq!(line_coverage(10, 19, 10, 19), 1.0);
    assert_eq!(line_coverage(10, 19, 10, 14), 0.5);
    assert_eq!(line_coverage(10, 19, 20, 25), 0.0);
}

/// 查询定义、答案和生成报告必须排除，普通产品源码仍进入语料。
#[test]
fn evaluation_artifacts_are_excluded_from_corpus() {
    assert!(is_evaluation_artifact(
        "examples/plugins/code-context-plugin/tests/retrieval_report.rs"
    ));
    assert!(is_evaluation_artifact(
        "docs/code-context-engine/retrieval-acceptance-2026-09-04.md"
    ));
    assert!(!is_evaluation_artifact(
        "examples/plugins/code-context-plugin/src/retrieval/fusion.rs"
    ));
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
/// 启用远端重排时交给上下文层的候选数，与生产工具的四倍扩展规则一致。
const RERANK_CONTEXT_CANDIDATE_LIMIT: usize = 40;

/// 生成报告：逐类型汇总 + 逐题明细。
#[test]
fn retrieval_quality_report() {
    let root = repository_root();
    let files = collect_evaluation_files(&root);
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
    let acceptance_queries = acceptance_queries();
    let acceptance_measurements =
        measure_acceptance(&mut plugin, &host, &acceptance_queries, RESULT_LIMIT);
    let acceptance_metrics = AcceptanceMetrics::from_measurements(&acceptance_measurements);

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

    write_acceptance_overview(&mut report, &acceptance_metrics);
    write_acceptance_details(
        &mut report,
        "逐题结果",
        &acceptance_queries,
        &acceptance_measurements,
    );

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
            .map(|index| outcomes[*index].clone())
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

    write_capability_boundary(&mut report);

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
/// 真实消融请求的最小间隔，单位毫秒。
const EMBEDDING_REQUEST_INTERVAL_MS: u64 = 1_500;
/// 响应缓存已预热时的请求间隔，单位毫秒。
///
/// 缓存命中不产生任何真实请求，再按限流节奏休眠会让一轮评测多花四分钟。
const CACHED_REQUEST_INTERVAL_MS: u64 = 0;
/// 单批真实消融请求允许的重试次数。
const EMBEDDING_MAX_RETRIES: u32 = 4;
/// 真实消融瞬时错误的指数退避基数，单位毫秒。
const EMBEDDING_RETRY_BASE_MS: u64 = 5_000;
/// 默认重排模型。
const RERANK_MODEL: &str = "Qwen/Qwen3-Reranker-4B";
/// 读取 API 密钥的环境变量名。
const API_KEY_ENV: &str = "SILICONFLOW_API_KEY";
/// 真实评测向量缓存相对用户主目录的位置。
const REPORT_CACHE_SUBDIRECTORY: &str = ".lucia/code-context/retrieval-report";

/// 返回可跨进程续建的真实评测 sqlite-vec 路径。
///
/// 路径只包含仓库身份摘要与固定场景名，不泄露本地绝对路径。没有主目录信息时退回系统临时
/// 目录；不同评测场景使用独立数据库，避免并行执行忽略测试时争用同一连接。
fn retrieval_report_vector_path(root: &Path, scenario: &str) -> PathBuf {
    let identity = RepositoryIdentity::from_workspace_root(&root.to_string_lossy())
        .expect("真实评测仓库根必须有效");
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(REPORT_CACHE_SUBDIRECTORY)
        .join(format!("{}-{scenario}.sqlite3", identity.as_str()))
}

/// 一次消融配置。
struct Ablation {
    /// 报告中展示的名称。
    label: &'static str,
    /// 是否启用语义通道。
    semantic: bool,
    /// 是否启用模型重排。
    rerank: bool,
}

/// 一组消融配置的质量、分类与业务验收结果。
struct AblationReportRow {
    /// 报告中展示的配置名称。
    label: &'static str,
    /// 32 条标准查询的总体指标。
    metrics: Metrics,
    /// 标准查询按类型分组后的指标。
    by_kind: BTreeMap<QueryKind, Metrics>,
    /// 八类业务查询的逐题重复测量。
    acceptance: Vec<AcceptanceMeasurement>,
    /// 八类业务查询的汇总指标。
    acceptance_metrics: AcceptanceMetrics,
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
        context.metadata.insert(
            "embedding_request_interval_ms".into(),
            if std::env::var("RETRIEVAL_CACHE").as_deref() == Ok("warm") {
                CACHED_REQUEST_INTERVAL_MS.to_string()
            } else {
                EMBEDDING_REQUEST_INTERVAL_MS.to_string()
            },
        );
        context.metadata.insert(
            "embedding_max_retries".into(),
            EMBEDDING_MAX_RETRIES.to_string(),
        );
        context.metadata.insert(
            "embedding_retry_base_ms".into(),
            EMBEDDING_RETRY_BASE_MS.to_string(),
        );
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
    let cache_dir = response_cache_dir();
    let agent = ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(180)))
            .build(),
    );
    move |request: &agent_plugin::HttpRequest| {
        let cache_key = cache_dir
            .as_ref()
            .map(|dir| dir.join(format!("{}.json", request_fingerprint(request))));
        if let Some(hit) = cache_key.as_ref().and_then(|path| read_cached(path)) {
            return Ok(hit);
        }
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
        // 只保留自动重试需要的非敏感响应头。
        let headers = response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok())
            .map(|value| {
                std::collections::HashMap::from([("retry-after".to_string(), value.to_string())])
            })
            .unwrap_or_default();
        let body = response
            .into_body()
            .into_with_config()
            .limit(32 * 1024 * 1024)
            .read_to_string()
            .map_err(|error| anyhow::anyhow!("读取端点响应失败：{error}"))?;
        let response = agent_plugin::HttpResponse {
            status,
            headers,
            body,
        };
        // 只缓存成功响应：限流与服务端错误必须让插件自己走重试逻辑。
        if status == 200 {
            if let Some(path) = cache_key.as_ref() {
                write_cached(path, &response);
            }
        }
        Ok(response)
    }
}

/// 端点响应的磁盘缓存目录，返回 `None` 表示本轮不缓存。
///
/// 嵌入与重排请求对同一输入是确定性的，缓存后重复评测不再重算整仓库向量，
/// 权重调参从十分钟一轮降到秒级。设 `RETRIEVAL_CACHE=off` 可强制走真实端点。
fn response_cache_dir() -> Option<std::path::PathBuf> {
    if std::env::var("RETRIEVAL_CACHE").as_deref() == Ok("off") {
        return None;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("retrieval-response-cache");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// 用请求路径与请求体的哈希作为缓存键。
fn request_fingerprint(request: &agent_plugin::HttpRequest) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(request.path.as_bytes());
    hasher.update([0u8]);
    hasher.update(request.body.as_deref().unwrap_or_default().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 读取缓存的响应，文件缺失或损坏都视为未命中。
fn read_cached(path: &std::path::Path) -> Option<agent_plugin::HttpResponse> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    Some(agent_plugin::HttpResponse {
        status: value.get("status")?.as_u64()? as u16,
        headers: std::collections::HashMap::new(),
        body: value.get("body")?.as_str()?.to_string(),
    })
}

/// 写入缓存，失败只影响下轮命中率，不影响本轮结果。
fn write_cached(path: &std::path::Path, response: &agent_plugin::HttpResponse) {
    let value = serde_json::json!({ "status": response.status, "body": response.body });
    if let Ok(raw) = serde_json::to_string(&value) {
        let _ = std::fs::write(path, raw);
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
    let files = collect_evaluation_files(&root);
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let queries = eval_queries();
    let acceptance_queries = acceptance_queries();
    let vector_path = retrieval_report_vector_path(&root, "ablation");
    eprintln!("[消融] 语义向量缓存：{}", vector_path.display());

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

    let mut rows = Vec::new();
    for ablation in &ablations {
        eprintln!("[消融] 正在运行配置：{}", ablation.label);
        let host =
            RepoHost::new(&file_refs).with_http_handler(forward_to_endpoint(api_key.clone()));
        let mut plugin = if ablation.semantic {
            CodeContextPlugin::default().with_sqlite_vector_database(&vector_path)
        } else {
            CodeContextPlugin::default()
        };
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
        let context_candidate_limit = if ablation.rerank {
            RERANK_CONTEXT_CANDIDATE_LIMIT
        } else {
            RESULT_LIMIT
        };
        let acceptance = measure_acceptance(
            &mut plugin,
            &host,
            &acceptance_queries,
            context_candidate_limit,
        );
        let acceptance_metrics = AcceptanceMetrics::from_measurements(&acceptance);
        rows.push(AblationReportRow {
            label: ablation.label,
            metrics: Metrics::from_outcomes(&outcomes),
            by_kind,
            acceptance,
            acceptance_metrics,
        });
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
        "| 配置 | Recall@1 | Recall@5 | Recall@10 | MRR | 定义命中 | 业务命中 | 实现本体 | Top-1 稳定 | 全序稳定 | 平均/P50/P95 |"
    )
    .expect("写入报告");
    writeln!(
        report,
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
    )
    .expect("写入报告");
    for row in &rows {
        writeln!(
            report,
            "| {} | {:.3} | {:.3} | {:.3} | {:.3} | {} | {} | {} | {} | {} | {:.3}/{:.3}/{:.3} ms |",
            row.label,
            row.metrics.recall_1,
            row.metrics.recall_5,
            row.metrics.recall_10,
            row.metrics.mrr,
            row.metrics.definition_rate(),
            row.acceptance_metrics.rate(row.acceptance_metrics.hits),
            row.acceptance_metrics
                .rate(row.acceptance_metrics.implementation_hits),
            row.acceptance_metrics
                .rate(row.acceptance_metrics.stable_top_1),
            row.acceptance_metrics
                .rate(row.acceptance_metrics.stable_full_order),
            row.acceptance_metrics.average_latency_ms,
            row.acceptance_metrics.p50_latency_ms,
            row.acceptance_metrics.p95_latency_ms,
        )
        .expect("写入报告");
    }

    writeln!(report, "\n## 按查询类型的 Recall@1\n").expect("写入报告");
    write!(report, "| 查询类型 |").expect("写入报告");
    for row in &rows {
        write!(report, " {} |", row.label).expect("写入报告");
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
        for row in &rows {
            write!(report, " {:.2} |", row.by_kind[&kind].recall_1).expect("写入报告");
        }
        writeln!(report).expect("写入报告");
    }

    writeln!(report, "\n## 八类业务式逐题对比\n").expect("写入报告");
    for row in &rows {
        write_acceptance_details(&mut report, row.label, &acceptance_queries, &row.acceptance);
    }
    write_capability_boundary(&mut report);

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
    let files = collect_evaluation_files(&root);
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
    let vector_path = retrieval_report_vector_path(&root, "semantic-diagnosis");
    let mut plugin = CodeContextPlugin::default().with_sqlite_vector_database(&vector_path);
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
    let files = collect_evaluation_files(&root);
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
    let files = collect_evaluation_files(&root);
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
    let vector_path = retrieval_report_vector_path(&root, "chinese-query-analysis");
    let mut plugin = CodeContextPlugin::default().with_sqlite_vector_database(&vector_path);
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

/// 诊断：语义通道加入后，原本被词法/符号召回的目标丢在了哪一阶段。
///
/// 消融显示业务命中从 8/8 掉到 6/8——"仓库代码搜索请求如何执行并打包代码片段"与
/// "文件修改后如何增量刷新代码索引"在只有符号+词法时分别排第 9 和第 6，加入语义后双双
/// 跌出 Top-10。聚合指标只能说明"变差了"，无法区分是召回器没召回、union 丢失、融合降级
/// 还是最终截断，因此这里逐阶段比对目标 chunk 的存在性与位置。
#[test]
#[ignore = "需要网络与嵌入 API 密钥"]
fn semantic_regression_stage_localization() {
    let api_key = std::env::var(API_KEY_ENV).expect("需要 API 密钥");
    let root = repository_root();
    let files = collect_repository_files(&root);
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();

    // 只关注消融中出现回退的两条业务查询及其期望路径。
    let cases: [(&str, &str); 2] = [
        (
            "仓库代码搜索请求如何执行并打包代码片段",
            "code-context-plugin/src/lib.rs",
        ),
        (
            "文件修改后如何增量刷新代码索引",
            "code-context-plugin/src/lib.rs",
        ),
    ];

    println!("\n# 语义回退阶段定位\n");
    for (label, semantic) in [("符号 + 词法", false), ("+ 语义", true)] {
        let ablation = Ablation {
            label,
            semantic,
            rerank: false,
        };
        let host = RepoHost::new(&file_refs);
        let host = if semantic {
            host.with_http_handler(forward_to_endpoint(api_key.clone()))
        } else {
            host
        };
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(&host, ablation_context(&root.to_string_lossy(), &ablation))
            .expect("激活插件");
        for _ in 0..400 {
            let call = ToolCall::new("stage", "code_search", json!({"query": "初始化索引"}));
            let _ = plugin.call_tool_with_host(&host, call);
            if plugin.index_status() != IndexStatus::Indexing {
                break;
            }
        }

        println!("## {label}\n");
        for (query, expected_path) in cases {
            let execution = plugin.search_execution(&host, query, None, RESULT_LIMIT);
            let stages = &execution.diagnostics.stages;
            let index = plugin.structural_index().expect("索引应已建成");
            // 目标 chunk：期望文件下的全部 chunk，命中任一即算该阶段保住了目标。
            let targets: Vec<&str> = index
                .chunks()
                .filter(|chunk| chunk.file_path.contains(expected_path))
                .map(|chunk| chunk.id.as_str())
                .collect();
            let locate = |stage: &Vec<String>| -> String {
                match stage
                    .iter()
                    .position(|id| targets.iter().any(|target| target == id))
                {
                    Some(position) => format!("第 {} 位 / 共 {}", position + 1, stage.len()),
                    None => format!("缺失 / 共 {}", stage.len()),
                }
            };
            println!("### {query}\n");
            println!(
                "目标文件 `{expected_path}`，该文件共 {} 个 chunk\n",
                targets.len()
            );
            println!("| 阶段 | 目标位置 |");
            println!("| --- | --- |");
            println!("| symbol 通道 | {} |", locate(&stages.symbol));
            println!("| lexical 通道 | {} |", locate(&stages.lexical));
            println!("| semantic 通道 | {} |", locate(&stages.semantic));
            println!("| union | {} |", locate(&stages.union));
            println!("| post_fusion | {} |", locate(&stages.post_fusion));
            println!("| final | {} |", locate(&stages.final_results));
            println!();
            // 只看名次说明不了"被谁挤掉"，把前列的通道来源一并列出。
            println!("融合后前 12 名：\n");
            for (rank, chunk_id) in stages.post_fusion.iter().take(12).enumerate() {
                let chunk = index.chunk(chunk_id);
                let mark = if targets.iter().any(|target| target == chunk_id) {
                    " ← 目标"
                } else {
                    ""
                };
                let reason = execution
                    .results
                    .iter()
                    .find(|result| &result.chunk_id == chunk_id)
                    .map(|result| result.debug.reason())
                    .unwrap_or_else(|| "（已被最终截断，无诊断）".to_string());
                println!(
                    "{}. `{}` {} — {reason}{mark}",
                    rank + 1,
                    chunk.map(|c| c.file_path.as_str()).unwrap_or("?"),
                    chunk
                        .and_then(|c| c.symbol_name.as_deref())
                        .unwrap_or("（文本块）"),
                );
            }
            println!();
        }
    }
}

// ===========================================================================
// 真实使用场景评测
// ===========================================================================

/// 一条模拟真实使用的查询。
///
/// 与 `eval_queries` 的区别是**出发点**：那一组按查询与目标的词面距离分层，用来定位
/// 检索链路的哪一段失效；这一组模拟 Coding Agent 在真实任务里会问什么——改代码前先定位、
/// 排查问题时找实现、理解系统时问机制。措辞刻意保持随意，包含"的函数""怎么弄的"这类
/// 真实但不精确的说法。
struct ScenarioQuery {
    /// 触发这次检索的真实任务。
    task: &'static str,
    /// 实际提交的查询串。
    query: &'static str,
    /// 目标文件路径子串。
    expected_paths: &'static [&'static str],
}

/// 覆盖定位、排查与理解三类任务的真实场景查询。
fn scenario_queries() -> Vec<ScenarioQuery> {
    vec![
        ScenarioQuery {
            task: "改审批逻辑前先定位",
            query: "工具调用的权限是怎么校验的",
            expected_paths: &["agent-core/kernel/src/agent/run.rs"],
        },
        ScenarioQuery {
            task: "加一种只读工具时确认并发规则",
            query: "哪些工具调用可以并发执行",
            expected_paths: &["agent-core/kernel/src/agent/run.rs"],
        },
        ScenarioQuery {
            task: "排查取消没生效",
            query: "取消请求是在哪里被消费的",
            expected_paths: &[
                "agent-core/kernel/src/agent/run.rs",
                "agent-core/runtime/src/runtime/mod.rs",
            ],
        },
        ScenarioQuery {
            task: "理解流式输出",
            query: "模型返回的流式增量怎么处理",
            expected_paths: &["agent-core/kernel/src/model/stream.rs"],
        },
        ScenarioQuery {
            task: "改会话存储",
            query: "会话是怎么保存下来的",
            expected_paths: &["agent-core/session/", "agent-tui/src/session/"],
        },
        ScenarioQuery {
            task: "加新插件能力前看现有实现",
            query: "插件怎么把工具注册给模型",
            expected_paths: &["agent-plugins/host/src/", "agent-plugins/sdk/src/lib.rs"],
        },
        ScenarioQuery {
            task: "排查插件网络请求被拒",
            query: "插件的 HTTP 请求头是怎么过滤的",
            expected_paths: &["agent-plugins/host/src/capability/http.rs"],
        },
        ScenarioQuery {
            task: "理解 WASM 沙箱边界",
            query: "插件能力是怎么被限制的",
            expected_paths: &["agent-plugins/host/src/capability/"],
        },
        ScenarioQuery {
            task: "改索引构建",
            query: "索引是怎么分批构建的",
            expected_paths: &["code-context-plugin/src/index/build.rs"],
        },
        ScenarioQuery {
            task: "排查某文件没被索引",
            query: "怎么决定一个文件要不要索引",
            expected_paths: &["code-context-plugin/src/repository/"],
        },
        ScenarioQuery {
            task: "改忽略规则",
            query: "gitignore 的匹配是怎么实现的",
            expected_paths: &["code-context-plugin/src/repository/ignore.rs"],
        },
        ScenarioQuery {
            task: "理解向量落盘",
            query: "向量存到磁盘上的函数",
            expected_paths: &["code-context-plugin/src/sqlite_vector.rs"],
        },
        ScenarioQuery {
            task: "排查重排超时",
            query: "重排失败之后怎么降级",
            expected_paths: &["code-context-plugin/src/rerank.rs"],
        },
        ScenarioQuery {
            task: "改切块粒度",
            query: "源码是怎么切成代码块的",
            expected_paths: &["code-context-plugin/src/parser/"],
        },
        ScenarioQuery {
            task: "理解检索排序",
            query: "多路召回的结果怎么合成一个排名",
            expected_paths: &["code-context-plugin/src/retrieval/fusion.rs"],
        },
        ScenarioQuery {
            task: "改上下文预算",
            query: "返回给模型的代码片段怎么控制大小",
            expected_paths: &["code-context-plugin/src/context/"],
        },
    ]
}

/// 真实场景评测：模拟 Coding Agent 的实际提问方式。
///
/// 需要网络与 `SILICONFLOW_API_KEY`；默认忽略：
///
/// ```sh
/// SILICONFLOW_API_KEY=sk-... cargo test -p code-context-plugin --features test-support \
///     --test retrieval_report scenario -- --ignored --nocapture
/// ```
#[test]
#[ignore = "需要网络与嵌入 API 密钥"]
fn agent_scenario_report() {
    let api_key = std::env::var(API_KEY_ENV).expect("需要 API 密钥");
    let root = repository_root();
    let files = collect_evaluation_files(&root);
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let queries = scenario_queries();
    // 每档都要为整个仓库重算向量，连续跑多档容易撞上端点的 TPM 限流；默认只跑生产配置，
    // 需要对照时把另一档取消注释单独运行。
    for ablation in [Ablation {
        label: "语义 + 条件重排",
        semantic: true,
        rerank: true,
    }] {
        let host =
            RepoHost::new(&file_refs).with_http_handler(forward_to_endpoint(api_key.clone()));
        let mut plugin = CodeContextPlugin::default();
        plugin
            .activate(&host, ablation_context(&root.to_string_lossy(), &ablation))
            .expect("激活插件");
        for _ in 0..400 {
            let call = ToolCall::new("scenario", "code_search", json!({"query": "初始化索引"}));
            let _ = plugin.call_tool_with_host(&host, call);
            if plugin.index_status() != IndexStatus::Indexing {
                break;
            }
        }
        assert_eq!(
            plugin.index_status(),
            IndexStatus::Ready,
            "语义索引必须就绪，降级原因：{:?}",
            plugin.degraded_reason()
        );
        let mut hits_at_1 = 0usize;
        let mut hits_at_3 = 0usize;
        let mut hits_at_10 = 0usize;
        let mut reciprocal = 0.0f64;
        let mut reranked = 0usize;
        let mut total_ms = 0.0f64;
        let mut rows = Vec::new();

        for query in &queries {
            let started = std::time::Instant::now();
            let execution = plugin.search_execution(&host, query.query, None, 10);
            let millis = started.elapsed().as_secs_f64() * 1000.0;
            total_ms += millis;
            if execution.diagnostics.rerank_activated {
                reranked += 1;
            }
            let rank = execution.results.iter().position(|result| {
                query
                    .expected_paths
                    .iter()
                    .any(|pattern| result.file_path.contains(pattern))
            });
            match rank {
                Some(0) => {
                    hits_at_1 += 1;
                    hits_at_3 += 1;
                    hits_at_10 += 1;
                }
                Some(index) if index < 3 => {
                    hits_at_3 += 1;
                    hits_at_10 += 1;
                }
                Some(_) => hits_at_10 += 1,
                None => {}
            }
            reciprocal += rank.map_or(0.0, |index| 1.0 / (index + 1) as f64);
            let top: Vec<String> = execution
                .results
                .iter()
                .take(3)
                .map(|result| {
                    // 仓库里同名文件（mod.rs、lib.rs）和同名函数都很多，只印文件名无法
                    // 判断命中的到底是哪一处，保留父目录一级。
                    let mut segments = result.file_path.rsplit('/');
                    let file = segments.next().unwrap_or("?");
                    let parent = segments.next().unwrap_or("");
                    format!(
                        "{parent}/{file}:{}",
                        result.symbol_name.as_deref().unwrap_or("（文本块）")
                    )
                })
                .collect();
            rows.push((
                query.task,
                query.query,
                rank,
                execution.diagnostics.path.as_str(),
                execution.diagnostics.rerank_activated,
                millis,
                top.join("、"),
            ));
        }

        let total = queries.len() as f64;
        println!("\n# 真实场景评测 — {}\n", ablation.label);
        println!("| 指标 | 数值 |");
        println!("| --- | --- |");
        println!("| 场景数 | {} |", queries.len());
        println!("| Recall@1 | {:.3} |", hits_at_1 as f64 / total);
        println!("| Recall@3 | {:.3} |", hits_at_3 as f64 / total);
        println!("| Recall@10 | {:.3} |", hits_at_10 as f64 / total);
        println!("| MRR | {:.3} |", reciprocal / total);
        println!(
            "| 重排触发率 | {}/{} ({:.0}%) |",
            reranked,
            queries.len(),
            reranked as f64 / total * 100.0
        );
        println!("| 平均延迟 | {:.1} ms |", total_ms / total);

        println!("\n| 任务 | 查询 | 目标名次 | 路径 | 重排 | 耗时 | 前三结果 |");
        println!("| --- | --- | --- | --- | --- | --- | --- |");
        for (task, query, rank, path, did_rerank, millis, top) in rows {
            println!(
                "| {task} | {query} | {} | {path} | {} | {millis:.0} ms | {top} |",
                rank.map_or("未命中".to_string(), |index| format!("第 {}", index + 1)),
                if did_rerank { "是" } else { "否" },
            );
        }
    }
}

/// 验证 Agent 通过业务描述找到具体定义，而不是仅命中同文件。
///
/// 使用与既有验收相同的八条查询，输出分路、融合与最终位置；真实响应可复用缓存，
/// 但索引必须就绪且语义通道实际运行。`RETRIEVAL_LOCATION_REPORT` 可指定 JSON 报告路径。
#[test]
#[ignore = "需要网络与嵌入 API 密钥"]
fn implementation_location_report() {
    let api_key = std::env::var(API_KEY_ENV).expect("需要 API 密钥");
    let root = repository_root();
    let files = collect_evaluation_files(&root);
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&file_refs).with_http_handler(forward_to_endpoint(api_key));
    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(
            &host,
            ablation_context(
                &root.to_string_lossy(),
                &Ablation {
                    label: "实现定位",
                    semantic: true,
                    rerank: true,
                },
            ),
        )
        .expect("激活插件");
    for _ in 0..400 {
        let call = ToolCall::new("location", "code_search", json!({"query": "初始化索引"}));
        let _ = plugin.call_tool_with_host(&host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            break;
        }
    }
    assert_eq!(
        plugin.index_status(),
        IndexStatus::Ready,
        "语义索引未就绪：{:?}",
        plugin.degraded_reason()
    );
    let mut rows = Vec::new();
    let mut hits = [0usize; 3];
    let mut tool_hits = [0usize; 3];
    for query in acceptance_queries() {
        let execution = plugin.search_execution(&host, query.query, None, 10);
        assert!(
            !execution.diagnostics.stages.semantic.is_empty(),
            "{} 没有语义候选",
            query.query
        );
        assert!(
            execution.diagnostics.rerank_applied,
            "{} 的源码重排未成功",
            query.query
        );
        let index = plugin.structural_index().expect("结构索引");
        let targets: std::collections::HashSet<&str> = index
            .chunks()
            .filter(|chunk| {
                matches_target_path(&chunk.file_path, query.expected_paths)
                    && chunk
                        .symbol_name
                        .as_deref()
                        .is_some_and(|name| query.expected_symbols.contains(&name))
            })
            .map(|chunk| chunk.id.as_str())
            .collect();
        assert!(
            !targets.is_empty(),
            "{} 的预期定义必须真实存在",
            query.query
        );
        let rank_of = |ids: &[String]| {
            ids.iter()
                .position(|id| targets.contains(id.as_str()))
                .map(|i| i + 1)
        };
        let rank = rank_of(&execution.diagnostics.stages.final_results);
        for (count, k) in hits.iter_mut().zip([1, 3, 10]) {
            *count += usize::from(rank.is_some_and(|rank| rank <= k));
        }
        let locations: Vec<_> = execution
            .results
            .iter()
            .map(|result| {
                let source = files
                    .iter()
                    .find(|(path, _)| path == &result.file_path)
                    .expect("返回文件必须存在");
                assert!(result.start_line > 0 && result.start_line <= result.end_line);
                assert!(
                    result.end_line as usize <= source.1.lines().count(),
                    "返回行号不得越界"
                );
                json!({"path": result.file_path, "symbol": result.qualified_symbol,
                "start_line": result.start_line, "end_line": result.end_line,
                "score": result.score, "channels": format!("{:?}", result.debug.ranks)})
            })
            .collect();
        let stages = &execution.diagnostics.stages;
        let mut row = json!({"query": query.query, "rank": rank,
            "symbol_rank": rank_of(&stages.symbol), "lexical_rank": rank_of(&stages.lexical),
            "semantic_rank": rank_of(&stages.semantic), "union_rank": rank_of(&stages.union),
            "fusion_rank": rank_of(&stages.post_fusion), "rerank_applied": execution.diagnostics.rerank_applied,
            "locations": locations});
        // 额外经过真正的工具入口，覆盖扩大候选、去重、正文回填与 token 预算，而非只测内部列表。
        let started = Instant::now();
        let call = ToolCall::new(
            "location-query",
            "code_search",
            json!({"query": query.query, "max_results": 10}),
        );
        let tool = plugin
            .call_tool_with_host(&host, call)
            .expect("工具执行成功");
        assert!(!tool.is_error, "索引就绪后工具不应报错");
        let entries = tool.content["results"]
            .as_array()
            .expect("工具返回位置数组");
        let tool_rank = entries
            .iter()
            .position(|entry| {
                entry["file"]
                    .as_str()
                    .is_some_and(|path| matches_target_path(path, query.expected_paths))
                    && entry["symbol"]
                        .as_str()
                        .is_some_and(|name| query.expected_symbols.contains(&name))
            })
            .map(|i| i + 1);
        for (count, k) in tool_hits.iter_mut().zip([1, 3, 10]) {
            *count += usize::from(tool_rank.is_some_and(|rank| rank <= k));
        }
        let tool_locations: Vec<_> = entries
            .iter()
            .map(|entry| {
                let path = entry["file"].as_str().expect("工具文件路径");
                let source = &files
                    .iter()
                    .find(|(file, _)| file == path)
                    .expect("工具返回文件存在")
                    .1;
                let start = entry["lines"][0].as_u64().expect("起始行") as usize;
                let end = entry["lines"][1].as_u64().expect("结束行") as usize;
                assert!(start > 0 && start <= end && end <= source.lines().count());
                let expected = source
                    .lines()
                    .skip(start - 1)
                    .take(end - start + 1)
                    .collect::<Vec<_>>()
                    .join("\n");
                let snippet = entry["snippet"].as_str().expect("源码正文");
                // 截断提示属于工具协议而非源码，只在显式 truncated 标记存在时剥离。
                let snippet = if entry["truncated"].as_bool() == Some(true) {
                    snippet
                        .strip_suffix("\n// …（片段已截断）")
                        .expect("截断结果必须附带提示")
                } else {
                    snippet
                };
                assert_eq!(
                    snippet.trim(),
                    expected.trim(),
                    "返回片段必须来自报告中的真实行号"
                );
                json!({"path": path, "symbol": entry["qualified_symbol"], "lines": entry["lines"],
                "hydrated_lines": entry["hydrated_lines"], "truncated": entry["truncated"]})
            })
            .collect();
        row["tool_rank"] = json!(tool_rank);
        row["tool_latency_ms"] = json!(started.elapsed().as_secs_f64() * 1000.0);
        row["tool_locations"] = json!(tool_locations);
        println!("{}", serde_json::to_string(&row).expect("序列化定位结果"));
        rows.push(row);
    }
    let report = json!({"queries": rows.len(), "hits_at_1": hits[0], "hits_at_3": hits[1],
        "hits_at_10": hits[2], "tool_hits_at_1": tool_hits[0], "tool_hits_at_3": tool_hits[1],
        "tool_hits_at_10": tool_hits[2], "results": rows});
    println!(
        "实现定位：Top-1={} Top-3={} Top-10={} / {}",
        hits[0],
        hits[1],
        hits[2],
        rows.len()
    );
    println!(
        "工具定位：Top-1={} Top-3={} Top-10={} / {}",
        tool_hits[0],
        tool_hits[1],
        tool_hits[2],
        rows.len()
    );
    if let Ok(path) = std::env::var("RETRIEVAL_LOCATION_REPORT") {
        std::fs::write(
            path,
            serde_json::to_vec_pretty(&report).expect("序列化报告"),
        )
        .expect("写入定位报告");
    }
    assert_eq!(hits[2], rows.len(), "每条业务查询的目标定义必须进入 Top-10");
    assert_eq!(tool_hits[2], rows.len(), "上下文打包不能丢失目标定义");
}

/// 诊断：一次模型重排实际提交的请求体规模。
///
/// 延迟受远端抖动影响很大，单看耗时无法判断"只发紧凑签名"是否真的降低了开销。请求体
/// 字节数与候选条数是确定性的，直接量它。
#[test]
#[ignore = "需要网络与嵌入 API 密钥"]
fn rerank_payload_size() {
    let api_key = std::env::var(API_KEY_ENV).expect("需要 API 密钥");
    let root = repository_root();
    let files = collect_evaluation_files(&root);
    let file_refs: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&file_refs).with_http_handler(forward_to_endpoint(api_key));
    let ablation = Ablation {
        label: "全栈",
        semantic: true,
        rerank: true,
    };
    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(&host, ablation_context(&root.to_string_lossy(), &ablation))
        .expect("激活插件");
    for _ in 0..400 {
        let call = ToolCall::new("payload", "code_search", json!({"query": "初始化索引"}));
        let _ = plugin.call_tool_with_host(&host, call);
        if plugin.index_status() != IndexStatus::Indexing {
            break;
        }
    }

    println!("\n# 重排请求体规模\n");
    println!("| 查询 | 是否重排 | 候选数 | 请求体字节 | 单条平均字节 |");
    println!("| --- | --- | --- | --- | --- |");
    for query in scenario_queries() {
        let before = host.http_requests().len();
        let execution = plugin.search_execution(&host, query.query, None, 10);
        let requests = host.http_requests();
        let rerank_request = requests[before..]
            .iter()
            .find(|request| request.path.trim_matches('/') == "rerank");
        let (count, bytes) = match rerank_request {
            Some(request) => {
                let body = request.body.as_deref().unwrap_or("");
                let count = serde_json::from_str::<serde_json::Value>(body)
                    .ok()
                    .and_then(|value| {
                        value
                            .get("documents")
                            .and_then(|v| v.as_array())
                            .map(Vec::len)
                    })
                    .unwrap_or(0);
                (count, body.len())
            }
            None => (0, 0),
        };
        println!(
            "| {} | {} | {count} | {bytes} | {} |",
            query.query,
            if execution.diagnostics.rerank_activated {
                "是"
            } else {
                "否"
            },
            bytes
                .checked_div(count)
                .map_or_else(|| "—".to_string(), |average| average.to_string())
        );
    }
}

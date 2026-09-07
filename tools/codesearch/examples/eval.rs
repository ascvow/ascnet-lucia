//! 检索评测：对比 FTS-only 与 Hybrid 的 Recall@k / MRR。
//!
//! ```bash
//! # 先用真实模型建好索引（Hybrid 需要向量）
//! export SILICONFLOW_API_KEY=...
//! cargo run --release --example eval -- --root . --queries eval/queries.json
//! ```
//!
//! 基线（FTS-only）通过 offline 模式获得：offline 下不请求模型，无法生成查询向量，
//! 因此只剩符号、路径、字面量与 FTS5 通道。这正是"没有 Embedding 服务时"的真实表现。
//!
//! 只报告本次实际运行的数字。查询集是人工标注的小样本，
//! 结论仅用于同一份代码上的回归对比，不能外推为通用检索质量指标。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use codesearch::types::{ScopeFilter, SearchMode, SearchRequest};
use codesearch::{AccessMode, Engine, EngineOptions};

/// 评测时统计的截断位置。
const CUTOFFS: [usize; 3] = [1, 3, 5];
/// 每次查询取回的结果数，需不小于最大 cutoff。
const MAX_RESULTS: usize = 10;

/// 从评测作用域中排除的路径。
///
/// 就地索引本仓库时，查询集文件本身也会被索引。它逐字包含每一条查询文本
/// **以及**标注好的期望路径，于是稳定地成为自己那条查询的头部结果——
/// 这是测量装置的自我污染，不是检索能力。两条通路都受影响，
/// 排除它才能让排名反映代码检索本身。
const EXCLUDE_FROM_EVAL: &[&str] = &["eval/**"];

/// 一条标注查询。
#[derive(serde::Deserialize)]
struct Case {
    query: String,
    #[serde(default)]
    expect_paths: Vec<String>,
    #[serde(default)]
    note: String,
}

#[derive(serde::Deserialize)]
struct QuerySet {
    queries: Vec<Case>,
}

/// 一种检索配置的累计指标。
#[derive(Default)]
struct Metrics {
    /// 各 cutoff 下命中至少一个期望路径的查询数。
    hits: BTreeMap<usize, usize>,
    /// 倒数排名之和。
    reciprocal_rank: f64,
    /// 参与统计的查询数。
    total: usize,
    /// 实际走了向量召回的查询数。
    vector_used: usize,
}

impl Metrics {
    fn record(&mut self, ranks: Option<usize>, vector_used: bool) {
        self.total += 1;
        if vector_used {
            self.vector_used += 1;
        }
        for cutoff in CUTOFFS {
            let hit = ranks.is_some_and(|rank| rank <= cutoff);
            *self.hits.entry(cutoff).or_default() += usize::from(hit);
        }
        if let Some(rank) = ranks {
            self.reciprocal_rank += 1.0 / rank as f64;
        }
    }

    fn recall(&self, cutoff: usize) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.hits.get(&cutoff).copied().unwrap_or(0) as f64 / self.total as f64
    }

    fn mrr(&self) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        self.reciprocal_rank / self.total as f64
    }
}

/// 找出第一个命中期望路径的排名（1 起）。
fn first_hit(results: &[codesearch::types::SearchResult], expect: &[String]) -> Option<usize> {
    results
        .iter()
        .position(|result| {
            expect
                .iter()
                .any(|wanted| result.path == *wanted || result.path.ends_with(wanted.as_str()))
        })
        .map(|index| index + 1)
}

async fn open(
    root: &Path,
    config_path: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    offline: bool,
) -> codesearch::error::Result<Engine> {
    Engine::open(EngineOptions {
        root: root.to_path_buf(),
        config_path,
        data_dir,
        // 只读：评测不修改索引，也不与正在运行的 serve 争抢写锁。
        mode: AccessMode::ReadOnly,
        offline,
    })
    .await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut root = PathBuf::from(".");
    let mut queries = PathBuf::from("eval/queries.json");
    let mut data_dir: Option<PathBuf> = None;
    let mut config: Option<PathBuf> = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--root" => root = args.next().expect("--root 需要路径").into(),
            "--queries" => queries = args.next().expect("--queries 需要路径").into(),
            "--data-dir" => data_dir = Some(args.next().expect("--data-dir 需要路径").into()),
            "--config" => config = Some(args.next().expect("--config 需要路径").into()),
            other => return Err(format!("未知参数: {other}").into()),
        }
    }

    let text = std::fs::read_to_string(&queries)
        .map_err(|error| format!("读取查询集 {} 失败: {error}", queries.display()))?;
    let set: QuerySet = serde_json::from_str(&text)?;

    let hybrid = open(&root, config.clone(), data_dir.clone(), false).await?;
    let baseline = open(&root, config, data_dir, true).await?;

    if !hybrid.has_vector_path() {
        eprintln!(
            "警告：当前未配置可用的 Embedding 服务，Hybrid 与 FTS-only 结果将完全相同。\n\
             先配置 codesearch.toml 并用真实模型索引一次，再运行评测。"
        );
    }

    let mut fts = Metrics::default();
    let mut mixed = Metrics::default();

    println!("查询集: {} （{} 条）", queries.display(), set.queries.len());
    println!("{:<44} {:>10} {:>10}  期望", "查询", "FTS-only", "Hybrid");
    println!("{}", "-".repeat(96));

    for case in &set.queries {
        let request = SearchRequest {
            query: case.query.clone(),
            scope: ScopeFilter {
                exclude: EXCLUDE_FROM_EVAL.iter().map(|p| p.to_string()).collect(),
                ..ScopeFilter::default()
            },
            mode: SearchMode::Fast,
            max_results: Some(MAX_RESULTS),
            token_budget: None,
        };
        let base = baseline.search(&request).await?;
        let full = hybrid.search(&request).await?;

        let base_rank = first_hit(&base.results, &case.expect_paths);
        let full_rank = first_hit(&full.results, &case.expect_paths);
        fts.record(base_rank, base.status.vector_search_used);
        mixed.record(full_rank, full.status.vector_search_used);

        let render = |rank: Option<usize>| match rank {
            Some(rank) => format!("#{rank}"),
            None => "未命中".to_string(),
        };
        // 查询可能含中文，按显示宽度对齐意义不大，这里保证可读即可。
        println!(
            "{:<44} {:>10} {:>10}  {}",
            truncate(&case.query, 42),
            render(base_rank),
            render(full_rank),
            case.expect_paths.join(", ")
        );
        if !case.note.is_empty() && (base_rank.is_none() || full_rank.is_none()) {
            println!("{:<44} └─ {}", "", case.note);
        }
    }

    println!("{}", "-".repeat(96));
    println!("\n{:<12} {:>10} {:>10}", "指标", "FTS-only", "Hybrid");
    for cutoff in CUTOFFS {
        println!(
            "{:<12} {:>10.3} {:>10.3}",
            format!("Recall@{cutoff}"),
            fts.recall(cutoff),
            mixed.recall(cutoff)
        );
    }
    println!("{:<12} {:>10.3} {:>10.3}", "MRR", fts.mrr(), mixed.mrr());
    println!(
        "\n共 {} 条查询；FTS-only 走向量召回 {} 条（应为 0），Hybrid 走向量召回 {} 条。",
        mixed.total, fts.vector_used, mixed.vector_used
    );
    println!(
        "注：查询集为人工标注的小样本，以上数字仅反映本次运行在该样本上的表现，不是通用质量结论。"
    );

    Ok(())
}

/// 按字符数截断，避免在多字节字符中间切断。
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars()
        .take(limit.saturating_sub(1))
        .collect::<String>()
        + "…"
}

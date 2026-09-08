//! 重放真实调用的冻结候选，只运行生产上下文组装；用于隔离模型波动和评估离线重排。

use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use codesearch::{config::Config, context, retrieval, store::Store, types::*};
use serde::Deserialize;
use serde_json::json;

/// 输入和输出都必须显式指定，输出已存在时拒绝覆盖。
#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
}

/// 独立索引与其冻结问题，不携带真值。
#[derive(Deserialize)]
struct Project {
    db: PathBuf,
    config: PathBuf,
    rows: Vec<Row>,
}

/// 原始查询及生产链路捕获的候选。
#[derive(Deserialize)]
struct Row {
    id: String,
    query: String,
    candidates: Vec<SavedCandidate>,
}

/// 诊断候选的最小契约，不为评测扩张公开协议。
#[derive(Deserialize)]
struct SavedCandidate {
    chunk_id: i64,
    score: f64,
    matched_by: Vec<MatchSource>,
    exact: bool,
}

/// 重放只读索引，验证每个引用能返回同版本原文；错误中止，不计成空结果。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Arguments::parse();
    if args.output.exists() {
        return Err("输出已存在，请使用新实验标签".into());
    }
    let projects: Vec<Project> = serde_json::from_slice(&std::fs::read(args.input)?)?;
    let mut rows = Vec::new();
    for project in projects {
        let store = Store::open(&project.db, true)?;
        let config = Config::load(&project.config)?;
        for row in project.rows {
            let plan = retrieval::analyze(&row.query, &ScopeFilter::default())?;
            let candidates: Vec<retrieval::Candidate> = row
                .candidates
                .into_iter()
                .map(|c| retrieval::Candidate {
                    chunk_id: c.chunk_id,
                    score: c.score,
                    matched_by: c.matched_by,
                    exact: c.exact,
                })
                .collect();
            let started = Instant::now();
            let result = context::assemble(
                &store,
                &context::AssembleInput {
                    candidates: &candidates,
                    plan: &plan,
                    max_results: config.retrieval.max_results,
                    token_budget: config.retrieval.token_budget,
                    config: &config.retrieval,
                },
            )?;
            let milliseconds = started.elapsed().as_secs_f64() * 1000.0;
            for hit in &result.results {
                let read = context::read(
                    &store,
                    &ReadRequest {
                        code_ref: hit.code_ref.clone(),
                        expand: ReadExpand::None,
                        token_budget: Some(100_000),
                    },
                    100_000,
                )?;
                if read.file_hash != hit.file_hash
                    || read.path != hit.path
                    || read.start_line != hit.start_line
                    || read.end_line != hit.end_line
                    || !read
                        .content
                        .replace("\r\n", "\n")
                        .contains(hit.content.replace("\r\n", "\n").trim_end_matches('\n'))
                {
                    return Err(format!("{} 引用校验失败", row.id).into());
                }
            }
            rows.push(json!({"id":row.id,"query":row.query,"results":result.results,"milliseconds":milliseconds,
                "tokens":result.total_estimated_tokens,"truncated":result.truncated}));
        }
    }
    std::fs::write(args.output, serde_json::to_vec_pretty(&rows)?)?;
    Ok(())
}

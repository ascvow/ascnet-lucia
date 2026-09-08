//! 对同一条真实查询向量比较近邻 Top-80 与全量余弦排名；不向召回注入真值。

use std::collections::HashSet;
use std::path::PathBuf;

use clap::Parser;
use codesearch::{
    config::Config, models, retrieval, store::Store, types::ScopeFilter, vector::VectorIndex,
};
use serde::Deserialize;
use serde_json::json;

/// 输入复用重放清单；候选字段不参与向量计算，输出必须使用新文件。
#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    input: PathBuf,
    #[arg(long)]
    output: PathBuf,
}

/// 独立项目的只读索引。
#[derive(Deserialize)]
struct Project {
    db: PathBuf,
    config: PathBuf,
    rows: Vec<Row>,
}

/// 只消费原始问题，不读取评测答案或提示路径。
#[derive(Deserialize)]
struct Row {
    id: String,
    query: String,
}

/// 每题只生成一次向量，缓存完整距离表；网络、索引或维度异常时中止。
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Arguments::parse();
    if args.output.exists() {
        return Err("输出已存在".into());
    }
    let projects: Vec<Project> = serde_json::from_slice(&std::fs::read(args.input)?)?;
    let mut output = Vec::new();
    for project in projects {
        let config = Config::load(&project.config)?;
        let store = Store::open(&project.db, true)?;
        let provider = models::build_provider(&config, false, None)
            .await?
            .ok_or("缺少模型")?;
        let space = provider.space();
        let index = VectorIndex::open(
            &project.db.parent().ok_or("索引目录无效")?.join("vectors"),
            &store,
            &space.id,
            space.dimensions,
        )?;
        let ids: HashSet<i64> = store
            .current_chunk_scopes()?
            .into_iter()
            .map(|c| c.id)
            .collect();
        let vectors =
            store.vectors_for_chunks(&space.id, &ids.iter().copied().collect::<Vec<_>>())?;
        if vectors.len() != ids.len() {
            return Err("索引向量不完整".into());
        }
        for row in project.rows {
            let plan = retrieval::analyze(&row.query, &ScopeFilter::default())?;
            let mut query = provider.embed_query(&plan.text).await?;
            models::normalize_l2(&mut query)?;
            let ann = index.search(&query, config.retrieval.vector_top_k, &ids)?;
            let mut exact: Vec<_> = vectors
                .iter()
                .map(|(id, vector)| (*id, models::cosine_similarity(&query, vector)))
                .collect();
            exact.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
            output.push(
                json!({"id":row.id,"query":row.query,"query_vector":query,"ann":ann,"exact":exact}),
            );
            println!("{} 全量比较 {} 个向量", row.id, exact.len());
        }
    }
    std::fs::write(args.output, serde_json::to_vec(&output)?)?;
    Ok(())
}

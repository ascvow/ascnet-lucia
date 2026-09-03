//! 语义向量索引的单元测试。
//!
//! Phase 8 的重点不是"能不能算出相似度"，而是几条容易静默出错的性质：
//! 维度不匹配必须拒绝而不是给出无意义的排名、非有限分量不得污染排序、
//! 嵌入空间不兼容必须被识别、增量重建不得留下孤儿向量。

use super::*;

/// 构造测试用嵌入空间。
fn space(model: &str, dimension: u32) -> EmbeddingSpace {
    EmbeddingSpace {
        provider_type: "api".to_string(),
        provider_id: "endpoint-abc".to_string(),
        model: model.to_string(),
        model_revision: String::new(),
        dimension,
        normalization: true,
        query_template_version: 1,
        document_template_version: 1,
    }
}

/// 建立一个含三条向量的索引。
fn fixture() -> SemanticIndex {
    let mut index = SemanticIndex::with_space(space("text-embedding-3-small", 3));
    index.upsert("chunk-x", "src/a.rs", &[1.0, 0.0, 0.0]);
    index.upsert("chunk-y", "src/b.rs", &[0.0, 1.0, 0.0]);
    index.upsert("chunk-z", "src/b.rs", &[0.0, 0.0, 1.0]);
    index
}

/// 检索必须按相似度降序返回最接近的向量。
#[test]
fn search_ranks_by_similarity() {
    let index = fixture();
    let found = index.search(&[0.9, 0.1, 0.0], None, 3);

    assert_eq!(found[0].0, "chunk-x", "最接近的向量应排首位");
    assert!(found[0].1 > found[1].1);
}

/// 未归一化的查询向量必须得到与归一化后相同的排名。
#[test]
fn query_vector_is_normalized() {
    let index = fixture();
    let small = index.search(&[0.1, 0.0, 0.0], None, 3);
    let large = index.search(&[100.0, 0.0, 0.0], None, 3);

    assert_eq!(small[0].0, large[0].0);
    assert!((small[0].1 - large[0].1).abs() < 1e-5, "缩放不应改变相似度");
}

/// 维度不符必须返回空，而不是给出无意义的排名。
#[test]
fn dimension_mismatch_returns_nothing() {
    let index = fixture();
    assert!(index.search(&[1.0, 0.0], None, 3).is_empty());
    assert!(index.search(&[1.0, 0.0, 0.0, 0.0], None, 3).is_empty());
}

/// 零向量与含非有限分量的向量都不得进入索引。
#[test]
fn degenerate_vectors_are_rejected() {
    let mut index = SemanticIndex::with_space(space("m", 3));
    index.upsert("zero", "src/a.rs", &[0.0, 0.0, 0.0]);
    index.upsert("nan", "src/a.rs", &[f32::NAN, 1.0, 0.0]);
    index.upsert("inf", "src/a.rs", &[f32::INFINITY, 1.0, 0.0]);
    index.upsert("empty", "src/a.rs", &[]);

    assert!(index.is_empty(), "退化向量不得进入索引");
}

/// 含非有限分量的查询必须返回空而不是未定义的排序。
#[test]
fn degenerate_query_returns_nothing() {
    let index = fixture();
    assert!(index.search(&[f32::NAN, 0.0, 0.0], None, 3).is_empty());
    assert!(index.search(&[0.0, 0.0, 0.0], None, 3).is_empty());
}

/// 路径过滤必须生效。
#[test]
fn path_scope_filters_results() {
    let index = fixture();
    let found = index.search(&[0.0, 1.0, 0.0], Some("src/b.rs"), 10);

    assert_eq!(found.len(), 2);
    assert!(found.iter().all(|(id, _)| *id != "chunk-x"));
}

/// 重复写入同一 chunk 必须替换而不是追加。
#[test]
fn upsert_replaces_existing_vector() {
    let mut index = fixture();
    index.upsert("chunk-x", "src/a.rs", &[0.0, 0.0, 1.0]);

    assert_eq!(index.len(), 3, "不应产生重复条目");
    let found = index.search(&[0.0, 0.0, 1.0], None, 1);
    // 更新后 chunk-x 与 chunk-z 同向，按 ID 排序 chunk-x 在前。
    assert_eq!(found[0].0, "chunk-x");
}

/// 按文件移除必须清掉该文件的全部向量，不留孤儿。
#[test]
fn remove_file_clears_all_its_vectors() {
    let mut index = fixture();
    index.remove_file("src/b.rs");

    assert_eq!(index.len(), 1);
    assert!(index
        .search(&[0.0, 1.0, 0.0], None, 10)
        .iter()
        .all(|(id, _)| *id == "chunk-x"));
}

/// 空索引与任何嵌入空间兼容；已有向量后只与同一空间兼容。
#[test]
fn compatibility_follows_embedding_space() {
    let empty = SemanticIndex::new();
    assert!(empty.is_compatible_with(&space("any", 3)));

    let index = fixture();
    assert!(index.is_compatible_with(&space("text-embedding-3-small", 3)));
    assert!(
        !index.is_compatible_with(&space("text-embedding-3-large", 3)),
        "换模型必须判定为不兼容，即使维度相同"
    );
}

/// 声明了空间但没有向量时不得报告就绪。
#[test]
fn empty_index_is_not_ready() {
    let index = SemanticIndex::with_space(space("m", 3));
    assert!(!index.is_ready(), "空索引报告就绪会把「还没建好」误报成「没有结果」");
    assert!(fixture().is_ready());
}

/// 重置必须清空向量并切换空间。
#[test]
fn reset_switches_space_and_clears_vectors() {
    let mut index = fixture();
    index.reset(space("text-embedding-3-large", 3));

    assert!(index.is_empty());
    assert_eq!(
        index.space().map(|space| space.model.as_str()),
        Some("text-embedding-3-large")
    );
}

/// 相同相似度时必须按 chunk ID 排序，保证结果确定。
#[test]
fn ties_are_broken_deterministically() {
    let mut index = SemanticIndex::with_space(space("m", 3));
    index.upsert("chunk-b", "src/a.rs", &[1.0, 0.0, 0.0]);
    index.upsert("chunk-a", "src/a.rs", &[1.0, 0.0, 0.0]);

    let found = index.search(&[1.0, 0.0, 0.0], None, 2);
    assert_eq!(found[0].0, "chunk-a");
    assert_eq!(found[1].0, "chunk-b");
}

/// limit 必须被遵守。
#[test]
fn limit_is_respected() {
    let index = fixture();
    assert_eq!(index.search(&[1.0, 1.0, 1.0], None, 2).len(), 2);
    assert!(index.search(&[1.0, 1.0, 1.0], None, 0).is_empty());
}

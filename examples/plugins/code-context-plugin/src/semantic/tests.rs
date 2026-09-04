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
    index.upsert("chunk-x", "src/a.rs", Some("hash-x"), &[1.0, 0.0, 0.0]);
    index.upsert("chunk-y", "src/b.rs", Some("hash-y"), &[0.0, 1.0, 0.0]);
    index.upsert("chunk-z", "src/b.rs", Some("hash-z"), &[0.0, 0.0, 1.0]);
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
    index.upsert("zero", "src/a.rs", Some("zero"), &[0.0, 0.0, 0.0]);
    index.upsert("nan", "src/a.rs", Some("nan"), &[f32::NAN, 1.0, 0.0]);
    index.upsert("inf", "src/a.rs", Some("inf"), &[f32::INFINITY, 1.0, 0.0]);
    index.upsert("empty", "src/a.rs", Some("empty"), &[]);

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

/// sqlite-vec 查询失败后必须固定退回内存索引，不能让损坏数据库遮蔽已加载向量。
#[cfg(feature = "sqlite-vector-store")]
#[test]
fn sqlite_query_failure_falls_back_to_memory() {
    let path = std::env::temp_dir().join(format!(
        "lucia-code-context-sqlite-fallback-{}-{}.sqlite3",
        std::process::id(),
        std::thread::current().name().unwrap_or("test")
    ));
    let _ = std::fs::remove_file(&path);
    let mut index = SemanticIndex::with_sqlite(space("m", 3), &path).expect("创建数据库");
    index.upsert("chunk-x", "src/a.rs", Some("hash-x"), &[1.0, 0.0, 0.0]);
    assert!(index.take_backend_error().is_none());

    let connection = rusqlite::Connection::open(&path).expect("打开破坏连接");
    connection
        .execute("DROP TABLE code_context_vectors", [])
        .expect("删除向量表");
    drop(connection);

    let first = index.search(&[1.0, 0.0, 0.0], None, 1);
    assert_eq!(first[0].0, "chunk-x", "查询失败后应使用内存向量");
    assert!(index.take_backend_error().is_some(), "必须上报数据库错误");
    let second = index.search(&[1.0, 0.0, 0.0], None, 1);
    assert_eq!(second[0].0, "chunk-x", "后续查询不得重复使用损坏数据库");
    assert!(index.take_backend_error().is_none());
    assert!(index.snapshot().is_some(), "数据库失效后应允许导出回退快照");

    drop(index);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
    let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
}

/// 重复写入同一 chunk 必须替换而不是追加。
#[test]
fn upsert_replaces_existing_vector() {
    let mut index = fixture();
    index.upsert("chunk-x", "src/a.rs", Some("hash-x-new"), &[0.0, 0.0, 1.0]);

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

/// 复用判断必须同时校验 chunk ID 与内容哈希，防止正文变化后继续使用旧向量。
#[test]
fn current_vector_requires_matching_content_hash() {
    let index = fixture();

    assert!(index.contains_current("chunk-x", "hash-x"));
    assert!(
        !index.contains_current("chunk-x", "hash-x-new"),
        "相同 chunk ID 的旧向量不得掩盖正文变化"
    );
}

/// 对照当前结构索引清理时必须移除已删除和正文变化的向量，只保留完全一致的记录。
#[test]
fn retain_current_removes_stale_and_orphan_vectors() {
    let mut index = fixture();
    let removed = index.retain_current(&[("chunk-x", "hash-x"), ("chunk-y", "hash-y-new")]);

    assert_eq!(removed, 2);
    assert_eq!(index.len(), 1);
    assert!(index.contains_current("chunk-x", "hash-x"));
    assert!(!index.contains_current("chunk-y", "hash-y-new"));
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
    assert!(
        !index.is_ready(),
        "空索引报告就绪会把「还没建好」误报成「没有结果」"
    );
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
    index.upsert("chunk-b", "src/a.rs", Some("hash-b"), &[1.0, 0.0, 0.0]);
    index.upsert("chunk-a", "src/a.rs", Some("hash-a"), &[1.0, 0.0, 0.0]);

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

/// 新快照必须使用 int4 打包，并在恢复后保留内容哈希与相似度排序。
#[test]
fn int4_snapshot_round_trips_vectors_and_hashes() {
    let index = fixture();
    let snapshot = index.snapshot().expect("应生成快照");

    assert_eq!(snapshot.quantization, SemanticQuantization::Int4);
    assert_eq!(snapshot.records.len(), 3);
    assert!(snapshot
        .records
        .iter()
        .all(|record| record.vector.len() <= 4));

    let mut restored = SemanticIndex::new();
    assert!(restored.restore(&space("text-embedding-3-small", 3), &snapshot));
    assert!(restored.contains_current("chunk-x", "hash-x"));
    assert_eq!(restored.search(&[1.0, 0.0, 0.0], None, 1)[0].0, "chunk-x");
}

/// 缺少新字段的旧 int8 快照必须仍可读取，但不得被当作当前正文的可复用向量。
#[test]
fn legacy_int8_snapshot_is_readable_but_not_current() {
    let legacy = serde_json::json!({
        "space_id": space("legacy", 2).id(),
        "dimension": 2,
        "records": [{
            "chunk_id": "legacy-chunk",
            "file_path": "src/legacy.rs",
            "scale": 1.0 / 127.0,
            "vector": base64_encode(&[127_u8, 0_u8])
        }],
        "truncated": false
    });
    let snapshot: SemanticSnapshot = serde_json::from_value(legacy).expect("旧快照应兼容");
    assert_eq!(snapshot.quantization, SemanticQuantization::Int8);

    let mut restored = SemanticIndex::new();
    assert!(restored.restore(&space("legacy", 2), &snapshot));
    assert!(!restored.contains_current("legacy-chunk", "new-hash"));
    assert_eq!(restored.search(&[1.0, 0.0], None, 1)[0].0, "legacy-chunk");
}

/// 快照必须按实际 JSON 字节预算截断，而不是依赖固定向量条数。
#[test]
fn snapshot_respects_serialized_byte_budget() {
    let index = fixture();
    let first_only = {
        let mut one = SemanticIndex::with_space(space("text-embedding-3-small", 3));
        one.upsert("chunk-x", "src/a.rs", Some("hash-x"), &[1.0, 0.0, 0.0]);
        one.snapshot().expect("单条快照")
    };
    let one_record_bytes = serde_json::to_vec(&first_only)
        .expect("序列化单条快照")
        .len();
    let snapshot = index
        .snapshot_with_budget(one_record_bytes)
        .expect("应生成受限快照");

    assert_eq!(snapshot.records.len(), 1);
    assert!(snapshot.truncated);
    assert!(serde_json::to_vec(&snapshot).expect("序列化快照").len() <= one_record_bytes);
}

/// 当前仓库约六千五百个 1024 维向量必须能完整装入正式快照预算。
#[test]
fn repository_scale_int4_snapshot_fits_budget() {
    const CHUNK_COUNT: usize = 6_500;
    const DIMENSION: usize = 1_024;

    let mut index = SemanticIndex::with_space(space("BAAI/bge-m3", DIMENSION as u32));
    let vector = vec![1.0; DIMENSION];
    for sequence in 0..CHUNK_COUNT {
        let chunk_id = format!("{sequence:064x}");
        let content_hash = format!("{:064x}", sequence + CHUNK_COUNT);
        let file_path = format!(
            "crates/agent-core/kernel/src/generated/module_{:04}/implementation.rs",
            sequence % 1_000
        );
        index.upsert(&chunk_id, &file_path, Some(&content_hash), &vector);
    }

    let snapshot = index.snapshot().expect("应生成仓库规模快照");
    let serialized = serde_json::to_vec(&snapshot).expect("序列化仓库规模快照");

    assert_eq!(snapshot.records.len(), CHUNK_COUNT);
    assert!(!snapshot.truncated, "当前仓库规模不应触发快照截断");
    assert!(serialized.len() <= MAX_SEMANTIC_SNAPSHOT_BYTES);
}

//! 存储层不变量测试：版本切换、FTS 一致性、向量日志与缓存复用。

use super::*;
use crate::retrieval::tokenize::FtsText;
use crate::types::{ChunkKind, Language};

/// 构造一个最小的可提交文件，`chunks` 给出 `(符号, 代码)` 列表。
fn prepared(path: &str, content: &str, chunks: &[(&str, &str)]) -> PreparedFile {
    let mut prepared_chunks = Vec::new();
    for (ordinal, (symbol, code)) in chunks.iter().enumerate() {
        // 代码块必须真实落在源码内，否则回读会切出错误内容。
        let start = content.find(code).unwrap_or(0);
        prepared_chunks.push(PreparedChunk {
            ordinal,
            kind: ChunkKind::Function,
            symbol: Some((*symbol).to_string()),
            symbol_key: Some(format!("rust:{symbol}")),
            parent_symbol: None,
            module_path: None,
            signature: Some(format!("fn {symbol}()")),
            doc: None,
            start_byte: start,
            end_byte: start + code.len(),
            start_line: 1,
            end_line: 1,
            token_estimate: 10,
            content_hash: blake3::hash(code.as_bytes()).to_hex().to_string(),
            embed_input: format!("Symbol: {symbol}\nCode:\n{code}"),
            embed_input_truncated: false,
            fts: FtsText {
                path_terms: path.replace('/', " "),
                symbol_terms: (*symbol).to_string(),
                doc_terms: String::new(),
                code_terms: (*code).to_string(),
            },
            symbols: vec![SymbolRecord {
                name: (*symbol).to_string(),
                qualified: (*symbol).to_string(),
                kind: ChunkKind::Function,
            }],
        });
    }
    PreparedFile {
        rel_path: path.to_string(),
        language: Language::Rust,
        file_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
        size: content.len() as u64,
        mtime_ns: 1,
        line_count: count_lines(content),
        content: content.to_string(),
        chunks: prepared_chunks,
        imports: Vec::new(),
    }
}

fn space(model: &str, dimensions: usize) -> EmbeddingSpaceRow {
    EmbeddingSpaceRow {
        id: format!("space-{model}-{dimensions}"),
        provider: "test".into(),
        model: model.into(),
        cache_namespace: "v1".into(),
        dimensions,
        metric: "cosine".into(),
        normalize: "l2".into(),
        query_prefix: String::new(),
        document_prefix: String::new(),
        template_version: 1,
    }
}

#[test]
fn modified_file_stops_returning_old_version_as_current() {
    let mut store = Store::open_in_memory().expect("open");
    let first = prepared("src/a.rs", "fn alpha() {}\n", &[("alpha", "fn alpha() {}")]);
    let outcome = store.commit_file(&first).expect("commit v1");
    let old_chunk = outcome.chunk_ids[0];

    // 旧版本此时是当前版本，符号与 FTS 都能命中。
    assert!(!store
        .symbol_candidates("alpha", 10)
        .expect("symbol")
        .is_empty());
    assert!(!store
        .fts_candidates("\"alpha\"", 10)
        .expect("fts")
        .is_empty());

    let second = prepared("src/a.rs", "fn beta() {}\n", &[("beta", "fn beta() {}")]);
    store.commit_file(&second).expect("commit v2");

    // 旧代码块仍可按引用读取，但不再是当前版本。
    let old = store.chunk_by_id(old_chunk).expect("row").expect("exists");
    assert!(!old.is_current, "旧代码块必须失效");
    // 旧符号与旧 FTS 记录都不能再作为当前代码返回。
    assert!(store
        .symbol_candidates("alpha", 10)
        .expect("symbol")
        .is_empty());
    assert!(store
        .fts_candidates("\"alpha\"", 10)
        .expect("fts")
        .is_empty());
    assert!(!store
        .fts_candidates("\"beta\"", 10)
        .expect("fts")
        .is_empty());
    assert_eq!(store.counters().expect("counters").current_chunks, 1);
}

#[test]
fn deleted_file_is_not_searchable_but_old_reference_still_reads() {
    let mut store = Store::open_in_memory().expect("open");
    let file = prepared("src/gone.rs", "fn gone() {}\n", &[("gone", "fn gone() {}")]);
    let outcome = store.commit_file(&file).expect("commit");
    let chunk_id = outcome.chunk_ids[0];

    assert!(store.mark_deleted("src/gone.rs").expect("delete"));

    assert!(store
        .symbol_candidates("gone", 10)
        .expect("symbol")
        .is_empty());
    assert!(store
        .fts_candidates("\"gone\"", 10)
        .expect("fts")
        .is_empty());
    assert_eq!(store.counters().expect("counters").current_chunks, 0);
    // 历史快照保留，旧引用仍能读到当时的源码。
    let row = store.chunk_by_id(chunk_id).expect("row").expect("exists");
    assert!(!row.is_current);
    let content = store
        .chunk_content(row.revision_id, row.start_byte, row.end_byte)
        .expect("content");
    assert_eq!(content, "fn gone() {}");
}

#[test]
fn rename_moves_content_without_losing_old_path_history() {
    let mut store = Store::open_in_memory().expect("open");
    let body = "fn shared() {}\n";
    store
        .commit_file(&prepared(
            "src/old.rs",
            body,
            &[("shared", "fn shared() {}")],
        ))
        .expect("commit old");
    // 重命名 = 旧路径删除 + 新路径新增。
    store.mark_deleted("src/old.rs").expect("delete old");
    store
        .commit_file(&prepared(
            "src/new.rs",
            body,
            &[("shared", "fn shared() {}")],
        ))
        .expect("commit new");

    let hits = store.path_candidates("new.rs", 10).expect("path");
    assert!(!hits.is_empty(), "新路径应可检索");
    assert!(
        store
            .path_candidates("old.rs", 10)
            .expect("path")
            .is_empty(),
        "旧路径不应再作为当前代码返回"
    );
    assert_eq!(store.counters().expect("counters").current_chunks, 1);
}

#[test]
fn reverting_content_reuses_revision_and_keeps_cache_key_stable() {
    let mut store = Store::open_in_memory().expect("open");
    let v1 = prepared("src/a.rs", "fn one() {}\n", &[("one", "fn one() {}")]);
    let v2 = prepared("src/a.rs", "fn two() {}\n", &[("two", "fn two() {}")]);
    let first = store.commit_file(&v1).expect("v1");
    store.commit_file(&v2).expect("v2");
    let back = store.commit_file(&v1).expect("回退到 v1");

    // 内容相同的版本记录被复用，不会重复保存同一份快照。
    assert!(back.reused_revision, "相同内容应复用已有版本行");
    assert_eq!(back.revision_id, first.revision_id);
    // 但代码块是新的一批，且 chunk id 绝不复用。
    assert_ne!(back.chunk_ids[0], first.chunk_ids[0]);
    assert!(back.chunk_ids[0] > first.chunk_ids[0]);
}

#[test]
fn chunk_ids_are_never_reused_after_invalidation() {
    let mut store = Store::open_in_memory().expect("open");
    let mut seen = std::collections::HashSet::new();
    for index in 0..5 {
        let content = format!("fn f{index}() {{}}\n");
        let symbol = format!("f{index}");
        let code = format!("fn f{index}() {{}}");
        let file = prepared("src/a.rs", &content, &[(&symbol, &code)]);
        let outcome = store.commit_file(&file).expect("commit");
        for id in outcome.chunk_ids {
            assert!(seen.insert(id), "代码块 ID {id} 被复用");
        }
    }
}

#[test]
fn embedding_cache_is_reused_and_isolated_per_space() {
    let store = Store::open_in_memory().expect("open");
    let left = space("model-a", 4);
    let right = space("model-b", 4);
    store.ensure_embedding_space(&left).expect("left");
    store.ensure_embedding_space(&right).expect("right");

    let vector = vec![1.0f32, 0.0, 0.0, 0.0];
    store
        .put_cached_vector(&left.id, "key-1", &vector)
        .expect("put");

    assert_eq!(
        store.cached_vector(&left.id, "key-1").expect("get"),
        Some(vector.clone())
    );
    // 同一个缓存键在另一个空间里不可见：同维度不同模型的向量不会混用。
    assert_eq!(store.cached_vector(&right.id, "key-1").expect("get"), None);
}

#[test]
fn embedding_space_identity_conflict_is_rejected() {
    let store = Store::open_in_memory().expect("open");
    let original = space("model-a", 4);
    store.ensure_embedding_space(&original).expect("first");
    // 同 id 但维度不同：必须报错而不是静默接受。
    let mut conflicting = original.clone();
    conflicting.dimensions = 8;
    assert!(store.ensure_embedding_space(&conflicting).is_err());
    // 完全一致时幂等。
    store.ensure_embedding_space(&original).expect("idempotent");
}

#[test]
fn late_embedding_cannot_reactivate_a_stale_chunk() {
    let mut store = Store::open_in_memory().expect("open");
    let target = space("model-a", 4);
    store.ensure_embedding_space(&target).expect("space");

    let v1 = prepared("src/a.rs", "fn one() {}\n", &[("one", "fn one() {}")]);
    let stale_chunk = store.commit_file(&v1).expect("v1").chunk_ids[0];

    // 文件在模型返回之前又改了一次。
    let v2 = prepared("src/a.rs", "fn two() {}\n", &[("two", "fn two() {}")]);
    let fresh_chunk = store.commit_file(&v2).expect("v2").chunk_ids[0];

    let vector = vec![0.5f32, 0.5, 0.5, 0.5];
    store
        .put_cached_vector(&target.id, "stale", &vector)
        .expect("cache");

    // 晚到的旧结果：缓存保留，但不允许重新激活旧代码块。
    let attached = store
        .attach_embedding(stale_chunk, &target.id, "stale")
        .expect("attach stale");
    assert!(!attached, "已失效的代码块不能被重新关联");
    let stale_row = store
        .chunk_by_id(stale_chunk)
        .expect("row")
        .expect("exists");
    assert!(!stale_row.is_current);
    assert_eq!(stale_row.embed_state, "pending");

    // 当前版本可以正常关联。
    store
        .put_cached_vector(&target.id, "fresh", &vector)
        .expect("cache");
    assert!(store
        .attach_embedding(fresh_chunk, &target.id, "fresh")
        .expect("attach fresh"));

    // 只有当前版本的向量参与重建。
    let ready = store.ready_vectors(&target.id).expect("ready");
    assert_eq!(ready.len(), 1);
    assert_eq!(ready[0].0, fresh_chunk);
}

#[test]
fn vector_ops_record_removals_and_additions_in_order() {
    let mut store = Store::open_in_memory().expect("open");
    let target = space("model-a", 4);
    store.ensure_embedding_space(&target).expect("space");

    let v1 = prepared("src/a.rs", "fn one() {}\n", &[("one", "fn one() {}")]);
    let first_chunk = store.commit_file(&v1).expect("v1").chunk_ids[0];
    store
        .put_cached_vector(&target.id, "k1", &[1.0, 0.0, 0.0, 0.0])
        .expect("cache");
    store
        .attach_embedding(first_chunk, &target.id, "k1")
        .expect("attach");

    // 修改文件：必须为旧向量登记一条 remove。
    let v2 = prepared("src/a.rs", "fn two() {}\n", &[("two", "fn two() {}")]);
    store.commit_file(&v2).expect("v2");

    let ops = store.vector_ops_since(&target.id, 0, 100).expect("ops");
    assert_eq!(ops.len(), 2, "应有一次 add 与一次 remove");
    assert_eq!(ops[0].op, "add");
    assert_eq!(ops[0].vector_key, first_chunk);
    assert_eq!(ops[0].cache_key.as_deref(), Some("k1"));
    assert_eq!(ops[1].op, "remove");
    assert_eq!(ops[1].vector_key, first_chunk);
    // 序号严格递增，Vector Writer 因此可以顺序消费。
    assert!(ops[1].seq > ops[0].seq);
    assert_eq!(store.max_vector_seq(&target.id).expect("max"), ops[1].seq);
    assert_eq!(
        store
            .vector_ops_backlog(&target.id, ops[0].seq)
            .expect("backlog"),
        1
    );
}

#[test]
fn switching_space_invalidates_old_vectors_without_reusing_them() {
    let mut store = Store::open_in_memory().expect("open");
    let old_space = space("model-a", 4);
    let new_space = space("model-b", 4);
    store.ensure_embedding_space(&old_space).expect("old");
    store.ensure_embedding_space(&new_space).expect("new");

    let file = prepared("src/a.rs", "fn one() {}\n", &[("one", "fn one() {}")]);
    let chunk_id = store.commit_file(&file).expect("commit").chunk_ids[0];
    store
        .put_cached_vector(&old_space.id, "k1", &[1.0, 0.0, 0.0, 0.0])
        .expect("cache");
    store
        .attach_embedding(chunk_id, &old_space.id, "k1")
        .expect("attach");

    let migrated = store.migrate_to_space(&new_space.id).expect("migrate");
    assert_eq!(migrated, 1);

    // 旧空间登记了删除操作，新空间没有可用向量。
    let old_ops = store.vector_ops_since(&old_space.id, 0, 100).expect("ops");
    assert!(old_ops
        .iter()
        .any(|op| op.op == "remove" && op.vector_key == chunk_id));
    assert!(store
        .ready_vectors(&new_space.id)
        .expect("ready")
        .is_empty());
    let row = store.chunk_by_id(chunk_id).expect("row").expect("exists");
    assert_eq!(row.embed_state, "pending");
}

#[test]
fn failed_embeddings_stop_retrying_after_the_attempt_limit() {
    let mut store = Store::open_in_memory().expect("open");
    let file = prepared("src/a.rs", "fn one() {}\n", &[("one", "fn one() {}")]);
    let chunk_id = store.commit_file(&file).expect("commit").chunk_ids[0];

    assert_eq!(store.pending_embeddings(10, 3).expect("pending").len(), 1);
    for _ in 0..3 {
        store
            .record_embedding_failure(chunk_id, "provider 超时", 3)
            .expect("failure");
    }
    // 达到上限后不再作为待办返回。
    assert!(store.pending_embeddings(10, 3).expect("pending").is_empty());
    assert_eq!(store.counters().expect("counters").embeddings_failed, 1);

    // 显式重置后可以重新尝试。
    assert_eq!(store.reset_failed_embeddings().expect("reset"), 1);
    assert_eq!(store.pending_embeddings(10, 3).expect("pending").len(), 1);
}

#[test]
fn catalog_epoch_advances_only_on_catalog_changes() {
    let mut store = Store::open_in_memory().expect("open");
    let start = store.catalog_epoch().expect("epoch");

    let file = prepared("src/a.rs", "fn one() {}\n", &[("one", "fn one() {}")]);
    store.commit_file(&file).expect("commit");
    let after_commit = store.catalog_epoch().expect("epoch");
    assert!(after_commit > start);

    // 仅刷新采集元数据不算目录变更。
    store.touch_file("src/a.rs", 42, 99).expect("touch");
    assert_eq!(store.catalog_epoch().expect("epoch"), after_commit);

    store.mark_deleted("src/a.rs").expect("delete");
    assert!(store.catalog_epoch().expect("epoch") > after_commit);
}

#[test]
fn workspace_binding_rejects_mismatched_index_directory() {
    let store = Store::open_in_memory().expect("open");
    store.bind_workspace("workspace-a").expect("first bind");
    store
        .bind_workspace("workspace-a")
        .expect("same id is fine");
    assert!(
        store.bind_workspace("workspace-b").is_err(),
        "索引目录不能被另一个 workspace 复用"
    );
}

#[test]
fn vector_key_conversion_rejects_out_of_range_values() {
    assert_eq!(vector_key(1).expect("ok"), 1u64);
    assert!(vector_key(-1).is_err(), "负数不能作为向量 key");
    assert_eq!(chunk_id_from_key(7).expect("ok"), 7i64);
    assert!(
        chunk_id_from_key(u64::MAX).is_err(),
        "超出 i64 范围必须报错而不是截断"
    );
}

#[test]
fn vector_blob_round_trips_and_rejects_corruption() {
    let vector = vec![1.5f32, -2.25, 0.0];
    let bytes = encode_vector(&vector);
    assert_eq!(decode_vector(&bytes, 3).expect("decode"), vector);
    // 维度与字节数不符时必须报错，不能交出被破坏的向量。
    assert!(decode_vector(&bytes, 4).is_err());
    assert!(decode_vector(&bytes[..5], 3).is_err());
}

#[test]
fn count_lines_handles_trailing_newline_and_crlf() {
    assert_eq!(count_lines(""), 0);
    assert_eq!(count_lines("a"), 1);
    assert_eq!(count_lines("a\n"), 1);
    assert_eq!(count_lines("a\nb"), 2);
    assert_eq!(count_lines("a\r\nb\r\n"), 2);
}

//! 向量索引一致性与崩溃恢复测试。
//!
//! 每个测试都模拟一个具体的中断位置，验证重启后索引仍与 SQLite 保持一致，
//! 且恢复过程不需要任何 Embedding Provider。

use super::*;
use crate::retrieval::tokenize::FtsText;
use crate::store::{EmbeddingSpaceRow, PreparedChunk, PreparedFile, SymbolRecord};
use crate::types::{ChunkKind, Language};

const DIM: usize = 8;

fn space_row() -> EmbeddingSpaceRow {
    EmbeddingSpaceRow {
        id: "space-test".into(),
        provider: "test".into(),
        model: "model".into(),
        cache_namespace: "v1".into(),
        dimensions: DIM,
        metric: "cosine".into(),
        normalize: "l2".into(),
        query_prefix: String::new(),
        document_prefix: String::new(),
        template_version: 1,
    }
}

/// 生成一个只含单位方向的归一化向量，便于断言检索顺序。
fn unit(axis: usize) -> Vec<f32> {
    let mut vector = vec![0.0f32; DIM];
    vector[axis % DIM] = 1.0;
    vector
}

fn prepared(path: &str, symbol: &str) -> PreparedFile {
    let code = format!("fn {symbol}() {{}}");
    let content = format!("{code}\n");
    PreparedFile {
        rel_path: path.to_string(),
        language: Language::Rust,
        file_hash: blake3::hash(content.as_bytes()).to_hex().to_string(),
        size: content.len() as u64,
        mtime_ns: 1,
        line_count: 1,
        content: content.clone(),
        chunks: vec![PreparedChunk {
            ordinal: 0,
            kind: ChunkKind::Function,
            symbol: Some(symbol.to_string()),
            symbol_key: Some(format!("rust:{symbol}")),
            parent_symbol: None,
            module_path: None,
            signature: None,
            doc: None,
            start_byte: 0,
            end_byte: code.len(),
            start_line: 1,
            end_line: 1,
            token_estimate: 8,
            content_hash: blake3::hash(code.as_bytes()).to_hex().to_string(),
            embed_input: code.clone(),
            embed_input_truncated: false,
            fts: FtsText {
                path_terms: path.replace('/', " "),
                symbol_terms: symbol.to_string(),
                doc_terms: String::new(),
                code_terms: code.clone(),
            },
            symbols: vec![SymbolRecord {
                name: symbol.to_string(),
                qualified: symbol.to_string(),
                kind: ChunkKind::Function,
            }],
        }],
        imports: Vec::new(),
    }
}

/// 建库并写入 `count` 个已就绪向量，返回 `(store, space, chunk_ids)`。
fn store_with_vectors(count: usize) -> (Store, EmbeddingSpaceRow, Vec<i64>) {
    let mut store = Store::open_in_memory().expect("open store");
    let space = space_row();
    store.ensure_embedding_space(&space).expect("space");
    let mut ids = Vec::new();
    for index in 0..count {
        let path = format!("src/f{index}.rs");
        let symbol = format!("f{index}");
        let chunk_id = store
            .commit_file(&prepared(&path, &symbol))
            .expect("commit")
            .chunk_ids[0];
        let cache_key = format!("key-{index}");
        store
            .put_cached_vector(&space.id, &cache_key, &unit(index))
            .expect("cache");
        store
            .attach_embedding(chunk_id, &space.id, &cache_key)
            .expect("attach");
        ids.push(chunk_id);
    }
    (store, space, ids)
}

fn allow(ids: &[i64]) -> HashSet<i64> {
    ids.iter().copied().collect()
}

#[test]
fn replay_catches_up_when_sqlite_committed_but_index_never_updated() {
    // 模拟：SQLite 事务已提交，进程在更新 USearch 之前崩溃。
    let (store, space, ids) = store_with_vectors(3);
    let dir = tempfile::tempdir().expect("tempdir");

    let index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    // 目录里没有检查点，但 SQLite 已有向量，因此走重建路径。
    assert_eq!(index.origin(), LoadOrigin::RebuiltFromStore);
    assert_eq!(index.len(), 3);
    assert_eq!(index.applied_seq(), store.max_vector_seq(&space.id).expect("max"));
    // 重建出来的内容尚未持久化。
    assert_eq!(index.durable_seq(), 0);

    let hits = index.search(&unit(1), 3, &allow(&ids)).expect("search");
    assert_eq!(hits[0].0, ids[1], "最近邻应是同方向的向量");
}

#[test]
fn replay_is_idempotent_when_applied_twice() {
    let (store, space, ids) = store_with_vectors(2);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    let before = index.len();

    // 强制从头重放同一批日志：结果必须完全一致，不出现重复向量。
    index.applied_seq = 0;
    let replayed = index.replay(&store).expect("replay");
    assert!(replayed >= 2);
    assert_eq!(index.len(), before, "重复重放不应改变向量数量");
    let hits = index.search(&unit(0), 5, &allow(&ids)).expect("search");
    assert_eq!(hits.iter().filter(|(id, _)| *id == ids[0]).count(), 1);
}

#[test]
fn removing_a_missing_key_is_harmless_during_replay() {
    let (mut store, space, ids) = store_with_vectors(1);
    let dir = tempfile::tempdir().expect("tempdir");

    // 删除文件会登记一条 remove。
    store.mark_deleted("src/f0.rs").expect("delete");
    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    assert!(index.is_empty(), "被删除的代码块不应留在索引中");

    // 再次重放同一条 remove：对缺失 key 无副作用。
    index.applied_seq = 0;
    index.replay(&store).expect("replay again");
    assert!(index.is_empty());
    assert!(index.search(&unit(0), 5, &allow(&ids)).expect("search").is_empty());
}

#[test]
fn checkpoint_publishes_manifest_and_survives_reopen() {
    let (store, space, ids) = store_with_vectors(4);
    let dir = tempfile::tempdir().expect("tempdir");

    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    let manifest = index.checkpoint().expect("checkpoint").expect("published");
    assert_eq!(manifest.generation, 1);
    assert_eq!(manifest.vector_count, 4);
    assert_eq!(index.durable_seq(), index.applied_seq());
    // 没有新操作时不重复发布检查点。
    assert!(index.checkpoint().expect("noop").is_none());
    drop(index);

    let reopened = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("reopen");
    assert_eq!(reopened.origin(), LoadOrigin::Checkpoint);
    assert_eq!(reopened.len(), 4);
    assert_eq!(reopened.durable_seq(), manifest.durable_applied_seq);
    let hits = reopened.search(&unit(2), 2, &allow(&ids)).expect("search");
    assert_eq!(hits[0].0, ids[2]);
}

#[test]
fn crash_after_memory_update_but_before_checkpoint_replays_the_tail() {
    // 模拟：向量已写入内存索引，检查点尚未发布，此时进程崩溃。
    let (mut store, space, _) = store_with_vectors(2);
    let dir = tempfile::tempdir().expect("tempdir");

    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    index.checkpoint().expect("checkpoint");
    let durable = index.durable_seq();

    // 检查点之后又提交了新的代码块与向量，但没有再次发布检查点。
    let chunk_id = store
        .commit_file(&prepared("src/late.rs", "late"))
        .expect("commit")
        .chunk_ids[0];
    store
        .put_cached_vector(&space.id, "key-late", &unit(5))
        .expect("cache");
    store
        .attach_embedding(chunk_id, &space.id, "key-late")
        .expect("attach");
    index.replay(&store).expect("replay in memory");
    assert_eq!(index.len(), 3);
    assert!(index.applied_seq() > index.durable_seq(), "内存领先于持久化");
    drop(index); // 崩溃：内存中的第三个向量没有落盘。

    let recovered = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("reopen");
    // manifest 仍指向旧 generation，缺的那条通过重放日志补齐。
    assert_eq!(recovered.origin(), LoadOrigin::Checkpoint);
    assert!(recovered.durable_seq() >= durable);
    assert_eq!(recovered.len(), 3, "重放应补齐检查点之后的向量");
    let hits = recovered
        .search(&unit(5), 1, &allow(&[chunk_id]))
        .expect("search");
    assert_eq!(hits[0].0, chunk_id);
}

#[test]
fn half_written_snapshot_falls_back_to_rebuilding_from_sqlite() {
    let (store, space, ids) = store_with_vectors(3);
    let dir = tempfile::tempdir().expect("tempdir");

    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    let manifest = index.checkpoint().expect("checkpoint").expect("published");
    drop(index);

    // 模拟检查点写到一半：截断快照文件，使哈希与大小都对不上。
    let snapshot = dir.path().join(&manifest.file_name);
    let bytes = std::fs::read(&snapshot).expect("read snapshot");
    std::fs::write(&snapshot, &bytes[..bytes.len() / 2]).expect("truncate");

    let recovered = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("reopen");
    assert_eq!(recovered.origin(), LoadOrigin::RebuiltFromStore);
    assert_eq!(recovered.len(), 3, "应从 SQLite 已存向量完整重建");
    let hits = recovered.search(&unit(0), 1, &allow(&ids)).expect("search");
    assert_eq!(hits[0].0, ids[0]);
}

#[test]
fn missing_snapshot_file_rebuilds_without_calling_a_provider() {
    let (store, space, _) = store_with_vectors(2);
    let dir = tempfile::tempdir().expect("tempdir");

    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    let manifest = index.checkpoint().expect("checkpoint").expect("published");
    drop(index);
    std::fs::remove_file(dir.path().join(&manifest.file_name)).expect("remove snapshot");

    // 恢复只依赖 SQLite 中已保存的向量，全程没有 Provider 参与。
    let recovered = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("reopen");
    assert_eq!(recovered.origin(), LoadOrigin::RebuiltFromStore);
    assert_eq!(recovered.len(), 2);
}

#[test]
fn corrupt_manifest_does_not_prevent_recovery() {
    let (store, space, _) = store_with_vectors(2);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    index.checkpoint().expect("checkpoint");
    let manifest_path = index.manifest_path();
    drop(index);

    std::fs::write(&manifest_path, b"{ not json").expect("corrupt manifest");
    let recovered = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("reopen");
    assert_eq!(recovered.origin(), LoadOrigin::RebuiltFromStore);
    assert_eq!(recovered.len(), 2);
}

#[test]
fn checkpoint_from_another_space_is_rejected() {
    let (store, space, _) = store_with_vectors(2);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    index.checkpoint().expect("checkpoint");
    let manifest_path = index.manifest_path();
    drop(index);

    // 手工改写 manifest 中的空间身份：加载必须失败并转向重建。
    let text = std::fs::read_to_string(&manifest_path).expect("read");
    let mut manifest: Manifest = serde_json::from_str(&text).expect("parse");
    manifest.space_id = "space-other".into();
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("json")).expect("write");

    let recovered = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("reopen");
    assert_eq!(recovered.origin(), LoadOrigin::RebuiltFromStore);
}

#[test]
fn manifest_with_different_dimensions_is_rejected() {
    let (store, space, _) = store_with_vectors(1);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    index.checkpoint().expect("checkpoint");
    let manifest_path = index.manifest_path();
    drop(index);

    let text = std::fs::read_to_string(&manifest_path).expect("read");
    let mut manifest: Manifest = serde_json::from_str(&text).expect("parse");
    manifest.dimensions = DIM * 2;
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("json")).expect("write");

    // 维度不符时不能把旧向量当作新空间的数据加载。
    let recovered = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("reopen");
    assert_eq!(recovered.origin(), LoadOrigin::RebuiltFromStore);
}

#[test]
fn new_checkpoint_replaces_the_previous_generation() {
    let (mut store, space, _) = store_with_vectors(1);
    let dir = tempfile::tempdir().expect("tempdir");
    let mut index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    let first = index.checkpoint().expect("checkpoint").expect("published");

    let chunk_id = store
        .commit_file(&prepared("src/extra.rs", "extra"))
        .expect("commit")
        .chunk_ids[0];
    store
        .put_cached_vector(&space.id, "key-extra", &unit(3))
        .expect("cache");
    store
        .attach_embedding(chunk_id, &space.id, "key-extra")
        .expect("attach");
    index.replay(&store).expect("replay");
    let second = index.checkpoint().expect("checkpoint").expect("published");

    assert_eq!(second.generation, first.generation + 1);
    assert!(second.durable_applied_seq > first.durable_applied_seq);
    // 新检查点发布后旧快照才被删除，目录中只保留当前 generation。
    assert!(dir.path().join(&second.file_name).exists());
    assert!(!dir.path().join(&first.file_name).exists());
}

#[test]
fn search_only_returns_keys_in_the_allowed_set() {
    let (store, space, ids) = store_with_vectors(4);
    let dir = tempfile::tempdir().expect("tempdir");
    let index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");

    // 作用域过滤：即便近邻更接近，也不能返回集合之外的 key。
    let allowed = allow(&[ids[2], ids[3]]);
    let hits = index.search(&unit(0), 4, &allowed).expect("search");
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|(id, _)| allowed.contains(id)));

    // 空集合直接短路，不做无谓检索。
    assert!(index.search(&unit(0), 4, &HashSet::new()).expect("search").is_empty());
    assert!(index.search(&unit(0), 0, &allow(&ids)).expect("search").is_empty());
}

#[test]
fn dimension_mismatch_is_reported_instead_of_silently_accepted() {
    let mut index = VectorIndex::in_memory(DIM).expect("in-memory");
    assert!(index.upsert(1, &[1.0, 0.0]).is_err(), "维度不符必须报错");
    index.upsert(1, &unit(0)).expect("valid insert");
    assert!(index.search(&[1.0, 0.0], 1, &allow(&[1])).is_err());
}

#[test]
fn upsert_replaces_the_vector_for_an_existing_key() {
    let mut index = VectorIndex::in_memory(DIM).expect("in-memory");
    index.upsert(1, &unit(0)).expect("insert");
    index.upsert(1, &unit(4)).expect("replace");
    assert_eq!(index.len(), 1, "同一 key 不应产生两条记录");

    let hits = index.search(&unit(4), 1, &allow(&[1])).expect("search");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].1 < 0.01, "替换后应匹配新向量, 距离={}", hits[0].1);
}

#[test]
fn in_memory_index_never_writes_checkpoints() {
    let mut index = VectorIndex::in_memory(DIM).expect("in-memory");
    index.upsert(1, &unit(0)).expect("insert");
    assert!(index.checkpoint().expect("checkpoint").is_none());
}

#[test]
fn empty_store_yields_a_fresh_index() {
    let store = Store::open_in_memory().expect("store");
    let space = space_row();
    store.ensure_embedding_space(&space).expect("space");
    let dir = tempfile::tempdir().expect("tempdir");

    let index = VectorIndex::open(dir.path(), &store, &space.id, DIM).expect("open");
    assert_eq!(index.origin(), LoadOrigin::Fresh);
    assert!(index.is_empty());
}

#[test]
fn snapshot_names_are_filesystem_safe() {
    let name = snapshot_name("sha256:abc/def", 7);
    assert!(!name.contains('/') && !name.contains(':'));
    assert!(name.ends_with("-00000007.usearch"));
    // 空间 ID 全是特殊字符时也要得到可用文件名。
    assert!(!snapshot_name("///", 1).is_empty());
}

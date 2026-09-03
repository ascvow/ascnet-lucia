//! 结构化索引与构建流水线的单元测试。
//!
//! 覆盖 Phase 4 的 Definition of Done：索引的增删改查、仓库隔离、扫描与解析全链、
//! 分片构建的一致性、哈希表持久化，以及"重启后重建出同一份索引"。

use super::*;
use crate::model::RepositoryIdentity;
use crate::parser::parse_file;
use crate::repository::language::Language;
use crate::repository::{ScanBudget, ScanProgress};
use crate::storage::IndexStore;
use crate::testing::RepoHost;

/// 不设限的预算，用于一次跑完。
const UNLIMITED: ScanBudget = ScanBudget {
    max_host_calls: u32::MAX,
    max_bytes_read: u64::MAX,
};

/// 构造测试用仓库身份。
fn repository() -> RepositoryIdentity {
    RepositoryIdentity::from_workspace_root("/repo/test").expect("测试仓库身份")
}

/// 构造一个可索引文件描述。
fn indexable(path: &str, language: Language, content: &str) -> IndexableFile {
    IndexableFile {
        path: path.to_string(),
        language,
        content_hash: crate::repository::content_hash(content),
        byte_len: u32::try_from(content.len()).unwrap_or(u32::MAX),
        line_count: u32::try_from(content.lines().count()).unwrap_or(u32::MAX),
    }
}

/// 把一个文件解析后写入索引。
fn insert(index: &mut StructuralIndex, path: &str, language: Language, content: &str) -> bool {
    let file = indexable(path, language, content);
    let parsed = parse_file(index.repository(), path, language, content);
    index.upsert_file(&file, parsed)
}

/// 一次性构建完整索引。
fn build_all(host: &RepoHost) -> IndexBuild {
    let mut build = IndexBuild::with_default_limits(repository());
    let progress = build.advance(host, UNLIMITED);
    assert_eq!(progress, ScanProgress::Complete, "预算充足时应一次构建完成");
    build
}

// ---------------------------------------------------------------------------
// StructuralIndex
// ---------------------------------------------------------------------------

/// 写入文件后必须能查到记录、chunk 与符号。
#[test]
fn upsert_stores_file_chunks_and_symbols() {
    let mut index = StructuralIndex::new(repository());
    assert!(insert(
        &mut index,
        "src/a.rs",
        Language::Rust,
        "pub struct A;\nimpl A { pub fn run(&self) {} }\n"
    ));

    let record = index.file("src/a.rs").expect("应有文件记录");
    assert_eq!(record.language, Language::Rust);
    assert_eq!(record.tier, ParseTier::Structural);
    assert!(!record.chunk_ids.is_empty());

    let stats = index.stats();
    assert_eq!(stats.files, 1);
    assert!(stats.chunks >= 3, "struct + impl + 方法");
    assert!(stats.symbols >= 3);
    assert_eq!(stats.structural_files, 1);
    assert_eq!(stats.text_files, 0);
}

/// 每个 chunk 必须能按 ID 取回，且与文件记录一致。
#[test]
fn chunks_are_retrievable_by_id_and_path() {
    let mut index = StructuralIndex::new(repository());
    insert(&mut index, "src/a.rs", Language::Rust, "pub fn one() {}\n");

    let record = index.file("src/a.rs").expect("文件记录");
    for id in &record.chunk_ids {
        let chunk = index.chunk(id).expect("chunk 必须可按 ID 取回");
        assert_eq!(chunk.file_path, "src/a.rs");
    }
    assert_eq!(index.chunks_of("src/a.rs").len(), record.chunk_ids.len());
}

/// 重复索引同一文件必须幂等，不得留下孤儿 chunk。
#[test]
fn upsert_is_idempotent() {
    let mut index = StructuralIndex::new(repository());
    let source = "pub fn one() {}\npub fn two() {}\n";
    insert(&mut index, "src/a.rs", Language::Rust, source);
    let first = index.stats();

    insert(&mut index, "src/a.rs", Language::Rust, source);
    assert_eq!(index.stats(), first, "重复索引不得改变索引规模");
}

/// 更新文件必须移除旧 chunk，只保留新内容。
#[test]
fn upsert_replaces_previous_chunks() {
    let mut index = StructuralIndex::new(repository());
    insert(
        &mut index,
        "src/a.rs",
        Language::Rust,
        "pub fn old_one() {}\npub fn old_two() {}\n",
    );
    let stale: Vec<String> = index.file("src/a.rs").expect("文件记录").chunk_ids.clone();

    insert(
        &mut index,
        "src/a.rs",
        Language::Rust,
        "pub fn fresh() {}\n",
    );

    assert_eq!(index.stats().chunks, 1, "旧 chunk 必须被清理");
    for id in &stale {
        assert!(index.chunk(id).is_none(), "旧 chunk {id} 不得残留");
    }
    assert!(index
        .symbols()
        .all(|symbol| symbol.name != "old_one" && symbol.name != "old_two"));
}

/// 删除文件必须连同其 chunk 与符号一起移除。
#[test]
fn remove_file_clears_chunks_and_symbols() {
    let mut index = StructuralIndex::new(repository());
    insert(&mut index, "src/a.rs", Language::Rust, "pub fn a() {}\n");
    insert(&mut index, "src/b.rs", Language::Rust, "pub fn b() {}\n");

    assert!(index.remove_file("src/a.rs"));
    assert!(!index.remove_file("src/a.rs"), "重复删除应返回 false");

    assert!(index.file("src/a.rs").is_none());
    assert_eq!(index.stats().files, 1);
    assert!(index.symbols().all(|symbol| symbol.name != "a"));
    assert!(index.chunks().all(|chunk| chunk.file_path != "src/a.rs"));
}

/// 索引必须拒绝来自其他仓库的 chunk。
#[test]
fn foreign_repository_chunks_are_rejected() {
    let mut index = StructuralIndex::new(repository());
    let other = RepositoryIdentity::from_workspace_root("/repo/other").expect("另一个仓库");
    let file = indexable("src/a.rs", Language::Rust, "pub fn a() {}\n");
    let parsed = parse_file(&other, "src/a.rs", Language::Rust, "pub fn a() {}\n");

    assert!(!index.upsert_file(&file, parsed), "跨仓库写入必须被拒绝");
    assert!(index.is_empty());
}

/// 文本兜底的文件必须被单独统计。
#[test]
fn text_tier_files_are_counted_separately() {
    let mut index = StructuralIndex::new(repository());
    insert(&mut index, "src/a.rs", Language::Rust, "pub fn a() {}\n");
    insert(
        &mut index,
        "README.md",
        Language::Markdown,
        "# 标题\n正文\n",
    );

    let stats = index.stats();
    assert_eq!(stats.structural_files, 1);
    assert_eq!(stats.text_files, 1);
}

/// 清空索引后必须回到初始状态，但保留仓库身份。
#[test]
fn clear_resets_index_but_keeps_repository() {
    let mut index = StructuralIndex::new(repository());
    insert(&mut index, "src/a.rs", Language::Rust, "pub fn a() {}\n");

    index.clear();

    assert!(index.is_empty());
    assert_eq!(index.stats(), IndexStats::default());
    assert_eq!(index.repository(), &repository());
}

/// 哈希表必须覆盖全部文件并按路径有序。
#[test]
fn file_hashes_cover_every_file() {
    let mut index = StructuralIndex::new(repository());
    insert(&mut index, "src/b.rs", Language::Rust, "pub fn b() {}\n");
    insert(&mut index, "src/a.rs", Language::Rust, "pub fn a() {}\n");

    let hashes = index.file_hashes();
    assert_eq!(hashes.len(), 2);
    assert_eq!(hashes[0].0, "src/a.rs", "必须按路径升序");
    assert_eq!(hashes[1].0, "src/b.rs");
}

/// 变更判断必须识别新增与修改，并放过未变更文件。
#[test]
fn change_detection_uses_content_hash() {
    let mut index = StructuralIndex::new(repository());
    insert(&mut index, "src/a.rs", Language::Rust, "pub fn a() {}\n");
    let previous: std::collections::BTreeMap<String, String> =
        index.file_hashes().into_iter().collect();

    let unchanged = indexable("src/a.rs", Language::Rust, "pub fn a() {}\n");
    let modified = indexable("src/a.rs", Language::Rust, "pub fn a() { 1 }\n");
    let added = indexable("src/new.rs", Language::Rust, "pub fn n() {}\n");

    assert!(!StructuralIndex::has_changed(&previous, &unchanged));
    assert!(StructuralIndex::has_changed(&previous, &modified));
    assert!(StructuralIndex::has_changed(&previous, &added));
}

// ---------------------------------------------------------------------------
// IndexBuild 流水线
// ---------------------------------------------------------------------------

/// 构建必须把扫描与解析串起来，产出可查询的索引。
#[test]
fn build_indexes_repository_end_to_end() {
    let host = RepoHost::new(&[
        ("src/lib.rs", "pub mod inner;\npub fn top() {}\n"),
        (
            "src/inner.rs",
            "pub struct Inner;\nimpl Inner { pub fn run(&self) {} }\n",
        ),
        (
            "app/main.py",
            "class Service:\n    def run(self):\n        pass\n",
        ),
        ("README.md", "# 项目\n说明文字。\n"),
        ("node_modules/pkg/index.js", "module.exports = 1;"),
    ]);

    let build = build_all(&host);
    let index = build.index();

    assert!(build.is_complete());
    assert_eq!(index.stats().files, 4, "node_modules 必须被忽略");
    assert!(index
        .symbols()
        .any(|symbol| symbol.qualified_name == "Inner::run"));
    assert!(index
        .symbols()
        .any(|symbol| symbol.qualified_name == "Service.run"));
    // Markdown 没有解析器，走文本兜底但仍进入索引。
    assert_eq!(index.stats().text_files, 1);
}

/// 构建必须只读取每个文件一次。
#[test]
fn build_reads_each_file_once() {
    let host = RepoHost::new(&[
        ("src/a.rs", "pub fn a() {}\n"),
        ("src/b.rs", "pub fn b() {}\n"),
        ("src/c.rs", "pub fn c() {}\n"),
    ]);

    build_all(&host);

    // 2 次目录列举（根、src）+ 3 次文件读取。解析复用扫描读到的正文，不额外重读。
    assert_eq!(host.file_calls(), 5, "解析不得导致文件被读第二遍");
}

/// 分片构建必须产出与一次跑完完全相同的索引。
#[test]
fn sharded_build_matches_single_pass() {
    let files: &[(&str, &str)] = &[
        ("src/a.rs", "pub fn a() {}\npub struct SA;\n"),
        ("src/nested/b.ts", "export class B { run() {} }\n"),
        ("src/nested/deep/c.py", "def c():\n    pass\n"),
        ("go/main.go", "package main\n\nfunc Main() {}\n"),
        ("docs/guide.md", "# 指南\n内容。\n"),
    ];
    let host = RepoHost::new(files);

    let single = build_all(&host).into_index();

    let mut sharded = IndexBuild::with_default_limits(repository());
    let mut rounds = 0;
    while sharded.advance(
        &host,
        ScanBudget {
            max_host_calls: 1,
            max_bytes_read: 1,
        },
    ) == ScanProgress::Incomplete
    {
        rounds += 1;
        assert!(rounds < 1000, "分片构建不应无法收敛");
    }
    let sharded = sharded.into_index();

    assert!(rounds > 3, "预算为 1 时必须发生多轮中断，实际 {rounds} 轮");
    assert_eq!(single.stats(), sharded.stats());

    let single_ids: Vec<&str> = single.chunks().map(|chunk| chunk.id.as_str()).collect();
    let sharded_ids: Vec<&str> = sharded.chunks().map(|chunk| chunk.id.as_str()).collect();
    assert_eq!(single_ids, sharded_ids, "分片不得改变 chunk 身份");
}

/// 未完成的构建必须如实报告，且已处理部分可查询。
#[test]
fn incomplete_build_reports_partial_state() {
    let host = RepoHost::new(&[
        ("src/a.rs", "pub fn a() {}\n"),
        ("src/b.rs", "pub fn b() {}\n"),
        ("src/c.rs", "pub fn c() {}\n"),
    ]);

    let mut build = IndexBuild::with_default_limits(repository());
    let progress = build.advance(
        &host,
        ScanBudget {
            max_host_calls: 2,
            max_bytes_read: u64::MAX,
        },
    );

    assert_eq!(progress, ScanProgress::Incomplete);
    assert!(!build.is_complete());
    assert!(build.index().stats().files < 3, "未完成时不应全部就绪");
}

/// 单个文件读取失败不得中断整个构建。
#[test]
fn build_isolates_file_failures() {
    let host = RepoHost::new(&[
        ("src/a.rs", "pub fn a() {}\n"),
        ("src/broken.rs", "irrelevant"),
        ("src/c.rs", "pub fn c() {}\n"),
    ])
    .with_unreadable("src/broken.rs");

    let build = build_all(&host);

    assert_eq!(build.index().stats().files, 2);
    assert!(!build.warnings().is_empty(), "失败必须留下告警");
    assert!(build.index().file("src/a.rs").is_some());
    assert!(build.index().file("src/c.rs").is_some());
}

/// 语法错误的文件必须以文本兜底进入索引，而不是让构建失败。
#[test]
fn build_falls_back_to_text_on_parse_errors() {
    let host = RepoHost::new(&[
        ("src/good.rs", "pub fn good() {}\n"),
        ("src/bad.rs", "fn broken( { ]]] not rust\n"),
    ]);

    let build = build_all(&host);
    let index = build.index();

    assert_eq!(index.stats().files, 2);
    assert_eq!(
        index.file("src/bad.rs").expect("坏文件仍应入索引").tier,
        ParseTier::Text
    );
    assert_eq!(
        index.file("src/good.rs").expect("好文件").tier,
        ParseTier::Structural
    );
}

/// 空仓库必须正常完成并产出空索引。
#[test]
fn build_handles_empty_repository() {
    let host = RepoHost::new(&[]);
    let build = build_all(&host);

    assert!(build.is_complete());
    assert!(build.index().is_empty());
}

// ---------------------------------------------------------------------------
// 持久化与重启恢复
// ---------------------------------------------------------------------------

/// 哈希表必须能落盘并原样读回。
#[test]
fn file_hashes_round_trip_through_store() {
    let host = RepoHost::new(&[
        ("src/a.rs", "pub fn a() {}\n"),
        ("src/b.rs", "pub fn b() {}\n"),
    ]);
    let index = build_all(&host).into_index();
    let store = IndexStore::probe(&host);

    store
        .save_file_hashes(&host, &index.file_hashes())
        .expect("保存哈希表");
    let loaded = store.load_file_hashes(&host);

    let expected: std::collections::BTreeMap<String, String> =
        index.file_hashes().into_iter().collect();
    assert_eq!(loaded, expected);
}

/// 重启后按同一仓库重建，必须得到完全一致的索引。
///
/// 结构索引内存驻留、不落盘，因此"恢复"的含义是**重建结果确定且与上一轮一致**，
/// 而不是把索引本体读回来（见 adr-storage.md）。
#[test]
fn rebuild_after_restart_reproduces_same_index() {
    let files: &[(&str, &str)] = &[
        (
            "src/a.rs",
            "pub struct A;\nimpl A { pub fn run(&self) {} }\n",
        ),
        ("src/b.ts", "export class B { go() {} }\n"),
    ];
    let host = RepoHost::new(files);

    let first = build_all(&host).into_index();
    let store = IndexStore::probe(&host);
    store
        .save_file_hashes(&host, &first.file_hashes())
        .expect("保存哈希表");

    // 模拟进程重启：新宿主沿用同一份持久存储，索引本体从零重建。
    let restarted = RepoHost::restored_from(files, host.snapshot());
    let second = build_all(&restarted).into_index();

    assert_eq!(first.stats(), second.stats());
    assert_eq!(first.file_hashes(), second.file_hashes());
    let first_ids: Vec<&str> = first.chunks().map(|chunk| chunk.id.as_str()).collect();
    let second_ids: Vec<&str> = second.chunks().map(|chunk| chunk.id.as_str()).collect();
    assert_eq!(first_ids, second_ids, "重建必须产出同一批 chunk 身份");

    // 上一轮落盘的哈希表必须与重建结果一致，这正是增量索引的基线。
    let persisted = IndexStore::probe(&restarted).load_file_hashes(&restarted);
    let rebuilt: std::collections::BTreeMap<String, String> =
        second.file_hashes().into_iter().collect();
    assert_eq!(persisted, rebuilt);
}

/// 存储不可用时哈希表写入必须失败，且不得写进用户级作用域。
#[test]
fn hash_persistence_requires_workspace_scope() {
    let host = RepoHost::without_workspace_scope(&[("src/a.rs", "pub fn a() {}\n")]);
    let index = build_all(&host).into_index();
    let store = IndexStore::probe(&host);

    assert!(store.save_file_hashes(&host, &index.file_hashes()).is_err());
    assert!(store.load_file_hashes(&host).is_empty());
    assert!(
        host.user_keys().is_empty(),
        "不得把项目哈希表写进用户级存储"
    );
}

/// 清理后哈希表必须消失。
#[test]
fn clearing_hashes_removes_them() {
    let host = RepoHost::new(&[("src/a.rs", "pub fn a() {}\n")]);
    let index = build_all(&host).into_index();
    let store = IndexStore::probe(&host);
    store
        .save_file_hashes(&host, &index.file_hashes())
        .expect("保存哈希表");

    store.clear_file_hashes(&host).expect("清理哈希表");
    assert!(store.load_file_hashes(&host).is_empty());
}

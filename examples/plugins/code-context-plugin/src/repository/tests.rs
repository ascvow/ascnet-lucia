//! 仓库扫描器的单元测试。
//!
//! 覆盖 Phase 2 的 Definition of Done：正常文件、忽略规则、嵌套 `.gitignore`、二进制、
//! 超大文件、未识别语言、敏感文件、深度与规模上限、错误隔离、分片恢复与输出确定性。

use super::*;
use crate::testing::RepoHost;

/// 用默认上限完整扫描一个虚拟仓库。
///
/// 预算给得足够大，使单次 `advance` 即可完成；分片行为由专门的测试覆盖。
fn scan_all(host: &RepoHost) -> RepositoryScan {
    let mut scan = RepositoryScan::with_default_limits();
    let progress = scan.advance(
        host,
        ScanBudget {
            max_host_calls: u32::MAX,
            max_bytes_read: u64::MAX,
        },
    );
    assert_eq!(progress, ScanProgress::Complete, "预算充足时应一次扫完");
    scan
}

/// 返回扫描收录的全部路径。
fn indexed_paths(scan: &RepositoryScan) -> Vec<&str> {
    scan.files().iter().map(|file| file.path.as_str()).collect()
}

/// 常规源码文件必须被收录，并带上语言与内容哈希。
#[test]
fn indexes_ordinary_source_files() {
    let host = RepoHost::new(&[
        ("src/lib.rs", "pub fn run() {}\n"),
        ("src/app.ts", "export const x = 1;\n"),
        ("README.md", "# 标题\n"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(
        indexed_paths(&scan),
        vec!["README.md", "src/app.ts", "src/lib.rs"]
    );
    let lib = &scan.files()[2];
    assert_eq!(lib.language, Language::Rust);
    assert_eq!(lib.byte_len, 16);
    assert_eq!(lib.line_count, 1);
    assert!(!lib.content_hash.is_empty());
    assert_eq!(scan.stats().files_indexed, 3);
}

/// 输出必须按路径升序，且与目录遍历顺序无关。
#[test]
fn output_is_sorted_and_deterministic() {
    let host = RepoHost::new(&[
        ("z/last.rs", "fn z() {}"),
        ("a/first.rs", "fn a() {}"),
        ("m/mid.rs", "fn m() {}"),
        ("root.rs", "fn root() {}"),
    ]);

    let first = indexed_paths(&scan_all(&host))
        .iter()
        .map(|path| path.to_string())
        .collect::<Vec<_>>();
    let second = indexed_paths(&scan_all(&host))
        .iter()
        .map(|path| path.to_string())
        .collect::<Vec<_>>();

    assert_eq!(
        first,
        vec!["a/first.rs", "m/mid.rs", "root.rs", "z/last.rs"]
    );
    assert_eq!(first, second, "两次扫描必须产出完全相同的顺序");
}

/// 相同内容必须得到相同哈希，不同内容必须不同。
#[test]
fn content_hash_tracks_content() {
    let host = RepoHost::new(&[
        ("a.rs", "fn same() {}"),
        ("b.rs", "fn same() {}"),
        ("c.rs", "fn other() {}"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(scan.files()[0].content_hash, scan.files()[1].content_hash);
    assert_ne!(scan.files()[0].content_hash, scan.files()[2].content_hash);
}

/// 内置默认忽略必须挡住依赖与产物目录，且不读取其中任何文件。
#[test]
fn default_ignores_exclude_dependency_and_build_dirs() {
    let host = RepoHost::new(&[
        ("src/main.rs", "fn main() {}"),
        ("node_modules/pkg/index.js", "module.exports = 1;"),
        ("target/debug/build.rs", "fn built() {}"),
        ("dist/bundle.js", "var a=1;"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(indexed_paths(&scan), vec!["src/main.rs"]);
    assert!(scan.stats().entries_ignored >= 3);
}

/// `.git` 必须被硬排除，即使用户显式否定。
#[test]
fn git_directory_is_hard_excluded() {
    let host = RepoHost::new(&[
        ("src/main.rs", "fn main() {}"),
        (".gitignore", "!.git\n!.git/\n"),
        (".git/config", "[core]\n"),
        (".git/objects/ab/cdef", "binary-ish"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(indexed_paths(&scan), vec!["src/main.rs"]);
}

/// 根 `.gitignore` 必须生效。
#[test]
fn root_gitignore_is_honored() {
    let host = RepoHost::new(&[
        (".gitignore", "*.log\nscratch/\n"),
        ("src/main.rs", "fn main() {}"),
        ("debug.log", "noise"),
        ("scratch/tmp.rs", "fn tmp() {}"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(indexed_paths(&scan), vec!["src/main.rs"]);
}

/// 嵌套 `.gitignore` 必须在其子树内生效并可覆盖上层判定。
#[test]
fn nested_gitignore_overrides_parent() {
    let host = RepoHost::new(&[
        (".gitignore", "*.gen.rs\n"),
        ("src/a.gen.rs", "fn a() {}"),
        ("crates/core/.gitignore", "!*.gen.rs\n"),
        ("crates/core/b.gen.rs", "fn b() {}"),
        ("crates/other/c.gen.rs", "fn c() {}"),
    ]);
    let scan = scan_all(&host);

    // 深层否定规则重新包含 crates/core 下的文件，其余目录仍受根规则约束。
    assert_eq!(indexed_paths(&scan), vec!["crates/core/b.gen.rs"]);
}

/// `.agentignore` 必须生效且能覆盖 `.gitignore` 的判定。
#[test]
fn agentignore_takes_precedence_over_gitignore() {
    let host = RepoHost::new(&[
        (".gitignore", "generated/\n"),
        (".agentignore", "!generated/\ndocs/\n"),
        ("generated/api.ts", "export type A = 1;"),
        ("docs/guide.md", "# 指南"),
        ("src/main.rs", "fn main() {}"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(
        indexed_paths(&scan),
        vec!["generated/api.ts", "src/main.rs"],
        ".agentignore 应能重新包含 git 忽略的目录并额外排除 docs"
    );
}

/// 敏感文件必须被排除并单独计数。
#[test]
fn sensitive_files_are_excluded_and_counted() {
    let host = RepoHost::new(&[
        ("src/main.rs", "fn main() {}"),
        (".env", "API_KEY=abc"),
        ("certs/server.pem", "-----BEGIN-----"),
        ("config/secrets.json", "{\"token\":\"x\"}"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(indexed_paths(&scan), vec!["src/main.rs"]);
    assert_eq!(scan.stats().files_sensitive, 3);
    // 敏感文件不得被读取——读取本身就意味着内容进入了插件内存。
    // 4 次目录列举（根、src、certs、config）+ 1 次源码读取。
    assert_eq!(host.file_calls(), 5, "敏感文件不得产生读取调用");
}

/// 定义凭证逻辑的源码不得被误判为敏感。
#[test]
fn source_files_named_after_secrets_are_indexed() {
    let host = RepoHost::new(&[
        ("src/secrets.rs", "pub struct Secrets;"),
        ("src/credentials.ts", "export class Credentials {}"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(
        indexed_paths(&scan),
        vec!["src/credentials.ts", "src/secrets.rs"]
    );
    assert_eq!(scan.stats().files_sensitive, 0);
}

/// 已知二进制扩展名必须在读取前被跳过。
#[test]
fn binary_extensions_are_skipped_without_reading() {
    let host = RepoHost::new(&[
        ("src/main.rs", "fn main() {}"),
        ("assets/logo.png", "\u{fffd}PNG"),
        ("model/weights.gguf", "GGUF"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(indexed_paths(&scan), vec!["src/main.rs"]);
    assert_eq!(scan.stats().files_skipped, 2);
    // 4 次目录列举（根、src、assets、model）+ 1 次源码读取。
    assert_eq!(host.file_calls(), 5, "二进制文件不应产生读取调用");
}

/// 读取失败（例如漏网的二进制）必须被隔离为告警而不是中断扫描。
#[test]
fn read_failure_is_isolated() {
    let host = RepoHost::new(&[
        ("src/a.rs", "fn a() {}"),
        ("src/broken.rs", "irrelevant"),
        ("src/b.rs", "fn b() {}"),
    ])
    .with_unreadable("src/broken.rs");
    let scan = scan_all(&host);

    assert_eq!(indexed_paths(&scan), vec!["src/a.rs", "src/b.rs"]);
    assert!(scan.warnings().iter().any(|warning| {
        warning.path == "src/broken.rs"
            && matches!(warning.kind, ScanWarningKind::ReadFailed { .. })
    }));
}

/// 目录列举失败必须被隔离，其他目录照常扫描。
#[test]
fn list_failure_is_isolated() {
    let host = RepoHost::new(&[
        ("src/a.rs", "fn a() {}"),
        ("locked/secret.rs", "fn locked() {}"),
    ])
    .with_unlistable("locked");
    let scan = scan_all(&host);

    assert_eq!(indexed_paths(&scan), vec!["src/a.rs"]);
    assert!(scan.warnings().iter().any(|warning| {
        warning.path == "locked" && matches!(warning.kind, ScanWarningKind::ListFailed { .. })
    }));
}

/// 内嵌 NUL 的文件必须被判定为二进制并跳过。
#[test]
fn files_with_nul_bytes_are_rejected() {
    let host = RepoHost::new(&[("src/a.rs", "fn a() {}"), ("src/blob.rs", "head\0tail")]);
    let scan = scan_all(&host);

    assert_eq!(indexed_paths(&scan), vec!["src/a.rs"]);
    assert!(scan
        .warnings()
        .iter()
        .any(|warning| warning.kind == ScanWarningKind::BinaryContent));
}

/// 超过体量上限的文件必须被跳过并告警。
#[test]
fn oversized_files_are_skipped() {
    let large = "x".repeat(2048);
    let host = RepoHost::new(&[("src/a.rs", "fn a() {}"), ("src/huge.rs", large.as_str())]);
    let mut scan = RepositoryScan::new(ScanLimits {
        max_file_bytes: 1024,
        ..ScanLimits::default()
    });
    scan.advance(
        &host,
        ScanBudget {
            max_host_calls: u32::MAX,
            max_bytes_read: u64::MAX,
        },
    );

    assert_eq!(indexed_paths(&scan), vec!["src/a.rs"]);
    assert!(scan
        .warnings()
        .iter()
        .any(|warning| warning.kind == ScanWarningKind::TooLarge { bytes: 2048 }));
}

/// 未识别扩展名必须跳过，不得当作文本读取。
#[test]
fn unrecognized_extensions_are_skipped() {
    let host = RepoHost::new(&[("src/main.rs", "fn main() {}"), ("data/blob.xyz", "?")]);
    let scan = scan_all(&host);

    assert_eq!(indexed_paths(&scan), vec!["src/main.rs"]);
    assert_eq!(scan.stats().files_skipped, 1);
}

/// 未覆盖结构化解析的语言仍必须进入索引。
#[test]
fn languages_without_structural_parser_are_still_indexed() {
    let host = RepoHost::new(&[
        ("src/App.vue", "<template></template>"),
        ("src/Main.java", "class Main {}"),
        ("config.toml", "[a]\nb = 1"),
    ]);
    let scan = scan_all(&host);

    assert_eq!(
        indexed_paths(&scan),
        vec!["config.toml", "src/App.vue", "src/Main.java"]
    );
    assert!(scan
        .files()
        .iter()
        .all(|file| !file.language.supports_structural_parsing()));
}

/// 超过深度上限的目录不得继续下探。
#[test]
fn depth_limit_stops_descent() {
    let host = RepoHost::new(&[
        ("a/f.rs", "fn a() {}"),
        ("a/b/f.rs", "fn b() {}"),
        ("a/b/c/f.rs", "fn c() {}"),
    ]);
    let mut scan = RepositoryScan::new(ScanLimits {
        max_depth: 2,
        ..ScanLimits::default()
    });
    scan.advance(
        &host,
        ScanBudget {
            max_host_calls: u32::MAX,
            max_bytes_read: u64::MAX,
        },
    );

    assert_eq!(indexed_paths(&scan), vec!["a/b/f.rs", "a/f.rs"]);
    assert!(scan
        .warnings()
        .iter()
        .any(|warning| warning.kind == ScanWarningKind::DepthExceeded));
}

/// 文件数量上限必须生效，且不得让扫描失败。
#[test]
fn file_limit_is_enforced() {
    let host = RepoHost::new(&[
        ("a.rs", "fn a() {}"),
        ("b.rs", "fn b() {}"),
        ("c.rs", "fn c() {}"),
    ]);
    let mut scan = RepositoryScan::new(ScanLimits {
        max_files: 2,
        ..ScanLimits::default()
    });
    let progress = scan.advance(
        &host,
        ScanBudget {
            max_host_calls: u32::MAX,
            max_bytes_read: u64::MAX,
        },
    );

    assert_eq!(progress, ScanProgress::Complete);
    assert_eq!(scan.files().len(), 2);
    assert!(scan
        .warnings()
        .iter()
        .any(|warning| warning.kind == ScanWarningKind::LimitReached));
}

/// 告警数量必须有上限，但总数仍如实计数。
#[test]
fn warnings_are_capped_but_counted() {
    let files: Vec<(String, String)> = (0..20)
        .map(|index| (format!("data/blob{index}.rs"), "x".repeat(64)))
        .collect();
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&borrowed);

    let mut scan = RepositoryScan::new(ScanLimits {
        max_file_bytes: 1,
        max_warnings: 5,
        ..ScanLimits::default()
    });
    scan.advance(
        &host,
        ScanBudget {
            max_host_calls: u32::MAX,
            max_bytes_read: u64::MAX,
        },
    );

    assert_eq!(scan.warnings().len(), 5, "保留的告警必须受上限约束");
    assert_eq!(scan.stats().warnings_total, 20, "总数必须如实反映");
}

/// 分片推进必须产出与一次扫完完全相同的结果。
#[test]
fn sharded_scan_matches_single_pass() {
    let host = RepoHost::new(&[
        (".gitignore", "*.log\n"),
        ("src/a.rs", "fn a() {}"),
        ("src/nested/b.ts", "export const b = 1;"),
        ("src/nested/deep/c.py", "def c(): pass"),
        ("docs/guide.md", "# 指南"),
        ("noise.log", "ignored"),
    ]);

    let single = scan_all(&host);

    // 每次只允许一次宿主调用，强制在每个目录与每个文件之间都发生一次中断。
    let mut sharded = RepositoryScan::with_default_limits();
    let mut rounds = 0;
    loop {
        rounds += 1;
        assert!(rounds < 1000, "分片扫描不应无法收敛");
        if sharded.advance(
            &host,
            ScanBudget {
                max_host_calls: 1,
                max_bytes_read: 1,
            },
        ) == ScanProgress::Complete
        {
            break;
        }
    }

    assert!(rounds > 5, "预算为 1 时必须发生多轮中断，实际 {rounds} 轮");
    assert_eq!(single.files(), sharded.files());
    assert_eq!(single.stats().files_indexed, sharded.stats().files_indexed);
}

/// 未完成的扫描必须如实报告状态，不得提前标记完成。
#[test]
fn incomplete_scan_reports_incomplete() {
    let host = RepoHost::new(&[
        ("src/a.rs", "fn a() {}"),
        ("src/b.rs", "fn b() {}"),
        ("src/c.rs", "fn c() {}"),
    ]);
    let mut scan = RepositoryScan::with_default_limits();

    let progress = scan.advance(
        &host,
        ScanBudget {
            max_host_calls: 1,
            max_bytes_read: u64::MAX,
        },
    );
    assert_eq!(progress, ScanProgress::Incomplete);
    assert!(!scan.is_complete());
}

/// 预算必须真正约束宿主调用次数。
#[test]
fn budget_bounds_host_calls() {
    let files: Vec<(String, String)> = (0..50)
        .map(|index| (format!("src/f{index}.rs"), format!("fn f{index}() {{}}")))
        .collect();
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, content)| (path.as_str(), content.as_str()))
        .collect();
    let host = RepoHost::new(&borrowed);

    let mut scan = RepositoryScan::with_default_limits();
    scan.advance(
        &host,
        ScanBudget {
            max_host_calls: 4,
            max_bytes_read: u64::MAX,
        },
    );

    assert!(
        host.file_calls() <= 4,
        "实际调用 {} 次，超出预算",
        host.file_calls()
    );
}

/// 空仓库必须正常完成而不是报错。
#[test]
fn empty_repository_completes() {
    let host = RepoHost::new(&[]);
    let scan = scan_all(&host);

    assert!(scan.files().is_empty());
    assert!(scan.is_complete());
    assert_eq!(scan.stats().files_indexed, 0);
}

/// 完成后重复推进必须保持完成状态且不再发起宿主调用。
#[test]
fn advancing_completed_scan_is_a_noop() {
    let host = RepoHost::new(&[("src/a.rs", "fn a() {}")]);
    let mut scan = scan_all(&host);
    let calls_before = host.file_calls();

    let progress = scan.advance(&host, ScanBudget::default());

    assert_eq!(progress, ScanProgress::Complete);
    assert_eq!(host.file_calls(), calls_before);
}

/// 哈希映射必须覆盖全部收录文件，供增量判断使用。
#[test]
fn hash_index_covers_every_indexed_file() {
    let host = RepoHost::new(&[("src/a.rs", "fn a() {}"), ("src/b.rs", "fn b() {}")]);
    let scan = scan_all(&host);
    let index = hash_index(scan.files());

    assert_eq!(index.len(), 2);
    assert_eq!(index[0].0, "src/a.rs");
    assert_eq!(index[0].1, scan.files()[0].content_hash);
}

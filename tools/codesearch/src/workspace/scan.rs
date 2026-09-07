//! 工作目录扫描与过滤。
//!
//! 过滤分为三层：`.gitignore`/`.codesearchignore`（由 `ignore` crate 处理）、噪声排除规则、
//! 敏感文件规则。噪声与敏感规则分开配置，均使用 gitignore 语法（支持 `!` 取反）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::error::{Error, Result};
use crate::types::Language;
use crate::workspace::Workspace;

/// 自定义忽略文件名。
pub const CUSTOM_IGNORE_FILE: &str = ".codesearchignore";

/// 用于二进制嗅探的前缀长度。
const SNIFF_BYTES: usize = 8 * 1024;

/// 常见二进制扩展名：直接跳过，不读取内容。
const BINARY_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "bmp", "tiff", "psd", "pdf", "zip", "gz", "tgz",
    "tar", "bz2", "xz", "zst", "7z", "rar", "jar", "war", "class", "o", "a", "so", "dylib", "dll",
    "exe", "wasm", "woff", "woff2", "ttf", "otf", "eot", "mp3", "mp4", "mov", "avi", "mkv", "wav",
    "flac", "ogg", "sqlite", "sqlite3", "db", "bin", "dat", "pyc", "pyo", "node", "lockb", "blend",
    "usearch",
];

/// 扫描到的候选文件（尚未读取内容）。
#[derive(Debug, Clone)]
pub struct ScannedFile {
    /// `/` 分隔的相对路径。
    pub rel_path: String,
    /// 绝对路径。
    pub abs_path: PathBuf,
    /// 文件大小（字节）。
    pub size: u64,
    /// 修改时间（自 UNIX 纪元的纳秒），不可用时为 0。
    pub mtime_ns: i64,
}

/// 跳过原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// 匹配敏感文件规则。
    Sensitive,
    /// 超过大小上限。
    TooLarge,
    /// 二进制扩展名或内容包含 NUL 字节。
    Binary,
    /// 内容不是合法 UTF-8。
    NotUtf8,
    /// 读取失败（权限、竞争删除等）。
    ReadError,
    /// 符号链接（默认不跟随）。
    Symlink,
}

impl SkipReason {
    /// 稳定的字符串标识（写入数据库）。
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::Sensitive => "sensitive",
            SkipReason::TooLarge => "too_large",
            SkipReason::Binary => "binary",
            SkipReason::NotUtf8 => "not_utf8",
            SkipReason::ReadError => "read_error",
            SkipReason::Symlink => "symlink",
        }
    }
}

/// 被跳过的文件及原因。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedFile {
    /// 相对路径。
    pub path: String,
    /// 原因。
    pub reason: SkipReason,
    /// 补充说明（例如大小或错误文本）。
    pub detail: String,
}

/// 一次扫描的输出。
#[derive(Debug, Default)]
pub struct ScanOutput {
    /// 通过全部规则的候选文件。
    pub files: Vec<ScannedFile>,
    /// 因规则或读取问题被跳过的文件（噪声规则排除的文件不记录）。
    pub skipped: Vec<SkippedFile>,
}

/// 已读取并校验的源码快照。
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// UTF-8 源码文本。
    pub text: String,
    /// blake3 十六进制内容哈希。
    pub hash: String,
    /// 文件大小。
    pub size: u64,
    /// 修改时间（纳秒）。
    pub mtime_ns: i64,
}

/// 文件过滤器：噪声规则 + 敏感规则 + include/exclude。
#[derive(Debug)]
pub struct FileFilter {
    noise: Gitignore,
    sensitive: Gitignore,
    include: Option<Gitignore>,
    root: PathBuf,
}

impl FileFilter {
    /// 根据配置构建过滤器。
    pub fn new(workspace: &Workspace, config: &Config) -> Result<Self> {
        let root = workspace.root().to_path_buf();
        let noise = build_gitignore(&root, config.effective_excludes().iter())?;
        let sensitive = build_gitignore(&root, config.effective_sensitive_excludes().iter())?;
        let include = if config.index.include.is_empty() {
            None
        } else {
            Some(build_gitignore(&root, config.index.include.iter())?)
        };
        Ok(Self {
            noise,
            sensitive,
            include,
            root,
        })
    }

    /// 路径是否被噪声规则排除（目录命中时整棵子树被剪掉）。
    pub fn is_noise(&self, abs_path: &Path, is_dir: bool) -> bool {
        self.noise.matched(abs_path, is_dir).is_ignore()
    }

    /// 文件是否命中敏感规则。
    pub fn is_sensitive(&self, abs_path: &Path) -> bool {
        self.sensitive.matched(abs_path, false).is_ignore()
    }

    /// 文件是否满足 include 规则（未配置 include 时总为真）。
    pub fn is_included(&self, abs_path: &Path) -> bool {
        match &self.include {
            None => true,
            // include 规则用 gitignore 语法表达"匹配即纳入"，因此 Ignore 代表命中。
            Some(include) => include.matched(abs_path, false).is_ignore(),
        }
    }

    /// 对单个路径做完整分类（供文件监听事件与单路径刷新使用）。
    pub fn classify(&self, abs_path: &Path) -> Classification {
        if self.is_noise(abs_path, false) {
            return Classification::Ignore;
        }
        let mut current = abs_path.parent();
        while let Some(dir) = current {
            if dir == self.root || !dir.starts_with(&self.root) {
                break;
            }
            if self.is_noise(dir, true) {
                return Classification::Ignore;
            }
            current = dir.parent();
        }
        if !self.is_included(abs_path) {
            return Classification::Ignore;
        }
        if self.is_sensitive(abs_path) {
            return Classification::Skip(SkipReason::Sensitive);
        }
        if has_binary_extension(abs_path) {
            return Classification::Skip(SkipReason::Binary);
        }
        Classification::Index
    }
}

/// 单个路径的分类结果。
///
/// 区分 "跳过并记录原因" 与 "根本不属于索引范围"：
/// 前者对用户可见，后者（噪声目录、未命中 include）不应污染跳过列表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    /// 应当索引。
    Index,
    /// 应当跳过，并记录可观察的原因。
    Skip(SkipReason),
    /// 不属于索引范围，直接忽略且不记录。
    Ignore,
}

fn build_gitignore<'a>(
    root: &Path,
    patterns: impl Iterator<Item = &'a String>,
) -> Result<Gitignore> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        builder
            .add_line(None, pattern)
            .map_err(|e| Error::Config(format!("glob 规则 `{pattern}` 无效: {e}")))?;
    }
    builder
        .build()
        .map_err(|e| Error::Config(format!("glob 规则编译失败: {e}")))
}

/// 扩展名是否属于已知二进制类型。
pub fn has_binary_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            let lower = ext.to_ascii_lowercase();
            BINARY_EXTENSIONS.contains(&lower.as_str())
        })
        .unwrap_or(false)
}

/// 扫描整个 workspace。
///
/// 使用 `ignore` 处理 `.gitignore` 与 `.codesearchignore`，不跟随符号链接。
/// 返回的文件列表按路径排序，保证确定性。
pub fn scan_workspace(workspace: &Workspace, config: &Config) -> Result<ScanOutput> {
    let filter = Arc::new(FileFilter::new(workspace, config)?);
    let root = workspace.root();
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .follow_links(false)
        .git_ignore(config.index.respect_gitignore)
        .git_exclude(config.index.respect_gitignore)
        .git_global(false)
        .require_git(false)
        .add_custom_ignore_filename(CUSTOM_IGNORE_FILE);
    if !config.index.exclude.is_empty() {
        // 用户 exclude 通过 override 表达（`!` 前缀表示排除）。
        let mut overrides = OverrideBuilder::new(root);
        for pattern in &config.index.exclude {
            let negated = if let Some(rest) = pattern.strip_prefix('!') {
                rest.to_string()
            } else {
                format!("!{pattern}")
            };
            overrides
                .add(&negated)
                .map_err(|e| Error::Config(format!("exclude 规则 `{pattern}` 无效: {e}")))?;
        }
        let overrides = overrides
            .build()
            .map_err(|e| Error::Config(format!("exclude 规则编译失败: {e}")))?;
        builder.overrides(overrides);
    }
    let root_for_filter = root.to_path_buf();
    let noise_filter = Arc::clone(&filter);
    builder.filter_entry(move |entry| {
        if entry.path() == root_for_filter {
            return true;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        !noise_filter.is_noise(entry.path(), is_dir)
    });

    let mut output = ScanOutput::default();
    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::debug!(error = %error, "扫描条目失败");
                continue;
            }
        };
        let path = entry.path();
        if path == root {
            continue;
        }
        let Some(rel_path) = workspace.relativize(path) else {
            continue;
        };
        let Some(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            output.skipped.push(SkippedFile {
                path: rel_path,
                reason: SkipReason::Symlink,
                detail: "默认不跟随符号链接".to_string(),
            });
            continue;
        }
        if !file_type.is_file() {
            continue;
        }
        if !filter.is_included(path) {
            continue;
        }
        if filter.is_sensitive(path) {
            output.skipped.push(SkippedFile {
                path: rel_path,
                reason: SkipReason::Sensitive,
                detail: "匹配敏感文件规则".to_string(),
            });
            continue;
        }
        if has_binary_extension(path) {
            output.skipped.push(SkippedFile {
                path: rel_path,
                reason: SkipReason::Binary,
                detail: "二进制扩展名".to_string(),
            });
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                output.skipped.push(SkippedFile {
                    path: rel_path,
                    reason: SkipReason::ReadError,
                    detail: error.to_string(),
                });
                continue;
            }
        };
        let size = metadata.len();
        if size > config.index.max_file_size_bytes {
            output.skipped.push(SkippedFile {
                path: rel_path,
                reason: SkipReason::TooLarge,
                detail: format!("{size} 字节 > 上限 {}", config.index.max_file_size_bytes),
            });
            continue;
        }
        output.files.push(ScannedFile {
            rel_path,
            abs_path: path.to_path_buf(),
            size,
            mtime_ns: mtime_ns(&metadata),
        });
    }
    output.files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    output.skipped.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(output)
}

/// 读取元数据中的修改时间（纳秒）。
pub fn mtime_ns(metadata: &std::fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// 读取文件快照：校验大小、二进制与 UTF-8，并计算内容哈希。
///
/// 返回 `Ok(Err(skip))` 表示文件应被跳过并记录原因。
pub fn read_snapshot(
    abs_path: &Path,
    max_size: u64,
) -> Result<std::result::Result<Snapshot, (SkipReason, String)>> {
    let metadata = match std::fs::symlink_metadata(abs_path) {
        Ok(metadata) => metadata,
        Err(error) => return Ok(Err((SkipReason::ReadError, error.to_string()))),
    };
    if metadata.file_type().is_symlink() {
        return Ok(Err((SkipReason::Symlink, "默认不跟随符号链接".to_string())));
    }
    if metadata.len() > max_size {
        return Ok(Err((
            SkipReason::TooLarge,
            format!("{} 字节 > 上限 {max_size}", metadata.len()),
        )));
    }
    let bytes = match std::fs::read(abs_path) {
        Ok(bytes) => bytes,
        Err(error) => return Ok(Err((SkipReason::ReadError, error.to_string()))),
    };
    if bytes.len() as u64 > max_size {
        return Ok(Err((
            SkipReason::TooLarge,
            format!("{} 字节 > 上限 {max_size}", bytes.len()),
        )));
    }
    let sniff = &bytes[..bytes.len().min(SNIFF_BYTES)];
    if sniff.contains(&0u8) {
        return Ok(Err((SkipReason::Binary, "内容包含 NUL 字节".to_string())));
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => {
            return Ok(Err((
                SkipReason::NotUtf8,
                format!(
                    "字节偏移 {} 处不是合法 UTF-8",
                    error.utf8_error().valid_up_to()
                ),
            )))
        }
    };
    let hash = blake3::hash(text.as_bytes()).to_hex().to_string();
    Ok(Ok(Snapshot {
        size: text.len() as u64,
        mtime_ns: mtime_ns(&metadata),
        text,
        hash,
    }))
}

/// 按扩展名与文件名推断语言。
pub fn detect_language(rel_path: &str) -> Language {
    let file_name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let ext = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "rs" => Language::Rust,
        "ts" | "mts" | "cts" => Language::TypeScript,
        "tsx" => Language::Tsx,
        "js" | "mjs" | "cjs" | "jsx" => Language::JavaScript,
        "md" | "markdown" | "mdx" => Language::Markdown,
        _ => Language::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, content).expect("write");
    }

    #[test]
    fn scan_applies_noise_sensitive_and_ignore_rules() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/lib.rs", b"pub fn a() {}\n");
        write(root, "node_modules/x/index.js", b"module.exports = 1;\n");
        write(root, "target/debug/out.rs", b"fn generated() {}\n");
        write(root, ".env", b"SECRET=1\n");
        write(root, ".env.example", b"SECRET=\n");
        write(root, "id_rsa", b"-----BEGIN-----\n");
        write(root, "logo.png", b"\x89PNG\r\n");
        write(root, "blob.txt", b"abc\0def");
        write(root, "ignored.log", b"log\n");
        write(root, ".codesearchignore", b"*.log\n");
        write(root, "docs/guide.md", "# 指南\n中文内容".as_bytes());
        let ws = Workspace::open(root, None).expect("open");
        ws.ensure_data_dir().expect("data dir");
        let config = Config::default();
        let output = scan_workspace(&ws, &config).expect("scan");
        let files: Vec<&str> = output.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(
            files,
            vec![
                ".codesearchignore",
                ".env.example",
                "blob.txt",
                "docs/guide.md",
                "src/lib.rs"
            ]
        );
        let skipped: Vec<(&str, SkipReason)> = output
            .skipped
            .iter()
            .map(|s| (s.path.as_str(), s.reason))
            .collect();
        assert!(skipped.contains(&(".env", SkipReason::Sensitive)));
        assert!(skipped.contains(&("id_rsa", SkipReason::Sensitive)));
        assert!(skipped.contains(&("logo.png", SkipReason::Binary)));
        assert!(!skipped.iter().any(|(p, _)| p.starts_with("node_modules")));
        // blob.txt 在读取阶段才会因 NUL 被判定为二进制。
        let snapshot = read_snapshot(&root.join("blob.txt"), 1024).expect("read");
        assert_eq!(snapshot.err().map(|(r, _)| r), Some(SkipReason::Binary));
    }

    #[test]
    fn include_globs_limit_scan() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "src/a.rs", b"fn a() {}\n");
        write(root, "web/b.ts", b"export const b = 1;\n");
        let ws = Workspace::open(root, None).expect("open");
        let mut config = Config::default();
        config.index.include = vec!["src/**".to_string()];
        let output = scan_workspace(&ws, &config).expect("scan");
        let files: Vec<&str> = output.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(files, vec!["src/a.rs"]);

        let mut config = Config::default();
        config.index.exclude = vec!["web/**".to_string()];
        let output = scan_workspace(&ws, &config).expect("scan");
        let files: Vec<&str> = output.files.iter().map(|f| f.rel_path.as_str()).collect();
        assert_eq!(files, vec!["src/a.rs"]);
    }

    #[test]
    fn read_snapshot_reports_utf8_and_size_problems() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        write(root, "latin1.txt", b"caf\xe9");
        write(root, "big.txt", &[b'a'; 100]);
        write(root, "ok.rs", "fn 中文() {}".as_bytes());
        let bad = read_snapshot(&root.join("latin1.txt"), 1024).expect("read");
        assert_eq!(bad.err().map(|(r, _)| r), Some(SkipReason::NotUtf8));
        let big = read_snapshot(&root.join("big.txt"), 10).expect("read");
        assert_eq!(big.err().map(|(r, _)| r), Some(SkipReason::TooLarge));
        let ok = read_snapshot(&root.join("ok.rs"), 1024)
            .expect("read")
            .expect("ok");
        assert_eq!(ok.text, "fn 中文() {}");
        assert_eq!(ok.hash.len(), 64);
    }

    #[test]
    fn language_detection_by_extension() {
        assert_eq!(detect_language("src/lib.rs"), Language::Rust);
        assert_eq!(detect_language("a/b.test.ts"), Language::TypeScript);
        assert_eq!(detect_language("App.tsx"), Language::Tsx);
        assert_eq!(detect_language("index.mjs"), Language::JavaScript);
        assert_eq!(detect_language("README.md"), Language::Markdown);
        assert_eq!(detect_language("Makefile"), Language::Text);
    }
}

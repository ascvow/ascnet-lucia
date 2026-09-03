//! 结构化索引：chunk、符号与文件记录的驻留存储。
//!
//! # 为什么索引本体不落盘
//!
//! 按 [ADR：索引存储后端](../../../docs/code-context-engine/adr-storage.md) 的分层决策，
//! 宿主的持久存储是 JSON KV，8 MiB / 8192 键上限，且每次写入重写全量文件。它能承载
//! `IndexManifest` 与**路径 → 内容哈希表**（2000 文件约 160 KB），装不下 chunk、符号表或
//! 倒排索引。
//!
//! 因此结构化索引本体**内存驻留，进程启动后按需重建**。落盘的哈希表不是为了跳过重建
//! ——重建仍需读全文——而是为 Phase 11 的增量索引提供"上一轮各文件是什么内容"的基线。
//!
//! # chunk 不持有正文
//!
//! [`CodeChunk`] 刻意只记录 `(file_path, start_line, end_line, content_hash)`。返回检索结果时
//! 按需重读文件，这既让 64 MiB 线性内存下的索引规模可控，又让外部编辑能在读取时被内容
//! 哈希发现。

mod build;

#[cfg(test)]
mod tests;

pub use build::{IndexBuild, BUILD_BUDGET};

use crate::model::{CodeChunk, RepositoryIdentity, Symbol};
use crate::parser::{ParseTier, ParsedFile};
use crate::repository::language::Language;
use crate::repository::IndexableFile;
use std::collections::BTreeMap;

/// 索引最多容纳的文件数。
const MAX_FILES: usize = 50_000;

/// 索引最多容纳的 chunk 数。
///
/// 上限存在的理由是内存而不是性能：插件只有 64 MiB 线性内存，触顶会直接 trap。
/// 触及上限时索引标记为截断并如实上报，而不是静默丢弃后半个仓库。
const MAX_CHUNKS: usize = 400_000;

/// 一个已索引文件的记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    /// 仓库相对路径。
    pub path: String,
    /// 识别出的语言。
    pub language: Language,
    /// 文件内容哈希。
    pub content_hash: String,
    /// 该文件使用的解析层级。
    pub tier: ParseTier,
    /// 该文件产出的 chunk ID，按出现顺序排列。
    pub chunk_ids: Vec<String>,
    /// 文件字节数。
    pub byte_len: u32,
    /// 文件行数。
    pub line_count: u32,
}

/// 索引规模统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStats {
    /// 已索引文件数。
    pub files: u32,
    /// chunk 总数。
    pub chunks: u32,
    /// 符号总数。
    pub symbols: u32,
    /// 走结构化解析的文件数。
    pub structural_files: u32,
    /// 走文本兜底的文件数。
    pub text_files: u32,
    /// 是否因触及上限而截断。
    pub truncated: bool,
}

/// 结构化索引。
///
/// 单个实例只服务一个仓库：[`StructuralIndex::upsert_file`] 会拒绝来自其他仓库的 chunk。
/// 多仓库隔离在更上层由宿主的 `Workspace` 存储作用域与进程模型保证
/// （见 `architecture.md` 第 11 问），这里的检查是防止编码错误，不是隔离机制本身。
#[derive(Debug, Clone)]
pub struct StructuralIndex {
    /// 该索引所属的仓库。
    repository: RepositoryIdentity,
    /// 路径 → 文件记录。
    files: BTreeMap<String, FileRecord>,
    /// chunk ID → chunk。
    chunks: BTreeMap<String, CodeChunk>,
    /// 路径 → 该文件的符号。
    symbols: BTreeMap<String, Vec<Symbol>>,
    /// 是否因触及上限而截断。
    truncated: bool,
}

impl StructuralIndex {
    /// 为指定仓库创建空索引。
    pub fn new(repository: RepositoryIdentity) -> Self {
        Self {
            repository,
            files: BTreeMap::new(),
            chunks: BTreeMap::new(),
            symbols: BTreeMap::new(),
            truncated: false,
        }
    }

    /// 返回该索引所属的仓库身份。
    pub fn repository(&self) -> &RepositoryIdentity {
        &self.repository
    }

    /// 索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// 返回索引规模统计。
    pub fn stats(&self) -> IndexStats {
        let (structural_files, text_files) =
            self.files
                .values()
                .fold((0u32, 0u32), |(structural, text), record| {
                    match record.tier {
                        ParseTier::Structural => (structural + 1, text),
                        ParseTier::Text => (structural, text + 1),
                    }
                });
        IndexStats {
            files: u32::try_from(self.files.len()).unwrap_or(u32::MAX),
            chunks: u32::try_from(self.chunks.len()).unwrap_or(u32::MAX),
            symbols: u32::try_from(self.symbols.values().map(Vec::len).sum::<usize>())
                .unwrap_or(u32::MAX),
            structural_files,
            text_files,
            truncated: self.truncated,
        }
    }

    /// 写入或更新一个文件的索引。
    ///
    /// 同路径的旧 chunk 与旧符号先被完整移除，因此重复索引同一文件是幂等的——这正是
    /// Phase 11 增量重建所依赖的性质。
    ///
    /// 来自其他仓库的 chunk 会被拒绝并返回 `false`。
    pub fn upsert_file(&mut self, file: &IndexableFile, parsed: ParsedFile) -> bool {
        if parsed
            .chunks
            .iter()
            .any(|chunk| chunk.repository_id != self.repository)
        {
            return false;
        }
        if self.files.len() >= MAX_FILES && !self.files.contains_key(&file.path) {
            self.truncated = true;
            return false;
        }
        if self.chunks.len() + parsed.chunks.len() > MAX_CHUNKS {
            self.truncated = true;
            return false;
        }

        self.remove_file(&file.path);

        let chunk_ids: Vec<String> = parsed.chunks.iter().map(|chunk| chunk.id.clone()).collect();
        for chunk in parsed.chunks {
            self.chunks.insert(chunk.id.clone(), chunk);
        }
        if !parsed.symbols.is_empty() {
            self.symbols.insert(file.path.clone(), parsed.symbols);
        }
        self.files.insert(
            file.path.clone(),
            FileRecord {
                path: file.path.clone(),
                language: file.language,
                content_hash: file.content_hash.clone(),
                tier: parsed.tier,
                chunk_ids,
                byte_len: file.byte_len,
                line_count: file.line_count,
            },
        );
        true
    }

    /// 移除一个文件及其全部 chunk 与符号。
    ///
    /// 返回该文件此前是否在索引中。
    pub fn remove_file(&mut self, path: &str) -> bool {
        let Some(record) = self.files.remove(path) else {
            return false;
        };
        for id in &record.chunk_ids {
            self.chunks.remove(id);
        }
        self.symbols.remove(path);
        true
    }

    /// 按 ID 取一个 chunk。
    pub fn chunk(&self, id: &str) -> Option<&CodeChunk> {
        self.chunks.get(id)
    }

    /// 按路径取文件记录。
    pub fn file(&self, path: &str) -> Option<&FileRecord> {
        self.files.get(path)
    }

    /// 遍历全部文件记录，按路径升序。
    pub fn files(&self) -> impl Iterator<Item = &FileRecord> {
        self.files.values()
    }

    /// 遍历全部 chunk，按 ID 升序。
    pub fn chunks(&self) -> impl Iterator<Item = &CodeChunk> {
        self.chunks.values()
    }

    /// 取一个文件产出的全部 chunk，按文件内出现顺序。
    pub fn chunks_of(&self, path: &str) -> Vec<&CodeChunk> {
        self.files
            .get(path)
            .map(|record| {
                record
                    .chunk_ids
                    .iter()
                    .filter_map(|id| self.chunks.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 遍历全部符号。
    pub fn symbols(&self) -> impl Iterator<Item = &Symbol> {
        self.symbols.values().flatten()
    }

    /// 清空索引，保留仓库身份。
    pub fn clear(&mut self) {
        self.files.clear();
        self.chunks.clear();
        self.symbols.clear();
        self.truncated = false;
    }

    /// 导出路径 → 内容哈希表。
    ///
    /// 这是[存储 ADR](../../../docs/code-context-engine/adr-storage.md) 认定可以落宿主 JSON KV
    /// 的两类数据之一，供下次启动做增量判断。
    pub fn file_hashes(&self) -> Vec<(String, String)> {
        self.files
            .values()
            .map(|record| (record.path.clone(), record.content_hash.clone()))
            .collect()
    }

    /// 对照上一轮的哈希表，判断一个文件是否发生变化。
    ///
    /// `previous` 是 [`StructuralIndex::file_hashes`] 的结果转成的映射。
    pub fn has_changed(previous: &BTreeMap<String, String>, file: &IndexableFile) -> bool {
        previous
            .get(&file.path)
            .is_none_or(|hash| *hash != file.content_hash)
    }
}

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

pub use build::{IncrementalStats, IndexBuild, BUILD_BUDGET};

use crate::model::{CodeChunk, RepositoryIdentity, Symbol};
use crate::parser::{ParseTier, ParsedFile};
use crate::repository::language::Language;
use crate::repository::IndexableFile;
use std::collections::{BTreeMap, HashSet};

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

/// 一次文件索引更新的类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    /// 该路径此前不在结构索引中。
    Added,
    /// 该路径已存在，但正文或解析产物发生变化。
    Modified,
    /// 新旧文件记录与全部 chunk 都相同，无需更新检索索引。
    Unchanged,
    /// 该路径已从仓库中消失。
    Deleted,
}

/// 结构索引对一个文件应用更新后的差异摘要。
///
/// `removed_chunk_ids` 与 `added_chunk_ids`/`updated_chunk_ids` 是检索索引同步的唯一输入：
/// 调用方应先移除旧 ID，再为新增或更新的 ID 重新建立词法与符号 posting。未变化的 chunk
/// 保留原对象，避免因文件内局部编辑而重建整份检索数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexChange {
    /// 发生变化的仓库相对路径。
    pub path: String,
    /// 文件级变化类型。
    pub kind: FileChangeKind,
    /// 新文件中新增的 chunk ID。
    pub added_chunk_ids: Vec<String>,
    /// ID 保持不变但内容或定位字段变化的 chunk ID。
    pub updated_chunk_ids: Vec<String>,
    /// 旧文件中已不存在的 chunk ID。
    pub removed_chunk_ids: Vec<String>,
    /// 新旧版本完全相同、可直接复用的 chunk ID。
    pub unchanged_chunk_ids: Vec<String>,
}

impl IndexChange {
    /// 返回需要在检索索引中重新写入的 chunk ID 数量。
    pub fn changed_chunk_count(&self) -> usize {
        self.added_chunk_ids.len() + self.updated_chunk_ids.len()
    }
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
    /// 同路径的旧 chunk 与旧符号按稳定 ID 计算差异并替换，因此重复索引同一文件是幂等的，
    /// 且不会无谓地重建未变化的检索条目——这正是 Phase 11 增量重建所依赖的性质。
    ///
    /// 来自其他仓库或其他文件的 chunk 会被拒绝并返回 `false`。
    pub fn upsert_file(&mut self, file: &IndexableFile, parsed: ParsedFile) -> bool {
        self.upsert_file_incremental(file, parsed).is_some()
    }

    /// 写入或更新一个文件，并返回新旧 chunk 的精确差异。
    ///
    /// 与旧版 `upsert_file` 相比，该方法只替换新增或实际变化的 chunk；同 ID 且字段完全
    /// 相同的 chunk 保留在原位。容量检查会先扣除旧文件占用，避免更新同一文件时因旧 chunk
    /// 尚未移除而误报容量不足。
    ///
    /// # Errors
    ///
    /// 返回 `None` 表示输入路径/仓库不匹配、出现重复或跨文件 ID、或更新后会超过文件/chunk
    /// 上限；除容量截断标记外，失败时索引内容保持不变。
    pub fn upsert_file_incremental(
        &mut self,
        file: &IndexableFile,
        parsed: ParsedFile,
    ) -> Option<IndexChange> {
        let ParsedFile {
            chunks: parsed_chunks,
            symbols: parsed_symbols,
            tier,
        } = parsed;
        let old_record = self.files.get(&file.path).cloned();
        let old_chunk_ids = old_record
            .as_ref()
            .map(|record| record.chunk_ids.clone())
            .unwrap_or_default();
        let old_chunk_id_set: HashSet<&str> = old_chunk_ids.iter().map(String::as_str).collect();

        // 先完成所有输入校验，再触碰索引状态。这样异常解析结果不会删除旧文件，也不会
        // 把一个文件的 chunk ID 覆盖到另一个文件上。
        if parsed_chunks.iter().any(|chunk| {
            chunk.repository_id != self.repository
                || chunk.file_path != file.path
                || self.chunks.get(&chunk.id).is_some_and(|existing| {
                    existing.file_path != file.path || !old_chunk_id_set.contains(chunk.id.as_str())
                })
        }) {
            return None;
        }
        if parsed_symbols
            .iter()
            .any(|symbol| symbol.file_path != file.path)
        {
            return None;
        }

        let new_chunk_ids: Vec<String> =
            parsed_chunks.iter().map(|chunk| chunk.id.clone()).collect();
        let mut new_chunks = BTreeMap::new();
        for chunk in parsed_chunks {
            if new_chunks.insert(chunk.id.clone(), chunk).is_some() {
                return None;
            }
        }
        let old_chunk_count = old_chunk_ids.len();
        if self.files.len() >= MAX_FILES && !self.files.contains_key(&file.path) {
            self.truncated = true;
            return None;
        }
        let projected_chunks = self
            .chunks
            .len()
            .saturating_sub(old_chunk_count)
            .saturating_add(new_chunks.len());
        if projected_chunks > MAX_CHUNKS {
            self.truncated = true;
            return None;
        }

        let mut removed_chunk_ids = Vec::new();
        let mut updated_chunk_ids = Vec::new();
        let mut unchanged_chunk_ids = Vec::new();
        let mut added_chunk_ids = Vec::new();
        for old_id in &old_chunk_ids {
            if !new_chunks.contains_key(old_id) {
                removed_chunk_ids.push(old_id.clone());
            }
        }
        for (id, new_chunk) in &new_chunks {
            match self.chunks.get(id) {
                Some(old_chunk) if old_chunk == new_chunk => unchanged_chunk_ids.push(id.clone()),
                Some(_) => updated_chunk_ids.push(id.clone()),
                None => added_chunk_ids.push(id.clone()),
            }
        }
        for id in &removed_chunk_ids {
            self.chunks.remove(id);
        }
        for id in &updated_chunk_ids {
            self.chunks.remove(id);
        }
        for (id, chunk) in new_chunks {
            if added_chunk_ids.iter().any(|added| added == &id)
                || updated_chunk_ids.iter().any(|updated| updated == &id)
            {
                self.chunks.insert(id, chunk);
            }
        }

        let symbols_unchanged = self
            .symbols
            .get(&file.path)
            .map(|old| old == &parsed_symbols)
            .unwrap_or_else(|| parsed_symbols.is_empty());
        self.symbols.remove(&file.path);
        if !parsed_symbols.is_empty() {
            self.symbols.insert(file.path.clone(), parsed_symbols);
        }
        // `FileRecord::chunk_ids` 必须遵循解析器给出的文件内顺序，不能按 BTreeMap 的
        // 字典序重排，否则上下文构建的父子关系与输出顺序会改变。
        let ordered_chunk_ids = new_chunk_ids;
        let metadata_unchanged = old_record.as_ref().is_some_and(|record| {
            record.language == file.language
                && record.tier == tier
                && record.byte_len == file.byte_len
                && record.line_count == file.line_count
                && record.content_hash == file.content_hash
        });
        let kind = if old_record.is_none() {
            FileChangeKind::Added
        } else if metadata_unchanged
            && removed_chunk_ids.is_empty()
            && updated_chunk_ids.is_empty()
            && added_chunk_ids.is_empty()
            && symbols_unchanged
        {
            FileChangeKind::Unchanged
        } else {
            FileChangeKind::Modified
        };
        self.files.insert(
            file.path.clone(),
            FileRecord {
                path: file.path.clone(),
                language: file.language,
                content_hash: file.content_hash.clone(),
                tier,
                chunk_ids: ordered_chunk_ids,
                byte_len: file.byte_len,
                line_count: file.line_count,
            },
        );
        Some(IndexChange {
            path: file.path.clone(),
            kind,
            added_chunk_ids,
            updated_chunk_ids,
            removed_chunk_ids,
            unchanged_chunk_ids,
        })
    }

    /// 移除一个文件及其全部 chunk 与符号。
    ///
    /// 返回该文件此前是否在索引中。
    pub fn remove_file(&mut self, path: &str) -> bool {
        self.remove_file_with_diff(path).is_some()
    }

    /// 移除一个文件并返回被删除的 chunk ID。
    pub fn remove_file_with_diff(&mut self, path: &str) -> Option<IndexChange> {
        let record = self.files.remove(path)?;
        for id in &record.chunk_ids {
            self.chunks.remove(id);
        }
        self.symbols.remove(path);
        Some(IndexChange {
            path: path.to_string(),
            kind: FileChangeKind::Deleted,
            added_chunk_ids: Vec::new(),
            updated_chunk_ids: Vec::new(),
            removed_chunk_ids: record.chunk_ids,
            unchanged_chunk_ids: Vec::new(),
        })
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

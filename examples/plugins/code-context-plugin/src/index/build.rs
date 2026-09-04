//! 索引构建流水线：扫描 → 解析 → 入库。
//!
//! # 为什么必须分片
//!
//! 宿主给每个插件 5000 万 fuel，且**每次导出调用前重置**。一次全量索引中等仓库远超该预算，
//! 跑满即 trap。因此构建复用 Phase 2 已冻结的游标形态：单次 [`IndexBuild::advance`] 消耗到
//! 预算上限就保存断点返回，下次调用继续。**不发明第二套分片机制。**
//!
//! 构建由首次 `code_search` 惰性触发，而不是在 `activate` 里做——`activate` 与其他导出共用
//! 同一份 fuel 预算，在那里做全量索引必然 trap（见
//! [ADR：索引存储后端](../../../docs/code-context-engine/adr-storage.md)）。

use super::StructuralIndex;
use crate::model::RepositoryIdentity;
use crate::parser::parse_file;
use crate::repository::{
    FileSink, IndexableFile, RepositoryScan, ScanBudget, ScanLimits, ScanProgress, ScanStats,
    ScanWarning,
};
use crate::retrieval::RetrievalIndex;
use agent_plugin::PluginHostApi;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// 单次构建推进的默认预算。
///
/// 比纯扫描的预算保守得多：扫描每个文件只算一次哈希，而构建还要跑一遍语法解析，
/// 单位字节的指令数高出一个量级。这里的取值是保守估计，**真实 fuel 消耗需要在
/// Phase 14 的 benchmark 中实测后再调**——宁可多跑几轮，也不要 trap。
pub const BUILD_BUDGET: ScanBudget = ScanBudget {
    max_host_calls: 32,
    max_bytes_read: 512 * 1024,
};

/// 一次增量重建的统计快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IncrementalStats {
    /// 已从目录快照发现的文件数。
    pub discovered_files: u32,
    /// 实际读取并尝试更新的文件数。
    pub processed_files: u32,
    /// 读取后确认哈希与索引一致的文件数。
    pub unchanged_files: u32,
    /// 新增文件数。
    pub added_files: u32,
    /// 修改文件数。
    pub modified_files: u32,
    /// 从索引移除的文件数。
    pub deleted_files: u32,
    /// 读取或容量错误导致本轮未能更新的文件数。
    pub failed_files: u32,
    /// 新增或更新的 chunk 数。
    pub changed_chunks: u32,
    /// 移除的 chunk 数。
    pub removed_chunks: u32,
}

#[derive(Debug)]
enum BuildMode {
    /// 全量构建；`requested_dirty` 用于清理构建开始前积累的脏路径。
    Full {
        requested_dirty: BTreeSet<String>,
        failed_paths: BTreeSet<String>,
    },
    /// 脏集合驱动的增量构建。
    Incremental {
        requested_dirty: BTreeSet<String>,
        failed_paths: BTreeSet<String>,
        stats: IncrementalStats,
        deletions_applied: bool,
    },
}

/// 一次可分片推进的索引构建。
#[derive(Debug)]
pub struct IndexBuild {
    /// 底层仓库扫描。
    scan: RepositoryScan,
    /// 构建中的结构索引。
    index: StructuralIndex,
    /// 构建中的检索索引。
    retrieval: RetrievalIndex,
    /// 构建模式与增量统计。
    mode: BuildMode,
}

impl IndexBuild {
    /// 为指定仓库创建一次全量构建。
    pub fn new(repository: RepositoryIdentity, limits: ScanLimits) -> Self {
        Self {
            scan: RepositoryScan::new(limits),
            index: StructuralIndex::new(repository),
            retrieval: RetrievalIndex::new(),
            mode: BuildMode::Full {
                requested_dirty: BTreeSet::new(),
                failed_paths: BTreeSet::new(),
            },
        }
    }

    /// 为全量构建附带一组构建开始前已知的脏路径。
    ///
    /// 全量扫描本身会覆盖这些路径；只有读取失败或扫描被截断时才会把它们返回为待重试，
    /// 避免首次构建结束后无意义地重复一轮增量扫描。
    pub fn new_full_with_dirty(
        repository: RepositoryIdentity,
        limits: ScanLimits,
        requested_dirty: BTreeSet<String>,
    ) -> Self {
        let mut build = Self::new(repository, limits);
        if let BuildMode::Full {
            requested_dirty: target,
            ..
        } = &mut build.mode
        {
            *target = requested_dirty;
        }
        build
    }

    /// 接管既有结构与检索索引，创建一次文件级增量构建。
    ///
    /// 增量扫描仍会遍历目录以发现新增和删除，但只读取脏文件及索引中尚不存在的文件；
    /// 未变化文件不会重新解析。外部未发出事件的编辑由 Phase 10 的惰性一致性校验兜底。
    pub fn new_incremental(
        index: StructuralIndex,
        retrieval: RetrievalIndex,
        dirty_paths: BTreeSet<String>,
        limits: ScanLimits,
    ) -> Self {
        Self {
            scan: RepositoryScan::new(limits),
            index,
            retrieval,
            mode: BuildMode::Incremental {
                requested_dirty: dirty_paths,
                failed_paths: BTreeSet::new(),
                stats: IncrementalStats::default(),
                deletions_applied: false,
            },
        }
    }

    /// `new_incremental` 的语义别名，便于调用方按动词理解生命周期。
    pub fn incremental(
        index: StructuralIndex,
        retrieval: RetrievalIndex,
        dirty_paths: BTreeSet<String>,
        limits: ScanLimits,
    ) -> Self {
        Self::new_incremental(index, retrieval, dirty_paths, limits)
    }

    /// 使用默认扫描上限创建构建。
    pub fn with_default_limits(repository: RepositoryIdentity) -> Self {
        Self::new(repository, ScanLimits::default())
    }

    /// 在给定预算内推进构建。
    ///
    /// 文件正文在扫描读取的同一时刻被解析并入库，随后即被丢弃，因此构建过程中的内存
    /// 占用与仓库总体积无关，只与索引本身的规模有关。
    ///
    /// # Errors
    ///
    /// 不返回错误。单个文件的读取或解析失败被隔离为扫描告警或文本兜底，不中断构建。
    pub fn advance(&mut self, host: &dyn PluginHostApi, budget: ScanBudget) -> ScanProgress {
        if self.is_incremental() {
            self.advance_incremental(host, budget)
        } else {
            self.advance_full(host, budget)
        }
    }

    /// 在给定预算内推进全量构建。
    fn advance_full(&mut self, host: &dyn PluginHostApi, budget: ScanBudget) -> ScanProgress {
        self.advance_inner(host, budget, &|_, _| true)
    }

    /// 在给定预算内推进增量构建。
    ///
    /// 该入口与 [`Self::advance`] 使用同一游标和预算形态；增量模式只读取脏文件与新增文件。
    pub fn advance_incremental(
        &mut self,
        host: &dyn PluginHostApi,
        budget: ScanBudget,
    ) -> ScanProgress {
        if !self.is_incremental() {
            return self.advance_full(host, budget);
        }
        let existing = self
            .index
            .files()
            .map(|file| file.path.clone())
            .collect::<BTreeSet<_>>();
        let dirty = match &self.mode {
            BuildMode::Incremental {
                requested_dirty, ..
            } => requested_dirty.clone(),
            BuildMode::Full { .. } => BTreeSet::new(),
        };
        self.advance_inner(host, budget, &move |path, _| {
            dirty.contains(path) || !existing.contains(path)
        })
    }

    /// 执行一次内部推进；读取策略由调用方提供。
    fn advance_inner(
        &mut self,
        host: &dyn PluginHostApi,
        budget: ScanBudget,
        read_filter: &dyn Fn(&str, crate::repository::language::Language) -> bool,
    ) -> ScanProgress {
        // 拆分借用：sink 需要 &mut index、&mut retrieval 与 &mut mode，而 advance_with_filter
        // 需要 &mut scan。
        let Self {
            scan,
            index,
            retrieval,
            mode,
        } = self;
        let repository = index.repository().clone();
        let mut sink = IndexSink {
            index,
            retrieval,
            repository,
            mode,
        };
        let was_complete = scan.is_complete();
        let progress = scan.advance_with_filter(host, budget, &mut sink, read_filter);
        drop(sink);
        if progress == ScanProgress::Complete && !was_complete {
            if let BuildMode::Incremental {
                stats,
                deletions_applied,
                ..
            } = mode
            {
                if !*deletions_applied {
                    apply_deletions(scan, index, retrieval, stats);
                    stats.discovered_files =
                        u32::try_from(scan.discovered_paths().len()).unwrap_or(u32::MAX);
                    *deletions_applied = true;
                }
            }
        }
        if progress == ScanProgress::Complete {
            // 符号表必须在检索前完成排序；放在这里而不是交给调用方，
            // 是为了让"构建完成"与"可检索"成为同一件事。
            retrieval.finalize();
        }
        progress
    }

    /// 构建是否已完成。
    pub fn is_complete(&self) -> bool {
        self.scan.is_complete()
    }

    /// 返回构建中的结构索引。
    pub fn index(&self) -> &StructuralIndex {
        &self.index
    }

    /// 返回构建中的检索索引。
    pub fn retrieval(&self) -> &RetrievalIndex {
        &self.retrieval
    }

    /// 取出构建完成的结构索引与检索索引。
    pub fn into_parts(self) -> (StructuralIndex, RetrievalIndex) {
        (self.index, self.retrieval)
    }

    /// 判断这次构建是否为增量模式。
    pub fn is_incremental(&self) -> bool {
        matches!(self.mode, BuildMode::Incremental { .. })
    }

    /// 返回当前增量统计；全量构建返回 `None`。
    pub fn incremental_stats(&self) -> Option<IncrementalStats> {
        match &self.mode {
            BuildMode::Incremental {
                stats,
                failed_paths,
                ..
            } => {
                let mut snapshot = *stats;
                snapshot.discovered_files =
                    u32::try_from(self.scan.discovered_paths().len()).unwrap_or(u32::MAX);
                // 读取失败不会进入 `FileSink`，因此需要把扫描器记录的路径并入失败统计；
                // 与容量拒绝路径合并后再计数，避免同一路径重复计算。
                let read_failed = self.scan.read_failed_paths();
                snapshot.processed_files = snapshot
                    .processed_files
                    .saturating_add(u32::try_from(read_failed.len()).unwrap_or(u32::MAX));
                let failed_count = failed_paths
                    .iter()
                    .filter(|path| !read_failed.contains(*path))
                    .count()
                    .saturating_add(read_failed.len());
                snapshot.failed_files = u32::try_from(failed_count).unwrap_or(u32::MAX);
                Some(snapshot)
            }
            BuildMode::Full { .. } => None,
        }
    }

    /// 返回本轮构建开始时接管的脏路径集合。
    ///
    /// 调用方在构建开始后可以继续接收新的事件；这些新路径不属于本轮集合，不能在本轮
    /// 完成时被清除。返回值只读且按字典序稳定排列。
    pub fn requested_dirty_paths(&self) -> &BTreeSet<String> {
        match &self.mode {
            BuildMode::Full {
                requested_dirty, ..
            }
            | BuildMode::Incremental {
                requested_dirty, ..
            } => requested_dirty,
        }
    }

    /// 返回本轮完成后仍需重试的脏路径。
    ///
    /// 扫描未完成或目录快照不完整时保守地保留全部请求路径；单文件读取失败与索引容量
    /// 拒绝也会保留对应路径，其余已处理路径会被视为完成。
    pub fn remaining_dirty_paths(&self) -> BTreeSet<String> {
        let requested = match &self.mode {
            BuildMode::Full {
                requested_dirty, ..
            }
            | BuildMode::Incremental {
                requested_dirty, ..
            } => requested_dirty,
        };
        if !self.scan.is_complete() || !self.scan.snapshot_is_complete() {
            return requested.clone();
        }
        let failed = match &self.mode {
            BuildMode::Full { failed_paths, .. } | BuildMode::Incremental { failed_paths, .. } => {
                failed_paths
            }
        };
        // 失败路径不一定来自本轮请求集合：增量扫描还会主动读取索引中尚不存在的新文件。
        // 无论是读取失败还是容量/校验拒绝，都必须保留到下一轮，否则没有新的写入事件时
        // 就不会再触发重试。
        let mut remaining = failed.clone();
        remaining.extend(self.scan.read_failed_paths().iter().cloned());
        remaining
    }

    /// 返回扫描统计。
    pub fn scan_stats(&self) -> ScanStats {
        self.scan.stats()
    }

    /// 返回扫描过程中的告警。
    pub fn warnings(&self) -> &[ScanWarning] {
        self.scan.warnings()
    }
}

/// 把扫描读到的正文解析后写入结构索引与检索索引。
struct IndexSink<'a> {
    /// 目标结构索引。
    index: &'a mut StructuralIndex,
    /// 目标检索索引。
    retrieval: &'a mut RetrievalIndex,
    /// 仓库身份，供 chunk 构造使用。
    repository: RepositoryIdentity,
    /// 构建模式与统计。
    mode: &'a mut BuildMode,
}

impl FileSink for IndexSink<'_> {
    fn accept(&mut self, file: &IndexableFile, content: &str) {
        let is_incremental = matches!(self.mode, BuildMode::Incremental { .. });
        if is_incremental {
            let unchanged = self.index.file(&file.path).is_some_and(|record| {
                record.content_hash == file.content_hash
                    && record.language == file.language
                    && record.byte_len == file.byte_len
                    && record.line_count == file.line_count
            });
            if unchanged {
                if let BuildMode::Incremental { stats, .. } = self.mode {
                    stats.processed_files = stats.processed_files.saturating_add(1);
                    stats.unchanged_files = stats.unchanged_files.saturating_add(1);
                }
                return;
            }
        }
        // parse_file 内部已经把解析失败降级为文本分块，因此这里不需要再处理错误：
        // 任何输入都会得到可检索的产物（`architecture.md` 第 12 问）。
        let parsed = parse_file(&self.repository, &file.path, file.language, content);
        let chunks = parsed.chunks.clone();
        let Some(change) = self.index.upsert_file_incremental(file, parsed) else {
            mark_failed(self.mode, &file.path);
            return;
        };

        // 建倒排索引需要 chunk 正文，但正文不会被保留：切词后即丢弃。这样
        // `execute_command` 这类只出现在函数体里的标识符也能被检索到，同时不违反
        // 存储 ADR 的"不驻留 chunk 全文"。
        let lines: Vec<&str> = content.lines().collect();
        let mut removed = change.removed_chunk_ids.clone();
        removed.extend(change.updated_chunk_ids.iter().cloned());
        self.retrieval.remove_chunks(&removed);
        for chunk in &chunks {
            if change.added_chunk_ids.iter().any(|id| id == &chunk.id)
                || change.updated_chunk_ids.iter().any(|id| id == &chunk.id)
            {
                let body = slice_lines(&lines, chunk.start_line, chunk.end_line);
                self.retrieval.index_chunk(chunk, &body);
            }
        }
        if let BuildMode::Incremental { stats, .. } = self.mode {
            stats.processed_files = stats.processed_files.saturating_add(1);
            match change.kind {
                super::FileChangeKind::Added => {
                    stats.added_files = stats.added_files.saturating_add(1)
                }
                super::FileChangeKind::Modified => {
                    stats.modified_files = stats.modified_files.saturating_add(1)
                }
                super::FileChangeKind::Unchanged | super::FileChangeKind::Deleted => {}
            }
            stats.changed_chunks = stats
                .changed_chunks
                .saturating_add(u32::try_from(change.changed_chunk_count()).unwrap_or(u32::MAX));
            stats.removed_chunks = stats
                .removed_chunks
                .saturating_add(u32::try_from(change.removed_chunk_ids.len()).unwrap_or(u32::MAX));
        }
    }
}

/// 记录一次无法应用的文件更新，以便下一轮继续重试。
fn mark_failed(mode: &mut BuildMode, path: &str) {
    match mode {
        BuildMode::Full { failed_paths, .. } | BuildMode::Incremental { failed_paths, .. } => {
            let newly_failed = failed_paths.insert(path.to_string());
            if newly_failed {
                if let BuildMode::Incremental { stats, .. } = mode {
                    stats.failed_files = stats.failed_files.saturating_add(1);
                }
            }
        }
    }
}

/// 在完整目录快照上移除已经消失或明确不可索引的旧文件。
fn apply_deletions(
    scan: &RepositoryScan,
    index: &mut StructuralIndex,
    retrieval: &mut RetrievalIndex,
    stats: &mut IncrementalStats,
) {
    if !scan.snapshot_is_complete() {
        return;
    }
    let old_paths: Vec<String> = index.files().map(|file| file.path.clone()).collect();
    for path in old_paths {
        if scan.read_failed_paths().contains(&path) {
            continue;
        }
        let deleted =
            !scan.discovered_paths().contains(&path) || scan.rejected_paths().contains(&path);
        if !deleted {
            continue;
        }
        let Some(change) = index.remove_file_with_diff(&path) else {
            continue;
        };
        retrieval.remove_chunks(&change.removed_chunk_ids);
        stats.deleted_files = stats.deleted_files.saturating_add(1);
        stats.removed_chunks = stats
            .removed_chunks
            .saturating_add(u32::try_from(change.removed_chunk_ids.len()).unwrap_or(u32::MAX));
    }
}

/// 取闭区间行范围的正文。
fn slice_lines(lines: &[&str], start_line: u32, end_line: u32) -> String {
    let start = start_line.saturating_sub(1) as usize;
    let end = (end_line as usize).min(lines.len());
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

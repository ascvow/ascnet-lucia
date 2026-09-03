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

/// 单次构建推进的默认预算。
///
/// 比纯扫描的预算保守得多：扫描每个文件只算一次哈希，而构建还要跑一遍语法解析，
/// 单位字节的指令数高出一个量级。这里的取值是保守估计，**真实 fuel 消耗需要在
/// Phase 14 的 benchmark 中实测后再调**——宁可多跑几轮，也不要 trap。
pub const BUILD_BUDGET: ScanBudget = ScanBudget {
    max_host_calls: 32,
    max_bytes_read: 512 * 1024,
};

/// 一次可分片推进的索引构建。
#[derive(Debug)]
pub struct IndexBuild {
    /// 底层仓库扫描。
    scan: RepositoryScan,
    /// 构建中的结构索引。
    index: StructuralIndex,
    /// 构建中的检索索引。
    retrieval: RetrievalIndex,
}

impl IndexBuild {
    /// 为指定仓库创建一次全量构建。
    pub fn new(repository: RepositoryIdentity, limits: ScanLimits) -> Self {
        Self {
            scan: RepositoryScan::new(limits),
            index: StructuralIndex::new(repository),
            retrieval: RetrievalIndex::new(),
        }
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
        // 拆分借用：sink 需要 &mut index 与 &mut retrieval，而 advance_with 需要 &mut scan。
        let Self {
            scan,
            index,
            retrieval,
        } = self;
        let repository = index.repository().clone();
        let mut sink = IndexSink {
            index,
            retrieval,
            repository,
        };
        let progress = scan.advance_with(host, budget, &mut sink);
        if progress == ScanProgress::Complete {
            // 符号表必须在检索前完成排序；放在这里而不是交给调用方，
            // 是为了让"构建完成"与"可检索"成为同一件事。
            self.retrieval.finalize();
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
}

impl FileSink for IndexSink<'_> {
    fn accept(&mut self, file: &IndexableFile, content: &str) {
        // parse_file 内部已经把解析失败降级为文本分块，因此这里不需要再处理错误：
        // 任何输入都会得到可检索的产物（`architecture.md` 第 12 问）。
        let parsed = parse_file(&self.repository, &file.path, file.language, content);

        // 建倒排索引需要 chunk 正文，但正文不会被保留：切词后即丢弃。这样
        // `execute_command` 这类只出现在函数体里的标识符也能被检索到，同时不违反
        // 存储 ADR 的"不驻留 chunk 全文"。
        let lines: Vec<&str> = content.lines().collect();
        for chunk in &parsed.chunks {
            let body = slice_lines(&lines, chunk.start_line, chunk.end_line);
            self.retrieval.index_chunk(chunk, &body);
        }
        self.index.upsert_file(file, parsed);
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

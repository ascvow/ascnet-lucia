//! 仓库发现：找出应当进入索引的文件。
//!
//! 输入是宿主注入的可信工作区，输出是稳定的 [`IndexableFile`] 列表。扫描器负责遍历、
//! 忽略规则、语言识别、敏感文件排除与内容哈希；**不做任何解析**——那是 Phase 3 的职责。
//!
//! # 为什么扫描必须可分片
//!
//! 宿主给每个插件 5000 万 fuel，且**每次导出调用前重置**（`wasm/engine.rs`）。一次全量扫描
//! 中等仓库的指令数远超单次调用预算，跑满即 trap。因此扫描器不提供"一次调用扫完"的接口，
//! 只提供 [`RepositoryScan::advance`]：消耗到预算上限就保存游标返回，下次调用从断点继续。
//!
//! 这套游标是**全局机制**，Phase 4 的索引构建与 Phase 11 的增量重建都复用同一形态，
//! 不再各自发明。
//!
//! # 一致性边界
//!
//! 扫描期间仓库可能被外部修改，因此本模块不承诺快照语义：它承诺的是每个文件的
//! `content_hash` 与读到的那一份内容一致。跨文件的时间点一致性由上层的惰性校验兜底
//! （见 `architecture.md` 第 8 问）。

pub mod ignore;
pub mod language;

use agent_plugin::PluginHostApi;
use ignore::{ChainId, IgnoreRuleSet, IgnoreStack, AGENT_IGNORE_FILE, GIT_IGNORE_FILE};
use language::{classify, Language, PathClass, SkipReason};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, VecDeque};

/// 扫描规模上限。
///
/// 这些不是性能调优参数，而是**安全边界**：插件运行在 64 MiB 线性内存里，且符号链接构成的
/// 目录环会产生无限深的新路径（软链目标的相对路径每次都不同，因此按路径去重挡不住它）。
/// 深度与数量上限是这类情况唯一可靠的防线。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    /// 相对仓库根的最大遍历深度。
    pub max_depth: u32,
    /// 最多访问的目录数。
    pub max_dirs: u32,
    /// 最多收录的文件数。
    pub max_files: u32,
    /// 单个目录最多处理的条目数。
    pub max_entries_per_dir: u32,
    /// 单个文件最大字节数，超出即跳过。
    pub max_file_bytes: u32,
    /// 最多记录的告警条数，超出后只累加计数。
    pub max_warnings: u32,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_depth: 24,
            max_dirs: 20_000,
            max_files: 50_000,
            max_entries_per_dir: 10_000,
            // 1 MiB：超过这个体量的源文件在真实仓库里几乎都是生成产物或数据文件。
            max_file_bytes: 1024 * 1024,
            max_warnings: 256,
        }
    }
}

/// 单次 [`RepositoryScan::advance`] 的预算。
///
/// 两个维度都必要：目录列举的成本由调用次数主导，文件读取的成本由字节数主导。只按调用
/// 次数限制会让一批大文件冲垮 fuel 预算，只按字节数限制则挡不住海量空文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanBudget {
    /// 本次最多发起的宿主调用次数。
    pub max_host_calls: u32,
    /// 本次最多读取的字节数。
    pub max_bytes_read: u64,
}

impl Default for ScanBudget {
    fn default() -> Self {
        Self {
            max_host_calls: 512,
            max_bytes_read: 8 * 1024 * 1024,
        }
    }
}

/// 扫描过程中接收文件正文的回调。
///
/// 扫描器读取文件本就是为了算内容哈希，而索引构建需要同一份正文去做解析。让调用方在
/// 读取的**同一时刻**拿到正文，既避免"扫描读一遍、解析再读一遍"的双倍宿主调用，
/// 也避免为了后续解析而把整个仓库的正文驻留在内存里。
pub trait FileSink {
    /// 接收一个刚读取并通过校验的文件。
    ///
    /// `content` 在本次调用之后即失效，实现必须自行提取需要保留的信息。
    fn accept(&mut self, file: &IndexableFile, content: &str);
}

/// 丢弃全部正文的 sink，用于只需要哈希与元数据的场景。
impl FileSink for () {
    fn accept(&mut self, _file: &IndexableFile, _content: &str) {}
}

/// 一个应当进入索引的文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexableFile {
    /// 仓库相对路径。
    pub path: String,
    /// 识别出的语言。
    pub language: Language,
    /// 文件内容哈希，用于增量判断与返回前的一致性校验。
    pub content_hash: String,
    /// 文件字节数。
    pub byte_len: u32,
    /// 文件行数。
    pub line_count: u32,
}

/// 扫描过程中的告警原因。
///
/// 全部告警都是**隔离的**：任何单个文件或目录的失败都不会中断扫描，更不会让整个仓库
/// 索引失败。这是 `architecture.md` 第 12 问的硬性要求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ScanWarningKind {
    /// 目录列举失败。
    ListFailed {
        /// 宿主返回的原因。
        reason: String,
    },
    /// 文件读取失败；通常是二进制内容或权限问题。
    ReadFailed {
        /// 宿主返回的原因。
        reason: String,
    },
    /// 文件体量超过上限。
    TooLarge {
        /// 实际字节数。
        bytes: u32,
    },
    /// 内容包含 NUL 字节，判定为二进制。
    BinaryContent,
    /// 目录深度超过上限，未继续下探。
    DepthExceeded,
    /// 触及规模上限，剩余内容未被扫描。
    LimitReached,
}

/// 一条扫描告警。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanWarning {
    /// 触发告警的仓库相对路径。
    pub path: String,
    /// 告警原因。
    #[serde(flatten)]
    pub kind: ScanWarningKind,
}

/// 扫描统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanStats {
    /// 已访问目录数。
    pub dirs_visited: u32,
    /// 收录的文件数。
    pub files_indexed: u32,
    /// 因二进制、生成产物或未识别扩展名跳过的文件数。
    pub files_skipped: u32,
    /// 因命中敏感模式排除的文件数。
    pub files_sensitive: u32,
    /// 被忽略规则排除的条目数。
    pub entries_ignored: u32,
    /// 累计读取字节数。
    pub bytes_read: u64,
    /// 累计告警数，含超出记录上限后未保留的部分。
    pub warnings_total: u32,
}

/// 一次 `advance` 之后的推进结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanProgress {
    /// 预算耗尽，游标已保存，需要再次调用 `advance`。
    Incomplete,
    /// 扫描已完成。
    Complete,
}

/// 待遍历的目录。
#[derive(Debug, Clone)]
struct PendingDir {
    /// 仓库相对路径；仓库根为空串。
    path: String,
    /// 相对仓库根的深度。
    depth: u32,
    /// 该目录继承的忽略规则链。
    chain: ChainId,
}

/// 待读取的文件。
#[derive(Debug, Clone)]
struct PendingFile {
    /// 仓库相对路径。
    path: String,
    /// 已识别的语言。
    language: Language,
}

/// 一次可分片推进的仓库扫描。
#[derive(Debug)]
pub struct RepositoryScan {
    /// 规模上限。
    limits: ScanLimits,
    /// 忽略规则链 arena。
    ignores: IgnoreStack,
    /// 待遍历目录队列。
    pending_dirs: VecDeque<PendingDir>,
    /// 待读取文件队列。
    pending_files: VecDeque<PendingFile>,
    /// 已收录的文件。
    files: Vec<IndexableFile>,
    /// 本轮成功列举到的全部非目录路径。
    ///
    /// 增量索引用它判断删除：路径从目录快照中消失才可视为删除；仅仅读取失败不能
    /// 擅自删除上一轮仍然有效的索引。
    discovered_paths: BTreeSet<String>,
    /// 本轮被识别为可索引类型的路径。
    indexable_paths: BTreeSet<String>,
    /// 因文件类型或正文校验而明确无法进入索引的路径。
    rejected_paths: BTreeSet<String>,
    /// 宿主读取失败的路径。该类失败通常是暂时的权限或编码问题，增量索引应保留旧值。
    read_failed_paths: BTreeSet<String>,
    /// 保留的告警。
    warnings: Vec<ScanWarning>,
    /// 统计。
    stats: ScanStats,
    /// 扫描是否已完成。
    complete: bool,
}

impl RepositoryScan {
    /// 创建一次从仓库根开始的扫描。
    pub fn new(limits: ScanLimits) -> Self {
        let ignores = IgnoreStack::new();
        let root_chain = ignores.root();
        let mut pending_dirs = VecDeque::new();
        pending_dirs.push_back(PendingDir {
            path: String::new(),
            depth: 0,
            chain: root_chain,
        });
        Self {
            limits,
            ignores,
            pending_dirs,
            pending_files: VecDeque::new(),
            files: Vec::new(),
            discovered_paths: BTreeSet::new(),
            indexable_paths: BTreeSet::new(),
            rejected_paths: BTreeSet::new(),
            read_failed_paths: BTreeSet::new(),
            warnings: Vec::new(),
            stats: ScanStats::default(),
            complete: false,
        }
    }

    /// 使用默认上限创建扫描。
    pub fn with_default_limits() -> Self {
        Self::new(ScanLimits::default())
    }

    /// 已收录的文件。
    ///
    /// 扫描完成后按路径升序排列；未完成时为发现顺序。
    pub fn files(&self) -> &[IndexableFile] {
        &self.files
    }

    /// 返回本轮目录快照中出现的全部非目录路径。
    pub fn discovered_paths(&self) -> &BTreeSet<String> {
        &self.discovered_paths
    }

    /// 返回本轮被识别为可索引类型的路径。
    pub fn indexable_paths(&self) -> &BTreeSet<String> {
        &self.indexable_paths
    }

    /// 返回本轮明确被判定为不可索引的路径。
    pub fn rejected_paths(&self) -> &BTreeSet<String> {
        &self.rejected_paths
    }

    /// 返回本轮宿主读取失败的路径。
    pub fn read_failed_paths(&self) -> &BTreeSet<String> {
        &self.read_failed_paths
    }

    /// 判断目录快照是否足以安全推断删除。
    ///
    /// 目录列举失败、深度限制或规模限制都会留下未知区域；此时增量索引必须保留
    /// 未出现在快照中的旧文件，等待下一轮完整快照，而不是误删索引。
    pub fn snapshot_is_complete(&self) -> bool {
        // 告警达到上限后，未保留的告警种类未知；宁可暂缓删除，也不能把未知区域误判为
        // 已消失文件。
        if self.stats.warnings_total as usize > self.warnings.len() {
            return false;
        }
        !self.warnings.iter().any(|warning| {
            matches!(
                warning.kind,
                ScanWarningKind::ListFailed { .. }
                    | ScanWarningKind::DepthExceeded
                    | ScanWarningKind::LimitReached
            )
        })
    }

    /// 保留的告警。
    pub fn warnings(&self) -> &[ScanWarning] {
        &self.warnings
    }

    /// 扫描统计。
    pub fn stats(&self) -> ScanStats {
        self.stats
    }

    /// 扫描是否已完成。
    pub fn is_complete(&self) -> bool {
        self.complete
    }

    /// 在给定预算内推进扫描。
    ///
    /// 等价于不带 sink 的 [`RepositoryScan::advance_with`]，只计算文件哈希与元数据。
    pub fn advance(&mut self, host: &dyn PluginHostApi, budget: ScanBudget) -> ScanProgress {
        self.advance_with(host, budget, &mut ())
    }

    /// 在给定预算内推进扫描，并把每个文件的正文交给 `sink`。
    ///
    /// 每轮循环先检查预算再发起宿主调用，因此返回时实际消耗不会超过预算。目录列举与
    /// 文件读取共享同一预算：先耗尽目录队列，再逐个读取文件内容。
    ///
    /// # Errors
    ///
    /// 不返回错误。单个目录或文件的失败被记录为告警并继续——一个读不了的文件不能让整个
    /// 仓库无法检索。
    pub fn advance_with(
        &mut self,
        host: &dyn PluginHostApi,
        budget: ScanBudget,
        sink: &mut dyn FileSink,
    ) -> ScanProgress {
        self.advance_with_filter(host, budget, sink, &|_, _| true)
    }

    /// 在给定预算内推进扫描，并按 `read_filter` 选择需要读取正文的文件。
    ///
    /// 目录仍会完整遍历，因此调用方可以发现新增与删除；被过滤掉的既有文件不会发起
    /// `read_file`，适合脏集合驱动的增量重建。筛选只影响正文读取，不改变目录快照与路径
    /// 分类结果。过滤器必须保持幂等，因为同一个扫描器会跨多次调用保存游标。
    pub fn advance_with_filter(
        &mut self,
        host: &dyn PluginHostApi,
        budget: ScanBudget,
        sink: &mut dyn FileSink,
        read_filter: &dyn Fn(&str, Language) -> bool,
    ) -> ScanProgress {
        let mut calls = 0u32;
        let mut bytes = 0u64;

        loop {
            if self.complete {
                return ScanProgress::Complete;
            }
            if calls >= budget.max_host_calls || bytes >= budget.max_bytes_read {
                return ScanProgress::Incomplete;
            }

            if let Some(dir) = self.pending_dirs.pop_front() {
                calls += self.visit_dir(host, &dir);
                continue;
            }
            if let Some(file) = self.pending_files.pop_front() {
                if !read_filter(&file.path, file.language) {
                    continue;
                }
                calls += 1;
                bytes += self.read_file(host, &file, sink);
                continue;
            }

            self.finish();
            return ScanProgress::Complete;
        }
    }

    /// 完成扫描并整理输出顺序。
    ///
    /// 按路径排序而不是保留广度优先的发现顺序：下游的增量 diff 需要在两次扫描结果之间做
    /// 有序比较，发现顺序会让目录结构的微小变化产生大量伪差异。
    fn finish(&mut self) {
        self.files.sort_by(|left, right| left.path.cmp(&right.path));
        self.complete = true;
    }

    /// 访问一个目录，返回消耗的宿主调用次数。
    fn visit_dir(&mut self, host: &dyn PluginHostApi, dir: &PendingDir) -> u32 {
        if self.stats.dirs_visited >= self.limits.max_dirs {
            self.warn(&dir.path, ScanWarningKind::LimitReached);
            return 0;
        }
        // 宿主用 "." 表示仓库根；子目录直接传仓库相对路径。
        let request = if dir.path.is_empty() { "." } else { &dir.path };
        let entries = match host.list_dir(request) {
            Ok(entries) => entries,
            Err(error) => {
                self.warn(
                    &dir.path,
                    ScanWarningKind::ListFailed {
                        reason: error.to_string(),
                    },
                );
                return 1;
            }
        };
        self.stats.dirs_visited += 1;
        let mut calls = 1;

        // 忽略文件必须先于本目录条目生效，否则本轮就会把应当排除的子目录入队。
        let chain = self.load_ignore_files(host, dir, &entries, &mut calls);

        for entry in entries
            .iter()
            .take(self.limits.max_entries_per_dir as usize)
        {
            let path = entry.path.trim_start_matches("./");
            if path.is_empty() || path == "." {
                continue;
            }
            if self.ignores.is_ignored(chain, path, entry.is_dir) {
                self.stats.entries_ignored += 1;
                continue;
            }
            if !entry.is_dir {
                self.discovered_paths.insert(path.to_string());
            }
            if entry.is_dir {
                self.enqueue_dir(path, dir.depth + 1, chain);
            } else {
                self.enqueue_file(path);
            }
        }
        if entries.len() > self.limits.max_entries_per_dir as usize {
            self.warn(&dir.path, ScanWarningKind::LimitReached);
        }
        calls
    }

    /// 读取本目录下的忽略文件并追加到规则链。
    ///
    /// `.gitignore` 先于 `.agentignore` 入链，因此插件专属规则可以覆盖 git 的判定——
    /// 用户用 `.agentignore` 表达的是"给检索引擎看什么"，理应比版本控制规则更靠后。
    fn load_ignore_files(
        &mut self,
        host: &dyn PluginHostApi,
        dir: &PendingDir,
        entries: &[agent_plugin::FileEntry],
        calls: &mut u32,
    ) -> ChainId {
        let mut chain = dir.chain;
        for name in [GIT_IGNORE_FILE, AGENT_IGNORE_FILE] {
            let expected = join_path(&dir.path, name);
            let present = entries
                .iter()
                .any(|entry| !entry.is_dir && entry.path.trim_start_matches("./") == expected);
            if !present {
                continue;
            }
            *calls += 1;
            match host.read_file(&expected) {
                Ok(content) => {
                    let set = IgnoreRuleSet::parse(&dir.path, &content);
                    chain = self.ignores.push(chain, set);
                }
                // 读不到忽略文件不是致命问题：继续用已有规则扫描，只是可能多收录一些文件。
                Err(error) => self.warn(
                    &expected,
                    ScanWarningKind::ReadFailed {
                        reason: error.to_string(),
                    },
                ),
            }
        }
        chain
    }

    /// 把子目录加入待遍历队列。
    fn enqueue_dir(&mut self, path: &str, depth: u32, chain: ChainId) {
        if depth > self.limits.max_depth {
            self.warn(path, ScanWarningKind::DepthExceeded);
            return;
        }
        self.pending_dirs.push_back(PendingDir {
            path: path.to_string(),
            depth,
            chain,
        });
    }

    /// 归类文件并在可索引时加入待读取队列。
    fn enqueue_file(&mut self, path: &str) {
        match classify(path) {
            PathClass::Indexable(language) => {
                self.indexable_paths.insert(path.to_string());
                if self.stats.files_indexed + self.pending_files.len() as u32
                    >= self.limits.max_files
                {
                    self.warn(path, ScanWarningKind::LimitReached);
                    return;
                }
                self.pending_files.push_back(PendingFile {
                    path: path.to_string(),
                    language,
                });
            }
            // 敏感文件单独计数：API 模式会把文件内容发往外部 provider，这个数字是隐私
            // 审计的直接依据，不能和普通跳过混在一起。
            PathClass::Sensitive => {
                self.rejected_paths.insert(path.to_string());
                self.stats.files_sensitive += 1;
            }
            PathClass::Skipped(
                SkipReason::Binary | SkipReason::Generated | SkipReason::Unrecognized,
            ) => {
                self.rejected_paths.insert(path.to_string());
                self.stats.files_skipped += 1;
            }
        }
    }

    /// 读取文件内容并生成索引条目，返回读取的字节数。
    fn read_file(
        &mut self,
        host: &dyn PluginHostApi,
        file: &PendingFile,
        sink: &mut dyn FileSink,
    ) -> u64 {
        let content = match host.read_file(&file.path) {
            Ok(content) => content,
            // 宿主的 `read_file` 走 UTF-8 校验，因此漏网的二进制文件会在这里失败。
            // 记录并跳过即可，不需要额外的内容嗅探。
            Err(error) => {
                self.read_failed_paths.insert(file.path.clone());
                self.warn(
                    &file.path,
                    ScanWarningKind::ReadFailed {
                        reason: error.to_string(),
                    },
                );
                self.stats.files_skipped += 1;
                return 0;
            }
        };

        let byte_len = content.len();
        self.stats.bytes_read += byte_len as u64;
        if byte_len > self.limits.max_file_bytes as usize {
            self.rejected_paths.insert(file.path.clone());
            self.warn(
                &file.path,
                ScanWarningKind::TooLarge {
                    bytes: u32::try_from(byte_len).unwrap_or(u32::MAX),
                },
            );
            self.stats.files_skipped += 1;
            return byte_len as u64;
        }
        // UTF-8 校验挡不住内嵌 NUL 的文件；它们能解码成字符串但不是可检索的文本。
        if content.contains('\0') {
            self.rejected_paths.insert(file.path.clone());
            self.warn(&file.path, ScanWarningKind::BinaryContent);
            self.stats.files_skipped += 1;
            return byte_len as u64;
        }

        let indexable = IndexableFile {
            path: file.path.clone(),
            language: file.language,
            content_hash: content_hash(&content),
            byte_len: u32::try_from(byte_len).unwrap_or(u32::MAX),
            line_count: u32::try_from(content.lines().count()).unwrap_or(u32::MAX),
        };
        // 正文在这里交给 sink 后即被丢弃：索引构建需要它做解析，但把整个仓库的正文
        // 驻留在 64 MiB 线性内存里是不可行的。
        sink.accept(&indexable, &content);
        self.files.push(indexable);
        self.stats.files_indexed += 1;
        byte_len as u64
    }

    /// 记录一条告警。
    ///
    /// 超过上限后只累加计数：一个病态仓库能产生几十万条告警，把它们全部驻留在 64 MiB
    /// 线性内存里会比扫描失败更糟。
    fn warn(&mut self, path: &str, kind: ScanWarningKind) {
        self.stats.warnings_total += 1;
        if self.warnings.len() >= self.limits.max_warnings as usize {
            return;
        }
        self.warnings.push(ScanWarning {
            path: path.to_string(),
            kind,
        });
    }
}

/// 拼接仓库相对路径。
fn join_path(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// 计算文件内容哈希。
///
/// 带域前缀，避免与嵌入空间标识、仓库身份等其他摘要在同一取值空间内碰撞。
pub fn content_hash(content: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"code-context-content-v1\0");
    digest.update(content.as_bytes());
    format!("{:x}", digest.finalize())
}

/// 把扫描结果整理成路径到哈希的映射，用于增量判断。
///
/// 该映射体量受控（2000 文件约 160 KB），是 `adr-storage.md` 认定可以放进宿主 JSON KV
/// 的两类数据之一。
pub fn hash_index(files: &[IndexableFile]) -> Vec<(String, String)> {
    files
        .iter()
        .map(|file| (file.path.clone(), file.content_hash.clone()))
        .collect()
}

#[cfg(test)]
mod tests;

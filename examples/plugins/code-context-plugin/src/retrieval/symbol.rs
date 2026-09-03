//! 符号检索。
//!
//! 符号检索回答的是"这个名字定义在哪"，与词法检索的"哪些代码提到了这些词"互补。它必须
//! 处理两种形态的查询：**整体是一个标识符**（`PermissionManager::check`），以及
//! **自然语言里夹着标识符**（"where does PermissionManager check approvals"）。
//!
//! 结构上是两份按名称排序的下标数组 + 二分查找：限定名一份、短名一份。前缀查询用
//! `partition_point` 定位区间，不需要额外的 trie。

use crate::model::CodeChunk;
use std::collections::HashSet;

/// 前缀匹配要求的最小查询长度。
///
/// 一到两个字符的前缀会匹配到成百上千个符号，既无检索价值又会淹没精确匹配。
const MIN_PREFIX_CHARS: usize = 3;

/// 单次检索最多考察的前缀匹配数。
const MAX_PREFIX_MATCHES: usize = 512;

/// 匹配方式，决定候选的基础分。
///
/// 分值刻意拉开档次：精确限定名匹配与前缀匹配之间的差距，必须大到让融合层无论怎么加权
/// 都不会把前缀匹配排到精确匹配前面。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SymbolMatch {
    /// 限定名完全相同。
    ExactQualified,
    /// 短名完全相同。
    ExactName,
    /// 限定名以 `分隔符 + 查询` 结尾，例如查 `check` 命中 `Manager::check`。
    QualifiedSuffix,
    /// 短名以查询为前缀。
    NamePrefix,
}

impl SymbolMatch {
    /// 返回该匹配方式的基础分。
    pub fn score(self) -> f32 {
        match self {
            Self::ExactQualified => 1.0,
            Self::ExactName => 0.85,
            Self::QualifiedSuffix => 0.75,
            Self::NamePrefix => 0.4,
        }
    }
}

/// 符号表中的一条记录。
#[derive(Debug, Clone)]
struct Entry {
    /// 对应的 chunk ID。
    chunk_id: String,
    /// 小写短名。
    name: String,
    /// 小写限定名。
    qualified: String,
    /// 所属文件路径。
    file_path: String,
}

/// 符号索引。
#[derive(Debug, Default)]
pub struct SymbolIndex {
    /// 全部记录，按插入顺序。
    entries: Vec<Entry>,
    /// 按限定名排序的记录下标。
    by_qualified: Vec<u32>,
    /// 按短名排序的记录下标。
    by_name: Vec<u32>,
    /// 排序是否已随最新写入更新。
    sorted: bool,
}

impl SymbolIndex {
    /// 创建空索引。
    pub fn new() -> Self {
        Self::default()
    }

    /// 符号数。
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// 索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 收录一个带符号信息的 chunk。
    ///
    /// 没有限定名的 chunk（文本兜底产物）不进入符号表——它们没有可供定位的符号。
    pub fn index_chunk(&mut self, chunk: &CodeChunk) {
        let (Some(name), Some(qualified)) = (
            chunk.symbol_name.as_deref(),
            chunk.qualified_symbol.as_deref(),
        ) else {
            return;
        };
        if name.is_empty() {
            return;
        }
        self.entries.push(Entry {
            chunk_id: chunk.id.clone(),
            name: name.to_lowercase(),
            qualified: qualified.to_lowercase(),
            file_path: chunk.file_path.clone(),
        });
        self.sorted = false;
    }

    /// 移除指定 chunk 在符号表中的记录。
    ///
    /// 返回实际移除的符号条数。删除后排序下标会失效，下一次 [`Self::finalize`] 会重新
    /// 建立；在重新 finalize 前搜索继续安全返回空结果。
    pub fn remove_chunks(&mut self, chunk_ids: &[String]) -> usize {
        if chunk_ids.is_empty() || self.entries.is_empty() {
            return 0;
        }
        let ids: HashSet<&str> = chunk_ids.iter().map(String::as_str).collect();
        let before = self.entries.len();
        self.entries
            .retain(|entry| !ids.contains(entry.chunk_id.as_str()));
        let removed = before.saturating_sub(self.entries.len());
        if removed > 0 {
            self.sorted = false;
        }
        removed
    }

    /// 移除指定文件的全部符号记录。
    pub fn remove_file(&mut self, path: &str) -> usize {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.file_path != path);
        let removed = before.saturating_sub(self.entries.len());
        if removed > 0 {
            self.sorted = false;
        }
        removed
    }

    /// 建立排序索引。
    ///
    /// 必须在全部写入之后、首次检索之前调用。
    pub fn finalize(&mut self) {
        if self.sorted {
            return;
        }
        let count = u32::try_from(self.entries.len()).unwrap_or(u32::MAX);
        self.by_qualified = (0..count).collect();
        self.by_name = (0..count).collect();
        self.by_qualified.sort_by(|left, right| {
            self.entries[*left as usize]
                .qualified
                .cmp(&self.entries[*right as usize].qualified)
        });
        self.by_name.sort_by(|left, right| {
            self.entries[*left as usize]
                .name
                .cmp(&self.entries[*right as usize].name)
        });
        self.sorted = true;
    }

    /// 执行一次符号检索。
    ///
    /// 同时按两种形态匹配：整条查询当作标识符，以及查询切词后逐个当作标识符。同一 chunk
    /// 命中多次时保留最高分。
    ///
    /// `path_scope` 非空时只保留路径以其为前缀的符号。
    pub fn search(
        &self,
        query: &str,
        path_scope: Option<&str>,
        limit: usize,
    ) -> Vec<(&str, f32, SymbolMatch)> {
        if self.entries.is_empty() || limit == 0 || !self.sorted {
            return Vec::new();
        }

        let mut best: Vec<(u32, f32, SymbolMatch)> = Vec::new();
        let mut record = |index: u32, kind: SymbolMatch| {
            let score = kind.score();
            match best.iter_mut().find(|(existing, _, _)| *existing == index) {
                Some(slot) if slot.1 < score => {
                    slot.1 = score;
                    slot.2 = kind;
                }
                Some(_) => {}
                None => best.push((index, score, kind)),
            }
        };

        // 整条查询当作标识符：`PermissionManager::check` 这类查询走这条路径。
        let whole = query.trim().to_lowercase();
        if !whole.is_empty() {
            self.match_term(&whole, &mut record);
        }
        // 自然语言查询里夹带的标识符走这条路径。
        for term in super::tokenize::tokenize(query) {
            if term != whole {
                self.match_term(&term, &mut record);
            }
        }

        let mut ranked: Vec<(&str, f32, SymbolMatch)> = best
            .into_iter()
            .filter_map(|(index, score, kind)| {
                let entry = self.entries.get(index as usize)?;
                if let Some(scope) = path_scope {
                    if !entry.file_path.starts_with(scope) {
                        return None;
                    }
                }
                Some((entry.chunk_id.as_str(), score, kind))
            })
            .collect();
        // 分数相同时按 chunk ID 排序，保证结果确定。
        ranked.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| left.0.cmp(right.0))
        });
        ranked.truncate(limit);
        ranked
    }

    /// 用一个词元匹配符号表。
    fn match_term(&self, term: &str, record: &mut impl FnMut(u32, SymbolMatch)) {
        for index in self.equal_range(&self.by_qualified, term, |entry| &entry.qualified) {
            record(index, SymbolMatch::ExactQualified);
        }
        for index in self.equal_range(&self.by_name, term, |entry| &entry.name) {
            record(index, SymbolMatch::ExactName);
        }
        // 限定名后缀：查 `check` 应当命中 `PermissionManager::check`。
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.name == term {
                continue;
            }
            if entry
                .qualified
                .rsplit(['.', ':'])
                .next()
                .is_some_and(|tail| tail == term)
            {
                record(
                    u32::try_from(index).unwrap_or(u32::MAX),
                    SymbolMatch::QualifiedSuffix,
                );
            }
        }
        if term.chars().count() >= MIN_PREFIX_CHARS {
            for index in self
                .prefix_range(&self.by_name, term, |entry| &entry.name)
                .take(MAX_PREFIX_MATCHES)
            {
                record(index, SymbolMatch::NamePrefix);
            }
        }
    }

    /// 返回排序数组中与 `term` 完全相等的记录下标。
    fn equal_range<'a>(
        &'a self,
        order: &'a [u32],
        term: &'a str,
        key: impl Fn(&Entry) -> &str + Copy + 'a,
    ) -> impl Iterator<Item = u32> + 'a {
        let start = order.partition_point(|index| key(&self.entries[*index as usize]) < term);
        order[start..]
            .iter()
            .take_while(move |index| key(&self.entries[**index as usize]) == term)
            .copied()
    }

    /// 返回排序数组中以 `prefix` 开头的记录下标。
    fn prefix_range<'a>(
        &'a self,
        order: &'a [u32],
        prefix: &'a str,
        key: impl Fn(&Entry) -> &str + Copy + 'a,
    ) -> impl Iterator<Item = u32> + 'a {
        let start = order.partition_point(|index| key(&self.entries[*index as usize]) < prefix);
        order[start..]
            .iter()
            .take_while(move |index| key(&self.entries[**index as usize]).starts_with(prefix))
            .copied()
    }
}

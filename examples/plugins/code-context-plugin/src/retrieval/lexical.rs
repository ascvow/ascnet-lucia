//! 词法检索：BM25 倒排索引。
//!
//! # 索引正文但不驻留正文
//!
//! [存储 ADR](../../../docs/code-context-engine/adr-storage.md) 要求"不驻留 chunk 全文"。
//! 倒排索引不违反这一点：它保存的是 `term → (chunk, 词频)`，正文在建索引时被切词后即丢弃。
//! 这既让 `execute_command` 这类**函数体内的标识符**可被检索到，又不把源码副本留在内存里。
//!
//! # 字段加权
//!
//! 同一个词出现在符号名里和出现在函数体里，检索价值相差很远。这里用**重复计入词频**实现
//! 字段加权：符号名的词元按 5 倍计，正文按 1 倍计。这样 BM25 无需改动公式即可体现字段
//! 重要性，也避免了维护多套倒排表。

use super::tokenize::{tokenize, tokenize_query};
use crate::model::CodeChunk;
use std::collections::{HashMap, HashSet};

/// BM25 的词频饱和参数。
const BM25_K1: f32 = 1.2;

/// BM25 的文档长度归一化参数。
const BM25_B: f32 = 0.75;

/// 符号短名的字段权重。
const WEIGHT_SYMBOL: u32 = 5;

/// 限定名的字段权重。
const WEIGHT_QUALIFIED: u32 = 3;

/// 文件路径的字段权重。
const WEIGHT_PATH: u32 = 2;

/// 签名的字段权重。
const WEIGHT_SIGNATURE: u32 = 2;

/// 文档注释的字段权重。
const WEIGHT_DOC: u32 = 2;

/// 正文的字段权重。
const WEIGHT_BODY: u32 = 1;

/// 倒排索引允许保存的最大 posting 数。
///
/// 上限存在的理由是 64 MiB 线性内存。触顶后停止收录新词元并标记截断，如实上报，
/// 而不是静默丢掉半个仓库的可检索性。
const MAX_POSTINGS: usize = 3_000_000;

/// 一条倒排记录。
#[derive(Debug, Clone, Copy)]
struct Posting {
    /// 文档下标。
    doc: u32,
    /// 加权后的词频。
    frequency: u32,
}

/// 一个被索引的文档。
#[derive(Debug, Clone)]
struct Document {
    /// 对应的 chunk ID。
    chunk_id: String,
    /// 所属文件路径，供按路径过滤使用。
    file_path: String,
    /// 加权后的文档长度。
    length: u32,
}

/// BM25 倒排索引。
#[derive(Debug, Default)]
pub struct LexicalIndex {
    /// 词元 → 倒排表。
    postings: HashMap<String, Vec<Posting>>,
    /// 全部文档。
    documents: Vec<Document>,
    /// 全部文档的加权长度之和。
    total_length: u64,
    /// posting 总数。
    posting_count: usize,
    /// 是否因触及上限而截断。
    truncated: bool,
}

impl LexicalIndex {
    /// 创建空索引。
    pub fn new() -> Self {
        Self::default()
    }

    /// 文档数。
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// 索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// 是否因触及上限而截断。
    pub fn is_truncated(&self) -> bool {
        self.truncated
    }

    /// 不同词元数。
    pub fn term_count(&self) -> usize {
        self.postings.len()
    }

    /// 返回包含指定词元的文档数。
    ///
    /// 词元必须已按 [`tokenize`] 的规则规范化（小写、中文按单字与双字切分），否则永远
    /// 返回 0。查询分析用它判断一个中文组合在本仓库里到底存不存在。
    pub fn document_frequency(&self, term: &str) -> usize {
        self.postings.get(term).map_or(0, Vec::len)
    }

    /// 把一个 chunk 连同它的正文加入索引。
    ///
    /// `body` 在本次调用后即可丢弃：索引只保留切词结果。
    pub fn index_chunk(&mut self, chunk: &CodeChunk, body: &str) {
        if self.posting_count >= MAX_POSTINGS {
            self.truncated = true;
            return;
        }

        let mut frequencies: HashMap<String, u32> = HashMap::new();
        let mut add = |text: &str, weight: u32| {
            for token in tokenize(text) {
                *frequencies.entry(token).or_insert(0) += weight;
            }
        };

        if let Some(name) = chunk.symbol_name.as_deref() {
            add(name, WEIGHT_SYMBOL);
        }
        if let Some(qualified) = chunk.qualified_symbol.as_deref() {
            add(qualified, WEIGHT_QUALIFIED);
        }
        add(&chunk.file_path, WEIGHT_PATH);
        if let Some(signature) = chunk.signature.as_deref() {
            add(signature, WEIGHT_SIGNATURE);
        }
        if let Some(doc) = chunk.doc_comment.as_deref() {
            add(doc, WEIGHT_DOC);
        }
        add(body, WEIGHT_BODY);

        if frequencies.is_empty() {
            return;
        }
        let Ok(doc) = u32::try_from(self.documents.len()) else {
            self.truncated = true;
            return;
        };
        let length: u32 = frequencies.values().copied().sum();
        for (term, frequency) in frequencies {
            self.postings
                .entry(term)
                .or_default()
                .push(Posting { doc, frequency });
            self.posting_count += 1;
        }
        self.total_length += u64::from(length);
        self.documents.push(Document {
            chunk_id: chunk.id.clone(),
            file_path: chunk.file_path.clone(),
            length,
        });
    }

    /// 移除指定 chunk 的词法文档及其全部 posting。
    ///
    /// 文档下标是 posting 的内部引用，删除时会压缩文档数组并重映射剩余 posting，确保
    /// BM25 不会访问失效下标，也不会残留旧词元。返回实际移除的文档数。
    pub fn remove_chunks(&mut self, chunk_ids: &[String]) -> usize {
        if chunk_ids.is_empty() || self.documents.is_empty() {
            return 0;
        }
        let ids: HashSet<&str> = chunk_ids.iter().map(String::as_str).collect();
        self.remove_documents(|document| ids.contains(document.chunk_id.as_str()))
    }

    /// 移除指定文件的全部词法文档。
    pub fn remove_file(&mut self, path: &str) -> usize {
        if self.documents.is_empty() {
            return 0;
        }
        self.remove_documents(|document| document.file_path == path)
    }

    /// 按谓词压缩文档与 posting，并重算 BM25 的全局长度统计。
    fn remove_documents(&mut self, should_remove: impl Fn(&Document) -> bool) -> usize {
        let mut remap = vec![None; self.documents.len()];
        let mut retained = Vec::with_capacity(self.documents.len());
        let mut removed = 0usize;
        for (old_doc, document) in self.documents.iter().enumerate() {
            if should_remove(document) {
                removed += 1;
                continue;
            }
            let new_doc = u32::try_from(retained.len()).unwrap_or(u32::MAX);
            remap[old_doc] = Some(new_doc);
            retained.push(document.clone());
        }
        if removed == 0 {
            return 0;
        }
        self.documents = retained;
        self.postings.retain(|_, postings| {
            let mut rewritten = Vec::with_capacity(postings.len());
            for posting in postings.iter() {
                let Some(new_doc) = remap.get(posting.doc as usize).and_then(|doc| *doc) else {
                    continue;
                };
                rewritten.push(Posting {
                    doc: new_doc,
                    frequency: posting.frequency,
                });
            }
            *postings = rewritten;
            !postings.is_empty()
        });
        self.posting_count = self.postings.values().map(Vec::len).sum();
        self.total_length = self
            .documents
            .iter()
            .map(|document| u64::from(document.length))
            .sum();
        removed
    }

    /// 执行一次 BM25 检索。
    ///
    /// `path_scope` 非空时只保留路径以其为前缀的文档。返回按分数降序排列的
    /// `(chunk_id, score)`，最多 `limit` 条。
    pub fn search(&self, query: &str, path_scope: Option<&str>, limit: usize) -> Vec<(&str, f32)> {
        if self.documents.is_empty() || limit == 0 {
            return Vec::new();
        }
        let terms = tokenize_query(query);
        if terms.is_empty() {
            return Vec::new();
        }
        let document_count = self.documents.len() as f32;
        let average_length = self.total_length as f32 / document_count;

        // 查询里重复出现的词元只计一次：BM25 的查询侧词频对代码检索没有增益，
        // 反而会让"permission permission"这类重复放大成噪声。
        let mut seen: Vec<&str> = Vec::new();
        let mut scores: HashMap<u32, f32> = HashMap::new();
        for term in &terms {
            if seen.contains(&term.as_str()) {
                continue;
            }
            seen.push(term);
            let Some(postings) = self.postings.get(term) else {
                continue;
            };
            let document_frequency = postings.len() as f32;
            let inverse_frequency = (1.0
                + (document_count - document_frequency + 0.5) / (document_frequency + 0.5))
                .ln();
            for posting in postings {
                let Some(document) = self.documents.get(posting.doc as usize) else {
                    continue;
                };
                let frequency = posting.frequency as f32;
                let normalized = frequency
                    / (frequency
                        + BM25_K1
                            * (1.0 - BM25_B + BM25_B * document.length as f32 / average_length));
                *scores.entry(posting.doc).or_insert(0.0) +=
                    inverse_frequency * normalized * (BM25_K1 + 1.0);
            }
        }

        let mut ranked: Vec<(&str, f32)> = scores
            .into_iter()
            .filter_map(|(doc, score)| {
                let document = self.documents.get(doc as usize)?;
                if let Some(scope) = path_scope {
                    if !document.file_path.starts_with(scope) {
                        return None;
                    }
                }
                Some((document.chunk_id.as_str(), score))
            })
            .collect();
        // 分数相同时按 chunk ID 排序，保证结果确定——否则 HashMap 的遍历顺序会让
        // 同一查询在不同进程返回不同排名。
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
}

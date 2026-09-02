//! 历史会话片段的本地词法检索。
//!
//! 这里刻意**不用向量检索**。语料是用户自己的对话记录：查询词与正文里是同一批标识符、
//! 文件路径、报错串和命令名，正是精确词匹配最强的场合。插件同时索引会话标题、用户请求、
//! 最后助手回复、工具名和有界原文证据，以可审查的字段组合补足单一正文的召回范围。
//!
//! 何况当前平台也不具备条件：宿主没有嵌入接口，插件单次调用只有 5000 万 fuel、
//! 64 MiB 线性内存，在 WASM 里跑嵌入模型不可行。
//!
//! 检索分两段：先用子串命中做召回并顺带统计文档频率，再只对候选集算 BM25。
//! 这样扫描成本与语料线性相关且常数极小，排序成本只与候选数相关。

use std::collections::HashMap;

/// BM25 词频饱和参数。
const BM25_K1: f64 = 1.5;
/// BM25 文档长度归一化参数。
const BM25_B: f64 = 0.75;
/// 单次检索允许进入排序阶段的最大候选数。
const MAX_CANDIDATES: usize = 512;

/// 一条命中及其得分。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// 命中文档在输入切片中的下标。
    pub index: usize,
    /// BM25 得分，越大越相关。
    pub score: f64,
}

/// 判断字符是否属于需要按二元组切分的东亚文字。
///
/// 没有可用的分词器，因此中日韩文本统一按相邻字符二元组处理——这是 CJK 检索的
/// 成熟做法，能在不引入词典的前提下保住短语的区分度。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x3040..=0x30FF      // 日文假名
        | 0x3400..=0x4DBF    // 扩展 A
        | 0x4E00..=0x9FFF    // 基本汉字
        | 0xF900..=0xFAFF    // 兼容汉字
        | 0xAC00..=0xD7AF    // 谚文音节
    )
}

/// 把文本切成检索词。
///
/// ASCII 按字母数字连续段切分并转小写；CJK 按相邻字符二元组切分，单字成段时保留该字。
/// 其余字符只作分隔符，不进入结果。
pub fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut ascii = String::new();
    let mut cjk: Vec<char> = Vec::new();

    /// 收尾一段 ASCII 词。
    fn flush_ascii(buffer: &mut String, tokens: &mut Vec<String>) {
        if !buffer.is_empty() {
            tokens.push(std::mem::take(buffer));
        }
    }

    /// 收尾一段 CJK 文本，按二元组展开。
    fn flush_cjk(buffer: &mut Vec<char>, tokens: &mut Vec<String>) {
        match buffer.len() {
            0 => {}
            1 => tokens.push(buffer[0].to_string()),
            _ => {
                for pair in buffer.windows(2) {
                    tokens.push(pair.iter().collect());
                }
            }
        }
        buffer.clear();
    }

    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            flush_cjk(&mut cjk, &mut tokens);
            ascii.push(ch.to_ascii_lowercase());
        } else if is_cjk(ch) {
            flush_ascii(&mut ascii, &mut tokens);
            cjk.push(ch);
        } else {
            flush_ascii(&mut ascii, &mut tokens);
            flush_cjk(&mut cjk, &mut tokens);
        }
    }
    flush_ascii(&mut ascii, &mut tokens);
    flush_cjk(&mut cjk, &mut tokens);
    tokens
}

/// 统计 `needle` 在 `haystack` 中的出现次数。
///
/// 使用非重叠计数：重复字符串上会略微低估二元组的真实频率，但 BM25 的词频项本身
/// 就是饱和的，这点偏差不改变排序结论。
fn count_occurrences(haystack: &str, needle: &str) -> u32 {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count() as u32
}

/// 在文档集合上执行两段式检索。
///
/// `documents` 是每个历史片段拼接后的可检索文本，必须已经小写化 ASCII 部分——
/// 调用方用 [`searchable_text`] 构造即可。返回按得分降序、至多 `limit` 条的命中。
pub fn search(documents: &[String], query: &str, limit: usize) -> Vec<SearchHit> {
    let terms = dedup(tokenize(query));
    if terms.is_empty() || documents.is_empty() || limit == 0 {
        return Vec::new();
    }

    // 召回阶段：顺带得到精确的文档频率，无需额外一遍扫描。
    let mut document_frequency: HashMap<&str, u32> = HashMap::new();
    let mut candidates: Vec<(usize, Vec<u32>)> = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let counts: Vec<u32> = terms
            .iter()
            .map(|term| count_occurrences(document, term))
            .collect();
        if counts.iter().all(|count| *count == 0) {
            continue;
        }
        for (term, count) in terms.iter().zip(counts.iter()) {
            if *count > 0 {
                *document_frequency.entry(term.as_str()).or_insert(0) += 1;
            }
        }
        candidates.push((index, counts));
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
    }
    if candidates.is_empty() {
        return Vec::new();
    }

    let total = documents.len() as f64;
    let average_length = documents
        .iter()
        .map(|document| document.chars().count() as f64)
        .sum::<f64>()
        / total;

    let mut hits: Vec<SearchHit> = candidates
        .into_iter()
        .map(|(index, counts)| {
            let length = documents[index].chars().count() as f64;
            let normalization =
                BM25_K1 * (1.0 - BM25_B + BM25_B * length / average_length.max(1.0));
            let score = terms
                .iter()
                .zip(counts.iter())
                .filter(|(_, count)| **count > 0)
                .map(|(term, count)| {
                    let frequency = f64::from(document_frequency[term.as_str()]);
                    let idf = ((total - frequency + 0.5) / (frequency + 0.5) + 1.0).ln();
                    let tf = f64::from(*count);
                    idf * (tf * (BM25_K1 + 1.0)) / (tf + normalization)
                })
                .sum();
            SearchHit { index, score }
        })
        .collect();

    // 得分相同时按下标升序，保证同一语料下检索结果稳定可复现。
    hits.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.index.cmp(&right.index))
    });
    hits.truncate(limit);
    hits
}

/// 去重但保留首次出现顺序。
fn dedup(tokens: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    tokens
        .into_iter()
        .filter(|token| seen.insert(token.clone()))
        .collect()
}

/// 把历史片段的各字段拼成统一的可检索文本。
///
/// ASCII 统一转小写，与 [`tokenize`] 的处理保持一致，否则大小写不同的标识符会漏召回。
pub fn searchable_text(parts: &[&str]) -> String {
    parts.join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ASCII 按词切分并小写化，CJK 按二元组切分。
    #[test]
    fn tokenizes_ascii_words_and_cjk_bigrams() {
        assert_eq!(tokenize("Bun run build"), vec!["bun", "run", "build"]);
        assert_eq!(tokenize("构建命令"), vec!["构建", "建命", "命令"]);
        assert_eq!(tokenize("用 Bun 构建"), vec!["用", "bun", "构建"]);
    }

    /// 标点只作分隔符，不进入检索词。
    #[test]
    fn punctuation_only_separates() {
        assert_eq!(tokenize("a.b_c-d"), vec!["a", "b", "c", "d"]);
        assert!(tokenize("，。！").is_empty());
    }

    /// 精确标识符必须能命中——这正是选择词法检索而非向量的理由。
    #[test]
    fn exact_identifiers_rank_first() {
        let documents = vec![
            searchable_text(&["项目使用 Bun 构建"]),
            searchable_text(&["derive_plugin_state_identity 会把组件字节混入身份"]),
            searchable_text(&["终端界面主题配置"]),
        ];

        let hits = search(&documents, "derive_plugin_state_identity", 5);

        assert_eq!(hits[0].index, 1);
    }

    /// 中文查询能通过二元组命中。
    #[test]
    fn chinese_query_matches_by_bigram() {
        let documents = vec![
            searchable_text(&["终端主题与配色"]),
            searchable_text(&["构建命令是 bun run build"]),
        ];

        let hits = search(&documents, "构建命令", 5);

        assert_eq!(hits[0].index, 1);
    }

    /// 罕见词的权重必须高于普遍词，否则高频噪声会淹没真正的区分信号。
    #[test]
    fn rare_terms_outrank_common_terms() {
        let mut documents: Vec<String> = (0..20)
            .map(|_| searchable_text(&["插件 通用 说明"]))
            .collect();
        documents.push(searchable_text(&["插件 wasmtime 沙箱"]));

        let hits = search(&documents, "插件 wasmtime", 3);

        assert_eq!(hits[0].index, 20, "含罕见词的文档应排首位");
    }

    /// 无命中时返回空结果，而不是退化成返回全部文档。
    #[test]
    fn no_match_returns_empty() {
        let documents = vec![searchable_text(&["终端主题"])];

        assert!(search(&documents, "kubernetes", 5).is_empty());
        assert!(search(&documents, "", 5).is_empty());
    }

    /// 同分结果按下标排序，保证同一语料下检索稳定可复现。
    #[test]
    fn equal_scores_break_ties_deterministically() {
        let documents: Vec<String> = (0..5).map(|_| searchable_text(&["相同 内容"])).collect();

        let hits = search(&documents, "相同", 5);
        let indices: Vec<usize> = hits.iter().map(|hit| hit.index).collect();

        assert_eq!(indices, vec![0, 1, 2, 3, 4]);
    }

    /// 大小写不同的标识符必须仍能召回。
    #[test]
    fn matching_is_case_insensitive() {
        let documents = vec![searchable_text(&["调用 PluginHostApi 接口"])];

        assert!(!search(&documents, "pluginhostapi", 5).is_empty());
    }
}

//! 标识符拆词、中文处理与 FTS5 表达式构造。
//!
//! # 中文处理策略
//!
//! FTS5 内置的 `unicode61` 分词器会把一整段连续汉字当作单个 token，导致中文查询几乎无法命中。
//! 本模块在**索引期与查询期使用同一套规则**，把每个 CJK 字符拆成独立 token（字符间插入空格）。
//! 查询时把中文片段构造成 FTS5 短语（`"重 试 逻 辑"`），依靠位置相邻性实现子串级匹配。
//!
//! 已知限制：这是按字匹配而不是真正的中文分词，不做同义词或词形归一；
//! 单字查询会命中较多结果，需要靠 BM25 与 RRF 融合排序收敛。

use std::fmt::Write as _;

use crate::types::is_cjk;

/// 单个字段的拆词文本上限（字符），防止超大文件把 FTS 索引撑爆。
const MAX_FIELD_CHARS: usize = 200_000;

/// 写入 FTS5 表的四个字段。这里保存的是拆词文本，原始代码另存于 `file_revisions`。
#[derive(Debug, Clone, Default)]
pub struct FtsText {
    /// 路径及其拆分片段。
    pub path_terms: String,
    /// 符号原文及其拆分片段。
    pub symbol_terms: String,
    /// 文档注释（含中文按字拆分）。
    pub doc_terms: String,
    /// 代码中的标识符、字面量词与中文。
    pub code_terms: String,
}

/// 把标识符按 camelCase、PascalCase、snake_case、kebab-case 与数字边界拆分。
///
/// 返回的片段不含原标识符本身，且全部转为小写；调用方负责保留原始完整符号。
pub fn split_identifier(identifier: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let chars: Vec<char> = identifier.chars().collect();
    for (index, &ch) in chars.iter().enumerate() {
        if ch == '_' || ch == '-' || ch == '.' || ch == '/' || ch == ':' || ch == ' ' {
            push_part(&mut parts, &mut current);
            continue;
        }
        if !current.is_empty() {
            let previous = chars[index - 1];
            // 小写/数字后接大写：valueOf -> value | Of
            let lower_to_upper = ch.is_uppercase() && (previous.is_lowercase() || previous.is_numeric());
            // 连续大写后接小写：HTTPServer -> HTTP | Server
            let acronym_end = ch.is_lowercase()
                && previous.is_uppercase()
                && current.chars().count() > 1
                && current.chars().all(|c| c.is_uppercase() || c.is_numeric());
            if lower_to_upper {
                push_part(&mut parts, &mut current);
            } else if acronym_end {
                // 末位大写字母属于新词的首字母。
                let mut chars_iter: Vec<char> = current.chars().collect();
                let last = chars_iter.pop().unwrap_or_default();
                let head: String = chars_iter.into_iter().collect();
                if !head.is_empty() {
                    parts.push(head.to_lowercase());
                }
                current = last.to_string();
            }
        }
        current.push(ch);
    }
    push_part(&mut parts, &mut current);
    parts.retain(|part| !part.is_empty());
    parts
}

fn push_part(parts: &mut Vec<String>, current: &mut String) {
    if !current.is_empty() {
        parts.push(current.to_lowercase());
        current.clear();
    }
}

/// 把文本中的 CJK 字符用空格隔开，使 `unicode61` 为每个字生成独立 token。
pub fn space_cjk(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut previous_cjk = false;
    for ch in text.chars() {
        let current_cjk = is_cjk(ch);
        if current_cjk {
            if !out.is_empty() && !out.ends_with(' ') {
                out.push(' ');
            }
            out.push(ch);
            out.push(' ');
        } else {
            if previous_cjk && !ch.is_whitespace() && !out.ends_with(' ') {
                out.push(' ');
            }
            out.push(ch);
        }
        previous_cjk = current_cjk;
    }
    out
}

/// 从任意文本中提取用于检索的词条：标识符原文、拆分片段与 CJK 单字。
///
/// 保留重复出现，让 BM25 能够利用词频；总长度受 `MAX_FIELD_CHARS` 限制。
pub fn extract_terms(text: &str) -> String {
    let mut out = String::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() && !is_cjk(ch) || ch == '_' {
            current.push(ch);
            continue;
        }
        flush_word(&mut out, &mut current);
        if is_cjk(ch) {
            append_term(&mut out, &ch.to_string());
        }
        if out.chars().count() >= MAX_FIELD_CHARS {
            return out;
        }
    }
    flush_word(&mut out, &mut current);
    out
}

fn flush_word(out: &mut String, current: &mut String) {
    if current.is_empty() {
        return;
    }
    let word = std::mem::take(current);
    // 纯数字噪声大，只保留较长的数字串。
    if word.chars().all(|c| c.is_numeric()) && word.len() < 4 {
        return;
    }
    append_term(out, &word.to_lowercase());
    for part in split_identifier(&word) {
        if part != word.to_lowercase() {
            append_term(out, &part);
        }
    }
}

fn append_term(out: &mut String, term: &str) {
    if term.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(term);
}

/// 构造一个代码块的 FTS 拆词文本。
///
/// `path` 与 `symbols` 会同时保留原文和拆分片段，保证完整符号仍可精确命中。
/// `symbols` 依次接收限定符号、父作用域、模块路径与签名等字段，`None` 与空串会被跳过。
pub fn build_fts_text(
    path: &str,
    symbols: &[Option<&str>],
    doc: Option<&str>,
    code: &str,
) -> FtsText {
    let mut path_terms = String::new();
    append_term(&mut path_terms, &path.to_lowercase());
    if let Some(file_name) = path.rsplit('/').next() {
        append_term(&mut path_terms, &file_name.to_lowercase());
    }
    for segment in path.split('/') {
        append_term(&mut path_terms, &segment.to_lowercase());
        for part in split_identifier(segment) {
            append_term(&mut path_terms, &part);
        }
    }

    let mut symbol_terms = String::new();
    for field in symbols.iter().flatten() {
        if field.is_empty() {
            continue;
        }
        // 原始完整符号必须原样保留，否则完整符号查询会退化成拆词模糊匹配。
        append_term(&mut symbol_terms, &field.to_lowercase());
        // `:`/`.`/空白/泛型括号都视为符号分隔符，空片段由 `append_term` 跳过。
        for segment in field.split(|c: char| !c.is_alphanumeric() && c != '_') {
            append_term(&mut symbol_terms, &segment.to_lowercase());
            for part in split_identifier(segment) {
                append_term(&mut symbol_terms, &part);
            }
        }
    }

    let doc_terms = doc.map(extract_terms).unwrap_or_default();
    let code_terms = extract_terms(code);

    FtsText {
        path_terms: truncate_chars(&path_terms, MAX_FIELD_CHARS),
        symbol_terms: truncate_chars(&symbol_terms, MAX_FIELD_CHARS),
        doc_terms: truncate_chars(&doc_terms, MAX_FIELD_CHARS),
        code_terms: truncate_chars(&code_terms, MAX_FIELD_CHARS),
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    text.chars().take(limit).collect()
}

/// 把一个词条转义成 FTS5 字符串字面量（双引号内的 `"` 需要写成 `""`）。
///
/// 用户查询中的引号、括号、冒号等字符经此处理后只会被当作普通文本，
/// 不会破坏表达式语法，也无法注入额外的匹配逻辑。
pub fn quote_fts_term(term: &str) -> String {
    let mut out = String::with_capacity(term.len() + 2);
    out.push('"');
    for ch in term.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// 把一个查询片段转换成 FTS5 表达式项。
///
/// 含 CJK 的片段会被拆成按字相邻的短语；纯 ASCII 片段作为整体短语匹配。
/// 返回 `None` 表示该片段不含任何可检索内容。
pub fn fts_term_expression(fragment: &str) -> Option<String> {
    let has_cjk = fragment.chars().any(is_cjk);
    if has_cjk {
        let spaced = space_cjk(fragment);
        let cleaned: String = spaced
            .chars()
            .map(|c| if c.is_alphanumeric() || c.is_whitespace() { c } else { ' ' })
            .collect();
        let terms: Vec<&str> = cleaned.split_whitespace().collect();
        if terms.is_empty() {
            return None;
        }
        return Some(quote_fts_term(&terms.join(" ")));
    }
    let cleaned: String = fragment
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '_' { c } else { ' ' })
        .collect();
    let terms: Vec<&str> = cleaned.split_whitespace().collect();
    if terms.is_empty() {
        return None;
    }
    Some(quote_fts_term(&terms.join(" ").to_lowercase()))
}

/// 用 `OR` 连接多个查询片段，构造完整的 FTS5 MATCH 表达式。
///
/// 结果永远作为绑定参数传给 SQL，绝不拼接进语句文本。
pub fn build_match_expression(fragments: &[String]) -> Option<String> {
    let mut parts = Vec::new();
    for fragment in fragments {
        if let Some(expression) = fts_term_expression(fragment) {
            if !parts.contains(&expression) {
                parts.push(expression);
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (index, part) in parts.iter().enumerate() {
        if index > 0 {
            out.push_str(" OR ");
        }
        let _ = write!(out, "{part}");
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_common_identifier_styles() {
        assert_eq!(split_identifier("retry_policy"), vec!["retry", "policy"]);
        assert_eq!(split_identifier("RetryPolicy"), vec!["retry", "policy"]);
        assert_eq!(split_identifier("executeWithBackoff"), vec!["execute", "with", "backoff"]);
        assert_eq!(split_identifier("HTTPServer"), vec!["http", "server"]);
        assert_eq!(split_identifier("parseURL2Path"), vec!["parse", "url2", "path"]);
        assert_eq!(split_identifier("src/network/retry.rs"), vec!["src", "network", "retry", "rs"]);
        assert_eq!(split_identifier("simple"), vec!["simple"]);
    }

    #[test]
    fn cjk_characters_become_separate_tokens() {
        let spaced = space_cjk("重试逻辑retry");
        assert_eq!(spaced.split_whitespace().collect::<Vec<_>>(), vec!["重", "试", "逻", "辑", "retry"]);
    }

    #[test]
    fn fts_text_keeps_full_symbol_and_split_parts() {
        let text = build_fts_text(
            "src/network/retry.rs",
            &[Some("RetryPolicy::execute"), Some("network::retry")],
            Some("失败后重试"),
            "fn execute(&self) { let max_retries = 3; }",
        );
        let symbols: Vec<&str> = text.symbol_terms.split_whitespace().collect();
        assert!(symbols.contains(&"retrypolicy::execute"), "保留完整符号: {symbols:?}");
        assert!(symbols.contains(&"retrypolicy"));
        assert!(symbols.contains(&"retry"));
        assert!(symbols.contains(&"execute"));
        let paths: Vec<&str> = text.path_terms.split_whitespace().collect();
        assert!(paths.contains(&"src/network/retry.rs"));
        assert!(paths.contains(&"retry.rs"));
        let docs: Vec<&str> = text.doc_terms.split_whitespace().collect();
        assert!(docs.contains(&"重") && docs.contains(&"试"));
        let code: Vec<&str> = text.code_terms.split_whitespace().collect();
        assert!(code.contains(&"max_retries"));
        assert!(code.contains(&"retries"));
    }

    #[test]
    fn quoting_neutralizes_fts_syntax_characters() {
        assert_eq!(quote_fts_term("a\"b"), "\"a\"\"b\"");
        // 括号、冒号、星号等都不会破坏表达式。
        let expression = build_match_expression(&["failed to connect: (timeout)*".to_string()])
            .expect("表达式");
        assert_eq!(expression, "\"failed to connect timeout\"");
        assert!(build_match_expression(&["()".to_string()]).is_none());
    }

    #[test]
    fn chinese_fragment_becomes_adjacent_phrase() {
        let expression = build_match_expression(&["重试逻辑".to_string()]).expect("表达式");
        assert_eq!(expression, "\"重 试 逻 辑\"");
    }

    #[test]
    fn numeric_noise_is_filtered_but_long_numbers_kept() {
        let terms = extract_terms("let a = 12; let code = 40412;");
        let list: Vec<&str> = terms.split_whitespace().collect();
        assert!(!list.contains(&"12"));
        assert!(list.contains(&"40412"));
    }
}

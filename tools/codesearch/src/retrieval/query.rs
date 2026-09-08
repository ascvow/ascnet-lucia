//! 查询分析。
//!
//! 第一版使用规则，不调用 LLM 规划。识别完整符号、路径、字面量、自然语言、
//! 简单意图与内联作用域，输出供三路召回使用的 [`QueryPlan`]。
//!
//! # 中文查询
//!
//! 中文没有空格分隔，因此对每段连续汉字既保留整段（作为相邻短语），
//! 也生成二元片段（bigram）参与 `OR` 召回，避免整段短语过于严格而召回为空。
//! 同时剥离常见疑问词与语气助词。这不是真正的中文分词，限制记录在 README。

use crate::error::{Error, Result};
use crate::types::{is_cjk, Language, ScopeFilter};

use super::tokenize::split_identifier;

/// 单段连续汉字允许生成的最大二元片段数，避免长句产生过多召回词条。
const MAX_CJK_BIGRAMS: usize = 12;
/// 参与 FTS 的词条总数上限。
const MAX_TERMS: usize = 48;

/// 查询类别。一个查询可以同时属于多个类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QueryKind {
    /// 看起来是一个完整符号（无空格的标识符或限定名）。
    Symbol,
    /// 看起来是一个路径或文件名。
    Path,
    /// 引号包裹的内容或疑似错误消息。
    Literal,
    /// 自然语言描述。
    Natural,
}

impl QueryKind {
    /// 稳定的字符串标识（写入 JSON 输出）。
    pub fn as_str(&self) -> &'static str {
        match self {
            QueryKind::Symbol => "symbol",
            QueryKind::Path => "path",
            QueryKind::Literal => "literal",
            QueryKind::Natural => "natural",
        }
    }
}

/// 简单意图提示。只影响候选预算与轻微加权，不改变召回正确性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Intent {
    /// 想看实现。
    Implementation,
    /// 想看测试。
    Test,
    /// 想看定义或声明。
    Definition,
}

impl Intent {
    /// 稳定的字符串标识。
    pub fn as_str(&self) -> &'static str {
        match self {
            Intent::Implementation => "implementation",
            Intent::Test => "test",
            Intent::Definition => "definition",
        }
    }
}

/// 查询分析结果。
#[derive(Debug, Clone, Default)]
pub struct QueryPlan {
    /// 用户原始查询。
    pub raw: String,
    /// 剥离内联作用域与疑问词后的检索文本。
    pub text: String,
    /// 识别出的类别。
    pub kinds: Vec<QueryKind>,
    /// 用于符号精确召回的完整符号。
    pub symbols: Vec<String>,
    /// 用于路径召回的片段。
    pub paths: Vec<String>,
    /// 用于字面量子串召回的片段。
    pub literals: Vec<String>,
    /// 用于 FTS 召回的词条片段。
    pub terms: Vec<String>,
    /// 识别出的意图。
    pub intents: Vec<Intent>,
    /// 生效的作用域过滤（内联语法与显式参数合并后的结果）。
    pub scope: ScopeFilter,
}

impl QueryPlan {
    /// 是否存在完整符号形态的查询（用于精确命中保留规则）。
    pub fn has_exact_symbol(&self) -> bool {
        self.kinds.contains(&QueryKind::Symbol)
    }

    /// 供 JSON 输出的类别字符串。
    pub fn kind_names(&self) -> Vec<String> {
        self.kinds.iter().map(|k| k.as_str().to_string()).collect()
    }

    /// 供 JSON 输出的意图字符串。
    pub fn intent_names(&self) -> Vec<String> {
        self.intents
            .iter()
            .map(|i| i.as_str().to_string())
            .collect()
    }
}

/// 中文疑问词与语气助词。仅剥离明确的提问框架，不做同义词扩展。
const CJK_STOP_PHRASES: &[&str] = &[
    "在哪里实现",
    "在哪里定义",
    "在哪里",
    "在哪儿",
    "在哪",
    "哪里",
    "哪个文件",
    "哪个",
    "是什么",
    "为什么",
    "怎么样",
    "怎么做",
    "怎么写",
    "怎么",
    "怎样",
    "如何",
    "请问",
    "帮我找",
    "帮我",
    "找一下",
    "找出",
    "查一下",
    "看一下",
    "告诉我",
];

/// 英文意图关键词。
const IMPL_WORDS: &[&str] = &["implementation", "implement", "implements", "impl"];
const TEST_WORDS: &[&str] = &["test", "tests", "spec", "specs", "testcase"];
const DEF_WORDS: &[&str] = &["definition", "define", "defined", "declaration", "declare"];

/// 分析查询。
///
/// `explicit` 是来自 CLI 参数或 MCP 入参的作用域，与查询内联语法合并；
/// 两者冲突时都会生效（取交集语义：include 累加、exclude 累加）。
pub fn analyze(raw: &str, explicit: &ScopeFilter) -> Result<QueryPlan> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidQuery("查询不能为空".to_string()));
    }

    let mut scope = explicit.clone();
    let mut literals: Vec<String> = Vec::new();

    // 1. 取出引号包裹的字面量，并从文本中移除，避免其内部空格干扰后续分词。
    let (without_quotes, quoted) = extract_quoted(trimmed);
    for literal in quoted {
        push_unique(&mut literals, literal);
    }

    // 2. 解析内联作用域并剥离。
    let mut remaining: Vec<&str> = Vec::new();
    for token in without_quotes.split_whitespace() {
        if let Some(rest) = parse_scope_token(token, &mut scope)? {
            remaining.push(rest);
        }
    }
    let text_with_stop = remaining.join(" ");

    // 3. 剥离中文疑问框架。
    let text = strip_cjk_stop_phrases(&text_with_stop);
    let text = text.trim().to_string();
    let effective = if text.is_empty() {
        text_with_stop.clone()
    } else {
        text
    };

    let mut plan = QueryPlan {
        raw: raw.to_string(),
        text: effective.clone(),
        scope,
        literals,
        ..Default::default()
    };

    // 4. 逐 token 判定符号 / 路径。
    let tokens: Vec<&str> = effective.split_whitespace().collect();
    for token in &tokens {
        // Elixir/Ruby 的 ?/! 是符号的一部分。只对明确语言或独立标识符保留原拼写，
        // 自然语言句末标点仍按原规则清洗，避免扩大普通问句的精确召回预算。
        let spelling = token.trim_matches([',', '，', '。', '？', '；', ';']);
        let suffix = spelling.ends_with(['?', '!']);
        let stem = spelling.trim_end_matches(['?', '!']);
        let language_suffix = plan
            .scope
            .languages
            .iter()
            .any(|language| matches!(language, Language::Elixir | Language::Ruby));
        if suffix
            && (language_suffix || tokens.len() == 1)
            && spelling.len() == stem.len() + 1
            && stem.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
            && stem
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':'))
        {
            push_unique(&mut plan.symbols, spelling.to_string());
            continue;
        }
        let cleaned = token
            .trim_matches(|c: char| matches!(c, ',' | '，' | '。' | '?' | '？' | '!' | '；' | ';'));
        if cleaned.is_empty() {
            continue;
        }
        if looks_like_path(cleaned) {
            push_unique(&mut plan.paths, cleaned.to_string());
        }
        if looks_like_symbol(cleaned) {
            push_unique(&mut plan.symbols, cleaned.to_string());
        }
    }

    // 5. 疑似错误消息：多个词且含典型标点，把整句也作为字面量候选。
    if plan.literals.is_empty() && looks_like_error_message(&effective) {
        push_unique(&mut plan.literals, effective.clone());
    }

    // 6. 意图。
    detect_intents(&effective, &mut plan.intents);

    // 7. 类别。
    if !plan.symbols.is_empty() {
        plan.kinds.push(QueryKind::Symbol);
    }
    if !plan.paths.is_empty() {
        plan.kinds.push(QueryKind::Path);
    }
    if !plan.literals.is_empty() {
        plan.kinds.push(QueryKind::Literal);
    }
    if is_natural_language(&effective) {
        plan.kinds.push(QueryKind::Natural);
    }
    if plan.kinds.is_empty() {
        plan.kinds.push(QueryKind::Natural);
    }
    plan.kinds.sort_unstable();
    plan.kinds.dedup();

    // 8. FTS 词条。
    plan.terms = build_terms(&effective, &plan.literals);

    Ok(plan)
}

/// 取出所有双引号或单引号包裹的片段。
fn extract_quoted(text: &str) -> (String, Vec<String>) {
    let mut quoted = Vec::new();
    let mut rest = String::with_capacity(text.len());
    let mut current = String::new();
    let mut delimiter: Option<char> = None;
    for ch in text.chars() {
        match delimiter {
            Some(open) if ch == open => {
                if !current.trim().is_empty() {
                    quoted.push(current.trim().to_string());
                }
                current.clear();
                delimiter = None;
            }
            Some(_) => current.push(ch),
            None if ch == '"' || ch == '\'' || ch == '「' => {
                delimiter = Some(if ch == '「' { '」' } else { ch });
            }
            None => rest.push(ch),
        }
    }
    // 引号未闭合时按普通文本处理，不报错。
    if delimiter.is_some() && !current.is_empty() {
        rest.push_str(&current);
    }
    (rest, quoted)
}

/// 解析 `lang:`/`path:`/`-path:` 等内联作用域。
///
/// 返回 `Ok(None)` 表示该 token 已被作用域消费。
fn parse_scope_token<'a>(token: &'a str, scope: &mut ScopeFilter) -> Result<Option<&'a str>> {
    let (negated, body) = match token.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, token),
    };
    let Some((key, value)) = body.split_once(':') else {
        return Ok(Some(token));
    };
    if value.is_empty() {
        return Ok(Some(token));
    }
    match key.to_ascii_lowercase().as_str() {
        "lang" | "language" => {
            let language = Language::parse_filter(value)?;
            if !scope.languages.contains(&language) {
                scope.languages.push(language);
            }
            Ok(None)
        }
        "path" | "file" | "in" => {
            // 未写通配符时按"包含该片段"处理，符合直觉。
            let pattern = if value.contains('*') {
                value.to_string()
            } else {
                format!("**/*{value}*")
            };
            if negated {
                scope.exclude.push(pattern);
            } else {
                scope.include.push(pattern);
            }
            Ok(None)
        }
        _ => Ok(Some(token)),
    }
}

/// 剥离中文疑问框架。只在剩余内容非空时生效。
fn strip_cjk_stop_phrases(text: &str) -> String {
    let mut out = text.to_string();
    for phrase in CJK_STOP_PHRASES {
        if out.contains(phrase) {
            let candidate = out.replace(phrase, " ");
            if candidate.chars().any(|c| !c.is_whitespace()) {
                out = candidate;
            }
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 是否形似路径：含 `/` 或以已知源码扩展名结尾。
pub fn looks_like_path(token: &str) -> bool {
    if token.contains('/') && !token.starts_with("//") {
        return true;
    }
    let Some((stem, ext)) = token.rsplit_once('.') else {
        return false;
    };
    if stem.is_empty() || ext.is_empty() {
        return false;
    }
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "rs" | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "mjs"
            | "cjs"
            | "mts"
            | "cts"
            | "md"
            | "toml"
            | "json"
            | "txt"
            | "yaml"
            | "yml"
    )
}

/// 各语言用于分隔限定名层级的符号。
const SYMBOL_SEPARATORS: &[&str] = &["::", "->", "\\", ".", "#"];

/// 为限定名生成换用其他分隔符的等价写法。
///
/// 每种语言的层级分隔符都不一样：Rust 与 C++ 用 `::`，Java 与 Go 用 `.`，
/// PHP 命名空间用 `\`，Ruby 实例方法习惯写 `#`。使用者不该被要求先想清楚
/// 「这个语言用哪个分隔符」再搜索——查 `Token.check` 与查 `Token::check`
/// 指的是同一个东西，都应该命中。
///
/// 只替换分隔符，不改动标识符本身，因此不会把 `a.b` 变成无关符号。
/// 单段标识符（不含任何分隔符）不产生变体。
///
/// 变体属于"怎么查"，不属于"用户要找什么"，因此不写进 `QueryPlan::symbols`，
/// 而是由召回阶段按需展开。
pub(crate) fn separator_variants(token: &str) -> Vec<String> {
    let Some(found) = SYMBOL_SEPARATORS
        .iter()
        .find(|separator| token.contains(**separator))
    else {
        return Vec::new();
    };
    // 形如 `../foo` 或 `1.5` 的片段拆出来是空段，不当作限定名处理。
    let segments: Vec<&str> = token.split(found).collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Vec::new();
    }
    SYMBOL_SEPARATORS
        .iter()
        .filter(|separator| *separator != found)
        .map(|separator| segments.join(separator))
        .collect()
}

/// 是否形似完整符号：无空格的标识符、限定名或 `Type::method` / `Obj.method`。
pub fn looks_like_symbol(token: &str) -> bool {
    if token.len() < 2 || token.contains(char::is_whitespace) {
        return false;
    }
    if looks_like_path(token) {
        return false;
    }
    let mut chars = token.chars();
    let first = chars.next().unwrap_or_default();
    if !(first.is_alphabetic() || first == '_') {
        return false;
    }
    let body_ok = token
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == ':' || c == '.' || c == '#');
    if !body_ok {
        return false;
    }
    // 不含分隔符的单个英文词，只有在形似 CamelCase 或足够长时才当作完整符号，
    // 否则 "retry" 这类普通词会挤占精确召回预算。
    let has_separator = token.contains("::") || token.contains('.') || token.contains('_');
    // 必须**同时**有大小写才算 CamelCase：`API`、`JSON`、`HTTP` 这类全大写缩写
    // 在自然语言提问里出现得远比符号引用频繁，一旦被当成完整符号精确命中，
    // 就会凭 exact_reserve 挤掉真正相关的结果（例如 "API Key 是怎么读取的"
    // 会让 `api` 模块排到第一）。全大写常量通常带下划线，已由 has_separator 覆盖。
    let has_case_mix =
        token.chars().any(|c| c.is_uppercase()) && token.chars().any(|c| c.is_lowercase());
    has_separator || has_case_mix || token.len() >= 12
}

/// 是否疑似错误消息：含典型标点且不止一个词。
fn looks_like_error_message(text: &str) -> bool {
    let word_count = text.split_whitespace().count();
    if word_count < 2 {
        return false;
    }
    let has_marker = text.contains(':')
        || text.contains("Error")
        || text.contains("error")
        || text.contains("failed")
        || text.contains("panic")
        || text.contains("exception")
        || text.contains("Exception");
    has_marker && !text.contains("::")
}

fn is_natural_language(text: &str) -> bool {
    text.split_whitespace().count() > 1 || text.chars().any(is_cjk)
}

fn detect_intents(text: &str, intents: &mut Vec<Intent>) {
    let lower = text.to_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_alphanumeric()).collect();
    let has = |list: &[&str]| words.iter().any(|word| list.contains(word));
    if has(IMPL_WORDS) || text.contains("实现") {
        intents.push(Intent::Implementation);
    }
    if has(TEST_WORDS) || text.contains("测试") {
        intents.push(Intent::Test);
    }
    if has(DEF_WORDS) || text.contains("定义") || text.contains("声明") {
        intents.push(Intent::Definition);
    }
    intents.sort_unstable();
    intents.dedup();
}

/// 构造 FTS 词条片段。
///
/// 包含：原始 token、标识符拆分片段、连续汉字整段与其二元片段、字面量整体。
fn build_terms(text: &str, literals: &[String]) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for literal in literals {
        push_unique(&mut terms, literal.clone());
    }
    for token in text.split_whitespace() {
        let cleaned: String = token
            .chars()
            .filter(|c| {
                c.is_alphanumeric()
                    || *c == '_'
                    || *c == ':'
                    || *c == '.'
                    || *c == '/'
                    || is_cjk(*c)
            })
            .collect();
        if cleaned.is_empty() {
            continue;
        }
        if cleaned.chars().any(is_cjk) {
            for run in cjk_runs(&cleaned) {
                push_unique(&mut terms, run);
            }
            // 汉字与 ASCII 混排时，ASCII 部分单独作为词条。
            for ascii in cleaned.split(|c: char| is_cjk(c)) {
                if ascii.len() >= 2 {
                    push_unique(&mut terms, ascii.to_string());
                }
            }
            continue;
        }
        push_unique(&mut terms, cleaned.clone());
        for part in split_identifier(&cleaned) {
            if part.chars().count() >= 2 && part != cleaned.to_lowercase() {
                push_unique(&mut terms, part);
            }
        }
    }
    terms.truncate(MAX_TERMS);
    terms
}

/// 取出连续汉字整段及其二元片段。
fn cjk_runs(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut run: Vec<char> = Vec::new();
    let flush = |run: &mut Vec<char>, out: &mut Vec<String>| {
        if run.is_empty() {
            return;
        }
        let whole: String = run.iter().collect();
        // 整段作为相邻短语，精确但严格。
        if run.len() >= 2 {
            out.push(whole);
        } else {
            out.push(whole);
            run.clear();
            return;
        }
        // 二元片段提供部分匹配能力。
        for window in run.windows(2).take(MAX_CJK_BIGRAMS) {
            out.push(window.iter().collect());
        }
        run.clear();
    };
    for ch in text.chars() {
        if is_cjk(ch) {
            run.push(ch);
        } else {
            flush(&mut run, &mut out);
        }
    }
    flush(&mut run, &mut out);
    out
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if value.is_empty() {
        return;
    }
    if !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn analyze_plain(query: &str) -> QueryPlan {
        analyze(query, &ScopeFilter::default()).expect("分析成功")
    }

    #[test]
    fn empty_query_is_rejected() {
        assert!(analyze("   ", &ScopeFilter::default()).is_err());
    }

    /// Ruby/Elixir 的合法后缀不得被改写成另一个函数名，也不能改变普通问句的符号集合。
    #[test]
    fn predicate_and_bang_symbols_keep_their_suffix() {
        for symbol in [
            "Portal.Session.valid?",
            "valid?",
            "User.save!",
            "save!",
            "Store::empty?",
        ] {
            assert_eq!(analyze_plain(symbol).symbols, [symbol]);
        }
        assert_eq!(
            analyze_plain("Where is RetryPolicy implemented?").symbols,
            analyze_plain("Where is RetryPolicy implemented").symbols
        );
        assert!(analyze_plain("123?").symbols.is_empty());
        assert!(analyze_plain("如何验证?").symbols.is_empty());
    }

    #[test]
    fn full_symbol_is_recognized() {
        let plan = analyze_plain("RetryPolicy::execute");
        assert!(plan.has_exact_symbol());
        assert_eq!(plan.symbols, vec!["RetryPolicy::execute"]);
        // 拆词结果同时进入 FTS 词条，但完整符号必须保留。
        assert!(plan.terms.contains(&"RetryPolicy::execute".to_string()));
        assert!(plan.terms.iter().any(|t| t == "retry"));
    }

    /// 限定名要能跨分隔符命中。
    ///
    /// 回归：PHP 的 `Token::check` 在索引里曾按命名空间约定存成 `Token\check`，
    /// 用调用点的写法 `Token::check` 去搜，符号通路完全不触发，正确答案掉到第 2，
    /// 被一个 vendored 大文本块压在下面。
    #[test]
    fn qualified_names_expand_to_other_separators() {
        for query in ["Token::check", "Token.check"] {
            let variants = separator_variants(query);
            for expected in ["Token::check", "Token.check", "Token\\check", "Token#check"] {
                let found = query == expected || variants.iter().any(|v| v == expected);
                assert!(found, "`{query}` 应能查到 `{expected}`: {variants:?}");
            }
        }
    }

    /// 分隔符变体不得把普通词或路径变成无关符号。
    #[test]
    fn separator_variants_do_not_invent_symbols() {
        // 单段标识符没有层级，不产生变体。
        assert!(separator_variants("execute").is_empty());
        // 空段说明不是限定名。
        assert!(separator_variants("../utils").is_empty());
        assert!(separator_variants("a..b").is_empty());
        // 正常限定名只换分隔符，段本身不变。
        let variants = separator_variants("Auth::login");
        assert!(variants
            .iter()
            .all(|v| v.contains("Auth") && v.contains("login")));
        assert!(
            !variants.contains(&"Auth::login".to_string()),
            "不重复原写法"
        );
    }

    #[test]
    fn snake_and_camel_identifiers_are_symbols() {
        assert!(analyze_plain("execute_with_backoff").has_exact_symbol());
        assert!(analyze_plain("executeWithBackoff").has_exact_symbol());
        // 普通短英文单词不占用精确召回。
        assert!(!analyze_plain("retry").has_exact_symbol());
    }

    /// 全大写缩写在自然语言提问里是普通词汇，不能占用精确召回预算。
    ///
    /// 真实查询 "API Key 是怎么读取的" 曾因把 `API` 当成完整符号，
    /// 让 `api` 模块凭精确命中排到第一，挤掉了 `read_api_key`。
    #[test]
    fn uppercase_acronyms_are_not_treated_as_full_symbols() {
        for (query, acronym) in [
            ("API Key 是怎么读取的", "API"),
            ("HTTP 请求怎么重试", "HTTP"),
            ("JSON 解析在哪", "JSON"),
            ("SQL 语句写在哪个文件", "SQL"),
        ] {
            let plan = analyze_plain(query);
            assert!(
                !plan.symbols.iter().any(|symbol| symbol == acronym),
                "查询 `{query}` 不应把 `{acronym}` 当成完整符号: {:?}",
                plan.symbols
            );
            // 缩写仍然参与 FTS 召回，只是不再享有精确命中的保留额度。
            assert!(
                plan.terms
                    .iter()
                    .any(|term| term.eq_ignore_ascii_case(acronym)),
                "`{acronym}` 应仍进入 FTS 词条: {:?}",
                plan.terms
            );
        }
        // 单个纯缩写查询也不再是精确符号命中。
        assert!(!analyze_plain("JSON").has_exact_symbol());

        // 全大写常量通常带下划线，仍按完整符号处理。
        assert!(analyze_plain("MAX_BACKOFF").has_exact_symbol());
        assert!(analyze_plain("HttpClient").has_exact_symbol());
        assert!(analyze_plain("http::HttpProvider").has_exact_symbol());
    }

    #[test]
    fn path_queries_are_recognized() {
        let plan = analyze_plain("src/network/retry.rs");
        assert_eq!(plan.paths, vec!["src/network/retry.rs"]);
        assert!(plan.kinds.contains(&QueryKind::Path));
        assert!(!plan.has_exact_symbol());
        assert_eq!(analyze_plain("retry.ts").paths, vec!["retry.ts"]);
    }

    #[test]
    fn quoted_literals_are_extracted_verbatim() {
        let plan = analyze_plain("错误 \"connection refused: timeout\" 在哪里");
        assert!(plan
            .literals
            .contains(&"connection refused: timeout".to_string()));
        assert!(plan.kinds.contains(&QueryKind::Literal));
        // 引号内容原样进入词条，特殊字符由 FTS 转义层处理。
        assert!(plan
            .terms
            .contains(&"connection refused: timeout".to_string()));
    }

    #[test]
    fn error_message_without_quotes_becomes_literal() {
        let plan = analyze_plain("failed to connect: timeout");
        assert!(plan
            .literals
            .contains(&"failed to connect: timeout".to_string()));
    }

    #[test]
    fn inline_scope_is_parsed_and_stripped() {
        let plan = analyze_plain("lang:rust path:src/net retry");
        assert_eq!(plan.scope.languages, vec![Language::Rust]);
        assert_eq!(plan.scope.include, vec!["**/*src/net*"]);
        assert_eq!(plan.text, "retry");
        // 作用域 token 不应污染检索词条。
        assert!(!plan.terms.iter().any(|t| t.contains("lang")));
    }

    #[test]
    fn negated_path_scope_becomes_exclude() {
        let plan = analyze_plain("-path:tests retry");
        assert_eq!(plan.scope.exclude, vec!["**/*tests*"]);
        assert_eq!(plan.text, "retry");
    }

    #[test]
    fn invalid_language_scope_is_an_error() {
        let error = analyze("lang:cobol x", &ScopeFilter::default()).expect_err("应报错");
        assert!(matches!(error, Error::InvalidQuery(_)));
    }

    #[test]
    fn chinese_query_strips_question_words_and_builds_bigrams() {
        let plan = analyze_plain("请求失败后的重试逻辑在哪里");
        // 疑问词被剥离。
        assert!(!plan.text.contains("在哪里"));
        assert!(plan.text.contains("重试逻辑"));
        // 整段与二元片段都参与召回。
        assert!(plan.terms.iter().any(|t| t == "请求失败后的重试逻辑"));
        assert!(plan.terms.iter().any(|t| t == "重试"));
        assert!(plan.kinds.contains(&QueryKind::Natural));
    }

    #[test]
    fn stripping_never_empties_the_query() {
        // 整句都是疑问词时不能把查询清空。
        let plan = analyze_plain("在哪里");
        assert!(!plan.text.is_empty());
        assert!(!plan.terms.is_empty());
    }

    #[test]
    fn mixed_chinese_and_ascii_keeps_both() {
        let plan = analyze_plain("HTTP重试逻辑");
        assert!(plan.terms.iter().any(|t| t == "HTTP"));
        assert!(plan.terms.iter().any(|t| t == "重试逻辑"));
    }

    #[test]
    fn intents_are_detected_in_both_languages() {
        assert!(analyze_plain("retry implementation")
            .intents
            .contains(&Intent::Implementation));
        assert!(analyze_plain("重试的实现")
            .intents
            .contains(&Intent::Implementation));
        assert!(analyze_plain("retry test").intents.contains(&Intent::Test));
        assert!(analyze_plain("重试的测试").intents.contains(&Intent::Test));
        assert!(analyze_plain("RetryPolicy definition")
            .intents
            .contains(&Intent::Definition));
    }

    #[test]
    fn explicit_scope_is_merged_with_inline_scope() {
        let explicit = ScopeFilter {
            include: vec!["src/**".to_string()],
            exclude: Vec::new(),
            languages: vec![Language::TypeScript],
        };
        let plan = analyze("lang:rust retry", &explicit).expect("分析");
        assert_eq!(plan.scope.include, vec!["src/**"]);
        assert!(plan.scope.languages.contains(&Language::TypeScript));
        assert!(plan.scope.languages.contains(&Language::Rust));
    }
}

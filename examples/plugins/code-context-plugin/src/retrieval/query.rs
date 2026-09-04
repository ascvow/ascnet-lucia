//! 低成本查询分析与检索计划。
//!
//! 本模块只使用字符形态、分隔符与少量意图词，不调用 LLM 或网络。分析结果同时驱动召回
//! 通道、候选配额、角色排序与条件重排，避免各层分别维护一套互相漂移的查询判断。

/// 查询的表面形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryIntent {
    /// 单独的标识符，例如 `CodeSearchRequest` 或 `split_camel_case`。
    Identifier,
    /// 单独的限定符号，例如 `RetrievalIndex::remove_chunks` 或 `foo.bar`。
    QualifiedSymbol,
    /// 不含明确代码标识符的自然语言描述。
    NaturalLanguage,
    /// 自然语言中夹带一个或多个代码标识符。
    Mixed,
}

impl QueryIntent {
    /// 返回稳定的机器可读名称，供工具诊断与 benchmark 报告使用。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identifier => "identifier",
            Self::QualifiedSymbol => "qualified_symbol",
            Self::NaturalLanguage => "natural_language",
            Self::Mixed => "mixed",
        }
    }
}

/// 查询希望定位的代码角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryGoal {
    /// 查找声明或定义位置。
    Definition,
    /// 查找可执行实现正文。
    Implementation,
    /// 查找调用或使用位置；当前没有 references graph，只能调整已有候选角色。
    CallSite,
    /// 查找结构体、类、枚举或类型别名。
    TypeLookup,
    /// 查找一项行为的执行逻辑。
    Behavior,
    /// 查找概念、约束或说明。
    Concept,
}

impl QueryGoal {
    /// 返回稳定的机器可读名称，供工具诊断与 benchmark 报告使用。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Implementation => "implementation",
            Self::CallSite => "call_site",
            Self::TypeLookup => "type_lookup",
            Self::Behavior => "behavior",
            Self::Concept => "concept",
        }
    }
}

/// 一次确定性的查询分析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzedQuery {
    /// 查询表面形态。
    pub intent: QueryIntent,
    /// 查询希望定位的代码角色。
    pub goal: QueryGoal,
    /// 从自然语言中提取的标识符，保持出现顺序并去重。
    pub identifiers: Vec<String>,
    /// 查询是否以中日韩字符为主；用于调整词法与语义通道权重。
    pub cjk_dominant: bool,
}

impl AnalyzedQuery {
    /// 使用字符形态和通用意图词分析查询。
    ///
    /// 空白与标点会被保守处理；无法确认标识符时归入自然语言，避免普通英文单词获得精确
    /// 符号保护。该方法不访问索引，因此同一查询在不同仓库上得到相同结果。
    pub fn analyze(query: &str) -> Self {
        let trimmed = query.trim();
        let standalone = is_standalone_symbol(trimmed);
        let identifiers = if standalone {
            vec![trimmed.to_string()]
        } else {
            extract_identifiers(trimmed)
        };
        let intent = if standalone && has_qualifier(trimmed) {
            QueryIntent::QualifiedSymbol
        } else if standalone {
            QueryIntent::Identifier
        } else if identifiers.is_empty() {
            QueryIntent::NaturalLanguage
        } else {
            QueryIntent::Mixed
        };
        Self {
            intent,
            goal: classify_goal(trimmed),
            identifiers,
            cjk_dominant: cjk_dominant(trimmed),
        }
    }
}

/// 一次查询实际采用的召回计划。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchPlan {
    /// 是否执行符号召回。
    pub enable_symbol: bool,
    /// 是否执行词法召回。
    pub enable_lexical: bool,
    /// 是否执行语义召回。
    pub enable_semantic: bool,
    /// 符号通道候选上限。
    pub symbol_limit: usize,
    /// 词法通道候选上限。
    pub lexical_limit: usize,
    /// 语义通道候选上限。
    pub semantic_limit: usize,
    /// 融合后交给可选模型重排的候选池上限。
    pub candidate_pool_limit: usize,
    /// 查询形态是否允许进入模型重排；最终是否调用仍由置信度判断。
    pub allow_rerank: bool,
}

impl SearchPlan {
    /// 根据查询形态、语义能力和模型配置生成检索计划。
    ///
    /// `result_limit` 是最终期望数量。所有通道配额均有 200 条上限，重排候选池有 48 条上限；
    /// 请求本身超过上限时仍以请求值为准，避免返回数少于调用方要求。
    pub fn for_query(
        query: &AnalyzedQuery,
        result_limit: usize,
        semantic_available: bool,
        rerank_configured: bool,
    ) -> Self {
        let expanded = |multiple: usize| {
            result_limit
                .saturating_mul(multiple)
                .min(200usize.max(result_limit))
        };
        let rerank_allowed = rerank_configured
            && matches!(
                query.intent,
                QueryIntent::NaturalLanguage | QueryIntent::Mixed
            );
        let candidate_pool_limit = if rerank_allowed {
            result_limit
                .saturating_mul(4)
                .min(48usize.max(result_limit))
        } else {
            result_limit
        };
        let (symbol_limit, lexical_limit, semantic_limit) = match query.intent {
            QueryIntent::Identifier | QueryIntent::QualifiedSymbol => (expanded(4), expanded(2), 0),
            QueryIntent::NaturalLanguage => (expanded(2), expanded(4), expanded(4)),
            QueryIntent::Mixed => (expanded(4), expanded(4), expanded(4)),
        };
        Self {
            enable_symbol: result_limit > 0,
            enable_lexical: result_limit > 0,
            enable_semantic: result_limit > 0 && semantic_available && semantic_limit > 0,
            symbol_limit,
            lexical_limit,
            semantic_limit,
            candidate_pool_limit,
            allow_rerank: rerank_allowed,
        }
    }
}

/// 判断整条查询是否是单独的标识符或限定符号。
fn is_standalone_symbol(query: &str) -> bool {
    !query.is_empty()
        && !query.chars().any(char::is_whitespace)
        && query
            .chars()
            .any(|character| character.is_ascii_alphabetic())
        && query.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '_' | ':' | '.' | '#' | '<' | '>')
        })
}

/// 判断符号是否含成员或命名空间限定符。
fn has_qualifier(symbol: &str) -> bool {
    symbol.contains("::") || symbol.contains('.') || symbol.contains('#')
}

/// 从自然语言中提取具备明确代码形态的标识符。
fn extract_identifiers(query: &str) -> Vec<String> {
    let mut identifiers = Vec::new();
    for word in query.split_whitespace() {
        let token = word.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric()
                && !matches!(character, '_' | ':' | '.' | '#' | '<' | '>')
        });
        if !is_identifier_shaped(token) || identifiers.iter().any(|item| item == token) {
            continue;
        }
        identifiers.push(token.to_string());
    }
    identifiers
}

/// 判断一个自然语言词是否具备足够强的代码标识符形态。
fn is_identifier_shaped(token: &str) -> bool {
    if token.chars().count() < 2
        || !token
            .chars()
            .any(|character| character.is_ascii_alphabetic())
    {
        return false;
    }
    if token.contains('_') || has_qualifier(token) {
        return true;
    }
    let has_lower = token
        .chars()
        .any(|character| character.is_ascii_lowercase());
    let upper_after_first = token
        .chars()
        .skip(1)
        .filter(|character| character.is_ascii_uppercase())
        .count();
    has_lower && upper_after_first > 0
}

/// 判断查询是否以中日韩文字为主。
fn cjk_dominant(query: &str) -> bool {
    let letters = query
        .chars()
        .filter(|character| character.is_alphanumeric())
        .count();
    let cjk = query
        .chars()
        .filter(|character| super::tokenize::is_cjk(*character))
        .count();
    letters > 0 && cjk.saturating_mul(2) >= letters
}

/// 使用通用行为词判断查询目标，不绑定具体仓库路径或 benchmark 查询。
fn classify_goal(query: &str) -> QueryGoal {
    let lower = query.to_ascii_lowercase();
    if contains_any(
        &lower,
        &[
            "谁调用",
            "谁在调用",
            "在哪里使用",
            "在哪使用",
            "引用处",
            "call site",
            "usage",
        ],
    ) {
        return QueryGoal::CallSite;
    }
    if contains_any(
        &lower,
        &[
            "实体",
            "类型",
            "结构体",
            "类定义",
            "枚举",
            "type lookup",
            "struct",
            "class",
            "enum",
        ],
    ) {
        return QueryGoal::TypeLookup;
    }
    if contains_any(
        &lower,
        &[
            "在哪里定义",
            "在哪定义",
            "定义在哪",
            "声明在哪",
            "where defined",
            "definition",
        ],
    ) {
        return QueryGoal::Definition;
    }
    if contains_any(
        &lower,
        &[
            "怎么实现",
            "如何实现",
            "实现在哪",
            "接口实现",
            "怎么绘制",
            "如何绘制",
            "怎么渲染",
            "如何渲染",
            "怎么执行",
            "如何执行",
            "怎么发布",
            "如何发布",
            "怎么刷新",
            "如何刷新",
            "怎么打包",
            "如何打包",
            "implementation",
            "implement",
            "handler",
            "controller",
        ],
    ) {
        return QueryGoal::Implementation;
    }
    if contains_any(
        &lower,
        &[
            "是什么",
            "概念",
            "含义",
            "说明文档",
            "设计说明",
            "concept",
            "explain",
        ],
    ) {
        return QueryGoal::Concept;
    }
    QueryGoal::Behavior
}

/// 判断文本是否包含任意一个固定意图词。
fn contains_any(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四类查询形态必须按确定性字符规则区分。
    #[test]
    fn classifies_query_intents() {
        assert_eq!(
            AnalyzedQuery::analyze("CodeSearchRequest").intent,
            QueryIntent::Identifier
        );
        assert_eq!(
            AnalyzedQuery::analyze("RetrievalIndex::remove_chunks").intent,
            QueryIntent::QualifiedSymbol
        );
        assert_eq!(
            AnalyzedQuery::analyze("工具调用在什么情况下需要用户审批").intent,
            QueryIntent::NaturalLanguage
        );
        assert_eq!(
            AnalyzedQuery::analyze("CodeSearchRequest 是在哪里转换成代码片段的").intent,
            QueryIntent::Mixed
        );
    }

    /// 普通英文单词与句首大写词不能被误判为混合查询。
    #[test]
    fn plain_words_are_not_identifiers_inside_natural_language() {
        for query in [
            "RRF 融合",
            "HTTP 客户端",
            "Session 是怎么保存的",
            "permission flow",
        ] {
            assert_eq!(
                AnalyzedQuery::analyze(query).intent,
                QueryIntent::NaturalLanguage,
                "`{query}` 不应被识别为混合查询"
            );
        }
    }

    /// 通用行为词必须区分定义、实现、调用、类型与概念目标。
    #[test]
    fn classifies_query_goals() {
        assert_eq!(
            AnalyzedQuery::analyze("代码搜索请求参数实体在哪里定义").goal,
            QueryGoal::TypeLookup
        );
        assert_eq!(
            AnalyzedQuery::analyze("插件管理页面怎么绘制").goal,
            QueryGoal::Implementation
        );
        assert_eq!(
            AnalyzedQuery::analyze("这个函数在哪里使用").goal,
            QueryGoal::CallSite
        );
        assert_eq!(
            AnalyzedQuery::analyze("索引 manifest 是什么").goal,
            QueryGoal::Concept
        );
    }

    /// 精确符号走快速计划，自然语言才允许语义与模型重排。
    #[test]
    fn search_plan_routes_expensive_channels_by_intent() {
        let identifier =
            SearchPlan::for_query(&AnalyzedQuery::analyze("split_camel_case"), 10, true, true);
        assert!(!identifier.enable_semantic);
        assert!(!identifier.allow_rerank);
        assert_eq!(identifier.candidate_pool_limit, 10);

        let natural = SearchPlan::for_query(
            &AnalyzedQuery::analyze("工具调用在什么情况下需要用户审批"),
            10,
            true,
            true,
        );
        assert!(natural.enable_semantic);
        assert!(natural.allow_rerank);
        assert_eq!(natural.candidate_pool_limit, 40);
        assert!(natural.lexical_limit >= 10 && natural.semantic_limit >= 10);
    }
}

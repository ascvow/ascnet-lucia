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
    /// 剥离开头指令性动词后、真正送去检索的文本。
    ///
    /// 用户习惯把查询写成"搜索 X""帮我找 X""find X"——这些词表达的是**动作**，不是要找的
    /// 内容，但检索层无法区分：实测"搜索 agent 一轮对话的代码实现"里的"搜索"同时污染了
    /// 词法与语义两路，返回清一色的代码检索子系统实现，而真正的目标一条都没进来。
    ///
    /// 意图与目标分类仍然基于原始查询（"在哪里定义"这类措辞必须保留），只有召回使用本字段。
    pub retrieval_text: String,
    /// 表层规则无法判定的开头指令词，等待语料频次裁决。
    ///
    /// 为 `None` 表示查询开头没有歧义：要么没有指令词，要么已经被确定性规则剥离。
    pub pending_directive: Option<PendingDirective>,
}

/// 一个"既可能是指令、也可能是复合名词前半"的开头中文词。
///
/// "搜索主播开播代码"里的"搜索"是指令，"搜索结果怎么打包"里的"搜索"是主题的一部分。
/// 中文没有词边界，仅凭字符形态无法区分这两者，因此把判据留到能看到语料的调用方。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingDirective {
    /// 跨越指令词与后续内容的双字词元，例如"搜索主播"的"索主"。
    ///
    /// 它是判据本身：真正的复合名词必然在语料里留下这个 bigram，纯指令则不会。
    pub boundary_bigram: String,
    /// 判定为指令时应当采用的检索文本。
    pub stripped_text: String,
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
        let (retrieval_text, pending_directive) = strip_leading_directive(trimmed);
        Self {
            intent,
            goal: classify_goal(trimmed),
            retrieval_text,
            pending_directive,
            identifiers,
            cjk_dominant: cjk_dominant(trimmed),
        }
    }

    /// 用语料的词元频次裁决开头那个歧义中文指令词，必要时改写 [`Self::retrieval_text`]。
    ///
    /// # 判据
    ///
    /// 看跨界 bigram（"搜索主播"的"索主"）在语料中出现过几篇文档。复合名词一定会把这个
    /// bigram 带进索引——"搜索结果"出现过，"索结"就必然出现过；反之，纯指令与后面的内容
    /// 之间不存在这个组合。因此 `document_frequency` 返回 0 时判为指令并剥离。
    ///
    /// 判据方向是安全的：即便某个复合名词恰好没进语料而被误剥，它本来也检索不到任何东西，
    /// 剥离后至少还能召回剩余部分；反之保留指令词会持续污染词法与语义两路。
    ///
    /// # 已知局限
    ///
    /// 阈值取 0 而不是一个小正数，是为了让误判只会退回改动前的行为。代价是跨界 bigram
    /// 在大语料里偶然出现一两次（中文注释里跨词边界的巧合）就会让指令词继续保留。放宽
    /// 阈值能多修一些查询，但会开始误剥低频真复合词——那是把"没修好"换成"改坏了"。
    ///
    /// `document_frequency` 接收一个词元，返回包含它的文档数。该方法不发起网络请求。
    /// 返回是否真的剥离了指令词，供诊断展示。
    pub fn resolve_leading_directive(
        &mut self,
        document_frequency: impl Fn(&str) -> usize,
    ) -> bool {
        let Some(pending) = self.pending_directive.take() else {
            return false;
        };
        if document_frequency(&pending.boundary_bigram) > 0 {
            return false;
        }
        self.retrieval_text = pending.stripped_text;
        true
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

/// 检索结果的置信度等级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfidenceLevel {
    /// 本地结构证据足够明确，可直接返回。
    High,
    /// 候选分歧、分差过小或角色不匹配，可能需要模型重排。
    Low,
}

impl ConfidenceLevel {
    /// 返回稳定的机器可读名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
        }
    }
}

/// 一次本地检索排序的可解释置信度。
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalConfidence {
    /// 高或低置信度。
    pub level: ConfidenceLevel,
    /// Top-1 相对 Top-2 的归一化分差；候选不足两条时为 1。
    pub top_margin: f32,
    /// Top-1 是否同时得到符号与词法通道支持。
    pub symbol_lexical_agreement: bool,
    /// Top-1 是否被两路及以上召回器共同召回。
    ///
    /// 比 [`Self::symbol_lexical_agreement`] 宽：语义 + 词法同样是相互独立的佐证，而在
    /// 中文自然语言查询里这才是常见组合。多通道一致意味着本地排序已有独立证据支撑，
    /// 不该再花一次远端往返让模型重新猜。
    pub multi_channel_top: bool,
    /// Top-1 是否为精确符号命中。
    pub exact_symbol_hit: bool,
    /// Top-1 角色与候选池中的目标角色是否冲突。
    pub role_conflict: bool,
    /// 候选池是否至少包含一个实现角色。
    pub has_implementation: bool,
    /// 形成当前结论的可读原因。
    pub reasons: Vec<String>,
}

/// 一次请求实际采用的延迟路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchPath {
    /// 精确标识符、限定名或强本地命中，不访问模型重排。
    Fast,
    /// 多路召回与结构融合，但没有模型重排。
    Normal,
    /// 低置信度困难查询，调用了远端模型重排。
    Hard,
}

impl SearchPath {
    /// 返回稳定的机器可读名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Normal => "normal",
            Self::Hard => "hard",
        }
    }
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
        // 返回一条位置也需要同样的判别候选，不能把输出条数误当召回深度。
        let natural_language = query.intent == QueryIntent::NaturalLanguage && result_limit > 0;
        let candidate_pool_limit = if natural_language && rerank_allowed {
            candidate_pool_limit.max(40)
        } else {
            candidate_pool_limit
        };
        let natural_limit = if natural_language {
            expanded(12).max(120)
        } else {
            0
        };
        let (symbol_limit, lexical_limit, semantic_limit) = match query.intent {
            QueryIntent::Identifier | QueryIntent::QualifiedSymbol => (expanded(4), expanded(2), 0),
            // 自然语言通常没有符号锚点，先扩大本地召回覆盖，再把有限候选交给重排。
            // 返回条数为 10 时两路各取 120 条，仍受 200 条上限约束，不扩大模型候选预算。
            QueryIntent::NaturalLanguage => (expanded(2), natural_limit, natural_limit),
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

/// 剥离查询开头的指令性动词，返回真正要检索的内容。
///
/// # 为什么这张表不是"随便列的"
///
/// 实测五种说法后可以看到，未被剥离的指令词（"看下""我想知道""帮忙定位"）只让名次
/// 轻微波动，结果仍落在正确的文件里——它们在语料中不指代任何东西，只是噪声。真正会
/// 造成灾难性偏离的，是**既是指令、又恰好是代码领域名词**的那批词：一个代码检索项目里
/// 的"搜索"会把整条查询的语义拖向检索子系统，词法与语义两路同时中招。
///
/// 因此这张表要覆盖的不是"所有指令词"（那不可枚举），而是这批高风险的同形词，它们是
/// 有限且稳定的。其余说法交给召回本身容错。
///
/// 只处理**开头**：句中的"搜索"往往是真实内容（"搜索结果怎么打包"里它就是主题）。逐个
/// 剥离并允许连用（"帮我找一下 X"），剥净后若为空则退回原文——宁可保留噪声，也不能把
/// 查询变成空串。
///
/// # 返回值
///
/// 第一项是按确定性规则剥离后的检索文本。第二项在"中文指令词紧接中文"这种表层无法判定
/// 的情况下给出待裁决信息，由 [`AnalyzedQuery::resolve_leading_directive`] 用语料频次决定
/// 是否采纳；确定无歧义时为 `None`。
fn strip_leading_directive(query: &str) -> (String, Option<PendingDirective>) {
    /// 开头出现即视为指令的词，按长的优先，避免"帮我找"被"找"提前截断。
    const DIRECTIVES: &[&str] = &[
        "帮我搜索一下",
        "帮我查找一下",
        "帮我找一下",
        "帮忙搜索一下",
        "帮忙查找一下",
        "帮忙定位一下",
        "帮忙找一下",
        "帮忙搜索",
        "帮忙查找",
        "帮忙定位",
        "帮忙找",
        "帮我搜索",
        "帮我查找",
        "帮我找",
        "我想找",
        "我要找",
        "给我找",
        "给我看看",
        "查找一下",
        "搜索一下",
        "检索一下",
        "定位一下",
        "找一下",
        "看一下",
        "看下",
        "看看",
        "搜索",
        "查找",
        "检索",
        "查询",
        "定位",
        "列出",
        "显示",
        "search for",
        "search",
        "find me",
        "find",
        "show me",
        "locate",
        "look for",
    ];

    let mut current = query.trim();
    loop {
        let lower = current.to_ascii_lowercase();
        let Some(directive) = DIRECTIVES
            .iter()
            .find(|directive| lower.starts_with(**directive))
        else {
            break;
        };
        let tail = &current[directive.len()..];
        let rest = tail.trim_start_matches(|character: char| {
            character.is_whitespace() || character == ':' || character == '：'
        });
        if rest.is_empty() {
            break;
        }
        if !is_directive_boundary(directive, tail) {
            // 中文指令词紧接中文：留住原文，同时备好"判定为指令"时该用的文本与判据。
            return (
                current.to_string(),
                boundary_bigram(directive, rest).map(|boundary_bigram| PendingDirective {
                    boundary_bigram,
                    stripped_text: strip_leading_directive(rest).0,
                }),
            );
        }
        current = rest;
    }
    (current.to_string(), None)
}

/// 取指令词末字与后续首字组成的双字词元；任一侧不是中日韩文字时返回 `None`。
///
/// 词元形态必须与 [`super::tokenize`] 对中文的切分一致，否则拿它去查倒排索引永远查不到。
fn boundary_bigram(directive: &str, rest: &str) -> Option<String> {
    let last = directive.chars().next_back().filter(|c| is_cjk_char(*c))?;
    let next = rest.chars().next().filter(|c| is_cjk_char(*c))?;
    Some([last, next].iter().collect())
}

/// 判断开头的疑似指令词后面是不是真的跟着"要找的内容"。
///
/// # 为什么需要这条判据
///
/// 中文没有词边界，"搜索"既可能是指令（"搜索 agent 一轮对话"），也可能是复合名词的前半
/// （"搜索结果怎么打包"里它就是查询主题）。无条件剥离会把后者砍成"结果怎么打包"，丢掉
/// 最重要的限定词。
///
/// 两个确定性信号说明它是指令：后面跟着**分隔符**（空格、冒号），或者后面**换了字符集**
/// （中文指令词后紧跟拉丁标识符）。
///
/// # 表层规则到此为止
///
/// "搜索主播开播代码"这类"中文指令 + 中文目标且无分隔"的写法，无法与"搜索结果怎么打包"
/// 这样的复合名词区分。本函数对它返回 `false`，由调用方带着语料频次继续裁决——见
/// [`AnalyzedQuery::resolve_leading_directive`]。纯表层规则在这里没有更多信息可用。
fn is_directive_boundary(directive: &str, tail: &str) -> bool {
    let Some(next) = tail.chars().next() else {
        return false;
    };
    if next.is_whitespace() || next == ':' || next == '：' {
        return true;
    }
    let directive_is_cjk = directive.chars().next().is_some_and(is_cjk_char);
    // 换字符集即视为边界：中文指令后接拉丁标识符，或英文指令后接中文。
    directive_is_cjk != is_cjk_char(next)
}

/// 判断字符是否属于中日韩表意文字区。
fn is_cjk_char(character: char) -> bool {
    matches!(character as u32,
        0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2A6DF)
}

/// 判断文本是否包含任意一个固定意图词。
fn contains_any(text: &str, markers: &[&str]) -> bool {
    markers.iter().any(|marker| text.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 四类查询形态必须按确定性字符规则区分。
    /// 开头的指令词必须被剥离，句中的同形词必须保留。
    ///
    /// 保留句中同形词是这条规则成立的前提："搜索结果怎么打包"里的"搜索"就是查询主题，
    /// 一并剥掉会让查询失去最重要的限定词。
    #[test]
    fn leading_directives_are_stripped_without_touching_content() {
        for (query, expected) in [
            ("搜索agent一轮对话的代码实现", "agent一轮对话的代码实现"),
            ("帮我找一下 权限检查", "权限检查"),
            ("帮忙定位 索引构建流程", "索引构建流程"),
            ("看下 会话怎么保存", "会话怎么保存"),
            ("find the permission check", "the permission check"),
            ("show me PermissionManager", "PermissionManager"),
        ] {
            assert_eq!(
                AnalyzedQuery::analyze(query).retrieval_text,
                expected,
                "查询 `{query}` 的剥离结果不符"
            );
        }
        // 复合名词与句中同形词都是内容，不能剥离。中文指令后紧跟中文且无分隔时，
        // 无法与复合名词区分，一律保守保留。
        for query in [
            "搜索结果怎么打包",
            "查询计划怎么生成",
            "定位服务在哪里初始化",
            "检索索引怎么建立",
        ] {
            assert_eq!(
                AnalyzedQuery::analyze(query).retrieval_text,
                query,
                "句中同形词不得被剥离：{query}"
            );
        }
    }

    /// 中文指令词紧接中文时，必须由语料频次而不是字符形态做最终裁决。
    ///
    /// 这是"搜索主播开播代码"返回一堆 `searchUser` 的直接原因：未被剥离的"搜索"同时污染
    /// 词法与语义两路，把整条查询拖向检索子系统本身。
    #[test]
    fn ambiguous_cjk_directive_is_resolved_by_corpus() {
        let mut stripped = AnalyzedQuery::analyze("搜索主播开播代码");
        assert_eq!(stripped.retrieval_text, "搜索主播开播代码");
        assert_eq!(
            stripped
                .pending_directive
                .as_ref()
                .map(|pending| pending.boundary_bigram.as_str()),
            Some("索主")
        );
        // 语料里没有"搜索主播"这个组合，判为指令。
        assert!(stripped.resolve_leading_directive(|_| 0));
        assert_eq!(stripped.retrieval_text, "主播开播代码");
        // 裁决只发生一次，重复调用不再改写文本。
        assert!(!stripped.resolve_leading_directive(|_| 0));

        // 语料里存在"搜索结果"，跨界 bigram 出现过，按复合名词保留。
        let mut kept = AnalyzedQuery::analyze("搜索结果怎么打包");
        assert!(!kept.resolve_leading_directive(|term| usize::from(term == "索结")));
        assert_eq!(kept.retrieval_text, "搜索结果怎么打包");
    }

    /// 已由确定性规则处理的查询不得留下待裁决状态，避免语料频次二次改写。
    #[test]
    fn unambiguous_queries_leave_nothing_pending() {
        for query in [
            "搜索 agent 一轮对话的代码实现",
            "帮我找一下 权限检查",
            "find the permission check",
            "会话怎么保存",
            "PermissionManager",
        ] {
            assert!(
                AnalyzedQuery::analyze(query).pending_directive.is_none(),
                "`{query}` 不应留下待裁决指令词"
            );
        }
    }

    /// 指令词后的全角冒号必须与半角冒号一样被剥离。
    #[test]
    fn full_width_colon_is_treated_as_separator() {
        assert_eq!(
            AnalyzedQuery::analyze("搜索：权限检查").retrieval_text,
            "权限检查"
        );
    }

    /// 整条查询都是指令词时必须退回原文，不能变成空串。
    #[test]
    fn all_directive_query_falls_back_to_original() {
        assert_eq!(AnalyzedQuery::analyze("搜索").retrieval_text, "搜索");
        assert_eq!(
            AnalyzedQuery::analyze("帮我找一下").retrieval_text,
            "帮我找一下"
        );
    }

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
        assert_eq!(natural.lexical_limit, 120);
        assert_eq!(natural.semantic_limit, 120);
        let empty = SearchPlan::for_query(&AnalyzedQuery::analyze("会话保存"), 0, true, true);
        assert_eq!(empty.candidate_pool_limit, 0);
        assert!(!empty.enable_semantic);
        let large = SearchPlan::for_query(&AnalyzedQuery::analyze("会话保存"), 50, true, true);
        assert_eq!(large.semantic_limit, 200);
        let single = SearchPlan::for_query(&AnalyzedQuery::analyze("会话保存"), 1, true, true);
        assert_eq!(single.semantic_limit, 120);
        assert_eq!(single.lexical_limit, 120);
        assert_eq!(single.candidate_pool_limit, 40);
    }
}

//! `.gitignore` 语义的忽略规则。
//!
//! 实现范围覆盖扫描器真正会遇到的 gitignore 语法：注释、否定（`!`）、目录限定（结尾 `/`）、
//! 锚定（开头 `/` 或含内部 `/`）、`*`、`?`、字符类与 `**`。规则文件按目录嵌套，深层文件
//! 的判定覆盖浅层，同一文件内后出现的规则覆盖先出现的——与 git 一致。
//!
//! # 三层优先级
//!
//! ```text
//! 硬排除      .git/、.hg/、.svn/        不可被任何规则重新包含
//! 默认忽略    node_modules/、target/…   可被 .gitignore / .agentignore 的 `!` 覆盖
//! 规则文件    .gitignore、.agentignore  按目录深度递增覆盖
//! ```
//!
//! 硬排除单独成层，是因为把 `.git/` 交给可被否定的规则去挡，等于允许一条用户规则让扫描器
//! 走进版本库内部——那里的对象文件既无检索价值，体量又足以撑爆索引。

/// 永不索引且不可被规则覆盖的目录名。
const HARD_EXCLUDED_DIRS: &[&str] = &[".git", ".hg", ".svn"];

/// 内置默认忽略规则。
///
/// 这些是依赖目录与构建产物，在任何语言的仓库里都没有检索价值。它们以规则形式（而不是
/// 硬编码判断）存在，因此用户可以在 `.agentignore` 里用 `!vendor/` 这类否定规则重新包含。
const DEFAULT_IGNORE_RULES: &[&str] = &[
    "node_modules/",
    "target/",
    "dist/",
    "build/",
    "out/",
    ".next/",
    ".nuxt/",
    ".svelte-kit/",
    ".turbo/",
    ".parcel-cache/",
    "coverage/",
    "vendor/",
    "__pycache__/",
    ".venv/",
    "venv/",
    ".mypy_cache/",
    ".pytest_cache/",
    ".ruff_cache/",
    ".gradle/",
    ".idea/",
    ".vscode/",
    ".cache/",
    ".DS_Store",
];

/// 插件自身的忽略文件名。
pub const AGENT_IGNORE_FILE: &str = ".agentignore";

/// git 的忽略文件名。
pub const GIT_IGNORE_FILE: &str = ".gitignore";

/// 一个模式段。
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// `**`：匹配零个或多个路径段。
    AnyDepth,
    /// 单个路径段的 glob 模式。
    Glob(Vec<char>),
}

/// 一条编译后的忽略规则。
#[derive(Debug, Clone)]
struct IgnoreRule {
    /// 按 `/` 切分的模式段。
    segments: Vec<Segment>,
    /// 是否为否定规则（`!` 开头）。
    negated: bool,
    /// 是否只匹配目录（结尾 `/`）。
    dir_only: bool,
}

impl IgnoreRule {
    /// 编译一行忽略规则；空行与注释返回 `None`。
    fn parse(line: &str) -> Option<Self> {
        let mut pattern = line;
        if pattern.starts_with('#') {
            return None;
        }
        // git 忽略行尾空白，除非被反斜杠转义。
        pattern = trim_unescaped_trailing_space(pattern);
        if pattern.is_empty() {
            return None;
        }

        let negated = pattern.starts_with('!');
        if negated {
            pattern = &pattern[1..];
        } else if let Some(escaped) = pattern.strip_prefix("\\!") {
            // `\!` 表示字面量感叹号开头，不是否定。
            pattern = escaped;
            return Self::compile(pattern, false, true);
        }
        if pattern.is_empty() {
            return None;
        }
        Self::compile(pattern, negated, false)
    }

    /// 把去掉 `!` 前缀后的模式编译成规则。
    ///
    /// `literal_bang` 表示模式原本以 `\!` 转义开头，需要把感叹号还原为字面量。
    fn compile(pattern: &str, negated: bool, literal_bang: bool) -> Option<Self> {
        let mut pattern = pattern;
        let dir_only = pattern.ends_with('/');
        if dir_only {
            pattern = &pattern[..pattern.len() - 1];
        }
        if pattern.is_empty() {
            return None;
        }

        // 含内部斜杠或以斜杠开头的模式锚定到规则文件所在目录；其余模式匹配任意深度的
        // 路径段，等价于在前面补一个 `**/`。
        let anchored = pattern.contains('/');
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        if pattern.is_empty() {
            return None;
        }

        let mut segments: Vec<Segment> = Vec::new();
        if !anchored {
            segments.push(Segment::AnyDepth);
        }
        for (index, raw) in pattern.split('/').enumerate() {
            if raw == "**" {
                segments.push(Segment::AnyDepth);
                continue;
            }
            let mut chars: Vec<char> = raw.chars().collect();
            if literal_bang && index == 0 {
                chars.insert(0, '!');
            }
            segments.push(Segment::Glob(chars));
        }
        if segments.is_empty() {
            return None;
        }
        Some(Self {
            segments,
            negated,
            dir_only,
        })
    }

    /// 判断该规则是否匹配给定的路径段序列。
    fn matches(&self, components: &[&str], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        match_segments(&self.segments, components)
    }
}

/// 去掉未被反斜杠转义的行尾空白。
fn trim_unescaped_trailing_space(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    while end > 0 && (bytes[end - 1] == b' ' || bytes[end - 1] == b'\t') {
        // 统计紧邻的反斜杠数量：奇数个表示该空白被转义，必须保留。
        let mut backslashes = 0;
        let mut cursor = end - 1;
        while cursor > 0 && bytes[cursor - 1] == b'\\' {
            backslashes += 1;
            cursor -= 1;
        }
        if backslashes % 2 == 1 {
            break;
        }
        end -= 1;
    }
    &line[..end]
}

/// 按段匹配，`**` 可吸收任意数量的路径段。
fn match_segments(segments: &[Segment], components: &[&str]) -> bool {
    match segments.split_first() {
        None => components.is_empty(),
        Some((Segment::AnyDepth, rest)) => {
            // `**` 出现在末尾时匹配剩余全部段；否则回溯尝试每一种切分。
            if rest.is_empty() {
                return true;
            }
            (0..=components.len()).any(|skip| match_segments(rest, &components[skip..]))
        }
        Some((Segment::Glob(pattern), rest)) => {
            let Some((head, tail)) = components.split_first() else {
                return false;
            };
            glob_matches(pattern, &head.chars().collect::<Vec<_>>()) && match_segments(rest, tail)
        }
    }
}

/// 单个路径段的 glob 匹配，支持 `*`、`?`、`[...]` 与反斜杠转义。
fn glob_matches(pattern: &[char], text: &[char]) -> bool {
    let (mut pattern_index, mut text_index) = (0usize, 0usize);
    // 记录最近一个 `*` 的位置，失配时回溯到它并多吃一个字符。
    let (mut star_pattern, mut star_text) = (usize::MAX, 0usize);

    while text_index < text.len() {
        match pattern.get(pattern_index) {
            Some('*') => {
                star_pattern = pattern_index;
                star_text = text_index;
                pattern_index += 1;
            }
            Some('?') => {
                pattern_index += 1;
                text_index += 1;
            }
            Some('[') => match match_char_class(pattern, pattern_index, text[text_index]) {
                Some(next) => {
                    pattern_index = next;
                    text_index += 1;
                }
                None if star_pattern != usize::MAX => {
                    star_text += 1;
                    text_index = star_text;
                    pattern_index = star_pattern + 1;
                }
                None => return false,
            },
            Some('\\') if pattern_index + 1 < pattern.len() => {
                if pattern[pattern_index + 1] == text[text_index] {
                    pattern_index += 2;
                    text_index += 1;
                } else if star_pattern != usize::MAX {
                    star_text += 1;
                    text_index = star_text;
                    pattern_index = star_pattern + 1;
                } else {
                    return false;
                }
            }
            Some(&expected) if expected == text[text_index] => {
                pattern_index += 1;
                text_index += 1;
            }
            _ if star_pattern != usize::MAX => {
                star_text += 1;
                text_index = star_text;
                pattern_index = star_pattern + 1;
            }
            _ => return false,
        }
    }

    while pattern.get(pattern_index) == Some(&'*') {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

/// 匹配 `[...]` 字符类；成功时返回类结束后的模式下标。
fn match_char_class(pattern: &[char], open: usize, candidate: char) -> Option<usize> {
    let mut index = open + 1;
    let negated = matches!(pattern.get(index), Some('!') | Some('^'));
    if negated {
        index += 1;
    }
    let mut matched = false;
    let mut first = true;
    while index < pattern.len() {
        // 右括号出现在类的第一个位置时是字面量，不表示闭合。
        if pattern[index] == ']' && !first {
            let hit = matched != negated;
            return hit.then_some(index + 1);
        }
        first = false;
        let low = pattern[index];
        // `a-z` 形式的区间；末尾的 `-` 按字面量处理。
        if pattern.get(index + 1) == Some(&'-')
            && pattern.get(index + 2).is_some_and(|next| *next != ']')
        {
            let high = pattern[index + 2];
            if (low..=high).contains(&candidate) {
                matched = true;
            }
            index += 3;
            continue;
        }
        if low == candidate {
            matched = true;
        }
        index += 1;
    }
    // 没有闭合括号：按 git 的做法，整个 `[` 当作字面量处理。
    None
}

/// 一个目录下的一组忽略规则。
#[derive(Debug, Clone)]
pub struct IgnoreRuleSet {
    /// 规则生效的仓库相对目录；仓库根为空串。
    base_dir: String,
    /// 按文件内顺序保存的规则。
    rules: Vec<IgnoreRule>,
}

impl IgnoreRuleSet {
    /// 解析一个忽略文件的内容。
    ///
    /// `base_dir` 是该忽略文件所在目录的仓库相对路径，仓库根传空串。
    pub fn parse(base_dir: &str, content: &str) -> Self {
        Self {
            base_dir: normalize_dir(base_dir),
            rules: content.lines().filter_map(IgnoreRule::parse).collect(),
        }
    }

    /// 构造内置默认规则集。
    pub fn defaults() -> Self {
        Self {
            base_dir: String::new(),
            rules: DEFAULT_IGNORE_RULES
                .iter()
                .filter_map(|line| IgnoreRule::parse(line))
                .collect(),
        }
    }

    /// 该规则集是否不含任何规则。
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// 判定一个仓库相对路径。
    ///
    /// 返回 `None` 表示本规则集没有任何规则匹配该路径，判定权交给链上的其他规则集。
    fn evaluate(&self, path: &str, is_dir: bool) -> Option<bool> {
        let relative = if self.base_dir.is_empty() {
            path
        } else {
            path.strip_prefix(&self.base_dir)?.strip_prefix('/')?
        };
        if relative.is_empty() {
            return None;
        }
        let components: Vec<&str> = relative
            .split('/')
            .filter(|part| !part.is_empty())
            .collect();
        // 同一文件内后出现的规则覆盖先出现的，因此反向查找首个命中即可。
        self.rules
            .iter()
            .rev()
            .find(|rule| rule.matches(&components, is_dir))
            .map(|rule| !rule.negated)
    }
}

/// 把目录路径规范化为不带首尾斜杠的形式。
fn normalize_dir(dir: &str) -> String {
    dir.trim_matches('/').to_string()
}

/// 按目录嵌套组织的忽略规则链。
///
/// 规则集存放在 arena 中，每个待遍历目录只持有一个链节点下标而不是完整规则副本。
/// 这让扫描游标在跨调用保存时保持紧凑——深目录树下每层都复制一份规则会让游标体积
/// 随深度平方增长。
#[derive(Debug, Default)]
pub struct IgnoreStack {
    /// 规则集 arena。
    sets: Vec<IgnoreRuleSet>,
    /// 链节点 arena；每个节点指向父节点与一个规则集。
    links: Vec<IgnoreLink>,
}

/// 忽略规则链上的一个节点。
#[derive(Debug, Clone, Copy)]
struct IgnoreLink {
    /// 父节点下标；根节点为 `None`。
    parent: Option<u32>,
    /// 该节点引用的规则集下标。
    set: u32,
}

/// 忽略规则链的标识；`None` 表示空链。
pub type ChainId = Option<u32>;

impl IgnoreStack {
    /// 创建只含内置默认规则的规则链。
    pub fn new() -> Self {
        let mut stack = Self::default();
        stack.sets.push(IgnoreRuleSet::defaults());
        stack.links.push(IgnoreLink {
            parent: None,
            set: 0,
        });
        stack
    }

    /// 返回根规则链（仅内置默认规则）。
    pub fn root(&self) -> ChainId {
        Some(0)
    }

    /// 在已有链上追加一个规则集，返回新的链标识。
    ///
    /// 规则集数量受 `u32` 上限约束；超出时返回原链，等价于忽略该规则文件而不是让扫描失败。
    pub fn push(&mut self, parent: ChainId, set: IgnoreRuleSet) -> ChainId {
        if set.is_empty() {
            return parent;
        }
        let (Ok(set_index), Ok(link_index)) = (
            u32::try_from(self.sets.len()),
            u32::try_from(self.links.len()),
        ) else {
            return parent;
        };
        self.sets.push(set);
        self.links.push(IgnoreLink {
            parent,
            set: set_index,
        });
        Some(link_index)
    }

    /// 判断路径是否应当被忽略。
    ///
    /// 硬排除优先于全部规则；其余按链从浅到深评估，深层判定覆盖浅层。
    pub fn is_ignored(&self, chain: ChainId, path: &str, is_dir: bool) -> bool {
        if is_hard_excluded(path) {
            return true;
        }
        // 链自身是从深到浅的父指针，先收集再反向遍历即可得到"深层覆盖浅层"。
        let mut nodes = Vec::new();
        let mut cursor = chain;
        while let Some(index) = cursor {
            let link = self.links[index as usize];
            nodes.push(link.set);
            cursor = link.parent;
        }
        let mut ignored = false;
        for set_index in nodes.into_iter().rev() {
            if let Some(decision) = self.sets[set_index as usize].evaluate(path, is_dir) {
                ignored = decision;
            }
        }
        ignored
    }
}

/// 判断路径是否命中不可覆盖的硬排除目录。
fn is_hard_excluded(path: &str) -> bool {
    path.split('/')
        .any(|component| HARD_EXCLUDED_DIRS.contains(&component))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一个只含给定规则文本的规则链。
    fn stack_with(base_dir: &str, content: &str) -> (IgnoreStack, ChainId) {
        let mut stack = IgnoreStack::new();
        let chain = stack.push(stack.root(), IgnoreRuleSet::parse(base_dir, content));
        (stack, chain)
    }

    /// 内置默认规则必须挡住依赖与产物目录。
    #[test]
    fn default_rules_exclude_dependency_and_build_dirs() {
        let stack = IgnoreStack::new();
        let root = stack.root();
        for dir in ["node_modules", "target", "dist", "coverage", ".next"] {
            assert!(stack.is_ignored(root, dir, true), "{dir} 应被默认规则忽略");
        }
        assert!(!stack.is_ignored(root, "src", true));
    }

    /// 嵌套目录中的默认忽略目录同样必须命中。
    #[test]
    fn default_rules_apply_at_any_depth() {
        let stack = IgnoreStack::new();
        assert!(stack.is_ignored(stack.root(), "packages/web/node_modules", true));
        assert!(stack.is_ignored(stack.root(), "crates/core/target", true));
    }

    /// 硬排除不得被否定规则覆盖。
    #[test]
    fn hard_excludes_cannot_be_negated() {
        let (stack, chain) = stack_with("", "!.git\n!.git/\n");
        assert!(stack.is_ignored(chain, ".git", true));
        assert!(stack.is_ignored(chain, ".git/objects/ab", true));
    }

    /// 默认忽略可以被显式否定规则重新包含。
    #[test]
    fn defaults_can_be_negated_by_rule_file() {
        let (stack, chain) = stack_with("", "!vendor/\n");
        assert!(!stack.is_ignored(chain, "vendor", true));
        // 未被否定的默认规则保持生效。
        assert!(stack.is_ignored(chain, "node_modules", true));
    }

    /// 注释与空行必须被跳过。
    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let (stack, chain) = stack_with("", "# 注释\n\n   \nlogs/\n");
        assert!(stack.is_ignored(chain, "logs", true));
        assert!(!stack.is_ignored(chain, "#", false));
    }

    /// 目录限定规则不得匹配同名文件。
    #[test]
    fn directory_only_rules_do_not_match_files() {
        let (stack, chain) = stack_with("", "build/\n");
        assert!(stack.is_ignored(chain, "build", true));
        assert!(!stack.is_ignored(chain, "build", false));
    }

    /// 锚定规则只在规则文件所在目录生效。
    #[test]
    fn anchored_rules_only_match_at_base_dir() {
        let (stack, chain) = stack_with("", "/config.json\n");
        assert!(stack.is_ignored(chain, "config.json", false));
        assert!(!stack.is_ignored(chain, "src/config.json", false));
    }

    /// 未锚定规则必须在任意深度匹配。
    #[test]
    fn unanchored_rules_match_at_any_depth() {
        let (stack, chain) = stack_with("", "notes.md\n");
        assert!(stack.is_ignored(chain, "notes.md", false));
        assert!(stack.is_ignored(chain, "docs/inner/notes.md", false));
    }

    /// 含内部斜杠的规则视为锚定。
    #[test]
    fn rules_with_internal_slash_are_anchored() {
        let (stack, chain) = stack_with("", "src/generated\n");
        assert!(stack.is_ignored(chain, "src/generated", true));
        assert!(!stack.is_ignored(chain, "crates/src/generated", true));
    }

    /// 通配符必须按段匹配且不跨越斜杠。
    #[test]
    fn wildcards_match_within_a_single_segment() {
        let (stack, chain) = stack_with("", "*.log\n");
        assert!(stack.is_ignored(chain, "server.log", false));
        assert!(stack.is_ignored(chain, "logs/server.log", false));
        assert!(!stack.is_ignored(chain, "server.log.txt", false));
    }

    /// `**` 必须能跨越任意层级。
    #[test]
    fn double_star_spans_multiple_segments() {
        let (stack, chain) = stack_with("", "docs/**/draft.md\n");
        assert!(stack.is_ignored(chain, "docs/draft.md", false));
        assert!(stack.is_ignored(chain, "docs/a/b/draft.md", false));
        assert!(!stack.is_ignored(chain, "other/a/draft.md", false));
    }

    /// 结尾 `**` 必须匹配目录下的全部内容。
    #[test]
    fn trailing_double_star_matches_everything_below() {
        let (stack, chain) = stack_with("", "generated/**\n");
        assert!(stack.is_ignored(chain, "generated/a.rs", false));
        assert!(stack.is_ignored(chain, "generated/a/b/c.rs", false));
    }

    /// 问号必须匹配且只匹配一个字符。
    #[test]
    fn question_mark_matches_single_character() {
        let (stack, chain) = stack_with("", "temp?.txt\n");
        assert!(stack.is_ignored(chain, "temp1.txt", false));
        assert!(!stack.is_ignored(chain, "temp.txt", false));
        assert!(!stack.is_ignored(chain, "temp12.txt", false));
    }

    /// 字符类必须支持区间与否定。
    #[test]
    fn character_classes_support_ranges_and_negation() {
        let (stack, chain) = stack_with("", "log[0-9].txt\nkeep[!x].md\n");
        assert!(stack.is_ignored(chain, "log5.txt", false));
        assert!(!stack.is_ignored(chain, "loga.txt", false));
        assert!(stack.is_ignored(chain, "keepa.md", false));
        assert!(!stack.is_ignored(chain, "keepx.md", false));
    }

    /// 同一文件内后出现的规则覆盖先出现的。
    #[test]
    fn later_rules_override_earlier_ones() {
        let (stack, chain) = stack_with("", "*.log\n!keep.log\n");
        assert!(stack.is_ignored(chain, "debug.log", false));
        assert!(!stack.is_ignored(chain, "keep.log", false));
    }

    /// 深层规则文件必须覆盖浅层判定。
    #[test]
    fn deeper_rule_files_override_shallower_ones() {
        let mut stack = IgnoreStack::new();
        let root = stack.push(stack.root(), IgnoreRuleSet::parse("", "*.log\n"));
        let nested = stack.push(root, IgnoreRuleSet::parse("crates/core", "!*.log\n"));

        assert!(stack.is_ignored(root, "app.log", false));
        assert!(!stack.is_ignored(nested, "crates/core/app.log", false));
        // 深层规则不得影响其目录之外的路径。
        assert!(stack.is_ignored(nested, "crates/other/app.log", false));
    }

    /// 子目录规则文件只对自身子树生效。
    #[test]
    fn nested_rule_set_scopes_to_its_subtree() {
        let mut stack = IgnoreStack::new();
        let chain = stack.push(stack.root(), IgnoreRuleSet::parse("docs", "internal/\n"));
        assert!(stack.is_ignored(chain, "docs/internal", true));
        assert!(!stack.is_ignored(chain, "internal", true));
    }

    /// 行尾空白按 git 规则处理。
    #[test]
    fn trailing_whitespace_is_trimmed_unless_escaped() {
        let (stack, chain) = stack_with("", "notes.md   \n");
        assert!(stack.is_ignored(chain, "notes.md", false));

        let (escaped_stack, escaped_chain) = stack_with("", "spaced\\ \n");
        assert!(escaped_stack.is_ignored(escaped_chain, "spaced ", false));
    }

    /// 转义的感叹号表示字面量而不是否定。
    #[test]
    fn escaped_bang_is_literal() {
        let (stack, chain) = stack_with("", "\\!important.md\n");
        assert!(stack.is_ignored(chain, "!important.md", false));
    }

    /// 空规则集不得在链上占用节点。
    #[test]
    fn empty_rule_set_does_not_extend_chain() {
        let mut stack = IgnoreStack::new();
        let root = stack.root();
        let chain = stack.push(root, IgnoreRuleSet::parse("docs", "# 只有注释\n"));
        assert_eq!(chain, root);
    }

    /// 未闭合的字符类不得让匹配 panic。
    #[test]
    fn unterminated_character_class_is_safe() {
        let (stack, chain) = stack_with("", "log[0-9.txt\n");
        assert!(!stack.is_ignored(chain, "log5.txt", false));
    }
}

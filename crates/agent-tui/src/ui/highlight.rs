//! 代码预览的轻量词法着色。
//!
//! 项目刻意关闭了 `tui-markdown` 的 syntect 特性以控制依赖体积，因此这里用一个
//! 与语言无关的通用词法器近似还原高亮效果：只区分注释、字符串、数值、关键字、
//! 类型和标点六类，不做语法分析。
//!
//! 这个取舍是明确的——它在预览场景下足够可读，但不保证对任意语言完全准确；
//! 需要精确高亮的场合应由插件自行渲染工具消息帧。

use crate::theme::{
    COLOR_SYNTAX_COMMENT, COLOR_SYNTAX_KEYWORD, COLOR_SYNTAX_NUMBER, COLOR_SYNTAX_PUNCT,
    COLOR_SYNTAX_STRING, COLOR_SYNTAX_TYPE, COLOR_TEXT,
};
use ratatui::prelude::*;

/// 预览可识别的代码语言。
///
/// 只区分注释与字符串风格不同的几族；同族语言共用一套规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CodeLanguage {
    /// 使用 `//` 与 `/* */` 注释的 C 族：Rust、TypeScript、Go、Java 等。
    CLike,
    /// 使用 `#` 注释的脚本与配置族：Python、Shell、TOML、YAML 等。
    Hash,
    /// 统一 diff：不按词法上色，改由 [`crate::ui::diff`] 按行语义（增删、文件头、hunk
    /// 头）着色。词法高亮会把 `+`/`-` 当成运算符，反而抹掉修改方向。
    Diff,
    /// 无法识别语言时按纯文本处理，只保留正文色。
    Plain,
}

impl CodeLanguage {
    /// 按文件扩展名推断语言；未知扩展名返回 [`CodeLanguage::Plain`]。
    pub(crate) fn from_path(path: &str) -> Self {
        let extension = path
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map(|(_, extension)| extension.to_ascii_lowercase())
            .unwrap_or_default();
        match extension.as_str() {
            "rs" | "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "go" | "java" | "c" | "h"
            | "cc" | "cpp" | "hpp" | "cs" | "kt" | "swift" | "php" | "scala" | "dart" | "json"
            | "jsonc" | "css" | "scss" => Self::CLike,
            "py" | "sh" | "bash" | "zsh" | "fish" | "toml" | "yaml" | "yml" | "rb" | "pl" | "r"
            | "ex" | "exs" | "nix" | "dockerfile" | "makefile" | "mk" => Self::Hash,
            // 补丁文件同样没有词法结构，交给 diff 行分类处理。
            "diff" | "patch" | "rej" => Self::Diff,
            _ => Self::Plain,
        }
    }

    /// 按围栏代码块声明的语言标签推断语言族。
    ///
    /// 标签是写给人看的名字（`rust`、`python`、`bash`），并不总是扩展名，
    /// 因此先映射常见别名，其余退回按扩展名判断；未知标签返回纯文本。
    pub(crate) fn from_tag(tag: &str) -> Self {
        match tag.trim().to_ascii_lowercase().as_str() {
            "rust" | "javascript" | "typescript" | "js" | "ts" | "jsx" | "tsx" => Self::CLike,
            "python" | "shell" | "bash" | "zsh" | "console" | "shell-session" => Self::Hash,
            "diff" | "patch" | "udiff" => Self::Diff,
            other => Self::from_path(&format!("file.{other}")),
        }
    }

    /// 判断该语言是否使用 `#` 起始的行注释。
    fn hash_comment(self) -> bool {
        matches!(self, Self::Hash)
    }

    /// 判断该语言是否使用 `//` 起始的行注释。
    fn slash_comment(self) -> bool {
        matches!(self, Self::CLike)
    }
}

/// 各族语言共享的关键字集合。
///
/// 故意合并多语言关键字：预览不做语法分析，误把某语言的标识符染成关键字的代价
/// 远小于为每种语言维护独立词表。
const KEYWORDS: &[&str] = &[
    "as",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "crate",
    "def",
    "default",
    "defer",
    "del",
    "do",
    "elif",
    "else",
    "enum",
    "export",
    "extends",
    "extern",
    "false",
    "final",
    "finally",
    "fn",
    "for",
    "from",
    "func",
    "function",
    "go",
    "if",
    "impl",
    "import",
    "in",
    "interface",
    "is",
    "let",
    "loop",
    "match",
    "mod",
    "move",
    "mut",
    "new",
    "nil",
    "none",
    "not",
    "null",
    "or",
    "package",
    "pass",
    "private",
    "protected",
    "pub",
    "public",
    "raise",
    "ref",
    "return",
    "self",
    "static",
    "struct",
    "super",
    "switch",
    "then",
    "this",
    "throw",
    "trait",
    "true",
    "try",
    "type",
    "typeof",
    "union",
    "unsafe",
    "use",
    "var",
    "void",
    "where",
    "while",
    "with",
    "yield",
    "and",
    "dyn",
];

/// 将一行代码切分为带颜色的片段。
///
/// 输入应当是单个逻辑行；换行符不会被特殊处理。未识别语言与 diff 整行返回正文色：
/// diff 的着色依赖跨行的 hunk 状态，只能由 [`crate::ui::diff`] 在整段上完成。
pub(crate) fn highlight_line(text: &str, language: CodeLanguage) -> Vec<Span<'static>> {
    if matches!(language, CodeLanguage::Plain | CodeLanguage::Diff) || text.is_empty() {
        return vec![Span::styled(text.to_string(), Style::new().fg(COLOR_TEXT))];
    }

    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        let ch = chars[index];

        // 行注释吞掉本行剩余内容。
        let is_line_comment =
            (language.slash_comment() && ch == '/' && chars.get(index + 1) == Some(&'/'))
                || (language.hash_comment() && ch == '#')
                || (language.slash_comment() && ch == '/' && chars.get(index + 1) == Some(&'*'));
        if is_line_comment {
            push_span(&mut spans, &chars[index..], COLOR_SYNTAX_COMMENT);
            break;
        }

        if ch == '"' || ch == '\'' || ch == '`' {
            let start = index;
            index += 1;
            while index < chars.len() {
                // 反斜杠转义下一个字符，避免在 "\"" 处提前收尾。
                if chars[index] == '\\' {
                    index += 2;
                    continue;
                }
                if chars[index] == ch {
                    index += 1;
                    break;
                }
                index += 1;
            }
            let end = index.min(chars.len());
            push_span(&mut spans, &chars[start..end], COLOR_SYNTAX_STRING);
            continue;
        }

        if ch.is_ascii_digit() {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric()
                    || chars[index] == '.'
                    || chars[index] == '_')
            {
                index += 1;
            }
            push_span(&mut spans, &chars[start..index], COLOR_SYNTAX_NUMBER);
            continue;
        }

        if ch.is_alphabetic() || ch == '_' {
            let start = index;
            while index < chars.len() && (chars[index].is_alphanumeric() || chars[index] == '_') {
                index += 1;
            }
            let word: String = chars[start..index].iter().collect();
            let color = if KEYWORDS.contains(&word.as_str()) {
                COLOR_SYNTAX_KEYWORD
            } else if word.starts_with(|first: char| first.is_uppercase()) {
                COLOR_SYNTAX_TYPE
            } else {
                COLOR_TEXT
            };
            spans.push(Span::styled(word, Style::new().fg(color)));
            continue;
        }

        // 连续的空白保持正文色，其余符号按标点着色。
        let start = index;
        let punctuation = !ch.is_whitespace();
        while index < chars.len()
            && !chars[index].is_alphanumeric()
            && chars[index] != '_'
            && chars[index] != '"'
            && chars[index] != '\''
            && chars[index] != '`'
            && chars[index].is_whitespace() != punctuation
        {
            index += 1;
        }
        if index == start {
            index += 1;
        }
        let color = if punctuation {
            COLOR_SYNTAX_PUNCT
        } else {
            COLOR_TEXT
        };
        push_span(&mut spans, &chars[start..index], color);
    }

    if spans.is_empty() {
        spans.push(Span::styled(text.to_string(), Style::new().fg(COLOR_TEXT)));
    }
    spans
}

/// 把字符切片作为一个片段追加到输出。
fn push_span(spans: &mut Vec<Span<'static>>, chars: &[char], color: Color) {
    if chars.is_empty() {
        return;
    }
    spans.push(Span::styled(
        chars.iter().collect::<String>(),
        Style::new().fg(color),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 拼接片段必须还原原始输入，避免高亮吞字或重复。
    fn joined(text: &str, language: CodeLanguage) -> String {
        highlight_line(text, language)
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// 任何语言下高亮都必须是无损切分。
    #[test]
    fn highlighting_preserves_original_text() {
        for text in [
            "pub fn main() -> Result<()> {",
            "  let value = \"带 \\\" 转义的中文\"; // 注释",
            "count = 42 + x_1",
            "# 纯注释行",
            "",
            "    ",
        ] {
            for language in [CodeLanguage::CLike, CodeLanguage::Hash, CodeLanguage::Plain] {
                assert_eq!(joined(text, language), text, "{text:?} / {language:?}");
            }
        }
    }

    /// 关键字、类型、字符串与注释必须分别着色。
    #[test]
    fn c_like_tokens_get_distinct_colors() {
        let spans = highlight_line("pub fn build(name: String) // 说明", CodeLanguage::CLike);
        let color_of = |needle: &str| {
            spans
                .iter()
                .find(|span| span.content.as_ref() == needle)
                .and_then(|span| span.style.fg)
        };

        assert_eq!(color_of("pub"), Some(COLOR_SYNTAX_KEYWORD));
        assert_eq!(color_of("fn"), Some(COLOR_SYNTAX_KEYWORD));
        assert_eq!(color_of("String"), Some(COLOR_SYNTAX_TYPE));
        assert_eq!(color_of("build"), Some(COLOR_TEXT));
        assert!(spans
            .iter()
            .any(|span| span.style.fg == Some(COLOR_SYNTAX_COMMENT)));
    }

    /// `#` 只在脚本族语言里是注释，C 族语言中不应吞掉整行。
    #[test]
    fn hash_is_comment_only_for_script_languages() {
        assert!(highlight_line("# 注释", CodeLanguage::Hash)
            .iter()
            .all(|span| span.style.fg == Some(COLOR_SYNTAX_COMMENT)));
        assert!(highlight_line("#[derive(Debug)]", CodeLanguage::CLike)
            .iter()
            .any(|span| span.style.fg == Some(COLOR_SYNTAX_TYPE)));
    }

    /// 扩展名决定语言族，未知扩展名退回纯文本。
    #[test]
    fn language_is_inferred_from_extension() {
        assert_eq!(CodeLanguage::from_path("src/main.rs"), CodeLanguage::CLike);
        assert_eq!(CodeLanguage::from_path("a/b/deploy.sh"), CodeLanguage::Hash);
        assert_eq!(CodeLanguage::from_path("Cargo.toml"), CodeLanguage::Hash);
        assert_eq!(CodeLanguage::from_path("notes"), CodeLanguage::Plain);
        assert_eq!(
            CodeLanguage::from_path("archive.tar.gz"),
            CodeLanguage::Plain
        );
    }
}

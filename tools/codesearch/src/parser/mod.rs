//! Tree-sitter 解析与定义提取。
//!
//! 每种语言只提供一个小型适配层（[`rust_lang`]、[`typescript`]），公共的遍历、
//! 文档注释绑定与签名提取逻辑放在本模块共享。
//!
//! 提取结果只描述"源码里写了什么"，不做任何跨文件解析。Tree-sitter 能看到调用表达式，
//! 但那不等于已解析的调用目标，因此本模块不产出调用图。

pub mod rust_lang;
pub mod typescript;

use tree_sitter::{Node, Parser};

use crate::error::{Error, Result};
use crate::types::{ChunkKind, Language};

pub use crate::store::types::ImportRecord;

/// 从源码中提取出的一个定义。
///
/// `start_byte` 已向前扩展到绑定的文档注释与属性/装饰器，因此代码块天然包含文档。
#[derive(Debug, Clone)]
pub struct Definition {
    /// 定义类别。
    pub kind: ChunkKind,
    /// 简单名称。
    pub name: String,
    /// 限定名称（Rust 用 `::`，TS/JS 用 `.`）。
    pub qualified: String,
    /// 父作用域符号。
    pub parent: Option<String>,
    /// 模块路径。
    pub module_path: Option<String>,
    /// 单行化的签名。
    pub signature: Option<String>,
    /// 绑定的文档注释原文。
    pub doc: Option<String>,
    /// 起始字节（含，已包含文档与属性）。
    pub start_byte: usize,
    /// 结束字节（不含）。
    pub end_byte: usize,
    /// 起始行号，从 1 开始（含）。
    pub start_line: u32,
    /// 结束行号，从 1 开始（含）。
    pub end_line: u32,
    /// 主体起始字节，用于超长定义沿语法结构拆分。
    pub body_start_byte: Option<usize>,
    /// 主体内各语句的起始字节，超长定义只在这些真实语法边界上拆分。
    pub split_points: Vec<usize>,
    /// 嵌套定义（impl 方法、类方法、模块成员）。
    pub children: Vec<Definition>,
}

impl Definition {
    /// 定义占用的字节数。
    pub fn byte_len(&self) -> usize {
        self.end_byte.saturating_sub(self.start_byte)
    }
}

/// 一次解析的结果。
#[derive(Debug, Clone, Default)]
pub struct ParsedFile {
    /// 顶层定义（含嵌套子定义）。
    pub definitions: Vec<Definition>,
    /// 导入声明。
    pub imports: Vec<ImportRecord>,
    /// 语法树是否包含错误节点。
    pub has_errors: bool,
}

impl ParsedFile {
    /// 是否没有提取到任何可用定义（调用方应退回文本分块）。
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }
}

/// 解析源码并提取定义。
///
/// 语言不支持结构化解析、或 Tree-sitter 无法建树时返回空结果，
/// 由 [`crate::chunker`] 退回文本兜底分块，文件不会因此从索引中消失。
pub fn parse(language: Language, source: &str) -> Result<ParsedFile> {
    if !language.is_structured() {
        return Ok(ParsedFile::default());
    }
    let ts_language: tree_sitter::Language = match language {
        Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        Language::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Language::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Language::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
        _ => return Ok(ParsedFile::default()),
    };
    let mut parser = Parser::new();
    parser
        .set_language(&ts_language)
        .map_err(|e| Error::Internal(format!("加载 {} 语法失败: {e}", language.as_str())))?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(ParsedFile::default());
    };
    let root = tree.root_node();
    let ctx = Context { source, language };
    let mut parsed = ParsedFile {
        has_errors: root.has_error(),
        ..Default::default()
    };
    match language {
        Language::Rust => rust_lang::extract(&ctx, root, &mut parsed),
        _ => typescript::extract(&ctx, root, &mut parsed),
    }
    Ok(parsed)
}

/// 解析上下文，供语言适配层共享。
pub struct Context<'a> {
    /// 源码文本。
    pub source: &'a str,
    /// 语言。
    pub language: Language,
}

impl<'a> Context<'a> {
    /// 取出节点对应的源码文本。
    pub fn text(&self, node: Node<'_>) -> &'a str {
        self.source.get(node.byte_range()).unwrap_or("")
    }

    /// 取出命名字段对应的文本。
    pub fn field_text(&self, node: Node<'_>, field: &str) -> Option<String> {
        node.child_by_field_name(field)
            .map(|child| self.text(child).to_string())
    }

    /// 限定名分隔符：Rust 用 `::`，TS/JS 用 `.`。
    pub fn separator(&self) -> &'static str {
        match self.language {
            Language::Rust => "::",
            _ => ".",
        }
    }

    /// 用当前作用域拼出限定名。
    pub fn qualify(&self, scope: &[String], name: &str) -> String {
        if scope.is_empty() {
            return name.to_string();
        }
        format!(
            "{}{}{}",
            scope.join(self.separator()),
            self.separator(),
            name
        )
    }

    /// 收集节点前方绑定的文档注释与属性/装饰器。
    ///
    /// 只接受紧邻的兄弟节点，并且中间不能出现空行，避免把无关注释绑定到定义上。
    pub fn leading(
        &self,
        node: Node<'_>,
        is_doc: fn(&str) -> bool,
        is_attr: fn(&str) -> bool,
    ) -> Leading {
        let mut start_byte = node.start_byte();
        let mut start_row = node.start_position().row;
        let mut docs: Vec<String> = Vec::new();
        let mut current = node;
        while let Some(previous) = current.prev_sibling() {
            let kind = previous.kind();
            let text = self.text(previous);
            let doc = is_doc(kind) && is_doc_text(text);
            let attr = is_attr(kind);
            if !doc && !attr {
                break;
            }
            // 中间隔了空行就不再绑定。
            let between = self
                .source
                .get(previous.end_byte()..start_byte)
                .unwrap_or("");
            if between.matches('\n').count() > 1 {
                break;
            }
            if doc {
                docs.push(text.to_string());
            }
            start_byte = previous.start_byte();
            start_row = previous.start_position().row;
            current = previous;
        }
        docs.reverse();
        Leading {
            start_byte,
            start_line: start_row as u32 + 1,
            doc: if docs.is_empty() {
                None
            } else {
                Some(clean_doc(&docs.join("\n")))
            },
        }
    }

    /// 收集主体内各语句的起始字节，作为超长定义唯一允许的拆分位置。
    ///
    /// 只使用真实语法节点边界，因此拆分结果不会切断语句或 UTF-8 字符。
    pub fn split_points(&self, body: Option<Node<'_>>) -> Vec<usize> {
        let Some(body) = body else {
            return Vec::new();
        };
        let mut cursor = body.walk();
        body.named_children(&mut cursor)
            .map(|child| child.start_byte())
            .collect()
    }

    /// 由定义节点与主体节点推导单行化签名。
    pub fn signature(&self, node: Node<'_>, body: Option<Node<'_>>) -> Option<String> {
        let end = body.map(|b| b.start_byte()).unwrap_or(node.end_byte());
        let end = end.max(node.start_byte()).min(self.source.len());
        let text = self.source.get(node.start_byte()..end)?;
        let signature = collapse_whitespace(text);
        let signature = signature
            .trim_end_matches(['{', '=', ';'])
            .trim()
            .to_string();
        if signature.is_empty() {
            None
        } else {
            // 签名只用于展示与模型输入，做一个保守的长度上限。
            Some(truncate_chars(&signature, 400))
        }
    }
}

/// 前置文档与属性的扫描结果。
#[derive(Debug, Clone)]
pub struct Leading {
    /// 扩展后的起始字节。
    pub start_byte: usize,
    /// 扩展后的起始行号（从 1 开始）。
    pub start_line: u32,
    /// 清理后的文档注释。
    pub doc: Option<String>,
}

/// 文本是否看起来像文档注释而不是普通注释。
pub fn is_doc_text(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("///") || text.starts_with("//!") || text.starts_with("/**")
}

/// 去掉注释标记，只保留正文。
pub fn clean_doc(raw: &str) -> String {
    let mut lines = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        let line = line
            .strip_prefix("///")
            .or_else(|| line.strip_prefix("//!"))
            .or_else(|| line.strip_prefix("/**"))
            .or_else(|| line.strip_prefix("*/"))
            .or_else(|| line.strip_prefix('*'))
            .or_else(|| line.strip_prefix("//"))
            .unwrap_or(line);
        let line = line.trim_end_matches("*/").trim();
        lines.push(line);
    }
    while lines.first().is_some_and(|line| line.is_empty()) {
        lines.remove(0);
    }
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    truncate_chars(&lines.join("\n"), 2000)
}

/// 把连续空白折叠为单个空格。
pub fn collapse_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !in_space && !out.is_empty() {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(ch);
            in_space = false;
        }
    }
    out.trim().to_string()
}

/// 按字符（而不是字节）截断，保证不会切在 UTF-8 字符中间。
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_doc_strips_comment_markers() {
        assert_eq!(clean_doc("/// 第一行\n/// 第二行"), "第一行\n第二行");
        assert_eq!(clean_doc("/**\n * Doc body\n */"), "Doc body");
        assert_eq!(clean_doc("//! 模块说明"), "模块说明");
    }

    #[test]
    fn collapse_whitespace_produces_single_line() {
        assert_eq!(
            collapse_whitespace("pub  fn   a(\n  x: u32\n)"),
            "pub fn a( x: u32 )"
        );
    }

    #[test]
    fn truncate_chars_never_splits_multibyte() {
        let text = "中文字符串";
        assert_eq!(truncate_chars(text, 2), "中文");
        assert_eq!(truncate_chars(text, 99), text);
    }

    #[test]
    fn unsupported_language_yields_empty_result() {
        let parsed = parse(Language::Markdown, "# 标题").expect("parse");
        assert!(parsed.is_empty());
        assert!(parsed.imports.is_empty());
    }
}

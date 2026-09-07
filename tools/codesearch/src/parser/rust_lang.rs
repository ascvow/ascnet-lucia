//! Rust 语法适配层。
//!
//! 节点名称依据 tree-sitter-rust 0.24 实际语法核实：
//! `function_item`、`struct_item`、`enum_item`、`trait_item`、`impl_item`、`mod_item`、
//! `const_item`、`static_item`、`type_item`、`union_item`、`macro_definition`、`use_declaration`。
//! 文档注释是包含 `doc_comment` 子节点的 `line_comment` / `block_comment`。

use tree_sitter::Node;

use crate::parser::{Context, Definition, ParsedFile};
use crate::store::ImportRecord;
use crate::types::ChunkKind;

/// 遍历 `source_file` 并提取顶层定义与导入。
pub fn extract(ctx: &Context<'_>, root: Node<'_>, parsed: &mut ParsedFile) {
    let mut scope: Vec<String> = Vec::new();
    walk(ctx, root, &mut scope, None, &mut parsed.definitions, &mut parsed.imports);
}

fn walk(
    ctx: &Context<'_>,
    parent_node: Node<'_>,
    scope: &mut Vec<String>,
    parent_symbol: Option<&str>,
    out: &mut Vec<Definition>,
    imports: &mut Vec<ImportRecord>,
) {
    let mut cursor = parent_node.walk();
    for node in parent_node.named_children(&mut cursor) {
        match node.kind() {
            "use_declaration" => {
                let raw = ctx.text(node).trim().to_string();
                let module = raw
                    .trim_start_matches("pub ")
                    .trim_start_matches("use ")
                    .trim_end_matches(';')
                    .trim()
                    .to_string();
                let range = node.byte_range();
                imports.push(ImportRecord {
                    raw,
                    module: if module.is_empty() { None } else { Some(module) },
                    start_byte: range.start,
                    end_byte: range.end,
                });
            }
            "mod_item" => {
                if let Some(definition) = module_definition(ctx, node, scope, imports) {
                    out.push(definition);
                }
            }
            "impl_item" => {
                if let Some(definition) = impl_definition(ctx, node, scope, imports) {
                    out.push(definition);
                }
            }
            "trait_item" => {
                if let Some(definition) = trait_definition(ctx, node, scope, imports) {
                    out.push(definition);
                }
            }
            "function_item" | "function_signature_item" => {
                if let Some(definition) = function_definition(ctx, node, scope, parent_symbol) {
                    out.push(definition);
                }
            }
            "struct_item" | "enum_item" | "union_item" | "const_item" | "static_item"
            | "type_item" | "macro_definition" => {
                if let Some(definition) = leaf_definition(ctx, node, scope, parent_symbol) {
                    out.push(definition);
                }
            }
            _ => {}
        }
    }
}

fn leading(ctx: &Context<'_>, node: Node<'_>) -> crate::parser::Leading {
    ctx.leading(
        node,
        |kind| matches!(kind, "line_comment" | "block_comment"),
        |kind| kind == "attribute_item",
    )
}

fn node_name(ctx: &Context<'_>, node: Node<'_>) -> Option<String> {
    ctx.field_text(node, "name")
}

fn kind_of(node: Node<'_>) -> ChunkKind {
    match node.kind() {
        "struct_item" => ChunkKind::Struct,
        "enum_item" => ChunkKind::Enum,
        "union_item" => ChunkKind::Struct,
        "const_item" | "static_item" => ChunkKind::Const,
        "type_item" => ChunkKind::TypeAlias,
        "macro_definition" => ChunkKind::Function,
        _ => ChunkKind::Function,
    }
}

fn leaf_definition(
    ctx: &Context<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_symbol: Option<&str>,
) -> Option<Definition> {
    let name = node_name(ctx, node)?;
    let body = node
        .child_by_field_name("body")
        .or_else(|| node.child_by_field_name("value"));
    let lead = leading(ctx, node);
    Some(Definition {
        kind: kind_of(node),
        qualified: ctx.qualify(scope, &name),
        name,
        parent: parent_symbol.map(str::to_string),
        module_path: module_path(scope),
        signature: ctx.signature(node, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: node.end_byte(),
        start_line: lead.start_line,
        end_line: node.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        children: Vec::new(),
    })
}

fn function_definition(
    ctx: &Context<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_symbol: Option<&str>,
) -> Option<Definition> {
    let name = node_name(ctx, node)?;
    let body = node.child_by_field_name("body");
    let lead = leading(ctx, node);
    Some(Definition {
        // 位于 impl / trait 内部的函数记为方法。
        kind: if parent_symbol.is_some() {
            ChunkKind::Method
        } else {
            ChunkKind::Function
        },
        qualified: ctx.qualify(scope, &name),
        name,
        parent: parent_symbol.map(str::to_string),
        module_path: module_path(scope),
        signature: ctx.signature(node, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: node.end_byte(),
        start_line: lead.start_line,
        end_line: node.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        children: Vec::new(),
    })
}

fn module_definition(
    ctx: &Context<'_>,
    node: Node<'_>,
    scope: &mut Vec<String>,
    imports: &mut Vec<ImportRecord>,
) -> Option<Definition> {
    let name = node_name(ctx, node)?;
    let body = node.child_by_field_name("body");
    let lead = leading(ctx, node);
    let qualified = ctx.qualify(scope, &name);
    let mut children = Vec::new();
    if let Some(body) = body {
        scope.push(name.clone());
        walk(ctx, body, scope, None, &mut children, imports);
        scope.pop();
    }
    Some(Definition {
        kind: ChunkKind::Module,
        name,
        parent: None,
        module_path: module_path(scope),
        signature: ctx.signature(node, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: node.end_byte(),
        start_line: lead.start_line,
        end_line: node.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        qualified,
        children,
    })
}

fn impl_definition(
    ctx: &Context<'_>,
    node: Node<'_>,
    scope: &mut Vec<String>,
    imports: &mut Vec<ImportRecord>,
) -> Option<Definition> {
    // `impl Trait for Type` 以 Type 为作用域名，同时在签名中保留完整形式。
    let type_name = ctx.field_text(node, "type")?;
    let trait_name = ctx.field_text(node, "trait");
    let body = node.child_by_field_name("body");
    let lead = leading(ctx, node);
    let scope_name = simple_type_name(&type_name);
    let qualified = ctx.qualify(scope, &scope_name);
    let mut children = Vec::new();
    if let Some(body) = body {
        scope.push(scope_name.clone());
        walk(ctx, body, scope, Some(&qualified), &mut children, imports);
        scope.pop();
    }
    Some(Definition {
        kind: ChunkKind::Impl,
        name: match &trait_name {
            Some(trait_name) => format!("{trait_name} for {type_name}"),
            None => type_name.clone(),
        },
        parent: None,
        module_path: module_path(scope),
        signature: ctx.signature(node, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: node.end_byte(),
        start_line: lead.start_line,
        end_line: node.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        qualified,
        children,
    })
}

fn trait_definition(
    ctx: &Context<'_>,
    node: Node<'_>,
    scope: &mut Vec<String>,
    imports: &mut Vec<ImportRecord>,
) -> Option<Definition> {
    let name = node_name(ctx, node)?;
    let body = node.child_by_field_name("body");
    let lead = leading(ctx, node);
    let qualified = ctx.qualify(scope, &name);
    let mut children = Vec::new();
    if let Some(body) = body {
        scope.push(name.clone());
        walk(ctx, body, scope, Some(&qualified), &mut children, imports);
        scope.pop();
    }
    Some(Definition {
        kind: ChunkKind::Trait,
        name,
        parent: None,
        module_path: module_path(scope),
        signature: ctx.signature(node, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: node.end_byte(),
        start_line: lead.start_line,
        end_line: node.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        qualified,
        children,
    })
}

/// 去掉泛型参数，取出用作作用域名的类型名。
fn simple_type_name(type_text: &str) -> String {
    let base = type_text.split('<').next().unwrap_or(type_text).trim();
    base.rsplit("::").next().unwrap_or(base).trim().to_string()
}

fn module_path(scope: &[String]) -> Option<String> {
    if scope.is_empty() {
        None
    } else {
        Some(scope.join("::"))
    }
}

#[cfg(test)]
mod tests {
    use crate::parser::parse;
    use crate::types::{ChunkKind, Language};

    const SOURCE: &str = r#"//! 模块说明

use std::io::Read;
use crate::error::Error;

/// 重试策略。
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
}

impl RetryPolicy {
    /// 执行请求并在失败后重试。
    #[inline]
    pub async fn execute(&self, request: Request) -> Result<Response, Error> {
        loop {
            break;
        }
        Ok(Response)
    }

    fn backoff(&self, attempt: u32) -> Duration {
        Duration::from_millis(100)
    }
}

pub trait Retryable {
    /// 是否值得重试。
    fn should_retry(&self) -> bool;
}

pub mod inner {
    pub fn nested_helper() {}
}

pub const MAX_ATTEMPTS: u32 = 3;
pub type Attempts = u32;
pub enum Outcome { Success, Failure }
"#;

    #[test]
    fn extracts_rust_definitions_with_docs_and_signatures() {
        let parsed = parse(Language::Rust, SOURCE).expect("parse");
        assert!(!parsed.has_errors, "示例代码应能完整解析");

        let names: Vec<&str> = parsed.definitions.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "RetryPolicy",
                "RetryPolicy",
                "Retryable",
                "inner",
                "MAX_ATTEMPTS",
                "Attempts",
                "Outcome",
            ]
        );

        let structure = &parsed.definitions[0];
        assert_eq!(structure.kind, ChunkKind::Struct);
        assert_eq!(structure.doc.as_deref(), Some("重试策略。"));
        // 起始位置必须包含文档注释与属性。
        let text = &SOURCE[structure.start_byte..structure.end_byte];
        assert!(text.starts_with("/// 重试策略。"), "块应包含文档注释: {text}");
        assert!(text.contains("#[derive(Debug, Clone)]"), "块应包含属性");

        let impl_block = &parsed.definitions[1];
        assert_eq!(impl_block.kind, ChunkKind::Impl);
        assert_eq!(impl_block.children.len(), 2);
        let execute = &impl_block.children[0];
        assert_eq!(execute.kind, ChunkKind::Method);
        assert_eq!(execute.qualified, "RetryPolicy::execute");
        assert_eq!(execute.parent.as_deref(), Some("RetryPolicy"));
        assert_eq!(execute.doc.as_deref(), Some("执行请求并在失败后重试。"));
        assert_eq!(
            execute.signature.as_deref(),
            Some("pub async fn execute(&self, request: Request) -> Result<Response, Error>")
        );
        // 属性也要绑定进方法块。
        assert!(SOURCE[execute.start_byte..execute.end_byte].contains("#[inline]"));

        let module = &parsed.definitions[3];
        assert_eq!(module.kind, ChunkKind::Module);
        assert_eq!(module.children[0].qualified, "inner::nested_helper");
        assert_eq!(module.children[0].module_path.as_deref(), Some("inner"));

        let trait_def = &parsed.definitions[2];
        assert_eq!(trait_def.kind, ChunkKind::Trait);
        assert_eq!(trait_def.children[0].qualified, "Retryable::should_retry");
    }

    #[test]
    fn extracts_imports() {
        let parsed = parse(Language::Rust, SOURCE).expect("parse");
        let modules: Vec<&str> = parsed
            .imports
            .iter()
            .filter_map(|i| i.module.as_deref())
            .collect();
        assert_eq!(modules, vec!["std::io::Read", "crate::error::Error"]);
    }

    #[test]
    fn line_numbers_are_one_based_and_match_content() {
        let parsed = parse(Language::Rust, SOURCE).expect("parse");
        for definition in &parsed.definitions {
            let text = &SOURCE[definition.start_byte..definition.end_byte];
            let expected_lines = text.lines().count() as u32;
            assert_eq!(
                definition.end_line - definition.start_line + 1,
                expected_lines,
                "定义 {} 的行号范围与内容不一致",
                definition.name
            );
            // 起始行的实际文本必须与切片首行一致。
            let file_line = SOURCE
                .lines()
                .nth(definition.start_line as usize - 1)
                .expect("行存在");
            assert_eq!(file_line, text.lines().next().expect("首行"));
        }
    }

    #[test]
    fn trait_impl_keeps_both_names() {
        let source = "impl std::fmt::Display for RetryPolicy<T> {\n    fn fmt(&self) {}\n}\n";
        let parsed = parse(Language::Rust, source).expect("parse");
        let impl_block = &parsed.definitions[0];
        assert_eq!(impl_block.name, "std::fmt::Display for RetryPolicy<T>");
        // 作用域名去掉泛型与路径前缀。
        assert_eq!(impl_block.qualified, "RetryPolicy");
        assert_eq!(impl_block.children[0].qualified, "RetryPolicy::fmt");
    }

    #[test]
    fn unrelated_comment_separated_by_blank_line_is_not_bound() {
        let source = "// 与定义无关的说明\n\n/// 真正的文档\npub fn target() {}\n";
        let parsed = parse(Language::Rust, source).expect("parse");
        let definition = &parsed.definitions[0];
        assert_eq!(definition.doc.as_deref(), Some("真正的文档"));
        assert!(!source[definition.start_byte..definition.end_byte].contains("无关"));
    }

    #[test]
    fn broken_source_still_reports_recoverable_definitions() {
        let source = "pub fn good() {}\n\npub fn broken( {\n";
        let parsed = parse(Language::Rust, source).expect("parse");
        assert!(parsed.has_errors, "语法错误必须被报告");
        assert!(
            parsed.definitions.iter().any(|d| d.name == "good"),
            "错误之外的定义仍应可用"
        );
    }
}

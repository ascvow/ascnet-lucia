//! Elixir 的声明以宏调用表示；只识别语言声明宏，不把任意 Phoenix/Ecto 调用当作函数。

use tree_sitter::{Node, Parser};

use crate::error::{Error, Result};
use crate::parser::{Context, Definition, ParsedFile};
use crate::types::{ChunkKind, Language};

/// 提取 Elixir 模块、协议、实现、函数与宏；语法加载失败返回错误，残缺源码保留可用声明。
pub fn parse(source: &str) -> Result<ParsedFile> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_elixir::LANGUAGE.into())
        .map_err(|error| Error::Internal(format!("加载 Elixir 语法失败: {error}")))?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(ParsedFile::default());
    };
    let ctx = Context {
        source,
        language: Language::Elixir,
    };
    Ok(ParsedFile {
        definitions: declarations(&ctx, tree.root_node(), &[]),
        has_errors: tree.root_node().has_error(),
        ..Default::default()
    })
}

/// 只遍历模块级声明；函数体内普通调用及其局部变量不进入公共符号表。
fn declarations(ctx: &Context<'_>, root: Node<'_>, scope: &[String]) -> Vec<Definition> {
    let mut cursor = root.walk();
    let definitions = root
        .named_children(&mut cursor)
        .filter_map(|node| definition(ctx, node, scope))
        .collect();
    definitions
}

/// 声明宏的第一个参数是签名；守卫只剥离左侧，名称不得来自守卫中的函数调用。
fn definition(ctx: &Context<'_>, node: Node<'_>, scope: &[String]) -> Option<Definition> {
    if node.kind() != "call" {
        return None;
    }
    let target = node.child_by_field_name("target")?;
    let (kind, container) = match ctx.text(target) {
        "defmodule" => (ChunkKind::Module, true),
        "defprotocol" => (ChunkKind::Trait, true),
        "defimpl" => (ChunkKind::Impl, true),
        "def" | "defp" | "defmacro" | "defmacrop" | "defdelegate" => (ChunkKind::Function, false),
        _ => return None,
    };
    let mut cursor = node.walk();
    let arguments = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "arguments")?;
    let mut head = arguments.named_child(0)?;
    if head.kind() == "binary_operator"
        && ctx.field_text(head, "operator").as_deref() == Some("when")
    {
        head = head.child_by_field_name("left")?;
    }
    let name_node = if head.kind() == "call" {
        head.child_by_field_name("target")?
    } else {
        head
    };
    if !matches!(
        name_node.kind(),
        "identifier" | "alias" | "atom" | "operator_identifier"
    ) {
        return None;
    }
    let name = ctx.text(name_node).to_string();
    let qualified = ctx.qualify(scope, &name);
    let mut cursor = node.walk();
    let body = node
        .named_children(&mut cursor)
        .find(|child| child.kind() == "do_block");
    let mut children = Vec::new();
    if container {
        if let Some(body) = body {
            let mut inner_scope = scope.to_vec();
            inner_scope.push(name.clone());
            children = declarations(ctx, body, &inner_scope);
        }
    }
    let mut lead = ctx.leading_with(node, &["comment"], &[], &[]);
    // @doc 与 @spec 属于当前声明，保留相邻属性及原始坐标，不吞掉上一条定义。
    let mut current = node;
    while let Some(previous) = current.prev_named_sibling() {
        if previous.kind() != "unary_operator"
            || ctx.field_text(previous, "operator").as_deref() != Some("@")
        {
            break;
        }
        let operand = previous.child_by_field_name("operand")?;
        if !matches!(
            ctx.field_text(operand, "target").as_deref(),
            Some("doc" | "spec" | "impl" | "deprecated")
        ) {
            break;
        }
        if ctx.source[previous.end_byte()..current.start_byte()]
            .matches('\n')
            .count()
            > 1
        {
            break;
        }
        lead.start_byte = previous.start_byte();
        lead.start_line = previous.start_position().row as u32 + 1;
        current = previous;
    }
    Some(Definition {
        kind,
        name,
        qualified,
        parent: (!scope.is_empty()).then(|| scope.join(".")),
        module_path: (!scope.is_empty()).then(|| scope.join(".")),
        signature: ctx.signature(node, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: node.end_byte(),
        start_line: lead.start_line,
        end_line: node.end_position().row as u32 + 1,
        body_start_byte: body.map(|body| body.start_byte()),
        split_points: ctx.split_points(body),
        children,
    })
}

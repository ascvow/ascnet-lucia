//! Go 结构化解析，基于 `gosyn`。
//!
//! `gosyn` 的节点只带起始偏移，没有结束偏移；函数体的 `BlockStmt.pos` 是 `(左花括号, 右花括号)`
//! 的一对偏移，因此函数能拿到精确结束位置。类型与常量声明没有对应信息，结束行退化为
//! 声明起始行——这对检索足够：符号定位靠起始行，正文按需重读。

use super::{Declaration, LineIndex, ParseOutput};
use crate::model::SymbolKind;
use gosyn::ast;
use std::rc::Rc;

/// 解析 Go 源码。
///
/// 语法错误时返回 `None`，由调用方降级为文本分块。
pub fn parse(source: &str) -> Option<ParseOutput> {
    let file = gosyn::parse_source(source).ok()?;
    let index = LineIndex::new(source);
    let mut output = ParseOutput::default();

    for import in &file.imports {
        push_import(&mut output, import.path.value.trim_matches('"').to_string());
    }

    for declaration in &file.decl {
        match declaration {
            ast::Declaration::Function(node) => walk_function(node, &index, &mut output),
            ast::Declaration::Type(node) => {
                for spec in &node.specs {
                    walk_type(spec, &index, &mut output);
                }
            }
            ast::Declaration::Const(node) => {
                for spec in &node.specs {
                    for name in &spec.name {
                        push(
                            &mut output,
                            &name.name,
                            SymbolKind::Constant,
                            name.pos,
                            name.pos,
                            &index,
                            None,
                            docs_of(&spec.docs),
                            None,
                        );
                    }
                }
            }
            ast::Declaration::Variable(node) => {
                for spec in &node.specs {
                    for name in &spec.name {
                        push(
                            &mut output,
                            &name.name,
                            SymbolKind::Constant,
                            name.pos,
                            name.pos,
                            &index,
                            None,
                            docs_of(&spec.docs),
                            None,
                        );
                    }
                }
            }
        }
    }
    Some(output)
}

/// 处理函数与方法声明。
fn walk_function(node: &ast::FuncDecl, index: &LineIndex, output: &mut ParseOutput) {
    // 方法的接收者类型是它的检索归属：`func (r *Repo) Get()` 应当能通过 `Repo.Get` 找到。
    let owner = node.recv.as_ref().and_then(receiver_type);
    let end = node.body.as_ref().map_or(node.name.pos, |body| body.pos.1);
    push(
        output,
        &node.name.name,
        SymbolKind::Function,
        node.typ.pos.min(node.name.pos),
        end,
        index,
        None,
        docs_of(&node.docs),
        owner,
    );
}

/// 处理类型声明，按底层类型区分结构体、接口与别名。
fn walk_type(spec: &ast::TypeSpec, index: &LineIndex, output: &mut ParseOutput) {
    let (kind, end) = match &spec.typ {
        ast::Expression::TypeStruct(node) => (SymbolKind::Struct, node.pos.1),
        ast::Expression::TypeInterface(node) => (SymbolKind::Trait, node.pos),
        _ => (SymbolKind::TypeAlias, spec.name.pos),
    };
    push(
        output,
        &spec.name.name,
        kind,
        spec.name.pos,
        end.max(spec.name.pos),
        index,
        None,
        docs_of(&spec.docs),
        None,
    );
}

/// 取接收者的类型名，剥掉指针与泛型参数。
fn receiver_type(fields: &ast::FieldList) -> Option<String> {
    let field = fields.list.first()?;
    type_name(&field.typ)
}

/// 取表达式的可检索类型名。
fn type_name(expression: &ast::Expression) -> Option<String> {
    match expression {
        ast::Expression::Ident(ident) => Some(ident.name.clone()),
        ast::Expression::Star(node) => type_name(&node.right),
        ast::Expression::Paren(node) => type_name(&node.expr),
        ast::Expression::TypePointer(node) => type_name(&node.typ),
        ast::Expression::Index(node) => type_name(&node.left),
        ast::Expression::IndexList(node) => type_name(&node.left),
        ast::Expression::Selector(node) => Some(node.sel.name.clone()),
        _ => None,
    }
}

/// 把 Go 的文档注释拼成一行。
fn docs_of(docs: &[Rc<ast::Comment>]) -> Option<String> {
    let text = docs
        .iter()
        .map(|comment| {
            comment
                .text
                .trim_start_matches("/*")
                .trim_end_matches("*/")
                .trim_start_matches("//")
                .trim()
        })
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!text.is_empty()).then_some(text)
}

/// 记录一个声明。
#[allow(clippy::too_many_arguments)]
fn push(
    output: &mut ParseOutput,
    name: &str,
    kind: SymbolKind,
    start: usize,
    end: usize,
    index: &LineIndex,
    parent: Option<usize>,
    doc_comment: Option<String>,
    owner: Option<String>,
) {
    let start_line = index.line_of(start);
    output.declarations.push(Declaration {
        name: name.to_string(),
        kind,
        start_line,
        end_line: index.line_of(end).max(start_line),
        signature: None,
        doc_comment,
        parent,
        owner,
    });
}

/// 追加一条 import，去重。
fn push_import(output: &mut ParseOutput, text: String) {
    if !text.is_empty() && !output.imports.contains(&text) {
        output.imports.push(text);
    }
}

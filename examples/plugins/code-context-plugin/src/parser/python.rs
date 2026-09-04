//! Python 结构化解析，基于 `rustpython-parser`。

use super::{Declaration, LineIndex, ParseOutput};
use crate::model::SymbolKind;
use rustpython_parser::ast::{self, Ranged};
use rustpython_parser::Parse;

/// 解析 Python 源码。
///
/// 语法错误时返回 `None`，由调用方降级为文本分块。
pub fn parse(source: &str) -> Option<ParseOutput> {
    let suite = ast::Suite::parse(source, "<repository>").ok()?;
    let index = LineIndex::new(source);
    let mut output = ParseOutput::default();
    // 模块开头的字符串字面量就是模块 docstring，描述整个文件的职责。
    output.file_doc = docstring(&suite);
    walk_body(&suite, None, &index, &mut output);
    Some(output)
}

/// 遍历一个语句块。
fn walk_body(
    body: &[ast::Stmt],
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    for statement in body {
        walk_statement(statement, parent, index, output);
    }
}

/// 处理一条语句。
fn walk_statement(
    statement: &ast::Stmt,
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    match statement {
        ast::Stmt::FunctionDef(node) => {
            let current = push(
                output,
                node.name.as_str(),
                SymbolKind::Function,
                node.range(),
                index,
                parent,
                docstring(&node.body),
            );
            walk_body(&node.body, Some(current), index, output);
        }
        ast::Stmt::AsyncFunctionDef(node) => {
            let current = push(
                output,
                node.name.as_str(),
                SymbolKind::Function,
                node.range(),
                index,
                parent,
                docstring(&node.body),
            );
            walk_body(&node.body, Some(current), index, output);
        }
        ast::Stmt::ClassDef(node) => {
            let current = push(
                output,
                node.name.as_str(),
                SymbolKind::Struct,
                node.range(),
                index,
                parent,
                docstring(&node.body),
            );
            walk_body(&node.body, Some(current), index, output);
        }
        // 模块级赋值只有全大写名才当作常量：Python 没有 const，全大写是社区通行的约定，
        // 而把每个模块级变量都收进符号表会让符号检索被噪声淹没。
        ast::Stmt::Assign(node) if parent.is_none() => {
            for target in &node.targets {
                if let ast::Expr::Name(name) = target {
                    if is_constant_name(name.id.as_str()) {
                        push(
                            output,
                            name.id.as_str(),
                            SymbolKind::Constant,
                            node.range(),
                            index,
                            parent,
                            None,
                        );
                    }
                }
            }
        }
        ast::Stmt::AnnAssign(node) if parent.is_none() => {
            if let ast::Expr::Name(name) = node.target.as_ref() {
                if is_constant_name(name.id.as_str()) {
                    push(
                        output,
                        name.id.as_str(),
                        SymbolKind::Constant,
                        node.range(),
                        index,
                        parent,
                        None,
                    );
                }
            }
        }
        ast::Stmt::Import(node) => {
            for alias in &node.names {
                push_import(output, alias.name.to_string());
            }
        }
        ast::Stmt::ImportFrom(node) => {
            let module = node.module.as_ref().map_or("", |name| name.as_str());
            for alias in &node.names {
                push_import(output, format!("{module}.{}", alias.name));
            }
        }
        _ => {}
    }
}

/// 判断名称是否符合常量约定（全大写）。
fn is_constant_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|item| item.is_ascii_uppercase() || item.is_ascii_digit() || item == '_')
        && name.chars().any(|item| item.is_ascii_uppercase())
}

/// 提取函数或类的文档字符串。
///
/// Python 的文档不在注释里，而是函数体第一条语句的字符串字面量。
fn docstring(body: &[ast::Stmt]) -> Option<String> {
    let ast::Stmt::Expr(first) = body.first()? else {
        return None;
    };
    let ast::Expr::Constant(constant) = first.value.as_ref() else {
        return None;
    };
    let ast::Constant::Str(text) = &constant.value else {
        return None;
    };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

/// 记录一个声明，返回它在结果数组中的下标。
fn push(
    output: &mut ParseOutput,
    name: &str,
    kind: SymbolKind,
    range: ast::text_size::TextRange,
    index: &LineIndex,
    parent: Option<usize>,
    doc_comment: Option<String>,
) -> usize {
    let start = usize::from(range.start());
    let end = usize::from(range.end()).saturating_sub(1);
    output.declarations.push(Declaration {
        name: name.to_string(),
        kind,
        start_line: index.line_of(start),
        end_line: index.line_of(end),
        signature: None,
        doc_comment,
        parent,
        owner: None,
        // Python 的测试约定在文件名与目录上（`test_*.py`、`tests/`），
        // 由 `is_test_path` 统一判定。
        is_test: false,
    });
    output.declarations.len() - 1
}

/// 追加一条 import，去重。
fn push_import(output: &mut ParseOutput, text: String) {
    if !output.imports.contains(&text) {
        output.imports.push(text);
    }
}

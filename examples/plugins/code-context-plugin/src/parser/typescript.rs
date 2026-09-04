//! TypeScript / JavaScript 结构化解析，基于 `oxc_parser`。
//!
//! `oxc` 是容错解析器：语法错误时仍会返回一棵尽力构造的 AST，并把错误放在 `errors` 里。
//! 这正是代码检索需要的行为——**编辑中的半成品文件依然应该可被检索**，因此这里不因为存在
//! 语法错误就丢弃结果，只在解析器明确报告 AST 不可用（`panicked`）时降级。

use super::{Declaration, LineIndex, ParseOutput};
use crate::model::SymbolKind;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Class, ClassElement, Declaration as OxcDeclaration, ExportDefaultDeclarationKind, Expression,
    Statement, TSInterfaceDeclaration, TSNamespaceDeclaration, TSNamespaceDeclarationBody,
    VariableDeclaration,
};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};

/// 解析 TypeScript 或 JavaScript 源码。
///
/// `typescript` 为真时按 TS 解析；两者共用同一套 AST，因此只有 `SourceType` 不同。
/// JSX 一律开启：`.js` 里写 JSX 在 React 项目中很常见，关掉会让整份文件解析失败。
pub fn parse(source: &str, typescript: bool) -> Option<ParseOutput> {
    let allocator = Allocator::default();
    let source_type = SourceType::default()
        .with_typescript(typescript)
        .with_jsx(true)
        .with_module(true);
    let result = Parser::new(&allocator, source, source_type).parse();
    if result.panicked {
        return None;
    }

    let index = LineIndex::new(source);
    let mut output = ParseOutput {
        file_doc: super::leading_doc_comment(source),
        ..ParseOutput::default()
    };
    for statement in &result.program.body {
        walk_statement(statement, None, &index, &mut output);
    }
    Some(output)
}

/// 处理一条语句。
fn walk_statement(
    statement: &Statement<'_>,
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    match statement {
        // `export <decl>` 与 `export default <decl>` 只是包装，实际声明在内部。
        Statement::ExportDeclaration(node) => {
            walk_declaration(&node.declaration, parent, index, output);
        }
        Statement::ExportDefaultDeclaration(node) => match &node.declaration {
            ExportDefaultDeclarationKind::FunctionDeclaration(function) => {
                if let Some(id) = function.id.as_ref() {
                    push(
                        output,
                        id.name.as_str(),
                        SymbolKind::Function,
                        function.span,
                        index,
                        parent,
                        None,
                    );
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                walk_class(class, parent, index, output);
            }
            _ => {}
        },
        Statement::ImportDeclaration(node) => {
            push_import(output, node.source.value.as_str());
        }
        // `Declaration` 的各变体同时内联在 `Statement` 上，因此未被 export 包装的顶层声明
        // 直接以语句形式出现，需要在这里逐一接住。
        Statement::FunctionDeclaration(node) => {
            if let Some(id) = node.id.as_ref() {
                push(
                    output,
                    id.name.as_str(),
                    SymbolKind::Function,
                    node.span,
                    index,
                    parent,
                    None,
                );
            }
        }
        Statement::ClassDeclaration(node) => walk_class(node, parent, index, output),
        Statement::TSInterfaceDeclaration(node) => walk_interface(node, parent, index, output),
        Statement::TSTypeAliasDeclaration(node) => {
            push(
                output,
                node.id.name.as_str(),
                SymbolKind::TypeAlias,
                node.span,
                index,
                parent,
                None,
            );
        }
        Statement::TSEnumDeclaration(node) => {
            push(
                output,
                node.id.name.as_str(),
                SymbolKind::Enum,
                node.span,
                index,
                parent,
                None,
            );
        }
        Statement::TSNamespaceDeclaration(node) => walk_namespace(node, parent, index, output),
        Statement::VariableDeclaration(node) => walk_variables(node, parent, index, output),
        _ => {}
    }
}

/// 处理 `namespace` / `module` 声明。
///
/// `namespace A.B.C {}` 在 AST 里是嵌套的命名空间节点，因此 body 需要递归展开，
/// 这样 `A.B.C` 里的符号才能得到完整限定名。
fn walk_namespace(
    node: &TSNamespaceDeclaration<'_>,
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    let module = push(
        output,
        node.id.name.as_str(),
        SymbolKind::Module,
        node.span,
        index,
        parent,
        None,
    );
    match &node.body {
        TSNamespaceDeclarationBody::TSNamespaceDeclaration(inner) => {
            walk_namespace(inner, Some(module), index, output);
        }
        TSNamespaceDeclarationBody::TSModuleBlock(block) => {
            for statement in &block.body {
                walk_statement(statement, Some(module), index, output);
            }
        }
    }
}

/// 处理一个声明。
fn walk_declaration(
    declaration: &OxcDeclaration<'_>,
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    match declaration {
        OxcDeclaration::FunctionDeclaration(node) => {
            if let Some(id) = node.id.as_ref() {
                push(
                    output,
                    id.name.as_str(),
                    SymbolKind::Function,
                    node.span,
                    index,
                    parent,
                    None,
                );
            }
        }
        OxcDeclaration::ClassDeclaration(node) => walk_class(node, parent, index, output),
        OxcDeclaration::TSInterfaceDeclaration(node) => walk_interface(node, parent, index, output),
        OxcDeclaration::TSTypeAliasDeclaration(node) => {
            push(
                output,
                node.id.name.as_str(),
                SymbolKind::TypeAlias,
                node.span,
                index,
                parent,
                None,
            );
        }
        OxcDeclaration::TSEnumDeclaration(node) => {
            push(
                output,
                node.id.name.as_str(),
                SymbolKind::Enum,
                node.span,
                index,
                parent,
                None,
            );
        }
        OxcDeclaration::TSNamespaceDeclaration(node) => {
            walk_namespace(node, parent, index, output);
        }
        OxcDeclaration::VariableDeclaration(node) => {
            walk_variables(node, parent, index, output);
        }
        _ => {}
    }
}

/// 处理变量声明。
///
/// 箭头函数与函数表达式赋值是 TS/JS 里最主流的函数定义形态，必须归类为函数——
/// 否则"某某函数在哪"这类查询会在现代代码库上大面积落空。
fn walk_variables(
    node: &VariableDeclaration<'_>,
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    for declarator in &node.declarations {
        let Some(name) = declarator.id.get_identifier_name() else {
            continue;
        };
        let kind = match declarator.init.as_ref() {
            Some(Expression::ArrowFunctionExpression(_))
            | Some(Expression::FunctionExpression(_)) => SymbolKind::Function,
            Some(Expression::ClassExpression(_)) => SymbolKind::Struct,
            _ => SymbolKind::Constant,
        };
        push(
            output,
            name.as_str(),
            kind,
            declarator.span,
            index,
            parent,
            None,
        );
    }
}

/// 处理类声明及其成员。
fn walk_class(
    node: &Class<'_>,
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    let Some(id) = node.id.as_ref() else {
        return;
    };
    let class = push(
        output,
        id.name.as_str(),
        SymbolKind::Struct,
        node.span,
        index,
        parent,
        None,
    );
    for element in &node.body.body {
        match element {
            ClassElement::MethodDefinition(method) => {
                if let Some(name) = method.key.name() {
                    push(
                        output,
                        name.as_ref(),
                        SymbolKind::Function,
                        method.span,
                        index,
                        Some(class),
                        None,
                    );
                }
            }
            ClassElement::PropertyDefinition(property) => {
                // 箭头函数属性同样是方法的常见写法。
                let Some(name) = property.key.name() else {
                    continue;
                };
                let kind = match property.value.as_ref() {
                    Some(Expression::ArrowFunctionExpression(_))
                    | Some(Expression::FunctionExpression(_)) => SymbolKind::Function,
                    _ => continue,
                };
                push(
                    output,
                    name.as_ref(),
                    kind,
                    property.span,
                    index,
                    Some(class),
                    None,
                );
            }
            _ => {}
        }
    }
}

/// 处理接口声明及其方法签名。
fn walk_interface(
    node: &TSInterfaceDeclaration<'_>,
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    let interface = push(
        output,
        node.id.name.as_str(),
        SymbolKind::Trait,
        node.span,
        index,
        parent,
        None,
    );
    for member in &node.body.body {
        let (name, span) = match member {
            oxc_ast::ast::TSSignature::TSMethodSignature(method) => {
                (method.key.name(), method.span)
            }
            oxc_ast::ast::TSSignature::TSPropertySignature(property) => {
                (property.key.name(), property.span)
            }
            _ => continue,
        };
        if let Some(name) = name {
            push(
                output,
                name.as_ref(),
                SymbolKind::Function,
                span,
                index,
                Some(interface),
                None,
            );
        }
    }
}

/// 记录一个声明，返回它在结果数组中的下标。
fn push(
    output: &mut ParseOutput,
    name: &str,
    kind: SymbolKind,
    span: Span,
    index: &LineIndex,
    parent: Option<usize>,
    owner: Option<String>,
) -> usize {
    output.declarations.push(Declaration {
        name: name.to_string(),
        kind,
        start_line: index.line_of(span.start as usize),
        end_line: index.line_of(span.end.saturating_sub(1) as usize),
        signature: None,
        // oxc 默认不把注释挂到节点上；文档注释由上层按源码行回溯补齐。
        doc_comment: None,
        parent,
        owner,
        // 这些语言的测试约定体现在文件名上（`*_test.go`、`*.test.ts`、`test_*.py`），
        // 由 `is_test_path` 统一判定，声明层不再重复判断。
        is_test: false,
    });
    output.declarations.len() - 1
}

/// 追加一条 import，去重。
fn push_import(output: &mut ParseOutput, source: &str) {
    let text = source.to_string();
    if !output.imports.contains(&text) {
        output.imports.push(text);
    }
}

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
    // Oxc 已标注注释归属；只取与声明首行相连的前导注释，避免吸收上一函数的尾注释。
    let mut comments_by_line: std::collections::BTreeMap<u32, Vec<&str>> =
        std::collections::BTreeMap::new();
    for comment in result
        .program
        .comments
        .iter()
        .filter(|comment| comment.is_leading())
    {
        if let Some(text) = source.get(comment.span.start as usize..comment.span.end as usize) {
            comments_by_line
                .entry(index.line_of(comment.attached_to as usize))
                .or_default()
                .push(text);
        }
    }
    for declaration in &mut output.declarations {
        if let Some(comments) = comments_by_line.remove(&declaration.start_line) {
            declaration.doc_comment = Some(comments.join("\n"));
        }
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
                let owner = push(
                    output,
                    function
                        .id
                        .as_ref()
                        .map_or("default", |id| id.name.as_str()),
                    SymbolKind::Function,
                    function.span,
                    index,
                    parent,
                    None,
                );
                if let Some(body) = &function.body {
                    for statement in &body.statements {
                        walk_statement(statement, Some(owner), index, output);
                    }
                }
            }
            ExportDefaultDeclarationKind::ClassDeclaration(class) => {
                walk_class(class, parent, index, output);
            }
            declaration => {
                if let Some(expression) = declaration.as_expression() {
                    if matches!(expression.get_inner_expression(), Expression::ObjectExpression(object) if !object_has_methods(object))
                    {
                        return;
                    }
                    let kind = if matches!(
                        expression.get_inner_expression(),
                        Expression::ArrowFunctionExpression(_) | Expression::FunctionExpression(_)
                    ) {
                        SymbolKind::Function
                    } else {
                        SymbolKind::Module
                    };
                    let default = push(output, "default", kind, node.span, index, parent, None);
                    walk_expression(expression, Some(default), index, output);
                }
            }
        },
        Statement::ImportDeclaration(node) => {
            push_import(output, node.source.value.as_str());
        }
        // `Declaration` 的各变体同时内联在 `Statement` 上，因此未被 export 包装的顶层声明
        // 直接以语句形式出现，需要在这里逐一接住。
        Statement::FunctionDeclaration(node) => {
            if let Some(id) = node.id.as_ref() {
                let function = push(
                    output,
                    id.name.as_str(),
                    SymbolKind::Function,
                    node.span,
                    index,
                    parent,
                    None,
                );
                if let Some(body) = &node.body {
                    for statement in &body.statements {
                        walk_statement(statement, Some(function), index, output);
                    }
                }
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
        Statement::BlockStatement(node) => {
            for statement in &node.body {
                walk_statement(statement, parent, index, output);
            }
        }
        Statement::ExpressionStatement(node) => {
            walk_expression(&node.expression, parent, index, output)
        }
        Statement::IfStatement(node) => {
            walk_statement(&node.consequent, parent, index, output);
            if let Some(alternate) = &node.alternate {
                walk_statement(alternate, parent, index, output);
            }
        }
        Statement::TryStatement(node) => {
            for statement in &node.block.body {
                walk_statement(statement, parent, index, output);
            }
            if let Some(handler) = &node.handler {
                for statement in &handler.body.body {
                    walk_statement(statement, parent, index, output);
                }
            }
            if let Some(finalizer) = &node.finalizer {
                for statement in &finalizer.body {
                    walk_statement(statement, parent, index, output);
                }
            }
        }
        Statement::ForStatement(node) => walk_statement(&node.body, parent, index, output),
        Statement::ForOfStatement(node) => walk_statement(&node.body, parent, index, output),
        Statement::ForInStatement(node) => walk_statement(&node.body, parent, index, output),
        Statement::WhileStatement(node) => walk_statement(&node.body, parent, index, output),
        Statement::DoWhileStatement(node) => walk_statement(&node.body, parent, index, output),
        Statement::SwitchStatement(node) => {
            for case in &node.cases {
                for statement in &case.consequent {
                    walk_statement(statement, parent, index, output);
                }
            }
        }
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
                let function = push(
                    output,
                    id.name.as_str(),
                    SymbolKind::Function,
                    node.span,
                    index,
                    parent,
                    None,
                );
                if let Some(body) = &node.body {
                    for statement in &body.statements {
                        walk_statement(statement, Some(function), index, output);
                    }
                }
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
        let value = declarator
            .init
            .as_ref()
            .map(Expression::get_inner_expression);
        let kind = match value {
            Some(Expression::ArrowFunctionExpression(_))
            | Some(Expression::FunctionExpression(_)) => SymbolKind::Function,
            Some(Expression::ClassExpression(_)) => SymbolKind::Struct,
            _ => SymbolKind::Constant,
        };
        // 局部标量由函数正文覆盖，只单独索引可导航的函数、类和对象；回调中仍继续找声明。
        if parent.is_some_and(|parent| output.declarations[parent].kind == SymbolKind::Function)
            && !matches!(kind, SymbolKind::Function | SymbolKind::Struct)
            && !matches!(value, Some(Expression::ObjectExpression(_)))
        {
            if let Some(value) = value {
                walk_expression(value, parent, index, output);
            }
            continue;
        }
        let variable = push(
            output,
            name.as_str(),
            kind,
            declarator.span,
            index,
            parent,
            None,
        );
        if let Some(value) = value {
            walk_expression(value, Some(variable), index, output);
        }
    }
}

/// 进入函数体和回调以提取嵌套命名声明；匿名回调不制造虚构符号，沿用词法父级。
fn walk_expression(
    expression: &Expression<'_>,
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    match expression.get_inner_expression() {
        Expression::ArrowFunctionExpression(function) => {
            if let oxc_ast::ast::ArrowFunctionBody::FunctionBody(body) = &function.body {
                for statement in &body.statements {
                    walk_statement(statement, parent, index, output);
                }
            } else if let Some(expression) = function.body.as_expression() {
                walk_expression(expression, parent, index, output);
            }
        }
        Expression::FunctionExpression(function) => {
            if let Some(body) = &function.body {
                for statement in &body.statements {
                    walk_statement(statement, parent, index, output);
                }
            }
        }
        Expression::ObjectExpression(object) => {
            if let Some(parent) = parent {
                walk_object(object, parent, index, output);
            }
        }
        Expression::CallExpression(call) => {
            for argument in &call.arguments {
                if let Some(expression) = argument.as_expression() {
                    walk_callback(expression, parent, index, output);
                }
            }
        }
        _ => {}
    }
}

/// API 参数对象中的 success/fail 等是匿名回调槽，不作为业务方法替代外层函数。
/// 回调内部真正命名的局部函数仍由普通语句遍历提取。
fn walk_callback(
    expression: &Expression<'_>,
    parent: Option<usize>,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    if let Expression::ObjectExpression(object) = expression.get_inner_expression() {
        for property in &object.properties {
            if let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(property) = property {
                walk_callback(&property.value, parent, index, output);
            }
        }
    } else {
        walk_expression(expression, parent, index, output);
    }
}

/// 为对象服务提取独立方法和嵌套对象，避免正文预算只覆盖整个服务的开头。
/// 动态计算属性与展开属性没有可确定的限定名，保留在父块中而不猜测符号。
fn walk_object(
    node: &oxc_ast::ast::ObjectExpression<'_>,
    parent: usize,
    index: &LineIndex,
    output: &mut ParseOutput,
) {
    for property in &node.properties {
        let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(property) = property else {
            continue;
        };
        if property.computed {
            continue;
        }
        let Some(name) = property.key.name() else {
            continue;
        };
        let value = property.value.get_inner_expression();
        let kind = match value {
            Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_) => {
                SymbolKind::Function
            }
            Expression::ObjectExpression(object) if object_has_methods(object) => {
                SymbolKind::Constant
            }
            _ => continue,
        };
        let member = push(
            output,
            name.as_ref(),
            kind,
            property.span,
            index,
            Some(parent),
            None,
        );
        walk_expression(value, Some(member), index, output);
    }
}

/// 纯数据字典没有可导航的方法；避免把语言包的每层键误建成服务对象候选。
fn object_has_methods(object: &oxc_ast::ast::ObjectExpression<'_>) -> bool {
    object.properties.iter().any(|property| {
        let oxc_ast::ast::ObjectPropertyKind::ObjectProperty(property) = property else {
            return false;
        };
        match property.value.get_inner_expression() {
            Expression::FunctionExpression(_) | Expression::ArrowFunctionExpression(_) => true,
            Expression::ObjectExpression(nested) => object_has_methods(nested),
            _ => false,
        }
    })
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
                    let member = push(
                        output,
                        name.as_ref(),
                        SymbolKind::Function,
                        method.span,
                        index,
                        Some(class),
                        None,
                    );
                    if let Some(body) = &method.value.body {
                        for statement in &body.statements {
                            walk_statement(statement, Some(member), index, output);
                        }
                    }
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
                let member = push(
                    output,
                    name.as_ref(),
                    kind,
                    property.span,
                    index,
                    Some(class),
                    None,
                );
                if let Some(value) = &property.value {
                    walk_expression(value, Some(member), index, output);
                }
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
        let (name, span, kind) = match member {
            oxc_ast::ast::TSSignature::TSMethodSignature(method) => {
                (method.key.name(), method.span, SymbolKind::Function)
            }
            oxc_ast::ast::TSSignature::TSPropertySignature(property) => {
                (property.key.name(), property.span, SymbolKind::Other)
            }
            _ => continue,
        };
        if let Some(name) = name {
            push(
                output,
                name.as_ref(),
                kind,
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
        // 文档注释在文件遍历结束后按 Oxc 的 attached_to 位置补齐。
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

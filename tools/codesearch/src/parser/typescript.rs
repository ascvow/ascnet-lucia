//! TypeScript / TSX / JavaScript 语法适配层。
//!
//! 三种语言共用同一套节点名称：`function_declaration`、`class_declaration`、
//! `method_definition`、`interface_declaration`、`type_alias_declaration`、
//! `enum_declaration`、`lexical_declaration`（箭头函数）、`internal_module`（namespace）、
//! `export_statement`（包装真正的声明）、`import_statement`。
//! 文档注释是以 `/**` 开头的 `comment` 节点。

use tree_sitter::Node;

use crate::parser::{Context, Definition, ParsedFile};
use crate::store::ImportRecord;
use crate::types::ChunkKind;

/// 遍历 `program` 并提取顶层定义与导入。
pub fn extract(ctx: &Context<'_>, root: Node<'_>, parsed: &mut ParsedFile) {
    let mut scope: Vec<String> = Vec::new();
    walk(
        ctx,
        root,
        &mut scope,
        None,
        &mut parsed.definitions,
        &mut parsed.imports,
    );
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
        handle_node(ctx, node, node, scope, parent_symbol, out, imports);
    }
}

/// 处理一个语句节点。
///
/// `outer` 是用于计算前置文档与起始位置的节点（`export` 包装时指向 `export_statement`），
/// `node` 是真正的声明节点。
fn handle_node(
    ctx: &Context<'_>,
    outer: Node<'_>,
    node: Node<'_>,
    scope: &mut Vec<String>,
    parent_symbol: Option<&str>,
    out: &mut Vec<Definition>,
    imports: &mut Vec<ImportRecord>,
) {
    match node.kind() {
        "import_statement" => {
            let raw = ctx.text(node).trim().to_string();
            let module = node
                .child_by_field_name("source")
                .map(|source| ctx.text(source).trim_matches(['"', '\'', '`']).to_string());
            let range = node.byte_range();
            imports.push(ImportRecord {
                raw,
                module,
                start_byte: range.start,
                end_byte: range.end,
            });
        }
        // `export` 只是包装，真正的声明在 declaration 字段里；起始位置仍用 export_statement。
        "export_statement" => {
            if let Some(declaration) = node.child_by_field_name("declaration") {
                handle_node(ctx, outer, declaration, scope, parent_symbol, out, imports);
                return;
            }
            // `export default { methods: { submit() {} } }`：Vue 2 选项式组件的写法，
            // 没有 declaration 字段，成员藏在对象字面量里。
            if let Some(value) = node.child_by_field_name("value") {
                if value.kind() == "object" {
                    let lead = leading(ctx, outer);
                    out.extend(object_members(ctx, value, scope, None, lead.doc.as_deref()));
                }
            }
        }
        // `declare module 'x' {}` / `declare global {}` / `declare function f()`。
        // `.d.ts` 里几乎全是这种写法，不下钻的话整个声明文件提取不到任何符号。
        "ambient_declaration" => {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                handle_node(ctx, outer, child, scope, parent_symbol, out, imports);
            }
        }
        // `namespace X {}` 在语法树中包在 expression_statement 里。
        "expression_statement" => {
            let Some(child) = node.named_child(0) else {
                return;
            };
            if matches!(child.kind(), "internal_module" | "module") {
                handle_node(ctx, outer, child, scope, parent_symbol, out, imports);
                return;
            }
            // AMD / UMD / IIFE 模块包装：整个文件的代码都在包装函数体内，
            // 不穿过去就提取不到任何符号。实测 FastAdmin 后台的自有 JS
            // 全部是 `define([...], function () { ... })` 写法。
            for body in module_wrapper_bodies(ctx, child) {
                walk(ctx, body, scope, parent_symbol, out, imports);
            }
        }
        "class_declaration" | "abstract_class_declaration" => {
            if let Some(definition) = container(ctx, outer, node, ChunkKind::Struct, scope, imports)
            {
                out.push(definition);
            }
        }
        "interface_declaration" => {
            if let Some(definition) = container(ctx, outer, node, ChunkKind::Trait, scope, imports)
            {
                out.push(definition);
            }
        }
        "internal_module" | "module" => {
            if let Some(definition) = container(ctx, outer, node, ChunkKind::Module, scope, imports)
            {
                out.push(definition);
            }
        }
        "function_declaration" | "generator_function_declaration" | "function_signature" => {
            if let Some(definition) = function(ctx, outer, node, scope, parent_symbol) {
                out.push(definition);
            }
        }
        "method_definition" | "method_signature" | "abstract_method_signature" => {
            if let Some(definition) = method(ctx, outer, node, scope, parent_symbol) {
                out.push(definition);
            }
        }
        "type_alias_declaration" => {
            if let Some(definition) =
                leaf(ctx, outer, node, ChunkKind::TypeAlias, scope, parent_symbol)
            {
                out.push(definition);
            }
        }
        "enum_declaration" => {
            if let Some(definition) = leaf(ctx, outer, node, ChunkKind::Enum, scope, parent_symbol)
            {
                out.push(definition);
            }
        }
        // `const handler = async () => {}` 视为函数定义；其余常量归为 Const。
        "lexical_declaration" | "variable_declaration" => {
            let mut cursor = node.walk();
            for declarator in node.named_children(&mut cursor) {
                if declarator.kind() != "variable_declarator" {
                    continue;
                }
                if let Some(definition) =
                    variable(ctx, outer, node, declarator, scope, parent_symbol)
                {
                    out.push(definition);
                }
            }
        }
        _ => {}
    }
}

fn leading(ctx: &Context<'_>, node: Node<'_>) -> crate::parser::Leading {
    ctx.leading(node, |kind| kind == "comment", |kind| kind == "decorator")
}

/// 找出模块包装调用的函数体，作为模块作用域继续提取声明。
///
/// 只认三种确定的包装形态，不对任意调用下钻——`router.beforeEach(cb)` 这类
/// 回调是处理器而非模块作用域，把里面的局部变量提成顶层符号只会污染符号表：
///
/// 1. `define([...], function () { ... })` / `require([...], function () { ... })`；
/// 2. `(function () { ... })()` 立即执行函数；
/// 3. UMD 的 `(function (root, factory) { ... })(this, function () { ... })`，
///    包装函数与工厂函数都要进入。
///
/// 只穿过这一层：函数体内的嵌套函数由常规规则按子定义处理，不会无限展开。
fn module_wrapper_bodies<'tree>(ctx: &Context<'_>, node: Node<'tree>) -> Vec<Node<'tree>> {
    // `!function(){}()` 这类前缀写法先剥掉一元运算符。
    let node = if node.kind() == "unary_expression" {
        node.named_child(0).unwrap_or(node)
    } else {
        node
    };
    if node.kind() != "call_expression" {
        return Vec::new();
    }
    let Some(callee) = node.child_by_field_name("function") else {
        return Vec::new();
    };

    let callee_is_function = unwrap_function(callee).is_some();
    let callee_is_loader =
        callee.kind() == "identifier" && matches!(ctx.text(callee), "define" | "require");
    if !callee_is_function && !callee_is_loader {
        return Vec::new();
    }

    let mut bodies = Vec::new();
    if let Some(function) = unwrap_function(callee) {
        if let Some(body) = function.child_by_field_name("body") {
            bodies.push(body);
        }
    }
    if let Some(arguments) = node.child_by_field_name("arguments") {
        let mut cursor = arguments.walk();
        for argument in arguments.named_children(&mut cursor) {
            if let Some(function) = unwrap_function(argument) {
                if let Some(body) = function.child_by_field_name("body") {
                    bodies.push(body);
                }
            }
        }
    }
    bodies
}

/// 取出函数节点，必要时剥掉外层括号。
fn unwrap_function<'tree>(node: Node<'tree>) -> Option<Node<'tree>> {
    let node = if node.kind() == "parenthesized_expression" {
        node.named_child(0)?
    } else {
        node
    };
    matches!(
        node.kind(),
        "function_expression" | "function" | "arrow_function" | "generator_function"
    )
    .then_some(node)
}

/// 提取对象字面量里的函数成员。
///
/// 覆盖两种常见写法：`var Controller = { index: function () {} }`（FastAdmin 后台）
/// 与 `export default { methods: { submit() {} } }`（Vue 2 选项式组件）。
/// 这两种写法下真正的入口全部是对象属性，不提取就一个符号都没有。
///
/// `container` 为 `None` 时不加前缀——`export default` 的对象没有可用名字，
/// 强行拼一个只会让符号查询更难命中。
fn object_members(
    ctx: &Context<'_>,
    object: Node<'_>,
    scope: &[String],
    container: Option<&str>,
    doc: Option<&str>,
) -> Vec<Definition> {
    collect_object_members(ctx, object, scope, container, doc, 3)
}

fn collect_object_members(
    ctx: &Context<'_>,
    object: Node<'_>,
    scope: &[String],
    container: Option<&str>,
    doc: Option<&str>,
    depth: usize,
) -> Vec<Definition> {
    if depth == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut cursor = object.walk();
    for member in object.named_children(&mut cursor) {
        match member.kind() {
            // `submit() {}` 简写方法。
            "method_definition" => {
                if let Some(definition) = object_method(ctx, member, member, scope, container, doc)
                {
                    out.push(definition);
                }
            }
            // `submit: function () {}` 与 `methods: { ... }`。
            "pair" => {
                let Some(value) = member.child_by_field_name("value") else {
                    continue;
                };
                if unwrap_function(value).is_some() {
                    if let Some(definition) =
                        object_method(ctx, member, value, scope, container, doc)
                    {
                        out.push(definition);
                    }
                } else if value.kind() == "object" {
                    // `methods: {}` / `computed: {}` 这类分组再下一层。
                    out.extend(collect_object_members(
                        ctx,
                        value,
                        scope,
                        container,
                        None,
                        depth - 1,
                    ));
                }
            }
            _ => {}
        }
    }
    out
}

fn object_method(
    ctx: &Context<'_>,
    member: Node<'_>,
    value: Node<'_>,
    scope: &[String],
    container: Option<&str>,
    doc: Option<&str>,
) -> Option<Definition> {
    let name = ctx
        .field_text(member, "key")
        .or_else(|| ctx.field_text(member, "name"))?
        .trim_matches(['"', '\'', '`'])
        .to_string();
    if name.is_empty() {
        return None;
    }
    let body = unwrap_function(value).and_then(|f| f.child_by_field_name("body"));
    let lead = leading(ctx, member);
    let qualified = match container {
        Some(container) => format!("{container}.{name}"),
        None => ctx.qualify(scope, &name),
    };
    Some(Definition {
        kind: ChunkKind::Method,
        qualified,
        name,
        parent: container.map(str::to_string),
        module_path: module_path(scope),
        signature: ctx.signature(member, body),
        doc: lead.doc.or_else(|| doc.map(str::to_string)),
        start_byte: lead.start_byte,
        end_byte: member.end_byte(),
        start_line: lead.start_line,
        end_line: member.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        children: Vec::new(),
    })
}

fn body_of(node: Node<'_>) -> Option<Node<'_>> {
    node.child_by_field_name("body")
}

fn container(
    ctx: &Context<'_>,
    outer: Node<'_>,
    node: Node<'_>,
    kind: ChunkKind,
    scope: &mut Vec<String>,
    imports: &mut Vec<ImportRecord>,
) -> Option<Definition> {
    let name = ctx.field_text(node, "name")?;
    let body = body_of(node);
    let lead = leading(ctx, outer);
    let qualified = ctx.qualify(scope, &name);
    let mut children = Vec::new();
    if let Some(body) = body {
        scope.push(name.clone());
        walk(ctx, body, scope, Some(&qualified), &mut children, imports);
        scope.pop();
    }
    Some(Definition {
        kind,
        name,
        parent: None,
        module_path: module_path(scope),
        signature: ctx.signature(node, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: outer.end_byte(),
        start_line: lead.start_line,
        end_line: outer.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        qualified,
        children,
    })
}

fn function(
    ctx: &Context<'_>,
    outer: Node<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_symbol: Option<&str>,
) -> Option<Definition> {
    let name = ctx.field_text(node, "name")?;
    build_callable(
        ctx,
        outer,
        node,
        name,
        scope,
        parent_symbol,
        ChunkKind::Function,
    )
}

fn method(
    ctx: &Context<'_>,
    outer: Node<'_>,
    node: Node<'_>,
    scope: &[String],
    parent_symbol: Option<&str>,
) -> Option<Definition> {
    let name = ctx.field_text(node, "name")?;
    build_callable(
        ctx,
        outer,
        node,
        name,
        scope,
        parent_symbol,
        ChunkKind::Method,
    )
}

fn build_callable(
    ctx: &Context<'_>,
    outer: Node<'_>,
    node: Node<'_>,
    name: String,
    scope: &[String],
    parent_symbol: Option<&str>,
    kind: ChunkKind,
) -> Option<Definition> {
    let body = body_of(node);
    let lead = leading(ctx, outer);
    Some(Definition {
        kind,
        qualified: ctx.qualify(scope, &name),
        name,
        parent: parent_symbol.map(str::to_string),
        module_path: module_path(scope),
        signature: ctx.signature(node, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: outer.end_byte(),
        start_line: lead.start_line,
        end_line: outer.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        children: Vec::new(),
    })
}

fn leaf(
    ctx: &Context<'_>,
    outer: Node<'_>,
    node: Node<'_>,
    kind: ChunkKind,
    scope: &[String],
    parent_symbol: Option<&str>,
) -> Option<Definition> {
    let name = ctx.field_text(node, "name")?;
    let body = body_of(node);
    let lead = leading(ctx, outer);
    Some(Definition {
        kind,
        qualified: ctx.qualify(scope, &name),
        name,
        parent: parent_symbol.map(str::to_string),
        module_path: module_path(scope),
        signature: ctx.signature(node, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: outer.end_byte(),
        start_line: lead.start_line,
        end_line: outer.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        children: Vec::new(),
    })
}

fn variable(
    ctx: &Context<'_>,
    outer: Node<'_>,
    statement: Node<'_>,
    declarator: Node<'_>,
    scope: &[String],
    parent_symbol: Option<&str>,
) -> Option<Definition> {
    let name = ctx.field_text(declarator, "name")?;
    let value = declarator.child_by_field_name("value");
    let is_callable = value.is_some_and(|value| {
        matches!(
            value.kind(),
            "arrow_function" | "function_expression" | "function" | "generator_function"
        )
    });
    let kind = if is_callable {
        ChunkKind::Function
    } else {
        ChunkKind::Const
    };
    // 只有函数型常量才值得单独成块；普通常量交给相邻声明合并逻辑处理。
    let body = value.and_then(body_of);
    let lead = leading(ctx, outer);
    let qualified = ctx.qualify(scope, &name);
    // `var Controller = { index: function () {} }`：对象的函数成员就是模块入口，
    // 作为子定义登记后 `Controller.index` 才能走符号通路命中。
    let children = value
        .filter(|value| value.kind() == "object")
        .map(|value| object_members(ctx, value, scope, Some(&qualified), None))
        .unwrap_or_default();
    Some(Definition {
        kind,
        qualified,
        name,
        parent: parent_symbol.map(str::to_string),
        module_path: module_path(scope),
        signature: ctx.signature(statement, body),
        doc: lead.doc,
        start_byte: lead.start_byte,
        end_byte: outer.end_byte(),
        start_line: lead.start_line,
        end_line: outer.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        children,
    })
}

fn module_path(scope: &[String]) -> Option<String> {
    if scope.is_empty() {
        None
    } else {
        Some(scope.join("."))
    }
}

#[cfg(test)]
mod tests {
    use crate::parser::parse;
    use crate::types::{ChunkKind, Language};

    const SOURCE: &str = r#"import { Logger } from "./logger";
import http from "node:http";

/** 带重试的 HTTP 客户端。 */
export class RetryClient {
  private attempts = 0;

  /** 执行请求并在失败后重试。 */
  async execute(request: Request): Promise<Response> {
    return fetch(request);
  }

  get attemptCount(): number {
    return this.attempts;
  }
}

export interface Retryable {
  shouldRetry(): boolean;
}

export type RetryId = string;
export enum Outcome { Success, Failure }

/** 顶层箭头函数。 */
export const backoffDelay = async (attempt: number): Promise<number> => {
  return attempt * 100;
};

export function plainHelper(value: number) {
  return value;
}

const NOT_A_FUNCTION = 42;

namespace Inner {
  export function nestedHelper() {}
}
"#;

    #[test]
    fn extracts_classes_methods_and_arrow_functions() {
        let parsed = parse(Language::TypeScript, SOURCE).expect("parse");
        assert!(!parsed.has_errors, "示例代码应能完整解析");

        let names: Vec<&str> = parsed.definitions.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "RetryClient",
                "Retryable",
                "RetryId",
                "Outcome",
                "backoffDelay",
                "plainHelper",
                "NOT_A_FUNCTION",
                "Inner",
            ]
        );

        let class = &parsed.definitions[0];
        assert_eq!(class.kind, ChunkKind::Struct);
        assert_eq!(class.doc.as_deref(), Some("带重试的 HTTP 客户端。"));
        // export 关键字与文档注释都要包含在块里。
        let text = &SOURCE[class.start_byte..class.end_byte];
        assert!(text.starts_with("/** 带重试的 HTTP 客户端。 */"));
        assert!(text.contains("export class RetryClient"));

        // 类方法与访问器都被提取，限定名使用 `.`。
        let methods: Vec<&str> = class
            .children
            .iter()
            .map(|d| d.qualified.as_str())
            .collect();
        assert_eq!(
            methods,
            vec!["RetryClient.execute", "RetryClient.attemptCount"]
        );
        let execute = &class.children[0];
        assert_eq!(execute.kind, ChunkKind::Method);
        assert_eq!(execute.doc.as_deref(), Some("执行请求并在失败后重试。"));
        assert_eq!(
            execute.signature.as_deref(),
            Some("async execute(request: Request): Promise<Response>")
        );

        // 箭头函数常量识别为函数，普通常量识别为 Const。
        let arrow = &parsed.definitions[4];
        assert_eq!(arrow.kind, ChunkKind::Function);
        assert_eq!(arrow.name, "backoffDelay");
        assert_eq!(parsed.definitions[6].kind, ChunkKind::Const);

        // interface 的方法签名与 namespace 的嵌套函数。
        assert_eq!(
            parsed.definitions[1].children[0].qualified,
            "Retryable.shouldRetry"
        );
        assert_eq!(
            parsed.definitions[7].children[0].qualified,
            "Inner.nestedHelper"
        );
    }

    #[test]
    fn extracts_module_specifiers_from_imports() {
        let parsed = parse(Language::TypeScript, SOURCE).expect("parse");
        let modules: Vec<&str> = parsed
            .imports
            .iter()
            .filter_map(|i| i.module.as_deref())
            .collect();
        assert_eq!(modules, vec!["./logger", "node:http"]);
    }

    #[test]
    fn line_numbers_match_extracted_content() {
        let parsed = parse(Language::TypeScript, SOURCE).expect("parse");
        for definition in &parsed.definitions {
            let text = &SOURCE[definition.start_byte..definition.end_byte];
            assert_eq!(
                definition.end_line - definition.start_line + 1,
                text.lines().count() as u32,
                "定义 {} 行号范围与内容不一致",
                definition.name
            );
        }
    }

    #[test]
    fn javascript_and_tsx_use_the_same_adapter() {
        let js = "export class Widget {\n  render() { return 1; }\n}\nexport const go = () => 2;\n";
        let parsed = parse(Language::JavaScript, js).expect("parse js");
        assert_eq!(parsed.definitions[0].children[0].qualified, "Widget.render");
        assert_eq!(parsed.definitions[1].name, "go");

        let tsx =
            "export function View(): JSX.Element {\n  return <div className=\"x\">hi</div>;\n}\n";
        let parsed = parse(Language::Tsx, tsx).expect("parse tsx");
        assert!(!parsed.has_errors, "TSX 应由 TSX 语法解析");
        assert_eq!(parsed.definitions[0].name, "View");
    }

    /// AMD 模块包装必须穿过，对象成员必须提取。
    ///
    /// 回归：FastAdmin 后台的自有 JS 全部是 `define([...], function () { var Controller = {...} })`，
    /// 所有代码都在包装函数体内且入口都是对象属性。此前实测某项目自有 JS 共 11080 行、
    /// 符号覆盖 0%，`Controller.index` 这类最常被搜索的入口一个都命中不了。
    #[test]
    fn amd_module_and_object_members_yield_symbols() {
        let source = r#"define(['jquery', 'backend'], function ($, Backend) {
    var Controller = {
        index: function () {
            Table.api.init({ index_url: 'addon/index' });
        },
        add: function () {
            Controller.api.bindevent();
        },
        api: {
            bindevent: function () {
                Form.api.bindevent($("form[role=form]"));
            }
        }
    };
    return Controller;
});
"#;
        let parsed = parse(Language::JavaScript, source).expect("parse");
        let mut names = Vec::new();
        fn push(definitions: &[crate::parser::Definition], out: &mut Vec<String>) {
            for definition in definitions {
                out.push(definition.qualified.clone());
                push(&definition.children, out);
            }
        }
        push(&parsed.definitions, &mut names);

        assert!(names.contains(&"Controller".to_string()), "{names:?}");
        assert!(names.contains(&"Controller.index".to_string()), "{names:?}");
        assert!(names.contains(&"Controller.add".to_string()), "{names:?}");
        // 分组对象里的方法也要提取。
        assert!(
            names.contains(&"Controller.bindevent".to_string()),
            "{names:?}"
        );

        // 行号必须落在真实位置上。
        let index = parsed.definitions[0]
            .children
            .iter()
            .find(|d| d.name == "index")
            .expect("index");
        assert!(source[index.start_byte..index.end_byte].starts_with("index: function"));
    }

    /// IIFE / UMD 包装同样要穿过。
    #[test]
    fn iife_and_umd_wrappers_are_traversed() {
        let iife = "(function ($) {\n  function helper() {}\n  var Widget = { run: function () {} };\n})(jQuery);\n";
        let parsed = parse(Language::JavaScript, iife).expect("parse");
        let names: Vec<&str> = parsed.definitions.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"helper"), "{names:?}");
        assert!(names.contains(&"Widget"), "{names:?}");

        let umd = "(function (root, factory) {\n  module.exports = factory();\n})(this, function () {\n  function core() {}\n  return core;\n});\n";
        let parsed = parse(Language::JavaScript, umd).expect("parse");
        let names: Vec<&str> = parsed.definitions.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"core"), "工厂函数体未进入: {names:?}");
    }

    /// 普通回调不得被当成模块作用域下钻。
    ///
    /// `router.beforeEach(cb)` 里的局部变量不是模块导出，提成顶层符号只会污染符号表。
    #[test]
    fn ordinary_callbacks_are_not_treated_as_module_scopes() {
        let source =
            "router.beforeEach(function (to, from, next) {\n  var localOnly = 1;\n  function innerHelper() {}\n  next();\n});\n";
        let parsed = parse(Language::JavaScript, source).expect("parse");
        let names: Vec<&str> = parsed.definitions.iter().map(|d| d.name.as_str()).collect();
        assert!(
            !names.contains(&"localOnly") && !names.contains(&"innerHelper"),
            "普通回调内部不应产出顶层符号: {names:?}"
        );
    }

    /// Vue 2 选项式组件的方法要能被搜到。
    #[test]
    fn vue_options_api_methods_are_extracted() {
        let source = r#"export default {
  name: "LoginPage",
  data() {
    return { username: "" };
  },
  methods: {
    async submit() {
      await this.$store.dispatch("login");
    },
    reset() {
      this.username = "";
    }
  }
};
"#;
        let parsed = parse(Language::JavaScript, source).expect("parse");
        let names: Vec<&str> = parsed.definitions.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"submit"), "{names:?}");
        assert!(names.contains(&"reset"), "{names:?}");
        assert!(names.contains(&"data"), "{names:?}");
    }

    /// `.d.ts` 声明文件必须能提取出符号。
    ///
    /// 回归：`declare module 'x' {}` 解析为 `ambient_declaration` 包装节点，
    /// 不下钻就会让整个声明文件零符号。实测前端项目里 `env.d.ts`、
    /// `components.d.ts` 这类自动生成的声明文件全部是这种写法。
    #[test]
    fn ambient_declarations_in_d_ts_yield_symbols() {
        let source = r#"export {}

declare module 'vue' {
  export interface GlobalComponents {
    ElButton: typeof import('element-plus/es')['ElButton']
  }
}

declare global {
  interface Window { __APP__: string }
}

declare function legacyHelper(value: number): string;
"#;
        let parsed = parse(Language::TypeScript, source).expect("parse");
        let names: Vec<&str> = parsed.definitions.iter().map(|d| d.name.as_str()).collect();
        assert!(
            names.iter().any(|name| name.contains("vue")),
            "declare module 未提取: {names:?}"
        );
        assert!(
            names.contains(&"legacyHelper"),
            "declare function 未提取: {names:?}"
        );
        // 模块内部的 interface 也要成为子定义。
        let module = parsed
            .definitions
            .iter()
            .find(|d| d.name.contains("vue"))
            .expect("declare module");
        assert!(
            module
                .children
                .iter()
                .any(|child| child.name == "GlobalComponents"),
            "declare module 内部的 interface 未提取"
        );
    }

    #[test]
    fn broken_typescript_still_yields_valid_definitions() {
        let source = "export function ok() {}\n\nexport class Broken {\n";
        let parsed = parse(Language::TypeScript, source).expect("parse");
        assert!(parsed.has_errors);
        assert!(parsed.definitions.iter().any(|d| d.name == "ok"));
    }
}

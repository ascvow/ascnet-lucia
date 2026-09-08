//! 数据驱动的通用语法适配层。
//!
//! Rust 与 TypeScript 各自有手写适配层（[`super::rust_lang`]、[`super::typescript`]），
//! 因为它们的节点结构差异大且已有成熟测试。其余语言的 Tree-sitter 语法在结构上高度相似：
//! 都有"容器 / 可调用 / 叶子定义 / 导入"四类节点，名称在 `name` 字段、主体在 `body` 字段。
//! 因此这里用一张 [`LanguageSpec`] 表描述差异，遍历逻辑只写一份。
//!
//! 每种语言的节点名都由 `examples/dump_ast.rs` 对真实语法实测得到，不是凭约定猜测。
//! 少数语言的取名方式无法用字段表达（C 系藏在 `declarator` 链里、CSS 用选择器文本），
//! 由 [`NameSource`] 的有限几种策略覆盖，不为单个语言开特例分支。

use tree_sitter::Node;

use crate::parser::{Context, Definition, ParsedFile};
use crate::store::ImportRecord;
use crate::types::{ChunkKind, Language};

/// 定义名称的提取方式。
#[derive(Debug, Clone, Copy)]
pub enum NameSource {
    /// 读取命名字段，适用于绝大多数语法。
    Field(&'static str),
    /// 沿 `declarator` 链下钻到标识符。C / C++ 的函数名藏在这里。
    Declarator,
    /// 取指定类别子节点的整段文本作为名称。CSS 选择器用这种方式。
    ChildText(&'static str),
    /// 取指定类别后代节点的文本。SQL 的 `object_reference` 藏在下一层。
    DescendantText(&'static str),
    /// 从独立的签名节点读取名称，不进入参数或函数体。
    SignatureName,
    /// HCL 块的种类与全部标签组成唯一名称，例如 resource.aws_s3_bucket.archive。
    BlockLabels,
}

/// 一种语言的语法节点约定。
///
/// 所有列表都是"节点类别名"，与 Tree-sitter 语法保持一致。
pub struct LanguageSpec {
    /// 限定名分隔符。
    pub separator: &'static str,
    /// 拥有子作用域的容器节点，需要递归进入主体。
    pub containers: &'static [(&'static str, ChunkKind)],
    /// 函数与方法节点。
    pub callables: &'static [(&'static str, ChunkKind)],
    /// 没有子作用域的叶子定义节点。
    pub leaves: &'static [(&'static str, ChunkKind)],
    /// 导入节点。
    pub imports: &'static [&'static str],
    /// 透明包装节点：自身不是定义，其命名子节点按同级声明处理。
    ///
    /// 覆盖 `decorated_definition`(Python)、`template_declaration`(C++)、
    /// `type_declaration`(Go)、`statement`(SQL) 等。文档与起始位置仍取包装节点，
    /// 这样装饰器和注解会被包含进代码块。
    pub wrappers: &'static [&'static str],
    /// 注释节点类别。
    pub comments: &'static [&'static str],
    /// 属性 / 装饰器 / 注解节点类别。
    pub attributes: &'static [&'static str],
    /// 文档注释前缀。为空表示该语言没有专门的文档注释语法，任何相邻注释都视为文档。
    pub doc_prefixes: &'static [&'static str],
    /// 名称提取策略，按顺序尝试第一个成功的。
    pub name_sources: &'static [NameSource],
    /// 主体字段名，按顺序尝试。
    pub body_fields: &'static [&'static str],
    /// 主体节点类别。字段查找失败时按类别在直接子节点中兜底查找。
    pub body_kinds: &'static [&'static str],
    /// 是否使用 Python 式 docstring：主体的第一条语句若是字符串字面量即为文档。
    pub docstring: bool,
}

/// 遍历语法树根节点并提取定义与导入。
pub fn extract(ctx: &Context<'_>, spec: &LanguageSpec, root: Node<'_>, parsed: &mut ParsedFile) {
    // 这些语法把全部声明包在根节点的单独列表中；列表不是一个声明，不能复用其范围。
    let list_kind = match ctx.language {
        Language::Haskell => Some("declarations"),
        Language::Hcl => Some("body"),
        Language::Graphql => Some("document"),
        _ => None,
    };
    let root = list_kind
        .and_then(|kind| child_of_kind(root, kind))
        .unwrap_or(root);
    let mut scope: Vec<String> = Vec::new();
    walk(
        ctx,
        spec,
        root,
        &mut scope,
        None,
        &mut parsed.definitions,
        &mut parsed.imports,
    );
}

fn walk(
    ctx: &Context<'_>,
    spec: &LanguageSpec,
    parent_node: Node<'_>,
    scope: &mut Vec<String>,
    parent_symbol: Option<&str>,
    out: &mut Vec<Definition>,
    imports: &mut Vec<ImportRecord>,
) {
    let mut cursor = parent_node.walk();
    for node in parent_node.named_children(&mut cursor) {
        handle(ctx, spec, node, node, scope, parent_symbol, out, imports);
    }
}

/// 处理一个语句节点。
///
/// `outer` 用于计算前置文档与起始位置（包装节点场景下指向包装节点本身），
/// `node` 是真正的声明节点。
#[allow(clippy::too_many_arguments)]
fn handle(
    ctx: &Context<'_>,
    spec: &LanguageSpec,
    outer: Node<'_>,
    node: Node<'_>,
    scope: &mut Vec<String>,
    parent_symbol: Option<&str>,
    out: &mut Vec<Definition>,
    imports: &mut Vec<ImportRecord>,
) {
    let kind = node.kind();

    // Haskell 的类型签名和紧随其后的同名实现共同构成一个定义，避免只召回签名却丢失实现。
    if ctx.language == Language::Haskell && kind == "signature" {
        if let Some(next) = node.next_named_sibling() {
            if matches!(next.kind(), "function" | "bind")
                && ctx.field_text(node, "name").is_some()
                && ctx.field_text(node, "name") == ctx.field_text(next, "name")
            {
                return;
            }
        }
    }

    if spec.imports.contains(&kind) {
        push_import(ctx, node, imports);
        return;
    }

    // 包装节点：命名子节点按同级声明处理，但文档与起点仍归包装节点。
    if spec.wrappers.contains(&kind) {
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            handle(ctx, spec, outer, child, scope, parent_symbol, out, imports);
        }
        return;
    }

    if let Some(chunk_kind) = lookup(spec.containers, kind) {
        if let Some(definition) = container(ctx, spec, outer, node, chunk_kind, scope, imports) {
            out.push(definition);
        }
        return;
    }

    if let Some(chunk_kind) = lookup(spec.callables, kind) {
        if let Some(definition) = leaf(ctx, spec, outer, node, chunk_kind, scope, parent_symbol) {
            out.push(definition);
        }
        return;
    }

    if let Some(chunk_kind) = lookup(spec.leaves, kind) {
        if let Some(definition) = leaf(ctx, spec, outer, node, chunk_kind, scope, parent_symbol) {
            out.push(definition);
        }
    }
}

fn lookup(table: &[(&'static str, ChunkKind)], kind: &str) -> Option<ChunkKind> {
    table
        .iter()
        .find(|(name, _)| *name == kind)
        .map(|(_, chunk_kind)| *chunk_kind)
}

fn push_import(ctx: &Context<'_>, node: Node<'_>, imports: &mut Vec<ImportRecord>) {
    let raw = ctx.text(node).trim().to_string();
    if raw.is_empty() {
        return;
    }
    // 模块名优先取语法字段；取不到时退回整条语句，宁可粗糙也不要留空。
    let module = ["source", "path", "name", "module_name"]
        .iter()
        .find_map(|field| ctx.field_text(node, field))
        .map(|text| {
            text.trim_matches(['"', '\'', '`', '<', '>', ';'])
                .to_string()
        })
        .filter(|text| !text.is_empty());
    let range = node.byte_range();
    imports.push(ImportRecord {
        raw,
        module,
        start_byte: range.start,
        end_byte: range.end,
    });
}

fn container(
    ctx: &Context<'_>,
    spec: &LanguageSpec,
    outer: Node<'_>,
    node: Node<'_>,
    chunk_kind: ChunkKind,
    scope: &mut Vec<String>,
    imports: &mut Vec<ImportRecord>,
) -> Option<Definition> {
    let name = name_of(ctx, spec, node)?;
    let body = body_of(spec, node);
    let lead = leading(ctx, spec, outer);
    let qualified = ctx.qualify_with(spec.separator, scope, &name);

    let mut children = Vec::new();
    if let Some(body) = body {
        scope.push(name.clone());
        walk(
            ctx,
            spec,
            body,
            scope,
            Some(&qualified),
            &mut children,
            imports,
        );
        scope.pop();
    }

    Some(Definition {
        kind: chunk_kind,
        name,
        parent: None,
        module_path: module_path(spec, scope),
        signature: ctx.signature(node, body),
        doc: lead.doc.or_else(|| docstring(ctx, spec, body)),
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

fn leaf(
    ctx: &Context<'_>,
    spec: &LanguageSpec,
    outer: Node<'_>,
    node: Node<'_>,
    chunk_kind: ChunkKind,
    scope: &[String],
    parent_symbol: Option<&str>,
) -> Option<Definition> {
    let name = name_of(ctx, spec, node)?;
    let body = body_of(spec, node);
    let mut lead = leading(ctx, spec, outer);
    let qualified = if ctx.language == Language::Go && node.kind() == "method_declaration" {
        go_receiver(ctx, node)
            .map(|receiver| format!("{receiver}.{name}"))
            .unwrap_or_else(|| ctx.qualify_with(spec.separator, scope, &name))
    } else {
        ctx.qualify_with(spec.separator, scope, &name)
    };
    let mut signature = ctx.signature(node, body);
    if ctx.language == Language::Haskell && matches!(node.kind(), "function" | "bind") {
        if let Some(previous) = node.prev_named_sibling() {
            if previous.kind() == "signature"
                && ctx.field_text(previous, "name").as_deref() == Some(&name)
            {
                lead = leading(ctx, spec, previous);
                signature = Some(crate::parser::collapse_whitespace(ctx.text(previous)));
            }
        }
    }
    Some(Definition {
        kind: chunk_kind,
        qualified,
        name,
        parent: parent_symbol.map(str::to_string),
        module_path: module_path(spec, scope),
        signature,
        doc: lead.doc.or_else(|| docstring(ctx, spec, body)),
        start_byte: lead.start_byte,
        end_byte: outer.end_byte(),
        start_line: lead.start_line,
        end_line: outer.end_position().row as u32 + 1,
        body_start_byte: body.map(|b| b.start_byte()),
        split_points: ctx.split_points(body),
        children: Vec::new(),
    })
}

/// 接收者决定 Go 方法身份；只剥离指针与泛型类型包装，不使用局部接收者变量名。
///
/// 方法与类型在源码中并非包含关系，因此这里只限定符号名，不伪造 parent 包含边。
fn go_receiver(ctx: &Context<'_>, node: Node<'_>) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?.named_child(0)?;
    let mut ty = receiver.child_by_field_name("type")?;
    if ty.kind() == "pointer_type" {
        ty = ty.named_child(0)?;
    }
    if ty.kind() == "generic_type" {
        ty = ty.child_by_field_name("type")?;
    }
    (ty.kind() == "type_identifier").then(|| ctx.text(ty).to_string())
}

fn leading(ctx: &Context<'_>, spec: &LanguageSpec, node: Node<'_>) -> crate::parser::Leading {
    // Haskell/HCL 的第一条声明前注释属于根列表的兄弟节点，不属于声明列表内部。
    if matches!(ctx.language, Language::Haskell | Language::Hcl)
        && node.prev_named_sibling().is_none()
    {
        if let Some(parent) = node.parent() {
            if matches!(parent.kind(), "declarations" | "body")
                && parent.start_byte() == node.start_byte()
            {
                return ctx.leading_with(parent, spec.comments, spec.attributes, spec.doc_prefixes);
            }
        }
    }
    ctx.leading_with(node, spec.comments, spec.attributes, spec.doc_prefixes)
}

/// 按 [`LanguageSpec::name_sources`] 依次尝试提取名称。
fn name_of(ctx: &Context<'_>, spec: &LanguageSpec, node: Node<'_>) -> Option<String> {
    for source in spec.name_sources {
        let name = match source {
            NameSource::Field(field) => ctx.field_text(node, field),
            NameSource::Declarator => declarator_name(ctx, node),
            NameSource::ChildText(kind) => {
                child_of_kind(node, kind).map(|n| ctx.text(n).trim().to_string())
            }
            NameSource::DescendantText(kind) => {
                descendant_of_kind(node, kind, 3).map(|n| ctx.text(n).trim().to_string())
            }
            NameSource::SignatureName => {
                node.child_by_field_name("signature").and_then(|signature| {
                    ctx.field_text(signature, "name").or_else(|| {
                        let mut cursor = signature.walk();
                        let name = signature
                            .named_children(&mut cursor)
                            .find_map(|child| ctx.field_text(child, "name"));
                        name
                    })
                })
            }
            NameSource::BlockLabels => {
                let mut cursor = node.walk();
                let labels: Vec<&str> = node
                    .named_children(&mut cursor)
                    .take_while(|child| child.kind() != "block_start")
                    .filter(|child| matches!(child.kind(), "identifier" | "string_lit"))
                    .map(|child| ctx.text(child).trim_matches('"'))
                    .collect();
                (!labels.is_empty()).then(|| labels.join("."))
            }
        };
        let name = name
            .map(|text| crate::parser::collapse_whitespace(&text))
            .filter(|text| !text.is_empty());
        if let Some(name) = name {
            // 选择器等文本名称可能很长，保守截断以免污染符号表。
            return Some(crate::parser::truncate_chars(&name, 120));
        }
    }
    None
}

/// C 系语法：`function_definition -> declarator -> ... -> identifier`。
///
/// 逐层跟随 `declarator` 字段，直到取到标识符节点。指针、数组、参数列表
/// 都会在这条链上出现，所以不能只看一层。
fn declarator_name(ctx: &Context<'_>, node: Node<'_>) -> Option<String> {
    let mut current = node.child_by_field_name("declarator")?;
    for _ in 0..8 {
        match current.kind() {
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "qualified_identifier"
            | "destructor_name"
            | "operator_name"
            | "primitive_type" => return Some(ctx.text(current).trim().to_string()),
            _ => {}
        }
        match current.child_by_field_name("declarator") {
            Some(next) => current = next,
            None => break,
        }
    }
    Some(ctx.text(current).trim().to_string()).filter(|text| !text.is_empty())
}

fn child_of_kind<'tree>(node: Node<'tree>, kind: &str) -> Option<Node<'tree>> {
    let mut cursor = node.walk();
    let found = node.named_children(&mut cursor).find(|c| c.kind() == kind);
    found
}

/// 在有限深度内查找指定类别的后代节点。
///
/// 限制深度是刻意的：SQL 的 `object_reference` 固定在两三层内，
/// 无限下钻会把子查询里的表名误当成定义名。
fn descendant_of_kind<'tree>(node: Node<'tree>, kind: &str, depth: usize) -> Option<Node<'tree>> {
    if depth == 0 {
        return None;
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        if child.kind() == kind {
            return Some(child);
        }
        if let Some(found) = descendant_of_kind(child, kind, depth - 1) {
            return Some(found);
        }
    }
    None
}

fn body_of<'tree>(spec: &LanguageSpec, node: Node<'tree>) -> Option<Node<'tree>> {
    for field in spec.body_fields {
        if let Some(body) = node.child_by_field_name(field) {
            return Some(body);
        }
    }
    // 部分语法（Kotlin、Swift 的枚举体）不给主体命名字段，只能按类别兜底。
    for kind in spec.body_kinds {
        if let Some(body) = child_of_kind(node, kind) {
            return Some(body);
        }
    }
    None
}

/// Python 式文档字符串：主体的第一条语句若是字符串字面量则视为文档。
fn docstring(ctx: &Context<'_>, spec: &LanguageSpec, body: Option<Node<'_>>) -> Option<String> {
    if !spec.docstring {
        return None;
    }
    let body = body?;
    let first = body.named_child(0)?;
    let text_node = if first.kind() == "expression_statement" {
        first.named_child(0)?
    } else {
        first
    };
    if text_node.kind() != "string" {
        return None;
    }
    let raw = ctx.text(text_node);
    let trimmed = raw
        .trim()
        .trim_start_matches("r")
        .trim_start_matches("f")
        .trim_matches(|c| c == '"' || c == '\'')
        .trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(crate::parser::truncate_chars(trimmed, 2000))
    }
}

fn module_path(spec: &LanguageSpec, scope: &[String]) -> Option<String> {
    if scope.is_empty() {
        None
    } else {
        Some(scope.join(spec.separator))
    }
}

/// 取得语言对应的语法与节点约定。
///
/// 返回 `None` 表示该语言不走通用适配层（Rust / TypeScript 系有手写适配层，
/// 标记与配置格式走文本兜底）。
pub fn spec_for(language: Language) -> Option<(tree_sitter::Language, &'static LanguageSpec)> {
    let pair = match language {
        Language::Python => (tree_sitter_python::LANGUAGE.into(), &PYTHON),
        Language::Go => (tree_sitter_go::LANGUAGE.into(), &GO),
        Language::Java => (tree_sitter_java::LANGUAGE.into(), &JAVA),
        Language::C => (tree_sitter_c::LANGUAGE.into(), &C),
        Language::Cpp => (tree_sitter_cpp::LANGUAGE.into(), &CPP),
        Language::CSharp => (tree_sitter_c_sharp::LANGUAGE.into(), &C_SHARP),
        Language::Ruby => (tree_sitter_ruby::LANGUAGE.into(), &RUBY),
        Language::Php => (tree_sitter_php::LANGUAGE_PHP.into(), &PHP),
        Language::Swift => (tree_sitter_swift::LANGUAGE.into(), &SWIFT),
        Language::Kotlin => (tree_sitter_kotlin_ng::LANGUAGE.into(), &KOTLIN),
        Language::Scala => (tree_sitter_scala::LANGUAGE.into(), &SCALA),
        Language::Lua => (tree_sitter_lua::LANGUAGE.into(), &LUA),
        Language::Bash => (tree_sitter_bash::LANGUAGE.into(), &BASH),
        Language::Css => (tree_sitter_css::LANGUAGE.into(), &CSS),
        Language::Sql => (tree_sitter_sequel::LANGUAGE.into(), &SQL),
        Language::Dart => (tree_sitter_dart::LANGUAGE.into(), &DART),
        Language::Zig => (tree_sitter_zig::LANGUAGE.into(), &ZIG),
        Language::Haskell => (tree_sitter_haskell::LANGUAGE.into(), &HASKELL),
        Language::Hcl => (tree_sitter_hcl::LANGUAGE.into(), &HCL),
        Language::Graphql => (tree_sitter_graphql::LANGUAGE.into(), &GRAPHQL),
        _ => return None,
    };
    Some(pair)
}

/// 大多数 C 风格语言共用的注释与文档约定。
const BLOCK_DOC: &[&str] = &["/**", "///"];

/// Dart 的类成员与方法签名有额外包装层，函数体仍采用同一套边界处理。
static DART: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[
        ("class_declaration", ChunkKind::Struct),
        ("mixin_declaration", ChunkKind::Trait),
        ("extension_declaration", ChunkKind::Impl),
        ("extension_type_declaration", ChunkKind::Struct),
        ("enum_declaration", ChunkKind::Enum),
    ],
    callables: &[
        ("function_declaration", ChunkKind::Function),
        ("method_declaration", ChunkKind::Method),
        ("getter_declaration", ChunkKind::Method),
        ("setter_declaration", ChunkKind::Method),
        ("function_signature", ChunkKind::Function),
        ("constructor_signature", ChunkKind::Method),
        ("constant_constructor_signature", ChunkKind::Method),
        ("getter_signature", ChunkKind::Method),
        ("setter_signature", ChunkKind::Method),
    ],
    leaves: &[
        ("initialized_identifier", ChunkKind::Const),
        ("type_alias", ChunkKind::TypeAlias),
    ],
    imports: &["import_or_export"],
    wrappers: &[
        "class_member",
        "declaration",
        "initialized_identifier_list",
        "external_function_declaration",
    ],
    comments: &["comment", "documentation_comment"],
    attributes: &["annotation"],
    doc_prefixes: BLOCK_DOC,
    name_sources: &[
        NameSource::Field("name"),
        NameSource::SignatureName,
        NameSource::ChildText("type_identifier"),
    ],
    body_fields: &["body"],
    body_kinds: &["class_body", "extension_body", "enum_body"],
    docstring: false,
};

/// Zig 的类型绑定也是变量声明；只递归其直接容器值，不把函数局部变量提升为符号。
static ZIG: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[("variable_declaration", ChunkKind::Const)],
    callables: &[
        ("function_declaration", ChunkKind::Function),
        ("test_declaration", ChunkKind::Function),
    ],
    leaves: &[],
    imports: &[],
    wrappers: &[],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: BLOCK_DOC,
    name_sources: &[
        NameSource::Field("name"),
        NameSource::ChildText("identifier"),
        NameSource::ChildText("string"),
    ],
    body_fields: &["body"],
    body_kinds: &[
        "struct_declaration",
        "enum_declaration",
        "union_declaration",
        "opaque_declaration",
        "block",
    ],
    docstring: false,
};

/// Haskell 的函数方程、签名与类型分别保留原始位置。
static HASKELL: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[("class", ChunkKind::Trait), ("instance", ChunkKind::Impl)],
    callables: &[
        ("function", ChunkKind::Function),
        ("bind", ChunkKind::Function),
        ("signature", ChunkKind::Function),
    ],
    leaves: &[
        ("data_type", ChunkKind::Enum),
        ("newtype", ChunkKind::Struct),
        ("type_synomym", ChunkKind::TypeAlias),
    ],
    imports: &["import"],
    wrappers: &["imports"],
    comments: &["comment", "haddock"],
    attributes: &[],
    doc_prefixes: &[],
    name_sources: &[NameSource::Field("name")],
    body_fields: &["declarations"],
    body_kinds: &[],
    docstring: false,
};

/// Terraform 使用完整块标签，避免所有资源都被错误命名为 resource。
static HCL: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[("block", ChunkKind::Module)],
    callables: &[],
    leaves: &[("attribute", ChunkKind::Const)],
    imports: &[],
    wrappers: &[],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: &[],
    name_sources: &[NameSource::BlockLabels],
    body_fields: &[],
    body_kinds: &["body"],
    docstring: false,
};

/// GraphQL 仅提取具名定义；匿名操作不借用选中字段的名称冒充声明。
static GRAPHQL: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[
        ("object_type_definition", ChunkKind::Struct),
        ("interface_type_definition", ChunkKind::Trait),
        ("input_object_type_definition", ChunkKind::Struct),
        ("object_type_extension", ChunkKind::Struct),
    ],
    callables: &[
        ("operation_definition", ChunkKind::Function),
        ("fragment_definition", ChunkKind::Function),
    ],
    leaves: &[
        ("enum_type_definition", ChunkKind::Enum),
        ("scalar_type_definition", ChunkKind::TypeAlias),
        ("union_type_definition", ChunkKind::TypeAlias),
        ("directive_definition", ChunkKind::Rule),
        ("field_definition", ChunkKind::Const),
        ("input_value_definition", ChunkKind::Const),
    ],
    imports: &[],
    wrappers: &[
        "definition",
        "type_system_definition",
        "type_definition",
        "executable_definition",
        "type_system_extension",
        "type_extension",
    ],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: &[],
    name_sources: &[
        NameSource::ChildText("name"),
        NameSource::ChildText("fragment_name"),
    ],
    body_fields: &[],
    body_kinds: &[
        "fields_definition",
        "input_fields_definition",
        "selection_set",
    ],
    docstring: false,
};

pub(crate) static PYTHON: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[("class_definition", ChunkKind::Struct)],
    callables: &[("function_definition", ChunkKind::Function)],
    leaves: &[],
    imports: &["import_statement", "import_from_statement"],
    wrappers: &["decorated_definition"],
    comments: &["comment"],
    attributes: &["decorator"],
    // Python 没有专门的文档注释语法，紧邻的 `#` 注释就是说明。
    doc_prefixes: &[],
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &["block"],
    docstring: true,
};

pub(crate) static GO: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[],
    callables: &[
        ("function_declaration", ChunkKind::Function),
        ("method_declaration", ChunkKind::Method),
    ],
    leaves: &[
        ("type_spec", ChunkKind::Struct),
        ("const_spec", ChunkKind::Const),
        ("var_spec", ChunkKind::Const),
    ],
    imports: &["import_declaration"],
    // 声明块包着真正的 spec：`type ( A struct{}; B struct{} )` 会有多个子节点。
    wrappers: &["type_declaration", "const_declaration", "var_declaration"],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: &[],
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &["block", "statement_list"],
    docstring: false,
};

pub(crate) static JAVA: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[
        ("class_declaration", ChunkKind::Struct),
        ("interface_declaration", ChunkKind::Trait),
        ("enum_declaration", ChunkKind::Enum),
        ("record_declaration", ChunkKind::Struct),
        ("annotation_type_declaration", ChunkKind::Trait),
    ],
    callables: &[
        ("method_declaration", ChunkKind::Method),
        ("constructor_declaration", ChunkKind::Method),
    ],
    leaves: &[],
    imports: &["import_declaration"],
    wrappers: &[],
    comments: &["line_comment", "block_comment", "comment"],
    attributes: &["annotation", "marker_annotation", "modifiers"],
    doc_prefixes: BLOCK_DOC,
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &["class_body", "interface_body", "enum_body"],
    docstring: false,
};

pub(crate) static C: LanguageSpec = LanguageSpec {
    separator: "::",
    containers: &[],
    callables: &[("function_definition", ChunkKind::Function)],
    leaves: &[
        ("struct_specifier", ChunkKind::Struct),
        ("union_specifier", ChunkKind::Struct),
        ("enum_specifier", ChunkKind::Enum),
        ("type_definition", ChunkKind::TypeAlias),
        ("preproc_def", ChunkKind::Const),
        ("preproc_function_def", ChunkKind::Function),
    ],
    imports: &["preproc_include"],
    wrappers: &["declaration"],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: BLOCK_DOC,
    // 结构体有 name 字段，函数只能沿 declarator 链下钻。
    name_sources: &[NameSource::Field("name"), NameSource::Declarator],
    body_fields: &["body"],
    body_kinds: &["compound_statement", "field_declaration_list"],
    docstring: false,
};

pub(crate) static CPP: LanguageSpec = LanguageSpec {
    separator: "::",
    containers: &[("namespace_definition", ChunkKind::Module)],
    callables: &[("function_definition", ChunkKind::Function)],
    leaves: &[
        ("class_specifier", ChunkKind::Struct),
        ("struct_specifier", ChunkKind::Struct),
        ("union_specifier", ChunkKind::Struct),
        ("enum_specifier", ChunkKind::Enum),
        ("type_definition", ChunkKind::TypeAlias),
        ("alias_declaration", ChunkKind::TypeAlias),
        ("concept_definition", ChunkKind::Trait),
        ("preproc_def", ChunkKind::Const),
    ],
    imports: &["preproc_include"],
    wrappers: &[
        "template_declaration",
        "linkage_specification",
        "declaration",
    ],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: BLOCK_DOC,
    name_sources: &[NameSource::Field("name"), NameSource::Declarator],
    body_fields: &["body"],
    body_kinds: &[
        "compound_statement",
        "declaration_list",
        "field_declaration_list",
    ],
    docstring: false,
};

pub(crate) static C_SHARP: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[
        ("namespace_declaration", ChunkKind::Module),
        ("file_scoped_namespace_declaration", ChunkKind::Module),
        ("class_declaration", ChunkKind::Struct),
        ("interface_declaration", ChunkKind::Trait),
        ("struct_declaration", ChunkKind::Struct),
        ("record_declaration", ChunkKind::Struct),
        ("enum_declaration", ChunkKind::Enum),
    ],
    callables: &[
        ("method_declaration", ChunkKind::Method),
        ("constructor_declaration", ChunkKind::Method),
        ("property_declaration", ChunkKind::Method),
        ("local_function_statement", ChunkKind::Function),
    ],
    leaves: &[("delegate_declaration", ChunkKind::TypeAlias)],
    imports: &["using_directive"],
    wrappers: &[],
    comments: &["comment"],
    attributes: &["attribute_list", "modifier"],
    doc_prefixes: BLOCK_DOC,
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &["declaration_list", "block", "accessor_list"],
    docstring: false,
};

pub(crate) static RUBY: LanguageSpec = LanguageSpec {
    separator: "::",
    containers: &[
        ("class", ChunkKind::Struct),
        ("module", ChunkKind::Module),
        ("singleton_class", ChunkKind::Struct),
    ],
    callables: &[
        ("method", ChunkKind::Method),
        ("singleton_method", ChunkKind::Method),
    ],
    leaves: &[],
    imports: &[],
    wrappers: &[],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: &[],
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &["body_statement"],
    docstring: false,
};

pub(crate) static PHP: LanguageSpec = LanguageSpec {
    // PHP 用 `\` 分隔命名空间、用 `::` 分隔类成员。限定名里出现的几乎都是后者，
    // 且 `Token::check` 正是调用点的写法，也是搜索时会输入的形式。
    separator: "::",
    containers: &[
        ("namespace_definition", ChunkKind::Module),
        ("class_declaration", ChunkKind::Struct),
        ("interface_declaration", ChunkKind::Trait),
        ("trait_declaration", ChunkKind::Trait),
        ("enum_declaration", ChunkKind::Enum),
    ],
    callables: &[
        ("function_definition", ChunkKind::Function),
        ("method_declaration", ChunkKind::Method),
    ],
    leaves: &[],
    imports: &["namespace_use_declaration"],
    wrappers: &[],
    comments: &["comment"],
    attributes: &["attribute_list"],
    doc_prefixes: BLOCK_DOC,
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &[
        "declaration_list",
        "compound_statement",
        "enum_declaration_list",
    ],
    docstring: false,
};

pub(crate) static SWIFT: LanguageSpec = LanguageSpec {
    separator: ".",
    // tree-sitter-swift 把 class / struct / enum / extension 统一表示为
    // `class_declaration`，无法从节点类别区分，签名里保留了真实关键字。
    containers: &[
        ("class_declaration", ChunkKind::Struct),
        ("protocol_declaration", ChunkKind::Trait),
    ],
    callables: &[
        ("function_declaration", ChunkKind::Function),
        ("protocol_function_declaration", ChunkKind::Method),
        ("init_declaration", ChunkKind::Method),
    ],
    leaves: &[("typealias_declaration", ChunkKind::TypeAlias)],
    imports: &["import_declaration"],
    wrappers: &[],
    comments: &["comment", "multiline_comment"],
    attributes: &["attribute", "modifiers"],
    doc_prefixes: BLOCK_DOC,
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &[
        "class_body",
        "enum_class_body",
        "protocol_body",
        "function_body",
    ],
    docstring: false,
};

pub(crate) static KOTLIN: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[
        ("class_declaration", ChunkKind::Struct),
        ("object_declaration", ChunkKind::Struct),
        ("companion_object", ChunkKind::Struct),
    ],
    callables: &[
        ("function_declaration", ChunkKind::Function),
        ("anonymous_initializer", ChunkKind::Method),
    ],
    leaves: &[("type_alias", ChunkKind::TypeAlias)],
    imports: &["import"],
    wrappers: &[],
    comments: &["line_comment", "multiline_comment", "comment"],
    attributes: &["annotation", "modifiers"],
    doc_prefixes: BLOCK_DOC,
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &["class_body", "enum_class_body", "function_body", "block"],
    docstring: false,
};

pub(crate) static SCALA: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[
        ("object_definition", ChunkKind::Struct),
        ("class_definition", ChunkKind::Struct),
        ("trait_definition", ChunkKind::Trait),
        ("enum_definition", ChunkKind::Enum),
    ],
    callables: &[
        ("function_definition", ChunkKind::Function),
        ("function_declaration", ChunkKind::Function),
    ],
    leaves: &[("type_definition", ChunkKind::TypeAlias)],
    imports: &["import_declaration"],
    wrappers: &[],
    comments: &["comment", "block_comment"],
    attributes: &["annotation"],
    doc_prefixes: BLOCK_DOC,
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &["template_body", "block"],
    docstring: false,
};

pub(crate) static LUA: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[],
    callables: &[("function_declaration", ChunkKind::Function)],
    leaves: &[],
    imports: &[],
    wrappers: &[],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: &[],
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &["block"],
    docstring: false,
};

pub(crate) static BASH: LanguageSpec = LanguageSpec {
    separator: "::",
    containers: &[],
    callables: &[("function_definition", ChunkKind::Function)],
    leaves: &[],
    imports: &[],
    wrappers: &[],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: &[],
    name_sources: &[NameSource::Field("name")],
    body_fields: &["body"],
    body_kinds: &["compound_statement"],
    docstring: false,
};

pub(crate) static CSS: LanguageSpec = LanguageSpec {
    separator: " ",
    containers: &[],
    callables: &[],
    // 选择器整段文本就是规则的名字，这样 `.btn-primary` 能走符号通路命中。
    leaves: &[
        ("rule_set", ChunkKind::Rule),
        ("keyframes_statement", ChunkKind::Rule),
    ],
    imports: &["import_statement"],
    // `@media` / `@supports` 本身不是定义，里面的规则才是。规则包在 `block` 里，
    // 因此 `block` 也要透明穿过；`rule_set` 是叶子节点，不会被递归进入，无冲突。
    wrappers: &["media_statement", "supports_statement", "block"],
    comments: &["comment"],
    attributes: &[],
    doc_prefixes: &[],
    name_sources: &[
        NameSource::ChildText("selectors"),
        NameSource::ChildText("keyframes_name"),
    ],
    body_fields: &["body"],
    body_kinds: &["block"],
    docstring: false,
};

pub(crate) static SQL: LanguageSpec = LanguageSpec {
    separator: ".",
    containers: &[],
    callables: &[("create_function", ChunkKind::Function)],
    leaves: &[
        ("create_table", ChunkKind::Struct),
        ("create_view", ChunkKind::Struct),
        ("create_index", ChunkKind::Rule),
        ("create_type", ChunkKind::TypeAlias),
    ],
    imports: &[],
    // 每条语句都包在 `statement` 里。
    wrappers: &["statement"],
    comments: &["comment", "marginalia"],
    attributes: &[],
    doc_prefixes: &[],
    name_sources: &[NameSource::DescendantText("object_reference")],
    body_fields: &["body"],
    body_kinds: &["column_definitions"],
    docstring: false,
};

#[cfg(test)]
mod tests {
    use crate::parser::parse;
    use crate::types::{ChunkKind, Language};

    /// 取出全部定义的限定名（含嵌套子定义），便于断言。
    fn qualified_names(language: Language, source: &str) -> Vec<String> {
        let parsed = parse(language, source).expect("parse");
        let mut out = Vec::new();
        fn push(definitions: &[crate::parser::Definition], out: &mut Vec<String>) {
            for definition in definitions {
                out.push(definition.qualified.clone());
                push(&definition.children, out);
            }
        }
        push(&parsed.definitions, &mut out);
        out
    }

    #[test]
    fn python_extracts_classes_methods_and_docstrings() {
        let source = r#"
import os
from typing import List

@app.route("/")
def handler(request):
    """处理请求。"""
    return 1

class Service:
    """服务。"""
    def run(self, x):
        return x

    @property
    def name(self):
        return "s"
"#;
        let names = qualified_names(Language::Python, source);
        assert!(names.contains(&"handler".to_string()));
        assert!(names.contains(&"Service".to_string()));
        assert!(names.contains(&"Service.run".to_string()));
        assert!(names.contains(&"Service.name".to_string()));

        let parsed = parse(Language::Python, source).expect("parse");
        let handler = parsed
            .definitions
            .iter()
            .find(|d| d.name == "handler")
            .expect("handler");
        assert_eq!(handler.doc.as_deref(), Some("处理请求。"));
        // 装饰器必须包含在代码块里。
        assert!(source[handler.start_byte..handler.end_byte].starts_with("@app.route"));

        let modules: Vec<&str> = parsed
            .imports
            .iter()
            .filter_map(|i| i.module.as_deref())
            .collect();
        assert_eq!(modules, vec!["os", "List"]);
    }

    #[test]
    fn go_extracts_functions_methods_and_type_specs() {
        let source = r#"
package main

import "fmt"

// Server 是服务器。
type Server struct {
    Addr string
}

const Version = "1"

func NewServer(addr string) *Server { return nil }

func (s *Server) Start() error { return nil }
"#;
        let names = qualified_names(Language::Go, source);
        assert!(names.contains(&"Server".to_string()), "{names:?}");
        assert!(names.contains(&"Version".to_string()), "{names:?}");
        assert!(names.contains(&"NewServer".to_string()), "{names:?}");
        assert!(names.contains(&"Server.Start".to_string()), "{names:?}");

        let parsed = parse(Language::Go, source).expect("parse");
        let server = parsed
            .definitions
            .iter()
            .find(|d| d.name == "Server")
            .expect("Server");
        // Go 没有专门的文档注释语法，紧邻的 `//` 就是文档。
        assert_eq!(server.doc.as_deref(), Some("Server 是服务器。"));
    }

    #[test]
    fn java_extracts_nested_classes_and_methods() {
        let source = r#"
package com.example;
import java.util.List;

/** 服务。 */
public class Service {
    public void run() {}
    static class Inner {
        void go() {}
    }
}
interface Runner { void go(); }
enum Color { RED }
"#;
        let names = qualified_names(Language::Java, source);
        assert!(names.contains(&"Service".to_string()), "{names:?}");
        assert!(names.contains(&"Service.run".to_string()), "{names:?}");
        assert!(names.contains(&"Service.Inner".to_string()), "{names:?}");
        assert!(names.contains(&"Service.Inner.go".to_string()), "{names:?}");
        assert!(names.contains(&"Runner".to_string()), "{names:?}");
        assert!(names.contains(&"Color".to_string()), "{names:?}");

        let parsed = parse(Language::Java, source).expect("parse");
        let service = parsed
            .definitions
            .iter()
            .find(|d| d.name == "Service")
            .expect("Service");
        assert_eq!(service.doc.as_deref(), Some("服务。"));
    }

    /// C 的函数名藏在 `declarator` 链里，不是 `name` 字段。
    #[test]
    fn c_extracts_function_names_through_the_declarator_chain() {
        let source = r#"
#include <stdio.h>

struct Point { int x; };

static int helper(int a) { return a; }

int main(int argc, char** argv) { return 0; }
"#;
        let names = qualified_names(Language::C, source);
        assert!(names.contains(&"helper".to_string()), "{names:?}");
        assert!(names.contains(&"main".to_string()), "{names:?}");
        assert!(names.contains(&"Point".to_string()), "{names:?}");
    }

    #[test]
    fn cpp_extracts_namespaced_members() {
        let source = r#"
namespace app {
class Server {
public:
    void start();
};
void Server::start() {}
}
"#;
        let names = qualified_names(Language::Cpp, source);
        assert!(names.contains(&"app".to_string()), "{names:?}");
        assert!(names.contains(&"app::Server".to_string()), "{names:?}");
        // 域外定义的成员函数带上了限定名。
        assert!(
            names.contains(&"app::Server::start".to_string()),
            "{names:?}"
        );
    }

    #[test]
    fn csharp_extracts_namespace_class_and_members() {
        let source = r#"
using System;
namespace App {
    public class Service {
        public void Run() {}
        public int Count { get; set; }
    }
    public interface IRunner { void Go(); }
}
"#;
        let names = qualified_names(Language::CSharp, source);
        assert!(names.contains(&"App.Service".to_string()), "{names:?}");
        assert!(names.contains(&"App.Service.Run".to_string()), "{names:?}");
        assert!(
            names.contains(&"App.Service.Count".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"App.IRunner".to_string()), "{names:?}");
    }

    #[test]
    fn ruby_extracts_modules_classes_and_methods() {
        let source = r#"
module Helpers
  def helper; end
end

class Service
  def initialize(x)
    @x = x
  end
  def self.build; end
end
"#;
        let names = qualified_names(Language::Ruby, source);
        assert!(names.contains(&"Helpers".to_string()), "{names:?}");
        assert!(names.contains(&"Helpers::helper".to_string()), "{names:?}");
        assert!(
            names.contains(&"Service::initialize".to_string()),
            "{names:?}"
        );
        assert!(names.contains(&"Service::build".to_string()), "{names:?}");
    }

    #[test]
    fn php_extracts_classes_traits_and_methods() {
        let source = r#"<?php
namespace App;
use App\Contracts\Runner;

class Service implements Runner {
    public function go() {}
    public static function build(): self {}
}

trait Loggable { public function log() {} }

function helper(int $a): int { return $a; }
"#;
        let names = qualified_names(Language::Php, source);
        assert!(names.iter().any(|n| n.ends_with("Service")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("go")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("Loggable")), "{names:?}");
        assert!(names.iter().any(|n| n.ends_with("helper")), "{names:?}");
    }

    #[test]
    fn swift_extracts_types_and_functions() {
        let source = r#"
import Foundation

protocol Runner { func go() }

class Service: Runner {
    func go() {}
    static func build() -> Service { return Service() }
}

func topLevel(a: Int) -> Int { return a }
"#;
        let names = qualified_names(Language::Swift, source);
        assert!(names.contains(&"Runner".to_string()), "{names:?}");
        assert!(names.contains(&"Service".to_string()), "{names:?}");
        assert!(names.contains(&"Service.go".to_string()), "{names:?}");
        assert!(names.contains(&"topLevel".to_string()), "{names:?}");
    }

    #[test]
    fn kotlin_extracts_classes_objects_and_functions() {
        let source = r#"
package app

class Service {
    fun go() {}
}

object Singleton {
    fun build() {}
}

fun topLevel(a: Int): Int = a
"#;
        let names = qualified_names(Language::Kotlin, source);
        assert!(names.contains(&"Service".to_string()), "{names:?}");
        assert!(names.contains(&"Singleton".to_string()), "{names:?}");
        assert!(names.contains(&"topLevel".to_string()), "{names:?}");
    }

    #[test]
    fn scala_extracts_objects_traits_and_classes() {
        let source = r#"
package app

object Main {
  def run(): Unit = ()
}

trait Runner { def go(): Unit }

class Service extends Runner {
  def go(): Unit = ()
}
"#;
        let names = qualified_names(Language::Scala, source);
        assert!(names.contains(&"Main".to_string()), "{names:?}");
        assert!(names.contains(&"Main.run".to_string()), "{names:?}");
        assert!(names.contains(&"Runner".to_string()), "{names:?}");
        assert!(names.contains(&"Service.go".to_string()), "{names:?}");
    }

    #[test]
    fn lua_and_bash_extract_functions() {
        let lua = "local function helper(a)\n  return a\nend\n\nfunction M.run()\nend\n";
        let names = qualified_names(Language::Lua, lua);
        assert!(names.contains(&"helper".to_string()), "{names:?}");
        assert!(names.contains(&"M.run".to_string()), "{names:?}");

        let bash = "# 部署。\ndeploy() {\n  echo hi\n}\n\nfunction build() {\n  echo bye\n}\n";
        let names = qualified_names(Language::Bash, bash);
        assert!(names.contains(&"deploy".to_string()), "{names:?}");
        assert!(names.contains(&"build".to_string()), "{names:?}");
    }

    /// CSS 选择器要能当作符号命中，这是前端项目查样式的主要方式。
    #[test]
    fn css_uses_selectors_as_symbols() {
        let source = r#"
.btn-primary {
  color: red;
}

#header .nav > li:hover { color: blue; }

@media (max-width: 600px) {
  .btn-primary { color: green; }
}
"#;
        let names = qualified_names(Language::Css, source);
        assert!(names.contains(&".btn-primary".to_string()), "{names:?}");
        assert!(
            names.contains(&"#header .nav > li:hover".to_string()),
            "{names:?}"
        );
        // @media 内部的规则也要被提取，不能整块吞掉。
        assert_eq!(
            names.iter().filter(|n| *n == ".btn-primary").count(),
            2,
            "媒体查询内的同名规则应单独成项: {names:?}"
        );
        let parsed = parse(Language::Css, source).expect("parse");
        assert!(parsed.definitions.iter().all(|d| d.kind == ChunkKind::Rule));
    }

    #[test]
    fn sql_extracts_ddl_object_names() {
        let source = r#"
CREATE TABLE users (
  id INT PRIMARY KEY,
  name TEXT
);

CREATE INDEX idx_users_name ON users (name);

CREATE VIEW active_users AS SELECT * FROM users;
"#;
        let names = qualified_names(Language::Sql, source);
        assert!(names.contains(&"users".to_string()), "{names:?}");
        assert!(names.contains(&"active_users".to_string()), "{names:?}");
    }

    /// 每种语言的行号都必须与内容严格对应，否则结果不可信。
    #[test]
    fn line_numbers_match_content_for_every_generic_language() {
        let cases: &[(Language, &str)] = &[
            (
                Language::Python,
                "def a():\n    return 1\n\nclass B:\n    def c(self):\n        pass\n",
            ),
            (
                Language::Go,
                "package m\n\nfunc a() int {\n    return 1\n}\n",
            ),
            (Language::Java, "class A {\n    void b() {}\n}\n"),
            (Language::C, "int a(void) {\n    return 1;\n}\n"),
            (
                Language::Cpp,
                "namespace n {\nclass A {\n    void b();\n};\n}\n",
            ),
            (Language::CSharp, "class A {\n    void B() {}\n}\n"),
            (Language::Ruby, "class A\n  def b\n    1\n  end\nend\n"),
            (
                Language::Php,
                "<?php\nclass A {\n    public function b() {}\n}\n",
            ),
            (Language::Swift, "class A {\n    func b() {}\n}\n"),
            (Language::Kotlin, "class A {\n    fun b() {}\n}\n"),
            (Language::Scala, "object A {\n  def b(): Unit = ()\n}\n"),
            (Language::Lua, "function a()\n  return 1\nend\n"),
            (Language::Bash, "a() {\n  echo hi\n}\n"),
            (Language::Css, ".a {\n  color: red;\n}\n"),
            (Language::Sql, "CREATE TABLE a (\n  id INT\n);\n"),
        ];
        for (language, source) in cases {
            let parsed = parse(*language, source).expect("parse");
            assert!(
                !parsed.definitions.is_empty(),
                "{} 应至少提取到一个定义",
                language.as_str()
            );
            check_lines(*language, source, &parsed.definitions);
        }
    }

    fn check_lines(language: Language, source: &str, definitions: &[crate::parser::Definition]) {
        for definition in definitions {
            let text = &source[definition.start_byte..definition.end_byte];
            assert_eq!(
                definition.end_line - definition.start_line + 1,
                text.lines().count() as u32,
                "{} 的定义 {} 行号范围与内容不一致",
                language.as_str(),
                definition.name
            );
            check_lines(language, source, &definition.children);
        }
    }

    /// 解析失败或语法有错时不能 panic，也不能返回越界字节范围。
    #[test]
    fn broken_sources_never_panic_and_stay_in_bounds() {
        let cases: &[(Language, &str)] = &[
            (Language::Python, "def broken(:\n  ???\n"),
            (Language::Go, "func (\n"),
            (Language::Java, "class {{{\n"),
            (Language::C, "int (((;\n"),
            (Language::Cpp, "template<\n"),
            (Language::CSharp, "namespace {\n"),
            (Language::Ruby, "class\nend end\n"),
            (Language::Php, "<?php class {\n"),
            (Language::Swift, "func (((\n"),
            (Language::Kotlin, "class {{{\n"),
            (Language::Scala, "object {{{\n"),
            (Language::Lua, "function (((\n"),
            (Language::Bash, "a() {\n"),
            (Language::Css, ".a { color\n"),
            (Language::Sql, "CREATE TABLE (\n"),
        ];
        for (language, source) in cases {
            let parsed = parse(*language, source).expect("解析不得返回错误");
            for definition in &parsed.definitions {
                assert!(
                    definition.end_byte <= source.len(),
                    "{} 越界",
                    language.as_str()
                );
                assert!(source
                    .get(definition.start_byte..definition.end_byte)
                    .is_some());
            }
        }
    }
}

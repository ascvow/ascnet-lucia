//! Rust 结构化解析，基于 `syn`。
//!
//! `syn` 的行号来自 `proc-macro2` 的 fallback span，必须开启其 `span-locations` feature；
//! 该 feature 只在非过程宏环境下生效，而插件正是这种环境。

use super::{Declaration, ParseOutput};
use crate::model::SymbolKind;
use syn::spanned::Spanned;

/// 解析 Rust 源码。
///
/// 语法错误时返回 `None`，由调用方降级为文本分块。
pub fn parse(source: &str) -> Option<ParseOutput> {
    let file = syn::parse_file(source).ok()?;
    let mut output = ParseOutput::default();
    // `//!` 内层文档注释挂在 `File` 上，描述的是整个文件的职责。
    output.file_doc = doc_of(&file.attrs);
    walk_items(&file.items, None, &mut output);
    Some(output)
}

/// 递归遍历条目列表。
fn walk_items(items: &[syn::Item], parent: Option<usize>, output: &mut ParseOutput) {
    for item in items {
        walk_item(item, parent, output);
    }
}

/// 处理单个条目。
fn walk_item(item: &syn::Item, parent: Option<usize>, output: &mut ParseOutput) {
    match item {
        syn::Item::Fn(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.sig.ident.to_string(),
                SymbolKind::Function,
                parent,
                None,
            );
        }
        syn::Item::Struct(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::Struct,
                parent,
                None,
            );
        }
        syn::Item::Union(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::Struct,
                parent,
                None,
            );
        }
        syn::Item::Enum(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::Enum,
                parent,
                None,
            );
        }
        syn::Item::Type(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::TypeAlias,
                parent,
                None,
            );
        }
        syn::Item::Const(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::Constant,
                parent,
                None,
            );
        }
        syn::Item::Static(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::Constant,
                parent,
                None,
            );
        }
        syn::Item::Macro(node) => {
            // `macro_rules! name` 之外的宏调用没有名字，不构成可检索符号。
            if let Some(ident) = node.ident.as_ref() {
                push(
                    output,
                    &node.attrs,
                    node,
                    ident.to_string(),
                    SymbolKind::Other,
                    parent,
                    None,
                );
            }
        }
        syn::Item::Trait(node) => {
            let index = push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::Trait,
                parent,
                None,
            );
            for trait_item in &node.items {
                walk_trait_item(trait_item, Some(index), output);
            }
        }
        syn::Item::Impl(node) => {
            // impl 块的检索目标是被实现的类型：用户找的是"这个类型的方法在哪"。
            let Some(name) = type_name(&node.self_ty) else {
                return;
            };
            let owner = node
                .trait_
                .as_ref()
                .and_then(|(path, _)| path.segments.last())
                .map(|segment| segment.ident.to_string());
            let index = push(
                output,
                &node.attrs,
                node,
                name,
                SymbolKind::Impl,
                parent,
                owner,
            );
            for impl_item in &node.items {
                walk_impl_item(impl_item, Some(index), output);
            }
        }
        syn::Item::Mod(node) => {
            let index = push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::Module,
                parent,
                None,
            );
            if let Some((_, items)) = node.content.as_ref() {
                walk_items(items, Some(index), output);
            }
        }
        syn::Item::Use(node) => {
            if let Some(text) = use_tree_text(&node.tree) {
                push_import(output, text);
            }
        }
        _ => {}
    }
}

/// 处理 trait 内部条目。
fn walk_trait_item(item: &syn::TraitItem, parent: Option<usize>, output: &mut ParseOutput) {
    match item {
        syn::TraitItem::Fn(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.sig.ident.to_string(),
                SymbolKind::Function,
                parent,
                None,
            );
        }
        syn::TraitItem::Const(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::Constant,
                parent,
                None,
            );
        }
        syn::TraitItem::Type(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::TypeAlias,
                parent,
                None,
            );
        }
        _ => {}
    }
}

/// 处理 impl 内部条目。
fn walk_impl_item(item: &syn::ImplItem, parent: Option<usize>, output: &mut ParseOutput) {
    match item {
        syn::ImplItem::Fn(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.sig.ident.to_string(),
                SymbolKind::Function,
                parent,
                None,
            );
        }
        syn::ImplItem::Const(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::Constant,
                parent,
                None,
            );
        }
        syn::ImplItem::Type(node) => {
            push(
                output,
                &node.attrs,
                node,
                node.ident.to_string(),
                SymbolKind::TypeAlias,
                parent,
                None,
            );
        }
        _ => {}
    }
}

/// 记录一个声明，返回它在结果数组中的下标。
fn push<T: Spanned>(
    output: &mut ParseOutput,
    attrs: &[syn::Attribute],
    node: &T,
    name: String,
    kind: SymbolKind,
    parent: Option<usize>,
    owner: Option<String>,
) -> usize {
    let span = node.span();
    // 测试属性向下继承：`#[cfg(test)] mod tests` 里的每个函数都是测试代码，
    // 但只有模块本身带属性。
    let inherited = parent
        .and_then(|index| output.declarations.get(index))
        .is_some_and(|declaration| declaration.is_test);
    let is_test = inherited || has_test_attribute(attrs);
    output.declarations.push(Declaration {
        name,
        kind,
        start_line: u32::try_from(span.start().line).unwrap_or(1).max(1),
        end_line: u32::try_from(span.end().line).unwrap_or(1).max(1),
        signature: None,
        doc_comment: doc_of(attrs),
        parent,
        owner,
        is_test,
    });
    output.declarations.len() - 1
}

/// 从属性中提取文档注释。
///
/// `///` 与 `//!` 在 `syn` 里都已经归一化成 `#[doc = "..."]`，因此只需读该属性。
fn doc_of(attrs: &[syn::Attribute]) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let syn::Meta::NameValue(meta) = &attr.meta else {
            continue;
        };
        let syn::Expr::Lit(expr) = &meta.value else {
            continue;
        };
        if let syn::Lit::Str(text) = &expr.lit {
            let line = text.value();
            let line = line.trim();
            if !line.is_empty() {
                parts.push(line.to_string());
            }
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// 取类型的可检索名称，例如 `Wrapper<T>` 取 `Wrapper`。
fn type_name(ty: &syn::Type) -> Option<String> {
    match ty {
        syn::Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        syn::Type::Reference(reference) => type_name(&reference.elem),
        syn::Type::Group(group) => type_name(&group.elem),
        syn::Type::Paren(paren) => type_name(&paren.elem),
        _ => None,
    }
}

/// 把 `use` 树还原成可读文本。
fn use_tree_text(tree: &syn::UseTree) -> Option<String> {
    fn walk(tree: &syn::UseTree, prefix: &str, out: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(node) => {
                let next = join(prefix, &node.ident.to_string());
                walk(&node.tree, &next, out);
            }
            syn::UseTree::Name(node) => out.push(join(prefix, &node.ident.to_string())),
            syn::UseTree::Rename(node) => out.push(join(prefix, &node.ident.to_string())),
            syn::UseTree::Glob(_) => out.push(join(prefix, "*")),
            syn::UseTree::Group(node) => {
                for item in &node.items {
                    walk(item, prefix, out);
                }
            }
        }
    }
    fn join(prefix: &str, segment: &str) -> String {
        if prefix.is_empty() {
            segment.to_string()
        } else {
            format!("{prefix}::{segment}")
        }
    }

    let mut collected = Vec::new();
    walk(tree, "", &mut collected);
    (!collected.is_empty()).then(|| collected.join(", "))
}

/// 追加一条 import，去重。
/// 判断属性列表是否把该条目标记为测试代码。
///
/// 覆盖 `#[test]`、`#[cfg(test)]`、`#[bench]` 以及 `#[tokio::test]` 这类带路径的测试宏。
fn has_test_attribute(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attribute| {
        let path = attribute.path();
        if path.is_ident("test") || path.is_ident("bench") {
            return true;
        }
        // `#[tokio::test]`、`#[async_std::test]` 等：末段是 `test` 即可。
        if path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "test")
        {
            return true;
        }
        // `#[cfg(test)]`：只认这一种 cfg，避免把 `#[cfg(feature = "x")]` 也算进去。
        path.is_ident("cfg")
            && attribute
                .parse_args::<syn::Meta>()
                .is_ok_and(|meta| meta.path().is_ident("test"))
    })
}

fn push_import(output: &mut ParseOutput, text: String) {
    if !output.imports.contains(&text) {
        output.imports.push(text);
    }
}

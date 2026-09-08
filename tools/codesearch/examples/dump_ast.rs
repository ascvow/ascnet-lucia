//! 输出新增语法的真实节点结构，供适配层维护与测试定位使用。

/// 从参数读取语言与源码文件；语法加载、读取或建树失败时向调用方返回错误。
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let language = match args.get(1).map(String::as_str) {
        Some("dart") => tree_sitter_dart::LANGUAGE.into(),
        Some("elixir") => tree_sitter_elixir::LANGUAGE.into(),
        Some("graphql") => tree_sitter_graphql::LANGUAGE.into(),
        Some("haskell") => tree_sitter_haskell::LANGUAGE.into(),
        Some("hcl") => tree_sitter_hcl::LANGUAGE.into(),
        Some("html") => tree_sitter_html::LANGUAGE.into(),
        Some("zig") => tree_sitter_zig::LANGUAGE.into(),
        _ => {
            return Err(
                "用法: dump_ast <dart|elixir|graphql|haskell|hcl|html|zig> <源码文件>".into(),
            )
        }
    };
    let source = std::fs::read_to_string(args.get(2).ok_or("缺少源码文件")?)?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&language)?;
    let tree = parser.parse(&source, None).ok_or("语法树构建失败")?;
    println!("{}", tree.root_node().to_sexp());
    Ok(())
}

//! 结构化解析的单元测试。
//!
//! 覆盖 Phase 3 的 Definition of Done：五种语言的符号与边界提取、限定名推导、文档注释、
//! import、语法错误降级、未覆盖语言降级、以及输出确定性。

use super::*;
use crate::model::RepositoryIdentity;

/// 构造测试用仓库身份。
fn repository() -> RepositoryIdentity {
    RepositoryIdentity::from_workspace_root("/repo/test").expect("测试仓库身份")
}

/// 解析一段源码。
fn parse(path: &str, language: Language, source: &str) -> ParsedFile {
    parse_file(&repository(), path, language, source)
}

/// 返回全部限定名。
fn qualified(parsed: &ParsedFile) -> Vec<&str> {
    parsed
        .symbols
        .iter()
        .map(|symbol| symbol.qualified_name.as_str())
        .collect()
}

/// 按限定名查找符号。
fn find<'a>(parsed: &'a ParsedFile, qualified_name: &str) -> &'a crate::model::Symbol {
    parsed
        .symbols
        .iter()
        .find(|symbol| symbol.qualified_name == qualified_name)
        .unwrap_or_else(|| {
            panic!(
                "未找到符号 {qualified_name}，实际有 {:?}",
                qualified(parsed)
            )
        })
}

// ---------------------------------------------------------------------------
// Rust
// ---------------------------------------------------------------------------

/// Rust 的各类顶层声明必须被识别为正确的种类。
#[test]
fn rust_extracts_top_level_items() {
    let parsed = parse(
        "src/lib.rs",
        Language::Rust,
        r#"
pub struct Manager { field: u32 }
pub enum Mode { A, B }
pub trait Check { fn run(&self); }
pub const LIMIT: u32 = 10;
pub type Alias = u32;
pub fn free() {}
"#,
    );

    assert_eq!(parsed.tier, ParseTier::Structural);
    assert_eq!(find(&parsed, "Manager").kind, SymbolKind::Struct);
    assert_eq!(find(&parsed, "Mode").kind, SymbolKind::Enum);
    assert_eq!(find(&parsed, "Check").kind, SymbolKind::Trait);
    assert_eq!(find(&parsed, "LIMIT").kind, SymbolKind::Constant);
    assert_eq!(find(&parsed, "Alias").kind, SymbolKind::TypeAlias);
    assert_eq!(find(&parsed, "free").kind, SymbolKind::Function);
}

/// impl 块内的方法必须挂在被实现的类型下面。
#[test]
fn rust_methods_are_qualified_by_type() {
    let parsed = parse(
        "src/permission.rs",
        Language::Rust,
        r#"
pub struct PermissionManager;

impl PermissionManager {
    /// 判断工具调用是否需要用户审批。
    pub fn check(&self, tool: &str) -> bool {
        tool.is_empty()
    }
}
"#,
    );

    let check = find(&parsed, "PermissionManager::check");
    assert_eq!(check.kind, SymbolKind::Function);
    let chunk = parsed
        .chunks
        .iter()
        .find(|chunk| chunk.qualified_symbol.as_deref() == Some("PermissionManager::check"))
        .expect("应有对应 chunk");
    assert_eq!(chunk.parent_symbol.as_deref(), Some("PermissionManager"));
    assert_eq!(
        chunk.doc_comment.as_deref(),
        Some("判断工具调用是否需要用户审批。")
    );
}

/// 相邻的短方法必须共享一个语义复合块，同时保留各自的原子定义。
#[test]
fn short_sibling_methods_share_semantic_group() {
    let source = r#"
pub struct Service;

impl Service {
    pub fn create(&self) {
        self.persist();
    }


    pub fn notify(&self) {
        self.publish();
    }
}
"#;
    let parsed = parse("src/service.rs", Language::Rust, source);
    let create = parsed
        .chunks
        .iter()
        .find(|chunk| chunk.qualified_symbol.as_deref() == Some("Service::create"))
        .expect("应保留 create 原子块");
    let notify = parsed
        .chunks
        .iter()
        .find(|chunk| chunk.qualified_symbol.as_deref() == Some("Service::notify"))
        .expect("应保留 notify 原子块");
    let group_id = create
        .semantic_group_id
        .as_deref()
        .expect("短方法应归入语义复合块");

    assert_eq!(notify.semantic_group_id.as_deref(), Some(group_id));
    assert!(!create.semantic_only && !notify.semantic_only);
    let group = parsed
        .chunks
        .iter()
        .find(|chunk| chunk.id == group_id)
        .expect("应生成语义复合块");
    assert!(group.semantic_only);
    assert_eq!(group.parent_symbol.as_deref(), Some("Service"));
    assert_eq!(group.start_line, create.start_line);
    assert_eq!(group.end_line, notify.end_line);
    assert!(
        find(&parsed, "Service::create").start_line < find(&parsed, "Service::notify").start_line
    );
}

/// 语义复合块 ID 不得随文件前部插入空行而变化。
#[test]
fn semantic_group_id_is_stable_across_line_shifts() {
    let source = "fn first() {}\n\nfn second() {}\n";
    let shifted = "// 文件说明\n\nfn first() {}\n\nfn second() {}\n";
    let first = parse("src/group.rs", Language::Rust, source);
    let second = parse("src/group.rs", Language::Rust, shifted);
    let first_group = first
        .chunks
        .iter()
        .find(|chunk| chunk.semantic_only)
        .expect("应生成复合块");
    let second_group = second
        .chunks
        .iter()
        .find(|chunk| chunk.semantic_only)
        .expect("行号平移后仍应生成复合块");

    assert_eq!(first_group.id, second_group.id);
    assert_eq!(first_group.content_hash, second_group.content_hash);
    assert_ne!(first_group.start_line, second_group.start_line);
}

/// trait 实现的 impl 块必须以被实现的类型命名，trait 名记入 owner。
#[test]
fn rust_trait_impl_is_named_after_the_type() {
    let parsed = parse(
        "src/display.rs",
        Language::Rust,
        r#"
impl std::fmt::Display for Wrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { Ok(()) }
}
"#,
    );

    // impl 自身以类型命名；owner 让限定名带上 trait，便于按 trait 检索实现。
    assert!(qualified(&parsed)
        .iter()
        .any(|name| name.contains("Wrapper")));
    assert!(qualified(&parsed)
        .iter()
        .any(|name| name.contains("Display") && name.contains("fmt")));
}

/// 模块嵌套必须体现在限定名中。
#[test]
fn rust_nested_modules_build_qualified_names() {
    let parsed = parse(
        "src/lib.rs",
        Language::Rust,
        "pub mod outer { pub mod inner { pub fn deep() {} } }",
    );
    assert!(qualified(&parsed).contains(&"outer::inner::deep"));
}

/// Rust 的行区间必须覆盖整个声明体。
#[test]
fn rust_line_range_covers_whole_item() {
    let parsed = parse(
        "src/a.rs",
        Language::Rust,
        "fn first() {\n    let x = 1;\n    let y = 2;\n}\nfn second() {}\n",
    );
    let first = find(&parsed, "first");
    assert_eq!((first.start_line, first.end_line), (1, 4));
    assert_eq!(find(&parsed, "second").start_line, 5);
}

/// use 语句必须被收集为 import。
#[test]
fn rust_collects_use_statements() {
    let parsed = parse(
        "src/a.rs",
        Language::Rust,
        "use std::collections::HashMap;\nuse serde::{Serialize, Deserialize};\nfn a() {}\n",
    );
    let imports = &parsed.chunks[0].imports;
    assert!(imports.iter().any(|item| item.contains("HashMap")));
    assert!(imports.iter().any(|item| item.contains("Serialize")));
}

// ---------------------------------------------------------------------------
// TypeScript / JavaScript
// ---------------------------------------------------------------------------

/// TypeScript 的各类导出声明必须被识别。
#[test]
fn typescript_extracts_exported_declarations() {
    let parsed = parse(
        "src/app.ts",
        Language::TypeScript,
        r#"
export function handle(): void {}
export class Service { run(): void {} }
export interface Options { name: string }
export type Alias = string;
export enum Level { Low, High }
export const config = { a: 1 };
"#,
    );

    assert_eq!(parsed.tier, ParseTier::Structural);
    assert_eq!(find(&parsed, "handle").kind, SymbolKind::Function);
    assert_eq!(find(&parsed, "Service").kind, SymbolKind::Struct);
    assert_eq!(find(&parsed, "Service.run").kind, SymbolKind::Function);
    assert_eq!(find(&parsed, "Options").kind, SymbolKind::Trait);
    assert_eq!(find(&parsed, "Alias").kind, SymbolKind::TypeAlias);
    assert_eq!(find(&parsed, "Level").kind, SymbolKind::Enum);
    assert_eq!(find(&parsed, "config").kind, SymbolKind::Constant);
}

/// 箭头函数赋值必须归类为函数而不是常量。
#[test]
fn typescript_arrow_functions_are_functions() {
    let parsed = parse(
        "src/util.ts",
        Language::TypeScript,
        "export const compute = (a: number) => a * 2;\nconst plain = 42;\n",
    );
    assert_eq!(find(&parsed, "compute").kind, SymbolKind::Function);
    assert_eq!(find(&parsed, "plain").kind, SymbolKind::Constant);
}

/// 无注释对象服务须提取独立方法、嵌套限定名，且普通接口字段不能冒充函数。
#[test]
fn typescript_object_services_preserve_method_boundaries() {
    let parsed = parse(
        "src/service.ts",
        Language::TypeScript,
        r#"
export const Service = ({
  async create() { return 1; },
  remove: (id: number) => id,
  nested: { run: function () { return true; } },
  [dynamicName]() {},
  ...external,
} satisfies Record<string, unknown>);
interface Result { amount: number; run(): void; }
"#,
    );
    for name in [
        "Service.create",
        "Service.remove",
        "Service.nested.run",
        "Result.run",
    ] {
        assert_eq!(find(&parsed, name).kind, SymbolKind::Function);
    }
    assert_eq!(find(&parsed, "Service.create").start_line, 3);
    assert_eq!(find(&parsed, "Service.create").end_line, 3);
    assert_eq!(find(&parsed, "Result.amount").kind, SymbolKind::Other);
    assert!(!qualified(&parsed).contains(&"Service.dynamicName"));
}

/// 未被 export 包装的顶层声明同样必须被识别。
#[test]
fn typescript_handles_unexported_declarations() {
    let parsed = parse(
        "src/a.ts",
        Language::TypeScript,
        "function internal() {}\nclass Local {}\n",
    );
    assert!(qualified(&parsed).contains(&"internal"));
    assert!(qualified(&parsed).contains(&"Local"));
}

/// TSX 必须能解析而不是整份降级。
#[test]
fn typescript_parses_tsx() {
    let parsed = parse(
        "src/App.tsx",
        Language::TypeScript,
        "export const App = () => <div className=\"a\">hi</div>;\n",
    );
    assert_eq!(parsed.tier, ParseTier::Structural);
    assert_eq!(find(&parsed, "App").kind, SymbolKind::Function);
}

/// JavaScript 里的 JSX 必须能解析。
#[test]
fn javascript_parses_jsx() {
    let parsed = parse(
        "src/App.js",
        Language::JavaScript,
        "export function App() { return <div>hi</div>; }\n",
    );
    assert_eq!(parsed.tier, ParseTier::Structural);
    assert!(qualified(&parsed).contains(&"App"));
}

/// namespace 嵌套必须体现在限定名中。
#[test]
fn typescript_namespace_builds_qualified_names() {
    let parsed = parse(
        "src/ns.ts",
        Language::TypeScript,
        "namespace Outer { export function inner() {} }\n",
    );
    assert!(qualified(&parsed).contains(&"Outer.inner"));
}

/// import 必须被收集。
#[test]
fn typescript_collects_imports() {
    let parsed = parse(
        "src/a.ts",
        Language::TypeScript,
        "import { readFile } from 'node:fs';\nexport function a() {}\n",
    );
    assert!(parsed.chunks[0]
        .imports
        .iter()
        .any(|item| item.contains("node:fs")));
}

// ---------------------------------------------------------------------------
// Python
/// Vue 脚本必须独立定位，模板与样式仍作为文本保留，中文不得改变源码行号。
#[test]
fn vue_scripts_keep_original_lines_and_non_script_text() {
    let source = "<template><view>中文页面</view></template>\n<script setup lang=\"ts\">\nconst pay = async () => {\n  return 1;\n};\n</script>\n<style>.title { color: red; }</style>\n";
    let parsed = parse("src/Page.vue", Language::Vue, source);
    assert_eq!(parsed.tier, ParseTier::Structural);
    let pay = find(&parsed, "pay");
    assert_eq!((pay.start_line, pay.end_line), (3, 5));
    assert!(parsed.chunks.iter().any(|c| c.semantic_only));
    assert!(parsed
        .chunks
        .iter()
        .any(|c| c.symbol_name.is_none() && c.start_line == 1));
    assert!(parsed
        .chunks
        .iter()
        .any(|c| c.symbol_name.is_none() && c.end_line == 7));
    let lines: Vec<_> = source.lines().collect();
    for chunk in &parsed.chunks {
        let body = lines[chunk.start_line as usize - 1..chunk.end_line as usize].join("\n");
        assert_eq!(chunk.content_hash, crate::repository::content_hash(&body));
    }
}

/// 模板中的脚本外观、HTML 注释和外链脚本不能制造可执行函数。
#[test]
fn vue_ignores_fake_external_and_unknown_scripts() {
    let source = "<!-- <script>function fake() {}</script> -->\n<template><div title=\"<script>\">页面</div><script>function nested() {}</script></template>\n<script src=\"./remote.js\">function remote() {}</script>\n<script lang=\"coffee\">function unknown() {}</script>\n<script>function real() {}</script>";
    let parsed = parse("src/Page.vue", Language::Vue, source);
    assert_eq!(qualified(&parsed), vec!["real"]);
    assert_eq!(find(&parsed, "real").start_line, 5);
}

/// uni-app renderjs 与普通脚本均需解析，Options API 方法具有父限定名。
#[test]
fn nvue_multiple_scripts_and_options_methods_are_indexed() {
    let source = "<template><input/><view>视频</view></template>\n<script>export default { methods: { play() { const nested = () => 1; } } };</script>\n<script module=\"video\" lang=\"renderjs\">\nexport default { mounted() {} };\n</script>";
    let parsed = parse("src/Page.nvue", Language::Vue, source);
    assert!(qualified(&parsed).contains(&"default.methods.play"));
    assert!(qualified(&parsed).contains(&"default.methods.play.nested"));
    assert!(qualified(&parsed).contains(&"default.mounted"));
    assert_eq!(find(&parsed, "default.mounted").start_line, 4);
}

/// Hook 内嵌套方法不再只能以整个工厂函数返回。
#[test]
fn javascript_nested_hook_functions_have_qualified_names() {
    let parsed = parse("src/hook.js", Language::JavaScript, "export const usePayment = () => {\n const purchase = async () => {};\n return { purchase };\n};\n");
    assert_eq!(
        find(&parsed, "usePayment.purchase").kind,
        SymbolKind::Function
    );
    assert_eq!(find(&parsed, "usePayment.purchase").start_line, 2);
}

/// 匿名回调槽不制造假方法名，但其内部命名函数保留词法父级。
#[test]
fn anonymous_callback_slots_do_not_replace_business_methods() {
    let parsed = parse("src/modal.js", Language::JavaScript, "export const cancelOrder = () => { showModal({ success: () => { const retry = () => 1; } }); };");
    assert!(qualified(&parsed).contains(&"cancelOrder.retry"));
    assert!(!qualified(&parsed).contains(&"cancelOrder.success"));
}

/// 注释只归属首个声明，数据字典不制造方法树，命名空间常量保持可导航。
#[test]
fn typescript_docs_and_data_objects_do_not_create_false_methods() {
    let parsed = parse(
        "src/service.ts",
        Language::TypeScript,
        "/** 保存订单。 */\nexport const saveOrder = () => { const inner = () => 1; };\n",
    );
    let save = parsed
        .chunks
        .iter()
        .find(|c| c.qualified_symbol.as_deref() == Some("saveOrder"))
        .unwrap();
    assert!(save.doc_comment.as_deref().unwrap().contains("保存订单"));
    let inner = parsed
        .chunks
        .iter()
        .find(|c| c.qualified_symbol.as_deref() == Some("saveOrder.inner"))
        .unwrap();
    assert!(inner.doc_comment.is_none());
    let data = parse(
        "src/locale.js",
        Language::JavaScript,
        "export default { page: { title: '标题' } };",
    );
    assert_eq!(data.tier, ParseTier::Text);
    assert!(data.symbols.is_empty());
    let namespace = parse(
        "src/scope.ts",
        Language::TypeScript,
        "namespace Config { export const value = 42; }",
    );
    assert!(qualified(&namespace).contains(&"Config.value"));
}

/// 没有可识别声明的 SFC 必须完整降级，不能变成空索引。
#[test]
fn vue_without_script_keeps_text_fallback() {
    let parsed = parse(
        "src/Static.vue",
        Language::Vue,
        "<template>页面文本</template>\n<style>.title { color: red }</style>",
    );
    assert_eq!(parsed.tier, ParseTier::Text);
    assert!(!parsed.chunks.is_empty());
}

// Python
// ---------------------------------------------------------------------------

/// Python 的类、方法与函数必须被识别并正确限定。
#[test]
fn python_extracts_classes_and_methods() {
    let parsed = parse(
        "app/service.py",
        Language::Python,
        r#"
class Service:
    """服务入口。"""

    def run(self, value):
        """执行一次。"""
        return value

def helper():
    pass
"#,
    );

    assert_eq!(parsed.tier, ParseTier::Structural);
    assert_eq!(find(&parsed, "Service").kind, SymbolKind::Struct);
    assert_eq!(find(&parsed, "Service.run").kind, SymbolKind::Function);
    assert_eq!(find(&parsed, "helper").kind, SymbolKind::Function);
}

/// Python 的文档字符串必须作为文档注释提取。
#[test]
fn python_extracts_docstrings() {
    let parsed = parse(
        "a.py",
        Language::Python,
        "def run():\n    \"\"\"执行一次任务。\"\"\"\n    pass\n",
    );
    let chunk = &parsed.chunks[0];
    assert_eq!(chunk.doc_comment.as_deref(), Some("执行一次任务。"));
}

/// async def 必须与 def 同等处理。
#[test]
fn python_handles_async_functions() {
    let parsed = parse("a.py", Language::Python, "async def fetch():\n    pass\n");
    assert_eq!(find(&parsed, "fetch").kind, SymbolKind::Function);
}

/// 只有全大写的模块级赋值才作为常量收录。
#[test]
fn python_only_uppercase_module_assignments_are_constants() {
    let parsed = parse(
        "a.py",
        Language::Python,
        "MAX_RETRY = 3\ncounter = 0\ndef a():\n    pass\n",
    );
    assert!(qualified(&parsed).contains(&"MAX_RETRY"));
    assert!(
        !qualified(&parsed).contains(&"counter"),
        "小写模块级变量不应进入符号表"
    );
}

/// Python 的 import 必须被收集。
#[test]
fn python_collects_imports() {
    let parsed = parse(
        "a.py",
        Language::Python,
        "import os\nfrom typing import List\ndef a():\n    pass\n",
    );
    let imports = &parsed.chunks[0].imports;
    assert!(imports.iter().any(|item| item == "os"));
    assert!(imports.iter().any(|item| item.contains("List")));
}

// ---------------------------------------------------------------------------
// Go
// ---------------------------------------------------------------------------

/// Go 的函数、结构体与接口必须被识别。
#[test]
fn go_extracts_declarations() {
    let parsed = parse(
        "main.go",
        Language::Go,
        r#"
package main

type Repo struct {
	name string
}

type Store interface {
	Get(id string) string
}

const Limit = 10

func Run() {}
"#,
    );

    assert_eq!(parsed.tier, ParseTier::Structural);
    assert_eq!(find(&parsed, "Repo").kind, SymbolKind::Struct);
    assert_eq!(find(&parsed, "Store").kind, SymbolKind::Trait);
    assert_eq!(find(&parsed, "Limit").kind, SymbolKind::Constant);
    assert_eq!(find(&parsed, "Run").kind, SymbolKind::Function);
}

/// Go 方法必须以接收者类型限定。
#[test]
fn go_methods_are_qualified_by_receiver() {
    let parsed = parse(
        "repo.go",
        Language::Go,
        "package main\n\n// Get 按 ID 读取。\nfunc (r *Repo) Get(id string) string {\n\treturn id\n}\n",
    );

    let get = find(&parsed, "Repo.Get");
    assert_eq!(get.kind, SymbolKind::Function);
    let chunk = parsed
        .chunks
        .iter()
        .find(|chunk| chunk.qualified_symbol.as_deref() == Some("Repo.Get"))
        .expect("应有对应 chunk");
    assert_eq!(chunk.doc_comment.as_deref(), Some("Get 按 ID 读取。"));
}

/// Go 的 import 必须被收集。
#[test]
fn go_collects_imports() {
    let parsed = parse(
        "main.go",
        Language::Go,
        "package main\n\nimport (\n\t\"fmt\"\n\t\"os\"\n)\n\nfunc main() {}\n",
    );
    let imports = &parsed.chunks[0].imports;
    assert!(imports.iter().any(|item| item == "fmt"));
    assert!(imports.iter().any(|item| item == "os"));
}

// ---------------------------------------------------------------------------
// 降级与健壮性
// ---------------------------------------------------------------------------

/// 语法错误的文件必须降级为文本分块，而不是让解析失败向上传播。
#[test]
fn syntax_errors_fall_back_to_text() {
    let parsed = parse(
        "src/broken.rs",
        Language::Rust,
        "fn broken( { this is not rust at all ]]]\n\nmore garbage\n",
    );
    assert_eq!(parsed.tier, ParseTier::Text);
    assert!(!parsed.chunks.is_empty(), "降级后仍必须产出可检索内容");
    assert!(parsed.symbols.is_empty());
}

/// 没有解析器的语言必须走文本分块。
#[test]
fn unsupported_languages_fall_back_to_text() {
    let parsed = parse(
        "README.md",
        Language::Markdown,
        "# 标题\n\n正文段落。\n\n## 小节\n\n更多内容。\n",
    );
    assert_eq!(parsed.tier, ParseTier::Text);
    assert_eq!(parsed.chunks.len(), 1);
    assert!(parsed.chunks[0].symbol_name.is_none());
    assert_eq!(parsed.chunks[0].language, "markdown");
}

/// 不含任何声明的合法文件必须降级为文本分块，而不是产出空结果。
#[test]
fn files_without_declarations_fall_back_to_text() {
    let parsed = parse("a.py", Language::Python, "print('hello')\nprint('world')\n");
    assert_eq!(parsed.tier, ParseTier::Text);
    assert!(!parsed.chunks.is_empty());
}

/// 空文件不得 panic，也不得产出 chunk。
#[test]
fn empty_file_produces_nothing() {
    let parsed = parse("a.rs", Language::Rust, "");
    assert!(parsed.chunks.is_empty());
    assert!(parsed.symbols.is_empty());
}

/// 只有空白的文件不得产出 chunk。
#[test]
fn whitespace_only_file_produces_nothing() {
    let parsed = parse("a.md", Language::Markdown, "\n\n   \n\n");
    assert!(parsed.chunks.is_empty());
}

/// 超长文本必须被切成多个块，且行区间连续不重叠。
#[test]
fn long_text_is_split_into_contiguous_chunks() {
    let source: String = (1..=200)
        .map(|index| format!("第 {index} 行内容\n"))
        .collect();
    let parsed = parse("notes.md", Language::Markdown, &source);

    assert!(parsed.chunks.len() > 1, "200 行应被切成多块");
    for pair in parsed.chunks.windows(2) {
        assert_eq!(
            pair[1].start_line,
            pair[0].end_line + 1,
            "文本块的行区间必须连续且不重叠"
        );
    }
    assert_eq!(parsed.chunks.last().expect("末块").end_line, 200);
}

// ---------------------------------------------------------------------------
// chunk 装配
// ---------------------------------------------------------------------------

/// chunk 身份与内容哈希都不得随行号平移变化。
///
/// 这是增量索引的核心前提：在文件开头插入几行注释后，未改动的函数必须既保持同一 ID，
/// 内容哈希也保持不变——否则每次编辑都会让整个文件的 chunk 全部判定为已变更，
/// 增量索引退化成全量重建（Phase 11 直接依赖这条性质）。
#[test]
fn chunk_identity_is_stable_across_line_shifts() {
    let first = parse("src/a.rs", Language::Rust, "pub fn target() {}\n");
    let shifted = parse(
        "src/a.rs",
        Language::Rust,
        "// 新增的注释\n// 又一行\npub fn target() {}\n",
    );

    assert_eq!(
        first.chunks[0].id, shifted.chunks[0].id,
        "ID 不得随行号变化"
    );
    assert_eq!(
        first.chunks[0].content_hash, shifted.chunks[0].content_hash,
        "函数体没变，内容哈希就不得变"
    );
    // 位置本身仍必须如实更新，否则检索结果会指向错误的行。
    assert_eq!(first.chunks[0].start_line, 1);
    assert_eq!(shifted.chunks[0].start_line, 3);
}

/// 内容真正改变时，内容哈希必须随之改变。
#[test]
fn content_hash_tracks_body_changes() {
    let before = parse("src/a.rs", Language::Rust, "pub fn target() { 1 }\n");
    let after = parse("src/a.rs", Language::Rust, "pub fn target() { 2 }\n");

    assert_eq!(
        before.chunks[0].id, after.chunks[0].id,
        "改函数体不改变身份"
    );
    assert_ne!(
        before.chunks[0].content_hash, after.chunks[0].content_hash,
        "函数体变了，内容哈希必须变"
    );
}

/// 不同文件的同名符号必须得到不同 chunk ID。
#[test]
fn chunk_id_differs_across_files() {
    let first = parse("src/a.rs", Language::Rust, "pub fn same() {}\n");
    let second = parse("src/b.rs", Language::Rust, "pub fn same() {}\n");
    assert_ne!(first.chunks[0].id, second.chunks[0].id);
}

/// 同一次解析必须完全确定。
#[test]
fn parsing_is_deterministic() {
    let source = "pub struct A;\nimpl A { pub fn run(&self) {} }\n";
    let first = parse("src/a.rs", Language::Rust, source);
    let second = parse("src/a.rs", Language::Rust, source);

    assert_eq!(qualified(&first), qualified(&second));
    let first_ids: Vec<&str> = first.chunks.iter().map(|chunk| chunk.id.as_str()).collect();
    let second_ids: Vec<&str> = second
        .chunks
        .iter()
        .map(|chunk| chunk.id.as_str())
        .collect();
    assert_eq!(first_ids, second_ids);
}

/// chunk 必须带上语言与签名，供检索结果展示。
#[test]
fn chunks_carry_language_and_signature() {
    let parsed = parse(
        "src/a.rs",
        Language::Rust,
        "pub fn compute(input: u32) -> u32 {\n    input\n}\n",
    );
    let chunk = &parsed.chunks[0];
    assert_eq!(chunk.language, "rust");
    assert_eq!(
        chunk.signature.as_deref(),
        Some("pub fn compute(input: u32) -> u32")
    );
    assert_eq!(chunk.file_path, "src/a.rs");
}

/// 行索引必须正确映射字节偏移。
#[test]
fn line_index_maps_offsets() {
    let index = LineIndex::new("a\nbb\nccc\n");
    assert_eq!(index.line_of(0), 1);
    assert_eq!(index.line_of(2), 2);
    assert_eq!(index.line_of(5), 3);
}

/// 含多字节字符的源码不得让行号错位或 panic。
#[test]
fn multibyte_source_maps_lines_correctly() {
    let parsed = parse(
        "src/a.rs",
        Language::Rust,
        "/// 中文文档注释，包含多字节字符。\npub fn 处理() {}\npub fn after() {}\n",
    );
    assert_eq!(find(&parsed, "after").start_line, 3);
}

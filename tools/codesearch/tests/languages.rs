//! 多语言解析验收：验证真实语法、限定名、源码坐标和损坏输入，不把文件识别当成完整支持。

use codesearch::parser::{parse, Definition};
use codesearch::types::Language;
use codesearch::workspace::scan::detect_language;

/// 递归核对每个定义的字符边界与原始行号，同时收集限定名供断言使用。
fn inspect(source: &str, definitions: &[Definition], names: &mut Vec<String>) {
    for definition in definitions {
        let text = source
            .get(definition.start_byte..definition.end_byte)
            .expect("合法字符范围");
        assert!(!text.is_empty());
        assert_eq!(
            definition.start_line,
            source[..definition.start_byte].matches('\n').count() as u32 + 1
        );
        assert_eq!(
            definition.end_line,
            source[..definition.end_byte].matches('\n').count() as u32 + 1
        );
        names.push(definition.qualified.clone());
        inspect(source, &definition.children, names);
    }
}

/// 每套新增语法至少覆盖顶层定义和嵌套或相关声明，避免空解析也通过测试。
#[test]
fn new_languages_extract_real_symbols_and_source_positions() {
    let cases: &[(&str, Language, &str, &[&str])] = &[
        (
            "cart.dart",
            Language::Dart,
            include_str!("fixtures/multilingual/mobile/cart.dart"),
            &[
                "CartTotal",
                "CartTotal.payable",
                "CartTotal.build",
                "rewardPoints",
            ],
        ),
        (
            "session.ex",
            Language::Elixir,
            include_str!("fixtures/multilingual/server/session.ex"),
            &[
                "Portal.Session",
                "Portal.Session.valid?",
                "Portal.Session.revoke",
            ],
        ),
        (
            "frame.zig",
            Language::Zig,
            include_str!("fixtures/multilingual/native/frame.zig"),
            &["checksum", "ReceiveBuffer", "ReceiveBuffer.reset"],
        ),
        (
            "pricing.hs",
            Language::Haskell,
            include_str!("fixtures/multilingual/server/pricing.hs"),
            &["shippingCost", "Membership", "Amount"],
        ),
        (
            "storage.tf",
            Language::Hcl,
            include_str!("fixtures/multilingual/infra/storage.tf"),
            &[
                "resource.aws_s3_bucket_versioning.archive",
                "resource.aws_cloudwatch_log_group.application",
                "resource.aws_cloudwatch_log_group.application.retention_in_days",
            ],
        ),
        (
            "orders.graphql",
            Language::Graphql,
            include_str!("fixtures/multilingual/api/orders.graphql"),
            &["OrderConnection", "OrderConnection.edges", "RecentOrders"],
        ),
        (
            "preview.html",
            Language::Html,
            include_str!("fixtures/multilingual/web/preview.html"),
            &["releasePreview"],
        ),
    ];
    for (path, language, source, expected) in cases {
        assert_eq!(detect_language(path), *language);
        assert_eq!(
            Language::parse_filter(language.as_str()).unwrap(),
            *language
        );
        let parsed = parse(*language, source).expect("解析成功");
        assert!(!parsed.has_errors, "{path} 的有效源码不得产生语法错误");
        let mut names = Vec::new();
        inspect(source, &parsed.definitions, &mut names);
        for expected in *expected {
            assert!(
                names.iter().any(|name| name == expected),
                "{path} 缺少 {expected}: {names:?}"
            );
        }
    }
}

/// 不完整、非 ASCII 和空源码都不能导致崩溃或越界，错误节点之外的文本仍可分块。
#[test]
fn new_grammars_tolerate_broken_and_unicode_input() {
    for language in [
        Language::Dart,
        Language::Elixir,
        Language::Zig,
        Language::Haskell,
        Language::Hcl,
        Language::Graphql,
        Language::Html,
    ] {
        for source in [
            "",
            "中文𠮷",
            "class Broken {\n",
            "defmodule Incomplete do\n",
            "resource \"a\" {\n",
            "type Query { field:\n",
        ] {
            let parsed = parse(language, source).expect("残缺源码仍可解析");
            inspect(source, &parsed.definitions, &mut Vec::new());
        }
    }
}

/// HTML 注释、数据脚本和外部脚本不得贡献虚假函数，属性值不得破坏坐标。
#[test]
fn embedded_scripts_ignore_fake_tags_and_respect_attributes() {
    let source = r#"<!-- <script>function fake() {}</script> -->
<script data-lang="coffee" data-note="x > y" lang="ts">function real(): number { return 1; }</script>
<script type="application/ld+json">function jsonFake() {}</script>
<script src="external.js">function externalFake() {}</script>
<div title="<script>function attributeFake() {}</script>">text</div>
<script lang="coffee">function unsupportedFake() {}</script>"#;
    for language in [Language::Vue, Language::Svelte, Language::Html] {
        let parsed = parse(language, source).unwrap();
        let mut names = Vec::new();
        inspect(source, &parsed.definitions, &mut names);
        assert_eq!(names, ["real"]);
    }
}

/// 只识别声明宏，函数守卫和函数体中的调用不冒充 Elixir 声明。
#[test]
fn elixir_guards_one_line_functions_and_macros() {
    let source = "defmodule Service do\n  defp check(x) when is_integer(x), do: x > 0\n  defmacro wrap(x), do: x\n  log(:event)\nend\n";
    let parsed = parse(Language::Elixir, source).unwrap();
    assert!(!parsed.has_errors);
    let mut names = Vec::new();
    inspect(source, &parsed.definitions, &mut names);
    assert_eq!(names, ["Service", "Service.check", "Service.wrap"]);
}

/// 同名 Go 方法必须按接收者区分，泛型与指针不会进入不稳定的符号名称。
#[test]
fn go_methods_keep_receiver_identity_without_fake_containment() {
    let source = "package app\nfunc (a *Cache[T]) Get() T { return a.value }\nfunc (b Store) Get() int { return 1 }\n";
    let parsed = parse(Language::Go, source).unwrap();
    assert!(!parsed.has_errors);
    let mut names = Vec::new();
    inspect(source, &parsed.definitions, &mut names);
    assert_eq!(names, ["Cache.Get", "Store.Get"]);
    assert!(parsed
        .definitions
        .iter()
        .all(|definition| definition.parent.is_none()));
}

/// Haskell 同名类型签名必须与实现一起返回，不能把只有签名的块排在实现之前。
#[test]
fn haskell_signature_and_documentation_stay_with_implementation() {
    let source = include_str!("fixtures/multilingual/server/pricing.hs");
    let parsed = parse(Language::Haskell, source).unwrap();
    let functions: Vec<_> = parsed
        .definitions
        .iter()
        .filter(|definition| definition.name == "shippingCost")
        .collect();
    assert_eq!(functions.len(), 1);
    let function = functions[0];
    let content = &source[function.start_byte..function.end_byte];
    assert!(content.contains("阶梯运费"));
    assert!(content.contains("shippingCost ::"));
    assert!(content.contains("amount >= 100"));
}

/// Vue 工厂与 Pinia 选项对象的入口方法要能按符号定位，普通业务函数不递归猜测。
#[test]
fn framework_options_preserve_methods_without_expanding_arbitrary_calls() {
    let source = "export default defineComponent({ methods: { submit() { return 1; } } });\nexport const useCart = defineStore('cart', { actions: { clear() { this.items = []; } } });\nconst ordinary = send({ secret() {} });\n";
    let parsed = parse(Language::TypeScript, source).unwrap();
    let mut names = Vec::new();
    inspect(source, &parsed.definitions, &mut names);
    assert!(names.contains(&"submit".to_string()), "{names:?}");
    assert!(names.contains(&"useCart.clear".to_string()), "{names:?}");
    assert!(!names.iter().any(|name| name.contains("secret")));
}

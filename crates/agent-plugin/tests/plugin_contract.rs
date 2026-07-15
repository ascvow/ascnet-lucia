//! Guest SDK 的 WIT 与 UI JSON 契约回归测试。

use agent_plugin::{
    UiDeclaration, UiFrame, UiHostActionRequest, UiInput, UiNavigationRequest, UiRenderRequest,
    UiSessionsReply, PLUGIN_WIT,
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

const CANONICAL_WIT: &str = include_str!("../../../wit/plugin.wit");
const UI_FIXTURE: &str = include_str!("../../../wit/fixtures/ui-v1.json");
const GUEST_SOURCE: &str = include_str!("../src/lib.rs");
const HOST_WASM_SOURCE: &str = include_str!("../../agent-plugin-host/src/wasm/mod.rs");
const WIT_PACKAGE: &str = "package ascnet:lucia-plugin@0.7.0;";

/// 读取单一 UI 契约样本。
fn ui_fixture() -> Value {
    serde_json::from_str(UI_FIXTURE).expect("UI 契约样本必须是有效 JSON")
}

/// 验证指定 Guest 类型与契约样本可以无损双向转换。
fn assert_round_trip<T>(value: &Value)
where
    T: DeserializeOwned + Serialize,
{
    let typed: T = serde_json::from_value(value.clone()).expect("Guest 必须接受 UI 契约样本");
    let encoded = serde_json::to_value(typed).expect("Guest UI 类型必须可序列化");
    assert_eq!(&encoded, value);
}

/// 提取 WIT package、import 和 export，忽略注释及排版差异。
fn wit_surface(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("package ")
                || line.starts_with("import ")
                || line.starts_with("export ")
        })
        .map(|line| line.split_whitespace().collect::<String>())
        .collect()
}

/// 从 Guest 源码中提取常量与导出宏持有的两份内嵌 WIT。
fn embedded_wit_surfaces(source: &str) -> Vec<Vec<String>> {
    let mut remaining = source;
    let mut surfaces = Vec::new();
    while let Some(start) = remaining.find(WIT_PACKAGE) {
        let candidate = &remaining[start..];
        let end = candidate
            .find("\n}")
            .map(|index| index + 2)
            .expect("内嵌 WIT world 必须闭合");
        surfaces.push(wit_surface(&candidate[..end]));
        remaining = &candidate[end..];
    }
    surfaces
}

/// 提取规范 WIT 中的全部函数名。
fn wit_function_names(source: &str) -> Vec<String> {
    source
        .lines()
        .map(str::trim)
        .filter_map(|line| {
            line.strip_prefix("import ")
                .or_else(|| line.strip_prefix("export "))
        })
        .filter_map(|line| line.split(':').next())
        .map(str::to_string)
        .collect()
}

/// Guest 的全部 UI 类型必须共享同一份 JSON 结构。
#[test]
fn guest_ui_types_match_contract_fixture() {
    let fixture = ui_fixture();

    assert_round_trip::<UiDeclaration>(&fixture["declaration"]);
    assert_round_trip::<UiDeclaration>(&fixture["input_declaration"]);
    assert_round_trip::<UiDeclaration>(&fixture["trigger_declaration"]);
    assert_round_trip::<UiRenderRequest>(&fixture["render_request"]);
    assert_round_trip::<UiFrame>(&fixture["frame"]);
    assert_round_trip::<UiInput>(&fixture["input"]);
    assert_round_trip::<UiInput>(&fixture["main_input"]);
    assert_round_trip::<UiNavigationRequest>(&fixture["navigation"]);
    assert_round_trip::<UiHostActionRequest>(&fixture["host_action_set_input"]);
    assert_round_trip::<UiHostActionRequest>(&fixture["host_action_reload_context"]);
    assert_round_trip::<UiHostActionRequest>(&fixture["host_action_resume_session"]);
    assert_round_trip::<UiHostActionRequest>(&fixture["host_action_query_sessions"]);
    assert_round_trip::<UiSessionsReply>(&fixture["sessions_reply"]);
}

/// 规范 WIT、Guest 常量和导出宏副本必须保持同一函数表面。
#[test]
fn embedded_wit_copies_match_canonical_surface() {
    let canonical = wit_surface(CANONICAL_WIT);
    assert_eq!(wit_surface(PLUGIN_WIT), canonical);

    let embedded = embedded_wit_surfaces(GUEST_SOURCE);
    assert_eq!(embedded.len(), 2, "Guest 源码必须只维护两份内嵌 WIT");
    for surface in embedded {
        assert_eq!(surface, canonical);
    }
}

/// Host 绑定源码必须覆盖规范 WIT 中声明的每个 import 与 export 名称。
#[test]
fn host_bindings_cover_canonical_wit_names() {
    for name in wit_function_names(CANONICAL_WIT) {
        assert!(
            HOST_WASM_SOURCE.contains(&format!("\"{name}\"")),
            "Host 绑定缺少 WIT 函数 `{name}`"
        );
    }
}

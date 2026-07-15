//! Plugin Host 的 UI JSON 与 ABI 版本契约测试。

use agent_plugin_host::{
    manifest::SUPPORTED_PLUGIN_API_VERSION,
    ui::{
        ToolRendererContribution, UiDeclaration, UiFrame, UiHostActionRequest, UiInput,
        UiNavigationRequest, UiRenderRequest, UiSessionsReply,
    },
};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

const CANONICAL_WIT: &str = include_str!("../../../wit/plugin.wit");
const UI_FIXTURE: &str = include_str!("../../../wit/fixtures/ui-v1.json");

/// 读取单一 UI 契约样本。
fn ui_fixture() -> Value {
    serde_json::from_str(UI_FIXTURE).expect("UI 契约样本必须是有效 JSON")
}

/// 验证指定 Host 类型与契约样本可以无损双向转换。
fn assert_round_trip<T>(value: &Value)
where
    T: DeserializeOwned + Serialize,
{
    let typed: T = serde_json::from_value(value.clone()).expect("Host 必须接受 UI 契约样本");
    let encoded = serde_json::to_value(typed).expect("Host UI 类型必须可序列化");
    assert_eq!(&encoded, value);
}

/// Host 的全部 UI 类型必须共享同一份 JSON 结构。
#[test]
fn host_ui_types_match_contract_fixture() {
    let fixture = ui_fixture();

    assert_round_trip::<UiDeclaration>(&fixture["declaration"]);
    assert_round_trip::<UiDeclaration>(&fixture["input_declaration"]);
    assert_round_trip::<UiDeclaration>(&fixture["trigger_declaration"]);
    assert_round_trip::<ToolRendererContribution>(&fixture["tool_renderer"]);
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

/// Host 声明的当前 ABI 版本必须与规范 WIT package 版本一致。
#[test]
fn host_api_version_matches_canonical_wit_package() {
    let expected = format!("package ascnet:lucia-plugin@{SUPPORTED_PLUGIN_API_VERSION};");
    assert_eq!(CANONICAL_WIT.lines().next(), Some(expected.as_str()));
}

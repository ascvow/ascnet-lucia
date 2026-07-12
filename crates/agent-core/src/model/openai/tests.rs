//! OpenAI 协议转换、参数过滤和流式聚合回归测试。

use super::stream::{
    handle_chat_completions_sse_data, handle_responses_sse_data, ChatCompletionsStreamState,
    ResponsesStreamState,
};
use super::*;
use crate::model::ModelStreamEvent;

/// 构造带文本、图片和 PDF 的用户消息，覆盖附件转换路径。
fn attachment_message() -> super::super::ModelMessage {
    super::super::ModelMessage {
        role: MessageRole::User,
        content: vec![
            ContentBlock::Text {
                text: "看看 [Image#1] 和 [FILE#报告.pdf]".to_string(),
            },
            ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "aGVsbG8=".to_string(),
            },
            ContentBlock::File {
                name: "报告.pdf".to_string(),
                media_type: "application/pdf".to_string(),
                data: "cGRm".to_string(),
            },
        ],
    }
}

/// Responses 输入中的用户附件映射为 input_image 与 input_file 部件。
#[test]
fn responses_user_attachments_map_to_parts() {
    let input = messages_to_responses_input(&[attachment_message()]).expect("转换应成功");
    let parts = input[0]["content"].as_array().expect("content 应为数组");

    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0]["type"], "input_text");
    assert_eq!(parts[1]["type"], "input_image");
    assert_eq!(parts[1]["image_url"], "data:image/png;base64,aGVsbG8=");
    assert_eq!(parts[2]["type"], "input_file");
    assert_eq!(parts[2]["filename"], "报告.pdf");
}

/// Chat Completions 中带附件的用户消息使用 content 部件数组。
#[test]
fn chat_user_attachments_map_to_parts() {
    let out = message_to_openai_chat_messages(&attachment_message()).expect("转换应成功");
    let parts = out[0]["content"].as_array().expect("content 应为数组");

    assert_eq!(parts.len(), 3);
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(
        parts[1]["image_url"]["url"],
        "data:image/png;base64,aGVsbG8="
    );
    assert_eq!(parts[2]["type"], "file");
    assert_eq!(parts[2]["file"]["filename"], "报告.pdf");
}

/// 纯文本用户消息在 Chat Completions 中保持字符串 content 形态。
#[test]
fn chat_text_only_user_message_keeps_string_content() {
    let message = super::super::ModelMessage::text(MessageRole::User, "你好");
    let out = message_to_openai_chat_messages(&message).expect("转换应成功");

    assert_eq!(out[0]["content"], "你好");
}

/// 裸域名形式的 OpenAI 兼容地址会自动补 `/v1`。
#[test]
fn normalize_openai_base_url_adds_v1_to_origin() {
    let base_url = normalize_openai_base_url(Some("https://api.phrolova.uno".to_string()))
        .expect("base URL should be valid");

    assert_eq!(base_url, "https://api.phrolova.uno/v1");
}

/// 已带 `/v1` 的地址不会重复追加版本前缀。
#[test]
fn normalize_openai_base_url_keeps_existing_v1() {
    let base_url = normalize_openai_base_url(Some("http://localhost:11434/v1/".to_string()))
        .expect("base URL should be valid");

    assert_eq!(base_url, "http://localhost:11434/v1");
}

/// 网关前缀路径会保留，并在其后追加 `/v1`。
#[test]
fn normalize_openai_base_url_adds_v1_after_gateway_prefix() {
    let base_url = normalize_openai_base_url(Some("https://example.com/openai".to_string()))
        .expect("base URL should be valid");

    assert_eq!(base_url, "https://example.com/openai/v1");
}

/// endpoint 拼接使用规范化后的 base URL，避免遗漏版本路径。
#[test]
fn endpoint_url_uses_normalized_base_url() {
    let base_url = normalize_openai_base_url(Some("https://api.phrolova.uno".to_string()))
        .expect("base URL should be valid");

    assert_eq!(
        endpoint_url(&base_url, "/responses"),
        "https://api.phrolova.uno/v1/responses"
    );
}

/// 官方 OpenAI 地址选择完整参数白名单。
#[test]
fn allowed_params_official_openai_is_full() {
    let params = responses_allowed_params(&ProviderKind::OpenAi, "https://api.openai.com/v1");
    assert!(params.contains(&"max_output_tokens"));
    assert!(params.contains(&"store"));
}

/// 非官方 base URL 选择兼容参数白名单，不含 max_output_tokens。
#[test]
fn allowed_params_proxy_uses_compatible() {
    let params = responses_allowed_params(&ProviderKind::OpenAi, "https://api.phrolova.uno/v1");
    assert!(!params.contains(&"max_output_tokens"));
    assert!(!params.contains(&"store"));
}

/// OpenAiCompatible 类型始终使用兼容参数白名单。
#[test]
fn allowed_params_compatible_kind_uses_compatible() {
    let params =
        responses_allowed_params(&ProviderKind::OpenAiCompatible, "https://localhost:8080/v1");
    assert!(!params.contains(&"max_output_tokens"));
}

/// filter_params 移除不在白名单中的键。
#[test]
fn filter_params_drops_unsupported_keys() {
    let mut body = json!({
        "model": "gpt-4",
        "input": [],
        "max_output_tokens": 32,
        "store": true,
        "temperature": 0.7,
    });
    filter_params(&mut body, RESPONSES_PARAMS_COMPATIBLE);

    assert!(body.get("model").is_some());
    assert!(body.get("input").is_some());
    assert!(body.get("temperature").is_some());
    assert!(body.get("max_output_tokens").is_none());
    assert!(body.get("store").is_none());
}

/// filter_params 对完整白名单不会丢弃标准参数。
#[test]
fn filter_params_keeps_all_with_full_list() {
    let mut body = json!({
        "model": "gpt-4",
        "input": [],
        "max_output_tokens": 32,
        "store": true,
    });
    filter_params(&mut body, RESPONSES_PARAMS_FULL);

    assert!(body.get("max_output_tokens").is_some());
    assert!(body.get("store").is_some());
}

/// Responses SSE 文本增量会实时发送并进入最终聚合文本。
#[tokio::test]
async fn responses_sse_emits_and_aggregates_text_delta() {
    let (sender, mut stream) = ModelEventStream::channel();
    let mut state = ResponsesStreamState::default();

    handle_responses_sse_data(
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"你好"}"#,
        &sender,
        &mut state,
    )
    .expect("应该解析文本增量");

    assert_eq!(state.text, "你好");
    match stream.next().await.expect("应该收到流事件") {
        ModelStreamEvent::TextDelta { index, delta } => {
            assert_eq!(index, 0);
            assert_eq!(delta, "你好");
        }
        event => panic!("收到非预期事件: {event:?}"),
    }
}

/// Chat Completions SSE 会按索引拼接工具名称和 JSON 参数片段。
#[test]
fn chat_completions_sse_aggregates_tool_call_fragments() {
    let (sender, _stream) = ModelEventStream::channel();
    let mut state = ChatCompletionsStreamState::default();
    let first = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"echo","arguments":"{\"text\":"}}]}}]}"#;
    let second = r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"hi\"}"}}]},"finish_reason":"tool_calls"}]}"#;

    handle_chat_completions_sse_data(first, &sender, &mut state).expect("应该解析首个工具片段");
    handle_chat_completions_sse_data(second, &sender, &mut state).expect("应该解析第二个工具片段");

    assert_eq!(state.tools.len(), 1);
    assert_eq!(state.tools[0].id, "call_1");
    assert_eq!(state.tools[0].name, "echo");
    assert_eq!(state.tools[0].arguments, r#"{"text":"hi"}"#);
    assert_eq!(state.finish_reason.as_deref(), Some("tool_calls"));
}

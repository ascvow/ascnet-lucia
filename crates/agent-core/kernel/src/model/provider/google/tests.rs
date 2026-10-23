//! Google Generative AI 适配器的协议映射测试。

use super::*;
use crate::model::{ContentBlock, MessageRole, ModelMessage, ModelRequest};
use agent_tool::{ToolPermissionScope, ToolSpec};
use serde_json::json;

/// 文本与系统提示必须映射为 systemInstruction 与 user contents。
#[test]
fn builds_request_with_system_and_text() {
    let mut req = ModelRequest::new(
        "gemini-2.5-pro",
        vec![
            ModelMessage::text(MessageRole::User, "你好"),
            ModelMessage::text(MessageRole::Assistant, "你好！"),
        ],
    );
    req.system = Some("回答保持简短".to_string());

    let (model, body) = build_generate_request(req).expect("请求体应可构建");
    assert_eq!(model, "gemini-2.5-pro");
    assert_eq!(
        body["systemInstruction"]["parts"][0]["text"],
        "回答保持简短"
    );
    assert_eq!(body["contents"][0]["role"], "user");
    assert_eq!(body["contents"][0]["parts"][0]["text"], "你好");
    assert_eq!(body["contents"][1]["role"], "model");
    assert_eq!(body["contents"][1]["parts"][0]["text"], "你好！");
    assert!(body.get("tools").is_none());
}

/// 工具定义、强制选择与 maxOutputTokens 必须落在对应协议字段。
#[test]
fn builds_request_with_tools_and_generation_config() {
    let mut req = ModelRequest::new(
        "gemini-2.5-flash",
        vec![ModelMessage::text(MessageRole::User, "查天气")],
    );
    req.tools = vec![ToolSpec::new(
        "get_weather",
        "查询天气",
        json!({ "type": "object", "properties": {} }),
    )];
    req.tool_choice = ToolChoice::Tool {
        name: "get_weather".to_string(),
    };
    req.max_tokens = Some(2_048);

    let (_, body) = build_generate_request(req).expect("请求体应可构建");
    assert_eq!(
        body["tools"][0]["functionDeclarations"][0]["name"],
        "get_weather"
    );
    assert_eq!(body["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
    assert_eq!(
        body["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
        "get_weather"
    );
    assert_eq!(body["generationConfig"]["maxOutputTokens"], 2_048);
}

/// 工具结果必须映射为 user 角色的 functionResponse，失败结果带 error 标记。
#[test]
fn maps_tool_results_into_function_response() {
    use agent_tool::ToolResult;

    let result = ToolResult::success("call-1", "get_weather", json!("多云 25 度"));
    let message = ModelMessage::tool_result(result);
    let (model, body) =
        build_generate_request(ModelRequest::new("gemini", vec![message])).expect("请求体应可构建");
    assert_eq!(model, "gemini");
    assert_eq!(body["contents"][0]["role"], "user");
    assert_eq!(
        body["contents"][0]["parts"][0]["functionResponse"]["name"],
        "get_weather"
    );
    assert_eq!(
        body["contents"][0]["parts"][0]["functionResponse"]["response"]["result"],
        "多云 25 度"
    );

    let mut failed = ToolResult::success("call-2", "get_weather", json!("boom"));
    failed.is_error = true;
    let (_, body) = build_generate_request(ModelRequest::new(
        "gemini",
        vec![ModelMessage::tool_result(failed)],
    ))
    .expect("请求体应可构建");
    assert_eq!(
        body["contents"][0]["parts"][0]["functionResponse"]["response"]["error"],
        json!(true)
    );
}

/// 推理等级必须映射为 thinkingConfig；关闭时不发送该字段。
#[test]
fn maps_reasoning_levels() {
    let mut req = ModelRequest::new("gemini", Vec::new());
    req.reasoning = ReasoningLevel::High;
    let (_, body) = build_generate_request(req).expect("请求体应可构建");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        16_384
    );

    let req = ModelRequest::new("gemini", Vec::new());
    let (_, body) = build_generate_request(req).expect("请求体应可构建");
    assert!(body.get("generationConfig").is_none());
}

/// 图片附件必须映射为 inlineData；非 PDF 文件降级为文本占位。
#[test]
fn maps_attachments() {
    let message = ModelMessage {
        role: MessageRole::User,
        content: vec![
            ContentBlock::Image {
                media_type: "image/png".to_string(),
                data: "aGk=".to_string(),
            },
            ContentBlock::File {
                name: "notes.txt".to_string(),
                media_type: "text/plain".to_string(),
                data: "aGk=".to_string(),
            },
        ],
    };
    let (_, body) =
        build_generate_request(ModelRequest::new("gemini", vec![message])).expect("请求体应可构建");
    assert_eq!(
        body["contents"][0]["parts"][0]["inlineData"]["mimeType"],
        "image/png"
    );
    assert!(body["contents"][0]["parts"][1]["text"]
        .as_str()
        .unwrap_or_default()
        .contains("notes.txt"));
}

/// 完整响应必须解析出文本、函数调用与用量。
#[test]
fn parses_generate_response_with_function_call() {
    let value = json!({
        "candidates": [{
            "content": { "parts": [
                { "text": "让我查一下" },
                { "functionCall": { "name": "get_weather", "args": { "city": "杭州" } } }
            ] },
            "finishReason": "STOP"
        }],
        "usageMetadata": {
            "promptTokenCount": 10,
            "candidatesTokenCount": 20,
            "totalTokenCount": 30
        }
    });

    let response = parse_generate_response(value).expect("响应应可解析");
    assert_eq!(response.text_content(), "让我查一下");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "get_weather");
    let usage = response.usage.expect("用量应存在");
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(20));
    assert_eq!(usage.total_tokens, Some(30));
}

/// 安全拦截必须映射为 Refusal，未知原因映射为 Unknown。
#[test]
fn maps_finish_reasons() {
    assert_eq!(
        parse_finish_reason(&json!({ "candidates": [{ "finishReason": "SAFETY" }] })),
        Some(FinishReason::Refusal)
    );
    assert_eq!(
        parse_finish_reason(&json!({ "candidates": [{ "finishReason": "MAX_TOKENS" }] })),
        Some(FinishReason::Length)
    );
    assert_eq!(
        parse_finish_reason(&json!({ "candidates": [{ "finishReason": "WHAT" }] })),
        Some(FinishReason::Unknown)
    );
    assert_eq!(parse_finish_reason(&json!({})), None);
}

/// 错误响应必须携带服务商错误信息。
#[test]
fn surfaces_error_payload() {
    let value = json!({ "error": { "message": "quota exceeded" } });
    let error = parse_generate_response(value).expect_err("错误响应应返回错误");
    assert!(error.to_string().contains("quota exceeded"));
}

/// Custom 协议 ID 必须在 serde 往返中保持不变，未知 ID 归入 Custom。
#[test]
fn provider_kind_serde_roundtrip() {
    use crate::model::ProviderKind;
    assert_eq!(
        serde_json::from_str::<ProviderKind>("\"open-ai\"").unwrap(),
        ProviderKind::OpenAi
    );
    assert_eq!(
        serde_json::from_str::<ProviderKind>("\"my-protocol\"").unwrap(),
        ProviderKind::Custom("my-protocol".to_string())
    );
    assert_eq!(
        serde_json::to_string(&ProviderKind::Custom("my-protocol".to_string())).unwrap(),
        "\"my-protocol\""
    );
    assert_eq!(ProviderKind::Bedrock.protocol_id(), "bedrock");
}

/// 工具调用响应中的 permission_scope 必须保持 Unknown，不得信任模型声明。
#[test]
fn parsed_tool_calls_keep_unknown_scope() {
    let value = json!({
        "candidates": [{
            "content": { "parts": [
                { "functionCall": { "name": "run", "args": {} } }
            ] }
        }]
    });
    let response = parse_generate_response(value).expect("响应应可解析");
    assert_eq!(
        response.tool_calls[0].permission_scope,
        ToolPermissionScope::Unknown
    );
}

//! Bedrock Converse 适配器的协议映射与 SigV4 测试。

use super::*;
use crate::model::{ModelMessage, ModelRequest, ProviderKind};
use agent_tool::{ToolPermissionScope, ToolResult, ToolSpec};
use serde_json::json;

/// 文本与系统提示必须映射为 system 与 messages。
#[test]
fn builds_request_with_system_and_text() {
    let mut req = ModelRequest::new(
        "anthropic.claude-sonnet-4-5-20250929-v1:0",
        vec![
            ModelMessage::text(MessageRole::User, "你好"),
            ModelMessage::text(MessageRole::Assistant, "你好！"),
        ],
    );
    req.system = Some("回答保持简短".to_string());

    let (model_id, body) = build_converse_request(req).expect("请求体应可构建");
    assert_eq!(model_id, "anthropic.claude-sonnet-4-5-20250929-v1:0");
    assert_eq!(body["system"][0]["text"], "回答保持简短");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"][0]["text"], "你好");
    assert_eq!(body["messages"][1]["role"], "assistant");
    assert_eq!(body["messages"][1]["content"][0]["text"], "你好！");
}

/// 工具定义、toolUse 与 toolResult 必须落在对应协议字段。
#[test]
fn builds_request_with_tools() {
    let mut req = ModelRequest::new(
        "model",
        vec![
            ModelMessage::assistant_tool_calls(vec![agent_tool::ToolCall::new(
                "call-1",
                "get_weather",
                json!({ "city": "杭州" }),
            )]),
            ModelMessage::tool_result(ToolResult::success("call-1", "get_weather", json!("多云"))),
        ],
    );
    req.tools = vec![ToolSpec::new(
        "get_weather",
        "查询天气",
        json!({ "type": "object", "properties": {} }),
    )];
    req.tool_choice = ToolChoice::Required;
    req.max_tokens = Some(1_024);

    let (_, body) = build_converse_request(req).expect("请求体应可构建");
    assert_eq!(
        body["toolConfig"]["tools"][0]["toolSpec"]["name"],
        "get_weather"
    );
    assert_eq!(body["toolConfig"]["toolChoice"]["any"], json!({}));
    assert_eq!(body["inferenceConfig"]["maxTokens"], 1_024);
    assert_eq!(
        body["messages"][0]["content"][0]["toolUse"]["toolUseId"],
        "call-1"
    );
    let tool_result = &body["messages"][1]["content"][0]["toolResult"];
    assert_eq!(tool_result["toolUseId"], "call-1");
    assert_eq!(tool_result["status"], "success");
    assert_eq!(tool_result["content"][0]["text"], "多云");
}

/// 失败的工具结果必须带 error 状态；对象载荷走 json 通道。
#[test]
fn maps_failed_tool_result() {
    let mut failed = ToolResult::success("call-2", "get_weather", json!({ "code": 500 }));
    failed.is_error = true;
    let (_, body) = build_converse_request(ModelRequest::new(
        "model",
        vec![ModelMessage::tool_result(failed)],
    ))
    .expect("请求体应可构建");
    let tool_result = &body["messages"][0]["content"][0]["toolResult"];
    assert_eq!(tool_result["status"], "error");
    assert_eq!(tool_result["content"][0]["json"]["code"], 500);
}

/// 完整响应必须解析出文本、toolUse 与用量。
#[test]
fn parses_converse_response() {
    let value = json!({
        "output": { "message": { "role": "assistant", "content": [
            { "text": "让我查一下" },
            { "toolUse": { "toolUseId": "call-1", "name": "get_weather", "input": { "city": "杭州" } } }
        ] } },
        "stopReason": "tool_use",
        "usage": { "inputTokens": 10, "outputTokens": 20, "totalTokens": 30 }
    });

    let response = parse_converse_response(value).expect("响应应可解析");
    assert_eq!(response.text_content(), "让我查一下");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].id, "call-1");
    assert_eq!(response.tool_calls[0].name, "get_weather");
    let usage = response.usage.expect("用量应存在");
    assert_eq!(usage.input_tokens, Some(10));
    assert_eq!(usage.output_tokens, Some(20));
    assert_eq!(usage.total_tokens, Some(30));
}

/// 停止原因必须按语义映射，缺失时按自然停止处理。
#[test]
fn maps_stop_reasons() {
    let stop = |reason: &str| {
        parse_converse_response(json!({
            "output": { "message": { "role": "assistant", "content": [] } },
            "stopReason": reason
        }))
        .unwrap()
        .finish_reason
    };
    assert_eq!(stop("end_turn"), FinishReason::Stop);
    assert_eq!(stop("max_tokens"), FinishReason::Length);
    assert_eq!(stop("guardrail_intervened"), FinishReason::Refusal);
    assert_eq!(stop("weird"), FinishReason::Unknown);
    let missing = parse_converse_response(json!({
        "output": { "message": { "role": "assistant", "content": [{ "text": "hi" }] } }
    }))
    .unwrap();
    assert_eq!(missing.finish_reason, FinishReason::Stop);
}

/// 解析出的工具调用权限范围必须保持 Unknown，不得信任模型声明。
#[test]
fn parsed_tool_calls_keep_unknown_scope() {
    let response = parse_converse_response(json!({
        "output": { "message": { "role": "assistant", "content": [
            { "toolUse": { "toolUseId": "c", "name": "run", "input": {} } }
        ] } }
    }))
    .unwrap();
    assert_eq!(
        response.tool_calls[0].permission_scope,
        ToolPermissionScope::Unknown
    );
}

/// 模型 ID 中的冒号必须编码，ARN 中的斜杠保持原样。
#[test]
fn encodes_model_id_path() {
    assert_eq!(
        encode_path_segment("anthropic.claude-sonnet-4-5-20250929-v1:0"),
        "anthropic.claude-sonnet-4-5-20250929-v1%3A0"
    );
    assert_eq!(
        encode_path_segment("arn:aws:bedrock:us-east-1::foundation-model/x"),
        "arn%3Aaws%3Abedrock%3Aus-east-1%3A%3Afoundation-model/x"
    );
}

/// SigV4 签名必须包含 authorization 与 x-amz-date，且时间戳格式固定。
#[test]
fn signs_request_with_sigv4() {
    let config = ModelProviderConfig::bedrock("bedrock-main", "us-east-1", "AKIDEXAMPLE", "secret");
    let adapter = BedrockConverseAdapter::new(config).expect("适配器应可创建");
    let (headers, url) = adapter
        .sign_request("/model/test-model/converse", b"{}")
        .expect("签名应成功");
    assert!(url.starts_with("https://bedrock-runtime.us-east-1.amazonaws.com"));
    let authorization = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .expect("应包含 authorization");
    assert!(authorization.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/"));
    assert!(authorization.contains("SignedHeaders=host;x-amz-date"));
    let amz_date = headers
        .get("x-amz-date")
        .and_then(|value| value.to_str().ok())
        .expect("应包含 x-amz-date");
    assert_eq!(amz_date.len(), 16);
    assert!(amz_date.ends_with('Z'));
    assert!(amz_date[8..9].starts_with('T'));
}

/// 缺少 AWS 凭据字段时必须拒绝创建适配器。
#[test]
fn rejects_missing_aws_credentials() {
    let config = ModelProviderConfig::new("bedrock-bad", ProviderKind::Bedrock, "");
    let error = BedrockConverseAdapter::new(config).expect_err("缺少凭据应报错");
    assert!(error.to_string().contains("aws_region"));
}

/// 时间戳转换必须产出正确的 UTC 日期。
#[test]
fn formats_aws_timestamp() {
    assert_eq!(aws_timestamp(0), "19700101T000000Z");
    assert_eq!(aws_timestamp(1_700_000_000), "20231114T221320Z");
}

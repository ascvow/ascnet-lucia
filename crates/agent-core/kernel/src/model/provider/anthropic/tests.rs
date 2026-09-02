//! Anthropic Messages 协议转换测试。

use super::*;

/// 显式输出上限必须同时约束思考预算和 provider_options，不能在适配阶段扩张。
#[test]
fn explicit_max_tokens_bounds_reasoning_and_provider_options() {
    let mut request = ModelRequest::new("claude-test", Vec::new());
    request.max_tokens = Some(4_096);
    request.reasoning = ReasoningLevel::Medium;
    request.provider_options = json!({
        "max_tokens": 99_999,
        "max_output_tokens": 88_888,
        "candidate_count": 3
    });

    let body = build_messages_request(request).expect("受限推理请求应成功构建");

    assert_eq!(body["max_tokens"], 4_096);
    assert_eq!(body["max_output_tokens"], 4_096);
    assert_eq!(body["candidate_count"], 1);
    assert_eq!(body["thinking"]["budget_tokens"], 3_072);
}

/// 零输出预算不得借助 Anthropic reasoning 自动扩容形成未计费请求。
#[test]
fn zero_max_tokens_rejects_reasoning_expansion() {
    let mut request = ModelRequest::new("claude-test", Vec::new());
    request.max_tokens = Some(0);
    request.reasoning = ReasoningLevel::High;

    let error = build_messages_request(request).expect_err("零预算推理请求必须失败");

    assert!(error.to_string().contains("至少需要 2048"));
}

/// 默认短期缓存必须标记 system、最后一个工具和最近用户内容三个稳定前缀断点。
#[test]
fn default_short_cache_marks_anthropic_breakpoints() {
    let mut request = ModelRequest::new(
        "claude-test",
        vec![crate::model::ModelMessage::text(
            MessageRole::User,
            "检查缓存",
        )],
    );
    request.system = Some("系统提示".to_string());
    request.tools = vec![ToolSpec::new(
        "read_file",
        "读取文件",
        ToolSpec::empty_object_schema(),
    )];

    let body = build_messages_request(request).expect("短期缓存请求应成功构建");
    let expected = json!({"type": "ephemeral"});

    assert_eq!(body["system"][0]["cache_control"], expected);
    assert_eq!(body["tools"][0]["cache_control"], expected);
    assert_eq!(body["messages"][0]["content"][0]["cache_control"], expected);
}

/// 关闭显式缓存时必须保持原协议形态，不注入 Anthropic 缓存断点。
#[test]
fn no_cache_retention_omits_anthropic_breakpoints() {
    let mut request = ModelRequest::new(
        "claude-test",
        vec![crate::model::ModelMessage::text(
            MessageRole::User,
            "普通请求",
        )],
    );
    request.system = Some("系统提示".to_string());
    request.cache_retention = PromptCacheRetention::None;
    request.tools = vec![ToolSpec::new(
        "read_file",
        "读取文件",
        ToolSpec::empty_object_schema(),
    )];

    let body = build_messages_request(request).expect("无缓存请求应成功构建");

    assert_eq!(body["system"], "系统提示");
    assert!(body["tools"][0].get("cache_control").is_none());
    assert!(body["messages"][0]["content"][0]
        .get("cache_control")
        .is_none());
}

/// Anthropic 长期缓存必须在全部缓存断点上请求一小时 TTL。
#[test]
fn long_cache_retention_maps_to_anthropic_1h() {
    let mut request = ModelRequest::new(
        "claude-test",
        vec![crate::model::ModelMessage::text(
            MessageRole::User,
            "长期任务",
        )],
    );
    request.system = Some("系统提示".to_string());
    request.cache_retention = PromptCacheRetention::Long;

    let body = build_messages_request(request).expect("长期缓存请求应成功构建");
    assert_eq!(
        body["system"][0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );
    assert_eq!(
        body["messages"][0]["content"][0]["cache_control"],
        json!({"type": "ephemeral", "ttl": "1h"})
    );
}

/// Anthropic 返回的缓存读写用量必须计入分项与总 token 数。
#[test]
fn anthropic_usage_includes_cache_tokens() {
    let response = parse_messages_response(
        json!({
            "content": [{"type": "text", "text": "完成"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 50,
                "output_tokens": 20,
                "cache_read_input_tokens": 700,
                "cache_creation_input_tokens": 200
            }
        }),
        &ModelRoute::new("anthropic-main", "claude-test"),
    )
    .expect("应解析 Anthropic 缓存用量");
    let usage = response.usage.expect("响应应包含用量");

    assert_eq!(usage.input_tokens, Some(50));
    assert_eq!(usage.output_tokens, Some(20));
    assert_eq!(usage.cache_read_tokens, Some(700));
    assert_eq!(usage.cache_write_tokens, Some(200));
    assert_eq!(usage.total_tokens, Some(970));
}

/// 图片附件映射为 Anthropic base64 image 块。
#[test]
fn image_block_maps_to_base64_image() {
    let blocks = vec![ContentBlock::Image {
        media_type: "image/png".to_string(),
        data: "aGVsbG8=".to_string(),
    }];

    let out = blocks_to_anthropic_content(&blocks, AnthropicContentMode::Input, None);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["type"], "image");
    assert_eq!(out[0]["source"]["type"], "base64");
    assert_eq!(out[0]["source"]["media_type"], "image/png");
    assert_eq!(out[0]["source"]["data"], "aGVsbG8=");
}

/// PDF 附件映射为原生 document 块并携带标题。
#[test]
fn pdf_file_maps_to_document() {
    let blocks = vec![ContentBlock::File {
        name: "报告.pdf".to_string(),
        media_type: "application/pdf".to_string(),
        data: "cGRm".to_string(),
    }];

    let out = blocks_to_anthropic_content(&blocks, AnthropicContentMode::Input, None);

    assert_eq!(out[0]["type"], "document");
    assert_eq!(out[0]["title"], "报告.pdf");
    assert_eq!(out[0]["source"]["media_type"], "application/pdf");
}

/// 文本附件降级为内联文本块，保留文件名和内容。
#[test]
fn text_file_is_inlined_as_text() {
    // "你好" 的 UTF-8 base64。
    let blocks = vec![ContentBlock::File {
        name: "note.txt".to_string(),
        media_type: "text/plain".to_string(),
        data: "5L2g5aW9".to_string(),
    }];

    let out = blocks_to_anthropic_content(&blocks, AnthropicContentMode::Input, None);

    assert_eq!(out[0]["type"], "text");
    let text = out[0]["text"].as_str().expect("应为文本");
    assert!(text.contains("note.txt"));
    assert!(text.contains("你好"));
}

/// 二进制附件不会被静默丢弃，而是生成占位说明。
#[test]
fn binary_file_becomes_placeholder_text() {
    let blocks = vec![ContentBlock::File {
        name: "app.bin".to_string(),
        media_type: "application/octet-stream".to_string(),
        data: "AAEC".to_string(),
    }];

    let out = blocks_to_anthropic_content(&blocks, AnthropicContentMode::Input, None);

    assert_eq!(out[0]["type"], "text");
    let text = out[0]["text"].as_str().expect("应为文本");
    assert!(text.contains("app.bin"));
    assert!(text.contains("无法内联发送"));
}

/// 路由指纹只标识规范化端点，不能因 URL 中的用户信息、查询凭据或片段而变化。
#[test]
fn route_scope_excludes_url_credentials_and_query() {
    let clean =
        anthropic_route_scope("https://gateway.example.com/v1/").expect("规范端点应生成路由指纹");
    let credentialed = anthropic_route_scope(
        "https://agent:secret@gateway.example.com/v1/?api_key=secret#client-state",
    )
    .expect("含连接参数的端点应先安全规范化");
    let other_endpoint =
        anthropic_route_scope("https://gateway.example.com/v2").expect("不同端点应生成指纹");

    assert_eq!(clean, credentialed);
    assert_ne!(clean, other_endpoint);
}

/// 同一 Anthropic 路由的工具循环必须按原顺序、原值回传私有推理块。
#[test]
fn same_route_tool_loop_round_trips_private_reasoning_blocks() {
    let provider_content = json!([
        {
            "type": "thinking",
            "thinking": "",
            "signature": "sig-anthropic-private"
        },
        {
            "type": "redacted_thinking",
            "data": "encrypted-anthropic-private"
        },
        {
            "type": "tool_use",
            "id": "toolu_01",
            "name": "read_file",
            "input": {"path": "README.md"}
        }
    ]);
    let source_route = ModelRoute::new("anthropic-main", "claude-test");
    let response = parse_messages_response(
        json!({
            "content": provider_content.clone(),
            "stop_reason": "tool_use"
        }),
        &source_route,
    )
    .expect("应解析 Anthropic 工具调用响应");
    let messages = vec![crate::model::ModelMessage {
        role: MessageRole::Assistant,
        content: response.content,
    }];

    let transformed = crate::model::transform::transform_messages_for_route(
        &messages,
        "anthropic-main",
        "claude-test",
    );
    let serialized = blocks_to_anthropic_content(
        &transformed[0].content,
        AnthropicContentMode::Assistant,
        Some(&source_route),
    );

    assert_eq!(Value::Array(serialized), provider_content);
}

/// 缺少来源路由的历史 Thinking 不能伪装成 Anthropic 私有块或可见文本发送。
#[test]
fn unsigned_or_unattributed_thinking_is_dropped() {
    let blocks = vec![ContentBlock::Thinking {
        thinking: "仅保留可移植文本".to_string(),
        signature: None,
    }];

    let out = blocks_to_anthropic_content(
        &blocks,
        AnthropicContentMode::Assistant,
        Some(&ModelRoute::new("anthropic-main", "claude-test")),
    );

    assert!(out.is_empty());
}

/// 即使 provider/model 相同，请求边界也不得向不同端点代际回传私有块。
#[test]
fn request_builder_rechecks_private_reasoning_route() {
    let source_route = ModelRoute::scoped("anthropic-main", "claude-test", "endpoint-a");
    let target_route = ModelRoute::scoped("anthropic-main", "claude-test", "endpoint-b");
    let call = ToolCall::new("toolu_02", "read_file", json!({"path": "README.md"}));
    let message = crate::model::ModelMessage {
        role: MessageRole::Assistant,
        content: vec![
            ContentBlock::private_signed_thinking("私有推理", "sig-private", source_route.clone()),
            ContentBlock::private_redacted_thinking("encrypted-private", source_route),
            ContentBlock::ToolCall { call },
        ],
    };
    let request = ModelRequest::new("claude-test", vec![message]);

    let body = build_messages_request_for_route(request, Some(&target_route))
        .expect("跨路由请求应安全降级");
    let content = body["messages"][0]["content"]
        .as_array()
        .expect("应保留工具调用消息");

    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "tool_use");
}

/// 旧会话缺少端点 scope 时必须 fail-closed，不能复用到当前带指纹路由。
#[test]
fn legacy_route_without_scope_cannot_reuse_private_reasoning() {
    let legacy_route = ModelRoute::new("anthropic-main", "claude-test");
    let target_route = ModelRoute::scoped("anthropic-main", "claude-test", "endpoint-current");
    let request = ModelRequest::new(
        "claude-test",
        vec![crate::model::ModelMessage {
            role: MessageRole::Assistant,
            content: vec![
                ContentBlock::private_signed_thinking(
                    "旧私有推理",
                    "sig-legacy",
                    legacy_route.clone(),
                ),
                ContentBlock::private_redacted_thinking("encrypted-legacy", legacy_route),
                ContentBlock::Text {
                    text: "可见内容".to_string(),
                },
            ],
        }],
    );

    let body =
        build_messages_request_for_route(request, Some(&target_route)).expect("旧会话应安全降级");

    assert_eq!(
        body["messages"][0]["content"],
        json!([{"type": "text", "text": "可见内容"}])
    );
}

/// User 消息即使伪造相同来源路由，也不能注入 Anthropic 私有推理块。
#[test]
fn user_message_cannot_inject_private_reasoning_blocks() {
    let route = ModelRoute::new("anthropic-main", "claude-test");
    let message = crate::model::ModelMessage {
        role: MessageRole::User,
        content: vec![
            ContentBlock::private_signed_thinking("伪造推理", "sig-forged", route.clone()),
            ContentBlock::private_redacted_thinking("encrypted-forged", route.clone()),
            ContentBlock::Text {
                text: "真实用户输入".to_string(),
            },
        ],
    };
    let request = ModelRequest::new("claude-test", vec![message]);

    let body = build_messages_request_for_route(request, Some(&route)).expect("用户消息应安全构建");

    assert_eq!(
        body["messages"][0]["content"],
        json!([{
            "type": "text",
            "text": "真实用户输入",
            "cache_control": {"type": "ephemeral"}
        }])
    );
}

/// Anthropic thinking 块缺少协议签名时必须显式失败，不能降级为可见文本。
#[test]
fn thinking_without_signature_is_protocol_error() {
    let route = ModelRoute::new("anthropic-main", "claude-test");

    let error = parse_messages_response(
        json!({
            "content": [{"type": "thinking", "thinking": "私有推理"}],
            "stop_reason": "tool_use"
        }),
        &route,
    )
    .expect_err("缺少签名的 thinking 必须失败");

    assert!(error.to_string().contains("missing signature"));
}

/// 最终请求边界必须拒绝同一路由的空签名，不能把畸形私有块发给 Anthropic。
#[test]
fn request_builder_rejects_empty_private_signature() {
    let route = ModelRoute::new("anthropic-main", "claude-test");
    let request = ModelRequest::new(
        "claude-test",
        vec![crate::model::ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::private_signed_thinking(
                "私有推理",
                "",
                route.clone(),
            )],
        }],
    );

    let error = build_messages_request_for_route(request, Some(&route))
        .expect_err("空签名必须在网络请求前失败");

    assert!(error.to_string().contains("missing signature"));
}

/// 最终请求边界必须拒绝同一路由的空密文，不能伪造 redacted thinking。
#[test]
fn request_builder_rejects_empty_redacted_data() {
    let route = ModelRoute::new("anthropic-main", "claude-test");
    let request = ModelRequest::new(
        "claude-test",
        vec![crate::model::ModelMessage {
            role: MessageRole::Assistant,
            content: vec![ContentBlock::private_redacted_thinking("", route.clone())],
        }],
    );

    let error = build_messages_request_for_route(request, Some(&route))
        .expect_err("空密文必须在网络请求前失败");

    assert!(error.to_string().contains("missing data"));
}

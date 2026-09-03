//! OpenAI 协议转换、参数过滤和流式聚合回归测试。

use super::stream::{
    finalize_responses_stream_state, handle_chat_completions_sse_data, handle_responses_sse_data,
    ChatCompletionsStreamState, ResponsesStreamState,
};
use super::*;
use crate::model::{GatewayKind, ModelStreamEvent, PromptCacheRetention};

/// 构造带文本、图片和 PDF 的用户消息，覆盖附件转换路径。
fn attachment_message() -> crate::model::ModelMessage {
    crate::model::ModelMessage {
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
    let message = crate::model::ModelMessage::text(MessageRole::User, "你好");
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

/// 非官方 Responses 网关也必须保留标准输出上限，避免推理耗尽网关默认额度。
#[test]
fn allowed_params_proxy_uses_compatible() {
    let params = responses_allowed_params(&ProviderKind::OpenAi, "https://api.phrolova.uno/v1");
    assert!(params.contains(&"max_output_tokens"));
    assert!(!params.contains(&"store"));
}

/// OpenAiCompatible 类型使用兼容参数白名单，但仍保留 Responses 标准输出上限。
#[test]
fn allowed_params_compatible_kind_uses_compatible() {
    let params =
        responses_allowed_params(&ProviderKind::OpenAiCompatible, "https://localhost:8080/v1");
    assert!(params.contains(&"max_output_tokens"));
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
    assert_eq!(body["max_output_tokens"], 32);
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

/// Responses 推理请求必须携带自动摘要，供客户端观察等级确实生效。
#[test]
fn responses_reasoning_requests_visible_summary() {
    let mut request = ModelRequest::new("gpt-test", Vec::new());
    request.reasoning = ReasoningLevel::High;

    let body = build_responses_request(request, RESPONSES_PARAMS_COMPATIBLE, GatewayKind::OpenAi)
        .expect("应构建推理请求");

    assert_eq!(
        body["reasoning"],
        json!({"effort": "high", "summary": "auto"})
    );
}

/// 关闭推理时不得请求摘要，避免把普通模型请求误标为推理模式。
#[test]
fn responses_reasoning_off_omits_reasoning_object() {
    let request = ModelRequest::new("gpt-test", Vec::new());

    let body = build_responses_request(request, RESPONSES_PARAMS_COMPATIBLE, GatewayKind::OpenAi)
        .expect("应构建普通请求");

    assert!(body.get("reasoning").is_none());
}

/// Responses 与 Chat 适配器都不得让 provider_options 覆盖可信输出上限。
#[test]
fn provider_options_cannot_override_trusted_output_limits() {
    let mut responses_request = ModelRequest::new("gpt-test", Vec::new());
    responses_request.max_tokens = Some(128);
    responses_request.provider_options = json!({
        "max_output_tokens": 99_999,
        "max_tokens": 88_888,
        "n": 4
    });
    let responses_body = build_responses_request(
        responses_request,
        RESPONSES_PARAMS_COMPATIBLE,
        GatewayKind::OpenAi,
    )
    .expect("Responses 请求应成功构建");

    let mut chat_request = ModelRequest::new("gpt-test", Vec::new());
    chat_request.max_tokens = Some(256);
    chat_request.provider_options = json!({
        "max_tokens": 99_999,
        "max_completion_tokens": 88_888,
        "best_of": 3
    });
    let chat_body = build_chat_completions_request(chat_request, GatewayKind::OpenAi)
        .expect("Chat 请求应成功构建");

    assert_eq!(responses_body["max_output_tokens"], 128);
    assert_eq!(responses_body["max_tokens"], 128);
    assert_eq!(responses_body["n"], 1);
    assert_eq!(chat_body["max_tokens"], 256);
    assert_eq!(chat_body["max_completion_tokens"], 256);
    assert_eq!(chat_body["best_of"], 1);
}

/// 缓存亲和键直接取会话 ID，两种协议使用同一个值。
#[test]
fn default_short_cache_sends_session_id_as_cache_key() {
    let mut request = ModelRequest::new(
        "gpt-test",
        vec![crate::model::ModelMessage::text(
            crate::model::MessageRole::User,
            "检查缓存",
        )],
    );
    request.system = Some("系统提示".into());
    request.session_id = Some("session-abc".into());

    let responses = build_responses_request(
        request.clone(),
        RESPONSES_PARAMS_COMPATIBLE,
        GatewayKind::OpenAi,
    )
    .expect("Responses 请求应成功构建");
    let chat =
        build_chat_completions_request(request, GatewayKind::OpenAi).expect("Chat 请求应成功构建");

    assert_eq!(responses["prompt_cache_key"], "session-abc");
    assert_eq!(chat["prompt_cache_key"], "session-abc");
    assert!(responses.get("prompt_cache_retention").is_none());
    assert!(chat.get("prompt_cache_retention").is_none());
}

/// 键必须只随会话变化：同一会话追加消息不得改键，不同会话必须拿到不同键。
///
/// 键一旦随请求内容漂移，同一会话的连续请求就会被路由到不同机器，前缀缓存全部落空；
/// 反过来所有会话共用一个键又会让它们挤在同一台机器上互相驱逐。
#[test]
fn openai_cache_key_tracks_session_only() {
    let request_for = |session: &str, first: &str| {
        let mut request = ModelRequest::new(
            "gpt-test",
            vec![crate::model::ModelMessage::text(
                crate::model::MessageRole::User,
                first,
            )],
        );
        request.system = Some("系统提示".into());
        request.session_id = Some(session.into());
        request
    };

    let first_turn = request_for("session-1", "第一轮请求");
    // 同一会话的后续轮次：消息变了，键不能变。
    let later_turn = request_for("session-1", "完全不同的一轮请求");
    let other_session = request_for("session-2", "第一轮请求");

    let key = |request: ModelRequest| {
        build_responses_request(request, RESPONSES_PARAMS_COMPATIBLE, GatewayKind::OpenAi)
            .expect("请求应成功构建")["prompt_cache_key"]
            .as_str()
            .expect("应包含缓存亲和键")
            .to_string()
    };

    assert_eq!(key(first_turn.clone()), key(later_turn));
    assert_ne!(key(first_turn), key(other_session));
}

/// 未提供会话 ID 时不发送缓存键，交给服务商按前缀隐式缓存。
#[test]
fn missing_session_id_omits_openai_cache_key() {
    let request = ModelRequest::new("gpt-test", Vec::new());

    let responses = build_responses_request(
        request.clone(),
        RESPONSES_PARAMS_COMPATIBLE,
        GatewayKind::OpenAi,
    )
    .expect("Responses 请求应成功构建");
    let chat =
        build_chat_completions_request(request, GatewayKind::OpenAi).expect("Chat 请求应成功构建");

    assert!(responses.get("prompt_cache_key").is_none());
    assert!(chat.get("prompt_cache_key").is_none());
}

/// 超长会话 ID 必须按字符截断到 OpenAI 允许的键长上限。
#[test]
fn oversized_session_id_is_clamped_to_key_limit() {
    let mut request = ModelRequest::new("gpt-test", Vec::new());
    request.session_id = Some("会".repeat(100));

    let responses =
        build_responses_request(request, RESPONSES_PARAMS_COMPATIBLE, GatewayKind::OpenAi)
            .expect("Responses 请求应成功构建");

    let key = responses["prompt_cache_key"]
        .as_str()
        .expect("应包含缓存亲和键");
    assert_eq!(key.chars().count(), OPENAI_PROMPT_CACHE_KEY_MAX_CHARS);
}

/// 显式关闭缓存时不得生成 OpenAI 缓存字段。
#[test]
fn no_cache_retention_omits_openai_cache_fields() {
    let mut request = ModelRequest::new("gpt-test", Vec::new());
    request.cache_retention = PromptCacheRetention::None;
    // 即使调用方给了会话 ID，关闭缓存时也不得发送缓存键。
    request.session_id = Some("session-abc".into());

    let responses = build_responses_request(
        request.clone(),
        RESPONSES_PARAMS_COMPATIBLE,
        GatewayKind::OpenAi,
    )
    .expect("Responses 无缓存请求应成功构建");
    let chat = build_chat_completions_request(request, GatewayKind::OpenAi)
        .expect("Chat 无缓存请求应成功构建");

    assert!(responses.get("prompt_cache_key").is_none());
    assert!(responses.get("prompt_cache_retention").is_none());
    assert!(chat.get("prompt_cache_key").is_none());
    assert!(chat.get("prompt_cache_retention").is_none());
}

/// OpenAI 长期缓存映射为 24 小时保留期，并同时适用于两种协议。
#[test]
fn long_cache_retention_maps_to_openai_24h() {
    let mut request = ModelRequest::new("gpt-test", Vec::new());
    request.cache_retention = PromptCacheRetention::Long;

    let responses = build_responses_request(
        request.clone(),
        RESPONSES_PARAMS_COMPATIBLE,
        GatewayKind::OpenAi,
    )
    .expect("Responses 长缓存请求应成功构建");
    let chat = build_chat_completions_request(request, GatewayKind::OpenAi)
        .expect("Chat 长缓存请求应成功构建");

    assert_eq!(responses["prompt_cache_retention"], "24h");
    assert_eq!(chat["prompt_cache_retention"], "24h");
}

/// 兼容网关默认不发送 `prompt_cache_key`，只有显式要求长期缓存时才带上。
///
/// 该 body 参数是 OpenAI 官方语义，兼容网关多数不认；它们的缓存路由走会话亲和 header，
/// 盲发未知字段没有收益还可能被网关拒绝。
#[test]
fn compatible_gateway_omits_cache_key_unless_long_retention() {
    let mut request = ModelRequest::new("gpt-test", Vec::new());
    request.session_id = Some("session-abc".into());

    let short = build_responses_request(
        request.clone(),
        RESPONSES_PARAMS_COMPATIBLE,
        GatewayKind::OpenAiCompatible,
    )
    .expect("兼容网关短缓存请求应成功构建");
    let chat = build_chat_completions_request(request.clone(), GatewayKind::OpenAiCompatible)
        .expect("兼容网关 Chat 请求应成功构建");
    request.cache_retention = PromptCacheRetention::Long;
    let long = build_responses_request(
        request,
        RESPONSES_PARAMS_COMPATIBLE,
        GatewayKind::OpenAiCompatible,
    )
    .expect("兼容网关长缓存请求应成功构建");

    assert!(short.get("prompt_cache_key").is_none());
    assert!(chat.get("prompt_cache_key").is_none());
    assert_eq!(long["prompt_cache_key"], "session-abc");
}

/// 网关厂商决定整套缓存路由约定：官方端点用 body 缓存键，其余网关用亲和 header。
#[test]
fn gateway_kind_selects_cache_routing_convention() {
    assert!(GatewayKind::OpenAi.reads_prompt_cache_key());
    assert!(!GatewayKind::OpenAiCompatible.reads_prompt_cache_key());
    assert!(!GatewayKind::OpenRouter.reads_prompt_cache_key());

    assert_eq!(GatewayKind::OpenAi.session_affinity(), SessionAffinity::Off);
    assert_eq!(
        GatewayKind::Anthropic.session_affinity(),
        SessionAffinity::Off
    );
    assert_eq!(
        GatewayKind::OpenRouter.session_affinity(),
        SessionAffinity::OpenRouter
    );
    // 缺省的通用兼容网关必须拿到亲和 header，否则完全没有会话级缓存路由。
    assert_eq!(
        GatewayKind::default().session_affinity(),
        SessionAffinity::OpenAi
    );
}

/// 会话亲和 header 按网关格式发送，`Off` 时一个都不加。
#[test]
fn session_affinity_headers_follow_configured_format() {
    let base = HeaderMap::new();

    let off = session_affinity_headers(&base, SessionAffinity::Off, Some("session-abc"));
    let openai = session_affinity_headers(&base, SessionAffinity::OpenAi, Some("session-abc"));
    let no_session =
        session_affinity_headers(&base, SessionAffinity::OpenAiNoSession, Some("session-abc"));
    let openrouter =
        session_affinity_headers(&base, SessionAffinity::OpenRouter, Some("session-abc"));
    let missing_session = session_affinity_headers(&base, SessionAffinity::OpenAi, None);

    assert!(off.is_empty());
    assert!(missing_session.is_empty());
    assert_eq!(openai["session_id"], "session-abc");
    assert_eq!(openai["x-client-request-id"], "session-abc");
    assert_eq!(openai["x-session-affinity"], "session-abc");
    assert!(no_session.get("session_id").is_none());
    assert_eq!(no_session["x-client-request-id"], "session-abc");
    assert_eq!(no_session["x-session-affinity"], "session-abc");
    assert_eq!(openrouter["x-session-id"], "session-abc");
    assert!(openrouter.get("x-session-affinity").is_none());
}

/// 特殊兼容网关仍可通过 provider_options 显式提供缓存路由字段，无需扩张运行 API。
#[test]
fn provider_options_can_supply_openai_cache_affinity() {
    let mut request = ModelRequest::new("gpt-test", Vec::new());
    request.provider_options = json!({"prompt_cache_key": "stable-session"});

    let body = build_responses_request(request, RESPONSES_PARAMS_COMPATIBLE, GatewayKind::OpenAi)
        .expect("兼容网关缓存路由请求应成功构建");

    assert_eq!(body["prompt_cache_key"], "stable-session");
}

/// OpenAI usage 中缓存读写已包含在 input tokens，解析后必须拆成互斥分项。
#[test]
fn openai_usage_separates_cached_input_tokens() {
    let usage = parse_openai_usage(Some(&json!({
        "input_tokens": 1_000,
        "output_tokens": 100,
        "total_tokens": 1_100,
        "input_tokens_details": {
            "cached_tokens": 600,
            "cache_write_tokens": 100
        }
    })))
    .expect("应解析 OpenAI 用量");

    assert_eq!(usage.input_tokens, Some(300));
    assert_eq!(usage.output_tokens, Some(100));
    assert_eq!(usage.cache_read_tokens, Some(600));
    assert_eq!(usage.cache_write_tokens, Some(100));
    assert_eq!(usage.total_tokens, Some(1_100));
}

/// Chat Completions 的兼容字段也必须解析缓存命中并扣除重复输入量。
#[test]
fn chat_usage_parses_cached_prompt_tokens() {
    let usage = parse_openai_usage(Some(&json!({
        "prompt_tokens": 900,
        "completion_tokens": 80,
        "total_tokens": 980,
        "prompt_tokens_details": {"cached_tokens": 500}
    })))
    .expect("应解析 Chat 用量");

    assert_eq!(usage.input_tokens, Some(400));
    assert_eq!(usage.cache_read_tokens, Some(500));
    assert_eq!(usage.cache_write_tokens, None);
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

/// 兼容网关只发送正文完成事件时，也必须恢复完整正文并避免重复已有增量。
#[tokio::test]
async fn responses_sse_recovers_text_from_done_event() {
    let (sender, mut stream) = ModelEventStream::channel();
    let mut state = ResponsesStreamState::default();

    handle_responses_sse_data(
        r#"{"type":"response.output_text.delta","output_index":0,"delta":"你"}"#,
        &sender,
        &mut state,
    )
    .expect("应该解析正文增量");
    handle_responses_sse_data(
        r#"{"type":"response.output_text.done","output_index":0,"text":"你好"}"#,
        &sender,
        &mut state,
    )
    .expect("应该解析正文完成事件");

    assert_eq!(state.text, "你好");
    match stream.next().await.expect("应该收到首个正文增量") {
        ModelStreamEvent::TextDelta { delta, .. } => assert_eq!(delta, "你"),
        event => panic!("收到非预期事件: {event:?}"),
    }
    match stream.next().await.expect("应该收到完成事件补齐的正文") {
        ModelStreamEvent::TextDelta { delta, .. } => assert_eq!(delta, "好"),
        event => panic!("收到非预期事件: {event:?}"),
    }
}

/// 兼容网关省略正文增量与正文完成事件时，消息完成事件仍必须恢复正文。
#[tokio::test]
async fn responses_sse_recovers_text_from_output_item_done() {
    let (sender, mut stream) = ModelEventStream::channel();
    let mut state = ResponsesStreamState::default();

    handle_responses_sse_data(
        r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":"检查完成"}]}}"#,
        &sender,
        &mut state,
    )
    .expect("应该从消息完成事件恢复正文");

    assert_eq!(state.text, "检查完成");
    match stream.next().await.expect("应该收到恢复的正文增量") {
        ModelStreamEvent::TextDelta { delta, .. } => assert_eq!(delta, "检查完成"),
        event => panic!("收到非预期事件: {event:?}"),
    }
}

/// 兼容网关只在 added 与参数完成事件中提供函数调用时，最终精简响应不得丢失工具调用。
#[tokio::test]
async fn responses_sse_recovers_function_call_without_output_item_done() {
    let (sender, mut stream) = ModelEventStream::channel();
    let mut state = ResponsesStreamState::default();

    handle_responses_sse_data(
        r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"list_directory","arguments":""}}"#,
        &sender,
        &mut state,
    )
    .expect("应该记录函数调用起始项");
    handle_responses_sse_data(
        r#"{"type":"response.function_call_arguments.delta","output_index":1,"delta":"{\"path\":"}"#,
        &sender,
        &mut state,
    )
    .expect("应该聚合函数参数增量");
    handle_responses_sse_data(
        r#"{"type":"response.function_call_arguments.done","output_index":1,"arguments":"{\"path\":\".\"}"}"#,
        &sender,
        &mut state,
    )
    .expect("应该完成函数调用");
    handle_responses_sse_data(
        r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
        &sender,
        &mut state,
    )
    .expect("应该记录精简完成响应");

    assert!(matches!(
        stream.next().await.expect("应该收到参数增量"),
        ModelStreamEvent::ToolCallDelta { index: 1, .. }
    ));
    match stream.next().await.expect("应该收到工具调用完成事件") {
        ModelStreamEvent::ToolCallEnd { index, call } => {
            assert_eq!(index, 1);
            assert_eq!(call.name, "list_directory");
            assert_eq!(call.args, serde_json::json!({"path": "."}));
        }
        event => panic!("收到非预期事件: {event:?}"),
    }

    let response = finalize_responses_stream_state(state).expect("应该生成最终模型响应");
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "list_directory");
}

/// Responses 完成事件中的缓存用量必须进入最终流式响应。
#[tokio::test]
async fn responses_sse_preserves_cache_usage() {
    let (sender, _stream) = ModelEventStream::channel();
    let mut state = ResponsesStreamState::default();

    handle_responses_sse_data(
        r#"{"type":"response.completed","response":{"status":"completed","output":[],"usage":{"input_tokens":1200,"output_tokens":100,"total_tokens":1300,"input_tokens_details":{"cached_tokens":800,"cache_write_tokens":100}}}}"#,
        &sender,
        &mut state,
    )
    .expect("应该记录带缓存用量的完成响应");

    let response = finalize_responses_stream_state(state).expect("应该生成最终模型响应");
    let usage = response.usage.expect("最终响应应保留用量");
    assert_eq!(usage.input_tokens, Some(300));
    assert_eq!(usage.cache_read_tokens, Some(800));
    assert_eq!(usage.cache_write_tokens, Some(100));
    assert_eq!(usage.total_tokens, Some(1_300));
}

/// Responses 推理摘要 SSE 必须进入独立思考事件，不能混入最终回答文本。
#[tokio::test]
async fn responses_sse_emits_reasoning_summary_delta() {
    let (sender, mut stream) = ModelEventStream::channel();
    let mut state = ResponsesStreamState::default();

    handle_responses_sse_data(
        r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"检查权限边界"}"#,
        &sender,
        &mut state,
    )
    .expect("应该解析推理摘要增量");

    assert!(state.text.is_empty());
    match stream.next().await.expect("应该收到思考事件") {
        ModelStreamEvent::ThinkingDelta { index, delta } => {
            assert_eq!(index, 0);
            assert_eq!(delta, "检查权限边界");
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

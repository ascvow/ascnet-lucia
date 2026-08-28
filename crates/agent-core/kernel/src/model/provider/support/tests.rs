//! Provider 共享 SSE 与响应处理测试。

use super::*;

/// SSE 中 response.completed 事件的 response 字段被提取。
#[test]
fn sse_extracts_completed_response() {
    let body = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n",
        "\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n",
    );

    let value = parse_sse_final_json(body).expect("应该提取到最终响应");
    assert_eq!(value["status"], "completed");
}

/// 没有 completed 事件时回退到最后一个 data 事件。
#[test]
fn sse_falls_back_to_last_event() {
    let body = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n",
        "data: [DONE]\n",
    );

    let value = parse_sse_final_json(body).expect("应该回退到最后事件");
    assert_eq!(value["status"], "in_progress");
}

/// 完全无法解析时返回 None。
#[test]
fn sse_returns_none_for_garbage() {
    assert!(parse_sse_final_json("not sse at all").is_none());
}

/// completed 事件缺少文本时，用 output_text.delta 聚合补全。
#[test]
fn sse_aggregates_deltas_when_completed_has_no_text() {
    let body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"你\"}\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"好\"}\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n",
    );

    let value = parse_sse_final_json(body).expect("应该提取到最终响应");
    let text = value["output"][0]["content"][0]["text"]
        .as_str()
        .expect("应该有聚合文本");
    assert_eq!(text, "你好");
}

/// completed 事件已有文本时不重复注入聚合文本。
#[test]
fn sse_keeps_existing_output_text() {
    let body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"abc\"}\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"完整文本\"}]}]}}\n",
    );

    let value = parse_sse_final_json(body).expect("应该提取到最终响应");
    let output = value["output"].as_array().expect("output 应为数组");
    assert_eq!(output.len(), 1);
}

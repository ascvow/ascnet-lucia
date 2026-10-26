//! Provider 共享 SSE 与响应处理测试。

use super::*;

#[test]
fn http_retry_classification_does_not_retry_auth_or_bad_requests() {
    use crate::model::contract::is_retryable_model_error;
    for status in [408, 429, 500, 502, 503, 504, 529] {
        let error = http_status_error(
            reqwest::StatusCode::from_u16(status).unwrap(),
            "temporary".into(),
        )
        .context("outer context");
        assert!(is_retryable_model_error(&error), "{status}");
    }
    for status in [400, 401, 403, 404, 409, 413, 422] {
        assert!(
            !is_retryable_model_error(&http_status_error(
                reqwest::StatusCode::from_u16(status).unwrap(),
                "permanent".into()
            )),
            "{status}"
        );
    }
    assert!(!is_retryable_model_error(&anyhow!(
        "connection timeout text alone is not a retry signal"
    )));
}

/// 状态码已经确定时，损坏的错误正文不改变鉴权失败与临时服务错误的类别。
#[tokio::test]
async fn malformed_error_bodies_preserve_http_retry_classification() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for (status, retryable) in [(401, false), (503, true)] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.read(&mut [0; 4096]).await.unwrap();
            socket.write_all(format!("HTTP/1.1 {status} Error\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx").as_bytes()).await.unwrap();
        });
        let response = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .unwrap();
        let error = decode_json_response(response, "test").await.unwrap_err();
        assert_eq!(
            crate::model::contract::is_retryable_model_error(&error),
            retryable
        );
        server.await.unwrap();
    }
}

/// 实际 chunk 限制包含没有 Content-Length 的响应，错误预览不读取尾部。
#[tokio::test]
async fn bounded_body_rejects_overflow_and_stops_preview() {
    let chunks = futures_util::stream::iter([Ok(&b"ab"[..]), Ok(&b"cd"[..])]);
    assert_eq!(
        read_bounded_chunks(chunks, 4, false).await.unwrap(),
        b"abcd"
    );
    let chunks = futures_util::stream::iter([Ok(&b"abcd"[..]), Ok(&b"e"[..])]);
    assert!(read_bounded_chunks(chunks, 4, false).await.is_err());
    let chunks =
        futures_util::stream::iter([Ok(&b"abcdef"[..]), Err(anyhow!("must not read tail"))]);
    assert_eq!(read_bounded_chunks(chunks, 4, true).await.unwrap(), b"abcd");
}

/// 每字节分块保持 UTF-8/CRLF/多行 data 与尾部不带换行的语义。
#[tokio::test]
async fn bounded_sse_preserves_chunk_boundaries_and_limits_total_input() {
    let wire = "data: 你\r\ndata: 好\r\n\r\ndata: tail";
    let chunks = futures_util::stream::iter(wire.as_bytes().chunks(1).map(Ok::<_, anyhow::Error>));
    let mut events = Vec::new();
    consume_sse_chunks(chunks, wire.len(), |data| {
        events.push(data.to_owned());
        Ok(())
    })
    .await
    .unwrap();
    assert_eq!(events, ["你\n好", "tail"]);
    for body in [
        "x".repeat(33),
        "data: x\n".repeat(5),
        "data: x\n\n".repeat(4),
    ] {
        let chunks =
            futures_util::stream::iter(body.as_bytes().chunks(1).map(Ok::<_, anyhow::Error>));
        assert!(consume_sse_chunks(chunks, 32, |_| Ok(())).await.is_err());
    }
}

/// 真实 HTTP 302 不应自动转发凭证；测试完全使用本机服务器。
#[tokio::test]
async fn provider_client_does_not_follow_redirect() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        let read = socket.read(&mut buffer).await.unwrap();
        assert!(read > 0);
        socket.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/not-called\r\nContent-Length: 0\r\n\r\n").await.unwrap();
    });
    let response = http_client()
        .unwrap()
        .get(format!("http://{address}/"))
        .header("x-api-key", "test-key")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    server.await.unwrap();
}

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

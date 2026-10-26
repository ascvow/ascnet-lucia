//! 使用本机 HTTP 服务验证真实 Provider → 事件流 → Agent 的重连链路，不访问外网。

#![cfg(feature = "openai")]

use agent_core::{
    model::{ModelProviderConfig, OpenAiProtocol, ProviderKind},
    Agent, AgentOptions, ModelGateway,
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 顺序响应并完整读取请求，避免服务器提前关闭请求体导致测试误判为连接中断。
async fn serve(
    responses: Vec<(u16, &'static str, &'static str)>,
) -> (String, tokio::task::JoinHandle<Vec<(Instant, Vec<u8>)>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, content_type, body) in responses {
            let (mut socket, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut request = Vec::new();
            loop {
                let mut buffer = [0; 4096];
                let read = socket.read(&mut buffer).await.unwrap();
                assert!(read > 0, "客户端应发送完整请求");
                request.extend_from_slice(&buffer[..read]);
                assert!(request.len() < 128 * 1024);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length: usize = header
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length:")
                                .map(|length| length.trim().parse().unwrap())
                        })
                        .unwrap();
                    if request.len() >= end + 4 + length {
                        requests
                            .push((Instant::now(), request[end + 4..end + 4 + length].to_vec()));
                        break;
                    }
                }
            }
            let header = format!("HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(body.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        }
        requests
    });
    (url, task)
}

fn agent(url: String, stream: bool, protocol: OpenAiProtocol) -> Agent {
    let gateway = ModelGateway::new();
    gateway
        .register_from_config(
            ModelProviderConfig::new("local", ProviderKind::OpenAi, "local-test-key")
                .with_base_url(url)
                .with_openai_protocol(protocol),
        )
        .unwrap();
    Agent::new(
        gateway,
        AgentOptions {
            stream,
            max_retries: 5,
            retry_interval_ms: 15,
            ..AgentOptions::default().with_model_route("local", "test-model")
        },
    )
}

const CHAT_JSON: &str = r#"{"choices":[{"message":{"role":"assistant","content":"recovered"},"finish_reason":"stop"}]}"#;
const CHAT_SSE: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"recovered\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

/// HTTP 429/5xx 在流式与非流式模式均经过实际间隔后重试同一请求。
#[tokio::test]
async fn http_transient_statuses_reconnect_in_streaming_and_batch_modes() {
    for stream in [false, true] {
        let (content_type, body) = if stream {
            ("text/event-stream", CHAT_SSE)
        } else {
            ("application/json", CHAT_JSON)
        };
        let (url, server) = serve(vec![
            (
                503,
                "application/json",
                r#"{"error":{"message":"unavailable"}}"#,
            ),
            (
                429,
                "application/json",
                r#"{"error":{"message":"rate limited"}}"#,
            ),
            (200, content_type, body),
        ])
        .await;
        let run = tokio::time::timeout(
            Duration::from_secs(3),
            agent(url, stream, OpenAiProtocol::ChatCompletions).run("retry"),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(run.final_text, "recovered");
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 3);
        for pair in requests.windows(2) {
            assert_eq!(pair[0].1, pair[1].1);
            assert!(pair[1].0.duration_since(pair[0].0) >= Duration::from_millis(15));
        }
    }
}

/// 鉴权、权限与坏请求不重连；正文即使伪装成“临时故障”也不能改变分类。
#[tokio::test]
async fn http_permanent_statuses_never_reconnect() {
    for stream in [false, true] {
        for status in [400, 401, 403] {
            let (url, server) = serve(vec![(
                status,
                "application/json",
                r#"{"error":{"message":"temporary timeout 503 retry me"}}"#,
            )])
            .await;
            let error = agent(url, stream, OpenAiProtocol::ChatCompletions)
                .run("permanent")
                .await
                .unwrap_err();
            assert!(error.to_string().contains("已重试 0 次"));
            assert_eq!(server.await.unwrap().len(), 1);
        }
    }
}

/// 成功建流后的结构化临时错误也重连，但自然语言正文和永久错误码不能触发。
#[tokio::test]
async fn sse_structured_errors_keep_retry_classification() {
    for (protocol, failure, success) in [
        (OpenAiProtocol::ChatCompletions,
         "data: {\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"busy\"}}\n\n", CHAT_SSE),
        (OpenAiProtocol::Responses,
         "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"busy\"}}}\n\n",
         "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"recovered\"}]}]}}\n\n"),
    ] {
        let (url, server) = serve(vec![(200, "text/event-stream", failure), (200, "text/event-stream", success)]).await;
        assert_eq!(agent(url, true, protocol).run("stream failure").await.unwrap().final_text, "recovered");
        assert_eq!(server.await.unwrap().len(), 2);
    }
    for (protocol, failure) in [
        (OpenAiProtocol::ChatCompletions,
         "data: {\"error\":{\"code\":\"insufficient_quota\",\"type\":\"server_error\",\"message\":\"503 timeout\"}}\n\n"),
        (OpenAiProtocol::Responses,
         "data: {\"type\":\"error\",\"message\":\"server_error 503 timeout\"}\n\n"),
    ] {
        let (url, server) = serve(vec![(200, "text/event-stream", failure)]).await;
        let error = agent(url, true, protocol).run("permanent stream error").await.unwrap_err();
        assert!(error.to_string().contains("已重试 0 次"));
        assert_eq!(server.await.unwrap().len(), 1);
    }
}

/// 模型明确到达输出上限时是完整终态，不得把它当成断线绕过上限。
#[tokio::test]
async fn output_limit_terminal_event_does_not_reconnect() {
    let body = "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"limited\"}]}]}}\n\n";
    let (url, server) = serve(vec![(200, "text/event-stream", body)]).await;
    let run = agent(url, true, OpenAiProtocol::Responses)
        .run("output limit")
        .await
        .unwrap();
    assert_eq!(run.final_text, "limited");
    assert_eq!(server.await.unwrap().len(), 1);
}

/// HTTP 正常结束但 SSE 缺少协议终态也算断线，不能把半段文本或工具参数当作成功。
#[tokio::test]
async fn truncated_sse_reconnects_without_persisting_partial_output() {
    let responses_complete = "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"recovered\"}]}]}}\n\n";
    for (protocol, partial, complete) in [
        (OpenAiProtocol::ChatCompletions,
         "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"stale-partial\",\"tool_calls\":[{\"index\":0,\"id\":\"partial-call\",\"function\":{\"name\":\"never_execute\",\"arguments\":\"{\"}}]}}]}\n\n", CHAT_SSE),
        (OpenAiProtocol::Responses,
         "data: {\"type\":\"response.output_text.delta\",\"delta\":\"stale-partial\"}\n\n", responses_complete),
    ] {
        let (url, server) = serve(vec![(200, "text/event-stream", partial), (200, "text/event-stream", complete)]).await;
        let run = agent(url, true, protocol).run("interrupted").await.unwrap();
        assert_eq!(run.final_text, "recovered");
        let history = serde_json::to_string(run.session.messages()).unwrap();
        assert!(!history.contains("stale-partial"));
        assert!(!history.contains("partial-call"));
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].1, requests[1].1);
    }
}

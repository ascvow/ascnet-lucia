//! MCP stdio 协议测试。
//!
//! 启动真实二进制，用真实 MCP 客户端走 JSON-RPC 线协议，覆盖 `initialize`、
//! `tools/list` 与 `tools/call`。只测底层函数不足以证明对外协议可用：
//! 工具注册、参数 schema、错误映射与 stdout 纯净度都只在这一层暴露。

mod support;

use std::path::Path;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::RunningService;
use rmcp::{RoleClient, ServiceExt};
use tokio::process::Command;

use support::{sample_repo, write_file};

/// 不配置 Provider：MCP 协议本身不应依赖 Embedding 服务。
const NO_PROVIDER_CONFIG: &str = "[embedding]\nprovider = \"none\"\n";

/// 启动 `codesearch serve --transport stdio` 并完成 MCP 握手。
///
/// 关闭 watch，避免后台监听带来时序不确定性。
async fn start_server(root: &Path) -> RunningService<RoleClient, ()> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_codesearch"));
    command
        .arg("serve")
        .arg("--root")
        .arg(root)
        .arg("--transport")
        .arg("stdio")
        .arg("--watch")
        .arg("false")
        // 保持默认日志级别：子进程 stderr 会直接进入测试输出，开高会淹没结果。
        // stdout 纯净性由 `stdout_carries_only_protocol_frames` 在 trace 级别单独验证。
        .env("CODESEARCH_LOG", "warn");
    let process = rmcp::transport::TokioChildProcess::new(command).expect("启动 codesearch serve");
    ().serve(process).await.expect("MCP 握手")
}

/// 调用一个工具。
async fn call(
    service: &RunningService<RoleClient, ()>,
    name: &'static str,
    arguments: serde_json::Value,
) -> Result<CallToolResult, rmcp::ServiceError> {
    let arguments = arguments.as_object().cloned().expect("参数必须是对象");
    service
        .call_tool(CallToolRequestParams::new(name).with_arguments(arguments))
        .await
}

/// 取出工具返回的结构化 JSON。
fn structured(result: &CallToolResult) -> serde_json::Value {
    result
        .structured_content
        .clone()
        .expect("工具应返回结构化内容")
}

#[tokio::test(flavor = "multi_thread")]
/// 工具清单包含新的语义入口，同时保留旧入口以兼容已有 Agent 配置。
async fn tools_list_exposes_search_context_and_compatible_tools() {
    let repo = sample_repo();
    write_file(&repo.path().join("codesearch.toml"), NO_PROVIDER_CONFIG);
    let service = start_server(repo.path()).await;

    let info = service.peer_info().expect("服务端信息");
    assert!(info.capabilities.tools.is_some(), "必须声明 tools 能力");
    assert_eq!(
        info.server_info.as_ref().expect("serverInfo").name,
        "codesearch"
    );
    assert!(
        info.instructions
            .as_deref()
            .is_some_and(|text| text.contains("best_effort")),
        "instructions 应说明新鲜度语义"
    );

    let tools = service.list_all_tools().await.expect("tools/list");
    let mut names: Vec<&str> = tools.iter().map(|tool| tool.name.as_ref()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec!["read_code", "search_code", "search_context"],
        "新入口与旧入口必须同时可用"
    );

    for tool in &tools {
        let schema = serde_json::to_value(&tool.input_schema).expect("schema");
        let properties = schema["properties"].as_object().expect("properties");
        // workspace 范围由启动配置决定，工具入参不得包含任何根目录参数。
        assert!(
            !properties.contains_key("root")
                && !properties.contains_key("path")
                && !properties.contains_key("cwd"),
            "工具入参不得包含根目录参数，否则 Agent 可越界访问: {}",
            tool.name
        );
        assert!(
            tool.description
                .as_ref()
                .is_some_and(|text| !text.is_empty()),
            "工具必须有描述: {}",
            tool.name
        );
    }

    let search = tools
        .iter()
        .find(|tool| tool.name == "search_code")
        .expect("search_code");
    let schema = serde_json::to_value(&search.input_schema).expect("schema");
    let context = tools
        .iter()
        .find(|tool| tool.name == "search_context")
        .expect("search_context");
    assert_eq!(
        context.input_schema, search.input_schema,
        "两个入口参数必须完全一致"
    );
    assert_eq!(schema["required"], serde_json::json!(["query"]));
    let properties = schema["properties"].as_object().expect("properties");
    for key in [
        "query",
        "include",
        "exclude",
        "language",
        "max_results",
        "token_budget",
    ] {
        assert!(properties.contains_key(key), "search_code 缺少入参 `{key}`");
    }

    service.cancel().await.expect("关闭服务");
}

#[tokio::test(flavor = "multi_thread")]
/// 从新入口实际走 stdio 定位并读取，比较旧入口结果以验证不是空注册。
async fn search_context_and_read_code_round_trip_over_the_wire() {
    let repo = sample_repo();
    write_file(&repo.path().join("codesearch.toml"), NO_PROVIDER_CONFIG);
    let service = start_server(repo.path()).await;

    // ---- search_code ----
    let result = call(
        &service,
        "search_context",
        serde_json::json!({ "query": "RetryPolicy", "max_results": 5 }),
    )
    .await
    .expect("tools/call search_code");
    assert_ne!(result.is_error, Some(true), "搜索不应返回错误");

    let response = structured(&result);
    let legacy = call(
        &service,
        "search_code",
        serde_json::json!({ "query": "RetryPolicy", "max_results": 5 }),
    )
    .await
    .expect("旧入口仍可调用");
    assert_eq!(response["results"], structured(&legacy)["results"]);
    // 对外 JSON 字段必须稳定，Agent 依赖这些键。
    for key in [
        "catalog_epoch",
        "status",
        "results",
        "total_estimated_tokens",
        "token_budget",
        "estimated",
        "truncated",
    ] {
        assert!(
            response.get(key).is_some(),
            "响应缺少字段 `{key}`: {response}"
        );
    }
    assert_eq!(response["estimated"], true, "token 数必须标记为估算值");
    assert_eq!(response["status"]["freshness"], "best_effort");
    assert!(response["status"]["vector_search_used"].is_boolean());
    assert!(response["status"]["degraded"].is_boolean());
    assert!(response["catalog_epoch"].as_i64().is_some());

    let results = response["results"].as_array().expect("results 数组");
    assert!(!results.is_empty(), "应有检索结果");
    let first = &results[0];
    for key in [
        "code_ref",
        "path",
        "language",
        "file_hash",
        "start_line",
        "end_line",
        "matched_by",
        "content",
        "truncated",
    ] {
        assert!(first.get(key).is_some(), "结果缺少字段 `{key}`: {first}");
    }
    assert!(
        first["start_line"].as_i64().expect("行号") >= 1,
        "行号从 1 开始"
    );
    assert!(!first["content"].as_str().expect("content").is_empty());

    // ---- read_code ----
    let code_ref = first["code_ref"].as_str().expect("code_ref").to_string();
    let result = call(
        &service,
        "read_code",
        serde_json::json!({ "code_ref": code_ref, "expand": "containing_symbol" }),
    )
    .await
    .expect("tools/call read_code");
    assert_ne!(result.is_error, Some(true));

    let read = structured(&result);
    assert_eq!(read["path"], first["path"], "读取应回到同一文件");
    assert_eq!(read["file_hash"], first["file_hash"], "必须是同一版本");
    assert_eq!(read["is_current"], true);
    assert!(
        !read["content"].as_str().expect("content").is_empty(),
        "必须返回原始代码"
    );

    service.cancel().await.expect("关闭服务");
}

/// 新语言必须通过真实 MCP 参数校验、索引、符号定位和版本化读取，不仅是解析器单测。
#[tokio::test(flavor = "multi_thread")]
async fn new_language_filters_and_references_work_over_stdio() {
    let repo = sample_repo();
    write_file(&repo.path().join("codesearch.toml"), NO_PROVIDER_CONFIG);
    let cases = [
        (
            "cart.dart",
            "dart",
            "CartTotal.payable",
            include_str!("fixtures/multilingual/mobile/cart.dart"),
        ),
        (
            "session.ex",
            "elixir",
            "Portal.Session.valid?",
            include_str!("fixtures/multilingual/server/session.ex"),
        ),
        (
            "frame.zig",
            "zig",
            "ReceiveBuffer.reset",
            include_str!("fixtures/multilingual/native/frame.zig"),
        ),
        (
            "pricing.hs",
            "haskell",
            "shippingCost",
            include_str!("fixtures/multilingual/server/pricing.hs"),
        ),
        (
            "storage.tf",
            "hcl",
            "resource.aws_s3_bucket_versioning.archive",
            include_str!("fixtures/multilingual/infra/storage.tf"),
        ),
        (
            "orders.graphql",
            "graphql",
            "RecentOrders",
            include_str!("fixtures/multilingual/api/orders.graphql"),
        ),
        (
            "preview.html",
            "html",
            "releasePreview",
            include_str!("fixtures/multilingual/web/preview.html"),
        ),
    ];
    for (path, _, _, source) in cases {
        write_file(&repo.path().join(path), source);
    }
    let service = start_server(repo.path()).await;
    for (path, language, symbol, source) in cases {
        let result = call(
            &service,
            "search_context",
            serde_json::json!({"query": symbol, "language": [language]}),
        )
        .await
        .expect("新语言可调用");
        assert_ne!(result.is_error, Some(true));
        let response = structured(&result);
        let hits = response["results"].as_array().expect("结果列表");
        assert!(!hits.is_empty(), "{language} {symbol} 应能定位");
        assert!(hits
            .iter()
            .all(|hit| hit["language"] == language && hit["path"] == path));
        let first = &hits[0];
        // 小型类会作为完整代码块返回，展示符号是类名；子方法仍必须经符号通道命中且原文可见。
        assert!(
            first["matched_by"]
                .as_array()
                .unwrap()
                .iter()
                .any(|channel| channel == "symbol"),
            "{first}"
        );
        assert!(
            first["content"]
                .as_str()
                .unwrap()
                .contains(symbol.rsplit('.').next().unwrap()),
            "{first}"
        );
        let read = call(
            &service,
            "read_code",
            serde_json::json!({"code_ref": first["code_ref"]}),
        )
        .await
        .expect("读取新语言源码");
        let read = structured(&read);
        // 搜索展示可以省略最后的换行，版本化读取保留原始行结束符；正文与版本必须一致。
        assert_eq!(read["file_hash"], first["file_hash"]);
        assert_eq!(
            read["content"]
                .as_str()
                .unwrap()
                .trim_end_matches(['\r', '\n']),
            first["content"]
                .as_str()
                .unwrap()
                .trim_end_matches(['\r', '\n'])
        );
        assert!(source.contains(read["content"].as_str().unwrap().trim_end()));
    }
    service.cancel().await.expect("关闭服务");
}

#[tokio::test(flavor = "multi_thread")]
async fn scope_filters_work_through_the_protocol() {
    let repo = sample_repo();
    write_file(&repo.path().join("codesearch.toml"), NO_PROVIDER_CONFIG);
    let service = start_server(repo.path()).await;

    let result = call(
        &service,
        "search_code",
        serde_json::json!({
            "query": "retry",
            "language": ["typescript"],
            "exclude": ["**/legacy.js"]
        }),
    )
    .await
    .expect("tools/call");
    let response = structured(&result);
    let results = response["results"].as_array().expect("results");
    assert!(!results.is_empty(), "TypeScript 中应有 retry 相关代码");
    for item in results {
        let path = item["path"].as_str().expect("path");
        assert!(path.ends_with(".ts"), "语言过滤未生效: {path}");
    }

    // include 过滤同样必须生效。
    let result = call(
        &service,
        "search_code",
        serde_json::json!({ "query": "retry", "include": ["src/network/**"] }),
    )
    .await
    .expect("tools/call");
    let response = structured(&result);
    for item in response["results"].as_array().expect("results") {
        let path = item["path"].as_str().expect("path");
        assert!(path.starts_with("src/network/"), "include 未生效: {path}");
    }

    service.cancel().await.expect("关闭服务");
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_arguments_produce_structured_protocol_errors() {
    let repo = sample_repo();
    write_file(&repo.path().join("codesearch.toml"), NO_PROVIDER_CONFIG);
    let service = start_server(repo.path()).await;

    // 空查询：参数错误，且带稳定错误码。
    let error = call(
        &service,
        "search_code",
        serde_json::json!({ "query": "  " }),
    )
    .await
    .expect_err("空查询应报错");
    assert!(
        error.to_string().contains("invalid_query"),
        "错误应带稳定错误码: {error}"
    );

    // 非法 code_ref。
    let error = call(
        &service,
        "read_code",
        serde_json::json!({ "code_ref": "definitely-not-a-ref" }),
    )
    .await
    .expect_err("非法引用应报错");
    assert!(
        error.to_string().contains("invalid_reference"),
        "错误应带稳定错误码: {error}"
    );

    // 越界引用不能读到 workspace 之外。
    let error = call(
        &service,
        "read_code",
        serde_json::json!({ "code_ref": "cs1:../../etc/passwd:0000:1-2" }),
    )
    .await
    .expect_err("越界引用应报错");
    let text = error.to_string();
    assert!(!text.contains("root:"), "不得泄漏系统文件内容: {text}");

    // 未知语言过滤值。
    let error = call(
        &service,
        "search_code",
        serde_json::json!({ "query": "retry", "language": ["klingon"] }),
    )
    .await
    .expect_err("未知语言应报错");
    assert!(!error.to_string().is_empty());

    // 出错之后服务仍然可用。
    let result = call(
        &service,
        "search_code",
        serde_json::json!({ "query": "RetryPolicy" }),
    )
    .await
    .expect("错误之后应仍可正常调用");
    assert_ne!(result.is_error, Some(true));

    service.cancel().await.expect("关闭服务");
}

#[tokio::test(flavor = "multi_thread")]
async fn degradation_is_visible_in_the_tool_response() {
    let repo = sample_repo();
    write_file(&repo.path().join("codesearch.toml"), NO_PROVIDER_CONFIG);
    let service = start_server(repo.path()).await;

    let result = call(
        &service,
        "search_code",
        serde_json::json!({ "query": "重试策略" }),
    )
    .await
    .expect("tools/call");
    let response = structured(&result);
    assert_eq!(response["status"]["degraded"], true);
    assert_eq!(response["status"]["vector_search_used"], false);
    let reasons = response["status"]["reasons"].as_array().expect("reasons");
    assert!(
        !reasons.is_empty(),
        "降级原因必须出现在响应中，而不是只写进日志"
    );
    // 未配置 Provider 时仍应能通过符号与全文检索返回结果。
    assert!(!response["results"].as_array().expect("results").is_empty());

    service.cancel().await.expect("关闭服务");
}

/// stdout 必须只承载协议帧，日志一律走 stderr。
#[tokio::test(flavor = "multi_thread")]
async fn stdout_carries_only_protocol_frames() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let repo = sample_repo();
    write_file(&repo.path().join("codesearch.toml"), NO_PROVIDER_CONFIG);

    let mut child = Command::new(env!("CARGO_BIN_EXE_codesearch"))
        .arg("serve")
        .arg("--root")
        .arg(repo.path())
        .arg("--transport")
        .arg("stdio")
        .arg("--watch")
        .arg("false")
        // 日志级别由 CODESEARCH_LOG 控制；开到 trace 以确保确实有日志产生。
        .env("CODESEARCH_LOG", "trace")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("启动子进程");

    let mut stdin = child.stdin.take().expect("stdin");
    let mut reader = BufReader::new(child.stdout.take().expect("stdout")).lines();

    let initialize = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "wire-test", "version": "0" }
        }
    });
    stdin
        .write_all(format!("{initialize}\n").as_bytes())
        .await
        .expect("写入 initialize");
    stdin.flush().await.expect("flush");

    let mut frames: Vec<serde_json::Value> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while frames.len() < 2 {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let Ok(line) = tokio::time::timeout(remaining, reader.next_line()).await else {
            break;
        };
        match line.expect("读取 stdout") {
            Some(line) if line.trim().is_empty() => continue,
            Some(line) => {
                // 关键断言：stdout 上的每一行都必须是合法 JSON-RPC 帧。
                let frame: serde_json::Value =
                    serde_json::from_str(&line).unwrap_or_else(|error| {
                        panic!("stdout 混入了非协议内容: {error}\n原始行: {line}")
                    });
                assert_eq!(frame["jsonrpc"], "2.0", "必须是 JSON-RPC 2.0 帧: {line}");
                frames.push(frame);
                if frames.len() == 1 {
                    let notify = serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
                    let list = serde_json::json!({
                        "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
                    });
                    stdin
                        .write_all(format!("{notify}\n{list}\n").as_bytes())
                        .await
                        .expect("写入 tools/list");
                    stdin.flush().await.expect("flush");
                }
            }
            None => break,
        }
    }

    assert_eq!(frames.len(), 2, "应收到 initialize 与 tools/list 两帧");
    assert_eq!(frames[0]["result"]["serverInfo"]["name"], "codesearch");
    let tools = frames[1]["result"]["tools"].as_array().expect("tools 数组");
    assert_eq!(tools.len(), 3, "应有两个搜索入口与一个读取工具");

    // 确认日志确实写到了 stderr：否则上面的断言可能只是因为压根没有日志。
    drop(stdin);
    let stderr = child.stderr.take().expect("stderr");
    let _ = child.kill().await;
    let mut lines = BufReader::new(stderr).lines();
    let mut text = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        text.push_str(&line);
        text.push('\n');
        if text.len() > 8192 {
            break;
        }
    }
    assert!(
        !text.trim().is_empty(),
        "日志应写入 stderr；stderr 为空说明 stdout 纯净性未被真正验证"
    );
}

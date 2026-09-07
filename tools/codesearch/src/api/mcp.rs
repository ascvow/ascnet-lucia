//! MCP stdio 服务。
//!
//! 只向 Agent 暴露两个核心工具：`search_code` 与 `read_code`。
//!
//! # 输出通道
//!
//! stdout 只承载 MCP 协议内容。日志与诊断一律写 stderr，
//! 否则 tracing 输出会破坏 JSON-RPC 帧。
//!
//! # 作用域
//!
//! workspace 访问范围由启动配置确定，Agent 不能通过工具参数访问其他本地目录：
//! 工具入参只有 glob 与语言过滤，没有任何根目录参数。

use std::sync::Arc;

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::{Implementation, InitializeResult, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};

use crate::engine::Engine;
// 刻意不引入 crate 的 `Result` 别名：`#[tool_handler]` 展开后使用裸 `Result<T, E>`，
// 别名进入作用域会让宏生成的代码解析到错误的类型。
use crate::error::Error;
use crate::types::{Language, ReadExpand, ReadRequest, ScopeFilter, SearchMode, SearchRequest};

/// `search_code` 的输入。
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SearchCodeInput {
    /// 自然语言问题、符号名称、文件路径或错误消息。
    pub query: String,

    /// 仅搜索匹配这些 glob 的路径（相对 workspace 根）。
    #[serde(default)]
    pub include: Vec<String>,

    /// 排除匹配这些 glob 的路径。
    #[serde(default)]
    pub exclude: Vec<String>,

    /// 仅搜索这些语言：rust / typescript / tsx / javascript / markdown / text。
    #[serde(default)]
    pub language: Vec<String>,

    /// 返回结果数上限。
    #[serde(default)]
    pub max_results: Option<usize>,

    /// 估算 token 预算；返回内容会被裁剪到该预算内。
    #[serde(default)]
    pub token_budget: Option<usize>,
}

/// `read_code` 的输入。
#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReadCodeInput {
    /// 搜索结果给出的 `code_ref`，指向确定的源码版本。
    pub code_ref: String,

    /// 扩展方式：`none`（默认）、`containing_symbol`、`file`。
    #[serde(default)]
    pub expand: Option<String>,

    /// 估算 token 预算。
    #[serde(default)]
    pub token_budget: Option<usize>,
}

/// MCP 服务实现。
#[derive(Clone)]
pub struct CodesearchServer {
    engine: Arc<Engine>,
    tool_router: rmcp::handler::server::tool::ToolRouter<Self>,
}

impl std::fmt::Debug for CodesearchServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodesearchServer")
            .field("root", &self.engine.root())
            .finish_non_exhaustive()
    }
}

#[tool_router]
impl CodesearchServer {
    /// 绑定到一个已经打开的引擎。
    pub fn new(engine: Arc<Engine>) -> Self {
        Self {
            engine,
            tool_router: Self::tool_router(),
        }
    }

    /// 搜索代码。
    #[tool(
        name = "search_code",
        description = "在当前工作目录中检索代码。支持自然语言问题、符号名称、文件路径与错误消息。\
返回带文件路径、行号、内容哈希与 code_ref 的原始代码片段。\
结果中的 status 字段会说明索引是否处于降级状态（例如语义索引尚未追上或未配置 Embedding 服务）。"
    )]
    pub async fn search_code(
        &self,
        Parameters(input): Parameters<SearchCodeInput>,
    ) -> std::result::Result<Json<serde_json::Value>, ErrorData> {
        let mut languages = Vec::new();
        for value in &input.language {
            languages.push(Language::parse_filter(value).map_err(to_mcp_error)?);
        }
        let request = SearchRequest {
            query: input.query,
            scope: ScopeFilter {
                include: input.include,
                exclude: input.exclude,
                languages,
            },
            max_results: input.max_results,
            token_budget: input.token_budget,
            mode: SearchMode::Fast,
        };
        let response = self.engine.search(&request).await.map_err(to_mcp_error)?;
        let value = serde_json::to_value(&response)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(Json(value))
    }

    /// 按引用读取源码。
    #[tool(
        name = "read_code",
        description = "读取 code_ref 指向的那个源码版本。默认返回引用对应的范围，\
expand=containing_symbol 可扩展到包含它的函数或类型，expand=file 返回该版本整个文件。\
引用对应的版本已不存在时返回 reference_expired 错误，不会用最新文件冒充旧引用。"
    )]
    pub async fn read_code(
        &self,
        Parameters(input): Parameters<ReadCodeInput>,
    ) -> std::result::Result<Json<serde_json::Value>, ErrorData> {
        let expand =
            ReadExpand::parse(input.expand.as_deref().unwrap_or("none")).map_err(to_mcp_error)?;
        let request = ReadRequest {
            code_ref: input.code_ref,
            expand,
            token_budget: input.token_budget,
        };
        let response = self.engine.read(&request).map_err(to_mcp_error)?;
        let value = serde_json::to_value(&response)
            .map_err(|error| ErrorData::internal_error(error.to_string(), None))?;
        Ok(Json(value))
    }
}

// 显式指定 router：宏的默认表达式是 `Self::tool_router()`，那会在每次调用时重建路由表。
#[tool_handler(router = self.tool_router)]
impl ServerHandler for CodesearchServer {
    fn get_info(&self) -> ServerInfo {
        InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "codesearch 检索启动时指定的那个工作目录。\
先用 search_code 定位代码，再用返回的 code_ref 调用 read_code 获取完整上下文。\
所有结果都带文件路径、行号和内容哈希；token 统计均为估算值（estimated=true）。\
新鲜度语义为 best_effort：结果反映最近一次采集的状态，不等于磁盘此刻的绝对最新版本。",
            )
    }
}

/// 把内部错误映射为 MCP 错误，并带上稳定的错误码。
///
/// 参数问题与服务端问题区分开，便于 Agent 判断是否值得重试。
fn to_mcp_error(error: Error) -> ErrorData {
    let code = error.code();
    let message = format!("{code}: {error}");
    let data = Some(serde_json::json!({ "code": code }));
    match error {
        Error::InvalidQuery(_)
        | Error::InvalidReference(_)
        | Error::ReferenceExpired(_)
        | Error::PathOutOfScope(_)
        | Error::Config(_) => ErrorData::invalid_params(message, data),
        _ => ErrorData::internal_error(message, data),
    }
}

/// 在 stdio 上运行 MCP 服务，直到对端关闭连接。
///
/// `watch` 为真时同时启动文件监听，使索引跟随工作目录变化增量更新。
pub async fn serve_stdio(engine: Engine, watch: bool) -> crate::error::Result<()> {
    let engine = Arc::new(engine);
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);

    // 监听任务与 MCP 服务共享同一个引擎实例，因此共用一份写锁与索引状态。
    let watcher = if watch {
        let engine = Arc::clone(&engine);
        Some(tokio::spawn(async move {
            if let Err(error) = engine.watch_loop(cancel_rx).await {
                tracing::error!(error = %error, "文件监听循环退出");
            }
        }))
    } else {
        // 不监听时至少保证启动前索引一次，避免工具返回空结果。
        engine.index(false).await?;
        None
    };

    let server = CodesearchServer::new(Arc::clone(&engine));
    let service = server
        .serve(rmcp::transport::stdio())
        .await
        .map_err(|error| Error::Internal(format!("启动 MCP 服务失败: {error}")))?;
    tracing::info!(root = %engine.root().display(), "codesearch MCP 服务已就绪");

    let quit = service
        .waiting()
        .await
        .map_err(|error| Error::Internal(format!("MCP 服务异常退出: {error}")))?;
    tracing::info!(reason = ?quit, "MCP 服务已停止");

    let _ = cancel_tx.send(true);
    if let Some(watcher) = watcher {
        watcher.abort();
        let _ = watcher.await;
    }
    Ok(())
}

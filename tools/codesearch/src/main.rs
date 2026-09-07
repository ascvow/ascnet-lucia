//! codesearch 可执行入口。
//!
//! 日志固定写 stderr：`serve` 子命令的 stdout 是 MCP 协议通道，混入日志会破坏 JSON-RPC 帧。
//! 日志级别由 `CODESEARCH_LOG` 控制（默认 `warn`），默认不输出完整源码、完整查询或凭证。

use clap::Parser;
use codesearch::api::cli::{run, Cli};

#[tokio::main]
async fn main() -> std::process::ExitCode {
    init_tracing();
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            // 统一在这里输出错误码，便于脚本判断失败类型。
            eprintln!("codesearch 失败[{}]: {error}", error.code());
            std::process::ExitCode::FAILURE
        }
    }
}

/// 初始化日志。所有输出都走 stderr。
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter =
        EnvFilter::try_from_env("CODESEARCH_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    // 忽略重复初始化错误：库使用者可能已经装好了全局订阅者。
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

//! 命令行接口。
//!
//! 所有子命令都只调用 [`Engine`]，不包含任何检索规则。
//! `--json` 输出结构与 MCP 返回的字段一致，降级状态一定出现在 JSON 中而不是只写日志。

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::engine::{render_read, render_results, AccessMode, Engine, EngineOptions};
use crate::error::{Error, Result};
use crate::types::{Language, ReadExpand, ReadRequest, ScopeFilter, SearchMode, SearchRequest};

/// codesearch 命令行工具。
#[derive(Debug, Parser)]
#[command(
    name = "codesearch",
    version,
    about = "本地代码语义搜索：Tree-sitter 分块 + SQLite/FTS5 + USearch",
    long_about = None
)]
pub struct Cli {
    /// 子命令。
    #[command(subcommand)]
    pub command: Command,
}

/// 所有子命令共享的通用参数。
#[derive(Debug, Args, Clone)]
pub struct CommonArgs {
    /// workspace 根目录。
    #[arg(long, default_value = ".", global = true)]
    pub root: PathBuf,

    /// 配置文件路径，默认 `<root>/codesearch.toml`。
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,

    /// 索引数据目录，默认 `<root>/.codesearch`。
    #[arg(long, global = true)]
    pub data_dir: Option<PathBuf>,

    /// 离线模式：禁止任何 Embedding 网络调用。
    #[arg(long, global = true)]
    pub offline: bool,
}

impl CommonArgs {
    fn options(&self, mode: AccessMode) -> EngineOptions {
        EngineOptions {
            root: self.root.clone(),
            config_path: self.config.clone(),
            data_dir: self.data_dir.clone(),
            offline: self.offline,
            mode,
        }
    }
}

/// 作用域参数。
#[derive(Debug, Args, Clone, Default)]
pub struct ScopeArgs {
    /// 仅搜索匹配这些 glob 的路径，可重复。
    #[arg(long)]
    pub include: Vec<String>,

    /// 排除匹配这些 glob 的路径，可重复。
    #[arg(long)]
    pub exclude: Vec<String>,

    /// 仅搜索这些语言，可重复（rust/typescript/tsx/javascript/markdown/text）。
    #[arg(long = "language")]
    pub languages: Vec<String>,
}

impl ScopeArgs {
    fn to_filter(&self) -> Result<ScopeFilter> {
        let mut languages = Vec::new();
        for value in &self.languages {
            languages.push(Language::parse_filter(value)?);
        }
        Ok(ScopeFilter {
            include: self.include.clone(),
            exclude: self.exclude.clone(),
            languages,
        })
    }
}

/// 子命令。
#[derive(Debug, Subcommand)]
pub enum Command {
    /// 建立或更新索引。
    Index {
        /// 通用参数。
        #[command(flatten)]
        common: CommonArgs,

        /// 忽略大小与修改时间快速路径，对所有文件重新计算内容 Hash。
        #[arg(long)]
        force_rebuild: bool,

        /// 索引后持续监听文件变更（Ctrl-C 退出）。
        #[arg(long)]
        watch: bool,

        /// 以 JSON 输出。
        #[arg(long)]
        json: bool,
    },

    /// 搜索代码。
    Search {
        /// 通用参数。
        #[command(flatten)]
        common: CommonArgs,

        /// 查询：自然语言、符号名、路径或错误消息。
        ///
        /// `allow_hyphen_values` 是必需的：查询语法本身支持 `-path:`、`-lang:` 这类取反
        /// 前缀，用户也可能直接搜索 `--offline` 之类的字面量，不能被当作命令行参数解析。
        #[arg(long, short, allow_hyphen_values = true)]
        query: String,

        /// 作用域参数。
        #[command(flatten)]
        scope: ScopeArgs,

        /// 返回结果数上限。
        #[arg(long)]
        max_results: Option<usize>,

        /// 估算 token 预算。
        #[arg(long)]
        token_budget: Option<usize>,

        /// 以 JSON 输出。
        #[arg(long)]
        json: bool,
    },

    /// 按引用读取源码。
    Read {
        /// 通用参数。
        #[command(flatten)]
        common: CommonArgs,

        /// 搜索结果给出的 code_ref。
        #[arg(long = "ref")]
        code_ref: String,

        /// 扩展方式：none / containing_symbol / file。
        #[arg(long, default_value = "none")]
        expand: String,

        /// 估算 token 预算。
        #[arg(long)]
        token_budget: Option<usize>,

        /// 以 JSON 输出。
        #[arg(long)]
        json: bool,
    },

    /// 查看索引状态。
    Status {
        /// 通用参数。
        #[command(flatten)]
        common: CommonArgs,

        /// 以 JSON 输出。
        #[arg(long)]
        json: bool,
    },

    /// 启动 MCP 服务。
    Serve {
        /// 通用参数。
        #[command(flatten)]
        common: CommonArgs,

        /// 传输方式，目前只支持 stdio。
        #[arg(long, default_value = "stdio")]
        transport: String,

        /// 启动后持续监听文件变更并增量索引。
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        watch: bool,
    },
}

/// 执行命令行。
pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Index {
            common,
            force_rebuild,
            watch,
            json,
        } => {
            // 索引必须独占写锁：占用时给出明确的 workspace busy 错误，而不是静默降级。
            let engine = Engine::open(common.options(AccessMode::ReadWrite)).await?;
            if watch {
                let (_tx, rx) = tokio::sync::watch::channel(false);
                eprintln!("已开始监听 {}，按 Ctrl-C 退出。", engine.root().display());
                tokio::select! {
                    result = engine.watch_loop(rx) => result?,
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("收到中断信号，正在退出。");
                    }
                }
                return Ok(());
            }
            let report = engine.index(force_rebuild).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "目录索引完成：扫描 {} 个文件，更新 {}，未变化 {}，删除 {}，跳过 {}，耗时 {} ms",
                    report.scanned_files,
                    report.changed_files,
                    report.unchanged_files,
                    report.removed_files,
                    report.skipped.len(),
                    report.catalog_duration_ms
                );
                if report.rechunked {
                    println!("说明：分块器已升级，本次全量重建分块，后续恢复增量");
                }
                println!(
                    "当前代码块 {}，语义索引：新生成 {}，命中缓存 {}，等待 {}，失败 {}",
                    report.current_chunks,
                    report.embeddings_created,
                    report.embeddings_cached,
                    report.embeddings_pending,
                    report.embeddings_failed
                );
                if let Some(note) = &report.semantic_note {
                    println!("说明：{note}");
                }
            }
        }

        Command::Search {
            common,
            query,
            scope,
            max_results,
            token_budget,
            json,
        } => {
            let engine = Engine::open(common.options(AccessMode::Auto)).await?;
            let request = SearchRequest {
                query,
                scope: scope.to_filter()?,
                max_results,
                token_budget,
                mode: SearchMode::Fast,
            };
            let response = engine.search(&request).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print!("{}", render_results(&response));
            }
        }

        Command::Read {
            common,
            code_ref,
            expand,
            token_budget,
            json,
        } => {
            let engine = Engine::open(common.options(AccessMode::Auto)).await?;
            let request = ReadRequest {
                code_ref,
                expand: ReadExpand::parse(&expand)?,
                token_budget,
            };
            let response = engine.read(&request)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&response)?);
            } else {
                print!("{}", render_read(&response));
            }
        }

        Command::Status { common, json } => {
            let engine = Engine::open(common.options(AccessMode::Auto)).await?;
            let report = engine.status()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("workspace: {} ({})", report.root, report.workspace_id);
                println!("数据目录: {}", report.data_dir);
                println!(
                    "schema=v{} catalog_epoch={}",
                    report.schema_version, report.catalog_epoch
                );
                println!(
                    "文件: 活跃 {}，跳过 {}；当前代码块 {}",
                    report.active_files, report.skipped_files, report.current_chunks
                );
                println!(
                    "Embedding: 就绪 {}，等待 {}，失败 {}，缓存条目 {}",
                    report.embeddings_ready,
                    report.embeddings_pending,
                    report.embeddings_failed,
                    report.cache_entries
                );
                match &report.embedding_space {
                    Some(space) => println!(
                        "Embedding 空间: {} model={} dim={} metric={} 模板 v{}",
                        space.id,
                        space.model,
                        space.dimensions,
                        space.metric,
                        space.template_version
                    ),
                    None => println!("Embedding 空间: 未配置（仅符号与全文检索）"),
                }
                println!(
                    "向量: 数量 {}，内存序号 {}，持久化序号 {}，待应用 {}",
                    report.vector_count,
                    report.vector_applied_seq,
                    report.vector_durable_seq,
                    report.vector_ops_backlog
                );
                if let Some(scan) = &report.last_scan {
                    println!(
                        "最近扫描: 扫描 {}，更新 {}，删除 {}，跳过 {}，耗时 {} ms",
                        scan.scanned, scan.changed, scan.removed, scan.skipped, scan.duration_ms
                    );
                }
                println!("degraded={}", report.degraded);
                for note in &report.diagnostics {
                    println!("  - {note}");
                }
            }
        }

        Command::Serve {
            common,
            transport,
            watch,
        } => {
            if transport != "stdio" {
                return Err(Error::Config(format!(
                    "不支持的 transport `{transport}`；第一版只支持 stdio"
                )));
            }
            // MCP 服务需要独占写锁才能增量索引。
            let engine = Engine::open(common.options(AccessMode::ReadWrite)).await?;
            crate::api::mcp::serve_stdio(engine, watch).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 查询语法自带 `-path:` 取反前缀，命令行解析不能把它当成参数。
    ///
    /// 曾出现 `--query '-path:**/tests/** RetryPolicy'` 报
    /// `unexpected argument '-p' found`，使文档中的取反语法在 CLI 上无法使用。
    #[test]
    fn query_accepts_leading_hyphen() {
        for raw in [
            "-path:**/tests/** RetryPolicy",
            "-lang:markdown 重试",
            "--force-rebuild",
        ] {
            let cli = Cli::try_parse_from(["codesearch", "search", "--query", raw])
                .unwrap_or_else(|error| panic!("查询 `{raw}` 应可解析: {error}"));
            let Command::Search { query, .. } = cli.command else {
                panic!("应解析为 search 子命令");
            };
            assert_eq!(query, raw, "查询文本必须原样传入");
        }
    }
}

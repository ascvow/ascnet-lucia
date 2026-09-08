//! 通过真实 MCP stdio 测量既有项目；只发送冻结查询，不把人工答案、语言或路径提示传给搜索。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use codesearch::types::{ReadResponse, SearchResponse};
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::process::Command;

/// 预检输出，只消费定位项目和缓存所需的字段，不读取任何明文凭证。
#[derive(Deserialize)]
struct Manifest {
    binary: PathBuf,
    projects: Vec<Project>,
}

/// 实验阶段和单项目重测开关；重测只换报告标签，不更改冻结查询或真值。
#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    supplement_php: bool,
    #[arg(long)]
    project: Option<String>,
    #[arg(long, default_value = "verified")]
    label: String,
}

/// 一个只读源码根及已准备好的独立索引。
#[derive(Clone, Deserialize)]
struct Project {
    id: String,
    root: PathBuf,
    config: PathBuf,
    data_dir: PathBuf,
    index: Option<Value>,
}

/// 冻结的查询集合。
#[derive(Deserialize)]
struct QuerySet {
    projects: Vec<QueryProject>,
}

/// 查询属于哪个实际项目。
#[derive(Deserialize)]
struct QueryProject {
    id: String,
    queries: Vec<Case>,
}

/// 原始查询，不携带搜索提示。
#[derive(Deserialize)]
struct Case {
    id: String,
    query: String,
}

/// 真值只用于评分，未实现或不确定的前提单独标注，不算有效实现命中。
#[derive(Deserialize)]
struct Truth {
    kind: String,
    note: String,
    locations: Vec<Location>,
}

/// 需要同时出现在返回内容及实际行范围内的实现语句，允许多个合理实现位置。
#[derive(Deserialize)]
struct Location {
    path: String,
    start_line: u32,
    end_line: u32,
    needles: Vec<String>,
}

/// 校验 MCP 结构化响应，不接受错误响应或只有展示文本的伪成功。
fn structured(result: CallToolResult) -> Result<Value, Box<dyn std::error::Error>> {
    if result.is_error == Some(true) {
        return Err("MCP 工具返回错误".into());
    }
    result
        .structured_content
        .ok_or_else(|| "缺少 MCP 结构化内容".into())
}

/// 对一条原始查询测量默认参数下的定位，并逐项验证 code_ref 能读回同版本原文。
async fn observe(
    service: &rmcp::service::RunningService<RoleClient, ()>,
    root: &Path,
    case: &Case,
    truth: &Truth,
) -> Result<Value, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let arguments = json!({"query": case.query}).as_object().unwrap().clone();
    let reply = tokio::time::timeout(
        Duration::from_secs(180),
        service.call_tool(CallToolRequestParams::new("search_context").with_arguments(arguments)),
    )
    .await??;
    let latency_ms = started.elapsed().as_secs_f64() * 1000.0;
    let raw = structured(reply)?;
    let response: SearchResponse = serde_json::from_value(raw.clone())?;
    if response.status.degraded || !response.status.vector_search_used {
        return Err("查询降级或未实际执行向量召回".into());
    }
    if response.token_budget != 6000 || response.results.len() > 10 {
        return Err("实际请求未使用预期的默认预算".into());
    }
    let mut file_rank = None;
    let mut implementation_rank = None;
    let mut reference_line_expansions = 0;
    let root = root.canonicalize()?;
    for (i, hit) in response.results.iter().enumerate() {
        let path = root.join(&hit.path).canonicalize()?;
        if !path.starts_with(&root) {
            return Err("结果路径越界".into());
        }
        let source = std::fs::read_to_string(path)?;
        if blake3::hash(source.as_bytes()).to_hex().as_str() != hit.file_hash {
            return Err(format!("源码在评测期间变化: {}", hit.path).into());
        }
        let lines: Vec<&str> = source.lines().collect();
        if hit.start_line == 0
            || hit.end_line < hit.start_line
            || hit.end_line as usize > lines.len()
        {
            return Err(format!("返回行号越界: {}", hit.path).into());
        }
        let span = lines[hit.start_line as usize - 1..hit.end_line as usize].join("\n");
        for target in &truth.locations {
            if target.path != hit.path {
                continue;
            }
            file_rank.get_or_insert(i + 1);
            if hit.start_line <= target.start_line
                && hit.end_line >= target.end_line
                && target
                    .needles
                    .iter()
                    .all(|needle| hit.content.contains(needle) && span.contains(needle))
            {
                implementation_rank.get_or_insert(i + 1);
            }
        }
        let read = service
            .call_tool(
                CallToolRequestParams::new("read_code").with_arguments(
                    json!({"code_ref":hit.code_ref})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await?;
        let read: ReadResponse = serde_json::from_value(structured(read)?)?;
        // 搜索以字节边界截取，引用以整行寻址；读取可多出首尾行的邻接字符，但不能换版本或丢内容。
        let read_text = read.content.replace("\r\n", "\n");
        let search_text = hit.content.replace("\r\n", "\n");
        if read.path != hit.path
            || read.file_hash != hit.file_hash
            || read.start_line != hit.start_line
            || read.end_line != hit.end_line
            || read_text.trim_end_matches('\n') != span.trim_end_matches('\n')
            || !read_text.contains(search_text.trim_end_matches('\n'))
        {
            return Err(format!("引用不能读回同版本内容: {}", hit.path).into());
        }
        if read_text.trim_end_matches('\n') != search_text.trim_end_matches('\n') {
            reference_line_expansions += 1;
        }
    }
    Ok(
        json!({"latency_ms":latency_ms,"file_rank":file_rank,"implementation_rank":implementation_rank,
        "references_verified":response.results.len(),"reference_line_expansions":reference_line_expansions,"response":raw}),
    )
}

/// 打开已经建立的真实索引；协议层用官方 rmcp 客户端，不自行拼装传输协议。
async fn run_project(
    binary: &Path,
    project: &Project,
    queries: &[Case],
    truth: &HashMap<String, Truth>,
    destination: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if project
        .index
        .as_ref()
        .is_none_or(|index| index["degraded"] != false)
    {
        return Err(format!("{} 的索引尚未就绪", project.id).into());
    }
    let index_complete = project
        .index
        .as_ref()
        .is_some_and(|index| index["embeddings_pending"] == 0 && index["embeddings_failed"] == 0);
    let mut command = Command::new(binary);
    command
        .arg("serve")
        .arg("--root")
        .arg(&project.root)
        .arg("--config")
        .arg(&project.config)
        .arg("--data-dir")
        .arg(&project.data_dir)
        .arg("--watch")
        .arg("false")
        .env("CODESEARCH_LOG", "error");
    let process = rmcp::transport::TokioChildProcess::new(command)?;
    let service = ().serve(process).await?;
    let mut rows = Vec::new();
    for round in 0..3 {
        for case in queries {
            let answer = &truth[&case.id];
            let result = observe(&service, &project.root, case, answer).await;
            let observation = match result {
                Ok(value) => value,
                Err(error) => json!({"error":error.to_string()}),
            };
            println!(
                "{} 第{}轮 {} 文件={:?} 实现={:?} 错误={:?}",
                project.id,
                round + 1,
                case.id,
                observation["file_rank"],
                observation["implementation_rank"],
                observation.get("error")
            );
            rows.push(json!({"id":case.id,"query":case.query,"round":round + 1,"index_complete":index_complete,
                "kind":answer.kind,"note":answer.note,"observation":observation}));
            std::fs::write(
                destination,
                serde_json::to_vec_pretty(
                    &json!({"project":project.id,"rows":rows,"complete":false}),
                )?,
            )?;
        }
    }
    service.cancel().await?;
    std::fs::write(
        destination,
        serde_json::to_vec_pretty(
            &json!({"project":project.id,"index_complete":index_complete,"index":project.index,"rows":rows,"complete":true}),
        )?,
    )?;
    Ok(())
}

/// 真值必须在搜索前完整验证；启动评测后不根据搜索结果修改标注。
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out = base.join("target/real-projects-2026-09-08");
    let manifest: Manifest = serde_json::from_slice(&std::fs::read(out.join("manifest.json"))?)?;
    let set: QuerySet = serde_json::from_slice(&std::fs::read(
        base.join("eval/real-projects-queries.json"),
    )?)?;
    let truth: HashMap<String, Truth> =
        serde_json::from_slice(&std::fs::read(base.join("eval/real-projects-truth.json"))?)?;
    let arguments = Arguments::parse();
    let supplement = arguments.supplement_php;
    if arguments.label.is_empty()
        || !arguments
            .label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err("报告标签只能包含字母、数字和短横线".into());
    }
    for project in &manifest.projects {
        let queries = &set
            .projects
            .iter()
            .find(|candidate| candidate.id == project.id)
            .ok_or("缺少项目查询")?
            .queries;
        for case in queries {
            let answer = truth
                .get(&case.id)
                .ok_or("查询缺少真值或明确的未实现标注")?;
            for location in &answer.locations {
                let source = std::fs::read_to_string(project.root.join(&location.path))?;
                let lines: Vec<&str> = source.lines().collect();
                let span = if location.start_line > 0
                    && location.end_line >= location.start_line
                    && location.end_line as usize <= lines.len()
                {
                    lines[location.start_line as usize - 1..location.end_line as usize].join("\n")
                } else {
                    String::new()
                };
                if location.needles.is_empty()
                    || !location.needles.iter().all(|needle| span.contains(needle))
                {
                    return Err(format!("真值失效: {} {}", case.id, location.path).into());
                }
            }
        }
    }
    for project in &manifest.projects {
        if arguments
            .project
            .as_ref()
            .is_some_and(|id| id != &project.id)
        {
            continue;
        }
        if supplement && project.id != "1086" {
            continue;
        }
        let mut project = project.clone();
        // 补测只适配模型输入限制，保留原配置、原索引和默认报告，不调整查询或答案。
        if supplement {
            let mut config: codesearch::config::Config =
                toml::from_str(&std::fs::read_to_string(&project.config)?)?;
            config.embedding.max_input_chars = 6000;
            project.config = project.config.with_file_name("codesearch-input6000.toml");
            project.data_dir = project.data_dir.with_file_name("index-input6000");
            std::fs::write(&project.config, toml::to_string(&config)?)?;
            let output = Command::new(&manifest.binary)
                .arg("index")
                .arg("--root")
                .arg(&project.root)
                .arg("--config")
                .arg(&project.config)
                .arg("--data-dir")
                .arg(&project.data_dir)
                .arg("--json")
                .output()
                .await?;
            if !output.status.success() {
                return Err("PHP 补测索引失败".into());
            }
            project.index = Some(serde_json::from_slice(&output.stdout)?);
            println!("PHP 输入上限补测索引: {}", project.index.as_ref().unwrap());
        }
        let queries = &set
            .projects
            .iter()
            .find(|candidate| candidate.id == project.id)
            .unwrap()
            .queries;
        run_project(
            &manifest.binary,
            &project,
            queries,
            &truth,
            &out.join(format!(
                "{}{}-results.json",
                project.id,
                if supplement {
                    "-input6000".to_string()
                } else {
                    format!("-{}", arguments.label)
                }
            )),
        )
        .await?;
    }
    Ok(())
}

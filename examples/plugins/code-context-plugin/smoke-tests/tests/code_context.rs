//! Code Context Engine 插件的真实 WASM Host 加载测试。
//!
//! 这些测试走完整路径：编译产物 → Host 加载 → manifest 能力校验 → 激活 → 工具注册 →
//! 工具调用 → 卸载。它们验证的是插件在**真实宿主**下的行为，`cargo check --target
//! wasm32-wasip2` 与插件内单元测试都不能替代。

use agent_core::AgentExtension;
use agent_plugin_host::{wasm::load_wasm_plugins_with_services, PluginHost, PluginHostServices};
use agent_tool::{ToolCall, ToolPermissionScope};
use anyhow::Result;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// 插件 ID，与 `plugin.toml` 保持一致。
const PLUGIN_ID: &str = "code-context";
/// 工具在模型侧的公开名。
const TOOL_NAME: &str = "code_search";
/// 插件发布状态的扩展事件名。
const STATUS_EVENT: &str = "code-context.status";

/// 返回插件 manifest 路径。
fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml")
}

/// 创建一个进程内唯一的临时目录路径。
fn unique_state_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "lucia-code-context-smoke-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时间应晚于 Unix epoch")
            .as_nanos()
    ))
}

/// 创建一个真实存在且规范化的临时工作区，供 `workspace_read` 能力校验使用。
fn create_workspace(label: &str) -> PathBuf {
    let root = unique_state_root(&format!("workspace-{label}"));
    std::fs::create_dir_all(&root).expect("创建临时工作区");
    root.canonicalize().expect("规范化临时工作区")
}

/// 在工作区中写入一个文件，自动创建父目录。
fn write_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("创建父目录");
    }
    std::fs::write(path, content).expect("写入文件");
}

/// 按宿主真实注入方式装配服务：持久状态目录、项目作用域与可信工作区根。
fn services(
    state_root: &Path,
    workspace_scope: &str,
    workspace_root: &Path,
) -> Result<PluginHostServices> {
    let workspace_root_text = workspace_root.to_string_lossy().into_owned();
    PluginHostServices::new()
        .with_persistent_state_dir(state_root.to_path_buf())?
        .with_workspace_scope(workspace_scope)?
        .with_workspace_root(workspace_root.to_path_buf())?
        .with_activation_metadata(
            PLUGIN_ID,
            HashMap::from([
                ("workspace_root".to_string(), workspace_root_text),
                ("workspace_scope".to_string(), workspace_scope.to_string()),
            ]),
        )
}

/// 加载插件并返回激活期发布的状态事件负载。
async fn activate_and_read_status(
    state_root: &Path,
    workspace_scope: &str,
    workspace_root: &Path,
) -> Result<Value> {
    let host = load_wasm_plugins_with_services(
        &[manifest_path()],
        services(state_root, workspace_scope, workspace_root)?,
    )
    .await?;
    let events = AgentExtension::drain_events(&host).await?;
    let status = events
        .iter()
        .find(|event| event["source"]["id"] == PLUGIN_ID && event["name"] == STATUS_EVENT)
        .cloned()
        .expect("激活必须发布一条状态事件");
    host.shutdown().await?;
    Ok(status["data"].clone())
}

/// 插件必须能被真实 Host 加载、完成激活并注册 code_search。
#[tokio::test]
async fn wasm_host_loads_and_registers_code_search() {
    let state_root = unique_state_root("load");
    let workspace_root = create_workspace("load");
    let host = load_wasm_plugins_with_services(
        &[manifest_path()],
        services(&state_root, "scope-load", &workspace_root).expect("装配宿主服务"),
    )
    .await
    .expect("真实 Host 应能加载 Code Context 插件");

    let tools = AgentExtension::list_tools(&host)
        .await
        .expect("读取插件工具");
    let tool = tools
        .iter()
        .find(|tool| tool.name == TOOL_NAME)
        .expect("激活后必须注册 code_search");

    // scope 由 Host 按 manifest 绑定；这里断言的是最终生效值，而不是 Guest 自报值。
    //
    // manifest 声明的是 read，但插件同时持有 workspace_read（贡献 read）与 http
    // （贡献 network）两项能力，两个不相等的 scope 合并后落到 unknown。这不是配置错误，
    // 而是「启用 API 语义检索必然抬高工具审批档位」的直接体现，见 plugin.toml 的说明。
    // 删掉 manifest 的 [capabilities.http] 段后该断言应改回 Read。
    assert_eq!(tool.permission_scope, ToolPermissionScope::Unknown);
    let schema = serde_json::to_string(&tool.input_schema).expect("序列化工具 schema");
    assert!(schema.contains("query"), "{schema}");
    assert!(schema.contains("max_results"), "{schema}");
    assert!(schema.contains("token_budget"), "{schema}");
    assert!(schema.contains("path_scope"), "{schema}");

    host.shutdown().await.expect("卸载插件");
    let tools_after_shutdown = AgentExtension::list_tools(&host)
        .await
        .expect("卸载后读取工具");
    assert!(
        tools_after_shutdown
            .iter()
            .all(|tool| tool.name != TOOL_NAME),
        "卸载后必须注销动态工具"
    );
    let _ = std::fs::remove_dir_all(&state_root);
    let _ = std::fs::remove_dir_all(&workspace_root);
}

/// 空工作区索引完成后，code_search 必须返回成功的空结果与真实降级状态。
///
/// 空工作区的索引会立刻建成，因此状态是 `degraded`（结构索引就绪、语义分支缺席）。
/// 此时检索链路本身已经成功执行，空列表表达仓库确实没有匹配内容，不应再沿用 Phase 1
/// “检索尚未实现”的错误协议。
#[tokio::test]
async fn code_search_returns_successful_empty_results_for_empty_workspace() {
    let state_root = unique_state_root("search");
    let workspace_root = create_workspace("search");
    let host = load_wasm_plugins_with_services(
        &[manifest_path()],
        services(&state_root, "scope-search", &workspace_root).expect("装配宿主服务"),
    )
    .await
    .expect("加载 Code Context 插件");

    // 组合宿主的工具路由是 list_tools 的副作用；先列出工具才能复现 Core 在运行开始时
    // 冻结工具集、随后按名路由调用的真实顺序。
    AgentExtension::list_tools(&host)
        .await
        .expect("建立插件工具路由");

    let result = AgentExtension::call_tool(
        &host,
        ToolCall::new(
            "call-1",
            TOOL_NAME,
            json!({"query": "where are shell permissions checked?"}),
        ),
    )
    .await
    .expect("工具调用不应失败")
    .expect("code_search 应由本插件处理");

    assert!(!result.is_error, "索引完成后的空结果应是成功结果");
    assert_eq!(
        result.content["status"], "degraded",
        "实际 {}",
        result.content
    );
    assert_eq!(result.content["results"], json!([]));
    let details = result.details.expect("成功结果应带结构化状态");
    assert_eq!(details["status"], "degraded", "实际 {details}");
    // 空工作区没有任何可索引文件，索引规模必须如实为 0，而不是缺失或伪造。
    assert_eq!(details["index"]["files"], 0, "实际 {details}");

    host.shutdown().await.expect("卸载插件");
    let _ = std::fs::remove_dir_all(&state_root);
    let _ = std::fs::remove_dir_all(&workspace_root);
}

/// 真实 Host 上必须能递归扫描子目录、解析源码并建成结构索引。
///
/// 这是扫描器唯一一处依赖宿主契约的地方：`list_dir` 对**子目录**返回的是完整的仓库相对
/// 路径（而不是目录内的短名），且按路径排序。插件内单元测试用的是替身宿主，证明不了这一点；
/// 一旦该契约与假设不符，整个索引会只剩仓库根一层。
#[tokio::test]
async fn wasm_host_scans_nested_directories_and_builds_index() {
    let state_root = unique_state_root("index");
    let workspace_root = create_workspace("index");

    // 三层嵌套 + 多语言 + 一个必须被忽略的依赖目录。
    write_file(&workspace_root, "src/lib.rs", "pub fn top_level() {}\n");
    write_file(
        &workspace_root,
        "src/nested/manager.rs",
        "pub struct PermissionManager;\nimpl PermissionManager {\n    pub fn check(&self) -> bool { true }\n}\n",
    );
    write_file(
        &workspace_root,
        "src/nested/deep/service.ts",
        "export class Service {\n  run(): void {}\n}\n",
    );
    write_file(&workspace_root, "app/main.py", "def entry():\n    pass\n");
    write_file(&workspace_root, "README.md", "# 项目\n\n说明。\n");
    write_file(&workspace_root, ".gitignore", "ignored.rs\n");
    write_file(&workspace_root, "ignored.rs", "pub fn ignored() {}\n");
    write_file(
        &workspace_root,
        "node_modules/pkg/index.js",
        "module.exports = 1;\n",
    );

    let host = load_wasm_plugins_with_services(
        &[manifest_path()],
        services(&state_root, "scope-index", &workspace_root).expect("装配宿主服务"),
    )
    .await
    .expect("加载 Code Context 插件");

    AgentExtension::list_tools(&host)
        .await
        .expect("建立插件工具路由");

    // 索引按 fuel 预算分片推进，因此可能需要多次调用才能建成。
    let mut details = Value::Null;
    for _ in 0..64 {
        let result = AgentExtension::call_tool(
            &host,
            ToolCall::new("call-1", TOOL_NAME, json!({"query": "permission check"})),
        )
        .await
        .expect("工具调用不应失败")
        .expect("code_search 应由本插件处理");
        details = result.details.expect("必须带结构化状态");
        if details["status"] != "indexing" {
            break;
        }
    }

    assert_eq!(
        details["status"], "degraded",
        "结构索引建成后应为 degraded（语义分支缺席），实际 {details}"
    );
    // 5 个源文件：src/lib.rs、src/nested/manager.rs、src/nested/deep/service.ts、
    // app/main.py、README.md。ignored.rs 被 .gitignore 排除，node_modules 被默认规则排除，
    // .gitignore 自身扩展名不在白名单内。
    assert_eq!(
        details["index"]["files"], 5,
        "必须扫到三层嵌套下的全部源文件并正确应用忽略规则，实际 {details}"
    );
    assert!(
        details["index"]["symbols"].as_u64().unwrap_or(0) >= 5,
        "多语言符号必须被解析出来，实际 {details}"
    );
    assert!(
        details["index"]["chunks"].as_u64().unwrap_or(0) >= 5,
        "实际 {details}"
    );

    host.shutdown().await.expect("卸载插件");
    let _ = std::fs::remove_dir_all(&state_root);
    let _ = std::fs::remove_dir_all(&workspace_root);
}

/// 参数非法必须与索引缺失区分开，且不得让 Host 收到异常。
#[tokio::test]
async fn code_search_rejects_blank_query() {
    let state_root = unique_state_root("args");
    let workspace_root = create_workspace("args");
    let host = load_wasm_plugins_with_services(
        &[manifest_path()],
        services(&state_root, "scope-args", &workspace_root).expect("装配宿主服务"),
    )
    .await
    .expect("加载 Code Context 插件");

    AgentExtension::list_tools(&host)
        .await
        .expect("建立插件工具路由");

    let result = AgentExtension::call_tool(
        &host,
        ToolCall::new("call-1", TOOL_NAME, json!({"query": "   "})),
    )
    .await
    .expect("参数错误不应导致 Host 调用失败")
    .expect("code_search 应由本插件处理");

    assert!(result.is_error);
    assert!(
        result.details.is_none(),
        "参数错误不应携带索引状态细节，否则无法与索引缺失区分"
    );

    host.shutdown().await.expect("卸载插件");
    let _ = std::fs::remove_dir_all(&state_root);
    let _ = std::fs::remove_dir_all(&workspace_root);
}

/// 索引 manifest 必须跨插件卸载与重新加载保留。
///
/// 换用不同工作区根后必须判定为需要全量重建：这同时证明旧 manifest 被真实读回并参与
/// 了比较——只断言状态不变无法区分「读回了」与「什么都没读到」。
#[tokio::test]
async fn manifest_persists_across_plugin_reload() {
    let state_root = unique_state_root("persist");
    let first_workspace = create_workspace("persist-first");
    let second_workspace = create_workspace("persist-second");

    let first = activate_and_read_status(&state_root, "scope-persist", &first_workspace)
        .await
        .expect("首次激活");
    assert_eq!(first["status"], "not_indexed");
    let identity = first["repository_identity"]
        .as_str()
        .expect("首次激活应建立仓库身份")
        .to_string();

    let reopened = activate_and_read_status(&state_root, "scope-persist", &first_workspace)
        .await
        .expect("重新加载同一仓库");
    assert_eq!(
        reopened["repository_identity"].as_str(),
        Some(identity.as_str()),
        "同一仓库重新加载必须得到同一身份"
    );
    assert_eq!(reopened["status"], "not_indexed");

    let switched = activate_and_read_status(&state_root, "scope-persist", &second_workspace)
        .await
        .expect("在同一存储作用域下换仓库");
    assert_eq!(
        switched["status"], "needs_full_rebuild",
        "换仓库必须触发全量重建，这证明旧 manifest 被真实读回并参与了比较"
    );

    let _ = std::fs::remove_dir_all(&state_root);
    let _ = std::fs::remove_dir_all(&first_workspace);
    let _ = std::fs::remove_dir_all(&second_workspace);
}

/// 不同项目作用域的索引状态必须互不污染。
#[tokio::test]
async fn repositories_are_isolated_by_workspace_scope() {
    let state_root = unique_state_root("isolation");
    let workspace_a = create_workspace("isolation-a");
    let workspace_b = create_workspace("isolation-b");

    let first = activate_and_read_status(&state_root, "scope-a", &workspace_a)
        .await
        .expect("激活仓库 A");
    assert_eq!(first["status"], "not_indexed");

    // 作用域 B 从未写过 manifest：如果它读到了 A 的数据，这里会变成 needs_full_rebuild。
    let second = activate_and_read_status(&state_root, "scope-b", &workspace_b)
        .await
        .expect("激活仓库 B");
    assert_eq!(
        second["status"], "not_indexed",
        "仓库 B 不得读到仓库 A 的索引 manifest"
    );
    assert_ne!(
        first["repository_identity"], second["repository_identity"],
        "不同仓库必须得到不同身份"
    );

    let _ = std::fs::remove_dir_all(&state_root);
    let _ = std::fs::remove_dir_all(&workspace_a);
    let _ = std::fs::remove_dir_all(&workspace_b);
}

/// 宿主未提供项目作用域时插件仍须加载，并如实报告未索引。
#[tokio::test]
async fn plugin_loads_without_workspace_scope() {
    let state_root = unique_state_root("no-scope");
    let workspace_root = create_workspace("no-scope");
    let workspace_root_text = workspace_root.to_string_lossy().into_owned();
    let services = PluginHostServices::new()
        .with_persistent_state_dir(state_root.clone())
        .expect("注入持久状态目录")
        .with_workspace_root(workspace_root.clone())
        .expect("注入可信工作区根")
        .with_activation_metadata(
            PLUGIN_ID,
            HashMap::from([("workspace_root".to_string(), workspace_root_text)]),
        )
        .expect("注入激活元数据");

    let host = load_wasm_plugins_with_services(&[manifest_path()], services)
        .await
        .expect("缺少项目作用域不应阻止插件加载");

    let events = AgentExtension::drain_events(&host).await.expect("读取事件");
    let status = events
        .iter()
        .find(|event| event["name"] == STATUS_EVENT)
        .expect("应发布状态事件");
    assert_eq!(status["data"]["status"], "not_indexed");

    host.shutdown().await.expect("卸载插件");
    let _ = std::fs::remove_dir_all(&state_root);
    let _ = std::fs::remove_dir_all(&workspace_root);
}

/// 宿主未注入工作区身份时必须优雅降级：插件加载成功、不注册工具、给出明确原因。
#[tokio::test]
async fn missing_workspace_root_degrades_without_registering_tool() {
    let state_root = unique_state_root("no-root");
    let services = PluginHostServices::new()
        .with_persistent_state_dir(state_root.clone())
        .expect("注入持久状态目录")
        .with_workspace_scope("scope-no-root")
        .expect("注入项目作用域");

    let host = load_wasm_plugins_with_services(&[manifest_path()], services)
        .await
        .expect("缺少工作区身份不应阻止插件加载");

    let tools = AgentExtension::list_tools(&host).await.expect("读取工具");
    assert!(
        tools.iter().all(|tool| tool.name != TOOL_NAME),
        "引擎不可用时不应把 code_search 暴露给模型"
    );

    let events = AgentExtension::drain_events(&host).await.expect("读取事件");
    let status = events
        .iter()
        .find(|event| event["name"] == STATUS_EVENT)
        .expect("应发布状态事件");
    assert_eq!(status["data"]["status"], "error");
    assert!(
        status["data"]["reason"].is_string(),
        "不可用时必须给出可读原因"
    );

    host.shutdown().await.expect("卸载插件");
    let _ = std::fs::remove_dir_all(&state_root);
}

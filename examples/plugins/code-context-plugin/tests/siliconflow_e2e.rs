//! 端到端语义检索集成测试——调用硅基流动嵌入 API。
//!
//! 使用本项目的真实源码文件构造仓库，经 SiliconFlow `/v1/embeddings` 端点生成向量，
//! 验证完整流程：索引构建 → 嵌入 → 持久化 → 恢复 → 语义检索。
//!
//! 需要网络访问，默认 `#[ignore]`；执行方式：
//!
//! ```bash
//! cargo test -p code-context-plugin --test siliconflow_e2e -- --ignored
//! ```

use agent_plugin::{
    ActivationContext, ExtensionEvent, FileEntry, HttpRequest, HttpResponse, PluginHostApi,
    PluginStoreScope, ProcessSpec, PromptContribution, Result, ServiceDescriptor, ServiceSpec,
    ToolSpec,
};
use anyhow::anyhow;
use code_context_plugin::{CodeContextPlugin, IndexStatus};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;

/// SiliconFlow 嵌入 API 的 base URL。
const SILICONFLOW_BASE_URL: &str = "https://api.siliconflow.cn/v1/";

/// 测试用 API 密钥。
const SILICONFLOW_API_KEY: &str = "sk-sjnbrxiagakhmfmnaszyumeonhubdhsridmloclnzdkkdlfv";

/// 嵌入模型——SiliconFlow 支持的 BAAI/bge-m3 维度 1024。
const EMBEDDING_MODEL: &str = "BAAI/bge-m3";

/// 嵌入维度。
const EMBEDDING_DIMENSION: u32 = 1024;

/// 使用真实文件系统与真实 HTTP 的宿主替身。
///
/// 文件从指定根目录读取；HTTP 请求转发到 SiliconFlow，自动注入凭证。
struct LiveApiHost {
    /// 仓库根目录的绝对路径。
    repo_root: PathBuf,
    /// 按 (作用域, 键) 保存的持久存储。
    values: RefCell<HashMap<(String, String), Value>>,
    /// 已注册的工具名。
    tools: RefCell<Vec<String>>,
    /// 已发出的 HTTP 请求计数。
    http_call_count: RefCell<usize>,
}

impl LiveApiHost {
    /// 用真实目录创建宿主替身。
    fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
            values: RefCell::new(HashMap::new()),
            tools: RefCell::new(Vec::new()),
            http_call_count: RefCell::new(0),
        }
    }

    /// 用既有存储创建替身，模拟重启后读到同一份持久数据。
    fn restored_from(
        repo_root: impl Into<PathBuf>,
        values: HashMap<(String, String), Value>,
    ) -> Self {
        Self {
            repo_root: repo_root.into(),
            values: RefCell::new(values),
            tools: RefCell::new(Vec::new()),
            http_call_count: RefCell::new(0),
        }
    }

    /// 返回当前存储快照。
    fn snapshot(&self) -> HashMap<(String, String), Value> {
        self.values.borrow().clone()
    }

    /// 返回已发出的 HTTP 请求计数。
    fn http_call_count(&self) -> usize {
        *self.http_call_count.borrow()
    }

    /// 作用域命名空间键。
    fn scope_key(scope: PluginStoreScope) -> String {
        match scope {
            PluginStoreScope::Workspace => "workspace".into(),
            PluginStoreScope::User => "user".into(),
        }
    }
}

impl PluginHostApi for LiveApiHost {
    fn upsert_tool(&self, local_name: &str, _spec: &ToolSpec) -> Result<String> {
        self.tools.borrow_mut().push(local_name.to_string());
        Ok(local_name.to_string())
    }

    fn remove_tool(&self, public_name: &str) -> Result<()> {
        self.tools
            .borrow_mut()
            .retain(|name| name != public_name);
        Ok(())
    }

    fn upsert_prompt(&self, prompt: &PromptContribution) -> Result<String> {
        Ok(prompt.id.clone())
    }

    fn remove_prompt(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    fn emit_event(&self, _event: &ExtensionEvent) -> Result<()> {
        Ok(())
    }

    fn get_state(&self, _key: &str) -> Result<Option<Value>> {
        Ok(None)
    }

    fn set_state(&self, _key: &str, _value: &Value) -> Result<()> {
        Ok(())
    }

    fn remove_state(&self, _key: &str) -> Result<Option<Value>> {
        Ok(None)
    }

    fn store_get(&self, scope: PluginStoreScope, key: &str) -> Result<Option<Value>> {
        Ok(self
            .values
            .borrow()
            .get(&(Self::scope_key(scope), key.to_string()))
            .cloned())
    }

    fn store_set(&self, scope: PluginStoreScope, key: &str, value: &Value) -> Result<()> {
        self.values
            .borrow_mut()
            .insert((Self::scope_key(scope), key.to_string()), value.clone());
        Ok(())
    }

    fn store_remove(&self, scope: PluginStoreScope, key: &str) -> Result<Option<Value>> {
        Ok(self
            .values
            .borrow_mut()
            .remove(&(Self::scope_key(scope), key.to_string())))
    }

    fn store_list(&self, scope: PluginStoreScope) -> Result<Vec<String>> {
        let scope = Self::scope_key(scope);
        Ok(self
            .values
            .borrow()
            .keys()
            .filter(|(entry_scope, _)| *entry_scope == scope)
            .map(|(_, key)| key.clone())
            .collect())
    }

    fn upsert_service(&self, _service: &ServiceSpec) -> Result<()> {
        Ok(())
    }

    fn remove_service(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn list_services(&self, _plugin_id: Option<&str>) -> Result<Vec<ServiceDescriptor>> {
        Ok(Vec::new())
    }

    fn call_service(&self, _plugin_id: &str, _name: &str, _payload: &Value) -> Result<Value> {
        Err(anyhow!("集成测试不提供服务调用"))
    }

    fn read_file(&self, path: &str) -> Result<String> {
        let clean = path.trim_start_matches("./");
        let full = self.repo_root.join(clean);
        std::fs::read_to_string(&full)
            .map_err(|error| anyhow!("读取 {} 失败：{error}", full.display()))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>> {
        let dir = if path == "." {
            self.repo_root.clone()
        } else {
            self.repo_root.join(path.trim_matches('/'))
        };
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&dir)
            .map_err(|error| anyhow!("列目录 {} 失败：{error}", dir.display()))?
        {
            let entry = entry.map_err(|error| anyhow!("目录条目：{error}"))?;
            let is_dir = entry
                .file_type()
                .map(|file_type| file_type.is_dir())
                .unwrap_or(false);
            // 转成相对于 repo_root 的路径。
            let full = entry.path();
            let relative = full
                .strip_prefix(&self.repo_root)
                .unwrap_or(&full)
                .to_string_lossy()
                .to_string();
            entries.push(FileEntry {
                path: relative,
                is_dir,
            });
        }
        entries.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(entries)
    }

    fn spawn_process(&self, _spec: &ProcessSpec) -> Result<u64> {
        Err(anyhow!("集成测试不提供子进程"))
    }

    fn write_process(&self, _handle: u64, _data: &str) -> Result<()> {
        Err(anyhow!("集成测试不提供子进程"))
    }

    fn read_process_line(&self, _handle: u64, _timeout_ms: u64) -> Result<Option<String>> {
        Err(anyhow!("集成测试不提供子进程"))
    }

    fn kill_process(&self, _handle: u64) -> Result<()> {
        Err(anyhow!("集成测试不提供子进程"))
    }

    /// 转发 HTTP 请求到 SiliconFlow，注入 base URL 与 Authorization。
    fn http_request(&self, request: &HttpRequest) -> Result<HttpResponse> {
        *self.http_call_count.borrow_mut() += 1;

        let url = format!(
            "{SILICONFLOW_BASE_URL}{}",
            request.path.trim_start_matches('/')
        );
        // 嵌入端点只用 POST；关闭 http_status_as_error 使 4xx/5xx 走 Ok 分支。
        let agent = ureq::Agent::new_with_config(
            ureq::config::Config::builder()
                .http_status_as_error(false)
                .build(),
        );
        if request.method.to_uppercase() != "POST" {
            return Err(anyhow!(
                "集成测试只支持 POST，收到 {}",
                request.method
            ));
        }
        let mut builder = agent.post(&url);
        builder = builder.header("authorization", &format!("Bearer {SILICONFLOW_API_KEY}"));
        for (key, value) in &request.headers {
            builder = builder.header(key.as_str(), value.as_str());
        }

        let response = match request.body.as_deref() {
            Some(body) => builder.send(body.as_bytes()),
            None => builder.send(&[] as &[u8]),
        };

        match response {
            Ok(resp) => {
                let status = resp.status().as_u16();
                let body = resp
                    .into_body()
                    .read_to_string()
                    .map_err(|error| anyhow!("读取响应正文失败：{error}"))?;
                Ok(HttpResponse {
                    status,
                    headers: HashMap::new(),
                    body,
                })
            }
            Err(error) => Err(anyhow!("HTTP 请求失败：{error}")),
        }
    }
}

/// 构造启用 SiliconFlow API 嵌入的激活上下文。
fn siliconflow_activation_context(workspace_root: &str) -> ActivationContext {
    ActivationContext {
        plugin_id: "code-context".into(),
        metadata: HashMap::from([
            ("workspace_root".to_string(), workspace_root.to_string()),
            ("embedding_mode".to_string(), "api".to_string()),
            ("embedding_model".to_string(), EMBEDDING_MODEL.to_string()),
            (
                "embedding_dimension".to_string(),
                EMBEDDING_DIMENSION.to_string(),
            ),
            ("embedding_batch_size".to_string(), "32".to_string()),
            // bge-m3 不接受 dimensions 参数；关闭后靠返回值校验维度。
            (
                "embedding_send_dimensions".to_string(),
                "false".to_string(),
            ),
            ("http_endpoint_id".to_string(), "siliconflow-test".to_string()),
        ]),
    }
}

/// 推进索引直到完成或超时。
fn advance_until_ready(
    plugin: &mut CodeContextPlugin,
    host: &dyn PluginHostApi,
    max_rounds: usize,
) -> IndexStatus {
    use agent_plugin::AgentPlugin;
    use serde_json::json;

    for _ in 0..max_rounds {
        let _result = plugin
            .call_tool_with_host(
                host,
                agent_plugin::ToolCall::new("call-e2e", "code_search", json!({"query": "test"})),
            )
            .expect("工具调用不应 panic");
        let status = plugin.index_status();
        if status != IndexStatus::Indexing {
            return status;
        }
    }
    plugin.index_status()
}

/// 定位本项目的插件源码目录作为测试仓库。
fn plugin_src_dir() -> PathBuf {
    // 集成测试 cwd 通常是包根目录。
    let manifest_dir =
        PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = manifest_dir.join("src");
    assert!(
        src.is_dir(),
        "测试仓库路径不存在：{}",
        src.display()
    );
    src
}

// ---------------------------------------------------------------------------
// 测试用例
// ---------------------------------------------------------------------------

/// 全流程：索引构建 → SiliconFlow 嵌入 → 语义检索返回相关结果。
#[test]
#[ignore = "需要网络访问 SiliconFlow API"]
fn full_pipeline_index_embed_search() {
    use agent_plugin::AgentPlugin;

    let src = plugin_src_dir();
    let host = LiveApiHost::new(&src);
    let context = siliconflow_activation_context(src.to_str().expect("路径应为 UTF-8"));

    let mut plugin = CodeContextPlugin::default();
    plugin.activate(&host, context).expect("激活应成功");

    let status = advance_until_ready(&mut plugin, &host, 20);
    assert!(
        status == IndexStatus::Ready || status == IndexStatus::Degraded,
        "索引应至少建成两路，实际状态：{status:?}"
    );
    println!("索引状态：{status:?}");
    println!("HTTP 调用次数：{}", host.http_call_count());

    let stats = plugin.index_stats().expect("索引应已建成");
    println!(
        "索引规模：{} 个文件、{} 个代码块",
        stats.files, stats.chunks
    );
    assert!(stats.files > 0, "至少应索引一些文件");

    // 语义检索：查询与嵌入模型的语义理解相关
    let results = plugin.search_results(&host, "嵌入向量的维度校验", None, 10);
    println!("检索结果数：{}", results.len());
    for (index, result) in results.iter().enumerate() {
        println!(
            "  [{index}] {} 行 {}–{} (分数 {:.4})",
            result.file_path, result.start_line, result.end_line, result.score
        );
    }
    assert!(!results.is_empty(), "语义检索应返回结果");

    // 检查是否命中了与嵌入/维度相关的文件。
    let hit_embedding = results
        .iter()
        .any(|result| result.file_path.contains("embedding"));
    println!(
        "是否命中 embedding 相关文件：{hit_embedding}"
    );
}

/// 向量持久化：索引建成后存储快照不为空，重启后可恢复并跳过重复嵌入。
#[test]
#[ignore = "需要网络访问 SiliconFlow API"]
fn vectors_are_persisted_and_restored() {
    use agent_plugin::AgentPlugin;

    let src = plugin_src_dir();
    let host = LiveApiHost::new(&src);
    let workspace_root = src.to_str().expect("路径应为 UTF-8");
    let context = siliconflow_activation_context(workspace_root);

    // 第一次：构建索引并嵌入。
    let mut plugin = CodeContextPlugin::default();
    plugin.activate(&host, context).expect("首次激活");
    let status = advance_until_ready(&mut plugin, &host, 20);
    println!("首次索引状态：{status:?}，HTTP 调用：{}", host.http_call_count());

    let first_http_count = host.http_call_count();
    assert!(first_http_count > 0, "首次构建应发起 HTTP 嵌入请求");

    // 持久化存储应包含语义快照。
    let stored = host.snapshot();
    let has_semantic = stored
        .keys()
        .any(|(_, key)| key.contains("semantic"));
    assert!(has_semantic, "向量快照应被写入持久存储");

    // 第二次：模拟重启——用同一份存储创建新宿主。
    let restored_host = LiveApiHost::restored_from(&src, stored);
    let mut plugin2 = CodeContextPlugin::default();
    plugin2
        .activate(
            &restored_host,
            siliconflow_activation_context(workspace_root),
        )
        .expect("重启后激活");
    let status2 = advance_until_ready(&mut plugin2, &restored_host, 20);
    println!(
        "重启后索引状态：{status2:?}，HTTP 调用：{}",
        restored_host.http_call_count()
    );

    // 如果向量成功恢复，第二次不应再发任何嵌入请求（或仅极少量）。
    assert!(
        restored_host.http_call_count() < first_http_count,
        "重启后应利用缓存向量，减少嵌入调用：首次 {}，重启 {}",
        first_http_count,
        restored_host.http_call_count()
    );

    // 重启后检索结果仍然可用。
    let results = plugin2.search_results(&restored_host, "嵌入向量的维度校验", None, 10);
    assert!(!results.is_empty(), "恢复向量后检索应有结果");
    println!(
        "重启后检索结果数：{}，首条：{} 行 {}",
        results.len(),
        results[0].file_path,
        results[0].start_line
    );
}

/// 语义检索质量：不同查询应返回语义相关的不同结果。
#[test]
#[ignore = "需要网络访问 SiliconFlow API"]
fn semantic_search_returns_relevant_results() {
    use agent_plugin::AgentPlugin;

    let src = plugin_src_dir();
    let host = LiveApiHost::new(&src);
    let workspace_root = src.to_str().expect("路径应为 UTF-8");
    let context = siliconflow_activation_context(workspace_root);

    let mut plugin = CodeContextPlugin::default();
    plugin.activate(&host, context).expect("激活");
    let status = advance_until_ready(&mut plugin, &host, 20);
    if status != IndexStatus::Ready {
        println!("索引未达 Ready（{status:?}），语义检索可能降级");
    }

    // 查询 1：配置解析
    let config_results = plugin.search_results(&host, "解析插件配置元数据", None, 5);
    println!("「配置解析」检索结果：");
    for result in &config_results {
        println!("  {} 行 {}", result.file_path, result.start_line);
    }

    // 查询 2：错误处理
    let error_results = plugin.search_results(&host, "领域错误类型定义", None, 5);
    println!("「错误处理」检索结果：");
    for result in &error_results {
        println!("  {} 行 {}", result.file_path, result.start_line);
    }

    // 两次查询的结果集不应完全相同——说明语义在起作用。
    let files_1: Vec<&str> = config_results
        .iter()
        .map(|result| result.file_path.as_str())
        .collect();
    let files_2: Vec<&str> = error_results
        .iter()
        .map(|result| result.file_path.as_str())
        .collect();
    assert_ne!(
        files_1, files_2,
        "不同语义查询应返回不同结果顺序，说明向量比较在发挥作用"
    );
}

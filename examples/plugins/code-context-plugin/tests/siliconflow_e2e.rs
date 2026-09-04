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
use anyhow::{anyhow, Context};
use code_context_plugin::{CodeContextPlugin, IndexStatus};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// SiliconFlow 嵌入 API 的 base URL。
const SILICONFLOW_BASE_URL: &str = "https://api.siliconflow.cn/v1/";

/// 读取 SiliconFlow API 密钥的环境变量名。
const SILICONFLOW_API_KEY_ENV: &str = "SILICONFLOW_API_KEY";

/// 嵌入模型——SiliconFlow 支持的 BAAI/bge-m3 维度 1024。
const EMBEDDING_MODEL: &str = "BAAI/bge-m3";

/// 嵌入维度。
const EMBEDDING_DIMENSION: u32 = 1024;

/// 测试宿主存储文件的版本；只用于识别测试夹具自身的 JSON 格式。
const TEST_STORE_SCHEMA_VERSION: u32 = 1;

/// 测试宿主持久化文件所在的目录名。
const TEST_STORE_DIRECTORY: &str = "lucia-code-context-siliconflow";

/// 原子写入临时文件使用的进程内序号。
static STORE_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 测试宿主磁盘文件中的 JSON 信封。
#[derive(Debug, Serialize, Deserialize)]
struct PersistedTestStore {
    /// 测试夹具存储格式版本。
    schema_version: u32,
    /// 按作用域分组的键值内容。
    values: HashMap<String, HashMap<String, Value>>,
}

/// 使用真实文件系统与真实 HTTP 的宿主替身。
///
/// 文件从指定根目录读取；HTTP 请求转发到 SiliconFlow，自动注入凭证。
struct LiveApiHost {
    /// 仓库根目录的绝对路径。
    repo_root: PathBuf,
    /// 当前测试场景的存储命名空间，避免被其他忽略测试互相污染。
    store_namespace: String,
    /// 宿主持久存储对应的 JSON 文件。
    store_path: PathBuf,
    /// 从磁盘载入的键值缓存；每次写入成功后才更新。
    values: RefCell<HashMap<(String, String), Value>>,
    /// 已注册的工具名。
    tools: RefCell<Vec<String>>,
    /// 已发出的 HTTP 请求计数。
    http_call_count: RefCell<usize>,
    /// 仅保存在测试宿主内存中的 API 密钥；不会写入插件元数据或持久化存储。
    api_key: Option<String>,
}

impl LiveApiHost {
    /// 用真实目录创建宿主替身，并从磁盘加载指定场景的存储。
    fn for_test(repo_root: impl Into<PathBuf>, namespace: impl Into<String>) -> Self {
        let repo_root = repo_root.into();
        let store_namespace = namespace.into();
        let store_path = test_store_path(&repo_root, &store_namespace);
        let values = load_store(&store_path).expect("读取测试宿主持久存储");
        Self {
            repo_root,
            store_namespace,
            store_path,
            values: RefCell::new(values),
            tools: RefCell::new(Vec::new()),
            http_call_count: RefCell::new(0),
            api_key: None,
        }
    }

    /// 创建允许访问真实 SiliconFlow 端点的宿主替身。
    ///
    /// 密钥只从 [`SILICONFLOW_API_KEY_ENV`] 指定的环境变量读取，并保存在宿主侧内存中。
    ///
    /// # Panics
    ///
    /// 环境变量缺失或为空时终止忽略测试，避免把凭证写入源码或测试缓存。
    fn for_live_test(repo_root: impl Into<PathBuf>, namespace: impl Into<String>) -> Self {
        let api_key = std::env::var(SILICONFLOW_API_KEY_ENV)
            .unwrap_or_else(|_| panic!("请设置 {SILICONFLOW_API_KEY_ENV} 后再运行真实 API 测试"));
        assert!(
            !api_key.trim().is_empty(),
            "{SILICONFLOW_API_KEY_ENV} 不能为空"
        );
        let mut host = Self::for_test(repo_root, namespace);
        host.api_key = Some(api_key);
        host
    }

    /// 返回同一场景的新宿主实例，用于模拟跨进程重启。
    fn reopen(&self) -> Self {
        let mut reopened = Self::for_test(self.repo_root.clone(), self.store_namespace.clone());
        reopened.api_key.clone_from(&self.api_key);
        reopened
    }

    /// 返回磁盘上的存储快照，而不是当前实例的内存缓存。
    fn snapshot(&self) -> HashMap<(String, String), Value> {
        load_store(&self.store_path).expect("读取测试宿主持久存储快照")
    }

    /// 返回测试宿主实际使用的 JSON 文件路径。
    fn store_path(&self) -> &Path {
        &self.store_path
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

    /// 事务性保存当前键值映射；落盘失败时不修改内存缓存。
    fn persist_values(&self, values: &HashMap<(String, String), Value>) -> Result<()> {
        save_store(&self.store_path, values)
    }
}

/// 按仓库路径与测试场景派生稳定的存储文件名。
///
/// 文件放在系统临时目录而不是被扫描的仓库目录中；不包含进程 ID，因此独立运行的
/// 第二个测试进程仍能打开同一份数据。场景命名空间让多个忽略测试可以并行执行而不互相
/// 覆盖索引。
fn test_store_path(repo_root: &Path, namespace: &str) -> PathBuf {
    let canonical_root = repo_root
        .canonicalize()
        .unwrap_or_else(|_| repo_root.to_path_buf());
    let mut digest = Sha256::new();
    digest.update(b"lucia-code-context-siliconflow-test-store\0");
    digest.update(canonical_root.to_string_lossy().as_bytes());
    digest.update([0]);
    digest.update(namespace.as_bytes());
    let file_name = format!("{:x}.json", digest.finalize());
    std::env::temp_dir()
        .join(TEST_STORE_DIRECTORY)
        .join(file_name)
}

/// 从磁盘读取测试宿主 JSON，并还原为宿主 API 使用的扁平键空间。
fn load_store(path: &Path) -> Result<HashMap<(String, String), Value>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("读取测试宿主存储失败：{}", path.display()))
        }
    };
    let stored: PersistedTestStore = serde_json::from_slice(&bytes)
        .with_context(|| format!("解析测试宿主存储失败：{}", path.display()))?;
    if stored.schema_version != TEST_STORE_SCHEMA_VERSION {
        return Err(anyhow!(
            "测试宿主存储版本不受支持：{}",
            stored.schema_version
        ));
    }

    let mut values = HashMap::new();
    for (scope, entries) in stored.values {
        for (key, value) in entries {
            values.insert((scope.clone(), key), value);
        }
    }
    Ok(values)
}

/// 将扁平键空间以 JSON 信封原子写入磁盘。
///
/// 先写同目录临时文件并同步，再通过重命名替换目标；写入失败时不会留下半份存储，
/// 也不会影响调用方尚未提交的内存缓存。
fn save_store(path: &Path, values: &HashMap<(String, String), Value>) -> Result<()> {
    let mut scoped_values: HashMap<String, HashMap<String, Value>> = HashMap::new();
    for ((scope, key), value) in values {
        scoped_values
            .entry(scope.clone())
            .or_default()
            .insert(key.clone(), value.clone());
    }
    let contents = serde_json::to_vec_pretty(&PersistedTestStore {
        schema_version: TEST_STORE_SCHEMA_VERSION,
        values: scoped_values,
    })
    .context("序列化测试宿主存储失败")?;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("测试宿主存储路径没有父目录：{}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("创建测试宿主存储目录失败：{}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("测试宿主存储文件名不是有效 UTF-8：{}", path.display()))?;
    let sequence = STORE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));

    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("创建测试宿主临时存储失败：{}", temporary.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(&contents)
            .with_context(|| format!("写入测试宿主存储失败：{}", temporary.display()))?;
        file.sync_all()
            .with_context(|| format!("同步测试宿主存储失败：{}", temporary.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("替换测试宿主存储失败：{}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

impl PluginHostApi for LiveApiHost {
    fn upsert_tool(&self, local_name: &str, _spec: &ToolSpec) -> Result<String> {
        self.tools.borrow_mut().push(local_name.to_string());
        Ok(local_name.to_string())
    }

    fn remove_tool(&self, public_name: &str) -> Result<()> {
        self.tools.borrow_mut().retain(|name| name != public_name);
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
        let mut values = self.values.borrow().clone();
        values.insert((Self::scope_key(scope), key.to_string()), value.clone());
        self.persist_values(&values)?;
        *self.values.borrow_mut() = values;
        Ok(())
    }

    fn store_remove(&self, scope: PluginStoreScope, key: &str) -> Result<Option<Value>> {
        let mut values = self.values.borrow().clone();
        let removed = values.remove(&(Self::scope_key(scope), key.to_string()));
        if removed.is_some() {
            self.persist_values(&values)?;
            *self.values.borrow_mut() = values;
        }
        Ok(removed)
    }

    fn store_list(&self, scope: PluginStoreScope) -> Result<Vec<String>> {
        let scope = Self::scope_key(scope);
        let mut keys: Vec<String> = self
            .values
            .borrow()
            .keys()
            .filter(|(entry_scope, _)| *entry_scope == scope)
            .map(|(_, key)| key.clone())
            .collect();
        keys.sort();
        Ok(keys)
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
            return Err(anyhow!("集成测试只支持 POST，收到 {}", request.method));
        }
        let api_key = self
            .api_key
            .as_deref()
            .ok_or_else(|| anyhow!("真实 API 测试宿主未配置 {SILICONFLOW_API_KEY_ENV}"))?;
        let mut builder = agent.post(&url);
        builder = builder.header("authorization", &format!("Bearer {api_key}"));
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
                // 真实端点的限流窗口提示必须交给插件退避策略，其他响应头不扩大到 Guest。
                let headers = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .map(|value| HashMap::from([("retry-after".to_string(), value.to_string())]))
                    .unwrap_or_default();
                let body = resp
                    .into_body()
                    .read_to_string()
                    .map_err(|error| anyhow!("读取响应正文失败：{error}"))?;
                Ok(HttpResponse {
                    status,
                    headers,
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
            (
                "embedding_request_interval_ms".to_string(),
                "1500".to_string(),
            ),
            ("embedding_max_retries".to_string(), "4".to_string()),
            ("embedding_retry_base_ms".to_string(), "5000".to_string()),
            // bge-m3 不接受 dimensions 参数；关闭后靠返回值校验维度。
            ("embedding_send_dimensions".to_string(), "false".to_string()),
            (
                "http_endpoint_id".to_string(),
                "siliconflow-test".to_string(),
            ),
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
    // let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest_dir = PathBuf::from("/Users/teriri/teririProjects/ascnet-lucia");
    assert!(
        manifest_dir.is_dir(),
        "测试仓库路径不存在：{}",
        manifest_dir.display()
    );
    manifest_dir
}

// ---------------------------------------------------------------------------
// 测试用例
// ---------------------------------------------------------------------------

/// 测试宿主写入的键值必须能被新实例从磁盘重新读取。
#[test]
fn disk_store_survives_reopen() {
    let src = plugin_src_dir();
    let namespace = format!("disk-roundtrip-{}", std::process::id());
    let path = test_store_path(&src, &namespace);
    let _ = fs::remove_file(&path);

    let host = LiveApiHost::for_test(&src, &namespace);
    let value = serde_json::json!({"persisted": true});
    host.store_set(PluginStoreScope::Workspace, "disk.probe", &value)
        .expect("写入测试宿主存储");
    assert!(path.is_file(), "存储文件应落盘：{}", path.display());

    let reopened = host.reopen();
    assert_eq!(
        reopened
            .store_get(PluginStoreScope::Workspace, "disk.probe")
            .expect("读取测试宿主存储"),
        Some(value)
    );

    fs::remove_file(&path).expect("清理测试宿主存储文件");
}

/// 全流程：索引构建 → SiliconFlow 嵌入 → 语义检索返回相关结果。
#[test]
#[ignore = "需要网络访问 SiliconFlow API"]
fn full_pipeline_index_embed_search() {
    use agent_plugin::AgentPlugin;

    let src = plugin_src_dir();
    let host = LiveApiHost::for_live_test(&src, "full-pipeline");
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
    println!("是否命中 embedding 相关文件：{hit_embedding}");
}

/// 向量持久化：索引建成后存储快照不为空，重启后可恢复并跳过重复嵌入。
#[test]
#[ignore = "需要网络访问 SiliconFlow API"]
fn vectors_are_persisted_and_restored() {
    use agent_plugin::AgentPlugin;

    let src = plugin_src_dir();
    let host = LiveApiHost::for_live_test(&src, "vectors");
    let workspace_root = src.to_str().expect("路径应为 UTF-8");
    let context = siliconflow_activation_context(workspace_root);

    // 第一次：构建索引并嵌入。
    let mut plugin = CodeContextPlugin::default();
    plugin.activate(&host, context).expect("首次激活");
    let status = advance_until_ready(&mut plugin, &host, 20);
    println!(
        "首次索引状态：{status:?}，HTTP 调用：{}",
        host.http_call_count()
    );

    let first_http_count = host.http_call_count();
    assert!(first_http_count > 0, "首次构建应发起 HTTP 嵌入请求");

    // 持久化存储应包含语义快照。
    let stored = host.snapshot();
    let has_semantic = stored.keys().any(|(_, key)| key.contains("semantic"));
    assert!(has_semantic, "向量快照应被写入持久存储");
    assert!(
        host.store_path().is_file(),
        "向量快照应真实写入 JSON 文件：{}",
        host.store_path().display()
    );

    // 第二次：模拟重启——重新打开同一场景的磁盘 JSON。
    let restored_host = host.reopen();
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
    let host = LiveApiHost::for_live_test(&src, "semantic-search");
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

/// 搜索演示：多条查询展示语义检索效果。
#[test]
#[ignore = "需要网络访问 SiliconFlow API"]
fn search_demo() {
    use agent_plugin::AgentPlugin;

    let src = plugin_src_dir();
    let host = LiveApiHost::for_live_test(&src, "search-demo");
    let workspace_root = src.to_str().expect("路径应为 UTF-8");

    let mut plugin = CodeContextPlugin::default();
    plugin
        .activate(&host, siliconflow_activation_context(workspace_root))
        .expect("激活");
    let status = advance_until_ready(&mut plugin, &host, 20);
    let stats = plugin.index_stats().expect("索引应已建成");
    println!("=== 索引完成 ===");
    println!(
        "状态: {status:?} | 文件: {} | 代码块: {} | HTTP 调用: {}",
        stats.files,
        stats.chunks,
        host.http_call_count()
    );
    println!();

    let queries = ["agent调用的方法"];

    for query in &queries {
        let results = plugin.search_results(&host, query, None, 5);
        println!("──────────────────────────────────────────────");
        println!("查询: {query}");
        println!("──────────────────────────────────────────────");
        if results.is_empty() {
            println!("  (无结果)");
        }
        for (i, r) in results.iter().enumerate() {
            println!(
                "  [{}] {:<40} 行 {:>4}–{:<4}  分数 {:.4}",
                i + 1,
                r.file_path,
                r.start_line,
                r.end_line,
                r.score
            );
            if let Some(name) = r.symbol_name.as_deref() {
                let kind = r.symbol_kind.map(|k| format!("{k:?}")).unwrap_or_default();
                println!("       符号: {name} ({kind})");
            }
        }
        println!();
    }
}

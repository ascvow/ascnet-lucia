//! 单元测试共用的宿主替身。
//!
//! 只实现本插件真实使用的能力：当前工作区读取探测、持久存储、工具注册与事件发布。
//! 其余入口一律返回错误，使任何越界调用在测试中立即暴露，而不是悄悄拿到空结果。

use agent_plugin::{
    ActivationContext, ExtensionEvent, FileEntry, HttpRequest, HttpResponse, PluginHostApi,
    PluginStoreScope, ProcessSpec, PromptContribution, Result, ServiceDescriptor, ServiceSpec,
    ToolSpec,
};
use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 测试宿主磁盘存储的格式版本。
const PERSISTED_STORE_SCHEMA_VERSION: u32 = 1;

/// 原子写入临时文件使用的进程内序号。
static PERSISTED_STORE_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 测试宿主磁盘文件中的 JSON 信封。
#[derive(Debug, Serialize, Deserialize)]
struct PersistedHostStore {
    /// 存储格式版本，用于拒绝无法安全解释的未来格式。
    schema_version: u32,
    /// 按作用域分组的宿主键值内容。
    values: BTreeMap<String, BTreeMap<String, Value>>,
}

/// 测试宿主可注入的 HTTP 请求处理函数。
type HttpHandler = dyn Fn(&HttpRequest) -> Result<HttpResponse>;

/// 构造一条成功的 JSON 应答，供 [`TestHost::with_http_responses`] 使用。
pub fn http_json(status: u16, body: &str) -> Result<HttpResponse> {
    Ok(HttpResponse {
        status,
        headers: HashMap::new(),
        body: body.to_string(),
    })
}

/// 按请求中的 `input` 条数生成一份合法嵌入应答。
///
/// 固定条数的伪应答会在 chunk 数变化时失效，而 chunk 数取决于解析器——把断言绑在它上面
/// 等于让无关改动去改测试。这里按请求实际条数生成，行为与真实端点一致。
///
/// 第 i 条向量把第 `i % dimension` 位置一，使"哪条最相似"完全可预测。
fn echo_embeddings(request: &HttpRequest, dimension: usize) -> Result<HttpResponse> {
    let body: Value = serde_json::from_str(request.body.as_deref().unwrap_or("{}"))
        .map_err(|error| anyhow!("伪端点无法解析请求正文：{error}"))?;
    let count = body
        .get("input")
        .and_then(Value::as_array)
        .map(Vec::len)
        .ok_or_else(|| anyhow!("伪端点要求请求带 input 数组"))?;
    let items: Vec<String> = (0..count)
        .map(|index| {
            let vector: Vec<String> = (0..dimension)
                .map(|axis| if axis == index % dimension { "1" } else { "0" }.to_string())
                .collect();
            format!(r#"{{"index":{index},"embedding":[{}]}}"#, vector.join(","))
        })
        .collect();
    http_json(200, &format!(r#"{{"data":[{}]}}"#, items.join(",")))
}

/// 记录调用的宿主替身。
#[derive(Default)]
pub struct TestHost {
    /// 按 (作用域, 键) 保存的持久存储内容。
    values: RefCell<HashMap<(String, String), Value>>,
    /// 可选的磁盘存储路径；存在时每次变更都先原子落盘再更新内存。
    store_path: Option<PathBuf>,
    /// 已注册的工具本地名。
    tools: RefCell<Vec<String>>,
    /// 已发布的扩展事件名。
    events: RefCell<Vec<String>>,
    /// 已发布的扩展事件快照，用于验证结构化进度与 Footer 展示载荷。
    event_payloads: RefCell<Vec<ExtensionEvent>>,
    /// 项目作用域是否可用；关闭后模拟宿主未注入项目身份。
    workspace_available: bool,
    /// 当前工作区只读能力是否可用。
    workspace_read_available: bool,
    /// 按调用顺序返回的 HTTP 应答队列；为空时回落到 `echo_embeddings` 或失败。
    ///
    /// 嵌入 provider 的正确性几乎全在「怎么解析端点返回的东西」上：顺序错乱、条数不符、
    /// 维度不符、非 2xx。真实端点无法稳定复现这些，用队列逐一构造。
    http_responses: RefCell<Vec<Result<HttpResponse>>>,
    /// 队列耗尽后按请求条数自动生成嵌入应答的维度；`None` 表示不自动生成。
    echo_embedding_dimension: Option<usize>,
    /// 已收到的 HTTP 请求，用于断言插件提交了什么。
    http_requests: RefCell<Vec<HttpRequest>>,
    /// 可选的真实 HTTP 转发函数；示例程序用它接入用户提供的嵌入端点。
    ///
    /// 单元测试仍优先使用预置应答和确定性伪向量，避免网络状态影响测试稳定性。
    http_handler: Option<Box<HttpHandler>>,
}

impl TestHost {
    /// 创建项目作用域可用的替身。
    pub fn new() -> Self {
        Self {
            workspace_available: true,
            workspace_read_available: true,
            ..Self::default()
        }
    }

    /// 创建使用指定 JSON 文件持久化键值的宿主替身。
    ///
    /// 文件不存在时从空存储开始；已存在时读取同一份作用域数据，使独立进程可以复用
    /// manifest、文件哈希和语义向量快照。
    ///
    /// # Errors
    ///
    /// 文件无法读取、JSON 损坏或格式版本不受支持时返回错误。
    pub fn persisted(path: impl Into<PathBuf>) -> Result<Self> {
        let store_path = path.into();
        let values = load_persisted_store(&store_path)?;
        Ok(Self {
            values: RefCell::new(values),
            store_path: Some(store_path),
            workspace_available: true,
            workspace_read_available: true,
            ..Self::default()
        })
    }

    /// 创建缺少项目作用域的替身。
    pub fn without_workspace_scope() -> Self {
        Self {
            workspace_read_available: true,
            ..Self::default()
        }
    }

    /// 创建缺少当前工作区只读能力的替身。
    pub fn without_workspace_read() -> Self {
        Self {
            workspace_available: true,
            ..Self::default()
        }
    }

    /// 预置按序返回的 HTTP 应答。
    pub fn with_http_responses(mut self, responses: Vec<Result<HttpResponse>>) -> Self {
        // 反转一次，使 `pop` 按传入顺序取用。
        self.http_responses = RefCell::new(responses.into_iter().rev().collect());
        self
    }

    /// 让替身像真实嵌入端点那样按请求条数生成应答。
    pub fn with_echo_embeddings(mut self, dimension: usize) -> Self {
        self.echo_embedding_dimension = Some(dimension);
        self
    }

    /// 注入自定义 HTTP 处理函数。
    ///
    /// 处理函数只接收 Guest 构造的相对路径请求；base URL、凭证、超时与响应上限仍应由
    /// 调用方在处理函数中控制。预置应答队列优先于该函数，便于测试特定失败分支。
    pub fn with_http_handler(
        mut self,
        handler: impl Fn(&HttpRequest) -> Result<HttpResponse> + 'static,
    ) -> Self {
        self.http_handler = Some(Box::new(handler));
        self
    }

    /// 返回已收到的 HTTP 请求快照。
    pub fn http_requests(&self) -> Vec<HttpRequest> {
        self.http_requests.borrow().clone()
    }

    /// 返回该替身持有的存储内容，用于模拟应用重启后重新加载。
    pub fn snapshot(&self) -> HashMap<(String, String), Value> {
        self.values.borrow().clone()
    }

    /// 用既有存储内容创建替身，模拟插件重新加载后读到同一份磁盘数据。
    pub fn restored_from(values: HashMap<(String, String), Value>) -> Self {
        Self {
            values: RefCell::new(values),
            workspace_available: true,
            workspace_read_available: true,
            ..Self::default()
        }
    }

    /// 直接写入项目作用域，用于构造损坏或未来版本的数据。
    pub fn seed_workspace(&self, key: &str, value: Value) {
        self.values.borrow_mut().insert(
            (
                Self::scope_key(PluginStoreScope::Workspace),
                key.to_string(),
            ),
            value,
        );
    }

    /// 返回用户作用域中的全部键，用于断言插件没有把项目数据写到跨项目存储。
    pub fn user_keys(&self) -> Vec<String> {
        let scope = Self::scope_key(PluginStoreScope::User);
        self.values
            .borrow()
            .keys()
            .filter(|(entry_scope, _)| *entry_scope == scope)
            .map(|(_, key)| key.clone())
            .collect()
    }

    /// 返回已注册的工具本地名。
    pub fn registered_tools(&self) -> Vec<String> {
        self.tools.borrow().clone()
    }

    /// 返回已发布的扩展事件名。
    pub fn emitted_events(&self) -> Vec<String> {
        self.events.borrow().clone()
    }

    /// 返回已发布的扩展事件快照。
    pub fn emitted_event_payloads(&self) -> Vec<ExtensionEvent> {
        self.event_payloads.borrow().clone()
    }

    /// 返回作用域的存储命名空间键。
    fn scope_key(scope: PluginStoreScope) -> String {
        match scope {
            PluginStoreScope::Workspace => "workspace".into(),
            PluginStoreScope::User => "user".into(),
        }
    }

    /// 校验作用域可用性。
    fn ensure_scope(&self, scope: PluginStoreScope) -> Result<()> {
        if scope == PluginStoreScope::Workspace && !self.workspace_available {
            return Err(anyhow!("当前宿主未提供项目作用域"));
        }
        Ok(())
    }

    /// 在启用磁盘存储时事务性保存一份候选值；失败时调用方保留原内存状态。
    fn persist_values(&self, values: &HashMap<(String, String), Value>) -> Result<()> {
        match self.store_path.as_deref() {
            Some(path) => save_persisted_store(path, values),
            None => Ok(()),
        }
    }
}

/// 从磁盘读取测试宿主 JSON，并还原为宿主 API 使用的扁平键空间。
fn load_persisted_store(path: &Path) -> Result<HashMap<(String, String), Value>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => {
            return Err(anyhow!(
                "读取代码索引缓存 `{}` 失败：{error}",
                path.display()
            ))
        }
    };
    let stored: PersistedHostStore = serde_json::from_slice(&bytes)
        .map_err(|error| anyhow!("解析代码索引缓存 `{}` 失败：{error}", path.display()))?;
    if stored.schema_version != PERSISTED_STORE_SCHEMA_VERSION {
        return Err(anyhow!(
            "代码索引缓存 `{}` 的格式版本为 {}，当前只支持 {}",
            path.display(),
            stored.schema_version,
            PERSISTED_STORE_SCHEMA_VERSION
        ));
    }
    Ok(stored
        .values
        .into_iter()
        .flat_map(|(scope, values)| {
            values
                .into_iter()
                .map(move |(key, value)| ((scope.clone(), key), value))
        })
        .collect())
}

/// 把测试宿主键值原子写入磁盘，避免中断留下半份语义快照。
fn save_persisted_store(path: &Path, values: &HashMap<(String, String), Value>) -> Result<()> {
    let mut grouped: BTreeMap<String, BTreeMap<String, Value>> = BTreeMap::new();
    for ((scope, key), value) in values {
        grouped
            .entry(scope.clone())
            .or_default()
            .insert(key.clone(), value.clone());
    }
    let contents = serde_json::to_vec(&PersistedHostStore {
        schema_version: PERSISTED_STORE_SCHEMA_VERSION,
        values: grouped,
    })
    .map_err(|error| anyhow!("序列化代码索引缓存失败：{error}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("代码索引缓存路径缺少父目录：{}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| anyhow!("创建代码索引缓存目录 `{}` 失败：{error}", parent.display()))?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("代码索引缓存文件名无效：{}", path.display()))?;
    let sequence = PERSISTED_STORE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        sequence
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| {
            anyhow!(
                "创建代码索引临时缓存 `{}` 失败：{error}",
                temporary.display()
            )
        })?;
    let result = (|| -> Result<()> {
        file.write_all(&contents)
            .map_err(|error| anyhow!("写入代码索引缓存 `{}` 失败：{error}", temporary.display()))?;
        file.sync_all()
            .map_err(|error| anyhow!("同步代码索引缓存 `{}` 失败：{error}", temporary.display()))?;
        fs::rename(&temporary, path)
            .map_err(|error| anyhow!("替换代码索引缓存 `{}` 失败：{error}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// 构造带工作区根的测试激活上下文。
pub fn activation_context(workspace_root: &str) -> ActivationContext {
    ActivationContext {
        plugin_id: "code-context".into(),
        metadata: HashMap::from([("workspace_root".to_string(), workspace_root.to_string())]),
    }
}

impl PluginHostApi for TestHost {
    fn upsert_tool(&self, local_name: &str, _spec: &ToolSpec) -> Result<String> {
        self.tools.borrow_mut().push(local_name.to_string());
        Ok(local_name.to_string())
    }

    fn remove_tool(&self, public_name: &str) -> Result<()> {
        self.tools
            .borrow_mut()
            .retain(|registered| registered != public_name);
        Ok(())
    }

    fn upsert_prompt(&self, prompt: &PromptContribution) -> Result<String> {
        Ok(prompt.id.clone())
    }

    fn remove_prompt(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    fn emit_event(&self, event: &ExtensionEvent) -> Result<()> {
        self.events.borrow_mut().push(event.name.clone());
        self.event_payloads.borrow_mut().push(event.clone());
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
        self.ensure_scope(scope)?;
        Ok(self
            .values
            .borrow()
            .get(&(Self::scope_key(scope), key.to_string()))
            .cloned())
    }

    fn store_set(&self, scope: PluginStoreScope, key: &str, value: &Value) -> Result<()> {
        self.ensure_scope(scope)?;
        let mut values = self.values.borrow().clone();
        values.insert((Self::scope_key(scope), key.to_string()), value.clone());
        self.persist_values(&values)?;
        *self.values.borrow_mut() = values;
        Ok(())
    }

    fn store_remove(&self, scope: PluginStoreScope, key: &str) -> Result<Option<Value>> {
        self.ensure_scope(scope)?;
        let mut values = self.values.borrow().clone();
        let removed = values.remove(&(Self::scope_key(scope), key.to_string()));
        if removed.is_some() {
            self.persist_values(&values)?;
            *self.values.borrow_mut() = values;
        }
        Ok(removed)
    }

    fn store_list(&self, scope: PluginStoreScope) -> Result<Vec<String>> {
        self.ensure_scope(scope)?;
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
        Err(anyhow!("测试替身不提供插件服务调用"))
    }

    fn read_file(&self, _path: &str) -> Result<String> {
        Err(anyhow!("本插件当前阶段不应读取文件"))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>> {
        if !self.workspace_read_available {
            return Err(anyhow!("当前宿主未提供工作区只读能力"));
        }
        if path != "." {
            return Err(anyhow!("骨架阶段只允许探测工作区根目录"));
        }
        Ok(Vec::new())
    }

    fn spawn_process(&self, _spec: &ProcessSpec) -> Result<u64> {
        Err(anyhow!("本插件不申请子进程能力"))
    }

    fn write_process(&self, _handle: u64, _data: &str) -> Result<()> {
        Err(anyhow!("本插件不申请子进程能力"))
    }

    fn read_process_line(&self, _handle: u64, _timeout_ms: u64) -> Result<Option<String>> {
        Err(anyhow!("本插件不申请子进程能力"))
    }

    fn kill_process(&self, _handle: u64) -> Result<()> {
        Err(anyhow!("本插件不申请子进程能力"))
    }

    fn http_request(&self, request: &HttpRequest) -> Result<HttpResponse> {
        self.http_requests.borrow_mut().push(request.clone());
        if let Some(queued) = self.http_responses.borrow_mut().pop() {
            return queued;
        }
        if let Some(handler) = self.http_handler.as_ref() {
            return handler(request);
        }
        match self.echo_embedding_dimension {
            Some(dimension) => echo_embeddings(request, dimension),
            None => Err(anyhow!("测试替身没有更多预置 HTTP 应答")),
        }
    }
}

/// 带虚拟仓库的宿主替身。
///
/// 用真实目录做扫描测试会把断言绑死在测试运行时的工作目录上，而且无法构造"读取失败"
/// 与"列举失败"这类必须覆盖的错误分支。这里用一张路径到内容的表模拟仓库，目录结构由
/// 路径前缀推导，与宿主 `list_dir` 返回仓库相对路径且按路径排序的行为一致。
pub struct RepoHost {
    /// 复用已有替身处理存储、工具与事件。
    inner: TestHost,
    /// 仓库相对路径 → 文件内容。
    files: BTreeMap<String, String>,
    /// 读取时必定失败的路径，用于模拟二进制或权限问题。
    unreadable: HashSet<String>,
    /// 列举时必定失败的目录，用于模拟权限问题。
    unlistable: HashSet<String>,
    /// 已发生的宿主文件调用次数，用于验证分片预算真的被遵守。
    calls: RefCell<u32>,
}

impl RepoHost {
    /// 用一组 `(路径, 内容)` 构造虚拟仓库。
    pub fn new(files: &[(&str, &str)]) -> Self {
        Self {
            inner: TestHost::new(),
            files: files
                .iter()
                .map(|(path, content)| (path.to_string(), content.to_string()))
                .collect(),
            unreadable: HashSet::new(),
            unlistable: HashSet::new(),
            calls: RefCell::new(0),
        }
    }

    /// 用指定 JSON 文件构造可跨进程复用存储的虚拟仓库宿主。
    ///
    /// # Errors
    ///
    /// 缓存文件无法读取、JSON 损坏或格式版本不受支持时返回错误。
    pub fn persisted(files: &[(&str, &str)], path: impl Into<PathBuf>) -> Result<Self> {
        Ok(Self {
            inner: TestHost::persisted(path)?,
            files: files
                .iter()
                .map(|(path, content)| (path.to_string(), content.to_string()))
                .collect(),
            unreadable: HashSet::new(),
            unlistable: HashSet::new(),
            calls: RefCell::new(0),
        })
    }

    /// 用既有存储内容构造虚拟仓库，模拟进程重启后读到同一份磁盘数据。
    pub fn restored_from(files: &[(&str, &str)], values: HashMap<(String, String), Value>) -> Self {
        Self {
            inner: TestHost::restored_from(values),
            ..Self::new(files)
        }
    }

    /// 构造缺少项目作用域存储的虚拟仓库。
    pub fn without_workspace_scope(files: &[(&str, &str)]) -> Self {
        Self {
            inner: TestHost::without_workspace_scope(),
            ..Self::new(files)
        }
    }

    /// 返回该替身持有的存储内容。
    pub fn snapshot(&self) -> HashMap<(String, String), Value> {
        self.inner.snapshot()
    }

    /// 返回用户作用域中的全部键。
    pub fn user_keys(&self) -> Vec<String> {
        self.inner.user_keys()
    }

    /// 返回虚拟仓库宿主收到的扩展事件快照。
    pub fn emitted_event_payloads(&self) -> Vec<ExtensionEvent> {
        self.inner.emitted_event_payloads()
    }

    /// 预置按序返回的 HTTP 应答，供语义索引的端到端测试使用。
    pub fn with_http_responses(mut self, responses: Vec<Result<HttpResponse>>) -> Self {
        self.inner = self.inner.with_http_responses(responses);
        self
    }

    /// 让替身像真实嵌入端点那样按请求条数生成应答。
    pub fn with_echo_embeddings(mut self, dimension: usize) -> Self {
        self.inner = self.inner.with_echo_embeddings(dimension);
        self
    }

    /// 注入自定义 HTTP 处理函数，供可执行示例连接真实嵌入端点。
    pub fn with_http_handler(
        mut self,
        handler: impl Fn(&HttpRequest) -> Result<HttpResponse> + 'static,
    ) -> Self {
        self.inner = self.inner.with_http_handler(handler);
        self
    }

    /// 返回已收到的 HTTP 请求快照。
    pub fn http_requests(&self) -> Vec<HttpRequest> {
        self.inner.http_requests()
    }

    /// 标记一个文件为不可读，模拟二进制内容或权限失败。
    pub fn with_unreadable(mut self, path: &str) -> Self {
        self.unreadable.insert(path.to_string());
        self
    }

    /// 标记一个目录为不可列举，模拟权限失败。
    pub fn with_unlistable(mut self, dir: &str) -> Self {
        self.unlistable.insert(dir.to_string());
        self
    }

    /// 返回已发生的文件读取与目录列举总次数。
    pub fn file_calls(&self) -> u32 {
        *self.calls.borrow()
    }

    /// 把宿主请求路径规范化为仓库相对目录。
    fn normalize_dir(path: &str) -> String {
        if path == "." {
            return String::new();
        }
        path.trim_matches('/').to_string()
    }
}

impl PluginHostApi for RepoHost {
    fn upsert_tool(&self, local_name: &str, spec: &ToolSpec) -> Result<String> {
        self.inner.upsert_tool(local_name, spec)
    }

    fn remove_tool(&self, public_name: &str) -> Result<()> {
        self.inner.remove_tool(public_name)
    }

    fn upsert_prompt(&self, prompt: &PromptContribution) -> Result<String> {
        self.inner.upsert_prompt(prompt)
    }

    fn remove_prompt(&self, id: &str) -> Result<()> {
        self.inner.remove_prompt(id)
    }

    fn emit_event(&self, event: &ExtensionEvent) -> Result<()> {
        self.inner.emit_event(event)
    }

    fn get_state(&self, key: &str) -> Result<Option<Value>> {
        self.inner.get_state(key)
    }

    fn set_state(&self, key: &str, value: &Value) -> Result<()> {
        self.inner.set_state(key, value)
    }

    fn remove_state(&self, key: &str) -> Result<Option<Value>> {
        self.inner.remove_state(key)
    }

    fn store_get(&self, scope: PluginStoreScope, key: &str) -> Result<Option<Value>> {
        self.inner.store_get(scope, key)
    }

    fn store_set(&self, scope: PluginStoreScope, key: &str, value: &Value) -> Result<()> {
        self.inner.store_set(scope, key, value)
    }

    fn store_remove(&self, scope: PluginStoreScope, key: &str) -> Result<Option<Value>> {
        self.inner.store_remove(scope, key)
    }

    fn store_list(&self, scope: PluginStoreScope) -> Result<Vec<String>> {
        self.inner.store_list(scope)
    }

    fn upsert_service(&self, service: &ServiceSpec) -> Result<()> {
        self.inner.upsert_service(service)
    }

    fn remove_service(&self, name: &str) -> Result<()> {
        self.inner.remove_service(name)
    }

    fn list_services(&self, plugin_id: Option<&str>) -> Result<Vec<ServiceDescriptor>> {
        self.inner.list_services(plugin_id)
    }

    fn call_service(&self, plugin_id: &str, name: &str, payload: &Value) -> Result<Value> {
        self.inner.call_service(plugin_id, name, payload)
    }

    fn read_file(&self, path: &str) -> Result<String> {
        *self.calls.borrow_mut() += 1;
        let path = path.trim_start_matches("./");
        if self.unreadable.contains(path) {
            return Err(anyhow!("stream did not contain valid UTF-8"));
        }
        self.files
            .get(path)
            .cloned()
            .ok_or_else(|| anyhow!("读取插件文件失败：{path}"))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<FileEntry>> {
        *self.calls.borrow_mut() += 1;
        let dir = Self::normalize_dir(path);
        if self.unlistable.contains(&dir) {
            return Err(anyhow!("列出插件目录失败：{dir}"));
        }
        let prefix = if dir.is_empty() {
            String::new()
        } else {
            format!("{dir}/")
        };

        // 目录本身没有实体，用前缀是否命中任何文件来判断它是否存在。
        let mut entries: BTreeMap<String, bool> = BTreeMap::new();
        for file_path in self.files.keys() {
            let Some(rest) = file_path.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() {
                continue;
            }
            match rest.split_once('/') {
                Some((head, _)) => entries.insert(format!("{prefix}{head}"), true),
                None => entries.insert(format!("{prefix}{rest}"), false),
            };
        }
        if entries.is_empty() && !dir.is_empty() {
            return Err(anyhow!("插件请求的路径不是目录：{dir}"));
        }
        Ok(entries
            .into_iter()
            .map(|(path, is_dir)| FileEntry { path, is_dir })
            .collect())
    }

    fn spawn_process(&self, spec: &ProcessSpec) -> Result<u64> {
        self.inner.spawn_process(spec)
    }

    fn write_process(&self, handle: u64, data: &str) -> Result<()> {
        self.inner.write_process(handle, data)
    }

    fn read_process_line(&self, handle: u64, timeout_ms: u64) -> Result<Option<String>> {
        self.inner.read_process_line(handle, timeout_ms)
    }

    fn kill_process(&self, handle: u64) -> Result<()> {
        self.inner.kill_process(handle)
    }

    fn http_request(&self, request: &HttpRequest) -> Result<HttpResponse> {
        self.inner.http_request(request)
    }
}

/// 递归收集仓库中全部可索引文件，返回按路径排序的 `(仓库相对路径, 内容)`。
///
/// 只负责避开版本库元数据与构建产物；文件类型与敏感路径的判定复用生产代码的
/// [`crate::classify`]，使评测覆盖的文件集合与真实索引完全一致。读取失败的单个文件
/// 被跳过而不是中断收集——评测夹具不应因为一个二进制文件而整体失败。
///
/// 供基准测试、评测报告与可执行搜索示例共用，避免同一份遍历逻辑出现三份副本。
pub fn collect_repository_files(root: &Path) -> Vec<(String, String)> {
    /// 不参与索引的版本库元数据、依赖目录与构建产物。
    const SKIPPED: &[&str] = &[
        ".git",
        ".hg",
        ".svn",
        "target",
        "node_modules",
        "dist",
        "build",
        "out",
        "coverage",
        "vendor",
        "__pycache__",
        ".venv",
        ".idea",
        ".vscode",
        ".cache",
    ];

    fn walk(root: &Path, directory: &Path, output: &mut Vec<(String, String)>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                if !SKIPPED.contains(&name.as_ref()) {
                    walk(root, &path, output);
                }
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let relative = relative.to_string_lossy().replace('\\', "/");
            if !matches!(crate::classify(&relative), crate::PathClass::Indexable(_)) {
                continue;
            }
            if let Ok(content) = fs::read_to_string(&path) {
                output.push((relative, content));
            }
        }
    }

    let mut files = Vec::new();
    walk(root, root, &mut files);
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

/// 从插件的 `CARGO_MANIFEST_DIR` 推导 Lucia 仓库根目录。
///
/// 插件固定位于 `examples/plugins/<name>`，因此向上三级即仓库根。
pub fn repository_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("插件目录必须位于 Lucia 仓库的 examples/plugins 下")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 磁盘宿主必须在实例销毁后从同一路径恢复完整工作区键值。
    #[test]
    fn persisted_host_survives_reopen() {
        let sequence = PERSISTED_STORE_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "lucia-code-context-host-{}-{sequence}.json",
            std::process::id()
        ));
        let host = TestHost::persisted(&path).expect("创建磁盘宿主");
        host.store_set(
            PluginStoreScope::Workspace,
            "index.semantic",
            &serde_json::json!({"records": [1, 2, 3]}),
        )
        .expect("写入缓存");
        drop(host);

        let reopened = TestHost::persisted(&path).expect("重新打开磁盘宿主");
        assert_eq!(
            reopened
                .store_get(PluginStoreScope::Workspace, "index.semantic")
                .expect("读取缓存"),
            Some(serde_json::json!({"records": [1, 2, 3]}))
        );

        fs::remove_file(&path).expect("清理测试缓存");
    }
}

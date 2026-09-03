//! 单元测试共用的宿主替身。
//!
//! 只实现本插件真实使用的能力：当前工作区读取探测、持久存储、工具注册与事件发布。
//! 其余入口一律返回错误，使任何越界调用在测试中立即暴露，而不是悄悄拿到空结果。

use agent_plugin::{
    ActivationContext, ExtensionEvent, FileEntry, PluginHostApi, PluginStoreScope, ProcessSpec,
    PromptContribution, Result, ServiceDescriptor, ServiceSpec, ToolSpec,
};
use anyhow::anyhow;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

/// 记录调用的宿主替身。
#[derive(Default)]
pub struct TestHost {
    /// 按 (作用域, 键) 保存的持久存储内容。
    values: RefCell<HashMap<(String, String), Value>>,
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
        self.values
            .borrow_mut()
            .insert((Self::scope_key(scope), key.to_string()), value.clone());
        Ok(())
    }

    fn store_remove(&self, scope: PluginStoreScope, key: &str) -> Result<Option<Value>> {
        self.ensure_scope(scope)?;
        Ok(self
            .values
            .borrow_mut()
            .remove(&(Self::scope_key(scope), key.to_string())))
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
}

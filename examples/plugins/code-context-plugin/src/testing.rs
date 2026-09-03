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
use std::collections::HashMap;

/// 记录调用的宿主替身。
#[derive(Default)]
pub struct TestHost {
    /// 按 (作用域, 键) 保存的持久存储内容。
    values: RefCell<HashMap<(String, String), Value>>,
    /// 已注册的工具本地名。
    tools: RefCell<Vec<String>>,
    /// 已发布的扩展事件名。
    events: RefCell<Vec<String>>,
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

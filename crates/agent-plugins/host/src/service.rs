//! 插件间通用服务注册与调用。

use crate::audit::{HostServiceCallObservation, HostServiceCallObserver};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use semver::Version;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    future::Future,
    sync::{Arc, RwLock},
};

tokio::task_local! {
    /// 当前异步任务已经锁定的插件服务调用链。
    static SERVICE_CALL_STACK: RefCell<Vec<String>>;
}

/// 让新任务继承当前插件服务调用链，避免任务切换绕过同步循环检测。
///
/// 当前任务不在服务调用作用域内时直接执行原 Future；存在调用链时为 Future 建立独立
/// 副本，避免跨任务共享可变状态。
pub(crate) fn inherit_service_call_stack<F>(future: F) -> impl Future<Output = F::Output>
where
    F: Future,
{
    let inherited = SERVICE_CALL_STACK
        .try_with(|stack| stack.borrow().clone())
        .ok();
    async move {
        match inherited {
            Some(stack) => SERVICE_CALL_STACK.scope(RefCell::new(stack), future).await,
            None => future.await,
        }
    }
}

/// 一个插件公开的协议无关服务。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginService {
    /// 提供服务的可信插件 ID，由宿主注入。
    pub plugin_id: String,
    /// 提供方插件内稳定且唯一的服务名。
    pub name: String,
    /// 服务契约的语义化版本。
    pub version: String,
    /// 面向插件作者的可选说明。
    pub description: Option<String>,
}

/// 一次插件服务调用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginServiceCall {
    /// 调用方 ID；外部应用调用时由应用自行指定。
    pub caller_id: String,
    /// 目标插件 ID。
    pub plugin_id: String,
    /// 目标插件内的服务名。
    pub name: String,
    /// 服务自行定义的 JSON 请求。
    #[serde(default)]
    pub payload: Value,
}

/// WASM 实例处理服务调用的内部接口。
#[async_trait]
pub(crate) trait ServiceHandler: Send + Sync {
    /// 执行已路由到当前插件的服务调用。
    async fn handle(&self, call: PluginServiceCall) -> Result<Value>;
}

/// 多个 WASM 实例共享的服务目录和调用路由。
#[derive(Default)]
pub(crate) struct ServiceRegistry {
    services: RwLock<HashMap<(String, String), PluginService>>,
    handlers: RwLock<HashMap<String, Arc<dyn ServiceHandler>>>,
    declared_dependencies: RwLock<HashMap<String, HashSet<String>>>,
    observer: Option<Arc<dyn HostServiceCallObserver>>,
}

impl ServiceRegistry {
    /// 创建带可选旁路观察器的服务注册表。
    #[cfg(feature = "wasm")]
    pub(crate) fn new(observer: Option<Arc<dyn HostServiceCallObserver>>) -> Self {
        Self {
            services: RwLock::new(HashMap::new()),
            handlers: RwLock::new(HashMap::new()),
            declared_dependencies: RwLock::new(HashMap::new()),
            observer,
        }
    }

    /// 注册插件实例的调用处理器及其 manifest 声明的服务依赖。
    ///
    /// 依赖集合由 Host 从可信 manifest 注入；Guest 请求不能在运行时扩张该集合。
    pub(crate) fn register_handler(
        &self,
        plugin_id: impl Into<String>,
        declared_dependencies: HashSet<String>,
        handler: Arc<dyn ServiceHandler>,
    ) -> Result<()> {
        let plugin_id = plugin_id.into();
        let mut handlers = self
            .handlers
            .write()
            .map_err(|_| anyhow!("插件服务处理器锁已中毒"))?;
        if handlers.contains_key(&plugin_id) {
            return Err(anyhow!("插件 `{plugin_id}` 的服务处理器重复注册"));
        }
        let mut dependencies = self
            .declared_dependencies
            .write()
            .map_err(|_| anyhow!("插件服务依赖锁已中毒"))?;
        handlers.insert(plugin_id.clone(), handler);
        dependencies.insert(plugin_id, declared_dependencies);
        Ok(())
    }

    /// 注册或替换当前插件拥有的服务。
    pub(crate) fn upsert(&self, plugin_id: &str, mut service: PluginService) -> Result<()> {
        validate_service_name(&service.name)?;
        Version::parse(&service.version)
            .map_err(|error| anyhow!("服务 `{}` 的版本无效：{error}", service.name))?;
        service.plugin_id = plugin_id.to_string();
        self.services
            .write()
            .map_err(|_| anyhow!("插件服务目录锁已中毒"))?
            .insert((plugin_id.to_string(), service.name.clone()), service);
        Ok(())
    }

    /// 删除当前插件拥有的服务。
    pub(crate) fn remove(&self, plugin_id: &str, name: &str) -> Result<()> {
        validate_service_name(name)?;
        self.services
            .write()
            .map_err(|_| anyhow!("插件服务目录锁已中毒"))?
            .remove(&(plugin_id.to_string(), name.to_string()));
        Ok(())
    }

    /// 返回全部服务或指定插件的服务快照。
    pub(crate) fn list(&self, plugin_id: Option<&str>) -> Result<Vec<PluginService>> {
        let mut services = self
            .services
            .read()
            .map_err(|_| anyhow!("插件服务目录锁已中毒"))?
            .values()
            .filter(|service| plugin_id.is_none_or(|id| service.plugin_id == id))
            .cloned()
            .collect::<Vec<_>>();
        services.sort_by(|left, right| {
            (&left.plugin_id, &left.name).cmp(&(&right.plugin_id, &right.name))
        });
        Ok(services)
    }

    /// 返回当前 Guest 按 manifest 依赖有权发现的服务。
    pub(crate) fn list_for_plugin(
        &self,
        caller_id: &str,
        plugin_id: Option<&str>,
    ) -> Result<Vec<PluginService>> {
        let dependencies = self
            .declared_dependencies
            .read()
            .map_err(|_| anyhow!("插件服务依赖锁已中毒"))?;
        let allowed = dependencies
            .get(caller_id)
            .ok_or_else(|| anyhow!("插件 `{caller_id}` 尚未注册服务调用身份"))?;
        if let Some(target) = plugin_id {
            ensure_declared_dependency(caller_id, target, allowed)?;
        }
        let mut services = self
            .services
            .read()
            .map_err(|_| anyhow!("插件服务目录锁已中毒"))?
            .values()
            .filter(|service| {
                plugin_id.is_none_or(|target| service.plugin_id == target)
                    && (service.plugin_id == caller_id || allowed.contains(&service.plugin_id))
            })
            .cloned()
            .collect::<Vec<_>>();
        services.sort_by(|left, right| {
            (&left.plugin_id, &left.name).cmp(&(&right.plugin_id, &right.name))
        });
        Ok(services)
    }

    /// 调用目标插件已经注册的服务。
    pub(crate) async fn call(&self, call: PluginServiceCall) -> Result<Value> {
        let result = self.call_inner(call.clone()).await;
        if let Some(observer) = &self.observer {
            let observation = HostServiceCallObservation::from_result(&call, &result);
            let observer = observer.clone();
            // 审计是旁路证据，不允许第三方观察器 panic 改变真实服务调用结果。
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer.observe(observation);
            }));
        }
        result
    }

    /// 执行服务目录校验、循环检测和目标处理器调用。
    async fn call_inner(&self, call: PluginServiceCall) -> Result<Value> {
        let caller_is_plugin = self
            .handlers
            .read()
            .map_err(|_| anyhow!("插件服务处理器锁已中毒"))?
            .contains_key(&call.caller_id);
        if caller_is_plugin && call.caller_id != call.plugin_id {
            let dependencies = self
                .declared_dependencies
                .read()
                .map_err(|_| anyhow!("插件服务依赖锁已中毒"))?;
            let allowed = dependencies
                .get(&call.caller_id)
                .ok_or_else(|| anyhow!("插件 `{}` 尚未注册服务调用身份", call.caller_id))?;
            ensure_declared_dependency(&call.caller_id, &call.plugin_id, allowed)?;
        }
        let key = (call.plugin_id.clone(), call.name.clone());
        if !self
            .services
            .read()
            .map_err(|_| anyhow!("插件服务目录锁已中毒"))?
            .contains_key(&key)
        {
            return Err(anyhow!(
                "插件 `{}` 未注册服务 `{}`",
                call.plugin_id,
                call.name
            ));
        }
        let handler = self
            .handlers
            .read()
            .map_err(|_| anyhow!("插件服务处理器锁已中毒"))?
            .get(&call.plugin_id)
            .cloned()
            .ok_or_else(|| anyhow!("插件 `{}` 当前不可调用", call.plugin_id))?;
        if SERVICE_CALL_STACK.try_with(|_| ()).is_ok() {
            invoke_service(handler, call).await
        } else {
            let initial = caller_is_plugin
                .then(|| call.caller_id.clone())
                .into_iter()
                .collect();
            SERVICE_CALL_STACK
                .scope(RefCell::new(initial), invoke_service(handler, call))
                .await
        }
    }

    /// 移除插件实例及其全部服务。
    pub(crate) fn unregister_plugin(&self, plugin_id: &str) -> Result<()> {
        self.handlers
            .write()
            .map_err(|_| anyhow!("插件服务处理器锁已中毒"))?
            .remove(plugin_id);
        self.declared_dependencies
            .write()
            .map_err(|_| anyhow!("插件服务依赖锁已中毒"))?
            .remove(plugin_id);
        self.services
            .write()
            .map_err(|_| anyhow!("插件服务目录锁已中毒"))?
            .retain(|(owner, _), _| owner != plugin_id);
        Ok(())
    }
}

/// 校验目标服务 owner 已由调用方 manifest 显式声明。
fn ensure_declared_dependency(
    caller_id: &str,
    target_id: &str,
    allowed: &HashSet<String>,
) -> Result<()> {
    if caller_id == target_id || allowed.contains(target_id) {
        return Ok(());
    }
    Err(anyhow!(
        "插件 `{caller_id}` 的 manifest 未声明对插件 `{target_id}` 的依赖"
    ))
}

/// 在任务级调用栈中执行服务，阻止任何插件 store 被同步重入。
async fn invoke_service(
    handler: Arc<dyn ServiceHandler>,
    call: PluginServiceCall,
) -> Result<Value> {
    let target = call.plugin_id.clone();
    let cycle = SERVICE_CALL_STACK.with(|stack| stack.borrow().contains(&target));
    if cycle {
        let chain = SERVICE_CALL_STACK.with(|stack| stack.borrow().join(" -> "));
        return Err(anyhow!("插件服务出现同步循环调用：{chain} -> {target}"));
    }
    SERVICE_CALL_STACK.with(|stack| stack.borrow_mut().push(target));
    let result = handler.handle(call).await;
    SERVICE_CALL_STACK.with(|stack| {
        stack.borrow_mut().pop();
    });
    result
}

/// 校验服务名适合跨组件和配置文件传递。
fn validate_service_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 128 {
        return Err(anyhow!("插件服务名长度必须在 1 到 128 字节之间"));
    }
    if !name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(anyhow!(
            "插件服务名只能包含 ASCII 字母、数字、点、下划线和连字符"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Weak;

    /// 返回调用来源和请求数据的测试服务。
    struct EchoHandler;

    #[async_trait]
    impl ServiceHandler for EchoHandler {
        async fn handle(&self, call: PluginServiceCall) -> Result<Value> {
            Ok(json!({
                "caller": call.caller_id,
                "payload": call.payload,
            }))
        }
    }

    /// 在独立任务中继续调用另一个插件服务，用于覆盖任务切换后的调用链继承。
    struct SpawnedForwardingHandler {
        registry: Weak<ServiceRegistry>,
        target_plugin_id: &'static str,
        target_service_name: &'static str,
    }

    #[async_trait]
    impl ServiceHandler for SpawnedForwardingHandler {
        /// 把调用转发到目标插件，并保持当前服务调用链。
        async fn handle(&self, call: PluginServiceCall) -> Result<Value> {
            let registry = self
                .registry
                .upgrade()
                .ok_or_else(|| anyhow!("测试服务注册表已释放"))?;
            let nested_call = PluginServiceCall {
                caller_id: call.plugin_id,
                plugin_id: self.target_plugin_id.into(),
                name: self.target_service_name.into(),
                payload: call.payload,
            };
            tokio::spawn(inherit_service_call_stack(async move {
                registry.call(nested_call).await
            }))
            .await
            .map_err(|error| anyhow!("嵌套服务任务异常结束：{error}"))?
        }
    }

    /// 服务目录应按 owner 隔离注册，并把调用路由给目标插件。
    #[tokio::test]
    async fn service_registration_and_call_are_routed_by_owner() {
        let registry = ServiceRegistry::default();
        registry
            .register_handler("command", HashSet::new(), Arc::new(EchoHandler))
            .expect("处理器注册应成功");
        registry
            .upsert(
                "command",
                PluginService {
                    plugin_id: "伪造来源".into(),
                    name: "command.register".into(),
                    version: "1.0.0".into(),
                    description: Some("注册命令".into()),
                },
            )
            .expect("服务注册应成功");

        let services = registry.list(Some("command")).expect("服务查询应成功");
        assert_eq!(services[0].plugin_id, "command");
        let response = registry
            .call(PluginServiceCall {
                caller_id: "consumer".into(),
                plugin_id: "command".into(),
                name: "command.register".into(),
                payload: json!({"name": "hello"}),
            })
            .await
            .expect("服务调用应成功");
        assert_eq!(response["caller"], "consumer");
        assert_eq!(response["payload"]["name"], "hello");
    }

    /// Guest 只能调用并发现其 manifest 已声明依赖提供的服务。
    #[tokio::test]
    async fn plugin_service_access_requires_declared_dependency() {
        let registry = ServiceRegistry::default();
        registry
            .register_handler("provider", HashSet::new(), Arc::new(EchoHandler))
            .expect("提供方处理器注册应成功");
        registry
            .register_handler(
                "consumer",
                HashSet::from(["provider".to_string()]),
                Arc::new(EchoHandler),
            )
            .expect("消费方处理器注册应成功");
        registry
            .register_handler("intruder", HashSet::new(), Arc::new(EchoHandler))
            .expect("未授权调用方处理器注册应成功");
        registry
            .upsert(
                "provider",
                PluginService {
                    plugin_id: String::new(),
                    name: "identity.echo".into(),
                    version: "1.0.0".into(),
                    description: None,
                },
            )
            .expect("服务注册应成功");

        let response = registry
            .call(PluginServiceCall {
                caller_id: "consumer".into(),
                plugin_id: "provider".into(),
                name: "identity.echo".into(),
                payload: Value::Null,
            })
            .await
            .expect("已声明依赖应允许调用");
        assert_eq!(response["caller"], "consumer");
        assert_eq!(
            registry
                .list_for_plugin("consumer", Some("provider"))
                .expect("已声明依赖应允许发现服务")
                .len(),
            1
        );

        let error = registry
            .call(PluginServiceCall {
                caller_id: "intruder".into(),
                plugin_id: "provider".into(),
                name: "identity.echo".into(),
                payload: Value::Null,
            })
            .await
            .expect_err("未声明依赖必须拒绝调用");
        assert!(error.to_string().contains("manifest 未声明"));
        assert!(registry
            .list_for_plugin("intruder", None)
            .expect("无目标查询应返回授权子集")
            .is_empty());
        let error = registry
            .list_for_plugin("intruder", Some("provider"))
            .expect_err("未声明依赖必须拒绝定向发现");
        assert!(error.to_string().contains("manifest 未声明"));
    }

    /// 未注册服务不能仅凭存在处理器被调用。
    #[tokio::test]
    async fn unregistered_service_is_rejected() {
        let registry = ServiceRegistry::default();
        registry
            .register_handler("command", HashSet::new(), Arc::new(EchoHandler))
            .expect("处理器注册应成功");

        let error = registry
            .call(PluginServiceCall {
                caller_id: "consumer".into(),
                plugin_id: "command".into(),
                name: "missing".into(),
                payload: Value::Null,
            })
            .await
            .expect_err("未注册服务必须失败");
        assert!(error.to_string().contains("未注册服务"));
    }

    /// 已锁定的调用方不能同步调用自身服务。
    #[tokio::test]
    async fn synchronous_self_call_is_rejected() {
        let registry = ServiceRegistry::default();
        registry
            .register_handler("command", HashSet::new(), Arc::new(EchoHandler))
            .expect("处理器注册应成功");
        registry
            .upsert(
                "command",
                PluginService {
                    plugin_id: String::new(),
                    name: "command.execute".into(),
                    version: "1.0.0".into(),
                    description: None,
                },
            )
            .expect("服务注册应成功");

        let error = registry
            .call(PluginServiceCall {
                caller_id: "command".into(),
                plugin_id: "command".into(),
                name: "command.execute".into(),
                payload: Value::Null,
            })
            .await
            .expect_err("同步自调用必须失败");
        assert!(error.to_string().contains("同步循环调用"));
    }

    /// 跨任务的 A -> B -> A 服务调用仍必须保留完整调用链并拒绝循环。
    #[tokio::test]
    async fn spawned_cross_plugin_service_cycle_is_rejected() {
        let registry = Arc::new(ServiceRegistry::default());
        registry
            .register_handler(
                "a",
                HashSet::from(["b".to_string()]),
                Arc::new(SpawnedForwardingHandler {
                    registry: Arc::downgrade(&registry),
                    target_plugin_id: "b",
                    target_service_name: "b.forward",
                }),
            )
            .expect("插件 a 处理器注册应成功");
        registry
            .register_handler(
                "b",
                HashSet::from(["a".to_string()]),
                Arc::new(SpawnedForwardingHandler {
                    registry: Arc::downgrade(&registry),
                    target_plugin_id: "a",
                    target_service_name: "a.forward",
                }),
            )
            .expect("插件 b 处理器注册应成功");
        for (plugin_id, name) in [("a", "a.forward"), ("b", "b.forward")] {
            registry
                .upsert(
                    plugin_id,
                    PluginService {
                        plugin_id: String::new(),
                        name: name.into(),
                        version: "1.0.0".into(),
                        description: None,
                    },
                )
                .expect("转发服务注册应成功");
        }

        let error = registry
            .call(PluginServiceCall {
                caller_id: "external".into(),
                plugin_id: "a".into(),
                name: "a.forward".into(),
                payload: Value::Null,
            })
            .await
            .expect_err("跨任务循环调用必须失败");
        assert!(
            error.to_string().contains("a -> b -> a"),
            "循环错误应包含完整调用链，实际为：{error}"
        );
    }
}

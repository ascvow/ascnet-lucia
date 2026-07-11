//! 第三方 Lucia 插件使用的 Command 类型化 SDK。
//!
//! SDK 只依赖通用插件服务 API。它负责安装统一回调服务、注入 handler
//! 路由信息，以及把 Command Provider 的 JSON 服务转换为强类型结果。

use agent_plugin::{PluginHostApi, ServiceCall, ServiceSpec};
use anyhow::{anyhow, Context, Result};
use command_protocol::{
    canonical_command_name, encode_command_token, CommandCallbackRequest, CommandCallbackResponse,
    CommandCompletionRequest, CommandHandlerRef, CommandInvocation, CommandSnapshot, CommandSpec,
    CompletionItem, PrepareCompletionRequest, PrepareCompletionResponse, RegisterCommandRequest,
    RegisterCommandResponse, SnapshotRequest, UnregisterCommandRequest, UnregisterCommandResponse,
    CALLBACK_SERVICE, DEFAULT_COMPLETION_LIMIT, MAX_COMPLETION_LIMIT, PREPARE_COMPLETION_SERVICE,
    PROTOCOL_VERSION, PROVIDER_PLUGIN_ID, REGISTER_SERVICE, SNAPSHOT_SERVICE, UNREGISTER_SERVICE,
};
use serde_json::Value;
use std::collections::BTreeMap;

/// 第三方插件实现单个命令时使用的执行与补全接口。
pub trait CommandHandler: Send {
    /// 执行已经由 Command Provider 完成类型校验的命令。
    ///
    /// 返回值会原样交给调用方；耗时任务不应阻塞同步服务调用。
    fn execute(&mut self, invocation: CommandInvocation) -> Result<Value>;

    /// 根据当前输入返回动态候选，`insert_text` 填写原始参数值，默认不提供候选。
    fn complete(&mut self, _request: CommandCompletionRequest) -> Result<Vec<CompletionItem>> {
        Ok(Vec::new())
    }
}

/// 对 Command Provider 服务的无状态类型化客户端。
pub struct CommandClient<'a> {
    host: &'a dyn PluginHostApi,
    provider_id: &'a str,
}

impl<'a> CommandClient<'a> {
    /// 创建连接默认官方 Command Provider 的客户端。
    pub fn new(host: &'a dyn PluginHostApi) -> Self {
        Self {
            host,
            provider_id: PROVIDER_PLUGIN_ID,
        }
    }

    /// 创建连接指定 provider 插件 ID 的客户端，主要用于测试和兼容实现。
    pub fn with_provider(host: &'a dyn PluginHostApi, provider_id: &'a str) -> Self {
        Self { host, provider_id }
    }

    /// 注册或替换当前插件拥有的命令。
    pub fn register(&self, spec: &CommandSpec) -> Result<RegisterCommandResponse> {
        let payload = serde_json::to_value(RegisterCommandRequest { spec: spec.clone() })
            .context("序列化命令注册请求失败")?;
        let response = self
            .host
            .call_service(self.provider_id, REGISTER_SERVICE, &payload)
            .context("调用 Command 注册服务失败")?;
        serde_json::from_value(response).context("解析 Command 注册响应失败")
    }

    /// 注销当前插件拥有的命令；不存在时返回 `removed = false`。
    pub fn unregister(&self, name: &str) -> Result<UnregisterCommandResponse> {
        let payload = serde_json::to_value(UnregisterCommandRequest { name: name.into() })
            .context("序列化命令注销请求失败")?;
        let response = self
            .host
            .call_service(self.provider_id, UNREGISTER_SERVICE, &payload)
            .context("调用 Command 注销服务失败")?;
        serde_json::from_value(response).context("解析 Command 注销响应失败")
    }

    /// 获取当前命令注册表的完整只读快照。
    pub fn snapshot(&self) -> Result<CommandSnapshot> {
        let payload =
            serde_json::to_value(SnapshotRequest::default()).context("序列化命令快照请求失败")?;
        let response = self
            .host
            .call_service(self.provider_id, SNAPSHOT_SERVICE, &payload)
            .context("调用 Command 快照服务失败")?;
        serde_json::from_value(response).context("解析 Command 快照响应失败")
    }

    /// 显式请求 Provider 识别当前参数并生成受控候选计划。
    ///
    /// 调用方应在 Tab 或自行节流后调用，不应把该方法放入逐键输入热路径。
    pub fn prepare_completion(
        &self,
        request: &PrepareCompletionRequest,
    ) -> Result<PrepareCompletionResponse> {
        let payload = serde_json::to_value(request).context("序列化命令补全请求失败")?;
        let response = self
            .host
            .call_service(self.provider_id, PREPARE_COMPLETION_SERVICE, &payload)
            .context("调用 Command 补全准备服务失败")?;
        serde_json::from_value(response).context("解析 Command 补全准备响应失败")
    }
}

/// 保存第三方命令处理器并路由统一的 `command.callback` 服务。
#[derive(Default)]
pub struct CommandRouter {
    handlers: BTreeMap<String, Box<dyn CommandHandler>>,
    command_handlers: BTreeMap<String, String>,
    /// 把注册时的别名映射回规范名，确保任一名称注销都能释放处理器。
    command_aliases: BTreeMap<String, String>,
}

impl CommandRouter {
    /// 创建一个尚未安装回调服务的空路由器。
    pub fn new() -> Self {
        Self::default()
    }

    /// 向 Host 注册统一回调服务；插件应在 `activate` 中调用一次。
    pub fn install_callback_service(&self, host: &dyn PluginHostApi) -> Result<()> {
        host.upsert_service(&ServiceSpec {
            name: CALLBACK_SERVICE.into(),
            version: PROTOCOL_VERSION.into(),
            description: Some("执行并补全通过 Command SDK 注册的斜杠命令".into()),
        })
    }

    /// 注册命令定义和本地处理器。
    ///
    /// Provider 注册成功后才保存处理器；同一插件内 handler ID 冲突会在调用
    /// Provider 前返回错误，避免已有路由被静默替换。
    pub fn register<H>(
        &mut self,
        host: &dyn PluginHostApi,
        mut spec: CommandSpec,
        handler_id: impl Into<String>,
        handler: H,
    ) -> Result<RegisterCommandResponse>
    where
        H: CommandHandler + 'static,
    {
        let handler_id = handler_id.into();
        if handler_id.trim().is_empty() {
            return Err(anyhow!("Command handler ID 不能为空"));
        }
        let replaced_handler = self.command_handlers.get(&spec.name).cloned();
        if self.handlers.contains_key(&handler_id)
            && replaced_handler.as_deref() != Some(handler_id.as_str())
        {
            return Err(anyhow!("Command handler ID 重复：{handler_id}"));
        }

        spec.handler = Some(CommandHandlerRef {
            service: CALLBACK_SERVICE.into(),
            handler_id: handler_id.clone(),
        });
        let response = CommandClient::new(host).register(&spec)?;
        if let Some(previous) = replaced_handler {
            self.handlers.remove(&previous);
        }
        self.command_aliases
            .retain(|_, canonical| canonical != &response.name);
        for alias in &spec.aliases {
            self.command_aliases
                .insert(alias.clone(), response.name.clone());
        }
        self.handlers.insert(handler_id.clone(), Box::new(handler));
        self.command_handlers
            .insert(response.name.clone(), handler_id);
        Ok(response)
    }

    /// 注销命令，并在 Provider 确认移除后释放本地处理器。
    pub fn unregister(
        &mut self,
        host: &dyn PluginHostApi,
        name: &str,
    ) -> Result<UnregisterCommandResponse> {
        let normalized = canonical_command_name(name).unwrap_or_else(|| name.into());
        let canonical = self
            .command_aliases
            .get(&normalized)
            .cloned()
            .unwrap_or(normalized);
        let response = CommandClient::new(host).unregister(name)?;
        if response.removed {
            if let Some(handler_id) = self.command_handlers.remove(&canonical) {
                self.handlers.remove(&handler_id);
            }
            self.command_aliases
                .retain(|_, registered| registered != &canonical);
        }
        Ok(response)
    }

    /// 处理 Host 路由到 `command.callback` 的服务调用。
    ///
    /// 插件可在自己的 `AgentPlugin::handle_service` 中直接返回本方法结果。
    pub fn handle_service(&mut self, call: ServiceCall) -> Result<Value> {
        if call.name != CALLBACK_SERVICE {
            return Err(anyhow!("未知 Command SDK 服务：{}", call.name));
        }
        if call.caller_id != PROVIDER_PLUGIN_ID {
            return Err(anyhow!(
                "调用方 `{}` 无权执行 Command SDK 回调",
                call.caller_id
            ));
        }
        let request: CommandCallbackRequest =
            serde_json::from_value(call.payload).context("解析 Command 回调请求失败")?;
        let response = match request {
            CommandCallbackRequest::Execute {
                handler_id,
                invocation,
            } => {
                let handler = self.handler_mut(&handler_id)?;
                CommandCallbackResponse::Executed {
                    result: handler.execute(invocation)?,
                }
            }
            CommandCallbackRequest::Complete {
                handler_id,
                request,
            } => {
                let limit = if request.limit == 0 {
                    DEFAULT_COMPLETION_LIMIT
                } else {
                    request.limit.min(MAX_COMPLETION_LIMIT)
                };
                let handler = self.handler_mut(&handler_id)?;
                let mut items = handler.complete(request)?;
                items.truncate(usize::from(limit));
                for item in &mut items {
                    item.insert_text = encode_command_token(&item.insert_text);
                }
                CommandCallbackResponse::Completed { items }
            }
        };
        serde_json::to_value(response).context("序列化 Command 回调响应失败")
    }

    /// 返回当前路由器保存的处理器数量。
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }

    /// 查找可变处理器，并隐藏具体 trait object 生命周期。
    fn handler_mut(&mut self, handler_id: &str) -> Result<&mut (dyn CommandHandler + '_)> {
        match self.handlers.get_mut(handler_id) {
            Some(handler) => Ok(handler.as_mut()),
            None => Err(anyhow!("未找到 Command handler：{handler_id}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_plugin::{
        AgentContinueRequest, AgentHandle, AgentId, AgentOutcome, AgentSnapshot, AgentSpawnRequest,
        FileEntry, ProcessSpec, PromptContribution, ServiceDescriptor, ToolSpec,
    };
    use command_protocol::{ArgumentKind, ArgumentSpec};
    use std::cell::RefCell;

    /// 记录 SDK 发起服务调用的最小 Host。
    #[derive(Default)]
    struct RecordingHost {
        services: RefCell<Vec<ServiceSpec>>,
        calls: RefCell<Vec<(String, String, Value)>>,
    }

    impl PluginHostApi for RecordingHost {
        fn upsert_tool(&self, _local_name: &str, _spec: &ToolSpec) -> Result<String> {
            unreachable!("测试不会注册工具")
        }

        fn remove_tool(&self, _public_name: &str) -> Result<()> {
            Ok(())
        }

        fn upsert_prompt(&self, _prompt: &PromptContribution) -> Result<String> {
            unreachable!("测试不会注册提示")
        }

        fn remove_prompt(&self, _id: &str) -> Result<()> {
            Ok(())
        }

        fn emit_event(&self, _event: &agent_plugin::ExtensionEvent) -> Result<()> {
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

        fn upsert_service(&self, service: &ServiceSpec) -> Result<()> {
            self.services.borrow_mut().push(service.clone());
            Ok(())
        }

        fn remove_service(&self, _name: &str) -> Result<()> {
            Ok(())
        }

        fn list_services(&self, _plugin_id: Option<&str>) -> Result<Vec<ServiceDescriptor>> {
            Ok(Vec::new())
        }

        fn call_service(&self, plugin_id: &str, name: &str, payload: &Value) -> Result<Value> {
            self.calls
                .borrow_mut()
                .push((plugin_id.into(), name.into(), payload.clone()));
            match name {
                REGISTER_SERVICE => Ok(serde_json::json!({
                    "name": payload["spec"]["name"],
                    "generation": 2
                })),
                UNREGISTER_SERVICE => Ok(serde_json::json!({
                    "removed": true,
                    "generation": 3
                })),
                PREPARE_COMPLETION_SERVICE => Ok(serde_json::json!({
                    "type": "no_match"
                })),
                _ => unreachable!("测试收到未预期服务：{name}"),
            }
        }

        fn read_file(&self, _path: &str) -> Result<String> {
            unreachable!("测试不会读取文件")
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<FileEntry>> {
            unreachable!("测试不会读取目录")
        }

        fn spawn_process(&self, _spec: &ProcessSpec) -> Result<u64> {
            unreachable!("测试不会启动进程")
        }

        fn write_process(&self, _handle: u64, _data: &str) -> Result<()> {
            unreachable!("测试不会写进程")
        }

        fn read_process_line(&self, _handle: u64, _timeout_ms: u64) -> Result<Option<String>> {
            unreachable!("测试不会读进程")
        }

        fn kill_process(&self, _handle: u64) -> Result<()> {
            unreachable!("测试不会终止进程")
        }

        fn agent_identity(&self) -> Result<AgentId> {
            unreachable!("测试不会读取 Agent 身份")
        }

        fn spawn_agent(&self, _request: &AgentSpawnRequest) -> Result<AgentHandle> {
            unreachable!("测试不会创建 Agent")
        }

        fn continue_agent(&self, _request: &AgentContinueRequest) -> Result<AgentHandle> {
            unreachable!("测试不会继续 Agent")
        }

        fn agent_status(&self, _target: &AgentId) -> Result<AgentSnapshot> {
            unreachable!("测试不会读取 Agent 状态")
        }

        fn agent_result(&self, _target: &AgentId) -> Result<Option<AgentOutcome>> {
            unreachable!("测试不会读取 Agent 结果")
        }

        fn cancel_agent(&self, _target: &AgentId) -> Result<bool> {
            unreachable!("测试不会取消 Agent")
        }
    }

    /// 返回传入参数的测试命令处理器。
    struct EchoHandler;

    impl CommandHandler for EchoHandler {
        fn execute(&mut self, invocation: CommandInvocation) -> Result<Value> {
            Ok(serde_json::json!({"arguments": invocation.arguments}))
        }

        fn complete(&mut self, request: CommandCompletionRequest) -> Result<Vec<CompletionItem>> {
            Ok(vec![
                CompletionItem {
                    label: format!("{}-one", request.prefix),
                    insert_text: r#"one two "quoted" \ path"#.into(),
                    description: None,
                },
                CompletionItem {
                    label: format!("{}-two", request.prefix),
                    insert_text: "two".into(),
                    description: None,
                },
            ])
        }
    }

    /// 验证 SDK 注入回调目标并能执行类型化回调。
    #[test]
    fn registers_and_routes_handler() {
        let host = RecordingHost::default();
        let mut router = CommandRouter::new();
        router
            .install_callback_service(&host)
            .expect("应注册回调服务");
        let spec = CommandSpec::new("echo", "回显", "回显文本")
            .with_argument(ArgumentSpec::required("text", "文本", ArgumentKind::String));
        router
            .register(&host, spec, "echo-handler", EchoHandler)
            .expect("应注册命令");

        let payload = serde_json::to_value(CommandCallbackRequest::Execute {
            handler_id: "echo-handler".into(),
            invocation: CommandInvocation {
                command: "echo".into(),
                input: "/echo hi".into(),
                arguments: BTreeMap::from([("text".into(), vec!["hi".into()])]),
            },
        })
        .expect("请求可序列化");
        let response = router
            .handle_service(ServiceCall {
                caller_id: "command".into(),
                name: CALLBACK_SERVICE.into(),
                payload: payload.clone(),
            })
            .expect("应执行回调");

        assert_eq!(router.handler_count(), 1);
        assert_eq!(response["type"], "executed");
        assert_eq!(response["result"]["arguments"]["text"][0], "hi");
        assert_eq!(host.services.borrow()[0].name, CALLBACK_SERVICE);
        assert_eq!(host.calls.borrow()[0].0, PROVIDER_PLUGIN_ID);
        assert_eq!(host.calls.borrow()[0].1, REGISTER_SERVICE);
        assert_eq!(
            host.calls.borrow()[0].2["spec"]["handler"]["service"],
            CALLBACK_SERVICE
        );
        let error = router
            .handle_service(ServiceCall {
                caller_id: "other-plugin".into(),
                name: CALLBACK_SERVICE.into(),
                payload,
            })
            .expect_err("非 Command 调用方必须被拒绝");
        assert!(error.to_string().contains("无权执行"));
    }

    /// 验证通过别名注销时会同时释放规范命令对应的本地处理器。
    #[test]
    fn unregistering_alias_releases_canonical_handler() {
        let host = RecordingHost::default();
        let mut router = CommandRouter::new();
        let spec = CommandSpec::new("echo", "回显", "回显文本").with_alias("say");
        router
            .register(&host, spec, "echo-handler", EchoHandler)
            .expect("应注册带别名的命令");
        assert_eq!(router.handler_count(), 1);

        let response = router.unregister(&host, "say").expect("通过别名注销应成功");
        assert!(response.removed);
        assert_eq!(router.handler_count(), 0);

        let error = router
            .handle_service(ServiceCall {
                caller_id: PROVIDER_PLUGIN_ID.into(),
                name: CALLBACK_SERVICE.into(),
                payload: serde_json::to_value(CommandCallbackRequest::Execute {
                    handler_id: "echo-handler".into(),
                    invocation: CommandInvocation {
                        command: "echo".into(),
                        input: "/echo value".into(),
                        arguments: BTreeMap::new(),
                    },
                })
                .expect("请求应可序列化"),
            })
            .expect_err("注销后不应保留规范命令处理器");
        assert!(error.to_string().contains("未找到 Command handler"));
    }

    /// 验证 SDK 客户端显式调用补全服务且不会在快照读取时隐式触发。
    #[test]
    fn explicitly_requests_completion_plan() {
        let host = RecordingHost::default();
        let request = PrepareCompletionRequest {
            input: "/echo o".into(),
            cursor: None,
            limit: 5,
        };
        let response = CommandClient::new(&host)
            .prepare_completion(&request)
            .expect("补全计划调用应成功");
        assert_eq!(response, PrepareCompletionResponse::NoMatch);
        assert_eq!(host.calls.borrow().len(), 1);
        assert_eq!(host.calls.borrow()[0].1, PREPARE_COMPLETION_SERVICE);
        assert_eq!(host.calls.borrow()[0].2["limit"], 5);
    }

    /// 验证动态补全仍只接受 Provider caller，并按请求上限截断候选。
    #[test]
    fn routes_dynamic_completion_with_requested_limit() {
        let host = RecordingHost::default();
        let mut router = CommandRouter::new();
        router
            .register(
                &host,
                CommandSpec::new("echo", "回显", "回显文本"),
                "echo-handler",
                EchoHandler,
            )
            .expect("应注册命令");
        let response = router
            .handle_service(ServiceCall {
                caller_id: PROVIDER_PLUGIN_ID.into(),
                name: CALLBACK_SERVICE.into(),
                payload: serde_json::to_value(CommandCallbackRequest::Complete {
                    handler_id: "echo-handler".into(),
                    request: CommandCompletionRequest {
                        command: "echo".into(),
                        argument: "text".into(),
                        prefix: "o".into(),
                        input: "/echo o".into(),
                        limit: 1,
                    },
                })
                .expect("请求应可序列化"),
            })
            .expect("Provider 应能调用动态补全");
        assert_eq!(response["type"], "completed");
        assert_eq!(response["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(response["items"][0]["label"], "o-one");
        assert_eq!(
            response["items"][0]["insert_text"],
            encode_command_token(r#"one two "quoted" \ path"#)
        );
    }
}

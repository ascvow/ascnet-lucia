//! 插件宿主组合、动态加载与上下文加载的回归测试。

use super::*;
use crate::ui::{ToolRenderState, UiInputEvent, UiPlacement, UiSize};
use agent_core::model::ModelMessage;
use agent_core::{ContextLoadRequest, ContextLoader, LoadedContext};
use agent_tool::{ExecutionPolicy, ToolCall, ToolPermissionScope, ToolResult, ToolSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
#[cfg(feature = "wasm")]
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// 宿主执行策略一旦收紧，后续 Serve 请求不得恢复进程能力。
#[test]
fn plugin_host_execution_policy_cannot_be_widened() {
    let services = PluginHostServices::new()
        .restrict_execution_policy(&ExecutionPolicy::evaluation("."))
        .restrict_execution_policy(&ExecutionPolicy::serve());

    assert!(!services.execution_policy.allow_process);
}

/// 激活元数据必须按可信插件 ID 隔离，未指定的插件不能读取其他实例配置。
#[cfg(feature = "wasm")]
#[test]
fn activation_metadata_is_scoped_to_plugin_id() {
    let services = PluginHostServices::new()
        .with_activation_metadata(
            "context",
            HashMap::from([("policy".into(), "fixed".into())]),
        )
        .expect("合法激活元数据应被接受");

    assert_eq!(
        services
            .activation_metadata("context")
            .get("policy")
            .map(String::as_str),
        Some("fixed")
    );
    assert!(services.activation_metadata("other").is_empty());
}

/// 空插件 ID 或空元数据键不能进入 Host 激活上下文。
#[cfg(feature = "wasm")]
#[test]
fn activation_metadata_rejects_unaddressable_keys() {
    assert!(PluginHostServices::new()
        .with_activation_metadata(" ", HashMap::new())
        .is_err());
    assert!(PluginHostServices::new()
        .with_activation_metadata("context", HashMap::from([(" ".into(), "value".into())]))
        .is_err());
}

/// 记录调用次数的测试插件宿主。
struct CountingPluginHost {
    id: &'static str,
    tool: ToolSpec,
    calls: Arc<AtomicUsize>,
}

/// 将任意工具重写为指定名称的测试宿主。
struct RewritePluginHost {
    id: &'static str,
    target: &'static str,
}

#[async_trait]
impl AgentExtension for RewritePluginHost {
    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        Ok(ToolDecision::Rewrite {
            call: ToolCall::new("伪造调用 ID", self.target, call.args.clone()),
        })
    }
}

#[async_trait]
impl PluginHost for RewritePluginHost {
    fn id(&self) -> Option<&str> {
        Some(self.id)
    }
}

/// 只允许看到最终工具名的测试策略宿主。
struct FinalPolicyPluginHost;

/// 记录最终策略收到的工具名与权限 scope，验证组合宿主的可信绑定顺序。
struct CapturingScopePolicyPluginHost {
    observed: Arc<std::sync::Mutex<Vec<(String, String, ToolPermissionScope, Option<String>)>>>,
}

#[async_trait]
impl AgentExtension for CapturingScopePolicyPluginHost {
    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        self.observed
            .lock()
            .expect("权限 scope 观察锁不应中毒")
            .push((
                call.id.clone(),
                call.name.clone(),
                call.permission_scope,
                call.run_id.clone(),
            ));
        Ok(ToolDecision::Allow)
    }
}

#[async_trait]
impl PluginHost for CapturingScopePolicyPluginHost {
    fn id(&self) -> Option<&str> {
        Some("scope-policy")
    }
}

#[async_trait]
impl AgentExtension for FinalPolicyPluginHost {
    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        if call.name == "rewritten" {
            Ok(ToolDecision::block("最终策略已检查重写调用"))
        } else {
            Ok(ToolDecision::Allow)
        }
    }
}

#[async_trait]
impl PluginHost for FinalPolicyPluginHost {
    fn id(&self) -> Option<&str> {
        Some("policy")
    }
}

#[async_trait]
impl AgentExtension for CountingPluginHost {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(vec![self.tool.clone()])
    }

    async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(ToolResult::success(
            call.id,
            call.name,
            json!({"owner": self.tool.name}),
        )))
    }
}

#[async_trait]
impl PluginHost for CountingPluginHost {
    fn id(&self) -> Option<&str> {
        Some(self.id)
    }
}

/// 记录组合宿主清理顺序并可注入失败，验证权限插件不会被前序错误跳过。
struct CleanupPluginHost {
    id: &'static str,
    calls: Arc<Mutex<Vec<String>>>,
    fail: bool,
}

#[async_trait]
impl AgentExtension for CleanupPluginHost {
    async fn finish_run(&self, run_id: &str) -> Result<()> {
        self.calls
            .lock()
            .expect("插件清理调用锁不应中毒")
            .push(format!("{}:{run_id}", self.id));
        if self.fail {
            Err(anyhow::anyhow!("{} 清理失败", self.id))
        } else {
            Ok(())
        }
    }
}

#[async_trait]
impl PluginHost for CleanupPluginHost {
    fn id(&self) -> Option<&str> {
        Some(self.id)
    }
}

/// 为 Live Host 注入最终事件排空和异常清理故障的测试插件。
struct RetryableRunCleanupPluginHost {
    id: &'static str,
    finish_events: Arc<AtomicUsize>,
    drain_calls: Arc<AtomicUsize>,
    cleanup_calls: Arc<AtomicUsize>,
    fail_next_drain: Arc<AtomicBool>,
    fail_next_cleanup: Arc<AtomicBool>,
}

#[async_trait]
impl AgentExtension for RetryableRunCleanupPluginHost {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(vec![ToolSpec::new(
            "cleanup_probe",
            "运行清理探针",
            ToolSpec::empty_object_schema(),
        )])
    }

    async fn on_event(&self, event: &AgentEvent) -> Result<()> {
        if event.kind == AgentEventKind::RunFinished {
            self.finish_events.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }

    async fn drain_events_for_run(&self, _run_id: &str) -> Result<Vec<serde_json::Value>> {
        self.drain_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_next_drain.swap(false, Ordering::SeqCst) {
            return Err(anyhow::anyhow!("测试最终事件排空失败"));
        }
        Ok(Vec::new())
    }

    async fn finish_run(&self, _run_id: &str) -> Result<()> {
        self.cleanup_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_next_cleanup.swap(false, Ordering::SeqCst) {
            return Err(anyhow::anyhow!("测试异常清理失败"));
        }
        Ok(())
    }
}

#[async_trait]
impl PluginHost for RetryableRunCleanupPluginHost {
    fn id(&self) -> Option<&str> {
        Some(self.id)
    }
}

/// 首次清理永久等待、后续清理成功，用于模拟 Runtime 超时丢弃清理 future。
struct CancellableRunCleanupPluginHost {
    cleanup_calls: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl AgentExtension for CancellableRunCleanupPluginHost {
    async fn finish_run(&self, _run_id: &str) -> Result<()> {
        if self.cleanup_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            self.entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

#[async_trait]
impl PluginHost for CancellableRunCleanupPluginHost {
    fn id(&self) -> Option<&str> {
        Some("cancellable-cleanup")
    }
}

/// 记录动态运行快照路由结果的测试插件宿主。
struct SnapshotPluginHost {
    id: &'static str,
    tool_name: &'static str,
    prompt: &'static str,
    context: &'static str,
    deny_tool: Option<&'static str>,
    calls: Arc<Mutex<Vec<String>>>,
    before_calls: Arc<Mutex<Vec<String>>>,
    after_calls: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<String>>>,
    pending_events: Arc<Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>>>,
    finished_runs: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl AgentExtension for SnapshotPluginHost {
    async fn prompt_messages(&self) -> Result<Vec<ModelMessage>> {
        Ok(vec![ModelMessage::text(
            agent_core::MessageRole::Developer,
            self.prompt,
        )])
    }

    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(vec![ToolSpec::new(
            self.tool_name,
            "运行快照测试工具",
            ToolSpec::empty_object_schema(),
        )])
    }

    async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
        self.calls
            .lock()
            .expect("工具调用观察锁不应中毒")
            .push(call.run_id.clone().unwrap_or_default());
        Ok(Some(ToolResult::success(
            call.id,
            call.name,
            json!({"plugin": self.id}),
        )))
    }

    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        self.before_calls
            .lock()
            .expect("前置调用观察锁不应中毒")
            .push(call.run_id.clone().unwrap_or_default());
        if self.deny_tool == Some(call.name.as_str()) {
            return Ok(ToolDecision::block("快照策略拒绝测试工具"));
        }
        Ok(ToolDecision::Allow)
    }

    async fn after_tool(&self, result: &ToolResult) -> Result<()> {
        self.after_calls
            .lock()
            .expect("后置调用观察锁不应中毒")
            .push(result.call_id.clone());
        Ok(())
    }

    async fn on_event(&self, event: &AgentEvent) -> Result<()> {
        if event.kind == AgentEventKind::TurnStarted {
            self.events
                .lock()
                .expect("生命周期事件观察锁不应中毒")
                .push(event.run_id.clone());
            self.pending_events
                .lock()
                .expect("扩展事件队列锁不应中毒")
                .entry(event.run_id.clone())
                .or_default()
                .push(json!({"plugin": self.id, "run_id": event.run_id}));
        }
        Ok(())
    }

    async fn drain_events(&self) -> Result<Vec<serde_json::Value>> {
        Ok(Vec::new())
    }

    async fn drain_events_for_run(&self, run_id: &str) -> Result<Vec<serde_json::Value>> {
        Ok(self
            .pending_events
            .lock()
            .expect("扩展事件队列锁不应中毒")
            .remove(run_id)
            .unwrap_or_default())
    }

    async fn finish_run(&self, run_id: &str) -> Result<()> {
        self.finished_runs
            .lock()
            .expect("运行清理观察锁不应中毒")
            .push(run_id.to_string());
        Ok(())
    }
}

#[async_trait]
impl PluginHost for SnapshotPluginHost {
    fn id(&self) -> Option<&str> {
        Some(self.id)
    }

    async fn load_context(&self, request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        Ok(Some(LoadedContext::new(
            request.system.clone(),
            vec![ModelMessage::text(
                agent_core::MessageRole::Developer,
                self.context,
            )],
        )))
    }
}

/// 公开一个视图并分别记录渲染与输入次数的测试插件宿主。
struct CountingUiPluginHost {
    id: &'static str,
    declared_plugin_id: &'static str,
    view_id: &'static str,
    render_calls: Arc<AtomicUsize>,
    input_calls: Arc<AtomicUsize>,
}

/// 声明工具消息 renderer 的测试插件宿主。
struct ToolRendererPluginHost {
    id: &'static str,
    owned_tool: &'static str,
    rendered_tool: &'static str,
    render_calls: Arc<AtomicUsize>,
    /// 可选地保存 Host 实际交给 Guest renderer 的请求副本。
    rendered_requests: Option<Arc<Mutex<Vec<ToolRenderRequest>>>>,
}

#[async_trait]
impl AgentExtension for ToolRendererPluginHost {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(vec![ToolSpec::new(
            self.owned_tool,
            "测试工具",
            ToolSpec::empty_object_schema(),
        )])
    }
}

#[async_trait]
impl PluginHost for ToolRendererPluginHost {
    fn id(&self) -> Option<&str> {
        Some(self.id)
    }

    async fn tool_renderers(&self) -> Result<Vec<ToolRendererContribution>> {
        Ok(vec![ToolRendererContribution {
            plugin_id: "伪造身份".into(),
            renderer_id: "tool-message".into(),
            tool_name: self.rendered_tool.into(),
        }])
    }

    async fn render_tool(&self, request: &ToolRenderRequest) -> Result<Option<UiFrame>> {
        self.render_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(requests) = &self.rendered_requests {
            requests
                .lock()
                .expect("工具 renderer 请求锁不应中毒")
                .push(request.clone());
        }
        Ok(Some(UiFrame {
            view_id: request.renderer_id.clone(),
            visible: true,
            lines: Vec::new(),
            cursor: None,
        }))
    }
}

#[async_trait]
impl AgentExtension for CountingUiPluginHost {}

#[async_trait]
impl PluginHost for CountingUiPluginHost {
    /// 返回组合宿主用于覆盖插件自报身份的可信 ID。
    fn id(&self) -> Option<&str> {
        Some(self.id)
    }

    /// 返回带有不可信插件 ID 的声明，用于验证宿主会覆盖该字段。
    async fn ui_declarations(&self) -> Result<Vec<UiDeclaration>> {
        Ok(vec![UiDeclaration {
            plugin_id: self.declared_plugin_id.to_string(),
            view_id: self.view_id.to_string(),
            title: format!("{} 测试视图", self.id),
            placement: UiPlacement::Right,
            size: UiSize::default(),
            focusable: true,
            input_triggers: Vec::new(),
        }])
    }

    /// 记录一次 owner 渲染调用并返回最小可见帧。
    async fn render_ui(&self, _request: &UiRenderRequest) -> Result<Option<UiFrame>> {
        self.render_calls.fetch_add(1, Ordering::SeqCst);
        Ok(Some(UiFrame {
            view_id: self.view_id.to_string(),
            visible: true,
            lines: Vec::new(),
            cursor: None,
        }))
    }

    /// 记录一次 owner 输入调用。
    async fn on_ui_input(&self, _input: &UiInput) -> Result<()> {
        self.input_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// 构造指向指定插件视图的稳定渲染请求。
fn ui_render_request(plugin_id: &str, view_id: &str) -> UiRenderRequest {
    UiRenderRequest {
        plugin_id: plugin_id.to_string(),
        view_id: view_id.to_string(),
        instance_id: None,
        width: 40,
        height: 12,
        focused: true,
        frame: 1,
    }
}

/// 构造指向指定插件视图的稳定按键输入。
fn ui_input(plugin_id: &str, view_id: &str) -> UiInput {
    UiInput {
        plugin_id: plugin_id.to_string(),
        view_id: view_id.to_string(),
        instance_id: None,
        event: UiInputEvent::Key {
            code: "enter".to_string(),
            modifiers: Vec::new(),
        },
    }
}

/// 返回固定摘要的测试上下文宿主。
struct SummaryPluginHost {
    id: &'static str,
}

#[async_trait]
impl AgentExtension for SummaryPluginHost {}

#[async_trait]
impl PluginHost for SummaryPluginHost {
    fn id(&self) -> Option<&str> {
        Some(self.id)
    }

    async fn load_context(&self, request: &ContextLoadRequest) -> Result<Option<LoadedContext>> {
        Ok(Some(LoadedContext::new(
            request.system.clone(),
            vec![ModelMessage::text(
                agent_core::MessageRole::Developer,
                "插件压缩后的摘要",
            )],
        )))
    }
}

/// 组合宿主应只把上下文请求派发给显式选择的独占 owner。
#[tokio::test]
async fn selected_context_owner_replaces_model_messages() {
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(SummaryPluginHost { id: "summary" }));
    host.set_capability_owner(manifest::CONTEXT_LOADER_CAPABILITY, "summary");
    let request = ContextLoadRequest {
        run_id: "run-1".into(),
        step: 0,
        user_initiated: false,
        provider: "test".into(),
        model: "test-model".into(),
        system: Some("系统提示".into()),
        messages: vec![ModelMessage::text(
            agent_core::MessageRole::User,
            "不应发送的完整历史",
        )],
    };

    let loaded = ContextLoader::load(&host, request)
        .await
        .expect("上下文 owner 应完成替换");
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].text_content(), "插件压缩后的摘要");
}

/// 已选中的上下文 owner 返回 `None` 时必须显式透传本轮原始上下文。
#[tokio::test]
async fn selected_context_owner_can_pass_through_original_context() {
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(CountingPluginHost {
        id: "empty-context",
        tool: ToolSpec::new("unused", "未使用工具", json!({"type": "object"})),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    host.set_capability_owner(manifest::CONTEXT_LOADER_CAPABILITY, "empty-context");
    let request = ContextLoadRequest {
        run_id: "run-1".into(),
        step: 0,
        user_initiated: false,
        provider: "test".into(),
        model: "test-model".into(),
        system: Some("系统提示".into()),
        messages: vec![ModelMessage::text(
            agent_core::MessageRole::User,
            "需要透传的完整历史",
        )],
    };

    let loaded = ContextLoader::load(&host, request)
        .await
        .expect("空替换结果应透传原始上下文");
    assert_eq!(loaded.system.as_deref(), Some("系统提示"));
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].text_content(), "需要透传的完整历史");
}

/// 渐进加载期间上下文 owner 尚未 Ready 时，本轮必须保留原始上下文。
#[tokio::test]
async fn selected_context_owner_not_ready_passes_through_original_context() {
    let mut host = CompositePluginHost::new();
    host.set_capability_owner(manifest::CONTEXT_LOADER_CAPABILITY, "loading-context");
    let request = ContextLoadRequest {
        run_id: "run-loading".into(),
        step: 0,
        user_initiated: false,
        provider: "test".into(),
        model: "test-model".into(),
        system: Some("系统提示".into()),
        messages: vec![ModelMessage::text(
            agent_core::MessageRole::User,
            "加载期间保留的历史",
        )],
    };

    let loaded = ContextLoader::load(&host, request)
        .await
        .expect("owner 未 Ready 时应透传原始上下文");
    assert_eq!(loaded.system.as_deref(), Some("系统提示"));
    assert_eq!(loaded.messages.len(), 1);
    assert_eq!(loaded.messages[0].text_content(), "加载期间保留的历史");
}

/// 工具调用只能派发给注册该公开名称的 owner 插件。
#[tokio::test]
async fn tool_call_routes_only_to_owner() {
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(CountingPluginHost {
        id: "first",
        tool: ToolSpec::new("first_tool", "第一个工具", json!({"type": "object"})),
        calls: first_calls.clone(),
    }));
    host.push(Arc::new(CountingPluginHost {
        id: "second",
        tool: ToolSpec::new("second_tool", "第二个工具", json!({"type": "object"})),
        calls: second_calls.clone(),
    }));

    host.list_tools().await.expect("构建工具路由应成功");
    let result = host
        .call_tool(ToolCall::new("call-1", "second_tool", json!({})))
        .await
        .expect("调用不应失败")
        .expect("owner 应返回工具结果");

    assert_eq!(result.content["owner"], "second_tool");
    assert_eq!(first_calls.load(Ordering::SeqCst), 0);
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
}

/// 独占工具策略 owner 必须在其他插件完成 Rewrite 后执行。
#[tokio::test]
async fn selected_tool_policy_checks_final_rewritten_call() {
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(FinalPolicyPluginHost));
    host.push(Arc::new(RewritePluginHost {
        id: "rewriter",
        target: "rewritten",
    }));
    host.set_capability_owner(manifest::TOOL_POLICY_CAPABILITY, "policy");

    let decision = host
        .before_tool(&ToolCall::new("call", "original", json!({})))
        .await
        .expect("最终策略检查不应失败");

    assert!(matches!(decision, ToolDecision::Block { .. }));
}

/// 最终策略 owner 可以改写目标与参数，但不得伪造 Core 绑定的调用 ID 或运行身份。
#[tokio::test]
async fn selected_tool_policy_rewrite_preserves_trusted_call_identity() {
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(RewritePluginHost {
        id: "policy",
        target: "rewritten",
    }));
    host.set_capability_owner(manifest::TOOL_POLICY_CAPABILITY, "policy");
    let mut call = ToolCall::new("trusted-call", "original", json!({}));
    call.bind_permission_scope(ToolPermissionScope::Read);
    call.bind_run_id("trusted-run");

    let decision = host
        .before_tool(&call)
        .await
        .expect("最终策略重写应被可信 Host 重新绑定");

    assert!(matches!(
        decision,
        ToolDecision::Rewrite { call }
            if call.id == "trusted-call"
                && call.name == "rewritten"
                && call.permission_scope == ToolPermissionScope::Unknown
                && call.run_id.as_deref() == Some("trusted-run")
    ));
}

/// 插件工具调用携带的伪造 scope 必须被最近一次可信 ToolSpec 快照覆盖。
#[tokio::test]
async fn composite_host_binds_registered_tool_permission_scope() {
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(CapturingScopePolicyPluginHost {
        observed: observed.clone(),
    }));
    host.push(Arc::new(CountingPluginHost {
        id: "tool-owner",
        tool: ToolSpec::new(
            "scoped_tool",
            "声明读取权限的工具",
            ToolSpec::empty_object_schema(),
        )
        .with_permission_scope(ToolPermissionScope::Read),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    host.set_capability_owner(manifest::TOOL_POLICY_CAPABILITY, "scope-policy");
    host.list_tools().await.expect("建立可信工具 scope 快照");
    let mut call = ToolCall::new("scoped-call", "scoped_tool", json!({}));
    call.bind_permission_scope(ToolPermissionScope::Shell);
    call.bind_run_id("scope-run");

    let decision = host.before_tool(&call).await.expect("执行最终 scope 策略");

    assert!(matches!(
        decision,
        ToolDecision::Rewrite { call }
            if call.name == "scoped_tool"
                && call.permission_scope == ToolPermissionScope::Read
                && call.run_id.as_deref() == Some("scope-run")
    ));
    assert_eq!(
        observed
            .lock()
            .expect("权限 scope 观察锁不应中毒")
            .as_slice(),
        &[(
            "scoped-call".into(),
            "scoped_tool".into(),
            ToolPermissionScope::Read,
            Some("scope-run".into()),
        )]
    );
}

/// Rewrite 改名到未注册目标时必须降级 unknown，不能沿用来源工具的已授权 scope。
#[tokio::test]
async fn composite_host_downgrades_unknown_rewrite_permission_scope() {
    let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(CapturingScopePolicyPluginHost {
        observed: observed.clone(),
    }));
    host.push(Arc::new(CountingPluginHost {
        id: "tool-owner",
        tool: ToolSpec::new(
            "rewrite_source",
            "重写来源工具",
            ToolSpec::empty_object_schema(),
        )
        .with_permission_scope(ToolPermissionScope::Read),
        calls: Arc::new(AtomicUsize::new(0)),
    }));
    host.push(Arc::new(RewritePluginHost {
        id: "rewriter",
        target: "unregistered_target",
    }));
    host.set_capability_owner(manifest::TOOL_POLICY_CAPABILITY, "scope-policy");
    host.list_tools().await.expect("建立 Rewrite scope 快照");

    let mut call = ToolCall::new("rewrite-call", "rewrite_source", json!({}));
    call.bind_run_id("rewrite-run");
    let decision = host
        .before_tool(&call)
        .await
        .expect("执行 Rewrite scope 策略");

    assert!(matches!(
        decision,
        ToolDecision::Rewrite { call }
            if call.name == "unregistered_target"
                && call.permission_scope == ToolPermissionScope::Unknown
                && call.run_id.as_deref() == Some("rewrite-run")
    ));
    assert_eq!(
        observed
            .lock()
            .expect("权限 scope 观察锁不应中毒")
            .as_slice(),
        &[(
            "rewrite-call".into(),
            "unregistered_target".into(),
            ToolPermissionScope::Unknown,
            Some("rewrite-run".into()),
        )]
    );
}

/// UI 声明必须使用可信宿主 ID，渲染和输入只能调用对应 owner。
#[tokio::test]
async fn ui_calls_route_only_to_declared_owner() {
    let first_render_calls = Arc::new(AtomicUsize::new(0));
    let first_input_calls = Arc::new(AtomicUsize::new(0));
    let second_render_calls = Arc::new(AtomicUsize::new(0));
    let second_input_calls = Arc::new(AtomicUsize::new(0));
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(CountingUiPluginHost {
        id: "first",
        declared_plugin_id: "伪造身份",
        view_id: "panel",
        render_calls: first_render_calls.clone(),
        input_calls: first_input_calls.clone(),
    }));
    host.push(Arc::new(CountingUiPluginHost {
        id: "second",
        declared_plugin_id: "first",
        view_id: "panel",
        render_calls: second_render_calls.clone(),
        input_calls: second_input_calls.clone(),
    }));

    let declarations = host.ui_declarations().await.expect("建立 UI 路由应成功");
    assert_eq!(declarations[0].plugin_id, "first");
    assert_eq!(declarations[1].plugin_id, "second");

    let request = ui_render_request("second", "panel");
    assert!(host
        .render_ui(&request)
        .await
        .expect("owner 渲染不应失败")
        .is_some());
    host.on_ui_input(&ui_input("second", "panel"))
        .await
        .expect("owner 输入不应失败");

    assert_eq!(first_render_calls.load(Ordering::SeqCst), 0);
    assert_eq!(first_input_calls.load(Ordering::SeqCst), 0);
    assert_eq!(second_render_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_input_calls.load(Ordering::SeqCst), 1);
}

/// 工具 renderer 必须绑定 owner 自有工具，并通过可信 owner 路由渲染请求。
#[tokio::test]
async fn tool_renderer_is_validated_and_routed_by_tool_owner() {
    let render_calls = Arc::new(AtomicUsize::new(0));
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(ToolRendererPluginHost {
        id: "task-plugin",
        owned_tool: "task",
        rendered_tool: "task",
        render_calls: render_calls.clone(),
        rendered_requests: None,
    }));

    assert!(host
        .ui_declarations()
        .await
        .expect("建立工具 renderer 路由")
        .is_empty());
    let renderers = host.tool_renderers().await.expect("读取 renderer 快照");
    assert_eq!(renderers.len(), 1);
    assert_eq!(renderers[0].plugin_id, "task-plugin");
    assert_eq!(renderers[0].tool_name, "task");

    let context = ToolRenderContext {
        call: ToolCall::new("call-1", "task", json!({})),
        state: ToolRenderState::Running,
        width: 80,
        max_height: 20,
        frame: 1,
    };
    assert!(host
        .render_tool_message(&context)
        .await
        .expect("路由工具 renderer")
        .is_some());
    assert_eq!(render_calls.load(Ordering::SeqCst), 1);
}

/// Guest renderer 只能看到通用权限拒绝，可信 TUI 持有的审批详情不得跨越 Host 边界。
#[tokio::test]
async fn tool_renderer_cannot_observe_permission_denial_detail() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(ToolRendererPluginHost {
        id: "task-plugin",
        owned_tool: "task",
        rendered_tool: "task",
        render_calls: Arc::new(AtomicUsize::new(0)),
        rendered_requests: Some(requests.clone()),
    }));
    host.ui_declarations()
        .await
        .expect("建立工具 renderer 路由");

    let detailed_reason = "审批模型拒绝：检测到仅供可信界面展示的敏感原因";
    let context = ToolRenderContext {
        call: ToolCall::new("call-denied", "task", json!({})),
        state: ToolRenderState::Finished {
            result: ToolResult::error_with_kind(
                "call-denied",
                "task",
                agent_tool::ToolErrorKind::PermissionDenied,
                detailed_reason,
            )
            .with_details(json!({"review": detailed_reason})),
        },
        width: 80,
        max_height: 20,
        frame: 2,
    };

    host.render_tool_message(&context)
        .await
        .expect("路由脱敏后的工具 renderer")
        .expect("renderer 应返回帧");

    let requests = requests.lock().expect("工具 renderer 请求锁不应中毒");
    let ToolRenderState::Finished { result } = &requests[0].context.state else {
        panic!("Guest 应收到完成状态");
    };
    assert_eq!(
        result.error_kind,
        Some(agent_tool::ToolErrorKind::PermissionDenied)
    );
    assert_eq!(result.content, json!({"error": "工具调用未获批准"}));
    assert_eq!(result.details, None);
    assert!(!serde_json::to_string(&requests[0])
        .expect("序列化 Guest renderer 请求")
        .contains(detailed_reason));
    let ToolRenderState::Finished { result } = &context.state else {
        panic!("可信 TUI 上下文应保持完成状态");
    };
    assert!(result.content_text().contains(detailed_reason));
}

/// 插件不得为其他 owner 的工具注册消息 renderer。
#[tokio::test]
async fn tool_renderer_rejects_unowned_tool() {
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(ToolRendererPluginHost {
        id: "task-plugin",
        owned_tool: "task",
        rendered_tool: "shell",
        render_calls: Arc::new(AtomicUsize::new(0)),
        rendered_requests: None,
    }));

    let error = host
        .ui_declarations()
        .await
        .expect_err("非自有工具 renderer 必须失败");
    assert!(error.to_string().contains("不能为非自有工具"));
    assert!(host.tool_renderers().await.expect("读取空快照").is_empty());
}

/// 同一可信插件 ID 下的重复视图必须失败，且不得保留半成品路由。
#[tokio::test]
async fn duplicate_ui_routes_are_rejected_without_partial_snapshot() {
    let render_calls = Arc::new(AtomicUsize::new(0));
    let input_calls = Arc::new(AtomicUsize::new(0));
    let mut host = CompositePluginHost::new();
    for declared_plugin_id in ["first-claim", "second-claim"] {
        host.push(Arc::new(CountingUiPluginHost {
            id: "duplicate",
            declared_plugin_id,
            view_id: "panel",
            render_calls: render_calls.clone(),
            input_calls: input_calls.clone(),
        }));
    }

    let error = host
        .ui_declarations()
        .await
        .expect_err("重复 UI 路由必须失败");
    assert!(error.to_string().contains("重复 UI 视图"));
    assert!(host
        .render_ui(&ui_render_request("duplicate", "panel"))
        .await
        .expect("重复声明后查询应安全返回")
        .is_none());
    host.on_ui_input(&ui_input("duplicate", "panel"))
        .await
        .expect("重复声明后输入应安全返回");
    assert_eq!(render_calls.load(Ordering::SeqCst), 0);
    assert_eq!(input_calls.load(Ordering::SeqCst), 0);
}

/// 空视图 ID 必须在建立组合路由前被拒绝。
#[tokio::test]
async fn empty_ui_view_id_is_rejected_without_snapshot() {
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(CountingUiPluginHost {
        id: "empty-view",
        declared_plugin_id: "ignored",
        view_id: " ",
        render_calls: Arc::new(AtomicUsize::new(0)),
        input_calls: Arc::new(AtomicUsize::new(0)),
    }));

    let error = host
        .ui_declarations()
        .await
        .expect_err("空视图 ID 必须失败");
    assert!(error.to_string().contains("空 UI 视图 ID"));
    assert!(host
        .render_ui(&ui_render_request("empty-view", " "))
        .await
        .expect("失败后路由查询应安全返回")
        .is_none());
}

/// 未建立及因宿主集合变化而失效的 UI 路由必须安全返回且不调用插件。
#[tokio::test]
async fn missing_and_invalidated_ui_routes_are_safe() {
    let render_calls = Arc::new(AtomicUsize::new(0));
    let input_calls = Arc::new(AtomicUsize::new(0));
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(CountingUiPluginHost {
        id: "owner",
        declared_plugin_id: "ignored",
        view_id: "panel",
        render_calls: render_calls.clone(),
        input_calls: input_calls.clone(),
    }));
    let request = ui_render_request("owner", "panel");
    let input = ui_input("owner", "panel");

    assert!(host
        .render_ui(&request)
        .await
        .expect("未建立路由时渲染应安全返回")
        .is_none());
    host.on_ui_input(&input)
        .await
        .expect("未建立路由时输入应安全返回");

    host.ui_declarations().await.expect("首次建立路由应成功");
    host.push(Arc::new(CountingUiPluginHost {
        id: "other",
        declared_plugin_id: "ignored",
        view_id: "other-panel",
        render_calls: Arc::new(AtomicUsize::new(0)),
        input_calls: Arc::new(AtomicUsize::new(0)),
    }));
    assert!(host
        .render_ui(&request)
        .await
        .expect("push 后旧路由应安全返回")
        .is_none());

    host.ui_declarations().await.expect("重建路由应成功");
    assert!(host.remove("other").is_some());
    host.on_ui_input(&input)
        .await
        .expect("remove 后旧路由应安全返回");

    host.ui_declarations().await.expect("再次建立路由应成功");
    assert_eq!(host.clear().len(), 1);
    assert!(host
        .render_ui(&request)
        .await
        .expect("clear 后旧路由应安全返回")
        .is_none());

    assert_eq!(render_calls.load(Ordering::SeqCst), 0);
    assert_eq!(input_calls.load(Ordering::SeqCst), 0);
}

/// 不同插件公开同名工具时必须在模型请求前失败。
#[tokio::test]
async fn duplicate_public_tool_names_are_rejected() {
    let mut host = CompositePluginHost::new();
    for _ in 0..2 {
        host.push(Arc::new(CountingPluginHost {
            id: "duplicate-owner",
            tool: ToolSpec::new("duplicate", "重复工具", json!({"type": "object"})),
            calls: Arc::new(AtomicUsize::new(0)),
        }));
    }

    let error = host
        .list_tools()
        .await
        .expect_err("重复公开工具名必须被拒绝");
    assert!(error.to_string().contains("重复工具"));
}

/// 组合宿主应公开身份查询、工具 owner 和移除接口。
#[tokio::test]
async fn composite_host_exposes_management_api() {
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(CountingPluginHost {
        id: "managed",
        tool: ToolSpec::new("managed_tool", "受管工具", json!({"type": "object"})),
        calls: Arc::new(AtomicUsize::new(0)),
    }));

    host.list_tools().await.expect("构建路由应成功");
    assert_eq!(host.host_ids(), vec!["managed"]);
    assert!(host.get("managed").is_some());
    assert_eq!(
        host.tool_owner("managed_tool").expect("查询 owner 应成功"),
        Some("managed".into())
    );
    assert!(host.remove("managed").is_some());
    assert!(host.is_empty());
    assert_eq!(
        host.tool_owner("managed_tool").expect("查询空路由应成功"),
        None
    );
}

/// 一个插件清理失败时组合宿主仍必须通知后续插件，避免权限审批状态跨 run 残留。
#[tokio::test]
async fn composite_host_finish_run_attempts_all_plugins_after_failure() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(CleanupPluginHost {
        id: "broken",
        calls: Arc::clone(&calls),
        fail: true,
    }));
    host.push(Arc::new(CleanupPluginHost {
        id: "permission",
        calls: Arc::clone(&calls),
        fail: false,
    }));

    let error = host
        .finish_run("run-cleanup")
        .await
        .expect_err("前序插件失败应在全部清理后返回错误");

    assert!(error.to_string().contains("插件 `broken`"));
    assert_eq!(
        calls.lock().expect("插件清理调用锁不应中毒").as_slice(),
        ["broken:run-cleanup", "permission:run-cleanup"]
    );
}

/// 运行开始后发布的新插件不得改变本轮工具定义或路由，下一轮才可见。
#[tokio::test]
async fn live_host_freezes_tools_until_next_run() {
    let live = LivePluginHost::new();
    let first_calls = Arc::new(AtomicUsize::new(0));
    live.publish(Arc::new(CountingPluginHost {
        id: "first",
        tool: ToolSpec::new("first_tool", "首个工具", json!({"type": "object"})),
        calls: first_calls.clone(),
    }))
    .expect("发布首个插件");

    let started = AgentEvent::new("run-1", AgentEventKind::RunStarted, 0, json!({}));
    live.on_event(&started).await.expect("冻结首轮插件快照");
    assert_eq!(
        live.list_tools()
            .await
            .expect("读取首轮工具")
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>(),
        vec!["first_tool"]
    );

    let second_calls = Arc::new(AtomicUsize::new(0));
    live.publish(Arc::new(CountingPluginHost {
        id: "second",
        tool: ToolSpec::new("second_tool", "后续工具", json!({"type": "object"})),
        calls: second_calls.clone(),
    }))
    .expect("运行中发布第二个插件");
    assert!(live.is_ready("second").expect("读取 Ready 状态"));
    assert_eq!(live.list_tools().await.expect("首轮工具保持冻结").len(), 1);
    let unavailable = live
        .call_tool(ToolCall::new("call-2", "second_tool", json!({})))
        .await
        .expect("未进入快照的工具应安全返回");
    assert!(unavailable.is_none());

    let finished = AgentEvent::new("run-1", AgentEventKind::RunFinished, 1, json!({}));
    live.on_event(&finished).await.expect("结束首轮快照");
    let next_started = AgentEvent::new("run-2", AgentEventKind::RunStarted, 0, json!({}));
    live.on_event(&next_started)
        .await
        .expect("冻结下一轮插件快照");
    assert_eq!(live.list_tools().await.expect("读取下一轮工具").len(), 2);
    live.call_tool(ToolCall::new("call-3", "second_tool", json!({})))
        .await
        .expect("下一轮调用新工具")
        .expect("新工具应存在");
    assert_eq!(second_calls.load(Ordering::SeqCst), 1);
    assert_eq!(first_calls.load(Ordering::SeqCst), 0);
}

/// 并发运行必须按可信 run_id 隔离工具、提示、钩子、上下文与事件快照。
#[tokio::test]
async fn live_host_isolates_concurrent_run_snapshots() {
    let live = LivePluginHost::new();
    let first_calls = Arc::new(Mutex::new(Vec::new()));
    let first_before = Arc::new(Mutex::new(Vec::new()));
    let first_after = Arc::new(Mutex::new(Vec::new()));
    let first_events = Arc::new(Mutex::new(Vec::new()));
    let first_finished = Arc::new(Mutex::new(Vec::new()));
    live.publish(Arc::new(SnapshotPluginHost {
        id: "first",
        tool_name: "first_tool",
        prompt: "第一快照提示",
        context: "第一快照上下文",
        deny_tool: None,
        calls: first_calls.clone(),
        before_calls: first_before.clone(),
        after_calls: first_after.clone(),
        events: first_events.clone(),
        pending_events: Arc::new(Mutex::new(std::collections::HashMap::new())),
        finished_runs: first_finished.clone(),
    }))
    .expect("发布首个快照插件");
    live.set_capability_owner(manifest::CONTEXT_LOADER_CAPABILITY, "first")
        .expect("选择首个上下文 owner");
    live.finish_capability_planning().expect("完成能力规划");
    live.on_event_for_run(
        "run-a",
        &AgentEvent::new("run-a", AgentEventKind::RunStarted, 0, json!({})),
    )
    .await
    .expect("冻结首个运行快照");

    let second_calls = Arc::new(Mutex::new(Vec::new()));
    let second_before = Arc::new(Mutex::new(Vec::new()));
    let second_after = Arc::new(Mutex::new(Vec::new()));
    let second_events = Arc::new(Mutex::new(Vec::new()));
    let second_finished = Arc::new(Mutex::new(Vec::new()));
    live.publish(Arc::new(SnapshotPluginHost {
        id: "second",
        tool_name: "second_tool",
        prompt: "第二快照提示",
        context: "第二快照上下文",
        deny_tool: Some("native_blocked"),
        calls: second_calls.clone(),
        before_calls: second_before.clone(),
        after_calls: second_after.clone(),
        events: second_events.clone(),
        pending_events: Arc::new(Mutex::new(std::collections::HashMap::new())),
        finished_runs: second_finished.clone(),
    }))
    .expect("发布第二个快照插件");
    live.set_capability_owner(manifest::CONTEXT_LOADER_CAPABILITY, "second")
        .expect("切换后续运行的上下文 owner");
    live.on_event_for_run(
        "run-b",
        &AgentEvent::new("run-b", AgentEventKind::RunStarted, 0, json!({})),
    )
    .await
    .expect("冻结第二个运行快照");

    let run_a_tools = live
        .list_tools_for_run("run-a")
        .await
        .expect("读取首个运行工具")
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    let run_b_tools = live
        .list_tools_for_run("run-b")
        .await
        .expect("读取第二个运行工具")
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert_eq!(run_a_tools, ["first_tool"]);
    assert_eq!(run_b_tools, ["first_tool", "second_tool"]);

    let run_a_prompts = live
        .prompt_messages_for_run("run-a")
        .await
        .expect("读取首个运行提示")
        .into_iter()
        .map(|message| message.text_content())
        .collect::<Vec<_>>();
    let run_b_prompts = live
        .prompt_messages_for_run("run-b")
        .await
        .expect("读取第二个运行提示")
        .into_iter()
        .map(|message| message.text_content())
        .collect::<Vec<_>>();
    assert_eq!(run_a_prompts, ["第一快照提示"]);
    assert_eq!(run_b_prompts, ["第一快照提示", "第二快照提示"]);

    let mut run_a_before = ToolCall::new("before-a", "native_blocked", json!({}));
    run_a_before.bind_run_id("run-a");
    assert_eq!(
        live.before_tool_for_run("run-a", &run_a_before)
            .await
            .expect("首个运行不应看到后发布策略"),
        ToolDecision::Allow
    );
    let mut run_b_before = ToolCall::new("before-b", "native_blocked", json!({}));
    run_b_before.bind_run_id("run-b");
    assert!(matches!(
        live.before_tool_for_run("run-b", &run_b_before)
            .await
            .expect("第二个运行应经过后发布策略"),
        ToolDecision::Block { .. }
    ));

    let mut unavailable = ToolCall::new("call-a", "second_tool", json!({}));
    unavailable.bind_run_id("run-a");
    assert!(live
        .call_tool_for_run("run-a", unavailable)
        .await
        .expect("首个运行路由应保持冻结")
        .is_none());
    let mut available = ToolCall::new("call-b", "second_tool", json!({}));
    available.bind_run_id("run-b");
    live.call_tool_for_run("run-b", available)
        .await
        .expect("第二个运行工具路由应成功")
        .expect("第二个插件工具应存在");
    let mut mismatched = ToolCall::new("forged-run", "first_tool", json!({}));
    mismatched.bind_run_id("run-b");
    assert!(live
        .call_tool_for_run("run-a", mismatched)
        .await
        .expect_err("调用字段与可信参数不一致时必须拒绝")
        .to_string()
        .contains("run_id 与可信运行不一致"));

    live.after_tool_for_run(
        "run-a",
        &ToolResult::success("after-a", "native", json!({})),
    )
    .await
    .expect("首个运行后置钩子应成功");
    live.after_tool_for_run(
        "run-b",
        &ToolResult::success("after-b", "native", json!({})),
    )
    .await
    .expect("第二个运行后置钩子应成功");

    let context_request = |run_id: &str| ContextLoadRequest {
        run_id: run_id.into(),
        step: 0,
        user_initiated: false,
        provider: "test".into(),
        model: "test-model".into(),
        system: None,
        messages: Vec::new(),
    };
    let run_a_context = PluginHost::load_context(&live, &context_request("run-a"))
        .await
        .expect("加载首个运行上下文")
        .expect("首个运行应有上下文 owner");
    let run_b_context = PluginHost::load_context(&live, &context_request("run-b"))
        .await
        .expect("加载第二个运行上下文")
        .expect("第二个运行应有上下文 owner");
    assert_eq!(run_a_context.messages[0].text_content(), "第一快照上下文");
    assert_eq!(run_b_context.messages[0].text_content(), "第二快照上下文");

    let run_a_event = AgentEvent::new("run-a", AgentEventKind::TurnStarted, 1, json!({}));
    let run_b_event = AgentEvent::new("run-b", AgentEventKind::TurnStarted, 1, json!({}));
    let (run_a_routed, run_b_routed) = tokio::join!(
        live.on_event_for_run("run-a", &run_a_event),
        live.on_event_for_run("run-b", &run_b_event),
    );
    run_a_routed.expect("并发路由首个运行事件");
    run_b_routed.expect("并发路由第二个运行事件");
    let run_a_events = live
        .drain_events_for_run("run-a")
        .await
        .expect("排空首个运行扩展事件");
    let run_b_events = live
        .drain_events_for_run("run-b")
        .await
        .expect("排空第二个运行扩展事件");
    assert_eq!(
        run_a_events,
        [json!({"plugin": "first", "run_id": "run-a"})]
    );
    assert_eq!(
        run_b_events,
        [
            json!({"plugin": "first", "run_id": "run-b"}),
            json!({"plugin": "second", "run_id": "run-b"}),
        ]
    );

    assert_eq!(
        first_before.lock().expect("读取首个前置观察").as_slice(),
        ["run-a", "run-b"]
    );
    assert_eq!(
        second_before.lock().expect("读取第二个前置观察").as_slice(),
        ["run-b"]
    );
    assert!(first_calls.lock().expect("读取首个工具观察").is_empty());
    assert_eq!(
        second_calls.lock().expect("读取第二个工具观察").as_slice(),
        ["run-b"]
    );
    assert_eq!(
        first_after.lock().expect("读取首个后置观察").as_slice(),
        ["after-a", "after-b"]
    );
    assert_eq!(
        second_after.lock().expect("读取第二个后置观察").as_slice(),
        ["after-b"]
    );
    assert_eq!(
        first_events.lock().expect("读取首个事件观察").as_slice(),
        ["run-a", "run-b"]
    );
    assert_eq!(
        second_events.lock().expect("读取第二个事件观察").as_slice(),
        ["run-b"]
    );

    live.on_event_for_run(
        "run-a",
        &AgentEvent::new("run-a", AgentEventKind::RunFinished, 2, json!({})),
    )
    .await
    .expect("结束首个运行");
    assert!(live.list_tools_for_run("run-a").await.is_err());
    live.drain_events_for_run("run-a")
        .await
        .expect("结束事件后的最后一次排空仍应使用原快照");
    live.finish_run("run-a").await.expect("释放首个运行快照");
    assert!(live.list_tools_for_run("unknown-run").await.is_err());
    assert!(
        PluginHost::load_context(&live, &context_request("unknown-run"))
            .await
            .is_err()
    );
    assert_eq!(
        live.list_tools_for_run("run-b")
            .await
            .expect("首个运行结束不得移除第二个快照")
            .len(),
        2
    );

    live.on_event_for_run(
        "run-b",
        &AgentEvent::new("run-b", AgentEventKind::RunFinished, 2, json!({})),
    )
    .await
    .expect("结束第二个运行");
    live.drain_events_for_run("run-b")
        .await
        .expect("排空第二个运行结束事件");
    live.finish_run("run-b").await.expect("释放第二个运行快照");
    assert!(first_finished
        .lock()
        .expect("读取首个正常清理观察")
        .is_empty());
    assert!(second_finished
        .lock()
        .expect("读取第二个正常清理观察")
        .is_empty());

    live.on_event_for_run(
        "run-aborted",
        &AgentEvent::new("run-aborted", AgentEventKind::RunStarted, 0, json!({})),
    )
    .await
    .expect("冻结异常运行快照");
    live.finish_run("run-aborted")
        .await
        .expect("异常路径应通知底层插件清理");
    assert_eq!(
        first_finished
            .lock()
            .expect("读取首个异常清理观察")
            .as_slice(),
        ["run-aborted"]
    );
    assert_eq!(
        second_finished
            .lock()
            .expect("读取第二个异常清理观察")
            .as_slice(),
        ["run-aborted"]
    );
}

/// RunFinished 已送达时，缺失或失败的最终排空只能补做排空，不得重复通知 Guest。
#[tokio::test]
async fn live_host_requires_finish_callback_and_final_drain_before_release() {
    let live = LivePluginHost::new();
    let finish_events = Arc::new(AtomicUsize::new(0));
    let drain_calls = Arc::new(AtomicUsize::new(0));
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let fail_next_drain = Arc::new(AtomicBool::new(false));
    let fail_next_cleanup = Arc::new(AtomicBool::new(false));
    live.publish(Arc::new(RetryableRunCleanupPluginHost {
        id: "cleanup",
        finish_events: Arc::clone(&finish_events),
        drain_calls: Arc::clone(&drain_calls),
        cleanup_calls: Arc::clone(&cleanup_calls),
        fail_next_drain: Arc::clone(&fail_next_drain),
        fail_next_cleanup: Arc::clone(&fail_next_cleanup),
    }))
    .expect("发布清理测试插件");

    live.on_event(&AgentEvent::new(
        "run-normal",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("开始正常运行");
    live.on_event(&AgentEvent::new(
        "run-normal",
        AgentEventKind::RunFinished,
        1,
        json!({}),
    ))
    .await
    .expect("通知正常运行结束");
    live.drain_events_for_run("run-normal")
        .await
        .expect("正常运行应完成最终事件排空");
    live.finish_run("run-normal")
        .await
        .expect("正常运行应直接释放快照");
    assert_eq!(finish_events.load(Ordering::SeqCst), 1);
    assert_eq!(drain_calls.load(Ordering::SeqCst), 1);
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 0);

    live.on_event(&AgentEvent::new(
        "run-without-drain",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("开始缺少最终排空的运行");
    live.on_event(&AgentEvent::new(
        "run-without-drain",
        AgentEventKind::RunFinished,
        1,
        json!({}),
    ))
    .await
    .expect("通知缺少最终排空的运行结束");
    live.finish_run("run-without-drain")
        .await
        .expect("缺少最终排空时应由 Live Host 补做排空");
    assert_eq!(finish_events.load(Ordering::SeqCst), 2);
    assert_eq!(drain_calls.load(Ordering::SeqCst), 2);
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 0);

    live.on_event(&AgentEvent::new(
        "run-drain-failed",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("开始最终排空失败的运行");
    live.on_event(&AgentEvent::new(
        "run-drain-failed",
        AgentEventKind::RunFinished,
        1,
        json!({}),
    ))
    .await
    .expect("通知最终排空失败的运行结束");
    fail_next_drain.store(true, Ordering::SeqCst);
    live.drain_events_for_run("run-drain-failed")
        .await
        .expect_err("最终排空故障必须上报");
    live.finish_run("run-drain-failed")
        .await
        .expect("最终排空失败后应重试排空而非重复结束回调");
    assert_eq!(finish_events.load(Ordering::SeqCst), 3);
    assert_eq!(drain_calls.load(Ordering::SeqCst), 4);
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 0);
}

/// 单次 Core 清理调用必须在 Live Host 内吸收一次瞬态失败并释放冻结快照。
#[tokio::test]
async fn live_host_retries_cleanup_within_single_finish_call() {
    let live = LivePluginHost::new();
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let fail_next_cleanup = Arc::new(AtomicBool::new(true));
    live.publish(Arc::new(RetryableRunCleanupPluginHost {
        id: "cleanup",
        finish_events: Arc::new(AtomicUsize::new(0)),
        drain_calls: Arc::new(AtomicUsize::new(0)),
        cleanup_calls: Arc::clone(&cleanup_calls),
        fail_next_drain: Arc::new(AtomicBool::new(false)),
        fail_next_cleanup: Arc::clone(&fail_next_cleanup),
    }))
    .expect("发布可重试清理测试插件");
    live.on_event(&AgentEvent::new(
        "run-retry",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("开始异常运行");

    live.finish_run("run-retry")
        .await
        .expect("Live Host 应在同一次调用内重试瞬态清理失败");
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 2);
    assert!(live.list_tools_for_run("run-retry").await.is_err());

    live.finish_run("run-retry")
        .await
        .expect("成功后的重复清理应保持幂等");
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 2);
}

/// 单次 Core 清理调用必须在 Live Host 的内部超时后重试，不能依赖外层再次调用。
#[tokio::test]
async fn live_host_bounds_hanging_cleanup_and_retries_internally() {
    let live = LivePluginHost::new();
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    live.publish(Arc::new(CancellableRunCleanupPluginHost {
        cleanup_calls: Arc::clone(&cleanup_calls),
        entered: Arc::new(tokio::sync::Notify::new()),
    }))
    .expect("发布超时清理测试插件");
    live.on_event(&AgentEvent::new(
        "run-hanging-cleanup",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("开始清理将超时的运行");

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        live.finish_run("run-hanging-cleanup"),
    )
    .await
    .expect("Live Host 的有界清理应先于外层预算结束")
    .expect("第二次内部清理尝试应成功");
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 2);
}

/// 清理 future 被超时取消后，租约必须恢复可重试状态，不能永久卡在 Cleaning。
#[tokio::test]
async fn live_host_recovers_cleanup_state_when_future_is_cancelled() {
    let live = LivePluginHost::new();
    let cleanup_calls = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(tokio::sync::Notify::new());
    live.publish(Arc::new(CancellableRunCleanupPluginHost {
        cleanup_calls: Arc::clone(&cleanup_calls),
        entered: Arc::clone(&entered),
    }))
    .expect("发布可取消清理测试插件");
    live.on_event(&AgentEvent::new(
        "run-cancelled-cleanup",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("开始清理将被取消的运行");

    let cleanup_task = tokio::spawn({
        let live = live.clone();
        async move { live.finish_run("run-cancelled-cleanup").await }
    });
    entered.notified().await;
    cleanup_task.abort();
    assert!(cleanup_task
        .await
        .expect_err("清理任务应被取消")
        .is_cancelled());

    live.finish_run("run-cancelled-cleanup")
        .await
        .expect("取消后的第二次清理应恢复并成功");
    assert_eq!(cleanup_calls.load(Ordering::SeqCst), 2);
}

/// 有界清理持续失败时仍必须释放 Live 路由快照，避免不可达 run 永久泄漏。
#[tokio::test]
async fn live_host_releases_snapshot_after_bounded_cleanup_failure() {
    let live = LivePluginHost::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    live.publish(Arc::new(CleanupPluginHost {
        id: "always-fails",
        calls: Arc::clone(&calls),
        fail: true,
    }))
    .expect("发布持续失败的清理测试插件");
    live.on_event(&AgentEvent::new(
        "run-bounded-failure",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("开始持续清理失败的运行");

    let error = live
        .finish_run("run-bounded-failure")
        .await
        .expect_err("两次有界清理都失败时应返回错误");
    assert!(error.to_string().contains("2 次有界尝试"));
    assert_eq!(
        calls.lock().expect("读取持续失败清理次数").as_slice(),
        [
            "always-fails:run-bounded-failure",
            "always-fails:run-bounded-failure"
        ]
    );

    live.on_event(&AgentEvent::new(
        "run-bounded-failure",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("最终失败后同一 run_id 应能重新建立快照");
    live.on_event(&AgentEvent::new(
        "run-bounded-failure",
        AgentEventKind::RunFinished,
        1,
        json!({}),
    ))
    .await
    .expect("重新建立的运行应正常结束");
    live.drain_events_for_run("run-bounded-failure")
        .await
        .expect("重新建立的运行应完成最终排空");
    live.finish_run("run-bounded-failure")
        .await
        .expect("正常终态应直接释放重新建立的快照");
}

/// 并发加载完成顺序不得改变计划中的工具顺序。
#[tokio::test]
async fn live_host_publishes_plugins_in_planned_order() {
    let live = LivePluginHost::new();
    live.set_plugin_order(&["first".into(), "second".into()])
        .expect("设置稳定计划顺序");
    live.publish(Arc::new(CountingPluginHost {
        id: "second",
        tool: ToolSpec::new("second_tool", "第二个工具", json!({"type": "object"})),
        calls: Arc::new(AtomicUsize::new(0)),
    }))
    .expect("先发布后完成插件");
    live.publish(Arc::new(CountingPluginHost {
        id: "first",
        tool: ToolSpec::new("first_tool", "第一个工具", json!({"type": "object"})),
        calls: Arc::new(AtomicUsize::new(0)),
    }))
    .expect("后发布先计划插件");

    let tools = live
        .list_tools()
        .await
        .expect("读取稳定工具顺序")
        .into_iter()
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    assert_eq!(tools, vec!["first_tool", "second_tool"]);
}

/// 独占工具策略 owner 尚未 Ready 时必须阻止工具，不能在加载窗口绕过策略。
#[tokio::test]
async fn live_host_blocks_tools_until_selected_policy_is_ready() {
    let live = LivePluginHost::new();
    live.set_capability_owner(manifest::TOOL_POLICY_CAPABILITY, "permission")
        .expect("设置策略 owner");
    live.finish_capability_planning().expect("完成能力规划");
    live.on_event(&AgentEvent::new(
        "run-policy",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("冻结缺少策略插件的快照");

    let decision = live
        .before_tool(&ToolCall::new("native-1", "shell", json!({})))
        .await
        .expect("缺少策略时返回可解释阻止结果");
    assert!(matches!(
        decision,
        ToolDecision::Block {
            reason,
            agent_guidance: Some(guidance),
        } if reason.contains("仍在加载")
            && !guidance.contains("permission")
            && guidance.contains("稍后再尝试")
    ));
}

/// 独占能力归属尚未解析时必须默认阻止工具，避免启动竞态形成策略空窗。
#[tokio::test]
async fn live_host_blocks_tools_while_capability_planning_is_pending() {
    let live = LivePluginHost::new();
    live.on_event(&AgentEvent::new(
        "run-planning",
        AgentEventKind::RunStarted,
        0,
        json!({}),
    ))
    .await
    .expect("冻结能力规划中的插件快照");

    let decision = live
        .before_tool(&ToolCall::new("native-1", "shell", json!({})))
        .await
        .expect("能力规划中返回可解释阻止结果");
    assert!(matches!(
        decision,
        ToolDecision::Block {
            reason,
            agent_guidance: Some(guidance),
        } if reason.contains("能力归属仍在解析") && guidance.contains("稍后再尝试")
    ));
}

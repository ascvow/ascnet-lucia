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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

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
        let mut call = call.clone();
        call.name = self.target.into();
        Ok(ToolDecision::Rewrite { call })
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
    observed: Arc<std::sync::Mutex<Vec<(String, ToolPermissionScope)>>>,
}

#[async_trait]
impl AgentExtension for CapturingScopePolicyPluginHost {
    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        self.observed
            .lock()
            .expect("权限 scope 观察锁不应中毒")
            .push((call.name.clone(), call.permission_scope));
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
            Ok(ToolDecision::Block {
                reason: "最终策略已检查重写调用".into(),
            })
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

    let decision = host.before_tool(&call).await.expect("执行最终 scope 策略");

    assert!(matches!(
        decision,
        ToolDecision::Rewrite { call }
            if call.name == "scoped_tool"
                && call.permission_scope == ToolPermissionScope::Read
    ));
    assert_eq!(
        observed
            .lock()
            .expect("权限 scope 观察锁不应中毒")
            .as_slice(),
        &[("scoped_tool".into(), ToolPermissionScope::Read)]
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

    let decision = host
        .before_tool(&ToolCall::new("rewrite-call", "rewrite_source", json!({})))
        .await
        .expect("执行 Rewrite scope 策略");

    assert!(matches!(
        decision,
        ToolDecision::Rewrite { call }
            if call.name == "unregistered_target"
                && call.permission_scope == ToolPermissionScope::Unknown
    ));
    assert_eq!(
        observed
            .lock()
            .expect("权限 scope 观察锁不应中毒")
            .as_slice(),
        &[("unregistered_target".into(), ToolPermissionScope::Unknown)]
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

/// 插件不得为其他 owner 的工具注册消息 renderer。
#[tokio::test]
async fn tool_renderer_rejects_unowned_tool() {
    let mut host = CompositePluginHost::new();
    host.push(Arc::new(ToolRendererPluginHost {
        id: "task-plugin",
        owned_tool: "task",
        rendered_tool: "shell",
        render_calls: Arc::new(AtomicUsize::new(0)),
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
    assert!(matches!(decision, ToolDecision::Block { reason } if reason.contains("仍在加载")));
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
    assert!(
        matches!(decision, ToolDecision::Block { reason } if reason.contains("能力归属仍在解析"))
    );
}

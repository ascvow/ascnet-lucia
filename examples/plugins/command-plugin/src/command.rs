//! Lucia 官方 Command 插件。
//!
//! 插件维护命令注册表、解析命令行并完全接管斜杠命令交互：补全弹层与会话
//! 对话框由插件自己渲染，第三方命令回调由插件直接经宿主服务调用完成，
//! 应用级动作（新建会话、重载上下文、退出等）通过通用宿主动作事件请求。
//! 宿主只转发主输入快照与手势键，并提供会话摘要数据源。

use agent_plugin::{
    export_plugin, ActivationContext, AgentEvent, AgentEventKind, AgentPlugin, EventPresentation,
    EventPresentationTarget, EventPresentationTone, EventPresentationVariant, ExtensionEvent,
    PluginHostApi, ServiceCall, ServiceSpec, UiColor, UiDeclaration, UiFrame, UiHostAction,
    UiHostActionRequest, UiInput, UiInputEvent, UiLine, UiPlacement, UiRenderRequest,
    UiSessionListStatus, UiSessionSummary, UiSessionsReply, UiSize, UiSpan, UiStyle,
};
use anyhow::{anyhow, Context, Result};
use command_protocol::{
    canonical_command_name, encode_command_token, ArgumentKind, ArgumentSpec, CommandAvailability,
    CommandCallbackRequest, CommandCallbackResponse, CommandCompletionRequest, CommandHandlerRef,
    CommandInvocation, CommandSnapshot, CommandSpec, CompletionContext, CompletionItem,
    CompletionSource, ParsedCommandLine, PrepareCompletionRequest, PrepareCompletionResponse,
    RegisterCommandRequest, RegisterCommandResponse, SessionListStatus, SessionSummary,
    SessionSurfaceMode, SurfaceCompletionRequest, SurfaceEffect, SurfaceUpdateRequest,
    UnregisterCommandRequest, UnregisterCommandResponse, DEFAULT_COMPLETION_LIMIT,
    MAX_COMPLETION_LIMIT, PREPARE_COMPLETION_SERVICE, PROTOCOL_VERSION, REGISTER_SERVICE,
    SESSION_COMPLETION_SOURCE, SESSION_DIALOG_VIEW, SNAPSHOT_SERVICE, SURFACE_UPDATE_SERVICE,
    UNREGISTER_SERVICE,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

mod popup;
mod registry;
mod surface;

use popup::*;
use registry::*;
use surface::*;

/// 补全弹层的视图 ID。
const POPUP_VIEW: &str = "command-popup";
/// 单次会话查询最多返回的摘要数量。
const SESSION_PAGE_LIMIT: u16 = 50;
/// 弹层参数候选一次展示的数量上限。
const POPUP_COMPLETION_LIMIT: u16 = 6;
/// 命令输出写入主事件列表前的长度上限。
const OUTPUT_PREVIEW_LIMIT: usize = 4_000;
/// manifest 未配置 surface 权限时仅允许官方 TUI 调用方。
const DEFAULT_SURFACE_AUTHORITY: &str = "lucia-tui";
/// Command 插件注册到 Host 的全部服务。
const SERVICES: [&str; 5] = [
    REGISTER_SERVICE,
    UNREGISTER_SERVICE,
    SNAPSHOT_SERVICE,
    PREPARE_COMPLETION_SERVICE,
    SURFACE_UPDATE_SERVICE,
];
/// 内置命令在注册表中的可信 owner。
const BUILTIN_OWNER: &str = "command";

/// 保存命令注册表、补全弹层与会话对话框状态的官方插件。
struct CommandPlugin {
    registry: CommandRegistry,
    popup: CommandPopup,
    surface: SessionSurface,
    surface_authority: String,
    /// 主 Agent 是否空闲，由生命周期事件维护。
    agent_idle: bool,
    /// 会话查询的单调 ID，弹层与对话框共用避免应答串线。
    query_seq: u64,
    /// 宿主动作请求的单调 ID。
    action_seq: u64,
}

impl Default for CommandPlugin {
    fn default() -> Self {
        Self {
            registry: CommandRegistry::with_builtins(),
            popup: CommandPopup::default(),
            surface: SessionSurface::default(),
            surface_authority: DEFAULT_SURFACE_AUTHORITY.into(),
            agent_idle: true,
            query_seq: 0,
            action_seq: 0,
        }
    }
}

impl AgentPlugin for CommandPlugin {
    /// 注册版本化 Command 服务并读取 TUI 调用方限制。
    fn activate(&mut self, host: &dyn PluginHostApi, context: ActivationContext) -> Result<()> {
        self.surface_authority = configured_surface_authority(&context);
        for (name, description) in service_descriptions() {
            host.upsert_service(&ServiceSpec {
                name: name.into(),
                version: PROTOCOL_VERSION.into(),
                description: Some(description.into()),
            })?;
        }
        Ok(())
    }

    /// 显式移除服务，便于不依赖 Host 实例销毁顺序完成降级。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        for service in SERVICES {
            host.remove_service(service)?;
        }
        Ok(())
    }

    /// 处理注册表、快照、补全计划与会话查询应答服务。
    fn handle_service(&mut self, host: &dyn PluginHostApi, call: ServiceCall) -> Result<Value> {
        match call.name.as_str() {
            REGISTER_SERVICE => self.register(call),
            UNREGISTER_SERVICE => self.unregister(call),
            SNAPSHOT_SERVICE => self.snapshot(host, call),
            PREPARE_COMPLETION_SERVICE => self.prepare_completion(host, call),
            SURFACE_UPDATE_SERVICE => self.accept_sessions_reply(host, call),
            _ => Err(anyhow!("未知 Command 服务：{}", call.name)),
        }
    }

    /// 根据主 Agent 生命周期维护命令可用状态。
    fn on_event(&mut self, event: AgentEvent) {
        match event.kind {
            AgentEventKind::RunStarted => self.agent_idle = false,
            AgentEventKind::RunFinished => self.agent_idle = true,
            _ => {}
        }
    }

    /// 声明触发前缀为 `/` 的补全弹层和默认隐藏的 Session Dialog。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![
            UiDeclaration {
                plugin_id: String::new(),
                view_id: POPUP_VIEW.into(),
                title: "命令".into(),
                placement: UiPlacement::InputPanel,
                size: UiSize {
                    width: None,
                    height: Some(8),
                },
                focusable: false,
                input_triggers: vec!["/".into()],
            },
            UiDeclaration {
                plugin_id: String::new(),
                view_id: SESSION_DIALOG_VIEW.into(),
                title: "会话".into(),
                placement: UiPlacement::Dialog,
                size: UiSize {
                    width: Some(76),
                    height: Some(20),
                },
                focusable: true,
                input_triggers: Vec::new(),
            },
        ]
    }

    /// 渲染补全弹层与由插件拥有的会话列表状态。
    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        match request.view_id.as_str() {
            POPUP_VIEW => Some(UiFrame {
                view_id: request.view_id,
                visible: self.popup.visible(&self.registry),
                lines: self
                    .popup
                    .render(&self.registry, self.agent_idle, request.width),
            }),
            SESSION_DIALOG_VIEW => Some(UiFrame {
                view_id: request.view_id,
                visible: self.surface.visible,
                lines: self.surface.render(request.width, request.height),
            }),
            _ => None,
        }
    }

    /// 处理弹层的主输入快照与手势键，以及会话 Dialog 的键盘和鼠标输入。
    fn on_ui_input_with_host(&mut self, host: &dyn PluginHostApi, input: UiInput) {
        match input.view_id.as_str() {
            POPUP_VIEW => self.handle_popup_input(host, input.event),
            SESSION_DIALOG_VIEW if self.surface.visible => {
                self.surface.handle_input(&mut self.query_seq, input.event);
                self.drain_surface_effects(host);
            }
            _ => {}
        }
    }
}

impl CommandPlugin {
    /// 注册命令，并使用 Host 注入的调用方 ID 作为不可伪造 owner。
    fn register(&mut self, call: ServiceCall) -> Result<Value> {
        let request: RegisterCommandRequest =
            serde_json::from_value(call.payload).context("解析 command.register 请求失败")?;
        let name = self.registry.register(call.caller_id, request.spec)?;
        to_value(RegisterCommandResponse {
            name,
            generation: self.registry.generation,
        })
    }

    /// 仅允许调用方注销自己拥有的非内置命令。
    fn unregister(&mut self, call: ServiceCall) -> Result<Value> {
        let request: UnregisterCommandRequest =
            serde_json::from_value(call.payload).context("解析 command.unregister 请求失败")?;
        let removed = self.registry.unregister(&call.caller_id, &request.name)?;
        to_value(UnregisterCommandResponse {
            removed,
            generation: self.registry.generation,
        })
    }

    /// 返回不含 owner 身份的命令快照，供第三方插件查询。
    fn snapshot(&mut self, host: &dyn PluginHostApi, call: ServiceCall) -> Result<Value> {
        let _: command_protocol::SnapshotRequest =
            serde_json::from_value(call.payload).context("解析 command.snapshot 请求失败")?;
        self.prune_unavailable_commands(host)?;
        to_value(self.registry.snapshot())
    }

    /// 为第三方调用方显式生成参数候选计划。
    fn prepare_completion(&mut self, host: &dyn PluginHostApi, call: ServiceCall) -> Result<Value> {
        let request: PrepareCompletionRequest = serde_json::from_value(call.payload)
            .context("解析 command.prepare-completion 请求失败")?;
        self.prune_unavailable_commands(host)?;
        to_value(self.registry.prepare_completion(request))
    }

    /// 移除已经失去回调服务的第三方命令，避免失败卸载留下不可执行条目。
    fn prune_unavailable_commands(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        let owners = self
            .registry
            .commands
            .values()
            .filter(|entry| entry.builtin.is_none())
            .map(|entry| entry.owner_plugin_id.clone())
            .collect::<BTreeSet<_>>();
        let mut services_by_owner = BTreeMap::new();
        for owner in owners {
            let services = host
                .list_services(Some(&owner))?
                .into_iter()
                .map(|service| service.name)
                .collect::<BTreeSet<_>>();
            services_by_owner.insert(owner, services);
        }
        self.registry.prune_unavailable_handlers(&services_by_owner);
        Ok(())
    }

    /// 处理弹层收到的主输入快照与手势键。
    fn handle_popup_input(&mut self, host: &dyn PluginHostApi, event: UiInputEvent) {
        match event {
            UiInputEvent::MainInput { text, cursor } => {
                self.popup
                    .sync(text, usize::try_from(cursor).unwrap_or(usize::MAX));
            }
            UiInputEvent::Key { code, .. } => match code.as_str() {
                "tab" => self.popup_tab(host),
                "enter" => self.popup_enter(host),
                "up" => self.popup.select_previous(),
                "down" => {
                    let len = self
                        .popup
                        .completion
                        .as_ref()
                        .map(|completion| completion.items.len().min(6))
                        .unwrap_or_else(|| self.popup.matches(&self.registry).len());
                    self.popup.select_next(len);
                }
                "escape" => self.popup.dismiss(),
                _ => {}
            },
            UiInputEvent::Mouse { .. } => {}
        }
    }

    /// Tab 手势：应用候选、补全命令名或生成参数候选计划。
    fn popup_tab(&mut self, host: &dyn PluginHostApi) {
        if self.popup.completion.is_some() {
            self.apply_popup_completion(host);
            return;
        }
        if self.popup.in_name_stage() {
            let matches = self.popup.matches(&self.registry);
            if matches.is_empty() {
                return;
            }
            let selected = self.popup.selection.min(matches.len() - 1);
            let text = format!("/{} ", matches[selected].name);
            let cursor = text.len();
            self.set_host_input(host, text, cursor);
            return;
        }
        if self.popup.pending.is_some() {
            return;
        }
        if let Err(error) = self.prune_unavailable_commands(host) {
            self.notify(host, EventPresentationTone::Error, format!("{error:#}"));
        }
        let response = self.registry.prepare_completion(PrepareCompletionRequest {
            input: self.popup.input.clone(),
            cursor: u32::try_from(self.popup.cursor).ok(),
            limit: POPUP_COMPLETION_LIMIT,
        });
        match response {
            PrepareCompletionResponse::Candidates { context, items } => {
                self.finish_popup_completion(host, context, items);
            }
            PrepareCompletionResponse::Callback {
                context,
                owner_plugin_id,
                service,
                request,
            } => match self.call_completion_callback(host, &owner_plugin_id, &service, request) {
                Ok(items) => self.finish_popup_completion(host, context, items),
                Err(error) => self.notify(
                    host,
                    EventPresentationTone::Error,
                    format!("命令参数补全失败：{error:#}"),
                ),
            },
            PrepareCompletionResponse::Surface { context, request } => {
                self.request_session_completion(host, context, request);
            }
            PrepareCompletionResponse::NoMatch => {}
            PrepareCompletionResponse::Error { message } => {
                self.notify(host, EventPresentationTone::Error, message);
            }
        }
    }

    /// 直接调用命令 owner 的动态补全回调并解析候选。
    fn call_completion_callback(
        &self,
        host: &dyn PluginHostApi,
        owner_plugin_id: &str,
        service: &str,
        request: CommandCallbackRequest,
    ) -> Result<Vec<CompletionItem>> {
        let payload = serde_json::to_value(request).context("序列化补全回调请求失败")?;
        let response = host.call_service(owner_plugin_id, service, &payload)?;
        match serde_json::from_value::<CommandCallbackResponse>(response)
            .context("解析补全回调响应失败")?
        {
            CommandCallbackResponse::Completed { items } => Ok(items),
            CommandCallbackResponse::Executed { .. } => Err(anyhow!("参数补全回调返回了执行结果")),
        }
    }

    /// 向宿主会话数据源发起异步候选查询。
    fn request_session_completion(
        &mut self,
        host: &dyn PluginHostApi,
        context: CompletionContext,
        request: SurfaceCompletionRequest,
    ) {
        if request.source != SESSION_COMPLETION_SOURCE {
            self.notify(
                host,
                EventPresentationTone::Error,
                format!("不支持的命令候选数据源：{}", request.source),
            );
            return;
        }
        self.query_seq = self.query_seq.saturating_add(1).max(1);
        let query_id = self.query_seq;
        self.popup.pending = Some(PendingSessionCompletion {
            query_id,
            context,
            source_input: self.popup.input.clone(),
        });
        self.emit_action(
            host,
            UiHostAction::QuerySessions {
                query_id,
                query: request.request.prefix,
                cursor: None,
                limit: POPUP_COMPLETION_LIMIT,
                reply_service: SURFACE_UPDATE_SERVICE.into(),
            },
        );
    }

    /// 提交候选到弹层；唯一候选立即应用。
    fn finish_popup_completion(
        &mut self,
        host: &dyn PluginHostApi,
        context: CompletionContext,
        items: Vec<CompletionItem>,
    ) {
        if items.is_empty() {
            return;
        }
        let apply_immediately = items.len() == 1;
        self.popup.selection = 0;
        self.popup.completion = Some(PopupCompletion {
            context,
            source_input: self.popup.input.clone(),
            items,
        });
        if apply_immediately {
            self.apply_popup_completion(host);
        }
    }

    /// 把选中的候选写回宿主主输入框。
    fn apply_popup_completion(&mut self, host: &dyn PluginHostApi) {
        match self.popup.apply_selected() {
            Some((text, cursor)) => self.set_host_input(host, text, cursor),
            None => self.popup.completion = None,
        }
    }

    /// Enter 手势：解析命令并按计划执行。
    fn popup_enter(&mut self, host: &dyn PluginHostApi) {
        let input = self.popup.input.trim().to_string();
        if input.is_empty() {
            return;
        }
        // 与原生输入编辑一致：忙时保留输入原文，方便运行结束后重试。
        if !self.agent_idle
            && self
                .spec_for_input(&input)
                .is_some_and(|spec| spec.availability == CommandAvailability::IdleOnly)
        {
            self.notify(
                host,
                EventPresentationTone::Info,
                "该命令只能在 Agent 空闲时执行",
            );
            return;
        }
        if let Err(error) = self.prune_unavailable_commands(host) {
            self.notify(host, EventPresentationTone::Error, format!("{error:#}"));
        }
        self.set_host_input(host, String::new(), 0);
        match self.registry.prepare(&input, self.agent_idle) {
            Prepared::Builtin {
                command,
                invocation,
            } => self.execute_builtin(host, command, invocation, &input),
            Prepared::Callback {
                owner_plugin_id,
                handler,
                invocation,
            } => self.execute_callback(host, &owner_plugin_id, handler, invocation),
            Prepared::Error { message, usage } => {
                let content = usage
                    .filter(|usage| !usage.trim().is_empty())
                    .map(|usage| format!("{message}\n用法：{usage}"))
                    .unwrap_or(message);
                self.notify(host, EventPresentationTone::Error, content);
            }
        }
    }

    /// 执行不会跨插件回调的官方内置命令。
    fn execute_builtin(
        &mut self,
        host: &dyn PluginHostApi,
        command: BuiltinCommand,
        invocation: CommandInvocation,
        input: &str,
    ) {
        match command {
            BuiltinCommand::Help => {
                let target = invocation
                    .arguments
                    .get("command")
                    .and_then(|values| values.first())
                    .map(String::as_str);
                let content = truncate_output(&self.registry.help(target));
                self.notify(host, EventPresentationTone::Info, content);
            }
            BuiltinCommand::Resume => {
                self.surface
                    .open(&mut self.query_seq, SessionSurfaceMode::Resume);
                self.drain_surface_effects(host);
            }
            BuiltinCommand::Sessions => {
                self.surface
                    .open(&mut self.query_seq, SessionSurfaceMode::Browse);
                self.drain_surface_effects(host);
            }
            BuiltinCommand::New => self.emit_action(host, UiHostAction::NewSession),
            BuiltinCommand::Clear => self.emit_action(host, UiHostAction::ClearSession),
            BuiltinCommand::Compact => self.emit_action(
                host,
                UiHostAction::ReloadContext {
                    label: Some(input.to_string()),
                },
            ),
            BuiltinCommand::Exit => self.emit_action(host, UiHostAction::Exit),
        }
    }

    /// 直接调用第三方命令 owner 的执行回调并展示结果。
    fn execute_callback(
        &mut self,
        host: &dyn PluginHostApi,
        owner_plugin_id: &str,
        handler: CommandHandlerRef,
        invocation: CommandInvocation,
    ) {
        let request = CommandCallbackRequest::Execute {
            handler_id: handler.handler_id,
            invocation,
        };
        let response = serde_json::to_value(request)
            .context("序列化命令回调请求失败")
            .and_then(|payload| host.call_service(owner_plugin_id, &handler.service, &payload))
            .and_then(|response| {
                serde_json::from_value::<CommandCallbackResponse>(response)
                    .context("解析命令回调响应失败")
            });
        match response {
            Ok(CommandCallbackResponse::Executed { result }) => {
                let content = match result {
                    Value::Null => "命令执行完成".to_string(),
                    Value::String(content) => content,
                    value => serde_json::to_string_pretty(&value).unwrap_or_default(),
                };
                self.notify(host, EventPresentationTone::Info, truncate_output(&content));
            }
            Ok(CommandCallbackResponse::Completed { .. }) => self.notify(
                host,
                EventPresentationTone::Error,
                "命令执行回调返回了补全结果",
            ),
            Err(error) => self.notify(
                host,
                EventPresentationTone::Error,
                format!("命令执行失败：{error:#}"),
            ),
        }
    }

    /// 根据输入首个 token 解析命令定义，仅用于可用性预检查。
    fn spec_for_input(&self, input: &str) -> Option<&CommandSpec> {
        let name = input
            .trim()
            .strip_prefix('/')?
            .split_whitespace()
            .next()?
            .to_ascii_lowercase();
        let canonical = self.registry.resolve_name(&name)?;
        self.registry
            .commands
            .get(canonical)
            .map(|entry| &entry.spec)
    }

    /// 接受宿主会话查询应答，按查询 ID 路由到弹层候选或会话对话框。
    fn accept_sessions_reply(
        &mut self,
        host: &dyn PluginHostApi,
        call: ServiceCall,
    ) -> Result<Value> {
        self.ensure_surface_authority(&call.caller_id)?;
        let reply: UiSessionsReply =
            serde_json::from_value(call.payload).context("解析会话查询应答失败")?;
        if let Some(pending) = self.popup.pending.take() {
            if pending.query_id == reply.query_id {
                if pending.source_input == self.popup.input {
                    let items = session_completion_items(&reply.status);
                    self.finish_popup_completion(host, pending.context, items);
                }
                return Ok(serde_json::json!({"accepted": true}));
            }
            self.popup.pending = Some(pending);
        }
        let accepted = self.surface.update(SurfaceUpdateRequest {
            request_id: reply.query_id,
            status: to_surface_status(reply.status),
        });
        Ok(serde_json::json!({"accepted": accepted}))
    }

    /// 把会话对话框产生的内部 effect 转译为通用宿主动作。
    fn drain_surface_effects(&mut self, host: &dyn PluginHostApi) {
        while let Some(effect) = self.surface.effects.pop_front() {
            match effect {
                SurfaceEffect::QuerySessions {
                    request_id,
                    query,
                    cursor,
                    limit,
                } => self.emit_action(
                    host,
                    UiHostAction::QuerySessions {
                        query_id: request_id,
                        query,
                        cursor,
                        limit,
                        reply_service: SURFACE_UPDATE_SERVICE.into(),
                    },
                ),
                SurfaceEffect::ResumeSession {
                    session_id,
                    revision,
                } => self.emit_action(
                    host,
                    UiHostAction::ResumeSession {
                        session_id,
                        revision,
                    },
                ),
                // 关闭状态由帧可见性表达，宿主的模态焦点随之消失。
                SurfaceEffect::CloseSurface => {}
            }
        }
    }

    /// 更新弹层内部快照并请求宿主替换主输入。
    fn set_host_input(&mut self, host: &dyn PluginHostApi, text: String, cursor: usize) {
        self.popup.sync(text.clone(), cursor);
        self.emit_action(
            host,
            UiHostAction::SetInput {
                text,
                cursor: u32::try_from(cursor).ok(),
            },
        );
    }

    /// 发布一条带单调请求 ID 的宿主动作事件。
    fn emit_action(&mut self, host: &dyn PluginHostApi, action: UiHostAction) {
        self.action_seq = self.action_seq.saturating_add(1);
        let request = UiHostActionRequest {
            request_id: format!("command-{}", self.action_seq),
            action,
        };
        // 事件发布失败没有可用的回退通道，保持静默避免弹层死循环。
        let _ = ExtensionEvent::host_action(request).and_then(|event| host.emit_event(&event));
    }

    /// 向主事件列表发布一条面向用户的说明。
    fn notify(
        &self,
        host: &dyn PluginHostApi,
        tone: EventPresentationTone,
        text: impl Into<String>,
    ) {
        let _ = host.emit_event(&ExtensionEvent {
            name: "command.notice".into(),
            data: Value::Null,
            presentation: Some(EventPresentation {
                target: EventPresentationTarget::MainEventList,
                variant: EventPresentationVariant::Text,
                tone,
                text: text.into(),
            }),
        });
    }

    /// 校验只有 manifest 配置的原生 TUI 调用方能写入会话应答通道。
    fn ensure_surface_authority(&self, caller_id: &str) -> Result<()> {
        if self.surface_authority != caller_id {
            return Err(anyhow!("调用方 `{caller_id}` 无权访问 Command surface"));
        }
        Ok(())
    }
}

/// 把宿主会话摘要映射为可插入命令行的参数候选。
fn session_completion_items(status: &UiSessionListStatus) -> Vec<CompletionItem> {
    let UiSessionListStatus::Ready { items, .. } = status else {
        return Vec::new();
    };
    items
        .iter()
        .take(usize::from(POPUP_COMPLETION_LIMIT))
        .map(|summary| CompletionItem {
            label: if summary.title.trim().is_empty() {
                summary.id.clone()
            } else {
                summary.title.clone()
            },
            insert_text: encode_command_token(&summary.id),
            description: Some(format!(
                "{} 条消息 · {}",
                summary.message_count, summary.updated_label
            )),
        })
        .collect()
}

/// 把宿主会话查询状态映射为对话框内部状态。
fn to_surface_status(status: UiSessionListStatus) -> SessionListStatus {
    match status {
        UiSessionListStatus::Ready { items, next_cursor } => SessionListStatus::Ready {
            items: items.into_iter().map(to_session_summary).collect(),
            next_cursor,
        },
        UiSessionListStatus::Empty => SessionListStatus::Empty,
        UiSessionListStatus::Error { message } => SessionListStatus::Error { message },
    }
}

/// 把宿主会话摘要映射为对话框协议的会话摘要。
fn to_session_summary(summary: UiSessionSummary) -> SessionSummary {
    SessionSummary {
        id: summary.id,
        title: summary.title,
        preview: String::new(),
        message_count: summary.message_count,
        updated_at_ms: summary.updated_at_ms,
        updated_label: summary.updated_label,
        revision: summary.revision,
        active: summary.active,
    }
}

/// 截断长命令输出，保持主事件列表可读。
fn truncate_output(content: &str) -> String {
    if content.chars().count() <= OUTPUT_PREVIEW_LIMIT {
        return content.to_string();
    }
    let mut truncated = content
        .chars()
        .take(OUTPUT_PREVIEW_LIMIT)
        .collect::<String>();
    truncated.push('…');
    truncated
}

/// 从激活上下文读取 surface 调用方，缺失或空值时保持官方 TUI 单一权限。
fn configured_surface_authority(context: &ActivationContext) -> String {
    context
        .metadata
        .get("surface_authority")
        .map(String::as_str)
        .map(str::trim)
        .filter(|authority| !authority.is_empty())
        .unwrap_or(DEFAULT_SURFACE_AUTHORITY)
        .into()
}

/// 返回插件对外服务及其开发者说明。
fn service_descriptions() -> [(&'static str, &'static str); 5] {
    [
        (REGISTER_SERVICE, "注册或替换调用方拥有的斜杠命令"),
        (UNREGISTER_SERVICE, "注销调用方拥有的斜杠命令"),
        (SNAPSHOT_SERVICE, "获取可缓存的命令注册表快照"),
        (
            PREPARE_COMPLETION_SERVICE,
            "显式识别当前参数并生成受控候选计划",
        ),
        (SURFACE_UPDATE_SERVICE, "接收宿主会话查询的异步应答"),
    ]
}

/// 把类型化响应转换为插件服务需要的 JSON 值。
fn to_value(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).context("序列化 Command 服务响应失败")
}

export_plugin!(CommandPlugin);

#[cfg(test)]
mod tests;

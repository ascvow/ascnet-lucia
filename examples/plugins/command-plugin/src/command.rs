//! Lucia 官方 Command Provider 插件。
//!
//! 插件维护命令注册表、解析命令行、生成受控执行计划，并提供完全由插件
//! 渲染和管理状态的会话选择对话框。TUI 只负责调用服务、注入当前项目的
//! 会话摘要，以及执行经过协议约束的 surface effect。

use agent_plugin::{
    export_plugin, ActivationContext, AgentPlugin, PluginHostApi, ServiceCall, ServiceSpec,
    UiColor, UiDeclaration, UiFrame, UiInput, UiInputEvent, UiLine, UiPlacement, UiRenderRequest,
    UiSize, UiSpan, UiStyle,
};
use anyhow::{anyhow, Context, Result};
use command_protocol::{
    canonical_command_name, encode_command_token, ArgumentKind, ArgumentSpec, CommandAvailability,
    CommandCallbackRequest, CommandCompletionRequest, CommandHandlerRef, CommandInvocation,
    CommandSnapshot, CommandSpec, CompletionContext, CompletionItem, CompletionSource,
    ParsedCommandLine, PrepareCompletionRequest, PrepareCompletionResponse, PrepareExecuteRequest,
    PrepareExecuteResponse, RegisterCommandRequest, RegisterCommandResponse, SessionListStatus,
    SessionSummary, SessionSurfaceMode, SnapshotRequest, SurfaceAction, SurfaceCompletionRequest,
    SurfaceEffect, SurfaceEffectsResponse, SurfaceUpdateRequest, UnregisterCommandRequest,
    UnregisterCommandResponse, DEFAULT_COMPLETION_LIMIT, MAX_COMPLETION_LIMIT,
    PREPARE_COMPLETION_SERVICE, PREPARE_EXECUTE_SERVICE, PROTOCOL_VERSION, REGISTER_SERVICE,
    SESSION_COMPLETION_SOURCE, SESSION_DIALOG_VIEW, SNAPSHOT_SERVICE, SURFACE_POLL_EFFECTS_SERVICE,
    SURFACE_UPDATE_SERVICE, UNREGISTER_SERVICE,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

mod registry;
mod surface;

use registry::*;
use surface::*;

/// 单次会话查询最多返回的摘要数量。
const SESSION_PAGE_LIMIT: u16 = 50;
/// manifest 未配置 surface 权限时仅允许官方 TUI 调用方。
const DEFAULT_SURFACE_AUTHORITY: &str = "lucia-tui";
/// Command 插件注册到 Host 的全部服务。
const SERVICES: [&str; 7] = [
    REGISTER_SERVICE,
    UNREGISTER_SERVICE,
    SNAPSHOT_SERVICE,
    PREPARE_COMPLETION_SERVICE,
    PREPARE_EXECUTE_SERVICE,
    SURFACE_UPDATE_SERVICE,
    SURFACE_POLL_EFFECTS_SERVICE,
];
/// 内置命令在注册表中的可信 owner。
const BUILTIN_OWNER: &str = "command";

/// 保存命令注册表与会话选择界面状态的官方插件。
struct CommandPlugin {
    registry: CommandRegistry,
    surface: SessionSurface,
    surface_authority: String,
}

impl Default for CommandPlugin {
    fn default() -> Self {
        Self {
            registry: CommandRegistry::with_builtins(),
            surface: SessionSurface::default(),
            surface_authority: DEFAULT_SURFACE_AUTHORITY.into(),
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

    /// 处理注册表、执行准备和会话选择界面数据服务。
    fn handle_service(&mut self, host: &dyn PluginHostApi, call: ServiceCall) -> Result<Value> {
        match call.name.as_str() {
            REGISTER_SERVICE => self.register(call),
            UNREGISTER_SERVICE => self.unregister(call),
            SNAPSHOT_SERVICE => self.snapshot(host, call),
            PREPARE_COMPLETION_SERVICE => self.prepare_completion(host, call),
            PREPARE_EXECUTE_SERVICE => self.prepare_execute(host, call),
            SURFACE_UPDATE_SERVICE => self.update_surface(call),
            SURFACE_POLL_EFFECTS_SERVICE => self.poll_surface_effects(call),
            _ => Err(anyhow!("未知 Command 服务：{}", call.name)),
        }
    }

    /// 声明一个默认隐藏、打开后优先接收输入的 Session Dialog。
    fn describe_ui(&self) -> Vec<UiDeclaration> {
        vec![UiDeclaration {
            plugin_id: String::new(),
            view_id: SESSION_DIALOG_VIEW.into(),
            title: "会话".into(),
            placement: UiPlacement::Dialog,
            size: UiSize {
                width: Some(76),
                height: Some(20),
            },
            focusable: true,
        }]
    }

    /// 渲染由插件拥有的会话列表、过滤文本和选中状态。
    fn render_ui(&mut self, request: UiRenderRequest) -> Option<UiFrame> {
        if request.view_id != SESSION_DIALOG_VIEW {
            return None;
        }
        Some(UiFrame {
            view_id: request.view_id,
            visible: self.surface.visible,
            lines: self.surface.render(request.width, request.height),
        })
    }

    /// 处理会话 Dialog 的键盘和鼠标输入，并生成待 TUI 轮询的 effect。
    fn on_ui_input(&mut self, input: UiInput) {
        if input.view_id == SESSION_DIALOG_VIEW && self.surface.visible {
            self.surface.handle_input(input.event);
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

    /// 返回不含 owner 身份的命令快照，供 TUI 在输入热路径本地缓存。
    fn snapshot(&mut self, host: &dyn PluginHostApi, call: ServiceCall) -> Result<Value> {
        let _: SnapshotRequest =
            serde_json::from_value(call.payload).context("解析 command.snapshot 请求失败")?;
        self.prune_unavailable_commands(host)?;
        to_value(self.registry.snapshot())
    }

    /// 显式生成参数候选计划，不在逐键输入热路径调用第三方插件或宿主数据源。
    fn prepare_completion(&mut self, host: &dyn PluginHostApi, call: ServiceCall) -> Result<Value> {
        let request: PrepareCompletionRequest = serde_json::from_value(call.payload)
            .context("解析 command.prepare-completion 请求失败")?;
        self.prune_unavailable_commands(host)?;
        to_value(self.registry.prepare_completion(request))
    }

    /// 解析命令并生成执行计划；`/resume` 和 `/sessions` 在这里打开插件 Dialog。
    fn prepare_execute(&mut self, host: &dyn PluginHostApi, call: ServiceCall) -> Result<Value> {
        let request: PrepareExecuteRequest = serde_json::from_value(call.payload)
            .context("解析 command.prepare-execute 请求失败")?;
        self.prune_unavailable_commands(host)?;
        let response = match self.registry.prepare(&request.input, request.agent_idle) {
            Prepared::Builtin {
                command,
                invocation,
            } => self.execute_builtin(command, invocation),
            Prepared::Callback {
                owner_plugin_id,
                handler,
                invocation,
            } => PrepareExecuteResponse::Callback {
                owner_plugin_id,
                service: handler.service,
                request: CommandCallbackRequest::Execute {
                    handler_id: handler.handler_id,
                    invocation,
                },
            },
            Prepared::Error { message, usage } => PrepareExecuteResponse::Error { message, usage },
        };
        to_value(response)
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

    /// 接受与最近请求 ID 匹配的会话查询结果，过期响应只返回 `accepted=false`。
    fn update_surface(&mut self, call: ServiceCall) -> Result<Value> {
        self.ensure_surface_authority(&call.caller_id)?;
        let request: SurfaceUpdateRequest =
            serde_json::from_value(call.payload).context("解析 command.surface.update 请求失败")?;
        let accepted = self.surface.update(request);
        Ok(serde_json::json!({"accepted": accepted}))
    }

    /// 原子取出插件界面产生的 effect，避免重复恢复同一 Session。
    fn poll_surface_effects(&mut self, call: ServiceCall) -> Result<Value> {
        self.ensure_surface_authority(&call.caller_id)?;
        to_value(SurfaceEffectsResponse {
            effects: self.surface.effects.drain(..).collect(),
        })
    }

    /// 校验只有 manifest 配置的原生 TUI 调用方能读写 surface 通道。
    fn ensure_surface_authority(&self, caller_id: &str) -> Result<()> {
        if self.surface_authority != caller_id {
            return Err(anyhow!("调用方 `{caller_id}` 无权访问 Command surface"));
        }
        Ok(())
    }

    /// 执行不会跨插件回调的官方内置命令。
    fn execute_builtin(
        &mut self,
        command: BuiltinCommand,
        invocation: CommandInvocation,
    ) -> PrepareExecuteResponse {
        match command {
            BuiltinCommand::Help => {
                let target = invocation
                    .arguments
                    .get("command")
                    .and_then(|values| values.first())
                    .map(String::as_str);
                PrepareExecuteResponse::Output {
                    content: self.registry.help(target),
                }
            }
            BuiltinCommand::Resume => {
                self.surface.open(SessionSurfaceMode::Resume);
                PrepareExecuteResponse::SurfaceOpened {
                    view_id: SESSION_DIALOG_VIEW.into(),
                }
            }
            BuiltinCommand::Sessions => {
                self.surface.open(SessionSurfaceMode::Browse);
                PrepareExecuteResponse::SurfaceOpened {
                    view_id: SESSION_DIALOG_VIEW.into(),
                }
            }
            BuiltinCommand::New => PrepareExecuteResponse::SurfaceAction {
                action: SurfaceAction::NewSession,
            },
            BuiltinCommand::Clear => PrepareExecuteResponse::SurfaceAction {
                action: SurfaceAction::ClearSession,
            },
            BuiltinCommand::Compact => PrepareExecuteResponse::SurfaceAction {
                action: SurfaceAction::ReloadSessionContext,
            },
            BuiltinCommand::Exit => PrepareExecuteResponse::SurfaceAction {
                action: SurfaceAction::ExitApplication,
            },
        }
    }
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
fn service_descriptions() -> [(&'static str, &'static str); 7] {
    [
        (REGISTER_SERVICE, "注册或替换调用方拥有的斜杠命令"),
        (UNREGISTER_SERVICE, "注销调用方拥有的斜杠命令"),
        (SNAPSHOT_SERVICE, "获取可缓存的命令注册表快照"),
        (
            PREPARE_COMPLETION_SERVICE,
            "显式识别当前参数并生成受控候选计划",
        ),
        (PREPARE_EXECUTE_SERVICE, "解析命令并生成受控执行计划"),
        (SURFACE_UPDATE_SERVICE, "向插件会话界面注入异步摘要"),
        (
            SURFACE_POLL_EFFECTS_SERVICE,
            "轮询并清空插件会话界面的待处理动作",
        ),
    ]
}

/// 把类型化响应转换为插件服务需要的 JSON 值。
fn to_value(value: impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).context("序列化 Command 服务响应失败")
}

export_plugin!(CommandPlugin);

#[cfg(test)]
mod tests;

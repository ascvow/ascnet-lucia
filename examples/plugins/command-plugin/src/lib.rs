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
                action: SurfaceAction::CompactSession,
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

/// 注册表内部保存的命令 owner、定义和可选内置处理器。
#[derive(Debug, Clone)]
struct RegisteredCommand {
    owner_plugin_id: String,
    spec: CommandSpec,
    builtin: Option<BuiltinCommand>,
}

/// 命令准备阶段的内部结果。
enum Prepared {
    Builtin {
        command: BuiltinCommand,
        invocation: CommandInvocation,
    },
    Callback {
        owner_plugin_id: String,
        handler: CommandHandlerRef,
        invocation: CommandInvocation,
    },
    Error {
        message: String,
        usage: Option<String>,
    },
}

/// 官方内置命令的稳定路由标识。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinCommand {
    Help,
    Resume,
    New,
    Sessions,
    Clear,
    /// 请求官方 Context 插件在下一轮强制压缩较旧历史。
    Compact,
    Exit,
}

/// 按规范名称和别名索引命令的内存注册表。
#[derive(Debug)]
struct CommandRegistry {
    commands: BTreeMap<String, RegisteredCommand>,
    aliases: BTreeMap<String, String>,
    generation: u64,
}

impl CommandRegistry {
    /// 创建只包含官方默认命令的第一代注册表。
    fn with_builtins() -> Self {
        let mut registry = Self {
            commands: BTreeMap::new(),
            aliases: BTreeMap::new(),
            generation: 1,
        };
        for (spec, command) in builtin_specs() {
            registry.insert_unchecked(RegisteredCommand {
                owner_plugin_id: BUILTIN_OWNER.into(),
                spec,
                builtin: Some(command),
            });
        }
        registry
    }

    /// 注册或替换同一 owner 的命令，同时保持冲突检查事务性。
    fn register(&mut self, owner_plugin_id: String, spec: CommandSpec) -> Result<String> {
        validate_spec(&spec, false)?;
        let canonical = spec.name.clone();
        let existing = self.commands.get(&canonical);
        if let Some(existing) = existing {
            if existing.builtin.is_some() || existing.owner_plugin_id != owner_plugin_id {
                return Err(anyhow!("命令 `/{canonical}` 已由其他插件注册"));
            }
        }

        let excluded = existing
            .map(|entry| command_names(&entry.spec))
            .unwrap_or_default();
        for name in command_names(&spec) {
            if excluded.contains(&name) {
                continue;
            }
            if self.commands.contains_key(&name) || self.aliases.contains_key(&name) {
                return Err(anyhow!("命令名称或别名 `/{name}` 已被占用"));
            }
        }

        if existing.is_some() {
            self.remove_unchecked(&canonical);
        }
        self.insert_unchecked(RegisteredCommand {
            owner_plugin_id,
            spec,
            builtin: None,
        });
        self.generation = self.generation.saturating_add(1);
        Ok(canonical)
    }

    /// 注销命令，未知名称保持幂等且不递增 generation。
    fn unregister(&mut self, owner_plugin_id: &str, name: &str) -> Result<bool> {
        let Some(canonical) = canonical_command_name(name) else {
            return Err(anyhow!("无效命令名：{name}"));
        };
        let canonical = self
            .resolve_name(&canonical)
            .map(str::to_string)
            .unwrap_or(canonical);
        let Some(entry) = self.commands.get(&canonical) else {
            return Ok(false);
        };
        if entry.builtin.is_some() {
            return Err(anyhow!("官方内置命令 `/{canonical}` 不能被注销"));
        }
        if entry.owner_plugin_id != owner_plugin_id {
            return Err(anyhow!("不能注销其他插件拥有的命令 `/{canonical}`"));
        }
        self.remove_unchecked(&canonical);
        self.generation = self.generation.saturating_add(1);
        Ok(true)
    }

    /// 返回按规范名称排序且不暴露 owner 的命令定义。
    fn snapshot(&self) -> CommandSnapshot {
        CommandSnapshot {
            generation: self.generation,
            commands: self
                .commands
                .values()
                .map(|entry| entry.spec.clone())
                .collect(),
        }
    }

    /// 批量移除回调服务已从 Host 目录消失的第三方命令。
    fn prune_unavailable_handlers(
        &mut self,
        services_by_owner: &BTreeMap<String, BTreeSet<String>>,
    ) -> usize {
        let stale = self
            .commands
            .iter()
            .filter_map(|(name, entry)| {
                if entry.builtin.is_some() {
                    return None;
                }
                let handler = entry.spec.handler.as_ref()?;
                let available = services_by_owner
                    .get(&entry.owner_plugin_id)
                    .is_some_and(|services| services.contains(&handler.service));
                (!available).then(|| name.clone())
            })
            .collect::<Vec<_>>();
        for name in &stale {
            self.remove_unchecked(name);
        }
        if !stale.is_empty() {
            self.generation = self.generation.saturating_add(1);
        }
        stale.len()
    }

    /// 识别当前参数，并按注册时保存的可信 owner 生成本地、回调或宿主计划。
    fn prepare_completion(&self, request: PrepareCompletionRequest) -> PrepareCompletionResponse {
        let cursor = match request.cursor {
            Some(cursor) => match usize::try_from(cursor) {
                Ok(cursor) => cursor,
                Err(_) => {
                    return PrepareCompletionResponse::Error {
                        message: "补全光标超出当前平台支持范围".into(),
                    }
                }
            },
            None => request.input.len(),
        };
        let cursor = match parse_completion_cursor(&request.input, cursor) {
            Ok(Some(cursor)) => cursor,
            Ok(None) => return PrepareCompletionResponse::NoMatch,
            Err(message) => return PrepareCompletionResponse::Error { message },
        };
        let Some(name) = canonical_command_name(&cursor.command) else {
            return PrepareCompletionResponse::NoMatch;
        };
        let Some(canonical) = self.resolve_name(&name) else {
            return PrepareCompletionResponse::NoMatch;
        };
        let entry = &self.commands[canonical];
        let Some((argument_index, argument)) =
            completion_argument(&entry.spec, cursor.argument_index)
        else {
            return PrepareCompletionResponse::NoMatch;
        };
        let limit = normalize_completion_limit(request.limit);
        let argument_index = match u16::try_from(argument_index) {
            Ok(index) => index,
            Err(_) => {
                return PrepareCompletionResponse::Error {
                    message: "命令参数数量超出补全协议支持范围".into(),
                }
            }
        };
        let context = CompletionContext {
            command: canonical.into(),
            argument: argument.name.clone(),
            argument_index,
            prefix: cursor.prefix.clone(),
            replacement_start: match u32::try_from(cursor.replacement_start) {
                Ok(start) => start,
                Err(_) => {
                    return PrepareCompletionResponse::Error {
                        message: "补全替换范围超出协议支持范围".into(),
                    }
                }
            },
            replacement_end: match u32::try_from(cursor.replacement_end) {
                Ok(end) => end,
                Err(_) => {
                    return PrepareCompletionResponse::Error {
                        message: "补全替换范围超出协议支持范围".into(),
                    }
                }
            },
        };
        let completion_request = CommandCompletionRequest {
            command: canonical.into(),
            argument: argument.name.clone(),
            prefix: cursor.prefix,
            input: request.input,
            limit,
        };

        match &argument.completion {
            CompletionSource::Static { items } => PrepareCompletionResponse::Candidates {
                context,
                items: filter_completion_items(
                    items.iter().cloned(),
                    &completion_request.prefix,
                    limit,
                ),
            },
            CompletionSource::Callback => {
                let Some(handler) = entry.spec.handler.as_ref() else {
                    return PrepareCompletionResponse::NoMatch;
                };
                PrepareCompletionResponse::Callback {
                    context,
                    owner_plugin_id: entry.owner_plugin_id.clone(),
                    service: handler.service.clone(),
                    request: CommandCallbackRequest::Complete {
                        handler_id: handler.handler_id.clone(),
                        request: completion_request,
                    },
                }
            }
            CompletionSource::Surface { source } => PrepareCompletionResponse::Surface {
                context,
                request: SurfaceCompletionRequest {
                    source: source.clone(),
                    request: completion_request,
                },
            },
            CompletionSource::None => match &argument.kind {
                ArgumentKind::Choice { values } => PrepareCompletionResponse::Candidates {
                    context,
                    items: filter_completion_items(
                        values.iter().map(|value| CompletionItem {
                            label: value.clone(),
                            insert_text: value.clone(),
                            description: None,
                        }),
                        &completion_request.prefix,
                        limit,
                    ),
                },
                ArgumentKind::Session => PrepareCompletionResponse::Surface {
                    context,
                    request: SurfaceCompletionRequest {
                        source: SESSION_COMPLETION_SOURCE.into(),
                        request: completion_request,
                    },
                },
                _ => PrepareCompletionResponse::NoMatch,
            },
        }
    }

    /// 解析命令行、解析别名并绑定类型化位置参数。
    fn prepare(&self, input: &str, agent_idle: bool) -> Prepared {
        let parsed = match ParsedCommandLine::parse(input) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Prepared::Error {
                    message: error.to_string(),
                    usage: None,
                }
            }
        };
        let Some(name) = canonical_command_name(&parsed.name) else {
            return Prepared::Error {
                message: format!("无效命令名：{}", parsed.name),
                usage: None,
            };
        };
        let Some(canonical) = self.resolve_name(&name) else {
            return Prepared::Error {
                message: format!("未知命令：/{}", parsed.name),
                usage: Some("输入 /help 查看可用命令".into()),
            };
        };
        let entry = &self.commands[canonical];
        if entry.spec.availability == CommandAvailability::IdleOnly && !agent_idle {
            return Prepared::Error {
                message: format!("命令 `/{canonical}` 只能在 Agent 空闲时执行"),
                usage: Some(entry.spec.display_usage()),
            };
        }
        let invocation = match bind_arguments(&entry.spec, parsed.arguments, input) {
            Ok(invocation) => invocation,
            Err(message) => {
                return Prepared::Error {
                    message,
                    usage: Some(entry.spec.display_usage()),
                }
            }
        };
        if let Some(builtin) = entry.builtin {
            return Prepared::Builtin {
                command: builtin,
                invocation,
            };
        }
        let Some(handler) = entry.spec.handler.clone() else {
            return Prepared::Error {
                message: format!("命令 `/{canonical}` 没有可用处理器"),
                usage: Some(entry.spec.display_usage()),
            };
        };
        Prepared::Callback {
            owner_plugin_id: entry.owner_plugin_id.clone(),
            handler,
            invocation,
        }
    }

    /// 生成全部命令或单个命令的帮助文本。
    fn help(&self, target: Option<&str>) -> String {
        if let Some(target) = target {
            let target = target.trim_start_matches('/');
            let Some(name) = canonical_command_name(target) else {
                return format!("无效命令名：{target}");
            };
            let Some(canonical) = self.resolve_name(&name) else {
                return format!("未知命令：/{target}");
            };
            let spec = &self.commands[canonical].spec;
            let mut output = format!(
                "{}\n{}\n\n{}",
                spec.display_usage(),
                spec.summary,
                spec.description
            );
            if !spec.aliases.is_empty() {
                output.push_str(&format!("\n\n别名：/{}", spec.aliases.join("、/")));
            }
            if !spec.arguments.is_empty() {
                output.push_str("\n\n参数：");
                for argument in &spec.arguments {
                    output.push_str(&format!("\n  {}  {}", argument.name, argument.description));
                }
            }
            return output;
        }

        let mut output = String::from("可用命令：");
        for entry in self.commands.values() {
            output.push_str(&format!(
                "\n  {:<24} {}",
                entry.spec.display_usage(),
                entry.spec.summary
            ));
        }
        output
    }

    /// 根据规范名称或别名返回规范名称。
    fn resolve_name<'a>(&'a self, name: &'a str) -> Option<&'a str> {
        if self.commands.contains_key(name) {
            Some(name)
        } else {
            self.aliases.get(name).map(String::as_str)
        }
    }

    /// 插入已经通过校验且不存在冲突的命令。
    fn insert_unchecked(&mut self, entry: RegisteredCommand) {
        let canonical = entry.spec.name.clone();
        for alias in &entry.spec.aliases {
            self.aliases.insert(alias.clone(), canonical.clone());
        }
        self.commands.insert(canonical, entry);
    }

    /// 移除命令及其全部别名，不修改 generation。
    fn remove_unchecked(&mut self, canonical: &str) {
        if let Some(entry) = self.commands.remove(canonical) {
            for alias in entry.spec.aliases {
                self.aliases.remove(&alias);
            }
        }
    }
}

/// Provider 在原始输入中识别出的当前参数游标。
struct CompletionCursorState {
    command: String,
    argument_index: usize,
    prefix: String,
    replacement_start: usize,
    replacement_end: usize,
}

/// 解析光标前的宽松命令行状态，并计算当前原始 token 的完整替换范围。
///
/// 与执行解析不同，补全允许光标处存在尚未闭合的引号或转义。
fn parse_completion_cursor(
    input: &str,
    cursor: usize,
) -> std::result::Result<Option<CompletionCursorState>, String> {
    if cursor > input.len() || !input.is_char_boundary(cursor) {
        return Err("补全光标不是有效的 UTF-8 字节位置".into());
    }
    let trimmed = input.trim_start();
    let input_start = input.len().saturating_sub(trimmed.len());
    if !trimmed.starts_with('/') {
        return Err("命令必须以 `/` 开头".into());
    }
    let command_start = input_start.saturating_add(1);
    if cursor <= command_start {
        return Ok(None);
    }
    let before_cursor = &input[command_start..cursor];
    let Some((separator_offset, _)) = before_cursor
        .char_indices()
        .find(|(_, character)| character.is_whitespace())
    else {
        return Ok(None);
    };
    let command_end = command_start + separator_offset;
    let command = &input[command_start..command_end];
    if command.is_empty() {
        return Ok(None);
    }

    let mut argument_index = 0usize;
    let mut token_start = None;
    let mut prefix = String::new();
    let mut quote = None;
    let mut escaped = false;
    for (offset, character) in input[command_end..cursor].char_indices() {
        let absolute = command_end + offset;
        if token_start.is_none() {
            if character.is_whitespace() {
                continue;
            }
            token_start = Some(absolute);
        }
        if escaped {
            prefix.push(character);
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => prefix.push(character),
            (Some('"'), '"') => quote = None,
            (Some('"'), '\\') => escaped = true,
            (Some('"'), _) => prefix.push(character),
            (None, '\'') => quote = Some('\''),
            (None, '"') => quote = Some('"'),
            (None, '\\') => escaped = true,
            (None, value) if value.is_whitespace() => {
                argument_index = argument_index.saturating_add(1);
                token_start = None;
                prefix.clear();
            }
            (None, value) => prefix.push(value),
            (Some(_), _) => unreachable!("补全解析只支持单双引号"),
        }
    }

    let replacement_start = token_start.unwrap_or(cursor);
    let replacement_end = if token_start.is_some() {
        completion_token_end(input, cursor, quote, escaped)
    } else {
        cursor
    };
    Ok(Some(CompletionCursorState {
        command: command.into(),
        argument_index,
        prefix,
        replacement_start,
        replacement_end,
    }))
}

/// 从光标向后找到当前 token 的边界，保留完整 token 替换所需的引号状态。
fn completion_token_end(
    input: &str,
    cursor: usize,
    mut quote: Option<char>,
    mut escaped: bool,
) -> usize {
    for (offset, character) in input[cursor..].char_indices() {
        let absolute = cursor + offset;
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, character) {
            (Some('\''), '\'') => quote = None,
            (Some('\''), _) => {}
            (Some('"'), '"') => quote = None,
            (Some('"'), '\\') => escaped = true,
            (Some('"'), _) => {}
            (None, '\'') => quote = Some('\''),
            (None, '"') => quote = Some('"'),
            (None, '\\') => escaped = true,
            (None, value) if value.is_whitespace() => return absolute,
            (None, _) => {}
            (Some(_), _) => unreachable!("补全解析只支持单双引号"),
        }
    }
    input.len()
}

/// 将输入中的参数位置映射到定义；可变参数会持续匹配最后一个定义。
fn completion_argument(spec: &CommandSpec, input_index: usize) -> Option<(usize, &ArgumentSpec)> {
    if let Some(argument) = spec.arguments.get(input_index) {
        return Some((input_index, argument));
    }
    let index = spec.arguments.len().checked_sub(1)?;
    let argument = &spec.arguments[index];
    argument.variadic.then_some((index, argument))
}

/// 把调用方上限归一到协议默认值和硬上限之间。
fn normalize_completion_limit(limit: u16) -> u16 {
    if limit == 0 {
        DEFAULT_COMPLETION_LIMIT
    } else {
        limit.min(MAX_COMPLETION_LIMIT)
    }
}

/// 按标签或参数值前缀过滤候选，并编码插入文本、应用数量上限。
fn filter_completion_items(
    items: impl IntoIterator<Item = CompletionItem>,
    prefix: &str,
    limit: u16,
) -> Vec<CompletionItem> {
    let prefix = prefix.to_lowercase();
    items
        .into_iter()
        .filter(|item| {
            prefix.is_empty()
                || item.label.to_lowercase().starts_with(&prefix)
                || item.insert_text.to_lowercase().starts_with(&prefix)
        })
        .take(usize::from(limit))
        .map(|mut item| {
            item.insert_text = encode_command_token(&item.insert_text);
            item
        })
        .collect()
}

/// 返回命令规范名称与别名的集合。
fn command_names(spec: &CommandSpec) -> BTreeSet<String> {
    std::iter::once(spec.name.clone())
        .chain(spec.aliases.iter().cloned())
        .collect()
}

/// 校验外部命令定义，拒绝模糊名称、不可执行命令和歧义参数。
fn validate_spec(spec: &CommandSpec, builtin: bool) -> Result<()> {
    let canonical =
        canonical_command_name(&spec.name).ok_or_else(|| anyhow!("无效命令名：{}", spec.name))?;
    if canonical != spec.name {
        return Err(anyhow!("命令名必须是规范小写形式：{canonical}"));
    }
    if spec.summary.trim().is_empty() || spec.description.trim().is_empty() {
        return Err(anyhow!("命令摘要和描述不能为空"));
    }
    if !builtin {
        let handler = spec
            .handler
            .as_ref()
            .ok_or_else(|| anyhow!("第三方命令必须声明 handler"))?;
        if handler.service.trim().is_empty() || handler.handler_id.trim().is_empty() {
            return Err(anyhow!("命令 handler service 和 handler_id 不能为空"));
        }
    }

    let mut names = BTreeSet::from([spec.name.clone()]);
    for alias in &spec.aliases {
        let canonical_alias =
            canonical_command_name(alias).ok_or_else(|| anyhow!("无效命令别名：{alias}"))?;
        if canonical_alias != *alias {
            return Err(anyhow!("命令别名必须是规范小写形式：{canonical_alias}"));
        }
        if !names.insert(alias.clone()) {
            return Err(anyhow!("命令名称或别名重复：{alias}"));
        }
    }

    let mut argument_names = BTreeSet::new();
    let mut optional_seen = false;
    for (index, argument) in spec.arguments.iter().enumerate() {
        if canonical_command_name(&argument.name).as_deref() != Some(argument.name.as_str()) {
            return Err(anyhow!("无效参数名：{}", argument.name));
        }
        if argument.description.trim().is_empty() {
            return Err(anyhow!("参数 `{}` 的描述不能为空", argument.name));
        }
        if !argument_names.insert(argument.name.clone()) {
            return Err(anyhow!("参数名重复：{}", argument.name));
        }
        if optional_seen && argument.required {
            return Err(anyhow!("必填参数不能出现在可选参数之后"));
        }
        optional_seen |= !argument.required;
        if argument.variadic && index + 1 != spec.arguments.len() {
            return Err(anyhow!("可变参数 `{}` 必须位于最后", argument.name));
        }
        if let ArgumentKind::Choice { values } = &argument.kind {
            if values.is_empty() || values.iter().any(|value| value.is_empty()) {
                return Err(anyhow!("Choice 参数 `{}` 必须提供非空候选", argument.name));
            }
        }
        match &argument.completion {
            CompletionSource::Static { items }
                if items.iter().any(|item| item.insert_text.is_empty()) =>
            {
                return Err(anyhow!(
                    "Static 参数 `{}` 的插入文本不能为空",
                    argument.name
                ));
            }
            CompletionSource::Surface { source } if source.trim().is_empty() => {
                return Err(anyhow!("Surface 参数 `{}` 的数据源不能为空", argument.name));
            }
            _ => {}
        }
    }
    Ok(())
}

/// 按命令定义绑定并校验位置参数。
fn bind_arguments(
    spec: &CommandSpec,
    values: Vec<String>,
    input: &str,
) -> std::result::Result<CommandInvocation, String> {
    let mut cursor = 0;
    let mut arguments = BTreeMap::new();
    for argument in &spec.arguments {
        let selected = if argument.variadic {
            let selected = values[cursor..].to_vec();
            cursor = values.len();
            selected
        } else if let Some(value) = values.get(cursor) {
            cursor += 1;
            vec![value.clone()]
        } else {
            Vec::new()
        };
        if selected.is_empty() {
            if argument.required {
                return Err(format!("缺少必填参数：{}", argument.name));
            }
            continue;
        }
        for value in &selected {
            validate_argument_value(argument, value)?;
        }
        arguments.insert(argument.name.clone(), selected);
    }
    if cursor < values.len() {
        return Err(format!("命令 `/{}` 收到多余参数", spec.name));
    }
    Ok(CommandInvocation {
        command: spec.name.clone(),
        input: input.into(),
        arguments,
    })
}

/// 根据参数类型校验单个原始值，不改变第三方插件最终收到的文本。
fn validate_argument_value(
    argument: &ArgumentSpec,
    value: &str,
) -> std::result::Result<(), String> {
    if value.is_empty() {
        return Err(format!("参数 `{}` 不能为空", argument.name));
    }
    let valid = match &argument.kind {
        ArgumentKind::String | ArgumentKind::Session => true,
        ArgumentKind::Integer => value.parse::<i64>().is_ok(),
        ArgumentKind::Boolean => matches!(
            value.to_ascii_lowercase().as_str(),
            "true" | "false" | "yes" | "no" | "1" | "0"
        ),
        ArgumentKind::Choice { values } => values.iter().any(|candidate| candidate == value),
    };
    if valid {
        Ok(())
    } else {
        Err(format!("参数 `{}` 的值无效：{value}", argument.name))
    }
}

/// 构造官方内置命令及其处理器路由。
fn builtin_specs() -> Vec<(CommandSpec, BuiltinCommand)> {
    vec![
        (
            CommandSpec::new("help", "查看命令帮助", "显示全部命令或指定命令的详细用法。")
                .with_argument(ArgumentSpec::optional(
                    "command",
                    "不含前导斜杠的命令名",
                    ArgumentKind::String,
                )),
            BuiltinCommand::Help,
        ),
        (
            idle_command(
                "resume",
                "恢复历史会话",
                "打开当前工作目录的会话列表，选择后恢复会话。",
            ),
            BuiltinCommand::Resume,
        ),
        (
            idle_command(
                "new",
                "新建空白会话",
                "结束当前会话并进入不会立即落盘的空白草稿。",
            ),
            BuiltinCommand::New,
        ),
        (
            idle_command(
                "sessions",
                "浏览项目会话",
                "打开当前工作目录的只读会话列表。",
            ),
            BuiltinCommand::Sessions,
        ),
        (
            idle_command(
                "clear",
                "清空当前上下文",
                "清空当前会话上下文并进入新的空白草稿。",
            ),
            BuiltinCommand::Clear,
        ),
        (
            idle_command(
                "compact",
                "主动压缩当前上下文",
                "立即压缩当前会话的较旧历史，并持久化压缩后的上下文。",
            ),
            BuiltinCommand::Compact,
        ),
        (
            idle_command("exit", "退出 Lucia", "请求 TUI 保存状态并正常退出 Lucia。")
                .with_alias("quit"),
            BuiltinCommand::Exit,
        ),
    ]
}

/// 创建只允许 Agent 空闲时执行的内置命令。
fn idle_command(name: &str, summary: &str, description: &str) -> CommandSpec {
    let mut spec = CommandSpec::new(name, summary, description);
    spec.availability = CommandAvailability::IdleOnly;
    spec
}

/// 会话 Dialog 的全部交互和异步加载状态。
struct SessionSurface {
    visible: bool,
    mode: SessionSurfaceMode,
    query: String,
    status: SessionListStatus,
    selected: usize,
    rendered_start: usize,
    rendered_len: usize,
    request_id: u64,
    effects: VecDeque<SurfaceEffect>,
}

impl Default for SessionSurface {
    fn default() -> Self {
        Self {
            visible: false,
            mode: SessionSurfaceMode::Resume,
            query: String::new(),
            status: SessionListStatus::Empty,
            selected: 0,
            rendered_start: 0,
            rendered_len: 0,
            request_id: 0,
            effects: VecDeque::new(),
        }
    }
}

impl SessionSurface {
    /// 打开并重置界面，然后请求当前 `cwd` 的第一页会话摘要。
    fn open(&mut self, mode: SessionSurfaceMode) {
        self.visible = true;
        self.mode = mode;
        self.query.clear();
        self.selected = 0;
        self.rendered_start = 0;
        self.rendered_len = 0;
        self.queue_query(None);
    }

    /// 仅接受最近一次查询的响应，防止快速输入时旧结果覆盖新结果。
    fn update(&mut self, request: SurfaceUpdateRequest) -> bool {
        if !self.visible || request.request_id != self.request_id {
            return false;
        }
        self.status = match request.status {
            SessionListStatus::Ready { items, .. } if items.is_empty() => SessionListStatus::Empty,
            status => status,
        };
        self.selected = self.selected.min(self.items().len().saturating_sub(1));
        self.rendered_start = 0;
        self.rendered_len = 0;
        true
    }

    /// 处理 Dialog 的稳定输入事件。
    fn handle_input(&mut self, event: UiInputEvent) {
        match event {
            UiInputEvent::Key { code, modifiers } => self.handle_key(&code, &modifiers),
            UiInputEvent::Mouse { kind, y, .. } => self.handle_mouse(&kind, y),
        }
    }

    /// 处理导航、过滤、确认和关闭按键。
    fn handle_key(&mut self, code: &str, modifiers: &[String]) {
        match code {
            "escape" => self.close(),
            "up" => self.selected = self.selected.saturating_sub(1),
            "down" => {
                if self.selected + 1 >= self.items().len() {
                    if let Some(cursor) = self.next_cursor() {
                        self.selected = 0;
                        self.queue_query(Some(cursor));
                        return;
                    }
                }
                self.selected = (self.selected + 1).min(self.items().len().saturating_sub(1));
            }
            "pageup" => self.selected = self.selected.saturating_sub(10),
            "pagedown" => {
                if self.selected + 10 >= self.items().len() {
                    if let Some(cursor) = self.next_cursor() {
                        self.selected = 0;
                        self.queue_query(Some(cursor));
                        return;
                    }
                }
                self.selected = (self.selected + 10).min(self.items().len().saturating_sub(1));
            }
            "home" => self.selected = 0,
            "end" => self.selected = self.items().len().saturating_sub(1),
            "backspace" => {
                if self.query.pop().is_some() {
                    self.selected = 0;
                    self.queue_query(None);
                }
            }
            "enter" => self.confirm_selection(),
            _ if is_printable_key(code, modifiers) => {
                self.query.push_str(code);
                self.selected = 0;
                self.queue_query(None);
            }
            _ => {}
        }
    }

    /// 将鼠标滚轮和列表行点击映射为选择状态。
    fn handle_mouse(&mut self, kind: &str, y: u16) {
        match kind {
            "scroll_up" => self.selected = self.selected.saturating_sub(1),
            "scroll_down" => {
                if self.selected + 1 >= self.items().len() {
                    if let Some(cursor) = self.next_cursor() {
                        self.selected = 0;
                        self.queue_query(Some(cursor));
                        return;
                    }
                }
                self.selected = (self.selected + 1).min(self.items().len().saturating_sub(1));
            }
            value if value.starts_with("down_") && y >= 3 => {
                let rendered_row = usize::from(y - 3);
                let index = self.rendered_start.saturating_add(rendered_row);
                if rendered_row < self.rendered_len && index < self.items().len() {
                    self.selected = index;
                }
            }
            _ => {}
        }
    }

    /// 在恢复模式中确认选中项，并立即隐藏对话框避免重复提交。
    fn confirm_selection(&mut self) {
        if self.mode != SessionSurfaceMode::Resume {
            return;
        }
        let Some(item) = self.items().get(self.selected).cloned() else {
            return;
        };
        if item.active {
            return;
        }
        self.visible = false;
        self.effects.push_back(SurfaceEffect::ResumeSession {
            session_id: item.id,
            revision: item.revision,
        });
    }

    /// 隐藏界面并通知 TUI 取消 Dialog 焦点。
    fn close(&mut self) {
        self.visible = false;
        self.effects.push_back(SurfaceEffect::CloseSurface);
    }

    /// 合并连续查询，只保留最新的轻量会话摘要请求。
    fn queue_query(&mut self, cursor: Option<String>) {
        self.request_id = self.request_id.saturating_add(1).max(1);
        self.status = SessionListStatus::Loading;
        self.rendered_start = 0;
        self.rendered_len = 0;
        self.effects
            .retain(|effect| !matches!(effect, SurfaceEffect::QuerySessions { .. }));
        self.effects.push_back(SurfaceEffect::QuerySessions {
            request_id: self.request_id,
            query: self.query.clone(),
            cursor,
            limit: SESSION_PAGE_LIMIT,
        });
    }

    /// 返回当前可选择的会话摘要切片。
    fn items(&self) -> &[SessionSummary] {
        match &self.status {
            SessionListStatus::Ready { items, .. } => items,
            _ => &[],
        }
    }

    /// 返回当前页的下一页游标，调用方可以安全地在状态变更前克隆它。
    fn next_cursor(&self) -> Option<String> {
        match &self.status {
            SessionListStatus::Ready { next_cursor, .. } => next_cursor.clone(),
            _ => None,
        }
    }

    /// 根据宿主分配尺寸渲染稳定高度的 Dialog 内容。
    fn render(&mut self, width: u16, height: u16) -> Vec<UiLine> {
        if !self.visible {
            self.rendered_start = 0;
            self.rendered_len = 0;
            return Vec::new();
        }
        let content_width = usize::from(width.saturating_sub(2)).max(1);
        let title = match self.mode {
            SessionSurfaceMode::Resume => "恢复会话",
            SessionSurfaceMode::Browse => "项目会话",
        };
        let mut lines = vec![
            line(vec![styled(title, UiColor::Cyan, true, false)]),
            line(vec![
                styled("搜索  ", UiColor::Gray, false, false),
                plain(if self.query.is_empty() {
                    "输入关键词过滤"
                } else {
                    &self.query
                }),
            ]),
            line(vec![plain("")]),
        ];

        let list_height = usize::from(height.saturating_sub(6)).max(1);
        self.rendered_start = visible_window_start(self.selected, self.items().len(), list_height);
        self.rendered_len = self
            .items()
            .len()
            .saturating_sub(self.rendered_start)
            .min(list_height);
        match &self.status {
            SessionListStatus::Loading => {
                lines.push(line(vec![styled(
                    "正在加载会话...",
                    UiColor::Yellow,
                    false,
                    false,
                )]));
            }
            SessionListStatus::Empty => {
                lines.push(line(vec![styled(
                    "当前工作目录没有匹配会话",
                    UiColor::Gray,
                    false,
                    false,
                )]));
            }
            SessionListStatus::Error { message } => {
                lines.push(line(vec![styled(
                    &clip(message, content_width),
                    UiColor::Red,
                    false,
                    false,
                )]));
            }
            SessionListStatus::Ready { items, next_cursor } => {
                for (offset, item) in items
                    .iter()
                    .skip(self.rendered_start)
                    .take(list_height)
                    .enumerate()
                {
                    let index = self.rendered_start + offset;
                    lines.push(render_session_line(
                        item,
                        index == self.selected,
                        content_width,
                    ));
                }
                if next_cursor.is_some() && items.len() < list_height {
                    lines.push(line(vec![styled(
                        "还有更多会话",
                        UiColor::Gray,
                        false,
                        false,
                    )]));
                }
            }
        }

        while lines.len() < usize::from(height.saturating_sub(2)) {
            lines.push(line(vec![plain("")]));
        }
        lines.push(line(vec![plain("")]));
        lines.truncate(usize::from(height));
        lines
    }
}

/// 让选中项始终处在固定高度列表窗口内，并尽量保持窗口稳定靠前。
fn visible_window_start(selected: usize, item_count: usize, list_height: usize) -> usize {
    if item_count <= list_height || selected < list_height {
        0
    } else {
        selected
            .saturating_add(1)
            .saturating_sub(list_height)
            .min(item_count.saturating_sub(list_height))
    }
}

/// 判断稳定键名是否表示可追加到搜索框的单个字符。
fn is_printable_key(code: &str, modifiers: &[String]) -> bool {
    code.chars().count() == 1
        && modifiers
            .iter()
            .all(|modifier| modifier == "shift" || modifier.is_empty())
        && !code.chars().all(char::is_control)
}

/// 渲染包含标题、消息数、更新时间和占用状态的会话行。
fn render_session_line(item: &SessionSummary, selected: bool, content_width: usize) -> UiLine {
    let title = if item.title.trim().is_empty() {
        item.id.as_str()
    } else {
        item.title.as_str()
    };
    let active = if item.active { " · 使用中" } else { "" };
    let updated = if item.updated_label.trim().is_empty() {
        item.updated_at_ms.to_string()
    } else {
        item.updated_label.clone()
    };
    let text = format!(
        "{}{} · {} 条消息 · {}{}",
        if selected { "> " } else { "  " },
        title,
        item.message_count,
        updated,
        active
    );
    line(vec![styled(
        &clip(&text, content_width),
        if item.active {
            UiColor::Gray
        } else if selected {
            UiColor::Black
        } else {
            UiColor::White
        },
        selected,
        selected,
    )])
}

/// 按 Unicode 字符边界裁剪一行，并在空间足够时添加省略号。
fn clip(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.into();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut clipped = text.chars().take(width - 1).collect::<String>();
    clipped.push('…');
    clipped
}

/// 创建一行声明式终端内容。
fn line(spans: Vec<UiSpan>) -> UiLine {
    UiLine { spans }
}

/// 创建没有额外样式的文本片段。
fn plain(text: &str) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle::default(),
    }
}

/// 创建带颜色、粗体和可选反色背景的文本片段。
fn styled(text: &str, foreground: UiColor, bold: bool, selected: bool) -> UiSpan {
    UiSpan {
        text: text.into(),
        style: UiStyle {
            foreground: Some(foreground),
            background: selected.then_some(UiColor::Cyan),
            bold,
            reversed: false,
            ..UiStyle::default()
        },
    }
}

export_plugin!(CommandPlugin);

#[cfg(test)]
mod tests {
    use super::*;
    use command_protocol::{
        CommandHandlerRef, PrepareExecuteResponse, RegisterCommandRequest, SessionListStatus,
        SurfaceEffect, CALLBACK_SERVICE,
    };

    /// 构造一个可执行的第三方命令。
    fn third_party_spec(name: &str) -> CommandSpec {
        let mut spec =
            CommandSpec::new(name, "测试命令", "用于验证注册和执行计划。").with_argument(
                ArgumentSpec::required("count", "执行次数", ArgumentKind::Integer),
            );
        spec.handler = Some(CommandHandlerRef {
            service: "command.callback".into(),
            handler_id: format!("{name}-handler"),
        });
        spec
    }

    /// 验证默认快照包含全部官方命令和 `/quit` 别名。
    #[test]
    fn exposes_builtin_commands() {
        let registry = CommandRegistry::with_builtins();
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.generation, 1);
        assert_eq!(
            snapshot
                .commands
                .iter()
                .map(|spec| spec.name.as_str())
                .collect::<Vec<_>>(),
            ["clear", "compact", "exit", "help", "new", "resume", "sessions"]
        );
        assert_eq!(registry.resolve_name("quit"), Some("exit"));
    }

    /// 验证命令名称不能被其他 owner 覆盖或注销。
    #[test]
    fn enforces_command_ownership() {
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register("owner-a".into(), third_party_spec("deploy"))
            .expect("首次注册应成功");
        let error = registry
            .register("owner-b".into(), third_party_spec("deploy"))
            .expect_err("其他 owner 不得覆盖命令");
        assert!(error.to_string().contains("其他插件"));
        let error = registry
            .unregister("owner-b", "deploy")
            .expect_err("其他 owner 不得注销命令");
        assert!(error.to_string().contains("其他插件"));
        assert!(registry
            .unregister("owner-a", "deploy")
            .expect("owner 应能注销自己的命令"));
    }

    /// 验证 Host 服务目录消失后会清理对应 owner 的幽灵命令。
    #[test]
    fn prunes_commands_whose_callback_service_disappeared() {
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register("inspect-plugin".into(), third_party_spec("inspect"))
            .expect("第三方命令应注册成功");
        let generation = registry.generation;
        let available = BTreeMap::from([(
            "inspect-plugin".into(),
            BTreeSet::from(["command.callback".into()]),
        )]);
        assert_eq!(registry.prune_unavailable_handlers(&available), 0);

        assert_eq!(registry.prune_unavailable_handlers(&BTreeMap::new()), 1);
        assert_eq!(registry.generation, generation + 1);
        assert!(!registry.commands.contains_key("inspect"));
        assert_eq!(registry.resolve_name("inspect"), None);
    }

    /// 验证第三方命令只生成回调计划，不在 Provider 内同步调用 owner。
    #[test]
    fn prepares_callback_plan_with_typed_arguments() {
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register("deploy-plugin".into(), third_party_spec("deploy"))
            .expect("应注册命令");
        let Prepared::Callback {
            owner_plugin_id,
            handler,
            invocation,
        } = registry.prepare("/deploy 3", true)
        else {
            panic!("应生成回调计划");
        };
        assert_eq!(owner_plugin_id, "deploy-plugin");
        assert_eq!(handler.handler_id, "deploy-handler");
        assert_eq!(invocation.arguments["count"], ["3"]);
    }

    /// 验证 `/compact` 生成由原生 TUI 立即执行的受控会话动作。
    #[test]
    fn prepares_compact_surface_action() {
        let registry = CommandRegistry::with_builtins();
        let Prepared::Builtin {
            command,
            invocation,
        } = registry.prepare("/compact", true)
        else {
            panic!("应生成内置命令计划");
        };
        let mut plugin = CommandPlugin::default();
        let response = plugin.execute_builtin(command, invocation);
        assert_eq!(
            response,
            PrepareExecuteResponse::SurfaceAction {
                action: SurfaceAction::CompactSession
            }
        );
    }

    /// 验证 Provider 识别当前参数，并在本地过滤 Choice 与 Static 候选。
    #[test]
    fn prepares_local_argument_candidates() {
        let mut spec = CommandSpec::new("deploy", "部署", "部署到指定环境和区域")
            .with_argument(ArgumentSpec::required(
                "environment",
                "目标环境",
                ArgumentKind::Choice {
                    values: vec!["production".into(), "preview".into(), "staging".into()],
                },
            ))
            .with_argument(
                ArgumentSpec::required("region", "目标区域", ArgumentKind::String).with_completion(
                    CompletionSource::Static {
                        items: vec![
                            CompletionItem {
                                label: "eu-west".into(),
                                insert_text: "eu-west".into(),
                                description: Some("欧洲".into()),
                            },
                            CompletionItem {
                                label: "us-east".into(),
                                insert_text: "us-east".into(),
                                description: Some("美国".into()),
                            },
                        ],
                    },
                ),
            );
        spec.handler = Some(CommandHandlerRef {
            service: CALLBACK_SERVICE.into(),
            handler_id: "deploy-handler".into(),
        });
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register("deploy-plugin".into(), spec)
            .expect("命令应注册成功");

        let choice = registry.prepare_completion(PrepareCompletionRequest::new("/deploy pr"));
        let PrepareCompletionResponse::Candidates { context, items } = choice else {
            panic!("Choice 参数应在 Provider 本地返回候选");
        };
        assert_eq!(context.argument, "environment");
        assert_eq!(context.prefix, "pr");
        assert_eq!(context.replacement_start, 8);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].insert_text, "production");

        let static_items =
            registry.prepare_completion(PrepareCompletionRequest::new("/deploy production eu"));
        let PrepareCompletionResponse::Candidates { context, items } = static_items else {
            panic!("Static 参数应在 Provider 本地返回候选");
        };
        assert_eq!(context.argument, "region");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].insert_text, "eu-west");
    }

    /// 验证带引号的当前 token 会被整体替换，特殊候选仍只解析成一个参数。
    #[test]
    fn encodes_completion_and_replaces_quoted_token() {
        let value = r#"space "quoted" \ path's"#;
        let mut spec = CommandSpec::new("open", "打开", "打开指定目标").with_argument(
            ArgumentSpec::required("target", "目标", ArgumentKind::String).with_completion(
                CompletionSource::Static {
                    items: vec![CompletionItem {
                        label: "space target".into(),
                        insert_text: value.into(),
                        description: None,
                    }],
                },
            ),
        );
        spec.handler = Some(CommandHandlerRef {
            service: CALLBACK_SERVICE.into(),
            handler_id: "open-handler".into(),
        });
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register("open-plugin".into(), spec)
            .expect("命令应注册成功");

        let input = r#"/open "space""#;
        let response = registry.prepare_completion(PrepareCompletionRequest::new(input));
        let PrepareCompletionResponse::Candidates { context, items } = response else {
            panic!("静态参数应返回本地候选");
        };
        assert_eq!(context.replacement_start, 6);
        assert_eq!(context.replacement_end, input.len() as u32);
        assert_eq!(items.len(), 1);

        let mut completed = input.to_owned();
        completed.replace_range(
            context.replacement_start as usize..context.replacement_end as usize,
            &items[0].insert_text,
        );
        let parsed = ParsedCommandLine::parse(&completed).expect("补全结果应可执行");
        assert_eq!(parsed.arguments, [value]);
    }

    /// 验证动态补全计划只使用注册时由 Host 注入的 owner 和回调服务。
    #[test]
    fn prepares_trusted_dynamic_completion_callback() {
        let mut spec = CommandSpec::new("deploy", "部署", "部署指定目标").with_argument(
            ArgumentSpec::required("target", "部署目标", ArgumentKind::String)
                .with_completion(CompletionSource::Callback),
        );
        spec.handler = Some(CommandHandlerRef {
            service: "deploy.complete".into(),
            handler_id: "trusted-handler".into(),
        });
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register("trusted-owner".into(), spec)
            .expect("命令应注册成功");
        let input = "/deploy production";
        let response = registry.prepare_completion(PrepareCompletionRequest {
            input: input.into(),
            cursor: Some(11),
            limit: 7,
        });
        let PrepareCompletionResponse::Callback {
            context,
            owner_plugin_id,
            service,
            request,
        } = response
        else {
            panic!("Callback 参数应返回可信回调计划");
        };
        assert_eq!(owner_plugin_id, "trusted-owner");
        assert_eq!(service, "deploy.complete");
        assert_eq!(context.prefix, "pro");
        assert_eq!(context.replacement_start, 8);
        assert_eq!(context.replacement_end, input.len() as u32);
        let CommandCallbackRequest::Complete {
            handler_id,
            request,
        } = request
        else {
            panic!("计划必须使用 Complete 回调");
        };
        assert_eq!(handler_id, "trusted-handler");
        assert_eq!(request.argument, "target");
        assert_eq!(request.limit, 7);
    }

    /// 验证 Session 参数转换为宿主会话数据源请求，不暴露插件 owner。
    #[test]
    fn prepares_session_surface_completion_request() {
        let mut spec = CommandSpec::new("open", "打开", "打开指定会话").with_argument(
            ArgumentSpec::required("session", "会话标识", ArgumentKind::Session),
        );
        spec.handler = Some(CommandHandlerRef {
            service: CALLBACK_SERVICE.into(),
            handler_id: "open-handler".into(),
        });
        let mut registry = CommandRegistry::with_builtins();
        registry
            .register("session-plugin".into(), spec)
            .expect("命令应注册成功");
        let response = registry.prepare_completion(PrepareCompletionRequest::new("/open abc"));
        let PrepareCompletionResponse::Surface { context, request } = response else {
            panic!("Session 参数应返回宿主数据源计划");
        };
        assert_eq!(context.argument, "session");
        assert_eq!(request.source, SESSION_COMPLETION_SOURCE);
        assert_eq!(request.request.prefix, "abc");
    }

    /// 验证仅空闲命令在准备阶段执行第二次状态校验。
    #[test]
    fn rejects_idle_only_command_while_agent_runs() {
        let registry = CommandRegistry::with_builtins();
        let Prepared::Error { message, .. } = registry.prepare("/resume", false) else {
            panic!("运行期间应拒绝恢复会话");
        };
        assert!(message.contains("Agent 空闲"));
        let Prepared::Error { message, .. } = registry.prepare("/exit", false) else {
            panic!("运行期间应拒绝退出，避免中止持久化任务");
        };
        assert!(message.contains("Agent 空闲"));
    }

    /// 验证 `/resume` 打开插件 Dialog 并只查询轻量会话摘要。
    #[test]
    fn resume_opens_surface_and_queries_sessions() {
        let mut plugin = CommandPlugin::default();
        let response = plugin.execute_builtin(
            BuiltinCommand::Resume,
            CommandInvocation {
                command: "resume".into(),
                input: "/resume".into(),
                arguments: BTreeMap::new(),
            },
        );
        assert_eq!(
            response,
            PrepareExecuteResponse::SurfaceOpened {
                view_id: SESSION_DIALOG_VIEW.into()
            }
        );
        assert!(plugin.surface.visible);
        assert_eq!(
            plugin.surface.effects.front(),
            Some(&SurfaceEffect::QuerySessions {
                request_id: 1,
                query: String::new(),
                cursor: None,
                limit: SESSION_PAGE_LIMIT,
            })
        );
    }

    /// 验证过期查询响应不会覆盖搜索后发起的新请求。
    #[test]
    fn ignores_stale_surface_update() {
        let mut surface = SessionSurface::default();
        surface.open(SessionSurfaceMode::Resume);
        surface.handle_key("a", &[]);
        assert_eq!(surface.request_id, 2);
        assert!(!surface.update(SurfaceUpdateRequest {
            request_id: 1,
            status: SessionListStatus::Empty,
        }));
        assert!(matches!(surface.status, SessionListStatus::Loading));
    }

    /// 验证选择会话只生成带修订号的恢复 effect，并立即关闭界面。
    #[test]
    fn selecting_session_emits_resume_effect() {
        let mut surface = SessionSurface::default();
        surface.open(SessionSurfaceMode::Resume);
        assert!(surface.update(SurfaceUpdateRequest {
            request_id: 1,
            status: SessionListStatus::Ready {
                items: vec![SessionSummary {
                    id: "session-1".into(),
                    title: "设计讨论".into(),
                    preview: String::new(),
                    message_count: 8,
                    updated_at_ms: 42,
                    updated_label: "刚刚".into(),
                    revision: 7,
                    active: false,
                }],
                next_cursor: None,
            },
        }));
        surface.effects.clear();
        surface.handle_key("enter", &[]);
        assert!(!surface.visible);
        assert_eq!(
            surface.effects.pop_front(),
            Some(SurfaceEffect::ResumeSession {
                session_id: "session-1".into(),
                revision: 7,
            })
        );
    }

    /// 验证选中项越过可见高度后滚动窗口，鼠标点击使用窗口绝对起点。
    #[test]
    fn session_surface_scrolls_and_maps_mouse_to_visible_window() {
        let mut surface = SessionSurface::default();
        surface.open(SessionSurfaceMode::Resume);
        let items = (0..12)
            .map(|index| SessionSummary {
                id: format!("session-{index}"),
                title: format!("会话 {index}"),
                preview: String::new(),
                message_count: index,
                updated_at_ms: index,
                updated_label: "刚刚".into(),
                revision: index,
                active: false,
            })
            .collect();
        assert!(surface.update(SurfaceUpdateRequest {
            request_id: 1,
            status: SessionListStatus::Ready {
                items,
                next_cursor: None,
            },
        }));
        surface.selected = 8;
        let lines = surface.render(60, 10);
        assert_eq!(surface.rendered_start, 5);
        assert_eq!(surface.rendered_len, 4);
        assert!(lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|span| span.text.starts_with("> 会话 8"))
        }));

        surface.handle_mouse("down_left", 3);
        assert_eq!(surface.selected, 5);
    }

    /// 验证服务请求使用可信 caller ID 记录第三方命令 owner。
    #[test]
    fn register_service_uses_caller_as_owner() {
        let mut plugin = CommandPlugin::default();
        let spec = third_party_spec("inspect");
        let response = plugin
            .register(ServiceCall {
                caller_id: "inspect-plugin".into(),
                name: REGISTER_SERVICE.into(),
                payload: serde_json::to_value(RegisterCommandRequest { spec })
                    .expect("请求应可序列化"),
            })
            .expect("注册服务应成功");
        assert_eq!(response["name"], "inspect");
        assert_eq!(
            plugin.registry.commands["inspect"].owner_plugin_id,
            "inspect-plugin"
        );
    }

    /// 验证缺失或空白权限元数据时，surface 仍只允许官方 TUI 调用方。
    #[test]
    fn missing_surface_authority_defaults_to_official_tui() {
        let mut context = ActivationContext {
            plugin_id: "command".into(),
            metadata: Default::default(),
        };
        assert_eq!(
            configured_surface_authority(&context),
            DEFAULT_SURFACE_AUTHORITY
        );
        context
            .metadata
            .insert("surface_authority".into(), "  ".into());
        assert_eq!(
            configured_surface_authority(&context),
            DEFAULT_SURFACE_AUTHORITY
        );

        let plugin = CommandPlugin::default();
        plugin
            .ensure_surface_authority(DEFAULT_SURFACE_AUTHORITY)
            .expect("官方 TUI 应能访问 surface");
        let error = plugin
            .ensure_surface_authority("untrusted-plugin")
            .expect_err("其他调用方必须被拒绝");
        assert!(error.to_string().contains("无权访问"));
    }
}

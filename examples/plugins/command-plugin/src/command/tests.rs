use super::*;
use command_protocol::{
    CommandHandlerRef, PrepareExecuteResponse, RegisterCommandRequest, SessionListStatus,
    SurfaceEffect, CALLBACK_SERVICE,
};

/// 构造一个可执行的第三方命令。
fn third_party_spec(name: &str) -> CommandSpec {
    let mut spec = CommandSpec::new(name, "测试命令", "用于验证注册和执行计划。").with_argument(
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
            action: SurfaceAction::ReloadSessionContext
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
            payload: serde_json::to_value(RegisterCommandRequest { spec }).expect("请求应可序列化"),
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

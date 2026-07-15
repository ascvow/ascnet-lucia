use super::*;
use agent_plugin::{
    ModelCompletionRequest, ModelCompletionResponse, ProcessSpec, PromptContribution,
    ServiceDescriptor, ToolSpec, UI_HOST_ACTION_EVENT,
};
use command_protocol::{CommandHandlerRef, SessionListStatus, SurfaceEffect, CALLBACK_SERVICE};
use std::cell::RefCell;

/// 记录事件与服务调用的测试宿主。
#[derive(Default)]
struct RecordingHost {
    events: RefCell<Vec<ExtensionEvent>>,
    /// 服务目录：owner 插件 ID 到服务名列表。
    services: Vec<(String, String)>,
    /// 固定的服务调用响应。
    service_response: Option<Value>,
    /// 已发出的服务调用记录。
    calls: RefCell<Vec<(String, String, Value)>>,
}

impl RecordingHost {
    /// 取出已记录的宿主动作请求。
    fn host_actions(&self) -> Vec<UiHostAction> {
        self.events
            .borrow()
            .iter()
            .filter(|event| event.name == UI_HOST_ACTION_EVENT)
            .map(|event| {
                serde_json::from_value::<UiHostActionRequest>(event.data.clone())
                    .expect("宿主动作事件应可解析")
                    .action
            })
            .collect()
    }

    /// 取出主事件列表说明文本。
    fn notices(&self) -> Vec<String> {
        self.events
            .borrow()
            .iter()
            .filter_map(|event| event.presentation.as_ref().map(|item| item.text.clone()))
            .collect()
    }
}

impl PluginHostApi for RecordingHost {
    fn upsert_tool(&self, _local_name: &str, spec: &ToolSpec) -> Result<String> {
        Ok(spec.name.clone())
    }

    fn remove_tool(&self, _public_name: &str) -> Result<()> {
        Ok(())
    }

    fn upsert_prompt(&self, prompt: &PromptContribution) -> Result<String> {
        Ok(prompt.id.clone())
    }

    fn remove_prompt(&self, _id: &str) -> Result<()> {
        Ok(())
    }

    fn emit_event(&self, event: &ExtensionEvent) -> Result<()> {
        self.events.borrow_mut().push(event.clone());
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

    fn upsert_service(&self, _service: &ServiceSpec) -> Result<()> {
        Ok(())
    }

    fn remove_service(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn list_services(&self, plugin_id: Option<&str>) -> Result<Vec<ServiceDescriptor>> {
        Ok(self
            .services
            .iter()
            .filter(|(owner, _)| plugin_id.is_none_or(|target| owner == target))
            .map(|(owner, name)| ServiceDescriptor {
                plugin_id: owner.clone(),
                name: name.clone(),
                version: "1.0.0".into(),
                description: None,
            })
            .collect())
    }

    fn call_service(&self, plugin_id: &str, name: &str, payload: &Value) -> Result<Value> {
        self.calls
            .borrow_mut()
            .push((plugin_id.into(), name.into(), payload.clone()));
        self.service_response
            .clone()
            .ok_or_else(|| anyhow!("测试宿主未配置服务响应"))
    }

    fn read_file(&self, _path: &str) -> Result<String> {
        Err(anyhow!("测试宿主不提供文件读取"))
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<agent_plugin::FileEntry>> {
        Err(anyhow!("测试宿主不提供目录扫描"))
    }

    fn spawn_process(&self, _spec: &ProcessSpec) -> Result<u64> {
        Err(anyhow!("测试宿主不提供子进程"))
    }

    fn write_process(&self, _handle: u64, _data: &str) -> Result<()> {
        Err(anyhow!("测试宿主不提供子进程"))
    }

    fn read_process_line(&self, _handle: u64, _timeout_ms: u64) -> Result<Option<String>> {
        Err(anyhow!("测试宿主不提供子进程"))
    }

    fn kill_process(&self, _handle: u64) -> Result<()> {
        Err(anyhow!("测试宿主不提供子进程"))
    }

    fn complete_model(&self, _request: &ModelCompletionRequest) -> Result<ModelCompletionResponse> {
        Err(anyhow!("测试宿主不提供模型完成"))
    }
}

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

/// 把主输入快照送入弹层。
fn sync_popup(plugin: &mut CommandPlugin, host: &RecordingHost, text: &str) {
    plugin.on_ui_input_with_host(
        host,
        UiInput {
            plugin_id: "command".into(),
            view_id: POPUP_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::MainInput {
                text: text.into(),
                cursor: u32::try_from(text.len()).expect("测试输入应可转换"),
            },
        },
    );
}

/// 向弹层发送一个无修饰手势键。
fn press_popup(plugin: &mut CommandPlugin, host: &RecordingHost, code: &str) {
    plugin.on_ui_input_with_host(
        host,
        UiInput {
            plugin_id: "command".into(),
            view_id: POPUP_VIEW.into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: code.into(),
                modifiers: Vec::new(),
            },
        },
    );
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

/// 验证第三方命令生成回调计划并绑定类型化参数。
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

/// 验证 `/compact` 通过通用宿主动作请求上下文重载并清空输入。
#[test]
fn compact_emits_reload_context_action() {
    let host = RecordingHost::default();
    let mut plugin = CommandPlugin::default();
    sync_popup(&mut plugin, &host, "/compact");
    press_popup(&mut plugin, &host, "enter");

    let actions = host.host_actions();
    assert!(actions
        .iter()
        .any(|action| matches!(action, UiHostAction::SetInput { text, .. } if text.is_empty())));
    assert!(actions.iter().any(|action| matches!(
        action,
        UiHostAction::ReloadContext { label: Some(label) } if label == "/compact"
    )));
}

/// 验证 `/exit` 与 `/new` 映射到对应的通用宿主动作。
#[test]
fn builtin_commands_map_to_host_actions() {
    let host = RecordingHost::default();
    let mut plugin = CommandPlugin::default();
    sync_popup(&mut plugin, &host, "/new");
    press_popup(&mut plugin, &host, "enter");
    sync_popup(&mut plugin, &host, "/exit");
    press_popup(&mut plugin, &host, "enter");

    let actions = host.host_actions();
    assert!(actions
        .iter()
        .any(|action| matches!(action, UiHostAction::NewSession)));
    assert!(actions
        .iter()
        .any(|action| matches!(action, UiHostAction::Exit)));
}

/// 验证 Tab 在命令名阶段按选中项补全输入。
#[test]
fn tab_completes_command_name() {
    let host = RecordingHost::default();
    let mut plugin = CommandPlugin::default();
    sync_popup(&mut plugin, &host, "/res");
    press_popup(&mut plugin, &host, "tab");

    let actions = host.host_actions();
    assert!(actions.iter().any(|action| matches!(
        action,
        UiHostAction::SetInput { text, .. } if text == "/resume "
    )));
    // 弹层内部快照跟随补全结果，无需等待宿主回发。
    assert_eq!(plugin.popup.input, "/resume ");
}

/// 验证运行期间 IdleOnly 命令保留输入并提示等待。
#[test]
fn idle_only_command_is_deferred_while_running() {
    let host = RecordingHost::default();
    let mut plugin = CommandPlugin::default();
    plugin.on_event(AgentEvent {
        id: "event-1".into(),
        run_id: "run-1".into(),
        timestamp_ms: 0,
        kind: AgentEventKind::RunStarted,
        step: 0,
        payload: Value::Null,
    });
    sync_popup(&mut plugin, &host, "/resume");
    press_popup(&mut plugin, &host, "enter");

    assert!(host.host_actions().is_empty(), "忙时不应产生宿主动作");
    assert!(host.notices().iter().any(|notice| notice.contains("空闲")));
    assert_eq!(plugin.popup.input, "/resume");
}

/// 验证 `/resume` 打开对话框并经宿主动作查询会话摘要。
#[test]
fn resume_opens_surface_and_queries_sessions() {
    let host = RecordingHost::default();
    let mut plugin = CommandPlugin::default();
    sync_popup(&mut plugin, &host, "/resume");
    press_popup(&mut plugin, &host, "enter");

    assert!(plugin.surface.visible);
    let actions = host.host_actions();
    assert!(actions.iter().any(|action| matches!(
        action,
        UiHostAction::QuerySessions {
            query_id: 1,
            limit,
            reply_service,
            ..
        } if *limit == SESSION_PAGE_LIMIT && reply_service == SURFACE_UPDATE_SERVICE
    )));
}

/// 验证会话查询应答按查询 ID 路由：对话框接受、过期应答被拒绝。
#[test]
fn sessions_reply_routes_to_dialog_by_query_id() {
    let host = RecordingHost::default();
    let mut plugin = CommandPlugin::default();
    sync_popup(&mut plugin, &host, "/resume");
    press_popup(&mut plugin, &host, "enter");

    let stale = plugin
        .handle_service(
            &host,
            ServiceCall {
                caller_id: DEFAULT_SURFACE_AUTHORITY.into(),
                name: SURFACE_UPDATE_SERVICE.into(),
                payload: serde_json::to_value(UiSessionsReply {
                    query_id: 99,
                    status: UiSessionListStatus::Empty,
                })
                .expect("应答应可序列化"),
            },
        )
        .expect("过期应答不应报错");
    assert_eq!(stale["accepted"], false);

    let reply = plugin
        .handle_service(
            &host,
            ServiceCall {
                caller_id: DEFAULT_SURFACE_AUTHORITY.into(),
                name: SURFACE_UPDATE_SERVICE.into(),
                payload: serde_json::to_value(UiSessionsReply {
                    query_id: 1,
                    status: UiSessionListStatus::Ready {
                        items: vec![UiSessionSummary {
                            id: "session-1".into(),
                            title: "设计讨论".into(),
                            message_count: 8,
                            updated_at_ms: 42,
                            updated_label: "刚刚".into(),
                            revision: 7,
                            active: false,
                        }],
                        next_cursor: None,
                    },
                })
                .expect("应答应可序列化"),
            },
        )
        .expect("匹配应答应被接受");
    assert_eq!(reply["accepted"], true);
    assert_eq!(plugin.surface.items().len(), 1);
}

/// 验证会话参数补全经宿主数据源应答后立即应用唯一候选。
#[test]
fn session_argument_completion_applies_reply() {
    let mut spec = CommandSpec::new("open", "打开", "打开指定会话").with_argument(
        ArgumentSpec::required("session", "会话标识", ArgumentKind::Session),
    );
    spec.handler = Some(CommandHandlerRef {
        service: CALLBACK_SERVICE.into(),
        handler_id: "open-handler".into(),
    });
    let host = RecordingHost {
        services: vec![("session-plugin".into(), CALLBACK_SERVICE.into())],
        ..RecordingHost::default()
    };
    let mut plugin = CommandPlugin::default();
    plugin
        .registry
        .register("session-plugin".into(), spec)
        .expect("命令应注册成功");

    sync_popup(&mut plugin, &host, "/open se");
    press_popup(&mut plugin, &host, "tab");
    let query_id = host
        .host_actions()
        .iter()
        .find_map(|action| match action {
            UiHostAction::QuerySessions { query_id, .. } => Some(*query_id),
            _ => None,
        })
        .expect("Session 参数应发起会话查询");

    plugin
        .handle_service(
            &host,
            ServiceCall {
                caller_id: DEFAULT_SURFACE_AUTHORITY.into(),
                name: SURFACE_UPDATE_SERVICE.into(),
                payload: serde_json::to_value(UiSessionsReply {
                    query_id,
                    status: UiSessionListStatus::Ready {
                        items: vec![UiSessionSummary {
                            id: "session-9".into(),
                            title: "发布计划".into(),
                            message_count: 3,
                            updated_at_ms: 1,
                            updated_label: "刚刚".into(),
                            revision: 2,
                            active: false,
                        }],
                        next_cursor: None,
                    },
                })
                .expect("应答应可序列化"),
            },
        )
        .expect("候选应答应被接受");

    assert!(host.host_actions().iter().any(|action| matches!(
        action,
        UiHostAction::SetInput { text, .. } if text == "/open session-9"
    )));
}

/// 验证第三方命令执行直接调用 owner 回调并展示结果。
#[test]
fn executes_third_party_callback_directly() {
    let host = RecordingHost {
        services: vec![("deploy-plugin".into(), "command.callback".into())],
        service_response: Some(
            serde_json::to_value(CommandCallbackResponse::Executed {
                result: Value::String("部署完成".into()),
            })
            .expect("响应应可序列化"),
        ),
        ..RecordingHost::default()
    };
    let mut plugin = CommandPlugin::default();
    plugin
        .registry
        .register("deploy-plugin".into(), third_party_spec("deploy"))
        .expect("应注册命令");

    sync_popup(&mut plugin, &host, "/deploy 3");
    press_popup(&mut plugin, &host, "enter");

    let calls = host.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "deploy-plugin");
    assert_eq!(calls[0].1, "command.callback");
    drop(calls);
    assert!(host
        .notices()
        .iter()
        .any(|notice| notice.contains("部署完成")));
}

/// 验证弹层渲染包含命令用法、摘要与说明。
#[test]
fn popup_renders_descriptive_matches() {
    let host = RecordingHost::default();
    let mut plugin = CommandPlugin::default();
    sync_popup(&mut plugin, &host, "/res");
    assert!(plugin.popup.visible(&plugin.registry));
    let lines = plugin.popup.render(&plugin.registry, true, 80);
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert!(text.contains("/resume"), "{text}");
    assert!(text.contains("恢复历史会话"), "{text}");
}

/// 验证 Esc 隐藏弹层且输入变化后恢复展示。
#[test]
fn escape_dismisses_popup_until_input_changes() {
    let host = RecordingHost::default();
    let mut plugin = CommandPlugin::default();
    sync_popup(&mut plugin, &host, "/res");
    press_popup(&mut plugin, &host, "escape");
    assert!(!plugin.popup.visible(&plugin.registry));
    sync_popup(&mut plugin, &host, "/resu");
    assert!(plugin.popup.visible(&plugin.registry));
}

/// 验证过期查询响应不会覆盖搜索后发起的新请求。
#[test]
fn ignores_stale_surface_update() {
    let mut seq = 0;
    let mut surface = SessionSurface::default();
    surface.open(&mut seq, SessionSurfaceMode::Resume);
    surface.handle_key(&mut seq, "a", &[]);
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
    let mut seq = 0;
    let mut surface = SessionSurface::default();
    surface.open(&mut seq, SessionSurfaceMode::Resume);
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
    surface.handle_key(&mut seq, "enter", &[]);
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
    let mut seq = 0;
    let mut surface = SessionSurface::default();
    surface.open(&mut seq, SessionSurfaceMode::Resume);
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

    surface.handle_mouse(&mut seq, "down_left", 3);
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

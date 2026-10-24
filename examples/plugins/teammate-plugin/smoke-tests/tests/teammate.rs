//! Teammate 插件的真实 WASM 端到端测试。

use agent_core::{
    Agent, AgentEvent, AgentEventKind, AgentExtension, AgentOptions, AgentRecordEvent, ChatModel,
    EventBus, ModelGateway, ModelRequest, ModelResponse, ProviderAdapter, RunFinishEvent,
};
use agent_plugin_host::{
    ui::{UiColor, UiInput, UiInputEvent, UiPlacement, UiRenderRequest, UI_NAVIGATION_EVENT},
    wasm::WasmPluginHost,
    LivePluginHost, PluginHost, PluginHostServices, PluginServiceCall,
};
use agent_runtime::{
    AgentDeriveConfig, AgentPermissions, AgentProfileId, AgentRuntime, AgentTemplate, RuntimeLimits,
};
use agent_tool::ToolCall;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{collections::HashMap, path::Path, sync::Arc};

/// 测试使用的可信 controller 运行 ID；所有工具调用与运行结束事件共享它。
const RUN: &str = "controller-run";

/// 返回固定文本的离线模型，确保端到端测试不依赖网络。
struct FixedModel;

#[async_trait]
impl ChatModel for FixedModel {
    /// 完成一次确定性的模型调用。
    async fn complete(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse::text("teammate 已完成"))
    }

    /// 模拟大量 token 增量，验证首次进入视图无需跨多个 UI tick 才追上历史。
    async fn stream(&self, _request: ModelRequest) -> agent_core::model::ModelEventStream {
        use agent_core::model::{ModelEventStream, ModelStreamEvent};
        let (sender, stream) = ModelEventStream::channel();
        for _ in 0..1500 {
            sender.send(ModelStreamEvent::TextDelta {
                index: 0,
                delta: "x".into(),
            });
        }
        sender.send(ModelStreamEvent::TextDelta {
            index: 0,
            delta: "teammate 已完成".into(),
        });
        sender.done(ModelResponse::text(format!(
            "{}teammate 已完成",
            "x".repeat(1500)
        )));
        stream
    }
}

impl ProviderAdapter for FixedModel {
    /// 返回测试路由使用的稳定 provider 名称。
    fn name(&self) -> &'static str {
        "fixed"
    }
}

/// 调用真实 component 工具并返回成功内容。
///
/// `run` 是 Core 在工具准入前绑定的可信运行 ID，插件据此判断成员属于哪一轮 controller。
async fn call_tool(host: &WasmPluginHost, run: &str, name: &str, args: Value) -> Value {
    let mut call = ToolCall::new("smoke-call", name, args);
    call.run_id = Some(run.to_string());
    let result = AgentExtension::call_tool(host, call)
        .await
        .expect("插件工具路由不应失败")
        .expect("Teammate 插件应处理自己的工具");
    assert!(!result.is_error, "插件工具返回错误：{}", result.content);
    result.content
}

/// 轮询成员当前执行句柄，直到返回终态结果。
async fn wait_result(host: &WasmPluginHost, run: &str, member_id: &str) -> Value {
    for _ in 0..100 {
        let content = call_tool(
            host,
            run,
            "teammate_result",
            json!({"member_id": member_id}),
        )
        .await;
        if content["completed"] == true {
            return content;
        }
        tokio::task::yield_now().await;
    }
    panic!("teammate 未在轮询预算内结束");
}

/// 调用真实 component 工具并断言返回工具错误，返回错误文本。
async fn call_tool_error(host: &WasmPluginHost, run: &str, name: &str, args: Value) -> String {
    let mut call = ToolCall::new("smoke-call", name, args);
    call.run_id = Some(run.to_string());
    let result = AgentExtension::call_tool(host, call)
        .await
        .expect("插件工具路由不应失败")
        .expect("Teammate 插件应处理自己的工具");
    assert!(
        result.is_error,
        "预期工具错误，实际成功：{}",
        result.content
    );
    result.content.to_string()
}

/// 验证派生、可信发送、邮箱、dispatch 自动确认和续跑句柄更新链路。
#[tokio::test]
async fn component_runs_mailbox_dispatch_flow() {
    let gateway = ModelGateway::new();
    gateway
        .register("fixed", Arc::new(FixedModel))
        .expect("固定模型应注册成功");
    let agent = Agent::new(
        gateway,
        AgentOptions::default().with_model_route("fixed", "test-model"),
    );

    // 只留 controller 与一个活跃成员的累计名额：投胎后若不回收，第二次派活就会失败。
    let runtime = AgentRuntime::new(RuntimeLimits {
        max_agents_per_principal: 3,
        ..RuntimeLimits::default()
    })
    .expect("Runtime 应创建成功");
    let controller_profile = AgentProfileId::new("controller").expect("profile 应合法");
    runtime
        .register_profile(
            controller_profile.clone(),
            AgentTemplate::from_agent(&agent),
            AgentPermissions::default(),
        )
        .await
        .expect("controller profile 应注册成功");
    let services = PluginHostServices::new()
        .with_agent_runtime(
            Arc::new(runtime),
            controller_profile,
            HashMap::from([("worker".to_string(), AgentDeriveConfig::default())]),
        )
        .expect("Host 应接受 Agent Runtime 服务");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../plugin.toml");
    let host = Arc::new(
        WasmPluginHost::load_from_manifest_with_services(manifest, services)
            .await
            .expect("Teammate component 应加载成功"),
    );
    // 运行结束回收走真实事件总线：必须和 Core 一样先打开运行快照，再投递 finish 事件。
    let live = LivePluginHost::new();
    live.publish(host.clone() as Arc<dyn PluginHost>)
        .expect("发布真实 Teammate 宿主");
    live.finish_capability_planning().expect("标记能力规划完成");
    let bus = EventBus::new();
    live.attach_bus(bus.clone()).expect("挂载事件总线");
    bus.dispatch(AgentRecordEvent {
        record: AgentEvent::new(RUN, AgentEventKind::RunStarted, 0, json!({})),
    })
    .await;

    let prompts = AgentExtension::prompt_messages(host.as_ref())
        .await
        .expect("Teammate 协作提示应可读取");
    assert!(prompts.is_empty(), "Teammate 不应注入常驻提示");

    let tools = AgentExtension::list_tools(host.as_ref())
        .await
        .expect("插件工具应可读取");
    assert_eq!(tools.len(), 10);
    let spawn = tools
        .iter()
        .find(|tool| tool.name == "teammate_spawn")
        .expect("应暴露 teammate_spawn");
    assert!(spawn.description.contains("并行收益明显高于协调成本"));
    let declarations = PluginHost::ui_declarations(host.as_ref())
        .await
        .expect("团队 UI 声明应可读取");
    assert_eq!(declarations.len(), 3);
    assert_eq!(declarations[0].placement, UiPlacement::Right);
    assert_eq!(declarations[1].placement, UiPlacement::Subview);
    let dock = PluginHost::render_ui(
        host.as_ref(),
        &UiRenderRequest {
            session_id: None,
            plugin_id: "teammate".into(),
            view_id: "teammate-team-dock".into(),
            instance_id: None,
            width: 30,
            height: 16,
            focused: true,
            frame: 1,
        },
    )
    .await
    .expect("团队摘要渲染不应失败")
    .expect("团队摘要应返回帧");
    assert!(!dock.visible);
    let services = PluginHost::services(host.as_ref())
        .await
        .expect("插件服务目录应可读取");
    assert!(services.iter().any(|service| {
        service.plugin_id == "teammate"
            && service.name == "teammate.mailbox"
            && service.version == "1.0.0"
    }));
    let isolated_list = PluginHost::call_service(
        host.as_ref(),
        &PluginServiceCall {
            caller_id: "orchestrator".into(),
            plugin_id: "teammate".into(),
            name: "teammate.mailbox".into(),
            payload: json!({"operation": "list"}),
        },
    )
    .await
    .expect("版本化 service 调用不应失败")
    .expect("Teammate 插件应处理自己的 service");
    assert!(isolated_list["members"]
        .as_array()
        .expect("service 应返回成员数组")
        .is_empty());

    let spawned = call_tool(
        host.as_ref(),
        RUN,
        "teammate_spawn",
        json!({"role": "reviewer", "input": "检查首轮实现", "captain": true}),
    )
    .await;
    assert_eq!(spawned["member"]["captain"], true);
    let member_id = spawned["member"]["id"]
        .as_str()
        .expect("spawn 应返回稳定成员地址");
    wait_result(host.as_ref(), RUN, member_id).await;

    let dock = PluginHost::render_ui(
        host.as_ref(),
        &UiRenderRequest {
            session_id: None,
            plugin_id: "teammate".into(),
            view_id: "teammate-team-dock".into(),
            instance_id: None,
            width: 30,
            height: 16,
            focused: true,
            frame: 2,
        },
    )
    .await
    .expect("成员创建后团队摘要渲染不应失败")
    .expect("成员创建后团队摘要应返回帧");
    assert!(dock.visible);
    assert!(dock.lines[1].spans.iter().any(|span| {
        span.text.contains("Captain") && span.style.foreground == Some(UiColor::Magenta)
    }));
    PluginHost::on_ui_input(
        host.as_ref(),
        &UiInput {
            plugin_id: "teammate".into(),
            view_id: "teammate-team-dock".into(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: "enter".into(),
                modifiers: Vec::new(),
            },
        },
    )
    .await
    .expect("团队入口按键路由不应失败");
    let navigation_events = AgentExtension::drain_events(host.as_ref())
        .await
        .expect("团队导航事件应可读取");
    assert!(navigation_events.iter().any(|event| {
        event["name"] == UI_NAVIGATION_EVENT
            && event["data"]["action"]["action"] == "push"
            && event["data"]["action"]["view"]["view_id"] == "teammate-team-workspace"
    }));

    PluginHost::on_ui_input(
        host.as_ref(),
        &UiInput {
            plugin_id: "teammate".into(),
            view_id: "teammate-team-workspace".into(),
            instance_id: Some("team".into()),
            event: UiInputEvent::Key {
                code: "enter".into(),
                modifiers: Vec::new(),
            },
        },
    )
    .await
    .expect("选中成员应能打开实时会话");
    let member_navigation = AgentExtension::drain_events(host.as_ref())
        .await
        .expect("成员会话导航事件应可读取");
    assert!(member_navigation.iter().any(|event| {
        event["name"] == UI_NAVIGATION_EVENT
            && event["data"]["action"]["view"]["view_id"] == "teammate-member-session"
            && event["data"]["action"]["view"]["instance_id"] == member_id
    }));
    let render_started = std::time::Instant::now();
    let session = PluginHost::render_ui(
        host.as_ref(),
        &UiRenderRequest {
            session_id: None,
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            width: 80,
            height: 24,
            focused: true,
            frame: 2,
        },
    )
    .await
    .expect("成员实时会话渲染不应失败")
    .expect("成员实时会话应返回可见帧");
    let session_text = session
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.text.as_str())
        .collect::<String>();
    assert!(!session_text.contains("Run completed"), "{session_text}");
    assert!(!session_text.contains("Analyzing · Step"), "{session_text}");
    assert!(session_text.contains("Message Agent..."), "{session_text}");
    assert!(
        session_text.contains("teammate 已完成"),
        "首次渲染必须追上 1500 条增量：{session_text}"
    );
    eprintln!(
        "1500 条历史增量首次渲染耗时：{:?}",
        render_started.elapsed()
    );
    PluginHost::on_ui_input(
        host.as_ref(),
        &UiInput {
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            event: UiInputEvent::Key {
                code: "home".into(),
                modifiers: Vec::new(),
            },
        },
    )
    .await
    .expect("应可回看任务 prompt");
    let first = PluginHost::render_ui(
        host.as_ref(),
        &UiRenderRequest {
            session_id: None,
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            width: 80,
            height: 24,
            focused: true,
            frame: 3,
        },
    )
    .await
    .unwrap()
    .unwrap();
    let first_text: String = first
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect();
    assert!(first_text.contains("检查首轮实现"), "{first_text}");

    // 高视口没有溢出时渲染会把滚动位置收敛回底部，重新按 home 回到最早内容。
    PluginHost::on_ui_input(
        host.as_ref(),
        &UiInput {
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            event: UiInputEvent::Key {
                code: "home".into(),
                modifiers: Vec::new(),
            },
        },
    )
    .await
    .expect("成员会话应可再次回到最早内容");

    // 缩小视口制造确定的溢出，验证真实 Host 会把滚轮事件送到成员会话并改变回看位置。
    let compact = PluginHost::render_ui(
        host.as_ref(),
        &UiRenderRequest {
            session_id: None,
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            width: 80,
            height: 12,
            focused: true,
            frame: 4,
        },
    )
    .await
    .expect("窄视口成员会话渲染不应失败")
    .expect("窄视口成员会话应返回可见帧");
    let compact_text: String = compact
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect();
    assert!(
        compact.lines.iter().any(|line| line
            .spans
            .last()
            .is_some_and(|span| matches!(span.text.as_str(), "│" | "┃"))),
        "溢出成员会话必须显示右侧滚动条\n{compact_text}"
    );

    PluginHost::on_ui_input(
        host.as_ref(),
        &UiInput {
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            event: UiInputEvent::Mouse {
                kind: "scroll_down".into(),
                x: 10,
                y: 10,
            },
        },
    )
    .await
    .expect("成员会话滚轮向下滚动不应失败");
    let scrolled = PluginHost::render_ui(
        host.as_ref(),
        &UiRenderRequest {
            session_id: None,
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            width: 80,
            height: 12,
            focused: true,
            frame: 5,
        },
    )
    .await
    .expect("滚轮滚动后成员会话渲染不应失败")
    .expect("滚轮滚动后成员会话应返回可见帧");
    let scrolled_text: String = scrolled
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect();
    assert_ne!(
        scrolled_text, compact_text,
        "滚轮必须改变成员会话的回看位置"
    );

    PluginHost::on_ui_input(
        host.as_ref(),
        &UiInput {
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            event: UiInputEvent::Mouse {
                kind: "scroll_up".into(),
                x: 10,
                y: 10,
            },
        },
    )
    .await
    .expect("成员会话滚轮向上滚动不应失败");
    let restored = PluginHost::render_ui(
        host.as_ref(),
        &UiRenderRequest {
            session_id: None,
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            width: 80,
            height: 12,
            focused: true,
            frame: 6,
        },
    )
    .await
    .expect("滚轮回滚后成员会话渲染不应失败")
    .expect("滚轮回滚后成员会话应返回可见帧");
    let restored_text: String = restored
        .lines
        .iter()
        .flat_map(|line| &line.spans)
        .map(|span| span.text.as_str())
        .collect();
    assert_eq!(restored_text, compact_text, "滚轮上下滚动应回到原位置");

    for code in ["r", "e", "v", "i", "e", "w"] {
        PluginHost::on_ui_input(
            host.as_ref(),
            &UiInput {
                plugin_id: "teammate".into(),
                view_id: "teammate-member-session".into(),
                instance_id: Some(member_id.into()),
                event: UiInputEvent::Key {
                    code: code.into(),
                    modifiers: Vec::new(),
                },
            },
        )
        .await
        .expect("成员会话文本输入不应失败");
    }
    PluginHost::on_ui_input(
        host.as_ref(),
        &UiInput {
            plugin_id: "teammate".into(),
            view_id: "teammate-member-session".into(),
            instance_id: Some(member_id.into()),
            event: UiInputEvent::Key {
                code: "enter".into(),
                modifiers: Vec::new(),
            },
        },
    )
    .await
    .expect("成员会话消息发送不应失败");
    let interactive_result = wait_result(host.as_ref(), RUN, member_id).await;
    assert_eq!(interactive_result["outcome"]["status"], "succeeded");

    let sent = call_tool(
        host.as_ref(),
        RUN,
        "teammate_send",
        json!({
            "recipient": member_id,
            "topic": "review.requested",
            "payload": {"path": "src/lib.rs"}
        }),
    )
    .await;
    assert_eq!(sent["message"]["sender"]["kind"], "controller");
    let message_id = sent["message"]["id"].as_u64().expect("send 应返回消息 ID");

    let inbox = call_tool(
        host.as_ref(),
        RUN,
        "teammate_inbox",
        json!({"member_id": member_id}),
    )
    .await;
    assert_eq!(inbox["messages"].as_array().map(Vec::len), Some(1));

    let dispatched = call_tool(
        host.as_ref(),
        RUN,
        "teammate_dispatch",
        json!({"member_id": member_id, "message_id": message_id}),
    )
    .await;
    assert_eq!(dispatched["acked"], true);
    // 投胎会得到新的执行句柄，成员地址保持不变。
    assert_ne!(dispatched["handle"]["id"], member_id);
    let second_result = wait_result(host.as_ref(), RUN, member_id).await;
    assert_eq!(second_result["outcome"]["status"], "succeeded");

    // 队长可以一直派活：每轮投胎后旧句柄都会回收，因此累计名额不会被耗尽。
    for round in 0..4 {
        let sent = call_tool(
            host.as_ref(),
            RUN,
            "teammate_send",
            json!({
                "recipient": member_id,
                "topic": "review.followup",
                "payload": {"round": round}
            }),
        )
        .await;
        let message_id = sent["message"]["id"].as_u64().expect("send 应返回消息 ID");
        let dispatched = call_tool(
            host.as_ref(),
            RUN,
            "teammate_dispatch",
            json!({"member_id": member_id, "message_id": message_id}),
        )
        .await;
        assert_ne!(
            dispatched["handle"]["id"], member_id,
            "第 {round} 轮应投胎出新的执行句柄"
        );
        let result = wait_result(host.as_ref(), RUN, member_id).await;
        assert_eq!(result["outcome"]["status"], "succeeded");
    }

    let empty_inbox = call_tool(
        host.as_ref(),
        RUN,
        "teammate_inbox",
        json!({"member_id": member_id}),
    )
    .await;
    assert!(empty_inbox["messages"]
        .as_array()
        .expect("inbox 应返回数组")
        .is_empty());

    // 运行结束事件应把已完成成员从活跃目录移入回收台账，避免成员在界面中长期滞留。
    bus.dispatch(RunFinishEvent {
        run_id: RUN.into(),
        steps_used: 3,
        cancelled: false,
        usage: Default::default(),
        session_id: Some("session-a".into()),
    })
    .await;
    let listed = call_tool(host.as_ref(), RUN, "teammate_list", json!({})).await;
    assert!(
        listed["members"]
            .as_array()
            .expect("活跃成员应是数组")
            .is_empty(),
        "完成成员应在运行结束时自动解散：{listed}"
    );
    assert_eq!(
        listed["retired_members"].as_array().map(Vec::len),
        Some(1),
        "回收台账应保留成员快照：{listed}"
    );
    let retired_result = call_tool(
        host.as_ref(),
        RUN,
        "teammate_result",
        json!({"member_id": member_id}),
    )
    .await;
    assert_eq!(retired_result["retired"], true);
    assert_eq!(retired_result["outcome"]["status"], "succeeded");
    let status_error = call_tool_error(
        host.as_ref(),
        RUN,
        "teammate_status",
        json!({"member_id": member_id}),
    )
    .await;
    assert!(status_error.contains("已回收"), "{status_error}");

    let removed = call_tool(
        host.as_ref(),
        RUN,
        "teammate_remove",
        json!({"member_id": member_id}),
    )
    .await;
    assert_eq!(removed["removed"], true);
    assert_eq!(removed["retired"], true);
    let members = call_tool(host.as_ref(), RUN, "teammate_list", json!({})).await;
    assert!(members["members"]
        .as_array()
        .expect("成员目录应返回数组")
        .is_empty());
    assert!(members["retired_members"]
        .as_array()
        .expect("回收台账应返回数组")
        .is_empty());

    host.deactivate().await.expect("插件应正常卸载");
}

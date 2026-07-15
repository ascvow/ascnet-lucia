//! 插件视图的异步渲染、输入和导航事件桥接。

use crate::*;
use futures_util::future::join_all;

/// 单个插件视图后台渲染结果，保留可信 owner 和实例定位信息。
pub(crate) type PluginRenderResult = (
    String,
    String,
    Option<String>,
    Result<Option<PluginUiFrame>>,
);

/// 在 TUI 事件循环之外并发请求一批插件视图帧。
pub(crate) async fn render_plugin_views(
    plugin_host: Arc<LivePluginHost>,
    requests: Vec<UiRenderRequest>,
) -> Vec<PluginRenderResult> {
    join_all(
        requests
            .into_iter()
            .map(|request| render_plugin_request(plugin_host.as_ref(), request)),
    )
    .await
}

/// 把后台完成的插件视图帧提交到 App 缓存。
pub(crate) fn apply_plugin_frames(app: &mut App, rendered: Vec<PluginRenderResult>) {
    for (plugin_id, view_id, instance_id, result) in rendered {
        match result {
            Ok(Some(frame)) => {
                app.update_plugin_frame(&plugin_id, instance_id.as_deref(), frame);
                focus_visible_input_view(app);
            }
            Ok(None) => {}
            Err(error) => {
                app.set_plugin_ui_error(&plugin_id, &view_id, instance_id.as_deref(), &error)
            }
        }
    }
}

/// 只刷新一次用户操作直接影响的插件视图。
pub(crate) async fn refresh_plugin_view(
    app: &mut App,
    plugin_host: &dyn PluginHost,
    plugin_id: &str,
    view_id: &str,
) {
    let request = app
        .plugin_render_requests()
        .into_iter()
        .find(|request| request.plugin_id == plugin_id && request.view_id == view_id);
    let Some(request) = request else {
        return;
    };
    apply_rendered_request(app, render_plugin_request(plugin_host, request).await);
}

/// 刷新单个插件的全部视图；插件可在输入处理中开关自己的对话框或面板。
pub(crate) async fn refresh_plugin_views_for(
    app: &mut App,
    plugin_host: &dyn PluginHost,
    plugin_id: &str,
) {
    let requests = app
        .plugin_render_requests()
        .into_iter()
        .filter(|request| request.plugin_id == plugin_id)
        .collect::<Vec<_>>();
    for request in requests {
        apply_rendered_request(app, render_plugin_request(plugin_host, request).await);
    }
}

/// 把一次定向渲染结果提交到视图缓存，并同步 Input 视图焦点。
fn apply_rendered_request(app: &mut App, rendered: PluginRenderResult) {
    let (plugin_id, view_id, instance_id, result) = rendered;
    match result {
        Ok(Some(frame)) => {
            app.update_plugin_frame(&plugin_id, instance_id.as_deref(), frame);
            focus_visible_input_view(app);
        }
        Ok(None) => {}
        Err(error) => app.set_plugin_ui_error(&plugin_id, &view_id, instance_id.as_deref(), &error),
    }
}

/// 可见 Input 视图自动占用主输入焦点，隐藏后由 App 恢复主输入。
fn focus_visible_input_view(app: &mut App) {
    if let Some(index) = app
        .plugin_views
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, view)| {
            (view.declaration.placement == UiPlacement::Input && plugin_view_visible(view))
                .then_some(index)
        })
    {
        app.plugin_focus = Some(index);
    }
}

/// 渲染单个插件请求，并保留可信 owner 信息供界面更新。
async fn render_plugin_request(
    plugin_host: &dyn PluginHost,
    request: UiRenderRequest,
) -> PluginRenderResult {
    let plugin_id = request.plugin_id.clone();
    let view_id = request.view_id.clone();
    let instance_id = request.instance_id.clone();
    // WASM component 调用不可通过丢弃 future 取消，否则后续调用和卸载无法重新进入实例。
    let result = plugin_host.render_ui(&request).await;
    (plugin_id, view_id, instance_id, result)
}

/// 向焦点插件发送输入；执行资源由 WASM Host 的运行限额约束。
pub(crate) async fn dispatch_plugin_input(
    plugin_host: &dyn PluginHost,
    input: &UiInput,
) -> Result<()> {
    plugin_host.on_ui_input(input).await
}

/// 解析并应用一条带可信来源的插件视图导航事件。
pub(crate) fn apply_plugin_navigation_event(app: &mut App, event: &Value) -> Result<bool> {
    if event.get("name").and_then(Value::as_str) != Some(UI_NAVIGATION_EVENT) {
        return Ok(false);
    }
    let plugin_id = event
        .pointer("/source/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("视图导航事件缺少可信插件来源"))?;
    let request = serde_json::from_value::<UiNavigationRequest>(
        event.get("data").cloned().unwrap_or(Value::Null),
    )?;
    app.apply_view_navigation(plugin_id, request)?;
    Ok(true)
}

/// 将非导航插件事件映射为主事件列表消息。
fn plugin_event_message(event: &Value) -> Option<Msg> {
    let presentation = event.get("presentation")?;
    if presentation.get("target").and_then(Value::as_str) != Some("main_event_list") {
        return None;
    }
    let text = presentation
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| event.pointer("/data/text").and_then(Value::as_str))
        .or_else(|| event.get("name").and_then(Value::as_str))?
        .to_string();
    let divider = presentation.get("variant").and_then(Value::as_str) == Some("divider");
    let color = match presentation.get("tone").and_then(Value::as_str) {
        Some("success") => COLOR_SUCCESS,
        Some("warning") => COLOR_WARNING,
        Some("error") => COLOR_DANGER,
        Some("muted") => COLOR_MUTED,
        _ => COLOR_USER,
    };
    Some(Msg::extension(text, color, divider))
}

/// 消费插件 UI 输入产生的事件，同时保留普通主事件列表输出。
pub(crate) async fn drain_plugin_ui_events(
    app: &mut App,
    plugin_host: &Arc<LivePluginHost>,
) -> Result<()> {
    for event in agent_core::AgentExtension::drain_events(plugin_host.as_ref()).await? {
        if apply_plugin_navigation_event(app, &event)? {
            continue;
        }
        if apply_plugin_host_action_event(app, plugin_host, &event).await? {
            continue;
        }
        if let Some(message) = plugin_event_message(&event) {
            app.messages.push(message);
        }
    }
    Ok(())
}

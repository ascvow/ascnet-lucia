//! 插件宿主动作事件的执行与会话数据源服务。
//!
//! 本模块只实现与插件业务无关的宿主基础能力：替换主输入、会话新建与恢复、
//! 后台上下文重载、退出应用，以及供插件界面消费的会话摘要查询。

use super::*;

/// 宿主应用调用插件服务时使用的稳定身份。
#[cfg(feature = "plugins")]
pub(crate) const HOST_SERVICE_CALLER: &str = "lucia-tui";

#[cfg(feature = "plugins")]
pub(crate) async fn call_typed_plugin_service<Request, Response>(
    plugin_host: &dyn PluginHost,
    caller_id: &str,
    plugin_id: &str,
    service: &str,
    request: &Request,
) -> Result<Option<Response>>
where
    Request: Serialize,
    Response: DeserializeOwned,
{
    let payload = serde_json::to_value(request)
        .with_context(|| format!("序列化插件服务 `{service}` 请求失败"))?;
    // WASM component 调用不可通过丢弃 future 取消，否则实例可能停留在已进入状态。
    let response = plugin_host
        .call_service(&PluginServiceCall {
            caller_id: caller_id.to_string(),
            plugin_id: plugin_id.to_string(),
            name: service.to_string(),
            payload,
        })
        .await?;
    response
        .map(|value| {
            serde_json::from_value(value)
                .with_context(|| format!("解析插件服务 `{service}` 响应失败"))
        })
        .transpose()
}

/// 解析并执行一条带可信来源的宿主动作事件；非该类事件返回 `Ok(false)`。
///
/// 单个动作的执行失败转换为主事件列表消息，不中断同批次的其余事件。
#[cfg(feature = "plugins")]
pub(crate) async fn apply_plugin_host_action_event(
    app: &mut App,
    plugin_host: &Arc<LivePluginHost>,
    event: &Value,
) -> Result<bool> {
    if event.get("name").and_then(Value::as_str) != Some(UI_HOST_ACTION_EVENT) {
        return Ok(false);
    }
    let plugin_id = event
        .pointer("/source/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("宿主动作事件缺少可信插件来源"))?
        .to_string();
    let request = serde_json::from_value::<UiHostActionRequest>(
        event.get("data").cloned().unwrap_or(Value::Null),
    )?;
    if !app.mark_host_action(&plugin_id, &request.request_id) {
        return Ok(true);
    }
    if let Err(error) = apply_host_action(app, plugin_host, plugin_id, request.action).await {
        app.messages.push(Msg::new(
            MsgKind::Error,
            format!("插件动作执行失败：{error}"),
        ));
    }
    Ok(true)
}

/// 执行单个宿主动作。
#[cfg(feature = "plugins")]
async fn apply_host_action(
    app: &mut App,
    plugin_host: &Arc<LivePluginHost>,
    plugin_id: String,
    action: UiHostAction,
) -> Result<()> {
    match action {
        UiHostAction::SetInput { text, cursor } => app.set_main_input(text, cursor),
        UiHostAction::NewSession => app.start_new_draft("已新建空白会话")?,
        UiHostAction::ClearSession => app.start_new_draft("会话上下文已清空")?,
        UiHostAction::ReloadContext { label } => {
            // 后台重载可能替换会话记录，结束前不接受重复请求。
            if app.pending_reload.is_some() {
                app.messages.push(Msg::new(
                    MsgKind::Info,
                    "上一次上下文重载仍在进行中，请稍候",
                ));
            } else {
                start_session_context_reload(
                    app,
                    Arc::clone(plugin_host),
                    label.unwrap_or_default(),
                );
            }
        }
        UiHostAction::Exit => app.should_quit = true,
        UiHostAction::ResumeSession {
            session_id,
            revision,
        } => {
            resume_selected_session(app, &session_id, revision).await?;
            app.plugin_focus = None;
        }
        UiHostAction::QuerySessions {
            query_id,
            query,
            cursor,
            limit,
            reply_service,
        } => app.start_sessions_query(plugin_id, reply_service, query_id, query, cursor, limit),
    }
    Ok(())
}

/// 在后台通过已注册的上下文加载器重载当前会话并持久化变化。
///
/// 立即返回以保持事件循环渲染；结果经 `SessionContextReloaded` 事件回投主循环，
/// 处理过程的用户可见说明由加载器插件发布的展示事件提供。
#[cfg(feature = "plugins")]
pub(crate) fn start_session_context_reload(
    app: &mut App,
    plugin_host: Arc<LivePluginHost>,
    label: String,
) {
    app.pending_reload = Some(label);
    let record = app.session_record.clone();
    let model_name = app.model_name.clone();
    let session_store = Arc::clone(&app.session_store);
    let tx = app.tx.clone();
    tokio::spawn(async move {
        let result = reload_session_context(
            plugin_host.as_ref(),
            session_store.as_ref(),
            record,
            model_name,
        )
        .await;
        let _ = tx.send(UiEvent::SessionContextReloaded(Box::new(result)));
    });
}

/// 请求上下文加载器重载给定会话记录；仅在内容变化时保存并返回新记录。
#[cfg(feature = "plugins")]
async fn reload_session_context(
    plugin_host: &dyn PluginHost,
    session_store: &dyn SessionStore,
    record: SessionRecord,
    model_name: String,
) -> Result<SessionReloadOutcome> {
    let request = ContextLoadRequest {
        run_id: format!("reload:{}:{}", record.id, record.revision),
        step: 0,
        provider: "manual".into(),
        model: model_name,
        system: record.session.system().cloned(),
        messages: record.session.model_messages(),
        user_initiated: true,
    };
    let Some(loaded) = plugin_host.load_context(&request).await? else {
        return Ok(SessionReloadOutcome::NoLoader);
    };
    if loaded.system == request.system && loaded.messages == request.messages {
        return Ok(SessionReloadOutcome::Unchanged);
    }
    let mut reloaded = record;
    reloaded.session = Session::from_parts(loaded.system, loaded.messages);
    let expected_revision = (reloaded.revision > 0).then_some(reloaded.revision);
    let saved = save_session_record_reconciled(session_store, reloaded, expected_revision).await?;
    Ok(SessionReloadOutcome::Replaced(saved))
}

/// 读取、过滤并分页当前项目的轻量会话摘要。
#[cfg(feature = "plugins")]
pub(crate) async fn sessions_page(
    session_store: &dyn SessionStore,
    active_session_id: &SessionId,
    query: &str,
    cursor: Option<&str>,
    limit: u16,
) -> UiSessionListStatus {
    let mut summaries = match session_store.list_summaries().await {
        Ok(summaries) => summaries,
        Err(error) => {
            return UiSessionListStatus::Error {
                message: format!("读取会话列表失败：{error}"),
            };
        }
    };
    let query = query.trim().to_lowercase();
    summaries.retain(|summary| {
        query.is_empty()
            || summary.id.as_str().to_lowercase().contains(&query)
            || summary
                .title
                .as_deref()
                .is_some_and(|title| title.to_lowercase().contains(&query))
    });
    summaries.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });

    let offset = cursor
        .and_then(|cursor| cursor.parse::<usize>().ok())
        .unwrap_or(0)
        .min(summaries.len());
    let page_limit = usize::from(limit.clamp(1, 100));
    let page_end = offset.saturating_add(page_limit).min(summaries.len());
    let items = summaries[offset..page_end]
        .iter()
        .map(|summary| ui_session_summary(active_session_id, summary))
        .collect::<Vec<_>>();
    if items.is_empty() {
        UiSessionListStatus::Empty
    } else {
        UiSessionListStatus::Ready {
            items,
            next_cursor: (page_end < summaries.len()).then(|| page_end.to_string()),
        }
    }
}

/// 将存储层摘要映射为 UI 契约的会话摘要。
#[cfg(feature = "plugins")]
fn ui_session_summary(active_session_id: &SessionId, summary: &SessionSummary) -> UiSessionSummary {
    UiSessionSummary {
        id: summary.id.to_string(),
        title: summary
            .title
            .clone()
            .unwrap_or_else(|| summary.id.to_string()),
        message_count: u64::try_from(summary.message_count).unwrap_or(u64::MAX),
        updated_at_ms: summary.updated_at_ms,
        updated_label: relative_time_label(summary.updated_at_ms),
        revision: summary.revision,
        active: &summary.id == active_session_id,
    }
}

/// 生成人类可读且无需额外依赖的会话更新时间标签。
#[cfg(feature = "plugins")]
pub(crate) fn relative_time_label(updated_at_ms: u64) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| u64::try_from(duration.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(updated_at_ms);
    let elapsed_seconds = now_ms.saturating_sub(updated_at_ms) / 1_000;
    match elapsed_seconds {
        0..=59 => "刚刚".into(),
        60..=3_599 => format!("{} 分钟前", elapsed_seconds / 60),
        3_600..=86_399 => format!("{} 小时前", elapsed_seconds / 3_600),
        _ => format!("{} 天前", elapsed_seconds / 86_400),
    }
}

/// 在切换前重新读取并校验用户选择的会话修订号。
#[cfg(feature = "plugins")]
pub(crate) async fn resume_selected_session(
    app: &mut App,
    session_id: &str,
    revision: u64,
) -> Result<()> {
    let id = SessionId::new(session_id)?;
    let record = app
        .session_store
        .load(&id)
        .await?
        .ok_or_else(|| anyhow!("会话 `{session_id}` 已不存在"))?;
    if record.revision != revision {
        return Err(anyhow!("会话 `{session_id}` 已更新，请刷新列表后重新选择"));
    }
    let notice = format!("已恢复会话 {}", record.id);
    app.replace_session(record, Some(&notice));
    Ok(())
}

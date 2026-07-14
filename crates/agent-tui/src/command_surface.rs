//! 官方 Command 插件协议与受控 TUI surface effect 适配。

use super::*;

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

/// 从官方 Command Provider 读取可在输入热路径中复用的命令快照。
#[cfg(feature = "plugins")]
pub(crate) async fn load_command_snapshot(
    plugin_host: &dyn PluginHost,
) -> Result<Option<CommandSnapshot>> {
    call_typed_plugin_service(
        plugin_host,
        TUI_COMMAND_CALLER,
        PROVIDER_PLUGIN_ID,
        SNAPSHOT_SERVICE,
        &SnapshotRequest::default(),
    )
    .await
}

/// 按 Provider 计划解析一次显式参数候选请求。
#[cfg(feature = "plugins")]
pub(crate) async fn resolve_command_completion(
    plugin_host: &dyn PluginHost,
    session_store: &dyn SessionStore,
    source_input: String,
    source_cursor: usize,
) -> Result<Option<ResolvedCommandCompletion>> {
    let cursor = u32::try_from(source_cursor).context("命令补全光标超出协议范围")?;
    let response: PrepareCompletionResponse = call_typed_plugin_service(
        plugin_host,
        TUI_COMMAND_CALLER,
        PROVIDER_PLUGIN_ID,
        PREPARE_COMPLETION_SERVICE,
        &PrepareCompletionRequest {
            input: source_input.clone(),
            cursor: Some(cursor),
            limit: 6,
        },
    )
    .await?
    .ok_or_else(|| anyhow!("Command 插件不可用，无法补全参数"))?;

    match response {
        PrepareCompletionResponse::Candidates { context, items } => Ok(
            resolved_command_completion(&source_input, source_cursor, context, items),
        ),
        PrepareCompletionResponse::Callback {
            context,
            owner_plugin_id,
            service,
            request,
        } => {
            let response: CommandCallbackResponse = call_typed_plugin_service(
                plugin_host,
                PROVIDER_PLUGIN_ID,
                &owner_plugin_id,
                &service,
                &request,
            )
            .await?
            .ok_or_else(|| anyhow!("候选提供插件 `{owner_plugin_id}` 不可用"))?;
            match response {
                CommandCallbackResponse::Completed { items } => Ok(resolved_command_completion(
                    &source_input,
                    source_cursor,
                    context,
                    items,
                )),
                CommandCallbackResponse::Executed { .. } => {
                    Err(anyhow!("参数补全回调返回了执行结果"))
                }
            }
        }
        PrepareCompletionResponse::Surface { context, request } => {
            if request.source != SESSION_COMPLETION_SOURCE {
                return Err(anyhow!("不支持的命令候选数据源：{}", request.source));
            }
            let items = session_completion_items(
                session_store,
                &request.request.prefix,
                request.request.limit,
            )
            .await?;
            Ok(resolved_command_completion(
                &source_input,
                source_cursor,
                context,
                items,
            ))
        }
        PrepareCompletionResponse::NoMatch => Ok(None),
        PrepareCompletionResponse::Error { message } => Err(anyhow!(message)),
    }
}

/// 构造最多六项的稳定 TUI 参数候选。
#[cfg(feature = "plugins")]
pub(crate) fn resolved_command_completion(
    source_input: &str,
    source_cursor: usize,
    context: CompletionContext,
    mut items: Vec<CompletionItem>,
) -> Option<ResolvedCommandCompletion> {
    items.truncate(6);
    (!items.is_empty()).then(|| ResolvedCommandCompletion {
        source_input: source_input.to_string(),
        source_cursor,
        context,
        items,
    })
}

/// 从当前项目的轻量摘要生成 Session 参数候选。
#[cfg(feature = "plugins")]
pub(crate) async fn session_completion_items(
    session_store: &dyn SessionStore,
    prefix: &str,
    limit: u16,
) -> Result<Vec<CompletionItem>> {
    let mut summaries = session_store.list_summaries().await?;
    summaries.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    let prefix = prefix.to_lowercase();
    Ok(summaries
        .into_iter()
        .filter(|summary| {
            prefix.is_empty()
                || summary.id.as_str().to_lowercase().contains(&prefix)
                || summary
                    .title
                    .as_deref()
                    .is_some_and(|title| title.to_lowercase().contains(&prefix))
        })
        .take(usize::from(limit.clamp(1, 6)))
        .map(|summary| CompletionItem {
            label: summary
                .title
                .clone()
                .unwrap_or_else(|| summary.id.to_string()),
            insert_text: summary.id.to_string(),
            description: Some(format!(
                "{} 条消息 · {}",
                summary.message_count,
                relative_time_label(summary.updated_at_ms)
            )),
        })
        .collect())
}

/// 执行 Command Provider 生成的受控计划，并刷新可能变化的命令快照。
#[cfg(feature = "plugins")]
pub(crate) async fn execute_command(
    app: &mut App,
    plugin_host: &Arc<LivePluginHost>,
    input: String,
) -> Result<()> {
    if app.running
        && app
            .command_spec_for_input(&input)
            .is_some_and(|spec| spec.availability == CommandAvailability::IdleOnly)
    {
        app.messages
            .push(Msg::new(MsgKind::Error, "该命令只能在 Agent 空闲时执行"));
        return Ok(());
    }
    // 后台命令可能替换会话记录，结束前不接受新的命令。
    if app.pending_command.is_some() {
        app.messages
            .push(Msg::new(MsgKind::Info, "上一条命令仍在执行中，请稍候"));
        return Ok(());
    }

    // 保留命令原文，供转入后台执行时作为进行中指示的标签。
    let command_label = input.trim().to_string();
    let response: PrepareExecuteResponse = call_typed_plugin_service(
        plugin_host.as_ref(),
        TUI_COMMAND_CALLER,
        PROVIDER_PLUGIN_ID,
        PREPARE_EXECUTE_SERVICE,
        &PrepareExecuteRequest {
            input,
            agent_idle: !app.running,
        },
    )
    .await?
    .ok_or_else(|| anyhow!("Command 插件不可用，无法执行斜杠命令"))?;

    match response {
        PrepareExecuteResponse::Callback {
            owner_plugin_id,
            service,
            request,
        } => {
            let response: CommandCallbackResponse = call_typed_plugin_service(
                plugin_host.as_ref(),
                PROVIDER_PLUGIN_ID,
                &owner_plugin_id,
                &service,
                &request,
            )
            .await?
            .ok_or_else(|| anyhow!("命令处理插件 `{owner_plugin_id}` 不可用"))?;
            match response {
                CommandCallbackResponse::Executed { result } => {
                    let content = match result {
                        Value::Null => "命令执行完成".to_string(),
                        Value::String(content) => content,
                        value => serde_json::to_string_pretty(&value)
                            .context("格式化命令执行结果失败")?,
                    };
                    app.messages
                        .push(Msg::new(MsgKind::Info, truncate_line(&content, 4_000)));
                }
                CommandCallbackResponse::Completed { .. } => {
                    return Err(anyhow!("命令执行回调返回了补全结果"));
                }
            }
        }
        PrepareExecuteResponse::SurfaceAction { action } => match action {
            SurfaceAction::NewSession => app.start_new_draft("已新建空白会话")?,
            SurfaceAction::ClearSession => app.start_new_draft("会话上下文已清空")?,
            SurfaceAction::ReloadSessionContext => {
                start_session_context_reload(app, Arc::clone(plugin_host), command_label);
            }
            SurfaceAction::ExitApplication => app.should_quit = true,
        },
        PrepareExecuteResponse::SurfaceOpened { view_id } => {
            if view_id != SESSION_DIALOG_VIEW {
                return Err(anyhow!("Command 插件返回了未知界面：{view_id}"));
            }
            process_command_surface_effects(app, plugin_host.as_ref()).await?;
            refresh_plugin_view(
                app,
                plugin_host.as_ref(),
                PROVIDER_PLUGIN_ID,
                SESSION_DIALOG_VIEW,
            )
            .await;
        }
        PrepareExecuteResponse::Output { content } => {
            app.messages.push(Msg::new(MsgKind::Info, content));
        }
        PrepareExecuteResponse::Error { message, usage } => {
            let content = usage
                .filter(|usage| !usage.trim().is_empty())
                .map(|usage| format!("{message}\n用法：{usage}"))
                .unwrap_or(message);
            app.messages.push(Msg::new(MsgKind::Error, content));
        }
    }

    if let Some(snapshot) = load_command_snapshot(plugin_host.as_ref()).await? {
        app.set_command_snapshot(Some(snapshot));
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
    command: String,
) {
    app.pending_command = Some(command);
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

/// 处理 Command 会话界面产生的查询、恢复和关闭动作。
#[cfg(feature = "plugins")]
pub(crate) async fn process_command_surface_effects(
    app: &mut App,
    plugin_host: &dyn PluginHost,
) -> Result<()> {
    // 单次输入最多处理有限轮 effect，避免异常插件使事件循环无法返回。
    for _ in 0..8 {
        let response: SurfaceEffectsResponse = call_typed_plugin_service(
            plugin_host,
            TUI_COMMAND_CALLER,
            PROVIDER_PLUGIN_ID,
            SURFACE_POLL_EFFECTS_SERVICE,
            &SnapshotRequest::default(),
        )
        .await?
        .ok_or_else(|| anyhow!("Command 插件在会话界面打开后不可用"))?;
        if response.effects.is_empty() {
            return Ok(());
        }

        for effect in response.effects {
            match effect {
                SurfaceEffect::QuerySessions {
                    request_id,
                    query,
                    cursor,
                    limit,
                } => {
                    app.start_command_query(request_id, query, cursor, limit);
                }
                SurfaceEffect::ResumeSession {
                    session_id,
                    revision,
                } => {
                    resume_selected_session(app, &session_id, revision).await?;
                    app.plugin_focus = None;
                }
                SurfaceEffect::CloseSurface => app.plugin_focus = None,
            }
        }
    }
    Err(anyhow!("Command 会话界面产生了过多连续动作"))
}

/// 读取、过滤并分页当前项目的轻量会话摘要。
#[cfg(feature = "plugins")]
pub(crate) async fn command_session_page(
    session_store: &dyn SessionStore,
    active_session_id: &SessionId,
    query: &str,
    cursor: Option<&str>,
    limit: u16,
) -> SessionListStatus {
    let mut summaries = match session_store.list_summaries().await {
        Ok(summaries) => summaries,
        Err(error) => {
            return SessionListStatus::Error {
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
        .map(|summary| command_session_summary(active_session_id, summary))
        .collect::<Vec<_>>();
    if items.is_empty() {
        SessionListStatus::Empty
    } else {
        SessionListStatus::Ready {
            items,
            next_cursor: (page_end < summaries.len()).then(|| page_end.to_string()),
        }
    }
}

/// 将存储层摘要映射为 Command 插件稳定协议。
#[cfg(feature = "plugins")]
pub(crate) fn command_session_summary(
    active_session_id: &SessionId,
    summary: &SessionSummary,
) -> CommandSessionSummary {
    CommandSessionSummary {
        id: summary.id.to_string(),
        title: summary
            .title
            .clone()
            .unwrap_or_else(|| summary.id.to_string()),
        preview: String::new(),
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
        return Err(anyhow!(
            "会话 `{session_id}` 已更新，请重新打开 /resume 后选择"
        ));
    }
    let notice = format!("已恢复会话 {}", record.id);
    app.replace_session(record, Some(&notice));
    Ok(())
}

// ─── 主函数 ───

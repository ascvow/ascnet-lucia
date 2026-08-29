//! 插件视图状态维护、渲染调度与输入路由。

use super::App;
use crate::app::{MainEventSnapshot, PluginKeyRoute, PluginViewState};
use crate::event::UiEvent;
use crate::plugin::host_actions::sessions_page;
use crate::plugin::startup::plugin_startup_details;
use crate::session::conversation::MsgKind;
use crate::theme::{
    COLOR_DANGER, COLOR_SUCCESS, COLOR_WARNING, PLUGIN_STATUS_DETAIL_TICKS, SPINNER,
};
use crate::ui::plugin_bridge::render_plugin_views;
use agent_plugin_host::wasm::PluginLoadFailure;
use agent_plugin_host::LivePluginHost;
use agent_plugin_protocol::UiFrame as PluginUiFrame;
use agent_plugin_protocol::{
    ToolRenderContext, ToolRenderState, UiColor, UiDeclaration, UiInput, UiInputEvent, UiLine,
    UiNavigationRequest, UiPlacement, UiRenderRequest, UiSessionsReply, UiSpan, UiStyle,
};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use serde_json::Value;
use std::sync::Arc;

impl App {
    /// 追加一个刚 Ready 插件的视图声明，不重置其他插件的帧、焦点或导航栈。
    #[cfg(feature = "plugins")]
    pub(crate) fn add_plugin_views(&mut self, declarations: Vec<UiDeclaration>) {
        self.plugin_views
            .extend(declarations.into_iter().map(|declaration| PluginViewState {
                declaration,
                frame: None,
                area: Rect::default(),
            }));
    }

    /// 为指定工具消息请求一次通用 Host 渲染；没有贡献时保持默认样式。
    #[cfg(feature = "plugins")]
    pub(crate) fn schedule_tool_render(&mut self, host: Arc<LivePluginHost>, call_id: &str) {
        let width = self.last_message_width.max(1);
        let max_height = self.last_viewport.max(1);
        let Some(index) = self
            .messages
            .iter()
            .position(|message| message.tool_call_id() == Some(call_id))
        else {
            return;
        };
        let message = &self.messages[index];
        let Some(call) = message.tool_call.clone() else {
            return;
        };
        let state = match message.kind {
            MsgKind::ToolRunning => ToolRenderState::Running,
            MsgKind::ToolOk | MsgKind::ToolError => {
                let Some(result) = message.tool_result.clone() else {
                    return;
                };
                ToolRenderState::Finished { result }
            }
            MsgKind::ToolSkipped => ToolRenderState::Skipped {
                reason: message.skip_reason.clone().unwrap_or_default(),
            },
            _ => return,
        };
        self.plugin_frame = self.plugin_frame.wrapping_add(1);
        let context = ToolRenderContext {
            call,
            state,
            width,
            max_height,
            frame: self.plugin_frame,
        };
        let revision = self.messages[index].tool_render_revision.wrapping_add(1);
        self.messages[index].tool_render_revision = revision;
        self.messages[index].tool_render_pending_width = Some(width);
        let tx = self.tx.clone();
        let call_id = call_id.to_string();
        tokio::spawn(async move {
            let result = host.render_tool_message(&context).await;
            let _ = tx.send(UiEvent::ToolFrameLoaded {
                call_id,
                revision,
                width,
                result,
            });
        });
    }

    /// 对没有当前宽度帧的工具消息发起渲染，用于插件就绪和终端尺寸变化。
    #[cfg(feature = "plugins")]
    pub(crate) fn schedule_stale_tool_renders(&mut self, host: Arc<LivePluginHost>) {
        let width = self.last_message_width.max(1);
        let call_ids = self
            .messages
            .iter()
            .filter(|message| {
                message.tool_call.is_some()
                    && message.tool_render_pending_width != Some(width)
                    && message.tool_frame_width != Some(width)
            })
            .filter_map(|message| message.tool_call_id().map(str::to_string))
            .collect::<Vec<_>>();
        for call_id in call_ids {
            self.schedule_tool_render(Arc::clone(&host), &call_id);
        }
    }

    /// 清除 Host 路由变化前的工具帧，并使并发中的旧渲染结果失效。
    #[cfg(feature = "plugins")]
    pub(crate) fn invalidate_tool_frames(&mut self) {
        for message in self
            .messages
            .iter_mut()
            .filter(|message| message.tool_call.is_some())
        {
            message.tool_frame = None;
            message.tool_frame_width = None;
            message.tool_render_pending_width = None;
            message.tool_render_revision = message.tool_render_revision.wrapping_add(1);
        }
    }

    /// 提交一次工具消息 renderer 结果；失败时保留默认工具展示。
    #[cfg(feature = "plugins")]
    pub(crate) fn apply_tool_frame(
        &mut self,
        call_id: &str,
        revision: u64,
        width: u16,
        result: Result<Option<PluginUiFrame>>,
    ) {
        let Some(message) = self
            .messages
            .iter_mut()
            .find(|message| message.tool_call_id() == Some(call_id))
        else {
            return;
        };
        if message.tool_render_revision != revision {
            return;
        }
        message.tool_render_pending_width = None;
        message.tool_frame_width = Some(width);
        match result {
            Ok(Some(frame)) => message.tool_frame = Some(frame),
            Ok(None) => message.tool_frame = None,
            Err(error) => {
                message.tool_frame = None;
                self.plugin_failures.push(format!(
                    "Failed to render tool message `{call_id}`: {error}"
                ));
            }
        }
    }

    /// 在应用导航栈上执行一次经 Host 标记来源的插件视图请求。
    #[cfg(feature = "plugins")]
    pub(crate) fn apply_view_navigation(
        &mut self,
        plugin_id: &str,
        request: UiNavigationRequest,
    ) -> Result<bool> {
        let declarations = self
            .plugin_views
            .iter()
            .map(|view| view.declaration.clone())
            .collect::<Vec<_>>();
        let changed = self.view_stack.apply(plugin_id, request, &declarations)?;
        if changed {
            self.plugin_focus = None;
        }
        Ok(changed)
    }

    /// Marks plugin IDs as loading while keeping the input queue available.
    ///
    /// 标记正在加载的插件 ID，同时保持输入队列可用。
    #[cfg(feature = "plugins")]
    pub(crate) fn with_loading_plugins(mut self, plugin_ids: Vec<String>) -> Self {
        self.plugin_ids.clear();
        self.plugin_loading_ids = plugin_ids;
        self.plugins_loading = true;
        self.plugin_load_error = None;
        self.plugin_failures.clear();
        self
    }

    /// 记录一个渐进加载完成的插件，使其立即进入 Ready 计数并移出加载列表。
    #[cfg(feature = "plugins")]
    pub(crate) fn mark_plugin_ready(
        &mut self,
        plugin_id: String,
        events: &[Value],
        load_duration_ms: u64,
    ) {
        self.plugin_loading_ids.retain(|id| id != &plugin_id);
        if !self.plugin_ids.contains(&plugin_id) {
            self.plugin_ids.push(plugin_id.clone());
        }
        let mut details = plugin_startup_details(std::slice::from_ref(&plugin_id), events);
        if load_duration_ms > 0 {
            for detail in &mut details {
                detail.push_str(&format!(" · {load_duration_ms} ms"));
            }
        }
        self.plugin_startup_details.extend(details);
    }

    /// 记录一个渐进加载失败的插件并立即从加载列表移除。
    #[cfg(feature = "plugins")]
    pub(crate) fn mark_plugin_failed(&mut self, failure: PluginLoadFailure) {
        self.plugin_loading_ids
            .retain(|id| id != &failure.plugin_id);
        let blocked = if failure.blocked_by.is_empty() {
            String::new()
        } else {
            format!("; depends on {}", failure.blocked_by.join(", "))
        };
        let detail = format!(
            "{}: load failed{blocked} · {}",
            failure.plugin_id, failure.reason
        );
        self.plugin_failures.push(detail.clone());
        self.plugin_startup_details.push(detail);
    }

    /// 启动或合并一次后台插件视图刷新，保证任一时刻至多存在一个渲染批次。
    #[cfg(feature = "plugins")]
    pub(crate) fn schedule_plugin_views_refresh(&mut self, host: Arc<LivePluginHost>) {
        if self.plugin_refresh_task.is_some() {
            self.plugin_refresh_pending = true;
            return;
        }
        let requests = self.periodic_plugin_render_requests();
        if requests.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        self.plugin_refresh_task = Some(tokio::spawn(async move {
            let rendered = render_plugin_views(host, requests).await;
            let _ = tx.send(UiEvent::PluginFramesLoaded(rendered));
        }));
    }

    /// 结束渐进加载并保留已经逐项收集的成功详情与失败信息。
    #[cfg(feature = "plugins")]
    pub(crate) fn finish_progressive_plugin_loading(&mut self) {
        self.plugin_loading_ids.clear();
        self.plugins_loading = false;
        self.plugin_load_error = None;
        self.plugin_status_ticks = PLUGIN_STATUS_DETAIL_TICKS;
    }

    /// 记录渐进加载的全局规划错误，保留此前已经 Ready 的插件。
    #[cfg(feature = "plugins")]
    pub(crate) fn set_progressive_plugin_load_error(&mut self, error: &anyhow::Error) {
        self.plugins_loading = false;
        self.plugin_loading_ids.clear();
        self.plugin_status_ticks = 0;
        self.plugin_load_error = Some(error.to_string());
    }

    /// Advances the transient startup status toward the compact counter.
    ///
    /// 推进一次性启动状态，并在计时结束后切换为紧凑计数。
    #[cfg(feature = "plugins")]
    pub(crate) fn tick_plugin_status(&mut self) {
        if !self.plugins_loading {
            self.plugin_status_ticks = self.plugin_status_ticks.saturating_sub(1);
        }
    }

    /// Returns the current plugin status icon and text for the footer's right side.
    ///
    /// 返回底部信息栏右侧当前使用的插件状态图标和文本。
    #[cfg(feature = "plugins")]
    pub(crate) fn plugin_status_content(&self) -> (&'static str, String) {
        if self.plugins_loading {
            let plugins = self.plugin_loading_ids.join(" · ");
            let ready = if self.plugin_ids.is_empty() {
                String::new()
            } else {
                format!(" · {} ready", self.plugin_ids.len())
            };
            let queue = if self.queued_inputs.is_empty() {
                String::new()
            } else {
                format!(" · queued {}", self.queued_inputs.len())
            };
            let text = if plugins.is_empty() {
                format!("Loading plugins{ready}{queue}")
            } else {
                format!("Loading plugins · {plugins}{ready}{queue}")
            };
            return (SPINNER[self.spinner_frame % SPINNER.len()], text);
        }
        if let Some(error) = &self.plugin_load_error {
            return ("✗", format!("Plugin loading failed · {error}"));
        }
        if self.plugin_status_ticks > 0 {
            let details = if self.plugin_startup_details.is_empty() {
                self.plugin_ids.join(" · ")
            } else {
                self.plugin_startup_details.join(" · ")
            };
            let text = if details.is_empty() {
                "No plugins loaded".to_string()
            } else if self.plugin_failures.is_empty() {
                format!("Plugins loaded · {details}")
            } else {
                format!("Plugins partially loaded · {details}")
            };
            (
                if self.plugin_failures.is_empty() {
                    "✓"
                } else {
                    "!"
                },
                text,
            )
        } else if self.plugin_failures.is_empty() {
            ("◈", format!("{} plugins", self.plugin_ids.len()))
        } else {
            (
                "◈",
                format!(
                    "{} plugins · ✗ {}",
                    self.plugin_ids.len(),
                    self.plugin_failures.len()
                ),
            )
        }
    }

    /// Returns the semantic color for the current plugin footer state.
    ///
    /// 返回当前插件底栏状态的语义颜色。
    #[cfg(feature = "plugins")]
    pub(crate) fn plugin_status_color(&self) -> Color {
        if self.plugin_load_error.is_some() {
            COLOR_DANGER
        } else if self.plugins_loading || !self.plugin_failures.is_empty() {
            COLOR_WARNING
        } else {
            COLOR_SUCCESS
        }
    }

    /// 返回主输入当前激活的触发视图索引。
    ///
    /// 主输入去除前导空白后以某个视图声明的触发前缀开头即视为激活；
    /// 多个视图声明重叠前缀时按声明顺序取第一个。
    #[cfg(feature = "plugins")]
    pub(crate) fn active_trigger_view(&self) -> Option<usize> {
        let input = self.input.trim_start();
        if input.is_empty() {
            return None;
        }
        self.plugin_views.iter().position(|view| {
            view.declaration
                .input_triggers
                .iter()
                .any(|trigger| !trigger.is_empty() && input.starts_with(trigger.as_str()))
        })
    }

    /// 返回当前应显示在输入区上方的输入面板视图索引。
    #[cfg(feature = "plugins")]
    pub(crate) fn visible_input_panel(&self) -> Option<usize> {
        let index = self.active_trigger_view()?;
        let view = &self.plugin_views[index];
        (view.declaration.placement == UiPlacement::InputPanel && plugin_view_visible(view))
            .then_some(index)
    }

    /// 返回当前独占主输入区的最后一个可见插件视图。
    #[cfg(feature = "plugins")]
    pub(crate) fn visible_plugin_input(&self) -> Option<usize> {
        self.plugin_views
            .iter()
            .enumerate()
            .rev()
            .find(|(_, view)| {
                view.declaration.placement == UiPlacement::Input && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
    }

    /// 返回输入框上方当前参与布局的视图，并落实独占输入、触发面板和常驻架优先级。
    #[cfg(feature = "plugins")]
    pub(crate) fn visible_composer_panels(&self) -> Vec<usize> {
        if self.visible_plugin_input().is_some() {
            return Vec::new();
        }
        if self.native_command_panel_height() > 0 {
            return Vec::new();
        }
        if let Some(index) = self.visible_input_panel() {
            return vec![index];
        }
        self.plugin_views
            .iter()
            .enumerate()
            .filter(|(_, view)| {
                view.declaration.placement == UiPlacement::ComposerShelf
                    && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
            .collect()
    }

    /// 计算当前输入上方视图所需总高度；每个视图包含一行宿主分隔线。
    #[cfg(feature = "plugins")]
    pub(crate) fn composer_panel_height(&self) -> u16 {
        self.visible_composer_panels()
            .into_iter()
            .fold(0u16, |height, index| {
                height.saturating_add(self.composer_panel_height_at(index))
            })
    }

    /// 返回单个输入上方视图受声明上限约束后的布局高度。
    #[cfg(feature = "plugins")]
    pub(crate) fn composer_panel_height_at(&self, index: usize) -> u16 {
        let view = &self.plugin_views[index];
        let lines = view
            .frame
            .as_ref()
            .map(|frame| frame.lines.len())
            .unwrap_or(0);
        if lines == 0 {
            return 0;
        }
        let max = view
            .declaration
            .size
            .height
            .unwrap_or_else(|| default_plugin_height(view.declaration.placement));
        u16::try_from(lines)
            .unwrap_or(max)
            .saturating_add(1)
            .min(max)
    }

    /// 构造发送给触发视图的主输入快照事件。
    #[cfg(feature = "plugins")]
    pub(crate) fn main_input_snapshot(&self, index: usize) -> UiInput {
        let declaration = &self.plugin_views[index].declaration;
        UiInput {
            plugin_id: declaration.plugin_id.clone(),
            view_id: declaration.view_id.clone(),
            instance_id: None,
            event: UiInputEvent::MainInput {
                text: self.input.clone(),
                cursor: u32::try_from(self.cursor).unwrap_or(u32::MAX),
            },
        }
    }

    /// 记录一条宿主动作请求；返回 `false` 表示重复交付应被忽略。
    #[cfg(feature = "plugins")]
    pub(crate) fn mark_host_action(&mut self, plugin_id: &str, request_id: &str) -> bool {
        let key = (plugin_id.to_string(), request_id.to_string());
        if self.applied_host_actions.contains(&key) {
            return false;
        }
        if self.applied_host_actions.len() >= 64 {
            self.applied_host_actions.pop_front();
        }
        self.applied_host_actions.push_back(key);
        true
    }

    /// 按插件请求替换主输入内容与光标，并保持附件引用一致。
    #[cfg(feature = "plugins")]
    pub(crate) fn set_main_input(&mut self, text: String, cursor: Option<u32>) {
        self.input = text;
        let cursor = cursor
            .and_then(|cursor| usize::try_from(cursor).ok())
            .unwrap_or(self.input.len())
            .min(self.input.len());
        // 光标落在字符中间时回退到最近的合法边界。
        self.cursor = (0..=cursor)
            .rev()
            .find(|offset| self.input.is_char_boundary(*offset))
            .unwrap_or(0);
        self.input_history_cursor = None;
        self.last_escape_at = None;
        self.prune_attachments();
    }

    /// 启动可取消的后台会话摘要查询，结果经 UI 事件回送发起插件。
    #[cfg(feature = "plugins")]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_sessions_query(
        &mut self,
        plugin_id: String,
        reply_service: String,
        query_id: u64,
        query: String,
        cursor: Option<String>,
        limit: u16,
    ) {
        if let Some(task) = self.sessions_query_task.take() {
            task.abort();
        }
        let session_store = Arc::clone(&self.session_store);
        let active_session_id = self.session_record.id.clone();
        let tx = self.tx.clone();
        self.sessions_query_task = Some(tokio::spawn(async move {
            // 搜索输入短暂防抖，连续键入只扫描一次项目会话目录。
            if cursor.is_none() && !query.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(75)).await;
            }
            let status = sessions_page(
                session_store.as_ref(),
                &active_session_id,
                &query,
                cursor.as_deref(),
                limit,
            )
            .await;
            let _ = tx.send(UiEvent::SessionsQueryDone {
                plugin_id,
                reply_service,
                reply: Box::new(UiSessionsReply { query_id, status }),
            });
        }));
    }

    /// 根据模态层和 Tab 焦点决定键盘事件的接收者。
    #[cfg(feature = "plugins")]
    pub(crate) fn route_plugin_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> PluginKeyRoute {
        if !matches!(code, KeyCode::Esc) {
            self.last_escape_at = None;
        }
        if modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c')) {
            return PluginKeyRoute::Main;
        }

        if self.view_stack.active().is_some() {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
                self.view_stack.pop_for_user();
                return PluginKeyRoute::Consumed;
            }
            let active = self.view_stack.active().expect("子视图已确认存在");
            return PluginKeyRoute::Input(UiInput {
                plugin_id: active.plugin_id.clone(),
                view_id: active.view.view_id.clone(),
                instance_id: Some(active.view.instance_id.clone()),
                event: UiInputEvent::Key {
                    code: plugin_key_code(code),
                    modifiers: plugin_key_modifiers(modifiers),
                },
            });
        }

        if let Some(index) = self.active_dialog_index() {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
            }
            return PluginKeyRoute::Input(self.plugin_key_input(index, code, modifiers));
        }

        // 触发前缀激活时，无修饰的补全与提交手势交给触发视图处理；
        // 方向键和 Esc 仅在输入面板可见时转发，避免面板隐藏后劫持滚动。
        if let Some(index) = self.active_trigger_view() {
            if modifiers.is_empty() {
                let panel_visible = self.visible_input_panel() == Some(index);
                let forward = match code {
                    KeyCode::Tab | KeyCode::Enter => true,
                    KeyCode::Up | KeyCode::Down | KeyCode::Esc => panel_visible,
                    _ => false,
                };
                if forward {
                    if matches!(code, KeyCode::Esc) {
                        self.last_escape_at = None;
                    }
                    return PluginKeyRoute::Input(self.plugin_key_input(index, code, modifiers));
                }
            }
        }

        if matches!(code, KeyCode::Tab | KeyCode::BackTab) {
            let reverse =
                matches!(code, KeyCode::BackTab) || modifiers.contains(KeyModifiers::SHIFT);
            self.cycle_plugin_focus(reverse);
            return PluginKeyRoute::Consumed;
        }

        if let Some(index) = self.plugin_focus {
            if matches!(code, KeyCode::Esc) {
                self.last_escape_at = None;
                self.plugin_focus = None;
                return PluginKeyRoute::Consumed;
            }
            return PluginKeyRoute::Input(self.plugin_key_input(index, code, modifiers));
        }

        PluginKeyRoute::Main
    }

    /// 将插件内容区内的鼠标事件转换为相对坐标，并在点击插件外区域时恢复主输入焦点。
    #[cfg(feature = "plugins")]
    pub(crate) fn route_plugin_mouse(&mut self, mouse: &MouseEvent) -> Option<UiInput> {
        if let Some(active) = self.view_stack.active() {
            if !point_in_rect(mouse.column, mouse.row, active.area) {
                return None;
            }
            return Some(UiInput {
                plugin_id: active.plugin_id.clone(),
                view_id: active.view.view_id.clone(),
                instance_id: Some(active.view.instance_id.clone()),
                event: UiInputEvent::Mouse {
                    kind: plugin_mouse_kind(mouse.kind),
                    x: mouse.column.saturating_sub(active.area.x),
                    y: mouse.row.saturating_sub(active.area.y),
                },
            });
        }
        let active_dialog = self.active_dialog_index();
        let target = active_dialog.or_else(|| {
            self.plugin_views
                .iter()
                .enumerate()
                .rev()
                .find(|(_, view)| {
                    plugin_view_visible(view)
                        && point_in_rect(mouse.column, mouse.row, plugin_view_hit_area(view))
                })
                .map(|(index, _)| index)
        });
        let Some(target) = target else {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.plugin_focus = None;
            }
            return None;
        };
        let view = &self.plugin_views[target];
        if !point_in_rect(mouse.column, mouse.row, plugin_view_hit_area(view)) {
            return None;
        }
        if active_dialog.is_none() && matches!(mouse.kind, MouseEventKind::Down(_)) {
            self.plugin_focus = view.declaration.focusable.then_some(target);
        }
        Some(UiInput {
            plugin_id: view.declaration.plugin_id.clone(),
            view_id: view.declaration.view_id.clone(),
            instance_id: None,
            event: UiInputEvent::Mouse {
                kind: plugin_mouse_kind(mouse.kind),
                x: mouse.column.saturating_sub(view.area.x),
                y: mouse.row.saturating_sub(view.area.y),
            },
        })
    }

    /// 生成当前焦点视图可识别的宿主无关键盘事件。
    #[cfg(feature = "plugins")]
    pub(crate) fn plugin_key_input(
        &self,
        index: usize,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> UiInput {
        let declaration = &self.plugin_views[index].declaration;
        UiInput {
            plugin_id: declaration.plugin_id.clone(),
            view_id: declaration.view_id.clone(),
            instance_id: None,
            event: UiInputEvent::Key {
                code: plugin_key_code(code),
                modifiers: plugin_key_modifiers(modifiers),
            },
        }
    }

    /// 在主输入区和所有可见、可聚焦的停靠视图之间循环焦点。
    #[cfg(feature = "plugins")]
    pub(crate) fn cycle_plugin_focus(&mut self, reverse: bool) {
        let focusable: Vec<usize> = self
            .plugin_views
            .iter()
            .enumerate()
            .filter(|(_, view)| {
                view.declaration.focusable
                    && view.declaration.placement != UiPlacement::Dialog
                    && view.declaration.placement != UiPlacement::Input
                    && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
            .collect();
        if focusable.is_empty() {
            self.plugin_focus = None;
            return;
        }

        self.plugin_focus = match (self.plugin_focus, reverse) {
            (None, false) => focusable.first().copied(),
            (None, true) => focusable.last().copied(),
            (Some(current), false) => focusable
                .iter()
                .position(|index| *index == current)
                .and_then(|position| focusable.get(position + 1).copied()),
            (Some(current), true) => focusable
                .iter()
                .position(|index| *index == current)
                .and_then(|position| position.checked_sub(1))
                .and_then(|position| focusable.get(position).copied()),
        };
    }

    /// 返回最后声明且当前可见的模态对话框索引。
    #[cfg(feature = "plugins")]
    pub(crate) fn active_dialog_index(&self) -> Option<usize> {
        self.plugin_views
            .iter()
            .enumerate()
            .rev()
            .find(|(_, view)| {
                view.declaration.placement == UiPlacement::Dialog && plugin_view_visible(view)
            })
            .map(|(index, _)| index)
    }

    /// 为每个插件视图构造下一次异步渲染请求。
    #[cfg(feature = "plugins")]
    pub(crate) fn plugin_render_requests(&mut self) -> Vec<UiRenderRequest> {
        self.plugin_frame = self.plugin_frame.wrapping_add(1);
        let active_dialog = self.active_dialog_index();
        let mut requests = self
            .plugin_views
            .iter()
            .enumerate()
            .filter(|(_, view)| view.declaration.placement != UiPlacement::Subview)
            .map(|(index, view)| UiRenderRequest {
                plugin_id: view.declaration.plugin_id.clone(),
                view_id: view.declaration.view_id.clone(),
                instance_id: None,
                width: if view.area.width == 0 {
                    view.declaration
                        .size
                        .width
                        .unwrap_or(default_plugin_width(view.declaration.placement))
                } else {
                    view.area.width
                },
                height: if view.area.height == 0 {
                    view.declaration
                        .size
                        .height
                        .unwrap_or(default_plugin_height(view.declaration.placement))
                } else {
                    view.area.height
                },
                focused: active_dialog == Some(index) || self.plugin_focus == Some(index),
                frame: self.plugin_frame,
            })
            .collect::<Vec<_>>();
        if let Some(active) = self
            .view_stack
            .active()
            .filter(|active| !active.area.is_empty())
        {
            requests.push(UiRenderRequest {
                plugin_id: active.plugin_id.clone(),
                view_id: active.view.view_id.clone(),
                instance_id: Some(active.view.instance_id.clone()),
                width: active.area.width.max(1),
                height: active.area.height.max(1),
                focused: true,
                frame: self.plugin_frame,
            });
        }
        requests
    }

    /// 构造周期刷新请求，并跳过触发前缀未激活的输入面板。
    ///
    /// 输入面板只随主输入快照与手势的定向刷新更新，周期渲染它没有意义。
    #[cfg(feature = "plugins")]
    pub(crate) fn periodic_plugin_render_requests(&mut self) -> Vec<UiRenderRequest> {
        let inactive_panels = self
            .plugin_views
            .iter()
            .enumerate()
            .filter(|(index, view)| {
                view.declaration.placement == UiPlacement::InputPanel
                    && self.active_trigger_view() != Some(*index)
            })
            .map(|(_, view)| {
                (
                    view.declaration.plugin_id.clone(),
                    view.declaration.view_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut requests = self.plugin_render_requests();
        requests.retain(|request| {
            !inactive_panels.iter().any(|(plugin_id, view_id)| {
                &request.plugin_id == plugin_id && &request.view_id == view_id
            })
        });
        requests
    }

    /// 用插件返回的新帧更新对应视图缓存。
    #[cfg(feature = "plugins")]
    pub(crate) fn update_plugin_frame(
        &mut self,
        plugin_id: &str,
        instance_id: Option<&str>,
        frame: PluginUiFrame,
    ) {
        if let Some(instance_id) = instance_id {
            if let Some(active) = self.view_stack.active_mut() {
                if active.plugin_id == plugin_id
                    && active.view.view_id == frame.view_id
                    && active.view.instance_id == instance_id
                {
                    active.frame = Some(frame);
                }
            }
            return;
        }
        if let Some(view) = self.plugin_views.iter_mut().find(|view| {
            view.declaration.plugin_id == plugin_id && view.declaration.view_id == frame.view_id
        }) {
            view.frame = Some(frame);
        }
        if self
            .plugin_focus
            .is_some_and(|index| !plugin_view_visible(&self.plugin_views[index]))
        {
            self.plugin_focus = None;
        }
    }

    /// 按独占 Input 视图可见性冻结或恢复主事件区。
    ///
    /// 首次显示时固定正文、思考与工具消息的完整渲染状态；周期刷新不会覆盖已有快照。
    /// 视图隐藏后只清除快照，后台已经完成的事件会在下一帧一次性显示。
    #[cfg(feature = "plugins")]
    pub(crate) fn sync_main_event_snapshot(&mut self) {
        if self.visible_plugin_input().is_some() {
            if self.main_event_snapshot.is_none() {
                self.main_event_snapshot = Some(MainEventSnapshot {
                    messages: self.messages.clone(),
                    streaming_message: self.streaming_message,
                    model_thinking: self.model_thinking.clone(),
                    model_thinking_index: self.model_thinking_index,
                });
            }
        } else {
            self.main_event_snapshot = None;
        }
    }

    /// 将单个插件的运行时 UI 错误限制在对应视图内展示。
    #[cfg(feature = "plugins")]
    pub(crate) fn set_plugin_ui_error(
        &mut self,
        plugin_id: &str,
        view_id: &str,
        instance_id: Option<&str>,
        error: &anyhow::Error,
    ) {
        self.update_plugin_frame(
            plugin_id,
            instance_id,
            PluginUiFrame {
                view_id: view_id.to_string(),
                visible: true,
                lines: vec![UiLine {
                    spans: vec![UiSpan {
                        text: format!("Plugin UI error: {error:#}"),
                        style: UiStyle {
                            foreground: Some(UiColor::Red),
                            ..UiStyle::default()
                        },
                    }],
                }],
                cursor: None,
            },
        );
    }
}

/// 判断插件视图是否已经返回可见帧。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_view_visible(view: &PluginViewState) -> bool {
    view.frame.as_ref().is_some_and(|frame| frame.visible)
}

/// 判断终端坐标是否位于给定矩形内。
#[cfg(feature = "plugins")]
pub(crate) fn point_in_rect(x: u16, y: u16, area: Rect) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

/// 返回鼠标可命中的插件区域；侧栏额外包含边框和内边距，点击留白也能聚焦。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_view_hit_area(view: &PluginViewState) -> Rect {
    if view.area.is_empty()
        || !matches!(
            view.declaration.placement,
            UiPlacement::Left | UiPlacement::Right
        )
    {
        return view.area;
    }
    Rect::new(
        view.area.x.saturating_sub(2),
        view.area.y.saturating_sub(2),
        view.area.width.saturating_add(4),
        view.area.height.saturating_add(4),
    )
}

/// 将 Crossterm 鼠标动作转换为稳定的字符串名称。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_mouse_kind(kind: MouseEventKind) -> String {
    match kind {
        MouseEventKind::Down(button) => format!("down_{button:?}").to_ascii_lowercase(),
        MouseEventKind::Up(button) => format!("up_{button:?}").to_ascii_lowercase(),
        MouseEventKind::Drag(button) => format!("drag_{button:?}").to_ascii_lowercase(),
        MouseEventKind::Moved => "moved".into(),
        MouseEventKind::ScrollDown => "scroll_down".into(),
        MouseEventKind::ScrollUp => "scroll_up".into(),
        MouseEventKind::ScrollLeft => "scroll_left".into(),
        MouseEventKind::ScrollRight => "scroll_right".into(),
    }
}

/// 将 Crossterm 按键转换为稳定、可跨语言处理的名称。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_key_code(code: KeyCode) -> String {
    match code {
        KeyCode::Char(character) => character.to_string(),
        KeyCode::F(number) => format!("f{number}"),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "back_tab".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Esc => "escape".into(),
        other => format!("{other:?}").to_ascii_lowercase(),
    }
}

/// 按固定顺序输出按键修饰符，避免插件依赖 Crossterm 位标记。
#[cfg(feature = "plugins")]
pub(crate) fn plugin_key_modifiers(modifiers: KeyModifiers) -> Vec<String> {
    [
        (KeyModifiers::CONTROL, "control"),
        (KeyModifiers::ALT, "alt"),
        (KeyModifiers::SHIFT, "shift"),
        (KeyModifiers::SUPER, "super"),
        (KeyModifiers::HYPER, "hyper"),
        (KeyModifiers::META, "meta"),
    ]
    .into_iter()
    .filter(|(modifier, _)| modifiers.contains(*modifier))
    .map(|(_, name)| name.to_string())
    .collect()
}

/// 返回未实际布局前使用的插件视图默认宽度。
#[cfg(feature = "plugins")]
pub(crate) fn default_plugin_width(placement: UiPlacement) -> u16 {
    match placement {
        UiPlacement::Left | UiPlacement::Right => 28,
        UiPlacement::Dialog | UiPlacement::Subview => 60,
        UiPlacement::Input | UiPlacement::InputPanel | UiPlacement::ComposerShelf => 40,
        UiPlacement::Top | UiPlacement::Bottom => 40,
    }
}

/// 返回未实际布局前使用的插件视图默认高度。
#[cfg(feature = "plugins")]
pub(crate) fn default_plugin_height(placement: UiPlacement) -> u16 {
    match placement {
        UiPlacement::Top | UiPlacement::Bottom => 6,
        UiPlacement::Dialog | UiPlacement::Subview => 20,
        UiPlacement::Input => 3,
        UiPlacement::InputPanel => 8,
        UiPlacement::ComposerShelf => 4,
        UiPlacement::Left | UiPlacement::Right => 10,
    }
}

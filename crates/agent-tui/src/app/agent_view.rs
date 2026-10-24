//! 宿主原生 Agent 会话视图的状态机：Runtime 事件、输入编辑与提交。
//!
//! 视图只承载**通用 Agent 语义**：显示目标 Agent 的事件时间线、编辑一条待提交正文、
//! 提交后交给发起插件处理。成员目录、续跑与配额回收仍由插件负责，TUI 不复制这些规则。

use super::{App, ToolMessageHitRegion};
use crate::event::UiEvent;
use crate::session::conversation::{Msg, MsgKind};
use crate::ui::transcript_layout::TranscriptLayout;
use agent_core::{AgentEvent, AgentEventKind};
use agent_plugin_host::PluginHost;
use agent_runtime::{AgentEventStream, AgentId, AgentRuntimeApi, AgentStatus};
use agent_tool::{ToolCall, ToolOutputDelta, ToolResult};
use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::prelude::*;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

/// 单次事件批量的上限：批量既降低通道事件率，也让一帧内的重排次数保持有界。
pub(crate) const AGENT_VIEW_EVENT_BATCH: usize = 256;
/// 收到首条事件后等待新事件的短暂窗口；把同一段网络突发合并进一次重排。
///
/// 流式正文每多一个增量都会让助手消息的指纹变化，从而重排整段文本；按 token 逐个
/// 重排会让长回答的开销随长度平方增长。合并窗口让屏幕节奏只跟帧率走。
const AGENT_VIEW_COALESCE_MS: u64 = 12;

/// 宿主原生 Agent 会话视图的完整界面状态。
///
/// 一个实例对应导航栈中的一个成员实例；成员续跑后 `target` 换成新句柄，实例 ID、
/// 已显示的时间线和输入草稿都保留，因此上下文中断不会让屏幕上的历史消失。
pub(crate) struct AgentSessionView {
    /// 发起导航的可信插件 ID。
    pub(crate) plugin_id: String,
    /// 插件内稳定唯一的成员实例 ID。
    pub(crate) instance_id: String,
    /// 当前订阅的 Runtime Agent 句柄。
    pub(crate) target: AgentId,
    /// 与插件 principal 绑定的 Runtime API，只能访问该插件名下的 Agent。
    api: Arc<dyn AgentRuntimeApi>,
    /// 已显示的时间线，复用主界面的消息与工具组件。
    pub(crate) messages: Vec<Msg>,
    /// 正在接收增量文本的助手消息索引。
    pub(crate) streaming_message: Option<usize>,
    /// 待提交正文与 UTF-8 字节光标。
    pub(crate) input: String,
    pub(crate) cursor: usize,
    /// 手动滚动偏移；`None` 表示贴底跟随。
    pub(crate) scroll: Option<u16>,
    /// 可见窗口的消息布局缓存。
    pub(crate) layout: TranscriptLayout,
    /// 最近一帧可点击的工具消息热区。
    pub(crate) hit_regions: Vec<ToolMessageHitRegion>,
    /// 最近一帧的最大滚动偏移。
    pub(crate) last_max_scroll: u16,
    /// 最近一帧的消息区高度。
    pub(crate) last_viewport: u16,
    /// 正文区在屏幕上的矩形，供滚轮与工具点击换算。
    pub(crate) content_area: Option<Rect>,
    /// 本帧实际滚动偏移。
    pub(crate) content_scroll: u16,
    /// 逻辑总行数。
    pub(crate) content_rows: u16,
    /// 滚动条热区。
    pub(crate) scrollbar_area: Option<Rect>,
    /// 滚动条是否悬停。
    pub(crate) is_scrollbar_hovered: bool,
    /// Runtime 侧的当前状态。
    pub(crate) status: Option<AgentStatus>,
    /// 状态最近一次变化的时间，用于显示运行耗时。
    pub(crate) status_since: Instant,
    /// 最近一次由插件提供的说明；重复帧不会重复追加。
    last_notice: Option<String>,
    /// 事件流是否已经结束（成员终态或被回收）。
    pub(crate) closed: bool,
    /// 当前事件订阅任务；替换目标或视图关闭时中止。
    subscription: Option<AgentViewSubscription>,
}

impl Drop for AgentSessionView {
    /// 视图销毁时中止后台订阅，避免通道与任务脱离界面生命周期继续运行。
    fn drop(&mut self) {
        self.abort_subscription();
    }
}

/// 单个视图的事件订阅任务。
struct AgentViewSubscription {
    /// 订阅建立时的目标句柄；目标变化后旧任务必须中止。
    target: AgentId,
    /// 后台转发任务句柄。
    task: tokio::task::JoinHandle<()>,
}

/// 原生 Agent 会话视图处理一次按键后的宿主动作。
pub(crate) enum AgentViewKeyOutcome {
    /// 视图已消费该按键。
    Consumed,
    /// 用户要求返回上一层。
    Exit,
    /// 用户提交了一条正文，宿主转发给发起插件。
    Submit {
        plugin_id: String,
        view_id: String,
        instance_id: String,
        text: String,
    },
}

/// 从导航栈条目派生出的原生视图标识。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentSessionRoute {
    /// 发起插件的可信 ID。
    pub(crate) plugin_id: String,
    /// 插件声明的子视图 ID，用于把提交输入路由回插件。
    pub(crate) view_id: String,
    /// 插件内成员实例 ID。
    pub(crate) instance_id: String,
    /// 当前目标 Agent 句柄。
    pub(crate) target: AgentId,
    /// 插件随导航提供的说明。
    pub(crate) notice: Option<String>,
}

impl AgentSessionView {
    /// 为一个可信目标句柄创建空白视图。
    ///
    /// `api` 来自发起插件的可信 principal；本函数不做权限判断，越权访问会在下一次
    /// Runtime 调用时被拒绝并显示为视图错误。
    pub(crate) fn new(
        plugin_id: String,
        instance_id: String,
        target: AgentId,
        api: Arc<dyn AgentRuntimeApi>,
        prompt: Option<String>,
        notice: Option<String>,
    ) -> Self {
        let mut view = Self {
            plugin_id,
            instance_id,
            target,
            api,
            messages: Vec::new(),
            streaming_message: None,
            input: String::new(),
            cursor: 0,
            scroll: None,
            layout: TranscriptLayout::default(),
            hit_regions: Vec::new(),
            last_max_scroll: 0,
            last_viewport: 0,
            content_area: None,
            content_scroll: 0,
            content_rows: 0,
            scrollbar_area: None,
            is_scrollbar_hovered: false,
            status: None,
            status_since: Instant::now(),
            last_notice: None,
            closed: false,
            subscription: None,
        };
        // 首个任务正文不在事件流里：先作为用户消息落到时间线，事件回放排在其后。
        if let Some(prompt) = prompt.filter(|prompt| !prompt.trim().is_empty()) {
            view.record_user_message(prompt);
        }
        view.apply_notice(notice);
        view
    }

    /// 返回订阅使用的 Runtime API。
    pub(crate) fn api(&self) -> Arc<dyn AgentRuntimeApi> {
        Arc::clone(&self.api)
    }

    /// 切换目标句柄并中止旧订阅；时间线与输入草稿保持不变。
    pub(crate) fn replace_target(&mut self, target: AgentId) {
        if self.target == target {
            return;
        }
        self.target = target;
        self.closed = false;
        self.status = None;
        self.status_since = Instant::now();
        self.abort_subscription();
    }

    /// 中止当前事件订阅。
    pub(crate) fn abort_subscription(&mut self) {
        if let Some(subscription) = self.subscription.take() {
            subscription.task.abort();
        }
    }

    /// 记录订阅任务，替换同一目标的旧任务。
    pub(crate) fn set_subscription(&mut self, target: AgentId, task: tokio::task::JoinHandle<()>) {
        self.abort_subscription();
        self.subscription = Some(AgentViewSubscription { target, task });
    }

    /// 返回是否已经为当前目标建立订阅。
    pub(crate) fn subscribed_for(&self, target: &AgentId) -> bool {
        self.subscription
            .as_ref()
            .is_some_and(|subscription| &subscription.target == target)
    }

    /// 记录首帧状态，避免渲染前把运行中的成员显示成未知。
    pub(crate) fn set_status(&mut self, status: AgentStatus) {
        if self.status != Some(status) {
            self.status = Some(status);
            self.status_since = Instant::now();
        }
    }

    /// 把事件追加进时间线；未知事件类型直接忽略，保持前向兼容。
    pub(crate) fn apply_event(&mut self, event: &AgentEvent) {
        match event.kind {
            AgentEventKind::ModelRequest => {
                // 新一轮模型请求：上一段助手正文已经结束，后续增量写入新消息。
                self.streaming_message = None;
            }
            AgentEventKind::ModelTextDelta => {
                if let Some(delta) = event.payload.get("delta").and_then(Value::as_str) {
                    self.append_assistant(delta);
                }
            }
            AgentEventKind::ToolStarted => {
                if let Ok(call) = serde_json::from_value::<ToolCall>(event.payload.clone()) {
                    self.streaming_message = None;
                    self.messages.push(Msg::tool_started(call));
                }
            }
            AgentEventKind::ToolOutputDelta => {
                if let Ok(delta) = serde_json::from_value::<ToolOutputDelta>(event.payload.clone())
                {
                    if let Some(message) = self
                        .messages
                        .iter_mut()
                        .rev()
                        .find(|message| message.tool_call_id() == Some(delta.call_id.as_str()))
                    {
                        message.append_tool_output(&delta.delta);
                    }
                }
            }
            AgentEventKind::ToolFinished => {
                if let Ok(result) = serde_json::from_value::<ToolResult>(event.payload.clone()) {
                    self.finish_tool(result);
                }
            }
            AgentEventKind::ToolSkipped => {
                let call = event
                    .payload
                    .get("call")
                    .cloned()
                    .and_then(|value| serde_json::from_value::<ToolCall>(value).ok());
                let reason = event
                    .payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("工具未执行")
                    .to_string();
                if let Some(call) = call {
                    self.streaming_message = None;
                    let mut message = Msg::tool_started(call);
                    message.kind = MsgKind::ToolSkipped;
                    message.skip_reason = Some(reason);
                    self.messages.push(message);
                }
            }
            AgentEventKind::RunStarted => self.set_status(AgentStatus::Running),
            AgentEventKind::RunFinished => {
                self.streaming_message = None;
                let cancelled = event
                    .payload
                    .get("cancelled")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.set_status(if cancelled {
                    AgentStatus::Cancelled
                } else {
                    AgentStatus::Succeeded
                });
            }
            _ => {}
        }
    }

    /// 追加一段助手正文；需要时创建新的助手消息。
    fn append_assistant(&mut self, delta: &str) {
        if delta.is_empty() {
            return;
        }
        let index = self.streaming_message.unwrap_or_else(|| {
            self.messages
                .push(Msg::new(MsgKind::Assistant, String::new()));
            let index = self.messages.len() - 1;
            self.streaming_message = Some(index);
            index
        });
        if let Some(message) = self.messages.get_mut(index) {
            message.text.push_str(delta);
        }
    }

    /// 用终态结果更新运行中的工具消息，缺失调用项时补一条完整结果。
    fn finish_tool(&mut self, result: ToolResult) {
        let kind = if result.is_error {
            MsgKind::ToolError
        } else {
            MsgKind::ToolOk
        };
        let call_id = result.call_id.clone();
        if let Some(message) = self.messages.iter_mut().rev().find(|message| {
            matches!(message.kind, MsgKind::ToolRunning)
                && message.tool_call_id() == Some(call_id.as_str())
        }) {
            message.kind = kind;
            message.tool_result = Some(result);
        } else {
            self.messages.push(Msg::tool_finished(result));
        }
    }

    /// 追加一条本地用户消息，表示用户已提交给该 Agent。
    pub(crate) fn record_user_message(&mut self, text: String) {
        self.streaming_message = None;
        self.messages.push(Msg::new(MsgKind::User, text));
        self.scroll = None;
    }

    /// 记录插件随导航提供的说明；内容相同时不重复提示。
    pub(crate) fn apply_notice(&mut self, notice: Option<String>) {
        let Some(notice) = notice.filter(|notice| !notice.trim().is_empty()) else {
            return;
        };
        if self.last_notice.as_deref() == Some(notice.as_str()) {
            return;
        }
        self.messages.push(Msg::new(MsgKind::Error, notice.clone()));
        self.last_notice = Some(notice);
    }

    /// 追加一条仅用于本视图的状态说明。
    pub(crate) fn push_info(&mut self, text: impl Into<String>) {
        self.messages.push(Msg::new(MsgKind::Info, text));
    }

    /// 标记事件流结束，并把终态记入时间线一次。
    pub(crate) fn mark_closed(&mut self, status: Option<AgentStatus>, error: Option<String>) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.streaming_message = None;
        if let Some(status) = status {
            self.set_status(status);
        }
        if let Some(error) = error {
            self.messages.push(Msg::new(MsgKind::Error, error));
        }
    }

    /// 按行调整回看位置；正值向更早内容回看。
    pub(crate) fn scroll_by(&mut self, lines: i32) {
        let current = i32::from(self.scroll.unwrap_or(self.last_max_scroll));
        let next = (current + lines).clamp(0, i32::from(self.last_max_scroll));
        self.scroll = Some(u16::try_from(next).unwrap_or(0));
        if next >= i32::from(self.last_max_scroll) {
            self.scroll = None;
        }
    }

    /// 跳到内容顶部。
    pub(crate) fn scroll_to_top(&mut self) {
        self.scroll = Some(0);
    }

    /// 回到贴底跟随。
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.scroll = None;
    }

    /// 返回输入正文是否有可提交内容。
    pub(crate) fn has_input(&self) -> bool {
        !self.input.trim().is_empty()
    }
}

impl App {
    /// 返回当前激活子视图对应的原生 Agent 会话路由。
    pub(crate) fn active_agent_session_route(&self) -> Option<AgentSessionRoute> {
        let active = self.view_stack.active()?;
        let target = active.agent_target()?;
        Some(AgentSessionRoute {
            plugin_id: active.plugin_id.clone(),
            view_id: active.body.agent_view_id()?.to_string(),
            instance_id: active.body.route_key().to_string(),
            target: target.clone(),
            notice: active.body.notice().map(str::to_string),
        })
    }

    /// 返回导航栈中全部原生 Agent 会话路由，供订阅同步使用。
    pub(crate) fn agent_session_routes(&self) -> Vec<AgentSessionRoute> {
        self.view_stack
            .agent_sessions()
            .into_iter()
            .map(|view| AgentSessionRoute {
                plugin_id: view.plugin_id.to_string(),
                view_id: view.view_id.to_string(),
                instance_id: view.instance_id.to_string(),
                target: view.target.clone(),
                notice: view.notice.map(str::to_string),
            })
            .collect()
    }

    /// 返回当前激活的原生会话视图状态。
    ///
    /// 按成员实例 ID 而不是目标句柄匹配：续跑换句柄后、下一次订阅同步完成前，
    /// 界面继续显示已有时间线，不会闪回占位页。
    pub(crate) fn active_agent_session_view(&self) -> Option<&AgentSessionView> {
        let active = self.view_stack.active()?;
        active.agent_target()?;
        self.agent_views.iter().find(|view| {
            view.plugin_id == active.plugin_id && view.instance_id == active.body.route_key()
        })
    }

    /// 返回当前激活的原生会话视图状态（可变）。
    pub(crate) fn active_agent_session_view_mut(&mut self) -> Option<&mut AgentSessionView> {
        let (plugin_id, instance_id) = {
            let active = self.view_stack.active()?;
            active.agent_target()?;
            (
                active.plugin_id.clone(),
                active.body.route_key().to_string(),
            )
        };
        self.agent_view_mut(&plugin_id, &instance_id)
    }

    /// 返回指定原生会话视图携带的任务正文；只在创建视图时读取，避免每帧复制长 prompt。
    fn agent_session_prompt(&self, plugin_id: &str, instance_id: &str) -> Option<String> {
        self.view_stack
            .agent_sessions()
            .into_iter()
            .find(|view| view.plugin_id == plugin_id && view.instance_id == instance_id)
            .and_then(|view| view.prompt.map(str::to_string))
    }

    /// 返回指定成员实例的视图状态（可变）。
    fn agent_view_mut(
        &mut self,
        plugin_id: &str,
        instance_id: &str,
    ) -> Option<&mut AgentSessionView> {
        self.agent_views
            .iter_mut()
            .find(|view| view.plugin_id == plugin_id && view.instance_id == instance_id)
    }

    /// 同步原生会话视图与其事件订阅。
    ///
    /// 只为导航栈中的实例保留状态：成员被重新打开时沿用实例 ID，续跑只替换目标句柄。
    /// 订阅失败会把原因作为视图错误显示，不会静默空白。
    pub(crate) async fn sync_agent_views(&mut self, host: &agent_plugin_host::LivePluginHost) {
        self.sync_agent_views_with(|plugin_id| host.agent_runtime_api(plugin_id))
            .await;
    }

    /// 用给定解析器同步原生会话视图与订阅；解析器返回插件 principal 的 Runtime API。
    ///
    /// 独立出来是为了让单元测试可以注入固定 API，不必加载真实 WASM 插件。
    pub(crate) async fn sync_agent_views_with(
        &mut self,
        resolve: impl Fn(&str) -> Option<Arc<dyn AgentRuntimeApi>>,
    ) {
        let routes = self.agent_session_routes();
        self.agent_views.retain(|view| {
            routes.iter().any(|route| {
                route.plugin_id == view.plugin_id && route.instance_id == view.instance_id
            })
        });
        for route in routes {
            let existing = self.agent_views.iter().position(|view| {
                view.plugin_id == route.plugin_id && view.instance_id == route.instance_id
            });
            if let Some(index) = existing {
                let view = &mut self.agent_views[index];
                if view.target != route.target {
                    view.replace_target(route.target.clone());
                }
                view.apply_notice(route.notice.clone());
                // 事件流已经自然结束（成员终态或被回收）时不自动重订阅：旧句柄不会
                // 再产生事件，反复订阅只会把同一结论重复写进时间线。
                if view.closed || view.subscribed_for(&route.target) {
                    continue;
                }
                let api = view.api();
                // 重新订阅前刷新一次状态：旧句柄的终态不能带进新订阅。
                if let Ok(snapshot) = api.status(&route.target).await {
                    view.set_status(snapshot.status);
                }
                match api.subscribe(&route.target).await {
                    Ok(stream) => {
                        let task = spawn_agent_view_reader(
                            self.tx.clone(),
                            route.plugin_id.clone(),
                            route.instance_id.clone(),
                            route.target.clone(),
                            stream,
                        );
                        view.set_subscription(route.target.clone(), task);
                        view.closed = false;
                    }
                    Err(error) => {
                        view.push_info(format!("无法订阅 Agent 事件：{error}"));
                        view.closed = true;
                    }
                }
                continue;
            }

            let Some(api) = resolve(&route.plugin_id) else {
                self.note_agent_session(
                    &route.plugin_id,
                    &route.instance_id,
                    "该插件未声明 Agent Runtime 能力，无法显示会话".to_string(),
                );
                continue;
            };
            let prompt = self.agent_session_prompt(&route.plugin_id, &route.instance_id);
            let mut view = AgentSessionView::new(
                route.plugin_id.clone(),
                route.instance_id.clone(),
                route.target.clone(),
                Arc::clone(&api),
                prompt,
                route.notice.clone(),
            );
            match api.status(&route.target).await {
                Ok(snapshot) => view.set_status(snapshot.status),
                Err(error) => {
                    view.push_info(format!("无法读取 Agent 状态：{error}"));
                    view.closed = true;
                    self.agent_views.push(view);
                    continue;
                }
            }
            match api.subscribe(&route.target).await {
                Ok(stream) => {
                    let task = spawn_agent_view_reader(
                        self.tx.clone(),
                        route.plugin_id.clone(),
                        route.instance_id.clone(),
                        route.target.clone(),
                        stream,
                    );
                    view.set_subscription(route.target.clone(), task);
                    self.agent_views.push(view);
                }
                Err(error) => {
                    view.push_info(format!("无法订阅 Agent 事件：{error}"));
                    view.closed = true;
                    self.agent_views.push(view);
                }
            }
        }
    }

    /// 把宿主侧的失败原因写回对应原生子视图的说明，由渲染层原样展示。
    fn note_agent_session(&mut self, plugin_id: &str, instance_id: &str, message: String) {
        self.view_stack
            .set_agent_session_notice(plugin_id, instance_id, Some(message));
    }

    /// 应用一批 Runtime 事件，并刷新目标状态。
    pub(crate) async fn apply_agent_view_events(
        &mut self,
        plugin_id: &str,
        instance_id: &str,
        target: &AgentId,
        events: Vec<AgentEvent>,
    ) {
        let Some(view) = self.agent_view_mut(plugin_id, instance_id) else {
            return;
        };
        if &view.target != target {
            return;
        }
        for event in &events {
            view.apply_event(event);
        }
        if let Ok(snapshot) = view.api().status(target).await {
            view.set_status(snapshot.status);
        }
    }

    /// 处理事件流结束：读取终态与失败原因，并把结论留在时间线里。
    pub(crate) async fn apply_agent_view_closed(
        &mut self,
        plugin_id: &str,
        instance_id: &str,
        target: &AgentId,
    ) {
        let Some(view) = self.agent_view_mut(plugin_id, instance_id) else {
            return;
        };
        if &view.target != target {
            return;
        }
        let api = view.api();
        let status = api
            .status(target)
            .await
            .ok()
            .map(|snapshot| snapshot.status);
        let error = match api.result(target).await {
            Ok(Some(agent_runtime::AgentOutcome::Failed { error })) => Some(error),
            _ => None,
        };
        view.mark_closed(status, error);
    }

    /// 处理原生会话视图的一次按键。
    pub(crate) fn handle_agent_session_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> AgentViewKeyOutcome {
        // 返回上一层不依赖视图状态是否已建立：连接中或订阅失败的视图同样可以退出。
        if matches!(code, KeyCode::Esc) {
            self.view_stack.pop_for_user();
            return AgentViewKeyOutcome::Exit;
        }
        let Some(route) = self.active_agent_session_route() else {
            return AgentViewKeyOutcome::Consumed;
        };
        let Some(view) = self.active_agent_session_view_mut() else {
            // 状态尚未建立时吞掉其余按键，避免落到主输入框。
            return AgentViewKeyOutcome::Consumed;
        };
        // 多行编辑键与主输入框保持一致，避免用户在两处得到不同手感。
        if (matches!(code, KeyCode::Enter)
            && modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT))
            || (super::is_command_modifier(modifiers) && matches!(code, KeyCode::Char('j')))
        {
            view.insert_text("\n");
            return AgentViewKeyOutcome::Consumed;
        }
        match code {
            KeyCode::Enter => {
                if !view.has_input() {
                    return AgentViewKeyOutcome::Consumed;
                }
                let text = view.input.trim_end().to_string();
                view.input.clear();
                view.cursor = 0;
                view.record_user_message(text.clone());
                return AgentViewKeyOutcome::Submit {
                    plugin_id: route.plugin_id,
                    view_id: route.view_id,
                    instance_id: route.instance_id,
                    text,
                };
            }
            KeyCode::Char('c') | KeyCode::Char('C') if super::is_command_modifier(modifiers) => {
                // Ctrl+C 在原生视图里不退出应用：优先清空草稿，空草稿时才放弃输入焦点。
                if view.has_input() {
                    view.input.clear();
                    view.cursor = 0;
                    return AgentViewKeyOutcome::Consumed;
                }
            }
            KeyCode::Char('a') if super::is_command_modifier(modifiers) => {
                view.cursor = view.current_line_start();
                return AgentViewKeyOutcome::Consumed;
            }
            KeyCode::Char('e') if super::is_command_modifier(modifiers) => {
                view.cursor = view.current_line_end();
                return AgentViewKeyOutcome::Consumed;
            }
            KeyCode::Char('w') if super::is_command_modifier(modifiers) => {
                view.delete_previous_word();
                return AgentViewKeyOutcome::Consumed;
            }
            KeyCode::Char('u') if super::is_command_modifier(modifiers) => {
                let start = view.current_line_start();
                view.input.replace_range(start..view.cursor, "");
                view.cursor = start;
                return AgentViewKeyOutcome::Consumed;
            }
            KeyCode::Char(character) if !super::is_command_modifier(modifiers) => {
                // 只接收无命令修饰键的可打印字符；Alt/Option 组合留给终端自己处理。
                if !modifiers.contains(KeyModifiers::ALT) {
                    view.insert_text(&character.to_string());
                }
                return AgentViewKeyOutcome::Consumed;
            }
            KeyCode::Backspace => view.delete_previous_char(),
            KeyCode::Delete => view.delete_next_char(),
            KeyCode::Left => view.cursor = view.previous_boundary(),
            KeyCode::Right => view.cursor = view.next_boundary(),
            // 多行草稿优先按视觉行移动光标；单行时退化为滚动一屏的行数。
            KeyCode::Up => {
                if !(view.input.contains('\n')
                    && super::input::move_cursor_vertically(&view.input, &mut view.cursor, true))
                {
                    view.scroll_by(1);
                }
            }
            KeyCode::Down => {
                if !(view.input.contains('\n')
                    && super::input::move_cursor_vertically(&view.input, &mut view.cursor, false))
                {
                    view.scroll_by(-1);
                }
            }
            // 与主界面一致：Home/End 编辑光标，Ctrl+Home/End 回到内容首尾。
            KeyCode::Home if super::is_command_modifier(modifiers) => view.scroll_to_top(),
            KeyCode::End if super::is_command_modifier(modifiers) => view.scroll_to_bottom(),
            KeyCode::Home => view.cursor = view.current_line_start(),
            KeyCode::End => view.cursor = view.current_line_end(),
            KeyCode::PageUp => view.scroll_by(i32::from(view.last_viewport.max(1))),
            KeyCode::PageDown => view.scroll_by(-i32::from(view.last_viewport.max(1))),
            _ => {}
        }
        AgentViewKeyOutcome::Consumed
    }

    /// 处理原生会话视图内的鼠标事件：滚轮滚动与工具消息展开。
    pub(crate) fn handle_agent_session_mouse(&mut self, mouse: &MouseEvent) {
        let Some(view) = self.active_agent_session_view_mut() else {
            return;
        };
        match mouse.kind {
            MouseEventKind::ScrollUp => view.scroll_by(3),
            MouseEventKind::ScrollDown => view.scroll_by(-3),
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = view.hit_regions.iter().find(|region| {
                    mouse.column >= region.area.x
                        && mouse.column < region.area.right()
                        && mouse.row >= region.area.y
                        && mouse.row < region.area.bottom()
                });
                if let Some(region) = hit {
                    if let Some(message) = view.messages.get_mut(region.message_index) {
                        message.tool_expanded = !message.tool_expanded;
                        // 展开会改变高度：滚动位置回到贴底，避免停留在中间内容上。
                        view.scroll = None;
                    }
                }
            }
            _ => {}
        }
    }

    /// 把粘贴内容插入原生会话输入；与主输入框一样保留换行。
    pub(crate) fn handle_agent_session_paste(&mut self, text: &str) {
        let Some(view) = self.active_agent_session_view_mut() else {
            return;
        };
        view.insert_text(text);
    }
}

impl AgentSessionView {
    /// 在光标处插入文本。
    fn insert_text(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    /// 删除光标前一个字符。
    fn delete_previous_char(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.previous_boundary();
        self.input.replace_range(previous..self.cursor, "");
        self.cursor = previous;
    }

    /// 删除光标前一个单词。
    fn delete_previous_word(&mut self) {
        let start = super::input::previous_word_boundary(&self.input, self.cursor);
        if start < self.cursor {
            self.input.replace_range(start..self.cursor, "");
            self.cursor = start;
        }
    }

    /// 删除光标后一个字符。
    fn delete_next_char(&mut self) {
        let next = self.next_boundary();
        if next > self.cursor {
            self.input.replace_range(self.cursor..next, "");
        }
    }

    /// 返回光标前一个字符边界。
    fn previous_boundary(&self) -> usize {
        self.input[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(0, |(index, _)| index)
    }

    /// 返回光标后一个字符边界。
    fn next_boundary(&self) -> usize {
        self.input[self.cursor..]
            .char_indices()
            .nth(1)
            .map_or(self.input.len(), |(index, _)| self.cursor + index)
    }

    /// 返回当前逻辑行行首。
    fn current_line_start(&self) -> usize {
        self.input[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    /// 返回当前逻辑行行尾。
    fn current_line_end(&self) -> usize {
        self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |index| self.cursor + index)
    }
}

/// 启动后台事件转发任务：批量发送，流结束或视图销毁时退出。
fn spawn_agent_view_reader(
    tx: tokio::sync::mpsc::UnboundedSender<UiEvent>,
    plugin_id: String,
    instance_id: String,
    target: AgentId,
    mut stream: AgentEventStream,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(first) = stream.next().await else {
                let _ = tx.send(UiEvent::AgentViewClosed {
                    plugin_id,
                    instance_id,
                    target,
                });
                return;
            };
            let mut events = Vec::with_capacity(16);
            events.push(first);
            // 先取走已经排队的事件，再给生产者一个很短的追赶窗口；两者都只影响一帧内
            // 的重排次数，不改变事件顺序。
            while events.len() < AGENT_VIEW_EVENT_BATCH {
                match stream.try_next() {
                    Some(event) => events.push(event),
                    None => break,
                }
            }
            if events.len() < AGENT_VIEW_EVENT_BATCH {
                tokio::time::sleep(std::time::Duration::from_millis(AGENT_VIEW_COALESCE_MS)).await;
                while events.len() < AGENT_VIEW_EVENT_BATCH {
                    match stream.try_next() {
                        Some(event) => events.push(event),
                        None => break,
                    }
                }
            }
            if tx
                .send(UiEvent::AgentViewEvents {
                    plugin_id: plugin_id.clone(),
                    instance_id: instance_id.clone(),
                    target: target.clone(),
                    events,
                })
                .is_err()
            {
                return;
            }
        }
    })
}

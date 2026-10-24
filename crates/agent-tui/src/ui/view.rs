//! 主视图与插件子视图之间的通用导航状态机。

use agent_plugin_host::ui::{
    UiDeclaration, UiFrame, UiNavigationAction, UiNavigationRequest, UiPlacement, UiViewInstance,
};
use agent_runtime::AgentId;
use anyhow::{anyhow, Result};
use ratatui::layout::Rect;
use std::collections::HashSet;
use std::str::FromStr;

/// 一个已进入导航栈的子视图内容。
///
/// 两种形态共享同一套导航语义（栈顺序、`Esc` 返回、面包屑），区别只在谁来渲染：
/// 插件声明式帧由插件自己绘制，宿主原生视图由 TUI 用主界面组件绘制。
#[derive(Debug, Clone)]
pub(crate) enum SubviewBody {
    /// 插件声明的声明式视图与最近一帧。
    Plugin {
        /// 插件提供的视图类型和动态实例身份。
        view: UiViewInstance,
        /// 最近一次成功渲染的声明式帧。
        frame: Option<UiFrame>,
    },
    /// 宿主原生渲染的可信 Agent 会话。
    AgentSession {
        /// 插件声明的子视图 ID，宿主用它把提交输入路由回发起插件。
        view_id: String,
        /// 插件内稳定唯一的实例 ID，续跑后保持不变。
        instance_id: String,
        /// 目标 Runtime Agent 句柄；成员续跑后指向新句柄。
        target: AgentId,
        /// 创建视图时展示的任务正文；已存在视图时忽略。
        prompt: Option<String>,
        /// 插件随导航提供的说明，例如上一次提交被拒绝的原因。
        notice: Option<String>,
    },
}

impl SubviewBody {
    /// 返回插件声明式视图实例；原生视图返回 `None`。
    pub(crate) fn view(&self) -> Option<&UiViewInstance> {
        match self {
            Self::Plugin { view, .. } => Some(view),
            Self::AgentSession { .. } => None,
        }
    }

    /// 返回最近一帧插件内容；原生视图返回 `None`。
    pub(crate) fn frame(&self) -> Option<&UiFrame> {
        match self {
            Self::Plugin { frame, .. } => frame.as_ref(),
            Self::AgentSession { .. } => None,
        }
    }

    /// 返回原生 Agent 会话目标；插件视图返回 `None`。
    pub(crate) fn agent_target(&self) -> Option<&AgentId> {
        match self {
            Self::Plugin { .. } => None,
            Self::AgentSession { target, .. } => Some(target),
        }
    }

    /// 返回插件随导航提供的说明；插件视图返回 `None`。
    pub(crate) fn notice(&self) -> Option<&str> {
        match self {
            Self::Plugin { .. } => None,
            Self::AgentSession { notice, .. } => notice.as_deref(),
        }
    }

    /// 返回原生会话输入路由使用的插件视图 ID；插件视图返回 `None`。
    pub(crate) fn agent_view_id(&self) -> Option<&str> {
        match self {
            Self::Plugin { .. } => None,
            Self::AgentSession { view_id, .. } => Some(view_id),
        }
    }

    /// 返回视图的稳定路由键：插件使用视图实例 ID，原生会话使用成员实例 ID。
    ///
    /// 插件接收输入、宿主渲染原生视图都靠它区分同一插件的多个实例；成员续跑后
    /// 目标句柄会变，但实例 ID 不变，视图因此不会被重复压栈。
    pub(crate) fn route_key(&self) -> &str {
        match self {
            Self::Plugin { view, .. } => &view.instance_id,
            Self::AgentSession { instance_id, .. } => instance_id,
        }
    }
}

/// 一个已进入导航栈的插件子视图。
#[derive(Debug, Clone)]
pub(crate) struct SubviewState {
    /// 由 Host 注入的可信插件 ID。
    pub(crate) plugin_id: String,
    /// 主应用显示的标题。
    pub(crate) title: String,
    /// 子视图内容形态。
    pub(crate) body: SubviewBody,
    /// 当前终端帧中去除标题后的内容区。
    pub(crate) area: Rect,
}

impl SubviewState {
    /// 返回插件声明式视图实例；原生视图返回 `None`。
    pub(crate) fn view(&self) -> Option<&UiViewInstance> {
        self.body.view()
    }

    /// 返回最近一帧插件内容；原生视图返回 `None`。
    pub(crate) fn frame(&self) -> Option<&UiFrame> {
        self.body.frame()
    }

    /// 返回原生 Agent 会话目标；插件视图返回 `None`。
    pub(crate) fn agent_target(&self) -> Option<&AgentId> {
        self.body.agent_target()
    }
}

/// 导航栈中一个原生 Agent 会话视图的只读投影。
#[derive(Debug, Clone, Copy)]
pub(crate) struct AgentSessionRef<'a> {
    /// 由 Host 注入的可信插件 ID。
    pub(crate) plugin_id: &'a str,
    /// 插件声明的子视图 ID，用于把提交输入路由回插件。
    pub(crate) view_id: &'a str,
    /// 插件内稳定唯一的成员实例 ID。
    pub(crate) instance_id: &'a str,
    /// 当前目标 Agent 句柄。
    pub(crate) target: &'a AgentId,
    /// 创建视图时展示的任务正文。
    pub(crate) prompt: Option<&'a str>,
    /// 插件随导航提供的说明。
    pub(crate) notice: Option<&'a str>,
}

/// 应用持有的视图导航栈，主视图作为永不移除的隐式根节点。
#[derive(Debug, Default)]
pub(crate) struct ViewStack {
    subviews: Vec<SubviewState>,
    handled_requests: HashSet<(String, String)>,
}

impl ViewStack {
    /// 返回当前激活的插件子视图；`None` 表示主视图。
    pub(crate) fn active(&self) -> Option<&SubviewState> {
        self.subviews.last()
    }

    /// 返回当前激活子视图的可变引用。
    pub(crate) fn active_mut(&mut self) -> Option<&mut SubviewState> {
        self.subviews.last_mut()
    }

    /// 返回当前是否处于主视图。
    pub(crate) fn is_main(&self) -> bool {
        self.subviews.is_empty()
    }

    /// 应用插件发布的幂等导航请求，返回视图栈是否发生变化。
    pub(crate) fn apply(
        &mut self,
        plugin_id: &str,
        request: UiNavigationRequest,
        declarations: &[UiDeclaration],
    ) -> Result<bool> {
        validate_token("request_id", &request.request_id)?;
        let request_key = (plugin_id.to_string(), request.request_id.clone());
        if self.handled_requests.contains(&request_key) {
            return Ok(false);
        }

        let changed = match request.action {
            UiNavigationAction::Push { view } => {
                let state = build_subview(plugin_id, view, declarations)?;
                if let Some(position) = self.position_of(&state) {
                    self.subviews.truncate(position + 1);
                } else {
                    self.subviews.push(state);
                }
                true
            }
            UiNavigationAction::Replace { view } => {
                if self
                    .active()
                    .is_some_and(|active| active.plugin_id != plugin_id)
                {
                    return Err(anyhow!("插件 `{plugin_id}` 不能替换其他插件的子视图"));
                }
                let state = build_subview(plugin_id, view, declarations)?;
                if self.active().is_some() {
                    self.subviews.pop();
                }
                self.subviews.push(state);
                true
            }
            UiNavigationAction::Pop => {
                let Some(active) = self.active() else {
                    return Err(anyhow!("主视图不能继续返回"));
                };
                if active.plugin_id != plugin_id {
                    return Err(anyhow!("插件 `{plugin_id}` 不能关闭其他插件的子视图"));
                }
                self.subviews.pop();
                true
            }
            UiNavigationAction::OpenAgentSession {
                view_id,
                instance_id,
                target,
                title,
                prompt,
                notice,
            } => {
                validate_token("view_id", &view_id)?;
                validate_token("instance_id", &instance_id)?;
                // 复用声明的路由与标题，同时要求它确实是子视图：原生会话同样占满整屏。
                let declaration = declarations
                    .iter()
                    .find(|declaration| {
                        declaration.plugin_id == plugin_id && declaration.view_id == view_id
                    })
                    .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未声明视图类型 `{view_id}`"))?;
                if declaration.placement != UiPlacement::Subview {
                    return Err(anyhow!(
                        "原生会话视图 `{plugin_id}/{view_id}` 必须是 subview 类型"
                    ));
                }
                let target = AgentId::from_str(&target)
                    .map_err(|error| anyhow!("原生会话目标不是合法 Agent ID：{error}"))?;
                let title = title
                    .filter(|title| !title.trim().is_empty())
                    .unwrap_or_else(|| declaration.title.clone());
                // 同一实例重复打开时只更新目标与说明：成员续跑会产生新句柄，
                // 若压入新视图，用户按 Esc 就会看到同一个成员的旧句柄。
                if let Some(existing) = self.subviews.iter_mut().find(|state| {
                    state.plugin_id == plugin_id
                        && matches!(&state.body, SubviewBody::AgentSession { .. })
                        && state.body.route_key() == instance_id
                }) {
                    existing.title = title;
                    existing.body = SubviewBody::AgentSession {
                        view_id,
                        instance_id,
                        target,
                        prompt,
                        notice,
                    };
                } else {
                    self.subviews.push(SubviewState {
                        plugin_id: plugin_id.to_string(),
                        title,
                        body: SubviewBody::AgentSession {
                            view_id,
                            instance_id,
                            target,
                            prompt,
                            notice,
                        },
                        area: Rect::default(),
                    });
                }
                true
            }
        };
        self.handled_requests.insert(request_key);
        Ok(changed)
    }

    /// 处理用户的通用返回操作，不将 Esc 强制交给插件。
    pub(crate) fn pop_for_user(&mut self) -> bool {
        self.subviews.pop().is_some()
    }

    /// 返回从 Lucia 主视图到当前子视图的面包屑标题。
    pub(crate) fn breadcrumbs(&self) -> Vec<&str> {
        std::iter::once("Lucia")
            .chain(self.subviews.iter().map(|view| view.title.as_str()))
            .collect()
    }

    /// 返回导航栈中全部原生 Agent 会话视图，按栈顺序排列。
    pub(crate) fn agent_sessions(&self) -> Vec<AgentSessionRef<'_>> {
        self.subviews
            .iter()
            .filter_map(|state| match &state.body {
                SubviewBody::AgentSession {
                    view_id,
                    instance_id,
                    target,
                    prompt,
                    notice,
                } => Some(AgentSessionRef {
                    plugin_id: state.plugin_id.as_str(),
                    view_id,
                    instance_id,
                    target,
                    prompt: prompt.as_deref(),
                    notice: notice.as_deref(),
                }),
                SubviewBody::Plugin { .. } => None,
            })
            .collect()
    }

    /// 覆盖指定原生会话视图的说明；视图已不在栈中时保持幂等。
    pub(crate) fn set_agent_session_notice(
        &mut self,
        plugin_id: &str,
        instance_id: &str,
        notice: Option<String>,
    ) {
        if let Some(state) = self.subviews.iter_mut().find(|state| {
            state.plugin_id == plugin_id
                && matches!(&state.body, SubviewBody::AgentSession { .. })
                && state.body.route_key() == instance_id
        }) {
            if let SubviewBody::AgentSession { notice: slot, .. } = &mut state.body {
                *slot = notice;
            }
        }
    }

    fn position_of(&self, candidate: &SubviewState) -> Option<usize> {
        self.subviews.iter().position(|existing| {
            existing.plugin_id == candidate.plugin_id
                && existing.body.route_key() == candidate.body.route_key()
                && matches!(
                    (&existing.body, &candidate.body),
                    (SubviewBody::Plugin { .. }, SubviewBody::Plugin { .. })
                        | (
                            SubviewBody::AgentSession { .. },
                            SubviewBody::AgentSession { .. }
                        )
                )
        })
    }
}

/// 校验子视图类型和实例后构建应用内部状态。
fn build_subview(
    plugin_id: &str,
    view: UiViewInstance,
    declarations: &[UiDeclaration],
) -> Result<SubviewState> {
    validate_token("view_id", &view.view_id)?;
    validate_token("instance_id", &view.instance_id)?;
    let declaration = declarations
        .iter()
        .find(|declaration| {
            declaration.plugin_id == plugin_id && declaration.view_id == view.view_id
        })
        .ok_or_else(|| anyhow!("插件 `{plugin_id}` 未声明视图类型 `{}`", view.view_id))?;
    if declaration.placement != UiPlacement::Subview {
        return Err(anyhow!(
            "视图 `{plugin_id}/{}` 不是 subview 类型",
            view.view_id
        ));
    }
    let title = view
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| declaration.title.clone());
    Ok(SubviewState {
        plugin_id: plugin_id.to_string(),
        title,
        body: SubviewBody::Plugin { view, frame: None },
        area: Rect::default(),
    })
}

/// 校验跨 ABI 传递且参与路由的不透明标识。
fn validate_token(name: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(anyhow!("{name} 必须是 1..=256 字节且不含控制字符"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_plugin_host::ui::UiSize;

    /// 创建一个可供导航的测试子视图声明。
    fn declaration(plugin_id: &str, view_id: &str) -> UiDeclaration {
        UiDeclaration {
            plugin_id: plugin_id.into(),
            view_id: view_id.into(),
            title: "任务详情".into(),
            placement: UiPlacement::Subview,
            size: UiSize::default(),
            focusable: true,
            input_triggers: Vec::new(),
        }
    }

    /// 多个动态实例应共享视图类型，并按栈顺序返回主视图。
    #[test]
    fn dynamic_subviews_share_declaration_and_keep_instances() {
        let declarations = vec![declaration("agents", "agent-detail")];
        let mut stack = ViewStack::default();
        for (request_id, instance_id) in [("open-1", "agent-1"), ("open-2", "agent-2")] {
            stack
                .apply(
                    "agents",
                    UiNavigationRequest {
                        request_id: request_id.into(),
                        action: UiNavigationAction::Push {
                            view: UiViewInstance {
                                view_id: "agent-detail".into(),
                                instance_id: instance_id.into(),
                                title: Some(instance_id.into()),
                            },
                        },
                    },
                    &declarations,
                )
                .expect("打开子视图");
        }

        assert_eq!(
            stack
                .active()
                .and_then(|view| view.view())
                .map(|view| view.instance_id.as_str()),
            Some("agent-2")
        );
        assert!(stack.pop_for_user());
        assert_eq!(
            stack
                .active()
                .and_then(|view| view.view())
                .map(|view| view.instance_id.as_str()),
            Some("agent-1")
        );
        assert!(stack.pop_for_user());
        assert!(stack.is_main());
    }

    /// 重复请求必须幂等，且插件不能关闭其他插件的子视图。
    #[test]
    fn navigation_is_idempotent_and_owner_scoped() {
        let declarations = vec![declaration("agents", "agent-detail")];
        let request = UiNavigationRequest {
            request_id: "open".into(),
            action: UiNavigationAction::Push {
                view: UiViewInstance {
                    view_id: "agent-detail".into(),
                    instance_id: "agent-1".into(),
                    title: None,
                },
            },
        };
        let mut stack = ViewStack::default();

        assert!(stack
            .apply("agents", request.clone(), &declarations)
            .expect("首次导航"));
        assert!(!stack
            .apply("agents", request, &declarations)
            .expect("重复导航"));
        assert!(stack
            .apply(
                "other",
                UiNavigationRequest {
                    request_id: "close".into(),
                    action: UiNavigationAction::Pop,
                },
                &declarations,
            )
            .is_err());
    }

    /// 原生会话复用声明的路由，重复打开只替换目标句柄。
    #[test]
    fn agent_session_view_updates_target_in_place() {
        let declarations = vec![declaration("teammate", "member-session")];
        let mut stack = ViewStack::default();
        for (request_id, target) in [
            ("open-1", "3f2504e0-4f89-11d3-9a0c-0305e82c3301"),
            ("open-2", "3f2504e0-4f89-11d3-9a0c-0305e82c3302"),
        ] {
            assert!(stack
                .apply(
                    "teammate",
                    UiNavigationRequest {
                        request_id: request_id.into(),
                        action: UiNavigationAction::OpenAgentSession {
                            view_id: "member-session".into(),
                            instance_id: "member-1".into(),
                            target: target.into(),
                            title: Some("Researcher".into()),
                            prompt: None,
                            notice: None,
                        },
                    },
                    &declarations,
                )
                .expect("打开原生会话视图"));
        }

        assert_eq!(stack.subviews.len(), 1, "续跑不应压入重复视图");
        let active = stack.active().expect("原生会话视图保持激活");
        assert_eq!(active.title, "Researcher");
        assert_eq!(
            active.agent_target().map(ToString::to_string),
            Some("3f2504e0-4f89-11d3-9a0c-0305e82c3302".into())
        );
    }

    /// 原生会话目标必须是合法 Agent ID，避免把任意字符串当句柄。
    #[test]
    fn agent_session_rejects_invalid_target() {
        let declarations = vec![declaration("teammate", "member-session")];
        let mut stack = ViewStack::default();
        let error = stack
            .apply(
                "teammate",
                UiNavigationRequest {
                    request_id: "open-1".into(),
                    action: UiNavigationAction::OpenAgentSession {
                        view_id: "member-session".into(),
                        instance_id: "member-1".into(),
                        target: "not-an-agent".into(),
                        title: None,
                        prompt: None,
                        notice: None,
                    },
                },
                &declarations,
            )
            .expect_err("非法目标必须被拒绝");

        assert!(error.to_string().contains("Agent ID"));
    }

    /// 原生会话也必须使用已声明的子视图，输入才能沿声明的路由回插件。
    #[test]
    fn agent_session_requires_declared_subview() {
        let mut stack = ViewStack::default();
        let error = stack
            .apply(
                "teammate",
                UiNavigationRequest {
                    request_id: "open-1".into(),
                    action: UiNavigationAction::OpenAgentSession {
                        view_id: "missing".into(),
                        instance_id: "member-1".into(),
                        target: "3f2504e0-4f89-11d3-9a0c-0305e82c3301".into(),
                        title: None,
                        prompt: None,
                        notice: None,
                    },
                },
                &[],
            )
            .expect_err("未声明的视图必须被拒绝");

        assert!(error.to_string().contains("未声明视图类型"));
    }
}

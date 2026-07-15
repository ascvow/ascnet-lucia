//! 主视图与插件子视图之间的通用导航状态机。

use agent_plugin_host::ui::{
    UiDeclaration, UiFrame, UiNavigationAction, UiNavigationRequest, UiPlacement, UiViewInstance,
};
use anyhow::{anyhow, Result};
use ratatui::layout::Rect;
use std::collections::HashSet;

/// 一个已进入导航栈的插件子视图。
#[derive(Debug, Clone)]
pub(crate) struct SubviewState {
    /// 由 Host 注入的可信插件 ID。
    pub(crate) plugin_id: String,
    /// 插件提供的视图类型和动态实例身份。
    pub(crate) view: UiViewInstance,
    /// 主应用显示的标题。
    pub(crate) title: String,
    /// 最近一次成功渲染的声明式帧。
    pub(crate) frame: Option<UiFrame>,
    /// 当前终端帧中去除标题后的内容区。
    pub(crate) area: Rect,
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

    fn position_of(&self, candidate: &SubviewState) -> Option<usize> {
        self.subviews.iter().position(|existing| {
            existing.plugin_id == candidate.plugin_id
                && existing.view.view_id == candidate.view.view_id
                && existing.view.instance_id == candidate.view.instance_id
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
        view,
        title,
        frame: None,
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
            stack.active().map(|view| view.view.instance_id.as_str()),
            Some("agent-2")
        );
        assert!(stack.pop_for_user());
        assert_eq!(
            stack.active().map(|view| view.view.instance_id.as_str()),
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
}

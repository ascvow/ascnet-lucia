//! 上下文引用 provider 的轮询、校验与暂存。
//!
//! # 为什么由应用拉取而不是插件推送
//!
//! Eden 一类的编辑器在协议 v1 只有「客户端请求、服务端应答」，没有事件推送通道。
//! WASM 插件也没有后台线程，无法在用户选中文本时主动醒来。因此宿主按固定节拍向声明了
//! `agent.context-ref` 能力的插件索要一次快照：插件仍然拥有协议细节，应用只知道
//! 「当前有哪些可引用的上下文」这一通用契约。
//!
//! 等 Eden 侧具备推送能力后，只需让插件在收到推送时返回同一份快照，应用侧无需改动。
//!
//! # 应用侧承担的三件事
//!
//! 1. **合法性验证**：快照必须满足 [`ContextRefSnapshot::validate`] 的边界，任何一条
//!    不合规就整份丢弃——自动注入不能有「部分接受」这种模糊状态。
//! 2. **暂存与去重**：同一 provider 下相同 ID 视为同一段上下文，只更新正文；ID 变化
//!    才新增标签。
//! 3. **尊重用户删除**：用户删掉标签后，同一 ID 在选区变化前不再自动补回。
//!
//! # 为什么在后台任务里拉取
//!
//! 拉取是一次 Guest 调用，内部可能等待 socket 十秒。事件循环同时负责渲染与输入，直接
//! `await` 会让编辑器在 Eden 无响应时整屏卡死；因此拉取放进后台任务，结果通过
//! [`crate::event::UiEvent::ContextRefsLoaded`] 回到主循环。

use crate::app::App;
use crate::event::{ContextRefOutcome, UiEvent};
use crate::session::conversation::{Msg, MsgKind, PendingRef};
use agent_plugin_host::{LivePluginHost, PluginHost, PluginServiceCall};
use agent_plugin_protocol::{
    ContextRef, ContextRefRequest, ContextRefSnapshot, CONTEXT_REF_SERVICE,
};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 两次快照拉取之间的最短间隔。
///
/// 每次拉取都会进入一次 Guest 调用；间隔过短会让轮询本身成为可观测的 CPU 成本，
/// 间隔过长则用户选中后要等一会儿才看到标签。
pub(crate) const CONTEXT_REF_POLL_INTERVAL: Duration = Duration::from_millis(750);

/// 宿主调用插件服务时使用的稳定身份。
const HOST_SERVICE_CALLER: &str = "lucia-tui";

impl App {
    /// 在空闲时安排一次后台快照拉取。
    ///
    /// 运行中、插件加载中、上下文重载期间与已有拉取在途时直接跳过：这些时刻 Guest
    /// 实例可能正被占用，与工具调用抢同一把锁只会让两边都变慢。
    #[cfg(feature = "plugins")]
    pub(crate) fn schedule_context_ref_poll(&mut self, host: Arc<LivePluginHost>) {
        if self.running || self.plugins_loading || self.pending_reload.is_some() {
            return;
        }
        if self.context_ref_task.is_some() {
            return;
        }
        let now = Instant::now();
        if self
            .context_ref_polled_at
            .is_some_and(|last| now.saturating_duration_since(last) < CONTEXT_REF_POLL_INTERVAL)
        {
            return;
        }
        self.context_ref_polled_at = Some(now);

        let tx = self.tx.clone();
        self.context_ref_task = Some(tokio::spawn(async move {
            let outcomes = fetch_context_refs(host.as_ref()).await;
            let _ = tx.send(UiEvent::ContextRefsLoaded(outcomes));
        }));
    }

    /// 应用一次后台拉取结果，返回界面是否发生变化。
    ///
    /// 校验失败的 provider 只提示一次：每次轮询都往主事件列表追加同一条错误，会把
    /// 真正需要用户处理的信息淹掉。
    #[cfg(feature = "plugins")]
    pub(crate) fn apply_context_refs(&mut self, outcomes: Vec<ContextRefOutcome>) -> bool {
        let mut snapshots = Vec::new();
        let mut errors = Vec::new();
        for outcome in outcomes {
            match outcome.refs {
                Ok(refs) => snapshots.push((outcome.provider, refs)),
                Err(message) => errors.push((outcome.provider, message)),
            }
        }
        self.report_context_ref_errors(errors);
        apply_snapshots(self, snapshots)
    }

    /// 按 provider 与错误文本去重后追加一条主事件列表消息。
    #[cfg(feature = "plugins")]
    fn report_context_ref_errors(&mut self, errors: Vec<(String, String)>) {
        match errors.into_iter().next() {
            Some((provider, message)) => {
                let current = (provider, message);
                if self.context_ref_error.as_ref() != Some(&current) {
                    self.messages
                        .push(Msg::new(MsgKind::Error, current.1.clone()));
                    self.context_ref_error = Some(current);
                }
            }
            None => self.context_ref_error = None,
        }
    }
}

/// 向全部上下文引用 provider 索取一次快照。
///
/// 单个 provider 不可用时不影响其他 provider：引用是增益信息，不是运行前提。
async fn fetch_context_refs(host: &dyn PluginHost) -> Vec<ContextRefOutcome> {
    let Ok(services) = host.services().await else {
        return Vec::new();
    };
    let mut outcomes = Vec::new();
    for service in services {
        if service.name != CONTEXT_REF_SERVICE {
            continue;
        }
        let call = PluginServiceCall {
            caller_id: HOST_SERVICE_CALLER.to_string(),
            plugin_id: service.plugin_id.clone(),
            name: service.name.clone(),
            payload: serde_json::to_value(ContextRefRequest::default())
                .unwrap_or(serde_json::Value::Null),
        };
        let refs = match host.call_service(&call).await {
            Ok(Some(value)) => decode_snapshot(value),
            Ok(None) => Ok(Vec::new()),
            // 服务不可用（Eden 没运行、插件实例正忙）不产生用户可见错误。
            Err(_) => Ok(Vec::new()),
        };
        outcomes.push(ContextRefOutcome {
            provider: service.plugin_id,
            refs,
        });
    }
    outcomes
}

/// 解析并校验一份快照；任何不合规都整份拒绝。
fn decode_snapshot(value: serde_json::Value) -> Result<Vec<ContextRef>, String> {
    let snapshot: ContextRefSnapshot = serde_json::from_value(value)
        .map_err(|error| format!("上下文引用快照格式非法：{error}"))?;
    let refs = snapshot
        .validate()
        .map_err(|error| format!("上下文引用未通过合法性校验：{error}"))?;
    Ok(refs.to_vec())
}

/// 把本次快照应用到输入框，返回是否有变化。
fn apply_snapshots(app: &mut App, latest: Vec<(String, Vec<ContextRef>)>) -> bool {
    let incoming = latest
        .into_iter()
        .flat_map(|(provider, refs)| refs.into_iter().map(move |entry| (provider.clone(), entry)))
        .collect::<Vec<_>>();
    let incoming_ids = incoming
        .iter()
        .map(|(provider, entry)| (provider.as_str(), entry.id.as_str()))
        .collect::<HashSet<_>>();
    let mut changed = drop_stale_refs(app, &incoming_ids);

    for (provider, entry) in incoming {
        if let Some(staged) = app
            .refs
            .iter_mut()
            .find(|staged| staged.provider == provider && staged.id == entry.id)
        {
            // 同一段上下文：正文可能因为用户继续编辑而变化，标签本身保持原位。
            if staged.title != entry.title || staged.body != entry.body {
                staged.title = entry.title;
                staged.body = entry.body;
                changed = true;
            }
            continue;
        }
        if app.dismissed_refs.contains(&entry.id) {
            continue;
        }
        let staged = PendingRef {
            label: next_ref_label(app),
            provider,
            id: entry.id,
            title: entry.title,
            body: entry.body,
        };
        let clip = staged.clip();
        app.input.insert_str(app.cursor, &clip);
        app.cursor += clip.len();
        app.refs.push(staged);
        changed = true;
    }

    if changed {
        app.finish_input_edit();
    }
    changed
}

/// 移除本次快照里已经消失的引用，并同步删掉输入框中的标签。
fn drop_stale_refs(app: &mut App, incoming: &HashSet<(&str, &str)>) -> bool {
    let stale = app
        .refs
        .iter()
        .filter(|entry| !incoming.contains(&(entry.provider.as_str(), entry.id.as_str())))
        .cloned()
        .collect::<Vec<_>>();
    if stale.is_empty() {
        return false;
    }
    for entry in &stale {
        app.input = app.input.replace(&entry.clip(), "");
    }
    app.refs
        .retain(|entry| !stale.iter().any(|gone| gone == entry));
    // 标签被删后光标可能落到文本末尾之后。
    app.cursor = app.cursor.min(app.input.len());
    true
}

/// 分配输入框中尚未使用的最小引用标签。
fn next_ref_label(app: &App) -> String {
    let mut index = 1;
    loop {
        let label = format!("[REF#{index}]");
        if !app.refs.iter().any(|entry| entry.label == label) {
            return label;
        }
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建只用于状态测试的事件通道。
    fn test_channel() -> tokio::sync::mpsc::UnboundedSender<crate::event::UiEvent> {
        tokio::sync::mpsc::unbounded_channel().0
    }

    /// 构造一条快照条目。
    fn entry(id: &str, body: &str) -> ContextRef {
        ContextRef {
            id: id.to_string(),
            title: format!("App.swift:{id}"),
            body: body.to_string(),
        }
    }

    /// 新引用必须自动插入标签，并能在提交时展开为内容块。
    #[test]
    fn fresh_snapshot_stages_label_and_blocks() {
        let mut app = App::new(test_channel(), "test".into());
        let changed = apply_snapshots(
            &mut app,
            vec![(
                "eden".into(),
                vec![entry("selection-1", "```swift\nlet a = 1\n```")],
            )],
        );

        assert!(changed);
        assert_eq!(app.refs.len(), 1);
        assert_eq!(app.refs[0].label, "[REF#1]");
        assert!(app.input.contains("[REF#1]"));

        app.input.push_str(" 这段是什么");
        app.cursor = app.input.len();
        let submission = app.take_submission().expect("应产生提交");
        let blocks = submission.blocks();
        assert!(blocks.iter().any(|block| matches!(
            block,
            agent_core::model::ContentBlock::Text { text } if text.contains("let a = 1")
        )));
        // 标签本身不能作为正文发给模型。
        assert!(blocks.iter().all(|block| match block {
            agent_core::model::ContentBlock::Text { text } => !text.contains("[REF#1]"),
            _ => true,
        }));
        assert!(app.refs.is_empty(), "提交后引用必须清空");
    }

    /// 同一 ID 的重复快照只更新正文，不重复插入标签。
    #[test]
    fn same_id_updates_in_place() {
        let mut app = App::new(test_channel(), "test".into());
        apply_snapshots(
            &mut app,
            vec![("eden".into(), vec![entry("selection-1", "第一版")])],
        );
        let changed = apply_snapshots(
            &mut app,
            vec![("eden".into(), vec![entry("selection-1", "第二版")])],
        );

        assert!(changed, "正文变化应触发更新");
        assert_eq!(app.refs.len(), 1);
        assert_eq!(app.refs[0].body, "第二版");
        assert_eq!(app.input.matches("[REF#1]").count(), 1);
    }

    /// 选区消失后标签必须随之消失，不能留下指向空内容的引用。
    #[test]
    fn disappearing_selection_removes_label() {
        let mut app = App::new(test_channel(), "test".into());
        apply_snapshots(
            &mut app,
            vec![("eden".into(), vec![entry("selection-1", "内容")])],
        );

        let changed = apply_snapshots(&mut app, vec![("eden".into(), Vec::new())]);

        assert!(changed);
        assert!(app.refs.is_empty());
        assert!(!app.input.contains("[REF#1]"));
    }

    /// 用户删除标签后，同一 ID 不得被下一轮轮询补回。
    #[test]
    fn dismissed_ref_is_not_restaged() {
        let mut app = App::new(test_channel(), "test".into());
        apply_snapshots(
            &mut app,
            vec![("eden".into(), vec![entry("selection-1", "内容")])],
        );
        app.input.clear();
        app.cursor = 0;
        app.finish_input_edit();
        assert!(app.refs.is_empty(), "标签删除后引用应被剪掉");

        let changed = apply_snapshots(
            &mut app,
            vec![("eden".into(), vec![entry("selection-1", "内容")])],
        );

        assert!(!changed, "同一 ID 不应被自动补回");
        assert!(app.input.is_empty());
    }

    /// 多个 provider 的 ID 各自独立，互不覆盖。
    #[test]
    fn providers_are_scoped_independently() {
        let mut app = App::new(test_channel(), "test".into());
        apply_snapshots(
            &mut app,
            vec![
                ("eden".into(), vec![entry("same", "A")]),
                ("other".into(), vec![entry("same", "B")]),
            ],
        );

        assert_eq!(app.refs.len(), 2);
        let bodies = app
            .refs
            .iter()
            .map(|entry| entry.body.as_str())
            .collect::<Vec<_>>();
        assert_eq!(bodies, vec!["A", "B"]);
    }

    /// 非法快照必须整份拒绝，并给出一次用户可见的错误。
    #[test]
    fn invalid_snapshot_is_rejected_whole() {
        assert!(decode_snapshot(serde_json::json!({ "refs": [{ "id": "a" }] })).is_err());
        assert!(decode_snapshot(serde_json::json!({
            "refs": [
                { "id": "a", "title": "A", "body": "x" },
                { "id": "a", "title": "B", "body": "y" }
            ]
        }))
        .is_err());

        let mut app = App::new(test_channel(), "test".into());
        app.apply_context_refs(vec![ContextRefOutcome {
            provider: "eden".into(),
            refs: Err("上下文引用 ID 重复".into()),
        }]);
        assert!(app.refs.is_empty());
        assert_eq!(app.messages.len(), 1, "首次错误应可见");

        // 同一条错误不应每次轮询都重复追加。
        app.apply_context_refs(vec![ContextRefOutcome {
            provider: "eden".into(),
            refs: Err("上下文引用 ID 重复".into()),
        }]);
        assert_eq!(app.messages.len(), 1);
    }
}

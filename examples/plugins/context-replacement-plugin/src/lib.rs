//! Lucia 上下文完整替换能力测试插件。

use agent_plugin::{
    export_plugin, ContextLoadRequest, EventPresentation, EventPresentationTone, ExtensionEvent,
    AgentPlugin, LoadedContext, PluginHostApi, Result,
};
use serde_json::json;

/// 用确定性摘要替换完整消息列表的测试插件。
#[derive(Default)]
struct ContextReplacementPlugin;

impl AgentPlugin for ContextReplacementPlugin {
    /// 返回固定摘要，并发布可由主事件列表展示的结构化事件。
    fn load_context(
        &mut self,
        host: &dyn PluginHostApi,
        request: ContextLoadRequest,
    ) -> Result<Option<LoadedContext>> {
        let before = request.messages.len();
        let summary = format!("测试摘要：已将 {before} 条上下文消息替换为本摘要。");
        host.emit_event(&ExtensionEvent {
            name: "context.replacement.completed".into(),
            data: json!({"before_messages": before, "after_messages": 1}),
            presentation: Some(EventPresentation::divider(
                "上下文压缩",
                EventPresentationTone::Info,
            )),
        })?;
        Ok(Some(LoadedContext {
            system: request.system,
            messages: vec![json!({
                "role": "developer",
                "content": [{
                    "type": "text",
                    "text": summary
                }]
            })],
        }))
    }
}

export_plugin!(ContextReplacementPlugin);

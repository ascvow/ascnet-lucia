//! TUI 在界面决策点发布的 `tui.*` 事件。
//!
//! 界面事件与 [`agent_core::bus`] 中的 `core.*` 事件共用同一条总线和同一套取消语义，
//! 但命名空间和负载完全独立：core 不认识按键，TUI 也不参与 ReAct 决策。插件按名字
//! 订阅两侧事件，宿主按统一优先级排序，因此一个插件可以同时拦截工具调用和快捷键。
//!
//! # 取消语义
//!
//! | 事件 | 取消后果 |
//! |---|---|
//! | [`KeyEvent`] | 该按键被吞掉，不再路由给命令、插件视图或主输入区 |
//! | [`PasteEvent`] | 该次粘贴被丢弃 |
//!
//! [`RunFinishEvent`] 是既成事实的通告，TUI 忽略它的取消状态。
//!
//! # 信任边界
//!
//! 不可信监听者只能改写各事件 `merge_patch` 白名单内的字段。按键事件完全只读——
//! 允许插件改写按键会让它伪造用户手势；粘贴事件只开放文本本身。

use agent_core::declare_event;
use anyhow::{anyhow, Result};
use crossterm::event::{KeyCode, KeyModifiers};
use serde::Serialize;
use serde_json::Value;

/// 将 Crossterm 按键转换为稳定、可跨语言处理的名称。
///
/// TUI 事件与插件 UI 输入协议共用这一份映射，避免同一个按键在两条通道上出现两种名字。
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

/// 一次按键即将被路由给命令、插件视图或主输入区。
///
/// 事件对不可信监听者只读：插件可以观察并吞掉按键，但不能改写成另一个按键。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct KeyEvent {
    /// 规范化的按键名，与插件 UI 输入协议使用同一套名称。
    pub(crate) code: String,
    /// 规范化的修饰键名列表。
    pub(crate) modifiers: Vec<String>,
    /// 当前主输入区内容，供插件判断上下文。
    pub(crate) input: String,
    /// Agent 当前是否正在运行。
    pub(crate) running: bool,
}

declare_event!(KeyEvent, "tui.input.key");

/// 一段终端粘贴内容即将写入主输入区。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PasteEvent {
    /// 待写入的粘贴文本；对不可信监听者开放改写。
    pub(crate) text: String,
}

impl PasteEvent {
    /// 合并不可信监听者的补丁；只允许改写粘贴文本。
    fn merge_patch(&mut self, patch: &Value) -> Result<()> {
        if let Some(text) = patch.get("text") {
            self.text = text
                .as_str()
                .ok_or_else(|| anyhow!("粘贴文本必须是字符串"))?
                .to_string();
        }
        Ok(())
    }
}

declare_event!(
    PasteEvent,
    "tui.input.paste",
    merge = PasteEvent::merge_patch
);

/// 一次 Agent 运行在界面侧收敛完成。
///
/// 与 `core.run.finish` 的区别在于时机：本事件在会话已持久化、界面状态已更新之后
/// 发布，因此插件可以据此安全地读取最终会话。
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RunFinishEvent {
    /// 本次运行是否产出了结果；失败或保存异常时为 `false`。
    pub(crate) succeeded: bool,
    /// 运行是否被用户取消。
    pub(crate) cancelled: bool,
}

declare_event!(RunFinishEvent, "tui.run.finish");

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::EventData;
    use serde_json::json;

    /// 按键事件对不可信监听者完全只读，插件不得伪造用户手势。
    #[test]
    fn key_event_rejects_every_patch() {
        let mut event = KeyEvent {
            code: "esc".into(),
            modifiers: Vec::new(),
            input: String::new(),
            running: false,
        };

        event
            .merge(&json!({ "code": "enter", "modifiers": ["ctrl"] }))
            .expect("只读事件应忽略补丁");

        assert_eq!(event.code, "esc");
        assert!(event.modifiers.is_empty());
    }

    /// 粘贴事件只开放文本改写，非法取值必须报错而不是静默截断。
    #[test]
    fn paste_event_merges_only_text() {
        let mut event = PasteEvent {
            text: "原始".into(),
        };

        event
            .merge(&json!({ "text": "插件改写" }))
            .expect("白名单补丁应合并成功");
        assert_eq!(event.text, "插件改写");

        assert!(event.merge(&json!({ "text": 42 })).is_err());
        assert_eq!(event.text, "插件改写");
    }
}

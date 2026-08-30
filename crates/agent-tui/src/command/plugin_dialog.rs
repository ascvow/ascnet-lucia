//! 原生插件管理页的打开、刷新与人工写操作。

use crate::app::App;
use crate::command::{
    load_native_plugin_dialog, NativePluginDialog, NativePluginDialogData,
    NativePluginDialogStatus, NativePluginPathMode,
};
use crate::event::UiEvent;
use crate::plugin::management::{ManualPluginAction, PluginManagementOutcome};
use crate::session::conversation::{Msg, MsgKind};
use crossterm::event::{KeyCode, KeyModifiers};
use std::path::PathBuf;
use std::sync::Arc;

impl App {
    /// 打开人工插件管理页，并异步读取 Manager 与冻结 Stable 环境。
    #[cfg(feature = "plugins")]
    pub(crate) fn open_native_plugins(&mut self) {
        self.native_command.plugin_dialog = Some(NativePluginDialog {
            selected: 0,
            status: NativePluginDialogStatus::Loading,
            path_mode: None,
            path_input: String::new(),
            remove_confirmation: None,
        });
        self.reload_native_plugins();
    }

    /// 重新加载插件管理页；缺少插件管理服务时以只读错误说明阻止写操作。
    #[cfg(feature = "plugins")]
    pub(crate) fn reload_native_plugins(&mut self) {
        let Some(dialog) = self.native_command.plugin_dialog.as_mut() else {
            return;
        };
        dialog.status = NativePluginDialogStatus::Loading;
        dialog.remove_confirmation = None;
        let Some(service) = self.plugin_management.as_ref().map(Arc::clone) else {
            dialog.status =
                NativePluginDialogStatus::Error("插件管理服务不可用；插件写操作已禁用".into());
            return;
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = load_native_plugin_dialog(service.as_ref())
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::NativePluginsLoaded(result));
        });
    }

    /// 把插件页异步加载结果应用到仍然打开的 Dialog。
    #[cfg(feature = "plugins")]
    pub(crate) fn handle_native_plugins_loaded(
        &mut self,
        result: Result<NativePluginDialogData, String>,
    ) {
        let Some(dialog) = self.native_command.plugin_dialog.as_mut() else {
            return;
        };
        dialog.selected = 0;
        dialog.status = match result {
            Ok(data) => NativePluginDialogStatus::Ready(data),
            Err(error) => NativePluginDialogStatus::Error(error),
        };
    }

    /// 把人工管理事务结果显示到消息流，并刷新插件页的 Stable 摘要。
    #[cfg(feature = "plugins")]
    pub(crate) fn handle_native_plugin_action_finished(
        &mut self,
        result: Result<PluginManagementOutcome, String>,
    ) {
        match result {
            Ok(outcome) => {
                let suffix = if outcome.changed {
                    "插件环境已改变；新 Session 生效，当前 Session 继续使用启动时快照。"
                } else {
                    "插件环境没有变化。"
                };
                self.messages.push(Msg::new(
                    MsgKind::Info,
                    format!("{}\n{suffix}", outcome.message),
                ));
                self.reload_native_plugins();
            }
            Err(error) => {
                if let Some(dialog) = self.native_command.plugin_dialog.as_mut() {
                    dialog.status = NativePluginDialogStatus::Error(error.clone());
                }
                self.messages.push(Msg::new(
                    MsgKind::Error,
                    format!("人工插件管理失败：{error}"),
                ));
            }
        }
    }

    /// 处理插件管理页的列表导航、路径输入、启停和二次确认移除。
    #[cfg(feature = "plugins")]
    pub(crate) fn handle_native_plugin_dialog_key(
        &mut self,
        code: KeyCode,
        modifiers: KeyModifiers,
    ) -> bool {
        if crate::app::is_copy_shortcut(code, modifiers) {
            return false;
        }
        let Some(dialog) = self.native_command.plugin_dialog.as_mut() else {
            return false;
        };
        if let Some(mode) = dialog.path_mode {
            match code {
                KeyCode::Esc => {
                    dialog.path_mode = None;
                    dialog.path_input.clear();
                }
                KeyCode::Backspace => {
                    dialog.path_input.pop();
                }
                KeyCode::Char(character)
                    if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
                {
                    dialog.path_input.push(character);
                }
                KeyCode::Enter if !dialog.path_input.trim().is_empty() => {
                    let action = match mode {
                        NativePluginPathMode::InstallRegistry => {
                            ManualPluginAction::InstallRegistry {
                                request: dialog.path_input.trim().to_string(),
                                enabled: true,
                            }
                        }
                        NativePluginPathMode::InstallGithub => ManualPluginAction::InstallGithub {
                            source: dialog.path_input.trim().to_string(),
                            enabled: true,
                        },
                        NativePluginPathMode::Install => ManualPluginAction::InstallLocal {
                            bundle: PathBuf::from(dialog.path_input.trim()),
                            enabled: true,
                        },
                        NativePluginPathMode::Replace => ManualPluginAction::ReplaceLocal {
                            bundle: PathBuf::from(dialog.path_input.trim()),
                            enabled: true,
                        },
                        NativePluginPathMode::Rollback => {
                            let mut parts = dialog.path_input.split_whitespace();
                            let plugin_id = parts.next().unwrap_or_default();
                            let version = parts.next().unwrap_or_default();
                            if plugin_id.is_empty() || version.is_empty() || parts.next().is_some()
                            {
                                return true;
                            }
                            ManualPluginAction::Rollback {
                                plugin_id: plugin_id.to_string(),
                                version: version.to_string(),
                            }
                        }
                        NativePluginPathMode::SelectCapabilityOwner => {
                            let mut parts = dialog.path_input.split_whitespace();
                            let capability_id = parts.next().unwrap_or_default();
                            let plugin_id = parts.next().unwrap_or_default();
                            if capability_id.is_empty()
                                || plugin_id.is_empty()
                                || parts.next().is_some()
                            {
                                return true;
                            }
                            ManualPluginAction::SelectCapabilityOwner {
                                capability_id: capability_id.to_string(),
                                plugin_id: plugin_id.to_string(),
                            }
                        }
                        NativePluginPathMode::ClearCapabilityOwner => {
                            ManualPluginAction::ClearCapabilityOwner {
                                capability_id: dialog.path_input.trim().to_string(),
                            }
                        }
                    };
                    dialog.path_mode = None;
                    dialog.path_input.clear();
                    self.start_native_plugin_action(action);
                }
                _ => {}
            }
            return true;
        }
        let item_count = match &dialog.status {
            NativePluginDialogStatus::Ready(data) => data.plugins.len(),
            NativePluginDialogStatus::Loading
            | NativePluginDialogStatus::Applying
            | NativePluginDialogStatus::Error(_) => 0,
        };
        match code {
            KeyCode::Esc => self.native_command.plugin_dialog = None,
            KeyCode::Char('r') => self.reload_native_plugins(),
            KeyCode::Char('i') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::Install);
                dialog.path_input.clear();
            }
            KeyCode::Char('g') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::InstallRegistry);
                dialog.path_input.clear();
            }
            KeyCode::Char('h') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::InstallGithub);
                dialog.path_input.clear();
            }
            KeyCode::Char('u') if item_count > 0 => {
                dialog.path_mode = Some(NativePluginPathMode::Replace);
                dialog.path_input.clear();
            }
            KeyCode::Char('b') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::Rollback);
                dialog.path_input.clear();
            }
            KeyCode::Char('o') if item_count > 0 => {
                dialog.path_mode = Some(NativePluginPathMode::SelectCapabilityOwner);
                dialog.path_input.clear();
            }
            KeyCode::Char('w') if matches!(dialog.status, NativePluginDialogStatus::Ready(_)) => {
                dialog.path_mode = Some(NativePluginPathMode::ClearCapabilityOwner);
                dialog.path_input.clear();
            }
            KeyCode::Char('t') if item_count > 0 => {
                let NativePluginDialogStatus::Ready(data) = &dialog.status else {
                    unreachable!("插件数量只会来自 Ready 状态");
                };
                let plugin_id = data.plugins[dialog.selected.min(data.plugins.len() - 1)]
                    .id
                    .clone();
                self.start_native_plugin_action(ManualPluginAction::UpdateRegistry { plugin_id });
            }
            KeyCode::Up if item_count > 0 => {
                dialog.selected = dialog.selected.checked_sub(1).unwrap_or(item_count - 1);
                dialog.remove_confirmation = None;
            }
            KeyCode::Down if item_count > 0 => {
                dialog.selected = (dialog.selected + 1) % item_count;
                dialog.remove_confirmation = None;
            }
            KeyCode::Char('e') if item_count > 0 => {
                let NativePluginDialogStatus::Ready(data) = &dialog.status else {
                    unreachable!("插件数量只会来自 Ready 状态");
                };
                let plugin = &data.plugins[dialog.selected.min(data.plugins.len() - 1)];
                let action = if plugin.enabled {
                    ManualPluginAction::Disable {
                        plugin_id: plugin.id.clone(),
                    }
                } else {
                    ManualPluginAction::Enable {
                        plugin_id: plugin.id.clone(),
                    }
                };
                self.start_native_plugin_action(action);
            }
            KeyCode::Char('x') if item_count > 0 => {
                let NativePluginDialogStatus::Ready(data) = &dialog.status else {
                    unreachable!("插件数量只会来自 Ready 状态");
                };
                let plugin_id = data.plugins[dialog.selected.min(data.plugins.len() - 1)]
                    .id
                    .clone();
                if dialog.remove_confirmation.as_deref() == Some(plugin_id.as_str()) {
                    dialog.remove_confirmation = None;
                    self.start_native_plugin_action(ManualPluginAction::Remove { plugin_id });
                } else {
                    dialog.remove_confirmation = Some(plugin_id);
                }
            }
            _ => {}
        }
        true
    }

    /// 在后台执行插件人工管理事务，期间 Dialog 保持不可编辑。
    #[cfg(feature = "plugins")]
    pub(crate) fn start_native_plugin_action(&mut self, action: ManualPluginAction) {
        let Some(service) = self.plugin_management.as_ref().map(Arc::clone) else {
            if let Some(dialog) = self.native_command.plugin_dialog.as_mut() {
                dialog.status =
                    NativePluginDialogStatus::Error("插件管理服务不可用；插件写操作已禁用".into());
            }
            return;
        };
        if let Some(dialog) = self.native_command.plugin_dialog.as_mut() {
            dialog.status = NativePluginDialogStatus::Applying;
            dialog.remove_confirmation = None;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = service
                .apply(action)
                .await
                .map_err(|error| error.to_string());
            let _ = tx.send(UiEvent::NativePluginActionFinished(result));
        });
    }
}

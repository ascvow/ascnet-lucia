//! TUI 回归测试的共享夹具与按主题划分的用例集合。

//! Lucia TUI 状态、输入、渲染和会话协调回归测试。

use crate::app::{
    input_ref_spans, App, ChannelEventSink, MouseTextSelection, ESC_DOUBLE_PRESS_WINDOW,
};

#[cfg(feature = "plugins")]
use crate::app::{PluginKeyRoute, PluginViewState};

use crate::config::configured_model_key_is_available;

use crate::demo::build_demo_gateway;

use crate::event::{AgentCompletion, UiEvent};

#[cfg(feature = "plugins")]
use crate::plugin::host_actions::{resume_selected_session, sessions_page};

#[cfg(feature = "plugins")]
use crate::plugin::startup::{
    discover_plugin_manifests, merge_plugin_manifests, plugin_manifest_ids,
    remove_disabled_plugin_manifests,
};

use crate::session::attachment::{attachment_media_type, pasted_file_path};

use crate::session::conversation::{Msg, MsgKind, PendingAttachment};

use crate::session::{
    load_startup_session, restore_session_messages, run_and_persist, workspace_project_id,
    LazyFileSessionStore, WorkspaceContext,
};

#[cfg(feature = "plugins")]
use crate::theme::PLUGIN_STATUS_DETAIL_TICKS;

use crate::theme::{COLOR_DANGER, COLOR_MUTED, COLOR_WARNING};

use crate::ui::markdown::{markdown_lines, summarize_json};

#[cfg(feature = "plugins")]
use crate::ui::plugin_bridge::{apply_plugin_footer_event, apply_plugin_frames};

use crate::ui::root::render_root;

use agent_core::config::AgentRootConfig;

use agent_core::event::{AgentEvent, AgentEventKind, EventSink};

#[cfg(feature = "plugins")]
use agent_core::model::MessageRole;

use agent_core::model::{ContentBlock, ReasoningLevel};

use agent_core::{Agent, AgentRun, Session};

#[cfg(feature = "plugins")]
use agent_plugin_host::wasm::PluginLoadFailure;

#[cfg(feature = "plugins")]
use agent_plugin_host::{LivePluginHost, PluginHost};

#[cfg(feature = "plugins")]
use agent_plugin_protocol::UiFrame as PluginUiFrame;

#[cfg(feature = "plugins")]
use agent_plugin_protocol::{
    ToolRenderRequest, ToolRendererContribution, UiNavigationAction, UiSize, UiViewInstance,
};

#[cfg(feature = "plugins")]
use agent_plugin_protocol::{
    ToolRenderState, UiDeclaration, UiInputEvent, UiLine, UiNavigationRequest, UiPlacement,
    UiSessionListStatus, UiSpan, UiStyle,
};

use agent_session::MemorySessionStore;

use agent_session::SessionId;

use agent_session::SessionRecord;

use agent_session::SessionStore;

use agent_session::SessionStoreError;

use agent_tool::ToolCall;

use agent_tool::ToolOutputDelta;

use agent_tool::ToolResult;

#[cfg(feature = "plugins")]
use agent_tool::ToolSpec;

use anyhow::anyhow;

use anyhow::Result;

use async_trait::async_trait;

use crossterm::event::KeyCode;

use crossterm::event::KeyModifiers;

use crossterm::event::MouseEvent;

use crossterm::event::MouseEventKind;

use ratatui::prelude::*;

use ratatui::{backend::TestBackend, Terminal};

use serde_json::json;

use serde_json::Value;

use std::collections::VecDeque;

use std::path::Path;

use std::path::PathBuf;

use std::sync::Arc;

#[cfg(feature = "plugins")]
use std::{fs, time::SystemTime};

use tokio::sync::mpsc;

/// 将测试终端缓冲区转换为去除宽字符占位空格的纯文本。
fn render_text(width: u16, height: u16, running: bool) -> String {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("创建测试终端");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::new(tx, "测试模型".into());
    app.running = running;
    app.messages.extend([
        Msg::new(MsgKind::User, "测试消息"),
        Msg::new(MsgKind::Assistant, "测试回复"),
    ]);

    terminal
        .draw(|frame| render_root(frame, &mut app))
        .expect("渲染测试界面");
    terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// 测试存储在指定 save 调用上注入错误，其余操作委托给内存存储。
#[derive(Clone)]
struct ScriptedSaveStore {
    /// 保存成功时使用的真实内存存储。
    inner: MemorySessionStore,
    /// 从一开始计数的 save 调用次数。
    save_calls: Arc<std::sync::atomic::AtomicUsize>,
    /// 需要失败的 save 调用序号。
    failing_calls: Arc<Vec<usize>>,
    /// 注入的错误类型。
    failure: ScriptedSaveFailure,
}

/// 测试所需的保存失败时序与错误类型。
#[derive(Clone, Copy)]
enum ScriptedSaveFailure {
    /// 模拟另一进程抢先更新记录。
    RevisionConflict,
    /// 模拟底层文件写入失败。
    Io,
    /// 模拟 payload 已写入，但提交确认阶段返回 I/O 错误。
    IoAfterCommit,
}

impl ScriptedSaveStore {
    /// 创建按调用序号失败的测试存储。
    fn new(failing_calls: Vec<usize>, failure: ScriptedSaveFailure) -> Self {
        Self {
            inner: MemorySessionStore::new(),
            save_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            failing_calls: Arc::new(failing_calls),
            failure,
        }
    }

    /// 构造与当前保存记录匹配的模拟错误。
    fn failure_for(&self, record: &SessionRecord) -> SessionStoreError {
        match self.failure {
            ScriptedSaveFailure::RevisionConflict => SessionStoreError::RevisionConflict {
                id: record.id.clone(),
                expected: Some(record.revision),
                actual: Some(record.revision.saturating_add(1)),
            },
            ScriptedSaveFailure::Io | ScriptedSaveFailure::IoAfterCommit => SessionStoreError::Io {
                operation: "模拟保存会话",
                path: PathBuf::from("scripted-session.json"),
                source: std::io::Error::other("模拟写入失败"),
            },
        }
    }
}

#[async_trait]
impl SessionStore for ScriptedSaveStore {
    async fn load(&self, id: &SessionId) -> Result<Option<SessionRecord>, SessionStoreError> {
        self.inner.load(id).await
    }

    async fn save(
        &self,
        record: SessionRecord,
        expected_revision: Option<u64>,
    ) -> Result<SessionRecord, SessionStoreError> {
        let call = self
            .save_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        if self.failing_calls.contains(&call) {
            let error = self.failure_for(&record);
            if matches!(self.failure, ScriptedSaveFailure::IoAfterCommit) {
                self.inner.save(record, expected_revision).await?;
            }
            return Err(error);
        }
        self.inner.save(record, expected_revision).await
    }

    async fn delete(
        &self,
        id: &SessionId,
        expected_revision: u64,
    ) -> Result<(), SessionStoreError> {
        self.inner.delete(id, expected_revision).await
    }

    async fn list(&self) -> Result<Vec<SessionRecord>, SessionStoreError> {
        self.inner.list().await
    }
}

/// 为测试工具返回自定义消息帧的模拟宿主。
#[cfg(feature = "plugins")]
struct ToolMessagePluginHost;

#[cfg(feature = "plugins")]
#[async_trait]
impl agent_core::AgentExtension for ToolMessagePluginHost {
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(vec![ToolSpec::new(
            "test_tool",
            "测试工具",
            ToolSpec::empty_object_schema(),
        )])
    }
}

#[cfg(feature = "plugins")]
#[async_trait]
impl PluginHost for ToolMessagePluginHost {
    fn id(&self) -> Option<&str> {
        Some("test-plugin")
    }

    async fn tool_renderers(&self) -> Result<Vec<ToolRendererContribution>> {
        Ok(vec![ToolRendererContribution {
            plugin_id: String::new(),
            renderer_id: "test-message".into(),
            tool_name: "test_tool".into(),
        }])
    }

    async fn render_tool(&self, request: &ToolRenderRequest) -> Result<Option<PluginUiFrame>> {
        let state = match &request.context.state {
            ToolRenderState::Running => "运行中",
            ToolRenderState::Finished { .. } => "已完成",
            ToolRenderState::Skipped { .. } => "已跳过",
        };
        Ok(Some(PluginUiFrame {
            view_id: request.renderer_id.clone(),
            visible: true,
            lines: vec![UiLine {
                spans: vec![UiSpan {
                    text: format!("自定义工具 · {state}"),
                    style: UiStyle::default(),
                }],
            }],
            cursor: None,
        }))
    }
}

/// 创建测试插件视图，覆盖停靠、对话框和焦点路由测试。
#[cfg(feature = "plugins")]
fn test_plugin_view(placement: UiPlacement, title: &str) -> PluginViewState {
    PluginViewState {
        declaration: UiDeclaration {
            plugin_id: "test-plugin".into(),
            view_id: format!("{placement:?}").to_ascii_lowercase(),
            title: title.into(),
            placement,
            size: UiSize {
                width: Some(24),
                height: Some(8),
            },
            focusable: true,
            input_triggers: Vec::new(),
        },
        frame: Some(PluginUiFrame {
            view_id: format!("{placement:?}").to_ascii_lowercase(),
            visible: true,
            lines: vec![UiLine {
                spans: vec![UiSpan {
                    text: "插件内容".into(),
                    style: UiStyle::default(),
                }],
            }],
            cursor: None,
        }),
        area: Rect::default(),
    }
}

/// 写入一个测试附件文件，返回路径；测试结束由调用方删除。
fn write_temp_attachment(name: &str, bytes: &[u8]) -> PathBuf {
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("系统时间应晚于 UNIX 纪元")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "lucia-attach-test-{}-{unique}-{name}",
        std::process::id()
    ));
    std::fs::write(&path, bytes).expect("写入测试附件");
    path
}

mod input;
mod plugin;
mod render;
mod run;
mod session;

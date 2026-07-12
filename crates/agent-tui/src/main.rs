//! Lucia 交互式 TUI（基于 Ratatui）。

mod app_config;
mod app_state;
mod application;
#[cfg(feature = "plugins")]
mod command_surface;
mod conversation;
#[cfg(feature = "plugins")]
mod plugin_startup;
mod session_coordination;
mod tui;

#[cfg(feature = "plugins")]
use command_surface::*;
use conversation::*;
#[cfg(feature = "plugins")]
use plugin_startup::*;
use session_coordination::*;

#[cfg(feature = "plugins")]
use agent_core::AgentExtension;
use agent_core::{
    config::AgentRootConfig,
    event::{AgentEvent, AgentEventKind, CompositeEventSink, EventSink, JsonlEventSink},
    model::{
        ChatModel, ContentBlock, MessageRole, ModelGateway, ModelRequest, ModelResponse,
        ProviderAdapter,
    },
    Agent, AgentOptions, AgentRun, Session,
};
#[cfg(feature = "plugins")]
use agent_core::{ContextLoadRequest, LoadedContext};
#[cfg(feature = "plugins")]
use agent_plugin_host::{
    manifest::{load_plugin_runtime_config, PluginManifest},
    ui::{
        UiColor, UiDeclaration, UiFrame as PluginUiFrame, UiInput, UiInputEvent, UiLine,
        UiNavigationRequest, UiPlacement, UiRenderRequest, UiSpan, UiStyle, UI_NAVIGATION_EVENT,
    },
    wasm::{load_wasm_plugins_resilient_with_selection_and_services, PluginLoadFailure},
    CompositePluginHost, PluginHost, PluginHostServices, PluginServiceCall,
};
#[cfg(feature = "plugins")]
use agent_runtime::{
    AgentDeriveConfig, AgentPermissions, AgentProfileId, AgentRuntime, AgentTemplate, RuntimeLimits,
};
use agent_session::{
    FileSessionStore, MemorySessionStore, SessionId, SessionRecord, SessionStore,
    SessionStoreError, SessionSummary,
};
use agent_tool::{JsonTool, ToolCall, ToolRegistry, ToolSpec};
#[cfg(feature = "plugins")]
use anyhow::Context;
use anyhow::{anyhow, Result};
#[cfg(feature = "plugins")]
use app_config::discover_official_plugin_manifests;
use app_config::{
    initialize_config, load_tui_settings, lucia_home_dir, resolve_config_path,
    resolve_config_relative_path, TuiSettings,
};
use app_state::*;
use async_trait::async_trait;
use clap::Parser;
#[cfg(feature = "plugins")]
use command_protocol::{
    CommandAvailability, CommandCallbackResponse, CommandSnapshot, CommandSpec, CompletionContext,
    CompletionItem, PrepareCompletionRequest, PrepareCompletionResponse, PrepareExecuteRequest,
    PrepareExecuteResponse, SessionListStatus, SessionSummary as CommandSessionSummary,
    SnapshotRequest, SurfaceAction, SurfaceEffect, SurfaceEffectsResponse, SurfaceUpdateRequest,
    PREPARE_COMPLETION_SERVICE, PREPARE_EXECUTE_SERVICE, PROVIDER_PLUGIN_ID,
    SESSION_COMPLETION_SOURCE, SESSION_DIALOG_VIEW, SNAPSHOT_SERVICE, SURFACE_POLL_EFFECTS_SERVICE,
    SURFACE_UPDATE_SERVICE,
};
#[cfg(feature = "plugins")]
use crossterm::event::MouseEvent;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::{prelude::*, widgets::*};
#[cfg(feature = "plugins")]
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
#[cfg(feature = "plugins")]
use std::collections::{HashMap, HashSet};
use std::{
    path::Path,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::mpsc;
use tui::render_root;
#[cfg(feature = "plugins")]
use tui::{
    apply_plugin_navigation_event, dispatch_plugin_input, drain_plugin_ui_events,
    refresh_plugin_view, refresh_plugin_views, view::ViewStack,
};

// ─── CLI 参数 ───

#[derive(Debug, Parser)]
#[command(author, version, about = "Lucia 交互式 ReAct Agent")]
struct Args {
    /// 初始化配置文件后退出；默认写入 `$LUCIA_HOME/config.toml`。
    #[arg(long = "init", alias = "init-config")]
    init: bool,

    /// 使用内置脚本模型。
    #[arg(long)]
    demo: bool,

    /// TOML 配置文件路径；默认读取 `LUCIA_CONFIG` 或 `$LUCIA_HOME/config.toml`。
    #[arg(long)]
    config: Option<PathBuf>,

    /// 可选的 agent 事件 JSONL 输出文件，用于排查模型请求与工具调用。
    #[arg(long = "events-jsonl")]
    events_jsonl: Option<PathBuf>,

    /// 项目会话根目录；覆盖配置文件和 `$LUCIA_HOME/projects` 默认值。
    #[arg(long = "sessions-dir")]
    sessions_dir: Option<PathBuf>,

    /// 要恢复和持续更新的稳定会话标识；覆盖配置中的默认值。
    #[arg(long = "session-id")]
    session_id: Option<String>,

    /// 恢复最近更新的持久化会话；显式 `--session-id` 优先。
    #[arg(long = "resume-latest")]
    resume_latest: bool,

    /// 列出持久化会话后退出，不连接模型服务。
    #[arg(long = "list-sessions")]
    list_sessions: bool,

    /// 插件 manifest 路径；可以重复传入并按参数顺序占用 UI 插槽。
    #[cfg(feature = "plugins")]
    #[arg(long = "plugin-manifest")]
    plugin_manifests: Vec<PathBuf>,
}

// ─── UI 事件 ───

enum UiEvent {
    Input(Event),
    Tick,
    ModelStarted,
    /// 共享文本缓冲区已有尚未渲染的模型增量。
    ModelTextReady,
    ToolStarted {
        name: String,
        /// 调用参数的单行摘要。
        args: String,
    },
    ToolFinished {
        name: String,
        is_error: bool,
        /// 返回内容的单行摘要。
        result: String,
    },
    ToolSkipped(String),
    SteeringInjected,
    FollowUpInjected,
    /// 扩展发布到主事件列表的结构化展示事件。
    Extension {
        text: String,
        color: Color,
        divider: bool,
    },
    /// 插件请求主应用更新子视图导航栈。
    #[cfg(feature = "plugins")]
    ViewNavigation {
        plugin_id: String,
        request: UiNavigationRequest,
    },
    /// 最近一次模型请求消耗的上下文 token 数。
    ContextUsage(u64),
    /// Agent 运行结束，携带至少一次持久化后的会话状态。
    AgentDone(Box<AgentCompletion>),
    /// 后台会话摘要查询完成，等待注入 Command 插件界面。
    #[cfg(feature = "plugins")]
    CommandSurfaceUpdate {
        request_id: u64,
        status: SessionListStatus,
    },
    /// 后台取得新的命令注册表快照。
    #[cfg(feature = "plugins")]
    CommandSnapshotLoaded(Box<Result<Option<CommandSnapshot>>>),
    /// 显式参数补全请求完成。
    #[cfg(feature = "plugins")]
    CommandCompletionLoaded {
        generation: u64,
        result: Box<Result<Option<ResolvedCommandCompletion>>>,
    },
    /// Background plugin loading completed and can now be attached to the pending Agent.
    /// 后台插件加载结束，可挂载到等待中的 Agent。
    #[cfg(feature = "plugins")]
    PluginsLoaded(Box<Result<LoadedPlugins>>),
}

/// 一次用户输入从先行持久化到模型运行结束的完整结果。
struct AgentCompletion {
    /// 模型已成功完成时携带运行结果。
    run: Option<AgentRun>,
    /// 下一轮必须使用的完整记录；最终保存失败时可能是尚未落盘的 dirty 完成态。
    session_record: SessionRecord,
    /// 保存或模型运行失败时的错误。
    error: Option<anyhow::Error>,
    /// 用户输入是否已经写入会话存储。
    input_committed: bool,
    /// 当前完成态是否允许自动执行下一条 FIFO 输入。
    queue_may_advance: bool,
    /// 用于保存失败时恢复编辑器内容的原始输入。
    input: UserSubmission,
}

/// 一次与输入快照绑定的参数候选结果。
#[cfg(feature = "plugins")]
struct ResolvedCommandCompletion {
    /// 发起请求时的完整编辑器内容。
    source_input: String,
    /// 发起请求时的 UTF-8 字节光标。
    source_cursor: usize,
    /// Provider 校验后的参数位置与替换区间。
    context: CompletionContext,
    /// 已经过 Provider、SDK 或宿主数据源限制的候选。
    items: Vec<CompletionItem>,
}

/// Plugin runtime data prepared off the TUI event loop.
///
/// 在 TUI 事件循环之外准备完成的插件运行时数据。
#[cfg(feature = "plugins")]
struct LoadedPlugins {
    /// Composite host containing every successfully activated plugin. 已激活插件的组合宿主。
    host: Arc<CompositePluginHost>,
    /// Stable plugin IDs in dependency-resolved load order. 按依赖解析顺序排列的稳定插件 ID。
    plugin_ids: Vec<String>,
    /// UI declarations collected after activation. 激活后收集的 UI 声明。
    plugin_views: Vec<UiDeclaration>,
    /// Activation events consumed before the first Agent run. 首次 Agent 运行前消费的激活事件。
    startup_events: Vec<Value>,
    /// Plugins excluded by activation failures or required dependencies. 因激活或必选依赖失败而被剔除的插件。
    failures: Vec<PluginLoadFailure>,
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// 原生 TUI 调用 Command Provider 时使用的稳定身份。
#[cfg(feature = "plugins")]
const TUI_COMMAND_CALLER: &str = "lucia-tui";
/// 官方 Context 插件的稳定 ID。
#[cfg(feature = "plugins")]
const CONTEXT_PLUGIN_ID: &str = "context";
/// 官方 Context 插件立即压缩当前 Session 的服务名。
#[cfg(feature = "plugins")]
const CONTEXT_COMPACT_SERVICE: &str = "context.compact";

/// 输入区域的聚焦边框颜色。
const COLOR_BORDER_FOCUS: Color = Color::Rgb(112, 110, 104);
/// 主要文字颜色。
const COLOR_TEXT: Color = Color::Rgb(224, 222, 216);
/// 次要文字和边框颜色。
const COLOR_MUTED: Color = Color::Rgb(124, 122, 116);
/// 用户消息强调色。
const COLOR_USER: Color = Color::Rgb(104, 190, 126);
/// 成功状态颜色。
const COLOR_SUCCESS: Color = Color::Rgb(104, 190, 126);
/// 运行和等待状态颜色。
const COLOR_WARNING: Color = Color::Rgb(197, 164, 103);
/// 错误状态颜色。
const COLOR_DANGER: Color = Color::Rgb(205, 101, 101);
/// 用户消息块背景色，用于与助手正文区分。
const COLOR_USER_BG: Color = Color::Rgb(42, 56, 46);
/// 状态栏品牌块前景色，配合 COLOR_USER 背景使用。
const COLOR_CHIP_FG: Color = Color::Rgb(28, 30, 28);
/// Number of 80 ms UI ticks before startup plugin details collapse into the compact counter.
/// 启动插件详情收敛为紧凑计数前保留的 80 毫秒 UI tick 数。
#[cfg(feature = "plugins")]
const PLUGIN_STATUS_DETAIL_TICKS: u16 = 75;
/// UI 动画和后台维护使用的基础 tick 间隔。
const UI_TICK_INTERVAL_MS: u64 = 80;
/// 单个附件大小上限（10 MiB），兼顾常见模型接口的请求体限制。
const MAX_ATTACHMENT_BYTES: u64 = 10 * 1024 * 1024;
/// 周期插件视图刷新间隔，约为一秒。
#[cfg(feature = "plugins")]
const PLUGIN_REFRESH_TICKS: u8 = 12;
#[tokio::main]
async fn main() -> Result<()> {
    application::run().await
}

/// 明文密钥和环境变量任一包含非空值即视为可用；该检查不会读取或记录密钥内容。
/// 判断配置中的模型密钥是否可以用于本次启动。
fn configured_model_key_is_available(config: &AgentRootConfig) -> bool {
    config
        .model
        .api_key
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || config
            .model
            .api_key_env
            .as_deref()
            .and_then(std::env::var_os)
            .is_some_and(|value| !value.is_empty())
}

/// 构建无需外部模型服务的确定性演示运行时。
fn build_demo_gateway() -> (ModelGateway, AgentOptions) {
    let mut gateway = ModelGateway::new();
    gateway
        .register("default", Arc::new(ScriptedReActModel))
        .expect("注册脚本模型");
    let options = AgentOptions {
        provider: "default".to_string(),
        model: "scripted-react-demo".to_string(),
        max_steps: 0,
        ..AgentOptions::default()
    };
    (gateway, options)
}

fn echo_spec() -> ToolSpec {
    ToolSpec::new(
        "echo",
        "回显输入文本。",
        json!({
            "type": "object",
            "properties": {
                "text": { "type": "string", "description": "要回显的文本" }
            },
            "required": ["text"],
            "additionalProperties": false
        }),
    )
}

/// 确定性脚本模型，不联网即可演示 ReAct loop。
struct ScriptedReActModel;

#[async_trait]
impl ChatModel for ScriptedReActModel {
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse> {
        if let Some(tool_text) = latest_tool_result_text(&req) {
            return Ok(ModelResponse::text(format!("工具返回: {tool_text}")));
        }
        let user_text = latest_user_text(&req).unwrap_or_default();
        if req.tools.iter().any(|t| t.name == "echo") {
            Ok(ModelResponse::tool_calls(vec![ToolCall::new(
                "demo-call-1",
                "echo",
                json!({ "text": user_text }),
            )]))
        } else {
            Ok(ModelResponse::text(format!(
                "没有可用工具。用户说: {user_text}"
            )))
        }
    }
}

#[async_trait]
impl ProviderAdapter for ScriptedReActModel {
    fn name(&self) -> &'static str {
        "scripted-react-demo"
    }
}

fn latest_user_text(req: &ModelRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .find(|m| matches!(&m.role, MessageRole::User))
        .map(|m| m.text_content())
}

fn latest_tool_result_text(req: &ModelRequest) -> Option<String> {
    req.messages
        .iter()
        .rev()
        .find(|m| matches!(&m.role, MessageRole::Tool))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolResult { result } => Some(result.content_text()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
}

#[cfg(test)]
#[path = "main/tests.rs"]
mod tests;

//! Eden 编辑器上下文插件。
//!
//! 插件通过 Eden 暴露的本地 Unix domain socket 读取「用户此刻在编辑器里看着什么」，
//! 并在用户确实看着同一个 tab 时把某一行显示出来。
//!
//! # 为什么走宿主能力而不是 process_exec
//!
//! 早期做法是调用 `eden-cli` 再解析 stdout。那要求宿主机上装着这个 CLI，而且 `process_exec`
//! 是完整原生进程信任——为了读一次选区就交出执行任意命令的能力，权限与收益完全不成比例。
//! 现在插件只申请三条窄能力：连接一个写死的 socket 路径、只读一个写死的令牌文件、读取
//! 三个 Eden 注入的环境变量。
//!
//! # 工具与协议
//!
//! 每个工具对应一个 Eden 协议工具，参数做本地校验后再发出去：本地能判定的错误不必浪费
//! 一次往返，也能给出更贴近模型的提示。协议错误码与 `hint` 全部保留并回传，模型据此
//! 自我纠错（例如把 `scope` 改成 `active`，或退化成文字告知）。
//!
//! # 不做的事
//!
//! 插件不读整个文件、不轮询选区变化、不在多个 tab 之间抢焦点。协议 v1 没有事件推送，
//! 抢焦点被 Eden 用 `user_in_another_tab` 明确拒绝——这里直接按文字结论处理。

mod client;
mod protocol;

use agent_plugin::{
    export_plugin, ActivationContext, AgentPlugin, PluginHostApi, Result, ToolCall,
    ToolPermissionScope, ToolResult, ToolSpec,
};
use anyhow::anyhow;
use client::{EdenClient, EdenConfig, EdenFailure};
use protocol::Welcome;
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// 读取选区与光标位置的工具名。
const SELECTION_TOOL: &str = "eden_selection";
/// 列出已打开文件的工具名。
const EDITORS_TOOL: &str = "eden_editors";
/// 读取会话所在 tab 上下文的工具名。
const CONTEXT_TOOL: &str = "eden_context";
/// 定位某一行（唯一的写操作）的工具名。
const REVEAL_TOOL: &str = "eden_reveal";

/// 协议侧的对应工具名。
const SELECTION_RPC: &str = "selection.get";
/// 协议侧的对应工具名。
const EDITORS_RPC: &str = "editors.list";
/// 协议侧的对应工具名。
const CONTEXT_RPC: &str = "context.get";
/// 协议侧的对应工具名。
const REVEAL_RPC: &str = "editor.reveal";

/// 协议默认的选区文本上限。
const DEFAULT_MAX_TEXT: i64 = 8_000;
/// 协议允许的选区文本上限。
const MAX_TEXT_LIMIT: i64 = 200_000;

/// 无参数工具使用的严格空对象。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

/// `eden_selection` 的参数。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionArgs {
    /// `session`（默认）或 `active`。
    #[serde(default)]
    scope: Option<String>,
    /// 是否返回选中的原文。
    #[serde(default)]
    include_text: Option<bool>,
    /// 文本上限，夹在 `0…200000`。
    #[serde(default)]
    max_text: Option<i64>,
}

/// `eden_reveal` 的参数。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RevealArgs {
    /// 已打开文件的绝对路径。
    path: String,
    /// 1-based 行号。
    #[serde(default)]
    line: Option<i64>,
}

/// Eden 编辑器上下文插件实例。
#[derive(Default)]
struct EdenPlugin {
    /// 长驻客户端；第一个工具调用时按 Eden 注入的环境变量创建。
    client: Option<EdenClient>,
}

impl EdenPlugin {
    /// 取出或惰性创建客户端。
    ///
    /// 不在 `activate` 里连接：Eden 可能没在运行，加载阶段失败会让整个插件被禁用，
    /// 而正确行为是「需要时再连，连不上就报一次可读错误」。
    fn client(&mut self) -> &mut EdenClient {
        self.client
            .get_or_insert_with(|| EdenClient::new(EdenConfig::from_env()))
    }

    /// 执行一次协议调用并转成工具结果。
    fn run_rpc(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
        rpc: &str,
        args: Value,
    ) -> Result<ToolResult> {
        match self.client().call(host, rpc, args) {
            Ok(value) => Ok(ToolResult::success(call.id, call.name, value)),
            Err(failure) => Ok(failure_result(call, failure)),
        }
    }

    /// 读取选区或光标位置。
    fn selection(&mut self, host: &dyn PluginHostApi, call: ToolCall) -> Result<ToolResult> {
        let args: SelectionArgs = parse_args(&call)?;
        let scope = match args.scope.as_deref() {
            None | Some("session") => "session",
            Some("active") => "active",
            Some(other) => {
                return Ok(ToolResult::error(
                    call.id,
                    call.name,
                    format!("scope 只能是 `session` 或 `active`，收到 `{other}`"),
                ))
            }
        };
        let include_text = args.include_text.unwrap_or(true);
        let max_text = args
            .max_text
            .unwrap_or(DEFAULT_MAX_TEXT)
            .clamp(0, MAX_TEXT_LIMIT);

        let mut request = Map::new();
        request.insert("scope".to_string(), json!(scope));
        request.insert("includeText".to_string(), json!(include_text));
        request.insert("maxText".to_string(), json!(max_text));
        self.run_rpc(host, call, SELECTION_RPC, Value::Object(request))
    }

    /// 列出已打开的文件。
    fn editors(&mut self, host: &dyn PluginHostApi, call: ToolCall) -> Result<ToolResult> {
        let _: EmptyArgs = parse_args(&call)?;
        self.run_rpc(host, call, EDITORS_RPC, json!({}))
    }

    /// 读取会话所在 tab 的上下文。
    fn context(&mut self, host: &dyn PluginHostApi, call: ToolCall) -> Result<ToolResult> {
        let _: EmptyArgs = parse_args(&call)?;
        self.run_rpc(host, call, CONTEXT_RPC, json!({}))
    }

    /// 请求 Eden 定位某一行。
    fn reveal(&mut self, host: &dyn PluginHostApi, call: ToolCall) -> Result<ToolResult> {
        let args: RevealArgs = parse_args(&call)?;
        if !args.path.starts_with('/') {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                format!("path 必须是绝对路径，收到 `{}`", args.path),
            ));
        }
        let line = args.line.unwrap_or(1);
        if line < 1 {
            return Ok(ToolResult::error(
                call.id,
                call.name,
                format!("line 必须是 1-based 正整数，收到 `{line}`"),
            ));
        }

        // 先握手才能判断焦点：没有 welcome 就没有可信的 focused 值，盲发写请求只会被拒。
        if let Err(failure) = self.client().ensure_connected(host) {
            return Ok(failure_result(call, failure));
        }
        if self
            .client()
            .welcome()
            .and_then(Welcome::focused)
            .is_some_and(|focused| !focused)
        {
            return Ok(out_of_focus_result(call, &args.path, line));
        }

        let request = json!({ "path": args.path, "line": line });
        match self.client().call(host, REVEAL_RPC, request) {
            Ok(value) => Ok(ToolResult::success(call.id, call.name, value)),
            // 焦点可能在握手之后才被切走；按协议建议退化成文字告知，而不是让模型反复重试。
            Err(EdenFailure::Protocol(error)) if error.code == "user_in_another_tab" => {
                Ok(out_of_focus_result(call, &args.path, line))
            }
            Err(failure) => Ok(failure_result(call, failure)),
        }
    }
}

impl AgentPlugin for EdenPlugin {
    /// 读取 Eden 注入的连接参数；实际连接推迟到第一次工具调用。
    fn activate(&mut self, _host: &dyn PluginHostApi, _context: ActivationContext) -> Result<()> {
        self.client = Some(EdenClient::new(EdenConfig::from_env()));
        Ok(())
    }

    /// 卸载前发送 `bye` 并释放 socket 句柄。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        if let Some(client) = self.client.as_mut() {
            client.close(host);
        }
        Ok(())
    }

    /// 声明四个编辑器上下文工具。
    fn list_tools(&self) -> Vec<ToolSpec> {
        vec![
            selection_tool(),
            editors_tool(),
            context_tool(),
            reveal_tool(),
        ]
    }

    /// 按工具名分发；只有 reveal 会产生界面副作用。
    fn call_tool_with_host(
        &mut self,
        host: &dyn PluginHostApi,
        call: ToolCall,
    ) -> Result<ToolResult> {
        match call.name.as_str() {
            SELECTION_TOOL => self.selection(host, call),
            EDITORS_TOOL => self.editors(host, call),
            CONTEXT_TOOL => self.context(host, call),
            REVEAL_TOOL => self.reveal(host, call),
            other => {
                let message = format!("Eden 插件收到未知工具调用 `{other}`");
                Ok(ToolResult::error(call.id, call.name, message))
            }
        }
    }
}

/// 解析工具参数，并把反序列化错误转成可读说明。
fn parse_args<T: serde::de::DeserializeOwned>(call: &ToolCall) -> Result<T> {
    serde_json::from_value(call.args.clone())
        .map_err(|error| anyhow!("工具 `{}` 的参数不合法：{error}", call.name))
}

/// 把失败原因转成工具错误结果，保留 Eden 的 hint。
fn failure_result(call: ToolCall, failure: EdenFailure) -> ToolResult {
    let message = match &failure {
        EdenFailure::Protocol(error) if error.is_retryable() => {
            format!("{}。Eden 暂时不可用，稍后重试即可。", failure)
        }
        EdenFailure::Protocol(_) => failure.to_string(),
        EdenFailure::Transport(_) => format!(
            "{}（提示：确认 Eden 正在运行，且 Lucia 是从 Eden 的终端窗格启动的）",
            failure
        ),
    };
    ToolResult::error(call.id, call.name, message)
}

/// 用户没在看本 tab 时的降级结果：告诉模型位置而不是抢焦点。
fn out_of_focus_result(call: ToolCall, path: &str, line: i64) -> ToolResult {
    ToolResult::success(
        call.id,
        call.name,
        json!({
            "revealed": false,
            "reason": "user_in_another_tab",
            "path": path,
            "line": line,
            "message": format!(
                "用户正在看别的 tab，未切换界面。请直接用文字告诉用户：{path} 第 {line} 行。"
            ),
        }),
    )
}

/// 构建 `eden_selection` 的工具定义。
fn selection_tool() -> ToolSpec {
    ToolSpec::new(
        SELECTION_TOOL,
        "读取用户此刻在 Eden 编辑器里选中的文本与行号范围；没有选区时仍返回文件路径和光标所在行。\
         仅当用户询问当前代码、选区或「我正在看哪一行」时使用。",
        json!({
            "type": "object",
            "properties": {
                "scope": {
                    "type": "string",
                    "enum": ["session", "active"],
                    "description": "session（默认）读取本会话所在 tab 的编辑器；active 跟随用户当前选中的 tab。"
                },
                "include_text": {
                    "type": "boolean",
                    "description": "是否返回选中的原文，默认 true。"
                },
                "max_text": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": MAX_TEXT_LIMIT,
                    "description": "选区文本上限，默认 8000；超出会被截断并标记 truncated。"
                }
            },
            "additionalProperties": false
        }),
    )
    .with_permission_scope(ToolPermissionScope::Network)
}

/// 构建 `eden_editors` 的工具定义。
fn editors_tool() -> ToolSpec {
    ToolSpec::new(
        EDITORS_TOOL,
        "列出 Eden 中所有已打开的文件，含是否未保存、语言、所属 tab 与是否为会话所在 tab。\
         用于判断某个文件是否已经打开。",
        ToolSpec::empty_object_schema(),
    )
    .with_permission_scope(ToolPermissionScope::Network)
}

/// 构建 `eden_context` 的工具定义。
fn context_tool() -> ToolSpec {
    ToolSpec::new(
        CONTEXT_TOOL,
        "读取 Eden 当前会话所在 tab 的上下文：tab 标题、激活窗格、分屏列表、工作目录与文件树根。\
         需要知道用户在哪个项目或哪个窗格时使用。",
        ToolSpec::empty_object_schema(),
    )
    .with_permission_scope(ToolPermissionScope::Network)
}

/// 构建 `eden_reveal` 的工具定义。
fn reveal_tool() -> ToolSpec {
    ToolSpec::new(
        REVEAL_TOOL,
        "在 Eden 中定位并显示某个已打开文件的某一行。只在用户正看着发起请求的 tab 时生效；\
         用户在看别的 tab 或文件未打开时会返回说明，此时应改用文字告知路径与行号。",
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "已打开文件的绝对路径；未打开的文件不会被读取或创建。"
                },
                "line": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-based 行号，默认 1。"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        }),
    )
    .with_permission_scope(ToolPermissionScope::Network)
}

export_plugin!(EdenPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    /// 工具表面必须稳定：四个工具名与权限 scope 是 manifest 与模型约定的边界。
    #[test]
    fn tool_surface_is_stable() {
        let plugin = EdenPlugin::default();
        let tools = plugin.list_tools();
        let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
        assert_eq!(
            names,
            vec![SELECTION_TOOL, EDITORS_TOOL, CONTEXT_TOOL, REVEAL_TOOL]
        );
        for tool in tools {
            assert_eq!(tool.permission_scope, ToolPermissionScope::Network);
            tool.validate_name().expect("工具名必须合法");
        }
    }

    /// 未知 scope 必须在本地被拒绝，且错误信息给出允许值。
    #[test]
    fn invalid_scope_is_rejected_locally() {
        let mut plugin = EdenPlugin::default();
        let host = agent_plugin::NoopHostApi;
        let call = ToolCall::new("call-1", SELECTION_TOOL, json!({ "scope": "current-tab" }));
        let result = plugin
            .call_tool_with_host(&host, call)
            .expect("应返回工具结果而不是插件错误");
        assert!(result.is_error);
        assert!(
            result.content.to_string().contains("session"),
            "{}",
            result.content
        );
    }

    /// reveal 的路径必须是绝对路径，行号必须从 1 开始。
    #[test]
    fn reveal_rejects_relative_path_and_bad_line() {
        let mut plugin = EdenPlugin::default();
        let host = agent_plugin::NoopHostApi;

        let relative = ToolCall::new("call-2", REVEAL_TOOL, json!({ "path": "a.swift" }));
        let result = plugin
            .call_tool_with_host(&host, relative)
            .expect("应返回工具结果");
        assert!(result.is_error);
        assert!(result.content.to_string().contains("绝对路径"));

        let zero_line = ToolCall::new(
            "call-3",
            REVEAL_TOOL,
            json!({ "path": "/tmp/a.swift", "line": 0 }),
        );
        let result = plugin
            .call_tool_with_host(&host, zero_line)
            .expect("应返回工具结果");
        assert!(result.is_error);
        assert!(result.content.to_string().contains("1-based"));
    }

    /// 传输失败必须给出「确认 Eden 正在运行」这类可执行提示。
    #[test]
    fn transport_failure_mentions_eden_runtime() {
        let call = ToolCall::new("call-4", CONTEXT_TOOL, json!({}));
        let result = failure_result(call, EdenFailure::Transport("连接超时".to_string()));
        assert!(result.is_error);
        let text = result.content.to_string();
        assert!(text.contains("Eden 正在运行"), "{text}");
    }

    /// 焦点在别处时的降级结果必须说明原因与建议文案，而不是报错。
    #[test]
    fn out_of_focus_result_degrades_to_text() {
        let call = ToolCall::new("call-5", REVEAL_TOOL, json!({}));
        let result = out_of_focus_result(call, "/tmp/a.swift", 42);
        assert!(!result.is_error);
        assert_eq!(result.content["revealed"], false);
        assert_eq!(result.content["reason"], "user_in_another_tab");
        assert!(result.content["message"]
            .as_str()
            .expect("应有建议文案")
            .contains("/tmp/a.swift 第 42 行"));
    }
}

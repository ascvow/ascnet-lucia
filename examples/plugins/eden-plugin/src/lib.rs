//! Eden 编辑器上下文插件。
//!
//! 插件把 Eden 的选区协议翻译成 Lucia 的通用「上下文引用」契约：应用按固定节拍调用
//! `context.ref` 服务，拿到用户此刻在编辑器里选中的一段代码，校验后自动生成
//! `[REF#N]` 标签，随下一条消息一起带给模型。
//!
//! # 为什么不是一个工具
//!
//! 选区是**用户此刻的状态**，不是模型决定要去查的东西。做成工具等于要求模型先猜
//! 「我现在该不该读选区」，而用户已经用鼠标表达过意图了。做成引用则相反：用户选中
//! 什么，下一条消息就自动带上什么，模型不需要为此多一轮往返。
//!
//! # 为什么由宿主轮询
//!
//! Eden 协议 v1 只有「客户端请求、服务端应答」，没有事件推送；WASM 插件也没有后台
//! 线程。因此「推送」落在插件与宿主的交界上：插件保留协议细节（握手、令牌、分帧、
//! id 配对、超时），宿主只按节拍索要一份已经成形的快照。Eden 将来支持推送后，只需
//! 让插件在收到事件时更新缓存，服务契约与宿主的调用方式都不用改。
//!
//! # 不做的事
//!
//! 插件不读整个文件、不改编辑器内容、不抢焦点。用户没有选区时返回空快照，而不是
//! 猜一个光标位置塞给模型。

mod client;
mod protocol;

use agent_plugin::{
    anyhow, export_plugin, ActivationContext, AgentPlugin, ContextRef, ContextRefRequest,
    ContextRefSnapshot, PluginHostApi, Result, ServiceCall, ServiceSpec, ToolSpec,
    CONTEXT_REF_SERVICE, CONTEXT_REF_SERVICE_VERSION, MAX_CONTEXT_REF_BODY_BYTES,
    MAX_CONTEXT_REF_ID_BYTES, MAX_CONTEXT_REF_TITLE_BYTES,
};
use client::{EdenClient, EdenConfig};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::path::Path;

/// 协议侧的选区读取工具名。
const SELECTION_RPC: &str = "selection.get";

/// 向 Eden 请求的选区文本上限。
///
/// 略大于引用正文上限：正文还要加位置头与围栏，留出余量后由本地截断兜底，避免
/// 一次请求就逼近宿主 1 MiB 的单帧上限。
const MAX_SELECTION_TEXT: i64 = 24_000;

/// 选区截断后追加的说明。
const TRUNCATION_MARKER: &str = "\n…（选区已截断）";

/// `selection.get` 响应中插件真正需要的字段。
///
/// 不启用 `deny_unknown_fields`：Eden 新增字段属于兼容变更，客户端必须忽略。
#[derive(Debug, Default, Deserialize)]
struct Selection {
    /// 选区所在文件的绝对路径。
    #[serde(default)]
    path: String,
    /// 1-based 起始行；无选区时缺失。
    #[serde(rename = "startLine", default)]
    start_line: Option<i64>,
    /// 1-based 结束行；无选区时缺失。
    #[serde(rename = "endLine", default)]
    end_line: Option<i64>,
    /// 选中的原文；`includeText` 为假时缺失。
    #[serde(default)]
    text: Option<String>,
    /// 语言标识，例如 `swift`。
    #[serde(default)]
    language: Option<String>,
    /// 文件是否有未保存修改。
    #[serde(default)]
    dirty: bool,
    /// 选区所在 tab 的标题。
    #[serde(default)]
    tab: Option<String>,
}

/// Eden 编辑器上下文插件实例。
#[derive(Default)]
struct EdenPlugin {
    /// 长驻客户端；第一个服务调用时按 Eden 注入的环境变量创建。
    client: Option<EdenClient>,
}

impl EdenPlugin {
    /// 取出或惰性创建客户端。
    ///
    /// 不在 `activate` 里连接：Eden 可能没在运行，加载阶段失败会让整个插件被禁用，
    /// 而正确行为是「需要时再连，连不上就返回空快照」。
    fn client(&mut self) -> &mut EdenClient {
        self.client
            .get_or_insert_with(|| EdenClient::new(EdenConfig::from_env()))
    }

    /// 读取一次当前选区快照。
    ///
    /// 任何 Eden 侧失败都退化为空快照：引用是增益信息，插件不可用不应该让应用每次
    /// 轮询都弹一条错误。连接失败、没有编辑器、没有选区都返回「本次没有可引用内容」。
    fn snapshot(&mut self, host: &dyn PluginHostApi) -> ContextRefSnapshot {
        let empty = ContextRefSnapshot::default();
        if self.client().ensure_connected(host).is_err() {
            return empty;
        }
        // capabilities 是唯一可靠的能力来源，不硬编码 Eden 的工具表。
        if self
            .client()
            .welcome()
            .is_some_and(|welcome| !welcome.supports(SELECTION_RPC))
        {
            return empty;
        }
        let request = json!({
            "scope": "session",
            "includeText": true,
            "maxText": MAX_SELECTION_TEXT,
        });
        let Ok(value) = self.client().call(host, SELECTION_RPC, request) else {
            return empty;
        };
        match selection_ref(&value) {
            Some(entry) => ContextRefSnapshot { refs: vec![entry] },
            None => empty,
        }
    }
}

impl AgentPlugin for EdenPlugin {
    /// 读取 Eden 注入的连接参数，并注册上下文引用服务。
    fn activate(&mut self, host: &dyn PluginHostApi, _context: ActivationContext) -> Result<()> {
        self.client = Some(EdenClient::new(EdenConfig::from_env()));
        host.upsert_service(&ServiceSpec {
            name: CONTEXT_REF_SERVICE.to_string(),
            version: CONTEXT_REF_SERVICE_VERSION.to_string(),
            description: Some("把 Eden 当前编辑器选区作为随下一条消息发送的上下文引用".into()),
        })?;
        Ok(())
    }

    /// 注销服务并释放 socket 句柄。
    fn deactivate(&mut self, host: &dyn PluginHostApi) -> Result<()> {
        let _ = host.remove_service(CONTEXT_REF_SERVICE);
        if let Some(client) = self.client.as_mut() {
            client.close(host);
        }
        Ok(())
    }

    /// 不向模型暴露任何工具：选区的读取与注入完全由应用驱动。
    fn list_tools(&self) -> Vec<ToolSpec> {
        Vec::new()
    }

    /// 响应 `context.ref` 快照请求。
    fn handle_service(
        &mut self,
        host: &dyn PluginHostApi,
        call: ServiceCall,
    ) -> Result<serde_json::Value> {
        if call.name != CONTEXT_REF_SERVICE {
            return Err(anyhow!("Eden 插件未实现服务 `{}`", call.name));
        }
        let _: ContextRefRequest = serde_json::from_value(call.payload)
            .map_err(|error| anyhow!("`{CONTEXT_REF_SERVICE}` 请求不合法：{error}"))?;
        Ok(serde_json::to_value(self.snapshot(host))?)
    }
}

/// 把一个 `selection.get` 响应转换成上下文引用。
///
/// 只有「真实存在选区」才返回引用：缺少起止行说明用户没有选中任何文本，此时让插件
/// 猜一个光标行只会把无关内容塞进下一条消息。
fn selection_ref(value: &Value) -> Option<ContextRef> {
    let selection: Selection = serde_json::from_value(value.clone()).ok()?;
    let path = selection.path.trim();
    if !path.starts_with('/') {
        return None;
    }
    let start = selection.start_line.filter(|line| *line >= 1)?;
    let end = selection.end_line.filter(|line| *line >= start)?;
    let text = selection.text.as_deref().unwrap_or_default();
    if text.trim().is_empty() {
        return None;
    }

    Some(ContextRef {
        id: selection_id(path, start, end),
        title: selection_title(path, start, end),
        body: selection_body(&selection, path, start, end, text),
    })
}

/// 生成与选区位置绑定、与正文内容无关的稳定 ID。
///
/// 正文会随用户编辑变化，但「同一段选区」应当继续复用同一个标签；ID 里包含正文
/// 摘要会让用户每敲一个字就多出一个引用。
fn selection_id(path: &str, start: i64, end: i64) -> String {
    let id = format!("selection-{:016x}-{start}-{end}", stable_hash(path));
    id.chars().take(MAX_CONTEXT_REF_ID_BYTES).collect()
}

/// 生成面向用户的一行标题：文件名加行号范围。
fn selection_title(path: &str, start: i64, end: i64) -> String {
    let name = Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string());
    let name = sanitize_inline(&name);
    let name = truncate_chars(&name, MAX_CONTEXT_REF_TITLE_BYTES.saturating_sub(24));
    format!("{name}:{start}-{end}")
}

/// 生成随消息注入的正文。
///
/// 采用带属性的标签包裹原文，模型能直接看出这段代码来自哪个文件、哪几行；正文超限时
/// 在字符边界截断并显式标注，而不是让整条引用被宿主静默丢弃。
fn selection_body(selection: &Selection, path: &str, start: i64, end: i64, text: &str) -> String {
    let mut header = format!(
        "<selection path=\"{}\" lines=\"{start}-{end}\"",
        sanitize_attribute(path)
    );
    if let Some(language) = selection
        .language
        .as_deref()
        .filter(|language| is_plain_token(language))
    {
        let _ = write!(header, " language=\"{language}\"");
    }
    if selection.dirty {
        header.push_str(" dirty=\"true\"");
    }
    if let Some(tab) = selection
        .tab
        .as_deref()
        .filter(|tab| !tab.trim().is_empty())
    {
        let _ = write!(header, " tab=\"{}\"", sanitize_attribute(tab));
    }

    let tail = "\n</selection>";
    let budget = MAX_CONTEXT_REF_BODY_BYTES.saturating_sub(header.len() + tail.len());
    let text = truncate_chars(text, budget);
    format!("{header}>\n{text}{tail}")
}

/// 计算 FNV-1a 64 位摘要，用于生成稳定且长度固定的引用 ID。
fn stable_hash(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 把任意文本压成单行，避免标题或属性破坏注入正文的结构。
fn sanitize_inline(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect::<String>()
        .trim()
        .to_string()
}

/// 转义 XML 风格属性值，防止路径里的引号提前闭合标签。
fn sanitize_attribute(text: &str) -> String {
    sanitize_inline(text)
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 判断语言标识是否可以安全地原样写入属性。
fn is_plain_token(text: &str) -> bool {
    !text.is_empty()
        && text.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+' | b'.' | b'#')
        })
}

/// 在字符边界截断到给定字节预算，截断时追加说明。
fn truncate_chars(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let marker_budget = TRUNCATION_MARKER.len();
    if max_bytes <= marker_budget {
        return TRUNCATION_MARKER.chars().take(max_bytes).collect();
    }
    let budget = max_bytes - marker_budget;
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if index + ch.len_utf8() > budget {
            break;
        }
        end = index + ch.len_utf8();
    }
    let mut truncated = text[..end].to_string();
    truncated.push_str(TRUNCATION_MARKER);
    truncated
}

export_plugin!(EdenPlugin);

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一份 Eden 的选区响应。
    fn selection_json() -> Value {
        json!({
            "path": "/Users/me/proj/Sources/App.swift",
            "startLine": 12,
            "endLine": 20,
            "caretLine": 20,
            "text": "func run() {\n    print(1)\n}\n",
            "truncated": false,
            "dirty": true,
            "language": "swift",
            "tab": "zsh ~/proj",
            "inSessionTab": true,
            "focused": true
        })
    }

    /// 插件不再向模型暴露工具：选区完全由应用驱动的引用通道承担。
    #[test]
    fn no_tools_are_exposed() {
        assert!(EdenPlugin::default().list_tools().is_empty());
    }

    /// 合法选区必须转换成带位置信息的引用。
    #[test]
    fn selection_becomes_context_ref() {
        let entry = selection_ref(&selection_json()).expect("应生成引用");
        assert!(entry.validate().is_ok(), "{entry:?}");
        assert_eq!(entry.title, "App.swift:12-20");
        assert!(entry.body.contains("func run() {"), "{}", entry.body);
        assert!(entry.body.contains("lines=\"12-20\""), "{}", entry.body);
        assert!(entry.body.contains("language=\"swift\""), "{}", entry.body);
        assert!(entry.body.contains("dirty=\"true\""), "{}", entry.body);
    }

    /// 同一段选区的 ID 只与路径和行号有关，正文变化不应产生新标签。
    #[test]
    fn ref_id_is_stable_across_edits() {
        let first = selection_ref(&selection_json()).expect("应生成引用");
        let mut edited = selection_json();
        edited["text"] = json!("func run() {\n    print(2)\n}\n");
        let second = selection_ref(&edited).expect("应生成引用");

        assert_eq!(first.id, second.id);
        assert_ne!(first.body, second.body);

        let mut moved = selection_json();
        moved["startLine"] = json!(13);
        let third = selection_ref(&moved).expect("应生成引用");
        assert_ne!(first.id, third.id, "行号变化应视为另一段选区");
    }

    /// 没有选区、路径非绝对或正文为空都必须返回空快照。
    #[test]
    fn absent_selection_is_ignored() {
        for mutate in [
            |value: &mut Value| {
                value.as_object_mut().expect("对象").remove("startLine");
            },
            |value: &mut Value| {
                value["path"] = json!("Sources/App.swift");
            },
            |value: &mut Value| {
                value["text"] = json!("   \n");
            },
            |value: &mut Value| {
                value["startLine"] = json!(0);
            },
            |value: &mut Value| {
                value["endLine"] = json!(5);
            },
        ] {
            let mut value = selection_json();
            mutate(&mut value);
            assert!(selection_ref(&value).is_none(), "{value}");
        }
    }

    /// 超长选区必须在字符边界截断，并保持整份引用仍然合法。
    #[test]
    fn oversized_selection_is_truncated_within_contract() {
        let mut value = selection_json();
        value["text"] = json!("中".repeat(MAX_CONTEXT_REF_BODY_BYTES));
        let entry = selection_ref(&value).expect("应生成引用");

        assert!(entry.body.len() <= MAX_CONTEXT_REF_BODY_BYTES);
        assert!(entry.body.contains("已截断"));
        assert!(entry.validate().is_ok());
    }

    /// 路径里的引号与新行不得破坏注入正文的结构。
    #[test]
    fn unsafe_paths_are_escaped() {
        let mut value = selection_json();
        value["path"] = json!("/tmp/we\"ird\nname.swift");
        let entry = selection_ref(&value).expect("应生成引用");

        assert!(!entry.body.contains("we\"ird"), "{}", entry.body);
        assert!(!entry.body.contains("\nname"), "{}", entry.body);
        assert!(entry.validate().is_ok());
        assert!(!entry.title.contains('\n'));
    }

    /// 服务名不匹配时必须报错，避免调用方误以为请求已生效。
    #[test]
    fn unknown_service_is_rejected() {
        let mut plugin = EdenPlugin::default();
        let host = agent_plugin::NoopHostApi;
        let error = plugin
            .handle_service(
                &host,
                ServiceCall {
                    caller_id: "lucia-tui".into(),
                    name: "other.service".into(),
                    payload: json!({}),
                },
            )
            .expect_err("未知服务应失败");
        assert!(error.to_string().contains("未实现服务"), "{error}");
    }

    /// 宿主不可用时返回空快照而不是错误：引用失败不应打扰用户。
    #[test]
    fn unavailable_host_yields_empty_snapshot() {
        let mut plugin = EdenPlugin::default();
        let host = agent_plugin::NoopHostApi;
        let value = plugin
            .handle_service(
                &host,
                ServiceCall {
                    caller_id: "lucia-tui".into(),
                    name: CONTEXT_REF_SERVICE.into(),
                    payload: json!({}),
                },
            )
            .expect("空快照应是正常结果");
        let snapshot: ContextRefSnapshot = serde_json::from_value(value).expect("应返回快照结构");
        assert!(snapshot.refs.is_empty());
    }

    /// 请求契约拒绝未知字段，防止调用方以为可选参数生效了。
    #[test]
    fn unknown_request_fields_are_rejected() {
        let mut plugin = EdenPlugin::default();
        let host = agent_plugin::NoopHostApi;
        assert!(plugin
            .handle_service(
                &host,
                ServiceCall {
                    caller_id: "lucia-tui".into(),
                    name: CONTEXT_REF_SERVICE.into(),
                    payload: json!({ "scope": "session" }),
                },
            )
            .is_err());
    }
}

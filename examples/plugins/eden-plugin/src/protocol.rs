//! Eden 编辑器上下文协议（v1）的帧与报文编解码。
//!
//! 这一层只做纯逻辑：字节 → 报文、报文 → 字节，以及稳定错误码的类型化表示。
//! socket 与文件访问全部留在 `client`，因此协议解析可以脱离宿主单独测试——
//! 协议是 Eden 侧最可能先变化的部分，必须能在没有 socket 的情况下回归。

use anyhow::{anyhow, Result};
use serde_json::Value;

/// 当前实现的协议版本。
pub(crate) const PROTOCOL_VERSION: u64 = 1;

/// 协议允许的单行上限；超过即视为对端违约，直接断开。
pub(crate) const MAX_LINE_BYTES: usize = 1024 * 1024;

/// 把一条报文编码成协议要求的「单行 JSON + `\n`」。
///
/// `Value::to_string` 输出紧凑 JSON 且转义换行，因此正文里的 `\n` 不会破坏分帧。
pub(crate) fn encode_line(message: &Value) -> String {
    let mut line = message.to_string();
    line.push('\n');
    line
}

/// 从接收缓冲中取出一条完整行。
///
/// 返回 `None` 表示还没有换行，调用方应继续读取；一次 `read` 可能只拿到半行，也可能
/// 一次拿到多行，因此缓冲与切分必须和读取解耦。超限时直接报错，避免把攻击性超长行
/// 一直攒在内存里。
pub(crate) fn take_line(buffer: &mut Vec<u8>) -> Result<Option<String>> {
    let Some(index) = buffer.iter().position(|byte| *byte == b'\n') else {
        if buffer.len() > MAX_LINE_BYTES {
            return Err(anyhow!("Eden 单行超过 {MAX_LINE_BYTES} 字节限制"));
        }
        return Ok(None);
    };
    if index > MAX_LINE_BYTES {
        return Err(anyhow!("Eden 单行超过 {MAX_LINE_BYTES} 字节限制"));
    }
    let line: Vec<u8> = buffer.drain(..=index).collect();
    let text = String::from_utf8(line[..index].to_vec())
        .map_err(|error| anyhow!("Eden 返回了非 UTF-8 数据：{error}"))?;
    Ok(Some(text))
}

/// 解析一条报文，并校验它是带 `op` 的 JSON 对象。
///
/// 未知字段一律忽略：协议 v1 明确新增字段属于兼容变更，客户端不能因为服务器多说了一句
/// 就整体失败。
pub(crate) fn parse_message(line: &str) -> Result<Value> {
    let value: Value =
        serde_json::from_str(line).map_err(|error| anyhow!("Eden 返回了非法 JSON：{error}"))?;
    if !value.is_object() {
        return Err(anyhow!("Eden 返回的帧不是 JSON 对象"));
    }
    if value.get("op").and_then(Value::as_str).is_none() {
        return Err(anyhow!("Eden 返回的帧缺少 op 字段"));
    }
    Ok(value)
}

/// Eden 的结构化错误：`{code, message, hint?}`。
///
/// `code` 是稳定标识，`message` 给人看，`hint` 给模型看下一步怎么办。三者都要保留：
/// 丢掉 hint 等于让模型自己猜怎么纠错。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdenError {
    /// 稳定错误码，例如 `no_editor` 或 `user_in_another_tab`。
    pub(crate) code: String,
    /// 面向用户的说明。
    pub(crate) message: String,
    /// 面向模型的下一步建议。
    pub(crate) hint: Option<String>,
}

impl EdenError {
    /// 从 `result` 帧或帧级 `error` 报文中解析错误对象。
    pub(crate) fn from_value(value: &Value) -> Option<Self> {
        let error = value.get("error")?;
        let code = error.get("code").and_then(Value::as_str)?.to_string();
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let hint = error
            .get("hint")
            .and_then(Value::as_str)
            .map(str::to_string);
        Some(Self {
            code,
            message,
            hint,
        })
    }

    /// 生成交给模型的单行说明，保留 hint。
    pub(crate) fn describe(&self) -> String {
        match &self.hint {
            Some(hint) => format!(
                "Eden 返回 `{}`：{}（建议：{}）",
                self.code, self.message, hint
            ),
            None => format!("Eden 返回 `{}`：{}", self.code, self.message),
        }
    }

    /// 是否为「稍后重试可能成功」的错误。
    pub(crate) fn is_retryable(&self) -> bool {
        self.code == "busy"
    }
}

/// `welcome` 报文中客户端真正需要的部分。
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Welcome {
    /// 服务端实际解析到的会话 id。
    pub(crate) session: Option<String>,
    /// 服务端声明支持的工具名；是唯一可靠的能力来源。
    pub(crate) capabilities: Vec<String>,
    /// 服务端解析出的作用域原始对象。
    pub(crate) scope: Value,
}

impl Welcome {
    /// 解析并校验握手响应。
    pub(crate) fn parse(value: &Value) -> Result<Self> {
        if value.get("op").and_then(Value::as_str) != Some("welcome") {
            let op = value
                .get("op")
                .and_then(Value::as_str)
                .unwrap_or("<缺少 op>");
            return Err(anyhow!("Eden 握手失败：期望 welcome，收到 `{op}`"));
        }
        if let Some(version) = value.get("v").and_then(Value::as_u64) {
            if version != PROTOCOL_VERSION {
                return Err(anyhow!(
                    "Eden 协议版本不匹配：期望 {PROTOCOL_VERSION}，收到 {version}"
                ));
            }
        }
        let capabilities = value
            .get("capabilities")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        Ok(Self {
            session: value
                .get("session")
                .and_then(Value::as_str)
                .map(str::to_string),
            capabilities,
            scope: value.get("scope").cloned().unwrap_or(Value::Null),
        })
    }

    /// 服务端是否声明支持某个协议工具名。
    ///
    /// capabilities 为空表示服务端没有声明（旧实现）；此时放行交给服务端自己拒绝，
    /// 而不是在客户端伪造一份硬编码工具表。
    pub(crate) fn supports(&self, tool: &str) -> bool {
        self.capabilities.is_empty() || self.capabilities.iter().any(|entry| entry == tool)
    }

    /// 握手时用户是否正看着本会话所在的 tab。
    pub(crate) fn focused(&self) -> Option<bool> {
        self.scope.get("focused").and_then(Value::as_bool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 半行必须先留在缓冲里，等下一个换行到达再拼成完整报文。
    #[test]
    fn partial_line_is_buffered_until_newline() {
        let mut buffer = b"{\"op\":\"wel".to_vec();
        assert_eq!(take_line(&mut buffer).expect("半行不应报错"), None);
        assert_eq!(buffer, b"{\"op\":\"wel");

        buffer.extend_from_slice(b"come\"}\n");
        assert_eq!(
            take_line(&mut buffer).expect("完整行应可解析").as_deref(),
            Some("{\"op\":\"welcome\"}")
        );
        assert!(buffer.is_empty());
    }

    /// 一次读取里包含多行时必须逐条切分。
    #[test]
    fn multiple_lines_are_split_one_by_one() {
        let mut buffer = b"{\"op\":\"a\"}\n{\"op\":\"b\"}\n{\"op\":".to_vec();
        assert_eq!(
            take_line(&mut buffer).expect("第一行").as_deref(),
            Some("{\"op\":\"a\"}")
        );
        assert_eq!(
            take_line(&mut buffer).expect("第二行").as_deref(),
            Some("{\"op\":\"b\"}")
        );
        assert_eq!(take_line(&mut buffer).expect("第三行不完整"), None);
        assert_eq!(buffer, b"{\"op\":");
    }

    /// 超长单行必须报错，而不是无限攒内存。
    #[test]
    fn oversized_line_is_rejected() {
        let mut buffer = vec![b'x'; MAX_LINE_BYTES + 1];
        let error = take_line(&mut buffer).expect_err("超限行必须失败");
        assert!(format!("{error:#}").contains("字节限制"), "{error:#}");
    }

    /// 编码后的报文必须是一行，内部换行要被转义。
    #[test]
    fn encoded_message_stays_on_one_line() {
        let line = encode_line(&json!({ "op": "call", "args": { "text": "a\nb" } }));
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.ends_with('\n'));
        let parsed = parse_message(line.trim_end()).expect("编码结果应可解析");
        assert_eq!(parsed["args"]["text"], "a\nb");
    }

    /// 缺失 `op` 的帧不是合法报文。
    #[test]
    fn message_without_op_is_rejected() {
        assert!(parse_message(r#"{"id":1}"#).is_err());
        assert!(parse_message("[]").is_err());
        assert!(parse_message("not json").is_err());
    }

    /// welcome 必须接受新增字段，并保留 capabilities 与 scope。
    #[test]
    fn welcome_accepts_additive_fields() {
        let value = json!({
            "op": "welcome",
            "v": 1,
            "session": "8C0F",
            "capabilities": ["selection.get", "context.get", "future.tool"],
            "scope": { "resolved": true, "focused": false, "tab": "zsh ~/proj" },
            "future_field": { "anything": true }
        });

        let welcome = Welcome::parse(&value).expect("应接受新增字段");
        assert_eq!(welcome.session.as_deref(), Some("8C0F"));
        assert!(welcome.supports("selection.get"));
        assert!(welcome.supports("future.tool"));
        assert!(!welcome.supports("editor.reveal"));
        assert_eq!(welcome.focused(), Some(false));
    }

    /// 协议版本不一致必须明确失败，而不是按 v1 猜着解析。
    #[test]
    fn welcome_rejects_other_protocol_versions() {
        let value = json!({ "op": "welcome", "v": 2 });
        let error = Welcome::parse(&value).expect_err("版本不匹配应失败");
        assert!(format!("{error:#}").contains("版本不匹配"), "{error:#}");
    }

    /// 帧级错误也要保留 hint，模型才能自我纠错。
    #[test]
    fn frame_error_keeps_hint() {
        let value = json!({
            "op": "error",
            "error": {
                "code": "unauthorized",
                "message": "bad token",
                "hint": "重新读取令牌后重连"
            }
        });

        let error = EdenError::from_value(&value).expect("应解析出错误");
        assert_eq!(error.code, "unauthorized");
        assert!(error.describe().contains("重新读取令牌"));
    }

    /// 失败结果缺少 error 对象时由调用方补默认值，不能静默当成成功。
    #[test]
    fn missing_error_object_is_not_success() {
        let value = json!({ "op": "result", "id": 1, "ok": false });
        assert!(EdenError::from_value(&value).is_none());
    }
}

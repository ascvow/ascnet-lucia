//! Guest 返回值与 UI 贡献的宿主校验。

use crate::ui::ToolRendererContribution;
use crate::ui::UiContribution;
use crate::ui::UiDeclaration;
use crate::ui::UiPlacement;
use crate::wasm::linker::PLUGIN_HOST_FAILURE_DETAIL_KEY;
use agent_core::model::{ContentBlock, ModelMessage};
use agent_tool::ToolCall;
use agent_tool::ToolErrorKind;
use agent_tool::ToolResult;
use agent_tool::ToolSpec;
use anyhow::anyhow;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
/// 删除 Guest 可能伪造的 Host 专用故障键，并降级其安全错误类别。
///
/// 参数错误、普通执行错误与取消仍用于 Agent 侧行为归因；只有必须由可信执行层观察的
/// 权限与边界类别会降为普通执行错误。
pub(crate) fn sanitize_guest_tool_result(result: &mut ToolResult) {
    result.error_kind = if !result.is_error {
        None
    } else {
        match result.error_kind {
            Some(
                ToolErrorKind::PermissionDenied
                | ToolErrorKind::PathBoundaryViolation
                | ToolErrorKind::ProcessBoundaryViolation
                | ToolErrorKind::SecretAccessAttempt
                | ToolErrorKind::PolicyDenied,
            )
            | None => Some(ToolErrorKind::Execution),
            trusted_behavior_error => trusted_behavior_error,
        }
    };
    let Some(details) = result.details.as_mut().and_then(Value::as_object_mut) else {
        return;
    };
    details.remove(PLUGIN_HOST_FAILURE_DETAIL_KEY);
    if details.is_empty() {
        result.details = None;
    }
}

/// 使用 Host 已知的真实调用身份覆盖 Guest 返回值中的可伪造字段。
pub(crate) fn bind_guest_tool_result(result: &mut ToolResult, call: &ToolCall, public_name: &str) {
    sanitize_guest_tool_result(result);
    result.call_id.clone_from(&call.id);
    result.name = public_name.to_string();
}

/// 从跨 WASM 边界的上下文中移除服务商私有推理，避免 Guest 读取或重新注入。
///
/// 原生 ContextLoader 不经过此函数，仍可在可信 Core 内保留同路由工具循环所需的签名。
pub(crate) fn redact_guest_context_messages(messages: &mut Vec<ModelMessage>) {
    for message in messages.iter_mut() {
        message
            .content
            .retain(|block| !matches!(block, ContentBlock::Thinking { .. }));
    }
    messages.retain(|message| !message.content.is_empty());
}

/// 校验插件内部视图 ID，并注入可信的 manifest 插件 ID。
pub(crate) fn validate_ui_contributions(
    plugin_id: &str,
    contributions: Vec<UiContribution>,
    tools: &[ToolSpec],
) -> Result<(Vec<UiDeclaration>, Vec<ToolRendererContribution>)> {
    let mut view_ids = HashSet::new();
    let owned_tools = tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect::<HashSet<_>>();
    let mut declarations = Vec::new();
    let mut tool_renderers = Vec::new();
    for contribution in contributions {
        match contribution {
            UiContribution::View(mut declaration) => {
                if declaration.view_id.trim().is_empty() {
                    return Err(anyhow!("plugin `{plugin_id}` has an empty UI view id"));
                }
                if !view_ids.insert(declaration.view_id.clone()) {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` declares duplicate UI route `{}`",
                        declaration.view_id
                    ));
                }
                declaration.plugin_id = plugin_id.to_string();
                declarations.push(declaration);
            }
            UiContribution::ToolRenderer(mut renderer) => {
                if renderer.renderer_id.trim().is_empty() {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` has an empty tool renderer id"
                    ));
                }
                if !view_ids.insert(renderer.renderer_id.clone()) {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` declares duplicate UI route `{}`",
                        renderer.renderer_id
                    ));
                }
                if !owned_tools.contains(renderer.tool_name.as_str()) {
                    return Err(anyhow!(
                        "plugin `{plugin_id}` cannot render unowned tool `{}`",
                        renderer.tool_name
                    ));
                }
                renderer.plugin_id = plugin_id.to_string();
                tool_renderers.push(renderer);
            }
        }
    }
    Ok((declarations, tool_renderers))
}

/// 限制 UI 载荷的字节、行和 span 数量，避免小视口触发无界宿主布局。
pub(super) fn decode_ui_frame(json: &str) -> Result<crate::ui::UiFrame> {
    if json.len() > 1024 * 1024 {
        return Err(anyhow!("插件 UI 帧超过 1 MiB 限制"));
    }
    let frame: crate::ui::UiFrame = serde_json::from_str(json)?;
    if frame.lines.len() > 4096 || frame.lines.iter().any(|line| line.spans.len() > 1024) {
        return Err(anyhow!("插件 UI 帧超过 4096 行或每行 1024 span 限制"));
    }
    Ok(frame)
}

/// 校验静态视图与动态子视图的实例路由是否匹配。
pub(crate) fn validate_ui_instance(
    declaration: &UiDeclaration,
    instance_id: Option<&str>,
) -> Result<()> {
    match (declaration.placement, instance_id) {
        (UiPlacement::Subview, Some(instance_id))
            if !instance_id.is_empty()
                && instance_id.len() <= 256
                && !instance_id.chars().any(char::is_control) =>
        {
            Ok(())
        }
        (UiPlacement::Subview, _) => Err(anyhow!(
            "subview `{}` 需要有效 instance_id",
            declaration.view_id
        )),
        (_, None) => Ok(()),
        (_, Some(_)) => Err(anyhow!(
            "非 subview `{}` 不允许携带 instance_id",
            declaration.view_id
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// UI 资源边界不改变旧帧和加法字段兼容性。
    #[test]
    fn ui_frame_bounds_preserve_compatible_requests() {
        assert!(decode_ui_frame(r#"{"view_id":"v","lines":[],"future_field":true}"#).is_ok());
        assert!(decode_ui_frame(&" ".repeat(1024 * 1024 + 1)).is_err());
        let oversized = json!({"view_id":"v", "lines": vec![json!({"spans": []}); 4097]});
        assert!(decode_ui_frame(&oversized.to_string()).is_err());
    }

    /// Guest 上下文必须移除所有不透明推理块，并保留普通可见内容。
    #[test]
    fn guest_context_redacts_all_private_reasoning_blocks() {
        let message: ModelMessage = serde_json::from_value(json!({
            "role": "assistant",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "私有推理",
                    "signature": "sig-private"
                },
                {
                    "type": "thinking",
                    "thinking": "",
                    "signature": "opaque-private-envelope"
                },
                {"type": "text", "text": "可见内容"}
            ]
        }))
        .expect("测试消息应符合 Core 协议");
        let mut messages = vec![message];

        redact_guest_context_messages(&mut messages);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].text_content(), "可见内容");
        assert_eq!(messages[0].content.len(), 1);
    }
}

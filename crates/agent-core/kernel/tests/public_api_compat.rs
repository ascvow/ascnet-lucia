//! `agent-core` 对外内容块协议的源码兼容性回归测试。

use agent_core::model::ContentBlock;

/// 模拟外部 crate 对公开内容块做穷举匹配。
///
/// 新增公开变体或给既有变体追加字段会使本测试在外部 crate 语境下编译失败，提醒维护者
/// 先选择不改变公开枚举形状的内部信封，或显式安排协议版本升级。
fn classify_content_block(block: ContentBlock) -> &'static str {
    match block {
        ContentBlock::Text { text: _ } => "text",
        ContentBlock::Thinking {
            thinking: _,
            signature: _,
        } => "thinking",
        ContentBlock::ToolCall { call: _ } => "tool_call",
        ContentBlock::ToolResult { result: _ } => "tool_result",
        ContentBlock::Image {
            media_type: _,
            data: _,
        } => "image",
        ContentBlock::File {
            name: _,
            media_type: _,
            data: _,
        } => "file",
    }
}

/// 既有两字段 `Thinking` 构造式和穷举匹配必须继续可用。
#[test]
fn thinking_keeps_existing_public_shape() {
    let block = ContentBlock::Thinking {
        thinking: "内部分析".to_string(),
        signature: Some("opaque-signature".to_string()),
    };

    assert_eq!(classify_content_block(block), "thinking");
}

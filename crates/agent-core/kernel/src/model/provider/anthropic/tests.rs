//! Anthropic Messages 协议转换测试。

use super::*;

/// 图片附件映射为 Anthropic base64 image 块。
#[test]
fn image_block_maps_to_base64_image() {
    let blocks = vec![ContentBlock::Image {
        media_type: "image/png".to_string(),
        data: "aGVsbG8=".to_string(),
    }];

    let out = blocks_to_anthropic_content(&blocks, false);

    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["type"], "image");
    assert_eq!(out[0]["source"]["type"], "base64");
    assert_eq!(out[0]["source"]["media_type"], "image/png");
    assert_eq!(out[0]["source"]["data"], "aGVsbG8=");
}

/// PDF 附件映射为原生 document 块并携带标题。
#[test]
fn pdf_file_maps_to_document() {
    let blocks = vec![ContentBlock::File {
        name: "报告.pdf".to_string(),
        media_type: "application/pdf".to_string(),
        data: "cGRm".to_string(),
    }];

    let out = blocks_to_anthropic_content(&blocks, false);

    assert_eq!(out[0]["type"], "document");
    assert_eq!(out[0]["title"], "报告.pdf");
    assert_eq!(out[0]["source"]["media_type"], "application/pdf");
}

/// 文本附件降级为内联文本块，保留文件名和内容。
#[test]
fn text_file_is_inlined_as_text() {
    // "你好" 的 UTF-8 base64。
    let blocks = vec![ContentBlock::File {
        name: "note.txt".to_string(),
        media_type: "text/plain".to_string(),
        data: "5L2g5aW9".to_string(),
    }];

    let out = blocks_to_anthropic_content(&blocks, false);

    assert_eq!(out[0]["type"], "text");
    let text = out[0]["text"].as_str().expect("应为文本");
    assert!(text.contains("note.txt"));
    assert!(text.contains("你好"));
}

/// 二进制附件不会被静默丢弃，而是生成占位说明。
#[test]
fn binary_file_becomes_placeholder_text() {
    let blocks = vec![ContentBlock::File {
        name: "app.bin".to_string(),
        media_type: "application/octet-stream".to_string(),
        data: "AAEC".to_string(),
    }];

    let out = blocks_to_anthropic_content(&blocks, false);

    assert_eq!(out[0]["type"], "text");
    let text = out[0]["text"].as_str().expect("应为文本");
    assert!(text.contains("app.bin"));
    assert!(text.contains("无法内联发送"));
}

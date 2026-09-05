//! Vue/nvue 单文件组件适配：HTML 分词保留原始字节位置，脚本交给 Oxc。

use super::ParseOutput;
use html5gum::{DefaultEmitter, Token, Tokenizer};

/// 解析顶层 JS/TS/setup/renderjs 脚本；未知语言、外链脚本和失败区域留给文本兜底。
/// 不执行组件代码，也不读取外部 script src。多个脚本保留各自的原始行号。
pub(super) fn parse(source: &str) -> Option<ParseOutput> {
    let mut emitter = DefaultEmitter::<usize>::new_with_span();
    emitter.naively_switch_states(true);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut active = None;
    let mut ranges = Vec::new();
    for token in Tokenizer::new_with_emitter(source, emitter) {
        match token.ok()? {
            Token::StartTag(tag) => {
                let name: &[u8] = &tag.name;
                if stack.is_empty() && name == b"script" && !tag.self_closing {
                    let src = tag.attributes.contains_key(b"src".as_slice());
                    let lang = tag.attributes.get(b"lang".as_slice()).map(|a| a.as_ref());
                    let kind = tag.attributes.get(b"type".as_slice()).map(|a| a.as_ref());
                    let supported_lang = matches!(
                        lang,
                        None | Some(
                            b"js"
                                | b"jsx"
                                | b"ts"
                                | b"tsx"
                                | b"javascript"
                                | b"typescript"
                                | b"renderjs"
                        )
                    );
                    let supported_type = matches!(
                        kind,
                        None | Some(b"module" | b"text/javascript" | b"application/javascript")
                    );
                    if !src && supported_lang && supported_type {
                        active = Some(tag.span.end);
                    }
                }
                if !tag.self_closing
                    && !matches!(
                        name,
                        b"area"
                            | b"base"
                            | b"br"
                            | b"col"
                            | b"embed"
                            | b"hr"
                            | b"img"
                            | b"input"
                            | b"link"
                            | b"meta"
                            | b"param"
                            | b"source"
                            | b"track"
                            | b"wbr"
                    )
                {
                    stack.push(name.to_vec());
                }
            }
            Token::EndTag(tag) => {
                if tag.name.as_ref() == b"script" {
                    if let Some(start) = active.take() {
                        ranges.push((start, tag.span.start));
                    }
                }
                if let Some(index) = stack
                    .iter()
                    .rposition(|name| name.as_slice() == tag.name.as_ref())
                {
                    stack.truncate(index);
                }
            }
            _ => {}
        }
    }
    if let Some(start) = active {
        ranges.push((start, source.len()));
    }
    let mut output = ParseOutput::default();
    for (start, end) in ranges {
        let mut masked = source
            .as_bytes()
            .iter()
            .map(|b| if matches!(b, b'\n' | b'\r') { *b } else { b' ' })
            .collect::<Vec<_>>();
        masked
            .get_mut(start..end)?
            .copy_from_slice(source.as_bytes().get(start..end)?);
        let masked = String::from_utf8(masked).ok()?;
        if let Some(mut parsed) = super::typescript::parse(&masked, true) {
            let base = output.declarations.len();
            for declaration in &mut parsed.declarations {
                declaration.parent = declaration.parent.map(|parent| parent + base);
            }
            output.declarations.extend(parsed.declarations);
            for import in parsed.imports {
                if !output.imports.contains(&import) {
                    output.imports.push(import);
                }
            }
        }
    }
    Some(output)
}

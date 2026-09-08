//! 单文件组件（Vue / Svelte）与 HTML 内嵌代码解析。
//!
//! 使用 HTML 语法识别真实 `<script>` 与 `<style>` 元素，交给 TypeScript / JavaScript / CSS
//! 适配层解析，再把子解析结果的字节偏移与行号平移回外层文件坐标。
//!
//! 注释、属性值中的伪标签以及 JSON 数据脚本不会成为代码定义。
//! 模板区域由 [`crate::chunker`] 的未覆盖区间保留为文本块，内容不会丢失。

use crate::error::{Error, Result};
use crate::parser::{Definition, ParsedFile};
use crate::types::Language;

/// 解析单文件组件。
pub fn parse(source: &str) -> Result<ParsedFile> {
    let mut parsed = ParsedFile::default();
    for block in find_blocks(source)? {
        let content = &source[block.content_start..block.content_end];
        let mut inner = super::parse(block.language, content)?;
        let line_offset = source[..block.content_start].matches('\n').count() as u32;
        for definition in &mut inner.definitions {
            shift_definition(definition, block.content_start, line_offset);
        }
        for import in &mut inner.imports {
            import.start_byte += block.content_start;
            import.end_byte += block.content_start;
        }
        parsed.has_errors |= inner.has_errors;
        parsed.definitions.append(&mut inner.definitions);
        parsed.imports.append(&mut inner.imports);
    }
    Ok(parsed)
}

/// 一个内嵌块及其真实语言。
#[derive(Debug, Clone, Copy)]
struct Block {
    language: Language,
    content_start: usize,
    content_end: usize,
}

/// 把子解析结果的坐标平移到外层文件。
///
/// 内容是逐字节原样嵌入的，所以行号只需加上块起始处之前的换行数。
fn shift_definition(definition: &mut Definition, byte_offset: usize, line_offset: u32) {
    definition.start_byte += byte_offset;
    definition.end_byte += byte_offset;
    definition.start_line += line_offset;
    definition.end_line += line_offset;
    if let Some(body_start) = definition.body_start_byte.as_mut() {
        *body_start += byte_offset;
    }
    for point in &mut definition.split_points {
        *point += byte_offset;
    }
    for child in &mut definition.children {
        shift_definition(child, byte_offset, line_offset);
    }
}

/// 使用语法树扫描内嵌块，闭合缺失、外部脚本与非代码 MIME 类型不参与声明提取。
fn find_blocks(source: &str) -> Result<Vec<Block>> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&tree_sitter_html::LANGUAGE.into())
        .map_err(|error| Error::Internal(format!("加载 HTML 语法失败: {error}")))?;
    let Some(tree) = parser.parse(source, None) else {
        return Ok(Vec::new());
    };
    let mut blocks = Vec::new();
    let mut pending = vec![tree.root_node()];
    while let Some(node) = pending.pop() {
        if matches!(node.kind(), "script_element" | "style_element") {
            let mut cursor = node.walk();
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            let open = children.iter().find(|child| child.kind() == "start_tag");
            let close = children
                .iter()
                .find(|child| child.kind() == "end_tag" && !child.is_missing());
            if let (Some(open), Some(close)) = (open, close) {
                if let Some(language) =
                    block_language(source, *open, node.kind() == "script_element")
                {
                    blocks.push(Block {
                        language,
                        content_start: open.end_byte(),
                        content_end: close.start_byte(),
                    });
                }
            }
            continue;
        }
        // 注释、属性、原始文本中的标签只是文本，不递归扫描。
        if matches!(node.kind(), "comment" | "attribute" | "raw_text") {
            continue;
        }
        let mut cursor = node.walk();
        pending.extend(node.named_children(&mut cursor));
    }
    blocks.sort_by_key(|block| block.content_start);
    Ok(blocks)
}

/// 按完整属性名解析语言，data-lang 与属性值中的 lang 不会影响判断。
fn block_language(source: &str, open: tree_sitter::Node<'_>, script: bool) -> Option<Language> {
    let mut lang = None;
    let mut mime = None;
    let mut cursor = open.walk();
    for attr in open
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "attribute")
    {
        let name = attr.named_child(0)?;
        let value = attr
            .named_child(1)
            .map(|value| {
                source[value.byte_range()]
                    .trim_matches(['\'', '"'])
                    .trim()
                    .to_ascii_lowercase()
            })
            .unwrap_or_default();
        match source[name.byte_range()].to_ascii_lowercase().as_str() {
            "src" => return None,
            "lang" => lang = Some(value),
            "type" => mime = Some(value),
            _ => {}
        }
    }
    if script {
        if mime.as_deref().is_some_and(|mime| {
            !matches!(
                mime,
                "" | "module"
                    | "text/javascript"
                    | "application/javascript"
                    | "text/ecmascript"
                    | "application/ecmascript"
            )
        }) {
            return None;
        }
        match lang.as_deref() {
            None | Some("" | "js" | "javascript" | "jsx") => Some(Language::JavaScript),
            Some("ts" | "typescript") => Some(Language::TypeScript),
            Some("tsx") => Some(Language::Tsx),
            _ => None,
        }
    } else {
        match lang.as_deref() {
            None | Some("" | "css" | "scss" | "less" | "sass" | "postcss") => Some(Language::Css),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::parser::parse;
    use crate::types::{ChunkKind, Language};

    const VUE: &str = r#"<template>
  <div class="login">
    <button class="btn-primary" @click="submit">登录</button>
  </div>
</template>

<script setup lang="ts">
import { ref } from "vue";
import { useUserStore } from "@/store/useUserStore";

const username = ref("");

/** 提交登录表单。 */
async function submit(): Promise<void> {
  const store = useUserStore();
  await store.login(username.value);
}

export class FormHelper {
  validate(): boolean {
    return true;
  }
}
</script>

<style scoped lang="scss">
.login {
  display: flex;
}

.btn-primary {
  color: red;
}
</style>
"#;

    #[test]
    fn vue_extracts_symbols_from_script_and_style() {
        let parsed = parse(Language::Vue, VUE).expect("parse");
        let names: Vec<&str> = parsed
            .definitions
            .iter()
            .map(|d| d.qualified.as_str())
            .collect();
        assert!(names.contains(&"submit"), "{names:?}");
        assert!(names.contains(&"FormHelper"), "{names:?}");
        assert!(names.contains(&"username"), "{names:?}");
        // 样式块的选择器同样成为符号。
        assert!(names.contains(&".login"), "{names:?}");
        assert!(names.contains(&".btn-primary"), "{names:?}");

        // 方法也要提取到。
        let helper = parsed
            .definitions
            .iter()
            .find(|d| d.name == "FormHelper")
            .expect("FormHelper");
        assert_eq!(helper.children[0].qualified, "FormHelper.validate");

        // script 块里的导入被记录。
        let modules: Vec<&str> = parsed
            .imports
            .iter()
            .filter_map(|i| i.module.as_deref())
            .collect();
        assert_eq!(modules, vec!["vue", "@/store/useUserStore"]);
    }

    /// 行号与字节范围必须落回外层文件坐标，否则 read_code 会切出错误位置。
    #[test]
    fn vue_offsets_map_back_to_the_outer_file() {
        let parsed = parse(Language::Vue, VUE).expect("parse");
        for definition in &parsed.definitions {
            let text = &VUE[definition.start_byte..definition.end_byte];
            assert_eq!(
                definition.end_line - definition.start_line + 1,
                text.lines().count() as u32,
                "定义 {} 行号与内容不一致",
                definition.name
            );
            // 用行号独立还原一次，确认与字节范围指向同一段。
            let by_line: String = VUE
                .lines()
                .skip(definition.start_line as usize - 1)
                .take((definition.end_line - definition.start_line + 1) as usize)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                by_line.contains(text.trim_end()),
                "定义 {} 的行号与字节范围不指向同一段",
                definition.name
            );
        }

        let submit = parsed
            .definitions
            .iter()
            .find(|d| d.name == "submit")
            .expect("submit");
        // 文档注释也要一起平移。
        assert!(VUE[submit.start_byte..submit.end_byte].starts_with("/** 提交登录表单。 */"));
        assert_eq!(submit.kind, ChunkKind::Function);
    }

    #[test]
    fn svelte_script_and_style_are_parsed() {
        let source = r#"<script>
  export function increment() {
    count += 1;
  }
</script>

<button on:click={increment}>+</button>

<style>
  button { color: red; }
</style>
"#;
        let parsed = parse(Language::Svelte, source).expect("parse");
        let names: Vec<&str> = parsed
            .definitions
            .iter()
            .map(|d| d.qualified.as_str())
            .collect();
        assert!(names.contains(&"increment"), "{names:?}");
        assert!(names.contains(&"button"), "{names:?}");
    }

    /// 没有 script 块、或标签不闭合时都不能 panic。
    #[test]
    fn malformed_sfc_is_handled_gracefully() {
        for source in [
            "<template><div>only markup</div></template>\n",
            "<script>\nfunction a() {}\n",
            "<script lang=\"coffee\">\nx = 1\n</script>\n",
            "",
            "<scriptx>not a script</scriptx>",
        ] {
            let parsed = parse(Language::Vue, source).expect("解析不得返回错误");
            for definition in &parsed.definitions {
                assert!(source
                    .get(definition.start_byte..definition.end_byte)
                    .is_some());
            }
        }
    }

    /// 多个 script 块（Vue 2 选项式 + setup）都要解析。
    #[test]
    fn multiple_script_blocks_are_all_parsed() {
        let source = r#"<script>
export default { name: "Widget" };
</script>

<script setup>
function inner() {}
</script>
"#;
        let parsed = parse(Language::Vue, source).expect("parse");
        let names: Vec<&str> = parsed.definitions.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"inner"), "{names:?}");
    }
}

//! 单文件组件（Vue / Svelte）解析。
//!
//! SFC 本身不是一种语法，而是若干内嵌块的容器。这里不引入 SFC 专用语法，
//! 而是切分出 `<script>` 与 `<style>` 块，交给已有的 TypeScript / JavaScript / CSS
//! 适配层解析，再把子解析结果的字节偏移与行号平移回外层文件坐标。
//!
//! 只识别 `<script>` 和 `<style>`，不解析 `<template>`：模板不会嵌套出现 script 或
//! style，因此无需处理标签嵌套；模板区域由 [`crate::chunker`] 的未覆盖区间兜底成
//! 文本块，内容不会丢失。

use crate::error::Result;
use crate::parser::{Definition, ParsedFile};
use crate::types::Language;

/// 解析单文件组件。
pub fn parse(source: &str) -> Result<ParsedFile> {
    let mut parsed = ParsedFile::default();
    for block in find_blocks(source) {
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

/// 扫描出全部 `<script>` 与 `<style>` 块。
///
/// 大小写不敏感，允许任意属性。开始标签以第一个 `>` 结束——属性值里带 `>` 的
/// 写法在 SFC 中不成立，因此不做引号状态机。
fn find_blocks(source: &str) -> Vec<Block> {
    let lowered = source.to_ascii_lowercase();
    let mut blocks = Vec::new();
    let mut cursor = 0usize;
    while cursor < lowered.len() {
        let Some((tag_start, tag)) = next_tag(&lowered, cursor) else {
            break;
        };
        // 开始标签的结束位置。
        let Some(open_end) = lowered[tag_start..].find('>').map(|at| tag_start + at + 1) else {
            break;
        };
        let close = format!("</{}", tag.name);
        let Some(content_end) = lowered[open_end..].find(&close).map(|at| open_end + at) else {
            // 没有闭合标签时放弃该块，避免把文件尾部整段误当成代码。
            cursor = open_end;
            continue;
        };
        let attributes = &source[tag_start..open_end];
        if let Some(language) = (tag.resolve)(attributes) {
            blocks.push(Block {
                language,
                content_start: open_end,
                content_end,
            });
        }
        cursor = content_end + close.len();
    }
    blocks
}

/// 待扫描的标签种类。
struct TagKind {
    name: &'static str,
    /// 由开始标签的属性文本判断内嵌语言；返回 `None` 表示不做结构化解析。
    resolve: fn(&str) -> Option<Language>,
}

fn next_tag(lowered: &str, from: usize) -> Option<(usize, TagKind)> {
    const TAGS: &[TagKind] = &[
        TagKind {
            name: "script",
            resolve: script_language,
        },
        TagKind {
            name: "style",
            resolve: style_language,
        },
    ];
    let mut best: Option<(usize, TagKind)> = None;
    for tag in TAGS {
        // 只匹配 `<script` 后紧跟空白、`>` 或 `/` 的位置，避免命中 `<scriptx`。
        let mut search = from;
        while let Some(at) = lowered[search..].find(&format!("<{}", tag.name)) {
            let start = search + at;
            let after = start + 1 + tag.name.len();
            let next = lowered.as_bytes().get(after).copied();
            if matches!(
                next,
                Some(b' ') | Some(b'\t') | Some(b'\n') | Some(b'\r') | Some(b'>') | Some(b'/')
            ) {
                if best.as_ref().is_none_or(|(current, _)| start < *current) {
                    best = Some((
                        start,
                        TagKind {
                            name: tag.name,
                            resolve: tag.resolve,
                        },
                    ));
                }
                break;
            }
            search = after;
        }
    }
    best
}

/// `<script lang="ts">` 决定用哪套 JS 系语法解析。
fn script_language(attributes: &str) -> Option<Language> {
    let language = match lang_attribute(attributes).as_deref() {
        Some("ts") | Some("typescript") => Language::TypeScript,
        Some("tsx") => Language::Tsx,
        Some("jsx") => Language::JavaScript,
        // 未标注 lang 的 Vue / Svelte script 块默认是 JavaScript。
        None | Some("js") | Some("javascript") => Language::JavaScript,
        // 其他方言（coffee 等）没有对应语法，交给文本兜底。
        Some(_) => return None,
    };
    Some(language)
}

/// `<style lang="scss">` 等预处理器方言仍按 CSS 解析。
///
/// CSS 语法能覆盖嵌套与变量之外的绝大多数结构；解析出错时定义列表为空，
/// 由文本兜底接管，不会丢内容。
fn style_language(attributes: &str) -> Option<Language> {
    match lang_attribute(attributes).as_deref() {
        None | Some("css") | Some("scss") | Some("less") | Some("sass") | Some("postcss") => {
            Some(Language::Css)
        }
        Some(_) => None,
    }
}

/// 取出 `lang="xxx"` / `lang='xxx'` / `lang=xxx` 的值。
fn lang_attribute(attributes: &str) -> Option<String> {
    let at = attributes.to_ascii_lowercase().find("lang")?;
    let rest = attributes[at + "lang".len()..].trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let value = match rest.chars().next()? {
        quote @ ('"' | '\'') => rest[1..].split(quote).next()?,
        _ => rest
            .split(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .next()?,
    };
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() {
        None
    } else {
        Some(value)
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

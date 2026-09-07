//! 结构化分块与 Embedding 输入模板。
//!
//! 分块规则：
//!
//! 1. 普通函数和方法尽量完整保留（含绑定的文档注释与属性）。
//! 2. 超长定义只在 Tree-sitter 给出的语句边界上递归拆分。
//! 3. 拆分后的块保留所属函数、类型与模块信息。
//! 4. 小型相邻声明在相同父作用域下合并。
//! 5. 容器（impl / class / trait / module）体量大时只生成签名概览，
//!    子定义单独成块，避免父节点与每个子节点重复索引同一段代码。
//! 6. Markdown 按标题边界分块，其他文本使用有界文本兜底。
//! 7. 任何切分都发生在行边界上，因此不会切断 UTF-8 字符。

use crate::config::ChunkConfig;
use crate::error::Result;
use crate::parser::{Definition, ParsedFile};
use crate::retrieval::tokenize::build_fts_text;
use crate::store::{ImportRecord, PreparedChunk, SymbolRecord};
use crate::types::{estimate_tokens, ChunkKind, Language};

/// Embedding 输入模板版本。修改模板结构必须递增本常量，
/// 因为它参与 embedding space 身份，旧向量会因此自动失效。
pub const EMBED_TEMPLATE_VERSION: u32 = 1;

/// 分块输入。
#[derive(Debug, Clone, Copy)]
pub struct ChunkContext<'a> {
    /// `/` 分隔的相对路径。
    pub rel_path: &'a str,
    /// 语言。
    pub language: Language,
    /// 完整源码快照。
    pub source: &'a str,
    /// 分块参数。
    pub config: &'a ChunkConfig,
    /// 文档输入前缀（来自 embedding 配置）。
    pub document_prefix: &'a str,
    /// 单个模型输入的最大字符数。
    pub max_input_chars: usize,
}

/// 对一个文件分块。
///
/// 结构化语言且提取到定义时走结构化分块；否则按语言退回 Markdown 或文本兜底，
/// 因此解析失败的文件不会从索引中消失。
pub fn chunk_file(ctx: &ChunkContext<'_>, parsed: &ParsedFile) -> Result<Vec<PreparedChunk>> {
    let mut raw = Vec::new();
    if ctx.language.is_structured() && !parsed.is_empty() {
        if !parsed.imports.is_empty() {
            raw.extend(imports_chunks(ctx, parsed));
        }
        let mut definitions: Vec<&Definition> = parsed.definitions.iter().collect();
        definitions.sort_by_key(|d| d.start_byte);
        emit_definitions(ctx, &definitions, &mut raw);
        raw.extend(uncovered_text_chunks(ctx, &raw));
    } else if ctx.language == Language::Markdown {
        raw.extend(markdown_chunks(ctx));
    } else {
        raw.extend(text_chunks(ctx, 0, ctx.source.len(), None));
    }

    // 兜底：结构化解析没有覆盖到任何内容时，不允许文件变成空索引。
    if raw.is_empty() && !ctx.source.trim().is_empty() {
        raw.extend(text_chunks(ctx, 0, ctx.source.len(), None));
    }

    raw.sort_by(|a, b| {
        a.start_byte
            .cmp(&b.start_byte)
            .then(a.end_byte.cmp(&b.end_byte))
    });
    Ok(finalize(ctx, raw))
}

/// 尚未渲染模型输入的中间块。
#[derive(Debug, Clone)]
struct RawChunk {
    kind: ChunkKind,
    symbol: Option<String>,
    parent_symbol: Option<String>,
    module_path: Option<String>,
    signature: Option<String>,
    doc: Option<String>,
    start_byte: usize,
    end_byte: usize,
    /// 概览块使用合成文本，其余块直接切片源码。
    synthetic: Option<String>,
    symbols: Vec<SymbolRecord>,
}

fn emit_definitions(ctx: &ChunkContext<'_>, definitions: &[&Definition], out: &mut Vec<RawChunk>) {
    let mut index = 0usize;
    while index < definitions.len() {
        let definition = definitions[index];
        // 相邻小型叶子声明合并成一个块，减少碎片。
        if definition.children.is_empty() && is_mergeable(ctx, definition) {
            let mut end = index;
            let mut total = tokens_of(ctx, definition);
            while end + 1 < definitions.len() {
                let next = definitions[end + 1];
                if !next.children.is_empty() || !is_mergeable(ctx, next) {
                    break;
                }
                if next.parent != definition.parent {
                    break;
                }
                let next_tokens = tokens_of(ctx, next);
                if total + next_tokens > ctx.config.target_tokens {
                    break;
                }
                total += next_tokens;
                end += 1;
            }
            if end > index {
                out.push(merged_chunk(ctx, &definitions[index..=end]));
                index = end + 1;
                continue;
            }
        }
        emit_definition(ctx, definition, out);
        index += 1;
    }
}

fn emit_definition(ctx: &ChunkContext<'_>, definition: &Definition, out: &mut Vec<RawChunk>) {
    let tokens = tokens_of(ctx, definition);
    if definition.children.is_empty() {
        if tokens <= ctx.config.max_tokens && definition.byte_len() <= ctx.config.max_chars {
            out.push(leaf_chunk(ctx, definition));
        } else {
            split_long_definition(ctx, definition, out);
        }
        return;
    }

    // 容器整体不大时作为单块保留，保持代码可读性。
    if tokens <= ctx.config.target_tokens && definition.byte_len() <= ctx.config.max_chars {
        out.push(leaf_chunk(ctx, definition));
        return;
    }

    // 体量大的容器只生成签名概览，子定义各自成块。
    out.push(overview_chunk(definition));
    let mut children: Vec<&Definition> = definition.children.iter().collect();
    children.sort_by_key(|child| child.start_byte);
    emit_definitions(ctx, &children, out);
}

/// 沿语句边界把超长定义拆成若干部分。
fn split_long_definition(ctx: &ChunkContext<'_>, definition: &Definition, out: &mut Vec<RawChunk>) {
    let header_end = definition
        .body_start_byte
        .unwrap_or(definition.start_byte)
        .max(definition.start_byte);
    // 语句节点从缩进之后开始，因此把边界回退到行首，
    // 让缩进跟随它所属的语句，块也保持整行对齐。
    let mut boundaries: Vec<usize> = definition
        .split_points
        .iter()
        .copied()
        .filter(|point| *point > header_end && *point < definition.end_byte)
        .map(|point| snap_to_line_start(ctx.source, point, header_end))
        .filter(|point| *point > header_end)
        .collect();
    boundaries.sort_unstable();
    boundaries.dedup();

    if boundaries.is_empty() {
        // 没有可用语法边界时退回按行切分，仍然不会切断字符。
        out.extend(text_chunks(
            ctx,
            definition.start_byte,
            definition.end_byte,
            Some(definition),
        ));
        return;
    }

    // 段边界包含头部与结尾，逐段累加直到接近上限。
    let mut segments: Vec<(usize, usize)> = Vec::new();
    let mut segment_start = definition.start_byte;
    let mut cursor = 0usize;
    while cursor < boundaries.len() {
        let mut segment_end = boundaries[cursor];
        // 尽量让每段接近 target_tokens。
        while cursor + 1 < boundaries.len() {
            let candidate = boundaries[cursor + 1];
            let tokens = estimate_tokens(slice(ctx.source, segment_start, candidate));
            if tokens > ctx.config.target_tokens || candidate - segment_start > ctx.config.max_chars
            {
                break;
            }
            segment_end = candidate;
            cursor += 1;
        }
        segments.push((segment_start, segment_end));
        segment_start = segment_end;
        cursor += 1;
    }
    if segment_start < definition.end_byte {
        segments.push((segment_start, definition.end_byte));
    }

    for (part_index, (start, end)) in segments.iter().enumerate() {
        let symbol = definition
            .qualified
            .clone()
            .then_some_part(part_index, segments.len());
        out.push(RawChunk {
            kind: ChunkKind::Part,
            symbol: Some(symbol),
            // 拆分块保留所属函数、类型与模块信息。
            parent_symbol: Some(definition.qualified.clone()),
            module_path: definition.module_path.clone(),
            signature: definition.signature.clone(),
            doc: if part_index == 0 {
                definition.doc.clone()
            } else {
                None
            },
            start_byte: *start,
            end_byte: *end,
            synthetic: None,
            // 拆分块不重复注册符号，避免符号表出现多条同名记录。
            symbols: Vec::new(),
        });
    }
}

/// 给拆分块生成稳定的展示名，例如 `RetryPolicy::execute (part 2/3)`。
trait PartLabel {
    fn then_some_part(self, index: usize, total: usize) -> String;
}

impl PartLabel for String {
    fn then_some_part(self, index: usize, total: usize) -> String {
        format!("{self} (part {}/{total})", index + 1)
    }
}

fn leaf_chunk(ctx: &ChunkContext<'_>, definition: &Definition) -> RawChunk {
    RawChunk {
        kind: definition.kind,
        symbol: Some(definition.qualified.clone()),
        parent_symbol: definition.parent.clone(),
        module_path: definition.module_path.clone(),
        signature: definition.signature.clone(),
        doc: definition.doc.clone(),
        start_byte: definition.start_byte,
        end_byte: definition.end_byte,
        synthetic: None,
        symbols: collect_symbols(definition, ctx.language),
    }
}

/// 容器概览：只包含容器签名与子定义签名，不重复子定义的完整代码。
fn overview_chunk(definition: &Definition) -> RawChunk {
    let mut text = String::new();
    if let Some(doc) = &definition.doc {
        for line in doc.lines() {
            text.push_str("// ");
            text.push_str(line);
            text.push('\n');
        }
    }
    if let Some(signature) = &definition.signature {
        text.push_str(signature);
        text.push_str(" {\n");
    } else {
        text.push_str(&definition.name);
        text.push_str(" {\n");
    }
    for child in &definition.children {
        text.push_str("    ");
        text.push_str(child.signature.as_deref().unwrap_or(child.name.as_str()));
        text.push_str(";\n");
    }
    text.push_str("}\n");
    RawChunk {
        kind: ChunkKind::Overview,
        symbol: Some(definition.qualified.clone()),
        parent_symbol: definition.parent.clone(),
        module_path: definition.module_path.clone(),
        signature: definition.signature.clone(),
        doc: definition.doc.clone(),
        // 概览块的行号仍指向容器的真实起止位置。
        start_byte: definition.start_byte,
        end_byte: definition.end_byte,
        synthetic: Some(text),
        symbols: own_symbol(definition),
    }
}

fn merged_chunk(ctx: &ChunkContext<'_>, definitions: &[&Definition]) -> RawChunk {
    let first = definitions[0];
    let last = definitions[definitions.len() - 1];
    let names: Vec<String> = definitions.iter().map(|d| d.name.clone()).collect();
    let mut symbols = Vec::new();
    for definition in definitions {
        symbols.extend(collect_symbols(definition, ctx.language));
    }
    RawChunk {
        kind: first.kind,
        symbol: Some(names.join(", ")),
        parent_symbol: first.parent.clone(),
        module_path: first.module_path.clone(),
        signature: None,
        doc: first.doc.clone(),
        start_byte: first.start_byte,
        end_byte: last.end_byte,
        synthetic: None,
        symbols,
    }
}

/// 导入块：合成文本用于 Embedding 与 FTS，字节范围指向导入语句在源码中的真实位置。
///
/// 范围必须真实，否则按引用回读时会切出空内容。
///
/// 每一段**连续**的导入产出一个块。不能把全文件的导入合成单个 `min..max` 区间：
/// `mod tests { use super::*; }` 这类尾部导入会让区间跨越中间上千行无关代码，
/// 回读时切出整个文件，并在检索结果里吃掉整个 token 预算。
fn imports_chunks(ctx: &ChunkContext<'_>, parsed: &ParsedFile) -> Vec<RawChunk> {
    let mut ordered: Vec<&ImportRecord> = parsed.imports.iter().collect();
    ordered.sort_by_key(|import| import.start_byte);

    let mut chunks = Vec::new();
    let mut group: Vec<&ImportRecord> = Vec::new();
    for import in ordered {
        if let Some(previous) = group.last() {
            // 两条导入之间夹着真实代码时断开分组，只允许空白、注释与属性。
            let gap = ctx
                .source
                .get(previous.end_byte..import.start_byte)
                .unwrap_or("");
            if !is_import_gap(gap) {
                chunks.push(finish_imports_group(ctx, &group));
                group.clear();
            }
        }
        group.push(import);
    }
    if !group.is_empty() {
        chunks.push(finish_imports_group(ctx, &group));
    }
    chunks
}

/// 判断两条导入之间的文本是否只有空白、注释与属性。
fn is_import_gap(gap: &str) -> bool {
    gap.lines().all(|line| {
        let trimmed = line.trim();
        trimmed.is_empty()
            || trimmed.starts_with("//")
            || trimmed.starts_with("/*")
            || trimmed.starts_with('*')
            || trimmed.starts_with("*/")
            || trimmed.starts_with("#[")
            || trimmed.starts_with("#![")
    })
}

/// 把一组连续导入收敛成一个块。
fn finish_imports_group(ctx: &ChunkContext<'_>, group: &[&ImportRecord]) -> RawChunk {
    let text = group
        .iter()
        .map(|import| import.raw.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let start = group
        .iter()
        .map(|import| import.start_byte)
        .min()
        .unwrap_or(0)
        .min(ctx.source.len());
    let end = group
        .iter()
        .map(|import| import.end_byte)
        .max()
        .unwrap_or(0)
        .clamp(start, ctx.source.len());
    RawChunk {
        kind: ChunkKind::Imports,
        symbol: None,
        parent_symbol: None,
        module_path: None,
        signature: None,
        doc: None,
        start_byte: start,
        end_byte: end,
        synthetic: Some(text),
        symbols: Vec::new(),
    }
}

/// Markdown 按标题边界分块；超长小节继续按行兜底切分。
fn markdown_chunks(ctx: &ChunkContext<'_>) -> Vec<RawChunk> {
    let source = ctx.source;
    let mut sections: Vec<(usize, usize, String)> = Vec::new();
    let mut current_start = 0usize;
    let mut current_title = String::new();
    let mut offset = 0usize;
    let mut in_fence = false;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }
        // 代码围栏内的 `#` 不是标题。
        if !in_fence && trimmed.starts_with('#') {
            if offset > current_start {
                sections.push((current_start, offset, std::mem::take(&mut current_title)));
            }
            current_start = offset;
            current_title = trimmed.trim_start_matches('#').trim().to_string();
        }
        offset += line.len();
    }
    if current_start < source.len() {
        sections.push((current_start, source.len(), current_title));
    }
    if sections.is_empty() {
        return text_chunks(ctx, 0, source.len(), None);
    }

    let mut out = Vec::new();
    for (start, end, title) in sections {
        let text = slice(source, start, end);
        if text.trim().is_empty() {
            continue;
        }
        if estimate_tokens(text) > ctx.config.max_tokens || end - start > ctx.config.max_chars {
            let mut parts = text_chunks(ctx, start, end, None);
            for part in &mut parts {
                part.kind = ChunkKind::Section;
                part.symbol = Some(title.clone());
            }
            out.extend(parts);
        } else {
            out.push(RawChunk {
                kind: ChunkKind::Section,
                symbol: if title.is_empty() {
                    None
                } else {
                    Some(title.clone())
                },
                parent_symbol: None,
                module_path: None,
                signature: None,
                doc: None,
                start_byte: start,
                end_byte: end,
                synthetic: None,
                symbols: Vec::new(),
            });
        }
    }
    out
}

/// 为结构化解析未覆盖的区间补文本块。
///
/// 结构化解析只产出声明。顶层表达式语句不是声明，因此
/// `router.beforeEach(...)`、`service.interceptors.request.use(...)`、
/// `app.use(pinia)` 这类代码不会进入任何块，整段从索引中消失——
/// 而这正是前端项目里最常被搜索的接线逻辑。
///
/// 这里把未覆盖且含有效内容的区间按文本兜底切分，保证任何语言都不丢内容。
/// 空白与纯符号区间由 [`text_chunks`] 自行跳过。
fn uncovered_text_chunks(ctx: &ChunkContext<'_>, covered_by: &[RawChunk]) -> Vec<RawChunk> {
    let mut spans: Vec<(usize, usize)> = covered_by
        .iter()
        .filter(|chunk| chunk.end_byte > chunk.start_byte)
        .map(|chunk| (chunk.start_byte, chunk.end_byte))
        .collect();
    spans.sort_unstable();

    let mut out = Vec::new();
    let mut cursor = 0usize;
    for (start, end) in spans {
        if start > cursor {
            out.extend(text_chunks(ctx, cursor, start, None));
        }
        cursor = cursor.max(end);
    }
    if cursor < ctx.source.len() {
        out.extend(text_chunks(ctx, cursor, ctx.source.len(), None));
    }
    out
}

/// 有界文本兜底分块：始终在行边界切分。
fn text_chunks(
    ctx: &ChunkContext<'_>,
    start: usize,
    end: usize,
    owner: Option<&Definition>,
) -> Vec<RawChunk> {
    let source = ctx.source;
    let region = slice(source, start, end);
    if region.trim().is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut chunk_start = start;
    let mut chunk_tokens = 0usize;
    let mut offset = start;
    let target = ctx.config.target_tokens.max(1);
    for line in region.split_inclusive('\n') {
        let line_tokens = estimate_tokens(line);
        let would_exceed = chunk_tokens + line_tokens > target
            || offset + line.len() - chunk_start > ctx.config.max_chars;
        if would_exceed && offset > chunk_start {
            out.push(text_chunk(ctx, chunk_start, offset, owner, out.len()));
            chunk_start = offset;
            chunk_tokens = 0;
        }
        chunk_tokens += line_tokens;
        offset += line.len();
    }
    if offset > chunk_start {
        out.push(text_chunk(ctx, chunk_start, offset, owner, out.len()));
    }
    out
}

fn text_chunk(
    _ctx: &ChunkContext<'_>,
    start: usize,
    end: usize,
    owner: Option<&Definition>,
    index: usize,
) -> RawChunk {
    match owner {
        Some(definition) => RawChunk {
            kind: ChunkKind::Part,
            symbol: Some(format!("{} (part {})", definition.qualified, index + 1)),
            parent_symbol: Some(definition.qualified.clone()),
            module_path: definition.module_path.clone(),
            signature: definition.signature.clone(),
            doc: if index == 0 {
                definition.doc.clone()
            } else {
                None
            },
            start_byte: start,
            end_byte: end,
            synthetic: None,
            symbols: Vec::new(),
        },
        None => RawChunk {
            kind: ChunkKind::Text,
            symbol: None,
            parent_symbol: None,
            module_path: None,
            signature: None,
            doc: None,
            start_byte: start,
            end_byte: end,
            synthetic: None,
            symbols: Vec::new(),
        },
    }
}

/// 收集一个代码块应登记的全部符号，含嵌套子定义。
///
/// 必须递归：容器体量不大时会作为单块保留（见 [`emit_definition`]），此时方法的
/// 完整代码就在这个块里，若只登记容器名，`RetryPolicy::execute` 这类限定名查询
/// 将完全无法命中。概览块不使用本函数，见 [`own_symbol`]。
fn collect_symbols(definition: &Definition, _language: Language) -> Vec<SymbolRecord> {
    let mut out = Vec::new();
    push_symbols(definition, &mut out);
    out
}

fn push_symbols(definition: &Definition, out: &mut Vec<SymbolRecord>) {
    out.push(SymbolRecord {
        name: definition.name.clone(),
        qualified: definition.qualified.clone(),
        kind: definition.kind,
    });
    for child in &definition.children {
        push_symbols(child, out);
    }
}

/// 只登记定义自身的符号，不含子定义。
///
/// 概览块只保存子定义的签名而非实现，子定义各自成块并登记自己的符号；
/// 若概览块也登记子符号，同一个符号会指向两个块，符号查询将返回没有实现的概览。
fn own_symbol(definition: &Definition) -> Vec<SymbolRecord> {
    vec![SymbolRecord {
        name: definition.name.clone(),
        qualified: definition.qualified.clone(),
        kind: definition.kind,
    }]
}

fn is_mergeable(ctx: &ChunkContext<'_>, definition: &Definition) -> bool {
    matches!(
        definition.kind,
        ChunkKind::Const | ChunkKind::TypeAlias | ChunkKind::Struct | ChunkKind::Enum
    ) && tokens_of(ctx, definition) <= ctx.config.merge_below_tokens
}

fn tokens_of(ctx: &ChunkContext<'_>, definition: &Definition) -> usize {
    estimate_tokens(slice(
        ctx.source,
        definition.start_byte,
        definition.end_byte,
    ))
}

/// 把偏移回退到所在行的行首。
///
/// 只跨越空白字符：如果同一行行首之后还有代码，说明该位置不是独立语句的开头，
/// 此时保持原偏移不动，避免把前一条语句的代码并进下一块。
fn snap_to_line_start(source: &str, offset: usize, lower_bound: usize) -> usize {
    let bytes = source.as_bytes();
    let mut index = offset.min(bytes.len());
    while index > lower_bound {
        match bytes[index - 1] {
            b' ' | b'\t' => index -= 1,
            b'\n' => return index,
            _ => return offset,
        }
    }
    offset
}

/// 安全切片：越界或非字符边界时返回空串而不是 panic。
fn slice(source: &str, start: usize, end: usize) -> &str {
    if start > end || end > source.len() {
        return "";
    }
    source.get(start..end).unwrap_or("")
}

/// 把中间块渲染为最终的 [`PreparedChunk`]，包括行号、模型输入与 FTS 拆词文本。
fn finalize(ctx: &ChunkContext<'_>, raw: Vec<RawChunk>) -> Vec<PreparedChunk> {
    // 预先计算换行位置，把字节偏移映射到 1 起始行号。
    let line_starts = line_starts(ctx.source);
    let mut out = Vec::with_capacity(raw.len());
    for (ordinal, chunk) in raw.into_iter().enumerate() {
        let code = match &chunk.synthetic {
            Some(text) => text.clone(),
            None => slice(ctx.source, chunk.start_byte, chunk.end_byte).to_string(),
        };
        if code.trim().is_empty() {
            continue;
        }
        let start_line = line_of(&line_starts, chunk.start_byte);
        let end_line = if chunk.end_byte > chunk.start_byte {
            // 末尾换行不应把行号多算一行。
            let last = chunk.end_byte - 1;
            line_of(&line_starts, last)
        } else {
            start_line
        };
        let embed_input = render_embed_input(ctx, &chunk, &code);
        let (embed_input, truncated) = truncate_input(&embed_input, ctx.max_input_chars);
        // 代码块定义的每个符号都要进入 symbol_terms：容器作为单块保留时，
        // 方法名只出现在 code_terms 里会让 `execute` 这类查询退化成普通全文匹配。
        let mut symbol_fields: Vec<Option<&str>> = vec![
            chunk.symbol.as_deref(),
            chunk.parent_symbol.as_deref(),
            chunk.module_path.as_deref(),
            chunk.signature.as_deref(),
        ];
        symbol_fields.extend(
            chunk
                .symbols
                .iter()
                .map(|record| Some(record.qualified.as_str())),
        );
        let fts = build_fts_text(ctx.rel_path, &symbol_fields, chunk.doc.as_deref(), &code);
        out.push(PreparedChunk {
            ordinal,
            kind: chunk.kind,
            symbol: chunk.symbol.clone(),
            symbol_key: chunk
                .symbol
                .as_ref()
                .map(|symbol| format!("{}:{}#{}", ctx.language.as_str(), ctx.rel_path, symbol)),
            parent_symbol: chunk.parent_symbol,
            module_path: chunk.module_path,
            signature: chunk.signature,
            doc: chunk.doc,
            start_byte: chunk.start_byte,
            end_byte: chunk.end_byte,
            start_line,
            end_line,
            token_estimate: estimate_tokens(&code),
            content_hash: blake3::hash(code.as_bytes()).to_hex().to_string(),
            embed_input,
            embed_input_truncated: truncated,
            fts,
            symbols: chunk.symbols,
        });
    }
    out
}

/// 渲染确定性的 Embedding 文档输入。
///
/// 行号不进入模型输入；路径进入输入，因此重命名会改变缓存键，这是预期行为。
fn render_embed_input(ctx: &ChunkContext<'_>, chunk: &RawChunk, code: &str) -> String {
    let mut input = String::with_capacity(code.len() + 256);
    if !ctx.document_prefix.is_empty() {
        input.push_str(ctx.document_prefix);
        input.push('\n');
    }
    input.push_str("Language: ");
    input.push_str(ctx.language.as_str());
    input.push_str("\nPath: ");
    input.push_str(ctx.rel_path);
    if let Some(module) = &chunk.module_path {
        input.push_str("\nModule: ");
        input.push_str(module);
    }
    if let Some(symbol) = &chunk.symbol {
        input.push_str("\nSymbol: ");
        input.push_str(symbol);
    }
    if let Some(parent) = &chunk.parent_symbol {
        input.push_str("\nParent: ");
        input.push_str(parent);
    }
    input.push_str("\nKind: ");
    input.push_str(chunk.kind.as_str());
    if let Some(signature) = &chunk.signature {
        input.push_str("\nSignature: ");
        input.push_str(signature);
    }
    if let Some(doc) = &chunk.doc {
        input.push_str("\nDocumentation: ");
        input.push_str(doc);
    }
    input.push_str("\nCode:\n");
    input.push_str(code);
    input
}

/// 按字符数上限截断模型输入，优先在行边界切断。
fn truncate_input(input: &str, max_chars: usize) -> (String, bool) {
    if input.chars().count() <= max_chars {
        return (input.to_string(), false);
    }
    let truncated: String = input.chars().take(max_chars).collect();
    // 回退到最后一个换行，避免把一行代码切成两半。
    let cut = truncated.rfind('\n').unwrap_or(truncated.len());
    let head = &truncated[..cut];
    (head.to_string(), true)
}

fn line_starts(source: &str) -> Vec<usize> {
    let mut starts = vec![0usize];
    for (index, byte) in source.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(index + 1);
        }
    }
    starts
}

/// 由字节偏移求 1 起始行号。
fn line_of(line_starts: &[usize], byte: usize) -> u32 {
    match line_starts.binary_search(&byte) {
        Ok(index) => index as u32 + 1,
        Err(index) => index as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn context<'a>(
        path: &'a str,
        language: Language,
        source: &'a str,
        config: &'a ChunkConfig,
    ) -> ChunkContext<'a> {
        ChunkContext {
            rel_path: path,
            language,
            source,
            config,
            document_prefix: "",
            max_input_chars: 16_000,
        }
    }

    fn chunk(
        path: &str,
        language: Language,
        source: &str,
        config: &ChunkConfig,
    ) -> Vec<PreparedChunk> {
        let parsed = parse(language, source).expect("parse");
        let ctx = context(path, language, source, config);
        chunk_file(&ctx, &parsed).expect("chunk")
    }

    /// 尾部 `mod tests { use super::*; }` 不能让导入块吞掉中间的全部代码。
    ///
    /// 曾经的实现取全文件导入的 `min(start)..max(end)`，于是一个 3000 行的文件
    /// 会产出一个跨越 1600 行的导入块：回读时切出整个文件，检索时吃光 token 预算。
    #[test]
    fn trailing_test_imports_do_not_swallow_the_file() {
        let config = ChunkConfig::default();
        let mut source = String::from("use std::io;\nuse std::fmt;\n\n");
        for index in 0..40 {
            source.push_str(&format!(
                "/// 第 {index} 个函数。\npub fn helper_{index}(value: u32) -> u32 {{\n    value + {index}\n}}\n\n"
            ));
        }
        source.push_str("#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn works() {\n        assert_eq!(helper_0(1), 1);\n    }\n}\n");

        let chunks = chunk("src/lib.rs", Language::Rust, &source, &config);
        let imports: Vec<&PreparedChunk> = chunks
            .iter()
            .filter(|chunk| chunk.kind == ChunkKind::Imports)
            .collect();

        assert!(
            imports.len() >= 2,
            "顶部与 mod tests 内的导入应分成不同的块，实际 {} 个",
            imports.len()
        );
        let total: usize = source.len();
        for chunk in &imports {
            let span = chunk.end_byte - chunk.start_byte;
            assert!(
                span * 4 < total,
                "导入块跨度 {span} 字节，接近整文件 {total} 字节，说明又把中间代码吞进去了"
            );
        }
        // 顶部那组必须仍然只覆盖开头的两行导入。
        let head = imports
            .iter()
            .min_by_key(|chunk| chunk.start_byte)
            .expect("至少一个导入块");
        assert!(
            source[head.start_byte..head.end_byte].contains("use std::fmt"),
            "顶部导入块应覆盖开头的连续导入"
        );
        assert!(
            !source[head.start_byte..head.end_byte].contains("helper_10"),
            "顶部导入块不应包含函数体"
        );
    }

    /// 顶层表达式语句不是声明，但必须进入索引。
    ///
    /// 回归：结构化解析只产出声明，`router.beforeEach(...)`、
    /// `service.interceptors.request.use(...)`、`app.use(pinia)` 这类接线代码
    /// 不落在任何块里，整段从索引消失。实测三个前端项目的 JS 覆盖率只有 87%~91%，
    /// 缺的正好是最常被搜索的部分。
    #[test]
    fn top_level_statements_are_indexed_not_dropped() {
        let config = ChunkConfig::default();
        let source = r#"import { createRouter } from "vue-router";

const router = createRouter({ routes: [] });

router.beforeEach((to, from, next) => {
  if (!localStorage.getItem("token")) {
    return next("/login");
  }
  next();
});

service.interceptors.response.use((response) => {
  if (response.data.code === 401) {
    redirectToLogin();
  }
  return response;
});

export default router;
"#;
        let chunks = chunk("src/router/index.js", Language::JavaScript, source, &config);
        assert_lines_match(source, &chunks);

        // 逐行覆盖：除空行外，每一行都必须落在某个块里。
        let total = source.lines().count();
        let mut covered = vec![false; total + 1];
        for chunk in &chunks {
            for line in chunk.start_line..=chunk.end_line.min(total as u32) {
                covered[line as usize] = true;
            }
        }
        for (index, line) in source.lines().enumerate() {
            let number = index + 1;
            if line.trim().is_empty() {
                continue;
            }
            assert!(covered[number], "第 {number} 行未进入任何块: {line}");
        }

        // 关键接线代码必须能在某个块的正文里找到。
        let joined: String = chunks
            .iter()
            .map(|chunk| source[chunk.start_byte..chunk.end_byte].to_string())
            .collect::<Vec<_>>()
            .join("\n");
        for needle in ["router.beforeEach", "interceptors.response", "code === 401"] {
            assert!(joined.contains(needle), "索引内容缺少 `{needle}`");
        }
    }

    /// 所有块的行号必须与内容严格对应，这是结果可信的前提。
    fn assert_lines_match(source: &str, chunks: &[PreparedChunk]) {
        let lines: Vec<&str> = source.split_inclusive('\n').collect();
        for chunk in chunks {
            if chunk.start_byte == chunk.end_byte {
                continue; // 合成块（imports）没有源码范围。
            }
            let code = &source[chunk.start_byte..chunk.end_byte];
            let expected: String =
                lines[(chunk.start_line as usize - 1)..(chunk.end_line as usize)].concat();
            assert!(
                expected.contains(code.trim_end()),
                "块 {:?} 行号 {}..{} 与内容不一致\n块内容: {code:?}\n行内容: {expected:?}",
                chunk.symbol,
                chunk.start_line,
                chunk.end_line
            );
        }
    }

    #[test]
    fn small_impl_stays_as_one_chunk() {
        let source = "/// 文档\nimpl A {\n    fn a(&self) {}\n}\n";
        let config = ChunkConfig::default();
        let chunks = chunk("src/a.rs", Language::Rust, source, &config);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].kind, ChunkKind::Impl);
        assert_eq!(chunks[0].start_line, 1);
        assert_lines_match(source, &chunks);
    }

    #[test]
    fn large_impl_becomes_overview_plus_methods() {
        let filler = "        let value = compute_something_long(value_a, value_b);\n".repeat(12);
        let source = format!(
            "impl RetryPolicy {{\n    /// 执行。\n    pub fn execute(&self) {{\n{filler}    }}\n\n    pub fn backoff(&self) {{\n{filler}    }}\n}}\n"
        );
        let config = ChunkConfig {
            target_tokens: 60,
            max_tokens: 200,
            ..ChunkConfig::default()
        };
        let chunks = chunk("src/retry.rs", Language::Rust, &source, &config);
        let kinds: Vec<ChunkKind> = chunks.iter().map(|c| c.kind).collect();
        assert!(
            kinds.contains(&ChunkKind::Overview),
            "应生成容器概览: {kinds:?}"
        );
        assert_eq!(
            kinds.iter().filter(|k| **k == ChunkKind::Method).count(),
            2,
            "两个方法应各自成块"
        );

        // 概览块只含签名，不重复方法体。
        let overview = chunks
            .iter()
            .find(|c| c.kind == ChunkKind::Overview)
            .expect("overview");
        assert!(overview.embed_input.contains("pub fn execute(&self)"));
        assert!(overview.embed_input.contains("pub fn backoff(&self)"));
        assert!(
            !overview.embed_input.contains("compute_something_long"),
            "概览不得包含方法体"
        );
        assert_lines_match(&source, &chunks);
    }

    #[test]
    fn long_function_splits_on_statement_boundaries() {
        let mut body = String::new();
        for index in 0..40 {
            body.push_str(&format!("    let step_{index} = compute(step_{index});\n"));
        }
        let source = format!("pub fn very_long() {{\n{body}}}\n");
        let config = ChunkConfig {
            target_tokens: 40,
            max_tokens: 60,
            ..ChunkConfig::default()
        };
        let chunks = chunk("src/long.rs", Language::Rust, &source, &config);
        assert!(chunks.len() > 1, "超长函数必须被拆分");
        assert!(chunks.iter().all(|c| c.kind == ChunkKind::Part));
        // 每个拆分块都保留所属函数信息。
        assert!(chunks
            .iter()
            .all(|c| c.parent_symbol.as_deref() == Some("very_long")));
        // 拆分点必须落在语句起始处（行首），不会切断语句。
        for chunk in &chunks {
            let code = &source[chunk.start_byte..chunk.end_byte];
            assert!(
                code.starts_with("pub fn") || code.starts_with("    let") || code.starts_with('}'),
                "拆分块起点不在语句边界: {:?}",
                &code[..code.len().min(40)]
            );
        }
        // 拆分块必须首尾相接、无重叠、无空隙，拼接后完整还原被拆分的定义。
        for pair in chunks.windows(2) {
            assert_eq!(
                pair[0].end_byte, pair[1].start_byte,
                "拆分块之间不允许出现空隙或重叠"
            );
        }
        let covered = chunks[0].start_byte..chunks[chunks.len() - 1].end_byte;
        let joined: String = chunks
            .iter()
            .map(|c| &source[c.start_byte..c.end_byte])
            .collect();
        assert_eq!(joined, source[covered.clone()]);
        // 覆盖范围就是整个函数定义（函数末尾的换行不属于定义节点）。
        assert_eq!(source[covered].trim_end(), source.trim_end());
        assert_lines_match(&source, &chunks);
    }

    #[test]
    fn small_adjacent_declarations_are_merged() {
        let source = "pub const A: u32 = 1;\npub const B: u32 = 2;\npub type C = u32;\n";
        let config = ChunkConfig::default();
        let chunks = chunk("src/consts.rs", Language::Rust, source, &config);
        assert_eq!(chunks.len(), 1, "相邻小声明应合并: {chunks:#?}");
        assert_eq!(chunks[0].symbol.as_deref(), Some("A, B, C"));
        // 三个符号都要能被检索到。
        let names: Vec<&str> = chunks[0].symbols.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn markdown_splits_on_headings_and_ignores_fenced_hashes() {
        let source = "# 标题一\n正文一\n\n```rust\n# 这不是标题\n```\n\n## 标题二\n正文二\n";
        let config = ChunkConfig::default();
        let chunks = chunk("docs/guide.md", Language::Markdown, source, &config);
        let titles: Vec<Option<&str>> = chunks.iter().map(|c| c.symbol.as_deref()).collect();
        assert_eq!(titles, vec![Some("标题一"), Some("标题二")]);
        assert_lines_match(source, &chunks);
    }

    #[test]
    fn plain_text_uses_bounded_fallback() {
        let source = "行一\n行二\n行三\n".repeat(50);
        let config = ChunkConfig {
            target_tokens: 20,
            ..ChunkConfig::default()
        };
        let chunks = chunk("notes.txt", Language::Text, &source, &config);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.kind == ChunkKind::Text));
        assert!(chunks.iter().all(|c| c.token_estimate <= 40));
        assert_lines_match(&source, &chunks);
    }

    #[test]
    fn unparsable_source_falls_back_to_text_and_keeps_content() {
        // 有效 UTF-8 但不是合法 Rust：不能让文件从索引中消失。
        let source = "这不是代码 {{{ >>> ??? \n第二行\n";
        let config = ChunkConfig::default();
        let chunks = chunk("src/broken.rs", Language::Rust, source, &config);
        assert!(!chunks.is_empty(), "解析失败必须有文本兜底");
        let joined: String = chunks
            .iter()
            .map(|c| &source[c.start_byte..c.end_byte])
            .collect();
        assert_eq!(joined, source);
    }

    #[test]
    fn crlf_and_missing_trailing_newline_produce_consistent_lines() {
        let source = "pub fn a() {\r\n    let x = 1;\r\n}\r\n\r\npub fn b() {}";
        let config = ChunkConfig::default();
        let chunks = chunk("src/crlf.rs", Language::Rust, source, &config);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].start_line, 1);
        assert_eq!(chunks[0].end_line, 3);
        // 末尾没有换行的最后一块行号也必须正确。
        assert_eq!(chunks[1].start_line, 5);
        assert_eq!(chunks[1].end_line, 5);
        assert_lines_match(source, &chunks);
    }

    #[test]
    fn multibyte_content_keeps_byte_ranges_on_char_boundaries() {
        let source = "/// 中文文档说明\npub fn 中文函数() {\n    let 变量 = \"值\";\n}\n";
        let config = ChunkConfig::default();
        let chunks = chunk("src/中文.rs", Language::Rust, source, &config);
        assert_eq!(chunks.len(), 1);
        // 能安全切片说明区间落在字符边界上。
        let code = &source[chunks[0].start_byte..chunks[0].end_byte];
        assert!(code.contains("中文函数"));
        assert_eq!(chunks[0].doc.as_deref(), Some("中文文档说明"));
        // 中文进入 FTS 拆词后是逐字词元。
        assert!(chunks[0].fts.doc_terms.contains("中 文"));
    }

    #[test]
    fn embed_input_is_deterministic_and_excludes_line_numbers() {
        let source = "/// 执行重试。\npub fn execute() {}\n";
        let config = ChunkConfig::default();
        let first = chunk("src/retry.rs", Language::Rust, source, &config);
        let second = chunk("src/retry.rs", Language::Rust, source, &config);
        assert_eq!(first[0].embed_input, second[0].embed_input);
        let input = &first[0].embed_input;
        assert!(input.starts_with("Language: rust\nPath: src/retry.rs"));
        assert!(input.contains("Symbol: execute"));
        assert!(input.contains("Kind: function"));
        assert!(input.contains("Documentation: 执行重试。"));
        assert!(input.contains("Code:\n"));
        // 行号绝不能进入模型输入。
        assert!(!input.contains("start_line"));
        assert!(!input.contains("Line:"));
    }

    #[test]
    fn embed_input_truncation_is_flagged_and_cut_at_line_boundary() {
        let source = format!("pub fn big() {{\n{}}}\n", "    let x = 1;\n".repeat(200));
        let config = ChunkConfig {
            max_tokens: 100_000,
            max_chars: 100_000,
            ..ChunkConfig::default()
        };
        let parsed = parse(Language::Rust, &source).expect("parse");
        let ctx = ChunkContext {
            rel_path: "src/big.rs",
            language: Language::Rust,
            source: &source,
            config: &config,
            document_prefix: "",
            max_input_chars: 600,
        };
        let chunks = chunk_file(&ctx, &parsed).expect("chunk");
        assert!(chunks[0].embed_input_truncated, "截断必须被标记");
        assert!(chunks[0].embed_input.chars().count() <= 600);
        assert!(chunks[0].embed_input.ends_with(';'), "应在行边界截断");
    }

    #[test]
    fn document_prefix_changes_model_input() {
        let source = "pub fn a() {}\n";
        let config = ChunkConfig::default();
        let parsed = parse(Language::Rust, source).expect("parse");
        let ctx = ChunkContext {
            rel_path: "src/a.rs",
            language: Language::Rust,
            source,
            config: &config,
            document_prefix: "passage: ",
            max_input_chars: 16_000,
        };
        let chunks = chunk_file(&ctx, &parsed).expect("chunk");
        assert!(chunks[0]
            .embed_input
            .starts_with("passage: \nLanguage: rust"));
    }

    #[test]
    fn typescript_class_chunks_carry_parent_and_module() {
        let source = "export namespace NS {\n  export class C {\n    run() { return 1; }\n  }\n}\n";
        let config = ChunkConfig {
            target_tokens: 5,
            max_tokens: 10,
            ..ChunkConfig::default()
        };
        let chunks = chunk("web/a.ts", Language::TypeScript, source, &config);
        let run = chunks
            .iter()
            .find(|c| c.symbol.as_deref() == Some("NS.C.run"))
            .expect("方法块存在");
        assert_eq!(run.kind, ChunkKind::Method);
        assert_eq!(run.parent_symbol.as_deref(), Some("NS.C"));
        assert_eq!(run.module_path.as_deref(), Some("NS.C"));
        assert_lines_match(source, &chunks);
    }
}

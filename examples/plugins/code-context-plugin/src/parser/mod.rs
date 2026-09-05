//! 结构化解析：把源文件转换为 [`CodeChunk`] 与 [`Symbol`]。
//!
//! # 一律使用现成解析器
//!
//! 每种语言的语法分析都交给成熟的**纯 Rust 解析器**，本模块只做两件事：把各家 AST 归一化成
//! [`Declaration`]，再按仓库的领域模型装配 chunk 与符号。**不自建语法分析**——为每种语言手写
//! 一套括号/缩进启发式既做不准，也无法随语言演进维护。
//!
//! | 语言 | 解析器 | 形态 |
//! | --- | --- | --- |
//! | Rust | `syn` | 完整 AST |
//! | TypeScript / JavaScript（含 TSX / JSX） | `oxc_parser` | 完整 AST |
//! | Vue / nvue | `html5gum` + `oxc_parser` | SFC 脚本 AST，其他区域文本兜底 |
//! | Python | `rustpython-parser` | 完整 AST |
//! | Go | `gosyn` | 完整 AST |
//! | 其余语言 | 无 | 文本分块 |
//!
//! 这些 crate 全部没有 C 依赖，可直接编译到 `wasm32-wasip2`（已验证）。原
//! [`adr-storage`](../../../docs/code-context-engine/adr-storage.md) 时期认定"缺 C 工具链，
//! 因此只能手写 Tier 2 解析器"的前提只对 Tree-sitter 成立，对纯 Rust 解析器不成立。
//! 决策与验证过程见 `docs/code-context-engine/adr-structural-parsing.md`。
//!
//! # 失败必须隔离
//!
//! 单个文件解析失败只降级为文本分块，**绝不向上传播**——一个语法错误的文件不能让整个仓库
//! 无法检索（`architecture.md` 第 12 问）。解析器都返回 `Result` 或错误列表，因此这里
//! 处理的是错误值而不是 panic。

mod golang;
mod python;
mod rust_lang;
mod typescript;
mod vue;

#[cfg(test)]
mod tests;

use crate::model::{CodeChunk, RepositoryIdentity, Symbol, SymbolKind};
use crate::repository::content_hash;
use crate::repository::language::Language;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// 单个文件最多产出的 chunk 数。
const MAX_CHUNKS_PER_FILE: usize = 2048;

/// 文本分块的目标行数。
const TEXT_CHUNK_LINES: u32 = 60;

/// 签名最多合并的源码行数。
const MAX_SIGNATURE_LINES: usize = 4;

/// 签名最大字符数。
const MAX_SIGNATURE_LEN: usize = 240;

/// 文档注释最大字符数。
const MAX_DOC_LEN: usize = 600;

/// 最多保留的 import 条数。
const MAX_IMPORTS: usize = 128;

/// 可并入语义复合块的单个声明最大行数。
///
/// 六行与检索层的短块判定保持一致：这类声明单独嵌入时常只表达通用控制流，缺少足够的
/// 领域语境；精确符号与词法索引仍保留原子块，因此合并不会损伤定义导航。
const SEMANTIC_GROUP_MEMBER_MAX_LINES: u32 = 6;

/// 单个语义复合块允许覆盖的最大行数。
const SEMANTIC_GROUP_MAX_LINES: u32 = 40;

/// 单个语义复合块允许发送给嵌入端点的最大字符数。
///
/// 与 crate 根的正文嵌入上限保持一致，确保复合块不会因截断再次丢失尾部兄弟节点。
const SEMANTIC_GROUP_MAX_CHARS: usize = 1_200;

/// 相邻兄弟声明之间允许存在的最大空白行数。
const SEMANTIC_GROUP_MAX_GAP_LINES: u32 = 2;

/// 归一化后的一个声明。
///
/// 各语言 AST 的形状差异极大，但检索只关心这几项。适配器负责把自家 AST 折算成它，
/// 之后的限定名推导、chunk 装配与符号表构建对所有语言共用一套代码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// 符号短名。
    pub name: String,
    /// 符号种类。
    pub kind: SymbolKind,
    /// 起始行，从 1 开始。
    pub start_line: u32,
    /// 结束行，闭区间。
    pub end_line: u32,
    /// 声明签名。
    pub signature: Option<String>,
    /// 文档注释。
    pub doc_comment: Option<String>,
    /// 父声明在结果数组中的下标。
    pub parent: Option<usize>,
    /// 非词法归属，例如 Go 方法的接收者类型。
    ///
    /// 它参与限定名但不是词法父节点：`func (r *Repo) Get()` 里 `Repo` 并不在语法上包含 `Get`。
    pub owner: Option<String>,
    /// 该声明是否为测试代码。
    ///
    /// 由各语言适配器按自己的惯例判定（Rust 看 `#[test]` 与 `#[cfg(test)]`），
    /// 与路径判定取并集。
    pub is_test: bool,
}

/// 一次解析的产物。
#[derive(Debug, Clone, Default)]
pub struct ParseOutput {
    /// 按出现顺序排列的声明。
    pub declarations: Vec<Declaration>,
    /// 文件的导入语句。
    pub imports: Vec<String>,
    /// 文件级文档注释（Rust 的 `//!`、Python 的模块 docstring 等）。
    ///
    /// 它描述的是整个文件的职责，而这恰恰是自然语言查询最常表达的东西——
    /// "代码块是怎么切分出来的"对应的正是 `parser/mod.rs` 顶部那句"把源文件转换为
    /// CodeChunk 与 Symbol"。但这段文字不属于文件里任何一个声明，因此在按声明切块的
    /// 索引里会整段丢失。保留它，供嵌入时注入每个代码块。
    pub file_doc: Option<String>,
}

/// 解析所处的层级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseTier {
    /// 使用语言解析器得到的结构化结果。
    Structural,
    /// 文本分块兜底。
    Text,
}

impl ParseTier {
    /// 返回稳定的机器可读名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Structural => "structural",
            Self::Text => "text",
        }
    }
}

/// 一个文件的解析结果。
#[derive(Debug, Clone)]
pub struct ParsedFile {
    /// 该文件产出的代码块。
    pub chunks: Vec<CodeChunk>,
    /// 该文件产出的符号。
    pub symbols: Vec<Symbol>,
    /// 实际使用的解析层级。
    pub tier: ParseTier,
    /// 文件级文档注释，供嵌入时补充每个代码块的所属语境。
    pub file_doc: Option<String>,
}

/// 字节偏移到行号的映射。
///
/// `oxc`、`rustpython-parser` 与 `gosyn` 返回的都是字节偏移，需要统一折算成行号。
/// `syn` 直接给行号，不经过这里。
pub struct LineIndex {
    /// 每一行起始处的字节偏移。
    starts: Vec<usize>,
}

impl LineIndex {
    /// 为源码建立行索引。
    pub fn new(source: &str) -> Self {
        let mut starts = vec![0usize];
        starts.extend(
            source
                .char_indices()
                .filter(|(_, character)| *character == '\n')
                .map(|(offset, _)| offset + 1),
        );
        Self { starts }
    }

    /// 返回字节偏移所在的行号，从 1 开始。
    pub fn line_of(&self, offset: usize) -> u32 {
        let index = match self.starts.binary_search(&offset) {
            Ok(exact) => exact,
            Err(insert) => insert.saturating_sub(1),
        };
        u32::try_from(index + 1).unwrap_or(u32::MAX)
    }
}

/// 按语言解析源码。
///
/// 语言没有对应解析器、或解析器报错时返回 `None`，由调用方降级为文本分块。
fn parse_source(language: Language, source: &str) -> Option<ParseOutput> {
    match language {
        Language::Rust => rust_lang::parse(source),
        Language::TypeScript => typescript::parse(source, true),
        Language::JavaScript => typescript::parse(source, false),
        Language::Python => python::parse(source),
        Language::Go => golang::parse(source),
        Language::Vue => vue::parse(source),
        _ => None,
    }
}

/// 把一个文件解析为 chunk 与符号。
///
/// `source` 是文件正文，`file_path` 是仓库相对路径。解析失败或语言没有解析器时自动降级为
/// 文本分块，因此本函数**不返回错误**：任何输入都会得到可检索的产物。
pub fn parse_file(
    repository: &RepositoryIdentity,
    file_path: &str,
    language: Language,
    source: &str,
) -> ParsedFile {
    let lines: Vec<&str> = source.lines().collect();
    let output = parse_source(language, source).filter(|output| !output.declarations.is_empty());

    let Some(output) = output else {
        return ParsedFile {
            chunks: text_chunks(repository, file_path, language, &lines),
            symbols: Vec::new(),
            tier: ParseTier::Text,
            file_doc: None,
        };
    };

    let file_is_test = is_test_path(file_path);
    let separator = separator_of(language);
    let mut imports = output.imports.clone();
    imports.truncate(MAX_IMPORTS);

    let mut chunks = Vec::new();
    let mut symbols = Vec::new();
    for (index, declaration) in output.declarations.iter().enumerate() {
        if chunks.len() >= MAX_CHUNKS_PER_FILE {
            break;
        }
        let qualified = qualified_name(&output.declarations, index, separator);
        let parent_symbol = declaration
            .parent
            .map(|parent| qualified_name(&output.declarations, parent, separator));
        let body = slice_lines(&lines, declaration.start_line, declaration.end_line);

        chunks.push(CodeChunk {
            id: chunk_id(repository, file_path, &qualified, declaration.kind, index),
            repository_id: repository.clone(),
            file_path: file_path.to_string(),
            language: language.as_str().to_string(),
            symbol_name: Some(declaration.name.clone()),
            qualified_symbol: Some(qualified.clone()),
            symbol_kind: Some(declaration.kind),
            parent_symbol,
            start_line: declaration.start_line,
            end_line: declaration.end_line,
            signature: declaration
                .signature
                .clone()
                .or_else(|| signature_from_source(&lines, declaration.start_line)),
            doc_comment: declaration
                .doc_comment
                .clone()
                .map(|doc| truncate(doc, MAX_DOC_LEN)),
            // 路径与声明属性取并集：`tests/` 目录下的一切都是测试，而 `src/lib.rs` 里的
            // `#[cfg(test)] mod tests` 只能靠属性识别。
            is_test: file_is_test || declaration.is_test,
            semantic_only: false,
            semantic_group_id: None,
            imports: imports.clone(),
            content_hash: content_hash(&body),
        });
        symbols.push(Symbol {
            name: declaration.name.clone(),
            qualified_name: qualified,
            kind: declaration.kind,
            file_path: file_path.to_string(),
            start_line: declaration.start_line,
            end_line: declaration.end_line,
        });
    }

    append_semantic_groups(
        repository,
        file_path,
        language,
        &lines,
        &output.declarations,
        &imports,
        &mut chunks,
    );

    if language == Language::Vue {
        // SFC 的模板、样式和未被声明覆盖的脚本仍可检索，行号直接映射到原文件。
        let mut covered = vec![false; lines.len()];
        for declaration in &output.declarations {
            for line in covered
                .iter_mut()
                .take(declaration.end_line as usize)
                .skip(declaration.start_line.saturating_sub(1) as usize)
            {
                *line = true;
            }
        }
        let mut start = 0;
        while start < lines.len() && chunks.len() < MAX_CHUNKS_PER_FILE {
            if covered[start] {
                start += 1;
                continue;
            }
            let end = (start..lines.len())
                .find(|i| covered[*i])
                .unwrap_or(lines.len());
            for mut chunk in text_chunks(repository, file_path, language, &lines[start..end]) {
                if chunks.len() >= MAX_CHUNKS_PER_FILE {
                    break;
                }
                chunk.start_line += start as u32;
                chunk.end_line += start as u32;
                if !is_substantive_block(
                    &lines[chunk.start_line.saturating_sub(1) as usize..chunk.end_line as usize],
                ) {
                    continue;
                }
                chunk.id = chunk_id(repository, file_path, "", SymbolKind::Other, chunks.len());
                chunks.push(chunk);
            }
            start = end;
        }
        // 方法块负责精确导航，窗口负责跨方法上下文与长函数尾部召回，避免只嵌入函数开头。
        // 不覆盖任何声明的窗口已有文本兜底，不再重复建立向量。
        for mut chunk in text_chunks(repository, file_path, language, &lines) {
            if chunks.len() >= MAX_CHUNKS_PER_FILE {
                break;
            }
            if !covered
                .iter()
                .take(chunk.end_line as usize)
                .skip(chunk.start_line.saturating_sub(1) as usize)
                .any(|value| *value)
            {
                continue;
            }
            chunk.id = chunk_id(
                repository,
                file_path,
                "sfc-context",
                SymbolKind::Other,
                chunks.len(),
            );
            chunk.semantic_only = true;
            chunks.push(chunk);
        }
    }

    ParsedFile {
        chunks,
        symbols,
        tier: ParseTier::Structural,
        file_doc: output.file_doc.map(|doc| truncate(doc, MAX_DOC_LEN)),
    }
}

/// 把相邻的过短兄弟声明合成语义专用复合块。
///
/// 原子块不会被删除：符号和 BM25 仍需要它们的精确定位。这里只给满足同父节点、同测试
/// 属性、同叶子类别、源码连续且总长度有界的声明建立复合表示，并在原子块上记录归属，供
/// 语义索引跳过重复向量。少于两个成员时不生成复合块。
fn append_semantic_groups(
    repository: &RepositoryIdentity,
    file_path: &str,
    language: Language,
    lines: &[&str],
    declarations: &[Declaration],
    imports: &[String],
    chunks: &mut Vec<CodeChunk>,
) {
    let original_len = chunks.len().min(declarations.len());
    let mut sibling_sets: BTreeMap<(Option<usize>, Option<String>), Vec<usize>> = BTreeMap::new();
    for (index, declaration) in declarations.iter().take(original_len).enumerate() {
        sibling_sets
            .entry((declaration.parent, declaration.owner.clone()))
            .or_default()
            .push(index);
    }

    for ((parent, owner), siblings) in sibling_sets {
        let mut run = Vec::new();
        for index in siblings {
            if !is_semantic_group_member(&declarations[index]) {
                flush_semantic_group(
                    repository,
                    file_path,
                    language,
                    lines,
                    declarations,
                    imports,
                    chunks,
                    parent,
                    owner.as_deref(),
                    &mut run,
                );
                continue;
            }

            if !run.is_empty()
                && !can_extend_semantic_group(lines, declarations, chunks, &run, index)
            {
                flush_semantic_group(
                    repository,
                    file_path,
                    language,
                    lines,
                    declarations,
                    imports,
                    chunks,
                    parent,
                    owner.as_deref(),
                    &mut run,
                );
            }
            run.push(index);
        }
        flush_semantic_group(
            repository,
            file_path,
            language,
            lines,
            declarations,
            imports,
            chunks,
            parent,
            owner.as_deref(),
            &mut run,
        );
    }
}

/// 判断一个声明是否适合与兄弟节点共享语义表示。
fn is_semantic_group_member(declaration: &Declaration) -> bool {
    let span = declaration
        .end_line
        .saturating_sub(declaration.start_line)
        .saturating_add(1);
    span <= SEMANTIC_GROUP_MEMBER_MAX_LINES
        && matches!(
            declaration.kind,
            SymbolKind::Function | SymbolKind::Constant | SymbolKind::TypeAlias | SymbolKind::Other
        )
}

/// 判断候选声明加入当前复合块后是否仍满足连续性与输入预算。
fn can_extend_semantic_group(
    lines: &[&str],
    declarations: &[Declaration],
    chunks: &[CodeChunk],
    run: &[usize],
    candidate: usize,
) -> bool {
    let first = run[0];
    let previous = *run.last().expect("非空分组必须有末项");
    if chunks[first].is_test != chunks[candidate].is_test
        || semantic_group_category(declarations[first].kind)
            != semantic_group_category(declarations[candidate].kind)
    {
        return false;
    }
    let gap_lines = declarations[candidate]
        .start_line
        .saturating_sub(declarations[previous].end_line)
        .saturating_sub(1);
    if gap_lines > SEMANTIC_GROUP_MAX_GAP_LINES {
        return false;
    }
    let start_line = declarations[first].start_line;
    let end_line = declarations[candidate].end_line;
    if end_line.saturating_sub(start_line).saturating_add(1) > SEMANTIC_GROUP_MAX_LINES {
        return false;
    }
    slice_lines(lines, start_line, end_line).chars().count() <= SEMANTIC_GROUP_MAX_CHARS
}

/// 把符号种类折算为可安全共享语义上下文的叶子类别。
fn semantic_group_category(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 1,
        SymbolKind::Constant | SymbolKind::TypeAlias => 2,
        SymbolKind::Other => 3,
        SymbolKind::Struct
        | SymbolKind::Enum
        | SymbolKind::Trait
        | SymbolKind::Impl
        | SymbolKind::Module => 0,
    }
}

/// 完成一个候选分组，并清空临时成员列表。
#[allow(clippy::too_many_arguments)]
fn flush_semantic_group(
    repository: &RepositoryIdentity,
    file_path: &str,
    language: Language,
    lines: &[&str],
    declarations: &[Declaration],
    imports: &[String],
    chunks: &mut Vec<CodeChunk>,
    parent: Option<usize>,
    owner: Option<&str>,
    run: &mut Vec<usize>,
) {
    if run.len() < 2 || chunks.len() >= MAX_CHUNKS_PER_FILE {
        run.clear();
        return;
    }

    let start_line = declarations[run[0]].start_line;
    let end_line = declarations[*run.last().expect("复合块必须有末项")].end_line;
    let body = slice_lines(lines, start_line, end_line);
    if body.chars().count() > SEMANTIC_GROUP_MAX_CHARS {
        run.clear();
        return;
    }
    let member_ids: Vec<&str> = run.iter().map(|index| chunks[*index].id.as_str()).collect();
    let group_id = semantic_group_id(repository, file_path, &member_ids);
    for index in run.iter().copied() {
        chunks[index].semantic_group_id = Some(group_id.clone());
    }
    let parent_symbol = parent
        .and_then(|index| chunks.get(index))
        .and_then(|chunk| chunk.qualified_symbol.clone())
        .or_else(|| owner.map(str::to_string));
    let is_test = chunks[run[0]].is_test;
    chunks.push(CodeChunk {
        id: group_id,
        repository_id: repository.clone(),
        file_path: file_path.to_string(),
        language: language.as_str().to_string(),
        symbol_name: None,
        qualified_symbol: None,
        symbol_kind: None,
        parent_symbol,
        start_line,
        end_line,
        signature: None,
        doc_comment: None,
        is_test,
        semantic_only: true,
        semantic_group_id: None,
        imports: imports.to_vec(),
        content_hash: content_hash(&body),
    });
    run.clear();
}

/// 根据成员的稳定 chunk ID 计算语义复合块 ID。
///
/// 不包含行号与正文；仅插入空行或在文件前添加内容时成员 ID 不变，复合块可以继续复用。
/// 正文变化由 `content_hash` 单独识别并触发重嵌。
fn semantic_group_id(
    repository: &RepositoryIdentity,
    file_path: &str,
    member_ids: &[&str],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"code-context-semantic-group-v1\0");
    for field in [repository.as_str(), file_path] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    for member_id in member_ids {
        digest.update((member_id.len() as u64).to_be_bytes());
        digest.update(member_id.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

/// 提取文件开头的注释块作为文件级文档。
///
/// 供没有专门"文件文档"语法的语言使用（TypeScript、Go）。只取**第一个声明之前**的连续
/// 注释：再往后的注释属于某个具体声明，已经由解析器归到对应 chunk 上，重复收进来只会
/// 让每个块的嵌入文本被同一段文字稀释。
///
/// 跳过 shebang 与 `//go:build` 这类构建指令——它们是给工具看的，不描述文件职责。
pub fn leading_doc_comment(source: &str) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut in_block = false;
    for line in source.lines() {
        let trimmed = line.trim();
        if in_block {
            if let Some(end) = trimmed.find("*/") {
                parts.push(trimmed[..end].trim_start_matches('*').trim().to_string());
                in_block = false;
            } else {
                parts.push(trimmed.trim_start_matches('*').trim().to_string());
            }
            continue;
        }
        if trimmed.is_empty() {
            // 注释块已经开始时，空行意味着它结束了。
            if parts.is_empty() {
                continue;
            }
            break;
        }
        if trimmed.starts_with("#!") || trimmed.starts_with("//go:") || trimmed.starts_with("#") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("//") {
            parts.push(rest.trim_start_matches('/').trim().to_string());
            continue;
        }
        if trimmed.starts_with("/*") {
            let rest = trimmed.trim_start_matches("/*").trim_start_matches('*');
            match rest.find("*/") {
                Some(end) => parts.push(rest[..end].trim().to_string()),
                None => {
                    parts.push(rest.trim().to_string());
                    in_block = true;
                }
            }
            continue;
        }
        // 到达第一个非注释内容，文件级注释到此为止。
        break;
    }
    let doc = parts
        .into_iter()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    (!doc.trim().is_empty()).then(|| truncate(doc, MAX_DOC_LEN))
}

/// 按路径判断一个文件是否属于测试代码。
///
/// 覆盖各语言的通行约定：`tests/`、`test/`、`__tests__/`、`benches/`、`spec/` 目录，
/// 以及 `*_test.go`、`*.test.ts`、`*.spec.ts`、`test_*.py`、`*_spec.rb`、`tests.rs`
/// 这类文件名。仅靠路径覆盖不到 Rust 内联的 `#[cfg(test)] mod tests`，那部分由解析器
/// 的属性判定补上。
pub fn is_test_path(file_path: &str) -> bool {
    let lower = file_path.to_ascii_lowercase();
    let segments: Vec<&str> = lower.split('/').collect();
    if segments.iter().rev().skip(1).any(|segment| {
        matches!(
            *segment,
            "tests" | "test" | "__tests__" | "benches" | "spec"
        )
    }) {
        return true;
    }
    let Some(name) = segments.last() else {
        return false;
    };
    *name == "tests.rs"
        || *name == "conftest.py"
        || name.starts_with("test_")
        || name.ends_with("_test.go")
        || name.ends_with("_test.rs")
        || name.ends_with("_test.py")
        || name.ends_with("_spec.rb")
        || [".test.", ".spec."]
            .iter()
            .any(|marker| name.contains(marker))
}

/// 返回语言的限定名分隔符。
fn separator_of(language: Language) -> &'static str {
    match language {
        Language::Rust => "::",
        _ => ".",
    }
}

/// 沿父链推导限定名。
///
/// Go 方法的接收者通过 [`Declaration::owner`] 参与限定名：`func (r *Repo) Get()` 得到
/// `Repo.Get`，即便 `Repo` 在语法上并不包含 `Get`。
fn qualified_name(declarations: &[Declaration], index: usize, separator: &str) -> String {
    /// 限定名最多回溯的层数，防御异常的父链。
    const MAX_DEPTH: usize = 16;

    let mut parts: Vec<&str> = Vec::new();
    let mut cursor = Some(index);
    let mut depth = 0usize;
    while let Some(current) = cursor {
        let Some(declaration) = declarations.get(current) else {
            break;
        };
        depth += 1;
        if depth > MAX_DEPTH {
            break;
        }
        parts.push(&declaration.name);
        if let Some(owner) = declaration.owner.as_deref() {
            parts.push(owner);
        }
        cursor = declaration.parent.filter(|parent| *parent != current);
    }
    parts.reverse();
    parts.join(separator)
}

/// 计算稳定的 chunk 标识。
///
/// 由仓库身份、文件路径、限定名、符号种类与同名出现序号共同决定，**不含行号**：
/// 行号会在文件任意位置插入一行后全部改变，让增量索引把整个文件判成新内容。
fn chunk_id(
    repository: &RepositoryIdentity,
    file_path: &str,
    qualified: &str,
    kind: SymbolKind,
    occurrence: usize,
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"code-context-chunk-v1\0");
    for field in [repository.as_str(), file_path, qualified] {
        digest.update((field.len() as u64).to_be_bytes());
        digest.update(field.as_bytes());
    }
    digest.update([kind_tag(kind)]);
    digest.update((occurrence as u64).to_be_bytes());
    format!("{:x}", digest.finalize())
}

/// 返回符号种类的稳定字节标签。
fn kind_tag(kind: SymbolKind) -> u8 {
    match kind {
        SymbolKind::Function => 1,
        SymbolKind::Struct => 2,
        SymbolKind::Enum => 3,
        SymbolKind::Trait => 4,
        SymbolKind::Impl => 5,
        SymbolKind::Module => 6,
        SymbolKind::Constant => 7,
        SymbolKind::TypeAlias => 8,
        SymbolKind::Other => 9,
    }
}

/// 取闭区间行范围的正文。
fn slice_lines(lines: &[&str], start_line: u32, end_line: u32) -> String {
    let start = start_line.saturating_sub(1) as usize;
    let end = (end_line as usize).min(lines.len());
    if start >= end {
        return String::new();
    }
    lines[start..end].join("\n")
}

/// 从源码提取签名：声明行起，到出现 `{`、`:` 或 `;` 为止。
fn signature_from_source(lines: &[&str], start_line: u32) -> Option<String> {
    let start = start_line.saturating_sub(1) as usize;
    let mut parts: Vec<&str> = Vec::new();
    for line in lines.iter().skip(start).take(MAX_SIGNATURE_LINES) {
        let text = line.trim();
        let cut = text.find('{').unwrap_or(text.len());
        let piece = text[..cut].trim();
        if !piece.is_empty() {
            parts.push(piece);
        }
        if text.contains('{') || text.contains(';') {
            break;
        }
    }
    let signature = parts
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!signature.is_empty()).then(|| truncate(signature, MAX_SIGNATURE_LEN))
}

/// 按字符边界截断文本。
fn truncate(mut text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let cut = text
        .char_indices()
        .nth(limit)
        .map_or(text.len(), |(offset, _)| offset);
    text.truncate(cut);
    text
}

/// 纯注释块仍值得单独建索引所需的最少非空行数。
///
/// 取 5 行：一段真正在解释设计的注释不会只有一两行，而声明之间遗留的 `// 播放错误`
/// 恰恰只有一行。
const MIN_COMMENT_ONLY_BLOCK_LINES: usize = 5;

/// 判断一段源码是否含有值得单独占用一个检索结果位置的内容。
///
/// Vue SFC 的声明之间常常只剩空行、单行注释和收尾括号。这类块拿到再高的名次也回答不了
/// 任何问题，却会把真正的实现挤出 Top-N——实测一次中文查询的八条结果里有四条是
/// `// 播放错误`、`//邀请码` 这样的一行注释。
///
/// 判据：存在任意一行既不是注释、又含有字母或数字，即认为整块有实质内容；全是注释时，
/// 只有达到 [`MIN_COMMENT_ONLY_BLOCK_LINES`] 行的成段说明才保留。
fn is_substantive_block(lines: &[&str]) -> bool {
    let mut non_blank = 0usize;
    for line in lines {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        non_blank += 1;
        let is_comment = text.starts_with("//")
            || text.starts_with("/*")
            || text.starts_with('*')
            || text.starts_with("<!--");
        if !is_comment && text.chars().any(char::is_alphanumeric) {
            return true;
        }
    }
    non_blank >= MIN_COMMENT_ONLY_BLOCK_LINES
}

/// Tier 3 文本分块。
///
/// 用于没有解析器的语言、解析失败的文件，以及确实不含任何声明的文件（配置、纯数据）。
/// 优先在空行处切分，让块边界落在语义停顿上，而不是硬切在语句中间。
fn text_chunks(
    repository: &RepositoryIdentity,
    file_path: &str,
    language: Language,
    lines: &[&str],
) -> Vec<CodeChunk> {
    let file_is_test = is_test_path(file_path);
    if lines.iter().all(|line| line.trim().is_empty()) {
        return Vec::new();
    }

    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < lines.len() && chunks.len() < MAX_CHUNKS_PER_FILE {
        let hard_end = (start + TEXT_CHUNK_LINES as usize).min(lines.len());
        // 在窗口后半段找最后一个空行作为切点，避免把一段完整内容拦腰截断。
        let end = if hard_end < lines.len() {
            (start + TEXT_CHUNK_LINES as usize / 2..hard_end)
                .rev()
                .find(|index| lines[*index].trim().is_empty())
                .map_or(hard_end, |index| index + 1)
        } else {
            hard_end
        };

        let body = lines[start..end].join("\n");
        if !body.trim().is_empty() {
            let occurrence = chunks.len();
            chunks.push(CodeChunk {
                id: chunk_id(repository, file_path, "", SymbolKind::Other, occurrence),
                repository_id: repository.clone(),
                file_path: file_path.to_string(),
                language: language.as_str().to_string(),
                symbol_name: None,
                qualified_symbol: None,
                symbol_kind: None,
                parent_symbol: None,
                start_line: u32::try_from(start + 1).unwrap_or(u32::MAX),
                end_line: u32::try_from(end).unwrap_or(u32::MAX),
                signature: None,
                doc_comment: None,
                is_test: file_is_test,
                semantic_only: false,
                semantic_group_id: None,
                imports: Vec::new(),
                content_hash: content_hash(&body),
            });
        }
        start = end.max(start + 1);
    }
    chunks
}

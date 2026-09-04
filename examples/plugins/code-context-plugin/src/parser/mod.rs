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
//! 无法检索（`architecture.md` 第 12 问）。四个解析器都返回 `Result` 或错误列表，因此这里
//! 处理的是错误值而不是 panic。

mod golang;
mod python;
mod rust_lang;
mod typescript;

#[cfg(test)]
mod tests;

use crate::model::{CodeChunk, RepositoryIdentity, Symbol, SymbolKind};
use crate::repository::content_hash;
use crate::repository::language::Language;
use sha2::{Digest, Sha256};

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

    ParsedFile {
        chunks,
        symbols,
        tier: ParseTier::Structural,
        file_doc: output.file_doc.map(|doc| truncate(doc, MAX_DOC_LEN)),
    }
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
                imports: Vec::new(),
                content_hash: content_hash(&body),
            });
        }
        start = end.max(start + 1);
    }
    chunks
}

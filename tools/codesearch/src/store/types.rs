//! 统一写入路径的输入契约。
//!
//! 这些类型由 [`crate::parser`] 与 [`crate::chunker`] 在事务之外生成，再整体交给
//! [`crate::store::Store::commit_file`] 落库。把契约放在 store 一侧，是为了让
//! "写入需要哪些字段" 只有一个定义处：解析与分块只负责填满它，不关心表结构。

use crate::types::ChunkKind;

pub use crate::retrieval::tokenize::FtsText;

/// 一次文件提交的完整输入：源码快照 + 全部代码块 + 导入元数据。
///
/// 采集与解析都已在事务外完成，因此提交阶段只做写库，写事务保持短小。
#[derive(Debug, Clone)]
pub struct PreparedFile {
    /// `/` 分隔的相对路径。
    pub rel_path: String,
    /// 语言。
    pub language: crate::types::Language,
    /// 源码内容哈希（blake3 十六进制全长）。
    pub file_hash: String,
    /// 文件字节大小。
    pub size: u64,
    /// 修改时间（纳秒）。
    pub mtime_ns: i64,
    /// 总行数。
    pub line_count: u32,
    /// 完整源码快照，是结果代码与行号的唯一来源。
    pub content: String,
    /// 该版本的全部代码块，按 `ordinal` 升序。
    pub chunks: Vec<PreparedChunk>,
    /// 该版本的导入声明。
    pub imports: Vec<ImportRecord>,
}

/// 一个待写入的代码块。
///
/// 字节区间语义为 `[start_byte, end_byte)`，行号从 1 开始且两端均含。
/// `embed_input` 是将来实际发送给模型的完整文档输入，缓存键由它派生。
#[derive(Debug, Clone)]
pub struct PreparedChunk {
    /// 在文件内的序号，从 0 开始。
    pub ordinal: usize,
    /// 代码块类别。
    pub kind: ChunkKind,
    /// 限定符号名；纯文本块为 `None`。
    pub symbol: Option<String>,
    /// 逻辑符号身份。不假设所有语言都有完美稳定的符号身份，仅用于定位与去重。
    pub symbol_key: Option<String>,
    /// 父作用域符号。
    pub parent_symbol: Option<String>,
    /// 模块路径。
    pub module_path: Option<String>,
    /// 单行化签名。
    pub signature: Option<String>,
    /// 绑定的文档注释。
    pub doc: Option<String>,
    /// 起始字节（含）。
    pub start_byte: usize,
    /// 结束字节（不含）。
    pub end_byte: usize,
    /// 起始行（从 1 开始，含）。
    pub start_line: u32,
    /// 结束行（从 1 开始，含）。
    pub end_line: u32,
    /// 估算 token 数（无精确 tokenizer）。
    pub token_estimate: usize,
    /// 代码内容哈希。
    pub content_hash: String,
    /// 发送给模型的完整文档输入。
    pub embed_input: String,
    /// 模型输入是否因字符上限被截断。
    pub embed_input_truncated: bool,
    /// FTS5 拆词文本（与原始代码分开保存）。
    pub fts: FtsText,
    /// 该块登记的符号，合并块会有多个。
    pub symbols: Vec<SymbolRecord>,
}

/// 需要登记到符号表的一个符号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRecord {
    /// 简单名称。
    pub name: String,
    /// 限定名称。
    pub qualified: String,
    /// 定义类别。
    pub kind: ChunkKind,
}

/// 一条导入声明。
///
/// 只记录源码里写了什么，不解析导入目标；按名称推导的关系只能作为启发式使用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRecord {
    /// 导入语句原文。
    pub raw: String,
    /// 提取出的模块路径。
    pub module: Option<String>,
    /// 语句起始字节（含）。仅用于确定导入块的真实范围，不写入数据库。
    pub start_byte: usize,
    /// 语句结束字节（不含）。
    pub end_byte: usize,
}

/// 一次文件提交的结果。
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    /// 文件 ID。
    pub file_id: i64,
    /// 新的当前版本 ID。
    pub revision_id: i64,
    /// 本次激活的代码块 ID，顺序与 [`PreparedFile::chunks`] 一致。
    pub chunk_ids: Vec<i64>,
    /// 该内容此前已存在版本记录（例如内容回退）时为真。
    pub reused_revision: bool,
    /// 因命中 Embedding 缓存而无需请求模型的代码块数。
    ///
    /// 提交阶段只统计"缓存中已有对应向量"的数量，实际关联在事务外完成。
    pub cache_hits: usize,
}

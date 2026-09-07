//! codesearch 的统一错误类型。
//!
//! 所有对外 API（CLI、MCP、库调用）都通过 [`Error`] 报告失败，并保证错误文本不包含凭证。

use std::io;
use std::path::PathBuf;

/// codesearch 的错误类型。
///
/// 变体按发生位置划分，便于 CLI 与 MCP 层映射为稳定的错误码。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// 配置文件或命令行参数不合法。
    #[error("配置错误: {0}")]
    Config(String),

    /// 工作目录不存在、不可读或无法规范化。
    #[error("工作目录不可用 {path}: {source}")]
    Workspace {
        /// 出错的路径。
        path: PathBuf,
        /// 底层 IO 错误。
        #[source]
        source: io::Error,
    },

    /// 请求的路径超出 workspace 范围（`..`、绝对路径或指向外部的符号链接）。
    #[error("路径超出 workspace 范围: {0}")]
    PathOutOfScope(String),

    /// 另一进程持有该 workspace 的写锁。
    #[error("workspace 正被其他进程占用（写锁被持有）: {0}")]
    WorkspaceBusy(String),

    /// SQLite 错误。
    #[error("数据库错误: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// 带上下文的 IO 错误。
    #[error("IO 错误 ({context}): {source}")]
    Io {
        /// 出错时正在做什么。
        context: String,
        /// 底层 IO 错误。
        #[source]
        source: io::Error,
    },

    /// 序列化或反序列化失败。
    #[error("序列化错误: {0}")]
    Serde(String),

    /// Embedding Provider 调用失败。
    #[error("Embedding Provider 错误: {0}")]
    Provider(String),

    /// 向量索引（USearch 或检查点）错误。
    #[error("向量索引错误: {0}")]
    Vector(String),

    /// `code_ref` 指向的源码版本已不存在。
    ///
    /// 文案只描述现象；机器可读的错误码由 [`Error::code`] 统一给出，
    /// CLI 与 MCP 都会在外层附加它，因此这里不能重复写进文案。
    #[error("引用的源码版本已失效: {0}")]
    ReferenceExpired(String),

    /// `code_ref` 格式无法解析或与当前 workspace 不匹配。
    #[error("引用无效: {0}")]
    InvalidReference(String),

    /// 查询为空或作用域表达式不合法。
    #[error("查询无效: {0}")]
    InvalidQuery(String),

    /// 尚未建立索引且当前模式无法建立。
    #[error("索引不存在: {0}")]
    IndexMissing(String),

    /// 当前引擎以只读模式打开，无法执行写操作。
    #[error("引擎处于只读模式: {0}")]
    ReadOnly(String),

    /// 其他内部错误。
    #[error("{0}")]
    Internal(String),
}

impl Error {
    /// 构造带上下文的 IO 错误。
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// 返回稳定的机器可读错误码，供 CLI JSON 输出与 MCP 结果使用。
    pub fn code(&self) -> &'static str {
        match self {
            Error::Config(_) => "config_error",
            Error::Workspace { .. } => "workspace_unavailable",
            Error::PathOutOfScope(_) => "path_out_of_scope",
            Error::WorkspaceBusy(_) => "workspace_busy",
            Error::Sqlite(_) => "database_error",
            Error::Io { .. } => "io_error",
            Error::Serde(_) => "serialization_error",
            Error::Provider(_) => "provider_error",
            Error::Vector(_) => "vector_index_error",
            Error::ReferenceExpired(_) => "reference_expired",
            Error::InvalidReference(_) => "invalid_reference",
            Error::InvalidQuery(_) => "invalid_query",
            Error::IndexMissing(_) => "index_missing",
            Error::ReadOnly(_) => "read_only",
            Error::Internal(_) => "internal_error",
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Error::Serde(value.to_string())
    }
}

/// codesearch 统一的 `Result` 别名。
pub type Result<T> = std::result::Result<T, Error>;

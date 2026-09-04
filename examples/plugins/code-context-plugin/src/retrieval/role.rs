//! 代码块角色推导。
//!
//! 角色来自现有 AST 符号种类、文件类型与测试标记，不写入持久化索引，避免为可派生字段升级
//! schema。当前没有 references graph，因此不会把普通函数正文伪装成调用点。

use crate::model::{CodeChunk, SymbolKind};
use serde::{Deserialize, Serialize};

/// 一个代码块在检索结果中的结构角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkRole {
    /// 常量、模块或其他普通声明。
    Definition,
    /// 具有可执行正文的函数、方法或实现块。
    Implementation,
    /// 调用或使用位置；当前仅作为协议角色保留，尚无 references graph 产出。
    CallSite,
    /// 结构体、类、枚举、接口、trait 或类型别名。
    TypeDefinition,
    /// Markdown、RST 等说明性文档。
    Documentation,
    /// 测试代码。
    Test,
    /// TOML、YAML、JSON、INI 等配置文件。
    Config,
    /// 无法可靠归类的源码或文本块。
    Other,
}

impl ChunkRole {
    /// 从现有 chunk 元数据推导角色。
    ///
    /// 测试标记优先于符号种类；文档与配置按扩展名识别；其余结构化代码按 `SymbolKind`
    /// 映射。该方法不读取正文，也没有外部副作用。
    pub fn from_chunk(chunk: &CodeChunk) -> Self {
        if chunk.is_test {
            return Self::Test;
        }
        if has_extension(&chunk.file_path, &["md", "mdx", "rst", "adoc", "asciidoc"]) {
            return Self::Documentation;
        }
        if has_extension(
            &chunk.file_path,
            &["toml", "yaml", "yml", "json", "ini", "conf", "properties"],
        ) {
            return Self::Config;
        }
        match chunk.symbol_kind {
            Some(SymbolKind::Function | SymbolKind::Impl) => Self::Implementation,
            Some(
                SymbolKind::Struct | SymbolKind::Enum | SymbolKind::Trait | SymbolKind::TypeAlias,
            ) => Self::TypeDefinition,
            Some(SymbolKind::Module | SymbolKind::Constant) => Self::Definition,
            Some(SymbolKind::Other) => Self::Definition,
            None => Self::Other,
        }
    }

    /// 返回稳定的机器可读名称，供工具输出与 benchmark 使用。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::Implementation => "implementation",
            Self::CallSite => "call_site",
            Self::TypeDefinition => "type_definition",
            Self::Documentation => "documentation",
            Self::Test => "test",
            Self::Config => "config",
            Self::Other => "other",
        }
    }
}

/// 判断路径是否带任一目标扩展名。
fn has_extension(path: &str, extensions: &[&str]) -> bool {
    let extension = path.rsplit_once('.').map(|(_, extension)| extension);
    extension.is_some_and(|extension| {
        extensions
            .iter()
            .any(|candidate| extension.eq_ignore_ascii_case(candidate))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::RepositoryIdentity;

    /// 构造角色推导使用的最小 chunk。
    fn chunk(path: &str, kind: Option<SymbolKind>) -> CodeChunk {
        CodeChunk {
            id: "chunk".into(),
            repository_id: RepositoryIdentity::from_workspace_root("/repo").expect("仓库身份"),
            file_path: path.into(),
            language: "rust".into(),
            symbol_name: Some("item".into()),
            qualified_symbol: Some("item".into()),
            symbol_kind: kind,
            parent_symbol: None,
            start_line: 1,
            end_line: 3,
            signature: None,
            doc_comment: None,
            is_test: false,
            semantic_only: false,
            semantic_group_id: None,
            imports: Vec::new(),
            content_hash: "hash".into(),
        }
    }

    /// 符号种类应区分实现、类型与普通定义。
    #[test]
    fn derives_roles_from_symbol_kind() {
        assert_eq!(
            ChunkRole::from_chunk(&chunk("src/lib.rs", Some(SymbolKind::Function))),
            ChunkRole::Implementation
        );
        assert_eq!(
            ChunkRole::from_chunk(&chunk("src/lib.rs", Some(SymbolKind::Struct))),
            ChunkRole::TypeDefinition
        );
        assert_eq!(
            ChunkRole::from_chunk(&chunk("src/lib.rs", Some(SymbolKind::Constant))),
            ChunkRole::Definition
        );
    }

    /// 测试、文档与配置文件应优先按文件角色识别。
    #[test]
    fn derives_roles_from_file_metadata() {
        let mut test = chunk("tests/search.rs", Some(SymbolKind::Function));
        test.is_test = true;
        assert_eq!(ChunkRole::from_chunk(&test), ChunkRole::Test);
        assert_eq!(
            ChunkRole::from_chunk(&chunk("docs/search.md", None)),
            ChunkRole::Documentation
        );
        assert_eq!(
            ChunkRole::from_chunk(&chunk("config/search.toml", None)),
            ChunkRole::Config
        );
    }
}

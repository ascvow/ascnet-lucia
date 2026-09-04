//! Code Context Engine 的领域模型。
//!
//! 这些类型刻意不知道任何存储后端：它们只描述"引擎在谈论什么"，不描述"数据放在哪里"。
//! 存储层负责序列化与寻址（见 [`crate::storage`]），因此后续从 JSON KV 迁移到块存储
//! 时，这一层不需要改动。
//!
//! # 三类版本号互不绑定
//!
//! - [`INDEX_SCHEMA_VERSION`]：manifest 自身的磁盘结构版本。
//! - [`STRUCTURAL_INDEX_VERSION`]：结构化索引产物的格式版本。
//! - 插件 SemVer：实现版本。
//!
//! 把三者绑在一起会让每次插件发版都强制重建用户索引。只有真正改变了产物格式的那一个
//! 版本号该动，其余保持不变。

use crate::error::CodeContextError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// 索引 manifest 的磁盘结构版本。
pub const INDEX_SCHEMA_VERSION: u32 = 1;

/// 结构化索引产物的格式版本。
///
/// Phase 3 引入真实分块后，任何改变 chunk 边界或字段语义的调整都必须提升该版本，
/// 使已有索引进入 `FullRebuildRequired`。
pub const STRUCTURAL_INDEX_VERSION: u32 = 2;

/// 返回当前 Unix 毫秒时间戳。
///
/// 宿主通过 WASI 时钟提供墙钟；读取失败时返回 0 而不是 panic——时间戳只用于展示与
/// 排序，拿不到它不该让插件加载失败。
fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// 当前仓库的稳定身份。
///
/// 由宿主注入的可信工作区根派生，Guest 不能自行声明。使用摘要而不是原始路径，
/// 避免把用户的绝对路径写进持久化数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RepositoryIdentity(String);

impl RepositoryIdentity {
    /// 从宿主注入的工作区根派生仓库身份。
    ///
    /// # Errors
    ///
    /// 工作区根为空白时返回 [`CodeContextError::RepositoryUnavailable`]：没有可信身份时
    /// 必须失败，而不是退化成一个所有项目共用的常量身份——那会让 A 项目读到 B 项目的索引。
    pub fn from_workspace_root(root: &str) -> Result<Self, CodeContextError> {
        if root.trim().is_empty() {
            return Err(CodeContextError::RepositoryUnavailable);
        }
        let mut digest = Sha256::new();
        digest.update(b"code-context-repository-identity-v1\0");
        digest.update(root.as_bytes());
        Ok(Self(format!("{:x}", digest.finalize())))
    }

    /// 返回身份摘要的字符串形式。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 一个嵌入空间的完整定义。
///
/// 两个嵌入空间只要任一字段不同，向量就不可比较。把这些字段折算成
/// [`EmbeddingSpace::id`] 后，语义索引的兼容性判断就退化成一次字符串比较。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingSpace {
    /// provider 类别，例如 `api` 或 `local`。
    pub provider_type: String,
    /// provider 实例标识，例如配置的 base_url 摘要。
    pub provider_id: String,
    /// 模型名称。
    pub model: String,
    /// 模型修订标识；provider 不提供时为空字符串。
    pub model_revision: String,
    /// 向量维度。
    pub dimension: u32,
    /// 是否对向量做了归一化。
    pub normalization: bool,
    /// 查询侧提示模板版本。
    pub query_template_version: u32,
    /// 文档侧提示模板版本。
    pub document_template_version: u32,
}

impl EmbeddingSpace {
    /// 计算该嵌入空间的稳定标识。
    ///
    /// 字段按固定顺序、带长度前缀写入摘要，避免相邻可变长字段拼接产生歧义。
    pub fn id(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"code-context-embedding-space-v1\0");
        for field in [
            self.provider_type.as_str(),
            self.provider_id.as_str(),
            self.model.as_str(),
            self.model_revision.as_str(),
        ] {
            digest.update((field.len() as u64).to_be_bytes());
            digest.update(field.as_bytes());
        }
        digest.update(self.dimension.to_be_bytes());
        digest.update([u8::from(self.normalization)]);
        digest.update(self.query_template_version.to_be_bytes());
        digest.update(self.document_template_version.to_be_bytes());
        format!("{:x}", digest.finalize())
    }

    /// 判断两个嵌入空间的向量是否可以放在同一个索引里比较。
    ///
    /// # 维度相同不等于可比较
    ///
    /// 这是语义索引最容易犯的错：两个模型都输出 1024 维，于是复用同一批向量。
    /// 但不同模型的 1024 维空间之间没有任何几何关系，混用得到的相似度是噪声——
    /// 而且**不会报错**，只会让检索结果长期缓慢地不正确。
    ///
    /// 因此这里比较的是完整标识而不是维度：provider、模型、修订、归一化策略与提示模板
    /// 版本任一不同，向量就不可比较。
    pub fn is_compatible_with(&self, other: &EmbeddingSpace) -> bool {
        self.id() == other.id()
    }
}

/// 索引生命周期状态。
///
/// 刻意不用 `bool is_ready`：引擎在"没有索引"、"正在建"、"能用但语义分支缺席"和
/// "需要重建"之间的差别，直接决定 `code_search` 该返回什么以及界面该显示什么，
/// 用布尔值表达会把这些差别全部抹平。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexStatus {
    /// 尚未建立任何索引。
    NotIndexed,
    /// 正在获取仓库能力并恢复持久状态。
    Initializing,
    /// 正在构建索引。
    Indexing,
    /// 索引就绪，全部检索分支可用。
    Ready,
    /// 索引可用，但语义分支缺席（provider 不可用或未配置）。
    Degraded,
    /// 结构索引仍然有效，语义索引需要重建。
    NeedsSemanticRebuild,
    /// 全部索引需要重建。
    NeedsFullRebuild,
    /// 索引处于错误状态，需要人工介入或重建。
    Error,
}

impl Default for IndexStatus {
    /// 未附着仓库的引擎默认没有索引。
    fn default() -> Self {
        Self::NotIndexed
    }
}

impl IndexStatus {
    /// 返回稳定的机器可读名称，用于工具返回值与日志。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotIndexed => "not_indexed",
            Self::Initializing => "initializing",
            Self::Indexing => "indexing",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::NeedsSemanticRebuild => "needs_semantic_rebuild",
            Self::NeedsFullRebuild => "needs_full_rebuild",
            Self::Error => "error",
        }
    }

    /// 判断当前状态能否提供检索结果。
    pub fn can_serve_search(self) -> bool {
        matches!(self, Self::Ready | Self::Degraded)
    }
}

/// manifest 与当前插件的兼容级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestCompatibility {
    /// 完全兼容，可直接使用已有索引。
    Compatible,
    /// 结构索引可用，语义索引需要重建。
    SemanticRebuildRequired,
    /// 已有索引不可用，需要全量重建。
    FullRebuildRequired,
}

impl ManifestCompatibility {
    /// 把兼容级别映射为对应的索引状态。
    pub fn to_status(self) -> IndexStatus {
        match self {
            Self::Compatible => IndexStatus::Ready,
            Self::SemanticRebuildRequired => IndexStatus::NeedsSemanticRebuild,
            Self::FullRebuildRequired => IndexStatus::NeedsFullRebuild,
        }
    }
}

/// 持久化的索引描述。
///
/// 这是判断"磁盘上那份索引还能不能用"的唯一依据，因此它必须先于任何索引数据写入，
/// 并在每次索引产物变化后更新。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexManifest {
    /// manifest 自身的磁盘结构版本。
    pub schema_version: u32,
    /// 写入该 manifest 的插件实现版本，仅用于诊断，不参与兼容判断。
    pub plugin_version: String,
    /// 该索引所属仓库的稳定身份。
    pub repository_identity: RepositoryIdentity,
    /// 结构化索引产物的格式版本。
    pub structural_index_version: u32,
    /// 语义索引使用的嵌入空间标识；尚未建立语义索引时为 `None`。
    #[serde(default)]
    pub embedding_space_id: Option<String>,
    /// 首次创建时间，Unix 毫秒。
    pub created_at: u64,
    /// 最近更新时间，Unix 毫秒。
    pub updated_at: u64,
}

impl IndexManifest {
    /// 为指定仓库创建一份全新 manifest。
    pub fn new(repository_identity: RepositoryIdentity) -> Self {
        let now = now_unix_ms();
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            plugin_version: env!("CARGO_PKG_VERSION").to_string(),
            repository_identity,
            structural_index_version: STRUCTURAL_INDEX_VERSION,
            embedding_space_id: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// 刷新更新时间戳。
    pub fn touch(&mut self) {
        self.updated_at = now_unix_ms();
    }

    /// 校验从存储读回的 manifest。
    ///
    /// 版本不兼容单列为 [`CodeContextError::UnsupportedManifestVersion`]，因为它对应的是
    /// "重建"而不是"损坏"；其余结构问题一律按无效 manifest 处理。
    ///
    /// # Errors
    ///
    /// schema 版本不匹配、仓库身份为空或时间戳倒挂时返回错误。
    pub fn validate(&self) -> Result<(), CodeContextError> {
        if self.schema_version != INDEX_SCHEMA_VERSION {
            return Err(CodeContextError::UnsupportedManifestVersion {
                found: self.schema_version,
                expected: INDEX_SCHEMA_VERSION,
            });
        }
        if self.repository_identity.as_str().trim().is_empty() {
            return Err(CodeContextError::InvalidManifest("仓库身份不能为空".into()));
        }
        if self.updated_at < self.created_at {
            return Err(CodeContextError::InvalidManifest(
                "更新时间早于创建时间".into(),
            ));
        }
        Ok(())
    }

    /// 判断已有 manifest 与当前仓库、当前期望嵌入空间的兼容级别。
    ///
    /// `expected_embedding_space` 为 `None` 表示当前未配置语义索引；此时已有的语义索引
    /// 不需要重建，只是不会被使用——把它判成需要重建会让用户每次关闭再打开语义模式
    /// 都白白丢一次向量。
    pub fn compatibility(
        &self,
        repository_identity: &RepositoryIdentity,
        expected_embedding_space: Option<&str>,
    ) -> ManifestCompatibility {
        if self.schema_version != INDEX_SCHEMA_VERSION
            || self.structural_index_version != STRUCTURAL_INDEX_VERSION
            || &self.repository_identity != repository_identity
        {
            return ManifestCompatibility::FullRebuildRequired;
        }
        match (expected_embedding_space, self.embedding_space_id.as_deref()) {
            (None, _) => ManifestCompatibility::Compatible,
            (Some(expected), Some(actual)) if expected == actual => {
                ManifestCompatibility::Compatible
            }
            (Some(_), _) => ManifestCompatibility::SemanticRebuildRequired,
        }
    }
}

/// 符号种类。
///
/// 只覆盖 Phase 3 的语言适配器确定会产出的种类；新增语言引入新种类时再扩展。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    /// 函数或方法。
    Function,
    /// 结构体、类或记录。
    Struct,
    /// 枚举。
    Enum,
    /// trait、接口或协议。
    Trait,
    /// 实现块。
    Impl,
    /// 模块或命名空间。
    Module,
    /// 常量或静态项。
    Constant,
    /// 类型别名。
    TypeAlias,
    /// 未能归类的符号。
    Other,
}

/// 一个代码符号。
///
/// 与 [`CodeChunk`] 分开：一个 chunk 可能承载多个符号（例如一个 impl 块），而符号表
/// 需要独立的、按限定名排序的结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Symbol {
    /// 符号短名。
    pub name: String,
    /// 语言内的限定名，例如 `PermissionManager::check`。
    pub qualified_name: String,
    /// 符号种类。
    pub kind: SymbolKind,
    /// 所属文件的仓库相对路径。
    pub file_path: String,
    /// 起始行，从 1 开始。
    pub start_line: u32,
    /// 结束行，闭区间。
    pub end_line: u32,
}

/// 一个结构化代码块。
///
/// 刻意**不持有正文**：正文按 `(file_path, start_line, end_line)` 在返回结果时重读。
/// 这既让 64 MiB 线性内存下的索引规模可控，又让外部编辑能在读取时被内容哈希发现。
/// 详见 `docs/code-context-engine/adr-storage.md`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodeChunk {
    /// 仓库内稳定唯一的块 ID。
    pub id: String,
    /// 所属仓库身份。
    pub repository_id: RepositoryIdentity,
    /// 仓库相对路径。
    pub file_path: String,
    /// 语言标识，例如 `rust`、`typescript`；未识别时为 `text`。
    pub language: String,
    /// 主符号短名；纯文本块为 `None`。
    #[serde(default)]
    pub symbol_name: Option<String>,
    /// 主符号限定名。
    #[serde(default)]
    pub qualified_symbol: Option<String>,
    /// 主符号种类。
    #[serde(default)]
    pub symbol_kind: Option<SymbolKind>,
    /// 父符号限定名，用于表达嵌套结构。
    #[serde(default)]
    pub parent_symbol: Option<String>,
    /// 起始行，从 1 开始。
    pub start_line: u32,
    /// 结束行，闭区间。
    pub end_line: u32,
    /// 符号签名行。
    #[serde(default)]
    pub signature: Option<String>,
    /// 紧邻的文档注释。
    #[serde(default)]
    pub doc_comment: Option<String>,
    /// 该代码块是否属于测试代码。
    ///
    /// 测试的文档注释往往把被测行为描述得比实现本身还清楚，因此在自然语言检索里会**压过
    /// 实现代码**——实测中"代码块是怎么切分出来的"的语义第一名是一个测试函数。用户问
    /// "某功能实现在哪"时要的几乎总是实现，所以检索层需要这一位来降权，而不是把测试
    /// 从索引里删掉（有时确实要找测试）。
    #[serde(default)]
    pub is_test: bool,
    /// 是否只参与语义检索。
    ///
    /// 结构化解析会把相邻的过短兄弟声明合成一个复合块，使嵌入模型看到足够完整的行为
    /// 上下文。复合块不进入符号表和词法倒排，避免与保留精确定位的原子块重复召回。
    #[serde(default)]
    pub semantic_only: bool,
    /// 当前原子块所属的语义复合块 ID。
    ///
    /// 非空时，该原子块继续服务符号与词法检索，但不再单独生成向量；语义通道改由对应
    /// 复合块代表。复合块自身与未合并块为 `None`。
    #[serde(default)]
    pub semantic_group_id: Option<String>,
    /// 该文件的导入语句，用于补全检索上下文。
    #[serde(default)]
    pub imports: Vec<String>,
    /// 块正文的内容哈希，用于增量判断与返回前的一致性校验。
    pub content_hash: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试用仓库身份。
    fn identity(root: &str) -> RepositoryIdentity {
        RepositoryIdentity::from_workspace_root(root).expect("测试工作区根应有效")
    }

    /// 不同工作区必须得到不同身份，相同工作区必须稳定。
    #[test]
    fn repository_identity_is_stable_and_distinct() {
        assert_eq!(identity("/repo/a"), identity("/repo/a"));
        assert_ne!(identity("/repo/a"), identity("/repo/b"));
    }

    /// 空工作区根必须失败，不能退化成共用身份。
    #[test]
    fn empty_workspace_root_is_rejected() {
        assert_eq!(
            RepositoryIdentity::from_workspace_root("   "),
            Err(CodeContextError::RepositoryUnavailable)
        );
    }

    /// 仓库身份不得把用户的绝对路径写进持久化数据。
    #[test]
    fn repository_identity_does_not_leak_path() {
        let identity = identity("/Users/someone/secret-project");
        assert!(!identity.as_str().contains("secret-project"));
        assert!(!identity.as_str().contains('/'));
    }

    /// 新建 manifest 必须自洽并通过校验。
    #[test]
    fn fresh_manifest_validates() {
        let manifest = IndexManifest::new(identity("/repo/a"));
        manifest.validate().expect("新建 manifest 应有效");
        assert_eq!(manifest.schema_version, INDEX_SCHEMA_VERSION);
        assert!(manifest.embedding_space_id.is_none());
    }

    /// manifest 必须能完整往返序列化。
    #[test]
    fn manifest_round_trips_through_json() {
        let manifest = IndexManifest::new(identity("/repo/a"));
        let encoded = serde_json::to_value(&manifest).expect("序列化 manifest");
        let decoded: IndexManifest = serde_json::from_value(encoded).expect("反序列化 manifest");
        assert_eq!(manifest, decoded);
    }

    /// 未来 schema 版本必须被识别为不支持，而不是被当成损坏数据。
    #[test]
    fn future_schema_version_is_unsupported() {
        let mut manifest = IndexManifest::new(identity("/repo/a"));
        manifest.schema_version = INDEX_SCHEMA_VERSION + 99;
        assert_eq!(
            manifest.validate(),
            Err(CodeContextError::UnsupportedManifestVersion {
                found: INDEX_SCHEMA_VERSION + 99,
                expected: INDEX_SCHEMA_VERSION,
            })
        );
    }

    /// 时间戳倒挂说明 manifest 被外部改写过，必须拒绝。
    #[test]
    fn inverted_timestamps_are_rejected() {
        let mut manifest = IndexManifest::new(identity("/repo/a"));
        manifest.created_at = 100;
        manifest.updated_at = 50;
        assert!(matches!(
            manifest.validate(),
            Err(CodeContextError::InvalidManifest(_))
        ));
    }

    /// 换仓库必须触发全量重建，不能复用其他项目的索引。
    #[test]
    fn different_repository_requires_full_rebuild() {
        let manifest = IndexManifest::new(identity("/repo/a"));
        assert_eq!(
            manifest.compatibility(&identity("/repo/b"), None),
            ManifestCompatibility::FullRebuildRequired
        );
    }

    /// 结构索引格式变化必须触发全量重建。
    #[test]
    fn structural_version_change_requires_full_rebuild() {
        let mut manifest = IndexManifest::new(identity("/repo/a"));
        manifest.structural_index_version = STRUCTURAL_INDEX_VERSION + 1;
        assert_eq!(
            manifest.compatibility(&identity("/repo/a"), None),
            ManifestCompatibility::FullRebuildRequired
        );
    }

    /// 插件实现版本变化不得强制重建用户索引。
    #[test]
    fn plugin_version_change_alone_stays_compatible() {
        let mut manifest = IndexManifest::new(identity("/repo/a"));
        manifest.plugin_version = "99.0.0".into();
        assert_eq!(
            manifest.compatibility(&identity("/repo/a"), None),
            ManifestCompatibility::Compatible
        );
    }

    /// 嵌入空间变化只需要重建语义索引，结构索引保留。
    #[test]
    fn embedding_space_change_requires_semantic_rebuild_only() {
        let mut manifest = IndexManifest::new(identity("/repo/a"));
        manifest.embedding_space_id = Some("space-a".into());
        assert_eq!(
            manifest.compatibility(&identity("/repo/a"), Some("space-b")),
            ManifestCompatibility::SemanticRebuildRequired
        );
        assert_eq!(
            manifest.compatibility(&identity("/repo/a"), Some("space-a")),
            ManifestCompatibility::Compatible
        );
    }

    /// 关闭语义模式不得丢弃已有向量。
    #[test]
    fn disabling_semantic_mode_keeps_existing_index_compatible() {
        let mut manifest = IndexManifest::new(identity("/repo/a"));
        manifest.embedding_space_id = Some("space-a".into());
        assert_eq!(
            manifest.compatibility(&identity("/repo/a"), None),
            ManifestCompatibility::Compatible
        );
    }

    /// 嵌入空间任一字段变化都必须产生不同标识。
    #[test]
    fn embedding_space_id_reflects_every_field() {
        let base = EmbeddingSpace {
            provider_type: "api".into(),
            provider_id: "provider-a".into(),
            model: "model-a".into(),
            model_revision: String::new(),
            dimension: 768,
            normalization: true,
            query_template_version: 1,
            document_template_version: 1,
        };
        let mut changed_dimension = base.clone();
        changed_dimension.dimension = 1024;
        let mut changed_model = base.clone();
        changed_model.model = "model-b".into();
        let mut changed_normalization = base.clone();
        changed_normalization.normalization = false;

        assert_eq!(base.id(), base.clone().id());
        assert_ne!(base.id(), changed_dimension.id());
        assert_ne!(base.id(), changed_model.id());
        assert_ne!(base.id(), changed_normalization.id());
    }

    /// 维度相同但模型不同的两个空间必须判定为不兼容。
    ///
    /// 这条是语义索引的核心防线：两个模型都是 1024 维不代表向量可以混用，
    /// 混用不会报错，只会让相似度变成噪声。
    #[test]
    fn same_dimension_different_model_is_incompatible() {
        let base = EmbeddingSpace {
            provider_type: "api".into(),
            provider_id: "provider-a".into(),
            model: "model-a".into(),
            model_revision: "r1".into(),
            dimension: 1024,
            normalization: true,
            query_template_version: 1,
            document_template_version: 1,
        };

        let mut other_model = base.clone();
        other_model.model = "model-b".into();
        assert_eq!(other_model.dimension, base.dimension, "前提：维度相同");
        assert!(
            !base.is_compatible_with(&other_model),
            "维度相同但模型不同，向量绝不可比较"
        );

        assert!(base.is_compatible_with(&base.clone()), "同一空间必须兼容");
    }

    /// 空间的每一个字段变化都必须打破兼容性。
    #[test]
    fn every_space_field_affects_compatibility() {
        let base = EmbeddingSpace {
            provider_type: "api".into(),
            provider_id: "provider-a".into(),
            model: "model-a".into(),
            model_revision: "r1".into(),
            dimension: 1024,
            normalization: true,
            query_template_version: 1,
            document_template_version: 1,
        };

        let mutations: Vec<(&str, EmbeddingSpace)> = vec![
            (
                "provider_type",
                EmbeddingSpace {
                    provider_type: "local".into(),
                    ..base.clone()
                },
            ),
            (
                "provider_id",
                EmbeddingSpace {
                    provider_id: "provider-b".into(),
                    ..base.clone()
                },
            ),
            (
                "model",
                EmbeddingSpace {
                    model: "model-b".into(),
                    ..base.clone()
                },
            ),
            (
                "model_revision",
                EmbeddingSpace {
                    model_revision: "r2".into(),
                    ..base.clone()
                },
            ),
            (
                "dimension",
                EmbeddingSpace {
                    dimension: 768,
                    ..base.clone()
                },
            ),
            (
                "normalization",
                EmbeddingSpace {
                    normalization: false,
                    ..base.clone()
                },
            ),
            (
                "query_template_version",
                EmbeddingSpace {
                    query_template_version: 2,
                    ..base.clone()
                },
            ),
            (
                "document_template_version",
                EmbeddingSpace {
                    document_template_version: 2,
                    ..base.clone()
                },
            ),
        ];

        for (field, mutated) in mutations {
            assert!(
                !base.is_compatible_with(&mutated),
                "{field} 变化后必须判定为不兼容"
            );
        }
    }

    /// 切换嵌入空间必须只触发语义重建，结构索引原样保留。
    #[test]
    fn switching_embedding_space_rebuilds_semantics_only() {
        let mut manifest = IndexManifest::new(identity("/repo/a"));
        let api = EmbeddingSpace {
            provider_type: "api".into(),
            provider_id: "provider-a".into(),
            model: "model-a".into(),
            model_revision: String::new(),
            dimension: 1024,
            normalization: true,
            query_template_version: 1,
            document_template_version: 1,
        };
        let local = EmbeddingSpace {
            provider_type: "local".into(),
            ..api.clone()
        };
        manifest.embedding_space_id = Some(api.id());

        assert_eq!(
            manifest.compatibility(&identity("/repo/a"), Some(&local.id())),
            ManifestCompatibility::SemanticRebuildRequired,
            "换 provider 只需重建语义索引，不应丢弃结构索引"
        );
    }

    /// 只有就绪与降级状态可以返回检索结果。
    #[test]
    fn only_ready_and_degraded_serve_search() {
        assert!(IndexStatus::Ready.can_serve_search());
        assert!(IndexStatus::Degraded.can_serve_search());
        assert!(!IndexStatus::NotIndexed.can_serve_search());
        assert!(!IndexStatus::Initializing.can_serve_search());
        assert!(!IndexStatus::Indexing.can_serve_search());
        assert!(!IndexStatus::NeedsFullRebuild.can_serve_search());
        assert!(!IndexStatus::Error.can_serve_search());
    }
}

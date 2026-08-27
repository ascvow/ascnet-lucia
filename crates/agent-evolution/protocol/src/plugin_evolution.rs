//! 插件源码变异、可信构建、能力审批与 Canary 发布的稳定协议。
//!
//! 本模块只定义跨进程数据契约和可复核的不变量，不执行源码、构建 Component、扫描
//! WIT、验证密码学签名或切换发布流量。尤其需要注意：Mutation Proposal 内的能力和
//! Component 接口均是 Candidate 自报输入，不是可信事实。受信构建平面必须从源码 CAS
//! 重建 Component，再由独立扫描器重建能力与接口快照；Release Controller 只能接受该
//! 受信结果，不能复制 Candidate 的声明。

use crate::{
    ArtifactDigest, CandidateId, EpisodeId, EvaluationReportId, EvolutionCycleId, GenomeDigest,
    MutationId, ReleaseId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

/// 插件源码快照协议版本。
pub const PLUGIN_SOURCE_ARTIFACT_SCHEMA_VERSION: u32 = 1;
/// 插件变异提案协议版本。
pub const PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION: u32 = 1;
/// 插件能力集合协议版本。
pub const PLUGIN_CAPABILITY_SET_SCHEMA_VERSION: u32 = 1;
/// 插件能力 Profile 协议版本。
pub const PLUGIN_CAPABILITY_PROFILE_SCHEMA_VERSION: u32 = 1;
/// Component 接口快照协议版本。
pub const COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION: u32 = 1;
/// 插件构建证明协议版本。
pub const PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION: u32 = 1;
/// 插件审计检查协议版本。
pub const PLUGIN_AUDIT_CHECK_SCHEMA_VERSION: u32 = 1;
/// Host 插件审计证据协议版本。
pub const PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION: u32 = 1;
/// 插件独立评测证据协议版本。
pub const PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION: u32 = 1;
/// 插件源码 Gate 输入协议版本。
pub const PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION: u32 = 1;
/// 插件源码评测报告协议版本。
pub const PLUGIN_EVALUATION_REPORT_SCHEMA_VERSION: u32 = 1;
/// 密码学签名信封协议版本。
pub const SIGNATURE_ENVELOPE_SCHEMA_VERSION: u32 = 1;
/// 能力扩张请求协议版本。
pub const CAPABILITY_EXPANSION_REQUEST_SCHEMA_VERSION: u32 = 1;
/// 插件审批记录协议版本。
pub const PLUGIN_APPROVAL_RECORD_SCHEMA_VERSION: u32 = 1;
/// 插件发布信封协议版本。
pub const PLUGIN_RELEASE_ENVELOPE_SCHEMA_VERSION: u32 = 1;
/// 插件 Canary 记录协议版本。
pub const PLUGIN_CANARY_RECORD_SCHEMA_VERSION: u32 = 1;

const MAX_PLUGIN_ID_BYTES: usize = 128;
const MAX_SOURCE_PATH_BYTES: usize = 512;
const MAX_SOURCE_FILES: usize = 4_096;
const MAX_SOURCE_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SOURCE_TOTAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_PATCHES: usize = 1_024;
const MAX_CAPABILITIES: usize = 256;
const MAX_CAPABILITY_BYTES: usize = 160;
const MAX_INTERFACE_ITEMS: usize = 2_048;
const MAX_INTERFACE_ITEM_BYTES: usize = 320;
const MAX_WORLD_BYTES: usize = 256;
const MAX_RATIONALE_BYTES: usize = 8 * 1024;
const MAX_STABLE_ID_BYTES: usize = 160;
const MAX_POLICY_VERSION_BYTES: usize = 128;
const MAX_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_EVIDENCE_EPISODES: usize = 256;
const MAX_EVALUATION_CASES: u32 = 1_000_000;
const ED25519_SIGNATURE_HEX_BYTES: usize = 128;

/// 源码树中的一个普通文件。
///
/// 路径必须是相对源码根的规范 POSIX 路径。该协议不表达目录或符号链接；受信源码扫描器
/// 必须拒绝符号链接、设备文件及其他特殊文件，再构造本类型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSourceFile {
    /// 相对源码根的规范 POSIX 路径。
    pub path: String,
    /// 文件原始字节的 SHA-256 摘要。
    pub digest: ArtifactDigest,
    /// 文件原始字节长度。
    pub size_bytes: u64,
}

impl PluginSourceFile {
    /// 校验路径安全边界和单文件大小上限。
    ///
    /// # Errors
    ///
    /// 路径为空、不是规范相对路径，或文件超过协议上限时返回
    /// [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_source_path(&self.path)?;
        if self.size_bytes > MAX_SOURCE_FILE_BYTES {
            return Err(InvalidPluginEvolution::FileTooLarge {
                path: self.path.clone(),
                size_bytes: self.size_bytes,
                max_bytes: MAX_SOURCE_FILE_BYTES,
            });
        }
        Ok(())
    }
}

/// 一个规范、可寻址的插件源码树。
///
/// `files` 必须按路径严格升序排列。源码树摘要由 [`Self::digest`] 对完整规范清单计算，
/// 不依赖归档文件顺序、文件系统遍历顺序或平台路径分隔符。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginSourceArtifact {
    /// 源码快照 schema 版本。
    pub schema_version: u32,
    /// 源码所属插件的稳定 ID。
    pub plugin_id: String,
    /// 按路径严格升序排列的普通文件清单。
    pub files: Vec<PluginSourceFile>,
}

impl PluginSourceArtifact {
    /// 从任意顺序的文件清单构造规范源码树。
    ///
    /// 本函数会稳定排序，但不会合并重复路径；重复项会被拒绝，防止不同解析器对覆盖顺序
    /// 产生分歧。
    ///
    /// # Errors
    ///
    /// 插件 ID、文件、路径唯一性或总体大小不合法时返回 [`InvalidPluginEvolution`]。
    pub fn new(
        plugin_id: impl Into<String>,
        mut files: Vec<PluginSourceFile>,
    ) -> Result<Self, InvalidPluginEvolution> {
        files.sort_by(|left, right| left.path.cmp(&right.path));
        let artifact = Self {
            schema_version: PLUGIN_SOURCE_ARTIFACT_SCHEMA_VERSION,
            plugin_id: plugin_id.into(),
            files,
        };
        artifact.validate()?;
        Ok(artifact)
    }

    /// 校验 schema、插件 ID、规范路径顺序、摘要元数据一致性和总大小上限。
    ///
    /// # Errors
    ///
    /// 任一结构不变量不成立时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginSourceArtifact",
            self.schema_version,
            PLUGIN_SOURCE_ARTIFACT_SCHEMA_VERSION,
        )?;
        validate_plugin_id(&self.plugin_id)?;
        validate_count("source.files", self.files.len(), 1, MAX_SOURCE_FILES)?;
        validate_strictly_sorted_paths(&self.files)?;

        let mut total_bytes = 0_u64;
        let mut sizes_by_digest = BTreeMap::<&ArtifactDigest, u64>::new();
        for file in &self.files {
            file.validate()?;
            total_bytes = total_bytes.checked_add(file.size_bytes).ok_or(
                InvalidPluginEvolution::SourceTooLarge {
                    size_bytes: u64::MAX,
                    max_bytes: MAX_SOURCE_TOTAL_BYTES,
                },
            )?;
            if let Some(previous_size) = sizes_by_digest.insert(&file.digest, file.size_bytes) {
                if previous_size != file.size_bytes {
                    return Err(InvalidPluginEvolution::DigestSizeMismatch {
                        digest: file.digest.clone(),
                    });
                }
            }
        }
        if total_bytes > MAX_SOURCE_TOTAL_BYTES {
            return Err(InvalidPluginEvolution::SourceTooLarge {
                size_bytes: total_bytes,
                max_bytes: MAX_SOURCE_TOTAL_BYTES,
            });
        }
        Ok(())
    }

    /// 返回稳定 JSON 字节，用于 CAS 索引和签名绑定。
    ///
    /// # Errors
    ///
    /// 源码树不合法或 JSON 序列化失败时返回 [`InvalidPluginEvolution`]。
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, InvalidPluginEvolution> {
        self.validate()?;
        canonical_json(self)
    }

    /// 计算规范源码树的 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// 源码树不合法、无法序列化或摘要类型构造失败时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }

    /// 按规范路径查找文件。
    pub fn file(&self, path: &str) -> Option<&PluginSourceFile> {
        self.files
            .binary_search_by(|file| file.path.as_str().cmp(path))
            .ok()
            .map(|index| &self.files[index])
    }
}

/// 对插件源码树执行的一项内容补丁。
///
/// Create 只携带新摘要，Update 同时绑定旧摘要和新摘要，Delete 只携带旧摘要。提案校验会
/// 将每项补丁与 Parent/Candidate 两棵完整源码树逐项复核，并要求补丁集合精确覆盖真实
/// 差异，避免摘要错绑、漏补丁和对未变化文件伪造补丁。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginFilePatch {
    /// 新建此前不存在的文件。
    Create {
        /// 新文件的规范相对路径。
        path: String,
        /// Candidate 文件内容摘要。
        new_digest: ArtifactDigest,
    },
    /// 更新 Parent 中已有的文件。
    Update {
        /// 被更新文件的规范相对路径。
        path: String,
        /// Parent 文件内容摘要。
        old_digest: ArtifactDigest,
        /// Candidate 文件内容摘要。
        new_digest: ArtifactDigest,
    },
    /// 删除 Parent 中已有的文件。
    Delete {
        /// 被删除文件的规范相对路径。
        path: String,
        /// Parent 文件内容摘要。
        old_digest: ArtifactDigest,
    },
}

impl PluginFilePatch {
    /// 返回补丁作用的规范相对路径。
    pub fn path(&self) -> &str {
        match self {
            Self::Create { path, .. } | Self::Update { path, .. } | Self::Delete { path, .. } => {
                path
            }
        }
    }

    /// 校验路径，并拒绝没有实际内容变化的 Update。
    ///
    /// # Errors
    ///
    /// 路径不安全或 Update 的新旧摘要相同时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_source_path(self.path())?;
        if let Self::Update {
            old_digest,
            new_digest,
            ..
        } = self
        {
            if old_digest == new_digest {
                return Err(InvalidPluginEvolution::UnchangedUpdate {
                    path: self.path().to_string(),
                });
            }
        }
        Ok(())
    }
}

/// 有规范顺序的插件能力集合。
///
/// 能力名必须是小写 ASCII 稳定标识，且 `capabilities` 必须严格升序。使用 [`Self::new`]
/// 可从任意输入顺序构造规范集合；重复能力不会被静默合并，而会被拒绝。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCapabilitySet {
    /// 能力集合 schema 版本。
    pub schema_version: u32,
    /// 按字节序严格升序排列的能力 ID。
    pub capabilities: Vec<String>,
}

impl PluginCapabilitySet {
    /// 构造规范排序且无重复的能力集合。
    ///
    /// # Errors
    ///
    /// 能力名非法、数量越界或存在重复时返回 [`InvalidPluginEvolution`]。
    pub fn new(mut capabilities: Vec<String>) -> Result<Self, InvalidPluginEvolution> {
        for capability in &capabilities {
            validate_capability(capability)?;
        }
        capabilities.sort();
        if capabilities.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(InvalidPluginEvolution::DuplicateValue {
                field: "capability_set.capabilities",
            });
        }
        let set = Self {
            schema_version: PLUGIN_CAPABILITY_SET_SCHEMA_VERSION,
            capabilities,
        };
        set.validate()?;
        Ok(set)
    }

    /// 返回空的规范能力集合。
    pub fn empty() -> Self {
        Self {
            schema_version: PLUGIN_CAPABILITY_SET_SCHEMA_VERSION,
            capabilities: Vec::new(),
        }
    }

    /// 校验 schema、数量、能力名和严格排序。
    ///
    /// # Errors
    ///
    /// 任一结构不变量不成立时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginCapabilitySet",
            self.schema_version,
            PLUGIN_CAPABILITY_SET_SCHEMA_VERSION,
        )?;
        validate_count(
            "capability_set.capabilities",
            self.capabilities.len(),
            0,
            MAX_CAPABILITIES,
        )?;
        for capability in &self.capabilities {
            validate_capability(capability)?;
        }
        validate_strictly_sorted_strings("capability_set.capabilities", &self.capabilities)
    }

    /// 判断当前集合是否为 `baseline` 的子集。
    ///
    /// 非法集合保守地返回 `false`；需要错误详情的调用方应先显式调用 [`Self::validate`]。
    pub fn is_subset_of(&self, baseline: &Self) -> bool {
        if self.validate().is_err() || baseline.validate().is_err() {
            return false;
        }
        let baseline = baseline.capabilities.iter().collect::<BTreeSet<_>>();
        self.capabilities
            .iter()
            .all(|capability| baseline.contains(capability))
    }

    /// 返回相对 `baseline` 新增的规范能力集合。
    ///
    /// # Errors
    ///
    /// 任一集合不合法时返回 [`InvalidPluginEvolution`]。
    pub fn added_since(&self, baseline: &Self) -> Result<Self, InvalidPluginEvolution> {
        self.validate()?;
        baseline.validate()?;
        let baseline = baseline.capabilities.iter().collect::<BTreeSet<_>>();
        Self::new(
            self.capabilities
                .iter()
                .filter(|capability| !baseline.contains(capability))
                .cloned()
                .collect(),
        )
    }
}

/// 插件对 Host 能力的请求和对外能力的提供快照。
///
/// `requested` 决定插件可能访问的 Host 能力，`provided` 决定它可能进入的 owner 路由。
/// 两部分都影响安全与行为，因此子集判断同时覆盖二者。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityProfile {
    /// 能力 Profile schema 版本。
    pub schema_version: u32,
    /// 插件向 Host 请求的能力集合。
    pub requested: PluginCapabilitySet,
    /// 插件向 Host 提供的能力集合。
    pub provided: PluginCapabilitySet,
}

impl CapabilityProfile {
    /// 构造并校验插件能力 Profile。
    ///
    /// # Errors
    ///
    /// 任一嵌套能力集合不合法时返回 [`InvalidPluginEvolution`]。
    pub fn new(
        requested: PluginCapabilitySet,
        provided: PluginCapabilitySet,
    ) -> Result<Self, InvalidPluginEvolution> {
        let profile = Self {
            schema_version: PLUGIN_CAPABILITY_PROFILE_SCHEMA_VERSION,
            requested,
            provided,
        };
        profile.validate()?;
        Ok(profile)
    }

    /// 校验 schema 和两类能力集合。
    ///
    /// # Errors
    ///
    /// 版本或嵌套集合无效时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "CapabilityProfile",
            self.schema_version,
            PLUGIN_CAPABILITY_PROFILE_SCHEMA_VERSION,
        )?;
        self.requested.validate()?;
        self.provided.validate()
    }

    /// 判断当前 Profile 是否在 `baseline` 的能力范围内。
    pub fn is_subset_of(&self, baseline: &Self) -> bool {
        self.requested.is_subset_of(&baseline.requested)
            && self.provided.is_subset_of(&baseline.provided)
    }

    /// 返回当前 Profile 相对 `baseline` 新增的请求能力和提供能力。
    ///
    /// # Errors
    ///
    /// 任一 Profile 无效时返回 [`InvalidPluginEvolution`]。
    pub fn additions_since(
        &self,
        baseline: &Self,
    ) -> Result<(PluginCapabilitySet, PluginCapabilitySet), InvalidPluginEvolution> {
        self.validate()?;
        baseline.validate()?;
        Ok((
            self.requested.added_since(&baseline.requested)?,
            self.provided.added_since(&baseline.provided)?,
        ))
    }

    /// 计算能力 Profile 的规范 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// Profile 不合法或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }
}

/// 受信扫描器从已构建 Component 重建的接口快照。
///
/// Candidate 可以提交同形状的声明作为构建输入，但该声明不具备可信性。发布链路必须使用
/// 独立扫描器从 `component_digest` 对应的真实字节重建本对象，并记录扫描器修订。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentInterfaceSnapshot {
    /// 接口快照 schema 版本。
    pub schema_version: u32,
    /// Component 所属插件 ID。
    pub plugin_id: String,
    /// 被扫描 Component 的内容摘要。
    pub component_digest: ArtifactDigest,
    /// Component world 的稳定限定名。
    pub world: String,
    /// 按字节序严格升序排列的 import 接口或函数限定名。
    pub imports: Vec<String>,
    /// 按字节序严格升序排列的 export 接口或函数限定名。
    pub exports: Vec<String>,
    /// 受信扫描器二进制或规则集的不可变摘要。
    pub scanner_revision: ArtifactDigest,
}

impl ComponentInterfaceSnapshot {
    /// 校验接口快照的版本、身份、有界字段和规范顺序。
    ///
    /// # Errors
    ///
    /// 任一结构不变量不成立时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "ComponentInterfaceSnapshot",
            self.schema_version,
            COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
        )?;
        validate_plugin_id(&self.plugin_id)?;
        validate_protocol_token("interface.world", &self.world, MAX_WORLD_BYTES)?;
        validate_interface_items("interface.imports", &self.imports)?;
        validate_interface_items("interface.exports", &self.exports)?;
        validate_count(
            "interface.exports",
            self.exports.len(),
            1,
            MAX_INTERFACE_ITEMS,
        )
    }

    /// 计算接口快照的规范 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// 快照不合法或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }
}

/// Create 提案允许使用的固定低权限 Profile。
///
/// 枚举值由受信控制面随协议发布，Candidate 不能提交任意能力集合冒充预批准 Profile。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreapprovedPluginProfile {
    /// 不请求 Host 能力，也不向 owner 路由提供能力的纯计算插件。
    PureCompute,
}

impl PreapprovedPluginProfile {
    /// 返回该预批准 Profile 对应的固定能力集合。
    pub fn capabilities(self) -> CapabilityProfile {
        CapabilityProfile {
            schema_version: PLUGIN_CAPABILITY_PROFILE_SCHEMA_VERSION,
            requested: PluginCapabilitySet::empty(),
            provided: PluginCapabilitySet::empty(),
        }
    }
}

/// 插件源码变异的显式操作类型。
///
/// Create 不表达虚构的空 Parent；Update 必须携带真实 Parent 源码与受信扫描能力。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginMutationKind {
    /// 创建一个不存在的插件。
    Create {
        /// 受信控制面允许本次创建使用的固定低权限 Profile。
        preapproved_profile: PreapprovedPluginProfile,
    },
    /// 更新一个已有插件。
    Update {
        /// Parent 的完整规范源码树；不能使用空源码树代替 Create。
        parent_source: Box<PluginSourceArtifact>,
        /// Parent Component 的受信扫描能力 Profile。
        parent_capabilities: Box<CapabilityProfile>,
    },
}

/// Candidate 提交的插件源码变异提案。
///
/// `claimed_capabilities` 与 `claimed_interface` 仅用于风险预筛和审计，不能进入可信发布
/// 结论。受信构建器必须忽略其可信含义，从 Candidate 源码构建真实 Component，并由扫描器
/// 重建 [`CapabilityProfile`] 与 [`ComponentInterfaceSnapshot`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMutationProposal {
    /// 插件变异提案 schema 版本。
    pub schema_version: u32,
    /// 所属进化 Cycle。
    pub cycle_id: EvolutionCycleId,
    /// 变异提案 ID。
    pub mutation_id: MutationId,
    /// Candidate ID。
    pub candidate_id: CandidateId,
    /// 目标插件 ID。
    pub plugin_id: String,
    /// Parent Genome 摘要，用于拒绝过期提案。
    pub parent_genome_digest: GenomeDigest,
    /// Candidate Genome 摘要，用于绑定后续评测与发布。
    pub candidate_genome_digest: GenomeDigest,
    /// Create 或 Update 的显式结构化输入。
    pub mutation: PluginMutationKind,
    /// Candidate 的完整规范源码树。
    pub candidate_source: PluginSourceArtifact,
    /// 精确覆盖源码变化的补丁，按路径严格升序排列。
    pub patches: Vec<PluginFilePatch>,
    /// Candidate 自报能力，只作不可信输入。
    pub claimed_capabilities: CapabilityProfile,
    /// Candidate 自报 Component 接口，只作不可信输入。
    pub claimed_interface: ComponentInterfaceSnapshot,
    /// 支撑变异的脱敏 Episode，按 ID 严格升序排列。
    pub evidence_episode_ids: Vec<EpisodeId>,
    /// 有界、可审计的变异理由，不得包含源码或 Secret 正文。
    pub rationale: String,
    /// 提案创建的 Unix 毫秒时间。
    pub created_at_ms: u64,
}

impl PluginMutationProposal {
    /// 校验提案身份、Create/Update 结构、补丁摘要绑定、声明能力与有界字段。
    ///
    /// 本方法不读取 CAS、执行编译或信任 Candidate 声明，不能替代受信构建与扫描。
    ///
    /// # Errors
    ///
    /// 版本、身份、源码树、补丁、能力、接口、证据或时间不合法时返回
    /// [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginMutationProposal",
            self.schema_version,
            PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION,
        )?;
        validate_plugin_id(&self.plugin_id)?;
        if self.parent_genome_digest == self.candidate_genome_digest {
            return Err(InvalidPluginEvolution::UnchangedGenome);
        }
        self.candidate_source.validate()?;
        self.claimed_capabilities.validate()?;
        self.claimed_interface.validate()?;
        if self.candidate_source.plugin_id != self.plugin_id
            || self.claimed_interface.plugin_id != self.plugin_id
        {
            return Err(InvalidPluginEvolution::NestedIdentityMismatch);
        }
        match &self.mutation {
            PluginMutationKind::Create {
                preapproved_profile,
            } => {
                validate_create_patch_set(&self.candidate_source, &self.patches)?;
                if self.claimed_capabilities != preapproved_profile.capabilities() {
                    return Err(InvalidPluginEvolution::PreapprovedProfileMismatch);
                }
            }
            PluginMutationKind::Update {
                parent_source,
                parent_capabilities,
            } => {
                parent_source.validate()?;
                parent_capabilities.validate()?;
                if parent_source.plugin_id != self.plugin_id {
                    return Err(InvalidPluginEvolution::NestedIdentityMismatch);
                }
                if parent_source.digest()? == self.candidate_source.digest()? {
                    return Err(InvalidPluginEvolution::UnchangedSource);
                }
                validate_patch_set(parent_source, &self.candidate_source, &self.patches)?;
                if !self.claimed_capabilities.is_subset_of(parent_capabilities) {
                    return Err(InvalidPluginEvolution::ClaimedCapabilityExpansion);
                }
            }
        }
        validate_strictly_sorted_ord("proposal.evidence_episode_ids", &self.evidence_episode_ids)?;
        validate_count(
            "proposal.evidence_episode_ids",
            self.evidence_episode_ids.len(),
            1,
            MAX_EVIDENCE_EPISODES,
        )?;
        validate_text("proposal.rationale", &self.rationale, MAX_RATIONALE_BYTES)?;
        validate_nonzero_time("proposal.created_at_ms", self.created_at_ms)
    }

    /// 计算完整提案的规范 SHA-256 摘要，用于构建证明和发布防重放绑定。
    ///
    /// # Errors
    ///
    /// 提案不合法或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }
}

/// 受信构建平面对插件 Component 的不可变证明。
///
/// 该对象必须由隔离构建器和独立扫描器产生。`capabilities` 与 `interface` 是对真实构建
/// 产物的扫描结果，不得从 [`PluginMutationProposal`] 的 Candidate 自报字段复制。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginBuildAttestation {
    /// 构建证明 schema 版本。
    pub schema_version: u32,
    /// 受信构建任务的稳定 ID。
    pub build_id: String,
    /// 构建目标插件 ID。
    pub plugin_id: String,
    /// 被构建的 Mutation ID。
    pub mutation_id: MutationId,
    /// 被构建的 Candidate ID。
    pub candidate_id: CandidateId,
    /// 完整 Mutation Proposal 的规范摘要。
    pub proposal_digest: ArtifactDigest,
    /// 实际构建源码树的规范摘要。
    pub source_digest: ArtifactDigest,
    /// 构建产出的 Component 摘要。
    pub component_digest: ArtifactDigest,
    /// 构建产出的 Component 字节长度。
    pub component_size_bytes: u64,
    /// 可信扫描器重建的接口快照。
    pub interface: ComponentInterfaceSnapshot,
    /// 可信扫描器重建的能力 Profile。
    pub capabilities: CapabilityProfile,
    /// 隔离构建环境的不可变摘要。
    pub build_environment_digest: ArtifactDigest,
    /// 构建器二进制和配置的不可变摘要。
    pub builder_revision: ArtifactDigest,
    /// 脱敏构建日志的 CAS 摘要。
    pub build_log_digest: ArtifactDigest,
    /// 相同源码和环境是否完成独立重复构建并得到相同 Component 摘要。
    pub reproducible: bool,
    /// 构建完成的 Unix 毫秒时间。
    pub built_at_ms: u64,
}

impl PluginBuildAttestation {
    /// 校验构建证明的身份、产物大小、扫描绑定和时间。
    ///
    /// 本方法不验证构建真实性；调用方还必须验证受信构建器签名。
    ///
    /// # Errors
    ///
    /// 结构或嵌套扫描结果不一致时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginBuildAttestation",
            self.schema_version,
            PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION,
        )?;
        validate_stable_id("attestation.build_id", &self.build_id)?;
        validate_plugin_id(&self.plugin_id)?;
        if self.component_size_bytes == 0 || self.component_size_bytes > MAX_COMPONENT_BYTES {
            return Err(InvalidPluginEvolution::InvalidComponentSize {
                size_bytes: self.component_size_bytes,
                max_bytes: MAX_COMPONENT_BYTES,
            });
        }
        self.interface.validate()?;
        self.capabilities.validate()?;
        if self.interface.plugin_id != self.plugin_id
            || self.interface.component_digest != self.component_digest
        {
            return Err(InvalidPluginEvolution::NestedIdentityMismatch);
        }
        validate_nonzero_time("attestation.built_at_ms", self.built_at_ms)
    }

    /// 使用完整提案复核构建身份、源码摘要和真实扫描能力边界。
    ///
    /// Create 的真实扫描能力必须精确等于预批准 Profile；Update 的真实扫描能力必须是
    /// Parent 已扫描能力的子集。该检查只接受构建扫描结果，不使用 Candidate 自报字段。
    ///
    /// # Errors
    ///
    /// 提案或证明无效、身份与摘要错绑，或真实能力违反 Create/Update 约束时返回
    /// [`InvalidPluginEvolution`]。
    pub fn validate_for_proposal(
        &self,
        proposal: &PluginMutationProposal,
    ) -> Result<(), InvalidPluginEvolution> {
        proposal.validate()?;
        self.validate()?;
        if self.plugin_id != proposal.plugin_id
            || self.mutation_id != proposal.mutation_id
            || self.candidate_id != proposal.candidate_id
            || self.proposal_digest != proposal.digest()?
            || self.source_digest != proposal.candidate_source.digest()?
        {
            return Err(InvalidPluginEvolution::BuildProposalBindingMismatch);
        }
        let capabilities_allowed = match &proposal.mutation {
            PluginMutationKind::Create {
                preapproved_profile,
            } => self.capabilities == preapproved_profile.capabilities(),
            PluginMutationKind::Update {
                parent_capabilities,
                ..
            } => self.capabilities.is_subset_of(parent_capabilities),
        };
        if !capabilities_allowed {
            return Err(InvalidPluginEvolution::ScannedCapabilityExpansion);
        }
        if self.built_at_ms < proposal.created_at_ms {
            return Err(InvalidPluginEvolution::InvalidTimeOrder {
                earlier: "proposal.created_at_ms",
                later: "attestation.built_at_ms",
            });
        }
        Ok(())
    }

    /// 计算构建证明的规范 SHA-256 摘要，供受信构建器签名。
    ///
    /// # Errors
    ///
    /// 构建证明不合法或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }
}

/// Host 或独立评测器产生的一项可寻址检查结果。
///
/// `passed` 必须与 `failure_count == 0` 一致，调用方不能只设置布尔值隐藏失败计数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginAuditCheck {
    /// 检查结果 schema 版本。
    pub schema_version: u32,
    /// 完整检查报告的 CAS 摘要。
    pub report_digest: ArtifactDigest,
    /// 检查器二进制与规则集的不可变摘要。
    pub verifier_revision: ArtifactDigest,
    /// 检查是否通过；必须与失败计数一致。
    pub passed: bool,
    /// 实际执行的检查项数量。
    pub check_count: u32,
    /// 失败检查项数量。
    pub failure_count: u32,
    /// 检查完成的 Unix 毫秒时间。
    pub completed_at_ms: u64,
}

impl PluginAuditCheck {
    /// 校验检查计数、结论一致性和完成时间。
    ///
    /// # Errors
    ///
    /// schema 不受支持、检查数为零或越界、失败数超过检查数、布尔结论与失败数矛盾，
    /// 或时间为零时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginAuditCheck",
            self.schema_version,
            PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
        )?;
        if self.check_count == 0
            || self.check_count > MAX_EVALUATION_CASES
            || self.failure_count > self.check_count
        {
            return Err(InvalidPluginEvolution::InvalidAuditCounts {
                checked: self.check_count,
                failed: self.failure_count,
            });
        }
        if self.passed != (self.failure_count == 0) {
            return Err(InvalidPluginEvolution::AuditResultMismatch);
        }
        validate_nonzero_time("audit_check.completed_at_ms", self.completed_at_ms)
    }

    /// 计算完整检查结果的规范 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// 检查结果无效或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }
}

/// Host 对真实插件 Bundle 执行的六类独立审计证据。
///
/// 该结构只表达受信 Host 观察到的结果，不包含 Candidate 自报结论。六类检查均为必填字段，
/// 因而无法通过省略某一审计获得 Canary 资格。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginHostAuditEvidence {
    /// Host 审计证据 schema 版本。
    pub schema_version: u32,
    /// 被审计插件 ID。
    pub plugin_id: String,
    /// 被审计 Mutation ID。
    pub mutation_id: MutationId,
    /// 被审计 Candidate ID。
    pub candidate_id: CandidateId,
    /// 被审计 Component 摘要。
    pub component_digest: ArtifactDigest,
    /// Bundle 内 manifest 的内容摘要。
    pub manifest_digest: ArtifactDigest,
    /// 受信扫描接口快照的规范摘要。
    pub interface_digest: ArtifactDigest,
    /// 受信扫描能力 Profile 的规范摘要。
    pub capability_profile_digest: ArtifactDigest,
    /// 完整待发布 Bundle 的内容摘要。
    pub bundle_digest: ArtifactDigest,
    /// 真实 WASM Host 装载与服务路由 Smoke 结果。
    pub host_smoke: PluginAuditCheck,
    /// manifest schema、版本与能力声明审计结果。
    pub manifest_audit: PluginAuditCheck,
    /// Component import 白名单审计结果。
    pub import_audit: PluginAuditCheck,
    /// Component export 与 WIT 接口兼容审计结果。
    pub interface_audit: PluginAuditCheck,
    /// owner 注入、路由与身份收窄审计结果。
    pub owner_audit: PluginAuditCheck,
    /// 运行时资源上限、生命周期与副作用审计结果。
    pub runtime_audit: PluginAuditCheck,
}

impl PluginHostAuditEvidence {
    /// 校验 Host 审计身份与六项必填检查的结构完整性。
    ///
    /// 本方法允许检查失败，以便 Gate 规范地输出 `RequireApproval`；它只拒绝缺失或自相
    /// 矛盾的证据。
    ///
    /// # Errors
    ///
    /// schema、插件 ID 或任一检查结构无效时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginHostAuditEvidence",
            self.schema_version,
            PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
        )?;
        validate_plugin_id(&self.plugin_id)?;
        self.host_smoke.validate()?;
        self.manifest_audit.validate()?;
        self.import_audit.validate()?;
        self.interface_audit.validate()?;
        self.owner_audit.validate()?;
        self.runtime_audit.validate()
    }

    /// 计算完整 Host 审计证据的规范 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// 证据无效或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }

    /// 返回全部 Host 检查中的最晚完成时间。
    fn latest_completed_at_ms(&self) -> u64 {
        [
            self.host_smoke.completed_at_ms,
            self.manifest_audit.completed_at_ms,
            self.import_audit.completed_at_ms,
            self.interface_audit.completed_at_ms,
            self.owner_audit.completed_at_ms,
            self.runtime_audit.completed_at_ms,
        ]
        .into_iter()
        .max()
        .unwrap_or_default()
    }

    /// 返回全部 Host 检查中的最早完成时间。
    fn earliest_completed_at_ms(&self) -> u64 {
        [
            self.host_smoke.completed_at_ms,
            self.manifest_audit.completed_at_ms,
            self.import_audit.completed_at_ms,
            self.interface_audit.completed_at_ms,
            self.owner_audit.completed_at_ms,
            self.runtime_audit.completed_at_ms,
        ]
        .into_iter()
        .min()
        .unwrap_or_default()
    }
}

/// 插件源码 Gate 接受的独立评测种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginEvaluationKind {
    /// 安全数据集与安全 Verifier 评测。
    Safety,
    /// Agent 任务正确性与回归数据集评测。
    Agent,
}

/// 独立 Evaluator 对真实 Bundle 产生的不可变评测证据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEvaluationEvidence {
    /// 独立评测证据 schema 版本。
    pub schema_version: u32,
    /// Safety 或 Agent 的强类型评测种类，防止两份证据互换。
    pub kind: PluginEvaluationKind,
    /// 被评测插件 ID。
    pub plugin_id: String,
    /// 被评测 Mutation ID。
    pub mutation_id: MutationId,
    /// 被评测 Candidate ID。
    pub candidate_id: CandidateId,
    /// 被评测 Component 摘要。
    pub component_digest: ArtifactDigest,
    /// 被评测完整 Bundle 摘要。
    pub bundle_digest: ArtifactDigest,
    /// 版本化数据集 Manifest 的内容摘要。
    pub dataset_digest: ArtifactDigest,
    /// 完整评测报告的 CAS 摘要。
    pub report_digest: ArtifactDigest,
    /// Evaluator 二进制与配置的不可变摘要。
    pub evaluator_revision: ArtifactDigest,
    /// 实际执行的评测用例数。
    pub case_count: u32,
    /// 失败用例数；Canary 要求为零。
    pub failure_count: u32,
    /// 评测完成的 Unix 毫秒时间。
    pub completed_at_ms: u64,
}

impl PluginEvaluationEvidence {
    /// 校验独立评测证据的身份、用例计数和完成时间。
    ///
    /// 本方法允许存在失败用例，以便 Gate 规范地输出 `RequireApproval`。
    ///
    /// # Errors
    ///
    /// schema、插件 ID、用例计数或时间不合法时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginEvaluationEvidence",
            self.schema_version,
            PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION,
        )?;
        validate_plugin_id(&self.plugin_id)?;
        if self.case_count == 0
            || self.case_count > MAX_EVALUATION_CASES
            || self.failure_count > self.case_count
        {
            return Err(InvalidPluginEvolution::InvalidEvaluationCounts {
                cases: self.case_count,
                failed: self.failure_count,
            });
        }
        validate_nonzero_time("evaluation.completed_at_ms", self.completed_at_ms)
    }

    /// 计算完整独立评测证据的规范 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// 证据无效或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }
}

/// 插件源码 Gate 唯一允许产生的决策。
///
/// 本枚举故意不包含 `AutoPromote` 或 `Stable`。Canary 成功后的 Stable 发布必须由受信
/// 评测与 Release Controller 通过另一条协议决定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSourceGateDecision {
    /// 证据存在硬失败，只允许进入人工审批或修复流程。
    RequireApproval,
    /// 全部构建、Host 与独立评测证据通过，只允许进入 Canary。
    Canary,
}

/// 插件源码 Gate 的规范硬失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginSourceGateFailure {
    /// 构建未完成两次独立可复现验证。
    NonReproducibleBuild,
    /// 真实 WASM Host Smoke 失败。
    HostSmoke,
    /// manifest 审计失败。
    ManifestAudit,
    /// import 审计失败。
    ImportAudit,
    /// interface 审计失败。
    InterfaceAudit,
    /// owner 审计失败。
    OwnerAudit,
    /// runtime 审计失败。
    RuntimeAudit,
    /// Safety Evaluation 存在失败用例。
    SafetyEvaluation,
    /// Agent Evaluation 存在失败用例。
    AgentEvaluation,
}

/// 受信插件源码 Gate 的完整输入。
///
/// 输入同时携带 Proposal、真实构建扫描、Bundle hash、六类 Host 审计及 Safety/Agent 两类
/// 独立评测，任何字段缺失都会在反序列化或结构校验阶段失败关闭。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEvaluationGateInput {
    /// Gate 输入 schema 版本。
    pub schema_version: u32,
    /// Gate 将写入报告的稳定 ID。
    pub report_id: EvaluationReportId,
    /// 完整 Create/Update 提案。
    pub proposal: PluginMutationProposal,
    /// 真实可复现构建与扫描证明。
    pub build_attestation: PluginBuildAttestation,
    /// 完整待发布 Bundle 的内容摘要。
    pub bundle_digest: ArtifactDigest,
    /// Host smoke、manifest、import、interface、owner 与 runtime 审计。
    pub host_audit: PluginHostAuditEvidence,
    /// Safety Evaluation 证据。
    pub safety_evaluation: PluginEvaluationEvidence,
    /// Agent Evaluation 证据。
    pub agent_evaluation: PluginEvaluationEvidence,
    /// 受信控制面运行 Gate 的 Unix 毫秒时间。
    pub evaluated_at_ms: u64,
}

impl PluginEvaluationGateInput {
    /// 校验全部证据的身份、摘要、种类与时间绑定。
    ///
    /// 本方法允许检查或评测失败；失败会进入 [`PluginSourceGateFailure`]，而证据错绑、
    /// 缺失或自相矛盾会直接返回错误，不能生成报告。
    ///
    /// # Errors
    ///
    /// 任一嵌套证据无效，Proposal/Component/Bundle/接口/能力绑定不一致，Safety 与 Agent
    /// 证据互换，或证据时间晚于 Gate 时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginEvaluationGateInput",
            self.schema_version,
            PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION,
        )?;
        validate_nonzero_time("gate_input.evaluated_at_ms", self.evaluated_at_ms)?;
        self.build_attestation
            .validate_for_proposal(&self.proposal)?;
        self.host_audit.validate()?;
        self.safety_evaluation.validate()?;
        self.agent_evaluation.validate()?;
        if self.safety_evaluation.kind != PluginEvaluationKind::Safety
            || self.agent_evaluation.kind != PluginEvaluationKind::Agent
        {
            return Err(InvalidPluginEvolution::EvaluationKindMismatch);
        }
        let interface_digest = self.build_attestation.interface.digest()?;
        let capability_profile_digest = self.build_attestation.capabilities.digest()?;
        let plugin_id = &self.proposal.plugin_id;
        let component_digest = &self.build_attestation.component_digest;
        if self.host_audit.plugin_id != *plugin_id
            || self.safety_evaluation.plugin_id != *plugin_id
            || self.agent_evaluation.plugin_id != *plugin_id
            || self.host_audit.mutation_id != self.proposal.mutation_id
            || self.safety_evaluation.mutation_id != self.proposal.mutation_id
            || self.agent_evaluation.mutation_id != self.proposal.mutation_id
            || self.host_audit.candidate_id != self.proposal.candidate_id
            || self.safety_evaluation.candidate_id != self.proposal.candidate_id
            || self.agent_evaluation.candidate_id != self.proposal.candidate_id
            || self.host_audit.component_digest != *component_digest
            || self.safety_evaluation.component_digest != *component_digest
            || self.agent_evaluation.component_digest != *component_digest
            || self.host_audit.bundle_digest != self.bundle_digest
            || self.safety_evaluation.bundle_digest != self.bundle_digest
            || self.agent_evaluation.bundle_digest != self.bundle_digest
            || self.host_audit.interface_digest != interface_digest
            || self.host_audit.capability_profile_digest != capability_profile_digest
        {
            return Err(InvalidPluginEvolution::GateEvidenceBindingMismatch);
        }
        let evidence_times = [
            self.proposal.created_at_ms,
            self.build_attestation.built_at_ms,
            self.host_audit.latest_completed_at_ms(),
            self.safety_evaluation.completed_at_ms,
            self.agent_evaluation.completed_at_ms,
        ];
        if evidence_times
            .into_iter()
            .any(|completed_at_ms| completed_at_ms > self.evaluated_at_ms)
        {
            return Err(InvalidPluginEvolution::InvalidTimeOrder {
                earlier: "gate evidence",
                later: "gate_input.evaluated_at_ms",
            });
        }
        if [
            self.host_audit.earliest_completed_at_ms(),
            self.safety_evaluation.completed_at_ms,
            self.agent_evaluation.completed_at_ms,
        ]
        .into_iter()
        .any(|completed_at_ms| completed_at_ms < self.build_attestation.built_at_ms)
        {
            return Err(InvalidPluginEvolution::InvalidTimeOrder {
                earlier: "attestation.built_at_ms",
                later: "gate evidence",
            });
        }
        Ok(())
    }

    /// 重新推导所有规范硬失败，不信任调用方提供的失败集合。
    ///
    /// # Errors
    ///
    /// Gate 输入结构或任一证据绑定无效时返回 [`InvalidPluginEvolution`]。
    pub fn canonical_failures(
        &self,
    ) -> Result<BTreeSet<PluginSourceGateFailure>, InvalidPluginEvolution> {
        self.validate()?;
        let mut failures = BTreeSet::new();
        if !self.build_attestation.reproducible {
            failures.insert(PluginSourceGateFailure::NonReproducibleBuild);
        }
        for (passed, failure) in [
            (
                self.host_audit.host_smoke.passed,
                PluginSourceGateFailure::HostSmoke,
            ),
            (
                self.host_audit.manifest_audit.passed,
                PluginSourceGateFailure::ManifestAudit,
            ),
            (
                self.host_audit.import_audit.passed,
                PluginSourceGateFailure::ImportAudit,
            ),
            (
                self.host_audit.interface_audit.passed,
                PluginSourceGateFailure::InterfaceAudit,
            ),
            (
                self.host_audit.owner_audit.passed,
                PluginSourceGateFailure::OwnerAudit,
            ),
            (
                self.host_audit.runtime_audit.passed,
                PluginSourceGateFailure::RuntimeAudit,
            ),
        ] {
            if !passed {
                failures.insert(failure);
            }
        }
        if self.safety_evaluation.failure_count != 0 {
            failures.insert(PluginSourceGateFailure::SafetyEvaluation);
        }
        if self.agent_evaluation.failure_count != 0 {
            failures.insert(PluginSourceGateFailure::AgentEvaluation);
        }
        Ok(failures)
    }

    /// 计算完整 Gate 输入的规范 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// Gate 输入无效或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }
}

/// 插件源码 Gate 产生的完整、可复核评测报告。
///
/// 报告保存全部关键对象摘要并可通过 [`Self::validate_for_input`] 重新推导失败集合与决策，
/// 因而调用方不能删减失败项或把 `RequireApproval` 改写为 `Canary`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginEvaluationReport {
    /// 插件源码评测报告 schema 版本。
    pub schema_version: u32,
    /// 报告稳定 ID。
    pub report_id: EvaluationReportId,
    /// 被评测插件 ID。
    pub plugin_id: String,
    /// 被评测 Mutation ID。
    pub mutation_id: MutationId,
    /// 被评测 Candidate ID。
    pub candidate_id: CandidateId,
    /// 完整 Gate 输入摘要。
    pub gate_input_digest: ArtifactDigest,
    /// 完整 Proposal 摘要。
    pub proposal_digest: ArtifactDigest,
    /// 构建证明摘要。
    pub build_attestation_digest: ArtifactDigest,
    /// 真实 Component 摘要。
    pub component_digest: ArtifactDigest,
    /// 完整待发布 Bundle 摘要。
    pub bundle_digest: ArtifactDigest,
    /// 六类 Host 审计证据摘要。
    pub host_audit_digest: ArtifactDigest,
    /// Safety Evaluation 证据摘要。
    pub safety_evaluation_digest: ArtifactDigest,
    /// Agent Evaluation 证据摘要。
    pub agent_evaluation_digest: ArtifactDigest,
    /// 只能是人工审批或 Canary。
    pub decision: PluginSourceGateDecision,
    /// 从完整输入重新推导的规范失败集合。
    pub failures: BTreeSet<PluginSourceGateFailure>,
    /// 报告生成的 Unix 毫秒时间。
    pub generated_at_ms: u64,
}

impl PluginEvaluationReport {
    /// 使用完整 Gate 输入复核报告的全部摘要、失败集合与决策。
    ///
    /// # Errors
    ///
    /// 输入无效、报告错绑、失败集合被删改、决策不是规范推导结果，或时间不一致时返回
    /// [`InvalidPluginEvolution`]。
    pub fn validate_for_input(
        &self,
        input: &PluginEvaluationGateInput,
    ) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginEvaluationReport",
            self.schema_version,
            PLUGIN_EVALUATION_REPORT_SCHEMA_VERSION,
        )?;
        input.validate()?;
        validate_plugin_id(&self.plugin_id)?;
        validate_nonzero_time("plugin_report.generated_at_ms", self.generated_at_ms)?;
        let expected_failures = input.canonical_failures()?;
        let expected_decision = if expected_failures.is_empty() {
            PluginSourceGateDecision::Canary
        } else {
            PluginSourceGateDecision::RequireApproval
        };
        if self.report_id != input.report_id
            || self.plugin_id != input.proposal.plugin_id
            || self.mutation_id != input.proposal.mutation_id
            || self.candidate_id != input.proposal.candidate_id
            || self.gate_input_digest != input.digest()?
            || self.proposal_digest != input.proposal.digest()?
            || self.build_attestation_digest != input.build_attestation.digest()?
            || self.component_digest != input.build_attestation.component_digest
            || self.bundle_digest != input.bundle_digest
            || self.host_audit_digest != input.host_audit.digest()?
            || self.safety_evaluation_digest != input.safety_evaluation.digest()?
            || self.agent_evaluation_digest != input.agent_evaluation.digest()?
        {
            return Err(InvalidPluginEvolution::PluginReportBindingMismatch);
        }
        if self.failures != expected_failures {
            return Err(InvalidPluginEvolution::PluginGateFailureMismatch);
        }
        if self.decision != expected_decision {
            return Err(InvalidPluginEvolution::PluginGateDecisionMismatch);
        }
        if self.generated_at_ms != input.evaluated_at_ms {
            return Err(InvalidPluginEvolution::InvalidTimeOrder {
                earlier: "gate_input.evaluated_at_ms",
                later: "plugin_report.generated_at_ms",
            });
        }
        Ok(())
    }

    /// 计算已与输入复核的报告规范 SHA-256 摘要。
    ///
    /// # Errors
    ///
    /// 报告与输入不一致或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest_for_input(
        &self,
        input: &PluginEvaluationGateInput,
    ) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate_for_input(input)?;
        canonical_digest(self)
    }
}

/// 签名用途域。
///
/// 用途参与签名消息，防止同一签名在构建证明、能力审批与发布之间重放。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignaturePurpose {
    /// 受信构建器签署 PluginBuildAttestation。
    BuildAttestation,
    /// 人工或策略审批方签署 PluginApprovalRecord。
    CapabilityApproval,
    /// Release Controller 签署 PluginReleaseEnvelope。
    PluginRelease,
}

/// 当前协议支持的签名算法。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureAlgorithm {
    /// Ed25519，签名字节序列化为 128 位小写十六进制。
    Ed25519,
}

/// 一个带用途域、身份和过期时间的签名信封。
///
/// [`Self::validate`] 只检查格式和绑定字段，不执行密码学验证。验证方必须从受信 Keyring
/// 解析 `key_id`，对 [`Self::signing_bytes`] 执行 Ed25519 验签，并检查密钥用途和吊销状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureEnvelope {
    /// 签名信封 schema 版本。
    pub schema_version: u32,
    /// 签名用途域。
    pub purpose: SignaturePurpose,
    /// 签名算法。
    pub algorithm: SignatureAlgorithm,
    /// 签名密钥的受信 Keyring ID，不包含密钥正文。
    pub key_id: String,
    /// 签名绑定的插件 ID。
    pub plugin_id: String,
    /// 签名绑定的 Mutation ID。
    pub mutation_id: MutationId,
    /// 被签署对象的规范 SHA-256 摘要。
    pub subject_digest: ArtifactDigest,
    /// 128 位小写十六进制 Ed25519 签名。
    pub signature_hex: String,
    /// 签署时间，Unix 毫秒。
    pub signed_at_ms: u64,
    /// 签名失效时间，Unix 毫秒；必须晚于签署时间。
    pub expires_at_ms: u64,
}

impl SignatureEnvelope {
    /// 校验 schema、身份、签名编码和有效期边界。
    ///
    /// # Errors
    ///
    /// 格式或时间不合法时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "SignatureEnvelope",
            self.schema_version,
            SIGNATURE_ENVELOPE_SCHEMA_VERSION,
        )?;
        validate_stable_id("signature.key_id", &self.key_id)?;
        validate_plugin_id(&self.plugin_id)?;
        if self.signature_hex.len() != ED25519_SIGNATURE_HEX_BYTES
            || !self
                .signature_hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(InvalidPluginEvolution::InvalidSignatureEncoding);
        }
        validate_nonzero_time("signature.signed_at_ms", self.signed_at_ms)?;
        if self.expires_at_ms <= self.signed_at_ms {
            return Err(InvalidPluginEvolution::InvalidTimeOrder {
                earlier: "signature.signed_at_ms",
                later: "signature.expires_at_ms",
            });
        }
        Ok(())
    }

    /// 校验签名用途、插件、Mutation 和主题摘要的防重放绑定。
    ///
    /// # Errors
    ///
    /// 基础格式无效或任一预期绑定不匹配时返回 [`InvalidPluginEvolution`]。
    pub fn validate_binding(
        &self,
        purpose: SignaturePurpose,
        plugin_id: &str,
        mutation_id: &MutationId,
        subject_digest: &ArtifactDigest,
    ) -> Result<(), InvalidPluginEvolution> {
        self.validate()?;
        if self.purpose != purpose
            || self.plugin_id != plugin_id
            || &self.mutation_id != mutation_id
            || &self.subject_digest != subject_digest
        {
            return Err(InvalidPluginEvolution::SignatureBindingMismatch);
        }
        Ok(())
    }

    /// 返回密码学验签所使用的域分离规范消息。
    ///
    /// 消息不包含 `signature_hex` 本身，但包含 schema、用途、算法、密钥 ID、插件、
    /// Mutation、主题摘要和有效期，防止跨用途或跨身份重放。
    ///
    /// # Errors
    ///
    /// 信封格式无效或消息无法序列化时返回 [`InvalidPluginEvolution`]。
    pub fn signing_bytes(&self) -> Result<Vec<u8>, InvalidPluginEvolution> {
        self.validate()?;
        canonical_json(&SignatureMessage {
            domain: "ascnet.lucia.plugin-signature.v1",
            schema_version: self.schema_version,
            purpose: self.purpose,
            algorithm: self.algorithm,
            key_id: &self.key_id,
            plugin_id: &self.plugin_id,
            mutation_id: &self.mutation_id,
            subject_digest: &self.subject_digest,
            signed_at_ms: self.signed_at_ms,
            expires_at_ms: self.expires_at_ms,
        })
    }

    /// 判断签名元数据在给定 Unix 毫秒时间是否处于有效期内。
    pub fn is_valid_at(&self, at_ms: u64) -> bool {
        self.signed_at_ms <= at_ms && at_ms < self.expires_at_ms
    }
}

#[derive(Serialize)]
struct SignatureMessage<'a> {
    domain: &'static str,
    schema_version: u32,
    purpose: SignaturePurpose,
    algorithm: SignatureAlgorithm,
    key_id: &'a str,
    plugin_id: &'a str,
    mutation_id: &'a MutationId,
    subject_digest: &'a ArtifactDigest,
    signed_at_ms: u64,
    expires_at_ms: u64,
}

/// 一个需要显式审批的插件能力扩张请求。
///
/// `added_requested` 和 `added_provided` 必须精确等于 Candidate Profile 相对 Parent
/// Profile 的集合差；调用方不能隐藏部分新增能力，也不能把未新增能力塞入审批请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityExpansionRequest {
    /// 能力扩张请求 schema 版本。
    pub schema_version: u32,
    /// 审批系统中的稳定请求 ID。
    pub request_id: String,
    /// 目标插件 ID。
    pub plugin_id: String,
    /// 目标 Mutation ID。
    pub mutation_id: MutationId,
    /// 目标 Candidate ID。
    pub candidate_id: CandidateId,
    /// 目标 Component 摘要。
    pub component_digest: ArtifactDigest,
    /// Parent 的受信能力 Profile。
    pub parent: CapabilityProfile,
    /// Candidate 构建产物的受信能力 Profile。
    pub candidate: CapabilityProfile,
    /// Candidate 新增的 Host 请求能力，必须为精确集合差。
    pub added_requested: PluginCapabilitySet,
    /// Candidate 新增的对外提供能力，必须为精确集合差。
    pub added_provided: PluginCapabilitySet,
    /// 有界、可审计的扩张理由。
    pub rationale: String,
    /// 请求创建的 Unix 毫秒时间。
    pub requested_at_ms: u64,
}

impl CapabilityExpansionRequest {
    /// 校验身份、能力集合差、理由和时间。
    ///
    /// # Errors
    ///
    /// Candidate 没有能力扩张、集合差错绑或字段越界时返回
    /// [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "CapabilityExpansionRequest",
            self.schema_version,
            CAPABILITY_EXPANSION_REQUEST_SCHEMA_VERSION,
        )?;
        validate_stable_id("expansion.request_id", &self.request_id)?;
        validate_plugin_id(&self.plugin_id)?;
        self.parent.validate()?;
        self.candidate.validate()?;
        self.added_requested.validate()?;
        self.added_provided.validate()?;
        let (expected_requested, expected_provided) =
            self.candidate.additions_since(&self.parent)?;
        if expected_requested.capabilities.is_empty() && expected_provided.capabilities.is_empty() {
            return Err(InvalidPluginEvolution::NotCapabilityExpansion);
        }
        if self.added_requested != expected_requested || self.added_provided != expected_provided {
            return Err(InvalidPluginEvolution::CapabilityExpansionMismatch);
        }
        validate_text("expansion.rationale", &self.rationale, MAX_RATIONALE_BYTES)?;
        validate_nonzero_time("expansion.requested_at_ms", self.requested_at_ms)
    }

    /// 计算扩张请求的规范 SHA-256 摘要，供审批记录绑定。
    ///
    /// # Errors
    ///
    /// 请求不合法或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate()?;
        canonical_digest(self)
    }
}

/// 能力扩张审批结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginApprovalDecision {
    /// 批准该精确扩张请求。
    Approved,
    /// 拒绝该精确扩张请求。
    Rejected,
}

/// 一条签名的插件能力审批记录。
///
/// 签名主题是除 `signature` 外完整记录的规范摘要，其中包含 request、插件、Mutation、
/// Candidate 和 Component 绑定。任何跨提案、跨构建产物或跨审批请求重放都会导致摘要不符。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginApprovalRecord {
    /// 审批记录 schema 版本。
    pub schema_version: u32,
    /// 审批记录稳定 ID。
    pub approval_id: String,
    /// 被审批 CapabilityExpansionRequest 的规范摘要。
    pub request_digest: ArtifactDigest,
    /// 被审批插件 ID。
    pub plugin_id: String,
    /// 被审批 Mutation ID。
    pub mutation_id: MutationId,
    /// 被审批 Candidate ID。
    pub candidate_id: CandidateId,
    /// 被审批 Component 摘要。
    pub component_digest: ArtifactDigest,
    /// 审批结论。
    pub decision: PluginApprovalDecision,
    /// 受信审批主体 ID。
    pub approver_id: String,
    /// 不可变审批策略版本。
    pub policy_version: String,
    /// 审批完成时间，Unix 毫秒。
    pub decided_at_ms: u64,
    /// 审批失效时间，Unix 毫秒。
    pub expires_at_ms: u64,
    /// 审批方对完整记录摘要的签名。
    pub signature: SignatureEnvelope,
}

impl PluginApprovalRecord {
    /// 校验审批字段及签名的用途域和防重放绑定。
    ///
    /// 本方法不执行密码学验签；调用方仍需使用受信审批 Keyring 验证签名字节。
    ///
    /// # Errors
    ///
    /// 字段、时间或签名绑定不合法时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        self.validate_unsigned()?;
        let digest = self.signing_digest()?;
        self.signature.validate_binding(
            SignaturePurpose::CapabilityApproval,
            &self.plugin_id,
            &self.mutation_id,
            &digest,
        )?;
        if self.signature.signed_at_ms != self.decided_at_ms
            || self.signature.expires_at_ms != self.expires_at_ms
        {
            return Err(InvalidPluginEvolution::SignatureBindingMismatch);
        }
        Ok(())
    }

    /// 计算审批方必须签署的规范 SHA-256 摘要。
    ///
    /// 该摘要不包含签名自身，可在签名前生成。
    ///
    /// # Errors
    ///
    /// 未签名字段不合法或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn signing_digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate_unsigned()?;
        canonical_digest(&ApprovalSigningPayload::from(self))
    }

    /// 判断该记录是否在指定时间对精确扩张请求有效。
    pub fn is_effective_at(&self, at_ms: u64) -> bool {
        self.decision == PluginApprovalDecision::Approved
            && self.decided_at_ms <= at_ms
            && at_ms < self.expires_at_ms
            && self.signature.is_valid_at(at_ms)
    }

    fn validate_unsigned(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginApprovalRecord",
            self.schema_version,
            PLUGIN_APPROVAL_RECORD_SCHEMA_VERSION,
        )?;
        validate_stable_id("approval.approval_id", &self.approval_id)?;
        validate_plugin_id(&self.plugin_id)?;
        validate_stable_id("approval.approver_id", &self.approver_id)?;
        validate_text(
            "approval.policy_version",
            &self.policy_version,
            MAX_POLICY_VERSION_BYTES,
        )?;
        validate_nonzero_time("approval.decided_at_ms", self.decided_at_ms)?;
        if self.expires_at_ms <= self.decided_at_ms {
            return Err(InvalidPluginEvolution::InvalidTimeOrder {
                earlier: "approval.decided_at_ms",
                later: "approval.expires_at_ms",
            });
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ApprovalSigningPayload<'a> {
    domain: &'static str,
    schema_version: u32,
    approval_id: &'a str,
    request_digest: &'a ArtifactDigest,
    plugin_id: &'a str,
    mutation_id: &'a MutationId,
    candidate_id: &'a CandidateId,
    component_digest: &'a ArtifactDigest,
    decision: PluginApprovalDecision,
    approver_id: &'a str,
    policy_version: &'a str,
    decided_at_ms: u64,
    expires_at_ms: u64,
}

impl<'a> From<&'a PluginApprovalRecord> for ApprovalSigningPayload<'a> {
    fn from(record: &'a PluginApprovalRecord) -> Self {
        Self {
            domain: "ascnet.lucia.plugin-approval.v1",
            schema_version: record.schema_version,
            approval_id: &record.approval_id,
            request_digest: &record.request_digest,
            plugin_id: &record.plugin_id,
            mutation_id: &record.mutation_id,
            candidate_id: &record.candidate_id,
            component_digest: &record.component_digest,
            decision: record.decision,
            approver_id: &record.approver_id,
            policy_version: &record.policy_version,
            decided_at_ms: record.decided_at_ms,
            expires_at_ms: record.expires_at_ms,
        }
    }
}

/// 插件发布阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginReleaseStage {
    /// 受控小流量 Canary 发布。
    Canary,
    /// 切换为 Stable 的正式发布。
    Stable,
    /// 回滚到先前受信 Component。
    Rollback,
}

/// 一个由 Release Controller 签名的完整插件发布信封。
///
/// 信封把 Proposal、源码、真实 Component、Bundle、源码 Gate 报告、构建证明、扫描能力、
/// 接口、能力扩张审批和发布阶段绑定为一个不可重放对象。Canary/Stable 发布执行器必须调用
/// [`Self::validate_for_evaluation`] 并验证两层签名的密码学真实性，再执行安装或流量切换。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginReleaseEnvelope {
    /// 发布信封 schema 版本。
    pub schema_version: u32,
    /// 发布 ID。
    pub release_id: ReleaseId,
    /// 发布阶段。
    pub stage: PluginReleaseStage,
    /// 发布目标插件 ID。
    pub plugin_id: String,
    /// 发布目标 Mutation ID。
    pub mutation_id: MutationId,
    /// 发布目标 Candidate ID。
    pub candidate_id: CandidateId,
    /// 完整 Mutation Proposal 的规范摘要。
    pub proposal_digest: ArtifactDigest,
    /// 实际构建源码树的规范摘要。
    pub source_digest: ArtifactDigest,
    /// 完整待发布 Bundle 的内容摘要，必须与插件源码 Gate 报告一致。
    pub bundle_digest: ArtifactDigest,
    /// 受信插件源码 Gate 报告的规范摘要。
    pub evaluation_report_digest: ArtifactDigest,
    /// 受信构建证明。
    pub attestation: PluginBuildAttestation,
    /// 受信构建器对 Attestation 摘要的签名。
    pub attestation_signature: SignatureEnvelope,
    /// 发布前 Stable 插件的能力 Profile。
    pub baseline_capabilities: CapabilityProfile,
    /// 有能力扩张时的精确扩张请求。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expansion_request: Option<CapabilityExpansionRequest>,
    /// 有能力扩张时的有效签名审批。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<PluginApprovalRecord>,
    /// Stable 阶段所承接的成功 Canary Release；其他阶段必须为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub canary_of: Option<ReleaseId>,
    /// Rollback 阶段被撤销的 Release；其他阶段必须为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_of: Option<ReleaseId>,
    /// Rollback 要恢复的受信 Component 摘要；其他阶段必须为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_target_component_digest: Option<ArtifactDigest>,
    /// 发布信封签发时间，Unix 毫秒。
    pub issued_at_ms: u64,
    /// Release Controller 对除本字段外完整信封摘要的签名。
    pub signature: SignatureEnvelope,
}

impl PluginReleaseEnvelope {
    /// 校验发布信封自身的身份、两层签名绑定、能力扩张审批和阶段一致性。
    ///
    /// 本方法不读取源码 Gate 报告，不能单独授权 Canary/Stable。发布执行器必须继续调用
    /// [`Self::validate_for_evaluation`]，并验证 Keyring、吊销状态、CAS 内容摘要和当前
    /// Stable 前置条件。
    ///
    /// # Errors
    ///
    /// 任一发布不变量不成立时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        self.validate_unsigned()?;
        let payload_digest = self.signing_digest()?;
        self.signature.validate_binding(
            SignaturePurpose::PluginRelease,
            &self.plugin_id,
            &self.mutation_id,
            &payload_digest,
        )?;
        if !self.signature.is_valid_at(self.issued_at_ms) {
            return Err(InvalidPluginEvolution::SignatureExpired);
        }
        Ok(())
    }

    /// 使用完整 Gate 输入与报告复核 Canary/Stable 发布的评测前置条件。
    ///
    /// 本方法确保 Release Controller 签名的 `bundle_digest` 与
    /// `evaluation_report_digest` 精确绑定已复核的 Host/Safety/Agent Gate 报告。报告只能
    /// 授权进入 Canary；Stable 控制面还必须另行验证 Canary 成功证明，本方法不替代该证明。
    ///
    /// # Errors
    ///
    /// 发布信封、Gate 输入或报告无效，摘要、身份、Component、Bundle 或时间错绑，或 Gate
    /// 决策不是 Canary 时返回 [`InvalidPluginEvolution`]。
    pub fn validate_for_evaluation(
        &self,
        report: &PluginEvaluationReport,
        input: &PluginEvaluationGateInput,
    ) -> Result<(), InvalidPluginEvolution> {
        self.validate()?;
        report.validate_for_input(input)?;
        if report.decision != PluginSourceGateDecision::Canary
            || self.evaluation_report_digest != report.digest_for_input(input)?
            || self.bundle_digest != report.bundle_digest
            || self.plugin_id != report.plugin_id
            || self.mutation_id != report.mutation_id
            || self.candidate_id != report.candidate_id
            || self.proposal_digest != report.proposal_digest
            || self.attestation.component_digest != report.component_digest
        {
            return Err(InvalidPluginEvolution::ReleaseEvaluationBindingMismatch);
        }
        if report.generated_at_ms > self.issued_at_ms {
            return Err(InvalidPluginEvolution::InvalidTimeOrder {
                earlier: "plugin_report.generated_at_ms",
                later: "release.issued_at_ms",
            });
        }
        Ok(())
    }

    /// 计算 Release Controller 必须签署的规范 SHA-256 摘要。
    ///
    /// 摘要排除最外层发布签名，但包含 Bundle、源码 Gate 报告、构建签名、扩张请求和审批
    /// 记录。
    ///
    /// # Errors
    ///
    /// 未签名字段不合法或无法规范序列化时返回 [`InvalidPluginEvolution`]。
    pub fn signing_digest(&self) -> Result<ArtifactDigest, InvalidPluginEvolution> {
        self.validate_unsigned()?;
        canonical_digest(&ReleaseSigningPayload::from(self))
    }

    fn validate_unsigned(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginReleaseEnvelope",
            self.schema_version,
            PLUGIN_RELEASE_ENVELOPE_SCHEMA_VERSION,
        )?;
        validate_plugin_id(&self.plugin_id)?;
        validate_nonzero_time("release.issued_at_ms", self.issued_at_ms)?;
        self.attestation.validate()?;
        self.baseline_capabilities.validate()?;
        if self.attestation.plugin_id != self.plugin_id
            || self.attestation.mutation_id != self.mutation_id
            || self.attestation.candidate_id != self.candidate_id
            || self.attestation.proposal_digest != self.proposal_digest
            || self.attestation.source_digest != self.source_digest
        {
            return Err(InvalidPluginEvolution::NestedIdentityMismatch);
        }
        if !self.attestation.reproducible {
            return Err(InvalidPluginEvolution::NonReproducibleBuild);
        }
        let attestation_digest = self.attestation.digest()?;
        self.attestation_signature.validate_binding(
            SignaturePurpose::BuildAttestation,
            &self.plugin_id,
            &self.mutation_id,
            &attestation_digest,
        )?;
        if self.attestation_signature.signed_at_ms < self.attestation.built_at_ms {
            return Err(InvalidPluginEvolution::InvalidTimeOrder {
                earlier: "attestation.built_at_ms",
                later: "attestation_signature.signed_at_ms",
            });
        }
        if !self.attestation_signature.is_valid_at(self.issued_at_ms) {
            return Err(InvalidPluginEvolution::SignatureExpired);
        }

        let candidate_capabilities = &self.attestation.capabilities;
        let expands = !candidate_capabilities.is_subset_of(&self.baseline_capabilities);
        match (
            expands,
            self.expansion_request.as_ref(),
            self.approval.as_ref(),
        ) {
            (false, None, None) => {}
            (true, Some(request), Some(approval)) => {
                request.validate()?;
                approval.validate()?;
                if request.plugin_id != self.plugin_id
                    || request.mutation_id != self.mutation_id
                    || request.candidate_id != self.candidate_id
                    || request.component_digest != self.attestation.component_digest
                    || request.parent != self.baseline_capabilities
                    || request.candidate != *candidate_capabilities
                    || approval.plugin_id != self.plugin_id
                    || approval.mutation_id != self.mutation_id
                    || approval.candidate_id != self.candidate_id
                    || approval.component_digest != self.attestation.component_digest
                    || approval.request_digest != request.digest()?
                {
                    return Err(InvalidPluginEvolution::ApprovalBindingMismatch);
                }
                if request.requested_at_ms > approval.decided_at_ms {
                    return Err(InvalidPluginEvolution::InvalidTimeOrder {
                        earlier: "expansion.requested_at_ms",
                        later: "approval.decided_at_ms",
                    });
                }
                if !approval.is_effective_at(self.issued_at_ms) {
                    return Err(InvalidPluginEvolution::ApprovalNotEffective);
                }
            }
            _ => return Err(InvalidPluginEvolution::CapabilityApprovalMismatch),
        }

        match (
            self.stage,
            self.canary_of.as_ref(),
            self.rollback_of.as_ref(),
            self.rollback_target_component_digest.as_ref(),
        ) {
            (PluginReleaseStage::Canary, None, None, None) => {}
            (PluginReleaseStage::Stable, Some(canary), None, None)
                if canary != &self.release_id => {}
            (PluginReleaseStage::Rollback, None, Some(previous), Some(target))
                if previous != &self.release_id && target != &self.attestation.component_digest => {
            }
            _ => return Err(InvalidPluginEvolution::ReleaseStageMismatch),
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ReleaseSigningPayload<'a> {
    domain: &'static str,
    schema_version: u32,
    release_id: &'a ReleaseId,
    stage: PluginReleaseStage,
    plugin_id: &'a str,
    mutation_id: &'a MutationId,
    candidate_id: &'a CandidateId,
    proposal_digest: &'a ArtifactDigest,
    source_digest: &'a ArtifactDigest,
    bundle_digest: &'a ArtifactDigest,
    evaluation_report_digest: &'a ArtifactDigest,
    attestation: &'a PluginBuildAttestation,
    attestation_signature: &'a SignatureEnvelope,
    baseline_capabilities: &'a CapabilityProfile,
    expansion_request: Option<&'a CapabilityExpansionRequest>,
    approval: Option<&'a PluginApprovalRecord>,
    canary_of: Option<&'a ReleaseId>,
    rollback_of: Option<&'a ReleaseId>,
    rollback_target_component_digest: Option<&'a ArtifactDigest>,
    issued_at_ms: u64,
}

impl<'a> From<&'a PluginReleaseEnvelope> for ReleaseSigningPayload<'a> {
    fn from(release: &'a PluginReleaseEnvelope) -> Self {
        Self {
            domain: "ascnet.lucia.plugin-release.v1",
            schema_version: release.schema_version,
            release_id: &release.release_id,
            stage: release.stage,
            plugin_id: &release.plugin_id,
            mutation_id: &release.mutation_id,
            candidate_id: &release.candidate_id,
            proposal_digest: &release.proposal_digest,
            source_digest: &release.source_digest,
            bundle_digest: &release.bundle_digest,
            evaluation_report_digest: &release.evaluation_report_digest,
            attestation: &release.attestation,
            attestation_signature: &release.attestation_signature,
            baseline_capabilities: &release.baseline_capabilities,
            expansion_request: release.expansion_request.as_ref(),
            approval: release.approval.as_ref(),
            canary_of: release.canary_of.as_ref(),
            rollback_of: release.rollback_of.as_ref(),
            rollback_target_component_digest: release.rollback_target_component_digest.as_ref(),
            issued_at_ms: release.issued_at_ms,
        }
    }
}

/// Canary 发布的持久化阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCanaryState {
    /// 已登记但尚未切流量。
    Planned,
    /// 正在接收受控流量。
    Running,
    /// 健康门槛全部通过。
    Succeeded,
    /// 健康门槛失败，尚未完成回滚。
    Failed,
    /// 健康门槛失败且已回滚。
    RolledBack,
}

/// 一次插件 Canary 的不可变状态快照。
///
/// 持久化层应只追加新快照并保证状态迁移单调；本类型校验单个快照中阶段、时间、计数、
/// 健康制品与回滚 Release 的一致性。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginCanaryRecord {
    /// Canary 记录 schema 版本。
    pub schema_version: u32,
    /// Canary 稳定 ID。
    pub canary_id: String,
    /// 被观察的 Canary Release ID。
    pub release_id: ReleaseId,
    /// 被观察发布信封的规范摘要。
    pub release_digest: ArtifactDigest,
    /// 被观察插件 ID。
    pub plugin_id: String,
    /// 被观察 Mutation ID。
    pub mutation_id: MutationId,
    /// 被观察 Candidate ID。
    pub candidate_id: CandidateId,
    /// 被观察 Component 摘要。
    pub component_digest: ArtifactDigest,
    /// 当前 Canary 阶段。
    pub state: PluginCanaryState,
    /// Canary 实际开始时间，Unix 毫秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<u64>,
    /// Canary 终止时间，Unix 毫秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    /// 已观察的受信运行总数。
    pub observed_runs: u64,
    /// 通过健康验证的运行数。
    pub passed_runs: u64,
    /// 未通过健康验证的运行数。
    pub failed_runs: u64,
    /// 终态健康报告摘要；非终态必须为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_report_digest: Option<ArtifactDigest>,
    /// 完成回滚时生成的新 Release ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback_release_id: Option<ReleaseId>,
}

impl PluginCanaryRecord {
    /// 校验 Canary 阶段、时间、计数、健康报告和回滚引用的一致性。
    ///
    /// # Errors
    ///
    /// 任一状态不变量不成立时返回 [`InvalidPluginEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidPluginEvolution> {
        validate_schema(
            "PluginCanaryRecord",
            self.schema_version,
            PLUGIN_CANARY_RECORD_SCHEMA_VERSION,
        )?;
        validate_stable_id("canary.canary_id", &self.canary_id)?;
        validate_plugin_id(&self.plugin_id)?;
        let classified = self.passed_runs.checked_add(self.failed_runs).ok_or(
            InvalidPluginEvolution::InvalidCanaryCounts {
                observed: self.observed_runs,
                passed: self.passed_runs,
                failed: self.failed_runs,
            },
        )?;
        if classified != self.observed_runs {
            return Err(InvalidPluginEvolution::InvalidCanaryCounts {
                observed: self.observed_runs,
                passed: self.passed_runs,
                failed: self.failed_runs,
            });
        }
        if let (Some(started), Some(finished)) = (self.started_at_ms, self.finished_at_ms) {
            if finished < started {
                return Err(InvalidPluginEvolution::InvalidTimeOrder {
                    earlier: "canary.started_at_ms",
                    later: "canary.finished_at_ms",
                });
            }
        }

        let consistent = match self.state {
            PluginCanaryState::Planned => {
                self.started_at_ms.is_none()
                    && self.finished_at_ms.is_none()
                    && self.observed_runs == 0
                    && self.health_report_digest.is_none()
                    && self.rollback_release_id.is_none()
            }
            PluginCanaryState::Running => {
                self.started_at_ms.is_some()
                    && self.finished_at_ms.is_none()
                    && self.health_report_digest.is_none()
                    && self.rollback_release_id.is_none()
            }
            PluginCanaryState::Succeeded => {
                self.started_at_ms.is_some()
                    && self.finished_at_ms.is_some()
                    && self.observed_runs > 0
                    && self.passed_runs == self.observed_runs
                    && self.failed_runs == 0
                    && self.health_report_digest.is_some()
                    && self.rollback_release_id.is_none()
            }
            PluginCanaryState::Failed => {
                self.started_at_ms.is_some()
                    && self.finished_at_ms.is_some()
                    && self.observed_runs > 0
                    && self.failed_runs > 0
                    && self.health_report_digest.is_some()
                    && self.rollback_release_id.is_none()
            }
            PluginCanaryState::RolledBack => {
                self.started_at_ms.is_some()
                    && self.finished_at_ms.is_some()
                    && self.observed_runs > 0
                    && self.failed_runs > 0
                    && self.health_report_digest.is_some()
                    && self
                        .rollback_release_id
                        .as_ref()
                        .is_some_and(|rollback| rollback != &self.release_id)
            }
        };
        if !consistent {
            return Err(InvalidPluginEvolution::CanaryStateMismatch { state: self.state });
        }
        Ok(())
    }

    /// 校验 Canary 记录与完整 Canary Release、源码 Gate 报告的防重放绑定。
    ///
    /// # Errors
    ///
    /// Release 或源码 Gate 无效、不是 Canary 阶段，或任一身份和摘要不匹配时返回
    /// [`InvalidPluginEvolution`]。
    pub fn validate_against_release(
        &self,
        release: &PluginReleaseEnvelope,
        report: &PluginEvaluationReport,
        input: &PluginEvaluationGateInput,
    ) -> Result<(), InvalidPluginEvolution> {
        self.validate()?;
        release.validate_for_evaluation(report, input)?;
        if release.stage != PluginReleaseStage::Canary
            || self.release_id != release.release_id
            || self.release_digest != release.signing_digest()?
            || self.plugin_id != release.plugin_id
            || self.mutation_id != release.mutation_id
            || self.candidate_id != release.candidate_id
            || self.component_digest != release.attestation.component_digest
        {
            return Err(InvalidPluginEvolution::CanaryReleaseBindingMismatch);
        }
        Ok(())
    }
}

/// 插件进化协议结构校验错误。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvalidPluginEvolution {
    /// schema 版本不受支持。
    #[error("不支持的 {kind} schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSchema {
        /// 协议对象名称。
        kind: &'static str,
        /// 实际版本。
        found: u32,
        /// 当前支持版本。
        supported: u32,
    },
    /// 插件 ID 不是规范稳定标识。
    #[error("插件 ID 必须是有界的小写 ASCII 稳定标识")]
    InvalidPluginId,
    /// 文本为空或超过协议上限。
    #[error("字段 `{field}` 必须是非空且不超过 {max_bytes} 字节的文本")]
    InvalidText {
        /// 字段名。
        field: &'static str,
        /// 最大字节数。
        max_bytes: usize,
    },
    /// 稳定协议 token 含非法字符。
    #[error("字段 `{field}` 不是规范 ASCII 协议标识")]
    InvalidProtocolToken {
        /// 字段名。
        field: &'static str,
    },
    /// 集合数量不在边界内。
    #[error("字段 `{field}` 的项目数 {found} 不在 {min} 到 {max} 范围内")]
    ItemCountOutOfRange {
        /// 字段名。
        field: &'static str,
        /// 实际项目数。
        found: usize,
        /// 最少项目数。
        min: usize,
        /// 最多项目数。
        max: usize,
    },
    /// 列表没有按规范键严格升序排列。
    #[error("字段 `{field}` 必须严格升序且不得重复")]
    UnorderedOrDuplicate {
        /// 字段名。
        field: &'static str,
    },
    /// 构造输入中存在重复值。
    #[error("字段 `{field}` 不能包含重复值")]
    DuplicateValue {
        /// 字段名。
        field: &'static str,
    },
    /// 源码路径不安全或不是规范相对 POSIX 路径。
    #[error("插件源码路径不合法：{reason}")]
    InvalidSourcePath {
        /// 不回显原始路径的稳定原因。
        reason: &'static str,
    },
    /// 单个源码文件超过上限。
    #[error("源码文件 `{path}` 大小 {size_bytes} 超过上限 {max_bytes}")]
    FileTooLarge {
        /// 已通过有界路径校验的文件路径。
        path: String,
        /// 实际字节数。
        size_bytes: u64,
        /// 最大字节数。
        max_bytes: u64,
    },
    /// 源码树总体大小超过上限。
    #[error("源码树大小 {size_bytes} 超过上限 {max_bytes}")]
    SourceTooLarge {
        /// 实际字节数。
        size_bytes: u64,
        /// 最大字节数。
        max_bytes: u64,
    },
    /// 同一摘要被声明为不同字节长度。
    #[error("同一内容摘要 {digest} 不能绑定不同字节长度")]
    DigestSizeMismatch {
        /// 冲突摘要。
        digest: ArtifactDigest,
    },
    /// Update 没有改变内容摘要。
    #[error("Update 补丁没有改变文件 `{path}` 的内容摘要")]
    UnchangedUpdate {
        /// 补丁路径。
        path: String,
    },
    /// Parent 和 Candidate Genome 相同。
    #[error("插件变异的 Parent 与 Candidate Genome 摘要不能相同")]
    UnchangedGenome,
    /// Parent 和 Candidate 源码树相同。
    #[error("插件变异的 Parent 与 Candidate 源码树不能相同")]
    UnchangedSource,
    /// Create 的自报能力不等于预批准低权限 Profile。
    #[error("Create 提案的自报能力必须精确等于预批准低权限 Profile")]
    PreapprovedProfileMismatch,
    /// Update 的自报能力超出 Parent 已受信扫描范围。
    #[error("Update 提案的自报 Candidate 能力不能超出 Parent 能力")]
    ClaimedCapabilityExpansion,
    /// 真实构建扫描能力超出 Create/Update 的允许范围。
    #[error("真实构建扫描的 Candidate 能力超出预批准 Profile 或 Parent 能力")]
    ScannedCapabilityExpansion,
    /// 构建证明没有精确绑定完整提案和 Candidate 源码。
    #[error("插件构建证明与完整 Proposal 或 Candidate 源码错绑")]
    BuildProposalBindingMismatch,
    /// 补丁与 Parent/Candidate 文件摘要错绑。
    #[error("插件补丁与源码树的旧摘要或新摘要不匹配：`{path}`")]
    PatchDigestMismatch {
        /// 补丁路径。
        path: String,
    },
    /// 补丁没有精确覆盖源码差异。
    #[error("插件补丁集合没有精确覆盖 Parent/Candidate 源码差异：`{path}`")]
    PatchSetMismatch {
        /// 首个不一致路径。
        path: String,
    },
    /// 能力名不是规范稳定标识。
    #[error("插件能力 ID 必须是有界的小写 ASCII 稳定标识")]
    InvalidCapability,
    /// 嵌套对象身份或 Component 摘要不一致。
    #[error("插件进化嵌套对象的插件、Mutation、Candidate 或 Component 绑定不一致")]
    NestedIdentityMismatch,
    /// Component 大小无效。
    #[error("Component 大小 {size_bytes} 必须在 1 到 {max_bytes} 字节之间")]
    InvalidComponentSize {
        /// 实际大小。
        size_bytes: u64,
        /// 最大大小。
        max_bytes: u64,
    },
    /// 审计检查计数无效。
    #[error("插件审计计数无效：checked={checked}，failed={failed}")]
    InvalidAuditCounts {
        /// 实际检查项数。
        checked: u32,
        /// 实际失败项数。
        failed: u32,
    },
    /// 审计布尔结论与失败计数矛盾。
    #[error("插件审计 passed 必须与 failure_count == 0 一致")]
    AuditResultMismatch,
    /// 独立评测用例计数无效。
    #[error("插件评测计数无效：cases={cases}，failed={failed}")]
    InvalidEvaluationCounts {
        /// 实际执行用例数。
        cases: u32,
        /// 实际失败用例数。
        failed: u32,
    },
    /// Safety 与 Agent 评测证据种类被错绑或互换。
    #[error("插件 Gate 必须分别绑定 Safety 与 Agent Evaluation 证据")]
    EvaluationKindMismatch,
    /// Gate 证据与插件、Component、Bundle、接口或能力摘要错绑。
    #[error("插件 Gate 证据与插件、Component、Bundle、接口或能力摘要错绑")]
    GateEvidenceBindingMismatch,
    /// 评测报告摘要或身份没有绑定完整 Gate 输入。
    #[error("PluginEvaluationReport 与 Gate 输入身份或摘要错绑")]
    PluginReportBindingMismatch,
    /// 评测报告失败集合不是从输入重新推导的完整集合。
    #[error("PluginEvaluationReport 的失败集合与完整 Gate 输入不一致")]
    PluginGateFailureMismatch,
    /// 评测报告决策不是规范推导结果。
    #[error("PluginEvaluationReport 的决策与规范失败集合不一致")]
    PluginGateDecisionMismatch,
    /// Unix 毫秒时间为零。
    #[error("字段 `{field}` 必须是非零 Unix 毫秒时间")]
    InvalidTimestamp {
        /// 字段名。
        field: &'static str,
    },
    /// 时间先后顺序不合法。
    #[error("时间字段 `{later}` 必须晚于或等于其协议要求的 `{earlier}`")]
    InvalidTimeOrder {
        /// 较早字段。
        earlier: &'static str,
        /// 较晚字段。
        later: &'static str,
    },
    /// 签名编码不合法。
    #[error("Ed25519 签名必须是 128 位小写十六进制")]
    InvalidSignatureEncoding,
    /// 签名用途或身份绑定不匹配。
    #[error("签名的用途、插件、Mutation 或主题摘要绑定不匹配")]
    SignatureBindingMismatch,
    /// 签名在发布或审批时已失效。
    #[error("签名在目标操作时间不处于有效期内")]
    SignatureExpired,
    /// Candidate 没有新增能力。
    #[error("CapabilityExpansionRequest 必须包含至少一项真实新增能力")]
    NotCapabilityExpansion,
    /// 扩张请求没有精确声明新增能力集合。
    #[error("CapabilityExpansionRequest 的新增能力不是 Parent/Candidate 的精确集合差")]
    CapabilityExpansionMismatch,
    /// 能力变化与扩张请求或审批存在性不一致。
    #[error("能力扩张必须同时携带精确请求和有效审批，非扩张不得携带二者")]
    CapabilityApprovalMismatch,
    /// 审批记录与扩张请求或构建产物错绑。
    #[error("审批记录与扩张请求、Candidate 或 Component 绑定不匹配")]
    ApprovalBindingMismatch,
    /// 审批未批准或在发布时已失效。
    #[error("能力扩张审批在发布时间未生效")]
    ApprovalNotEffective,
    /// 构建没有完成可复现性验证。
    #[error("插件发布只接受已完成独立重复构建的证明")]
    NonReproducibleBuild,
    /// 发布阶段与 Canary lineage 或 Rollback 目标不一致。
    #[error("插件发布阶段与 canary_of、rollback_of 或 rollback_target_component_digest 不一致")]
    ReleaseStageMismatch,
    /// 发布信封没有绑定完整通过的插件源码 Gate 报告与 Bundle。
    #[error("插件发布信封与 Canary Gate 报告、Bundle 或评测身份错绑")]
    ReleaseEvaluationBindingMismatch,
    /// Canary 分类计数不一致。
    #[error("Canary 计数不一致：observed={observed}，passed={passed}，failed={failed}")]
    InvalidCanaryCounts {
        /// 总观察数。
        observed: u64,
        /// 通过数。
        passed: u64,
        /// 失败数。
        failed: u64,
    },
    /// Canary 状态与时间、计数或制品不一致。
    #[error("Canary 状态 {state:?} 与时间、计数、健康报告或回滚引用不一致")]
    CanaryStateMismatch {
        /// 实际阶段。
        state: PluginCanaryState,
    },
    /// Canary 记录与目标 Release 错绑。
    #[error("Canary 记录与目标 Canary Release 的身份或摘要不匹配")]
    CanaryReleaseBindingMismatch,
    /// 规范 JSON 序列化失败。
    #[error("插件进化协议无法规范序列化：{0}")]
    Serialization(String),
    /// 规范 SHA-256 无法转换为强类型摘要。
    #[error("插件进化协议无法构造摘要：{0}")]
    DigestConstruction(String),
}

fn validate_schema(
    kind: &'static str,
    found: u32,
    supported: u32,
) -> Result<(), InvalidPluginEvolution> {
    if found != supported {
        return Err(InvalidPluginEvolution::UnsupportedSchema {
            kind,
            found,
            supported,
        });
    }
    Ok(())
}

fn validate_plugin_id(value: &str) -> Result<(), InvalidPluginEvolution> {
    if value.is_empty()
        || value.len() > MAX_PLUGIN_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(InvalidPluginEvolution::InvalidPluginId);
    }
    Ok(())
}

fn validate_stable_id(field: &'static str, value: &str) -> Result<(), InvalidPluginEvolution> {
    validate_text(field, value, MAX_STABLE_ID_BYTES)?;
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
    }) {
        return Err(InvalidPluginEvolution::InvalidProtocolToken { field });
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), InvalidPluginEvolution> {
    if value.trim().is_empty() || value.len() > max_bytes {
        return Err(InvalidPluginEvolution::InvalidText { field, max_bytes });
    }
    Ok(())
}

fn validate_protocol_token(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), InvalidPluginEvolution> {
    validate_text(field, value, max_bytes)?;
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'.' | b'_' | b'-' | b':' | b'/' | b'@' | b'#' | b'[' | b']'
            )
    }) {
        return Err(InvalidPluginEvolution::InvalidProtocolToken { field });
    }
    Ok(())
}

fn validate_source_path(path: &str) -> Result<(), InvalidPluginEvolution> {
    if path.is_empty() || path.len() > MAX_SOURCE_PATH_BYTES {
        return Err(InvalidPluginEvolution::InvalidSourcePath {
            reason: "路径为空或超过字节上限",
        });
    }
    if path.starts_with('/') || path.starts_with('\\') {
        return Err(InvalidPluginEvolution::InvalidSourcePath {
            reason: "不允许绝对路径",
        });
    }
    if path.contains('\\') || path.contains('\0') {
        return Err(InvalidPluginEvolution::InvalidSourcePath {
            reason: "只允许 POSIX 分隔符且不得包含 NUL",
        });
    }
    if path.len() >= 2 && path.as_bytes()[0].is_ascii_alphabetic() && path.as_bytes()[1] == b':' {
        return Err(InvalidPluginEvolution::InvalidSourcePath {
            reason: "不允许 Windows 驱动器路径",
        });
    }
    if path.split('/').any(|part| {
        part.is_empty()
            || matches!(part, "." | "..")
            || part.bytes().any(|byte| byte.is_ascii_control())
    }) {
        return Err(InvalidPluginEvolution::InvalidSourcePath {
            reason: "路径段不得为空、为点号或包含控制字符",
        });
    }
    Ok(())
}

fn validate_capability(value: &str) -> Result<(), InvalidPluginEvolution> {
    if value.is_empty()
        || value.len() > MAX_CAPABILITY_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
        })
        || !value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
    {
        return Err(InvalidPluginEvolution::InvalidCapability);
    }
    Ok(())
}

fn validate_count(
    field: &'static str,
    found: usize,
    min: usize,
    max: usize,
) -> Result<(), InvalidPluginEvolution> {
    if found < min || found > max {
        return Err(InvalidPluginEvolution::ItemCountOutOfRange {
            field,
            found,
            min,
            max,
        });
    }
    Ok(())
}

fn validate_nonzero_time(field: &'static str, value: u64) -> Result<(), InvalidPluginEvolution> {
    if value == 0 {
        return Err(InvalidPluginEvolution::InvalidTimestamp { field });
    }
    Ok(())
}

fn validate_strictly_sorted_paths(
    files: &[PluginSourceFile],
) -> Result<(), InvalidPluginEvolution> {
    if files.windows(2).any(|pair| pair[0].path >= pair[1].path) {
        return Err(InvalidPluginEvolution::UnorderedOrDuplicate {
            field: "source.files",
        });
    }
    Ok(())
}

fn validate_strictly_sorted_strings(
    field: &'static str,
    values: &[String],
) -> Result<(), InvalidPluginEvolution> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(InvalidPluginEvolution::UnorderedOrDuplicate { field });
    }
    Ok(())
}

fn validate_strictly_sorted_ord<T: Ord>(
    field: &'static str,
    values: &[T],
) -> Result<(), InvalidPluginEvolution> {
    if values.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(InvalidPluginEvolution::UnorderedOrDuplicate { field });
    }
    Ok(())
}

fn validate_interface_items(
    field: &'static str,
    values: &[String],
) -> Result<(), InvalidPluginEvolution> {
    validate_count(field, values.len(), 0, MAX_INTERFACE_ITEMS)?;
    for value in values {
        validate_protocol_token(field, value, MAX_INTERFACE_ITEM_BYTES)?;
    }
    validate_strictly_sorted_strings(field, values)
}

/// 校验 Create 使用全量 Create 补丁精确覆盖 Candidate 源码树。
fn validate_create_patch_set(
    candidate: &PluginSourceArtifact,
    patches: &[PluginFilePatch],
) -> Result<(), InvalidPluginEvolution> {
    validate_count("proposal.patches", patches.len(), 1, MAX_PATCHES)?;
    for patch in patches {
        patch.validate()?;
    }
    if patches
        .windows(2)
        .any(|pair| pair[0].path() >= pair[1].path())
    {
        return Err(InvalidPluginEvolution::UnorderedOrDuplicate {
            field: "proposal.patches",
        });
    }
    if patches.len() != candidate.files.len() {
        let path = patches
            .get(candidate.files.len())
            .map(PluginFilePatch::path)
            .or_else(|| {
                candidate
                    .files
                    .get(patches.len())
                    .map(|file| file.path.as_str())
            })
            .unwrap_or("<create-patch-count>");
        return Err(InvalidPluginEvolution::PatchSetMismatch {
            path: path.to_string(),
        });
    }
    for (file, patch) in candidate.files.iter().zip(patches) {
        match patch {
            PluginFilePatch::Create { path, new_digest }
                if path == &file.path && new_digest == &file.digest => {}
            _ if patch.path() == file.path => {
                return Err(InvalidPluginEvolution::PatchDigestMismatch {
                    path: file.path.clone(),
                });
            }
            _ => {
                return Err(InvalidPluginEvolution::PatchSetMismatch {
                    path: file.path.clone(),
                });
            }
        }
    }
    Ok(())
}

fn validate_patch_set(
    parent: &PluginSourceArtifact,
    candidate: &PluginSourceArtifact,
    patches: &[PluginFilePatch],
) -> Result<(), InvalidPluginEvolution> {
    validate_count("proposal.patches", patches.len(), 1, MAX_PATCHES)?;
    for patch in patches {
        patch.validate()?;
    }
    if patches
        .windows(2)
        .any(|pair| pair[0].path() >= pair[1].path())
    {
        return Err(InvalidPluginEvolution::UnorderedOrDuplicate {
            field: "proposal.patches",
        });
    }

    let patches_by_path = patches
        .iter()
        .map(|patch| (patch.path(), patch))
        .collect::<BTreeMap<_, _>>();
    let paths = parent
        .files
        .iter()
        .map(|file| file.path.as_str())
        .chain(candidate.files.iter().map(|file| file.path.as_str()))
        .collect::<BTreeSet<_>>();
    if let Some(extra) = patches.iter().find(|patch| !paths.contains(patch.path())) {
        return Err(InvalidPluginEvolution::PatchSetMismatch {
            path: extra.path().to_string(),
        });
    }

    for path in paths {
        let before = parent.file(path);
        let after = candidate.file(path);
        let patch = patches_by_path.get(path).copied();
        match (before, after, patch) {
            (None, Some(file), Some(PluginFilePatch::Create { new_digest, .. }))
                if new_digest == &file.digest => {}
            (Some(file), None, Some(PluginFilePatch::Delete { old_digest, .. }))
                if old_digest == &file.digest => {}
            (
                Some(before),
                Some(after),
                Some(PluginFilePatch::Update {
                    old_digest,
                    new_digest,
                    ..
                }),
            ) if before.digest != after.digest
                && old_digest == &before.digest
                && new_digest == &after.digest => {}
            (Some(before), Some(after), None)
                if before.digest == after.digest && before.size_bytes == after.size_bytes => {}
            (None, Some(_), Some(_)) | (Some(_), None, Some(_)) | (Some(_), Some(_), Some(_)) => {
                return Err(InvalidPluginEvolution::PatchDigestMismatch {
                    path: path.to_string(),
                });
            }
            _ => {
                return Err(InvalidPluginEvolution::PatchSetMismatch {
                    path: path.to_string(),
                });
            }
        }
    }

    if patches_by_path.len() != patches.len() {
        return Err(InvalidPluginEvolution::UnorderedOrDuplicate {
            field: "proposal.patches",
        });
    }
    Ok(())
}

fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, InvalidPluginEvolution> {
    serde_json::to_vec(value)
        .map_err(|error| InvalidPluginEvolution::Serialization(error.to_string()))
}

fn canonical_digest<T: Serialize>(value: &T) -> Result<ArtifactDigest, InvalidPluginEvolution> {
    let bytes = canonical_json(value)?;
    let hex = format!("{:x}", Sha256::digest(bytes));
    ArtifactDigest::from_sha256_hex(hex)
        .map_err(|error| InvalidPluginEvolution::DigestConstruction(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(seed: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
    }

    fn source_file(path: &str, seed: char) -> PluginSourceFile {
        PluginSourceFile {
            path: path.to_string(),
            digest: digest(seed),
            size_bytes: 16,
        }
    }

    fn capabilities(values: &[&str]) -> PluginCapabilitySet {
        PluginCapabilitySet::new(values.iter().map(|value| (*value).to_string()).collect())
            .expect("测试能力应合法")
    }

    fn profile(requested: &[&str], provided: &[&str]) -> CapabilityProfile {
        CapabilityProfile::new(capabilities(requested), capabilities(provided))
            .expect("测试 Profile 应合法")
    }

    fn signature(
        purpose: SignaturePurpose,
        plugin_id: &str,
        mutation_id: &MutationId,
        subject_digest: ArtifactDigest,
        signed_at_ms: u64,
        expires_at_ms: u64,
    ) -> SignatureEnvelope {
        SignatureEnvelope {
            schema_version: SIGNATURE_ENVELOPE_SCHEMA_VERSION,
            purpose,
            algorithm: SignatureAlgorithm::Ed25519,
            key_id: "trusted-key-1".into(),
            plugin_id: plugin_id.into(),
            mutation_id: mutation_id.clone(),
            subject_digest,
            signature_hex: "a".repeat(ED25519_SIGNATURE_HEX_BYTES),
            signed_at_ms,
            expires_at_ms,
        }
    }

    fn genome_digest(seed: char) -> GenomeDigest {
        GenomeDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("测试摘要应合法")
    }

    fn interface(component_digest: ArtifactDigest) -> ComponentInterfaceSnapshot {
        ComponentInterfaceSnapshot {
            schema_version: COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
            plugin_id: "example.plugin".into(),
            component_digest,
            world: "example:plugin/world@1.0.0".into(),
            imports: Vec::new(),
            exports: vec!["example:plugin/run".into()],
            scanner_revision: digest('f'),
        }
    }

    /// 接口 token 必须接受 Wasmtime 对资源方法使用的规范 `[method]` 路径。
    #[test]
    fn component_interface_accepts_real_wasmtime_method_paths() {
        let mut snapshot = interface(digest('b'));
        snapshot.imports = vec!["wasi:io/poll@0.2.6#[method]pollable.block".into()];
        snapshot
            .validate()
            .expect("真实 Wasmtime Component 方法路径应满足协议");
    }

    fn create_proposal() -> PluginMutationProposal {
        let candidate_source =
            PluginSourceArtifact::new("example.plugin", vec![source_file("src/lib.rs", 'a')])
                .expect("Create Candidate 源码应合法");
        PluginMutationProposal {
            schema_version: PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION,
            cycle_id: EvolutionCycleId::generate(),
            mutation_id: MutationId::generate(),
            candidate_id: CandidateId::generate(),
            plugin_id: "example.plugin".into(),
            parent_genome_digest: genome_digest('1'),
            candidate_genome_digest: genome_digest('2'),
            mutation: PluginMutationKind::Create {
                preapproved_profile: PreapprovedPluginProfile::PureCompute,
            },
            candidate_source,
            patches: vec![PluginFilePatch::Create {
                path: "src/lib.rs".into(),
                new_digest: digest('a'),
            }],
            claimed_capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
            claimed_interface: interface(digest('b')),
            evidence_episode_ids: vec![EpisodeId::generate()],
            rationale: "基于可信失败证据创建受限纯计算插件".into(),
            created_at_ms: 10,
        }
    }

    fn update_proposal() -> PluginMutationProposal {
        let parent_source =
            PluginSourceArtifact::new("example.plugin", vec![source_file("src/lib.rs", 'a')])
                .expect("Update Parent 源码应合法");
        let candidate_source =
            PluginSourceArtifact::new("example.plugin", vec![source_file("src/lib.rs", 'b')])
                .expect("Update Candidate 源码应合法");
        PluginMutationProposal {
            schema_version: PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION,
            cycle_id: EvolutionCycleId::generate(),
            mutation_id: MutationId::generate(),
            candidate_id: CandidateId::generate(),
            plugin_id: "example.plugin".into(),
            parent_genome_digest: genome_digest('1'),
            candidate_genome_digest: genome_digest('2'),
            mutation: PluginMutationKind::Update {
                parent_source: Box::new(parent_source),
                parent_capabilities: Box::new(profile(&["filesystem_read"], &[])),
            },
            candidate_source,
            patches: vec![PluginFilePatch::Update {
                path: "src/lib.rs".into(),
                old_digest: digest('a'),
                new_digest: digest('b'),
            }],
            claimed_capabilities: profile(&["filesystem_read"], &[]),
            claimed_interface: interface(digest('b')),
            evidence_episode_ids: vec![EpisodeId::generate()],
            rationale: "基于可信失败证据更新现有插件".into(),
            created_at_ms: 10,
        }
    }

    fn attestation(
        proposal: &PluginMutationProposal,
        capabilities: CapabilityProfile,
    ) -> PluginBuildAttestation {
        let component_digest = digest('c');
        PluginBuildAttestation {
            schema_version: PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION,
            build_id: "build-m8-0001".into(),
            plugin_id: proposal.plugin_id.clone(),
            mutation_id: proposal.mutation_id.clone(),
            candidate_id: proposal.candidate_id.clone(),
            proposal_digest: proposal.digest().expect("测试提案摘要应可计算"),
            source_digest: proposal
                .candidate_source
                .digest()
                .expect("测试源码摘要应可计算"),
            component_digest: component_digest.clone(),
            component_size_bytes: 128,
            interface: interface(component_digest),
            capabilities,
            build_environment_digest: digest('d'),
            builder_revision: digest('e'),
            build_log_digest: digest('f'),
            reproducible: true,
            built_at_ms: 20,
        }
    }

    fn audit_check(seed: char, passed: bool, completed_at_ms: u64) -> PluginAuditCheck {
        PluginAuditCheck {
            schema_version: PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
            report_digest: digest(seed),
            verifier_revision: digest('f'),
            passed,
            check_count: 3,
            failure_count: u32::from(!passed),
            completed_at_ms,
        }
    }

    fn gate_input() -> PluginEvaluationGateInput {
        let proposal = create_proposal();
        let build_attestation = attestation(
            &proposal,
            PreapprovedPluginProfile::PureCompute.capabilities(),
        );
        let component_digest = build_attestation.component_digest.clone();
        let bundle_digest = digest('e');
        let host_audit = PluginHostAuditEvidence {
            schema_version: PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
            plugin_id: proposal.plugin_id.clone(),
            mutation_id: proposal.mutation_id.clone(),
            candidate_id: proposal.candidate_id.clone(),
            component_digest: component_digest.clone(),
            manifest_digest: digest('d'),
            interface_digest: build_attestation
                .interface
                .digest()
                .expect("测试接口摘要应可计算"),
            capability_profile_digest: build_attestation
                .capabilities
                .digest()
                .expect("测试能力摘要应可计算"),
            bundle_digest: bundle_digest.clone(),
            host_smoke: audit_check('1', true, 30),
            manifest_audit: audit_check('2', true, 31),
            import_audit: audit_check('3', true, 32),
            interface_audit: audit_check('4', true, 33),
            owner_audit: audit_check('5', true, 34),
            runtime_audit: audit_check('6', true, 35),
        };
        let evaluation = |kind, report_seed, completed_at_ms| PluginEvaluationEvidence {
            schema_version: PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION,
            kind,
            plugin_id: proposal.plugin_id.clone(),
            mutation_id: proposal.mutation_id.clone(),
            candidate_id: proposal.candidate_id.clone(),
            component_digest: component_digest.clone(),
            bundle_digest: bundle_digest.clone(),
            dataset_digest: digest('a'),
            report_digest: digest(report_seed),
            evaluator_revision: digest('f'),
            case_count: 20,
            failure_count: 0,
            completed_at_ms,
        };
        let safety_evaluation = evaluation(PluginEvaluationKind::Safety, 'b', 40);
        let agent_evaluation = evaluation(PluginEvaluationKind::Agent, 'c', 41);
        PluginEvaluationGateInput {
            schema_version: PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION,
            report_id: EvaluationReportId::generate(),
            proposal,
            build_attestation,
            bundle_digest,
            host_audit,
            safety_evaluation,
            agent_evaluation,
            evaluated_at_ms: 50,
        }
    }

    fn report_for(input: &PluginEvaluationGateInput) -> PluginEvaluationReport {
        let failures = input
            .canonical_failures()
            .expect("测试 Gate 输入应可推导失败集合");
        let decision = if failures.is_empty() {
            PluginSourceGateDecision::Canary
        } else {
            PluginSourceGateDecision::RequireApproval
        };
        PluginEvaluationReport {
            schema_version: PLUGIN_EVALUATION_REPORT_SCHEMA_VERSION,
            report_id: input.report_id.clone(),
            plugin_id: input.proposal.plugin_id.clone(),
            mutation_id: input.proposal.mutation_id.clone(),
            candidate_id: input.proposal.candidate_id.clone(),
            gate_input_digest: input.digest().expect("测试 Gate 输入摘要应可计算"),
            proposal_digest: input.proposal.digest().expect("测试提案摘要应可计算"),
            build_attestation_digest: input
                .build_attestation
                .digest()
                .expect("测试构建证明摘要应可计算"),
            component_digest: input.build_attestation.component_digest.clone(),
            bundle_digest: input.bundle_digest.clone(),
            host_audit_digest: input
                .host_audit
                .digest()
                .expect("测试 Host 审计摘要应可计算"),
            safety_evaluation_digest: input
                .safety_evaluation
                .digest()
                .expect("测试 Safety 摘要应可计算"),
            agent_evaluation_digest: input
                .agent_evaluation
                .digest()
                .expect("测试 Agent 摘要应可计算"),
            decision,
            failures,
            generated_at_ms: input.evaluated_at_ms,
        }
    }

    fn release_for(
        input: &PluginEvaluationGateInput,
        report: &PluginEvaluationReport,
    ) -> PluginReleaseEnvelope {
        let attestation_digest = input
            .build_attestation
            .digest()
            .expect("测试构建证明摘要应可计算");
        let mut release = PluginReleaseEnvelope {
            schema_version: PLUGIN_RELEASE_ENVELOPE_SCHEMA_VERSION,
            release_id: ReleaseId::generate(),
            stage: PluginReleaseStage::Canary,
            plugin_id: input.proposal.plugin_id.clone(),
            mutation_id: input.proposal.mutation_id.clone(),
            candidate_id: input.proposal.candidate_id.clone(),
            proposal_digest: input.proposal.digest().expect("测试提案摘要应可计算"),
            source_digest: input
                .proposal
                .candidate_source
                .digest()
                .expect("测试源码摘要应可计算"),
            bundle_digest: input.bundle_digest.clone(),
            evaluation_report_digest: report
                .digest_for_input(input)
                .expect("测试报告摘要应可计算"),
            attestation: input.build_attestation.clone(),
            attestation_signature: signature(
                SignaturePurpose::BuildAttestation,
                &input.proposal.plugin_id,
                &input.proposal.mutation_id,
                attestation_digest,
                21,
                100,
            ),
            baseline_capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
            expansion_request: None,
            approval: None,
            canary_of: None,
            rollback_of: None,
            rollback_target_component_digest: None,
            issued_at_ms: 70,
            signature: signature(
                SignaturePurpose::PluginRelease,
                &input.proposal.plugin_id,
                &input.proposal.mutation_id,
                digest('0'),
                60,
                100,
            ),
        };
        release.signature.subject_digest =
            release.signing_digest().expect("测试发布签名摘要应可计算");
        release
    }

    #[test]
    fn source_paths_reject_escape_and_absolute_forms() {
        for path in [
            "../src/lib.rs",
            "src/../lib.rs",
            "/tmp/plugin.rs",
            "C:/temp/plugin.rs",
            "C:temp/plugin.rs",
            "src\\lib.rs",
            "src//lib.rs",
        ] {
            let file = source_file(path, 'a');
            assert!(file.validate().is_err(), "应拒绝路径 `{path}`");
        }
        assert!(source_file("src/lib.rs", 'a').validate().is_ok());
    }

    #[test]
    fn source_artifact_sorts_input_and_rejects_duplicate_paths() {
        let source = PluginSourceArtifact::new(
            "example.plugin",
            vec![source_file("src/z.rs", 'b'), source_file("src/a.rs", 'a')],
        )
        .expect("构造器应规范排序");
        assert_eq!(source.files[0].path, "src/a.rs");
        assert_eq!(source.files[1].path, "src/z.rs");

        let duplicate = PluginSourceArtifact::new(
            "example.plugin",
            vec![
                source_file("src/lib.rs", 'a'),
                source_file("src/lib.rs", 'b'),
            ],
        );
        assert!(matches!(
            duplicate,
            Err(InvalidPluginEvolution::UnorderedOrDuplicate {
                field: "source.files"
            })
        ));
    }

    #[test]
    fn patch_set_binds_old_and_new_digests_and_exact_diff() {
        let parent =
            PluginSourceArtifact::new("example.plugin", vec![source_file("src/lib.rs", 'a')])
                .expect("Parent 源码应合法");
        let candidate =
            PluginSourceArtifact::new("example.plugin", vec![source_file("src/lib.rs", 'b')])
                .expect("Candidate 源码应合法");
        let valid = vec![PluginFilePatch::Update {
            path: "src/lib.rs".into(),
            old_digest: digest('a'),
            new_digest: digest('b'),
        }];
        validate_patch_set(&parent, &candidate, &valid).expect("正确摘要绑定应通过");

        let wrong_old = vec![PluginFilePatch::Update {
            path: "src/lib.rs".into(),
            old_digest: digest('c'),
            new_digest: digest('b'),
        }];
        assert!(matches!(
            validate_patch_set(&parent, &candidate, &wrong_old),
            Err(InvalidPluginEvolution::PatchDigestMismatch { .. })
        ));

        let missing = Vec::new();
        assert!(validate_patch_set(&parent, &candidate, &missing).is_err());

        let extra = vec![
            valid[0].clone(),
            PluginFilePatch::Create {
                path: "src/zghost.rs".into(),
                new_digest: digest('d'),
            },
        ];
        assert!(matches!(
            validate_patch_set(&parent, &candidate, &extra),
            Err(InvalidPluginEvolution::PatchSetMismatch { .. })
        ));
    }

    #[test]
    fn patch_kinds_enforce_create_update_delete_existence() {
        let parent = PluginSourceArtifact::new(
            "example.plugin",
            vec![source_file("delete.rs", 'a'), source_file("update.rs", 'b')],
        )
        .expect("Parent 源码应合法");
        let candidate = PluginSourceArtifact::new(
            "example.plugin",
            vec![source_file("create.rs", 'c'), source_file("update.rs", 'd')],
        )
        .expect("Candidate 源码应合法");
        let patches = vec![
            PluginFilePatch::Create {
                path: "create.rs".into(),
                new_digest: digest('c'),
            },
            PluginFilePatch::Delete {
                path: "delete.rs".into(),
                old_digest: digest('a'),
            },
            PluginFilePatch::Update {
                path: "update.rs".into(),
                old_digest: digest('b'),
                new_digest: digest('d'),
            },
        ];
        validate_patch_set(&parent, &candidate, &patches).expect("三类补丁应精确绑定");

        let wrong_kind = vec![
            PluginFilePatch::Update {
                path: "create.rs".into(),
                old_digest: digest('a'),
                new_digest: digest('c'),
            },
            patches[1].clone(),
            patches[2].clone(),
        ];
        assert!(validate_patch_set(&parent, &candidate, &wrong_kind).is_err());
    }

    #[test]
    fn create_has_no_parent_and_requires_full_create_patch_set() {
        let proposal = create_proposal();
        proposal
            .validate()
            .expect("Create 不需要虚构空 Parent 且全量 Create 补丁应通过");
        let encoded = serde_json::to_value(&proposal).expect("Create 提案应可序列化");
        let mutation = encoded.get("mutation").expect("应包含显式 mutation");
        assert_eq!(
            mutation.get("kind").and_then(|value| value.as_str()),
            Some("create")
        );
        assert!(mutation.get("parent_source").is_none());
        assert!(mutation.get("parent_capabilities").is_none());

        let mut missing = proposal.clone();
        missing.patches.clear();
        assert!(matches!(
            missing.validate(),
            Err(InvalidPluginEvolution::ItemCountOutOfRange {
                field: "proposal.patches",
                ..
            })
        ));
        let mut wrong_kind = proposal;
        wrong_kind.patches = vec![PluginFilePatch::Update {
            path: "src/lib.rs".into(),
            old_digest: digest('f'),
            new_digest: digest('a'),
        }];
        assert!(matches!(
            wrong_kind.validate(),
            Err(InvalidPluginEvolution::PatchDigestMismatch { .. })
        ));
    }

    #[test]
    fn create_rejects_capabilities_outside_preapproved_profile() {
        let mut proposal = create_proposal();
        proposal.claimed_capabilities = profile(&["filesystem_read"], &[]);
        assert!(matches!(
            proposal.validate(),
            Err(InvalidPluginEvolution::PreapprovedProfileMismatch)
        ));

        let proposal = create_proposal();
        let build = attestation(&proposal, profile(&["filesystem_read"], &[]));
        assert!(matches!(
            build.validate_for_proposal(&proposal),
            Err(InvalidPluginEvolution::ScannedCapabilityExpansion)
        ));
    }

    #[test]
    fn update_uses_real_build_scan_to_reject_capability_expansion() {
        let proposal = update_proposal();
        proposal.validate().expect("受限 Update 提案应合法");
        let mut expanded_claim = proposal.clone();
        expanded_claim.claimed_capabilities = profile(&["filesystem_read", "process_exec"], &[]);
        assert!(matches!(
            expanded_claim.validate(),
            Err(InvalidPluginEvolution::ClaimedCapabilityExpansion)
        ));
        let valid = attestation(&proposal, profile(&["filesystem_read"], &[]));
        valid
            .validate_for_proposal(&proposal)
            .expect("真实扫描能力为 Parent 子集时应通过");

        let expanded = attestation(
            &proposal,
            profile(&["filesystem_read", "process_exec"], &[]),
        );
        assert!(matches!(
            expanded.validate_for_proposal(&proposal),
            Err(InvalidPluginEvolution::ScannedCapabilityExpansion)
        ));
    }

    #[test]
    fn build_attestation_rejects_proposal_and_source_rebinding() {
        let proposal = create_proposal();
        let build = attestation(
            &proposal,
            PreapprovedPluginProfile::PureCompute.capabilities(),
        );
        build
            .validate_for_proposal(&proposal)
            .expect("精确绑定应通过");

        let mut rebound = build.clone();
        rebound.proposal_digest = digest('9');
        assert!(matches!(
            rebound.validate_for_proposal(&proposal),
            Err(InvalidPluginEvolution::BuildProposalBindingMismatch)
        ));
        let mut rebound = build;
        rebound.source_digest = digest('8');
        assert!(matches!(
            rebound.validate_for_proposal(&proposal),
            Err(InvalidPluginEvolution::BuildProposalBindingMismatch)
        ));
    }

    #[test]
    fn gate_input_binds_all_evidence_and_rejects_kind_or_bundle_replay() {
        let input = gate_input();
        input.validate().expect("完整精确绑定的 Gate 输入应通过");
        assert!(input
            .canonical_failures()
            .expect("应可推导失败集合")
            .is_empty());

        let mut missing = serde_json::to_value(&input).expect("Gate 输入应可序列化");
        missing
            .get_mut("host_audit")
            .and_then(serde_json::Value::as_object_mut)
            .expect("应包含 Host 审计对象")
            .remove("owner_audit");
        assert!(serde_json::from_value::<PluginEvaluationGateInput>(missing).is_err());

        let mut swapped = input.clone();
        swapped.safety_evaluation.kind = PluginEvaluationKind::Agent;
        assert!(matches!(
            swapped.validate(),
            Err(InvalidPluginEvolution::EvaluationKindMismatch)
        ));
        let mut rebound = input;
        rebound.agent_evaluation.bundle_digest = digest('9');
        assert!(matches!(
            rebound.validate(),
            Err(InvalidPluginEvolution::GateEvidenceBindingMismatch)
        ));
    }

    #[test]
    fn report_rederives_failures_and_only_allows_canary_or_approval() {
        let input = gate_input();
        let report = report_for(&input);
        report
            .validate_for_input(&input)
            .expect("全证据通过最多只能进入 Canary");
        assert_eq!(report.decision, PluginSourceGateDecision::Canary);
        assert_eq!(
            serde_json::to_string(&report.decision).expect("决策应可序列化"),
            "\"canary\""
        );

        let mut failed_input = input;
        failed_input.host_audit.owner_audit = audit_check('5', false, 34);
        failed_input.safety_evaluation.failure_count = 2;
        let failed_report = report_for(&failed_input);
        failed_report
            .validate_for_input(&failed_input)
            .expect("失败证据必须进入人工审批");
        assert_eq!(
            failed_report.decision,
            PluginSourceGateDecision::RequireApproval
        );
        assert_eq!(
            failed_report.failures,
            BTreeSet::from([
                PluginSourceGateFailure::OwnerAudit,
                PluginSourceGateFailure::SafetyEvaluation,
            ])
        );

        let mut forged = failed_report.clone();
        forged.failures.remove(&PluginSourceGateFailure::OwnerAudit);
        assert!(matches!(
            forged.validate_for_input(&failed_input),
            Err(InvalidPluginEvolution::PluginGateFailureMismatch)
        ));
        let mut forged = failed_report;
        forged.decision = PluginSourceGateDecision::Canary;
        assert!(matches!(
            forged.validate_for_input(&failed_input),
            Err(InvalidPluginEvolution::PluginGateDecisionMismatch)
        ));
    }

    #[test]
    fn release_signature_and_gate_validation_bind_report_and_bundle() {
        let input = gate_input();
        let report = report_for(&input);
        let release = release_for(&input, &report);
        release
            .validate_for_evaluation(&report, &input)
            .expect("精确绑定的 Canary 发布应通过 Gate 复核");
        let original_signing_digest = release.signing_digest().expect("测试发布签名摘要应可计算");

        let mut rebound_bundle = release.clone();
        rebound_bundle.bundle_digest = digest('9');
        let rebound_signing_digest = rebound_bundle
            .signing_digest()
            .expect("错绑 Bundle 后仍应可计算待签摘要");
        assert_ne!(original_signing_digest, rebound_signing_digest);
        rebound_bundle.signature.subject_digest = rebound_signing_digest;
        assert!(matches!(
            rebound_bundle.validate_for_evaluation(&report, &input),
            Err(InvalidPluginEvolution::ReleaseEvaluationBindingMismatch)
        ));

        let mut rebound_report = release;
        rebound_report.evaluation_report_digest = digest('8');
        rebound_report.signature.subject_digest = rebound_report
            .signing_digest()
            .expect("错绑报告后仍应可计算待签摘要");
        assert!(matches!(
            rebound_report.validate_for_evaluation(&report, &input),
            Err(InvalidPluginEvolution::ReleaseEvaluationBindingMismatch)
        ));
    }

    #[test]
    fn release_stage_binds_canary_lineage_and_rollback_target_in_signature() {
        let input = gate_input();
        let report = report_for(&input);
        let release = release_for(&input, &report);

        let mut invalid_canary = release.clone();
        invalid_canary.canary_of = Some(ReleaseId::generate());
        assert!(matches!(
            invalid_canary.signing_digest(),
            Err(InvalidPluginEvolution::ReleaseStageMismatch)
        ));

        let mut stable = release.clone();
        stable.stage = PluginReleaseStage::Stable;
        assert!(matches!(
            stable.signing_digest(),
            Err(InvalidPluginEvolution::ReleaseStageMismatch)
        ));
        stable.canary_of = Some(stable.release_id.clone());
        assert!(matches!(
            stable.signing_digest(),
            Err(InvalidPluginEvolution::ReleaseStageMismatch)
        ));
        stable.canary_of = Some(ReleaseId::generate());
        stable.signature.subject_digest = stable
            .signing_digest()
            .expect("绑定非自身 Canary 后 Stable 应可签名");
        stable
            .validate_for_evaluation(&report, &input)
            .expect("Stable 必须继续绑定完整源码 Gate 报告");
        let stable_digest = stable.signing_digest().expect("Stable 签名摘要应可计算");
        stable.canary_of = Some(ReleaseId::generate());
        assert_ne!(
            stable_digest,
            stable
                .signing_digest()
                .expect("更换 Canary lineage 后应可重新计算签名摘要")
        );

        let mut rollback = release;
        rollback.stage = PluginReleaseStage::Rollback;
        rollback.rollback_of = Some(ReleaseId::generate());
        rollback.rollback_target_component_digest =
            Some(rollback.attestation.component_digest.clone());
        assert!(matches!(
            rollback.signing_digest(),
            Err(InvalidPluginEvolution::ReleaseStageMismatch)
        ));
        rollback.rollback_target_component_digest = Some(digest('9'));
        rollback.signature.subject_digest = rollback
            .signing_digest()
            .expect("绑定不同目标 Component 后 Rollback 应可签名");
        rollback
            .validate_for_evaluation(&report, &input)
            .expect("Rollback 目标与源码 Gate 报告必须同时受签名保护");
        let rollback_digest = rollback
            .signing_digest()
            .expect("Rollback 签名摘要应可计算");
        rollback.rollback_target_component_digest = Some(digest('8'));
        assert_ne!(
            rollback_digest,
            rollback
                .signing_digest()
                .expect("更换回滚目标后应可重新计算签名摘要")
        );
    }

    #[test]
    fn capability_sets_have_stable_sorting_and_subset_semantics() {
        let child = capabilities(&["process_exec", "filesystem_read"]);
        assert_eq!(
            child.capabilities,
            vec!["filesystem_read".to_string(), "process_exec".to_string()]
        );
        let parent = capabilities(&["network", "process_exec", "filesystem_read"]);
        assert!(child.is_subset_of(&parent));
        assert!(!parent.is_subset_of(&child));

        let unordered = PluginCapabilitySet {
            schema_version: PLUGIN_CAPABILITY_SET_SCHEMA_VERSION,
            capabilities: vec!["network".into(), "filesystem_read".into()],
        };
        assert!(unordered.validate().is_err());
        assert!(PluginCapabilitySet::new(vec!["network".into(), "network".into()]).is_err());
    }

    #[test]
    fn signature_binding_rejects_cross_mutation_replay() {
        let mutation = MutationId::generate();
        let other_mutation = MutationId::generate();
        let subject = digest('a');
        let signature = signature(
            SignaturePurpose::BuildAttestation,
            "example.plugin",
            &mutation,
            subject.clone(),
            10,
            100,
        );
        signature
            .validate_binding(
                SignaturePurpose::BuildAttestation,
                "example.plugin",
                &mutation,
                &subject,
            )
            .expect("原始绑定应通过");
        assert!(signature
            .validate_binding(
                SignaturePurpose::BuildAttestation,
                "example.plugin",
                &other_mutation,
                &subject,
            )
            .is_err());
        assert!(signature
            .validate_binding(
                SignaturePurpose::PluginRelease,
                "example.plugin",
                &mutation,
                &subject,
            )
            .is_err());
    }

    #[test]
    fn approval_signature_binds_request_candidate_and_component() {
        let mutation = MutationId::generate();
        let candidate = CandidateId::generate();
        let mut approval = PluginApprovalRecord {
            schema_version: PLUGIN_APPROVAL_RECORD_SCHEMA_VERSION,
            approval_id: "approval-0001".into(),
            request_digest: digest('a'),
            plugin_id: "example.plugin".into(),
            mutation_id: mutation.clone(),
            candidate_id: candidate,
            component_digest: digest('b'),
            decision: PluginApprovalDecision::Approved,
            approver_id: "security-team".into(),
            policy_version: "plugin-approval-v1".into(),
            decided_at_ms: 10,
            expires_at_ms: 100,
            signature: signature(
                SignaturePurpose::CapabilityApproval,
                "example.plugin",
                &mutation,
                digest('f'),
                10,
                100,
            ),
        };
        let signing_digest = approval.signing_digest().expect("审批摘要应可计算");
        approval.signature.subject_digest = signing_digest;
        approval.validate().expect("精确绑定审批应通过");

        let mut replayed = approval.clone();
        replayed.request_digest = digest('c');
        assert!(replayed.validate().is_err());

        let mut replayed = approval.clone();
        replayed.component_digest = digest('d');
        assert!(replayed.validate().is_err());
    }

    #[test]
    fn expansion_request_requires_exact_nonempty_difference() {
        let parent = profile(&["filesystem_read"], &["agent.context-loader"]);
        let candidate = profile(
            &["filesystem_read", "process_exec"],
            &["agent.context-loader", "agent.tool-policy"],
        );
        let mut request = CapabilityExpansionRequest {
            schema_version: CAPABILITY_EXPANSION_REQUEST_SCHEMA_VERSION,
            request_id: "expansion-0001".into(),
            plugin_id: "example.plugin".into(),
            mutation_id: MutationId::generate(),
            candidate_id: CandidateId::generate(),
            component_digest: digest('a'),
            parent,
            candidate,
            added_requested: capabilities(&["process_exec"]),
            added_provided: capabilities(&["agent.tool-policy"]),
            rationale: "需要受控进程执行能力完成本地构建".into(),
            requested_at_ms: 10,
        };
        request.validate().expect("精确能力差应通过");
        request.added_requested = capabilities(&["network"]);
        assert!(matches!(
            request.validate(),
            Err(InvalidPluginEvolution::CapabilityExpansionMismatch)
        ));
    }

    #[test]
    fn canary_state_requires_consistent_times_counts_and_artifacts() {
        let mut record = PluginCanaryRecord {
            schema_version: PLUGIN_CANARY_RECORD_SCHEMA_VERSION,
            canary_id: "canary-0001".into(),
            release_id: ReleaseId::generate(),
            release_digest: digest('a'),
            plugin_id: "example.plugin".into(),
            mutation_id: MutationId::generate(),
            candidate_id: CandidateId::generate(),
            component_digest: digest('b'),
            state: PluginCanaryState::Succeeded,
            started_at_ms: Some(10),
            finished_at_ms: Some(20),
            observed_runs: 3,
            passed_runs: 3,
            failed_runs: 0,
            health_report_digest: Some(digest('c')),
            rollback_release_id: None,
        };
        record.validate().expect("成功状态应自洽");

        record.failed_runs = 1;
        assert!(matches!(
            record.validate(),
            Err(InvalidPluginEvolution::InvalidCanaryCounts { .. })
        ));

        record.state = PluginCanaryState::RolledBack;
        record.observed_runs = 3;
        record.passed_runs = 2;
        record.failed_runs = 1;
        assert!(matches!(
            record.validate(),
            Err(InvalidPluginEvolution::CanaryStateMismatch { .. })
        ));
        record.rollback_release_id = Some(ReleaseId::generate());
        record
            .validate()
            .expect("失败且有独立回滚 Release 时应通过");
    }
}

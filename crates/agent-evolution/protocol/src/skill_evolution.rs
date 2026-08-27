//! M7 Skill 自进化协议。
//!
//! 本模块只定义稳定、可验证的协议数据，不负责读取 CAS、查询 Episode Store、执行
//! Skill 或决定 Promotion。调用方必须把 Host/Recorder 掌握的可信绑定传入校验函数；
//! Skill 或插件自行声明的成功不能替代可信 Episode 终态与 Verifier 结论。

use crate::{
    ArtifactDigest, CandidateId, EpisodeId, EvaluationReportId, EventId, EvolutionCycleId,
    GateDecision, GenomeDigest, GenomeRevisionId, MutationId, MutationSurface, OutcomeRevisionId,
    RunId,
};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};
use thiserror::Error;
use uuid::Uuid;

/// 当前支持的 Skill 制品结构版本。
pub const SKILL_ARTIFACT_SCHEMA_VERSION: u32 = 1;
/// 当前支持的 Skill 使用观察结构版本。
pub const SKILL_USAGE_OBSERVATION_SCHEMA_VERSION: u32 = 2;
/// 当前支持的 Skill 变异提案结构版本。
pub const SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION: u32 = 1;
/// 当前支持的 Skill Candidate 结构版本。
pub const SKILL_CANDIDATE_SCHEMA_VERSION: u32 = 1;
/// 当前支持的 Skill 评测报告结构版本。
pub const SKILL_EVALUATION_REPORT_SCHEMA_VERSION: u32 = 2;

const SKILL_ID_BODY_MIN_BYTES: usize = 8;
const SKILL_ID_BODY_MAX_BYTES: usize = 64;
const MAX_SKILL_NAME_BYTES: usize = 128;
const MAX_SKILL_DESCRIPTION_BYTES: usize = 4_096;
const MAX_SKILL_INSTRUCTIONS_BYTES: usize = 65_536;
const MAX_SKILL_HYPOTHESIS_BYTES: usize = 4_096;
const MAX_LABEL_BYTES: usize = 128;
const MAX_PLUGIN_EVENT_KIND_BYTES: usize = 128;
const MAX_SET_ITEMS: usize = 256;
const MAX_PROPOSED_ARTIFACTS: usize = 16;
const MAX_STATUS_TRANSITIONS: usize = 64;
const MAX_USAGE_OBSERVATIONS: usize = 4_096;

/// Skill 的稳定标识。
///
/// 序列化形态为 `skill_<8-64 位小写字母或数字>`。Skill 内容变化不会改变该标识；
/// 具体修订由 Artifact CAS 摘要区分。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SkillId(String);

impl SkillId {
    /// Skill ID 的固定前缀。
    pub const PREFIX: &'static str = "skill";
    /// 跨语言校验可采用的稳定正则表达式。
    pub const PATTERN: &'static str = "^skill_[0-9a-z]{8,64}$";

    /// 生成不含时间、路径或用户信息的随机 Skill ID。
    pub fn generate() -> Self {
        Self(format!("{}_{}", Self::PREFIX, Uuid::new_v4().simple()))
    }

    /// 校验并创建 Skill ID。
    ///
    /// # Errors
    ///
    /// 前缀错误、正文长度越界，或正文含小写字母与数字之外的字符时返回
    /// [`InvalidSkillId`]。
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidSkillId> {
        let value = value.into();
        validate_skill_id(&value)?;
        Ok(Self(value))
    }

    /// 返回 Skill ID 的稳定字符串形式。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SkillId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for SkillId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl FromStr for SkillId {
    type Err = InvalidSkillId;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for SkillId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for SkillId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(de::Error::custom)
    }
}

/// Skill ID 校验错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidSkillId {
    /// ID 没有使用固定 `skill_` 前缀。
    #[error("SkillId 必须以 `skill_` 开头")]
    InvalidPrefix,
    /// ID 正文长度不在协议边界内。
    #[error("SkillId 正文长度必须位于 {min}..={max} 字节，实际为 {actual}")]
    InvalidLength {
        /// 最小字节数。
        min: usize,
        /// 最大字节数。
        max: usize,
        /// 实际字节数。
        actual: usize,
    },
    /// ID 正文出现非法字符。
    #[error("SkillId 正文只允许小写字母和数字，出现了 `{character}`")]
    InvalidCharacter {
        /// 首个非法字符。
        character: char,
    },
}

/// Skill 的自动选择方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillTriggerModeV1 {
    /// 只能由用户或可信控制面显式选择。
    Manual,
    /// 仅匹配明确的触发短语。
    Explicit,
    /// 仅匹配版本化语义意图。
    Semantic,
    /// 同时支持明确短语与语义意图。
    Hybrid,
}

/// Skill 的有界触发策略。
///
/// 所有集合使用 `BTreeSet`，因此 JSON 输出按字典序稳定排列并自动去重。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillTriggerPolicyV1 {
    /// 触发模式。
    pub mode: SkillTriggerModeV1,
    /// 显式触发短语；Manual/Semantic 模式下必须为空。
    #[serde(default)]
    pub explicit_triggers: BTreeSet<String>,
    /// 版本化语义意图名；Manual/Explicit 模式下必须为空。
    #[serde(default)]
    pub semantic_intents: BTreeSet<String>,
    /// 语义触发的最低置信度，单位为万分比。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum_confidence_bps: Option<u16>,
}

impl Default for SkillTriggerPolicyV1 {
    fn default() -> Self {
        Self {
            mode: SkillTriggerModeV1::Manual,
            explicit_triggers: BTreeSet::new(),
            semantic_intents: BTreeSet::new(),
            minimum_confidence_bps: None,
        }
    }
}

impl SkillTriggerPolicyV1 {
    /// 校验触发模式与触发集合的一致性及全部字段边界。
    ///
    /// # Errors
    ///
    /// 模式所需集合为空、出现模式不允许的字段、置信度越界，或字符串集合超出数量与
    /// 长度上限时返回 [`InvalidSkillEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidSkillEvolution> {
        validate_label_set("explicit_triggers", &self.explicit_triggers, true)?;
        validate_label_set("semantic_intents", &self.semantic_intents, true)?;
        let has_explicit = !self.explicit_triggers.is_empty();
        let has_semantic = !self.semantic_intents.is_empty();
        let has_confidence = self.minimum_confidence_bps.is_some();
        let shape_is_valid = match self.mode {
            SkillTriggerModeV1::Manual => !has_explicit && !has_semantic && !has_confidence,
            SkillTriggerModeV1::Explicit => has_explicit && !has_semantic && !has_confidence,
            SkillTriggerModeV1::Semantic => !has_explicit && has_semantic && has_confidence,
            SkillTriggerModeV1::Hybrid => has_explicit && has_semantic && has_confidence,
        };
        if !shape_is_valid {
            return Err(InvalidSkillEvolution::InvalidTriggerPolicy);
        }
        if let Some(value) = self.minimum_confidence_bps {
            if !(1..=10_000).contains(&value) {
                return Err(InvalidSkillEvolution::InvalidConfidence(value));
            }
        }
        Ok(())
    }
}

/// Skill 的生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillStatusV1 {
    /// 新建或重新隔离的 Skill，尚不可自动装载。
    Quarantined,
    /// 已生成可信独立评测，但尚未进入 Stable Skill Set。
    Evaluated,
    /// 已通过 Skill Commit Policy，可由新运行装载。
    Active,
    /// 已停止进入新运行，但历史修订仍可追溯。
    Deprecated,
    /// 逻辑删除；CAS 制品、状态链和审计记录必须保留。
    Deleted,
}

/// Skill 状态链中的一条只追加记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillStatusTransitionV1 {
    /// 该条记录产生后的状态。
    pub status: SkillStatusV1,
    /// 可信控制面记录的 Unix 毫秒时间，必须严格递增。
    pub recorded_at_ms: u64,
    /// 支撑 `Evaluated` 或 `Active` 状态的正式评测报告。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_report_id: Option<EvaluationReportId>,
}

/// Delete 操作允许的唯一语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillDeletionModeV1 {
    /// 追加 Deleted 墓碑，并永久保留既有 CAS 制品和审计链。
    #[default]
    LogicalTombstone,
}

/// 一次 Skill 制品修订的来源操作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SkillOperationV1 {
    /// 创建新的 Skill 稳定 ID。
    Create,
    /// 更新同一 Skill；前一修订仍保留在 CAS。
    Update {
        /// 前一 Skill 制品的 CAS 摘要。
        previous_artifact_digest: ArtifactDigest,
    },
    /// 合并至少两个 Skill 制品。
    Merge {
        /// 来源 Skill ID 到其精确制品摘要的稳定映射。
        source_artifacts: BTreeMap<SkillId, ArtifactDigest>,
    },
    /// 把一个 Skill 拆分为至少两个结果 Skill。
    Split {
        /// 被拆分的来源 Skill ID。
        source_skill_id: SkillId,
        /// 被拆分制品的 CAS 摘要。
        source_artifact_digest: ArtifactDigest,
        /// 同一 Split 操作产生的全部 Skill ID；必须包含当前制品的 ID。
        result_skill_ids: BTreeSet<SkillId>,
    },
    /// 停止把 Skill 装配进新运行，但保留全部历史。
    Deprecate {
        /// 被弃用 Skill 的前一制品摘要。
        previous_artifact_digest: ArtifactDigest,
    },
    /// 追加逻辑删除墓碑；协议不表达物理删除。
    Delete {
        /// 被逻辑删除 Skill 的前一制品摘要。
        previous_artifact_digest: ArtifactDigest,
        /// 删除模式；V1 只能是逻辑墓碑。
        #[serde(default)]
        deletion_mode: SkillDeletionModeV1,
    },
}

/// 一个可写入 Artifact CAS 的版本化 Skill 制品。
///
/// `status_history` 是完整的只追加状态链。任何更新都应创建新的 CAS 制品，禁止覆盖
/// 旧字节；Delete 也必须保留名称、说明、指令、来源与前一制品摘要以供审计。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillArtifactV1 {
    /// Skill 制品结构版本。
    pub schema_version: u32,
    /// 跨修订稳定的 Skill ID。
    pub skill_id: SkillId,
    /// 同一 Skill ID 下从 1 开始单调递增的修订号。
    pub revision: u32,
    /// 产生本修订的操作与不可删除的来源摘要。
    pub operation: SkillOperationV1,
    /// 面向选择器与审计界面的短名称。
    pub name: String,
    /// 不含 Secret、Hidden 数据或原始 ToolResult 的用途说明。
    pub description: String,
    /// Skill 的完整版本化指令正文。
    pub instructions: String,
    /// 触发规则。
    #[serde(default)]
    pub trigger_policy: SkillTriggerPolicyV1,
    /// 执行本 Skill 所需能力；必须由 Candidate Builder 证明不超出 Parent。
    #[serde(default)]
    pub required_capabilities: BTreeSet<String>,
    /// 产生本制品的脱敏 Episode；只保存 ID，不保存正文或 Hidden 内容。
    #[serde(default)]
    pub source_episode_ids: BTreeSet<EpisodeId>,
    /// 产生本制品的 Mutation。
    pub mutation_id: MutationId,
    /// 从 `Quarantined` 开始的完整只追加状态链。
    #[serde(default)]
    pub status_history: Vec<SkillStatusTransitionV1>,
}

impl SkillArtifactV1 {
    /// 校验制品版本、文本边界、来源、操作语义、能力集合和只追加状态链。
    ///
    /// # Errors
    ///
    /// schema 不受支持、字段为空或过长、Create 修订号错误、Merge/Split 来源不完整、
    /// 状态不是从 Quarantined 开始、状态跃迁非法，或 Delete 未以 Deleted 结束时返回
    /// [`InvalidSkillEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidSkillEvolution> {
        if self.schema_version != SKILL_ARTIFACT_SCHEMA_VERSION {
            return Err(InvalidSkillEvolution::UnsupportedSchemaVersion {
                schema: "SkillArtifact",
                found: self.schema_version,
                supported: SKILL_ARTIFACT_SCHEMA_VERSION,
            });
        }
        if self.revision == 0 {
            return Err(InvalidSkillEvolution::InvalidRevision);
        }
        validate_text("name", &self.name, MAX_SKILL_NAME_BYTES)?;
        validate_text(
            "description",
            &self.description,
            MAX_SKILL_DESCRIPTION_BYTES,
        )?;
        validate_text(
            "instructions",
            &self.instructions,
            MAX_SKILL_INSTRUCTIONS_BYTES,
        )?;
        self.trigger_policy.validate()?;
        validate_label_set("required_capabilities", &self.required_capabilities, true)?;
        if self.source_episode_ids.is_empty() || self.source_episode_ids.len() > MAX_SET_ITEMS {
            return Err(InvalidSkillEvolution::InvalidCollectionSize {
                field: "source_episode_ids",
                min: 1,
                max: MAX_SET_ITEMS,
                actual: self.source_episode_ids.len(),
            });
        }
        self.validate_operation()?;
        validate_status_history(&self.status_history)?;
        let final_status = self
            .status_history
            .last()
            .map(|entry| entry.status)
            .ok_or(InvalidSkillEvolution::MissingStatusHistory)?;
        match self.operation {
            SkillOperationV1::Deprecate { .. } if final_status != SkillStatusV1::Deprecated => {
                Err(InvalidSkillEvolution::OperationStatusMismatch {
                    operation: "deprecate",
                    expected: SkillStatusV1::Deprecated,
                    actual: final_status,
                })
            }
            SkillOperationV1::Delete { .. } if final_status != SkillStatusV1::Deleted => {
                Err(InvalidSkillEvolution::OperationStatusMismatch {
                    operation: "delete",
                    expected: SkillStatusV1::Deleted,
                    actual: final_status,
                })
            }
            _ => Ok(()),
        }
    }

    /// 返回可用于 Artifact CAS 的稳定 JSON 字节。
    ///
    /// # Errors
    ///
    /// 制品不满足协议不变量，或 JSON 序列化失败时返回 [`InvalidSkillEvolution`]。
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, InvalidSkillEvolution> {
        self.validate()?;
        serde_json::to_vec(self)
            .map_err(|error| InvalidSkillEvolution::Serialization(error.to_string()))
    }

    /// 计算规范 Skill 制品字节的 SHA-256 Artifact 摘要。
    ///
    /// # Errors
    ///
    /// 制品无效、JSON 序列化失败，或摘要无法构造时返回 [`InvalidSkillEvolution`]。
    pub fn digest(&self) -> Result<ArtifactDigest, InvalidSkillEvolution> {
        let bytes = self.canonical_bytes()?;
        let hex = format!("{:x}", Sha256::digest(bytes));
        ArtifactDigest::from_sha256_hex(hex)
            .map_err(|error| InvalidSkillEvolution::InvalidArtifactDigest(error.to_string()))
    }

    /// 从 JSON 读取并校验 Skill 制品。
    ///
    /// 未知加法字段会被忽略；有默认值的集合与可选字段允许缺失。核心绑定字段缺失、
    /// JSON 无效或结构校验失败时返回 [`InvalidSkillEvolution`]。
    pub fn from_json_slice(bytes: &[u8]) -> Result<Self, InvalidSkillEvolution> {
        let artifact: Self = serde_json::from_slice(bytes)
            .map_err(|error| InvalidSkillEvolution::Serialization(error.to_string()))?;
        artifact.validate()?;
        Ok(artifact)
    }

    fn validate_operation(&self) -> Result<(), InvalidSkillEvolution> {
        match &self.operation {
            SkillOperationV1::Create => {
                if self.revision != 1 {
                    return Err(InvalidSkillEvolution::CreateRevisionMustBeOne);
                }
            }
            SkillOperationV1::Update { .. }
            | SkillOperationV1::Deprecate { .. }
            | SkillOperationV1::Delete { .. } => {
                if self.revision <= 1 {
                    return Err(InvalidSkillEvolution::ExistingSkillRevisionRequired);
                }
            }
            SkillOperationV1::Merge { source_artifacts } => {
                if source_artifacts.len() < 2 || source_artifacts.len() > MAX_SET_ITEMS {
                    return Err(InvalidSkillEvolution::InvalidCollectionSize {
                        field: "merge.source_artifacts",
                        min: 2,
                        max: MAX_SET_ITEMS,
                        actual: source_artifacts.len(),
                    });
                }
            }
            SkillOperationV1::Split {
                result_skill_ids, ..
            } => {
                if result_skill_ids.len() < 2 || result_skill_ids.len() > MAX_SET_ITEMS {
                    return Err(InvalidSkillEvolution::InvalidCollectionSize {
                        field: "split.result_skill_ids",
                        min: 2,
                        max: MAX_SET_ITEMS,
                        actual: result_skill_ids.len(),
                    });
                }
                if !result_skill_ids.contains(&self.skill_id) {
                    return Err(InvalidSkillEvolution::SplitResultMissingCurrentSkill);
                }
            }
        }
        Ok(())
    }
}

/// Core 可信事件流中的真实原生 Skill 工具终态引用。
///
/// `payload_digest` 只固定脱敏事件 payload，不把用户正文或 ToolResult 放入协议。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TrustedSkillToolEventRefV1 {
    /// Event Envelope 中的真实事件 ID。
    pub event_id: EventId,
    /// Episode 内从 1 开始的单调事件序号。
    pub sequence: u64,
    /// Core 在事件产生时注入的运行来源；原生 Skill 固定为 `native`。
    pub runtime_origin: String,
    /// Core 记录的真实工具名；原生 Skill 固定为 `skill_read`。
    pub tool_name: String,
    /// 脱敏事件 payload 的 Artifact CAS 摘要。
    pub payload_digest: ArtifactDigest,
}

impl TrustedSkillToolEventRefV1 {
    /// 校验事件序号以及运行来源、工具名的文本边界。
    ///
    /// # Errors
    ///
    /// 序号为零，或文本字段为空、过长时返回 [`InvalidSkillEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidSkillEvolution> {
        if self.sequence == 0 {
            return Err(InvalidSkillEvolution::InvalidSkillToolEventSequence);
        }
        validate_label("runtime_origin", &self.runtime_origin)?;
        validate_text("tool_name", &self.tool_name, MAX_PLUGIN_EVENT_KIND_BYTES)
    }
}

/// Recorder/Core 从可信 Episode 与原生 Skill 工具事件解析出的使用绑定。
///
/// 此结构是校验输入，不接受 Skill 或模型自行构造的值作为事实来源。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TrustedSkillUsageBindingV1 {
    /// 真实 Episode。
    pub episode_id: EpisodeId,
    /// Episode 对应的真实运行。
    pub run_id: RunId,
    /// 运行开始时固定的 Genome 修订。
    pub genome_revision_id: GenomeRevisionId,
    /// 实际装载的 Skill ID。
    pub skill_id: SkillId,
    /// 实际装载的 Skill 制品摘要。
    pub skill_artifact_digest: ArtifactDigest,
    /// 真实原生 Skill 工具终态引用。
    pub tool_event: TrustedSkillToolEventRefV1,
}

impl TrustedSkillUsageBindingV1 {
    /// 校验真实原生 Skill 工具事件引用的局部结构。
    ///
    /// # Errors
    ///
    /// 工具事件序号或文本字段非法时返回 [`InvalidSkillEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidSkillEvolution> {
        self.tool_event.validate()
    }
}

/// 使用观察的事实来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillUsageEvidenceSourceV1 {
    /// Recorder 把真实原生工具事件绑定到可信 Episode 终态与 OutcomeRevision。
    TrustedEpisodeOutcome,
    /// Skill 或插件自行上报；只能用于诊断，禁止进入评测成功计数。
    SkillSelfReported,
}

/// 一次 Skill 使用的可信结果分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillUsageResultV1 {
    /// Skill 被正确选择且可信 Verifier 判定任务成功。
    VerifiedSuccess,
    /// Skill 被选择，但可信 Verifier 判定任务失败。
    VerifiedFailure,
    /// Skill 不应触发却被选择。
    IncorrectTrigger,
    /// Skill 应触发却未被选择。
    MissedTrigger,
}

/// 一次与可信 Episode、Run、Genome 和真实原生工具事件绑定的 Skill 使用观察。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillUsageObservationV1 {
    /// 使用观察结构版本。
    pub schema_version: u32,
    /// Recorder/Host 观测到的完整绑定。
    pub binding: TrustedSkillUsageBindingV1,
    /// 支撑任务终态的 Outcome 修订。
    pub outcome_revision_id: OutcomeRevisionId,
    /// 观察事实来源；自动评测只接受 `TrustedEpisodeOutcome`。
    pub evidence_source: SkillUsageEvidenceSourceV1,
    /// 可信结果分类。
    pub result: SkillUsageResultV1,
    /// 可信 Verifier 判定；成功必须为 `Some(true)`，失败必须为 `Some(false)`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verifier_passed: Option<bool>,
    /// 可信安全 Verifier 报告的失败数。
    #[serde(default)]
    pub safety_failures: u32,
    /// 可信控制面记录的 Unix 毫秒时间。
    pub observed_at_ms: u64,
}

impl SkillUsageObservationV1 {
    /// 使用 Host/Recorder 提供的可信绑定校验观察，拒绝 Skill 自报成功。
    ///
    /// # Errors
    ///
    /// schema 不受支持、绑定与可信 Episode/Run/Genome/Skill/事件任一字段不一致、事实
    /// 来源是 Skill 自报，或结果与 Verifier 判定矛盾时返回 [`InvalidSkillEvolution`]。
    pub fn validate(
        &self,
        trusted_binding: &TrustedSkillUsageBindingV1,
    ) -> Result<(), InvalidSkillEvolution> {
        if self.schema_version != SKILL_USAGE_OBSERVATION_SCHEMA_VERSION {
            return Err(InvalidSkillEvolution::UnsupportedSchemaVersion {
                schema: "SkillUsageObservation",
                found: self.schema_version,
                supported: SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
            });
        }
        self.binding.validate()?;
        trusted_binding.validate()?;
        if &self.binding != trusted_binding {
            return Err(InvalidSkillEvolution::UsageBindingMismatch);
        }
        if self.evidence_source != SkillUsageEvidenceSourceV1::TrustedEpisodeOutcome {
            return Err(InvalidSkillEvolution::UntrustedSkillSelfReport);
        }
        let verifier_is_consistent = match self.result {
            SkillUsageResultV1::VerifiedSuccess => self.verifier_passed == Some(true),
            SkillUsageResultV1::VerifiedFailure => self.verifier_passed == Some(false),
            SkillUsageResultV1::IncorrectTrigger | SkillUsageResultV1::MissedTrigger => {
                self.verifier_passed != Some(true)
            }
        };
        if !verifier_is_consistent {
            return Err(InvalidSkillEvolution::InconsistentVerifierResult);
        }
        if matches!(self.result, SkillUsageResultV1::VerifiedSuccess) && self.safety_failures != 0 {
            return Err(InvalidSkillEvolution::SuccessfulUsageHasSafetyFailure);
        }
        if self.observed_at_ms == 0 {
            return Err(InvalidSkillEvolution::InvalidTimestamp {
                field: "observed_at_ms",
            });
        }
        Ok(())
    }
}

/// Skill Genome Set 中一条强类型、可验证的引用。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SkillGenomeRefV1 {
    /// Skill 稳定 ID，也是 Skill Set 的排序键。
    pub skill_id: SkillId,
    /// 该运行实际装载的 Skill 制品摘要。
    pub artifact_digest: ArtifactDigest,
    /// 该制品声明的必需能力。
    #[serde(default)]
    pub required_capabilities: BTreeSet<String>,
}

impl SkillGenomeRefV1 {
    /// 校验必需能力集合的数量与名称边界。
    ///
    /// # Errors
    ///
    /// 能力数量过多，或名称为空、过长时返回 [`InvalidSkillEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidSkillEvolution> {
        validate_label_set(
            "skill_set.required_capabilities",
            &self.required_capabilities,
            true,
        )
    }
}

/// M7 Mutator 产生的受限 Skill 变异提案。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMutationProposalV1 {
    /// 提案结构版本。
    pub schema_version: u32,
    /// 本次变异的稳定 ID。
    pub mutation_id: MutationId,
    /// 提案基于的 Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// 提案生成时观察到的 Parent Genome 摘要。
    pub parent_genome_digest: GenomeDigest,
    /// 支撑提案的脱敏 Episode ID。
    #[serde(default)]
    pub evidence_episode_ids: BTreeSet<EpisodeId>,
    /// 按 `(skill_id, revision)` 严格升序的候选 Skill 制品。
    #[serde(default)]
    pub proposed_artifacts: Vec<SkillArtifactV1>,
    /// 不含 Secret、Hidden 内容或原始 ToolResult 的有界假设。
    pub hypothesis: String,
}

impl SkillMutationProposalV1 {
    /// 校验提案与每个 Skill 制品的 Mutation、Episode 和排序绑定。
    ///
    /// # Errors
    ///
    /// schema 不受支持、证据或制品为空、字段越界、制品未严格排序，或制品绑定到其他
    /// Mutation/Episode 时返回 [`InvalidSkillEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidSkillEvolution> {
        if self.schema_version != SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION {
            return Err(InvalidSkillEvolution::UnsupportedSchemaVersion {
                schema: "SkillMutationProposal",
                found: self.schema_version,
                supported: SKILL_MUTATION_PROPOSAL_SCHEMA_VERSION,
            });
        }
        if self.evidence_episode_ids.is_empty() || self.evidence_episode_ids.len() > MAX_SET_ITEMS {
            return Err(InvalidSkillEvolution::InvalidCollectionSize {
                field: "evidence_episode_ids",
                min: 1,
                max: MAX_SET_ITEMS,
                actual: self.evidence_episode_ids.len(),
            });
        }
        if self.proposed_artifacts.is_empty()
            || self.proposed_artifacts.len() > MAX_PROPOSED_ARTIFACTS
        {
            return Err(InvalidSkillEvolution::InvalidCollectionSize {
                field: "proposed_artifacts",
                min: 1,
                max: MAX_PROPOSED_ARTIFACTS,
                actual: self.proposed_artifacts.len(),
            });
        }
        validate_text("hypothesis", &self.hypothesis, MAX_SKILL_HYPOTHESIS_BYTES)?;
        ensure_strict_skill_artifact_order(&self.proposed_artifacts)?;
        for artifact in &self.proposed_artifacts {
            artifact.validate()?;
            if artifact.mutation_id != self.mutation_id {
                return Err(InvalidSkillEvolution::MutationBindingMismatch);
            }
            if artifact.source_episode_ids != self.evidence_episode_ids {
                return Err(InvalidSkillEvolution::EpisodeBindingMismatch);
            }
        }
        Ok(())
    }
}

/// 可信 Candidate Builder 产生的 Skill Candidate。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillCandidateV1 {
    /// Candidate 结构版本。
    pub schema_version: u32,
    /// Candidate 稳定 ID。
    pub candidate_id: CandidateId,
    /// Candidate 所属进化周期。
    pub cycle_id: EvolutionCycleId,
    /// 来源 Mutation。
    pub mutation_id: MutationId,
    /// Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// Candidate Genome 修订。
    pub candidate_revision_id: GenomeRevisionId,
    /// Parent Genome 行为摘要。
    pub parent_genome_digest: GenomeDigest,
    /// Candidate Genome 行为摘要。
    pub candidate_genome_digest: GenomeDigest,
    /// Parent Genome 的 Skill Set，按 Skill ID 严格升序。
    #[serde(default)]
    pub parent_skill_set: Vec<SkillGenomeRefV1>,
    /// Candidate Genome 的 Skill Set，按 Skill ID 严格升序。
    #[serde(default)]
    pub candidate_skill_set: Vec<SkillGenomeRefV1>,
    /// Parent 允许 Skill 使用的能力集合。
    #[serde(default)]
    pub parent_capabilities: BTreeSet<String>,
    /// Candidate 实际要求的能力集合，必须是 Parent 的子集。
    #[serde(default)]
    pub candidate_capabilities: BTreeSet<String>,
    /// 本次提案生成的 Skill ID 到 CAS 制品摘要的稳定映射。
    #[serde(default)]
    pub candidate_artifact_digests: BTreeMap<SkillId, ArtifactDigest>,
    /// 可信完整 Genome Diff 的实际变化表面。
    #[serde(default)]
    pub changed_surfaces: BTreeSet<MutationSurface>,
    /// 已绑定的正式评测报告；尚未评测时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_report_id: Option<EvaluationReportId>,
    /// 可信控制面记录的 Unix 毫秒时间。
    pub created_at_ms: u64,
}

impl SkillCandidateV1 {
    /// 校验 Candidate 的 Genome 差异、Skill Set 排序和能力不扩大约束。
    ///
    /// # Errors
    ///
    /// schema 不受支持、Parent/Candidate 相同、Skill Set 未严格排序或未发生变化、变化
    /// 表面不精确等于 `{Skill}`、能力扩大，或 Skill 声明了 Candidate 未提供的能力时返回
    /// [`InvalidSkillEvolution`]。
    pub fn validate(&self) -> Result<(), InvalidSkillEvolution> {
        if self.schema_version != SKILL_CANDIDATE_SCHEMA_VERSION {
            return Err(InvalidSkillEvolution::UnsupportedSchemaVersion {
                schema: "SkillCandidate",
                found: self.schema_version,
                supported: SKILL_CANDIDATE_SCHEMA_VERSION,
            });
        }
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(InvalidSkillEvolution::SameGenomeRevision);
        }
        if self.parent_genome_digest == self.candidate_genome_digest {
            return Err(InvalidSkillEvolution::SameGenomeDigest);
        }
        ensure_strict_skill_ref_order("parent_skill_set", &self.parent_skill_set)?;
        ensure_strict_skill_ref_order("candidate_skill_set", &self.candidate_skill_set)?;
        if self.parent_skill_set == self.candidate_skill_set {
            return Err(InvalidSkillEvolution::UnchangedSkillSet);
        }
        let expected = BTreeSet::from([MutationSurface::Skill]);
        if self.changed_surfaces != expected {
            return Err(InvalidSkillEvolution::InvalidCandidateSurfaces(
                self.changed_surfaces.clone(),
            ));
        }
        validate_label_set("parent_capabilities", &self.parent_capabilities, true)?;
        validate_label_set("candidate_capabilities", &self.candidate_capabilities, true)?;
        if !self
            .candidate_capabilities
            .is_subset(&self.parent_capabilities)
        {
            return Err(InvalidSkillEvolution::CapabilityExpansion);
        }
        if self.candidate_artifact_digests.is_empty()
            || self.candidate_artifact_digests.len() > MAX_PROPOSED_ARTIFACTS
        {
            return Err(InvalidSkillEvolution::InvalidCollectionSize {
                field: "candidate_artifact_digests",
                min: 1,
                max: MAX_PROPOSED_ARTIFACTS,
                actual: self.candidate_artifact_digests.len(),
            });
        }
        for skill in &self.parent_skill_set {
            skill.validate()?;
        }
        for skill in &self.candidate_skill_set {
            skill.validate()?;
            if !skill
                .required_capabilities
                .is_subset(&self.candidate_capabilities)
            {
                return Err(InvalidSkillEvolution::UnavailableSkillCapability {
                    skill_id: skill.skill_id.clone(),
                });
            }
        }
        for (skill_id, digest) in &self.candidate_artifact_digests {
            let is_installed = self
                .candidate_skill_set
                .iter()
                .any(|skill| &skill.skill_id == skill_id && &skill.artifact_digest == digest);
            let is_deleted = !self
                .candidate_skill_set
                .iter()
                .any(|skill| &skill.skill_id == skill_id);
            if !is_installed && !is_deleted {
                return Err(InvalidSkillEvolution::CandidateArtifactBindingMismatch {
                    skill_id: skill_id.clone(),
                });
            }
        }
        if self.created_at_ms == 0 {
            return Err(InvalidSkillEvolution::InvalidTimestamp {
                field: "created_at_ms",
            });
        }
        Ok(())
    }

    /// 同时校验 Candidate 与 MutationProposal、可选 EvaluationReport 的双向绑定。
    ///
    /// `evaluation_report` 必须来自独立可信 Evaluator。Candidate 尚未评测时，
    /// `evaluation_report_id` 与参数都应为 `None`。
    ///
    /// # Errors
    ///
    /// 任一结构无效、Mutation/Parent/制品集合错绑、只提供一侧 Evaluation 绑定，或报告
    /// 指向其他 Candidate/Genome 时返回 [`InvalidSkillEvolution`]。
    pub fn validate_bindings(
        &self,
        proposal: &SkillMutationProposalV1,
        evaluation_report: Option<&SkillEvaluationReportV1>,
        trusted_usage_bindings: &BTreeMap<EventId, TrustedSkillUsageBindingV1>,
    ) -> Result<(), InvalidSkillEvolution> {
        self.validate()?;
        proposal.validate()?;
        if self.mutation_id != proposal.mutation_id
            || self.parent_revision_id != proposal.parent_revision_id
            || self.parent_genome_digest != proposal.parent_genome_digest
        {
            return Err(InvalidSkillEvolution::MutationBindingMismatch);
        }
        let proposed: BTreeSet<SkillId> = proposal
            .proposed_artifacts
            .iter()
            .map(|artifact| artifact.skill_id.clone())
            .collect();
        let built: BTreeSet<SkillId> = self.candidate_artifact_digests.keys().cloned().collect();
        if proposed != built {
            return Err(InvalidSkillEvolution::CandidateArtifactSetMismatch);
        }
        for artifact in &proposal.proposed_artifacts {
            let actual_digest = artifact.digest()?;
            if self.candidate_artifact_digests.get(&artifact.skill_id) != Some(&actual_digest) {
                return Err(InvalidSkillEvolution::CandidateArtifactBindingMismatch {
                    skill_id: artifact.skill_id.clone(),
                });
            }
            let installed = self
                .candidate_skill_set
                .iter()
                .find(|skill| skill.skill_id == artifact.skill_id);
            match &artifact.operation {
                SkillOperationV1::Deprecate { .. } | SkillOperationV1::Delete { .. } => {
                    if installed.is_some() {
                        return Err(InvalidSkillEvolution::RemovedSkillStillInstalled {
                            skill_id: artifact.skill_id.clone(),
                        });
                    }
                }
                _ => {
                    if installed.map(|skill| &skill.artifact_digest) != Some(&actual_digest) {
                        return Err(InvalidSkillEvolution::CandidateArtifactBindingMismatch {
                            skill_id: artifact.skill_id.clone(),
                        });
                    }
                }
            }
        }
        match (self.evaluation_report_id.as_ref(), evaluation_report) {
            (None, None) => Ok(()),
            (Some(expected_id), Some(report)) => {
                report.validate(trusted_usage_bindings)?;
                if &report.report_id != expected_id
                    || report.mutation_id != self.mutation_id
                    || report.candidate_id != self.candidate_id
                    || report.parent_revision_id != self.parent_revision_id
                    || report.candidate_revision_id != self.candidate_revision_id
                    || report.parent_genome_digest != self.parent_genome_digest
                    || report.candidate_genome_digest != self.candidate_genome_digest
                    || report.evaluated_skill_ids != built
                {
                    return Err(InvalidSkillEvolution::EvaluationBindingMismatch);
                }
                Ok(())
            }
            _ => Err(InvalidSkillEvolution::EvaluationBindingMismatch),
        }
    }

    /// 把任意顺序的 Skill 引用规范化为按 ID 排序、相同项去重的 Skill Set。
    ///
    /// # Errors
    ///
    /// 同一 Skill ID 对应不同制品或能力声明时返回 [`InvalidSkillEvolution`]；这种冲突
    /// 不能通过静默覆盖解决。
    pub fn normalize_skill_set(
        skills: impl IntoIterator<Item = SkillGenomeRefV1>,
    ) -> Result<Vec<SkillGenomeRefV1>, InvalidSkillEvolution> {
        let mut normalized = BTreeMap::<SkillId, SkillGenomeRefV1>::new();
        for skill in skills {
            skill.validate()?;
            match normalized.get(&skill.skill_id) {
                Some(existing) if existing != &skill => {
                    return Err(InvalidSkillEvolution::ConflictingSkillReference {
                        skill_id: skill.skill_id,
                    });
                }
                Some(_) => {}
                None => {
                    normalized.insert(skill.skill_id.clone(), skill);
                }
            }
        }
        Ok(normalized.into_values().collect())
    }

    /// 返回验证后的稳定 JSON 字节。
    ///
    /// # Errors
    ///
    /// Candidate 无效或 JSON 序列化失败时返回 [`InvalidSkillEvolution`]。
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, InvalidSkillEvolution> {
        self.validate()?;
        serde_json::to_vec(self)
            .map_err(|error| InvalidSkillEvolution::Serialization(error.to_string()))
    }
}

/// Skill 独立 Commit Gate 的硬失败类别。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillGateFailureV1 {
    /// 没有任何可由可信事件复核的使用观察。
    NoTrustedUsage,
    /// 至少一次可信任务结果失败。
    VerifiedFailure,
    /// 出现误触发或漏触发。
    TriggerRegression,
    /// 可信安全 Verifier 报告失败。
    SafetyFailure,
    /// Candidate 所需能力超出 Parent。
    CapabilityExpansion,
    /// 完整 Genome Diff 不只包含 Skill。
    GenomeDiff,
    /// Skill、Episode、事件或评测制品完整性失败。
    Integrity,
}

/// 独立 Evaluator 产生的 Skill 对照评测报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEvaluationReportV1 {
    /// 报告结构版本。
    pub schema_version: u32,
    /// 报告稳定 ID。
    pub report_id: EvaluationReportId,
    /// 被评测的 Mutation。
    pub mutation_id: MutationId,
    /// 被评测的 Candidate。
    pub candidate_id: CandidateId,
    /// Parent Genome 修订。
    pub parent_revision_id: GenomeRevisionId,
    /// Candidate Genome 修订。
    pub candidate_revision_id: GenomeRevisionId,
    /// Parent Genome 摘要。
    pub parent_genome_digest: GenomeDigest,
    /// Candidate Genome 摘要。
    pub candidate_genome_digest: GenomeDigest,
    /// 本报告评测的全部 Skill ID。
    #[serde(default)]
    pub evaluated_skill_ids: BTreeSet<SkillId>,
    /// 按真实原生 Skill 工具 Event ID 严格升序的使用观察。
    #[serde(default)]
    pub observations: Vec<SkillUsageObservationV1>,
    /// Skill Commit Policy 的结论；V1 自动 Gate 只接受 Pass 或 Reject。
    pub decision: GateDecision,
    /// 硬失败集合；为空当且仅当结论为 Pass。
    #[serde(default)]
    pub failures: BTreeSet<SkillGateFailureV1>,
    /// 可信控制面记录的 Unix 毫秒生成时间。
    pub generated_at_ms: u64,
}

impl SkillEvaluationReportV1 {
    /// 使用 Host/Recorder 的真实事件索引校验报告及全部使用观察。
    ///
    /// Skill 自报成功、观察错绑、重复/乱序事件、通过报告含失败观察、决策与失败集合
    /// 不一致时均失败关闭。
    ///
    /// # Errors
    ///
    /// 报告版本、Genome 绑定、观察顺序或可信事件绑定无效，或 Gate 结论与观察矛盾时
    /// 返回 [`InvalidSkillEvolution`]。
    pub fn validate(
        &self,
        trusted_usage_bindings: &BTreeMap<EventId, TrustedSkillUsageBindingV1>,
    ) -> Result<(), InvalidSkillEvolution> {
        if self.schema_version != SKILL_EVALUATION_REPORT_SCHEMA_VERSION {
            return Err(InvalidSkillEvolution::UnsupportedSchemaVersion {
                schema: "SkillEvaluationReport",
                found: self.schema_version,
                supported: SKILL_EVALUATION_REPORT_SCHEMA_VERSION,
            });
        }
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(InvalidSkillEvolution::SameGenomeRevision);
        }
        if self.parent_genome_digest == self.candidate_genome_digest {
            return Err(InvalidSkillEvolution::SameGenomeDigest);
        }
        if self.evaluated_skill_ids.is_empty() || self.evaluated_skill_ids.len() > MAX_SET_ITEMS {
            return Err(InvalidSkillEvolution::InvalidCollectionSize {
                field: "evaluated_skill_ids",
                min: 1,
                max: MAX_SET_ITEMS,
                actual: self.evaluated_skill_ids.len(),
            });
        }
        if self.observations.len() > MAX_USAGE_OBSERVATIONS {
            return Err(InvalidSkillEvolution::InvalidCollectionSize {
                field: "observations",
                min: 0,
                max: MAX_USAGE_OBSERVATIONS,
                actual: self.observations.len(),
            });
        }
        let mut previous_event: Option<&EventId> = None;
        for observation in &self.observations {
            let event_id = &observation.binding.tool_event.event_id;
            if previous_event.is_some_and(|previous| previous >= event_id) {
                return Err(InvalidSkillEvolution::UnorderedCollection {
                    field: "observations",
                });
            }
            previous_event = Some(event_id);
            let trusted = trusted_usage_bindings
                .get(event_id)
                .ok_or(InvalidSkillEvolution::MissingTrustedSkillToolEvent)?;
            observation.validate(trusted)?;
            if !self
                .evaluated_skill_ids
                .contains(&observation.binding.skill_id)
            {
                return Err(InvalidSkillEvolution::EvaluationSkillMismatch);
            }
        }
        let consistent_decision = match self.decision {
            GateDecision::Pass => {
                !self.observations.is_empty()
                    && self.failures.is_empty()
                    && self.observations.iter().all(|observation| {
                        observation.result == SkillUsageResultV1::VerifiedSuccess
                            && observation.safety_failures == 0
                    })
            }
            GateDecision::Reject => !self.failures.is_empty(),
            GateDecision::RequireApproval | GateDecision::Unknown => false,
        };
        if !consistent_decision {
            return Err(InvalidSkillEvolution::InconsistentGateDecision);
        }
        if self.generated_at_ms == 0 {
            return Err(InvalidSkillEvolution::InvalidTimestamp {
                field: "generated_at_ms",
            });
        }
        Ok(())
    }
}

/// M7 Skill 协议结构校验错误。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidSkillEvolution {
    /// schema 版本不受支持。
    #[error("不支持的 {schema} schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSchemaVersion {
        /// schema 名称。
        schema: &'static str,
        /// 实际版本。
        found: u32,
        /// 当前支持版本。
        supported: u32,
    },
    /// 文本字段为空或过长。
    #[error("Skill 字段 `{field}` 必须非空且不超过 {max_bytes} 字节")]
    InvalidText {
        /// 字段名。
        field: &'static str,
        /// 最大 UTF-8 字节数。
        max_bytes: usize,
    },
    /// 集合数量不在协议边界内。
    #[error("Skill 集合 `{field}` 数量必须位于 {min}..={max}，实际为 {actual}")]
    InvalidCollectionSize {
        /// 字段名。
        field: &'static str,
        /// 最少项目数。
        min: usize,
        /// 最大项目数。
        max: usize,
        /// 实际项目数。
        actual: usize,
    },
    /// 标签字段含空白、控制字符或长度越界。
    #[error("Skill 标签 `{field}` 含空值、控制字符或超过 {max_bytes} 字节")]
    InvalidLabel {
        /// 字段名。
        field: &'static str,
        /// 最大 UTF-8 字节数。
        max_bytes: usize,
    },
    /// 触发模式与字段组合不一致。
    #[error("SkillTriggerPolicy 的模式与触发短语、语义意图或置信度不一致")]
    InvalidTriggerPolicy,
    /// 语义置信度不在 1..=10000。
    #[error("Skill 语义触发置信度 {0}bps 必须位于 1..=10000")]
    InvalidConfidence(u16),
    /// Skill 修订号不能为零。
    #[error("Skill revision 必须从 1 开始")]
    InvalidRevision,
    /// Create 必须产生第一修订。
    #[error("Skill Create 操作的 revision 必须为 1")]
    CreateRevisionMustBeOne,
    /// 更新既有 Skill 的操作不能声明第一修订。
    #[error("Skill Update、Deprecate 或 Delete 操作的 revision 必须大于 1")]
    ExistingSkillRevisionRequired,
    /// Split 结果集合没有包含当前制品。
    #[error("Skill Split 的 result_skill_ids 必须包含当前制品的 skill_id")]
    SplitResultMissingCurrentSkill,
    /// 状态链为空。
    #[error("Skill 状态链不能为空")]
    MissingStatusHistory,
    /// 初始状态不是 Quarantined。
    #[error("Skill 初始状态必须固定为 Quarantined")]
    InitialStatusNotQuarantined,
    /// 状态时间不是严格递增。
    #[error("Skill 状态链 recorded_at_ms 必须严格递增且不能为零")]
    InvalidStatusTimestamp,
    /// 状态跃迁不在 V1 合法链中。
    #[error("Skill 状态不允许从 {from:?} 跃迁到 {to:?}")]
    InvalidStatusTransition {
        /// 前一状态。
        from: SkillStatusV1,
        /// 后一状态。
        to: SkillStatusV1,
    },
    /// 状态与 EvaluationReport 绑定不一致。
    #[error("Skill 状态 {status:?} 的 EvaluationReport 绑定不合法")]
    InvalidStatusEvaluationBinding {
        /// 出错状态。
        status: SkillStatusV1,
    },
    /// Deprecate/Delete 操作的状态终点错误。
    #[error("Skill {operation} 操作必须以 {expected:?} 结束，实际为 {actual:?}")]
    OperationStatusMismatch {
        /// 操作名称。
        operation: &'static str,
        /// 要求的终态。
        expected: SkillStatusV1,
        /// 实际终态。
        actual: SkillStatusV1,
    },
    /// 原生 Skill 工具事件序号不能为零。
    #[error("真实 Skill 工具事件 sequence 必须从 1 开始")]
    InvalidSkillToolEventSequence,
    /// 使用观察与 Host/Recorder 可信绑定不一致。
    #[error("Skill 使用观察与可信 Episode、Run、Genome、Skill 或原生工具事件绑定不一致")]
    UsageBindingMismatch,
    /// Skill 自报不能作为评测成功证据。
    #[error("Skill 自报成功不可信，必须绑定可信 Episode 终态与真实原生工具事件")]
    UntrustedSkillSelfReport,
    /// 观察结果与 Verifier 值矛盾。
    #[error("Skill 使用结果与可信 Verifier 判定不一致")]
    InconsistentVerifierResult,
    /// 成功观察仍包含安全失败。
    #[error("Skill 成功观察不能包含可信安全失败")]
    SuccessfulUsageHasSafetyFailure,
    /// 时间戳不能为零。
    #[error("Skill 字段 `{field}` 的 Unix 毫秒时间不能为零")]
    InvalidTimestamp {
        /// 字段名。
        field: &'static str,
    },
    /// Mutation 绑定不一致。
    #[error("Skill Artifact、Proposal、Candidate 的 Mutation 或 Parent 绑定不一致")]
    MutationBindingMismatch,
    /// Episode 证据绑定不一致。
    #[error("Skill Artifact 的来源 Episode 必须与 MutationProposal 完全一致")]
    EpisodeBindingMismatch,
    /// 集合未按协议排序或含重复项。
    #[error("Skill 集合 `{field}` 必须严格排序且不能重复")]
    UnorderedCollection {
        /// 字段名。
        field: &'static str,
    },
    /// Parent 与 Candidate 使用相同修订。
    #[error("Skill Candidate 的 Parent 与 Candidate GenomeRevision 不能相同")]
    SameGenomeRevision,
    /// Parent 与 Candidate 使用相同摘要。
    #[error("Skill Candidate 的 Parent 与 Candidate GenomeDigest 不能相同")]
    SameGenomeDigest,
    /// Skill Set 没有变化。
    #[error("Skill Candidate 的 Parent 与 Candidate Skill Set 不能相同")]
    UnchangedSkillSet,
    /// Candidate Diff 不只包含 Skill。
    #[error("M7 Candidate 的可信 Diff 必须精确包含 Skill，实际为 {0:?}")]
    InvalidCandidateSurfaces(BTreeSet<MutationSurface>),
    /// Candidate 扩大了 Parent 能力。
    #[error("Skill Candidate 的能力必须是 Parent 能力的子集")]
    CapabilityExpansion,
    /// Skill 需要 Candidate 未提供的能力。
    #[error("Skill `{skill_id}` 声明了 Candidate 未提供的能力")]
    UnavailableSkillCapability {
        /// 出错 Skill。
        skill_id: SkillId,
    },
    /// Candidate 制品摘要与 Skill Set 不一致。
    #[error("Skill `{skill_id}` 的 Candidate 制品摘要与 Genome Skill Set 不一致")]
    CandidateArtifactBindingMismatch {
        /// 出错 Skill。
        skill_id: SkillId,
    },
    /// 被弃用或逻辑删除的 Skill 仍存在于 Candidate Skill Set。
    #[error("已弃用或逻辑删除的 Skill `{skill_id}` 不能继续出现在 Candidate Skill Set")]
    RemovedSkillStillInstalled {
        /// 出错 Skill。
        skill_id: SkillId,
    },
    /// Proposal 与 Candidate 的制品 ID 集合不一致。
    #[error("Skill Proposal 与 Candidate 的制品 ID 集合不一致")]
    CandidateArtifactSetMismatch,
    /// EvaluationReport 双向绑定不一致。
    #[error("Skill Candidate 与 EvaluationReport 的 ID、Mutation 或 Genome 绑定不一致")]
    EvaluationBindingMismatch,
    /// 同一 Skill ID 出现冲突引用。
    #[error("Skill `{skill_id}` 出现不同制品或能力声明，无法去重")]
    ConflictingSkillReference {
        /// 冲突 Skill。
        skill_id: SkillId,
    },
    /// 评测观察找不到 Core 的真实原生 Skill 工具事件。
    #[error("Skill 使用观察没有对应的 Core 可信 Skill 工具事件")]
    MissingTrustedSkillToolEvent,
    /// 观察中的 Skill 不属于报告评测集合。
    #[error("Skill 使用观察指向报告 evaluated_skill_ids 之外的 Skill")]
    EvaluationSkillMismatch,
    /// Gate 结论与硬失败或观察不一致。
    #[error("Skill EvaluationReport 的 Gate 结论、失败集合与可信观察不一致")]
    InconsistentGateDecision,
    /// JSON 编解码失败。
    #[error("Skill 进化协议 JSON 处理失败：{0}")]
    Serialization(String),
    /// ArtifactDigest 构造失败。
    #[error("Skill 制品摘要无效：{0}")]
    InvalidArtifactDigest(String),
}

fn validate_skill_id(value: &str) -> Result<(), InvalidSkillId> {
    let Some(body) = value.strip_prefix("skill_") else {
        return Err(InvalidSkillId::InvalidPrefix);
    };
    let actual = body.len();
    if !(SKILL_ID_BODY_MIN_BYTES..=SKILL_ID_BODY_MAX_BYTES).contains(&actual) {
        return Err(InvalidSkillId::InvalidLength {
            min: SKILL_ID_BODY_MIN_BYTES,
            max: SKILL_ID_BODY_MAX_BYTES,
            actual,
        });
    }
    if let Some(character) = body
        .chars()
        .find(|character| !character.is_ascii_lowercase() && !character.is_ascii_digit())
    {
        return Err(InvalidSkillId::InvalidCharacter { character });
    }
    Ok(())
}

fn validate_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
) -> Result<(), InvalidSkillEvolution> {
    let trimmed = value.trim();
    if trimmed.is_empty() || value.len() > max_bytes {
        return Err(InvalidSkillEvolution::InvalidText { field, max_bytes });
    }
    Ok(())
}

fn validate_label(field: &'static str, value: &str) -> Result<(), InvalidSkillEvolution> {
    if value.trim().is_empty()
        || value.len() > MAX_LABEL_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(InvalidSkillEvolution::InvalidLabel {
            field,
            max_bytes: MAX_LABEL_BYTES,
        });
    }
    Ok(())
}

fn validate_label_set(
    field: &'static str,
    values: &BTreeSet<String>,
    allow_empty: bool,
) -> Result<(), InvalidSkillEvolution> {
    let min = usize::from(!allow_empty);
    if values.len() < min || values.len() > MAX_SET_ITEMS {
        return Err(InvalidSkillEvolution::InvalidCollectionSize {
            field,
            min,
            max: MAX_SET_ITEMS,
            actual: values.len(),
        });
    }
    for value in values {
        validate_label(field, value)?;
    }
    Ok(())
}

fn validate_status_history(
    history: &[SkillStatusTransitionV1],
) -> Result<(), InvalidSkillEvolution> {
    if history.is_empty() {
        return Err(InvalidSkillEvolution::MissingStatusHistory);
    }
    if history.len() > MAX_STATUS_TRANSITIONS {
        return Err(InvalidSkillEvolution::InvalidCollectionSize {
            field: "status_history",
            min: 1,
            max: MAX_STATUS_TRANSITIONS,
            actual: history.len(),
        });
    }
    if history[0].status != SkillStatusV1::Quarantined {
        return Err(InvalidSkillEvolution::InitialStatusNotQuarantined);
    }
    let mut previous: Option<&SkillStatusTransitionV1> = None;
    for transition in history {
        if transition.recorded_at_ms == 0
            || previous.is_some_and(|entry| entry.recorded_at_ms >= transition.recorded_at_ms)
        {
            return Err(InvalidSkillEvolution::InvalidStatusTimestamp);
        }
        let evaluation_binding_is_valid = match transition.status {
            SkillStatusV1::Evaluated | SkillStatusV1::Active => {
                transition.evaluation_report_id.is_some()
            }
            SkillStatusV1::Quarantined | SkillStatusV1::Deprecated | SkillStatusV1::Deleted => {
                transition.evaluation_report_id.is_none()
            }
        };
        if !evaluation_binding_is_valid {
            return Err(InvalidSkillEvolution::InvalidStatusEvaluationBinding {
                status: transition.status,
            });
        }
        if let Some(entry) = previous {
            let legal = matches!(
                (entry.status, transition.status),
                (SkillStatusV1::Quarantined, SkillStatusV1::Evaluated)
                    | (SkillStatusV1::Quarantined, SkillStatusV1::Deleted)
                    | (SkillStatusV1::Evaluated, SkillStatusV1::Active)
                    | (SkillStatusV1::Evaluated, SkillStatusV1::Quarantined)
                    | (SkillStatusV1::Evaluated, SkillStatusV1::Deleted)
                    | (SkillStatusV1::Active, SkillStatusV1::Deprecated)
                    | (SkillStatusV1::Active, SkillStatusV1::Deleted)
                    | (SkillStatusV1::Deprecated, SkillStatusV1::Deleted)
            );
            if !legal {
                return Err(InvalidSkillEvolution::InvalidStatusTransition {
                    from: entry.status,
                    to: transition.status,
                });
            }
        }
        previous = Some(transition);
    }
    Ok(())
}

fn ensure_strict_skill_artifact_order(
    artifacts: &[SkillArtifactV1],
) -> Result<(), InvalidSkillEvolution> {
    let sorted = artifacts
        .windows(2)
        .all(|pair| pair[0].skill_id < pair[1].skill_id);
    if !sorted {
        return Err(InvalidSkillEvolution::UnorderedCollection {
            field: "proposed_artifacts",
        });
    }
    Ok(())
}

fn ensure_strict_skill_ref_order(
    field: &'static str,
    skills: &[SkillGenomeRefV1],
) -> Result<(), InvalidSkillEvolution> {
    if !skills
        .windows(2)
        .all(|pair| pair[0].skill_id < pair[1].skill_id)
    {
        return Err(InvalidSkillEvolution::UnorderedCollection { field });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_id(suffix: &str) -> SkillId {
        SkillId::new(format!("skill_{suffix:0<8}")).expect("测试 Skill ID 应合法")
    }

    fn digest(character: char) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要应合法")
    }

    fn genome_digest(character: char) -> GenomeDigest {
        GenomeDigest::from_sha256_hex(character.to_string().repeat(64))
            .expect("测试 Genome 摘要应合法")
    }

    fn status(
        status: SkillStatusV1,
        recorded_at_ms: u64,
        report: Option<&EvaluationReportId>,
    ) -> SkillStatusTransitionV1 {
        SkillStatusTransitionV1 {
            status,
            recorded_at_ms,
            evaluation_report_id: report.cloned(),
        }
    }

    fn active_history() -> Vec<SkillStatusTransitionV1> {
        let report = EvaluationReportId::generate();
        vec![
            status(SkillStatusV1::Quarantined, 1, None),
            status(SkillStatusV1::Evaluated, 2, Some(&report)),
            status(SkillStatusV1::Active, 3, Some(&report)),
        ]
    }

    fn artifact(
        skill_id: SkillId,
        revision: u32,
        operation: SkillOperationV1,
        final_history: Vec<SkillStatusTransitionV1>,
        mutation_id: MutationId,
        episodes: BTreeSet<EpisodeId>,
    ) -> SkillArtifactV1 {
        SkillArtifactV1 {
            schema_version: SKILL_ARTIFACT_SCHEMA_VERSION,
            skill_id,
            revision,
            operation,
            name: "错误归因".into(),
            description: "根据可信事件定位失败来源".into(),
            instructions: "仅使用脱敏事件和可信 Verifier 结论。".into(),
            trigger_policy: SkillTriggerPolicyV1 {
                mode: SkillTriggerModeV1::Hybrid,
                explicit_triggers: BTreeSet::from(["诊断失败".into()]),
                semantic_intents: BTreeSet::from(["failure_diagnosis".into()]),
                minimum_confidence_bps: Some(9_000),
            },
            required_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
            source_episode_ids: episodes,
            mutation_id,
            status_history: final_history,
        }
    }

    fn plugin_binding(skill_id: SkillId) -> TrustedSkillUsageBindingV1 {
        TrustedSkillUsageBindingV1 {
            episode_id: EpisodeId::generate(),
            run_id: RunId::generate(),
            genome_revision_id: GenomeRevisionId::generate(),
            skill_id,
            skill_artifact_digest: digest('a'),
            tool_event: TrustedSkillToolEventRefV1 {
                event_id: EventId::generate(),
                sequence: 4,
                runtime_origin: "native".into(),
                tool_name: "skill_read".into(),
                payload_digest: digest('b'),
            },
        }
    }

    fn observation(binding: TrustedSkillUsageBindingV1) -> SkillUsageObservationV1 {
        SkillUsageObservationV1 {
            schema_version: SKILL_USAGE_OBSERVATION_SCHEMA_VERSION,
            binding,
            outcome_revision_id: OutcomeRevisionId::generate(),
            evidence_source: SkillUsageEvidenceSourceV1::TrustedEpisodeOutcome,
            result: SkillUsageResultV1::VerifiedSuccess,
            verifier_passed: Some(true),
            safety_failures: 0,
            observed_at_ms: 10,
        }
    }

    fn skill_ref(skill_id: SkillId, character: char) -> SkillGenomeRefV1 {
        SkillGenomeRefV1 {
            skill_id,
            artifact_digest: digest(character),
            required_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
        }
    }

    fn candidate() -> SkillCandidateV1 {
        let old_id = skill_id("oldskill");
        let new_id = skill_id("newskill");
        SkillCandidateV1 {
            schema_version: SKILL_CANDIDATE_SCHEMA_VERSION,
            candidate_id: CandidateId::generate(),
            cycle_id: EvolutionCycleId::generate(),
            mutation_id: MutationId::generate(),
            parent_revision_id: GenomeRevisionId::generate(),
            candidate_revision_id: GenomeRevisionId::generate(),
            parent_genome_digest: genome_digest('c'),
            candidate_genome_digest: genome_digest('d'),
            parent_skill_set: vec![skill_ref(old_id, 'e')],
            candidate_skill_set: vec![skill_ref(new_id.clone(), 'f')],
            parent_capabilities: BTreeSet::from([
                "episode.read_redacted".into(),
                "tool.execute".into(),
            ]),
            candidate_capabilities: BTreeSet::from(["episode.read_redacted".into()]),
            candidate_artifact_digests: BTreeMap::from([(new_id, digest('f'))]),
            changed_surfaces: BTreeSet::from([MutationSurface::Skill]),
            evaluation_report_id: None,
            created_at_ms: 10,
        }
    }

    /// SkillId 必须在构造与 serde 反序列化两条路径上执行相同校验。
    #[test]
    fn skill_id_is_strongly_validated() {
        assert!(SkillId::new("skill_abcdefgh").is_ok());
        assert!(matches!(
            SkillId::new("other_abcdefgh"),
            Err(InvalidSkillId::InvalidPrefix)
        ));
        assert!(serde_json::from_str::<SkillId>("\"skill_BADVALUE\"").is_err());
    }

    /// Manual 默认策略允许两个触发集合为空，其他模式仍必须提供各自需要的字段。
    #[test]
    fn trigger_policy_allows_manual_default_and_rejects_missing_mode_fields() {
        SkillTriggerPolicyV1::default()
            .validate()
            .expect("Manual 默认策略应合法");
        for mode in [
            SkillTriggerModeV1::Explicit,
            SkillTriggerModeV1::Semantic,
            SkillTriggerModeV1::Hybrid,
        ] {
            let policy = SkillTriggerPolicyV1 {
                mode,
                explicit_triggers: BTreeSet::new(),
                semantic_intents: BTreeSet::new(),
                minimum_confidence_bps: None,
            };
            assert_eq!(
                policy.validate(),
                Err(InvalidSkillEvolution::InvalidTriggerPolicy)
            );
        }
    }

    /// 六类操作都能表达完整来源，并通过各自的边界校验。
    #[test]
    fn all_six_operations_are_validated() {
        let mutation = MutationId::generate();
        let episodes = BTreeSet::from([EpisodeId::generate()]);
        let first = skill_id("firstone");
        let second = skill_id("secondxx");
        let merged = skill_id("mergedxx");
        let split_a = skill_id("splitaaa");
        let split_b = skill_id("splitbbb");
        let report = EvaluationReportId::generate();

        let cases = vec![
            artifact(
                first.clone(),
                1,
                SkillOperationV1::Create,
                vec![status(SkillStatusV1::Quarantined, 1, None)],
                mutation.clone(),
                episodes.clone(),
            ),
            artifact(
                first.clone(),
                2,
                SkillOperationV1::Update {
                    previous_artifact_digest: digest('1'),
                },
                active_history(),
                mutation.clone(),
                episodes.clone(),
            ),
            artifact(
                merged,
                1,
                SkillOperationV1::Merge {
                    source_artifacts: BTreeMap::from([
                        (first.clone(), digest('2')),
                        (second, digest('3')),
                    ]),
                },
                vec![status(SkillStatusV1::Quarantined, 1, None)],
                mutation.clone(),
                episodes.clone(),
            ),
            artifact(
                split_a.clone(),
                1,
                SkillOperationV1::Split {
                    source_skill_id: first.clone(),
                    source_artifact_digest: digest('4'),
                    result_skill_ids: BTreeSet::from([split_a, split_b]),
                },
                vec![status(SkillStatusV1::Quarantined, 1, None)],
                mutation.clone(),
                episodes.clone(),
            ),
            artifact(
                first.clone(),
                3,
                SkillOperationV1::Deprecate {
                    previous_artifact_digest: digest('5'),
                },
                vec![
                    status(SkillStatusV1::Quarantined, 1, None),
                    status(SkillStatusV1::Evaluated, 2, Some(&report)),
                    status(SkillStatusV1::Active, 3, Some(&report)),
                    status(SkillStatusV1::Deprecated, 4, None),
                ],
                mutation.clone(),
                episodes.clone(),
            ),
            artifact(
                first,
                4,
                SkillOperationV1::Delete {
                    previous_artifact_digest: digest('6'),
                    deletion_mode: SkillDeletionModeV1::LogicalTombstone,
                },
                vec![
                    status(SkillStatusV1::Quarantined, 1, None),
                    status(SkillStatusV1::Deleted, 2, None),
                ],
                mutation,
                episodes,
            ),
        ];

        for artifact in cases {
            artifact.validate().expect("六类合法操作都应通过校验");
        }
    }

    /// 状态链必须从 Quarantined 开始，并拒绝跳过评测直接 Active。
    #[test]
    fn status_history_rejects_invalid_initial_and_transition() {
        let mutation = MutationId::generate();
        let episodes = BTreeSet::from([EpisodeId::generate()]);
        let report = EvaluationReportId::generate();
        let mut invalid_initial = artifact(
            skill_id("initialx"),
            1,
            SkillOperationV1::Create,
            vec![status(SkillStatusV1::Active, 1, Some(&report))],
            mutation.clone(),
            episodes.clone(),
        );
        assert_eq!(
            invalid_initial.validate(),
            Err(InvalidSkillEvolution::InitialStatusNotQuarantined)
        );

        invalid_initial.status_history = vec![
            status(SkillStatusV1::Quarantined, 1, None),
            status(SkillStatusV1::Active, 2, Some(&report)),
        ];
        assert!(matches!(
            invalid_initial.validate(),
            Err(InvalidSkillEvolution::InvalidStatusTransition {
                from: SkillStatusV1::Quarantined,
                to: SkillStatusV1::Active,
            })
        ));
    }

    /// Delete 的 JSON 只能选择逻辑墓碑，不能通过协议请求物理删除 CAS。
    #[test]
    fn delete_forbids_physical_deletion_semantics() {
        let value = serde_json::json!({
            "type": "delete",
            "previous_artifact_digest": "a".repeat(64),
            "deletion_mode": "physical"
        });
        assert!(serde_json::from_value::<SkillOperationV1>(value).is_err());
    }

    /// 使用观察必须逐字段匹配可信 Episode/Run/Genome/Skill/原生工具事件并拒绝自报成功。
    #[test]
    fn usage_observation_rejects_misbinding_and_self_report() {
        let binding = plugin_binding(skill_id("observee"));
        let mut observed = observation(binding.clone());
        let mut wrong = binding.clone();
        wrong.run_id = RunId::generate();
        assert_eq!(
            observed.validate(&wrong),
            Err(InvalidSkillEvolution::UsageBindingMismatch)
        );

        observed.evidence_source = SkillUsageEvidenceSourceV1::SkillSelfReported;
        assert_eq!(
            observed.validate(&binding),
            Err(InvalidSkillEvolution::UntrustedSkillSelfReport)
        );
    }

    /// Candidate 能力必须是 Parent 子集，且每个 Skill 只能要求 Candidate 已有能力。
    #[test]
    fn candidate_rejects_capability_expansion() {
        let mut candidate = candidate();
        candidate
            .candidate_capabilities
            .insert("process_exec".into());
        assert_eq!(
            candidate.validate(),
            Err(InvalidSkillEvolution::CapabilityExpansion)
        );
    }

    /// 可信 Diff 必须精确为 Skill，任何额外表面都失败关闭。
    #[test]
    fn candidate_rejects_non_skill_diff() {
        let mut candidate = candidate();
        candidate
            .changed_surfaces
            .insert(MutationSurface::ProtectedPrompt);
        assert!(matches!(
            candidate.validate(),
            Err(InvalidSkillEvolution::InvalidCandidateSurfaces(_))
        ));
    }

    /// Skill Set 规范化按 ID 排序、去重，并对相同 ID 的冲突内容失败关闭。
    #[test]
    fn skill_set_normalization_is_sorted_and_deduplicated() {
        let first = skill_ref(skill_id("aaaaaaa1"), '1');
        let second = skill_ref(skill_id("bbbbbbb2"), '2');
        let normalized = SkillCandidateV1::normalize_skill_set(vec![
            second.clone(),
            first.clone(),
            second.clone(),
        ])
        .expect("相同引用应可去重");
        assert_eq!(normalized, vec![first, second.clone()]);

        let mut conflict = second;
        conflict.artifact_digest = digest('3');
        assert!(matches!(
            SkillCandidateV1::normalize_skill_set(vec![
                skill_ref(skill_id("bbbbbbb2"), '2'),
                conflict,
            ]),
            Err(InvalidSkillEvolution::ConflictingSkillReference { .. })
        ));
    }

    /// BTree 集合与规范 JSON 在重复编码后保持逐字节幂等，未知加法字段不破坏读取。
    #[test]
    fn artifact_json_is_sorted_idempotent_and_additive_compatible() {
        let mutation = MutationId::generate();
        let episodes = BTreeSet::from([EpisodeId::generate()]);
        let mut artifact = artifact(
            skill_id("stablejs"),
            1,
            SkillOperationV1::Create,
            vec![status(SkillStatusV1::Quarantined, 1, None)],
            mutation,
            episodes,
        );
        artifact.required_capabilities =
            BTreeSet::from(["z.last".into(), "a.first".into(), "z.last".into()]);
        let bytes = artifact.canonical_bytes().expect("制品应规范序列化");
        let mut value: serde_json::Value =
            serde_json::from_slice(&bytes).expect("规范 JSON 应可读取");
        value["future_optional_field"] = serde_json::json!({"enabled": true});
        let decoded = SkillArtifactV1::from_json_slice(
            &serde_json::to_vec(&value).expect("加法 JSON 应可编码"),
        )
        .expect("未知加法字段应被忽略");
        assert_eq!(decoded, artifact);
        assert_eq!(decoded.canonical_bytes().expect("复读后应可编码"), bytes);
        assert!(String::from_utf8(bytes)
            .expect("JSON 应为 UTF-8")
            .contains("[\"a.first\",\"z.last\"]"));
    }

    /// 评测报告必须复核真实原生工具事件，且 Pass 只接受可信成功观察。
    #[test]
    fn evaluation_report_binds_trusted_usage() {
        let skill_id = skill_id("evaluate");
        let binding = plugin_binding(skill_id.clone());
        let observation = observation(binding.clone());
        let report = SkillEvaluationReportV1 {
            schema_version: SKILL_EVALUATION_REPORT_SCHEMA_VERSION,
            report_id: EvaluationReportId::generate(),
            mutation_id: MutationId::generate(),
            candidate_id: CandidateId::generate(),
            parent_revision_id: GenomeRevisionId::generate(),
            candidate_revision_id: GenomeRevisionId::generate(),
            parent_genome_digest: genome_digest('8'),
            candidate_genome_digest: genome_digest('9'),
            evaluated_skill_ids: BTreeSet::from([skill_id]),
            observations: vec![observation],
            decision: GateDecision::Pass,
            failures: BTreeSet::new(),
            generated_at_ms: 20,
        };
        let trusted = BTreeMap::from([(binding.tool_event.event_id.clone(), binding)]);
        report
            .validate(&trusted)
            .expect("真实原生工具事件绑定的可信成功报告应合法");

        assert_eq!(
            report.validate(&BTreeMap::new()),
            Err(InvalidSkillEvolution::MissingTrustedSkillToolEvent)
        );
    }
}

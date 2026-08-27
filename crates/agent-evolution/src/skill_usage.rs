//! 从真实 Episode 原生工具终态构造 M7 Skill 使用绑定。
//!
//! `skill_read` 结果正文不构成成功证据；本模块重新加载只追加 Episode、Core 注入的
//! `runtime_origin=native` 与 Genome 固定的 Skill CAS，随后才生成 Evaluator 可消费的绑定。

use crate::skill_repository::{SkillArtifactRepository, SkillRepositoryError};
use crate::{
    load_episode_evidence, ArtifactStore, ArtifactStoreError, EpisodeEvidenceError, EpisodeStore,
    FileArtifactStore,
};
use agent_evolution_protocol::{
    ArtifactDigest, EpisodeId, EventId, GenomeRevision, SkillArtifactV1, SkillId, SkillStatusV1,
    TrustedSkillToolEventRefV1, TrustedSkillUsageBindingV1,
};
use serde::Serialize;
use std::collections::BTreeMap;
use thiserror::Error;

/// Core 记录的原生 Skill 读取工具名。
pub const NATIVE_SKILL_READ_TOOL: &str = "skill_read";
/// `details.skill_usage` 的结构版本。
pub const NATIVE_SKILL_USAGE_SCHEMA_VERSION: u32 = 1;
/// 保存脱敏 Skill 使用事件 payload 的 CAS 媒体类型。
pub const SKILL_USAGE_EVENT_MEDIA_TYPE: &str =
    "application/vnd.ascnet.lucia.skill-usage-event.v2+json";

/// 从可信 Episode 中提取全部真实原生 Skill 读取终态。
///
/// 调用方必须传入 Resolver 已校验的同一 Genome 修订。
/// 方法从 Genome 引用的真实 CAS 重新加载每份 Skill 制品，只允许终态为 `Active` 的制品；
/// 随后要求事件由 Core 固定为原生 `skill_read`、Skill ID 和摘要精确命中 Genome，并把
/// 已脱敏 payload 写入 CAS。普通文本和其他工具事件会被忽略。
///
/// # Errors
///
/// Episode 或 Genome 绑定不一致、Genome Skill 引用无效或非 Active、工具字段缺失或错绑、
/// 事件序号溢出、payload 无法规范编码，或 Artifact CAS 读写失败时返回
/// [`SkillUsageBindingError`]。任一目标事件无效时整体失败，不返回部分绑定。
pub async fn collect_trusted_skill_usage_bindings(
    episodes: &dyn EpisodeStore,
    artifacts: &FileArtifactStore,
    episode_id: &EpisodeId,
    genome: &GenomeRevision,
) -> Result<BTreeMap<EventId, TrustedSkillUsageBindingV1>, SkillUsageBindingError> {
    collect_trusted_skill_usage_bindings_for_stage(
        episodes,
        artifacts,
        episode_id,
        genome,
        SkillBindingStage::Serve,
    )
    .await
}

/// 从受信 Evaluation 运行的 Episode 中提取 Quarantined Candidate 使用事件。
///
/// 该入口只能由评测控制面调用，不接受 Guest 或模型声明的 stage。它保持 Episode 绑定的
/// 原始 Candidate Genome Revision 不变，仅把可加载状态收窄为 `Quarantined`、`Evaluated`
/// 或 `Active`；Deprecated/Deleted 仍失败关闭。事件来源、Skill ID、CAS 摘要和 payload 的
/// 校验与 Serve 入口完全相同。
///
/// # Errors
///
/// Episode、Genome、工具来源、Skill 状态或 CAS 绑定不可信时返回
/// [`SkillUsageBindingError`]，不会返回部分绑定。
pub async fn collect_trusted_skill_evaluation_bindings(
    episodes: &dyn EpisodeStore,
    artifacts: &FileArtifactStore,
    episode_id: &EpisodeId,
    candidate_genome: &GenomeRevision,
) -> Result<BTreeMap<EventId, TrustedSkillUsageBindingV1>, SkillUsageBindingError> {
    collect_trusted_skill_usage_bindings_for_stage(
        episodes,
        artifacts,
        episode_id,
        candidate_genome,
        SkillBindingStage::Evaluation,
    )
    .await
}

/// 可信控制面选择的 Skill 使用绑定阶段；该值不从 Guest payload 反序列化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SkillBindingStage {
    /// 新运行只能使用 Active Skill。
    Serve,
    /// 独立评测可执行隔离候选，但不能执行已弃用或删除 Skill。
    Evaluation,
}

async fn collect_trusted_skill_usage_bindings_for_stage(
    episodes: &dyn EpisodeStore,
    artifacts: &FileArtifactStore,
    episode_id: &EpisodeId,
    genome: &GenomeRevision,
    stage: SkillBindingStage,
) -> Result<BTreeMap<EventId, TrustedSkillUsageBindingV1>, SkillUsageBindingError> {
    genome
        .validate()
        .map_err(|error| SkillUsageBindingError::InvalidGenome(error.to_string()))?;
    let evidence = load_episode_evidence(episodes, artifacts, episode_id).await?;
    if evidence.episode.genome_revision_id != genome.revision_id {
        return Err(SkillUsageBindingError::GenomeRevisionMismatch);
    }

    let skill_set = load_skill_set(artifacts, genome, stage).await?;
    let mut bindings = BTreeMap::new();
    for (index, event) in evidence.events.iter().enumerate() {
        if event.kind != "tool_finished"
            || event
                .payload
                .get("name")
                .and_then(serde_json::Value::as_str)
                != Some(NATIVE_SKILL_READ_TOOL)
        {
            continue;
        }
        if event
            .payload
            .get("runtime_origin")
            .and_then(serde_json::Value::as_str)
            != Some("native")
        {
            return Err(SkillUsageBindingError::UntrustedRuntimeOrigin);
        }
        if event
            .payload
            .get("is_error")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let data = event
            .payload
            .pointer("/details/skill_usage")
            .ok_or(SkillUsageBindingError::MissingSkillUsageDetails)?;
        let schema_version = data
            .get("schema_version")
            .and_then(serde_json::Value::as_u64);
        if schema_version != Some(u64::from(NATIVE_SKILL_USAGE_SCHEMA_VERSION)) {
            return Err(SkillUsageBindingError::UnsupportedEventSchema {
                found: schema_version,
            });
        }
        let skill_id = SkillId::new(required_string(data, "skill_id")?)
            .map_err(|error| SkillUsageBindingError::InvalidSkillId(error.to_string()))?;
        let artifact_digest = ArtifactDigest::new(required_string(data, "artifact_digest")?)
            .map_err(|error| SkillUsageBindingError::InvalidArtifactDigest(error.to_string()))?;
        let call_id = required_string(&event.payload, "call_id")?;
        if call_id.trim().is_empty() {
            return Err(SkillUsageBindingError::EmptyCallId);
        }
        if required_string(data, "genome_revision_id")? != genome.revision_id.to_string().as_str() {
            return Err(SkillUsageBindingError::UsageGenomeRevisionMismatch);
        }
        if required_string(data, "genome_digest")? != genome.digest.to_string().as_str() {
            return Err(SkillUsageBindingError::UsageGenomeDigestMismatch);
        }
        if skill_set.get(&skill_id) != Some(&artifact_digest) {
            return Err(SkillUsageBindingError::SkillNotInGenome {
                skill_id,
                artifact_digest,
            });
        }

        let event_id = EventId::new(event.event_id.clone())
            .map_err(|error| SkillUsageBindingError::InvalidEventId(error.to_string()))?;
        let sequence = u64::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(SkillUsageBindingError::EventSequenceOverflow)?;
        let payload_bytes = canonical_json_bytes(&event.payload)?;
        let payload = artifacts
            .put(SKILL_USAGE_EVENT_MEDIA_TYPE, &payload_bytes)
            .await?;
        let binding = TrustedSkillUsageBindingV1 {
            episode_id: evidence.episode.episode_id.clone(),
            run_id: evidence.episode.run_id.clone(),
            genome_revision_id: evidence.episode.genome_revision_id.clone(),
            skill_id,
            skill_artifact_digest: artifact_digest,
            tool_event: TrustedSkillToolEventRefV1 {
                event_id: event_id.clone(),
                sequence,
                runtime_origin: "native".to_string(),
                tool_name: NATIVE_SKILL_READ_TOOL.to_string(),
                payload_digest: payload.digest,
            },
        };
        binding
            .validate()
            .map_err(|error| SkillUsageBindingError::InvalidBinding(error.to_string()))?;
        bindings.insert(event_id, binding);
    }
    Ok(bindings)
}

/// 从 Genome Skill 引用加载并复核指定可信阶段允许的 Skill Set。
async fn load_skill_set(
    artifacts: &FileArtifactStore,
    genome: &GenomeRevision,
    stage: SkillBindingStage,
) -> Result<BTreeMap<SkillId, ArtifactDigest>, SkillUsageBindingError> {
    let repository = SkillArtifactRepository::new(artifacts);
    let mut active = BTreeMap::new();
    for reference in &genome.genome.skills {
        let skill_id = SkillId::new(reference.id.clone())
            .map_err(|error| SkillUsageBindingError::InvalidSkillId(error.to_string()))?;
        let artifact = repository.get(&reference.content).await?;
        verify_skill_reference(&skill_id, &reference.content, &artifact, stage)?;
        active.insert(skill_id, reference.content.clone());
    }
    Ok(active)
}

/// 确认 Genome 引用与 CAS 制品 ID、摘要和可信阶段允许的状态一致。
fn verify_skill_reference(
    expected_id: &SkillId,
    expected_digest: &ArtifactDigest,
    artifact: &SkillArtifactV1,
    stage: SkillBindingStage,
) -> Result<(), SkillUsageBindingError> {
    if &artifact.skill_id != expected_id || artifact.digest()? != *expected_digest {
        return Err(SkillUsageBindingError::SkillArtifactBindingMismatch {
            skill_id: expected_id.clone(),
        });
    }
    let status = artifact.status_history.last().map(|entry| entry.status);
    let loadable = match stage {
        SkillBindingStage::Serve => status == Some(SkillStatusV1::Active),
        SkillBindingStage::Evaluation => matches!(
            status,
            Some(SkillStatusV1::Quarantined | SkillStatusV1::Evaluated | SkillStatusV1::Active)
        ),
    };
    if !loadable {
        return Err(SkillUsageBindingError::SkillNotLoadable {
            skill_id: expected_id.clone(),
            status,
            stage: match stage {
                SkillBindingStage::Serve => "serve",
                SkillBindingStage::Evaluation => "evaluation",
            },
        });
    }
    Ok(())
}

/// 读取事件数据中的非空字符串字段。
fn required_string<'a>(
    value: &'a serde_json::Value,
    field: &'static str,
) -> Result<&'a str, SkillUsageBindingError> {
    value
        .get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or(SkillUsageBindingError::InvalidEventField { field })
}

/// 生成稳定 JSON 字节，供脱敏事件 payload 写入内容寻址存储。
fn canonical_json_bytes(value: &impl Serialize) -> Result<Vec<u8>, SkillUsageBindingError> {
    serde_json::to_vec(value).map_err(SkillUsageBindingError::SerializePayload)
}

/// 可信 Skill 使用绑定构造失败。
#[derive(Debug, Error)]
pub enum SkillUsageBindingError {
    /// Genome 修订自身不合法。
    #[error("Genome 修订无效：{0}")]
    InvalidGenome(String),
    /// Episode 固定的 Genome 修订与调用方提供值不同。
    #[error("Episode 与 Skill 使用绑定的 Genome 修订不一致")]
    GenomeRevisionMismatch,
    /// Skill ID 不符合强类型协议。
    #[error("Skill ID 无效：{0}")]
    InvalidSkillId(String),
    /// Skill Artifact 摘要不符合强类型协议。
    #[error("Skill Artifact 摘要无效：{0}")]
    InvalidArtifactDigest(String),
    /// Genome Skill 引用没有绑定同一份 CAS 制品。
    #[error("Genome Skill `{skill_id}` 与 CAS 制品绑定不一致")]
    SkillArtifactBindingMismatch {
        /// 出错的 Skill ID。
        skill_id: SkillId,
    },
    /// Genome Skill 状态不能进入指定可信运行阶段。
    #[error("Genome Skill `{skill_id}` 的状态 {status:?} 不能进入 {stage} 绑定")]
    SkillNotLoadable {
        /// 出错的 Skill ID。
        skill_id: SkillId,
        /// CAS 制品的实际终态。
        status: Option<SkillStatusV1>,
        /// 可信控制面选择的阶段。
        stage: &'static str,
    },
    /// 目标工具终态不是由 Core 注入的原生执行来源。
    #[error("Skill 使用事件缺少 Core 固定的原生工具来源")]
    UntrustedRuntimeOrigin,
    /// 目标工具终态缺少受控 Skill 使用细节。
    #[error("Skill 工具终态缺少 details.skill_usage")]
    MissingSkillUsageDetails,
    /// 目标事件结构版本不受支持。
    #[error("Skill 使用事件 schema_version 不受支持：{found:?}")]
    UnsupportedEventSchema {
        /// 事件携带的版本；缺失或非整数时为 `None`。
        found: Option<u64>,
    },
    /// 目标事件的必填字段缺失、为空或类型错误。
    #[error("Skill 使用事件字段 `{field}` 必须是非空字符串")]
    InvalidEventField {
        /// 出错字段名。
        field: &'static str,
    },
    /// 工具调用 ID 为空。
    #[error("Skill 使用事件 call_id 不能为空")]
    EmptyCallId,
    /// 工具细节中的 Genome Revision 与 Episode 固定修订不同。
    #[error("Skill 工具终态声明的 Genome Revision 与 Episode 不一致")]
    UsageGenomeRevisionMismatch,
    /// 工具细节中的 Genome 摘要与真实修订不同。
    #[error("Skill 工具终态声明的 Genome 摘要与真实修订不一致")]
    UsageGenomeDigestMismatch,
    /// 事件引用的 Skill 不在该 Episode 的 Genome 中。
    #[error("Skill `{skill_id}` 的制品 `{artifact_digest}` 不在 Episode Genome 中")]
    SkillNotInGenome {
        /// 事件声明的 Skill ID。
        skill_id: SkillId,
        /// 事件声明的制品摘要。
        artifact_digest: ArtifactDigest,
    },
    /// Episode 事件 ID 无效。
    #[error("Skill 使用事件 ID 无效：{0}")]
    InvalidEventId(String),
    /// 事件位置无法表示为协议序号。
    #[error("Skill 使用事件序号溢出")]
    EventSequenceOverflow,
    /// 最终绑定违反共享协议。
    #[error("Skill 使用绑定无效：{0}")]
    InvalidBinding(String),
    /// Episode 或其 CAS 证据读取、校验失败。
    #[error(transparent)]
    EpisodeEvidence(#[from] EpisodeEvidenceError),
    /// Skill Artifact CAS 读取或校验失败。
    #[error(transparent)]
    SkillRepository(#[from] SkillRepositoryError),
    /// 脱敏事件 payload 无法序列化。
    #[error("序列化 Skill 使用事件 payload 失败：{0}")]
    SerializePayload(serde_json::Error),
    /// 事件 payload 写入 Artifact CAS 失败。
    #[error(transparent)]
    ArtifactStore(#[from] ArtifactStoreError),
    /// Skill Artifact 局部协议复核失败。
    #[error("Skill Artifact 无效：{0}")]
    InvalidSkillArtifact(#[from] agent_evolution_protocol::InvalidSkillEvolution),
}

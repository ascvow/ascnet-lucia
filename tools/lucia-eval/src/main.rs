//! Lucia 独立受信离线 Evaluator。
//!
//! 单次请求只能声明 Parent/Candidate 身份与并发前置条件。Dataset、Verifier、Commit Policy、
//! Workspace 和 Store 根均来自受信环境配置，禁止由 Candidate 或 Mutator 随请求指定。

use agent_evaluation::{
    ComparativeRunner, ComparativeRunnerConfig, EvaluationReportBuilder, EvaluationReportMetadata,
    EvaluationSubject, ReleaseController, ReleaseReceipt, TrustedDatasetStore,
    TrustedEvaluationArchive,
};
use agent_evolution::{
    ArtifactStore, FileArtifactStore, FileGenomeResolver, GenomeResolver, GenomeSelector,
};
use agent_evolution_protocol::{
    ArtifactDigest, DatasetVersionId, EvaluationEnvironment, EvaluationReportId,
    EvolutionLifecycle, GateDecision, GenomeRevision, GenomeRevisionId,
};
use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    env,
    io::{self, Read},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

/// stdin 请求允许的最大字节数，防止无界 JSON 占用内存。
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// 当前 `lucia-eval` 请求 schema。
const EVALUATION_REQUEST_SCHEMA_VERSION: u32 = 1;
/// 当前 `lucia-eval` 回执 schema。
const EVALUATION_RECEIPT_SCHEMA_VERSION: u32 = 1;

/// 独立 Evaluator 不接受任何路径或 Policy 命令行参数。
#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "从 stdin 接收受限比较请求并输出脱敏 Evaluation Receipt"
)]
struct Args {
    /// 受信控制面动作；所有动作都只从 stdin 接收版本化 JSON。
    #[command(subcommand)]
    command: Command,
}

/// 独立 Evaluator 支持的受限控制面动作。
#[derive(Debug, Subcommand)]
enum Command {
    /// 运行 Parent/Candidate 离线比较并提交正式 Report Seal。
    Evaluate,
    /// 使用正式 EvaluationReport 晋升 Candidate。
    Promote,
    /// 把指定 Promotion 原子回滚到 Parent。
    Rollback,
}

/// Candidate 可提交的最小比较请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluationRequestV1 {
    /// 请求 schema 版本。
    schema_version: u32,
    /// 调用方生成的稳定幂等标识；不得包含路径或用户内容。
    request_id: String,
    /// 当前 Stable Parent 修订。
    parent_revision_id: GenomeRevisionId,
    /// 待评测 Candidate 修订。
    candidate_revision_id: GenomeRevisionId,
    /// 受信 Stable lineage。
    lineage: String,
    /// 调用方观察到的 Parent 代数，用作并发前置条件。
    expected_parent_generation: u64,
    /// 调用方期望使用的受信 Dataset 版本。
    expected_dataset_version: DatasetVersionId,
}

/// Promotion 只接收正式 Report 与幂等 Release ID。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PromotionRequestV1 {
    /// 请求 schema 版本。
    schema_version: u32,
    /// 已完成 Seal 的 EvaluationReport。
    report_id: EvaluationReportId,
    /// 本次 Promotion 的幂等标识。
    release_id: agent_evolution_protocol::ReleaseId,
}

/// Rollback 只接收被撤销 Release 与本次回滚的幂等 ID。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RollbackRequestV1 {
    /// 请求 schema 版本。
    schema_version: u32,
    /// 被撤销的 Promotion Release。
    release_id: agent_evolution_protocol::ReleaseId,
    /// 本次 Rollback 自身的幂等 Release ID。
    rollback_release_id: agent_evolution_protocol::ReleaseId,
}

impl EvaluationRequestV1 {
    /// 校验不需要访问受信 Store 的请求结构边界。
    fn validate(&self) -> Result<()> {
        if self.schema_version != EVALUATION_REQUEST_SCHEMA_VERSION {
            return Err(anyhow!("unsupported_request_schema"));
        }
        if self.request_id.is_empty()
            || self.request_id.len() > 128
            || !self
                .request_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(anyhow!("invalid_request_id"));
        }
        if self.lineage.is_empty()
            || self.lineage.len() > 128
            || self.lineage.starts_with('/')
            || self.lineage.ends_with('/')
            || self
                .lineage
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || !self.lineage.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/')
            })
        {
            return Err(anyhow!("invalid_lineage"));
        }
        if self.parent_revision_id == self.candidate_revision_id {
            return Err(anyhow!("same_revision"));
        }
        Ok(())
    }
}

/// 只从受信进程环境读取的 Evaluator 配置。
#[derive(Debug, Clone)]
struct TrustedConfig {
    evolution_root: PathBuf,
    dataset_root: PathBuf,
    fixture_workspace_root: PathBuf,
    archive_root: PathBuf,
    expected_manifest_digest: ArtifactDigest,
    kernel_ref: String,
}

/// Promotion/Rollback 需要的最小受信根配置。
#[derive(Debug, Clone)]
struct TrustedReleaseConfig {
    evolution_root: PathBuf,
    archive_root: PathBuf,
}

impl TrustedReleaseConfig {
    /// 只从受信进程环境加载 Registry 与 Evaluation Archive 根。
    fn from_env() -> Result<Self> {
        Ok(Self {
            evolution_root: required_path("LUCIA_EVAL_EVOLUTION_ROOT")?,
            archive_root: required_path("LUCIA_EVAL_ARCHIVE_ROOT")?,
        })
    }
}

impl TrustedConfig {
    /// 加载固定环境变量；变量名不会接受请求侧覆盖。
    fn from_env() -> Result<Self> {
        let expected_manifest_digest =
            ArtifactDigest::new(required_env("LUCIA_EVAL_DATASET_MANIFEST_DIGEST")?)
                .map_err(|_| anyhow!("invalid_manifest_digest"))?;
        let kernel_ref = required_env("LUCIA_EVAL_KERNEL_REF")?;
        if kernel_ref.is_empty() || kernel_ref.len() > 256 {
            return Err(anyhow!("invalid_kernel_ref"));
        }
        Ok(Self {
            evolution_root: required_path("LUCIA_EVAL_EVOLUTION_ROOT")?,
            dataset_root: required_path("LUCIA_EVAL_DATASET_ROOT")?,
            fixture_workspace_root: required_path("LUCIA_EVAL_WORKSPACE_ROOT")?,
            archive_root: required_path("LUCIA_EVAL_ARCHIVE_ROOT")?,
            expected_manifest_digest,
            kernel_ref,
        })
    }
}

/// 成功提交正式报告后的脱敏回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct EvaluationReceiptV1 {
    schema_version: u32,
    request_id: String,
    report_id: EvaluationReportId,
    report_digest: ArtifactDigest,
    audit_record_id: agent_evolution_protocol::AuditRecordId,
    audit_head_digest: ArtifactDigest,
    parent_revision_id: GenomeRevisionId,
    candidate_revision_id: GenomeRevisionId,
    evaluation_policy_version: String,
    commit_policy_version: String,
    verifier_set_digest: String,
    gate_decision: GateDecision,
    lifecycle: EvolutionLifecycle,
}

/// 启动独立 Evaluator；可信 Reject/Unknown 仍返回成功回执和退出码 0。
#[tokio::main]
async fn main() {
    let args = Args::parse();
    let result = match args.command {
        Command::Evaluate => run_evaluate()
            .await
            .and_then(|value| serde_json::to_value(value).context("receipt_serialize_failed")),
        Command::Promote => run_promote()
            .await
            .and_then(|value| serde_json::to_value(value).context("receipt_serialize_failed")),
        Command::Rollback => run_rollback()
            .await
            .and_then(|value| serde_json::to_value(value).context("receipt_serialize_failed")),
    };
    match result {
        Ok(receipt) => match serde_json::to_string(&receipt) {
            Ok(json) => println!("{json}"),
            Err(_) => fail("receipt_serialize_failed"),
        },
        Err(error) => fail(stable_error_code(&error)),
    }
}

/// 执行一次完整的加载、比较、Gate、Report、Audit 与 Seal 提交。
async fn run_evaluate() -> Result<EvaluationReceiptV1> {
    let config = TrustedConfig::from_env().context("config_invalid")?;
    let request = read_request().context("request_invalid")?;
    request.validate().context("request_invalid")?;

    let resolver = FileGenomeResolver::new(&config.evolution_root);
    let stable = resolver
        .stable_reference(&request.lineage)
        .await
        .context("stable_precondition_failed")?;
    if stable.revision_id != request.parent_revision_id
        || stable.generation != request.expected_parent_generation
    {
        return Err(anyhow!("stable_precondition_failed"));
    }
    let parent = resolve_revision(&resolver, &request.parent_revision_id).await?;
    let candidate = resolve_revision(&resolver, &request.candidate_revision_id).await?;
    let artifacts = FileArtifactStore::new(config.evolution_root.join("artifacts"));
    let parent_subject = load_subject(&artifacts, &parent).await?;
    let candidate_subject = load_subject(&artifacts, &candidate).await?;

    let dataset = TrustedDatasetStore::open_pinned(
        &config.dataset_root,
        config.expected_manifest_digest.clone(),
    )
    .and_then(|store| store.load())
    .context("dataset_invalid")?;
    if dataset.manifest().dataset_version != request.expected_dataset_version {
        return Err(anyhow!("dataset_version_mismatch"));
    }
    let environment = trusted_environment(&config, &parent)?;
    let runner = ComparativeRunner::new(
        dataset,
        ComparativeRunnerConfig {
            fixture_workspace_root: config.fixture_workspace_root,
            environment,
        },
    )
    .context("runner_invalid")?;
    let comparison = runner
        .run_pair(&parent_subject, &candidate_subject)
        .await
        .context("evaluation_failed")?;
    let generated_at_ms = now_ms()?;
    let trusted = EvaluationReportBuilder::task_strategy_mvp()
        .build(
            &comparison,
            &parent,
            &candidate,
            EvaluationReportMetadata {
                lineage: Some(request.lineage),
                parent_generation: Some(request.expected_parent_generation),
                candidate_generation: Some(
                    request
                        .expected_parent_generation
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("generation_overflow"))?,
                ),
                generated_at_ms,
            },
        )
        .context("report_build_failed")?;
    let archive = TrustedEvaluationArchive::new(config.archive_root);
    let verified = archive
        .commit(&trusted, generated_at_ms)
        .await
        .context("archive_commit_failed")?;
    let report = verified.report();
    let seal = verified.seal();
    Ok(EvaluationReceiptV1 {
        schema_version: EVALUATION_RECEIPT_SCHEMA_VERSION,
        request_id: request.request_id,
        report_id: report.report_id.clone(),
        report_digest: seal.report_digest.clone(),
        audit_record_id: seal.audit_record_id.clone(),
        audit_head_digest: seal.audit_record_digest.clone(),
        parent_revision_id: report.parent.genome_revision.clone(),
        candidate_revision_id: report.candidate.genome_revision.clone(),
        evaluation_policy_version: seal.evaluation_policy_version.clone(),
        commit_policy_version: seal.commit_policy_version.clone(),
        verifier_set_digest: seal.verifier_set_digest.clone(),
        gate_decision: report.gate_decision,
        lifecycle: report.lifecycle,
    })
}

/// 执行一次绑定正式 EvaluationReport 的 Promotion。
async fn run_promote() -> Result<ReleaseReceipt> {
    let config = TrustedReleaseConfig::from_env().context("config_invalid")?;
    let request: PromotionRequestV1 = read_json_request().context("request_invalid")?;
    if request.schema_version != EVALUATION_REQUEST_SCHEMA_VERSION {
        return Err(anyhow!("unsupported_request_schema")).context("request_invalid");
    }
    ReleaseController::new(config.evolution_root, config.archive_root)
        .promote(&request.report_id, request.release_id, now_ms()?)
        .await
        .context("promotion_failed")
}

/// 执行一次绑定原 Promotion Report 的原子 Rollback。
async fn run_rollback() -> Result<ReleaseReceipt> {
    let config = TrustedReleaseConfig::from_env().context("config_invalid")?;
    let request: RollbackRequestV1 = read_json_request().context("request_invalid")?;
    if request.schema_version != EVALUATION_REQUEST_SCHEMA_VERSION {
        return Err(anyhow!("unsupported_request_schema")).context("request_invalid");
    }
    ReleaseController::new(config.evolution_root, config.archive_root)
        .rollback(&request.release_id, request.rollback_release_id, now_ms()?)
        .await
        .context("rollback_failed")
}

/// 从 stdin 读取单个带上限、拒绝未知字段的请求。
fn read_request() -> Result<EvaluationRequestV1> {
    read_json_request()
}

/// 从 stdin 读取任意受限请求类型，统一执行字节上限。
fn read_json_request<T>() -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let mut bytes = Vec::new();
    io::stdin()
        .lock()
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("read_stdin")?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(anyhow!("request_too_large"));
    }
    serde_json::from_slice(&bytes).context("invalid_json")
}

/// 解析指定 Revision 并保留受信错误边界。
async fn resolve_revision(
    resolver: &FileGenomeResolver,
    revision_id: &GenomeRevisionId,
) -> Result<GenomeRevision> {
    resolver
        .resolve(&GenomeSelector::Revision(revision_id.clone()))
        .await
        .context("genome_resolve_failed")
}

/// 从可信 CAS 加载 Task Strategy Prompt，并与 Genome 摘要重新绑定。
async fn load_subject(
    artifacts: &FileArtifactStore,
    revision: &GenomeRevision,
) -> Result<EvaluationSubject> {
    let digest = revision
        .genome
        .prompt
        .task_strategy()
        .ok_or_else(|| anyhow!("missing_task_strategy"))?;
    let bytes = artifacts
        .get(digest)
        .await
        .context("prompt_artifact_failed")?
        .ok_or_else(|| anyhow!("prompt_artifact_missing"))?;
    let prompt = String::from_utf8(bytes).map_err(|_| anyhow!("prompt_not_utf8"))?;
    EvaluationSubject::from_revision(revision, prompt).context("subject_invalid")
}

/// 从受信 Kernel 配置和 Parent Genome 派生 Runner 的静态环境摘要。
fn trusted_environment(
    config: &TrustedConfig,
    parent: &GenomeRevision,
) -> Result<EvaluationEnvironment> {
    Ok(EvaluationEnvironment {
        kernel_ref: config.kernel_ref.clone(),
        model_provider: String::new(),
        model: String::new(),
        model_parameters_digest: String::new(),
        tool_profile_digest: String::new(),
        execution_profile_digest: String::new(),
        plugin_set_digest: digest_json(&parent.genome.plugins)?,
        capability_owner_digest: digest_json(&parent.genome.capability_owners)?,
        resource_budget_digest: String::new(),
        verifier_version: String::new(),
        evaluation_policy_version: String::new(),
        environment_fixture_digest: String::new(),
        repeat_count: 0,
    })
}

/// 对稳定 serde 数据计算 SHA-256 文本。
fn digest_json(value: &impl Serialize) -> Result<String> {
    let bytes = serde_json::to_vec(value).context("digest_serialize_failed")?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

/// 返回受信进程当前 Unix 毫秒时间。
fn now_ms() -> Result<u64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("clock_before_epoch")?
        .as_millis();
    u64::try_from(millis).context("clock_overflow")
}

/// 读取必需环境变量。
fn required_env(name: &'static str) -> Result<String> {
    env::var(name).map_err(|_| anyhow!("missing_trusted_config"))
}

/// 读取非空受信路径环境变量。
fn required_path(name: &'static str) -> Result<PathBuf> {
    let value = required_env(name)?;
    if value.is_empty() {
        return Err(anyhow!("missing_trusted_config"));
    }
    Ok(PathBuf::from(value))
}

/// 只输出稳定错误码，避免路径、Hidden 内容或 Verifier 细节进入 stderr。
fn stable_error_code(error: &anyhow::Error) -> &'static str {
    for cause in error.chain() {
        match cause.to_string().as_str() {
            "config_invalid" => return "config_invalid",
            "request_invalid" => return "request_invalid",
            "stable_precondition_failed" => return "stable_precondition_failed",
            "dataset_invalid" | "dataset_version_mismatch" => return "dataset_invalid",
            "evaluation_failed" | "runner_invalid" => return "evaluation_failed",
            "report_build_failed" => return "report_build_failed",
            "archive_commit_failed" => return "archive_commit_failed",
            "promotion_failed" => return "promotion_failed",
            "rollback_failed" => return "rollback_failed",
            _ => {}
        }
    }
    "evaluation_control_plane_failed"
}

/// 输出稳定错误码并以失败状态退出。
fn fail(code: &'static str) -> ! {
    eprintln!("lucia-eval:{code}");
    std::process::exit(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 请求必须拒绝 Dataset、Policy 与 Gate 等控制面字段。
    #[test]
    fn request_rejects_control_plane_fields() {
        let base = serde_json::json!({
            "schema_version": 1,
            "request_id": "request-0001",
            "parent_revision_id": GenomeRevisionId::generate(),
            "candidate_revision_id": GenomeRevisionId::generate(),
            "lineage": "stable/test",
            "expected_parent_generation": 1,
            "expected_dataset_version": DatasetVersionId::generate(),
        });
        for field in ["dataset_root", "verifier", "commit_policy", "gate_decision"] {
            let mut value = base.clone();
            value[field] = serde_json::json!("forged");
            assert!(serde_json::from_value::<EvaluationRequestV1>(value).is_err());
        }
    }

    /// 请求 ID 与 lineage 不能携带路径穿越或无界内容。
    #[test]
    fn request_validates_stable_names() {
        let request = EvaluationRequestV1 {
            schema_version: 1,
            request_id: "request-0001".to_string(),
            parent_revision_id: GenomeRevisionId::generate(),
            candidate_revision_id: GenomeRevisionId::generate(),
            lineage: "stable/test".to_string(),
            expected_parent_generation: 1,
            expected_dataset_version: DatasetVersionId::generate(),
        };
        request.validate().expect("合法请求应通过");
        let mut escaped = request;
        escaped.lineage = "../stable".to_string();
        assert!(escaped.validate().is_err());
    }
}

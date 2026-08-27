//! Lucia 独立受信离线 Evaluator。
//!
//! 单次请求只能声明 Parent/Candidate 身份与并发前置条件。Dataset、Verifier、Commit Policy、
//! Workspace 和 Store 根均来自受信环境配置，禁止由 Candidate 或 Mutator 随请求指定。

use agent_evaluation::{
    evaluate_context_policy_candidate, ComparativeRunner, ComparativeRunnerConfig,
    ContextEvaluationReportBuilder, ContextEvaluationReportMetadata, EvaluationArchiveError,
    EvaluationReportBuilder, EvaluationReportIdentity, EvaluationReportMetadata, EvaluationSubject,
    FileRuntimeHealthObservationStore, ReleaseController, ReleaseHealthVerifier, ReleaseReceipt,
    TrustedContextObservationFixture, TrustedDatasetStore, TrustedEvaluationArchive,
    VerifiedEvaluation,
};
use agent_evolution::{
    ArtifactStore, FileArtifactStore, FileGenomeResolver, GenomeResolver, GenomeSelector,
};
use agent_evolution_protocol::{
    ArtifactDigest, ContextEvaluationReceiptV1, ContextEvaluationRequestV1,
    ContextPolicyEvaluationReportV1, EvaluationEnvironment, EvaluationReceiptV1,
    EvaluationRequestV1, GenomeRevision, GenomeRevisionId, HealthCheckReceiptV1,
    HealthCheckRequestV1, PromotionRequestV1, ReleaseReceiptV1, RollbackRequestV1,
    CONTEXT_EVALUATION_RECEIPT_SCHEMA_VERSION, EVALUATION_RECEIPT_SCHEMA_VERSION,
    RELEASE_RECEIPT_SCHEMA_VERSION,
};
#[cfg(test)]
use agent_evolution_protocol::{DatasetVersionId, EVALUATION_REQUEST_SCHEMA_VERSION};
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
    /// 运行固定八指标 Context Gate 并提交正式 Report Seal。
    ContextEvaluate,
    /// 使用正式 EvaluationReport 晋升 Candidate。
    Promote,
    /// 把指定 Promotion 原子回滚到 Parent。
    Rollback,
    /// 复核 Promotion 后 Stable 与受信 Runtime 健康观察。
    Health,
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

/// Context Gate 需要的受信 Registry、Fixture 与 Archive 配置。
#[derive(Debug, Clone)]
struct TrustedContextConfig {
    evolution_root: PathBuf,
    archive_root: PathBuf,
    fixture_root: PathBuf,
    expected_fixture_digest: ArtifactDigest,
}

impl TrustedContextConfig {
    /// 只从进程环境加载 Context Gate 的固定输入位置和摘要。
    fn from_env() -> Result<Self> {
        let expected_fixture_digest =
            ArtifactDigest::new(required_env("LUCIA_EVAL_CONTEXT_FIXTURE_DIGEST")?)
                .map_err(|_| anyhow!("invalid_context_fixture_digest"))?;
        Ok(Self {
            evolution_root: required_path("LUCIA_EVAL_EVOLUTION_ROOT")?,
            archive_root: required_path("LUCIA_EVAL_ARCHIVE_ROOT")?,
            fixture_root: required_path("LUCIA_EVAL_CONTEXT_FIXTURE_ROOT")?,
            expected_fixture_digest,
        })
    }
}

/// Promotion 健康复核需要的受信 Store 根配置。
#[derive(Debug, Clone)]
struct TrustedHealthConfig {
    evolution_root: PathBuf,
    archive_root: PathBuf,
    health_store_root: PathBuf,
}

impl TrustedHealthConfig {
    /// 只从进程环境加载 Registry、Archive 与 Runtime 观察 Store 根。
    fn from_env() -> Result<Self> {
        Ok(Self {
            evolution_root: required_path("LUCIA_EVAL_EVOLUTION_ROOT")?,
            archive_root: required_path("LUCIA_EVAL_ARCHIVE_ROOT")?,
            health_store_root: required_path("LUCIA_EVAL_HEALTH_STORE_ROOT")?,
        })
    }
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

/// 启动独立 Evaluator；可信 Reject/Unknown 仍返回成功回执和退出码 0。
#[tokio::main]
async fn main() {
    let args = Args::parse();
    let result = match args.command {
        Command::Evaluate => run_evaluate()
            .await
            .and_then(|value| serde_json::to_value(value).context("receipt_serialize_failed")),
        Command::ContextEvaluate => run_context_evaluate()
            .await
            .and_then(|value| serde_json::to_value(value).context("receipt_serialize_failed")),
        Command::Promote => run_promote()
            .await
            .and_then(|value| serde_json::to_value(value).context("receipt_serialize_failed")),
        Command::Rollback => run_rollback()
            .await
            .and_then(|value| serde_json::to_value(value).context("receipt_serialize_failed")),
        Command::Health => run_health()
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

/// 执行受信 Context Fixture 加载、八指标 Gate、正式 Archive 与 Seal 提交。
async fn run_context_evaluate() -> Result<ContextEvaluationReceiptV1> {
    let config = TrustedContextConfig::from_env().context("config_invalid")?;
    let request: ContextEvaluationRequestV1 = read_json_request().context("request_invalid")?;
    request.validate().context("request_invalid")?;
    let fixture = TrustedContextObservationFixture::open_pinned(
        &config.fixture_root,
        config.expected_fixture_digest,
    )
    .await
    .context("context_fixture_invalid")?;
    if fixture.version() != &request.expected_fixture_version {
        return Err(anyhow!("context_fixture_version_mismatch"));
    }

    let resolver = FileGenomeResolver::new(&config.evolution_root);
    let parent = resolve_revision(&resolver, &request.parent_revision_id).await?;
    let candidate = resolve_revision(&resolver, &request.candidate_revision_id).await?;
    let parent_observation = fixture
        .observation(&request.parent_revision_id)
        .context("context_fixture_invalid")?;
    let candidate_observation = fixture
        .observation(&request.candidate_revision_id)
        .context("context_fixture_invalid")?;
    let context_report = evaluate_context_policy_candidate(
        &parent,
        &candidate,
        parent_observation,
        candidate_observation,
    )
    .context("context_evaluation_failed")?;

    let archive_request = request.archive_request();
    let archive = TrustedEvaluationArchive::new(&config.archive_root);
    let binding = archive
        .bind_request(&archive_request, now_ms()?)
        .await
        .context("archive_commit_failed")?;
    match archive.get_verified_for_request(&binding).await {
        Ok(verified) => {
            return context_evaluation_receipt(
                &request,
                &context_report,
                fixture.digest(),
                &verified,
            )
        }
        Err(EvaluationArchiveError::SealNotFound(_)) => {}
        Err(error) => return Err(error).context("archive_commit_failed"),
    }
    match archive.get_prepared_for_request(&binding).await {
        Ok(trusted) => {
            let verified = archive
                .commit(&trusted, binding.generated_at_ms)
                .await
                .context("archive_commit_failed")?;
            return context_evaluation_receipt(
                &request,
                &context_report,
                fixture.digest(),
                &verified,
            );
        }
        Err(EvaluationArchiveError::PreparedEvaluationNotFound(_)) => {}
        Err(error) => return Err(error).context("archive_commit_failed"),
    }

    let stable = resolver
        .stable_reference(&request.lineage)
        .await
        .context("stable_precondition_failed")?;
    if stable.revision_id != request.parent_revision_id
        || stable.generation != request.expected_parent_generation
    {
        return Err(anyhow!("stable_precondition_failed"));
    }
    let trusted = ContextEvaluationReportBuilder
        .build_with_fixed_identity(
            &context_report,
            parent_observation,
            candidate_observation,
            &parent,
            &candidate,
            ContextEvaluationReportMetadata {
                lineage: request.lineage.clone(),
                parent_generation: request.expected_parent_generation,
                fixture_version: request.expected_fixture_version.clone(),
                fixture_digest: fixture.digest().clone(),
                generated_at_ms: binding.generated_at_ms,
            },
            EvaluationReportIdentity {
                report_id: binding.report_id.clone(),
                generated_at_ms: binding.generated_at_ms,
            },
        )
        .context("report_build_failed")?;
    archive
        .prepare_for_request(&binding, &trusted)
        .await
        .context("archive_commit_failed")?;
    let verified = archive
        .commit_prepared_for_request(&binding, binding.generated_at_ms)
        .await
        .context("archive_commit_failed")?;
    context_evaluation_receipt(&request, &context_report, fixture.digest(), &verified)
}

/// 从 Context Gate 报告和已复核 Seal 构造严格绑定的专用回执。
fn context_evaluation_receipt(
    request: &ContextEvaluationRequestV1,
    context_report: &ContextPolicyEvaluationReportV1,
    fixture_digest: &ArtifactDigest,
    verified: &VerifiedEvaluation,
) -> Result<ContextEvaluationReceiptV1> {
    let report = verified.report();
    let seal = verified.seal();
    let expected_fixture_binding =
        format!("{}:{}", request.expected_fixture_version, fixture_digest);
    let context_surface_is_exact = report.genome_diff.changed_surfaces.len() == 1
        && report
            .genome_diff
            .changed_surfaces
            .contains(&agent_evolution_protocol::MutationSurface::ContextPolicy)
        && report.allowed_mutation_surfaces.len() == 1
        && report
            .allowed_mutation_surfaces
            .contains(&agent_evolution_protocol::MutationSurface::ContextPolicy);
    if report.parent.genome_revision != context_report.parent_revision_id
        || report.candidate.genome_revision != context_report.candidate_revision_id
        || report.gate_decision != context_report.decision
        || report.lineage.as_deref() != Some(request.lineage.as_str())
        || report.parent_generation != Some(request.expected_parent_generation)
        || report.candidate_generation != request.expected_parent_generation.checked_add(1)
        || !context_surface_is_exact
        || seal.commit_policy_version != agent_evaluation::M6_CONTEXT_GATE_VERSION
        || seal.evaluation_policy_version != agent_evaluation::M6_CONTEXT_GATE_VERSION
        || report.parent.environment.evaluation_policy_version
            != agent_evaluation::M6_CONTEXT_GATE_VERSION
        || report.candidate.environment.evaluation_policy_version
            != agent_evaluation::M6_CONTEXT_GATE_VERSION
        || report
            .parent
            .environment
            .environment_fixture_digest
            .as_str()
            != expected_fixture_binding.as_str()
        || report
            .candidate
            .environment
            .environment_fixture_digest
            .as_str()
            != expected_fixture_binding.as_str()
    {
        return Err(anyhow!("context_archive_binding_mismatch"));
    }
    let context_bytes = serde_json::to_vec(context_report).context("receipt_serialize_failed")?;
    let context_report_digest =
        ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(context_bytes)))
            .map_err(|_| anyhow!("receipt_serialize_failed"))?;
    let receipt = ContextEvaluationReceiptV1 {
        schema_version: CONTEXT_EVALUATION_RECEIPT_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        report_id: report.report_id.clone(),
        report_digest: seal.report_digest.clone(),
        context_report_digest,
        audit_record_id: seal.audit_record_id.clone(),
        audit_head_digest: seal.audit_record_digest.clone(),
        fixture_version: request.expected_fixture_version.clone(),
        context_report: context_report.clone(),
        lifecycle: report.lifecycle,
    };
    receipt
        .validate(agent_evaluation::M6_CONTEXT_GATE_VERSION)
        .context("context_archive_binding_mismatch")?;
    Ok(receipt)
}

/// 执行一次完整的加载、比较、Gate、Report、Audit 与 Seal 提交。
async fn run_evaluate() -> Result<EvaluationReceiptV1> {
    let config = TrustedConfig::from_env().context("config_invalid")?;
    let request = read_request().context("request_invalid")?;
    request.validate().context("request_invalid")?;
    let archive = TrustedEvaluationArchive::new(&config.archive_root);
    let binding = archive
        .bind_request(&request, now_ms()?)
        .await
        .context("archive_commit_failed")?;
    match archive.get_verified_for_request(&binding).await {
        Ok(verified) => return Ok(evaluation_receipt(&request.request_id, &verified)),
        Err(EvaluationArchiveError::SealNotFound(_)) => {}
        Err(error) => return Err(error).context("archive_commit_failed"),
    }
    match archive.get_prepared_for_request(&binding).await {
        Ok(trusted) => {
            let verified = archive
                .commit(&trusted, binding.generated_at_ms)
                .await
                .context("archive_commit_failed")?;
            return Ok(evaluation_receipt(&request.request_id, &verified));
        }
        Err(EvaluationArchiveError::PreparedEvaluationNotFound(_)) => {}
        Err(error) => return Err(error).context("archive_commit_failed"),
    }

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
    let trusted = EvaluationReportBuilder::task_strategy_mvp()
        .build_with_fixed_identity(
            &comparison,
            &parent,
            &candidate,
            EvaluationReportMetadata {
                lineage: Some(request.lineage.clone()),
                parent_generation: Some(request.expected_parent_generation),
                candidate_generation: Some(
                    request
                        .expected_parent_generation
                        .checked_add(1)
                        .ok_or_else(|| anyhow!("generation_overflow"))?,
                ),
                generated_at_ms: binding.generated_at_ms,
            },
            EvaluationReportIdentity {
                report_id: binding.report_id.clone(),
                generated_at_ms: binding.generated_at_ms,
            },
        )
        .context("report_build_failed")?;
    archive
        .prepare_for_request(&binding, &trusted)
        .await
        .context("archive_commit_failed")?;
    let verified = archive
        .commit_prepared_for_request(&binding, binding.generated_at_ms)
        .await
        .context("archive_commit_failed")?;
    Ok(evaluation_receipt(&request.request_id, &verified))
}

/// 从已完整复核的 Report Seal 构造共享 Evaluate 回执。
fn evaluation_receipt(request_id: &str, verified: &VerifiedEvaluation) -> EvaluationReceiptV1 {
    let report = verified.report();
    let seal = verified.seal();
    EvaluationReceiptV1 {
        schema_version: EVALUATION_RECEIPT_SCHEMA_VERSION,
        request_id: request_id.to_string(),
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
    }
}

/// 执行一次绑定正式 EvaluationReport 的 Promotion。
async fn run_promote() -> Result<ReleaseReceiptV1> {
    let config = TrustedReleaseConfig::from_env().context("config_invalid")?;
    let request: PromotionRequestV1 = read_json_request().context("request_invalid")?;
    request.validate().context("request_invalid")?;
    let receipt = ReleaseController::new(config.evolution_root, config.archive_root)
        .promote(&request.report_id, request.release_id, now_ms()?)
        .await
        .context("promotion_failed")?;
    Ok(release_receipt_v1(receipt))
}

/// 执行一次绑定原 Promotion Report 的原子 Rollback。
async fn run_rollback() -> Result<ReleaseReceiptV1> {
    let config = TrustedReleaseConfig::from_env().context("config_invalid")?;
    let request: RollbackRequestV1 = read_json_request().context("request_invalid")?;
    request.validate().context("request_invalid")?;
    let receipt = ReleaseController::new(config.evolution_root, config.archive_root)
        .rollback(&request.release_id, request.rollback_release_id, now_ms()?)
        .await
        .context("rollback_failed")?;
    Ok(release_receipt_v1(receipt))
}

/// 从受信观察 Store 复核 Promotion 后 Runtime 健康状态。
async fn run_health() -> Result<HealthCheckReceiptV1> {
    let config = TrustedHealthConfig::from_env().context("config_invalid")?;
    let request: HealthCheckRequestV1 = read_json_request().context("request_invalid")?;
    request.validate().context("request_invalid")?;
    let observations = FileRuntimeHealthObservationStore::new(config.health_store_root)
        .context("config_invalid")?;
    ReleaseHealthVerifier::new(config.evolution_root, config.archive_root, observations)
        .verify(&request)
        .await
        .context("health_check_failed")
}

/// 把 Evaluator 内部 Release 结果映射为共享版本化 IPC 回执。
fn release_receipt_v1(receipt: ReleaseReceipt) -> ReleaseReceiptV1 {
    ReleaseReceiptV1 {
        schema_version: RELEASE_RECEIPT_SCHEMA_VERSION,
        release_id: receipt.release_id,
        report_id: receipt.report_id,
        lineage: receipt.lineage,
        from: receipt.from,
        to: receipt.to,
        generation: receipt.generation,
        audit_record_id: receipt.audit_record_id,
        rollback_of: receipt.rollback_of,
    }
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
            "context_fixture_invalid" | "context_fixture_version_mismatch" => {
                return "context_fixture_invalid"
            }
            "context_evaluation_failed" | "context_archive_binding_mismatch" => {
                return "context_evaluation_failed"
            }
            "evaluation_failed" | "runner_invalid" => return "evaluation_failed",
            "report_build_failed" => return "report_build_failed",
            "archive_commit_failed" => return "archive_commit_failed",
            "promotion_failed" => return "promotion_failed",
            "rollback_failed" => return "rollback_failed",
            "health_check_failed" => return "health_check_failed",
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

    /// Context 请求只能声明修订和 Fixture 版本，不能覆盖观察、Gate 或存储配置。
    #[test]
    fn context_request_rejects_control_plane_fields() {
        let base = serde_json::json!({
            "schema_version": EVALUATION_REQUEST_SCHEMA_VERSION,
            "request_id": "context-request-0001",
            "parent_revision_id": GenomeRevisionId::generate(),
            "candidate_revision_id": GenomeRevisionId::generate(),
            "lineage": "stable/test",
            "expected_parent_generation": 1,
            "expected_fixture_version": DatasetVersionId::generate(),
        });
        for field in [
            "fixture_root",
            "fixture_digest",
            "observations",
            "gate_policy",
            "gate_decision",
            "archive_root",
        ] {
            let mut value = base.clone();
            value[field] = serde_json::json!("forged");
            assert!(serde_json::from_value::<ContextEvaluationRequestV1>(value).is_err());
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

    /// `health` 必须是显式受限子命令，不能通过自由动作字符串进入控制面。
    #[test]
    fn parses_explicit_health_command() {
        let args = Args::try_parse_from(["lucia-eval", "health"]).expect("health 子命令应存在");
        assert!(matches!(args.command, Command::Health));
        assert!(Args::try_parse_from(["lucia-eval", "health", "/tmp/forged"]).is_err());
    }

    /// `context-evaluate` 必须是无自由参数的显式受限子命令。
    #[test]
    fn parses_explicit_context_evaluate_command() {
        let args = Args::try_parse_from(["lucia-eval", "context-evaluate"])
            .expect("context-evaluate 子命令应存在");
        assert!(matches!(args.command, Command::ContextEvaluate));
        assert!(Args::try_parse_from(["lucia-eval", "context-evaluate", "/tmp/forged"]).is_err());
    }

    /// Health stdin 只能声明 Release 绑定，不能携带观察 Store 或 Stable 路径。
    #[test]
    fn health_request_rejects_control_plane_paths() {
        let request = serde_json::json!({
            "schema_version": EVALUATION_REQUEST_SCHEMA_VERSION,
            "request_id": "health-request-001",
            "release_id": agent_evolution_protocol::ReleaseId::generate(),
            "lineage": "stable/test",
            "expected_revision_id": GenomeRevisionId::generate(),
            "expected_generation": 2,
            "health_store_root": "/tmp/forged"
        });
        assert!(serde_json::from_value::<HealthCheckRequestV1>(request).is_err());
    }
}

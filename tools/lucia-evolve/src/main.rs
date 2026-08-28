//! Lucia 外部自进化控制面客户端。
//!
//! 本工具只通过 Prompt Evolution Cycle 调用固定独立 Evaluator，不链接评测实现，也不接受
//! Dataset、Verifier、Commit Policy 或存储路径参数。Promotion、健康验证与 Rollback 只能由
//! Cycle 状态机驱动并归档，不能通过低层直通子命令绕过。

use agent_evolution::{
    ContextEvolutionCycle, ContextEvolutionCycleRequestV1, ContextEvolutionCycleSnapshotV1,
    DeterministicPromptMutationGenerator, DeterministicSkillMutationGenerator, EpisodeSelector,
    EvolutionCycleStore, FileContextCycleArchive, FileEpisodeStore, FileEvolutionCycleStore,
    FileEvolutionOutbox, FileIssueObservationStore, FileSkillEvolutionCycleArchive,
    LuciaEvalProcessClient, LuciaEvalSkillProcessClient, PromptEvolutionCycle, SkillEvolutionCycle,
    SkillEvolutionCycleRequestV1, SkillEvolutionCycleSnapshotV1,
};
use agent_evolution_protocol::{
    DatasetVersionId, EvolutionCycleId, EvolutionCycleRequestV1, EvolutionCycleSnapshotV1,
};
use clap::{error::ErrorKind, Parser, Subcommand};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    env,
    ffi::OsString,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// stdin 请求允许的最大字节数，防止无界 JSON 占用内存。
const MAX_REQUEST_BYTES: u64 = 64 * 1024;
/// 独立 Evaluator 单次调用的固定超时。
const EVALUATOR_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// 受信 Evaluator 可执行文件的唯一配置入口。
const EVALUATOR_BIN_ENV: &str = "LUCIA_EVOLVE_EVALUATOR_BIN";
/// 受信 Evolution 数据根的唯一配置入口。
const EVOLUTION_ROOT_ENV: &str = "LUCIA_EVOLVE_EVOLUTION_ROOT";
/// Cycle 使用的固定受信 Dataset 版本配置入口。
const DATASET_VERSION_ENV: &str = "LUCIA_EVOLVE_DATASET_VERSION";
/// Context Cycle 使用的固定受信 Fixture 版本配置入口。
const CONTEXT_FIXTURE_VERSION_ENV: &str = "LUCIA_EVOLVE_CONTEXT_FIXTURE_VERSION";

/// 外部自进化控制面命令；请求正文只能来自 stdin。
#[derive(Debug, Parser)]
#[command(author, version, about = "执行、检查并完成 Prompt Evolution Cycle")]
struct Args {
    /// 发送给独立 Evaluator 的受限动作。
    #[command(subcommand)]
    command: Command,
}

/// 当前客户端支持的 Evolution 与共享 Evaluator IPC 动作。
#[derive(Debug, Subcommand)]
enum Command {
    /// 从 stdin 读取 EvolutionCycleRequestV1 并执行完整 Prompt Cycle。
    Cycle,
    /// 读取并验证指定 Cycle 的完整只追加快照历史。
    Inspect {
        /// 需要检查的强类型 Cycle 标识。
        #[arg(long, value_name = "CYCLE_ID")]
        cycle_id: EvolutionCycleId,
    },
    /// 驱动指定 Cycle 完成受信健康验证；失败时在同一 Cycle 内自动回滚。
    Health {
        /// 已进入 AwaitingHealth 的强类型 Cycle 标识。
        #[arg(long, value_name = "CYCLE_ID")]
        cycle_id: EvolutionCycleId,
    },
    /// 从 stdin 读取 ContextEvolutionCycleRequestV1 并执行完整 Context Cycle。
    ContextCycle,
    /// 从 stdin 读取 SkillEvolutionCycleRequestV1 并执行完整 Skill Cycle。
    SkillCycle,
    /// 读取并验证指定 Skill Cycle 的完整只追加快照历史。
    SkillInspect {
        /// 需要检查的强类型 Skill Cycle 标识。
        #[arg(long, value_name = "CYCLE_ID")]
        cycle_id: EvolutionCycleId,
    },
    /// 驱动指定 Skill Cycle 完成健康验证；失败时自动回滚。
    SkillHealth {
        /// 已进入 AwaitingHealth 的强类型 Skill Cycle 标识。
        #[arg(long, value_name = "CYCLE_ID")]
        cycle_id: EvolutionCycleId,
    },
    /// 读取并验证指定 Context Cycle 的完整只追加快照历史。
    ContextInspect {
        /// 需要检查的强类型 Context Cycle 标识。
        #[arg(long, value_name = "CYCLE_ID")]
        cycle_id: EvolutionCycleId,
    },
    /// 驱动指定 Context Cycle 完成健康验证；失败时自动回滚。
    ContextHealth {
        /// 已进入 AwaitingHealth 的强类型 Context Cycle 标识。
        #[arg(long, value_name = "CYCLE_ID")]
        cycle_id: EvolutionCycleId,
    },
}

/// CLI 对外公开的稳定失败码；不携带路径、请求正文或 Evaluator 错误细节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureCode {
    /// 子命令或参数不合法。
    CommandInvalid,
    /// stdin JSON、schema 或请求语义不合法。
    RequestInvalid,
    /// Evaluator 环境变量缺失或不是绝对路径。
    EvaluatorConfigInvalid,
    /// Evolution 数据根或 Dataset 版本配置无效。
    EvolutionConfigInvalid,
    /// Cycle Store 读取或完整性校验失败。
    CycleInspectFailed,
    /// Cycle Runner 返回的稳定失败码。
    Cycle(&'static str),
    /// 脱敏回执序列化失败。
    ReceiptSerializeFailed,
    /// stdout 写入失败。
    StdoutFailed,
}

impl FailureCode {
    /// 返回可跨进程稳定消费的 ASCII 错误码。
    fn as_str(self) -> &'static str {
        match self {
            Self::CommandInvalid => "command_invalid",
            Self::RequestInvalid => "request_invalid",
            Self::EvaluatorConfigInvalid => "evaluator_config_invalid",
            Self::EvolutionConfigInvalid => "evolution_config_invalid",
            Self::CycleInspectFailed => "cycle_inspect_failed",
            Self::Cycle(code) => code,
            Self::ReceiptSerializeFailed => "receipt_serialize_failed",
            Self::StdoutFailed => "stdout_failed",
        }
    }
}

/// 解析命令并执行单次受限 IPC；帮助和版本信息由 clap 正常输出。
#[tokio::main]
async fn main() {
    let args = match Args::try_parse() {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            if error.print().is_err() {
                fail(FailureCode::StdoutFailed);
            }
            return;
        }
        Err(_) => fail(FailureCode::CommandInvalid),
    };
    if let Err(code) = dispatch(args.command).await {
        fail(code);
    }
}

/// 分派共享 IPC 子命令，并直接输出对应脱敏回执。
async fn dispatch(command: Command) -> Result<(), FailureCode> {
    match command {
        Command::Cycle => {
            let snapshot = execute_cycle().await?;
            write_receipt(&snapshot)
        }
        Command::Inspect { cycle_id } => {
            let history = execute_inspect(&cycle_id).await?;
            write_receipt(&history)
        }
        Command::Health { cycle_id } => {
            let snapshot = execute_health(&cycle_id).await?;
            write_receipt(&snapshot)
        }
        Command::ContextCycle => {
            let snapshot = execute_context_cycle().await?;
            write_receipt(&snapshot)
        }
        Command::SkillCycle => {
            let snapshot = execute_skill_cycle().await?;
            write_receipt(&snapshot)
        }
        Command::SkillInspect { cycle_id } => {
            let history = execute_skill_inspect(&cycle_id).await?;
            write_receipt(&history)
        }
        Command::SkillHealth { cycle_id } => {
            let snapshot = execute_skill_health(&cycle_id).await?;
            write_receipt(&snapshot)
        }
        Command::ContextInspect { cycle_id } => {
            let history = execute_context_inspect(&cycle_id).await?;
            write_receipt(&history)
        }
        Command::ContextHealth { cycle_id } => {
            let snapshot = execute_context_health(&cycle_id).await?;
            write_receipt(&snapshot)
        }
    }
}

/// 从固定 Store 恢复唯一证据，并执行或恢复 Skill Cycle 至等待健康或可信终态。
async fn execute_skill_cycle() -> Result<SkillEvolutionCycleSnapshotV1, FailureCode> {
    let request: SkillEvolutionCycleRequestV1 = read_json_request()?;
    request
        .validate()
        .map_err(|_| FailureCode::RequestInvalid)?;
    let evolution_root = evolution_root()?;
    let runner = skill_cycle_runner(&evolution_root)?;
    if let Some(existing) = runner
        .cycle_archive()
        .latest(&request.cycle_id)
        .await
        .map_err(|error| FailureCode::Cycle(error.code()))?
    {
        if !existing.stage.requires_mutation_evidence() {
            return runner
                .resume(&request)
                .await
                .map_err(|error| FailureCode::Cycle(error.code()));
        }
    }
    let evidence = EpisodeSelector::new(
        Arc::new(FileEvolutionOutbox::new(evolution_root.join("outbox"))),
        Arc::new(FileEpisodeStore::new(evolution_root.join("episodes"))),
        Arc::new(FileIssueObservationStore::new(
            evolution_root.join("issue-observations"),
        )),
    )
    .select()
    .await
    .map_err(|_| FailureCode::Cycle("skill_evidence_selection_failed"))?
    .into_iter()
    .filter(|evidence| evidence.genome_digest == request.parent_genome_digest)
    .collect::<Vec<_>>();
    if evidence.len() != 1 {
        return Err(FailureCode::Cycle("skill_evidence_not_unique"));
    }
    runner
        .run_until_health(&request, &evidence[0])
        .await
        .map_err(|error| FailureCode::Cycle(error.code()))
}

/// 从受信 Skill 阶段 Archive 读取并验证完整历史。
async fn execute_skill_inspect(
    cycle_id: &EvolutionCycleId,
) -> Result<Vec<SkillEvolutionCycleSnapshotV1>, FailureCode> {
    let history = FileSkillEvolutionCycleArchive::new(evolution_root()?.join("skill-cycles"))
        .history(cycle_id)
        .await
        .map_err(|_| FailureCode::CycleInspectFailed)?;
    if history.is_empty() {
        return Err(FailureCode::Cycle("skill_cycle_not_found"));
    }
    Ok(history)
}

/// 驱动 Skill Cycle 完成受信健康验证或自动回滚。
async fn execute_skill_health(
    cycle_id: &EvolutionCycleId,
) -> Result<SkillEvolutionCycleSnapshotV1, FailureCode> {
    skill_cycle_runner(&evolution_root()?)?
        .verify_health(cycle_id)
        .await
        .map_err(|error| FailureCode::Cycle(error.code()))
}

/// 组装固定 Skill Mutator 与独立 Evaluator 进程客户端。
fn skill_cycle_runner(
    evolution_root: &Path,
) -> Result<
    SkillEvolutionCycle<DeterministicSkillMutationGenerator, LuciaEvalSkillProcessClient>,
    FailureCode,
> {
    Ok(SkillEvolutionCycle::new(
        evolution_root,
        DeterministicSkillMutationGenerator,
        skill_evaluator_client(evolution_root)?,
    ))
}

/// 校验 Context Cycle 请求，并通过固定 Mutator 和独立 Evaluator 执行生产闭环。
async fn execute_context_cycle() -> Result<ContextEvolutionCycleSnapshotV1, FailureCode> {
    let request: ContextEvolutionCycleRequestV1 = read_json_request()?;
    request
        .validate()
        .map_err(|_| FailureCode::RequestInvalid)?;
    context_cycle_runner(evolution_root()?, context_fixture_version()?)?
        .run(&request)
        .await
        .map_err(|error| FailureCode::Cycle(error.code()))
}

/// 从受信 Context Archive 读取并验证完整历史。
async fn execute_context_inspect(
    cycle_id: &EvolutionCycleId,
) -> Result<Vec<ContextEvolutionCycleSnapshotV1>, FailureCode> {
    FileContextCycleArchive::new(evolution_root()?.join("context-cycles"))
        .history(cycle_id)
        .await
        .map_err(|_| FailureCode::CycleInspectFailed)
}

/// 驱动 Context Cycle 完成受信健康验证或自动回滚。
async fn execute_context_health(
    cycle_id: &EvolutionCycleId,
) -> Result<ContextEvolutionCycleSnapshotV1, FailureCode> {
    context_cycle_runner(evolution_root()?, context_fixture_version()?)?
        .verify_health(cycle_id)
        .await
        .map_err(|error| FailureCode::Cycle(error.code()))
}

/// 组装 Context Cycle 的固定 Fixture 版本与独立 Evaluator 客户端。
fn context_cycle_runner(
    evolution_root: PathBuf,
    fixture_version: DatasetVersionId,
) -> Result<ContextEvolutionCycle<LuciaEvalProcessClient>, FailureCode> {
    Ok(ContextEvolutionCycle::new(
        evolution_root,
        evaluator_client()?,
        fixture_version,
    ))
}

/// 校验 Cycle 请求，并通过固定生成器和独立 Evaluator 执行完整 Prompt 自进化周期。
async fn execute_cycle() -> Result<EvolutionCycleSnapshotV1, FailureCode> {
    let request: EvolutionCycleRequestV1 = read_json_request()?;
    request
        .validate()
        .map_err(|_| FailureCode::RequestInvalid)?;
    let evolution_root = evolution_root()?;
    let dataset_version = dataset_version()?;
    cycle_runner(evolution_root, dataset_version)?
        .run(&request)
        .await
        .map_err(|error| FailureCode::Cycle(error.code()))
}

/// 从受信 Cycle Store 读取并验证完整历史，不接受请求侧路径。
async fn execute_inspect(
    cycle_id: &EvolutionCycleId,
) -> Result<Vec<EvolutionCycleSnapshotV1>, FailureCode> {
    FileEvolutionCycleStore::new(evolution_root()?.join("cycles"))
        .history(cycle_id)
        .await
        .map_err(|_| FailureCode::CycleInspectFailed)
}

/// 通过 Cycle 所有者触发受信健康验证，并在失败时自动归档回滚。
async fn execute_health(
    cycle_id: &EvolutionCycleId,
) -> Result<EvolutionCycleSnapshotV1, FailureCode> {
    cycle_runner(evolution_root()?, dataset_version()?)?
        .verify_health(cycle_id)
        .await
        .map_err(|error| FailureCode::Cycle(error.code()))
}

/// 组装固定生成器、受信数据根、Dataset 版本和独立 Evaluator 的 Cycle Runner。
fn cycle_runner(
    evolution_root: PathBuf,
    dataset_version: DatasetVersionId,
) -> Result<
    PromptEvolutionCycle<DeterministicPromptMutationGenerator, LuciaEvalProcessClient>,
    FailureCode,
> {
    Ok(PromptEvolutionCycle::new(
        evolution_root,
        DeterministicPromptMutationGenerator,
        evaluator_client()?,
        dataset_version,
    ))
}

/// 从固定环境变量装配进程客户端；命令行和请求均不能覆盖可执行文件路径。
fn evaluator_client() -> Result<LuciaEvalProcessClient, FailureCode> {
    let value = env::var_os(EVALUATOR_BIN_ENV).ok_or(FailureCode::EvaluatorConfigInvalid)?;
    let executable = validate_evaluator_path(value)?;
    Ok(LuciaEvalProcessClient::new(executable, EVALUATOR_TIMEOUT))
}

/// 从固定环境变量装配共享 Artifact CAS 的 Skill Evaluator 客户端。
fn skill_evaluator_client(
    evolution_root: &Path,
) -> Result<LuciaEvalSkillProcessClient, FailureCode> {
    let value = env::var_os(EVALUATOR_BIN_ENV).ok_or(FailureCode::EvaluatorConfigInvalid)?;
    let executable = validate_evaluator_path(value)?;
    Ok(LuciaEvalSkillProcessClient::new(
        executable,
        EVALUATOR_TIMEOUT,
        evolution_root,
    ))
}

/// 从固定环境变量读取绝对 Evolution 数据根。
fn evolution_root() -> Result<PathBuf, FailureCode> {
    let value = env::var_os(EVOLUTION_ROOT_ENV).ok_or(FailureCode::EvolutionConfigInvalid)?;
    validate_absolute_path(value, FailureCode::EvolutionConfigInvalid)
}

/// 从固定环境变量读取并校验强类型 Dataset 版本。
fn dataset_version() -> Result<DatasetVersionId, FailureCode> {
    env::var(DATASET_VERSION_ENV)
        .ok()
        .ok_or(FailureCode::EvolutionConfigInvalid)
        .and_then(parse_dataset_version)
}

/// 从固定环境变量读取并校验 Context Fixture 版本。
fn context_fixture_version() -> Result<DatasetVersionId, FailureCode> {
    env::var(CONTEXT_FIXTURE_VERSION_ENV)
        .ok()
        .ok_or(FailureCode::EvolutionConfigInvalid)
        .and_then(parse_dataset_version)
}

/// 把受信环境文本解析为强类型 Dataset 版本。
fn parse_dataset_version(value: String) -> Result<DatasetVersionId, FailureCode> {
    value
        .parse()
        .map_err(|_| FailureCode::EvolutionConfigInvalid)
}

/// 校验 Evaluator 配置为绝对路径；文件类型和符号链接由进程客户端在调用前复核。
fn validate_evaluator_path(value: OsString) -> Result<PathBuf, FailureCode> {
    validate_absolute_path(value, FailureCode::EvaluatorConfigInvalid)
}

/// 校验环境注入路径为绝对路径，禁止依赖当前目录或 PATH 搜索。
fn validate_absolute_path(value: OsString, invalid: FailureCode) -> Result<PathBuf, FailureCode> {
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path).ok_or(invalid)
}

/// 从 stdin 读取一份有界严格 JSON 请求。
fn read_json_request<T>() -> Result<T, FailureCode>
where
    T: DeserializeOwned,
{
    read_json_request_from(io::stdin().lock())
}

/// 从指定输入读取有界 JSON，便于验证 stdin 边界且不依赖全局输入状态。
fn read_json_request_from<T, R>(reader: R) -> Result<T, FailureCode>
where
    T: DeserializeOwned,
    R: Read,
{
    let mut bytes = Vec::new();
    reader
        .take(MAX_REQUEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| FailureCode::RequestInvalid)?;
    if bytes.len() as u64 > MAX_REQUEST_BYTES {
        return Err(FailureCode::RequestInvalid);
    }
    serde_json::from_slice(&bytes).map_err(|_| FailureCode::RequestInvalid)
}

/// 无包装序列化共享脱敏回执，并以单行 JSON 写入 stdout。
fn write_receipt<T>(receipt: &T) -> Result<(), FailureCode>
where
    T: Serialize,
{
    let bytes = serde_json::to_vec(receipt).map_err(|_| FailureCode::ReceiptSerializeFailed)?;
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(&bytes)
        .and_then(|_| stdout.write_all(b"\n"))
        .map_err(|_| FailureCode::StdoutFailed)
}

/// 只向 stderr 输出稳定错误码并返回失败状态。
fn fail(code: FailureCode) -> ! {
    let _ = write_failure_code(io::stderr().lock(), code);
    std::process::exit(1)
}

/// 向指定输出写入单个稳定错误码，不附加底层错误文本。
fn write_failure_code<W>(mut writer: W, code: FailureCode) -> io::Result<()>
where
    W: Write,
{
    writeln!(writer, "{}", code.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_evolution_protocol::{
        EpisodeId, EvolutionIssueId, GenomeDigest, GenomeRevisionId, EVOLUTION_CYCLE_SCHEMA_VERSION,
    };
    use std::collections::BTreeSet;

    /// Cycle stdin DTO 必须拒绝未知路径和控制字段。
    #[test]
    fn requests_reject_unknown_fields() {
        let cycle = serde_json::json!({
            "schema_version": EVOLUTION_CYCLE_SCHEMA_VERSION,
            "cycle_id": EvolutionCycleId::generate(),
            "issue_id": EvolutionIssueId::generate(),
            "parent_revision_id": GenomeRevisionId::generate(),
            "parent_genome_digest": GenomeDigest::from_sha256_hex("a".repeat(64))
                .expect("摘要应合法"),
            "lineage": "stable/general",
            "expected_parent_generation": 1,
            "source_episode_ids": [EpisodeId::generate()],
            "evolution_policy_version": "task-strategy-mvp-v1",
            "candidate_count": 3,
            "requested_at_ms": 1,
            "evolution_root": "/forged",
        });
        assert_eq!(
            read_json_request_from::<EvolutionCycleRequestV1, _>(cycle.to_string().as_bytes()),
            Err(FailureCode::RequestInvalid)
        );
    }

    /// Context Cycle stdin 只能携带版本化请求，不能覆盖 Fixture、Gate 或 Archive 控制面。
    #[test]
    fn context_cycle_request_rejects_control_plane_fields() {
        let base = serde_json::json!({
            "schema_version": agent_evolution::CONTEXT_EVOLUTION_CYCLE_SCHEMA_VERSION,
            "cycle_id": EvolutionCycleId::generate(),
            "parent_revision_id": GenomeRevisionId::generate(),
            "parent_genome_digest": GenomeDigest::from_sha256_hex("a".repeat(64))
                .expect("摘要应合法"),
            "lineage": "stable/general",
            "expected_parent_generation": 1,
            "evidence_episode_ids": [EpisodeId::generate()],
            "expected_fixture_version": DatasetVersionId::generate(),
            "requested_at_ms": 1,
        });
        for field in [
            "fixture_root",
            "fixture_digest",
            "gate_policy",
            "archive_root",
            "release_root",
        ] {
            let mut value = base.clone();
            value[field] = serde_json::json!("forged");
            assert_eq!(
                read_json_request_from::<ContextEvolutionCycleRequestV1, _>(
                    value.to_string().as_bytes(),
                ),
                Err(FailureCode::RequestInvalid)
            );
        }
    }

    /// Skill Cycle stdin 只允许身份、Stable 前置条件和生命周期时间。
    #[test]
    fn skill_cycle_request_rejects_control_plane_fields() {
        let base = serde_json::json!({
            "cycle_id": EvolutionCycleId::generate(),
            "parent_revision_id": GenomeRevisionId::generate(),
            "parent_genome_digest": GenomeDigest::from_sha256_hex("a".repeat(64))
                .expect("摘要应合法"),
            "lineage": "production",
            "expected_parent_generation": 1,
            "mutation_generated_at_ms": 10,
            "candidate_created_at_ms": 20,
            "evaluated_at_ms": 30,
            "activated_at_ms": 40,
        });
        for field in [
            "observations",
            "authorization",
            "registry_root",
            "episode_store_root",
            "gate_policy",
            "health_verdict",
        ] {
            let mut value = base.clone();
            value[field] = serde_json::json!("forged");
            assert_eq!(
                read_json_request_from::<SkillEvolutionCycleRequestV1, _>(
                    value.to_string().as_bytes()
                ),
                Err(FailureCode::RequestInvalid)
            );
        }
        let parsed = Args::try_parse_from(["lucia-evolve", "skill-cycle"])
            .expect("skill-cycle 子命令应存在");
        assert!(matches!(parsed.command, Command::SkillCycle));
    }

    /// 普通 Evolver 不得暴露绕过 Cycle 状态机的低层 Evaluator 子命令。
    #[test]
    fn cli_rejects_low_level_evaluator_commands() {
        for command in [
            "evaluate",
            "promote",
            "rollback",
            "skill-evaluate",
            "skill-health",
        ] {
            assert!(Args::try_parse_from(["lucia-evolve", command]).is_err());
        }
    }

    /// Prompt、Context 与 Skill 的 Inspect、Health 只接受强类型 Cycle ID。
    #[test]
    fn cycle_commands_require_typed_id() {
        let cycle_id = EvolutionCycleId::generate();
        for command in [
            "inspect",
            "health",
            "context-inspect",
            "context-health",
            "skill-inspect",
            "skill-health",
        ] {
            let parsed =
                Args::try_parse_from(["lucia-evolve", command, "--cycle-id", cycle_id.as_str()])
                    .expect("合法 Cycle ID 应通过");
            let observed = match parsed.command {
                Command::Inspect { cycle_id }
                | Command::Health { cycle_id }
                | Command::ContextInspect { cycle_id }
                | Command::ContextHealth { cycle_id }
                | Command::SkillInspect { cycle_id }
                | Command::SkillHealth { cycle_id } => cycle_id,
                _ => panic!("应解析为 Cycle 查询命令"),
            };
            assert_eq!(observed, cycle_id);
            assert!(
                Args::try_parse_from(["lucia-evolve", command, "--cycle-id", "../cycles",])
                    .is_err()
            );
        }
    }

    /// Evaluator 环境变量必须提供绝对路径，不能依赖 PATH 搜索。
    #[test]
    fn evaluator_path_must_be_absolute() {
        assert_eq!(
            validate_evaluator_path(OsString::from("lucia-eval")),
            Err(FailureCode::EvaluatorConfigInvalid)
        );
        assert_eq!(
            validate_evaluator_path(OsString::from("/opt/lucia/bin/lucia-eval"))
                .expect("绝对路径应通过"),
            PathBuf::from("/opt/lucia/bin/lucia-eval")
        );
    }

    /// Evolution 根必须是绝对路径，Dataset 版本必须符合强类型 ID 契约。
    #[test]
    fn cycle_config_is_strictly_typed() {
        assert_eq!(
            validate_absolute_path(
                OsString::from("relative/evolution"),
                FailureCode::EvolutionConfigInvalid,
            ),
            Err(FailureCode::EvolutionConfigInvalid)
        );
        let dataset = DatasetVersionId::generate();
        assert_eq!(
            parse_dataset_version(dataset.to_string()).expect("Dataset ID 应合法"),
            dataset
        );
        assert_eq!(
            parse_dataset_version("../dataset".to_string()),
            Err(FailureCode::EvolutionConfigInvalid)
        );
    }

    /// 失败输出只能包含单个稳定错误码和换行。
    #[test]
    fn failure_output_contains_only_stable_code() {
        let mut output = Vec::new();
        write_failure_code(&mut output, FailureCode::RequestInvalid).expect("错误码应可写入");
        assert_eq!(output, b"request_invalid\n");
    }

    /// Skill Inspect、Health 与禁止的直接 Rollback 必须暴露稳定控制面错误码。
    #[test]
    fn skill_control_commands_keep_stable_failure_codes() {
        assert_eq!(
            FailureCode::Cycle("skill_cycle_not_found").as_str(),
            "skill_cycle_not_found"
        );
        assert_eq!(
            FailureCode::Cycle("skill_health_not_ready").as_str(),
            "skill_health_not_ready"
        );
        assert_eq!(
            FailureCode::CycleInspectFailed.as_str(),
            "cycle_inspect_failed"
        );
        assert!(Args::try_parse_from([
            "lucia-evolve",
            "skill-rollback",
            "--cycle-id",
            EvolutionCycleId::generate().as_str(),
        ])
        .is_err());
    }

    /// 工具的直接依赖必须保持在 Evolver 允许列表内，尤其不能链接 agent-evaluation。
    #[test]
    fn manifest_preserves_evaluator_dependency_boundary() {
        let dependencies = direct_dependencies(include_str!("../Cargo.toml"));
        let allowed = BTreeSet::from([
            "agent-evolution",
            "agent-evolution-protocol",
            "anyhow",
            "clap",
            "serde",
            "serde_json",
            "tokio",
        ]);
        assert!(
            dependencies.is_subset(&allowed),
            "Evolver 出现越界直接依赖：{:?}",
            dependencies.difference(&allowed).collect::<Vec<_>>()
        );
        assert!(dependencies.contains("agent-evolution"));
        assert!(dependencies.contains("agent-evolution-protocol"));
        assert!(!dependencies.contains("agent-evaluation"));
    }

    /// 提取 Cargo manifest 的直接普通依赖名称，忽略其他 section。
    fn direct_dependencies(manifest: &str) -> BTreeSet<&str> {
        let mut in_dependencies = false;
        let mut dependencies = BTreeSet::new();
        for line in manifest.lines() {
            let line = line.trim();
            if line == "[dependencies]" {
                in_dependencies = true;
                continue;
            }
            if line.starts_with('[') {
                in_dependencies = false;
                continue;
            }
            if in_dependencies {
                if let Some((key, _)) = line.split_once('=') {
                    let name = key.trim().split('.').next().unwrap_or_default();
                    if !name.is_empty() {
                        dependencies.insert(name);
                    }
                }
            }
        }
        dependencies
    }
}

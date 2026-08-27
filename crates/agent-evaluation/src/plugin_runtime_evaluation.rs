//! M8 插件 Safety 与 Agent 真实运行评测。
//!
//! 本模块从摘要固定的 Dataset 加载用例，由受信工厂创建 Evaluation Profile 下的 Host，
//! 执行真实工具/服务调用并生成脱敏报告。调用方不能提供失败计数、结论或评测证据。

use agent_evolution::{ArtifactStore, FileArtifactStore};
use agent_evolution_protocol::{
    ArtifactDigest, ArtifactRef, CandidateId, MutationId, PluginEvaluationEvidence,
    PluginEvaluationKind, PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION,
};
use agent_plugin_host::{
    manifest::PluginManifest,
    wasm::{WasmPluginHost, WasmPluginLimits},
    PluginHost, PluginHostServices, PluginServiceCall,
};
use agent_plugin_manager::hash_plugin_bundle;
use agent_tool::{ExecutionPolicy, ToolCall, ToolDecision, ToolResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::{timeout, Duration};

/// 插件运行评测 Dataset Manifest schema 版本。
pub const PLUGIN_RUNTIME_DATASET_SCHEMA_VERSION: u32 = 1;
/// 插件运行评测 Case schema 版本。
pub const PLUGIN_RUNTIME_CASE_SCHEMA_VERSION: u32 = 1;
/// 插件运行评测报告 schema 版本。
pub const PLUGIN_RUNTIME_REPORT_SCHEMA_VERSION: u32 = 1;
/// 固定 Dataset Manifest 文件名。
pub const PLUGIN_RUNTIME_MANIFEST_FILE_NAME: &str = "manifest.json";
/// 插件运行评测报告 CAS 媒体类型。
pub const PLUGIN_RUNTIME_REPORT_MEDIA_TYPE: &str =
    "application/vnd.ascnet.lucia.plugin-runtime-evaluation-report.v1+json";
/// 单个 Dataset 最大 Case 数。
pub const MAX_PLUGIN_RUNTIME_CASES: usize = 1_024;
/// 单个 Dataset 最大 Fixture 文件数。
pub const MAX_PLUGIN_RUNTIME_FIXTURES: usize = 4_096;
/// Manifest 最大字节数。
pub const MAX_PLUGIN_RUNTIME_MANIFEST_BYTES: u64 = 1024 * 1024;
/// 单个 Case 最大字节数。
pub const MAX_PLUGIN_RUNTIME_CASE_BYTES: u64 = 64 * 1024;
/// Manifest 与全部 Case 的最大总字节数。
pub const MAX_PLUGIN_RUNTIME_DATASET_BYTES: u64 = 8 * 1024 * 1024;
/// 单个 Case 允许的最大墙钟时间。
pub const MAX_PLUGIN_RUNTIME_CASE_TIMEOUT_MS: u64 = 30_000;
/// Evaluation WASM 单实例允许的最大 fuel。
pub const MAX_PLUGIN_EVALUATION_WASM_FUEL: u64 = 50_000_000;
/// Evaluation WASM 允许的最大协作 yield fuel 间隔。
pub const MAX_PLUGIN_EVALUATION_WASM_YIELD_INTERVAL: u64 = 250_000;
/// Evaluation WASM 单线性内存允许的最大字节数。
pub const MAX_PLUGIN_EVALUATION_WASM_MEMORY_BYTES: usize = 64 * 1024 * 1024;

/// 被评测插件与真实制品的完整身份绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginEvaluationSubject {
    /// 插件稳定 ID。
    pub plugin_id: String,
    /// Mutation ID。
    pub mutation_id: MutationId,
    /// Candidate ID。
    pub candidate_id: CandidateId,
    /// 真实 Component SHA-256。
    pub component_digest: ArtifactDigest,
    /// 完整 bundle 树 SHA-256。
    pub bundle_digest: ArtifactDigest,
}

/// 生产配置固定的 Dataset 根与 Manifest 摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedPluginEvaluationDataset {
    /// Dataset 绝对根目录。
    pub root: PathBuf,
    /// `manifest.json` 规范字节的受信摘要。
    pub manifest_digest: ArtifactDigest,
}

/// Dataset 内受摘要保护的 Case 文件引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRuntimeCaseRefV1 {
    /// Case 稳定 ID，必须与 Case 正文一致。
    pub id: String,
    /// Dataset 根内普通相对路径。
    pub path: String,
    /// Case 规范 JSON 字节摘要。
    pub digest: ArtifactDigest,
}

/// Dataset 内受摘要保护的 Fixture 文件引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRuntimeFixtureRefV1 {
    /// 相对 `fixtures/` 工作区的普通文件路径。
    pub path: String,
    /// Fixture 原始字节的 SHA-256 摘要。
    pub digest: ArtifactDigest,
}

/// 固定、版本化的插件运行评测 Dataset Manifest。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRuntimeDatasetManifestV1 {
    /// Manifest schema 版本。
    pub schema_version: u32,
    /// Dataset 稳定 ID。
    pub dataset_id: String,
    /// Dataset 语义版本。
    pub version: u32,
    /// Safety 或 Agent；Case 类型必须与之匹配。
    pub kind: PluginEvaluationKind,
    /// 按执行顺序排列的 Case 引用。
    pub cases: Vec<PluginRuntimeCaseRefV1>,
    /// Fixture Workspace 内全部普通文件的完整摘要清单。
    pub fixtures: Vec<PluginRuntimeFixtureRefV1>,
}

/// 插件运行评测执行动作。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginRuntimeActionV1 {
    /// 列出 Host 实际公开的工具。
    ListTools,
    /// 调用一个真实插件工具。
    CallTool {
        /// 工具名。
        name: String,
        /// JSON 参数。
        args: Value,
    },
    /// 列出 Host 实际公开的插件服务。
    ListServices,
    /// 调用一个真实插件服务。
    CallService {
        /// 目标插件 ID；必须等于受信 Subject 的插件 ID。
        plugin_id: String,
        /// 服务名。
        name: String,
        /// JSON 请求。
        payload: Value,
    },
}

/// 确定性 Verifier 规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginRuntimeVerifierV1 {
    /// 实际工具列表必须包含指定名称。
    ToolListed {
        /// 期望工具名。
        name: String,
    },
    /// 实际服务列表必须包含指定插件与服务名。
    ServiceListed {
        /// 期望插件 ID。
        plugin_id: String,
        /// 期望服务名。
        name: String,
    },
    /// 工具或服务实际 JSON 结果必须精确相等。
    JsonEquals {
        /// 期望 JSON。
        expected: Value,
    },
    /// 调用必须被 Host 拒绝、未路由，或返回可信错误结果。
    Rejected,
    /// 调用必须被拒绝，且 Fixture Workspace 内目标文件在调用前后保持不变。
    NoSideEffect {
        /// 相对 Fixture Workspace 的文件路径。
        path: String,
    },
}

/// 一个版本化的插件运行评测 Case。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRuntimeCaseV1 {
    /// Case schema 版本。
    pub schema_version: u32,
    /// Case 稳定 ID。
    pub id: String,
    /// 要执行的真实 Host 动作。
    pub action: PluginRuntimeActionV1,
    /// 只由 Evaluator 读取的确定性断言。
    pub verifier: PluginRuntimeVerifierV1,
    /// Case 墙钟超时；必须位于 1 到固定上限之间。
    pub timeout_ms: u64,
}

/// 报告中的稳定动作类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginRuntimeActionKindV1 {
    /// 列出工具。
    ListTools,
    /// 调用工具。
    CallTool,
    /// 列出服务。
    ListServices,
    /// 调用服务。
    CallService,
}

/// 单个 Case 的脱敏执行结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRuntimeCaseReceiptV1 {
    /// Case 稳定 ID。
    pub id: String,
    /// 实际执行的动作类别。
    pub action: PluginRuntimeActionKindV1,
    /// 确定性 Verifier 是否通过。
    pub passed: bool,
    /// 实际结构化结果的规范摘要；无结果或 Host 错误时为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual_digest: Option<ArtifactDigest>,
    /// Safety 通过时的明确拒绝来源；普通成功和失败回执为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rejection_code: Option<String>,
    /// 稳定、脱敏的失败类别；不保存 Host 原始错误或结果正文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
}

/// 完整、规范且不含原始工具/服务输出的插件运行评测报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRuntimeEvaluationReportV1 {
    /// 报告 schema 版本。
    pub schema_version: u32,
    /// Safety 或 Agent。
    pub kind: PluginEvaluationKind,
    /// 被评测插件 ID。
    pub plugin_id: String,
    /// 被评测 Mutation ID。
    pub mutation_id: MutationId,
    /// 被评测 Candidate ID。
    pub candidate_id: CandidateId,
    /// 被评测 Component 摘要。
    pub component_digest: ArtifactDigest,
    /// 被评测 bundle 摘要。
    pub bundle_digest: ArtifactDigest,
    /// Dataset Manifest 摘要。
    pub dataset_digest: ArtifactDigest,
    /// Evaluator 二进制与固定配置摘要。
    pub evaluator_revision: ArtifactDigest,
    /// 实际 Case 回执，顺序与 Manifest 一致。
    pub cases: Vec<PluginRuntimeCaseReceiptV1>,
    /// 实际执行 Case 数，由回执派生。
    pub case_count: u32,
    /// 未通过 Case 数，由回执派生。
    pub failure_count: u32,
    /// Host shutdown 完成后的 Unix 毫秒时间。
    pub completed_at_ms: u64,
}

/// 评测结果以及已写入 CAS 的完整报告引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRuntimeEvaluationOutput {
    /// 从实际回执派生的协议证据。
    pub evidence: PluginEvaluationEvidence,
    /// 完整脱敏报告。
    pub report: PluginRuntimeEvaluationReportV1,
    /// 报告 CAS 引用。
    pub report_artifact: ArtifactRef,
}

/// Host 工厂收到的受信装配请求。
#[derive(Debug, Clone)]
pub struct PluginEvaluationHostRequest {
    /// 被评测制品身份。
    pub subject: PluginEvaluationSubject,
    /// 只能进一步收紧、不能放宽的 Evaluation 执行策略。
    pub execution_policy: ExecutionPolicy,
}

/// 为一次评测创建独占 Host 的工厂边界。
#[async_trait]
pub trait PluginEvaluationHostFactory: Send + Sync {
    /// 创建真实或测试 Host；生产实现必须消费 `execution_policy` 收紧所有宿主能力。
    ///
    /// # Errors
    ///
    /// 制品身份不匹配、Host 无法加载或策略无法应用时返回错误。
    async fn create(
        &self,
        request: PluginEvaluationHostRequest,
    ) -> Result<Arc<dyn PluginHost>, PluginRuntimeEvaluationError>;
}

/// 从已安装 bundle 创建真实 WASM Host 的生产工厂。
#[derive(Debug, Clone)]
pub struct WasmPluginEvaluationHostFactory {
    bundle_root: PathBuf,
    manifest_path: PathBuf,
    limits: WasmPluginLimits,
}

impl WasmPluginEvaluationHostFactory {
    /// 固定受管理 bundle、manifest 与受信 WASM 限制；每次创建 Host 前都会重新校验。
    pub fn new(
        bundle_root: impl Into<PathBuf>,
        manifest_path: impl Into<PathBuf>,
        limits: WasmPluginLimits,
    ) -> Self {
        Self {
            bundle_root: bundle_root.into(),
            manifest_path: manifest_path.into(),
            limits,
        }
    }
}

#[async_trait]
impl PluginEvaluationHostFactory for WasmPluginEvaluationHostFactory {
    async fn create(
        &self,
        request: PluginEvaluationHostRequest,
    ) -> Result<Arc<dyn PluginHost>, PluginRuntimeEvaluationError> {
        validate_wasm_limits(&self.limits)?;
        let root = canonical_real_directory(&self.bundle_root, "插件 bundle 根")?;
        let manifest_path =
            canonical_real_file_within(&root, &self.manifest_path, "插件 manifest")?;
        let manifest = PluginManifest::load(&manifest_path).map_err(|error| {
            PluginRuntimeEvaluationError::Host(format!("读取 manifest 失败：{error}"))
        })?;
        if manifest.plugin.id != request.subject.plugin_id {
            return Err(PluginRuntimeEvaluationError::Binding(
                "manifest 插件 ID 与评测 Subject 不一致".to_string(),
            ));
        }
        let bundle_hex = hash_plugin_bundle(&root).map_err(|error| {
            PluginRuntimeEvaluationError::Host(format!("计算 bundle 摘要失败：{error}"))
        })?;
        let bundle_digest = ArtifactDigest::from_sha256_hex(bundle_hex)
            .map_err(|error| PluginRuntimeEvaluationError::Binding(error.to_string()))?;
        if bundle_digest != request.subject.bundle_digest {
            return Err(PluginRuntimeEvaluationError::Binding(
                "实际 bundle 摘要与评测 Subject 不一致".to_string(),
            ));
        }
        let wasm_relative = safe_relative_path(&manifest.plugin.wasm)?;
        let wasm_path =
            canonical_real_file_within(&root, &root.join(wasm_relative), "插件 Component")?;
        let component_bytes =
            fs::read(&wasm_path).map_err(|source| PluginRuntimeEvaluationError::Io {
                path: wasm_path.clone(),
                source,
            })?;
        if digest_bytes(&component_bytes) != request.subject.component_digest {
            return Err(PluginRuntimeEvaluationError::Binding(
                "实际 Component 摘要与评测 Subject 不一致".to_string(),
            ));
        }
        let services =
            PluginHostServices::new().restrict_execution_policy(&request.execution_policy);
        let host = WasmPluginHost::load_from_manifest_with_limits_and_services(
            &manifest_path,
            self.limits.clone(),
            services,
        )
        .await
        .map_err(|error| {
            PluginRuntimeEvaluationError::Host(format!("加载 WASM Host 失败：{error}"))
        })?;
        Ok(Arc::new(host))
    }
}

/// 校验生产 Evaluation Host 使用的 WASM 资源限制不为零且不超过固定硬上限。
fn validate_wasm_limits(limits: &WasmPluginLimits) -> Result<(), PluginRuntimeEvaluationError> {
    let yield_interval = limits.fuel_yield_interval.ok_or_else(|| {
        PluginRuntimeEvaluationError::Binding(
            "Evaluation WASM 必须配置协作 yield fuel 间隔".to_string(),
        )
    })?;
    if limits.fuel == 0
        || limits.fuel > MAX_PLUGIN_EVALUATION_WASM_FUEL
        || yield_interval == 0
        || yield_interval > MAX_PLUGIN_EVALUATION_WASM_YIELD_INTERVAL
        || yield_interval > limits.fuel
        || limits.max_memory_bytes == 0
        || limits.max_memory_bytes > MAX_PLUGIN_EVALUATION_WASM_MEMORY_BYTES
    {
        return Err(PluginRuntimeEvaluationError::Binding(
            "Evaluation WASM fuel、yield 或内存限制无效".to_string(),
        ));
    }
    Ok(())
}

/// 执行真实插件 Safety/Agent Dataset 的受信 Evaluator。
pub struct PluginRuntimeEvaluator<'a> {
    artifacts: &'a FileArtifactStore,
    host_factory: &'a dyn PluginEvaluationHostFactory,
    evaluator_revision: ArtifactDigest,
}

impl<'a> PluginRuntimeEvaluator<'a> {
    /// 固定 CAS、Host 工厂和 Evaluator 修订创建评测器。
    pub fn new(
        artifacts: &'a FileArtifactStore,
        host_factory: &'a dyn PluginEvaluationHostFactory,
        evaluator_revision: ArtifactDigest,
    ) -> Self {
        Self {
            artifacts,
            host_factory,
            evaluator_revision,
        }
    }

    /// 执行 Safety Dataset；Dataset 必须覆盖未授权工具、未授权服务和副作用拒绝。
    ///
    /// # Errors
    ///
    /// Dataset/制品绑定无效、Host 无法创建或 shutdown、报告无法写入 CAS 时返回错误。Host
    /// 创建成功后，无论用例执行是否中途失败都会调用 shutdown。
    pub async fn evaluate_safety(
        &self,
        subject: PluginEvaluationSubject,
        dataset: PinnedPluginEvaluationDataset,
    ) -> Result<PluginRuntimeEvaluationOutput, PluginRuntimeEvaluationError> {
        self.evaluate(PluginEvaluationKind::Safety, subject, dataset)
            .await
    }

    /// 执行 Agent Dataset；每个 Case 必须真实列举或调用工具/服务并验证实际结果。
    ///
    /// # Errors
    ///
    /// Dataset/制品绑定无效、Host 无法创建或 shutdown、报告无法写入 CAS 时返回错误。
    pub async fn evaluate_agent(
        &self,
        subject: PluginEvaluationSubject,
        dataset: PinnedPluginEvaluationDataset,
    ) -> Result<PluginRuntimeEvaluationOutput, PluginRuntimeEvaluationError> {
        self.evaluate(PluginEvaluationKind::Agent, subject, dataset)
            .await
    }

    /// 加载 Dataset、创建受限 Host、执行并在 shutdown 后提交报告。
    async fn evaluate(
        &self,
        kind: PluginEvaluationKind,
        subject: PluginEvaluationSubject,
        dataset: PinnedPluginEvaluationDataset,
    ) -> Result<PluginRuntimeEvaluationOutput, PluginRuntimeEvaluationError> {
        validate_subject(&subject)?;
        let loaded = load_dataset(&dataset, kind, &subject)?;
        let fixture_root = loaded.fixture_root;
        let policy = ExecutionPolicy::evaluation(&fixture_root);
        let host = self
            .host_factory
            .create(PluginEvaluationHostRequest {
                subject: subject.clone(),
                execution_policy: policy,
            })
            .await?;
        if host.id() != Some(subject.plugin_id.as_str()) {
            let shutdown = host.shutdown().await.err().map(|error| error.to_string());
            return Err(PluginRuntimeEvaluationError::HostIdentity {
                expected: subject.plugin_id,
                actual: host.id().map(str::to_string),
                shutdown_error: shutdown,
            });
        }
        let execution = run_cases(host.as_ref(), &loaded.cases, &fixture_root, &subject).await;
        let shutdown = host.shutdown().await;
        let receipts = match (execution, shutdown) {
            (Ok(receipts), Ok(())) => receipts,
            (Err(error), Ok(())) => return Err(error),
            (Ok(_), Err(error)) => {
                return Err(PluginRuntimeEvaluationError::Shutdown(error.to_string()));
            }
            (Err(primary), Err(shutdown)) => {
                return Err(PluginRuntimeEvaluationError::ExecutionAndShutdown {
                    primary: primary.to_string(),
                    shutdown: shutdown.to_string(),
                });
            }
        };
        let case_count = u32::try_from(receipts.len())
            .map_err(|_| PluginRuntimeEvaluationError::Dataset("Case 数量溢出".to_string()))?;
        let failure_count = u32::try_from(receipts.iter().filter(|case| !case.passed).count())
            .map_err(|_| PluginRuntimeEvaluationError::Dataset("失败数量溢出".to_string()))?;
        let completed_at_ms = current_time_ms()?;
        let report = PluginRuntimeEvaluationReportV1 {
            schema_version: PLUGIN_RUNTIME_REPORT_SCHEMA_VERSION,
            kind,
            plugin_id: subject.plugin_id.clone(),
            mutation_id: subject.mutation_id.clone(),
            candidate_id: subject.candidate_id.clone(),
            component_digest: subject.component_digest.clone(),
            bundle_digest: subject.bundle_digest.clone(),
            dataset_digest: loaded.manifest_digest.clone(),
            evaluator_revision: self.evaluator_revision.clone(),
            cases: receipts,
            case_count,
            failure_count,
            completed_at_ms,
        };
        validate_report(&report)?;
        let report_bytes = canonical_bytes(&report)?;
        let report_artifact = self
            .artifacts
            .put(PLUGIN_RUNTIME_REPORT_MEDIA_TYPE, &report_bytes)
            .await
            .map_err(|error| PluginRuntimeEvaluationError::Artifact(error.to_string()))?;
        let evidence = PluginEvaluationEvidence {
            schema_version: PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION,
            kind,
            plugin_id: subject.plugin_id,
            mutation_id: subject.mutation_id,
            candidate_id: subject.candidate_id,
            component_digest: subject.component_digest,
            bundle_digest: subject.bundle_digest,
            dataset_digest: loaded.manifest_digest,
            report_digest: report_artifact.digest.clone(),
            evaluator_revision: self.evaluator_revision.clone(),
            case_count,
            failure_count,
            completed_at_ms,
        };
        evidence
            .validate()
            .map_err(|error| PluginRuntimeEvaluationError::Binding(error.to_string()))?;
        Ok(PluginRuntimeEvaluationOutput {
            evidence,
            report,
            report_artifact,
        })
    }
}

/// 已完整加载并校验的内部 Dataset。
struct LoadedDataset {
    fixture_root: PathBuf,
    manifest_digest: ArtifactDigest,
    cases: Vec<PluginRuntimeCaseV1>,
}

/// 加载固定 Manifest 和所有 Case，并执行大小、摘要、路径和类型检查。
fn load_dataset(
    pinned: &PinnedPluginEvaluationDataset,
    kind: PluginEvaluationKind,
    subject: &PluginEvaluationSubject,
) -> Result<LoadedDataset, PluginRuntimeEvaluationError> {
    let root = canonical_real_directory(&pinned.root, "Dataset 根")?;
    let manifest_path = canonical_real_file_within(
        &root,
        &root.join(PLUGIN_RUNTIME_MANIFEST_FILE_NAME),
        "Dataset Manifest",
    )?;
    let manifest_bytes = read_limited(&manifest_path, MAX_PLUGIN_RUNTIME_MANIFEST_BYTES)?;
    let actual_manifest_digest = digest_bytes(&manifest_bytes);
    if actual_manifest_digest != pinned.manifest_digest {
        return Err(PluginRuntimeEvaluationError::DigestMismatch {
            path: manifest_path,
            expected: pinned.manifest_digest.clone(),
            actual: actual_manifest_digest,
        });
    }
    let manifest: PluginRuntimeDatasetManifestV1 =
        parse_canonical_json(&manifest_path, &manifest_bytes)?;
    validate_manifest(&manifest, kind)?;
    let mut total_bytes = u64::try_from(manifest_bytes.len()).unwrap_or(u64::MAX);
    let mut cases = Vec::with_capacity(manifest.cases.len());
    let mut ids = BTreeSet::new();
    for reference in &manifest.cases {
        validate_stable_name("Case ID", &reference.id)?;
        if !ids.insert(reference.id.clone()) {
            return Err(PluginRuntimeEvaluationError::Dataset(format!(
                "Dataset 包含重复 Case ID `{}`",
                reference.id
            )));
        }
        let relative = safe_relative_path(&reference.path)?;
        let path = canonical_real_file_within(&root, &root.join(relative), "Dataset Case")?;
        let bytes = read_limited(&path, MAX_PLUGIN_RUNTIME_CASE_BYTES)?;
        total_bytes = total_bytes
            .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| {
                PluginRuntimeEvaluationError::Dataset("Dataset 字节数溢出".to_string())
            })?;
        if total_bytes > MAX_PLUGIN_RUNTIME_DATASET_BYTES {
            return Err(PluginRuntimeEvaluationError::Dataset(format!(
                "Dataset 超过 {} 字节上限",
                MAX_PLUGIN_RUNTIME_DATASET_BYTES
            )));
        }
        let actual = digest_bytes(&bytes);
        if actual != reference.digest {
            return Err(PluginRuntimeEvaluationError::DigestMismatch {
                path,
                expected: reference.digest.clone(),
                actual,
            });
        }
        let case: PluginRuntimeCaseV1 = parse_canonical_json(&path, &bytes)?;
        validate_case(&case, kind, subject)?;
        if case.id != reference.id {
            return Err(PluginRuntimeEvaluationError::Dataset(format!(
                "Case 引用 `{}` 与正文 ID `{}` 不一致",
                reference.id, case.id
            )));
        }
        cases.push(case);
    }
    let fixture_root = load_fixture_workspace(&root, &manifest.fixtures, &mut total_bytes)?;
    validate_kind_coverage(kind, &cases)?;
    Ok(LoadedDataset {
        fixture_root,
        manifest_digest: actual_manifest_digest,
        cases,
    })
}

/// 校验 Manifest 固定 schema、类型和数量边界。
fn validate_manifest(
    manifest: &PluginRuntimeDatasetManifestV1,
    expected_kind: PluginEvaluationKind,
) -> Result<(), PluginRuntimeEvaluationError> {
    validate_stable_name("Dataset ID", &manifest.dataset_id)?;
    if manifest.schema_version != PLUGIN_RUNTIME_DATASET_SCHEMA_VERSION
        || manifest.version == 0
        || manifest.kind != expected_kind
        || manifest.cases.is_empty()
        || manifest.cases.len() > MAX_PLUGIN_RUNTIME_CASES
        || manifest.fixtures.len() > MAX_PLUGIN_RUNTIME_FIXTURES
    {
        return Err(PluginRuntimeEvaluationError::Dataset(
            "Dataset Manifest schema、版本、类型、Case 或 Fixture 数量无效".to_string(),
        ));
    }
    Ok(())
}

/// 校验 Case schema、动作、Verifier 组合和受信插件身份。
fn validate_case(
    case: &PluginRuntimeCaseV1,
    kind: PluginEvaluationKind,
    subject: &PluginEvaluationSubject,
) -> Result<(), PluginRuntimeEvaluationError> {
    validate_stable_name("Case ID", &case.id)?;
    if case.schema_version != PLUGIN_RUNTIME_CASE_SCHEMA_VERSION
        || case.timeout_ms == 0
        || case.timeout_ms > MAX_PLUGIN_RUNTIME_CASE_TIMEOUT_MS
    {
        return Err(PluginRuntimeEvaluationError::Dataset(format!(
            "Case `{}` schema 或超时无效",
            case.id
        )));
    }
    if let PluginRuntimeActionV1::CallService { plugin_id, .. } = &case.action {
        if plugin_id != &subject.plugin_id {
            return Err(PluginRuntimeEvaluationError::Binding(format!(
                "Case `{}` 服务目标未绑定受信插件",
                case.id
            )));
        }
    }
    let compatible = matches!(
        (&case.action, &case.verifier, kind),
        (
            PluginRuntimeActionV1::ListTools,
            PluginRuntimeVerifierV1::ToolListed { .. },
            PluginEvaluationKind::Agent,
        ) | (
            PluginRuntimeActionV1::ListServices,
            PluginRuntimeVerifierV1::ServiceListed { .. },
            PluginEvaluationKind::Agent,
        ) | (
            PluginRuntimeActionV1::CallTool { .. },
            PluginRuntimeVerifierV1::JsonEquals { .. },
            PluginEvaluationKind::Agent,
        ) | (
            PluginRuntimeActionV1::CallService { .. },
            PluginRuntimeVerifierV1::JsonEquals { .. },
            PluginEvaluationKind::Agent,
        ) | (
            PluginRuntimeActionV1::CallTool { .. },
            PluginRuntimeVerifierV1::Rejected,
            PluginEvaluationKind::Safety,
        ) | (
            PluginRuntimeActionV1::CallService { .. },
            PluginRuntimeVerifierV1::Rejected,
            PluginEvaluationKind::Safety,
        ) | (
            PluginRuntimeActionV1::CallTool { .. },
            PluginRuntimeVerifierV1::NoSideEffect { .. },
            PluginEvaluationKind::Safety,
        ) | (
            PluginRuntimeActionV1::CallService { .. },
            PluginRuntimeVerifierV1::NoSideEffect { .. },
            PluginEvaluationKind::Safety,
        )
    );
    if !compatible {
        return Err(PluginRuntimeEvaluationError::Dataset(format!(
            "Case `{}` 的动作、Verifier 与评测类型不兼容",
            case.id
        )));
    }
    if let PluginRuntimeVerifierV1::NoSideEffect { path } = &case.verifier {
        safe_relative_path(path)?;
    }
    Ok(())
}

/// Safety 必须真实覆盖工具拒绝、服务拒绝和副作用不变；Agent 必须至少执行一个调用。
fn validate_kind_coverage(
    kind: PluginEvaluationKind,
    cases: &[PluginRuntimeCaseV1],
) -> Result<(), PluginRuntimeEvaluationError> {
    let valid = match kind {
        PluginEvaluationKind::Safety => {
            let denied_tool = cases.iter().any(|case| {
                matches!(case.action, PluginRuntimeActionV1::CallTool { .. })
                    && matches!(case.verifier, PluginRuntimeVerifierV1::Rejected)
            });
            let denied_service = cases.iter().any(|case| {
                matches!(case.action, PluginRuntimeActionV1::CallService { .. })
                    && matches!(case.verifier, PluginRuntimeVerifierV1::Rejected)
            });
            let no_side_effect = cases
                .iter()
                .any(|case| matches!(case.verifier, PluginRuntimeVerifierV1::NoSideEffect { .. }));
            denied_tool && denied_service && no_side_effect
        }
        PluginEvaluationKind::Agent => cases.iter().any(|case| {
            matches!(
                case.action,
                PluginRuntimeActionV1::CallTool { .. } | PluginRuntimeActionV1::CallService { .. }
            )
        }),
    };
    if !valid {
        return Err(PluginRuntimeEvaluationError::Dataset(match kind {
            PluginEvaluationKind::Safety => {
                "Safety Dataset 必须覆盖未授权工具、未授权服务和副作用拒绝".to_string()
            }
            PluginEvaluationKind::Agent => {
                "Agent Dataset 必须至少真实调用一个工具或服务".to_string()
            }
        }));
    }
    Ok(())
}

/// 执行全部用例；结构错误中断时由上层保证 shutdown。
async fn run_cases(
    host: &dyn PluginHost,
    cases: &[PluginRuntimeCaseV1],
    fixture_root: &Path,
    subject: &PluginEvaluationSubject,
) -> Result<Vec<PluginRuntimeCaseReceiptV1>, PluginRuntimeEvaluationError> {
    let mut receipts = Vec::with_capacity(cases.len());
    for case in cases {
        let before = match &case.verifier {
            PluginRuntimeVerifierV1::NoSideEffect { path } => {
                Some(snapshot_fixture_path(fixture_root, path)?)
            }
            _ => None,
        };
        let actual = match timeout(
            Duration::from_millis(case.timeout_ms),
            execute_action(host, case, subject),
        )
        .await
        {
            Ok(actual) => actual,
            Err(_) => ActualResult::HostFailure("timeout"),
        };
        let after = match &case.verifier {
            PluginRuntimeVerifierV1::NoSideEffect { path } => {
                Some(snapshot_fixture_path(fixture_root, path)?)
            }
            _ => None,
        };
        receipts.push(verify_actual(case, actual, before, after)?);
    }
    Ok(receipts)
}

/// 实际调用 Host API，不把原始错误写入结果。
async fn execute_action(
    host: &dyn PluginHost,
    case: &PluginRuntimeCaseV1,
    subject: &PluginEvaluationSubject,
) -> ActualResult {
    match &case.action {
        PluginRuntimeActionV1::ListTools => match host.list_tools().await {
            Ok(tools) => {
                let mut names = tools.into_iter().map(|tool| tool.name).collect::<Vec<_>>();
                names.sort();
                names.dedup();
                ActualResult::ToolList(names)
            }
            Err(_) => ActualResult::HostFailure("host_error"),
        },
        PluginRuntimeActionV1::CallTool { name, args } => {
            let call = ToolCall::new(format!("evaluation:{}", case.id), name, args.clone());
            match host.before_tool(&call).await {
                Ok(ToolDecision::Allow) => execute_tool_call(host, call).await,
                Ok(ToolDecision::Rewrite { call }) => execute_tool_call(host, call).await,
                Ok(ToolDecision::Block { .. }) => {
                    let blocked = ToolResult::error(call.id, call.name, "插件工具策略拒绝评测调用");
                    if host.after_tool(&blocked).await.is_ok() {
                        ActualResult::ToolPolicyBlock
                    } else {
                        ActualResult::HostFailure("host_error")
                    }
                }
                Ok(ToolDecision::CancelRun { .. }) => {
                    let cancelled = ToolResult::error(call.id, call.name, "插件取消评测调用");
                    if host.after_tool(&cancelled).await.is_ok() {
                        ActualResult::HostFailure("cancelled")
                    } else {
                        ActualResult::HostFailure("host_error")
                    }
                }
                Err(_) => ActualResult::HostFailure("host_error"),
            }
        }
        PluginRuntimeActionV1::ListServices => match host.services().await {
            Ok(services) => {
                let mut names = services
                    .into_iter()
                    .map(|service| format!("{}/{}", service.plugin_id, service.name))
                    .collect::<Vec<_>>();
                names.sort();
                names.dedup();
                ActualResult::ServiceList(names)
            }
            Err(_) => ActualResult::HostFailure("host_error"),
        },
        PluginRuntimeActionV1::CallService {
            plugin_id,
            name,
            payload,
        } => {
            if plugin_id != &subject.plugin_id {
                return ActualResult::HostFailure("binding_error");
            }
            let registered = match host.services().await {
                Ok(services) => services
                    .iter()
                    .any(|service| service.plugin_id == *plugin_id && service.name == *name),
                Err(_) => return ActualResult::HostFailure("host_error"),
            };
            if !registered {
                return ActualResult::NotHandled;
            }
            let call = PluginServiceCall {
                caller_id: "trusted-plugin-evaluator".to_string(),
                plugin_id: plugin_id.clone(),
                name: name.clone(),
                payload: payload.clone(),
            };
            match host.call_service(&call).await {
                Ok(Some(value)) => ActualResult::ServiceResult(value),
                Ok(None) => ActualResult::NotHandled,
                Err(_) => ActualResult::HostFailure("host_error"),
            }
        }
    }
}

/// 执行已经通过前置决策的插件工具，并复现 Core 的身份重绑与后置通知语义。
async fn execute_tool_call(host: &dyn PluginHost, call: ToolCall) -> ActualResult {
    match host.call_tool(call.clone()).await {
        Ok(Some(mut result)) => {
            result.call_id = call.id;
            result.name = call.name;
            if host.after_tool(&result).await.is_err() {
                return ActualResult::HostFailure("host_error");
            }
            ActualResult::ToolResult {
                content: result.content,
                is_error: result.is_error,
            }
        }
        Ok(None) => {
            let unknown = ToolResult::error(call.id, call.name, "插件工具未注册");
            if host.after_tool(&unknown).await.is_ok() {
                ActualResult::NotHandled
            } else {
                ActualResult::HostFailure("host_error")
            }
        }
        Err(_) => ActualResult::HostFailure("host_error"),
    }
}

/// 仅存在于内存中的实际结果；报告只记录其摘要或稳定分类。
enum ActualResult {
    ToolList(Vec<String>),
    ServiceList(Vec<String>),
    ToolResult { content: Value, is_error: bool },
    ServiceResult(Value),
    ToolPolicyBlock,
    NotHandled,
    HostFailure(&'static str),
}

/// 使用确定性 Verifier 生成脱敏回执。
fn verify_actual(
    case: &PluginRuntimeCaseV1,
    actual: ActualResult,
    before: Option<FixtureSnapshot>,
    after: Option<FixtureSnapshot>,
) -> Result<PluginRuntimeCaseReceiptV1, PluginRuntimeEvaluationError> {
    let rejected = matches!(
        actual,
        ActualResult::NotHandled | ActualResult::ToolPolicyBlock
    ) || matches!(actual, ActualResult::ToolResult { is_error: true, .. });
    let passed = match (&case.verifier, &actual) {
        (PluginRuntimeVerifierV1::ToolListed { name }, ActualResult::ToolList(names)) => {
            names.contains(name)
        }
        (
            PluginRuntimeVerifierV1::ServiceListed { plugin_id, name },
            ActualResult::ServiceList(names),
        ) => names.contains(&format!("{plugin_id}/{name}")),
        (
            PluginRuntimeVerifierV1::JsonEquals { expected },
            ActualResult::ToolResult { content, is_error },
        ) => !is_error && content == expected,
        (PluginRuntimeVerifierV1::JsonEquals { expected }, ActualResult::ServiceResult(actual)) => {
            actual == expected
        }
        (PluginRuntimeVerifierV1::Rejected, _) => rejected,
        (PluginRuntimeVerifierV1::NoSideEffect { .. }, _) => {
            rejected && before.is_some() && before == after
        }
        _ => false,
    };
    let summary = summarize_actual(&actual, passed)?;
    Ok(PluginRuntimeCaseReceiptV1 {
        id: case.id.clone(),
        action: action_kind(&case.action),
        passed,
        actual_digest: summary.digest,
        rejection_code: summary.rejection_code,
        failure_code: summary.failure_code,
    })
}

/// 实际结果写入报告前的脱敏摘要。
struct ActualSummary {
    digest: Option<ArtifactDigest>,
    rejection_code: Option<String>,
    failure_code: Option<String>,
}

/// 摘要化结构化结果，并把错误压缩为稳定类别。
fn summarize_actual(
    actual: &ActualResult,
    passed: bool,
) -> Result<ActualSummary, PluginRuntimeEvaluationError> {
    let digest = match actual {
        ActualResult::ToolList(names) | ActualResult::ServiceList(names) => {
            Some(digest_bytes(&canonical_bytes(names)?))
        }
        ActualResult::ToolResult { content, .. } | ActualResult::ServiceResult(content) => {
            Some(digest_bytes(&canonical_bytes(content)?))
        }
        ActualResult::ToolPolicyBlock | ActualResult::NotHandled | ActualResult::HostFailure(_) => {
            None
        }
    };
    let rejection = if passed {
        match actual {
            ActualResult::ToolPolicyBlock => Some("tool_policy_block".to_string()),
            ActualResult::ToolResult { is_error: true, .. } => Some("tool_error".to_string()),
            ActualResult::NotHandled => Some("not_handled".to_string()),
            _ => None,
        }
    } else {
        None
    };
    let failure = if passed {
        None
    } else {
        Some(
            match actual {
                ActualResult::HostFailure(code) => *code,
                ActualResult::NotHandled => "not_handled",
                ActualResult::ToolResult { is_error: true, .. } => "tool_error",
                _ => "verification_failed",
            }
            .to_string(),
        )
    };
    Ok(ActualSummary {
        digest,
        rejection_code: rejection,
        failure_code: failure,
    })
}

/// 返回动作稳定类别。
fn action_kind(action: &PluginRuntimeActionV1) -> PluginRuntimeActionKindV1 {
    match action {
        PluginRuntimeActionV1::ListTools => PluginRuntimeActionKindV1::ListTools,
        PluginRuntimeActionV1::CallTool { .. } => PluginRuntimeActionKindV1::CallTool,
        PluginRuntimeActionV1::ListServices => PluginRuntimeActionKindV1::ListServices,
        PluginRuntimeActionV1::CallService { .. } => PluginRuntimeActionKindV1::CallService,
    }
}

/// Fixture 文件在动作前后的不可逆摘要快照。
#[derive(Debug, Clone, PartialEq, Eq)]
enum FixtureSnapshot {
    Missing,
    File { digest: ArtifactDigest, size: u64 },
}

/// 安全解析 Fixture 相对路径并读取普通文件快照。
fn snapshot_fixture_path(
    fixture_root: &Path,
    relative: &str,
) -> Result<FixtureSnapshot, PluginRuntimeEvaluationError> {
    let relative = safe_relative_path(relative)?;
    let path = fixture_root.join(relative);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FixtureSnapshot::Missing),
        Err(source) => Err(PluginRuntimeEvaluationError::Io { path, source }),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(PluginRuntimeEvaluationError::UnsafePath(path))
        }
        Ok(metadata) => {
            let canonical =
                fs::canonicalize(&path).map_err(|source| PluginRuntimeEvaluationError::Io {
                    path: path.clone(),
                    source,
                })?;
            if !canonical.starts_with(fixture_root) {
                return Err(PluginRuntimeEvaluationError::UnsafePath(path));
            }
            let bytes = read_limited(&canonical, MAX_PLUGIN_RUNTIME_DATASET_BYTES)?;
            Ok(FixtureSnapshot::File {
                digest: digest_bytes(&bytes),
                size: metadata.len(),
            })
        }
    }
}

/// 校验固定 Fixture Workspace 的完整文件集合、摘要和总字节上限。
fn load_fixture_workspace(
    dataset_root: &Path,
    references: &[PluginRuntimeFixtureRefV1],
    total_bytes: &mut u64,
) -> Result<PathBuf, PluginRuntimeEvaluationError> {
    let fixture_root =
        canonical_real_directory(&dataset_root.join("fixtures"), "Fixture Workspace")?;
    let mut expected = BTreeMap::new();
    for reference in references {
        let relative = safe_relative_path(&reference.path)?;
        let normalized = relative.to_string_lossy().into_owned();
        if expected
            .insert(normalized.clone(), reference.digest.clone())
            .is_some()
        {
            return Err(PluginRuntimeEvaluationError::Dataset(format!(
                "Dataset 包含重复 Fixture 路径 `{normalized}`"
            )));
        }
    }

    let mut actual = BTreeMap::new();
    collect_fixture_files(&fixture_root, &fixture_root, &mut actual)?;
    if actual.keys().ne(expected.keys()) {
        return Err(PluginRuntimeEvaluationError::Dataset(
            "Fixture Workspace 文件集合与 Manifest 不一致".to_string(),
        ));
    }
    for (relative, path) in actual {
        let bytes = read_limited(&path, MAX_PLUGIN_RUNTIME_DATASET_BYTES)?;
        *total_bytes = total_bytes
            .checked_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .ok_or_else(|| {
                PluginRuntimeEvaluationError::Dataset("Dataset 字节数溢出".to_string())
            })?;
        if *total_bytes > MAX_PLUGIN_RUNTIME_DATASET_BYTES {
            return Err(PluginRuntimeEvaluationError::Dataset(format!(
                "Dataset 超过 {} 字节上限",
                MAX_PLUGIN_RUNTIME_DATASET_BYTES
            )));
        }
        let actual_digest = digest_bytes(&bytes);
        let expected_digest = expected
            .get(&relative)
            .expect("文件集合相等时必须存在期望摘要");
        if &actual_digest != expected_digest {
            return Err(PluginRuntimeEvaluationError::DigestMismatch {
                path,
                expected: expected_digest.clone(),
                actual: actual_digest,
            });
        }
    }
    Ok(fixture_root)
}

/// 递归枚举 Fixture Workspace 内的普通文件并拒绝符号链接和特殊文件。
fn collect_fixture_files(
    fixture_root: &Path,
    directory: &Path,
    files: &mut BTreeMap<String, PathBuf>,
) -> Result<(), PluginRuntimeEvaluationError> {
    let entries = fs::read_dir(directory).map_err(|source| PluginRuntimeEvaluationError::Io {
        path: directory.to_path_buf(),
        source,
    })?;
    let mut paths = entries
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|source| PluginRuntimeEvaluationError::Io {
                    path: directory.to_path_buf(),
                    source,
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    for path in paths {
        let metadata =
            fs::symlink_metadata(&path).map_err(|source| PluginRuntimeEvaluationError::Io {
                path: path.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(PluginRuntimeEvaluationError::UnsafePath(path));
        }
        if metadata.is_dir() {
            collect_fixture_files(fixture_root, &path, files)?;
            continue;
        }
        if !metadata.is_file() {
            return Err(PluginRuntimeEvaluationError::UnsafePath(path));
        }
        let canonical =
            fs::canonicalize(&path).map_err(|source| PluginRuntimeEvaluationError::Io {
                path: path.clone(),
                source,
            })?;
        if !canonical.starts_with(fixture_root) {
            return Err(PluginRuntimeEvaluationError::UnsafePath(path));
        }
        let relative = canonical
            .strip_prefix(fixture_root)
            .map_err(|_| PluginRuntimeEvaluationError::UnsafePath(canonical.clone()))?
            .to_string_lossy()
            .into_owned();
        if files.insert(relative.clone(), canonical).is_some() {
            return Err(PluginRuntimeEvaluationError::Dataset(format!(
                "Fixture Workspace 包含重复文件 `{relative}`"
            )));
        }
        if files.len() > MAX_PLUGIN_RUNTIME_FIXTURES {
            return Err(PluginRuntimeEvaluationError::Dataset(format!(
                "Fixture Workspace 超过 {MAX_PLUGIN_RUNTIME_FIXTURES} 个文件上限"
            )));
        }
    }
    Ok(())
}

/// 校验 Subject 的稳定身份字段。
fn validate_subject(subject: &PluginEvaluationSubject) -> Result<(), PluginRuntimeEvaluationError> {
    validate_stable_name("插件 ID", &subject.plugin_id)
}

/// 校验报告计数和身份字段都由实际回执派生。
fn validate_report(
    report: &PluginRuntimeEvaluationReportV1,
) -> Result<(), PluginRuntimeEvaluationError> {
    let actual_cases = u32::try_from(report.cases.len()).unwrap_or(u32::MAX);
    let actual_failures =
        u32::try_from(report.cases.iter().filter(|case| !case.passed).count()).unwrap_or(u32::MAX);
    if report.schema_version != PLUGIN_RUNTIME_REPORT_SCHEMA_VERSION
        || report.cases.is_empty()
        || report.case_count != actual_cases
        || report.failure_count != actual_failures
        || report.completed_at_ms == 0
    {
        return Err(PluginRuntimeEvaluationError::Binding(
            "运行评测报告 schema、计数或时间不一致".to_string(),
        ));
    }
    Ok(())
}

/// 解析规范 JSON；带空白、字段重排或非规范转义的字节会被拒绝。
fn parse_canonical_json<T>(path: &Path, bytes: &[u8]) -> Result<T, PluginRuntimeEvaluationError>
where
    T: for<'de> Deserialize<'de> + Serialize,
{
    let value: T =
        serde_json::from_slice(bytes).map_err(|source| PluginRuntimeEvaluationError::Json {
            path: path.to_path_buf(),
            source,
        })?;
    let canonical = canonical_bytes(&value)?;
    if canonical != bytes {
        return Err(PluginRuntimeEvaluationError::Dataset(format!(
            "JSON 不是规范编码：{}",
            path.display()
        )));
    }
    Ok(value)
}

/// 使用稳定结构字段顺序生成规范 JSON。
fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>, PluginRuntimeEvaluationError> {
    serde_json::to_vec(value)
        .map_err(|error| PluginRuntimeEvaluationError::Serialization(error.to_string()))
}

/// 读取有界普通文件，读取前后都验证大小。
fn read_limited(path: &Path, maximum: u64) -> Result<Vec<u8>, PluginRuntimeEvaluationError> {
    let metadata =
        fs::symlink_metadata(path).map_err(|source| PluginRuntimeEvaluationError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PluginRuntimeEvaluationError::UnsafePath(path.to_path_buf()));
    }
    if metadata.len() > maximum {
        return Err(PluginRuntimeEvaluationError::FileTooLarge {
            path: path.to_path_buf(),
            actual: metadata.len(),
            maximum,
        });
    }
    let bytes = fs::read(path).map_err(|source| PluginRuntimeEvaluationError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if bytes.len() as u64 > maximum {
        return Err(PluginRuntimeEvaluationError::FileTooLarge {
            path: path.to_path_buf(),
            actual: bytes.len() as u64,
            maximum,
        });
    }
    Ok(bytes)
}

/// 返回真实、非符号链接绝对目录。
fn canonical_real_directory(
    path: &Path,
    label: &'static str,
) -> Result<PathBuf, PluginRuntimeEvaluationError> {
    if !path.is_absolute() {
        return Err(PluginRuntimeEvaluationError::Dataset(format!(
            "{label}必须是绝对路径：{}",
            path.display()
        )));
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|source| PluginRuntimeEvaluationError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PluginRuntimeEvaluationError::UnsafePath(path.to_path_buf()));
    }
    fs::canonicalize(path).map_err(|source| PluginRuntimeEvaluationError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// 返回根内真实普通文件，并拒绝路径链和最终目标的符号链接逃逸。
fn canonical_real_file_within(
    root: &Path,
    path: &Path,
    label: &'static str,
) -> Result<PathBuf, PluginRuntimeEvaluationError> {
    if !path.is_absolute() || !path.starts_with(root) {
        return Err(PluginRuntimeEvaluationError::Dataset(format!(
            "{label}不在受信根内：{}",
            path.display()
        )));
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| PluginRuntimeEvaluationError::UnsafePath(path.to_path_buf()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(PluginRuntimeEvaluationError::UnsafePath(path.to_path_buf()));
        };
        current.push(name);
        let metadata =
            fs::symlink_metadata(&current).map_err(|source| PluginRuntimeEvaluationError::Io {
                path: current.clone(),
                source,
            })?;
        if metadata.file_type().is_symlink() {
            return Err(PluginRuntimeEvaluationError::UnsafePath(current));
        }
    }
    let metadata =
        fs::symlink_metadata(&current).map_err(|source| PluginRuntimeEvaluationError::Io {
            path: current.clone(),
            source,
        })?;
    if !metadata.is_file() {
        return Err(PluginRuntimeEvaluationError::UnsafePath(current));
    }
    let canonical =
        fs::canonicalize(&current).map_err(|source| PluginRuntimeEvaluationError::Io {
            path: current.clone(),
            source,
        })?;
    if !canonical.starts_with(root) {
        return Err(PluginRuntimeEvaluationError::UnsafePath(current));
    }
    Ok(canonical)
}

/// 验证普通相对路径并返回 PathBuf。
fn safe_relative_path(value: &str) -> Result<PathBuf, PluginRuntimeEvaluationError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PluginRuntimeEvaluationError::UnsafePath(path.to_path_buf()));
    }
    Ok(path.to_path_buf())
}

/// 校验用于协议寻址的非空 ASCII 稳定名称。
fn validate_stable_name(
    label: &'static str,
    value: &str,
) -> Result<(), PluginRuntimeEvaluationError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(PluginRuntimeEvaluationError::Dataset(format!(
            "{label}只允许 1-128 位 ASCII 字母、数字、点、下划线或连字符"
        )));
    }
    Ok(())
}

/// 计算协议格式的 SHA-256 摘要。
fn digest_bytes(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .expect("SHA-256 输出必须符合 ArtifactDigest 格式")
}

/// 返回当前 Unix 毫秒时间。
fn current_time_ms() -> Result<u64, PluginRuntimeEvaluationError> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| PluginRuntimeEvaluationError::Clock(error.to_string()))?;
    u64::try_from(duration.as_millis())
        .map_err(|_| PluginRuntimeEvaluationError::Clock("Unix 毫秒时间溢出".to_string()))
}

/// M8 插件真实运行评测错误。
#[derive(Debug, thiserror::Error)]
pub enum PluginRuntimeEvaluationError {
    /// Dataset schema、覆盖范围或规范编码无效。
    #[error("插件运行评测 Dataset 无效：{0}")]
    Dataset(String),
    /// Subject、Host、报告或协议身份错绑。
    #[error("插件运行评测绑定无效：{0}")]
    Binding(String),
    /// 文件路径越界、是符号链接或类型错误。
    #[error("插件运行评测路径不安全：{0}")]
    UnsafePath(PathBuf),
    /// 文件超过固定读取上限。
    #[error("插件运行评测文件过大 `{path}`：{actual} 字节，上限 {maximum} 字节")]
    FileTooLarge {
        /// 文件路径。
        path: PathBuf,
        /// 实际字节数。
        actual: u64,
        /// 固定上限。
        maximum: u64,
    },
    /// Manifest 或 Case 摘要不一致。
    #[error("插件运行评测摘要不一致 `{path}`：期望 {expected}，实际 {actual}")]
    DigestMismatch {
        /// 失败文件。
        path: PathBuf,
        /// 受信摘要。
        expected: ArtifactDigest,
        /// 实际摘要。
        actual: ArtifactDigest,
    },
    /// JSON 违反 schema 或无法解析。
    #[error("插件运行评测 JSON 无效 `{path}`：{source}")]
    Json {
        /// JSON 文件。
        path: PathBuf,
        /// 解析错误。
        #[source]
        source: serde_json::Error,
    },
    /// JSON 无法规范序列化。
    #[error("插件运行评测 JSON 序列化失败：{0}")]
    Serialization(String),
    /// 文件系统操作失败。
    #[error("插件运行评测 I/O 失败 `{path}`：{source}")]
    Io {
        /// 失败路径。
        path: PathBuf,
        /// 原始错误。
        #[source]
        source: std::io::Error,
    },
    /// Host 创建、制品验证或加载失败。
    #[error("插件运行评测 Host 失败：{0}")]
    Host(String),
    /// Host 身份错绑，并保留 shutdown 失败。
    #[error("插件运行评测 Host ID 不一致：期望 {expected}，实际 {actual:?}，shutdown 错误 {shutdown_error:?}")]
    HostIdentity {
        /// 受信插件 ID。
        expected: String,
        /// Host 实际 ID。
        actual: Option<String>,
        /// shutdown 失败文本。
        shutdown_error: Option<String>,
    },
    /// Case 结构执行失败且 shutdown 也失败。
    #[error("插件运行评测执行与 shutdown 同时失败：执行 {primary}；shutdown {shutdown}")]
    ExecutionAndShutdown {
        /// 执行主错误。
        primary: String,
        /// shutdown 错误。
        shutdown: String,
    },
    /// Host shutdown 失败。
    #[error("插件运行评测 Host shutdown 失败：{0}")]
    Shutdown(String),
    /// 报告写入 CAS 失败。
    #[error("插件运行评测报告 CAS 失败：{0}")]
    Artifact(String),
    /// 系统时钟无效。
    #[error("插件运行评测时钟失败：{0}")]
    Clock(String),
}

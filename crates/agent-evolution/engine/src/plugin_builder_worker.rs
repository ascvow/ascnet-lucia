//! 已验证插件 Candidate 的隔离、固定流程构建 Worker。
//!
//! Worker 不接受任意命令、参数或构建脚本。它只消费
//! [`ValidatedPluginDependencyPlan`]，为每次构建创建独立 Cargo home 与 target，清空继承
//! 环境后注入显式白名单，并按固定顺序执行 offline/frozen 的 fmt、check、clippy、test 和
//! wasm32-wasip2 release build。每一步前后都会重验完整源码清单与 Cargo.lock。

use crate::plugin_dependency_policy::{PluginDependencyPolicyError, ValidatedPluginDependencyPlan};
use agent_evolution_protocol::{
    ArtifactDigest, CandidateId, CapabilityProfile, ComponentInterfaceSnapshot, MutationId,
    PluginBuildAttestation, PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

/// 单个 WASM Component 允许的最大字节数，与协议层上限保持一致。
pub const MAX_PLUGIN_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
/// Runner 或 Inspector 错误文本允许的最大字节数。
pub const MAX_PLUGIN_BUILDER_ERROR_BYTES: usize = 1_024;
/// 构建日志规范摘要 schema 版本。
pub const PLUGIN_BUILD_LOG_SCHEMA_VERSION: u32 = 1;
/// 构建环境规范摘要 schema 版本。
pub const PLUGIN_BUILD_ENVIRONMENT_SCHEMA_VERSION: u32 = 1;

const ENVIRONMENT_ALLOWLIST: &[&str] = &[
    "LANG",
    "LC_ALL",
    "PATH",
    "RUSTC",
    "RUSTDOC",
    "RUSTUP_HOME",
    "SOURCE_DATE_EPOCH",
    "TMPDIR",
    "TZ",
];

/// Worker 固定执行的五个构建步骤。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginBuildStep {
    /// 校验所有 Rust 文件已经格式化。
    Format,
    /// 执行原生类型检查。
    Check,
    /// 对所有 target 执行严格 Clippy。
    Clippy,
    /// 运行全部原生测试 target。
    Test,
    /// 构建 wasm32-wasip2 release Component。
    WasmRelease,
}

impl PluginBuildStep {
    fn arguments(self) -> &'static [&'static str] {
        match self {
            Self::Format => &["--offline", "--frozen", "fmt", "--all", "--", "--check"],
            Self::Check => &["--offline", "--frozen", "check"],
            Self::Clippy => &[
                "--offline",
                "--frozen",
                "clippy",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
            Self::Test => &["--offline", "--frozen", "test", "--all-targets"],
            Self::WasmRelease => &[
                "--offline",
                "--frozen",
                "build",
                "--release",
                "--target",
                "wasm32-wasip2",
            ],
        }
    }
}

const BUILD_STEPS: [PluginBuildStep; 5] = [
    PluginBuildStep::Format,
    PluginBuildStep::Check,
    PluginBuildStep::Clippy,
    PluginBuildStep::Test,
    PluginBuildStep::WasmRelease,
];

/// 传递给 [`ProcessRunner`] 的完整、不可扩展命令请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRequest {
    /// 必须为绝对路径的 Cargo 可执行文件。
    pub program: PathBuf,
    /// Worker 按固定步骤产生的参数，调用方不能注入。
    pub arguments: Vec<String>,
    /// 必须为绝对路径的插件 crate 根目录。
    pub current_directory: PathBuf,
    /// 必须为 true；真实 Runner 据此执行 `env_clear()`。
    pub clear_environment: bool,
    /// 清空环境后注入的规范有序环境映射。
    pub environment: BTreeMap<String, String>,
    /// 固定步骤的最大执行时间；真实 Runner 超时后必须 kill 并 wait。
    pub timeout: Duration,
}

/// 单个 stdout 或 stderr 流的有界日志元数据。
///
/// 实际日志正文不会进入进化记录。真实 Runner 边读边计算长度和 SHA-256，仅返回固定大小
/// 元数据，因此任意输出量都不会扩大 Worker 的持久内存或证明对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProcessStreamSummary {
    /// 原始流的完整字节长度。
    pub byte_len: u64,
    /// 原始流完整字节的 SHA-256 摘要。
    pub digest: ArtifactDigest,
}

/// 一个进程完成后的稳定退出状态与有界日志摘要。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    /// 进程是否成功退出。
    pub success: bool,
    /// 平台可提供时的退出码；被信号终止时为 None。
    pub exit_code: Option<i32>,
    /// stdout 的长度与摘要。
    pub stdout: ProcessStreamSummary,
    /// stderr 的长度与摘要。
    pub stderr: ProcessStreamSummary,
}

impl ProcessOutput {
    /// 从测试或受信 Runner 已捕获的字节构造输出。
    ///
    /// `exit_code == 0` 被视为成功；其他退出码均视为失败。此辅助构造器会立即丢弃正文，
    /// 只保存完整长度与 SHA-256。
    pub fn from_bytes(exit_code: i32, stdout: &[u8], stderr: &[u8]) -> Self {
        Self {
            success: exit_code == 0,
            exit_code: Some(exit_code),
            stdout: summarize_bytes(stdout),
            stderr: summarize_bytes(stderr),
        }
    }
}

/// ProcessRunner 的稳定、有界失败信息。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ProcessRunnerFailure {
    message: String,
}

impl ProcessRunnerFailure {
    /// 创建一个有界错误；过长消息按 UTF-8 边界截断。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: truncate_utf8(message.into(), MAX_PLUGIN_BUILDER_ERROR_BYTES),
        }
    }
}

/// 执行单个固定 Cargo 命令的边界。
///
/// 测试替身可记录请求以证明顺序、参数和环境；生产实现应使用
/// [`RealProcessRunner`]。Runner 不得解释 Candidate 文件为额外命令参数。
pub trait ProcessRunner {
    /// 执行请求并等待进程及两个输出流完全关闭。
    ///
    /// # Errors
    ///
    /// 请求边界、启动、等待或输出读取失败时返回 [`ProcessRunnerFailure`]。
    fn run(&mut self, request: &ProcessRequest) -> Result<ProcessOutput, ProcessRunnerFailure>;
}

/// 使用 `std::process::Command` 的真实同步 Runner。
#[derive(Debug, Clone, Copy, Default)]
pub struct RealProcessRunner;

impl ProcessRunner for RealProcessRunner {
    fn run(&mut self, request: &ProcessRequest) -> Result<ProcessOutput, ProcessRunnerFailure> {
        validate_process_request(request)?;
        let metadata = fs::symlink_metadata(&request.program)
            .map_err(|error| ProcessRunnerFailure::new(format!("无法验证 Cargo：{error}")))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ProcessRunnerFailure::new(
                "Cargo 可执行文件必须是非符号链接普通文件",
            ));
        }
        let mut command = Command::new(&request.program);
        command
            .args(&request.arguments)
            .current_dir(&request.current_directory)
            .env_clear()
            .envs(&request.environment)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| ProcessRunnerFailure::new(format!("启动 Cargo 失败：{error}")))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProcessRunnerFailure::new("Cargo 缺少 stdout 管道"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProcessRunnerFailure::new("Cargo 缺少 stderr 管道"))?;
        let stdout_task = thread::spawn(move || summarize_reader(stdout));
        let stderr_task = thread::spawn(move || summarize_reader(stderr));
        let started = Instant::now();
        let (status, timed_out) = loop {
            match child
                .try_wait()
                .map_err(|error| ProcessRunnerFailure::new(format!("轮询 Cargo 失败：{error}")))?
            {
                Some(status) => break (status, false),
                None if started.elapsed() >= request.timeout => {
                    let kill_error = child.kill().err();
                    let status = child.wait().map_err(|error| {
                        ProcessRunnerFailure::new(format!("超时后回收 Cargo 失败：{error}"))
                    })?;
                    if let Some(error) = kill_error {
                        return Err(ProcessRunnerFailure::new(format!(
                            "Cargo 超时且终止失败：{error}"
                        )));
                    }
                    break (status, true);
                }
                None => thread::sleep(Duration::from_millis(10)),
            }
        };
        let stdout = stdout_task
            .join()
            .map_err(|_| ProcessRunnerFailure::new("stdout 读取线程异常终止"))??;
        let stderr = stderr_task
            .join()
            .map_err(|_| ProcessRunnerFailure::new("stderr 读取线程异常终止"))??;
        if timed_out {
            return Err(ProcessRunnerFailure::new(format!(
                "Cargo 进程超过固定时限 {} 毫秒",
                request.timeout.as_millis()
            )));
        }
        Ok(ProcessOutput {
            success: status.success(),
            exit_code: status.code(),
            stdout,
            stderr,
        })
    }
}

/// 交给独立 Component 扫描器的真实产物绑定。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentInspectionRequest {
    /// 实际 WASM 文件的绝对路径。
    pub component_path: PathBuf,
    /// Worker 从实际字节计算的摘要。
    pub component_digest: ArtifactDigest,
    /// 实际 WASM 字节长度。
    pub component_size_bytes: u64,
    /// 依赖计划绑定的插件 ID。
    pub plugin_id: String,
}

/// 独立扫描器从真实 Component 重建的可信结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustedComponentInspection {
    /// 真实 Component 的 import/export 接口快照。
    pub interface: ComponentInterfaceSnapshot,
    /// 从真实 Component 和受信 manifest 规则重建的能力 Profile。
    pub capabilities: CapabilityProfile,
}

/// ComponentInspector 的稳定、有界失败信息。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct ComponentInspectorFailure {
    message: String,
}

impl ComponentInspectorFailure {
    /// 创建一个有界扫描失败说明。
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: truncate_utf8(message.into(), MAX_PLUGIN_BUILDER_ERROR_BYTES),
        }
    }
}

/// 独立扫描真实 WASM Component 的受信边界。
///
/// 实现方必须从 `component_path` 指向的实际字节重建接口与能力，不能复制 Candidate 的
/// `claimed_interface` 或 `claimed_capabilities`。
pub trait ComponentInspector {
    /// 扫描一个已由 Worker 计算摘要和长度的真实 Component。
    ///
    /// # Errors
    ///
    /// 文件无法解析、WIT 不兼容或能力无法可信重建时返回 [`ComponentInspectorFailure`]。
    fn inspect(
        &mut self,
        request: &ComponentInspectionRequest,
    ) -> Result<TrustedComponentInspection, ComponentInspectorFailure>;
}

/// 隔离 Worker 的受信静态配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginBuilderWorkerConfig {
    /// Cargo 真实可执行文件的绝对路径。
    pub cargo_executable: PathBuf,
    /// 用于创建单次 Cargo home 与 target 的绝对、非符号链接父目录。
    pub scratch_parent: PathBuf,
    /// 构建器二进制及固定配置的受信修订摘要。
    pub builder_revision: ArtifactDigest,
    /// 清空继承环境后允许注入的显式键值；键必须位于固定白名单。
    pub environment: BTreeMap<String, String>,
    /// 每个固定 Cargo 步骤的最大执行时间。
    pub step_timeout: Duration,
}

/// 单次插件构建的身份绑定。
#[derive(Debug)]
pub struct PluginBuildRequest {
    /// 依赖策略产生的不可伪造计划；请求会消费其专用物化目录。
    pub dependency_plan: ValidatedPluginDependencyPlan,
    /// 完整 PluginMutationProposal 的规范摘要。
    pub proposal_digest: ArtifactDigest,
    /// 被构建的 Mutation ID。
    pub mutation_id: MutationId,
    /// 被构建的 Candidate ID。
    pub candidate_id: CandidateId,
    /// 受信调度器分配的稳定构建 ID。
    pub build_id: String,
    /// 构建完成记录使用的 Unix 毫秒时间；必须非零。
    pub built_at_ms: u64,
}

/// 一个固定步骤的脱敏、定长日志记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuildStepRecord {
    /// 独立构建序号；当前固定为 1 或 2。
    pub attempt: u8,
    /// 固定流程中的步骤。
    pub step: PluginBuildStep,
    /// 平台可提供时的退出码。
    pub exit_code: Option<i32>,
    /// stdout 长度与摘要。
    pub stdout: ProcessStreamSummary,
    /// stderr 长度与摘要。
    pub stderr: ProcessStreamSummary,
}

/// 成功构建并完成独立扫描的结果。
#[derive(Debug)]
pub struct PluginBuildResult {
    /// 可进入签名与发布链的现有协议构建证明。
    pub attestation: PluginBuildAttestation,
    /// 按固定顺序排列的五步脱敏日志。
    pub steps: Vec<BuildStepRecord>,
    /// 已与证明摘要绑定的真实 Component 字节，供调用方写入 CAS。
    pub component_bytes: Vec<u8>,
}

/// 隔离插件构建 Worker。
#[derive(Debug)]
pub struct PluginBuilderWorker<R, I> {
    config: PluginBuilderWorkerConfig,
    runner: R,
    inspector: I,
}

impl<R, I> PluginBuilderWorker<R, I>
where
    R: ProcessRunner,
    I: ComponentInspector,
{
    /// 创建 Worker。配置会在每次构建前重新校验，避免运行期间被不安全替换。
    pub fn new(config: PluginBuilderWorkerConfig, runner: R, inspector: I) -> Self {
        Self {
            config,
            runner,
            inspector,
        }
    }

    /// 返回 Runner 引用，便于确定性测试核对实际请求。
    pub fn runner(&self) -> &R {
        &self.runner
    }

    /// 执行固定五步构建、读取真实 Component、调用独立扫描器并生成构建证明。
    ///
    /// 本方法无论成功失败都会清理单次 Cargo home、target 和已消费的 Candidate 物化目录。
    /// 任一步失败立即停止，不会继续执行后续命令或伪造 `reproducible=true`。
    ///
    /// # Errors
    ///
    /// 配置、工作区复核、进程、固定步骤、产物、扫描、协议证明或清理失败时返回
    /// [`PluginBuilderWorkerError`]。
    pub fn build(
        &mut self,
        request: PluginBuildRequest,
    ) -> Result<PluginBuildResult, PluginBuilderWorkerError> {
        let mut scratch_directories = Vec::with_capacity(2);
        let result = (|| {
            validate_config(&self.config)?;
            request.dependency_plan.revalidate_workspace()?;
            let mut records = Vec::with_capacity(BUILD_STEPS.len() * 2);
            let mut built_components = Vec::with_capacity(2);
            let mut trusted_environment_digest = None;
            for attempt in 1..=2 {
                let created = create_scratch_directory(&self.config.scratch_parent)?;
                scratch_directories.push(created.clone());
                let cargo_home = created.join("cargo-home");
                let target = created.join("target");
                fs::create_dir(&cargo_home).map_err(|source| io_error(&cargo_home, source))?;
                fs::create_dir(&target).map_err(|source| io_error(&target, source))?;
                let environment = build_environment(&self.config, &cargo_home, &target)?;
                let environment_digest =
                    build_environment_digest(&self.config, &request.dependency_plan, &environment)?;
                if let Some(previous) = &trusted_environment_digest {
                    if previous != &environment_digest {
                        return Err(PluginBuilderWorkerError::ReproducibilityMismatch);
                    }
                } else {
                    trusted_environment_digest = Some(environment_digest);
                }

                for step in BUILD_STEPS {
                    request.dependency_plan.revalidate_workspace()?;
                    let process_request = ProcessRequest {
                        program: self.config.cargo_executable.clone(),
                        arguments: step
                            .arguments()
                            .iter()
                            .map(|argument| (*argument).to_string())
                            .collect(),
                        current_directory: request.dependency_plan.crate_root().to_path_buf(),
                        clear_environment: true,
                        environment: environment.clone(),
                        timeout: self.config.step_timeout,
                    };
                    let output = self
                        .runner
                        .run(&process_request)
                        .map_err(|source| PluginBuilderWorkerError::Runner { step, source })?;
                    records.push(BuildStepRecord {
                        attempt,
                        step,
                        exit_code: output.exit_code,
                        stdout: output.stdout,
                        stderr: output.stderr,
                    });
                    request.dependency_plan.revalidate_workspace()?;
                    if !output.success {
                        return Err(PluginBuilderWorkerError::StepFailed {
                            step,
                            exit_code: output.exit_code,
                        });
                    }
                }

                let component_path = target
                    .join("wasm32-wasip2")
                    .join("release")
                    .join(request.dependency_plan.component_file_name());
                let component_bytes = read_component(&component_path)?;
                let component_digest = artifact_digest(&component_bytes)?;
                built_components.push((component_path, component_bytes, component_digest));
            }
            let second = built_components
                .pop()
                .ok_or(PluginBuilderWorkerError::ReproducibilityMismatch)?;
            let first = built_components
                .pop()
                .ok_or(PluginBuilderWorkerError::ReproducibilityMismatch)?;
            if first.2 != second.2 || first.1.len() != second.1.len() {
                return Err(PluginBuilderWorkerError::ReproducibilityMismatch);
            }
            let component_path = first.0;
            let component_bytes = first.1;
            let component_digest = first.2;
            let component_size_bytes = component_bytes.len() as u64;
            let inspection_request = ComponentInspectionRequest {
                component_path,
                component_digest: component_digest.clone(),
                component_size_bytes,
                plugin_id: request.dependency_plan.plugin_id().to_string(),
            };
            let inspection = self
                .inspector
                .inspect(&inspection_request)
                .map_err(PluginBuilderWorkerError::Inspector)?;
            inspection
                .interface
                .validate()
                .map_err(|error| PluginBuilderWorkerError::InvalidInspection(error.to_string()))?;
            inspection
                .capabilities
                .validate()
                .map_err(|error| PluginBuilderWorkerError::InvalidInspection(error.to_string()))?;
            if inspection.interface.plugin_id != inspection_request.plugin_id
                || inspection.interface.component_digest != component_digest
            {
                return Err(PluginBuilderWorkerError::InspectionBindingMismatch);
            }
            let build_log_digest = build_log_digest(&records)?;
            let attestation = PluginBuildAttestation {
                schema_version: PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION,
                build_id: request.build_id,
                plugin_id: request.dependency_plan.plugin_id().to_string(),
                mutation_id: request.mutation_id,
                candidate_id: request.candidate_id,
                proposal_digest: request.proposal_digest,
                source_digest: request.dependency_plan.source_digest().clone(),
                component_digest,
                component_size_bytes,
                interface: inspection.interface,
                capabilities: inspection.capabilities,
                build_environment_digest: trusted_environment_digest
                    .ok_or(PluginBuilderWorkerError::ReproducibilityMismatch)?,
                builder_revision: self.config.builder_revision.clone(),
                build_log_digest,
                reproducible: true,
                built_at_ms: request.built_at_ms,
            };
            attestation
                .validate()
                .map_err(|error| PluginBuilderWorkerError::InvalidAttestation(error.to_string()))?;
            Ok(PluginBuildResult {
                attestation,
                steps: records,
                component_bytes,
            })
        })();

        let cleanup = cleanup_owned_directories(&scratch_directories, &request.dependency_plan);
        match (result, cleanup) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(cleanup)) => Err(PluginBuilderWorkerError::Cleanup {
                original: None,
                cleanup,
            }),
            (Err(error), Err(cleanup)) => Err(PluginBuilderWorkerError::Cleanup {
                original: Some(error.to_string()),
                cleanup,
            }),
        }
    }
}

/// Plugin Builder Worker 的失败原因。
#[derive(Debug, thiserror::Error)]
pub enum PluginBuilderWorkerError {
    /// Worker 配置不满足绝对路径、目录或环境白名单约束。
    #[error("插件构建 Worker 配置无效：{0}")]
    InvalidConfig(String),
    /// 已验证依赖计划在构建前后复核失败。
    #[error("插件构建工作区复核失败：{0}")]
    DependencyPolicy(#[from] PluginDependencyPolicyError),
    /// 固定步骤的 Runner 无法完成进程边界。
    #[error("插件构建步骤 {step:?} 无法执行：{source}")]
    Runner {
        /// 失败步骤。
        step: PluginBuildStep,
        /// Runner 失败。
        #[source]
        source: ProcessRunnerFailure,
    },
    /// Cargo 返回非成功状态。
    #[error("插件构建步骤 {step:?} 失败，退出码 {exit_code:?}")]
    StepFailed {
        /// 失败步骤。
        step: PluginBuildStep,
        /// 平台可提供时的退出码。
        exit_code: Option<i32>,
    },
    /// 构建完成后缺少安全、非空且有界的 WASM 文件。
    #[error("插件 Component 产物无效：{0}")]
    InvalidComponent(String),
    /// 独立 Component 扫描器失败。
    #[error("插件 Component 扫描失败：{0}")]
    Inspector(#[source] ComponentInspectorFailure),
    /// 扫描结果自身协议结构不合法。
    #[error("插件 Component 扫描结果不合法：{0}")]
    InvalidInspection(String),
    /// 扫描器返回的插件身份或 Component 摘要错绑。
    #[error("插件 Component 扫描结果与真实产物错绑")]
    InspectionBindingMismatch,
    /// 两个独立 Cargo home/target 的真实 Component 摘要不一致。
    #[error("插件两次独立构建的 Component 摘要不一致")]
    ReproducibilityMismatch,
    /// 现有 PluginBuildAttestation 协议拒绝构建结果。
    #[error("插件构建证明不合法：{0}")]
    InvalidAttestation(String),
    /// 文件系统操作失败。
    #[error("插件构建文件系统操作失败：{path}: {source}")]
    Io {
        /// 操作路径。
        path: PathBuf,
        /// 原始错误。
        #[source]
        source: std::io::Error,
    },
    /// 摘要规范序列化失败。
    #[error("序列化插件构建证明输入失败：{0}")]
    Serialization(serde_json::Error),
    /// SHA-256 无法转换为强类型摘要。
    #[error("构造插件构建摘要失败：{0}")]
    DigestConstruction(String),
    /// 构建结果已产生或已失败，但临时目录未能完全清理。
    #[error("插件构建目录清理失败；原始错误：{original:?}；清理错误：{cleanup}")]
    Cleanup {
        /// 清理前的构建错误；成功后清理失败时为 None。
        original: Option<String>,
        /// 有界清理错误。
        cleanup: String,
    },
}

#[derive(Serialize)]
struct BuildEnvironmentDigestPayload<'a> {
    domain: &'static str,
    schema_version: u32,
    cargo_executable: String,
    builder_revision: &'a ArtifactDigest,
    dependency_digest: &'a ArtifactDigest,
    cargo_lock_digest: &'a ArtifactDigest,
    source_digest: &'a ArtifactDigest,
    target: &'static str,
    release: bool,
    offline: bool,
    frozen: bool,
    clear_environment: bool,
    step_timeout_ms: u64,
    commands: Vec<Vec<&'static str>>,
    configured_environment: &'a BTreeMap<String, String>,
    injected_environment: BTreeMap<&'static str, &'static str>,
}

#[derive(Serialize)]
struct BuildLogDigestPayload<'a> {
    domain: &'static str,
    schema_version: u32,
    records: &'a [BuildStepRecord],
}

fn validate_config(config: &PluginBuilderWorkerConfig) -> Result<(), PluginBuilderWorkerError> {
    if !config.cargo_executable.is_absolute() {
        return Err(PluginBuilderWorkerError::InvalidConfig(
            "Cargo 可执行文件必须是绝对路径".to_string(),
        ));
    }
    if !config.scratch_parent.is_absolute() {
        return Err(PluginBuilderWorkerError::InvalidConfig(
            "临时目录父路径必须是绝对路径".to_string(),
        ));
    }
    if config.step_timeout.is_zero() || config.step_timeout > Duration::from_secs(60 * 60) {
        return Err(PluginBuilderWorkerError::InvalidConfig(
            "单步超时必须位于 1 纳秒到 1 小时之间".to_string(),
        ));
    }
    let metadata = fs::symlink_metadata(&config.scratch_parent)
        .map_err(|source| io_error(&config.scratch_parent, source))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(PluginBuilderWorkerError::InvalidConfig(
            "临时目录父路径必须是非符号链接目录".to_string(),
        ));
    }
    for (key, value) in &config.environment {
        if !ENVIRONMENT_ALLOWLIST.contains(&key.as_str()) {
            return Err(PluginBuilderWorkerError::InvalidConfig(format!(
                "环境变量 `{key}` 不在白名单"
            )));
        }
        if key.contains('\0') || value.contains('\0') || value.len() > 16 * 1024 {
            return Err(PluginBuilderWorkerError::InvalidConfig(format!(
                "环境变量 `{key}` 不合法"
            )));
        }
    }
    Ok(())
}

fn build_environment(
    config: &PluginBuilderWorkerConfig,
    cargo_home: &Path,
    target: &Path,
) -> Result<BTreeMap<String, String>, PluginBuilderWorkerError> {
    let mut environment = config.environment.clone();
    environment.insert("CARGO_HOME".to_string(), path_string(cargo_home)?);
    environment.insert("CARGO_TARGET_DIR".to_string(), path_string(target)?);
    environment.insert("CARGO_NET_OFFLINE".to_string(), "true".to_string());
    environment.insert("CARGO_TERM_COLOR".to_string(), "never".to_string());
    environment.insert("RUST_BACKTRACE".to_string(), "0".to_string());
    Ok(environment)
}

fn build_environment_digest(
    config: &PluginBuilderWorkerConfig,
    plan: &ValidatedPluginDependencyPlan,
    environment: &BTreeMap<String, String>,
) -> Result<ArtifactDigest, PluginBuilderWorkerError> {
    let required_keys = [
        "CARGO_HOME",
        "CARGO_NET_OFFLINE",
        "CARGO_TARGET_DIR",
        "CARGO_TERM_COLOR",
        "RUST_BACKTRACE",
    ];
    if !required_keys
        .iter()
        .all(|key| environment.contains_key(*key))
    {
        return Err(PluginBuilderWorkerError::InvalidConfig(
            "构建环境缺少 Worker 固定变量".to_string(),
        ));
    }
    let bytes = serde_json::to_vec(&BuildEnvironmentDigestPayload {
        domain: "ascnet.lucia.plugin-build-environment.v1",
        schema_version: PLUGIN_BUILD_ENVIRONMENT_SCHEMA_VERSION,
        cargo_executable: path_string(&config.cargo_executable)?,
        builder_revision: &config.builder_revision,
        dependency_digest: plan.dependency_digest(),
        cargo_lock_digest: plan.cargo_lock_digest(),
        source_digest: plan.source_digest(),
        target: "wasm32-wasip2",
        release: true,
        offline: true,
        frozen: true,
        clear_environment: true,
        step_timeout_ms: config.step_timeout.as_millis() as u64,
        commands: BUILD_STEPS
            .iter()
            .map(|step| step.arguments().to_vec())
            .collect(),
        configured_environment: &config.environment,
        injected_environment: BTreeMap::from([
            ("CARGO_NET_OFFLINE", "true"),
            ("CARGO_TERM_COLOR", "never"),
            ("RUST_BACKTRACE", "0"),
        ]),
    })
    .map_err(PluginBuilderWorkerError::Serialization)?;
    artifact_digest(&bytes)
}

fn build_log_digest(
    records: &[BuildStepRecord],
) -> Result<ArtifactDigest, PluginBuilderWorkerError> {
    let bytes = serde_json::to_vec(&BuildLogDigestPayload {
        domain: "ascnet.lucia.plugin-build-log.v1",
        schema_version: PLUGIN_BUILD_LOG_SCHEMA_VERSION,
        records,
    })
    .map_err(PluginBuilderWorkerError::Serialization)?;
    artifact_digest(&bytes)
}

fn create_scratch_directory(parent: &Path) -> Result<PathBuf, PluginBuilderWorkerError> {
    for _ in 0..8 {
        let path = parent.join(format!(".lucia-plugin-build-{}", Uuid::new_v4().simple()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(io_error(&path, source)),
        }
    }
    Err(PluginBuilderWorkerError::InvalidConfig(
        "无法分配唯一构建临时目录".to_string(),
    ))
}

fn cleanup_owned_directories(
    scratch_directories: &[PathBuf],
    plan: &ValidatedPluginDependencyPlan,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for path in scratch_directories {
        if let Err(error) = remove_owned_path(path) {
            errors.push(format!("清理构建临时目录失败：{error}"));
        }
    }
    if let Err(error) = remove_owned_path(plan.workspace_root()) {
        errors.push(format!("清理 Candidate 工作区失败：{error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(truncate_utf8(
            errors.join("；"),
            MAX_PLUGIN_BUILDER_ERROR_BYTES,
        ))
    }
}

fn remove_owned_path(path: &Path) -> Result<(), std::io::Error> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn read_component(path: &Path) -> Result<Vec<u8>, PluginBuilderWorkerError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| PluginBuilderWorkerError::InvalidComponent(error.to_string()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PluginBuilderWorkerError::InvalidComponent(
            "产物必须是非符号链接普通文件".to_string(),
        ));
    }
    if metadata.len() == 0 || metadata.len() > MAX_PLUGIN_COMPONENT_BYTES {
        return Err(PluginBuilderWorkerError::InvalidComponent(format!(
            "产物大小 {} 超出 1..={MAX_PLUGIN_COMPONENT_BYTES}",
            metadata.len()
        )));
    }
    let file = fs::File::open(path)
        .map_err(|error| PluginBuilderWorkerError::InvalidComponent(error.to_string()))?;
    let mut bytes = Vec::new();
    file.take(MAX_PLUGIN_COMPONENT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| PluginBuilderWorkerError::InvalidComponent(error.to_string()))?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_PLUGIN_COMPONENT_BYTES {
        return Err(PluginBuilderWorkerError::InvalidComponent(
            "读取期间 Component 大小越界".to_string(),
        ));
    }
    Ok(bytes)
}

fn validate_process_request(request: &ProcessRequest) -> Result<(), ProcessRunnerFailure> {
    if !request.program.is_absolute()
        || !request.current_directory.is_absolute()
        || !request.clear_environment
        || request.timeout.is_zero()
    {
        return Err(ProcessRunnerFailure::new(
            "进程请求必须使用绝对路径并清空环境",
        ));
    }
    Ok(())
}

fn summarize_reader(mut reader: impl Read) -> Result<ProcessStreamSummary, ProcessRunnerFailure> {
    let mut hasher = Sha256::new();
    let mut byte_len = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| ProcessRunnerFailure::new(format!("读取 Cargo 输出失败：{error}")))?;
        if read == 0 {
            break;
        }
        byte_len = byte_len
            .checked_add(read as u64)
            .ok_or_else(|| ProcessRunnerFailure::new("Cargo 输出长度溢出"))?;
        hasher.update(&buffer[..read]);
    }
    let digest = ArtifactDigest::from_sha256_hex(format!("{:x}", hasher.finalize()))
        .map_err(|error| ProcessRunnerFailure::new(format!("构造输出摘要失败：{error}")))?;
    Ok(ProcessStreamSummary { byte_len, digest })
}

fn summarize_bytes(bytes: &[u8]) -> ProcessStreamSummary {
    let digest = ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .expect("SHA-256 十六进制必须符合 ArtifactDigest 格式");
    ProcessStreamSummary {
        byte_len: bytes.len() as u64,
        digest,
    }
}

fn artifact_digest(bytes: &[u8]) -> Result<ArtifactDigest, PluginBuilderWorkerError> {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .map_err(|error| PluginBuilderWorkerError::DigestConstruction(error.to_string()))
}

fn path_string(path: &Path) -> Result<String, PluginBuilderWorkerError> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| PluginBuilderWorkerError::InvalidConfig("路径必须是 UTF-8".to_string()))
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value
}

fn io_error(path: &Path, source: std::io::Error) -> PluginBuilderWorkerError {
    PluginBuilderWorkerError::Io {
        path: path.to_path_buf(),
        source,
    }
}

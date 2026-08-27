//! M8 插件真实 Safety/Agent 运行评测测试。

use agent_core::AgentExtension;
use agent_evaluation::*;
use agent_evolution::{ArtifactStore, FileArtifactStore};
use agent_evolution_protocol::{ArtifactDigest, CandidateId, MutationId, PluginEvaluationKind};
use agent_plugin_host::{wasm::WasmPluginLimits, PluginHost, PluginService, PluginServiceCall};
use agent_plugin_manager::hash_plugin_bundle;
use agent_tool::{ExecutionProfile, ToolCall, ToolDecision, ToolResult, ToolSpec};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tempfile::{tempdir, TempDir};

/// 持有临时 Dataset 生命周期和受信 Manifest 摘要。
struct TestDataset {
    _directory: TempDir,
    pinned: PinnedPluginEvaluationDataset,
}

/// 固定行为的 Plugin Host，用于证明 Evaluator 只根据真实调用回执形成结论。
struct MockPluginHost {
    id: &'static str,
    shutdowns: AtomicUsize,
    before_calls: AtomicUsize,
    tool_calls: AtomicUsize,
    after_calls: AtomicUsize,
    mutate_path: Mutex<Option<PathBuf>>,
    shutdown_fails: AtomicBool,
}

impl MockPluginHost {
    /// 创建默认成功且记录生命周期调用的测试 Host。
    fn new() -> Self {
        Self {
            id: "echo",
            shutdowns: AtomicUsize::new(0),
            before_calls: AtomicUsize::new(0),
            tool_calls: AtomicUsize::new(0),
            after_calls: AtomicUsize::new(0),
            mutate_path: Mutex::new(None),
            shutdown_fails: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl AgentExtension for MockPluginHost {
    /// 公开测试需要的真实工具清单。
    async fn list_tools(&self) -> Result<Vec<ToolSpec>> {
        Ok(["echo", "blocked", "mutate", "error", "explode", "slow"]
            .into_iter()
            .map(|name| ToolSpec::new(name, "评测测试工具", ToolSpec::empty_object_schema()))
            .collect())
    }

    /// 对 `blocked` 返回明确策略阻断，其余调用保持或按测试要求重写。
    async fn before_tool(&self, call: &ToolCall) -> Result<ToolDecision> {
        self.before_calls.fetch_add(1, Ordering::SeqCst);
        match call.name.as_str() {
            "blocked" => Ok(ToolDecision::Block {
                reason: "private-block-reason".to_string(),
            }),
            "rewrite" => Ok(ToolDecision::Rewrite {
                call: ToolCall::new(call.id.clone(), "echo", call.args.clone()),
            }),
            _ => Ok(ToolDecision::Allow),
        }
    }

    /// 返回实际结构化结果，或按名称模拟未处理和 Host 错误。
    async fn call_tool(&self, call: ToolCall) -> Result<Option<ToolResult>> {
        self.tool_calls.fetch_add(1, Ordering::SeqCst);
        match call.name.as_str() {
            "echo" => Ok(Some(ToolResult::success(
                call.id,
                call.name,
                json!({
                    "echo": call.args.get("text").cloned().unwrap_or(Value::Null),
                    "private": "sensitive-output"
                }),
            ))),
            "error" => Ok(Some(ToolResult::error(
                call.id,
                call.name,
                "private-tool-error",
            ))),
            "explode" => Err(anyhow!("private-host-error")),
            "slow" => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok(None)
            }
            "mutate" => {
                if let Some(path) = self.mutate_path.lock().expect("路径锁不应中毒").take() {
                    fs::remove_file(&path)?;
                    fs::create_dir(&path)?;
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    /// 记录 Core 语义要求的工具后置通知。
    async fn after_tool(&self, _result: &ToolResult) -> Result<()> {
        self.after_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl PluginHost for MockPluginHost {
    /// 返回与受信 Subject 一致的 Host 身份。
    fn id(&self) -> Option<&str> {
        Some(self.id)
    }

    /// 公开一个测试服务用于列表和真实结果校验。
    async fn services(&self) -> Result<Vec<PluginService>> {
        Ok(vec![PluginService {
            plugin_id: self.id.to_string(),
            name: "echo-service".to_string(),
            version: "1.0.0".to_string(),
            description: None,
        }])
    }

    /// 未注册服务返回 `None`，Host 崩溃返回错误，其他服务返回结构化结果。
    async fn call_service(&self, call: &PluginServiceCall) -> Result<Option<Value>> {
        match call.name.as_str() {
            "missing" => Ok(None),
            "explode" => Err(anyhow!("private-service-error")),
            _ => Ok(Some(
                json!({"service": call.payload, "private": "service-secret"}),
            )),
        }
    }

    /// 记录 shutdown，并允许测试双重错误保留。
    async fn shutdown(&self) -> Result<()> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        if self.shutdown_fails.load(Ordering::SeqCst) {
            Err(anyhow!("private-shutdown-error"))
        } else {
            Ok(())
        }
    }
}

/// 捕获受信装配请求并返回独占测试 Host 的工厂。
struct MockHostFactory {
    host: Arc<MockPluginHost>,
    requests: Mutex<Vec<PluginEvaluationHostRequest>>,
}

impl MockHostFactory {
    /// 以指定 Host 创建捕获工厂。
    fn new(host: Arc<MockPluginHost>) -> Self {
        Self {
            host,
            requests: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl PluginEvaluationHostFactory for MockHostFactory {
    /// 保存 Evaluator 派生的请求，调用方不能替换其中的执行策略。
    async fn create(
        &self,
        request: PluginEvaluationHostRequest,
    ) -> std::result::Result<Arc<dyn PluginHost>, PluginRuntimeEvaluationError> {
        self.requests.lock().expect("请求锁不应中毒").push(request);
        Ok(self.host.clone())
    }
}

/// 创建固定身份和制品摘要的被评测 Subject。
fn subject() -> PluginEvaluationSubject {
    PluginEvaluationSubject {
        plugin_id: "echo".to_string(),
        mutation_id: MutationId::generate(),
        candidate_id: CandidateId::generate(),
        component_digest: repeated_digest('a'),
        bundle_digest: repeated_digest('b'),
    }
}

/// 创建一个固定 Evaluator 修订摘要。
fn evaluator_revision() -> ArtifactDigest {
    repeated_digest('e')
}

/// 用重复十六进制字符构造合法测试摘要。
fn repeated_digest(character: char) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(character.to_string().repeat(64)).expect("测试摘要必须合法")
}

/// 计算与生产协议相同的 SHA-256 Artifact 摘要。
fn digest_bytes(bytes: &[u8]) -> ArtifactDigest {
    ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
        .expect("SHA-256 摘要必须合法")
}

/// 序列化规范 JSON 测试字节。
fn canonical_bytes(value: &impl Serialize) -> Vec<u8> {
    serde_json::to_vec(value).expect("测试 JSON 必须可序列化")
}

/// 写入摘要固定、文件集合完整声明的测试 Dataset。
fn write_dataset(
    kind: PluginEvaluationKind,
    cases: Vec<PluginRuntimeCaseV1>,
    fixtures: Vec<(String, Vec<u8>)>,
) -> TestDataset {
    let directory = tempdir().expect("创建临时 Dataset");
    fs::create_dir(directory.path().join("cases")).expect("创建 Case 目录");
    fs::create_dir(directory.path().join("fixtures")).expect("创建 Fixture 目录");

    let case_refs = cases
        .iter()
        .map(|case| {
            let relative = format!("cases/{}.json", case.id);
            let bytes = canonical_bytes(case);
            fs::write(directory.path().join(&relative), &bytes).expect("写入 Case");
            PluginRuntimeCaseRefV1 {
                id: case.id.clone(),
                path: relative,
                digest: digest_bytes(&bytes),
            }
        })
        .collect();
    let fixture_refs = fixtures
        .into_iter()
        .map(|(relative, bytes)| {
            let path = directory.path().join("fixtures").join(&relative);
            fs::create_dir_all(path.parent().expect("Fixture 必须有父目录"))
                .expect("创建 Fixture 父目录");
            fs::write(&path, &bytes).expect("写入 Fixture");
            PluginRuntimeFixtureRefV1 {
                path: relative,
                digest: digest_bytes(&bytes),
            }
        })
        .collect();
    let manifest = PluginRuntimeDatasetManifestV1 {
        schema_version: PLUGIN_RUNTIME_DATASET_SCHEMA_VERSION,
        dataset_id: format!("{}-dataset", kind_name(kind)),
        version: 1,
        kind,
        cases: case_refs,
        fixtures: fixture_refs,
    };
    let manifest_bytes = canonical_bytes(&manifest);
    fs::write(
        directory.path().join(PLUGIN_RUNTIME_MANIFEST_FILE_NAME),
        &manifest_bytes,
    )
    .expect("写入 Manifest");
    let root = directory.path().canonicalize().expect("规范化 Dataset 根");
    TestDataset {
        _directory: directory,
        pinned: PinnedPluginEvaluationDataset {
            root,
            manifest_digest: digest_bytes(&manifest_bytes),
        },
    }
}

/// 返回评测类型的稳定测试名称。
fn kind_name(kind: PluginEvaluationKind) -> &'static str {
    match kind {
        PluginEvaluationKind::Safety => "safety",
        PluginEvaluationKind::Agent => "agent",
    }
}

/// 创建固定超时的测试 Case。
fn runtime_case(
    id: &str,
    action: PluginRuntimeActionV1,
    verifier: PluginRuntimeVerifierV1,
) -> PluginRuntimeCaseV1 {
    PluginRuntimeCaseV1 {
        schema_version: PLUGIN_RUNTIME_CASE_SCHEMA_VERSION,
        id: id.to_string(),
        action,
        verifier,
        timeout_ms: 5_000,
    }
}

/// 读取、修改并重新固定 Manifest，供路径和数量边界测试使用。
fn rewrite_manifest(
    dataset: &mut TestDataset,
    update: impl FnOnce(&mut PluginRuntimeDatasetManifestV1),
) {
    let path = dataset.pinned.root.join(PLUGIN_RUNTIME_MANIFEST_FILE_NAME);
    let mut manifest: PluginRuntimeDatasetManifestV1 =
        serde_json::from_slice(&fs::read(&path).expect("读取 Manifest")).expect("解析 Manifest");
    update(&mut manifest);
    let bytes = canonical_bytes(&manifest);
    fs::write(&path, &bytes).expect("重写 Manifest");
    dataset.pinned.manifest_digest = digest_bytes(&bytes);
}

/// 替换一个 Case 的原始字节并同步其 Manifest 摘要。
fn rewrite_case_bytes(dataset: &mut TestDataset, index: usize, bytes: &[u8]) {
    let manifest_path = dataset.pinned.root.join(PLUGIN_RUNTIME_MANIFEST_FILE_NAME);
    let mut manifest: PluginRuntimeDatasetManifestV1 =
        serde_json::from_slice(&fs::read(&manifest_path).expect("读取 Manifest"))
            .expect("解析 Manifest");
    let reference = manifest.cases.get_mut(index).expect("Case 引用必须存在");
    fs::write(dataset.pinned.root.join(&reference.path), bytes).expect("重写 Case");
    reference.digest = digest_bytes(bytes);
    let manifest_bytes = canonical_bytes(&manifest);
    fs::write(&manifest_path, &manifest_bytes).expect("重写 Manifest");
    dataset.pinned.manifest_digest = digest_bytes(&manifest_bytes);
}

/// 创建一个具备全部三类覆盖的 Safety Dataset。
fn safety_dataset() -> TestDataset {
    write_dataset(
        PluginEvaluationKind::Safety,
        vec![
            runtime_case(
                "blocked-tool",
                PluginRuntimeActionV1::CallTool {
                    name: "blocked".to_string(),
                    args: json!({}),
                },
                PluginRuntimeVerifierV1::Rejected,
            ),
            runtime_case(
                "missing-service",
                PluginRuntimeActionV1::CallService {
                    plugin_id: "echo".to_string(),
                    name: "missing".to_string(),
                    payload: json!({}),
                },
                PluginRuntimeVerifierV1::Rejected,
            ),
            runtime_case(
                "no-side-effect",
                PluginRuntimeActionV1::CallTool {
                    name: "blocked".to_string(),
                    args: json!({}),
                },
                PluginRuntimeVerifierV1::NoSideEffect {
                    path: "guard.txt".to_string(),
                },
            ),
        ],
        vec![("guard.txt".to_string(), b"unchanged".to_vec())],
    )
}

/// Agent 评测必须绑定真实 list/call 回执、Evidence 身份和 CAS 报告。
#[tokio::test]
async fn agent_evaluation_binds_actual_receipts_evidence_and_cas() {
    let expected = json!({"echo": "hello", "private": "sensitive-output"});
    let dataset = write_dataset(
        PluginEvaluationKind::Agent,
        vec![
            runtime_case(
                "list-tools",
                PluginRuntimeActionV1::ListTools,
                PluginRuntimeVerifierV1::ToolListed {
                    name: "echo".to_string(),
                },
            ),
            runtime_case(
                "call-echo",
                PluginRuntimeActionV1::CallTool {
                    name: "echo".to_string(),
                    args: json!({"text": "hello"}),
                },
                PluginRuntimeVerifierV1::JsonEquals {
                    expected: expected.clone(),
                },
            ),
        ],
        Vec::new(),
    );
    let host = Arc::new(MockPluginHost::new());
    let factory = MockHostFactory::new(host.clone());
    let cas = tempdir().expect("创建临时 CAS");
    let artifacts = FileArtifactStore::new(cas.path());
    let evaluated_subject = subject();
    let output = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(evaluated_subject.clone(), dataset.pinned.clone())
        .await
        .expect("Agent 评测应通过");

    assert_eq!(output.report.failure_count, 0);
    assert_eq!(output.report.case_count, 2);
    assert_eq!(
        output.report.cases[1].actual_digest,
        Some(digest_bytes(&canonical_bytes(&expected)))
    );
    assert_eq!(output.evidence.plugin_id, evaluated_subject.plugin_id);
    assert_eq!(output.evidence.mutation_id, evaluated_subject.mutation_id);
    assert_eq!(output.evidence.candidate_id, evaluated_subject.candidate_id);
    assert_eq!(output.evidence.report_digest, output.report_artifact.digest);
    let stored = artifacts
        .get(&output.report_artifact.digest)
        .await
        .expect("读取报告 CAS")
        .expect("报告必须存在");
    assert_eq!(stored, canonical_bytes(&output.report));
    assert!(!String::from_utf8_lossy(&stored).contains("sensitive-output"));
    assert_eq!(host.before_calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.after_calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.shutdowns.load(Ordering::SeqCst), 1);
}

/// 实际失败计数必须由回执派生，报告不得保存 Host 原始输出、错误或输入。
#[tokio::test]
async fn actual_failures_derive_counts_without_leaking_host_data() {
    let dataset = write_dataset(
        PluginEvaluationKind::Agent,
        vec![
            runtime_case(
                "wrong-result",
                PluginRuntimeActionV1::CallTool {
                    name: "echo".to_string(),
                    args: json!({"text": "secret-input"}),
                },
                PluginRuntimeVerifierV1::JsonEquals {
                    expected: json!({"expected": "different"}),
                },
            ),
            runtime_case(
                "host-error",
                PluginRuntimeActionV1::CallTool {
                    name: "explode".to_string(),
                    args: json!({}),
                },
                PluginRuntimeVerifierV1::JsonEquals {
                    expected: json!({}),
                },
            ),
        ],
        Vec::new(),
    );
    let host = Arc::new(MockPluginHost::new());
    let factory = MockHostFactory::new(host);
    let cas = tempdir().expect("创建临时 CAS");
    let artifacts = FileArtifactStore::new(cas.path());
    let output = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(subject(), dataset.pinned)
        .await
        .expect("Case 失败应形成报告而非伪造执行错误");

    assert_eq!(output.report.failure_count, 2);
    assert_eq!(output.evidence.failure_count, 2);
    assert_eq!(
        output.report.cases[0].failure_code.as_deref(),
        Some("verification_failed")
    );
    assert_eq!(
        output.report.cases[1].failure_code.as_deref(),
        Some("host_error")
    );
    let report_text = serde_json::to_string(&output.report).expect("序列化报告");
    assert!(!report_text.contains("sensitive-output"));
    assert!(!report_text.contains("private-host-error"));
    assert!(!report_text.contains("secret-input"));
}

/// Safety 必须覆盖工具拒绝、服务拒绝、无副作用并装配 Evaluation Policy。
#[tokio::test]
async fn safety_requires_explicit_rejections_and_evaluation_policy() {
    let dataset = safety_dataset();
    let expected_fixture_root = dataset.pinned.root.join("fixtures");
    let host = Arc::new(MockPluginHost::new());
    let factory = MockHostFactory::new(host.clone());
    let cas = tempdir().expect("创建临时 CAS");
    let artifacts = FileArtifactStore::new(cas.path());
    let output = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_safety(subject(), dataset.pinned)
        .await
        .expect("Safety 三类明确拒绝应通过");

    assert_eq!(output.report.failure_count, 0);
    assert_eq!(
        output.report.cases[0].rejection_code.as_deref(),
        Some("tool_policy_block")
    );
    assert_eq!(
        output.report.cases[1].rejection_code.as_deref(),
        Some("not_handled")
    );
    assert_eq!(
        output.report.cases[2].rejection_code.as_deref(),
        Some("tool_policy_block")
    );
    let requests = factory.requests.lock().expect("请求锁不应中毒");
    let policy = &requests[0].execution_policy;
    assert_eq!(policy.profile(), ExecutionProfile::Evaluation);
    assert!(!policy.permits_network_access());
    assert!(!policy.permits_secret_access());
    assert!(!policy.permits_process_execution());
    assert_eq!(
        policy.filesystem,
        agent_tool::FilesystemScope::Root(expected_fixture_root)
    );
    assert_eq!(host.tool_calls.load(Ordering::SeqCst), 0);
    assert_eq!(host.shutdowns.load(Ordering::SeqCst), 1);
}

/// Host 错误和 timeout 都不能伪装成 Safety 明确拒绝。
#[tokio::test]
async fn host_failures_and_timeouts_cannot_pass_rejected_verifier() {
    let mut timeout_case = runtime_case(
        "timeout",
        PluginRuntimeActionV1::CallTool {
            name: "slow".to_string(),
            args: json!({}),
        },
        PluginRuntimeVerifierV1::Rejected,
    );
    timeout_case.timeout_ms = 1;
    let dataset = write_dataset(
        PluginEvaluationKind::Safety,
        vec![
            timeout_case,
            runtime_case(
                "host-error",
                PluginRuntimeActionV1::CallTool {
                    name: "explode".to_string(),
                    args: json!({}),
                },
                PluginRuntimeVerifierV1::Rejected,
            ),
            runtime_case(
                "missing-service",
                PluginRuntimeActionV1::CallService {
                    plugin_id: "echo".to_string(),
                    name: "missing".to_string(),
                    payload: json!({}),
                },
                PluginRuntimeVerifierV1::Rejected,
            ),
            runtime_case(
                "no-side-effect",
                PluginRuntimeActionV1::CallTool {
                    name: "blocked".to_string(),
                    args: json!({}),
                },
                PluginRuntimeVerifierV1::NoSideEffect {
                    path: "guard.txt".to_string(),
                },
            ),
        ],
        vec![("guard.txt".to_string(), b"fixed".to_vec())],
    );
    let host = Arc::new(MockPluginHost::new());
    let factory = MockHostFactory::new(host);
    let cas = tempdir().expect("创建临时 CAS");
    let artifacts = FileArtifactStore::new(cas.path());
    let output = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_safety(subject(), dataset.pinned)
        .await
        .expect("Host 错误应形成失败回执");

    assert_eq!(output.report.failure_count, 2);
    assert_eq!(
        output.report.cases[0].failure_code.as_deref(),
        Some("timeout")
    );
    assert_eq!(
        output.report.cases[1].failure_code.as_deref(),
        Some("host_error")
    );
    assert!(output.report.cases[0].rejection_code.is_none());
    assert!(output.report.cases[1].rejection_code.is_none());
}

/// 未知字段和摘要篡改必须在 Host 创建前失败。
#[tokio::test]
async fn unknown_fields_and_digest_tampering_are_rejected_before_host_creation() {
    let mut unknown = write_dataset(
        PluginEvaluationKind::Agent,
        vec![runtime_case(
            "echo",
            PluginRuntimeActionV1::CallTool {
                name: "echo".to_string(),
                args: json!({"text": "hello"}),
            },
            PluginRuntimeVerifierV1::JsonEquals {
                expected: json!({}),
            },
        )],
        Vec::new(),
    );
    let mut value: Value = serde_json::from_slice(
        &fs::read(unknown.pinned.root.join("cases/echo.json")).expect("读取 Case"),
    )
    .expect("解析 Case");
    value
        .as_object_mut()
        .expect("Case 必须是对象")
        .insert("untrusted_verdict".to_string(), json!("pass"));
    rewrite_case_bytes(&mut unknown, 0, &canonical_bytes(&value));

    let host = Arc::new(MockPluginHost::new());
    let factory = MockHostFactory::new(host);
    let cas = tempdir().expect("创建临时 CAS");
    let artifacts = FileArtifactStore::new(cas.path());
    let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(subject(), unknown.pinned)
        .await
        .expect_err("未知字段必须拒绝");
    assert!(matches!(error, PluginRuntimeEvaluationError::Json { .. }));
    assert!(factory.requests.lock().expect("请求锁不应中毒").is_empty());

    let tampered = write_dataset(
        PluginEvaluationKind::Agent,
        vec![runtime_case(
            "echo",
            PluginRuntimeActionV1::CallTool {
                name: "echo".to_string(),
                args: json!({}),
            },
            PluginRuntimeVerifierV1::JsonEquals {
                expected: json!({}),
            },
        )],
        Vec::new(),
    );
    fs::write(tampered.pinned.root.join("cases/echo.json"), b"{}").expect("篡改 Case");
    let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(subject(), tampered.pinned)
        .await
        .expect_err("Case 摘要篡改必须拒绝");
    assert!(matches!(
        error,
        PluginRuntimeEvaluationError::DigestMismatch { .. }
    ));

    let manifest = write_dataset(
        PluginEvaluationKind::Agent,
        vec![runtime_case(
            "echo",
            PluginRuntimeActionV1::CallTool {
                name: "echo".to_string(),
                args: json!({}),
            },
            PluginRuntimeVerifierV1::JsonEquals {
                expected: json!({}),
            },
        )],
        Vec::new(),
    );
    fs::write(
        manifest.pinned.root.join(PLUGIN_RUNTIME_MANIFEST_FILE_NAME),
        b"{}",
    )
    .expect("篡改 Manifest");
    let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(subject(), manifest.pinned)
        .await
        .expect_err("Manifest 摘要篡改必须拒绝");
    assert!(matches!(
        error,
        PluginRuntimeEvaluationError::DigestMismatch { .. }
    ));
}

/// 绝对路径、父目录、symlink 和 Fixture 篡改必须失败关闭。
#[tokio::test]
async fn unsafe_paths_symlinks_and_fixture_tampering_are_rejected() {
    let base_case = || {
        runtime_case(
            "echo",
            PluginRuntimeActionV1::CallTool {
                name: "echo".to_string(),
                args: json!({}),
            },
            PluginRuntimeVerifierV1::JsonEquals {
                expected: json!({}),
            },
        )
    };
    let host = Arc::new(MockPluginHost::new());
    let factory = MockHostFactory::new(host);
    let cas = tempdir().expect("创建临时 CAS");
    let artifacts = FileArtifactStore::new(cas.path());

    for unsafe_path in ["/tmp/outside.json", "../outside.json"] {
        let mut dataset = write_dataset(PluginEvaluationKind::Agent, vec![base_case()], Vec::new());
        rewrite_manifest(&mut dataset, |manifest| {
            manifest.cases[0].path = unsafe_path.to_string();
        });
        let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
            .evaluate_agent(subject(), dataset.pinned)
            .await
            .expect_err("不安全路径必须拒绝");
        assert!(matches!(error, PluginRuntimeEvaluationError::UnsafePath(_)));
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let mut dataset = write_dataset(PluginEvaluationKind::Agent, vec![base_case()], Vec::new());
        let outside = tempdir().expect("创建根外目录");
        let outside_case = outside.path().join("outside.json");
        let bytes = canonical_bytes(&base_case());
        fs::write(&outside_case, &bytes).expect("写入根外 Case");
        let link = dataset.pinned.root.join("cases/link.json");
        symlink(&outside_case, &link).expect("创建 Case 符号链接");
        rewrite_manifest(&mut dataset, |manifest| {
            manifest.cases[0].path = "cases/link.json".to_string();
            manifest.cases[0].digest = digest_bytes(&bytes);
        });
        let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
            .evaluate_agent(subject(), dataset.pinned)
            .await
            .expect_err("符号链接逃逸必须拒绝");
        assert!(matches!(error, PluginRuntimeEvaluationError::UnsafePath(_)));
    }

    let fixture = write_dataset(
        PluginEvaluationKind::Agent,
        vec![base_case()],
        vec![("guard.txt".to_string(), b"original".to_vec())],
    );
    fs::write(fixture.pinned.root.join("fixtures/guard.txt"), b"tampered").expect("篡改 Fixture");
    let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(subject(), fixture.pinned)
        .await
        .expect_err("Fixture 摘要篡改必须拒绝");
    assert!(matches!(
        error,
        PluginRuntimeEvaluationError::DigestMismatch { .. }
    ));

    let unlisted = write_dataset(PluginEvaluationKind::Agent, vec![base_case()], Vec::new());
    fs::write(
        unlisted.pinned.root.join("fixtures/unlisted.txt"),
        b"untrusted",
    )
    .expect("写入未声明 Fixture");
    let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(subject(), unlisted.pinned)
        .await
        .expect_err("未声明 Fixture 必须拒绝");
    assert!(matches!(error, PluginRuntimeEvaluationError::Dataset(_)));
}

/// Case 数、单文件字节和 Dataset 总字节上限必须失败关闭。
#[tokio::test]
async fn case_count_and_byte_limits_fail_closed() {
    let mut too_many = write_dataset(
        PluginEvaluationKind::Agent,
        vec![runtime_case(
            "echo",
            PluginRuntimeActionV1::CallTool {
                name: "echo".to_string(),
                args: json!({}),
            },
            PluginRuntimeVerifierV1::JsonEquals {
                expected: json!({}),
            },
        )],
        Vec::new(),
    );
    rewrite_manifest(&mut too_many, |manifest| {
        let template = manifest.cases[0].clone();
        manifest.cases = (0..=MAX_PLUGIN_RUNTIME_CASES)
            .map(|index| PluginRuntimeCaseRefV1 {
                id: format!("case-{index}"),
                ..template.clone()
            })
            .collect();
    });
    let host = Arc::new(MockPluginHost::new());
    let factory = MockHostFactory::new(host);
    let cas = tempdir().expect("创建临时 CAS");
    let artifacts = FileArtifactStore::new(cas.path());
    let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(subject(), too_many.pinned)
        .await
        .expect_err("Case 数量上限必须强制执行");
    assert!(matches!(error, PluginRuntimeEvaluationError::Dataset(_)));

    let mut oversized = write_dataset(
        PluginEvaluationKind::Agent,
        vec![runtime_case(
            "echo",
            PluginRuntimeActionV1::CallTool {
                name: "echo".to_string(),
                args: json!({}),
            },
            PluginRuntimeVerifierV1::JsonEquals {
                expected: json!({}),
            },
        )],
        Vec::new(),
    );
    rewrite_case_bytes(
        &mut oversized,
        0,
        &vec![b'x'; MAX_PLUGIN_RUNTIME_CASE_BYTES as usize + 1],
    );
    let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(subject(), oversized.pinned)
        .await
        .expect_err("Case 字节上限必须强制执行");
    assert!(matches!(
        error,
        PluginRuntimeEvaluationError::FileTooLarge { .. }
    ));

    let total = write_dataset(
        PluginEvaluationKind::Agent,
        vec![runtime_case(
            "echo",
            PluginRuntimeActionV1::CallTool {
                name: "echo".to_string(),
                args: json!({}),
            },
            PluginRuntimeVerifierV1::JsonEquals {
                expected: json!({}),
            },
        )],
        vec![(
            "blob.bin".to_string(),
            vec![0; MAX_PLUGIN_RUNTIME_DATASET_BYTES as usize],
        )],
    );
    let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(subject(), total.pinned)
        .await
        .expect_err("Dataset 总字节上限必须强制执行");
    assert!(matches!(error, PluginRuntimeEvaluationError::Dataset(_)));
}

/// 结构执行错误仍必须 shutdown，并保留主错误与 shutdown 双重错误。
#[tokio::test]
async fn structural_execution_error_still_shutdowns_and_preserves_both_errors() {
    let dataset = write_dataset(
        PluginEvaluationKind::Safety,
        vec![
            runtime_case(
                "mutate-fixture",
                PluginRuntimeActionV1::CallTool {
                    name: "mutate".to_string(),
                    args: json!({}),
                },
                PluginRuntimeVerifierV1::NoSideEffect {
                    path: "guard.txt".to_string(),
                },
            ),
            runtime_case(
                "blocked-tool",
                PluginRuntimeActionV1::CallTool {
                    name: "blocked".to_string(),
                    args: json!({}),
                },
                PluginRuntimeVerifierV1::Rejected,
            ),
            runtime_case(
                "missing-service",
                PluginRuntimeActionV1::CallService {
                    plugin_id: "echo".to_string(),
                    name: "missing".to_string(),
                    payload: json!({}),
                },
                PluginRuntimeVerifierV1::Rejected,
            ),
        ],
        vec![("guard.txt".to_string(), b"fixed".to_vec())],
    );
    let host = Arc::new(MockPluginHost::new());
    *host.mutate_path.lock().expect("路径锁不应中毒") =
        Some(dataset.pinned.root.join("fixtures/guard.txt"));
    host.shutdown_fails.store(true, Ordering::SeqCst);
    let factory = MockHostFactory::new(host.clone());
    let cas = tempdir().expect("创建临时 CAS");
    let artifacts = FileArtifactStore::new(cas.path());
    let error = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_safety(subject(), dataset.pinned)
        .await
        .expect_err("Fixture 类型突变必须中止评测");

    assert!(matches!(
        error,
        PluginRuntimeEvaluationError::ExecutionAndShutdown { .. }
    ));
    assert_eq!(host.shutdowns.load(Ordering::SeqCst), 1);
}

/// 非零、显式、受硬上限约束的 WASM limits 必须在 bundle 访问前校验。
#[tokio::test]
async fn invalid_explicit_wasm_limits_are_rejected_before_bundle_access() {
    let factory = WasmPluginEvaluationHostFactory::new(
        PathBuf::from("/nonexistent/bundle"),
        PathBuf::from("/nonexistent/bundle/plugin.toml"),
        WasmPluginLimits {
            fuel: 0,
            fuel_yield_interval: None,
            max_memory_bytes: 0,
        },
    );
    let result = factory
        .create(PluginEvaluationHostRequest {
            subject: subject(),
            execution_policy: agent_tool::ExecutionPolicy::evaluation("/tmp/fixture"),
        })
        .await;
    let error = match result {
        Ok(_) => panic!("零资源限制必须拒绝"),
        Err(error) => error,
    };
    assert!(matches!(error, PluginRuntimeEvaluationError::Binding(_)));
}

/// 真实 Echo Component 必须完成 Agent 工具调用和 Safety 前置阻断。
#[tokio::test]
#[ignore = "需要预构建 examples/plugins/echo-plugin 的真实 WASM Component"]
async fn real_echo_component_runs_agent_and_safety_evaluations() {
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("规范化仓库根");
    let bundle_root = repository_root.join("examples/plugins/echo-plugin");
    let manifest_path = bundle_root.join("plugin.toml");
    let component_path = bundle_root.join("target/wasm32-wasip2/release/echo_plugin.wasm");
    assert!(component_path.is_file(), "真实 Echo Component 尚未构建");
    let real_subject = PluginEvaluationSubject {
        plugin_id: "echo".to_string(),
        mutation_id: MutationId::generate(),
        candidate_id: CandidateId::generate(),
        component_digest: digest_bytes(&fs::read(&component_path).expect("读取 Echo Component")),
        bundle_digest: ArtifactDigest::from_sha256_hex(
            hash_plugin_bundle(&bundle_root).expect("计算 Echo bundle 摘要"),
        )
        .expect("Echo bundle 摘要必须合法"),
    };
    let factory = WasmPluginEvaluationHostFactory::new(
        &bundle_root,
        &manifest_path,
        WasmPluginLimits {
            fuel: MAX_PLUGIN_EVALUATION_WASM_FUEL,
            fuel_yield_interval: Some(MAX_PLUGIN_EVALUATION_WASM_YIELD_INTERVAL),
            max_memory_bytes: MAX_PLUGIN_EVALUATION_WASM_MEMORY_BYTES,
        },
    );
    let cas = tempdir().expect("创建真实 E2E CAS");
    let artifacts = FileArtifactStore::new(cas.path());

    let agent = write_dataset(
        PluginEvaluationKind::Agent,
        vec![runtime_case(
            "real-echo",
            PluginRuntimeActionV1::CallTool {
                name: "echo".to_string(),
                args: json!({"text": "真实 Agent 评测"}),
            },
            PluginRuntimeVerifierV1::JsonEquals {
                expected: json!({
                    "echo": "真实 Agent 评测",
                    "source": "wasm-plugin",
                    "calls_seen": 1,
                    "events_seen": 0
                }),
            },
        )],
        Vec::new(),
    );
    let agent_output = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_agent(real_subject.clone(), agent.pinned)
        .await
        .expect("真实 Echo Agent 评测应通过");
    assert_eq!(
        agent_output.report.failure_count, 0,
        "真实 Echo Agent 回执：{:?}",
        agent_output.report.cases
    );
    assert!(agent_output.report.cases[0].actual_digest.is_some());

    let safety = write_dataset(
        PluginEvaluationKind::Safety,
        vec![
            runtime_case(
                "empty-echo-blocked",
                PluginRuntimeActionV1::CallTool {
                    name: "echo".to_string(),
                    args: json!({"text": "   "}),
                },
                PluginRuntimeVerifierV1::Rejected,
            ),
            runtime_case(
                "missing-service",
                PluginRuntimeActionV1::CallService {
                    plugin_id: "echo".to_string(),
                    name: "missing".to_string(),
                    payload: json!({}),
                },
                PluginRuntimeVerifierV1::Rejected,
            ),
            runtime_case(
                "empty-echo-no-side-effect",
                PluginRuntimeActionV1::CallTool {
                    name: "echo".to_string(),
                    args: json!({"text": ""}),
                },
                PluginRuntimeVerifierV1::NoSideEffect {
                    path: "guard.txt".to_string(),
                },
            ),
        ],
        vec![("guard.txt".to_string(), b"fixed".to_vec())],
    );
    let safety_output = PluginRuntimeEvaluator::new(&artifacts, &factory, evaluator_revision())
        .evaluate_safety(real_subject, safety.pinned)
        .await
        .expect("真实 Echo Safety 评测应通过");
    assert_eq!(
        safety_output.report.failure_count, 0,
        "真实 Echo Safety 回执：{:?}",
        safety_output.report.cases
    );
    assert_eq!(
        safety_output.report.cases[0].rejection_code.as_deref(),
        Some("tool_policy_block")
    );
    assert_eq!(
        safety_output.report.cases[1].rejection_code.as_deref(),
        Some("not_handled")
    );
}

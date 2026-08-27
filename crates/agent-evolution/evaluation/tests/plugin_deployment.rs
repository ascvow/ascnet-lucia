mod release_support {
    use agent_evaluation::{
        evaluate_plugin_source, FilePluginReleaseArchive, PluginCanaryAdmissionV1,
        PluginReleaseArchiveRecordV1, PluginReleaseController, PluginRollbackRequestV1,
        TrustedPluginKeyring, TrustedPluginSigner,
    };
    use agent_evolution::FileArtifactStore;
    use agent_evolution_protocol::{
        ArtifactDigest, CandidateId, ComponentInterfaceSnapshot, EpisodeId, EvaluationReportId,
        EvolutionCycleId, GenomeDigest, MutationId, PluginAuditCheck, PluginBuildAttestation,
        PluginCanaryRecord, PluginCanaryState, PluginEvaluationEvidence, PluginEvaluationGateInput,
        PluginEvaluationKind, PluginEvaluationReport, PluginFilePatch, PluginHostAuditEvidence,
        PluginMutationKind, PluginMutationProposal, PluginReleaseEnvelope, PluginReleaseStage,
        PluginSourceArtifact, PluginSourceFile, PreapprovedPluginProfile, ReleaseId,
        SignaturePurpose, COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
        PLUGIN_AUDIT_CHECK_SCHEMA_VERSION, PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION,
        PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION, PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION,
        PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION, PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION,
        PLUGIN_RELEASE_ENVELOPE_SCHEMA_VERSION,
    };
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    /// 完整 M8 Candidate、Gate 报告及其真实制品字节。
    struct ReleaseFixture {
        input: PluginEvaluationGateInput,
        report: PluginEvaluationReport,
        component: Vec<u8>,
        bundle: Vec<u8>,
    }

    /// 三类用途隔离的真实 Ed25519 签名器与公钥 Keyring。
    struct SigningFixture {
        build_signer: TrustedPluginSigner,
        release_signer: TrustedPluginSigner,
        build_keys: TrustedPluginKeyring,
        approval_keys: TrustedPluginKeyring,
        release_keys: TrustedPluginKeyring,
    }

    impl SigningFixture {
        /// 创建固定私钥种子但用途隔离的测试签名控制面。
        fn new() -> Self {
            let build_signer = TrustedPluginSigner::from_secret_bytes(
                "m8-deployment-builder-v1",
                SignaturePurpose::BuildAttestation,
                &[41; 32],
            )
            .expect("构建签名器应创建成功");
            let release_signer = TrustedPluginSigner::from_secret_bytes(
                "m8-deployment-release-v1",
                SignaturePurpose::PluginRelease,
                &[42; 32],
            )
            .expect("发布签名器应创建成功");
            let mut build_keys = TrustedPluginKeyring::new();
            build_keys
                .insert(build_signer.verifying_key())
                .expect("构建公钥应登记成功");
            let mut release_keys = TrustedPluginKeyring::new();
            release_keys
                .insert(release_signer.verifying_key())
                .expect("发布公钥应登记成功");
            Self {
                build_signer,
                release_signer,
                build_keys,
                approval_keys: TrustedPluginKeyring::new(),
                release_keys,
            }
        }

        /// 为 Candidate 生成用途隔离、摘要精确绑定的 Release 信封。
        fn release(
            &self,
            fixture: &ReleaseFixture,
            stage: PluginReleaseStage,
            lineage: Option<ReleaseId>,
            rollback_target: Option<ArtifactDigest>,
            issued_at_ms: u64,
        ) -> PluginReleaseEnvelope {
            let attestation = fixture.input.build_attestation.clone();
            let attestation_signature = self
                .build_signer
                .sign(
                    fixture.input.proposal.plugin_id.clone(),
                    fixture.input.proposal.mutation_id.clone(),
                    attestation.digest().expect("构建证明摘要应可计算"),
                    attestation.built_at_ms + 1,
                    100_000,
                )
                .expect("构建证明应完成真实签名");
            let (canary_of, rollback_of) = match stage {
                PluginReleaseStage::Canary => (None, None),
                PluginReleaseStage::Stable => (lineage, None),
                PluginReleaseStage::Rollback => (None, lineage),
            };
            let mut release = PluginReleaseEnvelope {
                schema_version: PLUGIN_RELEASE_ENVELOPE_SCHEMA_VERSION,
                release_id: ReleaseId::generate(),
                stage,
                plugin_id: fixture.input.proposal.plugin_id.clone(),
                mutation_id: fixture.input.proposal.mutation_id.clone(),
                candidate_id: fixture.input.proposal.candidate_id.clone(),
                proposal_digest: fixture.input.proposal.digest().expect("提案摘要应可计算"),
                source_digest: fixture
                    .input
                    .proposal
                    .candidate_source
                    .digest()
                    .expect("源码摘要应可计算"),
                bundle_digest: fixture.input.bundle_digest.clone(),
                evaluation_report_digest: fixture
                    .report
                    .digest_for_input(&fixture.input)
                    .expect("Gate 报告摘要应可计算"),
                attestation,
                attestation_signature: attestation_signature.clone(),
                baseline_capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
                expansion_request: None,
                approval: None,
                canary_of,
                rollback_of,
                rollback_target_component_digest: rollback_target,
                issued_at_ms,
                // 外层签名不进入 Release signing digest，先使用结构合法信封占位。
                signature: attestation_signature,
            };
            let signing_digest = release.signing_digest().expect("Release 摘要应可计算");
            release.signature = self
                .release_signer
                .sign(
                    release.plugin_id.clone(),
                    release.mutation_id.clone(),
                    signing_digest,
                    issued_at_ms,
                    100_000,
                )
                .expect("Release 应完成真实签名");
            release.validate().expect("签名后的 Release 应合法");
            release
        }
    }

    /// 计算真实字节的协议 SHA-256 摘要。
    fn bytes_digest(bytes: &[u8]) -> ArtifactDigest {
        ArtifactDigest::from_sha256_hex(format!("{:x}", Sha256::digest(bytes)))
            .expect("SHA-256 摘要应合法")
    }

    /// 构造固定 Genome 摘要。
    fn genome_digest(seed: u8) -> GenomeDigest {
        GenomeDigest::from_sha256_hex(format!("{seed:02x}").repeat(32)).expect("Genome 摘要应合法")
    }

    /// 构造一项结构自洽的受信审计检查。
    fn audit(seed: u8, completed_at_ms: u64) -> PluginAuditCheck {
        PluginAuditCheck {
            schema_version: PLUGIN_AUDIT_CHECK_SCHEMA_VERSION,
            report_digest: bytes_digest(&[seed]),
            verifier_revision: bytes_digest(b"m8-deployment-audit-verifier-v1"),
            passed: true,
            check_count: 3,
            failure_count: 0,
            completed_at_ms,
        }
    }

    /// 生成与真实 Component 字节和可选 Parent 精确绑定的完整 Gate Fixture。
    fn release_fixture(
        tag: u8,
        base_time_ms: u64,
        parent: Option<&ReleaseFixture>,
    ) -> ReleaseFixture {
        let source_bytes = format!("pub fn run() -> u8 {{ {tag} }}").into_bytes();
        let source_digest = bytes_digest(&source_bytes);
        let source = PluginSourceArtifact::new(
            "example.plugin",
            vec![PluginSourceFile {
                path: "src/lib.rs".into(),
                digest: source_digest.clone(),
                size_bytes: source_bytes.len() as u64,
            }],
        )
        .expect("Candidate 源码清单应合法");
        let (mutation, patches, parent_genome_digest) = match parent {
            Some(parent) => {
                let parent_source = parent.input.proposal.candidate_source.clone();
                let parent_digest = parent_source.files[0].digest.clone();
                (
                    PluginMutationKind::Update {
                        parent_source: Box::new(parent_source),
                        parent_capabilities: Box::new(
                            parent.input.build_attestation.capabilities.clone(),
                        ),
                    },
                    vec![PluginFilePatch::Update {
                        path: "src/lib.rs".into(),
                        old_digest: parent_digest,
                        new_digest: source_digest,
                    }],
                    parent.input.proposal.candidate_genome_digest.clone(),
                )
            }
            None => (
                PluginMutationKind::Create {
                    preapproved_profile: PreapprovedPluginProfile::PureCompute,
                },
                vec![PluginFilePatch::Create {
                    path: "src/lib.rs".into(),
                    new_digest: source_digest,
                }],
                genome_digest(1),
            ),
        };
        let component = vec![0, b'a', b's', b'm', tag, 1, 2, 3];
        let bundle = format!("bundle-v1-{tag}").into_bytes();
        let component_digest = bytes_digest(&component);
        let bundle_digest = bytes_digest(&bundle);
        let interface = ComponentInterfaceSnapshot {
            schema_version: COMPONENT_INTERFACE_SNAPSHOT_SCHEMA_VERSION,
            plugin_id: "example.plugin".into(),
            component_digest: component_digest.clone(),
            world: "example:plugin/world@1.0.0".into(),
            imports: Vec::new(),
            exports: vec!["example:plugin/run".into()],
            scanner_revision: bytes_digest(b"m8-deployment-interface-scanner-v1"),
        };
        let proposal = PluginMutationProposal {
            schema_version: PLUGIN_MUTATION_PROPOSAL_SCHEMA_VERSION,
            cycle_id: EvolutionCycleId::generate(),
            mutation_id: MutationId::generate(),
            candidate_id: CandidateId::generate(),
            plugin_id: "example.plugin".into(),
            parent_genome_digest,
            candidate_genome_digest: genome_digest(tag.saturating_add(2)),
            mutation,
            candidate_source: source,
            patches,
            claimed_capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
            claimed_interface: interface.clone(),
            evidence_episode_ids: vec![EpisodeId::generate()],
            rationale: "根据可信失败证据生成受限插件 Candidate".into(),
            created_at_ms: base_time_ms,
        };
        let build_attestation = PluginBuildAttestation {
            schema_version: PLUGIN_BUILD_ATTESTATION_SCHEMA_VERSION,
            build_id: format!("build-m8-deployment-{tag}"),
            plugin_id: proposal.plugin_id.clone(),
            mutation_id: proposal.mutation_id.clone(),
            candidate_id: proposal.candidate_id.clone(),
            proposal_digest: proposal.digest().expect("提案摘要应可计算"),
            source_digest: proposal
                .candidate_source
                .digest()
                .expect("源码摘要应可计算"),
            component_digest: component_digest.clone(),
            component_size_bytes: component.len() as u64,
            interface: interface.clone(),
            capabilities: PreapprovedPluginProfile::PureCompute.capabilities(),
            build_environment_digest: bytes_digest(b"m8-deployment-build-environment-v1"),
            builder_revision: bytes_digest(b"m8-deployment-builder-v1"),
            build_log_digest: bytes_digest(&[tag, 9]),
            reproducible: true,
            built_at_ms: base_time_ms + 10,
        };
        let host_audit = PluginHostAuditEvidence {
            schema_version: PLUGIN_HOST_AUDIT_EVIDENCE_SCHEMA_VERSION,
            plugin_id: proposal.plugin_id.clone(),
            mutation_id: proposal.mutation_id.clone(),
            candidate_id: proposal.candidate_id.clone(),
            component_digest: component_digest.clone(),
            manifest_digest: bytes_digest(&[tag, 1]),
            interface_digest: interface.digest().expect("接口摘要应可计算"),
            capability_profile_digest: build_attestation
                .capabilities
                .digest()
                .expect("能力摘要应可计算"),
            bundle_digest: bundle_digest.clone(),
            host_smoke: audit(tag, base_time_ms + 20),
            manifest_audit: audit(tag.saturating_add(1), base_time_ms + 21),
            import_audit: audit(tag.saturating_add(2), base_time_ms + 22),
            interface_audit: audit(tag.saturating_add(3), base_time_ms + 23),
            owner_audit: audit(tag.saturating_add(4), base_time_ms + 24),
            runtime_audit: audit(tag.saturating_add(5), base_time_ms + 25),
        };
        let evaluation = |kind, suffix, completed_at_ms| PluginEvaluationEvidence {
            schema_version: PLUGIN_EVALUATION_EVIDENCE_SCHEMA_VERSION,
            kind,
            plugin_id: proposal.plugin_id.clone(),
            mutation_id: proposal.mutation_id.clone(),
            candidate_id: proposal.candidate_id.clone(),
            component_digest: component_digest.clone(),
            bundle_digest: bundle_digest.clone(),
            dataset_digest: bytes_digest(b"m8-deployment-dataset-v1"),
            report_digest: bytes_digest(&[tag, suffix]),
            evaluator_revision: bytes_digest(b"m8-deployment-evaluator-v1"),
            case_count: 12,
            failure_count: 0,
            completed_at_ms,
        };
        let safety_evaluation = evaluation(PluginEvaluationKind::Safety, 7, base_time_ms + 30);
        let agent_evaluation = evaluation(PluginEvaluationKind::Agent, 8, base_time_ms + 31);
        let input = PluginEvaluationGateInput {
            schema_version: PLUGIN_EVALUATION_GATE_INPUT_SCHEMA_VERSION,
            report_id: EvaluationReportId::generate(),
            proposal,
            build_attestation,
            bundle_digest,
            host_audit,
            safety_evaluation,
            agent_evaluation,
            evaluated_at_ms: base_time_ms + 40,
        };
        let report = evaluate_plugin_source(&input).expect("完整证据应通过源码 Gate");
        ReleaseFixture {
            input,
            report,
            component,
            bundle,
        }
    }

    /// 从 Planned 快照生成结构合法的 Running 观察。
    fn running_canary(planned: &PluginCanaryRecord, started_at_ms: u64) -> PluginCanaryRecord {
        let mut running = planned.clone();
        running.state = PluginCanaryState::Running;
        running.started_at_ms = Some(started_at_ms);
        running
    }

    /// 从 Running 快照生成成功或失败的真实健康终态。
    fn terminal_canary(
        running: &PluginCanaryRecord,
        succeeded: bool,
        finished_at_ms: u64,
    ) -> PluginCanaryRecord {
        let mut terminal = running.clone();
        terminal.state = if succeeded {
            PluginCanaryState::Succeeded
        } else {
            PluginCanaryState::Failed
        };
        terminal.finished_at_ms = Some(finished_at_ms);
        terminal.observed_runs = 2;
        terminal.passed_runs = u64::from(succeeded) * 2 + u64::from(!succeeded);
        terminal.failed_runs = u64::from(!succeeded);
        terminal.health_report_digest = Some(bytes_digest(if succeeded {
            b"m8-deployment-health-succeeded"
        } else {
            b"m8-deployment-health-failed"
        }));
        terminal
    }

    mod deployment_tests {
        use super::*;
        use agent_evaluation::{
            FilePluginDeploymentStore, PersistentPluginDeploymentController,
            PluginDeploymentController, PluginDeploymentError, PluginDeploymentId,
            PluginDeploymentStateV1, PluginPromotionReceipt,
        };
        use agent_evolution::{
            FileGenomeResolver, FileStableGenomePublisher, GenomeResolver, GenomeSelector,
            GenomeStore,
        };
        use agent_evolution_protocol::{
            AgentGenome, GenomeMetadata, GenomeRevision, ModelGenome, PluginGenome, PromptGenome,
            RuntimeIdentity, ToolProfileGenome, GENOME_SCHEMA_VERSION,
        };
        use agent_plugin_manager::{pack_plugin_bundle, PluginManager};
        use std::{
            collections::{BTreeMap, BTreeSet},
            path::{Path, PathBuf},
        };

        /// 创建真实插件 bundle 目录并返回确定性归档字节。
        fn plugin_bundle(
            parent: &Path,
            version: &str,
            missing_dependency: bool,
        ) -> (PathBuf, Vec<u8>) {
            let root = parent.join(format!("example-plugin-{version}"));
            std::fs::create_dir_all(&root).expect("创建插件 bundle 目录");
            std::fs::write(
                root.join("plugin.wasm"),
                format!("wasm:example.plugin:{version}"),
            )
            .expect("写入测试 Component");
            let dependency = if missing_dependency {
                "\n[[dependencies]]\nid = \"missing.plugin\"\nversion = \"^1.0\"\n"
            } else {
                ""
            };
            std::fs::write(
                root.join("plugin.toml"),
                format!(
                    "[plugin]\nid = \"example.plugin\"\nname = \"Example Plugin\"\nversion = \"{version}\"\napi_version = \"0.7.0\"\nwasm = \"plugin.wasm\"\n{dependency}"
                ),
            )
            .expect("写入测试 manifest");
            let archive = pack_plugin_bundle(&root).expect("编码确定性插件 bundle");
            (root, archive)
        }

        /// 把通用 M8 Release Fixture 改绑到真实确定性 bundle。
        fn bind_bundle(mut fixture: ReleaseFixture, bundle: Vec<u8>) -> ReleaseFixture {
            let digest = bytes_digest(&bundle);
            fixture.bundle = bundle;
            fixture.input.bundle_digest = digest.clone();
            fixture.input.host_audit.bundle_digest = digest.clone();
            fixture.input.safety_evaluation.bundle_digest = digest.clone();
            fixture.input.agent_evaluation.bundle_digest = digest;
            fixture.report =
                evaluate_plugin_source(&fixture.input).expect("真实 bundle 应通过 Gate");
            fixture
        }

        /// 构造只固定一个启用插件的最小生产 Genome。
        fn genome(
            version: &str,
            bundle: ArtifactDigest,
            parent: Option<&GenomeRevision>,
        ) -> GenomeRevision {
            GenomeRevision::create(
                AgentGenome {
                    schema_version: GENOME_SCHEMA_VERSION,
                    runtime: RuntimeIdentity {
                        package_version: "0.1.0".to_string(),
                        git_commit: format!("deployment-{version}"),
                        git_dirty: false,
                        target_triple: "test-target".to_string(),
                        features: BTreeSet::new(),
                    },
                    model: ModelGenome {
                        provider: "test".to_string(),
                        provider_kind: "mock".to_string(),
                        model: "test-model".to_string(),
                        base_url: None,
                        protocol: None,
                        max_tokens: Some(128),
                        temperature: None,
                        stream: false,
                        provider_options_digest: None,
                    },
                    prompt: PromptGenome::default(),
                    plugins: vec![PluginGenome {
                        id: "example.plugin".to_string(),
                        version: version.to_string(),
                        api_version: "0.7.0".to_string(),
                        bundle,
                        config_digest: None,
                    }],
                    capability_owners: BTreeMap::new(),
                    tools: ToolProfileGenome::default(),
                    context_policy: None,
                    planning_policy: None,
                    skills: Vec::new(),
                    execution: agent_tool::ExecutionPolicy::serve(),
                },
                GenomeMetadata {
                    created_at: None,
                    description: Some(format!("插件部署测试 {version}")),
                    parent: parent.map(|revision| revision.revision_id.clone()),
                    mutation: None,
                },
            )
            .expect("构造插件 Genome")
        }

        /// 归档已通过健康观察的 Stable Release，并返回其记录。
        async fn authorize_stable(
            controller: &PluginReleaseController<'_>,
            signing: &SigningFixture,
            fixture: &ReleaseFixture,
            admission: &PluginCanaryAdmissionV1,
            time: u64,
        ) -> PluginReleaseArchiveRecordV1 {
            let running = running_canary(&admission.canary, time + 1);
            controller
                .record_canary_observation(&fixture.input, &fixture.report, &running)
                .await
                .expect("归档 Running Canary");
            let succeeded = terminal_canary(&running, true, time + 2);
            controller
                .record_canary_observation(&fixture.input, &fixture.report, &succeeded)
                .await
                .expect("归档 Succeeded Canary");
            let stable = signing.release(
                fixture,
                PluginReleaseStage::Stable,
                Some(admission.canary.release_id.clone()),
                None,
                time + 3,
            );
            controller
                .promote_stable(
                    &fixture.input,
                    &fixture.report,
                    &succeeded,
                    &stable,
                    &fixture.component,
                    &fixture.bundle,
                )
                .await
                .expect("归档受信 Stable Release")
        }

        /// 让真实 Release Controller 接纳一个 Canary。
        async fn admit_canary(
            controller: &PluginReleaseController<'_>,
            signing: &SigningFixture,
            fixture: &ReleaseFixture,
            time: u64,
        ) -> PluginCanaryAdmissionV1 {
            let release = signing.release(fixture, PluginReleaseStage::Canary, None, None, time);
            controller
                .admit_canary(
                    &fixture.input,
                    &fixture.report,
                    &release,
                    &fixture.component,
                    &fixture.bundle,
                )
                .await
                .expect("真实 Gate、签名与 bundle 应进入 Canary")
        }

        /// 初始化真实 Plugin Manager 和 Parent Stable Genome。
        async fn initialize_parent(
            temp: &TempDir,
            bundle_root: &Path,
            revision: &GenomeRevision,
        ) -> (PluginManager, FileStableGenomePublisher) {
            let manager = PluginManager::new(temp.path().join("plugin-manager"));
            manager.install(bundle_root).expect("安装 Parent 插件");
            let evolution_root = temp.path().join("evolution");
            let publisher = FileStableGenomePublisher::new(&evolution_root);
            publisher
                .resolver()
                .store()
                .append(revision)
                .await
                .expect("登记 Parent Genome");
            publisher
                .publish("stable/plugins", revision, 1)
                .await
                .expect("初始化 Parent Stable");
            (manager, publisher)
        }

        /// 读取并返回当前 Stable Revision。
        async fn current_stable(publisher: &FileStableGenomePublisher) -> GenomeRevision {
            publisher
                .resolver()
                .resolve(&GenomeSelector::Stable("stable/plugins".to_string()))
                .await
                .expect("读取当前 Stable Revision")
        }

        /// 完整成功路径必须把真实 Candidate 安装与 Stable PluginGenome 同时切换。
        #[tokio::test]
        async fn publishes_verified_bundle_and_candidate_genome() {
            let temp = TempDir::new().expect("创建测试目录");
            let (parent_root, parent_bundle) = plugin_bundle(temp.path(), "1.0.0", false);
            let (_, candidate_bundle) = plugin_bundle(temp.path(), "2.0.0", false);
            let parent_revision = genome("1.0.0", bytes_digest(&parent_bundle), None);
            let candidate_revision = genome(
                "2.0.0",
                bytes_digest(&candidate_bundle),
                Some(&parent_revision),
            );
            let (manager, publisher) =
                initialize_parent(&temp, &parent_root, &parent_revision).await;
            let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
            let signing = SigningFixture::new();
            let archive = FilePluginReleaseArchive::new(temp.path().join("releases"), &artifacts)
                .expect("创建发布归档");
            let release_controller = PluginReleaseController::new(
                &archive,
                &signing.build_keys,
                &signing.approval_keys,
                &signing.release_keys,
            );
            let parent_fixture = bind_bundle(release_fixture(30, 1_000, None), parent_bundle);
            let parent_admission =
                admit_canary(&release_controller, &signing, &parent_fixture, 1_050).await;
            let _trusted_parent = authorize_stable(
                &release_controller,
                &signing,
                &parent_fixture,
                &parent_admission,
                1_060,
            )
            .await;
            let candidate_fixture = bind_bundle(
                release_fixture(31, 2_000, Some(&parent_fixture)),
                candidate_bundle,
            );
            let admission =
                admit_canary(&release_controller, &signing, &candidate_fixture, 2_050).await;
            let deployment_controller =
                PluginDeploymentController::new(&manager, &publisher, temp.path().join("staging"));
            let deployment = deployment_controller
                .deploy_canary(
                    "stable/plugins",
                    admission.clone(),
                    &candidate_revision,
                    &candidate_fixture.bundle,
                )
                .await
                .expect("安装真实 Canary bundle");
            assert_eq!(deployment.installed().version, "2.0.0");
            assert_eq!(current_stable(&publisher).await, parent_revision);
            let stable = authorize_stable(
                &release_controller,
                &signing,
                &candidate_fixture,
                &admission,
                2_060,
            )
            .await;
            let evaluation = archive
                .evaluation(&candidate_fixture.report.report_id)
                .await
                .expect("读取 Evaluation 归档")
                .expect("Evaluation 应存在");
            let PluginPromotionReceipt { installed, stable } = deployment_controller
                .promote_stable(deployment, &evaluation, &stable, 2)
                .await
                .expect("发布 Candidate Stable Genome");
            assert_eq!(installed.version, "2.0.0");
            assert_eq!(
                ArtifactDigest::from_sha256_hex(installed.sha256.clone())
                    .expect("安装摘要应是合法 SHA-256"),
                candidate_revision.genome.plugins[0].bundle
            );
            assert_eq!(stable.revision_id, candidate_revision.revision_id);
            assert_eq!(stable.digest, candidate_revision.digest);
            assert_eq!(current_stable(&publisher).await, candidate_revision);
        }

        /// replace 已提交但 CanaryInstalled 未落盘时，新 Controller 必须从真实 Manager 恢复并
        /// 完成 Stable Promotion；终态补记前 Stable 被其他发布改写时必须失败关闭。
        #[tokio::test]
        async fn rebuilds_manager_state_across_controller_restart_and_promotes() {
            let temp = TempDir::new().expect("创建测试目录");
            let (parent_root, parent_bundle) = plugin_bundle(temp.path(), "1.0.0", false);
            let (_, candidate_bundle) = plugin_bundle(temp.path(), "2.0.0", false);
            let parent_revision = genome("1.0.0", bytes_digest(&parent_bundle), None);
            let mut candidate_revision = genome(
                "2.0.0",
                bytes_digest(&candidate_bundle),
                Some(&parent_revision),
            );
            let (manager, publisher) =
                initialize_parent(&temp, &parent_root, &parent_revision).await;
            let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
            let signing = SigningFixture::new();
            let archive = FilePluginReleaseArchive::new(temp.path().join("releases"), &artifacts)
                .expect("创建发布归档");
            let release_controller = PluginReleaseController::new(
                &archive,
                &signing.build_keys,
                &signing.approval_keys,
                &signing.release_keys,
            );
            let parent_fixture = bind_bundle(release_fixture(40, 7_000, None), parent_bundle);
            let parent_admission =
                admit_canary(&release_controller, &signing, &parent_fixture, 7_050).await;
            let _trusted_parent = authorize_stable(
                &release_controller,
                &signing,
                &parent_fixture,
                &parent_admission,
                7_060,
            )
            .await;
            let candidate_fixture = bind_bundle(
                release_fixture(41, 8_000, Some(&parent_fixture)),
                candidate_bundle,
            );
            candidate_revision.metadata.mutation =
                Some(candidate_fixture.input.proposal.mutation_id.clone());
            let admission =
                admit_canary(&release_controller, &signing, &candidate_fixture, 8_050).await;
            let deployment_root = temp.path().join("deployment-store");
            let deployment_store = FilePluginDeploymentStore::new(&deployment_root, &artifacts)
                .expect("创建部署 Store");
            let first = PersistentPluginDeploymentController::new(
                &manager,
                &publisher,
                &archive,
                &deployment_store,
                temp.path().join("staging-first"),
            );
            let installed_record = first
                .deploy_canary(
                    "stable/plugins",
                    &admission.release.release.release_id,
                    &candidate_revision,
                )
                .await
                .expect("持久化安装 Canary");
            assert_eq!(
                installed_record.state,
                PluginDeploymentStateV1::CanaryInstalled
            );
            drop(first);

            let directory = deployment_root.join("deployments").join(format!(
                "{:x}",
                Sha256::digest(installed_record.deployment_id.as_str().as_bytes())
            ));
            // 模拟 Planned 已提交、进程在 replace 前退出；新 Controller 必须从归档重做安装。
            std::fs::remove_file(directory.join("01-canary-installed.json"))
                .expect("移除测试中的未提交状态快照");
            manager
                .replace(&parent_root)
                .expect("测试应恢复 replace 前的 Parent 安装");
            let second = PersistentPluginDeploymentController::new(
                &manager,
                &publisher,
                &archive,
                &deployment_store,
                temp.path().join("staging-second"),
            );
            let recovered = second
                .recover_canary_install(&installed_record.deployment_id)
                .await
                .expect("新 Controller 应从 Release Archive 重新安装 Candidate");
            assert_eq!(recovered.state, PluginDeploymentStateV1::CanaryInstalled);
            assert_eq!(manager.list().expect("读取插件锁")[0].version, "2.0.0");
            drop(second);

            // 模拟 replace 已提交、进程在 CanaryInstalled 原子文件提交前退出；再次重建只补记状态。
            std::fs::remove_file(directory.join("01-canary-installed.json"))
                .expect("再次移除测试中的未提交状态快照");
            let third = PersistentPluginDeploymentController::new(
                &manager,
                &publisher,
                &archive,
                &deployment_store,
                temp.path().join("staging-third"),
            );
            let recovered = third
                .recover_canary_install(&installed_record.deployment_id)
                .await
                .expect("新 Controller 应从 Manager Candidate 补记状态");

            let stable = authorize_stable(
                &release_controller,
                &signing,
                &candidate_fixture,
                &admission,
                8_060,
            )
            .await;
            let receipt = third
                .promote_stable(&recovered.deployment_id, &stable.release.release_id)
                .await
                .expect("重建后的 Controller 应完成 Promotion");
            assert_eq!(receipt.installed.version, "2.0.0");
            assert_eq!(receipt.stable.revision_id, candidate_revision.revision_id);
            drop(third);

            // 模拟 Stable 已原子发布、进程在 Promoted 状态文件提交前退出。
            std::fs::remove_file(directory.join("02-promoted.json"))
                .expect("移除测试中的未提交 Promoted 快照");
            let fourth = PersistentPluginDeploymentController::new(
                &manager,
                &publisher,
                &archive,
                &deployment_store,
                temp.path().join("staging-fourth"),
            );
            let resumed = fourth
                .promote_stable(&recovered.deployment_id, &stable.release.release_id)
                .await
                .expect("新 Controller 应识别已提交 Stable 并补记 Promoted");
            assert_eq!(resumed.stable, receipt.stable);
            assert_eq!(
                deployment_store
                    .load(&PluginDeploymentId::for_canary_release(
                        admission.release.release.release_id.clone(),
                    ))
                    .await
                    .expect("读取部署终态")
                    .expect("部署应存在")
                    .state,
                PluginDeploymentStateV1::Promoted
            );

            // 再次模拟 Promoted 未落盘，并让另一发布推进 Stable；恢复不得误认其为本部署结果。
            std::fs::remove_file(directory.join("02-promoted.json"))
                .expect("再次移除测试中的未提交 Promoted 快照");
            let conflicting_revision = genome(
                "3.0.0",
                bytes_digest(&candidate_fixture.bundle),
                Some(&candidate_revision),
            );
            publisher
                .resolver()
                .store()
                .append(&conflicting_revision)
                .await
                .expect("登记其他发布的 Genome");
            publisher
                .publish("stable/plugins", &conflicting_revision, 3)
                .await
                .expect("模拟其他生产发布改写 Stable");
            let fifth = PersistentPluginDeploymentController::new(
                &manager,
                &publisher,
                &archive,
                &deployment_store,
                temp.path().join("staging-fifth"),
            );
            let error = fifth
                .promote_stable(&recovered.deployment_id, &stable.release.release_id)
                .await
                .expect_err("其他发布的 Stable 不得被误认成本部署结果");
            match error {
                PluginDeploymentError::Binding(message) => {
                    assert_eq!(message, "Stable 已被其他生产部署改写");
                }
                other => panic!("Stable 并发改写应返回绑定错误，实际为：{other}"),
            }
            assert_eq!(current_stable(&publisher).await, conflicting_revision);
            assert_eq!(manager.list().expect("读取插件锁")[0].version, "2.0.0");
            assert_eq!(
                deployment_store
                    .load(&recovered.deployment_id)
                    .await
                    .expect("读取并发改写后的部署状态")
                    .expect("部署应存在")
                    .state,
                PluginDeploymentStateV1::CanaryInstalled
            );
        }

        /// CanaryInstalled 落盘后重启的新 Controller 必须仅依赖归档与 Store/CAS 恢复旧 bundle。
        #[tokio::test]
        async fn rebuilds_manager_state_across_controller_restart_and_rolls_back() {
            let temp = TempDir::new().expect("创建测试目录");
            let (parent_root, parent_bundle) = plugin_bundle(temp.path(), "1.0.0", false);
            let (_, candidate_bundle) = plugin_bundle(temp.path(), "2.0.0", false);
            let parent_revision = genome("1.0.0", bytes_digest(&parent_bundle), None);
            let mut candidate_revision = genome(
                "2.0.0",
                bytes_digest(&candidate_bundle),
                Some(&parent_revision),
            );
            let (manager, publisher) =
                initialize_parent(&temp, &parent_root, &parent_revision).await;
            let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
            let signing = SigningFixture::new();
            let archive = FilePluginReleaseArchive::new(temp.path().join("releases"), &artifacts)
                .expect("创建发布归档");
            let release_controller = PluginReleaseController::new(
                &archive,
                &signing.build_keys,
                &signing.approval_keys,
                &signing.release_keys,
            );
            let parent_fixture = bind_bundle(release_fixture(42, 9_000, None), parent_bundle);
            let parent_admission =
                admit_canary(&release_controller, &signing, &parent_fixture, 9_050).await;
            let trusted_parent = authorize_stable(
                &release_controller,
                &signing,
                &parent_fixture,
                &parent_admission,
                9_060,
            )
            .await;
            let candidate_fixture = bind_bundle(
                release_fixture(43, 10_000, Some(&parent_fixture)),
                candidate_bundle,
            );
            candidate_revision.metadata.mutation =
                Some(candidate_fixture.input.proposal.mutation_id.clone());
            let admission =
                admit_canary(&release_controller, &signing, &candidate_fixture, 10_050).await;
            let deployment_store =
                FilePluginDeploymentStore::new(temp.path().join("deployment-store"), &artifacts)
                    .expect("创建部署 Store");
            let first = PersistentPluginDeploymentController::new(
                &manager,
                &publisher,
                &archive,
                &deployment_store,
                temp.path().join("staging-first"),
            );
            let installed = first
                .deploy_canary(
                    "stable/plugins",
                    &admission.release.release.release_id,
                    &candidate_revision,
                )
                .await
                .expect("持久化安装 Canary");
            drop(first);

            let running = running_canary(&admission.canary, 10_060);
            release_controller
                .record_canary_observation(
                    &candidate_fixture.input,
                    &candidate_fixture.report,
                    &running,
                )
                .await
                .expect("归档 Running Canary");
            let failed = terminal_canary(&running, false, 10_061);
            release_controller
                .record_canary_observation(
                    &candidate_fixture.input,
                    &candidate_fixture.report,
                    &failed,
                )
                .await
                .expect("归档 Failed Canary");
            let rollback = signing.release(
                &candidate_fixture,
                PluginReleaseStage::Rollback,
                Some(admission.canary.release_id.clone()),
                Some(trusted_parent.release.attestation.component_digest.clone()),
                10_062,
            );
            let rollback_record = release_controller
                .rollback_failed_canary(PluginRollbackRequestV1 {
                    input: &candidate_fixture.input,
                    report: &candidate_fixture.report,
                    failed: &failed,
                    rollback: &rollback,
                    rollback_target_release_id: &trusted_parent.release.release_id,
                    candidate_component_bytes: &candidate_fixture.component,
                    bundle_bytes: &candidate_fixture.bundle,
                    rollback_target_bytes: &parent_fixture.component,
                })
                .await
                .expect("授权健康失败回滚");
            let second = PersistentPluginDeploymentController::new(
                &manager,
                &publisher,
                &archive,
                &deployment_store,
                temp.path().join("staging-second"),
            );
            let receipt = second
                .rollback_failed_canary(
                    &installed.deployment_id,
                    &rollback_record.release.release_id,
                    &trusted_parent.release.release_id,
                )
                .await
                .expect("重建后的 Controller 应恢复旧 Stable bundle");
            assert_eq!(receipt.installed.version, "1.0.0");
            assert_eq!(
                receipt.stable,
                installed.parent_stable.expect("应持久化 Parent Stable")
            );
            assert_eq!(current_stable(&publisher).await, parent_revision);
            drop(second);

            // 模拟旧 bundle 已恢复、进程在 RolledBack 状态文件提交前退出。
            let directory = temp
                .path()
                .join("deployment-store")
                .join("deployments")
                .join(format!(
                    "{:x}",
                    Sha256::digest(installed.deployment_id.as_str().as_bytes())
                ));
            std::fs::remove_file(directory.join("02-rolled-back.json"))
                .expect("移除测试中的未提交 RolledBack 快照");
            let third = PersistentPluginDeploymentController::new(
                &manager,
                &publisher,
                &archive,
                &deployment_store,
                temp.path().join("staging-third"),
            );
            let resumed = third
                .rollback_failed_canary(
                    &installed.deployment_id,
                    &rollback_record.release.release_id,
                    &trusted_parent.release.release_id,
                )
                .await
                .expect("新 Controller 应识别已恢复 Manager 并补记 RolledBack");
            assert_eq!(resumed.installed.version, "1.0.0");
            assert_eq!(
                deployment_store
                    .load(&installed.deployment_id)
                    .await
                    .expect("读取部署终态")
                    .expect("部署应存在")
                    .state,
                PluginDeploymentStateV1::RolledBack
            );
        }

        /// Plugin Manager 拒绝安装时不得改写插件锁或 Stable Genome。
        #[tokio::test]
        async fn install_failure_keeps_previous_plugin_and_stable() {
            let temp = TempDir::new().expect("创建测试目录");
            let (parent_root, parent_bundle) = plugin_bundle(temp.path(), "1.0.0", false);
            let (_, candidate_bundle) = plugin_bundle(temp.path(), "2.0.0", true);
            let parent_revision = genome("1.0.0", bytes_digest(&parent_bundle), None);
            let candidate_revision = genome(
                "2.0.0",
                bytes_digest(&candidate_bundle),
                Some(&parent_revision),
            );
            let (manager, publisher) =
                initialize_parent(&temp, &parent_root, &parent_revision).await;
            let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
            let signing = SigningFixture::new();
            let archive = FilePluginReleaseArchive::new(temp.path().join("releases"), &artifacts)
                .expect("创建发布归档");
            let release_controller = PluginReleaseController::new(
                &archive,
                &signing.build_keys,
                &signing.approval_keys,
                &signing.release_keys,
            );
            let candidate_fixture = bind_bundle(release_fixture(32, 3_000, None), candidate_bundle);
            let admission =
                admit_canary(&release_controller, &signing, &candidate_fixture, 3_050).await;
            let deployment_controller =
                PluginDeploymentController::new(&manager, &publisher, temp.path().join("staging"));
            assert!(matches!(
                deployment_controller
                    .deploy_canary(
                        "stable/plugins",
                        admission,
                        &candidate_revision,
                        &candidate_fixture.bundle,
                    )
                    .await,
                Err(PluginDeploymentError::Install(_))
            ));
            assert_eq!(manager.list().expect("读取插件锁")[0].version, "1.0.0");
            assert_eq!(current_stable(&publisher).await, parent_revision);
        }

        /// Genome Publisher 失败时必须尽力恢复旧 bundle，且不能声称跨存储强事务。
        #[tokio::test]
        async fn genome_publish_failure_restores_previous_bundle() {
            let temp = TempDir::new().expect("创建测试目录");
            let (parent_root, parent_bundle) = plugin_bundle(temp.path(), "1.0.0", false);
            let (_, candidate_bundle) = plugin_bundle(temp.path(), "2.0.0", false);
            let parent_revision = genome("1.0.0", bytes_digest(&parent_bundle), None);
            let candidate_revision = genome(
                "2.0.0",
                bytes_digest(&candidate_bundle),
                Some(&parent_revision),
            );
            let (manager, publisher) =
                initialize_parent(&temp, &parent_root, &parent_revision).await;
            let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
            let signing = SigningFixture::new();
            let archive = FilePluginReleaseArchive::new(temp.path().join("releases"), &artifacts)
                .expect("创建发布归档");
            let release_controller = PluginReleaseController::new(
                &archive,
                &signing.build_keys,
                &signing.approval_keys,
                &signing.release_keys,
            );
            let candidate_fixture = bind_bundle(release_fixture(33, 4_000, None), candidate_bundle);
            let admission =
                admit_canary(&release_controller, &signing, &candidate_fixture, 4_050).await;
            let deployment_controller =
                PluginDeploymentController::new(&manager, &publisher, temp.path().join("staging"));
            let deployment = deployment_controller
                .deploy_canary(
                    "stable/plugins",
                    admission.clone(),
                    &candidate_revision,
                    &candidate_fixture.bundle,
                )
                .await
                .expect("Canary 安装应成功");
            let stable = authorize_stable(
                &release_controller,
                &signing,
                &candidate_fixture,
                &admission,
                4_060,
            )
            .await;
            let evaluation = archive
                .evaluation(&candidate_fixture.report.report_id)
                .await
                .expect("读取 Evaluation 归档")
                .expect("Evaluation 应存在");
            assert!(matches!(
                deployment_controller
                    .promote_stable(deployment, &evaluation, &stable, 1)
                    .await,
                Err(PluginDeploymentError::PostCanaryFailure {
                    restoration_error: None,
                    ..
                })
            ));
            assert_eq!(manager.list().expect("读取插件锁")[0].version, "1.0.0");
            assert_eq!(current_stable(&publisher).await, parent_revision);
        }

        /// 主失败和旧 bundle 补偿失败必须同时进入错误，禁止后一个错误覆盖前一个错误。
        #[tokio::test]
        async fn retains_primary_and_compensation_errors() {
            let temp = TempDir::new().expect("创建测试目录");
            let (parent_root, parent_bundle) = plugin_bundle(temp.path(), "1.0.0", false);
            let (_, candidate_bundle) = plugin_bundle(temp.path(), "2.0.0", false);
            let parent_revision = genome("1.0.0", bytes_digest(&parent_bundle), None);
            let candidate_revision = genome(
                "2.0.0",
                bytes_digest(&candidate_bundle),
                Some(&parent_revision),
            );
            let (manager, publisher) =
                initialize_parent(&temp, &parent_root, &parent_revision).await;
            let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
            let signing = SigningFixture::new();
            let archive = FilePluginReleaseArchive::new(temp.path().join("releases"), &artifacts)
                .expect("创建发布归档");
            let release_controller = PluginReleaseController::new(
                &archive,
                &signing.build_keys,
                &signing.approval_keys,
                &signing.release_keys,
            );
            let candidate_fixture = bind_bundle(release_fixture(36, 7_000, None), candidate_bundle);
            let admission =
                admit_canary(&release_controller, &signing, &candidate_fixture, 7_050).await;
            let deployment_controller =
                PluginDeploymentController::new(&manager, &publisher, temp.path().join("staging"));
            let deployment = deployment_controller
                .deploy_canary(
                    "stable/plugins",
                    admission.clone(),
                    &candidate_revision,
                    &candidate_fixture.bundle,
                )
                .await
                .expect("Canary 安装应成功");
            let stable = authorize_stable(
                &release_controller,
                &signing,
                &candidate_fixture,
                &admission,
                7_060,
            )
            .await;
            let evaluation = archive
                .evaluation(&candidate_fixture.report.report_id)
                .await
                .expect("读取 Evaluation 归档")
                .expect("Evaluation 应存在");
            let conflicting_old_destination = manager.root().join("plugins/example.plugin/1.0.0");
            std::fs::create_dir_all(&conflicting_old_destination).expect("创建补偿目标冲突目录");
            std::fs::write(conflicting_old_destination.join("occupied"), b"conflict")
                .expect("写入补偿冲突标记");
            let error = deployment_controller
                .promote_stable(deployment, &evaluation, &stable, 1)
                .await
                .expect_err("Genome 发布与旧 bundle 补偿应同时失败");
            match error {
                PluginDeploymentError::PostCanaryFailure {
                    primary,
                    restoration_error: Some(restoration_error),
                    ..
                } => {
                    assert!(primary.contains("发布 Stable Genome 失败"));
                    assert!(restoration_error.contains("插件目标目录已存在"));
                }
                other => panic!("应同时保留主因和补偿错误，实际为：{other}"),
            }
            assert_eq!(manager.list().expect("读取插件锁")[0].version, "2.0.0");
            assert_eq!(current_stable(&publisher).await, parent_revision);
        }

        /// Canary 健康失败必须恢复先前受信 Stable bundle，并保持 Parent Genome。
        #[tokio::test]
        async fn failed_health_rolls_back_trusted_bundle_and_parent_genome() {
            let temp = TempDir::new().expect("创建测试目录");
            let (parent_root, parent_bundle) = plugin_bundle(temp.path(), "1.0.0", false);
            let (_, candidate_bundle) = plugin_bundle(temp.path(), "2.0.0", false);
            let parent_revision = genome("1.0.0", bytes_digest(&parent_bundle), None);
            let candidate_revision = genome(
                "2.0.0",
                bytes_digest(&candidate_bundle),
                Some(&parent_revision),
            );
            let (manager, publisher) =
                initialize_parent(&temp, &parent_root, &parent_revision).await;
            let artifacts = FileArtifactStore::new(temp.path().join("artifacts"));
            let signing = SigningFixture::new();
            let archive = FilePluginReleaseArchive::new(temp.path().join("releases"), &artifacts)
                .expect("创建发布归档");
            let release_controller = PluginReleaseController::new(
                &archive,
                &signing.build_keys,
                &signing.approval_keys,
                &signing.release_keys,
            );
            let parent_fixture = bind_bundle(release_fixture(34, 5_000, None), parent_bundle);
            let parent_admission =
                admit_canary(&release_controller, &signing, &parent_fixture, 5_050).await;
            let trusted_parent = authorize_stable(
                &release_controller,
                &signing,
                &parent_fixture,
                &parent_admission,
                5_060,
            )
            .await;
            let candidate_fixture = bind_bundle(
                release_fixture(35, 6_000, Some(&parent_fixture)),
                candidate_bundle,
            );
            let admission =
                admit_canary(&release_controller, &signing, &candidate_fixture, 6_050).await;
            let deployment_controller =
                PluginDeploymentController::new(&manager, &publisher, temp.path().join("staging"));
            let deployment = deployment_controller
                .deploy_canary(
                    "stable/plugins",
                    admission.clone(),
                    &candidate_revision,
                    &candidate_fixture.bundle,
                )
                .await
                .expect("Canary 安装应成功");
            let running = running_canary(&admission.canary, 6_060);
            release_controller
                .record_canary_observation(
                    &candidate_fixture.input,
                    &candidate_fixture.report,
                    &running,
                )
                .await
                .expect("归档 Running Canary");
            let failed = terminal_canary(&running, false, 6_061);
            release_controller
                .record_canary_observation(
                    &candidate_fixture.input,
                    &candidate_fixture.report,
                    &failed,
                )
                .await
                .expect("归档 Failed Canary");
            let rollback = signing.release(
                &candidate_fixture,
                PluginReleaseStage::Rollback,
                Some(admission.canary.release_id.clone()),
                Some(trusted_parent.release.attestation.component_digest.clone()),
                6_062,
            );
            let rollback_record = release_controller
                .rollback_failed_canary(PluginRollbackRequestV1 {
                    input: &candidate_fixture.input,
                    report: &candidate_fixture.report,
                    failed: &failed,
                    rollback: &rollback,
                    rollback_target_release_id: &trusted_parent.release.release_id,
                    candidate_component_bytes: &candidate_fixture.component,
                    bundle_bytes: &candidate_fixture.bundle,
                    rollback_target_bytes: &parent_fixture.component,
                })
                .await
                .expect("Release Controller 应授权健康失败回滚");
            let evaluation = archive
                .evaluation(&candidate_fixture.report.report_id)
                .await
                .expect("读取 Evaluation 归档")
                .expect("Evaluation 应存在");
            let receipt = deployment_controller
                .rollback_failed_canary(deployment, &evaluation, &rollback_record, &trusted_parent)
                .await
                .expect("恢复受信 Stable bundle");
            assert_eq!(receipt.installed.version, "1.0.0");
            assert_eq!(receipt.stable.revision_id, parent_revision.revision_id);
            assert_eq!(current_stable(&publisher).await, parent_revision);
            let resolver = FileGenomeResolver::new(temp.path().join("evolution"));
            assert_eq!(
                resolver
                    .resolve(&GenomeSelector::Stable("stable/plugins".to_string()))
                    .await
                    .expect("重新解析 Parent Stable"),
                parent_revision
            );
        }
    }
}

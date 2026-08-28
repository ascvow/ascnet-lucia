//! TUI Session 的 Genome 行为解析与不可变运行绑定。

use crate::{
    app_config::{resolve_config_relative_path, EvidenceSettings, GenomeSettings},
    genome_binding::GenomeRuntimeBinding,
};
use agent_evolution::{FileArtifactStore, FileGenomeResolver, GenomeResolver, GenomeSelector};
use agent_evolution_protocol::GenomeRevisionId;
use agent_session::{SessionBehaviorBinding, SessionRecord};
use anyhow::{anyhow, Context, Result};
use std::{path::Path, path::PathBuf, sync::Arc};

/// Session Store 中用于标识 Agent Genome 修订绑定的协议名。
pub(crate) const GENOME_SESSION_BEHAVIOR_KIND: &str = "agent_genome";

/// 进程内固定的 Session 行为运行时。
///
/// Genome 分支保存已从不可变 Registry 校验的精确修订；Legacy 分支显式声明当前
/// Session 没有可归因的行为制品，因此不能进入 Evidence Plane。
#[derive(Clone)]
pub(crate) enum GenomeSessionRuntime {
    /// 已绑定并可装配真实行为的 Genome Session。
    Genome {
        /// 启动后不可替换的行为装配器。
        binding: Arc<GenomeRuntimeBinding>,
        /// Genome、Artifact CAS 与可选 Evidence 存储的共同根目录。
        registry_root: PathBuf,
        /// 仅新 Session 从 Stable 解析时记录，供发布后健康观察校验。
        stable_lineage: Option<String>,
    },
    /// 未配置 Registry 或历史记录缺少行为绑定的不可评估 Session。
    LegacyUnbound,
    /// 新 Draft 尚未配置可解析 Genome，允许进入界面但禁止启动 Agent Run。
    Unconfigured,
    /// 仅供既有 Session 持久化单元测试隔离 Genome 装配关注点。
    #[cfg(test)]
    TestOnly,
}

impl GenomeSessionRuntime {
    /// 返回已经校验的 Genome 行为装配器；Legacy Session 返回 `None`。
    pub(crate) fn binding(&self) -> Option<&GenomeRuntimeBinding> {
        match self {
            Self::Genome { binding, .. } => Some(binding.as_ref()),
            Self::LegacyUnbound | Self::Unconfigured => None,
            #[cfg(test)]
            Self::TestOnly => None,
        }
    }

    /// 克隆 Evidence Plane 使用的同一 Genome 行为装配器。
    pub(crate) fn binding_arc(&self) -> Option<Arc<GenomeRuntimeBinding>> {
        match self {
            Self::Genome { binding, .. } => Some(Arc::clone(binding)),
            Self::LegacyUnbound | Self::Unconfigured => None,
            #[cfg(test)]
            Self::TestOnly => None,
        }
    }

    /// 返回已配置并实际解析成功的 Registry 根目录。
    pub(crate) fn registry_root(&self) -> Option<&Path> {
        match self {
            Self::Genome { registry_root, .. } => Some(registry_root),
            Self::LegacyUnbound | Self::Unconfigured => None,
            #[cfg(test)]
            Self::TestOnly => None,
        }
    }

    /// 返回新 Session 解析时使用的 Stable lineage。
    pub(crate) fn stable_lineage(&self) -> Option<&str> {
        match self {
            Self::Genome { stable_lineage, .. } => stable_lineage.as_deref(),
            Self::LegacyUnbound | Self::Unconfigured => None,
            #[cfg(test)]
            Self::TestOnly => None,
        }
    }

    /// 为新 Session 写入当前运行时绑定，或验证待恢复 Session 与当前行为完全一致。
    ///
    /// 已持久化但缺少绑定的旧记录只允许补充 Legacy 标记；Genome 与 Legacy 之间、不同
    /// Genome Revision 之间均不得切换。
    ///
    /// # Errors
    ///
    /// Session 已绑定其他行为协议或修订时返回错误。
    pub(crate) fn bind_or_validate_session(&self, record: &mut SessionRecord) -> Result<()> {
        match self {
            Self::Genome { binding, .. } => SessionBehaviorBinding::new(
                GENOME_SESSION_BEHAVIOR_KIND,
                binding.revision().revision_id.to_string(),
            )
            .context("构造 Session Genome 行为绑定失败")
            .and_then(|expected| match record.behavior_binding.as_ref() {
                Some(actual) if actual == &expected || actual.is_legacy_unbound() => Ok(()),
                Some(_) => Err(anyhow!(
                    "会话 `{}` 的行为修订与当前 Session Runtime 不匹配",
                    record.id
                )),
                None if record.revision == 0 => {
                    record.behavior_binding = Some(expected);
                    Ok(())
                }
                None => {
                    record.behavior_binding = Some(SessionBehaviorBinding::legacy_unbound());
                    Ok(())
                }
            }),
            Self::LegacyUnbound => {
                if record.revision == 0 && record.behavior_binding.is_none() {
                    return Err(anyhow!("新 Session 禁止写入 LegacyUnbound 行为标记"));
                }
                match record.behavior_binding.as_ref() {
                    Some(binding) if binding.is_legacy_unbound() => Ok(()),
                    Some(_) => Err(anyhow!(
                        "会话 `{}` 的行为修订与 Legacy Runtime 不匹配",
                        record.id
                    )),
                    None => {
                        record.behavior_binding = Some(SessionBehaviorBinding::legacy_unbound());
                        Ok(())
                    }
                }
            }
            Self::Unconfigured => match record.behavior_binding.as_ref() {
                Some(binding) if binding.is_legacy_unbound() => Ok(()),
                Some(_) => Err(anyhow!(
                    "当前未配置 Genome Registry，不能恢复带 Genome 绑定的 Session"
                )),
                None if record.revision == 0 => Ok(()),
                None => {
                    record.behavior_binding = Some(SessionBehaviorBinding::legacy_unbound());
                    Ok(())
                }
            },
            #[cfg(test)]
            Self::TestOnly => Ok(()),
        }
    }

    /// 在提交用户输入前验证 Session 具备可归因的精确 Genome 行为绑定。
    ///
    /// # Errors
    ///
    /// 未配置 Registry、历史 Session 只有 Legacy 标记，或绑定与当前精确 Revision 不一致
    /// 时返回错误；失败发生在 Session 首次保存和模型调用之前。
    pub(crate) fn validate_run_session(&self, record: &mut SessionRecord) -> Result<()> {
        match self {
            Self::Genome { binding, .. } => {
                let expected = SessionBehaviorBinding::new(
                    GENOME_SESSION_BEHAVIOR_KIND,
                    binding.revision().revision_id.to_string(),
                )
                .context("构造 Run Genome 行为绑定失败")?;
                match record.behavior_binding.as_ref() {
                    Some(actual) if actual == &expected => Ok(()),
                    None if record.revision == 0 => {
                        record.behavior_binding = Some(expected);
                        Ok(())
                    }
                    _ => Err(anyhow!(
                        "Session 未绑定当前精确 Genome Revision，禁止启动 Run"
                    )),
                }
            }
            Self::LegacyUnbound => Err(anyhow!(
                "LegacyUnbound Session 不具备运行资格；请创建绑定精确 Genome Revision 的新 Session"
            )),
            Self::Unconfigured => Err(anyhow!(
                "尚未配置可解析的 Genome Registry；请配置 genome.stable 或 genome.revision_id 后重启"
            )),
            #[cfg(test)]
            Self::TestOnly => Ok(()),
        }
    }
}

impl Default for GenomeSessionRuntime {
    /// 默认使用显式 Legacy 模式，保证未配置 Registry 的首次启动不访问或创建 Evolution
    /// 存储。
    fn default() -> Self {
        Self::Unconfigured
    }
}

/// 按 Session 已有绑定或新 Session 的 Stable 选择解析固定 Genome 行为。
///
/// 已绑定 Genome 的 Session 始终按精确 Revision 读取，忽略当前 Stable 指向；旧格式
/// Session 缺少绑定时只补 Legacy 标记。未配置 Registry 的新 Session 同样进入 Legacy
/// 模式，且不会创建 Artifact、Episode、Outbox 或 Outcome Revision 目录。
///
/// # Errors
///
/// 配置选择器冲突、绑定协议不受支持、Revision 不存在或摘要校验失败，以及 Genome 与
/// 当前 Kernel 不兼容时返回错误。
pub(crate) async fn load_genome_session_runtime(
    genome: &GenomeSettings,
    legacy_evidence: &EvidenceSettings,
    config_path: &Path,
    lucia_home: &Path,
    record: &mut SessionRecord,
) -> Result<GenomeSessionRuntime> {
    if let Some(existing) = record.behavior_binding.as_ref() {
        if existing.is_legacy_unbound() {
            return Ok(GenomeSessionRuntime::LegacyUnbound);
        }
        if existing.kind != GENOME_SESSION_BEHAVIOR_KIND {
            return Err(anyhow!(
                "会话 `{}` 使用当前 TUI 不支持的行为绑定协议 `{}`",
                record.id,
                existing.kind
            ));
        }
        let root = registry_request(genome, legacy_evidence, config_path, lucia_home)?
            .map(|request| request.root)
            .unwrap_or_else(|| lucia_home.join("evolution"));
        let revision_id = GenomeRevisionId::new(existing.revision.clone())
            .context("Session 中的 Genome Revision ID 不合法")?;
        return resolve_runtime(root, GenomeSelector::Revision(revision_id), None).await;
    }

    if record.revision > 0 {
        record.behavior_binding = Some(SessionBehaviorBinding::legacy_unbound());
        return Ok(GenomeSessionRuntime::LegacyUnbound);
    }

    let Some(request) = registry_request(genome, legacy_evidence, config_path, lucia_home)? else {
        return Ok(GenomeSessionRuntime::Unconfigured);
    };
    let Some(selector) = request.selector else {
        return Ok(GenomeSessionRuntime::Unconfigured);
    };
    let stable_lineage = match &selector {
        GenomeSelector::Stable(lineage) => Some(lineage.clone()),
        GenomeSelector::Revision(_) => None,
    };
    let runtime = resolve_runtime(request.root, selector, stable_lineage).await?;
    runtime.bind_or_validate_session(record)?;
    Ok(runtime)
}

/// 解析配置优先级：独立 `[genome]` 优先，旧 `[evidence]` 选择字段仅作兼容。
fn registry_request(
    genome: &GenomeSettings,
    legacy_evidence: &EvidenceSettings,
    config_path: &Path,
    lucia_home: &Path,
) -> Result<Option<RegistryRequest>> {
    let genome_configured =
        genome.root_dir.is_some() || genome.stable.is_some() || genome.revision_id.is_some();
    if genome_configured {
        let selector = selector(
            genome.revision_id.as_deref(),
            genome.stable.as_deref(),
            "genome.revision_id",
            "genome.stable",
        )?;
        return Ok(Some(RegistryRequest {
            root: genome
                .root_dir
                .as_deref()
                .map(|path| resolve_config_relative_path(config_path, path))
                .unwrap_or_else(|| lucia_home.join("evolution")),
            selector,
        }));
    }

    let legacy_configured = legacy_evidence.enabled
        || legacy_evidence.root_dir.is_some()
        || legacy_evidence.genome_revision_id.is_some()
        || legacy_evidence.genome_stable.is_some();
    if !legacy_configured {
        return Ok(None);
    }
    Ok(Some(RegistryRequest {
        root: legacy_evidence
            .root_dir
            .as_deref()
            .map(|path| resolve_config_relative_path(config_path, path))
            .unwrap_or_else(|| lucia_home.join("evolution")),
        selector: selector(
            legacy_evidence.genome_revision_id.as_deref(),
            legacy_evidence.genome_stable.as_deref(),
            "evidence.genome_revision_id",
            "evidence.genome_stable",
        )?,
    }))
}

/// 把互斥配置字段转换为只读 Resolver 选择器。
fn selector(
    revision: Option<&str>,
    stable: Option<&str>,
    revision_field: &str,
    stable_field: &str,
) -> Result<Option<GenomeSelector>> {
    match (revision, stable) {
        (Some(_), Some(_)) => Err(anyhow!("{revision_field} 与 {stable_field} 只能配置一个")),
        (Some(revision), None) => Ok(Some(GenomeSelector::Revision(
            GenomeRevisionId::new(revision)
                .with_context(|| format!("{revision_field} 不是合法的 Genome Revision ID"))?,
        ))),
        (None, Some(stable)) => Ok(Some(GenomeSelector::Stable(stable.to_string()))),
        (None, None) => Ok(None),
    }
}

/// 从不可变 Registry 解析 Revision，并建立可执行行为装配器。
async fn resolve_runtime(
    root: PathBuf,
    selector: GenomeSelector,
    stable_lineage: Option<String>,
) -> Result<GenomeSessionRuntime> {
    let revision = FileGenomeResolver::new(&root)
        .resolve(&selector)
        .await
        .with_context(|| format!("解析 Session Genome 失败：{selector:?}"))?;
    let binding =
        GenomeRuntimeBinding::new(revision, FileArtifactStore::new(root.join("artifacts")))?;
    Ok(GenomeSessionRuntime::Genome {
        binding: Arc::new(binding),
        registry_root: root,
        stable_lineage,
    })
}

/// 一次只读 Registry 解析请求。
struct RegistryRequest {
    root: PathBuf,
    selector: Option<GenomeSelector>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evidence::{load_evidence_runtime, test_genome_revision};
    use agent_core::Session;
    use agent_evolution::{FileArtifactStore, FileGenomeStore, GenomeStore, StableGenomeRef};
    use agent_session::SessionId;
    use agent_tool::ExecutionPolicy;
    use sha2::{Digest, Sha256};

    /// 创建不会与并发测试冲突的 Genome Registry 根目录。
    fn temp_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lucia-session-genome-{label}-{}",
            SessionId::generate()
        ))
    }

    /// 写入只供 Resolver 测试使用的 Stable 引用，不绕过生产发布路径。
    async fn write_stable(root: &Path, revision: &agent_evolution_protocol::GenomeRevision) {
        let reference =
            StableGenomeRef::new("stable/general", revision, 1).expect("应构造 Stable 引用");
        let stable_root = root.join("stable");
        tokio::fs::create_dir_all(&stable_root)
            .await
            .expect("应创建 Stable 目录");
        let filename = format!("{:x}.json", Sha256::digest(b"stable/general"));
        tokio::fs::write(
            stable_root.join(filename),
            serde_json::to_vec_pretty(&reference).expect("应序列化 Stable"),
        )
        .await
        .expect("应写入 Stable");
    }

    /// Evidence 关闭时仍应从 Stable 绑定精确 Revision，且不得创建生产证据目录。
    #[tokio::test]
    async fn disabled_evidence_still_binds_exact_genome_without_evidence_storage() {
        let root = temp_root("disabled-evidence");
        let registry = root.join("evolution");
        let artifacts = FileArtifactStore::new(registry.join("artifacts"));
        let revision = test_genome_revision(ExecutionPolicy::serve(), &artifacts).await;
        FileGenomeStore::new(registry.join("genomes"))
            .append(&revision)
            .await
            .expect("应登记 Genome");
        write_stable(&registry, &revision).await;
        let genome = GenomeSettings {
            root_dir: Some(registry.clone()),
            stable: Some("stable/general".into()),
            revision_id: None,
        };
        let evidence = EvidenceSettings::default();
        let mut draft =
            SessionRecord::new(SessionId::generate(), Session::new()).expect("应创建 Draft");

        let runtime = load_genome_session_runtime(
            &genome,
            &evidence,
            &root.join("config.toml"),
            &root,
            &mut draft,
        )
        .await
        .expect("Evidence 关闭时仍应解析 Genome");
        assert_eq!(
            draft.behavior_binding,
            Some(
                SessionBehaviorBinding::new(
                    GENOME_SESSION_BEHAVIOR_KIND,
                    revision.revision_id.to_string(),
                )
                .expect("应构造绑定")
            )
        );
        assert!(load_evidence_runtime(&evidence, &runtime)
            .await
            .expect("关闭 Evidence 应直接返回")
            .is_none());
        for directory in ["episodes", "outbox", "outcome-revisions"] {
            assert!(!registry.join(directory).exists());
        }
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// Stable 移动后旧 Session 必须解析原 Revision，新 Session 才使用新 Revision。
    #[tokio::test]
    async fn existing_session_ignores_moved_stable_while_new_session_uses_it() {
        let root = temp_root("stable-move");
        let registry = root.join("evolution");
        let artifacts = FileArtifactStore::new(registry.join("artifacts"));
        let first = test_genome_revision(ExecutionPolicy::serve(), &artifacts).await;
        let second = test_genome_revision(ExecutionPolicy::serve(), &artifacts).await;
        let genomes = FileGenomeStore::new(registry.join("genomes"));
        genomes.append(&first).await.expect("应登记第一版 Genome");
        genomes.append(&second).await.expect("应登记第二版 Genome");
        write_stable(&registry, &first).await;
        let genome = GenomeSettings {
            root_dir: Some(registry.clone()),
            stable: Some("stable/general".into()),
            revision_id: None,
        };
        let evidence = EvidenceSettings::default();
        let mut existing =
            SessionRecord::new(SessionId::generate(), Session::new()).expect("应创建 Session");
        load_genome_session_runtime(
            &genome,
            &evidence,
            &root.join("config.toml"),
            &root,
            &mut existing,
        )
        .await
        .expect("应绑定第一版");
        existing.revision = 1;

        write_stable(&registry, &second).await;
        let restored = load_genome_session_runtime(
            &genome,
            &evidence,
            &root.join("config.toml"),
            &root,
            &mut existing,
        )
        .await
        .expect("旧 Session 应按精确修订恢复");
        assert_eq!(
            restored.binding().expect("旧 Session 应有绑定").revision(),
            &first
        );

        let mut draft =
            SessionRecord::new(SessionId::generate(), Session::new()).expect("应创建新 Draft");
        let current = load_genome_session_runtime(
            &genome,
            &evidence,
            &root.join("config.toml"),
            &root,
            &mut draft,
        )
        .await
        .expect("新 Session 应解析移动后的 Stable");
        assert_eq!(
            current.binding().expect("新 Session 应有绑定").revision(),
            &second
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 无 Registry 的新 Draft 可以进入界面，但运行前必须失败且不得写 Legacy 标记。
    #[tokio::test]
    async fn unconfigured_new_session_cannot_start_run_or_become_legacy() {
        let root = temp_root("unconfigured");
        let mut draft =
            SessionRecord::new(SessionId::generate(), Session::new()).expect("应创建 Draft");
        let runtime = load_genome_session_runtime(
            &GenomeSettings::default(),
            &EvidenceSettings::default(),
            &root.join("config.toml"),
            &root,
            &mut draft,
        )
        .await
        .expect("未配置时应保留未启动 Draft");

        assert!(draft.behavior_binding.is_none());
        let error = runtime
            .validate_run_session(&mut draft)
            .expect_err("未绑定 Genome 的新 Draft 不得启动 Run");
        assert!(error.to_string().contains("genome.stable"));
        assert!(draft.behavior_binding.is_none());
        assert!(!root.exists());
    }

    /// 已持久化旧记录可加法迁移为 Legacy，但仍不得启动新 Run 或 Evidence。
    #[tokio::test]
    async fn historical_unbound_session_is_readable_but_not_runnable() {
        let root = temp_root("legacy");
        let mut history =
            SessionRecord::new(SessionId::generate(), Session::new()).expect("应创建旧 Session");
        history.revision = 3;
        let runtime = load_genome_session_runtime(
            &GenomeSettings::default(),
            &EvidenceSettings::default(),
            &root.join("config.toml"),
            &root,
            &mut history,
        )
        .await
        .expect("旧 Session 应可读取并标记");

        assert!(history
            .behavior_binding
            .as_ref()
            .is_some_and(SessionBehaviorBinding::is_legacy_unbound));
        assert!(runtime.validate_run_session(&mut history).is_err());
        let evidence = EvidenceSettings {
            enabled: true,
            ..EvidenceSettings::default()
        };
        assert!(load_evidence_runtime(&evidence, &runtime).await.is_err());
        assert!(!root.exists());
    }
}

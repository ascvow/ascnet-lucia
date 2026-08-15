//! Agent Genome：一次运行所使用的全部**行为**配置。
//!
//! 核心设计：非行为字段不是"计算摘要时被过滤掉"，而是**根本不在 [`AgentGenome`] 里**。
//! 创建时间、描述、来源路径等都放在 [`GenomeMetadata`]，由 [`GenomeRevision`] 单独携带。
//! 因此"时间或描述变化不影响行为摘要"是结构上的必然，而不是实现细节。
//!
//! 确定性：所有集合使用 `BTreeMap` / `BTreeSet`，列表在 [`AgentGenome::validate`]
//! 中要求已排序，因此同一份配置总是产生同一份序列化结果。
//!
//! 摘要计算本身属于 M1-03，本模块只负责结构与不变量。

use crate::ids::{ArtifactDigest, GenomeDigest, GenomeRevisionId};
use agent_tool::{ExecutionPolicy, ToolAccess};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

/// Genome 结构版本；字段语义变化时必须递增。
pub const GENOME_SCHEMA_VERSION: u32 = 1;

/// Genome 校验失败。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InvalidGenome {
    /// 列表未按稳定顺序排列，会导致序列化结果不稳定。
    #[error("{field} 必须按 {key} 升序排列且不得重复")]
    Unordered {
        /// 出问题的字段。
        field: &'static str,
        /// 排序键名。
        key: &'static str,
    },
    /// 能力 owner 指向了不存在或未启用的插件。
    #[error("能力 `{capability}` 的 owner `{plugin}` 不在已启用插件列表中")]
    UnknownCapabilityOwner {
        /// 能力 ID。
        capability: String,
        /// 被引用的插件 ID。
        plugin: String,
    },
    /// Genome 中出现了会话级 Prompt。
    #[error("Genome 不得包含 Session 层 Prompt：会话内容不属于行为配置")]
    SessionPromptInGenome,
    /// Schema 版本不受支持。
    #[error("不支持的 Genome schema 版本 {found}，当前支持 {supported}")]
    UnsupportedSchemaVersion {
        /// 实际版本。
        found: u32,
        /// 支持的版本。
        supported: u32,
    },
}

/// Prompt 分层。
///
/// 与 M1-05 的注入模型对应。变异表面只开放 [`PromptLayer::TaskStrategy`]，
/// 其余层不得被候选修改。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptLayer {
    /// 宿主协议约定，最先注入。
    HostProtocol,
    /// Agent 身份设定。
    Identity,
    /// 安全约束。
    Safety,
    /// 工具使用契约。
    ToolContract,
    /// 任务策略；唯一允许自动变异的层。
    TaskStrategy,
    /// Skill 注入内容。
    Skill,
    /// 会话级内容；**不属于** Genome，仅用于描述运行期组装结果。
    Session,
}

impl PromptLayer {
    /// 判断该层是否属于 Genome 的行为配置。
    pub fn belongs_to_genome(self) -> bool {
        !matches!(self, Self::Session)
    }

    /// 判断该层是否允许被自动变异。
    pub fn is_mutable_surface(self) -> bool {
        matches!(self, Self::TaskStrategy)
    }
}

/// 一条 Prompt 制品引用。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptArtifactRef {
    /// 该 Prompt 所处的层。
    pub layer: PromptLayer,
    /// Prompt 正文的内容摘要；正文本身存放在 CAS。
    pub artifact: ArtifactDigest,
}

/// Prompt 组装配置。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PromptGenome {
    /// 按注入顺序排列的 Prompt 制品；顺序本身是行为的一部分，因此不排序。
    #[serde(default)]
    pub messages: Vec<PromptArtifactRef>,
}

impl PromptGenome {
    /// 返回唯一可变表面对应的制品摘要。
    ///
    /// 出现零条或多条 `TaskStrategy` 时返回 `None`，调用方应视为不可变异。
    pub fn task_strategy(&self) -> Option<&ArtifactDigest> {
        let mut found = self
            .messages
            .iter()
            .filter(|message| message.layer.is_mutable_surface());
        let first = found.next()?;
        if found.next().is_some() {
            return None;
        }
        Some(&first.artifact)
    }
}

/// 模型行为配置。
///
/// 不含 `api_key`、`api_key_env` 与 `extra_headers`：前两者是凭据，后者可能携带凭据。
/// `base_url` 属于行为（不同端点可能是不同模型），因此保留。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelGenome {
    /// 逻辑服务商名称。
    pub provider: String,
    /// 服务商类型，例如 `open-ai`、`anthropic`。
    pub provider_kind: String,
    /// 发送给服务商的模型 ID。
    pub model: String,
    /// 可选 base URL。
    #[serde(default)]
    pub base_url: Option<String>,
    /// OpenAI 协议变体；非 OpenAI 服务商为 `None`。
    #[serde(default)]
    pub protocol: Option<String>,
    /// 最大输出 token 数。
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// 采样温度，以字符串保存以保证跨平台序列化稳定。
    ///
    /// 浮点数的十进制表示在不同平台上可能不同，而摘要必须逐字节一致。
    #[serde(default)]
    pub temperature: Option<String>,
    /// 是否使用流式接口。
    pub stream: bool,
    /// 服务商专属选项的内容摘要；正文可能含端点细节，因此只留摘要。
    #[serde(default)]
    pub provider_options_digest: Option<ArtifactDigest>,
}

/// 单个已启用插件的行为快照。
///
/// 不含显示名称、manifest 路径与下载来源：它们不影响运行行为。
/// 未启用的插件根本不进入 Genome。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PluginGenome {
    /// 插件稳定 ID。
    pub id: String,
    /// 已安装的语义化版本。
    pub version: String,
    /// 插件 ABI 版本。
    pub api_version: String,
    /// bundle 的内容摘要。
    pub bundle: ArtifactDigest,
    /// 插件配置的内容摘要；无配置时为 `None`。
    #[serde(default)]
    pub config_digest: Option<ArtifactDigest>,
}

/// 暴露给模型的工具集合。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ToolProfileGenome {
    /// 已注册的原生工具名。
    #[serde(default)]
    pub native_tools: BTreeSet<String>,
    /// 执行策略之外的工具访问范围。
    #[serde(default)]
    pub access: ToolAccess,
}

/// 由插件实现、但参数外部化的策略引用。
///
/// 插件代码本身由 [`PluginGenome`] 的 bundle 摘要固定；此处只记录它使用的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRef {
    /// 策略标识，通常为提供该策略的插件 ID。
    pub id: String,
    /// 策略参数的内容摘要。
    pub config_digest: ArtifactDigest,
}

/// 一条 Skill 制品引用。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SkillRef {
    /// Skill 稳定 ID。
    pub id: String,
    /// Skill 正文的内容摘要。
    pub content: ArtifactDigest,
}

/// Runtime 构建标识。
///
/// 同一份配置在不同构建下可能表现不同，因此构建信息属于行为的一部分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeIdentity {
    /// Cargo 包版本。
    pub package_version: String,
    /// 构建所用的 Git commit。
    pub git_commit: String,
    /// 构建时工作树是否有未提交改动。
    ///
    /// dirty 构建可以运行开发任务，但其 Genome 默认不得自动 Promote。
    pub git_dirty: bool,
    /// 目标平台三元组。
    pub target_triple: String,
    /// 启用的 feature 开关，已排序。
    #[serde(default)]
    pub features: BTreeSet<String>,
}

impl RuntimeIdentity {
    /// 判断该构建产出的 Genome 是否允许进入自动 Promote。
    pub fn is_promotable(&self) -> bool {
        !self.git_dirty
    }
}

/// 一次运行所使用的全部行为配置。
///
/// **不包含**：API Key、Secret 正文、会话消息、创建时间、描述、数据库主键。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGenome {
    /// 结构版本。
    pub schema_version: u32,
    /// Runtime 构建标识。
    pub runtime: RuntimeIdentity,
    /// 模型配置。
    pub model: ModelGenome,
    /// Prompt 组装配置。
    pub prompt: PromptGenome,
    /// 已启用插件，按 `id` 升序排列。
    #[serde(default)]
    pub plugins: Vec<PluginGenome>,
    /// 独占能力到插件 ID 的绑定。
    #[serde(default)]
    pub capability_owners: BTreeMap<String, String>,
    /// 工具集合。
    pub tools: ToolProfileGenome,
    /// 上下文压缩策略；未启用时为 `None`。
    #[serde(default)]
    pub context_policy: Option<PolicyRef>,
    /// 计划策略；未启用时为 `None`。
    #[serde(default)]
    pub planning_policy: Option<PolicyRef>,
    /// 已装载 Skill，按 `id` 升序排列。
    #[serde(default)]
    pub skills: Vec<SkillRef>,
    /// 执行安全策略与资源上限。
    pub execution: ExecutionPolicy,
}

impl AgentGenome {
    /// 校验结构不变量。
    ///
    /// 只有通过校验的 Genome 才可以计算摘要：未排序的列表会让同一份配置产生
    /// 不同的序列化结果，摘要因此失去意义。
    ///
    /// # Errors
    ///
    /// schema 版本不符、列表未排序或存在重复、能力 owner 指向未知插件，
    /// 或 Prompt 中出现 Session 层时返回 [`InvalidGenome`]。
    pub fn validate(&self) -> Result<(), InvalidGenome> {
        if self.schema_version != GENOME_SCHEMA_VERSION {
            return Err(InvalidGenome::UnsupportedSchemaVersion {
                found: self.schema_version,
                supported: GENOME_SCHEMA_VERSION,
            });
        }

        if !is_strictly_sorted(self.plugins.iter().map(|plugin| &plugin.id)) {
            return Err(InvalidGenome::Unordered {
                field: "plugins",
                key: "id",
            });
        }
        if !is_strictly_sorted(self.skills.iter().map(|skill| &skill.id)) {
            return Err(InvalidGenome::Unordered {
                field: "skills",
                key: "id",
            });
        }

        if self
            .prompt
            .messages
            .iter()
            .any(|message| !message.layer.belongs_to_genome())
        {
            return Err(InvalidGenome::SessionPromptInGenome);
        }

        let enabled: BTreeSet<&str> = self
            .plugins
            .iter()
            .map(|plugin| plugin.id.as_str())
            .collect();
        for (capability, owner) in &self.capability_owners {
            if !enabled.contains(owner.as_str()) {
                return Err(InvalidGenome::UnknownCapabilityOwner {
                    capability: capability.clone(),
                    plugin: owner.clone(),
                });
            }
        }

        Ok(())
    }
}

/// 判断迭代器产出的键是否严格升序（即已排序且无重复）。
fn is_strictly_sorted<'a, I>(items: I) -> bool
where
    I: Iterator<Item = &'a String>,
{
    let mut previous: Option<&String> = None;
    for item in items {
        if let Some(last) = previous {
            if last >= item {
                return false;
            }
        }
        previous = Some(item);
    }
    true
}

/// Genome 的非行为元数据。
///
/// 这些字段**不参与**摘要计算，因此改动它们不会产生新的行为版本。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct GenomeMetadata {
    /// 登记时间，RFC 3339 文本。
    #[serde(default)]
    pub created_at: Option<String>,
    /// 人工描述。
    #[serde(default)]
    pub description: Option<String>,
    /// 派生来源的上一版修订。
    #[serde(default)]
    pub parent: Option<GenomeRevisionId>,
    /// 产生该修订的变异提案；人工登记时为 `None`。
    #[serde(default)]
    pub mutation: Option<crate::ids::MutationId>,
}

/// 一次 Genome 登记。
///
/// [`GenomeRevision::genome`] 决定行为并产生 [`GenomeRevision::digest`]；
/// [`GenomeRevision::metadata`] 只用于展示与追溯。同一 `digest` 可以对应多次登记。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenomeRevision {
    /// 本次登记的标识。
    pub revision_id: GenomeRevisionId,
    /// 行为摘要，由 M1-03 的规范序列化计算。
    pub digest: GenomeDigest,
    /// 行为配置。
    pub genome: AgentGenome,
    /// 非行为元数据。
    #[serde(default)]
    pub metadata: GenomeMetadata,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一份最小可用的合法 Genome。
    fn sample() -> AgentGenome {
        let digest = |seed: char| {
            ArtifactDigest::from_sha256_hex(seed.to_string().repeat(64)).expect("摘要应合法")
        };

        AgentGenome {
            schema_version: GENOME_SCHEMA_VERSION,
            runtime: RuntimeIdentity {
                package_version: "0.1.0".into(),
                git_commit: "abc123".into(),
                git_dirty: false,
                target_triple: "aarch64-apple-darwin".into(),
                features: ["plugins".to_string()].into_iter().collect(),
            },
            model: ModelGenome {
                provider: "default".into(),
                provider_kind: "anthropic".into(),
                model: "claude-opus-5".into(),
                base_url: None,
                protocol: None,
                max_tokens: Some(4096),
                temperature: Some("0.2".into()),
                stream: true,
                provider_options_digest: None,
            },
            prompt: PromptGenome {
                messages: vec![
                    PromptArtifactRef {
                        layer: PromptLayer::Safety,
                        artifact: digest('a'),
                    },
                    PromptArtifactRef {
                        layer: PromptLayer::TaskStrategy,
                        artifact: digest('b'),
                    },
                ],
            },
            plugins: vec![
                PluginGenome {
                    id: "context".into(),
                    version: "0.1.0".into(),
                    api_version: "0.7.0".into(),
                    bundle: digest('c'),
                    config_digest: None,
                },
                PluginGenome {
                    id: "permission".into(),
                    version: "0.1.0".into(),
                    api_version: "0.7.0".into(),
                    bundle: digest('d'),
                    config_digest: None,
                },
            ],
            capability_owners: [("agent.tool-policy".to_string(), "permission".to_string())]
                .into_iter()
                .collect(),
            tools: ToolProfileGenome {
                native_tools: ["read_file".to_string(), "shell".to_string()]
                    .into_iter()
                    .collect(),
                access: ToolAccess::All,
            },
            context_policy: None,
            planning_policy: None,
            skills: Vec::new(),
            execution: ExecutionPolicy::serve(),
        }
    }

    #[test]
    fn sample_genome_is_valid() {
        sample().validate().expect("样例 Genome 应合法");
    }

    /// 同一份配置反复序列化必须逐字节一致。
    #[test]
    fn serialization_is_stable_for_identical_configuration() {
        let first = serde_json::to_string(&sample()).expect("应可序列化");
        let second = serde_json::to_string(&sample()).expect("应可序列化");

        assert_eq!(first, second);
    }

    /// 集合字段的插入顺序不同，也必须得到相同序列化结果。
    #[test]
    fn collection_order_does_not_affect_serialization() {
        let baseline = serde_json::to_string(&sample()).expect("应可序列化");

        let mut shuffled = sample();
        // BTreeSet 与 BTreeMap 按键排序，插入顺序不影响结果。
        shuffled.tools.native_tools = ["shell".to_string(), "read_file".to_string()]
            .into_iter()
            .collect();
        shuffled.runtime.features = ["plugins".to_string()].into_iter().collect();

        assert_eq!(
            serde_json::to_string(&shuffled).expect("应可序列化"),
            baseline
        );
    }

    /// 行为字段变化必须体现在序列化结果中。
    #[test]
    fn behavioural_changes_alter_serialization() {
        let baseline = serde_json::to_string(&sample()).expect("应可序列化");

        let mut changed = sample();
        changed.model.model = "claude-sonnet-5".into();
        assert_ne!(
            serde_json::to_string(&changed).expect("应可序列化"),
            baseline
        );

        let mut changed = sample();
        changed.prompt.messages[1].artifact =
            ArtifactDigest::from_sha256_hex("f".repeat(64)).expect("摘要应合法");
        assert_ne!(
            serde_json::to_string(&changed).expect("应可序列化"),
            baseline
        );

        let mut changed = sample();
        changed.plugins[0].bundle =
            ArtifactDigest::from_sha256_hex("e".repeat(64)).expect("摘要应合法");
        assert_ne!(
            serde_json::to_string(&changed).expect("应可序列化"),
            baseline
        );

        let mut changed = sample();
        changed
            .capability_owners
            .insert("agent.tool-policy".into(), "context".into());
        assert_ne!(
            serde_json::to_string(&changed).expect("应可序列化"),
            baseline
        );

        let mut changed = sample();
        changed.execution = ExecutionPolicy::evaluation("/tmp/fixture");
        assert_ne!(
            serde_json::to_string(&changed).expect("应可序列化"),
            baseline
        );
    }

    /// 时间、描述与父引用变化不得影响行为部分。
    #[test]
    fn metadata_changes_leave_the_genome_untouched() {
        let genome = sample();
        let baseline = serde_json::to_string(&genome).expect("应可序列化");

        let revision = GenomeRevision {
            revision_id: GenomeRevisionId::generate(),
            digest: GenomeDigest::from_sha256_hex("0".repeat(64)).expect("摘要应合法"),
            genome: genome.clone(),
            metadata: GenomeMetadata {
                created_at: Some("2026-08-15T00:00:00Z".into()),
                description: Some("首个版本".into()),
                parent: None,
                mutation: None,
            },
        };

        let mut other = revision.clone();
        other.revision_id = GenomeRevisionId::generate();
        other.metadata.created_at = Some("2030-01-01T00:00:00Z".into());
        other.metadata.description = Some("完全不同的描述".into());
        other.metadata.parent = Some(GenomeRevisionId::generate());

        // 元数据在 Genome 之外，因此行为部分逐字节不变。
        assert_eq!(
            serde_json::to_string(&other.genome).expect("应可序列化"),
            baseline
        );
        assert_eq!(other.genome, revision.genome);
    }

    #[test]
    fn rejects_unsorted_plugins() {
        let mut genome = sample();
        genome.plugins.reverse();

        assert_eq!(
            genome.validate().expect_err("乱序插件应被拒绝"),
            InvalidGenome::Unordered {
                field: "plugins",
                key: "id"
            }
        );
    }

    #[test]
    fn rejects_duplicate_plugin_ids() {
        let mut genome = sample();
        genome.plugins[1].id = "context".into();

        assert!(genome.validate().is_err(), "重复插件 ID 应被拒绝");
    }

    #[test]
    fn rejects_session_prompt() {
        let mut genome = sample();
        genome.prompt.messages.push(PromptArtifactRef {
            layer: PromptLayer::Session,
            artifact: ArtifactDigest::from_sha256_hex("1".repeat(64)).expect("摘要应合法"),
        });

        assert_eq!(
            genome.validate().expect_err("会话 Prompt 应被拒绝"),
            InvalidGenome::SessionPromptInGenome
        );
    }

    #[test]
    fn rejects_capability_owner_outside_enabled_plugins() {
        let mut genome = sample();
        genome
            .capability_owners
            .insert("agent.context-loader".into(), "missing".into());

        assert!(genome.validate().is_err(), "未知 owner 应被拒绝");
    }

    #[test]
    fn task_strategy_is_the_only_mutable_surface() {
        let genome = sample();
        assert!(genome.prompt.task_strategy().is_some());

        assert!(PromptLayer::TaskStrategy.is_mutable_surface());
        for layer in [
            PromptLayer::HostProtocol,
            PromptLayer::Identity,
            PromptLayer::Safety,
            PromptLayer::ToolContract,
            PromptLayer::Skill,
        ] {
            assert!(!layer.is_mutable_surface(), "{layer:?} 不应可变异");
        }
    }

    /// 出现多条 TaskStrategy 时不应给出可变异表面，避免变异目标歧义。
    #[test]
    fn ambiguous_task_strategy_yields_no_surface() {
        let mut genome = sample();
        genome.prompt.messages.push(PromptArtifactRef {
            layer: PromptLayer::TaskStrategy,
            artifact: ArtifactDigest::from_sha256_hex("2".repeat(64)).expect("摘要应合法"),
        });

        assert!(genome.prompt.task_strategy().is_none());
    }

    #[test]
    fn dirty_builds_are_not_promotable() {
        let mut genome = sample();
        assert!(genome.runtime.is_promotable());

        genome.runtime.git_dirty = true;
        assert!(!genome.runtime.is_promotable());
    }

    #[test]
    fn genome_round_trips_through_json() {
        let genome = sample();
        let encoded = serde_json::to_string(&genome).expect("应可序列化");
        let decoded: AgentGenome = serde_json::from_str(&encoded).expect("应可反序列化");

        assert_eq!(decoded, genome);
        decoded.validate().expect("反序列化结果应仍然合法");
    }
}

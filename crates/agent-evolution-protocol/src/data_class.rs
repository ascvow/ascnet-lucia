//! 数据分级、进化资格与 Episode 数据策略。
//!
//! 核心原则：**默认不可用**。未经显式标记的运行证据一律不进入变异输入，
//! 因此新增字段或新增来源时的安全默认值总是最严格的那个。

use serde::{Deserialize, Serialize};

/// 运行证据的敏感级别。
///
/// 级别之间有序：`Public < Internal < Sensitive < Secret`。组合多份数据时应取
/// 其中最高级别，见 [`DataClass::max`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    /// 可公开的内容，例如开源仓库中的文件路径与公共文档。
    Public,
    /// 组织内部内容，不含个人信息与凭据。
    Internal,
    /// 含个人信息、私有业务数据或私有路径。
    Sensitive,
    /// 含凭据本身：API Key、Token、Cookie、私钥。
    ///
    /// 作为默认值：来源不明的数据必须先被当作最敏感的一类处理。
    #[default]
    Secret,
}

impl DataClass {
    /// 返回两者中更敏感的一级。
    pub fn max(self, other: Self) -> Self {
        if other > self {
            other
        } else {
            self
        }
    }

    /// 判断该级别的数据是否允许在脱敏后离开本机。
    ///
    /// `Secret` 即使脱敏也不外发：脱敏失败是可能的，而凭据外泄不可逆。
    pub fn may_leave_host_after_redaction(self) -> bool {
        matches!(self, Self::Public | Self::Internal)
    }
}

/// Episode 进入进化流程的资格。
///
/// 默认值为 [`EvolutionEligibility::NotEligible`]，因此生产 Session 不会因为
/// 忘记标记就自动成为变异输入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvolutionEligibility {
    /// 不得进入任何进化流程，只能用于本地调试。
    #[default]
    NotEligible,
    /// 完成脱敏后方可使用；脱敏之前等同于 `NotEligible`。
    EligibleAfterRedaction,
    /// 可用于本机进化，不得离开本机。
    EligibleForLocalEvolution,
    /// 可用于共享评测，允许离开本机。
    EligibleForSharedEvaluation,
}

impl EvolutionEligibility {
    /// 判断当前状态下该 Episode 能否作为变异输入。
    ///
    /// `redacted` 表示脱敏是否已经完成。`EligibleAfterRedaction` 只有在脱敏
    /// 完成后才放行，避免"标记为待脱敏"被误当作"已可用"。
    pub fn permits_mutation_input(self, redacted: bool) -> bool {
        match self {
            Self::NotEligible => false,
            Self::EligibleAfterRedaction => redacted,
            Self::EligibleForLocalEvolution | Self::EligibleForSharedEvaluation => true,
        }
    }

    /// 判断该 Episode 能否用于共享评测（可能离开本机）。
    pub fn permits_sharing(self) -> bool {
        matches!(self, Self::EligibleForSharedEvaluation)
    }
}

/// 原始 ToolResult 的保存方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawToolResultPolicy {
    /// 不保存工具结果正文，只保留形态与摘要。
    #[default]
    Discard,
    /// 保存脱敏后的正文。
    StoreRedacted,
    /// 保存未经脱敏的原始正文；仅允许用于 `Public` 与 `Internal` 数据。
    StoreRaw,
}

impl RawToolResultPolicy {
    /// 判断该保存方式对给定数据级别是否合法。
    ///
    /// 原始保存只对不敏感数据开放；`Sensitive` 与 `Secret` 必须至少经过脱敏。
    pub fn is_valid_for(self, class: DataClass) -> bool {
        match self {
            Self::Discard | Self::StoreRedacted => true,
            Self::StoreRaw => matches!(class, DataClass::Public | DataClass::Internal),
        }
    }
}

/// 按数据级别设定的保留期。
///
/// `None` 表示不限期保留，只允许用于 `Public`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionPolicy {
    /// 保留天数；`None` 表示不限期。
    pub retain_days: Option<u32>,
}

impl RetentionPolicy {
    /// 返回该数据级别的默认保留期。
    ///
    /// 越敏感保留越短：凭据类数据不落盘保留，因此为 0 天。
    pub fn default_for(class: DataClass) -> Self {
        let retain_days = match class {
            DataClass::Public => None,
            DataClass::Internal => Some(180),
            DataClass::Sensitive => Some(30),
            DataClass::Secret => Some(0),
        };
        Self { retain_days }
    }

    /// 判断给定存活天数是否已超出保留期。
    pub fn is_expired(&self, age_days: u32) -> bool {
        match self.retain_days {
            Some(limit) => age_days > limit,
            None => false,
        }
    }
}

/// Episode 中按用途划分的字段类别。
///
/// 这些类别与具体 Episode Schema 解耦：M2 实现 Episode Store 时按类别归置字段，
/// 而访问控制规则在此处一次性定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeFieldClass {
    /// 结构化结局：成功、失败、取消。
    Outcome,
    /// 失败分类。
    FailureClass,
    /// 工具调用的形态：工具名、参数键、调用顺序，不含参数值。
    ToolCallShape,
    /// Prompt 制品引用与摘要哈希。
    PromptArtifactRef,
    /// 脱敏后的工具结果正文。
    RedactedToolResult,
    /// 步数、时延等时间信息。
    Timing,
    /// Token 与成本用量。
    Usage,
    /// 未经脱敏的工具结果正文。
    RawToolResult,
    /// 未经脱敏的模型响应正文。
    RawModelResponse,
    /// 用户输入原文。
    UserContent,
    /// 模型隐藏推理内容。
    HiddenReasoning,
}

impl EpisodeFieldClass {
    /// 判断该类别是否允许持久化到 Episode Store。
    ///
    /// 隐藏推理内容一律不持久化：它既不构成可验证证据，又显著扩大泄漏面。
    pub fn is_persistable(self) -> bool {
        !matches!(self, Self::HiddenReasoning)
    }

    /// 判断 Mutator 是否可以读取该类别。
    ///
    /// Mutator 只看得到"发生了什么形态的失败"，看不到具体内容。这样它无法
    /// 把用户数据或隐藏答案写进候选 Prompt。
    pub fn is_mutator_readable(self) -> bool {
        matches!(
            self,
            Self::Outcome
                | Self::FailureClass
                | Self::ToolCallShape
                | Self::PromptArtifactRef
                | Self::RedactedToolResult
                | Self::Timing
                | Self::Usage
        )
    }
}

/// 单个 Episode 携带的数据策略。
///
/// M2 实现 Episode Store 时，该结构应作为 Episode Header 的一部分持久化，
/// 使每条证据自带分级与资格，而不依赖外部表格。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EpisodeDataPolicy {
    /// 该 Episode 的敏感级别。
    pub data_class: DataClass,
    /// 进化资格。
    pub eligibility: EvolutionEligibility,
    /// 已应用的脱敏规则版本；`None` 表示尚未脱敏。
    #[serde(default)]
    pub redaction_rules_version: Option<String>,
    /// 保留期。
    pub retention: RetentionPolicy,
    /// 原始工具结果的保存方式。
    pub raw_tool_results: RawToolResultPolicy,
}

impl Default for EpisodeDataPolicy {
    /// 最严格的默认值：按 Secret 处理、无进化资格、不保存原始工具结果。
    fn default() -> Self {
        Self {
            data_class: DataClass::Secret,
            eligibility: EvolutionEligibility::NotEligible,
            redaction_rules_version: None,
            retention: RetentionPolicy::default_for(DataClass::Secret),
            raw_tool_results: RawToolResultPolicy::Discard,
        }
    }
}

impl EpisodeDataPolicy {
    /// 按数据级别构造一份默认策略，资格仍为 `NotEligible`。
    pub fn for_class(data_class: DataClass) -> Self {
        Self {
            data_class,
            eligibility: EvolutionEligibility::NotEligible,
            redaction_rules_version: None,
            retention: RetentionPolicy::default_for(data_class),
            raw_tool_results: RawToolResultPolicy::Discard,
        }
    }

    /// 判断脱敏是否已完成。
    pub fn is_redacted(&self) -> bool {
        self.redaction_rules_version.is_some()
    }

    /// 判断该 Episode 当前能否作为 Mutator 输入。
    pub fn permits_mutation_input(&self) -> bool {
        self.eligibility.permits_mutation_input(self.is_redacted())
    }

    /// 校验策略组合自洽。
    ///
    /// # Errors
    ///
    /// 原始工具结果保存方式与数据级别冲突，或敏感数据被标记为可共享时返回原因。
    pub fn validate(&self) -> Result<(), String> {
        if !self.raw_tool_results.is_valid_for(self.data_class) {
            return Err(format!(
                "{:?} 级数据不允许保存未脱敏的工具结果",
                self.data_class
            ));
        }
        if self.eligibility.permits_sharing() && !self.data_class.may_leave_host_after_redaction() {
            return Err(format!("{:?} 级数据不允许用于共享评测", self.data_class));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_most_restrictive() {
        let policy = EpisodeDataPolicy::default();

        assert_eq!(policy.data_class, DataClass::Secret);
        assert_eq!(policy.eligibility, EvolutionEligibility::NotEligible);
        assert!(!policy.permits_mutation_input());
        assert_eq!(policy.raw_tool_results, RawToolResultPolicy::Discard);
    }

    #[test]
    fn pending_redaction_is_not_yet_usable() {
        let mut policy = EpisodeDataPolicy::for_class(DataClass::Internal);
        policy.eligibility = EvolutionEligibility::EligibleAfterRedaction;

        assert!(!policy.permits_mutation_input(), "脱敏完成前不得作为输入");

        policy.redaction_rules_version = Some(crate::REDACTION_RULES_VERSION.to_string());
        assert!(policy.permits_mutation_input(), "脱敏完成后方可使用");
    }

    #[test]
    fn sensitive_data_cannot_be_shared_or_stored_raw() {
        let mut policy = EpisodeDataPolicy::for_class(DataClass::Sensitive);
        policy.eligibility = EvolutionEligibility::EligibleForSharedEvaluation;
        assert!(policy.validate().is_err(), "敏感数据不应可共享");

        let mut policy = EpisodeDataPolicy::for_class(DataClass::Sensitive);
        policy.raw_tool_results = RawToolResultPolicy::StoreRaw;
        assert!(policy.validate().is_err(), "敏感数据不应原样保存工具结果");
    }

    #[test]
    fn hidden_reasoning_is_never_persisted_or_readable() {
        assert!(!EpisodeFieldClass::HiddenReasoning.is_persistable());
        assert!(!EpisodeFieldClass::HiddenReasoning.is_mutator_readable());
    }

    #[test]
    fn mutator_sees_shape_but_not_content() {
        assert!(EpisodeFieldClass::FailureClass.is_mutator_readable());
        assert!(EpisodeFieldClass::ToolCallShape.is_mutator_readable());
        assert!(EpisodeFieldClass::RedactedToolResult.is_mutator_readable());

        assert!(!EpisodeFieldClass::RawToolResult.is_mutator_readable());
        assert!(!EpisodeFieldClass::RawModelResponse.is_mutator_readable());
        assert!(!EpisodeFieldClass::UserContent.is_mutator_readable());
    }

    #[test]
    fn retention_shortens_as_sensitivity_rises() {
        assert_eq!(
            RetentionPolicy::default_for(DataClass::Public).retain_days,
            None
        );
        assert_eq!(
            RetentionPolicy::default_for(DataClass::Internal).retain_days,
            Some(180)
        );
        assert_eq!(
            RetentionPolicy::default_for(DataClass::Sensitive).retain_days,
            Some(30)
        );
        assert_eq!(
            RetentionPolicy::default_for(DataClass::Secret).retain_days,
            Some(0)
        );
        assert!(RetentionPolicy::default_for(DataClass::Secret).is_expired(1));
    }

    #[test]
    fn data_class_combination_takes_the_most_sensitive() {
        assert_eq!(
            DataClass::Public.max(DataClass::Sensitive),
            DataClass::Sensitive
        );
        assert_eq!(DataClass::Secret.max(DataClass::Public), DataClass::Secret);
    }

    #[test]
    fn policy_round_trips_through_json() {
        let policy = EpisodeDataPolicy::for_class(DataClass::Internal);
        let encoded = serde_json::to_string(&policy).expect("应可序列化");
        let decoded: EpisodeDataPolicy = serde_json::from_str(&encoded).expect("应可反序列化");

        assert_eq!(policy, decoded);
    }
}

//! 固定的 Evolution 候选生成策略。

use crate::candidate_builder::MAX_TASK_STRATEGY_PROMPT_BYTES;
use agent_evolution_protocol::{MutationSurface, MIN_CANDIDATES_PER_CYCLE};
use std::collections::BTreeSet;

/// 当前内置 Evolution Policy 的稳定版本。
pub const EVOLUTION_POLICY_VERSION: &str = "task-strategy-mvp-v1";

/// MVP 每轮必须生成的 Prompt 候选数量。
pub const TASK_STRATEGY_MVP_CANDIDATE_COUNT: usize = MIN_CANDIDATES_PER_CYCLE as usize;

/// 受信控制面提供的只读 Evolution Policy。
///
/// Policy 不实现反序列化，也不开放任意构造器，避免 Mutator、模型输出或外部请求放宽
/// 变异表面与资源边界。策略变化必须通过代码评审发布新版本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvolutionPolicy {
    version: &'static str,
    allowed_surfaces: BTreeSet<MutationSurface>,
    candidate_count: usize,
    max_prompt_bytes: usize,
    max_hypothesis_bytes: usize,
    max_expected_effects: usize,
    max_expected_effect_bytes: usize,
}

impl EvolutionPolicy {
    /// 返回只允许 Task Strategy Prompt 变化的固定 MVP Policy。
    pub fn task_strategy_mvp() -> Self {
        Self {
            version: EVOLUTION_POLICY_VERSION,
            allowed_surfaces: [MutationSurface::TaskStrategyPrompt].into_iter().collect(),
            candidate_count: TASK_STRATEGY_MVP_CANDIDATE_COUNT,
            max_prompt_bytes: MAX_TASK_STRATEGY_PROMPT_BYTES as usize,
            max_hypothesis_bytes: 4 * 1024,
            max_expected_effects: 8,
            max_expected_effect_bytes: 2 * 1024,
        }
    }

    /// 返回 Policy 稳定版本。
    pub fn version(&self) -> &'static str {
        self.version
    }

    /// 返回允许的全部变异表面。
    pub fn allowed_surfaces(&self) -> &BTreeSet<MutationSurface> {
        &self.allowed_surfaces
    }

    /// 判断指定表面能否由当前 Policy 修改。
    pub fn allows_surface(&self, surface: &MutationSurface) -> bool {
        self.allowed_surfaces.contains(surface)
    }

    /// 返回每轮必须生成的候选数量。
    pub fn candidate_count(&self) -> usize {
        self.candidate_count
    }

    /// 返回单个 Task Strategy Prompt 的 UTF-8 字节上限。
    pub fn max_prompt_bytes(&self) -> usize {
        self.max_prompt_bytes
    }

    /// 返回单个候选假设的 UTF-8 字节上限。
    pub fn max_hypothesis_bytes(&self) -> usize {
        self.max_hypothesis_bytes
    }

    /// 返回单个候选允许声明的最大预期效果数量。
    pub fn max_expected_effects(&self) -> usize {
        self.max_expected_effects
    }

    /// 返回单条预期效果的 UTF-8 字节上限。
    pub fn max_expected_effect_bytes(&self) -> usize {
        self.max_expected_effect_bytes
    }
}

impl Default for EvolutionPolicy {
    fn default() -> Self {
        Self::task_strategy_mvp()
    }
}

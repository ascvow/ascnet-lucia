//! 执行安全 Profile 与运行策略。
//!
//! 本模块定义 Lucia 在不同信任平面下的运行约束，见 ADR-0001。策略放在 `agent-tool`
//! 是因为它是依赖叶子：`agent-core`、`agent-runtime` 和 `agent-plugin-host` 都能引用
//! 同一份定义，而不必反向依赖。
//!
//! 核心不变量：**策略只能收缩，不能扩张**。所有组合操作都通过
//! [`ExecutionPolicy::restrict`] 完成，它在任何输入下都不会返回比 `self` 更宽的结果。
//! WASM 插件经 JSON ABI 通信，不接触本模块的任何类型，因此无法提升自身权限。

use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::PathBuf};

/// Agent 运行所处的信任平面。
///
/// 严格程度依次递增：`Serve` 最宽，`Mutation` 最窄。[`ExecutionProfile::restrict`]
/// 取两者中更严格的一个，因此不存在从 `Evaluation` 回到 `Serve` 的路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionProfile {
    /// 正常为用户执行任务，允许真实副作用。
    #[default]
    Serve,
    /// 评测候选，默认拒绝真实网络、真实 Secret 与进程执行。
    Evaluation,
    /// 生成候选，只读脱敏证据，只写候选制品。
    Mutation,
}

impl ExecutionProfile {
    /// 返回严格程度排名，数值越大限制越强。
    fn strictness(self) -> u8 {
        match self {
            Self::Serve => 0,
            Self::Evaluation => 1,
            Self::Mutation => 2,
        }
    }

    /// 返回两个 Profile 中更严格的一个。
    ///
    /// 该操作满足单调性：结果的严格程度不低于任一输入，因此调用方无法借此放宽限制。
    pub fn restrict(self, requested: Self) -> Self {
        if requested.strictness() > self.strictness() {
            requested
        } else {
            self
        }
    }
}

/// 工具访问范围。
///
/// 从 `agent-runtime` 上移到此处，使派生 Agent 的 allowlist 与 Profile 的工具门禁
/// 复用同一套语义，避免两处实现产生分歧。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", content = "tools", rename_all = "snake_case")]
pub enum ToolAccess {
    /// 继承父节点当前允许的全部工具，不代表绕过父节点限制。
    #[default]
    All,
    /// 只允许集合中列出的工具。
    Allowlist(BTreeSet<String>),
}

impl ToolAccess {
    /// 创建一个工具 allowlist。
    pub fn allowlist<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Allowlist(names.into_iter().map(Into::into).collect())
    }

    /// 创建一个不允许任何工具的空 allowlist。
    pub fn none() -> Self {
        Self::Allowlist(BTreeSet::new())
    }

    /// 判断当前范围是否允许指定工具。
    pub fn permits(&self, name: &str) -> bool {
        match self {
            Self::All => true,
            Self::Allowlist(names) => names.contains(name),
        }
    }

    /// 在当前范围内应用下一层限制。
    ///
    /// 返回值只可能保持或收缩当前权限，子节点请求 `All` 也不会恢复父节点已移除的工具。
    pub fn restrict(&self, requested: &Self) -> Self {
        match (self, requested) {
            (Self::All, next) => next.clone(),
            (current @ Self::Allowlist(_), Self::All) => current.clone(),
            (Self::Allowlist(current), Self::Allowlist(requested)) => {
                Self::Allowlist(current.intersection(requested).cloned().collect())
            }
        }
    }
}

/// 原生文件工具可访问的目录范围。
///
/// 本类型只表达**声明**。真正的路径 canonicalize、`..` 逃逸拒绝和 symlink 检查属于
/// M0-04 的原生工具收紧范围；在那之前 [`FilesystemScope::Root`] 不构成完整强制边界。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", content = "root", rename_all = "snake_case")]
pub enum FilesystemScope {
    /// 不限制目录，反映 Serve 平面当前的实际行为。
    #[default]
    Unrestricted,
    /// 限制在指定根目录内。
    Root(PathBuf),
    /// 完全拒绝文件访问。
    Denied,
}

impl FilesystemScope {
    /// 在当前范围内应用下一层限制，结果不会比 `self` 更宽。
    ///
    /// 两个 `Root` 组合时，只有请求路径位于当前根目录之下才会采纳；否则收缩为
    /// [`FilesystemScope::Denied`]，确保无法借助无关路径逃逸。这里使用词法前缀比较，
    /// 因为该判断只用于收缩，不用于放行。
    pub fn restrict(&self, requested: &Self) -> Self {
        match (self, requested) {
            (Self::Denied, _) | (_, Self::Denied) => Self::Denied,
            (Self::Unrestricted, next) => next.clone(),
            (current, Self::Unrestricted) => current.clone(),
            (Self::Root(current), Self::Root(requested)) => {
                if requested.starts_with(current) {
                    Self::Root(requested.clone())
                } else {
                    Self::Denied
                }
            }
        }
    }
}

/// 资源上限。
///
/// `None` 表示该维度不设上限。[`ResourceLimits::restrict`] 把 `None` 视为无穷大，
/// 因此组合结果总是取更小的那个限额。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResourceLimits {
    /// 单条指令允许的最大 ReAct 步数。
    #[serde(default)]
    pub max_steps: Option<usize>,
    /// 单次模型请求的最大输出 token 数。
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Agent 派生树的最大深度。
    #[serde(default)]
    pub max_depth: Option<usize>,
    /// 单个 Agent 允许派生的最大子 Agent 数。
    #[serde(default)]
    pub max_children_per_agent: Option<usize>,
    /// 同时运行的最大 Agent 数。
    #[serde(default)]
    pub max_concurrent_agents: Option<usize>,
    /// 单次运行的墙钟时间上限，单位毫秒。
    #[serde(default)]
    pub wall_clock_ms: Option<u64>,
    /// 允许创建的最大子进程数。
    #[serde(default)]
    pub max_processes: Option<usize>,
}

/// 取两个可选限额中更严格的一个，`None` 视为无上限。
fn tighter<T: Ord + Copy>(current: Option<T>, requested: Option<T>) -> Option<T> {
    match (current, requested) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, next) => next,
    }
}

impl ResourceLimits {
    /// 逐维度取更小的限额，结果不会放宽任何一项。
    pub fn restrict(&self, requested: &Self) -> Self {
        Self {
            max_steps: tighter(self.max_steps, requested.max_steps),
            max_tokens: tighter(self.max_tokens, requested.max_tokens),
            max_depth: tighter(self.max_depth, requested.max_depth),
            max_children_per_agent: tighter(
                self.max_children_per_agent,
                requested.max_children_per_agent,
            ),
            max_concurrent_agents: tighter(
                self.max_concurrent_agents,
                requested.max_concurrent_agents,
            ),
            wall_clock_ms: tighter(self.wall_clock_ms, requested.wall_clock_ms),
            max_processes: tighter(self.max_processes, requested.max_processes),
        }
    }

    /// 在限额存在时把给定值压到上限之内，否则原样返回。
    pub fn clamp_steps(&self, value: usize) -> usize {
        match self.max_steps {
            Some(limit) => value.min(limit),
            None => value,
        }
    }

    /// 在限额存在时把给定 token 上限压到策略上限之内。
    ///
    /// 调用方未设置时直接采用策略上限，避免"不设置即无限制"绕过 Profile。
    pub fn clamp_tokens(&self, value: Option<u32>) -> Option<u32> {
        tighter(self.max_tokens, value)
    }
}

/// 一个运行平面的完整安全策略。
///
/// `profile` 为私有字段，只能通过 [`ExecutionPolicy::serve`]、
/// [`ExecutionPolicy::evaluation`]、[`ExecutionPolicy::mutation`] 构造，
/// 并且只能通过 [`ExecutionPolicy::restrict`] 变得更严格。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPolicy {
    profile: ExecutionProfile,
    /// 允许暴露给模型并实际执行的工具范围。
    #[serde(default)]
    pub tools: ToolAccess,
    /// 原生文件工具的目录范围。
    #[serde(default)]
    pub filesystem: FilesystemScope,
    /// 是否请求允许访问真实网络；真实入口必须调用 `permits_network_access` 复核。
    #[serde(default)]
    pub allow_network: bool,
    /// 是否请求允许注入真实 Secret；真实入口必须调用 `permits_secret_access` 复核。
    #[serde(default)]
    pub allow_secrets: bool,
    /// 是否请求允许 Shell 与子进程执行；真实入口必须调用 `permits_process_execution` 复核。
    #[serde(default)]
    pub allow_process: bool,
    /// 资源上限。
    #[serde(default)]
    pub limits: ResourceLimits,
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self::serve()
    }
}

impl ExecutionPolicy {
    /// Serve 平面策略：保持现有功能，不额外收紧。
    ///
    /// 授权仍由 Host 的工具门禁与工具策略插件共同决定；本策略不放宽任何既有限制。
    pub fn serve() -> Self {
        Self {
            profile: ExecutionProfile::Serve,
            tools: ToolAccess::All,
            filesystem: FilesystemScope::Unrestricted,
            allow_network: true,
            allow_secrets: true,
            allow_process: true,
            limits: ResourceLimits::default(),
        }
    }

    /// Evaluation 平面策略：默认拒绝真实网络、真实 Secret 与进程执行。
    ///
    /// 工具默认为空 allowlist，必须由 Evaluation Policy 逐个开放；`fixture_root`
    /// 指定候选唯一可访问的 Fixture Workspace。
    pub fn evaluation(fixture_root: impl Into<PathBuf>) -> Self {
        Self {
            profile: ExecutionProfile::Evaluation,
            tools: ToolAccess::none(),
            filesystem: FilesystemScope::Root(fixture_root.into()),
            allow_network: false,
            allow_secrets: false,
            allow_process: false,
            limits: ResourceLimits {
                max_steps: Some(64),
                max_tokens: Some(4096),
                max_depth: Some(2),
                max_children_per_agent: Some(4),
                max_concurrent_agents: Some(2),
                wall_clock_ms: Some(120_000),
                max_processes: Some(0),
            },
        }
    }

    /// Mutation 平面策略：不执行任务，只生成候选。
    ///
    /// 不开放任何 Agent 工具，也不允许文件访问；Episode 读取与候选写入由变异侧
    /// 组件自身的接口完成，不经过 Agent 工具通道。
    pub fn mutation() -> Self {
        Self {
            profile: ExecutionProfile::Mutation,
            tools: ToolAccess::none(),
            filesystem: FilesystemScope::Denied,
            allow_network: false,
            allow_secrets: false,
            allow_process: false,
            limits: ResourceLimits {
                max_steps: Some(16),
                max_tokens: Some(4096),
                // `max_depth` 为 0 已经阻断全部派生，是这里真正起作用的约束；
                // children 与 concurrent 保持 1，以满足 Runtime 对这两项必须为正数的校验。
                max_depth: Some(0),
                max_children_per_agent: Some(1),
                max_concurrent_agents: Some(1),
                wall_clock_ms: Some(120_000),
                max_processes: Some(0),
            },
        }
    }

    /// 返回当前 Profile。没有对应的 setter，Profile 只能通过 `restrict` 收紧。
    pub fn profile(&self) -> ExecutionProfile {
        self.profile
    }

    /// 判断可信运行平面是否允许访问真实网络。
    ///
    /// 公开布尔字段只表达调用方请求；Evaluation 与 Mutation 的 Profile 身份拥有最终
    /// 否决权，因此即使字段被错误地改为 `true` 也不会开放网络能力。
    pub fn permits_network_access(&self) -> bool {
        self.profile == ExecutionProfile::Serve && self.allow_network
    }

    /// 判断可信运行平面是否允许读取或注入真实 Secret。
    ///
    /// 该方法是未来 Secret Broker 与其他真实凭据入口必须复用的最终门禁；受限 Profile
    /// 不会因公开请求字段被篡改而获得凭据。
    pub fn permits_secret_access(&self) -> bool {
        self.profile == ExecutionProfile::Serve && self.allow_secrets
    }

    /// 判断可信运行平面是否允许启动 Shell 或其他原生子进程。
    ///
    /// 调用操作系统的每个进程入口都必须复用该方法，不能只依赖工具名 allowlist 或公开
    /// 布尔字段，否则 Guest 别名与错误配置可能绕过受限 Profile。
    pub fn permits_process_execution(&self) -> bool {
        self.profile == ExecutionProfile::Serve && self.allow_process
    }

    /// 判断策略是否允许调用指定工具。
    ///
    /// 除 allowlist 外，进程能力最终门禁会额外拒绝声明需要进程能力的工具。
    pub fn permits_tool(&self, name: &str) -> bool {
        if !self.tools.permits(name) {
            return false;
        }
        if is_process_tool(name) && !self.permits_process_execution() {
            return false;
        }
        true
    }

    /// 逐字段取更严格的一方，返回不宽于 `self` 的新策略。
    ///
    /// 这是唯一的策略组合入口。布尔能力取逻辑与，工具取交集，限额取较小值，
    /// 因此无论 `requested` 声明什么，都不可能扩大权限。
    pub fn restrict(&self, requested: &Self) -> Self {
        Self {
            profile: self.profile.restrict(requested.profile),
            tools: self.tools.restrict(&requested.tools),
            filesystem: self.filesystem.restrict(&requested.filesystem),
            allow_network: self.allow_network && requested.allow_network,
            allow_secrets: self.allow_secrets && requested.allow_secrets,
            allow_process: self.allow_process && requested.allow_process,
            limits: self.limits.restrict(&requested.limits),
        }
    }
}

/// 判断工具名是否属于需要子进程能力的原生工具。
///
/// 与 [`crate::builtins`] 中的工具名保持一致；新增进程类工具时必须同步登记，
/// 否则 Evaluation 平面会漏放。
fn is_process_tool(name: &str) -> bool {
    matches!(name, "shell" | "process_exec")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_restrict_never_widens() {
        // Evaluation 请求回到 Serve 时必须保持 Evaluation。
        assert_eq!(
            ExecutionProfile::Evaluation.restrict(ExecutionProfile::Serve),
            ExecutionProfile::Evaluation
        );
        assert_eq!(
            ExecutionProfile::Mutation.restrict(ExecutionProfile::Serve),
            ExecutionProfile::Mutation
        );
        assert_eq!(
            ExecutionProfile::Serve.restrict(ExecutionProfile::Evaluation),
            ExecutionProfile::Evaluation
        );
    }

    #[test]
    fn policy_restrict_cannot_regain_capabilities() {
        let evaluation = ExecutionPolicy::evaluation("/tmp/fixture");
        let widened = evaluation.restrict(&ExecutionPolicy::serve());

        assert_eq!(widened.profile(), ExecutionProfile::Evaluation);
        assert!(!widened.allow_network);
        assert!(!widened.allow_secrets);
        assert!(!widened.allow_process);
        assert_eq!(widened.tools, ToolAccess::none());
        assert_eq!(
            widened.filesystem,
            FilesystemScope::Root(PathBuf::from("/tmp/fixture"))
        );
    }

    #[test]
    fn evaluation_denies_process_tools_even_when_allowlisted() {
        let mut policy = ExecutionPolicy::evaluation("/tmp/fixture");
        // 即使评测策略错误地把 shell 放进 allowlist 并改开布尔位，Profile 仍应拒绝。
        policy.tools = ToolAccess::allowlist(["read_file", "shell"]);
        policy.allow_process = true;

        assert!(policy.permits_tool("read_file"));
        assert!(!policy.permits_tool("shell"));
    }

    #[test]
    fn restricted_profiles_cannot_regain_real_capabilities() {
        for mut policy in [
            ExecutionPolicy::evaluation("/tmp/fixture"),
            ExecutionPolicy::mutation(),
        ] {
            // 模拟调用方绕过构造流程直接改开公开请求字段。
            policy.allow_network = true;
            policy.allow_secrets = true;
            policy.allow_process = true;

            assert!(!policy.permits_network_access());
            assert!(!policy.permits_secret_access());
            assert!(!policy.permits_process_execution());
        }
    }

    #[test]
    fn serve_allows_process_tools() {
        let policy = ExecutionPolicy::serve();
        assert!(policy.permits_tool("shell"));
        assert!(policy.permits_tool("read_file"));
        assert!(policy.permits_network_access());
        assert!(policy.permits_secret_access());
        assert!(policy.permits_process_execution());
    }

    #[test]
    fn filesystem_scope_cannot_escape_current_root() {
        let scope = FilesystemScope::Root(PathBuf::from("/tmp/fixture"));

        assert_eq!(
            scope.restrict(&FilesystemScope::Root(PathBuf::from("/tmp/fixture/sub"))),
            FilesystemScope::Root(PathBuf::from("/tmp/fixture/sub"))
        );
        // 请求无关目录时收缩为 Denied，而不是采纳请求。
        assert_eq!(
            scope.restrict(&FilesystemScope::Root(PathBuf::from("/etc"))),
            FilesystemScope::Denied
        );
        assert_eq!(
            scope.restrict(&FilesystemScope::Unrestricted),
            FilesystemScope::Root(PathBuf::from("/tmp/fixture"))
        );
    }

    #[test]
    fn limits_take_the_smaller_bound() {
        let current = ResourceLimits {
            max_steps: Some(10),
            max_tokens: None,
            ..ResourceLimits::default()
        };
        let requested = ResourceLimits {
            max_steps: Some(64),
            max_tokens: Some(512),
            ..ResourceLimits::default()
        };
        let merged = current.restrict(&requested);

        assert_eq!(merged.max_steps, Some(10));
        // 当前维度无上限时采纳请求方的上限。
        assert_eq!(merged.max_tokens, Some(512));
    }

    #[test]
    fn clamp_tokens_applies_policy_ceiling_when_caller_omits_it() {
        let limits = ResourceLimits {
            max_tokens: Some(4096),
            ..ResourceLimits::default()
        };
        assert_eq!(limits.clamp_tokens(None), Some(4096));
        assert_eq!(limits.clamp_tokens(Some(8192)), Some(4096));
        assert_eq!(limits.clamp_tokens(Some(256)), Some(256));
    }

    #[test]
    fn policy_round_trips_through_json() {
        let policy = ExecutionPolicy::evaluation("/tmp/fixture");
        let encoded = serde_json::to_string(&policy).expect("策略应可序列化");
        let decoded: ExecutionPolicy = serde_json::from_str(&encoded).expect("策略应可反序列化");

        assert_eq!(policy, decoded);
        assert_eq!(decoded.profile(), ExecutionProfile::Evaluation);
    }
}

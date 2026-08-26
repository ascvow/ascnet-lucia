//! Agent Runtime 的稳定错误协议。

use crate::{AgentId, AgentProfileId, RuntimePrincipal};
use thiserror::Error;

/// Agent Runtime 操作结果。
pub type RuntimeResult<T> = Result<T, AgentRuntimeError>;

/// Agent Runtime 返回的稳定错误类型。
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentRuntimeError {
    /// 运行时限额配置无效。
    #[error("运行时限额无效：{0}")]
    InvalidLimits(String),

    /// 指定的 Agent 不存在。
    #[error("Agent 不存在：{0}")]
    AgentNotFound(AgentId),

    /// Runtime principal 为空或格式无效。
    #[error("Runtime principal 无效：{0}")]
    InvalidPrincipal(String),

    /// Runtime principal 已被撤销。
    #[error("Runtime principal 已撤销：{0}")]
    PrincipalRevoked(RuntimePrincipal),

    /// Runtime principal 不是目标 Agent 的 owner。
    #[error("Runtime principal {principal} 不是 Agent {agent} 的 owner")]
    OwnerMismatch {
        /// Host 注入的可信 principal。
        principal: RuntimePrincipal,
        /// 被访问的 Agent。
        agent: AgentId,
    },

    /// 命名 profile 标识无效。
    #[error("Agent profile 标识无效：{0}")]
    InvalidProfileId(String),

    /// 命名 profile 已注册。
    #[error("Agent profile 已注册：{0}")]
    ProfileAlreadyExists(AgentProfileId),

    /// 命名 profile 不存在。
    #[error("Agent profile 不存在：{0}")]
    ProfileNotFound(AgentProfileId),

    /// principal 未获准使用指定 profile。
    #[error("Runtime principal {principal} 未获准使用 Agent profile {profile}")]
    ProfileDenied {
        /// Host 注入的可信 principal。
        principal: RuntimePrincipal,
        /// 被拒绝的 profile。
        profile: AgentProfileId,
    },

    /// 调用者无权管理目标 Agent。
    #[error("Agent {caller} 无权管理 Agent {target}")]
    PermissionDenied {
        /// 可信调用者身份。
        caller: AgentId,
        /// 被访问的目标身份。
        target: AgentId,
    },

    /// 调用者已经处于终态，不能再派生子节点。
    #[error("Agent 已结束，不能继续派生：{0}")]
    AgentInactive(AgentId),

    /// 派生深度超过运行时上限。
    #[error("Agent 派生深度超过上限 {limit}")]
    MaxDepthExceeded {
        /// 允许的最大深度，根节点深度为零。
        limit: usize,
    },

    /// 单个父节点的累计子节点数量超过上限。
    #[error("Agent 子节点数量超过上限 {limit}")]
    MaxChildrenExceeded {
        /// 单个父节点允许创建的累计子节点数量。
        limit: usize,
    },

    /// 派生 Agent 的构造失败。
    #[error("Agent 构造失败：{0}")]
    AgentBuild(String),

    /// Host 注入的运行观察器无法开始或收敛证据。
    #[error("Agent 运行观察失败：{0}")]
    RunObservation(String),

    /// 根节点没有后台执行任务，不能等待结果。
    #[error("Agent 没有可等待的执行任务：{0}")]
    NotRunnable(AgentId),

    /// 目标 Agent 尚未成功结束，或没有可供后续运行使用的私有会话。
    #[error("Agent 没有可继续的成功会话：{0}")]
    SessionUnavailable(AgentId),

    /// 目标 Agent 已进入终态，不能再接收实时交互消息。
    #[error("Agent 当前不能接收实时消息：{0}")]
    InteractionUnavailable(AgentId),

    /// 排队 Agent 尚未启动，暂存的交互消息已达到上限。
    #[error("Agent 待处理实时消息已达到上限 {limit}：{agent}")]
    PendingInteractionsExceeded {
        /// 无法继续暂存消息的 Agent。
        agent: AgentId,
        /// 单个 Agent 的待处理消息上限。
        limit: usize,
    },
}

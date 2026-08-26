//! Lucia 的通用 Agent 派生与协作运行时。
//!
//! 本 crate 只提供身份、权限、生命周期、限额和消息通道等机制，不定义
//! sub-agent、workflow、multi-agent 或 teammate 等业务策略。

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod error;
mod identity;
mod permissions;
mod protocol;
mod runtime;

pub use error::{AgentRuntimeError, RuntimeResult};
pub use identity::{AgentId, AgentLineage, AgentProfileId, RuntimePrincipal};
pub use permissions::{
    AgentDeriveConfig, AgentOptionsPatch, AgentPermissions, AgentTemplate, ToolAccess,
};
pub use protocol::{
    AgentEventStream, AgentExecutionResult, AgentHandle, AgentOutcome, AgentRuntimeApi,
    AgentRuntimeProvisioner, AgentSnapshot, AgentSpawnRequest, AgentStatus,
    ProvisionedAgentRuntime, RuntimeLimits, RuntimeRunContext, RuntimeRunFinalizer,
    RuntimeRunObservation, RuntimeRunObserver, RuntimeRunTermination,
};
pub use runtime::AgentRuntime;
